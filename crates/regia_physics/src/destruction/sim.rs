// SPDX-License-Identifier: MPL-2.0
//! The deterministic break model: damage → bond damage → breaks → support graph → detached chunks.
//!
//! Everything here is decided in fixed-step order from the queued hits, with `f32` additions,
//! multiplications, divisions and square roots only (IEEE 754 rounds them the same on every
//! platform; Rust never fuses them), on plain arrays instead of SIMD vector types. So a server can
//! decide the breaks and a client replaying the server's [`BreakEvent`]s reaches the same
//! [`DestructionWorld::structure_hash`]. Chunk motion after a break is rapier's (`bridge.rs`)
//! and is cosmetic: it never feeds back into a decision.
//!
//! Model (ADR-1597). A hit gives each chunk it reaches an impulse (newton-seconds) scaled by the
//! material's `damage_scale`; impulses below `min_impulse` are dropped. A chunk's impulse loads
//! its intact bonds equally (a chunk with fewer bonds left breaks loose sooner). A bond breaks when
//! its accumulated load reaches `break_impulse`; loads heal by `decay_per_second`. After the
//! breaks, chunks that no path of intact bonds joins to a support chunk detach (the structure
//! collapses); an object without support chunks keeps its largest piece.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::asset::{DestructibleAsset, DestructibleError};
use super::load::{BondLoad, LoadReport, LoadSolution, LoadStats, solve_load};

/// Most events one `Destruction::apply_events` call accepts.
pub const MAX_EVENTS_PER_CALL: usize = 65_536;

/// Id of one destructible instance in a [`DestructionWorld`] (allocation order, never reused).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DestructibleId(pub u32);

/// Who decides the breaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Authority {
    /// This world evaluates damage and decides breaks (single player, or the server).
    #[default]
    Server,
    /// This world only applies [`BreakEvent`]s from a server ([`DestructionWorld::apply_events`]).
    Replica,
}

/// A world pose: translation and unit rotation quaternion `[x, y, z, w]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructiblePose {
    /// Translation, metres.
    pub translation: [f32; 3],
    /// Rotation quaternion `[x, y, z, w]` (normalized on define).
    pub rotation: [f32; 4],
}

impl Default for DestructiblePose {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

impl DestructiblePose {
    /// A pose with no rotation.
    #[must_use]
    pub fn at(translation: [f32; 3]) -> Self {
        Self {
            translation,
            ..Self::default()
        }
    }

    /// Asset space → world space.
    #[must_use]
    pub fn transform_point(&self, p: [f32; 3]) -> [f32; 3] {
        add(rotate(self.rotation, p), self.translation)
    }

    /// World space → asset space.
    #[must_use]
    pub fn inverse_transform_point(&self, p: [f32; 3]) -> [f32; 3] {
        rotate(conjugate(self.rotation), sub(p, self.translation))
    }
}

/// Caps that bound the cost of destruction per tick and in total.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructionBudget {
    /// Debris bodies alive at once; spawning past the cap recycles the oldest. 0: detached chunks
    /// get no body (they vanish; the cheapest setting).
    pub max_active_chunks: u32,
    /// Bonds broken per tick across all instances (0: unlimited); the rest break on later ticks.
    pub max_breaks_per_tick: u32,
    /// Debris bodies created per tick (0: unlimited); the rest wait in order.
    pub max_spawns_per_tick: u32,
    /// A debris body asleep this long is despawned (0: never).
    pub sleep_despawn_secs: f32,
    /// A debris body older than this is despawned (0: never).
    pub max_lifetime_secs: f32,
    /// A debris body below this height is despawned.
    pub kill_y: f32,
}

impl Default for DestructionBudget {
    fn default() -> Self {
        Self {
            max_active_chunks: 256,
            max_breaks_per_tick: 64,
            max_spawns_per_tick: 32,
            sleep_despawn_secs: 4.0,
            max_lifetime_secs: 30.0,
            kill_y: -100.0,
        }
    }
}

/// What a hit is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DamageKind {
    /// A blast: every attached chunk whose centre lies within `radius` of `center` gets
    /// `impulse * (1 - distance / radius)`, pushed away from the centre.
    Explosion {
        /// World-space centre.
        center: [f32; 3],
        /// Radius, metres (> 0).
        radius: f32,
        /// Impulse at the centre, newton-seconds (>= 0).
        impulse: f32,
    },
    /// A shot: the ray hits the first attached chunk's bounds within `max_distance`; chunks within
    /// `radius` of the hit point get `impulse * (1 - distance / radius)` along the ray (the hit
    /// chunk gets at least the full impulse's share at its own distance; a `radius` of 0 hits the
    /// one chunk only).
    Projectile {
        /// World-space ray origin.
        origin: [f32; 3],
        /// Ray direction (normalized on use; not zero).
        direction: [f32; 3],
        /// Impulse, newton-seconds (>= 0).
        impulse: f32,
        /// Spread radius around the hit point, metres (>= 0).
        radius: f32,
        /// Longest hit distance, metres (> 0).
        max_distance: f32,
    },
    /// An impulse on one chunk (a vehicle, a melee hit, a contact the game measured).
    Impulse {
        /// Chunk index.
        chunk: u32,
        /// World-space impulse vector, newton-seconds.
        impulse: [f32; 3],
    },
}

/// One hit, queued with [`DestructionWorld::damage`] and applied on the next step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Damage {
    /// The instance hit, or `None` for every instance (an explosion's blast, the nearest
    /// instance a shot hits). `Impulse` needs a target.
    pub target: Option<DestructibleId>,
    /// The hit.
    pub kind: DamageKind,
}

/// Why a chunk detached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetachCause {
    /// A hit this tick reached the chunk and its last bond to the structure broke.
    Damage,
    /// The chunk lost every path to a support chunk.
    Collapse,
}

/// What a [`BreakEvent`] records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BreakKind {
    /// A bond broke.
    BondBroken {
        /// Bond index in the asset.
        bond: u32,
        /// Its lower chunk.
        a: u32,
        /// Its higher chunk.
        b: u32,
        /// World-space anchor of the bond.
        position: [f32; 3],
    },
    /// A chunk left the structure and becomes debris.
    ChunkDetached {
        /// Chunk index in the asset.
        chunk: u32,
        /// Why.
        cause: DetachCause,
        /// World-space centre of mass when it detached.
        position: [f32; 3],
        /// Initial linear velocity, m/s.
        linvel: [f32; 3],
        /// Initial angular velocity, rad/s (from the seed).
        angvel: [f32; 3],
    },
}

/// One decided break: the unit a server sends and a client replays (network sync hooks; no
/// network code here). `seq` orders every event of a world; `seed` is derived from the instance's
/// seed, the tick and the chunk or bond, for client-side variation that must match.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BreakEvent {
    /// World-wide event sequence number (0, 1, 2, ...).
    pub seq: u64,
    /// Tick the break was decided on.
    pub tick: u64,
    /// The instance.
    pub destructible: DestructibleId,
    /// What broke.
    pub kind: BreakKind,
    /// Deterministic per-event seed.
    pub seed: u64,
}

/// Sound and VFX hook: what to play where.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructionFx {
    /// The instance.
    pub destructible: DestructibleId,
    /// The kind of moment.
    pub kind: FxKind,
    /// World-space position.
    pub position: [f32; 3],
    /// Strength: the bond's break impulse, or the detached chunk's mass in kilograms.
    pub magnitude: f32,
    /// The material's `fx_tag`.
    pub fx_tag: String,
    /// The event's seed (pick a variation with it).
    pub seed: u64,
}

/// Kinds of [`DestructionFx`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FxKind {
    /// A bond broke (crack sound, dust puff).
    BondBreak,
    /// A chunk was knocked loose by a hit.
    ChunkDetach,
    /// A chunk fell because its support went (rumble, falling debris).
    Collapse,
}

/// A detached chunk waiting for its debris body (the bridge takes these in order).
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnRequest {
    /// The instance.
    pub destructible: DestructibleId,
    /// Chunk index.
    pub chunk: u32,
    /// World-space centre of mass.
    pub position: [f32; 3],
    /// World rotation of the chunk (the instance's).
    pub rotation: [f32; 4],
    /// Initial linear velocity.
    pub linvel: [f32; 3],
    /// Initial angular velocity.
    pub angvel: [f32; 3],
    /// The tick it detached on.
    pub tick: u64,
    /// The falling section it belongs to: the lowest chunk index of the section, or `chunk` when it
    /// falls alone (always, without `FallingSettings`). Requests of one section on one tick come
    /// out of [`DestructionWorld::take_spawn_groups`] together (ADR-2734).
    pub section: u32,
}

/// What one [`DestructionWorld::step`] (or [`DestructionWorld::apply_events`]) decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepReport {
    /// The tick that ran.
    pub tick: u64,
    /// Breaks decided this tick, in `seq` order.
    pub events: Vec<BreakEvent>,
    /// Sound/VFX hooks.
    pub fx: Vec<DestructionFx>,
    /// Hits applied.
    pub hits: u32,
    /// Hits that reached no chunk (a shot that missed, a blast out of range).
    pub misses: u32,
    /// Bonds over their threshold that the per-tick cap held back.
    pub deferred_breaks: u32,
}

/// State of one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkState {
    /// Part of the structure.
    Attached,
    /// Detached (debris, or gone).
    Detached,
}

/// Errors of the destruction API. [`DestructionError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DestructionError {
    /// The asset failed to load or validate.
    #[error(transparent)]
    Asset(#[from] DestructibleError),
    /// No instance has this id.
    #[error("no destructible with id {0}")]
    UnknownId(u32),
    /// The chunk index is out of range.
    #[error("destructible {id} has no chunk {chunk}")]
    UnknownChunk {
        /// Instance id.
        id: u32,
        /// Chunk asked for.
        chunk: u32,
    },
    /// A damage parameter is out of range.
    #[error("invalid damage: {0}")]
    InvalidDamage(String),
    /// Damage was queued on a replica (only the server decides breaks).
    #[error("this world is a replica: only the server decides breaks")]
    Replica,
    /// Events were applied to a server world.
    #[error("this world decides its own breaks; apply_events needs a replica")]
    NotReplica,
    /// An event names an instance, bond or chunk this world does not have, or comes out of order.
    #[error("break event {seq} does not apply: {reason}")]
    BadEvent {
        /// Its sequence number.
        seq: u64,
        /// Why.
        reason: String,
    },
    /// The pose is not finite or the rotation is zero.
    #[error("invalid pose: {0}")]
    InvalidPose(String),
    /// A snapshot names a chunk, bond or asset this world cannot rebuild.
    #[error("destruction snapshot does not apply: {0}")]
    BadSnapshot(String),
}

impl DestructionError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Asset(e) => e.code(),
            Self::UnknownId(_) => "destruction_unknown_id",
            Self::UnknownChunk { .. } => "destruction_unknown_chunk",
            Self::InvalidDamage(_) => "destruction_invalid_damage",
            Self::Replica => "destruction_replica",
            Self::NotReplica => "destruction_not_replica",
            Self::BadEvent { .. } => "destruction_bad_event",
            Self::InvalidPose(_) => "destruction_invalid_pose",
            Self::BadSnapshot(_) => "destruction_bad_snapshot",
        }
    }
}

/// A [`BreakEvent`] without what a replica can work out itself from the asset, the pose and the
/// instance seed (ADR-1967). [`DestructionWorld::compact_event`] and
/// [`DestructionWorld::expand_event`] go between the two.
#[derive(Clone, Debug, PartialEq)]
pub enum CompactBreak {
    /// A bond broke.
    Bond {
        /// Bond index in the asset.
        bond: u32,
    },
    /// A chunk detached.
    Detach {
        /// Chunk index in the asset.
        chunk: u32,
        /// Why.
        cause: DetachCause,
        /// Initial linear velocity, m/s (it comes from the hit, which a replica never sees).
        linvel: [f32; 3],
    },
}

/// One instance in a [`DestructionSnapshot`].
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceSnapshot {
    /// The instance.
    pub id: DestructibleId,
    /// Its world pose.
    pub pose: DestructiblePose,
    /// Its seed.
    pub seed: u64,
    /// Detached chunks, ascending.
    pub detached: Vec<u32>,
    /// Broken bonds between chunks that are both still attached, ascending (a detached chunk's
    /// bonds are broken with it).
    pub broken: Vec<u32>,
}

/// Everything a replica needs to catch up without the event history: a late joiner, or one that
/// lost step. [`DestructionWorld::restore`] on it gives the server's
/// [`DestructionWorld::structure_hash`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DestructionSnapshot {
    /// The next instance id.
    pub next_id: u32,
    /// The next event's `seq`.
    pub next_seq: u64,
    /// The digest of every event so far.
    pub event_digest: u64,
    /// The server's tick.
    pub tick: u64,
    /// Every instance, in id order.
    pub instances: Vec<InstanceSnapshot>,
}

/// One live destructible.
#[derive(Clone, Debug)]
pub struct DestructibleInstance {
    asset: Arc<DestructibleAsset>,
    pose: DestructiblePose,
    seed: u64,
    chunks: Vec<ChunkState>,
    bond_damage: Vec<f32>,
    bond_broken: Vec<bool>,
    /// Bond indices per chunk, ascending.
    chunk_bonds: Vec<Vec<u32>>,
    /// World-space centres at the current pose.
    world_centroids: Vec<[f32; 3]>,
    /// Impulse vector each chunk received this tick.
    tick_impulse: Vec<[f32; 3]>,
    /// The chunk was hit this tick.
    tick_hit: Vec<bool>,
    /// Some bond may be at or over its threshold (set by a hit, cleared once the breaks are
    /// done): idle instances skip the bond scan.
    hot: bool,
    /// Structural load state, for assets with `LoadSettings` (ADR-2734).
    load: Option<LoadState>,
    /// The falling section of each detached chunk (`u32::MAX`: not decided yet); empty without
    /// `FallingSettings`.
    section_of: Vec<u32>,
    /// The bonds that held each falling section together when it left the structure, by section.
    section_bonds: BTreeMap<u32, Vec<u32>>,
}

/// The server-side structural load of one instance (ADR-2734).
#[derive(Clone, Debug, Default)]
struct LoadState {
    /// The last solve.
    solution: LoadSolution,
    /// Bonds at or over their threshold, queued to break: they carry no load from the moment
    /// they are decided, whether or not the per-tick cap let them break yet.
    doomed: Vec<bool>,
    /// Bonds decided to break, oldest first, with the tick they were decided on. Breaks leave in
    /// this order, so a capped cascade breaks the same bonds in the same order as an uncapped one.
    pending: VecDeque<(u64, u32)>,
    /// The load-carrying bonds changed since the last solve.
    dirty: bool,
    /// Some bond was over its limit after the last accumulation.
    overloaded: bool,
    /// Solves run.
    solves: u64,
}

impl DestructibleInstance {
    /// The asset.
    #[must_use]
    pub fn asset(&self) -> &Arc<DestructibleAsset> {
        &self.asset
    }

    /// The world pose.
    #[must_use]
    pub fn pose(&self) -> DestructiblePose {
        self.pose
    }

    /// The instance seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Chunk states, in asset order.
    #[must_use]
    pub fn chunk_states(&self) -> &[ChunkState] {
        &self.chunks
    }

    /// Accumulated damage per bond, newton-seconds.
    #[must_use]
    pub fn bond_damage(&self) -> &[f32] {
        &self.bond_damage
    }

    /// Broken flag per bond.
    #[must_use]
    pub fn bond_broken(&self) -> &[bool] {
        &self.bond_broken
    }

    /// Chunks still attached.
    #[must_use]
    pub fn attached_count(&self) -> usize {
        self.chunks.iter().filter(|s| **s == ChunkState::Attached).count()
    }

    /// Bonds broken.
    #[must_use]
    pub fn broken_count(&self) -> usize {
        self.bond_broken.iter().filter(|b| **b).count()
    }

    /// World-space centre of chunk `chunk` in the intact structure.
    #[must_use]
    pub fn world_centroid(&self, chunk: u32) -> Option<[f32; 3]> {
        self.world_centroids.get(chunk as usize).copied()
    }

    /// The signed load (newtons, positive from `a` toward `b`) through each bond from the last
    /// structural-load solve; `None` when the asset has no `LoadSettings` (ADR-2734), and also
    /// `None` until the first solve has run.
    #[must_use]
    pub fn bond_loads(&self) -> Option<&[f32]> {
        self.load.as_ref().map(|l| l.solution.flow.as_slice())
    }

    /// The last structural-load solve; `None` without `LoadSettings`.
    #[must_use]
    pub fn load_solution(&self) -> Option<&LoadSolution> {
        self.load.as_ref().map(|l| &l.solution)
    }

    /// The bonds that held falling section `section` (its lowest chunk index) together when it left
    /// the structure, ascending; `None` when no such section fell (ADR-2734).
    #[must_use]
    pub fn section_bonds(&self, section: u32) -> Option<&[u32]> {
        self.section_bonds.get(&section).map(Vec::as_slice)
    }

    /// Bonds decided to break that the per-tick cap still holds back (0 without `LoadSettings`).
    #[must_use]
    pub fn pending_breaks(&self) -> usize {
        self.load.as_ref().map_or(0, |l| l.pending.len())
    }

    fn intact_bond_count(&self, chunk: usize) -> usize {
        self.chunk_bonds[chunk]
            .iter()
            .filter(|&&b| !self.bond_broken[b as usize])
            .count()
    }
}

/// Counters for budgets and the agent surface.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DestructionCounters {
    /// Ticks run.
    pub ticks: u64,
    /// Hits applied.
    pub hits: u64,
    /// Bonds broken.
    pub bonds_broken: u64,
    /// Chunks detached.
    pub chunks_detached: u64,
    /// Breaks the per-tick cap deferred (summed over ticks).
    pub deferred_breaks: u64,
}

/// Every destructible of a game world, the damage queue and the decided breaks.
#[derive(Clone, Debug, Default)]
pub struct DestructionWorld {
    authority: Authority,
    instances: BTreeMap<DestructibleId, DestructibleInstance>,
    next_id: u32,
    tick: u64,
    next_seq: u64,
    next_damage: u64,
    queue: Vec<(u64, Damage)>,
    spawn_queue: VecDeque<SpawnRequest>,
    event_digest: u64,
    budget: DestructionBudget,
    counters: DestructionCounters,
    /// The newest events (the last `EVENT_LOG_CAP`), for tools and the agent surface.
    log: VecDeque<BreakEvent>,
    /// Replica: events and effects applied since the last step, reported by the next step.
    replayed: StepReport,
    /// Structural load totals.
    load_stats: LoadStats,
    /// Instance ids whose bonds broke this tick. Kept across ticks so a cold step does
    /// not allocate it.
    touched: Vec<DestructibleId>,
}

/// How many recent events [`DestructionWorld::recent_events`] keeps.
pub const EVENT_LOG_CAP: usize = 256;

/// Most hits [`DestructionWorld::damage`] queues between two steps; further calls are rejected.
pub const MAX_QUEUED_DAMAGE: usize = 65_536;

impl DestructionWorld {
    /// An empty world with the given authority and the default budget.
    #[must_use]
    pub fn new(authority: Authority) -> Self {
        Self {
            authority,
            ..Self::default()
        }
    }

    /// Who decides breaks.
    #[must_use]
    pub fn authority(&self) -> Authority {
        self.authority
    }

    /// The budget.
    #[must_use]
    pub fn budget(&self) -> &DestructionBudget {
        &self.budget
    }

    /// Replaces the budget (takes effect on the next step).
    pub fn set_budget(&mut self, budget: DestructionBudget) {
        self.budget = budget;
    }

    /// Ticks run so far.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Counters since creation.
    #[must_use]
    pub fn counters(&self) -> &DestructionCounters {
        &self.counters
    }

    /// Structural load totals since creation (ADR-2734).
    #[must_use]
    pub fn load_stats(&self) -> LoadStats {
        let mut stats = self.load_stats;
        stats.instances = self.instances.values().filter(|i| i.load.is_some()).count() as u32;
        stats.pending_breaks = self.instances.values().map(|i| i.pending_breaks() as u64).sum();
        stats
    }

    /// Asks for a fresh structural-load solve of `id` on the next step (the game changed something
    /// the solve depends on). Does nothing for an asset without `LoadSettings`.
    ///
    /// # Errors
    /// [`DestructionError::UnknownId`].
    pub fn resolve_load(&mut self, id: DestructibleId) -> Result<(), DestructionError> {
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or(DestructionError::UnknownId(id.0))?;
        if let Some(load) = inst.load.as_mut() {
            load.dirty = true;
        }
        Ok(())
    }

    /// Solves the structural load of `id` now, on its current bonds, and reports the `top` bonds
    /// nearest their limit (highest load over limit first; ties by bond index) and what each support
    /// carries. It changes nothing: a server's decisions use the solve of its own step. An asset
    /// without `LoadSettings` is solved with the default settings and reported with `enabled:
    /// false`. Works on a replica too (its bonds are the server's, replayed).
    ///
    /// # Errors
    /// [`DestructionError::UnknownId`].
    pub fn load_report(&self, id: DestructibleId, top: usize) -> Result<LoadReport, DestructionError> {
        let inst = self.instances.get(&id).ok_or(DestructionError::UnknownId(id.0))?;
        let enabled = inst.asset.load.is_some();
        let settings = inst.asset.load.clone().unwrap_or_default();
        let usable: Vec<bool> = (0..inst.bond_broken.len())
            .map(|b| !inst.bond_broken[b] && !inst.load.as_ref().is_some_and(|l| l.doomed[b]))
            .collect();
        let solution = solve_load(
            &inst.asset,
            &inst.chunks,
            &usable,
            &inst.chunk_bonds,
            settings.gravity,
            settings.iterations,
        );
        let mut bonds: Vec<BondLoad> = inst
            .asset
            .bonds
            .iter()
            .enumerate()
            .filter(|(b, _)| usable[*b])
            .map(|(b, spec)| BondLoad {
                bond: b as u32,
                a: spec.a,
                b: spec.b,
                load: solution.flow[b].abs(),
                max_load: spec.max_load_under(&settings),
                damage: inst.bond_damage[b],
                break_impulse: spec.break_impulse,
            })
            .collect();
        let over_limit = bonds.iter().filter(|b| b.load > b.max_load).count() as u32;
        // Ratio order without a division: l1 / m1 > l2 / m2 ⇔ l1 * m2 > l2 * m1 (limits are > 0).
        bonds.sort_by(|x, y| {
            (y.load * x.max_load)
                .total_cmp(&(x.load * y.max_load))
                .then(x.bond.cmp(&y.bond))
        });
        bonds.truncate(top);
        Ok(LoadReport {
            destructible: id,
            enabled,
            gravity: settings.gravity,
            iterations: settings.iterations,
            iterations_run: solution.iterations,
            relative_residual: solution.relative_residual,
            supported_weight: solution.supported_weight,
            unsupported_chunks: solution.unsupported_chunks,
            over_limit,
            pending_breaks: inst.pending_breaks() as u32,
            problem: (!solution.finite).then_some("destruction_load_not_finite"),
            supports: solution.reactions,
            top: bonds,
        })
    }

    /// Places a destructible. Ids are handed out in call order, so a server and its clients that
    /// define the same instances in the same order agree on ids.
    ///
    /// # Errors
    /// [`DestructionError::Asset`] when the asset fails validation, [`DestructionError::InvalidPose`]
    /// for a non-finite pose or a zero rotation.
    pub fn define(
        &mut self,
        asset: Arc<DestructibleAsset>,
        pose: DestructiblePose,
        seed: u64,
    ) -> Result<DestructibleId, DestructionError> {
        let instance = new_instance(asset, normalized_pose(pose)?, seed)?;
        let id = DestructibleId(self.next_id);
        self.next_id += 1;
        self.instances.insert(id, instance);
        Ok(id)
    }

    /// Places a destructible at the id a server gave it, on a replica (a late joiner's ids need not
    /// run in order: the server may have removed some). `pose` is the server's, already normalized:
    /// it is kept bit for bit, because dividing a unit quaternion by its length again can move a bit
    /// and every position a replica derives comes from these numbers.
    ///
    /// # Errors
    /// [`DestructionError::NotReplica`] on a server world, [`DestructionError::BadSnapshot`] when
    /// the id is taken, otherwise as [`Self::define`].
    pub fn define_at(
        &mut self,
        id: DestructibleId,
        asset: Arc<DestructibleAsset>,
        pose: DestructiblePose,
        seed: u64,
    ) -> Result<(), DestructionError> {
        if self.authority != Authority::Replica {
            return Err(DestructionError::NotReplica);
        }
        if self.instances.contains_key(&id) {
            return Err(DestructionError::BadSnapshot(format!(
                "instance {} already exists",
                id.0
            )));
        }
        let instance = new_instance(asset, checked_pose(pose)?, seed)?;
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        self.instances.insert(id, instance);
        Ok(())
    }

    /// Changes who decides breaks (a session started or ended). Becoming a replica drops the hits
    /// still queued: only the server decides from here on.
    pub fn set_authority(&mut self, authority: Authority) {
        if authority == Authority::Replica {
            self.queue.clear();
        }
        self.authority = authority;
    }

    /// Moves an instance (a destructible on a vehicle, or held by a moving hull). Hits after the
    /// move use the new pose; break events carry world positions at the moment of the decision.
    /// A server and its replicas stay in step as long as they set the same poses on the same
    /// ticks (the game's netcode already agrees on those).
    ///
    /// # Errors
    /// [`DestructionError::UnknownId`] or [`DestructionError::InvalidPose`].
    pub fn set_pose(&mut self, id: DestructibleId, pose: DestructiblePose) -> Result<(), DestructionError> {
        let pose = normalized_pose(pose)?;
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or(DestructionError::UnknownId(id.0))?;
        inst.pose = pose;
        for (w, c) in inst.world_centroids.iter_mut().zip(&inst.asset.chunks) {
            *w = pose.transform_point(c.centroid);
        }
        Ok(())
    }

    /// Re-expresses every world position after a floating-origin rebase: `shift` (meters) is
    /// subtracted from each instance pose (the chunk centroids follow), from every queued
    /// debris spawn and from every queued hit's blast centre or shot origin. Break events already
    /// reported keep the positions they carried.
    pub fn shift_origin(&mut self, shift: [f32; 3]) {
        for inst in self.instances.values_mut() {
            for (t, s) in inst.pose.translation.iter_mut().zip(shift) {
                *t -= s;
            }
            let pose = inst.pose;
            for (w, c) in inst.world_centroids.iter_mut().zip(&inst.asset.chunks) {
                *w = pose.transform_point(c.centroid);
            }
        }
        for request in &mut self.spawn_queue {
            for (p, s) in request.position.iter_mut().zip(shift) {
                *p -= s;
            }
        }
        // Hits queued for the next step carry world-space points too (a blast centre, a shot's
        // origin); `Impulse` vectors are directions and do not move.
        for (_, damage) in &mut self.queue {
            let point = match &mut damage.kind {
                DamageKind::Explosion { center, .. } => center,
                DamageKind::Projectile { origin, .. } => origin,
                DamageKind::Impulse { .. } => continue,
            };
            for (p, s) in point.iter_mut().zip(shift) {
                *p -= s;
            }
        }
    }

    /// Removes an instance (its queued spawns go too). Returns whether it existed.
    pub fn remove(&mut self, id: DestructibleId) -> bool {
        self.spawn_queue.retain(|s| s.destructible != id);
        self.instances.remove(&id).is_some()
    }

    /// One instance.
    #[must_use]
    pub fn instance(&self, id: DestructibleId) -> Option<&DestructibleInstance> {
        self.instances.get(&id)
    }

    /// Every instance in id order.
    pub fn instances(&self) -> impl Iterator<Item = (DestructibleId, &DestructibleInstance)> {
        self.instances.iter().map(|(id, i)| (*id, i))
    }

    /// Queues a hit for the next step and returns its queue number.
    ///
    /// # Errors
    /// [`DestructionError::Replica`] on a replica, [`DestructionError::UnknownId`] /
    /// [`DestructionError::UnknownChunk`] for a bad target, [`DestructionError::InvalidDamage`]
    /// for a non-finite or out-of-range parameter, and when the queue already holds
    /// [`MAX_QUEUED_DAMAGE`] hits waiting for the next step.
    pub fn damage(&mut self, damage: Damage) -> Result<u64, DestructionError> {
        if self.authority == Authority::Replica {
            return Err(DestructionError::Replica);
        }
        if let Some(id) = damage.target
            && !self.instances.contains_key(&id)
        {
            return Err(DestructionError::UnknownId(id.0));
        }
        let bad = |s: &str| Err(DestructionError::InvalidDamage(s.to_string()));
        match &damage.kind {
            DamageKind::Explosion {
                center,
                radius,
                impulse,
            } => {
                if !finite3(*center) || !(radius.is_finite() && *radius > 0.0) {
                    return bad("explosion needs a finite center and radius > 0");
                }
                if !(impulse.is_finite() && *impulse >= 0.0) {
                    return bad("impulse must be finite and >= 0");
                }
            }
            DamageKind::Projectile {
                origin,
                direction,
                impulse,
                radius,
                max_distance,
            } => {
                if !finite3(*origin) || !finite3(*direction) || dot(*direction, *direction) <= 0.0 {
                    return bad("projectile needs a finite origin and a non-zero direction");
                }
                if !(impulse.is_finite() && *impulse >= 0.0)
                    || !(radius.is_finite() && *radius >= 0.0)
                    || !(max_distance.is_finite() && *max_distance > 0.0)
                {
                    return bad("projectile impulse and radius must be >= 0, max_distance > 0");
                }
            }
            DamageKind::Impulse { chunk, impulse } => {
                let Some(id) = damage.target else {
                    return bad("an impulse needs a target");
                };
                if !finite3(*impulse) {
                    return bad("impulse must be finite");
                }
                let n = self.instances.get(&id).map_or(0, |i| i.chunks.len());
                if *chunk as usize >= n {
                    return Err(DestructionError::UnknownChunk {
                        id: id.0,
                        chunk: *chunk,
                    });
                }
            }
        }
        let n = self.next_damage;
        self.next_damage += 1;
        if self.queue.len() >= MAX_QUEUED_DAMAGE {
            return Err(DestructionError::InvalidDamage(format!(
                "damage queue is full ({MAX_QUEUED_DAMAGE} hits waiting for the next step)"
            )));
        }
        self.queue.push((n, damage));
        Ok(n)
    }

    /// Hits waiting for the next step.
    #[must_use]
    pub fn queued_damage(&self) -> usize {
        self.queue.len()
    }

    /// Runs one fixed tick of `dt` seconds: decay, queued hits in queue order, breaks (capped),
    /// the support graph, detaches. A replica only advances its tick (it breaks through
    /// [`Self::apply_events`]); its report carries the events and effects applied since the last
    /// step, so the same code plays sounds and effects on server and client.
    #[tracing::instrument(skip_all, target = "regia_physics", name = "destruction_step")]
    pub fn step(&mut self, dt: f32) -> StepReport {
        self.tick += 1;
        self.counters.ticks += 1;
        let mut report = StepReport {
            tick: self.tick,
            ..StepReport::default()
        };
        if self.authority == Authority::Replica {
            report.events = std::mem::take(&mut self.replayed.events);
            report.fx = std::mem::take(&mut self.replayed.fx);
            return report;
        }
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        for inst in self.instances.values_mut() {
            inst.tick_impulse.fill([0.0; 3]);
            inst.tick_hit.fill(false);
            let heal = inst.asset.material.decay_per_second * dt;
            if heal > 0.0 {
                // A bond already decided to break (structural load) stays decided.
                let doomed = inst.load.as_ref().map(|l| l.doomed.as_slice());
                for (b, (d, broken)) in inst.bond_damage.iter_mut().zip(&inst.bond_broken).enumerate() {
                    if !broken && !doomed.is_some_and(|x| x[b]) {
                        *d = (*d - heal).max(0.0);
                    }
                }
            }
        }
        let mut queue = std::mem::take(&mut self.queue);
        // Stable, in sequence order. A single queued hit does not allocate a key buffer.
        queue.sort_by_key(|hit| hit.0);
        for (_, damage) in &queue {
            if self.apply_damage(damage) {
                report.hits += 1;
            } else {
                report.misses += 1;
            }
        }
        queue.clear();
        self.queue = queue;
        self.counters.hits += u64::from(report.hits);

        // Structural load (assets with `LoadSettings`): solve where the bonds changed, then add
        // the overload of each bond to its damage.
        for inst in self.instances.values_mut() {
            step_load(inst, dt, &mut self.load_stats);
        }

        // Breaks, in (instance, bond) order, capped per tick.
        let cap = match self.budget.max_breaks_per_tick {
            0 => usize::MAX,
            n => n as usize,
        };
        let mut broken_now = 0usize;
        let tick = self.tick;
        let mut touched = std::mem::take(&mut self.touched);
        touched.clear();
        {
            let spawn_queue = &mut self.spawn_queue;
            let instances = &mut self.instances;
            for (&id, inst) in instances.iter_mut() {
                if inst.load.is_some() {
                    report.deferred_breaks +=
                        break_under_load(inst, id, tick, cap, &mut broken_now, &mut report, spawn_queue);
                    continue;
                }
                if !inst.hot {
                    continue;
                }
                let deferred_before = report.deferred_breaks;
                let mut any = false;
                for bond in 0..inst.bond_broken.len() {
                    if inst.bond_broken[bond] {
                        continue;
                    }
                    let spec = &inst.asset.bonds[bond];
                    if inst.bond_damage[bond] < spec.break_impulse {
                        continue;
                    }
                    if broken_now >= cap {
                        report.deferred_breaks += 1;
                        continue;
                    }
                    broken_now += 1;
                    any = true;
                    inst.bond_broken[bond] = true;
                    let position = inst.pose.transform_point(spec.anchor);
                    let seed = mix_seed(inst.seed, tick, 0x8000_0000 | bond as u64);
                    report.fx.push(DestructionFx {
                        destructible: id,
                        kind: FxKind::BondBreak,
                        position,
                        magnitude: spec.break_impulse,
                        fx_tag: inst.asset.material.fx_tag.clone(),
                        seed,
                    });
                    report.events.push(BreakEvent {
                        seq: 0,
                        tick,
                        destructible: id,
                        kind: BreakKind::BondBroken {
                            bond: bond as u32,
                            a: spec.a,
                            b: spec.b,
                            position,
                        },
                        seed,
                    });
                }
                // Bonds held back by the cap keep the instance hot for the next tick.
                inst.hot = report.deferred_breaks > deferred_before;
                if any {
                    touched.push(id);
                }
            }
        }
        self.counters.deferred_breaks += u64::from(report.deferred_breaks);

        // Support graph: detach what no intact path joins to a support.
        for &id in &touched {
            let Some(inst) = self.instances.get_mut(&id) else {
                continue;
            };
            for chunk in unsupported_chunks(inst) {
                let cause = if inst.tick_hit[chunk as usize] {
                    DetachCause::Damage
                } else {
                    DetachCause::Collapse
                };
                detach(inst, id, chunk, cause, tick, &mut report, &mut self.spawn_queue);
            }
        }
        self.touched = touched;
        self.finish_events(&mut report);
        report
    }

    /// Applies a server's events on a replica, in `seq` order: the same bonds break, the same
    /// chunks detach with the same velocities, and [`Self::structure_hash`] matches the
    /// server's. Events must come in increasing `seq` with no gap from the last applied one.
    ///
    /// # Errors
    /// [`DestructionError::NotReplica`] on a server world, [`DestructionError::BadEvent`] for an
    /// event out of order or naming something this world does not have (the events before it
    /// stay applied).
    pub fn apply_events(&mut self, events: &[BreakEvent]) -> Result<StepReport, DestructionError> {
        if self.authority != Authority::Replica {
            return Err(DestructionError::NotReplica);
        }
        let mut report = StepReport {
            tick: self.tick,
            ..StepReport::default()
        };
        if events.len() > MAX_EVENTS_PER_CALL {
            return Err(DestructionError::BadEvent {
                seq: events[0].seq,
                reason: format!(
                    "{} events in one call (at most {MAX_EVENTS_PER_CALL})",
                    events.len()
                ),
            });
        }
        for event in events {
            let bad = |reason: &str| DestructionError::BadEvent {
                seq: event.seq,
                reason: reason.to_string(),
            };
            if event.seq != self.next_seq {
                return Err(bad(&format!("expected seq {}", self.next_seq)));
            }
            let Some(inst) = self.instances.get_mut(&event.destructible) else {
                return Err(bad("unknown destructible"));
            };
            // Positions and velocities reach rapier: a bad peer must not get a NaN that far.
            let finite = match &event.kind {
                BreakKind::BondBroken { position, .. } => finite3(*position),
                BreakKind::ChunkDetached {
                    position,
                    linvel,
                    angvel,
                    ..
                } => finite3(*position) && finite3(*linvel) && finite3(*angvel),
            };
            if !finite {
                return Err(bad("a position or velocity is not finite"));
            }
            match &event.kind {
                BreakKind::BondBroken { bond, position, .. } => {
                    let Some(spec) = inst.asset.bonds.get(*bond as usize) else {
                        return Err(bad("unknown bond"));
                    };
                    if inst.bond_broken[*bond as usize] {
                        return Err(bad("bond already broken"));
                    }
                    inst.bond_broken[*bond as usize] = true;
                    self.counters.bonds_broken += 1;
                    report.fx.push(DestructionFx {
                        destructible: event.destructible,
                        kind: FxKind::BondBreak,
                        position: *position,
                        magnitude: spec.break_impulse,
                        fx_tag: inst.asset.material.fx_tag.clone(),
                        seed: event.seed,
                    });
                }
                BreakKind::ChunkDetached {
                    chunk,
                    cause,
                    position,
                    linvel,
                    angvel,
                } => {
                    let c = *chunk as usize;
                    if c >= inst.chunks.len() || inst.chunks[c] != ChunkState::Attached {
                        return Err(bad("unknown or already detached chunk"));
                    }
                    let section = assign_section(inst, c);
                    inst.chunks[c] = ChunkState::Detached;
                    for &b in &inst.chunk_bonds[c] {
                        inst.bond_broken[b as usize] = true;
                    }
                    report.fx.push(DestructionFx {
                        destructible: event.destructible,
                        kind: fx_kind(*cause),
                        position: *position,
                        magnitude: inst.asset.chunks[c].mass,
                        fx_tag: chunk_fx_tag(&inst.asset, c),
                        seed: event.seed,
                    });
                    self.spawn_queue.push_back(SpawnRequest {
                        destructible: event.destructible,
                        chunk: *chunk,
                        position: *position,
                        rotation: inst.pose.rotation,
                        linvel: *linvel,
                        angvel: *angvel,
                        tick: event.tick,
                        section,
                    });
                    self.counters.chunks_detached += 1;
                }
            }
            self.tick = self.tick.max(event.tick);
            self.event_digest = fold_event(self.event_digest, event);
            self.next_seq += 1;
            push_log(&mut self.log, event.clone());
            report.events.push(event.clone());
            // Kept for the next step's report too (every arm above added exactly one effect).
            self.replayed.events.push(event.clone());
            if let Some(fx) = report.fx.last() {
                self.replayed.fx.push(fx.clone());
            }
        }
        report.tick = self.tick;
        Ok(report)
    }

    /// The short form of `event`, or `None` when a replica could not rebuild it bit for bit from
    /// this world's asset, pose and seed (the instance moved after it was decided, or it is gone).
    #[must_use]
    pub fn compact_event(&self, event: &BreakEvent) -> Option<CompactBreak> {
        let compact = match &event.kind {
            BreakKind::BondBroken { bond, .. } => CompactBreak::Bond { bond: *bond },
            BreakKind::ChunkDetached {
                chunk, cause, linvel, ..
            } => CompactBreak::Detach {
                chunk: *chunk,
                cause: *cause,
                linvel: *linvel,
            },
        };
        let back = self.expand_event(event.seq, event.tick, event.destructible, &compact)?;
        (fold_event(0, &back) == fold_event(0, event)).then_some(compact)
    }

    /// The full event a [`CompactBreak`] stands for; `None` when this world has no such instance,
    /// bond or chunk.
    #[must_use]
    pub fn expand_event(
        &self,
        seq: u64,
        tick: u64,
        destructible: DestructibleId,
        compact: &CompactBreak,
    ) -> Option<BreakEvent> {
        let inst = self.instances.get(&destructible)?;
        let (kind, seed) = match compact {
            CompactBreak::Bond { bond } => {
                let spec = inst.asset.bonds.get(*bond as usize)?;
                (
                    BreakKind::BondBroken {
                        bond: *bond,
                        a: spec.a,
                        b: spec.b,
                        position: inst.pose.transform_point(spec.anchor),
                    },
                    mix_seed(inst.seed, tick, 0x8000_0000 | u64::from(*bond)),
                )
            }
            CompactBreak::Detach { chunk, cause, linvel } => {
                let seed = mix_seed(inst.seed, tick, u64::from(*chunk));
                (
                    BreakKind::ChunkDetached {
                        chunk: *chunk,
                        cause: *cause,
                        position: *inst.world_centroids.get(*chunk as usize)?,
                        linvel: *linvel,
                        angvel: detach_spin(seed, inst.asset.material.detach_spin),
                    },
                    seed,
                )
            }
        };
        Some(BreakEvent {
            seq,
            tick,
            destructible,
            kind,
            seed,
        })
    }

    /// What a replica needs to catch up ([`Self::restore`]).
    #[must_use]
    pub fn snapshot(&self) -> DestructionSnapshot {
        let instances = self
            .instances
            .iter()
            .map(|(id, inst)| instance_state(*id, inst))
            .collect();
        DestructionSnapshot {
            next_id: self.next_id,
            next_seq: self.next_seq,
            event_digest: self.event_digest,
            tick: self.tick,
            instances,
        }
    }

    /// One instance as a snapshot holds it ([`Self::restore_instance`] puts it back); `None` for
    /// an unknown id.
    #[must_use]
    pub fn instance_snapshot(&self, id: DestructibleId) -> Option<InstanceSnapshot> {
        self.instances.get(&id).map(|inst| instance_state(id, inst))
    }

    /// Replaces this replica's instances and event stream with `snapshot`; `assets` finds each
    /// instance's asset. Nothing changes on an error. Waiting debris spawns are dropped: chunks a
    /// snapshot says are detached get no body, they are simply gone.
    ///
    /// # Errors
    /// [`DestructionError::NotReplica`] on a server world, [`DestructionError::BadSnapshot`] for an
    /// unknown asset, chunk or bond, otherwise as [`Self::define`].
    pub fn restore(
        &mut self,
        snapshot: &DestructionSnapshot,
        assets: &dyn Fn(DestructibleId) -> Option<Arc<DestructibleAsset>>,
    ) -> Result<(), DestructionError> {
        if self.authority != Authority::Replica {
            return Err(DestructionError::NotReplica);
        }
        let bad = |what: String| DestructionError::BadSnapshot(what);
        let mut instances = BTreeMap::new();
        for s in &snapshot.instances {
            let asset = assets(s.id).ok_or_else(|| bad(format!("no asset for instance {}", s.id.0)))?;
            let inst = restored_instance(s, asset)?;
            if instances.insert(s.id, inst).is_some() {
                return Err(bad(format!("instance {} appears twice", s.id.0)));
            }
        }
        self.instances = instances;
        self.next_id = snapshot.next_id;
        self.next_seq = snapshot.next_seq;
        self.event_digest = snapshot.event_digest;
        self.tick = snapshot.tick;
        self.spawn_queue.clear();
        self.log.clear();
        self.replayed = StepReport::default();
        Ok(())
    }

    /// Adds one instance in the state a server snapshot gave it, on a replica whose server
    /// streams only some instances to it (ADR-2305): the instance became relevant. The event
    /// sequence is left alone. Its colliders and root come on the next tick, as after
    /// [`Self::restore`]; chunks the state says are detached get neither.
    ///
    /// # Errors
    /// [`DestructionError::NotReplica`] on a server world, [`DestructionError::BadSnapshot`] when
    /// the id is taken or the state names an unknown chunk or bond, otherwise as [`Self::define`].
    pub fn restore_instance(
        &mut self,
        state: &InstanceSnapshot,
        asset: Arc<DestructibleAsset>,
    ) -> Result<(), DestructionError> {
        if self.authority != Authority::Replica {
            return Err(DestructionError::NotReplica);
        }
        if self.instances.contains_key(&state.id) {
            return Err(DestructionError::BadSnapshot(format!(
                "instance {} already exists",
                state.id.0
            )));
        }
        let inst = restored_instance(state, asset)?;
        self.next_id = self.next_id.max(state.id.0.saturating_add(1));
        self.instances.insert(state.id, inst);
        Ok(())
    }

    /// Moves a replica's event sequence past events it does not apply, because they broke
    /// instances its server does not stream to it (ADR-2305): `next_seq` and `event_digest` are
    /// the server's after them, so [`Self::structure_hash`] can still match the server's hash of
    /// the instances this replica holds ([`Self::structure_hash_where`]).
    ///
    /// # Errors
    /// [`DestructionError::NotReplica`] on a server world, [`DestructionError::BadEvent`] when
    /// `next_seq` is behind this world's.
    pub fn skip_events(&mut self, next_seq: u64, event_digest: u64) -> Result<(), DestructionError> {
        if self.authority != Authority::Replica {
            return Err(DestructionError::NotReplica);
        }
        if next_seq < self.next_seq {
            return Err(DestructionError::BadEvent {
                seq: next_seq,
                reason: format!("skip back from seq {}", self.next_seq),
            });
        }
        self.next_seq = next_seq;
        self.event_digest = event_digest;
        Ok(())
    }

    /// Takes up to `limit` detached chunks waiting for a debris body (0: all), oldest first.
    pub fn take_spawns(&mut self, limit: usize) -> Vec<SpawnRequest> {
        let n = if limit == 0 {
            self.spawn_queue.len()
        } else {
            limit.min(self.spawn_queue.len())
        };
        self.spawn_queue.drain(..n).collect()
    }

    /// Takes up to `limit` groups of detached chunks waiting for a body (0: all), oldest first. A
    /// group is one falling section (every chunk of it detached on that tick) or a lone chunk;
    /// without `FallingSettings` every group is one chunk, in [`Self::take_spawns`] order.
    pub fn take_spawn_groups(&mut self, limit: usize) -> Vec<Vec<SpawnRequest>> {
        let mut groups = Vec::new();
        while limit == 0 || groups.len() < limit {
            let Some(first) = self.spawn_queue.pop_front() else {
                break;
            };
            let is_section = self
                .instances
                .get(&first.destructible)
                .is_some_and(|i| i.section_bonds.contains_key(&first.section));
            let mut group = vec![first];
            if is_section {
                let key = (group[0].destructible, group[0].section, group[0].tick);
                let mut rest = VecDeque::with_capacity(self.spawn_queue.len());
                for r in self.spawn_queue.drain(..) {
                    if (r.destructible, r.section, r.tick) == key {
                        group.push(r);
                    } else {
                        rest.push_back(r);
                    }
                }
                self.spawn_queue = rest;
            }
            groups.push(group);
        }
        groups
    }

    /// Detached chunks still waiting for a debris body.
    #[must_use]
    pub fn pending_spawns(&self) -> usize {
        self.spawn_queue.len()
    }

    /// The newest break events (up to [`EVENT_LOG_CAP`]), oldest first.
    pub fn recent_events(&self) -> impl Iterator<Item = &BreakEvent> {
        self.log.iter()
    }

    /// Events decided or applied so far (the next event's `seq`).
    #[must_use]
    pub fn event_count(&self) -> u64 {
        self.next_seq
    }

    /// The digest of every event so far (what [`fold_event_digest`] folds each event into).
    #[must_use]
    pub fn event_digest(&self) -> u64 {
        self.event_digest
    }

    /// Hash of what a replica must match: every instance's chunk states and broken bonds, and
    /// the digest of every event (ids, ticks, seeds, positions, velocities). FNV-1a 64.
    #[must_use]
    pub fn structure_hash(&self) -> u64 {
        self.structure_hash_where(|_| true)
    }

    /// [`Self::structure_hash`] over the instances `keep` accepts, as a replica that holds only
    /// those computes it ([`Self::skip_events`]). Keeping every instance gives
    /// [`Self::structure_hash`].
    #[must_use]
    pub fn structure_hash_where(&self, keep: impl Fn(DestructibleId) -> bool) -> u64 {
        let mut h = Fnv::new();
        let kept = self.instances.keys().filter(|id| keep(**id)).count();
        h.u64(kept as u64);
        for (id, inst) in self.instances.iter().filter(|(id, _)| keep(**id)) {
            h.u64(u64::from(id.0));
            h.u64(inst.chunks.len() as u64);
            for s in &inst.chunks {
                h.u8(match s {
                    ChunkState::Attached => 1,
                    ChunkState::Detached => 2,
                });
            }
            for b in &inst.bond_broken {
                h.u8(u8::from(*b));
            }
        }
        h.u64(self.next_seq);
        h.u64(self.event_digest);
        h.finish()
    }

    /// [`Self::structure_hash`] plus the server-only state: the tick and every bond's
    /// accumulated damage (bit patterns). Two servers fed the same hits agree on it.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.structure_hash());
        h.u64(self.tick);
        for inst in self.instances.values() {
            for d in &inst.bond_damage {
                h.u32(d.to_bits());
            }
        }
        h.finish()
    }

    fn finish_events(&mut self, report: &mut StepReport) {
        for event in &mut report.events {
            event.seq = self.next_seq;
            self.next_seq += 1;
            self.event_digest = fold_event(self.event_digest, event);
            match event.kind {
                BreakKind::BondBroken { .. } => self.counters.bonds_broken += 1,
                BreakKind::ChunkDetached { .. } => self.counters.chunks_detached += 1,
            }
            push_log(&mut self.log, event.clone());
        }
    }

    /// Applies one hit; `false` when it reached no chunk.
    fn apply_damage(&mut self, damage: &Damage) -> bool {
        match &damage.kind {
            DamageKind::Explosion {
                center,
                radius,
                impulse,
            } => {
                let mut reached = false;
                for (id, inst) in &mut self.instances {
                    if damage.target.is_some_and(|t| t != *id) {
                        continue;
                    }
                    reached |= splash(inst, *center, *radius, *impulse, None);
                }
                reached
            }
            DamageKind::Projectile {
                origin,
                direction,
                impulse,
                radius,
                max_distance,
            } => {
                let dir = normalize(*direction);
                let mut best: Option<(f32, DestructibleId, u32)> = None;
                for (id, inst) in &self.instances {
                    if damage.target.is_some_and(|t| t != *id) {
                        continue;
                    }
                    if let Some((t, chunk)) = ray_hit(inst, *origin, dir, *max_distance)
                        && best.is_none_or(|(bt, _, _)| t < bt)
                    {
                        best = Some((t, *id, chunk));
                    }
                }
                let Some((t, id, chunk)) = best else {
                    return false;
                };
                let Some(inst) = self.instances.get_mut(&id) else {
                    return false;
                };
                let point = add(*origin, scale(dir, t));
                if *radius > 0.0 {
                    splash(inst, point, *radius, *impulse, Some((chunk, dir)));
                } else {
                    load_chunk(inst, chunk as usize, scale(dir, *impulse));
                }
                true
            }
            DamageKind::Impulse { chunk, impulse } => {
                let Some(inst) = damage.target.and_then(|id| self.instances.get_mut(&id)) else {
                    return false;
                };
                let c = *chunk as usize;
                if c >= inst.chunks.len() || inst.chunks[c] != ChunkState::Attached {
                    return false;
                }
                load_chunk(inst, c, *impulse);
                true
            }
        }
    }
}

/// One step of structural load for an instance with `LoadSettings`: a fresh solve when the
/// load-carrying bonds changed, then each over-limit bond's overload added to its damage.
fn step_load(inst: &mut DestructibleInstance, dt: f32, stats: &mut LoadStats) {
    let DestructibleInstance {
        asset,
        chunks,
        bond_damage,
        bond_broken,
        chunk_bonds,
        load,
        ..
    } = inst;
    let (Some(settings), Some(state)) = (asset.load.as_ref(), load.as_mut()) else {
        return;
    };
    if state.dirty {
        let _span = tracing::debug_span!(target: "regia_physics", "destruction_load_solve").entered();
        let usable: Vec<bool> = bond_broken
            .iter()
            .zip(&state.doomed)
            .map(|(broken, doomed)| !broken && !doomed)
            .collect();
        state.solution = solve_load(
            asset,
            chunks,
            &usable,
            chunk_bonds,
            settings.gravity,
            settings.iterations,
        );
        state.dirty = false;
        state.solves += 1;
        stats.solves += 1;
        stats.iterations += u64::from(state.solution.iterations);
        if !state.solution.finite {
            stats.non_finite += 1;
        }
    }
    state.overloaded = false;
    if settings.failure_rate <= 0.0 || dt <= 0.0 {
        return;
    }
    for (b, spec) in asset.bonds.iter().enumerate() {
        if bond_broken[b] || state.doomed[b] {
            continue;
        }
        let carried = state.solution.flow[b].abs();
        let limit = spec.max_load_under(settings);
        if carried > limit {
            bond_damage[b] += (carried - limit) * settings.failure_rate * dt;
            state.overloaded = true;
        }
    }
}

/// The breaks of an instance with `LoadSettings`: bonds at or over their threshold join the
/// instance's queue in bond order (they stop carrying load at once), and the queue breaks oldest
/// first while the world's per-tick cap allows. The support graph runs each time the last bond
/// decided on one tick has broken, so a capped run detaches the same chunks after the same bonds as
/// an uncapped one. Returns the breaks still held back.
fn break_under_load(
    inst: &mut DestructibleInstance,
    id: DestructibleId,
    tick: u64,
    cap: usize,
    broken_now: &mut usize,
    report: &mut StepReport,
    spawns: &mut VecDeque<SpawnRequest>,
) -> u32 {
    let Some(mut state) = inst.load.take() else {
        return 0;
    };
    if inst.hot || state.overloaded {
        for (bond, spec) in inst.asset.bonds.iter().enumerate() {
            if inst.bond_broken[bond] || state.doomed[bond] || inst.bond_damage[bond] < spec.break_impulse {
                continue;
            }
            state.doomed[bond] = true;
            state.pending.push_back((tick, bond as u32));
            state.dirty = true;
        }
    }
    inst.hot = false;
    while *broken_now < cap {
        let Some((decided, bond)) = state.pending.pop_front() else {
            break;
        };
        let b = bond as usize;
        // A bond of a chunk that detached since it was decided broke with the chunk.
        if !inst.bond_broken[b] {
            *broken_now += 1;
            inst.bond_broken[b] = true;
            let spec = &inst.asset.bonds[b];
            let position = inst.pose.transform_point(spec.anchor);
            let seed = mix_seed(inst.seed, tick, 0x8000_0000 | u64::from(bond));
            report.fx.push(DestructionFx {
                destructible: id,
                kind: FxKind::BondBreak,
                position,
                magnitude: spec.break_impulse,
                fx_tag: inst.asset.material.fx_tag.clone(),
                seed,
            });
            report.events.push(BreakEvent {
                seq: 0,
                tick,
                destructible: id,
                kind: BreakKind::BondBroken {
                    bond,
                    a: spec.a,
                    b: spec.b,
                    position,
                },
                seed,
            });
        }
        if state.pending.front().is_none_or(|(next, _)| *next != decided) {
            for chunk in unsupported_chunks(inst) {
                let cause = if inst.tick_hit[chunk as usize] {
                    DetachCause::Damage
                } else {
                    DetachCause::Collapse
                };
                detach(inst, id, chunk, cause, tick, report, spawns);
                state.dirty = true;
            }
        }
    }
    let held = state.pending.len() as u32;
    inst.load = Some(state);
    held
}

/// Loads one chunk with an impulse vector: records it for the detach velocity and spreads its
/// magnitude (scaled, thresholded) over the chunk's intact bonds.
fn load_chunk(inst: &mut DestructibleInstance, c: usize, impulse: [f32; 3]) {
    let m = &inst.asset.material;
    let scaled = scale(impulse, m.damage_scale);
    let magnitude = length(scaled);
    if magnitude <= 0.0 || magnitude < m.min_impulse {
        return;
    }
    inst.tick_hit[c] = true;
    inst.hot = true;
    inst.tick_impulse[c] = add(inst.tick_impulse[c], scaled);
    let n = inst.intact_bond_count(c);
    if n == 0 {
        return;
    }
    let share = magnitude / n as f32;
    for i in 0..inst.chunk_bonds[c].len() {
        let b = inst.chunk_bonds[c][i] as usize;
        if !inst.bond_broken[b] {
            inst.bond_damage[b] += share;
        }
    }
}

/// A blast around `center`: linear falloff to `radius`. With `along`, the push follows that
/// direction (a shot) and the hit chunk is always reached. Returns whether any chunk was loaded.
fn splash(
    inst: &mut DestructibleInstance,
    center: [f32; 3],
    radius: f32,
    impulse: f32,
    along: Option<(u32, [f32; 3])>,
) -> bool {
    let mut reached = false;
    for c in 0..inst.chunks.len() {
        if inst.chunks[c] != ChunkState::Attached {
            continue;
        }
        let offset = sub(inst.world_centroids[c], center);
        let d = length(offset);
        let is_hit = along.is_some_and(|(hit, _)| hit as usize == c);
        if d >= radius && !is_hit {
            continue;
        }
        let falloff = if d >= radius { 0.0 } else { 1.0 - d / radius };
        // The chunk a shot hit gets at least half the impulse even when its centre is far from
        // the hit point (a big chunk struck at its edge).
        let falloff = if is_hit { falloff.max(0.5) } else { falloff };
        let dir = match along {
            Some((_, dir)) => dir,
            None if d > 0.0 => scale(offset, 1.0 / d),
            None => [0.0, 1.0, 0.0],
        };
        load_chunk(inst, c, scale(dir, impulse * falloff));
        reached = true;
    }
    reached
}

/// The nearest attached chunk whose bounds the ray enters within `max_distance`: `(t, chunk)`.
fn ray_hit(
    inst: &DestructibleInstance,
    origin: [f32; 3],
    dir: [f32; 3],
    max_distance: f32,
) -> Option<(f32, u32)> {
    let o = inst.pose.inverse_transform_point(origin);
    let d = rotate(conjugate(inst.pose.rotation), dir);
    let mut best: Option<(f32, u32)> = None;
    for (c, chunk) in inst.asset.chunks.iter().enumerate() {
        if inst.chunks[c] != ChunkState::Attached {
            continue;
        }
        let mut tmin = 0.0f32;
        let mut tmax = max_distance;
        let mut hit = true;
        let axes = o
            .iter()
            .zip(&d)
            .zip(chunk.bounds_min.iter().zip(&chunk.bounds_max));
        for ((&ok, &dk), (&lo, &hi)) in axes {
            if dk == 0.0 {
                if ok < lo || ok > hi {
                    hit = false;
                    break;
                }
                continue;
            }
            let inv = 1.0 / dk;
            let (t1, t2) = ((lo - ok) * inv, (hi - ok) * inv);
            let (near, far) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
            tmin = tmin.max(near);
            tmax = tmax.min(far);
            if tmin > tmax {
                hit = false;
                break;
            }
        }
        if hit && best.is_none_or(|(bt, _)| tmin < bt) {
            best = Some((tmin, c as u32));
        }
    }
    best
}

/// Attached chunks with no path of intact bonds to a support chunk, ascending. Without support
/// chunks, everything outside the largest piece (ties: the piece holding the lowest index).
fn unsupported_chunks(inst: &DestructibleInstance) -> Vec<u32> {
    let n = inst.chunks.len();
    let mut component = vec![u32::MAX; n];
    let mut sizes: Vec<usize> = Vec::new();
    let mut supported: Vec<bool> = Vec::new();
    let mut stack = Vec::new();
    for (start, state) in inst.chunks.iter().enumerate() {
        if *state != ChunkState::Attached || component[start] != u32::MAX {
            continue;
        }
        let label = sizes.len() as u32;
        let mut size = 0usize;
        let mut has_support = false;
        component[start] = label;
        stack.push(start);
        while let Some(c) = stack.pop() {
            size += 1;
            has_support |= inst.asset.chunks[c].support;
            for &b in &inst.chunk_bonds[c] {
                if inst.bond_broken[b as usize] {
                    continue;
                }
                let spec = &inst.asset.bonds[b as usize];
                let other = if spec.a as usize == c { spec.b } else { spec.a } as usize;
                if inst.chunks[other] == ChunkState::Attached && component[other] == u32::MAX {
                    component[other] = label;
                    stack.push(other);
                }
            }
        }
        sizes.push(size);
        supported.push(has_support);
    }
    let any_support = inst.asset.chunks.iter().any(|c| c.support);
    let keep: Vec<bool> = if any_support {
        supported
    } else {
        // Labels are handed out in ascending start index, so the first maximum is the piece
        // holding the lowest index.
        let mut best = 0usize;
        for (label, &size) in sizes.iter().enumerate() {
            if size > sizes[best] {
                best = label;
            }
        }
        (0..sizes.len()).map(|label| label == best).collect()
    };
    (0..n)
        .filter(|&c| inst.chunks[c] == ChunkState::Attached && !keep[component[c] as usize])
        .map(|c| c as u32)
        .collect()
}

fn fx_kind(cause: DetachCause) -> FxKind {
    match cause {
        DetachCause::Damage => FxKind::ChunkDetach,
        DetachCause::Collapse => FxKind::Collapse,
    }
}

/// Marks `chunk` detached, breaks its remaining bonds, and records the event, the effect and the
/// spawn request.
fn detach(
    inst: &mut DestructibleInstance,
    id: DestructibleId,
    chunk: u32,
    cause: DetachCause,
    tick: u64,
    report: &mut StepReport,
    spawns: &mut VecDeque<SpawnRequest>,
) {
    let c = chunk as usize;
    let section = assign_section(inst, c);
    inst.chunks[c] = ChunkState::Detached;
    for &b in &inst.chunk_bonds[c] {
        inst.bond_broken[b as usize] = true;
    }
    let spec = &inst.asset.chunks[c];
    let seed = mix_seed(inst.seed, tick, u64::from(chunk));
    // A valid but tiny mass (`1 / mass` overflows) or an impulse sum that overflowed `f32` would
    // put an infinite velocity in the event: rapier must never see it (spec 15 § 5), and a replica
    // refuses the event (`apply_events`, the wire's `read_finite`). It detaches at rest instead
    // (ADR-7417).
    let linvel = scale(inst.tick_impulse[c], 1.0 / spec.mass);
    let linvel = if finite3(linvel) {
        linvel
    } else {
        tracing::warn!(
            target: "regia_physics",
            destructible = id.0,
            chunk,
            "destruction.detach_velocity_not_finite"
        );
        [0.0; 3]
    };
    let angvel = detach_spin(seed, inst.asset.material.detach_spin);
    let position = inst.world_centroids[c];
    report.fx.push(DestructionFx {
        destructible: id,
        kind: fx_kind(cause),
        position,
        magnitude: spec.mass,
        fx_tag: chunk_fx_tag(&inst.asset, c),
        seed,
    });
    report.events.push(BreakEvent {
        seq: 0,
        tick,
        destructible: id,
        kind: BreakKind::ChunkDetached {
            chunk,
            cause,
            position,
            linvel,
            angvel,
        },
        seed,
    });
    spawns.push_back(SpawnRequest {
        destructible: id,
        chunk,
        position,
        rotation: inst.pose.rotation,
        linvel,
        angvel,
        tick,
        section,
    });
}

/// The falling section of chunk `c`, decided when the first chunk of its piece detaches: the
/// piece is the chunks still attached and joined to `c` by intact bonds (everything that left the
/// structure together). A piece of at least `min_section_chunks` chunks is one section named by
/// its lowest chunk index, with the bonds that hold it; a smaller one, or any piece without
/// `FallingSettings`, falls chunk by chunk. A server and a replica detach the same chunks in the
/// same order from the same bonds, so both decide the same sections.
fn assign_section(inst: &mut DestructibleInstance, c: usize) -> u32 {
    let Some(falling) = inst.asset.falling.as_ref() else {
        return c as u32;
    };
    if inst.section_of[c] != u32::MAX {
        return inst.section_of[c];
    }
    let mut members = vec![c];
    let mut seen = vec![false; inst.chunks.len()];
    seen[c] = true;
    let mut bonds = Vec::new();
    let mut i = 0;
    while i < members.len() {
        let m = members[i];
        i += 1;
        for &b in &inst.chunk_bonds[m] {
            if inst.bond_broken[b as usize] {
                continue;
            }
            let spec = &inst.asset.bonds[b as usize];
            let o = if spec.a as usize == m { spec.b } else { spec.a } as usize;
            if inst.chunks[o] != ChunkState::Attached {
                continue;
            }
            if spec.a as usize == m {
                bonds.push(b);
            }
            if !seen[o] {
                seen[o] = true;
                members.push(o);
            }
        }
    }
    if members.len() < falling.min_section_chunks as usize {
        for m in members {
            inst.section_of[m] = m as u32;
        }
        return c as u32;
    }
    let section = members.iter().copied().min().unwrap_or(c) as u32;
    for &m in &members {
        inst.section_of[m] = section;
    }
    bonds.sort_unstable();
    inst.section_bonds.insert(section, bonds);
    section
}

/// The angular velocity a chunk detaches with: three draws from its event seed, up to `spin`.
fn detach_spin(seed: u64, spin: f32) -> [f32; 3] {
    let mut rng = seed;
    let mut unit = || {
        rng = splitmix(rng);
        // 24 random bits → [-1, 1), exact in f32.
        ((rng >> 40) as f32) * (2.0 / 16_777_216.0) - 1.0
    };
    [unit() * spin, unit() * spin, unit() * spin]
}

/// A validated instance with every chunk attached and every bond whole.
fn new_instance(
    asset: Arc<DestructibleAsset>,
    pose: DestructiblePose,
    seed: u64,
) -> Result<DestructibleInstance, DestructionError> {
    asset.validate()?;
    let n = asset.chunks.len();
    let mut chunk_bonds = vec![Vec::new(); n];
    for (i, b) in asset.bonds.iter().enumerate() {
        chunk_bonds[b.a as usize].push(i as u32);
        chunk_bonds[b.b as usize].push(i as u32);
    }
    let world_centroids = asset
        .chunks
        .iter()
        .map(|c| pose.transform_point(c.centroid))
        .collect();
    Ok(DestructibleInstance {
        pose,
        seed,
        chunks: vec![ChunkState::Attached; n],
        bond_damage: vec![0.0; asset.bonds.len()],
        bond_broken: vec![false; asset.bonds.len()],
        chunk_bonds,
        world_centroids,
        tick_impulse: vec![[0.0; 3]; n],
        tick_hit: vec![false; n],
        hot: false,
        load: asset.load.as_ref().map(|_| LoadState {
            doomed: vec![false; asset.bonds.len()],
            dirty: true,
            ..LoadState::default()
        }),
        section_of: if asset.falling.is_some() {
            vec![u32::MAX; n]
        } else {
            Vec::new()
        },
        section_bonds: BTreeMap::new(),
        asset,
    })
}

/// The chunk's own sound/VFX tag, or the material's.
fn chunk_fx_tag(asset: &DestructibleAsset, c: usize) -> String {
    asset
        .chunks
        .get(c)
        .and_then(|ch| ch.fx_tag.clone())
        .unwrap_or_else(|| asset.material.fx_tag.clone())
}

fn push_log(log: &mut VecDeque<BreakEvent>, event: BreakEvent) {
    if log.len() == EVENT_LOG_CAP {
        log.pop_front();
    }
    log.push_back(event);
}

/// The length of the pose's rotation, once the pose is known to be finite and not zero.
fn checked_rotation_length(pose: &DestructiblePose) -> Result<f32, DestructionError> {
    if !finite3(pose.translation) || !pose.rotation.iter().all(|c| c.is_finite()) {
        return Err(DestructionError::InvalidPose("not finite".into()));
    }
    let q = pose.rotation;
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if len <= 0.0 {
        return Err(DestructionError::InvalidPose("zero rotation".into()));
    }
    Ok(len)
}

/// The same pose, refused when it is not finite or its rotation is zero.
fn checked_pose(pose: DestructiblePose) -> Result<DestructiblePose, DestructionError> {
    checked_rotation_length(&pose)?;
    Ok(pose)
}

fn normalized_pose(pose: DestructiblePose) -> Result<DestructiblePose, DestructionError> {
    let len = checked_rotation_length(&pose)?;
    let q = pose.rotation;
    Ok(DestructiblePose {
        translation: pose.translation,
        rotation: [q[0] / len, q[1] / len, q[2] / len, q[3] / len],
    })
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014).
fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The seed of the event about `what` (a chunk, or `0x8000_0000 | bond`) on `tick`.
fn mix_seed(seed: u64, tick: u64, what: u64) -> u64 {
    splitmix(splitmix(seed ^ tick.wrapping_mul(0xA24B_AED4_963E_E407)) ^ what)
}

/// The event digest after `event`, from the digest before it: what
/// [`DestructionWorld::event_digest`] becomes when the event is decided or applied. A server uses
/// it to tell a replica the digest past events it does not send (ADR-2305).
#[must_use]
pub fn fold_event_digest(digest: u64, event: &BreakEvent) -> u64 {
    fold_event(digest, event)
}

/// An instance's state as a snapshot carries it.
fn instance_state(id: DestructibleId, inst: &DestructibleInstance) -> InstanceSnapshot {
    let attached = |c: u32| inst.chunks[c as usize] == ChunkState::Attached;
    InstanceSnapshot {
        id,
        pose: inst.pose,
        seed: inst.seed,
        detached: (0..inst.chunks.len() as u32).filter(|c| !attached(*c)).collect(),
        broken: inst
            .asset
            .bonds
            .iter()
            .enumerate()
            .filter(|(b, spec)| inst.bond_broken[*b] && attached(spec.a) && attached(spec.b))
            .map(|(b, _)| b as u32)
            .collect(),
    }
}

/// An instance rebuilt from its snapshot state.
fn restored_instance(
    s: &InstanceSnapshot,
    asset: Arc<DestructibleAsset>,
) -> Result<DestructibleInstance, DestructionError> {
    let bad = |what: String| DestructionError::BadSnapshot(what);
    let mut inst = new_instance(asset, checked_pose(s.pose)?, s.seed)?;
    for &b in &s.broken {
        *inst
            .bond_broken
            .get_mut(b as usize)
            .ok_or_else(|| bad(format!("instance {} has no bond {b}", s.id.0)))? = true;
    }
    for &c in &s.detached {
        *inst
            .chunks
            .get_mut(c as usize)
            .ok_or_else(|| bad(format!("instance {} has no chunk {c}", s.id.0)))? = ChunkState::Detached;
        for &b in &inst.chunk_bonds[c as usize] {
            inst.bond_broken[b as usize] = true;
        }
    }
    Ok(inst)
}

fn fold_event(digest: u64, e: &BreakEvent) -> u64 {
    let mut h = Fnv(digest ^ 0xCBF2_9CE4_8422_2325);
    h.u64(e.seq);
    h.u64(e.tick);
    h.u64(u64::from(e.destructible.0));
    h.u64(e.seed);
    match &e.kind {
        BreakKind::BondBroken { bond, a, b, position } => {
            h.u8(1);
            h.u64(u64::from(*bond));
            h.u64(u64::from(*a));
            h.u64(u64::from(*b));
            h.f3(*position);
        }
        BreakKind::ChunkDetached {
            chunk,
            cause,
            position,
            linvel,
            angvel,
        } => {
            h.u8(2);
            h.u64(u64::from(*chunk));
            h.u8(match cause {
                DetachCause::Damage => 1,
                DetachCause::Collapse => 2,
            });
            h.f3(*position);
            h.f3(*linvel);
            h.f3(*angvel);
        }
    }
    h.finish()
}

/// FNV-1a 64 (Fowler, Noll, Vo; public domain).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }
    fn u8(&mut self, v: u8) {
        self.0 ^= u64::from(v);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
    }
    fn u32(&mut self, v: u32) {
        for b in v.to_le_bytes() {
            self.u8(b);
        }
    }
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.u8(b);
        }
    }
    fn f3(&mut self, v: [f32; 3]) {
        for c in v {
            self.u32(c.to_bits());
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = length(a);
    if l > 0.0 { scale(a, 1.0 / l) } else { [0.0; 3] }
}

fn conjugate(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// `q * v * q⁻¹` for a unit quaternion: `v + w t + u × t` with `t = 2 u × v`.
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let t = scale(cross(u, v), 2.0);
    add(add(v, scale(t, q[3])), cross(u, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destruction::asset::{BoxGrid, DestructionMaterial, GridSupport};

    fn grid(counts: [u32; 3], support: GridSupport, strength: f32) -> Arc<DestructibleAsset> {
        Arc::new(
            DestructibleAsset::box_grid(&BoxGrid {
                name: "g".into(),
                counts,
                cell: [1.0, 1.0, 1.0],
                density: 100.0,
                strength_per_area: strength,
                support,
                material: DestructionMaterial::default(),
            })
            .expect("grid"),
        )
    }

    /// A server with two walls, one of them broken by a hit: what ADR-2305's replica API is for.
    fn two_walls() -> (
        DestructionWorld,
        DestructibleId,
        DestructibleId,
        Arc<DestructibleAsset>,
    ) {
        let asset = grid([4, 4, 1], GridSupport::Bottom, 50.0);
        let mut server = DestructionWorld::new(Authority::Server);
        let a = server
            .define(asset.clone(), DestructiblePose::at([0.0, 0.0, 0.0]), 1)
            .expect("a");
        let b = server
            .define(asset.clone(), DestructiblePose::at([10.0, 0.0, 0.0]), 2)
            .expect("b");
        server
            .damage(Damage {
                target: Some(b),
                kind: DamageKind::Explosion {
                    center: [11.5, 1.5, 0.0],
                    radius: 3.0,
                    impulse: 1.0e5,
                },
            })
            .expect("hit");
        for _ in 0..4 {
            server.step(1.0 / 60.0);
        }
        assert!(server.event_count() > 0);
        (server, a, b, asset)
    }

    #[test]
    fn a_filtered_hash_of_everything_is_the_structure_hash() {
        let (server, a, b, _) = two_walls();
        assert_eq!(server.structure_hash_where(|_| true), server.structure_hash());
        assert_ne!(server.structure_hash_where(|id| id == a), server.structure_hash());
        assert_ne!(
            server.structure_hash_where(|id| id == a),
            server.structure_hash_where(|id| id == b)
        );
    }

    #[test]
    fn a_replica_holding_one_instance_matches_the_servers_hash_of_it() {
        let (server, a, b, asset) = two_walls();
        // The replica holds only wall a: it skips every event (all of them broke b).
        let mut replica = DestructionWorld::new(Authority::Replica);
        let state = server.instance_snapshot(a).expect("a");
        replica
            .restore_instance(&state, asset.clone())
            .expect("restore a");
        replica
            .skip_events(server.event_count(), server.event_digest())
            .expect("skip");
        assert_eq!(
            replica.structure_hash(),
            server.structure_hash_where(|id| id == a)
        );
        // Then b becomes relevant: its state as the server has it.
        let state = server.instance_snapshot(b).expect("b");
        assert!(!state.detached.is_empty(), "the hit detached chunks");
        replica
            .restore_instance(&state, asset.clone())
            .expect("restore b");
        assert_eq!(replica.structure_hash(), server.structure_hash());
        assert_eq!(replica.instance_snapshot(b), server.instance_snapshot(b));
        // The id is taken now; a skip back is refused; a server refuses both.
        assert!(matches!(
            replica.restore_instance(&state, asset.clone()),
            Err(DestructionError::BadSnapshot(_))
        ));
        assert!(matches!(
            replica.skip_events(0, 0),
            Err(DestructionError::BadEvent { .. })
        ));
        let mut server = server;
        assert!(matches!(
            server.restore_instance(&state, asset),
            Err(DestructionError::NotReplica)
        ));
        assert!(matches!(
            server.skip_events(u64::MAX, 0),
            Err(DestructionError::NotReplica)
        ));
    }

    #[test]
    fn folding_the_events_gives_the_world_digest() {
        let (server, ..) = two_walls();
        let digest = server.recent_events().fold(0, fold_event_digest);
        assert!(server.event_count() as usize <= EVENT_LOG_CAP);
        assert_eq!(digest, server.event_digest());
    }

    #[test]
    fn rotate_matches_a_quarter_turn() {
        // 90 degrees about +Y maps +X to -Z.
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let r = rotate([0.0, h, 0.0, h], [1.0, 0.0, 0.0]);
        assert!(r[0].abs() < 1e-6 && (r[2] + 1.0).abs() < 1e-6, "{r:?}");
        let pose = DestructiblePose {
            translation: [1.0, 2.0, 3.0],
            rotation: [0.0, h, 0.0, h],
        };
        let p = pose.inverse_transform_point(pose.transform_point([0.3, -0.2, 0.7]));
        assert!(length(sub(p, [0.3, -0.2, 0.7])) < 1e-5, "{p:?}");
    }

    #[test]
    fn impulse_below_threshold_accumulates_then_breaks() {
        // Two cubes side by side, the left one held by the world; the bond holds 10 N s.
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([2, 1, 1], GridSupport::Explicit(vec![0]), 10.0),
                DestructiblePose::default(),
                7,
            )
            .expect("define");
        let hit = Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 1,
                impulse: [4.0, 0.0, 0.0],
            },
        };
        for _ in 0..2 {
            w.damage(hit.clone()).expect("queue");
            let r = w.step(1.0 / 60.0);
            assert!(r.events.is_empty(), "8 N s < 10 N s: {r:?}");
        }
        w.damage(hit).expect("queue");
        let r = w.step(1.0 / 60.0);
        assert_eq!(r.events.len(), 2, "{r:?}");
        assert!(matches!(r.events[0].kind, BreakKind::BondBroken { bond: 0, .. }));
        let BreakKind::ChunkDetached {
            chunk, cause, linvel, ..
        } = r.events[1].kind
        else {
            panic!("{r:?}");
        };
        assert_eq!((chunk, cause), (1, DetachCause::Damage));
        // 4 N s on a 100 kg chunk.
        assert!((linvel[0] - 0.04).abs() < 1e-6, "{linvel:?}");
        assert_eq!(w.take_spawns(0).len(), 1);
        assert_eq!(r.events[0].seq, 0);
        assert_eq!(r.events[1].seq, 1);
    }

    #[test]
    fn losing_support_collapses_the_rest() {
        // A column of 4 on one support: breaking the bottom bond drops the three above.
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([1, 4, 1], GridSupport::Bottom, 5.0),
                DestructiblePose::default(),
                1,
            )
            .expect("define");
        w.damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 0,
                impulse: [0.0, 0.0, 6.0],
            },
        })
        .expect("queue");
        let r = w.step(0.0);
        let detached: Vec<(u32, DetachCause)> = r
            .events
            .iter()
            .filter_map(|e| match e.kind {
                BreakKind::ChunkDetached { chunk, cause, .. } => Some((chunk, cause)),
                BreakKind::BondBroken { .. } => None,
            })
            .collect();
        assert_eq!(
            detached,
            vec![
                (1, DetachCause::Collapse),
                (2, DetachCause::Collapse),
                (3, DetachCause::Collapse)
            ]
        );
        let inst = w.instance(id).expect("inst");
        assert_eq!(inst.chunk_states()[0], ChunkState::Attached, "the support stays");
        assert!(r.fx.iter().filter(|f| f.kind == FxKind::Collapse).count() == 3);
    }

    #[test]
    fn free_object_keeps_its_largest_piece() {
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([3, 1, 1], GridSupport::None, 5.0),
                DestructiblePose::default(),
                1,
            )
            .expect("define");
        // Chunk 2 has one bond: 6 N s breaks it; chunks 0-1 stay as the larger piece.
        w.damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 2,
                impulse: [6.0, 0.0, 0.0],
            },
        })
        .expect("queue");
        w.step(0.0);
        let inst = w.instance(id).expect("inst");
        assert_eq!(
            inst.chunk_states(),
            &[ChunkState::Attached, ChunkState::Attached, ChunkState::Detached]
        );
    }

    #[test]
    fn break_cap_defers_to_later_ticks_in_order() {
        let mut w = DestructionWorld::new(Authority::Server);
        w.set_budget(DestructionBudget {
            max_breaks_per_tick: 2,
            ..DestructionBudget::default()
        });
        let id = w
            .define(
                grid([6, 1, 1], GridSupport::None, 1.0),
                DestructiblePose::default(),
                1,
            )
            .expect("define");
        w.damage(Damage {
            target: Some(id),
            kind: DamageKind::Explosion {
                center: [0.0, 0.5, 0.0],
                radius: 100.0,
                impulse: 100.0,
            },
        })
        .expect("queue");
        let mut bonds = Vec::new();
        let mut deferred = Vec::new();
        for _ in 0..4 {
            let r = w.step(0.0);
            deferred.push(r.deferred_breaks);
            bonds.extend(r.events.iter().filter_map(|e| match e.kind {
                BreakKind::BondBroken { bond, .. } => Some(bond),
                BreakKind::ChunkDetached { .. } => None,
            }));
        }
        // Detaching a chunk breaks its other bonds silently, so fewer than 5 bond events may
        // show; the ones that do are in ascending order within each tick and capped at 2.
        assert_eq!(deferred[0], 3, "{deferred:?}");
        assert!(bonds.len() >= 2, "{bonds:?}");
        assert_eq!(w.instance(id).expect("i").attached_count(), 1, "one piece left");
    }

    #[test]
    fn projectile_hits_the_nearest_chunk_and_misses_report() {
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([3, 1, 1], GridSupport::None, 1.0),
                DestructiblePose::at([0.0, 0.0, 0.0]),
                1,
            )
            .expect("define");
        // From +X looking -X: the first chunk entered is chunk 2 (x in [0.5, 1.5]).
        w.damage(Damage {
            target: None,
            kind: DamageKind::Projectile {
                origin: [10.0, 0.5, 0.0],
                direction: [-1.0, 0.0, 0.0],
                impulse: 3.0,
                radius: 0.0,
                max_distance: 100.0,
            },
        })
        .expect("queue");
        w.damage(Damage {
            target: None,
            kind: DamageKind::Projectile {
                origin: [10.0, 50.0, 0.0],
                direction: [-1.0, 0.0, 0.0],
                impulse: 3.0,
                radius: 0.0,
                max_distance: 100.0,
            },
        })
        .expect("queue");
        let r = w.step(0.0);
        assert_eq!((r.hits, r.misses), (1, 1));
        let inst = w.instance(id).expect("inst");
        assert_eq!(inst.chunk_states()[2], ChunkState::Detached);
        assert_eq!(inst.chunk_states()[1], ChunkState::Attached);
    }

    #[test]
    fn replica_reaches_the_server_structure_hash() {
        let asset = grid([4, 4, 1], GridSupport::Bottom, 20.0);
        let mut server = DestructionWorld::new(Authority::Server);
        let mut client = DestructionWorld::new(Authority::Replica);
        let s = server
            .define(asset.clone(), DestructiblePose::at([0.0, 0.0, 5.0]), 42)
            .expect("s");
        let c = client
            .define(asset, DestructiblePose::at([0.0, 0.0, 5.0]), 42)
            .expect("c");
        assert_eq!(s, c);
        assert_eq!(server.structure_hash(), client.structure_hash());
        server
            .damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.0, 2.0, 5.5],
                    radius: 1.5,
                    impulse: 200.0,
                },
            })
            .expect("queue");
        let r = server.step(1.0 / 60.0);
        assert!(!r.events.is_empty());
        assert_ne!(server.structure_hash(), client.structure_hash());
        assert_eq!(
            client.damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.0; 3],
                    radius: 1.0,
                    impulse: 1.0,
                },
            }),
            Err(DestructionError::Replica)
        );
        let applied = client.apply_events(&r.events).expect("apply");
        assert_eq!(applied.events, r.events);
        assert_eq!(server.structure_hash(), client.structure_hash());
        assert_eq!(server.pending_spawns(), client.pending_spawns());
        assert_eq!(server.take_spawns(0), client.take_spawns(0));
        // Out of order: replaying the same events again fails and changes nothing.
        let before = client.structure_hash();
        let err = client.apply_events(&r.events).expect_err("again");
        assert_eq!(err.code(), "destruction_bad_event");
        assert_eq!(client.structure_hash(), before);
        assert_eq!(
            server.apply_events(&[]).expect_err("server").code(),
            "destruction_not_replica"
        );
    }

    #[test]
    fn set_pose_moves_where_hits_land() {
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([2, 1, 1], GridSupport::Explicit(vec![0]), 1.0),
                DestructiblePose::default(),
                1,
            )
            .expect("define");
        let far = Damage {
            target: None,
            kind: DamageKind::Explosion {
                center: [0.5, 10.5, 0.0],
                radius: 0.6,
                impulse: 10.0,
            },
        };
        // Before the move the blast at y = 10.5 reaches nothing.
        w.damage(far.clone()).expect("queue");
        assert_eq!(w.step(0.0).misses, 1);
        w.set_pose(id, DestructiblePose::at([0.0, 10.0, 0.0]))
            .expect("move");
        assert_eq!(
            w.instance(id).and_then(|i| i.world_centroid(1)),
            Some([0.5, 10.5, 0.0])
        );
        w.damage(far).expect("queue");
        let r = w.step(0.0);
        assert_eq!(r.hits, 1);
        let detached = r.events.iter().find_map(|e| match e.kind {
            BreakKind::ChunkDetached { chunk, position, .. } => Some((chunk, position)),
            BreakKind::BondBroken { .. } => None,
        });
        assert_eq!(detached, Some((1, [0.5, 10.5, 0.0])), "{r:?}");
        assert_eq!(
            w.set_pose(DestructibleId(9), DestructiblePose::default())
                .expect_err("id")
                .code(),
            "destruction_unknown_id"
        );
        let zero = DestructiblePose {
            translation: [0.0; 3],
            rotation: [0.0; 4],
        };
        assert_eq!(
            w.set_pose(id, zero).expect_err("pose").code(),
            "destruction_invalid_pose"
        );
    }

    #[test]
    fn decay_heals_unbroken_bonds() {
        let mut asset = (*grid([2, 1, 1], GridSupport::Explicit(vec![0]), 10.0)).clone();
        asset.material.decay_per_second = 2.0;
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(Arc::new(asset), DestructiblePose::default(), 1)
            .expect("define");
        w.damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 1,
                impulse: [5.0, 0.0, 0.0],
            },
        })
        .expect("queue");
        w.step(0.5);
        assert_eq!(w.instance(id).expect("i").bond_damage()[0], 5.0);
        // 2 N s per second heals 1 N s per half-second tick.
        w.step(0.5);
        assert_eq!(w.instance(id).expect("i").bond_damage()[0], 4.0);
    }

    #[test]
    fn invalid_inputs_have_codes() {
        let mut w = DestructionWorld::new(Authority::Server);
        let id = w
            .define(
                grid([1, 1, 1], GridSupport::Bottom, 1.0),
                DestructiblePose::default(),
                1,
            )
            .expect("define");
        let e = w
            .damage(Damage {
                target: Some(DestructibleId(99)),
                kind: DamageKind::Explosion {
                    center: [0.0; 3],
                    radius: 1.0,
                    impulse: 1.0,
                },
            })
            .expect_err("unknown");
        assert_eq!(e.code(), "destruction_unknown_id");
        let e = w
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Impulse {
                    chunk: 9,
                    impulse: [1.0; 3],
                },
            })
            .expect_err("chunk");
        assert_eq!(e.code(), "destruction_unknown_chunk");
        let e = w
            .damage(Damage {
                target: None,
                kind: DamageKind::Explosion {
                    center: [0.0; 3],
                    radius: 0.0,
                    impulse: 1.0,
                },
            })
            .expect_err("radius");
        assert_eq!(e.code(), "destruction_invalid_damage");
        let e = w
            .define(
                grid([1, 1, 1], GridSupport::Bottom, 1.0),
                DestructiblePose {
                    translation: [0.0; 3],
                    rotation: [0.0; 4],
                },
                1,
            )
            .expect_err("pose");
        assert_eq!(e.code(), "destruction_invalid_pose");
        for code in [
            "destruction_unknown_id",
            "destruction_unknown_chunk",
            "destruction_invalid_damage",
            "destruction_replica",
            "destruction_not_replica",
            "destruction_bad_event",
            "destruction_invalid_pose",
        ] {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    fn warmed_step_allocs(world: &mut DestructionWorld) -> usize {
        let _ = world.step(1.0 / 60.0);
        let before = crate::alloc_count::allocations();
        let _ = world.step(1.0 / 60.0);
        crate::alloc_count::allocations() - before
    }

    #[test]
    fn cold_step_does_not_allocate_instance_ids() {
        let mut empty = DestructionWorld::new(Authority::Server);
        let empty_cost = warmed_step_allocs(&mut empty);

        let asset = grid([2, 2, 1], GridSupport::Bottom, 1.0e6);
        let mut world = DestructionWorld::new(Authority::Server);
        world
            .define(asset.clone(), DestructiblePose::at([0.0, 0.0, 0.0]), 1)
            .expect("a");
        world
            .define(asset, DestructiblePose::at([10.0, 0.0, 0.0]), 2)
            .expect("b");
        let _ = world.step(1.0 / 60.0);
        let hash = world.structure_hash();
        let before = crate::alloc_count::allocations();
        let _ = world.step(1.0 / 60.0);
        let cost = crate::alloc_count::allocations() - before;
        assert_eq!(
            world.structure_hash(),
            hash,
            "a cold step must not change structure"
        );
        assert_eq!(
            cost, empty_cost,
            "populated cold step allocated {cost}, empty world allocated {empty_cost}"
        );
        eprintln!("destruction_cold_step_allocs {cost} empty {empty_cost}");
    }
}
