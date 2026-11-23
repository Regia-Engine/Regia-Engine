// SPDX-License-Identifier: MPL-2.0
//! The frames of the three features whose costs ADR-2482 cut (lens effects, the shadow casters'
//! two-phase cull, thin film on 500 materials) are pinned by hash on the lavapipe reference: the
//! cost fixes must not move a pixel. The pins were taken on the base of the change
//! (`integ/v035` at 94d4d501e) with this file; another adapter prints the hashes and checks only
//! that the frames are deterministic.
//!
//! When a pin moves on purpose (a look change of the feature), update it in the same commit and
//! say why, as for a golden image.

use std::sync::Mutex;

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabMesh, LabScene, LabVertex, LensEffects, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, ShadowCast, ThinFilmFactors, box_mesh,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

/// Pinned frame hashes on lavapipe (`llvmpipe`): lens effects (cinematic), shadow occlusion on,
/// thin film on 500 materials.
///
/// `thin_film_500` was re-pinned on integ-v049 (ADR-5993 (19)) after a bisect on the build server lavapipe
/// (Mesa 26.1.6): frame 3 reads 0xcb6c_93d6_979d_42c6 at d3732b665 (integ-v047 before the merge of
/// v05-dynamic-gi-fix-d) and 0x4819_7af3_a72a_115d at that merge 0cc820080, whose diff changes the
/// probe shaders (`ddgi_common.wgsl`, `ddgi_sample.wgsl`, `ddgi_trace.wgsl`): an intended GI change
/// (all three frames move, so it is lighting, not temporal). The merged head reads the same value.
///
/// Re-pinned again on v06-gate-reds-render (ADR-8785), from 0x4819_7af3_a72a_115d: a bisect on
/// the build server lavapipe puts the move at merge 182daaf76 (lane v051-holo-over-sky, ADR-7963: the
/// physical sky background is drawn into the forward image between opaque and blended draws,
/// before the screen-space passes). Only frame 3 moves, 20 of 32 000 pixels by one level. With
/// `ssr` and `screen_space_gi` off, or with `physical_atmosphere` off, the frames hash the same
/// before and after that merge: SSR and SSGI now read the sky's radiance where they used to read
/// the cleared color. An intended consequence of the ordering change, not a thin film change.
const PINS: [(&str, u64); 3] = [
    ("lens_cinematic", 0x9d50_eeac_9406_907a),
    ("shadow_occlusion", 0xdc79_1396_f7d3_b5f0),
    ("thin_film_500", 0xa1ae_0124_6ebb_4491),
];

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn lavapipe(device: &RenderDevice) -> bool {
    device
        .adapter
        .get_info()
        .name
        .to_ascii_lowercase()
        .contains("llvmpipe")
}

fn check(device: &RenderDevice, name: &str, frames: &[Vec<u8>]) {
    let hashes: Vec<u64> = frames.iter().map(|f| fnv1a(f)).collect();
    eprintln!("{name}: frame hashes {hashes:x?}");
    let last = hashes.last().copied().unwrap_or(0);
    let pin = PINS.iter().find(|(n, _)| *n == name).map_or(0, |(_, p)| *p);
    if lavapipe(device) && pin != 0 {
        assert_eq!(
            last, pin,
            "{name}: the frame moved (hash {last:#x}, pinned {pin:#x})"
        );
    }
}

// ---- lens effects ----

const LW: u32 = 320;
const LH: u32 = 180;

fn light_at(c: [f32; 2]) -> LabMesh {
    let depth = 5.0;
    let half_h = depth * (0.45_f32).tan();
    let half_w = half_h * LW as f32 / LH as f32;
    let (dx, dy) = (4.0 / LW as f32, 4.0 / LH as f32);
    let (x0, x1) = ((c[0] - dx) * half_w, (c[0] + dx) * half_w);
    let (y0, y1) = (-(c[1] - dy) * half_h, -(c[1] + dy) * half_h);
    let z = -depth;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x0, y1, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y1, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x1, y0, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x0, y0, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

#[test]
fn lens_effects_frame_is_pinned() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let mut post = PostSettings::golden_deterministic();
    post.lens_effects = LensEffects::CINEMATIC;
    let lab = LabScene {
        width: LW,
        height: LH,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -1.0],
        fov_y: 0.9,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 500.0,
            ..LabMaterial::default()
        }],
        meshes: vec![light_at([0.5, -0.4]), light_at([-0.3, 0.2]), light_at([0.1, 0.6])],
    };
    let mut renderer = FrameRenderer::new();
    assert!(renderer.enable_lens_effects());
    let frames: Vec<Vec<u8>> = (0..4)
        .map(|_| {
            renderer
                .render_offscreen(&device, &lab.as_frame_scene())
                .expect("frame")
                .pixels
        })
        .collect();
    assert_eq!(frames[2], frames[3], "a static lens frame is deterministic");
    check(&device, "lens_cinematic", &frames);
}

// ---- shadow occlusion (the geometry of `shadow_occlusion_steady.rs`) ----

fn shadow_mesh<'g>(
    key: u64,
    mesh_key: u64,
    geometry: &'g (Vec<LabVertex>, Vec<u32>),
    material: &'g LabMaterial,
    at: Vec3,
) -> FrameMesh<'g> {
    FrameMesh {
        is_static: false,
        mesh_key,
        material_key: 1,
        object_key: key,
        vertices: geometry.0.as_slice(),
        indices: geometry.1.as_slice(),
        material,
        model: Mat4::from_translation(at).to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

#[test]
fn shadow_occlusion_frame_is_pinned() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let ground = box_mesh([8.0, 0.05, 8.0]);
    let slab = box_mesh([3.2, 0.05, 3.2]);
    let cube = box_mesh([0.1, 0.1, 0.1]);
    let material = LabMaterial::default();
    let mesh = |key: u64, mesh_key: u64, geometry, at| shadow_mesh(key, mesh_key, geometry, &material, at);
    let mut meshes = vec![
        mesh(1, 1, &ground, Vec3::new(0.0, -0.05, 0.0)),
        mesh(2, 2, &slab, Vec3::new(0.0, 3.0, 0.0)),
    ];
    for i in 0..256usize {
        let (col, row) = ((i % 16) as f32, (i / 16) as f32);
        meshes.push(mesh(
            i as u64 + 10,
            3,
            &cube,
            Vec3::new(-2.4 + col * 0.32, 0.25, -2.4 + row * 0.32),
        ));
    }
    let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let scene = FrameScene {
        width: 96,
        height: 64,
        eye: [0.0, 2.5, 7.5],
        target: [0.0, 0.5, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.5,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.15, -1.0, -0.1], [1.0, 1.0, 1.0], 3.0),
        local_lights: &[],
        quality: FrameQuality {
            csm_cascade_count: 2,
            csm_resolution: 512,
            paged_sun_shadows: false,
            taau: false,
            ..base
        },
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_shadow_occlusion(true);
    renderer.set_shadow_occlusion_min_casters(Some(64));
    let frames: Vec<Vec<u8>> = (0..6)
        .map(|_| renderer.render_offscreen(&device, &scene).expect("frame").pixels)
        .collect();
    check(&device, "shadow_occlusion", &frames);
}

// ---- thin film on 500 materials ----

/// 500 filmed materials on a 25 × 20 grid of small boxes, every one its own material key.
fn film_grid_materials() -> Vec<LabMaterial> {
    (0..500)
        .map(|i| LabMaterial {
            base_color: [
                0.05 + 0.9 * ((i % 7) as f32 / 6.0),
                0.05 + 0.9 * ((i % 11) as f32 / 10.0),
                0.05 + 0.9 * ((i % 13) as f32 / 12.0),
                1.0,
            ],
            roughness: 0.2 + 0.6 * ((i % 5) as f32 / 4.0),
            thin_film: ThinFilmFactors {
                factor: 1.0,
                ior: 1.3,
                thickness_min: 100.0,
                thickness_max: 200.0 + (i % 9) as f32 * 30.0,
            },
            ..LabMaterial::EMPTY
        })
        .collect()
}

#[test]
fn thin_film_frame_is_pinned() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let materials = film_grid_materials();
    let cube = box_mesh([0.18, 0.18, 0.18]);
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, material)| {
            let (col, row) = ((i % 25) as f32, (i / 25) as f32);
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1000 + i as u64,
                object_key: 5000 + i as u64,
                vertices: cube.0.as_slice(),
                indices: cube.1.as_slice(),
                material,
                model: Mat4::from_translation(Vec3::new(-6.0 + col * 0.5, -4.75 + row * 0.5, 0.0))
                    .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    let scene = FrameScene {
        width: 200,
        height: 160,
        eye: [0.0, 0.0, 14.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        quality: FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh)),
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    let frames: Vec<Vec<u8>> = (0..3)
        .map(|_| renderer.render_offscreen(&device, &scene).expect("frame").pixels)
        .collect();
    assert_eq!(renderer.thin_film_stats().fallback, 0, "every filmed draw shaded");
    check(&device, "thin_film_500", &frames);
}
