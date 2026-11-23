// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/modes/koth.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1-108 and the tests, 110-474). Named issues: the rules step a board and a
// contestant list instead of the game's runtime state; the hill owns its position; the four-team
// range, the score limit, the accrual period (one second at the tick rate) and the objective cap
// are settings; the match-over lifecycle event became a `RulesEvent`, and each point is reported.
// The accrual rule is unchanged: once per period, a sole occupying team gains one point; contested
// or empty gains nothing.

//! King of the hill: one shared hill volume; once per accrual period its sole occupying team gains
//! a point.
//!
//! The hill is a 3-D box ([`HillVolume`] has a height band), so on stacked floors a unit in a
//! tunnel under the hill is outside it: [`ObjectiveSettings::touch_height`] (flags only) has no
//! effect here.
//!
//! [`ObjectiveSettings::touch_height`]: super::objective::ObjectiveSettings::touch_height

use super::RulesError;
use super::mode::MatchMode;
use super::objective::{
    Contestant, EndReason, HillVolume, Objective, ObjectiveBoard, ObjectiveKind, RulesEvent,
};

impl ObjectiveBoard {
    /// Install the match's one hill volume as the host's entity `entity_id` (map data or a test).
    /// Fails when a hill exists. Counts against the objective cap.
    pub fn install_hill_volume(&mut self, entity_id: u32, volume: HillVolume) -> Result<(), RulesError> {
        if self.objectives.iter().any(Objective::is_hill) {
            return Err(RulesError::HillExists);
        }
        self.check_room(entity_id)?;
        let center = volume.center();
        self.objectives.push(Objective {
            entity_id,
            kind: ObjectiveKind::Hill,
            team_id: 0,
            carrier_id: None,
            at_base: false,
            base_position: center,
            position: center,
            volume: Some(volume),
            dropped_tick: None,
            zone: None,
        });
        Ok(())
    }

    /// The king-of-the-hill step at match tick `tick`: on every accrual tick (a multiple of the
    /// period, never tick 0), a sole occupying team gains one point. Does nothing outside
    /// [`MatchMode::KingOfTheHill`], without a hill, or once the match is over.
    pub fn koth_tick(&mut self, tick: u64, units: &[Contestant]) {
        if self.mode != MatchMode::KingOfTheHill || self.match_over {
            return;
        }
        let period = u64::from(self.settings.effective_hill_accrual_ticks());
        if tick == 0 || !tick.is_multiple_of(period) {
            return;
        }
        let Some(volume) = self
            .objectives
            .iter()
            .find(|o| o.is_hill() && !self.is_locked(o.entity_id))
            .and_then(|o| o.volume)
        else {
            return;
        };
        // Which fighting teams have a living unit inside (index 0 unused).
        let mut present = vec![false; usize::from(self.settings.max_teams) + 1];
        for u in units {
            if !u.alive || !self.settings.is_fighting_team(u.team) {
                continue;
            }
            if volume.contains(u.position) {
                present[usize::from(u.team)] = true;
            }
        }
        let mut sole = None;
        for (team, &here) in present.iter().enumerate().skip(1) {
            if here {
                if sole.is_some() {
                    return; // contested
                }
                sole = Some(team);
            }
        }
        let Some(team) = sole.and_then(|t| u8::try_from(t).ok()) else {
            return; // empty
        };
        let score = self.add_point(team);
        self.push_event(RulesEvent::HillPoint { tick, team, score });
        if score >= self.settings.effective_hill_score_limit() {
            self.end_match(tick, team, EndReason::HillScoreLimitReached);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective::{DEFAULT_HILL_SCORE_LIMIT, ObjectiveSettings};

    const HZ: u64 = regia_detsim::tick::TICK_RATE_HZ as u64;
    const HILL: HillVolume = HillVolume {
        min: [-10.0, -1.0, -10.0],
        max: [10.0, 20.0, 10.0],
    };

    /// A KOTH board with the hill installed, and its match tick.
    struct Match {
        board: ObjectiveBoard,
        tick: u64,
    }

    impl Match {
        fn with_settings(settings: ObjectiveSettings) -> Self {
            let mut board = ObjectiveBoard::new(MatchMode::KingOfTheHill, settings);
            board.install_hill_volume(900, HILL).expect("hill");
            Self { board, tick: 0 }
        }

        fn new() -> Self {
            Self::with_settings(ObjectiveSettings::default())
        }

        fn advance(&mut self, units: &[Contestant]) {
            self.tick += 1;
            self.board.koth_tick(self.tick, units);
        }

        fn advance_one_second(&mut self, units: &[Contestant]) {
            for _ in 0..HZ {
                self.advance(units);
            }
        }

        fn scores(&self) -> &[u32] {
            &self.board.team_scores
        }
    }

    fn unit(id: u32, team: u8, pos: [f32; 3]) -> Contestant {
        Contestant {
            id,
            team,
            position: pos,
            alive: true,
        }
    }

    fn dead(id: u32, team: u8, pos: [f32; 3]) -> Contestant {
        Contestant {
            alive: false,
            ..unit(id, team, pos)
        }
    }

    #[test]
    fn koth_sole_occupant_gains_one_per_second() {
        let mut m = Match::new();
        let units = [unit(1001, 1, [0.0, 0.0, 0.0])];
        m.advance_one_second(&units);
        assert_eq!(m.board.score(1), 1);
        m.advance_one_second(&units);
        assert_eq!(m.board.score(1), 2);
    }

    #[test]
    fn koth_two_teams_inside_gain_zero() {
        let mut m = Match::new();
        m.advance_one_second(&[unit(1001, 1, [-5.0, 0.0, -5.0]), unit(1002, 2, [5.0, 0.0, 5.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_empty_gains_zero() {
        let mut m = Match::new();
        m.advance_one_second(&[unit(1001, 1, [100.0, 0.0, 100.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_unit_above_the_height_band_scores_nothing() {
        let mut m = Match::new();
        m.board.koth_tick(HZ, &[unit(1001, 1, [0.0, 30.0, 0.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_dead_units_and_team_zero_score_nothing() {
        let mut m = Match::new();
        m.board.koth_tick(
            HZ,
            &[dead(1001, 1, [0.0, 0.0, 0.0]), unit(1002, 0, [1.0, 0.0, 1.0])],
        );
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_same_setup_reaches_the_same_score_twice() {
        let units = [unit(1001, 2, [0.0, 0.0, 0.0]), unit(1002, 2, [100.0, 0.0, 100.0])];
        let (mut first, mut second) = (Match::new(), Match::new());
        for _ in 0..3 {
            first.advance_one_second(&units);
            second.advance_one_second(&units);
        }
        assert_eq!(first.scores(), second.scores());
        assert_eq!(first.board.score(2), 3);
        assert_eq!(first.board.match_over, second.board.match_over);
    }

    #[test]
    fn koth_accrual_ignores_entity_spawn_order() {
        let a = unit(1001, 1, [0.0, 0.0, 0.0]);
        let b = unit(1002, 3, [100.0, 0.0, 100.0]);
        let (mut first, mut second) = (Match::new(), Match::new());
        first.advance_one_second(&[a, b]);
        second.advance_one_second(&[b, a]);
        assert_eq!(first.scores(), second.scores());
        assert_eq!(first.board.score(1), 1);
    }

    #[test]
    fn koth_reaching_limit_ends_the_match() {
        let mut m = Match::with_settings(ObjectiveSettings {
            hill_score_limit: 3,
            ..ObjectiveSettings::default()
        });
        let units = [unit(1001, 2, [0.0, 0.0, 0.0])];
        for _ in 0..3 {
            m.advance_one_second(&units);
        }
        assert_eq!(m.board.score(2), 3);
        assert!(m.board.match_over);
        assert_eq!(m.board.end_reason, Some(EndReason::HillScoreLimitReached));
        assert!(m.board.drain_events().iter().any(|e| matches!(
            e,
            RulesEvent::MatchOver {
                team: 2,
                reason: EndReason::HillScoreLimitReached,
                ..
            }
        )));
    }

    #[test]
    fn koth_second_hill_install_is_rejected() {
        let mut m = Match::new();
        let err = m
            .board
            .install_hill_volume(
                901,
                HillVolume {
                    min: [-5.0, -1.0, -5.0],
                    max: [5.0, 20.0, 5.0],
                },
            )
            .expect_err("exactly one hill per match");
        assert_eq!(err, RulesError::HillExists);
        assert!(err.to_string().contains("already installed"), "{err}");
        assert_eq!(m.board.objectives.iter().filter(|o| o.is_hill()).count(), 1);
    }

    #[test]
    fn koth_hill_is_a_shared_objective_counted_against_the_cap() {
        let m = Match::new();
        assert_eq!(m.board.objectives.len(), 1);
        let hill = &m.board.objectives[0];
        assert_eq!(hill.kind, ObjectiveKind::Hill);
        assert_eq!(hill.kind.as_str(), crate::objective::KIND_HILL);
        assert!(hill.is_hill());
        assert_eq!(hill.team_id, 0, "the hill is shared, not owned");
        assert!(hill.carrier_id.is_none());
        assert_eq!(hill.volume, Some(HILL));
        assert_eq!(
            hill.position,
            [0.0, 9.5, 0.0],
            "the hill sits at the volume's center"
        );
        assert_eq!(m.board.live_objective_count(), 1);
        let mut capped = ObjectiveBoard::new(
            MatchMode::KingOfTheHill,
            ObjectiveSettings {
                max_objectives: 0,
                ..ObjectiveSettings::default()
            },
        );
        assert_eq!(
            capped.install_hill_volume(1, HILL),
            Err(RulesError::ObjectiveCap { cap: 0 })
        );
    }

    #[test]
    fn koth_no_hill_accrues_nothing() {
        // DO27 ignored this test (its opening pieces placed a hill at the origin on the first
        // tick); the board places nothing by itself, so it runs here.
        let mut board = ObjectiveBoard::new(MatchMode::KingOfTheHill, ObjectiveSettings::default());
        for tick in 1..=HZ {
            board.koth_tick(tick, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
        }
        assert_eq!(board.team_scores, [0; 5]);
        assert!(!board.match_over);
    }

    #[test]
    fn koth_partial_second_accrues_nothing() {
        let mut m = Match::new();
        let units = [unit(1001, 1, [0.0, 0.0, 0.0])];
        for _ in 0..30 {
            m.advance(&units);
        }
        assert_eq!(m.board.score(1), 0, "half a second scores nothing");
        for _ in 0..29 {
            m.advance(&units);
        }
        assert_eq!(m.board.score(1), 0, "59 ticks still score nothing");
        m.advance(&units);
        assert_eq!(m.board.score(1), 1, "the full sim second scores one");
    }

    #[test]
    fn koth_tick_zero_never_accrues() {
        let mut m = Match::new();
        m.board.koth_tick(0, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
        assert_eq!(m.board.score(1), 0);
    }

    #[test]
    fn koth_dead_units_do_not_count() {
        let mut m = Match::new();
        m.advance_one_second(&[dead(1001, 1, [0.0, 0.0, 0.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_team_zero_and_out_of_range_units_do_not_count() {
        let mut m = Match::new();
        m.advance_one_second(&[unit(1001, 0, [0.0, 0.0, 0.0]), unit(1002, 5, [0.0, 0.0, 0.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_height_band_excludes_above_and_below() {
        let mut m = Match::new();
        m.advance_one_second(&[unit(1001, 1, [0.0, 100.0, 0.0]), unit(1002, 1, [0.0, -50.0, 0.0])]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_same_team_multiple_units_gain_one() {
        let mut m = Match::new();
        let units = [unit(1001, 3, [-5.0, 0.0, -5.0]), unit(1002, 3, [5.0, 0.0, 5.0])];
        m.advance_one_second(&units);
        assert_eq!(m.board.score(3), 1, "one point per second, not per unit");
        m.advance_one_second(&units);
        assert_eq!(m.board.score(3), 2);
    }

    #[test]
    fn koth_three_teams_inside_gain_zero() {
        let mut m = Match::new();
        m.advance_one_second(&[
            unit(1001, 1, [-5.0, 0.0, -5.0]),
            unit(1002, 2, [5.0, 0.0, 5.0]),
            unit(1003, 4, [-5.0, 0.0, 5.0]),
        ]);
        assert_eq!(m.scores(), [0; 5]);
    }

    #[test]
    fn koth_non_koth_mode_accrues_nothing() {
        let mut m = Match::new();
        m.board.mode = MatchMode::TeamDeathmatch;
        m.advance_one_second(&[unit(1001, 1, [0.0, 0.0, 0.0])]);
        assert_eq!(m.scores(), [0; 5]);
        assert!(!m.board.match_over);
    }

    #[test]
    fn koth_no_scoring_after_match_over() {
        let mut m = Match::with_settings(ObjectiveSettings {
            hill_score_limit: 2,
            ..ObjectiveSettings::default()
        });
        let units = [unit(1001, 1, [0.0, 0.0, 0.0])];
        m.advance_one_second(&units);
        m.advance_one_second(&units);
        assert_eq!(m.board.score(1), 2);
        assert!(m.board.match_over);
        assert!(m.board.match_over_tick.is_some());
        m.advance_one_second(&units);
        m.advance_one_second(&units);
        assert_eq!(m.board.score(1), 2, "score freezes once the match ends");
    }

    #[test]
    fn koth_default_limit_is_180_and_zero_clamps_to_one() {
        assert_eq!(DEFAULT_HILL_SCORE_LIMIT, 180);
        assert_eq!(ObjectiveSettings::default().effective_hill_score_limit(), 180);
        let mut m = Match::with_settings(ObjectiveSettings {
            hill_score_limit: 0,
            ..ObjectiveSettings::default()
        });
        m.advance_one_second(&[unit(1001, 1, [0.0, 0.0, 0.0])]);
        assert!(m.board.match_over, "zero limit clamps to min 1");
    }

    #[test]
    fn koth_accrual_period_and_team_count_are_settings() {
        let mut m = Match::with_settings(ObjectiveSettings {
            max_teams: 8,
            hill_accrual_ticks: 30,
            ..ObjectiveSettings::default()
        });
        let units = [unit(1001, 7, [0.0, 0.0, 0.0])];
        m.advance_one_second(&units);
        assert_eq!(m.board.score(7), 2, "two 30-tick periods in a second");
    }
}
