// SPDX-License-Identifier: MPL-2.0
//! The simulation ground: the unchanged low-resolution heightfield the gameplay physics
//! collides with, and the visual-vs-sim delta view (spec 26 § 1 line ~10).
//!
//! DO27 draws a 4096² visual heightfield over its **unchanged** 256² simulation grid. Regia Engine
//! never resamples the sim grid: [`SimGround`] wraps it and answers
//! `TerrainSampler`-identical bilinear height queries — the exact math of
//! [`crate::sampler::TerrainMap`] (border-clamped bilinear over the quantised u16 samples, the
//! same conversion DO27's sampler applies) — so the character/vehicle ground *is* the sim
//! surface wherever the two fields differ. The visual map may add relief on top; the delta is
//! exposed for the debug view ([`visual_sim_delta`]) and measured by the tests.

use regia_asset::Heightfield;

use crate::TerrainMap;
use crate::sampler::TerrainMapError;

/// DO27 world extent shared by the 256² sim grid and the 4096² visual grid, metres.
///
/// 4096 samples across this width is `1280 / 4095 ≈ 0.313` m per texel.
pub const DO27_WORLD_WIDTH_M: f32 = 1280.0;

/// Visual heightfield resolution (samples along one edge).
pub const DO27_VISUAL_RESOLUTION: u32 = 4096;

/// Simulation heightfield resolution. Regia Engine never resamples this grid.
pub const DO27_SIM_RESOLUTION: u32 = 256;

/// Slope from up, radians, below which a cell is driveable. Matches the WGSL debug view.
pub const DRIVEABLE_SLOPE_RAD: f32 = 0.6;

/// Driveable cells must stay within this many metres of the sim surface.
pub const DRIVEABLE_DELTA_M: f32 = 0.25;

/// Cliff cells must stay within this many metres of the sim surface.
pub const CLIFF_DELTA_M: f32 = 2.0;

/// Cooked-package ceiling for two DO27 maps, bytes (6 GiB).
pub const DO27_PACKAGE_BUDGET_BYTES: u64 = 6 * 1024 * 1024 * 1024;

/// How the visual-vs-sim debug view classifies one cell.
///
/// The WGSL view (`debug_view == 7`) uses the same thresholds: green / red for
/// driveable within / outside ±0.25 m, blue / yellow for cliffs within / outside ±2 m.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgreementFlag {
    /// Driveable, |visual − sim| ≤ [`DRIVEABLE_DELTA_M`].
    WithinDriveable,
    /// Cliff, |visual − sim| ≤ [`CLIFF_DELTA_M`].
    WithinCliff,
    /// Driveable and outside ±0.25 m.
    DriveableOver,
    /// Cliff and outside ±2 m.
    CliffOver,
}

/// Classifies a cell from its slope (radians from up) and signed height delta (metres).
pub fn agreement_flag(slope_rad: f32, delta_m: f32) -> AgreementFlag {
    let driveable = slope_rad < DRIVEABLE_SLOPE_RAD;
    let abs = delta_m.abs();
    if driveable {
        if abs > DRIVEABLE_DELTA_M {
            AgreementFlag::DriveableOver
        } else {
            AgreementFlag::WithinDriveable
        }
    } else if abs > CLIFF_DELTA_M {
        AgreementFlag::CliffOver
    } else {
        AgreementFlag::WithinCliff
    }
}

/// One GPU's stated terrain budget. Frame times are **targets** (spec 26 § 6's 2.5 ms
/// floor-tier ceiling at 1080p, tightened for the 5090). They are not a measurement
/// from this machine. Memory is the resident tile-cache allowance from the terrain-data
/// cook (`docs/evidence/terrain-data/budgets.md`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuBudget {
    /// Profile name.
    pub gpu: &'static str,
    /// Target framebuffer height, pixels.
    pub height_px: u32,
    /// Terrain-pass target, milliseconds.
    pub terrain_pass_ms: f32,
    /// Resident tile-cache budget, gibibytes.
    pub tile_budget_gib: f32,
}

/// RTX 5090, RX 7900 XTX, and Steam Deck Medium.
pub fn do27_gpu_budgets() -> &'static [GpuBudget] {
    &[
        GpuBudget {
            gpu: "RTX 5090",
            height_px: 1440,
            terrain_pass_ms: 2.0,
            tile_budget_gib: 8.0,
        },
        GpuBudget {
            gpu: "RX 7900 XTX",
            height_px: 1440,
            terrain_pass_ms: 2.5,
            tile_budget_gib: 6.0,
        },
        GpuBudget {
            gpu: "Steam Deck (Medium)",
            height_px: 800,
            terrain_pass_ms: 4.0,
            tile_budget_gib: 1.0,
        },
    ]
}

/// The gameplay ground: a low-resolution heightfield with `TerrainSampler`-identical queries.
///
/// Build this from the *same* cooked heightfield the simulation reads (the DO27 256² grid);
/// the queries below then agree with the sim's sampler by construction.
#[derive(Clone, Debug)]
pub struct SimGround {
    map: TerrainMap,
}

impl SimGround {
    /// Wraps the simulation heightfield with its world mapping.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainMapError`] (wrapped in [`crate::TerrainError::InvalidTerrain`]) when
    /// the heightfield or mapping is invalid; see [`TerrainMap::new`].
    pub fn new(
        heightfield: &Heightfield,
        world_width: f32,
        height_scale: f32,
        height_offset: f32,
    ) -> Result<Self, TerrainMapError> {
        Ok(Self {
            map: TerrainMap::new(heightfield, world_width, height_scale, height_offset)?,
        })
    }

    /// The underlying map (for LOD ranges, min/max, the debug view).
    pub fn map(&self) -> &TerrainMap {
        &self.map
    }

    /// Ground height at world XZ: the `TerrainSampler`-identical bilinear query, in metres.
    ///
    /// This is what characters and vehicles stand on. It never consults the visual map.
    pub fn height(&self, x: f32, z: f32) -> f32 {
        self.map.sample_bilinear(x, z)
    }

    /// Ground normal at world XZ (central differences, the character-controller convention).
    pub fn normal(&self, x: f32, z: f32) -> [f32; 3] {
        self.map.normal_at(x, z)
    }

    /// Grid resolution of the sim heightfield (e.g. 256 for DO27) — never resampled.
    pub fn resolution(&self) -> u32 {
        self.map.width().max(self.map.height())
    }
}

/// Signed visual-minus-sim height delta at world XZ, in metres.
///
/// Positive: the visual surface is above the sim ground (characters appear to sink);
/// negative: the visual surface is below (characters float).
pub fn visual_sim_delta(visual: &TerrainMap, sim: &SimGround, x: f32, z: f32) -> f32 {
    visual.sample_bilinear(x, z) - sim.height(x, z)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bilinear metres from a raw `u16` buffer. No [`TerrainMap`] method is called:
    /// if `sample_bilinear` changes its weights or its u16 conversion, `SimGround::height`
    /// (which goes through that method) diverges from this formula.
    fn raw_u16_bilinear(samples: &[u16], n: u32, world: f32, scale: f32, offset: f32, x: f32, z: f32) -> f32 {
        let half = world * 0.5;
        let u = ((x + half) / world * (n as f32 - 1.0)).clamp(0.0, n as f32 - 1.0);
        let v = ((z + half) / world * (n as f32 - 1.0)).clamp(0.0, n as f32 - 1.0);
        let x0 = u.floor() as u32;
        let z0 = v.floor() as u32;
        let x1 = (x0 + 1).min(n - 1);
        let z1 = (z0 + 1).min(n - 1);
        let fu = u - x0 as f32;
        let fv = v - z0 as f32;
        let metres = |s: u16| f32::from(s) / 65535.0 * scale + offset;
        let h00 = metres(samples[(z0 * n + x0) as usize]);
        let h10 = metres(samples[(z0 * n + x1) as usize]);
        let h01 = metres(samples[(z1 * n + x0) as usize]);
        let h11 = metres(samples[(z1 * n + x1) as usize]);
        let a = h00 + (h10 - h00) * fu;
        let b = h01 + (h11 - h01) * fu;
        a + (b - a) * fv
    }

    /// 256² ramp over the DO27 1280 m extent. The raw buffer stays in the test.
    fn ramp_sim() -> (Vec<u16>, SimGround) {
        let n = DO27_SIM_RESOLUTION;
        let world = DO27_WORLD_WIDTH_M;
        let scale = world;
        let samples: Vec<u16> = (0..n * n)
            .map(|i| {
                let col = i % n;
                // Height rises with column, 0 at −X to `scale` at +X.
                (col as f32 / (n - 1) as f32 * 65535.0).round() as u16
            })
            .collect();
        let sim = SimGround::new(
            &Heightfield {
                width: n,
                height: n,
                samples: samples.clone(),
            },
            world,
            scale,
            0.0,
        )
        .expect("sim ground");
        (samples, sim)
    }

    #[test]
    fn sim_queries_match_raw_u16_bilinear() {
        let (samples, sim) = ramp_sim();
        assert_eq!(sim.resolution(), DO27_SIM_RESOLUTION);
        assert_eq!(sim.map().world_width(), DO27_WORLD_WIDTH_M);
        let n = DO27_SIM_RESOLUTION;
        let world = DO27_WORLD_WIDTH_M;
        let half = world * 0.5;
        let mut rng = regia_core::math::Rng::new(2026);
        for _ in 0..10_000 {
            let x = rng.range(-half..half);
            let z = rng.range(-half..half);
            let reference = raw_u16_bilinear(&samples, n, world, world, 0.0, x, z);
            let got = sim.height(x, z);
            let err = (got - reference).abs();
            assert!(
                err < 1e-3,
                "sample_bilinear diverged from the raw u16 bilinear: {err} m at ({x}, {z}) \
                 (got {got}, reference {reference})"
            );
        }
        // A point on the far half of the 1280 m map. A sim built on a ~255 m extent
        // would clamp here and return the border sample instead of the ramp.
        let far = 500.0f32;
        let reference = raw_u16_bilinear(&samples, n, world, world, 0.0, far, 0.0);
        let got = sim.height(far, 0.0);
        assert!(
            (got - reference).abs() < 1e-3,
            "far-half clamp: {got} vs {reference}"
        );
        assert!(
            (got - (far + half)).abs() < 1.0,
            "x = {far} should sit near height {} on the ramp, got {got}",
            far + half
        );
    }

    #[test]
    fn visual_and_sim_share_the_do27_extent() {
        let texel = DO27_WORLD_WIDTH_M / (DO27_VISUAL_RESOLUTION - 1) as f32;
        assert!(
            (texel - 0.3126).abs() < 0.001,
            "4096² over 1280 m should be ~0.31 m/texel, got {texel}"
        );
        let world = DO27_WORLD_WIDTH_M;
        let (samples, sim) = ramp_sim();
        // Visual is denser and adds a ripple, but covers the same world.
        let vis_n = 128u32;
        let mut heights = Vec::with_capacity((vis_n * vis_n) as usize);
        for row in 0..vis_n {
            for col in 0..vis_n {
                let x = col as f32 / (vis_n - 1) as f32 * world - world * 0.5;
                let z = row as f32 / (vis_n - 1) as f32 * world - world * 0.5;
                let base = raw_u16_bilinear(&samples, DO27_SIM_RESOLUTION, world, world, 0.0, x, z);
                heights.push(base + 0.4 * (x * 0.2).sin());
            }
        }
        let visual = TerrainMap::from_heights(vis_n, vis_n, heights, world, world, 0.0).expect("visual");
        assert_eq!(visual.world_width(), sim.map().world_width());
        let x = 500.0;
        let delta = visual_sim_delta(&visual, &sim, x, 0.0);
        assert!(
            delta.abs() > 0.05 && delta.abs() < 0.5,
            "shared extent should show the ripple, not a clamped hundreds-of-meters delta: {delta}"
        );
        // The ground query is the sim, not the visual.
        let ground = sim.height(x, 0.0);
        assert!((visual.sample_bilinear(x, 0.0) - ground - delta).abs() < 1e-3);
    }

    #[test]
    fn agreement_flags_use_the_do27_bounds() {
        assert_eq!(agreement_flag(0.1, 0.1), AgreementFlag::WithinDriveable);
        assert_eq!(agreement_flag(0.1, 0.25), AgreementFlag::WithinDriveable);
        assert_eq!(agreement_flag(0.1, 0.26), AgreementFlag::DriveableOver);
        assert_eq!(agreement_flag(0.1, -0.4), AgreementFlag::DriveableOver);
        assert_eq!(agreement_flag(1.2, 1.0), AgreementFlag::WithinCliff);
        assert_eq!(agreement_flag(1.2, 2.0), AgreementFlag::WithinCliff);
        assert_eq!(agreement_flag(1.2, 2.1), AgreementFlag::CliffOver);
        assert_eq!(agreement_flag(1.2, -3.0), AgreementFlag::CliffOver);
    }

    #[test]
    fn stated_gpu_budgets_name_the_three_targets() {
        let rows = do27_gpu_budgets();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].gpu, "RTX 5090");
        assert_eq!(rows[1].gpu, "RX 7900 XTX");
        assert_eq!(rows[2].gpu, "Steam Deck (Medium)");
        assert!(rows[0].terrain_pass_ms <= 2.5);
        assert!(rows[1].terrain_pass_ms <= 2.5);
        assert!(rows[2].terrain_pass_ms <= 4.0);
        assert_eq!(rows[0].tile_budget_gib, 8.0);
        assert_eq!(rows[1].tile_budget_gib, 6.0);
        assert_eq!(rows[2].tile_budget_gib, 1.0);
        assert_eq!(DO27_PACKAGE_BUDGET_BYTES, 6 * 1024 * 1024 * 1024);
    }
}
