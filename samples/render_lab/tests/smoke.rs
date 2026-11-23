// SPDX-License-Identifier: MIT-0
//! Headless smoke test for `render_lab`: `render_lab pt cornell` traces a tiny image with the
//! reference path tracer on the offscreen device (lavapipe in CI) and writes the EXR, PNG and
//! metadata files. Needs a Vulkan adapter. The scene is analytic, so no goldens are involved.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn path_traced_cornell_writes_its_outputs() {
    let dir = regia_test_utils::unique_temp_dir("render-lab-smoke");
    let out = dir.join("pt");
    let output = Command::new(env!("CARGO_BIN_EXE_render_lab"))
        .current_dir(&dir)
        .args([
            "pt", "cornell", "--spp", "2", "--width", "16", "--height", "16", "--out",
        ])
        .arg(&out)
        .output()
        .expect("spawn render_lab");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "render_lab failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    for name in ["cornell_pt.exr", "cornell_pt.png", "cornell_pt.json"] {
        let len = std::fs::metadata(out.join(name)).map(|m| m.len()).unwrap_or(0);
        assert!(len > 0, "{name} written and non-empty\nstdout:\n{stdout}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_scene_fails_with_a_message() {
    let output = Command::new(env!("CARGO_BIN_EXE_render_lab"))
        .args(["pt", "no_such_scene"])
        .output()
        .expect("spawn render_lab");
    assert!(!output.status.success(), "an unknown pt scene must fail");
}
