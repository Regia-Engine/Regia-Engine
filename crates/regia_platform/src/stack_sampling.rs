// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/stack_dump.rs,
// commit d6ed37677, regia-absorb-1), ADR-8012; adapted under ADR-8156.
//! Best-effort, on-demand thread stacks for hang diagnostics (ADR-8156).
//!
//! The Linux GNU backend is adapted from DO27 `stack_dump.rs`, d6ed37677
//! (`regia-absorb-1`), under the owner's 2026-11-16 P-08 copy exception.
//! Install on a normal thread at startup. Capture on a watchdog, never in a panic
//! hook or fatal-signal handler. The unwinder is not guaranteed async-signal-safe;
//! a stuck walk keeps its buffer reserved until it completes. Other OSes return
//! [`std::io::ErrorKind::Unsupported`]. No background thread is created.

use std::time::Duration;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "platform/linux/stack_sampling.rs"]
mod imp;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod symbols;

/// The allocation-free handler's maximum supported stack depth.
pub const FRAME_CAPACITY: usize = 256;

/// Per-capture diagnostic budgets; none affect simulation or frame scheduling.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Frames retained per sampled thread (1 through [`FRAME_CAPACITY`]).
    pub max_frames: usize,
    /// Maximum threads sampled; other enumerated identities receive `LimitReached`.
    pub max_samples: usize,
    /// Time allowed for a thread to accept its signal.
    pub sample_wait: Duration,
    /// Additional time allowed for a handler that accepted the request to finish.
    pub walk_wait: Duration,
    /// Maximum bytes read from each ELF symbol or string table (zero skips names).
    pub max_symbol_bytes: usize,
    /// Maximum bytes scanned per ELF symbol name (zero skips names).
    pub max_symbol_name_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_frames: 96,
            max_samples: 1024,
            sample_wait: Duration::from_millis(250),
            walk_wait: Duration::from_secs(2),
            max_symbol_bytes: 64 * 1024 * 1024,
            max_symbol_name_bytes: 1024,
        }
    }
}

/// Outcome for a thread present in the task-directory snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleOutcome {
    /// Raw instruction pointers, in innermost-to-outermost order.
    Captured(Vec<usize>),
    /// The thread exited before its signal could be sent.
    Exited,
    /// No handler accepted the request before the configured timeout.
    NoResponse,
    /// The handler accepted the request but did not finish before the timeout.
    WalkTimedOut,
    /// A capture or an earlier unfinished handler owns the sample buffer.
    Busy,
    /// The configured sample-count limit was reached.
    LimitReached,
    /// Sending the sampling signal failed with this OS error number.
    SignalError(i32),
}

/// One thread, including failures; identities are never truncated or grouped away.
#[derive(Debug, Clone)]
pub struct ThreadSample {
    /// OS thread ID.
    pub id: u32,
    /// OS thread name, or `?` if it exited before the name could be read.
    pub name: String,
    /// Whether this is the thread calling [`capture`].
    pub is_caller: bool,
    /// Captured instruction pointers or the reason none are available.
    pub outcome: SampleOutcome,
    /// ELF link addresses and raw function names, or explicit file offsets when unavailable.
    pub stack: String,
}

/// A best-effort snapshot, not an atomic view of all running threads.
#[derive(Debug, Clone)]
pub struct StackDump {
    /// Every thread enumerated, ordered by OS thread ID. Threads born later are absent.
    pub threads: Vec<ThreadSample>,
}

impl StackDump {
    /// Renders all identities and outcomes. The crash store scrubs this before saving.
    pub fn text(&self) -> String {
        use std::fmt::Write;
        let mut out = format!("thread stacks: {} enumerated\n", self.threads.len());
        out.push_str("file:line offline: addr2line -C -f -i -e <module> <offset> on the exact build\n");
        for thread in &self.threads {
            let _ = writeln!(
                out,
                "\nthread {} {}{}: {:?}",
                thread.id,
                thread.name,
                if thread.is_caller { " [caller]" } else { "" },
                thread.outcome
            );
            out.push_str(&thread.stack);
        }
        out
    }
}

/// Installs the Linux sampling handler on `SIGRTMIN + 6` without replacing another handler.
/// Idempotent. The process must reserve that signal for the lifetime of this code.
/// Use [`install_with_signal`] if that signal belongs to a host library.
///
/// # Errors
/// Returns `Unsupported` off Linux GNU, or the OS/install error. Installation must
/// be coordinated with other process-wide signal users by the host at startup.
pub fn install() -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::install(libc::SIGRTMIN() + 6)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        Err(unsupported())
    }
}

/// Installs the sampler using a host-reserved real-time signal number.
/// Once installed, the signal cannot be changed or uninstalled; late queued signals
/// must always retain a handler. The host must not replace it or unload this code.
///
/// # Errors
/// Returns an error for non-real-time signals, conflicting handlers or OS failures.
pub fn install_with_signal(signal: i32) -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::install(signal)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = signal;
        Err(unsupported())
    }
}

/// Captures every currently enumerated thread, including the caller.
/// Concurrent captures return a snapshot of `Busy` outcomes instead of blocking.
/// Blocked signals and exited threads have explicit outcomes. Symbolization reads
/// mapped ELF files only after all walks, without executing external tools.
///
/// # Errors
/// Returns an error if not installed, settings are invalid or enumeration fails.
/// Off Linux GNU this returns `Unsupported`.
pub fn capture(settings: &Settings) -> std::io::Result<StackDump> {
    if settings.max_frames == 0
        || settings.max_frames > FRAME_CAPACITY
        || settings.sample_wait.is_zero()
        || settings.walk_wait.is_zero()
        || settings.sample_wait.checked_add(settings.walk_wait).is_none()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid stack sampling budgets",
        ));
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::capture(settings)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        Err(unsupported())
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "thread stack sampling requires Linux GNU",
    )
}

static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Requests one hang snapshot; repeated requests coalesce until [`take_request`].
/// This sets an atomic flag only. A host watchdog chooses when to capture/save.
pub fn request() {
    REQUESTED.store(true, std::sync::atomic::Ordering::Release);
}

/// Consumes the pending request flag. Never samples, allocates or blocks.
pub fn take_request() -> bool {
    REQUESTED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Opts into `SIGUSR1` requests on Linux GNU. The handler only calls [`request`].
/// No capture or file IO runs in the handler. Host code polls [`take_request`].
///
/// # Errors
/// Refuses an occupied signal, conflicting installation or unsupported platform.
pub fn install_request_handler() -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::install_request_signal(libc::SIGUSR1)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        Err(unsupported())
    }
}

/// Installs requests on a host-reserved SIGUSR1, SIGUSR2 or real-time signal.
/// Coordinate installation at startup; the handler and code must remain alive.
///
/// # Errors
/// Refuses other signals, an occupied signal or a change after installation.
pub fn install_request_signal(signal: i32) -> std::io::Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        imp::install_request_signal(signal)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = signal;
        Err(unsupported())
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn invalid_capture_budgets_are_rejected_before_os_work() {
        for settings in [
            Settings {
                max_frames: 0,
                ..Settings::default()
            },
            Settings {
                max_frames: FRAME_CAPACITY + 1,
                ..Settings::default()
            },
            Settings {
                sample_wait: Duration::ZERO,
                ..Settings::default()
            },
            Settings {
                walk_wait: Duration::ZERO,
                ..Settings::default()
            },
            Settings {
                sample_wait: Duration::MAX,
                ..Settings::default()
            },
        ] {
            assert_eq!(
                capture(&settings).expect_err("invalid settings").kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn unavailable_platform_does_not_claim_to_sample_threads() {
        assert_eq!(
            install().expect_err("unsupported").kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(
            capture(&Settings::default()).expect_err("unsupported").kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}
