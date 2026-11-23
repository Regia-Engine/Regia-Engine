// SPDX-License-Identifier: MPL-2.0
//! Double-sided materials on the GPU-driven instanced path (glTF `doubleSided`).
//!
//! The frame culls back faces. A double-sided standard material draws with cull-none pipelines,
//! and the cull mode is part of the instanced batch key: its meshes batch and instance in the
//! depth prepass, the shadow maps and the forward colour pass exactly like single-sided ones,
//! instead of falling back to one CPU draw per object, and a prewarmed level creates no pipeline
//! on the frame path.

use regia_render::{
    DirectionalLightUniform, FramePathKind, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabTexture, LabVertex, PostSettings, RenderDevice, frame_path_counters, take_frame_path_events,
    trace_frame_path,
};

const W: u32 = 128;
const H: u32 = 96;
/// Shared mesh key of the fins: one GPU mesh, so the instanced path can batch them.
const FIN_MESH: u64 = 0xF1D0_0B1E;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Instancing needs `INDIRECT_FIRST_INSTANCE` + multi-draw indirect.
fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

fn quality(gpu: bool) -> FrameQuality {
    FrameQuality {
        gpu_driven: gpu,
        ..FrameQuality::LAB
    }
}

fn translate(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// A quad in the XY plane through the origin, front face +Z (CCW seen from +Z), tangent +X.
fn fin(half: f32) -> LabMesh {
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// Ground plane facing +Y.
fn ground(half: f32) -> LabMesh {
    let corners = [
        [-half, 0.0, -half],
        [half, 0.0, -half],
        [half, 0.0, half],
        [-half, 0.0, half],
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&position| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }
}

fn fin_material(double_sided: bool, normal_map: Option<LabTexture>) -> LabMaterial {
    LabMaterial {
        base_color: [0.7, 0.45, 0.3, 1.0],
        roughness: 0.6,
        normal_scale: if normal_map.is_some() { 1.0 } else { 0.0 },
        normal_map,
        double_sided,
        ..LabMaterial::default()
    }
}

fn base_scene(eye: [f32; 3], target: [f32; 3], sun_travel: [f32; 3], shadows: bool) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: shadows,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        // The frame culls. Double-sided is a property of the fin material.
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new(sun_travel, [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    }
}

/// A ground and a 4 × 4 field of double-sided fins facing +Z, seen from −Z (their back faces)
/// with the sun behind the camera: the fins' back faces are lit and the fins shadow the ground.
fn fin_field() -> LabScene {
    let mut scene = base_scene([0.6, 2.2, -9.0], [0.0, 0.6, 0.0], [-0.25, -0.6, 0.75], true);
    scene.materials = vec![
        LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        },
        fin_material(true, None),
    ];
    scene.meshes.push(ground(12.0));
    for i in 0..16 {
        let (x, z) = ((i % 4) as f32 * 1.6 - 2.4, (i / 4) as f32 * 1.6 - 2.4);
        scene.meshes.push(LabMesh {
            material: 1,
            model: translate(x, 0.55, z),
            ..fin(0.5)
        });
    }
    scene
}

fn share_fin_mesh(frame: &mut FrameScene<'_>) {
    for mesh in frame.meshes.iter_mut().skip(1) {
        mesh.mesh_key = FIN_MESH;
    }
}

fn rect_luma(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> f64 {
    let (x0, x1) = ((x0 * W as f32) as u32, (x1 * W as f32) as u32);
    let (y0, y1) = ((y0 * H as f32) as u32, (y1 * H as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn centre_luma(px: &[u8]) -> f64 {
    rect_luma(px, 0.4, 0.6, 0.4, 0.6)
}

/// Max channel difference and the pixels differing by more than `tolerance`.
fn compare(a: &[u8], b: &[u8], tolerance: u8) -> (u8, usize) {
    let mut max = 0u8;
    let mut over = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        if d > tolerance {
            over += 1;
        }
    }
    (max, over)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn double_sided_draws_batch_into_the_instanced_lists() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = fin_field();
    let render = |gpu: bool| {
        let mut frame = scene.as_frame_scene_with_quality(quality(gpu));
        share_fin_mesh(&mut frame);
        let mut renderer = FrameRenderer::new();
        let image = renderer.render_offscreen(&device, &frame).expect("frame");
        (image.pixels, renderer.draw_stats())
    };
    let (per_object, cpu_stats) = render(false);
    assert_eq!(cpu_stats.per_object, 17, "{cpu_stats:?}");
    assert_eq!(cpu_stats.double_sided_per_object, 16, "{cpu_stats:?}");
    let (instanced, gpu_stats) = render(true);
    if !gpu_driven(&device) {
        eprintln!(
            "device has no indirect first-instance: the GPU-driven path falls back to per-object draws"
        );
        return;
    }
    eprintln!("cpu {cpu_stats:?}\ngpu {gpu_stats:?}");
    // Ground + 16 double-sided fins, all in instanced batches: nothing on the CPU loop.
    assert_eq!(gpu_stats.instanced, 17, "{gpu_stats:?}");
    assert_eq!(gpu_stats.per_object, 0, "{gpu_stats:?}");
    assert_eq!(gpu_stats.double_sided_per_object, 0, "{gpu_stats:?}");
    // Same picture as the per-object cull-none draws (depth, shadows and colour).
    let (max, over) = compare(&per_object, &instanced, 1);
    eprintln!("instanced vs per-object double-sided draws: max {max}/255, {over} px over 1/255");
    assert_eq!(
        over, 0,
        "the instanced double-sided path must render the per-object frame (max {max})"
    );
    // The fins are there: their lit back faces are brighter than the shadowed ground around them.
    assert!(
        centre_luma(&instanced) > 20.0,
        "fins missing: {:.1}",
        centre_luma(&instanced)
    );
}

/// One fin at the origin seen along ∓Z. `from_front`: camera on +Z (front face) with the sun
/// behind it; otherwise the whole setup turned 180° about Y (camera on −Z, the back face).
/// `sun_behind_fin` puts the sun on the far side of the fin instead (the visible face unlit).
fn single_fin(
    from_front: bool,
    sun_behind_fin: bool,
    double_sided: bool,
    normal_map: Option<LabTexture>,
) -> LabScene {
    // Front setup: camera on +Z, sun travelling toward −Z from the upper right.
    let (mut eye, mut sun) = ([0.0, 0.0, 3.0], [-0.55, -0.25, -0.8]);
    if sun_behind_fin {
        sun[2] = -sun[2];
    }
    if !from_front {
        // 180° about Y: (x, y, z) → (−x, y, −z).
        eye = [-eye[0], eye[1], -eye[2]];
        sun = [-sun[0], sun[1], -sun[2]];
    }
    let mut scene = base_scene(eye, [0.0, 0.0, 0.0], sun, true);
    scene.materials = vec![fin_material(double_sided, normal_map)];
    scene.meshes.push(fin(1.2));
    scene
}

fn render_gpu(scene: &LabScene) -> Vec<u8> {
    let device = device();
    let frame = scene.as_frame_scene_with_quality(quality(true));
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &frame).expect("frame").pixels
}

#[test]
fn double_sided_back_face_renders_lit() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let front = render_gpu(&single_fin(true, false, true, None));
    let back = render_gpu(&single_fin(false, false, true, None));
    let back_unlit = render_gpu(&single_fin(false, true, true, None));
    let culled = render_gpu(&single_fin(false, false, false, None));
    let (front_l, back_l, unlit_l, culled_l) = (
        centre_luma(&front),
        centre_luma(&back),
        centre_luma(&back_unlit),
        centre_luma(&culled),
    );
    eprintln!(
        "front {front_l:.1}, back {back_l:.1}, back with the sun behind {unlit_l:.1}, single-sided back {culled_l:.1}"
    );
    // The back face shades with the flipped normal: lit like the front face of the mirrored setup
    // (the same caster in the shadow map, so no self-shadowing either side).
    assert!(back_l > 40.0, "the back face must be lit: {back_l:.1}");
    assert!(
        (back_l - front_l).abs() <= 2.0,
        "the back face must shade like the front face: back {back_l:.1} vs front {front_l:.1}"
    );
    assert!(
        back_l > unlit_l * 3.0,
        "a sun behind the fin must leave its back face dark: {unlit_l:.1}"
    );
    // Single-sided: the back face is culled, the clear shows through.
    assert!(
        culled_l < back_l * 0.25,
        "a single-sided back face must stay culled: {culled_l:.1}"
    );
}

#[test]
fn prewarmed_double_sided_level_creates_no_pipeline_on_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = fin_field();
    let mut frame = scene.as_frame_scene_with_quality(quality(true));
    share_fin_mesh(&mut frame);
    let mut renderer = FrameRenderer::new();
    renderer.prewarm(&device, &frame.meshes, 64).expect("prewarm");
    let after_prewarm = frame_path_counters().pipelines;
    renderer
        .prewarm_double_sided(&device, regia_render::GlassPipelineTarget::HDR)
        .expect("again");
    assert_eq!(
        frame_path_counters().pipelines,
        after_prewarm,
        "a second prewarm must not rebuild the cull-none pipelines"
    );

    trace_frame_path(true);
    let _ = take_frame_path_events();
    renderer.render_offscreen(&device, &frame).expect("first frame");
    let cull_none: Vec<_> = take_frame_path_events()
        .iter()
        .filter(|event| event.kind == FramePathKind::Pipeline && event.label.contains("cull none"))
        .map(|event| event.label.clone())
        .collect();
    assert!(
        cull_none.is_empty(),
        "the frame compiled a cull-none pipeline after prewarm: {cull_none:?}"
    );

    // The rest of the frame (forward, post) settles, then nothing new is created.
    for _ in 0..4 {
        renderer.render_offscreen(&device, &frame).expect("warm frame");
    }
    let pipelines = frame_path_counters().pipelines;
    for _ in 0..3 {
        renderer.render_offscreen(&device, &frame).expect("steady frame");
    }
    assert_eq!(
        frame_path_counters().pipelines,
        pipelines,
        "a frame after warm-up created a pipeline"
    );
    trace_frame_path(false);
    if gpu_driven(&device) {
        let stats = renderer.draw_stats();
        assert_eq!(stats.per_object, 0, "{stats:?}");
    }
}
