// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/stack_dump.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! Linux GNU stack sampling; adapted from DO27 stack_dump.rs:44-196, 320-364,
//! d6ed37677 (regia-absorb-1), owner-authorized P-08 copy-and-adapt, class P3.
//! Named adaptations: configurable budgets, preserved caller/all identities,
//! conflict-aware installation and buffer ownership across timed-out captures.

use super::{FRAME_CAPACITY, SampleOutcome, Settings, StackDump, ThreadSample};
use std::ffi::{c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const IDLE: i32 = 0;
const CLAIMED: i32 = -1;
const DONE: i32 = -2;
static SIGNAL: AtomicI32 = AtomicI32::new(0);
static BUSY: Mutex<()> = Mutex::new(());
static SLOT: Slot = Slot::new();
#[cfg(test)]
static SIGNAL_TESTS: Mutex<()> = Mutex::new(());

// SAFETY: these declarations match the system unwinder ABI. The callback uses only
// valid contexts supplied by _Unwind_Backtrace; no pointer escapes the call.
unsafe extern "C" {
    fn _Unwind_Backtrace(trace: extern "C" fn(*mut c_void, *mut c_void) -> c_int, arg: *mut c_void) -> c_int;
    fn _Unwind_GetIP(ctx: *mut c_void) -> usize;
}

struct Slot {
    // Positive values are target TIDs. CLAIMED grants the handler exclusive write
    // ownership; DONE releases it to the capturing thread. Only the BUSY holder
    // may request/cancel/read/reclaim. A timeout never changes CLAIMED.
    state: AtomicI32,
    count: AtomicUsize,
    limit: AtomicUsize,
    frames: [AtomicUsize; FRAME_CAPACITY],
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicI32::new(IDLE),
            count: AtomicUsize::new(0),
            limit: AtomicUsize::new(FRAME_CAPACITY),
            frames: [const { AtomicUsize::new(0) }; FRAME_CAPACITY],
        }
    }

    fn prepare(&self, tid: i32, limit: usize) -> bool {
        if self.state.load(Ordering::Acquire) == CLAIMED {
            return false;
        }
        self.limit.store(limit, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
        self.state.store(tid, Ordering::Release);
        true
    }

    fn claim(&self, tid: i32) -> bool {
        self.state
            .compare_exchange(tid, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn cancel(&self, tid: i32) -> bool {
        self.state
            .compare_exchange(tid, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn read(&self) -> Option<Vec<usize>> {
        if self.state.load(Ordering::Acquire) != DONE {
            return None;
        }
        let n = self.count.load(Ordering::Relaxed).min(FRAME_CAPACITY);
        let frames = self.frames[..n]
            .iter()
            .map(|frame| frame.load(Ordering::Relaxed))
            .collect();
        self.state.store(IDLE, Ordering::Release);
        Some(frames)
    }
}

fn gettid() -> i32 {
    // SAFETY: gettid takes no pointer arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

extern "C" fn on_frame(ctx: *mut c_void, _arg: *mut c_void) -> c_int {
    let n = SLOT.count.load(Ordering::Relaxed);
    if n >= SLOT.limit.load(Ordering::Relaxed).min(FRAME_CAPACITY) {
        return 4;
    }
    // SAFETY: ctx is the unwinder's context for this callback, valid until return.
    let ip = unsafe { _Unwind_GetIP(ctx) };
    if ip == 0 {
        return 4;
    }
    SLOT.frames[n].store(ip, Ordering::Relaxed);
    SLOT.count.store(n + 1, Ordering::Relaxed);
    0
}

extern "C" fn on_sample(_signal: c_int) {
    // SAFETY: errno is this thread's TLS; its original value is restored below.
    let errno = unsafe { *libc::__errno_location() };
    if SLOT.claim(gettid()) {
        // SAFETY: the handler owns SLOT until its release-store of DONE. The
        // callback touches atomics only. The unwinder may stall; timeouts on the
        // coordinator never revoke a running handler's ownership (ADR-8156).
        unsafe {
            _Unwind_Backtrace(on_frame, std::ptr::null_mut());
        }
        SLOT.state.store(DONE, Ordering::Release);
    }
    // SAFETY: restore this thread's saved errno after the signal interruption.
    unsafe {
        *libc::__errno_location() = errno;
    }
}

pub(super) fn install(signal: i32) -> std::io::Result<()> {
    if signal < libc::SIGRTMIN() || signal > libc::SIGRTMAX() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sampling requires a reserved real-time signal",
        ));
    }
    let _guard = BUSY
        .lock()
        .map_err(|_| std::io::Error::other("sampler lock poisoned"))?;
    let installed = SIGNAL.load(Ordering::Acquire);
    if installed == signal {
        return Ok(());
    }
    if installed != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "sampler already uses another signal",
        ));
    }
    install_handler(signal, on_sample)?;
    SIGNAL.store(signal, Ordering::Release);
    Ok(())
}

fn install_handler(signal: i32, handler: extern "C" fn(c_int)) -> std::io::Result<()> {
    // SAFETY: sigaction is plain C data; both zeroed structs are fully initialized
    // before use, and valid for the syscall's output/input respectively.
    let (mut previous, mut action): (libc::sigaction, libc::sigaction) = unsafe { std::mem::zeroed() };
    // SAFETY: queries the handler only, writing to a valid sigaction object.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if previous.sa_sigaction != libc::SIG_DFL {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "diagnostic signal already has a handler",
        ));
    }
    action.sa_sigaction = handler as usize;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: a writable sigset and a static, ABI-correct handler. Installation is
    // coordinated at startup by the host; no other library may claim this signal.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn sample(tid: i32, settings: &Settings, signal: i32) -> SampleOutcome {
    if !SLOT.prepare(tid, settings.max_frames) {
        return SampleOutcome::Busy;
    }
    // SAFETY: tgkill targets a TID in this process with the installed sampler signal.
    // For the caller it executes synchronously: this normal thread's own unwind
    // has the same best-effort limits as an ordinary backtrace.
    let sent = unsafe { libc::syscall(libc::SYS_tgkill, std::process::id() as i32, tid, signal) };
    if sent != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        SLOT.cancel(tid);
        return if errno == libc::ESRCH {
            SampleOutcome::Exited
        } else {
            SampleOutcome::SignalError(errno)
        };
    }
    let start = Instant::now();
    loop {
        if let Some(frames) = SLOT.read() {
            return SampleOutcome::Captured(frames);
        }
        if start.elapsed() >= settings.sample_wait {
            if SLOT.cancel(tid) {
                return SampleOutcome::NoResponse;
            }
            if start.elapsed() >= settings.sample_wait.saturating_add(settings.walk_wait) {
                return SampleOutcome::WalkTimedOut;
            }
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

pub(super) fn capture(settings: &Settings) -> std::io::Result<StackDump> {
    let signal = SIGNAL.load(Ordering::Acquire);
    if signal == 0 {
        return Err(std::io::Error::other("stack sampler is not installed"));
    }
    let caller = gettid() as u32;
    let mut threads = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let name = std::fs::read_to_string(entry.path().join("comm"))
            .map(|name| name.trim().to_owned())
            .unwrap_or_else(|_| "?".into());
        threads.push(ThreadSample {
            id,
            name,
            is_caller: id == caller,
            outcome: SampleOutcome::Busy,
            stack: String::new(),
        });
    }
    threads.sort_by_key(|thread| thread.id);
    if let Ok(_guard) = BUSY.try_lock() {
        for (i, thread) in threads.iter_mut().enumerate() {
            thread.outcome = if i < settings.max_samples {
                sample(thread.id as i32, settings, signal)
            } else {
                SampleOutcome::LimitReached
            };
        }
    }
    super::symbols::symbolize(
        &mut threads,
        settings.max_symbol_bytes,
        settings.max_symbol_name_bytes,
    );
    Ok(StackDump { threads })
}

static REQUEST_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_request(_signal: c_int) {
    super::request();
}

pub(super) fn install_request_signal(signal: i32) -> std::io::Result<()> {
    if ![libc::SIGUSR1, libc::SIGUSR2].contains(&signal)
        && !(libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "requests require SIGUSR1, SIGUSR2 or a reserved real-time signal",
        ));
    }
    let _guard = BUSY
        .lock()
        .map_err(|_| std::io::Error::other("sampler lock poisoned"))?;
    let installed = REQUEST_SIGNAL.load(Ordering::Acquire);
    if installed == signal {
        return Ok(());
    }
    if installed != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "request handler already uses another signal",
        ));
    }
    install_handler(signal, on_request)?;
    REQUEST_SIGNAL.store(signal, Ordering::Release);
    Ok(())
}

#[cfg(test)]
fn isolated(test: &str) -> bool {
    if std::env::var("REGIA_PLATFORM_STACK_CHILD").as_deref() == Ok(test) {
        return false;
    }
    // The sampler intentionally interrupts every thread. Keep unrelated platform
    // tests and their process-wide signal/loader state out of this OS-level proof.
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test, "--nocapture"])
        .env("REGIA_PLATFORM_STACK_CHILD", test)
        .status()
        .expect("isolated signal test");
    assert!(status.success(), "isolated {test}: {status}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_handler_owns_the_buffer_across_later_dumps() {
        let slot = Slot::new();
        assert!(slot.prepare(10, 8));
        assert!(slot.claim(10));
        assert!(!slot.cancel(10));
        assert!(!slot.prepare(20, 1));
        assert!(!slot.prepare(10, 1));
        assert_eq!(slot.limit.load(Ordering::Relaxed), 8);
        slot.frames[0].store(123, Ordering::Relaxed);
        slot.count.store(1, Ordering::Relaxed);
        slot.state.store(DONE, Ordering::Release);
        assert_eq!(slot.read(), Some(vec![123]));
        assert!(slot.prepare(20, 1));
    }

    #[test]
    fn canceled_unclaimed_request_cannot_write_a_later_threads_buffer() {
        let slot = Slot::new();
        assert!(slot.prepare(10, 8));
        assert!(slot.cancel(10));
        assert!(slot.prepare(20, 2));
        assert!(!slot.claim(10));
        assert!(slot.claim(20));
    }
}

#[cfg(test)]
mod signal_tests {
    use super::*;

    #[test]
    fn blocked_signal_times_out_and_a_late_signal_does_not_poison_the_next_capture() {
        if isolated(
            "stack_sampling::imp::signal_tests::blocked_signal_times_out_and_a_late_signal_does_not_poison_the_next_capture",
        ) {
            return;
        }
        let _test = SIGNAL_TESTS.lock().expect("serialize signal tests");
        install(libc::SIGRTMIN() + 6).expect("test fixture");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: initialized sigsets and thread-local signal mask. This worker
            // blocks only the reserved sampling signal, and restores its old mask.
            let old = unsafe {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, SIGNAL.load(Ordering::Acquire));
                assert_eq!(libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut old), 0);
                old
            };
            ready_tx.send(gettid() as u32).expect("test fixture");
            release_rx.recv().expect("test fixture");
            // SAFETY: old is this worker's previous valid mask. A canceled queued
            // sampling signal can now run; it must not claim another thread's slot.
            unsafe {
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()),
                    0
                );
            }
        });
        let tid = ready_rx.recv().expect("test fixture");
        let settings = Settings {
            sample_wait: Duration::from_millis(1),
            max_symbol_bytes: 0,
            ..Settings::default()
        };
        let dump = capture(&settings).expect("test fixture");
        release_tx.send(()).expect("test fixture");
        worker.join().expect("test fixture");
        assert_eq!(
            dump.threads
                .iter()
                .find(|thread| thread.id == tid)
                .expect("test fixture")
                .outcome,
            SampleOutcome::NoResponse
        );
        let next = capture(&Settings {
            max_symbol_bytes: 0,
            ..Settings::default()
        })
        .expect("test fixture");
        assert!(
            next.threads
                .iter()
                .any(|thread| thread.is_caller && matches!(thread.outcome, SampleOutcome::Captured(_)))
        );
        assert_eq!(SLOT.state.load(Ordering::Acquire), IDLE);
        // The vanished thread cannot still have a request or claim.
        let _guard = BUSY.lock().expect("test fixture");
        assert_eq!(
            sample(tid as i32, &settings, SIGNAL.load(Ordering::Acquire)),
            SampleOutcome::Exited
        );
        let busy = capture(&settings).expect("test fixture");
        assert!(
            busy.threads
                .iter()
                .all(|thread| thread.outcome == SampleOutcome::Busy)
        );
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    #[test]
    fn request_signals_coalesce_without_sampling_and_conflicts_are_refused() {
        if isolated(
            "stack_sampling::imp::request_tests::request_signals_coalesce_without_sampling_and_conflicts_are_refused",
        ) {
            return;
        }
        let _test = SIGNAL_TESTS.lock().expect("serialize signal tests");
        install_request_signal(libc::SIGUSR1).expect("request handler");
        install_request_signal(libc::SIGUSR1).expect("idempotent install");
        assert!(!super::super::take_request());
        // SAFETY: raises our installed request signal on the calling thread; its
        // handler sets an atomic flag only and returns synchronously.
        unsafe {
            assert_eq!(libc::raise(libc::SIGUSR1), 0);
            assert_eq!(libc::raise(libc::SIGUSR1), 0);
        }
        assert!(super::super::take_request());
        assert!(!super::super::take_request());
        assert_eq!(
            install_request_signal(libc::SIGTERM)
                .expect_err("refuse termination signal")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        let _guard = BUSY.lock().expect("installation lock");
        assert_eq!(
            install_handler(libc::SIGUSR1, on_sample)
                .expect_err("occupied signal")
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }
}
