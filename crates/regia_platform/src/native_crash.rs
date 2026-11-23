// SPDX-License-Identifier: MPL-2.0
//! Native crash signals (SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGTRAP), including crashes
//! inside the GPU driver: a thin, safe seam over Embark's `crash-handler` (ADR-158).
//!
//! Linux and Windows install a handler (`attach` exists only there; on Windows it is a structured
//! exception filter, not a signal handler). On every other target nothing changes, so callers fall
//! back to the panic-only behaviour. The feedback crash bundle (`regia_framework::feedback::native`) is the user.
//!
//! Android (ADR-328) installs the same signal handlers, but cannot run a monitor process: the
//! handler writes an in-process [`record`] into a pre-opened file, and the next launch reads it
//! back (a log line, a `logs://` report, and a crash bundle with the feedback plugin). The
//! platform arms it at startup in every Android app; `attach` there takes the record folder.
//!
//! The callback runs **inside a signal handler** on the crashing thread, with the process in an
//! unknown state: it must not allocate, lock anything it cannot `try_lock`, or panic. After it
//! returns, the previous handlers are restored and the signal is re-raised, so the process dies
//! exactly as it would have without the handler (Rust's stack-overflow message, the exit status,
//! a core dump if enabled). A signal *sent* by a process (`kill -SEGV`: `si_code <= 0`) is not
//! re-raised that way, so after the callback it gets its default action at once: the process
//! dies of that signal right after the bundle request instead of at the watchdog.
//!
//! Once a deliberate exit has begun ([`crate::begin_deliberate_exit`]: a lost GPU device whose
//! bundle is already written), the handler is disarmed: the callback does not run (no second,
//! "native crash" bundle), and the fault ends the process at once with the deliberate exit code
//! (`_exit` / `TerminateProcess`, both safe in a handler). A fault then is the dead device's
//! teardown racing a driver thread, not a new crash (ADR-276).

/// Why no handler was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeCrashError {
    /// This target has no native crash handler (everything but Linux, Windows and Android for now).
    Unsupported,
    /// A handler is already attached in this process.
    AlreadyAttached,
    /// `crash-handler` could not install the handler.
    Install(String),
}

impl std::fmt::Display for NativeCrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("native crash handling is not supported on this target"),
            Self::AlreadyAttached => f.write_str("a native crash handler is already attached"),
            Self::Install(e) => write!(f, "could not install the native crash handler: {e}"),
        }
    }
}

impl std::error::Error for NativeCrashError {}

pub mod record;

#[cfg(any(target_os = "android", all(test, target_os = "linux")))]
mod memory_probe;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub(crate) use android::start_records;
#[cfg(target_os = "android")]
pub use android::{
    CRASH_TEST_PROPERTY, CrashContext, RECORD_DIR, attach, is_attached, set_recording, take_collected,
};
#[cfg(target_os = "linux")]
pub use linux::{CrashContext, arm_crash_watchdog, attach, is_attached};
#[cfg(windows)]
pub use windows::{
    CrashContext, CrashInfo, arm_crash_watchdog, attach, crash_info, dump_in_progress, is_attached,
    process_alive, set_dump_wait, thread_name,
};

/// The Windows backend: `crash-handler`'s unhandled-exception filter plus the CRT abort,
/// invalid-parameter and pure-call hooks, and the heap-corruption vectored handler.
///
/// The callback runs **inside the exception filter** on the crashing thread, in a process in an
/// unknown state, under the same rules as the Linux signal handler: no allocation, `try_lock`
/// only, no panics. When it returns, the filter reports "continue search": the process ends the
/// way it would have without us (Windows Error Reporting, the exit code is the exception code).
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, GetThreadDescription, INFINITE, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, SetEvent, THREAD_QUERY_LIMITED_INFORMATION, TerminateProcess,
        WaitForSingleObject,
    };

    use super::NativeCrashError;

    /// The crashing thread's state, as `crash-handler` delivers it (exception pointers, exception
    /// code, pid, tid). Hand it to the out-of-process minidump writer.
    pub use crash_handler::CrashContext;

    static HANDLER: OnceLock<crash_handler::CrashHandler> = OnceLock::new();
    /// The watchdog thread's wake-up event (created at attach so the crash path allocates
    /// nothing), as a `usize` because raw handles are not `Sync`.
    static WATCHDOG_EVENT: OnceLock<usize> = OnceLock::new();
    static WATCHDOG_SECONDS: AtomicU32 = AtomicU32::new(5);
    /// Extra seconds the watchdog waits while the monitor writes the minidump.
    static DUMP_WAIT_SECONDS: AtomicU32 = AtomicU32::new(30);
    /// Set while the crash path waits for the monitor's dump acknowledgment.
    static DUMP_ACTIVE: AtomicBool = AtomicBool::new(false);
    /// `STILL_ACTIVE` (259) would read as "running"; the watchdog's exit code is the exception.
    static EXIT_CODE: AtomicU32 = AtomicU32::new(1);

    /// What the monitor report needs from a Windows exception: the code, whether an access
    /// violation read (0), wrote (1) or fetched (8), the address it touched, process and thread.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CrashInfo {
        /// The `NTSTATUS` exception code (`0xC0000005` for an access violation).
        pub code: u32,
        /// For access violations and in-page errors: 0 read, 1 write, 8 DEP; else 0.
        pub access: u32,
        /// The address the access touched (access violations), else the faulting instruction.
        pub address: u64,
        /// The crashed process.
        pub pid: u32,
        /// The crashing thread.
        pub tid: u32,
    }

    /// The `EXCEPTION_RECORD` fields read here (the layout is fixed by the Windows ABI).
    #[repr(C)]
    struct ExceptionRecord {
        code: i32,
        flags: u32,
        next: *const ExceptionRecord,
        address: *const std::ffi::c_void,
        parameters: u32,
        information: [usize; 15],
    }

    /// `EXCEPTION_POINTERS`: the record, then the CPU context (not needed here).
    #[repr(C)]
    struct ExceptionPointers {
        record: *const ExceptionRecord,
        context: *const std::ffi::c_void,
    }

    /// The address of the `CrashContext` the exception filter is handling right now (0 when no
    /// filter runs). Set by [`attach`]'s filter just before the callback, cleared after it.
    static LIVE_CONTEXT: AtomicUsize = AtomicUsize::new(0);

    /// Reads the exception out of `cc`. Allocation-free; safe in the filter.
    ///
    /// The exception record behind `cc.exception_pointers` is valid only while the exception
    /// filter that produced `cc` runs. The record is therefore read only when `cc` is the very
    /// context the filter is handling right now (the reference the callback of [`attach`]
    /// received); for any other context (a copy, or one used after the filter returned) only the
    /// plain fields are used: `access` is 0 and `address` is 0.
    #[must_use]
    pub fn crash_info(cc: &CrashContext) -> CrashInfo {
        let mut info = CrashInfo {
            code: cc.exception_code.cast_unsigned(),
            access: 0,
            address: 0,
            pid: cc.process_id,
            tid: cc.thread_id,
        };
        if LIVE_CONTEXT.load(Ordering::SeqCst) != std::ptr::from_ref(cc).addr() {
            return info;
        }
        let pointers = cc.exception_pointers.cast::<ExceptionPointers>();
        // SAFETY: `cc` is the context the running exception filter was handed (checked above), so
        // `exception_pointers` is null or the live `EXCEPTION_POINTERS` of the exception being
        // handled; the record layout is the ABI's.
        unsafe {
            if let Some(record) = pointers.as_ref().and_then(|p| p.record.as_ref()) {
                info.address = record.address as u64;
                if matches!(info.code, 0xC000_0005 | 0xC000_0006) && record.parameters >= 2 {
                    info.access = record.information[0] as u32;
                    info.address = record.information[1] as u64;
                }
            }
        }
        info
    }

    /// Sets how long (seconds) the watchdog waits, beyond its own bound, for a minidump the
    /// monitor is still writing (see [`dump_in_progress`]). Default 30.
    pub fn set_dump_wait(seconds: u32) {
        DUMP_WAIT_SECONDS.store(seconds, Ordering::SeqCst);
    }

    /// Marks the dump request as sent (`true`) or acknowledged (`false`). While set, the
    /// watchdog does not kill the process at its bound but waits up to the dump wait, so a
    /// minidump that is still being written is not cut short. Async-safe.
    pub fn dump_in_progress(active: bool) {
        DUMP_ACTIVE.store(active, Ordering::SeqCst);
    }

    /// Bounds the crash path: within `seconds` the process is dead, whatever the callback (or
    /// the monitor it waits on) does. A thread started at [`attach`] waits on an event; this only
    /// sets the event (async-safe), and the thread then sleeps and calls `TerminateProcess`.
    pub fn arm_crash_watchdog(seconds: u32) {
        WATCHDOG_SECONDS.store(seconds, Ordering::SeqCst);
        if let Some(&event) = WATCHDOG_EVENT.get() {
            // SAFETY: `event` is a live event handle created at attach and never closed.
            unsafe { SetEvent(event as HANDLE) };
        }
    }

    /// Starts the watchdog thread and publishes its event. Fails (leaving nothing behind) when
    /// the event or the thread cannot be created, so `attach` can refuse to run unbounded.
    fn start_watchdog_thread() -> Result<(), NativeCrashError> {
        // SAFETY: plain Win32 call with null attributes and name.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(NativeCrashError::Install(
                "could not create the crash watchdog event".into(),
            ));
        }
        let event_addr = event as usize;
        let spawned = std::thread::Builder::new()
            .name("regia-crash-watchdog".into())
            .spawn(move || {
                // SAFETY: waits on the event created above, then ends this process.
                unsafe {
                    WaitForSingleObject(event_addr as HANDLE, INFINITE);
                    std::thread::sleep(std::time::Duration::from_secs(u64::from(
                        WATCHDOG_SECONDS.load(Ordering::SeqCst).min(3600),
                    )));
                    let wait_end = std::time::Instant::now()
                        + std::time::Duration::from_secs(u64::from(
                            DUMP_WAIT_SECONDS.load(Ordering::SeqCst).min(600),
                        ));
                    while DUMP_ACTIVE.load(Ordering::SeqCst) && std::time::Instant::now() < wait_end {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    TerminateProcess(GetCurrentProcess(), EXIT_CODE.load(Ordering::SeqCst));
                }
            });
        if let Err(e) = spawned {
            // SAFETY: closes the event created above; no thread holds it.
            unsafe { CloseHandle(event) };
            return Err(NativeCrashError::Install(format!(
                "could not start the crash watchdog thread: {e}"
            )));
        }
        if WATCHDOG_EVENT.set(event_addr).is_err() {
            // A concurrent attach won; the thread above waits forever on its own event, harmless.
            return Err(NativeCrashError::AlreadyAttached);
        }
        Ok(())
    }

    /// Whether process `pid` still runs. A process that cannot be opened counts as gone.
    #[must_use]
    pub fn process_alive(pid: u32) -> bool {
        // SAFETY: opens, waits with a zero timeout and closes a handle we own.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid);
            if handle.is_null() {
                return false;
            }
            let alive = WaitForSingleObject(handle, 0) == 0x102; // WAIT_TIMEOUT
            CloseHandle(handle);
            alive
        }
    }

    /// The name another process gave thread `tid` (`SetThreadDescription`, which Rust's
    /// `thread::Builder::name` calls), when it can be read.
    #[must_use]
    pub fn thread_name(tid: u32) -> Option<String> {
        // SAFETY: the description is a system-allocated wide string we read then `LocalFree`.
        unsafe {
            let handle = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
            if handle.is_null() {
                return None;
            }
            let mut text: *mut u16 = std::ptr::null_mut();
            let hr = GetThreadDescription(handle, &raw mut text);
            CloseHandle(handle);
            if hr < 0 || text.is_null() {
                return None;
            }
            let name = String::from_utf16_lossy(crate::wide::until_nul(text));
            LocalFree(text.cast());
            (!name.is_empty()).then_some(name)
        }
    }

    /// A fault after a deliberate exit began (ADR-276): `TerminateProcess` with its exit code,
    /// looped so that a return (it should not) never falls through to the crash path.
    fn end_deliberate_exit(code: i32) -> ! {
        loop {
            // SAFETY: terminates the current process; no pointers.
            unsafe { TerminateProcess(GetCurrentProcess(), code.cast_unsigned()) };
        }
    }

    /// Installs `on_crash` as the process's native crash handler. `ptracer` is ignored on
    /// Windows (`MiniDumpWriteDump` needs no ptrace permission).
    ///
    /// `on_crash` runs in the exception filter: no allocation, no blocking locks, no panics. Its
    /// result is ignored; the process then dies the way it would have. Before it runs,
    /// [`arm_crash_watchdog`]`(watchdog_s)` guarantees the process dies within `watchdog_s`
    /// seconds even if `on_crash` blocks forever.
    ///
    /// # Errors
    /// [`NativeCrashError::AlreadyAttached`] on a second call, [`NativeCrashError::Install`] when
    /// the handlers could not be installed.
    pub fn attach(
        on_crash: impl Fn(&CrashContext) + Send + Sync + 'static,
        _ptracer: Option<u32>,
        watchdog_s: u32,
    ) -> Result<(), NativeCrashError> {
        if HANDLER.get().is_some() {
            return Err(NativeCrashError::AlreadyAttached);
        }
        start_watchdog_thread()?;
        // SAFETY: `make_crash_event` is unsafe only because the closure runs in an exception
        // filter; the contract above is the caller's, and the one caller
        // (`regia_framework::feedback::native`) upholds it.
        let event = unsafe {
            crash_handler::make_crash_event(move |context: &CrashContext| {
                // Disarmed (ADR-276): the deliberate exit's bundle is written; end with its code.
                if let Some(code) = crate::deliberate_exit::PROCESS.run_crash_hook(
                    || {
                        EXIT_CODE.store(context.exception_code.cast_unsigned(), Ordering::SeqCst);
                        arm_crash_watchdog(watchdog_s);
                    },
                    || {
                        // `crash_info` reads the exception record only for this live context.
                        let addr = std::ptr::from_ref(context).addr();
                        LIVE_CONTEXT.store(addr, Ordering::SeqCst);
                        on_crash(context);
                        let _ = LIVE_CONTEXT.compare_exchange(addr, 0, Ordering::SeqCst, Ordering::SeqCst);
                    },
                ) {
                    end_deliberate_exit(code);
                }
                // `false`: continue the search, so the process dies as it would have without us.
                crash_handler::CrashEventResult::Handled(false)
            })
        };
        let handler = crash_handler::CrashHandler::attach(event)
            .map_err(|e| NativeCrashError::Install(e.to_string()))?;
        HANDLER
            .set(handler)
            .map_err(|_| NativeCrashError::AlreadyAttached)
    }

    /// Whether a handler is attached in this process.
    #[must_use]
    pub fn is_attached() -> bool {
        HANDLER.get().is_some()
    }

    #[cfg(test)]
    mod tests {
        use std::sync::{Mutex, PoisonError};

        use super::*;

        /// Serializes the tests that publish a live context (`LIVE_CONTEXT` is process-wide).
        static LIVE: Mutex<()> = Mutex::new(());

        fn record(code: i32, address: usize, access: usize, fault: usize) -> ExceptionRecord {
            let mut information = [0usize; 15];
            information[0] = access;
            information[1] = fault;
            ExceptionRecord {
                code,
                flags: 0,
                next: std::ptr::null(),
                address: std::ptr::without_provenance(address),
                parameters: 2,
                information,
            }
        }

        fn context(pointers: *const ExceptionPointers, code: i32) -> CrashContext {
            CrashContext {
                exception_pointers: pointers.cast(),
                exception_code: code,
                process_id: 7,
                thread_id: 9,
            }
        }

        #[test]
        fn crash_info_ignores_the_record_of_a_context_no_filter_is_handling() {
            // A dangling pointer: reading it would crash, so this proves no read happens.
            let cc = context(std::ptr::without_provenance(16), 0xC000_0005_u32.cast_signed());
            let info = crash_info(&cc);
            assert_eq!(
                info,
                CrashInfo {
                    code: 0xC000_0005,
                    access: 0,
                    address: 0,
                    pid: 7,
                    tid: 9,
                }
            );
        }

        #[test]
        fn crash_info_reads_the_access_violation_of_the_live_context() {
            let code = 0xC000_0005_u32.cast_signed();
            let rec = record(code, 0x1000, 1, 0xdead_0000);
            let pointers = ExceptionPointers {
                record: &raw const rec,
                context: std::ptr::null(),
            };
            let cc = context(&raw const pointers, code);
            let _live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
            let addr = std::ptr::from_ref(&cc).addr();
            LIVE_CONTEXT.store(addr, Ordering::SeqCst);
            let info = crash_info(&cc);
            let _ = LIVE_CONTEXT.compare_exchange(addr, 0, Ordering::SeqCst, Ordering::SeqCst);
            assert_eq!((info.access, info.address), (1, 0xdead_0000));
            assert_eq!((info.code, info.pid, info.tid), (0xC000_0005, 7, 9));
        }

        #[test]
        fn crash_info_uses_the_instruction_address_for_other_exceptions() {
            let code = 0x8000_0003_u32.cast_signed(); // breakpoint
            let rec = record(code, 0x4242, 1, 0xdead_0000);
            let pointers = ExceptionPointers {
                record: &raw const rec,
                context: std::ptr::null(),
            };
            let cc = context(&raw const pointers, code);
            let _live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
            let addr = std::ptr::from_ref(&cc).addr();
            LIVE_CONTEXT.store(addr, Ordering::SeqCst);
            let info = crash_info(&cc);
            let _ = LIVE_CONTEXT.compare_exchange(addr, 0, Ordering::SeqCst, Ordering::SeqCst);
            assert_eq!((info.access, info.address), (0, 0x4242));
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::OnceLock;

    use super::NativeCrashError;

    /// The crashing thread's state, as `crash-handler` delivers it (signal info, registers, pid,
    /// tid). Hand it to the out-of-process minidump writer.
    pub use crash_handler::CrashContext;

    static HANDLER: OnceLock<crash_handler::CrashHandler> = OnceLock::new();

    /// Bounds the crash path: within `seconds` the process is dead, whatever the callback (or the
    /// process it waits on) does. Async-signal-safe: raw syscalls only, no allocation, no locks.
    ///
    /// A forked watchdog sends `SIGKILL`. `SIGKILL` is used because it is the one signal a ptrace
    /// stop cannot hold: a monitor that hangs while it has the game ptrace-attached would swallow
    /// `SIGALRM`. The watchdog is a raw `clone(SIGCHLD)`, so glibc's `fork` atfork handlers (which
    /// take the malloc locks the crash may hold) do not run. `PR_SET_PDEATHSIG` ends it with the
    /// game, so a fast crash leaves nothing behind. If the clone fails, the fallback is
    /// `alarm(seconds)` with `SIGALRM` at its default action (terminate).
    pub fn arm_crash_watchdog(seconds: u32) {
        // SAFETY: only async-signal-safe libc calls (getpid, syscall(clone), sigaction, alarm);
        // the child path calls only raw syscalls and `_exit` and never returns.
        unsafe {
            let parent = libc::getpid();
            let zero: libc::c_long = 0;
            let pid = libc::syscall(
                libc::SYS_clone,
                libc::c_long::from(libc::SIGCHLD),
                zero,
                zero,
                zero,
                zero,
            );
            if pid == 0 {
                watchdog_child(parent, seconds);
            }
            if pid < 0 {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut());
                libc::alarm(seconds);
            }
        }
    }

    /// A pidfd for `pid` (`pidfd_open`, Linux 5.3+), or a negative value when unavailable.
    /// Raw syscall: async-signal-safe.
    pub(crate) fn pidfd_open(pid: libc::pid_t) -> libc::c_int {
        // SAFETY: plain syscall on integer arguments.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::c_long::from(pid), 0 as libc::c_long) };
        libc::c_int::try_from(fd).unwrap_or(-1)
    }

    /// Sends `SIGKILL` through `pidfd`: a pid that was recycled after the process behind the fd
    /// ended fails with `ESRCH` instead of killing the new owner. Returns whether it was sent.
    pub(crate) fn pidfd_kill(pidfd: libc::c_int) -> bool {
        // SAFETY: plain syscall; a null siginfo asks for the default `si_code`.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                libc::c_long::from(pidfd),
                libc::c_long::from(libc::SIGKILL),
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        rc == 0
    }

    /// The watchdog process: sleeps, then kills the game if it is still its parent.
    ///
    /// # Safety
    ///
    /// Only for the child of the raw `clone` in [`arm_crash_watchdog`]: one thread, a copy of a
    /// process in an unknown state, so nothing here allocates, locks or returns.
    unsafe fn watchdog_child(parent: libc::pid_t, seconds: u32) -> ! {
        // SAFETY: raw syscalls on plain values in a freshly cloned single-threaded child.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // Opened before the parent check: while we are still its child the pid cannot have
            // been recycled, so the fd names the game, and signalling it later cannot hit a
            // process that reused the pid.
            let pidfd = pidfd_open(parent);
            if libc::getppid() == parent {
                let mut request = libc::timespec {
                    tv_sec: libc::time_t::from(seconds.min(3600) as i32),
                    tv_nsec: 0,
                };
                let mut remaining = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                while libc::nanosleep(&request, &mut remaining) == -1 {
                    request = remaining;
                }
                if pidfd >= 0 {
                    pidfd_kill(pidfd);
                } else if libc::getppid() == parent {
                    libc::kill(parent, libc::SIGKILL);
                }
            }
            libc::_exit(0)
        }
    }

    /// Whether `signal` was sent by a process (`kill`, `tgkill`, `sigqueue`: `si_code <= 0`) rather
    /// than raised by a fault, and is not `SIGABRT`.
    ///
    /// A sent signal is not re-raised by returning from the handler, and `crash-handler`'s own
    /// re-raise lands in the handler it restores: Rust std's SIGSEGV/SIGBUS stack-overflow handler,
    /// which only resets the disposition and returns. The game then stayed alive until the
    /// watchdog's `SIGKILL` (5 s). `SIGABRT` is left to `crash-handler`, whose re-raise already
    /// reaches the default action (std installs no SIGABRT handler) and which the panic-abort
    /// dedupe relies on unchanged.
    pub(crate) fn is_sent_signal(signal: libc::c_int, si_code: i32) -> bool {
        si_code <= 0 && signal != libc::SIGABRT
    }

    /// Ends the process with `signal` at its default action, now: `SIG_DFL`, unblock it on this
    /// thread (the kernel masked it for the handler) and send it to this thread, so it is
    /// delivered as the `tgkill` returns. The exit status reports `signal` (and a core dump is
    /// written if enabled), exactly as for a fault. If that somehow returns, `SIGKILL`.
    /// Async-signal-safe: `sigaction`, `sigprocmask` and raw syscalls on stack values only.
    fn die_of_sent_signal(signal: libc::c_int) {
        // SAFETY: plain libc/syscall wrappers on zeroed stack values; nothing is allocated or
        // locked, and the process is meant to end here.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
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

    /// A fault after a deliberate exit began (ADR-276): one fixed line on stderr, then `_exit`
    /// with the deliberate exit code. Async-signal-safe: `write` and `_exit` on static data.
    fn end_deliberate_exit(code: i32) -> ! {
        const LINE: &[u8] = b"regia exit: native fault during a deliberate exit; ending with its exit code\n";
        // SAFETY: `write` reads the static slice; `_exit` takes no pointers and never returns.
        unsafe {
            libc::write(libc::STDERR_FILENO, LINE.as_ptr().cast(), LINE.len());
            libc::_exit(code)
        }
    }

    /// Installs `on_crash` as the process's native crash handler. `ptracer` is the process that
    /// will read this one's memory to write the minidump (Yama `ptrace_scope = 1` needs it named).
    ///
    /// `on_crash` runs in a signal handler: no allocation, no blocking locks, no panics. Its result
    /// is ignored; the previous handlers always run afterwards. Before it runs,
    /// [`arm_crash_watchdog`]`(watchdog_s)` guarantees the process dies within `watchdog_s`
    /// seconds even if `on_crash` blocks forever (a crashed fullscreen game must never stay
    /// frozen on the compositor).
    ///
    /// # Errors
    /// [`NativeCrashError::AlreadyAttached`] on a second call, [`NativeCrashError::Install`] when
    /// the signal handlers could not be installed.
    pub fn attach(
        on_crash: impl Fn(&CrashContext) + Send + Sync + 'static,
        ptracer: Option<u32>,
        watchdog_s: u32,
    ) -> Result<(), NativeCrashError> {
        if HANDLER.get().is_some() {
            return Err(NativeCrashError::AlreadyAttached);
        }
        // SAFETY: `make_crash_event` is unsafe only because the closure runs in a signal handler;
        // the contract above (no allocation, try-locks only, no panics) is the caller's, and the
        // one caller (`regia_framework::feedback::native`) upholds it.
        let event = unsafe {
            crash_handler::make_crash_event(move |context: &CrashContext| {
                if let Some(code) = crate::deliberate_exit::PROCESS
                    .run_crash_hook(|| arm_crash_watchdog(watchdog_s), || on_crash(context))
                {
                    end_deliberate_exit(code);
                }
                let signal = context.siginfo.ssi_signo.cast_signed();
                if is_sent_signal(signal, context.siginfo.ssi_code) {
                    die_of_sent_signal(signal);
                }
                // `false`: restore the previous handlers and re-raise, so the process dies as it
                // would have without us.
                crash_handler::CrashEventResult::Handled(false)
            })
        };
        let handler = crash_handler::CrashHandler::attach(event)
            .map_err(|e| NativeCrashError::Install(e.to_string()))?;
        handler.set_ptracer(ptracer);
        HANDLER
            .set(handler)
            .map_err(|_| NativeCrashError::AlreadyAttached)
    }

    /// Whether a handler is attached in this process.
    #[must_use]
    pub fn is_attached() -> bool {
        HANDLER.get().is_some()
    }
}

/// Whether a handler is attached in this process (never, on this target).
#[cfg(not(any(target_os = "linux", windows, target_os = "android")))]
#[must_use]
pub fn is_attached() -> bool {
    false
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{is_sent_signal, pidfd_kill, pidfd_open};

    #[test]
    fn pidfd_kill_signals_the_process_the_fd_names() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let fd = pidfd_open(child.id().cast_signed());
        if fd < 0 {
            // Kernel older than 5.3 or a sandbox that blocks pidfd_open: the watchdog falls back.
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        assert!(pidfd_kill(fd));
        let status = child.wait().expect("wait");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        // SAFETY: closes the fd opened above.
        unsafe { libc::close(fd) };
        assert!(!pidfd_kill(-1), "an invalid fd sends nothing");
    }

    #[test]
    fn only_sent_non_abort_signals_take_the_immediate_path() {
        // kill (SI_USER 0), tgkill (SI_TKILL -6), sigqueue (SI_QUEUE -1).
        for code in [0, -6, -1] {
            assert!(is_sent_signal(libc::SIGSEGV, code));
            assert!(is_sent_signal(libc::SIGBUS, code));
            assert!(is_sent_signal(libc::SIGILL, code));
            assert!(
                !is_sent_signal(libc::SIGABRT, code),
                "abort keeps crash-handler's path"
            );
        }
        // Faults: SEGV_MAPERR 1, SEGV_ACCERR 2, SI_KERNEL 0x80.
        for code in [1, 2, 0x80] {
            assert!(!is_sent_signal(libc::SIGSEGV, code));
        }
    }
}
