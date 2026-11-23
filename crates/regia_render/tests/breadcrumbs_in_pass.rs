// SPDX-License-Identifier: MPL-2.0
//! G15 (ADR-2307), GPU (lavapipe counts), `test-hooks`: a device loss injected inside a named
//! pass yields a breadcrumb report whose first suspect is that pass, and the crash report the
//! engine writes on a loss (`regia_platform::crash::write_device_lost_report`, uploaded by
//! `regia_crash`) names it in its `gpu breadcrumbs:` block.
//!
//! Its own test binary: the device-lost flag and the crash directory are process-wide.

use regia_render::breadcrumbs::fault;
use regia_render::breadcrumbs::{self, BreadcrumbReport, EvidenceLevel};
use regia_render::gpu_health;
use regia_render::{FrameRenderer, RenderDevice};
use regia_test_utils::scenes::light_shadows_scene;

#[test]
fn a_loss_injected_in_a_pass_names_that_pass_in_both_reports() {
    let dir = regia_test_utils::unique_temp_dir("g15-in-pass");
    regia_platform::crash::configure(
        Some(dir.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    let device = RenderDevice::headless().expect("lavapipe");
    assert!(
        regia_platform::crash::report_sections().contains(&"gpu_breadcrumbs"),
        "the renderer registers its crash-report section when it picks an adapter"
    );
    let scene = light_shadows_scene();
    // One renderer, so the faulted frame has its own record (a new renderer restarts at frame 1).
    let mut renderer = FrameRenderer::new();
    let _ = renderer
        .render_offscreen(&device, &scene.as_frame_scene())
        .expect("warm-up");
    assert!(!gpu_health::device_lost());
    let names = breadcrumbs::snapshot().names;
    assert!(names.iter().any(|n| n == "opaque"), "{names:?}");

    fault::arm_in_pass("opaque", 0);
    let _ = renderer
        .render_offscreen(&device, &scene.as_frame_scene())
        .expect("the frame still renders");
    let _ = device.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    assert!(
        gpu_health::device_lost(),
        "the loss fired after the submit that carried the pass"
    );
    let info = gpu_health::device_lost_info().expect("info");
    assert!(info.message.contains("`opaque`"), "{}", info.message);
    assert_eq!(info.exit_code(), gpu_health::DEVICE_LOST_EXIT_CODE);

    let report = BreadcrumbReport::capture(&breadcrumbs::snapshot(), Some(&info));
    assert_eq!(report.level, EvidenceLevel::Timestamped);
    assert_eq!(
        report.suspects.first().map(|s| s.name.as_str()),
        Some("opaque"),
        "{:?}",
        report.suspects
    );
    let frame = &report.frames[0];
    assert_eq!(
        frame.passes.iter().filter(|p| *p == "opaque").count(),
        1,
        "{:?}",
        frame.passes
    );
    let at = frame
        .passes
        .iter()
        .position(|p| p == "opaque")
        .expect("in the frame");
    assert!(at > 0, "passes before it resolved: {:?}", frame.passes);
    let in_frame = report.suspects.iter().filter(|s| s.frame == frame.frame).count();
    assert_eq!(
        in_frame,
        frame.passes.len() - at,
        "the injected pass and the ones after it are the suspects: {:?} in {:?}",
        report.suspects,
        frame.passes
    );

    // The crash report the device-lost path writes (`surface::exit_for_lost_device`).
    let path = regia_platform::crash::write_device_lost_report(
        info.kind(),
        info.reason,
        &info.message,
        info.exit_code(),
    )
    .expect("written");
    let text = std::fs::read_to_string(&path).expect("readable");
    assert!(text.starts_with("Regia Engine crash report"), "{text}");
    assert!(
        text.contains("device_lost: unknown: injected device loss in pass `opaque`"),
        "{text}"
    );
    assert!(text.contains("gpu breadcrumbs: level 3 (timestamped)"), "{text}");
    let suspects_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("suspects ("))
        .expect("suspects line");
    let first = suspects_line
        .split(": ")
        .nth(1)
        .and_then(|s| s.split(", ").next());
    assert_eq!(first, Some("opaque"), "{suspects_line}");
    assert!(
        regia_platform::crash::pending().iter().any(|f| f.path == path),
        "the reporter's pending list has it"
    );
    fault::disarm();
    let _ = std::fs::remove_dir_all(&dir);
}
