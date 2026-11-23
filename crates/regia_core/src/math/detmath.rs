// SPDX-License-Identifier: MPL-2.0
// Reference algorithms: fdlibm (freely distributable, Netlib; range-reduction and series
// structure re-derived from the public algorithm descriptions, original code below) and the
// existing `det_cos` kernel in `rng.rs` (same Taylor-on-`f64` idea, ADR-4773, spec 15 § 5).

//! Platform-independent elementary functions (ADR-4773).
//!
//! Every function here returns the same bits on every target (Linux, Windows, Android), built
//! only from plain IEEE `f64` `+`, `-`, `*`, `/`, `sqrt` and comparisons — never from the
//! platform libm, whose `sin`/`cos`/`atan2`/`acos`/`powf`/`cbrt` may differ in the last ulp
//! between glibc, UCRT and bionic. Physics that must agree across a lockstep session, or between
//! a cook host and a runtime host, routes its trigonometry through these functions.
//!
//! Accuracy: results track the correctly-rounded `f64` libm value to a few ulps (documented per
//! function), far inside `f32` rounding — what matters is that the bits are *stable across
//! targets*, not that they are correctly rounded on one target.

use super::{Quat, Vec3};

/// π/2 as an `f64` literal.
const FRAC_PI_2: f64 = core::f64::consts::FRAC_PI_2;
/// `e^(1/2)`, pinned beyond `f64` rounding.
const SQRT_E: f64 = 1.648_721_270_700_128_2;
/// `atan(1/4)`, pinned beyond `f64` rounding.
const ATAN_QUARTER: f64 = 0.244_978_663_126_864_14;

/// `(sin, cos)` of `radians`, from a reduce-then-Taylor kernel in `f64` (the same construction
/// as [`det_cos`]). The truncation error is below 1e-15 and the reduction
/// argument is scaled in `f64`, so the `f32` result is bit-identical on every target. Accuracy
/// tracks libm to a few ulps for `|radians|` up to about 2^24; for larger arguments the `f64`
/// range reduction can no longer place the result to `f32` precision, so the output stays
/// deterministic across targets but is not correctly rounded (no current caller passes angles
/// anywhere near that).
#[must_use]
pub fn det_sin_cos(radians: f32) -> (f32, f32) {
    let (sin, cos) = sin_cos_f64(f64::from(radians));
    (sin as f32, cos as f32)
}

/// `sin(radians)`: the same bits on every target. See [`det_sin_cos`].
#[must_use]
pub fn det_sin(radians: f32) -> f32 {
    det_sin_cos(radians).0
}

/// Platform-independent cosine, using the same kernel and accuracy range as [`det_sin_cos`].
#[inline]
pub fn det_cos(radians: f32) -> f32 {
    det_sin_cos(radians).1
}

/// Kernel of [`det_sin_cos`] on `f64`: quadrant-reduced, then an 8-term Taylor series on
/// `[-π/4, π/4]` (truncation below 1e-15). Plain `+`/`*` only: Rust never fuses them across
/// statements, and `f64` mul/add/div are correctly rounded on every target.
fn sin_cos_f64(radians: f64) -> (f64, f64) {
    let k = (radians / FRAC_PI_2 + 0.5).floor();
    let x = (radians / FRAC_PI_2 - k) * FRAC_PI_2;
    let x2 = x * x;
    let mut sin = 0.0;
    let mut cos = 0.0;
    for n in (0..8).rev() {
        let m = f64::from(2 * n);
        cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
        sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
    }
    let sin = x * sin;
    // `k` is integral; reduce modulo 4 in `f64` so huge arguments cannot overflow the cast.
    let quad = k - (k / 4.0).floor() * 4.0;
    match quad as i64 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// `atan(x)` on `f64` for `x >= 0`: reciprocal-reduce to `[0, 1]` (`atan(x) = π/2 − atan(1/x)`,
/// subtracted), then apply the addition formula `atan(v) = atan(c) + atan((v − c)/(1 + c·v))`
/// with `c = 1/4` until the argument is at most `1/4` (at most three applications from `v = 1`),
/// and a 15-term Taylor series there (truncation below 2e-19). Absolute error a few `f64` ulps.
fn atan_nonneg(x: f64) -> f64 {
    let mut v = x;
    let mut acc = 0.0;
    let flip = v > 1.0;
    if flip {
        v = 1.0 / v;
    }
    for _ in 0..4 {
        if v <= 0.25 {
            break;
        }
        v = (v - 0.25) / (1.0 + 0.25 * v);
        acc += ATAN_QUARTER;
    }
    let v2 = v * v;
    // Horner form of `1 − v²/3 + v⁴/5 − ...` (15 terms).
    let mut p = 1.0 / 29.0;
    for n in (0..14).rev() {
        p = 1.0 / f64::from(2 * n + 1) - v2 * p;
    }
    let inner = acc + v * p;
    if flip { FRAC_PI_2 - inner } else { inner }
}

/// `atan(x)` on `f64`: the odd extension of `atan_nonneg`.
fn atan_f64(x: f64) -> f64 {
    if x.is_sign_negative() {
        -atan_nonneg(-x)
    } else {
        atan_nonneg(x)
    }
}

/// `atan2(y, x)` on `f64`: quadrant-correct; both signs of zero, the infinity pairs and a single
/// infinite argument follow the libm convention (`atan2(±0, −0) = ±π`, `atan2(±∞, ±∞) = ±π/4` or
/// `±3π/4`, `atan2(±∞, finite) = ±π/2`, `atan2(finite, ±∞) = ±0` or `±π`).
fn atan2_f64(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    // Both infinite: the division below would be NaN; the angle is ±π/4 or ±3π/4 by quadrant.
    if x.is_infinite() && y.is_infinite() {
        return match (y.is_sign_negative(), x.is_sign_negative()) {
            (true, true) => -3.0 * core::f64::consts::FRAC_PI_4,
            (false, true) => 3.0 * core::f64::consts::FRAC_PI_4,
            (true, false) => -core::f64::consts::FRAC_PI_4,
            (false, false) => core::f64::consts::FRAC_PI_4,
        };
    }
    // One infinite argument (fixrev-16-k): the quotient below would be ±inf or a signed zero
    // the kernel must not see; atan2(±inf, finite) = ±π/2; atan2(finite y, ±inf) = ±0 or ±π by
    // x's sign (y's zero sign for y = ±0), as libm gives.
    if y.is_infinite() {
        return if y.is_sign_negative() {
            -FRAC_PI_2
        } else {
            FRAC_PI_2
        };
    }
    if x.is_infinite() {
        return if x.is_sign_negative() {
            if y.is_sign_negative() {
                -core::f64::consts::PI
            } else {
                core::f64::consts::PI
            }
        } else if y.is_sign_negative() {
            -0.0
        } else {
            0.0
        };
    }
    if x > 0.0 {
        atan_f64(y / x)
    } else if x < 0.0 {
        // `is_sign_negative`, not `>= 0.0`: `-0.0 >= 0.0` is true, but `atan2(-0, x < 0)` is
        // `-π`, matching the platform `atan2f` and IEEE 754.
        if y.is_sign_negative() {
            atan_f64(y / x) - core::f64::consts::PI
        } else {
            atan_f64(y / x) + core::f64::consts::PI
        }
    } else if y > 0.0 {
        FRAC_PI_2
    } else if y < 0.0 {
        -FRAC_PI_2
    } else {
        match (y.is_sign_negative(), x.is_sign_negative()) {
            (true, true) => -core::f64::consts::PI,
            (_, true) => core::f64::consts::PI,
            (true, false) => -0.0,
            (false, false) => 0.0,
        }
    }
}

/// `atan2(y, x)` on `f64` inputs: quadrant-correct; both signs of zero follow the libm
/// convention. Crate-internal: `f32` callers use [`det_atan2`], this is for `f64` pipelines
/// (e.g. mesh solid angles) that must be bit-stable across platform libms.
pub(crate) fn det_atan2_f64(y: f64, x: f64) -> f64 {
    atan2_f64(y, x)
}

/// `atan2(y, x)` for `f32` inputs: the same bits on every target. A few `f64` ulps from the
/// correctly-rounded value, far inside `f32` rounding.
#[must_use]
pub fn det_atan2(y: f32, x: f32) -> f32 {
    atan2_f64(f64::from(y), f64::from(x)) as f32
}

/// `atan(x)` for an `f32`: the same bits on every target. See [`det_atan2`].
#[must_use]
pub fn det_atan(x: f32) -> f32 {
    atan_f64(f64::from(x)) as f32
}

/// `acos(x)` for `x` in `[−1, 1]`: `atan2(√(1−x²), x)` with the `f64` square root (correctly
/// rounded on every target), so the same bits everywhere. NaN for `|x| > 1`.
#[must_use]
pub fn det_acos(x: f32) -> f32 {
    let x = f64::from(x);
    if x.abs() > 1.0 {
        return f32::NAN;
    }
    atan2_f64((1.0 - x * x).sqrt(), x) as f32
}

/// `log2(x)` on `f64` for positive finite `x`: decompose `x = m·2^e` with `m` in `[√½, √2)`,
/// then `ln(m) = 2·atanh(z)` with `z = (m−1)/(m+1)` (`|z|` ≤ 0.172) as a 12-term series, plus
/// `e·ln(2)`. A few `f64` ulps. (The `f64` square root and division are correctly rounded on
/// every target.)
fn log2_f64(x: f64) -> f64 {
    debug_assert!(x > 0.0);
    if !x.is_finite() {
        return x;
    }
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mantissa = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1023u64 << 52));
    let (m, e) = if mantissa > core::f64::consts::SQRT_2 {
        (mantissa * 0.5, e + 1)
    } else {
        (mantissa, e)
    };
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    // `2·(z + z³/3 + … + z²³/23)`: the dropped term is below 1e-18.
    let mut s = 1.0 / 23.0;
    for n in (0..11).rev() {
        s = s * z2 + 1.0 / f64::from(2 * n + 1);
    }
    (e as f64 * core::f64::consts::LN_2 + 2.0 * z * s) / core::f64::consts::LN_2
}

/// `2^y` on `f64`: `y·ln(2) = k + g` with `g` in `[0, 1)`, `e^g` by a 16-term Taylor series
/// around `1/2` (`|g − ½|` ≤ ½, truncation below 1e-17), scaled by the exact power of two
/// `2^k` built from its bits. A few `f64` ulps.
fn exp2_f64(y: f64) -> f64 {
    if y.is_nan() {
        return f64::NAN;
    }
    if y >= 1024.0 {
        return f64::INFINITY;
    }
    if y <= -1075.0 {
        return 0.0;
    }
    let k = y.floor();
    let g = (y - k) * core::f64::consts::LN_2;
    let u = g - 0.5;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..16 {
        term *= u / f64::from(n);
        sum += term;
    }
    let scaled = SQRT_E * sum;
    let k = k as i64;
    if k >= -1022 {
        scaled * f64::from_bits(((k + 1023) as u64) << 52)
    } else {
        // Deep subnormal power of two: 2^k = 2^(k+1022) · 2^-1022 with both factors exact.
        scaled * f64::from_bits(((k + 2045) as u64) << 52) * f64::from_bits(1u64 << 52)
    }
}

/// `base^exp` for `f32`: `2^(exp·log2(base))` through the kernels above, so the same bits on
/// every target. Matches `powf` to a few `f32` ulps. `1` for `exp == 0`; `0`/`∞` for a zero
/// base with positive/negative exponent; NaN for a negative or non-finite base or a non-finite
/// exponent. The audited call sites (wind falloff, tire load factor) always pass a finite
/// positive base.
#[must_use]
pub fn det_powf(base: f32, exp: f32) -> f32 {
    if exp == 0.0 {
        return 1.0;
    }
    if base.is_nan() || exp.is_nan() {
        return f32::NAN;
    }
    if base == 0.0 {
        return if exp > 0.0 { 0.0 } else { f32::INFINITY };
    }
    if base < 0.0 || !base.is_finite() || !exp.is_finite() {
        return f32::NAN;
    }
    exp2_f64(f64::from(exp) * log2_f64(f64::from(base))) as f32
}

/// Cube root of `x`, Newton-iterated in `f64` from a fixed exponent-third bit-hack seed: the
/// same bits on every target (no platform `cbrtf`). Tracks `cbrt` to the last `f32` ulp.
/// `0`, `±∞` and NaN pass through; negative inputs give the negative root.
#[must_use]
pub fn det_cbrt(x: f32) -> f32 {
    if x.is_nan() || x == 0.0 || !x.is_finite() {
        return x;
    }
    let v = f64::from(x.abs());
    // Seed: one third of the bit pattern plus the constant that re-biases the exponent
    // (`(e+1023)/3 ≈ e/3 + 341`; the constant adds ≈ 682 = 2·341 to the exponent field), so the
    // seed lands within a small factor of the true root for the whole `f32` range. Eight Newton
    // steps `y ← (2y + v/y²)/3` then converge to `f64` precision from any positive seed.
    let seed = f64::from_bits((v.to_bits() / 3).wrapping_add(0x2a9f_a4c5_0000_0000));
    let mut y = seed;
    for _ in 0..8 {
        y = (2.0 * y + v / (y * y)) / 3.0;
    }
    let result = y as f32;
    if x.is_sign_negative() { -result } else { result }
}

/// Quaternion for a rotation of `angle` radians about `axis` (normalized here), with
/// [`det_sin_cos`]: the same bits on every target, unlike `Quat::from_axis_angle` (platform
/// `sin`/`cos`). A zero or non-finite axis is the identity.
#[must_use]
pub fn det_quat_from_axis_angle(axis: Vec3, angle: f32) -> Quat {
    let len = axis.length();
    if len <= 1e-12 || !len.is_finite() {
        return Quat::IDENTITY;
    }
    let half = angle * 0.5;
    let (sin, cos) = det_sin_cos(half);
    let s = sin / len;
    Quat::from_xyzw(axis.x * s, axis.y * s, axis.z * s, cos)
}

/// Quaternion for a rotation vector (unit axis times radians): the deterministic twin of
/// `Quat::from_scaled_axis`, the same bits on every target. Small angles use the fixed
/// polynomials `sin(a/2)/a = 1/2 − a²/48 + a⁴/3840` and `cos(a/2) = 1 − a²/8 + a⁴/384` (exact
/// rationals, deterministic); larger angles go through [`det_sin_cos`].
#[must_use]
pub fn det_quat_from_scaled_axis(v: Vec3) -> Quat {
    let angle_sq = v.length_squared();
    if angle_sq < 1e-8 {
        let s = 0.5 - angle_sq / 48.0 + angle_sq * angle_sq / 3840.0;
        let w = 1.0 - angle_sq / 8.0 + angle_sq * angle_sq / 384.0;
        return Quat::from_xyzw(v.x * s, v.y * s, v.z * s, w);
    }
    let angle = angle_sq.sqrt();
    let half = angle * 0.5;
    let (sin, cos) = det_sin_cos(half);
    let s = sin / angle;
    Quat::from_xyzw(v.x * s, v.y * s, v.z * s, cos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_sin_cos_tracks_libm_everywhere() {
        for i in -4000..=4000 {
            let x = i as f32 * 0.01;
            let (s, c) = det_sin_cos(x);
            assert!((s - x.sin()).abs() <= 3.0e-7, "sin {x}");
            assert!((c - x.cos()).abs() <= 3.0e-7, "cos {x}");
        }
        assert_eq!(det_sin_cos(0.0), (0.0, 1.0));
        assert_eq!(det_sin_cos(18.849556), det_sin_cos(6.0 * std::f32::consts::PI));
        // Range boundary (muse fix-review, fix-physics1-determinism finding 4): at 2^24 the
        // result still tracks libm; past that the f64 reduction cannot place f32 ulps.
        let big = 16_777_215.0_f32; // 2^24 - 1
        let (s, c) = det_sin_cos(big);
        assert!((s - big.sin()).abs() <= 1.0e-5, "sin {big}");
        assert!((c - big.cos()).abs() <= 1.0e-5, "cos {big}");
    }

    /// The correctly rounded `atan2` reference (the `f64` libm rounded once to `f32`). The
    /// platform's `atan2f` is not one: macOS lands one ulp off glibc in places (for example
    /// `atan2(-7.855, -3.7)`: -2.0110068 there, -2.0110066 correctly rounded), and a 2e-7 bound is
    /// under one ulp in `[2, PI)` (ADR-5550).
    fn reference_atan2(y: f32, x: f32) -> f32 {
        f64::from(y).atan2(f64::from(x)) as f32
    }

    /// The correctly rounded reference of a one-argument function (the `f64` libm rounded once to
    /// `f32`), for the same reason as [`reference_atan2`]: macOS's `acosf(-1)` is one ulp off.
    fn reference(f: fn(f64) -> f64, x: f32) -> f32 {
        f(f64::from(x)) as f32
    }

    #[test]
    fn det_atan2_tracks_libm_everywhere() {
        // The reference is the `f64` libm rounded once (`reference_atan2`): macOS `atan2f` rounds
        // results near `±π` toward zero, one `f32` ulp (2.4e-7) from the nearest value.
        for i in -2000..=2000 {
            let y = i as f32 * 0.013;
            for j in 1..=64 {
                let x = j as f32 * 0.11;
                let got = det_atan2(y, x);
                assert!((got - reference_atan2(y, x)).abs() <= 2.0e-7, "atan2({y}, {x})");
                let xn = -x;
                let got = det_atan2(y, xn);
                assert!((got - reference_atan2(y, xn)).abs() <= 2.0e-7, "atan2({y}, {xn})");
            }
        }
        assert_eq!(det_atan2(0.0, 1.0), 0.0);
        assert_eq!(det_atan2(0.0, -1.0), std::f32::consts::PI);
        assert_eq!(det_atan2(1.0, 0.0), std::f32::consts::FRAC_PI_2);
        // The quadrant choice for `x < 0` keys on the sign bit, not `>= 0.0`: `-0.0 >= 0.0`
        // is true, and `atan2f(-0, x < 0)` is `-π`, not `+π` (IEEE 754).
        assert_eq!(det_atan2(-0.0, -1.0), -std::f32::consts::PI);
        assert_eq!(det_atan2(-0.0, 0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(det_atan2(0.0, -0.0), std::f32::consts::PI);
        assert!(det_atan2(f32::NAN, 1.0).is_nan());
        // The IEEE-754 pairs: both signs of zero and all four infinity combinations.
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
        // Signed zeros and infinities follow the libm convention (muse fix-review,
        // fix-physics1-determinism finding 3): `atan2(-0.0, x < 0)` is `-π`, not `+π`, and the
        // infinite arguments divide to NaN unless resolved before the quotient.
        assert_eq!(det_atan2(-0.0, -1.0), -std::f32::consts::PI);
        let pos_zero = det_atan2(-0.0, 1.0);
        assert_eq!(pos_zero, 0.0);
        assert!(pos_zero.is_sign_negative(), "atan2(-0.0, x > 0) is -0.0");
        for (y, x, want) in [
            (f32::INFINITY, f32::INFINITY, std::f32::consts::FRAC_PI_4),
            (f32::NEG_INFINITY, f32::INFINITY, -std::f32::consts::FRAC_PI_4),
            (
                f32::INFINITY,
                f32::NEG_INFINITY,
                3.0 * std::f32::consts::FRAC_PI_4,
            ),
            (
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
                -3.0 * std::f32::consts::FRAC_PI_4,
            ),
            (f32::INFINITY, 0.0, std::f32::consts::FRAC_PI_2),
            (f32::NEG_INFINITY, 0.0, -std::f32::consts::FRAC_PI_2),
            (1.0, f32::INFINITY, 0.0),
            (-1.0, f32::INFINITY, 0.0),
            (1.0, f32::NEG_INFINITY, std::f32::consts::PI),
            (-1.0, f32::NEG_INFINITY, -std::f32::consts::PI),
        ] {
            let got = det_atan2(y, x);
            assert!(
                (got - want).abs() <= 2.0e-7,
                "atan2({y}, {x}) = {got}, want {want}"
            );
        }
        assert!(det_atan2(-1.0, f32::INFINITY).is_sign_negative());
        assert!(det_atan2(f32::INFINITY, f32::NEG_INFINITY) > 0.0);
        assert!(det_atan2(f32::NEG_INFINITY, f32::NEG_INFINITY) < 0.0);
    }

    /// Both signs of zero follow the libm convention, as the doc says, including `y = −0` with a
    /// negative `x` (`−π`, not `+π`). The van Oosterom–Strackee solid angle hits that case for a
    /// point on a triangle's plane inside it, and used `f64::atan2` before ADR-4773, which gives
    /// `−2π` there (corr-core audit).
    #[test]
    fn det_atan2_signed_zero_follows_libm() {
        for (y, x) in [
            (-0.0f64, -1.0f64),
            (0.0, -1.0),
            (-0.0, 1.0),
            (0.0, 1.0),
            (-0.0, -0.0),
            (0.0, -0.0),
        ] {
            let got = det_atan2_f64(y, x);
            let want = y.atan2(x);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "atan2({y:?}, {x:?}): {got} vs libm {want}"
            );
            // The `f64` value rounded once: the platform `atan2f` may not round `π` to nearest
            // (macOS returns 0x40490fda, the kernel 0x40490fdb), and the sign is what is tested.
            let got = det_atan2(y as f32, x as f32);
            assert_eq!(got.to_bits(), (want as f32).to_bits(), "f32 atan2({y:?}, {x:?})");
        }
        // Coplanar with the triangle, inside it: the libm value is −2π for a −0 numerator.
        assert_eq!(2.0 * det_atan2_f64(-0.0, -0.5), -std::f64::consts::TAU);
    }

    #[test]
    fn det_acos_and_det_atan_track_libm() {
        // `f64` libm rounded once, as in `det_atan2_tracks_libm_everywhere` (macOS `acosf(-1)`
        // is 0x40490fda, one ulp under the nearest `π`).
        for i in -1000..=1000 {
            let x = i as f32 * 0.001;
            assert!(
                (det_acos(x) - reference(f64::acos, x)).abs() <= 2.0e-7,
                "acos {x}"
            );
            assert!(
                (det_atan(x) - reference(f64::atan, x)).abs() <= 2.0e-7,
                "atan {x}"
            );
        }
        assert!(det_acos(2.0).is_nan());
        assert_eq!(det_acos(1.0), 0.0);
        assert_eq!(det_acos(-1.0), std::f32::consts::PI);
    }

    #[test]
    fn det_powf_tracks_libm_for_wind_and_tire_shapes() {
        // Wind: base in (0, 1), falloff 0..4.
        for i in 1..=100 {
            let base = i as f32 * 0.0099;
            for exp in [0.5_f32, 1.0, 1.7, 2.0, 2.5, 3.0, 4.0] {
                let got = det_powf(base, exp);
                let want = base.powf(exp);
                assert!((got - want).abs() <= 2.0e-7 * want.max(1.0), "{base}^{exp}");
            }
        }
        // Tire load factor shapes: base around 0.5, negative exponent.
        for base in [0.25_f32, 0.5, 0.75, 1.5, 3.0] {
            let got = det_powf(base, -0.2);
            let want = base.powf(-0.2);
            assert!((got - want).abs() <= 2.0e-7 * want, "{base}^-0.2");
        }
        assert_eq!(det_powf(0.0, 2.0), 0.0);
        assert_eq!(det_powf(0.0, -2.0), f32::INFINITY);
        assert_eq!(det_powf(3.0, 0.0), 1.0);
        assert!(det_powf(-2.0, 0.5).is_nan());
    }

    #[test]
    fn det_cbrt_tracks_libm() {
        for i in 1..=2000 {
            let x = i as f32 * 0.173;
            assert!(
                (det_cbrt(x) - x.cbrt()).abs() <= 1.0e-7 * x.cbrt().max(1.0),
                "{x}"
            );
            assert_eq!(det_cbrt(-x), -det_cbrt(x));
        }
        assert_eq!(det_cbrt(0.0), 0.0);
        assert_eq!(det_cbrt(f32::INFINITY), f32::INFINITY);
    }

    #[test]
    fn det_quat_constructors_track_glam() {
        for (axis, angle) in [
            (Vec3::Y, 0.4_f32),
            (Vec3::X, -1.7),
            (Vec3::new(1.0, 2.0, 3.0), 2.2),
            (Vec3::Y, 0.0),
        ] {
            let got = det_quat_from_axis_angle(axis, angle);
            let want = Quat::from_axis_angle(axis.normalize_or_zero(), angle);
            assert!(got.abs_diff_eq(want, 2.0e-7), "{got:?} vs {want:?}");
        }
        // Small-angle polynomial branch.
        let v = Vec3::new(1e-5, -2e-5, 3e-5);
        let got = det_quat_from_scaled_axis(v);
        let want = Quat::from_scaled_axis(v);
        assert!(got.abs_diff_eq(want, 1.0e-9), "{got:?} vs {want:?}");
        let v = Vec3::new(0.3, 0.7, -0.2);
        let got = det_quat_from_scaled_axis(v);
        let want = Quat::from_scaled_axis(v);
        assert!(got.abs_diff_eq(want, 2.0e-7), "{got:?} vs {want:?}");
    }
}
