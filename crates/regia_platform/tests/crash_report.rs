// SPDX-License-Identifier: MPL-2.0
//! End-to-end: a panic writes a crash report to the configured crash directory, the report carries a
//! real backtrace frame plus the OS and build facts, and the hook chains to a previously-installed
//! hook without panicking itself (even with no directory configured).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

/// The tests share the process-global crash directory and panic hook, so they are serialised.
static SERIAL: Mutex<()> = Mutex::new(());

/// Set by the sentinel "previous" hook the crash hook must chain to.
static SENTINEL_RAN: AtomicBool = AtomicBool::new(false);

/// Installs the sentinel hook then the crash hook, exactly once, in that order (the crash hook is
/// idempotent, so the sentinel must be installed first).
static INSTALL_HOOK: Once = Once::new();

fn install_sentinel_and_crash_hook() {
    // A sentinel "previous" hook: the crash hook must chain to it after writing the report.
    std::panic::set_hook(Box::new(|_info| {
        SENTINEL_RAN.store(true, Ordering::SeqCst);
    }));
    regia_platform::crash::install_panic_hook();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-crash-hook-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn panic_writes_a_report_and_chains_the_previous_hook() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    let dir = temp_dir("ok");
    regia_platform::crash::configure(
        Some(dir.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    INSTALL_HOOK.call_once(install_sentinel_and_crash_hook);
    SENTINEL_RAN.store(false, Ordering::SeqCst);

    let caught = std::panic::catch_unwind(|| panic!("boom: the end-to-end test"));
    assert!(caught.is_err(), "the panic was caught");

    let reports = regia_platform::crash::pending();
    assert_eq!(reports.len(), 1, "one crash report was written: {reports:?}");
    let name = reports[0].name.clone();
    let text = regia_platform::crash::read(&name).expect("readable");

    // Message and location.
    assert!(
        text.contains("boom: the end-to-end test"),
        "message captured: {text}"
    );
    assert!(text.contains("location:"), "location captured");

    // Build and OS facts are captured as specific lines, not just section headers.
    for line in [
        "build:",
        "  version:",
        "  git:",
        "  profile:",
        "system:",
        "  os:",
        "  device:",
    ] {
        assert!(text.contains(line), "missing `{line}` in:\n{text}");
    }

    // A real backtrace frame is present (not just the `backtrace:` header): the hook's own frame is
    // always on the stack.
    let backtrace = text.split_once("backtrace:").expect("backtrace header").1;
    assert!(
        backtrace.contains("write_crash_report"),
        "the backtrace has a real frame (the hook itself):\n{backtrace}"
    );

    // The previously-installed sentinel hook ran (the crash hook chained to it).
    assert!(
        SENTINEL_RAN.load(Ordering::SeqCst),
        "the previous hook was chained and ran"
    );

    regia_platform::crash::remove(&name).expect("removable");
    assert!(regia_platform::crash::pending().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_crash_dir_means_no_file_but_no_panic() {
    let _serial = SERIAL.lock().expect("test mutex poisoned");
    regia_platform::crash::configure(
        None,
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    INSTALL_HOOK.call_once(install_sentinel_and_crash_hook);

    let caught = std::panic::catch_unwind(|| panic!("no dir"));
    assert!(caught.is_err(), "the panic was caught");
    // Without a configured directory the hook writes nothing, and it must not panic itself.
    assert!(regia_platform::crash::pending().is_empty());
}
