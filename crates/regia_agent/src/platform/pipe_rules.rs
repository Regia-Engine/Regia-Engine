// SPDX-License-Identifier: MPL-2.0
//! The Windows named pipe's decisions, apart from the FFI in `windows.rs` so they are unit
//! tested on every host (sprint-rev-kimi-9 findings 1, 2, 4 and 5; ADR-5878): what a
//! `ConnectNamedPipe` result means for the accept loop, which read errors are a clean close,
//! how a read timeout becomes a wait in milliseconds, the server-process check a client runs
//! before it sends the token, and the accept loop's back-off.
//!
//! The Win32 error codes are mirrored here as plain numbers (Microsoft Learn, "System Error
//! Codes"); `windows.rs` asserts at compile time that they equal the `windows-sys` constants.

use std::io;
use std::time::Duration;

/// `ERROR_BROKEN_PIPE`: the other end closed its handle.
pub(crate) const ERROR_BROKEN_PIPE: i32 = 109;
/// `ERROR_NO_DATA`: the pipe is being closed — on `ConnectNamedPipe`, a client connected and
/// closed again before the server's call.
pub(crate) const ERROR_NO_DATA: i32 = 232;
/// `ERROR_PIPE_NOT_CONNECTED`: the server disconnected the instance.
pub(crate) const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
/// `ERROR_PIPE_CONNECTED`: a client connected between `CreateNamedPipeW` and
/// `ConnectNamedPipe`; the connection is good.
pub(crate) const ERROR_PIPE_CONNECTED: i32 = 535;
/// `INFINITE` for `WaitForSingleObject`.
pub(crate) const INFINITE_MS: u32 = u32::MAX;

/// What the accept loop does after `ConnectNamedPipe` on its pending instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectStep {
    /// A client is connected: hand the instance to a connection thread.
    Serve,
    /// The client already left (`ERROR_NO_DATA`): disconnect the instance and wait on it
    /// again. Before the fix this ended the accept loop for the rest of the session.
    Reuse,
    /// Any other failure: replace the instance (with back-off) and keep accepting.
    Replace,
}

/// Classifies a `ConnectNamedPipe` outcome: `None` is success, `Some(code)` the Win32 error.
pub(crate) fn connect_step(error: Option<i32>) -> ConnectStep {
    match error {
        None | Some(ERROR_PIPE_CONNECTED) => ConnectStep::Serve,
        Some(ERROR_NO_DATA) => ConnectStep::Reuse,
        Some(_) => ConnectStep::Replace,
    }
}

/// Whether a failed pipe read is the peer's clean close (read as EOF, `Ok(0)`).
pub(crate) fn read_error_is_eof(code: Option<i32>) -> bool {
    matches!(code, Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED))
}

/// Refuses a zero read timeout, as `std`'s sockets do (`set_read_timeout(Some(0))` is
/// `InvalidInput`); `None` and any positive duration are fine.
pub(crate) fn check_read_timeout(timeout: Option<Duration>) -> io::Result<()> {
    if timeout == Some(Duration::ZERO) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot set a 0 duration timeout",
        ));
    }
    Ok(())
}

/// The `WaitForSingleObject` wait for a read timeout: `None` waits forever; a finite timeout
/// rounds sub-millisecond parts up (a 1 µs timeout must not become a zero-wait poll) and stays
/// below `INFINITE` however long it is.
pub(crate) fn timeout_millis(timeout: Option<Duration>) -> u32 {
    let Some(timeout) = timeout else {
        return INFINITE_MS;
    };
    let millis = timeout.as_nanos().div_ceil(1_000_000);
    u32::try_from(millis).map_or(INFINITE_MS - 1, |ms| ms.min(INFINITE_MS - 1))
}

/// The client's check before it sends the bearer token: the pipe's server process must be the
/// one the pipe name carries. A pipe name is predictable, so another account could create it
/// first; its server would be another process (sprint-rev-kimi-9 finding 4).
pub(crate) fn check_server_pid(expected: u32, actual: u32) -> io::Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the pipe is served by process {actual}, not {expected}: the token is not sent"),
    ))
}

/// How long the accept loop pauses after `failures` consecutive failures: none for the first
/// three (a client racing the instance is routine), then doubling from 50 ms up to 5 s, so a
/// persistent fault never spins a core and never ends the transport.
pub(crate) fn accept_backoff(failures: u32) -> Option<Duration> {
    if failures <= 3 {
        return None;
    }
    let doublings = (failures - 4).min(7);
    Some(Duration::from_millis(50 << doublings).min(Duration::from_secs(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sprint-rev-kimi-9 finding 5: a client that connected and closed before
    /// `ConnectNamedPipe` (`ERROR_NO_DATA`) and any other connect failure keep the accept loop
    /// running; only success and `ERROR_PIPE_CONNECTED` serve.
    #[test]
    fn connect_outcomes_never_end_the_accept_loop() {
        assert_eq!(connect_step(None), ConnectStep::Serve);
        assert_eq!(connect_step(Some(ERROR_PIPE_CONNECTED)), ConnectStep::Serve);
        assert_eq!(connect_step(Some(ERROR_NO_DATA)), ConnectStep::Reuse);
        // ERROR_OPERATION_ABORTED, ERROR_INVALID_HANDLE, ERROR_BROKEN_PIPE, anything else.
        for code in [995, 6, ERROR_BROKEN_PIPE, 0, -1, i32::MAX] {
            assert_eq!(connect_step(Some(code)), ConnectStep::Replace, "{code}");
        }
    }

    /// A peer's close reads as EOF; other read errors stay errors.
    #[test]
    fn only_a_closed_pipe_reads_as_eof() {
        assert!(read_error_is_eof(Some(ERROR_BROKEN_PIPE)));
        assert!(read_error_is_eof(Some(ERROR_PIPE_NOT_CONNECTED)));
        for code in [None, Some(ERROR_NO_DATA), Some(995), Some(5)] {
            assert!(!read_error_is_eof(code), "{code:?}");
        }
    }

    /// sprint-rev-kimi-9 findings 1 and 2: a pipe takes a read timeout (it used to refuse
    /// every `Some`), with `std`'s rule for zero, and maps it to a bounded wait.
    #[test]
    fn read_timeouts_map_to_bounded_waits() {
        assert!(check_read_timeout(None).is_ok());
        assert!(check_read_timeout(Some(Duration::from_millis(750))).is_ok());
        assert!(check_read_timeout(Some(Duration::from_secs(10))).is_ok());
        let zero = check_read_timeout(Some(Duration::ZERO)).expect_err("zero is refused");
        assert_eq!(zero.kind(), io::ErrorKind::InvalidInput);

        assert_eq!(timeout_millis(None), INFINITE_MS);
        assert_eq!(timeout_millis(Some(Duration::from_millis(750))), 750);
        assert_eq!(timeout_millis(Some(Duration::from_secs(10))), 10_000);
        assert_eq!(timeout_millis(Some(Duration::from_micros(1))), 1, "rounds up");
        assert_eq!(timeout_millis(Some(Duration::from_micros(1_500))), 2, "rounds up");
        assert_eq!(
            timeout_millis(Some(Duration::from_secs(u64::MAX))),
            INFINITE_MS - 1,
            "a finite timeout never means INFINITE"
        );
    }

    /// sprint-rev-kimi-9 finding 4: a squatted pipe (another server process) is refused
    /// before the token goes out.
    #[test]
    fn a_pipe_served_by_another_process_is_refused() {
        assert!(check_server_pid(4242, 4242).is_ok());
        let err = check_server_pid(4242, 999).expect_err("another process");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("999"), "{err}");
    }

    /// The back-off starts after three failures, doubles, and is capped at 5 s.
    #[test]
    fn accept_backoff_grows_and_is_capped() {
        for n in 0..=3 {
            assert_eq!(accept_backoff(n), None, "{n}");
        }
        assert_eq!(accept_backoff(4), Some(Duration::from_millis(50)));
        assert_eq!(accept_backoff(5), Some(Duration::from_millis(100)));
        assert_eq!(accept_backoff(10), Some(Duration::from_millis(3_200)));
        assert_eq!(accept_backoff(11), Some(Duration::from_secs(5)));
        assert_eq!(accept_backoff(u32::MAX), Some(Duration::from_secs(5)));
    }
}
