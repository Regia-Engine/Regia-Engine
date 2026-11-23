// SPDX-License-Identifier: MPL-2.0
//! A normal quit exits 0 and writes no crash bundle (v0.1.0 blocker: every quit segfaulted on
//! NVIDIA because the telemetry probe thread dropped the last `wgpu::Device` reference while the
//! process's exit handlers tore the driver down).
//!
//! Its own binary without the test harness: the native crash monitor re-launches the executable
//! (`run_crash_monitor_if_requested` must run first in `main`), and the child must leave `main`
//! the way a game does. The parent re-runs this binary as the child, which brings up
//! `DefaultPlugins` (with `TelemetryPlugin`) and `FeedbackPlugin` with native crash bundles on a
//! device of the machine (lavapipe in CI), runs frames until the telemetry probe holds the
//! device, starts the pipeline warm workers, quits through the runner's clean exit (every wgpu
//! object must be gone when it returns), drops the app, checks that no thread holding GPU objects survived the drop, and returns from
//! `main`. The parent then requires exit status 0 and an empty crash-bundle folder.
//!
//! `cargo test -p regia --test clean_exit`

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use regia::prelude::*;
#[cfg(target_os = "linux")]
use regia_framework::feedback::NativeCrashStatus;
use regia_framework::feedback::{FeedbackConfig, FeedbackPlugin, list_bundles};
use regia_platform::{ExitStep, LifecycleEvent, run_clean_exit};
use regia_render::{PipelineWarmQueue, RenderDevice};
use regia_telemetry::Telemetry;

const CHILD_ENV: &str = "REGIA_CLEAN_EXIT_CHILD";
const SKIP_MARK: &str = "clean_exit: skipped";

/// Names (as the kernel truncates them, 15 bytes) of engine threads that hold `wgpu` objects.
/// None may outlive the app: each must be joined before the device drops.
const GPU_THREADS: [&str; 3] = ["regia-telemet", "regia-pipelin", "regia-matgrap"];

fn main() -> ExitCode {
    regia_framework::feedback::run_crash_monitor_if_requested();
    match regia_core::brand::env_var_os(CHILD_ENV) {
        Some(dir) => child(Path::new(&dir)),
        None => parent(),
    }
}

fn parent() -> ExitCode {
    // libtest-style arguments (`--list`, filters) from `cargo test`: list ourselves, run otherwise.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("clean_exit: test");
        return ExitCode::SUCCESS;
    }
    if let Some(filter) = args.iter().find(|a| !a.starts_with('-'))
        && !"clean_exit".contains(filter.as_str())
    {
        return ExitCode::SUCCESS;
    }
    let root = std::env::temp_dir().join(format!("regia-clean-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundles = root.join("feedback");
    let project = root.join("project");
    for dir in [&bundles, &project] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!("clean_exit: cannot create {}: {err}", dir.display());
            return ExitCode::FAILURE;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("clean_exit: no test executable: {err}");
            return ExitCode::FAILURE;
        }
    };
    let out = Command::new(exe)
        .env(CHILD_ENV, &bundles)
        .env("REGIA_PROJECT_DIR", &project)
        .env("XDG_DATA_HOME", root.join("xdg"))
        .env("REGIA_AUDIO", "off")
        .env_remove("REGIA_TELEMETRY")
        .env_remove("REGIA_NO_NATIVE_CRASH")
        .output();
    let out = match out {
        Ok(out) => out,
        Err(err) => {
            eprintln!("clean_exit: cannot run the child: {err}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stdout.contains(SKIP_MARK) {
        println!("{}", stdout.trim());
        let _ = std::fs::remove_dir_all(&root);
        return ExitCode::SUCCESS;
    }
    // A crash bundle is written by the monitor process right after a fault; give it a moment.
    std::thread::sleep(Duration::from_millis(500));
    let written = list_bundles(&bundles);
    let mut failed = false;
    if !out.status.success() {
        eprintln!(
            "clean_exit: FAILED: the child's normal quit ended with {:?} (signal {:?})",
            out.status.code(),
            signal_of(&out.status)
        );
        failed = true;
    }
    if !written.is_empty() {
        let names: Vec<&str> = written.iter().map(|b| b.name.as_str()).collect();
        eprintln!("clean_exit: FAILED: a normal quit wrote crash bundle(s): {names:?}");
        failed = true;
    }
    if failed {
        eprintln!("--- child stdout\n{stdout}\n--- child stderr\n{stderr}");
        return ExitCode::FAILURE;
    }
    println!("{}", stdout.trim());
    println!("test clean_exit ... ok (exit 0, no crash bundle)");
    let _ = std::fs::remove_dir_all(&root);
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn child(bundles: &Path) -> ExitCode {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            println!("{SKIP_MARK}: no GPU device ({err})");
            return ExitCode::SUCCESS;
        }
    };
    let adapter = device.adapter.get_info().name;
    let mut app = App::new();
    app.insert_resource(device.clone());
    app.add_plugins(DefaultPlugins).add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(PathBuf::from(bundles)),
            ..FeedbackConfig::default()
        },
    });
    app.finish_plugins();
    // Native crash capture must be live, or "no crash bundle" proves nothing (Linux only).
    #[cfg(target_os = "linux")]
    {
        let armed = regia_framework::feedback::native::wait_until_armed(Duration::from_secs(10));
        if armed != NativeCrashStatus::Armed {
            println!("clean_exit: FAILED: native crash capture is {armed:?}, not armed");
            return ExitCode::FAILURE;
        }
    }
    // The pipeline warm workers hold the device too (no engine jobs: an idle pool).
    {
        let queue = app.world().resource::<PipelineWarmQueue>().clone();
        queue.set_engine_jobs(false);
        queue.start(&device);
    }
    drop(device);
    // Telemetry starts its probe (with the device) once the session header is out: after 300
    // frames when nothing renders.
    for _ in 0..320 {
        app.update();
    }
    let probe_running = app
        .world()
        .get_resource::<Telemetry>()
        .is_some_and(|t| t.session_json().is_some());
    if !probe_running {
        println!("clean_exit: FAILED: the telemetry session (and its probe) never started");
        return ExitCode::FAILURE;
    }
    let before = gpu_threads();
    // What the desktop runner does on quit (the clean exit, ADR-188), then the app drops as
    // `regia::run_with` returns.
    let report = run_clean_exit(
        &mut app,
        &[LifecycleEvent::Suspended, LifecycleEvent::WillTerminate],
    );
    let gpu = regia_render::last_gpu_exit_report();
    let live = gpu.as_ref().and_then(|g| g.live_after);
    drop(app);
    let after = gpu_threads();
    println!("clean_exit: {adapter}: GPU threads before quit {before:?}, after the app dropped {after:?}");
    println!("clean_exit: clean exit in {:?}; renderer {gpu:?}", report.total);
    if report.steps.len() != ExitStep::ALL.len() {
        println!(
            "clean_exit: FAILED: the clean exit ran {} steps",
            report.steps.len()
        );
        return ExitCode::FAILURE;
    }
    match live {
        Some(live) if live.total() == 0 => {}
        other => {
            println!("clean_exit: FAILED: wgpu objects outlived the clean exit (device included): {other:?}");
            return ExitCode::FAILURE;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if !before.iter().any(|t| t.starts_with("regia-telemet")) {
            println!("clean_exit: FAILED: no telemetry probe thread was running");
            return ExitCode::FAILURE;
        }
        if !after.is_empty() {
            println!("clean_exit: FAILED: threads holding GPU objects outlived the app: {after:?}");
            return ExitCode::FAILURE;
        }
    }
    // The end of `regia::run_with` (ADR-287): every registered GPU worker ends, so this
    // returns. A worker still running would end the process here with code 7 (reported by the
    // parent as a failed quit).
    regia_platform::finish_exit(7, || {});
    println!("clean_exit: every registered GPU worker ended");
    // Return from `main` right away, as a game does after `regia::run*`.
    ExitCode::SUCCESS
}

/// The live threads of this process whose names are in [`GPU_THREADS`] (Linux; empty elsewhere).
fn gpu_threads() -> Vec<String> {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut names: Vec<String> = tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| GPU_THREADS.iter().any(|g| name.starts_with(g)))
        .collect();
    names.sort();
    names
}
