// SPDX-License-Identifier: MPL-2.0
//! Ray-traced sun shadows in the frame (ADR-2035, `book/agents/hwrt.md`).
//!
//! A box floats over a large ground under a slanted sun, seen from above, so the shadow's position
//! is known analytically. Frames of the same scene are drawn with every sun-shadow variant the
//! machine runs; the compute-BVH reference and the screen-space trace run on every adapter
//! (lavapipe in the gate). The ray-query frames need a device with ray queries: they run where the
//! adapter offers them, and are checked against the BVH reference pixel for pixel.

use regia_render::hwrt::{RtShadowRequest, SunShadowReason, SunShadowVariant};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh,
};

const W: u32 = 192;
const H: u32 = 144;
/// Half extent of the floating box, metres.
const BOX_HALF: f32 = 1.0;
/// The box's centre above the ground, metres.
const BOX_Y: f32 = 3.0;
/// Direction the sun's light travels (not unit length).
const LIGHT: [f32; 3] = [0.5, -1.0, 0.25];

fn scene(moved: f32) -> LabScene {
    let (gv, gi) = box_mesh([40.0, 0.05, 40.0]);
    let (bv, bi) = box_mesh([BOX_HALF, BOX_HALF, BOX_HALF]);
    let meshes = vec![
        LabMesh {
            vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
            indices: gi,
            material: 0,
            ..LabMesh::unskinned()
        },
        LabMesh {
            vertices: translate_mesh(&bv, [0.0, BOX_Y, 0.0]),
            indices: bi,
            material: 1,
            // The transform alone: the mesh keeps its key and its upload.
            model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(moved, 0.0, 0.0))
                .to_cols_array_2d(),
            ..LabMesh::unskinned()
        },
    ];
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 16.0, 10.0],
        target: [1.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new(LIGHT, [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.2, 0.3, 0.6, 1.0],
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn quality(request: RtShadowRequest) -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.sun_shadow_trace = request;
    quality
}

fn draw(device: &RenderDevice, renderer: &mut FrameRenderer, request: RtShadowRequest) -> Vec<u8> {
    let lab = scene(0.0);
    let frame = lab.as_frame_scene_with_quality(quality(request));
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn luma(pixels: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.25 * f32::from(pixels[i]) + 0.5 * f32::from(pixels[i + 1]) + 0.25 * f32::from(pixels[i + 2])
}

/// The pixel a world point lands on, with the frame's camera.
fn project(world: [f32; 3]) -> (u32, u32) {
    let lab = scene(0.0);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        lab.eye.into(),
        lab.target.into(),
        regia_core::glam::Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        lab.fov_y,
        W as f32 / H as f32,
        lab.near,
        lab.far,
    );
    let clip = proj * view * regia_core::glam::Vec3::from(world).extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// Ground under the box's shadow: the box's centre carried along the light onto the ground.
fn in_shadow() -> (u32, u32) {
    let t = BOX_Y / -LIGHT[1];
    project([LIGHT[0] * t, 0.0, LIGHT[2] * t])
}

/// Open ground far from the box and its shadow.
fn in_sun() -> (u32, u32) {
    project([-7.0, 0.0, 5.0])
}

fn assert_shadow_where_it_belongs(pixels: &[u8], label: &str) {
    let (sx, sy) = in_shadow();
    let (ox, oy) = in_sun();
    let (shadow, sun) = (luma(pixels, sx, sy), luma(pixels, ox, oy));
    assert!(
        shadow < sun * 0.6,
        "{label}: ground under the box ({sx}, {sy}) is {shadow:.1}, open ground ({ox}, {oy}) {sun:.1}"
    );
}

/// The reference path on any adapter: the shadow falls where the geometry says, the frame reports
/// that it traced, and a steady frame rebuilds nothing and gives the same pixels.
#[test]
fn the_compute_reference_shadows_the_ground_under_the_box() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let first = draw(&device, &mut renderer, RtShadowRequest::Compute);
    assert_shadow_where_it_belongs(&first, "compute");
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);
    assert_eq!(report.selection.reason, SunShadowReason::ComputeRequested);
    assert!(renderer.sun_trace_applied());
    assert_eq!(report.casters, 2, "the ground and the box");
    assert_eq!(report.triangles, 24, "two boxes of twelve triangles");
    assert_eq!(report.scene_builds, 1);
    assert_eq!(report.failure, None);

    let second = draw(&device, &mut renderer, RtShadowRequest::Compute);
    let report = renderer.sun_trace_report();
    assert_eq!(report.scene_builds, 1, "nothing moved, nothing rebuilt");
    assert_eq!(report.traces, 2);
    assert_eq!(first, second, "the trace is deterministic");
}

/// A caster that moves rebuilds the scene, and the shadow follows it.
#[test]
fn a_moved_caster_rebuilds_the_scene_and_moves_the_shadow() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let request = RtShadowRequest::Compute;
    let lab = scene(0.0);
    let frame = lab.as_frame_scene_with_quality(quality(request));
    let before = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    let lab = scene(6.0);
    let frame = lab.as_frame_scene_with_quality(quality(request));
    let after = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    assert_eq!(renderer.sun_trace_report().scene_builds, 2);
    let (sx, sy) = in_shadow();
    assert!(
        luma(&after, sx, sy) > luma(&before, sx, sy) * 1.5,
        "the old shadow must be gone: {} then {}",
        luma(&before, sx, sy),
        luma(&after, sx, sy)
    );
}

/// The screen-space trace on any adapter: keeps the shadow map (same forward module), adds what
/// the march finds, and asks nothing of the device.
#[test]
fn the_screen_trace_runs_on_the_shadow_map() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let pixels = draw(&device, &mut renderer, RtShadowRequest::Screen);
    assert_shadow_where_it_belongs(&pixels, "screen");
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ScreenTrace);
    assert_eq!(report.selection.reason, SunShadowReason::ScreenRequested);
    assert!(renderer.sun_trace_applied());
    assert!(!report.selection.variant.forward_key_bit());
}

/// A Desktop High frame (linear HDR, SSAO, TAA, GPU-driven batches): the traced forward module goes
/// through the instanced colour pipelines too, and the shadow falls where it should.
#[test]
fn a_desktop_high_frame_traces_through_the_gpu_driven_path() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.sun_shadow_trace = RtShadowRequest::Compute;
    let quality = FrameQuality::from_profile(&profile);
    assert!(quality.contact_shadows, "the trace runs in their place");
    let post = PostSettings::default();
    let lab = scene(0.0);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    // TAA settles over a few frames.
    for _ in 0..4 {
        pixels = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    }
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);
    assert_eq!(report.selection.reason, SunShadowReason::ComputeRequested);
    assert_eq!(report.scene_builds, 1, "the jitter does not rebuild the scene");
    assert!(report.traces >= 1);
    assert!(
        renderer.draw_stats().instanced >= 2,
        "the casters draw in GPU-driven batches: {:?}",
        renderer.draw_stats()
    );
    assert_shadow_where_it_belongs(&pixels, "desktop high");
}

/// A transmissive surface blends over what is behind it, so it is not in the depth the trace
/// starts from, whatever its alpha mode says: it keeps the shadow map. Here the box shadows a pane
/// held over lit ground; the visibility texture at those pixels describes the lit ground.
#[test]
fn a_transmissive_pane_keeps_the_shadow_map_under_the_trace() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    // The box high above a pane, so what shows through the pane under its shadow is lit ground.
    let lift = 5.0;
    let pane_y = BOX_Y + lift - 2.0;
    let mut lab = scene(0.0);
    lab.meshes[1].model =
        regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(0.0, lift, 0.0))
            .to_cols_array_2d();
    let (pv, pi) = box_mesh([12.0, 0.02, 12.0]);
    lab.materials.push(LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.6,
        transmission: 0.5,
        ..LabMaterial::default()
    });
    lab.meshes.push(LabMesh {
        vertices: translate_mesh(&pv, [0.0, pane_y, 0.0]),
        indices: pi,
        material: 2,
        ..LabMesh::unskinned()
    });
    // The box's centre carried along the light onto the pane, and pane pixels clear of its shadow
    // with lit ground behind them too.
    let t = 2.0 / -LIGHT[1];
    let (sx, sy) = project([LIGHT[0] * t, pane_y, LIGHT[2] * t]);
    let (ox, oy) = project([-2.0, pane_y, 2.0]);
    let frame = |request| {
        let frame = lab.as_frame_scene_with_quality(quality(request));
        FrameRenderer::new()
            .render_offscreen(&device, &frame)
            .expect("frame")
            .pixels
    };
    let mapped = frame(RtShadowRequest::Off);
    let traced = frame(RtShadowRequest::Compute);
    let (mapped_shadow, mapped_open) = (luma(&mapped, sx, sy), luma(&mapped, ox, oy));
    let traced_shadow = luma(&traced, sx, sy);
    println!(
        "pane: shadow map {mapped_shadow:.1} in shadow, {mapped_open:.1} open; traced {traced_shadow:.1}"
    );
    assert!(
        mapped_shadow < mapped_open - 8.0,
        "the box must shadow the pane under the shadow map: {mapped_shadow:.1} against {mapped_open:.1}"
    );
    assert!(
        (traced_shadow - mapped_shadow).abs() < 4.0,
        "the pane takes its sun from the shadow map: traced {traced_shadow:.1}, shadow map {mapped_shadow:.1}"
    );
}

/// A device without ray queries answers `on` with the screen-space trace, and names why.
#[test]
fn on_takes_what_the_device_has() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    assert!(
        !device
            .device
            .features()
            .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
        "nothing asked this process's default device for ray queries"
    );
    let mut renderer = FrameRenderer::new();
    draw(&device, &mut renderer, RtShadowRequest::On);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ScreenTrace);
    assert_eq!(report.selection.reason, SunShadowReason::ScreenFallback);
    assert!(renderer.sun_trace_applied());
}

/// The knob off draws exactly the frame a renderer draws that never saw it, also after it was on:
/// the state is released and the shadow-map pipelines come back.
#[test]
fn the_knob_off_is_the_default_frame_also_after_tracing() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut plain = FrameRenderer::new();
    let reference = draw(&device, &mut plain, RtShadowRequest::Off);
    assert!(!plain.sun_trace_applied());
    assert_eq!(plain.sun_trace_report().traces, 0);
    assert_eq!(
        plain.sun_trace_report().selection.reason,
        SunShadowReason::NotRequested
    );

    let mut renderer = FrameRenderer::new();
    let traced = draw(&device, &mut renderer, RtShadowRequest::Compute);
    assert_ne!(traced, reference, "the variants differ on this scene");
    assert!(renderer.sun_trace_applied());
    // Temporal state is off on the lab baseline, so a frame depends only on this frame's inputs.
    let back = draw(&device, &mut renderer, RtShadowRequest::Off);
    assert!(!renderer.sun_trace_applied());
    assert_eq!(renderer.sun_trace_report().traces, 0, "the state was released");
    assert_eq!(back, reference, "the knob off is the default frame");
}

/// The frame graph lists the trace and its forward module's warm key, and a default game-frame warm
/// asks nothing for them.
#[test]
fn the_graph_names_the_trace_and_its_warm_keys() {
    let renderer = FrameRenderer::new();
    let graph = renderer.graph_report().graph;
    assert!(
        graph.warm_keys.contains(&"sun_shadow_trace"),
        "{:?}",
        graph.warm_keys
    );
    assert!(
        graph.warm_keys.contains(&"forward_sun_rt"),
        "{:?}",
        graph.warm_keys
    );
    let node = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    let trace = node("sun_shadow_trace");
    assert!(trace.live);
    assert_eq!(trace.reads, ["depth#1", "contact#1"]);
    assert_eq!(trace.writes, ["contact#2"]);
    assert!(
        node("contact_shadows").live,
        "both write one texture, and both stay"
    );
    assert_eq!(node("contact_shadows").writes, ["contact#1"]);
    assert!(node("forward").reads.iter().any(|r| r == "contact#2"));
    assert_eq!(node("forward").pipelines, ["forward_sun_rt", "forward_studio"]);
}

/// A scene the BVH refuses (a caster with a non-finite transform) traces through the depth buffer
/// on that frame, keeps the shadow map from the next, says why, and tries again when the request
/// changes.
#[test]
fn a_refused_scene_hands_over_to_the_shadow_map() {
    let device = RenderDevice::shared_headless().expect("lavapipe");
    let mut renderer = FrameRenderer::new();
    let request = RtShadowRequest::Compute;
    let mut lab = scene(0.0);
    for vertex in &mut lab.meshes[1].vertices {
        vertex.position[0] = f32::NAN;
    }
    let frame = lab.as_frame_scene_with_quality(quality(request));
    renderer.render_offscreen(&device, &frame).expect("refused frame");
    let report = renderer.sun_trace_report();
    assert_eq!(report.failure, Some("bvh_failed"));
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);

    renderer.render_offscreen(&device, &frame).expect("next frame");
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ShadowMap);
    assert_eq!(report.selection.reason, SunShadowReason::AccelFailed);
    assert!(!renderer.sun_trace_applied());

    let good = scene(0.0);
    let frame = good.as_frame_scene_with_quality(quality(RtShadowRequest::Screen));
    renderer.render_offscreen(&device, &frame).expect("screen frame");
    assert_eq!(
        renderer.sun_trace_report().selection.reason,
        SunShadowReason::ScreenRequested
    );
    let frame = good.as_frame_scene_with_quality(quality(request));
    let pixels = renderer.render_offscreen(&device, &frame).expect("again").pixels;
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::ComputeFallback);
    assert_eq!(report.failure, None);
    assert_shadow_where_it_belongs(&pixels, "after the refusal");
}

/// What each variant costs on this adapter, printed (`--nocapture`), never asserted: 1600 instances
/// of one box over the ground at 1080p, with GPU timing on. Run it on the card being measured.
#[test]
#[ignore = "a measurement for a real adapter (book/agents/hwrt.md)"]
fn sun_trace_cost_on_this_adapter() {
    let device = RenderDevice::headless_with_ray_queries().expect("device");
    let has_rq = device
        .device
        .features()
        .contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY);
    let (bv, bi) = box_mesh([0.4, 0.4, 0.4]);
    let (gv, gi) = box_mesh([60.0, 0.05, 60.0]);
    let mut lab = scene(0.0);
    lab.width = 1920;
    lab.height = 1080;
    lab.eye = [0.0, 30.0, 40.0];
    lab.meshes = vec![LabMesh {
        vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
        indices: gi,
        material: 0,
        ..LabMesh::unskinned()
    }];
    for i in 0..40 {
        for j in 0..40 {
            lab.meshes.push(LabMesh {
                vertices: translate_mesh(&bv, [i as f32 * 2.0 - 40.0, 1.0, j as f32 * 2.0 - 40.0]),
                indices: bi.clone(),
                material: 1,
                ..LabMesh::unskinned()
            });
        }
    }
    let mut requests = vec![RtShadowRequest::Off, RtShadowRequest::Screen];
    if has_rq {
        requests.push(RtShadowRequest::On);
    }
    requests.push(RtShadowRequest::Compute);
    println!("adapter: {}", device.adapter.get_info().name);
    for request in requests {
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let start = std::time::Instant::now();
        let frame = lab.as_frame_scene_with_quality(quality(request));
        renderer.render_offscreen(&device, &frame).expect("first");
        let first_ms = start.elapsed().as_secs_f32() * 1000.0;
        // The camera moves every frame: a held view reuses its screen passes, the trace included.
        let frames = 24;
        let (mut trace_ms, mut forward_ms, mut wall_ms) = (0.0, 0.0, 0.0);
        for i in 0..frames {
            let mut moved = lab.clone();
            moved.eye[0] += 0.05 * (i + 1) as f32;
            let frame = moved.as_frame_scene_with_quality(quality(request));
            let start = std::time::Instant::now();
            renderer.render_offscreen(&device, &frame).expect("frame");
            // The first frames' timings are still on their way back.
            if i >= frames / 2 {
                wall_ms += start.elapsed().as_secs_f32() * 1000.0;
                let gpu = renderer.gpu_timings();
                trace_ms += gpu.contact_shadows_ms;
                forward_ms += gpu.forward_ms;
            }
        }
        let counted = (frames - frames / 2) as f32;
        let report = renderer.sun_trace_report();
        println!(
            "{:>8} {:>16} first {first_ms:9.1} ms, frame (wall, with readback) {:8.2} ms, trace pass {:7.3} ms, forward {:7.3} ms, traces {}, scene builds {}, casters {}, blas {}",
            request.as_str(),
            report.selection.variant.as_str(),
            wall_ms / counted,
            trace_ms / counted,
            forward_ms / counted,
            report.traces,
            report.scene_builds,
            report.casters,
            report.accel.blas_resident,
        );
    }
}

/// The ray-query frame against the compute reference, pixel for pixel. Runs where the adapter
/// offers ray queries (lavapipe with `VK_KHR_ray_query`, an RTX card, an RX 7900).
#[test]
fn ray_query_frames_match_the_compute_reference() {
    let Some(device) = regia_test_utils::hwrt::ray_query_device() else {
        return;
    };
    let mut reference = FrameRenderer::new();
    let bvh = draw(&device, &mut reference, RtShadowRequest::Compute);

    let mut renderer = FrameRenderer::new();
    let rq = draw(&device, &mut renderer, RtShadowRequest::On);
    let report = renderer.sun_trace_report();
    assert_eq!(report.selection.variant, SunShadowVariant::RayQuery);
    assert_eq!(report.failure, None);
    assert_eq!(report.accel.instances, 2);
    assert_eq!(report.accel.blas_built, 2);
    assert_shadow_where_it_belongs(&rq, "ray query");

    let mut differing = 0usize;
    for (a, b) in rq.as_chunks::<4>().0.iter().zip(bvh.as_chunks::<4>().0) {
        if a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > 2) {
            differing += 1;
        }
    }
    let pixels = (W * H) as usize;
    assert!(
        differing * 200 <= pixels,
        "{differing} of {pixels} pixels differ between ray queries and the BVH"
    );

    // A steady frame builds nothing: the same BLAS, the same TLAS.
    let again = draw(&device, &mut renderer, RtShadowRequest::On);
    let report = renderer.sun_trace_report();
    assert_eq!(report.scene_builds, 1);
    assert_eq!(rq, again, "the ray-query frame is deterministic");

    // A moved caster refits the TLAS (ADR-2310) and builds no BLAS: its mesh key is the same upload.
    let lab = scene(6.0);
    let frame = lab.as_frame_scene_with_quality(quality(RtShadowRequest::On));
    let moved = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    let report = renderer.sun_trace_report();
    assert_eq!(report.scene_builds, 1, "a transform is a refit, not a rebuild");
    assert_eq!(report.tlas_refits, 1);
    assert!(report.accel.refit);
    assert_eq!(report.accel.instances_moved, 1);
    assert_eq!(report.accel.instances, 2);
    assert_eq!(report.accel.blas_built, 0, "a transform changes no geometry");
    let (sx, sy) = in_shadow();
    assert!(
        luma(&moved, sx, sy) > luma(&rq, sx, sy) * 1.5,
        "the shadow must follow the moved instance"
    );
}
