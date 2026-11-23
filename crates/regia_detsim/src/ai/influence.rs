// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/influence.rs lines 1-21, 55-306 and 308-440, commit
// c9513623f, tag regia-absorb-4), ADR-8012. Mark: adapt (plan row S-10, ADR-8395). The grid, `flank_bearing` and
// `line_of_fire` are DO27's code apart from rustfmt, doc comments and these named changes:
// - line of sight goes through the `LineOfSight` trait (`los: L` where DO27 took `Option<&TerrainSampler>`; the
//   heightfield rule moved to `los.rs`, same bits);
// - std float `min`/`max`/`clamp` are `crate::float::det_*` (ADR-8085: the same bits away from a zero or NaN
//   result), and `sin`/`cos`/`atan2`/`sqrt` are `crate::math` (libm 0.2.16, DO27's bits);
// - DO27's re-export of its cover and salvage helpers (game code) is dropped, and its design-doc and map names
//   are dropped from the comments;
// - test fix: DO27's `#[test] #[test]` above `resize_covering_keeps_stamps_on_the_alpha_square` and the missing
//   `#[test]` on `flank_bearing_returns_a_finite_bearing` (the bearing test never ran) are one misplaced
//   attribute, put back on the bearing test.
// The read accessors `cell_m`, `origin`, `threat_cells` and `control_cells` are new.

//! Influence map and cover / line-of-sight queries.
//!
//! One coarse decision grid ([`INFLUENCE_CELL_M`] cells) with two channels, threat (enemy weapon reach) and
//! control (allied presence), plus deterministic point queries. The grid is time-sliced: one stamp and one
//! cell decay per tick, so a full cycle never costs a spike in any single tick.

use serde::{Deserialize, Serialize};

use super::los::LineOfSight;
use crate::float::{det_clamp, det_max, det_min};

/// Cell size of the decision grid (metres).
pub const INFLUENCE_CELL_M: f32 = 8.0;
/// Falloff radius of a single stamp, in cells.
pub const STAMP_RADIUS_CELLS: usize = 2;
/// Multiplicative decay applied to a cell on each decay visit.
pub const CELL_DECAY: f32 = 0.9;
/// Cells in one decay cycle: a grid this size or smaller decays one cell per [`InfluenceGrid::decay_one`],
/// a larger one enough cells that a full cycle still takes this many calls (160², a 1280 m square at 8 m).
pub const DECAY_CYCLE_CELLS: usize = 160 * 160;

/// True when nothing in `los` blocks the straight line from `from` to `to`: [`LineOfSight::clear`] as a
/// function, for code written against Direct Order 27's `los_clear(terrain, from, to)` (pass the terrain
/// `Option` as it is).
pub fn los_clear<L: LineOfSight>(los: L, from: [f32; 3], to: [f32; 3]) -> bool {
    los.clear(from, to)
}

/// A two-channel (threat / control) decision grid over a bounded map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfluenceGrid {
    cell_m: f32,
    origin: [f32; 2],
    cols: usize,
    rows: usize,
    threat: Vec<f32>,
    control: Vec<f32>,
    /// Pending stamps: (world x, z, strength, is_threat).
    stamps: Vec<([f32; 2], f32, bool)>,
    stamp_cursor: usize,
    decay_cursor: usize,
}

impl InfluenceGrid {
    /// A grid covering `[min, max]` in X and Z, snapping to whole cells (at least one, at least 1 m).
    pub fn new(min: [f32; 2], max: [f32; 2], cell_m: f32) -> Self {
        let cell_m = det_max(cell_m, 1.0);
        let cols = (((max[0] - min[0]) / cell_m).ceil() as usize).max(1);
        let rows = (((max[1] - min[1]) / cell_m).ceil() as usize).max(1);
        let origin = [
            min[0] - ((cols as f32 * cell_m - (max[0] - min[0])) * 0.5),
            min[1] - ((rows as f32 * cell_m - (max[1] - min[1])) * 0.5),
        ];
        InfluenceGrid {
            cell_m,
            origin,
            cols,
            rows,
            threat: vec![0.0; cols * rows],
            control: vec![0.0; cols * rows],
            stamps: Vec::new(),
            stamp_cursor: 0,
            decay_cursor: 0,
        }
    }

    /// Columns (X cells).
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Rows (Z cells).
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The cell edge (metres).
    pub fn cell_m(&self) -> f32 {
        self.cell_m
    }

    /// The world X and Z of the grid's low corner.
    pub fn origin(&self) -> [f32; 2] {
        self.origin
    }

    /// The threat channel, row-major (`z * cols + x`).
    pub fn threat_cells(&self) -> &[f32] {
        &self.threat
    }

    /// The control channel, row-major (`z * cols + x`).
    pub fn control_cells(&self) -> &[f32] {
        &self.control
    }

    /// True when `(x, z)` falls in a cell. The exact far edge
    /// (`origin + cols * cell_m`) is outside, matching [`Self::threat_at`].
    pub fn contains_xz(&self, x: f32, z: f32) -> bool {
        self.cell(x, z).is_some()
    }

    /// Rebuild so the grid covers `[min, max]` when the snapped cells would
    /// differ. An identical coverage is a no-op: stamps and the decay cursor
    /// survive, so a 1280 m map (160² at ±640) does not reset mid-match.
    pub fn resize_covering(&mut self, min: [f32; 2], max: [f32; 2]) {
        let next = Self::new(min, max, self.cell_m);
        if next.cols == self.cols
            && next.rows == self.rows
            && next.origin == self.origin
            && next.cell_m == self.cell_m
        {
            return;
        }
        *self = next;
    }

    /// Cells decayed by one [`Self::decay_one`] call. A grid of up to
    /// [`DECAY_CYCLE_CELLS`] cells decays exactly one cell, so a full cycle
    /// stays one visit per cell per 25600 ticks. A wider grid decays
    /// `n.div_ceil(160²)` cells so the cycle time stays the same.
    pub fn decay_steps(&self) -> usize {
        let n = self.cols.saturating_mul(self.rows);
        if n == 0 || n <= DECAY_CYCLE_CELLS {
            1
        } else {
            n.div_ceil(DECAY_CYCLE_CELLS)
        }
    }

    fn cell(&self, x: f32, z: f32) -> Option<(usize, usize)> {
        let cx = ((x - self.origin[0]) / self.cell_m).floor();
        let cz = ((z - self.origin[1]) / self.cell_m).floor();
        if cx < 0.0 || cz < 0.0 {
            return None;
        }
        let (cx, cz) = (cx as usize, cz as usize);
        if cx >= self.cols || cz >= self.rows {
            return None;
        }
        Some((cx, cz))
    }

    /// Replace the pending stamp list (one enemy threat stamp per known enemy,
    /// one control stamp per allied unit). The pipeline calls this each tick;
    /// [`Self::stamp_one`] drains it one stamp per tick.
    pub fn set_stamps(&mut self, threat: &[([f32; 2], f32)], control: &[([f32; 2], f32)]) {
        self.stamps.clear();
        self.stamps.extend(threat.iter().map(|&(p, s)| (p, s, true)));
        self.stamps.extend(control.iter().map(|&(p, s)| (p, s, false)));
        if self.stamp_cursor >= self.stamps.len() {
            self.stamp_cursor = 0;
        }
    }

    /// Stamp one pending entry (round-robin), falling off over a fixed
    /// neighbourhood. Deterministic neighbour order.
    pub fn stamp_one(&mut self) {
        if self.stamps.is_empty() {
            return;
        }
        let (pos, strength, is_threat) = self.stamps[self.stamp_cursor % self.stamps.len()];
        self.stamp_cursor = (self.stamp_cursor + 1) % self.stamps.len();
        let Some((cx, cz)) = self.cell(pos[0], pos[1]) else {
            return;
        };
        let r = STAMP_RADIUS_CELLS as i32;
        for dz in -r..=r {
            for dx in -r..=r {
                let (nx, nz) = (cx as i32 + dx, cz as i32 + dz);
                if nx < 0 || nz < 0 || nx >= self.cols as i32 || nz >= self.rows as i32 {
                    continue;
                }
                let idx = nz as usize * self.cols + nx as usize;
                let d = (dx * dx + dz * dz) as f32;
                let falloff = 1.0 / (1.0 + d);
                let cell = if is_threat {
                    &mut self.threat[idx]
                } else {
                    &mut self.control[idx]
                };
                *cell = det_min(*cell + strength * falloff, 1000.0);
            }
        }
    }

    /// Decay one cell on a grid of up to [`DECAY_CYCLE_CELLS`] cells, or enough cells on a wider grid
    /// that a full cycle still takes 160² ticks. Integer and deterministic.
    pub fn decay_one(&mut self) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let steps = self.decay_steps();
        for _ in 0..steps {
            self.decay_cell();
        }
    }

    fn decay_cell(&mut self) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let n = self.cols * self.rows;
        let idx = self.decay_cursor % n;
        self.decay_cursor = (self.decay_cursor + 1) % n;
        self.threat[idx] *= CELL_DECAY;
        self.control[idx] *= CELL_DECAY;
    }

    /// The threat channel at `(x, z)`; 0 outside the grid.
    pub fn threat_at(&self, x: f32, z: f32) -> f32 {
        self.cell(x, z)
            .map(|(cx, cz)| self.threat[cz * self.cols + cx])
            .unwrap_or(0.0)
    }

    /// The control channel at `(x, z)`; 0 outside the grid.
    pub fn control_at(&self, x: f32, z: f32) -> f32 {
        self.cell(x, z)
            .map(|(cx, cz)| self.control[cz * self.cols + cx])
            .unwrap_or(0.0)
    }

    /// Signed local advantage: the difference of the two channels sampled at
    /// the unit's cell, normalised into -1..1. Positive = allied control dominates.
    pub fn local_force_ratio(&self, x: f32, z: f32) -> f32 {
        let (t, c) = (self.threat_at(x, z), self.control_at(x, z));
        // Normalise the difference into a -1..1 advantage; +1 = full control.
        let span = t + c;
        if span <= 1.0e-4 {
            0.0
        } else {
            det_clamp((c - t) / span, -1.0, 1.0)
        }
    }
}

/// A bearing (radians, `atan2(dx, dz)`) that has cover and is off `threat`'s facing, sampled
/// from 12 fixed directions around `unit`, 30 m out. Cover means `los` sees no line from the
/// threat to the probe point. Falls back to the direct bearing to the threat.
pub fn flank_bearing<L: LineOfSight>(los: L, unit: [f32; 3], threat: [f32; 3], threat_yaw: f32) -> f32 {
    let dx = threat[0] - unit[0];
    let dz = threat[2] - unit[2];
    let direct = crate::math::atan2(dx, dz);
    const RAYS: usize = 12;
    let mut best: Option<(f32, f32)> = None; // (score, bearing); lower score better
    for i in 0..RAYS {
        let bearing = direct + (i as f32 - RAYS as f32 * 0.5) * (std::f32::consts::TAU / RAYS as f32);
        // A probe point 30 m out on this bearing.
        let probe = [
            unit[0] + crate::math::sin(bearing) * 30.0,
            unit[1],
            unit[2] + crate::math::cos(bearing) * 30.0,
        ];
        // Off the threat's facing: the angle between the bearing and the
        // threat's forward must be large.
        let off_facing = {
            let forward = crate::math::atan2(crate::math::sin(threat_yaw), crate::math::cos(threat_yaw));
            let d = (bearing - forward + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            d.abs()
        };
        // Cover: LOS from the threat to the probe is blocked.
        let covered = !los.clear(threat, probe);
        // Prefer bearings that keep us off the threat's facing and in cover.
        let off_score = (std::f32::consts::FRAC_PI_2 - det_min(off_facing, std::f32::consts::FRAC_PI_2))
            / std::f32::consts::FRAC_PI_2;
        let cover_score = if covered { 0.0 } else { 1.0 };
        let score = cover_score * 0.6 + off_score * 0.4;
        if best.is_none_or(|(b, _)| score < b) {
            best = Some((score, bearing));
        }
    }
    best.map(|(_, bearing)| bearing).unwrap_or(direct)
}

/// True when any `friendlies` body sits inside the shot cone from `origin`
/// toward `target` (the friendly-fire gate). Cone half-angle about 10°
/// (tangent `FRIENDLY_CONE_TAN`), only for bodies nearer than the target, in plan (XZ).
pub fn line_of_fire(origin: [f32; 3], target: [f32; 3], friendlies: &[[f32; 3]]) -> bool {
    let dx = target[0] - origin[0];
    let dz = target[2] - origin[2];
    let range = crate::math::sqrt(dx * dx + dz * dz);
    if range <= 1.0e-3 {
        return false;
    }
    let (ux, uz) = (dx / range, dz / range);
    for friendly in friendlies {
        let fx = friendly[0] - origin[0];
        let fz = friendly[2] - origin[2];
        let along = fx * ux + fz * uz;
        if along <= 0.0 || along >= range {
            continue; // behind us or past the target
        }
        let perp = (fx * -uz + fz * ux).abs();
        if perp <= along * FRIENDLY_CONE_TAN {
            return true;
        }
    }
    false
}

/// Tangent of the friendly-fire cone half-angle (≈10°).
const FRIENDLY_CONE_TAN: f32 = 0.176;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::los::{HeightfieldLos, OpenField};

    /// DO27's flat 64² sampler over 1280 m: zero everywhere.
    fn flat_sampler() -> HeightfieldLos<impl Fn(f32, f32) -> f32> {
        HeightfieldLos(|_: f32, _: f32| 0.0)
    }

    /// DO27's ridge: a 20 m band across the map at world z ≈ 160 m (sample rows 37-43 of 64 over 1280 m,
    /// 20 m apart, so 100..220 m with bilinear shoulders). The same test against DO27's own sampler type runs
    /// in `regia_world_geom`'s `los_adapters` test.
    fn ridge_sampler() -> HeightfieldLos<impl Fn(f32, f32) -> f32> {
        HeightfieldLos(|_: f32, z: f32| if (100.0..=220.0).contains(&z) { 20.0 } else { 0.0 })
    }

    #[test]
    fn los_is_clear_over_flat_ground() {
        let t = flat_sampler();
        assert!(los_clear(Some(&t), [0.0, 0.0, -100.0], [0.0, 0.0, 100.0]));
    }

    #[test]
    fn los_is_blocked_by_a_ridge() {
        let t = ridge_sampler();
        // From the near side to the far side the segment crosses the ridge.
        assert!(!los_clear(Some(&t), [0.0, 0.0, 0.0], [0.0, 0.0, 400.0]));
        // Two points on the near side of the ridge stay clear.
        assert!(los_clear(Some(&t), [0.0, 0.0, 0.0], [0.0, 0.0, 40.0]));
    }

    #[test]
    fn no_terrain_is_always_clear() {
        assert!(los_clear(None::<&OpenField>, [0.0, 0.0, 0.0], [0.0, 0.0, 500.0]));
        assert!(los_clear(OpenField, [0.0, 0.0, 0.0], [0.0, 0.0, 500.0]));
    }

    #[test]
    fn grid_stamp_and_decay_are_time_sliced_and_bounded() {
        let mut grid = InfluenceGrid::new([-100.0, -100.0], [100.0, 100.0], INFLUENCE_CELL_M);
        grid.set_stamps(&[([0.0, 0.0], 10.0)], &[([20.0, 0.0], 5.0)]);
        let before = grid.threat_at(0.0, 0.0);
        grid.stamp_one();
        let after = grid.threat_at(0.0, 0.0);
        assert!(after > before, "a threat stamp raises the cell");
        // The control stamp is still pending: not stamped yet.
        assert_eq!(grid.control_at(20.0, 0.0), 0.0);
        grid.stamp_one();
        assert!(grid.control_at(20.0, 0.0) > 0.0);
        // Decay lowers a cell once per visit; a full cycle lowers every cell.
        let pre = grid.threat_at(0.0, 0.0);
        for _ in 0..grid.cols() * grid.rows() {
            grid.decay_one();
        }
        assert!(grid.threat_at(0.0, 0.0) < pre);
    }

    #[test]
    fn local_force_ratio_tracks_control_minus_threat() {
        let mut grid = InfluenceGrid::new([-100.0, -100.0], [100.0, 100.0], INFLUENCE_CELL_M);
        // Threat near the origin, control near (50,0).
        grid.set_stamps(&[([0.0, 0.0], 20.0)], &[([50.0, 0.0], 20.0)]);
        for _ in 0..grid.cols() * grid.rows() {
            grid.stamp_one();
        }
        let near_threat = grid.local_force_ratio(0.0, 0.0);
        let near_control = grid.local_force_ratio(50.0, 0.0);
        assert!(near_threat < 0.0, "threat-dominated cell is negative");
        assert!(near_control > 0.0, "control-dominated cell is positive");
    }

    #[test]
    fn line_of_fire_flags_a_friendly_in_the_cone() {
        let origin = [0.0, 0.0, 0.0];
        let target = [0.0, 0.0, 100.0];
        assert!(!line_of_fire(origin, target, &[]));
        // Dead ahead and close: in the cone.
        assert!(line_of_fire(origin, target, &[[0.0, 0.0, 20.0]]));
        // Well off to the side: out of the cone.
        assert!(!line_of_fire(origin, target, &[[40.0, 0.0, 20.0]]));
        // Behind the shooter: not flagged.
        assert!(!line_of_fire(origin, target, &[[0.0, 0.0, -20.0]]));
    }

    #[test]
    fn resize_covering_keeps_stamps_on_the_alpha_square() {
        let mut grid = InfluenceGrid::new([-640.0, -640.0], [640.0, 640.0], INFLUENCE_CELL_M);
        assert_eq!(grid.cols(), 160);
        assert_eq!(grid.rows(), 160);
        assert_eq!(grid.decay_steps(), 1);
        grid.set_stamps(&[([0.0, 0.0], 10.0)], &[]);
        grid.stamp_one();
        let threat = grid.threat_at(0.0, 0.0);
        assert!(threat > 0.0);
        let cursor = grid.decay_cursor;
        grid.resize_covering([-640.0, -640.0], [640.0, 640.0]);
        assert_eq!(grid.cols(), 160);
        assert_eq!(grid.threat_at(0.0, 0.0), threat);
        assert_eq!(grid.decay_cursor, cursor);
        assert!(!grid.contains_xz(900.0, 0.0));
        grid.decay_one();
        assert_eq!(grid.decay_cursor, cursor + 1);
    }

    #[test]
    fn resize_covering_extends_past_the_alpha_square() {
        let mut grid = InfluenceGrid::new([-640.0, -640.0], [640.0, 640.0], INFLUENCE_CELL_M);
        grid.resize_covering([-1025.0, -1025.0], [1025.0, 1025.0]);
        assert_eq!(grid.cols(), 257);
        assert_eq!(grid.rows(), 257);
        assert!(grid.contains_xz(900.0, 0.0));
        assert_eq!(grid.decay_steps(), 3);
        let small = InfluenceGrid::new([0.0, 0.0], [200.0, 200.0], INFLUENCE_CELL_M);
        assert!(small.cols() * small.rows() < 160 * 160);
        assert_eq!(small.decay_steps(), 1);
        let cursor = grid.decay_cursor;
        grid.decay_one();
        assert_eq!(grid.decay_cursor, cursor + 3);
    }

    #[test]
    fn flank_bearing_returns_a_finite_bearing() {
        let t = flat_sampler();
        let b = flank_bearing(Some(&t), [0.0, 0.0, 0.0], [100.0, 0.0, 0.0], 0.0);
        assert!(b.is_finite());
    }
}
