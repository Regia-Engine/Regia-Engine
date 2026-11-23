// SPDX-License-Identifier: MIT-0
//! Windowless smoke test for the boot path: `boot_gpu` opens the offscreen device (lavapipe in
//! CI), records the splash, runs the warm queue and prints one `BOOT_GPU {json}` line. The test
//! checks that the line exists, parses, and carries the boot marks. It asserts no timings.
//! `boot_probe` itself needs a window and a compositor, so it has no automated smoke test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn boot_gpu_reports_the_boot_timeline() {
    let dir = regia_test_utils::unique_temp_dir("boot-gpu-smoke");
    let output = Command::new(env!("CARGO_BIN_EXE_boot_gpu"))
        .arg("--cache-dir")
        .arg(&dir)
        .output()
        .expect("spawn boot_gpu");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "boot_gpu failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("BOOT_GPU "))
        .unwrap_or_else(|| panic!("no BOOT_GPU line in:\n{stdout}"));
    let report: serde_json::Value = serde_json::from_str(line).expect("BOOT_GPU line is JSON");
    let marks: Vec<&str> = report["marks"]
        .as_array()
        .expect("marks array")
        .iter()
        .filter_map(|m| m["mark"].as_str())
        .collect();
    for wanted in ["device", "splash_done"] {
        assert!(marks.contains(&wanted), "mark {wanted:?} missing from {marks:?}");
    }
    assert!(report["adapter"].is_string(), "adapter name reported");
    assert!(report["pipelines_created"].is_number(), "pipeline count reported");
    let _ = std::fs::remove_dir_all(&dir);
}
