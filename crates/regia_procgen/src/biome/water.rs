// SPDX-License-Identifier: MPL-2.0
//! Horizontal distance to water for the `WaterDistance` filter (spec 65 § 3.3).
//!
//! A texel of the captured heightfield is underwater when it lies below the level of a water plane
//! covering it, or inside a ribbon outline. The distance field is the exact Euclidean distance
//! transform of that mask (Felzenszwalb and Huttenlocher, "Distance Transforms of Sampled
//! Functions", 2012): one pass over columns, one over rows, on squared integer texel distances, so
//! it is the same on every platform. Dry texels get the distance to the nearest wet texel; wet
//! texels get minus the distance to the nearest dry one.

use crate::biome::area::polygon_depth;
use crate::data::GroundData;

/// A level water surface (a `WaterPlane`, or a `WaterBody` plane).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterLevel {
    /// World Y of the surface.
    pub level: f32,
    /// `[min_x, min_z, max_x, max_z]` it covers (`None` = everywhere, the shoreline plane).
    pub rect: Option<[f32; 4]>,
}

/// The water a biome run reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WaterSnap {
    /// Level surfaces.
    pub levels: Vec<WaterLevel>,
    /// Ribbon outlines (closed XZ polygons): inside is water.
    pub ribbons: Vec<Vec<[f32; 2]>>,
}

impl WaterSnap {
    /// Whether there is any water.
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty() && self.ribbons.is_empty()
    }
}

/// A signed distance field over the ground capture's sample grid.
#[derive(Clone, Debug, PartialEq)]
pub struct WaterField {
    origin: [f32; 2],
    step: [f32; 2],
    size: [usize; 2],
    /// Signed distances in meters, row-major.
    values: Vec<f32>,
}

const FAR: f64 = 1.0e20;

/// 1D squared distance transform of `f` (lower envelope of parabolas), in place into `d`; samples
/// are `sqrt(s2)` apart.
fn edt_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64], s2: f64) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut k = 0usize;
    v[0] = 0;
    z[0] = -FAR;
    z[1] = FAR;
    for q in 1..n {
        let fq = f[q] + s2 * (q * q) as f64;
        let mut s;
        loop {
            let p = v[k];
            s = (fq - (f[p] + s2 * (p * p) as f64)) / (2.0 * s2 * (q as f64 - p as f64));
            // z[0] = -FAR, so k never goes below 0.
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = FAR;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f64 - p as f64;
        *out = s2 * dq * dq + f[p];
    }
}

/// Squared distances to the nearest `true` cell of a `w × h` mask, where a column step is
/// `sqrt(sq[0])` long and a row step `sqrt(sq[1])` (squared steps, so non-square maps are exact).
fn edt_2d(mask: &[bool], w: usize, h: usize, sq: [f64; 2]) -> Vec<f64> {
    let mut grid: Vec<f64> = mask.iter().map(|&m| if m { 0.0 } else { FAR }).collect();
    let n = w.max(h);
    let (mut f, mut d) = (vec![0.0; n], vec![0.0; n]);
    let (mut v, mut z) = (vec![0usize; n], vec![0.0; n + 1]);
    for c in 0..w {
        for r in 0..h {
            f[r] = grid[r * w + c];
        }
        edt_1d(&f[..h], &mut d[..h], &mut v, &mut z, sq[1]);
        for r in 0..h {
            grid[r * w + c] = d[r];
        }
    }
    for r in 0..h {
        f[..w].copy_from_slice(&grid[r * w..(r + 1) * w]);
        edt_1d(&f[..w], &mut d[..w], &mut v, &mut z, sq[0]);
        grid[r * w..(r + 1) * w].copy_from_slice(&d[..w]);
    }
    grid
}

impl WaterField {
    /// Builds the field over the ground capture (`None` when there is no water: the filter then
    /// sees +infinity everywhere).
    pub fn build(ground: &GroundData, water: &WaterSnap) -> Option<Self> {
        if water.is_empty() {
            return None;
        }
        let _span = tracing::info_span!("biome_water_field").entered();
        let (w, h) = (ground.size[0] as usize, ground.size[1] as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let step = [
            ground.world_width / (ground.map_size[0].max(2) - 1) as f32,
            ground.world_width / (ground.map_size[1].max(2) - 1) as f32,
        ];
        let half = ground.world_width * 0.5;
        let origin = [
            ground.origin[0] as f32 * step[0] - half,
            ground.origin[1] as f32 * step[1] - half,
        ];
        let mut wet = vec![false; w * h];
        for r in 0..h {
            for c in 0..w {
                let x = origin[0] + c as f32 * step[0];
                let z = origin[1] + r as f32 * step[1];
                let y = ground.heights[r * w + c];
                let under = water.levels.iter().any(|l| {
                    y < l.level
                        && l.rect
                            .is_none_or(|b| x >= b[0] && x <= b[2] && z >= b[1] && z <= b[3])
                }) || water.ribbons.iter().any(|p| polygon_depth(p, x, z) > 0.0);
                wet[r * w + c] = under;
            }
        }
        let sq = [f64::from(step[0]).powi(2), f64::from(step[1]).powi(2)];
        let to_wet = edt_2d(&wet, w, h, sq);
        let dry: Vec<bool> = wet.iter().map(|b| !b).collect();
        let to_dry = edt_2d(&dry, w, h, sq);
        let values = (0..w * h)
            .map(|i| {
                if wet[i] {
                    -(to_dry[i].min(FAR).sqrt()) as f32
                } else {
                    (to_wet[i].min(FAR).sqrt()) as f32
                }
            })
            .collect();
        Some(Self {
            origin,
            step,
            size: [w, h],
            values,
        })
    }

    /// The signed distance at world XZ (nearest sample; clamped to the grid).
    pub fn at(&self, x: f32, z: f32) -> f32 {
        let c = ((x - self.origin[0]) / self.step[0]).round();
        let r = ((z - self.origin[1]) / self.step[1]).round();
        let c = if c.is_nan() {
            0
        } else {
            (c.max(0.0) as usize).min(self.size[0] - 1)
        };
        let r = if r.is_nan() {
            0
        } else {
            (r.max(0.0) as usize).min(self.size[1] - 1)
        };
        self.values[r * self.size[0] + c]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edt_matches_brute_force() {
        let (w, h) = (23usize, 17usize);
        let mask: Vec<bool> = (0..w * h)
            .map(|i| regia_core::math::hash::pcg_hash(i as u32 * 7 + 3).is_multiple_of(13))
            .collect();
        let d = edt_2d(&mask, w, h, [1.0, 1.0]);
        for r in 0..h {
            for c in 0..w {
                let mut best = FAR;
                for rr in 0..h {
                    for cc in 0..w {
                        if mask[rr * w + cc] {
                            let (dr, dc) = (rr as f64 - r as f64, cc as f64 - c as f64);
                            best = best.min(dr * dr + dc * dc);
                        }
                    }
                }
                assert_eq!(d[r * w + c], best, "cell {c},{r}");
            }
        }
    }

    #[test]
    fn edt_non_square_steps_are_meters() {
        // One wet cell at (0,0); a dry cell one row down is 2 m away (row step 2 m), one column
        // across 1 m away (column step 1 m).
        let (w, h) = (3usize, 3usize);
        let mut mask = vec![false; w * h];
        mask[0] = true;
        let d = edt_2d(&mask, w, h, [1.0, 4.0]);
        assert_eq!(d[w], 4.0, "one row down is 2 m");
        assert_eq!(d[1], 1.0, "one column across is 1 m");
        assert_eq!(d[w + 1], 5.0, "diagonal is sqrt(1 + 4) m");
        assert_eq!(d[2 * w + 2], 4.0 + 16.0);
    }
}
