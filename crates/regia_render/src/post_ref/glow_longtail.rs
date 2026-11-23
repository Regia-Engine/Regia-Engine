// SPDX-License-Identifier: MPL-2.0
// Source: clean-room behavior spec `glow_longtail_fringe.md` (Lumit repository, commit
// fa959219e8920f4af445c2a3fd584a2c3061b190, GPL-3.0, behavior-only); J. Jimenez, "Next Generation
// Post Processing in Call of Duty: Advanced Warfare", SIGGRAPH 2014 Advances in Real-Time Rendering
// course notes (the multi-scale bloom the glow is a delta over); G. Spencer, P. Shirley,
// K. Zimmerman, D. P. Greenberg, "Physically-Based Glare Effects for Digital Images", Proc.
// SIGGRAPH 1995 (long-tailed veiling glare); R. Keys, "Cubic Convolution Interpolation for Digital
// Image Processing", IEEE Trans. ASSP 29(6), 1981 (the Catmull-Rom upsampling the spec's quality
// section names for the reduced-grid GPU path).

//! CPU reference of the glow delta over the standard bloom: the long-tail halo kernel and the
//! chromatic fringe.
//!
//! This is a test oracle, not a runtime path: it evaluates the kernels directly on the CPU at test
//! sizes and follows the stages of the behavior spec:
//!
//! 1. [`bright_pass`]: `max(0, x - threshold)` per channel, weighted by a smoothstep that runs from
//!    `threshold - softness` to `threshold + softness`, applied to all four premultiplied channels
//!    (alpha included) so the halo carries coverage.
//! 2. [`GlowKernel::build`]: Falloff 0 is the plain Gaussian (`sigma = radius / 2`, read out to
//!    6 sigma). Falloff above 0 is five round exponentials `exp(-r / lambda)` with doubling decay
//!    lengths and geometric weights, each truncated at 16 of its own decay lengths with a
//!    smoothstep fade over the last third and renormalized, so the weights always sum to 1 and the
//!    halo never adds or removes light.
//! 3. [`halo`]: the convolution, with edges clamped to the nearest pixel so the halo keeps its
//!    strength along the frame border.
//! 4. [`apply_fringe`]: the chromatic fringe on the finished halo only (the picture under it is
//!    never displaced): a uniform offset of `amount% x radius` along `angle`, either the classic
//!    three per-channel-normalized taps or the spectral gradient of [`apply_fringe`]'s Wavelength
//!    mode. Alpha is not fringed.
//! 5. [`apply`]: `input + intensity x tint x halo`, with output alpha
//!    `min(1, alpha + intensity x halo alpha)`. Intensity 0 returns the input bit-exactly.
//!
//! The spec's performance section (reduced grids, tent downsampling, Catmull-Rom upsampling) is a
//! GPU concern; this reference convolves directly, which trivially meets the requirements the spec
//! actually tests: energy conservation, the enclosed-energy match, isotropic shape, no visible
//! truncation and no temporal popping.
//!
//! Determinism: every stage is a fixed-order loop over `f32`/`f64` arithmetic with no randomness,
//! so two runs on the same input produce identical bits.

use regia_core::glam::Vec2;

/// The ratio `radius / lambda_0` that gives a round exponential the same energy inside `radius` as
/// the Gaussian with `sigma = radius / 2` (both `1 - e^-2`): the root of `(1 + x) e^-x = e^-2`,
/// about 3.505, so `lambda_0` is about `0.2853 x radius`.
const GAUSSIAN_ENCLOSED_RATIO: f64 = 3.505_242;

/// How many of its own decay lengths each exponential kernel is evaluated out to before the
/// smoothstep fade reaches zero.
const REACH_DECAY_LENGTHS: f64 = 16.0;

/// The chromatic-fringe group of the glow settings (collapsed by default; Amount 0 is off and
/// reads no samples at all).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FringeParams {
    /// Fringe strength as a percent of the glow radius, 0 to 100. 0 disables the fringe; values
    /// above 100 are clamped to 100, so the offset never exceeds the radius.
    pub amount: f32,
    /// Fringe direction in degrees from the +x axis, positive turning toward +y (down in raster
    /// space). The direction is periodic; the slider range is -180 to 180.
    pub angle_deg: f32,
    /// Quality tier: `false` is the classic three-tap split, `true` the spectral gradient of
    /// [`Self::samples`] tinted copies.
    pub wavelength: bool,
    /// Copy count in Wavelength mode, 3 to 64, rounded then clamped so the middle color is always
    /// represented. Ignored in classic mode.
    pub samples: f32,
    /// Tint of the copy shifted one full offset along [`Self::angle_deg`], each channel 0 to 1,
    /// scene-linear.
    pub color1: [f32; 3],
    /// Tint of the unshifted copy (classic) or the middle gradient stop (Wavelength).
    pub color2: [f32; 3],
    /// Tint of the copy shifted one full offset against [`Self::angle_deg`].
    pub color3: [f32; 3],
}

impl Default for FringeParams {
    fn default() -> Self {
        Self {
            amount: 0.0,
            angle_deg: 0.0,
            wavelength: false,
            samples: 16.0,
            color1: [1.0, 0.0, 0.0],
            color2: [0.0, 1.0, 0.0],
            color3: [0.0, 0.0, 1.0],
        }
    }
}

/// The glow settings this reference models: the standard bloom baseline (bright pass, radius,
/// intensity, tint) plus the two delta features, the long-tail Falloff and the [`FringeParams`]
/// chromatic fringe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlowParams {
    /// Bright-pass threshold in scene-linear units, 0 to 4 (open above). Channels at or below it
    /// never glow.
    pub threshold: f32,
    /// Bright-pass softness, 0 to 1: the smoothstep onset runs from `threshold - softness` to
    /// `threshold + softness`. 0 is a hard step at the threshold.
    pub softness: f32,
    /// Halo radius in pixels at composition size, 0 to 200 (open above). The Gaussian sigma is
    /// `radius / 2` and the fringe offset is a percent of it. 0 makes the halo the bright pass
    /// itself.
    pub radius: f32,
    /// Falloff, 0 to 8 (open above): 0 selects the Gaussian kernel family, any value above 0 the
    /// long-tail exponential sum. Negative values are treated as 0.
    pub falloff: f32,
    /// Add-back strength, 0 to 10 (open above). 0 returns the input bit-exactly.
    pub intensity: f32,
    /// Per-channel color of the added light, 0 to 4 (HDR allowed). Never affects alpha.
    pub tint: [f32; 3],
    /// The chromatic-fringe group.
    pub fringe: FringeParams,
}

impl Default for GlowParams {
    fn default() -> Self {
        Self {
            threshold: 0.8,
            softness: 0.5,
            radius: 24.0,
            falloff: 0.0,
            intensity: 1.0,
            tint: [1.0, 1.0, 1.0],
            fringe: FringeParams::default(),
        }
    }
}

/// Failures of [`apply`], [`halo`] and [`apply_fringe`] on malformed buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GlowError {
    /// The declared width or height is zero.
    #[error("glow: width and height must be nonzero")]
    EmptyImage,
    /// The buffer does not match the declared shape.
    #[error("glow: expected {expected} elements for a {width}x{height} buffer, got {got}")]
    BufferLength {
        /// The width the caller declared.
        width: u32,
        /// The height the caller declared.
        height: u32,
        /// `width * height` pixels ([`halo`], [`apply_fringe`]) or `width * height * 4` f32s
        /// ([`apply`]).
        expected: usize,
        /// What the buffer actually held.
        got: usize,
    },
}

/// A discrete, radially symmetric halo kernel: the weights convolved with the bright pass.
///
/// At Falloff 0 the taps are a Gaussian with `sigma = radius / 2` evaluated out to `6 sigma`. Above
/// 0 they are the weighted sum of five round exponentials with doubling decay lengths, each
/// truncated at 16 decay lengths with a smoothstep fade over the last third. Either way the taps
/// sum to 1, so the halo conserves the bright pass's energy whatever the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct GlowKernel {
    /// The kernel's half width in taps; the grid is `(2 * half_width + 1)` square.
    half_width: usize,
    /// The taps, row-major by `(dy + half_width, dx + half_width)`, summing to 1.
    taps: Vec<f64>,
}

impl GlowKernel {
    /// Builds the kernel for a halo of `radius` pixels at `falloff`.
    ///
    /// `radius <= 0` (and NaN, treated as 0) gives the single-tap identity, so the halo is the
    /// bright pass itself. `falloff <= 0` (and NaN) selects the Gaussian family; every value above
    /// 0 selects the exponential sum, whose first decay length is calibrated to keep the energy
    /// inside `radius` equal to the Gaussian's.
    ///
    /// Cost note: this reference evaluates every exponential out to 16 of its own decay lengths,
    /// so the grid grows quadratically with radius (about `2 x 73 x radius + 1` taps square); it is
    /// meant for test sizes.
    #[must_use]
    pub fn build(radius: f32, falloff: f32) -> Self {
        let radius = radius.max(0.0);
        let falloff = falloff.max(0.0);
        if radius == 0.0 {
            return Self {
                half_width: 0,
                taps: vec![1.0],
            };
        }
        if falloff == 0.0 {
            gaussian_kernel(radius)
        } else {
            longtail_kernel(radius, falloff)
        }
    }

    /// The kernel's half width in taps; offsets beyond it carry zero weight.
    #[must_use]
    pub fn half_width(&self) -> usize {
        self.half_width
    }

    /// The weight at integer offset `(dx, dy)` from the center tap, `0` outside the grid.
    #[must_use]
    pub fn tap(&self, dx: i32, dy: i32) -> f64 {
        let hw = self.half_width;
        if dx.unsigned_abs() as usize > hw || dy.unsigned_abs() as usize > hw {
            return 0.0;
        }
        let size = 2 * hw + 1;
        self.taps[(dy + hw as i32) as usize * size + (dx + hw as i32) as usize]
    }

    /// The sum of all taps: 1 up to floating-point rounding.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.taps.iter().sum()
    }

    /// The fraction of the kernel's energy carried by taps at or inside `radius` pixels of the
    /// center (tap centers at integer offsets).
    #[must_use]
    pub fn energy_inside(&self, radius: f32) -> f64 {
        let limit = f64::from(radius);
        let hw = self.half_width as i32;
        let mut inside = 0.0;
        for dy in -hw..=hw {
            for dx in -hw..=hw {
                if ((dx * dx + dy * dy) as f64).sqrt() <= limit {
                    inside += self.tap(dx, dy);
                }
            }
        }
        inside
    }

    /// The fraction of the kernel's energy beyond `radius` pixels: `1 - energy_inside`.
    #[must_use]
    pub fn energy_beyond(&self, radius: f32) -> f64 {
        1.0 - self.energy_inside(radius)
    }
}

/// The normalized mixture weights of the five exponential kernels for `falloff`.
///
/// Kernel `k` gets the previous kernel's weight times `rho = falloff / (1 + falloff)` (computed as
/// `1 - 1 / (1 + falloff)` so an infinite falloff gives `rho = 1`, not NaN), then the five weights
/// are normalized to sum to 1. Falloff 1 therefore gives the ratio 16:8:4:2:1, an unbounded falloff
/// gives equal weights, and a negative falloff is treated as 0 (all energy in the tightest kernel).
#[must_use]
pub fn kernel_weights(falloff: f32) -> [f64; 5] {
    let rho = 1.0 - 1.0 / (1.0 + f64::from(falloff.max(0.0)));
    let mut raw = [1.0f64; 5];
    let mut w = 1.0;
    for r in raw.iter_mut().skip(1) {
        w *= rho;
        *r = w;
    }
    let total: f64 = raw.iter().sum();
    raw.map(|w| w / total)
}

/// The bright pass of one premultiplied pixel: `max(0, x - threshold)` per channel, multiplied by a
/// smoothstep weight that runs from `threshold - softness` to `threshold + softness`.
///
/// All four channels use the same threshold and softness, alpha included, so the halo carries
/// coverage and glow spreads over transparent areas. At or below the threshold the result is always
/// zero; softness only eases the onset above it. Softness <= 0 (and NaN, clamped to 0) is a hard
/// step.
#[must_use]
pub fn bright_pass(px: [f32; 4], threshold: f32, softness: f32) -> [f32; 4] {
    let t = f64::from(threshold);
    let s = f64::from(softness.max(0.0));
    let mut out = [0.0f32; 4];
    for c in 0..4 {
        let v = f64::from(px[c]);
        let above = (v - t).max(0.0);
        let weight = if s > 0.0 {
            smoothstep01((v - (t - s)) / (2.0 * s))
        } else if v > t {
            1.0
        } else {
            0.0
        };
        out[c] = (above * weight) as f32;
    }
    out
}

/// Convolves the bright pass with `kernel`, reading the source through clamp-to-edge addressing
/// (out-of-frame samples clamp to the nearest pixel), so the halo keeps its strength along the
/// frame border instead of fading where the kernel crosses the edge.
///
/// This is the gather form a clamp-to-edge sampler computes. A light away from the border gets
/// exactly its free-space halo (every read that finds it is unclamped), and a light on the border
/// keeps its strength because the share that would spread outside folds back onto the border
/// pixels. Accumulation runs in f64 and is quantized to f32 once, at the end.
///
/// # Errors
/// Returns [`GlowError::EmptyImage`] for a zero dimension and [`GlowError::BufferLength`] when
/// `bright` does not hold exactly `width * height` pixels.
pub fn halo(
    bright: &[[f32; 4]],
    width: u32,
    height: u32,
    kernel: &GlowKernel,
) -> Result<Vec<[f32; 4]>, GlowError> {
    if width == 0 || height == 0 {
        return Err(GlowError::EmptyImage);
    }
    let (w, h) = (width as usize, height as usize);
    if bright.len() != w * h {
        return Err(GlowError::BufferLength {
            width,
            height,
            expected: w * h,
            got: bright.len(),
        });
    }
    let (wi, hi) = (w as i32, h as i32);
    let hw = kernel.half_width as i32;
    let mut out = Vec::with_capacity(bright.len());
    for ty in 0..hi {
        for tx in 0..wi {
            let mut acc = [0.0f64; 4];
            for dy in -hw..=hw {
                for dx in -hw..=hw {
                    let weight = kernel.tap(dx, dy);
                    if weight == 0.0 {
                        continue;
                    }
                    let sx = (tx - dx).clamp(0, wi - 1) as usize;
                    let sy = (ty - dy).clamp(0, hi - 1) as usize;
                    let s = bright[sy * w + sx];
                    for c in 0..4 {
                        acc[c] += f64::from(s[c]) * weight;
                    }
                }
            }
            out.push([acc[0] as f32, acc[1] as f32, acc[2] as f32, acc[3] as f32]);
        }
    }
    Ok(out)
}

/// The uniform fringe offset vector for a halo of `radius`: `amount% x radius` along the fringe
/// angle, measured from the +x axis with positive angles turning toward +y (down in raster space).
///
/// The offset is the same everywhere in the frame (it never grows with distance from the center),
/// so color appears along the halo's own edges. Amount is clamped to 0..=100; amount 0 gives the
/// zero vector.
#[must_use]
pub fn fringe_offset(fringe: &FringeParams, radius: f32) -> Vec2 {
    let amount = fringe.amount.clamp(0.0, 100.0);
    let len = f64::from(amount) * 0.01 * f64::from(radius.max(0.0));
    let rad = f64::from(fringe.angle_deg).to_radians();
    Vec2::new((len * rad.cos()) as f32, (len * rad.sin()) as f32)
}

/// The sample offsets of the fringe's tinted copies, in copy order.
///
/// Classic mode is `[offset, zero, -offset]`: Color 1's copy displaced one full offset along the
/// angle, Color 2's copy unshifted, Color 3's copy displaced against it. Wavelength mode spreads
/// `samples` copies from one full offset along the angle to one full offset against it
/// (`u_i = 1 - 2 i / (n - 1)`). Amount 0, a zero radius, or a non-finite offset gives no taps at
/// all: the fringe is off and reads no samples.
#[must_use]
pub fn fringe_tap_offsets(fringe: &FringeParams, radius: f32) -> Vec<Vec2> {
    fringe_taps(fringe, radius).into_iter().map(|(d, _)| d).collect()
}

/// Applies the chromatic fringe to the finished halo, in place.
///
/// RGB is resampled from the offset copies and tinted; alpha stays where it was, so the coverage
/// the glow adds does not split. Samples are bilinear with out-of-frame reads clamped to the edge.
/// Wherever the halo is locally uniform the fringe reproduces it exactly, because the tint weights
/// are normalized per channel to sum to 1 (classic mode) or column-normalized across all copies
/// (Wavelength mode); a channel whose weights are all zero stays zero (deliberately removed).
/// Amount 0 or a zero offset (radius 0) is the identity and reads no samples.
///
/// # Errors
/// Returns [`GlowError::EmptyImage`] for a zero dimension and [`GlowError::BufferLength`] when
/// `halo` does not hold exactly `width * height` pixels.
pub fn apply_fringe(
    halo: &mut [[f32; 4]],
    width: u32,
    height: u32,
    fringe: &FringeParams,
    radius: f32,
) -> Result<(), GlowError> {
    if width == 0 || height == 0 {
        return Err(GlowError::EmptyImage);
    }
    let expected = width as usize * height as usize;
    if halo.len() != expected {
        return Err(GlowError::BufferLength {
            width,
            height,
            expected,
            got: halo.len(),
        });
    }
    let taps = fringe_taps(fringe, radius);
    if taps.is_empty() {
        return Ok(());
    }
    let tints: Vec<[f32; 3]> = taps.iter().map(|&(_, tint)| tint).collect();
    let weights = normalized_tint_columns(&tints);
    let src = halo.to_vec();
    let (w, h) = (width as i64, height as i64);
    for y in 0..h {
        for x in 0..w {
            let mut rgb = [0.0f32; 3];
            for ((displacement, _), wts) in taps.iter().zip(&weights) {
                let s = sample_rgb(
                    &src,
                    width,
                    height,
                    x as f32 - displacement.x,
                    y as f32 - displacement.y,
                );
                for c in 0..3 {
                    rgb[c] += wts[c] * s[c];
                }
            }
            let px = &mut halo[(y * w + x) as usize];
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
        }
    }
    Ok(())
}

/// The whole glow delta over the standard bloom on an interleaved `width x height` RGBA `f32`
/// buffer: bright pass, halo kernel, chromatic fringe and add-back.
///
/// The input is premultiplied scene-linear RGBA. The bright pass runs on all four channels so the
/// halo carries coverage; the output is `input + intensity x tint x halo` per RGB channel with
/// output alpha `min(1, alpha + intensity x halo alpha)`. Tint does not affect alpha and
/// highlights are never clipped. Intensity 0 returns the input bit-exactly.
///
/// # Errors
/// Returns [`GlowError::EmptyImage`] for a zero dimension and [`GlowError::BufferLength`] when the
/// buffer is not `width x height x 4` f32s.
pub fn apply(src: &[f32], width: u32, height: u32, params: &GlowParams) -> Result<Vec<f32>, GlowError> {
    if width == 0 || height == 0 {
        return Err(GlowError::EmptyImage);
    }
    let expected = width as usize * height as usize * 4;
    if !src.len().is_multiple_of(4) || src.len() != expected {
        return Err(GlowError::BufferLength {
            width,
            height,
            expected,
            got: src.len(),
        });
    }
    if params.intensity == 0.0 {
        return Ok(src.to_vec());
    }
    let (pixels, _) = src.as_chunks::<4>();
    let bright: Vec<[f32; 4]> = pixels
        .iter()
        .map(|&px| bright_pass(px, params.threshold, params.softness))
        .collect();
    let kernel = GlowKernel::build(params.radius, params.falloff);
    let mut halo_px = halo(&bright, width, height, &kernel)?;
    apply_fringe(&mut halo_px, width, height, &params.fringe, params.radius)?;
    let intensity = params.intensity;
    let tint = params.tint;
    let mut out = Vec::with_capacity(expected);
    for (inp, hal) in pixels.iter().zip(halo_px) {
        out.extend_from_slice(&[
            inp[0] + intensity * tint[0] * hal[0],
            inp[1] + intensity * tint[1] * hal[1],
            inp[2] + intensity * tint[2] * hal[2],
            (inp[3] + intensity * hal[3]).min(1.0),
        ]);
    }
    Ok(out)
}

/// The Gaussian kernel of the plain bloom: `exp(-r^2 / 2 sigma^2)` with `sigma = radius / 2`,
/// evaluated out to `6 sigma` and normalized to sum to 1.
fn gaussian_kernel(radius: f32) -> GlowKernel {
    let sigma = f64::from(radius) * 0.5;
    let reach = 6.0 * sigma;
    let half_width = reach.ceil().max(1.0) as usize;
    let hw = half_width as i32;
    let mut taps = Vec::with_capacity((2 * half_width + 1).pow(2));
    let mut sum = 0.0;
    for dy in -hw..=hw {
        for dx in -hw..=hw {
            let r2 = f64::from(dx * dx + dy * dy);
            let w = if r2 <= reach * reach {
                (-r2 / (2.0 * sigma * sigma)).exp()
            } else {
                0.0
            };
            sum += w;
            taps.push(w);
        }
    }
    for t in &mut taps {
        *t /= sum;
    }
    GlowKernel { half_width, taps }
}

/// The long-tail kernel: a weighted sum of five round exponentials with doubling decay lengths.
///
/// Each exponential is truncated at 16 of its own decay lengths with a smoothstep fade over the
/// last third, then normalized to unit energy, so the mixture's weights sum to 1 and Falloff only
/// redistributes light outward, never adding or removing any.
fn longtail_kernel(radius: f32, falloff: f32) -> GlowKernel {
    let weights = kernel_weights(falloff);
    let lambda0 = f64::from(radius) / GAUSSIAN_ENCLOSED_RATIO;
    let lambdas = [
        lambda0,
        lambda0 * 2.0,
        lambda0 * 4.0,
        lambda0 * 8.0,
        lambda0 * 16.0,
    ];
    let reaches = lambdas.map(|l| REACH_DECAY_LENGTHS * l);
    let half_width = reaches[4].ceil() as usize;
    let hw = half_width as i32;
    // Pass 1: each exponential's raw energy over the grid, for the per-kernel normalization.
    let mut sums = [0.0f64; 5];
    for dy in -hw..=hw {
        for dx in -hw..=hw {
            let r = ((dx * dx + dy * dy) as f64).sqrt();
            for k in 0..5 {
                if r < reaches[k] {
                    sums[k] += exponential_tap(r, lambdas[k], reaches[k]);
                }
            }
        }
    }
    // Pass 2: the normalized mixture, renormalized once more so rounding cannot leak energy.
    let mut taps = Vec::with_capacity((2 * half_width + 1).pow(2));
    let mut total = 0.0;
    for dy in -hw..=hw {
        for dx in -hw..=hw {
            let r = ((dx * dx + dy * dy) as f64).sqrt();
            let mut w = 0.0;
            for k in 0..5 {
                if r < reaches[k] {
                    w += weights[k] * exponential_tap(r, lambdas[k], reaches[k]) / sums[k];
                }
            }
            total += w;
            taps.push(w);
        }
    }
    for t in &mut taps {
        *t /= total;
    }
    GlowKernel { half_width, taps }
}

/// One un-normalized tap of a round exponential `exp(-r / lambda)`, faded to zero by a smoothstep
/// over the last third of its reach so the truncation at `reach` (16 decay lengths) never shows a
/// visible edge.
fn exponential_tap(r: f64, lambda: f64, reach: f64) -> f64 {
    if r >= reach {
        return 0.0;
    }
    let span = reach / 3.0;
    (-r / lambda).exp() * smoothstep01((reach - r) / span)
}

/// The smoothstep curve `t * t * (3 - 2t)` with `t` clamped to 0..1.
fn smoothstep01(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The sample displacement and tint of each fringe copy: the classic three taps, or the Wavelength
/// gradient spread. Empty when the fringe is off (amount 0, zero radius, or a non-finite offset).
fn fringe_taps(fringe: &FringeParams, radius: f32) -> Vec<(Vec2, [f32; 3])> {
    if fringe.amount.clamp(0.0, 100.0) <= 0.0 {
        return Vec::new();
    }
    let offset = fringe_offset(fringe, radius);
    let length_sq = offset.length_squared();
    if length_sq.is_nan() || length_sq <= 0.0 {
        return Vec::new();
    }
    if !fringe.wavelength {
        return vec![
            (offset, fringe.color1),
            (Vec2::ZERO, fringe.color2),
            (-offset, fringe.color3),
        ];
    }
    let n = tap_count(fringe.samples);
    (0..n)
        .map(|i| {
            let u = 1.0 - 2.0 * (i as f64) / ((n - 1) as f64);
            (
                offset * (u as f32),
                gradient_tint(u, fringe.color1, fringe.color2, fringe.color3),
            )
        })
        .collect()
}

/// The Wavelength-mode copy count: `samples` rounded, then clamped to 3..=64 so the middle color
/// stop is always represented.
fn tap_count(samples: f32) -> usize {
    (samples.round() as i64).clamp(3, 64) as usize
}

/// The gradient tint at position `u`: +1 is `color1` (along the angle), 0 is `color2`, -1 is
/// `color3` (against it), linearly interpolated between the stops.
fn gradient_tint(u: f64, color1: [f32; 3], color2: [f32; 3], color3: [f32; 3]) -> [f32; 3] {
    let (target, t) = if u >= 0.0 { (color1, u) } else { (color3, -u) };
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let base = f64::from(color2[c]);
        out[c] = (base + (f64::from(target[c]) - base) * t) as f32;
    }
    out
}

/// Per-channel normalized tint weights: each channel's column of `tints` divided by its sum, or
/// left at zero when the sum is zero or negative (a channel deliberately removed). This is what
/// makes a uniform halo pass through the fringe unchanged in both modes.
fn normalized_tint_columns(tints: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let mut out = vec![[0.0f32; 3]; tints.len()];
    for c in 0..3 {
        let sum: f64 = tints.iter().map(|t| f64::from(t[c])).sum();
        if sum > 0.0 {
            for (o, t) in out.iter_mut().zip(tints.iter()) {
                o[c] = (f64::from(t[c]) / sum) as f32;
            }
        }
    }
    out
}

/// Bilinear RGB sample of an image at pixel-space `(x, y)`, texel `i` spanning `[i, i + 1)`, with
/// out-of-frame reads clamped to the edge.
fn sample_rgb(img: &[[f32; 4]], width: u32, height: u32, x: f32, y: f32) -> [f32; 3] {
    let (w, h) = (width as i64, height as i64);
    let corner = |ix: i64, iy: i64| -> [f32; 3] {
        let p = img[(iy.clamp(0, h - 1) * w + ix.clamp(0, w - 1)) as usize];
        [p[0], p[1], p[2]]
    };
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (x0, y0) = (fx as i64, fy as i64);
    let c00 = corner(x0, y0);
    let c10 = corner(x0 + 1, y0);
    let c01 = corner(x0, y0 + 1);
    let c11 = corner(x0 + 1, y0 + 1);
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        let top = c00[c] + (c10[c] - c00[c]) * tx;
        let bottom = c01[c] + (c11[c] - c01[c]) * tx;
        out[c] = top + (bottom - top) * ty;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w x h` image that is black but for `value` at `(x, y)`.
    fn white_point(w: u32, h: u32, x: u32, y: u32, value: [f32; 4]) -> Vec<[f32; 4]> {
        let mut img = vec![[0.0, 0.0, 0.0, 0.0]; (w * h) as usize];
        img[(y * w + x) as usize] = value;
        img
    }

    /// A `w x h` image filled with `value`.
    fn flat(w: u32, h: u32, value: [f32; 4]) -> Vec<[f32; 4]> {
        vec![value; (w * h) as usize]
    }

    /// A `w x h` interleaved RGBA f32 buffer filled with `value`.
    fn flat_f32(w: u32, h: u32, value: [f32; 4]) -> Vec<f32> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&value);
        }
        v
    }

    #[test]
    fn kernel_weights_sum_to_one() {
        for (radius, falloff) in [
            (8.0f32, 0.0f32),
            (8.0, 1.0),
            (3.0, 8.0),
            (2.0, 0.05),
            (4.0, f32::INFINITY),
        ] {
            let k = GlowKernel::build(radius, falloff);
            assert!(
                (k.sum() - 1.0).abs() < 1e-9,
                "radius {radius}, falloff {falloff}: sum {}",
                k.sum()
            );
        }
        // radius 0: the single identity tap
        let k = GlowKernel::build(0.0, 3.0);
        assert_eq!(k.half_width(), 0);
        assert_eq!(k.tap(0, 0), 1.0);
        assert_eq!(k.tap(1, 0), 0.0);
        assert_eq!(k.tap(-1, -1), 0.0);
    }

    #[test]
    fn falloff_zero_is_the_plain_gaussian() {
        let radius = 8.0f32;
        let sigma = f64::from(radius) * 0.5;
        let reach = 6.0 * sigma;
        let k = GlowKernel::build(radius, 0.0);
        assert_eq!(k.half_width(), reach.ceil() as usize);
        // An independent normalization of the same formula must reproduce every tap.
        let mut norm = 0.0;
        for dy in -24i32..=24 {
            for dx in -24i32..=24 {
                let r2 = f64::from(dx * dx + dy * dy);
                if r2 <= reach * reach {
                    norm += (-r2 / (2.0 * sigma * sigma)).exp();
                }
            }
        }
        for dy in -30i32..=30 {
            for dx in -30i32..=30 {
                let r2 = f64::from(dx * dx + dy * dy);
                let expected = if r2 <= reach * reach {
                    (-r2 / (2.0 * sigma * sigma)).exp() / norm
                } else {
                    0.0
                };
                let got = k.tap(dx, dy);
                assert!(
                    (got - expected).abs() < 1e-12,
                    "tap ({dx},{dy}): {got} vs {expected}"
                );
            }
        }
    }

    #[test]
    fn longtail_tail_is_heavier_than_gaussian_beyond_three_sigma() {
        let radius = 6.0f32;
        let gauss = GlowKernel::build(radius, 0.0);
        let tail = GlowKernel::build(radius, 1.0);
        let three_sigma = radius * 1.5; // sigma = radius / 2
        let g = gauss.energy_beyond(three_sigma);
        let t = tail.energy_beyond(three_sigma);
        // The Gaussian keeps only e^-4.5 (~1.1%) beyond 3 sigma; the long tail carries far more.
        assert!(g < 0.02, "gaussian beyond 3 sigma: {g}");
        assert!(t > 0.05, "long tail beyond 3 sigma: {t}");
        assert!(t > 3.0 * g, "long tail {t} must dwarf gaussian {g}");
    }

    #[test]
    fn enclosed_energy_at_radius_matches_the_gaussian() {
        // Just above Falloff 0 nearly all energy sits in the first exponential, whose decay length
        // is calibrated so its energy inside Radius equals the Gaussian's (both 1 - e^-2).
        let radius = 8.0f32;
        let gauss = GlowKernel::build(radius, 0.0);
        let tail = GlowKernel::build(radius, 0.01);
        let continuous = 1.0 - std::f64::consts::E.powi(-2);
        let g = gauss.energy_inside(radius);
        let t = tail.energy_inside(radius);
        assert!((g - continuous).abs() < 0.02, "gaussian inside R: {g}");
        assert!((t - continuous).abs() < 0.02, "long tail inside R: {t}");
        assert!((g - t).abs() < 0.01, "enclosed-energy mismatch: {g} vs {t}");
    }

    #[test]
    fn falloff_one_weights_are_sixteen_eight_four_two_one() {
        let w = kernel_weights(1.0);
        let total: f64 = w.iter().sum();
        assert!((total - 1.0).abs() < 1e-12);
        let expected = [1.0, 0.5, 0.25, 0.125, 0.0625];
        for k in 0..5 {
            let ratio = w[k] / w[0];
            assert!(
                (ratio - expected[k]).abs() < 1e-12,
                "kernel {k}: {ratio} vs {}",
                expected[k]
            );
        }
        // An unbounded falloff gives equal weights, not NaN.
        let w = kernel_weights(f32::INFINITY);
        for (k, weight) in w.iter().enumerate() {
            assert!((weight - 0.2).abs() < 1e-12, "kernel {k}: {weight}");
        }
        // A negative falloff is treated as 0: all energy in the tightest kernel.
        let w = kernel_weights(-4.0);
        assert!((w[0] - 1.0).abs() < 1e-12);
        assert!(w.iter().skip(1).all(|&x| x < 1e-9));
    }

    #[test]
    fn bright_pass_is_zero_at_or_below_the_threshold() {
        let (t, s) = (0.8f32, 0.5f32);
        assert_eq!(bright_pass([0.0, 0.4, 0.8, 0.6], t, s), [0.0; 4]);
        assert_eq!(bright_pass([-1.0, 0.0, 0.79, t], t, s), [0.0; 4]);
        // Above: (x - threshold) weighted by the smoothstep from t - s to t + s.
        let x = 1.3f32;
        let w = smoothstep01((f64::from(x) - f64::from(t - s)) / (2.0 * f64::from(s)));
        let want = ((f64::from(x) - f64::from(t)).max(0.0) * w) as f32;
        let got = bright_pass([x, x, x, x], t, s);
        for (c, channel) in got.iter().enumerate() {
            assert!((channel - want).abs() < 1e-7, "channel {c}: {channel} vs {want}");
        }
        // Softness 0 is a hard step: nothing at or below the threshold, the excess above it.
        let got = bright_pass([0.79, 0.8, 0.81, 4.0], t, 0.0);
        assert_eq!(got[0], 0.0);
        assert_eq!(got[1], 0.0);
        assert!(got[2] > 0.0 && got[2] < 0.02, "0.81 gives {}", got[2]);
        assert!((got[3] - 3.2).abs() < 1e-6, "alpha gives {}", got[3]);
    }

    #[test]
    fn nothing_at_or_below_the_threshold_glow() {
        let params = GlowParams {
            radius: 3.0,
            falloff: 1.0,
            ..Default::default()
        };
        let value = [0.8, 0.5, 0.2, 0.8]; // every channel <= threshold 0.8
        assert_eq!(bright_pass(value, params.threshold, params.softness), [0.0; 4]);
        let src = flat_f32(6, 6, value);
        let out = apply(&src, 6, 6, &params).expect("6x6 shape");
        assert_eq!(out, src);
    }

    #[test]
    fn intensity_zero_returns_the_input_bit_exactly() {
        let params = GlowParams {
            intensity: 0.0,
            radius: 3.0,
            ..Default::default()
        };
        let mut src = flat_f32(4, 4, [0.2, 0.3, 0.4, 1.0]);
        // HDR alpha above 1 must survive the early-out untouched.
        for px in src.as_chunks_mut::<4>().0 {
            px[3] = 2.5;
        }
        let out = apply(&src, 4, 4, &params).expect("4x4 shape");
        assert_eq!(out, src);
    }

    #[test]
    fn radius_zero_halo_is_the_bright_pass_itself() {
        let mut params = GlowParams {
            radius: 0.0,
            falloff: 2.0,
            ..Default::default()
        }; // falloff irrelevant at radius 0
        params.fringe.amount = 100.0; // offset = amount% x radius = 0: still the identity
        let mut src = flat_f32(5, 5, [0.1, 0.2, 0.3, 0.4]);
        src.as_chunks_mut::<4>().0[12] = [0.1, 3.0, 0.3, 3.0]; // a bright pixel at (2, 2)
        let out = apply(&src, 5, 5, &params).expect("5x5 shape");
        let (pixels, _) = src.as_chunks::<4>();
        let bright: Vec<[f32; 4]> = pixels
            .iter()
            .map(|&px| bright_pass(px, params.threshold, params.softness))
            .collect();
        // The identity kernel reproduces the bright pass exactly.
        let hal = halo(&bright, 5, 5, &GlowKernel::build(0.0, 2.0)).expect("5x5 shape");
        assert_eq!(hal, bright);
        for ((&inp, h), got) in pixels.iter().zip(&hal).zip(out.as_chunks::<4>().0) {
            for c in 0..3 {
                let want = inp[c] + params.intensity * params.tint[c] * h[c];
                assert!((got[c] - want).abs() < 1e-6);
            }
            assert_eq!(got[3], (inp[3] + params.intensity * h[3]).min(1.0));
        }
        // The bright pixel's alpha glow clamps at 1.
        assert_eq!(out.as_chunks::<4>().0[12][3], 1.0);
    }

    #[test]
    fn negative_falloff_is_treated_as_zero() {
        assert_eq!(GlowKernel::build(4.0, -2.0), GlowKernel::build(4.0, 0.0));
    }

    #[test]
    fn fringe_amount_zero_is_the_identity() {
        let mut img = white_point(9, 9, 4, 4, [1.0, 1.0, 1.0, 1.0]);
        let before = img.clone();
        apply_fringe(&mut img, 9, 9, &FringeParams::default(), 5.0).expect("9x9 shape");
        assert_eq!(img, before);
    }

    #[test]
    fn classic_fringe_splits_a_white_point_along_the_angle() {
        // Offset = amount% x radius = 5 px along angle 0 (+x).
        let mut img = white_point(17, 17, 8, 8, [1.0, 1.0, 1.0, 1.0]);
        let fringe = FringeParams {
            amount: 100.0,
            ..Default::default()
        };
        apply_fringe(&mut img, 17, 17, &fringe, 5.0).expect("17x17 shape");
        // Red displaced along +x, green unshifted, blue against; alpha stays where it was.
        for (i, p) in img.iter().enumerate() {
            let (x, y) = ((i % 17) as u32, (i / 17) as u32);
            let expected = match (x, y) {
                (13, 8) => [1.0, 0.0, 0.0, 0.0],
                (8, 8) => [0.0, 1.0, 0.0, 1.0],
                (3, 8) => [0.0, 0.0, 1.0, 0.0],
                _ => [0.0, 0.0, 0.0, 0.0],
            };
            assert_eq!(p, &expected, "pixel ({x},{y})");
        }
    }

    #[test]
    fn fringe_offset_points_along_the_angle() {
        let fr = |amount: f32, angle: f32| FringeParams {
            amount,
            angle_deg: angle,
            ..Default::default()
        };
        let v = fringe_offset(&fr(50.0, 0.0), 8.0);
        assert!((v.x - 4.0).abs() < 1e-5 && v.y.abs() < 1e-5, "angle 0: {v:?}");
        let v = fringe_offset(&fr(100.0, 90.0), 8.0);
        assert!(v.x.abs() < 1e-5 && (v.y - 8.0).abs() < 1e-5, "angle 90: {v:?}");
        // +y is down in raster space: -90 degrees points up.
        let v = fringe_offset(&fr(100.0, -90.0), 8.0);
        assert!(v.x.abs() < 1e-5 && (v.y + 8.0).abs() < 1e-5, "angle -90: {v:?}");
        // Amount above 100 clamps to 100: the offset never exceeds the radius.
        let v = fringe_offset(&fr(250.0, 0.0), 7.0);
        assert!((v.x - 7.0).abs() < 1e-5 && v.y.abs() < 1e-5, "clamped: {v:?}");
        assert_eq!(fringe_offset(&fr(0.0, 30.0), 9.0), Vec2::ZERO);
        // Tap offsets: classic is [along, unshifted, against], wavelength spreads n copies.
        let classic = fringe_tap_offsets(&fr(100.0, 0.0), 5.0);
        assert_eq!(
            classic,
            vec![Vec2::new(5.0, 0.0), Vec2::ZERO, Vec2::new(-5.0, 0.0)]
        );
        let wave = fringe_tap_offsets(
            &FringeParams {
                amount: 100.0,
                wavelength: true,
                samples: 16.0,
                ..Default::default()
            },
            6.0,
        );
        assert_eq!(wave.len(), 16);
        assert_eq!(wave[0], Vec2::new(6.0, 0.0));
        assert_eq!(wave[15], Vec2::new(-6.0, 0.0));
        assert!(fringe_tap_offsets(&FringeParams::default(), 5.0).is_empty());
    }

    #[test]
    fn fringe_passes_a_uniform_halo_through_unchanged() {
        let mut img = flat(11, 11, [0.25, 0.5, 0.75, 1.0]);
        let fringe = FringeParams {
            amount: 40.0,
            angle_deg: 30.0,
            ..Default::default()
        };
        apply_fringe(&mut img, 11, 11, &fringe, 6.0).expect("11x11 shape");
        for p in &img {
            let want = [0.25, 0.5, 0.75];
            for c in 0..3 {
                assert!((p[c] - want[c]).abs() < 1e-6, "channel {c} moved: {}", p[c]);
            }
            assert_eq!(p[3], 1.0);
        }
    }

    #[test]
    fn classic_channel_with_all_zero_tints_stays_zero() {
        let mut img = flat(4, 4, [0.5, 0.5, 0.5, 1.0]);
        let fringe = FringeParams {
            amount: 100.0,
            color1: [0.0, 1.0, 0.0],
            color2: [0.0, 1.0, 1.0],
            color3: [0.0, 1.0, 0.0],
            ..Default::default()
        };
        apply_fringe(&mut img, 4, 4, &fringe, 2.0).expect("4x4 shape");
        for p in &img {
            assert_eq!(p[0], 0.0, "red was deliberately removed");
            assert!((p[1] - 0.5).abs() < 1e-6);
            assert!((p[2] - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn wavelength_mode_with_three_samples_equals_the_classic_split() {
        let make = || {
            let mut img = vec![[0.0f32; 4]; 15 * 15];
            for (i, p) in img.iter_mut().enumerate() {
                let x = (i % 15) as f32;
                *p = [0.01 * x, 0.02 * x, 0.03 * x, 1.0];
            }
            img
        };
        let mut classic = make();
        let mut wave = make();
        apply_fringe(
            &mut classic,
            15,
            15,
            &FringeParams {
                amount: 60.0,
                ..Default::default()
            },
            5.0,
        )
        .expect("classic shape");
        apply_fringe(
            &mut wave,
            15,
            15,
            &FringeParams {
                amount: 60.0,
                wavelength: true,
                samples: 3.0,
                ..Default::default()
            },
            5.0,
        )
        .expect("wave shape");
        assert_eq!(classic, wave);
    }

    #[test]
    fn wavelength_samples_are_rounded_and_clamped_to_3_64() {
        let make = || {
            let mut img = vec![[0.0f32; 4]; 13 * 13];
            for (i, p) in img.iter_mut().enumerate() {
                let c = (i % 13) as f32 * 0.05;
                *p = [c, c * 0.5, 1.0 - c, 1.0];
            }
            img
        };
        for (a, b) in [(2.0f32, 3.0f32), (100.0, 64.0), (3.7, 4.0), (63.4, 63.0)] {
            let mut x = make();
            let mut y = make();
            apply_fringe(
                &mut x,
                13,
                13,
                &FringeParams {
                    amount: 80.0,
                    wavelength: true,
                    samples: a,
                    ..Default::default()
                },
                4.0,
            )
            .expect("x shape");
            apply_fringe(
                &mut y,
                13,
                13,
                &FringeParams {
                    amount: 80.0,
                    wavelength: true,
                    samples: b,
                    ..Default::default()
                },
                4.0,
            )
            .expect("y shape");
            assert_eq!(x, y, "samples {a} should behave as {b}");
        }
    }

    #[test]
    fn wavelength_mode_spreads_a_rainbow_along_the_offset() {
        // Offset 6 px along +x, 16 copies spread from -6 to +6: red weight leaks to the along end,
        // blue to the against end, and each channel's total is conserved.
        let mut img = white_point(19, 19, 9, 9, [1.0, 1.0, 1.0, 1.0]);
        let fringe = FringeParams {
            amount: 100.0,
            wavelength: true,
            samples: 16.0,
            ..Default::default()
        };
        apply_fringe(&mut img, 19, 19, &fringe, 6.0).expect("19x19 shape");
        let px = |x: u32| img[(9 * 19 + x) as usize];
        // The along end is strongly red (the outermost copy plus its inner neighbor's bilinear
        // foot), the against end strongly blue.
        assert!(px(15)[0] > 0.2, "red end: {:?}", px(15));
        assert!(px(15)[0] > 4.0 * px(15)[2], "red end ratio: {:?}", px(15));
        assert!(px(3)[2] > 0.2, "blue end: {:?}", px(3));
        assert!(px(3)[2] > 4.0 * px(3)[0], "blue end ratio: {:?}", px(3));
        // Column normalization conserves each channel's total energy.
        for c in 0..3 {
            let total: f64 = img.iter().map(|p| f64::from(p[c])).sum();
            assert!((total - 1.0).abs() < 1e-5, "channel {c} total: {total}");
        }
        // Alpha is not fringed: it stays on the point.
        assert_eq!(img[(9 * 19 + 9) as usize][3], 1.0);
        assert_eq!(img[(9 * 19 + 15) as usize][3], 0.0);
    }

    #[test]
    fn halo_keeps_its_strength_along_the_frame_border() {
        // A bright corner pixel: the share of the kernel that would spread outside folds back, so
        // the corner keeps the whole +x/+y quadrant of the energy (more than a quarter, far more
        // than its free-space center tap).
        let kernel = GlowKernel::build(4.0, 0.0);
        let hw = kernel.half_width() as i32;
        let mut quadrant = 0.0;
        for dy in 0..=hw {
            for dx in 0..=hw {
                quadrant += kernel.tap(dx, dy);
            }
        }
        assert!(quadrant > 0.25, "quadrant share: {quadrant}");
        let b = [2.0, 2.0, 2.0, 1.0];
        let bright = white_point(9, 9, 0, 0, b);
        let hal = halo(&bright, 9, 9, &kernel).expect("9x9 shape");
        for c in 0..4 {
            let want = (quadrant * f64::from(b[c])) as f32;
            assert!(
                (hal[0][c] - want).abs() < 1e-5,
                "corner channel {c}: {} vs {want}",
                hal[0][c]
            );
        }
    }

    #[test]
    fn interior_light_gets_its_free_space_halo() {
        // A light at the center of a 25x25 frame with a radius-4 Gaussian (reach 12): no read
        // clamps, so the halo is the free-space splat, pixel for pixel, and energy is conserved.
        let kernel = GlowKernel::build(4.0, 0.0);
        let b = [2.0, 2.0, 2.0, 1.0];
        let bright = white_point(25, 25, 12, 12, b);
        let hal = halo(&bright, 25, 25, &kernel).expect("25x25 shape");
        let px = |x: i32, y: i32| hal[(y * 25 + x) as usize];
        for (dx, dy) in [(1, 0), (0, 1), (-2, 3), (5, -5)] {
            let want = (kernel.tap(dx, dy) * f64::from(b[0])) as f32;
            assert!((px(12 + dx, 12 + dy)[0] - want).abs() < 1e-6);
        }
        for c in 0..4 {
            let total: f64 = hal.iter().map(|p| f64::from(p[c])).sum();
            assert!(
                (total - f64::from(b[c])).abs() < 1e-4,
                "channel {c} total: {total}"
            );
        }
    }

    #[test]
    fn falloff_redistributes_energy_without_adding_or_removing_it() {
        // Every kernel carries the same total energy (weights sum to 1) whatever the falloff;
        // falloff only moves light outward.
        let mut previous_beyond = f64::MIN;
        for falloff in [0.0f32, 0.5, 1.0, 8.0] {
            let k = GlowKernel::build(2.0, falloff);
            assert!((k.sum() - 1.0).abs() < 1e-9, "falloff {falloff}: {}", k.sum());
            let beyond = k.energy_beyond(3.0);
            assert!(
                beyond > previous_beyond,
                "falloff {falloff}: {beyond} must outrun the previous {previous_beyond}"
            );
            previous_beyond = beyond;
        }
    }

    #[test]
    fn apply_is_deterministic() {
        let params = GlowParams {
            radius: 2.0,
            falloff: 1.0,
            intensity: 1.5,
            tint: [1.0, 0.5, 0.25],
            fringe: FringeParams {
                amount: 30.0,
                angle_deg: 45.0,
                wavelength: true,
                samples: 7.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut src = flat_f32(9, 9, [0.1, 0.1, 0.1, 1.0]);
        src.as_chunks_mut::<4>().0[40] = [4.0, 3.0, 2.0, 1.0]; // a bright HDR pixel at the centre
        let a = apply(&src, 9, 9, &params).expect("9x9 shape");
        let b = apply(&src, 9, 9, &params).expect("9x9 shape");
        assert_eq!(a, b);
        // The bright pixel gained light; the dark corner kept its alpha at 1.
        assert!(a.as_chunks::<4>().0[40][0] > 4.0);
        let corner = a.as_chunks::<4>().0[0];
        assert!(corner[0] >= 0.1 && corner[1] >= 0.1 && corner[2] >= 0.1);
        assert_eq!(corner[3], 1.0);
    }

    #[test]
    fn apply_matches_its_staged_reference_pieces() {
        let params = GlowParams {
            radius: 2.0,
            falloff: 4.0,
            intensity: 1.5,
            tint: [1.0, 0.5, 0.25],
            fringe: FringeParams {
                amount: 50.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut src = flat_f32(9, 9, [0.05, 0.05, 0.05, 1.0]);
        src.as_chunks_mut::<4>().0[40] = [4.0, 3.0, 2.0, 1.0];
        let out = apply(&src, 9, 9, &params).expect("9x9 shape");
        let (pixels, _) = src.as_chunks::<4>();
        let bright: Vec<[f32; 4]> = pixels
            .iter()
            .map(|&px| bright_pass(px, params.threshold, params.softness))
            .collect();
        let kernel = GlowKernel::build(params.radius, params.falloff);
        let mut hal = halo(&bright, 9, 9, &kernel).expect("9x9 shape");
        apply_fringe(&mut hal, 9, 9, &params.fringe, params.radius).expect("9x9 shape");
        for ((&inp, h), got) in pixels.iter().zip(&hal).zip(out.as_chunks::<4>().0) {
            for c in 0..3 {
                let want = inp[c] + params.intensity * params.tint[c] * h[c];
                assert!((got[c] - want).abs() < 1e-6, "channel {c}: {got:?} vs {want}");
            }
            assert_eq!(got[3], (inp[3] + params.intensity * h[3]).min(1.0));
        }
    }

    #[test]
    fn apply_rejects_malformed_buffers() {
        let params = GlowParams::default();
        assert_eq!(apply(&[0.0; 16], 0, 4, &params), Err(GlowError::EmptyImage));
        assert_eq!(apply(&[0.0; 16], 4, 0, &params), Err(GlowError::EmptyImage));
        assert_eq!(
            apply(&[0.0; 15], 2, 2, &params),
            Err(GlowError::BufferLength {
                width: 2,
                height: 2,
                expected: 16,
                got: 15
            })
        );
        assert_eq!(
            apply(&[0.0; 20], 2, 2, &params),
            Err(GlowError::BufferLength {
                width: 2,
                height: 2,
                expected: 16,
                got: 20
            })
        );
        assert_eq!(
            halo(&[[0.0; 4]; 5], 2, 3, &GlowKernel::build(1.0, 0.0)),
            Err(GlowError::BufferLength {
                width: 2,
                height: 3,
                expected: 6,
                got: 5
            })
        );
        assert_eq!(
            apply_fringe(&mut [[0.0; 4]; 7], 2, 3, &FringeParams::default(), 4.0),
            Err(GlowError::BufferLength {
                width: 2,
                height: 3,
                expected: 6,
                got: 7
            })
        );
    }

    #[test]
    fn defaults_match_the_spec_tables() {
        let p = GlowParams::default();
        assert_eq!(p.threshold, 0.8);
        assert_eq!(p.softness, 0.5);
        assert_eq!(p.radius, 24.0);
        assert_eq!(p.falloff, 0.0);
        assert_eq!(p.intensity, 1.0);
        assert_eq!(p.tint, [1.0, 1.0, 1.0]);
        assert_eq!(p.fringe, FringeParams::default());
        assert_eq!(p.fringe.amount, 0.0);
        assert_eq!(p.fringe.angle_deg, 0.0);
        assert!(!p.fringe.wavelength);
        assert_eq!(p.fringe.samples, 16.0);
        assert_eq!(p.fringe.color1, [1.0, 0.0, 0.0]);
        assert_eq!(p.fringe.color2, [0.0, 1.0, 0.0]);
        assert_eq!(p.fringe.color3, [0.0, 0.0, 1.0]);
    }
}
