// SPDX-License-Identifier: MPL-2.0
//! A lost GPU device ends the game with a crash bundle and exit code
//! [`regia_render::DEVICE_LOST_EXIT_CODE`] within 2 s, instead of a frozen black window
//! (ADR-171). Destroying the device while the app runs (lavapipe's stand-in for a driver reset)
//! must stop the frame systems, write a crash bundle whose `crash.txt` names `device_lost`, and
//! ask the app to exit with the error code; the runner's exit transitions and the app's drop then
//! complete without a signal, and the process ends with the code through the deliberate exit
//! (ADR-276: no process-exit handlers, no second "native crash" bundle).
//!
//! Its own binary without the test harness, like `clean_exit`: the native crash monitor re-launches
//! the executable, and the child must leave `main` the way a game does. Three children run:
//! `telemetry` (`DefaultPlugins` with `TelemetryPlugin`, whose hooks feed `gpu_health`),
//! `render` (`REGIA_TELEMETRY=off`, with the render plugin's own hooks, as the windowed device
//! gets them) and `surface` (a window surface that cannot be created: reason
//! `surface_unavailable`, exit code [`regia_render::SURFACE_UNAVAILABLE_EXIT_CODE`]).
//!
//! `cargo test -p regia --test device_lost_exit`

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_framework::feedback::{FeedbackConfig, FeedbackPlugin, list_bundles};
use regia_platform::{LifecycleEvent, apply_transition};
use regia_render::{PipelineWarmQueue, RenderDevice};

const CHILD_ENV: &str = "REGIA_DEVICE_LOST_CHILD";
const VARIANT_ENV: &str = "REGIA_DEVICE_LOST_VARIANT";
const SKIP_MARK: &str = "device_lost_exit: skipped";
/// From the loss to the exit request.
const EXIT_WITHIN: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    regia_framework::feedback::run_crash_monitor_if_requested();
    match regia_core::brand::env_var_os(CHILD_ENV) {
        Some(dir) => child(Path::new(&dir)),
        None => parent(),
    }
}

fn parent() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        println!("device_lost_exit: test");
        return ExitCode::SUCCESS;
    }
    if let Some(filter) = args.iter().find(|a| !a.starts_with('-'))
        && !"device_lost_exit".contains(filter.as_str())
    {
        return ExitCode::SUCCESS;
    }
    let mut failed = false;
    for variant in ["telemetry", "render", "surface"] {
        if !run_variant(variant) {
            failed = true;
        }
    }
    if failed {
        return ExitCode::FAILURE;
    }
    println!("test device_lost_exit ... ok");
    ExitCode::SUCCESS
}

/// Runs one child; true when it passed (or skipped without a GPU).
fn run_variant(variant: &str) -> bool {
    let root = std::env::temp_dir().join(format!("regia-device-lost-{variant}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundles = root.join("feedback");
    let project = root.join("project");
    for dir in [&bundles, &project] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            eprintln!(
                "device_lost_exit[{variant}]: cannot create {}: {err}",
                dir.display()
            );
            return false;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("device_lost_exit[{variant}]: no test executable: {err}");
            return false;
        }
    };
    let mut command = Command::new(exe);
    command
        .env(CHILD_ENV, &bundles)
        .env(VARIANT_ENV, variant)
        .env("REGIA_PROJECT_DIR", &project)
        .env("XDG_DATA_HOME", root.join("xdg"))
        .env("REGIA_AUDIO", "off")
        .env_remove("REGIA_NO_NATIVE_CRASH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if variant == "render" {
        command.env("REGIA_TELEMETRY", "off");
    } else {
        command.env_remove("REGIA_TELEMETRY");
    }
    let out = match command.output() {
        Ok(out) => out,
        Err(err) => {
            eprintln!("device_lost_exit[{variant}]: cannot run the child: {err}");
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stdout.contains(SKIP_MARK) {
        println!("{}", stdout.trim());
        let _ = std::fs::remove_dir_all(&root);
        return true;
    }
    let mut problems = Vec::new();
    let (want_code, want_reason) = if variant == "surface" {
        (regia_render::SURFACE_UNAVAILABLE_EXIT_CODE, "surface_unavailable")
    } else {
        (regia_render::DEVICE_LOST_EXIT_CODE, "device_lost")
    };
    let code = out.status.code();
    if code != Some(i32::from(want_code)) {
        problems.push(format!(
            "exit {:?} (signal {:?}), wanted code {want_code}",
            code,
            signal_of(&out.status),
        ));
    }
    if !stdout.contains("device_lost_exit: exit requested") {
        problems.push("the child never saw an exit request".to_owned());
    }
    let written = list_bundles(&bundles);
    match written.first() {
        None => problems.push("no crash bundle written".to_owned()),
        Some(bundle) => {
            let crash = regia_framework::feedback::bundle::read_bundle(&bundle.path)
                .ok()
                .and_then(|entries| entries.into_iter().find(|e| e.name == "crash.txt"))
                .map(|e| String::from_utf8_lossy(&e.data).into_owned());
            match crash {
                Some(text) if text.lines().next().is_some_and(|l| l.contains(want_reason)) => {}
                Some(text) => problems.push(format!(
                    "crash.txt's first line does not name {want_reason}:\n{text}"
                )),
                None => problems.push(format!("bundle {} has no crash.txt", bundle.name)),
            }
            if written.len() > 1 {
                problems.push(format!("{} bundles written, wanted one", written.len()));
            }
        }
    }
    if problems.is_empty() {
        println!("{}", stdout.trim());
        println!(
            "device_lost_exit[{variant}]: ok (exit {want_code}, bundle {})",
            written.first().map_or("", |b| b.name.as_str())
        );
        let _ = std::fs::remove_dir_all(&root);
        true
    } else {
        eprintln!("device_lost_exit[{variant}]: FAILED: {}", problems.join("; "));
        eprintln!("--- child stdout\n{stdout}\n--- child stderr\n{stderr}");
        false
    }
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
    let variant = regia_core::brand::env_var(VARIANT_ENV).unwrap_or_default();
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            println!("{SKIP_MARK}: no GPU device ({err})");
            return ExitCode::SUCCESS;
        }
    };
    if variant == "render" {
        // What the render plugin does for the device it creates for a window.
        regia_render::gpu_health::install_device_hooks(&device.device);
    }
    let mut app = App::new();
    app.insert_resource(device.clone());
    app.add_plugins(DefaultPlugins).add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(PathBuf::from(bundles)),
            ..FeedbackConfig::default()
        },
    });
    app.finish_plugins();
    {
        let queue = app.world().resource::<PipelineWarmQueue>().clone();
        queue.set_engine_jobs(false);
        queue.start(&device);
    }
    // With telemetry, run until its probe thread holds the device (after 300 frames headless).
    let frames = if variant == "telemetry" { 320 } else { 5 };
    for _ in 0..frames {
        app.update();
    }
    if app.should_exit().is_some() {
        println!("device_lost_exit: FAILED: exit requested before the loss");
        return ExitCode::FAILURE;
    }
    if variant == "surface" {
        // What the surface path does when a window surface cannot be created after its retry
        // (no window on the farm; `surface.rs` tests the retry itself).
        regia_render::gpu_health::mark_device_lost(
            regia_render::gpu_health::SURFACE_UNAVAILABLE,
            "test: cannot create a surface for the window (injected)",
        );
    } else {
        // The loss: the device dies under the running app; the next poll reports it.
        device.device.destroy();
        let _ = device.wait_idle();
    }
    drop(device);
    let lost_at = Instant::now();
    if !regia_render::device_lost() {
        println!("device_lost_exit: FAILED: the device-lost callback did not mark the loss");
        return ExitCode::FAILURE;
    }
    let mut exit = None;
    while exit.is_none() && lost_at.elapsed() < EXIT_WITHIN {
        app.update();
        exit = app.should_exit();
    }
    let Some(exit) = exit else {
        println!("device_lost_exit: FAILED: no exit request within {EXIT_WITHIN:?} of the loss");
        return ExitCode::FAILURE;
    };
    println!(
        "device_lost_exit: exit requested {:?} after the loss: {exit:?}",
        lost_at.elapsed()
    );
    // What the desktop runner does on exit, then the app drops as `regia::run_with` returns.
    apply_transition(&mut app, LifecycleEvent::Suspended);
    apply_transition(&mut app, LifecycleEvent::WillTerminate);
    drop(app);
    // And what `regia::run_with` does next (ADR-276): the process ends here with the
    // deliberate exit code, without the process-exit handlers.
    if regia_platform::deliberate_exit_code().is_none() {
        println!("device_lost_exit: FAILED: the loss did not begin a deliberate exit");
        return ExitCode::FAILURE;
    }
    regia_platform::finish_deliberate_exit(|| {});
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
