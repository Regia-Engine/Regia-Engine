// SPDX-License-Identifier: MPL-2.0
//! Crash exit codes of the dedicated server (spec 76 § 5, ADR-2440): whatever crashed, a server
//! ends with code 70, so a supervisor tells a crash from a configuration (2), port (3) or content
//! (4) failure and restarts it.
//!
//! A panic reaches 70 through the panic hook `regia::server` installs. A native fault (SIGSEGV,
//! SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGTRAP: a crash in C code or a driver, a stack overflow)
//! would otherwise end the process by the signal, which a shell reports as `128 + signal`.
//! [`install_native_crash_exit`] attaches the platform's native crash handler
//! (`regia_platform::native_crash`) with a callback that ends the process with code 70 at once:
//! `_exit` on Linux (async-signal-safe: no allocation, no lock, no exit handlers) and
//! `TerminateProcess` on Windows (the deliberate-exit path the platform backend already uses).
//! The per-OS code is in `server_host/platform/` (`docs/04-conventions.md` § 8).

use super::ExitCode;

/// What ended a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashCause {
    /// A Rust panic.
    Panic,
    /// A native fault signal (Unix signal number).
    NativeSignal(i32),
    /// A Windows structured exception (its code).
    WindowsException(u32),
}

/// The exit code of a crash: always [`ExitCode::Crash`] (70), whatever the cause, so supervisors
/// match one number.
pub fn crash_exit_code(cause: CrashCause) -> ExitCode {
    match cause {
        CrashCause::Panic | CrashCause::NativeSignal(_) | CrashCause::WindowsException(_) => ExitCode::Crash,
    }
}

/// Attaches the native crash handler that ends the process with code 70 (Linux and Windows).
/// Call it once, early in a server process; an app that already attached a handler (the feedback
/// crash monitor) keeps its own, and this returns `Err`.
///
/// # Errors
///
/// A handler is already attached, it could not be installed, or this target has none (macOS and
/// Android dedicated or listen processes keep the panic-only behaviour for now).
pub fn install_native_crash_exit() -> Result<(), String> {
    super::platform::install_native_crash_exit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_crash_maps_to_exit_70() {
        // SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGTRAP on Linux; an access violation and a
        // stack overflow on Windows.
        let causes = [
            CrashCause::Panic,
            CrashCause::NativeSignal(11),
            CrashCause::NativeSignal(7),
            CrashCause::NativeSignal(4),
            CrashCause::NativeSignal(8),
            CrashCause::NativeSignal(6),
            CrashCause::NativeSignal(5),
            CrashCause::WindowsException(0xC000_0005),
            CrashCause::WindowsException(0xC000_00FD),
        ];
        for cause in causes {
            assert_eq!(crash_exit_code(cause), ExitCode::Crash, "{cause:?}");
            assert_eq!(crash_exit_code(cause).code(), 70, "{cause:?}");
        }
        // Distinct from every other documented code.
        for other in [ExitCode::Ok, ExitCode::Config, ExitCode::Bind, ExitCode::Content] {
            assert_ne!(other.code(), 70);
        }
    }

    /// The child side of `a_native_fault_exits_70`: only runs with the variable set.
    #[cfg(any(target_os = "linux", windows))]
    const CHILD_ENV: &str = "REGIA_TEST_NATIVE_CRASH_CHILD";

    // Ignored: on its own it has nothing to check, and a plain run would count it as a pass
    // (rc1-rev-net-k 13). `a_native_fault_exits_70` runs it with `--ignored`.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    #[ignore = "the child process of a_native_fault_exits_70, which runs it"]
    fn native_crash_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        install_native_crash_exit().expect("attach");
        #[cfg(target_os = "linux")]
        {
            let _ = signal_hook::low_level::raise(signal_hook::consts::SIGSEGV);
        }
        #[cfg(windows)]
        {
            // An access violation in native code: the exception filter must end the process
            // with 70, not with 0xC0000005. A null `write` may be caught by the debug
            // UB-precondition panic instead of faulting, so fault through an invalid address
            // like `xtask/tests/native_crash.rs` does.
            #[allow(unsafe_code)]
            // SAFETY: writes to the invalid address 0x10, which faults at once; that is the
            // point of this child process (the attached handler must end it before this
            // returns).
            unsafe {
                std::ptr::write_volatile(std::ptr::without_provenance_mut::<u8>(0x10), 1);
            }
        }
        // Not reached: the handler ends the process with 70.
        std::process::exit(1);
    }

    /// A SIGSEGV in a process with the handler ends it with exit code 70 (not by the signal).
    /// Runs this test binary again as the child; no clock, no port.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_native_fault_exits_70() {
        use std::os::unix::process::ExitStatusExt;
        let exe = std::env::current_exe().expect("test binary");
        let status = std::process::Command::new(exe)
            .args([
                "server_host::crash::tests::native_crash_child",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("child runs");
        assert_eq!(status.signal(), None, "ended by a signal: {status:?}");
        assert_eq!(status.code(), Some(70), "{status:?}");
    }

    /// The Windows twin: an access violation in a process with the handler ends it with exit
    /// code 70 (not the NTSTATUS). Runs this test binary again as the child; no clock, no port.
    #[cfg(windows)]
    #[test]
    fn a_native_fault_exits_70() {
        let exe = std::env::current_exe().expect("test binary");
        let status = std::process::Command::new(exe)
            .args([
                "server_host::crash::tests::native_crash_child",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("child runs");
        assert_eq!(status.code(), Some(70), "{status:?}");
    }
}
