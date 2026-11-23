// SPDX-License-Identifier: MPL-2.0
//! Native crash bundles end to end (ADR-158): a child process segfaults for real, the crash
//! monitor (this binary re-launched) writes a bundle with a minidump, and `cargo xtask minidump`'s
//! reader parses it and walks the crashing thread. Also: a panic followed by `abort()` (what
//! `panic = "abort"` does) still writes exactly one panic bundle, a clean exit writes none, and a
//! SIGSEGV sent with `kill` kills the game at once (not at the 5 s watchdog) and writes a bundle.
//!
//!
//! Windows (ADR-5806): the same child, with capture opted in (`native_crashes_windows`). An access
//! violation writes one bundle with a minidump that the reader parses and walks, the game still
//! dies of `EXCEPTION_ACCESS_VIOLATION`, a panic followed by `abort()` (a fast fail) writes exactly
//! one panic bundle, and a clean exit writes none. The stalled-monitor and sent-signal checks are
//! Linux only. Other targets print a skip line, which `windows-pass` records as `skipped`.
//!
//! `harness = false`: the binary's `main` must call `run_crash_monitor_if_requested` first.

fn main() {
    regia_framework::feedback::run_crash_monitor_if_requested();
    #[cfg(any(target_os = "linux", windows))]
    native::main();
    #[cfg(not(any(target_os = "linux", windows)))]
    println!("native_crash: skipped (native crash bundles are Linux and Windows only)");
}

#[cfg(any(target_os = "linux", windows))]
mod native {
    #[cfg(target_os = "linux")]
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus};
    use std::time::{Duration, Instant};

    use regia_app::prelude::*;
    use regia_framework::feedback::native::wait_until_armed;
    use regia_framework::feedback::{
        FeedbackCategory, FeedbackConfig, FeedbackHost, FeedbackPlugin, NativeCrashStatus, list_bundles,
        read_bundle, unannounced_crashes,
    };
    use regia_framework::settings::GameSettings;

    const MODE_ENV: &str = "REGIA_NATIVE_CRASH_TEST_MODE";
    const DIR_ENV: &str = "REGIA_NATIVE_CRASH_TEST_DIR";
    /// "await-kill" mode: the child creates this file once armed, then waits to be killed.
    const READY_ENV: &str = "REGIA_NATIVE_CRASH_TEST_READY";

    pub fn main() {
        match std::env::var(MODE_ENV).as_deref() {
            Ok(mode) => child(mode),
            Err(_) => driver(),
        }
    }

    /// The game: records frames and host fields, waits until capture is armed, then crashes.
    fn child(mode: &str) {
        let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap_or_default());
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.add_plugins(FeedbackPlugin {
            config: FeedbackConfig {
                dir: Some(dir),
                // Capture is opt-in on Windows; ignored on Linux.
                native_crashes_windows: true,
                ..FeedbackConfig::default()
            },
        });
        app.insert_resource(GameSettings::default());
        let status = wait_until_armed(Duration::from_secs(15));
        if status != NativeCrashStatus::Armed {
            eprintln!("native crash capture did not arm: {status:?}");
            std::process::exit(3);
        }
        app.world_mut()
            .resource_mut::<FeedbackHost>()
            .set("match_id", "m-native");
        for _ in 0..30 {
            app.update();
        }
        regia_core::log::ring::push(
            "WARN",
            "native_crash_test",
            "the last line before the crash".into(),
        );
        match mode {
            "segv" => segv(),
            "panic-abort" => {
                let _ = std::panic::catch_unwind(|| panic!("native crash test: panic, then abort"));
                std::process::abort();
            }
            "await-kill" => {
                if let Ok(ready) = std::env::var(READY_ENV) {
                    let _ = std::fs::write(ready, std::process::id().to_string());
                }
                // The parent sends `kill -SEGV`; exit 4 if it never comes.
                for _ in 0..600 {
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::process::exit(4);
            }
            _ => {} // "clean": a normal exit
        }
    }

    #[allow(unsafe_code)]
    #[inline(never)]
    fn segv() {
        // SAFETY: not safe, on purpose: this is the crash under test. A volatile store to an
        // unmapped page is a real hardware fault (SIGSEGV, SEGV_MAPERR; on Windows an access
        // violation writing address 0x10), not a raised signal.
        unsafe { std::ptr::write_volatile(std::ptr::without_provenance_mut::<u32>(0x10), 0xdead) };
    }

    fn run_child(mode: &str, dir: &Path) -> ExitStatus {
        run_child_env(mode, dir, &[])
    }

    fn run_child_env(mode: &str, dir: &Path, env: &[(&str, &str)]) -> ExitStatus {
        let exe = std::env::current_exe().expect("test exe");
        let mut command = Command::new(exe);
        command.env(MODE_ENV, mode).env(DIR_ENV, dir);
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command.spawn().expect("spawn child");
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{mode}: the child hung (a crash handler must never hang the game)");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The monitor writes the bundle after the game died: wait for it.
    fn wait_for_bundles(
        dir: &Path,
        n: usize,
        timeout: Duration,
    ) -> Vec<regia_framework::feedback::BundleFile> {
        let deadline = Instant::now() + timeout;
        loop {
            let bundles = list_bundles(dir);
            if bundles.len() >= n || Instant::now() > deadline {
                return bundles;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn text(entries: &[regia_framework::feedback::zip::ZipEntry], name: &str) -> String {
        let e = entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        String::from_utf8_lossy(&e.data).into_owned()
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-native-crash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(target_os = "linux")]
    fn driver() {
        segfault_writes_a_bundle_with_a_readable_minidump();
        panic_then_abort_writes_one_panic_bundle();
        clean_exit_writes_nothing();
        a_stuck_monitor_cannot_freeze_the_crashed_game();
        a_sent_segv_kills_at_once_and_writes_a_bundle();
        println!("native_crash: 5 checks passed");
    }

    #[cfg(windows)]
    fn driver() {
        segfault_writes_a_bundle_with_a_readable_minidump();
        panic_then_abort_writes_one_panic_bundle();
        clean_exit_writes_nothing();
        println!("native_crash: 3 checks passed (Windows)");
    }

    /// `EXCEPTION_ACCESS_VIOLATION`, the exit code of a process that dies of one.
    #[cfg(windows)]
    const ACCESS_VIOLATION: u32 = 0xC000_0005;
    /// `STATUS_STACK_BUFFER_OVERRUN`: `std::process::abort` is a fast fail on Windows.
    #[cfg(windows)]
    const FAST_FAIL: u32 = 0xC000_0409;

    /// The game died of the fault under test, as it would have without capture.
    fn assert_died_of_the_fault(status: ExitStatus) {
        #[cfg(target_os = "linux")]
        assert_eq!(
            status.signal(),
            Some(11),
            "the game still dies of SIGSEGV: {status:?}"
        );
        #[cfg(windows)]
        assert_eq!(
            status.code().map(i32::cast_unsigned),
            Some(ACCESS_VIOLATION),
            "the game still dies of EXCEPTION_ACCESS_VIOLATION: {status:?}"
        );
    }

    /// The process ended in `abort()`.
    fn assert_aborted(status: ExitStatus) {
        #[cfg(target_os = "linux")]
        assert_eq!(status.signal(), Some(6), "{status:?}");
        #[cfg(windows)]
        assert_eq!(
            status.code().map(i32::cast_unsigned),
            Some(FAST_FAIL),
            "{status:?}"
        );
    }

    /// `(crash.txt line, feedback.json signal, minidump reader reason)` of the fault under test.
    #[cfg(target_os = "linux")]
    const FAULT: (&str, &str, &str) = ("native crash: SIGSEGV (address not mapped)", "SIGSEGV", "SIGSEGV");
    #[cfg(windows)]
    const FAULT: (&str, &str, &str) = (
        "native crash: EXCEPTION_ACCESS_VIOLATION (write to an inaccessible address)",
        "EXCEPTION_ACCESS_VIOLATION",
        "EXCEPTION_ACCESS_VIOLATION",
    );

    fn segfault_writes_a_bundle_with_a_readable_minidump() {
        let dir = fresh_dir("segv");
        let status = run_child("segv", &dir);
        assert_died_of_the_fault(status);
        let (crash_line, signal, reason) = FAULT;
        let bundles = wait_for_bundles(&dir, 1, Duration::from_secs(30));
        assert_eq!(bundles.len(), 1, "{bundles:?}");
        assert_eq!(bundles[0].category, Some(FeedbackCategory::Crash));
        let entries = read_bundle(&bundles[0].path).expect("readable bundle");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        for part in [
            "feedback.json",
            "frame_times.csv",
            "hitch_report.txt",
            "system.txt",
            "settings.ron",
            "render_problems.txt",
            "log_tail.txt",
            "build_info.json",
            "host.json",
            "crash.txt",
            "minidump.dmp",
        ] {
            assert!(names.contains(&part), "missing {part} in {names:?}");
        }
        let crash = text(&entries, "crash.txt");
        println!("crash.txt:\n{crash}");
        assert!(crash.contains(crash_line), "{crash}");
        assert!(crash.contains("fault address: 0x0000000000000010"), "{crash}");
        assert!(crash.contains("minidump: minidump.dmp ("), "{crash}");
        assert!(crash.contains("last frame: "), "{crash}");
        assert!(text(&entries, "host.json").contains("m-native"));
        assert!(text(&entries, "settings.ron").contains("preset"));
        assert!(text(&entries, "frame_times.csv").contains(",16.000,"));
        assert!(text(&entries, "log_tail.txt").contains("the last line before the crash"));
        let meta: serde_json::Value = serde_json::from_str(&text(&entries, "feedback.json")).expect("json");
        assert_eq!(meta["category"], "crash");
        assert_eq!(meta["crash"]["native"], true);
        assert_eq!(meta["crash"]["signal"], signal);
        assert!(meta["frames"].as_u64().is_some_and(|n| n >= 20), "{meta}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".dmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temporary .dmp is removed: {leftovers:?}"
        );

        // The reader parses the minidump and walks the crashing thread.
        let dump = xtask::minidump::load_dump_bytes(&bundles[0].path).expect("minidump in bundle");
        let report = xtask::minidump::analyze(dump, None).expect("valid minidump");
        println!("{}", report.text());
        assert!(report.reason.contains(reason), "{}", report.reason);
        assert_eq!(report.address, 0x10);
        assert!(!report.frames.is_empty());
        assert!(report.threads >= 1);
        let exe_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        assert_eq!(
            report.frames[0].module, exe_name,
            "the fault is in the test binary"
        );
        // `addr2line` resolves the DWARF of a Linux build only. A Windows dump keeps module+offset
        // frames (MSVC symbols live in a PDB, which addr2line cannot read; `xtask minidump` says
        // so), even where Git-Bash puts an `addr2line` on PATH (ADR-5874).
        #[cfg(target_os = "linux")]
        if Command::new("addr2line").arg("--version").output().is_ok() {
            assert!(
                report
                    .frames
                    .iter()
                    .any(|f| f.symbol.as_deref().is_some_and(|s| s.contains("segv"))),
                "the unstripped test binary resolves the crashing function:\n{}",
                report.text()
            );
        }

        // The next launch announces it like a panic bundle.
        assert_eq!(unannounced_crashes(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn panic_then_abort_writes_one_panic_bundle() {
        let dir = fresh_dir("abort");
        let status = run_child("panic-abort", &dir);
        assert_aborted(status);
        // Give the monitor time to (wrongly) write a second bundle.
        std::thread::sleep(Duration::from_secs(2));
        let bundles = list_bundles(&dir);
        assert_eq!(
            bundles.len(),
            1,
            "one panic bundle, no native duplicate: {bundles:?}"
        );
        let entries = read_bundle(&bundles[0].path).expect("readable");
        let crash = text(&entries, "crash.txt");
        assert!(
            crash.starts_with("panic: native crash test: panic, then abort"),
            "{crash}"
        );
        assert!(!entries.iter().any(|e| e.name == "minidump.dmp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The monitor hangs on the dump request (test hook). The crash-path watchdog must kill the
    /// game within ~5 s; the monitor then still writes a bundle, without a minidump.
    #[cfg(target_os = "linux")]
    fn a_stuck_monitor_cannot_freeze_the_crashed_game() {
        let dir = fresh_dir("stuck");
        let started = Instant::now();
        let status = run_child_env(
            "segv",
            &dir,
            &[(regia_framework::feedback::native::STALL_ENV, "1")],
        );
        let took = started.elapsed();
        println!("stuck monitor: the game died after {took:?} ({status:?})");
        assert_eq!(
            status.signal(),
            Some(9),
            "the watchdog's SIGKILL ends it: {status:?}"
        );
        assert!(
            took >= Duration::from_secs(5),
            "the watchdog waits its 5 s: {took:?}"
        );
        assert!(
            took < Duration::from_secs(20),
            "dead within ~5 s of the crash (plus start-up): {took:?}"
        );
        let bundles = wait_for_bundles(&dir, 1, Duration::from_secs(30));
        assert_eq!(bundles.len(), 1, "{bundles:?}");
        let entries = read_bundle(&bundles[0].path).expect("readable");
        let crash = text(&entries, "crash.txt");
        assert!(crash.contains("native crash: SIGSEGV"), "{crash}");
        assert!(
            crash.contains("minidump: not written (the crash monitor stalled"),
            "{crash}"
        );
        assert!(!entries.iter().any(|e| e.name == "minidump.dmp"));
        assert!(text(&entries, "frame_times.csv").contains(",16.000,"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `kill -SEGV` from another process (si_code SI_USER, not a fault): the bundle is requested
    /// and the game dies of SIGSEGV right after, not when the 5 s watchdog's SIGKILL lands.
    #[cfg(target_os = "linux")]
    fn a_sent_segv_kills_at_once_and_writes_a_bundle() {
        let dir = fresh_dir("kill");
        let ready = dir.with_extension("ready");
        let _ = std::fs::remove_file(&ready);
        let exe = std::env::current_exe().expect("test exe");
        let mut child = Command::new(exe)
            .env(MODE_ENV, "await-kill")
            .env(DIR_ENV, &dir)
            .env(READY_ENV, &ready)
            .spawn()
            .expect("spawn child");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("wait") {
                panic!("await-kill: the child exited before it was armed: {status:?}");
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("await-kill: the child never armed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = child.id().to_string();
        let sent = Instant::now();
        let kill = Command::new("kill")
            .args(["-SEGV", &pid])
            .status()
            .expect("run kill");
        assert!(kill.success(), "kill -SEGV {pid}: {kill:?}");
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            if sent.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("await-kill: the child survived kill -SEGV");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let took = sent.elapsed();
        println!("sent SIGSEGV: the game died after {took:?} ({status:?})");
        assert_eq!(
            status.signal(),
            Some(11),
            "the game dies of the SIGSEGV it was sent, not the watchdog's SIGKILL: {status:?}"
        );
        assert!(
            took < Duration::from_secs(2),
            "dead right after the bundle request, not at the 5 s watchdog: {took:?}"
        );
        let bundles = wait_for_bundles(&dir, 1, Duration::from_secs(30));
        assert_eq!(bundles.len(), 1, "{bundles:?}");
        let entries = read_bundle(&bundles[0].path).expect("readable bundle");
        let crash = text(&entries, "crash.txt");
        println!("crash.txt:\n{crash}");
        assert!(crash.contains("native crash: SIGSEGV"), "{crash}");
        assert!(crash.contains("sent by a process"), "{crash}");
        assert!(
            crash.contains("fault address: 0x0000000000000000"),
            "a sent signal has no fault address (not the sender's pid/uid): {crash}"
        );
        assert!(crash.contains("minidump: minidump.dmp ("), "{crash}");
        assert!(entries.iter().any(|e| e.name == "minidump.dmp"));
        assert!(text(&entries, "log_tail.txt").contains("the last line before the crash"));
        let _ = std::fs::remove_file(&ready);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn clean_exit_writes_nothing() {
        let dir = fresh_dir("clean");
        let status = run_child("clean", &dir);
        assert!(status.success(), "{status:?}");
        std::thread::sleep(Duration::from_secs(1));
        assert!(list_bundles(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
