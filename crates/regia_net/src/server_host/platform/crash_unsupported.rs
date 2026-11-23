// SPDX-License-Identifier: MPL-2.0
//! Targets without a native crash exit (Android, macOS and the rest): the panic hook alone ends a
//! server with 70.

/// Refuses: this target has no native crash exit yet.
pub(crate) fn install_native_crash_exit() -> Result<(), String> {
    Err("native crash exit code 70 is installed on Linux and Windows servers only".to_owned())
}
