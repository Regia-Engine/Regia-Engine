// SPDX-License-Identifier: MPL-2.0
//! Read-only copies at the built-in pass boundaries (ADR-7860), against never-captured control
//! renderers drawn at the same frame numbers. No blessed images: every comparison is between two
//! renders of this process.
use regia_render::frame_capture::{
    CaptureSession, CapturedFrame, PassCaptureError, PassCaptureRequest, pass_hook_counters,
};
use regia_render::{
    DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings,
    RenderDevice,
};

static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Frames drawn before anything is measured, in both renderers.
const WARM: usize = 3;

fn scene(post: bool) -> LabScene {
    let normal = [0.0, 0.0, 1.0];
    LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 0.0, 4.0],
        target: [0.0; 3],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 10_000.0,
        double_sided: true,
        post: post.then(|| PostSettings {
            exposure_ev: 14.5,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0; 3], 100_000.0),
        local_lights: vec![],
        materials: vec![LabMaterial {
            base_color: [0.2, 0.6, 0.1, 1.0],
            ..Default::default()
        }],
        meshes: vec![LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([-1.0, -1.0, 0.0], normal, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([1.0, -1.0, 0.0], normal, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([0.0, 1.0, 0.0], normal, [0.5, 1.0]),
            ],
            indices: vec![0, 1, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The pass-boundary copies of a capture (checkpoint observer copies are named `frame_capture_*`).
fn pass_targets(capture: &CapturedFrame) -> Vec<&regia_render::frame_capture::CapturedTarget> {
    capture
        .targets
        .iter()
        .filter(|t| !t.checkpoint.starts_with("frame_capture_"))
        .collect()
}

/// The lead's off-path rule: with no request, N frames do no work for the feature. The hook
/// runs no boundary, records no copy and allocates no staging buffer, and every counted GPU
/// object, map, poll and submit of each frame equals a never-requested renderer's at the same
/// frame number. A request that was armed and then cancelled leaves nothing behind either.
#[test]
fn pass_capture_off_adds_no_copy_allocation_or_gpu_work_over_frames() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true);
    let scene = lab.as_frame_scene();
    let mut control = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    for _ in 0..WARM {
        control.render_offscreen(&device, &scene).expect("warm control");
        renderer.render_offscreen(&device, &scene).expect("warm renderer");
    }
    renderer
        .request_pass_capture(&PassCaptureRequest::all())
        .expect("request");
    assert!(renderer.cancel_pass_capture(), "the request was armed");
    assert!(!renderer.pass_capture_requested());
    let hook_before = pass_hook_counters();
    for frame in 0..6 {
        let before = regia_render::frame_path_counters();
        control.render_offscreen(&device, &scene).expect("control frame");
        let expected = regia_render::frame_path_counters().since(before);
        let before = regia_render::frame_path_counters();
        renderer.render_offscreen(&device, &scene).expect("frame");
        let off = regia_render::frame_path_counters().since(before);
        assert_eq!(
            off, expected,
            "frame {frame}: the cancelled request changed counted GPU work"
        );
    }
    let hook = pass_hook_counters().since(hook_before);
    assert_eq!(hook.boundaries, 0, "the hook ran with no request: {hook:?}");
    assert_eq!(hook.copies, 0, "{hook:?}");
    assert_eq!(hook.staging_buffers, 0, "{hook:?}");
}

/// Read-only: a full capture (every pass boundary plus the checkpoints) leaves the frame it
/// copies, and the frame after it, byte-identical to a never-captured renderer at the same frame
/// numbers. The copies are real: the depth prepass's depth, the forward pass's color, and the
/// post stack's output; the last output copy is the final image.
#[test]
fn full_pass_capture_leaves_the_final_image_byte_identical() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true);
    let scene = lab.as_frame_scene();
    let mut control = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    for _ in 0..WARM {
        control.render_offscreen(&device, &scene).expect("warm control");
        renderer.render_offscreen(&device, &scene).expect("warm renderer");
    }
    let session = CaptureSession::arm_with_passes(&mut renderer, &PassCaptureRequest::all()).expect("arm");
    let hook_before = pass_hook_counters();
    let expected = control.render_offscreen(&device, &scene).expect("control frame");
    let captured = renderer
        .render_offscreen(&device, &scene)
        .expect("captured frame");
    let hook = pass_hook_counters().since(hook_before);
    assert!(
        expected
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel != &expected.pixels[..4]),
        "the control scene must show geometry, not a uniform clear"
    );
    assert_eq!(
        captured.pixels, expected.pixels,
        "the pass copies changed the frame"
    );
    let capture = session.finish(&mut renderer, &device).expect("read capture");
    assert!(!renderer.pass_capture_requested(), "finish released the request");
    assert!(renderer.registered_passes().is_empty());
    assert!(capture.pass_copies);
    assert_eq!(capture.graph.dispatch.violations, 0);
    let copies = pass_targets(&capture);
    assert_eq!(
        hook.copies,
        copies.iter().filter(|t| t.pixels.is_some()).count() as u64,
        "{hook:?}"
    );
    for target in &copies {
        let pass = target.after_pass.as_deref().expect("a pass copy names its pass");
        assert_eq!(pass, target.checkpoint);
        assert!(
            capture.graph.dispatch.recorded.contains(&pass),
            "`{pass}` was not recorded this frame"
        );
        assert_eq!(Some(target.frame), capture.graph.dispatch.last_frame);
        assert!(
            target
                .version
                .as_ref()
                .is_some_and(|v| v.starts_with(target.resource)),
            "{pass} {}: {:?}",
            target.resource,
            target.version
        );
    }
    let copied = |pass: &str, resource: &str| {
        copies
            .iter()
            .find(|t| t.after_pass.as_deref() == Some(pass) && t.resource == resource)
            .and_then(|t| t.pixels.as_ref())
            .unwrap_or_else(|| panic!("no `{resource}` copy after `{pass}`: {copies:#?}"))
    };
    let depth = copied("depth_prepass", "depth");
    assert!(
        depth.as_chunks::<4>().0.iter().any(|p| p != &depth[..4]),
        "the prepass depth copy shows the triangle"
    );
    let forward = capture
        .graph
        .dispatch
        .recorded
        .iter()
        .rev()
        .find(|name| ["forward", "oit_composite"].contains(name))
        .expect("forward recorded");
    let color = copied(forward, "color");
    assert!(color.as_chunks::<4>().0.iter().any(|p| p != &color[..4]));
    let last_output = copies
        .iter()
        .rev()
        .find(|t| t.resource == "output" && t.pixels.is_some())
        .expect("an output copy");
    assert_eq!(last_output.pixels.as_ref(), Some(&captured.pixels));
    for skip in &capture.skipped {
        assert!(capture.graph.dispatch.recorded.contains(&skip.pass), "{skip:?}");
        assert!(
            !["color", "depth", "output"].contains(&skip.resource) || skip.reason != "capture_internal",
            "a shared target is never internal: {skip:?}"
        );
    }
    eprintln!(
        "full capture: {} pass copies, {} copies in all, {} staging bytes, skipped {:?}",
        hook.copies, capture.copies, capture.staging_bytes, capture.skipped
    );
    // The frame after the capture: no copy, and still the control's image.
    let hook_before = pass_hook_counters();
    let expected = control.render_offscreen(&device, &scene).expect("control after");
    let after = renderer.render_offscreen(&device, &scene).expect("after");
    assert_eq!(after.pixels, expected.pixels);
    assert_eq!(pass_hook_counters().since(hook_before).boundaries, 0);
}

/// Per-capture memory: a budget that holds one depth copy records what fits, stops allocating,
/// and names every output it skipped with `capture_budget`; the image is still the control's.
#[test]
fn pass_capture_over_budget_records_what_fits_and_names_the_skipped_passes() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true);
    let scene = lab.as_frame_scene();
    let mut control = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    for _ in 0..WARM {
        control.render_offscreen(&device, &scene).expect("warm control");
        renderer.render_offscreen(&device, &scene).expect("warm renderer");
    }
    // One 64 x 48 Depth32Float copy: 256-byte rows (already aligned), 48 rows.
    let budget = 256 * 48;
    let request = PassCaptureRequest {
        passes: Vec::new(),
        budget_bytes: budget,
    };
    let session = CaptureSession::arm_with_passes(&mut renderer, &request).expect("arm");
    let expected = control.render_offscreen(&device, &scene).expect("control frame");
    let captured = renderer
        .render_offscreen(&device, &scene)
        .expect("captured frame");
    assert_eq!(captured.pixels, expected.pixels);
    let capture = session.finish(&mut renderer, &device).expect("read capture");
    let copies = pass_targets(&capture);
    let fitted: Vec<_> = copies.iter().filter(|t| t.pixels.is_some()).collect();
    assert_eq!(fitted.len(), 1, "exactly the first copy fits: {copies:#?}");
    assert_eq!(fitted[0].resource, "depth");
    let over: Vec<_> = capture
        .skipped
        .iter()
        .filter(|s| s.reason == "capture_budget")
        .collect();
    assert!(!over.is_empty(), "{:?}", capture.skipped);
    for skip in &over {
        assert!(
            copies.iter().any(|t| t.after_pass.as_deref() == Some(skip.pass)
                && t.resource == skip.resource
                && t.unavailable == Some("capture_budget")),
            "{skip:?} has its unavailable target row"
        );
    }
}

/// A pass list copies only after the sites that recorded a listed pass; an unknown name or a
/// second request is refused and arms nothing.
#[test]
fn pass_capture_filters_by_pass_and_refuses_unknown_names() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene(true);
    let scene = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(&device, &scene).expect("warm");
    let unknown = PassCaptureRequest {
        passes: vec!["gbuffer".into()],
        ..PassCaptureRequest::default()
    };
    let refused = CaptureSession::arm_with_passes(&mut renderer, &unknown).err();
    assert_eq!(refused.map(|e| e.code()), Some("capture_unknown_pass"));
    assert!(!renderer.pass_capture_requested());
    assert!(renderer.registered_passes().is_empty());
    let only = PassCaptureRequest {
        passes: vec!["depth_prepass".into()],
        ..PassCaptureRequest::default()
    };
    let session = CaptureSession::arm_with_passes(&mut renderer, &only).expect("arm");
    assert_eq!(
        renderer.request_pass_capture(&only),
        Err(PassCaptureError::Pending)
    );
    renderer
        .render_offscreen(&device, &scene)
        .expect("captured frame");
    let capture = session.finish(&mut renderer, &device).expect("read capture");
    let copies = pass_targets(&capture);
    assert!(!copies.is_empty());
    for target in copies {
        // Of the prepass's record site only the prepass writes a shared target.
        assert_eq!(target.after_pass.as_deref(), Some("depth_prepass"), "{target:?}");
        assert_eq!(target.resource, "depth");
    }
}
