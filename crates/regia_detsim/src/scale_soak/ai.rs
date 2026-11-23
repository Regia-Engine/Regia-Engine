// SPDX-License-Identifier: MPL-2.0
//! AI-on scale soak (ADR-8561): seats, teams, range queries, shots, contacts and ID recycling.
//!
//! An engine-original synthetic workload built from the S-18 arena idea (units pursue
//! opponents) and the spatial index of ADR-8318/ADR-8478. It is not a game's AI, combat
//! or tuning: every count, radius, speed and damage is an [`AiSoakConfig`] setting whose
//! defaults were chosen for this soak only.
//!
//! One tick runs ten systems, in this order, each named on [`AI_SOAK_SYSTEMS`] so the
//! tick profiler (ADR-8498) can time them and `sim.tick_profile` can report one row each:
//!
//! 1. `input`: every seat computes a drive direction.
//! 2. `build`: one [`DetSpatial`] index over all units.
//! 3. `acquire`: each unit takes the first `acquire_k` enemies within `acquire_radius`
//!    ([`DetSpatial::first_k_within_by`]).
//! 4. `target`: each unit picks the enemy with the lowest score within `engage_radius`
//!    ([`DetSpatial::within`]); equal scores keep the lowest slot.
//! 5. `ally`: the nearest teammate within `ally_radius` ([`DetSpatial::nearest`]).
//! 6. `keep_out`: every unit within `keep_out_radius` pushes the unit away ([`DetSpatial::within`]).
//! 7. `move`: integrate, clamp to the arena, and refile the index ([`DetSpatial::update`]).
//! 8. `shots`: fire up to `shots_per_tick` new shots, then sweep every live shot along its
//!    path ([`DetSpatial::first_along_segment`]); a hit splashes a burst ([`DetSpatial::within`]).
//! 9. `recycle`: units at zero health free their handle and respawn with a FIFO-reused one.
//! 10. `contacts`: ordered disc contacts ([`DetBroadphase::for_each_contact`]).
//!
//! Decisions (systems 3-6) read the start-of-tick positions; movement applies after them.
//!
//! **Oracle mode** ([`AiSoakScenario::set_oracle`]) runs a linear scan beside every query
//! and compares the accepted slot sequences. The state is the same with the oracle on or off.

use std::ops::ControlFlow;

use crate::broadphase::{
    BroadBody, BroadphaseConfig, Contact, DetBroadphase, FloorRule, bodies_on_different_floors,
};
use crate::float::{det_clamp, det_min};
use crate::golden::scenarios::SplitMix64;
use crate::golden::{DetRun, DetScenario, DetTrace, StateHasher};
use crate::hash::Systems;
use crate::ids::{DetHandle, DetIdAllocator};
use crate::profile::{
    ClockKind, DEFAULT_TICK_BUDGET_NS, HostClock, MAX_RING_TICKS, ProfileClock, ProfileError, ProfileReport,
    ProfileSettings, TickProfiler,
};
use crate::spatial::{DetSpatial, SpatialRow};
use crate::{math, tick::TickRate};

/// The ten systems of one AI soak tick, in run order. Also the tick profiler's name list.
pub const AI_SOAK_SYSTEMS: Systems<10> = Systems::new([
    "input", "build", "acquire", "target", "ally", "keep_out", "move", "shots", "recycle", "contacts",
]);

/// Default whole-tick pass line on the reference host: 4.0 ms p99 (a setting, see the book).
pub const PASS_LINE_HOST_NS: u64 = DEFAULT_TICK_BUDGET_NS;
/// Default whole-tick pass line on a handheld or phone: 8.3 ms p99 (a setting, see the book).
pub const PASS_LINE_DEVICE_NS: u64 = 8_300_000;
/// Prefix of a machine-readable AI soak report line.
pub const REPORT_PREFIX: &str = "regia-ai-soak ";

const NONE: u32 = u32::MAX;
const MAX_TEAMS: u8 = 64;

/// One system of the AI soak tick. [`AiSystem::name`] is its profiler row name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSystem {
    /// Seat input.
    Input,
    /// Spatial index build.
    Build,
    /// First-k acquisition query.
    Acquire,
    /// Scored target pick.
    Target,
    /// Nearest ally.
    Ally,
    /// Keep-out separation query.
    KeepOut,
    /// Integration and index refiling.
    Move,
    /// Firing, segment sweeps and bursts.
    Shots,
    /// FIFO handle reuse for defeated units.
    Recycle,
    /// Ordered disc contacts.
    Contacts,
}

impl AiSystem {
    /// Run and report order.
    pub const ALL: [Self; 10] = [
        Self::Input,
        Self::Build,
        Self::Acquire,
        Self::Target,
        Self::Ally,
        Self::KeepOut,
        Self::Move,
        Self::Shots,
        Self::Recycle,
        Self::Contacts,
    ];

    /// Stable lowercase name, equal to the [`AI_SOAK_SYSTEMS`] entry at the same index.
    pub const fn name(self) -> &'static str {
        AI_SOAK_SYSTEMS.names()[self as usize]
    }
}

/// Settings of the synthetic load. Engine-original defaults; not game limits or rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AiSoakConfig {
    /// Teams; `2..=64` and at most `entities`. Unit `i` is on team `i % teams`.
    pub teams: u8,
    /// Input seats; `1..=entities`. Unit `i` follows seat `i % seats` when it has no contact.
    pub seats: u16,
    /// Live units, constant for the run.
    pub entities: u16,
    /// Half-width of the square arena, meters; finite, `1..=10000`.
    pub half_extent: f32,
    /// Unit disc radius, meters; finite, positive and below `half_extent`.
    pub radius: f32,
    /// Top speed, meters per second; finite, `0..=1000`.
    pub speed: f32,
    /// Acquisition radius, meters.
    pub acquire_radius: f32,
    /// Most contacts one acquisition keeps; positive.
    pub acquire_k: u16,
    /// Target-pick radius, meters.
    pub engage_radius: f32,
    /// Nearest-ally search radius, meters.
    pub ally_radius: f32,
    /// Keep-out radius, meters.
    pub keep_out_radius: f32,
    /// Spatial index cell edge, meters.
    pub query_cell: f32,
    /// Most new shots per tick; at most `max_shots`. Zero turns firing off.
    pub shots_per_tick: u16,
    /// Most live shots.
    pub max_shots: u16,
    /// Shot speed, meters per second; finite and positive.
    pub shot_speed: f32,
    /// Shot radius, meters; finite and non-negative.
    pub shot_radius: f32,
    /// Ticks a shot lives without a hit; positive.
    pub shot_life_ticks: u32,
    /// Burst radius around a hit unit, meters; finite and non-negative.
    pub burst_radius: f32,
    /// Initial and respawn health; positive.
    pub health: u32,
    /// Damage of a direct hit.
    pub shot_damage: u32,
    /// Damage of a burst to each other enemy in range.
    pub burst_damage: u32,
    /// Ticks between two shots of one unit; positive.
    pub cooldown_ticks: u32,
}

impl Default for AiSoakConfig {
    fn default() -> Self {
        Self {
            teams: 4,
            seats: 40,
            entities: 900,
            half_extent: 96.0,
            radius: 0.75,
            speed: 5.0,
            acquire_radius: 40.0,
            acquire_k: 24,
            engage_radius: 24.0,
            ally_radius: 16.0,
            keep_out_radius: 2.5,
            query_cell: 16.0,
            shots_per_tick: 32,
            max_shots: 512,
            shot_speed: 60.0,
            shot_radius: 0.15,
            shot_life_ticks: 45,
            burst_radius: 3.0,
            health: 100,
            shot_damage: 20,
            burst_damage: 8,
            cooldown_ticks: 30,
        }
    }
}

/// Invalid [`AiSoakConfig`]. [`AiSoakConfigError::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSoakConfigError {
    /// `teams` outside `2..=64` or above `entities`.
    Teams,
    /// `seats` zero or above `entities`.
    Counts,
    /// Arena half-width, unit radius or speed out of range.
    Geometry,
    /// A query radius or the index cell not finite and positive, or `acquire_k` zero.
    Queries,
    /// `shots_per_tick` above `max_shots`, or shot speed, radius, life or burst out of range.
    Shots,
    /// Health or cooldown zero.
    Combat,
}

impl AiSoakConfigError {
    /// Stable lowercase protocol code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Teams => "invalid_ai_soak_teams",
            Self::Counts => "invalid_ai_soak_counts",
            Self::Geometry => "invalid_ai_soak_geometry",
            Self::Queries => "invalid_ai_soak_queries",
            Self::Shots => "invalid_ai_soak_shots",
            Self::Combat => "invalid_ai_soak_combat",
        }
    }
}

impl std::fmt::Display for AiSoakConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            Self::Teams => "teams must be in 2..=64 and at most entities",
            Self::Counts => "seats must be in 1..=entities",
            Self::Geometry => "half_extent in 1..=10000, 0 < radius < half_extent, speed in 0..=1000",
            Self::Queries => "query radii and query_cell finite and positive, acquire_k positive",
            Self::Shots => "shots_per_tick <= max_shots, shot speed and life positive, radii finite and >= 0",
            Self::Combat => "health and cooldown_ticks positive",
        };
        write!(f, "{}: {detail}", self.code())
    }
}

impl std::error::Error for AiSoakConfigError {}

fn positive(v: f32) -> bool {
    v.is_finite() && v > 0.0 && v <= 1_000_000.0
}

fn non_negative(v: f32) -> bool {
    v.is_finite() && (0.0..=1_000_000.0).contains(&v)
}

impl AiSoakConfig {
    /// Check settings before allocating or stepping. The first failing group wins, in the
    /// order of [`AiSoakConfigError`]'s variants.
    pub fn validate(self) -> Result<(), AiSoakConfigError> {
        if !(2..=MAX_TEAMS).contains(&self.teams) || u16::from(self.teams) > self.entities {
            return Err(AiSoakConfigError::Teams);
        }
        if self.seats == 0 || self.seats > self.entities {
            return Err(AiSoakConfigError::Counts);
        }
        if !self.half_extent.is_finite()
            || !(1.0..=10000.0).contains(&self.half_extent)
            || !self.radius.is_finite()
            || self.radius <= 0.0
            || self.radius >= self.half_extent
            || !self.speed.is_finite()
            || !(0.0..=1000.0).contains(&self.speed)
        {
            return Err(AiSoakConfigError::Geometry);
        }
        let radii = [
            self.acquire_radius,
            self.engage_radius,
            self.ally_radius,
            self.keep_out_radius,
            self.query_cell,
        ];
        if !radii.into_iter().all(positive) || self.acquire_k == 0 {
            return Err(AiSoakConfigError::Queries);
        }
        if self.shots_per_tick > self.max_shots
            || !positive(self.shot_speed)
            || !non_negative(self.shot_radius)
            || !non_negative(self.burst_radius)
            || self.shot_life_ticks == 0
        {
            return Err(AiSoakConfigError::Shots);
        }
        if self.health == 0 || self.cooldown_ticks == 0 {
            return Err(AiSoakConfigError::Combat);
        }
        Ok(())
    }

    fn all_teams(self) -> u64 {
        if self.teams >= 64 {
            u64::MAX
        } else {
            (1_u64 << self.teams) - 1
        }
    }
}

/// Host profiling hook around each system. The tick profiler implements it.
pub trait AiSoakObserver {
    /// Start of a tick, before `input`.
    fn begin_tick(&mut self) {}
    /// Enter a system.
    fn begin(&mut self, system: AiSystem);
    /// Leave the system just entered.
    fn end(&mut self, system: AiSystem);
    /// End of tick `tick`, after `contacts`.
    fn end_tick(&mut self, _tick: u32) {}
}

impl AiSoakObserver for () {
    fn begin(&mut self, _: AiSystem) {}
    fn end(&mut self, _: AiSystem) {}
}

impl<C: ProfileClock> AiSoakObserver for TickProfiler<10, C> {
    fn begin_tick(&mut self) {
        TickProfiler::begin_tick(self);
    }
    fn begin(&mut self, system: AiSystem) {
        // The names are the profiler's own const list, so an unknown name cannot occur.
        let _ = TickProfiler::begin(self, system.name());
    }
    fn end(&mut self, system: AiSystem) {
        let _ = TickProfiler::end(self, system.name());
    }
    fn end_tick(&mut self, tick: u32) {
        TickProfiler::end_tick(self, u64::from(tick));
    }
}

/// The query kind of an oracle mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiQuery {
    /// `first_k_within_by` in `acquire`.
    Acquire,
    /// `within` in `target`.
    Target,
    /// `nearest` in `ally`.
    Ally,
    /// `within` in `keep_out`.
    KeepOut,
    /// `first_along_segment` in `shots`.
    Segment,
    /// `within` for a burst in `shots`.
    Burst,
}

impl AiQuery {
    /// Stable lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Acquire => "acquire",
            Self::Target => "target",
            Self::Ally => "ally",
            Self::KeepOut => "keep_out",
            Self::Segment => "segment",
            Self::Burst => "burst",
        }
    }
}

/// The first query whose grid sequence differs from the linear scan's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleMismatch {
    /// Scenario tick of the query.
    pub tick: u32,
    /// Which query.
    pub query: AiQuery,
    /// Querying unit slot, or shot index for segment and burst queries.
    pub subject: u32,
    /// Accepted slots from the index, in visit order.
    pub grid: Vec<u32>,
    /// Accepted slots from the linear scan, in slot order.
    pub linear: Vec<u32>,
}

/// Oracle counters since the last reset. No clock reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OracleReport {
    /// Queries compared, per [`AiQuery`] in declaration order.
    pub checked: [u64; 6],
    /// Queries whose sequences differed.
    pub mismatches: u64,
    /// The first mismatch, if any.
    pub first: Option<OracleMismatch>,
}

impl OracleReport {
    /// True when every compared query matched.
    pub fn passed(&self) -> bool {
        self.mismatches == 0
    }
}

/// Deterministic workload counters since reset (all hashed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AiSoakCounters {
    /// Acquired contacts summed over units and ticks.
    pub acquired: u64,
    /// Target picks that found an enemy.
    pub targets: u64,
    /// Nearest-ally hits.
    pub allies: u64,
    /// Keep-out neighbors that pushed a unit.
    pub keep_out: u64,
    /// Shots fired.
    pub shots_fired: u64,
    /// Segment sweeps (one per live shot per tick).
    pub sweeps: u64,
    /// Direct hits.
    pub hits: u64,
    /// Burst damage applications.
    pub burst_hits: u64,
    /// Handles freed and reissued.
    pub recycled: u64,
    /// Resolved disc contacts.
    pub contacts: u64,
}

#[derive(Debug, Clone)]
struct Seat {
    phase: f32,
    input: [f32; 2],
}

#[derive(Debug, Clone)]
struct Unit {
    handle: DetHandle<u16>,
    team: u8,
    velocity: [f32; 2],
    health: u32,
    cooldown: u32,
    target: u32,
    lead: u32,
    ally: u32,
    push: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct Shot {
    pos: [f32; 2],
    vel: [f32; 2],
    team: u8,
    life: u32,
}

/// Reusable per-query buffers (never part of the state).
#[derive(Debug, Clone, Default)]
struct Scratch {
    grid: Vec<usize>,
    linear: Vec<usize>,
}

/// Seeded AI-on load. Construct, then reset and step through [`DetScenario`].
pub struct AiSoakScenario {
    config: AiSoakConfig,
    seed: u64,
    seats: Vec<Seat>,
    units: Vec<Unit>,
    bodies: Vec<BroadBody>,
    rows: Vec<SpatialRow>,
    shots: Vec<Shot>,
    defeated: Vec<usize>,
    ids: DetIdAllocator<u16>,
    index: DetSpatial,
    contacts_grid: DetBroadphase,
    scratch: Scratch,
    oracle: Option<OracleReport>,
    dt: f32,
    tick: u32,
    counters: AiSoakCounters,
}

impl AiSoakScenario {
    /// Validate settings and bootstrap the default run.
    pub fn new(config: AiSoakConfig) -> Result<Self, AiSoakConfigError> {
        config.validate()?;
        let n = usize::from(config.entities);
        let mut result = Self {
            config,
            seed: 0,
            seats: Vec::with_capacity(usize::from(config.seats)),
            units: Vec::with_capacity(n),
            bodies: Vec::with_capacity(n),
            rows: Vec::with_capacity(n),
            shots: Vec::with_capacity(usize::from(config.max_shots)),
            defeated: Vec::with_capacity(n),
            ids: DetIdAllocator::new(config.entities),
            index: DetSpatial::new(query_grid(config)),
            contacts_grid: DetBroadphase::new(BroadphaseConfig::default()),
            scratch: Scratch {
                grid: Vec::with_capacity(n),
                linear: Vec::with_capacity(n),
            },
            oracle: None,
            dt: 0.0,
            tick: 0,
            counters: AiSoakCounters::default(),
        };
        result.reset(&result.default_run());
        Ok(result)
    }

    /// Current settings.
    pub fn config(&self) -> AiSoakConfig {
        self.config
    }
    /// Live unit count, constant for the run.
    pub fn entity_count(&self) -> usize {
        self.units.len()
    }
    /// Input seat count.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }
    /// Team count.
    pub fn team_count(&self) -> usize {
        usize::from(self.config.teams)
    }
    /// Live shots.
    pub fn live_shots(&self) -> usize {
        self.shots.len()
    }
    /// Workload counters since reset.
    pub fn counters(&self) -> AiSoakCounters {
        self.counters
    }

    /// Turn the linear-scan oracle on (counters restart) or off. The state does not change.
    pub fn set_oracle(&mut self, on: bool) {
        self.oracle = on.then(OracleReport::default);
    }

    /// The oracle's counters, or `None` when the oracle is off.
    pub fn oracle(&self) -> Option<&OracleReport> {
        self.oracle.as_ref()
    }

    /// Snapshot digest, outside the timed step.
    pub fn fingerprint(&self) -> u64 {
        let mut h = StateHasher::new();
        self.hash_state(&mut h);
        h.finish()
    }

    /// Step with an observer around each system (the tick profiler, or a tracing adapter).
    pub fn step_observed(&mut self, tick: u32, observer: &mut impl AiSoakObserver) {
        observer.begin_tick();
        self.advance(tick, observer);
        observer.end_tick(tick);
    }

    fn advance(&mut self, tick: u32, obs: &mut impl AiSoakObserver) {
        obs.begin(AiSystem::Input);
        for (i, seat) in self.seats.iter_mut().enumerate() {
            let angle = seat.phase + (tick as f32 * self.dt) * (0.25 + i as f32 * 0.003);
            seat.input = [math::cos(angle), math::sin(angle)];
        }
        obs.end(AiSystem::Input);

        obs.begin(AiSystem::Build);
        self.rows.clear();
        for (slot, (unit, body)) in self.units.iter().zip(&self.bodies).enumerate() {
            self.rows.push(SpatialRow {
                slot,
                body: *body,
                group_mask: 1_u64 << unit.team,
                flags: 0,
            });
        }
        // Slots are 0..n by construction, so build cannot report `spatial_slot`.
        let _ = self.index.build(&self.rows);
        obs.end(AiSystem::Build);

        obs.begin(AiSystem::Acquire);
        self.acquire(tick);
        obs.end(AiSystem::Acquire);
        obs.begin(AiSystem::Target);
        self.pick_targets(tick);
        obs.end(AiSystem::Target);
        obs.begin(AiSystem::Ally);
        self.find_allies(tick);
        obs.end(AiSystem::Ally);
        obs.begin(AiSystem::KeepOut);
        self.keep_out(tick);
        obs.end(AiSystem::KeepOut);

        obs.begin(AiSystem::Move);
        self.integrate();
        obs.end(AiSystem::Move);
        obs.begin(AiSystem::Shots);
        self.fire(tick);
        self.sweep(tick);
        obs.end(AiSystem::Shots);
        obs.begin(AiSystem::Recycle);
        self.recycle(tick);
        obs.end(AiSystem::Recycle);

        obs.begin(AiSystem::Contacts);
        let contacts = &mut self.counters.contacts;
        self.contacts_grid
            .for_each_contact(&mut self.bodies, |bodies, c| {
                *contacts += 1;
                separate(bodies, c);
            });
        obs.end(AiSystem::Contacts);
        self.tick = tick.wrapping_add(1);
    }

    fn enemy_mask(&self, team: u8) -> u64 {
        self.config.all_teams() & !(1_u64 << team)
    }

    fn acquire(&mut self, tick: u32) {
        let r = self.config.acquire_radius;
        let k = usize::from(self.config.acquire_k);
        let rule = self.index.config().floor_rule;
        for i in 0..self.units.len() {
            let mask = self.enemy_mask(self.units[i].team);
            let center = self.bodies[i];
            let bodies = &self.bodies;
            let accept = |s: usize| s != i && disc_within(&center, &bodies[s], r);
            self.index
                .first_k_within_by(center, r, mask, k, &mut self.scratch.grid, accept);
            if self.oracle.is_some() {
                linear_scan(
                    &self.rows,
                    rule,
                    &center,
                    mask,
                    &mut self.scratch.linear,
                    k,
                    accept,
                );
                self.check(tick, AiQuery::Acquire, i);
            }
            let unit = &mut self.units[i];
            unit.lead = self.scratch.grid.first().map_or(NONE, |&s| s as u32);
            self.counters.acquired += self.scratch.grid.len() as u64;
        }
    }

    fn pick_targets(&mut self, tick: u32) {
        let r = self.config.engage_radius;
        let rule = self.index.config().floor_rule;
        for i in 0..self.units.len() {
            let mask = self.enemy_mask(self.units[i].team);
            let center = self.bodies[i];
            let bodies = &self.bodies;
            let units = &self.units;
            let accept = |s: usize| s != i && units[s].health > 0 && disc_within(&center, &bodies[s], r);
            let grid = &mut self.scratch.grid;
            grid.clear();
            let _ = self.index.within(center, r, mask, |s| {
                if accept(s) {
                    grid.push(s);
                }
                ControlFlow::Continue(())
            });
            if self.oracle.is_some() {
                linear_scan(
                    &self.rows,
                    rule,
                    &center,
                    mask,
                    &mut self.scratch.linear,
                    usize::MAX,
                    accept,
                );
                self.check(tick, AiQuery::Target, i);
            }
            // Lowest score wins; a strict comparison keeps the lowest slot on ties.
            let mut best = NONE;
            let mut best_score = f32::INFINITY;
            for &s in &self.scratch.grid {
                let score = distance_sq(&center, &self.bodies[s]) + self.units[s].health as f32 * 0.5;
                if score < best_score {
                    best_score = score;
                    best = s as u32;
                }
            }
            self.units[i].target = best;
            self.counters.targets += u64::from(best != NONE);
        }
    }

    fn find_allies(&mut self, tick: u32) {
        let r = self.config.ally_radius;
        let rule = self.index.config().floor_rule;
        for i in 0..self.units.len() {
            let mask = 1_u64 << self.units[i].team;
            let center = self.bodies[i];
            let bodies = &self.bodies;
            let dist = |s: usize| {
                if s == i {
                    f32::NAN
                } else {
                    distance_sq(&center, &bodies[s])
                }
            };
            let found = self.index.nearest(center, mask, r, dist);
            if let Some(report) = self.oracle.as_mut() {
                let linear = linear_nearest(&self.rows, rule, &center, mask, r, dist);
                record(
                    report,
                    tick,
                    AiQuery::Ally,
                    i,
                    found.as_slice(),
                    linear.as_slice(),
                );
            }
            self.units[i].ally = found.map_or(NONE, |s| s as u32);
            self.counters.allies += u64::from(found.is_some());
        }
    }

    fn keep_out(&mut self, tick: u32) {
        let r = self.config.keep_out_radius;
        let rule = self.index.config().floor_rule;
        let all = self.config.all_teams();
        for i in 0..self.units.len() {
            let center = self.bodies[i];
            let bodies = &self.bodies;
            let accept = |s: usize| s != i && distance_sq(&center, &bodies[s]) < r * r;
            let grid = &mut self.scratch.grid;
            grid.clear();
            let _ = self.index.within(center, r, all, |s| {
                if accept(s) {
                    grid.push(s);
                }
                ControlFlow::Continue(())
            });
            if self.oracle.is_some() {
                linear_scan(
                    &self.rows,
                    rule,
                    &center,
                    all,
                    &mut self.scratch.linear,
                    usize::MAX,
                    accept,
                );
                self.check(tick, AiQuery::KeepOut, i);
            }
            let mut push = [0.0_f32; 2];
            for &s in &self.scratch.grid {
                let dx = center.pos[0] - self.bodies[s].pos[0];
                let dz = center.pos[2] - self.bodies[s].pos[2];
                let d = math::sqrt(dx * dx + dz * dz);
                if d > 0.0001 {
                    let w = (r - d) / (r * d);
                    push[0] += dx * w;
                    push[1] += dz * w;
                } else {
                    // Coincident centers: a fixed side by slot order, never a random one.
                    push[0] += if s > i { -1.0 } else { 1.0 };
                }
            }
            self.units[i].push = push;
            self.counters.keep_out += self.scratch.grid.len() as u64;
        }
    }

    fn integrate(&mut self) {
        let c = self.config;
        let edge = c.half_extent - c.radius;
        let gain = det_min(self.dt * 4.0, 1.0);
        for i in 0..self.units.len() {
            let pos = [self.bodies[i].pos[0], self.bodies[i].pos[2]];
            let unit = &self.units[i];
            let toward = |slot: u32, speed: f32| -> [f32; 2] {
                let b = &self.bodies[slot as usize];
                let dx = b.pos[0] - pos[0];
                let dz = b.pos[2] - pos[1];
                let d = math::sqrt(dx * dx + dz * dz);
                if d > 0.0001 {
                    [dx / d * speed, dz / d * speed]
                } else {
                    [0.0; 2]
                }
            };
            let mut desired = if unit.target != NONE {
                if distance_sq(&self.bodies[i], &self.bodies[unit.target as usize])
                    > c.engage_radius * c.engage_radius * 0.25
                {
                    toward(unit.target, c.speed)
                } else {
                    [0.0; 2]
                }
            } else if unit.lead != NONE {
                toward(unit.lead, c.speed)
            } else {
                let input = self.seats[i % self.seats.len()].input;
                // A restoring drive keeps idle units drifting back toward the fight.
                [
                    input[0] * c.speed - pos[0] * 0.05,
                    input[1] * c.speed - pos[1] * 0.05,
                ]
            };
            if unit.ally != NONE {
                let cohesion = toward(unit.ally, c.speed * 0.25);
                desired[0] += cohesion[0];
                desired[1] += cohesion[1];
            }
            desired[0] += unit.push[0] * c.speed;
            desired[1] += unit.push[1] * c.speed;
            let unit = &mut self.units[i];
            let body = &mut self.bodies[i];
            for (axis, component) in [0, 2].into_iter().enumerate() {
                unit.velocity[axis] += (desired[axis] - unit.velocity[axis]) * gain;
                body.pos[component] += unit.velocity[axis] * self.dt;
                if body.pos[component].abs() > edge {
                    body.pos[component] = det_clamp(body.pos[component], -edge, edge);
                    unit.velocity[axis] = -unit.velocity[axis];
                }
            }
            unit.cooldown = unit.cooldown.saturating_sub(1);
            // The slot exists (0..n), so update cannot report `spatial_slot`.
            let _ = self.index.update(i, [body.pos[0], body.pos[2]]);
        }
    }

    fn fire(&mut self, tick: u32) {
        let c = self.config;
        let n = self.units.len();
        let start = (tick as usize).wrapping_mul(37) % n;
        let mut fired = 0;
        for offset in 0..n {
            if fired == c.shots_per_tick || self.shots.len() >= usize::from(c.max_shots) {
                break;
            }
            let i = (start + offset) % n;
            let unit = &self.units[i];
            if unit.cooldown > 0 || unit.target == NONE {
                continue;
            }
            let from = &self.bodies[i];
            let to = &self.bodies[unit.target as usize];
            let dx = to.pos[0] - from.pos[0];
            let dz = to.pos[2] - from.pos[2];
            let d = math::sqrt(dx * dx + dz * dz);
            if d <= 0.0001 {
                continue;
            }
            let dir = [dx / d, dz / d];
            self.shots.push(Shot {
                pos: [from.pos[0], from.pos[2]],
                vel: [dir[0] * c.shot_speed, dir[1] * c.shot_speed],
                team: unit.team,
                life: c.shot_life_ticks,
            });
            self.units[i].cooldown = c.cooldown_ticks;
            self.counters.shots_fired += 1;
            fired += 1;
        }
    }

    fn sweep(&mut self, tick: u32) {
        let c = self.config;
        let rule = self.index.config().floor_rule;
        let mut at = 0;
        let mut subject = 0_usize;
        while at < self.shots.len() {
            let shot = self.shots[at];
            let mask = self.enemy_mask(shot.team);
            let to = [
                shot.pos[0] + shot.vel[0] * self.dt,
                shot.pos[1] + shot.vel[1] * self.dt,
            ];
            let from = BroadBody::new(
                [shot.pos[0], 0.0, shot.pos[1]],
                c.shot_radius,
                ground_band(c.radius),
            );
            let bodies = &self.bodies;
            let units = &self.units;
            let accept =
                |s: usize| units[s].health > 0 && capsule_hit(shot.pos, to, c.shot_radius, &bodies[s]);
            let hit = self
                .index
                .first_along_segment(from, to, c.shot_radius, mask, accept);
            if self.oracle.is_some() {
                linear_scan(&self.rows, rule, &from, mask, &mut self.scratch.linear, 1, accept);
                self.scratch.grid.clear();
                self.scratch.grid.extend(hit);
                self.check(tick, AiQuery::Segment, subject);
            }
            self.counters.sweeps += 1;
            if let Some(hit) = hit {
                self.counters.hits += 1;
                self.damage(hit, c.shot_damage);
                self.burst(tick, subject, hit, mask);
                self.shots.remove(at);
            } else {
                let live = &mut self.shots[at];
                live.pos = to;
                live.life -= 1;
                if live.life == 0 {
                    self.shots.remove(at);
                } else {
                    at += 1;
                }
            }
            subject += 1;
        }
    }

    fn burst(&mut self, tick: u32, subject: usize, hit: usize, mask: u64) {
        let c = self.config;
        if c.burst_radius <= 0.0 || c.burst_damage == 0 {
            return;
        }
        let rule = self.index.config().floor_rule;
        let center = self.bodies[hit];
        let r = c.burst_radius;
        let bodies = &self.bodies;
        let units = &self.units;
        let accept = |s: usize| s != hit && units[s].health > 0 && disc_within(&center, &bodies[s], r);
        let grid = &mut self.scratch.grid;
        grid.clear();
        let _ = self.index.within(center, r, mask, |s| {
            if accept(s) {
                grid.push(s);
            }
            ControlFlow::Continue(())
        });
        if self.oracle.is_some() {
            linear_scan(
                &self.rows,
                rule,
                &center,
                mask,
                &mut self.scratch.linear,
                usize::MAX,
                accept,
            );
            self.check(tick, AiQuery::Burst, subject);
        }
        let grid = std::mem::take(&mut self.scratch.grid);
        for &s in &grid {
            self.damage(s, c.burst_damage);
            self.counters.burst_hits += 1;
        }
        self.scratch.grid = grid;
    }

    fn damage(&mut self, slot: usize, amount: u32) {
        let unit = &mut self.units[slot];
        unit.health = unit.health.saturating_sub(amount);
    }

    fn recycle(&mut self, tick: u32) {
        self.defeated.clear();
        for (i, unit) in self.units.iter().enumerate() {
            if unit.health == 0 {
                self.defeated.push(i);
            }
        }
        for &i in &self.defeated {
            // A live scenario handle: freeing it cannot fail.
            let _ = self.ids.free_handle(self.units[i].handle);
        }
        let defeated = std::mem::take(&mut self.defeated);
        for &i in &defeated {
            // The batch just freed guarantees an ID, independent of input.
            let Some(handle) = self.ids.alloc_handle() else {
                continue;
            };
            let team = self.units[i].team;
            let mut rng = SplitMix64::new(self.seed ^ (u64::from(tick) << 20) ^ u64::from(handle.id));
            let pos = spawn_point(self.config, team, &mut rng);
            self.bodies[i].pos[0] = pos[0];
            self.bodies[i].pos[2] = pos[1];
            self.units[i] = Unit {
                handle,
                team,
                velocity: [0.0; 2],
                health: self.config.health,
                cooldown: self.config.cooldown_ticks,
                target: NONE,
                lead: NONE,
                ally: NONE,
                push: [0.0; 2],
            };
            self.counters.recycled += 1;
        }
        self.defeated = defeated;
    }

    fn check(&mut self, tick: u32, query: AiQuery, subject: usize) {
        if let Some(report) = self.oracle.as_mut() {
            record(
                report,
                tick,
                query,
                subject,
                &self.scratch.grid,
                &self.scratch.linear,
            );
        }
    }
}

fn record(
    report: &mut OracleReport,
    tick: u32,
    query: AiQuery,
    subject: usize,
    grid: &[usize],
    linear: &[usize],
) {
    report.checked[query as usize] += 1;
    if grid != linear {
        report.mismatches += 1;
        if report.first.is_none() {
            report.first = Some(OracleMismatch {
                tick,
                query,
                subject: subject as u32,
                grid: grid.iter().map(|&s| s as u32).collect(),
                linear: linear.iter().map(|&s| s as u32).collect(),
            });
        }
    }
}

/// Every row in slot order that passes the mask, the floor rule and `accept`, up to `k`.
/// Independent of the index: no cells, no candidate envelope. The floor rule reads the
/// rows' bands, which `update` never changes; `accept` reads the current positions.
fn linear_scan(
    rows: &[SpatialRow],
    rule: FloorRule,
    center: &BroadBody,
    mask: u64,
    out: &mut Vec<usize>,
    k: usize,
    accept: impl Fn(usize) -> bool,
) {
    out.clear();
    for (slot, row) in rows.iter().enumerate() {
        if out.len() == k {
            break;
        }
        if row.group_mask & mask != 0 && !bodies_on_different_floors(rule, center, &row.body) && accept(slot)
        {
            out.push(slot);
        }
    }
}

fn linear_nearest(
    rows: &[SpatialRow],
    rule: FloorRule,
    center: &BroadBody,
    mask: u64,
    max_r: f32,
    dist: impl Fn(usize) -> f32,
) -> Option<usize> {
    let mut best = None;
    let mut best_d = max_r * max_r;
    for (slot, row) in rows.iter().enumerate() {
        if row.group_mask & mask == 0 || bodies_on_different_floors(rule, center, &row.body) {
            continue;
        }
        let d = dist(slot);
        if d <= best_d && (best.is_none() || d < best_d) {
            best = Some(slot);
            best_d = d;
        }
    }
    best
}

fn query_grid(config: AiSoakConfig) -> BroadphaseConfig {
    BroadphaseConfig {
        cell_size: config.query_cell,
        ..BroadphaseConfig::default()
    }
}

fn ground_band(radius: f32) -> [f32; 2] {
    [-radius, radius]
}

fn distance_sq(a: &BroadBody, b: &BroadBody) -> f32 {
    let dx = b.pos[0] - a.pos[0];
    let dz = b.pos[2] - a.pos[2];
    dx * dx + dz * dz
}

/// Center of `b` within `r` of the center of `a` (inclusive).
fn disc_within(a: &BroadBody, b: &BroadBody, r: f32) -> bool {
    distance_sq(a, b) <= r * r
}

/// The closest-point capsule test the spatial index's envelope covers: strict.
fn capsule_hit(a: [f32; 2], b: [f32; 2], r: f32, body: &BroadBody) -> bool {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [body.pos[0] - a[0], body.pos[2] - a[1]];
    let len_sq = ab[0] * ab[0] + ab[1] * ab[1];
    let t = (ap[0] * ab[0] + ap[1] * ab[1]) / len_sq;
    let t = if t.is_nan() { 0.0 } else { det_clamp(t, 0.0, 1.0) };
    let dx = ap[0] - ab[0] * t;
    let dz = ap[1] - ab[1] * t;
    let reach = r + body.radius;
    dx * dx + dz * dz < reach * reach
}

fn spawn_point(config: AiSoakConfig, team: u8, rng: &mut SplitMix64) -> [f32; 2] {
    let sector = std::f32::consts::TAU / f32::from(config.teams);
    let angle = f32::from(team) * sector + rng.signed() * sector * 0.35;
    let distance = config.half_extent * (0.45 + rng.unit() * 0.4);
    [math::cos(angle) * distance, math::sin(angle) * distance]
}

fn separate(bodies: &mut [BroadBody], c: Contact) {
    let distance = math::sqrt(c.dist_sq);
    let (nx, nz) = if distance > 0.0001 {
        (c.dx / distance, c.dz / distance)
    } else {
        (1.0, 0.0)
    };
    let half = (c.min_dist - distance) * 0.5;
    bodies[c.a].pos[0] -= nx * half;
    bodies[c.a].pos[2] -= nz * half;
    bodies[c.b].pos[0] += nx * half;
    bodies[c.b].pos[2] += nz * half;
}

impl DetScenario for AiSoakScenario {
    fn name(&self) -> &str {
        "ai_soak"
    }
    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_a100_0900, 600, TickRate::default())
    }
    fn reset(&mut self, run: &DetRun) {
        let c = self.config;
        self.seed = run.seed;
        self.dt = run.rate.dt();
        self.tick = 0;
        self.counters = AiSoakCounters::default();
        self.ids = DetIdAllocator::new(c.entities);
        self.index = DetSpatial::new(query_grid(c));
        self.contacts_grid = DetBroadphase::new(BroadphaseConfig::default());
        if let Some(report) = self.oracle.as_mut() {
            *report = OracleReport::default();
        }
        self.seats.clear();
        self.units.clear();
        self.bodies.clear();
        self.shots.clear();
        let mut rng = SplitMix64::new(run.seed);
        for _ in 0..c.seats {
            self.seats.push(Seat {
                phase: rng.unit() * std::f32::consts::TAU,
                input: [0.0; 2],
            });
        }
        for i in 0..usize::from(c.entities) {
            let team = (i % usize::from(c.teams)) as u8;
            let pos = spawn_point(c, team, &mut rng);
            self.bodies.push(BroadBody::new(
                [pos[0], 0.0, pos[1]],
                c.radius,
                ground_band(c.radius),
            ));
            // The validated cap equals `entities`, so every bootstrap allocation succeeds.
            let handle = self.ids.alloc_handle().unwrap_or(DetHandle {
                id: (i + 1) as u16,
                generation: 0,
            });
            self.units.push(Unit {
                handle,
                team,
                velocity: [0.0; 2],
                health: c.health,
                cooldown: (i as u32) % c.cooldown_ticks,
                target: NONE,
                lead: NONE,
                ally: NONE,
                push: [0.0; 2],
            });
        }
    }
    fn step(&mut self, tick: u32) {
        let _span = tracing::trace_span!("detsim_ai_soak_step", tick).entered();
        self.advance(tick, &mut ());
    }
    fn hash_state(&self, h: &mut StateHasher) {
        let c = self.config;
        h.u32("tick", self.tick);
        h.u64("seed", self.seed);
        h.f32("dt", self.dt);
        h.u32("teams", u32::from(c.teams));
        h.u32("seats", u32::from(c.seats));
        h.u32("entities", u32::from(c.entities));
        for (name, v) in [
            ("half_extent", c.half_extent),
            ("radius", c.radius),
            ("speed", c.speed),
            ("acquire_radius", c.acquire_radius),
            ("engage_radius", c.engage_radius),
            ("ally_radius", c.ally_radius),
            ("keep_out_radius", c.keep_out_radius),
            ("query_cell", c.query_cell),
            ("shot_speed", c.shot_speed),
            ("shot_radius", c.shot_radius),
            ("burst_radius", c.burst_radius),
        ] {
            h.f32(name, v);
        }
        for (name, v) in [
            ("acquire_k", u32::from(c.acquire_k)),
            ("shots_per_tick", u32::from(c.shots_per_tick)),
            ("max_shots", u32::from(c.max_shots)),
            ("shot_life_ticks", c.shot_life_ticks),
            ("health", c.health),
            ("shot_damage", c.shot_damage),
            ("burst_damage", c.burst_damage),
            ("cooldown_ticks", c.cooldown_ticks),
        ] {
            h.u32(name, v);
        }
        let k = self.counters;
        for (name, v) in [
            ("acquired", k.acquired),
            ("targets", k.targets),
            ("allies", k.allies),
            ("keep_out", k.keep_out),
            ("shots_fired", k.shots_fired),
            ("sweeps", k.sweeps),
            ("hits", k.hits),
            ("burst_hits", k.burst_hits),
            ("recycled", k.recycled),
            ("contacts", k.contacts),
        ] {
            h.u64(name, v);
        }
        for (i, seat) in self.seats.iter().enumerate() {
            h.item("seat", i as u32);
            h.f32("phase", seat.phase);
            h.f32("input_x", seat.input[0]);
            h.f32("input_z", seat.input[1]);
        }
        for (i, (unit, body)) in self.units.iter().zip(&self.bodies).enumerate() {
            h.item("unit", i as u32);
            h.u32("id", u32::from(unit.handle.id));
            h.u32("generation", unit.handle.generation);
            h.u32("team", u32::from(unit.team));
            h.f32("x", body.pos[0]);
            h.f32("z", body.pos[2]);
            h.f32("velocity_x", unit.velocity[0]);
            h.f32("velocity_z", unit.velocity[1]);
            h.u32("health", unit.health);
            h.u32("cooldown", unit.cooldown);
            h.u32("target", unit.target);
            h.u32("lead", unit.lead);
            h.u32("ally", unit.ally);
            h.f32("push_x", unit.push[0]);
            h.f32("push_z", unit.push[1]);
        }
        h.u64("shots", self.shots.len() as u64);
        for (i, shot) in self.shots.iter().enumerate() {
            h.item("shot", i as u32);
            h.f32("x", shot.pos[0]);
            h.f32("z", shot.pos[1]);
            h.f32("vx", shot.vel[0]);
            h.f32("vz", shot.vel[1]);
            h.u32("team", u32::from(shot.team));
            h.u32("life", shot.life);
        }
    }
    fn flip_bit(&mut self, bit: u32) -> bool {
        if bit >= 32 {
            return false;
        }
        self.tick ^= 1_u32 << bit;
        true
    }
}

/// Invalid measurement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiMeasureError {
    /// Zero measured ticks, warmup plus ticks above `u32::MAX`, or more measured ticks than
    /// the profiler ring holds ([`MAX_RING_TICKS`]).
    Range,
    /// The profiler could not reserve its ring.
    Profiler(ProfileError),
}

impl AiMeasureError {
    /// Stable lowercase protocol code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Range => "invalid_ai_soak_measurement",
            Self::Profiler(e) => e.code(),
        }
    }
}

impl std::fmt::Display for AiMeasureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Range => f.write_str(
                "invalid_ai_soak_measurement: require 0 < ticks <= 1048576 and warmup + ticks <= u32::MAX",
            ),
            Self::Profiler(e) => write!(f, "{}: the tick profiler could not reserve its ring", e.code()),
        }
    }
}

impl std::error::Error for AiMeasureError {}

/// Drives a reset scenario through warmup and measured ticks, with the tick profiler around
/// every measured tick. Use it to sample memory between ticks (the example's long mode).
pub struct AiSoakRunner<C: ProfileClock> {
    scenario: AiSoakScenario,
    profiler: TickProfiler<10, C>,
    run: DetRun,
    warmup: u32,
    next: u32,
    hashes: Vec<u64>,
    pass_line_ns: u64,
}

impl AiSoakRunner<HostClock> {
    /// A runner on the host thread CPU clock (wall time when the platform lacks one).
    /// Probe on the thread that steps.
    pub fn new(
        scenario: AiSoakScenario,
        run: DetRun,
        warmup: u32,
        pass_line_ns: u64,
    ) -> Result<Self, AiMeasureError> {
        Self::with_clock(scenario, run, warmup, pass_line_ns, HostClock::probe())
    }
}

impl<C: ProfileClock> AiSoakRunner<C> {
    /// A runner on `clock` (a `ManualClock` in tests). `run.ticks` counts measured ticks;
    /// the scenario is reset to `warmup + run.ticks` ticks. `pass_line_ns` is the whole-tick
    /// budget and every system's budget.
    pub fn with_clock(
        mut scenario: AiSoakScenario,
        run: DetRun,
        warmup: u32,
        pass_line_ns: u64,
        clock: C,
    ) -> Result<Self, AiMeasureError> {
        if run.ticks == 0 || run.ticks as usize > MAX_RING_TICKS {
            return Err(AiMeasureError::Range);
        }
        let total = run.ticks.checked_add(warmup).ok_or(AiMeasureError::Range)?;
        let settings = ProfileSettings {
            ring_ticks: run.ticks as usize,
            tick_budget_ns: pass_line_ns,
            system_budget_ns: pass_line_ns,
        };
        let profiler =
            TickProfiler::new(AI_SOAK_SYSTEMS, clock, settings).map_err(AiMeasureError::Profiler)?;
        let run = DetRun { ticks: total, ..run };
        scenario.reset(&run);
        let mut hashes = Vec::new();
        hashes
            .try_reserve_exact(total as usize)
            .map_err(|_| AiMeasureError::Profiler(ProfileError::AllocationFailed))?;
        Ok(Self {
            scenario,
            profiler,
            run,
            warmup,
            next: 0,
            hashes,
            pass_line_ns,
        })
    }

    /// Ticks still to step.
    pub fn remaining(&self) -> u32 {
        self.run.ticks - self.next
    }

    /// The next tick to step.
    pub fn next_tick(&self) -> u32 {
        self.next
    }

    /// The scenario (read-only).
    pub fn scenario(&self) -> &AiSoakScenario {
        &self.scenario
    }

    /// The profiler, so a test can move a `ManualClock`.
    pub fn profiler_mut(&mut self) -> &mut TickProfiler<10, C> {
        &mut self.profiler
    }

    /// Step one tick (warmup ticks are untimed). Returns false when the run is complete.
    /// Hashing is outside the timed region.
    pub fn step(&mut self) -> bool {
        if self.next >= self.run.ticks {
            return false;
        }
        let tick = self.next;
        if tick < self.warmup {
            self.scenario.step(tick);
        } else {
            self.scenario.step_observed(tick, &mut self.profiler);
        }
        self.hashes.push(self.scenario.fingerprint());
        self.next += 1;
        true
    }

    /// Step to the end and build the measurement.
    pub fn finish(mut self) -> AiSoakMeasurement {
        while self.step() {}
        let report = self.profiler.report();
        AiSoakMeasurement {
            config: self.scenario.config(),
            counters: self.scenario.counters(),
            oracle: self.scenario.oracle().cloned(),
            warmup: self.warmup,
            pass_line_ns: self.pass_line_ns,
            trace: DetTrace {
                scenario: self.scenario.name().into(),
                run: self.run,
                tick_hashes: self.hashes,
                planted: self.scenario.planted(),
            },
            report,
        }
    }
}

/// A finished AI soak: the deterministic trace and the tick profiler's report.
#[derive(Debug, Clone)]
pub struct AiSoakMeasurement {
    /// Settings used.
    pub config: AiSoakConfig,
    /// Workload counters, including warmup.
    pub counters: AiSoakCounters,
    /// Oracle counters when the oracle was on.
    pub oracle: Option<OracleReport>,
    /// Untimed warmup ticks.
    pub warmup: u32,
    /// Whole-tick pass line, nanoseconds.
    pub pass_line_ns: u64,
    /// The trace of every tick, warmup included.
    pub trace: DetTrace,
    /// One row per system on [`AI_SOAK_SYSTEMS`], whole-tick quantiles and over-line ticks.
    /// The same type `sim.tick_profile` returns: insert it as the host's `DetTickProfile`.
    pub report: ProfileReport,
}

impl AiSoakMeasurement {
    /// Whether the whole-tick p99 is at or below the pass line.
    pub fn inside_pass_line(&self) -> bool {
        self.report.tick.p99_ns <= self.pass_line_ns
    }

    /// The clock that produced the report.
    pub fn clock(&self) -> ClockKind {
        self.report.clock
    }
}

/// Reset `scenario` and measure `run.ticks` ticks after `warmup` untimed ticks.
pub fn measure(
    scenario: AiSoakScenario,
    run: DetRun,
    warmup: u32,
    pass_line_ns: u64,
) -> Result<AiSoakMeasurement, AiMeasureError> {
    let _span = tracing::info_span!("detsim_ai_soak", ticks = run.ticks, warmup).entered();
    Ok(AiSoakRunner::new(scenario, run, warmup, pass_line_ns)?.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_records_the_first_differing_sequence() {
        let mut report = OracleReport::default();
        record(&mut report, 3, AiQuery::Target, 7, &[1, 2], &[1, 2]);
        assert!(report.passed());
        record(&mut report, 4, AiQuery::Burst, 9, &[2, 1], &[1, 2]);
        record(&mut report, 5, AiQuery::Ally, 1, &[], &[0]);
        assert_eq!(report.mismatches, 2);
        assert_eq!(report.checked[AiQuery::Target as usize], 1);
        let first = report.first.expect("a mismatch");
        assert_eq!((first.tick, first.query, first.subject), (4, AiQuery::Burst, 9));
        assert_eq!((first.grid, first.linear), (vec![2, 1], vec![1, 2]));
    }

    #[test]
    fn linear_scan_applies_mask_floor_and_k() {
        let body = |x: f32, y: f32| BroadBody::new([x, y, 0.0], 0.5, [y - 0.5, y + 0.5]);
        let rows: Vec<SpatialRow> = [(0.0, 0.0, 1), (1.0, 0.0, 2), (2.0, 9.0, 1), (3.0, 0.0, 1)]
            .into_iter()
            .enumerate()
            .map(|(slot, (x, y, m))| SpatialRow {
                slot,
                body: body(x, y),
                group_mask: m,
                flags: 0,
            })
            .collect();
        let mut out = Vec::new();
        linear_scan(
            &rows,
            FloorRule::Bands,
            &body(0.0, 0.0),
            1,
            &mut out,
            usize::MAX,
            |_| true,
        );
        assert_eq!(out, [0, 3], "mask drops slot 1, the floor rule drops slot 2");
        linear_scan(&rows, FloorRule::Never, &body(0.0, 0.0), 1, &mut out, 2, |s| {
            s != 0
        });
        assert_eq!(out, [2, 3]);
        let nearest = linear_nearest(&rows, FloorRule::Never, &body(1.5, 0.0), u64::MAX, 10.0, |s| {
            let d = rows[s].body.pos[0] - 1.5;
            d * d
        });
        assert_eq!(nearest, Some(1), "equal distances keep the lowest slot");
    }

    #[test]
    fn capsule_test_maps_a_zero_length_step_to_its_point() {
        let target = BroadBody::new([1.0, 0.0, 0.0], 0.5, [-0.5, 0.5]);
        assert!(capsule_hit([0.0, 0.0], [2.0, 0.0], 0.1, &target));
        assert!(capsule_hit([0.7, 0.0], [0.7, 0.0], 0.0, &target));
        assert!(!capsule_hit([0.0, 0.0], [0.0, 0.0], 0.1, &target));
        assert!(!capsule_hit([0.0, 2.0], [2.0, 2.0], 0.1, &target));
    }
}
