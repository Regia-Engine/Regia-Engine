// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/modes/ctf.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1-287 and the tests, 289-812). Named issues: the rules step a board and a
// contestant list instead of the game's runtime state (a dead or zero-health unit is a contestant
// with `alive: false`); the flag owns its position; the four-team range, the touch radius, the
// capture limit and the objective cap are settings; HUD, log and lifecycle side effects became
// `RulesEvent`s (pickups and drops are reported too). The rule order is unchanged: carriers sync
// first, captures resolve before pickups and returns, the first touching unit in the host's order
// wins.
//
// Also from Direct Order 27 (ir-sim/src/modes/mod.rs, commit d6ed37677), lines 86-143: the
// symmetric flag bases. The flag offset and the edge margin are parameters, and the game's own
// legacy-canvas special case is not carried (see `ctf_base_position_for_starts`).

//! Capture the flag: flags are objectives on the [`ObjectiveBoard`]; a carrier brings an enemy flag
//! to its own base while its own flag is home to score.

use regia_detsim::float::{det_clamp, det_max};

use super::RulesError;
use super::mode::MatchMode;
use super::objective::{
    AutoReturnCause, Contestant, EndReason, Objective, ObjectiveBoard, ObjectiveKind, RulesEvent,
};
use super::objective::{ObjectiveSettings, distance_sq_xz};

/// The host's answer for a point where a flag lies on the ground (see
/// [`ObjectiveBoard::ctf_tick_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DropCheck {
    /// The flag stays where it lies (S-14's rule, and what [`ObjectiveBoard::ctf_tick`] answers).
    #[default]
    Keep,
    /// Nobody can reach the point (a kill volume, out of bounds, inside collision, a sealed
    /// tunnel): the flag goes home at once with a `flag_auto_returned` event (cause `drop_check`).
    Return,
}

/// Home base of `team`'s flag with the flags `offset` world units from the center: teams 1 and 2
/// face off on X, teams 3 and 4 on Z, so every pair starts the same distance apart. Other teams
/// sit at the center.
pub fn ctf_base_position(team: u8, offset: f32) -> [f32; 3] {
    match team {
        1 => [-offset, 0.0, 0.0],
        2 => [offset, 0.0, 0.0],
        3 => [0.0, 0.0, -offset],
        4 => [0.0, 0.0, offset],
        _ => [0.0, 0.0, 0.0],
    }
}

/// Flag home of `team` from a map's two team starts: teams 1 and 2 sit on their starts (at ground
/// height 0), teams 3 and 4 at `-|start x|` and `+|start x|` on Z. With the playfield `extent`
/// (`[width, depth]`), teams 3 and 4 are clamped inside `depth / 2 - edge_margin`.
pub fn ctf_base_position_for_starts(
    starts: [[f32; 3]; 2],
    extent: Option<[f32; 2]>,
    edge_margin: f32,
    team: u8,
) -> [f32; 3] {
    let ax = starts[0][0].abs();
    match team {
        1 => [starts[0][0], 0.0, starts[0][2]],
        2 => [starts[1][0], 0.0, starts[1][2]],
        3 | 4 => {
            let sign = if team == 3 { -1.0 } else { 1.0 };
            let mut z = sign * ax;
            if let Some([_, depth]) = extent {
                // ADR-8085: the lockstep float order functions, not std `max`/`clamp` (zero sign).
                let limit = det_max(depth * 0.5 - edge_margin, 0.0);
                z = det_clamp(z, -limit, limit);
            }
            [0.0, 0.0, z]
        }
        _ => [0.0, 0.0, 0.0],
    }
}

/// True when a carrier at `pos` is out of reach of its base at `base`. Written with `>` as S-14
/// wrote the capture test (a NaN distance does not block it), so `touch_height: None` keeps
/// S-14's bits; the height band adds `dy * dy > h * h`.
fn carrier_off_base(settings: &ObjectiveSettings, pos: [f32; 3], base: [f32; 3]) -> bool {
    let r = settings.flag_touch_radius;
    if distance_sq_xz(pos, base) > r * r {
        return true;
    }
    match settings.touch_height {
        None => false,
        Some(h) => {
            let dy = pos[1] - base[1];
            dy * dy > h * h
        }
    }
}

impl ObjectiveBoard {
    /// Place `team`'s flag idle at `base_position`, as the host's entity `entity_id`. Counts against
    /// the objective cap.
    pub fn spawn_flag(
        &mut self,
        entity_id: u32,
        team: u8,
        base_position: [f32; 3],
    ) -> Result<(), RulesError> {
        if !self.settings.is_fighting_team(team) {
            return Err(RulesError::FlagTeam {
                team,
                max_teams: self.settings.max_teams,
            });
        }
        if self.objectives.iter().any(|o| o.is_flag() && o.team_id == team) {
            return Err(RulesError::FlagExists { team });
        }
        self.check_room(entity_id)?;
        self.objectives.push(Objective {
            entity_id,
            kind: ObjectiveKind::Flag,
            team_id: team,
            carrier_id: None,
            at_base: true,
            base_position,
            position: base_position,
            volume: None,
            dropped_tick: None,
            zone: None,
        });
        Ok(())
    }

    /// Place one flag per team in `teams` (ascending, each once) at
    /// [`ctf_base_position`]`(team, offset)`, with entity ids from `first_entity_id` up. Teams that
    /// already have a flag are skipped, so a second call places nothing.
    pub fn place_flags(
        &mut self,
        teams: &[u8],
        offset: f32,
        first_entity_id: u32,
    ) -> Result<usize, RulesError> {
        let mut placed = 0;
        let mut next_id = first_entity_id;
        for &team in teams {
            if self.objectives.iter().any(|o| o.is_flag() && o.team_id == team) {
                continue;
            }
            while self.objective(next_id).is_some() {
                next_id = next_id.wrapping_add(1);
            }
            self.spawn_flag(next_id, team, ctf_base_position(team, offset))?;
            next_id = next_id.wrapping_add(1);
            placed += 1;
        }
        Ok(placed)
    }

    /// The capture-the-flag step at match tick `tick`: carried flags follow their carriers (a dead
    /// or missing carrier drops the flag), then captures resolve, then pickups and returns, then
    /// dropped flags whose [`ObjectiveSettings::flag_return_ticks`] ran out go home. Does nothing
    /// outside [`MatchMode::CaptureTheFlag`] or once the match is over. Every drop point is kept
    /// ([`ObjectiveBoard::ctf_tick_with`] answering [`DropCheck::Keep`]).
    pub fn ctf_tick(&mut self, tick: u64, units: &[Contestant]) {
        self.ctf_tick_with(tick, units, |_| DropCheck::Keep);
    }

    /// [`ObjectiveBoard::ctf_tick`] with a host drop check: after carriers sync, `drop_check` is
    /// asked once for each flag lying on the ground (the step it drops and every later step it
    /// still lies there), in board order, with the point it lies at. [`DropCheck::Return`] sends it
    /// home at once (a `flag_auto_returned` event, cause `drop_check`), before anyone can touch it
    /// on this step. The answer must be the same on every peer for the same point and step: use
    /// map data, never local physics state that peers do not share.
    pub fn ctf_tick_with<F>(&mut self, tick: u64, units: &[Contestant], mut drop_check: F)
    where
        F: FnMut([f32; 3]) -> DropCheck,
    {
        if self.mode != MatchMode::CaptureTheFlag || self.match_over {
            return;
        }
        self.ctf_sync_carriers_and_drops(tick, units);
        self.ctf_check_drops(tick, &mut drop_check);
        self.ctf_resolve_touches(tick, units);
        if !self.match_over {
            self.ctf_expire_drops(tick);
        }
    }

    /// Ask the host about every flag lying on the ground; start the timer of a lying flag that has
    /// none (a state set by hand or saved before the timer existed).
    fn ctf_check_drops<F>(&mut self, tick: u64, drop_check: &mut F)
    where
        F: FnMut([f32; 3]) -> DropCheck,
    {
        for oi in 0..self.objectives.len() {
            let flag = &mut self.objectives[oi];
            if !flag.is_dropped() {
                continue;
            }
            if flag.dropped_tick.is_none() {
                flag.dropped_tick = Some(tick);
            }
            if drop_check(flag.position) == DropCheck::Return {
                self.ctf_auto_return(tick, oi, AutoReturnCause::DropCheck);
            }
        }
    }

    /// Send home the lying flags whose return timer ran out on this step.
    fn ctf_expire_drops(&mut self, tick: u64) {
        let Some(limit) = self.settings.flag_return_ticks else {
            return;
        };
        for oi in 0..self.objectives.len() {
            let flag = &self.objectives[oi];
            if !flag.is_dropped() {
                continue;
            }
            let since = flag.dropped_tick.unwrap_or(tick);
            if tick.saturating_sub(since) >= limit {
                self.ctf_auto_return(tick, oi, AutoReturnCause::Timeout);
            }
        }
    }

    fn ctf_auto_return(&mut self, tick: u64, oi: usize, cause: AutoReturnCause) {
        let (flag, flag_team) = (self.objectives[oi].entity_id, self.objectives[oi].team_id);
        self.ctf_return_flag(oi);
        self.events.push(RulesEvent::FlagAutoReturned {
            tick,
            flag,
            flag_team,
            cause,
        });
    }

    fn ctf_sync_carriers_and_drops(&mut self, tick: u64, units: &[Contestant]) {
        let mut dropped = Vec::new();
        for flag in self.objectives.iter_mut().filter(|o| o.is_flag()) {
            let Some(carrier_id) = flag.carrier_id else {
                continue;
            };
            match units.iter().find(|u| u.id == carrier_id) {
                Some(unit) if unit.alive => {
                    flag.position = unit.position;
                    continue;
                }
                // Carrier death: drop at the death position.
                Some(unit) => flag.position = unit.position,
                // Carrier gone: drop at the flag's last position.
                None => {}
            }
            flag.carrier_id = None;
            flag.at_base = false;
            flag.dropped_tick = Some(tick);
            dropped.push(RulesEvent::FlagDropped {
                tick,
                flag: flag.entity_id,
                unit: carrier_id,
                flag_team: flag.team_id,
                position: flag.position,
            });
        }
        for event in dropped {
            self.push_event(event);
        }
    }

    fn ctf_resolve_touches(&mut self, tick: u64, units: &[Contestant]) {
        let settings = self.settings;
        // Living units on a fighting team, in the host's order.
        let living: Vec<Contestant> = units
            .iter()
            .filter(|u| u.alive && settings.is_fighting_team(u.team))
            .copied()
            .collect();

        // Capture: the carrier of an enemy flag touches its own base while its own flag is home
        // (or at all, with `capture_needs_home_flag: false`).
        for (flag_oi, scoring_team) in self.ctf_pending_captures(&living) {
            self.ctf_apply_capture(tick, flag_oi, scoring_team);
            if self.match_over {
                return;
            }
        }

        // Pickup or return of flags nobody carries.
        for oi in 0..self.objectives.len() {
            let flag = &self.objectives[oi];
            if !flag.is_flag() || flag.carrier_id.is_some() || self.is_locked(flag.entity_id) {
                continue;
            }
            let (flag_team, flag_id, at_base, flag_pos) =
                (flag.team_id, flag.entity_id, flag.at_base, flag.position);
            // The first touching unit in the host's order wins.
            let toucher = living.iter().find(|u| {
                settings.flag_touches(u.position, flag_pos)
                    // Own team: only a dropped flag is returned (never carried from its base).
                    // An enemy picks it up from its base or the ground.
                    && (u.team != flag_team || !at_base)
            });
            let Some(unit) = toucher else {
                continue;
            };
            if unit.team == flag_team {
                self.ctf_return_flag(oi);
                self.push_event(RulesEvent::FlagReturned {
                    tick,
                    flag: flag_id,
                    unit: unit.id,
                    flag_team,
                });
            } else {
                let flag = &mut self.objectives[oi];
                flag.carrier_id = Some(unit.id);
                flag.at_base = false;
                flag.dropped_tick = None;
                self.push_event(RulesEvent::FlagTaken {
                    tick,
                    flag: flag_id,
                    unit: unit.id,
                    team: unit.team,
                    flag_team,
                });
            }
        }
    }

    /// `(enemy flag objective index, scoring team)` for the captures that are ready.
    fn ctf_pending_captures(&self, living: &[Contestant]) -> Vec<(usize, u8)> {
        let mut out = Vec::new();
        for (oi, obj) in self.objectives.iter().enumerate() {
            if !obj.is_flag() {
                continue;
            }
            let Some(carrier_id) = obj.carrier_id else {
                continue;
            };
            // A locked flag does not score until it is unlocked.
            if self.is_locked(obj.entity_id) {
                continue;
            }
            let Some(carrier) = living.iter().find(|u| u.id == carrier_id) else {
                continue;
            };
            // Only an enemy flag scores.
            if carrier.team == obj.team_id {
                continue;
            }
            let own_flag_home = self
                .objectives
                .iter()
                .any(|o| o.is_flag() && o.team_id == carrier.team && o.at_base && o.carrier_id.is_none());
            let own_base = self
                .objectives
                .iter()
                .find(|o| o.is_flag() && o.team_id == carrier.team)
                .map(|o| o.base_position);
            let Some(base) = own_base else {
                continue;
            };
            if carrier_off_base(&self.settings, carrier.position, base) {
                continue;
            }
            // Own flag away: no score, the enemy flag stays carried (unless the setting is off).
            if own_flag_home || !self.settings.capture_needs_home_flag {
                out.push((oi, carrier.team));
            }
        }
        out
    }

    fn ctf_apply_capture(&mut self, tick: u64, enemy_flag_oi: usize, scoring_team: u8) {
        if scoring_team == 0 {
            return;
        }
        let flag_team = self.objectives[enemy_flag_oi].team_id;
        let flag_id = self.objectives[enemy_flag_oi].entity_id;
        let carrier_id = self.objectives[enemy_flag_oi].carrier_id.unwrap_or(0);
        let score = self.add_point(scoring_team);
        // The captured flag returns to its owner's base at once.
        self.ctf_return_flag(enemy_flag_oi);
        self.push_event(RulesEvent::FlagCaptured {
            tick,
            flag: flag_id,
            unit: carrier_id,
            team: scoring_team,
            flag_team,
            score,
        });
        if score >= self.settings.effective_capture_limit() {
            self.end_match(tick, scoring_team, EndReason::CaptureLimitReached);
        }
    }

    fn ctf_return_flag(&mut self, oi: usize) {
        let flag = &mut self.objectives[oi];
        flag.carrier_id = None;
        flag.at_base = true;
        flag.position = flag.base_position;
        flag.dropped_tick = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DO27's default flag offset, used as test data.
    const FLAG_X: f32 = 550.0;

    fn empty_board() -> ObjectiveBoard {
        ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default())
    }

    fn unit(id: u32, team: u8, pos: [f32; 3]) -> Contestant {
        Contestant {
            id,
            team,
            position: pos,
            alive: true,
        }
    }

    fn index(board: &ObjectiveBoard, id: u32) -> usize {
        board
            .objectives
            .iter()
            .position(|o| o.entity_id == id)
            .expect("objective")
    }

    fn flag(board: &ObjectiveBoard, id: u32) -> &Objective {
        board.objective(id).expect("flag")
    }

    fn force_carry(board: &mut ObjectiveBoard, flag: u32, carrier: u32) {
        let oi = index(board, flag);
        board.objectives[oi].carrier_id = Some(carrier);
        board.objectives[oi].at_base = false;
    }

    fn force_drop(board: &mut ObjectiveBoard, flag: u32, at: [f32; 3]) {
        let oi = index(board, flag);
        board.objectives[oi].carrier_id = None;
        board.objectives[oi].at_base = false;
        board.objectives[oi].position = at;
    }

    const BASE1: [f32; 3] = [0.0, 0.0, 0.0];
    const BASE2: [f32; 3] = [100.0, 0.0, 0.0];

    fn two_flags() -> ObjectiveBoard {
        let mut board = empty_board();
        board.spawn_flag(2, 2, BASE2).expect("flag2");
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board
    }

    #[test]
    fn ctf_rejects_team_outside_the_fighting_teams() {
        let mut board = empty_board();
        assert_eq!(
            board.spawn_flag(1, 0, BASE1),
            Err(RulesError::FlagTeam {
                team: 0,
                max_teams: 4
            })
        );
        assert!(board.spawn_flag(1, 5, BASE1).is_err());
        assert!(board.objectives.is_empty());
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        assert_eq!(
            board.spawn_flag(2, 1, BASE1),
            Err(RulesError::FlagExists { team: 1 })
        );
        assert_eq!(
            board.spawn_flag(1, 2, BASE2),
            Err(RulesError::ObjectiveId { id: 1 })
        );
        let six = ObjectiveSettings {
            max_teams: 6,
            max_objectives: 2,
            ..ObjectiveSettings::default()
        };
        let mut wide = ObjectiveBoard::new(MatchMode::CaptureTheFlag, six);
        wide.spawn_flag(1, 6, BASE1)
            .expect("team 6 fights when the setting says so");
        wide.spawn_flag(2, 5, BASE2).expect("flag");
        assert_eq!(
            wide.spawn_flag(3, 4, BASE2),
            Err(RulesError::ObjectiveCap { cap: 2 })
        );
    }

    #[test]
    fn ctf_enemy_touch_picks_up_flag_at_base() {
        let mut board = two_flags();
        board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
        let obj = flag(&board, 2);
        assert_eq!(obj.carrier_id, Some(1001));
        assert!(!obj.at_base);
    }

    #[test]
    fn ctf_own_flag_at_base_is_not_carryable() {
        let mut board = empty_board();
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board.ctf_tick(1, &[unit(1001, 1, BASE1)]);
        let obj = flag(&board, 1);
        assert!(obj.carrier_id.is_none());
        assert!(obj.at_base);
    }

    #[test]
    fn ctf_carrier_death_drops_flag_at_death_position() {
        let mut board = two_flags();
        let death = [50.0, 0.0, 10.0];
        force_carry(&mut board, 2, 1001);
        let mut dead = unit(1001, 1, death);
        dead.alive = false;
        board.ctf_tick(1, &[dead]);
        let obj = flag(&board, 2);
        assert!(obj.carrier_id.is_none());
        assert!(!obj.at_base);
        assert!((obj.position[0] - death[0]).abs() < 0.01);
        assert!((obj.position[2] - death[2]).abs() < 0.01);
        assert!(matches!(
            board.drain_events().as_slice(),
            [RulesEvent::FlagDropped {
                unit: 1001,
                flag_team: 2,
                ..
            }]
        ));
    }

    #[test]
    fn ctf_carrier_gone_drops_flag_where_it_was() {
        let mut board = two_flags();
        board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
        board.ctf_tick(2, &[unit(1001, 1, [70.0, 0.0, 3.0])]);
        board.ctf_tick(3, &[]);
        let obj = flag(&board, 2);
        assert!(obj.is_dropped());
        assert_eq!(obj.position, [70.0, 0.0, 3.0]);
    }

    #[test]
    fn ctf_ally_touch_returns_dropped_flag() {
        let mut board = empty_board();
        let drop = [40.0, 0.0, 0.0];
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        force_drop(&mut board, 1, drop);
        board.ctf_tick(1, &[unit(1001, 1, drop)]);
        let obj = flag(&board, 1);
        assert!(obj.at_base);
        assert!(obj.carrier_id.is_none());
        assert!((obj.position[0] - BASE1[0]).abs() < 0.01);
    }

    #[test]
    fn ctf_capture_only_when_own_flag_is_home() {
        let mut board = empty_board();
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board.spawn_flag(2, 2, BASE2).expect("flag2");
        let units = [unit(1001, 1, BASE1)];
        force_carry(&mut board, 2, 1001);
        // Own flag not home: no score.
        force_drop(&mut board, 1, BASE1);
        board.ctf_tick(1, &units);
        assert_eq!(board.score(1), 0);
        assert_eq!(
            flag(&board, 2).carrier_id,
            Some(1001),
            "enemy flag stays carried when own flag is away"
        );
        // Own flag home: score.
        let oi1 = index(&board, 1);
        board.objectives[oi1].at_base = true;
        board.ctf_tick(2, &units);
        assert_eq!(board.score(1), 1);
        assert!(flag(&board, 2).at_base);
        assert!(flag(&board, 2).carrier_id.is_none());
    }

    #[test]
    fn ctf_capture_limit_ends_the_match() {
        let mut board = ObjectiveBoard::new(
            MatchMode::CaptureTheFlag,
            ObjectiveSettings {
                capture_limit: 2,
                ..ObjectiveSettings::default()
            },
        );
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board.spawn_flag(2, 2, BASE2).expect("flag2");
        for tick in 1..=2 {
            force_carry(&mut board, 2, 1001);
            board.ctf_tick(tick, &[unit(1001, 1, BASE1)]);
        }
        assert_eq!(board.score(1), 2);
        assert!(board.match_over);
        assert_eq!(board.match_over_tick, Some(2));
        assert_eq!(board.end_reason, Some(EndReason::CaptureLimitReached));
        assert!(board.drain_events().iter().any(|e| matches!(
            e,
            RulesEvent::MatchOver {
                team: 1,
                reason: EndReason::CaptureLimitReached,
                ..
            }
        )));
        // Nothing moves after the end.
        force_carry(&mut board, 2, 1001);
        board.ctf_tick(3, &[unit(1001, 1, BASE1)]);
        assert_eq!(board.score(1), 2);
    }

    #[test]
    fn ctf_enemy_touch_picks_up_dropped_flag() {
        let mut board = two_flags();
        let drop = [60.0, 0.0, 5.0];
        force_drop(&mut board, 2, drop);
        board.ctf_tick(1, &[unit(1001, 1, drop)]);
        let obj = flag(&board, 2);
        assert_eq!(obj.carrier_id, Some(1001));
        assert!(!obj.at_base);
    }

    #[test]
    fn ctf_any_living_unit_picks_up_and_a_teammate_repicks_a_drop() {
        // DO27's on-foot pilot and hull test, without the game's unit kinds: any living
        // contestant carries, and a teammate re-picks the flag its carrier dropped.
        let mut board = two_flags();
        board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
        assert_eq!(flag(&board, 2).carrier_id, Some(1001), "the first unit picks up");
        let death = [40.0, 0.0, 0.0];
        let mut fallen = unit(1001, 1, death);
        fallen.alive = false;
        board.ctf_tick(2, &[fallen, unit(1002, 1, death)]);
        let obj = flag(&board, 2);
        assert_eq!(obj.carrier_id, Some(1002), "a teammate re-picks the dropped flag");
        assert!(!obj.at_base);
    }

    #[test]
    fn ctf_carried_flag_follows_carrier() {
        let mut board = two_flags();
        board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
        assert_eq!(flag(&board, 2).carrier_id, Some(1001));
        let moved = [30.0, 0.0, -12.0];
        board.ctf_tick(2, &[unit(1001, 1, moved)]);
        let pos = flag(&board, 2).position;
        assert!((pos[0] - moved[0]).abs() < 0.01);
        assert!((pos[2] - moved[2]).abs() < 0.01);
        assert_eq!(flag(&board, 2).carrier_id, Some(1001));
    }

    #[test]
    fn ctf_place_flags_puts_one_flag_per_fighting_team_once() {
        let mut board = empty_board();
        assert_eq!(board.place_flags(&[1, 2, 3], FLAG_X, 500), Ok(3));
        let mut teams: Vec<u8> = board
            .objectives
            .iter()
            .filter(|o| o.is_flag())
            .map(|o| o.team_id)
            .collect();
        teams.sort_unstable();
        assert_eq!(teams, vec![1, 2, 3]);
        for obj in board.objectives.iter().filter(|o| o.is_flag()) {
            assert_eq!(obj.base_position, ctf_base_position(obj.team_id, FLAG_X));
            assert_eq!(obj.position, obj.base_position);
            assert!(obj.at_base);
            assert!(obj.carrier_id.is_none());
        }
        assert_eq!(
            board.place_flags(&[1, 2, 3], FLAG_X, 500),
            Ok(0),
            "a second call places nothing"
        );
        assert_eq!(board.objectives.len(), 3);
        // Four teams on another board: each flag at its symmetric base.
        let mut four = empty_board();
        assert_eq!(four.place_flags(&[1, 2, 3, 4], FLAG_X, 1), Ok(4));
        let t1 = four.objectives.iter().find(|o| o.team_id == 1).expect("t1");
        let t2 = four.objectives.iter().find(|o| o.team_id == 2).expect("t2");
        assert!((t1.base_position[0] + FLAG_X).abs() < 0.01);
        assert!((t2.base_position[0] - FLAG_X).abs() < 0.01);
    }

    #[test]
    fn ctf_bases_pair_teams_at_equal_distance() {
        let b = [1u8, 2, 3, 4].map(|t| ctf_base_position(t, FLAG_X));
        for base in b {
            assert!(base[1].abs() < 0.01, "bases sit at ground level");
        }
        let dist2 = |a: [f32; 3], c: [f32; 3]| distance_sq_xz(a, c);
        let span2 = (2.0 * FLAG_X) * (2.0 * FLAG_X);
        assert!((dist2(b[0], b[1]) - span2).abs() < 1.0);
        assert!((dist2(b[2], b[3]) - span2).abs() < 1.0);
        // Every team starts the same walk to either enemy base.
        assert_eq!(dist2(b[0], b[2]), dist2(b[0], b[3]));
        assert_eq!(dist2(b[0], b[2]), dist2(b[1], b[2]));
    }

    #[test]
    fn flags_follow_authored_starts_and_clamp_inside_the_playfield() {
        let wash = [[-880.0, 4.0, 12.0], [880.0, 4.0, -12.0]];
        assert_eq!(
            ctf_base_position_for_starts(wash, None, 20.0, 1),
            [-880.0, 0.0, 12.0]
        );
        assert_eq!(
            ctf_base_position_for_starts(wash, None, 20.0, 2),
            [880.0, 0.0, -12.0]
        );
        assert_eq!(
            ctf_base_position_for_starts(wash, None, 20.0, 3),
            [0.0, 0.0, -880.0]
        );
        assert_eq!(
            ctf_base_position_for_starts(wash, None, 20.0, 4),
            [0.0, 0.0, 880.0]
        );
        let wide = [[-2000.0, 0.0, 0.0], [2000.0, 0.0, 0.0]];
        let z3 = ctf_base_position_for_starts(wide, Some([2050.0, 2050.0]), 20.0, 3)[2];
        assert!((z3 - -(1025.0 - 20.0)).abs() < 0.01, "clamped {z3}");
        // A square playfield wider than the offset leaves the symmetric layout as it is.
        let square = [[-FLAG_X, 0.0, 0.0], [FLAG_X, 0.0, 0.0]];
        for team in 1..=4 {
            assert_eq!(
                ctf_base_position_for_starts(square, Some([1280.0, 1280.0]), 20.0, team),
                ctf_base_position(team, FLAG_X)
            );
        }
    }

    #[test]
    fn ctf_three_team_scores_do_not_pretend_only_teams_one_and_two() {
        let mut board = empty_board();
        let bases = [(1u8, BASE1), (2, BASE2), (3, [0.0, 0.0, 100.0])];
        for (team, base) in bases {
            board.spawn_flag(u32::from(team), team, base).expect("flag");
        }
        // Team 3 carries team 1's flag to team 3's base; team 3's flag is home.
        force_carry(&mut board, 1, 1003);
        board.ctf_tick(1, &[unit(1003, 3, bases[2].1)]);
        assert_eq!(board.score(3), 1, "team 3 scored");
        assert_eq!(board.score(1), 0);
        assert_eq!(board.score(2), 0);
        assert_eq!(board.score(4), 0);
    }

    #[test]
    fn ctf_capture_and_return_push_feed_events() {
        let mut board = empty_board();
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board.spawn_flag(2, 2, BASE2).expect("flag2");
        // Return path: a team 2 unit on its own dropped flag.
        let drop = [60.0, 0.0, 0.0];
        force_drop(&mut board, 2, drop);
        // Capture path: a team 2 carrier holds team 1's flag at team 2's base. Captures resolve
        // before returns each tick, so the return lands on tick one (own flag away: no score yet)
        // and the capture on tick two (own flag home again).
        force_carry(&mut board, 1, 2001);
        let units = [unit(1002, 2, drop), unit(2001, 2, BASE2)];
        board.ctf_tick(1, &units);
        assert_eq!(board.score(2), 0, "own flag was away on tick one");
        board.ctf_tick(2, &units);
        assert_eq!(board.score(2), 1);
        let events = board.drain_events();
        assert!(events.iter().any(|e| matches!(
            e,
            RulesEvent::FlagReturned {
                tick: 1,
                flag_team: 2,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            RulesEvent::FlagCaptured {
                tick: 2,
                team: 2,
                flag_team: 1,
                score: 1,
                ..
            }
        )));
    }

    #[test]
    fn ctf_capture_limit_defaults_to_three_and_clamps_to_one() {
        assert_eq!(crate::objective::DEFAULT_CAPTURE_LIMIT, 3);
        assert_eq!(empty_board().settings.effective_capture_limit(), 3);
        let mut board = ObjectiveBoard::new(
            MatchMode::CaptureTheFlag,
            ObjectiveSettings {
                capture_limit: 0,
                ..ObjectiveSettings::default()
            },
        );
        board.spawn_flag(1, 1, BASE1).expect("flag1");
        board.spawn_flag(2, 2, BASE2).expect("flag2");
        force_carry(&mut board, 2, 1001);
        board.ctf_tick(1, &[unit(1001, 1, BASE1)]);
        assert!(board.match_over, "a zero capture limit reads as one");
    }

    #[test]
    fn ctf_other_modes_and_dead_or_neutral_units_touch_nothing() {
        let mut board = two_flags();
        board.mode = MatchMode::KingOfTheHill;
        board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
        assert!(flag(&board, 2).at_base, "not a CTF match");
        board.mode = MatchMode::CaptureTheFlag;
        let mut dead = unit(1001, 1, BASE2);
        dead.alive = false;
        board.ctf_tick(2, &[dead, unit(1002, 0, BASE2), unit(1003, 9, BASE2)]);
        assert!(
            flag(&board, 2).at_base,
            "dead, neutral and out-of-range units never touch"
        );
    }

    #[test]
    fn ctf_twin_runs_stay_identical() {
        fn run() -> ObjectiveBoard {
            let mut board = empty_board();
            board.place_flags(&[1, 2], FLAG_X, 10).expect("flags");
            // Cross-raiders: each starts on the enemy flag, grabs it and walks home.
            let mut a = [FLAG_X, 0.0, 0.0];
            let mut b = [-FLAG_X, 0.0, 0.0];
            for tick in 1..=600u64 {
                let step = 0.1 * (tick % 7) as f32 + 1.7;
                a[0] -= step;
                b[0] += step;
                a[2] = 0.25 * (tick % 13) as f32;
                board.ctf_tick(tick, &[unit(1001, 1, a), unit(1002, 2, b)]);
            }
            board
        }
        let (first, second) = (run(), run());
        assert_eq!(first, second);
        for (fa, fb) in first.objectives.iter().zip(&second.objectives) {
            for axis in 0..3 {
                assert_eq!(fa.position[axis].to_bits(), fb.position[axis].to_bits());
            }
        }
    }
}
