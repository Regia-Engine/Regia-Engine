// SPDX-License-Identifier: MPL-2.0
//! A quit in the middle of a frame, with a cold frame warm running and GPU copies in flight,
//! runs the whole clean exit (ADR-188) in a `panic = "abort"` build, destroys the device before
//! `main` returns, and the process exits 0 within budget.
//!
//! Builds `examples/clean_quit.rs` with `CARGO_PROFILE_DEV_PANIC=abort` into the same target
//! dir as `warm_quit_abort` (next to this test's, cached between runs).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds the child with `panic = "abort"`; `None` (skip) when there is no cargo to call.
fn build_child() -> Option<PathBuf> {
    let cargo = regia_core::brand::env_var_os("CARGO")?;
    let test_exe = std::env::current_exe().ok()?;
    // <target>/debug/deps/clean_quit_abort-<hash> → <target>/panic-abort
    let target = test_exe.parent()?.parent()?.parent()?.join("panic-abort");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let status = Command::new(cargo)
        .args([
            "build",
            "--offline",
            "--locked",
            "-p",
            "regia_render",
            "--example",
            "clean_quit",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_PROFILE_DEV_PANIC", "abort")
        .env_remove("RUSTFLAGS")
        .status()
        .expect("run cargo build for the panic=abort child");
    assert!(
        status.success(),
        "building the panic=abort child failed: {status}"
    );
    let exe = target
        .join("debug")
        .join("examples")
        .join(format!("clean_quit{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.exists(), "no child at {}", exe.display());
    Some(exe)
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The number after `prefix` on the marker line that starts with it.
fn value(lines: &[String], prefix: &str) -> Option<u128> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix(prefix))
        .and_then(|v| v.trim().parse().ok())
}

fn run(exe: &Path) -> Option<(Vec<String>, u128)> {
    let marker = std::env::temp_dir().join(format!("regia-clean-quit-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let status = Command::new(exe)
        .arg(&marker)
        // A cold driver: the warm keeps compiling through the quit.
        .env("MESA_SHADER_CACHE_DISABLE", "true")
        .env("__GL_SHADER_DISK_CACHE", "0")
        .env("RUST_LOG", "info")
        .status()
        .expect("run the child");
    let exited = unix_ms();
    let lines: Vec<String> = std::fs::read_to_string(&marker)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_file(&marker);
    if let Some(reason) = lines.iter().find_map(|l| l.strip_prefix("skip: ")) {
        eprintln!("skip clean_quit_abort: {reason}");
        return None;
    }
    assert!(
        status.success(),
        "the panic=abort child exited with {status} (a crash on quit); marker: {lines:?}"
    );
    Some((lines, exited))
}

#[test]
fn a_mid_frame_quit_runs_the_clean_exit_and_exits_within_budget_in_a_panic_abort_build() {
    let Some(exe) = build_child() else {
        eprintln!("skip clean_quit_abort: no CARGO");
        return;
    };
    let Some((lines, exited)) = run(&exe) else {
        return;
    };
    eprintln!("{}", lines.join("\n"));
    assert_eq!(
        lines.first().map(String::as_str),
        Some("panic=abort"),
        "built with abort"
    );
    let steps: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("step "))
        .filter_map(|l| l.split(' ').next())
        .collect();
    assert_eq!(
        steps,
        [
            "StopBackgroundWork",
            "LeaveFullscreen",
            "WaitGpuIdle",
            "DropSurfaces",
            "DropGpuResources",
            "DropDevice",
            "SaveCaches"
        ],
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l == "device gone true"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "live 0"), "{lines:?}");
    let quit = value(&lines, "quit at ").expect("quit time");
    let returned = value(&lines, "main returns at ").expect("main return time");
    let quit_to_exit = exited.saturating_sub(quit);
    eprintln!(
        "main returned {} ms after the quit, the process exited {quit_to_exit} ms after it",
        returned.saturating_sub(quit)
    );
    // The budgets add up to about 6 s at worst (a warm compile in flight, the 1 s idle and
    // device timeouts); generous for a loaded CPU running lavapipe.
    assert!(
        quit_to_exit < 15_000,
        "the process exited {quit_to_exit} ms after the quit"
    );
}
