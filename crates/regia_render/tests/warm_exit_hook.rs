// SPDX-License-Identifier: MPL-2.0
//! Quit while a pipeline warm job is still compiling, from a runner that never shuts the queue
//! down (no `WillTerminate`): the process's exit hook cancels the job between compiles and joins
//! the worker before the process exits, so the worker's device clone is not inside the driver
//! while the driver's own exit handlers run (the v0.1.0 quit-segfault class).
//!
//! The parent test re-runs this binary as a child that runs only
//! `child_returns_with_a_warm_job_compiling` and exits. The child's warm job holds a guard that
//! writes a marker file when it unwinds; the marker exists only if something stopped the job
//! during process exit (without the hook the process exits with the job still running and the
//! guard never drops).

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use regia_render::{PipelineWarmQueue, RenderDevice, WarmTier};

const CHILD_ENV: &str = "REGIA_WARM_EXIT_CHILD";
const CHILD_TEST: &str = "child_returns_with_a_warm_job_compiling";

/// Writes `cancelled` (unwinding) or `finished` to its file when dropped.
struct Marker(PathBuf);

impl Drop for Marker {
    fn drop(&mut self) {
        let what = if std::thread::panicking() {
            "cancelled"
        } else {
            "finished"
        };
        let _ = std::fs::write(&self.0, what);
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The child (a no-op unless the parent started it): starts a warm job that would compile for
/// 20 s and returns with it running, never calling `shutdown`.
#[test]
fn child_returns_with_a_warm_job_compiling() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(marker) = regia_core::brand::env_var_os(CHILD_ENV).map(PathBuf::from) else {
        return;
    };
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            let _ = std::fs::write(&marker, format!("skip: {err}"));
            return;
        }
    };
    let queue = PipelineWarmQueue::new(1);
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    queue.push(WarmTier::Boot, "long cold warm", move |device| {
        let _marker = Marker(marker);
        let _ = started_tx.send(());
        for _ in 0..4000 {
            let _ = regia_render::pipeline_cache::cache_for(device);
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    });
    queue.start(&device);
    started
        .recv_timeout(Duration::from_secs(60))
        .expect("the warm job started");
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn process_exit_cancels_and_joins_a_compiling_warm_worker() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if regia_core::brand::env_var_os(CHILD_ENV).is_some() {
        return;
    }
    let marker = std::env::temp_dir().join(format!("regia-warm-exit-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let exe = std::env::current_exe().expect("test executable");
    let t0 = Instant::now();
    let mut child = Command::new(exe)
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, &marker)
        .spawn()
        .expect("spawn the child");
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            break status;
        }
        if t0.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            panic!("the child did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let took = t0.elapsed();
    let what = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_file(&marker);
    if let Some(reason) = what.strip_prefix("skip: ") {
        eprintln!("skip warm_exit_hook: {reason}");
        return;
    }
    assert!(status.success(), "the child exited with {status}");
    assert_eq!(
        what, "cancelled",
        "the exit hook stopped the compiling warm job before the process exited"
    );
    // The job alone would run 20 s; the exit hook waits at most 5 s.
    assert!(took < Duration::from_secs(60), "the child took {took:?}");
}
