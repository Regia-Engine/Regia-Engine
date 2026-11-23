// SPDX-License-Identifier: MPL-2.0
//! Native crash bundles: a SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT or SIGTRAP (a crash inside
//! the GPU driver included) writes the same feedback crash bundle as a panic, plus a minidump
//! (ADR-158). Linux and Windows; a no-op everywhere else. On Windows the "signal" is the
//! exception code (`EXCEPTION_ACCESS_VIOLATION`…), the handler is an unhandled-exception filter,
//! the monitor talks over an `AF_UNIX` socket in the temp directory, and the dump is written by
//! `MiniDumpWriteDump` from the monitor.
//!
//! Writing a minidump inside a crashed process is unsafe, so the game re-launches its own
//! executable as a small **monitor** process at startup (Embark's `minidumper`). The host makes
//! that possible by calling [`run_crash_monitor_if_requested`] first in `main`: in the monitor
//! process it never returns. On a crash, the signal handler (`regia_platform::native_crash`)
//! sends the monitor the signal, the frame ring and the log tail from preallocated memory (no
//! allocation, `try_lock` only), then asks for the dump and waits. The monitor writes the
//! minidump, lets the game die exactly as it would have, and writes the bundle: `crash.txt`
//! (signal, fault address, thread, last frame), `minidump.dmp`, the frames, the log tail, the
//! settings, render problems, host fields, system and build facts. The next launch announces it
//! like a panic bundle.
//!
//! Cost while running: the monitor is an idle process blocked in `poll`; the game only installs
//! signal handlers and sends the crash context to the monitor when it changes (settings, host
//! fields, render problems: the same moments the panic hook's context is refreshed). No
//! per-frame work.
//!
//! **Android** (ADR-328) has no monitor process: `regia_platform` arms an in-process handler at
//! startup in every Android app, which writes a fixed-layout record (`native_crash::record`) and
//! no minidump. On the next launch the platform collects it (a log line and a `logs://` report),
//! and this plugin turns it into the same crash bundle ([`write_record_bundle`]): `"minidump":
//! null`, `"platform": "android"`, the stack from the record, and the tail of the crashed run's
//! log file. No early call is needed there.
//!
//! Never worse than before: when the early call is missing, the monitor cannot start or connect,
//! or the host opted out ([`FeedbackConfig::native_crashes`](super::FeedbackConfig) or the
//! [`OPT_OUT_ENV`] variable), nothing is installed and crashes behave as they did. A panic under
//! `panic = "abort"` writes its panic bundle and the abort that follows writes no second one.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use regia_core::log::ring::LogRecord;
use regia_platform::native_crash::record::{self, CollectedRecord};

use super::bundle::{BundleInfo, BundleInput, FeedbackCategory, NativeCrash, list_bundles, write_bundle};
use super::crash;

/// Set in the monitor process's environment: the monitor's socket name.
pub const MONITOR_ENV: &str = "REGIA_CRASH_MONITOR";
/// Set in the monitor process's environment: the game's process id.
pub const PARENT_ENV: &str = "REGIA_CRASH_MONITOR_PARENT";
/// Test hook: a monitor started with this set hangs on the dump request until the game is gone.
#[doc(hidden)]
pub const STALL_ENV: &str = "REGIA_CRASH_MONITOR_TEST_STALL";
/// Set to `1` to turn native crash capture off for one run (the player-side opt-out; hosts use
/// [`FeedbackConfig::native_crashes`](super::FeedbackConfig)).
pub const OPT_OUT_ENV: &str = "REGIA_NO_NATIVE_CRASH";
/// Set to `1` to turn native crash capture **on** for a run on Windows, where it is opt-in until
/// it has run on real Windows (the host can use `FeedbackConfig::native_crashes_windows`).
pub const OPT_IN_ENV: &str = "REGIA_NATIVE_CRASH";
/// Seconds the crash path may take before the process is killed (default 5; Windows and Linux).
pub const WATCHDOG_ENV: &str = "REGIA_CRASH_WATCHDOG_S";
/// Windows: extra seconds the watchdog waits for the monitor to finish a minidump it is writing
/// (default 30), after which the process is killed anyway.
pub const DUMP_WAIT_ENV: &str = "REGIA_CRASH_DUMP_WAIT_S";

/// Where native crash capture stands in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCrashStatus {
    /// Not started (no `FeedbackPlugin` yet, or crash bundles are off).
    Off,
    /// The host opted out (config or [`OPT_OUT_ENV`]), or this is Windows and nobody opted in
    /// ([`OPT_IN_ENV`], `FeedbackConfig::native_crashes_windows`).
    OptedOut,
    /// `main` did not call [`run_crash_monitor_if_requested`] first, so the monitor could not be
    /// started safely.
    NoEarlyCall,
    /// This target has no native crash capture (everything but Linux and Windows for now).
    Unsupported,
    /// The monitor is starting.
    Starting,
    /// Armed: a native crash writes a bundle with a minidump.
    Armed,
    /// The monitor could not start or connect; crashes behave as before.
    Failed,
}

static STATUS: AtomicU8 = AtomicU8::new(0);
static EARLY_CALL: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

fn set_status(status: NativeCrashStatus) {
    STATUS.store(status as u8, Ordering::SeqCst);
}

/// The current status.
#[must_use]
pub fn native_crash_status() -> NativeCrashStatus {
    match STATUS.load(Ordering::SeqCst) {
        1 => NativeCrashStatus::OptedOut,
        2 => NativeCrashStatus::NoEarlyCall,
        3 => NativeCrashStatus::Unsupported,
        4 => NativeCrashStatus::Starting,
        5 => NativeCrashStatus::Armed,
        6 => NativeCrashStatus::Failed,
        _ => NativeCrashStatus::Off,
    }
}

/// Waits up to `timeout` while the monitor is starting; returns the status then. The game never
/// needs this (capture arms itself in the background); tests and smoke checks do.
#[must_use]
pub fn wait_until_armed(timeout: Duration) -> NativeCrashStatus {
    let deadline = Instant::now() + timeout;
    loop {
        let status = native_crash_status();
        if status != NativeCrashStatus::Starting || Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// **Call this first in `main`.** In the crash monitor process (the game re-launched with
/// [`MONITOR_ENV`] set) it runs the monitor and exits the process; everywhere else it returns
/// at once and allows [`FeedbackPlugin`](super::FeedbackPlugin) to start the monitor later.
pub fn run_crash_monitor_if_requested() {
    EARLY_CALL.store(true, Ordering::SeqCst);
    #[cfg(any(target_os = "linux", windows))]
    if let Ok(name) = regia_core::brand::env_var(MONITOR_ENV) {
        std::process::exit(imp::monitor_main(&name));
    }
}

/// The host or player opted out. On Android the platform's handler stays installed but writes
/// no record.
fn opt_out() {
    set_status(NativeCrashStatus::OptedOut);
    #[cfg(target_os = "android")]
    regia_platform::native_crash::set_recording(false);
}

fn env_flag(name: &str) -> bool {
    regia_core::brand::env_var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Starts the monitor and arms the handler in the background (from `FeedbackPlugin::build`).
/// Idempotent.
pub(crate) fn start(enabled: bool, windows_opt_in: bool) {
    if !enabled {
        opt_out();
        return;
    }
    // Opt-in on Windows until the capture has run on real Windows (see the Windows-parity ADR in docs/decisions.md).
    if cfg!(windows) && !windows_opt_in && !env_flag(OPT_IN_ENV) {
        opt_out();
        return;
    }
    if env_flag(OPT_OUT_ENV) {
        opt_out();
        return;
    }
    // Android starts no monitor process, so it needs no early call.
    if !EARLY_CALL.load(Ordering::SeqCst) && !cfg!(target_os = "android") {
        set_status(NativeCrashStatus::NoEarlyCall);
        tracing::info!(
            target: "regia_framework::feedback",
            "native crash bundles are off: call feedback::run_crash_monitor_if_requested() first in main"
        );
        return;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(any(target_os = "linux", windows))]
    imp::start();
    #[cfg(target_os = "android")]
    android_start();
    #[cfg(not(any(target_os = "linux", windows, target_os = "android")))]
    set_status(NativeCrashStatus::Unsupported);
}

/// Android: the last run's record becomes a crash bundle, and the status follows the platform's
/// handler (armed at startup by `regia_platform`).
#[cfg(target_os = "android")]
fn android_start() {
    use regia_platform::native_crash;
    native_crash::set_recording(true);
    if let Some(collected) = native_crash::take_collected() {
        match write_record_bundle(&collected) {
            Some(bundle) => tracing::warn!(
                target: "regia_framework::feedback",
                "native crash bundle from the last run's record: {}",
                bundle.path.display()
            ),
            None => tracing::info!(
                target: "regia_framework::feedback",
                "no bundle for the last run's native crash record (crash bundles off, no folder, or \
                 the abort after a panic bundle); the record is {}",
                collected.path.display()
            ),
        }
    }
    set_status(if native_crash::is_attached() {
        NativeCrashStatus::Armed
    } else {
        NativeCrashStatus::Failed
    });
}

/// The crash bundle input of an in-process native crash record (Android, ADR-328): the same
/// shape as a desktop native crash bundle, with no minidump, `platform` set, the stack from the
/// record and the build identity the crashed run had. `log_tail` is the crashed run's log.
#[must_use]
pub fn record_bundle_input(
    collected: &CollectedRecord,
    platform: &str,
    log_tail: Vec<LogRecord>,
) -> BundleInput {
    let r = &collected.record;
    let mut input = BundleInput::new(FeedbackCategory::Crash, "");
    if r.unix_ms != 0 {
        input.unix_ms = r.unix_ms;
    }
    input.screenshot_missing =
        Some("crash bundles carry no screenshot (the process died on a native crash)".into());
    input.build = r.build();
    // Unknown: the record holds no error count.
    input.gpu_errors = None;
    input.log_tail = log_tail;
    let record_name = collected
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    input.host = vec![("native_record".to_owned(), record_name)];
    input.native_crash = Some(NativeCrash {
        signal: r.signal,
        code: r.code,
        address: r.address,
        pid: r.pid,
        tid: r.tid,
        thread: r.thread_name(),
        minidump: None,
        minidump_error: Some(format!(
            "no minidump on {platform}: the crash was recorded in-process (ADR-328)"
        )),
        platform: platform.to_owned(),
        stack: record::stack_lines(r, &collected.maps),
    });
    input
}

/// The last `lines` lines of the crashed run's log file in `logs_dir`: the newest
/// `<YYYY-MM-DD_HH-MM-SS>.log` that started at or before `crash_ms`. Lines buffered in memory
/// when the process died are not in it. Empty when there is none.
#[must_use]
pub fn previous_log_tail(logs_dir: &Path, crash_ms: u64, lines: usize) -> Vec<LogRecord> {
    let stamp = record::stamp(crash_ms);
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let Some(name) = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".log") && n.get(..stamp.len()).is_some_and(|s| *s <= *stamp))
        .max()
    else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(logs_dir.join(&name)) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    let skip = all.len().saturating_sub(lines);
    all[skip..]
        .iter()
        .enumerate()
        .map(|(seq, line)| LogRecord {
            seq: seq as u64,
            unix_ms: crash_ms,
            level: String::new(),
            target: name.clone(),
            message: (*line).to_owned(),
        })
        .collect()
}

/// Writes the crash bundle of an in-process native crash record into the crash bundle folder
/// (the feedback folder), with the tail of the crashed run's log (`logs://`). `None` when crash
/// bundles are off, no folder is configured, the record is the `SIGABRT` that follows a panic
/// bundle (within 5 s), or the write failed.
pub fn write_record_bundle(collected: &CollectedRecord) -> Option<BundleInfo> {
    let ctx = crash::with_context(|c| c.clone());
    let dir = ctx.dir.clone().filter(|_| ctx.enabled)?;
    let crash_bundles: Vec<u64> = list_bundles(&dir)
        .into_iter()
        .filter(|b| b.category == Some(FeedbackCategory::Crash))
        .map(|b| b.modified_unix_ms)
        .collect();
    if record::follows_panic(&collected.record, &crash_bundles) {
        return None;
    }
    let log_tail = regia_platform::crash::crash_dir()
        .map(|logs| previous_log_tail(&logs, collected.record.unix_ms, ctx.log_lines))
        .unwrap_or_default();
    let input = record_bundle_input(collected, std::env::consts::OS, log_tail);
    write_bundle(&dir, &input, ctx.keep).ok()
}

/// Sends the crash context to the monitor (after it changed). No-op until the monitor is
/// connected, and on targets without native capture.
///
/// Never blocks the caller (the frame thread) on the monitor socket: it only wakes the
/// publisher thread. When an earlier update is still waiting (the monitor is slow or stopped
/// and the socket buffer is full), the newer context supersedes it and the older one is counted
/// as dropped ([`context_updates_dropped`]).
pub(crate) fn publish_context() {
    #[cfg(any(target_os = "linux", windows))]
    imp::publish_context();
}

/// Crash-context updates that were superseded before the monitor took them (the monitor was
/// slow or stuck). Always 0 where native capture is unsupported.
#[cfg(any(target_os = "linux", windows))]
#[must_use]
pub fn context_updates_dropped() -> u64 {
    imp::QUEUE.dropped()
}

/// Crash-context updates that were superseded before the monitor took them (the monitor was
/// slow or stuck). Always 0 where native capture is unsupported.
#[cfg(not(any(target_os = "linux", windows)))]
#[must_use]
pub fn context_updates_dropped() -> u64 {
    0
}

#[cfg(any(target_os = "linux", windows))]
mod imp {
    use std::fs::File;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use minidumper::{LoopAction, MinidumpBinary, ServerHandler};
    use regia_core::log::ring::LogRecord;
    use regia_platform::native_crash::CrashContext;

    use platform::{describe, is_abort, parent_gone, socket, socket_name, thread_name};
    use serde_json::{Value, json};

    use super::{MONITOR_ENV, NativeCrashStatus, PARENT_ENV, set_status};
    use crate::feedback::bundle::{
        BundleInput, FeedbackCategory, FrameSample, NativeCrash, unix_ms_now, write_bundle,
    };
    use crate::feedback::crash;

    /// Largest message payload: small enough that one `sendmsg` on a Unix stream socket arrives
    /// in one piece (the monitor reads each message with a single `recv`).
    const CHUNK: usize = 8 * 1024;
    const KIND_CTX_BEGIN: u32 = 1;
    const KIND_CTX_PART: u32 = 2;
    const KIND_CTX_END: u32 = 3;
    const KIND_INFO: u32 = 4;
    const KIND_FRAMES: u32 = 5;
    const KIND_LOG: u32 = 6;
    const SAMPLE_BYTES: usize = 40;
    const INFO_BYTES: usize = 24;
    /// Log messages longer than this are cut in the crash copy.
    const LOG_MESSAGE_MAX: usize = 1024;
    /// The crashed game is dead this many seconds after the signal, whatever the monitor does.
    const CRASH_WATCHDOG_S: u32 = 5;
    /// Windows: how long the watchdog may wait on a minidump the monitor is still writing.
    #[cfg(windows)]
    const DUMP_WAIT_S: u32 = 30;
    /// `STATUS_STACK_OVERFLOW`: the crash path runs on a nearly exhausted stack.
    const STACK_OVERFLOW: u32 = 0xC000_00FD;
    /// A SIGABRT this soon after a panic bundle is the `panic = "abort"` abort: no second bundle.
    const PANIC_ABORT_WINDOW_MS: u64 = 5_000;

    static CLIENT: OnceLock<minidumper::Client> = OnceLock::new();
    /// Preallocated message buffer for the signal handler.
    static SCRATCH: OnceLock<Mutex<Box<[u8]>>> = OnceLock::new();
    /// Room for one corrected crash context (capacity 1, reserved at startup: pushing never
    /// allocates). Linux only: the Windows exception carries the address in its own record.
    #[cfg(target_os = "linux")]
    static FIXED: OnceLock<Mutex<Vec<CrashContext>>> = OnceLock::new();
    static IN_CRASH: AtomicBool = AtomicBool::new(false);
    /// True while the publisher thread is inside a context send. The crash handler waits briefly
    /// for it to end: on Windows the socket is a byte stream, and two writers could interleave.
    static PUBLISHING: AtomicBool = AtomicBool::new(false);
    /// How long the crash handler waits for an in-flight publish to end.
    const PUBLISH_GRACE: Duration = Duration::from_millis(200);
    static LOG_LINES: AtomicUsize = AtomicUsize::new(400);
    /// Context updates for the publisher thread (the connect thread, once armed).
    pub(super) static QUEUE: PublishQueue = PublishQueue::new();

    /// Coalesces crash-context publish requests: the frame thread only flips a flag, the
    /// publisher thread does the blocking socket sends. Both hold the mutex only to flip the
    /// flag, never across a send, so a stuck monitor blocks the publisher thread alone.
    pub(super) struct PublishQueue {
        pending: Mutex<bool>,
        wake: Condvar,
        dropped: AtomicU64,
    }

    impl PublishQueue {
        pub(super) const fn new() -> Self {
            Self {
                pending: Mutex::new(false),
                wake: Condvar::new(),
                dropped: AtomicU64::new(0),
            }
        }

        /// Asks for a publish. Returns `false`, and counts a drop, when an earlier request is
        /// still waiting: the publisher is busy or stuck, and it will send the newest context
        /// when it gets to it.
        pub(super) fn request(&self) -> bool {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if *pending {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            *pending = true;
            self.wake.notify_one();
            true
        }

        /// Waits for a request and takes it (publisher thread).
        pub(super) fn take(&self) {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            while !*pending {
                pending = self.wake.wait(pending).unwrap_or_else(PoisonError::into_inner);
            }
            *pending = false;
        }

        pub(super) fn dropped(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    pub(super) fn start() {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => return fail(&format!("no executable path: {e}")),
        };
        let pid = std::process::id();
        let name = socket_name(pid);
        let child = Command::new(exe)
            .env(MONITOR_ENV, &name)
            .env(PARENT_ENV, pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(e) => return fail(&format!("could not start the crash monitor: {e}")),
        };
        SCRATCH.get_or_init(|| Mutex::new(vec![0u8; CHUNK].into_boxed_slice()));
        #[cfg(target_os = "linux")]
        FIXED.get_or_init(|| Mutex::new(Vec::with_capacity(1)));
        set_status(NativeCrashStatus::Starting);
        let spawned = std::thread::Builder::new()
            .name("regia-crash-monitor-connect".into())
            .spawn(move || connect(&name, child));
        if let Err(e) = spawned {
            fail(&format!("could not start the connect thread: {e}"));
        }
    }

    /// A whole number of seconds from the environment, `default` when unset or unreadable.
    fn env_secs(name: &str, default: u32) -> u32 {
        regia_core::brand::env_var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(default, |s| s.clamp(1, 600))
    }

    fn fail(why: &str) {
        set_status(NativeCrashStatus::Failed);
        tracing::warn!(target: "regia_framework::feedback", "native crash bundles are off: {why}");
    }

    fn connect(name: &str, mut child: Child) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let client = loop {
            match minidumper::Client::with_name(socket(name)) {
                Ok(client) => break client,
                Err(e) => {
                    let exited = matches!(child.try_wait(), Ok(Some(_)));
                    if exited || Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return fail(&format!("the crash monitor did not answer: {e}"));
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };
        if CLIENT.set(client).is_err() {
            return;
        }
        let Some(client) = CLIENT.get() else {
            return;
        };
        send_context(client);
        #[cfg(windows)]
        regia_platform::native_crash::set_dump_wait(env_secs(super::DUMP_WAIT_ENV, DUMP_WAIT_S));
        let watchdog_s = env_secs(super::WATCHDOG_ENV, CRASH_WATCHDOG_S);
        match regia_platform::native_crash::attach(on_crash, Some(child.id()), watchdog_s) {
            Ok(()) => {
                set_status(NativeCrashStatus::Armed);
                tracing::info!(
                    target: "regia_framework::feedback",
                    "native crash bundles armed (monitor pid {})",
                    child.id()
                );
            }
            // The monitor exits on its own when this process does.
            Err(e) => return fail(&e.to_string()),
        }
        // Dropping `child` neither kills nor waits: the monitor lives as long as the game.
        drop(child);
        // This thread becomes the publisher: it waits on a condition variable (no CPU), holds no
        // lock while idle and nothing that must outlive `main` (the process exit ends it).
        loop {
            QUEUE.take();
            if IN_CRASH.load(Ordering::SeqCst) {
                continue;
            }
            PUBLISHING.store(true, Ordering::SeqCst);
            send_context(client);
            PUBLISHING.store(false, Ordering::SeqCst);
        }
    }

    // Linux: sends from the publisher thread and from the crash handler cannot tear each other's
    // messages: `minidumper::Client::send_message` (0.11) is one `sendmsg(MSG_EOR)` of header and
    // body on a SOCK_SEQPACKET socket (`uds::UnixSeqpacketConn::send_vectored`), so every message
    // is an atomic packet. Windows: the `AF_UNIX` socket is a byte stream, so a message could
    // interleave with the handler's; `on_crash` therefore sets `IN_CRASH` (the publisher stops
    // starting sends) and waits up to `PUBLISH_GRACE` for a send in flight. A context interrupted
    // between BEGIN and END by a crash is harmless: the monitor keeps the last complete one (it
    // replaces it only on END).

    /// Asks the publisher thread to send the current crash context. Never blocks on the socket.
    pub(super) fn publish_context() {
        if CLIENT.get().is_none() {
            return;
        }
        QUEUE.request();
    }

    /// Sends the current crash context (settings, host fields, problems, GPU, folder). Blocking;
    /// runs only on the connect/publisher thread.
    fn send_context(client: &minidumper::Client) {
        let ctx = crash::with_context(|c| c.clone());
        LOG_LINES.store(ctx.log_lines, Ordering::Relaxed);
        let text = json!({
            "dir": ctx.dir,
            "enabled": ctx.enabled,
            "settings_ron": ctx.settings_ron,
            "render_problems": ctx.render_problems,
            "host": ctx.host,
            "window_s": ctx.window_s,
            "log_lines": ctx.log_lines,
            "keep": ctx.keep,
            "gpu": regia_platform::crash::gpu_info(),
        })
        .to_string();
        if client.send_message(KIND_CTX_BEGIN, []).is_err() {
            return;
        }
        for part in text.as_bytes().chunks(CHUNK) {
            if client.send_message(KIND_CTX_PART, part).is_err() {
                return;
            }
        }
        let _ = client.send_message(KIND_CTX_END, []);
    }

    /// Fills a preallocated buffer and sends it in [`CHUNK`]-sized messages. No allocation.
    struct Chunker<'a> {
        client: &'a minidumper::Client,
        kind: u32,
        buf: &'a mut [u8],
        len: usize,
    }

    impl Chunker<'_> {
        fn reserve(&mut self, n: usize) {
            if self.len + n > self.buf.len() {
                self.flush();
            }
        }

        fn put(&mut self, bytes: &[u8]) {
            let n = bytes.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
            self.len += n;
        }

        fn flush(&mut self) {
            if self.len > 0 {
                let _ = self.client.send_message(self.kind, &self.buf[..self.len]);
                self.len = 0;
            }
        }
    }

    fn encode_sample(s: &FrameSample) -> [u8; SAMPLE_BYTES] {
        let mut b = [0u8; SAMPLE_BYTES];
        b[0..8].copy_from_slice(&s.t_s.to_le_bytes());
        b[8..12].copy_from_slice(&s.frame_ms.to_le_bytes());
        b[12..16].copy_from_slice(&s.cpu_ms.to_le_bytes());
        b[16..20].copy_from_slice(&s.gpu_ms.to_le_bytes());
        b[20..24].copy_from_slice(&s.pipelines.to_le_bytes());
        b[24..28].copy_from_slice(&s.bind_groups.to_le_bytes());
        b[28..32].copy_from_slice(&s.buffers.to_le_bytes());
        b[32..36].copy_from_slice(&s.textures.to_le_bytes());
        b[36..38].copy_from_slice(&s.blocking_polls.to_le_bytes());
        b[38..40].copy_from_slice(&s.inline_bakes.to_le_bytes());
        b
    }

    fn decode_sample(b: &[u8; SAMPLE_BYTES]) -> FrameSample {
        let f32_at = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut t = [0u8; 8];
        t.copy_from_slice(&b[0..8]);
        FrameSample {
            timing: None, // The fixed native-crash wire record remains unchanged.
            t_s: f64::from_le_bytes(t),
            frame_ms: f32_at(8),
            cpu_ms: f32_at(12),
            gpu_ms: f32_at(16),
            pipelines: u32_at(20),
            bind_groups: u32_at(24),
            buffers: u32_at(28),
            textures: u32_at(32),
            blocking_polls: u16::from_le_bytes([b[36], b[37]]),
            inline_bakes: u16::from_le_bytes([b[38], b[39]]),
        }
    }

    /// The signal handler. Runs on the crashing thread in a compromised process: no allocation,
    /// `try_lock` only, no panics (slices are bounds-checked against fixed sizes).
    fn on_crash(cc: &CrashContext) {
        if IN_CRASH.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(client) = CLIENT.get() else {
            return;
        };
        let (signal, code, address, pid, tid) = describe(cc);
        // Let a publish in flight finish (bounded, no allocation, no lock).
        let grace_end = Instant::now() + PUBLISH_GRACE;
        while PUBLISHING.load(Ordering::SeqCst) && Instant::now() < grace_end {
            std::hint::spin_loop();
        }
        if is_abort(signal) {
            let last = crash::LAST_CRASH_MS.load(Ordering::Relaxed);
            if last != 0 && unix_ms_now().saturating_sub(last) < PANIC_ABORT_WINDOW_MS {
                return;
            }
        }
        let mut info = [0u8; INFO_BYTES];
        info[0..4].copy_from_slice(&signal.to_le_bytes());
        info[4..8].copy_from_slice(&code.to_le_bytes());
        info[8..16].copy_from_slice(&address.to_le_bytes());
        info[16..20].copy_from_slice(&pid.to_le_bytes());
        info[20..24].copy_from_slice(&tid.to_le_bytes());
        let _ = client.send_message(KIND_INFO, info);
        // A stack overflow leaves almost no stack: send only the info packet and ask for the dump.
        let minimal = signal == STACK_OVERFLOW;
        if minimal {
            platform::request_dump(client, cc, address);
            return;
        }
        if let Some(mut scratch) = SCRATCH.get().and_then(|m| m.try_lock().ok()) {
            if let Some(ring) = crash::FRAMES.get().and_then(|f| f.try_lock().ok()) {
                let mut out = Chunker {
                    client,
                    kind: KIND_FRAMES,
                    buf: &mut scratch[..],
                    len: 0,
                };
                ring.for_each(|s| {
                    out.reserve(SAMPLE_BYTES);
                    out.put(&encode_sample(s));
                });
                out.flush();
            }
            let mut out = Chunker {
                client,
                kind: KIND_LOG,
                buf: &mut scratch[..],
                len: 0,
            };
            regia_core::log::ring::try_for_each_recent(LOG_LINES.load(Ordering::Relaxed), |r| {
                let message = &r.message.as_bytes()[..r.message.len().min(LOG_MESSAGE_MAX)];
                out.reserve(8 + r.level.len() + r.target.len().min(256) + message.len() + 3);
                out.put(&r.unix_ms.to_le_bytes());
                out.put(r.level.as_bytes());
                out.put(&[0x1f]);
                out.put(&r.target.as_bytes()[..r.target.len().min(256)]);
                out.put(&[0x1f]);
                out.put(message);
                out.put(&[0x1e]);
            });
            out.flush();
        }
        // Blocks until the monitor has written the minidump.
        platform::request_dump(client, cc, address);
    }

    /// The Linux pieces: abstract socket, `/proc`, `siginfo`.
    #[cfg(target_os = "linux")]
    mod platform {
        use minidumper::SocketName;
        use regia_platform::native_crash::CrashContext;

        use crate::feedback::bundle::unix_ms_now;

        /// An abstract socket name unique to this game process.
        pub(super) fn socket_name(pid: u32) -> String {
            format!("regia-crash-{pid}-{}", unix_ms_now())
        }

        pub(super) fn socket(name: &str) -> SocketName<'_> {
            SocketName::Abstract(name)
        }

        /// Abstract sockets vanish with their owner.
        pub(super) fn cleanup_socket(_name: &str) {}

        /// Whether process `pid` no longer exists.
        pub(super) fn parent_gone(pid: u32) -> bool {
            !std::path::Path::new(&format!("/proc/{pid}")).exists()
        }

        /// The crashing thread's name (`comm`).
        pub(super) fn thread_name(pid: u32, tid: u32) -> Option<String> {
            let name = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm")).ok()?;
            Some(name.trim().to_owned())
        }

        /// SIGABRT: the `panic = "abort"` abort that follows a panic bundle.
        pub(super) fn is_abort(signal: u32) -> bool {
            signal == 6
        }

        /// `(signal, si_code, fault address, pid, tid)`.
        pub(super) fn describe(cc: &CrashContext) -> (u32, i32, u64, u32, u32) {
            (
                cc.siginfo.ssi_signo,
                cc.siginfo.ssi_code,
                fault_address(cc),
                cc.pid.cast_unsigned(),
                cc.tid.cast_unsigned(),
            )
        }

        /// Asks the monitor for the dump. Its exception record takes the fault address from
        /// `ssi_addr`, so hand it a corrected copy when one fits (no allocation: the slot's
        /// capacity was reserved at startup).
        pub(super) fn request_dump(client: &minidumper::Client, cc: &CrashContext, address: u64) {
            match super::FIXED.get().and_then(|m| m.try_lock().ok()) {
                Some(mut slot) if slot.capacity() > 0 => {
                    slot.clear();
                    slot.push(cc.clone());
                    if let Some(fixed) = slot.first_mut() {
                        fixed.siginfo.ssi_addr = address;
                        let _ = client.request_dump(fixed);
                    }
                }
                _ => {
                    let _ = client.request_dump(cc);
                }
            }
        }

        /// The faulting address. `crash-handler` copies the kernel's `siginfo_t` over a
        /// `signalfd_siginfo` byte for byte, so `ssi_addr` is not `si_addr`: on 64-bit Linux
        /// `si_addr` sits at offset 16 (`ssi_uid` and `ssi_fd`), on 32-bit at offset 12 (`ssi_pid`).
        /// Only fault signals carry an address; a signal sent by a process (`si_code <= 0`) carries
        /// the sender's pid and uid there instead, so it reads 0.
        fn fault_address(cc: &CrashContext) -> u64 {
            let si = &cc.siginfo;
            if !matches!(si.ssi_signo, 4 | 5 | 7 | 8 | 11) || si.ssi_code <= 0 {
                return 0;
            }
            if si.ssi_addr != 0 {
                return si.ssi_addr;
            }
            #[cfg(target_pointer_width = "64")]
            {
                u64::from(si.ssi_uid) | (u64::from(si.ssi_fd.cast_unsigned()) << 32)
            }
            #[cfg(not(target_pointer_width = "64"))]
            {
                u64::from(si.ssi_pid)
            }
        }
    }

    /// The Windows pieces: a socket file in the temp directory, process and thread queries.
    #[cfg(windows)]
    mod platform {
        use minidumper::SocketName;
        use regia_platform::native_crash::{CrashContext, crash_info, dump_in_progress, process_alive};

        use crate::feedback::bundle::unix_ms_now;

        /// `STATUS_STACK_BUFFER_OVERRUN`: what `abort()` and a `panic = "abort"` build raise (fast fail).
        const FAST_FAIL: u32 = 0xC000_0409;

        /// A socket file path in the temp directory, unique to this game process.
        pub(super) fn socket_name(pid: u32) -> String {
            std::env::temp_dir()
                .join(format!("regia-crash-{pid}-{}.sock", unix_ms_now()))
                .to_string_lossy()
                .into_owned()
        }

        pub(super) fn socket(name: &str) -> SocketName<'_> {
            SocketName::Path(std::path::Path::new(name))
        }

        /// The socket is a file: remove it once the monitor is done.
        pub(super) fn cleanup_socket(name: &str) {
            let _ = std::fs::remove_file(name);
        }

        /// Whether process `pid` no longer runs.
        pub(super) fn parent_gone(pid: u32) -> bool {
            !process_alive(pid)
        }

        /// The crashing thread's description (the name Rust gave it), if readable.
        pub(super) fn thread_name(_pid: u32, tid: u32) -> Option<String> {
            regia_platform::native_crash::thread_name(tid)
        }

        /// A fast fail soon after a panic bundle is the `panic = "abort"` abort.
        pub(super) fn is_abort(signal: u32) -> bool {
            signal == FAST_FAIL
        }

        /// `(exception code, access kind, fault address, pid, tid)`.
        pub(super) fn describe(cc: &CrashContext) -> (u32, i32, u64, u32, u32) {
            // Called from the crash callback with the filter's own context, so the exception
            // record (code, access kind, fault address) is read; see `crash_info`.
            let i = crash_info(cc);
            (i.code, i.access.cast_signed(), i.address, i.pid, i.tid)
        }

        /// Asks the monitor for the dump (`MiniDumpWriteDump` reads the exception pointers out of
        /// this process). The watchdog waits (bounded) while the dump is being written.
        pub(super) fn request_dump(client: &minidumper::Client, cc: &CrashContext, _address: u64) {
            dump_in_progress(true);
            let _ = client.request_dump(cc);
            dump_in_progress(false);
        }
    }

    // ---- the monitor process ----

    #[derive(Default)]
    struct Monitor {
        parent: Option<u32>,
        ctx_buf: Mutex<Vec<u8>>,
        ctx: Mutex<Option<Value>>,
        info: Mutex<Option<[u8; INFO_BYTES]>>,
        frames: Mutex<Vec<u8>>,
        log: Mutex<Vec<u8>>,
        dump: Mutex<Option<Result<PathBuf, String>>>,
        thread: Mutex<String>,
        connected: AtomicBool,
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    impl Monitor {
        fn dir(&self) -> Option<PathBuf> {
            lock(&self.ctx)
                .as_ref()
                .and_then(|c| c["dir"].as_str().map(PathBuf::from))
        }
    }

    struct Handler(Arc<Monitor>);

    impl ServerHandler for Handler {
        fn create_minidump_file(&self) -> Result<(File, PathBuf), std::io::Error> {
            if regia_core::brand::env_var_os(super::STALL_ENV).is_some() {
                // Test hook: a monitor that hangs while the game waits for its dump. The game's
                // watchdog must kill it; then carry on and write the bundle without a minidump.
                while self.0.parent.is_some_and(|p| !parent_gone(p)) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                *lock(&self.0.dump) = Some(Err("the crash monitor stalled (test hook)".to_owned()));
                return Err(std::io::Error::other("stalled"));
            }
            let dir = self.0.dir().unwrap_or_else(std::env::temp_dir);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!(".native-crash-{}.dmp", std::process::id()));
            Ok((File::create(&path)?, path))
        }

        fn on_minidump_created(&self, result: Result<MinidumpBinary, minidumper::Error>) -> LoopAction {
            // The game is still stopped here: read the crashing thread's name now.
            if let Some(info) = *lock(&self.0.info) {
                let pid = u32::from_le_bytes([info[16], info[17], info[18], info[19]]);
                let tid = u32::from_le_bytes([info[20], info[21], info[22], info[23]]);
                if let Some(name) = thread_name(pid, tid) {
                    *lock(&self.0.thread) = name;
                }
            }
            *lock(&self.0.dump) = Some(result.map(|b| b.path).map_err(|e| e.to_string()));
            LoopAction::Exit
        }

        fn on_message(&self, kind: u32, buffer: Vec<u8>) {
            match kind {
                KIND_CTX_BEGIN => lock(&self.0.ctx_buf).clear(),
                KIND_CTX_PART => lock(&self.0.ctx_buf).extend_from_slice(&buffer),
                KIND_CTX_END => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&lock(&self.0.ctx_buf)) {
                        *lock(&self.0.ctx) = Some(value);
                    }
                }
                KIND_INFO if buffer.len() == INFO_BYTES => {
                    let mut info = [0u8; INFO_BYTES];
                    info.copy_from_slice(&buffer);
                    *lock(&self.0.info) = Some(info);
                }
                KIND_FRAMES => lock(&self.0.frames).extend_from_slice(&buffer),
                KIND_LOG => lock(&self.0.log).extend_from_slice(&buffer),
                _ => {}
            }
        }

        fn on_client_connected(&self, _num_clients: usize) -> LoopAction {
            self.0.connected.store(true, Ordering::SeqCst);
            LoopAction::Continue
        }

        fn on_client_disconnected(&self, _num_clients: usize) -> LoopAction {
            // The game exited (normally, or it died before the dump request arrived).
            LoopAction::Exit
        }
    }

    /// The monitor process's `main`. Returns the exit code.
    pub(super) fn monitor_main(name: &str) -> i32 {
        let parent: Option<u32> = regia_core::brand::env_var(PARENT_ENV)
            .ok()
            .and_then(|p| p.parse().ok());
        let Ok(mut server) = minidumper::Server::with_name(socket(name)) else {
            return 2;
        };
        let monitor = Arc::new(Monitor {
            parent,
            ..Monitor::default()
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        {
            // Give up if the game never connects (it died, or it could not connect in time).
            let monitor = Arc::clone(&monitor);
            let shutdown = Arc::clone(&shutdown);
            let started = Instant::now();
            let _ = std::thread::Builder::new()
                .name("regia-crash-monitor-watchdog".into())
                .spawn(move || {
                    while !monitor.connected.load(Ordering::SeqCst) {
                        if parent.is_some_and(parent_gone) || started.elapsed() > Duration::from_secs(20) {
                            shutdown.store(true, Ordering::SeqCst);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                });
        }
        let _ = server.run(Box::new(Handler(Arc::clone(&monitor))), &shutdown, None);
        write_native_bundle(&monitor);
        platform::cleanup_socket(name);
        0
    }

    fn decode_log(bytes: &[u8]) -> Vec<LogRecord> {
        let mut out = Vec::new();
        for (seq, record) in bytes.split(|b| *b == 0x1e).filter(|r| r.len() >= 8).enumerate() {
            let mut ms = [0u8; 8];
            ms.copy_from_slice(&record[..8]);
            let mut fields = record[8..].splitn(3, |b| *b == 0x1f);
            let mut next = || String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
            let (level, target, message) = (next(), next(), next());
            out.push(LogRecord {
                seq: seq as u64,
                unix_ms: u64::from_le_bytes(ms),
                level,
                target,
                message,
            });
        }
        out
    }

    fn strings(v: &Value) -> Vec<(String, String)> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((p.get(0)?.as_str()?.to_owned(), p.get(1)?.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// After the game died: the bundle with the minidump inside. Keeps the bare `.dmp` beside the
    /// bundles if the bundle cannot be written.
    fn write_native_bundle(monitor: &Monitor) {
        let Some(info) = *lock(&monitor.info) else {
            return; // a normal exit
        };
        let ctx = lock(&monitor.ctx).clone().unwrap_or(Value::Null);
        let dump = lock(&monitor.dump).take();
        let Some(dir) = monitor.dir() else {
            return;
        };
        let le32 = |i: usize| [info[i], info[i + 1], info[i + 2], info[i + 3]];
        let mut addr = [0u8; 8];
        addr.copy_from_slice(&info[8..16]);
        let (minidump, minidump_error, dump_path) = match dump {
            Some(Ok(path)) => match std::fs::read(&path) {
                Ok(bytes) if !bytes.is_empty() => (Some(bytes), None, Some(path)),
                Ok(_) => (None, Some("the minidump is empty".to_owned()), Some(path)),
                Err(e) => (
                    None,
                    Some(format!("could not read the minidump: {e}")),
                    Some(path),
                ),
            },
            Some(Err(e)) => (None, Some(e), None),
            None => (
                None,
                Some("the game died before the dump was requested".to_owned()),
                None,
            ),
        };
        let window_s = ctx["window_s"].as_f64().unwrap_or(30.0);
        let frames: Vec<FrameSample> = lock(&monitor.frames)
            .as_chunks::<SAMPLE_BYTES>()
            .0
            .iter()
            .map(decode_sample)
            .collect();
        let newest = frames.last().map_or(0.0, |f| f.t_s);
        let mut input = BundleInput::new(FeedbackCategory::Crash, "");
        input.screenshot_missing =
            Some("crash bundles carry no screenshot (the process died on a native crash)".into());
        input.window_s = window_s;
        input.frames = frames
            .into_iter()
            .filter(|f| newest - f.t_s <= window_s)
            .collect();
        input.settings_ron = ctx["settings_ron"].as_str().map(str::to_owned);
        input.render_problems = (!ctx["render_problems"].is_null()).then(|| strings(&ctx["render_problems"]));
        input.host = strings(&ctx["host"]);
        // The monitor is another process: the game's error count is not known here.
        input.gpu_errors = None;
        if let Some(gpu) = ctx["gpu"].as_str() {
            input.system.gpu = gpu.to_owned();
        }
        input.log_tail = decode_log(&lock(&monitor.log));
        input.native_crash = Some(NativeCrash {
            signal: u32::from_le_bytes(le32(0)),
            code: i32::from_le_bytes(le32(4)),
            address: u64::from_le_bytes(addr),
            pid: u32::from_le_bytes(le32(16)),
            tid: u32::from_le_bytes(le32(20)),
            thread: lock(&monitor.thread).clone(),
            minidump: minidump.clone(),
            minidump_error,
            platform: std::env::consts::OS.to_owned(),
            stack: Vec::new(),
        });
        let keep = ctx["keep"].as_u64().map_or(50, |k| k as usize);
        let written = write_bundle(&dir, &input, keep).is_ok();
        if let Some(path) = dump_path {
            if written || minidump.is_none() {
                let _ = std::fs::remove_file(&path);
            } else {
                let _ = std::fs::rename(&path, dir.join(format!("native-crash-{}.dmp", input.unix_ms)));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A publisher stuck in a socket send never blocks the requester: later requests return
        /// at once, coalesce into one pending publish and are counted as dropped.
        #[test]
        fn publish_requests_never_wait_for_a_stuck_publisher() {
            let queue = Arc::new(PublishQueue::new());
            let (stuck_tx, stuck_rx) = std::sync::mpsc::channel::<()>();
            let (took_tx, took_rx) = std::sync::mpsc::channel::<()>();
            let publisher = {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    queue.take();
                    let _ = took_tx.send(());
                    // The "send" to a stopped monitor: blocks until the test lets go.
                    let _ = stuck_rx.recv();
                })
            };
            assert!(queue.request(), "first request wakes the publisher");
            took_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("publisher took the request");
            let start = Instant::now();
            assert!(queue.request(), "one update waits for the publisher");
            for _ in 0..1000 {
                assert!(!queue.request(), "the rest coalesce");
            }
            assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
            assert_eq!(queue.dropped(), 1000);
            let _ = stuck_tx.send(());
            publisher.join().expect("publisher");
            // The pending update is still there for the next take.
            queue.take();
        }

        #[test]
        fn frame_samples_round_trip() {
            let s = FrameSample {
                timing: None,
                t_s: 12.5,
                frame_ms: 16.6,
                cpu_ms: 4.0,
                gpu_ms: 9.25,
                pipelines: 3,
                bind_groups: 7,
                buffers: 1,
                textures: 2,
                blocking_polls: 5,
                inline_bakes: 6,
            };
            assert_eq!(decode_sample(&encode_sample(&s)), s);
        }

        #[test]
        fn log_records_round_trip() {
            let mut bytes = Vec::new();
            for (ms, level, target, msg) in [(5u64, "WARN", "a::b", "hello"), (6, "INFO", "c", "x\u{1f}y")] {
                bytes.extend_from_slice(&ms.to_le_bytes());
                bytes.extend_from_slice(level.as_bytes());
                bytes.push(0x1f);
                bytes.extend_from_slice(target.as_bytes());
                bytes.push(0x1f);
                bytes.extend_from_slice(msg.as_bytes());
                bytes.push(0x1e);
            }
            let records = decode_log(&bytes);
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].level, "WARN");
            assert_eq!(records[0].target, "a::b");
            assert_eq!(records[0].message, "hello");
            assert_eq!(records[1].unix_ms, 6);
            assert_eq!(
                records[1].message, "x\u{1f}y",
                "the message keeps separators after the second"
            );
        }
    }
}

/// The Android record path (ADR-328), tested on the host: the record format and the next-launch
/// conversion are plain code on every target.
#[cfg(test)]
mod record_tests {
    use std::path::Path;

    use regia_platform::native_crash::record::{
        self, Collected, CollectedRecord, CrashRecord, HEADER_SIZE, KEEP_RECORDS, PENDING_FILE, RecordError,
        WalkEnd,
    };
    use serde_json::Value;

    use super::{previous_log_tail, record_bundle_input};
    use crate::feedback::bundle::build_bundle;
    use crate::feedback::zip::read_entries;

    fn sample(unix_ms: u64) -> CrashRecord {
        let mut r = CrashRecord::with_build(&regia_platform::crash::BuildInfo {
            version: "2.0.0".into(),
            git_hash: "0123abcd".into(),
            profile: "shipping".into(),
            engine: "0.1.28".into(),
        });
        r.unix_ms = unix_ms;
        r.signal = 11;
        r.code = 1;
        r.address = 0x10;
        r.pid = 900;
        r.tid = 901;
        r.thread = record::fixed("regia-crasher");
        r.pc = 0x7a00_1000;
        r.lr = 0x7a00_2000;
        r.frames[0] = 0x7c00_1234;
        r.frame_count = 1;
        r.walk_end = WalkEnd::EndOfChain;
        r
    }

    const MAPS: &str = "7a000000-7a010000 r-xp 00000000 fd:01 12 /data/app/x/lib/arm64/libmain.so\n\
                        7c000000-7c200000 r-xp 00040000 fd:02 99 /vendor/lib64/egl/libGLES_mali.so\n";

    /// What the handler leaves: header then maps text, padded with zeros to the file size.
    fn pending_bytes(r: &CrashRecord) -> Vec<u8> {
        let mut r = *r;
        r.maps_len = MAPS.len() as u32;
        let mut head = [0u8; HEADER_SIZE];
        record::encode(&r, &mut head);
        let mut bytes = head.to_vec();
        bytes.extend_from_slice(MAPS.as_bytes());
        bytes.resize(record::FILE_SIZE, 0);
        bytes
    }

    /// Writes `bytes` as the record folder's `pending.bin` (temp file, then rename).
    fn put_pending(dir: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(dir).expect("dir");
        let tmp = dir.join("pending.tmp");
        std::fs::write(&tmp, bytes).expect("write");
        std::fs::rename(&tmp, dir.join(PENDING_FILE)).expect("rename");
    }

    #[test]
    fn the_next_launch_bundle_has_the_desktop_keys_and_android_fields() {
        let collected = CollectedRecord {
            path: "/data/user/0/x/files/crash/2026-10-21_13-00-00.crash.bin".into(),
            record: sample(1_792_600_000_000),
            maps: MAPS.to_owned(),
        };
        let input = record_bundle_input(&collected, "android", Vec::new());
        assert_eq!(
            input.unix_ms, 1_792_600_000_000,
            "the bundle carries the crash time"
        );
        assert_eq!(
            input.build.git_hash, "0123abcd",
            "the crashed run's build, not this one's"
        );
        let (bytes, names) = build_bundle(&input);
        for n in [
            "feedback.json",
            "crash.txt",
            "host.json",
            "log_tail.txt",
            "build_info.json",
        ] {
            assert!(names.contains(&n.to_owned()), "{n} in {names:?}");
        }
        assert!(!names.contains(&"minidump.dmp".to_owned()));
        let entries = read_entries(&bytes).expect("readable");
        let get = |n: &str| {
            String::from_utf8(entries.iter().find(|e| e.name == n).expect("entry").data.clone())
                .expect("utf8")
        };
        let meta: Value = serde_json::from_str(&get("feedback.json")).expect("json");
        let crash = &meta["crash"];
        // The desktop native crash keys, all present.
        for key in [
            "native",
            "signal",
            "reason",
            "address",
            "thread",
            "tid",
            "minidump",
            "minidump_error",
        ] {
            assert!(crash.get(key).is_some(), "{key} missing: {crash}");
        }
        assert_eq!(crash["native"], true);
        assert_eq!(crash["signal"], "SIGSEGV");
        assert_eq!(crash["reason"], "SIGSEGV (address not mapped)");
        assert_eq!(crash["address"], "0x10");
        assert_eq!(crash["thread"], "regia-crasher");
        assert_eq!(crash["tid"], 901);
        assert_eq!(crash["minidump"], Value::Null);
        assert_eq!(crash["platform"], "android");
        assert_eq!(meta["category"], "crash");
        let stack = crash["stack"].as_array().expect("stack");
        assert_eq!(stack.len(), 3, "pc, lr, one frame: {stack:?}");
        assert!(
            stack[2]
                .as_str()
                .is_some_and(|l| l.contains("libGLES_mali.so+0x41234")),
            "{stack:?}"
        );
        let text = get("crash.txt");
        assert!(
            text.contains("native crash: SIGSEGV (address not mapped)"),
            "{text}"
        );
        assert!(text.contains("regia-crasher (tid 901)"), "{text}");
        assert!(text.contains("no minidump on android"), "{text}");
        assert!(!text.contains("The stack is in the minidump"), "{text}");
        assert!(get("host.json").contains("2026-10-21_13-00-00.crash.bin"));
    }

    #[test]
    fn collection_keeps_the_newest_five_records() {
        let dir = regia_test_utils::unique_temp_dir("android-crash-rotate");
        assert!(
            matches!(record::collect_pending(&dir), Ok(Collected::Nothing)),
            "no file"
        );
        let mut kept = Vec::new();
        for i in 0..7u64 {
            // A fresh armed file reads as "no crash".
            drop(record::prepare_pending(&dir).expect("prepare"));
            assert!(
                matches!(record::collect_pending(&dir), Ok(Collected::Nothing)),
                "zeros"
            );
            let r = sample(1_792_600_000_000 + i * 61_000);
            put_pending(&dir, &pending_bytes(&r));
            match record::collect_pending(&dir).expect("collect") {
                Collected::Record(c) => {
                    assert_eq!(c.record.tid, 901);
                    assert_eq!(c.maps, MAPS);
                    kept.push(c.path);
                }
                other => panic!("record {i}: {other:?}"),
            }
            assert!(!dir.join(PENDING_FILE).exists(), "moved, not copied");
        }
        let left = record::collected_records(&dir);
        assert_eq!(left.len(), KEEP_RECORDS);
        assert_eq!(left.as_slice(), &kept[2..], "the two oldest were removed");
        // A bad file is rejected with its error and removed.
        let mut bad = pending_bytes(&sample(1));
        bad[0] = b'?';
        put_pending(&dir, &bad);
        assert!(matches!(
            record::collect_pending(&dir),
            Ok(Collected::Rejected(RecordError::BadMagic))
        ));
        assert!(!dir.join(PENDING_FILE).exists());
        put_pending(&dir, &pending_bytes(&sample(1))[..40]);
        assert!(matches!(
            record::collect_pending(&dir),
            Ok(Collected::Rejected(RecordError::Truncated { len: 40, .. }))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_is_written_unless_the_record_follows_a_panic() {
        let logs = regia_test_utils::unique_temp_dir("android-crash-report");
        let collected = CollectedRecord {
            path: logs.join("x.crash.bin"),
            record: sample(1_792_600_000_000),
            maps: MAPS.to_owned(),
        };
        let path = record::write_report(&logs, &collected, "android")
            .expect("io")
            .expect("written");
        let name = path.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(regia_platform::crash::is_crash_file_name(name), "{name}");
        assert!(name.ends_with(record::REPORT_SUFFIX), "{name}");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("platform: android") && text.contains("SIGSEGV"),
            "{text}"
        );
        // An abort right after a panic report (written now) gets no second report.
        std::fs::write(logs.join("crash-2026-10-21_00-00-00.txt"), "panic").expect("panic report");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let mut abort = collected.clone();
        abort.record.signal = 6;
        abort.record.unix_ms = now;
        assert_eq!(record::write_report(&logs, &abort, "android").expect("io"), None);
        let _ = std::fs::remove_dir_all(&logs);
    }

    #[test]
    fn the_log_tail_comes_from_the_crashed_runs_file() {
        let logs = regia_test_utils::unique_temp_dir("android-crash-logs");
        let crash_ms = 1_792_600_000_000; // 2026-10-21 16:26:40 UTC
        let stamp = record::stamp(crash_ms);
        assert_eq!(stamp, "2026-10-21_16-26-40");
        std::fs::write(logs.join("2026-10-21_16-00-00.log"), "old 1\nold 2\n").expect("older run");
        std::fs::write(logs.join("2026-10-21_16-20-00.log"), "a\nb\nc\nd\n").expect("crashed run");
        std::fs::write(logs.join("2026-10-21_16-30-00.log"), "this run\n").expect("this run");
        let tail = previous_log_tail(&logs, crash_ms, 3);
        let lines: Vec<&str> = tail.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(lines, ["b", "c", "d"]);
        assert_eq!(tail[0].target, "2026-10-21_16-20-00.log");
        assert!(
            previous_log_tail(&logs, 1_000, 3).is_empty(),
            "no file before the crash"
        );
        let _ = std::fs::remove_dir_all(&logs);
    }
}
