// SPDX-License-Identifier: MIT-0
//! Headless smoke test for `terrain_lab`: the binary renders the shared evidence shot offscreen
//! (lavapipe in CI), runs its own luminance asserts and writes a PNG. Needs a Vulkan adapter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn evidence_shot_renders_and_writes_a_png() {
    let dir = regia_test_utils::unique_temp_dir("terrain-lab-smoke");
    let shot = dir.join("shot.png");
    let output = Command::new(env!("CARGO_BIN_EXE_terrain_lab"))
        .arg("--screenshot")
        .arg(&shot)
        .output()
        .expect("spawn terrain_lab");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "terrain_lab failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    let bytes = std::fs::read(&shot).expect("terrain_lab wrote the screenshot");
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "the screenshot is a PNG"
    );
    assert!(stdout.contains("wrote "), "stdout:\n{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}
