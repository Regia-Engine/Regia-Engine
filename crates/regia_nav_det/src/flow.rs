// SPDX-License-Identifier: MPL-2.0
//! Flow fields on the deterministic grid: one search toward a goal set, then every mover on the map
//! reads its next step from the field instead of planning its own route.
//!
//! - [`FlowField`]: an integration field (integer cost to the nearest goal, Dijkstra over the 8
//!   neighbors with the grid's own step rule) plus one direction byte per cell. Memory: 5 bytes per
//!   cell of its window (`u32` integration + `u8` direction), [`FlowField::memory_bytes`].
//! - [`ClusterFlow`]: for large maps. An abstract route over the grid's per-cluster regions
//!   ([`NavGrid::cluster_cells`] a side, 8 by default; the same regions the two-level search
//!   uses, at either label width, ADR-8839), then a small local field per
//!   region built on demand, seeded from the next region's field so the costs chain to the goal.
//! - [`FlowCache`]: fields keyed by (goal set hash, profile, grid generation), least recently used
//!   out at a fixed capacity; clustered flows keep their local fields across a stamp change and
//!   rebuild only the ones the change touched ([`StampSync::dirty_clusters`]).
//!
//! Every decision is an integer: step costs are the grid's (10 straight, 14 diagonal, times the
//! entered cell's cost multiplier over [`COST_FLAT`]), ties break on `(cost, cell index)`, and no
//! container is a hash map. The only floats are in sampling ([`FlowField::direction_at`]): unit
//! vectors, an optional bilinear blend (`+ - * /` and an exact `floor`), and the line-of-sight
//! direction, normalized with [`regia_detsim::math::sqrt`].
//!
//! Method: Dijkstra (1959) for the integration field; the cluster flow follows Emerson, "Crowd
//! Pathfinding and Steering Using Flow Field Tiles" (Game AI Pro, 2013): an abstract route over
//! map tiles, then per-tile fields seeded from the tile downstream. Original code.
//!
//! ```
//! use std::sync::Arc;
//! use regia_nav_det::flow::{FlowField, FlowSettings};
//! use regia_nav_det::grid::{GridSettings, NavGrid, NavStatic, Profile};
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! let settings = Arc::new(GridSettings::hover_ground_jet());
//! let grid = NavGrid::new(Arc::new(NavStatic::build(None, settings)?))?;
//! let goal = grid.cell_at(100.0, 0.0);
//! let field = FlowField::build(&grid, Profile::HOVER, &[goal], FlowSettings::default());
//! // West of the goal on open ground: straight east.
//! assert_eq!(field.direction_at([50.0, 0.0]), Some([1.0, 0.0]));
//! assert_eq!(field.integration(goal), Some(0));
//! # Ok(())
//! # }
//! ```

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use regia_detsim::debug::{DebugCapture, DebugKind, DebugSink, DebugSource, DebugWhat};
use regia_detsim::math;
use serde::{Deserialize, Serialize};

use super::grid::{COST_FLAT, LabelsRef, NEIGHBOURS_8, NavGrid, Profile, StampSync};
use super::search::octile;

/// Low four bits of a direction byte: the move, an index into [`NEIGHBOURS_8`] (0..=7), or
/// [`DIR_GOAL`] / [`DIR_NONE`].
pub const DIR_MASK: u8 = 0x0F;
/// Direction byte of a goal cell.
pub const DIR_GOAL: u8 = 8;
/// Direction byte of a cell no goal can be reached from (or one outside the field's region).
pub const DIR_NONE: u8 = 9;
/// Flag bit of a direction byte: a clear grid line of sight to the field's single goal (steer
/// straight at it, see [`FlowSettings::los_cells`]).
pub const FLAG_LOS: u8 = 0x10;
/// Integration value of a cell no goal can be reached from.
pub const UNREACHED: u32 = u32::MAX;
/// Bytes a field stores per cell of its window: a `u32` integration value and a `u8` direction.
pub const BYTES_PER_CELL: usize = 5;

/// Unit vectors of the 8 moves, in [`NEIGHBOURS_8`] order (diagonals `1/sqrt(2)` per axis).
const UNIT: [[f32; 2]; 8] = {
    const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
    [
        [1.0, 0.0],
        [-1.0, 0.0],
        [0.0, 1.0],
        [0.0, -1.0],
        [S, S],
        [-S, S],
        [S, -S],
        [-S, -S],
    ]
};

/// How a field is built and sampled. Every peer of a lockstep match must use the same settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FlowSettings {
    /// Line-of-sight smoothing: a cell within this many cells (Chebyshev) of the goal that has a
    /// clear grid line of sight to it ([`NavGrid::los`]) gets [`FLAG_LOS`], and sampling steers
    /// straight at the goal from there. 0 (the default) is off. Only a field with exactly one goal
    /// sets the flag; a clustered flow never does.
    pub los_cells: u32,
    /// Costliest cell multiplier the line may cross (default [`COST_FLAT`]: open ground only).
    pub los_max_cost: u32,
    /// Bilinear sampling: [`FlowField::direction_at`] blends the vectors of the four cells around
    /// the point (weights by position, unreachable cells left out) instead of reading the nearest
    /// cell. Off by default. The blend uses only `+ - * /`; the result is not normalized (its
    /// length is at most 1).
    pub blend: bool,
}

impl Default for FlowSettings {
    fn default() -> Self {
        FlowSettings {
            los_cells: 0,
            los_max_cost: COST_FLAT,
            blend: false,
        }
    }
}

/// A rectangle of cells, `x0..x1` by `z0..z1` (half open), in grid cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CellRect {
    /// First column.
    pub x0: u32,
    /// First row.
    pub z0: u32,
    /// One past the last column.
    pub x1: u32,
    /// One past the last row.
    pub z1: u32,
}

impl CellRect {
    /// The whole grid.
    pub fn whole(grid: &NavGrid) -> CellRect {
        CellRect {
            x0: 0,
            z0: 0,
            x1: grid.width() as u32,
            z1: grid.height() as u32,
        }
    }

    /// One cluster's cells ([`NavGrid::cluster_cells`] a side, cut at the grid's edge).
    pub fn cluster(grid: &NavGrid, cluster: usize) -> CellRect {
        let side = grid.cluster_cells();
        let (cx, cz) = (cluster % grid.clusters_x, cluster / grid.clusters_x);
        let (x0, z0) = (cx * side, cz * side);
        CellRect {
            x0: x0 as u32,
            z0: z0 as u32,
            x1: (x0 + side).min(grid.width()) as u32,
            z1: (z0 + side).min(grid.height()) as u32,
        }
    }

    /// The rectangle cut to the grid (an empty rectangle when nothing is left).
    pub fn clipped(self, grid: &NavGrid) -> CellRect {
        let (w, h) = (grid.width() as u32, grid.height() as u32);
        let x1 = self.x1.min(w);
        let z1 = self.z1.min(h);
        CellRect {
            x0: self.x0.min(x1),
            z0: self.z0.min(z1),
            x1,
            z1,
        }
    }

    /// Columns.
    #[inline]
    pub fn width(&self) -> usize {
        self.x1.saturating_sub(self.x0) as usize
    }

    /// Rows.
    #[inline]
    pub fn height(&self) -> usize {
        self.z1.saturating_sub(self.z0) as usize
    }

    /// Cells.
    #[inline]
    pub fn area(&self) -> usize {
        self.width() * self.height()
    }

    /// True when cell `(x, z)` is inside.
    #[inline]
    pub fn contains(&self, x: usize, z: usize) -> bool {
        x >= self.x0 as usize && x < self.x1 as usize && z >= self.z0 as usize && z < self.z1 as usize
    }
}

/// The move a field gives one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowDir {
    /// Step to the neighbor `NEIGHBOURS_8[k]`.
    Step(u8),
    /// The cell is a goal.
    Goal,
    /// No goal is reachable (or the cell is outside the field).
    Unreachable,
}

/// Work one build did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FlowStats {
    /// Cells taken off the queue (each once).
    pub expansions: u32,
    /// Cells with a finite integration value.
    pub reached: u32,
    /// Cells flagged [`FLAG_LOS`].
    pub los_cells: u32,
}

/// One arrow of a field's debug view (see [`FlowField::arrows`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FlowArrow {
    /// World XZ of the cell center.
    pub from: [f32; 2],
    /// World XZ the arrow points at: the next cell's center, the goal for a line-of-sight cell,
    /// `from` itself at a goal.
    pub to: [f32; 2],
    /// The cell steers straight at the goal ([`FLAG_LOS`]).
    pub los: bool,
    /// The cell is a goal.
    pub goal: bool,
}

/// A capped window of a field, for tools and agents (see [`FlowField::view`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlowView {
    /// The cells covered (inside the field's window).
    pub rect: CellRect,
    /// Direction bytes, row-major over `rect`.
    pub dirs: Vec<u8>,
    /// Integration values, row-major over `rect` ([`UNREACHED`] when no goal is reachable).
    pub integration: Vec<u32>,
    /// True when the request was larger than the cap and `rect` was cut.
    pub truncated: bool,
}

/// An integration field and a direction per cell toward the nearest goal of a goal set.
///
/// Built once ([`FlowField::build`]) and read by any number of movers: [`FlowField::direction_at`]
/// for a steering vector, [`FlowField::next_cell`] for a cell walk. A field built over the whole
/// grid covers every cell; a clustered flow's local fields cover one cluster's region.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowField {
    profile: Profile,
    window: CellRect,
    stride: u32,
    origin: [f32; 2],
    cell_m: f32,
    goals: Vec<u32>,
    settings: FlowSettings,
    integration: Vec<u32>,
    dirs: Vec<u8>,
    stats: FlowStats,
}

/// The cost of one move into `target`: 10 straight or 14 diagonal, times the target's cost
/// multiplier over [`COST_FLAT`] (the cell search's rule). 0 when the target is blocked.
#[inline]
pub fn step_cost(grid: &NavGrid, profile: Profile, target: u32, diagonal: bool) -> u32 {
    let base = if diagonal { 14 } else { 10 };
    base * grid.cost(profile, target) / COST_FLAT
}

/// True when the profile may move from `cell` by `(dx, dz)` (one of the 8 unit moves): the target
/// is on the grid and passable, the terrain allows the step ([`NavGrid::can_step`]), and a
/// diagonal cuts no corner (both side cells passable, both side steps allowed).
pub fn can_move(grid: &NavGrid, profile: Profile, cell: u32, dx: i32, dz: i32) -> bool {
    let (x, z) = grid.xz(cell);
    let (nx, nz) = (x as i32 + dx, z as i32 + dz);
    if nx < 0 || nz < 0 || nx >= grid.width() as i32 || nz >= grid.height() as i32 {
        return false;
    }
    if !grid.passable(profile, grid.index(nx as usize, nz as usize)) {
        return false;
    }
    side_rule(grid, profile, cell, dx, dz)
}

/// The terrain and corner rule of a move whose target is already known to be on the grid and open.
#[inline]
fn side_rule(grid: &NavGrid, profile: Profile, cell: u32, dx: i32, dz: i32) -> bool {
    if !grid.can_step(profile, cell, dx, dz) {
        return false;
    }
    if dx == 0 || dz == 0 {
        return true;
    }
    let (x, z) = grid.xz(cell);
    let a = grid.index((x as i32 + dx) as usize, z);
    let b = grid.index(x, (z as i32 + dz) as usize);
    grid.passable(profile, a)
        && grid.passable(profile, b)
        && grid.can_step(profile, cell, dx, 0)
        && grid.can_step(profile, cell, 0, dz)
}

/// Sorted, deduplicated goal cells inside the grid.
fn normal_goals(grid: &NavGrid, goals: &[u32]) -> Vec<u32> {
    let n = (grid.width() * grid.height()) as u32;
    let mut out: Vec<u32> = goals.iter().copied().filter(|g| *g < n).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// FNV-1a 64 of a sorted goal list.
pub fn goal_set_hash(goals: &[u32]) -> u64 {
    let mut h = Fnv::new();
    h.u64(goals.len() as u64);
    for g in goals {
        h.u32(*g);
    }
    h.finish()
}

/// What a cell may belong to in one integration.
#[derive(Clone, Copy)]
enum Member {
    /// Every passable cell of the window.
    Passable,
    /// Cells of one region label (a clustered flow's local field).
    Label(usize, u16),
}

impl Member {
    #[inline]
    fn holds(self, grid: &NavGrid, profile: Profile, cell: u32) -> bool {
        match self {
            Member::Passable => grid.passable(profile, cell),
            Member::Label(pi, label) => label_at(grid.labels_ref(pi), cell) == Some(label),
        }
    }
}

impl FlowField {
    /// Build the field of `goals` (cell indices) over the whole grid for `profile`.
    ///
    /// Goals off the grid or in a cell the profile cannot stand in are dropped; with none left
    /// every cell is unreachable. Cost: one Dijkstra over the reachable cells (each expanded once,
    /// 8 neighbors); memory [`BYTES_PER_CELL`] per grid cell.
    pub fn build(grid: &NavGrid, profile: Profile, goals: &[u32], settings: FlowSettings) -> FlowField {
        let goals = normal_goals(grid, goals);
        let _span = tracing::trace_span!("det_flow_build", cells = grid.width() * grid.height()).entered();
        let mut field = integrate(
            grid,
            profile,
            CellRect::whole(grid),
            Member::Passable,
            &goals,
            &[],
            settings,
        );
        field.apply_los(grid);
        field
    }

    /// The profile the field was built for.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The cells the field covers (the whole grid, or one cluster for a clustered flow's field).
    pub fn window(&self) -> CellRect {
        self.window
    }

    /// The goal cells it was built from (sorted; impassable ones dropped).
    pub fn goals(&self) -> &[u32] {
        &self.goals
    }

    /// The settings it was built with.
    pub fn settings(&self) -> FlowSettings {
        self.settings
    }

    /// Work the build did.
    pub fn stats(&self) -> FlowStats {
        self.stats
    }

    /// Integration values, row-major over [`Self::window`] ([`UNREACHED`] when unreachable).
    pub fn integration_values(&self) -> &[u32] {
        &self.integration
    }

    /// Direction bytes, row-major over [`Self::window`] (see [`DIR_MASK`], [`FLAG_LOS`]).
    pub fn dir_bytes(&self) -> &[u8] {
        &self.dirs
    }

    /// Heap bytes plus the struct: [`BYTES_PER_CELL`] per window cell and 4 per goal.
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.integration.capacity() * 4
            + self.dirs.capacity()
            + self.goals.capacity() * 4
    }

    #[inline]
    fn local(&self, cell: u32) -> Option<usize> {
        let (x, z) = ((cell % self.stride) as usize, (cell / self.stride) as usize);
        local_index(self.window, x, z)
    }

    /// Integer cost from the cell to its nearest goal, or `None` when unreachable or outside.
    pub fn integration(&self, cell: u32) -> Option<u32> {
        let v = *self.integration.get(self.local(cell)?)?;
        (v != UNREACHED).then_some(v)
    }

    /// The cell's move.
    pub fn dir(&self, cell: u32) -> FlowDir {
        match self.local(cell).map(|li| self.dirs[li] & DIR_MASK) {
            Some(DIR_GOAL) => FlowDir::Goal,
            Some(k) if k < 8 => FlowDir::Step(k),
            _ => FlowDir::Unreachable,
        }
    }

    /// True when the cell has a clear line of sight to the single goal ([`FLAG_LOS`]).
    pub fn los(&self, cell: u32) -> bool {
        self.local(cell).is_some_and(|li| self.dirs[li] & FLAG_LOS != 0)
    }

    /// The cell one move along the field, or `None` at a goal or an unreachable cell.
    pub fn next_cell(&self, cell: u32) -> Option<u32> {
        match self.dir(cell) {
            FlowDir::Step(k) => {
                let (dx, dz) = NEIGHBOURS_8[k as usize];
                let stride = self.stride as i64;
                Some((cell as i64 + dz as i64 * stride + dx as i64) as u32)
            }
            _ => None,
        }
    }

    /// FNV-1a 64 over the window, the goals, the settings, every integration value and direction
    /// byte. The same on every peer and architecture for the same grid and inputs.
    pub fn bits_hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.u32(u32::from(self.profile.0));
        for v in [self.window.x0, self.window.z0, self.window.x1, self.window.z1] {
            h.u32(v);
        }
        h.u64(self.goals.len() as u64);
        for g in &self.goals {
            h.u32(*g);
        }
        h.u32(self.settings.los_cells);
        h.u32(self.settings.los_max_cost);
        h.u32(u32::from(self.settings.blend));
        for v in &self.integration {
            h.u32(*v);
        }
        h.bytes(&self.dirs);
        h.finish()
    }

    /// World XZ of a cell center.
    #[inline]
    fn center(&self, x: usize, z: usize) -> [f32; 2] {
        [
            self.origin[0] + x as f32 * self.cell_m,
            self.origin[1] + z as f32 * self.cell_m,
        ]
    }

    /// The steering vector of one window cell for a mover at `pos`.
    fn cell_vector(&self, li: usize, pos: [f32; 2]) -> Option<[f32; 2]> {
        let d = self.dirs[li];
        match d & DIR_MASK {
            DIR_GOAL => Some([0.0, 0.0]),
            k if k < 8 => {
                if d & FLAG_LOS != 0
                    && let Some(g) = self.goals.first()
                {
                    let (gx, gz) = ((g % self.stride) as usize, (g / self.stride) as usize);
                    let goal = self.center(gx, gz);
                    let (dx, dz) = (goal[0] - pos[0], goal[1] - pos[1]);
                    let len = math::sqrt(dx * dx + dz * dz);
                    if len > 0.0 {
                        return Some([dx / len, dz / len]);
                    }
                    return Some([0.0, 0.0]);
                }
                Some(UNIT[k as usize])
            }
            _ => None,
        }
    }

    /// The steering direction at a world point: the nearest cell's unit move (or, with
    /// [`FlowSettings::blend`], the bilinear blend of the four cells around the point). `[0, 0]` at a
    /// goal; toward the goal itself in a [`FLAG_LOS`] cell; `None` off the field or where no goal is
    /// reachable. A point on a blocked cell (a mover pushed into a wall) gets `None`: step it back
    /// with [`NavGrid::nearest_passable`].
    pub fn direction_at(&self, pos: [f32; 2]) -> Option<[f32; 2]> {
        let fx = (pos[0] - self.origin[0]) / self.cell_m;
        let fz = (pos[1] - self.origin[1]) / self.cell_m;
        if !self.settings.blend {
            let (cx, cz) = ((fx + 0.5).floor(), (fz + 0.5).floor());
            if cx.is_nan() || cz.is_nan() || cx < 0.0 || cz < 0.0 {
                return None;
            }
            let li = local_index(self.window, cx as usize, cz as usize)?;
            return self.cell_vector(li, pos);
        }
        let (x0, z0) = (fx.floor(), fz.floor());
        let (tx, tz) = (fx - x0, fz - z0);
        let mut sum = [0.0f32, 0.0f32];
        let mut weight = 0.0f32;
        for (ox, oz, w) in [
            (0.0, 0.0, (1.0 - tx) * (1.0 - tz)),
            (1.0, 0.0, tx * (1.0 - tz)),
            (0.0, 1.0, (1.0 - tx) * tz),
            (1.0, 1.0, tx * tz),
        ] {
            let (cx, cz) = (x0 + ox, z0 + oz);
            if cx.is_nan() || cz.is_nan() || cx < 0.0 || cz < 0.0 || w <= 0.0 {
                continue;
            }
            let Some(li) = local_index(self.window, cx as usize, cz as usize) else {
                continue;
            };
            if let Some(v) = self.cell_vector(li, pos) {
                sum[0] += v[0] * w;
                sum[1] += v[1] * w;
                weight += w;
            }
        }
        (weight > 0.0).then(|| [sum[0] / weight, sum[1] / weight])
    }

    /// Debug arrows for the reachable cells of `rect` (cut to the field), at most `cap`, row-major.
    /// A host hands them to its debug draw (or uses [`Self::debug_emit`] / [`FlowDebug`]).
    pub fn arrows(&self, rect: CellRect, cap: usize) -> Vec<FlowArrow> {
        let mut out = Vec::new();
        self.for_each_arrow(rect, cap, |arrow| out.push(arrow));
        out
    }

    /// Draw the arrows of `rect` (cut to the field), at most `cap`, row-major, into a debug sink at
    /// height `y` ([`DebugKind::Flow`], ADR-8839): a goal cell is a circle of 0.35 cells; any other
    /// reachable cell is a shaft 0.8 cells long from its center toward `FlowArrow::to` plus a
    /// two-line head. Returns the arrows drawn. Reads the field only and allocates nothing.
    pub fn debug_emit(&self, sink: &mut dyn DebugSink, rect: CellRect, cap: usize, y: f32) -> usize {
        let cell = self.cell_m;
        self.for_each_arrow(rect, cap, |arrow| {
            let [fx, fz] = arrow.from;
            if arrow.goal {
                sink.circle(DebugKind::Flow, [fx, y, fz], cell * 0.35);
                return;
            }
            let (dx, dz) = (arrow.to[0] - fx, arrow.to[1] - fz);
            let len = math::sqrt(dx * dx + dz * dz);
            if len <= 0.0 {
                return;
            }
            let (ux, uz) = (dx / len, dz / len);
            let tip = [fx + ux * cell * 0.8, y, fz + uz * cell * 0.8];
            let back = [tip[0] - ux * cell * 0.3, tip[2] - uz * cell * 0.3];
            let (sx, sz) = (-uz * cell * 0.2, ux * cell * 0.2);
            sink.line(DebugKind::Flow, [fx, y, fz], tip);
            sink.line(DebugKind::Flow, tip, [back[0] + sx, y, back[1] + sz]);
            sink.line(DebugKind::Flow, tip, [back[0] - sx, y, back[1] - sz]);
        })
    }

    /// Visit the arrows of the reachable cells of `rect` (cut to the field), at most `cap`,
    /// row-major. Returns the count visited.
    fn for_each_arrow(&self, rect: CellRect, cap: usize, mut visit: impl FnMut(FlowArrow)) -> usize {
        let r = intersect(rect, self.window);
        let mut count = 0usize;
        for z in r.z0 as usize..r.z1 as usize {
            for x in r.x0 as usize..r.x1 as usize {
                if count >= cap {
                    return count;
                }
                let Some(li) = local_index(self.window, x, z) else {
                    continue;
                };
                let d = self.dirs[li];
                let from = self.center(x, z);
                let k = d & DIR_MASK;
                let los = d & FLAG_LOS != 0;
                let to = if k == DIR_GOAL {
                    from
                } else if k < 8 {
                    match (los, self.goals.first()) {
                        (true, Some(g)) => {
                            self.center((g % self.stride) as usize, (g / self.stride) as usize)
                        }
                        _ => {
                            let (dx, dz) = NEIGHBOURS_8[k as usize];
                            self.center((x as i32 + dx) as usize, (z as i32 + dz) as usize)
                        }
                    }
                } else {
                    continue;
                };
                visit(FlowArrow {
                    from,
                    to,
                    los,
                    goal: k == DIR_GOAL,
                });
                count += 1;
            }
        }
        count
    }

    /// The raw bytes of `rect` (cut to the field), at most `cap` cells: the rectangle shrinks row by
    /// row (then column by column) until it fits, and `truncated` says so.
    pub fn view(&self, rect: CellRect, cap: usize) -> FlowView {
        let (r, truncated) = cap_rect(intersect(rect, self.window), cap);
        let mut dirs = Vec::with_capacity(r.area());
        let mut integration = Vec::with_capacity(r.area());
        for z in r.z0 as usize..r.z1 as usize {
            for x in r.x0 as usize..r.x1 as usize {
                if let Some(li) = local_index(self.window, x, z) {
                    dirs.push(self.dirs[li]);
                    integration.push(self.integration[li]);
                }
            }
        }
        FlowView {
            rect: r,
            dirs,
            integration,
            truncated,
        }
    }

    /// Flag the cells with a clear line of sight to the single goal.
    fn apply_los(&mut self, grid: &NavGrid) {
        let reach = self.settings.los_cells;
        if reach == 0 || self.goals.len() != 1 {
            return;
        }
        let goal = self.goals[0];
        if self.integration(goal) != Some(0) {
            return;
        }
        let (gx, gz) = grid.xz(goal);
        let w = self.window;
        let lo_x = (gx as u32).saturating_sub(reach).max(w.x0);
        let lo_z = (gz as u32).saturating_sub(reach).max(w.z0);
        let hi_x = (gx as u32).saturating_add(reach).saturating_add(1).min(w.x1);
        let hi_z = (gz as u32).saturating_add(reach).saturating_add(1).min(w.z1);
        let mut flagged = 0u32;
        for z in lo_z as usize..hi_z as usize {
            for x in lo_x as usize..hi_x as usize {
                let Some(li) = local_index(w, x, z) else {
                    continue;
                };
                if self.dirs[li] & DIR_MASK >= 8 {
                    continue;
                }
                let cell = grid.index(x, z);
                if grid.los(self.profile, cell, goal, self.settings.los_max_cost, 0) {
                    self.dirs[li] |= FLAG_LOS;
                    flagged += 1;
                }
            }
        }
        self.stats.los_cells = flagged;
    }
}

/// Shrink `r` row by row (then column by column) until it holds at most `cap` cells; true when it
/// was cut.
fn cap_rect(mut r: CellRect, cap: usize) -> (CellRect, bool) {
    let mut truncated = false;
    while r.area() > cap && r.height() > 1 {
        r.z1 -= 1;
        truncated = true;
    }
    while r.area() > cap && r.width() > 0 {
        r.x1 -= 1;
        truncated = true;
    }
    (r, truncated)
}

#[inline]
fn local_index(w: CellRect, x: usize, z: usize) -> Option<usize> {
    w.contains(x, z)
        .then(|| (z - w.z0 as usize) * w.width() + (x - w.x0 as usize))
}

fn intersect(a: CellRect, b: CellRect) -> CellRect {
    let x0 = a.x0.max(b.x0);
    let z0 = a.z0.max(b.z0);
    let x1 = a.x1.min(b.x1).max(x0);
    let z1 = a.z1.min(b.z1).max(z0);
    CellRect { x0, z0, x1, z1 }
}

/// Dijkstra from the goals (cost 0, inside `window`) and the outer seeds (cells outside `window`
/// with a fixed value, sorted by cell) over the member cells of `window`, then one direction per
/// reached cell: the legal move minimizing `(neighbor value + step, neighbor index)`.
fn integrate(
    grid: &NavGrid,
    profile: Profile,
    window: CellRect,
    member: Member,
    goals: &[u32],
    outer: &[(u32, u32)],
    settings: FlowSettings,
) -> FlowField {
    let n = window.area();
    let mut integ = vec![UNREACHED; n];
    let mut dirs = vec![DIR_NONE; n];
    let mut closed = vec![false; n];
    let (w, h) = (grid.width() as i32, grid.height() as i32);
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
    let mut kept_goals = Vec::with_capacity(goals.len());
    for &g in goals {
        let (x, z) = grid.xz(g);
        if let Some(li) = local_index(window, x, z)
            && member.holds(grid, profile, g)
        {
            integ[li] = 0;
            heap.push(Reverse((0, g)));
            kept_goals.push(g);
        }
    }
    for &(cell, value) in outer {
        heap.push(Reverse((value, cell)));
    }
    let outer_value = |cell: u32| -> Option<u32> {
        outer
            .binary_search_by_key(&cell, |s| s.0)
            .ok()
            .map(|i| outer[i].1)
    };
    let mut stats = FlowStats::default();
    while let Some(Reverse((v, cell))) = heap.pop() {
        let (x, z) = grid.xz(cell);
        if let Some(li) = local_index(window, x, z) {
            if closed[li] || integ[li] != v {
                continue;
            }
            closed[li] = true;
            stats.expansions += 1;
            stats.reached += 1;
            if v == 0 && kept_goals.binary_search(&cell).is_ok() {
                dirs[li] = DIR_GOAL;
            } else {
                // The best finished neighbor: (value + step, index), lowest first.
                let mut best: Option<(u32, u32, u8)> = None;
                for (k, (dx, dz)) in NEIGHBOURS_8.iter().enumerate() {
                    let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                    if nx < 0 || nz < 0 || nx >= w || nz >= h {
                        continue;
                    }
                    let next = grid.index(nx as usize, nz as usize);
                    let value = match local_index(window, nx as usize, nz as usize) {
                        Some(nl) if closed[nl] => integ[nl],
                        Some(_) => continue,
                        None => match outer_value(next) {
                            Some(value) => value,
                            None => continue,
                        },
                    };
                    if !grid.passable(profile, next) || !side_rule(grid, profile, cell, *dx, *dz) {
                        continue;
                    }
                    let total = value.saturating_add(step_cost(grid, profile, next, *dx != 0 && *dz != 0));
                    if best.is_none_or(|(bv, bn, _)| total < bv || (total == bv && next < bn)) {
                        best = Some((total, next, k as u8));
                    }
                }
                if let Some((_, _, k)) = best {
                    dirs[li] = k;
                }
            }
        }
        // Relax every member cell that can move into this one.
        let entered = grid.cost(profile, cell);
        if entered == 0 {
            continue;
        }
        for (dx, dz) in NEIGHBOURS_8 {
            let (px, pz) = (x as i32 - dx, z as i32 - dz);
            if px < 0 || pz < 0 || px >= w || pz >= h {
                continue;
            }
            let Some(pl) = local_index(window, px as usize, pz as usize) else {
                continue;
            };
            if closed[pl] {
                continue;
            }
            let pred = grid.index(px as usize, pz as usize);
            if !member.holds(grid, profile, pred) || !side_rule(grid, profile, pred, dx, dz) {
                continue;
            }
            let base = if dx != 0 && dz != 0 { 14 } else { 10 };
            let nv = v.saturating_add(base * entered / COST_FLAT).min(UNREACHED - 1);
            if nv < integ[pl] {
                integ[pl] = nv;
                heap.push(Reverse((nv, pred)));
            }
        }
    }
    FlowField {
        profile,
        window,
        stride: grid.width() as u32,
        origin: [grid.stat.origin_x, grid.stat.origin_z],
        cell_m: grid.cell_m(),
        goals: kept_goals,
        settings,
        integration: integ,
        dirs,
        stats,
    }
}

// ------------------------------------------------------------------------------------- debug draw

/// A [`DebugSource`] that draws one field's arrows when the switch holds [`DebugWhat::FLOW`]
/// (ADR-8839): hand it to the framework's `DetDebugDraw::record` next to the indexes, or call
/// [`DebugSource::debug_emit`] from a sim host's `det_debug`. It has no capture (drawing reads the
/// field only), so `set_debug_capture` does nothing. See [`FlowField::debug_emit`] for the shapes.
#[derive(Debug, Clone, Copy)]
pub struct FlowDebug<'a> {
    /// The field drawn.
    pub field: &'a FlowField,
    /// The cells drawn (cut to the field).
    pub rect: CellRect,
    /// Most arrows drawn.
    pub cap: usize,
    /// Height the arrows are drawn at (world units).
    pub y: f32,
}

impl<'a> FlowDebug<'a> {
    /// Arrows drawn by default: 4096 (a 64 x 64 cell window).
    pub const DEFAULT_CAP: usize = 4096;

    /// The whole field, at most [`Self::DEFAULT_CAP`] arrows, at height 0.
    pub fn new(field: &'a FlowField) -> Self {
        FlowDebug {
            field,
            rect: field.window(),
            cap: Self::DEFAULT_CAP,
            y: 0.0,
        }
    }
}

impl DebugSource for FlowDebug<'_> {
    fn set_debug_capture(&mut self, _capture: Option<DebugCapture>) {}

    fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat) {
        if what.contains(DebugWhat::FLOW) {
            self.field.debug_emit(sink, self.rect, self.cap, self.y);
        }
    }
}

// ------------------------------------------------------------------------------------ cluster flow

/// A region of one cluster: `(cluster id, local label)`. Stable while the cluster is not relabeled.
/// The label is [`NavGrid::region_label`]'s, so it holds both label widths (ADR-8839).
pub type RegionKey = (u32, u16);

/// The region label of `cell` in one profile's labels, `None` when the cell is blocked.
#[inline]
fn label_at(labels: LabelsRef<'_>, cell: u32) -> Option<u16> {
    let raw = labels.raw(cell as usize);
    (raw != labels.blocked()).then_some(raw as u16)
}

/// The abstract route: per region, the cost to the nearest goal region and the next region.
#[derive(Debug, Clone, Default)]
struct RegionRoute {
    /// Per cluster: the first node id; `clusters + 1` entries.
    offsets: Vec<u32>,
    /// Per node: abstract cost to a goal region, [`UNREACHED`] when none.
    cost: Vec<u32>,
    /// Per node: the next node toward the goal (itself for a goal region).
    next: Vec<u32>,
    /// Nodes taken off the queue.
    expansions: u32,
    /// Abstract edges.
    edges: u32,
}

impl RegionRoute {
    #[inline]
    fn node(&self, key: RegionKey) -> Option<u32> {
        let c = key.0 as usize;
        let first = *self.offsets.get(c)?;
        let end = *self.offsets.get(c + 1)?;
        let node = first + u32::from(key.1);
        (node < end).then_some(node)
    }

    fn key(&self, node: u32) -> RegionKey {
        key_at(&self.offsets, node)
    }

    /// Regions per cluster from the grid's labels, the region graph (every legal move from a
    /// region's cell into another region is an edge), then Dijkstra from the goal regions.
    /// Edge cost: the two-level search's coarse rule, the octile distance between representatives
    /// (at least 10) times the mean of the two regions' cost multipliers over [`COST_FLAT`].
    fn build(grid: &NavGrid, profile: Profile, goals: &[u32]) -> RegionRoute {
        let _span = tracing::trace_span!("det_flow_route").entered();
        let pi = grid.settings().profile_index(profile);
        let clusters = grid.clusters_x * grid.clusters_z;
        let mut offsets = Vec::with_capacity(clusters + 1);
        let mut total = 0u32;
        for c in 0..clusters {
            offsets.push(total);
            total += grid.regions[pi][c].reps.len() as u32;
        }
        offsets.push(total);
        let labels = grid.labels_ref(pi);
        let node_of = |cell: u32| -> Option<u32> {
            label_at(labels, cell).map(|label| offsets[grid.cluster_of(cell)] + u32::from(label))
        };
        // In-edges `(to, from)`: every move out of a cluster's border cells into another cluster.
        let (w, h) = (grid.width(), grid.height());
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        for c in 0..clusters {
            let r = CellRect::cluster(grid, c);
            for z in r.z0 as usize..r.z1 as usize {
                let edge_row = z == r.z0 as usize || z + 1 == r.z1 as usize;
                for x in r.x0 as usize..r.x1 as usize {
                    if !edge_row && x != r.x0 as usize && x + 1 != r.x1 as usize {
                        continue;
                    }
                    let cell = grid.index(x, z);
                    let Some(from) = node_of(cell) else {
                        continue;
                    };
                    for (dx, dz) in NEIGHBOURS_8 {
                        let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                        if nx < 0 || nz < 0 || nx >= w as i32 || nz >= h as i32 {
                            continue;
                        }
                        if r.contains(nx as usize, nz as usize) {
                            continue;
                        }
                        let next = grid.index(nx as usize, nz as usize);
                        let Some(to) = node_of(next) else {
                            continue;
                        };
                        if side_rule(grid, profile, cell, dx, dz) {
                            pairs.push((to, from));
                        }
                    }
                }
            }
        }
        // Inside one cluster two regions can still meet across a diagonal move: the corner rule checks
        // the side steps out of the mover's cell, not the steps into the target, while the labels
        // flood along straight steps only.
        // Only clusters with two or more regions can hold such a move.
        for c in 0..clusters {
            if grid.regions[pi][c].reps.len() < 2 {
                continue;
            }
            let r = CellRect::cluster(grid, c);
            for z in r.z0 as usize..r.z1 as usize {
                for x in r.x0 as usize..r.x1 as usize {
                    let cell = grid.index(x, z);
                    let Some(label) = label_at(labels, cell) else {
                        continue;
                    };
                    for (dx, dz) in [(1i32, 1i32), (-1, 1), (1, -1), (-1, -1)] {
                        let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                        if nx < 0 || nz < 0 || !r.contains(nx as usize, nz as usize) {
                            continue;
                        }
                        let next = grid.index(nx as usize, nz as usize);
                        if label_at(labels, next).is_none_or(|other| other == label) {
                            continue;
                        }
                        if side_rule(grid, profile, cell, dx, dz)
                            && let (Some(from), Some(to)) = (node_of(cell), node_of(next))
                        {
                            pairs.push((to, from));
                        }
                    }
                }
            }
        }
        pairs.sort_unstable();
        pairs.dedup();
        let nodes = total as usize;
        let edges = pairs.len() as u32;
        // CSR of the in-edges by target node.
        let mut first = vec![0u32; nodes + 1];
        for (to, _) in &pairs {
            first[*to as usize + 1] += 1;
        }
        for i in 0..nodes {
            first[i + 1] += first[i];
        }
        let rep_cost = |node: u32| -> (u32, u32) {
            let (c, r) = key_at(&offsets, node);
            let set = &grid.regions[pi][c as usize];
            (
                set.reps[r as usize],
                u32::from(set.costs[r as usize]).max(COST_FLAT),
            )
        };
        let mut closed = vec![false; nodes];
        let mut cost = vec![UNREACHED; nodes];
        let mut next = vec![u32::MAX; nodes];
        let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
        for g in goals {
            if grid.passable(profile, *g)
                && let Some(node) = node_of(*g)
                && cost[node as usize] != 0
            {
                cost[node as usize] = 0;
                next[node as usize] = node;
                heap.push(Reverse((0, node)));
            }
        }
        let mut expansions = 0u32;
        while let Some(Reverse((v, node))) = heap.pop() {
            let ni = node as usize;
            if closed[ni] || cost[ni] != v {
                continue;
            }
            closed[ni] = true;
            expansions += 1;
            let (rep_b, cost_b) = rep_cost(node);
            for &(_, from) in &pairs[first[ni] as usize..first[ni + 1] as usize] {
                let fi = from as usize;
                if closed[fi] {
                    continue;
                }
                let (rep_a, cost_a) = rep_cost(from);
                let step = octile(grid, rep_a, rep_b).max(10) * (cost_a + cost_b) / (2 * COST_FLAT);
                let nv = v.saturating_add(step).min(UNREACHED - 1);
                if nv < cost[fi] {
                    cost[fi] = nv;
                    next[fi] = node;
                    heap.push(Reverse((nv, from)));
                } else if nv == cost[fi] && node < next[fi] {
                    next[fi] = node;
                }
            }
        }
        RegionRoute {
            offsets,
            cost,
            next,
            expansions,
            edges,
        }
    }
}

/// The region key of a node: the last cluster whose first node is not past it.
fn key_at(offsets: &[u32], node: u32) -> RegionKey {
    let c = offsets.partition_point(|o| *o <= node).saturating_sub(1);
    (c as u32, (node - offsets[c]) as u16)
}

/// Counters of a clustered flow (and, summed, of a [`FlowCache`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ClusterStats {
    /// Abstract routes built (one at creation, one per changed stamp sync).
    pub route_builds: u32,
    /// Local fields built (first use, or a seed or route change after a sync).
    pub local_builds: u32,
    /// Local field lookups answered from the cache with no work.
    pub local_hits: u32,
    /// Local fields kept across a sync after their seeds were checked unchanged.
    pub local_revalidated: u32,
    /// Local fields dropped by a sync because their cluster or a neighbor was dirty.
    pub local_dropped: u32,
    /// Local fields evicted at the capacity (least recently used first).
    pub local_evicted: u32,
}

#[derive(Debug, Clone)]
struct LocalEntry {
    next: Option<RegionKey>,
    seeds: Vec<(u32, u32)>,
    field: FlowField,
    checked: u64,
    last_used: u64,
}

/// A flow for large maps: an abstract route over the grid's cluster regions, and per region a
/// local field (one cluster, [`BYTES_PER_CELL`] per cell) built on first use and cached.
///
/// A region's local field is seeded from the next region's field (the cells across the border with
/// their integration values), so values chain to the goal and following directions from any cell
/// arrives at a goal with exactly the cost the field reports. The route is restricted to the
/// abstract route's chain of regions, so the cost can be above the full field's (see the book page
/// "Deterministic navigation grid", section "Flow fields", for the measured bound).
///
/// Local fields live in a `BTreeMap` keyed by [`RegionKey`]; past the capacity the least recently
/// used go, down to 7/8 of it. Give a capacity above the regions units stand in plus their routes
/// to the goal: a working set larger than the capacity rebuilds fields every tick. After a grid change, [`ClusterFlow::on_sync`] rebuilds the abstract route, drops the
/// local fields of the dirty clusters and their 8 neighbors, and keeps the rest: each is checked on
/// its next use (same next region and seeds: kept, else rebuilt).
#[derive(Debug, Clone)]
pub struct ClusterFlow {
    profile: Profile,
    goals: Vec<u32>,
    route: RegionRoute,
    locals: BTreeMap<RegionKey, LocalEntry>,
    generation: u64,
    clock: u64,
    capacity: usize,
    stats: ClusterStats,
    shape: (usize, usize),
}

impl ClusterFlow {
    /// The abstract route to `goals` for `profile`; local fields are built on use. `capacity` caps
    /// the cached local fields (at least 1).
    pub fn build(grid: &NavGrid, profile: Profile, goals: &[u32], capacity: usize) -> ClusterFlow {
        let goals = normal_goals(grid, goals);
        let route = RegionRoute::build(grid, profile, &goals);
        ClusterFlow {
            profile,
            goals,
            route,
            locals: BTreeMap::new(),
            generation: 0,
            clock: 0,
            capacity: capacity.max(1),
            stats: ClusterStats {
                route_builds: 1,
                ..ClusterStats::default()
            },
            shape: (grid.width(), grid.height()),
        }
    }

    /// The profile.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The goal cells (sorted, deduplicated).
    pub fn goals(&self) -> &[u32] {
        &self.goals
    }

    /// Counters.
    pub fn stats(&self) -> ClusterStats {
        self.stats
    }

    /// Local fields in the cache.
    pub fn cached_fields(&self) -> usize {
        self.locals.len()
    }

    /// Bytes held: the abstract route (12 per region plus 4 per cluster) and every cached local
    /// field ([`FlowField::memory_bytes`]).
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.route.offsets.capacity() * 4
            + self.route.cost.capacity() * 4
            + self.route.next.capacity() * 4
            + self
                .locals
                .values()
                .map(|e| e.field.memory_bytes() + e.seeds.capacity() * 8 + std::mem::size_of::<LocalEntry>())
                .sum::<usize>()
    }

    /// Regions in the abstract graph, its edges and the regions the last route build expanded.
    pub fn route_size(&self) -> (u32, u32, u32) {
        (
            self.route.cost.len() as u32,
            self.route.edges,
            self.route.expansions,
        )
    }

    /// The region a cell belongs to for this flow's profile, or `None` when the cell is blocked.
    pub fn region_of(&self, grid: &NavGrid, cell: u32) -> Option<RegionKey> {
        if cell as usize >= grid.width() * grid.height() {
            return None;
        }
        let pi = grid.settings().profile_index(self.profile);
        let label = label_at(grid.labels_ref(pi), cell)?;
        Some((grid.cluster_of(cell) as u32, label))
    }

    /// The abstract cost of a region to its nearest goal region, or `None` when unreachable.
    pub fn region_cost(&self, key: RegionKey) -> Option<u32> {
        let node = self.route.node(key)?;
        let v = self.route.cost[node as usize];
        (v != UNREACHED).then_some(v)
    }

    /// The local field of the cell's region, built (with every region down its route that needs
    /// it) on first use. `None` when the cell is blocked or no goal is reachable. `grid` must be the
    /// grid this flow was built and synced with.
    pub fn field_for(&mut self, grid: &NavGrid, cell: u32) -> Option<&FlowField> {
        if (grid.width(), grid.height()) != self.shape {
            return None;
        }
        let key = self.region_of(grid, cell)?;
        self.ensure(grid, key)?;
        self.locals.get(&key).map(|e| &e.field)
    }

    /// Integer cost from the cell to a goal along this flow, or `None` (see [`Self::field_for`]).
    pub fn integration(&mut self, grid: &NavGrid, cell: u32) -> Option<u32> {
        self.field_for(grid, cell)?.integration(cell)
    }

    /// The cell's move along this flow.
    pub fn dir(&mut self, grid: &NavGrid, cell: u32) -> FlowDir {
        self.field_for(grid, cell)
            .map_or(FlowDir::Unreachable, |f| f.dir(cell))
    }

    /// The cell one move along the flow, or `None` at a goal or an unreachable cell.
    pub fn next_cell(&mut self, grid: &NavGrid, cell: u32) -> Option<u32> {
        self.field_for(grid, cell)?.next_cell(cell)
    }

    /// The steering direction at a world point (the nearest cell's move; a clustered flow does not
    /// blend and sets no line-of-sight flags). `None` off the grid, on a blocked cell, or where no
    /// goal is reachable.
    pub fn direction_at(&mut self, grid: &NavGrid, pos: [f32; 2]) -> Option<[f32; 2]> {
        if !grid.in_bounds_world(pos[0], pos[1]) {
            return None;
        }
        let cell = grid.cell_at(pos[0], pos[1]);
        self.field_for(grid, cell)?.direction_at(pos)
    }

    /// The raw bytes of `rect` (cut to the grid), at most `cap` cells, as [`FlowField::view`] gives
    /// them for a full field: per cell the direction byte and the integration value of its region's
    /// local field ([`DIR_NONE`] and [`UNREACHED`] for a blocked or unreachable cell). Builds the
    /// local fields the window needs (and the chains down to the goal); never sets [`FLAG_LOS`].
    pub fn view(&mut self, grid: &NavGrid, rect: CellRect, cap: usize) -> FlowView {
        let (r, truncated) = cap_rect(rect.clipped(grid), cap);
        let mut dirs = Vec::with_capacity(r.area());
        let mut integration = Vec::with_capacity(r.area());
        for z in r.z0 as usize..r.z1 as usize {
            for x in r.x0 as usize..r.x1 as usize {
                let cell = grid.index(x, z);
                let (dir, value) = match self.field_for(grid, cell) {
                    Some(field) => field.local(cell).map_or((DIR_NONE, UNREACHED), |li| {
                        (field.dirs[li], field.integration[li])
                    }),
                    None => (DIR_NONE, UNREACHED),
                };
                dirs.push(dir);
                integration.push(value);
            }
        }
        FlowView {
            rect: r,
            dirs,
            integration,
            truncated,
        }
    }

    /// Draw the arrows of the cached local fields that meet `rect`, at most `cap` in all, into a
    /// debug sink at height `y` (see [`FlowField::debug_emit`]). Fields are visited in region key
    /// order; a region no mover has asked about yet has no field and draws nothing (call
    /// [`Self::field_for`] first to fill a window). Returns the arrows drawn.
    pub fn debug_emit_cached(&self, sink: &mut dyn DebugSink, rect: CellRect, cap: usize, y: f32) -> usize {
        let mut drawn = 0usize;
        for entry in self.locals.values() {
            if drawn >= cap {
                break;
            }
            if intersect(rect, entry.field.window()).area() == 0 {
                continue;
            }
            drawn += entry.field.debug_emit(sink, rect, cap - drawn, y);
        }
        drawn
    }

    /// After `grid.sync_stamps` returned `sync`: rebuild the abstract route, drop the local fields
    /// of the dirty clusters and their 8 neighbors, and mark the rest for a seed check on next use.
    /// A sync that changed nothing is a no-op.
    pub fn on_sync(&mut self, grid: &NavGrid, sync: &StampSync) {
        if !sync.changed {
            return;
        }
        let _span =
            tracing::trace_span!("det_flow_cluster_sync", dirty = sync.dirty_clusters.len()).entered();
        self.generation += 1;
        self.route = RegionRoute::build(grid, self.profile, &self.goals);
        self.stats.route_builds += 1;
        let (cx_n, cz_n) = (grid.clusters_x as i64, grid.clusters_z as i64);
        let mut near = vec![false; (cx_n * cz_n) as usize];
        for c in &sync.dirty_clusters {
            let (cx, cz) = (*c as i64 % cx_n, *c as i64 / cx_n);
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let (x, z) = (cx + dx, cz + dz);
                    if x >= 0 && z >= 0 && x < cx_n && z < cz_n {
                        near[(z * cx_n + x) as usize] = true;
                    }
                }
            }
        }
        let before = self.locals.len();
        self.locals
            .retain(|key, _| !near.get(key.0 as usize).copied().unwrap_or(true));
        self.stats.local_dropped += (before - self.locals.len()) as u32;
    }

    /// Make the local field of `key` current: walk the route to the first region whose field is
    /// current (or a goal region), then refresh back up, goal side first.
    fn ensure(&mut self, grid: &NavGrid, key: RegionKey) -> Option<()> {
        let node = self.route.node(key)?;
        if self.route.cost[node as usize] == UNREACHED {
            return None;
        }
        self.clock += 1;
        if let Some(e) = self.locals.get_mut(&key)
            && e.checked == self.generation
        {
            e.last_used = self.clock;
            self.stats.local_hits += 1;
            return Some(());
        }
        let mut chain = Vec::new();
        let mut at = node;
        loop {
            let k = self.route.key(at);
            if self.locals.get(&k).is_some_and(|e| e.checked == self.generation) {
                break;
            }
            chain.push(k);
            let next = self.route.next[at as usize];
            if next == at || next == u32::MAX || chain.len() > self.route.cost.len() {
                break;
            }
            at = next;
        }
        for k in chain.iter().rev() {
            self.refresh(grid, *k);
        }
        if let Some(e) = self.locals.get_mut(&key) {
            e.last_used = self.clock;
        }
        self.evict(key);
        Some(())
    }

    /// Rebuild or re-check one local field; its next region's field is current.
    fn refresh(&mut self, grid: &NavGrid, key: RegionKey) {
        let Some(node) = self.route.node(key) else {
            return;
        };
        let next_node = self.route.next[node as usize];
        let next = (next_node != node && next_node != u32::MAX).then(|| self.route.key(next_node));
        let rect = CellRect::cluster(grid, key.0 as usize);
        let pi = grid.settings().profile_index(self.profile);
        let mut seeds: Vec<(u32, u32)> = Vec::new();
        if let Some(nk) = next
            && let Some(down) = self.locals.get(&nk)
        {
            let nrect = CellRect::cluster(grid, nk.0 as usize);
            for z in rect.z0 as usize..rect.z1 as usize {
                for x in rect.x0 as usize..rect.x1 as usize {
                    let cell = grid.index(x, z);
                    if label_at(grid.labels_ref(pi), cell) != Some(key.1) {
                        continue;
                    }
                    for (dx, dz) in NEIGHBOURS_8 {
                        let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                        if nx < 0 || nz < 0 || !nrect.contains(nx as usize, nz as usize) {
                            continue;
                        }
                        let ncell = grid.index(nx as usize, nz as usize);
                        if let Some(v) = down.field.integration(ncell) {
                            seeds.push((ncell, v));
                        }
                    }
                }
            }
            seeds.sort_unstable();
            seeds.dedup();
        }
        self.clock += 1;
        let clock = self.clock;
        if let Some(e) = self.locals.get_mut(&key)
            && e.next == next
            && e.seeds == seeds
        {
            e.checked = self.generation;
            e.last_used = clock;
            self.stats.local_revalidated += 1;
            return;
        }
        let goals: Vec<u32> = if next.is_none() {
            self.goals
                .iter()
                .copied()
                .filter(|g| rect.contains(grid.xz(*g).0, grid.xz(*g).1))
                .collect()
        } else {
            Vec::new()
        };
        let field = integrate(
            grid,
            self.profile,
            rect,
            Member::Label(pi, key.1),
            &goals,
            &seeds,
            FlowSettings::default(),
        );
        self.stats.local_builds += 1;
        self.locals.insert(
            key,
            LocalEntry {
                next,
                seeds,
                field,
                checked: self.generation,
                last_used: clock,
            },
        );
    }

    /// Past the capacity, evict the least recently used fields (ties: the lowest key), never `keep`,
    /// down to 7/8 of the capacity in one sorted pass, so a full cache does not pay a scan per insert.
    fn evict(&mut self, keep: RegionKey) {
        if self.locals.len() <= self.capacity {
            return;
        }
        let target = self.capacity - self.capacity / 8;
        let mut order: Vec<(u64, RegionKey)> = self
            .locals
            .iter()
            .filter(|(k, _)| **k != keep)
            .map(|(k, e)| (e.last_used, *k))
            .collect();
        order.sort_unstable();
        let excess = self.locals.len().saturating_sub(target.max(1));
        for (_, k) in order.into_iter().take(excess) {
            self.locals.remove(&k);
            self.stats.local_evicted += 1;
        }
    }
}

// ------------------------------------------------------------------------------------------- cache

/// The key of a cached full field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct FlowKey {
    /// [`goal_set_hash`] of the sorted goal cells.
    pub goal_hash: u64,
    /// The profile.
    pub profile: Profile,
    /// The cache's grid generation when it was built (one step per changed stamp sync).
    pub generation: u64,
}

/// Capacities of a [`FlowCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowCacheSettings {
    /// Full fields kept (least recently used out first). Default 16.
    pub capacity: usize,
    /// Clustered flows kept. Default 16.
    pub cluster_capacity: usize,
    /// Local fields each clustered flow keeps. Default 4096 (about 1.3 MB at the default 64 cells
    /// each; a field holds `cluster_cells^2` cells).
    pub local_capacity: usize,
}

impl Default for FlowCacheSettings {
    fn default() -> Self {
        FlowCacheSettings {
            capacity: 16,
            cluster_capacity: 16,
            local_capacity: 4096,
        }
    }
}

/// Counters of a [`FlowCache`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    /// Lookups answered by a cached field or flow.
    pub hits: u32,
    /// Lookups that built one.
    pub misses: u32,
    /// Entries evicted at a capacity.
    pub evictions: u32,
    /// Full fields dropped by a changed stamp sync.
    pub invalidated: u32,
}

#[derive(Debug, Clone)]
struct FullEntry {
    key: FlowKey,
    goals: Vec<u32>,
    settings: FlowSettings,
    last_used: u64,
    field: FlowField,
}

#[derive(Debug, Clone)]
struct ClusterEntry {
    goal_hash: u64,
    profile: Profile,
    goals: Vec<u32>,
    last_used: u64,
    flow: ClusterFlow,
}

/// Flow fields shared by every mover with the same goal set and profile.
///
/// Full fields are keyed by (goal set hash, profile, grid generation) and rebuilt whole after any
/// grid change; clustered flows are keyed by (goal set hash, profile) and keep their local fields
/// across a change, rebuilding only what it touched. Entries sit in vectors sorted by key; least
/// recently used out past the capacities. Deterministic: no hash-container iteration, no clock.
#[derive(Debug, Clone, Default)]
pub struct FlowCache {
    settings: FlowCacheSettings,
    generation: u64,
    clock: u64,
    full: Vec<FullEntry>,
    clustered: Vec<ClusterEntry>,
    stats: CacheStats,
}

impl FlowCache {
    /// An empty cache.
    pub fn new(settings: FlowCacheSettings) -> FlowCache {
        FlowCache {
            settings,
            ..FlowCache::default()
        }
    }

    /// The capacities.
    pub fn settings(&self) -> FlowCacheSettings {
        self.settings
    }

    /// Steps once per changed stamp sync ([`Self::on_sync`]).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Counters.
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Counters of every cached clustered flow, summed.
    pub fn cluster_stats(&self) -> ClusterStats {
        let mut s = ClusterStats::default();
        for e in &self.clustered {
            let t = e.flow.stats();
            s.route_builds += t.route_builds;
            s.local_builds += t.local_builds;
            s.local_hits += t.local_hits;
            s.local_revalidated += t.local_revalidated;
            s.local_dropped += t.local_dropped;
            s.local_evicted += t.local_evicted;
        }
        s
    }

    /// Full fields cached.
    pub fn len(&self) -> usize {
        self.full.len()
    }

    /// True when no full field is cached.
    pub fn is_empty(&self) -> bool {
        self.full.is_empty()
    }

    /// The keys of the cached full fields, ascending.
    pub fn keys(&self) -> Vec<FlowKey> {
        self.full.iter().map(|e| e.key).collect()
    }

    /// The full field of `goals` for `profile`, from the cache or built now.
    pub fn field(
        &mut self,
        grid: &NavGrid,
        profile: Profile,
        goals: &[u32],
        settings: FlowSettings,
    ) -> &FlowField {
        let goals = normal_goals(grid, goals);
        let key = FlowKey {
            goal_hash: goal_set_hash(&goals),
            profile,
            generation: self.generation,
        };
        self.clock += 1;
        let found = self
            .full
            .iter()
            .position(|e| e.key == key && e.goals == goals && e.settings == settings);
        let at = match found {
            Some(i) => {
                self.stats.hits += 1;
                i
            }
            None => {
                self.stats.misses += 1;
                let field = FlowField::build(grid, profile, &goals, settings);
                let entry = FullEntry {
                    key,
                    goals,
                    settings,
                    last_used: self.clock,
                    field,
                };
                let at = self
                    .full
                    .partition_point(|e| (e.key, &e.goals, e.settings) < (key, &entry.goals, settings));
                self.full.insert(at, entry);
                let cap = self.settings.capacity.max(1);
                let mut at = at;
                while self.full.len() > cap {
                    let victim = self
                        .full
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != at)
                        .min_by_key(|(i, e)| (e.last_used, *i))
                        .map(|(i, _)| i);
                    let Some(v) = victim else { break };
                    self.full.remove(v);
                    self.stats.evictions += 1;
                    if v < at {
                        at -= 1;
                    }
                }
                at
            }
        };
        self.full[at].last_used = self.clock;
        &self.full[at].field
    }

    /// The clustered flow of `goals` for `profile`, from the cache or built now.
    pub fn cluster_flow(&mut self, grid: &NavGrid, profile: Profile, goals: &[u32]) -> &mut ClusterFlow {
        let goals = normal_goals(grid, goals);
        let goal_hash = goal_set_hash(&goals);
        self.clock += 1;
        let found = self
            .clustered
            .iter()
            .position(|e| e.goal_hash == goal_hash && e.profile == profile && e.goals == goals);
        let at = match found {
            Some(i) => {
                self.stats.hits += 1;
                i
            }
            None => {
                self.stats.misses += 1;
                let flow = ClusterFlow::build(grid, profile, &goals, self.settings.local_capacity);
                let at = self
                    .clustered
                    .partition_point(|e| (e.goal_hash, e.profile, &e.goals) < (goal_hash, profile, &goals));
                self.clustered.insert(
                    at,
                    ClusterEntry {
                        goal_hash,
                        profile,
                        goals,
                        last_used: self.clock,
                        flow,
                    },
                );
                let cap = self.settings.cluster_capacity.max(1);
                let mut at = at;
                while self.clustered.len() > cap {
                    let victim = self
                        .clustered
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != at)
                        .min_by_key(|(i, e)| (e.last_used, *i))
                        .map(|(i, _)| i);
                    let Some(v) = victim else { break };
                    self.clustered.remove(v);
                    self.stats.evictions += 1;
                    if v < at {
                        at -= 1;
                    }
                }
                at
            }
        };
        self.clustered[at].last_used = self.clock;
        &mut self.clustered[at].flow
    }

    /// After `grid.sync_stamps` returned `sync`: a changed sync steps the generation, drops every
    /// full field (they are rebuilt on next use) and hands the sync to every clustered flow, which
    /// rebuilds only the touched clusters' fields. An unchanged sync is a no-op.
    pub fn on_sync(&mut self, grid: &NavGrid, sync: &StampSync) {
        if !sync.changed {
            return;
        }
        self.generation += 1;
        self.stats.invalidated += self.full.len() as u32;
        self.full.clear();
        for e in &mut self.clustered {
            e.flow.on_sync(grid, sync);
        }
    }
}

/// FNV-1a 64.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, b: &[u8]) {
        for byte in b {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests;
