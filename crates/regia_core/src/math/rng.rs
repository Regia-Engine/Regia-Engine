// SPDX-License-Identifier: MIT
// Portions ported from glam (https://github.com/bitshifter/glam-rs,
// commit e7d680ed34c051cfa8741f76821628a925161280), used under the MIT licence.
// Copyright 2020 Cameron Hart.
// DirectXMath attribution retained from glam's ATTRIBUTION.md:
// Copyright (c) 2011-2020 Microsoft Corp.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Seedable, deterministic random numbers: PCG32 (XSH-RR, 64-bit state), implemented in-house.
//!
//! Algorithm from Melissa E. O'Neill, "PCG: A Family of Simple Fast Space-Efficient Statistically Good
//! Algorithms for Random Number Generation" (2014) and the public `pcg32` reference description.

use std::ops::{Range, RangeInclusive};

use glam::{Quat, Vec3};
use serde::{Deserialize, Serialize};

const MULTIPLIER: u64 = 6_364_136_223_846_793_005;

/// A small, fast, seedable random number generator (PCG32). The same seed and stream always give the
/// same sequence on every platform, so it is safe for deterministic simulation and replays. Not for
/// cryptography.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rng {
    state: u64,
    inc: u64,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Rng {
    /// Stream used by [`Rng::new`].
    pub const DEFAULT_STREAM: u64 = 0xcafe_f00d_d15e_a5e5;

    /// A generator seeded with `seed` on the default stream.
    pub fn new(seed: u64) -> Self {
        Self::with_stream(seed, Self::DEFAULT_STREAM)
    }

    /// A generator with an explicit stream: different streams give independent sequences for the same seed.
    /// Matches the reference `pcg32_srandom(seed, stream)`.
    pub fn with_stream(seed: u64, stream: u64) -> Self {
        let mut rng = Self {
            state: 0,
            inc: (stream << 1) | 1,
        };
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed);
        rng.next_u32();
        rng
    }

    /// A generator seeded from the operating system's entropy source (non-deterministic).
    pub fn from_entropy() -> Self {
        let bits = uuid::Uuid::new_v4().as_u128();
        Self::with_stream(bits as u64, (bits >> 64) as u64)
    }

    /// A new generator seeded from this one, for handing independent deterministic sequences to
    /// subsystems.
    pub fn fork(&mut self) -> Self {
        let seed = self.next_u64();
        let stream = self.next_u64();
        Self::with_stream(seed, stream)
    }

    /// The next 32 random bits.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULTIPLIER).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// The next 64 random bits (two 32-bit outputs, high word first).
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let hi = u64::from(self.next_u32());
        let lo = u64::from(self.next_u32());
        (hi << 32) | lo
    }

    /// Uniform in `[0, 1)` with 24 bits of precision.
    #[inline]
    pub fn f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / (1u32 << 24) as f32)
    }

    /// Uniform in `[0, 1)` with 53 bits of precision.
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// A fair coin flip.
    #[inline]
    pub fn bool(&mut self) -> bool {
        self.next_u32() >> 31 == 1
    }

    /// `true` with probability `p` (clamped to `[0, 1]`).
    #[inline]
    pub fn chance(&mut self, p: f32) -> bool {
        self.f32() < p
    }

    /// Uniform in `range` (`a..b` or `a..=b`; integers or floats). An empty range returns its start.
    #[inline]
    pub fn range<T, R: SampleRange<T>>(&mut self, range: R) -> T {
        range.sample(self)
    }

    /// Uniform in `[0, bound)` without modulo bias (rejection as in the reference `pcg32_boundedrand`).
    /// `bound` must be non-zero.
    fn below_u32(&mut self, bound: u32) -> u32 {
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let r = self.next_u32();
            if r >= threshold {
                return r % bound;
            }
        }
    }

    fn below_u64(&mut self, bound: u64) -> u64 {
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let r = self.next_u64();
            if r >= threshold {
                return r % bound;
            }
        }
    }

    /// A uniformly distributed unit vector (Archimedes: uniform `z` and angle give a uniform sphere).
    pub fn unit_vector(&mut self) -> Vec3 {
        let z = self.range(-1.0f32..=1.0);
        let turns = self.f32();
        let r = (1.0 - z * z).max(0.0).sqrt();
        let (sin, cos) = det_sin_cos_turns(turns);
        Vec3::new(r * cos, r * sin, z)
    }

    /// A point uniformly distributed inside the unit ball.
    pub fn in_sphere(&mut self) -> Vec3 {
        let v = self.unit_vector();
        v * det_cbrt(self.f32())
    }

    /// Shuffles `slice` uniformly (Fisher–Yates).
    pub fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.range(0..=i);
            slice.swap(i, j);
        }
    }

    /// A uniformly chosen element, or `None` if `slice` is empty.
    pub fn choose<'a, T>(&mut self, slice: &'a [T]) -> Option<&'a T> {
        if slice.is_empty() {
            None
        } else {
            slice.get(self.range(0..slice.len()))
        }
    }
}

/// Ranges [`Rng::range`] can sample from.
pub trait SampleRange<T> {
    /// Draws one value.
    fn sample(self, rng: &mut Rng) -> T;
}

macro_rules! int_ranges {
    ($($t:ty, $same:ty, $wide:ty, $below:ident, $raw:ident;)*) => {$(
        impl SampleRange<$t> for Range<$t> {
            #[inline]
            fn sample(self, rng: &mut Rng) -> $t {
                if self.start >= self.end {
                    return self.start;
                }
                let span = self.end.wrapping_sub(self.start) as $same as $wide;
                self.start.wrapping_add(rng.$below(span) as $t)
            }
        }

        impl SampleRange<$t> for RangeInclusive<$t> {
            #[inline]
            fn sample(self, rng: &mut Rng) -> $t {
                let (start, end) = self.into_inner();
                if start > end {
                    return start;
                }
                let span = (end.wrapping_sub(start) as $same as $wide).wrapping_add(1);
                if span == 0 {
                    // The full range of the type.
                    return rng.$raw() as $t;
                }
                start.wrapping_add(rng.$below(span) as $t)
            }
        }
    )*};
}

int_ranges! {
    u8, u8, u32, below_u32, next_u32;
    i8, u8, u32, below_u32, next_u32;
    u16, u16, u32, below_u32, next_u32;
    i16, u16, u32, below_u32, next_u32;
    u32, u32, u32, below_u32, next_u32;
    i32, u32, u32, below_u32, next_u32;
    u64, u64, u64, below_u64, next_u64;
    i64, u64, u64, below_u64, next_u64;
    usize, usize, u64, below_u64, next_u64;
    isize, usize, u64, below_u64, next_u64;
}

macro_rules! float_ranges {
    ($($t:ty, $unit:ident, $bits:expr, $raw:ident, $shift:expr;)*) => {$(
        impl SampleRange<$t> for Range<$t> {
            #[inline]
            fn sample(self, rng: &mut Rng) -> $t {
                // Always consume one variate so a zero-width range keeps the stream aligned with the
                // integer ranges.
                let unit = rng.$unit();
                let non_empty = self.start < self.end;
                if !non_empty {
                    return self.start;
                }
                let span = self.end - self.start;
                if !span.is_finite() {
                    // Halve both ends so the span of a near-full-width range cannot overflow.
                    let (a, b) = (self.start * 0.5, self.end * 0.5);
                    let v = (a + (b - a) * unit) * 2.0;
                    return if v < self.end { v } else { self.end.next_down() };
                }
                let v = self.start + span * unit;
                // Rounding can land exactly on `end`; keep the range half-open.
                if v < self.end { v } else { self.end.next_down() }
            }
        }

        impl SampleRange<$t> for RangeInclusive<$t> {
            #[inline]
            fn sample(self, rng: &mut Rng) -> $t {
                let (start, end) = self.into_inner();
                // Uniform in [0, 1], both ends reachable; drawn even for an empty range (see above).
                let unit = (rng.$raw() >> $shift) as $t / ((1u64 << $bits) - 1) as $t;
                let non_empty = start < end;
                if !non_empty {
                    return start;
                }
                if !(end - start).is_finite() {
                    let (a, b) = (start * 0.5, end * 0.5);
                    return ((a + (b - a) * unit) * 2.0).min(end);
                }
                (start + (end - start) * unit).min(end)
            }
        }
    )*};
}

float_ranges! {
    f32, f32, 24, next_u32, 8;
    f64, f64, 53, next_u64, 11;
}

/// `(sin, cos)` of `turns * 2π`, from plain `+`/`*` on `f64` (Rust never fuses them), so the result is
/// bit-identical on every target instead of following the platform libm.
fn det_sin_cos_turns(turns: f32) -> (f32, f32) {
    let (sin, cos) = super::det::sin_cos_quarters(f64::from(turns) * 4.0);
    (sin as f32, cos as f32)
}

/// `cos(radians)` from the same kernel: the same bits on every target, for thresholds a fixed-step
/// decision compares against, where the platform libm's last digit would split lockstep peers
/// (spec 15 § 5). The argument is scaled in `f64`: an `f32` division here costs up to 6e-7 by 10 rad.
pub fn det_cos(radians: f32) -> f32 {
    super::det::sin_cos_quarters(f64::from(radians) / core::f64::consts::FRAC_PI_2).1 as f32
}

/// `(sin, cos)` of `radians` from the same kernel as [`det_cos`]: the same bits on every target.
/// The pair a placement path writes into the world (formation and spatial-query offsets), where the
/// platform libm's `sin`/`cos` may differ by an ulp between Linux, Windows and Android.
pub fn det_sin_cos(radians: f32) -> (f32, f32) {
    let (sin, cos) = super::det::sin_cos_quarters(f64::from(radians) / core::f64::consts::FRAC_PI_2);
    (sin as f32, cos as f32)
}

/// `e^x`, the same bits on every target (`2^n` scaling by exact exponent steps, then a Taylor
/// series on `|f| ≤ ln 2 / 2`; the construction of `regia_procgen::detmath::exp2`).
pub fn det_exp(x: f32) -> f32 {
    det_exp2_ai(f64::from(x) * core::f64::consts::LOG2_E) as f32
}

/// `2^x` on `f64`. Returns 0 below −1074 and +∞ above 1024.
fn det_exp2_ai(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x > 1024.0 {
        return f64::INFINITY;
    }
    if x < -1074.0 {
        return 0.0;
    }
    let n = (x + 0.5).floor();
    let f = (x - n) * core::f64::consts::LN_2; // |f| ≤ ln2 / 2
    // e^f by its Taylor series to f^14.
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..15 {
        term *= f / f64::from(k);
        sum += term;
    }
    scale_pow2(sum, n as i32)
}

/// `v · 2^n` by exact exponent steps.
fn scale_pow2(mut v: f64, mut n: i32) -> f64 {
    while n > 1000 {
        v *= f64::from_bits(((1023 + 1000) as u64) << 52);
        n -= 1000;
    }
    while n < -1000 {
        v *= f64::from_bits(((1023 - 1000) as u64) << 52);
        n += 1000;
    }
    v * f64::from_bits(((1023 + n) as u64) << 52)
}

/// `ln(x)`; NaN for negative input, −∞ at 0, +∞ at +∞. The same bits on every target
/// (exponent/mantissa split, then an atanh series on the mantissa, as
/// `regia_procgen::detmath::log2`).
pub fn det_ln(x: f32) -> f32 {
    let x = f64::from(x);
    if x.is_nan() || x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 {
        return f32::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f32::INFINITY;
    }
    (det_log2(x) * core::f64::consts::LN_2) as f32
}

/// `log2(x)` on `f64`.
fn det_log2(x: f64) -> f64 {
    // Normalize subnormals.
    let (x, bias) = if x < f64::MIN_POSITIVE {
        (x * f64::from_bits((1023 + 64) << 52), -64.0)
    } else {
        (x, 0.0)
    };
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52)); // [1, 2)
    if m > core::f64::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    // ln m = 2 atanh(s), s = (m − 1)/(m + 1), |s| ≤ 0.1716.
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = 0.0;
    let mut n = 1.0;
    for _ in 0..12 {
        sum += term / n;
        term *= s2;
        n += 2.0;
    }
    e as f64 + bias + 2.0 * sum / core::f64::consts::LN_2
}

/// `base^exponent`, the same bits on every target (via `det_exp2_ai` and `det_log2` below). Negative
/// `base` needs an integral `exponent`; `0^y` is 0 (y > 0), 1 (y = 0) or +∞ (y < 0), as the
/// platform `powf` defines.
pub fn det_powf(base: f32, exponent: f32) -> f32 {
    let (base, exponent) = (f64::from(base), f64::from(exponent));
    if base.is_nan() || exponent.is_nan() {
        return f32::NAN;
    }
    if exponent == 0.0 {
        return 1.0;
    }
    if base == 0.0 {
        return if exponent > 0.0 { 0.0 } else { f32::INFINITY };
    }
    if base < 0.0 {
        if exponent.floor() != exponent {
            return f32::NAN;
        }
        let r = det_exp2_ai(exponent * det_log2(-base)) as f32;
        let odd = (exponent / 2.0).floor() * 2.0 != exponent;
        return if odd { -r } else { r };
    }
    det_exp2_ai(exponent * det_log2(base)) as f32
}

/// `sin(radians)` from the same kernel: the same bits on every target. The animation root-motion
/// path builds yaw quaternions and slerps with this and [`det_cos`] instead of the platform
/// `sin_cos`, whose last digit would split lockstep peers (spec 73 § 6.7, ADR-4653).
pub fn det_sin(radians: f32) -> f32 {
    super::det::sin_cos_quarters(f64::from(radians) / core::f64::consts::FRAC_PI_2).0 as f32
}

/// `acos` on `[-1, 1]`, the same bits on every target: a port of glam's `acos_approx`
/// (`Quat::slerp`'s angle step), itself the 7-degree minimax from DirectXMath's `XMScalarAcos`
/// (MIT sources, see docs/provenance.md). Plain `f32` arithmetic and `sqrt` only; the measured
/// worst error on `[-1, 1]` is about 5e-7 rad (the `sqrt(1 − x)` factor shrinks the polynomial
/// error at the ends), so use it where bit identity matters more than the last digits (the
/// pinned quaternion slerp, ADR-4653).
pub fn det_acos_approx(v: f32) -> f32 {
    let nonnegative = v >= 0.0;
    let x = v.abs();
    let mut omx = 1.0 - x;
    if omx < 0.0 {
        omx = 0.0;
    }
    let root = omx.sqrt();
    // This is a fitted minimax coefficient, not pi/2; retain its bits (ADR-4852).
    #[allow(clippy::approx_constant)]
    let mut result = ((((((-0.001_262_491_1 * x + 0.006_670_09) * x - 0.017_088_126) * x + 0.030_891_88)
        * x
        - 0.050_174_303)
        * x
        + 0.088_978_99)
        * x
        - 0.214_598_8)
        * x
        + 1.570_796_3;
    result *= root;
    if nonnegative {
        result
    } else {
        core::f32::consts::PI - result
    }
}

/// `asin(x)` on `f64` from plain `+`/`*` and the exactly rounded `sqrt`: the same bits on every
/// target. Taylor series on `|x| <= 0.5` (coefficient recurrence `c_n = c_{n-1} * (2n-1)/(2n)`),
/// with the halving identity `asin(x) = π/2 - 2 asin(sqrt((1-x)/2))` above 0.5 (and its odd
/// mirror below -0.5), where `(1-x)/2` is exact by Sterbenz' lemma. 30 terms put the truncation
/// error below 1e-18 on the reduced range.
fn det_asin(x: f64) -> f64 {
    fn asin_reduced(x: f64) -> f64 {
        let x2 = x * x;
        let mut c = 1.0_f64;
        let mut term = x;
        let mut sum = x;
        for n in 1..30_usize {
            c *= (2 * n - 1) as f64 / (2 * n) as f64;
            term *= x2;
            sum += c * term / (2 * n + 1) as f64;
        }
        sum
    }
    if x > 0.5 {
        core::f64::consts::FRAC_PI_2 - 2.0 * asin_reduced(((1.0 - x) * 0.5).sqrt())
    } else if x < -0.5 {
        -core::f64::consts::FRAC_PI_2 + 2.0 * asin_reduced(((1.0 + x) * 0.5).sqrt())
    } else {
        asin_reduced(x)
    }
}

/// `acos(x) = π/2 - asin(x)` on the deterministic kernel (see `det_asin`).
fn det_acos_f64(x: f64) -> f64 {
    core::f64::consts::FRAC_PI_2 - det_asin(x)
}

/// `acos(x)` for `x ∈ [-1, 1]`, returning radians: the same bits on every target. The same
/// kernel as [`det_cos`], inverted — for sweep-angle bounds a fixed-step decision compares
/// against, where the platform libm's last digit would split lockstep peers.
pub fn det_acos(x: f32) -> f32 {
    det_acos_f64(f64::from(x)) as f32
}

/// `2^x` on `f64` from plain `+`/`*`: the same bits on every target. The argument is reduced to a
/// fraction `f ∈ [-0.5, 0.5]` around the nearest integer `k` (`f64::round` is exact, not libm);
/// `2^f` is the Taylor series of `f·ln 2` (18 terms, truncation error below 1e-19) and `2^k`
/// multiplies by an exact power of two built from its exponent bits. Below `2^-1022` the result
/// underflows to zero (no subnormal support — easing magnitudes never reach it).
pub(crate) fn det_exp2(x: f64) -> f64 {
    let k = x.round();
    let y = (x - k) * core::f64::consts::LN_2;
    let mut term = 1.0_f64;
    let mut sum = 1.0_f64;
    for n in 1..=18_usize {
        term *= y / n as f64;
        sum += term;
    }
    let k = k as i64;
    if k > 1023 {
        return f64::INFINITY;
    }
    if k < -1022 {
        return 0.0;
    }
    sum * f64::from_bits(((k + 1023) as u64) << 52)
}

/// Spherical linear interpolation along the shortest arc, deterministic: the same quaternion bits
/// on every target (x86_64 SSE2 without FMA, aarch64 NEON, any libm), unlike `glam::Quat::slerp`,
/// whose fused multiply-add use splits across targets. Rotation is evaluated on the `f64`
/// polynomial kernel (`det::sin_cos_quarters`, `det_acos`) and rounded once to `f32` per component.
///
/// Same contract as `glam::Quat::slerp` for unit inputs: `t` unclamped, the sign-flipped shortest
/// arc when the dot is negative, and a normalised lerp when the quaternions are nearly parallel
/// (dot above `1 - f32::EPSILON`).
pub fn det_slerp(a: Quat, b: Quat, t: f32) -> Quat {
    // f64 products and sums of the f32 components: never fused, unlike an f32 SIMD dot that an
    // FMA-enabled target contracts.
    let dot = f64::from(a.x) * f64::from(b.x)
        + f64::from(a.y) * f64::from(b.y)
        + f64::from(a.z) * f64::from(b.z)
        + f64::from(a.w) * f64::from(b.w);
    let (b, dot) = if dot < 0.0 {
        (Quat::from_xyzw(-b.x, -b.y, -b.z, -b.w), -dot)
    } else {
        (b, dot)
    };
    if dot > f64::from(1.0 - f32::EPSILON) {
        // Nearly parallel: a normalised lerp, matching glam's slerp fallback.
        let x = a.x + (b.x - a.x) * t;
        let y = a.y + (b.y - a.y) * t;
        let z = a.z + (b.z - a.z) * t;
        let w = a.w + (b.w - a.w) * t;
        let inv_len = 1.0 / (x * x + y * y + z * z + w * w).sqrt();
        return Quat::from_xyzw(x * inv_len, y * inv_len, z * inv_len, w * inv_len);
    }
    let theta = det_acos_f64(dot);
    let quarters = theta / core::f64::consts::FRAC_PI_2;
    let sin_theta = super::det::sin_cos_quarters(quarters).0;
    let w0 = super::det::sin_cos_quarters((1.0 - f64::from(t)) * quarters).0 / sin_theta;
    let w1 = super::det::sin_cos_quarters(f64::from(t) * quarters).0 / sin_theta;
    // Blend in f64, one rounding to f32 per component.
    let c = |av: f32, bv: f32| (f64::from(av) * w0 + f64::from(bv) * w1) as f32;
    Quat::from_xyzw(c(a.x, b.x), c(a.y, b.y), c(a.z, b.z), c(a.w, b.w))
}

/// `atan2(y, x)` from an `f64` Taylor kernel with argument reduction (no platform libm), so the
/// result is bit-identical on every target instead of following the platform's `atan2f`. The
/// reduction keeps `|u| <= tan(pi/8)` before the series; the truncation error stays below `1e-18`,
/// far inside the `f32` output's ulp. The zero and non-finite corners follow IEEE 754 and the
/// platform `atan2f` exactly (including both signs of zero): `atan2(±0, +0) = ±0`,
/// `atan2(±0, −0) = ±π`, `atan2(y, ±0) = ±π/2` for `y ≠ 0`, the infinity pairs
/// (`atan2(±∞, ±∞)` = ±π/4, ±3π/4 by quadrant), a single infinite argument (`atan2(±∞, x)` is
/// `±π/2`, `atan2(y, ±∞)` is `±0` or `±π`), and NaN in either input gives NaN.
#[must_use]
pub fn det_atan2(y: f32, x: f32) -> f32 {
    det_atan2_f64(f64::from(y), f64::from(x)) as f32
}

/// `atan2` on `f64`, quadrant by sign: the IEEE-754 special cases first, then the reduced kernel.
fn det_atan2_f64(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    // The zero corners come first and use both sign bits, exactly as IEEE 754 defines them:
    // `atan2(±0, +0) = ±0` and `atan2(±0, −0) = ±π`. Checking `y == 0.0` before `x == 0.0`
    // keeps `atan2(-0, +0)` at `-0` (the platform `atan2f` answer), not `-π/2`.
    if y == 0.0 {
        if x.is_sign_negative() {
            return if y.is_sign_negative() {
                -core::f64::consts::PI
            } else {
                core::f64::consts::PI
            };
        }
        return if y.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if y.is_infinite() {
        // Handle infinities before the division: ∞/∞ would be NaN. `f32::atan2` semantics:
        // both infinite → ±π/4 or ±3π/4 by the x sign; y alone infinite → ±π/2.
        let a = if x.is_infinite() {
            if x < 0.0 {
                3.0 * core::f64::consts::FRAC_PI_4
            } else {
                core::f64::consts::FRAC_PI_4
            }
        } else {
            core::f64::consts::FRAC_PI_2
        };
        return if y.is_sign_negative() { -a } else { a };
    }
    if x.is_infinite() {
        // atan2(y, -∞) is ±π, atan2(y, +∞) is ±0 (the sign of y, including -0).
        if x < 0.0 {
            return if y.is_sign_negative() {
                -core::f64::consts::PI
            } else {
                core::f64::consts::PI
            };
        }
        return if y.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if x == 0.0 {
        return if y.is_sign_negative() {
            -core::f64::consts::FRAC_PI_2
        } else {
            core::f64::consts::FRAC_PI_2
        };
    }
    let a = det_atan_unit(y.abs() / x.abs());
    let a = if x < 0.0 { core::f64::consts::PI - a } else { a };
    // `is_sign_negative`, not `< 0.0`: `-0.0 < 0.0` is false, but atan2(-0, x) is negative.
    if y.is_sign_negative() { -a } else { a }
}

/// `atan(t)` for `t >= 0` (the reciprocal identity leaves the argument at most `1`).
fn det_atan_unit(t: f64) -> f64 {
    if t > 1.0 {
        core::f64::consts::FRAC_PI_2 - det_atan_reduced(1.0 / t)
    } else {
        det_atan_reduced(t)
    }
}

/// `atan(t)` for `0 <= t <= 1`: `atan(t) = pi/4 + atan((t - 1)/(t + 1))` leaves
/// `|u| <= tan(pi/8) = 0.4142...`, where the Taylor series needs few terms.
fn det_atan_reduced(t: f64) -> f64 {
    let (k, u) = if t > 0.414_213_562_373_095_15 {
        (core::f64::consts::FRAC_PI_4, (t - 1.0) / (t + 1.0))
    } else {
        (0.0, t)
    };
    let u2 = u * u;
    // atan(u) = u * (1 - u^2/3 + u^4/5 - ...): Horner, highest term first.
    let mut acc = 0.0;
    for n in (0..=28).rev() {
        let d = 2.0 * f64::from(n) + 1.0;
        acc = 1.0 / d - u2 * acc;
    }
    k + u * acc
}

/// `Quat::from_rotation_y(yaw)` built from the deterministic kernel: the same bits on every
/// target, for the yaw a lockstep step writes and a rollback restores (spec 15 § 5), where the
/// platform libm's last digit would split lockstep peers.
pub fn det_yaw_quat(yaw: f32) -> Quat {
    let (s, c) = super::det::sin_cos_quarters(f64::from(yaw) / core::f64::consts::PI);
    Quat::from_xyzw(0.0, s as f32, 0.0, c as f32)
}

/// The yaw (Y component) of `q`'s YXZ Euler angles, from [`det_atan2`]: the same bits on every
/// target, for yaws replicated across lockstep peers and replays. Well-defined away from the
/// gimbal pose (an X rotation of ±π/2), where the formula's terms are near zero and the result
/// is f32-rounding noise; the upright characters that travel this path never hold that pose, but
/// a ragdoll's recover snap on a prone or supine fallen root can reach it — the value stays
/// deterministic there, only arbitrary (pinned by
/// `recover_snap_yaw_bits_are_pinned_at_the_gimbal_pose`).
pub fn det_yaw_of(q: Quat) -> f32 {
    det_atan2(2.0 * (q.x * q.z + q.w * q.y), 1.0 - 2.0 * (q.x * q.x + q.y * q.y))
}

/// Cube root of `u >= 0` by fixed-count Newton iteration on `f64` (no platform `cbrtf`).
///
/// The seed is one third of the exponent (a bit hack, not libm), so eight steps reach `f64`
/// precision for every positive finite `f32`. A seed of `1.0` does not: past about `1e13` and
/// below about `1e-25` the iteration is still decades away after any cheap fixed count.
/// Negative inputs and NaN follow the old contract (`0` and NaN).
pub fn det_cbrt(u: f32) -> f32 {
    if u.is_nan() {
        return f32::NAN;
    }
    if u <= 0.0 {
        return 0.0;
    }
    if !u.is_finite() {
        return u;
    }
    let v = f64::from(u);
    // `(e + 1023) / 3` re-biases the exponent; the constant puts the seed within a small
    // factor of the root. Same construction as `detmath::det_cbrt`.
    let mut y = f64::from_bits((v.to_bits() / 3).wrapping_add(0x2a9f_a4c5_0000_0000));
    if !(y.is_finite() && y > 0.0) {
        y = 1.0;
    }
    for _ in 0..8 {
        y = (2.0 * y + v / (y * y)) / 3.0;
    }
    y as f32
}

/// `tan(radians)` from the deterministic [`det_sin_cos`] pair: the same bits on every target.
/// Callers pass angles away from `π/2` (cone half angles clamp to 1.55 rad).
pub fn det_tan(radians: f32) -> f32 {
    let (sin, cos) = det_sin_cos(radians);
    sin / cos
}

/// `(sin, cos)` of `radians`, evaluated in `f32` with a fixed sequence of plain `+`/`*`/`/` so a
/// WGSL twin (`det_sin_cos` in `regia_vfx`'s event shaders) can reproduce the result bit for bit:
/// the CPU mirror of the GPU child-spawn sampling must agree with the pool, not with libm.
/// Accuracy is about 1e-7 (an `f32` Taylor series on `[−π/4, π/4]`); any edit here must edit
/// the WGSL twin in the same commit.
pub fn det_sin_cos_f32(radians: f32) -> (f32, f32) {
    let q = radians / core::f32::consts::FRAC_PI_2;
    let k = (q + 0.5).floor();
    let x = (q - k) * core::f32::consts::FRAC_PI_2;
    let x2 = x * x;
    // Taylor series on [-π/4, π/4]; the truncation error is below 1e-6 in f32.
    let mut sin = 0.0f32;
    let mut cos = 0.0f32;
    for n in (0..8).rev() {
        let m = (2 * n) as f32;
        cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
        sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
    }
    let sin = x * sin;
    // `u32(k) % 4` is exactly `rem_euclid(4)` for every integral `k` (4 divides 2^32); the WGSL
    // twin computes it this way because WGSL has no floored remainder.
    match (k as i32 as u32) % 4 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// Cube root of `u >= 0` by Newton iteration in `f32`; the twin of WGSL `det_cbrt` in
/// `regia_vfx`'s event shaders. The recurrence is unchanged. It stops once an iterate equals
/// the next one, which is a fixed point, so the remaining steps of the 48-step WGSL loop stay
/// on that same value. A change to the recurrence itself still has to edit the WGSL twin in
/// the same commit.
pub fn det_cbrt_f32(u: f32) -> f32 {
    if u <= 0.0 {
        return 0.0;
    }
    let mut y = 1.0f32;
    for _ in 0..48 {
        #[cfg(test)]
        CBRT_F32_STEPS.with(|steps| steps.set(steps.get() + 1));
        let next = (2.0 * y + u / (y * y)) / 3.0;
        if next == y {
            break;
        }
        y = next;
    }
    y
}

#[cfg(test)]
thread_local! {
    static CBRT_F32_STEPS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `e^x` on `f64` (the kernel behind [`det_exp`]): range reduction and a fixed-count Taylor
/// series, the same bits on every target. Exposed for `f64` intermediates (debris drag flight)
/// where the extra range matters. Underflows to `0` and overflows to `+∞` at the platform
/// `exp` limits (the subnormal window scales exactly, so the edges round as `e^x` would).
pub fn det_exp_f64(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x == f64::NEG_INFINITY {
        return 0.0;
    }
    const LN_2: f64 = std::f64::consts::LN_2;
    let n = (x / LN_2).round_ties_even();
    let r = x - n * LN_2;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..=20 {
        term *= r / f64::from(k);
        sum += term;
    }
    let n = n as i64;
    if n > 1_024 {
        return f64::INFINITY;
    }
    if n < -1_075 {
        return 0.0;
    }
    // `2^n` by exact exponent steps: the bit-built scale is only valid inside the normal
    // range, so the subnormal and overflow edges take one exact halving/doubling step and let
    // the final multiply round once, as `sum * 2^n` would (review fix-vfx2-determinism
    // finding 1: the unguarded cast returned sign-flipped garbage for n in [-1075, -1024]).
    if (-1_022..=1_023).contains(&n) {
        sum * f64::from_bits(((n + 1023) as u64) << 52)
    } else if n == 1_024 {
        sum * f64::from_bits(2_046_u64 << 52) * 2.0
    } else if n == -1_075 {
        (sum * 0.5) * f64::from_bits(1)
    } else {
        // n in [-1074, -1023]: 2^n = 2^(n + 1074) · 2^-1074, both exact powers of two.
        sum * f64::from_bits(1_u64 << (n + 1_074))
    }
}

/// `ln(x)` on `f64` (the kernel behind [`det_ln`]): binary normalization and a fixed-count atanh
/// series, the same bits on every target.
pub fn det_ln_f64(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x < 0.0 {
        return f64::NAN;
    }
    if x == f64::INFINITY {
        return f64::INFINITY;
    }
    // Normalize subnormals by an exact power of two before the bit split: a subnormal's
    // exponent field reads as 0, so the field arithmetic below would treat its mantissa as
    // 1.f (a value up to 2^52 too large). Scaling by 2^64 is exact and lands in the normal
    // range; the bias restores the exponent below.
    let (x, bias) = if x < f64::MIN_POSITIVE {
        (x * f64::from_bits(((1023 + 64) as u64) << 52), -64i64)
    } else {
        (x, 0i64)
    };
    // Normalize by bit manipulation (no libm): m in [1, 2), then halve into [1/√2, √2).
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7FF) as i64 - 1023 + bias;
    let mut m = f64::from_bits((bits & !(0x7FFu64 << 52)) | (1023u64 << 52));
    if m > std::f64::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    // ln(m) = 2·atanh(z) = 2z·(1 + z²/3 + z⁴/5 + …); 14 terms put the truncation below 1e-17
    // for |z| ≤ √2−1 over √2+1 ≈ 0.1716.
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 0..14 {
        term *= z2;
        sum += term / f64::from(2 * k + 3);
    }
    2.0 * z * sum + e as f64 * std::f64::consts::LN_2
}

/// `x.powf(y)` on `f64` as `det_exp_f64(y * det_ln_f64(x))`: the same bits on every target,
/// unlike `f64::powf`. `x < 0` is `NaN`; `x == 0` is `0` for positive `y` (`+inf` otherwise).
pub fn det_powf_f64(x: f64, y: f64) -> f64 {
    if x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f64::INFINITY };
    }
    det_exp_f64(y * det_ln_f64(x))
}

/// Power on the nonnegative-base domain, using [`det_powf_f64`] and rounding once to `f32`.
///
/// Every negative base returns NaN, including integral exponents. Zero returns zero for
/// positive exponents and positive infinity otherwise. Use [`det_powf`] for integral powers
/// of negative bases. This is the f32 VFX sampling contract (ADR-4911).
pub fn det_powf_nonnegative(base: f32, exponent: f32) -> f32 {
    det_powf_f64(f64::from(base), f64::from(exponent)) as f32
}

#[cfg(test)]
mod vfx_tests {
    use super::det_powf_nonnegative as det_powf;
    use super::*;

    const DET_COS_0_7_BITS: u32 = 0x3f43_ccb3;
    const DET_COS_50_DEG_BITS: u32 = 0x3f24_8dbb;

    #[test]
    fn det_cos_tracks_cos_and_is_pinned_to_its_bits() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            assert!((det_cos(x) - x.cos()).abs() <= 2.0e-7, "{x}");
        }
        assert_eq!(det_cos(0.0), 1.0);
        // Series on plain `f64` `+`/`*`: these bits hold on every target, whatever its libm says.
        assert_eq!(det_cos(0.7).to_bits(), DET_COS_0_7_BITS);
        assert_eq!(det_cos(0.872_664_6).to_bits(), DET_COS_50_DEG_BITS);
    }

    #[test]
    fn det_sin_cos_tracks_libm_and_is_pinned() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            let (s, c) = det_sin_cos(x);
            assert!((s - x.sin()).abs() <= 2.0e-7, "sin {x}");
            assert!((c - x.cos()).abs() <= 2.0e-7, "cos {x}");
        }
        // Pinned: plain `f64` arithmetic, so these bits hold on every target.
        assert_eq!(det_sin_cos(0.7).0.to_bits(), 0x3f24_eb73);
        assert_eq!(det_sin_cos(0.7).1.to_bits(), 0x3f43_ccb3);
        // The f32 twin the WGSL kernel mirrors (regia_vfx event child sampling): coarser
        // accuracy, but every bit pinned, so a WGSL edit that drifts fails the GPU tests.
        let (s, c) = det_sin_cos_f32(0.7);
        assert_eq!(s.to_bits(), 0x3f24_eb73);
        assert_eq!(c.to_bits(), 0x3f43_ccb3);
        let (s, c) = det_sin_cos_f32(4.2);
        assert_eq!(s.to_bits(), 0xbf5f_1f94);
        assert_eq!(c.to_bits(), 0xbefb_0381);
        let (s, c) = det_sin_cos_f32(core::f32::consts::TAU);
        assert_eq!(s.to_bits(), 0x0000_0000);
        assert_eq!(c.to_bits(), 0x3f80_0000);
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            let (s, c) = det_sin_cos_f32(x);
            assert!((s - x.sin()).abs() <= 2.0e-6, "twin sin {x}");
            assert!((c - x.cos()).abs() <= 2.0e-6, "twin cos {x}");
        }
        assert!((det_tan(0.7) - 0.7f32.tan()).abs() < 2.0e-7);
        assert!((det_tan(1.55) - 1.55f32.tan()).abs() < 1.0e-4);
    }

    #[test]
    fn det_cbrt_tracks_cbrt_and_is_pinned() {
        for i in 1..=1000u32 {
            let u = i as f32 / 1000.0;
            assert!((det_cbrt(u) - u.cbrt()).abs() < 1e-6, "cbrt {u}");
            assert!((det_cbrt_f32(u) - u.cbrt()).abs() < 2e-6, "cbrt_f32 {u}");
        }
        assert_eq!(det_cbrt(0.0), 0.0);
        assert_eq!(det_cbrt_f32(0.0), 0.0);
        assert_eq!(det_cbrt_f32(1.0), 1.0);
        // Pinned: the Newton twin differs from libm by one ulp here by design; hold the bits.
        assert_eq!(det_cbrt_f32(0.36).to_bits(), 0x3f36_1ce9);
        assert_eq!(det_cbrt(f32::from_bits(0x3380_0000)).to_bits(), 0x3b80_0000);
    }

    /// Child-spawn samples call [`det_cbrt_f32`] per particle. On a unit-interval
    /// sample the f32 Newton iterate is a fixed point long before 48 steps.
    #[test]
    fn det_cbrt_f32_unit_sample_stops_when_newton_settles() {
        assert_eq!(det_cbrt(0.5).to_bits(), 0x3f4b_2ff5, "det_cbrt(0.5) bits");
        CBRT_F32_STEPS.with(|steps| steps.set(0));
        let f32_bits = det_cbrt_f32(0.36).to_bits();
        let f32_steps = CBRT_F32_STEPS.with(|steps| steps.get());
        assert_eq!(f32_bits, 0x3f36_1ce9, "det_cbrt_f32(0.36) bits");
        assert!(
            f32_steps <= 12,
            "det_cbrt_f32(0.36) took {f32_steps} Newton steps"
        );
    }

    #[test]
    fn det_exp_ln_powf_track_libm() {
        for i in -600..=0 {
            let x = i as f32 * 0.1;
            let got = det_exp(x);
            let want = x.exp();
            assert!(
                (got - want).abs() <= 2.0e-7 * want.max(1e-30),
                "exp({x}): {got} vs {want}"
            );
        }
        assert_eq!(det_exp(-1.0).to_bits(), 0x3ebc_5ab2);
        for x in [0.1f32, 0.5, 1.0, 2.0, 10.0, 150.0, 1e-3] {
            let got = det_ln(x);
            let want = x.ln();
            assert!(
                (got - want).abs() <= 2.0e-7 * want.abs().max(1.0),
                "ln({x}): {got} vs {want}"
            );
            let p = det_powf(x, 1.0 / 7.0);
            let q = x.powf(1.0 / 7.0);
            assert!((p - q).abs() <= 1.0e-6 * q.max(1e-6), "powf({x}): {p} vs {q}");
        }
        // The wind-profile power law the weather surface writes every frame, pinned to its bits.
        assert_eq!(det_powf(10.0 / 1500.0, 1.0 / 7.0).to_bits(), 0x3efa_43ca);
        assert_eq!(det_exp(0.0), 1.0);
        assert_eq!(det_exp(f32::NEG_INFINITY), 0.0);
        assert_eq!(det_ln(1.0), 0.0);
        assert_eq!(det_ln(0.0), f32::NEG_INFINITY);
        assert!(det_ln(-1.0).is_nan());
        assert!(det_powf(-2.0, 0.5).is_nan());
        assert_eq!(det_powf(0.0, 2.0), 0.0);
    }

    #[test]
    fn det_exp_f64_det_ln_f64_at_extreme_ranges() {
        // Regression (Muse review of fix-vfx1-determinism finding 3): the subnormal scaling
        // wrapped into the sign/exponent bits below n = -1023 (det_exp_f64(-710) was NaN,
        // det_exp_f64(-730) a huge negative), and subnormal ln inputs skipped normalization.
        // The kernel's range reduction (x − n·ln 2 in one f64) loses ~log2|n| digits to
        // cancellation, so the tolerance grows with |x|; the subnormal results are exactly
        // rounded (the two-step power-of-two scaling) and pinned bit for bit below.
        for x in [-600.0, -700.0, -709.0, -710.0, -720.0, -730.0, -744.0] {
            let got = det_exp_f64(x);
            let want = x.exp();
            assert!(
                got.is_finite() && got > 0.0,
                "det_exp_f64({x}) must be a positive finite: {got}"
            );
            assert!(
                (got - want).abs() <= want * (1e-15 + 4.0e-16 * x.abs()),
                "det_exp_f64({x}): {got} vs {want}"
            );
        }
        // Bit pins at the subnormal boundary: pre-fix, -710 wrapped the exponent bits into
        // NaN and -730 into a huge negative; -745 rounds up to the smallest subnormal and
        // -750 below half of it to 0.
        assert_eq!(det_exp_f64(-710.0).to_bits(), 0x0003_3802_fd28_b3ad);
        assert_eq!(det_exp_f64(-720.0).to_bits(), 0x0000_0009_93b4_dc95);
        assert_eq!(det_exp_f64(-730.0).to_bits(), 0x0000_0000_001c_7ea3);
        assert_eq!(det_exp_f64(-744.0).to_bits(), 0x0000_0000_0000_0002);
        assert_eq!(det_exp_f64(-745.0), 5e-324);
        assert!(
            det_exp_f64(-745.0) > 0.0,
            "e^-745 rounds up to the smallest subnormal"
        );
        assert_eq!(det_exp_f64(-750.0), 0.0);
        assert_eq!(det_exp_f64(-1080.0), 0.0);
        assert_eq!(det_exp_f64(710.0), 710.0f64.exp());
        assert_eq!(det_exp_f64(f64::INFINITY), f64::INFINITY);
        assert_eq!(det_exp_f64(f64::NEG_INFINITY), 0.0);
        // Subnormal ln inputs, down to the denormal floor: ln(2^-1074) = -1074 ln 2, which is
        // below -744 (the broken version returned -709.09 for 5e-324).
        for k in [1023u32, 1050, 1074] {
            // 2^-k from its bits: `powi`'s precision is unspecified, and on the M5 `2f64.powi(-1050)`
            // is 0 (2^1050 overflows before the reciprocal), which tested ln(0) (ADR-5860).
            let x = f64::from_bits(1u64 << (1074 - k));
            let got = det_ln_f64(x);
            let want = x.ln();
            assert!((got - want).abs() < 1e-13, "det_ln_f64(2^-{k}): {got} vs {want}");
        }
        let min = 5e-324f64;
        assert!(
            det_ln_f64(min) < -744.0,
            "det_ln_f64(5e-324) = {} should be -744.44",
            det_ln_f64(min)
        );
        assert_eq!(det_ln_f64(f64::MIN_POSITIVE), f64::MIN_POSITIVE.ln());
        assert_eq!(det_ln_f64(1.0), 0.0);
        assert_eq!(det_ln_f64(f64::INFINITY), f64::INFINITY);
        assert!(det_ln_f64(-1.0).is_nan());
        assert_eq!(det_ln_f64(0.0), f64::NEG_INFINITY);
        // The debris drag flight and any powf through the f64 kernel stay finite or flush
        // to zero exactly where IEEE underflow says so.
        assert!(det_powf_f64(2.0, -1074.0).is_finite());
        assert!(det_powf_f64(0.5, 1000.0) > 0.0);
        assert_eq!(det_powf_f64(0.5, 2000.0), 0.0);
    }

    #[test]
    fn det_acos_tracks_acos_and_stays_in_range() {
        for i in -1000..=1000 {
            let x = i as f32 / 1000.0;
            let got = det_acos(x);
            assert!((got - x.acos()).abs() <= 2.0e-5, "acos({x}): {got}");
            assert!((0.0..=core::f32::consts::PI).contains(&got));
        }
        assert_eq!(det_acos(1.0), 0.0);
        assert_eq!(det_acos(-1.0), core::f32::consts::PI);
        assert!((det_acos(0.5) - core::f32::consts::FRAC_PI_3).abs() < 1e-6);
        // Pinned: Newton on the deterministic kernel, plain `f64` arithmetic.
        assert_eq!(det_acos(0.5).to_bits(), 0x3f86_0a92);
    }

    #[test]
    fn det_sin_cos_tracks_std_and_is_pinned_to_its_bits() {
        for i in -1000..=1000 {
            let x = i as f32 * 0.01;
            let (s, c) = det_sin_cos(x);
            assert!((s - x.sin()).abs() <= 2.0e-7, "sin {x}");
            assert!((c - x.cos()).abs() <= 2.0e-7, "cos {x}");
        }
        let (s, c) = det_sin_cos(0.7);
        assert_eq!(s.to_bits(), 0x3f24_eb73, "sin(0.7)");
        assert_eq!(c.to_bits(), 0x3f43_ccb3, "cos(0.7)");
        assert_eq!(det_sin_cos(0.0), (0.0, 1.0));
    }

    #[test]
    fn det_exp_tracks_std_and_is_pinned_to_its_bits() {
        for i in -800..=100 {
            let x = i as f32 * 0.1;
            let a = det_exp(x);
            let b = x.exp();
            assert!((a - b).abs() <= 1e-6 * b.abs().max(1e-30), "exp({x}): {a} vs {b}");
        }
        assert_eq!(det_exp(0.0), 1.0);
        assert_eq!(det_exp(f32::NEG_INFINITY), 0.0);
        assert!(det_exp(f32::INFINITY).is_infinite());
        assert_eq!(det_exp(1.0).to_bits(), det_exp(1.0).to_bits());
        // Pinned: recomputing by hand must reproduce these bits on every target.
        assert_eq!(det_exp(1.0f32).to_bits(), 0x402d_f854);
        assert_eq!(det_exp(-1.0f32).to_bits(), 0x3ebc_5ab2);
    }

    #[test]
    fn det_ln_tracks_std_and_is_pinned_to_its_bits() {
        for i in -30..=30 {
            let x = 10f32.powi(i);
            let a = det_ln(x);
            let b = x.ln();
            assert!((a - b).abs() <= 1e-5 * b.abs().max(1.0), "ln({x}): {a} vs {b}");
        }
        for x in [0.1f32, 0.5, 0.75, 1.5, 2.0, 100.0] {
            assert!((det_ln(x) - x.ln()).abs() < 1e-6, "ln({x})");
        }
        assert_eq!(det_ln(1.0), 0.0);
        assert_eq!(det_ln(0.0), f32::NEG_INFINITY);
        assert!(det_ln(-1.0).is_nan());
        assert_eq!(det_ln(2.0f32).to_bits(), 0x3f31_7218);
    }

    #[test]
    fn det_powf_tracks_std_and_is_pinned_to_its_bits() {
        for x in [0.05f32, 0.2, 0.5, 1.0, 2.0, 10.0] {
            for y in [-2.0f32, -0.5, 0.4, 1.0, 2.4] {
                let a = det_powf(x, y);
                let b = x.powf(y);
                assert!(
                    (a - b).abs() <= 2e-5 * b.abs().max(1e-6),
                    "powf({x}, {y}): {a} vs {b}"
                );
            }
        }
        assert_eq!(det_powf(0.0, 1.0), 0.0);
        assert_eq!(det_powf(0.0, -1.0), f32::INFINITY);
        assert!(det_powf(-1.0, 2.0).is_nan());
        assert_eq!(det_powf(4.0, 0.5), 2.0);
        // Pinned across targets: values the VFX atlases quantize to bytes.
        assert_eq!(det_powf(0.5, 2.4).to_bits(), det_powf(0.5, 2.4).to_bits());
        assert_eq!(
            det_powf(0.5, 2.4).to_bits(),
            0x3e42_02ff,
            "pinned after first run"
        );
    }

    /// Review fix-vfx2-determinism finding 1: the `2^n` scaling is only valid for exponents
    /// inside the normal range; the subnormal window used to return sign-flipped garbage
    /// (`det_exp_f64(-720) ≈ -6.6e303`) instead of the underflowed value.
    #[test]
    fn det_exp_f64_underflow_and_overflow_edges() {
        for x in [-709.0, -710.0, -720.0, -730.0, -744.0] {
            let got = det_exp_f64(x);
            let want = x.exp();
            assert!(
                got.is_finite() && got.is_sign_positive(),
                "exp({x}): {got} (want {want})"
            );
            // The platform libm is the accuracy oracle here (bits stay pinned by construction).
            // In the subnormal window the range reduction's `x - n·ln2` cancellation bounds
            // accuracy to ~1e-12 relative (it costs nothing at normal magnitudes); the
            // sign/finite assert above is what catches the garbage class this test pins.
            assert!(
                (got - want).abs() <= want.abs() * 1e-9 + 2.0 * f64::from_bits(1),
                "exp({x}): {got} vs {want}"
            );
        }
        assert_eq!(det_exp_f64(-745.2), 0.0);
        assert_eq!(det_exp_f64(-800.0), 0.0);
        assert_eq!(det_exp_f64(-1000.0), 0.0);
        assert_eq!(det_exp_f64(f64::NEG_INFINITY), 0.0);
        assert_eq!(det_exp_f64(710.0), f64::INFINITY);
        assert_eq!(det_exp_f64(720.0), f64::INFINITY);
        assert_eq!(det_exp_f64(800.0), f64::INFINITY);
        assert_eq!(det_exp_f64(1000.0), f64::INFINITY);
        assert_eq!(det_exp_f64(f64::INFINITY), f64::INFINITY);
        assert!(det_exp_f64(f64::NAN).is_nan());
        // The f32 kernel the in-game decal path uses: deep underflow is exactly zero.
        assert_eq!(det_exp(-800.0_f32), 0.0);
        assert_eq!(det_exp(800.0_f32), f32::INFINITY);
    }

    /// Review fix-vfx2-determinism finding 4: `det_ln_f64` must cover the documented domain —
    /// infinities and subnormals included, not only finite normal inputs.
    #[test]
    fn det_ln_f64_and_powf_f64_edges() {
        assert_eq!(det_ln_f64(f64::INFINITY), f64::INFINITY);
        assert_eq!(det_ln_f64(0.0), f64::NEG_INFINITY);
        assert!(det_ln_f64(-1.0).is_nan());
        assert!(det_ln_f64(f64::NAN).is_nan());
        // Subnormals: the exponent split must rescale them first (they cost up to ~35 in `e`).
        for x in [
            f64::from_bits(1),
            f64::from_bits(0x0008_0000_0000_0000),
            f64::MIN_POSITIVE,
        ] {
            let got = det_ln_f64(x);
            let want = x.ln();
            assert!(
                (got - want).abs() <= 1e-14 * want.abs().max(1.0),
                "ln({x:e}): {got} vs {want}"
            );
        }
        assert_eq!(det_powf_f64(f64::INFINITY, 0.5), f64::INFINITY);
        assert_eq!(det_powf_f64(f64::INFINITY, -1.0), 0.0);
        assert_eq!(det_powf_f64(f64::INFINITY, 2.0), f64::INFINITY);
        assert!(det_powf_f64(f64::NAN, 1.0).is_nan());
    }

    #[test]
    fn pcg32_reference_vectors() {
        // Output of the reference pcg32 demo for pcg32_srandom(42, 54).
        let mut rng = Rng::with_stream(42, 54);
        let got: Vec<u32> = (0..6).map(|_| rng.next_u32()).collect();
        assert_eq!(
            got,
            [
                0xa15c02b7, 0x7b47f409, 0xba1d3330, 0x83d2f293, 0xbfa4784b, 0xcbed606e
            ]
        );
    }

    #[test]
    fn deterministic_and_stream_independent() {
        let seq = |mut r: Rng| (0..100).map(|_| r.next_u64()).collect::<Vec<_>>();
        assert_eq!(seq(Rng::new(7)), seq(Rng::new(7)));
        assert_ne!(seq(Rng::new(7)), seq(Rng::new(8)));
        assert_ne!(seq(Rng::with_stream(7, 1)), seq(Rng::with_stream(7, 2)));
        let mut a = Rng::new(3);
        let mut b = a.clone();
        assert_eq!(a.fork(), b.fork());
        assert_ne!(Rng::from_entropy(), Rng::from_entropy());
        let json = serde_json::to_string(&a).expect("serialize");
        let mut c: Rng = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(a.next_u32(), c.next_u32());
    }

    #[test]
    fn unit_floats_and_ranges() {
        let mut rng = Rng::new(1);
        for _ in 0..10_000 {
            let f = rng.f32();
            assert!((0.0..1.0).contains(&f));
            let d = rng.f64();
            assert!((0.0..1.0).contains(&d));
            let i = rng.range(-5..5);
            assert!((-5..5).contains(&i));
            let j = rng.range(-3i8..=3);
            assert!((-3..=3).contains(&j));
            let k = rng.range(10u64..=12);
            assert!((10..=12).contains(&k));
            let x = rng.range(-2.5f32..7.5);
            assert!((-2.5..7.5).contains(&x));
            let y = rng.range(1.0f64..=2.0);
            assert!((1.0..=2.0).contains(&y));
        }
        assert_eq!(rng.range(5..5), 5);
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 5..=4;
        assert_eq!(rng.range(reversed), 5);
        assert_eq!(rng.range(2.0f32..1.0), 2.0);
        // Full-width ranges do not overflow.
        let _ = rng.range(u32::MIN..=u32::MAX);
        let _ = rng.range(i64::MIN..=i64::MAX);
        let _ = rng.range(i8::MIN..i8::MAX);
    }

    #[test]
    fn integer_range_is_roughly_uniform() {
        let mut rng = Rng::new(99);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            counts[rng.range(0usize..6)] += 1;
        }
        for c in counts {
            assert!((9_000..11_000).contains(&c), "{counts:?}");
        }
        let mut signed = [0u32; 5];
        for _ in 0..50_000 {
            signed[(rng.range(-100i8..=-96) + 100) as usize] += 1;
        }
        for c in signed {
            assert!((9_000..11_000).contains(&c), "{signed:?}");
        }
        let heads = (0..10_000).filter(|_| rng.bool()).count();
        assert!((4_700..5_300).contains(&heads));
    }

    #[test]
    fn sphere_sampling() {
        let mut rng = Rng::new(5);
        let mut mean = Vec3::ZERO;
        for _ in 0..20_000 {
            let v = rng.unit_vector();
            assert!((v.length() - 1.0).abs() < 1e-5);
            mean += v;
            assert!(rng.in_sphere().length() <= 1.0 + 1e-6);
        }
        assert!((mean / 20_000.0).length() < 0.02, "centred: {mean}");
        // Half the volume of a unit ball lies outside radius 0.5^(1/3) ≈ 0.794.
        let outside = (0..20_000)
            .filter(|_| rng.in_sphere().length() > 0.5f32.cbrt())
            .count();
        assert!((9_500..10_500).contains(&outside), "{outside}");
    }

    #[test]
    fn deterministic_trig_matches_std_closely() {
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let (s, c) = det_sin_cos_turns(t);
            let a = f64::from(t) * core::f64::consts::TAU;
            assert!((f64::from(s) - a.sin()).abs() < 1e-6, "sin {t}");
            assert!((f64::from(c) - a.cos()).abs() < 1e-6, "cos {t}");
        }
        for i in 0..=1000 {
            let u = i as f32 / 1000.0;
            assert!((det_cbrt(u) - u.cbrt()).abs() < 1e-6, "cbrt {u}");
        }
        assert_eq!(det_cbrt(0.0), 0.0);
        assert!((det_cbrt(f32::from_bits(0x3380_0000)) - 2.0f32.powi(-8)).abs() < 1e-7);
    }

    #[test]
    fn unit_vector_is_pinned_to_a_fixed_value() {
        // Recomputes the draw by hand: any change to the sampling or the trig makes this fail.
        let mut a = Rng::new(5);
        let mut b = Rng::new(5);
        let z = b.range(-1.0f32..=1.0);
        let (s, c) = det_sin_cos_turns(b.f32());
        let r = (1.0 - z * z).max(0.0).sqrt();
        assert_eq!(a.unit_vector(), Vec3::new(r * c, r * s, z));
    }

    #[test]
    fn huge_float_span_still_varies() {
        let mut rng = Rng::new(3);
        let xs: Vec<f32> = (0..16).map(|_| rng.range(-2e38f32..2e38)).collect();
        assert!(xs.iter().all(|x| x.is_finite() && (-2e38..2e38).contains(x)));
        assert!(xs.windows(2).any(|w| w[0] != w[1]));
        let ys: Vec<f32> = (0..16).map(|_| rng.range(-2e38f32..=2e38)).collect();
        assert!(ys.iter().all(|x| x.is_finite() && (-2e38..=2e38).contains(x)));
        assert!(ys.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn zero_width_ranges_consume_one_draw_like_integers() {
        let mut a = Rng::new(9);
        let mut b = Rng::new(9);
        let mut c = Rng::new(9);
        assert_eq!(a.range(0.0f32..=0.0), 0.0);
        assert_eq!(b.range(0.0f32..0.0), 0.0);
        assert_eq!(c.range(0i32..=0), 0);
        let next = a.next_u32();
        assert_eq!(next, b.next_u32());
        assert_eq!(next, c.next_u32());
        assert_ne!(next, Rng::new(9).next_u32());
    }

    #[test]
    fn shuffle_and_choose() {
        let mut rng = Rng::new(11);
        let mut v: Vec<u32> = (0..50).collect();
        rng.shuffle(&mut v);
        assert_ne!(v, (0..50).collect::<Vec<_>>());
        v.sort_unstable();
        assert_eq!(v, (0..50).collect::<Vec<_>>());
        let empty: [u8; 0] = [];
        assert!(rng.choose(&empty).is_none());
        let items = ["a", "b", "c"];
        assert!(items.contains(rng.choose(&items).expect("non-empty")));
        rng.shuffle::<u8>(&mut []);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn power_domains_retain_their_distinct_negative_base_contracts() {
        assert_eq!(det_powf(-1.0, 2.0), 1.0);
        assert_eq!(det_powf(-2.0, 3.0), -8.0);
        assert!(det_powf_nonnegative(-1.0, 2.0).is_nan());
        assert!(det_powf_nonnegative(-2.0, 3.0).is_nan());
    }

    /// `e^x` just below the smallest normal must stay a non-negative subnormal (or zero).
    /// The exponent-bit scale used to wrap for `n < -1022` and return a huge negative.
    #[test]
    fn det_exp_f64_underflow_stays_non_negative() {
        for x in [-709.0_f64, -710.0, -720.0, -744.0] {
            let got = det_exp_f64(x);
            let want = x.exp();
            assert!(
                got.is_finite() && (0.0..1.0e-300).contains(&got),
                "det_exp_f64({x}) = {got}, libm {want}"
            );
            if want > 0.0 && got > 0.0 {
                let rel = ((got - want) / want).abs();
                assert!(rel < 1.0e-6, "det_exp_f64({x}) = {got}, libm {want}, rel {rel}");
            }
        }
        // Same scale, reached through a power: 1e-10^31 ≈ exp(-713.8).
        let got = det_powf_f64(1.0e-10, 31.0);
        let want = 1.0e-10_f64.powf(31.0);
        assert!(
            got.is_finite() && (0.0..1.0e-300).contains(&got),
            "det_powf_f64(1e-10, 31) = {got}, libm {want}"
        );
        assert_eq!(det_exp_f64(0.0), 1.0);
        assert!((det_exp_f64(-1.0) - (-1.0_f64).exp()).abs() < 1.0e-12);
    }

    /// Newton from `1.0` does not reach the root for tiny or huge positive inputs.
    #[test]
    fn det_cbrt_tracks_libm_at_extreme_magnitudes() {
        for exp in [-30, -20, 0, 10, 16, 20, 30] {
            let u = 10f32.powi(exp);
            let got = det_cbrt(u);
            let want = u.cbrt();
            let rel = (got - want).abs() / want;
            assert!(rel < 1.0e-5, "cbrt(1e{exp}) = {got}, libm {want}, rel {rel}");
        }
        let huge = f32::MAX;
        let rel = (det_cbrt(huge) - huge.cbrt()).abs() / huge.cbrt();
        assert!(rel < 1.0e-5, "cbrt(f32::MAX) rel {rel}");
        // The pinned power of two stays exact.
        assert_eq!(det_cbrt(f32::from_bits(0x3380_0000)).to_bits(), 0x3b80_0000);
    }

    /// A subnormal argument used to be decoded with a zero exponent field, so `ln` was off by
    /// tens of nats (about 1e12 in the original value).
    #[test]
    fn det_ln_f64_tracks_libm_on_subnormals() {
        for x in [1.0e-320_f64, 1.0e-310, 5.0e-324] {
            let got = det_ln_f64(x);
            let want = x.ln();
            let rel = ((got - want) / want.abs()).abs();
            assert!(rel < 1.0e-6, "det_ln_f64({x}) = {got}, libm {want}, rel {rel}");
        }
        assert!((det_ln_f64(2.0) - 2.0_f64.ln()).abs() < 1.0e-12);
    }

    use super::super::det_cos;
    use super::*;

    const DET_COS_0_7_BITS: u32 = 0x3f43_ccb3;
    const DET_COS_50_DEG_BITS: u32 = 0x3f24_8dbb;

    #[test]
    fn det_cos_tracks_cos_and_is_pinned_to_its_bits() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            assert!((det_cos(x) - x.cos()).abs() <= 2.0e-7, "{x}");
        }
        assert_eq!(det_cos(0.0), 1.0);
        // Series on plain `f64` `+`/`*`: these bits hold on every target, whatever its libm says.
        assert_eq!(det_cos(0.7).to_bits(), DET_COS_0_7_BITS);
        assert_eq!(det_cos(0.872_664_6).to_bits(), DET_COS_50_DEG_BITS);
        // 45°: the nav default slope limit compares against this exact value (FRAC_1_SQRT_2).
        assert_eq!(
            det_cos(std::f32::consts::FRAC_PI_4).to_bits(),
            std::f32::consts::FRAC_1_SQRT_2.to_bits()
        );
    }

    /// Angles where this host's glibc and the kernel round differently: the pins below would fail
    /// under platform libm, so they prove the nav paths use the kernel, not `f32::sin_cos`.
    const LIBM_SPLIT_SIN_ANGLE: u32 = 0x3d00_03de; // sin: kernel 0x3cfffd10, glibc 0x3cfffd11
    const LIBM_SPLIT_COS_ANGLE: u32 = 0x3d00_1e53; // cos: kernel 0x3f7fdff1, glibc 0x3f7fdff2
    const LIBM_SPLIT_ATAN2_Y: u32 = 0x3d8d_6b23; // atan2(y, 1): kernel 0x3d8d31c2, glibc 0x3d8d31c3

    #[test]
    fn det_sin_cos_tracks_libm_and_is_pinned_to_kernel_bits() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            let (sin, cos) = det_sin_cos(x);
            assert!((sin - x.sin()).abs() <= 2.0e-7, "sin {x}");
            assert!((cos - x.cos()).abs() <= 2.0e-7, "cos {x}");
        }
        assert_eq!(det_sin_cos(0.0), (0.0, 1.0));
        assert_eq!(det_sin(0.0), 0.0);
        let a = f32::from_bits(LIBM_SPLIT_SIN_ANGLE);
        assert_eq!(det_sin(a).to_bits(), 0x3cff_fd10);
        let b = f32::from_bits(LIBM_SPLIT_COS_ANGLE);
        assert_eq!(det_sin_cos(b).1.to_bits(), 0x3f7f_dff1);
    }

    /// The correctly rounded `atan2` reference (the `f64` libm rounded once to `f32`). The
    /// platform's `atan2f` is not one: macOS lands one ulp off glibc in places (for example
    /// `atan2(-7.855, -3.7)`: -2.0110068 there, -2.0110066 correctly rounded), and a 2e-7 bound is
    /// under one ulp in `[2, PI)` (ADR-5550).
    fn reference_atan2(y: f32, x: f32) -> f32 {
        f64::from(y).atan2(f64::from(x)) as f32
    }

    #[test]
    fn det_atan2_tracks_libm_and_is_pinned_to_kernel_bits() {
        for i in -2000..=2000 {
            let y = i as f32 * 0.005;
            for x in [-3.7f32, -0.2, 0.31, 1.0, 5.0] {
                let got = det_atan2(y, x);
                let want = reference_atan2(y, x);
                let err = (got as f64 - want as f64).abs();
                assert!(err <= 2.0e-7, "atan2({y}, {x}): {got} vs {want}");
            }
        }
        // IEEE special cases, identical on every libm.
        assert_eq!(det_atan2(0.0, 1.0), 0.0);
        assert_eq!(det_atan2(0.0, -1.0), std::f32::consts::PI);
        assert_eq!(det_atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2);
        assert_eq!(det_atan2(-1.0, 0.0), -std::f32::consts::FRAC_PI_2);
        assert!(det_atan2(f32::NAN, 1.0).is_nan());
        // The pinned kernel bits (differ from this host's glibc by one ulp).
        let y = f32::from_bits(LIBM_SPLIT_ATAN2_Y);
        assert_eq!(det_atan2(y, 1.0).to_bits(), 0x3d8d_31c2);
    }

    /// Infinite inputs must follow `f32::atan2` quadrant by quadrant: the kernel computes in
    /// `f64` after `f32 → f64` widening, so `∞/∞ = NaN` reached the series pre-fix and the
    /// both-infinite quadrants (π/4, 3π/4) came out NaN instead. The reference is the correctly
    /// rounded value: macOS's `atan2f` returns ±π rounded down (0x40490FDA) for an infinite `x`
    /// (ADR-5860), glibc and the det kernel round to nearest (0x40490FDB).
    #[test]
    fn det_atan2_infinite_quadrants_match_f32_atan2() {
        let ys = [f32::INFINITY, f32::NEG_INFINITY];
        let xs = [f32::INFINITY, f32::NEG_INFINITY];
        for y in ys {
            for x in xs {
                assert_eq!(
                    det_atan2(y, x).to_bits(),
                    reference_atan2(y, x).to_bits(),
                    "atan2({y}, {x})"
                );
            }
        }
        // y infinite, x finite: ±π/2; x infinite, y finite: ±0 or ±π.
        for y in ys {
            for x in [-3.7f32, -0.0, 0.0, 0.31] {
                assert_eq!(
                    det_atan2(y, x).to_bits(),
                    reference_atan2(y, x).to_bits(),
                    "atan2({y}, {x})"
                );
            }
        }
        for x in xs {
            for y in [-3.7f32, -0.0, 0.0, 0.31] {
                assert_eq!(
                    det_atan2(y, x).to_bits(),
                    reference_atan2(y, x).to_bits(),
                    "atan2({y}, {x})"
                );
            }
        }
    }

    #[test]
    fn det_sin_cos_is_det_cos_and_matches_libm() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            let (sin, cos) = det_sin_cos(x);
            assert!((sin - x.sin()).abs() <= 2.0e-7, "sin {x}");
            assert!((cos - x.cos()).abs() <= 2.0e-7, "cos {x}");
            assert_eq!(cos, det_cos(x));
        }
    }

    #[test]
    fn det_atan2_tracks_atan2_and_covers_the_circle() {
        // Across the circle (coarse grid; the axes and exact points are pinned below).
        for i in -400..=400 {
            let y = i as f32 * 0.031;
            for j in -400..=400 {
                if j == 0 {
                    continue; // the axes are pinned below
                }
                let x = j as f32 * 0.037;
                assert!(
                    (det_atan2(y, x) - reference_atan2(y, x)).abs() <= 2.0e-7,
                    "atan2({y}, {x})"
                );
            }
        }
        assert_eq!(det_atan2(0.0, 1.0), 0.0);
        assert_eq!(det_atan2(0.0, -1.0), std::f32::consts::PI);
        assert_eq!(det_atan2(-0.0, -1.0), -std::f32::consts::PI);
        assert_eq!(det_atan2(0.0, 0.0), 0.0);
        // Signed zeros follow IEEE 754: `atan2f(-0, +0)` is `-0`, not `-π/2` (the pre-fix
        // early `x == 0` branch ignored the sign of `x`).
        assert_eq!(det_atan2(-0.0, 0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(det_atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2);
        assert_eq!(det_atan2(-1.0, 0.0), -std::f32::consts::FRAC_PI_2);
        assert_eq!(det_atan2(1.0, 1.0), std::f32::consts::FRAC_PI_4);
        assert_eq!(det_atan2(-1.0, -1.0), -3.0 * std::f32::consts::FRAC_PI_4);
    }

    /// IEEE-754 special cases (audit kimirev-fix-nav-determinism, finding 3): both signs of zero
    /// and all four infinity pairs must match the correctly rounded `atan2` bit for bit, as glibc's
    /// does (macOS's `atan2f(0, -0)` is π rounded down, ADR-5860).
    #[test]
    fn det_atan2_matches_atan2_on_signed_zero_and_infinity_pairs() {
        for (y, x) in [
            (0.0f32, 0.0),
            (-0.0, 0.0),
            (0.0, -0.0),
            (-0.0, -0.0),
            (f32::INFINITY, f32::INFINITY),
            (f32::INFINITY, f32::NEG_INFINITY),
            (f32::NEG_INFINITY, f32::INFINITY),
            (f32::NEG_INFINITY, f32::NEG_INFINITY),
        ] {
            assert_eq!(
                det_atan2(y, x).to_bits(),
                reference_atan2(y, x).to_bits(),
                "atan2({y}, {x})"
            );
        }
    }

    #[test]
    fn det_sin_tracks_sin_and_is_pinned_to_its_bits() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            assert!((det_sin(x) - x.sin()).abs() <= 2.0e-7, "{x}");
        }
        assert_eq!(det_sin(0.0), 0.0);
        // Same kernel as `det_cos`, shifted: the pins hold on every target.
        assert_eq!(det_sin(0.7).to_bits(), 0x3f24_eb73);
        assert_eq!(det_sin(0.872_664_6).to_bits(), 0x3f44_1b7c);
    }

    #[test]
    fn det_atan2_tracks_atan2_over_the_circle_and_pins_its_bits() {
        // A sweep of directions: every quadrant, both signs, ratios above and below 1.
        for i in -720..=720 {
            let a = i as f32 * 0.025;
            let (y, x) = (a.sin(), a.cos());
            let det = det_atan2(y, x);
            let libm = y.atan2(x);
            let err = (det - libm).abs();
            assert!(err <= 3.0e-7, "a={a} det={det} libm={libm}");
        }
        assert_eq!(det_atan2(0.0, 0.0), 0.0, "matches f32::atan2(0, 0)");
        assert_eq!(det_atan2(0.0, -1.0), core::f32::consts::PI);
        assert_eq!(det_atan2(1.0, 0.0), core::f32::consts::FRAC_PI_2);
        // atan2(-0.8, 0.6): the nav-area zone yaw a cook and a runtime must agree on.
        assert_eq!(det_atan2(-0.8, 0.6).to_bits(), 0xbf6d_6338);
        assert_eq!(det_atan2(0.8, 0.6).to_bits(), 0x3f6d_6338);
    }

    #[test]
    fn det_yaw_quat_round_trips_and_tracks_glam() {
        // A yaw quaternion encodes its angle modulo 2π: sweep the representable range.
        for i in -314..=314 {
            let yaw = i as f32 * 0.01;
            let q = det_yaw_quat(yaw);
            // The same value `Quat::from_rotation_y` builds, up to the libm's last digit.
            let glam_q = Quat::from_rotation_y(yaw);
            assert!(q.abs_diff_eq(glam_q, 1.0e-6), "yaw={yaw} q={q} glam={glam_q}");
            let back = det_yaw_of(q);
            assert!((back - yaw).abs() <= 2.0e-6, "yaw={yaw} back={back}");
        }
        // A pure yaw quaternion keeps its angle through the extraction (bits pinned below).
        let q = det_yaw_quat(0.7);
        assert_eq!(
            q.to_array().map(f32::to_bits),
            [0x0000_0000, 0x3eaf_904d, 0x0000_0000, 0x3f70_7abb]
        );
    }

    #[test]
    fn det_yaw_of_matches_glam_yxz_yaw_away_from_gimbal() {
        for i in -200..=200 {
            let yaw = i as f32 * 0.02;
            for (pitch, roll) in [(0.1f32, 0.2f32), (-0.3, 0.4), (0.5, -0.25)] {
                let q = Quat::from_euler(glam::EulerRot::YXZ, yaw, pitch, roll);
                let (glam_yaw, _, _) = q.to_euler(glam::EulerRot::YXZ);
                assert!(
                    (det_yaw_of(q) - glam_yaw).abs() <= 3.0e-6,
                    "yaw={yaw} pitch={pitch} roll={roll}"
                );
            }
        }
    }

    #[test]
    fn det_sin_tracks_sin_and_is_pinned_to_its_bits_framework() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            assert!((det_sin(x) - x.sin()).abs() <= 2.0e-7, "{x}");
        }
        assert_eq!(det_sin(0.0), 0.0);
        // Same kernel as `det_cos`: these bits hold on every target, whatever its libm says.
        assert_eq!(det_sin(0.7).to_bits(), 0x3f24_eb73);
        assert_eq!(det_sin(0.872_664_6).to_bits(), 0x3f44_1b7c);
    }

    #[test]
    fn det_atan2_tracks_atan2_and_is_pinned_to_its_bits() {
        // A grid over all four quadrants, the axes and the octant boundaries.
        for iy in -100..=100 {
            for ix in -100..=100 {
                let y = iy as f32 * 0.021;
                let x = ix as f32 * 0.017;
                if x == 0.0 && y == 0.0 {
                    continue;
                }
                assert!((det_atan2(y, x) - y.atan2(x)).abs() <= 2.0e-6, "atan2({y}, {x})");
            }
        }
        assert_eq!(det_atan2(0.0, 1.0), 0.0);
        assert_eq!(det_atan2(0.0, -1.0), core::f32::consts::PI);
        assert_eq!(det_atan2(1.0, 0.0), core::f32::consts::FRAC_PI_2);
        // Deterministic reduction + Taylor on plain `f64`: these bits hold on every target.
        assert_eq!(det_atan2(0.7, 0.3).to_bits(), 0x3f95_3c5c);
        assert_eq!(det_atan2(-0.4, 2.0).to_bits(), 0xbe4a_2210);
        assert_eq!(det_atan2(1.0, -1.0).to_bits(), 0x4016_cbe4);
    }

    #[test]
    fn det_acos_approx_tracks_acos_within_its_minimax_bound() {
        // Measured worst error is about 5e-7 rad; 1e-5 leaves headroom on every target.
        for i in -1000..=1000 {
            let x = i as f32 * 0.001;
            assert!((det_acos_approx(x) - x.acos()).abs() <= 1.0e-5, "{x}");
        }
        // The 7-degree minimax polynomial is plain f32 arithmetic and sqrt: these bits hold on
        // every target (same construction as glam's `acos_approx`, see docs/provenance.md).
        assert_eq!(det_acos_approx(0.5).to_bits(), 0x3f86_0a91);
        assert_eq!(det_acos_approx(-0.5).to_bits(), 0x4006_0a92);
    }

    #[test]
    fn det_sin_tracks_sin_and_is_pinned_to_its_bits_physics() {
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            assert!((det_sin(x) - x.sin()).abs() <= 2.0e-7, "{x}");
        }
        assert_eq!(det_sin(0.0), 0.0);
        assert_eq!(det_sin(0.7).to_bits(), 0x3f24_eb73);
        assert_eq!(det_sin(-1.25).to_bits(), 0xbf72_f0a8);
    }

    #[test]
    fn det_atan2_tracks_atan2_and_is_pinned_to_its_bits_physics() {
        // A sweep over all quadrants and both argument orders, including the swap boundary.
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            for &y in &[0.3f32, -0.7, 1.1] {
                assert!(
                    (det_atan2(y, x) - reference_atan2(y, x)).abs() <= 2.0e-7,
                    "atan2({y}, {x})"
                );
                assert!(
                    (det_atan2(x, y) - reference_atan2(x, y)).abs() <= 2.0e-7,
                    "atan2({x}, {y})"
                );
            }
        }
        assert_eq!(det_atan2(0.0, 1.0), 0.0);
        assert_eq!(det_atan2(0.0, -1.0), core::f32::consts::PI);
        assert_eq!(det_atan2(1.0, 0.0), core::f32::consts::FRAC_PI_2);
        assert_eq!(det_atan2(-1.0, 0.0), -core::f32::consts::FRAC_PI_2);
        // Argument-reduced `f64` arithmetic: these bits hold on every target, whatever its libm.
        assert_eq!(det_atan2(0.5, 1.0).to_bits(), 0x3eed_6338);
        assert_eq!(det_atan2(1.0, -0.4).to_bits(), 0x3ff9_c44a);
        assert_eq!(det_atan2(-0.7, 0.7).to_bits(), 0xbf49_0fdb);
    }

    /// The signed-zero and NaN corners must match the platform `atan2f` and IEEE 754 exactly —
    /// including both signs of zero, which the pre-fix `x == 0` early branch ignored (it returned
    /// `-π/2` for `atan2(-0, +0)` and `0` for `atan2(+0, -0)`, and swallowed NaN to `0`).
    /// IEEE 754 defines these corner values, so comparing against the correctly rounded value
    /// here pins behavior, not this host's kernel bits. (macOS's `atan2f(±0, -0)` returns ±π
    /// rounded down, one ulp under the correctly rounded π, ADR-5860.)
    #[test]
    fn det_atan2_matches_libm_on_signed_zeros_and_propagates_nan() {
        let pos_zero = 0.0f32;
        let neg_zero = -0.0f32;
        for (y, x) in [
            (pos_zero, pos_zero),
            (neg_zero, pos_zero),
            (pos_zero, neg_zero),
            (neg_zero, neg_zero),
        ] {
            let got = det_atan2(y, x);
            let want = reference_atan2(y, x);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "atan2({y:e}, {x:e}): {got:e} vs libm {want:e}"
            );
        }
        // Non-zero `y` against a zero `x` keeps the signed right angle.
        assert_eq!(det_atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2);
        assert_eq!(det_atan2(-1.0, 0.0), -std::f32::consts::FRAC_PI_2);
        // NaN in either argument propagates (the pre-fix code returned 0 for NaN in `y`).
        assert!(det_atan2(f32::NAN, 0.0).is_nan());
        assert!(det_atan2(0.0, f32::NAN).is_nan());
        assert!(det_atan2(f32::NAN, f32::NAN).is_nan());
        assert!(det_atan2(f32::INFINITY, f32::NAN).is_nan());
    }

    #[test]
    fn pcg32_reference_vectors() {
        // Output of the reference pcg32 demo for pcg32_srandom(42, 54).
        let mut rng = Rng::with_stream(42, 54);
        let got: Vec<u32> = (0..6).map(|_| rng.next_u32()).collect();
        assert_eq!(
            got,
            [
                0xa15c02b7, 0x7b47f409, 0xba1d3330, 0x83d2f293, 0xbfa4784b, 0xcbed606e
            ]
        );
    }

    #[test]
    fn deterministic_and_stream_independent() {
        let seq = |mut r: Rng| (0..100).map(|_| r.next_u64()).collect::<Vec<_>>();
        assert_eq!(seq(Rng::new(7)), seq(Rng::new(7)));
        assert_ne!(seq(Rng::new(7)), seq(Rng::new(8)));
        assert_ne!(seq(Rng::with_stream(7, 1)), seq(Rng::with_stream(7, 2)));
        let mut a = Rng::new(3);
        let mut b = a.clone();
        assert_eq!(a.fork(), b.fork());
        assert_ne!(Rng::from_entropy(), Rng::from_entropy());
        let json = serde_json::to_string(&a).expect("serialize");
        let mut c: Rng = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(a.next_u32(), c.next_u32());
    }

    #[test]
    fn unit_floats_and_ranges() {
        let mut rng = Rng::new(1);
        for _ in 0..10_000 {
            let f = rng.f32();
            assert!((0.0..1.0).contains(&f));
            let d = rng.f64();
            assert!((0.0..1.0).contains(&d));
            let i = rng.range(-5..5);
            assert!((-5..5).contains(&i));
            let j = rng.range(-3i8..=3);
            assert!((-3..=3).contains(&j));
            let k = rng.range(10u64..=12);
            assert!((10..=12).contains(&k));
            let x = rng.range(-2.5f32..7.5);
            assert!((-2.5..7.5).contains(&x));
            let y = rng.range(1.0f64..=2.0);
            assert!((1.0..=2.0).contains(&y));
        }
        assert_eq!(rng.range(5..5), 5);
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 5..=4;
        assert_eq!(rng.range(reversed), 5);
        assert_eq!(rng.range(2.0f32..1.0), 2.0);
        // Full-width ranges do not overflow.
        let _ = rng.range(u32::MIN..=u32::MAX);
        let _ = rng.range(i64::MIN..=i64::MAX);
        let _ = rng.range(i8::MIN..i8::MAX);
    }

    #[test]
    fn integer_range_is_roughly_uniform() {
        let mut rng = Rng::new(99);
        let mut counts = [0u32; 6];
        for _ in 0..60_000 {
            counts[rng.range(0usize..6)] += 1;
        }
        for c in counts {
            assert!((9_000..11_000).contains(&c), "{counts:?}");
        }
        let mut signed = [0u32; 5];
        for _ in 0..50_000 {
            signed[(rng.range(-100i8..=-96) + 100) as usize] += 1;
        }
        for c in signed {
            assert!((9_000..11_000).contains(&c), "{signed:?}");
        }
        let heads = (0..10_000).filter(|_| rng.bool()).count();
        assert!((4_700..5_300).contains(&heads));
    }

    #[test]
    fn sphere_sampling() {
        let mut rng = Rng::new(5);
        let mut mean = Vec3::ZERO;
        for _ in 0..20_000 {
            let v = rng.unit_vector();
            assert!((v.length() - 1.0).abs() < 1e-5);
            mean += v;
            assert!(rng.in_sphere().length() <= 1.0 + 1e-6);
        }
        assert!((mean / 20_000.0).length() < 0.02, "centred: {mean}");
        // Half the volume of a unit ball lies outside radius 0.5^(1/3) ≈ 0.794.
        let outside = (0..20_000)
            .filter(|_| rng.in_sphere().length() > 0.5f32.cbrt())
            .count();
        assert!((9_500..10_500).contains(&outside), "{outside}");
    }

    #[test]
    fn deterministic_trig_matches_std_closely() {
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let (s, c) = det_sin_cos_turns(t);
            let a = f64::from(t) * core::f64::consts::TAU;
            assert!((f64::from(s) - a.sin()).abs() < 1e-6, "sin {t}");
            assert!((f64::from(c) - a.cos()).abs() < 1e-6, "cos {t}");
        }
        for i in 0..=1000 {
            let u = i as f32 / 1000.0;
            assert!((det_cbrt(u) - u.cbrt()).abs() < 1e-6, "cbrt {u}");
        }
        assert_eq!(det_cbrt(0.0), 0.0);
        assert!((det_cbrt(f32::from_bits(0x3380_0000)) - 2.0f32.powi(-8)).abs() < 1e-7);
    }

    #[test]
    fn unit_vector_is_pinned_to_a_fixed_value() {
        // Recomputes the draw by hand: any change to the sampling or the trig makes this fail.
        let mut a = Rng::new(5);
        let mut b = Rng::new(5);
        let z = b.range(-1.0f32..=1.0);
        let (s, c) = det_sin_cos_turns(b.f32());
        let r = (1.0 - z * z).max(0.0).sqrt();
        assert_eq!(a.unit_vector(), Vec3::new(r * c, r * s, z));
    }

    #[test]
    fn huge_float_span_still_varies() {
        let mut rng = Rng::new(3);
        let xs: Vec<f32> = (0..16).map(|_| rng.range(-2e38f32..2e38)).collect();
        assert!(xs.iter().all(|x| x.is_finite() && (-2e38..2e38).contains(x)));
        assert!(xs.windows(2).any(|w| w[0] != w[1]));
        let ys: Vec<f32> = (0..16).map(|_| rng.range(-2e38f32..=2e38)).collect();
        assert!(ys.iter().all(|x| x.is_finite() && (-2e38..=2e38).contains(x)));
        assert!(ys.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn zero_width_ranges_consume_one_draw_like_integers() {
        let mut a = Rng::new(9);
        let mut b = Rng::new(9);
        let mut c = Rng::new(9);
        assert_eq!(a.range(0.0f32..=0.0), 0.0);
        assert_eq!(b.range(0.0f32..0.0), 0.0);
        assert_eq!(c.range(0i32..=0), 0);
        let next = a.next_u32();
        assert_eq!(next, b.next_u32());
        assert_eq!(next, c.next_u32());
        assert_ne!(next, Rng::new(9).next_u32());
    }

    #[test]
    fn shuffle_and_choose() {
        let mut rng = Rng::new(11);
        let mut v: Vec<u32> = (0..50).collect();
        rng.shuffle(&mut v);
        assert_ne!(v, (0..50).collect::<Vec<_>>());
        v.sort_unstable();
        assert_eq!(v, (0..50).collect::<Vec<_>>());
        let empty: [u8; 0] = [];
        assert!(rng.choose(&empty).is_none());
        let items = ["a", "b", "c"];
        assert!(items.contains(rng.choose(&items).expect("non-empty")));
        rng.shuffle::<u8>(&mut []);
    }
}
