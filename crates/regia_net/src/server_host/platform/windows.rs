// SPDX-License-Identifier: MPL-2.0
//! Windows: the native crash exit, console control events and the admin token's DACL.

use std::path::Path;

use crate::server_host::crash::{CrashCause, crash_exit_code};
use crate::server_host::signals::HostSignals;

/// Attaches `regia_platform::native_crash` with a callback that ends the process with code 70 by
/// `TerminateProcess`.
pub(crate) fn install_native_crash_exit() -> Result<(), String> {
    // The callback runs in the exception filter on the crashing thread, in a process in an
    // unknown state, so it must end the process without exit handlers or allocation —
    // TerminateProcess, the deliberate-exit path the platform backend already uses. Without
    // this the process status would be the NTSTATUS (e.g. 0xC0000005), which a supervisor
    // matching 70 never restarts on (spec 76 § 5).
    #[allow(unsafe_code)]
    fn terminate_with_crash_code() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: terminates the current process with the crash exit code; takes no
        // pointers and runs nothing else.
        unsafe {
            TerminateProcess(
                GetCurrentProcess(),
                crash_exit_code(CrashCause::WindowsException(0)).code() as u32,
            )
        };
    }
    regia_platform::native_crash::attach(|_context| terminate_with_crash_code(), None, 5)
        .map_err(|e| e.to_string())
}

/// Registers Ctrl+C, Ctrl+Break and console close as shutdown requests through
/// `regia_platform::console_ctrl` (ADR-1886). Reload is the `reload` console command here.
pub(crate) fn install_os_handlers(signals: &HostSignals) -> std::io::Result<Vec<&'static str>> {
    regia_platform::console_ctrl::install(regia_platform::console_ctrl::ConsoleFlags {
        shutdown: signals.shutdown.clone(),
        draining: signals.draining.clone(),
        stopped: signals.stopped.clone(),
    })?;
    Ok(vec!["CTRL_C", "CTRL_BREAK", "CTRL_CLOSE"])
}

/// False (with a warning) when the admin token file's DACL grants any access to a principal other
/// than the owner, SYSTEM and Administrators.
pub(crate) fn admin_token_file_private(path: &Path, _meta: &std::fs::Metadata) -> bool {
    // Same wall as for `server.key` (audit net1, platform): a token any principal other
    // than the owner, SYSTEM and Administrators can read or write is full remote admin for
    // every local user (a writable token is replaced with a known one). A check that
    // cannot run fails closed (remote admin off).
    if crate::windows_acl::exposed(path).unwrap_or(true) {
        tracing::warn!(path = %path.display(), "server host: other users can access the admin token file; remote admin off");
        return false;
    }
    true
}
