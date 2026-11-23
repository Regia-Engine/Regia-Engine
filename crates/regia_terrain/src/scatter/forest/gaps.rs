// SPDX-License-Identifier: MPL-2.0
//! Canopy gaps for a dense forest (ADR-1503): a deterministic density map with tree-fall gaps
//! and clearings cut into it, so a flythrough bursts out of the crowns into light and dives back
//! in, instead of crossing a uniform carpet.
//!
//! Public forest ecology, used as targets, not as data:
//!
//! - Gap sizes follow a power law: the frequency of gaps of area `A` falls as `A^-λ` with `λ`
//!   about 1.6-2.2 across very different forests (Kellner & Asner, "Convergent structural
//!   responses of tropical forests to diverse disturbance regimes", *Ecology Letters* 12, 2009;
//!   the same shape is reported for temperate stands). Sizes are drawn from the truncated
//!   power law by inverting its cumulative distribution.
//! - Gaps cover on the order of a tenth of an old forest's area (Runkle, "Patterns of
//!   disturbance in some old-growth mesic forests of eastern North America", *Ecology* 63,
//!   1982, reports about 10 %); [`CanopyGaps::gap_fraction`] is the target.
//! - A gap is roughly elliptical (a fallen crown lies along its fall line) with a ragged edge;
//!   the edge is softened over [`CanopyGaps::edge_m`] so trees thin into it.
//!
//! The map multiplies a class's keep probability like any density map (`docs/formats/terrain.md`
//! § 2): white keeps every candidate, black none. Placement drops only the candidates the map
//! darkens, so adding gaps never moves the trees that stay.

use regia_core::math::Rng;

use super::super::ScatterError;
use super::super::placement::DensityMap;

/// Largest map side the generator writes (64 MB of `f32`).
pub const MAX_GAP_MAP_SIDE: u32 = 4096;
/// Most gaps one map holds.
const MAX_GAPS: usize = 200_000;

/// How the gaps are drawn.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CanopyGaps {
    /// Seed (the same seed and settings give the same map).
    pub seed: u64,
    /// Target share of the zone's area inside gaps (0..0.9). Overlaps make the measured share
    /// ([`GapMap::measured_fraction`]) a little lower.
    pub gap_fraction: f32,
    /// Smallest gap, square metres (a single fallen crown).
    pub min_area_m2: f32,
    /// Largest gap, square metres (a clearing).
    pub max_area_m2: f32,
    /// Power-law exponent `λ` of the gap-size frequency (> 1).
    pub exponent: f32,
    /// Soft edge width, metres.
    pub edge_m: f32,
    /// Largest length-to-width ratio of a gap (>= 1).
    pub elongation: f32,
    /// Raggedness of the edge: relative radius noise (0 = smooth ellipses).
    pub roughness: f32,
    /// Zone the gaps are drawn in, world XZ minimum (metres; the whole terrain when the zone is
    /// empty).
    pub zone_min: [f32; 2],
    /// Zone maximum.
    pub zone_max: [f32; 2],
}

impl Default for CanopyGaps {
    fn default() -> Self {
        Self {
            seed: 0x6a70_5f67_6170_7331,
            gap_fraction: 0.1,
            min_area_m2: 25.0,
            max_area_m2: 2000.0,
            exponent: 1.9,
            edge_m: 4.0,
            elongation: 1.8,
            roughness: 0.2,
            zone_min: [0.0, 0.0],
            zone_max: [0.0, 0.0],
        }
    }
}

impl CanopyGaps {
    /// Values in range (the exponent above 1, areas ordered and positive, fractions clamped).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let fin = |v: f32, def: f32| if v.is_finite() { v } else { def };
        let min_area = fin(self.min_area_m2, d.min_area_m2).max(1.0);
        Self {
            seed: self.seed,
            gap_fraction: fin(self.gap_fraction, d.gap_fraction).clamp(0.0, 0.9),
            min_area_m2: min_area,
            max_area_m2: fin(self.max_area_m2, d.max_area_m2).max(min_area),
            exponent: {
                let e = fin(self.exponent, d.exponent);
                if (e - 1.0).abs() < 1e-3 {
                    1.001
                } else {
                    e.max(1.001)
                }
            },
            edge_m: fin(self.edge_m, d.edge_m).max(0.0),
            elongation: fin(self.elongation, d.elongation).max(1.0),
            roughness: fin(self.roughness, d.roughness).clamp(0.0, 0.6),
            zone_min: self.zone_min,
            zone_max: self.zone_max,
        }
    }

    /// Gap area for a uniform `u` in `0..1` (inverse of the truncated power-law distribution).
    #[must_use]
    pub fn area_for(&self, u: f32) -> f32 {
        let k = 1.0 - self.exponent;
        let (a, b) = (self.min_area_m2.powf(k), self.max_area_m2.powf(k));
        (a + u.clamp(0.0, 1.0) * (b - a)).powf(1.0 / k)
    }
}

/// One drawn gap.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Gap {
    /// Centre, world XZ metres.
    pub centre: [f32; 2],
    /// Semi-axes, metres (long, short).
    pub axes: [f32; 2],
    /// Direction of the long axis, radians from +X toward +Z.
    pub angle: f32,
    /// Area of the ellipse, square metres.
    pub area_m2: f32,
}

/// A rasterised gap map over the whole terrain.
#[derive(Clone, Debug, PartialEq)]
pub struct GapMap {
    /// Pixels per side.
    pub side: u32,
    /// Terrain width the map covers, metres (centred on the origin, the heightfield mapping).
    pub world_width: f32,
    /// Keep values, row-major, row 0 at −Z (0 in a gap, 1 under closed canopy).
    pub values: Vec<f32>,
    /// The gaps drawn, in draw order.
    pub gaps: Vec<Gap>,
    /// The zone the gaps were drawn in.
    pub zone: ([f32; 2], [f32; 2]),
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Draws and rasterises the gaps of `spec` on a `side`² map of a `world_width` terrain.
///
/// # Errors
///
/// [`ScatterError::BadParameter`] for a side outside `2..=MAX_GAP_MAP_SIDE` or a terrain width
/// that is not positive.
pub fn canopy_gap_map(spec: &CanopyGaps, world_width: f32, side: u32) -> Result<GapMap, ScatterError> {
    let _span = tracing::info_span!(target: "regia_terrain", "forest_gap_map").entered();
    if !(2..=MAX_GAP_MAP_SIDE).contains(&side) {
        return Err(ScatterError::BadParameter {
            field: "side",
            value: side as f32,
        });
    }
    if !(world_width.is_finite() && world_width > 0.0) {
        return Err(ScatterError::BadParameter {
            field: "world_width",
            value: world_width,
        });
    }
    let spec = spec.sanitized();
    let half = world_width * 0.5;
    let (zmin, zmax) = if spec.zone_max[0] > spec.zone_min[0] && spec.zone_max[1] > spec.zone_min[1] {
        (
            [spec.zone_min[0].max(-half), spec.zone_min[1].max(-half)],
            [spec.zone_max[0].min(half), spec.zone_max[1].min(half)],
        )
    } else {
        ([-half, -half], [half, half])
    };
    let zone_area = ((zmax[0] - zmin[0]) * (zmax[1] - zmin[1])).max(0.0);
    let mut rng = Rng::with_stream(spec.seed, 0x6761_7073);
    let mut gaps = Vec::new();
    let mut drawn = 0.0f32;
    let target = spec.gap_fraction * zone_area;
    while drawn < target && gaps.len() < MAX_GAPS {
        let area = spec.area_for(rng.f32());
        let aspect = 1.0 + (spec.elongation - 1.0) * rng.f32();
        let r = (area / std::f32::consts::PI).sqrt();
        let gap = Gap {
            centre: [
                zmin[0] + (zmax[0] - zmin[0]) * rng.f32(),
                zmin[1] + (zmax[1] - zmin[1]) * rng.f32(),
            ],
            axes: [r * aspect.sqrt(), r / aspect.sqrt()],
            angle: rng.f32() * std::f32::consts::PI,
            area_m2: area,
        };
        drawn += area;
        gaps.push(gap);
    }
    let n = side as usize;
    let mut values = vec![1.0f32; n * n];
    let px = world_width / (side - 1) as f32;
    let noise_seed = (spec.seed ^ (spec.seed >> 32)) as u32;
    for (gi, gap) in gaps.iter().enumerate() {
        let reach = gap.axes[0] * (1.0 + spec.roughness) + spec.edge_m;
        let col = |x: f32| (((x + half) / px).round() as i64).clamp(0, n as i64 - 1) as usize;
        let (c0, c1) = (col(gap.centre[0] - reach), col(gap.centre[0] + reach));
        let (r0, r1) = (col(gap.centre[1] - reach), col(gap.centre[1] + reach));
        let (sin, cos) = gap.angle.sin_cos();
        for row in r0..=r1 {
            let z = -half + row as f32 * px;
            for c in c0..=c1 {
                let x = -half + c as f32 * px;
                let (dx, dz) = (x - gap.centre[0], z - gap.centre[1]);
                // Into the gap's frame: u along the long axis.
                let u = dx * cos + dz * sin;
                let v = -dx * sin + dz * cos;
                let s = ((u / gap.axes[0]).powi(2) + (v / gap.axes[1]).powi(2)).sqrt();
                // Ragged edge: the boundary radius wobbles with low-frequency noise around the rim.
                let angle = v.atan2(u);
                let wobble = crate::noise::fbm2(
                    angle.cos() * 2.0 + gi as f32 * 7.31,
                    angle.sin() * 2.0,
                    noise_seed,
                ) * 2.0
                    - 1.0;
                let rim = 1.0 + spec.roughness * wobble;
                // Distance past the rim in metres along the mean radius.
                let mean = (gap.axes[0] * gap.axes[1]).sqrt();
                let past = (s - rim) * mean;
                let keep = smoothstep(-0.5 * spec.edge_m, 0.5 * spec.edge_m, past);
                let cell = &mut values[row * n + c];
                *cell = cell.min(keep);
            }
        }
    }
    Ok(GapMap {
        side,
        world_width,
        values,
        gaps,
        zone: (zmin, zmax),
    })
}

impl GapMap {
    /// The map as a scatter [`DensityMap`].
    ///
    /// # Errors
    ///
    /// See [`DensityMap::new`] (never for a map this module built).
    pub fn density_map(&self) -> Result<DensityMap, ScatterError> {
        DensityMap::new(self.side, self.side, self.values.clone())
    }

    /// This map multiplied into `other` (an existing class density map over the same terrain),
    /// at this map's resolution.
    ///
    /// # Errors
    ///
    /// See [`DensityMap::new`].
    pub fn combined(&self, other: &DensityMap) -> Result<DensityMap, ScatterError> {
        let n = self.side as usize;
        let half = self.world_width * 0.5;
        let px = self.world_width / (self.side - 1) as f32;
        let mut values = self.values.clone();
        for (i, v) in values.iter_mut().enumerate() {
            let (row, col) = (i / n, i % n);
            *v *= other.sample(-half + col as f32 * px, -half + row as f32 * px, self.world_width);
        }
        DensityMap::new(self.side, self.side, values)
    }

    /// 16-bit grayscale samples (row-major, row 0 at −Z), for a `.png` density map the
    /// `.scatter.ron` can name.
    #[must_use]
    pub fn to_gray16(&self) -> Vec<u16> {
        self.values
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16)
            .collect()
    }

    /// Measured share of the zone darker than one half (inside a gap).
    #[must_use]
    pub fn measured_fraction(&self) -> f32 {
        let n = self.side as usize;
        let half = self.world_width * 0.5;
        let px = self.world_width / (self.side - 1) as f32;
        let (zmin, zmax) = self.zone;
        let (mut inside, mut total) = (0u64, 0u64);
        for (i, v) in self.values.iter().enumerate() {
            let (row, col) = (i / n, i % n);
            let (x, z) = (-half + col as f32 * px, -half + row as f32 * px);
            if x >= zmin[0] && x <= zmax[0] && z >= zmin[1] && z <= zmax[1] {
                total += 1;
                inside += u64::from(*v < 0.5);
            }
        }
        if total == 0 {
            0.0
        } else {
            inside as f32 / total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CanopyGaps {
        CanopyGaps {
            zone_min: [-200.0, -200.0],
            zone_max: [200.0, 200.0],
            ..CanopyGaps::default()
        }
    }

    #[test]
    fn same_seed_same_map_and_another_seed_differs() {
        let a = canopy_gap_map(&spec(), 512.0, 257).expect("map");
        let b = canopy_gap_map(&spec(), 512.0, 257).expect("map");
        assert_eq!(a, b);
        let c = canopy_gap_map(&CanopyGaps { seed: 7, ..spec() }, 512.0, 257).expect("map");
        assert_ne!(a.values, c.values);
    }

    #[test]
    fn the_gap_share_lands_near_the_target_and_stays_in_the_zone() {
        let map = canopy_gap_map(&spec(), 512.0, 513).expect("map");
        let f = map.measured_fraction();
        // Overlaps and the ragged rim move it a little below the drawn total.
        assert!((0.06..=0.12).contains(&f), "measured gap share {f}");
        // Outside the zone (plus the largest gap's reach) the canopy is closed.
        let n = map.side as usize;
        let px = map.world_width / (map.side - 1) as f32;
        for (i, v) in map.values.iter().enumerate() {
            let (x, z) = (-256.0 + (i % n) as f32 * px, -256.0 + (i / n) as f32 * px);
            if x.abs() > 250.0 || z.abs() > 250.0 {
                assert!(*v > 0.999, "gap at ({x}, {z})");
            }
        }
    }

    #[test]
    fn gap_sizes_follow_the_power_law() {
        let s = spec().sanitized();
        // The inverse CDF hits both ends and is monotonic.
        assert!((s.area_for(0.0) - s.min_area_m2).abs() < 1e-2);
        assert!((s.area_for(1.0) - s.max_area_m2).abs() < 1.0);
        assert!(s.area_for(0.3) < s.area_for(0.6));
        let map = canopy_gap_map(&spec(), 512.0, 129).expect("map");
        // Small gaps dominate: the median is within a few times the minimum.
        let mut areas: Vec<f32> = map.gaps.iter().map(|g| g.area_m2).collect();
        areas.sort_by(f32::total_cmp);
        let median = areas[areas.len() / 2];
        assert!(median < 4.0 * s.min_area_m2, "median {median}");
        assert!(areas.len() > 20);
    }

    #[test]
    fn combining_only_darkens() {
        let map = canopy_gap_map(&spec(), 512.0, 129).expect("map");
        let half = DensityMap::constant(0.5);
        let combined = map.combined(&half).expect("combined");
        for (i, v) in map.values.iter().enumerate() {
            let (n, px) = (129usize, 512.0 / 128.0);
            let (x, z) = (-256.0 + (i % n) as f32 * px, -256.0 + (i / n) as f32 * px);
            assert!((combined.sample(x, z, 512.0) - v * 0.5).abs() < 1e-4);
        }
        assert!(canopy_gap_map(&spec(), 512.0, 1).is_err());
        assert!(canopy_gap_map(&spec(), -1.0, 64).is_err());
    }
}
