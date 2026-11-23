// SPDX-License-Identifier: MPL-2.0
//! Targets that are neither Unix nor Windows: no OS shutdown events and no file access check.

use std::path::Path;

use crate::server_host::signals::HostSignals;

/// Registers nothing: shutdown and reload are the `quit` and `reload` console commands here.
pub(crate) fn install_os_handlers(_signals: &HostSignals) -> std::io::Result<Vec<&'static str>> {
    tracing::warn!("server host: no OS shutdown events on this platform; use `quit` and `reload`");
    Ok(Vec::new())
}

/// No access check exists here: the token file is used as it is.
pub(crate) fn admin_token_file_private(_path: &Path, _meta: &std::fs::Metadata) -> bool {
    true
}
