// SPDX-License-Identifier: MPL-2.0
//! Spawn-point selection and the respawn policy (ADR-8647).
//!
//! Plain data for lockstep sims: [`pick_spawn`] chooses one of a list of [`SpawnPointData`] for a
//! [`SpawnAsk`] under a [`SpawnPolicy`], with the host's deterministic line-of-sight test
//! (`occluded(a, b) -> bool`). Every peer that passes the same points, contestants, policy and
//! [`SpawnState`] gets the same point.
//!
//! - **Order.** Points are walked in [`SpawnPointData::sort_key`] order: higher `priority` first,
//!   then the arrival order (`SpawnOrder`), then the id. [`SpawnPolicy::First`] takes the first
//!   eligible point in that order (with no `SpawnPoint` data on any start that is S-14's "first
//!   start in spawn order"). [`SpawnPolicy::RoundRobin`] takes the next eligible point after the
//!   last one it gave. [`SpawnPolicy::Random`] draws from the session's [`DetRng`] stream.
//! - **Eligibility.** A point is eligible when it is enabled, its side is `0` or the side the asking
//!   team plays on (after side swaps), it carries every tag the ask requires, and it is not blocked
//!   (the host's overlap flag, or a living contestant within the ask's clearance radius).
//! - **Score.** [`SpawnPolicy::Scored`] scores each eligible point with integer terms: the nearest
//!   enemy distance (capped, whole centimeters), minus enemies that see the point, plus teammates
//!   near it, minus recent uses, plus priority. Ties go to the earlier point in sort order.
//!
//! Distances are `+ - *` on `f32` and [`regia_detsim::math::sqrt`], turned into whole centimeters
//! before any weight touches them; scores are `i64`. Nothing iterates a hash container.
//!
//! The respawn half is [`RespawnRules`] (one [`RespawnPolicy`] per team) and [`RespawnBook`] (the
//! pending respawns, lives and eliminations of the running match), stepped by
//! [`crate::session::MatchSession`].

use regia_detsim::math;
use regia_detsim::rng::DetRng;
use serde::{Deserialize, Serialize};

use super::match_flow::SeatId;
use super::objective::{Contestant, RulesEvent};

/// Spawn uses [`SpawnState`] remembers for the recent-use term. The oldest use is dropped first.
pub const MAX_RECENT_USES: usize = 64;

/// Points one [`pick_spawn`] call (and `rules.spawn_preview`) reads. Further points are ignored.
pub const MAX_SPAWN_POINTS: usize = 1024;

/// Stream id the session's spawn generator derives from its seed ([`DetRng::stream`]).
pub const SPAWN_RNG_STREAM: u64 = 0x5350_574e; // "SPWN"

/// One spawn point as the picker sees it: plain data the host copies from its level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnPointData {
    /// Stable id (the entity id on an ECS host). The last tie-break.
    pub id: u32,
    /// Arrival order (`SpawnOrder`); lower is earlier. Breaks priority ties.
    pub order: u64,
    /// World position.
    pub position: [f32; 3],
    /// The side the point belongs to; `0` serves every team.
    pub team: u8,
    /// Tags an ask can require (`"attack"`, `"vehicle"`).
    pub tags: Vec<String>,
    /// Higher is walked first and adds to the score.
    pub priority: i16,
    /// A disabled point is never picked.
    pub enabled: bool,
    /// Set by the host when its overlap query found the pawn's shape blocked here.
    pub blocked: bool,
}

impl SpawnPointData {
    /// An enabled side-0 point at `position` with no tags and priority 0.
    pub fn at(id: u32, order: u64, position: [f32; 3]) -> Self {
        Self {
            id,
            order,
            position,
            team: 0,
            tags: Vec::new(),
            priority: 0,
            enabled: true,
            blocked: false,
        }
    }

    /// The walk order key: higher priority first, then arrival order, then id.
    pub fn sort_key(&self) -> SpawnKey {
        SpawnKey {
            priority: -i32::from(self.priority),
            order: self.order,
            id: self.id,
        }
    }
}

/// The order points are walked in (see [`SpawnPointData::sort_key`]); smaller comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct SpawnKey {
    /// Negated priority, so a higher priority sorts first.
    pub priority: i32,
    /// Arrival order.
    pub order: u64,
    /// Id.
    pub id: u32,
}

/// Weights of [`SpawnPolicy::Scored`]. Every term is an integer, so two peers add the same score.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnWeights {
    /// Points per meter of distance to the nearest enemy, up to `enemy_cap`.
    pub enemy: i64,
    /// Distance (meters) past which an enemy is "far enough". No enemy counts as this distance.
    pub enemy_cap: f32,
    /// Points subtracted per enemy that sees the point (the host's `occluded` returns false).
    pub los: i64,
    /// Enemies farther than this (meters) are not tested for sight. `0` tests every enemy.
    pub los_range: f32,
    /// Points per living teammate within `team_radius`.
    pub team: i64,
    /// Radius (meters) of the teammate term.
    pub team_radius: f32,
    /// Points subtracted per use of the point within the last `recent_ticks` match ticks.
    pub recent: i64,
    /// Window of the recent-use term, in match ticks.
    pub recent_ticks: u64,
    /// Points per unit of [`SpawnPointData::priority`].
    pub priority: i64,
}

impl Default for SpawnWeights {
    fn default() -> Self {
        Self {
            enemy: 10,
            enemy_cap: 40.0,
            los: 500,
            los_range: 0.0,
            team: 20,
            team_radius: 8.0,
            recent: 200,
            recent_ticks: 300,
            priority: 0,
        }
    }
}

/// How a spawn point is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnPolicy {
    /// The first eligible point in walk order (S-14's behavior).
    #[default]
    First,
    /// The next eligible point after the last one this policy gave, wrapping.
    RoundRobin,
    /// A uniform draw over the eligible points from the session's [`DetRng`] stream.
    Random,
    /// The best score (see the module docs); ties go to the earlier point.
    Scored(SpawnWeights),
}

impl SpawnPolicy {
    /// Lobby word: `first`, `round_robin`, `random` or `scored`.
    pub fn word(&self) -> &'static str {
        match self {
            Self::First => "first",
            Self::RoundRobin => "round_robin",
            Self::Random => "random",
            Self::Scored(_) => "scored",
        }
    }
}

/// Who asks for a spawn point.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnAsk {
    /// The asking contestant's id, skipped when counting enemies, teammates and blockers. `0` for none.
    pub id: u32,
    /// The asking team; `0` is free-for-all (every other contestant is an enemy).
    pub team: u8,
    /// The side that team plays on now ([`crate::phases::PhaseState::side_of`]); points of side `0`
    /// or this side are eligible.
    pub side: u8,
    /// Tags a point must carry, all of them.
    pub tags: Vec<String>,
    /// A living contestant closer than this (meters) to a point blocks it. `0` turns the test off.
    pub clearance: f32,
    /// The match tick of the ask (the recent-use window ends here).
    pub tick: u64,
}

/// Why a point is not eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnRefusal {
    /// The point is disabled.
    Disabled,
    /// The point belongs to another side.
    WrongTeam,
    /// The point lacks a required tag.
    MissingTag,
    /// The host's overlap flag or a contestant inside the clearance blocks it.
    Blocked,
}

impl SpawnRefusal {
    /// Wire word.
    pub fn word(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::WrongTeam => "wrong_team",
            Self::MissingTag => "missing_tag",
            Self::Blocked => "blocked",
        }
    }
}

/// One point's eligibility and score terms ([`preview_spawn`], `rules.spawn_preview`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnTerms {
    /// The point id.
    pub id: u32,
    /// Its walk-order key.
    pub key: SpawnKey,
    /// `None` when eligible.
    pub refused: Option<SpawnRefusal>,
    /// Nearest enemy distance in whole centimeters, capped at the weights' `enemy_cap`.
    pub nearest_enemy_cm: i64,
    /// Enemies that see the point.
    pub enemies_in_sight: u32,
    /// Living teammates within the team radius.
    pub teammates_near: u32,
    /// Uses within the recent window.
    pub recent_uses: u32,
    /// The total score (the sum of the weighted terms).
    pub score: i64,
}

/// One remembered use of a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnUse {
    /// The point id.
    pub point: u32,
    /// The match tick it was given out.
    pub tick: u64,
}

/// The picker's own state, kept in the session so it is part of the state every peer compares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnState {
    /// The [`SpawnPolicy::Random`] generator.
    pub rng: DetRng,
    /// The last point [`SpawnPolicy::RoundRobin`] gave.
    pub last: Option<SpawnKey>,
    /// Recent uses, oldest first, at most [`MAX_RECENT_USES`].
    pub uses: Vec<SpawnUse>,
}

impl Default for SpawnState {
    fn default() -> Self {
        Self::seeded(0)
    }
}

impl SpawnState {
    /// A state whose generator is stream [`SPAWN_RNG_STREAM`] of `seed`.
    pub fn seeded(seed: u64) -> Self {
        Self {
            rng: DetRng::seed_from(seed).stream(SPAWN_RNG_STREAM),
            last: None,
            uses: Vec::new(),
        }
    }

    /// Uses of `point` at or after `since`.
    pub fn uses_since(&self, point: u32, since: u64) -> u32 {
        let mut n = 0u32;
        for u in &self.uses {
            if u.point == point && u.tick >= since {
                n = n.saturating_add(1);
            }
        }
        n
    }

    fn remember(&mut self, point: u32, tick: u64) {
        if self.uses.len() >= MAX_RECENT_USES {
            self.uses.remove(0);
        }
        self.uses.push(SpawnUse { point, tick });
    }
}

/// The point [`pick_spawn`] chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnChoice {
    /// The point id.
    pub id: u32,
    /// Its index in the slice passed to [`pick_spawn`].
    pub index: usize,
    /// Its score under [`SpawnPolicy::Scored`]; `0` for the other policies.
    pub score: i64,
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

/// `meters` in whole centimeters, saturating; a NaN or negative value is 0.
fn to_cm(meters: f32) -> i64 {
    if meters > 0.0 { (meters * 100.0) as i64 } else { 0 }
}

fn is_enemy(ask: &SpawnAsk, c: &Contestant) -> bool {
    c.alive && c.id != ask.id && (ask.team == 0 || c.team == 0 || c.team != ask.team)
}

fn is_teammate(ask: &SpawnAsk, c: &Contestant) -> bool {
    c.alive && c.id != ask.id && ask.team != 0 && c.team == ask.team
}

/// Walk order: indices of the first [`MAX_SPAWN_POINTS`] points sorted by [`SpawnPointData::sort_key`].
fn walk_order(points: &[SpawnPointData]) -> Vec<usize> {
    let n = points.len().min(MAX_SPAWN_POINTS);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (points[i].sort_key(), i));
    order
}

fn refusal(point: &SpawnPointData, ask: &SpawnAsk, contestants: &[Contestant]) -> Option<SpawnRefusal> {
    if !point.enabled {
        return Some(SpawnRefusal::Disabled);
    }
    if point.team != 0 && point.team != ask.side {
        return Some(SpawnRefusal::WrongTeam);
    }
    if ask.tags.iter().any(|t| !point.tags.contains(t)) {
        return Some(SpawnRefusal::MissingTag);
    }
    if point.blocked {
        return Some(SpawnRefusal::Blocked);
    }
    if ask.clearance > 0.0 {
        let r2 = ask.clearance * ask.clearance;
        if contestants
            .iter()
            .any(|c| c.alive && c.id != ask.id && dist2(c.position, point.position) < r2)
        {
            return Some(SpawnRefusal::Blocked);
        }
    }
    None
}

fn terms<O>(
    point: &SpawnPointData,
    ask: &SpawnAsk,
    contestants: &[Contestant],
    weights: &SpawnWeights,
    state: &SpawnState,
    occluded: &mut O,
) -> SpawnTerms
where
    O: FnMut([f32; 3], [f32; 3]) -> bool,
{
    let refused = refusal(point, ask, contestants);
    let cap2 = weights.enemy_cap * weights.enemy_cap;
    let mut nearest2 = cap2;
    let mut in_sight = 0u32;
    let mut mates = 0u32;
    let team2 = weights.team_radius * weights.team_radius;
    let los2 = weights.los_range * weights.los_range;
    for c in contestants {
        let d2 = dist2(c.position, point.position);
        if is_enemy(ask, c) {
            if d2 < nearest2 {
                nearest2 = d2;
            }
            let in_range = weights.los_range <= 0.0 || d2 <= los2;
            if weights.los != 0 && in_range && !occluded(c.position, point.position) {
                in_sight = in_sight.saturating_add(1);
            }
        } else if is_teammate(ask, c) && d2 <= team2 {
            mates = mates.saturating_add(1);
        }
    }
    let nearest_cm = to_cm(math::sqrt(nearest2));
    let since = ask.tick.saturating_sub(weights.recent_ticks);
    let recent = if weights.recent_ticks == 0 {
        0
    } else {
        state.uses_since(point.id, since)
    };
    let score = weights
        .enemy
        .saturating_mul(nearest_cm)
        .saturating_div(100)
        .saturating_sub(weights.los.saturating_mul(i64::from(in_sight)))
        .saturating_add(weights.team.saturating_mul(i64::from(mates)))
        .saturating_sub(weights.recent.saturating_mul(i64::from(recent)))
        .saturating_add(weights.priority.saturating_mul(i64::from(point.priority)));
    SpawnTerms {
        id: point.id,
        key: point.sort_key(),
        refused,
        nearest_enemy_cm: nearest_cm,
        enemies_in_sight: in_sight,
        teammates_near: mates,
        recent_uses: recent,
        score,
    }
}

/// Every point's eligibility and score terms, in walk order. The terms use `weights` whatever the
/// session's policy is, so a debug overlay can show what [`SpawnPolicy::Scored`] would see.
pub fn preview_spawn<O>(
    points: &[SpawnPointData],
    ask: &SpawnAsk,
    contestants: &[Contestant],
    weights: &SpawnWeights,
    state: &SpawnState,
    mut occluded: O,
) -> Vec<SpawnTerms>
where
    O: FnMut([f32; 3], [f32; 3]) -> bool,
{
    walk_order(points)
        .into_iter()
        .map(|i| terms(&points[i], ask, contestants, weights, state, &mut occluded))
        .collect()
}

/// Choose a spawn point for `ask` (see the module docs). Returns `None` when no point is eligible;
/// the state is then unchanged. On a pick the state remembers the use (and, for
/// [`SpawnPolicy::RoundRobin`], the point; for [`SpawnPolicy::Random`], the draw).
///
/// `occluded(a, b)` must answer the same on every peer for the same two points: use map data, not a
/// peer's local physics state. It is only called under [`SpawnPolicy::Scored`].
pub fn pick_spawn<O>(
    points: &[SpawnPointData],
    ask: &SpawnAsk,
    contestants: &[Contestant],
    policy: &SpawnPolicy,
    state: &mut SpawnState,
    mut occluded: O,
) -> Option<SpawnChoice>
where
    O: FnMut([f32; 3], [f32; 3]) -> bool,
{
    let eligible: Vec<usize> = walk_order(points)
        .into_iter()
        .filter(|&i| refusal(&points[i], ask, contestants).is_none())
        .collect();
    let first = *eligible.first()?;
    let (index, score) = match policy {
        SpawnPolicy::First => (first, 0),
        SpawnPolicy::RoundRobin => {
            let next = state
                .last
                .and_then(|last| eligible.iter().copied().find(|&i| points[i].sort_key() > last));
            (next.unwrap_or(first), 0)
        }
        SpawnPolicy::Random => {
            let n = u32::try_from(eligible.len()).unwrap_or(u32::MAX);
            let at = state.rng.range_u32(n);
            (eligible[at as usize], 0)
        }
        SpawnPolicy::Scored(weights) => {
            let mut best = first;
            let mut best_score = i64::MIN;
            for &i in &eligible {
                let t = terms(&points[i], ask, contestants, weights, state, &mut occluded);
                // Strictly greater: a tie keeps the earlier point in walk order.
                if t.score > best_score {
                    best = i;
                    best_score = t.score;
                }
            }
            (best, best_score)
        }
    };
    let point = &points[index];
    state.last = Some(point.sort_key());
    state.remember(point.id, ask.tick);
    Some(SpawnChoice {
        id: point.id,
        index,
        score,
    })
}

// ---------------------------------------------------------------------------
// Respawn policy
// ---------------------------------------------------------------------------

/// When a dead seat comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RespawnPolicy {
    /// On the next step.
    #[default]
    Instant,
    /// This many match ticks after the death.
    Delay {
        /// Ticks to wait.
        ticks: u64,
    },
    /// Everyone dead is released together on the next multiple of `period` match ticks.
    Waves {
        /// Wave period in match ticks; `0` behaves as [`RespawnPolicy::Instant`].
        period: u64,
    },
    /// Instant while lives remain. The death that uses the last life eliminates the seat.
    Lives {
        /// Lives a seat starts the match with; `0` and `1` both mean the first death eliminates.
        lives: u32,
    },
    /// No respawn: the first death eliminates the seat.
    Never,
}

impl RespawnPolicy {
    /// The tick a seat that died at `tick` is released at, or `None` when it is eliminated
    /// (`lives_left` is the seat's lives before this death, for [`RespawnPolicy::Lives`]).
    pub fn due(&self, tick: u64, lives_left: u32) -> Option<u64> {
        match *self {
            Self::Instant => Some(tick),
            Self::Delay { ticks } => Some(tick.saturating_add(ticks)),
            Self::Waves { period } => Some(
                tick.checked_div(period)
                    .map_or(tick, |waves| waves.saturating_add(1).saturating_mul(period)),
            ),
            Self::Lives { .. } => (lives_left > 1).then_some(tick),
            Self::Never => None,
        }
    }

    /// Starting lives under this policy; `None` when lives are not counted.
    pub fn starting_lives(&self) -> Option<u32> {
        match *self {
            Self::Lives { lives } => Some(lives.max(1)),
            Self::Never => Some(1),
            _ => None,
        }
    }
}

/// One team's respawn policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRespawn {
    /// The team.
    pub team: u8,
    /// Its policy.
    pub policy: RespawnPolicy,
}

/// Respawn and spawn-selection settings of a session (`SessionSettings::respawn`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RespawnRules {
    /// Policy for a team without its own row.
    pub default: RespawnPolicy,
    /// Per-team overrides, consulted by team id.
    pub teams: Vec<TeamRespawn>,
    /// How spawn points are chosen.
    pub spawn: SpawnPolicy,
    /// Seed of the [`SpawnPolicy::Random`] stream.
    pub seed: u64,
}

impl RespawnRules {
    /// The policy of `team`.
    pub fn policy_of(&self, team: u8) -> RespawnPolicy {
        self.teams
            .iter()
            .find(|t| t.team == team)
            .map_or(self.default, |t| t.policy)
    }
}

/// A seat waiting to come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PendingRespawn {
    /// The match tick it is released on.
    pub due: u64,
    /// The seat.
    pub seat: SeatId,
}

/// Lives one seat has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatLivesLeft {
    /// The seat.
    pub seat: SeatId,
    /// Lives left.
    pub lives: u32,
}

/// The respawn state of the running match: pending releases, lives and eliminated seats, all in
/// sorted vectors. A match launch clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RespawnBook {
    /// Seats waiting, sorted by due tick then seat.
    pub pending: Vec<PendingRespawn>,
    /// Lives left per seat whose policy counts lives, sorted by seat.
    pub lives: Vec<SeatLivesLeft>,
    /// Eliminated seats, sorted.
    pub eliminated: Vec<SeatId>,
}

impl RespawnBook {
    /// Lives `seat` has left under `policy` (its starting lives before its first death); `None`
    /// when the policy does not count lives.
    pub fn lives_of(&self, seat: SeatId, policy: &RespawnPolicy) -> Option<u32> {
        let start = policy.starting_lives()?;
        Some(
            self.lives
                .iter()
                .find(|l| l.seat == seat)
                .map_or(start, |l| l.lives),
        )
    }

    /// Whether `seat` is out of the match.
    pub fn is_eliminated(&self, seat: SeatId) -> bool {
        self.eliminated.binary_search(&seat).is_ok()
    }

    /// The tick `seat` comes back on, when it is waiting.
    pub fn due_of(&self, seat: SeatId) -> Option<u64> {
        self.pending.iter().find(|p| p.seat == seat).map(|p| p.due)
    }

    /// Record the death of `seat` at `tick` under `policy`. An eliminated or already waiting seat
    /// is ignored. Pushes [`RulesEvent::Eliminated`] when this death ends the seat's match; an
    /// instant respawn is released by the next [`RespawnBook::release`].
    pub fn note_death(
        &mut self,
        seat: SeatId,
        tick: u64,
        policy: &RespawnPolicy,
        events: &mut Vec<RulesEvent>,
    ) {
        if self.is_eliminated(seat) || self.due_of(seat).is_some() {
            return;
        }
        let before = self.lives_of(seat, policy);
        let due = policy.due(tick, before.unwrap_or(u32::MAX));
        if let Some(before) = before {
            let left = before.saturating_sub(1);
            match self.lives.binary_search_by_key(&seat, |l| l.seat) {
                Ok(at) => self.lives[at].lives = left,
                Err(at) => self.lives.insert(at, SeatLivesLeft { seat, lives: left }),
            }
        }
        match due {
            Some(due) => {
                let row = PendingRespawn { due, seat };
                let at = self.pending.binary_search(&row).unwrap_or_else(|at| at);
                self.pending.insert(at, row);
            }
            None => {
                if let Err(at) = self.eliminated.binary_search(&seat) {
                    self.eliminated.insert(at, seat);
                }
                events.push(RulesEvent::Eliminated {
                    tick,
                    team: seat.team,
                    slot: seat.slot,
                });
            }
        }
    }

    /// Release every seat due at or before `tick`, in (due, seat) order, with one
    /// [`RulesEvent::RespawnAt`] each.
    pub fn release(&mut self, tick: u64, events: &mut Vec<RulesEvent>) {
        let n = self.pending.iter().take_while(|p| p.due <= tick).count();
        for p in self.pending.drain(..n) {
            events.push(RulesEvent::RespawnAt {
                tick,
                team: p.seat.team,
                slot: p.seat.slot,
            });
        }
    }

    /// Drop every row of `seat`: its pending release, its lives count and its elimination. A
    /// leave and a fresh seat call this, so neither an empty seat nor its next occupant keeps the
    /// last player's respawn state (ADR-8704).
    pub fn forget(&mut self, seat: SeatId) {
        self.pending.retain(|p| p.seat != seat);
        self.lives.retain(|l| l.seat != seat);
        self.eliminated.retain(|s| *s != seat);
    }

    /// Move the rows of each `(from, to)` seat move to `to` in one batch, so a swap keeps both
    /// players' rows (ADR-8704). A pending release keeps its due tick, a lives count its value and
    /// an elimination stays an elimination: a team change is not a way back into the match. Rows a
    /// destination held before the move belonged to nobody seated there and are dropped. Moves
    /// with `from == to` change nothing.
    pub fn carry(&mut self, moves: &[(SeatId, SeatId)]) {
        let moves: Vec<(SeatId, SeatId)> = moves.iter().copied().filter(|(from, to)| from != to).collect();
        if moves.is_empty() {
            return;
        }
        let mut pending = Vec::new();
        let mut lives = Vec::new();
        let mut eliminated = Vec::new();
        for (from, to) in &moves {
            if let Some(p) = self.pending.iter().find(|p| p.seat == *from) {
                pending.push(PendingRespawn {
                    due: p.due,
                    seat: *to,
                });
            }
            if let Some(l) = self.lives.iter().find(|l| l.seat == *from) {
                lives.push(SeatLivesLeft {
                    seat: *to,
                    lives: l.lives,
                });
            }
            if self.is_eliminated(*from) {
                eliminated.push(*to);
            }
        }
        for (from, to) in &moves {
            self.forget(*from);
            self.forget(*to);
        }
        for row in pending {
            let at = self.pending.binary_search(&row).unwrap_or_else(|at| at);
            self.pending.insert(at, row);
        }
        for row in lives {
            match self.lives.binary_search_by_key(&row.seat, |l| l.seat) {
                Ok(at) => self.lives[at] = row,
                Err(at) => self.lives.insert(at, row),
            }
        }
        for seat in eliminated {
            if let Err(at) = self.eliminated.binary_search(&seat) {
                self.eliminated.insert(at, seat);
            }
        }
    }

    /// Forget everything (a new match).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.lives.clear();
        self.eliminated.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
        Contestant {
            id,
            team,
            position,
            alive: true,
        }
    }

    fn sided(id: u32, team: u8, position: [f32; 3]) -> SpawnPointData {
        SpawnPointData {
            team,
            ..SpawnPointData::at(id, u64::from(id), position)
        }
    }

    fn open(_: [f32; 3], _: [f32; 3]) -> bool {
        false
    }

    #[test]
    fn first_policy_takes_the_first_point_in_spawn_order() {
        let points = vec![
            SpawnPointData::at(7, 2, [7.0, 0.0, 0.0]),
            SpawnPointData::at(3, 1, [3.0, 0.0, 0.0]),
        ];
        let mut state = SpawnState::default();
        let pick = pick_spawn(
            &points,
            &SpawnAsk::default(),
            &[],
            &SpawnPolicy::First,
            &mut state,
            open,
        );
        assert_eq!(pick.map(|p| p.id), Some(3));
    }

    #[test]
    fn team_spawn_never_picks_an_enemy_point() {
        let points = vec![
            sided(1, 1, [0.0, 0.0, 0.0]),
            sided(2, 2, [10.0, 0.0, 0.0]),
            sided(3, 1, [20.0, 0.0, 0.0]),
            sided(4, 2, [30.0, 0.0, 0.0]),
        ];
        for policy in [
            SpawnPolicy::First,
            SpawnPolicy::RoundRobin,
            SpawnPolicy::Random,
            SpawnPolicy::Scored(SpawnWeights::default()),
        ] {
            let mut state = SpawnState::seeded(9);
            for tick in 0..32 {
                // Team 2 on its own side.
                let ask = SpawnAsk {
                    team: 2,
                    side: 2,
                    tick,
                    ..SpawnAsk::default()
                };
                let pick =
                    pick_spawn(&points, &ask, &[], &policy, &mut state, open).expect("team 2 has points");
                assert!(matches!(pick.id, 2 | 4), "{policy:?} gave {pick:?}");
                // After a side swap team 2 plays on side 1.
                let swapped = SpawnAsk { side: 1, ..ask };
                let pick =
                    pick_spawn(&points, &swapped, &[], &policy, &mut state, open).expect("side 1 has points");
                assert!(matches!(pick.id, 1 | 3), "{policy:?} swapped gave {pick:?}");
            }
        }
    }

    #[test]
    fn scored_spawn_prefers_the_point_far_from_enemies_and_out_of_their_sight() {
        // A: next to the enemy. B: far, but in its sight. C: as far as B, behind a wall.
        let points = vec![
            SpawnPointData::at(1, 1, [2.0, 0.0, 0.0]),
            SpawnPointData::at(2, 2, [30.0, 0.0, 0.0]),
            SpawnPointData::at(3, 3, [0.0, 0.0, 30.0]),
        ];
        let enemy = unit(50, 2, [0.0, 0.0, 0.0]);
        let ask = SpawnAsk {
            id: 10,
            team: 1,
            side: 1,
            ..SpawnAsk::default()
        };
        let wall_hides_c = |_from: [f32; 3], to: [f32; 3]| to[2] > 1.0;
        let mut state = SpawnState::default();
        let policy = SpawnPolicy::Scored(SpawnWeights::default());
        let pick = pick_spawn(&points, &ask, &[enemy], &policy, &mut state, wall_hides_c);
        assert_eq!(pick.map(|p| p.id), Some(3));
        // Without the wall, B and C tie on distance; the earlier point (B) wins the tie.
        let mut state = SpawnState::default();
        let pick = pick_spawn(&points, &ask, &[enemy], &policy, &mut state, open);
        assert_eq!(pick.map(|p| p.id), Some(2));
        let terms = preview_spawn(
            &points,
            &ask,
            &[enemy],
            &SpawnWeights::default(),
            &SpawnState::default(),
            wall_hides_c,
        );
        assert_eq!(terms[0].nearest_enemy_cm, 200);
        assert_eq!(terms[1].enemies_in_sight, 1);
        assert_eq!(terms[2].enemies_in_sight, 0);
        assert!(terms[2].score > terms[1].score && terms[1].score > terms[0].score);
    }

    #[test]
    fn a_blocked_spawn_point_is_skipped() {
        let mut points = vec![
            SpawnPointData::at(1, 1, [0.0, 0.0, 0.0]),
            SpawnPointData::at(2, 2, [10.0, 0.0, 0.0]),
            SpawnPointData::at(3, 3, [20.0, 0.0, 0.0]),
        ];
        points[0].blocked = true;
        // A teammate stands on point 2: inside the clearance, so it blocks too.
        let mate = unit(20, 1, [10.2, 0.0, 0.0]);
        let ask = SpawnAsk {
            id: 10,
            team: 1,
            side: 1,
            clearance: 0.8,
            ..SpawnAsk::default()
        };
        let mut state = SpawnState::default();
        let pick = pick_spawn(&points, &ask, &[mate], &SpawnPolicy::First, &mut state, open);
        assert_eq!(pick.map(|p| p.id), Some(3));
        // Everything blocked: no point, state untouched.
        points[2].blocked = true;
        let before = state.clone();
        let pick = pick_spawn(&points, &ask, &[mate], &SpawnPolicy::First, &mut state, open);
        assert_eq!(pick, None);
        assert_eq!(state, before);
    }

    #[test]
    fn round_robin_cycles_points_in_spawn_order() {
        // Listed out of order; spawn order is by `order`.
        let points = vec![
            SpawnPointData::at(30, 3, [3.0, 0.0, 0.0]),
            SpawnPointData::at(10, 1, [1.0, 0.0, 0.0]),
            SpawnPointData::at(20, 2, [2.0, 0.0, 0.0]),
        ];
        let mut state = SpawnState::default();
        let ids: Vec<u32> = (0..7)
            .filter_map(|tick| {
                let ask = SpawnAsk {
                    tick,
                    ..SpawnAsk::default()
                };
                pick_spawn(&points, &ask, &[], &SpawnPolicy::RoundRobin, &mut state, open).map(|p| p.id)
            })
            .collect();
        assert_eq!(ids, vec![10, 20, 30, 10, 20, 30, 10]);
    }

    #[test]
    fn priority_walks_first_and_disabled_points_are_skipped() {
        let mut points = vec![
            SpawnPointData::at(1, 1, [0.0; 3]),
            SpawnPointData::at(2, 2, [0.0; 3]),
            SpawnPointData::at(3, 3, [0.0; 3]),
        ];
        points[2].priority = 5;
        points[1].enabled = false;
        let mut state = SpawnState::default();
        let ask = SpawnAsk::default();
        let ids: Vec<u32> = (0..3)
            .filter_map(|_| pick_spawn(&points, &ask, &[], &SpawnPolicy::RoundRobin, &mut state, open))
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec![3, 1, 3]);
    }

    #[test]
    fn required_tags_filter_points() {
        let mut points = vec![
            SpawnPointData::at(1, 1, [0.0; 3]),
            SpawnPointData::at(2, 2, [0.0; 3]),
        ];
        points[1].tags = vec!["attack".into()];
        let ask = SpawnAsk {
            tags: vec!["attack".into()],
            ..SpawnAsk::default()
        };
        let mut state = SpawnState::default();
        let pick = pick_spawn(&points, &ask, &[], &SpawnPolicy::First, &mut state, open);
        assert_eq!(pick.map(|p| p.id), Some(2));
        let terms = preview_spawn(&points, &ask, &[], &SpawnWeights::default(), &state, open);
        assert_eq!(terms[0].refused, Some(SpawnRefusal::MissingTag));
        assert_eq!(terms[1].refused, None);
    }

    #[test]
    fn recent_use_pushes_scored_picks_to_another_point() {
        let points = vec![
            SpawnPointData::at(1, 1, [0.0; 3]),
            SpawnPointData::at(2, 2, [5.0, 0.0, 0.0]),
        ];
        let policy = SpawnPolicy::Scored(SpawnWeights::default());
        let mut state = SpawnState::default();
        let ask = |tick| SpawnAsk {
            tick,
            ..SpawnAsk::default()
        };
        let a = pick_spawn(&points, &ask(0), &[], &policy, &mut state, open).map(|p| p.id);
        let b = pick_spawn(&points, &ask(1), &[], &policy, &mut state, open).map(|p| p.id);
        assert_eq!((a, b), (Some(1), Some(2)));
        // Past the window both are fresh again.
        let c = pick_spawn(&points, &ask(10_000), &[], &policy, &mut state, open).map(|p| p.id);
        assert_eq!(c, Some(1));
    }

    #[test]
    fn random_spawn_draws_from_the_seeded_stream() {
        let points: Vec<SpawnPointData> = (1..=8)
            .map(|i| SpawnPointData::at(i, u64::from(i), [0.0; 3]))
            .collect();
        let draw = |seed| {
            let mut state = SpawnState::seeded(seed);
            (0..16)
                .filter_map(|_| {
                    pick_spawn(
                        &points,
                        &SpawnAsk::default(),
                        &[],
                        &SpawnPolicy::Random,
                        &mut state,
                        open,
                    )
                })
                .map(|p| p.id)
                .collect::<Vec<u32>>()
        };
        assert_eq!(draw(4), draw(4));
        assert_ne!(draw(4), draw(5));
        assert!(draw(4).iter().any(|&id| id != draw(4)[0]));
    }

    #[test]
    fn pick_spawn_core_is_bit_identical_on_two_peers() {
        // Two peers with the same inputs, one given the points and contestants in reverse order:
        // every pick, every score and the whole state match after 200 asks.
        let mut points: Vec<SpawnPointData> = (0..24u32)
            .map(|i| {
                let f = i as f32;
                SpawnPointData {
                    team: (i % 3) as u8,
                    priority: (i % 4) as i16 - 1,
                    ..SpawnPointData::at(100 + i, u64::from(i), [f * 3.1, 0.25 * f, 40.0 - f * 1.7])
                }
            })
            .collect();
        points[5].enabled = false;
        let units: Vec<Contestant> = (0..12u32)
            .map(|i| {
                let f = i as f32;
                Contestant {
                    id: i + 1,
                    team: (i % 2 + 1) as u8,
                    position: [f * 5.3 - 20.0, 0.0, f * 2.9],
                    alive: i % 5 != 0,
                }
            })
            .collect();
        let occluded = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]) * (a[2] - b[2]) > 30.0;
        let weights = SpawnWeights {
            priority: 7,
            los_range: 25.0,
            ..SpawnWeights::default()
        };
        let policies = [
            SpawnPolicy::First,
            SpawnPolicy::RoundRobin,
            SpawnPolicy::Random,
            SpawnPolicy::Scored(weights),
        ];
        let run = |reverse: bool| {
            let mut pts = points.clone();
            let mut us = units.clone();
            if reverse {
                pts.reverse();
                // Contestant order matters to nothing but counts: reversed must agree.
                us.reverse();
            }
            let mut state = SpawnState::seeded(0xfeed);
            let mut out = Vec::new();
            for tick in 0..200u64 {
                let team = (tick % 2 + 1) as u8;
                let ask = SpawnAsk {
                    id: (tick % 12 + 1) as u32,
                    team,
                    side: if tick < 100 { team } else { 3 - team },
                    clearance: 1.5,
                    tick,
                    ..SpawnAsk::default()
                };
                let policy = &policies[(tick % 4) as usize];
                let pick = pick_spawn(&pts, &ask, &us, policy, &mut state, occluded);
                out.push(pick.map(|p| (p.id, p.score)));
            }
            (out, state)
        };
        let (a, sa) = run(false);
        let (b, sb) = run(true);
        assert_eq!(a, b);
        assert_eq!(sa, sb);
        assert!(a.iter().flatten().count() > 150);
    }

    #[test]
    fn delay_and_waves_compute_due_ticks() {
        assert_eq!(RespawnPolicy::Instant.due(7, 0), Some(7));
        assert_eq!(RespawnPolicy::Delay { ticks: 5 }.due(7, 0), Some(12));
        assert_eq!(RespawnPolicy::Waves { period: 10 }.due(7, 0), Some(10));
        assert_eq!(RespawnPolicy::Waves { period: 10 }.due(10, 0), Some(20));
        assert_eq!(RespawnPolicy::Waves { period: 0 }.due(10, 0), Some(10));
        assert_eq!(RespawnPolicy::Never.due(3, 1), None);
        assert_eq!(RespawnPolicy::Lives { lives: 3 }.due(3, 2), Some(3));
        assert_eq!(RespawnPolicy::Lives { lives: 3 }.due(3, 1), None);
    }

    #[test]
    fn wave_respawn_releases_everyone_on_the_wave_tick() {
        let policy = RespawnPolicy::Waves { period: 20 };
        let mut book = RespawnBook::default();
        let mut events = Vec::new();
        let seats = [SeatId::new(1, 0), SeatId::new(2, 1), SeatId::new(1, 2)];
        book.note_death(seats[0], 3, &policy, &mut events);
        book.note_death(seats[1], 11, &policy, &mut events);
        book.note_death(seats[2], 19, &policy, &mut events);
        assert!(events.is_empty());
        for tick in 0..20 {
            book.release(tick, &mut events);
            assert!(events.is_empty(), "released early at {tick}");
        }
        book.release(20, &mut events);
        let released: Vec<(u64, u8, u8)> = events
            .iter()
            .filter_map(|e| match e {
                RulesEvent::RespawnAt { tick, team, slot } => Some((*tick, *team, *slot)),
                _ => None,
            })
            .collect();
        assert_eq!(released, vec![(20, 1, 0), (20, 1, 2), (20, 2, 1)]);
        assert!(book.pending.is_empty());
    }

    #[test]
    fn lives_zero_eliminates_and_reports_it() {
        let policy = RespawnPolicy::Lives { lives: 2 };
        let seat = SeatId::new(1, 0);
        let mut book = RespawnBook::default();
        let mut events = Vec::new();
        assert_eq!(book.lives_of(seat, &policy), Some(2));
        book.note_death(seat, 5, &policy, &mut events);
        assert_eq!(book.lives_of(seat, &policy), Some(1));
        book.release(5, &mut events);
        assert!(matches!(
            events[..],
            [RulesEvent::RespawnAt {
                tick: 5,
                team: 1,
                slot: 0
            }]
        ));
        events.clear();
        book.note_death(seat, 9, &policy, &mut events);
        assert_eq!(book.lives_of(seat, &policy), Some(0));
        assert!(book.is_eliminated(seat));
        assert!(matches!(
            events[..],
            [RulesEvent::Eliminated {
                tick: 9,
                team: 1,
                slot: 0
            }]
        ));
        // Nothing comes back, and a later death report is ignored.
        events.clear();
        book.release(1000, &mut events);
        book.note_death(seat, 12, &policy, &mut events);
        assert!(events.is_empty());
        // Never: the first death eliminates.
        let mut book = RespawnBook::default();
        book.note_death(seat, 1, &RespawnPolicy::Never, &mut events);
        assert!(book.is_eliminated(seat));
    }

    #[test]
    fn the_session_books_deaths_and_releases_them_on_its_steps() {
        use crate::session::{MatchSession, SessionSettings};

        let mut session = MatchSession::new(SessionSettings {
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 3 },
                teams: vec![TeamRespawn {
                    team: 2,
                    policy: RespawnPolicy::Never,
                }],
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        });
        session.start_now();
        for _ in 0..2 {
            session.step(&[]);
        }
        let died_at = session.flow.match_tick;
        session.note_death(SeatId::new(1, 0));
        session.note_death(SeatId::new(2, 0));
        let mut released = Vec::new();
        let mut eliminated = Vec::new();
        for _ in 0..10 {
            for event in session.step(&[]).events {
                match event {
                    RulesEvent::RespawnAt { tick, team, slot } => released.push((tick, team, slot)),
                    RulesEvent::Eliminated { tick, team, slot } => eliminated.push((tick, team, slot)),
                    _ => {}
                }
            }
        }
        assert_eq!(released, vec![(died_at + 3, 1, 0)]);
        assert_eq!(eliminated, vec![(died_at, 2, 0)]);
        // A launch clears the book.
        assert_eq!(session.respawns.eliminated, vec![SeatId::new(2, 0)]);
        session.start_now();
        assert_eq!(session.respawns, RespawnBook::default());
    }

    #[test]
    fn the_session_spawn_ask_follows_the_side_swap() {
        use crate::session::MatchSession;

        let mut session = MatchSession::default();
        assert_eq!(session.spawn_ask(2).side, 2);
        session.flow.phases.sides = vec![0, 2, 1];
        assert_eq!(session.spawn_ask(2).side, 1);
        let points = vec![sided(1, 1, [0.0; 3]), sided(2, 2, [9.0, 0.0, 0.0])];
        let ask = SpawnAsk {
            team: 2,
            ..SpawnAsk::default()
        };
        let pick = session.pick_spawn(&points, &ask, &[], open);
        assert_eq!(pick.map(|p| p.id), Some(1));
        assert_eq!(session.spawn.uses.len(), 1);
    }

    /// ADR-8704: `forget` drops all three rows of one seat and leaves other seats alone.
    #[test]
    fn forget_drops_only_that_seats_rows() {
        let (a, b) = (SeatId::new(1, 0), SeatId::new(2, 0));
        let mut book = RespawnBook::default();
        let mut events = Vec::new();
        let lives = RespawnPolicy::Lives { lives: 3 };
        book.note_death(a, 5, &lives, &mut events);
        book.note_death(b, 5, &RespawnPolicy::Delay { ticks: 10 }, &mut events);
        book.release(5, &mut events);
        // `Never` counts one life, so this death uses another of the three and eliminates.
        book.note_death(a, 6, &RespawnPolicy::Never, &mut events);
        assert!(book.is_eliminated(a));
        assert_eq!(book.lives_of(a, &lives), Some(1));
        book.forget(a);
        assert!(!book.is_eliminated(a));
        assert_eq!(book.lives_of(a, &lives), Some(3), "full lives again");
        assert_eq!(book.due_of(b), Some(15), "the other seat keeps its booking");
    }

    /// ADR-8704: `carry` moves due ticks, lives and eliminations; a swap keeps both players'
    /// rows; a row the destination held for nobody is dropped; `from == to` changes nothing.
    #[test]
    fn carry_moves_rows_and_keeps_a_swap() {
        let (a, b, c, d) = (
            SeatId::new(1, 0),
            SeatId::new(2, 0),
            SeatId::new(3, 0),
            SeatId::new(4, 0),
        );
        let lives = RespawnPolicy::Lives { lives: 3 };
        let delay = RespawnPolicy::Delay { ticks: 10 };
        let mut events = Vec::new();
        let mut book = RespawnBook::default();
        book.note_death(a, 1, &delay, &mut events); // a due 11
        book.note_death(b, 2, &lives, &mut events); // b 2 lives, due 2
        book.release(2, &mut events); // b released, a still waiting
        book.note_death(c, 3, &RespawnPolicy::Never, &mut events); // c eliminated
        book.note_death(d, 4, &delay, &mut events); // d due 14: a stale row nobody owns
        let before = book.clone();
        book.carry(&[(a, a)]);
        assert_eq!(book, before, "a move onto the same seat is a no-op");

        // Swap a and b, move c onto d's coordinate.
        book.carry(&[(a, b), (b, a), (c, d)]);
        assert_eq!(book.due_of(b), Some(11), "a's booking now sits on b");
        assert_eq!(book.due_of(a), None);
        assert_eq!(book.lives_of(a, &lives), Some(2), "b's lives now sit on a");
        assert_eq!(book.lives_of(b, &lives), Some(3));
        assert!(book.is_eliminated(d), "an elimination survives a team change");
        assert!(!book.is_eliminated(c));
        assert_eq!(
            book.pending,
            vec![PendingRespawn { due: 11, seat: b }],
            "d's stale due-14 row is dropped and the list stays sorted"
        );
    }
}
