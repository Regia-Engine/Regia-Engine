// SPDX-License-Identifier: MPL-2.0
//! Game panics with a backtrace (`docs/specs/19-editor.md` § 7, ADR-7205).
//!
//! `catch_unwind` hands back only the panic payload; the stack is gone by the time it returns.
//! [`catch`] runs a closure with a per-thread capture flag set, and a process panic hook (installed
//! once, chained in front of whatever hook was there) records `Backtrace::force_capture()` while the
//! flag is set. The caller gets the message and that backtrace together, so the play-in-editor
//! paths log both into the Output Log (the shared log ring the agents read too).
//!
//! A play world's systems run on the ECS task pool, so the panic usually happens on a worker
//! thread and reaches the catching thread by re-raise. While any [`catch`] is active, a panic on
//! another thread is recorded, with its message, in a small process-wide list (the last
//! `ELSEWHERE_CAP`); when its own thread recorded nothing, [`catch`] takes the newest entry whose
//! message is the caught payload's. A panic elsewhere with another message (another thread's
//! failure) is never attributed to it.
//!
//! The hook does nothing while no [`catch`] runs, and always calls the previous hook, so the crash
//! reporter and the default "thread panicked" line still see every panic.
//!
//! `std` has no atomic "wrap the current hook": a chaining install is `take_hook` then `set_hook`,
//! and two of them at once can lose one (both take the same hook; the second set drops the first
//! set's hook and everything it chained). The editor's installers ([`install_hook`], the crash
//! reporter's `crash_reports::install`) go through [`chain_hook`] / [`with_hook_lock`], which hold one
//! process-wide lock across the pair (ADR-8847: the capture hook was lost that way in the full
//! `regia_editor` test run, next to a test that chains its own recorder).

use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once, PoisonError};

/// The most backtrace lines a log record carries (the frames nearest the panic come first).
pub const MAX_LOGGED_BACKTRACE_LINES: usize = 80;

thread_local! {
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    static CAPTURED: RefCell<Option<String>> = const { RefCell::new(None) };
}

static HOOK: Once = Once::new();

/// Held across every editor `take_hook` + `set_hook` pair ([`chain_hook`], [`with_hook_lock`]).
static HOOK_LOCK: Mutex<()> = Mutex::new(());

/// A process panic hook, as `std::panic::take_hook` returns it and `set_hook` takes it.
pub type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// How many [`catch`] calls are running, on any thread.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// How many panics on other threads `ELSEWHERE` keeps.
const ELSEWHERE_CAP: usize = 8;

/// `(message, backtrace)` of recent panics on threads with no [`catch`] of their own, recorded
/// while a catch was active, oldest first.
static ELSEWHERE: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// A panic [`catch`] caught.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaughtPanic {
    /// The panic message (`"unknown panic"` for a payload that is not a string).
    pub message: String,
    /// The backtrace taken at the panic, when the hook ran on this thread.
    pub backtrace: Option<String>,
}

impl CaughtPanic {
    /// The message and the backtrace (at most [`MAX_LOGGED_BACKTRACE_LINES`] lines) as one log text.
    #[must_use]
    pub fn log_text(&self) -> String {
        match &self.backtrace {
            Some(trace) => {
                let mut text = format!("{}\nbacktrace:\n", self.message);
                let mut lines = trace.lines();
                for line in lines.by_ref().take(MAX_LOGGED_BACKTRACE_LINES) {
                    text.push_str(line);
                    text.push('\n');
                }
                if lines.next().is_some() {
                    text.push_str("  ...\n");
                }
                text
            }
            None => format!("{} (no backtrace captured)", self.message),
        }
    }
}

/// Runs `change` with the editor's panic-hook lock held: for an installer that does its own
/// `take_hook` + `set_hook` (a dependency's), so it cannot interleave with [`chain_hook`]. The lock
/// is not reentrant: `change` must not call [`chain_hook`], [`with_hook_lock`] or a first
/// [`install_hook`].
pub fn with_hook_lock<R>(change: impl FnOnce() -> R) -> R {
    let _held = HOOK_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    change()
}

/// Puts the hook `wrap` builds around the current one (`wrap` gets the current hook to call) in
/// front of it, with the take and the set under the editor's panic-hook lock. Does nothing on a
/// panicking thread, where `std` refuses to change the hook.
pub fn chain_hook(wrap: impl FnOnce(PanicHook) -> PanicHook) {
    if std::thread::panicking() {
        return;
    }
    with_hook_lock(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(wrap(previous));
    });
}

/// Installs the capturing panic hook once per process, in front of the current one
/// ([`chain_hook`]). A call on a panicking thread (a [`catch`] inside a drop during unwinding)
/// leaves the install to a later call.
pub fn install_hook() {
    if std::thread::panicking() {
        return;
    }
    HOOK.call_once(|| {
        chain_hook(|previous| {
            Box::new(move |info| {
                if CAPTURING.with(Cell::get) {
                    let trace = Backtrace::force_capture().to_string();
                    CAPTURED.with(|slot| *slot.borrow_mut() = Some(trace));
                } else if ACTIVE.load(Ordering::Acquire) > 0 {
                    let trace = Backtrace::force_capture().to_string();
                    if let Ok(mut list) = ELSEWHERE.lock() {
                        if list.len() >= ELSEWHERE_CAP {
                            list.remove(0);
                        }
                        list.push((
                            info.payload_as_str()
                                .map_or_else(|| message(info.payload()), str::to_owned),
                            trace,
                        ));
                    }
                }
                previous(info);
            })
        });
    });
}

/// Runs `f`, catching a panic together with its backtrace.
///
/// # Errors
///
/// The [`CaughtPanic`] when `f` panicked.
pub fn catch<R>(f: impl FnOnce() -> R) -> Result<R, CaughtPanic> {
    install_hook();
    let was = CAPTURING.with(|flag| flag.replace(true));
    CAPTURED.with(|slot| slot.borrow_mut().take());
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    let result = std::panic::catch_unwind(AssertUnwindSafe(f));
    ACTIVE.fetch_sub(1, Ordering::AcqRel);
    CAPTURING.with(|flag| flag.set(was));
    result.map_err(|payload| {
        let message = message(payload.as_ref());
        let own = CAPTURED.with(|slot| slot.borrow_mut().take());
        // A worker thread's panic re-raised here: its backtrace is in the shared list.
        let backtrace = own.or_else(|| {
            let mut list = ELSEWHERE.lock().ok()?;
            let at = list.iter().rposition(|(said, _)| *said == message)?;
            Some(list.remove(at).1)
        });
        CaughtPanic { message, backtrace }
    })
}

/// The text of a panic payload.
#[must_use]
pub fn message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn a_marked_panicking_frame() {
        panic!("marked panic");
    }

    #[inline(never)]
    fn a_marked_worker_frame() {
        panic!("marked worker panic");
    }

    const CHAIN_MARK: &str = "chained-hooks marker panic";

    #[inline(never)]
    fn a_chain_marker_frame() {
        panic!("chained-hooks marker panic");
    }

    /// The capture hook is installed and no editor installer changes the hook chain while the
    /// test panics (ADR-8847): `take_hook` leaves the default hook in place until the `set_hook`,
    /// so a panic in between runs without the capture hook. The crash-report test lock comes
    /// first, as for the other tests that panic on purpose: a crash-report test configures its
    /// report folder before it installs the hooks, so a panic here while it waited for the hook
    /// lock was written into its folder.
    fn steady_hooks() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        install_hook();
        let reports = crate::crash_reports::tests::lock();
        let hooks = HOOK_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        (reports, hooks)
    }

    #[test]
    fn a_caught_panic_carries_a_backtrace_through_the_panicking_frame() {
        let _hooks = steady_hooks();
        let caught = catch(a_marked_panicking_frame).expect_err("it panics");
        assert_eq!(caught.message, "marked panic");
        let trace = caught.backtrace.as_deref().expect("a backtrace");
        assert!(
            trace.contains("a_marked_panicking_frame"),
            "the panicking function is a frame:\n{trace}"
        );
        assert!(caught.log_text().starts_with("marked panic\nbacktrace:\n"));
    }

    #[test]
    fn a_panic_on_a_worker_thread_reaches_the_catching_thread_with_its_backtrace() {
        let _hooks = steady_hooks();
        let caught = catch(|| {
            let worker = std::thread::spawn(a_marked_worker_frame);
            if let Err(payload) = worker.join() {
                std::panic::resume_unwind(payload);
            }
        })
        .expect_err("re-raised");
        assert_eq!(caught.message, "marked worker panic");
        let trace = caught.backtrace.expect("the worker's backtrace");
        assert!(trace.contains("a_marked_worker_frame"), "{trace}");
    }

    #[test]
    fn no_panic_returns_the_value_and_the_flag_is_restored() {
        assert_eq!(catch(|| 7), Ok(7));
        assert!(!CAPTURING.with(Cell::get));
    }

    /// ADR-8847: installers that chain at the same moment keep every hook. Sixteen threads chain
    /// a counting hook at once (behind a barrier); one caught panic then runs all sixteen and the
    /// capture hook. Unlocked, two `take_hook` calls can return the same hook and the later
    /// `set_hook` drops the earlier one with everything it chained.
    #[test]
    fn hooks_chained_at_the_same_time_all_stay_in_the_chain() {
        const THREADS: usize = 16;
        install_hook();
        let counts: std::sync::Arc<Vec<AtomicUsize>> =
            std::sync::Arc::new((0..THREADS).map(|_| AtomicUsize::new(0)).collect());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let installers: Vec<_> = (0..THREADS)
            .map(|index| {
                let counts = std::sync::Arc::clone(&counts);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    chain_hook(move |previous| {
                        Box::new(move |info| {
                            if info.payload_as_str() == Some(CHAIN_MARK) {
                                counts[index].fetch_add(1, Ordering::SeqCst);
                            }
                            previous(info);
                        })
                    });
                })
            })
            .collect();
        for installer in installers {
            installer.join().expect("installer");
        }
        let _hooks = steady_hooks();
        let caught = catch(a_chain_marker_frame).expect_err("it panics");
        assert_eq!(caught.message, CHAIN_MARK);
        assert!(caught.backtrace.is_some(), "the capture hook is still chained");
        let ran: Vec<usize> = counts.iter().map(|c| c.load(Ordering::SeqCst)).collect();
        assert_eq!(ran, vec![1; THREADS], "every chained hook ran once");
    }
}
