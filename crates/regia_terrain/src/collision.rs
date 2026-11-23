// SPDX-License-Identifier: MPL-2.0
//! Terrain collision parity: the [`TerrainMap`] laid out the way the physics heightfield
//! collider reads it, a CPU mirror of that collider's surface, and the drift between the live
//! (edited) map and the 16-bit heightfield it saves to.
//!
//! # Layout
//!
//! `regia_physics`' `ColliderShape::HeightField { nx, nz, heights, scale }` hands its heights
//! to Rapier's heightfield as a column-major matrix with `nx` rows: sample `(i, j)` is
//! `heights[i + j * nx]`, where `i` runs along **Z** and `j` along **X**, and the field spans
//! `[-0.5, 0.5] × scale` in X and Z around the collider's origin. A [`TerrainMap`] is row-major
//! with X fastest (`heights[row * width + col]`). Copying the map's heights straight into the
//! collider therefore mirrors the ground about the X = Z diagonal: on any map that is not
//! symmetric the player walks on a different hill than the one drawn.
//! [`TerrainMap::collision_heightfield`] does the transpose (and cuts the holes) once, so
//! [`CollisionHeightfield`]'s fields go into `ColliderShape::HeightField` field for field.
//!
//! # Surface
//!
//! The collider splits every cell along the diagonal from `(x0, z1)` to `(x1, z0)`, the same
//! diagonal as the cooked heightfield mesh and [`crate::cdlod::terrain_surface_height`], so the
//! collider and the finest drawn grid are the same triangles. The simulation's bilinear
//! sampler ([`TerrainMap::sample_bilinear`]) differs from both by at most a quarter of a cell's
//! twist `|h00 - h10 - h01 + h11|` ([`CollisionHeightfield::max_bilinear_delta`]).
//!
//! # Save drift
//!
//! The editor sculpts the `f32` mirror; the game loads the saved 16-bit heightfield
//! ([`TerrainMap::to_heightfield`]), which rounds every height to `height_scale / 65535` and
//! clamps it to `[height_offset, height_offset + height_scale]`. [`TerrainMap::save_drift`]
//! reports how far the live map is from what a reload (and DO27's simulation) will see.

use crate::deform::DirtyRect;
use crate::holes::HolesMask;
use crate::sampler::TerrainMap;

/// A terrain heightfield laid out for the physics collider (module docs, "Layout").
///
/// Field for field what `regia_physics::ColliderShape::HeightField` takes:
///
/// ```ignore
/// let c = terrain.map.collision_heightfield(terrain.holes.as_ref());
/// let shape = ColliderShape::HeightField {
///     nx: c.nx,
///     nz: c.nz,
///     heights: c.heights,
///     scale: Vec3::from(c.scale),
/// };
/// ```
///
/// with the collider at [`Self::center`]: the map's grid origin
/// ([`TerrainMap::grid_origin`], the world origin unless the host moved the grid, ADR-8179).
#[derive(Clone, Debug, PartialEq)]
pub struct CollisionHeightfield {
    /// Rows of the collider's height matrix: samples along **Z** (the map's row count).
    pub nx: usize,
    /// Columns of the collider's height matrix: samples along **X** (the map's column count).
    pub nz: usize,
    /// Heights in metres, `heights[row + col * nx]` (Z fastest). NaN marks a sample in or
    /// next to a hole: the collider removes every cell touching one.
    pub heights: Vec<f32>,
    /// `[world_width, 1, world_depth]` ([`TerrainMap::world_depth`], ADR-8369): the field spans `[-0.5, 0.5] × scale` in X and Z and
    /// heights are metres.
    pub scale: [f32; 3],
    /// Samples cut by holes (set to NaN).
    pub cut_samples: usize,
    /// World position of the collider: the map's grid origin at height 0,
    /// `[origin_x, 0, origin_z]` ([`TerrainMap::grid_origin`], ADR-8179). `[0, 0, 0]` for a
    /// map that keeps the default origin.
    pub center: [f32; 3],
}

/// One square piece of a [`CollisionHeightfield`] (ADR-1595): a heightfield collider of its
/// own, so a runtime height patch rebuilds only the pieces it touches
/// ([`CollisionHeightfield::chunks_touching`]) instead of the whole terrain collider.
/// Neighbouring chunks share their border samples, so the pieces meet without a gap.
///
/// Field for field what `regia_physics::ColliderShape::HeightField` takes, with the collider
/// placed at [`Self::center`] (the field spans `[-0.5, 0.5] × scale` around it).
#[derive(Clone, Debug, PartialEq)]
pub struct CollisionChunk {
    /// Chunk column (along X) and row (along Z).
    pub index: [u32; 2],
    /// Rows of the height matrix: samples along **Z**.
    pub nx: usize,
    /// Columns of the height matrix: samples along **X**.
    pub nz: usize,
    /// Heights, `heights[row + col * nx]` (Z fastest), NaN in holes as in the parent.
    pub heights: Vec<f32>,
    /// `[width along X, 1, width along Z]`, metres.
    pub scale: [f32; 3],
    /// World position of the collider: the chunk's centre at height 0.
    pub center: [f32; 3],
}

impl CollisionHeightfield {
    /// Chunks along X and Z when each chunk spans `cells` cells (the last ones may be
    /// smaller); `[0, 0]` for `cells == 0` or a field with fewer than 2 × 2 samples.
    #[must_use]
    pub fn chunk_grid(&self, cells: usize) -> [u32; 2] {
        if cells == 0 || self.nx < 2 || self.nz < 2 {
            return [0, 0];
        }
        [
            (self.nz - 1).div_ceil(cells) as u32,
            (self.nx - 1).div_ceil(cells) as u32,
        ]
    }

    /// Chunk `index` (`[x, z]`) of `cells` cells a side ([`CollisionChunk`]); `None` past
    /// [`Self::chunk_grid`].
    #[must_use]
    pub fn chunk(&self, cells: usize, index: [u32; 2]) -> Option<CollisionChunk> {
        let [gx, gz] = self.chunk_grid(cells);
        if index[0] >= gx || index[1] >= gz || self.heights.len() != self.nx * self.nz {
            return None;
        }
        let (c0, r0) = (index[0] as usize * cells, index[1] as usize * cells);
        let (c1, r1) = ((c0 + cells).min(self.nz - 1), (r0 + cells).min(self.nx - 1));
        let (rows, cols) = (r1 - r0 + 1, c1 - c0 + 1);
        let mut heights = Vec::with_capacity(rows * cols);
        for col in c0..=c1 {
            heights.extend_from_slice(&self.heights[r0 + col * self.nx..=r1 + col * self.nx]);
        }
        let dx = self.scale[0] / (self.nz - 1) as f32;
        let dz = self.scale[2] / (self.nx - 1) as f32;
        Some(CollisionChunk {
            index,
            nx: rows,
            nz: cols,
            heights,
            scale: [(cols - 1) as f32 * dx, self.scale[1], (rows - 1) as f32 * dz],
            center: [
                -0.5 * self.scale[0] + (c0 + c1) as f32 * 0.5 * dx + self.center[0],
                0.0,
                -0.5 * self.scale[2] + (r0 + r1) as f32 * 0.5 * dz + self.center[2],
            ],
        })
    }

    /// The chunks (`cells` cells a side) holding any sample of `rect` (a
    /// [`crate::TerrainHeightsChanged::rect`]), sorted by row then column: the colliders to
    /// rebuild after [`Self::patch`]. A sample on a chunk border belongs to both chunks.
    #[must_use]
    pub fn chunks_touching(&self, cells: usize, rect: &DirtyRect) -> Vec<[u32; 2]> {
        let [gx, gz] = self.chunk_grid(cells);
        if gx == 0 || rect.is_empty() {
            return Vec::new();
        }
        // Chunk i holds samples i·cells ..= (i + 1)·cells.
        let span = |lo: u32, hi_excl: u32, n: u32| {
            let (lo, hi, last_chunk) = (lo as usize, hi_excl as usize - 1, n as usize - 1);
            let first = if lo > 0 && lo.is_multiple_of(cells) {
                lo / cells - 1
            } else {
                lo / cells
            };
            (first.min(last_chunk), (hi / cells).min(last_chunk))
        };
        let (x0, x1) = span(rect.col_start, rect.col_end, gx);
        let (z0, z1) = span(rect.row_start, rect.row_end, gz);
        let mut out = Vec::with_capacity((x1 - x0 + 1) * (z1 - z0 + 1));
        for z in z0..=z1 {
            for x in x0..=x1 {
                out.push([x as u32, z as u32]);
            }
        }
        out
    }

    /// Rewrites the collider's heights over the sample rectangle `rect` from `map`, after a
    /// runtime height patch ([`crate::TerrainHeightsChanged`], ADR-1595), cutting `holes`
    /// there exactly as [`TerrainMap::collision_heightfield`] does, and keeps
    /// [`Self::cut_samples`] current. Costs `O(rect)`, not `O(map)`. Returns the samples
    /// written: 0 when `map` is not this collider's grid.
    pub fn patch(&mut self, map: &TerrainMap, holes: Option<&HolesMask>, rect: &DirtyRect) -> usize {
        let (cols, rows) = (map.width() as usize, map.height() as usize);
        if self.nx != rows || self.nz != cols || self.heights.len() != rows * cols {
            return 0;
        }
        let holes = holes.filter(|m| !m.is_empty());
        let margin_x = map.sample_spacing() * 0.5;
        let margin_z = map.sample_spacing_z() * 0.5;
        let src = map.heights();
        let mut written = 0;
        for row in rect.row_start as usize..(rect.row_end as usize).min(rows) {
            for col in rect.col_start as usize..(rect.col_end as usize).min(cols) {
                let slot = &mut self.heights[row + col * rows];
                if !slot.is_finite() {
                    self.cut_samples = self.cut_samples.saturating_sub(1);
                }
                let [x, z] = map.sample_to_world(col as u32, row as u32);
                *slot = if holes.is_some_and(|m| m.cuts_xz(x, z, margin_x, margin_z)) {
                    self.cut_samples += 1;
                    f32::NAN
                } else {
                    src[row * cols + col]
                };
                written += 1;
            }
        }
        written
    }

    /// Height of the collider surface at world `(x, z)`: the triangle of the cell under the
    /// point, split as the collider splits it. `None` outside the field or in a removed
    /// (hole) cell.
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> Option<f32> {
        let (rows, cols) = (self.nx, self.nz);
        if rows < 2 || cols < 2 || self.heights.len() != rows * cols {
            return None;
        }
        let (sx, sz) = (self.scale[0], self.scale[2]);
        if !(sx > 0.0 && sz > 0.0) {
            return None;
        }
        let ux = (x - self.center[0]) / sx + 0.5;
        let uz = (z - self.center[2]) / sz + 0.5;
        if !(0.0..=1.0).contains(&ux) || !(0.0..=1.0).contains(&uz) {
            return None;
        }
        let fx = ux * (cols - 1) as f32;
        let fz = uz * (rows - 1) as f32;
        let j = (fx.floor() as usize).min(cols - 2);
        let i = (fz.floor() as usize).min(rows - 2);
        let u = fx - j as f32;
        let v = fz - i as f32;
        let at = |row: usize, col: usize| self.heights[row + col * rows];
        let h00 = at(i, j);
        let h01 = at(i, j + 1); // (x1, z0)
        let h10 = at(i + 1, j); // (x0, z1)
        let h11 = at(i + 1, j + 1);
        if !(h00.is_finite() && h01.is_finite() && h10.is_finite() && h11.is_finite()) {
            return None;
        }
        // Triangles (p00, p10, p01) and (p10, p11, p01): diagonal (x0, z1)-(x1, z0).
        Some(if u + v <= 1.0 {
            h00 + (h01 - h00) * u + (h10 - h00) * v
        } else {
            h11 + (h10 - h11) * (1.0 - u) + (h01 - h11) * (1.0 - v)
        })
    }

    /// The largest difference between this collider and the bilinear sampler anywhere on
    /// the map: a quarter of the largest cell twist `|h00 - h10 - h01 + h11|`, metres.
    /// Removed (hole) cells are skipped.
    #[must_use]
    pub fn max_bilinear_delta(&self) -> f32 {
        let (rows, cols) = (self.nx, self.nz);
        if rows < 2 || cols < 2 || self.heights.len() != rows * cols {
            return 0.0;
        }
        let at = |row: usize, col: usize| self.heights[row + col * rows];
        let mut worst = 0.0f32;
        for j in 0..cols - 1 {
            for i in 0..rows - 1 {
                let twist = at(i, j) - at(i + 1, j) - at(i, j + 1) + at(i + 1, j + 1);
                if twist.is_finite() {
                    worst = worst.max(twist.abs() * 0.25);
                }
            }
        }
        worst
    }
}

/// How far the live map is from the 16-bit heightfield it saves to ([`TerrainMap::save_drift`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct SaveDrift {
    /// One 16-bit step, metres (`height_scale / 65535`).
    pub quantum_m: f32,
    /// Largest `|live - saved|` over every sample, metres.
    pub max_drift_m: f32,
    /// Samples outside `[height_offset, height_offset + height_scale]`: the save clamps them.
    pub clamped_samples: usize,
    /// `(column, row)` of the largest drift (`None` when the map saves exactly).
    pub worst_sample: Option<[u32; 2]>,
    /// Rounding slack of the f32 save and load expressions at the map's height magnitude,
    /// metres (a few `f32::EPSILON` of the largest `|height|` or `|offset| + |scale|`).
    pub float_slack_m: f32,
}

impl SaveDrift {
    /// `true` when nothing is clamped and every sample is within half a 16-bit step (plus
    /// float slack) of its saved value: what is drawn and collided now is what reloads.
    #[must_use]
    pub fn round_trips(&self) -> bool {
        self.clamped_samples == 0 && self.max_drift_m <= self.quantum_m * 0.5 + self.float_slack_m
    }
}

impl TerrainMap {
    /// The samples whose collider cells can touch the world XZ box `min`..`max`: every sample
    /// within one sample spacing of it, clamped to the map (empty when the box misses the map).
    /// After a hole opens or closes (ADR-1595), [`CollisionHeightfield::patch`] over this
    /// rectangle of the hole's [`crate::HoleShape::world_bounds`] re-cuts the collider there,
    /// and [`CollisionHeightfield::chunks_touching`] names the chunks to rebuild.
    #[must_use]
    pub fn sample_rect(&self, min: [f32; 2], max: [f32; 2]) -> DirtyRect {
        let empty = DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        };
        if !(min.iter().chain(&max).all(|v| v.is_finite()) && min[0] <= max[0] && min[1] <= max[1]) {
            return empty;
        }
        let [c0, r0] = self.world_to_grid(min[0], min[1]);
        let [c1, r1] = self.world_to_grid(max[0], max[1]);
        let span = |lo: f32, hi: f32, n: u32| -> Option<(u32, u32)> {
            let last = (n - 1) as f32;
            let (lo, hi) = ((lo - 1.0).floor(), (hi + 1.0).ceil());
            (hi >= 0.0 && lo <= last).then(|| (lo.max(0.0) as u32, hi.min(last) as u32 + 1))
        };
        match (span(c0, c1, self.width()), span(r0, r1, self.height())) {
            (Some((col_start, col_end)), Some((row_start, row_end))) => DirtyRect {
                col_start,
                col_end,
                row_start,
                row_end,
            },
            _ => empty,
        }
    }

    /// The map laid out for the physics heightfield collider, with every sample in a hole
    /// of `holes` (or within half the X sample spacing along X, and half the Z sample spacing
    /// along Z) cut to NaN, so the collider removes every cell the drawn cut touches
    /// (module docs). On a square grid the two reaches are the same.
    #[must_use]
    pub fn collision_heightfield(&self, holes: Option<&HolesMask>) -> CollisionHeightfield {
        let (cols, rows) = (self.width() as usize, self.height() as usize);
        let src = self.heights();
        let mut heights = vec![0.0f32; rows * cols];
        for row in 0..rows {
            for col in 0..cols {
                heights[row + col * rows] = src[row * cols + col];
            }
        }
        let mut cut_samples = 0;
        if let Some(mask) = holes {
            let margin_x = self.sample_spacing() * 0.5;
            let margin_z = self.sample_spacing_z() * 0.5;
            cut_samples = mask.cut_heights_xz(&mut heights, margin_x, margin_z, |i| {
                self.sample_to_world((i / rows) as u32, (i % rows) as u32)
            });
        }
        CollisionHeightfield {
            nx: rows,
            nz: cols,
            heights,
            scale: [self.world_width(), 1.0, self.world_depth()],
            cut_samples,
            center: [self.grid_origin()[0], 0.0, self.grid_origin()[1]],
        }
    }

    /// How far the live heights are from what [`TerrainMap::to_heightfield`] saves (module
    /// docs, "Save drift"). A map loaded from a heightfield and not edited since drifts by
    /// less than a float rounding step.
    #[must_use]
    pub fn save_drift(&self) -> SaveDrift {
        let scale = self.height_scale();
        let offset = self.height_offset();
        let inv = if scale.abs() > f32::EPSILON {
            65535.0 / scale
        } else {
            0.0
        };
        let width = self.width().max(1);
        // The load is `q / 65535 * scale + offset` in f32: a few ulps at the heights' magnitude.
        let magnitude = self
            .heights()
            .iter()
            .filter(|h| h.is_finite())
            .fold(offset.abs() + scale.abs(), |m, h| m.max(h.abs()));
        let mut drift = SaveDrift {
            quantum_m: scale.abs() / 65535.0,
            float_slack_m: 4.0 * f32::EPSILON * magnitude + 1e-6,
            ..SaveDrift::default()
        };
        for (i, &h) in self.heights().iter().enumerate() {
            let raw = ((h - offset) * inv).round();
            if !(0.0..=65535.0).contains(&raw) {
                drift.clamped_samples += 1;
            }
            let q = raw.clamp(0.0, 65535.0);
            // The exact expression `TerrainMap::new` loads a sample with.
            let saved = q / 65535.0 * scale + offset;
            let d = (h - saved).abs();
            if d > drift.max_drift_m {
                drift.max_drift_m = d;
                let i = i as u32;
                drift.worst_sample = Some([i % width, i / width]);
            }
        }
        drift
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HoleShape;
    use regia_core::math::Rng;

    /// `w × h` samples over 64 m: a different slope along X and Z, plus a bump, so a
    /// transposed layout is wrong everywhere off the diagonal.
    fn tilted(w: u32, h: u32) -> TerrainMap {
        let mut heights = Vec::with_capacity((w * h) as usize);
        for row in 0..h {
            for col in 0..w {
                let x = col as f32 / (w - 1) as f32;
                let z = row as f32 / (h - 1) as f32;
                heights.push(2.0 * x + 30.0 * z + 3.0 * (x * 9.0).sin() * (z * 7.0).cos());
            }
        }
        TerrainMap::from_heights(w, h, heights, 64.0, 100.0, -20.0).expect("map")
    }

    #[test]
    fn layout_is_z_fastest_with_rows_first() {
        let map = tilted(17, 9);
        let c = map.collision_heightfield(None);
        assert_eq!((c.nx, c.nz), (9, 17));
        assert_eq!(c.scale, [64.0, 1.0, 64.0]);
        assert_eq!(c.cut_samples, 0);
        for row in 0..9u32 {
            for col in 0..17u32 {
                assert_eq!(
                    c.heights[row as usize + col as usize * 9],
                    map.height_at_sample(i64::from(col), i64::from(row))
                );
            }
        }
    }

    #[test]
    fn collider_surface_is_the_finest_drawn_grid() {
        let map = tilted(65, 65);
        let c = map.collision_heightfield(None);
        let half = map.world_width() * 0.5;
        let mut rng = Rng::new(3);
        for _ in 0..4000 {
            let x = rng.range(-half..half);
            let z = rng.range(-half..half);
            let collider = c.height_at(x, z).expect("inside");
            let drawn = crate::cdlod::terrain_surface_height(&map, map.sample_spacing(), x, z);
            assert!(
                (collider - drawn).abs() < 1e-3,
                "({x}, {z}): collider {collider} drawn {drawn}"
            );
        }
        // At the samples themselves all three agree exactly.
        for (col, row) in [(0u32, 0u32), (64, 0), (0, 64), (64, 64), (31, 17)] {
            let [x, z] = map.sample_to_world(col, row);
            let exact = map.height_at_sample(i64::from(col), i64::from(row));
            let collider = c.height_at(x, z).expect("inside");
            assert!((collider - exact).abs() < 1e-3, "sample ({col}, {row})");
        }
        assert!(c.height_at(half + 1.0, 0.0).is_none());
        assert!(c.height_at(0.0, -half - 1.0).is_none());
    }

    #[test]
    fn collider_is_within_the_twist_bound_of_the_bilinear_sampler() {
        let map = tilted(65, 33);
        let c = map.collision_heightfield(None);
        let bound = c.max_bilinear_delta();
        assert!(bound > 0.0, "the map must twist somewhere");
        let half = map.world_width() * 0.5;
        let mut rng = Rng::new(8);
        let mut worst = 0.0f32;
        for _ in 0..4000 {
            let x = rng.range(-half..half);
            let z = rng.range(-half..half);
            let d = (c.height_at(x, z).expect("inside") - map.sample_bilinear(x, z)).abs();
            worst = worst.max(d);
        }
        assert!(worst <= bound + 1e-3, "worst {worst} > bound {bound}");
    }

    #[test]
    fn holes_remove_the_cells_they_touch() {
        let map = tilted(65, 65);
        let mask =
            HolesMask::from_shapes(vec![HoleShape::segment([-6.0, 0.0], [6.0, 0.0], 4.0)]).expect("mask");
        let c = map.collision_heightfield(Some(&mask));
        assert!(c.cut_samples > 0);
        assert!(c.height_at(0.0, 0.0).is_none(), "the trench centre");
        assert!(c.height_at(5.0, 1.5).is_none(), "inside the trench");
        assert!(c.height_at(0.0, 10.0).is_some(), "beside the trench");
        // Every point a sample spacing inside the (convex) trench is open on the collider: its
        // cell's corners are all inside, so all of them are cut.
        let mut rng = Rng::new(21);
        for _ in 0..2000 {
            let x = rng.range(-10.0..10.0);
            let z = rng.range(-4.0..4.0);
            if mask.contains_deep(x, z, 1.0) {
                assert!(c.height_at(x, z).is_none(), "({x}, {z}) is in the hole");
            }
        }
        // The twist bound ignores removed cells.
        assert!(c.max_bilinear_delta().is_finite());
    }

    /// A rectangular grid cuts samples within half the Z spacing, which a single X margin misses
    /// (ADR-5083). 5×3 samples over 8 m: X spacing 2 m, Z spacing 4 m.
    #[test]
    fn a_rectangular_grid_cuts_within_half_the_z_spacing() {
        let map = TerrainMap::from_heights(5, 3, vec![0.0; 15], 8.0, 1.0, 0.0).expect("map");
        assert!((map.sample_spacing() - 2.0).abs() < 1e-5);
        assert!((map.sample_spacing_z() - 4.0).abs() < 1e-5);
        let hole = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 2.0],
            half_extents: [0.4, 0.4],
            rotation_radians: 0.0,
        }])
        .expect("hole");
        let isotropic = {
            let mut heights = vec![0.0f32; 15];
            let margin = map.sample_spacing() * 0.5;
            hole.cut_heights(&mut heights, margin, |i| {
                map.sample_to_world((i / 3) as u32, (i % 3) as u32)
            });
            heights
        };
        assert!(
            isotropic.iter().all(|h| h.is_finite()),
            "a 1 m margin does not reach the samples: {isotropic:?}"
        );
        let c = map.collision_heightfield(Some(&hole));
        let rows = 3usize;
        let sample = |col: usize, row: usize| c.heights[row + col * rows];
        assert!(sample(2, 1).is_nan(), "world (0, 0) is within half the Z spacing");
        assert!(sample(2, 2).is_nan(), "world (0, 4) is within half the Z spacing");
        assert!(sample(2, 0).is_finite(), "world (0, -4) stays");
        assert!(c.height_at(0.0, 0.0).is_none());
        assert!(c.height_at(-4.0, -4.0).is_some(), "a far corner is still solid");
        assert!(
            crate::raycast_with_holes(
                &map,
                Some(&hole),
                glam::Vec3::new(0.0, 100.0, 2.0),
                glam::Vec3::NEG_Y,
                200.0,
            )
            .is_none(),
            "a ray straight down through the hole misses"
        );
    }

    /// Half the Z spacing is 2 m, so a hole 1 m from the sample at world (0, 0) is inside
    /// the reach. The 3×3 lattice steps by the full 2 m (z = −2, 0, 2) and misses a hole
    /// that only covers z in [0.7, 1.3]. The previous 1 m isotropic lattice probed z = 1
    /// and did cut this sample.
    #[test]
    fn a_rectangular_margin_reaches_a_hole_between_z_probes() {
        let map = TerrainMap::from_heights(5, 3, vec![0.0; 15], 8.0, 1.0, 0.0).expect("map");
        let hole = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 1.0],
            half_extents: [0.3, 0.3],
            rotation_radians: 0.0,
        }])
        .expect("hole");
        let c = map.collision_heightfield(Some(&hole));
        let rows = 3usize;
        let sample = |col: usize, row: usize| c.heights[row + col * rows];
        assert!(
            sample(2, 1).is_nan(),
            "world (0, 0) is 1 m from the hole and the Z reach is 2 m"
        );
        assert!(
            sample(2, 0).is_finite(),
            "world (0, -4) is 5 m from the hole and stays solid"
        );
    }

    /// Fix rev 2 (audfix2-a terrain-g finding 1): a 4:3 reach ratio is not an integer, and
    /// the old `round(reach / step)` folded the coarse axis to one step of 1.5 m — short of
    /// its 2 m reach — so a small hole midway between the rows cut no sample at all and the
    /// physics collider kept ground the renderer cut.
    #[test]
    fn a_non_integer_reach_ratio_still_cuts_a_hole_between_rows() {
        // 5 × 4 samples over 12 m: X spacing 3 m (reach 1.5), Z spacing 4 m (reach 2).
        let map = TerrainMap::from_heights(5, 4, vec![0.0; 20], 12.0, 1.0, 0.0).expect("map");
        assert!((map.sample_spacing() - 3.0).abs() < 1e-5);
        assert!((map.sample_spacing_z() - 4.0).abs() < 1e-5);
        // A 0.2 m Rect at the origin, midway between the rows at z = −2 and z = 2.
        let hole = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 0.0],
            half_extents: [0.1, 0.1],
            rotation_radians: 0.0,
        }])
        .expect("hole");
        let c = map.collision_heightfield(Some(&hole));
        let rows = 4usize;
        let sample = |col: usize, row: usize| c.heights[row + col * rows];
        assert!(c.cut_samples > 0, "the hole must cut at least its own samples");
        assert!(
            sample(2, 1).is_nan(),
            "world (0, −2) probes z = 0 within the 2 m reach: {}",
            sample(2, 1)
        );
        assert!(
            sample(2, 2).is_nan(),
            "world (0, 2) probes z = 0 within the 2 m reach: {}",
            sample(2, 2)
        );
        assert!(sample(2, 0).is_finite(), "world (0, −6) is 6 m away and stays");
        assert!(sample(2, 3).is_finite(), "world (0, 6) is 6 m away and stays");
        assert!(
            c.height_at(0.0, 0.0).is_none(),
            "the collider opens under the rendered hole"
        );
        assert!(c.height_at(-6.0, -6.0).is_some(), "a far corner stays solid");
    }

    /// Fix rev 2 (audfix2-a terrain-g finding 2): the visual ray follows the rendered mask
    /// exactly — a crossing is a miss only inside a hole — while the collider stays open on
    /// every cell touching a cut sample. At (0, 0) and (0, −4) they deliberately part ways:
    /// the ground is drawn there, so a pick (agent brush stroke, editor tool) must hit it.
    /// They agree on the hole itself and on solid ground.
    #[test]
    fn the_visual_ray_hits_ground_the_collider_removes() {
        let map = TerrainMap::from_heights(5, 3, vec![0.0; 15], 8.0, 1.0, 0.0).expect("map");
        let hole = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 2.0],
            half_extents: [0.4, 0.4],
            rotation_radians: 0.0,
        }])
        .expect("hole");
        let c = map.collision_heightfield(Some(&hole));
        // (0, 0) is the cut sample. (0, 2) is the hole. (0, -4) shares that sample's cell,
        // so the collider is open there too. (-4, -4) is a solid corner.
        for (x, z, collider_open, drawn) in [
            (0.0_f32, 0.0, true, true),
            (0.0, 2.0, true, false),
            (0.0, -4.0, true, true),
            (-4.0, -4.0, false, true),
        ] {
            let ray = crate::raycast_with_holes(
                &map,
                Some(&hole),
                glam::Vec3::new(x, 100.0, z),
                glam::Vec3::NEG_Y,
                200.0,
            );
            assert_eq!(
                c.height_at(x, z).is_none(),
                collider_open,
                "collider at ({x}, {z})"
            );
            assert_eq!(ray.is_some(), drawn, "ray at ({x}, {z}) is {ray:?}");
        }
    }

    #[test]
    fn a_loaded_map_saves_back_exactly() {
        let source = regia_asset::Heightfield {
            width: 4,
            height: 3,
            samples: vec![0, 1, 2, 65535, 30000, 30001, 12, 7, 65534, 9, 99, 4000],
        };
        let map = TerrainMap::new(&source, 32.0, 250.0, -10.0).expect("map");
        let drift = map.save_drift();
        assert!(drift.round_trips(), "{drift:?}");
        assert_eq!(drift.clamped_samples, 0);
        assert_eq!(map.to_heightfield().samples, source.samples);
        assert!((drift.quantum_m - 250.0 / 65535.0).abs() < 1e-9);
    }

    #[test]
    fn sculpting_past_the_height_range_is_reported_as_clamped() {
        let source = regia_asset::Heightfield {
            width: 3,
            height: 3,
            samples: vec![1000; 9],
        };
        let mut map = TerrainMap::new(&source, 10.0, 100.0, 0.0).expect("map");
        // Above the top of the range (100 m) and below the bottom (0 m).
        map.set_height(4, 130.0);
        map.set_height(0, -5.0);
        // A sub-step edit: half of it survives the save.
        let q = 100.0 / 65535.0;
        map.set_height(8, map.height_at_index(8) + 0.4 * q);
        let drift = map.save_drift();
        assert_eq!(drift.clamped_samples, 2, "{drift:?}");
        assert!((drift.max_drift_m - 30.0).abs() < 1e-3, "{drift:?}");
        assert_eq!(drift.worst_sample, Some([1, 1]));
        assert!(!drift.round_trips());
        // Without the out-of-range samples, the sub-step edit rounds within half a step.
        map.set_height(4, 50.3);
        map.set_height(0, 5.3);
        let drift = map.save_drift();
        assert_eq!(drift.clamped_samples, 0);
        assert!(drift.max_drift_m <= 0.5 * q + 1e-6, "{drift:?}");
        assert!(drift.round_trips(), "{drift:?}");
    }

    /// ADR-1595: the chunks of a collider are the same surface (border samples shared), and a
    /// patch touches exactly the chunks holding its samples.
    #[test]
    fn chunks_tile_the_collider_and_a_patch_finds_its_chunks() {
        let n = 41u32;
        let heights: Vec<f32> = (0..n * n)
            .map(|i| ((i % n) as f32 * 0.3).sin() + (i / n) as f32 * 0.05)
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, 40.0, 10.0, -5.0).expect("map");
        let whole = map.collision_heightfield(None);
        assert_eq!(whole.chunk_grid(16), [3, 3]);
        assert!(whole.chunk(16, [3, 0]).is_none());
        let last = whole.chunk(16, [2, 1]).expect("chunk");
        assert_eq!((last.nz, last.nx), (9, 17), "8 cells along X, 16 along Z");
        for z in 0..3 {
            for x in 0..3 {
                let chunk = whole.chunk(16, [x, z]).expect("chunk");
                let local = CollisionHeightfield {
                    nx: chunk.nx,
                    nz: chunk.nz,
                    heights: chunk.heights.clone(),
                    scale: chunk.scale,
                    cut_samples: 0,
                    center: [0.0; 3],
                };
                let ([x0, z0], [x1, z1]) = (
                    [
                        chunk.center[0] - chunk.scale[0] * 0.5,
                        chunk.center[2] - chunk.scale[2] * 0.5,
                    ],
                    [
                        chunk.center[0] + chunk.scale[0] * 0.5,
                        chunk.center[2] + chunk.scale[2] * 0.5,
                    ],
                );
                for j in 0..=10 {
                    for i in 0..=10 {
                        let wx = x0 + (x1 - x0) * i as f32 / 10.0;
                        let wz = z0 + (z1 - z0) * j as f32 / 10.0;
                        let a = whole.height_at(wx, wz).expect("whole");
                        let b = local
                            .height_at(wx - chunk.center[0], wz - chunk.center[2])
                            .expect("chunk");
                        assert!((a - b).abs() < 1e-4, "chunk {x},{z} at ({wx}, {wz}): {a} vs {b}");
                    }
                }
            }
        }
        let rect = |c0, c1, r0, r1| DirtyRect {
            col_start: c0,
            col_end: c1,
            row_start: r0,
            row_end: r1,
        };
        assert_eq!(whole.chunks_touching(16, &rect(3, 5, 3, 5)), vec![[0, 0]]);
        // Sample 16 is the border of chunks 0 and 1; sample 40 is the last chunk's edge.
        assert_eq!(
            whole.chunks_touching(16, &rect(16, 17, 0, 1)),
            vec![[0, 0], [1, 0]]
        );
        assert_eq!(whole.chunks_touching(16, &rect(40, 41, 40, 41)), vec![[2, 2]]);
        assert_eq!(
            whole.chunks_touching(16, &rect(30, 34, 10, 20)),
            vec![[1, 0], [2, 0], [1, 1], [2, 1]]
        );
        assert!(whole.chunks_touching(0, &rect(0, 1, 0, 1)).is_empty());
    }

    #[test]
    fn sample_rect_covers_a_world_box_and_its_cells() {
        // 41 samples over 40 m: sample i sits at x = i - 20.
        let map = TerrainMap::from_heights(41, 41, vec![0.0; 41 * 41], 40.0, 10.0, 0.0).expect("map");
        let r = map.sample_rect([-2.5, 3.0], [1.5, 4.0]);
        assert_eq!((r.col_start, r.col_end, r.row_start, r.row_end), (16, 24, 22, 26));
        let all = map.sample_rect([-100.0, -100.0], [100.0, 100.0]);
        assert_eq!(
            (all.col_start, all.col_end, all.row_start, all.row_end),
            (0, 41, 0, 41)
        );
        assert!(
            map.sample_rect([30.0, 0.0], [40.0, 1.0]).is_empty(),
            "off the map"
        );
        assert!(map.sample_rect([1.0, 0.0], [0.0, 1.0]).is_empty(), "inverted");
        assert!(map.sample_rect([f32::NAN, 0.0], [0.0, 1.0]).is_empty());
        // Re-cutting a newly opened hole over its rectangle equals a full rebuild.
        let mut field = map.collision_heightfield(None);
        let hole = crate::HoleShape::Rect {
            center: [3.0, -4.0],
            half_extents: [5.0, 2.5],
            rotation_radians: 0.4,
        };
        let mask = crate::HolesMask::from_shapes(vec![hole.clone()]).expect("mask");
        let (lo, hi) = hole.world_bounds();
        field.patch(&map, Some(&mask), &map.sample_rect(lo, hi));
        let full = map.collision_heightfield(Some(&mask));
        assert_eq!(field.cut_samples, full.cut_samples);
        assert!(
            field
                .heights
                .iter()
                .zip(&full.heights)
                .all(|(a, b)| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()))
        );
    }
}
