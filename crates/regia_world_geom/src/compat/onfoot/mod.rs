// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_collide.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-930, mark: adapt (Stage A: unchanged); tests 931-1347 (asserts unchanged). `pilot_floor_y` is
// ir-sim/src/state.rs 14246-14305 at the same tag (as-is over the seams).
// Edits: module paths, doc comments (DO27 file, plan, faction and engine names dropped, missing docs added); the
// spec 121 § 4.2 seams: `OnFootParams` for the source's human-scale constants, `OnFootSolids` for its building
// table, scrap, training-stage blockers and rocks (`building_key` takes an opaque `variant`, `door_local` takes the
// host's socket, `scrap_blocker` is the host's); `regia_detsim::math`; float min/max/clamp through
// `regia_detsim::float::det_*` (ADR-8166); `PILOT_STEP_M` is `compat::state`'s; the span-cap report
// (`PilotCell::add_wall` returns the cap hit, `PilotGrid::span_cap_hits`, plan row W-03 acceptance 1);
// `add_wall_capped` / `stamp_wall_capped` and `PilotGrid::wall_span_cap` (plan row W-05, ADR-8432: the
// shipped cap stays `MAX_WALL_SPANS`; a test may set 1). A dead `let _ = ground;` dropped; let-chains
// for nested `if let` (clippy); one integer `max` on its own line (det-scan float hint).

//! On-foot collision for a walker capsule (spec 121 § 4.1 row `compat::onfoot`).
//!
//! The host's integrator moves the body; this module is what stops it: a wall ring on each building
//! without a mesh grid, axis boxes for loose pieces and steps, and a sparse grid of [`CELL_M`] cells
//! holding set-piece and interior walls (up to [`MAX_WALL_SPANS`] disjoint vertical spans each) and
//! walkable tops. [`mesh`] builds the grid from authored glTF at load; on a map with mesh layers it
//! drops a drawn floor within [`FLOOR_MATCH_M`] of a sim floor (T3).
//!
//! Game data stays the host's: the [`OnFootSolids`] trait hands the query the building grids (by key,
//! with centre and yaw), the loose and fixed boxes and the rock props, and [`OnFootParams`] carries the
//! capsule and door sizes.

pub mod mesh;

use std::collections::BTreeMap;

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;

use crate::compat::sampler::TerrainSampler;
pub use crate::compat::state::PILOT_STEP_M;
use crate::compat::state::floor_with_skylight_prop;

/// The host's human-scale sizes the on-foot rules read (spec 121 § 4.2 seam for the source's
/// human-scale constants). There is no default: the host passes its values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OnFootParams {
    /// Walker capsule radius, metres.
    pub capsule_radius_m: f32,
    /// Walker height (feet to the top of the head), metres: a wall span wholly above this does not block.
    pub height_m: f32,
    /// Personnel door width, metres (the wall ring's door notch).
    pub door_width_m: f32,
    /// A building's collision rim radius, metres: [`door_local`]'s fallback notch sits on it.
    pub building_radius_m: f32,
}

/// Extra gap so the capsule does not sit inside a face.
pub const PILOT_SKIN_M: f32 = 0.04;
/// Drop the feet still stick to, so a ramp and a curb do not leave the body in the air.
pub const PILOT_STEP_DOWN_M: f32 = 0.55;
/// Thickness of the personnel wall ring when a building has no mesh grid.
pub const BUILDING_WALL_M: f32 = 1.2;
/// Set-piece and interior grid (metres). Coarser than the capsule, fine enough
/// that one tick of a run (about 0.13 m) cannot skip a cell.
pub const CELL_M: f32 = 0.75;
/// A floor this close to the heightfield is the heightfield. Stamping it again
/// fights the ground sample.
pub const FLOOR_MATCH_M: f32 = 0.30;

/// One static box. `force_block` never becomes a step (a large loose plate is
/// short, and it still stops a walker).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blocker {
    /// Minimum corner (world metres).
    pub min: [f32; 3],
    /// Maximum corner (world metres).
    pub max: [f32; 3],
    /// Blocks from the feet up, never a step.
    pub force_block: bool,
}

/// The most disjoint wall spans one cell keeps. A cell that kept one span, min(lo)..max(hi) of every
/// steep face that touched it, fused a lining shell's hidden floor-slab end face (under the floor) and
/// a roof facet 13 m overhead into one floor-to-roof column: a wall at body height that nothing drew.
/// Past the cap the two closest spans merge (smallest gap first): a ribbed roof's many facet spans
/// fold into each other long before the 10 m gap between a floor-level face and the roof closes. The
/// cap is a watched limit: [`PilotGrid::span_cap_hits`] counts the merges.
pub const MAX_WALL_SPANS: usize = 4;
/// Two wall spans that overlap or leave a gap this small are one span.
pub const SPAN_JOIN_M: f32 = 0.10;

/// One vertical wall span in a cell, `lo <= hi` (world metres, or building-local for a building grid).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WallSpan {
    /// Bottom, metres.
    pub lo: f32,
    /// Top, metres.
    pub hi: f32,
}

/// One grid cell: an optional walkable top and up to [`MAX_WALL_SPANS`] disjoint wall spans, sorted
/// by height with gaps over [`SPAN_JOIN_M`]. Without overflow the spans are the union of every
/// stamped span (closed under [`SPAN_JOIN_M`]), so the stamp order does not change them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PilotCell {
    /// Walkable top, metres (valid when `has_floor`).
    pub floor_y: f32,
    /// The cell has a walkable top.
    pub has_floor: bool,
    span_count: u8,
    spans: [WallSpan; MAX_WALL_SPANS],
}

impl Default for PilotCell {
    fn default() -> Self {
        Self {
            floor_y: 0.0,
            has_floor: false,
            span_count: 0,
            spans: [WallSpan::default(); MAX_WALL_SPANS],
        }
    }
}

impl PilotCell {
    /// The cell has at least one wall span.
    #[inline]
    pub fn has_wall(&self) -> bool {
        self.span_count > 0
    }

    /// The wall spans, lowest first.
    #[inline]
    pub fn spans(&self) -> &[WallSpan] {
        &self.spans[..self.span_count as usize]
    }

    /// Lowest bottom and highest top over every span (what the single-span cell kept).
    pub fn wall_envelope(&self) -> Option<(f32, f32)> {
        let spans = self.spans();
        Some((spans.first()?.lo, spans.last()?.hi))
    }

    /// Add the wall span `lo..=hi` under the shipped cap [`MAX_WALL_SPANS`].
    ///
    /// Same union as [`Self::add_wall_capped`] with that cap. Returns true when a cap merge ran.
    pub fn add_wall(&mut self, lo: f32, hi: f32) -> bool {
        self.add_wall_capped(lo, hi, MAX_WALL_SPANS)
    }

    /// Add the wall span `lo..=hi`, keeping at most `cap` disjoint spans.
    ///
    /// The span absorbs every span it overlaps or touches within [`SPAN_JOIN_M`]. Only comparisons
    /// and one add per span, no accumulation; the list stays sorted. `cap` is clamped to
    /// `1..=MAX_WALL_SPANS` (an integer bound, not a float clamp). Past the cap, the adjacent pair
    /// with the smallest gap merges (lowest index first on a tie) until the list fits, which can
    /// only add wall, never open one. With `cap == MAX_WALL_SPANS` the list grows by at most one
    /// span, so the merge runs at most once and matches [`Self::add_wall`] before this split.
    /// Returns true when a cap merge ran.
    ///
    /// The shipped stamp path passes [`MAX_WALL_SPANS`]. A test may pass 1 to force one fused span
    /// per cell (plan row W-05). This is not behind `stage_b`.
    pub fn add_wall_capped(&mut self, lo: f32, hi: f32, cap: usize) -> bool {
        if !(lo.is_finite() && hi.is_finite()) || hi < lo {
            return false;
        }
        let cap = span_cap_limit(cap);
        let (mut lo, mut hi) = (lo, hi);
        let mut out = [WallSpan::default(); MAX_WALL_SPANS + 1];
        let mut n = 0usize;
        let mut placed = false;
        for &s in self.spans() {
            if s.hi + SPAN_JOIN_M < lo {
                out[n] = s;
                n += 1;
            } else if hi + SPAN_JOIN_M < s.lo {
                if !placed {
                    out[n] = WallSpan { lo, hi };
                    n += 1;
                    placed = true;
                }
                out[n] = s;
                n += 1;
            } else {
                lo = det_min(lo, s.lo);
                hi = det_max(hi, s.hi);
            }
        }
        if !placed {
            out[n] = WallSpan { lo, hi };
            n += 1;
        }
        let mut capped = false;
        while n > cap {
            let mut at = 0usize;
            let mut best = f32::INFINITY;
            for i in 0..n - 1 {
                let gap = out[i + 1].lo - out[i].hi;
                if gap < best {
                    best = gap;
                    at = i;
                }
            }
            out[at].hi = det_max(out[at].hi, out[at + 1].hi);
            for i in at + 1..n - 1 {
                out[i] = out[i + 1];
            }
            n -= 1;
            capped = true;
        }
        self.spans[..n].copy_from_slice(&out[..n]);
        for slot in &mut self.spans[n..] {
            *slot = WallSpan::default();
        }
        self.span_count = n as u8;
        capped
    }

    /// Drop every span that overlaps the open band `y0..y1` (a carved door mouth). Spans wholly above
    /// or below it stay: a lintel over the door is over the head, a footing under it is under the feet.
    pub fn clear_walls_overlapping(&mut self, y0: f32, y1: f32) {
        let mut n = 0usize;
        for i in 0..self.span_count as usize {
            let s = self.spans[i];
            if s.lo < y1 && s.hi > y0 {
                continue;
            }
            self.spans[n] = s;
            n += 1;
        }
        for slot in &mut self.spans[n..] {
            *slot = WallSpan::default();
        }
        self.span_count = n as u8;
    }
}

/// One carved door mouth, in the grid's space. `leaf` is the drawn opening
/// span on the wide axis (the union when panels merge), `thin` the
/// through-depth. Nested panels that overlap both ranges merge into one cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DoorCut {
    /// True when the leaf runs along x (thin axis is z).
    pub x_wide: bool,
    /// Opening span on the wide axis.
    pub leaf: [f32; 2],
    /// Through-depth span on the thin axis.
    pub thin: [f32; 2],
    /// Height band the cut clears.
    pub y: [f32; 2],
}

/// Sparse XZ grid. `BTreeMap` so two hosts push in the same cell order.
#[derive(Clone, Debug, PartialEq)]
pub struct PilotGrid {
    /// Cells by `(cell_index(x), cell_index(z))`.
    pub cells: BTreeMap<(i32, i32), PilotCell>,
    /// Every carved door mouth; kept so offset panels merge into one cut.
    pub door_cuts: Vec<DoorCut>,
    /// How many wall stamps hit the active span cap in a cell and merged spans (a watched limit:
    /// the walls stay closed, but a high count means a lining is ribbed finer than the cap).
    /// Diagnostic only; no query reads it.
    pub span_cap_hits: u32,
    /// How many disjoint wall spans [`Self::stamp_wall`] keeps in one cell.
    ///
    /// [`Default`] is [`MAX_WALL_SPANS`] (4). A test may set this to 1 before stamping so a floor-slab
    /// end under the floor and a roof facet fuse into one column (plan row W-05). Queries do not read
    /// it. The field is not behind `stage_b`, and shipping code leaves the default.
    pub wall_span_cap: usize,
}

impl Default for PilotGrid {
    fn default() -> Self {
        Self {
            cells: BTreeMap::new(),
            door_cuts: Vec::new(),
            span_cap_hits: 0,
            wall_span_cap: MAX_WALL_SPANS,
        }
    }
}

/// Integer bound for a test span cap: `1..=MAX_WALL_SPANS`. [`usize::clamp`] is not the float clamp.
fn span_cap_limit(cap: usize) -> usize {
    cap.clamp(1, MAX_WALL_SPANS)
}

/// World grid plus one local grid per building art key ([`building_key`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PilotSolids {
    /// The set-piece grid, world space.
    pub world: PilotGrid,
    /// Building grids by art key, building-local space.
    pub buildings: BTreeMap<String, PilotGrid>,
}

impl PilotSolids {
    /// Floor a walker stands on from the static set-piece grid alone: the walkable top within one step
    /// up or [`PILOT_STEP_DOWN_M`] down of `feet_y`, else `base`. [`pilot_floor_y`] starts from this,
    /// and a presentation foot planter can call it too, so both read one floor.
    pub fn world_floor_y(&self, params: &OnFootParams, x: f32, z: f32, feet_y: f32, base: f32) -> f32 {
        let mut support = base;
        if let Some(top) = self.world.support(x, z, feet_y, params.capsule_radius_m) {
            accept_support(&mut support, top, feet_y);
        }
        support
    }
}

/// The host's on-foot solids (spec 121 § 4.2 seam): the static grids plus the live game state the
/// floor query reads. The host keeps its building table, loose pieces, stage props and rock scatter;
/// the crate never names them.
pub trait OnFootSolids {
    /// The static grids (world and building art).
    fn pilot_solids(&self) -> &PilotSolids;
    /// Every standing building, in the host's fixed entity order: its art key (a key of
    /// [`PilotSolids::buildings`], from [`building_key`]), centre (world metres) and yaw (radians).
    fn for_each_building(&self, f: &mut dyn FnMut(&str, [f32; 3], f32));
    /// Every loose piece box, in the host's fixed order. A `force_block` box is never a floor.
    fn for_each_loose_blocker(&self, f: &mut dyn FnMut(Blocker));
    /// Every fixed stage box, in the host's fixed order. Each one is a floor candidate.
    fn for_each_fixed_blocker(&self, f: &mut dyn FnMut(Blocker));
    /// Push `(x, z)` out of the host's rock props for a capsule of `radius` at `feet_y` (the host's
    /// settle pass after [`slide_move`]; identity on a map without rocks).
    fn rock_push_out(&self, x: f32, z: f32, feet_y: f32, radius: f32) -> [f32; 2];
}

/// Raise `support` to `top` when `top` is reachable from `feet_y` (a step up, or a short drop).
pub fn accept_support(support: &mut f32, top: f32, feet_y: f32) {
    if top <= feet_y + PILOT_STEP_M + 0.02 && top >= feet_y - PILOT_STEP_DOWN_M && top > *support {
        *support = top;
    }
}

/// Art key for a building mesh: the host's stem, with `@alt` when `variant` is set (a host with two
/// art sets per building stem keys the second one this way).
pub fn building_key(stem: &str, variant: bool) -> String {
    if variant {
        format!("{stem}@alt")
    } else {
        stem.to_string()
    }
}

/// The grid cell index of a world coordinate.
#[inline]
pub fn cell_index(v: f32) -> i32 {
    (v / CELL_M).floor() as i32
}

impl PilotGrid {
    /// Raise the cell at `(x, z)` to the walkable top `y`.
    pub fn stamp_floor(&mut self, x: f32, z: f32, y: f32) {
        let cell = self.cells.entry((cell_index(x), cell_index(z))).or_default();
        if !cell.has_floor || y > cell.floor_y {
            cell.floor_y = y;
            cell.has_floor = true;
        }
    }

    /// Add the wall span `lo..=hi` to the cell at `(x, z)`, under [`Self::wall_span_cap`].
    pub fn stamp_wall(&mut self, x: f32, z: f32, lo: f32, hi: f32) {
        self.stamp_wall_capped(x, z, lo, hi, self.wall_span_cap);
    }

    /// [`Self::stamp_wall`] with an explicit span cap (clamped to `1..=MAX_WALL_SPANS`).
    ///
    /// The grid's [`Self::wall_span_cap`] is unchanged. [`Self::span_cap_hits`] counts a merge.
    /// Shipping stamps use [`Self::stamp_wall`] and the default cap.
    pub fn stamp_wall_capped(&mut self, x: f32, z: f32, lo: f32, hi: f32, cap: usize) {
        if !(lo.is_finite() && hi.is_finite()) || hi < lo {
            return;
        }
        if self
            .cells
            .entry((cell_index(x), cell_index(z)))
            .or_default()
            .add_wall_capped(lo, hi, cap)
        {
            self.span_cap_hits += 1;
        }
    }

    /// Flat rectangle at `y`. Tests and raised decks.
    pub fn stamp_floor_rect(&mut self, min_x: f32, max_x: f32, min_z: f32, max_z: f32, y: f32) {
        let (min_x, max_x) = (det_min(min_x, max_x), det_max(min_x, max_x));
        let (min_z, max_z) = (det_min(min_z, max_z), det_max(min_z, max_z));
        let mut x = min_x;
        while x <= max_x {
            let mut z = min_z;
            while z <= max_z {
                self.stamp_floor(x, z, y);
                z += CELL_M * 0.5;
            }
            x += CELL_M * 0.5;
        }
    }

    /// Vertical box. Cells in the XZ footprint become walls from `min[1]` to `max[1]`.
    pub fn stamp_wall_box(&mut self, min: [f32; 3], max: [f32; 3]) {
        let mut x = min[0];
        while x <= max[0] {
            let mut z = min[2];
            while z <= max[2] {
                self.stamp_wall(x, z, min[1], max[1]);
                z += CELL_M * 0.5;
            }
            x += CELL_M * 0.5;
        }
    }

    /// One triangle. Upward faces are floors. Steep faces are walls. `sim_floor` answers the sim
    /// floor nearest a height at an XZ (T3: the heightfield or a mesh-layer floor); a drawn floor
    /// that already matches it within [`FLOOR_MATCH_M`] is dropped, so a walker stands on the one
    /// surface there.
    pub fn stamp_triangle(
        &mut self,
        a: [f32; 3],
        b: [f32; 3],
        c: [f32; 3],
        sim_floor: Option<&dyn Fn(f32, f32, f32) -> f32>,
    ) {
        let ux = b[0] - a[0];
        let uy = b[1] - a[1];
        let uz = b[2] - a[2];
        let vx = c[0] - a[0];
        let vy = c[1] - a[1];
        let vz = c[2] - a[2];
        let nx = uy * vz - uz * vy;
        let ny = uz * vx - ux * vz;
        let nz = ux * vy - uy * vx;
        let len = math::sqrt(nx * nx + ny * ny + nz * nz);
        if len < 1e-8 {
            return;
        }
        let ny = ny / len;
        let lo = det_min(det_min(a[1], b[1]), c[1]);
        let hi = det_max(det_max(a[1], b[1]), c[1]);
        if ny >= 0.55 {
            let cx = (a[0] + b[0] + c[0]) / 3.0;
            let cz = (a[2] + b[2] + c[2]) / 3.0;
            let cy = (a[1] + b[1] + c[1]) / 3.0;
            if let Some(sample) = sim_floor
                && (cy - sample(cx, cz, cy)).abs() <= FLOOR_MATCH_M
            {
                return;
            }
            self.stamp_floor_triangle(a, b, c);
            return;
        }
        if ny.abs() < 0.45 && hi - lo >= 0.05 {
            self.stamp_wall_triangle(a, b, c, lo, hi);
        }
    }

    fn stamp_floor_triangle(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3]) {
        let min_x = det_min(det_min(a[0], b[0]), c[0]);
        let max_x = det_max(det_max(a[0], b[0]), c[0]);
        let min_z = det_min(det_min(a[2], b[2]), c[2]);
        let max_z = det_max(det_max(a[2], b[2]), c[2]);
        // A canyon-sized triangle is the heightfield. Decks and ramps the
        // walker stands on are smaller than this. Stamping the whole map
        // into the grid would fight the ground sample.
        if max_x - min_x > 96.0 || max_z - min_z > 96.0 {
            return;
        }
        let mut x = min_x;
        while x <= max_x + CELL_M * 0.25 {
            let mut z = min_z;
            while z <= max_z + CELL_M * 0.25 {
                if let Some(y) = bary_y(x, z, a, b, c) {
                    self.stamp_floor(x, z, y);
                }
                z += CELL_M * 0.5;
            }
            x += CELL_M * 0.5;
        }
    }

    fn stamp_wall_triangle(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3], lo: f32, hi: f32) {
        let pts = [a, b, c];
        for i in 0..3 {
            let p = pts[i];
            let q = pts[(i + 1) % 3];
            let dx = q[0] - p[0];
            let dz = q[2] - p[2];
            let len = det_max(math::sqrt(dx * dx + dz * dz), CELL_M * 0.5);
            let steps = det_max((len / (CELL_M * 0.5)).ceil(), 1.0);
            let n = steps as i32;
            for s in 0..=n {
                let t = s as f32 / n as f32;
                self.stamp_wall(p[0] + dx * t, p[2] + dz * t, lo, hi);
            }
        }
    }

    /// The first drawn wall on the segment `from -> to`, as a parameter in `0..=1`, sampling every
    /// `step` metres. A cell blocks a point when the point's height is inside one of the cell's wall
    /// spans (plus `pad`). A presentation camera walks its boom through this so the eye stops at the
    /// drawn tunnel lining and portal rock, which the sim's collision tube does not follow at the
    /// junctions. `None` when the segment is clear or the grid is empty.
    pub fn wall_hit_along(&self, from: [f32; 3], to: [f32; 3], step: f32, pad: f32) -> Option<f32> {
        if self.cells.is_empty() {
            return None;
        }
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let len = math::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
        if len < 1e-4 {
            return None;
        }
        let n = det_max((len / det_max(step, 0.05)).ceil(), 1.0) as u32;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let p = [from[0] + d[0] * t, from[1] + d[1] * t, from[2] + d[2] * t];
            // The cell and its eight neighbours: a big lining shell is stamped with a triangle stride
            // (`mesh::STAMP_CAP`), which leaves one-cell gaps a thin ray would slip through.
            let (cx, cz) = (cell_index(p[0]), cell_index(p[2]));
            for iz in cz - 1..=cz + 1 {
                for ix in cx - 1..=cx + 1 {
                    if let Some(cell) = self.cells.get(&(ix, iz))
                        && cell
                            .spans()
                            .iter()
                            .any(|w| p[1] >= w.lo - pad && p[1] <= w.hi + pad)
                    {
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    /// Horizontal slide. Returns the corrected XZ.
    pub fn slide(&self, params: &OnFootParams, x: f32, z: f32, feet_y: f32, radius: f32) -> [f32; 2] {
        let mut x = x;
        let mut z = z;
        let reach = radius + PILOT_SKIN_M;
        for _ in 0..4 {
            let hits = self.blocking(params, x, z, reach, feet_y);
            if hits.is_empty() {
                break;
            }
            let mut moved = false;
            for (ix, iz) in hits {
                let min_x = ix as f32 * CELL_M;
                let max_x = min_x + CELL_M;
                let min_z = iz as f32 * CELL_M;
                let max_z = min_z + CELL_M;
                let (nx, nz, hit) = push_circle_out(x, z, min_x, max_x, min_z, max_z, reach);
                if hit {
                    x = nx;
                    z = nz;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        [x, z]
    }

    fn blocking(&self, params: &OnFootParams, x: f32, z: f32, radius: f32, feet_y: f32) -> Vec<(i32, i32)> {
        let min_i = cell_index(x - radius);
        let max_i = cell_index(x + radius);
        let min_k = cell_index(z - radius);
        let max_k = cell_index(z + radius);
        let mut out = Vec::new();
        for iz in min_k..=max_k {
            for ix in min_i..=max_i {
                let Some(cell) = self.cells.get(&(ix, iz)) else {
                    continue;
                };
                if wall_blocks(params, cell, feet_y) {
                    out.push((ix, iz));
                }
            }
        }
        out
    }

    /// Open a door leaf. `min`/`max` is the slab. The thin axis is deepened
    /// by the capsule so a 0.75 m cell does not stay shut across a 1 m door;
    /// the wide axis is the leaf span exactly, so no sliver of a jamb cell
    /// stays inside the opening (an inset once left a wall cell sealed
    /// across the mouth). A nested panel that overlaps this mouth merges
    /// into it, so two offset leaves cannot slit the corridor narrower than
    /// the capsule.
    pub fn clear_door_mouth(&mut self, params: &OnFootParams, min: [f32; 3], max: [f32; 3]) {
        let dx = max[0] - min[0];
        let dz = max[2] - min[2];
        // A leaf narrower than the capsule cannot be walked through; leave it
        // solid rather than cut a mouth the body only clips into.
        let wide_margin = params.capsule_radius_m + PILOT_SKIN_M;
        if det_max(dx, dz) < 2.0 * wide_margin + 0.10 {
            return;
        }
        // The moving leaf is one panel. The static shell keeps a second
        // panel about a meter further in. Cut through that thickness, and
        // stop short of the next interior wall.
        let depth = params.capsule_radius_m + PILOT_SKIN_M + 0.76;
        let mut cut = DoorCut {
            x_wide: dx > dz,
            leaf: if dx > dz {
                [min[0], max[0]]
            } else {
                [min[2], max[2]]
            },
            thin: if dx > dz {
                [min[2] - depth, max[2] + depth]
            } else {
                [min[0] - depth, max[0] + depth]
            },
            y: [min[1] - 0.25, max[1] + 0.25],
        };
        // Merge with any mouth this one overlaps (a leaf plus its static
        // inner panel). Two mouths on the same wall but apart stay separate.
        let mut kept = Vec::with_capacity(self.door_cuts.len());
        for other in std::mem::take(&mut self.door_cuts) {
            let leaf_overlap = cut.leaf[0] < other.leaf[1] + 0.10 && other.leaf[0] < cut.leaf[1] + 0.10;
            let thin_overlap = cut.thin[0] < other.thin[1] && other.thin[0] < cut.thin[1];
            if other.x_wide == cut.x_wide && leaf_overlap && thin_overlap {
                cut.leaf[0] = det_min(cut.leaf[0], other.leaf[0]);
                cut.leaf[1] = det_max(cut.leaf[1], other.leaf[1]);
                cut.thin[0] = det_min(cut.thin[0], other.thin[0]);
                cut.thin[1] = det_max(cut.thin[1], other.thin[1]);
                cut.y[0] = det_min(cut.y[0], other.y[0]);
                cut.y[1] = det_max(cut.y[1], other.y[1]);
            } else {
                kept.push(other);
            }
        }
        kept.push(cut);
        self.door_cuts = kept;
        let (min_x, max_x, min_z, max_z) = if cut.x_wide {
            (cut.leaf[0], cut.leaf[1], cut.thin[0], cut.thin[1])
        } else {
            (cut.thin[0], cut.thin[1], cut.leaf[0], cut.leaf[1])
        };
        let ix0 = cell_index(min_x);
        let ix1 = cell_index(max_x);
        let iz0 = cell_index(min_z);
        let iz1 = cell_index(max_z);
        for iz in iz0..=iz1 {
            for ix in ix0..=ix1 {
                let cmin_x = ix as f32 * CELL_M;
                let cmin_z = iz as f32 * CELL_M;
                let overlap_x = det_min(cmin_x + CELL_M, max_x) - det_max(cmin_x, min_x);
                let overlap_z = det_min(cmin_z + CELL_M, max_z) - det_max(cmin_z, min_z);
                if overlap_x < 0.05 || overlap_z < 0.05 {
                    continue;
                }
                let Some(cell) = self.cells.get_mut(&(ix, iz)) else {
                    continue;
                };
                cell.clear_walls_overlapping(cut.y[0], cut.y[1]);
            }
        }
    }

    /// Walkable top under a capsule at `prev_y`, if one is in reach.
    pub fn support(&self, x: f32, z: f32, prev_y: f32, radius: f32) -> Option<f32> {
        let min_i = cell_index(x - radius);
        let max_i = cell_index(x + radius);
        let min_k = cell_index(z - radius);
        let max_k = cell_index(z + radius);
        let mut best: Option<f32> = None;
        for iz in min_k..=max_k {
            for ix in min_i..=max_i {
                let Some(cell) = self.cells.get(&(ix, iz)) else {
                    continue;
                };
                if cell.has_floor {
                    consider_support(&mut best, cell.floor_y, prev_y);
                }
                for span in cell.spans() {
                    if span.hi - span.lo <= PILOT_STEP_M + 0.05 {
                        consider_support(&mut best, span.hi, prev_y);
                    }
                }
            }
        }
        best
    }
}

/// A wall cell this thin from lo to hi is a lip (a sill, a curb edge, a plate rim), not a wall.
pub const LIP_THICK_M: f32 = 0.3;
/// A lip no higher than one step plus this above the feet is stepped over.
pub const LIP_REACH_M: f32 = 0.35;

/// A cell blocks when any one of its spans stands in the body band. Spans above the head or under
/// the step never join into a wall between them.
fn wall_blocks(params: &OnFootParams, cell: &PilotCell, feet_y: f32) -> bool {
    cell.spans().iter().any(|span| span_blocks(params, *span, feet_y))
}

fn span_blocks(params: &OnFootParams, span: WallSpan, feet_y: f32) -> bool {
    if span.hi <= feet_y + PILOT_STEP_M {
        return false;
    }
    // A 0.17 m sliver of a set-piece mesh 0.57 m over a tunnel ramp held a walker 2 cm over the step
    // for good. A lip that low is walked over.
    if span.hi - span.lo < LIP_THICK_M && span.hi <= feet_y + PILOT_STEP_M + LIP_REACH_M {
        return false;
    }
    if span.lo >= feet_y + params.height_m {
        return false;
    }
    if span.hi <= feet_y {
        return false;
    }
    true
}

fn consider_support(best: &mut Option<f32>, top: f32, prev_y: f32) {
    if top <= prev_y + PILOT_STEP_M + 0.02 {
        *best = Some(best.map(|had| det_max(had, top)).unwrap_or(top));
    }
}

/// Push `(x, z)` out of an AABB when the circle of `radius` overlaps it.
fn push_circle_out(
    x: f32,
    z: f32,
    min_x: f32,
    max_x: f32,
    min_z: f32,
    max_z: f32,
    radius: f32,
) -> (f32, f32, bool) {
    let cx = det_clamp(x, min_x, max_x);
    let cz = det_clamp(z, min_z, max_z);
    let dx = x - cx;
    let dz = z - cz;
    let d2 = dx * dx + dz * dz;
    if d2 >= radius * radius {
        return (x, z, false);
    }
    if d2 > 1e-8 {
        let d = math::sqrt(d2);
        let push = radius - d;
        return (x + dx / d * push, z + dz / d * push, true);
    }
    let left = x - min_x;
    let right = max_x - x;
    let down = z - min_z;
    let up = max_z - z;
    let m = det_min(det_min(det_min(left, right), down), up);
    if m == left {
        (min_x - radius, z, true)
    } else if m == right {
        (max_x + radius, z, true)
    } else if m == down {
        (x, min_z - radius, true)
    } else {
        (x, max_z + radius, true)
    }
}

/// Height of the surface at `(x, z)` inside triangle `abc`, if the point is over it.
fn bary_y(x: f32, z: f32, a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Option<f32> {
    let v0x = b[0] - a[0];
    let v0z = b[2] - a[2];
    let v1x = c[0] - a[0];
    let v1z = c[2] - a[2];
    let v2x = x - a[0];
    let v2z = z - a[2];
    let den = v0x * v1z - v1x * v0z;
    if den.abs() < 1e-8 {
        return None;
    }
    let v = (v2x * v1z - v1x * v2z) / den;
    let w = (v0x * v2z - v2x * v0z) / den;
    let u = 1.0 - v - w;
    if u >= -0.02 && v >= -0.02 && w >= -0.02 {
        Some(a[1] * u + b[1] * v + c[1] * w)
    } else {
        None
    }
}

/// Sim yaw: local XZ of a world point. `+Z` local is forward.
pub fn world_to_local(x: f32, z: f32, center: [f32; 3], yaw: f32) -> [f32; 2] {
    let dx = x - center[0];
    let dz = z - center[2];
    let s = math::sin(yaw);
    let c = math::cos(yaw);
    [dx * c - dz * s, dx * s + dz * c]
}

/// Inverse of [`world_to_local`].
pub fn local_to_world(local_x: f32, local_z: f32, center: [f32; 3], yaw: f32) -> [f32; 2] {
    let s = math::sin(yaw);
    let c = math::cos(yaw);
    [
        center[0] + local_x * c + local_z * s,
        center[2] - local_x * s + local_z * c,
    ]
}

/// Personnel door in building-local metres: the host's door socket for the art (`socket`), or the
/// fallback notch on `-Z` at the building's rim ([`OnFootParams::building_radius_m`]) when the art
/// has none. The host keeps its socket table (spec 121 § 4.2 seam).
pub fn door_local(socket: Option<[f32; 2]>, params: &OnFootParams) -> [f32; 2] {
    let (x, z) = match socket {
        Some([x, z]) => (x, z),
        None => (0.0, -params.building_radius_m),
    };
    [x, z]
}

/// Push a walker out of a building's wall ring. The opening is the door.
/// A walker already in the interior is left there. Deep outside is left there.
pub fn push_out_of_wall_ring(
    params: &OnFootParams,
    feet_xz: [f32; 2],
    center_xz: [f32; 2],
    yaw: f32,
    door: [f32; 2],
    wall: f32,
    radius: f32,
) -> [f32; 2] {
    let dx = feet_xz[0] - center_xz[0];
    let dz = feet_xz[1] - center_xz[1];
    let s = math::sin(yaw);
    let c = math::cos(yaw);
    let mut local_x = dx * c - dz * s;
    let mut local_z = dx * s + dz * c;
    let dist = math::sqrt(local_x * local_x + local_z * local_z);
    let door_r = det_max(math::sqrt(door[0] * door[0] + door[1] * door[1]), 0.5);
    let outer = door_r + wall * 0.5;
    let inner = det_max(door_r - wall * 0.5, 0.0);
    let blocked_inner = det_max(inner - radius, 0.0);
    let blocked_outer = outer + radius + PILOT_SKIN_M;
    if dist >= blocked_outer || dist <= blocked_inner {
        return feet_xz;
    }
    let door_len = det_max(math::sqrt(door[0] * door[0] + door[1] * door[1]), 0.001);
    let dir_x = door[0] / door_len;
    let dir_z = door[1] / door_len;
    let along = local_x * dir_x + local_z * dir_z;
    let across = (local_x * dir_z - local_z * dir_x).abs();
    let half = params.door_width_m * 0.5 + radius;
    if across <= half && along >= inner - 1.5 && along <= outer + 2.0 {
        return feet_xz;
    }
    let target = if dist - blocked_inner <= blocked_outer - dist {
        blocked_inner
    } else {
        blocked_outer
    };
    if dist < 1e-4 {
        local_x = target;
        local_z = 0.0;
    } else {
        let scale = target / dist;
        local_x *= scale;
        local_z *= scale;
    }
    [
        center_xz[0] + local_x * c + local_z * s,
        center_xz[1] - local_x * s + local_z * c,
    ]
}

fn box_blocks(params: &OnFootParams, box_: &Blocker, feet_y: f32) -> bool {
    if box_.force_block {
        return box_.max[1] > feet_y - 0.05 && box_.min[1] < feet_y + params.height_m;
    }
    box_.max[1] > feet_y + PILOT_STEP_M && box_.min[1] < feet_y + params.height_m
}

fn box_hits(params: &OnFootParams, x: f32, z: f32, feet_y: f32, boxes: &[Blocker], radius: f32) -> bool {
    let grow = radius + PILOT_SKIN_M;
    boxes.iter().any(|box_| {
        box_blocks(params, box_, feet_y)
            && x >= box_.min[0] - grow
            && x <= box_.max[0] + grow
            && z >= box_.min[2] - grow
            && z <= box_.max[2] + grow
    })
}

/// Move from `from` toward `to`, stopping at the first wall or box.
/// A step that would land past a thin piece stays on the near side.
#[expect(
    clippy::too_many_arguments,
    reason = "the source's signature plus the OnFootParams seam (Stage A, ADR-8012 A-1)"
)]
pub fn slide_move(
    params: &OnFootParams,
    grid: &PilotGrid,
    boxes: &[Blocker],
    from_x: f32,
    from_z: f32,
    to_x: f32,
    to_z: f32,
    feet_y: f32,
    radius: f32,
) -> [f32; 2] {
    let blocked = |x: f32, z: f32| {
        !grid
            .blocking(params, x, z, radius + PILOT_SKIN_M, feet_y)
            .is_empty()
            || box_hits(params, x, z, feet_y, boxes, radius)
    };
    let dx = to_x - from_x;
    let dz = to_z - from_z;
    let len = math::sqrt(dx * dx + dz * dz);
    // The end of a long step can sit past a short piece. Sample the segment.
    // (Two lines: the integer `max` on its own line so the det-scan float hint does not read it as `f32::max`.)
    let steps = (len / 0.2).ceil() as i32;
    let steps = steps.max(1);
    let (mut x, mut z) = if len < 1e-5 {
        (to_x, to_z)
    } else {
        let mut blocked_from = None;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            if blocked(from_x + dx * t, from_z + dz * t) {
                blocked_from = Some((step - 1) as f32 / steps as f32);
                break;
            }
        }
        if let Some(prev) = blocked_from {
            let mut lo = prev;
            let mut hi = det_min(prev + 1.0 / steps as f32, 1.0);
            for _ in 0..8 {
                let mid = (lo + hi) * 0.5;
                if blocked(from_x + dx * mid, from_z + dz * mid) {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            (from_x + dx * lo, from_z + dz * lo)
        } else {
            (to_x, to_z)
        }
    };
    // Still overlapping at the start of the move (spawned inside a piece):
    // push out. The sweep above cannot, because every sample is blocked.
    if blocked(x, z) {
        let slid = grid.slide(params, x, z, feet_y, radius);
        x = slid[0];
        z = slid[1];
        let grow = radius + PILOT_SKIN_M;
        for _ in 0..4 {
            let mut moved = false;
            for box_ in boxes {
                if !box_blocks(params, box_, feet_y) {
                    continue;
                }
                let (nx, nz, hit) = push_circle_out(
                    x,
                    z,
                    box_.min[0] - grow,
                    box_.max[0] + grow,
                    box_.min[2] - grow,
                    box_.max[2] + grow,
                    1e-3,
                );
                if hit {
                    x = nx;
                    z = nz;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
    [x, z]
}

fn box_support(boxes: &[Blocker], x: f32, z: f32, prev_y: f32, radius: f32) -> Option<f32> {
    let mut best: Option<f32> = None;
    let grow = radius;
    for box_ in boxes {
        if box_.force_block {
            continue;
        }
        if x < box_.min[0] - grow
            || x > box_.max[0] + grow
            || z < box_.min[2] - grow
            || z > box_.max[2] + grow
        {
            continue;
        }
        consider_support(&mut best, box_.max[1], prev_y);
    }
    best
}

/// Feet after one move. `floor(x, z)` is the heightfield or tunnel floor.
/// `stick` is false while the body is airborne, so a jump is not glued down.
pub fn resolve_feet(
    params: &OnFootParams,
    feet: [f32; 3],
    delta_xz: [f32; 2],
    floor: impl Fn(f32, f32) -> f32,
    grid: &PilotGrid,
    boxes: &[Blocker],
    stick: bool,
) -> [f32; 3] {
    let prev_y = feet[1];
    let radius = params.capsule_radius_m;
    let slid = slide_move(
        params,
        grid,
        boxes,
        feet[0],
        feet[2],
        feet[0] + delta_xz[0],
        feet[2] + delta_xz[1],
        prev_y,
        radius,
    );
    let x = slid[0];
    let z = slid[1];
    let ground = floor(x, z);
    if !stick {
        return [x, prev_y, z];
    }
    let mut support = ground;
    if let Some(top) = grid.support(x, z, prev_y, radius)
        && top >= support - PILOT_STEP_DOWN_M
    {
        support = det_max(support, top);
    }
    if let Some(top) = box_support(boxes, x, z, prev_y, radius) {
        support = det_max(support, top);
    }
    // Do not drop the body through a surface it was standing on or above.
    if support < prev_y - PILOT_STEP_DOWN_M {
        // Still accept the heightfield when it is the thing under them and they have not crossed a
        // higher deck. `support` was already maxed with decks in reach; a big drop means there is no
        // deck, so fall only as far as the step. The host's hover spring owns the rest of a fall.
        support = prev_y;
    }
    if support > prev_y + PILOT_STEP_M {
        support = prev_y;
    }
    // A floor the capsule has entered from above: stand on it.
    if ground > prev_y && ground <= prev_y + PILOT_STEP_M {
        support = det_max(support, ground);
    }
    let y = if (prev_y - support).abs() <= det_max(PILOT_STEP_M, PILOT_STEP_DOWN_M) || support >= prev_y {
        support
    } else {
        prev_y
    };
    [x, y, z]
}

/// Floor a walker should stand on: the heightfield (`base`), or a set-piece deck, the skylight
/// rubble prop ([`floor_with_skylight_prop`]), a building floor, or a loose or fixed box top within
/// one step of `feet_y` (the source's `pilot_floor_y`, over the [`OnFootSolids`] seam).
pub fn pilot_floor_y(
    solids: &dyn OnFootSolids,
    terrain: Option<&TerrainSampler>,
    params: &OnFootParams,
    x: f32,
    z: f32,
    feet_y: f32,
    base: f32,
) -> f32 {
    let radius = params.capsule_radius_m;
    let grids = solids.pilot_solids();
    let mut support = grids.world_floor_y(params, x, z, feet_y, base);
    let accept = |support: &mut f32, top: f32| accept_support(support, top, feet_y);
    // The drawn rubble under a skylight is a prop for a walker (T3: it is not in the surface query).
    support = floor_with_skylight_prop(terrain, x, z, feet_y, support);
    solids.for_each_building(&mut |key, center, yaw| {
        let Some(grid) = grids.buildings.get(key) else {
            return;
        };
        let local = world_to_local(x, z, center, yaw);
        if let Some(top) = grid.support(local[0], local[1], feet_y - center[1], radius) {
            accept(&mut support, top + center[1]);
        }
    });
    solids.for_each_loose_blocker(&mut |box_| {
        if box_.force_block {
            return;
        }
        if x < box_.min[0] - radius
            || x > box_.max[0] + radius
            || z < box_.min[2] - radius
            || z > box_.max[2] + radius
        {
            return;
        }
        accept(&mut support, box_.max[1]);
    });
    solids.for_each_fixed_blocker(&mut |box_| {
        if x < box_.min[0] - radius
            || x > box_.max[0] + radius
            || z < box_.min[2] - radius
            || z > box_.max[2] + radius
        {
            return;
        }
        accept(&mut support, box_.max[1]);
    });
    support
}

#[cfg(test)]
mod tests;
