// SPDX-License-Identifier: MPL-2.0
//! Lattice value noise, fractal Brownian motion and ridged multifractal noise with a directional
//! grain, evaluated at world positions.
//!
//! The lattice hash mixes the cell indices with the xxHash32 primes (Yann Collet, xxHash
//! specification, BSD-2) and maps the result to `[-1, 1]`; corners are blended with the
//! smoothstep fade. fBm sums octaves of that noise (Musgrave, "Texturing and Modeling: A
//! Procedural Approach", ch. 16); ridged noise folds each octave to `(1 − |n|)^sharpness` and
//! weights it by the octave below, so detail gathers on the crests (the same chapter's ridged
//! multifractal). The grain divides the along-grain coordinate by `anisotropy`, stretching
//! features along `grain_angle_deg`.

use serde::{Deserialize, Serialize};

use super::{GridFrame, OpError, require, sin_cos_deg, smoothstep};

/// Settings of a fractal noise field.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct NoiseParams {
    /// Seed; the same seed and settings give the same field.
    pub seed: u32,
    /// fBm lies within `[-amplitude_m, amplitude_m]`; ridged noise within `[0, amplitude_m]`.
    pub amplitude_m: f64,
    /// Wavelength of the first (coarsest) octave, meters.
    pub wavelength_m: f64,
    /// Octaves, 1 to 16.
    pub octaves: u32,
    /// Frequency multiplier per octave.
    pub lacunarity: f64,
    /// Amplitude multiplier per octave.
    pub gain: f64,
    /// The direction features run along, degrees from +X toward +Z.
    pub grain_angle_deg: f64,
    /// How many times longer features are along the grain than across it (1 = isotropic).
    pub anisotropy: f64,
}

impl Default for NoiseParams {
    fn default() -> Self {
        Self {
            seed: 0,
            amplitude_m: 20.0,
            wavelength_m: 400.0,
            octaves: 5,
            lacunarity: 2.0,
            gain: 0.5,
            grain_angle_deg: 0.0,
            anisotropy: 1.0,
        }
    }
}

impl NoiseParams {
    /// Noise of `amplitude_m` and `wavelength_m` with `octaves` and the other settings at their
    /// defaults.
    #[must_use]
    pub fn new(seed: u32, amplitude_m: f64, wavelength_m: f64, octaves: u32) -> Self {
        Self {
            seed,
            amplitude_m,
            wavelength_m,
            octaves,
            ..Self::default()
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// [`OpError::BadParam`] naming the first bad setting.
    pub fn validate(&self) -> Result<(), OpError> {
        require(self.amplitude_m.is_finite(), "amplitude_m", "must be finite")?;
        require(
            self.wavelength_m.is_finite() && self.wavelength_m > 0.0,
            "wavelength_m",
            "must be finite and positive",
        )?;
        require((1..=16).contains(&self.octaves), "octaves", "must be 1 to 16")?;
        require(
            self.lacunarity.is_finite() && self.lacunarity > 0.0,
            "lacunarity",
            "must be finite and positive",
        )?;
        require(
            self.gain.is_finite() && self.gain >= 0.0,
            "gain",
            "must be finite and at least 0",
        )?;
        require(
            self.grain_angle_deg.is_finite(),
            "grain_angle_deg",
            "must be finite",
        )?;
        require(
            self.anisotropy.is_finite() && self.anisotropy >= 1.0,
            "anisotropy",
            "must be finite and at least 1 (1 = isotropic)",
        )
    }
}

/// Which fractal [`noise_field`] and [`add_noise`] evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseKind {
    /// Fractal Brownian motion, `[-amplitude, amplitude]`.
    Fbm,
    /// Ridged multifractal, `[0, amplitude]`; a larger `sharpness` gives thinner crests.
    Ridged {
        /// Exponent on the ridge profile (> 0; 2 is the usual choice).
        sharpness: f64,
    },
}

/// Lattice hash of cell `(ix, iz)` mapped to `[-1, 1]`.
#[must_use]
pub fn hash_unit(ix: i64, iz: i64, seed: u32) -> f64 {
    let s = seed.wrapping_mul(2_246_822_519).wrapping_add(3_266_489_917);
    // Two's-complement truncation, so negative cells hash like their 32-bit pattern.
    let a = (ix as u32).wrapping_mul(374_761_393);
    let b = (iz as u32).wrapping_mul(668_265_263);
    let mut h = a.wrapping_add(b).wrapping_add(s);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    f64::from(h) / 4_294_967_295.0 * 2.0 - 1.0
}

/// Smooth lattice value noise in `[-1, 1]` at lattice coordinates `(u, v)`.
#[must_use]
pub fn value_noise(u: f64, v: f64, seed: u32) -> f64 {
    let iu = u.floor();
    let iv = v.floor();
    let fu = smoothstep(u - iu);
    let fv = smoothstep(v - iv);
    let (i0, j0) = (iu as i64, iv as i64);
    let n00 = hash_unit(i0, j0, seed);
    let n10 = hash_unit(i0 + 1, j0, seed);
    let n01 = hash_unit(i0, j0 + 1, seed);
    let n11 = hash_unit(i0 + 1, j0 + 1, seed);
    let top = n00 + (n10 - n00) * fu;
    let bot = n01 + (n11 - n01) * fu;
    top + (bot - top) * fv
}

/// A noise field with its grain basis worked out once.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Field {
    p: NoiseParams,
    sin: f64,
    cos: f64,
}

impl Field {
    pub(crate) fn new(p: &NoiseParams) -> Result<Self, OpError> {
        p.validate()?;
        let (sin, cos) = sin_cos_deg(p.grain_angle_deg);
        Ok(Self { p: *p, sin, cos })
    }

    /// `(u, v)` lattice coordinates of the first octave at world `(x, z)`.
    #[inline]
    fn coords(&self, x: f64, z: f64) -> (f64, f64) {
        let along = x * self.cos + z * self.sin;
        let across = -x * self.sin + z * self.cos;
        (
            along / self.p.anisotropy / self.p.wavelength_m,
            across / self.p.wavelength_m,
        )
    }

    pub(crate) fn fbm(&self, x: f64, z: f64) -> f64 {
        let (u, v) = self.coords(x, z);
        let (mut total, mut amp, mut norm, mut freq) = (0.0, 1.0, 0.0, 1.0);
        for octave in 0..self.p.octaves {
            let seed = self.p.seed.wrapping_add(1013u32.wrapping_mul(octave));
            total += amp * value_noise(u * freq, v * freq, seed);
            norm += amp;
            amp *= self.p.gain;
            freq *= self.p.lacunarity;
        }
        if norm > 0.0 {
            total / norm * self.p.amplitude_m
        } else {
            0.0
        }
    }

    pub(crate) fn ridged(&self, sharpness: f64, x: f64, z: f64) -> f64 {
        let (u, v) = self.coords(x, z);
        let (mut total, mut amp, mut norm, mut freq, mut weight) = (0.0, 1.0, 0.0, 1.0, 1.0);
        for octave in 0..self.p.octaves {
            let seed = self.p.seed.wrapping_add(1013u32.wrapping_mul(octave));
            let n = value_noise(u * freq, v * freq, seed);
            let base = 1.0 - n.abs();
            let ridge = if sharpness == 2.0 {
                base * base
            } else if sharpness == 1.0 {
                base
            } else {
                f64::from(regia_core::math::detmath::det_powf(base as f32, sharpness as f32))
            } * weight;
            total += amp * ridge;
            weight = (ridge * 2.0).clamp(0.0, 1.0);
            norm += amp;
            amp *= self.p.gain;
            freq *= self.p.lacunarity;
        }
        if norm > 0.0 {
            (total / norm).clamp(0.0, 1.0) * self.p.amplitude_m
        } else {
            0.0
        }
    }

    pub(crate) fn eval(&self, kind: NoiseKind, x: f64, z: f64) -> f64 {
        match kind {
            NoiseKind::Fbm => self.fbm(x, z),
            NoiseKind::Ridged { sharpness } => self.ridged(sharpness, x, z),
        }
    }
}

fn check_kind(kind: NoiseKind) -> Result<(), OpError> {
    match kind {
        NoiseKind::Fbm => Ok(()),
        NoiseKind::Ridged { sharpness } => require(
            sharpness.is_finite() && sharpness > 0.0,
            "sharpness",
            "must be finite and positive",
        ),
    }
}

/// fBm height at world `(x, z)`, meters.
///
/// # Errors
///
/// [`OpError::BadParam`] for settings out of range.
pub fn fbm_at(p: &NoiseParams, x: f64, z: f64) -> Result<f64, OpError> {
    Ok(Field::new(p)?.fbm(x, z))
}

/// Ridged-noise height at world `(x, z)`, meters.
///
/// # Errors
///
/// [`OpError::BadParam`] for settings or a `sharpness` out of range.
pub fn ridged_at(p: &NoiseParams, sharpness: f64, x: f64, z: f64) -> Result<f64, OpError> {
    check_kind(NoiseKind::Ridged { sharpness })?;
    Ok(Field::new(p)?.ridged(sharpness, x, z))
}

/// A noise field over a grid, row-major.
///
/// # Errors
///
/// A frame error or [`OpError::BadParam`].
pub fn noise_field(frame: &GridFrame, p: &NoiseParams, kind: NoiseKind) -> Result<Vec<f32>, OpError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.noise").entered();
    frame.check()?;
    check_kind(kind)?;
    let field = Field::new(p)?;
    let mut out = Vec::with_capacity(frame.len());
    for r in 0..frame.rows {
        for c in 0..frame.cols {
            let [x, z] = frame.position(c, r);
            out.push(field.eval(kind, x, z) as f32);
        }
    }
    Ok(out)
}

/// fBm over a grid: heights within `[-amplitude_m, amplitude_m]`.
///
/// # Errors
///
/// As [`noise_field`].
pub fn fbm(frame: &GridFrame, p: &NoiseParams) -> Result<Vec<f32>, OpError> {
    noise_field(frame, p, NoiseKind::Fbm)
}

/// Ridged multifractal noise over a grid: heights within `[0, amplitude_m]`.
///
/// # Errors
///
/// As [`noise_field`].
pub fn ridged(frame: &GridFrame, p: &NoiseParams, sharpness: f64) -> Result<Vec<f32>, OpError> {
    noise_field(frame, p, NoiseKind::Ridged { sharpness })
}

/// `heights` plus a noise field.
///
/// # Errors
///
/// As [`noise_field`], plus [`OpError::SampleCount`] / [`OpError::NonFinite`] for the heights.
pub fn add_noise(
    frame: &GridFrame,
    heights: &[f32],
    p: &NoiseParams,
    kind: NoiseKind,
) -> Result<Vec<f32>, OpError> {
    frame.check_heights(heights)?;
    check_kind(kind)?;
    let field = Field::new(p)?;
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.add_noise").entered();
    let mut out = Vec::with_capacity(heights.len());
    for r in 0..frame.rows {
        for c in 0..frame.cols {
            let [x, z] = frame.position(c, r);
            let h = f64::from(heights[r as usize * frame.cols as usize + c as usize]);
            out.push((h + field.eval(kind, x, z)) as f32);
        }
    }
    Ok(out)
}
