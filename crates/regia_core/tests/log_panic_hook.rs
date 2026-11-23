// SPDX-License-Identifier: MPL-2.0
//! Spec 10 § 1.3: "`LogConfig` also installs a panic hook that logs the panic + backtrace and
//! flushes sinks." Logging is process-global, so this binary holds one test: it initialises
//! logging with a session file, panics on a thread, and reads the file back after the join
//! (the hook must have flushed it, and called the registered panic callback before that).
//!
//! `cargo test -p regia_core --test log_panic_hook`

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_core::log::{LogConfig, init, set_panic_callback};
use regia_test_utils::unique_temp_dir;

static CALLBACK_RAN: AtomicBool = AtomicBool::new(false);

fn on_panic() {
    CALLBACK_RAN.store(true, Ordering::SeqCst);
}

#[test]
fn the_panic_hook_logs_the_panic_with_a_backtrace_and_flushes_the_file_sink() {
    let dir = unique_temp_dir("core_panic_hook");
    let _guard = init(LogConfig {
        filter: "info".to_owned(),
        file_dir: Some(dir.clone()),
        panic_hook: true,
        ..LogConfig::default()
    })
    .expect("logging starts");
    assert!(set_panic_callback(on_panic), "no other callback in this process");
    let marker = "spec10-panic-hook-marker-7d3f";
    let joined = std::thread::Builder::new()
        .name("panicking".to_owned())
        .spawn(move || panic!("{marker}"))
        .expect("thread")
        .join();
    assert!(joined.is_err(), "the thread panicked");
    assert!(CALLBACK_RAN.load(Ordering::SeqCst), "the panic callback ran");
    let logs: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("log dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    assert_eq!(logs.len(), 1, "one session file: {logs:?}");
    let text = fs::read_to_string(&logs[0]).expect("read log");
    // `ERROR panic: panicked at <file>:<line>:<col>:` then the message on the next line.
    let at = text
        .find("ERROR panic: panicked at")
        .unwrap_or_else(|| panic!("an error on the `panic` target:\n{text}"));
    assert!(
        text[at..].contains(marker),
        "the panic message is in the file:\n{text}"
    );
    assert!(
        text.contains("backtrace:"),
        "the backtrace follows the message:\n{text}"
    );
    let _ = fs::remove_dir_all(&dir);
}
