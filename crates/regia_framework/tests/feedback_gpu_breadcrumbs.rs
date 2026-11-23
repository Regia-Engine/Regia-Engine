// SPDX-License-Identifier: MPL-2.0
//! Spec 54 test 7: a device-lost crash bundle carries `gpu_breadcrumbs.json` and the
//! `gpu breadcrumbs:` block of `crash.txt`, and lists the entry in `feedback.json`.
//!
//! Its own test binary: the crash context and the breadcrumb writer are process-wide.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::feedback::{
    FeedbackConfig, FeedbackPlugin, crash::write_device_lost_bundle, list_bundles, read_bundle,
};
use regia_framework::settings::GameSettings;
use regia_render::breadcrumbs::{self, BreadcrumbReport, CrumbList, EvidenceLevel, SubmissionId};
use regia_render::gpu_health::{DEVICE_LOST_EXIT_CODE, DeviceLostInfo};

fn submit_frame(frame: u64, passes: usize) -> SubmissionId {
    let mut list = CrumbList::new();
    for _ in 0..passes {
        drop(breadcrumbs::global().pass(&mut list, "gbuffer"));
    }
    breadcrumbs::note_submit(&[&list], frame)
}

#[test]
fn device_lost_bundle_holds_the_breadcrumb_entry() {
    let dir = regia_test_utils::unique_temp_dir("gpu-breadcrumbs-bundle");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(dir.clone()),
            ..FeedbackConfig::default()
        },
    });
    app.insert_resource(GameSettings::default());
    for _ in 0..5 {
        app.update();
    }
    // Three frames; the GPU finished the first submission only.
    let first = submit_frame(1, 3);
    submit_frame(2, 3);
    submit_frame(3, 3);
    breadcrumbs::note_completed(first);

    let info = DeviceLostInfo {
        reason: "unknown",
        message: "scripted stall".to_owned(),
    };
    assert_eq!(info.exit_code(), DEVICE_LOST_EXIT_CODE);
    write_device_lost_bundle(&info).expect("a bundle");

    let bundles = list_bundles(&dir);
    assert_eq!(bundles.len(), 1);
    let entries = read_bundle(&bundles[0].path).expect("readable");
    let get = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .data
            .clone()
    };
    let report: BreadcrumbReport = serde_json::from_slice(&get("gpu_breadcrumbs.json")).expect("parses");
    assert_eq!(report.format_version, 1);
    assert_eq!(report.level, EvidenceLevel::Fenced);
    assert_eq!(report.lost.as_ref().map(|l| l.reason.as_str()), Some("unknown"));
    assert_eq!(
        report.lost.as_ref().map(|l| l.message.as_str()),
        Some("scripted stall")
    );
    assert_eq!(report.counters.submitted, 3);
    assert_eq!(report.counters.completed, 1);
    assert_eq!(report.suspects.len(), 6, "submissions 2 and 3");
    let crash = String::from_utf8(get("crash.txt")).expect("utf8");
    assert!(crash.contains("gpu breadcrumbs: level 2 (fenced)"), "{crash}");
    let meta: serde_json::Value = serde_json::from_slice(&get("feedback.json")).expect("json");
    assert!(
        meta["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|f| f == "gpu_breadcrumbs.json"),
        "{meta}"
    );
    assert_eq!(meta["format"], 1, "BUNDLE_FORMAT stays 1");
    let _ = std::fs::remove_dir_all(&dir);
}
