// SPDX-License-Identifier: MPL-2.0
//! The streaming planner: which cells to load and unload for the current viewer positions.
//!
//! The planner is a pure decision function over a registry of cells (each with a memory cost and a
//! priority) and the set of cells that are currently loaded. [`StreamPlanner::plan`] reads the
//! viewer positions and returns a [`StreamPlan`]; it never loads anything itself. The host
//! performs the loads and unloads and then calls [`StreamPlanner::apply`] to record them, so the
//! planner stays testable without any asset system. See `book/agents/world-streaming.md`.
//!
//! # Rules
//!
//! - **Distance with hysteresis.** A cell that is not loaded becomes a load candidate when its
//!   distance to the nearest viewer is at most `load_radius`. A loaded cell is unloaded only when
//!   that distance exceeds `unload_radius`, which must be at least `load_radius`. A cell in the band
//!   between the two keeps its current state, so a viewer walking along the edge does not make a
//!   cell flicker.
//! - **Priority order.** Candidates are ordered by higher `priority` first, then nearer first, then
//!   by cell coordinate, so equal inputs always give the same order.
//! - **Memory budget.** Resident bytes never exceed `memory_budget_bytes` after a plan is applied.
//!   A candidate that does not fit may evict loaded cells that are beyond `load_radius` (farthest
//!   first); if even that is not enough it is deferred. A budget lowered below the resident size
//!   evicts the farthest loaded cells until the rest fit.
//! - **Rate limit.** At most `max_loads_per_step` loads are ordered per plan (0 means no limit); the
//!   rest are reported as deferred and will be candidates again next step.
//! - **VRAM bound (opt-in).** With [`StreamPlanner::set_vram_budget`] set, the cells' VRAM estimates
//!   ([`StreamPlanner::set_cell_vram`]) are a second hard bound with the same rules as the memory
//!   budget: a candidate must fit both, eviction frees both, and a lowered bound sheds the farthest
//!   cells (spec 78 § 8.4, ADR-2317). Without it VRAM is not planned.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

thread_local! {
    /// Registered cells the last [`StreamPlanner::plan`] or [`StreamPlanner::plan_views`] on this
    /// thread considered while looking for loads.
    static PLAN_CELLS_VISITED: Cell<u64> = const { Cell::new(0) };
    /// Times that plan summed the evictable ring. One plan sums it once.
    static PLAN_RING_SUMS: Cell<u64> = const { Cell::new(0) };
}

/// Registered cells the last plan on this thread considered while looking for loads.
#[must_use]
pub fn last_plan_cells_visited() -> u64 {
    PLAN_CELLS_VISITED.get()
}

/// Times the last plan on this thread summed the evictable ring.
#[must_use]
pub fn last_plan_ring_sums() -> u64 {
    PLAN_RING_SUMS.get()
}

/// Cells one viewer's unload window may cover before a plan scans the whole registry instead.
/// The world runtime uses the same cap, so a normal radius stays a window and a huge one still
/// returns a plan.
const PLAN_WINDOW_MAX_CELLS: u64 = 1 << 22;

use serde::{Deserialize, Serialize};

use crate::stream_grid::{CellCoord, StreamError, StreamGrid};

/// Distances and budgets that drive [`StreamPlanner::plan`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    /// A cell within this distance of a viewer is loaded, meters.
    pub load_radius: f64,
    /// A loaded cell beyond this distance from every viewer is unloaded, meters. At least `load_radius`.
    pub unload_radius: f64,
    /// The most bytes the loaded cells may occupy.
    pub memory_budget_bytes: u64,
    /// The most loads one plan may order; 0 means no limit.
    pub max_loads_per_step: u32,
}

impl StreamConfig {
    /// Check the radii: finite, not negative, and `unload_radius >= load_radius`.
    pub fn validate(&self) -> Result<(), StreamError> {
        for (what, value) in [
            ("load_radius", self.load_radius),
            ("unload_radius", self.unload_radius),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(StreamError::BadRadius { what, value });
            }
        }
        if self.unload_radius < self.load_radius {
            return Err(StreamError::NoHysteresis {
                load: self.load_radius,
                unload: self.unload_radius,
            });
        }
        Ok(())
    }
}

/// What the planner knows about one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellInfo {
    /// Memory the cell occupies while loaded.
    pub bytes: u64,
    /// Higher loads first among candidates; 0 is the default.
    pub priority: i32,
}

/// Why a cell is being unloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnloadReason {
    /// It is beyond the unload radius of every viewer.
    Distance,
    /// The loaded set exceeded the memory budget (for example the budget was lowered).
    OverBudget,
    /// It is outside the load radius and was evicted to fit a nearer or higher priority cell.
    MakeRoom,
}

/// Why a load candidate was not ordered this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferReason {
    /// The step's load limit was reached.
    RateLimited,
    /// The cell alone is larger than the whole budget, so it can never load.
    TooLarge,
    /// It does not fit even after evicting every cell that may be evicted.
    OverBudget,
}

/// A cell to load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadOrder {
    /// The cell.
    pub cell: CellCoord,
    /// Distance to the nearest viewer, meters.
    pub distance: f64,
    /// Memory it will occupy.
    pub bytes: u64,
    /// Its priority.
    pub priority: i32,
}

/// A cell to unload.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UnloadOrder {
    /// The cell.
    pub cell: CellCoord,
    /// Distance to the nearest viewer, meters.
    pub distance: f64,
    /// Memory it frees.
    pub bytes: u64,
    /// Why.
    pub reason: UnloadReason,
}

/// A load candidate that was not ordered, with the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredLoad {
    /// The cell.
    pub cell: CellCoord,
    /// Why it waits.
    pub reason: DeferReason,
}

/// The result of one planning step. Unloads come first and should be performed before the loads;
/// loads are in priority order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StreamPlan {
    /// Cells to unload.
    pub unloads: Vec<UnloadOrder>,
    /// Cells to load, highest priority first.
    pub loads: Vec<LoadOrder>,
    /// Candidates that did not make it this step.
    pub deferred: Vec<DeferredLoad>,
    /// Resident bytes once the plan is applied.
    pub resident_bytes_after: u64,
}

impl StreamPlan {
    /// True when the plan changes nothing.
    pub fn is_empty(&self) -> bool {
        self.unloads.is_empty() && self.loads.is_empty()
    }
}

/// Who a [`SourceView`] belongs to. The derived order is the order views are sorted in, so equal
/// inputs give equal plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SourceId {
    /// A source entity with a stable guid (the guid as an integer).
    Guid(u128),
    /// A source entity with no guid (entity bits; stable within one run only).
    Entity(u64),
    /// A scripted pin.
    Pin(u64),
    /// The implicit primary view of a streamed level with no enabled source.
    Implicit,
}

/// One point a streaming source wants content around, sampled for one planning step. A source with
/// `predict_s > 0` contributes two views with the same [`SourceId`] (spec 78 § 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceView {
    /// The source.
    pub source: SourceId,
    /// World position, meters.
    pub pos: [f64; 3],
    /// View direction, if the source has one; only ordering reads it.
    pub dir: Option<[f32; 3]>,
    /// Divides distances: a source with scale 2 wants cells twice as far. Must be above zero.
    pub radius_scale: f32,
    /// Added to the priority of every cell this source wants.
    pub priority: i32,
    /// How much the view direction pulls a cell forward in the load order, `0..=1`.
    pub view_bias: f32,
}

impl SourceView {
    /// A view at `pos` with scale 1, priority 0 and no direction.
    pub fn at(source: SourceId, pos: [f64; 3]) -> Self {
        Self {
            source,
            pos,
            dir: None,
            radius_scale: 1.0,
            priority: 0,
            view_bias: 0.0,
        }
    }
}

/// The streaming decision state: the grid, the cells, and which of them are loaded.
#[derive(Debug, Clone)]
pub struct StreamPlanner {
    grid: StreamGrid,
    config: StreamConfig,
    cells: BTreeMap<CellCoord, CellInfo>,
    loaded: BTreeSet<CellCoord>,
    /// Never load candidates while they are here (failed cells waiting for their retry step).
    held: BTreeSet<CellCoord>,
    /// Cells on their way out: no longer loaded, but the host still holds their memory.
    draining: BTreeSet<CellCoord>,
    /// VRAM estimate per cell; a cell with no entry counts 0.
    vram: BTreeMap<CellCoord, u64>,
    /// The VRAM bound; `None` plans memory only.
    vram_budget: Option<u64>,
}

/// The load-order key: priority, in the block around a view, biased distance.
type OrderKey = (i64, bool, f64);

/// A load candidate during planning: the cell, its info, its distance and its order key.
type Candidate = (CellCoord, CellInfo, f64, OrderKey);

/// One view as the planner reads it.
struct PlanPoint {
    x: f64,
    z: f64,
    scale: f64,
    priority: i32,
    bias: f64,
    /// Unit direction on the ground plane.
    dir: Option<[f64; 2]>,
    /// The view's own cell; `None` when the 3 x 3 block rule does not apply ([`StreamPlanner::plan`]).
    cell: Option<CellCoord>,
}

impl PlanPoint {
    /// The cosine of the angle between the view direction and the direction to `target`; 0 with no
    /// direction, and when the view stands on the target.
    fn facing(&self, target: [f64; 2]) -> f64 {
        match (self.dir, flat_direction([target[0] - self.x, target[1] - self.z])) {
            (Some(a), Some(b)) => a[0] * b[0] + a[1] * b[1],
            _ => 0.0,
        }
    }
}

/// `v` as a unit vector, or `None` when it has no usable length.
fn flat_direction(v: [f64; 2]) -> Option<[f64; 2]> {
    let length = v[0].hypot(v[1]);
    (length.is_finite() && length > 1.0e-9).then(|| [v[0] / length, v[1] / length])
}

/// Records `cell` when it is an unloaded load candidate. Split out so the window scan can finish
/// borrowing the registry before this reads the rest of the planner.
fn note_load_candidate(
    planner: &StreamPlanner,
    candidates: &mut Vec<Candidate>,
    viewers: &[PlanPoint],
    load_radius: f64,
    view_order: bool,
    cell: CellCoord,
    info: CellInfo,
) {
    PLAN_CELLS_VISITED.set(PLAN_CELLS_VISITED.get().saturating_add(1));
    if planner.loaded.contains(&cell) || planner.held.contains(&cell) || planner.draining.contains(&cell) {
        return;
    }
    let distance = planner.nearest(viewers, cell);
    if distance <= load_radius {
        let key = if view_order {
            planner.order_key(viewers, cell, info)
        } else {
            (i64::from(info.priority), false, distance)
        };
        candidates.push((cell, info, distance, key));
    }
}

/// A loaded cell during planning.
struct Resident {
    cell: CellCoord,
    distance: f64,
    bytes: u64,
    vram: u64,
}

impl StreamPlanner {
    /// A planner with no cells. The configuration is validated.
    pub fn new(grid: StreamGrid, config: StreamConfig) -> Result<Self, StreamError> {
        config.validate()?;
        Ok(Self {
            grid,
            config,
            cells: BTreeMap::new(),
            loaded: BTreeSet::new(),
            held: BTreeSet::new(),
            draining: BTreeSet::new(),
            vram: BTreeMap::new(),
            vram_budget: None,
        })
    }

    /// Set the VRAM estimate of a registered cell (spec 78 § 8.4). It only counts while a VRAM
    /// budget is set.
    pub fn set_cell_vram(&mut self, cell: CellCoord, bytes: u64) -> Result<(), StreamError> {
        if !self.cells.contains_key(&cell) {
            return Err(StreamError::UnknownCell(cell));
        }
        self.vram.insert(cell, bytes);
        Ok(())
    }

    /// The VRAM estimate of `cell`; 0 when none was set.
    pub fn cell_vram(&self, cell: CellCoord) -> u64 {
        self.vram.get(&cell).copied().unwrap_or(0)
    }

    /// Make VRAM a hard bound of the plan (`Some(bytes)`) or plan memory only (`None`, the
    /// default). Like a lowered memory budget, a lowered bound leaves the loaded set as it is and the
    /// next plan sheds the farthest cells.
    pub fn set_vram_budget(&mut self, budget: Option<u64>) {
        self.vram_budget = budget;
    }

    /// The VRAM bound, if one is set.
    pub fn vram_budget(&self) -> Option<u64> {
        self.vram_budget
    }

    /// VRAM estimate of the loaded cells.
    pub fn loaded_vram(&self) -> u64 {
        self.loaded
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(self.cell_vram(*c)))
    }

    /// VRAM estimate of the draining cells.
    pub fn draining_vram(&self) -> u64 {
        self.draining
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(self.cell_vram(*c)))
    }

    /// The grid.
    pub fn grid(&self) -> &StreamGrid {
        &self.grid
    }

    /// The current configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Replace the configuration (for example to lower the budget). The loaded set is left as it
    /// is; the next plan brings it back within the new limits.
    pub fn set_config(&mut self, config: StreamConfig) -> Result<(), StreamError> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    /// Add a cell with its cost. The cell starts unloaded.
    pub fn register_cell(&mut self, cell: CellCoord, info: CellInfo) -> Result<(), StreamError> {
        if self.cells.contains_key(&cell) {
            return Err(StreamError::DuplicateCell(cell));
        }
        self.cells.insert(cell, info);
        Ok(())
    }

    /// Forget a cell. A loaded cell is also dropped from the loaded set (the host should unload
    /// it). Returns whether the cell existed.
    pub fn remove_cell(&mut self, cell: CellCoord) -> bool {
        self.loaded.remove(&cell);
        self.held.remove(&cell);
        self.draining.remove(&cell);
        self.vram.remove(&cell);
        self.cells.remove(&cell).is_some()
    }

    /// Record the host's actual state of `cell` (for example after a load failed, or when
    /// resynchronizing from a saved state). The budget is not checked: if the loaded set is now over
    /// budget the next plan sheds the farthest cells.
    pub fn set_loaded(&mut self, cell: CellCoord, loaded: bool) -> Result<(), StreamError> {
        if !self.cells.contains_key(&cell) {
            return Err(StreamError::UnknownCell(cell));
        }
        if loaded {
            self.loaded.insert(cell);
        } else {
            self.loaded.remove(&cell);
        }
        Ok(())
    }

    /// Hold `cell` back from loading (`held = true`) or release it. A held cell is never a load
    /// candidate; everything else about it stays as it is. The host holds a failed cell until its
    /// retry step, or the next plan would order it again at once.
    pub fn set_held(&mut self, cell: CellCoord, held: bool) -> Result<(), StreamError> {
        if !self.cells.contains_key(&cell) {
            return Err(StreamError::UnknownCell(cell));
        }
        if held {
            self.held.insert(cell);
        } else {
            self.held.remove(&cell);
        }
        Ok(())
    }

    /// Whether `cell` is held back from loading.
    pub fn is_held(&self, cell: CellCoord) -> bool {
        self.held.contains(&cell)
    }

    /// Mark `cell` as draining: the host is still releasing it after an unload, so its memory stays
    /// counted against the budget and it cannot load again until the host clears the mark. A
    /// draining cell is not loaded, so plans order nothing for it.
    pub fn set_draining(&mut self, cell: CellCoord, draining: bool) -> Result<(), StreamError> {
        if !self.cells.contains_key(&cell) {
            return Err(StreamError::UnknownCell(cell));
        }
        if draining {
            self.loaded.remove(&cell);
            self.draining.insert(cell);
        } else {
            self.draining.remove(&cell);
        }
        Ok(())
    }

    /// Whether `cell` is draining.
    pub fn is_draining(&self, cell: CellCoord) -> bool {
        self.draining.contains(&cell)
    }

    /// Bytes of the draining cells.
    pub fn draining_bytes(&self) -> u64 {
        self.draining
            .iter()
            .filter_map(|c| self.cells.get(c))
            .fold(0u64, |acc, info| acc.saturating_add(info.bytes))
    }

    /// What the planner knows about `cell`.
    pub fn cell_info(&self, cell: CellCoord) -> Option<CellInfo> {
        self.cells.get(&cell).copied()
    }

    /// Number of registered cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Whether `cell` is loaded.
    pub fn is_loaded(&self, cell: CellCoord) -> bool {
        self.loaded.contains(&cell)
    }

    /// The loaded cells in cell order.
    pub fn loaded_cells(&self) -> impl Iterator<Item = CellCoord> + '_ {
        self.loaded.iter().copied()
    }

    /// Bytes occupied by the loaded cells.
    pub fn loaded_bytes(&self) -> u64 {
        self.loaded
            .iter()
            .filter_map(|c| self.cells.get(c))
            .fold(0u64, |acc, info| acc.saturating_add(info.bytes))
    }

    /// The distance that drives want and release: the nearest view, each view's distance divided
    /// by its radius scale.
    fn nearest(&self, views: &[PlanPoint], cell: CellCoord) -> f64 {
        views
            .iter()
            .map(|v| self.grid.distance_to_cell(v.x, v.z, cell) / v.scale)
            .fold(f64::INFINITY, f64::min)
    }

    /// The load-order key of a candidate cell (spec 78 § 8.3, steps 1 to 3).
    fn order_key(&self, views: &[PlanPoint], cell: CellCoord, info: CellInfo) -> OrderKey {
        let mut source_priority = None;
        let mut ring = false;
        let mut biased = f64::INFINITY;
        let center = self.grid.cell_center(cell);
        for v in views {
            let d = self.grid.distance_to_cell(v.x, v.z, cell) / v.scale;
            if d <= self.config.load_radius {
                source_priority = Some(source_priority.map_or(v.priority, |p: i32| p.max(v.priority)));
            }
            ring |= v
                .cell
                .is_some_and(|c| (c.x - cell.x).abs() <= 1 && (c.z - cell.z).abs() <= 1);
            biased = biased.min(d * (1.0 - v.bias * v.facing(center).max(0.0)));
        }
        (
            i64::from(info.priority) + i64::from(source_priority.unwrap_or(0)),
            ring,
            biased,
        )
    }

    /// Decide the loads and unloads for viewers at the given ground positions `[x, z]`.
    ///
    /// Runs once per streaming tick. Load candidates are the registered cells inside each viewer's
    /// unload window; a window the grid refuses as too large scans the registry instead.
    pub fn plan(&self, viewers: &[[f64; 2]]) -> Result<StreamPlan, StreamError> {
        let views: Vec<PlanPoint> = viewers
            .iter()
            .map(|v| PlanPoint {
                x: v[0],
                z: v[1],
                scale: 1.0,
                priority: 0,
                bias: 0.0,
                dir: None,
                cell: None,
            })
            .collect();
        self.plan_points(&views, false)
    }

    /// Decide the loads and unloads for sampled source views (spec 78 § 8.3).
    ///
    /// Want and release follow the rules of [`Self::plan`], with each view's distance divided by its
    /// `radius_scale`. The loads come out ordered by: cell priority plus the highest priority of the
    /// views wanting the cell; cells in the 3 x 3 block around a view; the view-biased distance; the
    /// cell coordinate. View bias changes only that order, never which cells are wanted, so turning
    /// a camera cannot unload anything.
    ///
    /// Unloads of this plan do not make room for its loads: a host using this form releases a cell
    /// over several frames ([`Self::set_draining`]), so the memory only counts as free in the next
    /// plan after that. A load that needs the room is deferred as [`DeferReason::OverBudget`].
    pub fn plan_views(&self, views: &[SourceView]) -> Result<StreamPlan, StreamError> {
        let mut points = Vec::with_capacity(views.len());
        for v in views {
            let scale = f64::from(v.radius_scale);
            if !scale.is_finite() || scale <= 0.0 {
                return Err(StreamError::BadRadius {
                    what: "radius_scale",
                    value: scale,
                });
            }
            points.push(PlanPoint {
                x: v.pos[0],
                z: v.pos[2],
                scale,
                priority: v.priority,
                bias: f64::from(v.view_bias.clamp(0.0, 1.0)),
                dir: v
                    .dir
                    .and_then(|d| flat_direction([f64::from(d[0]), f64::from(d[2])])),
                cell: Some(self.grid.cell_of(v.pos[0], v.pos[2])?),
            });
        }
        self.plan_points(&points, true)
    }

    /// `view_order` is the [`Self::plan_views`] form: its load order, and unloads that free nothing yet.
    fn plan_points(&self, viewers: &[PlanPoint], view_order: bool) -> Result<StreamPlan, StreamError> {
        let _span =
            tracing::trace_span!("stream_plan", cells = self.cells.len(), viewers = viewers.len()).entered();
        PLAN_CELLS_VISITED.set(0);
        PLAN_RING_SUMS.set(0);
        if viewers.is_empty() {
            return Err(StreamError::NoViewers);
        }
        for v in viewers {
            if !v.x.is_finite() {
                return Err(StreamError::NonFinite("viewer x"));
            }
            if !v.z.is_finite() {
                return Err(StreamError::NonFinite("viewer z"));
            }
        }
        let cfg = &self.config;
        let mut plan = StreamPlan::default();
        // Without a VRAM bound nothing is ever over it.
        let vram_cap = self.vram_budget.unwrap_or(u64::MAX);

        // Bytes of this plan's unloads that stay held until the host drains them.
        let mut released = 0u64;
        let mut released_vram = 0u64;
        // Loaded cells: unload the ones beyond the unload radius, keep the rest.
        let mut keep: Vec<Resident> = Vec::new();
        // Cells still draining hold their memory until the host releases them.
        let mut used = self.draining_bytes();
        let mut used_vram = self.draining_vram();
        for &cell in &self.loaded {
            let Some(info) = self.cells.get(&cell) else {
                continue;
            };
            let vram = self.cell_vram(cell);
            let distance = self.nearest(viewers, cell);
            if distance > cfg.unload_radius {
                plan.unloads.push(UnloadOrder {
                    cell,
                    distance,
                    bytes: info.bytes,
                    reason: UnloadReason::Distance,
                });
                if view_order {
                    released = released.saturating_add(info.bytes);
                    released_vram = released_vram.saturating_add(vram);
                }
            } else {
                used = used.saturating_add(info.bytes);
                used_vram = used_vram.saturating_add(vram);
                keep.push(Resident {
                    cell,
                    distance,
                    bytes: info.bytes,
                    vram,
                });
            }
        }

        // Farthest first (ties: the larger coordinate first), used for every eviction.
        keep.sort_by(|a, b| b.distance.total_cmp(&a.distance).then(b.cell.cmp(&a.cell)));

        // A lowered budget: shed the farthest cells until the rest fit.
        let mut first_kept = 0;
        while (used > cfg.memory_budget_bytes || used_vram > vram_cap) && first_kept < keep.len() {
            let r = &keep[first_kept];
            used = used.saturating_sub(r.bytes);
            used_vram = used_vram.saturating_sub(r.vram);
            if view_order {
                released = released.saturating_add(r.bytes);
                released_vram = released_vram.saturating_add(r.vram);
            }
            plan.unloads.push(UnloadOrder {
                cell: r.cell,
                distance: r.distance,
                bytes: r.bytes,
                reason: UnloadReason::OverBudget,
            });
            first_kept += 1;
        }
        // Cells that may be evicted to make room: outside the load radius, farthest first.
        let mut evictable: VecDeque<&Resident> = keep[first_kept..]
            .iter()
            .filter(|r| r.distance > cfg.load_radius)
            .collect();
        // One pass over the ring. Candidates that pop from it subtract, so a later candidate sees
        // the same total the old per-candidate fold would have seen.
        let mut reclaimable = evictable
            .iter()
            .fold(0u64, |acc, resident| acc.saturating_add(resident.bytes));
        let mut reclaimable_vram = evictable
            .iter()
            .fold(0u64, |acc, resident| acc.saturating_add(resident.vram));
        PLAN_RING_SUMS.set(1);

        // Load candidates: unloaded cells within the load radius, best first. The unload window
        // is a superset of the load window, so a cell outside it cannot be a candidate. Loaded
        // cells outside it are already released by the distance pass above.
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut full_scan = false;
        let mut window = BTreeSet::new();
        for viewer in viewers {
            let radius = cfg.unload_radius * viewer.scale;
            match self
                .grid
                .cells_within(viewer.x, viewer.z, radius, PLAN_WINDOW_MAX_CELLS)
            {
                Ok(cells) => window.extend(cells),
                Err(StreamError::TooManyCells { .. }) => {
                    full_scan = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        let registered: Vec<(CellCoord, CellInfo)> = if full_scan {
            self.cells.iter().map(|(&cell, &info)| (cell, info)).collect()
        } else {
            window
                .into_iter()
                .filter_map(|cell| self.cells.get(&cell).copied().map(|info| (cell, info)))
                .collect()
        };
        for (cell, info) in registered {
            note_load_candidate(
                self,
                &mut candidates,
                viewers,
                cfg.load_radius,
                view_order,
                cell,
                info,
            );
        }
        candidates.sort_by(|a, b| {
            b.3.0
                .cmp(&a.3.0)
                .then(b.3.1.cmp(&a.3.1))
                .then(a.3.2.total_cmp(&b.3.2))
                .then(a.0.cmp(&b.0))
        });

        let limit = if cfg.max_loads_per_step == 0 {
            usize::MAX
        } else {
            cfg.max_loads_per_step as usize
        };
        for (cell, info, distance, key) in candidates {
            let vram = self.cell_vram(cell);
            if info.bytes > cfg.memory_budget_bytes || vram > vram_cap {
                plan.deferred.push(DeferredLoad {
                    cell,
                    reason: DeferReason::TooLarge,
                });
                continue;
            }
            if plan.loads.len() >= limit {
                plan.deferred.push(DeferredLoad {
                    cell,
                    reason: DeferReason::RateLimited,
                });
                continue;
            }
            let free = cfg
                .memory_budget_bytes
                .saturating_sub(used.saturating_add(released));
            let free_vram = vram_cap.saturating_sub(used_vram.saturating_add(released_vram));
            if info.bytes > free || vram > free_vram {
                let need = info.bytes.saturating_sub(free);
                let need_vram = vram.saturating_sub(free_vram);
                if reclaimable < need || reclaimable_vram < need_vram {
                    plan.deferred.push(DeferredLoad {
                        cell,
                        reason: DeferReason::OverBudget,
                    });
                    continue;
                }
                let mut freed = 0u64;
                let mut freed_vram = 0u64;
                while freed < need || freed_vram < need_vram {
                    let Some(r) = evictable.pop_front() else { break };
                    reclaimable = reclaimable.saturating_sub(r.bytes);
                    reclaimable_vram = reclaimable_vram.saturating_sub(r.vram);
                    freed = freed.saturating_add(r.bytes);
                    freed_vram = freed_vram.saturating_add(r.vram);
                    used = used.saturating_sub(r.bytes);
                    used_vram = used_vram.saturating_sub(r.vram);
                    plan.unloads.push(UnloadOrder {
                        cell: r.cell,
                        distance: r.distance,
                        bytes: r.bytes,
                        reason: UnloadReason::MakeRoom,
                    });
                }
                if view_order {
                    // The room arrives once the evicted cells drained.
                    released = released.saturating_add(freed);
                    released_vram = released_vram.saturating_add(freed_vram);
                    plan.deferred.push(DeferredLoad {
                        cell,
                        reason: DeferReason::OverBudget,
                    });
                    continue;
                }
            }
            used = used.saturating_add(info.bytes);
            used_vram = used_vram.saturating_add(vram);
            plan.loads.push(LoadOrder {
                cell,
                distance,
                bytes: info.bytes,
                priority: key.0.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            });
        }
        plan.resident_bytes_after = used;
        Ok(plan)
    }

    /// Record that the host performed `plan`: unloads first, then loads.
    ///
    /// The plan is validated against the current state first and nothing changes on error: every
    /// unload must be loaded, every load must be registered and not loaded, and the result must fit
    /// the budget.
    pub fn apply(&mut self, plan: &StreamPlan) -> Result<(), StreamError> {
        let mut next = self.loaded.clone();
        for u in &plan.unloads {
            if !next.remove(&u.cell) {
                return Err(StreamError::NotLoaded(u.cell));
            }
        }
        for l in &plan.loads {
            if !self.cells.contains_key(&l.cell) {
                return Err(StreamError::UnknownCell(l.cell));
            }
            if !next.insert(l.cell) {
                return Err(StreamError::AlreadyLoaded(l.cell));
            }
        }
        let resident = next
            .iter()
            .filter_map(|c| self.cells.get(c))
            .fold(self.draining_bytes(), |acc, info| acc.saturating_add(info.bytes));
        // Cells draining past a lowered budget can only shrink, so a plan with no loads is fine.
        if resident > self.config.memory_budget_bytes && (self.draining.is_empty() || !plan.loads.is_empty())
        {
            return Err(StreamError::OverBudget {
                resident,
                budget: self.config.memory_budget_bytes,
            });
        }
        if let Some(cap) = self.vram_budget
            && !plan.loads.is_empty()
        {
            let vram = next.iter().fold(self.draining_vram(), |acc, c| {
                acc.saturating_add(self.cell_vram(*c))
            });
            if vram > cap {
                return Err(StreamError::OverBudget {
                    resident: vram,
                    budget: cap,
                });
            }
        }
        self.loaded = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(load: f64, unload: f64, budget: u64) -> StreamConfig {
        StreamConfig {
            load_radius: load,
            unload_radius: unload,
            memory_budget_bytes: budget,
            max_loads_per_step: 0,
        }
    }

    /// A planner over 100 m cells with `n` cells in a row along X, each `bytes` big.
    fn row(n: i64, bytes: u64, config: StreamConfig) -> StreamPlanner {
        let mut p = StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), config).expect("config");
        for x in 0..n {
            p.register_cell(CellCoord::new(x, 0), CellInfo { bytes, priority: 0 })
                .expect("unique");
        }
        p
    }

    fn step(p: &mut StreamPlanner, viewers: &[[f64; 2]]) -> StreamPlan {
        let plan = p.plan(viewers).expect("plan");
        p.apply(&plan).expect("apply");
        plan
    }

    fn cells_of(plan: &StreamPlan) -> Vec<i64> {
        plan.loads.iter().map(|l| l.cell.x).collect()
    }

    fn view(x: f64, z: f64) -> SourceView {
        SourceView::at(SourceId::Pin(0), [x, 0.0, z])
    }

    /// A planner over 100 m cells on an `n` by `n` block, each `bytes` big.
    fn block(n: i64, bytes: u64, config: StreamConfig) -> StreamPlanner {
        let mut p = StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), config).expect("config");
        for x in 0..n {
            for z in 0..n {
                p.register_cell(CellCoord::new(x, z), CellInfo { bytes, priority: 0 })
                    .expect("unique");
            }
        }
        p
    }

    #[test]
    fn plan_views_with_defaults_wants_what_plan_wants() {
        let p = row(20, 10, cfg(250.0, 300.0, 1000));
        let a = p.plan(&[[450.0, 50.0]]).expect("plan");
        let b = p.plan_views(&[view(450.0, 50.0)]).expect("plan");
        let cells = |plan: &StreamPlan| {
            let mut c = cells_of(plan);
            c.sort_unstable();
            c
        };
        assert_eq!(cells(&a), cells(&b));
        assert_eq!(a.resident_bytes_after, b.resident_bytes_after);
    }

    #[test]
    fn radius_scale_divides_distance_for_want_and_release() {
        let mut p = row(20, 10, cfg(100.0, 150.0, 1000));
        let near = p.plan_views(&[view(50.0, 50.0)]).expect("plan");
        assert_eq!(cells_of(&near), vec![0, 1]);
        let far = SourceView {
            radius_scale: 3.0,
            ..view(50.0, 50.0)
        };
        let wide = p.plan_views(&[far]).expect("plan");
        assert_eq!(cells_of(&wide), vec![0, 1, 2, 3]);
        p.apply(&wide).expect("apply");
        // Scale 1 again: cells 2 and 3 are 150 and 250 away, only the last is past the unload radius.
        let shrink = p.plan_views(&[view(50.0, 50.0)]).expect("plan");
        let gone: Vec<i64> = shrink.unloads.iter().map(|u| u.cell.x).collect();
        assert_eq!(gone, vec![3]);
    }

    #[test]
    fn a_zero_or_negative_radius_scale_is_refused() {
        let p = row(4, 10, cfg(100.0, 150.0, 1000));
        for scale in [0.0, -1.0, f32::NAN] {
            let bad = SourceView {
                radius_scale: scale,
                ..view(0.0, 0.0)
            };
            assert!(
                matches!(p.plan_views(&[bad]), Err(StreamError::BadRadius { .. })),
                "{scale}"
            );
        }
        assert_eq!(p.plan_views(&[]), Err(StreamError::NoViewers));
    }

    #[test]
    fn source_priority_outranks_distance_and_only_counts_for_cells_it_wants() {
        let p = row(30, 10, cfg(150.0, 200.0, 1000));
        let urgent = SourceView {
            priority: 5,
            ..view(2500.0, 50.0)
        };
        let plan = p.plan_views(&[view(50.0, 50.0), urgent]).expect("plan");
        let cells = cells_of(&plan);
        let first_far = cells.iter().position(|c| *c >= 20).expect("far cell");
        let last_far = cells.iter().rposition(|c| *c >= 20).expect("far cell");
        let first_near = cells.iter().position(|c| *c < 10).expect("near cell");
        assert!(last_far < first_near, "{cells:?}");
        assert_eq!(first_far, 0);
        assert_eq!(plan.loads[0].priority, 5);
        assert_eq!(plan.loads[first_near].priority, 0);
    }

    #[test]
    fn the_block_around_a_view_comes_before_a_nearer_cell_outside_it() {
        // Viewer in the far corner of cell (0, 0): (2, 0) is 101 m away, the block cell (-1, -1)
        // about 140 m, so plain distance would load (2, 0) first.
        let mut p = StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), cfg(160.0, 200.0, 1000))
            .expect("config");
        for (x, z) in [(2, 0), (-1, -1)] {
            p.register_cell(
                CellCoord::new(x, z),
                CellInfo {
                    bytes: 1,
                    priority: 0,
                },
            )
            .expect("unique");
        }
        let plan = p.plan_views(&[view(99.0, 99.0)]).expect("plan");
        assert_eq!(plan.loads[0].cell, CellCoord::new(-1, -1));
        // `plan` keeps its plain order.
        let plain = p.plan(&[[99.0, 99.0]]).expect("plan");
        assert_eq!(plain.loads[0].cell, CellCoord::new(2, 0));
    }

    #[test]
    fn view_bias_changes_the_order_never_the_wanted_set() {
        let p = block(9, 10, cfg(180.0, 220.0, 100_000));
        let at = |dir: Option<[f32; 3]>| SourceView {
            dir,
            view_bias: 1.0,
            ..view(450.0, 450.0)
        };
        let east = p.plan_views(&[at(Some([1.0, 0.0, 0.0]))]).expect("plan");
        let west = p.plan_views(&[at(Some([-1.0, 0.0, 0.0]))]).expect("plan");
        let set = |plan: &StreamPlan| {
            let mut c: Vec<_> = plan.loads.iter().map(|l| l.cell).collect();
            c.sort_unstable();
            c
        };
        assert_eq!(set(&east), set(&west));
        let order = |plan: &StreamPlan| plan.loads.iter().map(|l| l.cell).collect::<Vec<_>>();
        assert_ne!(order(&east), order(&west));
        // Past the 3 x 3 block, the cell the view looks at comes first.
        let outside = |plan: &StreamPlan| {
            plan.loads
                .iter()
                .find(|l| (l.cell.x - 4).abs() > 1 || (l.cell.z - 4).abs() > 1)
                .map(|l| l.cell)
        };
        assert_eq!(outside(&east).map(|c| c.x), Some(6));
        assert_eq!(outside(&west).map(|c| c.x), Some(2));
    }

    #[test]
    fn a_held_cell_is_never_a_candidate_until_released() {
        let mut p = row(4, 10, cfg(500.0, 600.0, 1000));
        let cell = CellCoord::new(1, 0);
        p.set_held(cell, true).expect("known");
        assert!(p.is_held(cell));
        let plan = step(&mut p, &[[0.0, 0.0]]);
        assert_eq!(cells_of(&plan), vec![0, 2, 3]);
        assert!(p.plan(&[[0.0, 0.0]]).expect("plan").is_empty());
        p.set_held(cell, false).expect("known");
        assert_eq!(cells_of(&p.plan(&[[0.0, 0.0]]).expect("plan")), vec![1]);
        assert_eq!(
            p.set_held(CellCoord::new(9, 9), true),
            Err(StreamError::UnknownCell(CellCoord::new(9, 9)))
        );
    }

    #[test]
    fn a_draining_cell_keeps_its_bytes_until_released() {
        let mut p = row(3, 10, cfg(500.0, 600.0, 20));
        step(&mut p, &[[0.0, 0.0]]);
        assert_eq!(p.loaded_bytes(), 20);
        // Cell 2 cannot fit; draining cell 0 frees nothing until the host says so.
        let cell = CellCoord::new(0, 0);
        p.set_draining(cell, true).expect("known");
        assert!(p.is_draining(cell));
        assert!(!p.is_loaded(cell));
        assert_eq!(p.draining_bytes(), 10);
        let plan = p.plan(&[[0.0, 0.0]]).expect("plan");
        assert!(plan.loads.is_empty(), "{plan:?}");
        p.set_draining(cell, false).expect("known");
        let plan = p.plan(&[[0.0, 0.0]]).expect("plan");
        assert_eq!(cells_of(&plan), vec![0]);
    }

    #[test]
    fn plan_views_unloads_free_nothing_until_they_drained() {
        // Two cells fit. The viewer jumps to the far end: the old cells unload, the new one waits.
        let mut p = row(10, 10, cfg(100.0, 150.0, 20));
        let plan = p.plan_views(&[view(50.0, 50.0)]).expect("plan");
        p.apply(&plan).expect("apply");
        assert_eq!(p.loaded_bytes(), 20);
        let jump = p.plan_views(&[view(950.0, 50.0)]).expect("plan");
        assert_eq!(jump.unloads.len(), 2);
        assert!(jump.loads.is_empty(), "{jump:?}");
        assert!(jump.deferred.iter().all(|d| d.reason == DeferReason::OverBudget));
        p.apply(&jump).expect("apply");
        for unload in &jump.unloads {
            p.set_draining(unload.cell, true).expect("known");
            p.set_draining(unload.cell, false).expect("known");
        }
        assert_eq!(p.plan_views(&[view(950.0, 50.0)]).expect("plan").loads.len(), 2);
        // `plan` keeps its one-shot accounting.
        let mut q = row(10, 10, cfg(100.0, 150.0, 20));
        step(&mut q, &[[50.0, 50.0]]);
        assert_eq!(q.plan(&[[950.0, 50.0]]).expect("plan").loads.len(), 2);
    }

    #[test]
    fn plan_views_evicts_for_a_near_cell_and_loads_it_after_the_drain() {
        // Cell 2 is in the band; a pin near cell 3 wants room.
        let mut p = row(6, 10, cfg(100.0, 300.0, 30));
        let first = p.plan_views(&[view(150.0, 50.0)]).expect("plan");
        p.apply(&first).expect("apply");
        assert_eq!(p.loaded_bytes(), 30);
        // Move right: cell 3 becomes a candidate and something beyond the load radius must go.
        let moved = p.plan_views(&[view(290.0, 50.0)]).expect("plan");
        assert!(moved.loads.is_empty(), "{moved:?}");
        let evicted: Vec<_> = moved
            .unloads
            .iter()
            .filter(|u| u.reason == UnloadReason::MakeRoom)
            .collect();
        assert_eq!(evicted.len(), 1, "{moved:?}");
        assert!(moved.deferred.iter().any(|d| d.cell.x == 3));
    }

    #[test]
    fn apply_tolerates_draining_bytes_over_a_lowered_budget_when_nothing_loads() {
        let mut p = row(3, 10, cfg(500.0, 600.0, 30));
        step(&mut p, &[[0.0, 0.0]]);
        p.set_draining(CellCoord::new(0, 0), true).expect("known");
        p.set_config(cfg(500.0, 600.0, 5)).expect("config");
        let plan = p.plan(&[[0.0, 0.0]]).expect("plan");
        assert!(plan.loads.is_empty());
        p.apply(&plan).expect("shedding cannot be refused");
        assert_eq!(p.loaded_bytes(), 0);
    }

    #[test]
    fn config_validation() {
        assert!(cfg(100.0, 100.0, 1).validate().is_ok());
        assert!(matches!(
            cfg(100.0, 99.0, 1).validate(),
            Err(StreamError::NoHysteresis { .. })
        ));
        assert!(matches!(
            cfg(-1.0, 5.0, 1).validate(),
            Err(StreamError::BadRadius {
                what: "load_radius",
                ..
            })
        ));
        assert!(matches!(
            cfg(1.0, f64::NAN, 1).validate(),
            Err(StreamError::BadRadius {
                what: "unload_radius",
                ..
            })
        ));
        let grid = StreamGrid::new(10.0).expect("grid");
        assert!(StreamPlanner::new(grid, cfg(5.0, 1.0, 1)).is_err());
        let mut p = StreamPlanner::new(grid, cfg(1.0, 2.0, 1)).expect("ok");
        assert!(p.set_config(cfg(3.0, 2.0, 1)).is_err());
        assert_eq!(p.config().load_radius, 1.0, "a refused config leaves the old one");
    }

    #[test]
    fn registry_rejects_duplicates_and_remove_clears_loaded() {
        let mut p = row(2, 10, cfg(500.0, 600.0, 1000));
        assert_eq!(
            p.register_cell(
                CellCoord::new(0, 0),
                CellInfo {
                    bytes: 1,
                    priority: 0
                }
            ),
            Err(StreamError::DuplicateCell(CellCoord::new(0, 0)))
        );
        step(&mut p, &[[0.0, 0.0]]);
        assert!(p.is_loaded(CellCoord::new(1, 0)));
        assert!(p.remove_cell(CellCoord::new(1, 0)));
        assert!(!p.is_loaded(CellCoord::new(1, 0)));
        assert!(!p.remove_cell(CellCoord::new(1, 0)));
        assert_eq!(p.cell_count(), 1);
        assert_eq!(p.loaded_bytes(), 10);
    }

    #[test]
    fn loads_within_radius_nearest_first() {
        let mut p = row(10, 10, cfg(250.0, 300.0, 10_000));
        let plan = step(&mut p, &[[50.0, 50.0]]);
        // Cells 0..=3 are within 250 m of x=50 (cell 3 starts at 300, 250 away).
        assert_eq!(cells_of(&plan), vec![0, 1, 2, 3]);
        assert_eq!(plan.resident_bytes_after, 40);
        assert!(plan.unloads.is_empty() && plan.deferred.is_empty());
    }

    #[test]
    fn hysteresis_band_keeps_state() {
        let mut p = row(10, 10, cfg(100.0, 300.0, 10_000));
        step(&mut p, &[[50.0, 50.0]]); // loads cells 0 and 1
        assert!(p.is_loaded(CellCoord::new(1, 0)));
        // Move so that cell 0 is 200 m away: inside the unload radius, outside the load radius.
        let plan = step(&mut p, &[[250.0, 50.0]]);
        assert!(p.is_loaded(CellCoord::new(0, 0)), "in the band: stays loaded");
        assert!(!plan.unloads.iter().any(|u| u.cell.x == 0));
        // A fresh planner at the same spot must not load cell 0 (outside the load radius).
        let mut fresh = row(10, 10, cfg(100.0, 300.0, 10_000));
        step(&mut fresh, &[[250.0, 50.0]]);
        assert!(
            !fresh.is_loaded(CellCoord::new(0, 0)),
            "in the band: stays unloaded"
        );
        // Go further: now cell 0 is beyond the unload radius.
        let plan = step(&mut p, &[[450.0, 50.0]]);
        assert!(!p.is_loaded(CellCoord::new(0, 0)));
        assert!(
            plan.unloads
                .iter()
                .any(|u| u.cell.x == 0 && u.reason == UnloadReason::Distance)
        );
    }

    #[test]
    fn walking_along_the_edge_does_not_flicker() {
        let mut p = row(6, 10, cfg(100.0, 200.0, 10_000));
        let mut changes = 0;
        // Oscillate around the load radius of cell 3 (starts at x=300).
        for i in 0..40 {
            let x = if i % 2 == 0 { 190.0 } else { 210.0 };
            let plan = step(&mut p, &[[x, 50.0]]);
            changes += plan.loads.len() + plan.unloads.len();
        }
        let after_first = {
            let mut q = row(6, 10, cfg(100.0, 200.0, 10_000));
            let first = step(&mut q, &[[190.0, 50.0]]);
            first.loads.len() + first.unloads.len()
        };
        assert!(
            changes <= after_first + 2,
            "{changes} changes for a repeating wobble"
        );
    }

    #[test]
    fn priority_outranks_distance() {
        let mut p = row(5, 10, cfg(450.0, 500.0, 10_000));
        p.remove_cell(CellCoord::new(4, 0));
        p.register_cell(
            CellCoord::new(4, 0),
            CellInfo {
                bytes: 10,
                priority: 5,
            },
        )
        .expect("re-register");
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(
            cells_of(&plan),
            vec![4, 0, 1, 2, 3],
            "high priority first, rest by distance"
        );
    }

    #[test]
    fn budget_is_respected_and_nearest_wins() {
        let mut p = row(10, 10, cfg(900.0, 1000.0, 35));
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(cells_of(&plan), vec![0, 1, 2]);
        assert_eq!(plan.resident_bytes_after, 30);
        assert_eq!(plan.deferred.len(), 7);
        assert!(plan.deferred.iter().all(|d| d.reason == DeferReason::OverBudget));
        assert!(p.loaded_bytes() <= 35);
    }

    #[test]
    fn budget_holds_along_a_whole_walk() {
        let mut p = row(40, 10, cfg(300.0, 450.0, 70));
        for i in 0..400 {
            let x = i as f64 * 10.0;
            let plan = step(&mut p, &[[x, 50.0]]);
            assert!(p.loaded_bytes() <= 70, "step {i}");
            assert_eq!(plan.resident_bytes_after, p.loaded_bytes(), "step {i}");
            // The cell under the viewer is always loaded when it can fit.
            let here = (x / 100.0).floor() as i64;
            assert!(p.is_loaded(CellCoord::new(here.min(39), 0)), "step {i}");
        }
    }

    #[test]
    fn a_cell_larger_than_the_budget_is_deferred_as_too_large() {
        let mut p = row(2, 10, cfg(500.0, 600.0, 100));
        p.register_cell(
            CellCoord::new(5, 5),
            CellInfo {
                bytes: 101,
                priority: 9,
            },
        )
        .expect("unique");
        let plan = step(&mut p, &[[400.0, 400.0]]);
        assert!(plan.deferred.contains(&DeferredLoad {
            cell: CellCoord::new(5, 5),
            reason: DeferReason::TooLarge
        }));
        assert!(!p.is_loaded(CellCoord::new(5, 5)));
    }

    #[test]
    fn rate_limit_defers_the_tail() {
        let mut config = cfg(900.0, 1000.0, 10_000);
        config.max_loads_per_step = 2;
        let mut p = row(5, 10, config);
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(cells_of(&plan), vec![0, 1]);
        assert_eq!(plan.deferred.len(), 3);
        assert!(plan.deferred.iter().all(|d| d.reason == DeferReason::RateLimited));
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(cells_of(&plan), vec![2, 3]);
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(cells_of(&plan), vec![4]);
        assert!(step(&mut p, &[[50.0, 50.0]]).is_empty(), "converged");
    }

    #[test]
    fn a_near_cell_evicts_a_band_cell_to_fit() {
        let mut p = row(10, 10, cfg(100.0, 400.0, 20));
        step(&mut p, &[[50.0, 50.0]]); // cells 0, 1 loaded (budget full)
        // Move to cell 3: cells 2, 3, 4 are candidates; 0 and 1 sit in the hysteresis band.
        let plan = step(&mut p, &[[350.0, 50.0]]);
        assert!(p.loaded_bytes() <= 20);
        assert!(
            p.is_loaded(CellCoord::new(3, 0)),
            "the cell under the viewer is loaded"
        );
        assert!(
            plan.unloads
                .iter()
                .all(|u| u.reason != UnloadReason::Distance || u.cell.x < 1)
        );
        assert!(
            plan.unloads
                .iter()
                .any(|u| u.reason == UnloadReason::MakeRoom && u.cell.x == 0)
        );
    }

    #[test]
    fn cells_inside_the_load_radius_are_never_evicted_for_room() {
        let mut p = row(4, 10, cfg(400.0, 500.0, 20));
        step(&mut p, &[[50.0, 50.0]]); // cells 0 and 1 (nearest) fit; 2 and 3 deferred
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert!(plan.is_empty(), "no eviction churn between equals: {plan:?}");
        assert!(p.is_loaded(CellCoord::new(0, 0)) && p.is_loaded(CellCoord::new(1, 0)));
    }

    #[test]
    fn a_lowered_budget_sheds_the_farthest_cells() {
        let mut p = row(5, 10, cfg(900.0, 1000.0, 1000));
        step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(p.loaded_bytes(), 50);
        p.set_config(cfg(900.0, 1000.0, 25)).expect("valid");
        let plan = step(&mut p, &[[50.0, 50.0]]);
        let shed: Vec<i64> = plan.unloads.iter().map(|u| u.cell.x).collect();
        assert_eq!(shed, vec![4, 3, 2], "farthest first");
        assert!(plan.unloads.iter().all(|u| u.reason == UnloadReason::OverBudget));
        assert_eq!(p.loaded_bytes(), 20);
        assert!(plan.loads.is_empty(), "shed cells do not come straight back");
    }

    #[test]
    fn the_nearest_of_several_viewers_counts() {
        let mut p = row(20, 10, cfg(100.0, 150.0, 10_000));
        let plan = step(&mut p, &[[50.0, 50.0], [1550.0, 50.0]]);
        let mut loaded = cells_of(&plan);
        loaded.sort_unstable();
        assert_eq!(loaded, vec![0, 1, 14, 15, 16]);
        // Drop the second viewer: its cells unload once beyond the unload radius.
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(plan.unloads.len(), 3);
    }

    #[test]
    fn bad_viewers_are_errors() {
        let p = row(1, 1, cfg(1.0, 2.0, 10));
        assert_eq!(p.plan(&[]), Err(StreamError::NoViewers));
        assert_eq!(
            p.plan(&[[f64::NAN, 0.0]]),
            Err(StreamError::NonFinite("viewer x"))
        );
        assert_eq!(
            p.plan(&[[0.0, f64::INFINITY]]),
            Err(StreamError::NonFinite("viewer z"))
        );
    }

    #[test]
    fn apply_refuses_inconsistent_plans_and_changes_nothing() {
        let mut p = row(3, 10, cfg(900.0, 1000.0, 25));
        let good = p.plan(&[[50.0, 50.0]]).expect("plan");
        p.apply(&good).expect("apply");
        let before: Vec<_> = p.loaded_cells().collect();
        assert_eq!(
            p.apply(&good),
            Err(StreamError::AlreadyLoaded(CellCoord::new(0, 0)))
        );
        let load = |x: i64| LoadOrder {
            cell: CellCoord::new(x, 0),
            distance: 0.0,
            bytes: 10,
            priority: 0,
        };
        let unknown = StreamPlan {
            loads: vec![load(99)],
            ..StreamPlan::default()
        };
        assert_eq!(
            p.apply(&unknown),
            Err(StreamError::UnknownCell(CellCoord::new(99, 0)))
        );
        let unload = StreamPlan {
            unloads: vec![UnloadOrder {
                cell: CellCoord::new(2, 0),
                distance: 0.0,
                bytes: 10,
                reason: UnloadReason::Distance,
            }],
            ..StreamPlan::default()
        };
        assert_eq!(
            p.apply(&unload),
            Err(StreamError::NotLoaded(CellCoord::new(2, 0)))
        );
        let over = StreamPlan {
            loads: vec![load(2)],
            ..StreamPlan::default()
        };
        assert!(matches!(
            p.apply(&over),
            Err(StreamError::OverBudget {
                resident: 30,
                budget: 25
            })
        ));
        assert_eq!(p.loaded_cells().collect::<Vec<_>>(), before);
    }

    #[test]
    fn plans_do_not_depend_on_registration_order() {
        let cells: Vec<CellCoord> = (0..6)
            .flat_map(|x| (0..6).map(move |z| CellCoord::new(x, z)))
            .collect();
        let build = |order: &[CellCoord]| {
            let mut p = StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), cfg(250.0, 350.0, 80))
                .expect("config");
            for c in order {
                let bytes = 5 + (c.x * 7 + c.z * 3) as u64 % 11;
                p.register_cell(
                    *c,
                    CellInfo {
                        bytes,
                        priority: (c.x % 3) as i32,
                    },
                )
                .expect("unique");
            }
            p
        };
        let mut reversed = cells.clone();
        reversed.reverse();
        let (mut a, mut b) = (build(&cells), build(&reversed));
        for i in 0..30 {
            let v = [[i as f64 * 17.0, 120.0 + i as f64 * 9.0], [400.0, 300.0]];
            let pa = a.plan(&v).expect("plan");
            let pb = b.plan(&v).expect("plan");
            assert_eq!(pa, pb, "step {i}");
            a.apply(&pa).expect("apply");
            b.apply(&pb).expect("apply");
            assert!(a.loaded_bytes() <= 80);
        }
    }

    #[test]
    fn set_loaded_resynchronizes_and_a_failed_load_is_retried() {
        let mut p = row(3, 10, cfg(900.0, 1000.0, 1000));
        let plan = p.plan(&[[50.0, 50.0]]).expect("plan");
        p.apply(&plan).expect("apply");
        // The host reports that cell 1 failed to load: the next plan orders it again.
        p.set_loaded(CellCoord::new(1, 0), false).expect("known");
        let again = p.plan(&[[50.0, 50.0]]).expect("plan");
        assert_eq!(cells_of(&again), vec![1]);
        assert_eq!(
            p.set_loaded(CellCoord::new(9, 9), true),
            Err(StreamError::UnknownCell(CellCoord::new(9, 9)))
        );
        // An over-budget resync is allowed and shed by the next plan.
        let mut q = row(3, 10, cfg(900.0, 1000.0, 15));
        for x in 0..3 {
            q.set_loaded(CellCoord::new(x, 0), true).expect("known");
        }
        let shed = q.plan(&[[50.0, 50.0]]).expect("plan");
        assert_eq!(shed.unloads.len(), 2);
        assert_eq!(shed.resident_bytes_after, 10);
    }

    #[test]
    fn a_settled_planner_plans_nothing() {
        let mut p = row(6, 10, cfg(300.0, 400.0, 1000));
        step(&mut p, &[[250.0, 50.0]]);
        assert!(p.plan(&[[250.0, 50.0]]).expect("plan").is_empty());
    }

    /// `row` with every cell's VRAM estimate set to `vram`.
    fn row_vram(n: i64, bytes: u64, vram: u64, config: StreamConfig) -> StreamPlanner {
        let mut p = row(n, bytes, config);
        for x in 0..n {
            p.set_cell_vram(CellCoord::new(x, 0), vram).expect("known");
        }
        p
    }

    #[test]
    fn without_a_vram_bound_vram_changes_no_plan() {
        let plain = row(10, 10, cfg(900.0, 1000.0, 1000));
        let estimated = row_vram(10, 10, 1 << 40, cfg(900.0, 1000.0, 1000));
        assert_eq!(estimated.vram_budget(), None);
        assert_eq!(
            plain.plan(&[[50.0, 50.0]]).expect("plan"),
            estimated.plan(&[[50.0, 50.0]]).expect("plan")
        );
        assert_eq!(
            plain.plan_views(&[view(50.0, 50.0)]).expect("plan"),
            estimated.plan_views(&[view(50.0, 50.0)]).expect("plan")
        );
    }

    #[test]
    fn the_vram_bound_limits_loads_nearest_first() {
        // Memory would take all ten; VRAM takes three.
        let mut p = row_vram(10, 10, 100, cfg(900.0, 1000.0, 1000));
        p.set_vram_budget(Some(350));
        let plan = step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(cells_of(&plan), vec![0, 1, 2]);
        assert_eq!(p.loaded_vram(), 300);
        assert!(plan.deferred.iter().all(|d| d.reason == DeferReason::OverBudget));
        // A cell whose VRAM alone is over the bound can never load.
        p.set_cell_vram(CellCoord::new(9, 0), 351).expect("known");
        let plan = p.plan(&[[950.0, 50.0]]).expect("plan");
        assert!(plan.deferred.contains(&DeferredLoad {
            cell: CellCoord::new(9, 0),
            reason: DeferReason::TooLarge
        }));
    }

    #[test]
    fn the_vram_bound_holds_along_a_whole_walk_and_evicts_band_cells() {
        let mut p = row_vram(40, 1, 10, cfg(300.0, 450.0, 1_000_000));
        p.set_vram_budget(Some(70));
        for i in 0..400 {
            let x = i as f64 * 10.0;
            step(&mut p, &[[x, 50.0]]);
            assert!(p.loaded_vram() <= 70, "step {i}");
            let here = (x / 100.0).floor() as i64;
            assert!(p.is_loaded(CellCoord::new(here.min(39), 0)), "step {i}");
        }
    }

    #[test]
    fn a_lowered_vram_bound_sheds_the_farthest_cells() {
        let mut p = row_vram(5, 1, 10, cfg(900.0, 1000.0, 1000));
        step(&mut p, &[[50.0, 50.0]]);
        assert_eq!(p.loaded_vram(), 50);
        p.set_vram_budget(Some(25));
        let plan = step(&mut p, &[[50.0, 50.0]]);
        let mut shed: Vec<i64> = plan.unloads.iter().map(|u| u.cell.x).collect();
        shed.sort_unstable();
        assert_eq!(shed, vec![2, 3, 4]);
        assert!(plan.unloads.iter().all(|u| u.reason == UnloadReason::OverBudget));
        assert_eq!(p.loaded_vram(), 20);
        // `apply` refuses a load past the bound.
        let mut sneak = StreamPlan::default();
        sneak.loads.push(LoadOrder {
            cell: CellCoord::new(4, 0),
            distance: 0.0,
            bytes: 1,
            priority: 0,
        });
        sneak.loads.push(LoadOrder {
            cell: CellCoord::new(3, 0),
            distance: 0.0,
            bytes: 1,
            priority: 0,
        });
        assert_eq!(
            p.apply(&sneak),
            Err(StreamError::OverBudget {
                resident: 40,
                budget: 25
            })
        );
    }

    #[test]
    fn draining_vram_counts_until_released() {
        let mut p = row_vram(3, 1, 10, cfg(150.0, 160.0, 1000));
        p.set_vram_budget(Some(20));
        let plan = p.plan_views(&[view(50.0, 50.0)]).expect("plan");
        p.apply(&plan).expect("apply");
        assert_eq!(cells_of(&plan), vec![0, 1]);
        // Walk away: cell 0 drains and still holds its VRAM, so cell 2 waits.
        p.set_draining(CellCoord::new(0, 0), true).expect("known");
        let plan = p.plan_views(&[view(250.0, 50.0)]).expect("plan");
        assert!(plan.loads.is_empty(), "{plan:?}");
        assert_eq!(p.draining_vram(), 10);
        p.set_draining(CellCoord::new(0, 0), false).expect("known");
        let plan = p.plan_views(&[view(250.0, 50.0)]).expect("plan");
        assert_eq!(cells_of(&plan), vec![2]);
    }

    /// A row of 100,000 cells and a viewer at the origin: the plan looks at the unload window,
    /// not the registry, and still loads the two cells the one-cell radius reaches.
    #[test]
    fn plan_visits_the_unload_window_only() {
        let config = cfg(100.0, 200.0, u64::MAX);
        let planner = row(100_000, 10, config);
        let plan = planner.plan(&[[0.0, 0.0]]).expect("plan");
        let visited = last_plan_cells_visited();
        assert!(
            visited < 80,
            "visited {visited} registered cells; the unload window is a few cells, not the row"
        );
        assert!(visited > 0, "the window is not empty");
        let loads: Vec<i64> = plan.loads.iter().map(|load| load.cell.x).collect();
        assert_eq!(loads, vec![0, 1], "{plan:?}");
        assert!(plan.unloads.is_empty(), "{plan:?}");
        assert!(plan.deferred.is_empty(), "{plan:?}");
        assert_eq!(plan.resident_bytes_after, 20);
    }

    /// Candidates fit the budget on their own, but a cell that must stay loaded leaves too little
    /// room and the hysteresis ring cannot cover the gap. The ring's bytes are totaled once, not
    /// once per candidate. A cell bigger than the whole budget is `TooLarge` and never reaches
    /// this sum, so these candidates stay under the budget.
    #[test]
    fn over_budget_sums_the_ring_once() {
        let mut config = cfg(1_000.0, 2_000.0, 100);
        config.max_loads_per_step = 0;
        let mut planner = StreamPlanner::new(StreamGrid::new(100.0).expect("grid"), config).expect("config");
        // (0, 0) holds the viewer and must stay. (1, 0)..=(8, 0) are candidates.
        planner
            .register_cell(
                CellCoord::new(0, 0),
                CellInfo {
                    bytes: 50,
                    priority: 0,
                },
            )
            .expect("unique");
        planner.set_loaded(CellCoord::new(0, 0), true).expect("known");
        for x in 1..=8 {
            planner
                .register_cell(
                    CellCoord::new(x, 0),
                    CellInfo {
                        bytes: 80,
                        priority: 0,
                    },
                )
                .expect("unique");
        }
        // Distance 1450 m: outside the load radius, inside the unload radius.
        planner
            .register_cell(
                CellCoord::new(0, 15),
                CellInfo {
                    bytes: 20,
                    priority: 0,
                },
            )
            .expect("unique");
        planner.set_loaded(CellCoord::new(0, 15), true).expect("known");
        let plan = planner.plan(&[[50.0, 50.0]]).expect("plan");
        assert_eq!(
            last_plan_ring_sums(),
            1,
            "ring sums for {} deferred loads",
            plan.deferred.len()
        );
        assert_eq!(plan.deferred.len(), 8, "{plan:?}");
        assert!(
            plan.deferred.iter().all(|d| d.reason == DeferReason::OverBudget),
            "{plan:?}"
        );
        assert!(plan.loads.is_empty(), "{plan:?}");
        assert!(plan.unloads.is_empty(), "{plan:?}");
    }
}
