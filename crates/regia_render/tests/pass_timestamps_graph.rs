// SPDX-License-Identifier: MPL-2.0
//! Per-pass GPU timestamps after the frame-graph migration (ADR-5386, burn-down J04 / M06).
//!
//! Every timestamp the frame writes goes through the frame's stamp handle, which tags it with the
//! frame-graph node whose record site was open (`FrameDispatch` publishes it). This file draws
//! Desktop High frames with GPU timing on and checks, against `render.graph`'s
//! `dispatch.recorded` for the same frame:
//!
//! - every GPU-timed pass ([`regia_render::GpuTimings::node_passes`]) is named after a node the
//!   frame recorded, and the passes come in the graph's order;
//! - every recorded node that [`regia_render::FRAME_STEPS`] says is timed has its pass (a dropped
//!   closing stamp fails here: its pass is not reported);
//! - no stamp sits outside its node ([`regia_render::GpuTimings::stamps`]): each one was written
//!   inside its owner node's record site, and the frame's first and last stamps outside every node.
//!
//! Without `TIMESTAMP_QUERY` (Metal, ADR-275) nothing is timed: the test says so on stderr and
//! checks that no stamp is reported, instead of passing on an empty list. Lavapipe has
//! timestamps, so the reference run checks everything. No golden image.

use regia_render::{
    DirectionalLightUniform, FRAME_STEPS, FrameQuality, FrameRenderer, FrameStep, GpuTimings, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

/// Frames drawn before the checked one (the first pays pipeline compiles).
const FRAMES: usize = 4;

fn scene() -> LabScene {
    let vertices = [
        LabVertex::from_pos_normal_uv([-8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.35, 0.42, 0.22, 1.0],
            roughness: 0.85,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: vertices.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// Draws `FRAMES` Desktop High frames at `render_scale` with GPU timing on (moving the camera a
/// little each frame, so the screen-space passes run) and returns the last frame's timings and
/// recorded nodes. `render_offscreen` waits for the pixels, so the timings are that frame's.
fn timed_frames(device: &RenderDevice, render_scale: f32) -> (GpuTimings, Vec<&'static str>) {
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.render_scale = render_scale;
    let quality = FrameQuality::from_profile(&profile);
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let mut lab = scene();
    lab.post = Some(post.clone());
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.ibl = true;
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(device, true);
    for i in 0..FRAMES {
        frame.eye[2] += 0.1;
        renderer
            .render_offscreen(device, &frame)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
    }
    let report = renderer.graph_report();
    assert_eq!(
        report.dispatch.violations, 0,
        "the frame dispatch is not clean: {:?}",
        report.dispatch
    );
    (renderer.gpu_timings(), report.dispatch.recorded)
}

/// The checks of the module docs; `label` names the case in failures.
fn check_timings_against_graph(label: &str, gpu: &GpuTimings, recorded: &[&'static str]) {
    let passes = gpu.node_passes();
    let stamps = gpu.stamps();
    eprintln!("{label}: recorded {recorded:?}");
    for pass in &passes {
        eprintln!("{label}: node {} pass {} {:.3} ms", pass.node, pass.pass, pass.ms);
    }
    assert!(!recorded.is_empty(), "{label}: the frame recorded no node");
    assert!(
        !passes.is_empty(),
        "{label}: timing is on, but no GPU-timed pass maps to a node (stamps {stamps:?})"
    );

    // Every timed pass is a recorded node, and the passes follow the graph's order.
    let mut previous = 0usize;
    for pass in &passes {
        let at = recorded
            .iter()
            .position(|node| *node == pass.node)
            .unwrap_or_else(|| {
                panic!(
                    "{label}: timed pass `{}` names node `{}`, which the frame did not record: {recorded:?}",
                    pass.pass, pass.node
                )
            });
        assert!(
            at >= previous,
            "{label}: timed pass `{}` (node `{}`) comes after a later node: {passes:?} vs {recorded:?}",
            pass.pass,
            pass.node
        );
        previous = at;
    }

    // Every recorded node that the frame steps time has its timed pass.
    for step in FRAME_STEPS {
        if let FrameStep::Builtin {
            name,
            timing: Some(timing),
            ..
        } = step
            && recorded.contains(name)
        {
            assert!(
                passes.iter().any(|pass| pass.pass == *timing),
                "{label}: node `{name}` was recorded and is timed as `{timing}`, but no stamped pass reports it \
                 (a closing stamp was not written): {passes:?}"
            );
        }
    }

    // No stamp sits outside its node.
    let outside: Vec<_> = stamps.iter().filter(|stamp| !stamp.in_owner).collect();
    assert!(
        outside.is_empty(),
        "{label}: stamps written outside their node's record site: {outside:?}"
    );
    for stamp in &stamps {
        assert!(
            stamp.owner == "frame" || recorded.contains(&stamp.owner),
            "{label}: stamp {} belongs to node `{}`, which the frame did not record: {recorded:?}",
            stamp.query,
            stamp.owner
        );
    }
    assert_eq!(
        stamps.iter().filter(|stamp| stamp.owner == "frame").count(),
        2,
        "{label}: the frame's first and last stamps: {stamps:?}"
    );
}

#[test]
fn every_gpu_timed_pass_is_a_recorded_graph_node_in_graph_order() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    if !device.capabilities.timestamp_query {
        // Not a pass: nothing here can be timed, so the node mapping is not checked on this adapter.
        eprintln!(
            "SKIPPED, NOT CHECKED: {} ({:?}) has no TIMESTAMP_QUERY; no pass is GPU-timed, so no stamp maps \
             to a graph node. Run this test on a timestamp adapter (lavapipe, a desktop card).",
            device.capabilities.adapter_name, device.capabilities.backend
        );
        let (gpu, _) = timed_frames(&device, 1.0);
        assert!(
            gpu.node_passes().is_empty() && gpu.stamps().is_empty() && gpu.stamp_nodes.is_empty(),
            "an adapter without timestamps reported stamps: {:?}",
            gpu.stamps()
        );
        return;
    }
    // Native: no upscale node runs a temporal pass of its own.
    let (gpu, recorded) = timed_frames(&device, 1.0);
    check_timings_against_graph("native", &gpu, &recorded);
    // The 0.67 budget: TAAU upscales, so `velocity` and `taau` record and are timed.
    let (gpu, recorded) = timed_frames(&device, 0.67);
    check_timings_against_graph("taau 0.67", &gpu, &recorded);
    assert!(
        gpu.node_passes().iter().any(|pass| pass.node == "taau"),
        "the upscaled frame times no `taau` node: {:?}",
        gpu.node_passes()
    );
}

/// Timing off: the frame writes no stamp, and no pass is reported, on every adapter.
#[test]
fn timing_off_reports_no_stamp_and_no_node_pass() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(FrameQuality::from_profile(&RenderProfile::from_preset(
        RenderProfilePreset::DesktopHigh,
    )));
    let mut renderer = FrameRenderer::new();
    for i in 0..2 {
        renderer
            .render_offscreen(&device, &frame)
            .unwrap_or_else(|err| panic!("frame {i}: {err}"));
    }
    let gpu = renderer.gpu_timings();
    assert!(gpu.stamp_nodes.is_empty(), "{:?}", gpu.stamps());
    assert!(gpu.node_passes().is_empty(), "{:?}", gpu.node_passes());
    assert!(
        !renderer.graph_report().dispatch.recorded.is_empty(),
        "the frames recorded no node"
    );
}
