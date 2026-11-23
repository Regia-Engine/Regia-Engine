// SPDX-License-Identifier: MPL-2.0
//! Valley mist: a noise-modulated local fog whose density follows the terrain.
//!
//! The froxel fog (`volumetrics.rs`) holds height fog and boxes and spheres of extra
//! density. None of those follow ground that rises and falls. [`ValleyMist`] adds a medium
//! whose density is a function of the height *above the terrain* under the sample, so it
//! lies on slopes, thickens and deepens in hollows (where the ground sits below its
//! surroundings) and thins out on open ground and ridges. A drifting value-noise field breaks
//! it into banks. Everything is a pure function of the sample position, the terrain grid and
//! [`ValleyMist::time_s`]: the same frame number gives the same mist, on every run.
//!
//! The terrain reaches the shader as a small `Rg32Float` grid ([`MistHeightGrid`], height and
//! concavity per texel), so the mist reads the ground without a dependency on the terrain
//! crate. `regia_terrain::mist_grid_from_map` builds one from a loaded terrain.
//!
//! The pass it extends is the existing froxel fog: [`ValleyMist::density_at`] is added to the
//! extinction there (`extinction_at` in `volumetric_fog.wgsl`), and
//! [`ValleyMist::sun_transmittance`] dims the sun's in-scatter by the mist between the sample
//! and the sun, so a low sun lights the bank tops and leaves the hollows blue. The sun's
//! cascade shadow already carves ridge shadows out of the same froxels: those are the shafts.
//! `crates/regia_render/shaders/mist.wgsl` mirrors this file line for line.
//!
//! Tiers: [`MistQuality::Full`] runs three noise octaves and a short march toward the sun.
//! [`MistQuality::Reduced`] (profiles without `volumetric_fog`: Low, Deck, Mobile) runs one
//! octave and no sun march; the froxel grid is 40×24×16 either way.
//!
//! Sources: value noise, Perlin (1985) and the classic lattice-hash form; fractional
//! Brownian motion, Musgrave in "Texturing and Modeling" (Ebert et al.). Terrain-relative
//! exponential height falloff is the same closed form as the height fog of Wenzel,
//! "Real-time Atmospheric Effects in Games" (GDC 2006). The summed-area table for the
//! concavity bake is Crow, "Summed-area tables for texture mapping", SIGGRAPH 1984.
//! Original code. See `docs/provenance.md`.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use regia_app::prelude::*;

/// The largest grid side (texels) [`MistHeightGrid`] accepts.
pub const MIST_GRID_MAX_SIDE: u32 = 2048;

/// The most sun-march steps [`ValleyMist::sun_steps`] uses.
pub const MIST_MAX_SUN_STEPS: u32 = 8;

/// How much of the mist model a frame evaluates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MistQuality {
    /// No mist.
    #[default]
    Off,
    /// One noise octave, no march toward the sun (Low, Deck and Mobile).
    Reduced,
    /// Three noise octaves and [`ValleyMist::sun_steps`] sun-march steps.
    Full,
}

impl MistQuality {
    /// Noise octaves this tier evaluates.
    #[must_use]
    pub fn octaves(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Reduced => 1,
            Self::Full => 3,
        }
    }

    /// The value the shader reads (`0` off, `1` reduced, `2` full).
    #[must_use]
    pub fn gpu(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Reduced => 1.0,
            Self::Full => 2.0,
        }
    }
}

/// Terrain heights and concavity on a regular grid, for [`ValleyMist`].
///
/// Texel `(i, j)` sits at world `(origin.x + i·cell, origin.z + j·cell)` and holds the ground
/// height (metres) and the concavity: how far (metres, at least zero) the ground lies below
/// the mean of the ground within the bake radius. A bowl has large concavity, a ridge zero.
#[derive(Clone)]
pub struct MistHeightGrid {
    origin: [f32; 2],
    cell_m: f32,
    width: u32,
    depth: u32,
    texels: Vec<[f32; 2]>,
    key: u64,
}

impl std::fmt::Debug for MistHeightGrid {
    // Short on purpose: the renderer hashes the outdoor parameters' Debug text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MistHeightGrid({}x{}, {:016x})",
            self.width, self.depth, self.key
        )
    }
}

impl PartialEq for MistHeightGrid {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.width == other.width && self.depth == other.depth
    }
}

impl MistHeightGrid {
    /// Samples `height(x, z)` (metres) at `width × depth` texels `cell_m` apart starting at
    /// `origin` (world X, Z), and bakes the concavity over a window of `concavity_radius_m`.
    ///
    /// `None` when a side is below 2 or above [`MIST_GRID_MAX_SIDE`], or `cell_m` is not
    /// positive and finite. Non-finite heights read as zero. A non-finite
    /// `concavity_radius_m` bakes zero concavity; a radius past the grid edge reads as
    /// the whole-grid mean.
    #[must_use]
    pub fn from_fn(
        origin: [f32; 2],
        cell_m: f32,
        width: u32,
        depth: u32,
        concavity_radius_m: f32,
        height: impl Fn(f32, f32) -> f32,
    ) -> Option<Self> {
        let sides = 2..=MIST_GRID_MAX_SIDE;
        if !(sides.contains(&width) && sides.contains(&depth) && cell_m.is_finite() && cell_m > 0.0) {
            return None;
        }
        let (w, d) = (width as usize, depth as usize);
        let mut heights = Vec::with_capacity(w * d);
        for j in 0..d {
            for i in 0..w {
                let h = height(origin[0] + i as f32 * cell_m, origin[1] + j as f32 * cell_m);
                heights.push(if h.is_finite() { h } else { 0.0 });
            }
        }
        // A non-finite radius bakes no concavity; a radius past the grid edge is
        // the whole-grid mean anyway (the window is clipped to the grid), and the
        // cap keeps `box_mean`'s index arithmetic from overflowing a huge window.
        let radius_m = if concavity_radius_m.is_finite() {
            concavity_radius_m.max(0.0)
        } else {
            0.0
        };
        let radius = ((radius_m / cell_m).ceil() as usize).min(w.max(d));
        let mean = box_mean(&heights, w, d, radius);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let texels: Vec<[f32; 2]> = heights
            .iter()
            .zip(&mean)
            .map(|(&h, &m)| [h, (m - h).max(0.0)])
            .collect();
        (
            width,
            depth,
            origin[0].to_bits(),
            origin[1].to_bits(),
            cell_m.to_bits(),
        )
            .hash(&mut hasher);
        for t in &texels {
            t[0].to_bits().hash(&mut hasher);
            t[1].to_bits().hash(&mut hasher);
        }
        Some(Self {
            origin,
            cell_m,
            width,
            depth,
            texels,
            key: hasher.finish(),
        })
    }

    /// Texels along world X.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Texels along world Z.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// World (X, Z) of texel `(0, 0)`.
    #[must_use]
    pub fn origin(&self) -> [f32; 2] {
        self.origin
    }

    /// Distance between texels (metres).
    #[must_use]
    pub fn cell_m(&self) -> f32 {
        self.cell_m
    }

    /// Content hash (the renderer re-uploads when it changes).
    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Row-major `(height, concavity)` texels, `index = j · width + i`.
    #[must_use]
    pub fn texels(&self) -> &[[f32; 2]] {
        &self.texels
    }

    /// Bilinear `(height, concavity)` at world (x, z), clamped to the border. The shader's
    /// `mist_ground` does the same.
    #[must_use]
    pub fn sample(&self, x: f32, z: f32) -> [f32; 2] {
        let u = ((x - self.origin[0]) / self.cell_m).clamp(0.0, (self.width - 1) as f32);
        let v = ((z - self.origin[1]) / self.cell_m).clamp(0.0, (self.depth - 1) as f32);
        let (i0, j0) = (u.floor() as usize, v.floor() as usize);
        let (i1, j1) = (
            (i0 + 1).min(self.width as usize - 1),
            (j0 + 1).min(self.depth as usize - 1),
        );
        let (tx, tz) = (u - i0 as f32, v - j0 as f32);
        let at = |i: usize, j: usize| self.texels[j * self.width as usize + i];
        let (a, b, c, d) = (at(i0, j0), at(i1, j0), at(i0, j1), at(i1, j1));
        std::array::from_fn(|k| {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bottom = c[k] + (d[k] - c[k]) * tx;
            top + (bottom - top) * tz
        })
    }
}

/// Mean over the `(2r + 1)²` window around every cell, window clipped at the border
/// (summed-area table in `f64`, so the result does not depend on the summing order).
fn box_mean(values: &[f32], w: usize, d: usize, radius: usize) -> Vec<f32> {
    let stride = w + 1;
    let mut sat = vec![0.0_f64; stride * (d + 1)];
    for j in 0..d {
        let mut row = 0.0_f64;
        for i in 0..w {
            row += f64::from(values[j * w + i]);
            sat[(j + 1) * stride + i + 1] = sat[j * stride + i + 1] + row;
        }
    }
    let mut out = Vec::with_capacity(w * d);
    for j in 0..d {
        let (j0, j1) = (
            j.saturating_sub(radius),
            j.saturating_add(radius).saturating_add(1).min(d),
        );
        for i in 0..w {
            let (i0, i1) = (
                i.saturating_sub(radius),
                i.saturating_add(radius).saturating_add(1).min(w),
            );
            let sum =
                sat[j1 * stride + i1] - sat[j0 * stride + i1] - sat[j1 * stride + i0] + sat[j0 * stride + i0];
            out.push((sum / ((i1 - i0) * (j1 - j0)) as f64) as f32);
        }
    }
    out
}

/// Valley mist settings. Insert as a resource; the renderer adds the mist to the froxel fog.
///
/// Build it with [`ValleyMist::default`] and change fields: more may be added.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ValleyMist {
    /// Extinction (1/metres) at the ground of a full hollow, before the noise.
    pub density: f32,
    /// Height above the ground (metres) over which the density falls by `1/e`, in a full hollow.
    pub scale_height_m: f32,
    /// Height above the ground (metres) where the mist has ended, in a full hollow.
    pub max_height_m: f32,
    /// Fraction (`0..1`) of the hollow density that open ground and ridges keep.
    pub open_ground_fraction: f32,
    /// Concavity (metres, [`MistHeightGrid`]) at which ground counts as a full hollow.
    pub hollow_depth_m: f32,
    /// Size of the biggest noise feature (metres).
    pub noise_scale_m: f32,
    /// How far the noise moves the density, `0..1` (`0` is smooth, `1` leaves gaps).
    pub noise_strength: f32,
    /// Drift of the noise (metres per second, world X and Z).
    pub wind_m_per_s: [f32; 2],
    /// Seconds of mist time. The host sets it from the frame number (`frame / fps`) so a
    /// frame renders the same on every run.
    pub time_s: f32,
    /// Steps of the march toward the sun at [`MistQuality::Full`] (`0` skips it).
    pub sun_steps: u32,
    /// Draw a [`MistQuality::Reduced`] mist on profiles without `volumetric_fog` (Low,
    /// Deck, Mobile). Off leaves those tiers without mist.
    pub reduced_on_low_tiers: bool,
    /// World Y of the ground when [`Self::grid`] is `None`.
    pub flat_ground_y: f32,
    /// The terrain, or `None` for flat ground at [`Self::flat_ground_y`].
    pub grid: Option<Arc<MistHeightGrid>>,
}

impl Default for ValleyMist {
    fn default() -> Self {
        Self {
            density: 0.012,
            scale_height_m: 14.0,
            max_height_m: 45.0,
            open_ground_fraction: 0.15,
            hollow_depth_m: 6.0,
            noise_scale_m: 160.0,
            noise_strength: 0.6,
            wind_m_per_s: [0.6, 0.25],
            time_s: 0.0,
            sun_steps: 4,
            reduced_on_low_tiers: true,
            flat_ground_y: 0.0,
            grid: None,
        }
    }
}

impl ValleyMist {
    /// Whether the mist adds any extinction at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.density > 0.0 && self.max_height_m > 0.0 && self.scale_height_m > 0.0
    }

    /// Mist time for `frame` at `fps` frames per second: the deterministic clock for
    /// [`Self::time_s`].
    #[must_use]
    pub fn time_for_frame(frame: u64, fps: f32) -> f32 {
        frame as f32 / fps.max(1.0e-3)
    }

    /// Ground height and concavity under world (x, z).
    #[must_use]
    pub fn ground(&self, x: f32, z: f32) -> [f32; 2] {
        self.grid
            .as_ref()
            .map_or([self.flat_ground_y, 0.0], |grid| grid.sample(x, z))
    }

    /// How much of a full hollow the ground under (x, z) is, `0..1`.
    #[must_use]
    pub fn hollow_at(&self, x: f32, z: f32) -> f32 {
        smoothstep(0.0, self.hollow_depth_m.max(1.0e-3), self.ground(x, z)[1])
    }

    /// Mist extinction (1/metres) at `point`.
    #[must_use]
    pub fn density_at(&self, point: [f32; 3], quality: MistQuality) -> f32 {
        if quality == MistQuality::Off || !self.is_active() {
            return 0.0;
        }
        let [ground, concavity] = self.ground(point[0], point[2]);
        let above = (point[1] - ground).max(0.0);
        let hollow = smoothstep(0.0, self.hollow_depth_m.max(1.0e-3), concavity);
        let open = self.open_ground_fraction.clamp(0.0, 1.0);
        let pool = open + (1.0 - open) * hollow;
        let depth_scale = 0.5 + 0.5 * hollow;
        let top = (self.max_height_m * depth_scale).max(1.0e-3);
        let profile = (-above / (self.scale_height_m * depth_scale).max(1.0e-3)).exp()
            * (1.0 - smoothstep(0.6 * top, top, above));
        (self.density * pool * profile * self.noise_factor(point, quality)).max(0.0)
    }

    /// The multiplier the drifting noise puts on the density, around 1.
    #[must_use]
    pub fn noise_factor(&self, point: [f32; 3], quality: MistQuality) -> f32 {
        let strength = self.noise_strength.clamp(0.0, 1.0);
        if strength <= 0.0 || quality == MistQuality::Off {
            return 1.0;
        }
        let n = mist_fbm(
            [
                (point[0] - self.wind_m_per_s[0] * self.time_s) / self.noise_scale_m.max(1.0),
                point[1] / (self.noise_scale_m.max(1.0) * 0.5),
                (point[2] - self.wind_m_per_s[1] * self.time_s) / self.noise_scale_m.max(1.0),
            ],
            quality.octaves(),
            self.time_s / self.noise_scale_m.max(1.0),
        );
        ((1.0 - strength) + strength * 2.0 * n).max(0.0)
    }

    /// Fraction of the sun that reaches `point` through the mist, `to_sun` a unit vector.
    /// 1 below [`MistQuality::Full`] or when [`Self::sun_steps`] is zero.
    #[must_use]
    pub fn sun_transmittance(&self, point: [f32; 3], to_sun: [f32; 3], quality: MistQuality) -> f32 {
        let steps = self.sun_steps.min(MIST_MAX_SUN_STEPS);
        if quality != MistQuality::Full || steps == 0 || !self.is_active() {
            return 1.0;
        }
        let ground = self.ground(point[0], point[2])[0];
        let reach =
            ((self.max_height_m - (point[1] - ground).max(0.0)) / to_sun[1].max(0.05)).clamp(8.0, 800.0);
        let dt = reach / steps as f32;
        let mut optical = 0.0;
        for k in 0..steps {
            let t = (k as f32 + 0.5) * dt;
            let q = [
                point[0] + to_sun[0] * t,
                point[1] + to_sun[1] * t,
                point[2] + to_sun[2] * t,
            ];
            optical += self.density_at(q, quality) * dt;
        }
        (-optical).exp()
    }

    /// The four rows of the fog uniform (`mist` in `volumetric_fog.wgsl`).
    ///
    /// `quality` of [`MistQuality::Off`] gives all zeros. Row 3 holds the grid placement; its
    /// `z` is `1 / cell` and is zero when no grid is bound.
    #[must_use]
    pub fn gpu_rows(&self, quality: MistQuality) -> [[f32; 4]; 4] {
        if quality == MistQuality::Off || !self.is_active() {
            return [[0.0; 4]; 4];
        }
        let (origin, inv_cell) = self
            .grid
            .as_ref()
            .map_or(([0.0, 0.0], 0.0), |grid| (grid.origin(), 1.0 / grid.cell_m()));
        [
            [
                self.density,
                self.scale_height_m,
                self.max_height_m,
                self.open_ground_fraction.clamp(0.0, 1.0),
            ],
            [
                self.hollow_depth_m.max(1.0e-3),
                self.noise_scale_m.max(1.0),
                self.noise_strength.clamp(0.0, 1.0),
                self.time_s,
            ],
            [
                self.wind_m_per_s[0],
                self.wind_m_per_s[1],
                self.sun_steps.min(MIST_MAX_SUN_STEPS) as f32,
                quality.gpu(),
            ],
            [origin[0], origin[1], inv_cell, self.flat_ground_y],
        ]
    }
}

/// The quality tier a frame draws the mist at: [`MistQuality::Full`] with `volumetric_fog`,
/// else [`MistQuality::Reduced`] when [`ValleyMist::reduced_on_low_tiers`], else off.
#[must_use]
pub fn mist_quality(mist: &ValleyMist, volumetric_fog: bool) -> MistQuality {
    if !mist.is_active() {
        MistQuality::Off
    } else if volumetric_fog {
        MistQuality::Full
    } else if mist.reduced_on_low_tiers {
        MistQuality::Reduced
    } else {
        MistQuality::Off
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Lattice hash to `[0, 1)`. Integer arithmetic only, so the shader's copy agrees exactly.
fn lattice(x: i32, y: i32, z: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (y as u32).wrapping_mul(0xd816_3841)
        ^ (z as u32).wrapping_mul(0xcb1a_b31f);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h >> 8) as f32 / 16_777_216.0
}

/// Smooth value noise at `q`, `0..1`.
#[must_use]
pub fn mist_noise(q: [f32; 3]) -> f32 {
    let f = q.map(f32::floor);
    let t = [q[0] - f[0], q[1] - f[1], q[2] - f[2]];
    let s = t.map(|v| v * v * (3.0 - 2.0 * v));
    let (ix, iy, iz) = (f[0] as i32, f[1] as i32, f[2] as i32);
    let lerp = |a: f32, b: f32, k: f32| a + (b - a) * k;
    let layer = |dz: i32| {
        let x0 = lerp(lattice(ix, iy, iz + dz), lattice(ix + 1, iy, iz + dz), s[0]);
        let x1 = lerp(
            lattice(ix, iy + 1, iz + dz),
            lattice(ix + 1, iy + 1, iz + dz),
            s[0],
        );
        lerp(x0, x1, s[1])
    };
    lerp(layer(0), layer(1), s[2])
}

/// Fractional Brownian motion of [`mist_noise`]: octave `k` doubles the frequency, takes
/// weight `0.57, 0.29, 0.14` (normalized over the octaves used) and is shifted by `shift`
/// times `0.35·k + 1`, so the octaves slide past each other and the banks evolve.
#[must_use]
pub fn mist_fbm(q: [f32; 3], octaves: u32, shift: f32) -> f32 {
    const WEIGHTS: [f32; 3] = [0.57, 0.29, 0.14];
    let used = octaves.clamp(1, 3) as usize;
    let mut sum = 0.0;
    let mut total = 0.0;
    let mut freq = 1.0;
    for (k, weight) in WEIGHTS.iter().take(used).enumerate() {
        let slide = shift * (1.0 + 0.35 * k as f32);
        sum += weight * mist_noise([q[0] * freq + slide, q[1] * freq, q[2] * freq - slide]);
        total += weight;
        freq *= 2.0;
    }
    sum / total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bowl_grid() -> Arc<MistHeightGrid> {
        // A 2 km square: a 40 m deep bowl in the middle of a plateau.
        let grid = MistHeightGrid::from_fn([-1000.0, -1000.0], 20.0, 101, 101, 300.0, |x, z| {
            let r = (x * x + z * z).sqrt();
            40.0 * (r / 400.0).min(1.0).powi(2)
        });
        Arc::new(grid.expect("bowl grid"))
    }

    fn bowl_mist() -> ValleyMist {
        ValleyMist {
            noise_strength: 0.0,
            grid: Some(bowl_grid()),
            ..ValleyMist::default()
        }
    }

    #[test]
    fn noise_is_in_range_and_deterministic() {
        for k in 0..200 {
            let q = [k as f32 * 0.37 - 20.0, k as f32 * 0.11, 5.0 - k as f32 * 0.29];
            let n = mist_noise(q);
            assert!((0.0..=1.0).contains(&n), "noise {n}");
            assert_eq!(n.to_bits(), mist_noise(q).to_bits());
            let f = mist_fbm(q, 3, 1.5);
            assert!((0.0..=1.0).contains(&f), "fbm {f}");
        }
    }

    #[test]
    fn noise_is_continuous_across_lattice_cells() {
        for k in 0..50 {
            let q = [k as f32 + 0.999_99, 3.5, -2.0];
            let step = [q[0] + 0.000_02, q[1], q[2]];
            assert!((mist_noise(q) - mist_noise(step)).abs() < 1.0e-3);
        }
    }

    #[test]
    fn a_hollow_holds_more_mist_than_open_ground() {
        let mist = bowl_mist();
        let hollow = mist.density_at([0.0, 2.0, 0.0], MistQuality::Full);
        let plateau = mist.density_at([800.0, 42.0, 0.0], MistQuality::Full);
        assert!(hollow > 0.0 && plateau > 0.0);
        assert!(hollow > 3.0 * plateau, "hollow {hollow} plateau {plateau}");
        assert!(mist.hollow_at(0.0, 0.0) > 0.9);
        assert!(mist.hollow_at(800.0, 0.0) < 0.05);
    }

    #[test]
    fn density_is_relative_to_the_terrain_not_to_world_height() {
        let mist = bowl_mist();
        // The same height above two different ground heights on the plateau gives the same mist.
        let a = mist.density_at([800.0, 40.0 + 5.0, 0.0], MistQuality::Full);
        let b = mist.density_at([-800.0, 40.0 + 5.0, 300.0], MistQuality::Full);
        assert!((a - b).abs() < 1.0e-6);
        // Five metres over the bowl floor is mist; the same world height over the rim is 40 m
        // lower in the bowl's wall: the ground there is higher, so the sample is under it.
        assert!(mist.density_at([0.0, 5.0, 0.0], MistQuality::Full) > 0.0);
    }

    #[test]
    fn mist_thins_with_height_and_ends_at_the_ceiling() {
        let mist = bowl_mist();
        let mut last = f32::MAX;
        for h in [0.0, 5.0, 10.0, 20.0, 30.0] {
            let d = mist.density_at([0.0, h, 0.0], MistQuality::Full);
            assert!(d < last, "height {h}: {d} !< {last}");
            last = d;
        }
        assert_eq!(
            mist.density_at([0.0, mist.max_height_m + 1.0, 0.0], MistQuality::Full),
            0.0
        );
    }

    #[test]
    fn flat_ground_without_a_grid_uses_the_flat_height() {
        let mist = ValleyMist {
            noise_strength: 0.0,
            flat_ground_y: 100.0,
            ..ValleyMist::default()
        };
        let at_ground = mist.density_at([3.0, 100.0, 4.0], MistQuality::Full);
        assert!((at_ground - mist.density * mist.open_ground_fraction).abs() < 1.0e-6);
        assert!(mist.density_at([3.0, 130.0, 4.0], MistQuality::Full) < at_ground);
    }

    #[test]
    fn drift_moves_the_noise_and_the_same_time_repeats_exactly() {
        let still = ValleyMist {
            grid: None,
            ..ValleyMist::default()
        };
        let moved = ValleyMist {
            time_s: 120.0,
            ..still.clone()
        };
        let again = ValleyMist {
            time_s: 120.0,
            ..still.clone()
        };
        let p = [40.0, 3.0, -25.0];
        assert_ne!(
            still.density_at(p, MistQuality::Full).to_bits(),
            moved.density_at(p, MistQuality::Full).to_bits()
        );
        assert_eq!(
            moved.density_at(p, MistQuality::Full).to_bits(),
            again.density_at(p, MistQuality::Full).to_bits()
        );
    }

    #[test]
    fn the_mist_clock_is_the_frame_number() {
        assert_eq!(ValleyMist::time_for_frame(0, 60.0), 0.0);
        assert!((ValleyMist::time_for_frame(90, 60.0) - 1.5).abs() < 1.0e-6);
    }

    #[test]
    fn noise_banks_vary_the_density_around_its_smooth_value() {
        let smooth = ValleyMist {
            noise_strength: 0.0,
            ..ValleyMist::default()
        };
        let noisy = ValleyMist::default();
        let (mut lo, mut hi, mut sum) = (f32::MAX, 0.0_f32, 0.0);
        let count = 400;
        for k in 0..count {
            let p = [k as f32 * 13.0, 2.0, (k * 7 % 31) as f32 * 40.0];
            let ratio = noisy.density_at(p, MistQuality::Full) / smooth.density_at(p, MistQuality::Full);
            lo = lo.min(ratio);
            hi = hi.max(ratio);
            sum += ratio;
        }
        assert!(hi - lo > 0.5, "banks too uniform: {lo}..{hi}");
        let mean = sum / count as f32;
        assert!((0.8..1.2).contains(&mean), "noise biases the mean: {mean}");
    }

    #[test]
    fn a_low_sun_crosses_more_mist_than_a_high_one() {
        let mist = bowl_mist();
        let p = [0.0, 3.0, 0.0];
        let dir = |deg: f32| {
            let e = deg.to_radians();
            [e.cos(), e.sin(), 0.0]
        };
        let low = mist.sun_transmittance(p, dir(10.0), MistQuality::Full);
        let high = mist.sun_transmittance(p, dir(60.0), MistQuality::Full);
        assert!(low < high, "low {low} high {high}");
        assert!(low < 1.0 && high <= 1.0);
        assert_eq!(mist.sun_transmittance(p, dir(10.0), MistQuality::Reduced), 1.0);
        let off = ValleyMist {
            sun_steps: 0,
            ..bowl_mist()
        };
        assert_eq!(off.sun_transmittance(p, dir(10.0), MistQuality::Full), 1.0);
    }

    #[test]
    fn reduced_quality_runs_one_octave_and_off_runs_none() {
        let mist = bowl_mist();
        let noisy = ValleyMist {
            noise_strength: 0.8,
            ..mist
        };
        let p = [30.0, 2.0, 60.0];
        assert_ne!(
            noisy.density_at(p, MistQuality::Full).to_bits(),
            noisy.density_at(p, MistQuality::Reduced).to_bits()
        );
        assert_eq!(noisy.density_at(p, MistQuality::Off), 0.0);
        assert_eq!(MistQuality::Reduced.octaves(), 1);
        assert_eq!(MistQuality::Full.octaves(), 3);
    }

    #[test]
    fn the_tier_follows_volumetric_fog_and_the_fallback_switch() {
        let mist = ValleyMist::default();
        assert_eq!(mist_quality(&mist, true), MistQuality::Full);
        assert_eq!(mist_quality(&mist, false), MistQuality::Reduced);
        let strict = ValleyMist {
            reduced_on_low_tiers: false,
            ..ValleyMist::default()
        };
        assert_eq!(mist_quality(&strict, false), MistQuality::Off);
        let none = ValleyMist {
            density: 0.0,
            ..ValleyMist::default()
        };
        assert_eq!(mist_quality(&none, true), MistQuality::Off);
    }

    #[test]
    fn the_bake_marks_a_bowl_concave_and_a_ridge_flat() {
        let grid = bowl_grid();
        assert!(grid.sample(0.0, 0.0)[1] > 10.0);
        assert!(grid.sample(900.0, 900.0)[1] < 1.0);
        let ridge = MistHeightGrid::from_fn([0.0, 0.0], 10.0, 64, 64, 100.0, |x, _| {
            50.0 - (x - 320.0).abs() * 0.1
        })
        .expect("ridge");
        assert!(ridge.sample(320.0, 300.0)[1] < 0.5);
    }

    #[test]
    fn grid_sampling_is_bilinear_and_clamped() {
        let grid = MistHeightGrid::from_fn([0.0, 0.0], 10.0, 3, 3, 0.0, |x, _| x).expect("ramp");
        assert!((grid.sample(5.0, 0.0)[0] - 5.0).abs() < 1.0e-5);
        assert!((grid.sample(-50.0, 0.0)[0]).abs() < 1.0e-5);
        assert!((grid.sample(500.0, 7.0)[0] - 20.0).abs() < 1.0e-5);
        assert_eq!(grid.texels().len(), 9);
    }

    #[test]
    fn a_non_finite_or_absurd_concavity_radius_does_not_overflow_the_window() {
        // INFINITY used to saturate the `as usize` cast and overflow the box
        // window's index arithmetic (a debug panic); a huge finite radius did
        // the same through the saturation. Both must behave like radius 0 /
        // the whole-grid mean.
        let inf = MistHeightGrid::from_fn([0.0, 0.0], 1.0, 4, 4, f32::INFINITY, |_, _| 0.0)
            .expect("infinite radius");
        assert!(inf.texels().iter().all(|t| t[1] == 0.0));
        let nan = MistHeightGrid::from_fn([0.0, 0.0], 1.0, 4, 4, f32::NAN, |_, _| 0.0).expect("nan radius");
        assert!(nan.texels().iter().all(|t| t[1] == 0.0));
        // A radius past the grid edge is the whole-grid mean: heights ramp 0,1,2,3
        // along X, so every texel's concavity is `mean - height`, clamped at 0.
        let huge = MistHeightGrid::from_fn([0.0, 0.0], 1.0, 4, 4, 1e20, |x, _| x).expect("huge radius");
        for j in 0..4 {
            for i in 0..4 {
                assert!(
                    (huge.texels()[j * 4 + i][1] - (1.5 - i as f32).max(0.0)).abs() < 1.0e-6,
                    "texel ({i},{j}): {:?}",
                    huge.texels()[j * 4 + i]
                );
            }
        }
    }

    #[test]
    fn bad_grids_are_refused_and_equal_grids_share_a_key() {
        assert!(MistHeightGrid::from_fn([0.0; 2], 0.0, 4, 4, 0.0, |_, _| 0.0).is_none());
        assert!(MistHeightGrid::from_fn([0.0; 2], f32::NAN, 4, 4, 0.0, |_, _| 0.0).is_none());
        assert!(MistHeightGrid::from_fn([0.0; 2], 1.0, 1, 4, 0.0, |_, _| 0.0).is_none());
        assert!(MistHeightGrid::from_fn([0.0; 2], 1.0, MIST_GRID_MAX_SIDE + 1, 4, 0.0, |_, _| 0.0).is_none());
        let a = MistHeightGrid::from_fn([0.0; 2], 2.0, 8, 8, 4.0, |x, z| x + z).expect("a");
        let b = MistHeightGrid::from_fn([0.0; 2], 2.0, 8, 8, 4.0, |x, z| x + z).expect("b");
        let c = MistHeightGrid::from_fn([0.0; 2], 2.0, 8, 8, 4.0, |x, z| x - z).expect("c");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let nan = MistHeightGrid::from_fn([0.0; 2], 2.0, 4, 4, 0.0, |_, _| f32::NAN).expect("nan");
        assert!(nan.texels().iter().all(|t| t[0] == 0.0));
    }

    #[test]
    fn gpu_rows_pack_the_settings_and_zero_when_off() {
        let mist = bowl_mist();
        let rows = mist.gpu_rows(MistQuality::Full);
        assert_eq!(rows[0][0], mist.density);
        assert_eq!(rows[2][3], 2.0);
        assert_eq!(rows[3][0], -1000.0);
        assert!((rows[3][2] - 1.0 / 20.0).abs() < 1.0e-7);
        assert_eq!(mist.gpu_rows(MistQuality::Off), [[0.0; 4]; 4]);
        let flat = ValleyMist::default().gpu_rows(MistQuality::Reduced);
        assert_eq!(flat[2][3], 1.0);
        assert_eq!(flat[3][2], 0.0);
    }

    #[test]
    fn an_astronomical_concavity_radius_means_the_whole_grid() {
        let h = |x: f32, z: f32| (x * 3.0 + z).sin() * 5.0 + x;
        let big = MistHeightGrid::from_fn([0.0, 0.0], 1.0, 8, 8, 1.0e20, h).expect("grid");
        let whole = MistHeightGrid::from_fn([0.0, 0.0], 1.0, 8, 8, 1000.0, h).expect("grid");
        assert_eq!(big.texels(), whole.texels());
        assert!(big.texels().iter().all(|t| t[1].is_finite() && t[1] >= 0.0));
    }
}
