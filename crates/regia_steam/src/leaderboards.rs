// SPDX-License-Identifier: MPL-2.0
//! Steam leaderboards behind the framework's [`Leaderboard`] resource (spec 24 § 2a, ADR-2340).
//!
//! [`SteamLeaderboardPlugin`] adds `regia_framework`'s [`LeaderboardPlugin`] (when it is not there
//! yet) and a `PreUpdate` step that, once Steam is ready, gives [`Leaderboard`] the Steam service
//! (ISteamUserStats: find or create, upload keep-best or force, download global, around the user
//! or friends). When Steam goes away the resource falls back to no service, so calls fail with
//! `leaderboard_unavailable` instead of hanging. The game only ever uses [`Leaderboard`].

use std::sync::Arc;

use regia_app::prelude::*;
use regia_framework::leaderboard::{
    Leaderboard, LeaderboardPlugin, LeaderboardService, UnavailableLeaderboards, leaderboard_system,
};

use crate::backend::SteamState;
use crate::resource::Steam;

/// The leaderboard service of the running Steam client, or an unavailable one.
pub fn steam_leaderboards(steam: &Steam) -> Arc<dyn LeaderboardService> {
    if steam.state != SteamState::Ready {
        return Arc::new(UnavailableLeaderboards);
    }
    steam
        .backend
        .leaderboards()
        .unwrap_or_else(|| Arc::new(UnavailableLeaderboards))
}

/// Wires Steam leaderboards into the framework's [`Leaderboard`] resource. Add it after
/// [`crate::SteamPlugin`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SteamLeaderboardPlugin;

/// Whether the Steam service is attached to [`Leaderboard`].
#[derive(Resource, Debug, Default)]
struct SteamLeaderboardState {
    attached: bool,
}

fn attach_steam_leaderboards(
    steam: Option<Res<Steam>>,
    mut boards: ResMut<Leaderboard>,
    mut state: ResMut<SteamLeaderboardState>,
) {
    let ready = steam.as_ref().is_some_and(|s| s.state == SteamState::Ready);
    if !state.attached && ready {
        if let Some(service) = steam.as_ref().and_then(|s| s.backend.leaderboards()) {
            boards.set_service(service);
            state.attached = true;
            tracing::info!(target: "regia_steam", "Steam leaderboards attached");
        }
    } else if state.attached && !ready {
        boards.set_service(Arc::new(UnavailableLeaderboards));
        state.attached = false;
        tracing::info!(target: "regia_steam", "Steam leaderboards detached (Steam is gone)");
    }
}

impl Plugin for SteamLeaderboardPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<LeaderboardPlugin>()) {
            app.add_plugins(LeaderboardPlugin::default());
        }
        app.init_resource::<SteamLeaderboardState>();
        app.add_systems(
            PreUpdate,
            attach_steam_leaderboards
                .after(crate::pump::pump_steam)
                .before(leaderboard_system),
        );
    }
}
