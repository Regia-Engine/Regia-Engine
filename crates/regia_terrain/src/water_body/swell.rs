// SPDX-License-Identifier: MPL-2.0
//! Swell on a plane water body (ADR-1620): a list of trochoidal waves whose surface is a pure
//! function of the water clock, evaluated here on the CPU and mirrored term for term by
//! `water_body.wgsl` (`swell_offset`, `swell_frame`).
//!
//! # The surface
//!
//! Each wave `i` has a wave number `k = 2π / λ`, a unit travel direction `d`, an amplitude
//! `A = height / 2`, a horizontal amplitude `H = steepness · A` and a phase `φ`. Its angular
//! frequency follows deep-water dispersion, `ω = sqrt(g k)` (Tessendorf 2001 § 3.1). A point that
//! rests at `r` on the still plane moves to
//!
//! ```text
//! P(r, t) = r + Σ ( H d cos θ,  A sin θ )      θ = k d · r − ω t + φ
//! ```
//!
//! (a trochoidal wave: Gerstner 1802; Fournier and Reeves 1986; Finch 2004, GPU Gems ch. 1).
//! The surface stays single-valued while the fold sum `Σ steepness · k · A` is at most 1
//! ([`MAX_FOLD`]); [`Swell::validate`] refuses more.
//!
//! # Determinism and precision
//!
//! The phase `−ω t + φ`, plus `k d · origin` for a chosen origin, is reduced modulo 2π in `f64`
//! ([`Swell::instant`]); only the small `k d · (r − origin)` is formed in `f32`. The GPU gets the
//! phases at the ring grid's origin (the snapped camera) and the CPU queries use the query point
//! as the origin, so neither sees large trigonometric arguments, at any distance and after any
//! number of hours.
//!
//! # Queries
//!
//! - [`SwellInstant::sample`]: offset, tangent frame, normal, orbital velocity and the Jacobian of
//!   the horizontal map at a rest point.
//! - [`SwellInstant::rest_point`]: the rest point under a world `(x, z)`, by a fixed count of
//!   Newton steps ([`INVERSE_ITERATIONS`]; a plain fixed-point step where the Jacobian is nearly
//!   singular), so the height answered is the height of the visible, displaced surface there.
//! - Crest foam ([`crest_foam`]): where the horizontal map compresses (`1 − J` above the onset).

#[cfg(test)]
use std::cell::Cell;

use serde::{Deserialize, Serialize};

use super::{GRAVITY, WaterBodyError, range};

#[cfg(test)]
thread_local! {
    static INSTANT_ALLOCS: Cell<u32> = const { Cell::new(0) };
    static INSTANT_CALLS: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn test_reset_instant_allocs() {
    INSTANT_ALLOCS.with(|c| c.set(0));
    INSTANT_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn test_instant_allocs() -> u32 {
    INSTANT_ALLOCS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn test_instant_calls() -> u32 {
    INSTANT_CALLS.with(|c| c.get())
}

/// Most waves one body carries (and the Ultra tier draws).
pub const MAX_SWELL_WAVES: usize = 32;

/// Most waves the GPU holds for all bodies of a frame (uniform array size in `water_body.wgsl`).
pub const MAX_SWELL_TABLE: usize = 128;

/// Largest fold sum `Σ steepness · k · A`: above it a crest folds over itself.
pub const MAX_FOLD: f32 = 1.0;

/// Fewest cells across the half of the inner square for which the rings snap on their own
/// lattices: below it the shift of a ring would be a large share of its width.
pub const MIN_SNAP_HALF_CELLS: u32 = 8;

/// Newton steps of the horizontal inverse ([`SwellInstant::rest_point`]).
pub const INVERSE_ITERATIONS: u32 = 8;

/// Uniform steps of the ray march over the clipped ray ([`super::WaterBody::raycast`]).
pub const RAY_MARCH_STEPS: u32 = 128;

/// Bisection steps after the march brackets a crossing.
pub const RAY_BISECTIONS: u32 = 24;

/// Longest ray a query follows (m).
pub const MAX_RAY_DISTANCE: f32 = 100_000.0;

/// Width (in `1 − J`) over which crest foam ramps from none to full above the onset.
pub const FOAM_RAMP: f32 = 0.3;

/// Below this Jacobian determinant the inverse takes a fixed-point step instead of a Newton step.
const MIN_NEWTON_DET: f32 = 0.05;

const TAU64: f64 = std::f64::consts::TAU;

fn default_steepness() -> f32 {
    0.5
}

/// One trochoidal wave of a [`Swell`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwellWave {
    /// Crest-to-crest length (m), 0.25..=2000.
    pub wavelength: f32,
    /// Crest-to-trough height (m), 0..=40.
    pub height: f32,
    /// Direction of travel along x and z (any non-zero length; normalized when used).
    pub direction: [f32; 2],
    /// Horizontal amplitude over the vertical one, 0..=1: 0 is a sine wave, 1 the sharpest
    /// trochoid (narrow crests, broad troughs).
    #[serde(default = "default_steepness")]
    pub steepness: f32,
    /// Phase offset (radians).
    #[serde(default)]
    pub phase: f32,
}

impl SwellWave {
    /// Wave number `2π / λ` (radians per meter).
    #[must_use]
    pub fn wave_number(&self) -> f32 {
        std::f32::consts::TAU / self.wavelength.max(1.0e-3)
    }

    /// Angular frequency by deep-water dispersion, `sqrt(g k)` (radians per second).
    #[must_use]
    pub fn angular_frequency(&self) -> f32 {
        (GRAVITY * self.wave_number()).sqrt()
    }

    /// Phase speed `ω / k = sqrt(g λ / 2π)` (m/s).
    #[must_use]
    pub fn speed(&self) -> f32 {
        self.angular_frequency() / self.wave_number()
    }

    /// Period `2π / ω` (s).
    #[must_use]
    pub fn period(&self) -> f32 {
        std::f32::consts::TAU / self.angular_frequency()
    }

    /// Vertical amplitude, half the height (m).
    #[must_use]
    pub fn amplitude(&self) -> f32 {
        0.5 * self.height
    }

    /// This wave's share of the fold sum, `steepness · k · A`.
    #[must_use]
    pub fn fold(&self) -> f32 {
        self.steepness * self.wave_number() * self.amplitude()
    }

    /// The unit direction of travel (`+x` for a zero direction).
    #[must_use]
    pub fn unit_direction(&self) -> [f32; 2] {
        let [x, z] = self.direction;
        let len = (x * x + z * z).sqrt();
        if len.is_finite() && len > 1.0e-6 {
            [x / len, z / len]
        } else {
            [1.0, 0.0]
        }
    }
}

/// Trochoidal swell on a plane body ([`super::WaterShape::Plane`]): the waves plus the crest foam.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Swell {
    /// The waves, largest first: a tier that draws fewer ([`super::WaterTier::swell_waves`]) keeps
    /// the first ones. Queries always use all of them.
    pub waves: Vec<SwellWave>,
    /// Crest foam strength, 0..=4 (0 none).
    pub crest_foam: f32,
    /// Horizontal compression `1 − J` where crest foam starts, 0..=1 (lower foams more). The
    /// default 0.15 is about 1.7 standard deviations of `1 − J` for a many-wave sea, so a few
    /// percent of the surface starts to foam.
    pub foam_onset: f32,
}

impl Default for Swell {
    fn default() -> Self {
        Self {
            waves: Vec::new(),
            crest_foam: 1.0,
            foam_onset: 0.15,
        }
    }
}

impl Swell {
    /// A deterministic open-sea swell: `count` waves (at most [`MAX_SWELL_WAVES`]) around
    /// `wavelength` (from 0.55 to 1.6 times it, geometric) and `direction` (spread ±35°,
    /// alternating sides), whose heights fall with the wavelength and whose root-sum-square height
    /// is `height`. Steepness is shared so the fold sum is 0.6 (or 1 when the waves are too gentle
    /// for that); phases follow the golden angle.
    #[must_use]
    pub fn open_sea(direction: [f32; 2], height: f32, wavelength: f32, count: usize) -> Self {
        let count = count.clamp(1, MAX_SWELL_WAVES);
        let base = SwellWave {
            wavelength,
            height,
            direction,
            steepness: 0.0,
            phase: 0.0,
        }
        .unit_direction();
        let heading = base[1].atan2(base[0]);
        let mut waves: Vec<SwellWave> = (0..count)
            .map(|i| {
                let t = if count == 1 {
                    0.5
                } else {
                    i as f32 / (count - 1) as f32
                };
                // Longest first.
                let lambda = wavelength.max(0.25) * 1.6 * (0.55f32 / 1.6).powf(t);
                let side = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
                let angle = heading + side * 35f32.to_radians() * t;
                SwellWave {
                    wavelength: lambda.clamp(0.25, 2000.0),
                    height: lambda,
                    direction: [angle.cos(), angle.sin()],
                    steepness: 0.0,
                    phase: (i as f32 * 2.399_963).rem_euclid(std::f32::consts::TAU),
                }
            })
            .collect();
        let rss = waves.iter().map(|w| w.height * w.height).sum::<f32>().sqrt();
        let scale = if rss > 0.0 { height.max(0.0) / rss } else { 0.0 };
        for w in &mut waves {
            w.height = (w.height * scale).clamp(0.0, 40.0);
        }
        let k_a: f32 = waves.iter().map(|w| w.wave_number() * w.amplitude()).sum();
        let steepness = if k_a > 0.0 { (0.6 / k_a).min(1.0) } else { 0.0 };
        for w in &mut waves {
            w.steepness = steepness;
        }
        Self {
            waves,
            ..Self::default()
        }
    }

    /// The fold sum `Σ steepness · k · A` (at most [`MAX_FOLD`] when valid).
    #[must_use]
    pub fn fold_sum(&self) -> f32 {
        self.waves.iter().map(SwellWave::fold).sum()
    }

    /// The largest rise (or fall) of the surface from the still level, `Σ A` (m).
    #[must_use]
    pub fn max_rise(&self) -> f32 {
        self.waves.iter().map(SwellWave::amplitude).sum()
    }

    /// The largest horizontal displacement, `Σ H` (m).
    #[must_use]
    pub fn max_shift(&self) -> f32 {
        self.waves.iter().map(|w| w.steepness * w.amplitude()).sum()
    }

    /// Checks every wave and the foam settings.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        if self.waves.len() > MAX_SWELL_WAVES {
            return Err(WaterBodyError::TooManyWaves {
                got: self.waves.len(),
                max: MAX_SWELL_WAVES,
            });
        }
        for w in &self.waves {
            range("swell.wavelength", w.wavelength, 0.25, 2000.0)?;
            range("swell.height", w.height, 0.0, 40.0)?;
            range("swell.steepness", w.steepness, 0.0, 1.0)?;
            range("swell.phase", w.phase, -1.0e4, 1.0e4)?;
            let [x, z] = w.direction;
            if !(x.is_finite() && z.is_finite()) {
                return Err(WaterBodyError::NotFinite("swell.direction"));
            }
            if x * x + z * z < 1.0e-12 {
                return Err(WaterBodyError::NotPositive("swell.direction"));
            }
        }
        range("swell.crest_foam", self.crest_foam, 0.0, 4.0)?;
        range("swell.foam_onset", self.foam_onset, 0.0, 1.0)?;
        let fold = self.fold_sum();
        if fold > MAX_FOLD {
            return Err(WaterBodyError::SwellFolds(fold));
        }
        Ok(())
    }

    /// The first `limit` waves ready to evaluate at `seconds`, with their phases taken at
    /// `origin` (world x and z) and reduced modulo 2π in `f64`.
    #[must_use]
    pub fn instant(&self, seconds: f64, origin: [f64; 2], limit: usize) -> SwellInstant {
        // Terms live in a stack array. Nothing here allocates.
        #[cfg(test)]
        INSTANT_CALLS.with(|c| c.set(c.get().saturating_add(1)));
        let t = if seconds.is_finite() { seconds } else { 0.0 };
        let mut terms = [WaveTerm::default(); MAX_SWELL_WAVES];
        let mut count = 0u8;
        for w in self.waves.iter().take(limit.min(MAX_SWELL_WAVES)) {
            let k = w.wave_number();
            let d = w.unit_direction();
            let omega = w.angular_frequency();
            let a = w.amplitude();
            let phase = (f64::from(k) * (f64::from(d[0]) * origin[0] + f64::from(d[1]) * origin[1])
                - f64::from(omega) * t
                + f64::from(w.phase))
            .rem_euclid(TAU64);
            terms[usize::from(count)] = WaveTerm {
                k: [k * d[0], k * d[1]],
                amplitude: a,
                horizontal: w.steepness * a,
                phase: phase as f32,
                omega,
            };
            count += 1;
        }
        SwellInstant {
            origin,
            terms,
            count,
            foam: self.crest_foam,
            onset: self.foam_onset,
        }
    }
}

/// One wave at one instant: what the GPU table holds ([`SwellInstant`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WaveTerm {
    /// Wave vector `k d` (radians per meter, x and z).
    pub k: [f32; 2],
    /// Vertical amplitude `A` (m).
    pub amplitude: f32,
    /// Horizontal amplitude `H = steepness · A` (m).
    pub horizontal: f32,
    /// Phase at the instant's origin, in 0..2π.
    pub phase: f32,
    /// Angular frequency `ω` (radians per second).
    pub omega: f32,
}

/// A swell frozen at one instant around one origin ([`Swell::instant`]).
///
/// The waves sit in a fixed array ([`MAX_SWELL_WAVES`]). [`Self::terms`] is the live prefix.
/// Unused slots stay zero, so equality ignores them.
#[derive(Debug, Clone, PartialEq)]
pub struct SwellInstant {
    /// World x and z the phases are taken at.
    pub origin: [f64; 2],
    /// Evaluated waves, then zeros. Not part of the public shape; use [`Self::terms`].
    terms: [WaveTerm; MAX_SWELL_WAVES],
    /// How many entries of `terms` are live.
    count: u8,
    /// Crest foam strength ([`Swell::crest_foam`]).
    pub foam: f32,
    /// Crest foam onset ([`Swell::foam_onset`]).
    pub onset: f32,
}

/// The displaced surface at one rest point ([`SwellInstant::sample`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwellSample {
    /// Displacement from the rest point: x, height above the still level, z (m).
    pub offset: [f32; 3],
    /// `∂P/∂x` of the displaced surface.
    pub dpdx: [f32; 3],
    /// `∂P/∂z` of the displaced surface.
    pub dpdz: [f32; 3],
    /// Orbital velocity of the surface particle, `∂P/∂t` (m/s).
    pub velocity: [f32; 3],
    /// Determinant of the horizontal map's Jacobian: 1 on still water, below 1 where the surface
    /// compresses toward a crest, 0 where it would fold.
    pub jacobian: f32,
}

impl SwellSample {
    /// The unit surface normal, `normalize(∂P/∂z × ∂P/∂x)` (pointing up).
    #[must_use]
    pub fn normal(&self) -> [f32; 3] {
        let a = self.dpdz;
        let b = self.dpdx;
        let n = [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len.is_finite() && len > 1.0e-12 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 1.0, 0.0]
        }
    }
}

/// Crest foam coverage, 0..=1, for a Jacobian `jacobian`: `strength · smoothstep(onset,
/// onset + FOAM_RAMP, 1 − J)`, clamped (mirrors `water_body.wgsl` `crest_foam`).
#[must_use]
pub fn crest_foam(jacobian: f32, strength: f32, onset: f32) -> f32 {
    let x = (((1.0 - jacobian) - onset) / FOAM_RAMP).clamp(0.0, 1.0);
    (strength * x * x * (3.0 - 2.0 * x)).clamp(0.0, 1.0)
}

impl SwellInstant {
    /// The waves evaluated, in order.
    #[must_use]
    pub fn terms(&self) -> &[WaveTerm] {
        &self.terms[..usize::from(self.count)]
    }

    /// The surface at the rest point `rest` (world x and z), every wave at full strength.
    #[must_use]
    pub fn sample(&self, rest: [f32; 2]) -> SwellSample {
        self.sample_on(rest, None).0
    }

    /// [`Self::sample`] over water `depth` meters deep (spec 87 § 3): each wave shoaled and
    /// capped by [`super::shore::shoal_scale`] (`None` is deep water, every wave at full
    /// strength). The scale is held constant in the derivatives (a slowly varying bed). Also
    /// returns the largest shoaled-amplitude-over-breaking-cap ratio of the waves (0 in deep
    /// water).
    #[must_use]
    pub fn sample_on(&self, rest: [f32; 2], depth: Option<f32>) -> (SwellSample, f32) {
        let local = [
            (f64::from(rest[0]) - self.origin[0]) as f32,
            (f64::from(rest[1]) - self.origin[1]) as f32,
        ];
        let mut offset = [0.0f32; 3];
        let mut dpdx = [1.0f32, 0.0, 0.0];
        let mut dpdz = [0.0f32, 0.0, 1.0];
        let mut velocity = [0.0f32; 3];
        let mut breaking = 0.0f32;
        for w in self.terms() {
            let k = (w.k[0] * w.k[0] + w.k[1] * w.k[1]).sqrt().max(1.0e-6);
            let d = [w.k[0] / k, w.k[1] / k];
            let (amplitude, horizontal) = match depth {
                None => (w.amplitude, w.horizontal),
                Some(h) => {
                    let (scale, ratio) = super::shore::shoal_scale(k, w.amplitude, h);
                    breaking = breaking.max(ratio);
                    (w.amplitude * scale, w.horizontal * scale)
                }
            };
            let theta = w.k[0] * local[0] + w.k[1] * local[1] + w.phase;
            let (s, c) = theta.sin_cos();
            offset[0] += horizontal * d[0] * c;
            offset[1] += amplitude * s;
            offset[2] += horizontal * d[1] * c;
            // ∂/∂x and ∂/∂z of the offset: θ grows by k_x and k_z.
            for (axis, deriv) in [(0usize, &mut dpdx), (1usize, &mut dpdz)] {
                let kk = w.k[axis];
                deriv[0] -= horizontal * d[0] * kk * s;
                deriv[1] += amplitude * kk * c;
                deriv[2] -= horizontal * d[1] * kk * s;
            }
            // θ falls by ω per second.
            velocity[0] += horizontal * d[0] * w.omega * s;
            velocity[1] -= amplitude * w.omega * c;
            velocity[2] += horizontal * d[1] * w.omega * s;
        }
        (
            SwellSample {
                offset,
                dpdx,
                dpdz,
                velocity,
                jacobian: dpdx[0] * dpdz[2] - dpdx[2] * dpdz[0],
            },
            breaking,
        )
    }

    /// The displacement the drawn grid gives the rest point `rest` on cells of `cell` meters:
    /// each wave faded by [`crate::water::vertex_wave_fade`] (mirrors `water_body.wgsl`
    /// `swell_offset`). `cell = 0` keeps every wave.
    #[must_use]
    pub fn grid_offset(&self, rest: [f32; 2], cell: f32) -> [f32; 3] {
        let local = [
            (f64::from(rest[0]) - self.origin[0]) as f32,
            (f64::from(rest[1]) - self.origin[1]) as f32,
        ];
        self.grid_offset_local(local, cell)
    }

    /// [`Self::grid_offset`] of the rest point `local` meters from the origin (the form the
    /// shader evaluates).
    #[must_use]
    pub fn grid_offset_local(&self, local: [f32; 2], cell: f32) -> [f32; 3] {
        self.grid_offset_local_on(local, cell, None)
    }

    /// [`Self::grid_offset_local`] over water `depth` meters deep: each wave also shoaled and
    /// capped ([`super::shore::shoal_scale`]; `None` is deep water). Mirrors `water_body.wgsl`
    /// `swell_offset` with a shore field.
    #[must_use]
    pub fn grid_offset_local_on(&self, local: [f32; 2], cell: f32, depth: Option<f32>) -> [f32; 3] {
        let mut offset = [0.0f32; 3];
        for w in self.terms() {
            let k = (w.k[0] * w.k[0] + w.k[1] * w.k[1]).sqrt().max(1.0e-6);
            let fade = crate::water::vertex_wave_fade(cell, k);
            let fade = match depth {
                None => fade,
                Some(h) => fade * super::shore::shoal_scale(k, w.amplitude, h).0,
            };
            let theta = w.k[0] * local[0] + w.k[1] * local[1] + w.phase;
            let (s, c) = theta.sin_cos();
            offset[0] += fade * w.horizontal * (w.k[0] / k) * c;
            offset[1] += fade * w.amplitude * s;
            offset[2] += fade * w.horizontal * (w.k[1] / k) * c;
        }
        offset
    }

    /// The rest point whose displaced position lies over world `(x, z)`, and the horizontal miss
    /// (m) left after [`INVERSE_ITERATIONS`] steps.
    #[must_use]
    pub fn rest_point(&self, x: f32, z: f32) -> ([f32; 2], f32) {
        self.rest_point_on(x, z, |_| None)
    }

    /// [`Self::rest_point`] over a bed: `depth(rest)` is the water depth at a rest point (`None`
    /// deep), so the inverse lands on the shoaled surface ([`Self::sample_on`]).
    #[must_use]
    pub fn rest_point_on(&self, x: f32, z: f32, depth: impl Fn([f32; 2]) -> Option<f32>) -> ([f32; 2], f32) {
        rest_point_by(x, z, |r| self.sample_on(r, depth(r)).0)
    }

    /// Crest foam at a sample ([`crest_foam`] with this swell's settings).
    #[must_use]
    pub fn foam_at(&self, sample: &SwellSample) -> f32 {
        crest_foam(sample.jacobian, self.foam, self.onset)
    }
}

/// The rest point under world `(x, z)` of the surface `sample` gives, and the horizontal miss (m)
/// left after [`INVERSE_ITERATIONS`] Newton steps (a fixed-point step where the Jacobian is
/// nearly singular).
#[must_use]
pub fn rest_point_by(x: f32, z: f32, sample: impl Fn([f32; 2]) -> SwellSample) -> ([f32; 2], f32) {
    let mut r = [x, z];
    for _ in 0..INVERSE_ITERATIONS {
        let s = sample(r);
        let f = [r[0] + s.offset[0] - x, r[1] + s.offset[2] - z];
        // Jacobian of r ↦ r + offset(r): columns ∂/∂x and ∂/∂z.
        let (a, b, c, d) = (s.dpdx[0], s.dpdz[0], s.dpdx[2], s.dpdz[2]);
        let det = a * d - b * c;
        let step = if det.abs() >= MIN_NEWTON_DET {
            [(d * f[0] - b * f[1]) / det, (a * f[1] - c * f[0]) / det]
        } else {
            f
        };
        r = [r[0] - step[0], r[1] - step[1]];
    }
    let s = sample(r);
    let miss = ((r[0] + s.offset[0] - x).powi(2) + (r[1] + s.offset[2] - z).powi(2)).sqrt();
    (r, miss)
}

/// Smallest ring-grid cell (m).
pub const MIN_RING_CELL: f32 = 1.0 / 32.0;

/// Largest ring-grid cell (m).
pub const MAX_RING_CELL: f32 = 64.0;

/// Most rings of the grid.
pub const MAX_RINGS: u32 = 20;

/// Screen pixels the finest cell spans at the nadir under the camera.
pub const PIXELS_PER_CELL: f32 = 6.0;

/// Cells from the center to the edge of the inner square at a 60° vertical field of view; a
/// narrower view gets proportionally more (up to the tier's cap).
pub const RING_CELLS_AT_60_DEG: u32 = 24;

/// The camera-centered ring grid a swell plane is drawn on (Losasso and Hoppe 2004): an inner
/// square of `2 half_cells` cells of `cell` meters, then rings whose cells double, each reaching
/// twice as far as the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RingGrid {
    /// Bits of the finest cell size (m), a power of two.
    pub cell_bits: u32,
    /// Cells from the center to the inner square's edge (even).
    pub half_cells: u32,
    /// Rings, the inner square included.
    pub rings: u32,
}

impl RingGrid {
    /// The finest cell (m).
    #[must_use]
    pub fn cell(&self) -> f32 {
        f32::from_bits(self.cell_bits)
    }

    /// Half the inner square's size (m).
    #[must_use]
    pub fn inner_half_size(&self) -> f32 {
        self.cell() * self.half_cells as f32
    }

    /// Half the whole grid's size (m).
    #[must_use]
    pub fn outer_half_size(&self) -> f32 {
        self.inner_half_size() * (1u64 << (self.rings.max(1) - 1)) as f32
    }

    /// The grid for a view: `fov_y` (radians), `height_px` rows, the eye `eye_height` meters above
    /// the water, at most `cap` cells across the inner half square (the tier's), covering `reach`
    /// meters (Chebyshev) from the center.
    ///
    /// The finest cell spans [`PIXELS_PER_CELL`] pixels at the nadir (`2 h tan(fov/2) / rows` per
    /// pixel), rounded down to a power of two so it changes only when the height doubles or
    /// halves; the inner square has [`RING_CELLS_AT_60_DEG`] `· tan(30°) / tan(fov/2)` cells
    /// across its half (a narrower view resolves farther before the cells double).
    #[must_use]
    pub fn for_view(fov_y: f32, height_px: u32, eye_height: f32, cap: u32, reach: f32) -> Self {
        let fov = if fov_y.is_finite() {
            fov_y.clamp(1.0e-3, 3.0)
        } else {
            std::f32::consts::FRAC_PI_3
        };
        let tan_half = (0.5 * fov).tan().max(1.0e-4);
        let rows = height_px.max(1) as f32;
        let h = if eye_height.is_finite() {
            eye_height.abs().max(1.0)
        } else {
            1.0
        };
        let raw = PIXELS_PER_CELL * 2.0 * h * tan_half / rows;
        let cell = 2f32
            .powi(raw.max(MIN_RING_CELL).log2().floor() as i32)
            .clamp(MIN_RING_CELL, MAX_RING_CELL);
        // The small bias keeps 60° itself from rounding up past the base count.
        let want =
            (RING_CELLS_AT_60_DEG as f32 * std::f32::consts::FRAC_PI_6.tan() / tan_half - 1.0e-3).ceil();
        let cap = cap.max(2) & !1;
        let half_cells = ((want as u32).clamp(2, cap) + 1) & !1;
        let half_cells = half_cells.min(cap);
        let reach = if reach.is_finite() { reach.max(0.0) } else { 0.0 };
        let mut rings = 1u32;
        let mut half = cell * half_cells as f32;
        while half < reach && rings < MAX_RINGS {
            half *= 2.0;
            rings += 1;
        }
        Self {
            cell_bits: cell.to_bits(),
            half_cells,
            rings,
        }
    }

    /// Whether the rings snap on their own lattices ([`MIN_SNAP_HALF_CELLS`]).
    #[must_use]
    pub fn snaps_rings(&self) -> bool {
        self.half_cells >= MIN_SNAP_HALF_CELLS && self.rings > 1
    }

    /// The per-ring snap shifts (ADR-2342): entry `j` is the shift of the ring boundary at
    /// `inner_half_size · 2^j` from the grid's center. Boundary `j >= 1` sits on the lattice of
    /// its own ring, the eye snapped down to `2^(j+1)` finest cells, so the far rings move rarely
    /// and by whole lattice steps; the shift is that snapped point minus `origin`. Boundary 0
    /// (the inner square's edge) does not move. All zero when the grid does not snap its rings.
    /// `origin` is the grid's center (the eye snapped to two cells) and `eye` the eye, both
    /// (x, z) in `f64` so far from the world origin nothing is lost.
    #[must_use]
    pub fn ring_shifts(&self, origin: [f64; 2], eye: [f64; 2]) -> Vec<[f32; 2]> {
        let mut shifts = vec![[0.0f32; 2]; self.rings as usize];
        if !self.snaps_rings() {
            return shifts;
        }
        let cell = f64::from(self.cell());
        for (j, shift) in shifts.iter_mut().enumerate().skip(1) {
            let step = cell * (1u64 << (j + 1)) as f64;
            *shift = [
                ((eye[0] / step).floor() * step - origin[0]) as f32,
                ((eye[1] / step).floor() * step - origin[1]) as f32,
            ];
        }
        shifts
    }

    /// The shift of the vertex at `local` (offset from the grid's center) given the boundary
    /// `shifts` ([`Self::ring_shifts`]): zero inside the inner square, then linear in the
    /// Chebyshev distance between the shifts of the ring's two boundaries. Continuous, so ring
    /// seams stay closed, and never folds a ring over itself (the shift changes by less than
    /// `4 / half_cells` of the distance). Mirrors `water_body.wgsl` `ring_shift`.
    #[must_use]
    pub fn ring_shift(&self, shifts: &[[f32; 2]], local: [f32; 2]) -> [f32; 2] {
        let rho = local[0].abs().max(local[1].abs());
        let inner = self.inner_half_size();
        if rho <= inner || self.rings < 2 || shifts.len() < self.rings as usize {
            return [0.0, 0.0];
        }
        let mut upper = inner;
        let mut k = 1usize;
        while k < self.rings as usize - 1 && rho > upper * 2.0 {
            upper *= 2.0;
            k += 1;
        }
        let (lo, hi) = (upper, upper * 2.0);
        let t = ((rho - lo) / (hi - lo)).clamp(0.0, 1.0);
        let (a, b) = (shifts[k - 1], shifts[k]);
        [a[0] * (1.0 - t) + b[0] * t, a[1] * (1.0 - t) + b[1] * t]
    }

    /// The vertex spacing the shader fades waves by at local offset `local` from the center:
    /// `cell · max(1, 2 d / inner_half_size)` with `d` the Chebyshev distance. Continuous, so
    /// rings meet without cracks; conservative (never finer than the grid). Mirrors
    /// `water_body.wgsl` `ring_cell`.
    #[must_use]
    pub fn fade_cell(&self, local: [f32; 2]) -> f32 {
        ring_fade_cell(local, self.cell(), self.inner_half_size())
    }
}

/// [`RingGrid::fade_cell`] from the raw numbers.
#[must_use]
pub fn ring_fade_cell(local: [f32; 2], cell: f32, inner_half: f32) -> f32 {
    let d = local[0].abs().max(local[1].abs());
    cell * (2.0 * d / inner_half.max(1.0e-6)).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swell() -> Swell {
        Swell {
            waves: vec![
                SwellWave {
                    wavelength: 60.0,
                    height: 1.6,
                    direction: [1.0, 0.2],
                    steepness: 0.8,
                    phase: 0.3,
                },
                SwellWave {
                    wavelength: 23.0,
                    height: 0.6,
                    direction: [0.4, 1.0],
                    steepness: 0.6,
                    phase: 1.9,
                },
                SwellWave {
                    wavelength: 9.0,
                    height: 0.2,
                    direction: [-0.3, 1.0],
                    steepness: 0.5,
                    phase: 4.0,
                },
            ],
            ..Swell::default()
        }
    }

    /// The constants the shader repeats are pinned here; `gpu.rs` compares them by text.
    #[test]
    fn constants_are_pinned() {
        assert_eq!(MAX_SWELL_WAVES, 32);
        assert_eq!(MAX_SWELL_TABLE, 128);
        assert_eq!(INVERSE_ITERATIONS, 8);
        assert_eq!(RAY_MARCH_STEPS, 128);
        assert_eq!(RAY_BISECTIONS, 24);
        assert_eq!(FOAM_RAMP, 0.3);
        assert_eq!(MAX_FOLD, 1.0);
    }

    /// Deep-water dispersion: a 100 m wave runs at about 12.5 m/s with an 8 s period.
    #[test]
    fn dispersion_is_deep_water() {
        let w = SwellWave {
            wavelength: 100.0,
            height: 1.0,
            direction: [1.0, 0.0],
            steepness: 0.5,
            phase: 0.0,
        };
        assert!((w.speed() - 12.49).abs() < 0.02, "{}", w.speed());
        assert!((w.period() - 8.0).abs() < 0.02, "{}", w.period());
        assert!((w.speed() * w.period() - 100.0).abs() < 1.0e-2);
    }

    /// A single sine wave (steepness 0) is exactly `A sin(k x − ω t + φ)`.
    #[test]
    fn single_sine_matches_the_closed_form() {
        let w = SwellWave {
            wavelength: 20.0,
            height: 2.0,
            direction: [1.0, 0.0],
            steepness: 0.0,
            phase: 0.5,
        };
        let s = Swell {
            waves: vec![w],
            ..Swell::default()
        };
        let t = 3.25;
        let inst = s.instant(t, [0.0, 0.0], 32);
        for x in [-7.0f32, 0.0, 3.3, 11.0] {
            let got = inst.sample([x, 2.0]).offset[1];
            let want = (w.wave_number() * x - w.angular_frequency() * t as f32 + 0.5).sin();
            assert!((got - want).abs() < 1.0e-5, "{x}: {got} vs {want}");
        }
    }

    /// The origin only moves where the phase is reduced: the same rest point gives the same
    /// surface from any origin (within f32 rounding).
    #[test]
    fn origin_does_not_change_the_surface() {
        let s = swell();
        let a = s.instant(1234.5, [0.0, 0.0], 32).sample([41.0, -17.0]);
        let b = s.instant(1234.5, [40.0, -16.0], 32).sample([41.0, -17.0]);
        for i in 0..3 {
            assert!((a.offset[i] - b.offset[i]).abs() < 1.0e-4, "{a:?} {b:?}");
        }
    }

    /// Far from the world origin and late in a session the surface keeps millimeter resolution:
    /// the same wave pattern a wavelength-multiple away, and periodic in time.
    #[test]
    fn far_and_late_keeps_precision() {
        let w = SwellWave {
            wavelength: 16.0,
            height: 1.0,
            direction: [1.0, 0.0],
            steepness: 0.7,
            phase: 0.0,
        };
        let s = Swell {
            waves: vec![w],
            ..Swell::default()
        };
        let near = s.instant(5.0, [3.0, 0.0], 32).sample([3.0, 0.0]);
        let far_x = 3.0 + 16.0 * 1000.0;
        let far = s.instant(5.0, [f64::from(far_x), 0.0], 32).sample([far_x, 0.0]);
        assert!(
            (near.offset[1] - far.offset[1]).abs() < 1.0e-3,
            "{near:?} {far:?}"
        );
        // 200 000 periods (about nine days) later, the period taken in f64 from the f32 frequency.
        let period = TAU64 / f64::from(w.angular_frequency());
        let late = s
            .instant(5.0 + period * 200_000.0, [3.0, 0.0], 32)
            .sample([3.0, 0.0]);
        assert!(
            (near.offset[1] - late.offset[1]).abs() < 2.0e-3,
            "{near:?} {late:?}"
        );
    }

    /// The analytic tangents and velocity agree with central differences.
    #[test]
    fn derivatives_match_finite_differences() {
        let s = swell();
        let t = 17.0;
        let inst = s.instant(t, [0.0, 0.0], 32);
        let r = [5.5f32, -3.25];
        let p = inst.sample(r);
        let e = 1.0e-2f32;
        let px = |dx: f32, dz: f32| inst.sample([r[0] + dx, r[1] + dz]).offset;
        for i in 0..3 {
            let fdx = (px(e, 0.0)[i] - px(-e, 0.0)[i]) / (2.0 * e) + if i == 0 { 1.0 } else { 0.0 };
            let fdz = (px(0.0, e)[i] - px(0.0, -e)[i]) / (2.0 * e) + if i == 2 { 1.0 } else { 0.0 };
            assert!(
                (fdx - p.dpdx[i]).abs() < 2.0e-3,
                "dpdx[{i}] {fdx} vs {}",
                p.dpdx[i]
            );
            assert!(
                (fdz - p.dpdz[i]).abs() < 2.0e-3,
                "dpdz[{i}] {fdz} vs {}",
                p.dpdz[i]
            );
        }
        let dt = 1.0e-3;
        let a = s.instant(t - dt, [0.0, 0.0], 32).sample(r).offset;
        let b = s.instant(t + dt, [0.0, 0.0], 32).sample(r).offset;
        for i in 0..3 {
            let v = (b[i] - a[i]) / (2.0 * dt as f32);
            assert!(
                (v - p.velocity[i]).abs() < 5.0e-3,
                "v[{i}] {v} vs {}",
                p.velocity[i]
            );
        }
        let n = p.normal();
        assert!(n[1] > 0.5);
        // The normal is perpendicular to both tangents.
        for t in [p.dpdx, p.dpdz] {
            let dot = n[0] * t[0] + n[1] * t[1] + n[2] * t[2];
            assert!(dot.abs() < 1.0e-5);
        }
    }

    /// The inverse finds the rest point whose displaced position is the query point.
    #[test]
    fn inverse_lands_on_the_query_point() {
        let s = swell();
        let inst = s.instant(42.0, [0.0, 0.0], 32);
        for (x, z) in [(0.0f32, 0.0f32), (13.7, -2.0), (-30.0, 25.0), (3.0, 3.0)] {
            let (r, miss) = inst.rest_point(x, z);
            let p = inst.sample(r);
            assert!(miss < 1.0e-4, "({x}, {z}): miss {miss}");
            assert!((r[0] + p.offset[0] - x).abs() < 1.0e-4);
            assert!((r[1] + p.offset[2] - z).abs() < 1.0e-4);
        }
    }

    /// Crests compress (J < 1) and foam there; troughs stretch (J > 1) and never foam.
    #[test]
    fn crest_foam_comes_from_compression() {
        let w = SwellWave {
            wavelength: 10.0,
            height: 3.0,
            direction: [1.0, 0.0],
            steepness: 1.0,
            phase: std::f32::consts::FRAC_PI_2,
        };
        let s = Swell {
            waves: vec![w],
            crest_foam: 1.0,
            foam_onset: 0.35,
        };
        assert!(s.validate().is_ok(), "{:?}", s.validate());
        let inst = s.instant(0.0, [0.0, 0.0], 32);
        // θ = k x + π/2: the crest (sin θ = 1) at x = 0, the trough at half a wavelength.
        let crest = inst.sample([0.0, 0.0]);
        let trough = inst.sample([5.0, 0.0]);
        assert!(crest.offset[1] > 1.49 && trough.offset[1] < -1.49);
        assert!(
            crest.jacobian < 0.1 && trough.jacobian > 1.9,
            "{crest:?} {trough:?}"
        );
        assert!(inst.foam_at(&crest) > 0.99);
        assert_eq!(inst.foam_at(&trough), 0.0);
        assert_eq!(crest_foam(1.0, 4.0, 0.0), 0.0);
        assert_eq!(crest_foam(0.0, 1.0, 0.35), 1.0);
    }

    #[test]
    fn validation_refuses_folding_and_bad_waves() {
        assert!(swell().validate().is_ok());
        let mut s = swell();
        s.waves[0].steepness = 1.0;
        s.waves[0].height = 20.0;
        assert!(matches!(s.validate(), Err(WaterBodyError::SwellFolds(_))));
        let mut s = swell();
        s.waves[1].direction = [0.0, 0.0];
        assert_eq!(s.validate(), Err(WaterBodyError::NotPositive("swell.direction")));
        let mut s = swell();
        s.waves[2].wavelength = 0.1;
        assert!(s.validate().is_err());
        let mut s = swell();
        s.waves = vec![s.waves[2]; MAX_SWELL_WAVES + 1];
        assert!(matches!(s.validate(), Err(WaterBodyError::TooManyWaves { .. })));
    }

    /// `open_sea` is deterministic, valid, longest first and has the asked height.
    #[test]
    fn open_sea_is_valid_and_deterministic() {
        let a = Swell::open_sea([1.0, 0.5], 2.0, 50.0, 16);
        assert_eq!(a, Swell::open_sea([1.0, 0.5], 2.0, 50.0, 16));
        assert!(a.validate().is_ok(), "{:?}", a.validate());
        assert_eq!(a.waves.len(), 16);
        assert!(a.waves.windows(2).all(|w| w[0].wavelength >= w[1].wavelength));
        let rss = a.waves.iter().map(|w| w.height * w.height).sum::<f32>().sqrt();
        assert!((rss - 2.0).abs() < 1.0e-3);
        // Gentle waves: steepness saturates at 1 below the 0.6 fold target.
        assert!(a.fold_sum() <= 0.6 + 1.0e-3);
        assert!(a.waves.iter().all(|w| w.steepness == 1.0), "{a:?}");
        // A steep sea reaches the target with a shared steepness under 1.
        let steep = Swell::open_sea([1.0, 0.0], 6.0, 30.0, 4);
        assert!(steep.validate().is_ok(), "{:?}", steep.validate());
        assert!((steep.fold_sum() - 0.6).abs() < 1.0e-3, "{}", steep.fold_sum());
        assert!(steep.waves[0].steepness < 1.0);
        assert_eq!(
            Swell::open_sea([0.0, 0.0], 1.0, 10.0, 99).waves.len(),
            MAX_SWELL_WAVES
        );
    }

    /// The grid is finer for a narrow view and for an eye close to the water, its cell is a
    /// power of two and it reaches as far as asked.
    #[test]
    fn ring_grid_follows_the_view() {
        let wide = RingGrid::for_view(60f32.to_radians(), 1080, 20.0, 48, 3000.0);
        assert_eq!(wide.half_cells, RING_CELLS_AT_60_DEG);
        assert!(wide.cell().log2().fract() == 0.0);
        assert!(wide.outer_half_size() >= 3000.0);
        assert!(wide.outer_half_size() < 6000.0 * 2.0);
        let scope = RingGrid::for_view(6f32.to_radians(), 1080, 20.0, 48, 3000.0);
        assert!(scope.cell() < wide.cell());
        assert!(scope.half_cells > wide.half_cells && scope.half_cells <= 48);
        assert_eq!(scope.half_cells % 2, 0);
        let low = RingGrid::for_view(6f32.to_radians(), 1080, 20.0, 16, 3000.0);
        assert_eq!(low.half_cells, 16);
        let high_eye = RingGrid::for_view(60f32.to_radians(), 1080, 400.0, 48, 3000.0);
        assert!(high_eye.cell() > wide.cell());
        let capped = RingGrid::for_view(60f32.to_radians(), 1080, 20.0, 48, 1.0e12);
        assert_eq!(capped.rings, MAX_RINGS);
        let nan = RingGrid::for_view(f32::NAN, 0, f32::NAN, 0, f32::NAN);
        assert!(nan.cell() >= MIN_RING_CELL && nan.half_cells >= 2 && nan.rings == 1);
    }

    #[test]
    fn fade_cell_is_continuous_and_doubles_per_ring() {
        let g = RingGrid::for_view(60f32.to_radians(), 1080, 20.0, 48, 3000.0);
        let r0 = g.inner_half_size();
        assert_eq!(g.fade_cell([0.0, 0.0]), g.cell());
        assert_eq!(g.fade_cell([0.5 * r0, -0.2 * r0]), g.cell());
        assert!((g.fade_cell([r0, 0.0]) - 2.0 * g.cell()).abs() < 1.0e-6);
        assert!((g.fade_cell([0.0, -2.0 * r0]) - 4.0 * g.cell()).abs() < 1.0e-6);
    }

    fn snap_grid(m: u32, rings: u32) -> RingGrid {
        RingGrid {
            cell_bits: 0.25f32.to_bits(),
            half_cells: m,
            rings,
        }
    }

    /// Eyes all over a big world: the center is the eye snapped to two cells, as the renderer does.
    fn sample_eyes() -> Vec<([f64; 2], [f64; 2])> {
        let cell = 0.25f64;
        [
            [0.0, 0.0],
            [0.3, -0.1],
            [1.7, 5.3],
            [-12.4, 3.9],
            [100_003.37, -250_000.61],
            [7.0e6 + 0.43, 2.0e6 - 0.77],
        ]
        .iter()
        .map(|eye| {
            let snap = 2.0 * cell;
            (
                [(eye[0] / snap).floor() * snap, (eye[1] / snap).floor() * snap],
                *eye,
            )
        })
        .collect()
    }

    /// The boundary shifts put each ring boundary on its own lattice (the eye snapped to
    /// `2^(j+1)` cells), the inner square does not move, and a grid with too few cells does not
    /// snap its rings.
    #[test]
    fn ring_shifts_land_on_the_ring_lattices() {
        let g = snap_grid(16, 6);
        assert!(g.snaps_rings());
        for (origin, eye) in sample_eyes() {
            let shifts = g.ring_shifts(origin, eye);
            assert_eq!(shifts.len(), 6);
            assert_eq!(shifts[0], [0.0, 0.0]);
            for (j, d) in shifts.iter().enumerate().skip(1) {
                let step = 0.25 * (1u64 << (j + 1)) as f64;
                for axis in 0..2 {
                    let world = origin[axis] + f64::from(d[axis]);
                    // Exact up to the f32 rounding of the shift.
                    let q = world / step;
                    assert!(
                        (q - q.round()).abs() < 1.0e-3 * (1.0 + world.abs() * 1.0e-7),
                        "ring {j} axis {axis}: {q}"
                    );
                    assert!(world <= eye[axis] + 1.0e-3 && world > eye[axis] - step - 1.0e-3);
                }
            }
        }
        assert!(!snap_grid(6, 6).snaps_rings());
        assert!(
            snap_grid(6, 6)
                .ring_shifts([0.0; 2], [3.3, 9.9])
                .iter()
                .all(|d| *d == [0.0, 0.0])
        );
        assert!(!snap_grid(16, 1).snaps_rings());
    }

    /// The shift is zero inside the inner square, continuous across every ring seam (the vertices
    /// the two rings share end up together), and never folds a ring: `x + shift(x)` stays strictly
    /// increasing along rays.
    #[test]
    fn ring_shift_is_continuous_and_never_folds() {
        let g = snap_grid(8, 7);
        let inner = g.inner_half_size();
        for (origin, eye) in sample_eyes() {
            let shifts = g.ring_shifts(origin, eye);
            assert_eq!(g.ring_shift(&shifts, [inner * 0.5, -inner * 0.9]), [0.0, 0.0]);
            // Seams: the value at a boundary, from either side.
            for k in 1..6u32 {
                let b = inner * (1u64 << k) as f32;
                let below = g.ring_shift(&shifts, [b, 0.3]);
                let above = g.ring_shift(&shifts, [b * (1.0 + 1.0e-6), 0.3]);
                for axis in 0..2 {
                    assert!(
                        (below[axis] - above[axis]).abs() < 1.0e-3,
                        "seam {k}: {below:?} {above:?}"
                    );
                    assert_eq!(
                        below[axis], shifts[k as usize][axis],
                        "on the boundary the node's shift"
                    );
                }
            }
            // No fold along an axis ray, over fine steps.
            let outer = g.outer_half_size();
            let mut last = f32::NEG_INFINITY;
            let mut x = 0.0f32;
            while x <= outer {
                let moved = x + g.ring_shift(&shifts, [x, 0.0])[0];
                assert!(moved > last || x == 0.0, "fold at {x}: {moved} <= {last}");
                last = moved;
                x += 0.07;
            }
        }
    }

    /// A far ring stays still while the eye moves a little: with the eye moving by a few finest
    /// cells the outermost boundary's world position changes far less often than the finest one.
    #[test]
    fn far_rings_move_rarely() {
        let g = snap_grid(16, 7);
        let cell = 0.25f64;
        let snap = 2.0 * cell;
        let mut changes = [0u32; 7];
        let mut last: Vec<[f64; 2]> = Vec::new();
        for i in 0..400 {
            let eye = [10.0 + f64::from(i) * 0.05, -4.0];
            let origin = [(eye[0] / snap).floor() * snap, (eye[1] / snap).floor() * snap];
            let world: Vec<[f64; 2]> = g
                .ring_shifts(origin, eye)
                .iter()
                .map(|d| [origin[0] + f64::from(d[0]), origin[1] + f64::from(d[1])])
                .collect();
            if !last.is_empty() {
                for j in 1..7 {
                    if (world[j][0] - last[j][0]).abs() > 1.0e-6 {
                        changes[j] += 1;
                    }
                }
            }
            last = world;
        }
        // Ring j snaps to 2^(j+1) cells: it steps once per that distance.
        assert!(changes[1] > changes[3] && changes[3] > changes[6], "{changes:?}");
        assert!(changes[6] <= 1, "{changes:?}");
    }

    /// Spec 100 § 6: boundary `j` steps once per `2^(j+1)` finest cells of eye travel, by exactly
    /// that lattice step, and never otherwise (a panning eye on an exact binary lattice, counted
    /// per ring against the analytic count).
    #[test]
    fn each_ring_steps_once_per_its_own_lattice_cell() {
        let g = snap_grid(16, 7);
        let cell = 0.25f64;
        let snap = 2.0 * cell;
        let (start, stride, frames) = (10.0f64, 1.0 / 32.0, 4096u32);
        let mut steps = [0u64; 7];
        let mut last: Vec<[f64; 2]> = Vec::new();
        for i in 0..frames {
            let eye = [start + f64::from(i) * stride, -4.0];
            let origin = [(eye[0] / snap).floor() * snap, (eye[1] / snap).floor() * snap];
            let world: Vec<[f64; 2]> = g
                .ring_shifts(origin, eye)
                .iter()
                .map(|d| [origin[0] + f64::from(d[0]), origin[1] + f64::from(d[1])])
                .collect();
            if !last.is_empty() {
                for j in 1..7 {
                    let lattice = cell * (1u64 << (j + 1)) as f64;
                    let moved = world[j][0] - last[j][0];
                    if moved != 0.0 {
                        assert_eq!(moved, lattice, "ring {j} moved by {moved} at {eye:?}");
                        steps[j] += 1;
                    }
                    assert_eq!(world[j][1], last[j][1], "ring {j} moved across the pan");
                }
            }
            last = world;
        }
        let end = start + f64::from(frames - 1) * stride;
        for (j, &count) in steps.iter().enumerate().skip(1) {
            let lattice = cell * (1u64 << (j + 1)) as f64;
            let want = (end / lattice).floor() - (start / lattice).floor();
            assert_eq!(count as f64, want, "ring {j}: {steps:?}");
        }
        assert!(
            steps[6] > 0,
            "the pan is long enough to step the outer ring: {steps:?}"
        );
    }

    /// Many surface samples of one swell plane share one term buffer, and a point outside the
    /// rectangle widened by the shift bound builds none.
    #[test]
    fn surface_samples_share_one_term_buffer_and_a_miss_builds_none() {
        use super::super::{WaterBody, WaterClock, WaterLook};
        let mut sea = WaterBody::plane([0.0, 1.0, 0.0], [20.0, 20.0], WaterLook::ocean());
        sea.swell = Some(Swell::open_sea([1.0, 0.2], 1.2, 18.0, 12));
        let clock = WaterClock {
            tick: 4_000,
            ..WaterClock::default()
        };
        let shift = sea.swell.as_ref().expect("swell").max_shift();
        test_reset_instant_allocs();
        assert!(sea.surface(20.0 + shift + 5.0, 0.0, &clock).is_none());
        assert_eq!(test_instant_calls(), 0, "a miss must not build an instant");
        assert_eq!(test_instant_allocs(), 0, "a miss must not allocate a term buffer");
        for _ in 0..16 {
            sea.surface(1.5, -2.0, &clock).expect("inside");
        }
        assert_eq!(test_instant_calls(), 16, "each inside sample builds one instant");
        assert!(
            test_instant_allocs() <= 1,
            "term-buffer allocations {}, want at most one",
            test_instant_allocs()
        );
    }

    /// `docs/specs/water.md` § 3, row by row: each field's range at both ends and just past them,
    /// the serde defaults (steepness 0.5, phase 0) and the foam defaults. The phase is refused
    /// beyond ±10 000 rad (ADR-5826), which keeps the f32 phase to well under a milliradian.
    #[test]
    fn swell_table_ranges_and_defaults_follow_the_spec() {
        let wave = |wavelength: f32, height: f32, steepness: f32, phase: f32| Swell {
            waves: vec![SwellWave {
                wavelength,
                height,
                direction: [1.0, 0.0],
                steepness,
                phase,
            }],
            ..Swell::default()
        };
        let named = |s: &Swell, name: &str| matches!(s.validate(), Err(WaterBodyError::OutOfRange { name: n, .. }) if n == name);
        // wavelength 0.25..=2000 m (a gentle wave, so the fold sum stays small).
        assert_eq!(wave(0.25, 0.01, 0.5, 0.0).validate(), Ok(()));
        assert_eq!(wave(2000.0, 1.0, 0.5, 0.0).validate(), Ok(()));
        assert!(named(&wave(0.24, 0.01, 0.5, 0.0), "swell.wavelength"));
        assert!(named(&wave(2000.5, 1.0, 0.5, 0.0), "swell.wavelength"));
        // height 0..=40 m.
        assert_eq!(wave(100.0, 0.0, 0.5, 0.0).validate(), Ok(()));
        assert_eq!(wave(2000.0, 40.0, 0.5, 0.0).validate(), Ok(()));
        assert!(named(&wave(100.0, -0.01, 0.5, 0.0), "swell.height"));
        assert!(named(&wave(2000.0, 40.01, 0.5, 0.0), "swell.height"));
        // steepness 0..=1.
        assert_eq!(wave(100.0, 1.0, 0.0, 0.0).validate(), Ok(()));
        assert_eq!(wave(100.0, 1.0, 1.0, 0.0).validate(), Ok(()));
        assert!(named(&wave(100.0, 1.0, -0.01, 0.0), "swell.steepness"));
        assert!(named(&wave(100.0, 1.0, 1.01, 0.0), "swell.steepness"));
        // phase: finite, within ±10 000 rad.
        assert_eq!(wave(100.0, 1.0, 0.5, -1.0e4).validate(), Ok(()));
        assert_eq!(wave(100.0, 1.0, 0.5, 1.0e4).validate(), Ok(()));
        assert!(named(&wave(100.0, 1.0, 0.5, 1.0001e4), "swell.phase"));
        assert_eq!(
            wave(100.0, 1.0, 0.5, f32::NAN).validate(),
            Err(WaterBodyError::NotFinite("swell.phase"))
        );
        // crest_foam 0..=4 (1), foam_onset 0..=1 (0.15).
        let d = Swell::default();
        assert_eq!((d.crest_foam, d.foam_onset), (1.0, 0.15));
        assert!(d.waves.is_empty());
        for (foam, onset, ok) in [
            (0.0, 0.0, true),
            (4.0, 1.0, true),
            (4.01, 0.15, false),
            (-0.01, 0.15, false),
            (1.0, 1.01, false),
            (1.0, -0.01, false),
        ] {
            let s = Swell {
                crest_foam: foam,
                foam_onset: onset,
                ..wave(100.0, 1.0, 0.5, 0.0)
            };
            assert_eq!(s.validate().is_ok(), ok, "{foam} / {onset}");
        }
        // Serde defaults: an authored wave without steepness or phase gets 0.5 and 0.
        let parsed: SwellWave =
            ron::from_str("(wavelength: 12.0, height: 0.5, direction: (0.0, 2.0))").expect("parse");
        assert_eq!((parsed.steepness, parsed.phase), (0.5, 0.0));
        assert_eq!(parsed.unit_direction(), [0.0, 1.0]);
        let parsed: Swell = ron::from_str("(waves: [])").expect("parse");
        assert_eq!(parsed, Swell::default());
        // The amplitude is half the height, the horizontal amplitude steepness times that.
        let s = wave(30.0, 2.0, 0.4, 0.0);
        assert_eq!(s.waves[0].amplitude(), 1.0);
        assert!((s.max_shift() - 0.4).abs() < 1.0e-6);
        assert_eq!(s.max_rise(), 1.0);
    }

    /// The ring cell spans [`PIXELS_PER_CELL`] pixels at the nadir, rounded down to a power of two
    /// and held to 1/32..=64 m; the inner half width is `⌈24 tan 30° / tan(fov / 2)⌉`, even and
    /// capped by the tier (`docs/specs/water.md` § 3).
    #[test]
    fn ring_cell_and_width_follow_the_formula() {
        // 20 m up, 60°, 1080 rows: 6 · 2 · 20 · tan 30° / 1080 = 0.128 m, down to 1/8.
        let g = RingGrid::for_view(60f32.to_radians(), 1080, 20.0, 48, 100.0);
        assert_eq!(g.cell(), 0.125);
        assert_eq!(g.half_cells, 24);
        // 80 m up: 0.513 m, down to 1/2.
        assert_eq!(
            RingGrid::for_view(60f32.to_radians(), 1080, 80.0, 48, 100.0).cell(),
            0.5
        );
        // The clamps.
        assert_eq!(
            RingGrid::for_view(60f32.to_radians(), 1080, 1.0e6, 48, 100.0).cell(),
            MAX_RING_CELL
        );
        assert_eq!(
            RingGrid::for_view(60f32.to_radians(), 4320, 1.0, 48, 100.0).cell(),
            MIN_RING_CELL
        );
        // 90°: 24 tan 30° = 13.86, up to 14. 40°: 38.07, up to 39, even 40; High caps it at 32.
        assert_eq!(
            RingGrid::for_view(90f32.to_radians(), 1080, 20.0, 48, 100.0).half_cells,
            14
        );
        assert_eq!(
            RingGrid::for_view(40f32.to_radians(), 1080, 20.0, 48, 100.0).half_cells,
            40
        );
        assert_eq!(
            RingGrid::for_view(40f32.to_radians(), 1080, 20.0, 32, 100.0).half_cells,
            32
        );
        // Rings double until they pass the reach: 24 cells of 1/8 m is 3 m, so 100 m takes six
        // doublings, seven rings.
        assert_eq!(g.inner_half_size(), 3.0);
        assert_eq!(g.rings, 7);
        assert!(g.outer_half_size() >= 100.0 && g.outer_half_size() < 200.0);
    }
}
