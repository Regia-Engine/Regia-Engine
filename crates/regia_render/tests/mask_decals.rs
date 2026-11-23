// SPDX-License-Identifier: MPL-2.0
//! Material mask recolor and decal slots (Tincal B7) end to end on the GPU: one test asset (a
//! quad with one mask and one decal atlas) drawn five times, four faction palettes and a chest
//! number from per-instance data only (no material clone, no per-faction texture), plus an
//! alpha-blended instance. Palette edits rewrite the instance blocks in place: no GPU buffer or
//! bind group is created after an instance's first frame.

use std::sync::Arc;

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    ALPHA_BLEND, DecalSlot, DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer,
    FrameScene, LabMaterial, LabVertex, MaskDecalInstance, MaskDecalTextures, MaskDecals, MaskPalette,
    RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast, SharedImage,
};

const WIDTH: u32 = 576;
const HEIGHT: u32 = 192;
const EYE: [f32; 3] = [0.0, 0.0, 3.4];
const FOV_Y: f32 = 0.8;
const HALF: f32 = 0.5;
/// Quad centers along x; the last one is the alpha-blended instance.
const XS: [f32; 5] = [-2.4, -1.2, 0.0, 1.2, 2.4];
/// The material's base color (unlit, so a region shows `BASE × palette` like a plain material).
const BASE: f32 = 0.8;

/// A quad facing +Z, UV (0, 0) at its top-left.
fn quad() -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    let vertices = vec![
        LabVertex::from_pos_normal_uv([-HALF, -HALF, 0.0], n, [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([HALF, -HALF, 0.0], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([HALF, HALF, 0.0], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([-HALF, HALF, 0.0], n, [0.0, 0.0]),
    ];
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// 4×4 mask: left half primary (R), top right secondary (G), bottom right trim (B).
fn mask() -> SharedImage {
    let mut rgba = Vec::with_capacity(64);
    for row in 0..4 {
        for col in 0..4 {
            let texel = match (col < 2, row < 2) {
                (true, _) => [255, 0, 0, 0],
                (false, true) => [0, 255, 0, 0],
                (false, false) => [0, 0, 255, 0],
            };
            rgba.extend_from_slice(&texel);
        }
    }
    SharedImage::rgba(4, 4, rgba, false).expect("mask")
}

/// 16×8 atlas, two 8×8 cells: cell 0 is the digit "1" (a white bar, columns 3-4, rows 1-6, on
/// transparent), cell 1 is empty.
fn atlas() -> SharedImage {
    let mut rgba = vec![0u8; 16 * 8 * 4];
    for row in 1..7 {
        for col in 3..5 {
            let i = (row * 16 + col) * 4;
            rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    SharedImage::rgba(16, 8, rgba, true).expect("atlas")
}

/// The chest number: atlas cell 0 over the top-left quadrant (inside the primary region).
const NUMBER_RECT: [f32; 4] = [0.15, 0.05, 0.45, 0.45];

fn palettes() -> [MaskPalette; 5] {
    let p = |primary: [f32; 3], secondary: [f32; 3], trim: [f32; 3]| MaskPalette {
        primary,
        secondary,
        trim,
        ..MaskPalette::NEUTRAL
    };
    [
        p([0.9, 0.05, 0.05], [0.2, 0.2, 0.2], [0.9, 0.8, 0.1]),
        p([0.05, 0.1, 0.9], [0.9, 0.9, 0.9], [0.1, 0.1, 0.1]),
        p([0.05, 0.7, 0.1], [0.4, 0.25, 0.1], [0.9, 0.9, 0.9]),
        p([0.9, 0.75, 0.05], [0.05, 0.05, 0.05], [0.6, 0.1, 0.6]),
        p([0.9, 0.05, 0.05], [0.2, 0.2, 0.2], [0.9, 0.8, 0.1]),
    ]
}

struct Asset {
    mesh: (Vec<LabVertex>, Vec<u32>),
    opaque: LabMaterial,
    blended: LabMaterial,
    textures: Arc<MaskDecalTextures>,
    quality: FrameQuality,
}

fn asset() -> Asset {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    let opaque = LabMaterial {
        base_color: [BASE, BASE, BASE, 1.0],
        unlit: true,
        ..LabMaterial::default()
    };
    Asset {
        mesh: quad(),
        blended: LabMaterial {
            alpha_mode: ALPHA_BLEND,
            ..opaque.clone()
        },
        opaque,
        textures: Arc::new(MaskDecalTextures {
            mask: Some(mask()),
            atlas: Some(atlas()),
            atlas_grid: [2, 1],
        }),
        quality,
    }
}

fn object_key(i: usize) -> u64 {
    300 + i as u64
}

fn scene(asset: &Asset) -> FrameScene<'_> {
    // One material for the four opaque instances: the looks come from instance data.
    let materials = [
        &asset.opaque,
        &asset.opaque,
        &asset.opaque,
        &asset.opaque,
        &asset.blended,
    ];
    scene_with(asset, materials, [101, 101, 101, 101, 102])
}

/// The five quads with `materials` (cache keys `keys`).
fn scene_with<'a>(asset: &'a Asset, materials: [&'a LabMaterial; 5], keys: [u64; 5]) -> FrameScene<'a> {
    let meshes = XS
        .iter()
        .enumerate()
        .map(|(i, x)| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: keys[i],
            object_key: object_key(i),
            vertices: &asset.mesh.0,
            indices: &asset.mesh.1,
            material: materials[i],
            model: Mat4::from_translation(Vec3::new(*x, 0.0, 0.0)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 1_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0; 3], 50_000.0),
        local_lights: &[],
        quality: asset.quality,
        meshes,
    }
}

fn instances(asset: &Asset, palettes: &[MaskPalette; 5], numbers: bool) -> Vec<MaskDecalInstance> {
    palettes
        .iter()
        .enumerate()
        .map(|(i, palette)| {
            let mut decals = MaskDecals::new(asset.textures.clone(), *palette);
            if numbers {
                decals = decals.with_decal(DecalSlot::atlas(0, NUMBER_RECT));
            }
            MaskDecalInstance {
                object_key: object_key(i),
                decals,
            }
        })
        .collect()
}

/// Pixel of quad `i` at its `uv`.
fn pixel(pixels: &[u8], i: usize, uv: [f32; 2]) -> [u8; 3] {
    let world = Vec3::new(XS[i] - HALF + uv[0] * 2.0 * HALF, HALF - uv[1] * 2.0 * HALF, 0.0);
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::ZERO, Vec3::Y);
    let proj = perspective(FOV_Y, WIDTH as f32 / HEIGHT as f32, 0.1, 40.0);
    let clip = proj * view * Vec4::new(world.x, world.y, world.z, 1.0);
    let ndc = clip.truncate() / clip.w;
    let x = (((ndc.x * 0.5 + 0.5) * WIDTH as f32) as usize).min(WIDTH as usize - 1);
    let y = (((0.5 - ndc.y * 0.5) * HEIGHT as f32) as usize).min(HEIGHT as usize - 1);
    let at = (y * WIDTH as usize + x) * 4;
    [pixels[at], pixels[at + 1], pixels[at + 2]]
}

/// The quads drawn with plain materials of base color `BASE × colors[i]` (the opaque or blended
/// material otherwise unchanged): what a recolored region must look like.
fn flat(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    asset: &Asset,
    colors: [[f32; 3]; 5],
    key_base: u64,
) -> Vec<u8> {
    let materials: Vec<LabMaterial> = colors
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let source = if i == 4 { &asset.blended } else { &asset.opaque };
            LabMaterial {
                base_color: [BASE * c[0], BASE * c[1], BASE * c[2], 1.0],
                ..source.clone()
            }
        })
        .collect();
    let refs = [
        &materials[0],
        &materials[1],
        &materials[2],
        &materials[3],
        &materials[4],
    ];
    let keys = std::array::from_fn(|i| key_base + i as u64);
    renderer
        .render_offscreen(device, &scene_with(asset, refs, keys))
        .expect("flat reference")
        .pixels
}

fn near(got: [u8; 3], want: [u8; 3], tolerance: i32, what: &str) {
    let off = got
        .iter()
        .zip(want)
        .map(|(g, w)| (i32::from(*g) - i32::from(w)).abs())
        .max()
        .unwrap_or(0);
    assert!(
        off <= tolerance,
        "{what}: got {got:?}, want {want:?} (±{tolerance})"
    );
}

const PRIMARY_UV: [f32; 2] = [0.25, 0.75];
const SECONDARY_UV: [f32; 2] = [0.75, 0.25];
const TRIM_UV: [f32; 2] = [0.75, 0.75];
/// The digit's bar (decal center).
const NUMBER_UV: [f32; 2] = [0.3, 0.25];
/// Inside the decal rectangle, on the cell's transparent part (and a full mask texel away from
/// the UV border: the material sampler repeats, so the 4×4 mask blends across it).
const NUMBER_EDGE_UV: [f32; 2] = [0.18, 0.25];

fn mask_events() -> Vec<String> {
    regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                FramePathKind::Buffer | FramePathKind::BindGroup | FramePathKind::Texture
            )
        })
        .map(|e| e.label)
        .filter(|label| label.contains("mask decal"))
        .collect()
}

#[test]
fn four_faction_palettes_and_a_chest_number_from_instance_data() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let asset = asset();
    let mut palettes = palettes();
    // References: the same quads with plain materials of the expected colors, drawn by a renderer
    // of their own (the masked renderer's caches stay as a host's would).
    let mut reference = FrameRenderer::new();
    let region = |reference: &mut FrameRenderer,
                  palettes: &[MaskPalette; 5],
                  pick: fn(&MaskPalette) -> [f32; 3],
                  key: u64| {
        flat(
            &device,
            reference,
            &asset,
            std::array::from_fn(|i| pick(&palettes[i])),
            key,
        )
    };
    let primary = region(&mut reference, &palettes, |p| p.primary, 1000);
    let secondary = region(&mut reference, &palettes, |p| p.secondary, 1010);
    let trim = region(&mut reference, &palettes, |p| p.trim, 1020);
    // A paint-over decal of white replaces the base color with 1 (`BASE × 1 / BASE`).
    let white = flat(&device, &mut reference, &asset, [[1.0 / BASE; 3]; 5], 1030);
    let grey = flat(&device, &mut reference, &asset, [[1.0; 3]; 5], 1040);
    let mut renderer = FrameRenderer::new();

    // Without instances every quad is the material's grey.
    let plain = renderer
        .render_offscreen(&device, &scene(&asset))
        .expect("plain")
        .pixels;
    for i in 0..XS.len() {
        near(
            pixel(&plain, i, PRIMARY_UV),
            pixel(&grey, i, PRIMARY_UV),
            2,
            "unmasked quad",
        );
    }

    renderer.set_mask_decals(&instances(&asset, &palettes, true));
    regia_render::trace_frame_path(true);
    let pixels = renderer
        .render_offscreen(&device, &scene(&asset))
        .expect("masked")
        .pixels;
    let first_frame_events = mask_events();
    let stats = renderer.mask_decal_stats();
    assert_eq!(stats.instances, 5);
    assert_eq!(stats.masked_draws, 5, "{stats:?}");
    assert_eq!(stats.fallback_draws, 0, "{stats:?}");
    assert_eq!(stats.unsupported_draws, 0, "{stats:?}");
    assert_eq!(stats.blocks_created, 5, "one uniform block per instance");
    assert_eq!(stats.gpu_entries, 5);
    assert!(
        first_frame_events
            .iter()
            .filter(|l| l.as_str() == "mask decal block")
            .count()
            == 5,
        "the first frame creates one block per instance: {first_frame_events:?}"
    );

    for i in 0..palettes.len() {
        near(
            pixel(&pixels, i, PRIMARY_UV),
            pixel(&primary, i, PRIMARY_UV),
            4,
            &format!("quad {i} primary"),
        );
        near(
            pixel(&pixels, i, SECONDARY_UV),
            pixel(&secondary, i, SECONDARY_UV),
            4,
            &format!("quad {i} secondary"),
        );
        near(
            pixel(&pixels, i, TRIM_UV),
            pixel(&trim, i, TRIM_UV),
            4,
            &format!("quad {i} trim"),
        );
        // The chest number: white on the digit, the faction color around it.
        near(
            pixel(&pixels, i, NUMBER_UV),
            pixel(&white, i, NUMBER_UV),
            4,
            &format!("quad {i} digit"),
        );
        near(
            pixel(&pixels, i, NUMBER_EDGE_UV),
            pixel(&primary, i, NUMBER_EDGE_UV),
            4,
            &format!("quad {i} number edge"),
        );
    }
    // Four distinct looks from one material and one set of textures.
    for a in 0..4 {
        for b in a + 1..4 {
            let (pa, pb) = (pixel(&pixels, a, PRIMARY_UV), pixel(&pixels, b, PRIMARY_UV));
            let distance: i32 = pa
                .iter()
                .zip(pb)
                .map(|(x, y)| (i32::from(*x) - i32::from(y)).abs())
                .sum();
            assert!(distance > 120, "quads {a} and {b} look alike: {pa:?} vs {pb:?}");
        }
    }

    // Steady frames with palette edits: blocks are rewritten in place, nothing is created.
    for frame in 0..4 {
        palettes[0].primary = [0.1 + 0.2 * frame as f32, 0.3, 0.6];
        renderer.set_mask_decals(&instances(&asset, &palettes, true));
        let pixels = renderer
            .render_offscreen(&device, &scene(&asset))
            .expect("edit")
            .pixels;
        let created = mask_events();
        assert!(created.is_empty(), "frame {frame} created {created:?}");
        assert_eq!(renderer.mask_decal_stats().blocks_created, 5);
        assert_eq!(
            renderer.mask_decal_stats().block_writes,
            1,
            "only the edited instance uploads"
        );
        let edited = region(&mut reference, &palettes, |p| p.primary, 1100 + 10 * frame);
        near(
            pixel(&pixels, 0, PRIMARY_UV),
            pixel(&edited, 0, PRIMARY_UV),
            4,
            "edited palette",
        );
    }
    regia_render::trace_frame_path(false);

    // The number is instance data: without the slot the primary color shows there.
    renderer.set_mask_decals(&instances(&asset, &palettes, false));
    let pixels = renderer
        .render_offscreen(&device, &scene(&asset))
        .expect("no numbers")
        .pixels;
    let primary = region(&mut reference, &palettes, |p| p.primary, 1200);
    for i in 0..palettes.len() {
        near(
            pixel(&pixels, i, NUMBER_UV),
            pixel(&primary, i, NUMBER_UV),
            4,
            &format!("quad {i} without number"),
        );
    }

    // An empty list releases every entry and draws the plain material again.
    renderer.set_mask_decals(&[]);
    let cleared = renderer
        .render_offscreen(&device, &scene(&asset))
        .expect("cleared")
        .pixels;
    assert_eq!(renderer.mask_decal_stats().gpu_entries, 0);
    assert_eq!(renderer.mask_decal_stats().masked_draws, 0);
    for i in 0..XS.len() {
        near(
            pixel(&cleared, i, PRIMARY_UV),
            pixel(&grey, i, PRIMARY_UV),
            2,
            "cleared quad",
        );
    }
}
