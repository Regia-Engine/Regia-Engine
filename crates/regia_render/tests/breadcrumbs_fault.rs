// SPDX-License-Identifier: MPL-2.0
//! Spec 54 tests 20 and the scripted fault path (GPU; lavapipe counts; `test-hooks`): an armed
//! fault takes the real loss path (the global device-lost flag, which the render plugin turns
//! into the hooks, the bundle and exit code 3), and the breadcrumb report names the frame.
//!
//! Its own test binary: the device-lost flag is process-wide and one-way.

use regia_render::RenderDevice;
use regia_render::breadcrumbs::fault::{self, Fault, FaultKind};
use regia_render::breadcrumbs::{self, BreadcrumbReport, EvidenceLevel};
use regia_render::gpu_health;
use regia_test_utils::scenes::render_light_shadows;

#[test]
fn a_scripted_loss_takes_the_real_path_and_the_report_names_the_passes() {
    let device = RenderDevice::headless().expect("lavapipe");
    let _ = render_light_shadows(&device).expect("warm-up");
    assert!(!gpu_health::device_lost());
    let before = breadcrumbs::snapshot().counters.submitted;
    fault::arm(Fault {
        kind: FaultKind::StallAtSubmission(before + 1),
        at_frame: 0,
    });
    let _ = render_light_shadows(&device).expect("the frame still renders");
    let _ = device.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    assert!(
        gpu_health::device_lost(),
        "the loss flag is set through mark_device_lost"
    );
    assert_eq!(
        gpu_health::device_lost_info().map(|i| i.exit_code()),
        Some(gpu_health::DEVICE_LOST_EXIT_CODE)
    );
    let info = gpu_health::device_lost_info().expect("info");
    let report = BreadcrumbReport::capture(&breadcrumbs::snapshot(), Some(&info));
    assert_eq!(report.level, EvidenceLevel::Fenced);
    assert_eq!(
        report.counters.completed, before,
        "everything from the stall on never completed"
    );
    assert!(report.counters.submitted > before);
    assert!(!report.suspects.is_empty());
    assert!(
        report.suspects.iter().any(|s| s.name == "opaque"),
        "{:?}",
        report.suspects
    );
    assert!(report.lost.is_some());
    fault::disarm();
}
