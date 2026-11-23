// SPDX-License-Identifier: MPL-2.0
//! Match phases: warm-up, countdown, live play, overtime, rounds with side swaps and the post-match
//! intermission (ADR-8485).
//!
//! [`PhaseSettings`] sits on the flow settings and is off by default: with every field off the
//! phase machine does nothing, emits nothing and the session keeps S-14's bits. A
//! [`PhaseState`] lives on the flow and is driven by the session once per step the match runs
//! (paused steps hold every phase clock). A host without a session (a sample with its own scores)
//! drives the same machine with [`PhaseState::start_match`], [`PhaseState::begin_tick`],
//! [`PhaseState::end_tick`] and [`PhaseState::enter_post_match`].
//!
//! Every clock is a tick count: the core never sees seconds. A host converts its setup values
//! with `regia_detsim::tick::ticks_from_secs` once, at setup. Everything here is integer data
//! stepped in a fixed order with `+ -` and comparisons, so every lockstep peer computes the same
//! phase, round and winner on the same step.
//!
//! # End conditions
//!
//! A match (or, with rounds on, a round) ends when the first condition of
//! [`END_CONDITION_ORDER`] fires on a tick. Two conditions that fire on the same tick never
//! combine: the earlier one in the list decides the winner and the reason.

use serde::{Deserialize, Serialize};

use super::RulesError;
use super::match_flow::SeatId;
use super::objective::{EndReason, RulesEvent};

/// Default hold between two rounds: 5 seconds at the default tick rate.
pub const DEFAULT_ROUND_END_TICKS: u64 = regia_detsim::tick::ticks_from_secs(5.0);

/// Where a match is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchPhase {
    /// Players gather. Nothing scores; the board is reset when the warm-up ends.
    Warmup,
    /// The short hold before live play (and before each later round). Nothing scores.
    Countdown,
    /// Live play: the objective rules run and the time limit counts. With every phase setting
    /// off the match is always here.
    #[default]
    Live,
    /// Live play after a tied time limit: the tie-break decides (see [`TimeUp`]).
    Overtime,
    /// The hold between two rounds. Nothing scores.
    RoundEnd,
    /// The match is decided; the intermission before the ready gate.
    PostMatch,
}

impl MatchPhase {
    /// Every phase, in declaration order.
    pub const ALL: [MatchPhase; 6] = [
        MatchPhase::Warmup,
        MatchPhase::Countdown,
        MatchPhase::Live,
        MatchPhase::Overtime,
        MatchPhase::RoundEnd,
        MatchPhase::PostMatch,
    ];

    /// The phase as a lowercase string (`"warmup"`, `"countdown"`, `"live"`, `"overtime"`,
    /// `"round_end"`, `"post_match"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warmup => "warmup",
            Self::Countdown => "countdown",
            Self::Live => "live",
            Self::Overtime => "overtime",
            Self::RoundEnd => "round_end",
            Self::PostMatch => "post_match",
        }
    }

    /// True for the phases in which the objective rules run and points count
    /// ([`MatchPhase::Live`] and [`MatchPhase::Overtime`]).
    pub fn is_play(self) -> bool {
        matches!(self, Self::Live | Self::Overtime)
    }
}

/// What happens when the time limit runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeUp {
    /// The match ends at once with [`EndReason::TimeLimit`]. The sole leader wins; a tie ends
    /// with team 0 (no winner).
    #[default]
    End,
    /// The sole leader wins with [`EndReason::TimeLimit`]; a tie ends as [`EndReason::Draw`]
    /// with team 0.
    Draw,
    /// The sole leader wins; a tie goes to overtime, where the first tick with a sole leader
    /// ends it. `ticks: Some(n)` caps the overtime; still tied at the cap is a draw.
    Overtime {
        /// Overtime cap in ticks. `None` plays on until the tie breaks.
        #[serde(default)]
        ticks: Option<u64>,
    },
    /// The sole leader wins; a tie goes to sudden death, where the next point by any team wins,
    /// even a team that was behind. Points on one tick by several teams go to the sole leader,
    /// or play on when they leave a tie.
    SuddenDeath,
}

/// Rounds: a match of several rounds, won by the first team to win `rounds_to_win` of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct RoundSettings {
    /// Round wins that win the match; `0` reads as `1`.
    pub rounds_to_win: u32,
    /// Most rounds played. When they are played without a winner, the most round wins wins
    /// ([`EndReason::RoundsWon`]); a tie is a draw. Below `rounds_to_win` reads as
    /// `rounds_to_win`.
    pub max_rounds: u32,
    /// Teams swap sides after every this many rounds; `0` never swaps.
    pub swap_sides_every: u32,
    /// Ticks held in [`MatchPhase::RoundEnd`] before the next round.
    pub round_end_ticks: u64,
}

impl Default for RoundSettings {
    fn default() -> Self {
        Self {
            rounds_to_win: 2,
            max_rounds: 3,
            swap_sides_every: 0,
            round_end_ticks: DEFAULT_ROUND_END_TICKS,
        }
    }
}

impl RoundSettings {
    /// Round wins that win the match (never below 1).
    pub fn effective_rounds_to_win(&self) -> u32 {
        self.rounds_to_win.max(1)
    }

    /// Most rounds played (never below [`RoundSettings::effective_rounds_to_win`]).
    pub fn effective_max_rounds(&self) -> u32 {
        self.max_rounds.max(self.effective_rounds_to_win())
    }
}

/// Phase settings of a match. Every field is off by default, which keeps S-14's behavior:
/// no warm-up, no countdown, no time limit, no rounds, no intermission, no forfeit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PhaseSettings {
    /// Warm-up length in match ticks. `None` with `warmup_min_seats` also `None`: no warm-up.
    pub warmup_ticks: Option<u64>,
    /// The warm-up also ends as soon as this many seated, non-observer seats are present.
    pub warmup_min_seats: Option<u8>,
    /// Countdown ticks before live play (after the warm-up and between rounds). `0` skips it.
    pub countdown_ticks: u64,
    /// Live ticks per match (or per round) before [`PhaseSettings::on_time_up`] applies.
    pub time_limit_ticks: Option<u64>,
    /// What the time limit does.
    pub on_time_up: TimeUp,
    /// Rounds. `None`: one match is one round.
    pub rounds: Option<RoundSettings>,
    /// Intermission ticks after the match is decided; then the ready gate opens
    /// (`MatchFlow::open_between`). `None` holds [`MatchPhase::PostMatch`] until the host acts.
    pub post_match_ticks: Option<u64>,
    /// A team with no seated, non-observer seat for this many ticks forfeits. `None`: never.
    pub forfeit_after_ticks: Option<u64>,
}

impl PhaseSettings {
    /// True when every phase setting is off: the phase machine is inert and the session keeps
    /// S-14's bits.
    pub fn is_off(&self) -> bool {
        *self == Self::default()
    }

    /// True when a warm-up runs at the start of each match.
    pub fn has_warmup(&self) -> bool {
        self.warmup_ticks.is_some() || self.warmup_min_seats.is_some()
    }

    /// Refuse settings that cannot run: a zero time limit, overtime cap, warm-up length or
    /// forfeit hold, and zero rounds. [`RulesError::PhaseSettings`] names the field.
    pub fn validate(&self) -> Result<(), RulesError> {
        let zero = |value: Option<u64>| value == Some(0);
        if zero(self.warmup_ticks) {
            return Err(RulesError::PhaseSettings {
                field: "warmup_ticks",
            });
        }
        if zero(self.time_limit_ticks) {
            return Err(RulesError::PhaseSettings {
                field: "time_limit_ticks",
            });
        }
        if let TimeUp::Overtime { ticks } = self.on_time_up
            && zero(ticks)
        {
            return Err(RulesError::PhaseSettings {
                field: "on_time_up.ticks",
            });
        }
        if zero(self.forfeit_after_ticks) {
            return Err(RulesError::PhaseSettings {
                field: "forfeit_after_ticks",
            });
        }
        if let Some(rounds) = self.rounds {
            if rounds.rounds_to_win == 0 {
                return Err(RulesError::PhaseSettings {
                    field: "rounds.rounds_to_win",
                });
            }
            if rounds.max_rounds < rounds.rounds_to_win {
                return Err(RulesError::PhaseSettings {
                    field: "rounds.max_rounds",
                });
            }
        }
        Ok(())
    }

    fn after_hold(&self) -> MatchPhase {
        if self.countdown_ticks > 0 {
            MatchPhase::Countdown
        } else {
            MatchPhase::Live
        }
    }

    fn opening_phase(&self) -> MatchPhase {
        if self.has_warmup() {
            MatchPhase::Warmup
        } else {
            self.after_hold()
        }
    }
}

/// A condition that can end a match (or a round), in evaluation order.
///
/// The genre kits and the series rules add conditions of their own: a host evaluates them and
/// reports the outcome with `MatchSession::report_condition` (or
/// [`PhaseState::report_condition`]) before the step, which lands first in the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCondition {
    /// A host-reported condition (a game or kit rule) since the last step.
    Reported,
    /// A score limit: the capture limit or the hill score limit of the objective board.
    ScoreLimit,
    /// A forfeit: one fighting team or none left after the absent teams forfeit.
    Forfeit,
    /// The time limit, with its overtime or sudden death.
    TimeLimit,
}

/// The fixed evaluation order of the end conditions. The first condition that fires on a tick
/// decides it; a later one firing on the same tick is ignored.
pub const END_CONDITION_ORDER: [EndCondition; 4] = [
    EndCondition::Reported,
    EndCondition::ScoreLimit,
    EndCondition::Forfeit,
    EndCondition::TimeLimit,
];

/// One side's base, recorded at the first side swap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SideBase {
    /// The side index.
    pub side: u8,
    /// The base position of that side.
    pub position: [f32; 3],
}

/// One team's presence, in team order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPresence {
    /// The team.
    pub team: u8,
    /// Consecutive counted ticks with no seated, non-observer seat.
    pub absent_ticks: u64,
}

/// A host-reported outcome waiting for the next tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedEnd {
    /// The winning team, `0` for none.
    pub team: u8,
    /// Why.
    pub reason: EndReason,
}

/// What [`PhaseState::begin_tick`] asks the host to do on this tick, in this order: swap sides,
/// reset the play state, open the ready gate, then run the objective rules when `play` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickPlan {
    /// Run the objective rules and count points on this tick.
    pub play: bool,
    /// Teams swapped sides: the host moves bases and spawns ([`PhaseState::side_of`]).
    pub swapped: bool,
    /// A warm-up ended or a new round starts: clear the scores and send the objectives home.
    pub reset_play: bool,
    /// The intermission ended: open the ready gate.
    pub open_between: bool,
}

/// What [`PhaseState::end_tick`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhaseEnd {
    /// The round that ended: its winner (`0` for none) and why.
    pub round: Option<(u8, EndReason)>,
    /// The match that ended: its winner (`0` for none) and why. The host ends its match with
    /// this, then calls [`PhaseState::enter_post_match`].
    pub matched: Option<(u8, EndReason)>,
}

/// The phase, round, clocks and round wins of the running match. Plain data on the flow.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PhaseState {
    /// The current phase.
    pub phase: MatchPhase,
    /// The current round, from 1, with rounds on; `0` with rounds off.
    pub round: u32,
    /// Ticks run in the current phase.
    pub phase_ticks: u64,
    /// Live ticks of this match (or this round): the time limit clock.
    pub clock_ticks: u64,
    /// Overtime ticks of this match (or this round).
    pub overtime_ticks: u64,
    /// Rounds played to an end in this match.
    pub rounds_played: u32,
    /// Round wins per team id (index 0 is neutral).
    pub round_wins: Vec<u32>,
    /// Side index per team id. A team past the end plays on its own side (`side == team`).
    pub sides: Vec<u8>,
    /// Bases per side, recorded at the first side swap, in side order.
    pub side_bases: Vec<SideBase>,
    /// Teams seen with a seated, non-observer seat in this match, in team order.
    pub teams: Vec<TeamPresence>,
    /// Teams that forfeited, in team order.
    pub forfeited: Vec<u8>,
    /// Team scores when overtime began (sudden death compares against them).
    pub overtime_from: Vec<u32>,
    /// A host-reported outcome for the next tick.
    pub reported: Option<ReportedEnd>,
    /// The match is decided.
    pub decided: bool,
}

impl PhaseState {
    /// The side `team` plays on now. Spawns and objective placement read this.
    pub fn side_of(&self, team: u8) -> u8 {
        self.sides.get(usize::from(team)).copied().unwrap_or(team)
    }

    /// Round wins of `team`.
    pub fn round_wins_of(&self, team: u8) -> u32 {
        self.round_wins.get(usize::from(team)).copied().unwrap_or(0)
    }

    /// Ticks left on the time limit (or on a capped overtime). `None` when no clock runs down.
    pub fn time_left(&self, settings: &PhaseSettings) -> Option<u64> {
        if self.phase == MatchPhase::Overtime {
            return match settings.on_time_up {
                TimeUp::Overtime { ticks: Some(cap) } => Some(cap.saturating_sub(self.overtime_ticks)),
                _ => None,
            };
        }
        settings
            .time_limit_ticks
            .map(|limit| limit.saturating_sub(self.clock_ticks))
    }

    /// Report a condition the host evaluated (a game or kit rule): it ends the round (rounds on)
    /// or the match on the next counted tick, first in [`END_CONDITION_ORDER`]. A later report
    /// before that tick replaces this one. [`EndReason::Ended`] and [`EndReason::Forfeit`]
    /// always end the match.
    pub fn report_condition(&mut self, team: u8, reason: EndReason) {
        self.reported = Some(ReportedEnd { team, reason });
    }

    /// Start a match: clear the state and enter the opening phase (warm-up, countdown or live).
    /// Emits [`RulesEvent::PhaseChanged`] unless every setting is off.
    pub fn start_match(&mut self, settings: &PhaseSettings, step: u64, events: &mut Vec<RulesEvent>) {
        *self = Self::default();
        if settings.is_off() {
            return;
        }
        if settings.rounds.is_some() {
            self.round = 1;
        }
        self.enter(settings.opening_phase(), step, events);
    }

    /// Count one tick the match ran, before the objective rules. `seats` are the seated,
    /// non-observer seats in seat order; `teams_on_board` are extra teams the side swap rotates
    /// (for example the teams that own a flag), in any order.
    pub fn begin_tick(
        &mut self,
        settings: &PhaseSettings,
        step: u64,
        seats: &[SeatId],
        teams_on_board: &[u8],
        events: &mut Vec<RulesEvent>,
    ) -> TickPlan {
        let mut plan = TickPlan::default();
        if settings.is_off() {
            plan.play = true;
            return plan;
        }
        self.phase_ticks = self.phase_ticks.saturating_add(1);
        self.note_presence(settings, seats);
        match self.phase {
            MatchPhase::Warmup => {
                let by_ticks = settings.warmup_ticks.is_some_and(|t| self.phase_ticks >= t);
                let by_seats = settings
                    .warmup_min_seats
                    .is_some_and(|min| seats.len() >= usize::from(min));
                if by_ticks || by_seats {
                    plan.reset_play = true;
                    self.enter(settings.after_hold(), step, events);
                }
            }
            MatchPhase::Countdown => {
                if self.phase_ticks >= settings.countdown_ticks {
                    self.enter(MatchPhase::Live, step, events);
                }
            }
            MatchPhase::Live => {
                self.clock_ticks = self.clock_ticks.saturating_add(1);
                plan.play = true;
            }
            MatchPhase::Overtime => {
                self.overtime_ticks = self.overtime_ticks.saturating_add(1);
                plan.play = true;
            }
            MatchPhase::RoundEnd => {
                let hold = settings.rounds.map(|r| r.round_end_ticks).unwrap_or(0);
                if self.phase_ticks >= hold {
                    let every = settings.rounds.map(|r| r.swap_sides_every).unwrap_or(0);
                    if every > 0 && self.rounds_played.is_multiple_of(every) {
                        plan.swapped = self.swap_sides(seats, teams_on_board);
                    }
                    self.round = self.round.saturating_add(1);
                    self.clock_ticks = 0;
                    self.overtime_ticks = 0;
                    self.overtime_from.clear();
                    plan.reset_play = true;
                    self.enter(settings.after_hold(), step, events);
                }
            }
            MatchPhase::PostMatch => {
                if settings.post_match_ticks.is_some_and(|t| self.phase_ticks >= t) {
                    plan.open_between = true;
                }
            }
        }
        plan
    }

    /// Evaluate the end conditions after the objective rules of a play tick, in
    /// [`END_CONDITION_ORDER`]. `scores` are the team scores (index = team); `score_limit` is the
    /// winner and reason when a score limit fired on this tick.
    pub fn end_tick(
        &mut self,
        settings: &PhaseSettings,
        step: u64,
        tick: u64,
        scores: &[u32],
        score_limit: Option<(u8, EndReason)>,
        events: &mut Vec<RulesEvent>,
    ) -> PhaseEnd {
        if settings.is_off() || self.decided || !self.phase.is_play() {
            return PhaseEnd::default();
        }
        let reported = self.reported.take().map(|r| (r.team, r.reason));
        let decision = reported
            .or(score_limit)
            .or_else(|| self.forfeit_outcome(settings))
            .or_else(|| self.time_outcome(settings, step, scores, events));
        let Some((team, reason)) = decision else {
            return PhaseEnd::default();
        };
        let Some(rounds) = settings.rounds else {
            return PhaseEnd {
                round: None,
                matched: Some((team, reason)),
            };
        };
        if matches!(
            reason,
            EndReason::Ended | EndReason::Forfeit | EndReason::RoundsWon
        ) {
            return PhaseEnd {
                round: None,
                matched: Some((team, reason)),
            };
        }
        self.rounds_played = self.rounds_played.saturating_add(1);
        if team != 0 {
            let index = usize::from(team);
            if self.round_wins.len() <= index {
                self.round_wins.resize(index + 1, 0);
            }
            self.round_wins[index] = self.round_wins[index].saturating_add(1);
        }
        events.push(RulesEvent::RoundOver {
            tick,
            round: self.round,
            team,
            reason,
        });
        let matched = if team != 0 && self.round_wins_of(team) >= rounds.effective_rounds_to_win() {
            Some((team, EndReason::RoundsWon))
        } else if self.rounds_played >= rounds.effective_max_rounds() {
            Some(match sole_leader(&self.round_wins) {
                Some(leader) => (leader, EndReason::RoundsWon),
                None => (0, EndReason::Draw),
            })
        } else {
            None
        };
        if matched.is_none() {
            self.enter(MatchPhase::RoundEnd, step, events);
        }
        PhaseEnd {
            round: Some((team, reason)),
            matched,
        }
    }

    /// The match is decided: enter [`MatchPhase::PostMatch`]. Call after ending the match with
    /// [`PhaseEnd::matched`], or when the host ended it some other way. Does nothing when every
    /// setting is off or the match is already decided.
    pub fn enter_post_match(&mut self, settings: &PhaseSettings, step: u64, events: &mut Vec<RulesEvent>) {
        if settings.is_off() || self.decided {
            return;
        }
        self.decided = true;
        self.enter(MatchPhase::PostMatch, step, events);
    }

    /// Record the bases of each side from `(team, base)` pairs. The first swap does this from the
    /// board; a host with its own bases may call it before the first swap.
    pub fn record_side_bases(&mut self, bases: &[(u8, [f32; 3])]) {
        if !self.side_bases.is_empty() {
            return;
        }
        for (team, position) in bases {
            let side = self.side_of(*team);
            if self.side_bases.iter().all(|b| b.side != side) {
                self.side_bases.push(SideBase {
                    side,
                    position: *position,
                });
            }
        }
        self.side_bases.sort_by_key(|b| b.side);
    }

    /// The recorded base of the side `team` plays on now.
    pub fn base_of(&self, team: u8) -> Option<[f32; 3]> {
        let side = self.side_of(team);
        self.side_bases
            .iter()
            .find(|b| b.side == side)
            .map(|b| b.position)
    }

    fn enter(&mut self, phase: MatchPhase, step: u64, events: &mut Vec<RulesEvent>) {
        self.phase = phase;
        self.phase_ticks = 0;
        events.push(RulesEvent::PhaseChanged {
            step,
            phase,
            round: self.round,
        });
    }

    /// Track which teams are present. A team with a seat joins the match; an absent team counts
    /// up outside the warm-up and the intermission and forfeits at the setting.
    fn note_presence(&mut self, settings: &PhaseSettings, seats: &[SeatId]) {
        for seat in seats {
            match self.teams.binary_search_by_key(&seat.team, |t| t.team) {
                Ok(index) => self.teams[index].absent_ticks = 0,
                Err(index) => self.teams.insert(
                    index,
                    TeamPresence {
                        team: seat.team,
                        absent_ticks: 0,
                    },
                ),
            }
        }
        let counting = !matches!(self.phase, MatchPhase::Warmup | MatchPhase::PostMatch);
        let Some(limit) = settings.forfeit_after_ticks else {
            return;
        };
        if !counting {
            return;
        }
        for presence in &mut self.teams {
            if seats.iter().any(|s| s.team == presence.team) {
                continue;
            }
            presence.absent_ticks = presence.absent_ticks.saturating_add(1);
            if presence.absent_ticks >= limit
                && let Err(index) = self.forfeited.binary_search(&presence.team)
            {
                self.forfeited.insert(index, presence.team);
            }
        }
    }

    fn forfeit_outcome(&self, settings: &PhaseSettings) -> Option<(u8, EndReason)> {
        settings.forfeit_after_ticks?;
        if self.forfeited.is_empty() {
            return None;
        }
        let mut left = self
            .teams
            .iter()
            .map(|t| t.team)
            .filter(|team| self.forfeited.binary_search(team).is_err());
        let first = left.next();
        if left.next().is_some() {
            return None;
        }
        Some((first.unwrap_or(0), EndReason::Forfeit))
    }

    fn time_outcome(
        &mut self,
        settings: &PhaseSettings,
        step: u64,
        scores: &[u32],
        events: &mut Vec<RulesEvent>,
    ) -> Option<(u8, EndReason)> {
        if self.phase == MatchPhase::Overtime {
            return self.overtime_outcome(settings, scores);
        }
        let limit = settings.time_limit_ticks?;
        if self.clock_ticks < limit {
            return None;
        }
        if let Some(leader) = sole_leader(scores) {
            return Some((leader, EndReason::TimeLimit));
        }
        match settings.on_time_up {
            TimeUp::End => Some((0, EndReason::TimeLimit)),
            TimeUp::Draw => Some((0, EndReason::Draw)),
            TimeUp::Overtime { .. } | TimeUp::SuddenDeath => {
                self.overtime_ticks = 0;
                self.overtime_from = scores.to_vec();
                self.enter(MatchPhase::Overtime, step, events);
                None
            }
        }
    }

    fn overtime_outcome(&mut self, settings: &PhaseSettings, scores: &[u32]) -> Option<(u8, EndReason)> {
        match settings.on_time_up {
            TimeUp::SuddenDeath => {
                let mut scorers = scores
                    .iter()
                    .enumerate()
                    .skip(1)
                    .filter(|(team, score)| **score > self.overtime_from.get(*team).copied().unwrap_or(0))
                    .map(|(team, _)| team);
                let first = scorers.next()?;
                if scorers.next().is_none() {
                    return u8::try_from(first).ok().map(|team| (team, EndReason::TimeLimit));
                }
                match sole_leader(scores) {
                    Some(leader) => Some((leader, EndReason::TimeLimit)),
                    None => {
                        self.overtime_from = scores.to_vec();
                        None
                    }
                }
            }
            TimeUp::Overtime { ticks } => {
                if let Some(leader) = sole_leader(scores) {
                    return Some((leader, EndReason::TimeLimit));
                }
                if ticks.is_some_and(|cap| self.overtime_ticks >= cap) {
                    return Some((0, EndReason::Draw));
                }
                None
            }
            TimeUp::End | TimeUp::Draw => None,
        }
    }

    /// Rotate the sides of the present teams (seated teams and `extra`, sorted): each team takes
    /// the side the next one held, the last takes the first's. Two teams swap. Fewer than two:
    /// nothing moves. Returns whether the sides moved.
    fn swap_sides(&mut self, seats: &[SeatId], extra: &[u8]) -> bool {
        let mut teams: Vec<u8> = self.teams.iter().map(|t| t.team).collect();
        teams.extend(seats.iter().map(|s| s.team));
        teams.extend(extra.iter().copied());
        teams.retain(|team| *team != 0);
        teams.sort_unstable();
        teams.dedup();
        if teams.len() < 2 {
            return false;
        }
        let old: Vec<u8> = teams.iter().map(|team| self.side_of(*team)).collect();
        let top = teams.iter().copied().max().unwrap_or(0);
        let len = usize::from(top) + 1;
        if self.sides.len() < len {
            let start = self.sides.len();
            for team in start..len {
                self.sides.push(u8::try_from(team).unwrap_or(u8::MAX));
            }
        }
        for (i, team) in teams.iter().enumerate() {
            let next = if i + 1 == teams.len() { 0 } else { i + 1 };
            self.sides[usize::from(*team)] = old[next];
        }
        true
    }
}

/// The team with the strictly highest score (index = team, index 0 skipped), or `None` on a tie
/// for the top.
pub fn sole_leader(scores: &[u32]) -> Option<u8> {
    let mut best: Option<(usize, u32)> = None;
    let mut tied = false;
    for (team, score) in scores.iter().enumerate().skip(1) {
        match best {
            None => best = Some((team, *score)),
            Some((_, top)) if *score > top => {
                best = Some((team, *score));
                tied = false;
            }
            Some((_, top)) if *score == top => tied = true,
            Some(_) => {}
        }
    }
    match best {
        Some((team, _)) if !tied => u8::try_from(team).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_off_and_inert() {
        let settings = PhaseSettings::default();
        assert!(settings.is_off());
        let mut state = PhaseState::default();
        let mut events = Vec::new();
        state.start_match(&settings, 0, &mut events);
        let plan = state.begin_tick(&settings, 0, &[], &[], &mut events);
        assert!(plan.play);
        let end = state.end_tick(&settings, 0, 1, &[0, 5, 0], None, &mut events);
        assert_eq!(end, PhaseEnd::default());
        assert!(events.is_empty());
        assert_eq!(state, PhaseState::default());
    }

    #[test]
    fn sole_leader_skips_neutral_and_refuses_ties() {
        assert_eq!(sole_leader(&[9, 1, 3, 2]), Some(2));
        assert_eq!(sole_leader(&[9, 3, 3, 2]), None);
        assert_eq!(sole_leader(&[0, 0, 0]), None);
        assert_eq!(sole_leader(&[0]), None);
        assert_eq!(sole_leader(&[0, 4]), Some(1));
    }

    #[test]
    fn validation_names_the_field() {
        let bad = PhaseSettings {
            time_limit_ticks: Some(0),
            ..PhaseSettings::default()
        };
        assert_eq!(
            bad.validate(),
            Err(RulesError::PhaseSettings {
                field: "time_limit_ticks"
            })
        );
        let rounds = PhaseSettings {
            rounds: Some(RoundSettings {
                rounds_to_win: 3,
                max_rounds: 2,
                ..RoundSettings::default()
            }),
            ..PhaseSettings::default()
        };
        assert_eq!(
            rounds.validate().map_err(|e| e.code()),
            Err("match_phase_settings")
        );
        assert!(PhaseSettings::default().validate().is_ok());
    }

    #[test]
    fn three_teams_rotate_sides() {
        let mut state = PhaseState::default();
        let seats = [SeatId::new(1, 0), SeatId::new(2, 0), SeatId::new(3, 0)];
        assert!(state.swap_sides(&seats, &[]));
        assert_eq!([state.side_of(1), state.side_of(2), state.side_of(3)], [2, 3, 1]);
        assert_eq!(state.side_of(4), 4, "an absent team keeps its own side");
        assert!(!PhaseState::default().swap_sides(&seats[..1], &[]));
    }

    #[test]
    fn phase_strings_are_lowercase_and_unique() {
        let names: Vec<&str> = MatchPhase::ALL.iter().map(|p| p.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        for (phase, name) in MatchPhase::ALL.iter().zip(&names) {
            let json = serde_json::to_string(phase).expect("serialize");
            assert_eq!(json, format!("\"{name}\""));
        }
    }
}
