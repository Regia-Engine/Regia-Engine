// SPDX-License-Identifier: MPL-2.0
//! Windows console control events for a console host (spec 76 § 5, ADR-1886): Ctrl+C, Ctrl+Break
//! and console close only set atomic flags that the host reads at the start of a tick.
//!
//! The first shutdown event requests a shutdown; one that arrives while the host is draining ends
//! the process at once with code 0, as a second SIGTERM does on Linux. A close or shutdown event
//! ends the process when its handler returns, so that handler waits (bounded by the time the OS
//! allows) until the host says it stopped. A logoff is ignored: it is sent for any user's logoff,
//! and a server running under a service wrapper must not stop because an admin left.
//!
//! [`ConsoleFlags`] and the event mapping are plain code, tested on every platform; only `install`
//! (Windows) touches the OS. Semantics from Microsoft's public `SetConsoleCtrlHandler` and
//! `HandlerRoutine` documentation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `CTRL_C_EVENT`.
const CTRL_C: u32 = 0;
/// `CTRL_BREAK_EVENT`.
const CTRL_BREAK: u32 = 1;
/// `CTRL_CLOSE_EVENT`.
const CTRL_CLOSE: u32 = 2;
/// `CTRL_LOGOFF_EVENT`.
const CTRL_LOGOFF: u32 = 5;
/// `CTRL_SHUTDOWN_EVENT`.
const CTRL_SHUTDOWN: u32 = 6;

/// How long a close or shutdown handler waits for the host: the OS ends the process about
/// 5 s after the event whatever the handler does.
pub const CLOSE_WAIT: Duration = Duration::from_millis(4500);

/// A console control event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+Break.
    CtrlBreak,
    /// The console window is closing.
    Close,
    /// The user is logging off.
    Logoff,
    /// The system is shutting down.
    Shutdown,
}

impl ConsoleEvent {
    /// The event for a `HandlerRoutine` control type; `None` for one this code does not know.
    #[must_use]
    pub fn from_ctrl_type(ctrl_type: u32) -> Option<Self> {
        match ctrl_type {
            CTRL_C => Some(Self::CtrlC),
            CTRL_BREAK => Some(Self::CtrlBreak),
            CTRL_CLOSE => Some(Self::Close),
            CTRL_LOGOFF => Some(Self::Logoff),
            CTRL_SHUTDOWN => Some(Self::Shutdown),
            _ => None,
        }
    }

    /// The OS ends the process when the handler of this event returns.
    #[must_use]
    pub fn ends_process(self) -> bool {
        matches!(self, Self::Close | Self::Shutdown)
    }
}

/// What the handler does after it set the flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleAction {
    /// Return from the handler; the host stops on its own.
    Return,
    /// Wait for [`ConsoleFlags::stopped`], then return (the OS ends the process on return).
    WaitForStop,
    /// End the process now with code 0.
    ExitNow,
}

/// The flags a console host shares with the handler. Cloning shares them.
#[derive(Debug, Clone, Default)]
pub struct ConsoleFlags {
    /// Set by a shutdown event; the host takes it at the start of a tick.
    pub shutdown: Arc<AtomicBool>,
    /// Set by the host once it is draining: from then on an event exits at once.
    pub draining: Arc<AtomicBool>,
    /// Set by the host once it has stopped and flushed its log.
    pub stopped: Arc<AtomicBool>,
}

impl ConsoleFlags {
    /// Records `event` and says what the handler does next.
    #[must_use]
    pub fn on_event(&self, event: ConsoleEvent) -> ConsoleAction {
        if event == ConsoleEvent::Logoff {
            return ConsoleAction::Return;
        }
        if self.draining.load(Ordering::SeqCst) && !event.ends_process() {
            return ConsoleAction::ExitNow;
        }
        self.shutdown.store(true, Ordering::SeqCst);
        if event.ends_process() {
            ConsoleAction::WaitForStop
        } else {
            ConsoleAction::Return
        }
    }

    /// Waits until the host stopped or `limit` passed; returns whether it stopped.
    #[must_use]
    pub fn wait_stopped(&self, limit: Duration) -> bool {
        let start = Instant::now();
        while !self.stopped.load(Ordering::SeqCst) {
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::Win32::System::Threading::ExitProcess;
    use windows_sys::core::BOOL;

    use super::{CLOSE_WAIT, ConsoleAction, ConsoleEvent, ConsoleFlags};

    static FLAGS: OnceLock<ConsoleFlags> = OnceLock::new();

    /// Runs on a thread the OS creates for the event, so it may wait; it touches atomics only.
    extern "system" fn handler(ctrl_type: u32) -> BOOL {
        let (Some(flags), Some(event)) = (FLAGS.get(), ConsoleEvent::from_ctrl_type(ctrl_type)) else {
            return FALSE;
        };
        match flags.on_event(event) {
            ConsoleAction::Return => {}
            ConsoleAction::WaitForStop => {
                let _ = flags.wait_stopped(CLOSE_WAIT);
            }
            // SAFETY: `ExitProcess` takes an exit code and does not return.
            ConsoleAction::ExitNow => unsafe { ExitProcess(0) },
        }
        TRUE
    }

    pub(super) fn install(flags: ConsoleFlags) -> std::io::Result<()> {
        if FLAGS.set(flags).is_err() {
            return Err(std::io::Error::other("console control handler already installed"));
        }
        // SAFETY: `handler` is a `'static` function with the `HandlerRoutine` signature, and it
        // stays valid for the life of the process.
        if unsafe { SetConsoleCtrlHandler(Some(handler), TRUE) } == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Registers the process's console control handler. Once per process; a second call fails.
///
/// # Errors
///
/// The OS refused, or a handler is already installed.
#[cfg(windows)]
pub fn install(flags: ConsoleFlags) -> std::io::Result<()> {
    imp::install(flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_types_map_to_events() {
        assert_eq!(ConsoleEvent::from_ctrl_type(0), Some(ConsoleEvent::CtrlC));
        assert_eq!(ConsoleEvent::from_ctrl_type(1), Some(ConsoleEvent::CtrlBreak));
        assert_eq!(ConsoleEvent::from_ctrl_type(2), Some(ConsoleEvent::Close));
        assert_eq!(ConsoleEvent::from_ctrl_type(5), Some(ConsoleEvent::Logoff));
        assert_eq!(ConsoleEvent::from_ctrl_type(6), Some(ConsoleEvent::Shutdown));
        assert_eq!(ConsoleEvent::from_ctrl_type(3), None);
    }

    #[test]
    fn first_ctrl_c_requests_shutdown_and_the_second_exits() {
        let flags = ConsoleFlags::default();
        assert_eq!(flags.on_event(ConsoleEvent::CtrlC), ConsoleAction::Return);
        assert!(flags.shutdown.load(Ordering::SeqCst));
        flags.draining.store(true, Ordering::SeqCst);
        assert_eq!(flags.on_event(ConsoleEvent::CtrlBreak), ConsoleAction::ExitNow);
    }

    #[test]
    fn a_logoff_does_not_stop_the_server() {
        let flags = ConsoleFlags::default();
        assert_eq!(flags.on_event(ConsoleEvent::Logoff), ConsoleAction::Return);
        assert!(!flags.shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn close_waits_for_the_host_even_while_draining() {
        let flags = ConsoleFlags::default();
        assert_eq!(flags.on_event(ConsoleEvent::Close), ConsoleAction::WaitForStop);
        assert!(flags.shutdown.load(Ordering::SeqCst));
        flags.draining.store(true, Ordering::SeqCst);
        assert_eq!(flags.on_event(ConsoleEvent::Close), ConsoleAction::WaitForStop);
        flags.stopped.store(true, Ordering::SeqCst);
        assert!(flags.wait_stopped(Duration::ZERO));
    }

    #[test]
    fn a_host_that_never_stops_does_not_hold_the_handler() {
        let flags = ConsoleFlags::default();
        assert!(!flags.wait_stopped(Duration::ZERO));
    }
}
