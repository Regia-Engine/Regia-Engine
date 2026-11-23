// SPDX-License-Identifier: MPL-2.0
//! The FFT ocean band (ADR-1969, `docs/specs/water-ocean.md`, N19): a wind sea synthesized from a
//! directional spectrum by an inverse FFT into tiling maps. This file is the CPU half: the
//! authored [`FftOcean`], the spectrum tables ([`OceanTables`]), the mirror of the compute pass
//! ([`OceanFrame`]) and the surface query. `ocean_gpu.rs` is the GPU half, `water_ocean.wgsl` the
//! compute shader.
//!
//! # The sea
//!
//! The surface is a sum of deep-water waves (Tessendorf 2001). A wave with wave vector `k` has the
//! complex amplitude `a(k)`, and the height and its choppy horizontal offset are
//!
//! ```text
//! ĥ(k, t) = a(k) e^(−iωt) + conj a(−k) e^(iωt)         ω = sqrt(g |k|)
//! h(x, t) = Σ ĥ(k, t) e^(i k·x)
//! D(x, t) = Σ i λ (k / |k|) ĥ(k, t) e^(i k·x)          a point at rest at x moves to x + D
//! ```
//!
//! `a(k)` is a complex Gaussian of variance `E(k) Δk² / 2`, where `E` is the variance density of
//! the [`OceanSpectrum`] over the wave-vector plane: JONSWAP (Hasselmann et al. 1973) or Phillips
//! (as in Tessendorf 2001), both with a directional spread. The Gaussian is hashed from the
//! seed, the band and the mode's integer index, never drawn from a running stream, so a mode has
//! the same amplitude at every map size.
//!
//! # Bands
//!
//! The modes are split over [`OCEAN_BANDS`] tiles ([`FftOcean::tiles`], longest first), each
//! holding the wave numbers its grid resolves with at least four texels per wavelength
//! ([`OceanTables::band_limits`]); the last band runs to the grid's Nyquist limit. A mode lives in
//! exactly one band, so nothing is counted twice, and the sum repeats only at the common multiple
//! of the three tiles.
//!
//! # Determinism, the loop and the phase
//!
//! Every `ω` is quantized to a whole multiple `n` of `2π / T` ([`OCEAN_LOOP_SECONDS`]; after
//! Tessendorf 2001 § 4.3), so the sea is periodic in `T` and the phase of a mode is exact in
//! integers: the loop fraction `τ` as a 32-bit fixed-point number ([`loop_fraction`]) times `n`,
//! wrapping. The compute shader and [`OceanFrame::at`] form the same integer, whatever the clock
//! says, after any number of hours. The surface is a pure function of the [`FftOcean`] and the
//! clock's seconds.
//!
//! # The CPU mirror
//!
//! [`OceanFrame`] runs the same inverse FFT on the CPU at [`OCEAN_QUERY_SIZE`], the grid High
//! draws, with the same modes; the shader and [`OceanFrame::sample`] blend texels the same way.
//! Ultra draws [`OCEAN_MAX_SIZE`]: the same modes plus finer ones in the last band, so a query
//! there misses only the waves shorter than twice the 128 grid's texel. The answer never depends
//! on the tier, nor on [`FftOcean::grid`], which picks a larger drawn grid (ADR-2126). A frame is
//! built lazily, once per tick and ocean, by the first query that can land inside the rectangle
//! ([`frame_at`]). A point outside that rectangle, widened by the shift bound, returns nothing
//! and does not transform. Later queries of that tick on the same thread reuse the frame without
//! taking the memo lock. [`OceanFrame::at`] runs at any of [`OCEAN_GRIDS`], so the same transform
//! is the reference the compute pass is checked against at every grid.
//!
//! # Near the shore
//!
//! A deep-water spectrum means nothing where the bed reaches the waves, and a tiling map cannot
//! shoal. Over a shore field the surface is a blend by the depth at the rest point,
//! `w F + (1 − w) S` ([`FftOcean::shore_weight`]), of the ocean `F` and the shoaled trochoidal
//! swell `S` of spec 87: the body's own swell, or a stand-in fitted to the spectrum
//! ([`FftOcean::stand_in_swell`]). Low draws `S` alone.

use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use super::shore::{self, ShoreField};
use super::swell::{self, SwellInstant, SwellSample};
use super::{
    GRAVITY, Swell, WaterBody, WaterBodyError, WaterClock, WaterShape, WaterSurface, WaterTier, range,
};

/// Bands of the ocean (tiles of the compute pass).
pub const OCEAN_BANDS: usize = 3;

/// Texels per side of the grid the CPU queries use, and the one High draws.
pub const OCEAN_QUERY_SIZE: u32 = 128;

/// Texels per side of the grid Ultra draws.
pub const OCEAN_MAX_SIZE: u32 = 256;

/// The grids an ocean can draw ([`FftOcean::grid`]). The tiers use the first two.
pub const OCEAN_GRIDS: [u32; 4] = [128, 256, 512, 1024];

/// The loop period `T` of the sea (s): every `ω` is a whole multiple of `2π / T`.
pub const OCEAN_LOOP_SECONDS: f64 = 2048.0;

/// Texels per wavelength at each band's short end (the last band runs to Nyquist).
pub const BAND_TEXELS_PER_WAVE: f64 = 4.0;

/// Waves of the stand-in swell ([`FftOcean::stand_in_swell`]).
pub const STAND_IN_WAVES: usize = 16;

/// Frames of [`frame_at`]'s memo (a tick may be asked for by two worlds, or by two map sizes).
const FRAME_MEMO: usize = 4;

/// Spectrum tables of [`tables_for`]'s memo.
const TABLE_MEMO: usize = 4;

const TAU64: f64 = std::f64::consts::TAU;

/// Radians per unit of the 32-bit phase, `2π / 2³²` (the shader repeats it).
pub const PHASE_SCALE: f32 = (TAU64 / 4_294_967_296.0) as f32;

/// The directional spectrum of an [`FftOcean`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OceanSpectrum {
    /// JONSWAP (Hasselmann et al. 1973): a fetch-limited sea with a sharpened peak, spread by
    /// `cos^(2s)(θ / 2)` around the wind (Longuet-Higgins et al. 1963).
    #[default]
    Jonswap,
    /// Phillips (as in Tessendorf 2001): the saturated range `(α / 2) k⁻⁴`, cut off below the
    /// longest wave the wind raises, with a `cos²` spread over the downwind half.
    Phillips,
}

impl OceanSpectrum {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Jonswap => "jonswap",
            Self::Phillips => "phillips",
        }
    }
}

/// The FFT ocean of a plane body ([`WaterBody::ocean`]): a wind sea on High and Ultra, the swell
/// on Low.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FftOcean {
    /// Seed of the random phases: the same seed and clock give the same sea, on any machine.
    pub seed: u32,
    /// The spectrum.
    pub spectrum: OceanSpectrum,
    /// Wind speed 10 m above the water (m/s), 0.5..=40.
    pub wind_speed: f32,
    /// Direction the wind blows toward, along x and z (any non-zero length).
    pub wind_direction: [f32; 2],
    /// Distance the wind has blown over open water (m), 1 000..=2 000 000 (JONSWAP only).
    pub fetch: f32,
    /// JONSWAP peak enhancement, 1..=7 (1 is a fully developed sea, 3.3 the mean of the data).
    pub gamma: f32,
    /// Exponent `s` of the `cos^(2s)(θ / 2)` spread, 1..=64: larger is more long-crested.
    pub spread: f32,
    /// Horizontal chop `λ`, 0..=2: 0 is a pure height field, 1 the linear theory's particle motion.
    pub choppiness: f32,
    /// Scale on the wave heights, 0..=4.
    pub height_scale: f32,
    /// Tile lengths (m), longest first, each at least twice the next, in 8..=20 000. Tiles that
    /// are not whole multiples of each other put off the repeat of the sum.
    pub tiles: [f32; OCEAN_BANDS],
    /// Crest foam strength, 0..=4 (0 none).
    pub crest_foam: f32,
    /// Horizontal compression `1 − J` where crest foam starts, 0..=1 (lower foams more).
    pub foam_onset: f32,
    /// Water at least this deep (m) carries the whole ocean (over a shore field).
    ///
    /// Validated as `shore_end..=1000`. The default `shore_end` is 3, so a patch that sets only
    /// `shore_start` below 3 is out of range.
    pub shore_start: f32,
    /// Water at most this deep (m) carries only the swell (over a shore field).
    pub shore_end: f32,
    /// Texels per side of the drawn maps, one of [`OCEAN_GRIDS`]; 0 (the default) lets the tier
    /// choose. Low draws no maps whatever this says, and queries always use the 128 grid, so this
    /// changes the look and the cost, never what gameplay feels (ADR-2126).
    pub grid: u32,
    /// Seconds lingering foam takes to fade from full to none, 0..=120; 0 (the default) keeps
    /// the foam instantaneous and allocates no history (ADR-2843, `ocean_foam`).
    pub foam_decay: f32,
    /// Seconds a breaking crest takes to leave about two thirds of its foam behind, 0..=30
    /// (0: at once). Only read when [`Self::foam_decay`] is on.
    pub foam_growth: f32,
    /// Rescales the drawn Jacobian by the slope variance the faded bands lost, so whitecaps keep
    /// their share of the surface with distance (ADR-2843; off by default).
    pub whitecap_rescale: bool,
}

impl Default for FftOcean {
    fn default() -> Self {
        Self {
            seed: 1,
            spectrum: OceanSpectrum::Jonswap,
            wind_speed: 12.0,
            wind_direction: [1.0, 0.0],
            fetch: 300_000.0,
            gamma: 3.3,
            spread: 8.0,
            choppiness: 0.9,
            height_scale: 1.0,
            tiles: [1009.0, 257.0, 67.0],
            crest_foam: 1.0,
            foam_onset: 0.15,
            shore_start: 24.0,
            shore_end: 3.0,
            grid: 0,
            foam_decay: 0.0,
            foam_growth: super::ocean_foam::DEFAULT_FOAM_GROWTH,
            whitecap_rescale: false,
        }
    }
}

impl FftOcean {
    /// Checks every field.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        range("ocean.wind_speed", self.wind_speed, 0.5, 40.0)?;
        let [x, z] = self.wind_direction;
        if !(x.is_finite() && z.is_finite()) {
            return Err(WaterBodyError::NotFinite("ocean.wind_direction"));
        }
        if x * x + z * z < 1.0e-12 {
            return Err(WaterBodyError::NotPositive("ocean.wind_direction"));
        }
        range("ocean.fetch", self.fetch, 1.0e3, 2.0e6)?;
        range("ocean.gamma", self.gamma, 1.0, 7.0)?;
        range("ocean.spread", self.spread, 1.0, 64.0)?;
        range("ocean.choppiness", self.choppiness, 0.0, 2.0)?;
        range("ocean.height_scale", self.height_scale, 0.0, 4.0)?;
        for tile in self.tiles {
            range("ocean.tiles", tile, 8.0, 20_000.0)?;
        }
        if self.tiles[0] < 2.0 * self.tiles[1] || self.tiles[1] < 2.0 * self.tiles[2] {
            return Err(WaterBodyError::OceanTiles);
        }
        range("ocean.crest_foam", self.crest_foam, 0.0, 4.0)?;
        range("ocean.foam_onset", self.foam_onset, 0.0, 1.0)?;
        range("ocean.shore_end", self.shore_end, 0.0, 1000.0)?;
        range("ocean.shore_start", self.shore_start, self.shore_end, 1000.0)?;
        if self.grid != 0 && !OCEAN_GRIDS.contains(&self.grid) {
            return Err(WaterBodyError::OceanGrid(self.grid));
        }
        range(
            "ocean.foam_decay",
            self.foam_decay,
            0.0,
            super::ocean_foam::FOAM_DECAY_MAX,
        )?;
        range(
            "ocean.foam_growth",
            self.foam_growth,
            0.0,
            super::ocean_foam::FOAM_GROWTH_MAX,
        )?;
        Ok(())
    }

    /// Texels per side of the maps this ocean draws on `tier`: 0 on a tier that draws none,
    /// [`Self::grid`] when it is one of [`OCEAN_GRIDS`], the tier's size otherwise. A body whose
    /// grid fails [`Self::validate`] is not drawn at all, like any invalid body.
    #[must_use]
    pub fn drawn_size(&self, tier: WaterTier) -> u32 {
        self.size_on(tier.ocean_size())
    }

    /// [`Self::drawn_size`] from the tier's size (0: none).
    pub(super) fn size_on(&self, tier_size: u32) -> u32 {
        if tier_size == 0 {
            0
        } else if OCEAN_GRIDS.contains(&self.grid) {
            self.grid
        } else {
            tier_size
        }
    }

    /// The unit direction the wind blows toward (`+x` for a zero direction).
    #[must_use]
    pub fn unit_wind(&self) -> [f64; 2] {
        let [x, z] = self.wind_direction.map(f64::from);
        let len = (x * x + z * z).sqrt();
        if len.is_finite() && len > 1.0e-6 {
            [x / len, z / len]
        } else {
            [1.0, 0.0]
        }
    }

    /// The angular frequency of the JONSWAP peak, `ω_p = 22 (g² / (U F))^(1/3)`, or of a fully
    /// developed Phillips sea, `sqrt(g / L)` with `L = U² / g` (radians per second).
    #[must_use]
    pub fn peak_frequency(&self) -> f64 {
        let g = f64::from(GRAVITY);
        let u = f64::from(self.wind_speed).max(0.1);
        match self.spectrum {
            OceanSpectrum::Jonswap => 22.0 * (g * g / (u * f64::from(self.fetch).max(1.0))).cbrt(),
            OceanSpectrum::Phillips => g / u,
        }
    }

    /// The wavelength at the spectrum's peak, `2π g / ω_p²` (m).
    #[must_use]
    pub fn peak_wavelength(&self) -> f64 {
        let w = self.peak_frequency();
        TAU64 * f64::from(GRAVITY) / (w * w)
    }

    /// The share of the ocean that stays at water `depth` meters deep (`None`, or outside a
    /// field: 1): 1 from [`Self::shore_start`] down, 0 at [`Self::shore_end`] and over land,
    /// a smoothstep between (mirrors `water_body.wgsl` `ocean_weight`).
    #[must_use]
    pub fn shore_weight(&self, depth: Option<f32>) -> f32 {
        let Some(h) = depth else { return 1.0 };
        if h.is_nan() || h <= 0.0 {
            return 0.0;
        }
        let span = (self.shore_start - self.shore_end).max(1.0e-3);
        let x = ((h - self.shore_end) / span).clamp(0.0, 1.0);
        x * x * (3.0 - 2.0 * x)
    }

    /// The variance density of the spectrum at the wave vector `(kx, kz)` (m⁴): its integral over
    /// the plane is the variance of the height, before [`Self::height_scale`]. Zero at `k = 0`.
    #[must_use]
    pub fn density(&self, kx: f64, kz: f64) -> f64 {
        let k = (kx * kx + kz * kz).sqrt();
        if k.is_nan() || k <= 1.0e-9 {
            return 0.0;
        }
        let g = f64::from(GRAVITY);
        let wind = self.unit_wind();
        let cos_t = ((kx * wind[0] + kz * wind[1]) / k).clamp(-1.0, 1.0);
        match self.spectrum {
            OceanSpectrum::Jonswap => {
                let w = (g * k).sqrt();
                // S(ω) dω over `dω/dk = g / 2ω`, then `dk = d²k / (k dθ)`.
                jonswap(w, self) * spread_density(cos_t, f64::from(self.spread)) * g / (2.0 * w) / k
            }
            OceanSpectrum::Phillips => {
                let l = f64::from(self.wind_speed) * f64::from(self.wind_speed) / g;
                let forward = cos_t.max(0.0);
                let tail = (-1.0 / (k * l).powi(2)).exp();
                0.5 * PHILLIPS_ALPHA * tail * k.powi(-4) * (2.0 / std::f64::consts::PI) * forward * forward
            }
        }
    }

    /// The significant height `4 sqrt(∫ E)` the spectrum asks for (m), [`Self::height_scale`]
    /// included: an integral over `ω` and `θ`, independent of the tiles.
    #[must_use]
    pub fn expected_height(&self) -> f64 {
        let peak = self.peak_frequency();
        // Log-spaced in k around the peak: 0.05..100 times its wave number.
        let k0 = peak * peak / f64::from(GRAVITY);
        let steps = 512;
        let dlog = (2000.0f64).ln() / f64::from(steps);
        let mut sum = 0.0;
        for i in 0..steps {
            let k = k0 * 0.05 * ((f64::from(i) + 0.5) * dlog).exp();
            let mut ring = 0.0;
            let turns = 64;
            for j in 0..turns {
                let t = (f64::from(j) + 0.5) / f64::from(turns) * TAU64;
                ring += self.density(k * t.cos(), k * t.sin());
            }
            sum += ring * TAU64 / f64::from(turns) * k * k * dlog;
        }
        4.0 * sum.sqrt() * f64::from(self.height_scale)
    }

    /// The trochoidal stand-in for this sea: [`Swell::open_sea`] downwind with the same
    /// significant height and peak wavelength, and this ocean's foam. Low draws it, and the shore
    /// blend uses it on a body without a swell of its own.
    #[must_use]
    pub fn stand_in_swell(&self) -> Swell {
        // A sum of sines of heights H has variance Σ H² / 8, so 4σ = √2 times the root sum square.
        let rss = self.expected_height() / std::f64::consts::SQRT_2;
        let wind = self.unit_wind();
        let mut swell = Swell::open_sea(
            [wind[0] as f32, wind[1] as f32],
            rss as f32,
            (self.peak_wavelength() as f32).clamp(0.25, 2000.0),
            STAND_IN_WAVES,
        );
        swell.crest_foam = self.crest_foam;
        swell.foam_onset = self.foam_onset;
        swell
    }
}

/// The Phillips constant `α`.
const PHILLIPS_ALPHA: f64 = 0.0081;

/// JONSWAP `S(ω)` (m² s) as Hasselmann et al. 1973 give it: `α g² ω⁻⁵ exp(−5/4 (ω_p / ω)⁴) γ^r`,
/// with `α = 0.076 (U² / (F g))^0.22` and the peak width `σ` 0.07 under the peak, 0.09 over it.
fn jonswap(w: f64, ocean: &FftOcean) -> f64 {
    let g = f64::from(GRAVITY);
    let u = f64::from(ocean.wind_speed).max(0.1);
    let alpha = 0.076 * (u * u / (f64::from(ocean.fetch).max(1.0) * g)).powf(0.22);
    let peak = ocean.peak_frequency();
    let sigma = if w <= peak { 0.07 } else { 0.09 };
    let r = (-(w - peak).powi(2) / (2.0 * sigma * sigma * peak * peak)).exp();
    let ratio = peak / w;
    alpha * g * g / w.powi(5) * (-1.25 * ratio.powi(4)).exp() * f64::from(ocean.gamma).max(1.0).powf(r)
}

/// `D(θ) = Q cos^(2s)(θ / 2)` around the wind, normalized over the circle, from `cos θ`. `Q`
/// by the closed form of the Beta function, through `ln Γ` (Stirling, shifted past 8).
fn spread_density(cos_t: f64, s: f64) -> f64 {
    let half_cos_sq = (0.5 * (1.0 + cos_t)).max(0.0);
    let q = (2.0 * ln_gamma(s + 1.0) + (2.0 * s - 1.0) * std::f64::consts::LN_2 - ln_gamma(2.0 * s + 1.0))
        .exp()
        / std::f64::consts::PI;
    q * half_cos_sq.powf(s)
}

/// `ln Γ(x)` for `x > 0`: the recurrence up past 8, then Stirling's series.
fn ln_gamma(x: f64) -> f64 {
    let mut x = x;
    let mut shift = 0.0;
    while x < 8.0 {
        shift += x.ln();
        x += 1.0;
    }
    let inv = 1.0 / x;
    let inv2 = inv * inv;
    (x - 0.5) * x.ln() - x + 0.5 * TAU64.ln() + inv * (1.0 / 12.0 - inv2 * (1.0 / 360.0 - inv2 / 1260.0))
        - shift
}

/// A 32-bit mix (the PCG output function; Jarzynski and Olano 2020).
fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// A unit complex Gaussian of the mode `(nx, nz)` of `band`: `⟨|ξ|²⟩ = 1`. Box-Muller from two
/// hashes, so it depends on the seed, the band and the mode only.
fn mode_gaussian(seed: u32, band: u32, nx: i32, nz: i32) -> [f32; 2] {
    let a = pcg(seed ^ pcg(band ^ pcg(nx as u32 ^ pcg(nz as u32))));
    let b = pcg(a);
    let u = (f64::from(a) + 0.5) / 4_294_967_296.0;
    let v = f64::from(b) / 4_294_967_296.0;
    let r = (-u.ln()).sqrt();
    let th = TAU64 * v;
    [(r * th.cos()) as f32, (r * th.sin()) as f32]
}

/// The whole multiple of `2π / T` nearest `ω = sqrt(g k)`, at least 1.
#[must_use]
pub fn quantized_frequency(k: f64) -> u32 {
    let omega = (f64::from(GRAVITY) * k.max(0.0)).sqrt();
    ((omega * OCEAN_LOOP_SECONDS / TAU64).round() as u32).max(1)
}

/// The loop fraction of `seconds`, `(t / T) mod 1`, in units of 2⁻³². A non-finite time is 0.
#[must_use]
pub fn loop_fraction(seconds: f64) -> u32 {
    let t = if seconds.is_finite() { seconds } else { 0.0 };
    ((t / OCEAN_LOOP_SECONDS).rem_euclid(1.0) * 4_294_967_296.0) as u32
}

/// One non-empty mode of a band's spectrum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OceanMode {
    /// Index in the band's grid: `row · size + column`, row along z, column along x.
    pub index: u32,
    /// Wave vector along x and z (radians per meter).
    pub k: [f32; 2],
    /// `1 / |k|`.
    pub inv_k: f32,
    /// `ω` in multiples of `2π / T`.
    pub n: u32,
    /// `a(k)`: the wave running along `+k`.
    pub a: [f32; 2],
    /// `conj a(−k)`: the wave running along `−k`.
    pub b: [f32; 2],
}

/// One band's share of the spectrum ([`OceanTables`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OceanBand {
    /// The tile (m).
    pub tile: f64,
    /// The wave numbers it holds: `lo <= |k| < hi` (radians per meter).
    pub limits: [f64; 2],
    /// The non-empty modes.
    pub modes: Vec<OceanMode>,
    /// The height variance the band draws for this seed (m²).
    pub variance: f64,
    /// Its mean square slope, `Σ k² |ĥ|²`: what a pixel loses when it fades the band.
    pub slope_variance: f64,
    /// `sqrt(Σ k² |ĥ|² / Σ |ĥ|²)`: the wave number the grid fades the band by (radians per meter).
    pub rms_k: f64,
}

/// The spectrum of an [`FftOcean`] on grids of `size` texels: built once ([`tables_for`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OceanTables {
    /// The ocean these tables were built from.
    pub ocean: FftOcean,
    /// Texels per side.
    pub size: u32,
    /// The bands, longest tile first.
    pub bands: [OceanBand; OCEAN_BANDS],
    /// The swell the shore blend falls to on a body without one.
    pub stand_in: Swell,
    /// A strict bound on the height: `Σ (|a| + |b|)` of every mode (m).
    pub rise_bound: f32,
    /// A strict bound on the horizontal offset, `λ Σ (|a| + |b|)` (m).
    pub shift_bound: f32,
    /// `cos(2π j / size)`, `j < size / 2`.
    twiddle_cos: Vec<f32>,
    /// `sin(2π j / size)`, `j < size / 2`.
    twiddle_sin: Vec<f32>,
}

impl OceanTables {
    /// Where the bands of `tiles` split at [`OCEAN_QUERY_SIZE`] texels: band `i` holds `|k|` in
    /// the `i`-th pair; the split is the band's own short end at [`BAND_TEXELS_PER_WAVE`] texels
    /// per wavelength, and the last band is open (its grid's Nyquist limit ends it). The splits
    /// use the query grid at every map size, so Ultra adds only finer modes to the last band.
    #[must_use]
    pub fn band_limits(tiles: [f32; OCEAN_BANDS]) -> [[f64; 2]; OCEAN_BANDS] {
        let split =
            |tile: f32| TAU64 / (BAND_TEXELS_PER_WAVE * f64::from(tile) / f64::from(OCEAN_QUERY_SIZE));
        let a = split(tiles[0]);
        let b = split(tiles[1]);
        [[0.0, a], [a, b], [b, f64::INFINITY]]
    }

    /// Builds the tables of `ocean` on grids of `size` texels (a power of two, 16 or more).
    #[must_use]
    pub fn new(ocean: &FftOcean, size: u32) -> Self {
        let _span = tracing::info_span!("water_ocean_tables", size).entered();
        let limits = Self::band_limits(ocean.tiles);
        let bands = [0usize, 1, 2].map(|b| build_band(ocean, b, limits[b], size));
        let bound: f32 = bands
            .iter()
            .flat_map(|b| &b.modes)
            .map(|m| m.a[0].hypot(m.a[1]) + m.b[0].hypot(m.b[1]))
            .sum();
        let half = (size / 2) as usize;
        let angle = |j: usize| TAU64 * j as f64 / f64::from(size);
        Self {
            ocean: *ocean,
            size,
            bands,
            stand_in: ocean.stand_in_swell(),
            rise_bound: bound,
            shift_bound: bound * ocean.choppiness,
            twiddle_cos: (0..half).map(|j| angle(j).cos() as f32).collect(),
            twiddle_sin: (0..half).map(|j| angle(j).sin() as f32).collect(),
        }
    }

    /// The significant height of this seed's sea, `4 sqrt(Σ variance)` (m).
    #[must_use]
    pub fn significant_height(&self) -> f64 {
        4.0 * self.bands.iter().map(|b| b.variance).sum::<f64>().sqrt()
    }

    /// `(cos, sin)` tables of the FFT's twiddles, `2π j / size` for `j < size / 2`.
    #[must_use]
    pub fn twiddles(&self) -> (&[f32], &[f32]) {
        (&self.twiddle_cos, &self.twiddle_sin)
    }
}

fn build_band(ocean: &FftOcean, band: usize, limits: [f64; 2], size: u32) -> OceanBand {
    let n = size as i32;
    let tile = f64::from(ocean.tiles[band]);
    let dk = TAU64 / tile;
    let top = n / 2;
    let scale = f64::from(ocean.height_scale);
    let wind_seed = ocean.seed;
    let amplitude = |nx: i32, nz: i32| {
        let (kx, kz) = (f64::from(nx) * dk, f64::from(nz) * dk);
        let density = ocean.density(kx, kz);
        if density <= 0.0 {
            return [0.0f32, 0.0];
        }
        let xi = mode_gaussian(wind_seed, band as u32, nx, nz);
        // ⟨|a|²⟩ = E Δk² / 2: this wave and its conjugate carry the variance E Δk².
        let a = (0.5 * density).sqrt() * dk * scale;
        [xi[0] * a as f32, xi[1] * a as f32]
    };
    let mut modes = Vec::new();
    let (mut variance, mut slope, mut k2_sum) = (0.0f64, 0.0f64, 0.0f64);
    for row in 0..n {
        let nz = if row < top { row } else { row - n };
        for column in 0..n {
            let nx = if column < top { column } else { column - n };
            // The Nyquist row and column have no conjugate: left empty, like the mean.
            if nx == -top || nz == -top || (nx == 0 && nz == 0) {
                continue;
            }
            let k = dk * f64::from(nx * nx + nz * nz).sqrt();
            if k < limits[0] || k >= limits[1] {
                continue;
            }
            let a = amplitude(nx, nz);
            let m = amplitude(-nx, -nz);
            if a == [0.0, 0.0] && m == [0.0, 0.0] {
                continue;
            }
            let power = f64::from(a[0] * a[0] + a[1] * a[1] + m[0] * m[0] + m[1] * m[1]);
            variance += power;
            slope += power * k * k;
            k2_sum += power * k * k;
            modes.push(OceanMode {
                index: (row * n + column) as u32,
                k: [(dk * f64::from(nx)) as f32, (dk * f64::from(nz)) as f32],
                inv_k: (1.0 / k) as f32,
                n: quantized_frequency(k),
                a,
                b: [m[0], -m[1]],
            });
        }
    }
    OceanBand {
        tile,
        limits,
        modes,
        variance,
        slope_variance: slope,
        rms_k: if variance > 0.0 {
            (k2_sum / variance).sqrt()
        } else {
            0.0
        },
    }
}

/// The fields of one band of an [`OceanFrame`], each `size²` values, row along z.
#[derive(Debug, Clone, PartialEq)]
pub struct OceanGrid {
    /// Offset along x.
    pub dx: Vec<f32>,
    /// Height.
    pub h: Vec<f32>,
    /// Offset along z.
    pub dz: Vec<f32>,
    /// `∂Dx/∂z = ∂Dz/∂x`.
    pub dxz: Vec<f32>,
    /// `∂h/∂x`.
    pub hx: Vec<f32>,
    /// `∂h/∂z`.
    pub hz: Vec<f32>,
    /// `∂Dx/∂x`.
    pub dxx: Vec<f32>,
    /// `∂Dz/∂z`.
    pub dzz: Vec<f32>,
    /// Particle velocity along x (m/s).
    pub vx: Vec<f32>,
    /// Particle velocity along y (m/s).
    pub vy: Vec<f32>,
    /// Particle velocity along z (m/s).
    pub vz: Vec<f32>,
    /// Unused half of the last transform.
    spare: Vec<f32>,
}

impl OceanGrid {
    fn new(texels: usize) -> Self {
        Self {
            dx: vec![0.0; texels],
            h: vec![0.0; texels],
            dz: vec![0.0; texels],
            dxz: vec![0.0; texels],
            hx: vec![0.0; texels],
            hz: vec![0.0; texels],
            dxx: vec![0.0; texels],
            dzz: vec![0.0; texels],
            vx: vec![0.0; texels],
            vy: vec![0.0; texels],
            vz: vec![0.0; texels],
            spare: vec![0.0; texels],
        }
    }

    /// The transforms' `(re, im)` pairs.
    fn pairs(&mut self) -> [(&mut [f32], &mut [f32]); 6] {
        [
            (self.dx.as_mut_slice(), self.h.as_mut_slice()),
            (self.dz.as_mut_slice(), self.dxz.as_mut_slice()),
            (self.hx.as_mut_slice(), self.hz.as_mut_slice()),
            (self.dxx.as_mut_slice(), self.dzz.as_mut_slice()),
            (self.vx.as_mut_slice(), self.vy.as_mut_slice()),
            (self.vz.as_mut_slice(), self.spare.as_mut_slice()),
        ]
    }
}

/// The CPU mirror of the compute pass: the ocean's maps at one instant ([`Self::at`]).
#[derive(Debug, Clone, PartialEq)]
pub struct OceanFrame {
    /// The spectrum.
    pub tables: Arc<OceanTables>,
    /// The loop fraction ([`loop_fraction`]).
    pub tau: u32,
    /// The bands' maps.
    pub grids: [OceanGrid; OCEAN_BANDS],
    /// The highest `|h|` of each band's grid (m): the bilinear surface never exceeds it.
    pub rise: [f32; OCEAN_BANDS],
    /// The longest `|D|` of each band's grid (m).
    pub shift: [f32; OCEAN_BANDS],
}

/// The phase rotation of a mode at the loop fraction `tau`: `(cos ωt, sin ωt)`.
#[must_use]
pub fn mode_phasor(n: u32, tau: u32) -> (f32, f32) {
    let theta = n.wrapping_mul(tau) as f32 * PHASE_SCALE;
    (theta.cos(), theta.sin())
}

impl OceanFrame {
    /// Evolves `tables` to the loop fraction `tau` and transforms them, the CPU mirror of
    /// `water_ocean.wgsl`.
    #[must_use]
    pub fn at(tables: &Arc<OceanTables>, tau: u32) -> Self {
        let texels = (tables.size * tables.size) as usize;
        let mut frame = Self {
            tables: Arc::clone(tables),
            tau,
            grids: [0, 1, 2].map(|_| OceanGrid::new(texels)),
            rise: [0.0; OCEAN_BANDS],
            shift: [0.0; OCEAN_BANDS],
        };
        frame.evolve();
        frame
    }

    /// Fills this frame's buffers for the loop fraction `tau` on `tables` (same size).
    fn reuse(mut self, tables: &Arc<OceanTables>, tau: u32) -> Self {
        self.tables = Arc::clone(tables);
        self.tau = tau;
        self.evolve();
        self
    }

    fn evolve(&mut self) {
        let _span = tracing::info_span!("water_ocean_mirror").entered();
        #[cfg(test)]
        EVOLVES.with(|c| c.set(c.get().saturating_add(1)));
        let tables = Arc::clone(&self.tables);
        let size = tables.size as usize;
        let (tc, ts) = tables.twiddles();
        let chop = tables.ocean.choppiness;
        // One scratch buffer per thread, grown to the grid. The transpose copies into a slice of
        // the same length as the field, so the closure sees exactly `size * size` floats.
        with_scratch(size * size, |scratch| {
            self.evolve_bands(&tables, size, tc, ts, chop, scratch);
        });
    }

    fn evolve_bands(
        &mut self,
        tables: &OceanTables,
        size: usize,
        tc: &[f32],
        ts: &[f32],
        chop: f32,
        scratch: &mut [f32],
    ) {
        for (b, (band, grid)) in tables.bands.iter().zip(&mut self.grids).enumerate() {
            let [
                (c0r, c0i),
                (c1r, c1i),
                (c2r, c2i),
                (c3r, c3i),
                (c4r, c4i),
                (c5r, c5i),
            ] = grid.pairs();
            for buffer in [
                &mut *c0r, &mut *c0i, &mut *c1r, &mut *c1i, &mut *c2r, &mut *c2i, &mut *c3r, &mut *c3i,
                &mut *c4r, &mut *c4i, &mut *c5r, &mut *c5i,
            ] {
                buffer.fill(0.0);
            }
            for m in &band.modes {
                let (c, s) = mode_phasor(m.n, self.tau);
                let i = m.index as usize;
                // ĥ = a e^(−iθ) + b e^(iθ).
                let hr = m.a[0] * c + m.a[1] * s + m.b[0] * c - m.b[1] * s;
                let hi = m.a[1] * c - m.a[0] * s + m.b[1] * c + m.b[0] * s;
                // ∂ĥ/∂t = iω (b e^(iθ) − a e^(−iθ)).
                let omega = (f64::from(m.n) * TAU64 / OCEAN_LOOP_SECONDS) as f32;
                let tr = m.b[0] * c - m.b[1] * s - m.a[0] * c - m.a[1] * s;
                let ti = m.b[1] * c + m.b[0] * s - m.a[1] * c + m.a[0] * s;
                let (vr, vi) = (-omega * ti, omega * tr);
                let (ux, uz) = (m.k[0] * m.inv_k, m.k[1] * m.inv_k);
                // Each spectrum `A + i B` of two real fields, with `i ĥ` = (−hi, hr).
                let (dxr, dxi) = (-chop * ux * hi, chop * ux * hr);
                let (dzr, dzi) = (-chop * uz * hi, chop * uz * hr);
                let q = -chop * m.inv_k;
                let (xz, xx, zz) = (q * m.k[0] * m.k[1], q * m.k[0] * m.k[0], q * m.k[1] * m.k[1]);
                c0r[i] = dxr - hi;
                c0i[i] = dxi + hr;
                c1r[i] = dzr - xz * hi;
                c1i[i] = dzi + xz * hr;
                let (hxr, hxi) = (-m.k[0] * hi, m.k[0] * hr);
                let (hzr, hzi) = (-m.k[1] * hi, m.k[1] * hr);
                c2r[i] = hxr - hzi;
                c2i[i] = hxi + hzr;
                c3r[i] = xx * hr - zz * hi;
                c3i[i] = xx * hi + zz * hr;
                // Horizontal velocity: `i λ (k / |k|) ∂ĥ/∂t`.
                let (vxr, vxi) = (-chop * ux * vi, chop * ux * vr);
                let (vzr, vzi) = (-chop * uz * vi, chop * uz * vr);
                c4r[i] = vxr - vi;
                c4i[i] = vxi + vr;
                c5r[i] = vzr;
                c5i[i] = vzi;
            }
            for (re, im) in [
                (&mut *c0r, &mut *c0i),
                (&mut *c1r, &mut *c1i),
                (&mut *c2r, &mut *c2i),
                (&mut *c3r, &mut *c3i),
                (&mut *c4r, &mut *c4i),
                (&mut *c5r, &mut *c5i),
            ] {
                inverse_fft_2d(re, im, size, tc, ts, scratch);
            }
            let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            self.rise[b] = peak(&grid.h);
            self.shift[b] = peak(&grid.dx).hypot(peak(&grid.dz));
        }
    }

    /// The tiling surface at the rest point `(x, z)`: offset, tangents, velocity and Jacobian,
    /// every band bilinear between texels (mirrors `water_body.wgsl` `ocean_frame`, unfaded).
    #[must_use]
    pub fn sample(&self, x: f32, z: f32) -> SwellSample {
        let (x, z) = (f64::from(x), f64::from(z));
        let mut offset = [0.0f32; 3];
        let mut dpdx = [1.0f32, 0.0, 0.0];
        let mut dpdz = [0.0f32, 0.0, 1.0];
        let mut velocity = [0.0f32; 3];
        for (band, grid) in self.tables.bands.iter().zip(&self.grids) {
            let c = BilinearCell::new(x, z, band.tile, self.tables.size);
            offset[0] += c.of(&grid.dx);
            offset[1] += c.of(&grid.h);
            offset[2] += c.of(&grid.dz);
            let xz = c.of(&grid.dxz);
            dpdx[0] += c.of(&grid.dxx);
            dpdx[1] += c.of(&grid.hx);
            dpdx[2] += xz;
            dpdz[0] += xz;
            dpdz[1] += c.of(&grid.hz);
            dpdz[2] += c.of(&grid.dzz);
            velocity[0] += c.of(&grid.vx);
            velocity[1] += c.of(&grid.vy);
            velocity[2] += c.of(&grid.vz);
        }
        SwellSample {
            offset,
            dpdx,
            dpdz,
            velocity,
            jacobian: dpdx[0] * dpdz[2] - dpdx[2] * dpdz[0],
        }
    }

    /// The displacement the ring grid gives the rest point `(x, z)` on cells of `cell` meters:
    /// each band faded by [`crate::water::vertex_wave_fade`] of its [`OceanBand::rms_k`] (mirrors
    /// `water_body.wgsl` `ocean_offset`). `cell = 0` keeps every band.
    #[must_use]
    pub fn grid_offset(&self, x: f64, z: f64, cell: f32) -> [f32; 3] {
        let mut offset = [0.0f32; 3];
        for (band, grid) in self.tables.bands.iter().zip(&self.grids) {
            let fade = crate::water::vertex_wave_fade(cell, band.rms_k as f32);
            if fade <= 0.0 {
                continue;
            }
            let c = BilinearCell::new(x, z, band.tile, self.tables.size);
            offset[0] += fade * c.of(&grid.dx);
            offset[1] += fade * c.of(&grid.h);
            offset[2] += fade * c.of(&grid.dz);
        }
        offset
    }

    /// The largest `|h|` the surface reaches at this instant (m).
    #[must_use]
    pub fn max_rise(&self) -> f32 {
        self.rise.iter().sum()
    }

    /// The largest `|D|` the surface reaches at this instant (m).
    #[must_use]
    pub fn max_shift(&self) -> f32 {
        self.shift.iter().sum()
    }

    /// The share of texels of the query surface whose Jacobian is below `limit`, over the
    /// longest tile on a `samples`² lattice.
    #[must_use]
    pub fn compressed_share(&self, limit: f32, samples: u32) -> f32 {
        let tile = self.tables.bands[0].tile as f32;
        let n = samples.max(1);
        let step = tile / n as f32;
        let hits = (0..n)
            .flat_map(|j| (0..n).map(move |i| (i, j)))
            .filter(|(i, j)| self.sample(*i as f32 * step, *j as f32 * step).jacobian < limit)
            .count();
        hits as f32 / (n * n) as f32
    }
}

/// A bilinear footprint in a tiling grid.
#[derive(Debug, Clone, Copy)]
pub(super) struct BilinearCell {
    /// Indices of the four texels, in the order (0,0), (1,0), (0,1), (1,1).
    at: [usize; 4],
    weight: [f32; 4],
}

impl BilinearCell {
    /// The cell of `(x, z)` in a tile of `tile` meters on `size` texels: texel `i` is at
    /// `i · tile / size`, and the grid wraps.
    #[inline]
    pub(super) fn new(x: f64, z: f64, tile: f64, size: u32) -> Self {
        let n = f64::from(size);
        let u = (x / tile).rem_euclid(1.0) * n;
        let v = (z / tile).rem_euclid(1.0) * n;
        let (i0, j0) = (u.floor(), v.floor());
        let (fu, fv) = ((u - i0) as f32, (v - j0) as f32);
        let mask = size as usize - 1;
        let (i0, j0) = (i0 as usize & mask, j0 as usize & mask);
        let (i1, j1) = ((i0 + 1) & mask, (j0 + 1) & mask);
        let stride = size as usize;
        Self {
            at: [
                j0 * stride + i0,
                j0 * stride + i1,
                j1 * stride + i0,
                j1 * stride + i1,
            ],
            weight: [(1.0 - fu) * (1.0 - fv), fu * (1.0 - fv), (1.0 - fu) * fv, fu * fv],
        }
    }

    #[inline]
    pub(super) fn of(&self, field: &[f32]) -> f32 {
        self.at
            .iter()
            .zip(self.weight)
            .map(|(i, w)| field.get(*i).copied().unwrap_or(0.0) * w)
            .sum()
    }
}

/// The inverse DFT `f[j, i] = Σ F[r, c] e^(2πi (r j + c i) / n)` of a square `n × n` grid in place,
/// radix 2. Each pass runs over whole lines, so the inner loop vectorizes; a transposition between
/// the passes turns the rows into columns.
fn inverse_fft_2d(re: &mut [f32], im: &mut [f32], n: usize, tc: &[f32], ts: &[f32], scratch: &mut [f32]) {
    inverse_fft_lines(re, im, n, tc, ts);
    transpose(re, n, scratch);
    transpose(im, n, scratch);
    inverse_fft_lines(re, im, n, tc, ts);
    transpose(re, n, scratch);
    transpose(im, n, scratch);
}

fn transpose(field: &mut [f32], n: usize, scratch: &mut [f32]) {
    scratch.copy_from_slice(field);
    for (j, row) in field.chunks_exact_mut(n).enumerate() {
        for (i, out) in row.iter_mut().enumerate() {
            *out = scratch[i * n + j];
        }
    }
}

/// One-dimensional inverse FFT along the first index: Cooley-Tukey decimation in time, with whole
/// rows as the elements.
fn inverse_fft_lines(re: &mut [f32], im: &mut [f32], n: usize, tc: &[f32], ts: &[f32]) {
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            for field in [&mut *re, &mut *im] {
                let (low, high) = field.split_at_mut(j * n);
                low[i * n..(i + 1) * n].swap_with_slice(&mut high[..n]);
            }
        }
    }
    let mut half = 1usize;
    while half < n {
        let step = n / (2 * half);
        for start in (0..n).step_by(2 * half) {
            for offset in 0..half {
                let (wr, wi) = (tc[offset * step], ts[offset * step]);
                let u = (start + offset) * n;
                let v = (start + offset + half) * n;
                let (re_lo, re_hi) = re.split_at_mut(v);
                let (im_lo, im_hi) = im.split_at_mut(v);
                let lines = re_lo[u..u + n]
                    .iter_mut()
                    .zip(&mut im_lo[u..u + n])
                    .zip(re_hi[..n].iter_mut().zip(&mut im_hi[..n]));
                for ((ar, ai), (br, bi)) in lines {
                    let tr = wr * *br - wi * *bi;
                    let ti = wr * *bi + wi * *br;
                    *br = *ar - tr;
                    *bi = *ai - ti;
                    *ar += tr;
                    *ai += ti;
                }
            }
        }
        half *= 2;
    }
}

type TableKey = (FftOcean, u32);

#[derive(Default)]
struct Memo {
    /// Newest last.
    tables: Vec<(TableKey, Arc<OceanTables>)>,
    /// Newest last.
    frames: Vec<(TableKey, u32, Arc<OceanFrame>)>,
}

/// Memo of the pure functions [`tables_for`] and [`frame_at`]: bounded, and invisible except in
/// time, since an entry is a function of its key.
static MEMO: LazyLock<Mutex<Memo>> = LazyLock::new(Mutex::default);

#[cfg(test)]
thread_local! {
    static EVOLVES: Cell<u64> = const { Cell::new(0) };
    static SCRATCH_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static MEMO_LOCKS: Cell<u64> = const { Cell::new(0) };
}

// Scratch for `inverse_fft_2d`'s transpose, one buffer per thread.
thread_local! {
    static SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// The query-grid frame this thread last built, so a later sample of the same tick skips the memo.
struct TickFrame {
    ocean: FftOcean,
    tau: u32,
    frame: Arc<OceanFrame>,
}

thread_local! {
    static TICK_FRAME: RefCell<Option<TickFrame>> = const { RefCell::new(None) };
}

fn with_scratch(n: usize, f: impl FnOnce(&mut [f32])) {
    SCRATCH.with(|slot| {
        let mut buf = slot.borrow_mut();
        if buf.len() < n {
            #[cfg(test)]
            SCRATCH_ALLOCS.with(|c| c.set(c.get().saturating_add(1)));
            buf.resize(n, 0.0);
        }
        f(&mut buf[..n]);
    });
}

fn tls_frame(ocean: &FftOcean, tau: u32) -> Option<Arc<OceanFrame>> {
    TICK_FRAME.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|tick| tick.tau == tau && tick.ocean == *ocean)
            .map(|tick| Arc::clone(&tick.frame))
    })
}

fn store_tick_frame(ocean: &FftOcean, tau: u32, frame: &Arc<OceanFrame>) {
    TICK_FRAME.with(|slot| {
        *slot.borrow_mut() = Some(TickFrame {
            ocean: *ocean,
            tau,
            frame: Arc::clone(frame),
        });
    });
}

fn clear_tick_frame() {
    TICK_FRAME.with(|slot| *slot.borrow_mut() = None);
}

fn memo_lock() -> std::sync::MutexGuard<'static, Memo> {
    #[cfg(test)]
    MEMO_LOCKS.with(|c| c.set(c.get().saturating_add(1)));
    MEMO.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn test_reset_ocean_counts() {
    EVOLVES.with(|c| c.set(0));
    SCRATCH_ALLOCS.with(|c| c.set(0));
    MEMO_LOCKS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn test_ocean_evolves() -> u64 {
    EVOLVES.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn test_ocean_scratch_allocs() -> u64 {
    SCRATCH_ALLOCS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn test_ocean_memo_locks() -> u64 {
    MEMO_LOCKS.with(|c| c.get())
}

/// The spectrum tables of `ocean` on grids of `size` texels (built once, kept in a small memo).
#[must_use]
pub fn tables_for(ocean: &FftOcean, size: u32) -> Arc<OceanTables> {
    let key = (*ocean, size);
    let found = {
        let memo = memo_lock();
        memo.tables
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, t)| Arc::clone(t))
    };
    if let Some(tables) = found {
        return tables;
    }
    let tables = Arc::new(OceanTables::new(ocean, size));
    let mut memo = memo_lock();
    if memo.tables.len() >= TABLE_MEMO {
        memo.tables.remove(0);
    }
    memo.tables.push((key, Arc::clone(&tables)));
    tables
}

/// The ocean's maps at `seconds` on grids of `size` texels: transformed by the first caller of a
/// tick, shared after that.
#[must_use]
pub fn frame_of(ocean: &FftOcean, seconds: f64, size: u32) -> Arc<OceanFrame> {
    let tau = loop_fraction(seconds);
    let key = (*ocean, size);
    let (found, spare) = {
        let mut memo = memo_lock();
        let found = memo
            .frames
            .iter()
            .find(|(k, t, _)| *t == tau && *k == key)
            .map(|(_, _, f)| Arc::clone(f));
        // A full memo gives up its oldest frame of this size: its buffers are reused.
        let spare = if found.is_none() && memo.frames.len() >= FRAME_MEMO {
            let at = memo.frames.iter().position(|(k, _, _)| k.1 == size).unwrap_or(0);
            Some(memo.frames.remove(at).2)
        } else {
            None
        };
        (found, spare)
    };
    if let Some(frame) = found {
        return frame;
    }
    let tables = tables_for(ocean, size);
    let frame = match spare.and_then(|s| Arc::try_unwrap(s).ok()) {
        Some(old) if old.tables.size == size => old.reuse(&tables, tau),
        _ => OceanFrame::at(&tables, tau),
    };
    let frame = Arc::new(frame);
    let mut memo = memo_lock();
    if memo.frames.len() >= FRAME_MEMO {
        memo.frames.remove(0);
    }
    memo.frames.push((key, tau, Arc::clone(&frame)));
    frame
}

/// The ocean's maps at `seconds` on the query grid ([`OCEAN_QUERY_SIZE`]).
///
/// The first call of a tick on this thread builds the frame (or reuses one the memo already
/// holds). Later calls of that tick return the thread-local frame and do not lock the memo.
#[must_use]
pub fn frame_at(ocean: &FftOcean, seconds: f64) -> Arc<OceanFrame> {
    let tau = loop_fraction(seconds);
    if let Some(frame) = tls_frame(ocean, tau) {
        return frame;
    }
    // Drop this thread's previous frame before building, so its buffers can be recycled.
    clear_tick_frame();
    let frame = frame_of(ocean, seconds, OCEAN_QUERY_SIZE);
    store_tick_frame(ocean, tau, &frame);
    frame
}

/// A query-grid frame already built for `ocean` at `seconds`, on this thread or in the memo.
/// Does not transform. A memo hit is stored on this thread so the next call skips the lock.
#[must_use]
pub(crate) fn cached_query_frame(ocean: &FftOcean, seconds: f64) -> Option<Arc<OceanFrame>> {
    let tau = loop_fraction(seconds);
    if let Some(frame) = tls_frame(ocean, tau) {
        return Some(frame);
    }
    let key = (*ocean, OCEAN_QUERY_SIZE);
    let found = {
        let memo = memo_lock();
        memo.frames
            .iter()
            .find(|(k, t, _)| *t == tau && *k == key)
            .map(|(_, _, frame)| Arc::clone(frame))
    };
    if let Some(frame) = &found {
        store_tick_frame(ocean, tau, frame);
    }
    found
}

/// `s` and `f` blended: `w f + (1 − w) s`, each term linear in the tangents, the Jacobian taken
/// from the blended tangents.
#[must_use]
pub fn blend_samples(s: &SwellSample, f: &SwellSample, w: f32) -> SwellSample {
    let v = 1.0 - w;
    let mix3 = |a: [f32; 3], b: [f32; 3]| [v * a[0] + w * b[0], v * a[1] + w * b[1], v * a[2] + w * b[2]];
    let dpdx = mix3(s.dpdx, f.dpdx);
    let dpdz = mix3(s.dpdz, f.dpdz);
    SwellSample {
        offset: mix3(s.offset, f.offset),
        dpdx,
        dpdz,
        velocity: mix3(s.velocity, f.velocity),
        jacobian: dpdx[0] * dpdz[2] - dpdx[2] * dpdz[0],
    }
}

/// One point of an ocean body's surface.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OceanPoint {
    sample: SwellSample,
    /// The swell term's foam and breaking ratio, before the blend.
    swell_foam: f32,
    ratio: f32,
    /// The ocean's foam, before the blend.
    ocean_foam: f32,
    /// The ocean's share.
    weight: f32,
}

/// What a plane's waves come from ([`WaterBody::set_wave_mode`], `water.set_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveMode {
    /// The trochoidal swell, if the body has one; a plane without one is calm.
    Swell,
    /// The FFT ocean ([`FftOcean`]).
    Fft,
}

impl WaveMode {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Swell => "swell",
            Self::Fft => "fft",
        }
    }

    /// The mode a protocol name means (`None` when unknown).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "swell" => Some(Self::Swell),
            "fft" => Some(Self::Fft),
            _ => None,
        }
    }
}

impl WaterBody {
    /// What this plane's waves come from now (`None` on a ribbon, which has neither).
    #[must_use]
    pub fn wave_mode(&self) -> Option<WaveMode> {
        match (&self.shape, &self.ocean) {
            (WaterShape::Ribbon { .. }, _) => None,
            (WaterShape::Plane { .. }, Some(_)) => Some(WaveMode::Fft),
            (WaterShape::Plane { .. }, None) => Some(WaveMode::Swell),
        }
    }

    /// Switches this plane between its swell and its FFT ocean, keeping both authored setups.
    ///
    /// [`WaveMode::Swell`] parks the ocean in [`Self::parked_ocean`]: queries and drawing go back
    /// to the body's swell. A body with no swell of its own takes the ocean's stand-in
    /// ([`FftOcean::stand_in_swell`]), so the sea keeps its height and peak wavelength.
    /// [`WaveMode::Fft`] brings the parked ocean back (a body that never had one gets the default)
    /// and drops that stand-in again, so a round trip restores the body exactly.
    ///
    /// # Errors
    ///
    /// [`WaterBodyError::OceanNeedsPlane`] on a ribbon; the body does not change.
    pub fn set_wave_mode(&mut self, mode: WaveMode) -> Result<(), WaterBodyError> {
        if !matches!(self.shape, WaterShape::Plane { .. }) {
            return Err(WaterBodyError::OceanNeedsPlane);
        }
        match mode {
            WaveMode::Swell => {
                if let Some(ocean) = self.ocean.take() {
                    if self.swell.is_none() {
                        self.swell = Some(ocean.stand_in_swell());
                    }
                    self.parked_ocean = Some(ocean);
                }
            }
            WaveMode::Fft => {
                if self.ocean.is_none() {
                    let ocean = self.parked_ocean.take().unwrap_or_default();
                    if self.swell.as_ref() == Some(&ocean.stand_in_swell()) {
                        self.swell = None;
                    }
                    self.ocean = Some(ocean);
                }
            }
        }
        Ok(())
    }

    /// The FFT ocean this body draws and answers queries with: a plane's [`FftOcean`].
    #[must_use]
    pub fn active_ocean(&self) -> Option<&FftOcean> {
        match (&self.shape, &self.ocean) {
            (WaterShape::Plane { .. }, Some(ocean)) => Some(ocean),
            _ => None,
        }
    }

    /// The swell this plane falls to: its own, or the stand-in of its ocean
    /// ([`FftOcean::stand_in_swell`]).
    #[must_use]
    pub fn fallback_swell(&self) -> Option<Cow<'_, Swell>> {
        match (self.active_swell(), self.active_ocean()) {
            (Some(swell), _) => Some(Cow::Borrowed(swell)),
            (None, Some(ocean)) => Some(Cow::Owned(tables_for(ocean, OCEAN_QUERY_SIZE).stand_in.clone())),
            (None, None) => None,
        }
    }

    /// The body as `tier` draws it, for queries that follow the drawn surface
    /// ([`super::WaterSettings::queries_follow_tier`], ADR-2843): a plane whose ocean is not drawn
    /// (Low, or `ocean_draw` off) answers its swell or the ocean's stand-in, and a swell keeps only
    /// the waves the tier draws ([`WaterTier::swell_waves`]). A plane whose ocean is drawn, and
    /// every other body, is itself.
    #[must_use]
    pub fn drawn_for(&self, tier: WaterTier, ocean_draw: bool) -> Cow<'_, WaterBody> {
        self.drawn_with(tier, ocean_draw && tier.ocean_size() > 0)
    }

    /// [`Self::drawn_for`] with the renderer's own answer to "was this body's ocean drawn"
    /// (ADR-3251): `ocean_drawn` false answers the swell fallback even on a tier that draws
    /// oceans, as the renderer does for a grid the device refused or a second spectrum.
    #[must_use]
    pub fn drawn_with(&self, tier: WaterTier, ocean_drawn: bool) -> Cow<'_, WaterBody> {
        if self.active_ocean().is_some() && ocean_drawn {
            return Cow::Borrowed(self);
        }
        let cap = tier.swell_waves();
        let parks = self.active_ocean().is_some();
        let caps = self.active_swell().is_some_and(|s| s.waves.len() > cap);
        if !parks && !caps {
            return Cow::Borrowed(self);
        }
        let mut drawn = self.clone();
        if parks {
            drawn.swell = self.fallback_swell().map(Cow::into_owned);
            drawn.ocean = None;
        }
        if let Some(swell) = drawn.swell.as_mut() {
            swell.waves.truncate(cap);
        }
        Cow::Owned(drawn)
    }

    /// [`Self::surface_on`] of a plane with an ocean: the rest point by the same Newton steps,
    /// on the blended surface.
    pub(super) fn ocean_surface(
        &self,
        ocean: &FftOcean,
        x: f32,
        z: f32,
        clock: &WaterClock,
        shore: Option<&ShoreField>,
    ) -> Option<WaterSurface> {
        let WaterShape::Plane {
            center,
            half_extent,
            flow,
        } = &self.shape
        else {
            return None;
        };
        let seconds = clock.seconds();
        // A rest point inside the rectangle can land at most one shift-bound away. Reject that
        // before the inverse FFT and before Newton. A frame already built for this tick uses its
        // grid peak (still an upper bound on the bilinear displacement); a cold tick uses the
        // strict spectral bound and does not transform on a miss. With a shore the swell is
        // shoaled, and the blend stays inside the larger of the two shifts.
        let cached = cached_query_frame(ocean, seconds);
        let (ocean_shift, stand_in_shift) = match &cached {
            Some(frame) => (frame.max_shift(), frame.tables.stand_in.max_shift()),
            None => {
                let tables = tables_for(ocean, OCEAN_QUERY_SIZE);
                (tables.shift_bound, tables.stand_in.max_shift())
            }
        };
        let swell_shift = if shore.is_some() {
            self.active_swell().map_or(stand_in_shift, Swell::max_shift) * shore::SHOAL_MAX
        } else {
            0.0
        };
        let pad = 1.0e-3;
        let shift = ocean_shift.max(swell_shift) + pad;
        if (x - center[0]).abs() > half_extent[0] + shift || (z - center[2]).abs() > half_extent[1] + shift {
            return None;
        }
        let frame = cached.unwrap_or_else(|| frame_at(ocean, seconds));
        let swell = self.active_swell().unwrap_or(&frame.tables.stand_in);
        // The swell is only asked for where a bed brings it in.
        let instant: Option<SwellInstant> =
            shore.map(|_| swell.instant(seconds, [f64::from(x), f64::from(z)], usize::MAX));
        let bed = |r: [f32; 2]| shore.and_then(|f| f.sample(r[0], r[1]));
        let point = |r: [f32; 2]| -> OceanPoint {
            let depth = bed(r).map(|b| b.depth);
            let weight = ocean.shore_weight(depth);
            let f = frame.sample(r[0], r[1]);
            let ocean_foam = swell::crest_foam(f.jacobian, ocean.crest_foam, ocean.foam_onset);
            let (sample, swell_foam, ratio) = match &instant {
                Some(instant) if weight < 1.0 => {
                    let (s, ratio) = instant.sample_on(r, depth);
                    (blend_samples(&s, &f, weight), instant.foam_at(&s), ratio)
                }
                _ => (f, 0.0, 0.0),
            };
            OceanPoint {
                sample,
                swell_foam,
                ratio,
                ocean_foam,
                weight,
            }
        };
        let (rest, miss) = swell::rest_point_by(x, z, |r| point(r).sample);
        let inside =
            (rest[0] - center[0]).abs() <= half_extent[0] && (rest[1] - center[2]).abs() <= half_extent[1];
        if !inside {
            return None;
        }
        let under = bed(rest);
        if under.is_some_and(|b| b.depth <= 0.0) {
            return None;
        }
        let p = point(rest);
        let v = p.sample.velocity;
        let w = p.weight;
        Some(WaterSurface {
            height: center[1] + p.sample.offset[1],
            normal: p.sample.normal(),
            velocity: [v[0] + flow[0], v[1], v[2] + flow[1]],
            crest_foam: ((1.0 - w) * p.swell_foam + w * p.ocean_foam).clamp(0.0, 1.0),
            rest,
            miss,
            depth: under.map(|b| b.depth),
            shore_distance: under.map(|b| b.distance.max(0.0)),
            breaking: if under.is_some() {
                (1.0 - w) * shore::breaker_foam(p.ratio)
            } else {
                0.0
            },
            shore_foam: under.map_or(0.0, |b| {
                shore::shore_foam(b.distance.max(0.0), self.look.wet_width)
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ocean() -> FftOcean {
        FftOcean::default()
    }

    /// Direct evaluation of a band's height at world `x`, `z` from its modes.
    fn direct_height(frame: &OceanFrame, x: f32, z: f32) -> f32 {
        let mut h = 0.0f64;
        for band in &frame.tables.bands {
            for m in &band.modes {
                let (c, s) = mode_phasor(m.n, frame.tau);
                let (c, s) = (f64::from(c), f64::from(s));
                let hr = f64::from(m.a[0]) * c + f64::from(m.a[1]) * s + f64::from(m.b[0]) * c
                    - f64::from(m.b[1]) * s;
                let hi = f64::from(m.a[1]) * c - f64::from(m.a[0]) * s
                    + f64::from(m.b[1]) * c
                    + f64::from(m.b[0]) * s;
                let phase = f64::from(m.k[0]) * f64::from(x) + f64::from(m.k[1]) * f64::from(z);
                h += hr * phase.cos() - hi * phase.sin();
            }
        }
        h as f32
    }

    /// The constants the shader repeats are pinned here; `ocean_gpu.rs` compares them by text.
    #[test]
    fn constants_are_pinned() {
        assert_eq!(OCEAN_BANDS, 3);
        assert_eq!(OCEAN_QUERY_SIZE, 128);
        assert_eq!(OCEAN_MAX_SIZE, 256);
        assert_eq!(OCEAN_LOOP_SECONDS, 2048.0);
        assert!(OCEAN_QUERY_SIZE.is_power_of_two() && OCEAN_MAX_SIZE.is_power_of_two());
    }

    /// The CPU reference at the large grids is the inverse DFT too: texels of the last band, the
    /// one that gains modes with the grid, equal the direct sum over its modes (ADR-2126).
    #[test]
    fn reference_fft_matches_the_direct_sum_at_the_large_grids() {
        for size in [256u32, 512, 1024] {
            let mut tables = OceanTables::new(&ocean(), size);
            tables.bands[0].modes.clear();
            tables.bands[1].modes.clear();
            let tile = tables.bands[2].tile as f32;
            let tables = Arc::new(tables);
            let frame = OceanFrame::at(&tables, loop_fraction(77.0));
            let cell = tile / size as f32;
            let last = size - 1;
            let mut peak = 0.0f32;
            for (i, j) in [(0, 0), (3, 5), (size / 2 + 1, 2), (last, last / 3)] {
                let got = frame.grids[2].h[(j * size + i) as usize];
                let want = direct_height(&frame, i as f32 * cell, j as f32 * cell);
                assert!((got - want).abs() < 1.0e-3, "{size} ({i},{j}): {got} vs {want}");
                peak = peak.max(want.abs());
            }
            assert!(peak > 1.0e-3, "{size}: the test compared flat maps: {peak}");
        }
    }

    /// A larger grid adds finer modes to the last band only: the long bands keep their modes, and
    /// the last band keeps every mode of the smaller grid (ADR-2126).
    #[test]
    fn larger_grids_add_only_finer_modes_to_the_last_band() {
        let o = ocean();
        let mut previous = OceanTables::new(&o, OCEAN_GRIDS[0]);
        for size in &OCEAN_GRIDS[1..] {
            let tables = OceanTables::new(&o, *size);
            for b in 0..2 {
                let pick = |t: &OceanTables| {
                    let mut modes: Vec<([u32; 2], [u32; 2], u32)> = t.bands[b]
                        .modes
                        .iter()
                        .map(|m| (m.k.map(f32::to_bits), m.a.map(f32::to_bits), m.n))
                        .collect();
                    modes.sort_unstable();
                    modes
                };
                assert_eq!(pick(&previous), pick(&tables), "{size}: band {b}");
            }
            let (small, large) = (&previous.bands[2], &tables.bands[2]);
            assert!(large.modes.len() > 3 * small.modes.len(), "{size}");
            let bits = |m: &OceanMode| {
                (
                    m.k.map(f32::to_bits),
                    m.a.map(f32::to_bits),
                    m.b.map(f32::to_bits),
                    m.n,
                )
            };
            let held: std::collections::HashSet<_> = large.modes.iter().map(bits).collect();
            for m in &small.modes {
                assert!(
                    held.contains(&bits(m)),
                    "{size}: a mode of the smaller grid went missing: {m:?}"
                );
            }
            previous = tables;
        }
    }

    /// The grid is a drawing choice: queries answer the same bits at any grid (ADR-2126).
    #[test]
    fn the_grid_does_not_change_what_gameplay_feels() {
        let clock = WaterClock {
            tick: 900,
            ..WaterClock::default()
        };
        let mut sea = WaterBody::plane([0.0, 2.0, 0.0], [300.0, 300.0], super::super::WaterLook::ocean());
        sea.ocean = Some(ocean());
        let answer = |sea: &WaterBody| format!("{:?}", sea.surface(12.5, -40.25, &clock));
        let base = answer(&sea);
        assert!(base.starts_with("Some"), "{base}");
        for grid in OCEAN_GRIDS {
            sea.ocean = Some(FftOcean { grid, ..ocean() });
            assert_eq!(answer(&sea), base, "grid {grid}");
        }
    }

    /// The grid is honored on the tiers that draw and refused or ignored otherwise.
    #[test]
    fn the_grid_picks_the_drawn_size_on_the_tiers_that_draw() {
        let of = |grid| FftOcean { grid, ..ocean() };
        assert_eq!(of(0).drawn_size(WaterTier::Low), 0);
        assert_eq!(of(0).drawn_size(WaterTier::High), 128);
        assert_eq!(of(0).drawn_size(WaterTier::Ultra), 256);
        for grid in OCEAN_GRIDS {
            assert_eq!(of(grid).validate(), Ok(()));
            assert_eq!(of(grid).drawn_size(WaterTier::Low), 0, "Low never computes");
            assert_eq!(of(grid).drawn_size(WaterTier::High), grid);
            assert_eq!(of(grid).drawn_size(WaterTier::Ultra), grid);
        }
        for bad in [1, 16, 100, 2048] {
            assert_eq!(of(bad).validate(), Err(WaterBodyError::OceanGrid(bad)));
            // Never reports an unusable size. The frame itself does not draw a body that fails
            // `validate` (`water_ocean_frame::the_grid_picks_the_maps_on_the_tiers_that_draw`).
            assert_eq!(of(bad).drawn_size(WaterTier::High), 128);
        }
    }

    /// A larger grid holds finer waves, and they compress the surface more, so the same onset foams
    /// more: about 1.6 times the area at 1024 for this sea, documented for hosts in the book page.
    /// A change that moves this much further is a look change and should say so.
    #[test]
    fn foam_share_by_grid() {
        let o = FftOcean {
            wind_speed: 14.0,
            ..ocean()
        };
        let limit = 1.0 - o.foam_onset;
        let mut shares = Vec::new();
        for size in OCEAN_GRIDS {
            let frame = OceanFrame::at(&Arc::new(OceanTables::new(&o, size)), loop_fraction(600.0));
            let (foamed, folded) = (
                frame.compressed_share(limit, 512),
                frame.compressed_share(0.0, 512),
            );
            eprintln!(
                "grid {size}: {:.2} % under the onset, {:.3} % folded",
                100.0 * foamed,
                100.0 * folded
            );
            shares.push(foamed);
        }
        assert!(shares.windows(2).all(|w| w[1] >= w[0]), "{shares:?}");
        assert!(shares[3] < 2.0 * shares[0], "{shares:?}");
        // The shares `docs/specs/water-ocean.md` § 8a quotes, to within 0.2 percentage points.
        for (share, quoted) in shares.iter().zip([10.5f32, 13.2, 15.3, 16.9]) {
            assert!(
                (100.0 * share - quoted).abs() < 0.2,
                "{shares:?} against the spec's {quoted} %"
            );
        }
    }

    /// Off unless asked: a new body has no ocean and does not ask for one (ADR-2126).
    #[test]
    fn a_new_body_has_no_ocean() {
        let sea = WaterBody::plane([0.0; 3], [10.0, 10.0], super::super::WaterLook::ocean());
        assert_eq!(sea.ocean, None);
        assert_eq!(sea.parked_ocean, None);
        assert_eq!(sea.active_ocean(), None);
        assert_eq!(sea.wave_mode(), Some(WaveMode::Swell));
        assert_eq!(FftOcean::default().grid, 0);
    }

    /// A round trip restores the body exactly, with or without an authored swell, and keeps the
    /// authored ocean while it is parked.
    #[test]
    fn switching_modes_round_trips_the_body() {
        let tuned = FftOcean {
            seed: 77,
            wind_speed: 21.0,
            grid: 512,
            ..ocean()
        };
        // No swell of its own: the parked ocean's stand-in carries the sea, then goes away.
        let mut bare = WaterBody::plane([0.0; 3], [300.0, 300.0], super::super::WaterLook::ocean());
        bare.ocean = Some(tuned);
        let original = bare.clone();
        bare.set_wave_mode(WaveMode::Swell).expect("plane");
        assert_eq!(bare.wave_mode(), Some(WaveMode::Swell));
        assert_eq!(bare.active_ocean(), None);
        assert_eq!(bare.parked_ocean, Some(tuned));
        assert_eq!(bare.swell, Some(tuned.stand_in_swell()));
        assert_eq!(bare.validate(), Ok(()));
        bare.set_wave_mode(WaveMode::Fft).expect("plane");
        assert_eq!(bare, original);
        // The stand-in is a legal swell across the whole range of the wind, so the switch is never
        // refused by the body's own validation.
        for wind_speed in [0.5, 12.0, 40.0] {
            for spectrum in [OceanSpectrum::Jonswap, OceanSpectrum::Phillips] {
                let mut sea = bare.clone();
                sea.ocean = Some(FftOcean {
                    wind_speed,
                    spectrum,
                    ..tuned
                });
                sea.set_wave_mode(WaveMode::Swell).expect("plane");
                assert_eq!(sea.validate(), Ok(()), "{wind_speed} m/s, {spectrum:?}");
            }
        }
        // An authored swell survives both ways.
        let mut swell = WaterBody::plane([0.0; 3], [300.0, 300.0], super::super::WaterLook::ocean());
        swell.swell = Some(Swell::open_sea([1.0, 0.4], 1.5, 25.0, 10));
        let original = swell.clone();
        swell.set_wave_mode(WaveMode::Fft).expect("plane");
        assert_eq!(swell.ocean, Some(FftOcean::default()));
        assert_eq!(swell.swell, original.swell);
        swell.set_wave_mode(WaveMode::Swell).expect("plane");
        assert_eq!(swell.swell, original.swell);
        assert_eq!(swell.active_ocean(), None);
        // The same mode twice changes nothing.
        let once = swell.clone();
        swell.set_wave_mode(WaveMode::Swell).expect("plane");
        assert_eq!(swell, once);
        // A ribbon refuses, unchanged.
        let points = [0.0f32, 2.0]
            .map(|x| super::super::RibbonPoint {
                position: [x, 0.0, 0.0],
                width: 1.0,
                depth: 0.2,
                speed: None,
            })
            .to_vec();
        let mut ribbon = WaterBody::ribbon(points, super::super::WaterLook::clear_stream());
        let unchanged = ribbon.clone();
        assert_eq!(ribbon.wave_mode(), None);
        assert_eq!(
            ribbon.set_wave_mode(WaveMode::Fft),
            Err(WaterBodyError::OceanNeedsPlane)
        );
        assert_eq!(ribbon, unchanged);
    }

    /// The transform is the inverse DFT: a grid point equals the direct sum over the modes.
    #[test]
    fn fft_matches_the_direct_sum() {
        let tables = Arc::new(OceanTables::new(&ocean(), 32));
        let frame = OceanFrame::at(&tables, loop_fraction(77.0));
        for (b, band) in tables.bands.iter().enumerate() {
            let mut only = (*tables).clone();
            for (o, other) in only.bands.iter_mut().enumerate() {
                if o != b {
                    other.modes.clear();
                }
            }
            let f = OceanFrame::at(&Arc::new(only), frame.tau);
            let cell = band.tile as f32 / 32.0;
            for (i, j) in [(0u32, 0u32), (3, 5), (17, 2), (31, 31)] {
                let got = f.grids[b].h[(j * 32 + i) as usize];
                let want = direct_height(&f, i as f32 * cell, j as f32 * cell);
                assert!((got - want).abs() < 1.0e-3, "band {b} ({i},{j}): {got} vs {want}");
            }
        }
    }

    /// A height field of one wave: the Gaussian draw aside, the mode runs along `+k` at the
    /// dispersion speed, and crests compress under the choppy offset.
    #[test]
    fn a_single_mode_is_a_travelling_trochoid() {
        let mut tables = OceanTables::new(&ocean(), 32);
        for band in &mut tables.bands {
            band.modes.clear();
        }
        let tile = tables.bands[0].tile;
        let k = TAU64 / tile * 3.0;
        let n = quantized_frequency(k);
        let amp = 0.5f32;
        tables.bands[0].modes.push(OceanMode {
            index: 3,
            k: [k as f32, 0.0],
            inv_k: (1.0 / k) as f32,
            n,
            a: [0.0, -amp],
            b: [0.0, 0.0],
        });
        tables.bands[0].modes.push(OceanMode {
            index: 32 - 3,
            k: [-k as f32, 0.0],
            inv_k: (1.0 / k) as f32,
            n,
            a: [0.0, 0.0],
            b: [0.0, amp],
        });
        let tables = Arc::new(tables);
        let omega = f64::from(n) * TAU64 / OCEAN_LOOP_SECONDS;
        for t in [0.0f64, 1.25, 40.0] {
            let frame = OceanFrame::at(&tables, loop_fraction(t));
            for i in [0u32, 5, 13, 27] {
                let x = f64::from(i) * tile / 32.0;
                let theta = k * x - omega * t;
                // a e^(iθ) + conj = 2 Re(−i A e^(iθ)) = 2 A sin θ.
                let want = 2.0 * f64::from(amp) * theta.sin();
                let got = f64::from(frame.grids[0].h[i as usize]);
                assert!((got - want).abs() < 3.0e-3, "t {t} x {x}: {got} vs {want}");
                let dx = f64::from(frame.grids[0].dx[i as usize]);
                let want = f64::from(ocean().choppiness) * 2.0 * f64::from(amp) * theta.cos();
                assert!(
                    (dx - want).abs() < 3.0e-3,
                    "offset at t {t} x {x}: {dx} vs {want}"
                );
                // ∂h/∂t = −2 A ω cos θ.
                let vy = f64::from(frame.grids[0].vy[i as usize]);
                let want = -2.0 * f64::from(amp) * omega * theta.cos();
                assert!(
                    (vy - want).abs() < 5.0e-3,
                    "velocity at t {t} x {x}: {vy} vs {want}"
                );
            }
        }
        // At a crest the horizontal map compresses (J < 1), at a trough it stretches.
        let frame = OceanFrame::at(&tables, 0);
        let lambda = (TAU64 / k) as f32;
        let crest = frame.sample(0.25 * lambda, 0.0);
        let trough = frame.sample(0.75 * lambda, 0.0);
        assert!(crest.offset[1] > trough.offset[1]);
        assert!(
            crest.jacobian < 0.99 && trough.jacobian > 1.01,
            "{crest:?} {trough:?}"
        );
    }

    /// Spatial derivatives and velocity agree with finite differences of the surface.
    #[test]
    fn derivative_maps_match_finite_differences() {
        let o = ocean();
        let f = frame_at(&o, 12.5);
        let tile = f.tables.bands[0].tile;
        let e = tile as f32 / OCEAN_QUERY_SIZE as f32;
        for (x, z) in [(40.0f32, -20.0f32), (301.0, 77.0)] {
            let s = f.sample(x, z);
            let p = |dx: f32, dz: f32| f.sample(x + dx, z + dz).offset;
            let fine = e / 16.0;
            let slope_x = (p(fine, 0.0)[1] - p(-fine, 0.0)[1]) / (2.0 * fine);
            let slope_z = (p(0.0, fine)[1] - p(0.0, -fine)[1]) / (2.0 * fine);
            // The bands' fine waves make the bilinear surface a coarse copy of the spectral slope.
            assert!((slope_x - s.dpdx[1]).abs() < 0.25, "{slope_x} vs {}", s.dpdx[1]);
            assert!((slope_z - s.dpdz[1]).abs() < 0.25, "{slope_z} vs {}", s.dpdz[1]);
        }
        let dt = 1.0f64 / 240.0;
        let a = frame_at(&o, 12.5 - dt).sample(40.0, -20.0).offset;
        let b = frame_at(&o, 12.5 + dt).sample(40.0, -20.0).offset;
        let v = f.sample(40.0, -20.0).velocity;
        for i in 0..3 {
            let fd = (b[i] - a[i]) / (2.0 * dt as f32);
            assert!((fd - v[i]).abs() < 0.05, "v[{i}] {fd} vs {}", v[i]);
        }
    }

    /// Same seed and time, same bytes; another seed or another time, another sea; the phases
    /// wrap exactly at the loop.
    #[test]
    fn deterministic_by_seed_and_time() {
        let o = ocean();
        let tables = Arc::new(OceanTables::new(&o, 64));
        let a = OceanFrame::at(&tables, loop_fraction(321.0));
        let b = OceanFrame::at(&Arc::new(OceanTables::new(&o, 64)), loop_fraction(321.0));
        assert_eq!(a.grids, b.grids);
        let later = OceanFrame::at(&tables, loop_fraction(322.0));
        assert_ne!(a.grids[0].h, later.grids[0].h);
        let other = OceanFrame::at(
            &Arc::new(OceanTables::new(&FftOcean { seed: 2, ..o }, 64)),
            loop_fraction(321.0),
        );
        assert_ne!(a.grids[1].h, other.grids[1].h);
        // After the loop the integers are the same, so the bytes are.
        assert_eq!(
            loop_fraction(321.0),
            loop_fraction(321.0 + 5.0 * OCEAN_LOOP_SECONDS)
        );
        let looped = OceanFrame::at(&tables, loop_fraction(321.0 + OCEAN_LOOP_SECONDS));
        assert_eq!(a.grids, looped.grids);
        // Ten days into a session the time step is still resolved.
        let late = 864_000.0;
        assert_ne!(loop_fraction(late), loop_fraction(late + 1.0 / 60.0));
        assert_eq!(loop_fraction(f64::NAN), 0);
    }

    /// Each band repeats on its own tile, in x and in z, at any point.
    #[test]
    fn bands_tile_with_wrap_around() {
        let frame = frame_at(&ocean(), 3.0);
        for (b, band) in frame.tables.bands.iter().enumerate() {
            let c = |x: f64, z: f64| BilinearCell::new(x, z, band.tile, OCEAN_QUERY_SIZE);
            let tile = band.tile;
            for (x, z) in [(1.25f64, 2.5f64), (-13.0, 8.0), (tile * 0.999, tile * 0.5)] {
                let h = c(x, z).of(&frame.grids[b].h);
                assert!((h - c(x + tile, z).of(&frame.grids[b].h)).abs() < 1.0e-4);
                assert!((h - c(x, z - 3.0 * tile).of(&frame.grids[b].h)).abs() < 1.0e-4);
            }
            // Across the seam: the last texel's right neighbor is texel 0.
            let n = OCEAN_QUERY_SIZE as usize;
            let (last, first) = (frame.grids[b].h[n - 1], frame.grids[b].h[0]);
            let seam = c(tile * (1.0 - 0.5 / f64::from(OCEAN_QUERY_SIZE)), 0.0).of(&frame.grids[b].h);
            assert!((seam - 0.5 * (last + first)).abs() < 1.0e-3);
        }
        // Three tiles with a whole common multiple: the query surface wraps at it too.
        let o = FftOcean {
            tiles: [400.0, 100.0, 25.0],
            ..ocean()
        };
        let f = frame_at(&o, 3.0);
        let a = f.sample(7.5, 3.25);
        let b = f.sample(7.5 + 400.0, 3.25 - 800.0);
        assert!((a.offset[1] - b.offset[1]).abs() < 2.0e-3, "{a:?} {b:?}");
    }

    /// The modes carry the variance of the spectrum: the significant height of the grid
    /// (Parseval) equals the sum over the modes, and both are near the spectrum's integral.
    #[test]
    fn energy_matches_the_spectrum() {
        for o in [
            ocean(),
            FftOcean {
                spectrum: OceanSpectrum::Phillips,
                ..ocean()
            },
            FftOcean {
                wind_speed: 20.0,
                fetch: 800_000.0,
                gamma: 1.0,
                ..ocean()
            },
        ] {
            let tables = tables_for(&o, OCEAN_QUERY_SIZE);
            let frame = frame_at(&o, 41.0);
            let texels = f64::from(OCEAN_QUERY_SIZE * OCEAN_QUERY_SIZE);
            let spatial: f64 = frame
                .grids
                .iter()
                .map(|g| g.h.iter().map(|h| f64::from(*h) * f64::from(*h)).sum::<f64>() / texels)
                .sum();
            let modes = tables.significant_height();
            let expected = o.expected_height();
            // The grids hold about the expected variance: a few low modes swing with the seed.
            assert!(
                (modes / expected - 1.0).abs() < 0.35,
                "{:?}: modes Hs {modes} vs spectrum {expected}",
                o.spectrum
            );
            // Time moves energy between the two waves of a mode: the mean stays near.
            let grid = 4.0 * spatial.sqrt();
            assert!((grid / modes - 1.0).abs() < 0.35, "grid {grid} vs modes {modes}");
        }
        // The spectrum's integral matches the known fully developed sea: Hs ≈ 0.21 U² / g within
        // the Phillips form's 0.18.
        let p = FftOcean {
            spectrum: OceanSpectrum::Phillips,
            wind_speed: 15.0,
            ..ocean()
        };
        let want = 0.18 * 225.0 / 9.81;
        assert!(
            (p.expected_height() / want - 1.0).abs() < 0.05,
            "{}",
            p.expected_height()
        );
    }

    /// The JONSWAP peak sits where the formula says, sharpened by gamma; longer fetch, longer
    /// and higher waves.
    #[test]
    fn jonswap_shape() {
        let o = ocean();
        let peak = o.peak_frequency();
        let at = |w: f64| jonswap(w, &o);
        assert!(at(peak) > at(0.9 * peak) && at(peak) > at(1.1 * peak));
        let flat = FftOcean { gamma: 1.0, ..o };
        assert!((jonswap(peak, &o) / jonswap(peak, &flat) - 3.3).abs() < 1.0e-6);
        let far = FftOcean {
            fetch: 4.0 * o.fetch,
            ..o
        };
        assert!(far.peak_wavelength() > o.peak_wavelength());
        assert!(far.expected_height() > o.expected_height());
        // Normalized spread: the circle integrates to one, and it peaks downwind.
        let turns = 7200;
        let sum: f64 = (0..turns)
            .map(|i| {
                let t = (f64::from(i) + 0.5) / f64::from(turns) * TAU64;
                spread_density(t.cos(), 8.0) * TAU64 / f64::from(turns)
            })
            .sum();
        assert!((sum - 1.0).abs() < 1.0e-6, "{sum}");
        assert!(spread_density(1.0, 8.0) > spread_density(0.5, 8.0));
        // Waves run with the wind.
        assert!(o.density(0.1, 0.0) > 100.0 * o.density(-0.1, 0.0));
    }

    /// A mode's amplitude depends on the seed, band and index only, so the 128 grid's modes are
    /// the 256 grid's, and every mode of the wider grid that is not on the smaller lies in the
    /// last band; each wave number falls in one band.
    #[test]
    fn bands_split_without_overlap_and_tiers_share_modes() {
        let o = ocean();
        let small = OceanTables::new(&o, 128);
        let large = OceanTables::new(&o, 256);
        for b in 0..2 {
            assert_eq!(small.bands[b].modes.len(), large.bands[b].modes.len(), "band {b}");
            let sa: Vec<_> = small.bands[b]
                .modes
                .iter()
                .map(|m| (m.k, m.a, m.b, m.n))
                .collect();
            let mut la: Vec<_> = large.bands[b]
                .modes
                .iter()
                .map(|m| (m.k, m.a, m.b, m.n))
                .collect();
            let key = |v: &([f32; 2], [f32; 2], [f32; 2], u32)| (v.0[0].to_bits(), v.0[1].to_bits());
            let mut sa = sa;
            sa.sort_by_key(key);
            la.sort_by_key(key);
            assert_eq!(sa, la, "band {b}");
        }
        assert!(large.bands[2].modes.len() > small.bands[2].modes.len());
        let limits = OceanTables::band_limits(o.tiles);
        assert_eq!(limits[0][1], limits[1][0]);
        assert_eq!(limits[1][1], limits[2][0]);
        for band in &large.bands {
            for m in &band.modes {
                let k = f64::from(m.k[0]).hypot(f64::from(m.k[1]));
                let slack = 1.0e-5 * k;
                assert!(k >= band.limits[0] - slack && k < band.limits[1] + slack, "{k}");
            }
        }
        // The band's energy sits in it: the fade number is inside the limits.
        for band in &small.bands {
            assert!(band.rms_k >= band.limits[0] && band.rms_k <= band.limits[1].min(1.0e6));
        }
        // Every band holds energy for the default sea.
        assert!(small.bands.iter().all(|b| b.variance > 0.0));
        // Each mode has a conjugate slot: the output is real because the pair is complete.
        for band in &small.bands {
            let n = OCEAN_QUERY_SIZE as usize;
            for m in &band.modes {
                let (row, column) = (m.index as usize / n, m.index as usize % n);
                let mirror = ((n - row) % n) * n + (n - column) % n;
                let found = band.modes.iter().find(|q| q.index as usize == mirror);
                let q = found.expect("the conjugate mode exists");
                assert_eq!((q.a, q.b), ([m.b[0], -m.b[1]], [m.a[0], -m.a[1]]));
            }
        }
    }

    /// The shore weight: 1 offshore and without a bed, 0 on land, monotonic between; the mirror
    /// of the shader's `ocean_weight`.
    #[test]
    fn shore_weight_fades_with_depth() {
        let o = ocean();
        assert_eq!(o.shore_weight(None), 1.0);
        assert_eq!(o.shore_weight(Some(500.0)), 1.0);
        assert_eq!(o.shore_weight(Some(o.shore_start)), 1.0);
        assert_eq!(o.shore_weight(Some(o.shore_end)), 0.0);
        assert_eq!(o.shore_weight(Some(-1.0)), 0.0);
        assert_eq!(o.shore_weight(Some(f32::NAN)), 0.0);
        let mid = o.shore_weight(Some(0.5 * (o.shore_start + o.shore_end)));
        assert!((mid - 0.5).abs() < 1.0e-5);
        let w: Vec<f32> = (0..40).map(|i| o.shore_weight(Some(i as f32))).collect();
        assert!(w.windows(2).all(|p| p[0] <= p[1]));
    }

    #[test]
    fn validation_refuses_bad_oceans() {
        assert_eq!(ocean().validate(), Ok(()));
        assert!(matches!(
            FftOcean {
                wind_speed: 90.0,
                ..ocean()
            }
            .validate(),
            Err(WaterBodyError::OutOfRange { .. })
        ));
        assert_eq!(
            FftOcean {
                wind_direction: [0.0, 0.0],
                ..ocean()
            }
            .validate(),
            Err(WaterBodyError::NotPositive("ocean.wind_direction"))
        );
        assert_eq!(
            FftOcean {
                tiles: [100.0, 90.0, 20.0],
                ..ocean()
            }
            .validate(),
            Err(WaterBodyError::OceanTiles)
        );
        assert!(
            FftOcean {
                shore_start: 1.0,
                shore_end: 2.0,
                ..ocean()
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            FftOcean {
                shore_start: 1.0,
                ..FftOcean::default()
            }
            .validate(),
            Err(WaterBodyError::OutOfRange {
                name: "ocean.shore_start",
                value: 1.0,
                min: 3.0,
                max: 1000.0,
            })
        );
        assert_eq!(
            FftOcean {
                fetch: f32::NAN,
                ..ocean()
            }
            .validate(),
            Err(WaterBodyError::NotFinite("ocean.fetch"))
        );
    }

    /// The stand-in swell is a valid swell of the ocean's height and peak, and runs downwind.
    #[test]
    fn stand_in_swell_fits_the_sea() {
        let o = ocean();
        let s = o.stand_in_swell();
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(s.waves.len(), STAND_IN_WAVES);
        let rss = s.waves.iter().map(|w| w.height * w.height).sum::<f32>().sqrt();
        assert!((f64::from(rss) * std::f64::consts::SQRT_2 / o.expected_height() - 1.0).abs() < 1.0e-3);
        let longest = s.waves[0].wavelength;
        assert!((f64::from(longest) / o.peak_wavelength() - 1.6).abs() < 0.01);
        assert_eq!(s.crest_foam, o.crest_foam);
        let d = s.waves[0].unit_direction();
        assert!(d[0] > 0.8, "{d:?}");
    }

    fn sea(level: f32) -> WaterBody {
        let mut body = WaterBody::plane(
            [3.0, level, -2.0],
            [900.0, 900.0],
            super::super::WaterLook::ocean(),
        );
        body.ocean = Some(ocean());
        body
    }

    /// The query lands on the mirror's displaced surface: the inverse's miss is small, the
    /// answer is the mirror's offset at the rest point, and a level plane under an ocean rises.
    #[test]
    fn surface_query_lands_on_the_mirror() {
        let body = sea(4.0);
        assert_eq!(body.validate(), Ok(()));
        let clock = WaterClock {
            tick: 12_000,
            ..WaterClock::default()
        };
        let frame = frame_at(&ocean(), clock.seconds());
        let mut spread = 0.0f32;
        for (x, z) in [(0.0f32, 0.0f32), (31.5, -77.0), (200.0, 300.0), (-411.0, 64.0)] {
            let s = body.surface(x, z, &clock).expect("inside");
            assert!(s.miss < 2.0e-2, "({x}, {z}): miss {}", s.miss);
            let f = frame.sample(s.rest[0], s.rest[1]);
            assert!((s.height - (4.0 + f.offset[1])).abs() < 1.0e-4, "{s:?}");
            assert!((s.rest[0] + f.offset[0] - x).abs() < 2.0e-2);
            assert!((s.rest[1] + f.offset[2] - z).abs() < 2.0e-2);
            assert!(s.normal[1] > 0.3, "{s:?}");
            spread = spread.max((s.height - 4.0).abs());
        }
        assert!(spread > 0.05, "{spread}");
        assert!(body.surface(5000.0, 0.0, &clock).is_none());
    }

    /// A ray lands where the surface query says; the bounds hold the surface; rays that miss miss.
    #[test]
    fn raycast_agrees_with_the_query() {
        let body = sea(0.0);
        let clock = WaterClock {
            tick: 3_000,
            ..WaterClock::default()
        };
        let frame = frame_at(&ocean(), clock.seconds());
        let (lo, hi) = body.bounds();
        assert!(hi[1] >= frame.max_rise() && lo[1] <= -frame.max_rise());
        for (origin, dir) in [
            ([0.0f32, 30.0, 0.0], [0.2f32, -1.0, 0.1]),
            ([-300.0, 15.0, 40.0], [1.0, -0.1, 0.05]),
        ] {
            let hit = body.raycast(origin, dir, 2000.0, &clock).expect("hit");
            let q = body.surface(hit.point[0], hit.point[2], &clock).expect("surface");
            assert!((q.height - hit.point[1]).abs() < 1.0e-3, "{hit:?}");
            let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
            let y = origin[1] + dir[1] / len * hit.distance;
            assert!((y - q.height).abs() < 2.0e-2, "{y} vs {}", q.height);
        }
        assert!(
            body.raycast([0.0, 30.0, 0.0], [0.0, 1.0, 0.0], 500.0, &clock)
                .is_none()
        );
    }

    /// Foam comes from the Jacobian: the query's foam is the crest function of the mirror's J at
    /// the rest point, and a stormy sea foams on some of its surface but not all.
    #[test]
    fn foam_comes_from_the_jacobian() {
        let storm = FftOcean {
            wind_speed: 22.0,
            choppiness: 1.3,
            foam_onset: 0.25,
            ..ocean()
        };
        let mut body = sea(0.0);
        body.ocean = Some(storm);
        let clock = WaterClock {
            tick: 600,
            ..WaterClock::default()
        };
        let frame = frame_at(&storm, clock.seconds());
        let (mut foamy, mut total) = (0, 0);
        for i in 0..120 {
            let (x, z) = (i as f32 * 7.3 - 400.0, (i * i % 97) as f32 * 8.0 - 390.0);
            let s = body.surface(x, z, &clock).expect("inside");
            let j = frame.sample(s.rest[0], s.rest[1]).jacobian;
            let want = swell::crest_foam(j, storm.crest_foam, storm.foam_onset);
            assert!((s.crest_foam - want).abs() < 1.0e-5);
            total += 1;
            foamy += i32::from(s.crest_foam > 0.0);
        }
        assert!(foamy > 0 && foamy < total, "{foamy} of {total}");
    }

    /// The default onset gives a windy sea whitecaps on a little of its surface, and more wind
    /// more of them (a linear sea's `1 − J` has a deviation near 0.12 here, so an onset of 0.35
    /// would leave it bare).
    #[test]
    fn default_foam_covers_a_little_of_a_windy_sea() {
        let coverage = |wind_speed: f32| {
            let o = FftOcean {
                wind_speed,
                ..ocean()
            };
            let frame = frame_at(&o, 90.0);
            let n = 256u32;
            let step = 1009.0 * 0.37 / n as f32;
            let sum: f32 = (0..n * n)
                .map(|i| {
                    let j = frame
                        .sample((i % n) as f32 * step, (i / n) as f32 * step)
                        .jacobian;
                    swell::crest_foam(j, o.crest_foam, o.foam_onset)
                })
                .sum();
            sum / (n * n) as f32
        };
        let (calm, windy, gale) = (coverage(6.0), coverage(14.0), coverage(28.0));
        assert!(windy > 0.003 && windy < 0.05, "14 m/s: {windy}");
        assert!(calm < windy && windy < gale, "{calm} {windy} {gale}");
    }

    /// Over a sloped beach the surface goes from the ocean offshore to the shoaled swell in the
    /// shallows, with no step, and dry land answers nothing.
    #[test]
    fn blends_into_the_swell_near_the_shore() {
        use super::super::shore::ShoreBakeDesc;
        let mut body = sea(0.0);
        body.swell = Some(Swell::open_sea([1.0, 0.0], 0.8, 40.0, 8));
        // The bed rises along +x: 50 m deep at x = -600, the shoreline at x = 400.
        let field = ShoreField::bake(
            &ShoreBakeDesc {
                level: 0.0,
                min: [-880.0, -500.0],
                max: [880.0, 500.0],
                cell: 4.0,
            },
            |x, _| (x - 400.0) / 20.0,
        )
        .expect("bake");
        let clock = WaterClock {
            tick: 1_800,
            ..WaterClock::default()
        };
        let o = ocean();
        let frame = frame_at(&o, clock.seconds());
        // Deep: the mirror alone.
        let deep = body.surface_on(-700.0, 0.0, &clock, Some(&field)).expect("deep");
        let plain = body.surface(-700.0, 0.0, &clock).expect("plain");
        assert!((deep.height - plain.height).abs() < 1.0e-5, "{deep:?} {plain:?}");
        // Shallow, under `shore_end`: the swell alone, the same as the body without the ocean.
        let mut swell_only = body.clone();
        swell_only.ocean = None;
        let shallow = body.surface_on(350.0, 3.0, &clock, Some(&field)).expect("wet");
        let want = swell_only
            .surface_on(350.0, 3.0, &clock, Some(&field))
            .expect("wet");
        assert!(
            (shallow.height - want.height).abs() < 1.0e-5,
            "{shallow:?} {want:?}"
        );
        // Across the blend the surface stays continuous.
        let heights: Vec<f32> = (0..1200)
            .map(|i| {
                let x = -80.0 + i as f32 * 0.35;
                body.surface_on(x, 7.0, &clock, Some(&field)).expect("wet").height
            })
            .collect();
        let jump = heights
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(jump < 0.35, "{jump}");
        let mid = body.surface_on(-40.0, 7.0, &clock, Some(&field)).expect("wet");
        let depth = mid.depth.expect("depth");
        assert!(depth > o.shore_end && depth < o.shore_start, "{depth}");
        assert!(frame.max_rise() > 0.0);
        // Land answers nothing, the ray misses it.
        assert!(body.surface_on(600.0, 0.0, &clock, Some(&field)).is_none());
        assert!(
            body.raycast_on([600.0, 20.0, 0.0], [0.0, -1.0, 0.0], 100.0, &clock, Some(&field))
                .is_none()
        );
    }

    /// The body round-trips through RON, an old body has no ocean, a ribbon refuses one.
    #[test]
    fn ocean_round_trips_through_ron() {
        let body = sea(1.0);
        let text = ron::to_string(&body).expect("serialize");
        let back: WaterBody = ron::from_str(&text).expect("parse");
        assert_eq!(back, body);
        let old: WaterBody =
            ron::from_str("(shape: plane(center: (0.0, 1.0, 0.0), half_extent: (5.0, 5.0)))").expect("parse");
        assert_eq!(old.ocean, None);
        let partial: FftOcean = ron::from_str("(wind_speed: 7.5)").expect("parse");
        assert_eq!(partial.wind_speed, 7.5);
        assert_eq!(partial.seed, ocean().seed);
        let points = (0..3)
            .map(|i| super::super::RibbonPoint {
                position: [i as f32, 0.0, 0.0],
                width: 1.0,
                depth: 0.2,
                speed: None,
            })
            .collect();
        let mut ribbon = WaterBody::ribbon(points, super::super::WaterLook::clear_stream());
        ribbon.ocean = Some(ocean());
        assert_eq!(ribbon.validate(), Err(WaterBodyError::OceanNeedsPlane));
        assert!(ribbon.active_ocean().is_none());
    }

    fn counted_sea() -> (WaterBody, FftOcean) {
        let mut ocean = ocean();
        ocean.seed = 0x0CE4_51A1;
        let mut body = sea(0.0);
        body.ocean = Some(ocean);
        (body, ocean)
    }

    /// A point outside the rectangle widened by the shift bound does not transform.
    #[test]
    fn a_miss_outside_the_rectangle_does_not_evolve() {
        let (body, _) = counted_sea();
        let cold = WaterClock {
            tick: 48_000_000,
            ..WaterClock::default()
        };
        test_reset_ocean_counts();
        assert!(body.surface(50_000.0, 50_000.0, &cold).is_none());
        assert_eq!(test_ocean_evolves(), 0, "a miss must not run the inverse FFT");
        assert_eq!(test_ocean_scratch_allocs(), 0, "a miss must not allocate scratch");
    }

    /// N surface samples of one tick transform once, the next tick once more, later samples skip
    /// the memo lock, and the scratch buffer is allocated at most once.
    #[test]
    fn samples_of_one_tick_evolve_once() {
        let (body, _) = counted_sea();
        let inside = WaterClock {
            tick: 48_000_120,
            ..WaterClock::default()
        };
        test_reset_ocean_counts();
        for _ in 0..8 {
            body.surface(3.0, -2.0, &inside).expect("inside the rectangle");
        }
        assert_eq!(test_ocean_evolves(), 1, "one tick transforms once");
        let locks_after_tick = test_ocean_memo_locks();
        for _ in 0..8 {
            body.surface(10.0, 4.0, &inside).expect("still inside");
        }
        assert_eq!(
            test_ocean_memo_locks(),
            locks_after_tick,
            "later samples of the tick skip the memo lock"
        );
        let next = WaterClock {
            tick: 48_000_121,
            ..WaterClock::default()
        };
        body.surface(3.0, -2.0, &next).expect("inside");
        assert_eq!(test_ocean_evolves(), 2, "the next tick transforms once more");
    }

    /// Each new tick reuses one scratch buffer instead of allocating another.
    #[test]
    fn evolve_reuses_one_scratch_buffer() {
        let mut o = ocean();
        o.seed = 0x5C2A_11E0;
        test_reset_ocean_counts();
        for i in 0..3u32 {
            let _ = frame_at(&o, 90_000.0 + f64::from(i));
        }
        assert_eq!(test_ocean_evolves(), 3);
        assert!(
            test_ocean_scratch_allocs() <= 1,
            "scratch allocations {}, want at most one",
            test_ocean_scratch_allocs()
        );
    }

    /// The memo's answers equal a fresh transform, and buffers it recycles stay clean.
    #[test]
    fn memo_shares_a_tick() {
        let o = FftOcean { seed: 77, ..ocean() };
        let a = frame_at(&o, 9.0);
        let b = frame_at(&o, 9.0);
        // Equal, not the same Arc: tests sharing the memo may evict an entry between two calls.
        assert_eq!((a.tau, &a.grids), (b.tau, &b.grids));
        let c = frame_at(&o, 9.5);
        assert_ne!(a.tau, c.tau);
        // Frames rebuilt from recycled buffers equal fresh ones.
        for i in 0..8 {
            let t = 20.0 + f64::from(i);
            let f = frame_at(&o, t);
            let fresh = OceanFrame::at(&tables_for(&o, OCEAN_QUERY_SIZE), loop_fraction(t));
            assert_eq!(f.grids, fresh.grids);
        }
    }

    /// `docs/specs/water-ocean.md` § 1, row by row: every default, and every range accepted at both
    /// ends and refused just past them, by the field's own name.
    #[test]
    fn defaults_and_ranges_follow_the_spec_table() {
        let d = FftOcean::default();
        assert_eq!(d.seed, 1);
        assert_eq!(d.spectrum, OceanSpectrum::Jonswap);
        assert_eq!(OceanSpectrum::default(), OceanSpectrum::Jonswap);
        assert_eq!(d.wind_speed, 12.0);
        assert_eq!(d.wind_direction, [1.0, 0.0]);
        assert_eq!(d.fetch, 300_000.0);
        assert_eq!(d.gamma, 3.3);
        assert_eq!(d.spread, 8.0);
        assert_eq!(d.choppiness, 0.9);
        assert_eq!(d.height_scale, 1.0);
        assert_eq!(d.tiles, [1009.0, 257.0, 67.0]);
        assert_eq!((d.crest_foam, d.foam_onset), (1.0, 0.15));
        assert_eq!((d.shore_start, d.shore_end), (24.0, 3.0));
        assert_eq!(d.grid, 0);
        assert_eq!(d.validate(), Ok(()));
        assert_eq!(
            (OceanSpectrum::Jonswap.name(), OceanSpectrum::Phillips.name()),
            ("jonswap", "phillips")
        );

        type Set = fn(&mut FftOcean, f32);
        let fields: [(&str, f32, f32, f32, f32, Set); 8] = [
            ("ocean.wind_speed", 0.5, 40.0, 0.49, 40.01, |o, v| {
                o.wind_speed = v
            }),
            ("ocean.fetch", 1.0e3, 2.0e6, 999.0, 2.0e6 + 1.0, |o, v| {
                o.fetch = v
            }),
            ("ocean.gamma", 1.0, 7.0, 0.99, 7.01, |o, v| o.gamma = v),
            ("ocean.spread", 1.0, 64.0, 0.99, 64.01, |o, v| o.spread = v),
            ("ocean.choppiness", 0.0, 2.0, -0.01, 2.01, |o, v| o.choppiness = v),
            ("ocean.height_scale", 0.0, 4.0, -0.01, 4.01, |o, v| {
                o.height_scale = v
            }),
            ("ocean.crest_foam", 0.0, 4.0, -0.01, 4.01, |o, v| o.crest_foam = v),
            ("ocean.foam_onset", 0.0, 1.0, -0.01, 1.01, |o, v| o.foam_onset = v),
        ];
        for (name, lo, hi, below, above, set) in fields {
            for ok in [lo, hi] {
                let mut o = d;
                set(&mut o, ok);
                assert_eq!(o.validate(), Ok(()), "{name} = {ok}");
            }
            for bad in [below, above] {
                let mut o = d;
                set(&mut o, bad);
                assert!(
                    matches!(o.validate(), Err(WaterBodyError::OutOfRange { name: n, .. }) if n == name),
                    "{name} = {bad}: {:?}",
                    o.validate()
                );
            }
            let mut o = d;
            set(&mut o, f32::INFINITY);
            assert_eq!(o.validate(), Err(WaterBodyError::NotFinite(name)), "{name}");
        }
        // Any non-zero wind direction, of any length.
        for wind in [[0.0, 1.0e-3], [-30.0, 4.0], [0.0, -1.0]] {
            assert_eq!(
                FftOcean {
                    wind_direction: wind,
                    ..d
                }
                .validate(),
                Ok(()),
                "{wind:?}"
            );
        }
        assert_eq!(
            FftOcean {
                wind_direction: [f32::NAN, 1.0],
                ..d
            }
            .validate(),
            Err(WaterBodyError::NotFinite("ocean.wind_direction"))
        );
        // Tiles: 8..=20 000 m, longest first, each at least twice the next.
        for tiles in [
            [20_000.0, 10_000.0, 5_000.0],
            [32.0, 16.0, 8.0],
            [4000.0, 2000.0, 1000.0],
        ] {
            assert_eq!(FftOcean { tiles, ..d }.validate(), Ok(()), "{tiles:?}");
        }
        for tiles in [[20_001.0, 10_000.0, 5_000.0], [32.0, 16.0, 7.9]] {
            assert!(
                matches!(
                    FftOcean { tiles, ..d }.validate(),
                    Err(WaterBodyError::OutOfRange {
                        name: "ocean.tiles",
                        ..
                    })
                ),
                "{tiles:?}"
            );
        }
        for tiles in [[100.0, 51.0, 25.0], [100.0, 50.0, 26.0], [67.0, 257.0, 1009.0]] {
            assert_eq!(
                FftOcean { tiles, ..d }.validate(),
                Err(WaterBodyError::OceanTiles),
                "{tiles:?}"
            );
        }
        // Shore depths: `shore_end` 0..=1000, `shore_start` `shore_end`..=1000 (ADR-5083).
        for (start, end) in [(0.0, 0.0), (1000.0, 1000.0), (3.0, 3.0), (1000.0, 0.0)] {
            let o = FftOcean {
                shore_start: start,
                shore_end: end,
                ..d
            };
            assert_eq!(o.validate(), Ok(()), "{start} / {end}");
        }
        for (start, end, name) in [
            (24.0, -0.01, "ocean.shore_end"),
            (1000.0, 1000.5, "ocean.shore_end"),
            (2.99, 3.0, "ocean.shore_start"),
            (1000.5, 3.0, "ocean.shore_start"),
        ] {
            let o = FftOcean {
                shore_start: start,
                shore_end: end,
                ..d
            };
            assert!(
                matches!(o.validate(), Err(WaterBodyError::OutOfRange { name: n, .. }) if n == name),
                "{start} / {end}: {:?}",
                o.validate()
            );
        }
        // Grid: 0 or one of the four sizes.
        for grid in [0, 128, 256, 512, 1024] {
            assert_eq!(FftOcean { grid, ..d }.validate(), Ok(()), "{grid}");
        }
        assert_eq!(OCEAN_GRIDS, [128, 256, 512, 1024]);
        // Every u32 seed is legal.
        assert_eq!(FftOcean { seed: u32::MAX, ..d }.validate(), Ok(()));
    }

    /// `WaterBody::parked_ocean` is left out of the output when empty, so a body that never
    /// switched serializes as before (spec § 1), and a parked ocean survives a round trip.
    #[test]
    fn a_body_that_never_switched_serializes_as_before() {
        let body = sea(1.0);
        let text = ron::to_string(&body).expect("serialize");
        assert!(!text.contains("parked_ocean"), "{text}");
        let mut parked = body.clone();
        parked.set_wave_mode(WaveMode::Swell).expect("plane");
        let text = ron::to_string(&parked).expect("serialize");
        assert!(text.contains("parked_ocean"), "{text}");
        let back: WaterBody = ron::from_str(&text).expect("parse");
        assert_eq!(back, parked);
        assert_eq!(back.wave_mode(), Some(WaveMode::Swell));
        let mut restored = back;
        restored.set_wave_mode(WaveMode::Fft).expect("plane");
        assert_eq!(restored, body);
    }
}
