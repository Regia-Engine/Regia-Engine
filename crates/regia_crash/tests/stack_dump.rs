// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/stack_dump.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! P-08 acceptance: a hang snapshot retains the caller and every live worker.
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use regia_crash::stack_dump::{SampleOutcome, Settings, capture, install};
use std::sync::{Arc, Barrier, Condvar, Mutex};

#[inline(never)]
fn parked_in_a_known_frame(lock: Arc<(Mutex<bool>, Condvar)>, ready: Arc<Barrier>) {
    let (mutex, cv) = &*lock;
    let mut done = mutex.lock().expect("test fixture");
    ready.wait();
    while !*done {
        done = cv.wait(done).expect("test fixture");
    }
}

static TESTS: Mutex<()> = Mutex::new(());

#[test]
fn hang_report_has_every_thread() {
    known_workers(16, "hang_report_has_every_thread");
}

#[test]
fn a_blocked_thread_shows_its_own_frames() {
    known_workers(1, "a_blocked_thread_shows_its_own_frames");
}

fn known_workers(count: usize, test_name: &str) {
    // Keep the test harness's other tests from exiting between our enumeration
    // and capture. A child running exactly this test has only stable harness
    // threads and our channel/condvar-controlled workers. Preserve every-ID checks.
    if std::env::var("REGIA_STACK_DUMP_CHILD").as_deref() != Ok(test_name) {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env("REGIA_STACK_DUMP_CHILD", test_name)
            .status()
            .expect("isolated sampler test");
        assert!(status.success(), "isolated {test_name}: {status}");
        return;
    }

    let _test = TESTS.lock().expect("test fixture");
    install().expect("install sampling handler");
    let mut workers = Vec::new();
    let mut locks = Vec::new();
    // More than the upstream group's twelve displayed identities.
    for i in 0..count {
        let lock = Arc::new((Mutex::new(false), Condvar::new()));
        let ready = Arc::new(Barrier::new(2));
        let (worker_lock, worker_ready) = (lock.clone(), ready.clone());
        workers.push(
            std::thread::Builder::new()
                .name(format!("stack-probe-{i:02}"))
                .spawn(move || parked_in_a_known_frame(worker_lock, worker_ready))
                .expect("test fixture"),
        );
        ready.wait();
        // Acquiring the mutex proves the worker reached its condvar wait.
        drop(lock.0.lock().expect("test fixture"));
        locks.push(lock);
    }
    let expected: Vec<u32> = std::fs::read_dir("/proc/self/task")
        .expect("test fixture")
        .map(|entry| {
            entry
                .expect("test fixture")
                .file_name()
                .to_str()
                .expect("test fixture")
                .parse()
                .expect("test fixture")
        })
        .collect();
    let report = capture(&Settings::default()).expect("capture");
    let limited = capture(&Settings {
        max_samples: 0,
        ..Settings::default()
    })
    .expect("test fixture");
    assert_eq!(
        limited.threads.len(),
        expected.len(),
        "limits must retain identities"
    );
    for tid in &expected {
        assert!(limited.threads.iter().any(|thread| thread.id == *tid));
    }
    assert!(
        limited
            .threads
            .iter()
            .all(|thread| thread.outcome == SampleOutcome::LimitReached)
    );
    let shallow = capture(&Settings {
        max_frames: 1,
        ..Settings::default()
    })
    .expect("test fixture");
    assert_eq!(
        shallow.threads.len(),
        expected.len(),
        "frame caps must retain identities"
    );
    for tid in &expected {
        assert!(shallow.threads.iter().any(|thread| thread.id == *tid));
    }
    assert!(
        shallow
            .threads
            .iter()
            .all(|thread| matches!(&thread.outcome, SampleOutcome::Captured(frames) if frames.len() == 1))
    );
    for lock in locks {
        *lock.0.lock().expect("test fixture") = true;
        lock.1.notify_all();
    }
    for worker in workers {
        worker.join().expect("test fixture");
    }
    for tid in expected {
        assert!(
            report.threads.iter().any(|thread| thread.id == tid),
            "missing {tid}: {report:?}"
        );
    }
    assert!(
        report
            .threads
            .iter()
            .any(|thread| thread.is_caller && matches!(thread.outcome, SampleOutcome::Captured(_)))
    );
    for i in 0..count {
        let name = format!("stack-probe-{i:02}");
        let thread = report
            .threads
            .iter()
            .find(|thread| thread.name == name)
            .expect("test fixture");
        let SampleOutcome::Captured(frames) = &thread.outcome else {
            panic!("{thread:?}");
        };
        assert!(frames.len() >= 3, "too few frames: {thread:?}");
        assert!(
            thread.stack.contains("parked_in_a_known_frame"),
            "probe frame not named: {thread:?}"
        );
        assert!(report.text().contains(&name), "omitted identity {name}");
    }
}

#[test]
fn saved_hang_report_uses_the_existing_scrubbed_crash_store() {
    let _test = TESTS.lock().expect("serialize signal captures");
    install().expect("install sampler");
    let dir = regia_test_utils::TempDir::new("saved-hang");
    regia_platform::crash::configure(
        Some(dir.path().to_owned()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    let path =
        regia_crash::stack_dump::write_hang_report("stalled token=never-save-this", &Settings::default())
            .expect("save report");
    let name = path.file_name().expect("file name").to_str().expect("UTF-8 name");
    let text = regia_platform::crash::read(name).expect("read saved report");
    assert!(text.contains("hang: stalled"));
    assert!(text.contains("[caller]"));
    assert!(!text.contains("never-save-this"));
    assert_eq!(regia_platform::crash::pending().len(), 1);
    regia_platform::crash::configure(
        None,
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
}
