// SPDX-License-Identifier: MPL-2.0
//! The end of the process and the GPU workers (ADR-287), in child processes: `finish_exit` asks
//! the registered workers to stop and waits for them. A worker that ends lets the child return
//! from its test normally (exit code 0, no deliberate exit). A worker that is still inside a
//! (fake) driver call at the end of the budget ends the child with the app's code through the
//! deliberate exit instead, so `main` never returns into the exit handlers.
//!
//! No wall clock: the stuck variant gets a zero budget through `REGIA_EXIT_WORKER_WAIT_MS`,
//! and the other variant's worker ends only when the exit's stop signal releases it.
//!
//! `cargo test -p regia_platform --test exit_workers`

use std::process::{Command, Output};
use std::sync::{Mutex, PoisonError, mpsc};

use regia_platform::{EXIT_WORKER_WAIT_ENV, GpuWorkerBuilder};

const CHILD_ENV: &str = "REGIA_EXIT_WORKERS_CHILD";
const STOP_SEEN: &str = "exit_workers: stop signalled";
const FLUSHED: &str = "exit_workers: flushed";
const RETURNED: &str = "exit_workers: finish_exit returned";
/// The code the child passes to `finish_exit` (what its app would have returned).
const APP_CODE: i32 = 5;

/// Runs [`child_end_of_process`] alone in a child process.
fn run_child(variant: &str, wait_ms: Option<&str>) -> Output {
    let exe = std::env::current_exe().expect("test executable");
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "child_end_of_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, variant)
        .env_remove(EXIT_WORKER_WAIT_ENV);
    if let Some(ms) = wait_ms {
        command.env(EXIT_WORKER_WAIT_ENV, ms);
    }
    command.output().expect("run the child")
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The child's body (does nothing in the normal test run): one registered fake GPU worker
/// inside a driver call, blocked until released, then `finish_exit`.
#[test]
fn child_end_of_process() {
    let Ok(variant) = regia_core::brand::env_var(CHILD_ENV) else {
        return;
    };
    let stuck = variant == "stuck";
    let (release, wait) = mpsc::channel::<()>();
    let (entered, started) = mpsc::channel::<()>();
    let stop_release = Mutex::new(release.clone());
    let _worker = GpuWorkerBuilder::new("fake-gpu-worker")
        .on_stop(move || {
            println!("{STOP_SEEN}");
            if !stuck {
                let _ = stop_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(());
            }
        })
        .spawn(move |worker| {
            let _call = worker.driver_call();
            let _ = entered.send(());
            let _ = wait.recv();
        })
        .expect("spawn the fake worker");
    started.recv().expect("the fake worker started");
    regia_platform::finish_exit(APP_CODE, || println!("{FLUSHED}"));
    println!("{RETURNED}");
    // Held to here: the stuck worker never sees its channel close before the exit.
    drop(release);
}

#[test]
fn a_worker_still_in_the_driver_ends_the_process_with_the_app_code() {
    let out = run_child("stuck", Some("0"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(APP_CODE), "{}", describe(&out));
    assert!(
        stdout.contains(STOP_SEEN),
        "the worker was asked to stop: {}",
        describe(&out)
    );
    assert!(
        stdout.contains(FLUSHED),
        "the log was flushed first: {}",
        describe(&out)
    );
    assert!(
        !stdout.contains(RETURNED),
        "finish_exit returned with a worker left: {}",
        describe(&out)
    );
}

#[test]
fn a_worker_that_stops_lets_the_process_end_normally() {
    // The default budget: the worker ends as soon as it is asked to stop.
    let out = run_child("stops", None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
    assert!(stdout.contains(STOP_SEEN), "{}", describe(&out));
    assert!(stdout.contains(RETURNED), "{}", describe(&out));
    assert!(
        !stdout.contains(FLUSHED),
        "no deliberate exit when every worker ended: {}",
        describe(&out)
    );
}
