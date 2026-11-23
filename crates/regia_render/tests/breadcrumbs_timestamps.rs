// SPDX-License-Identifier: MPL-2.0
//! G15 (ADR-2307), GPU: the timestamp backing (spec 54 § 2.3). With `TimestampMode::WhenSupported`
//! on a device with `timestamp_query`, the frame turns its GPU timing query set on and every frame
//! whose end stamp resolves is recorded as finished, so the report reaches `timestamped`. Without
//! the feature the report stays `fenced` and nothing is turned on.
//!
//! Its own test binary: the breadcrumb writer is process-wide.

use regia_render::breadcrumbs::{self, BreadcrumbConfig, BreadcrumbReport, EvidenceLevel, TimestampMode};
use regia_render::{FrameRenderer, RenderDevice};
use regia_test_utils::scenes::light_shadows_scene;

#[test]
fn resolved_frames_back_the_breadcrumbs_with_timestamps() {
    let device = RenderDevice::headless().expect("headless GPU");
    breadcrumbs::configure(BreadcrumbConfig {
        timestamps: TimestampMode::WhenSupported,
        ..BreadcrumbConfig::default()
    });
    // One renderer (frames 1..=6); an offscreen read waits for the GPU and collects its timing
    // slot at once, so every frame's stamps resolve.
    let scene = light_shadows_scene();
    let mut renderer = FrameRenderer::new();
    for _ in 0..6 {
        let _ = renderer
            .render_offscreen(&device, &scene.as_frame_scene())
            .expect("render");
    }
    let _ = device.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let snap = breadcrumbs::snapshot();
    let report = BreadcrumbReport::capture(&snap, None);
    if device.capabilities.timestamp_query {
        let ts = snap.timestamps.expect("a resolved frame was recorded");
        assert!(ts.frame >= 2, "the evidence follows the frames: {ts:?}");
        let f = snap
            .frames
            .iter()
            .find(|f| f.frame == ts.frame)
            .expect("still in the ring");
        assert_eq!(
            ts.resolved_passes as usize,
            f.markers.iter().filter(|m| m.open).count(),
            "every pass of a resolved frame finished"
        );
        assert_eq!(report.level, EvidenceLevel::Timestamped);
        assert!(
            report.suspects.iter().all(|s| s.frame > ts.frame),
            "no pass of a resolved frame is a suspect: {:?}",
            report.suspects
        );
    } else {
        eprintln!("this adapter has no timestamp_query: the backing stays off (run on a card for level 3)");
        assert!(snap.timestamps.is_none());
        assert_eq!(report.level, EvidenceLevel::Fenced);
    }
    breadcrumbs::configure(BreadcrumbConfig::default());
}
