// SPDX-License-Identifier: MPL-2.0
//! Unix (Linux, Android, macOS): shutdown and reload signals, and the admin token's mode bits.

use std::path::Path;

use crate::server_host::signals::HostSignals;

/// Registers SIGTERM and SIGINT (shutdown; the second one exits at once while draining) and SIGHUP
/// (reload) with `signal-hook`'s safe flag API (ADR-1663).
pub(crate) fn install_os_handlers(signals: &HostSignals) -> std::io::Result<Vec<&'static str>> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    // Registered first: a signal that arrives while draining exits at once.
    signal_hook::flag::register_conditional_shutdown(SIGTERM, 0, signals.draining.clone())?;
    signal_hook::flag::register_conditional_shutdown(SIGINT, 0, signals.draining.clone())?;
    signal_hook::flag::register(SIGTERM, signals.shutdown.clone())?;
    signal_hook::flag::register(SIGINT, signals.shutdown.clone())?;
    signal_hook::flag::register(SIGHUP, signals.reload.clone())?;
    Ok(vec!["SIGTERM", "SIGINT", "SIGHUP"])
}

/// False (with a warning) when group or other mode bits are set on the admin token file.
pub(crate) fn admin_token_file_private(path: &Path, meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Group bits too, as for `server.key`: a group member who reads the token has full
    // remote admin (audit v0.2.12).
    if meta.permissions().mode() & 0o077 != 0 {
        tracing::warn!(path = %path.display(), "server host: other users can access the admin token file (chmod 600 it); remote admin off");
        return false;
    }
    true
}
