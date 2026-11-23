// SPDX-License-Identifier: MPL-2.0
//! Socket options per operating system (spec 120 § 4.4, ADR-8407; `docs/04-conventions.md` § 8):
//! buffer sizes, the DSCP mark and don't-fragment (ADR-8893) of a UDP socket. Each backend exposes
//! the same functions; one without the calls reports `Unsupported`, and the transport carries on
//! with the defaults.

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(super) use unix::{
    is_size_refusal, read_buffer, read_dont_fragment, set_buffer, set_dont_fragment, set_dscp,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::{
    is_size_refusal, read_buffer, read_dont_fragment, set_buffer, set_dont_fragment, set_dscp,
};

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
pub(super) use other::{
    is_size_refusal, read_buffer, read_dont_fragment, set_buffer, set_dont_fragment, set_dscp,
};

/// Which buffer of a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Buffer {
    /// `SO_RCVBUF`.
    Receive,
    /// `SO_SNDBUF`.
    Send,
}
