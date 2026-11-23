// SPDX-License-Identifier: MPL-2.0
//! The Android backend (ADR-328): `crash-handler`'s signal handlers (the same crate as Linux, whose
//! `linux` module also covers Android) writing an in-process record ([`super::record`]).
//!
//! Android cannot re-launch the game as a monitor process, so the handler writes the record
//! itself into `<internal>/crash/pending.bin`, which was created, zero-filled and opened at
//! attach time. In the handler: atomics, `clock_gettime`, `prctl(PR_GET_NAME)`, `open`/`read` of
//! `/proc/self/maps`, `lseek`, `write`, `fsync` and the pipe reads of the frame walk; no
//! allocation, no locks, no panics. The next launch reads the record back
//! ([`super::record::collect_pending`]): one log line (logcat), a `logs://crash-<time>-native.txt`
//! report for `diagnostics.crash_reports`, and, with the feedback plugin, a crash bundle.
//!
//! After the record, the handler behaves as on Linux: a deliberate exit (ADR-276) disarms it,
//! a signal sent by a process (`si_code <= 0`, not `SIGABRT`) gets its default action at once,
//! and a fault restores the previous handlers and returns, so the fault repeats into them
//! (ART's chain and `debuggerd`: the tombstone is still written).

use std::os::fd::IntoRawFd as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::NativeCrashError;
use super::record::{self, Collected, CollectedRecord, CrashRecord, HEADER_SIZE, MAPS_MAX, MapsFilter};

/// The crashing thread's state, as `crash-handler` delivers it (signal info, registers, pid, tid).
pub use crash_handler::CrashContext;

static HANDLER: OnceLock<crash_handler::CrashHandler> = OnceLock::new();
/// The record with the build facts, filled at attach; the handler copies it.
static TEMPLATE: OnceLock<CrashRecord> = OnceLock::new();
/// `pending.bin`, open for writing (never closed: it lives as long as the process).
static RECORD_FD: AtomicI32 = AtomicI32::new(-1);
static RECORDING: AtomicBool = AtomicBool::new(true);
static IN_CRASH: AtomicBool = AtomicBool::new(false);
/// The last run's record, collected at startup, until the feedback plugin takes it.
static COLLECTED: Mutex<Option<CollectedRecord>> = Mutex::new(None);

/// Seconds the crash path may take before `SIGALRM` ends the process.
const WATCHDOG_S: u32 = 5;
/// The record folder under the app's internal data directory.
pub const RECORD_DIR: &str = "crash";
/// The system property that makes a run crash on purpose (`cargo xtask android-pass
/// --crash-test`): `segv` (after 5 s) or `segv:<seconds>`. Only `adb shell` (or root) can set a
/// `debug.` property; an app cannot.
pub const CRASH_TEST_PROPERTY: &str = "debug.regia.crash_test";

/// At startup (from the Android platform module, once `logs://` is configured): collects the last
/// run's record from `<internal>/crash`, then arms the handler for this run. Idempotent (a second
/// `android_main` in the same process finds it armed).
pub(crate) fn start_records(internal: &Path) {
    if is_attached() {
        return;
    }
    let dir = internal.join(RECORD_DIR);
    match record::collect_pending(&dir) {
        Ok(Collected::Record(collected)) => announce(*collected),
        Ok(Collected::Rejected(e)) => tracing::warn!(
            target: "regia_platform",
            "native crash record from the last run is unreadable and was removed: {e}"
        ),
        Ok(Collected::Nothing) => {}
        Err(e) => tracing::warn!(
            target: "regia_platform",
            "could not read the native crash record in {}: {e}",
            dir.display()
        ),
    }
    match attach(&dir, &crate::crash::BuildInfo::detect(), WATCHDOG_S) {
        Ok(()) => {
            tracing::info!(target: "regia_platform", "native crash records armed ({})", dir.display());
            start_crash_test_if_requested();
        }
        Err(e) => tracing::warn!(target: "regia_platform", "native crash records are off: {e}"),
    }
}

/// One log line (logcat and `logs://`), the text report, and the record kept for the feedback
/// plugin.
fn announce(collected: CollectedRecord) {
    let r = &collected.record;
    let thread = r.thread_name();
    let report = crate::crash::crash_dir().and_then(|logs| {
        record::write_report(&logs, &collected, "android")
            .map_err(|e| {
                tracing::warn!(target: "regia_platform", "could not write the native crash report: {e}");
            })
            .ok()
            .flatten()
    });
    tracing::warn!(
        target: "regia_platform",
        "native crash record from the last run: {} (code {}) at {:#x} on thread {} (tid {}), record {}, report {}",
        record::signal_name(r.signal),
        r.code,
        r.address,
        if thread.is_empty() { "<unknown>" } else { thread.as_str() },
        r.tid,
        collected.path.display(),
        report.map_or_else(
            || "none (no logs folder, a write error, or the abort after a panic report)".to_owned(),
            |p| p.display().to_string()
        ),
    );
    *COLLECTED.lock().unwrap_or_else(PoisonError::into_inner) = Some(collected);
}

/// The last run's record, once: the feedback plugin turns it into a crash bundle.
pub fn take_collected() -> Option<CollectedRecord> {
    COLLECTED.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Turns recording on or off (the feedback plugin's opt-out). The handler stays installed; when
/// off it writes no record and the process dies exactly as without it.
pub fn set_recording(on: bool) {
    RECORDING.store(on, Ordering::SeqCst);
}

/// Bounds the crash path: `alarm(seconds)` with `SIGALRM` at its default action (terminate).
/// There is no monitor that could hold a ptrace stop here, so the Linux clone-and-`SIGKILL`
/// watchdog is not needed. Async-signal-safe.
fn arm_crash_watchdog(seconds: u32) {
    // SAFETY: `sigaction` with a zeroed (SIG_DFL) action and `alarm`, both async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut());
        libc::alarm(seconds);
    }
}

/// Whether `signal` was sent by a process (`si_code <= 0`) and is not `SIGABRT` (see the Linux
/// backend: `SIGABRT` is left to `crash-handler`'s own re-raise).
fn is_sent_signal(signal: libc::c_int, si_code: i32) -> bool {
    si_code <= 0 && signal != libc::SIGABRT
}

/// Ends the process with `signal` at its default action, now. The default is set with the raw
/// `rt_sigaction` syscall: bionic's `sigaction` goes through ART's signal chain, which may keep
/// its own handler first. The action is all zero bytes (`SIG_DFL` is 0, no flags, empty mask), so
/// the kernel layout of `struct sigaction` does not matter. Then unblock and `tgkill`; if that
/// returns, `SIGKILL`. Async-signal-safe.
fn die_of_sent_signal(signal: libc::c_int) {
    // SAFETY: raw syscalls on stack values; the process is meant to end here.
    unsafe {
        let action = [0u64; 4];
        // The kernel's sigset size on Android's 64-bit and 32-bit ABIs (64 signals).
        let sigset_bytes: libc::c_long = 8;
        libc::syscall(
            libc::SYS_rt_sigaction,
            libc::c_long::from(signal),
            action.as_ptr(),
            std::ptr::null_mut::<libc::c_void>(),
            sigset_bytes,
        );
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        let pid = libc::getpid();
        let tid = libc::syscall(libc::SYS_gettid);
        libc::syscall(
            libc::SYS_tgkill,
            libc::c_long::from(pid),
            tid,
            libc::c_long::from(signal),
        );
        libc::kill(pid, libc::SIGKILL);
    }
}

/// A fault after a deliberate exit began (ADR-276): one fixed line on stderr, then `_exit` with
/// the deliberate exit code. Async-signal-safe.
fn end_deliberate_exit(code: i32) -> ! {
    const LINE: &[u8] = b"regia exit: native fault during a deliberate exit; ending with its exit code\n";
    // SAFETY: `write` reads the static slice; `_exit` takes no pointers and never returns.
    unsafe {
        libc::write(libc::STDERR_FILENO, LINE.as_ptr().cast(), LINE.len());
        libc::_exit(code)
    }
}

/// Writes all of `bytes` to `fd` (retrying short writes and `EINTR`). Async-signal-safe.
fn write_all(fd: libc::c_int, bytes: &[u8]) {
    let mut done = 0;
    while done < bytes.len() {
        let rest = &bytes[done..];
        // SAFETY: `rest` is a live slice; `write` reads at most `rest.len()` bytes from it.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        if n > 0 {
            done += n.cast_unsigned();
        } else if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        } else {
            return;
        }
    }
}

/// Wall-clock time in ms (`clock_gettime`, async-signal-safe).
fn realtime_ms() -> u64 {
    // SAFETY: `clock_gettime` fills the stack value.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) != 0 {
            return 0;
        }
        (ts.tv_sec.max(0) as u64) * 1000 + (ts.tv_nsec.max(0) as u64) / 1_000_000
    }
}

/// The calling thread's name (`prctl(PR_GET_NAME)`: the kernel's 16-byte `comm`).
fn current_thread_name() -> [u8; 16] {
    let mut name = [0u8; 16];
    // SAFETY: `PR_GET_NAME` writes at most 16 bytes, NUL-terminated, into `name`.
    unsafe {
        libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr());
    }
    name
}

/// The faulting address. `crash-handler` copies the kernel's `siginfo_t` over a
/// `signalfd_siginfo` byte for byte (as on Linux, see `feedback::native`), so `si_addr` is at
/// offset 16 on 64-bit targets (`ssi_uid`, `ssi_fd`) and 12 on 32-bit ones (`ssi_pid`). Only
/// fault signals raised by the kernel carry one.
fn fault_address(cc: &CrashContext) -> u64 {
    let si = &cc.siginfo;
    if !matches!(si.ssi_signo, 4 | 5 | 7 | 8 | 11) || si.ssi_code <= 0 {
        return 0;
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

/// Copies the executable lines of `/proc/self/maps` to `fd` at [`HEADER_SIZE`], at most
/// [`MAPS_MAX`] bytes. Returns the bytes written. Async-signal-safe (`open`, `read`, `lseek`,
/// `write`, `close` on stack buffers).
fn copy_exec_maps(fd: libc::c_int) -> u32 {
    // SAFETY: plain syscalls; the path is a static C string, the buffers live on this stack.
    unsafe {
        let maps = libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if maps < 0 {
            return 0;
        }
        if libc::lseek(fd, HEADER_SIZE as libc::off_t, libc::SEEK_SET) < 0 {
            libc::close(maps);
            return 0;
        }
        let mut filter = MapsFilter::new();
        let mut chunk = [0u8; 1024];
        let mut written = 0usize;
        let mut emit = |line: &[u8]| {
            if written + line.len() <= MAPS_MAX {
                write_all(fd, line);
                written += line.len();
            }
        };
        loop {
            let n = libc::read(maps, chunk.as_mut_ptr().cast(), chunk.len());
            if n <= 0 {
                break;
            }
            filter.feed(&chunk[..n.cast_unsigned()], &mut emit);
        }
        filter.finish(&mut emit);
        libc::close(maps);
        written as u32
    }
}

/// Registers and the frame-pointer walk. aarch64 (devices) and x86_64 (the emulator) keep frame
/// records in the same shape (`[fp]` = caller's frame pointer, `[fp + 8]` = return address).
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod regs {
    use super::CrashContext;
    use crate::native_crash::memory_probe::MemoryProbe;
    use crate::native_crash::record::{self, CrashRecord};

    /// How the walk reads memory that may be unmapped.
    static PROBE: MemoryProbe = MemoryProbe::new();

    /// Creates the probe's pipe (at attach). Without it the walk is skipped.
    pub(super) fn open_pipe() {
        PROBE.open();
    }

    /// PC, LR, SP and FP from the signal context, then the walk.
    pub(super) fn fill(cc: &CrashContext, rec: &mut CrashRecord) {
        #[cfg(target_arch = "aarch64")]
        {
            let m = &cc.context.uc_mcontext;
            rec.pc = m.pc;
            rec.lr = m.regs[30] & record::ADDRESS_MASK;
            rec.sp = m.sp;
            rec.fp = m.regs[29];
        }
        #[cfg(target_arch = "x86_64")]
        {
            // `gregs` order: REG_RBP 10, REG_RSP 15, REG_RIP 16.
            let g = &cc.context.uc_mcontext.gregs;
            rec.pc = g[16].cast_unsigned();
            rec.sp = g[15].cast_unsigned();
            rec.fp = g[10].cast_unsigned();
        }
        let (n, end) =
            record::walk_frame_pointers(rec.fp, rec.sp, |addr| PROBE.read_pair(addr), &mut rec.frames);
        rec.frame_count = n as u32;
        rec.walk_end = end;
    }
}

/// Other architectures record no registers and walk no frames.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
mod regs {
    use super::CrashContext;
    use crate::native_crash::record::CrashRecord;

    pub(super) fn open_pipe() {}

    pub(super) fn fill(_cc: &CrashContext, _rec: &mut CrashRecord) {}
}

/// The handler's work: fills a copy of the template and writes it, maps text first, header last.
fn write_record(cc: &CrashContext) {
    if !RECORDING.load(Ordering::SeqCst) || IN_CRASH.swap(true, Ordering::SeqCst) {
        return;
    }
    let fd = RECORD_FD.load(Ordering::SeqCst);
    let Some(template) = TEMPLATE.get() else {
        return;
    };
    if fd < 0 {
        return;
    }
    let mut rec = *template;
    rec.unix_ms = realtime_ms();
    rec.signal = cc.siginfo.ssi_signo;
    rec.code = cc.siginfo.ssi_code;
    rec.address = fault_address(cc);
    rec.pid = cc.pid.cast_unsigned();
    rec.tid = cc.tid.cast_unsigned();
    rec.thread = current_thread_name();
    regs::fill(cc, &mut rec);
    rec.maps_len = copy_exec_maps(fd);
    let mut head = [0u8; HEADER_SIZE];
    record::encode(&rec, &mut head);
    // SAFETY: `lseek` and `fsync` on the record descriptor opened at attach.
    unsafe {
        if libc::lseek(fd, 0, libc::SEEK_SET) == 0 {
            write_all(fd, &head);
        }
        libc::fsync(fd);
    }
}

/// Installs the handler and prepares `<dir>/pending.bin` (zero-filled, held open). `build` goes
/// into every record. Before the record is written, `alarm(watchdog_s)` bounds the crash path.
///
/// # Errors
/// [`NativeCrashError::AlreadyAttached`] on a second call, [`NativeCrashError::Install`] when the
/// record file cannot be created or the signal handlers cannot be installed.
pub fn attach(dir: &Path, build: &crate::crash::BuildInfo, watchdog_s: u32) -> Result<(), NativeCrashError> {
    if HANDLER.get().is_some() {
        return Err(NativeCrashError::AlreadyAttached);
    }
    let file = record::prepare_pending(dir)
        .map_err(|e| NativeCrashError::Install(format!("{}: {e}", dir.display())))?;
    let _ = TEMPLATE.set(CrashRecord::with_build(build));
    regs::open_pipe();
    RECORD_FD.store(file.into_raw_fd(), Ordering::SeqCst);
    // SAFETY: `make_crash_event` is unsafe only because the closure runs in a signal handler;
    // everything it calls is async-signal-safe (see the module docs).
    let event = unsafe {
        crash_handler::make_crash_event(move |context: &CrashContext| {
            if let Some(code) = crate::deliberate_exit::PROCESS
                .run_crash_hook(|| arm_crash_watchdog(watchdog_s), || write_record(context))
            {
                end_deliberate_exit(code);
            }
            let signal = context.siginfo.ssi_signo.cast_signed();
            if is_sent_signal(signal, context.siginfo.ssi_code) {
                die_of_sent_signal(signal);
            }
            // `false`: restore the previous handlers and return, so the fault repeats into them.
            crash_handler::CrashEventResult::Handled(false)
        })
    };
    let handler =
        crash_handler::CrashHandler::attach(event).map_err(|e| NativeCrashError::Install(e.to_string()))?;
    HANDLER
        .set(handler)
        .map_err(|_| NativeCrashError::AlreadyAttached)
}

/// Whether the handler is attached in this process.
#[must_use]
pub fn is_attached() -> bool {
    HANDLER.get().is_some()
}

/// `adb shell setprop debug.regia.crash_test segv[:<seconds>]`: after the delay (default 5 s,
/// past the first frame), a thread named `regia-crasher` reads address `0x10`, a real
/// `SIGSEGV` (`SEGV_MAPERR`) through the whole handler path. Test-only; nothing happens when the
/// property is unset.
fn start_crash_test_if_requested() {
    let mut value = [0u8; 92];
    let name = c"debug.regia.crash_test";
    // SAFETY: `__system_property_get` writes at most PROP_VALUE_MAX (92) bytes, NUL included.
    let len = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr().cast()) };
    let Ok(len) = usize::try_from(len) else {
        return;
    };
    let text = String::from_utf8_lossy(&value[..len.min(value.len())]).into_owned();
    let Some(rest) = text.trim().strip_prefix("segv") else {
        return;
    };
    let seconds = rest
        .strip_prefix(':')
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);
    tracing::warn!(
        target: "regia_platform",
        "crash test ({CRASH_TEST_PROPERTY}): SIGSEGV on thread regia-crasher in {seconds} s"
    );
    let spawned = std::thread::Builder::new()
        .name("regia-crasher".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(seconds.min(600)));
            tracing::warn!(target: "regia_platform", "crash test: reading address 0x10 now");
            // SAFETY: deliberately not safe: this test-only read faults (address 0x10 is never
            // mapped), which is the point.
            let value = unsafe { std::ptr::read_volatile(std::ptr::without_provenance::<u64>(0x10)) };
            tracing::error!(target: "regia_platform", "crash test: the read returned {value}");
        });
    if let Err(e) = spawned {
        tracing::warn!(target: "regia_platform", "crash test: could not start the thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::is_sent_signal;

    #[test]
    fn only_sent_non_abort_signals_take_the_immediate_path() {
        assert!(is_sent_signal(libc::SIGSEGV, 0));
        assert!(is_sent_signal(libc::SIGSEGV, -6));
        assert!(!is_sent_signal(libc::SIGABRT, -6));
        assert!(!is_sent_signal(libc::SIGSEGV, 1));
    }
}
