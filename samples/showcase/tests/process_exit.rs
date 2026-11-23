// SPDX-License-Identifier: MIT-0
//! How the real `showcase` process ends (ADR-1907): the binary, not the library, because the
//! defect this guards lived after `main`'s last line, in the exit handlers.
//!
//! A clean run must end with its own status, not a signal. That is all a software adapter can
//! show: whether a worker was inside a driver at exit is checked by `regia_platform`'s tests of
//! `finish_exit`, and the fault itself needs the driver that has it.

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use regia_render::RenderDevice;
use regia_test_utils::process::output_within;
use regia_test_utils::unique_temp_dir;

/// The longest wait for a run that has stopped making progress: a bound on a hang, not on speed.
const HANG_BOUND: Duration = Duration::from_secs(600);

fn showcase(args: &[&str]) -> Output {
    output_within(
        Command::new(env!("CARGO_BIN_EXE_showcase")).args(args),
        HANG_BOUND,
    )
    .expect("run the showcase binary")
}

fn example_scene(dir: &Path) -> String {
    let path = dir.to_str().expect("utf-8 temp path").to_string();
    let wrote = showcase(&["--write-example", &path]);
    assert!(wrote.status.success(), "{wrote:?}");
    path
}

#[test]
fn a_headless_run_ends_with_status_zero() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase process test: {err}");
        return;
    }
    let dir = unique_temp_dir("showcase-process-exit");
    let scene = example_scene(&dir);
    let run = showcase(&[
        &scene,
        "--headless",
        "--plain",
        "--size",
        "64x36",
        "--exit-after-frames",
        "3",
    ]);
    assert_eq!(run.status.code(), Some(0), "{run:?}");
    std::fs::remove_dir_all(&dir).expect("remove the temp scene");
}
