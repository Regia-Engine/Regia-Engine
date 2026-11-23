// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/spectator.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: only the vision rules are copied (sees_team, out of lives); the
// camera half (orbit, tactical clamps, follow, menu keys) belongs to spectator modes and is not
// duplicated; lives replace the source's clone count; a finished match and a host grant feed
// regia_framework::fog_view through reveals_every_team without changing that filter's default.

//! Spectator vision.
//!
//! A team seat sees its own team, its allies and neutrals. A finished match reveals every team.
//! A neutral seat sees fighting teams only when the host grants full vision; neutrals (team 0)
//! are always shown. A seat with no lives left becomes a team spectator and keeps that team's
//! vision. Camera modes are not in this module.
//!
//! `reveals_every_team` is what `regia_framework::fog_view` consults when the game asks the
//! per-viewer filter to show every entity. Leaving that flag off keeps the team filter.

use serde::{Deserialize, Serialize};

/// Who is looking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeatKind {
    /// A seat on a fighting team, including a seat that is spectating that team.
    Team {
        /// The team.
        team: u8,
    },
    /// A seat on no fighting team.
    Neutral,
}

/// Whether `other_team` is shown to `viewer`.
///
/// Team 0 is always shown. A finished match reveals everything. A team seat sees its own team
/// and its allies while the match is live. A neutral seat sees fighting teams only when
/// `host_full_vision` is set.
pub fn sees_team(
    viewer: SeatKind,
    other_team: u8,
    allied: impl Fn(u8, u8) -> bool,
    match_over: bool,
    host_full_vision: bool,
) -> bool {
    if other_team == 0 {
        return true;
    }
    if match_over {
        return true;
    }
    match viewer {
        SeatKind::Team { team } => other_team == team || allied(team, other_team),
        SeatKind::Neutral => host_full_vision,
    }
}

/// A seat with no lives left spectates instead of respawning.
pub fn is_out_of_lives(lives_left: u32) -> bool {
    lives_left == 0
}

/// Spectator seat for a player on `team` with `lives_left` lives.
/// `None` while the player can still respawn.
pub fn seat_out_of_lives(team: u8, lives_left: u32) -> Option<SeatKind> {
    if is_out_of_lives(lives_left) {
        Some(SeatKind::Team { team })
    } else {
        None
    }
}

/// Whether the per-viewer fog filter should keep every entity.
///
/// A finished match does. A neutral seat does when the host granted full vision. A team seat
/// does not: its own team, allies and neutrals stay with the team filter, including a seat that
/// is out of lives and now spectates that team.
pub fn reveals_every_team(viewer: SeatKind, match_over: bool, host_full_vision: bool) -> bool {
    if match_over {
        return true;
    }
    matches!(viewer, SeatKind::Neutral) && host_full_vision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_allies(a: u8, b: u8) -> bool {
        let _ = (a, b);
        false
    }

    fn ally_1_2(a: u8, b: u8) -> bool {
        (a == 1 && b == 2) || (a == 2 && b == 1)
    }

    #[test]
    fn team_seat_sees_own_team_and_neutrals_not_the_enemy() {
        let viewer = SeatKind::Team { team: 1 };
        assert!(sees_team(viewer, 1, no_allies, false, false));
        assert!(sees_team(viewer, 0, no_allies, false, false));
        assert!(!sees_team(viewer, 2, no_allies, false, false));
    }

    #[test]
    fn team_seat_sees_allies_but_never_the_enemy_while_live() {
        let viewer = SeatKind::Team { team: 1 };
        assert!(sees_team(viewer, 2, ally_1_2, false, false));
        assert!(!sees_team(viewer, 3, ally_1_2, false, false));
    }

    #[test]
    fn finished_match_reveals_everything_to_every_seat() {
        assert!(sees_team(SeatKind::Team { team: 1 }, 2, no_allies, true, false));
        assert!(sees_team(SeatKind::Neutral, 2, no_allies, true, false));
        assert!(reveals_every_team(SeatKind::Team { team: 1 }, true, false));
    }

    #[test]
    fn neutral_seat_needs_the_host_grant_for_full_vision() {
        assert!(!sees_team(SeatKind::Neutral, 1, no_allies, false, false));
        assert!(sees_team(SeatKind::Neutral, 0, no_allies, false, false));
        assert!(sees_team(SeatKind::Neutral, 1, no_allies, false, true));
        assert!(sees_team(SeatKind::Neutral, 2, no_allies, false, true));
        assert!(!reveals_every_team(SeatKind::Neutral, false, false));
        assert!(reveals_every_team(SeatKind::Neutral, false, true));
        assert!(!reveals_every_team(SeatKind::Team { team: 1 }, false, true));
    }

    #[test]
    fn out_of_lives_becomes_a_team_spectator() {
        assert!(is_out_of_lives(0));
        assert!(!is_out_of_lives(1));
        assert_eq!(seat_out_of_lives(2, 0), Some(SeatKind::Team { team: 2 }));
        assert_eq!(seat_out_of_lives(2, 3), None);
        let spectator = seat_out_of_lives(2, 0).expect("spectator");
        assert!(sees_team(spectator, 2, no_allies, false, false));
        assert!(!sees_team(spectator, 1, no_allies, false, false));
    }
}
