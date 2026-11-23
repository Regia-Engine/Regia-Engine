// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/modes/mod.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 17-66 and the `mode_tests` module, 209-244). Named issues: the game's own
// citadel mode left the enum (a game keeps its own modes and parses its strings first; anything the
// skeletons do not score is `Custom`), and the variants carry engine names.

//! The match modes the rules skeletons know, parsed from lobby, config and playlist strings.

use serde::{Deserialize, Serialize};

/// A match mode selector (lobby, config and [`super::playlist::PlaylistEntry`] mode
/// strings).
///
/// The skeletons score [`MatchMode::CaptureTheFlag`] and [`MatchMode::KingOfTheHill`]; the two
/// deathmatch modes are scored by the game's own kill rules. A game with modes of its own parses its
/// strings first and falls back to [`MatchMode::from_lobby_str`]; a mode the skeletons do not score
/// runs as [`MatchMode::Custom`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Team kill limit (`"tdm"`).
    #[default]
    TeamDeathmatch,
    /// Free-for-all kill limit beside team deathmatch (`"ffa"`): every player is its own team.
    FreeForAll,
    /// Capture the flag (`"ctf"`).
    CaptureTheFlag,
    /// King of the hill (`"koth"`).
    KingOfTheHill,
    /// A game-defined mode the skeletons do not score (`"custom"`); the playlist entry's mode string
    /// names it.
    Custom,
    /// Domination (`"dom"`): capture zones whose owners score ([`crate::zones`], ADR-8649). The
    /// zones and their [`ZoneMode`](crate::zones::ZoneMode) (domination, hardpoint or attack/defend)
    /// are map data the host installs.
    Domination,
}

impl MatchMode {
    /// Every mode, in declaration order.
    pub const ALL: [MatchMode; 6] = [
        MatchMode::TeamDeathmatch,
        MatchMode::FreeForAll,
        MatchMode::CaptureTheFlag,
        MatchMode::KingOfTheHill,
        MatchMode::Custom,
        MatchMode::Domination,
    ];

    /// Parse a lobby, config or playlist mode string, ignoring ASCII case. Unknown values yield
    /// `None`. `"deathmatch"` stays team deathmatch (config back-compat).
    pub fn from_lobby_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "tdm" | "deathmatch" | "team_deathmatch" => Some(Self::TeamDeathmatch),
            "ffa" | "free-for-all" | "free_for_all" | "freeforall" => Some(Self::FreeForAll),
            "ctf" | "capture_the_flag" => Some(Self::CaptureTheFlag),
            "koth" | "king_of_the_hill" => Some(Self::KingOfTheHill),
            "custom" => Some(Self::Custom),
            "dom" | "domination" => Some(Self::Domination),
            _ => None,
        }
    }

    /// Short id for lobby strings, config and logs (`"tdm"`, `"ffa"`, `"ctf"`, `"koth"`, `"dom"`,
    /// `"custom"`).
    pub fn as_lobby_str(self) -> &'static str {
        match self {
            Self::TeamDeathmatch => "tdm",
            Self::FreeForAll => "ffa",
            Self::CaptureTheFlag => "ctf",
            Self::KingOfTheHill => "koth",
            Self::Domination => "dom",
            Self::Custom => "custom",
        }
    }

    /// True for team deathmatch and free-for-all: the modes that share maps, spawns, pickups and
    /// kill-limit scoring.
    pub fn is_deathmatch_family(self) -> bool {
        matches!(self, Self::TeamDeathmatch | Self::FreeForAll)
    }

    /// True for the modes whose score comes from objectives ([`MatchMode::CaptureTheFlag`],
    /// [`MatchMode::KingOfTheHill`] and [`MatchMode::Domination`]).
    pub fn has_objectives(self) -> bool {
        matches!(
            self,
            Self::CaptureTheFlag | Self::KingOfTheHill | Self::Domination
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffa_parses_beside_tdm_and_deathmatch_stays_team() {
        assert_eq!(MatchMode::from_lobby_str("ffa"), Some(MatchMode::FreeForAll));
        assert_eq!(
            MatchMode::from_lobby_str("free-for-all"),
            Some(MatchMode::FreeForAll)
        );
        assert_eq!(
            MatchMode::from_lobby_str("free_for_all"),
            Some(MatchMode::FreeForAll)
        );
        assert_eq!(MatchMode::from_lobby_str("FFA"), Some(MatchMode::FreeForAll));
        // Config back-compat: "deathmatch" is still team deathmatch.
        assert_eq!(
            MatchMode::from_lobby_str("deathmatch"),
            Some(MatchMode::TeamDeathmatch)
        );
        assert_eq!(MatchMode::from_lobby_str("tdm"), Some(MatchMode::TeamDeathmatch));
        assert_eq!(MatchMode::from_lobby_str("tug"), None);
    }

    #[test]
    fn lobby_ids_round_trip_and_family_covers_both_dm_modes() {
        assert_eq!(MatchMode::FreeForAll.as_lobby_str(), "ffa");
        assert_eq!(MatchMode::TeamDeathmatch.as_lobby_str(), "tdm");
        for mode in MatchMode::ALL {
            assert_eq!(MatchMode::from_lobby_str(mode.as_lobby_str()), Some(mode));
        }
        assert!(MatchMode::TeamDeathmatch.is_deathmatch_family());
        assert!(MatchMode::FreeForAll.is_deathmatch_family());
        assert!(!MatchMode::CaptureTheFlag.is_deathmatch_family());
        assert!(!MatchMode::KingOfTheHill.is_deathmatch_family());
        assert!(!MatchMode::Custom.is_deathmatch_family());
        assert!(MatchMode::CaptureTheFlag.has_objectives());
        assert!(MatchMode::KingOfTheHill.has_objectives());
        assert!(!MatchMode::FreeForAll.has_objectives());
        assert!(MatchMode::Domination.has_objectives());
        assert_eq!(
            MatchMode::from_lobby_str("domination"),
            Some(MatchMode::Domination)
        );
        assert_eq!(MatchMode::Domination.as_lobby_str(), "dom");
    }
}
