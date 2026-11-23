// SPDX-License-Identifier: MPL-2.0
//! Caustics and underwater sun shafts (ADR-2432): the setting ([`WaterCaustics`]), the CPU
//! reference of the light-ray density the GPU tile pass evaluates, and the tile's CPU mirror.
//!
//! # Method
//!
//! The sun's rays cross a water surface with slope field `s(x)` and bend by Snell's law. Per metre
//! of depth a ray leaves the surface point `x` with a horizontal displacement `r(x)` (the refracted
//! direction's horizontal part over its downward part), so a receiver at depth `d` is lit by the
//! rays of the map `F(x) = x + d·r(x)`. Light flux through the surface is conserved, so the
//! irradiance on the receiver, relative to a flat surface, is the inverse area of the refracted
//! beam's footprint: `1 / |det ∂F/∂x| = 1 / |det(I + d·J)|` with `J = ∂r/∂x`. For a 2×2 `J` the
//! determinant is `1 + d·tr J + d²·det J`, so a tile that holds `tr J`, `det J` and the offset
//! `r − r₀` answers every depth with a few multiplies. That is the Jacobian of the refraction map
//! evaluated analytically at the receiver; [`caustic_density`] is the reference it is checked
//! against, a count of refracted rays landing in each receiver bin (the light-ray density idea
//! of Wallace, Cohen and Greenberg, and Jensen's photon-map density estimate, by counting in
//! bins), which conserves energy by construction.
//!
//! The tile is made on the GPU every frame (`cs_caustic_tile` in `water_body.wgsl`) from the same
//! ripple map the surface uses; [`cpu_tile`] is its CPU mirror.

use serde::{Deserialize, Serialize};

use super::clock::WaterClock;
use super::normal_map::{RIPPLE_MAP_SIZE, RippleLevel, decode};
use super::{WaterBodyError, WaterTier, range};

/// Edge of the caustic tile (texels).
pub const CAUSTIC_TILE_SIZE: u32 = 256;

/// Index of refraction of water, as the shader's `WATER_IOR`.
pub const IOR: f32 = 1.333;

/// Fewest shaft march steps when shafts are on.
pub const MIN_SHAFT_STEPS: u32 = 8;

/// Most shaft march steps.
pub const MAX_SHAFT_STEPS: u32 = 16;

/// Densest the focusing gets before the softened inverse levels off (the caustic line's peak).
pub const MAX_DENSITY: f32 = 8.0;

/// Weight of each ripple layer in the tile (keeps the combined slope's variance).
pub const LAYER_WEIGHT: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Offset of the second layer in tiles, as in the surface shader's second sample.
pub const LAYER_B_OFFSET: [f32; 2] = [0.37, 0.61];

/// Finite-difference half step of the tile pass (texels).
pub const FD_TEXELS: f32 = 2.0;

/// Spread of the three colour channels' focal depths at `chromatic = 1` (a fraction of the depth;
/// water's real dispersion is about 1 %, so 0.07 reads as physical).
pub const CHROMATIC_SPREAD: f32 = 0.15;

/// Caustics and sun shafts of one water body (a field of [`super::WaterLook`]).
///
/// Off by default. The look is not one of the frozen content formats (spec 12 § 4a lists none for
/// water): the field is additive, read with `serde(default)` and left out of a save while it is
/// the default, so existing files and their bytes do not change.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaterCaustics {
    /// Draw caustics on what lies below the surface and shafts under it. `false` costs nothing.
    pub enabled: bool,
    /// How much the focusing shows: 0 none, 1 physical, up to 4.
    pub strength: f32,
    /// Scale of the pattern against the ripple tile: 2 doubles the pattern's size.
    pub scale: f32,
    /// Speed of the pattern's change (1 is the ripples' own); the first enabled body's value
    /// drives the frame's shared tile.
    pub speed: f32,
    /// Spread of the colour channels' focus (0 none, 1 strong dispersion).
    pub chromatic: f32,
    /// Steps of the underwater shaft march: 0 draws no shafts, else 8 to 16.
    pub shaft_steps: u32,
    /// The lowest tier that draws the pass: [`WaterTier::High`] or [`WaterTier::Ultra`]. Low
    /// compiles it out whatever this says.
    pub tier_cost: WaterTier,
    /// Average the shaft march over frames (ADR-3251): the per-frame jitter moves and a temporal
    /// resolve, reprojected and restarted on camera cuts, removes the half-resolution grain.
    /// Off by default (two more half-resolution targets and one more dispatch when on). Runs on
    /// the tiers that draw the shafts.
    pub shaft_accumulate: bool,
    /// Draw this body's shafts with the eye above its surface too (ADR-3251), from where the view
    /// ray enters the water to what it sees below, as a term of the water's in-scatter. Planes
    /// only (their flat level); off by default.
    pub shafts_from_above: bool,
}

impl Default for WaterCaustics {
    fn default() -> Self {
        Self {
            enabled: false,
            strength: 1.0,
            scale: 1.0,
            speed: 1.0,
            chromatic: 0.0,
            shaft_steps: 12,
            tier_cost: WaterTier::High,
            shaft_accumulate: false,
            shafts_from_above: false,
        }
    }
}

impl WaterCaustics {
    /// `true` for the default (off) setting: left out of saves.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// `true` when a frame at `tier` pays for the pass.
    #[must_use]
    pub fn active_at(&self, tier: WaterTier) -> bool {
        self.enabled
            && self.strength > 0.0
            && !matches!(tier, WaterTier::Low)
            && (tier == self.tier_cost || tier == WaterTier::Ultra)
    }

    /// Checks every field is finite and inside its range.
    ///
    /// # Errors
    ///
    /// The first field that is not; [`WaterBodyError::CausticsTier`] for a Low `tier_cost` and
    /// [`WaterBodyError::ShaftSteps`] for a step count outside 0 or 8..=16.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        range("look.caustics.strength", self.strength, 0.0, 4.0)?;
        range("look.caustics.scale", self.scale, 0.1, 10.0)?;
        range("look.caustics.speed", self.speed, 0.0, 8.0)?;
        range("look.caustics.chromatic", self.chromatic, 0.0, 1.0)?;
        if self.shaft_steps != 0 && !(MIN_SHAFT_STEPS..=MAX_SHAFT_STEPS).contains(&self.shaft_steps) {
            return Err(WaterBodyError::ShaftSteps(self.shaft_steps));
        }
        if matches!(self.tier_cost, WaterTier::Low) {
            return Err(WaterBodyError::CausticsTier);
        }
        Ok(())
    }
}

/// The refracted direction (unit, pointing down) of the sun's ray at a surface with height-field
/// slope `slope = (∂h/∂x, ∂h/∂z)`; `to_sun` points toward the sun. Snell's law, air to water.
#[must_use]
pub fn refracted_ray(to_sun: [f32; 3], slope: [f32; 2]) -> [f32; 3] {
    let n = normalize3([-slope[0], 1.0, -slope[1]]);
    let i = normalize3([-to_sun[0], -to_sun[1], -to_sun[2]]);
    let eta = 1.0 / IOR;
    let cos_i = -dot3(n, i);
    let k = 1.0 - eta * eta * (1.0 - cos_i * cos_i);
    if k <= 0.0 {
        // Cannot happen going from air to water; keep the ray moving down.
        return normalize3([i[0], i[1].min(-0.05), i[2]]);
    }
    let a = eta * cos_i - k.sqrt();
    normalize3([
        eta * i[0] + a * n[0],
        eta * i[1] + a * n[1],
        eta * i[2] + a * n[2],
    ])
}

/// Horizontal displacement per metre of depth of the ray [`refracted_ray`] gives.
#[must_use]
pub fn displacement(to_sun: [f32; 3], slope: [f32; 2]) -> [f32; 2] {
    let t = refracted_ray(to_sun, slope);
    let down = (-t[1]).max(0.05);
    [t[0] / down, t[2] / down]
}

/// Relative irradiance from the Jacobian terms: `1 / |1 + d·tr + d²·det|`, softened so the
/// caustic line's peak stays at [`MAX_DENSITY`].
#[must_use]
pub fn density_from_jacobian(trace: f32, det: f32, depth: f32) -> f32 {
    let m = 1.0 + depth * trace + depth * depth * det;
    let floor = 1.0 / MAX_DENSITY;
    // Normalized so an unfocused beam (`m = 1`) is exactly 1.
    (1.0 + floor * floor).sqrt() / (m * m + floor * floor).sqrt()
}

/// Weight on the focusing at `depth` below the surface: fades in over the first centimetres
/// (nothing has focused yet), out with the absorption of the light's way down (`sigma` in 1/m, the
/// water's green channel plus scattering) and out to nothing by 6 m, where the beams have crossed
/// many times and the pattern would only be noise. The density is the ambient (1) where it is 0.
#[must_use]
pub fn depth_fade(depth: f32, sigma: f32, to_sun_y: f32) -> f32 {
    let d = depth.max(0.0);
    let up = smoothstep(0.02, 0.15, d);
    let absorb = (-sigma.max(0.0) * d / to_sun_y.max(0.25)).exp();
    let far = 1.0 - smoothstep(2.0, 6.0, d);
    up * absorb * far
}

/// The gain on the sun's light: 1 (the ambient pattern) blended to `density` by `strength · fade`.
#[must_use]
pub fn caustic_gain(density: f32, strength: f32, fade: f32) -> f32 {
    1.0 + (density - 1.0) * (strength * fade).clamp(0.0, 4.0)
}

/// Density of the sun's light on a receiver `depth` metres below a periodic surface, by counting
/// refracted rays (`slope` is the height field's gradient, periodic with `period`; `to_sun`
/// points toward the sun). `bins × bins` receiver cells over one period get `rays² ` rays each
/// from a regular grid of surface points; a cell's count over the flat-surface count is its
/// density, so the mean is exactly 1 (energy is conserved) and a focused cell is above it.
/// Row-major, `z` rows.
#[must_use]
pub fn caustic_density(
    slope: &dyn Fn([f32; 2]) -> [f32; 2],
    to_sun: [f32; 3],
    depth: f32,
    period: f32,
    bins: usize,
    rays: usize,
) -> Vec<f32> {
    let _span = tracing::info_span!("caustic_density_cpu").entered();
    let bins = bins.max(1);
    let n = bins * rays.max(1);
    let mut counts = vec![0.0f32; bins * bins];
    let wrap = |v: f32| v.rem_euclid(period);
    for iz in 0..n {
        for ix in 0..n {
            let x = [
                (ix as f32 + 0.5) / n as f32 * period,
                (iz as f32 + 0.5) / n as f32 * period,
            ];
            let r = displacement(to_sun, slope(x));
            let p = [wrap(x[0] + depth * r[0]), wrap(x[1] + depth * r[1])];
            let bx = ((p[0] / period * bins as f32) as usize).min(bins - 1);
            let bz = ((p[1] / period * bins as f32) as usize).min(bins - 1);
            counts[bz * bins + bx] += 1.0;
        }
    }
    let per_bin = (n * n) as f32 / (bins * bins) as f32;
    counts.iter().map(|c| c / per_bin).collect()
}

/// The Jacobian estimate of the same density at the receiver point `p`: solves `F(x) = p` by
/// fixed-point iteration (8 steps; exact before the rays cross) and returns
/// [`density_from_jacobian`] of the finite-difference Jacobian at `x`. This is what the shader
/// computes from its tile.
#[must_use]
pub fn jacobian_density_at(
    slope: &dyn Fn([f32; 2]) -> [f32; 2],
    to_sun: [f32; 3],
    depth: f32,
    p: [f32; 2],
    h: f32,
) -> f32 {
    let r = |x: [f32; 2]| displacement(to_sun, slope(x));
    let mut x = p;
    for _ in 0..8 {
        let rx = r(x);
        x = [p[0] - depth * rx[0], p[1] - depth * rx[1]];
    }
    let (trace, det) = jacobian_terms(&r, x, h);
    density_from_jacobian(trace, det, depth)
}

/// `(tr J, det J)` of the displacement field `r` at `x`, by central differences of half step `h`.
#[must_use]
pub fn jacobian_terms(r: &dyn Fn([f32; 2]) -> [f32; 2], x: [f32; 2], h: f32) -> (f32, f32) {
    let rxp = r([x[0] + h, x[1]]);
    let rxm = r([x[0] - h, x[1]]);
    let rzp = r([x[0], x[1] + h]);
    let rzm = r([x[0], x[1] - h]);
    let k = 0.5 / h;
    let (jxx, jzx) = ((rxp[0] - rxm[0]) * k, (rxp[1] - rxm[1]) * k);
    let (jxz, jzz) = ((rzp[0] - rzm[0]) * k, (rzp[1] - rzm[1]) * k);
    (jxx + jzz, jxx * jzz - jxz * jzx)
}

/// What the tile pass needs of a frame: the sun and the two layers' scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileParams {
    /// Unit vector toward the sun.
    pub to_sun: [f32; 3],
    /// Scroll of ripple layer a, in tiles.
    pub scroll_a: [f32; 2],
    /// Scroll of ripple layer b, in tiles.
    pub scroll_b: [f32; 2],
}

/// The tile parameters of a clock tick: the surface shader's scroll rates times `speed`, reduced in
/// `f64` so the pattern is a pure function of the tick (deterministic by frame number).
#[must_use]
pub fn tile_params(clock: &WaterClock, speed: f32, to_sun: [f32; 3]) -> TileParams {
    let seconds = clock.seconds() * f64::from(speed);
    let scroll = |rate: [f64; 2]| {
        [
            (seconds * rate[0]).rem_euclid(1.0) as f32,
            (seconds * rate[1]).rem_euclid(1.0) as f32,
        ]
    };
    TileParams {
        to_sun,
        scroll_a: scroll(super::gpu::SCROLL[0]),
        scroll_b: scroll(super::gpu::SCROLL[1]),
    }
}

/// Bilinear, wrapping read of the slope in a ripple level at tile coordinates `uv`. The tile
/// pass (`ripple_slope` in `water_body.wgsl`) filters the same way by hand, with f32 weights,
/// instead of through the sampler, so its tile does not depend on a driver's filter precision.
#[must_use]
pub fn sample_slope(level: &RippleLevel, uv: [f32; 2]) -> [f32; 2] {
    let n = level.size as i64;
    let fx = uv[0] * n as f32 - 0.5;
    let fz = uv[1] * n as f32 - 0.5;
    let (x0, z0) = (fx.floor(), fz.floor());
    let (tx, tz) = (fx - x0, fz - z0);
    let at = |x: i64, z: i64| {
        let i = ((z.rem_euclid(n) * n + x.rem_euclid(n)) * 4) as usize;
        let d = decode([
            level.texels[i],
            level.texels[i + 1],
            level.texels[i + 2],
            level.texels[i + 3],
        ]);
        [d[0], d[1]]
    };
    let (x0, z0) = (x0 as i64, z0 as i64);
    let (a, b, c, d) = (at(x0, z0), at(x0 + 1, z0), at(x0, z0 + 1), at(x0 + 1, z0 + 1));
    std::array::from_fn(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bottom = c[k] + (d[k] - c[k]) * tx;
        top + (bottom - top) * tz
    })
}

/// The surface slope of the tile's two-layer surface at tile coordinates `uv`.
#[must_use]
pub fn tile_slope(level: &RippleLevel, params: &TileParams, uv: [f32; 2]) -> [f32; 2] {
    let a = sample_slope(level, [uv[0] + params.scroll_a[0], uv[1] + params.scroll_a[1]]);
    let b = sample_slope(
        level,
        [
            uv[0] + params.scroll_b[0] + LAYER_B_OFFSET[0],
            uv[1] + params.scroll_b[1] + LAYER_B_OFFSET[1],
        ],
    );
    [LAYER_WEIGHT * (a[0] + b[0]), LAYER_WEIGHT * (a[1] + b[1])]
}

/// CPU mirror of `cs_caustic_tile`: per texel `(tr J, det J, r_x − r₀_x, r_z − r₀_z)`, `J` taken
/// per tile coordinate (divide by the tile's length in metres for 1/m).
#[must_use]
pub fn cpu_tile(level: &RippleLevel, params: &TileParams) -> Vec<[f32; 4]> {
    let _span = tracing::info_span!("caustic_tile_cpu").entered();
    let n = CAUSTIC_TILE_SIZE as usize;
    let r0 = displacement(params.to_sun, [0.0, 0.0]);
    let r = |uv: [f32; 2]| displacement(params.to_sun, tile_slope(level, params, uv));
    let h = FD_TEXELS / CAUSTIC_TILE_SIZE as f32;
    let mut out = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            let uv = [(x as f32 + 0.5) / n as f32, (y as f32 + 0.5) / n as f32];
            let (trace, det) = jacobian_terms(&r, uv, h);
            let here = r(uv);
            out.push([trace, det, here[0] - r0[0], here[1] - r0[1]]);
        }
    }
    out
}

/// `(mean, max)` relative irradiance of `tile` at `depth_tiles` (depth over the tile's length).
///
/// The tile is indexed by the surface point `x`, but light is measured at the receiver `x + d·r`,
/// so the mean weights each texel's density by the beam's footprint `|1 + d tr + d² det|` (the
/// receiver area it covers): that mean is 1 when the estimate conserves energy, and it drifts from
/// 1 only where the softened peak or a folded beam makes it lose or double count flux. The max is
/// the densest texel, the focus.
#[must_use]
pub fn tile_density_summary(tile: &[[f32; 4]], depth_tiles: f32) -> (f32, f32) {
    if tile.is_empty() {
        return (1.0, 1.0);
    }
    let mut flux = 0.0f64;
    let mut max = 0.0f32;
    for t in tile {
        let d = density_from_jacobian(t[0], t[1], depth_tiles);
        let footprint = (1.0 + depth_tiles * t[0] + depth_tiles * depth_tiles * t[1]).abs();
        flux += f64::from(d * footprint);
        max = max.max(d);
    }
    ((flux / tile.len() as f64) as f32, max)
}

/// Texels per side of [`cpu_tile`] versus the ripple map (they agree, so the pass samples 1:1).
const _: () = assert!(CAUSTIC_TILE_SIZE == RIPPLE_MAP_SIZE);

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = dot3(v, v).sqrt();
    if l > 1.0e-9 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SUN: [f32; 3] = [0.3, 0.9, 0.1];

    fn sine(amplitude: f32, period: f32) -> impl Fn([f32; 2]) -> [f32; 2] {
        move |x: [f32; 2]| {
            let k = TAU / period;
            [
                amplitude * k * (k * x[0]).cos(),
                0.6 * amplitude * k * (k * x[1] + 1.0).cos(),
            ]
        }
    }

    #[test]
    fn flat_surface_has_unit_density_everywhere() {
        let flat = |_: [f32; 2]| [0.0, 0.0];
        for depth in [0.0, 0.3, 2.0, 9.0] {
            let d = caustic_density(&flat, SUN, depth, 1.0, 16, 4);
            assert!(d.iter().all(|v| (*v - 1.0).abs() < 1.0e-6), "depth {depth}");
        }
        assert!((jacobian_density_at(&flat, SUN, 2.0, [0.3, 0.4], 0.01) - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn a_sinusoidal_surface_conserves_energy_and_focuses() {
        let surface = sine(0.02, 1.0);
        let d = caustic_density(&surface, SUN, 3.0, 1.0, 32, 4);
        let mean = d.iter().sum::<f32>() / d.len() as f32;
        assert!((mean - 1.0).abs() < 0.02, "mean {mean}");
        let max = d.iter().copied().fold(0.0, f32::max);
        assert!(max > 1.5, "max {max}");
        assert!(d.iter().copied().fold(f32::MAX, f32::min) < 0.8);
    }

    #[test]
    fn jacobian_estimate_matches_the_ray_count_before_the_rays_cross() {
        let surface = sine(0.02, 1.0);
        let depth = 1.5;
        let bins = 32;
        let counted = caustic_density(&surface, SUN, depth, 1.0, bins, 16);
        let mut worst = 0.0f32;
        let mut sum = 0.0f32;
        for bz in 0..bins {
            for bx in 0..bins {
                let p = [(bx as f32 + 0.5) / bins as f32, (bz as f32 + 0.5) / bins as f32];
                let est = jacobian_density_at(&surface, SUN, depth, p, 0.002);
                let diff = (est - counted[bz * bins + bx]).abs();
                worst = worst.max(diff);
                sum += diff;
            }
        }
        let mean_diff = sum / (bins * bins) as f32;
        assert!(
            worst < 0.25 && mean_diff < 0.04,
            "worst {worst}, mean difference {mean_diff}"
        );
    }

    #[test]
    fn the_pattern_fades_to_the_ambient_with_depth() {
        let sigma = 0.3;
        let mut last = f32::MAX;
        let mut deviation = Vec::new();
        for depth in [0.5, 1.0, 2.0, 4.0, 8.0] {
            let fade = depth_fade(depth, sigma, 0.9);
            assert!(fade <= last + 1.0e-6 || depth < 1.0);
            last = fade;
            deviation.push((caustic_gain(3.0, 1.0, fade) - 1.0).abs());
        }
        assert!(deviation[4] < 1.0e-6, "{deviation:?}");
        assert!(deviation[4] < deviation[1]);
        // Nothing at the surface itself: the light has not focused yet.
        assert!(depth_fade(0.0, sigma, 0.9) < 1.0e-6);
        assert!((caustic_gain(5.0, 0.0, 1.0) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn refraction_obeys_snell() {
        let up = [0.0, 1.0, 0.0];
        let t = refracted_ray(up, [0.0, 0.0]);
        assert!((t[1] + 1.0).abs() < 1.0e-6);
        let to_sun = normalize3([0.5, 0.8, 0.0]);
        let t = refracted_ray(to_sun, [0.0, 0.0]);
        let sin_i = (1.0 - to_sun[1] * to_sun[1]).sqrt();
        let sin_t = (t[0] * t[0] + t[2] * t[2]).sqrt();
        assert!((sin_i - IOR * sin_t).abs() < 1.0e-5);
    }

    #[test]
    fn the_tile_is_deterministic_by_tick_and_the_clock_moves_it() {
        let levels = super::super::normal_map::ripple_levels();
        let clock = WaterClock {
            tick: 90,
            ..WaterClock::default()
        };
        let p1 = tile_params(&clock, 1.0, SUN);
        let p2 = tile_params(&clock, 1.0, SUN);
        assert_eq!(p1, p2);
        assert_eq!(cpu_tile(&levels[0], &p1), cpu_tile(&levels[0], &p2));
        let later = tile_params(&WaterClock { tick: 91, ..clock }, 1.0, SUN);
        assert_ne!(p1, later);
        assert_ne!(cpu_tile(&levels[0], &p1), cpu_tile(&levels[0], &later));
        // Speed 0 freezes the pattern.
        let frozen_a = tile_params(&clock, 0.0, SUN);
        let frozen_b = tile_params(&WaterClock { tick: 500, ..clock }, 0.0, SUN);
        assert_eq!(frozen_a, frozen_b);
    }

    #[test]
    fn the_ripple_tile_focuses_light_and_keeps_its_energy() {
        let levels = super::super::normal_map::ripple_levels();
        let params = tile_params(&WaterClock::default(), 1.0, SUN);
        let tile = cpu_tile(&levels[0], &params);
        // A 1.5 m ripple tile at 0.4 m depth: about 0.27 tiles.
        let (mean, max) = tile_density_summary(&tile, 0.27);
        assert!(max > 1.5, "max {max}");
        assert!((0.9..1.1).contains(&mean), "mean {mean}");
    }

    #[test]
    fn defaults_are_off_and_validate() {
        let d = WaterCaustics::default();
        assert!(!d.enabled && d.is_default());
        d.validate().expect("the default is valid");
        assert!(!d.active_at(WaterTier::High));
        let on = WaterCaustics { enabled: true, ..d };
        assert!(on.active_at(WaterTier::High) && on.active_at(WaterTier::Ultra));
        assert!(!on.active_at(WaterTier::Low));
        let ultra_only = WaterCaustics {
            tier_cost: WaterTier::Ultra,
            ..on
        };
        assert!(!ultra_only.active_at(WaterTier::High) && ultra_only.active_at(WaterTier::Ultra));
        assert_eq!(
            WaterCaustics {
                tier_cost: WaterTier::Low,
                ..on
            }
            .validate(),
            Err(WaterBodyError::CausticsTier)
        );
        assert_eq!(
            WaterCaustics { shaft_steps: 5, ..on }.validate(),
            Err(WaterBodyError::ShaftSteps(5))
        );
        assert!(WaterCaustics { shaft_steps: 0, ..on }.validate().is_ok());
        assert!(WaterCaustics { strength: 9.0, ..on }.validate().is_err());
        assert!(
            WaterCaustics {
                chromatic: f32::NAN,
                ..on
            }
            .validate()
            .is_err()
        );
    }
}
