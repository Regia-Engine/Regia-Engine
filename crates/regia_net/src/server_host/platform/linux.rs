// SPDX-License-Identifier: MPL-2.0
//! Linux: the native crash exit (Unix signals and the admin token check are in `unix.rs`).

use crate::server_host::crash::{CrashCause, crash_exit_code};

/// Attaches `regia_platform::native_crash` with a callback that ends the process with code 70 by
/// `_exit` (async-signal-safe: no allocation, no lock, no exit handlers).
pub(crate) fn install_native_crash_exit() -> Result<(), String> {
    regia_platform::native_crash::attach(
        |_context| signal_hook::low_level::exit(crash_exit_code(CrashCause::NativeSignal(0)).code()),
        None,
        5,
    )
    .map_err(|e| e.to_string())
}
