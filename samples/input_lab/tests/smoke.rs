// SPDX-License-Identifier: MIT-0
//! Smoke test for `input_lab`: the binary runs every demo (its own assertions panic on a
//! regression) with no window or display, and reports success.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn every_demo_runs_and_passes() {
    let output = Command::new(env!("CARGO_BIN_EXE_input_lab"))
        .output()
        .expect("spawn input_lab");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "input_lab failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    for section in [
        "-- keyboard / mouse --",
        "-- rebinding & persistence --",
        "all demos passed.",
    ] {
        assert!(stdout.contains(section), "missing {section:?} in:\n{stdout}");
    }
}
