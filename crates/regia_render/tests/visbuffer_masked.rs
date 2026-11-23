// SPDX-License-Identifier: MPL-2.0
//! Masked (alpha-tested) clusters on the visibility path (ADR-3915), on a texture-table device of
//! whatever adapter `RenderDevice::headless` picks (lavapipe in CI).
//!
//! A row of leaf cards (subdivided quads with a leaf-shaped alpha mask and holes) stands between
//! the camera and a grid of small boxes in front of a wall, over a ground, under a shadowing sun.
//! Every mesh has cooked clusters. Checks:
//!
//! - **the same image**: the visibility frame (masked cards through the alpha-tested raster) and
//!   the forward frame agree within the differential's strict floors (98.5 % of pixels within 2
//!   levels, SSIM 0.985), so the cards' holes are where the forward alpha test puts them, the boxes
//!   seen through them are drawn (a masked hole never occludes in the Hi-Z cull), and the cards'
//!   shadows (forward, alpha-tested) are the forward frame's;
//! - **the cards take the path**: every card is a masked instance, alpha-tested, and masked
//!   clusters are drawn;
//! - **the holes show**: the same cards drawn opaque give a clearly different frame, so the first
//!   check can tell a tested card from a solid one;
//! - **switching the path off** gives the forward frame of a renderer that never took the path, at
//!   the same frame number (temporal GI moves frame by frame on both), and the casters the path drew
//!   as clusters are back in the forward shadow maps, in opaque scenes too. Byte for byte at once
//!   where the visibility frames equal the forward ones (lavapipe); where they match only within the
//!   floors (a card's alpha-test edges), the screen-space history holds that difference for a few
//!   frames, so the switched frame must be within 2 levels at once and byte-identical once the
//!   history has decayed (`SETTLE`); a caster left out never decays;
//! - **a warm frame creates nothing** with masked instances on the path;
//! - **a masked set** is alpha-tested from its first visibility frame.

use std::sync::Arc;

use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::MeshletBuilder;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabTexture, LabVertex, PostSettings, RenderDevice, RenderError, RenderProfile, RenderProfilePreset,
    ShadowCast, VisInstanceSet,
};
use regia_test_utils::golden::ssim_rgba8;

const SIZE: u32 = 128;
const CARDS: usize = 24;
const BOXES: usize = 64;

/// The level request is process-wide: only one test at a time may create a device.
static DEVICE_CREATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn table_device() -> Result<RenderDevice, RenderError> {
    let _guard = DEVICE_CREATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    let device = RenderDevice::headless();
    regia_render::texture_table::set_device_level_request(0);
    device
}

/// The cooker's cluster hierarchy, as a game's cooked meshes have it.
fn cooked_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        build_meshlet_mesh_with_attributes(
            &positions,
            Some(MeshletAttributes {
                normals: &normals,
                uvs: &uvs,
            }),
            indices,
            &MeshletSettings::default(),
        )
        .ok()
    })
}

/// A leaf: an ellipse across the card with round holes, alpha 255 inside and 0 outside.
fn leaf_texture() -> LabTexture {
    let size = 64u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let inside = u * u / 0.55 + v * v / 0.95 < 1.0;
            let hole =
                ((u * 4.0).fract().abs() - 0.5).powi(2) + ((v * 4.0).fract().abs() - 0.5).powi(2) < 0.05;
            let a = if inside && !hole { 255 } else { 0 };
            rgba.extend_from_slice(&[60 + (x % 7) as u8 * 10, 150, 50, a]);
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

/// A card of `n` × `n` quads, 1 × 1.4 m, in the XY plane, facing +Z, UV 0..1.
fn card(n: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            // A slight bend, so the simplifier has something to keep.
            let z = 0.08 * (u * std::f32::consts::PI).sin();
            vertices.push(LabVertex::from_pos_normal_uv(
                [u - 0.5, v * 1.4, z],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            ));
        }
    }
    let mut indices = Vec::new();
    for j in 0..n {
        for i in 0..n {
            let a = j * (n + 1) + i;
            let b = a + n + 1;
            indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    (vertices, indices)
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

struct Fixture {
    card: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    ground: (Vec<LabVertex>, Vec<u32>),
    wall: (Vec<LabVertex>, Vec<u32>),
    leaf: LabMaterial,
    stone: LabMaterial,
    floor: LabMaterial,
    plaster: LabMaterial,
}

fn fixture() -> Fixture {
    Fixture {
        card: card(4),
        cube: regia_render::box_mesh([0.12, 0.12, 0.12]),
        ground: regia_render::box_mesh([20.0, 0.05, 20.0]),
        wall: regia_render::box_mesh([6.0, 3.0, 0.1]),
        leaf: LabMaterial {
            albedo: Some(leaf_texture()),
            alpha_mode: regia_render::ALPHA_MASK,
            alpha_cutoff: 0.5,
            double_sided: true,
            ..LabMaterial::default()
        },
        stone: LabMaterial {
            base_color: [0.8, 0.3, 0.2, 1.0],
            ..LabMaterial::default()
        },
        floor: LabMaterial::default(),
        plaster: LabMaterial {
            base_color: [0.3, 0.4, 0.8, 1.0],
            ..LabMaterial::default()
        },
    }
}

fn post() -> PostSettings {
    PostSettings {
        taa: false,
        auto_exposure: false,
        bloom: false,
        exposure_ev: 15.0,
        ..PostSettings::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn mesh<'a>(
    key: u64,
    material_key: u64,
    object: u64,
    data: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
    model: [[f32; 4]; 4],
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key,
        object_key: object,
        vertices: &data.0,
        indices: &data.1,
        material,
        model,
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }
}

fn scene<'a>(f: &'a Fixture, post: &'a PostSettings, vis: bool) -> FrameScene<'a> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.visibility_buffer = vis;
    quality.render_scale_permille = 1000;
    quality.taau = false;
    let mut meshes = vec![
        mesh(1, 1001, 1, &f.ground, &f.floor, translate(0.0, -0.05, 0.0)),
        mesh(2, 1002, 2, &f.wall, &f.plaster, translate(0.0, 1.5, -3.0)),
    ];
    for i in 0..BOXES {
        let (x, y) = ((i % 16) as f32 * 0.32 - 2.4, (i / 16) as f32 * 0.4 + 0.3);
        meshes.push(mesh(
            3,
            1003,
            100 + i as u64,
            &f.cube,
            &f.stone,
            translate(x, y, -2.0),
        ));
    }
    for i in 0..CARDS {
        let (x, z) = ((i % 8) as f32 * 0.7 - 2.45, -((i / 8) as f32) * 0.35);
        meshes.push(mesh(
            4,
            1004,
            300 + i as u64,
            &f.card,
            &f.leaf,
            translate(x, 0.0, z),
        ));
    }
    FrameScene {
        width: SIZE,
        height: SIZE,
        eye: [0.0, 1.0, 3.2],
        target: [0.0, 0.9, -1.0],
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.4, -1.0, -0.5], [1.0; 3], 100_000.0),
        local_lights: &[],
        quality,
        meshes,
    }
}

/// A renderer with the cooker as its meshlet builder. On a CPU adapter the material sampler is
/// isotropic on both paths (as in `visbuffer::tests::comparison_renderer`).
fn renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    if device.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        renderer.set_material_anisotropy_for_tests(1);
    }
    renderer.set_meshlet_builder(Some(cooked_builder()));
    renderer
}

/// Frames drawn before a frame is read: the cull has last frame's visibility from the second on.
const FRAMES: usize = 3;

/// The last of `FRAMES` more frames.
fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>) -> Vec<u8> {
    let mut pixels = Vec::new();
    for _ in 0..FRAMES {
        pixels = renderer.render_offscreen(device, scene).expect("frame").pixels;
    }
    pixels
}

/// (worst channel difference, share of pixels within 2 levels).
fn diff(a: &[u8], b: &[u8]) -> (u8, f64) {
    let mut worst = 0u8;
    let mut close = 0usize;
    let pixels = a.len() / 4;
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = p
            .iter()
            .zip(q)
            .take(3)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        close += usize::from(d <= 2);
    }
    (worst, close as f64 / pixels.max(1) as f64)
}

/// Frames a switched renderer may take to draw the never-switched renderer's picture again: the
/// screen-space history (SSGI, GTAO, the dynamic GI's screen feedback) holds the visibility frames,
/// and decays frame by frame.
const SETTLE: usize = 60;

/// Compares `later` (from `forward_renderer`) with `off` (from `switched`, same frame number) and,
/// while they differ, draws `scene` on both one frame at a time until they are equal. Returns the
/// first comparison's (worst, close) and the frames it took to be equal (None: never within `SETTLE`).
fn settle(
    forward_renderer: &mut FrameRenderer,
    switched: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    later: Vec<u8>,
    off: Vec<u8>,
) -> (u8, f64, Option<usize>) {
    let (worst, close) = diff(&later, &off);
    let (mut later, mut off) = (later, off);
    for frames in 0..=SETTLE {
        if later == off {
            return (worst, close, Some(frames));
        }
        if frames == SETTLE {
            break;
        }
        later = forward_renderer
            .render_offscreen(device, scene)
            .expect("frame")
            .pixels;
        off = switched.render_offscreen(device, scene).expect("frame").pixels;
    }
    (worst, close, None)
}

/// Strict floors of `regia_test_utils/tests/visbuffer_differential.rs`; Metal rounds rasterizer
/// edges differently (ADR-275), so its floors sit at 0.97.
fn floor(device: &RenderDevice, strict: f64) -> f64 {
    if device.capabilities.backend == wgpu::Backend::Metal {
        strict.min(0.97)
    } else {
        strict
    }
}

#[test]
fn masked_cards_on_the_visibility_path_match_the_forward_frame() {
    let device = match table_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    }
    let f = fixture();
    let post = post();
    let mut forward_renderer = renderer(&device);
    let forward = frame(&mut forward_renderer, &device, &scene(&f, &post, false));
    let mut vis_renderer = renderer(&device);
    let vis = frame(&mut vis_renderer, &device, &scene(&f, &post, true));
    let stats = vis_renderer.visbuffer_stats();
    let counters = vis_renderer
        .visbuffer_counters(&device)
        .expect("readback")
        .expect("counters");
    let (worst, close) = diff(&forward, &vis);
    let ssim = ssim_rgba8(&forward, &vis, SIZE, SIZE).expect("ssim");
    println!(
        "masked cards: {:.3} % within 2 levels (worst {worst}), ssim {ssim:.4}; instances {}, masked {} \
         ({} materials, tested {}), masked clusters {}, clusters {}",
        close * 100.0,
        stats.instances,
        stats.masked_instances,
        stats.masked_materials,
        stats.masked_tested,
        counters.masked_clusters,
        counters.phase1_clusters + counters.phase2_clusters,
    );
    assert!(vis_renderer.visbuffer_active(), "the frame took the path");
    assert_eq!(
        stats.masked_instances as usize, CARDS,
        "every card is a masked instance"
    );
    assert_eq!(stats.masked_materials, 1);
    assert!(stats.masked_tested, "the masked clusters were alpha-tested");
    assert_eq!(
        stats.instances as usize,
        2 + BOXES + CARDS,
        "every mesh is on the path"
    );
    assert!(counters.masked_clusters > 0, "masked clusters drew");
    assert!(
        close >= floor(&device, 0.985) && ssim >= floor(&device, 0.985),
        "the visibility frame differs from the forward frame: {:.3} % close, ssim {ssim:.4}",
        close * 100.0
    );

    // The holes are in the picture: solid cards hide what the masked ones show.
    let solid = Fixture {
        leaf: LabMaterial {
            alpha_mode: regia_render::ALPHA_OPAQUE,
            ..f.leaf.clone()
        },
        ..fixture()
    };
    let solid = frame(&mut renderer(&device), &device, &scene(&solid, &post, false));
    let (_, solid_close) = diff(&forward, &solid);
    println!(
        "solid cards: {:.3} % within 2 levels of the masked frame",
        solid_close * 100.0
    );
    assert!(
        solid_close < 0.95,
        "masked and solid cards draw nearly the same frame ({:.3} % close): the scene cannot tell them apart",
        solid_close * 100.0
    );

    // Off again: what a renderer that stayed forward draws at the same frame number.
    let forward_scene = scene(&f, &post, false);
    let forward_later = frame(&mut forward_renderer, &device, &forward_scene);
    let off = frame(&mut vis_renderer, &device, &forward_scene);
    let (worst, close, settled) = settle(
        &mut forward_renderer,
        &mut vis_renderer,
        &device,
        &forward_scene,
        forward_later,
        off,
    );
    println!(
        "switched off: {:.3} % within 2 levels (worst {worst}), byte-identical after {settled:?} more frames",
        close * 100.0
    );
    assert!(
        close >= 0.999,
        "switching the path off differs from the forward frame: {:.3} % close, worst {worst}",
        close * 100.0
    );
    assert!(
        settled.is_some(),
        "the switched renderer never drew the forward frame again within {SETTLE} frames"
    );
}

/// The forward shadow lists are cached while nothing moves. Lists built on a visibility frame leave
/// the cluster casters out, so the path switching off has to rebuild them, or those casters are
/// gone from the shadow maps for as long as the camera holds still. No masked mesh here: this is
/// about the path, whatever it draws.
#[test]
fn switching_the_path_off_brings_the_cluster_casters_back_to_the_forward_shadows() {
    let device = match table_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let f = fixture();
    let post = post();
    let mut forward = scene(&f, &post, false);
    let mut vis = scene(&f, &post, true);
    forward.meshes.truncate(2 + BOXES);
    vis.meshes.truncate(2 + BOXES);
    let mut forward_renderer = renderer(&device);
    frame(&mut forward_renderer, &device, &forward);
    let forward_later = frame(&mut forward_renderer, &device, &forward);
    let mut switched = renderer(&device);
    frame(&mut switched, &device, &vis);
    assert!(switched.visbuffer_active(), "the frames took the path");
    let off = frame(&mut switched, &device, &forward);
    assert!(!switched.visbuffer_active(), "the path is off");
    let (worst, close, settled) = settle(
        &mut forward_renderer,
        &mut switched,
        &device,
        &forward,
        forward_later,
        off,
    );
    println!(
        "switched off, opaque: {:.3} % within 2 levels (worst {worst}), byte-identical after {settled:?} more frames",
        close * 100.0
    );
    assert!(
        close >= 0.999,
        "switching the path off differs from the forward frame: {:.3} % close, worst {worst}",
        close * 100.0
    );
    assert!(
        settled.is_some(),
        "the casters never came back: no byte-identical forward frame within {SETTLE} frames"
    );
}

#[test]
fn a_warm_masked_frame_creates_nothing() {
    let device = match table_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    }
    let f = fixture();
    let post = post();
    let mut renderer = renderer(&device);
    let s = scene(&f, &post, true);
    frame(&mut renderer, &device, &s);
    for _ in 0..3 {
        renderer.render_offscreen(&device, &s).expect("frame");
        let creates = renderer.visbuffer_create_stats().last_frame;
        assert_eq!(
            (
                creates.pipelines,
                creates.bind_groups,
                creates.buffers,
                creates.textures
            ),
            (0, 0, 0, 0),
            "a warm masked frame created {creates:?}"
        );
    }
    assert!(renderer.visbuffer_stats().masked_tested);
}

/// A masked persistent set is alpha-tested from its very first visibility frame: the renderer learns
/// the device has the table before it rebuilds the sets, so no frame draws solid cards.
#[test]
fn a_masked_set_is_masked_from_its_first_frame() {
    let device = match table_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    if regia_render::texture_table::device_support(&device.device)
        .capacity()
        .is_none()
    {
        eprintln!("skip: the adapter cannot carry the texture table");
        return;
    }
    let f = fixture();
    let post = post();
    let s = scene(&f, &post, true);
    let models: Vec<[[f32; 4]; 4]> = (0..8)
        .map(|i| translate(i as f32 * 0.4 - 1.4, 0.8, 2.0))
        .collect();
    let mut renderer = renderer(&device);
    let set = VisInstanceSet {
        mesh_key: 77,
        vertices: &f.card.0,
        indices: &f.card.1,
        material_key: 7077,
        material: &f.leaf,
        models: &models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    };
    assert!(renderer.set_instance_set(1, &set), "the set is stored");
    renderer.render_offscreen(&device, &s).expect("first frame");
    let stats = renderer.visbuffer_stats();
    assert_eq!(
        stats.masked_instances as usize,
        CARDS + models.len(),
        "first frame: the scene's cards and every instance of the set are masked"
    );
    assert!(stats.masked_tested, "first frame: the masked lists were tested");
}
