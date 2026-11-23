// SPDX-License-Identifier: MPL-2.0
//! Terrain line of sight for fog of war over the det grid's heights (plan ai-nav-18).
//!
//! [`GridLineOfSight`] implements [`regia_detsim::los::LineOfSight`] over a [`NavStatic`]'s ground
//! heights, so hills and ridges hide what lies behind them. It lives with the grid so that
//! `regia_detsim` stays free of `regia_nav`.
//!
//! The test, for a segment from `from` to `to` (world metres):
//!
//! 1. Both ends snap to their nearest grid cell (clamped onto the grid, as
//!    [`NavGrid::cell_at`](super::grid::NavGrid::cell_at) does).
//! 2. Each end's height is the higher of its `y` and the ground of its cell, plus
//!    [`GridLineOfSight::eye_m`] at `from` and [`GridLineOfSight::target_m`] at `to`. (A point
//!    below the ground, such as a fog cell's [`regia_detsim::fog::FOG_CELL_POINT_Y`], is lifted
//!    onto it.)
//! 3. The segment walks every cell it touches between the two cell centers
//!    ([`supercover`], the same walk as the grid's own `los`). At each
//!    cell other than the two ends, the ray height is interpolated at the cell center's projection
//!    onto the segment (`t = dot / len²` from integer cell offsets, clamped to `0..=1`), and the
//!    line is blocked when that cell's ground is higher than the ray.
//!
//! Every decision is an integer offset or an IEEE `+ - * /` in a fixed order, with no
//! transcendental, so the answer is the same on every peer: the hook is safe in a lockstep sim.
//!
//! ```
//! use std::sync::Arc;
//! use regia_detsim::los::LineOfSight;
//! use regia_nav_det::fog_los::GridLineOfSight;
//! use regia_nav_det::grid::{GridSettings, HeightCells, NavStatic};
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! // 64 x 4 cells of 5 m: a 20 m ridge across the middle (columns 31 and 32).
//! let mut heights = vec![0.0; 64 * 4];
//! for z in 0..4 {
//!     heights[z * 64 + 31] = 20.0;
//!     heights[z * 64 + 32] = 20.0;
//! }
//! let ground = HeightCells::centered(64, 4, 5.0, heights);
//! let stat = Arc::new(NavStatic::build(Some(&ground), Arc::new(GridSettings::hover_ground_jet()))?);
//! let los = GridLineOfSight::new(stat).with_eye_m(2.0);
//! // Ground level on both sides: the ridge blocks.
//! assert!(!los.clear([-60.0, 0.0, 0.0], [60.0, 0.0, 0.0]));
//! // From 40 m up, the eye sees over it.
//! assert!(los.clear([-60.0, 40.0, 0.0], [60.0, 0.0, 0.0]));
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use regia_detsim::float::{det_clamp, det_max};
use regia_detsim::los::LineOfSight;

use super::grid::{NavStatic, supercover};

/// Line of sight over a [`NavStatic`]'s ground heights (see the module docs).
#[derive(Debug, Clone)]
pub struct GridLineOfSight {
    stat: Arc<NavStatic>,
    /// Metres added to the `from` end's height (the watcher's eye above its feet). `0.0` by
    /// default; fog's per-class eye heights ([`regia_detsim::fog::SightTable::unit_eye_m`]) add
    /// to it.
    pub eye_m: f32,
    /// Metres added to the `to` end's height (the part of a target that must show, such as half a
    /// unit's height). `0.0` by default.
    pub target_m: f32,
}

impl GridLineOfSight {
    /// Line of sight over `stat`'s heights, with no eye or target height.
    pub fn new(stat: Arc<NavStatic>) -> Self {
        Self {
            stat,
            eye_m: 0.0,
            target_m: 0.0,
        }
    }

    /// The same hook with eye height `eye_m`.
    pub fn with_eye_m(mut self, eye_m: f32) -> Self {
        self.eye_m = eye_m;
        self
    }

    /// The same hook with target height `target_m`.
    pub fn with_target_m(mut self, target_m: f32) -> Self {
        self.target_m = target_m;
        self
    }

    /// The terrain the hook reads.
    pub fn stat(&self) -> &Arc<NavStatic> {
        &self.stat
    }

    /// Nearest cell `(x, z)` of a world point, clamped onto the grid.
    fn cell_of(&self, x: f32, z: f32) -> (i64, i64) {
        let s = &self.stat;
        let fx = (x - s.origin_x) / s.cell_m;
        let fz = (z - s.origin_z) / s.cell_m;
        let cx = (fx + 0.5).floor() as i64;
        let cz = (fz + 0.5).floor() as i64;
        let max_x = s.width as i64 - 1;
        let max_z = s.height as i64 - 1;
        (cx.clamp(0, max_x), cz.clamp(0, max_z))
    }

    fn ground(&self, x: i64, z: i64) -> f32 {
        let i = (z as usize) * self.stat.width + x as usize;
        self.stat.height_cells.get(i).copied().unwrap_or(0.0)
    }

    /// Ground height under a world point (nearest cell, clamped onto the grid).
    pub fn ground_at(&self, x: f32, z: f32) -> f32 {
        if self.stat.width == 0 || self.stat.height == 0 {
            return 0.0;
        }
        let (cx, cz) = self.cell_of(x, z);
        self.ground(cx, cz)
    }
}

impl LineOfSight for GridLineOfSight {
    fn clear(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        if self.stat.width == 0 || self.stat.height == 0 {
            return true;
        }
        let (x0, z0) = self.cell_of(from[0], from[2]);
        let (x1, z1) = self.cell_of(to[0], to[2]);
        let h0 = det_max(from[1], self.ground(x0, z0)) + self.eye_m;
        let h1 = det_max(to[1], self.ground(x1, z1)) + self.target_m;
        let (dx, dz) = (x1 - x0, z1 - z0);
        let len2 = dx * dx + dz * dz;
        if len2 == 0 {
            return true;
        }
        let rise = h1 - h0;
        let len2 = len2 as f32;
        // Cell offsets are bounded by the grid (well under 2^31 cells a side), so i32 holds them.
        supercover(x0 as i32, z0 as i32, x1 as i32, z1 as i32, |_, _, x, z| {
            let (x, z) = (i64::from(x), i64::from(z));
            if (x == x0 && z == z0) || (x == x1 && z == z1) {
                return true;
            }
            let dot = (x - x0) * dx + (z - z0) * dz;
            let t = det_clamp(dot as f32 / len2, 0.0, 1.0);
            let ray = h0 + rise * t;
            let blocked = self.ground(x, z) > ray;
            !blocked
        })
    }
}
