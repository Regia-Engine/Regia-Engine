// SPDX-License-Identifier: MPL-2.0
//! A quit with the pipeline warm queue running never crashes a `panic = "abort"` build (the
//! shipping profile's panic strategy). The warm's cancellation unwinds only where panics unwind;
//! under abort an unwind would kill the process at every quit during a warm (SIGABRT, or a
//! Windows fast-fail crash report), and so would a panic in the exit hook.
//!
//! Builds `examples/warm_quit.rs` with `CARGO_PROFILE_DEV_PANIC=abort` into its own target dir
//! (next to this test's, so it is cached between runs) and runs it in each mode: a plain quit,
//! a quit during a warm, and a warm queue that is never shut down. Each must exit 0 and return
//! from `main`; the two warm modes must let the job finish (joined by shutdown's wait or the
//! exit hook, never left inside a compile at exit).
//!
//! The shipping build cannot unwind a warm job, so the engine's warms stop between their
//! compiles through ordinary control flow (ADR-170 addendum). A quit in the middle of a real
//! cold frame warm must stop within the compile batch already in flight and return the job;
//! the process must join it before exit. The
//! same holds for a queue that is never shut down (the exit hook stops it).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Builds the child with `panic = "abort"`; `None` (skip) when there is no cargo to call.
fn build_child() -> Option<PathBuf> {
    static CHILD: OnceLock<Option<PathBuf>> = OnceLock::new();
    CHILD.get_or_init(compile_child).clone()
}

fn compile_child() -> Option<PathBuf> {
    let cargo = regia_core::brand::env_var_os("CARGO")?;
    let test_exe = std::env::current_exe().ok()?;
    // <target>/debug/deps/warm_quit_abort-<hash> → <target>/panic-abort
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
            "warm_quit",
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
        .join(format!("warm_quit{}", std::env::consts::EXE_SUFFIX));
    assert!(exe.exists(), "no child at {}", exe.display());
    Some(exe)
}

/// Runs the child in `mode`; returns its marker-file lines, or `None` when it skipped (no GPU).
fn run(exe: &Path, mode: &str) -> Option<Vec<String>> {
    run_timed(exe, mode).map(|(lines, _)| lines)
}

/// [`run`], also returning when the child exited (unix ms).
fn run_timed(exe: &Path, mode: &str) -> Option<(Vec<String>, u128)> {
    let dir = regia_test_utils::unique_temp_dir("regia-warm-quit");
    std::fs::create_dir_all(&dir).expect("marker directory");
    let marker = dir.join("events");
    let t0 = std::time::Instant::now();
    let status = Command::new(exe)
        .arg(mode)
        .arg(&marker)
        // A cold driver: Mesa's shader disk cache would turn the frame warms' compiles into hits.
        .env("MESA_SHADER_CACHE_DISABLE", "true")
        .env("__GL_SHADER_DISK_CACHE", "0")
        .status()
        .expect("run the child");
    let exited = unix_ms();
    let lines: Vec<String> = std::fs::read_to_string(&marker)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(reason) = lines.iter().find_map(|l| l.strip_prefix("skip: ")) {
        eprintln!("skip warm_quit_abort ({mode}): {reason}");
        return None;
    }
    assert!(
        status.success(),
        "{mode}: the panic=abort child exited with {status} (a crash on quit); marker: {lines:?}"
    );
    assert_eq!(
        lines.first().map(String::as_str),
        Some("panic=abort"),
        "{mode}: built with abort"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("main returns")),
        "{mode}: {lines:?}"
    );
    eprintln!("{mode}: exit 0 in {:?}", t0.elapsed());
    Some((lines, exited))
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

#[test]
fn quitting_with_the_warm_queue_running_never_aborts_a_panic_abort_build() {
    let Some(exe) = build_child() else {
        eprintln!("skip warm_quit_abort: no CARGO");
        return;
    };
    if run(&exe, "plain").is_none() {
        return;
    }
    for mode in ["during-warm", "no-shutdown"] {
        let lines = run(&exe, mode).expect("a device (the plain run had one)");
        assert!(
            lines.iter().any(|l| l == "finished"),
            "{mode}: the warm job was joined before the process exited: {lines:?}"
        );
    }
}

/// A quit in the middle of a real cold frame warm, in a `panic = "abort"` build: the warm
/// stops at its next pass boundary and returns as "canceled at shutdown", and the process
/// exits soon after (with `shutdown` or, never shut down, through the exit hook). Measured on
/// an idle lavapipe: `main` returns about 0.5 s after the quit and the process exits 10 ms
/// later. Elapsed times are diagnostics; cancellation is bounded by pipeline creations,
/// as in pipeline_warm's control-flow cancellation tests (ADR-2585 / ADR-4760).
/// Without the control-flow checks the job never reports cancelled (it runs on through its
/// frame warms) and the process exits only after shutdown's 2 s and the exit hook's 5 s, or
/// crashes at exit with the worker still compiling.
#[test]
fn a_quit_during_a_frame_warm_returns_promptly_in_a_panic_abort_build() {
    let Some(exe) = build_child() else {
        eprintln!("skip warm_quit_abort: no CARGO");
        return;
    };
    for mode in ["frame-warm", "frame-warm-no-shutdown"] {
        let Some((lines, exited)) = run_timed(&exe, mode) else {
            return;
        };
        let quit = value(&lines, "quit at ").expect("quit time");
        let returned = value(&lines, "main returns at ").expect("main return time");
        let quit_to_return = returned.saturating_sub(quit);
        let quit_to_exit = exited.saturating_sub(quit);
        eprintln!(
            "{mode}: main returned {quit_to_return} ms after the quit, the process exited {quit_to_exit} ms after it"
        );
        assert!(
            !lines.iter().any(|l| l == "finished"),
            "{mode}: the warm was cut short: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("job stopped: ") && l.contains("canceled at shutdown")),
            "{mode}: the warm stopped as cancelled: {lines:?}"
        );
        // The unchanged cancellation-work bound: at most the current compile batch
        // (a parallel set of up to 16 pipelines), regardless of scheduler latency.
        let late = value(&lines, "late pipelines ").expect("cancellation pipeline count");
        assert!(
            late <= 16,
            "{mode}: {late} pipelines compiled after cancellation: {lines:?}"
        );
    }
}
