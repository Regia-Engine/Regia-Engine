// SPDX-License-Identifier: MPL-2.0
//! Deliberate exits (ADR-276): the engine has decided to end the process with a documented exit
//! code (a lost GPU device: 3, no window surface: 4) and has already written the crash bundle
//! that explains why.
//!
//! Returning from `main` after such a decision ran the process's exit handlers (`atexit`, static
//! destructors) while threads that hold a clone of the dead device could still be inside the GPU
//! driver: the telemetry probe, pipeline-warm workers a bounded shutdown left compiling, material
//! graph workers. Under load on NVIDIA (615.71.09, RTX 5090) that ended in SIGSEGV instead of
//! exit code 3, and the native crash handler wrote a second, misleading bundle (soak3b v0.1.23).
//!
//! The protocol:
//!
//! 1. The bundle is written (the caller's hooks, on the frame thread).
//! 2. [`begin_deliberate_exit`]`(code)`: from here on the native crash handler
//!    ([`crate::native_crash`]) writes no bundle; a fault in the teardown of the dead device ends
//!    the process with `code` at once (`_exit` / `TerminateProcess` from the handler).
//! 3. The app exits through its normal path (the runner's clean exit, then the app drops).
//! 4. [`finish_deliberate_exit`]: logs and stdio are flushed, then the process ends with `code`
//!    through the platform's immediate exit (`_exit` on Linux, Android and macOS,
//!    `TerminateProcess` on Windows), so no process-exit destructor touches the driver.
//!    `regia::run_with` calls it after `App::run` returns; a custom host calls it before it
//!    returns from `main`.
//!
//! `std::process::abort` is never used for this: it raises SIGABRT, which the crash handler and
//! the OS report as a crash.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::gpu_worker_registry::{GpuWorkers, LeftWorker, exit_worker_wait_left, gpu_workers};

/// One process's deliberate-exit state. The process has one (`PROCESS`); tests make their own.
#[derive(Debug)]
pub struct DeliberateExit {
    /// The exit code, or [`NOT_BEGUN`].
    code: AtomicI32,
    finished: AtomicBool,
}

/// [`DeliberateExit::code`]'s value before a begin (exit codes are 0..=255).
const NOT_BEGUN: i32 = i32::MIN;

impl DeliberateExit {
    /// Not begun.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            code: AtomicI32::new(NOT_BEGUN),
            finished: AtomicBool::new(false),
        }
    }

    /// Begins the exit with `code`. True on the first call; later calls keep the first code.
    pub fn begin(&self, code: i32) -> bool {
        self.code
            .compare_exchange(NOT_BEGUN, code, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The exit code once [`DeliberateExit::begin`] ran. Async-signal-safe (one atomic load).
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        let code = self.code.load(Ordering::Acquire);
        (code != NOT_BEGUN).then_some(code)
    }

    /// The native crash handler's gate. Returns `Some(code)` when a deliberate exit has begun
    /// (the caller ends the process with it at once), else runs `arm` then `on_crash` and returns
    /// `None`. The flag is read again right before each step: `on_crash` can block for seconds in
    /// the dump request, and an exit begun meanwhile must not start a bundle. It is read once
    /// more after `on_crash`: an exit begun during the dump wins over re-raising the fault.
    pub fn run_crash_hook(&self, arm: impl FnOnce(), on_crash: impl FnOnce()) -> Option<i32> {
        if let Some(code) = self.code() {
            return Some(code);
        }
        arm();
        if let Some(code) = self.code() {
            return Some(code);
        }
        on_crash();
        self.code()
    }

    /// When begun: `flush`, then `exit(code)`, once (a second call returns without either).
    /// Returns without calling them when no exit was begun. `exit` is the platform's immediate
    /// exit in a real process and does not return; tests pass a recorder.
    pub fn finish_with(&self, flush: impl FnOnce(), exit: impl FnOnce(i32)) {
        let Some(code) = self.code() else {
            return;
        };
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        flush();
        exit(code);
    }
}

impl DeliberateExit {
    /// The end of a process (ADR-287). When no deliberate exit was begun: asks every worker of
    /// `workers` to stop and waits for them, at most `wait`. If one is still running then (it may
    /// be inside the GPU driver), the deliberate exit begins with `code`, so the process does not
    /// run its exit handlers under it. Then [`DeliberateExit::finish_with`]: `flush` and
    /// `exit(code)` when an exit was begun (here or before), nothing otherwise. A deliberate
    /// exit begun before (a lost device) does not wait: its workers may be stuck on the dead
    /// device, and the process skips the exit handlers anyway. Returns the workers left running
    /// (in a real process `exit` does not return when there are any).
    pub fn finish_after_workers(
        &self,
        workers: &GpuWorkers,
        wait: Duration,
        code: i32,
        flush: impl FnOnce(),
        exit: impl FnOnce(i32),
    ) -> Vec<LeftWorker> {
        let mut left = Vec::new();
        if self.code().is_none() {
            let start = Instant::now();
            let waited_for = workers.outstanding();
            workers.signal_stop();
            left = workers.wait(wait);
            let ms = start.elapsed().as_secs_f64() * 1e3;
            if !left.is_empty() {
                let names: Vec<String> = left.iter().map(ToString::to_string).collect();
                tracing::warn!(
                    target: "regia_platform",
                    "regia exit: {} GPU worker(s) still running after {ms:.0} ms: {}; ending the process \
                     with code {code} without process-exit handlers (ADR-287)",
                    left.len(),
                    names.join(", ")
                );
                self.begin(code);
            } else if !waited_for.is_empty() && ms >= 100.0 {
                let names: Vec<String> = waited_for.iter().map(ToString::to_string).collect();
                tracing::info!(
                    target: "regia_platform",
                    "regia exit: waited {ms:.0} ms for GPU worker(s) {}",
                    names.join(", ")
                );
            }
        }
        self.finish_with(flush, exit);
        left
    }
}

impl Default for DeliberateExit {
    fn default() -> Self {
        Self::new()
    }
}

/// This process's deliberate exit (read by the native crash handler).
pub(crate) static PROCESS: DeliberateExit = DeliberateExit::new();

/// Begins this process's deliberate exit with `code` (see the module docs): the native crash
/// handler is disarmed, and a later fault ends the process with `code`. Call it after the crash
/// bundle is written. True on the first call.
pub fn begin_deliberate_exit(code: i32) -> bool {
    let first = PROCESS.begin(code);
    if first {
        tracing::info!(
            target: "regia_platform",
            "regia exit: deliberate exit with code {code} begun; native crash bundles off, process \
             exit handlers will be skipped"
        );
    }
    first
}

/// This process's deliberate exit code, if one was begun.
#[must_use]
pub fn deliberate_exit_code() -> Option<i32> {
    PROCESS.code()
}

/// Ends the process with the deliberate exit code, if one was begun: `flush` (the host's log
/// guard), then stdout and stderr are flushed, then the platform's immediate exit runs, skipping
/// every process-exit handler and static destructor (some of which belong to the GPU driver).
///
/// Without a deliberate exit it is [`finish_exit`]`(0, flush)` (ADR-287): it waits for the
/// registered GPU workers and returns once they have all ended, or ends the process with code 0
/// when one is still running at the end of the budget. A host that knows its exit code calls
/// [`finish_exit`] instead.
pub fn finish_deliberate_exit(flush: impl FnOnce()) {
    finish_exit(0, flush);
}

/// The last call before a host returns from `main` (ADR-287; `regia::run_with` makes it
/// after `App::run`), once the app and every renderer are dropped.
///
/// 1. A deliberate exit already begun (a lost device, ADR-276) ends the process with its code
///    at once, as [`finish_deliberate_exit`] did.
/// 2. Otherwise every registered GPU worker ([`crate::gpu_workers`]) is asked to stop and waited
///    for, within what is left of the shared budget ([`crate::EXIT_WORKER_WAIT`] from the start
///    of the clean exit, [`crate::EXIT_WORKER_WAIT_ENV`] overrides it).
/// 3. When all have ended this returns, and the host returns from `main` as before.
/// 4. When one is still running (it may be inside the GPU driver), the names are logged, a
///    deliberate exit begins with `code`, `flush` and stdio run, and the process ends with `code`
///    without process-exit handlers (`_exit` / `TerminateProcess`).
pub fn finish_exit(code: i32, flush: impl FnOnce()) {
    let _ = PROCESS.finish_after_workers(
        gpu_workers(),
        exit_worker_wait_left(),
        code,
        || {
            flush();
            flush_stdio();
        },
        |code| {
            crate::platform::exit_immediately(code);
        },
    );
}

/// How long the exit waits for stdout and stderr to flush before it exits anyway.
const STDIO_FLUSH_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// Flushes stdout and stderr on a helper thread and waits at most [`STDIO_FLUSH_BOUND`]: a
/// blocked pipe must not hold the exit (or the stderr lock the exit watchdog prints under).
pub(crate) fn flush_stdio() {
    use std::io::Write as _;
    let (done, wait) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("regia-stdio-flush".into())
        .spawn(move || {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            let _ = done.send(());
        });
    if spawned.is_ok() {
        let _ = wait.recv_timeout(STDIO_FLUSH_BOUND);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    type Calls = Arc<Mutex<Vec<String>>>;

    fn push(calls: &Calls, line: String) {
        calls.lock().unwrap_or_else(PoisonError::into_inner).push(line);
    }

    fn calls_of(calls: &Calls) -> Vec<String> {
        calls.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn finish(exit: &DeliberateExit, calls: &Calls) {
        let (a, b) = (calls.clone(), calls.clone());
        exit.finish_with(
            move || push(&a, "flush".into()),
            move |c| push(&b, format!("exit {c}")),
        );
    }

    #[test]
    fn crash_hook_runs_both_steps_when_no_exit_began() {
        let exit = DeliberateExit::new();
        let calls = Calls::default();
        let (a, b) = (calls.clone(), calls.clone());
        let ended = exit.run_crash_hook(|| push(&a, "arm".into()), || push(&b, "dump".into()));
        assert_eq!(ended, None);
        assert_eq!(calls_of(&calls), ["arm", "dump"]);
    }

    #[test]
    fn crash_hook_skips_everything_when_the_exit_began_first() {
        let exit = DeliberateExit::new();
        assert!(exit.begin(3));
        let calls = Calls::default();
        let (a, b) = (calls.clone(), calls.clone());
        let ended = exit.run_crash_hook(|| push(&a, "arm".into()), || push(&b, "dump".into()));
        assert_eq!(ended, Some(3));
        assert!(calls_of(&calls).is_empty());
    }

    #[test]
    fn crash_hook_skips_the_dump_when_the_exit_begins_while_arming() {
        let exit = DeliberateExit::new();
        let calls = Calls::default();
        let b = calls.clone();
        let ended = exit.run_crash_hook(|| assert!(exit.begin(4)), || push(&b, "dump".into()));
        assert_eq!(ended, Some(4));
        assert!(calls_of(&calls).is_empty());
    }

    #[test]
    fn crash_hook_returns_the_code_when_the_exit_begins_during_the_dump() {
        let exit = DeliberateExit::new();
        let calls = Calls::default();
        let a = calls.clone();
        let ended = exit.run_crash_hook(|| push(&a, "arm".into()), || assert!(exit.begin(3)));
        assert_eq!(ended, Some(3));
        assert_eq!(calls_of(&calls), ["arm"]);
    }

    /// A registered fake GPU worker that blocks until `release` is sent (or its stop callback
    /// runs, with `ends_on_stop`), inside a marked driver call.
    fn fake_worker(
        workers: &GpuWorkers,
        ends_on_stop: bool,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, started) = std::sync::mpsc::channel::<()>();
        let stop_release = Mutex::new(release.clone());
        let mut builder = crate::GpuWorkerBuilder::new("fake-gpu-worker");
        if ends_on_stop {
            builder = builder.on_stop(move || {
                let _ = stop_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(());
            });
        }
        let handle = builder
            .spawn_in(workers, move |worker| {
                let _call = worker.driver_call();
                let _ = entered.send(());
                let _ = wait.recv();
            })
            .expect("spawn");
        started.recv().expect("the fake worker started");
        (release, handle)
    }

    fn finish_after(
        exit: &DeliberateExit,
        workers: &GpuWorkers,
        wait: Duration,
        calls: &Calls,
    ) -> Vec<LeftWorker> {
        let (a, b) = (calls.clone(), calls.clone());
        exit.finish_after_workers(
            workers,
            wait,
            5,
            move || push(&a, "flush".into()),
            move |c| push(&b, format!("exit {c}")),
        )
    }

    #[test]
    fn a_quit_waits_for_its_workers_then_returns_normally() {
        let exit = DeliberateExit::new();
        let workers = GpuWorkers::new();
        // Only the exit's stop signal ends this worker: an empty result means the exit waited.
        let (_release, handle) = fake_worker(&workers, true);
        let calls = Calls::default();
        let left = finish_after(&exit, &workers, Duration::MAX, &calls);
        assert!(left.is_empty(), "{left:?}");
        assert!(calls_of(&calls).is_empty(), "no exit: main returns as before");
        assert_eq!(exit.code(), None);
        handle.join().expect("join");
    }

    #[test]
    fn a_worker_left_at_the_end_of_the_budget_makes_the_exit_deliberate() {
        let exit = DeliberateExit::new();
        let workers = GpuWorkers::new();
        let (release, handle) = fake_worker(&workers, false);
        let calls = Calls::default();
        let left = finish_after(&exit, &workers, Duration::ZERO, &calls);
        assert_eq!(
            left,
            [LeftWorker {
                name: "fake-gpu-worker".into(),
                in_driver: true
            }]
        );
        assert_eq!(exit.code(), Some(5));
        assert_eq!(calls_of(&calls), ["flush", "exit 5"]);
        release.send(()).expect("release");
        handle.join().expect("join");
    }

    #[test]
    fn a_deliberate_exit_already_begun_does_not_wait_for_workers() {
        let exit = DeliberateExit::new();
        assert!(exit.begin(3));
        let workers = GpuWorkers::new();
        // Never released before the call: a wait would not return.
        let (release, handle) = fake_worker(&workers, false);
        let calls = Calls::default();
        let left = finish_after(&exit, &workers, Duration::MAX, &calls);
        assert!(left.is_empty());
        assert_eq!(calls_of(&calls), ["flush", "exit 3"]);
        release.send(()).expect("release");
        handle.join().expect("join");
    }

    #[test]
    fn nothing_happens_without_a_begin() {
        let exit = DeliberateExit::new();
        let calls = Calls::default();
        finish(&exit, &calls);
        assert_eq!(exit.code(), None);
        assert!(calls_of(&calls).is_empty());
    }

    #[test]
    fn first_code_wins_and_exit_runs_once_after_the_flush() {
        let exit = DeliberateExit::new();
        assert!(exit.begin(3));
        assert!(!exit.begin(4), "a second begin keeps the first code");
        assert_eq!(exit.code(), Some(3));
        let calls = Calls::default();
        finish(&exit, &calls);
        finish(&exit, &calls);
        assert_eq!(calls_of(&calls), ["flush", "exit 3"]);
    }
}
