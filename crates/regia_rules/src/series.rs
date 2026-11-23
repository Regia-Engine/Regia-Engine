// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/series.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: the scrap bank is an opaque carry value the game sets (the
// economy is not copied); GameplayRuntimeState::apply_series and round_result are not copied;
// the fighting-team cap is a setting whose default is 4, stored in a 5-slot table (index 0 unused).

//! Best-of-N series between matches.
//!
//! Lengths are 1 (a single match), 3, 5, 7 or 9. A round whose winner is outside the fighting
//! teams is replayed. The carry is an opaque `[u32; 5]` the game sets; this module does not
//! interpret it. When a side reaches the wins it needs, the series ends and the carry clears.

use serde::{Deserialize, Serialize};

use crate::RulesError;

/// Series lengths the host can pick. 1 is a single match.
pub const SERIES_LENGTHS: [u32; 5] = [1, 3, 5, 7, 9];

/// Slots in the win and carry tables. Index 0 is unused. Teams are `1..=4`.
pub const SERIES_TEAM_SLOTS: usize = 5;

/// The highest fighting team the tables can store. This is the default team cap.
pub const SERIES_MAX_TEAMS: u8 = 4;

/// Validate a host-picked series length.
pub fn clamp_series(length: u32) -> Result<u32, RulesError> {
    if SERIES_LENGTHS.contains(&length) {
        Ok(length)
    } else {
        Err(RulesError::SeriesLength { length })
    }
}

/// How one finished round came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoundResult {
    /// Winning team. 0, or a team outside the cap, means the round is replayed.
    pub winner: u8,
    /// Opaque per-team carry into the next round. Index is the team. The game sets the values.
    pub carry: [u32; SERIES_TEAM_SLOTS],
}

/// The series book. Index 0 of the arrays is unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesState {
    /// 1, 3, 5, 7 or 9.
    pub length: u8,
    /// Rounds that counted.
    pub played: u8,
    /// Wins per team. Index is the team.
    pub wins: [u8; SERIES_TEAM_SLOTS],
    /// Carry each team takes into the next round. Cleared when the series ends.
    pub carry: [u32; SERIES_TEAM_SLOTS],
    /// Team that won the series. 0 while it is open.
    pub winner: u8,
    /// Highest fighting team. The default is [`SERIES_MAX_TEAMS`].
    pub max_teams: u8,
}

impl Default for SeriesState {
    fn default() -> Self {
        Self::new(1)
    }
}

impl SeriesState {
    /// A series of `length`. An invalid length becomes a single match.
    pub fn new(length: u32) -> Self {
        let length = u8::try_from(clamp_series(length).unwrap_or(1)).unwrap_or(1);
        Self {
            length,
            played: 0,
            wins: [0; SERIES_TEAM_SLOTS],
            carry: [0; SERIES_TEAM_SLOTS],
            winner: 0,
            max_teams: SERIES_MAX_TEAMS,
        }
    }

    /// Wins that take the series: 1 for a single match, 2 of 3, 3 of 5, and so on.
    pub fn need(&self) -> u8 {
        self.length / 2 + 1
    }

    /// True when the length is more than one match.
    pub fn is_series(&self) -> bool {
        self.length > 1
    }

    /// True once a team has won the series.
    pub fn is_over(&self) -> bool {
        self.winner != 0
    }

    /// Back to 0-0 at the same length.
    pub fn reset(&mut self) {
        let length = u32::from(self.length);
        let max_teams = self.max_teams;
        *self = Self::new(length);
        self.max_teams = max_teams;
    }

    /// Fold one finished round in. A round with no winner does not count: it is replayed.
    /// Returns false when nothing changed (no winner, or the series is already decided).
    pub fn record_round(&mut self, round: &RoundResult) -> bool {
        if self.is_over() || round.winner == 0 || round.winner > self.max_teams {
            return false;
        }
        let index = usize::from(round.winner);
        if index >= SERIES_TEAM_SLOTS {
            return false;
        }
        self.played = self.played.saturating_add(1);
        let wins = self.wins[index].saturating_add(1);
        self.wins[index] = wins;
        if wins >= self.need() {
            self.winner = round.winner;
            self.carry = [0; SERIES_TEAM_SLOTS];
        } else {
            self.carry = round.carry;
        }
        true
    }

    /// Wins of teams 1 and 2, the pair the series line shows.
    fn score_pair(&self) -> (u8, u8) {
        (self.wins[1], self.wins[2])
    }

    /// The score line: `SERIES 2-1, BEST OF 5`. Empty for a single match.
    pub fn label(&self) -> String {
        if !self.is_series() {
            return String::new();
        }
        let (a, b) = self.score_pair();
        let length = self.length;
        format!("SERIES {a}-{b}, BEST OF {length}")
    }

    /// The decided line: `TEAM 1 WINS THE SERIES 2-1, BEST OF 3`. Empty while it is open,
    /// and empty for a single match.
    pub fn winner_label(&self) -> String {
        if !self.is_series() || !self.is_over() {
            return String::new();
        }
        let (a, b) = self.score_pair();
        let winner = self.winner;
        let length = self.length;
        format!("TEAM {winner} WINS THE SERIES {a}-{b}, BEST OF {length}")
    }

    /// What the host sees on the series row.
    pub fn row_text(&self) -> String {
        if self.is_series() {
            let length = self.length;
            format!("BEST OF {length}")
        } else {
            "SINGLE MATCH".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn won(team: u8, carry_value: u32) -> RoundResult {
        let mut carry = [0; SERIES_TEAM_SLOTS];
        carry[usize::from(team)] = carry_value;
        RoundResult { winner: team, carry }
    }

    #[test]
    fn lengths_are_single_or_odd_up_to_nine() {
        for ok in [1, 3, 5, 7, 9] {
            assert_eq!(clamp_series(ok), Ok(ok));
        }
        for bad in [0, 2, 4, 6, 8, 10, 99] {
            assert!(clamp_series(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn best_of_three_ends_at_two_wins_and_carries_between() {
        let mut series = SeriesState::new(3);
        assert_eq!(series.label(), "SERIES 0-0, BEST OF 3");
        assert!(series.record_round(&won(1, 40)));
        assert_eq!(series.label(), "SERIES 1-0, BEST OF 3");
        assert_eq!(series.carry[1], 40, "the winner's carry moves to the next round");
        assert!(!series.is_over());
        assert!(series.record_round(&won(2, 15)));
        assert_eq!(series.label(), "SERIES 1-1, BEST OF 3");
        assert_eq!((series.carry[1], series.carry[2]), (0, 15));
        assert!(series.record_round(&won(1, 9)));
        assert!(series.is_over());
        assert_eq!(series.winner, 1);
        assert_eq!(series.winner_label(), "TEAM 1 WINS THE SERIES 2-1, BEST OF 3");
        assert_eq!(
            series.carry, [0; SERIES_TEAM_SLOTS],
            "a finished series carries nothing"
        );
        assert!(
            !series.record_round(&won(2, 1)),
            "a decided series takes no more rounds"
        );
        series.reset();
        assert_eq!((series.played, series.winner, series.length), (0, 0, 3));
    }

    #[test]
    fn a_round_without_a_winner_is_replayed() {
        let mut series = SeriesState::new(5);
        assert!(!series.record_round(&RoundResult::default()));
        assert_eq!(series.played, 0);
        let outside = RoundResult {
            winner: 5,
            carry: [0; SERIES_TEAM_SLOTS],
        };
        assert!(!series.record_round(&outside));
        assert_eq!(series.played, 0);
    }

    #[test]
    fn a_single_match_is_not_a_series() {
        let mut one = SeriesState::new(1);
        assert_eq!(one.label(), "");
        assert!(one.record_round(&won(2, 3)));
        assert!(one.is_over());
        assert_eq!(one.winner_label(), "");
        assert_eq!(one.row_text(), "SINGLE MATCH");
        assert_eq!(SeriesState::new(9).row_text(), "BEST OF 9");
    }
}
