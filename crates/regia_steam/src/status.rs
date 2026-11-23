// SPDX-License-Identifier: MPL-2.0
//! [`SteamStatus`]: a plain-data snapshot of the Steam connection for UI, telemetry and the
//! agent protocol (`steam.status`). The plugin refreshes it every frame after the callbacks run.

use regia_app::prelude::*;

use crate::backend::{AppId, SteamBackend, SteamId, SteamState};

/// The shared Steam connection, as plain data. Inserted by [`crate::SteamPlugin`]; read it as
/// `Res<SteamStatus>`. All fields are `false`/`None` when Steam is unavailable.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamStatus {
    /// The Steam client is connected to this process (`SteamState::Ready`).
    pub running: bool,
    /// The Steam client runs in offline mode: connected, but not logged on to Steam's servers.
    /// Achievements and stats queue locally; cloud saves do not sync.
    pub offline: bool,
    /// The local user's Steam id, when signed in.
    pub user_id: Option<SteamId>,
    /// The app id the backend initialized with, when known.
    pub app_id: Option<AppId>,
    /// The Steam overlay is open (mirrors [`crate::OverlayActive`]); games pause input on this.
    pub overlay_open: bool,
}

impl SteamStatus {
    /// Reads the current status from `backend`. `overlay_open` is carried over from the pump,
    /// which owns it.
    pub(crate) fn read(backend: &dyn SteamBackend, state: SteamState, overlay_open: bool) -> Self {
        let running = state == SteamState::Ready;
        Self {
            running,
            offline: running && !backend.logged_on(),
            user_id: if running { backend.steam_id() } else { None },
            app_id: if running { backend.app_id() } else { None },
            overlay_open: running && overlay_open,
        }
    }
}
