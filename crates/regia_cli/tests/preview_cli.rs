// SPDX-License-Identifier: MPL-2.0
//! Integration tests for `regia preview` project path validation.

use std::path::Path;
use std::process::Command;

use regia_test_utils::unique_temp_dir;

fn run_preview(project: &Path, asset: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["preview", "--project"])
        .arg(project)
        .arg(asset)
        .output()
        .expect("regia preview starts")
}

#[test]
fn preview_rejects_project_escaping_asset_paths_before_cooking_or_opening_a_device() {
    let project = unique_temp_dir("preview-project");
    std::fs::write(project.join("Regia.toml"), "").expect("write project manifest");
    for (asset, expected_reason) in [
        ("../outside.glb", "must not hold `..`"),
        ("/outside.glb", "must be relative to the project"),
        ("..\\outside.glb", "must not hold `..`"),
        ("res://models/mesh.glb", "must not hold a drive letter or `:`"),
    ] {
        let output = run_preview(&project, asset);
        assert!(!output.status.success(), "asset {asset:?} must fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected_reason),
            "expected {expected_reason:?}: {stderr}"
        );
        assert!(
            !stderr.contains("adapter") && !stderr.contains("device"),
            "path validation must happen before device setup: {stderr}"
        );
    }
}

#[test]
fn preview_missing_project_fails_before_window_or_device_setup() {
    let missing = unique_temp_dir("preview-missing").join("project-does-not-exist");
    let output = run_preview(&missing, "models/mesh.glb");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("adapter") && !stderr.contains("device"),
        "missing project should fail before device setup: {stderr}"
    );
}
