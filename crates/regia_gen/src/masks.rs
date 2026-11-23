// SPDX-License-Identifier: MPL-2.0
//! Image mask operations over [`FloatImage`]: threshold, levels, blend, box blur, invert. Every
//! operation returns a new image and is deterministic to the bit (fixed summation order, `f64`
//! sums, and `regia_core::math::det::det_powf` for the levels gamma).

use serde::{Deserialize, Serialize};

use crate::{FloatImage, GenError};

/// The largest blur radius.
pub const MAX_BLUR_RADIUS: u32 = 256;

/// The most blur passes.
pub const MAX_BLUR_PASSES: u32 = 8;

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 1 where the value is at least `at`, else 0. A positive `softness` replaces the step with a
/// smoothstep over `[at − softness, at + softness]`.
pub fn threshold(image: &FloatImage, at: f32, softness: f32) -> Result<FloatImage, GenError> {
    crate::finite("threshold", &[at, softness])?;
    if softness < 0.0 {
        return Err(GenError::InvalidInput(
            "threshold softness must not be negative".into(),
        ));
    }
    let f = |v: f32| {
        if softness > 0.0 {
            smoothstep((v - (at - softness)) / (2.0 * softness))
        } else if v >= at {
            1.0
        } else {
            0.0
        }
    };
    Ok(map(image, f))
}

/// The levels adjustment: `[in_low, in_high]` maps to `[0, 1]` (clamped), raised to `gamma`, then
/// mapped onto `[out_low, out_high]`. `gamma > 1` darkens the mid-tones, `< 1` brightens them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Levels {
    /// Input black point (default 0).
    pub in_low: f32,
    /// Input white point, above `in_low` (default 1).
    pub in_high: f32,
    /// Exponent applied to the normalized value, positive (default 1).
    pub gamma: f32,
    /// Output black (default 0).
    pub out_low: f32,
    /// Output white (default 1).
    pub out_high: f32,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            in_low: 0.0,
            in_high: 1.0,
            gamma: 1.0,
            out_low: 0.0,
            out_high: 1.0,
        }
    }
}

/// Applies [`Levels`].
pub fn levels(image: &FloatImage, l: &Levels) -> Result<FloatImage, GenError> {
    crate::finite("levels", &[l.in_low, l.in_high, l.gamma, l.out_low, l.out_high])?;
    if l.in_high <= l.in_low {
        return Err(GenError::InvalidInput(
            "levels in_high must be above in_low".into(),
        ));
    }
    if l.gamma <= 0.0 {
        return Err(GenError::InvalidInput("levels gamma must be positive".into()));
    }
    let span = l.in_high - l.in_low;
    Ok(map(image, |v| {
        let t = ((v - l.in_low) / span).clamp(0.0, 1.0);
        let t = if l.gamma == 1.0 {
            t
        } else {
            regia_core::math::det::det_powf(t, l.gamma)
        };
        l.out_low + (l.out_high - l.out_low) * t
    }))
}

/// `1 − v` per pixel.
pub fn invert(image: &FloatImage) -> FloatImage {
    map(image, |v| 1.0 - v)
}

fn map(image: &FloatImage, f: impl Fn(f32) -> f32) -> FloatImage {
    FloatImage {
        width: image.width,
        height: image.height,
        pixels: image.pixels.iter().map(|&v| f(v)).collect(),
    }
}

/// How [`blend`] combines the base `a` with the layer `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    /// `b`.
    Mix,
    /// `a · b`.
    Multiply,
    /// `a + b`, clamped to `[0, 1]`.
    Add,
    /// `a − b`, clamped to `[0, 1]`.
    Subtract,
    /// `max(a, b)`.
    Max,
    /// `min(a, b)`.
    Min,
    /// `1 − (1 − a)(1 − b)`.
    Screen,
    /// `2ab` where `a < 0.5`, else `1 − 2(1 − a)(1 − b)`.
    Overlay,
}

/// Blends layer `b` over base `a`: `a + (mode(a, b) − a) · opacity`. The images must be the
/// same size; `opacity` is in `[0, 1]`.
pub fn blend(a: &FloatImage, b: &FloatImage, mode: BlendMode, opacity: f32) -> Result<FloatImage, GenError> {
    a.same_size(b)?;
    if !(opacity.is_finite() && (0.0..=1.0).contains(&opacity)) {
        return Err(GenError::InvalidInput(format!(
            "blend opacity must be in [0, 1], got {opacity}"
        )));
    }
    let op = |x: f32, y: f32| match mode {
        BlendMode::Mix => y,
        BlendMode::Multiply => x * y,
        BlendMode::Add => (x + y).clamp(0.0, 1.0),
        BlendMode::Subtract => (x - y).clamp(0.0, 1.0),
        BlendMode::Max => x.max(y),
        BlendMode::Min => x.min(y),
        BlendMode::Screen => 1.0 - (1.0 - x) * (1.0 - y),
        BlendMode::Overlay => {
            if x < 0.5 {
                2.0 * x * y
            } else {
                1.0 - 2.0 * (1.0 - x) * (1.0 - y)
            }
        }
    };
    Ok(FloatImage {
        width: a.width,
        height: a.height,
        pixels: a
            .pixels
            .iter()
            .zip(&b.pixels)
            .map(|(&x, &y)| {
                if opacity == 1.0 {
                    op(x, y)
                } else {
                    x + (op(x, y) - x) * opacity
                }
            })
            .collect(),
    })
}

/// A separable box blur of `radius` pixels, repeated `passes` times (three passes approximate a
/// Gaussian). `wrap` samples across the opposite edge (for tileable images); otherwise edge
/// pixels repeat outward. Each output is the mean of `2·radius + 1` samples summed in order in
/// `f64`.
pub fn blur(image: &FloatImage, radius: u32, passes: u32, wrap: bool) -> Result<FloatImage, GenError> {
    let _span = tracing::trace_span!("regia_gen::blur").entered();
    if radius > MAX_BLUR_RADIUS {
        return Err(GenError::TooLarge {
            what: "blur radius",
            value: radius as usize,
            limit: MAX_BLUR_RADIUS as usize,
        });
    }
    if passes > MAX_BLUR_PASSES {
        return Err(GenError::TooLarge {
            what: "blur passes",
            value: passes as usize,
            limit: MAX_BLUR_PASSES as usize,
        });
    }
    let mut out = image.clone();
    if radius == 0 {
        return Ok(out);
    }
    let (w, h) = (image.width as i64, image.height as i64);
    let r = i64::from(radius);
    let count = (2 * r + 1) as f64;
    let index = |i: i64, n: i64| if wrap { i.rem_euclid(n) } else { i.clamp(0, n - 1) };
    let mut tmp = vec![0.0f32; out.pixels.len()];
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0.0f64;
                for k in -r..=r {
                    sum += f64::from(out.pixels[(y * w + index(x + k, w)) as usize]);
                }
                tmp[(y * w + x) as usize] = (sum / count) as f32;
            }
        }
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0.0f64;
                for k in -r..=r {
                    sum += f64::from(tmp[(index(y + k, h) * w + x) as usize]);
                }
                out.pixels[(y * w + x) as usize] = (sum / count) as f32;
            }
        }
    }
    Ok(out)
}
