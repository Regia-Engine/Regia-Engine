// SPDX-License-Identifier: MPL-2.0
//! Spec 54 tests 9 and 17 (GPU; lavapipe counts): breadcrumbs never change rendered output, and
//! one frame of a sample scene records its main passes by name.
//!
//! Its own test binary: the breadcrumb writer is process-wide.

use regia_render::RenderDevice;
use regia_render::breadcrumbs::{self, BreadcrumbConfig, BreadcrumbReport, EvidenceLevel};
use regia_test_utils::scenes::render_light_shadows;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

#[test]
fn breadcrumbs_record_main_passes_and_never_change_the_output() {
    let device = device();
    // Warm-up so lazily built state is the same for every compared run.
    let _ = render_light_shadows(&device).expect("warm-up");

    breadcrumbs::configure(BreadcrumbConfig::default());
    let on_a = render_light_shadows(&device).expect("render");
    let on_b = render_light_shadows(&device).expect("render");
    // Completion callbacks arrive during a poll.
    let _ = device.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let snap = breadcrumbs::snapshot();
    let report = BreadcrumbReport::capture(&snap, None);
    assert_ne!(report.level, EvidenceLevel::None);
    let passes = &report.frames[0].passes;
    for expected in ["shadow_cascades", "opaque"] {
        assert!(
            passes.iter().any(|p| p == expected),
            "{expected} missing in {passes:?}"
        );
    }
    assert!(report.counters.submitted > 0);
    assert_eq!(
        report.counters.completed, report.counters.submitted,
        "after a wait-poll every submission completed"
    );
    assert_eq!(report.level, EvidenceLevel::Fenced);

    breadcrumbs::configure(BreadcrumbConfig {
        enabled: false,
        ..BreadcrumbConfig::default()
    });
    let off_a = render_light_shadows(&device).expect("render");
    let off_b = render_light_shadows(&device).expect("render");
    assert_eq!(on_a, on_b, "two runs with breadcrumbs on agree");
    assert_eq!(off_a, off_b, "two runs with breadcrumbs off agree");
    assert_eq!(on_a, off_a, "breadcrumbs do not change the output");
    breadcrumbs::configure(BreadcrumbConfig::default());
}
