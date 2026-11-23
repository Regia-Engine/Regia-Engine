// SPDX-License-Identifier: MPL-2.0
//! Read-only frame capture against control renders; no blessed images.
use regia_render::frame_capture::CaptureSession;
use regia_render::{
    DebugViewMode, DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    LightsViews, RenderDevice,
};

static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn capture_copies_the_same_base_color_frame_and_removes_every_observer() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let lab = scene();
    let mut scene = lab.as_frame_scene();
    scene.post = None;
    let mut renderer = FrameRenderer::new();
    renderer.set_lights_views(&LightsViews {
        shading_view: DebugViewMode::BaseColor,
        ..Default::default()
    });
    let control = renderer.render_offscreen(&device, &scene).expect("control");
    let empty = CaptureSession::arm(&mut renderer).expect("empty capture");
    assert!(
        empty.finish(&mut renderer, &device).is_err(),
        "the previous frame is not a capture"
    );
    assert!(renderer.registered_passes().is_empty());
    assert!(
        control
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel != &control.pixels[..4]),
        "control scene must contain visible geometry, not a uniform clear"
    );
    let session = CaptureSession::arm(&mut renderer).expect("arm");
    let captured = renderer.render_offscreen(&device, &scene).expect("captured");
    let before_read = regia_render::frame_path_counters();
    let capture = session.finish(&mut renderer, &device).expect("read capture");
    let reads = regia_render::frame_path_counters().since(before_read);
    assert_eq!(reads.map_asyncs, capture.copies);
    assert_eq!(reads.blocking_polls, capture.copies);
    assert_eq!(capture.graph.dispatch.violations, 0);
    for target in &capture.targets {
        assert_eq!(Some(target.frame), capture.graph.dispatch.last_frame);
        assert!(
            target
                .version
                .as_ref()
                .is_some_and(|v| v.starts_with(target.resource))
        );
        assert!(
            target
                .after_pass
                .as_ref()
                .is_some_and(|name| { capture.graph.dispatch.recorded.contains(&name.as_str()) })
        );
        if target.checkpoint == "frame_capture_after_post" {
            assert_ne!(target.after_pass.as_deref(), Some("gizmos"));
            assert_ne!(target.after_pass.as_deref(), Some("scalar_overlays"));
        }
    }
    assert_eq!(capture.graph.graph.order, renderer.graph_report().graph.order);
    assert_eq!(captured.pixels, control.pixels, "the observers changed the frame");
    let output = capture
        .targets
        .iter()
        .find(|t| t.checkpoint == "frame_capture_overlay" && t.resource == "output")
        .expect("final output");
    assert_eq!(output.pixels.as_ref().expect("pixels"), &captured.pixels);
    assert!(renderer.registered_passes().is_empty());
    assert!(capture.copies > 0);
    assert!(capture.counts.opaque.draws > 0);
    eprintln!(
        "capture on: {} copies, {} staging bytes, {} maps, {} blocking polls",
        capture.copies, capture.staging_bytes, reads.map_asyncs, reads.blocking_polls
    );
}

fn scene() -> LabScene {
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
        post: None,
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

#[test]
fn capture_off_has_no_observers_copies_or_staging_allocations() {
    let _guard = GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let lab = scene();
    let scene = lab.as_frame_scene();
    let mut control_renderer = FrameRenderer::new();
    let mut renderer = FrameRenderer::new();
    for _ in 0..3 {
        control_renderer
            .render_offscreen(&device, &scene)
            .expect("warm control");
        renderer
            .render_offscreen(&device, &scene)
            .expect("warm capture renderer");
    }
    let measure = |renderer: &mut FrameRenderer| {
        let before = regia_render::frame_path_counters();
        renderer.render_offscreen(&device, &scene).expect("frame");
        regia_render::frame_path_counters().since(before)
    };
    // Match frame indices and delayed-readback phases in both renderers.
    let control = measure(&mut control_renderer);
    let session = CaptureSession::arm(&mut renderer).expect("arm");
    session.cancel(&mut renderer);
    let off = measure(&mut renderer);
    assert!(renderer.registered_passes().is_empty());
    assert_eq!(off, control, "capture-off changed counted GPU work");
    eprintln!("capture off: {off:?}");
    let late = CaptureSession::arm(&mut renderer).expect("late collection");
    renderer.render_offscreen(&device, &scene).expect("first frame");
    renderer.render_offscreen(&device, &scene).expect("later frame");
    assert!(
        late.finish(&mut renderer, &device).is_err(),
        "later metadata must not label earlier targets"
    );
    assert!(renderer.registered_passes().is_empty());
}
