// SPDX-License-Identifier: MPL-2.0
//! Platform seams of the agent's local transport (`docs/04-conventions.md` § 8): a Unix
//! domain socket on Linux/macOS (`unix.rs`) and a named pipe on Windows (`windows.rs`), the
//! two local transports of spec 23 § 2. Other targets (Android) have no local transport —
//! dev builds there reach the WebSocket listener through `adb forward` — and get the
//! no-op `other.rs`.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::{Guard, Identity, LocalStream, fallback_socket_dir, start};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{Guard, Identity, LocalStream, start};

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) use other::{Guard, Identity, LocalStream, start};

// The Windows pipe's pure rules (error-code classes, timeouts, the server-pid check, accept
// back-off), kept apart from the FFI so their unit tests run on every host.
#[cfg(any(windows, test))]
pub(crate) mod pipe_rules;
