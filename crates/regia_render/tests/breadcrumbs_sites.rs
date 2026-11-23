// SPDX-License-Identifier: MPL-2.0
//! G15 (ADR-2307), GPU (lavapipe counts): spec 54 test 17 (pass-site coverage, the UI overlay
//! included) and the ADR-1585 default rule as counts: a steady frame records every pass it runs,
//! interns no name after its first frame, writes exactly two markers per pass, and stays far under
//! the marker cap.
//!
//! Its own test binary: the breadcrumb writer is process-wide.

use regia_render::breadcrumbs::{self, BreadcrumbReport, MAX_MARKERS};
use regia_render::{FrameRenderer, OverlayDraw, PostSettings, RenderDevice, WindowOverlay};
use regia_test_utils::scenes::light_shadows_scene;

/// A UI layer that draws nothing: the breadcrumb is the overlay loop's, not the layer's.
struct NoopUi;

impl OverlayDraw for NoopUi {
    fn draw(
        &mut self,
        _: &RenderDevice,
        _: &mut wgpu::CommandEncoder,
        _: &wgpu::TextureView,
        _: wgpu::TextureFormat,
        _: u32,
        _: u32,
    ) {
    }
}

#[test]
fn a_steady_frame_records_its_passes_with_two_markers_each_and_no_new_names() {
    let device = RenderDevice::headless().expect("lavapipe");
    // With the post stack, so the frame has shadow, opaque, post and UI passes (spec 54 test 17).
    let mut scene = light_shadows_scene();
    scene.post = Some(PostSettings {
        auto_exposure: false,
        ..PostSettings::default()
    });
    let mut overlay = WindowOverlay::default();
    overlay.set(Box::new(NoopUi));
    // One renderer, so each call is the next frame (a new renderer restarts at frame 1).
    let mut renderer = FrameRenderer::new();
    for _ in 0..2 {
        let _ = renderer
            .render_offscreen_with_overlay(&device, &scene.as_frame_scene(), &overlay)
            .expect("warm-up");
    }
    let interns = breadcrumbs::frame_pass_interns();
    let _ = renderer
        .render_offscreen_with_overlay(&device, &scene.as_frame_scene(), &overlay)
        .expect("steady");
    assert_eq!(
        breadcrumbs::frame_pass_interns(),
        interns,
        "a steady frame takes no name-table lock (per-thread tag cache)"
    );
    let snap = breadcrumbs::snapshot();
    let newest = snap.frames.first().expect("a frame");
    assert!(
        snap.frames.len() >= 3,
        "one record per frame: {}",
        snap.frames.len()
    );
    let opens = newest.markers.iter().filter(|m| m.open).count();
    let closes = newest.markers.len() - opens;
    assert_eq!(opens, closes, "every scope closes");
    assert!(!newest.truncated);
    assert!(
        newest.markers.len() * 2 <= MAX_MARKERS,
        "{} markers",
        newest.markers.len()
    );
    let report = BreadcrumbReport::capture(&snap, None);
    let passes = &report.frames[0].passes;
    eprintln!(
        "light_shadows frame passes ({}): {}",
        passes.len(),
        passes.join(" > ")
    );
    // Spec 54 test 17's shadow, opaque, post and UI passes, plus sites this lane added.
    for expected in [
        "shadow_cascades",
        "depth_prepass",
        "cluster_lights",
        "opaque",
        "post",
        "ui",
    ] {
        assert!(
            passes.iter().any(|p| p == expected),
            "{expected} missing in {passes:?}"
        );
    }
    assert_eq!(
        passes.last().map(String::as_str),
        Some("ui"),
        "the overlay draws last: {passes:?}"
    );
}
