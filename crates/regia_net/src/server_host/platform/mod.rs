// SPDX-License-Identifier: MPL-2.0
//! The server host's per-OS code behind one seam (`docs/04-conventions.md` § 8, rc1-rev-net-k 12).
//!
//! Every target provides the same three `pub(super)` functions, which the shared files call:
//!
//! - `install_native_crash_exit`: attaches the native crash handler that ends the process with
//!   code 70 (`crash.rs`). Linux (`linux.rs`) and Windows (`windows.rs`); other targets refuse
//!   (`crash_unsupported.rs`).
//! - `install_os_handlers`: registers the OS shutdown and reload events on a `HostSignals`
//!   (`signals.rs`). Unix signals (`unix.rs`), Windows console control events (`windows.rs`), none
//!   elsewhere (`other.rs`).
//! - `admin_token_file_private`: whether the admin token file is closed to other users, warning
//!   when it is not (`mod.rs`). Unix mode bits (`unix.rs`), the Windows DACL (`windows.rs`), no check
//!   elsewhere (`other.rs`).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(super) use unix::{admin_token_file_private, install_os_handlers};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) use linux::install_native_crash_exit;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::{admin_token_file_private, install_native_crash_exit, install_os_handlers};

#[cfg(not(any(unix, windows)))]
mod other;
#[cfg(not(any(unix, windows)))]
pub(super) use other::{admin_token_file_private, install_os_handlers};

#[cfg(not(any(target_os = "linux", windows)))]
mod crash_unsupported;
#[cfg(not(any(target_os = "linux", windows)))]
pub(super) use crash_unsupported::install_native_crash_exit;
