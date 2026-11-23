// SPDX-License-Identifier: MPL-2.0
//! Spec 54 § 4.1: an uncaptured validation error writes `gpu_breadcrumbs.json` with `lost`
//! null, at most once a minute, and does not take the crash-bundle slot.
//!
//! Its own test binary: the crash context and the validation gate are process-wide.

use regia_framework::feedback::crash::set_crash_dir;
use regia_framework::feedback::{list_bundles, read_bundle, write_validation_breadcrumb_bundle};
use regia_render::breadcrumbs::BreadcrumbReport;

#[test]
fn an_uncaptured_validation_error_writes_breadcrumbs_once_a_minute() {
    let dir = regia_test_utils::unique_temp_dir("validation-breadcrumbs");
    set_crash_dir(Some(dir.clone()));
    write_validation_breadcrumb_bundle("validation").expect("a bundle");
    assert!(write_validation_breadcrumb_bundle("validation").is_none());

    let bundles = list_bundles(&dir);
    assert_eq!(bundles.len(), 1, "{bundles:?}");
    let entries = read_bundle(&bundles[0].path).expect("readable");
    let get = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .data
            .clone()
    };
    let raw = String::from_utf8(get("gpu_breadcrumbs.json")).expect("utf8");
    assert!(
        raw.contains("\"lost\":null") || raw.contains("\"lost\": null"),
        "{raw}"
    );
    let report: BreadcrumbReport = serde_json::from_str(&raw).expect("parses");
    assert!(report.lost.is_none());
    let meta: serde_json::Value = serde_json::from_slice(&get("feedback.json")).expect("json");
    assert_eq!(meta["category"], "bug");
    assert_eq!(meta["note"], "uncaptured validation error");
    let _ = std::fs::remove_dir_all(&dir);
}
