// SPDX-License-Identifier: MPL-2.0
//! Sunlight bounced off the terrain, on the CPU, for meshes standing on the ground.
//!
//! The terrain shader adds light reflected by sunlit walls and slopes onto each point
//! (`horizon_sky_and_bounce` in `terrain.wgsl`, ADR-065). Meshes and scatter rocks standing
//! on that ground need the same light, or a rock on a shadowed trench floor prints darker than
//! the floor under it. [`bounce_factor`] is the exact CPU mirror of the shader's bounce term
//! (four diagonal sectors × four distances over the box-filtered height pyramid);
//! [`BounceGrid`] samples it on a grid around the camera, and
//! [`crate::TerrainSharedDraw`] hands that grid to the renderer as
//! [`regia_render::GroundBounce`].

use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

/// `Σ_sectors ¼ · sin²θ_h · max(n_wall·L, 0) · lit` at `world` (the shader's bounce factor),
/// for a horizontal receiver. `to_sun` is the unit vector toward the sun; `footprint_m` picks
/// the pyramid level of the near samples exactly as the shader's pixel footprint does. `lit`
/// is the part of each sector's wall above the shadow of the opposite sector's wall.
///
/// The bounce irradiance over π is `ρ_wall · E_sun · factor / π` (see `terrain.wgsl`).
#[must_use]
pub fn bounce_factor(
    map: &TerrainMap,
    mips: &HeightMips,
    world: [f32; 3],
    to_sun: [f32; 3],
    footprint_m: f32,
) -> f32 {
    bounce_and_horizon(map, mips, world, to_sun, footprint_m).0
}

/// [`bounce_factor`] and the raw horizon: `tan θ_h` per diagonal sector (azimuths 45°, 135°,
/// 225°, 315° from +X toward +Z; 0 when nothing rises above the point).
///
/// The bounce factor uses the sixteen point loads at 10, 30, 90 and 270 m, the same distances
/// as `horizon_sky_and_bounce` in `terrain.wgsl`. The terrain sky term stops at 270 m. The
/// returned horizon then takes the maximum with two more loads per sector, at 810 m and 2430 m.
/// Meshes read that extended horizon through [`regia_render::GroundBounceLevel::horizon`].
#[must_use]
pub fn bounce_and_horizon(
    map: &TerrainMap,
    mips: &HeightMips,
    world: [f32; 3],
    to_sun: [f32; 3],
    footprint_m: f32,
) -> (f32, [f32; 4]) {
    let texel = map.sample_spacing().max(1e-6);
    let top = mips.level_count().saturating_sub(1);
    let s0 = map.world_to_grid(world[0], world[2]);
    let to_s = (map.width() - 1) as f32 / map.world_width();
    let dirs: [[f32; 2]; 4] = std::array::from_fn(|a| {
        let ang = a as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4;
        [ang.cos(), ang.sin()]
    });
    // Raw horizon slope per sector and the distance of the wall that sets it.
    let mut tv = [0.0f32; 4];
    let mut dv = [10.0f32; 4];
    // Slope of the terrain just below each horizon point (the wall's face).
    let mut wv = [0.0f32; 4];
    // The four distances (10, 30, 90, 270 m) and their pyramid levels are the same for every
    // sector.
    let mut steps = [(0.0f32, 0u32); 4];
    let mut d = 10.0f32;
    for step in &mut steps {
        let size = (d * 0.25).max(footprint_m);
        let level = ((size.max(1e-6) / texel).log2().clamp(0.0, 12.0).round() as u32).min(top);
        *step = (d, level);
        d *= 3.0;
    }
    for (a, dir) in dirs.iter().enumerate() {
        let (mut prev_h, mut prev_d) = (world[1], 0.0f32);
        for &(d, level) in &steps {
            let h = point_load(
                map,
                mips,
                [s0[0] + dir[0] * d * to_s, s0[1] + dir[1] * d * to_s],
                level,
            );
            let slope = (h - world[1]) / d;
            if slope > tv[a] {
                tv[a] = slope;
                dv[a] = d;
                wv[a] = (h - prev_h) / (d - prev_d);
            }
            prev_h = h;
            prev_d = d;
        }
    }
    let mut bounce = 0.0;
    for (a, dir) in dirs.iter().enumerate() {
        let max_t = tv[a];
        let open = 1.0 / (1.0 + max_t * max_t);
        let face_t = wv[a].max(max_t);
        let wall = [-dir[0] * face_t, 1.0, -dir[1] * face_t];
        let len = (wall[0] * wall[0] + wall[1] * wall[1] + wall[2] * wall[2]).sqrt();
        let cos = (wall[0] * to_sun[0] + wall[1] * to_sun[1] + wall[2] * to_sun[2]) / len;
        let toward = -(to_sun[0] * dir[0] + to_sun[2] * dir[1]);
        let h1 = max_t * dv[a];
        let opp = (a + 2) % 4;
        let lit = if toward > 1e-3 && h1 > 1e-3 {
            let shadow_h = tv[opp] * dv[opp] - (dv[a] + dv[opp]) * to_sun[1] / toward;
            (1.0 - shadow_h.max(0.0) / h1).clamp(0.0, 1.0)
        } else {
            1.0
        };
        bounce += (1.0 - open) * cos.max(0.0) * lit;
    }
    // The sky horizon reaches further than the walls that bounce: mesas 0.3–2.5 km out still
    // hide a mesh's low sky (the Switchbacks plate's mid spot, `lookdev dyn_light`). Two more
    // loads per sector at 810 m and 2430 m, at the pyramid level the same rule picks.
    let mut horizon = tv;
    for (a, dir) in dirs.iter().enumerate() {
        let mut d = 810.0f32;
        for _ in 0..2 {
            let size = (d * 0.25).max(footprint_m);
            let level = ((size.max(1e-6) / texel).log2().clamp(0.0, 12.0).round() as u32).min(top);
            let h = point_load(
                map,
                mips,
                [s0[0] + dir[0] * d * to_s, s0[1] + dir[1] * d * to_s],
                level,
            );
            horizon[a] = horizon[a].max((h - world[1]) / d);
            d *= 3.0;
        }
    }
    (bounce * 0.25, horizon)
}

/// Nearest texel of pyramid level `level` at level-0 sample coordinates `s` (the shader's
/// `textureLoad(height_texture, round(p), level)`).
fn point_load(map: &TerrainMap, mips: &HeightMips, s: [f32; 2], level: u32) -> f32 {
    if level == 0 {
        let col = s[0].round() as i64;
        let row = s[1].round() as i64;
        return map.height_at_sample(col, row);
    }
    let Some(lv) = mips.level(level) else {
        return map.sample_bilinear_grid(s[0], s[1]);
    };
    let scale = (1u32 << level) as f32;
    let centre = (scale - 1.0) * 0.5;
    let x = (((s[0] - centre) / scale).round() as i64).clamp(0, i64::from(lv.width) - 1) as u32;
    let y = (((s[1] - centre) / scale).round() as i64).clamp(0, i64::from(lv.height) - 1) as u32;
    lv.data[(y * lv.width + x) as usize]
}

/// Bounce factors an update evaluates at least before it spreads its rows over the compute pool.
const PARALLEL_FACTORS: u64 = 512;
/// Row chunks a parallel update is split into.
const PARALLEL_TASKS: usize = 16;

/// Sun components in the cache key are rounded to this step (about 0.056°). A 1 ulp
/// perturbation of a unit sun stays in the same bin, so it does not rebuild the grid.
const SUN_KEY_STEP: f32 = 1.0 / 1024.0;

/// Quantised sun bits for [`GridKey`]. Non-finite components keep their own bits.
fn sun_key(to_sun: [f32; 3]) -> [u32; 3] {
    to_sun.map(|c| {
        if !c.is_finite() {
            return c.to_bits();
        }
        let mut q = (c / SUN_KEY_STEP).round() * SUN_KEY_STEP;
        if q == 0.0 {
            q = 0.0;
        }
        q.to_bits()
    })
}

/// What a [`BounceGrid`] was computed for: (snapped centre, quantised sun bits, heights
/// generation, cell bits, size, supersample).
type GridKey = ([i64; 2], [u32; 3], u64, u32, u32, u32);

/// [`bounce_factor`] on a square grid of cells around a centre, kept until the camera moves a
/// cell or the sun or the heights change. When only the camera moved, the cells the old and
/// new grids share are copied, so a moving camera recomputes just the rows it uncovered.
#[derive(Clone, Debug, Default)]
pub struct BounceGrid {
    key: Option<GridKey>,
    /// World XZ of the grid's −X/−Z corner.
    pub origin: [f32; 2],
    /// Cell edge, metres.
    pub cell_m: f32,
    /// Cells per side.
    pub size: u32,
    /// Bounce factor per cell, row-major (row = +Z): at the cell centre, or the mean of
    /// `supersample`² points spread over the cell.
    pub factor: Vec<f32>,
    /// Horizon `tan θ_h` per diagonal sector per cell ([`bounce_and_horizon`]), same layout
    /// and supersampling as [`Self::factor`].
    pub horizon: Vec<[f32; 4]>,
    /// Cells evaluated by the last [`Self::update`] (0 when it kept the grid).
    pub evaluated: u32,
    /// Previous factor grid, so a scroll can overwrite [`Self::factor`] in place.
    scratch_factor: Vec<f32>,
    /// Previous horizon grid, paired with `scratch_factor`.
    scratch_horizon: Vec<[f32; 4]>,
    /// Updates that took the whole-grid path (a cut, a sun or height change) (tests).
    #[cfg(test)]
    whole_builds: u32,
}

impl BounceGrid {
    /// Recomputes the grid around `eye_xz` (snapped to the cell) when the snapped centre, the
    /// quantised sun (`SUN_KEY_STEP`, 1/1024) or `heights_generation` changed. Each cell is the
    /// mean of `supersample`² factors spread evenly over it (1 = the centre). A camera scroll
    /// writes the new strip into the existing buffers. Returns `true` when any cell changed.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        map: &TerrainMap,
        mips: &HeightMips,
        eye_xz: [f32; 2],
        to_sun: [f32; 3],
        heights_generation: u64,
        cell_m: f32,
        size: u32,
        supersample: u32,
    ) -> bool {
        let ss = supersample.max(1);
        let snapped = [
            (eye_xz[0] / cell_m).round() as i64,
            (eye_xz[1] / cell_m).round() as i64,
        ];
        let key = (
            snapped,
            sun_key(to_sun),
            heights_generation,
            cell_m.to_bits(),
            size,
            ss,
        );
        self.evaluated = 0;
        if self.key == Some(key) {
            return false;
        }
        let _span = tracing::info_span!("terrain_bounce_grid").entered();
        let n = size as usize;
        // Same sun, heights and grid shape: scroll the existing buffers instead of
        // allocating a fresh factor and horizon.
        let scroll = match self.key {
            // The grids must overlap: a cut shares no cell, and scrolling it would evaluate
            // every cell in order on this thread instead of on the compute pool below.
            Some((c, sun, g, cb, sz, s))
                if (sun, g, cb, sz, s) == (key.1, key.2, key.3, key.4, key.5)
                    && snapped[0].abs_diff(c[0]) < u64::from(size)
                    && snapped[1].abs_diff(c[1]) < u64::from(size)
                    && n > 0
                    && self.factor.len() == n * n
                    && self.horizon.len() == n * n =>
            {
                Some(c)
            }
            _ => None,
        };
        self.key = Some(key);
        self.cell_m = cell_m;
        self.size = size;
        let half = i64::from(size / 2);
        self.origin = [
            (snapped[0] - half) as f32 * cell_m,
            (snapped[1] - half) as f32 * cell_m,
        ];
        if let Some(old_c) = scroll {
            self.evaluated = self.scroll_into(map, mips, old_c, snapped, to_sun, ss);
            return true;
        }
        // A camera cut or a sun change re-evaluates every cell (both grids are 8 192 factors,
        // about 4.7 ms on one core: the encode of every 2026-10-08 soak cut). Rows go to the
        // compute pool. Each cell is the same arithmetic either way, so the grid is bit-identical.
        #[cfg(test)]
        {
            self.whole_builds += 1;
        }
        let mut cells = vec![(0.0f32, [0.0f32; 4]); n * n];
        let origin = self.origin;
        let fill_rows = |first_row: usize, rows: &mut [(f32, [f32; 4])]| -> u32 {
            let mut evaluated = 0;
            for (r, row) in rows.chunks_mut(n.max(1)).enumerate() {
                let j = (first_row + r) as u32;
                for (i, cell) in row.iter_mut().enumerate() {
                    let i = i as u32;
                    let mut sum = 0.0;
                    let mut sum_h = [0.0f32; 4];
                    for sj in 0..ss {
                        for si in 0..ss {
                            let x = origin[0] + (i as f32 + (si as f32 + 0.5) / ss as f32) * cell_m;
                            let z = origin[1] + (j as f32 + (sj as f32 + 0.5) / ss as f32) * cell_m;
                            let y = map.sample_bilinear(x, z);
                            let (f, h) = bounce_and_horizon(map, mips, [x, y, z], to_sun, 0.0);
                            sum += f;
                            for (acc, v) in sum_h.iter_mut().zip(h) {
                                *acc += v;
                            }
                        }
                    }
                    let samples = (ss * ss) as f32;
                    *cell = (sum / samples, sum_h.map(|v| v / samples));
                    evaluated += 1;
                }
            }
            evaluated
        };
        let fresh = u64::from(size) * u64::from(size);
        if fresh * u64::from(ss * ss) >= PARALLEL_FACTORS && n > 1 {
            let rows_per_task = n.div_ceil(PARALLEL_TASKS).max(1);
            self.evaluated =
                regia_core::tasks::par_chunks_mut(&mut cells, rows_per_task * n, |chunk, rows| {
                    fill_rows(chunk * rows_per_task, rows)
                })
                .into_iter()
                .sum();
        } else {
            self.evaluated = fill_rows(0, &mut cells);
        }
        self.factor = cells.iter().map(|c| c.0).collect();
        self.horizon = cells.iter().map(|c| c.1).collect();
        true
    }

    /// Copies the overlapping cells into the existing buffers and evaluates the strip the
    /// camera uncovered. `factor` and `horizon` keep their allocation.
    fn scroll_into(
        &mut self,
        map: &TerrainMap,
        mips: &HeightMips,
        old_c: [i64; 2],
        snapped: [i64; 2],
        to_sun: [f32; 3],
        ss: u32,
    ) -> u32 {
        let n = self.size as usize;
        self.scratch_factor.clear();
        self.scratch_factor.extend_from_slice(&self.factor);
        self.scratch_horizon.clear();
        self.scratch_horizon.extend_from_slice(&self.horizon);
        let dx = snapped[0] - old_c[0];
        let dz = snapped[1] - old_c[1];
        let origin = self.origin;
        let cell_m = self.cell_m;
        let limit = i64::from(self.size);
        let mut evaluated = 0u32;
        for j in 0..self.size {
            for i in 0..self.size {
                let idx = j as usize * n + i as usize;
                let oi = i64::from(i) + dx;
                let oj = i64::from(j) + dz;
                if (0..limit).contains(&oi) && (0..limit).contains(&oj) {
                    let from = oj as usize * n + oi as usize;
                    self.factor[idx] = self.scratch_factor[from];
                    self.horizon[idx] = self.scratch_horizon[from];
                    continue;
                }
                let mut sum = 0.0;
                let mut sum_h = [0.0f32; 4];
                for sj in 0..ss {
                    for si in 0..ss {
                        let x = origin[0] + (i as f32 + (si as f32 + 0.5) / ss as f32) * cell_m;
                        let z = origin[1] + (j as f32 + (sj as f32 + 0.5) / ss as f32) * cell_m;
                        let y = map.sample_bilinear(x, z);
                        let (f, h) = bounce_and_horizon(map, mips, [x, y, z], to_sun, 0.0);
                        sum += f;
                        for (acc, v) in sum_h.iter_mut().zip(h) {
                            *acc += v;
                        }
                    }
                }
                let samples = (ss * ss) as f32;
                self.factor[idx] = sum / samples;
                self.horizon[idx] = sum_h.map(|v| v / samples);
                evaluated += 1;
            }
        }
        evaluated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trench() -> TerrainMap {
        let n = 257u32;
        let width = 128.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for _row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                heights.push(-20.0 * (1.0 - ((x.abs() - 12.0) / 2.0).clamp(0.0, 1.0)));
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 24.0, -22.0).expect("map")
    }

    #[test]
    fn flat_ground_gets_no_bounce_and_a_trench_floor_does() {
        let flat = TerrainMap::from_heights(65, 65, vec![1.0; 65 * 65], 64.0, 2.0, 0.0).expect("flat");
        let flat_mips = HeightMips::build(&flat);
        let sun = [-0.866, 0.5, 0.0];
        assert_eq!(bounce_factor(&flat, &flat_mips, [0.0, 1.0, 0.0], sun, 0.0), 0.0);
        let map = trench();
        let mips = HeightMips::build(&map);
        let floor = bounce_factor(&map, &mips, [0.0, -20.0, 0.0], sun, 0.0);
        // Positive and bounded by the blocked view factor. At the centre the walls are seen at
        // the 30 m samples (20 m over 30 m: sin²θ ≈ 0.31); the two sectors whose wall faces
        // the sun give 0.31 × 0.76, the other two 0.31 × 0.08, × ¼ each ≈ 0.13.
        assert!(floor > 0.02 && floor < 0.25, "trench floor bounce factor {floor}");
        // The trench walls (20 m high, 12–14 m out along ±X, so ~17–20 m along the diagonals)
        // rise ~45° in every sector seen from the floor's centre; flat ground has no horizon.
        let (_, horizon) = bounce_and_horizon(&map, &mips, [0.0, -20.0, 0.0], sun, 0.0);
        assert!(horizon.iter().all(|t| *t > 0.5 && *t < 2.5), "{horizon:?}");
        assert_eq!(
            bounce_and_horizon(&flat, &flat_mips, [0.0, 1.0, 0.0], sun, 0.0).1,
            [0.0; 4]
        );
    }

    #[test]
    fn the_grid_follows_the_camera_by_cells_and_caches() {
        let map = trench();
        let mips = HeightMips::build(&map);
        let mut grid = BounceGrid::default();
        let sun = [-0.866, 0.5, 0.0];
        assert!(grid.update(&map, &mips, [0.0, 0.0], sun, 0, 4.0, 16, 1));
        assert_eq!(grid.factor.len(), 256);
        assert_eq!(grid.evaluated, 256);
        assert!(!grid.update(&map, &mips, [0.5, -0.3], sun, 0, 4.0, 16, 1));
        assert_eq!(grid.evaluated, 0);
        // Two cells along +X: only the two uncovered columns are evaluated, and the scrolled
        // grid equals a fresh one.
        assert!(grid.update(&map, &mips, [8.0, 0.0], sun, 0, 4.0, 16, 1));
        assert_eq!(grid.evaluated, 32);
        let mut fresh = BounceGrid::default();
        fresh.update(&map, &mips, [8.0, 0.0], sun, 0, 4.0, 16, 1);
        assert_eq!(grid.factor, fresh.factor);
        assert_eq!(grid.horizon, fresh.horizon);
        assert_eq!(grid.origin, fresh.origin);
        assert!(grid.update(&map, &mips, [8.0, 0.0], sun, 1, 4.0, 16, 1));
        assert_eq!(grid.evaluated, 256);
        // A 1 ulp sun change stays in the same 1/1024 bin: the grid is kept.
        let mut nudged = sun;
        nudged[0] = f32::from_bits(sun[0].to_bits().wrapping_add(1));
        assert_ne!(nudged[0].to_bits(), sun[0].to_bits());
        let kept = !grid.update(&map, &mips, [8.0, 0.0], nudged, 1, 4.0, 16, 1);
        assert!(kept, "1 ulp sun rebuilt {} cells", grid.evaluated);
        assert_eq!(grid.evaluated, 0);
        // One cell along +X writes the new column into the existing buffers.
        grid.factor.reserve(1024);
        grid.horizon.reserve(1024);
        let factor_cap = grid.factor.capacity();
        let horizon_cap = grid.horizon.capacity();
        assert!(factor_cap > grid.factor.len());
        assert!(horizon_cap > grid.horizon.len());
        assert!(grid.update(&map, &mips, [12.0, 0.0], sun, 1, 4.0, 16, 1));
        assert_eq!(grid.evaluated, 16, "one uncovered column");
        assert_eq!(grid.factor.capacity(), factor_cap);
        assert_eq!(grid.horizon.capacity(), horizon_cap);
        let mut fresh = BounceGrid::default();
        fresh.update(&map, &mips, [12.0, 0.0], sun, 1, 4.0, 16, 1);
        assert_eq!(grid.factor, fresh.factor);
        assert_eq!(grid.horizon, fresh.horizon);
        // A sun in another bin still rebuilds every cell.
        assert!(grid.update(&map, &mips, [12.0, 0.0], [0.0, 1.0, 0.0], 1, 4.0, 16, 1));
        assert_eq!(grid.evaluated, 256);
    }

    /// A camera cut with the same sun shares no cell with the old grid: it takes the
    /// whole-grid path (the compute pool), not the cell-by-cell scroll, and gives the same grid
    /// as a fresh build. A one-cell move still scrolls.
    #[test]
    fn a_cut_with_the_same_sun_rebuilds_instead_of_scrolling() {
        let map = trench();
        let mips = HeightMips::build(&map);
        let sun = [-0.866, 0.5, 0.0];
        let (cell_m, size, ss) = (2.0, 16u32, 1u32);
        let mut grid = BounceGrid::default();
        assert!(grid.update(&map, &mips, [0.0, 0.0], sun, 0, cell_m, size, ss));
        assert_eq!(grid.whole_builds, 1);
        assert!(grid.update(&map, &mips, [cell_m, 0.0], sun, 0, cell_m, size, ss));
        assert_eq!(grid.whole_builds, 1, "a one-cell move scrolls");
        assert_eq!(grid.evaluated, size);
        let far = [cell_m * 1000.0, 0.0];
        assert!(grid.update(&map, &mips, far, sun, 0, cell_m, size, ss));
        assert_eq!(grid.whole_builds, 2, "a cut scrolled cell by cell");
        assert_eq!(grid.evaluated, size * size);
        let mut fresh = BounceGrid::default();
        fresh.update(&map, &mips, far, sun, 0, cell_m, size, ss);
        assert_eq!(
            grid.factor.iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
            fresh.factor.iter().map(|f| f.to_bits()).collect::<Vec<_>>()
        );
    }

    /// A whole-grid update (a camera cut) runs on the compute pool and is bit-identical to
    /// evaluating every cell in order on one thread; a scrolled grid still copies its overlap.
    #[test]
    fn a_cut_evaluates_on_the_pool_bit_identically() {
        let map = trench();
        let mips = HeightMips::build(&map);
        let sun = [-0.866, 0.5, 0.0];
        let (cell_m, size, ss) = (2.0, 32u32, 2u32);
        assert!(u64::from(size * size * ss * ss) >= PARALLEL_FACTORS);
        let mut grid = BounceGrid::default();
        assert!(grid.update(&map, &mips, [3.0, -5.0], sun, 0, cell_m, size, ss));
        assert_eq!(grid.evaluated, size * size);
        let serial: Vec<f32> = (0..size)
            .flat_map(|j| (0..size).map(move |i| (i, j)))
            .map(|(i, j)| {
                let mut sum = 0.0;
                for sj in 0..ss {
                    for si in 0..ss {
                        let x = grid.origin[0] + (i as f32 + (si as f32 + 0.5) / ss as f32) * cell_m;
                        let z = grid.origin[1] + (j as f32 + (sj as f32 + 0.5) / ss as f32) * cell_m;
                        let y = map.sample_bilinear(x, z);
                        sum += bounce_factor(&map, &mips, [x, y, z], sun, 0.0);
                    }
                }
                sum / (ss * ss) as f32
            })
            .collect();
        assert_eq!(
            grid.factor.iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
            serial.iter().map(|f| f.to_bits()).collect::<Vec<_>>()
        );
        // One cell along −Z and three along +X: 32 + 3 · 31 cells uncovered.
        assert!(grid.update(
            &map,
            &mips,
            [3.0 + 3.0 * cell_m, -5.0 - cell_m],
            sun,
            0,
            cell_m,
            size,
            ss
        ));
        assert_eq!(grid.evaluated, 32 + 3 * 31);
        let mut fresh = BounceGrid::default();
        fresh.update(
            &map,
            &mips,
            [3.0 + 3.0 * cell_m, -5.0 - cell_m],
            sun,
            0,
            cell_m,
            size,
            ss,
        );
        assert_eq!(grid.factor, fresh.factor);
    }
}
