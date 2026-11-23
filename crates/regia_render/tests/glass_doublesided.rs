// SPDX-License-Identifier: MPL-2.0
//! Double-sided glass on the `GLASS` pipelines (glTF `doubleSided` + a glass instance).
//!
//! The frame culls back faces. A double-sided pane takes the cull-none `GLASS` set, so a canopy
//! seen from inside still transmits. A single-sided pane stays culled from behind. After
//! [`FrameRenderer::prewarm`], a frame creates no glass pipeline.

use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FramePathKind, FrameRenderer, GlassInstance, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, frame_path_counters, take_frame_path_events,
    trace_frame_path,
};

const W: u32 = 96;
const H: u32 = 64;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// One quad in the XY plane at `z`. `face_positive_z` is the glTF front face (CCW from +Z,
/// the `+Z` face of [`regia_render::box_mesh`]).
fn sheet(z: f32, half: f32, face_positive_z: bool) -> LabMesh {
    let normal = if face_positive_z {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    let corners = [
        ([-half, -half, z], [0.0, 1.0]),
        ([half, -half, z], [1.0, 1.0]),
        ([half, half, z], [1.0, 0.0]),
        ([-half, half, z], [0.0, 0.0]),
    ];
    let mut vertices = Vec::with_capacity(4);
    for (position, uv) in corners {
        vertices.push(LabVertex::from_pos_normal_uv(position, normal, uv));
    }
    // `0,1,2` has normal +Z. The opposite winding faces −Z.
    let indices = if face_positive_z {
        vec![0, 1, 2, 0, 2, 3]
    } else {
        vec![0, 2, 1, 0, 3, 2]
    };
    LabMesh {
        vertices,
        indices,
        ..LabMesh::unskinned()
    }
}

fn backdrop() -> LabMaterial {
    LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: 4000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    }
}

fn glass(double_sided: bool) -> LabMaterial {
    LabMaterial {
        // Grey tint so transmission is obviously darker than the bare backdrop.
        base_color: [0.22, 0.22, 0.22, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        alpha_mode: ALPHA_PREMULTIPLIED,
        double_sided,
        ..LabMaterial::default()
    }
}

/// `eye` looks at the origin. The glass sheet faces +Z at z = 0. The backdrop sits on the far
/// side of the sheet and faces the camera, so a culled sheet shows the backdrop and a
/// transmitting sheet darkens it.
fn scene(eye_z: f32, double_sided: bool, with_glass: bool) -> LabScene {
    let from_positive = eye_z > 0.0;
    let backdrop_z = if from_positive { -1.2 } else { 1.2 };
    let mut meshes = vec![sheet(backdrop_z, 8.0, from_positive)];
    if with_glass {
        let mut pane = sheet(0.0, 3.0, true);
        pane.material = 1;
        meshes.push(pane);
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, eye_z],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.6,
        near: 0.05,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        // The frame culls. Double-sided is a property of the glass material.
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -0.8, 0.3], [1.0, 1.0, 1.0], 0.0),
        local_lights: Vec::new(),
        materials: vec![backdrop(), glass(double_sided)],
        meshes,
    }
}

/// Own-canopy instance: this is the `GLASS` pipeline path (a view with no damage still draws
/// it; an exterior pane with no damage does not).
fn canopy(object_key: u64) -> GlassInstance {
    GlassInstance {
        object_key,
        inside: true,
        specular_scale: 0.15,
        damage: None,
    }
}

fn render(scene: &LabScene, glass_instance: bool) -> Vec<u8> {
    let frame = scene.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    if glass_instance {
        let key = frame.meshes[1].object_key;
        renderer.set_glass(vec![canopy(key)]);
    }
    renderer
        .render_offscreen(&device(), &frame)
        .expect("frame")
        .pixels
}

fn rect_luma(px: &[u8]) -> f64 {
    let (x0, x1) = ((0.35 * W as f32) as u32, (0.65 * W as f32) as u32);
    let (y0, y1) = ((0.35 * H as f32) as u32, (0.65 * H as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            let luma =
                0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            sum += luma;
            n += 1.0;
        }
    }
    sum / n
}

fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(p, q)| p.abs_diff(*q)).max().unwrap_or(0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn double_sided_glass_seen_from_behind_transmits() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Camera on the −Z side of a +Z-facing sheet: those are back faces.
    let bare = render(&scene(-2.5, true, false), false);
    let through = render(&scene(-2.5, true, true), true);
    let bare_l = rect_luma(&bare);
    let through_l = rect_luma(&through);
    eprintln!("behind, double-sided: bare {bare_l:.2}, through glass {through_l:.2}");
    // Lavapipe + AgX maps this grey tint to about 0.62 of the bare backdrop's display luma
    // (bare ~201, through ~124). A culled pane stays at the bare luma, so 0.75 still fails it.
    assert!(
        through_l < bare_l * 0.75,
        "back faces must transmit the tinted pane: {through_l:.2} vs bare {bare_l:.2}"
    );
    assert!(
        through_l > bare_l * 0.05,
        "transmission must leave the backdrop visible: {through_l:.2} vs bare {bare_l:.2}"
    );
}

#[test]
fn single_sided_glass_is_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // From behind, back faces stay culled: the frame matches the backdrop with no pane.
    let bare_back = render(&scene(-2.5, false, false), false);
    let culled = render(&scene(-2.5, false, true), true);
    let diff = max_abs_diff(&bare_back, &culled);
    eprintln!(
        "behind, single-sided: bare {:.2}, culled {:.2}, max diff {diff}",
        rect_luma(&bare_back),
        rect_luma(&culled)
    );
    assert_eq!(diff, 0, "a single-sided pane must not draw its back faces");

    // From the front, the pane still transmits, and cull-none does not change that face.
    let bare_front = render(&scene(2.5, false, false), false);
    let single = render(&scene(2.5, false, true), true);
    let double = render(&scene(2.5, true, true), true);
    let bare_l = rect_luma(&bare_front);
    let single_l = rect_luma(&single);
    eprintln!(
        "front: bare {bare_l:.2}, single {single_l:.2}, double {:.2}, max diff {}",
        rect_luma(&double),
        max_abs_diff(&single, &double)
    );
    // Same AgX ratio as the back-face case (~0.62). 0.75 still rejects an opaque or missing pane.
    assert!(
        single_l < bare_l * 0.75,
        "single-sided glass must still transmit from the front: {single_l:.2} vs {bare_l:.2}"
    );
    assert_eq!(
        max_abs_diff(&single, &double),
        0,
        "cull-none must not change the front face of the same pane"
    );
}

#[test]
fn prewarmed_double_sided_glass_creates_no_pipeline_on_the_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = scene(-2.5, true, true);
    let frame = scene.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    renderer.prewarm(&device, &frame.meshes, 4).expect("prewarm");
    let after_prewarm = frame_path_counters().pipelines;
    renderer
        .prewarm_glass(&device, regia_render::GlassPipelineTarget::HDR)
        .expect("again");
    assert_eq!(
        frame_path_counters().pipelines,
        after_prewarm,
        "a second prewarm must not rebuild the GLASS sets"
    );

    let key = frame.meshes[1].object_key;
    renderer.set_glass(vec![canopy(key)]);
    trace_frame_path(true);
    let _ = take_frame_path_events();
    renderer.render_offscreen(&device, &frame).expect("first frame");
    let events = take_frame_path_events();
    let glass_pipelines: Vec<_> = events
        .iter()
        .filter(|event| event.kind == FramePathKind::Pipeline && event.label.contains("glass"))
        .map(|event| event.label.clone())
        .collect();
    assert!(
        glass_pipelines.is_empty(),
        "the frame compiled a GLASS pipeline after prewarm: {glass_pipelines:?}"
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
}
