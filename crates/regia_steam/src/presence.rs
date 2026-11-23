// SPDX-License-Identifier: MPL-2.0
//! Rich presence and invites (specs/24 § 2 "Friends, rich presence, invites"): the keys are set
//! from game state through [`crate::Steam::set_presence`]; a friend joining through the overlay or
//! the friends list arrives as [`JoinRequested`] with the game-defined connect string.

use regia_app::prelude::*;

use crate::backend::SteamId;

/// Longest `connect` string a join request may carry. A join token is a short address or code.
pub const MAX_CONNECT_BYTES: usize = 1024;

/// A friend joined the game through the overlay or the friends list (`GameRichPresenceJoinRequested`).
/// `connect` is the string the game set as the `connect` rich presence key; DO27 maps it to its
/// own connect flow (specs/24 § 2). It comes from the network, possibly from someone who is not a
/// friend, and one past [`MAX_CONNECT_BYTES`] is dropped: check it against what the game itself
/// would put there before dialing or parsing it.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct JoinRequested {
    /// The friend that joined; [`SteamId::INVALID`] when the join did not come from a friend.
    pub friend: SteamId,
    /// The game-defined connect string to hand to the game's join flow.
    pub connect: String,
}

/// An achievement was stored to Steam's servers: raised for manual `unlock` calls and for the
/// threshold auto-unlock in [`crate::Steam::stat_add`].
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct AchievementUnlocked {
    /// The achievement's Steam API name (matches `config/achievements.ron`).
    pub id: String,
}

/// A `store_stats` upload finished. `Ok(())` means Steam accepted every changed stat and
/// achievement.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct StatsStored {
    /// The upload result.
    pub result: Result<(), String>,
}

/// The keys Steam itself displays in the friends list; games may add their own (all values are
/// game-defined strings).
pub mod presence_keys {
    /// What the player is doing right now ("In the menus", "Chapter 2").
    pub const STATUS: &str = "status";
    /// The join token: what a friend's [`crate::JoinRequested`] carries back as `connect`
    /// (specs/24 § 2).
    pub const CONNECT: &str = "connect";
    /// Shown by Steam's enhanced rich presence instead of `status` when the game supports it.
    pub const STEAM_DISPLAY: &str = "steam_display";
}

#[cfg(test)]
mod tests {
    use super::presence_keys;

    #[test]
    /// Steam's enhanced rich presence localizes `# TOKEN_EN` strings from the app's localization
    /// file; the token prefix is the documented convention.
    fn key_names_match_steam_documentation() {
        assert_eq!(presence_keys::STATUS, "status");
        assert_eq!(presence_keys::CONNECT, "connect");
        assert_eq!(presence_keys::STEAM_DISPLAY, "steam_display");
    }
}
