// SPDX-License-Identifier: MPL-2.0
//! The `lobby` section of `config/net.ron` (spec 89 § 6). It sits outside the ECS lobby plugin so
//! `NetSettings` and the `--no-default-features` core build carry it (spec 120 § 3.2, ADR-8032).

use serde::{Deserialize, Serialize};

use super::LobbyVisibility;
use super::query::Distance;

/// The `lobby` section of `config/net.ron` (spec 89 § 6). Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LobbySettings {
    /// Member limit of lobbies the engine creates.
    pub max_members: u32,
    /// Visibility of lobbies the engine creates.
    pub visibility: LobbyVisibility,
    /// Members (owner included) needed before launch.
    pub min_players: u32,
    /// Also require the same content hash, not only the same protocol.
    pub match_content: bool,
    /// Members connect the `NetSession` when the owner launches.
    pub auto_connect: bool,
    /// Keep the lobby joinable after launch.
    pub joinable_in_game: bool,
    /// Matchmaking rounds, widening the distance.
    pub rounds: Vec<Distance>,
    /// Results per list round, `1..=50`.
    pub results_per_round: u32,
    /// Frames to wait for a platform answer.
    pub step_timeout: u32,
    /// Create a lobby when matchmaking finds none.
    pub create_if_none: bool,
    /// The Steam sockets channel the session uses.
    pub steam_channel: u32,
}

impl Default for LobbySettings {
    fn default() -> Self {
        Self {
            max_members: 8,
            visibility: LobbyVisibility::Public,
            min_players: 2,
            match_content: true,
            auto_connect: true,
            joinable_in_game: false,
            rounds: vec![
                Distance::Close,
                Distance::Default,
                Distance::Far,
                Distance::Worldwide,
            ],
            results_per_round: 50,
            step_timeout: 600,
            create_if_none: true,
            steam_channel: 0,
        }
    }
}
