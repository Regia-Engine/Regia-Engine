// SPDX-License-Identifier: MPL-2.0
//! Teams, seat assignment and the per-seat score ledger (ADR-8500).
//!
//! Plain data, stepped with integer arithmetic only. Team scores on the objective board stay the
//! S-14 counters; a capture or a hill point also credits the bound carrier or occupant here and
//! does not write those counters again. A later end-of-match stats record can adopt [`PlayerLedger`]
//! instead of keeping a second one.
//!
//! Assignment walks team definitions and seated rows as sorted vectors: fewest occupied seats, then
//! the lowest team score, then the lowest team id. A party shares one team when some team has room
//! for every member.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::RulesError;
use super::match_flow::{DEFAULT_SEATS_PER_TEAM, SeatId};
use super::objective::{DEFAULT_MAX_TEAMS, RulesEvent, ScoreReason};
use super::session::MatchSession;

/// Distinct named counters one ledger row holds. A further new name is [`RulesError::StatCap`]
/// (`match_stat_cap`).
pub const MAX_NAMED_COUNTERS: usize = 16;

/// Longest named-counter key, in bytes. A longer or empty name is [`RulesError::StatCap`].
pub const MAX_COUNTER_NAME_BYTES: usize = 32;

/// Damage contributors remembered per victim. The oldest (lowest tick, then lowest seat) is dropped.
pub const MAX_CONTRIBUTORS: usize = 8;

/// Default assist window: five seconds at the default tick rate. A contributor whose hit is older
/// than this many match ticks does not assist.
pub const DEFAULT_ASSIST_WINDOW_TICKS: u64 = 300;

/// Points added to [`PlayerLedger::score`] for one kill. Objective points add the same amount, one
/// per capture or hill point credited to that seat. Assists and deaths do not change `score`.
pub const KILL_SCORE: i64 = 1;

const DEFAULT_COLORS: [[u8; 4]; 4] = [
    [196, 64, 64, 255],
    [64, 112, 196, 255],
    [64, 168, 96, 255],
    [196, 168, 64, 255],
];

/// One fighting team: a stable id, a localization key, a presentation color and a seat cap.
///
/// Team 0 is neutral and has no definition. The default list is S-14's four unnamed teams
/// (`1..=4`), each with [`DEFAULT_SEATS_PER_TEAM`] seats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDef {
    /// Fighting team id (`1..=max_teams`).
    pub id: u8,
    /// Localization key for the heading. Empty means the team is unnamed.
    pub name_key: String,
    /// Straight RGBA, 0..=255 per channel. The score sheet scales this to unit floats for display.
    pub color: [u8; 4],
    /// Seats this team accepts. A party larger than this never lands here.
    pub max_seats: u8,
}

impl TeamDef {
    /// Team `id` with an empty name, `color` and `max_seats`.
    pub fn unnamed(id: u8, color: [u8; 4], max_seats: u8) -> Self {
        Self {
            id,
            name_key: String::new(),
            color,
            max_seats,
        }
    }
}

/// The four unnamed teams S-14 scored (`1..=4`), in id order.
pub fn default_team_defs() -> Vec<TeamDef> {
    (1..=DEFAULT_MAX_TEAMS)
        .map(|id| {
            let color = DEFAULT_COLORS[usize::from(id - 1) % DEFAULT_COLORS.len()];
            TeamDef::unnamed(id, color, DEFAULT_SEATS_PER_TEAM)
        })
        .collect()
}

/// When a player who already has a seat may move to another team.
///
/// The default is [`TeamSwitch::Never`]: explicit [`super::match_flow::FlowInput::Seat`] calls
/// keep working, and a team change is refused. New seats (a player who is not seated yet) are not
/// a switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "policy", content = "n", rename_all = "snake_case")]
pub enum TeamSwitch {
    /// Refuse every team change.
    #[default]
    Never,
    /// Allow a team change only while the flow is between matches.
    BetweenMatches,
    /// Allow a team change when the source team has at least `n` more seated players than the
    /// destination, live or between matches.
    WhenUnbalancedBy(u8),
}

/// Team list and the switches that may move a seated player. Stored on the session settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamRules {
    /// Fighting teams, consulted in id order. The default is [`default_team_defs`].
    pub defs: Vec<TeamDef>,
    /// When a seated player may change team. Default [`TeamSwitch::Never`].
    pub switch: TeamSwitch,
    /// When true, a match launch moves parties off the fullest teams until counts differ by at most
    /// one, without splitting a party. Default false (seats stay where they were put).
    pub rebalance_between_rounds: bool,
    /// Match ticks after a hit during which that seat can assist a kill. Default
    /// [`DEFAULT_ASSIST_WINDOW_TICKS`].
    pub assist_window_ticks: u64,
}

impl Default for TeamRules {
    fn default() -> Self {
        Self {
            defs: default_team_defs(),
            switch: TeamSwitch::Never,
            rebalance_between_rounds: false,
            assist_window_ticks: DEFAULT_ASSIST_WINDOW_TICKS,
        }
    }
}

/// One seat's match record. Arithmetic is saturating integer addition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerLedger {
    /// Team.
    pub team: u8,
    /// Slot on that team.
    pub slot: u8,
    /// The contestant id bound to this seat, when the host has bound one.
    pub unit: Option<u32>,
    /// Party id from the assign call that seated this player. `0` is a player who was seated alone
    /// (an explicit seat, or an auto-assign with an empty group).
    pub party: u64,
    /// Kills this match.
    pub kills: u32,
    /// Deaths this match.
    pub deaths: u32,
    /// Assists this match.
    pub assists: u32,
    /// Capture and hill points credited to this seat. They are not added to the team-score vector.
    pub objective_points: u32,
    /// Ranking total: one per kill and one per objective point.
    pub score: i64,
    /// Named counters, at most [`MAX_NAMED_COUNTERS`], keyed in order.
    pub counters: BTreeMap<String, i64>,
    /// This row's id, set by [`LedgerBook::ensure`] when the row opens: `generation << 16 |
    /// team << 8 | slot`, where `generation` counts the rows that coordinate has opened
    /// ([`LedgerBook::generations`], from 1). Never `0` for a row the book opened, never reused
    /// within a session, and the same on every peer whatever order one step's seat inputs arrived
    /// in. A team change or a slot move keeps it; a leave and a rejoin get a larger one. `0` is a
    /// row built by [`PlayerLedger::new`] alone or decoded from a session saved before ids existed
    /// (ADR-8657).
    #[serde(default)]
    pub id: u64,
}

impl PlayerLedger {
    /// A zeroed row for `seat`.
    pub fn new(seat: SeatId) -> Self {
        Self {
            team: seat.team,
            slot: seat.slot,
            unit: None,
            party: 0,
            kills: 0,
            deaths: 0,
            assists: 0,
            objective_points: 0,
            score: 0,
            counters: BTreeMap::new(),
            id: 0,
        }
    }

    /// The seat this row belongs to.
    pub fn seat(&self) -> SeatId {
        SeatId::new(self.team, self.slot)
    }
}

/// One damaging hit remembered for an assist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Who hit.
    pub seat: SeatId,
    /// Match tick of the latest hit from that seat.
    pub tick: u64,
}

/// Contributors toward one victim, capped at [`MAX_CONTRIBUTORS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionList {
    /// The victim.
    pub seat: SeatId,
    /// Hits, ordered by tick then seat.
    pub hits: Vec<Hit>,
}

/// One seated player moved by [`MatchSession::assign_team`] or by the launch rebalance.
///
/// The session appends these to [`MatchSession::seat_moves`] so a host that tags entities with a
/// team and a slot (the framework's `TeamId` and `LedgerSlot`) can retag them in the same step,
/// and damage keeps crediting the moved row (ADR-8657).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatMove {
    /// The seat the player left.
    pub from: SeatId,
    /// The seat the player now holds.
    pub to: SeatId,
    /// Moves with the same batch happened at once (one `assign_team` call, so a swap is two
    /// moves of one batch). Batches count up from `0` in the order they happened.
    pub batch: u32,
}

impl MatchSession {
    fn next_move_batch(&self) -> u32 {
        self.seat_moves
            .last()
            .map_or(0, |last| last.batch.saturating_add(1))
    }
}

/// What one [`super::session::MatchSession::assign_team`] call places.
///
/// `team` and `slot` set to `None` ask the session to choose. `from` set means this player already
/// has that seat and is moving; a team change then follows [`TeamSwitch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeatRequest {
    /// The seat to leave, when this is a move.
    pub from: Option<SeatId>,
    /// Preferred team. `None` asks for automatic assignment.
    pub team: Option<u8>,
    /// Preferred slot. `None` takes the lowest free slot on the chosen team.
    pub slot: Option<u8>,
    /// Contestant id to bind on the new row.
    pub unit: Option<u32>,
}

/// Every seat's ledger and the assist lists, both kept in seat order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LedgerBook {
    /// One row per seated player, sorted by team then slot.
    pub rows: Vec<PlayerLedger>,
    /// Per-victim contributor lists, sorted by the victim seat.
    pub contributions: Vec<ContributionList>,
    /// Rows each coordinate has opened, sorted by seat. Kept after a row is dropped, so a seat's
    /// next row gets a new [`PlayerLedger::id`].
    #[serde(default)]
    pub generations: Vec<SeatGeneration>,
}

/// How many ledger rows one coordinate has opened ([`LedgerBook::generations`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatGeneration {
    /// The coordinate.
    pub seat: SeatId,
    /// Rows opened there so far.
    pub opened: u32,
}

impl LedgerBook {
    /// The row for `seat`.
    pub fn row(&self, seat: SeatId) -> Option<&PlayerLedger> {
        let index = self.row_index(seat)?;
        self.rows.get(index)
    }

    /// The row for `seat`, for mutation.
    pub fn row_mut(&mut self, seat: SeatId) -> Option<&mut PlayerLedger> {
        let index = self.row_index(seat)?;
        self.rows.get_mut(index)
    }

    /// Insert a zeroed row when `seat` has none. Returns the row's index.
    ///
    /// A new row gets the next [`PlayerLedger::id`]; an existing row is returned as it is.
    pub fn ensure(&mut self, seat: SeatId) -> usize {
        match self.rows.binary_search_by_key(&seat, PlayerLedger::seat) {
            Ok(index) => index,
            Err(index) => {
                let generation = self.bump_generation(seat);
                let mut row = PlayerLedger::new(seat);
                row.id = (u64::from(generation) << 16) | (u64::from(seat.team) << 8) | u64::from(seat.slot);
                self.rows.insert(index, row);
                index
            }
        }
    }

    /// Replace `seat`'s row, contributor list and landed hits with a fresh zero row (a new id).
    /// Returns the row's index. A new occupant calls this so nothing a credit left on the
    /// unseated coordinate carries over (ADR-8657).
    pub fn reopen(&mut self, seat: SeatId) -> usize {
        self.remove(seat);
        self.ensure(seat)
    }

    /// Drop `seat`'s row, its contributor list, and any hit it landed.
    pub fn remove(&mut self, seat: SeatId) {
        if let Ok(index) = self.rows.binary_search_by_key(&seat, PlayerLedger::seat) {
            self.rows.remove(index);
        }
        self.contributions.retain(|list| list.seat != seat);
        for list in &mut self.contributions {
            list.hits.retain(|hit| hit.seat != seat);
        }
    }

    /// Bind `unit` to `seat`, clearing that unit from any other row.
    pub fn bind_unit(&mut self, seat: SeatId, unit: u32) -> Result<(), RulesError> {
        for row in &mut self.rows {
            if row.unit == Some(unit) && row.seat() != seat {
                row.unit = None;
            }
        }
        let index = self.row_index(seat).ok_or(RulesError::SeatMissing)?;
        self.rows[index].unit = Some(unit);
        Ok(())
    }

    /// Move the row, its party id and its pending hits from `from` to `to`.
    ///
    /// A destination that already has a row keeps it: nothing moves, and the source row stays
    /// where it was (ADR-8657). [`MatchSession::assign_team`] refuses such a move before it gets
    /// here.
    pub fn rekey(&mut self, from: SeatId, to: SeatId, party: u64) {
        self.rekey_many(&[(from, to)], party);
    }

    /// Zero the match totals. Seats, unit bindings and parties stay.
    pub fn reset_match_stats(&mut self) {
        for row in &mut self.rows {
            row.kills = 0;
            row.deaths = 0;
            row.assists = 0;
            row.objective_points = 0;
            row.score = 0;
            row.counters.clear();
        }
        self.contributions.clear();
    }

    /// Add `delta` to the named counter on `seat`.
    pub fn add_counter(&mut self, seat: SeatId, name: &str, delta: i64) -> Result<(), RulesError> {
        if name.is_empty() || name.len() > MAX_COUNTER_NAME_BYTES {
            return Err(RulesError::StatCap {
                cap: MAX_NAMED_COUNTERS,
            });
        }
        let index = self.row_index(seat).ok_or(RulesError::SeatMissing)?;
        let counters = &mut self.rows[index].counters;
        if !counters.contains_key(name) && counters.len() >= MAX_NAMED_COUNTERS {
            return Err(RulesError::StatCap {
                cap: MAX_NAMED_COUNTERS,
            });
        }
        let entry = counters.entry(name.to_string()).or_insert(0);
        *entry = entry.saturating_add(delta);
        Ok(())
    }

    /// Remember that `source` hit `victim` at `tick`. A hit on one's own seat is ignored.
    pub fn note_hit(&mut self, victim: SeatId, source: SeatId, tick: u64) {
        if victim == source {
            return;
        }
        let index = match self.contributions.binary_search_by_key(&victim, |list| list.seat) {
            Ok(index) => index,
            Err(index) => {
                self.contributions.insert(
                    index,
                    ContributionList {
                        seat: victim,
                        hits: Vec::new(),
                    },
                );
                index
            }
        };
        let hits = &mut self.contributions[index].hits;
        if let Some(existing) = hits.iter_mut().find(|hit| hit.seat == source) {
            if tick >= existing.tick {
                existing.tick = tick;
            }
        } else {
            hits.push(Hit { seat: source, tick });
            if hits.len() > MAX_CONTRIBUTORS
                && let Some(drop_at) = hits
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, hit)| (hit.tick, hit.seat))
                    .map(|(index, _)| index)
            {
                hits.remove(drop_at);
            }
        }
        hits.sort_by_key(|hit| (hit.tick, hit.seat));
    }

    /// Credit a kill: the killer's kill and score, the victim's death, and one assist for every
    /// other contributor whose latest hit is inside `window` ticks.
    pub fn credit_kill(
        &mut self,
        killer: SeatId,
        victim: SeatId,
        tick: u64,
        window: u64,
        events: &mut Vec<RulesEvent>,
    ) {
        self.credit_kill_where(killer, victim, tick, window, events, |_| true);
    }

    /// [`LedgerBook::credit_kill`] that books (and opens a row for) only the seats `credit`
    /// accepts. The victim's pending hits are dropped either way (ADR-8881).
    pub fn credit_kill_where<F>(
        &mut self,
        killer: SeatId,
        victim: SeatId,
        tick: u64,
        window: u64,
        events: &mut Vec<RulesEvent>,
        credit: F,
    ) where
        F: Fn(SeatId) -> bool,
    {
        if killer == victim {
            return;
        }
        if credit(killer) {
            self.ensure(killer);
            if let Some(row) = self.row_mut(killer) {
                row.kills = row.kills.saturating_add(1);
                row.score = row.score.saturating_add(KILL_SCORE);
                events.push(scored(tick, row, ScoreReason::Kill));
            }
        }
        if credit(victim) {
            self.ensure(victim);
            if let Some(row) = self.row_mut(victim) {
                row.deaths = row.deaths.saturating_add(1);
                events.push(scored(tick, row, ScoreReason::Death));
            }
        }
        let assisters = self
            .contributions
            .iter()
            .find(|list| list.seat == victim)
            .map(|list| {
                list.hits
                    .iter()
                    .filter(|hit| hit.seat != killer && hit.seat != victim)
                    .filter(|hit| hit.tick <= tick && tick - hit.tick <= window)
                    .map(|hit| hit.seat)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.contributions.retain(|list| list.seat != victim);
        for seat in assisters {
            if !credit(seat) {
                continue;
            }
            self.ensure(seat);
            if let Some(row) = self.row_mut(seat) {
                row.assists = row.assists.saturating_add(1);
                events.push(scored(tick, row, ScoreReason::Assist));
            }
        }
    }

    /// Credit one objective point to the row bound to `unit`. Does nothing when no row is bound,
    /// so an unbound capture leaves the ledger and the event list alone.
    pub fn credit_objective_unit(&mut self, unit: u32, tick: u64, events: &mut Vec<RulesEvent>) {
        let Some(index) = self.rows.iter().position(|row| row.unit == Some(unit)) else {
            return;
        };
        let row = &mut self.rows[index];
        row.objective_points = row.objective_points.saturating_add(1);
        row.score = row.score.saturating_add(KILL_SCORE);
        events.push(scored(tick, row, ScoreReason::Objective));
    }

    fn bump_generation(&mut self, seat: SeatId) -> u32 {
        match self.generations.binary_search_by_key(&seat, |entry| entry.seat) {
            Ok(index) => {
                let entry = &mut self.generations[index];
                entry.opened = entry.opened.saturating_add(1);
                entry.opened
            }
            Err(index) => {
                self.generations.insert(index, SeatGeneration { seat, opened: 1 });
                1
            }
        }
    }

    fn row_index(&self, seat: SeatId) -> Option<usize> {
        self.rows.binary_search_by_key(&seat, PlayerLedger::seat).ok()
    }
}

fn scored(tick: u64, row: &PlayerLedger, reason: ScoreReason) -> RulesEvent {
    RulesEvent::PlayerScored {
        tick,
        team: row.team,
        slot: row.slot,
        reason,
        score: row.score,
    }
}

/// One planned seat from [`MatchSession::assign_team`].
struct Placement {
    from: Option<SeatId>,
    dest: SeatId,
    unit: Option<u32>,
}

impl MatchSession {
    /// Remember that `source` hit `victim` on the current match tick. A hit on one's own seat is
    /// ignored, and so is a hit on or by a vacated coordinate ([`MatchSession::vacated`]).
    pub fn note_hit(&mut self, victim: SeatId, source: SeatId) {
        if !self.credits_seat(victim) || !self.credits_seat(source) {
            return;
        }
        let tick = self.flow.match_tick;
        self.ledgers.note_hit(victim, source, tick);
    }

    /// Credit a kill on the current match tick, using [`TeamRules::assist_window_ticks`].
    ///
    /// A vacated coordinate ([`MatchSession::vacated`]) gets nothing and opens no row: a departed
    /// killer's kill is not booked (the victim's death still is), and a departed victim's death is
    /// not booked (the killer's kill still is). Assists go to credited contributors only.
    pub fn credit_kill(&mut self, killer: SeatId, victim: SeatId) {
        let tick = self.flow.match_tick;
        let window = self.teams.assist_window_ticks;
        let credited =
            |seat: SeatId| self.flow.seated.contains(&seat) || self.vacated.binary_search(&seat).is_err();
        let (killer_ok, victim_ok) = (credited(killer), credited(victim));
        let assisters_ok: Vec<SeatId> = self
            .ledgers
            .contributions
            .iter()
            .filter(|list| list.seat == victim)
            .flat_map(|list| list.hits.iter().map(|hit| hit.seat))
            .filter(|seat| credited(*seat))
            .collect();
        self.ledgers
            .credit_kill_where(killer, victim, tick, window, &mut self.board.events, |seat| {
                (seat == killer && killer_ok)
                    || (seat == victim && victim_ok)
                    || (seat != killer && seat != victim && assisters_ok.contains(&seat))
            });
    }

    /// Add `delta` to the named counter on `seat`.
    pub fn add_counter(&mut self, seat: SeatId, name: &str, delta: i64) -> Result<(), RulesError> {
        self.ledgers.add_counter(seat, name, delta)
    }

    /// Seat `request` and `group` on one team when a team has room for every member.
    ///
    /// Automatic assignment (`request.team` unset) picks the team with the fewest occupied seats,
    /// then the lowest team score, then the lowest team id. A member's own `team` is ignored.
    /// When no team holds the whole party, each member is seated alone, still by that order.
    /// An explicit team that cannot hold the party is [`RulesError::SeatBad`] and nobody moves.
    ///
    /// A team change follows [`TeamSwitch`]. The default refuses it (`match_team_switch_refused`)
    /// and leaves every seat where it was. New seats are not a switch. The same team is a slot
    /// move, not a switch.
    pub fn assign_team(
        &mut self,
        request: &SeatRequest,
        group: &[SeatRequest],
    ) -> Result<Vec<SeatId>, RulesError> {
        let before = self.flow.seated.clone();
        let placed = self.assign_team_unchecked(request, group);
        // The coordinates a move leaves are vacated (ADR-8881).
        self.note_seat_changes(&before);
        placed
    }

    fn assign_team_unchecked(
        &mut self,
        request: &SeatRequest,
        group: &[SeatRequest],
    ) -> Result<Vec<SeatId>, RulesError> {
        let mut members = Vec::with_capacity(1 + group.len());
        members.push(*request);
        members.extend(group.iter().copied());
        let mut leaving = Vec::new();
        for member in &members {
            let Some(from) = member.from else {
                continue;
            };
            if !self.flow.seated.contains(&from) {
                return Err(RulesError::SeatMissing);
            }
            if leaving.contains(&from) {
                return Err(self.seat_bad(from.team, from.slot));
            }
            leaving.push(from);
        }
        let together = self.plan_together(&members, &leaving)?;
        let (placements, split) = if let Some(placements) = together {
            (placements, false)
        } else if request.team.is_some() {
            let slot = request.slot.unwrap_or(0);
            return Err(self.seat_bad(request.team.unwrap_or(0), slot));
        } else {
            (self.plan_individually(&members, &leaving)?, true)
        };
        for placement in &placements {
            if let Some(from) = placement.from
                && from.team != placement.dest.team
            {
                self.allow_switch(from, placement.dest.team)?;
            }
        }
        let party = if split || group.is_empty() {
            0
        } else {
            self.alloc_party()
        };
        self.commit_placements(&placements, party)?;
        Ok(placements.into_iter().map(|placement| placement.dest).collect())
    }

    fn alloc_party(&mut self) -> u64 {
        self.next_party = self.next_party.saturating_add(1);
        if self.next_party == 0 {
            self.next_party = 1;
        }
        self.next_party
    }

    fn seat_bad(&self, team: u8, slot: u8) -> RulesError {
        RulesError::SeatBad {
            team,
            slot,
            max_teams: self.flow.settings.max_teams,
            seats: self.flow.settings.seats_per_team,
        }
    }

    fn allow_switch(&self, from: SeatId, dest_team: u8) -> Result<(), RulesError> {
        let refused = RulesError::TeamSwitchRefused {
            team: from.team,
            slot: from.slot,
        };
        match self.teams.switch {
            TeamSwitch::Never => Err(refused),
            TeamSwitch::BetweenMatches => {
                if self.flow.between {
                    Ok(())
                } else {
                    Err(refused)
                }
            }
            TeamSwitch::WhenUnbalancedBy(margin) => {
                let source = self.team_headcount(from.team);
                let dest = self.team_headcount(dest_team);
                if source >= dest.saturating_add(usize::from(margin)) {
                    Ok(())
                } else {
                    Err(refused)
                }
            }
        }
    }

    fn team_headcount(&self, team: u8) -> usize {
        self.flow.seated.iter().filter(|seat| seat.team == team).count()
    }

    /// Team definitions in id order, one row per id, only fighting teams.
    fn defs_in_id_order(&self) -> Vec<TeamDef> {
        let mut defs = self.teams.defs.clone();
        defs.sort_by_key(|def| def.id);
        let mut out = Vec::new();
        for def in defs {
            if out.iter().any(|kept: &TeamDef| kept.id == def.id) {
                continue;
            }
            if !(1..=self.flow.settings.max_teams).contains(&def.id) {
                continue;
            }
            out.push(def);
        }
        out
    }

    fn def_of(&self, team: u8) -> Option<TeamDef> {
        self.defs_in_id_order().into_iter().find(|def| def.id == team)
    }

    fn seat_capacity(&self, def: &TeamDef) -> u8 {
        let mut cap = def.max_seats.min(self.flow.settings.seats_per_team);
        if let Some(limit) = self.flow.settings.seat_slot_limit {
            cap = cap.min(limit);
        }
        cap
    }

    fn occupied_slots(&self, team: u8, leaving: &[SeatId], planned: &[SeatId]) -> Vec<u8> {
        let mut slots: Vec<u8> = self
            .flow
            .seated
            .iter()
            .filter(|seat| seat.team == team && !leaving.contains(seat))
            .map(|seat| seat.slot)
            .collect();
        for seat in planned {
            if seat.team == team {
                slots.push(seat.slot);
            }
        }
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    fn occupied_count(&self, team: u8, leaving: &[SeatId], planned: &[SeatId]) -> usize {
        let staying = self
            .flow
            .seated
            .iter()
            .filter(|seat| seat.team == team && !leaving.contains(seat))
            .count();
        let added = planned.iter().filter(|seat| seat.team == team).count();
        staying + added
    }

    fn free_slots(&self, def: &TeamDef, leaving: &[SeatId], planned: &[SeatId]) -> Vec<u8> {
        let taken = self.occupied_slots(def.id, leaving, planned);
        let cap = self.seat_capacity(def);
        (0..cap).filter(|slot| !taken.contains(slot)).collect()
    }

    /// Teams that can hold `party_size`, fewest occupied seats, then lowest score, then lowest id.
    fn ranked_teams(&self, party_size: usize, leaving: &[SeatId], planned: &[SeatId]) -> Vec<TeamDef> {
        let mut ranked = Vec::new();
        for def in self.defs_in_id_order() {
            if self.free_slots(&def, leaving, planned).len() < party_size {
                continue;
            }
            let occupied = self.occupied_count(def.id, leaving, planned);
            let score = self.board.score(def.id);
            ranked.push((occupied, score, def.id, def));
        }
        ranked.sort_by_key(|(occupied, score, id, _)| (*occupied, *score, *id));
        ranked.into_iter().map(|(_, _, _, def)| def).collect()
    }

    /// `Ok(None)` when this team cannot hold every member. A bad explicit slot is an error.
    fn place_on(
        &self,
        def: &TeamDef,
        members: &[SeatRequest],
        leaving: &[SeatId],
        planned: &[SeatId],
    ) -> Result<Option<Vec<SeatId>>, RulesError> {
        let mut free = self.free_slots(def, leaving, planned);
        let mut dests = Vec::with_capacity(members.len());
        for member in members {
            let slot = if let Some(slot) = member.slot {
                self.flow.check_seat(SeatId::new(def.id, slot))?;
                if let Some(limit) = self.flow.settings.seat_slot_limit
                    && slot >= limit
                {
                    return Err(RulesError::SeatLimit { slot, limit });
                }
                if !free.contains(&slot) {
                    return Ok(None);
                }
                if member.from.is_some() && self.holds_foreign_row(SeatId::new(def.id, slot), leaving) {
                    return Ok(None);
                }
                slot
            } else {
                // A mover keeps its own row, so it skips a coordinate whose row belongs to nobody
                // seated (ADR-8657). A new player may take one: that row is reopened at zero.
                let Some(slot) = free.iter().copied().find(|slot| {
                    member.from.is_none() || !self.holds_foreign_row(SeatId::new(def.id, *slot), leaving)
                }) else {
                    return Ok(None);
                };
                slot
            };
            free.retain(|candidate| *candidate != slot);
            dests.push(SeatId::new(def.id, slot));
        }
        Ok(Some(dests))
    }

    fn placements_for(&self, members: &[SeatRequest], dests: &[SeatId]) -> Vec<Placement> {
        members
            .iter()
            .zip(dests.iter().copied())
            .map(|(member, dest)| Placement {
                from: member.from,
                dest,
                unit: member.unit,
            })
            .collect()
    }

    fn plan_together(
        &self,
        members: &[SeatRequest],
        leaving: &[SeatId],
    ) -> Result<Option<Vec<Placement>>, RulesError> {
        let requested = members.first().and_then(|member| member.team);
        if let Some(team) = requested {
            let Some(def) = self.def_of(team) else {
                let slot = members.first().and_then(|member| member.slot).unwrap_or(0);
                return Err(self.seat_bad(team, slot));
            };
            let Some(dests) = self.place_on(&def, members, leaving, &[])? else {
                return Ok(None);
            };
            return Ok(Some(self.placements_for(members, &dests)));
        }
        for def in self.ranked_teams(members.len(), leaving, &[]) {
            if let Some(dests) = self.place_on(&def, members, leaving, &[])? {
                return Ok(Some(self.placements_for(members, &dests)));
            }
        }
        Ok(None)
    }

    fn plan_individually(
        &self,
        members: &[SeatRequest],
        leaving: &[SeatId],
    ) -> Result<Vec<Placement>, RulesError> {
        let mut planned = Vec::new();
        let mut out = Vec::with_capacity(members.len());
        for member in members {
            let mut dest = None;
            for def in self.ranked_teams(1, leaving, &planned) {
                if let Some(dests) = self.place_on(&def, std::slice::from_ref(member), leaving, &planned)? {
                    dest = dests.into_iter().next();
                    break;
                }
            }
            let Some(dest) = dest else {
                return Err(self.seat_bad(member.team.unwrap_or(0), member.slot.unwrap_or(0)));
            };
            planned.push(dest);
            out.push(Placement {
                from: member.from,
                dest,
                unit: member.unit,
            });
        }
        Ok(out)
    }

    fn commit_placements(&mut self, placements: &[Placement], party: u64) -> Result<(), RulesError> {
        let leaving: Vec<SeatId> = placements.iter().filter_map(|placement| placement.from).collect();
        let mut occupied: Vec<SeatId> = self
            .flow
            .seated
            .iter()
            .copied()
            .filter(|seat| !leaving.contains(seat))
            .collect();
        for placement in placements {
            self.flow.check_seat(placement.dest)?;
            if let Some(limit) = self.flow.settings.seat_slot_limit
                && placement.dest.slot >= limit
            {
                return Err(RulesError::SeatLimit {
                    slot: placement.dest.slot,
                    limit,
                });
            }
            if occupied.contains(&placement.dest) {
                return Err(self.seat_bad(placement.dest.team, placement.dest.slot));
            }
            if placement.from.is_some_and(|from| from != placement.dest)
                && self.holds_foreign_row(placement.dest, &leaving)
            {
                return Err(self.seat_bad(placement.dest.team, placement.dest.slot));
            }
            occupied.push(placement.dest);
        }
        let moves: Vec<(SeatId, SeatId)> = placements
            .iter()
            .filter_map(|placement| placement.from.map(|from| (from, placement.dest)))
            .collect();
        for from in &leaving {
            if placements
                .iter()
                .any(|placement| placement.from == Some(*from) && placement.dest == *from)
            {
                continue;
            }
            self.flow.seated.retain(|seat| seat != from);
            self.flow.ready.retain(|seat| seat != from);
            self.flow.observers.retain(|seat| seat != from);
        }
        self.flow.carry_seat_state(&moves);
        for placement in placements {
            self.flow.seat_player(placement.dest)?;
        }
        self.ledgers.rekey_many(&moves, party);
        // The respawn book follows the player in the same batch (ADR-8704).
        self.respawns.carry(&moves);
        // ... and so does the race row, with its progress (ADR-8802).
        self.race.carry(&moves);
        let batch = self.next_move_batch();
        for (from, to) in &moves {
            if from != to {
                self.seat_moves.push(SeatMove {
                    from: *from,
                    to: *to,
                    batch,
                });
            }
        }
        for placement in placements {
            if placement.from.is_none() {
                // A new player starts from a zero row, whatever a credit to this unseated
                // coordinate left there before (ADR-8657), and alive with full lives (ADR-8704).
                self.respawns.forget(placement.dest);
                self.race.forget_unplaced(placement.dest);
                let index = self.ledgers.reopen(placement.dest);
                self.ledgers.rows[index].party = party;
            } else if self.ledgers.row(placement.dest).is_none() {
                let index = self.ledgers.ensure(placement.dest);
                self.ledgers.rows[index].party = party;
            }
            if let Some(unit) = placement.unit {
                self.ledgers.bind_unit(placement.dest, unit)?;
            }
        }
        Ok(())
    }

    /// Free slots of `def` a seated player may move onto: no seated player and no foreign row.
    fn landing_slots(&self, def: &TeamDef) -> Vec<u8> {
        self.free_slots(def, &[], &[])
            .into_iter()
            .filter(|slot| !self.holds_foreign_row(SeatId::new(def.id, *slot), &[]))
            .collect()
    }

    /// True when `seat` has a ledger row that nobody seated there owns: a credit to an unseated
    /// coordinate opened it. Seats in `leaving` are vacating in the same call, so their rows move
    /// with them and are not foreign.
    fn holds_foreign_row(&self, seat: SeatId, leaving: &[SeatId]) -> bool {
        self.ledgers.row(seat).is_some() && !self.flow.seated.contains(&seat) && !leaving.contains(&seat)
    }
}

impl LedgerBook {
    /// Move several rows at once. Overlapping destinations (a swap) keep every row.
    ///
    /// A move whose destination already holds a row that is not itself moving (a row outside the
    /// move) is skipped: that row keeps its stats, and the source row and its hits stay where they
    /// were (ADR-8657).
    pub fn rekey_many(&mut self, moves: &[(SeatId, SeatId)], party: u64) {
        let sources: Vec<SeatId> = moves
            .iter()
            .filter(|(from, to)| from != to)
            .map(|(from, _)| *from)
            .collect();
        let mut kept: Vec<(SeatId, SeatId)> = Vec::with_capacity(moves.len());
        for (from, to) in moves.iter().copied() {
            let free = from == to || sources.contains(&to) || self.row_index(to).is_none();
            let shared = from != to && kept.iter().any(|(_, other)| *other == to);
            if free && !shared {
                kept.push((from, to));
            }
        }
        let moves = kept;
        // Pending hits on or by a coordinate being moved onto belong to whoever was credited
        // there while it was empty; they do not follow the mover.
        let landing: Vec<SeatId> = moves
            .iter()
            .filter(|(from, to)| from != to && !sources.contains(to))
            .map(|(_, to)| *to)
            .collect();
        self.contributions.retain(|list| !landing.contains(&list.seat));
        for list in &mut self.contributions {
            list.hits.retain(|hit| !landing.contains(&hit.seat));
        }
        let mut held = Vec::new();
        for (from, to) in &moves {
            if from == to {
                if let Some(row) = self.row_mut(*from) {
                    row.party = party;
                }
                continue;
            }
            if let Ok(index) = self.rows.binary_search_by_key(from, PlayerLedger::seat) {
                let mut row = self.rows.remove(index);
                row.team = to.team;
                row.slot = to.slot;
                row.party = party;
                held.push(row);
            }
        }
        for row in held {
            let seat = row.seat();
            // Every destination is free after the filter above, so this inserts.
            let index = self
                .rows
                .binary_search_by_key(&seat, PlayerLedger::seat)
                .unwrap_or_else(|index| index);
            self.rows.insert(index, row);
        }
        for list in &mut self.contributions {
            if let Some((_, to)) = moves.iter().find(|(from, to)| *from == list.seat && from != to) {
                list.seat = *to;
            }
            for hit in &mut list.hits {
                if let Some((_, to)) = moves.iter().find(|(from, to)| *from == hit.seat && from != to) {
                    hit.seat = *to;
                }
            }
            list.hits.sort_by_key(|hit| (hit.tick, hit.seat));
        }
        self.contributions.sort_by_key(|list| list.seat);
    }
}

/// One party rebalance may move. `party` `0` is a single seat.
struct PartyChunk {
    seats: Vec<SeatId>,
    party: u64,
}

/// Even the fighting teams when [`TeamRules::rebalance_between_rounds`] is set.
///
/// Stops once counts differ by at most one, or no party can move without splitting or making the
/// destination at least as full as the source. A party moves only when every member is on the
/// fullest team. The smallest party goes first (tie: lowest seat) onto the emptiest team that has
/// room (tie: lowest id). A party too large to move without overshooting therefore stays, and the
/// counts can stay more than one apart (ADR-8657). A slot with a foreign ledger row is not room.
pub(crate) fn rebalance(session: &mut MatchSession) {
    let limit = session.flow.seated.len();
    for _ in 0..limit {
        let defs = session.defs_in_id_order();
        if defs.len() < 2 {
            return;
        }
        let counts: Vec<(u8, usize)> = defs
            .iter()
            .map(|def| (def.id, session.team_headcount(def.id)))
            .collect();
        let Some(max_count) = counts.iter().map(|(_, count)| *count).max() else {
            return;
        };
        let Some(min_count) = counts.iter().map(|(_, count)| *count).min() else {
            return;
        };
        if max_count.saturating_sub(min_count) <= 1 {
            return;
        }
        let Some(source_id) = counts
            .iter()
            .filter(|(_, count)| *count == max_count)
            .map(|(id, _)| *id)
            .max()
        else {
            return;
        };
        let Some(source_count) = counts
            .iter()
            .find(|(id, _)| *id == source_id)
            .map(|(_, count)| *count)
        else {
            return;
        };
        let mut moved = false;
        for chunk in parties_on(session, source_id) {
            let size = chunk.seats.len();
            let mut best: Option<(usize, u8)> = None;
            for def in &defs {
                if def.id == source_id {
                    continue;
                }
                let dest_count = counts
                    .iter()
                    .find(|(id, _)| *id == def.id)
                    .map(|(_, count)| *count)
                    .unwrap_or(0);
                if session.landing_slots(def).len() < size {
                    continue;
                }
                if source_count <= dest_count + size {
                    continue;
                }
                let key = (dest_count, def.id);
                let replace = match best {
                    None => true,
                    Some(current) => key < current,
                };
                if replace {
                    best = Some(key);
                }
            }
            let Some((_, dest_id)) = best else {
                continue;
            };
            let Some(dest_def) = session.def_of(dest_id) else {
                continue;
            };
            let free = session.landing_slots(&dest_def);
            if free.len() < size {
                continue;
            }
            let mut failed = false;
            for (index, from) in chunk.seats.iter().enumerate() {
                let Some(slot) = free.get(index).copied() else {
                    failed = true;
                    break;
                };
                let to = SeatId::new(dest_id, slot);
                if session.flow.relocate_seat(*from, to).is_err() {
                    failed = true;
                    break;
                }
                session.ledgers.rekey(*from, to, chunk.party);
                // A launch clears the book after this rebalance; the carry keeps a later caller
                // honest (ADR-8704).
                session.respawns.carry(&[(*from, to)]);
                session.race.carry(&[(*from, to)]);
                let batch = session.next_move_batch();
                session.seat_moves.push(SeatMove {
                    from: *from,
                    to,
                    batch,
                });
            }
            if failed {
                return;
            }
            moved = true;
            break;
        }
        if !moved {
            return;
        }
    }
}

fn parties_on(session: &MatchSession, team: u8) -> Vec<PartyChunk> {
    let mut grouped: Vec<(u64, Vec<SeatId>)> = Vec::new();
    let mut solos = Vec::new();
    for seat in &session.flow.seated {
        if seat.team != team {
            continue;
        }
        let party = session.ledgers.row(*seat).map(|row| row.party).unwrap_or(0);
        if party == 0 {
            solos.push(PartyChunk {
                seats: vec![*seat],
                party: 0,
            });
            continue;
        }
        if let Some((_, seats)) = grouped.iter_mut().find(|(id, _)| *id == party) {
            seats.push(*seat);
        } else {
            grouped.push((party, vec![*seat]));
        }
    }
    let mut chunks = solos;
    for (party, seats) in grouped {
        let split = session.flow.seated.iter().any(|seat| {
            seat.team != team && session.ledgers.row(*seat).is_some_and(|row| row.party == party)
        });
        if split || seats.is_empty() {
            continue;
        }
        chunks.push(PartyChunk { seats, party });
    }
    chunks.sort_by(|left, right| {
        left.seats
            .len()
            .cmp(&right.seats.len())
            .then_with(|| left.seats.first().cmp(&right.seats.first()))
    });
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_flow::{FlowInput, FlowSettings};
    use crate::objective::{Contestant, HillVolume, ObjectiveSettings, ScoreReason};
    use crate::playlist::PlaylistEntry;
    use crate::session::SessionSettings;

    fn color(index: u8) -> [u8; 4] {
        DEFAULT_COLORS[usize::from(index) % DEFAULT_COLORS.len()]
    }

    #[test]
    fn auto_assign_fills_the_smaller_team_and_keeps_a_party_together() {
        let mut session = MatchSession::default();
        session.teams.defs = vec![
            TeamDef::unnamed(1, color(0), 16),
            TeamDef::unnamed(2, color(1), 1),
            TeamDef::unnamed(3, color(2), 16),
        ];
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let one = session.assign_team(&SeatRequest::default(), &[]).expect("one");
        assert_eq!(one, vec![SeatId::new(2, 0)], "the empty team with the lowest id");

        let party = session
            .assign_team(
                &SeatRequest {
                    unit: Some(10),
                    ..SeatRequest::default()
                },
                &[SeatRequest {
                    unit: Some(11),
                    ..SeatRequest::default()
                }],
            )
            .expect("party");
        assert_eq!(party, vec![SeatId::new(3, 0), SeatId::new(3, 1)]);
        let first = session.ledgers.row(party[0]).expect("row");
        let second = session.ledgers.row(party[1]).expect("row");
        assert_ne!(first.party, 0);
        assert_eq!(first.party, second.party);
        assert_eq!((first.unit, second.unit), (Some(10), Some(11)));

        let mut tied = MatchSession::default();
        tied.teams.defs = vec![
            TeamDef::unnamed(1, color(0), 16),
            TeamDef::unnamed(2, color(1), 16),
        ];
        tied.board.team_scores[1] = 5;
        tied.board.team_scores[2] = 1;
        let picked = tied.assign_team(&SeatRequest::default(), &[]).expect("score");
        assert_eq!(
            picked,
            vec![SeatId::new(2, 0)],
            "equal sizes take the lower team score"
        );
    }

    #[test]
    fn team_switch_between_matches_only_is_refused_live() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::BetweenMatches;
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let same_team = SeatRequest {
            from: Some(SeatId::new(1, 0)),
            team: Some(1),
            slot: Some(3),
            unit: None,
        };
        assert_eq!(
            session.assign_team(&same_team, &[]).expect("slot move"),
            vec![SeatId::new(1, 3)]
        );
        let switch = SeatRequest {
            from: Some(SeatId::new(1, 3)),
            team: Some(2),
            slot: None,
            unit: None,
        };
        let refused = session.assign_team(&switch, &[]).expect_err("live");
        assert_eq!(refused.code(), "match_team_switch_refused");
        assert!(session.flow.seated.contains(&SeatId::new(1, 3)));
        assert!(!session.flow.seated.contains(&SeatId::new(2, 0)));
        session.apply(&FlowInput::OpenBetween).expect("between");
        assert_eq!(
            session.assign_team(&switch, &[]).expect("between"),
            vec![SeatId::new(2, 0)]
        );
    }

    #[test]
    fn a_kill_credits_killer_victim_and_assister_within_the_window() {
        let mut session = MatchSession::default();
        let killer = SeatId::new(1, 0);
        let victim = SeatId::new(2, 0);
        let assist = SeatId::new(1, 1);
        let stale = SeatId::new(2, 1);
        for seat in [killer, victim, assist, stale] {
            session
                .apply(&FlowInput::Seat {
                    team: seat.team,
                    slot: seat.slot,
                })
                .expect("seat");
        }
        session.flow.match_tick = 0;
        session.note_hit(victim, stale);
        session.flow.match_tick = 1;
        session.note_hit(victim, assist);
        session.note_hit(victim, victim);
        session.flow.match_tick = 301;
        session.credit_kill(killer, victim);
        let killer_row = session.ledgers.row(killer).expect("killer");
        let victim_row = session.ledgers.row(victim).expect("victim");
        let assist_row = session.ledgers.row(assist).expect("assist");
        let stale_row = session.ledgers.row(stale).expect("stale");
        assert_eq!((killer_row.kills, killer_row.score), (1, 1));
        assert_eq!((victim_row.deaths, victim_row.score), (1, 0));
        assert_eq!((assist_row.assists, assist_row.score), (1, 0));
        assert_eq!(
            stale_row.assists, 0,
            "a hit older than the window does not assist"
        );
        assert!(session.board.events.iter().any(|event| matches!(
            event,
            RulesEvent::PlayerScored {
                reason: ScoreReason::Kill,
                team: 1,
                slot: 0,
                ..
            }
        )));
        assert!(session.board.events.iter().any(|event| matches!(
            event,
            RulesEvent::PlayerScored {
                reason: ScoreReason::Death,
                team: 2,
                slot: 0,
                ..
            }
        )));
        assert!(session.board.events.iter().any(|event| matches!(
            event,
            RulesEvent::PlayerScored {
                reason: ScoreReason::Assist,
                team: 1,
                slot: 1,
                ..
            }
        )));
        assert!(
            session
                .ledgers
                .contributions
                .iter()
                .all(|list| list.seat != victim)
        );
    }

    fn capture_once(bind: bool) -> (MatchSession, Vec<RulesEvent>) {
        let mut session = MatchSession::default();
        session.playlist.set(vec![PlaylistEntry::new("field", "ctf")]);
        session.start_now();
        session.board.spawn_flag(1, 1, [-20.0, 0.0, 0.0]).expect("flag");
        session.board.spawn_flag(2, 2, [20.0, 0.0, 0.0]).expect("flag");
        if bind {
            session
                .apply(&FlowInput::Seat { team: 1, slot: 0 })
                .expect("seat");
            session.ledgers.bind_unit(SeatId::new(1, 0), 10).expect("bind");
        }
        let carrier = |position: [f32; 3]| Contestant {
            id: 10,
            team: 1,
            position,
            alive: true,
        };
        session.step(&[carrier([20.0, 0.0, 0.0])]);
        let step = session.step(&[carrier([-20.0, 0.0, 0.0])]);
        (session, step.events)
    }

    #[test]
    fn a_flag_capture_credits_the_carriers_ledger_and_leaves_team_score_bits_unchanged() {
        let (bound, bound_events) = capture_once(true);
        let (unbound, unbound_events) = capture_once(false);
        assert_eq!(bound.board.team_scores, unbound.board.team_scores);
        assert_eq!(bound.board.score(1), 1);
        let row = bound.ledgers.row(SeatId::new(1, 0)).expect("carrier");
        assert_eq!(row.objective_points, 1);
        assert_eq!(row.score, 1);
        assert!(bound_events.iter().any(|event| matches!(
            event,
            RulesEvent::PlayerScored {
                reason: ScoreReason::Objective,
                team: 1,
                slot: 0,
                score: 1,
                ..
            }
        )));
        assert!(
            unbound_events
                .iter()
                .all(|event| !matches!(event, RulesEvent::PlayerScored { .. }))
        );
        assert!(unbound.ledgers.rows.is_empty());
    }

    #[test]
    fn a_hill_point_credits_each_bound_occupant_once() {
        let mut session = MatchSession::new(SessionSettings {
            objectives: ObjectiveSettings {
                hill_accrual_ticks: 1,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![PlaylistEntry::new("hill", "koth")]);
        session.start_now();
        session
            .board
            .install_hill_volume(
                9,
                HillVolume {
                    min: [-1.0, -1.0, -1.0],
                    max: [1.0, 1.0, 1.0],
                },
            )
            .expect("hill");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 1 })
            .expect("seat");
        session.ledgers.bind_unit(SeatId::new(1, 0), 5).expect("bind");
        session.ledgers.bind_unit(SeatId::new(1, 1), 6).expect("bind");
        let units = [
            Contestant {
                id: 5,
                team: 1,
                position: [0.0, 0.0, 0.0],
                alive: true,
            },
            Contestant {
                id: 6,
                team: 1,
                position: [0.0, 0.0, 0.0],
                alive: true,
            },
        ];
        session.step(&units);
        assert_eq!(session.board.score(1), 1);
        assert_eq!(
            session
                .ledgers
                .row(SeatId::new(1, 0))
                .expect("a")
                .objective_points,
            1
        );
        assert_eq!(
            session
                .ledgers
                .row(SeatId::new(1, 1))
                .expect("b")
                .objective_points,
            1
        );
    }

    #[test]
    fn ledger_named_counters_are_capped() {
        let mut session = MatchSession::default();
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let seat = SeatId::new(1, 0);
        for index in 0..MAX_NAMED_COUNTERS {
            session
                .add_counter(seat, &format!("c{index}"), 1)
                .expect("counter");
        }
        let refused = session.add_counter(seat, "overflow", 1).expect_err("cap");
        assert_eq!(refused.code(), "match_stat_cap");
        session.add_counter(seat, "c0", 4).expect("existing");
        assert_eq!(
            session
                .ledgers
                .row(seat)
                .expect("row")
                .counters
                .get("c0")
                .copied(),
            Some(5)
        );
        assert_eq!(
            session.add_counter(seat, "", 1).expect_err("empty").code(),
            "match_stat_cap"
        );
        let long = "n".repeat(MAX_COUNTER_NAME_BYTES + 1);
        assert_eq!(
            session.add_counter(seat, &long, 1).expect_err("long").code(),
            "match_stat_cap"
        );
        assert_eq!(
            session
                .add_counter(SeatId::new(2, 0), "c0", 1)
                .expect_err("missing")
                .code(),
            "match_seat_missing"
        );
    }

    #[test]
    fn rebalance_between_rounds_moves_a_solo_and_keeps_the_party() {
        let mut session = MatchSession::default();
        session.teams.rebalance_between_rounds = true;
        session.teams.defs = vec![
            TeamDef::unnamed(1, color(0), 16),
            TeamDef::unnamed(2, color(1), 16),
        ];
        for slot in 0..3 {
            session.apply(&FlowInput::Seat { team: 1, slot }).expect("seat");
        }
        session.ledgers.rekey(SeatId::new(1, 0), SeatId::new(1, 0), 7);
        session.ledgers.rekey(SeatId::new(1, 1), SeatId::new(1, 1), 7);
        session.playlist.set(vec![PlaylistEntry::new("next", "tdm")]);
        session.start_now();
        let on = |team: u8| {
            session
                .flow
                .seated
                .iter()
                .filter(|seat| seat.team == team)
                .count()
        };
        assert_eq!((on(1), on(2)), (2, 1));
        let party: Vec<SeatId> = session
            .ledgers
            .rows
            .iter()
            .filter(|row| row.party == 7)
            .map(PlayerLedger::seat)
            .collect();
        assert_eq!(party.len(), 2);
        assert!(party.iter().all(|seat| seat.team == 1));
    }

    #[test]
    fn team_switch_when_unbalanced_uses_the_counts_before_the_move() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::WhenUnbalancedBy(2);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 1 })
            .expect("seat");
        let move_off = SeatRequest {
            from: Some(SeatId::new(1, 1)),
            team: Some(2),
            slot: None,
            unit: None,
        };
        assert_eq!(
            session.assign_team(&move_off, &[]).expect("unbalanced"),
            vec![SeatId::new(2, 0)]
        );
        let refused = session
            .assign_team(
                &SeatRequest {
                    from: Some(SeatId::new(1, 0)),
                    team: Some(2),
                    slot: None,
                    unit: None,
                },
                &[],
            )
            .expect_err("even");
        assert_eq!(refused.code(), "match_team_switch_refused");
    }

    /// Proof: a row `credit_kill` opened is not a fresh player. Seating that coordinate must not
    /// keep the previous kills. Fails at the reviewed head: `ensure` leaves the existing row.
    #[test]
    fn proof_seating_a_coordinate_does_not_inherit_an_earlier_kill() {
        let mut session = MatchSession::default();
        session.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        assert_eq!(
            session.ledgers.row(SeatId::new(1, 0)).expect("row").kills,
            0,
            "the player who just sat down has the kill that was recorded before anyone sat here"
        );
    }

    /// Proof: moving onto a ledger row that is not in `flow.seated` must not delete that row's
    /// kills. Fails at the reviewed head: `rekey_many` overwrites the destination row.
    #[test]
    fn proof_a_move_does_not_delete_kills_already_on_the_destination() {
        let mut session = MatchSession::default();
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.add_counter(SeatId::new(1, 0), "tag", 9).expect("counter");
        session.credit_kill(SeatId::new(2, 0), SeatId::new(3, 0));
        assert_eq!(session.ledgers.row(SeatId::new(2, 0)).expect("phantom").kills, 1);
        session.teams.switch = TeamSwitch::BetweenMatches;
        session.apply(&FlowInput::OpenBetween).expect("between");
        let moved = session.assign_team(
            &SeatRequest {
                from: Some(SeatId::new(1, 0)),
                team: Some(2),
                slot: Some(0),
                unit: None,
            },
            &[],
        );
        match moved {
            Err(_) => {
                assert_eq!(session.ledgers.row(SeatId::new(2, 0)).expect("kept").kills, 1);
                assert!(session.flow.seated.contains(&SeatId::new(1, 0)));
            }
            Ok(seats) => {
                assert_eq!(seats, vec![SeatId::new(2, 0)]);
                let row = session.ledgers.row(SeatId::new(2, 0)).expect("dest");
                assert_eq!(row.kills, 1, "the kill on the destination was deleted");
                assert_eq!(row.counters.get("tag").copied(), Some(9));
            }
        }
    }

    /// Proof: a team change carries the pause budget. Fails at the reviewed head: `pause_used`
    /// stays on the old seat, so the same player pauses a second time and the next occupant of
    /// the old seat inherits the exhausted budget.
    #[test]
    fn proof_a_team_change_carries_the_pause_budget() {
        use crate::match_flow::Issuer;

        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                max_pauses_per_seat: Some(1),
                ..FlowSettings::default()
            },
            ..SessionSettings::default()
        });
        session.teams.switch = TeamSwitch::WhenUnbalancedBy(1);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 1 })
            .expect("seat");
        session
            .apply_from(
                Issuer::guest(SeatId::new(1, 0)),
                &FlowInput::Pause { name: "ada".into() },
            )
            .expect("first pause");
        session
            .apply_from(
                Issuer::host(),
                &FlowInput::Resume {
                    name: String::new(),
                    host: false,
                },
            )
            .expect("resume");
        session
            .assign_team(
                &SeatRequest {
                    from: Some(SeatId::new(1, 0)),
                    team: Some(2),
                    slot: None,
                    unit: None,
                },
                &[],
            )
            .expect("switch");
        let again = session.apply_from(
            Issuer::guest(SeatId::new(2, 0)),
            &FlowInput::Pause { name: "ada".into() },
        );
        assert_eq!(
            again.expect_err("budget already used").code(),
            "match_pause_budget"
        );
    }

    /// Proof: a join ask does not keep naming a seat the player has left. Fails at the reviewed
    /// head: `commit_placements` never rewrites `flow.joins`.
    #[test]
    fn proof_a_team_change_does_not_leave_a_join_ask_on_the_old_seat() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::BetweenMatches;
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.apply(&FlowInput::OpenBetween).expect("between");
        session
            .apply(&FlowInput::Observe { team: 1, slot: 0 })
            .expect("observe");
        session
            .apply(&FlowInput::Ready { team: 1, slot: 0 })
            .expect("ask");
        assert_eq!(session.flow.joins.len(), 1);
        session
            .assign_team(
                &SeatRequest {
                    from: Some(SeatId::new(1, 0)),
                    team: Some(2),
                    slot: None,
                    unit: None,
                },
                &[],
            )
            .expect("switch");
        assert!(
            session.flow.joins.iter().all(|ask| ask.seat != SeatId::new(1, 0)),
            "join ask still names the vacated seat: {:?}",
            session.flow.joins
        );
    }

    /// The review's rebalance case (finding 4). The mover leaves party 7 (a single move seats it
    /// alone, party 0), so party 7 is three seats, all on team 1, and no team can take all three
    /// without ending fuller than team 1 is. Rebalance stops early and keeps the party whole; the
    /// book describes that stop (ADR-8657). Counts stay `(3, 1, 0, 0)`.
    #[test]
    fn rebalance_stops_early_instead_of_splitting_a_party() {
        let mut session = MatchSession::default();
        session.teams.rebalance_between_rounds = true;
        session.teams.switch = TeamSwitch::BetweenMatches;
        for slot in 0..4 {
            session.apply(&FlowInput::Seat { team: 1, slot }).expect("seat");
            session
                .ledgers
                .rekey(SeatId::new(1, slot), SeatId::new(1, slot), 7);
        }
        session.apply(&FlowInput::OpenBetween).expect("between");
        session
            .assign_team(
                &SeatRequest {
                    from: Some(SeatId::new(1, 0)),
                    team: Some(2),
                    slot: None,
                    unit: None,
                },
                &[],
            )
            .expect("move");
        assert_eq!(session.ledgers.row(SeatId::new(2, 0)).expect("mover").party, 0);
        session.playlist.set(vec![PlaylistEntry::new("next", "tdm")]);
        session.start_now();
        let count = |team: u8| {
            session
                .flow
                .seated
                .iter()
                .filter(|seat| seat.team == team)
                .count()
        };
        assert_eq!((count(1), count(2), count(3), count(4)), (3, 1, 0, 0));
        let party_teams: Vec<u8> = session
            .ledgers
            .rows
            .iter()
            .filter(|row| row.party == 7)
            .map(|row| row.team)
            .collect();
        assert_eq!(party_teams, vec![1, 1, 1], "party 7 stays whole");
    }

    /// Solo seats still even out to within one.
    #[test]
    fn rebalance_evens_solo_seats_to_within_one() {
        let mut session = MatchSession::default();
        session.teams.rebalance_between_rounds = true;
        for slot in 0..5 {
            session.apply(&FlowInput::Seat { team: 1, slot }).expect("seat");
        }
        session.playlist.set(vec![PlaylistEntry::new("next", "tdm")]);
        session.start_now();
        let count = |team: u8| {
            session
                .flow
                .seated
                .iter()
                .filter(|seat| seat.team == team)
                .count()
        };
        let counts = [count(1), count(2), count(3), count(4)];
        let max = counts.iter().max().copied().unwrap_or(0);
        let min = counts.iter().min().copied().unwrap_or(0);
        assert!(max - min <= 1, "{counts:?}");
        assert_eq!(counts.iter().sum::<usize>(), 5);
        assert_eq!(session.seat_moves.len(), 3, "{:?}", session.seat_moves);
        assert_eq!(session.ledgers.rows.len(), 5);
    }

    /// Proof: one hill point credits a bound unit once, even when the host slice lists that unit
    /// twice. Fails at the reviewed head: `credit_objective_events` walks every slice entry.
    #[test]
    fn proof_a_duplicate_hill_occupant_is_credited_once() {
        let mut session = MatchSession::new(SessionSettings {
            objectives: ObjectiveSettings {
                hill_accrual_ticks: 1,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![PlaylistEntry::new("hill", "koth")]);
        session.start_now();
        session
            .board
            .install_hill_volume(
                9,
                HillVolume {
                    min: [-1.0, -1.0, -1.0],
                    max: [1.0, 1.0, 1.0],
                },
            )
            .expect("hill");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.ledgers.bind_unit(SeatId::new(1, 0), 5).expect("bind");
        let occupant = Contestant {
            id: 5,
            team: 1,
            position: [0.0, 0.0, 0.0],
            alive: true,
        };
        session.step(&[occupant, occupant]);
        assert_eq!(session.board.score(1), 1);
        assert_eq!(
            session
                .ledgers
                .row(SeatId::new(1, 0))
                .expect("row")
                .objective_points,
            1,
            "one hill point was applied twice to the same unit"
        );
    }

    /// The session JSON keeps every new field. This one is expected to pass.
    #[test]
    fn ledger_round_trip_covers_kills_hits_counters_party_and_switch() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::WhenUnbalancedBy(2);
        session.teams.rebalance_between_rounds = true;
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .apply(&FlowInput::Seat { team: 2, slot: 0 })
            .expect("seat");
        session.note_hit(SeatId::new(2, 0), SeatId::new(1, 0));
        session
            .add_counter(SeatId::new(1, 0), "plants", 3)
            .expect("counter");
        session.next_party = 4;
        let text = serde_json::to_string(&session).expect("serialize");
        let back: MatchSession = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, session);
        assert!(text.contains("plants"));
        assert!(text.contains("when_unbalanced_by"));
        assert!(text.contains("\"hits\""));
    }

    /// Team-definition order is not an input. Same seats either way.
    #[test]
    fn assignment_ignores_the_order_of_team_definitions() {
        let mut forward = MatchSession::default();
        let mut reverse = MatchSession::default();
        reverse.teams.defs.reverse();
        let request = SeatRequest::default();
        assert_eq!(
            forward.assign_team(&request, &[]).expect("forward"),
            reverse.assign_team(&request, &[]).expect("reverse")
        );
    }

    fn move_to(
        session: &mut MatchSession,
        from: SeatId,
        team: u8,
        slot: Option<u8>,
    ) -> Result<Vec<SeatId>, RulesError> {
        session.assign_team(
            &SeatRequest {
                from: Some(from),
                team: Some(team),
                slot,
                unit: None,
            },
            &[],
        )
    }

    /// Row ids are handed out once: a move keeps the id, a leave and a rejoin get a new one.
    #[test]
    fn ledger_ids_are_never_zero_never_reused_and_follow_a_move() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::BetweenMatches;
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let first = session.ledgers.row(SeatId::new(1, 0)).expect("row").id;
        assert_ne!(first, 0);
        session.apply(&FlowInput::OpenBetween).expect("between");
        move_to(&mut session, SeatId::new(1, 0), 2, Some(0)).expect("move");
        assert_eq!(session.ledgers.row(SeatId::new(2, 0)).expect("moved").id, first);
        let rejoin = |session: &mut MatchSession| {
            session
                .apply(&FlowInput::Leave { team: 2, slot: 0 })
                .expect("leave");
            session
                .apply(&FlowInput::Seat { team: 2, slot: 0 })
                .expect("rejoin");
            session.ledgers.row(SeatId::new(2, 0)).expect("row").id
        };
        let second = rejoin(&mut session);
        assert!(second != 0 && second != first, "{second} after {first}");
        let third = rejoin(&mut session);
        assert!(
            third > second,
            "the same seat's next row has a larger id: {third} after {second}"
        );
        session
            .apply(&FlowInput::Seat { team: 2, slot: 0 })
            .expect("seat again");
        assert_eq!(
            session.ledgers.row(SeatId::new(2, 0)).expect("row").id,
            third,
            "seating a seat that is already seated keeps its row"
        );
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("old seat");
        let reused = session.ledgers.row(SeatId::new(1, 0)).expect("row").id;
        assert!(![first, second, third].contains(&reused), "{reused}");
    }

    /// A seat that is already seated keeps its stats when seated again.
    #[test]
    fn seating_an_already_seated_seat_keeps_its_row() {
        let mut session = MatchSession::default();
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .add_counter(SeatId::new(1, 0), "plants", 2)
            .expect("counter");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("again");
        let row = session.ledgers.row(SeatId::new(1, 0)).expect("row");
        assert_eq!(row.counters.get("plants").copied(), Some(2));
    }

    /// A new player seated through `assign_team` on a coordinate a credit opened starts at zero.
    #[test]
    fn a_new_player_from_assign_starts_from_a_zero_row() {
        let mut session = MatchSession::default();
        session.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
        let phantom = session.ledgers.row(SeatId::new(1, 0)).expect("phantom").id;
        let seats = session
            .assign_team(
                &SeatRequest {
                    from: None,
                    team: Some(1),
                    slot: Some(0),
                    unit: None,
                },
                &[],
            )
            .expect("seat");
        assert_eq!(seats, vec![SeatId::new(1, 0)]);
        let row = session.ledgers.row(SeatId::new(1, 0)).expect("row");
        assert_eq!((row.kills, row.deaths, row.score), (0, 0, 0));
        assert_ne!(row.id, phantom);
    }

    /// An automatic move skips a slot whose row belongs to nobody seated, and that row keeps its
    /// stats.
    #[test]
    fn an_automatic_move_skips_a_slot_with_a_foreign_row() {
        let mut session = MatchSession::default();
        session.teams.switch = TeamSwitch::BetweenMatches;
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session.credit_kill(SeatId::new(2, 0), SeatId::new(3, 0));
        session.apply(&FlowInput::OpenBetween).expect("between");
        let seats = move_to(&mut session, SeatId::new(1, 0), 2, None).expect("move");
        assert_eq!(seats, vec![SeatId::new(2, 1)]);
        assert_eq!(session.ledgers.row(SeatId::new(2, 0)).expect("kept").kills, 1);
        assert_eq!(
            session.seat_moves,
            vec![SeatMove {
                from: SeatId::new(1, 0),
                to: SeatId::new(2, 1),
                batch: 0,
            }]
        );
    }

    /// The book-level rekey never overwrites a destination row outside the move.
    #[test]
    fn rekey_keeps_a_destination_row_outside_the_move() {
        let mut book = LedgerBook::default();
        let mut events = Vec::new();
        book.ensure(SeatId::new(1, 0));
        book.credit_kill(SeatId::new(2, 0), SeatId::new(3, 0), 1, 300, &mut events);
        book.rekey(SeatId::new(1, 0), SeatId::new(2, 0), 4);
        assert_eq!(book.row(SeatId::new(2, 0)).expect("dest").kills, 1);
        assert!(book.row(SeatId::new(1, 0)).is_some(), "the source row stays");
        // A swap is inside the move and keeps both rows.
        book.ensure(SeatId::new(1, 1));
        let left = book.row(SeatId::new(1, 0)).expect("left").id;
        let right = book.row(SeatId::new(1, 1)).expect("right").id;
        book.rekey_many(
            &[
                (SeatId::new(1, 0), SeatId::new(1, 1)),
                (SeatId::new(1, 1), SeatId::new(1, 0)),
            ],
            0,
        );
        assert_eq!(book.row(SeatId::new(1, 1)).expect("swapped").id, left);
        assert_eq!(book.row(SeatId::new(1, 0)).expect("swapped").id, right);
    }

    /// Moving onto a coordinate drops the pending hits on and by it; the mover's hits follow.
    #[test]
    fn a_move_drops_pending_hits_on_the_landing_coordinate() {
        let mut book = LedgerBook::default();
        book.ensure(SeatId::new(1, 0));
        book.note_hit(SeatId::new(2, 0), SeatId::new(3, 0), 1);
        book.note_hit(SeatId::new(3, 0), SeatId::new(2, 0), 1);
        book.note_hit(SeatId::new(3, 0), SeatId::new(1, 0), 2);
        book.rekey(SeatId::new(1, 0), SeatId::new(2, 0), 0);
        assert!(
            book.contributions
                .iter()
                .all(|list| list.seat != SeatId::new(2, 0))
        );
        let on_three = book
            .contributions
            .iter()
            .find(|list| list.seat == SeatId::new(3, 0))
            .expect("list");
        assert_eq!(
            on_three.hits,
            vec![Hit {
                seat: SeatId::new(2, 0),
                tick: 2
            }]
        );
    }

    /// A swap keeps both pause budgets on their players.
    #[test]
    fn a_swap_carries_both_pause_budgets() {
        let mut session = MatchSession::default();
        let a = SeatId::new(1, 0);
        let b = SeatId::new(2, 0);
        session.flow.pause_used = vec![
            crate::match_flow::SeatPauseUse {
                seat: a,
                pauses: 1,
                ticks: 0,
            },
            crate::match_flow::SeatPauseUse {
                seat: b,
                pauses: 2,
                ticks: 5,
            },
        ];
        session.flow.carry_seat_state(&[(a, b), (b, a)]);
        let at = |seat: SeatId| {
            session
                .flow
                .pause_used
                .iter()
                .find(|row| row.seat == seat)
                .map(|row| (row.pauses, row.ticks))
        };
        assert_eq!(at(a), Some((2, 5)));
        assert_eq!(at(b), Some((1, 0)));
    }

    /// A vacated seat's next occupant has a fresh pause budget.
    #[test]
    fn the_next_occupant_of_a_vacated_seat_has_a_fresh_pause_budget() {
        use crate::match_flow::Issuer;

        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                max_pauses_per_seat: Some(1),
                ..FlowSettings::default()
            },
            ..SessionSettings::default()
        });
        session.teams.switch = TeamSwitch::WhenUnbalancedBy(1);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 1 })
            .expect("seat");
        session
            .apply_from(
                Issuer::guest(SeatId::new(1, 0)),
                &FlowInput::Pause { name: "ada".into() },
            )
            .expect("pause");
        session
            .apply_from(
                Issuer::host(),
                &FlowInput::Resume {
                    name: String::new(),
                    host: false,
                },
            )
            .expect("resume");
        move_to(&mut session, SeatId::new(1, 0), 2, None).expect("switch");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("new player");
        session
            .apply_from(
                Issuer::guest(SeatId::new(1, 0)),
                &FlowInput::Pause { name: "bo".into() },
            )
            .expect("a fresh budget");
    }

    /// Row ids do not depend on the order one step's seat inputs arrived in, so twin peers stay
    /// bit-identical (the flow keeps its seat lists sorted for the same reason).
    #[test]
    fn ledger_ids_do_not_depend_on_input_order() {
        let mut forward = MatchSession::default();
        let mut reverse = MatchSession::default();
        let seats = [(1, 0), (2, 0), (2, 1)];
        for (team, slot) in seats {
            forward.apply(&FlowInput::Seat { team, slot }).expect("seat");
        }
        for (team, slot) in seats.iter().rev().copied() {
            reverse.apply(&FlowInput::Seat { team, slot }).expect("seat");
        }
        assert_eq!(forward.ledgers, reverse.ledgers);
    }
}
