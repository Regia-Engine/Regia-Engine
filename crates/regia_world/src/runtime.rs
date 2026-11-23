// SPDX-License-Identifier: MPL-2.0
//! The streaming scheduler: per-level cell states, the plan-to-IO-to-apply pipeline, budgets,
//! failure and retry, pins and the trace (spec 78 §§ 7-10, 15).
//!
//! Everything here is plain methods on [`WorldStreaming`]; `systems` calls them at their points in
//! the frame. Decisions are pure functions of the cell registry, the loaded set, the source views and
//! the settings. Only completion times vary with the disk, and `StreamTiming::Lockstep` pins those.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use regia_asset::AssetId;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::floating_origin::{FloatingOrigin, LargePosition};
use regia_scene::level::LevelId;
use regia_scene::spawn::SpawnOptions;
use regia_scene::stream_grid::{CellCoord, StreamError, StreamGrid};
use regia_scene::stream_plan::{
    CellInfo, DeferReason, LoadOrder, SourceId, SourceView, StreamPlan, StreamPlanner,
};
use regia_scene::streaming::{
    CellFailed, CellLoaded, CellMember, CellReady, CellUnloading, LevelStreaming, Readiness, StreamBudgets,
    StreamGates, StreamTuning,
};
use thiserror::Error;

use crate::format::{CellBlob, CellRecord, WorldFormatError, WorldIndex};
use crate::source::{CancelToken, CellOutcome, CellResult, CellSink, CellSource, Ticket};

/// The most sources and pins at once (spec 78 § 7.2), the limit of the stateless planner method too.
pub const MAX_STREAM_SOURCES: usize = 64;

/// The events `stream.trace` keeps (spec 78 § 17).
pub const TRACE_CAPACITY: usize = 4096;

/// Steps measured memory must stay over budget before the async budget adjustment (spec 78 § 8.5).
const OVER_BUDGET_STEPS: u32 = 60;

/// Where a cell is in its life (spec 78 § 8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CellState {
    /// Nothing in memory.
    Unloaded,
    /// Wanted, waiting for an IO slot.
    Queued,
    /// Read and decode in flight.
    Loading,
    /// Decoded; entities being spawned over one or more frames.
    Applying,
    /// All entities exist; some simulation gates are not ready.
    Spawned,
    /// Every simulation gate is ready.
    SimReady,
    /// Every render gate is ready too.
    Visible,
    /// Despawn in progress.
    Unloading,
    /// The load failed, with this reason.
    Failed(String),
}

impl CellState {
    /// The agent spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Queued => "queued",
            Self::Loading => "loading",
            Self::Applying => "applying",
            Self::Spawned => "spawned",
            Self::SimReady => "sim_ready",
            Self::Visible => "visible",
            Self::Unloading => "unloading",
            Self::Failed(_) => "failed",
        }
    }

    /// All entities exist.
    pub fn is_spawned(&self) -> bool {
        matches!(self, Self::Spawned | Self::SimReady | Self::Visible)
    }

    /// Whether the cell reached `readiness`.
    pub fn reached(&self, readiness: Readiness) -> bool {
        match readiness {
            Readiness::SimReady => matches!(self, Self::SimReady | Self::Visible),
            Readiness::Visible => matches!(self, Self::Visible),
        }
    }
}

/// How completion times are decided (spec 78 § 15.2).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum StreamTiming {
    /// Completions apply when they arrive. For players.
    #[default]
    Async,
    /// A load issued at step N applies at step N + `latency_steps`, in issue order, and the main
    /// thread waits for late IO up to the cap. The world after each step is a pure function of the
    /// inputs. For tests, replays and lockstep games.
    Lockstep {
        /// Steps from issuing a load to applying it.
        latency_steps: u32,
    },
    /// Replays an async run: every decision and every apply happen at the steps of the trace.
    Replay(StreamTrace),
}

impl StreamTiming {
    /// The agent spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Async => "async",
            Self::Lockstep { .. } => "lockstep",
            Self::Replay(_) => "replay",
        }
    }

    /// Whether `Complete` and `Apply` run inside the fixed step.
    pub fn in_fixed_step(&self) -> bool {
        !matches!(self, Self::Async)
    }
}

/// What happened to a cell (spec 78 § 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceKind {
    /// Entered the IO queue.
    Queued,
    /// Its read was issued.
    Loading,
    /// All its entities spawned.
    Applied,
    /// Every simulation gate became ready.
    SimReady,
    /// Every render gate became ready.
    Visible,
    /// It started unloading.
    Unloading,
    /// It finished unloading.
    Unloaded,
    /// Its load was cancelled.
    Cancelled,
    /// Its load failed.
    Failed,
    /// The origin moved.
    Rebase,
}

impl TraceKind {
    /// The agent spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Loading => "loading",
            Self::Applied => "applied",
            Self::SimReady => "sim_ready",
            Self::Visible => "visible",
            Self::Unloading => "unloading",
            Self::Unloaded => "unloaded",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Rebase => "rebase",
        }
    }

    /// Whether this event is presentation, outside the determinism hash (spec 78 § 15.1).
    fn is_presentation(self) -> bool {
        matches!(self, Self::Visible)
    }

    /// Whether this event shows the simulated world changed, the part an async run and its replay
    /// share. `Loading` and retries move with IO timing there.
    fn changes_world(self) -> bool {
        matches!(self, Self::Applied | Self::Unloaded | Self::Rebase)
    }
}

/// One entry of the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEvent {
    /// The step it happened on.
    pub step: u64,
    /// The level.
    pub level: Option<LevelId>,
    /// The cell, unless the event is world-wide.
    pub cell: Option<CellCoord>,
    /// What happened.
    pub kind: TraceKind,
    /// More detail: a failure reason, a rebase's cell delta.
    pub detail: Option<String>,
}

/// A recorded run's events, oldest first, for [`StreamTiming::Replay`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamTrace {
    /// The events.
    pub events: Vec<TraceEvent>,
}

/// A pin's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PinId(pub u64);

/// What a pin waits for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinWait {
    /// Radius around the pin that must be ready, meters.
    pub radius_m: f64,
    /// How ready.
    pub readiness: Readiness,
}

/// A scripted source with no entity (spec 78 § 7.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// The pin's id.
    pub id: PinId,
    /// A name for humans and agents.
    pub name: String,
    /// World position, meters.
    pub center: [f64; 3],
    /// Divides distances.
    pub radius_scale: f32,
    /// Added to the priority of every cell it wants.
    pub priority: i32,
    /// Remaining steps; `None` keeps it until `unpin`.
    pub ttl_steps: Option<u32>,
    /// A wait that counts in `StreamStats::pending_waits` until met.
    pub wait: Option<PinWait>,
}

/// What the runtime counts (spec 78 § 16). A resource too, so `play.wait_until` and `play.expect`
/// predicates can read it.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default, Clone)]
#[non_exhaustive]
pub struct StreamStats {
    /// Pins with a wait that is not met yet, plus one for each level still in its open wait (spec 78
    /// § 8.7: the start cells are not `sim_ready` yet).
    pub pending_waits: u32,
    /// Cells from `Queued` to `Visible`, plus `Unloading`.
    pub resident_cells: u32,
    /// The planner's bytes: cook estimates of the same cells.
    pub resident_bytes_estimate: u64,
    /// Measured bytes of the decoded and spawned cells.
    pub resident_bytes_measured: u64,
    /// The cook's VRAM estimate of the cells spawned now (spec 78 § 8.4). It is reported, in this
    /// counter and in the `stream_cells` load set of `render.memory_budget`; the planner's hard bound
    /// is RAM.
    pub resident_vram_estimate: u64,
    /// [`Self::resident_vram_estimate`] is above the level's `vram_budget_bytes`.
    pub vram_over_budget: bool,
    /// Reads issued.
    pub loads_issued: u64,
    /// Loads cancelled in `Queued`, `Loading` or `Applying`.
    pub loads_cancelled: u64,
    /// Results dropped because their generation was stale.
    pub stale_discards: u64,
    /// Failed loads.
    pub failures: u64,
    /// HLOD leaves nothing covers in the last frame's cut (a gauge: 0 once the cut is covered).
    pub holes: u64,
    /// Steps a source's own body spent frozen.
    pub source_frozen_steps: u64,
    /// Times the main thread waited on IO (lockstep and replay only; 0 in async mode).
    pub main_thread_io_waits: u64,
    /// Hierarchies larger than the apply budget, spawned alone.
    pub apply_over_budget: u64,
    /// Rebases.
    pub rebases: u64,
    /// Rebases not taken because a `RebaseBlockers` entry was registered (ADR-2022).
    pub rebases_blocked: u64,
    /// The last apply run's time, milliseconds.
    pub last_apply_ms: f32,
    /// The last plan's time, milliseconds.
    pub last_plan_ms: f32,
    /// Entities spawned or despawned by the last apply run.
    pub last_apply_entities: u32,
    /// Cells in flight, now.
    pub inflight: u32,
    /// Decoded and in-flight bytes waiting for apply, now.
    pub pending_bytes: u64,
    /// Bodies frozen by the guard, now.
    pub frozen_bodies: u32,
    /// Teleports seen (a source moved more than twice the unload radius in one step).
    pub teleports: u64,
    /// Lockstep waits that hit the cap; such a run's determinism is void.
    pub lockstep_stalls: u64,
    /// Sources dropped because more than [`MAX_STREAM_SOURCES`] were enabled.
    pub sources_dropped: u64,
    /// Plans run.
    pub plans: u64,
    /// The current fixed step.
    pub step: u64,
    /// Rolling hash of every trace event except presentation (spec 78 § 15.1).
    pub trace_hash: u64,
    /// Rolling hash of the world-changing events, what an async run and its replay share.
    pub world_hash: u64,
    /// Cells the planner looked at over every plan: each plan walks the level's registered cells
    /// once (the loaded ones for release, the rest as load candidates).
    pub plan_cells_evaluated: u64,
    /// The most cells one plan looked at.
    pub peak_plan_cells: u32,
    /// The most cells in flight at the end of any step.
    pub peak_inflight: u32,
    /// The most decoded bytes waiting for apply at the end of any step.
    pub peak_pending_bytes: u64,
    /// The most entities one apply run spawned or despawned.
    pub peak_apply_entities: u32,
    /// The most reads issued in one step.
    pub peak_loads_per_step: u32,
}

/// Sets a level's lockstep wait cap, for tests that need a stall quickly.
pub fn set_wait_cap_for_tests(streaming: &mut WorldStreaming, level: LevelId, ms: u32) {
    if let Some(rt) = streaming.levels.get_mut(&level) {
        rt.tuning.lockstep_wait_cap_ms = ms;
    }
}

/// Most cells a pin wait's area may span in one level's grid (its bounding box): the cap the
/// wait scan lists cells with, so a wait [`WorldStreaming::set_pin_wait`] accepts can be scanned.
const PIN_WAIT_MAX_CELLS: u64 = 1 << 22;

/// Errors of the runtime API. [`Self::reason`] is the agent's `data.reason`.
#[derive(Debug, Error, PartialEq)]
pub enum StreamRuntimeError {
    /// No streamed level is open.
    #[error("no streamed level is open")]
    NotEnabled,
    /// The cell has no record in the level.
    #[error("cell {0} is not part of the world")]
    UnknownCell(CellCoord),
    /// No such pin.
    #[error("no pin {0}")]
    UnknownPin(u64),
    /// More than [`MAX_STREAM_SOURCES`] sources and pins.
    #[error("at most {MAX_STREAM_SOURCES} sources and pins")]
    SourceLimit,
    /// Budgets that cannot run.
    #[error("bad budget: {0}")]
    BadBudget(String),
    /// A world index newer than this build.
    #[error("world index format version {found} is newer than {supported}")]
    FormatNewer {
        /// Version in the file.
        found: u32,
        /// Highest version this build reads.
        supported: u32,
    },
    /// A world index that cannot run.
    #[error("bad world index: {0}")]
    BadIndex(String),
    /// A position that is not finite.
    #[error("{0} is not finite")]
    NonFinite(&'static str),
    /// A pin wait over an area an open level's grid refuses to list (its bounding box passes
    /// the `PIN_WAIT_MAX_CELLS` cap, or it leaves the grid's coordinate range): it could never be
    /// met.
    #[error("a wait of radius {radius_m} m around the pin is an area the grid cannot list: {detail}")]
    WaitAreaRefused {
        /// The refused radius, meters.
        radius_m: f64,
        /// The grid's reason.
        detail: String,
    },
}

impl StreamRuntimeError {
    /// The stable code (spec 78 § 17).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotEnabled => "stream_not_enabled",
            Self::UnknownCell(_) => "stream_unknown_cell",
            Self::UnknownPin(_) => "stream_unknown_pin",
            Self::SourceLimit => "stream_source_limit",
            Self::BadBudget(_) => "stream_bad_budget",
            Self::FormatNewer { .. } => "stream_format_newer",
            Self::BadIndex(_) | Self::NonFinite(_) => "stream_settings_invalid",
            Self::WaitAreaRefused { .. } => "stream_wait_area_refused",
        }
    }
}

impl From<WorldFormatError> for StreamRuntimeError {
    fn from(e: WorldFormatError) -> Self {
        match e {
            WorldFormatError::Newer { found, supported } => Self::FormatNewer { found, supported },
            other => Self::BadIndex(other.to_string()),
        }
    }
}

impl From<StreamError> for StreamRuntimeError {
    fn from(e: StreamError) -> Self {
        Self::BadIndex(e.to_string())
    }
}

/// The planner's VRAM bound for `budgets`: none while the bound is off or the budget is 0.
fn vram_bound_of(on: bool, budgets: &StreamBudgets) -> Option<u64> {
    (on && budgets.vram_budget_bytes > 0).then_some(budgets.vram_budget_bytes)
}

/// The name of a level's load set in `regia_asset::LoadSets`, which `render.memory_budget` reports
/// (spec 78 § 8.4).
///
/// ```
/// use regia_core::id::Guid;
/// use regia_scene::level::LevelId;
/// use regia_world::stream_cells_set;
///
/// let name = stream_cells_set(LevelId(Guid::from_u128(5)));
/// assert!(name.starts_with("stream_cells:"));
/// ```
#[must_use]
pub fn stream_cells_set(level: LevelId) -> String {
    format!("stream_cells:{}", level.0)
}

/// One cell's runtime record.
#[derive(Debug)]
struct CellRt {
    /// Marked resident in the level's `stream_cells` load set.
    in_load_set: bool,
    state: CellState,
    generation: u64,
    cancel: Option<CancelToken>,
    blob: Option<Arc<CellBlob>>,
    /// Hierarchy ranges of `blob` and the next one to spawn.
    ranges: Vec<Range<usize>>,
    next_range: usize,
    /// Spawned roots with their hierarchy's entity count.
    roots: Vec<(Entity, u32)>,
    /// Index of `roots`, so a move is a swap-remove.
    root_index: HashMap<Entity, usize>,
    /// Every spawned entity, for `CellLoaded`.
    entities: Vec<Entity>,
    /// Index of `entities`, so a move is a swap-remove.
    entity_index: HashMap<Entity, usize>,
    /// Failed loads since the cell was last unwanted.
    failures: u32,
    retry_step: Option<u64>,
    /// Decoded file bytes.
    measured: u64,
    /// Generations of this cell's reads whose bytes are still reserved in the level's
    /// `pending_bytes`: pushed when a read issues, removed when that read's reservation is
    /// released. A replay that lands a recorded failure while the read is in flight releases it
    /// then (the recording released it at that step), and the cancelled read's late result must
    /// not release it again, nor the retry's (fixrev-23, integ-v048b ADR-5495, ported to this
    /// runtime by integ-v048).
    reserved_reads: Vec<u64>,
}

impl CellRt {
    /// Releases the reservation of the read with `generation`, if it still holds one.
    fn take_reservation(&mut self, generation: u64) -> bool {
        match self.reserved_reads.iter().position(|g| *g == generation) {
            Some(i) => {
                self.reserved_reads.swap_remove(i);
                true
            }
            None => false,
        }
    }
}

impl Default for CellRt {
    fn default() -> Self {
        Self {
            in_load_set: false,
            state: CellState::Unloaded,
            generation: 0,
            cancel: None,
            blob: None,
            ranges: Vec::new(),
            next_range: 0,
            roots: Vec::new(),
            root_index: HashMap::new(),
            entities: Vec::new(),
            entity_index: HashMap::new(),
            failures: 0,
            retry_step: None,
            measured: 0,
            reserved_reads: Vec::new(),
        }
    }
}

impl CellRt {
    fn push_root(&mut self, entity: Entity, count: u32) {
        if self.root_index.contains_key(&entity) {
            return;
        }
        let at = self.roots.len();
        self.roots.push((entity, count));
        self.root_index.insert(entity, at);
    }

    fn pop_root(&mut self) -> Option<(Entity, u32)> {
        let (entity, count) = self.roots.pop()?;
        self.root_index.remove(&entity);
        Some((entity, count))
    }

    /// Removes `entity` by index. Returns its hierarchy count.
    fn take_root(&mut self, entity: Entity) -> Option<u32> {
        let at = self.root_index.remove(&entity)?;
        if self.roots.get(at).is_none_or(|(stored, _)| *stored != entity) {
            return None;
        }
        let (_, count) = self.roots.swap_remove(at);
        if let Some((moved, _)) = self.roots.get(at) {
            self.root_index.insert(*moved, at);
        }
        Some(count)
    }

    fn push_entity(&mut self, entity: Entity) {
        if self.entity_index.contains_key(&entity) {
            return;
        }
        let at = self.entities.len();
        self.entities.push(entity);
        self.entity_index.insert(entity, at);
    }

    fn extend_entities(&mut self, entities: impl IntoIterator<Item = Entity>) {
        for entity in entities {
            self.push_entity(entity);
        }
    }

    fn take_entity(&mut self, entity: Entity) {
        let Some(at) = self.entity_index.remove(&entity) else {
            return;
        };
        if self.entities.get(at).is_none_or(|stored| *stored != entity) {
            return;
        }
        self.entities.swap_remove(at);
        if let Some(moved) = self.entities.get(at) {
            self.entity_index.insert(*moved, at);
        }
    }

    fn clear_entities(&mut self) {
        self.entities.clear();
        self.entity_index.clear();
    }
}

/// The part of [`CellState`] the running totals need. `Failed`'s reason is not copied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Unloaded,
    Queued,
    Loading,
    Applying,
    Spawned,
    SimReady,
    Visible,
    Unloading,
    Failed,
}

impl CellKind {
    fn of(state: &CellState) -> Self {
        match state {
            CellState::Unloaded => Self::Unloaded,
            CellState::Queued => Self::Queued,
            CellState::Loading => Self::Loading,
            CellState::Applying => Self::Applying,
            CellState::Spawned => Self::Spawned,
            CellState::SimReady => Self::SimReady,
            CellState::Visible => Self::Visible,
            CellState::Unloading => Self::Unloading,
            CellState::Failed(_) => Self::Failed,
        }
    }

    fn resident(self) -> bool {
        !matches!(self, Self::Unloaded | Self::Failed)
    }

    fn spawned(self) -> bool {
        matches!(self, Self::Spawned | Self::SimReady | Self::Visible)
    }

    fn failed(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// Measured bytes and state kind before a cell mutation.
#[derive(Clone, Copy)]
struct CellSnap {
    kind: CellKind,
    measured: u64,
}

impl CellSnap {
    fn of(rt: &CellRt) -> Self {
        Self {
            kind: CellKind::of(&rt.state),
            measured: rt.measured,
        }
    }
}

/// A read the lockstep order waits for.
#[derive(Debug, Clone, Copy)]
struct Issued {
    ticket: Ticket,
    due_step: u64,
    /// It already hit the wait cap; later steps only look, they do not wait again.
    stalled: bool,
}

/// One open streamed level.
struct LevelRt {
    id: LevelId,
    /// Where this level's cell blobs come from.
    source: Arc<dyn CellSource>,
    index: Arc<WorldIndex>,
    records: BTreeMap<CellCoord, usize>,
    grid: StreamGrid,
    planner: StreamPlanner,
    cells: BTreeMap<CellCoord, CellRt>,
    /// Plan order; the head is issued first.
    queue: VecDeque<CellCoord>,
    /// Decoded cells in the order they are spawned.
    apply: VecDeque<CellCoord>,
    /// The level's `stream_cells` load set is defined.
    load_set_defined: bool,
    /// Cells whose `CellUnloading` goes out on the next apply run.
    announce: Vec<CellCoord>,
    /// Announced cells, despawning.
    despawn: VecDeque<CellCoord>,
    budgets: StreamBudgets,
    tuning: StreamTuning,
    inflight: u32,
    pending_bytes: u64,
    /// Steps measured memory stayed over budget.
    over_budget_steps: u32,
    /// The planner's budget right now, when lower than `budgets`.
    effective_budget: u64,
    /// The source points at the last plan.
    planned_points: BTreeMap<SourceId, [f64; 3]>,
    plan_due: bool,
    last_plan_step: u64,
    readiness_dirty: bool,
    /// The gates' revision the states were last compared with.
    gate_revision: u64,
    /// The cells around the start sources are not ready yet (`wait_on_open`).
    open_wait: bool,
    /// The last generation given out before this level opened.
    opened_at_generation: u64,
    /// Registered cells [`regia_scene::stream_plan::StreamPlanner::plan_views`] has walked.
    plan_cell_visits: u64,
    /// Cell records touched by the per-step maintenance walks.
    cell_record_visits: u64,
    /// Root-list entries the guard has examined in this level.
    guard_root_visits: u64,
    /// Cells in `Failed`, so a quiet step does not scan the registry for them.
    failed_cells: BTreeSet<CellCoord>,
    /// Cells in `Spawned`, `SimReady` or `Visible`.
    spawned_cells: BTreeSet<CellCoord>,
    /// Cells in `Visible`. The HLOD cut cache hashes this when a proxy is missing.
    visible_cells: BTreeSet<CellCoord>,
    /// Cells that are not `Unloaded` or `Failed`.
    resident_cells: u32,
    /// Sum of each cell's measured bytes.
    measured_total: u64,
    /// Cook VRAM of `spawned_cells`.
    vram_total: u64,
    /// A cell's state changed since the last stats refresh.
    stats_changed: bool,
    /// Cells whose spawned-ness changed. The load set updates only these rows.
    load_set_queue: Vec<CellCoord>,
    /// Cells the last full plan deferred because of the load cap, in that plan's order.
    rate_tail: VecDeque<CellCoord>,
    /// `rate_tail` is still the next plan while the view stays inside the movement threshold.
    rate_limited: bool,
}

/// Messages waiting for world access.
#[derive(Debug)]
enum Outgoing {
    Loaded(CellLoaded),
    Ready(CellReady),
    Unloading(CellUnloading),
    Failed(CellFailed),
}

/// Stats and trace, split from the levels so both can be borrowed at once.
#[derive(Default)]
struct Log {
    stats: StreamStats,
    trace: VecDeque<TraceEvent>,
    out: Vec<Outgoing>,
    /// The last generation given out. Process-wide, so a read of a closed level can never match a
    /// new load of the same cell after the level reopens.
    generation: u64,
    /// `loads_issued` at the end of the previous step, to count the reads of one step.
    loads_mark: u64,
}

/// FNV-1a over `bytes`, continuing from `hash`.
fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    if hash == 0 {
        hash = 0xcbf2_9ce4_8422_2325;
    }
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl Log {
    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn event(
        &mut self,
        level: Option<LevelId>,
        cell: Option<CellCoord>,
        kind: TraceKind,
        detail: Option<String>,
    ) {
        let step = self.stats.step;
        let mix = |hash: u64| {
            let mut hash = fnv(hash, &step.to_le_bytes());
            if let Some(level) = level {
                hash = fnv(hash, &level.0.as_u128().to_le_bytes());
            }
            if let Some(cell) = cell {
                hash = fnv(hash, &cell.x.to_le_bytes());
                hash = fnv(hash, &cell.z.to_le_bytes());
            }
            fnv(hash, kind.as_str().as_bytes())
        };
        if !kind.is_presentation() {
            self.stats.trace_hash = mix(self.stats.trace_hash);
        }
        if kind.changes_world() {
            // A sum: an async run can order two events of one step differently from its replay.
            // The detail carries what changed (a rebase's cell delta): without it two runs that
            // rebase on the same step by different cells would collide.
            let mut world_mix = mix(0);
            if let Some(detail) = &detail {
                world_mix = fnv(world_mix, detail.as_bytes());
            }
            self.stats.world_hash = self.stats.world_hash.wrapping_add(world_mix);
        }
        if self.trace.len() >= TRACE_CAPACITY {
            self.trace.pop_front();
        }
        self.trace.push_back(TraceEvent {
            step,
            level,
            cell,
            kind,
            detail,
        });
    }
}

/// The runtime: one record per open streamed level, the pins, the timing mode and the counters.
#[derive(Resource)]
pub struct WorldStreaming {
    levels: BTreeMap<LevelId, LevelRt>,
    pins: BTreeMap<PinId, Pin>,
    next_pin: u64,
    timing: StreamTiming,
    tx: Sender<CellResult>,
    rx: Receiver<CellResult>,
    log: Log,
    /// Results that arrived before their step (lockstep).
    arrived: HashMap<Ticket, CellOutcome>,
    issued: VecDeque<Issued>,
    /// The previous step's points, to measure speed and teleports.
    last_points: BTreeMap<SourceId, [f64; 3]>,
    suspended: bool,
    trimmed: bool,
    sim_only: bool,
    overlay: bool,
    /// Replay: the next event of the trace to run.
    replay_cursor: usize,
    /// Replay: cells that apply at the step of their `Applied` event.
    replay_due: VecDeque<ReplayDue>,
    /// Replay: cells whose `Unloaded` event came, so their despawn may finish.
    replay_release: BTreeSet<(LevelId, CellCoord)>,
    /// The floating-origin focus of the last sample, world meters.
    focus: Option<[f64; 3]>,
    /// A rebase asked for by `stream.rebase`.
    rebase_request: Option<[i64; 3]>,
    /// The last rebase: its step and cell delta.
    last_rebase: Option<(u64, [i64; 3])>,
    /// Sources that move once their teleport target is ready.
    moves: Vec<TeleportMove>,
    /// The HLOD tree of the first level that has one.
    hlod: Option<HlodRt>,
    /// Cell VRAM is a planner bound (ADR-2317; off by default).
    vram_bound: bool,
    /// Enabled `StreamingSource` entities, from the last sample or from the caller before [`Self::pin`].
    enabled_entity_sources: usize,
    /// Times a pin wait asked the grid for the cells inside its radius.
    pin_wait_scans: u64,
    /// Root-list entries the guard has examined that are not stored on one level.
    guard_root_visits: u64,
    /// Trace events a replay step copied out of [`StreamTiming::Replay`].
    replay_events_copied: u64,
    /// Pin waits that already passed. Dropped when a cell changes state or the pin changes.
    pin_waits_met: BTreeSet<PinId>,
    /// A cell's entity list changed since the last HLOD step.
    hlod_members_dirty: bool,
}

/// A cell whose read a replay must land before the recorded apply step of its `Applied` event.
#[derive(Debug, Clone, Copy)]
struct ReplayDue {
    /// The recorded apply step.
    due: u64,
    /// The level.
    level: LevelId,
    /// The cell.
    cell: CellCoord,
    /// The wait cap was already missed at the due step: later steps poll instead of blocking on
    /// the cap, the way the lockstep issued queue does.
    stalled: bool,
}

/// A source waiting for its teleport target.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TeleportMove {
    /// The open level whose cells hold the target when the move was queued; `close_level` of it
    /// drops the move (ADR-8813). `None`: no open level held it, and the move lives until the last
    /// level closes.
    pub(crate) level: Option<LevelId>,
    pub(crate) pin: PinId,
    pub(crate) entity: Entity,
    pub(crate) target: [f64; 3],
    pub(crate) readiness: Readiness,
    pub(crate) radius_m: f64,
}

/// The runtime side of the HLOD tree.
pub(crate) struct HlodRt {
    pub(crate) level: LevelId,
    pub(crate) tree: regia_scene::hlod::HlodTree,
    pub(crate) parents: Vec<Option<u32>>,
    pub(crate) proxies: Vec<Option<AssetId>>,
    pub(crate) anchors: Vec<[i64; 3]>,
    /// Flat proxy colors by node.
    pub(crate) colors: Vec<Option<[f32; 4]>>,
    /// Baked atlas materials by node (ADR-2483); empty or `None` draws the flat color.
    pub(crate) materials: Vec<Option<AssetId>>,
    /// Cross-fades in progress (ADR-2022).
    pub(crate) fade: crate::hlod_fade::FadeState,
    /// Entities that carry `HlodFade` now.
    pub(crate) faded: BTreeSet<Entity>,
    /// Proxy entities, by node.
    pub(crate) spawned: BTreeMap<u32, Entity>,
    /// Voxel proxies of the scatter by node (ADR-3253); empty without a voxel table.
    pub(crate) voxels: Vec<Option<crate::hlod_voxels::HlodVoxelNode>>,
    /// Voxel proxy entities, by node.
    pub(crate) spawned_voxels: BTreeMap<u32, Entity>,
    /// Nodes the budget may not drop (roots, the cut, fades, the selection's proxies). Kept across
    /// frames so the HLOD step does not allocate a set for them every frame.
    pub(crate) held_ids: Vec<u32>,
    /// Cells whose members carry `CellRenderHidden`, with how many members were marked: a cell that
    /// spawns behind its proxy keeps growing and its new members need the mark too.
    pub(crate) hidden: BTreeMap<CellCoord, usize>,
    /// Times `repair` walked a proxy's leaves. A steady camera does this once.
    pub(crate) leaf_walks: u64,
    /// Times repair ran. A cached cut does not add to this.
    pub(crate) repairs: u64,
    /// Leaf cells under each node, one entry per tree node, computed when the level opens.
    pub(crate) leaf_counts: Vec<u32>,
    /// The inputs of the cut currently published. `None` until the first repair.
    pub(crate) cut_key: Option<HlodCutKey>,
}

/// What the published HLOD cut was built from. Equal keys reuse that cut.
#[derive(Clone, PartialEq)]
pub(crate) struct HlodCutKey {
    /// The selection `HlodTree::select` returned.
    pub(crate) selection: Vec<regia_scene::hlod::HlodDraw>,
    /// Fingerprint of the resident mesh proxies.
    pub(crate) resident_fp: u64,
    /// Fingerprint of the resident voxel proxies.
    pub(crate) voxel_fp: u64,
    /// `(node, draws full)` for every full draw in the selection, sorted by node.
    pub(crate) full_visible: Vec<(u32, bool)>,
    /// Fingerprint of `Visible` cells when a selected proxy is not resident. Zero otherwise:
    /// a resident proxy's cut does not read cell visibility.
    pub(crate) visible_fp: u64,
}

impl Default for WorldStreaming {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            levels: BTreeMap::new(),
            pins: BTreeMap::new(),
            next_pin: 1,
            timing: StreamTiming::default(),
            tx,
            rx,
            log: Log::default(),
            arrived: HashMap::new(),
            issued: VecDeque::new(),
            last_points: BTreeMap::new(),
            suspended: false,
            trimmed: false,
            sim_only: false,
            overlay: false,
            replay_cursor: 0,
            replay_due: VecDeque::new(),
            replay_release: BTreeSet::new(),
            focus: None,
            rebase_request: None,
            last_rebase: None,
            moves: Vec::new(),
            hlod: None,
            vram_bound: false,
            enabled_entity_sources: 0,
            pin_wait_scans: 0,
            guard_root_visits: 0,
            replay_events_copied: 0,
            pin_waits_met: BTreeSet::new(),
            hlod_members_dirty: false,
        }
    }
}

impl LevelRt {
    fn record(&self, cell: CellCoord) -> Option<&CellRecord> {
        self.records.get(&cell).and_then(|i| self.index.cells.get(*i))
    }

    fn own_bytes(&self, cell: CellCoord) -> u64 {
        self.record(cell).map_or(0, |r| r.own_bytes)
    }

    /// Runs `f` on the cell and folds the state and measured-byte change into the running totals.
    fn with_cell<R>(&mut self, cell: CellCoord, f: impl FnOnce(&mut CellRt) -> R) -> Option<R> {
        let (result, before, after) = {
            let rt = self.cells.get_mut(&cell)?;
            let before = CellSnap::of(rt);
            let result = f(rt);
            let after = CellSnap::of(rt);
            (result, before, after)
        };
        self.apply_delta(cell, before, after);
        Some(result)
    }

    fn apply_delta(&mut self, cell: CellCoord, before: CellSnap, after: CellSnap) {
        if before.kind == after.kind && before.measured == after.measured {
            return;
        }
        if before.kind != after.kind {
            self.stats_changed = true;
        }
        match (before.kind.resident(), after.kind.resident()) {
            (false, true) => self.resident_cells = self.resident_cells.saturating_add(1),
            (true, false) => self.resident_cells = self.resident_cells.saturating_sub(1),
            _ => {}
        }
        if after.measured >= before.measured {
            self.measured_total = self
                .measured_total
                .saturating_add(after.measured - before.measured);
        } else {
            self.measured_total = self
                .measured_total
                .saturating_sub(before.measured - after.measured);
        }
        if before.kind.spawned() != after.kind.spawned() {
            let vram = self.record(cell).map_or(0, |record| record.vram_bytes);
            if after.kind.spawned() {
                self.spawned_cells.insert(cell);
                self.vram_total = self.vram_total.saturating_add(vram);
            } else {
                self.spawned_cells.remove(&cell);
                self.vram_total = self.vram_total.saturating_sub(vram);
            }
            self.load_set_queue.push(cell);
        }
        match (before.kind.failed(), after.kind.failed()) {
            (false, true) => {
                self.failed_cells.insert(cell);
            }
            (true, false) => {
                self.failed_cells.remove(&cell);
            }
            _ => {}
        }
        match (before.kind == CellKind::Visible, after.kind == CellKind::Visible) {
            (false, true) => {
                self.visible_cells.insert(cell);
            }
            (true, false) => {
                self.visible_cells.remove(&cell);
            }
            _ => {}
        }
    }

    fn in_bounds(&self, cell: CellCoord) -> bool {
        let [lo, hi] = self.index.bounds;
        (lo.x..=hi.x).contains(&cell.x) && (lo.z..=hi.z).contains(&cell.z)
    }

    /// Whether views want `cell`: within the unload radius of any of them.
    fn wanted(&self, views: &[SourceView], cell: CellCoord) -> bool {
        let unload = self.planner.config().unload_radius;
        views.iter().any(|v| {
            self.grid.distance_to_cell(v.pos[0], v.pos[2], cell) / f64::from(v.radius_scale) <= unload
        })
    }

    /// Whether `cell` counts as `readiness`. Cells outside the bounds always do; a cell in the bounds
    /// with no record has nothing to load, so only gates that cover empty cells hold it back.
    fn cell_ready(&self, gates: &StreamGates, cell: CellCoord, readiness: Readiness) -> bool {
        if !self.in_bounds(cell) {
            return true;
        }
        match self.cells.get(&cell) {
            Some(rt) => rt.state.reached(readiness),
            None => gates.is_empty_cell_ready(self.id, cell, readiness),
        }
    }
}

/// How one hierarchy fits the current apply run (spec 78 § 8.4).
enum ApplyBatch {
    /// It fits this run.
    Take,
    /// It is larger than the entity budget: take it as the only batch of this run and count it.
    TakeOverBudget,
    /// Leave it for a later run.
    Stop,
}

/// A hierarchy larger than `budget` starts only when nothing has been applied yet, and that run
/// stops after it. A hierarchy that fits is the async floor: while `time_left` it still starts
/// even if the sum would pass the entity budget, and it stops only once the time budget is spent.
fn apply_batch(applied: u32, size: u32, budget: u32, time_left: bool) -> ApplyBatch {
    if size > budget {
        if applied > 0 {
            ApplyBatch::Stop
        } else {
            ApplyBatch::TakeOverBudget
        }
    } else if applied > 0 && applied.saturating_add(size) > budget && !time_left {
        ApplyBatch::Stop
    } else {
        ApplyBatch::Take
    }
}

impl WorldStreaming {
    /// Starts streaming a level over `source`. Cells start `Unloaded`; the next step plans them.
    pub fn open_level(
        &mut self,
        level: LevelId,
        index: WorldIndex,
        source: Arc<dyn CellSource>,
    ) -> Result<(), StreamRuntimeError> {
        index.validate()?;
        let settings = &index.settings;
        let budgets = settings.effective_budgets();
        let grid = settings.grid()?;
        let mut planner = StreamPlanner::new(grid, settings.planner_config(&budgets))?;
        planner.set_vram_budget(vram_bound_of(self.vram_bound, &budgets));
        let mut records = BTreeMap::new();
        let mut cells = BTreeMap::new();
        for (i, record) in index.cells.iter().enumerate() {
            planner.register_cell(
                record.coord,
                CellInfo {
                    bytes: record.own_bytes.saturating_add(record.dep_bytes),
                    priority: record.priority,
                },
            )?;
            planner.set_cell_vram(record.coord, record.vram_bytes)?;
            records.insert(record.coord, i);
            cells.insert(record.coord, CellRt::default());
        }
        let tuning = settings.tuning;
        if self.hlod.is_none()
            && let Some(hlod) = &index.hlod
        {
            let tree = hlod
                .tree(&grid)
                .map_err(|e| StreamRuntimeError::BadIndex(format!("hlod: {e}")))?;
            let leaf_counts = crate::hlod::leaf_counts(&tree);
            self.hlod = Some(HlodRt {
                level,
                parents: crate::hlod::parents_of(&tree),
                proxies: hlod.node_proxies.clone(),
                anchors: hlod.node_anchors.clone(),
                colors: hlod.node_colors.clone(),
                materials: (0..hlod.node_proxies.len())
                    .map(|node| hlod.atlas(node).map(|atlas| atlas.material))
                    .collect(),
                fade: crate::hlod_fade::FadeState::default(),
                faded: BTreeSet::new(),
                tree,
                spawned: BTreeMap::new(),
                voxels: Vec::new(),
                spawned_voxels: BTreeMap::new(),
                held_ids: Vec::new(),
                hidden: BTreeMap::new(),
                leaf_walks: 0,
                repairs: 0,
                leaf_counts,
                cut_key: None,
            });
        }
        self.levels.insert(
            level,
            LevelRt {
                id: level,
                source,
                index: Arc::new(index),
                records,
                grid,
                planner,
                cells,
                queue: VecDeque::new(),
                apply: VecDeque::new(),
                load_set_defined: false,
                announce: Vec::new(),
                despawn: VecDeque::new(),
                budgets,
                tuning,
                inflight: 0,
                pending_bytes: 0,
                over_budget_steps: 0,
                effective_budget: budgets.memory_budget_bytes,
                planned_points: BTreeMap::new(),
                plan_due: true,
                last_plan_step: 0,
                readiness_dirty: true,
                gate_revision: 0,
                open_wait: tuning.wait_on_open,
                opened_at_generation: self.log.generation,
                plan_cell_visits: 0,
                cell_record_visits: 0,
                guard_root_visits: 0,
                failed_cells: BTreeSet::new(),
                spawned_cells: BTreeSet::new(),
                visible_cells: BTreeSet::new(),
                resident_cells: 0,
                measured_total: 0,
                vram_total: 0,
                stats_changed: false,
                load_set_queue: Vec::new(),
                rate_tail: VecDeque::new(),
                rate_limited: false,
            },
        );
        // A level opened during a low-memory trim plans at the trimmed budget from its first
        // step, like the levels that were open when the trim began (spec 78 § 19).
        if self.trimmed {
            self.apply_trim();
        }
        Ok(())
    }

    /// Stops streaming a level: cancels its loads and forgets its cells. The level's entities belong
    /// to the level manager, which despawns them with the level.
    pub fn close_level(&mut self, level: LevelId) -> bool {
        let Some(rt) = self.levels.remove(&level) else {
            return false;
        };
        for cell in rt.cells.values() {
            if let Some(cancel) = &cell.cancel {
                cancel.cancel();
            }
        }
        self.issued.retain(|i| i.ticket.level != level);
        self.arrived.retain(|t, _| t.level != level);
        if self.hlod.as_ref().is_some_and(|h| h.level == level) {
            self.hlod = None;
            // The gauge describes the closed level's cut.
            self.log.stats.holes = 0;
        }
        // A move into the level that just closed must not fire against another level's grid
        // (ADR-8813); with no level left, no queued move has a target (it must not fire at the
        // next one). Each takes its pin with it.
        let (stale, kept): (Vec<TeleportMove>, Vec<TeleportMove>) = std::mem::take(&mut self.moves)
            .into_iter()
            .partition(|m| self.levels.is_empty() || m.level == Some(level));
        self.moves = kept;
        for stale in stale {
            self.unpin(stale.pin);
        }
        if self.levels.is_empty() {
            self.last_points.clear();
            // A request no step took must not move the next streamed level's origin.
            self.rebase_request = None;
        }
        true
    }

    /// Whether any streamed level is open.
    pub fn is_active(&self) -> bool {
        !self.levels.is_empty()
    }

    /// The open streamed levels.
    pub fn levels(&self) -> impl Iterator<Item = LevelId> + '_ {
        self.levels.keys().copied()
    }

    /// The first open level, or [`StreamRuntimeError::NotEnabled`].
    pub fn first_level(&self) -> Result<LevelId, StreamRuntimeError> {
        self.levels
            .keys()
            .next()
            .copied()
            .ok_or(StreamRuntimeError::NotEnabled)
    }

    /// The settings a level was cooked with.
    pub fn settings(&self, level: LevelId) -> Option<&LevelStreaming> {
        self.levels.get(&level).map(|l| &l.index.settings)
    }

    /// The world index of a level.
    pub fn index(&self, level: LevelId) -> Option<&Arc<WorldIndex>> {
        self.levels.get(&level).map(|l| &l.index)
    }

    /// The state of a cell. `None` for a cell with no record.
    pub fn state(&self, level: LevelId, cell: CellCoord) -> Option<CellState> {
        self.levels
            .get(&level)
            .and_then(|l| l.cells.get(&cell))
            .map(|c| c.state.clone())
    }

    /// Every cell with a record and its state, in cell order.
    pub fn cells(&self, level: LevelId) -> impl Iterator<Item = (CellCoord, CellState)> + '_ {
        self.levels
            .get(&level)
            .into_iter()
            .flat_map(|l| l.cells.iter().map(|(c, rt)| (*c, rt.state.clone())))
    }

    /// The cook record of a cell.
    pub fn record(&self, level: LevelId, cell: CellCoord) -> Option<&CellRecord> {
        self.levels.get(&level).and_then(|l| l.record(cell))
    }

    /// The decoded bytes held for a cell, what `stream.cells` calls `bytes_measured`.
    pub fn measured_bytes(&self, level: LevelId, cell: CellCoord) -> u64 {
        self.levels
            .get(&level)
            .and_then(|l| l.cells.get(&cell))
            .map_or(0, |c| c.measured)
    }

    /// The entities a spawned cell holds.
    pub fn entities(&self, level: LevelId, cell: CellCoord) -> &[Entity] {
        self.levels
            .get(&level)
            .and_then(|l| l.cells.get(&cell))
            .map_or(&[], |c| &c.entities)
    }

    /// Records how many enabled entity sources exist, so [`Self::pin`] counts them toward
    /// [`MAX_STREAM_SOURCES`]. The source sample writes this every step; a caller that pins
    /// before the next sample (the agent) writes it first.
    pub fn note_enabled_entity_sources(&mut self, count: usize) {
        self.enabled_entity_sources = count;
    }

    /// Adds a scripted source (spec 78 § 7.2).
    ///
    /// `radius_scale` is `0.1..=8`, the same range as a `StreamingSource`. A pin that would put
    /// pins plus enabled entity sources past [`MAX_STREAM_SOURCES`] is [`StreamRuntimeError::SourceLimit`]
    /// and is not stored.
    pub fn pin(
        &mut self,
        name: &str,
        center: [f64; 3],
        radius_scale: f32,
        priority: i32,
        ttl_steps: Option<u32>,
    ) -> Result<PinId, StreamRuntimeError> {
        if center.iter().any(|v| !v.is_finite()) {
            return Err(StreamRuntimeError::NonFinite("center"));
        }
        if !radius_scale.is_finite() || radius_scale <= 0.0 {
            return Err(StreamRuntimeError::NonFinite("radius_scale"));
        }
        if !(0.1..=8.0).contains(&radius_scale) {
            return Err(StreamRuntimeError::BadIndex(
                "radius_scale must be 0.1..=8".into(),
            ));
        }
        if self.pins.len() + self.enabled_entity_sources >= MAX_STREAM_SOURCES {
            return Err(StreamRuntimeError::SourceLimit);
        }
        let id = PinId(self.next_pin);
        self.next_pin += 1;
        self.pins.insert(
            id,
            Pin {
                id,
                name: name.to_owned(),
                center,
                radius_scale,
                priority,
                ttl_steps,
                wait: None,
            },
        );
        self.pin_waits_met.remove(&id);
        self.touch_plans();
        Ok(id)
    }

    /// Removes a pin, and the teleport move queued on it: the move never fires.
    pub fn unpin(&mut self, pin: PinId) -> bool {
        let removed = self.pins.remove(&pin).is_some();
        if removed {
            self.pin_waits_met.remove(&pin);
            self.moves.retain(|m| m.pin != pin);
            self.touch_plans();
        }
        removed
    }

    /// Makes a pin wait for a readiness level around it; the wait counts in
    /// [`StreamStats::pending_waits`] until met.
    ///
    /// A wait over an area an open level's grid refuses to list is
    /// [`StreamRuntimeError::WaitAreaRefused`] and the pin keeps the wait it had: it could never be
    /// met. A level opened later is checked when the wait is scanned, where a refused area stays
    /// unmet (fail closed).
    pub fn set_pin_wait(&mut self, pin: PinId, wait: Option<PinWait>) -> Result<(), StreamRuntimeError> {
        let center = self
            .pins
            .get(&pin)
            .ok_or(StreamRuntimeError::UnknownPin(pin.0))?
            .center;
        if let Some(wait) = wait {
            for level in self.levels.values() {
                if let Err(e) =
                    level
                        .grid
                        .cells_within(center[0], center[2], wait.radius_m, PIN_WAIT_MAX_CELLS)
                {
                    return Err(StreamRuntimeError::WaitAreaRefused {
                        radius_m: wait.radius_m,
                        detail: e.to_string(),
                    });
                }
            }
        }
        if let Some(entry) = self.pins.get_mut(&pin) {
            entry.wait = wait;
        }
        self.pin_waits_met.remove(&pin);
        Ok(())
    }

    /// A pin.
    pub fn pin_info(&self, pin: PinId) -> Option<&Pin> {
        self.pins.get(&pin)
    }

    /// Every pin.
    pub fn pins(&self) -> impl Iterator<Item = &Pin> + '_ {
        self.pins.values()
    }

    /// [`Self::pin`] with priority +1000 and a wait for the cells around the target (spec 78 § 8.7).
    pub fn prepare_teleport(&mut self, target: [f64; 3]) -> Result<PinId, StreamRuntimeError> {
        let radius = self
            .levels
            .values()
            .next()
            .map(|l| l.planner.config().load_radius.min(l.grid.cell_size() * 2.0))
            .ok_or(StreamRuntimeError::NotEnabled)?;
        let pin = self.pin("teleport", target, 1.0, 1000, None)?;
        let wait = Some(PinWait {
            radius_m: radius,
            readiness: Readiness::SimReady,
        });
        // A target the grid cannot list cells around (outside its coordinate range) is refused,
        // and no pin is left behind (ADR-7612).
        if let Err(e) = self.set_pin_wait(pin, wait) {
            self.unpin(pin);
            return Err(e);
        }
        Ok(pin)
    }

    /// Whether every cell within `radius_m` of `center` reached `readiness`, in every open level.
    /// Takes the gates because cells with no record answer through them (spec 78 § 6.2).
    pub fn ready_near(
        &self,
        gates: &StreamGates,
        center: [f64; 3],
        radius_m: f64,
        readiness: Readiness,
    ) -> bool {
        let (waiting, refused) = self.scan_near(gates, center, radius_m, readiness, 1);
        waiting.is_empty() && !refused
    }

    /// The cells within `radius_m` of `center` that have not reached `readiness`, at most `limit`.
    /// A level whose grid refuses the area (more than 2^22 cells, or a center out of range) lists
    /// no cell here, but [`Self::ready_near`] answers `false` for it.
    pub fn waiting_near(
        &self,
        gates: &StreamGates,
        center: [f64; 3],
        radius_m: f64,
        readiness: Readiness,
        limit: usize,
    ) -> Vec<CellCoord> {
        self.scan_near(gates, center, radius_m, readiness, limit).0
    }

    /// [`Self::waiting_near`], and whether a level's grid refused the area. A refused area is
    /// never ready (fail closed, as the level-open wait is).
    fn scan_near(
        &self,
        gates: &StreamGates,
        center: [f64; 3],
        radius_m: f64,
        readiness: Readiness,
        limit: usize,
    ) -> (Vec<CellCoord>, bool) {
        let mut out = Vec::new();
        let mut refused = false;
        for level in self.levels.values() {
            let Ok(cells) = level.grid.cells_within(center[0], center[2], radius_m, 1 << 22) else {
                refused = true;
                continue;
            };
            for cell in cells {
                if !level.cell_ready(gates, cell, readiness) {
                    out.push(cell);
                    if out.len() >= limit {
                        return (out, refused);
                    }
                }
            }
        }
        (out, refused)
    }

    /// Replaces a level's budgets.
    pub fn set_budgets(&mut self, level: LevelId, budgets: StreamBudgets) -> Result<(), StreamRuntimeError> {
        budgets.validate().map_err(StreamRuntimeError::BadBudget)?;
        let rt = self
            .levels
            .get_mut(&level)
            .ok_or(StreamRuntimeError::NotEnabled)?;
        rt.budgets = budgets;
        rt.effective_budget = budgets.memory_budget_bytes;
        rt.over_budget_steps = 0;
        let config = rt.index.settings.planner_config(&budgets);
        rt.planner
            .set_config(config)
            .map_err(|e| StreamRuntimeError::BadBudget(e.to_string()))?;
        rt.planner
            .set_vram_budget(vram_bound_of(self.vram_bound, &budgets));
        Self::invalidate_plan(rt);
        self.apply_trim();
        Ok(())
    }

    /// Makes each level's `vram_budget_bytes` a hard bound of its plan (`true`) or only a reported
    /// number (`false`, the default), for the open levels and the ones opened later (spec 78 § 8.4,
    /// ADR-2317). With the bound on, a cell loads only if the cook's VRAM estimates of the loaded
    /// and draining cells plus its own fit, and a lowered budget sheds the farthest cells. A budget of
    /// 0 (a dedicated server) means no bound.
    pub fn set_vram_bound(&mut self, on: bool) {
        self.vram_bound = on;
        for level in self.levels.values_mut() {
            level.planner.set_vram_budget(vram_bound_of(on, &level.budgets));
            Self::invalidate_plan(level);
        }
    }

    /// Whether cell VRAM is a planner bound ([`Self::set_vram_bound`]).
    pub fn vram_bound(&self) -> bool {
        self.vram_bound
    }

    /// The cell grid, bounds and record count of an open level: what changes when it reopens.
    pub(crate) fn level_shape(&self, level: LevelId) -> Option<(StreamGrid, [CellCoord; 2], usize)> {
        self.levels
            .get(&level)
            .map(|rt| (rt.grid, rt.index.bounds, rt.records.len()))
    }

    /// The cell grid, bounds and content cells of an open level (spec 78 § 13.1), as hooks below the
    /// runtime see them in `StreamLevelLayouts`.
    pub fn level_layout(&self, level: LevelId) -> Option<regia_scene::streaming::StreamLevelLayout> {
        self.levels
            .get(&level)
            .map(|rt| regia_scene::streaming::StreamLevelLayout {
                grid: rt.grid,
                bounds: rt.index.bounds,
                records: rt.records.keys().copied().collect(),
            })
    }

    /// The budgets a level runs with.
    pub fn budgets(&self, level: LevelId) -> Option<&StreamBudgets> {
        self.levels.get(&level).map(|l| &l.budgets)
    }

    /// Switches the timing mode. Switching to lockstep keeps reads already issued in the new order.
    /// Switching to async drops that order: results the old order held back are accepted at once,
    /// inside this call, so those cells go to `Applying` before the next `Complete`. Switching to
    /// replay does not adopt cells that are `Loading`: replay applies only the trace's `Applied`
    /// events, so switch modes before streaming starts, not mid-load.
    pub fn set_timing(&mut self, timing: StreamTiming) {
        if let StreamTiming::Lockstep { latency_steps } = timing {
            // Reads issued under async timing join the order at once.
            for level in self.levels.values() {
                for (cell, rt) in &level.cells {
                    if rt.state == CellState::Loading {
                        let ticket = Ticket {
                            level: level.id,
                            cell: *cell,
                            generation: rt.generation,
                        };
                        if !self.issued.iter().any(|i| i.ticket == ticket) {
                            self.issued.push_back(Issued {
                                ticket,
                                due_step: self.log.stats.step + u64::from(latency_steps),
                                stalled: false,
                            });
                        }
                    }
                }
            }
        }
        if timing == StreamTiming::Async {
            // Reads the old order held back land the async way, in the order they were issued,
            // then any others by (level, cell, generation). `arrived` is a hash map: draining it
            // directly accepted them in a per-process order, so which cell went live first and
            // the ids it spawned differed between runs (audit v033b).
            let order: Vec<Ticket> = self.issued.drain(..).map(|issued| issued.ticket).collect();
            let mut held: Vec<(Ticket, CellOutcome)> = order
                .into_iter()
                .filter_map(|ticket| self.arrived.remove(&ticket).map(|outcome| (ticket, outcome)))
                .collect();
            let mut rest: Vec<(Ticket, CellOutcome)> = self.arrived.drain().collect();
            rest.sort_by_key(|(ticket, _)| (ticket.level, ticket.cell, ticket.generation));
            held.extend(rest);
            for (ticket, outcome) in held {
                self.accept(ticket, outcome);
            }
        }
        self.replay_cursor = 0;
        self.replay_due.clear();
        self.replay_release.clear();
        self.timing = timing;
    }

    /// The timing mode.
    pub fn timing(&self) -> &StreamTiming {
        &self.timing
    }

    /// Lets a failed cell try again now, resetting its retry count.
    pub fn retry(&mut self, level: LevelId, cell: CellCoord) -> Result<CellState, StreamRuntimeError> {
        let rt = self
            .levels
            .get_mut(&level)
            .ok_or(StreamRuntimeError::NotEnabled)?;
        let state = rt
            .cells
            .get(&cell)
            .map(|c| c.state.clone())
            .ok_or(StreamRuntimeError::UnknownCell(cell))?;
        if matches!(state, CellState::Failed(_)) {
            rt.with_cell(cell, |c| {
                c.failures = 0;
                c.retry_step = None;
                c.state = CellState::Unloaded;
            });
            rt.planner.set_held(cell, false)?;
            Self::invalidate_plan(rt);
            return Ok(CellState::Unloaded);
        }
        Ok(state)
    }

    /// The counters.
    pub fn stats(&self) -> &StreamStats {
        &self.log.stats
    }

    /// Registered cells the planner has walked since this runtime was created.
    ///
    /// Each call to the full plan adds that level's cell count.
    pub fn plan_cell_visits(&self) -> u64 {
        self.levels.values().map(|level| level.plan_cell_visits).sum()
    }

    /// Cell records the per-step maintenance walks have touched since this runtime was created.
    ///
    /// A quiet step adds nothing. The failed-cell scan is the only remaining walk, and it visits
    /// the failed set.
    pub fn cell_record_visits(&self) -> u64 {
        self.levels.values().map(|level| level.cell_record_visits).sum()
    }

    /// Times a pin wait asked the grid which cells lie inside its radius.
    pub fn pin_wait_scans(&self) -> u64 {
        self.pin_wait_scans
    }

    /// Root-list entries the membership guard has examined since this runtime was created.
    pub fn guard_root_visits(&self) -> u64 {
        self.guard_root_visits
            + self
                .levels
                .values()
                .map(|level| level.guard_root_visits)
                .sum::<u64>()
    }

    /// Trace events a replay step has copied. A step that reads the trace in place adds nothing.
    pub fn replay_events_copied(&self) -> u64 {
        self.replay_events_copied
    }

    /// Times HLOD repair expanded a proxy into its leaves since this runtime was created.
    pub fn hlod_leaf_walks(&self) -> u64 {
        self.hlod.as_ref().map_or(0, |hlod| hlod.leaf_walks)
    }

    /// Times HLOD repair ran since this runtime was created. A cached cut does not add to this.
    pub fn hlod_repairs(&self) -> u64 {
        self.hlod.as_ref().map_or(0, |hlod| hlod.repairs)
    }

    /// Whether a leaf cell draws its full content. A cell with no record draws in full. Matches the
    /// state in place, so a failed cell's reason is not cloned.
    pub(crate) fn cell_draws_full(&self, level: LevelId, cell: CellCoord) -> bool {
        match self
            .levels
            .get(&level)
            .and_then(|level| level.cells.get(&cell))
            .map(|rt| &rt.state)
        {
            Some(CellState::Visible) => true,
            Some(_) => false,
            None => true,
        }
    }

    /// Fingerprint of the cells currently `Visible` in `level`.
    pub(crate) fn visible_fp(&self, level: LevelId) -> u64 {
        let Some(level) = self.levels.get(&level) else {
            return 0;
        };
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for cell in &level.visible_cells {
            hash = fnv(hash, &cell.x.to_le_bytes());
            hash = fnv(hash, &cell.z.to_le_bytes());
        }
        hash
    }

    /// Takes the flag that a cell's entity list changed, and clears it.
    pub(crate) fn take_hlod_members_dirty(&mut self) -> bool {
        std::mem::take(&mut self.hlod_members_dirty)
    }

    /// The counters, for the systems that keep their own (guard, rebase, HLOD).
    pub(crate) fn stats_mut(&mut self) -> &mut StreamStats {
        &mut self.log.stats
    }

    /// The last events, oldest first.
    pub fn trace(&self) -> impl Iterator<Item = &TraceEvent> + '_ {
        self.log.trace.iter()
    }

    /// Everything still in the trace ring, for [`StreamTiming::Replay`].
    pub fn snapshot_trace(&self) -> StreamTrace {
        StreamTrace {
            events: self.log.trace.iter().cloned().collect(),
        }
    }

    /// Records a world-wide event.
    pub(crate) fn note(&mut self, kind: TraceKind, detail: Option<String>) {
        self.log.event(None, None, kind, detail);
    }

    /// A dedicated server has no render gates: `SimReady` is its final state (spec 78 § 8.1).
    pub fn set_sim_only(&mut self, sim_only: bool) {
        self.sim_only = sim_only;
        self.touch_readiness();
    }

    /// Stops issuing reads while the app is suspended; reads in flight finish and all cells stay
    /// (spec 78 § 19).
    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }

    /// Whether reads are stopped.
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// On a low-memory trim the memory budget drops to 50 % until cleared, and the planner sheds the
    /// farthest cells (spec 78 § 19).
    pub fn set_trimmed(&mut self, trimmed: bool) {
        self.trimmed = trimmed;
        self.apply_trim();
    }

    fn apply_trim(&mut self) {
        let trimmed = self.trimmed;
        for level in self.levels.values_mut() {
            // Half of the configured budget (spec 78 § 19), not of the adjusted one: the async
            // adjustment never goes below that half, so halving it again would plan at 25 %.
            let budget = if trimmed {
                level.budgets.memory_budget_bytes / 2
            } else {
                level.effective_budget
            };
            let mut config = *level.planner.config();
            if config.memory_budget_bytes != budget {
                config.memory_budget_bytes = budget;
                if level.planner.set_config(config).is_ok() {
                    Self::invalidate_plan(level);
                }
            }
        }
    }

    /// Moves `entity` to `target` once the cells around the target are ready, and removes `pin`.
    /// The move lives as long as its pin: a pin that does not exist queues nothing, and unpinning
    /// (or the pin's ttl running out) cancels the move. A second move on one pin replaces the
    /// first, so the queue never holds more moves than there are pins.
    ///
    /// The move belongs to the first open level (by id) whose cooked cell bounds hold the target;
    /// closing that level drops it and its pin (ADR-8813). A target outside every open level's
    /// bounds ties the move to no level: it is dropped when the last level closes.
    pub fn queue_move(
        &mut self,
        pin: PinId,
        entity: Entity,
        target: [f64; 3],
        readiness: Readiness,
        radius_m: f64,
    ) {
        if !self.pins.contains_key(&pin) {
            return;
        }
        self.moves.retain(|m| m.pin != pin);
        let level = self.level_holding(target);
        self.moves.push(TeleportMove {
            level,
            pin,
            entity,
            target,
            readiness,
            radius_m,
        });
    }

    /// Teleport moves waiting for their target (each names a live pin).
    pub fn queued_moves(&self) -> usize {
        self.moves.len()
    }

    /// The level a teleport move queued on `pin` belongs to: `Some(None)` when it belongs to no
    /// level, `None` when no move is queued on `pin` (ADR-8813).
    pub fn queued_move_level(&self, pin: PinId) -> Option<Option<LevelId>> {
        self.moves.iter().find(|m| m.pin == pin).map(|m| m.level)
    }

    /// The first open level (by id) whose cooked cell bounds hold the world position `target`.
    fn level_holding(&self, target: [f64; 3]) -> Option<LevelId> {
        self.levels.iter().find_map(|(id, rt)| {
            let cell = rt.grid.cell_of(target[0], target[2]).ok()?;
            let [lo, hi] = rt.index.bounds;
            (lo.x <= cell.x && cell.x <= hi.x && lo.z <= cell.z && cell.z <= hi.z).then_some(*id)
        })
    }

    /// Takes the moves whose target is ready.
    pub(crate) fn ready_moves(&mut self, gates: &StreamGates) -> Vec<TeleportMove> {
        let moves = std::mem::take(&mut self.moves);
        let (ready, waiting) = moves
            .into_iter()
            .partition(|m| self.ready_near(gates, m.target, m.radius_m, m.readiness));
        self.moves = waiting;
        ready
    }

    pub(crate) fn take_hlod(&mut self) -> Option<HlodRt> {
        self.hlod.take()
    }

    pub(crate) fn put_hlod(&mut self, hlod: HlodRt) {
        self.hlod = Some(hlod);
    }

    /// Hands the runtime `level`'s HLOD voxel table (ADR-3253): the nodes it names draw a voxel proxy
    /// of their scatter beside (or instead of) their mesh proxy. [`crate::open_streamed_level`] reads
    /// it from the cooked cache when the cook wrote one; a host that opens a level from its own bytes
    /// calls this after the open.
    ///
    /// # Errors
    ///
    /// [`StreamRuntimeError::BadIndex`] when `level` is not the open HLOD level, the table names
    /// another level or more nodes than the tree has, or it does not validate.
    pub fn set_hlod_voxels(
        &mut self,
        level: LevelId,
        table: crate::hlod_voxels::HlodVoxels,
    ) -> Result<(), StreamRuntimeError> {
        let bad = |m: String| StreamRuntimeError::BadIndex(format!("hlod voxels: {m}"));
        if table.level != level.0 {
            return Err(bad(format!("the table is level {}'s", table.level)));
        }
        table.validate().map_err(|e| bad(e.to_string()))?;
        let Some(hlod) = self.hlod.as_mut().filter(|h| h.level == level) else {
            return Err(bad(format!("level {} has no open HLOD tree", level.0)));
        };
        let nodes = hlod.tree.nodes().len();
        if table.nodes.len() > nodes {
            return Err(bad(format!(
                "{} entries for a {nodes}-node tree",
                table.nodes.len()
            )));
        }
        hlod.voxels = table.nodes;
        Ok(())
    }

    /// Nodes of the open HLOD tree with a voxel proxy (ADR-3253).
    pub fn hlod_voxel_nodes(&self) -> usize {
        self.hlod
            .as_ref()
            .map_or(0, |h| h.voxels.iter().flatten().count())
    }

    /// Whether a level is still in its open wait, when a hole only means a loading screen is up.
    pub(crate) fn is_opening(&self) -> bool {
        self.levels.values().any(|l| l.open_wait)
    }

    /// The level the HLOD tree belongs to.
    pub(crate) fn hlod_level(&self) -> Option<LevelId> {
        self.hlod.as_ref().map(|h| h.level)
    }

    /// Whether this process draws nothing (a dedicated server).
    pub fn is_sim_only(&self) -> bool {
        self.sim_only
    }

    /// The HLOD budget of `level`.
    /// The level's HLOD cross-fade length, frames (0: instant).
    pub(crate) fn hlod_fade_frames(&self, level: LevelId) -> u32 {
        self.levels.get(&level).map_or(0, |l| l.tuning.fade_frames())
    }

    pub(crate) fn hlod_budget(&self, level: LevelId) -> Option<u64> {
        self.levels.get(&level).map(|l| l.budgets.hlod_budget_bytes)
    }

    /// The focus of the last sample, world meters.
    pub fn focus(&self) -> Option<[f64; 3]> {
        self.focus
    }

    pub(crate) fn set_focus(&mut self, focus: Option<[f64; 3]>) {
        self.focus = focus;
    }

    /// Whether the first open level rebases.
    pub fn origin_enabled(&self) -> bool {
        self.levels
            .values()
            .next()
            .is_some_and(|l| l.index.settings.origin.enabled)
    }

    /// Asks the next step to move the origin to `cell`.
    pub fn request_rebase(&mut self, cell: [i64; 3]) {
        self.rebase_request = Some(cell);
    }

    pub(crate) fn take_rebase_request(&mut self) -> Option<[i64; 3]> {
        self.rebase_request.take()
    }

    pub(crate) fn note_rebase(&mut self, step: u64, cell_delta: [i64; 3]) {
        self.last_rebase = Some((step, cell_delta));
    }

    /// The last rebase: its step and cell delta.
    pub fn last_rebase(&self) -> Option<(u64, [i64; 3])> {
        self.last_rebase
    }

    /// Turns the debug overlay on or off.
    pub fn set_overlay(&mut self, on: bool) {
        self.overlay = on;
    }

    /// Whether the debug overlay is on.
    pub fn overlay(&self) -> bool {
        self.overlay
    }

    fn invalidate_plan(level: &mut LevelRt) {
        level.plan_due = true;
        level.rate_limited = false;
        level.rate_tail.clear();
    }

    fn touch_plans(&mut self) {
        for level in self.levels.values_mut() {
            Self::invalidate_plan(level);
        }
    }

    fn touch_readiness(&mut self) {
        for level in self.levels.values_mut() {
            level.readiness_dirty = true;
        }
    }

    /// Views of the pins, sorted by id.
    pub(crate) fn pin_views(&self) -> Vec<SourceView> {
        self.pins
            .values()
            .map(|p| SourceView {
                radius_scale: p.radius_scale,
                priority: p.priority,
                ..SourceView::at(SourceId::Pin(p.id.0), p.center)
            })
            .collect()
    }

    /// The point a source held last step, to measure its speed.
    pub(crate) fn last_point(&self, id: SourceId) -> Option<[f64; 3]> {
        self.last_points.get(&id).copied()
    }

    /// Enters the next fixed step. The first thing in the step: `Guard` runs before physics.
    pub(crate) fn begin_step(&mut self) {
        self.log.stats.step += 1;
        let step = self.log.stats.step;
        // A source shared by several levels counts the step once. The address list stays on
        // the stack for the levels a process actually opens; a spill buffer allocates only past that.
        const STACK: usize = 8;
        let mut seen: [usize; STACK] = [0; STACK];
        let mut count = 0usize;
        let mut spill: Vec<usize> = Vec::new();
        for level in self.levels.values() {
            let addr = source_addr(&level.source);
            if seen[..count].contains(&addr) || spill.contains(&addr) {
                continue;
            }
            level.source.on_step(step);
            if count < STACK {
                seen[count] = addr;
                count += 1;
            } else {
                spill.push(addr);
            }
        }
    }

    /// `StreamSet::Plan`: expire pins, retry held cells, and run the planner where a trigger fired
    /// (spec 78 § 8.2). In replay timing the trace decides instead.
    pub(crate) fn plan_step(
        &mut self,
        views: &[SourceView],
        unavailable: &dyn Fn(AssetId) -> Option<String>,
    ) {
        let step = self.log.stats.step;
        self.expire_pins();

        // Sources: speed, teleports, and the points for the next step.
        let mut points: BTreeMap<SourceId, [f64; 3]> = BTreeMap::new();
        for view in views {
            points.entry(view.source).or_insert(view.pos);
        }
        let levels: Vec<LevelId> = self.levels.keys().copied().collect();
        for id in levels {
            let Some(level) = self.levels.get_mut(&id) else {
                continue;
            };
            let unload = level.planner.config().unload_radius;
            let teleported = points.iter().any(|(source, p)| {
                self.last_points
                    .get(source)
                    .is_some_and(|q| (p[0] - q[0]).hypot(p[2] - q[2]) > 2.0 * unload)
            });
            if teleported {
                self.log.stats.teleports += 1;
                Self::invalidate_plan(level);
            }
            if matches!(self.timing, StreamTiming::Replay(_)) {
                continue;
            }
            Self::plan_level(level, &mut self.log, &points, views, step, unavailable);
        }
        self.last_points = points;

        let replay_events = match &mut self.timing {
            StreamTiming::Replay(trace) => std::mem::take(&mut trace.events),
            _ => Vec::new(),
        };
        if matches!(self.timing, StreamTiming::Replay(_)) {
            self.replay_step(&replay_events, step);
            if let StreamTiming::Replay(trace) = &mut self.timing {
                trace.events = replay_events;
            }
        }
        self.track_memory();
        self.pump_io();
    }

    fn expire_pins(&mut self) {
        let mut expired = Vec::new();
        for pin in self.pins.values_mut() {
            if let Some(ttl) = &mut pin.ttl_steps {
                if *ttl == 0 {
                    expired.push(pin.id);
                } else {
                    *ttl -= 1;
                }
            }
        }
        if !expired.is_empty() {
            for pin in expired {
                self.pins.remove(&pin);
                self.pin_waits_met.remove(&pin);
                self.moves.retain(|m| m.pin != pin);
            }
            self.touch_plans();
        }
    }

    fn plan_level(
        level: &mut LevelRt,
        log: &mut Log,
        points: &BTreeMap<SourceId, [f64; 3]>,
        views: &[SourceView],
        step: u64,
        unavailable: &dyn Fn(AssetId) -> Option<String>,
    ) {
        // Held cells: release the ones whose retry step came, forget the ones nobody wants.
        // Only the failed set is touched. An idle level has none.
        let failed: Vec<CellCoord> = level.failed_cells.iter().copied().collect();
        level.cell_record_visits += failed.len() as u64;
        for cell in failed {
            let wanted = level.wanted(views, cell);
            let action = level.with_cell(cell, |rt| {
                if !matches!(rt.state, CellState::Failed(_)) {
                    return 0;
                }
                if !wanted {
                    rt.state = CellState::Unloaded;
                    rt.failures = 0;
                    rt.retry_step = None;
                    1
                } else if rt.retry_step.is_some_and(|due| due <= step) {
                    rt.retry_step = None;
                    2
                } else {
                    0
                }
            });
            match action {
                Some(1) => {
                    let _ = level.planner.set_held(cell, false);
                }
                Some(2) => {
                    let _ = level.planner.set_held(cell, false);
                    Self::invalidate_plan(level);
                }
                _ => {}
            }
        }

        let threshold = level.grid.cell_size() / 8.0;
        let moved = points.len() != level.planned_points.len()
            || points.iter().any(|(source, p)| {
                level
                    .planned_points
                    .get(source)
                    .is_none_or(|q| (p[0] - q[0]).hypot(p[2] - q[2]) > threshold)
            });
        let interval = u64::from(level.tuning.plan_interval_steps.max(1));
        let due = level.plan_due || moved || step.saturating_sub(level.last_plan_step) >= interval;
        if !due || views.is_empty() {
            return;
        }
        let started = Instant::now();
        // The view has not crossed a cell boundary and the last plan only ran out of load slots.
        // Hand out the next slice of that plan instead of walking the registry again.
        if !moved && level.rate_limited && !level.rate_tail.is_empty() {
            if Self::serve_rate_tail(level, log, points, step, unavailable) {
                log.stats.last_plan_ms = started.elapsed().as_secs_f32() * 1000.0;
                return;
            }
            level.rate_limited = false;
            level.rate_tail.clear();
        }
        level.plan_cell_visits += level.planner.cell_count() as u64;
        let plan = match level.planner.plan_views(views) {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(target: "regia_world", "stream plan: {error}");
                return;
            }
        };
        level.plan_due = false;
        level.last_plan_step = step;
        level.planned_points.clone_from(points);

        let mut draining = Vec::new();
        for order in &plan.unloads {
            if Self::begin_unload(level, log, order.cell) {
                draining.push(order.cell);
            }
        }
        if let Err(error) = level.planner.apply(&plan) {
            tracing::warn!(target: "regia_world", "stream plan apply: {error}");
            return;
        }
        for cell in draining {
            let _ = level.planner.set_draining(cell, true);
        }
        let loads: Vec<CellCoord> = plan.loads.iter().map(|order| order.cell).collect();
        let invalidated = Self::queue_planned_loads(level, log, &loads, unavailable);
        // The limit is per plan: what it held back is the next plan's, unless a cell was over
        // budget and the next step has to evict. A cell that can never fit (`TooLarge`) does not
        // by itself throw the tail away. A load that failed just now already invalidated the tail.
        if !invalidated {
            let blocked = plan
                .deferred
                .iter()
                .any(|deferred| deferred.reason == DeferReason::OverBudget);
            if blocked {
                level.rate_tail.clear();
                level.rate_limited = false;
                level.plan_due = plan
                    .deferred
                    .iter()
                    .any(|deferred| deferred.reason == DeferReason::RateLimited);
            } else {
                level.rate_tail = plan
                    .deferred
                    .iter()
                    .filter(|deferred| deferred.reason == DeferReason::RateLimited)
                    .map(|deferred| deferred.cell)
                    .collect();
                level.rate_limited = !level.rate_tail.is_empty();
                level.plan_due = level.rate_limited;
            }
        }
        log.stats.plans += 1;
        let scanned = level.planner.cell_count() as u64;
        log.stats.plan_cells_evaluated += scanned;
        log.stats.peak_plan_cells = log
            .stats
            .peak_plan_cells
            .max(scanned.min(u64::from(u32::MAX)) as u32);
        log.stats.last_plan_ms = started.elapsed().as_secs_f32() * 1000.0;
    }

    /// Issues the next `max_loads_per_step` cells of a rate-limited plan without walking the registry.
    ///
    /// Returns false before any change when the cap is unlimited, a cell does not fit without an
    /// eviction, or the planner rejects the slice. The caller then runs a full plan.
    fn serve_rate_tail(
        level: &mut LevelRt,
        log: &mut Log,
        points: &BTreeMap<SourceId, [f64; 3]>,
        step: u64,
        unavailable: &dyn Fn(AssetId) -> Option<String>,
    ) -> bool {
        let cap = level.planner.config().max_loads_per_step;
        if cap == 0 {
            return false;
        }
        let memory_budget = level.planner.config().memory_budget_bytes;
        let vram_budget = level.planner.vram_budget();
        let mut chosen = Vec::new();
        let mut skipped = Vec::new();
        let mut used = level
            .planner
            .loaded_bytes()
            .saturating_add(level.planner.draining_bytes());
        let mut used_vram = level
            .planner
            .loaded_vram()
            .saturating_add(level.planner.draining_vram());
        for cell in level.rate_tail.iter().copied() {
            if chosen.len() >= cap as usize {
                break;
            }
            let loadable = level
                .cells
                .get(&cell)
                .is_some_and(|rt| rt.state == CellState::Unloaded)
                && !level.planner.is_held(cell)
                && !level.planner.is_loaded(cell)
                && !level.planner.is_draining(cell);
            if !loadable {
                skipped.push(cell);
                continue;
            }
            let Some(info) = level.planner.cell_info(cell) else {
                skipped.push(cell);
                continue;
            };
            let next = used.saturating_add(info.bytes);
            let next_vram = used_vram.saturating_add(level.planner.cell_vram(cell));
            if next > memory_budget || vram_budget.is_some_and(|budget| next_vram > budget) {
                return false;
            }
            chosen.push(cell);
            used = next;
            used_vram = next_vram;
        }
        if chosen.is_empty() && skipped.is_empty() {
            return false;
        }
        if !chosen.is_empty() {
            let mut loads = Vec::with_capacity(chosen.len());
            for cell in &chosen {
                let Some(info) = level.planner.cell_info(*cell) else {
                    return false;
                };
                loads.push(LoadOrder {
                    cell: *cell,
                    distance: 0.0,
                    bytes: info.bytes,
                    priority: info.priority,
                });
            }
            let plan = StreamPlan {
                loads,
                ..StreamPlan::default()
            };
            if level.planner.apply(&plan).is_err() {
                return false;
            }
        }
        let drop_these: BTreeSet<CellCoord> = chosen.iter().copied().chain(skipped.iter().copied()).collect();
        let remaining: VecDeque<CellCoord> = level
            .rate_tail
            .iter()
            .copied()
            .filter(|cell| !drop_these.contains(cell))
            .collect();
        let invalidated = Self::queue_planned_loads(level, log, &chosen, unavailable);
        if !invalidated {
            level.rate_tail = remaining;
            level.rate_limited = !level.rate_tail.is_empty();
            level.plan_due = level.rate_limited;
            level.last_plan_step = step;
            level.planned_points.clone_from(points);
        }
        log.stats.plans += 1;
        true
    }

    /// Queues `loads`. A cell whose chunk is not installed fails and invalidates the rate-limit tail.
    ///
    /// Returns whether that invalidation happened. The caller then leaves the tail cleared.
    fn queue_planned_loads(
        level: &mut LevelRt,
        log: &mut Log,
        loads: &[CellCoord],
        unavailable: &dyn Fn(AssetId) -> Option<String>,
    ) -> bool {
        let mut invalidated = false;
        for &cell in loads {
            let Some(record) = level.record(cell) else {
                continue;
            };
            if let Some(reason) = unavailable(record.blob) {
                tracing::debug!(
                    target: "regia_world",
                    "cell {cell} chunk {reason} is not installed"
                );
                Self::fail_cell(level, log, cell, "cell_unavailable", false);
                invalidated = true;
                continue;
            }
            if !level.cells.contains_key(&cell) {
                continue;
            }
            let generation = log.next_generation();
            level.with_cell(cell, |rt| {
                rt.generation = generation;
                rt.state = CellState::Queued;
            });
            level.queue.push_back(cell);
            log.event(Some(level.id), Some(cell), TraceKind::Queued, None);
        }
        invalidated
    }

    /// Takes a cell out of the world. Returns whether it still holds memory (the planner keeps it
    /// draining until the despawn finishes).
    fn begin_unload(level: &mut LevelRt, log: &mut Log, cell: CellCoord) -> bool {
        let kind = match level.cells.get(&cell).map(|rt| CellKind::of(&rt.state)) {
            Some(kind) => kind,
            None => return false,
        };
        match kind {
            CellKind::Queued => {
                level.with_cell(cell, |rt| {
                    if matches!(rt.state, CellState::Queued) {
                        rt.state = CellState::Unloaded;
                    }
                });
                level.queue.retain(|c| *c != cell);
                log.stats.loads_cancelled += 1;
                log.event(Some(level.id), Some(cell), TraceKind::Cancelled, None);
                false
            }
            CellKind::Loading => {
                level.with_cell(cell, |rt| {
                    if let Some(cancel) = rt.cancel.take() {
                        cancel.cancel();
                    }
                    if matches!(rt.state, CellState::Loading) {
                        rt.state = CellState::Unloaded;
                    }
                });
                log.stats.loads_cancelled += 1;
                log.event(Some(level.id), Some(cell), TraceKind::Cancelled, None);
                false
            }
            CellKind::Applying => {
                let roots_empty = level.cells.get(&cell).is_some_and(|rt| rt.roots.is_empty());
                let own = level.own_bytes(cell);
                let released = level.cells.get_mut(&cell).is_some_and(|rt| {
                    let generation = rt.generation;
                    rt.take_reservation(generation)
                });
                if released {
                    level.pending_bytes = level.pending_bytes.saturating_sub(own);
                }
                level.apply.retain(|c| *c != cell);
                log.stats.loads_cancelled += 1;
                log.event(Some(level.id), Some(cell), TraceKind::Cancelled, None);
                if roots_empty {
                    level.with_cell(cell, |rt| {
                        rt.blob = None;
                        rt.ranges.clear();
                        rt.next_range = 0;
                        rt.state = CellState::Unloaded;
                        rt.measured = 0;
                    });
                    false
                } else {
                    // No `CellLoaded` went out, so no `CellUnloading` follows it.
                    level.with_cell(cell, |rt| {
                        rt.blob = None;
                        rt.ranges.clear();
                        rt.next_range = 0;
                        rt.state = CellState::Unloading;
                    });
                    level.despawn.push_back(cell);
                    true
                }
            }
            CellKind::Spawned | CellKind::SimReady | CellKind::Visible => {
                level.with_cell(cell, |rt| {
                    rt.state = CellState::Unloading;
                });
                level.announce.push(cell);
                log.event(Some(level.id), Some(cell), TraceKind::Unloading, None);
                level.readiness_dirty = true;
                true
            }
            CellKind::Unloading => true,
            CellKind::Failed | CellKind::Unloaded => false,
        }
    }

    /// Marks a cell failed: releases its memory, holds it, and schedules a retry while retries last.
    fn fail_cell(level: &mut LevelRt, log: &mut Log, cell: CellCoord, reason: &str, had_reservation: bool) {
        let own = level.own_bytes(cell);
        let step = log.stats.step;
        let (retry_after, max_retries) = (level.tuning.retry_after_steps, level.tuning.max_retries);
        let updated = level.with_cell(cell, |rt| {
            rt.state = CellState::Failed(reason.to_owned());
            rt.cancel = None;
            rt.blob = None;
            rt.ranges.clear();
            rt.next_range = 0;
            rt.measured = 0;
            rt.failures += 1;
            rt.retry_step = (rt.failures <= max_retries).then(|| step + u64::from(retry_after));
        });
        if updated.is_none() {
            return;
        }
        // The current read's reservation, once: a stale result of a read the trace already failed
        // finds it gone.
        let released = had_reservation
            && level.cells.get_mut(&cell).is_some_and(|rt| {
                let generation = rt.generation;
                rt.take_reservation(generation)
            });
        if released {
            level.pending_bytes = level.pending_bytes.saturating_sub(own);
        }
        let _ = level.planner.set_loaded(cell, false);
        let _ = level.planner.set_held(cell, true);
        Self::invalidate_plan(level);
        log.stats.failures += 1;
        tracing::warn!(target: "regia_world", "cell {cell} failed: {reason}");
        log.event(
            Some(level.id),
            Some(cell),
            TraceKind::Failed,
            Some(reason.to_owned()),
        );
        log.out.push(Outgoing::Failed(CellFailed {
            level: level.id,
            cell,
            reason: reason.to_owned(),
        }));
    }

    /// Issues reads from the queues within `max_inflight` and `max_pending_bytes`.
    pub(crate) fn pump_io(&mut self) {
        if self.suspended {
            return;
        }
        let latency = match self.timing {
            StreamTiming::Lockstep { latency_steps } => Some(u64::from(latency_steps)),
            _ => None,
        };
        // A replay's reads follow its trace; the run it recorded already spaced them.
        let replay = matches!(self.timing, StreamTiming::Replay(_));
        for level in self.levels.values_mut() {
            while let Some(&cell) = level.queue.front() {
                if !replay && level.inflight >= level.budgets.max_inflight {
                    break;
                }
                let Some(record) = level.record(cell) else {
                    level.queue.pop_front();
                    continue;
                };
                let (own, blob, hash) = (record.own_bytes, record.blob, record.blob_hash);
                // A single cell larger than the budget may still go alone.
                if !replay
                    && level.pending_bytes > 0
                    && level.pending_bytes.saturating_add(own) > level.budgets.max_pending_bytes
                {
                    break;
                }
                level.queue.pop_front();
                let started = level.with_cell(cell, |rt| {
                    let cancel = CancelToken::new();
                    rt.cancel = Some(cancel.clone());
                    rt.state = CellState::Loading;
                    rt.reserved_reads.push(rt.generation);
                    (cancel, rt.generation)
                });
                let Some((cancel, generation)) = started else {
                    continue;
                };
                let ticket = Ticket {
                    level: level.id,
                    cell,
                    generation,
                };
                level.inflight += 1;
                level.pending_bytes = level.pending_bytes.saturating_add(own);
                self.log.stats.loads_issued += 1;
                self.log
                    .event(Some(level.id), Some(cell), TraceKind::Loading, None);
                if let Some(latency) = latency {
                    self.issued.push_back(Issued {
                        ticket,
                        due_step: self.log.stats.step + latency,
                        stalled: false,
                    });
                }
                let sink = CellSink::new(self.tx.clone(), ticket, blob, hash, cancel.clone());
                level.source.read(blob, ticket, cancel, sink);
            }
        }
    }

    /// `StreamSet::Complete` in async timing: takes whatever finished.
    pub(crate) fn complete_async(&mut self) {
        while let Ok(result) = self.rx.try_recv() {
            self.accept(result.ticket, result.outcome);
        }
        self.pump_io();
    }

    /// `StreamSet::Complete` in lockstep and replay timing: takes the reads due at this step in issue
    /// order, waiting for late IO up to the cap.
    pub(crate) fn complete_lockstep(&mut self) {
        let step = self.log.stats.step;
        while let Ok(result) = self.rx.try_recv() {
            self.arrived.insert(result.ticket, result.outcome);
        }
        self.sweep_arrived(true);
        while let Some(front) = self.issued.front().copied() {
            if front.due_step > step {
                break;
            }
            let cap = if front.stalled {
                Duration::ZERO
            } else {
                let ms = self
                    .levels
                    .get(&front.ticket.level)
                    .map_or(0, |l| l.tuning.lockstep_wait_cap_ms);
                Duration::from_millis(u64::from(ms))
            };
            match self.wait_for(front.ticket, cap) {
                Some(outcome) => {
                    self.issued.pop_front();
                    self.accept(front.ticket, outcome);
                }
                None => {
                    if !front.stalled {
                        self.log.stats.lockstep_stalls += 1;
                        tracing::error!(
                            target: "regia_world",
                            "stream_lockstep_stall: cell {} was not read within the wait cap; this run is not deterministic",
                            front.ticket.cell
                        );
                    }
                    if let Some(entry) = self.issued.front_mut() {
                        entry.stalled = true;
                    }
                    break;
                }
            }
        }
    }

    /// Whether `ticket` is the load its cell is waiting for.
    fn is_current(&self, ticket: Ticket) -> bool {
        self.levels
            .get(&ticket.level)
            .and_then(|l| l.cells.get(&ticket.cell))
            .is_some_and(|c| c.generation == ticket.generation && c.state == CellState::Loading)
    }

    /// Lands held results of reads nothing waits for any more: cancelled after they arrived, or
    /// cancelled under async timing, so never in the lockstep order. Their slots and bytes come back.
    fn sweep_arrived(&mut self, spare_issued: bool) {
        let orphans: Vec<Ticket> = self
            .arrived
            .keys()
            .copied()
            .filter(|ticket| {
                !self.is_current(*ticket)
                    && !(spare_issued && self.issued.iter().any(|i| i.ticket == *ticket))
            })
            .collect();
        for ticket in orphans {
            if let Some(outcome) = self.arrived.remove(&ticket) {
                self.accept(ticket, outcome);
            }
        }
    }

    /// The outcome of `ticket`, waiting at most `cap`.
    fn wait_for(&mut self, ticket: Ticket, cap: Duration) -> Option<CellOutcome> {
        while let Ok(result) = self.rx.try_recv() {
            self.arrived.insert(result.ticket, result.outcome);
        }
        if let Some(outcome) = self.arrived.remove(&ticket) {
            return Some(outcome);
        }
        if cap.is_zero() {
            return None;
        }
        self.log.stats.main_thread_io_waits += 1;
        let deadline = Instant::now() + cap;
        loop {
            match self.rx.recv_deadline(deadline) {
                Ok(result) if result.ticket == ticket => return Some(result.outcome),
                Ok(result) => {
                    self.arrived.insert(result.ticket, result.outcome);
                }
                Err(_) => return None,
            }
        }
    }

    fn accept(&mut self, ticket: Ticket, outcome: CellOutcome) {
        let Some(level) = self.levels.get_mut(&ticket.level) else {
            self.log.stats.stale_discards += 1;
            return;
        };
        // A read of a closed instance of this level: nothing here counted it.
        if ticket.generation <= level.opened_at_generation {
            self.log.stats.stale_discards += 1;
            return;
        }
        level.inflight = level.inflight.saturating_sub(1);
        let own = level.own_bytes(ticket.cell);
        let current = level
            .cells
            .get(&ticket.cell)
            .is_some_and(|c| c.generation == ticket.generation && c.state == CellState::Loading);
        if !current {
            // Its own reservation only, and only if a replayed failure has not released it.
            let released = level
                .cells
                .get_mut(&ticket.cell)
                .is_some_and(|rt| rt.take_reservation(ticket.generation));
            if released {
                level.pending_bytes = level.pending_bytes.saturating_sub(own);
            }
            Self::invalidate_plan(level);
            self.log.stats.stale_discards += 1;
            return;
        }
        match outcome {
            CellOutcome::Ready { blob, bytes } => {
                level.with_cell(ticket.cell, |rt| {
                    rt.cancel = None;
                    rt.ranges = blob.scene.hierarchy_ranges();
                    rt.next_range = 0;
                    rt.blob = Some(blob);
                    rt.measured = bytes;
                    rt.state = CellState::Applying;
                });
                level.apply.push_back(ticket.cell);
                // The cell is already in the planner's loaded set. A fresh plan is due, and a
                // rate-limit tail from the same view still stands.
                level.plan_due = true;
            }
            CellOutcome::Failed(reason) => {
                Self::fail_cell(level, &mut self.log, ticket.cell, reason, true);
            }
            CellOutcome::Abandoned => {
                Self::fail_cell(level, &mut self.log, ticket.cell, "cell_missing", true);
            }
        }
    }

    /// `StreamSet::Apply`: despawns and spawns within the budget, then writes the messages.
    ///
    /// `Unloading` cells were announced on an earlier run, so a game system got to read their
    /// members. Async timing spends the time budget, with the entity budget as a floor so progress
    /// never stops; lockstep and replay spend the entity budget only, so the result does not depend
    /// on how fast the machine is.
    pub(crate) fn apply_step(&mut self, world: &mut World, origin: &FloatingOrigin) {
        let started = Instant::now();
        let in_step = self.timing.in_fixed_step();
        let replay = matches!(self.timing, StreamTiming::Replay(_));
        let mut applied = 0u32;
        let mut members_changed = false;
        for level in self.levels.values_mut() {
            // Replay applies a cell in full at the step of its event.
            let budget = if replay {
                u32::MAX
            } else {
                level.budgets.apply_budget_entities
            };
            let time_left =
                |started: &Instant, ms: f32| !in_step && started.elapsed().as_secs_f32() * 1000.0 < ms;
            let ms = level.budgets.apply_budget_ms;

            // A replay announces first: the run it repeats may have done both in one step.
            if replay {
                Self::announce_unloads(level, &mut self.log);
            }

            // Despawn what an earlier run announced.
            let mut despawn_ended_run = false;
            while let Some(&cell) = level.despawn.front() {
                if replay && !self.replay_release.remove(&(level.id, cell)) {
                    break;
                }
                if !level.cells.contains_key(&cell) {
                    level.despawn.pop_front();
                    continue;
                }
                let mut blocked = false;
                let mut end_run = false;
                while let Some(rt) = level.cells.get_mut(&cell) {
                    let Some(&(root, count)) = rt.roots.last() else {
                        break;
                    };
                    match apply_batch(applied, count, budget, time_left(&started, ms)) {
                        ApplyBatch::Stop => {
                            blocked = true;
                            break;
                        }
                        ApplyBatch::TakeOverBudget => {
                            self.log.stats.apply_over_budget += 1;
                            end_run = true;
                        }
                        ApplyBatch::Take => {}
                    }
                    rt.pop_root();
                    // Gameplay may have despawned it first.
                    if let Ok(entity) = world.get_entity_mut(root) {
                        entity.despawn();
                    }
                    applied += count;
                    members_changed = true;
                    if end_run {
                        break;
                    }
                }
                if blocked || level.cells.get(&cell).is_some_and(|rt| !rt.roots.is_empty()) {
                    // An over-budget hierarchy ends the run even when the cell has more left:
                    // no announce and no spawn this step (spec 78 § 8.4).
                    despawn_ended_run = end_run;
                    break;
                }
                // The cell is emptied either way: an over-budget batch still finalizes it.
                level.despawn.pop_front();
                // A cell that failed halfway through its spawn stays failed.
                let unloading = level
                    .cells
                    .get(&cell)
                    .is_some_and(|rt| rt.state == CellState::Unloading);
                level.with_cell(cell, |rt| {
                    if unloading {
                        rt.state = CellState::Unloaded;
                    }
                    rt.clear_entities();
                    rt.measured = 0;
                });
                members_changed = true;
                let _ = level.planner.set_draining(cell, false);
                // The cell can be a load candidate again, so the rate-limit tail is stale.
                Self::invalidate_plan(level);
                self.log
                    .event(Some(level.id), Some(cell), TraceKind::Unloaded, None);
                if let Some(mut gates) = world.get_resource_mut::<StreamGates>() {
                    gates.forget_cell(level.id, cell);
                }
                if end_run {
                    // The over-budget despawn is the last batch of this run: nothing else
                    // applies this step (spec 78 § 8.4).
                    despawn_ended_run = true;
                    break;
                }
            }
            if despawn_ended_run {
                continue;
            }

            // Announce this run's unloads; they despawn from the next run on.
            Self::announce_unloads(level, &mut self.log);

            // Spawn whole hierarchies, in order.
            let mut spawned_any = false;
            'cells: while let Some(&cell) = level.apply.front() {
                let anchor;
                let blob = match level.cells.get(&cell).and_then(|c| c.blob.clone()) {
                    Some(blob) => {
                        anchor = blob.anchor;
                        blob
                    }
                    None => {
                        level.apply.pop_front();
                        continue;
                    }
                };
                let opts = SpawnOptions {
                    level: Some(level.id),
                    ..SpawnOptions::default()
                };
                let remap = |mut t: regia_scene::Transform| {
                    let local = origin.to_local(&LargePosition {
                        cell: anchor,
                        local: t.translation.to_array(),
                    });
                    t.translation = Vec3::from_array(local);
                    t
                };
                let mut end_run = false;
                while let Some(rt) = level.cells.get_mut(&cell) {
                    let Some(range) = rt.ranges.get(rt.next_range).cloned() else {
                        break;
                    };
                    let size = range.len() as u32;
                    let over_budget = match apply_batch(applied, size, budget, time_left(&started, ms)) {
                        ApplyBatch::Stop => break 'cells,
                        ApplyBatch::TakeOverBudget => true,
                        ApplyBatch::Take => false,
                    };
                    if over_budget {
                        self.log.stats.apply_over_budget += 1;
                    }
                    rt.next_range += 1;
                    match blob.scene.spawn_range(world, range, &opts, &remap) {
                        Ok(spawned) => {
                            applied += spawned.entities.len() as u32;
                            spawned_any = true;
                            for warning in &spawned.scene.warnings {
                                tracing::warn!(target: "regia_world", "cell {cell}: {warning}");
                            }
                            for entity in &spawned.entities {
                                if let Ok(mut e) = world.get_entity_mut(*entity) {
                                    e.insert(CellMember(cell));
                                }
                            }
                            // Layer rules apply to cell members (spec 78 § 10.1, ADR-7411): a
                            // member of a Parked or Absent layer arrives parked.
                            for entity in &spawned.entities {
                                if world
                                    .get::<regia_scene::layers::LayerMembership>(*entity)
                                    .is_some()
                                {
                                    regia_scene::layers::apply_member_presence(world, *entity);
                                }
                            }
                            let count = spawned.entities.len() as u32;
                            let roots = spawned.scene.roots.len() as u32;
                            for (i, root) in spawned.scene.roots.iter().enumerate() {
                                // A batch is one hierarchy; split the count if a merged range had more.
                                let share = if i == 0 { count - (roots - 1).min(count) } else { 1 };
                                rt.push_root(*root, share);
                            }
                            rt.extend_entities(spawned.entities);
                            members_changed = true;
                        }
                        Err(error) => {
                            tracing::warn!(target: "regia_world", "cell {cell}: spawn failed: {error}");
                            level.apply.pop_front();
                            let held = !rt.roots.is_empty();
                            if held {
                                level.despawn.push_back(cell);
                            }
                            Self::fail_cell(level, &mut self.log, cell, "cell_decode", true);
                            if over_budget {
                                break 'cells;
                            }
                            continue 'cells;
                        }
                    }
                    if over_budget {
                        end_run = true;
                        break;
                    }
                }
                if end_run
                    && level
                        .cells
                        .get(&cell)
                        .is_some_and(|rt| rt.next_range < rt.ranges.len())
                {
                    break 'cells;
                }
                // The cell finished.
                level.apply.pop_front();
                let own = level.own_bytes(cell);
                let released = level.cells.get_mut(&cell).is_some_and(|rt| {
                    let generation = rt.generation;
                    rt.take_reservation(generation)
                });
                if released {
                    level.pending_bytes = level.pending_bytes.saturating_sub(own);
                }
                let nav = blob.nav.as_deref().map(std::sync::Arc::<[u8]>::from);
                let Some(entities) = level.with_cell(cell, |rt| {
                    rt.blob = None;
                    rt.ranges.clear();
                    rt.state = CellState::Spawned;
                    rt.entities.clone()
                }) else {
                    continue;
                };
                self.log
                    .event(Some(level.id), Some(cell), TraceKind::Applied, None);
                self.log.out.push(Outgoing::Loaded(CellLoaded {
                    level: level.id,
                    cell,
                    entities,
                    nav,
                }));
                if let Some(mut gates) = world.get_resource_mut::<StreamGates>() {
                    gates.note_spawned(level.id, cell);
                }
                level.readiness_dirty = true;
                level.plan_due = true;
                if end_run {
                    break;
                }
            }
            if spawned_any {
                level.readiness_dirty = true;
            }
        }
        if members_changed {
            self.hlod_members_dirty = true;
        }
        self.pump_after_apply();

        self.log.stats.last_apply_entities = applied;
        self.log.stats.peak_apply_entities = self.log.stats.peak_apply_entities.max(applied);
        self.log.stats.last_apply_ms = started.elapsed().as_secs_f32() * 1000.0;
        self.refresh_readiness(world);
        self.flush_messages(world);
        self.refresh_stats(world);
        self.note_peaks();
    }

    /// Folds this step's in-flight bytes and reads into the high-water marks of [`StreamStats`].
    fn note_peaks(&mut self) {
        let stats = &mut self.log.stats;
        stats.peak_inflight = stats.peak_inflight.max(stats.inflight);
        stats.peak_pending_bytes = stats.peak_pending_bytes.max(stats.pending_bytes);
        let reads = stats.loads_issued.saturating_sub(self.log.loads_mark);
        self.log.loads_mark = stats.loads_issued;
        stats.peak_loads_per_step = stats
            .peak_loads_per_step
            .max(reads.min(u64::from(u32::MAX)) as u32);
    }

    /// Writes `CellUnloading` for the cells that started unloading and hands them to the despawn.
    fn announce_unloads(level: &mut LevelRt, log: &mut Log) {
        for cell in std::mem::take(&mut level.announce) {
            log.out.push(Outgoing::Unloading(CellUnloading {
                level: level.id,
                cell,
            }));
            level.despawn.push_back(cell);
        }
    }

    /// Memory came free: in async timing the next read need not wait for a step.
    fn pump_after_apply(&mut self) {
        if !self.timing.in_fixed_step() {
            self.pump_io();
        }
    }

    /// Moves cells between `Spawned`, `SimReady` and `Visible` as gates mark and clear them.
    fn refresh_readiness(&mut self, world: &World) {
        let Some(gates) = world.get_resource::<StreamGates>() else {
            return;
        };
        let revision = gates.revision();
        let sim_only = self.sim_only;
        for level in self.levels.values_mut() {
            if !level.readiness_dirty && level.gate_revision == revision {
                continue;
            }
            level.readiness_dirty = false;
            level.gate_revision = revision;
            let id = level.id;
            let cells: Vec<CellCoord> = level.spawned_cells.iter().copied().collect();
            for cell in cells {
                let transition = level.with_cell(cell, |rt| {
                    if !rt.state.is_spawned() {
                        return None;
                    }
                    let sim = gates.is_ready(id, cell, Readiness::SimReady);
                    let visible = sim && !sim_only && gates.is_ready(id, cell, Readiness::Visible);
                    let target = if visible {
                        CellState::Visible
                    } else if sim {
                        CellState::SimReady
                    } else {
                        CellState::Spawned
                    };
                    if target == rt.state {
                        return None;
                    }
                    let was_sim = rt.state.reached(Readiness::SimReady);
                    let was_visible = rt.state.reached(Readiness::Visible);
                    rt.state = target;
                    Some((sim && !was_sim, visible && !was_visible))
                });
                let Some(Some((became_sim, became_visible))) = transition else {
                    continue;
                };
                if became_sim {
                    self.log.event(Some(id), Some(cell), TraceKind::SimReady, None);
                    self.log.out.push(Outgoing::Ready(CellReady {
                        level: id,
                        cell,
                        readiness: Readiness::SimReady,
                    }));
                }
                if became_visible {
                    self.log.event(Some(id), Some(cell), TraceKind::Visible, None);
                    self.log.out.push(Outgoing::Ready(CellReady {
                        level: id,
                        cell,
                        readiness: Readiness::Visible,
                    }));
                }
            }
        }
    }

    fn flush_messages(&mut self, world: &mut World) {
        for message in self.log.out.drain(..) {
            match message {
                Outgoing::Loaded(m) => {
                    world.write_message(m);
                }
                Outgoing::Ready(m) => {
                    world.write_message(m);
                }
                Outgoing::Unloading(m) => {
                    world.write_message(m);
                }
                Outgoing::Failed(m) => {
                    world.write_message(m);
                }
            }
        }
    }

    /// Recomputes the live numbers and the pin waits, and publishes [`StreamStats`] as a resource.
    fn refresh_stats(&mut self, world: &mut World) {
        let stats = &mut self.log.stats;
        stats.resident_cells = 0;
        stats.resident_bytes_estimate = 0;
        stats.resident_bytes_measured = 0;
        stats.resident_vram_estimate = 0;
        stats.vram_over_budget = false;
        stats.inflight = 0;
        stats.pending_bytes = 0;
        let load_sets = world.get_resource::<regia_asset::LoadSets>().cloned();
        let mut any_changed = false;
        for (id, level) in &mut self.levels {
            stats.inflight += level.inflight;
            // Byte fields come from content (`CellRecord`): every sum saturates, as the planner's do.
            stats.pending_bytes = stats.pending_bytes.saturating_add(level.pending_bytes);
            stats.resident_bytes_estimate = stats.resident_bytes_estimate.saturating_add(
                level
                    .planner
                    .loaded_bytes()
                    .saturating_add(level.planner.draining_bytes()),
            );
            let set_name = stream_cells_set(*id);
            let defining = load_sets.is_some() && !level.load_set_defined;
            if let Some(sets) = &load_sets
                && !level.load_set_defined
            {
                let mut set = regia_asset::LoadSet::new(set_name.as_str());
                for record in &level.index.cells {
                    let key = format!("{set_name}/{}", record.coord.label());
                    set.push(
                        regia_asset::LoadKind::Other,
                        key,
                        record.own_bytes,
                        record.vram_bytes,
                    );
                }
                sets.define(set);
                level.load_set_defined = true;
            }
            // Tracked only while the load sets exist: a resource that arrives late still sees the
            // cells spawned before it. The first define marks every spawned cell; later steps
            // update only the cells whose spawned-ness changed.
            if let Some(sets) = &load_sets {
                if defining {
                    let spawned: Vec<CellCoord> = level.spawned_cells.iter().copied().collect();
                    for cell in spawned {
                        let key = format!("{set_name}/{}", cell.label());
                        sets.mark_resident(regia_asset::LoadKind::Other, &key);
                        if let Some(rt) = level.cells.get_mut(&cell) {
                            rt.in_load_set = true;
                        }
                    }
                    level.load_set_queue.clear();
                } else {
                    let queued = std::mem::take(&mut level.load_set_queue);
                    for cell in queued {
                        let spawned = level.spawned_cells.contains(&cell);
                        let Some(rt) = level.cells.get_mut(&cell) else {
                            continue;
                        };
                        if rt.in_load_set == spawned {
                            continue;
                        }
                        rt.in_load_set = spawned;
                        let key = format!("{set_name}/{}", cell.label());
                        if spawned {
                            sets.mark_resident(regia_asset::LoadKind::Other, &key);
                        } else {
                            sets.mark_evicted(regia_asset::LoadKind::Other, &key);
                        }
                    }
                }
            }
            stats.resident_cells += level.resident_cells;
            stats.resident_bytes_measured =
                stats.resident_bytes_measured.saturating_add(level.measured_total);
            stats.resident_vram_estimate = stats.resident_vram_estimate.saturating_add(level.vram_total);
            let over_vram =
                level.budgets.vram_budget_bytes > 0 && level.vram_total > level.budgets.vram_budget_bytes;
            stats.vram_over_budget |= over_vram;
            any_changed |= level.stats_changed;
            level.stats_changed = false;
        }
        if any_changed {
            self.pin_waits_met.clear();
        }
        if let Some(gates) = world.get_resource::<StreamGates>() {
            let mut waits = 0;
            // Level open waits for the start sources' ground (spec 78 § 8.7): the guard holds bodies
            // meanwhile, and this wait is what a loading screen watches.
            for level in self.levels.values_mut() {
                if !level.open_wait {
                    continue;
                }
                let radius = level
                    .planner
                    .config()
                    .load_radius
                    .min(level.grid.cell_size() * 2.0);
                // The open wait is a simulation gate (spec 78 § 8.7): it clears when the start
                // cells reach `SimReady`. Proxy residency is presentation (spec 78 § 15.1) and
                // never gates it — a peer without the render hook would wait forever, and a peer
                // with slow proxy IO would clear on another step.
                let met = !self.last_points.is_empty()
                    && self.last_points.values().all(|p| {
                        level
                            .grid
                            .cells_within(p[0], p[2], radius, 1 << 22)
                            .is_ok_and(|cells| {
                                cells
                                    .into_iter()
                                    .all(|c| level.cell_ready(gates, c, Readiness::SimReady))
                            })
                    });
                if met {
                    level.open_wait = false;
                } else {
                    waits += 1;
                }
            }
            let mut scans = 0u64;
            let mut newly_met = Vec::new();
            for pin in self.pins.values() {
                if let Some(wait) = pin.wait
                    && !self.levels.is_empty()
                {
                    if self.pin_waits_met.contains(&pin.id) {
                        continue;
                    }
                    let mut met = true;
                    for level in self.levels.values() {
                        scans += 1;
                        let Ok(cells) = level.grid.cells_within(
                            pin.center[0],
                            pin.center[2],
                            wait.radius_m,
                            PIN_WAIT_MAX_CELLS,
                        ) else {
                            // A refused area is never met (fail closed, as `ready_near`).
                            met = false;
                            continue;
                        };
                        met &= cells
                            .into_iter()
                            .all(|c| level.cell_ready(gates, c, wait.readiness));
                    }
                    if met {
                        newly_met.push(pin.id);
                    } else {
                        waits += 1;
                    }
                }
            }
            self.pin_wait_scans += scans;
            for id in newly_met {
                self.pin_waits_met.insert(id);
            }
            self.log.stats.pending_waits = waits;
        }
        world.insert_resource(self.log.stats.clone());
    }

    /// The budget adjustment of spec 78 § 8.5 (async timing only).
    fn track_memory(&mut self) {
        if !matches!(self.timing, StreamTiming::Async) {
            return;
        }
        let mut adjusted = false;
        for level in self.levels.values_mut() {
            let configured = level.budgets.memory_budget_bytes;
            let measured = level.measured_total;
            if measured.saturating_mul(10) > configured.saturating_mul(11) {
                level.over_budget_steps += 1;
            } else {
                level.over_budget_steps = 0;
            }
            if level.over_budget_steps >= OVER_BUDGET_STEPS {
                level.over_budget_steps = 0;
                let lowered = configured
                    .saturating_sub(measured.saturating_sub(configured))
                    .max(configured / 2);
                if lowered != level.effective_budget {
                    tracing::warn!(
                        target: "regia_world",
                        "stream_budget_adjusted: measured {measured} bytes over {configured}; planning with {lowered}"
                    );
                    level.effective_budget = lowered;
                    adjusted = true;
                }
            }
        }
        if adjusted {
            self.apply_trim();
        }
    }

    /// Replay timing: runs the trace's events of this step.
    fn replay_step(&mut self, events: &[TraceEvent], step: u64) {
        while let Some(event) = events.get(self.replay_cursor) {
            if event.step > step {
                break;
            }
            self.replay_cursor += 1;
            let (Some(id), Some(cell)) = (event.level, event.cell) else {
                continue;
            };
            let Some(level) = self.levels.get_mut(&id) else {
                continue;
            };
            match event.kind {
                TraceKind::Queued => {
                    let ready = level
                        .cells
                        .get(&cell)
                        .is_some_and(|rt| matches!(rt.state, CellState::Unloaded | CellState::Failed(_)));
                    if ready {
                        let generation = self.log.next_generation();
                        level.with_cell(cell, |rt| {
                            if matches!(rt.state, CellState::Unloaded | CellState::Failed(_)) {
                                rt.generation = generation;
                                rt.state = CellState::Queued;
                            }
                        });
                        level.queue.push_back(cell);
                        let _ = level.planner.set_loaded(cell, true);
                        self.log.event(Some(id), Some(cell), TraceKind::Queued, None);
                    }
                }
                TraceKind::Applied => {
                    self.replay_due.push_back(ReplayDue {
                        due: event.step,
                        level: id,
                        cell,
                        stalled: false,
                    });
                }
                TraceKind::Failed => {
                    // Land the recorded failure at the recorded step. This replay's read may not
                    // exist yet (the trace queues and fails in one step) or still be in flight
                    // (its IO timing need not match the recording's): cancel what there is and
                    // fail the cell now, so a retry the trace records still applies. The failure
                    // releases the read's pending-bytes reservation now, as the recording did at
                    // this step; the cancelled read's late stale result then frees only its
                    // `inflight` slot (its generation is no longer reserved, and the retry's is
                    // its own). The `Queued` branch has no read and passes `false`.
                    match level.cells.get(&cell).map(|rt| rt.state.clone()) {
                        Some(CellState::Loading) => {
                            if let Some(cancel) = level.cells.get_mut(&cell).and_then(|rt| rt.cancel.take()) {
                                cancel.cancel();
                            }
                            let reason = event.detail.as_deref().unwrap_or("cell_missing");
                            Self::fail_cell(level, &mut self.log, cell, reason, true);
                        }
                        Some(CellState::Queued) => {
                            level.queue.retain(|queued| *queued != cell);
                            let reason = event.detail.as_deref().unwrap_or("cell_missing");
                            Self::fail_cell(level, &mut self.log, cell, reason, false);
                        }
                        _ => {}
                    }
                }
                TraceKind::Unloaded => {
                    self.replay_release.insert((id, cell));
                }
                TraceKind::Unloading | TraceKind::Cancelled => {
                    if Self::begin_unload(level, &mut self.log, cell) {
                        let _ = level.planner.set_draining(cell, true);
                    } else {
                        let _ = level.planner.set_loaded(cell, false);
                    }
                }
                _ => {}
            }
        }
    }

    /// Replay timing: waits for the reads of the cells that apply at this step and hands them to the
    /// apply step in full.
    pub(crate) fn replay_complete(&mut self) {
        let step = self.log.stats.step;
        // Results of cancelled reads free their slots now; the rest wait for their step.
        while let Ok(result) = self.rx.try_recv() {
            // A result of a read its cell still waits on waits for its step, a good read as much
            // as a failure: the trace lands a recorded failure at the recorded step, and a
            // failure no recorded event waits for drains at the recorded apply step or once its
            // ticket goes stale.
            if self.is_current(result.ticket) {
                self.arrived.insert(result.ticket, result.outcome);
            } else {
                self.accept(result.ticket, result.outcome);
            }
        }
        self.sweep_arrived(false);
        while let Some(front) = self.replay_due.front().copied() {
            if front.due > step {
                break;
            }
            let Some(level) = self.levels.get(&front.level) else {
                self.replay_due.pop_front();
                continue;
            };
            let Some(rt) = level.cells.get(&front.cell) else {
                self.replay_due.pop_front();
                continue;
            };
            if rt.state != CellState::Loading {
                // The cell failed, retried or finished another way: no read to wait for.
                self.replay_due.pop_front();
                continue;
            }
            let ticket = Ticket {
                level: front.level,
                cell: front.cell,
                generation: rt.generation,
            };
            let cap = if front.stalled {
                Duration::ZERO
            } else {
                Duration::from_millis(u64::from(level.tuning.lockstep_wait_cap_ms))
            };
            match self.wait_for(ticket, cap) {
                Some(outcome) => {
                    self.replay_due.pop_front();
                    self.accept(ticket, outcome);
                }
                None => {
                    // Keep the entry due, the way the lockstep issued queue does: a later step
                    // polls and applies the read when it shows up, instead of dropping the apply
                    // and stranding the cell in `Loading` forever.
                    if !front.stalled {
                        self.log.stats.lockstep_stalls += 1;
                        tracing::error!(target: "regia_world", "stream_lockstep_stall: replay of cell {}", front.cell);
                        if let Some(entry) = self.replay_due.front_mut() {
                            entry.stalled = true;
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Whether the cell at the local position is ready, with a margin around it (the guard's check).
    pub(crate) fn guard_ready(&self, gates: &StreamGates, x: f64, z: f64) -> bool {
        for level in self.levels.values() {
            // Each level freezes bodies by its own cooked margin.
            let margin = f64::from(level.tuning.guard_margin_m);
            let (Ok(lo), Ok(hi)) = (
                level.grid.cell_of(x - margin, z - margin),
                level.grid.cell_of(x + margin, z + margin),
            ) else {
                continue;
            };
            for cx in lo.x..=hi.x {
                for cz in lo.z..=hi.z {
                    if !level.cell_ready(gates, CellCoord::new(cx, cz), Readiness::SimReady) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The cell a root would move to, or `None` when it stays where it is (spec 78 § 10.5).
    ///
    /// The guard calls this before walking the root's children: a member that has not crossed a
    /// cell boundary has no list to update.
    pub(crate) fn member_destination(
        &self,
        entity: Entity,
        from: Option<CellCoord>,
        x: f64,
        z: f64,
    ) -> Option<CellCoord> {
        // Every open grid still names `from` for this position, including no levels at all.
        // The body has not crossed a cell, so the root lists stay unread.
        if self
            .levels
            .values()
            .all(|level| level.grid.cell_of(x, z).ok() == from)
        {
            return None;
        }
        // Same level choice as `move_member`: the level whose cell lists the root, else the first
        // level, in `LevelId` order, whose cell at the position is spawned.
        let owner = from.and_then(|from_cell| {
            self.levels
                .iter()
                .find(|(_, level)| {
                    level
                        .cells
                        .get(&from_cell)
                        .is_some_and(|rt| rt.root_index.contains_key(&entity))
                })
                .map(|(id, _)| *id)
        });
        let level = match owner {
            Some(id) => self.levels.get(&id)?,
            None => self.levels.values().find(|level| {
                level
                    .grid
                    .cell_of(x, z)
                    .ok()
                    .and_then(|to| level.cells.get(&to))
                    .is_some_and(|rt| rt.state.is_spawned())
            })?,
        };
        let to = level.grid.cell_of(x, z).ok()?;
        if from == Some(to) || !level.cells.get(&to)?.state.is_spawned() {
            return None;
        }
        Some(to)
    }

    /// Moves a dynamic root to the cell it now stands in, when that cell is spawned. Returns the new
    /// cell if the member moved (spec 78 § 10.5).
    pub(crate) fn move_member(
        &mut self,
        entity: Entity,
        descendants: &[Entity],
        from: Option<CellCoord>,
        x: f64,
        z: f64,
    ) -> Option<CellCoord> {
        // The member moves inside the level whose cell lists it (audit v033b: with two levels open,
        // the move always edited the first one, so the old level kept the entity and despawned the
        // live body when that cell unloaded). An entity no level lists yet (an auto member) joins
        // the first level, in `LevelId` order, whose cell at the position is spawned.
        // Every open grid still names `from` for this position, including no levels at all.
        // The body has not crossed a cell, so the root lists stay unread.
        if self
            .levels
            .values()
            .all(|level| level.grid.cell_of(x, z).ok() == from)
        {
            return None;
        }
        let mut probes = 0u64;
        let owner = from.and_then(|from_cell| {
            for (id, level) in &self.levels {
                probes += 1;
                if level
                    .cells
                    .get(&from_cell)
                    .is_some_and(|rt| rt.root_index.contains_key(&entity))
                {
                    return Some(*id);
                }
            }
            None
        });
        self.guard_root_visits += probes;
        let moved = {
            let level = match owner {
                Some(id) => self.levels.get_mut(&id)?,
                None => self.levels.values_mut().find(|level| {
                    level
                        .grid
                        .cell_of(x, z)
                        .ok()
                        .and_then(|to| level.cells.get(&to))
                        .is_some_and(|rt| rt.state.is_spawned())
                })?,
            };
            let to = level.grid.cell_of(x, z).ok()?;
            if from == Some(to) || !level.cells.get(&to)?.state.is_spawned() {
                return None;
            }
            let mut count = 1;
            if let Some(from_cell) = from
                && let Some(rt) = level.cells.get_mut(&from_cell)
            {
                if let Some(n) = rt.take_root(entity) {
                    count = n;
                }
                rt.take_entity(entity);
                for e in descendants {
                    rt.take_entity(*e);
                }
            }
            let rt = level.cells.get_mut(&to)?;
            rt.push_root(entity, count);
            rt.push_entity(entity);
            rt.extend_entities(descendants.iter().copied());
            Some(to)
        };
        if moved.is_some() {
            self.hlod_members_dirty = true;
        }
        moved
    }
}

/// Data address of a cell source. Two `Arc`s of one allocation compare equal.
fn source_addr(source: &Arc<dyn CellSource>) -> usize {
    Arc::as_ptr(source) as *const () as usize
}

#[cfg(test)]
mod apply_batch_tests {
    use super::{ApplyBatch, apply_batch};

    #[test]
    fn an_over_budget_hierarchy_waits_until_it_is_the_only_batch() {
        // Async: time remains, so a hierarchy that fits still starts past the entity floor.
        assert!(matches!(apply_batch(0, 4, 10, true), ApplyBatch::Take));
        assert!(matches!(apply_batch(4, 4, 10, true), ApplyBatch::Take));
        assert!(matches!(apply_batch(8, 4, 10, true), ApplyBatch::Take));
        // The 20-entity hierarchy does not join a run that already applied the 4.
        assert!(matches!(apply_batch(4, 20, 10, true), ApplyBatch::Stop));
        // Alone, it is taken and counted, and the run ends after it.
        assert!(matches!(apply_batch(0, 20, 10, true), ApplyBatch::TakeOverBudget));
        // Lockstep (no time left): a fitting hierarchy stops once the sum would pass the budget.
        assert!(matches!(apply_batch(8, 4, 10, false), ApplyBatch::Stop));
        assert!(matches!(apply_batch(4, 4, 10, false), ApplyBatch::Take));
    }
}

/// Opens one empty streamed level so `is_active` is true. The source's step hook does not allocate.
#[cfg(test)]
pub(crate) fn open_empty_level_for_test(streaming: &mut WorldStreaming) {
    struct QuietSource;
    impl CellSource for QuietSource {
        fn read(&self, _id: AssetId, _ticket: Ticket, _cancel: CancelToken, _sink: CellSink) {}
    }

    let level = LevelId(regia_core::id::Guid::from_u128(1));
    let world_index = WorldIndex {
        format_version: crate::format::WORLD_INDEX_FORMAT_VERSION,
        level: level.0,
        content_hash: [0; 32],
        settings: LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        },
        cells: Vec::new(),
        bounds: [CellCoord::new(0, 0), CellCoord::new(0, 0)],
        hlod: None,
        replicated: Vec::new(),
        nav: None,
    };
    streaming
        .open_level(level, world_index, Arc::new(QuietSource))
        .expect("open level");
}

#[cfg(test)]
mod begin_step_alloc_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct HitSource {
        hits: AtomicU64,
    }

    impl CellSource for HitSource {
        fn read(&self, _id: AssetId, _ticket: Ticket, _cancel: CancelToken, _sink: CellSink) {}

        fn on_step(&self, _step: u64) {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn index_for(level: LevelId) -> WorldIndex {
        WorldIndex {
            format_version: crate::format::WORLD_INDEX_FORMAT_VERSION,
            level: level.0,
            content_hash: [0; 32],
            settings: LevelStreaming {
                enabled: true,
                ..LevelStreaming::default()
            },
            cells: Vec::new(),
            bounds: [CellCoord::new(0, 0), CellCoord::new(0, 0)],
            hlod: None,
            replicated: Vec::new(),
            nav: None,
        }
    }

    /// The counting allocator is installed for this crate's unit-test binary.
    ///
    /// `black_box` keeps the box. The dev profile optimises at level 1, and a box that does not
    /// escape is deleted, which would hide whether this allocator is installed.
    #[test]
    fn the_counter_sees_a_heap_allocation() {
        let before = crate::alloc_count::allocations();
        let boxed = std::hint::black_box(Box::new(1_u8));
        assert_eq!(*boxed, 1);
        assert!(
            crate::alloc_count::allocations() > before,
            "the counting allocator is not the global allocator of this test binary"
        );
    }

    /// One open level steps its source without a heap buffer. The dedup list used to be a `Vec`
    /// allocated on every step.
    #[test]
    fn begin_step_allocates_nothing_for_one_open_level() {
        let mut streaming = WorldStreaming::default();
        let source = Arc::new(HitSource {
            hits: AtomicU64::new(0),
        });
        let level = LevelId(regia_core::id::Guid::from_u128(7));
        streaming
            .open_level(level, index_for(level), source.clone())
            .expect("open");
        streaming.begin_step();
        let least = crate::alloc_count::least(32, || streaming.begin_step());
        assert_eq!(least, 0, "steady begin_step allocated {least} times");
        assert_eq!(source.hits.load(Ordering::Relaxed), 33);
    }

    /// Two levels over one source count the step once, including when the stack list spills.
    #[test]
    fn each_distinct_source_is_stepped_once_per_begin_step() {
        let mut streaming = WorldStreaming::default();
        let shared = Arc::new(HitSource {
            hits: AtomicU64::new(0),
        });
        for index in 0..2 {
            let level = LevelId(regia_core::id::Guid::from_u128(index));
            streaming
                .open_level(level, index_for(level), shared.clone())
                .expect("open");
        }
        streaming.begin_step();
        assert_eq!(shared.hits.load(Ordering::Relaxed), 1);

        let mut many = WorldStreaming::default();
        let mut sources = Vec::new();
        for index in 0..9 {
            let level = LevelId(regia_core::id::Guid::from_u128(100 + index));
            let source = Arc::new(HitSource {
                hits: AtomicU64::new(0),
            });
            many.open_level(level, index_for(level), source.clone())
                .expect("open");
            sources.push(source);
        }
        many.begin_step();
        assert!(
            sources
                .iter()
                .all(|source| source.hits.load(Ordering::Relaxed) == 1)
        );
    }
}

#[cfg(test)]
mod pin_wait_tests {
    use super::*;

    /// rev-zai-rc-b terrain 4 (w9-pack-stamp-and-content): a pin wait whose area the grid refuses
    /// to list (its bounding box passes the cell cap, or its corners leave the grid's range) can
    /// never be met, so `pending_waits` stayed up until the caller's own timeout. The wait is
    /// refused when it is set, and the pin keeps the wait it had.
    #[test]
    fn a_pin_wait_the_grid_refuses_is_refused_when_set() {
        let mut streaming = WorldStreaming::default();
        open_empty_level_for_test(&mut streaming);
        let pin = streaming.pin("far", [0.0; 3], 1.0, 0, None).expect("pin");
        let near = PinWait {
            radius_m: 10.0,
            readiness: Readiness::SimReady,
        };
        streaming
            .set_pin_wait(pin, Some(near))
            .expect("a small wait is fine");
        let error = streaming
            .set_pin_wait(
                pin,
                Some(PinWait {
                    radius_m: 1.0e9,
                    readiness: Readiness::SimReady,
                }),
            )
            .expect_err("a wait over an area the grid refuses was accepted");
        assert_eq!(error.reason(), "stream_wait_area_refused");
        assert_eq!(
            streaming.pin_info(pin).and_then(|p| p.wait),
            Some(near),
            "the refused wait replaced the pin's wait"
        );
        // Clearing a wait is always fine.
        streaming.set_pin_wait(pin, None).expect("clear");

        // A teleport target the grid cannot list cells around leaves no pin behind.
        let before = streaming.pins().count();
        let error = streaming
            .prepare_teleport([1.0e300, 0.0, 0.0])
            .expect_err("a teleport outside the grid's range was accepted");
        assert_eq!(error.reason(), "stream_wait_area_refused");
        assert_eq!(
            streaming.pins().count(),
            before,
            "the refused teleport left its pin"
        );
        streaming
            .prepare_teleport([5.0, 0.0, 5.0])
            .expect("a teleport inside the grid");
    }
}
