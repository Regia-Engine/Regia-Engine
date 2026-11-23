// SPDX-License-Identifier: MPL-2.0
//! Box-filtered height pyramid: the prefiltered surface coarse CDLOD vertices and distant
//! shading normals read, so a sharp wall seen through a coarse grid is a smooth ramp instead
//! of a regular saw-tooth.
//!
//! # Why
//!
//! A CDLOD vertex at level `l` sits on a grid of `cell(l) = node_size(l) / grid_size` metres.
//! Point-sampling the full-resolution heightfield there aliases every feature narrower than
//! two cells: a straight cliff that runs diagonally to the grid makes its vertices alternate
//! between the top and the foot of the wall, a saw-tooth of constant screen size at every
//! distance (the "stair-step terraces" of the 2026-10-07 parity plates). The fix is the
//! standard one for sampled signals — low-pass before you decimate (Williams 1983, "Pyramidal
//! Parametrics", the mip-map) — applied to the geometry: a vertex reads the pyramid level whose
//! texel matches its cell.
//!
//! # Continuity
//!
//! The level is `max(0, log2(cell(l) / texel) + t)`, where `t` is the vertex's CDLOD morph
//! factor ([`crate::cdlod::vertex_morph_t`]). A vertex fully morphed onto its parent's lattice
//! (`t = 1`) reads exactly the level its parent's unmorphed vertex reads (`cell(l + 1) =
//! 2 cell(l)`), so the crack-free argument of [`crate::cdlod`] carries over unchanged; within a
//! node the level varies continuously with the morph, so nothing pops. At the finest cells
//! (`cell <= texel`) the level is 0 and the surface is the exact bilinear heightfield — the
//! sim/render agreement near the camera is untouched.
//!
//! # Layout
//!
//! One rule, shared by this CPU mirror, the WGSL `level_dims` / `height_level` and the uploaded
//! GPU mips (ADR-3367):
//!
//! - **Size.** Level `k` has `max(1, w >> k) × max(1, h >> k)` texels ([`level_dims`]): the
//!   GPU's own mip size, which the height texture cannot change.
//! - **Content.** Texel `i` of level `k + 1` is the mean of texels `2i` and `2i + 1` of level
//!   `k` on each axis, except the last texel of an odd side: it averages the four texels
//!   `2i − 1 ..= 2i + 2` centred on it ([`parent_taps`]), so the side's trailing texel is not
//!   dropped. On an odd side (every `2^n + 1` map) the pairwise mean never saw the map's last
//!   row and column, and a coarse level's far border read the map's interior. A one-texel
//!   level averages its whole parent.
//! - **Position.** Texel `i` represents the level-0 sample coordinate `i · 2^k + (2^k − 1) / 2`.
//!   Every tap set above is centred on its texel, so a linear field is reproduced exactly at
//!   every level up to the clamped border. [`HeightMips::sample`] applies that offset; the WGSL
//!   `height_mip` mirrors it, and the slope pyramid keeps its uniform spacing.

use crate::deform::DirtyRect;
use crate::sampler::TerrainMap;

/// Highest pyramid level the renderer samples.
pub const MAX_HEIGHT_MIP: u32 = 12;

/// One level of the pyramid (metres, row-major, row 0 = −Z edge).
#[derive(Clone, Debug)]
pub struct HeightMipLevel {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Row-major heights, metres.
    pub data: Vec<f32>,
}

/// The box-filtered pyramid of a [`TerrainMap`]. Level 0 is the map itself and is not stored.
#[derive(Clone, Debug)]
pub struct HeightMips {
    base_width: u32,
    base_height: u32,
    /// Levels `1..`, coarsest last.
    levels: Vec<HeightMipLevel>,
}

/// Number of levels (including level 0) the GPU texture of a `width × height` map carries.
#[must_use]
pub fn mip_level_count(width: u32, height: u32) -> u32 {
    let largest = width.max(height).max(1);
    (u32::BITS - largest.leading_zeros()).min(MAX_HEIGHT_MIP + 1)
}

/// The pyramid level a CDLOD vertex reads: `max(0, log2(cell_m / texel_m) + morph_t)`.
///
/// `cell_m` is the node's grid cell (`node_size / grid_size`), `texel_m` the heightfield
/// sample spacing and `morph_t` the vertex morph factor in `0..=1`. The WGSL vertex shader
/// computes the same expression.
#[must_use]
pub fn vertex_mip(cell_m: f32, texel_m: f32, morph_t: f32) -> f32 {
    let ratio = (cell_m / texel_m.max(1e-9)).max(1e-9);
    let morph_t = if morph_t.is_nan() { 0.0 } else { morph_t };
    (ratio.log2() + morph_t).clamp(0.0, MAX_HEIGHT_MIP as f32)
}

impl HeightMips {
    /// Builds every level above 0 from `map`.
    #[must_use]
    pub fn build(map: &TerrainMap) -> Self {
        let count = mip_level_count(map.width(), map.height());
        let mut levels: Vec<HeightMipLevel> = Vec::with_capacity(count.saturating_sub(1) as usize);
        for k in 1..count {
            let (w, h) = level_dims(map.width(), map.height(), k);
            let data = vec![0.0; (w * h) as usize];
            levels.push(HeightMipLevel {
                width: w,
                height: h,
                data,
            });
        }
        let mut mips = Self {
            base_width: map.width(),
            base_height: map.height(),
            levels,
        };
        mips.update_region(
            map,
            &DirtyRect {
                col_start: 0,
                col_end: map.width(),
                row_start: 0,
                row_end: map.height(),
            },
        );
        mips
    }

    /// Number of levels including level 0 (the map).
    #[must_use]
    pub fn level_count(&self) -> u32 {
        self.levels.len() as u32 + 1
    }

    /// Level-0 sample spacing of a map `world_width` metres wide, metres.
    #[must_use]
    pub fn texel_m(&self, world_width: f32) -> f32 {
        world_width / (self.base_width.max(2) - 1) as f32
    }

    /// Steepest rise over run between neighbouring texels of level `k >= 1` over the square
    /// `[corner, corner + size]` (world XZ, map centred on the origin, `world_width` wide).
    /// 0 when the level does not exist.
    #[must_use]
    pub fn max_slope_in(&self, world_width: f32, corner: [f32; 2], size: f32, k: u32) -> f32 {
        let k = k.max(1).min(self.level_count().saturating_sub(1));
        let Some(lv) = self.level(k) else {
            return 0.0;
        };
        if lv.width < 2 || lv.height < 2 {
            return 0.0;
        }
        let texel0 = self.texel_m(world_width);
        let scale = (1u32 << k) as f32;
        let step = texel0 * scale;
        let half = world_width * 0.5;
        // Texel i of level k sits at level-0 sample i·2^k + (2^k − 1)/2.
        let to_texel = |w: f32| ((w + half) / texel0 - (scale - 1.0) * 0.5) / scale;
        let range = |lo: f32, hi: f32, n: u32| {
            let a = (to_texel(lo).floor().max(0.0) as u32).min(n - 2);
            let b = (to_texel(hi).ceil().max(0.0) as u32).min(n - 2).max(a);
            (a, b)
        };
        let (i0, i1) = range(corner[0], corner[0] + size, lv.width);
        let (j0, j1) = range(corner[1], corner[1] + size, lv.height);
        let w = lv.width as usize;
        let mut steepest = 0.0f32;
        for j in j0..=j1 {
            for i in i0..=i1 {
                let at = j as usize * w + i as usize;
                let h = lv.data[at];
                let dx = (lv.data[at + 1] - h) / step;
                let dz = (lv.data[at + w] - h) / step;
                steepest = steepest.max((dx * dx + dz * dz).sqrt());
            }
        }
        steepest
    }

    /// Level `k >= 1` (level 0 is the map's own heights).
    #[must_use]
    pub fn level(&self, k: u32) -> Option<&HeightMipLevel> {
        k.checked_sub(1).and_then(|i| self.levels.get(i as usize))
    }

    /// Recomputes the texels of every level that depend on the samples inside `dirty`
    /// (after a crater or any other edit of `map`). Returns, per level `k >= 1`, the texel
    /// rectangle `(col_start, col_end, row_start, row_end)` that changed, for the GPU upload.
    pub fn update_region(&mut self, map: &TerrainMap, dirty: &DirtyRect) -> Vec<(u32, u32, u32, u32)> {
        let mut changed = Vec::with_capacity(self.levels.len());
        if dirty.is_empty() || map.width() != self.base_width || map.height() != self.base_height {
            return changed;
        }
        let (mut c0, mut c1, mut r0, mut r1) =
            (dirty.col_start, dirty.col_end, dirty.row_start, dirty.row_end);
        for i in 0..self.levels.len() {
            // Texels of level k + 1 that read the dirty texels of level k ([`parent_taps`]): the
            // last texel of an odd parent side also reads parent texels `2w − 1` and `2w`.
            let (w, h) = (self.levels[i].width, self.levels[i].height);
            let (lower, upper) = self.levels.split_at_mut(i);
            let target = &mut upper[0];
            let (src_w, src_h, src): (u32, u32, &[f32]) = if i == 0 {
                (map.width(), map.height(), map.heights())
            } else {
                let prev = &lower[i - 1];
                (prev.width, prev.height, prev.data.as_slice())
            };
            (c0, c1) = child_range(c0, c1, w, src_w);
            (r0, r1) = child_range(r0, r1, h, src_h);
            for row in r0..r1 {
                let rows = parent_taps(row, h, src_h);
                for col in c0..c1 {
                    let cols = parent_taps(col, w, src_w);
                    let at = |x: u32, y: u32| src[(y * src_w + x) as usize];
                    let value = if cols.1 - cols.0 <= 1 && rows.1 - rows.0 <= 1 {
                        // One or two parents per axis: the f32 expression the pyramid has always
                        // used, so interior texels keep their bits.
                        let (x0, x1, y0, y1) = (cols.0, cols.1, rows.0, rows.1);
                        0.25 * (at(x0, y0) + at(x1, y0) + at(x0, y1) + at(x1, y1))
                    } else {
                        let mut sum = 0.0f64;
                        for y in rows.0..=rows.1 {
                            for x in cols.0..=cols.1 {
                                sum += f64::from(at(x, y));
                            }
                        }
                        let count = (cols.1 - cols.0 + 1) * (rows.1 - rows.0 + 1);
                        (sum / f64::from(count)) as f32
                    };
                    target.data[(row * target.width + col) as usize] = value;
                }
            }
            changed.push((c0, c1, r0, r1));
        }
        changed
    }

    /// Bilinear height at level-0 sample coordinates `s` (column, row; fractional) from
    /// fractional level `mip` (blending the two nearest levels). `mip = 0` is exactly
    /// [`TerrainMap::sample_bilinear_grid`].
    #[must_use]
    pub fn sample(&self, map: &TerrainMap, s: [f32; 2], mip: f32) -> f32 {
        // NaN inputs read the finest level / the map's border (`clamp` passes NaN through).
        let mip = if mip.is_nan() { 0.0 } else { mip };
        let s = [
            if s[0].is_nan() { 0.0 } else { s[0] },
            if s[1].is_nan() { 0.0 } else { s[1] },
        ];
        let top = (self.level_count() - 1) as f32;
        let mip = mip.clamp(0.0, top);
        let k0 = mip.floor() as u32;
        let f = mip - k0 as f32;
        let a = self.sample_level(map, s, k0);
        if f <= 0.0 || k0 as f32 >= top {
            return a;
        }
        let b = self.sample_level(map, s, k0 + 1);
        a + (b - a) * f
    }

    /// Bilinear height at world `(x, z)` from fractional level `mip`.
    #[must_use]
    pub fn sample_world(&self, map: &TerrainMap, x: f32, z: f32, mip: f32) -> f32 {
        self.sample(map, map.world_to_grid(x, z), mip)
    }

    /// Bilinear height of one integer level at level-0 sample coordinates.
    fn sample_level(&self, map: &TerrainMap, s: [f32; 2], k: u32) -> f32 {
        if k == 0 {
            return map.sample_bilinear_grid(s[0], s[1]);
        }
        let Some(level) = self.level(k) else {
            return map.sample_bilinear_grid(s[0], s[1]);
        };
        let scale = (1u32 << k) as f32;
        let centre = (scale - 1.0) * 0.5;
        let u = ((s[0] - centre) / scale).clamp(0.0, (level.width - 1) as f32);
        let v = ((s[1] - centre) / scale).clamp(0.0, (level.height - 1) as f32);
        let x0 = u.floor() as u32;
        let y0 = v.floor() as u32;
        let x1 = (x0 + 1).min(level.width - 1);
        let y1 = (y0 + 1).min(level.height - 1);
        let fx = u - x0 as f32;
        let fy = v - y0 as f32;
        let at = |x: u32, y: u32| level.data[(y * level.width + x) as usize];
        let a = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * fx;
        let b = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * fx;
        a + (b - a) * fy
    }
}

/// Dimensions of level `k` of a `w × h` base: `max(1, side >> k)` per axis, the GPU's own mip
/// size (the WGSL `level_dims` computes the same from the terrain uniform).
#[must_use]
pub fn level_dims(w: u32, h: u32, k: u32) -> (u32, u32) {
    (level_side(w, k), level_side(h, k))
}

/// `max(1, side >> k)`, with `k >= 32` read as the one-texel top.
fn level_side(side: u32, k: u32) -> u32 {
    side.checked_shr(k).unwrap_or(0).max(1)
}

/// The parent texels `[first, last]` (inclusive, equal weights) child texel `c` of a
/// `child`-wide level averages on one axis of its `parent`-wide parent (ADR-3367): `2c` and
/// `2c + 1`; on the last texel of an odd parent side (`parent = 2 · child + 1`) the four texels
/// `2c − 1 ..= 2c + 2`, centred on it like the pair, so the trailing texel is not dropped; on a
/// one-texel level the whole parent.
///
/// `c` at or past `child` is read as the last texel, so the taps stay inside the parent
/// ([`HeightMips::update_region`] only passes `c < child`).
#[must_use]
pub fn parent_taps(c: u32, child: u32, parent: u32) -> (u32, u32) {
    let parent = parent.max(1);
    if child <= 1 {
        return (0, parent - 1);
    }
    let c = c.min(child - 1);
    if c + 1 >= child && parent == 2 * child + 1 {
        return (2 * c - 1, 2 * c + 2);
    }
    let first = (2 * c).min(parent - 1);
    (first, (2 * c + 1).min(parent - 1))
}

/// The child texels `[start, end)` of a `child`-wide level whose taps ([`parent_taps`]) read
/// any of the dirty parent texels `[p0, p1)`.
fn child_range(p0: u32, p1: u32, child: u32, parent: u32) -> (u32, u32) {
    if child <= 1 {
        return (0, 1);
    }
    let c0 = (p0 / 2).min(child - 1);
    let mut c1 = p1.div_ceil(2).min(child).max(c0 + 1);
    // The last texel of an odd side reads parents `2·child − 3 ..= 2·child`.
    if parent == 2 * child + 1 && p1 > 2 * child - 3 {
        c1 = child;
    }
    (c0, c1)
}

/// The CPU pyramid shared by a terrain draw and its rendered-surface handles (ADR-303).
///
/// Readers take an immutable [`std::sync::Arc`] snapshot under the lock and read it after the
/// lock is released; writers copy on write ([`std::sync::Arc::make_mut`]). The lock is therefore
/// never held while a reader works, in particular not across a fan-out on the compute pool: a
/// thread waiting in `regia_core::tasks::scope` runs other queued pool tasks, and one of them
/// taking this lock would wait on its own thread forever.
#[derive(Clone, Debug)]
pub(crate) struct SharedHeightMips(std::sync::Arc<std::sync::Mutex<std::sync::Arc<HeightMips>>>);

impl SharedHeightMips {
    /// Shares `mips`.
    pub(crate) fn new(mips: HeightMips) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(std::sync::Arc::new(
            mips,
        ))))
    }

    /// The current pyramid. The lock is held only to clone the [`std::sync::Arc`].
    pub(crate) fn snapshot(&self) -> std::sync::Arc<HeightMips> {
        std::sync::Arc::clone(&self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Edits the pyramid. Snapshots taken before keep the old levels (the edit copies them
    /// when any is still alive); snapshots taken after see the edit.
    pub(crate) fn write<R>(&self, f: impl FnOnce(&mut HeightMips) -> R) -> R {
        let mut slot = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(std::sync::Arc::make_mut(&mut slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_map(n: u32) -> TerrainMap {
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                heights.push(col as f32 * 0.5 + row as f32 * 0.25);
            }
        }
        TerrainMap::from_heights(n, n, heights, (n - 1) as f32, 1.0, 0.0).expect("map")
    }

    #[test]
    fn level_zero_is_the_exact_bilinear_map() {
        let map = ramp_map(33);
        let mips = HeightMips::build(&map);
        for &(u, v) in &[(0.0, 0.0), (3.3, 7.9), (31.5, 0.25), (32.0, 32.0)] {
            let exact = map.sample_bilinear_grid(u, v);
            assert!((mips.sample(&map, [u, v], 0.0) - exact).abs() < 1e-6);
        }
    }

    #[test]
    fn a_plane_stays_the_same_plane_at_every_level() {
        // The centre offset makes every level of a linear field reproduce it exactly
        // (away from the clamped border): no level shifts the surface sideways.
        let map = ramp_map(65);
        let mips = HeightMips::build(&map);
        for k in 0..mips.level_count() - 2 {
            for &(u, v) in &[(20.0, 20.0), (30.5, 25.25), (40.0, 33.0)] {
                let exact = map.sample_bilinear_grid(u, v);
                let got = mips.sample(&map, [u, v], k as f32);
                assert!(
                    (got - exact).abs() < 1e-3,
                    "level {k} at ({u}, {v}): {got} vs {exact}"
                );
            }
        }
    }

    #[test]
    fn levels_average_their_children() {
        let n = 16u32;
        let heights: Vec<f32> = (0..n * n).map(|i| ((i * 7919) % 13) as f32).collect();
        let map = TerrainMap::from_heights(n, n, heights.clone(), 15.0, 1.0, 0.0).expect("map");
        let mips = HeightMips::build(&map);
        let l1 = mips.level(1).expect("level 1");
        assert_eq!((l1.width, l1.height), (8, 8));
        let at = |x: u32, y: u32| heights[(y * n + x) as usize];
        let want = 0.25 * (at(2, 4) + at(3, 4) + at(2, 5) + at(3, 5));
        assert!((l1.data[(2 * 8 + 1) as usize] - want).abs() < 1e-6);
        assert_eq!(mips.level_count(), mip_level_count(n, n));
        assert_eq!(mip_level_count(4096, 4096), 13);
        assert_eq!(mip_level_count(4097, 4097), 13);
    }

    #[test]
    fn region_update_matches_a_full_rebuild() {
        let n = 64u32;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % 17) as f32 * 0.3).collect();
        let mut map = TerrainMap::from_heights(n, n, heights, 63.0, 1.0, 0.0).expect("map");
        let mut mips = HeightMips::build(&map);
        let crater = crate::deform::Crater {
            center: [3.0, -5.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.4,
        };
        let dirty = crate::deform::apply_crater(&mut map, &crater).expect("crater");
        let changed = mips.update_region(&map, &dirty);
        assert_eq!(changed.len() as u32, mips.level_count() - 1);
        let fresh = HeightMips::build(&map);
        for k in 1..mips.level_count() {
            let (a, b) = (mips.level(k).expect("level"), fresh.level(k).expect("level"));
            for (x, y) in a.data.iter().zip(&b.data) {
                assert!((x - y).abs() < 1e-5, "level {k} differs after a region update");
            }
        }
    }

    #[test]
    fn vertex_mip_is_continuous_across_a_morph() {
        // A fully morphed level-l vertex reads its parent's unmorphed level.
        let texel = 0.3125;
        for cell in [0.3125_f32, 0.625, 5.0, 40.0] {
            let fine_done = vertex_mip(cell, texel, 1.0);
            let coarse_start = vertex_mip(cell * 2.0, texel, 0.0);
            assert!((fine_done - coarse_start).abs() < 1e-5);
        }
        assert_eq!(vertex_mip(0.1, texel, 0.0), 0.0);
    }

    fn cratered(map: &TerrainMap) -> (TerrainMap, DirtyRect) {
        let mut edited = TerrainMap::from_heights(
            map.width(),
            map.height(),
            map.heights().to_vec(),
            map.world_width(),
            1.0,
            0.0,
        )
        .expect("map");
        let crater = crate::deform::Crater {
            center: [3.0, -5.0],
            radius: 6.0,
            depth: 2.0,
            rim_height: 0.4,
        };
        let dirty = crate::deform::apply_crater(&mut edited, &crater).expect("crater");
        (edited, dirty)
    }

    #[test]
    fn shared_pyramid_snapshots_are_copy_on_write() {
        let n = 64u32;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % 17) as f32 * 0.3).collect();
        let map = TerrainMap::from_heights(n, n, heights, 63.0, 1.0, 0.0).expect("map");
        let (edited, dirty) = cratered(&map);
        let shared = SharedHeightMips::new(HeightMips::build(&map));
        let before = shared.snapshot();
        shared.write(|mips| mips.update_region(&edited, &dirty));
        let after = shared.snapshot();
        let (old, new) = (HeightMips::build(&map), HeightMips::build(&edited));
        for k in 1..old.level_count() {
            let level = |m: &HeightMips| m.level(k).expect("level").data.clone();
            assert_eq!(
                level(&before),
                level(&old),
                "a snapshot keeps the levels it was taken with"
            );
            for (x, y) in level(&after).iter().zip(&level(&new)) {
                assert!((x - y).abs() < 1e-5, "a later snapshot sees the edit (level {k})");
            }
        }
    }

    /// ADR-303: a reader fans out on the compute pool while it reads the pyramid, and the pool
    /// tasks read and edit the same pyramid. Under the old `Mutex<HeightMips>` (the lock held
    /// for the whole read) the tasks wait on the reader, which waits on them: a hang.
    #[test]
    fn a_fan_out_while_reading_the_pyramid_may_take_the_pyramid() {
        let n = 64u32;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % 13) as f32 * 0.2).collect();
        let map = TerrainMap::from_heights(n, n, heights, 63.0, 1.0, 0.0).expect("map");
        let (edited, dirty) = cratered(&map);
        let expected = HeightMips::build(&map).level(1).expect("level").data.clone();
        let shared = SharedHeightMips::new(HeightMips::build(&map));
        let len = expected.len();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // What `TerrainGpu::with_height_mips` does: a snapshot, read with no lock held.
            let mips = shared.snapshot();
            let mut texels = vec![0.0f32; len];
            regia_core::tasks::par_chunks_mut(&mut texels, 64, |chunk, out| {
                if chunk == 0 {
                    // A pool task editing the pyramid (a crater) while it is being read.
                    shared.write(|m| m.update_region(&edited, &dirty));
                }
                // A pool task reading it (a rendered-surface query).
                assert!(shared.snapshot().level(1).is_some());
                let level = mips.level(1).expect("level");
                out.copy_from_slice(&level.data[chunk * 64..chunk * 64 + out.len()]);
            });
            let _ = tx.send(texels);
        });
        // A failure guard only: the fan-out finishes in milliseconds.
        let texels = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("a fan-out inside a pyramid read deadlocked");
        assert_eq!(
            texels, expected,
            "the reader's snapshot is not changed by the edit"
        );
    }

    #[test]
    fn level_counts_for_tiny_and_non_square_maps() {
        assert_eq!(mip_level_count(2, 2), 2);
        assert_eq!(mip_level_count(1, 1), 1);
        assert_eq!(mip_level_count(0, 0), 1);
        assert_eq!(mip_level_count(64, 4), 7);
        assert_eq!(mip_level_count(1 << 20, 2), MAX_HEIGHT_MIP + 1);
    }

    #[test]
    fn two_by_two_map_builds_one_level_that_is_the_mean() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0, 4.0, 8.0, 12.0], 1.0, 1.0, 0.0).expect("map");
        let mips = HeightMips::build(&map);
        assert_eq!(mips.level_count(), 2);
        let l1 = mips.level(1).expect("level 1");
        assert_eq!((l1.width, l1.height), (1, 1));
        assert!((l1.data[0] - 6.0).abs() < 1e-6);
        assert!(mips.level(0).is_none() && mips.level(2).is_none());
        // A 1x1 level has no neighbouring texel pair, so there is no slope to report.
        assert_eq!(mips.max_slope_in(1.0, [-0.5, -0.5], 1.0, 1), 0.0);
        assert!(mips.sample(&map, [0.5, 0.5], 5.0).is_finite());
    }

    #[test]
    fn non_square_pyramid_clamps_the_short_axis_at_one_texel() {
        let (w, h) = (16u32, 4u32);
        let heights: Vec<f32> = (0..w * h).map(|i| (i % w) as f32).collect();
        let map = TerrainMap::from_heights(w, h, heights, 15.0, 1.0, 0.0).expect("map");
        let mips = HeightMips::build(&map);
        assert_eq!(mips.level_count(), mip_level_count(w, h));
        let dims: Vec<(u32, u32)> = (1..mips.level_count())
            .map(|k| {
                let l = mips.level(k).expect("level");
                (l.width, l.height)
            })
            .collect();
        assert_eq!(dims, vec![(8, 2), (4, 1), (2, 1), (1, 1)]);
        // Every level is finite and bounded by the source range.
        for k in 1..mips.level_count() {
            for &v in &mips.level(k).expect("level").data {
                assert!((0.0..=15.0).contains(&v), "level {k}: {v}");
            }
        }
        let steep = mips.max_slope_in(15.0, [-7.5, -7.5], 15.0, 1);
        assert!(steep.is_finite() && steep > 0.0, "slope {steep}");
    }

    #[test]
    fn sampling_outside_the_map_and_with_nan_stays_finite() {
        let map = ramp_map(17);
        let mips = HeightMips::build(&map);
        for &mip in &[0.0, 0.5, 2.0, 3.7, 99.0, -4.0, f32::NAN, f32::INFINITY] {
            for &s in &[
                [-100.0, -100.0],
                [1000.0, 1000.0],
                [f32::NAN, 3.0],
                [3.0, f32::NAN],
                [f32::INFINITY, f32::NEG_INFINITY],
            ] {
                let v = mips.sample(&map, s, mip);
                assert!(v.is_finite(), "sample {s:?} at mip {mip}: {v}");
            }
        }
        assert!(mips.sample_world(&map, f32::NAN, 0.0, 1.0).is_finite());
    }

    #[test]
    fn vertex_mip_is_clamped_and_nan_safe() {
        assert_eq!(vertex_mip(1e12, 0.1, 1.0), MAX_HEIGHT_MIP as f32);
        assert_eq!(vertex_mip(0.0, 0.1, 0.0), 0.0);
        assert_eq!(vertex_mip(1.0, 0.0, 0.0), MAX_HEIGHT_MIP as f32);
        assert_eq!(vertex_mip(f32::NAN, 0.1, 0.0), 0.0);
        let v = vertex_mip(1.0, 0.25, f32::NAN);
        assert!(
            (v - 2.0).abs() < 1e-5,
            "NaN morph reads the unmorphed level, got {v}"
        );
    }

    #[test]
    fn region_updates_ignore_empty_stale_and_out_of_range_rects() {
        let n = 16u32;
        let map = TerrainMap::from_heights(n, n, vec![1.0; (n * n) as usize], 15.0, 1.0, 0.0).expect("map");
        let mut mips = HeightMips::build(&map);
        let empty = DirtyRect {
            col_start: 4,
            col_end: 4,
            row_start: 2,
            row_end: 9,
        };
        assert!(mips.update_region(&map, &empty).is_empty());
        // A map of another size is not this pyramid's map.
        let other = TerrainMap::from_heights(8, 8, vec![2.0; 64], 7.0, 1.0, 0.0).expect("map");
        let all = DirtyRect {
            col_start: 0,
            col_end: 8,
            row_start: 0,
            row_end: 8,
        };
        assert!(mips.update_region(&other, &all).is_empty());
        assert!((mips.level(1).expect("level").data[0] - 1.0).abs() < 1e-6);
        // A rect running past the border is clamped to the level, not an out-of-bounds write.
        let past = DirtyRect {
            col_start: 12,
            col_end: 500,
            row_start: 14,
            row_end: 500,
        };
        let changed = mips.update_region(&map, &past);
        assert_eq!(changed.len() as u32, mips.level_count() - 1);
        for (k, &(c0, c1, r0, r1)) in changed.iter().enumerate() {
            let l = mips.level(k as u32 + 1).expect("level");
            assert!(
                c1 <= l.width && r1 <= l.height,
                "level {}: {c0}..{c1} {r0}..{r1}",
                k + 1
            );
        }
    }

    #[test]
    fn snapshots_survive_a_poisoned_lock() {
        let map = ramp_map(8);
        let shared = SharedHeightMips::new(HeightMips::build(&map));
        let clone = shared.clone();
        let _ = std::thread::spawn(move || {
            clone.write::<()>(|_| panic!("poison the pyramid lock"));
        })
        .join();
        assert_eq!(shared.snapshot().level_count(), mip_level_count(8, 8));
        assert_eq!(shared.write(|m| m.level_count()), mip_level_count(8, 8));
    }

    /// Hashed heights with a strong last row and column (what the pairwise pyramid dropped).
    fn odd_map(w: u32, h: u32) -> TerrainMap {
        let mut heights = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let hash = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663)) % 97;
                let edge = if x + 1 == w { 40.0 } else { 0.0 } + if y + 1 == h { 25.0 } else { 0.0 };
                heights.push(hash as f32 * 0.1 + edge);
            }
        }
        TerrainMap::from_heights(w, h, heights, 32.0, 1.0, 0.0).expect("map")
    }

    #[test]
    fn parent_taps_are_centred_and_read_every_parent() {
        for parent in 1u32..=70 {
            let child = level_side(parent, 1);
            let mut read = vec![false; parent as usize];
            for c in 0..child {
                let (first, last) = parent_taps(c, child, parent);
                assert!(first <= last && last < parent, "parent {parent} child {c}");
                for p in first..=last {
                    read[p as usize] = true;
                }
                if child >= 2 {
                    // Centred on the child, as the pair is: a linear field stays in place.
                    assert_eq!(
                        first + last,
                        4 * c + 1,
                        "parent {parent} child {c}: ({first}, {last})"
                    );
                }
            }
            assert!(read.iter().all(|&r| r), "parent {parent}: a texel no child reads");
        }
        assert_eq!(parent_taps(127, 128, 257), (253, 256));
        assert_eq!(parent_taps(126, 128, 257), (252, 253));
        assert_eq!(parent_taps(63, 64, 128), (126, 127));
        assert_eq!(parent_taps(0, 1, 3), (0, 2));
        assert_eq!(level_dims(257, 129, 1), (128, 64));
        assert_eq!(level_dims(3, 2, 1), (1, 1));
        assert_eq!(level_dims(3, 2, 40), (1, 1));
    }

    #[test]
    fn parent_taps_clamps_a_child_texel_outside_the_child() {
        // `update_region` only passes `c < child`, but the function is public: a future caller
        // passing `c >= child` used to index past the parent (`parent_taps(5, 2, 5)` gave
        // (9, 12) on a 5-wide parent). Clamped inputs must stay inside the parent, and the
        // in-range answers must be exactly the ones `update_region` has always produced.
        for parent in 1u32..=70 {
            let child = level_side(parent, 1);
            for c in 0..child + 4 {
                let (first, last) = parent_taps(c, child, parent);
                assert!(
                    first <= last && last < parent,
                    "parent {parent} child {child} c {c}: ({first}, {last})"
                );
            }
        }
        assert_eq!(parent_taps(1, 2, 5), (1, 4));
        assert_eq!(parent_taps(5, 2, 5), (1, 4));
        assert_eq!(parent_taps(127, 128, 257), (253, 256));
    }

    #[test]
    fn odd_levels_reproduce_a_plane_at_every_texel() {
        // A linear field along one axis: every texel of every level holds the field at its own
        // position `i · 2^k + (2^k − 1) / 2`, the last texel of an odd side included (its four
        // taps are centred on it). Axes a level has shrunk to one texel are not positional.
        for (w, h) in [(257u32, 129u32), (33, 17), (5, 5), (17, 4), (65, 65), (7, 3)] {
            for axis in 0..2 {
                let mut heights = Vec::with_capacity((w * h) as usize);
                for y in 0..h {
                    for x in 0..w {
                        heights.push(
                            if axis == 0 {
                                x as f32 * 0.5
                            } else {
                                y as f32 * 0.25
                            } + 3.0,
                        );
                    }
                }
                let map = TerrainMap::from_heights(w, h, heights, 32.0, 1.0, 0.0).expect("map");
                let mips = HeightMips::build(&map);
                for k in 1..mips.level_count() {
                    let level = mips.level(k).expect("level");
                    assert_eq!((level.width, level.height), level_dims(w, h, k));
                    let side = if axis == 0 { level.width } else { level.height };
                    if side < 2 {
                        continue;
                    }
                    let scale = (1u32 << k) as f32;
                    for j in 0..level.height {
                        for i in 0..level.width {
                            let pos = if axis == 0 { i } else { j } as f32 * scale + (scale - 1.0) * 0.5;
                            let want = pos * if axis == 0 { 0.5 } else { 0.25 } + 3.0;
                            let got = level.data[(j * level.width + i) as usize];
                            assert!(
                                (got - want).abs() < 1e-3,
                                "{w}x{h} axis {axis} level {k} texel ({i}, {j}): {got} vs plane {want}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_last_row_and_column_reach_every_level() {
        // Zero everywhere but the last column: the pairwise pyramid of an odd side never saw it.
        let (w, h) = (257u32, 129u32);
        let mut heights = vec![0.0f32; (w * h) as usize];
        for y in 0..h {
            heights[(y * w + w - 1) as usize] = 100.0;
        }
        let map = TerrainMap::from_heights(w, h, heights, 256.0, 1.0, 0.0).expect("map");
        let mips = HeightMips::build(&map);
        assert_eq!(
            mips.level(1).expect("level 1").data[127],
            25.0,
            "a quarter of the four taps"
        );
        for k in 1..mips.level_count() {
            let level = mips.level(k).expect("level");
            for row in 0..level.height {
                let last = level.data[(row * level.width + level.width - 1) as usize];
                assert!(last > 0.0, "level {k} row {row}: the last column is lost");
                if level.width > 1 {
                    assert_eq!(
                        level.data[(row * level.width + level.width - 2) as usize],
                        0.0,
                        "level {k}"
                    );
                }
            }
        }
    }

    #[test]
    fn odd_levels_stay_within_the_map_range() {
        for (w, h) in [(257u32, 129u32), (3, 2), (2, 3), (9, 33)] {
            let map = odd_map(w, h);
            let (lo, hi) = map
                .heights()
                .iter()
                .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            let mips = HeightMips::build(&map);
            for k in 1..mips.level_count() {
                for &v in &mips.level(k).expect("level").data {
                    assert!(
                        (lo..=hi).contains(&v),
                        "{w}x{h} level {k}: {v} outside {lo}..{hi}"
                    );
                }
            }
        }
    }

    #[test]
    fn even_sides_keep_the_pairwise_mean_bit_for_bit() {
        // Power-of-two sides (and every interior texel of an odd side) keep the f32 expression
        // the pyramid always used, so frames that never read a border texel do not move.
        let map = odd_map(64, 32);
        let mips = HeightMips::build(&map);
        let l1 = mips.level(1).expect("level 1");
        let at = |x: u32, y: u32| map.heights()[(y * 64 + x) as usize];
        for j in 0..l1.height {
            for i in 0..l1.width {
                let want = 0.25
                    * (at(2 * i, 2 * j)
                        + at(2 * i + 1, 2 * j)
                        + at(2 * i, 2 * j + 1)
                        + at(2 * i + 1, 2 * j + 1));
                assert_eq!(l1.data[(j * l1.width + i) as usize].to_bits(), want.to_bits());
            }
        }
        let odd = odd_map(65, 65);
        let mips = HeightMips::build(&odd);
        let l1 = mips.level(1).expect("level 1");
        let at = |x: u32, y: u32| odd.heights()[(y * 65 + x) as usize];
        let want = 0.25 * (at(10, 6) + at(11, 6) + at(10, 7) + at(11, 7));
        assert_eq!(l1.data[(3 * l1.width + 5) as usize].to_bits(), want.to_bits());
    }

    #[test]
    fn region_update_of_an_odd_border_matches_a_full_rebuild() {
        for (w, h) in [(65u32, 33u32), (3, 2), (257, 129)] {
            let map = odd_map(w, h);
            let mut mips = HeightMips::build(&map);
            // Edit the far border only: the last row and column of level 0.
            let mut heights = map.heights().to_vec();
            for x in 0..w {
                heights[((h - 1) * w + x) as usize] += 7.0;
            }
            for y in 0..h {
                heights[(y * w + w - 1) as usize] -= 3.0;
                // Two columns in: read by the last texel's outer tap only.
                heights[(y * w + w.saturating_sub(4)) as usize] += 1.5;
            }
            let edited = TerrainMap::from_heights(w, h, heights, 32.0, 1.0, 0.0).expect("map");
            let dirty = DirtyRect {
                col_start: w - 1,
                col_end: w,
                row_start: 0,
                row_end: h,
            };
            let _ = mips.update_region(&edited, &dirty);
            let dirty = DirtyRect {
                col_start: w.saturating_sub(4),
                col_end: w.saturating_sub(4) + 1,
                row_start: 0,
                row_end: h,
            };
            let _ = mips.update_region(&edited, &dirty);
            let dirty = DirtyRect {
                col_start: 0,
                col_end: w,
                row_start: h - 1,
                row_end: h,
            };
            let changed = mips.update_region(&edited, &dirty);
            for (k, &(c0, c1, r0, r1)) in changed.iter().enumerate() {
                let l = mips.level(k as u32 + 1).expect("level");
                assert!(
                    c0 < c1 && c1 <= l.width && r0 < r1 && r1 <= l.height,
                    "{w}x{h} level {}",
                    k + 1
                );
            }
            let fresh = HeightMips::build(&edited);
            for k in 1..mips.level_count() {
                let (a, b) = (mips.level(k).expect("level"), fresh.level(k).expect("level"));
                assert_eq!(a.data, b.data, "{w}x{h} level {k} differs after a border update");
            }
        }
    }
}
