// SPDX-License-Identifier: MPL-2.0
//! Panic-free numeric helpers for values that come from user data (graph files, parameters set
//! by game code, IK settings).
//!
//! `f32::clamp` panics when `min > max` or either bound is NaN, and NaN inputs pass straight
//! through it. Animation reads many bounds from authored data (look-at limits, joint limits,
//! pelvis ranges), so those sites use [`clamp_between`] instead: it orders the bounds, treats a
//! NaN bound as absent, and maps a NaN value to the lower bound.

use std::f64::consts::{FRAC_PI_2, PI};

use regia_core::math::Quat;

/// `value` when it is finite, else `fallback`.
#[inline]
pub(crate) fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// Clamp `value` between `a` and `b` in either order, never panicking.
///
/// A NaN bound leaves its side open: callers pass `(lower, upper)`, so a NaN `a` means no lower
/// bound and a NaN `b` no upper bound (the other bound still applies). A NaN `value` returns the
/// lower bound, else the upper one when only that is finite, else `0`.
#[inline]
pub(crate) fn clamp_between(value: f32, a: f32, b: f32) -> f32 {
    let (lo, hi) = match (a.is_nan(), b.is_nan()) {
        (false, false) => (a.min(b), a.max(b)),
        (true, false) => (f32::NEG_INFINITY, b),
        (false, true) => (a, f32::INFINITY),
        (true, true) => return if value.is_nan() { 0.0 } else { value },
    };
    if value.is_nan() {
        return if lo.is_finite() {
            lo
        } else if hi.is_finite() {
            hi
        } else {
            0.0
        };
    }
    value.max(lo).min(hi)
}

/// A weight read from user data: non-finite becomes `0`, the rest clamps to `0..=1`.
#[inline]
pub(crate) fn unit_weight(value: f32) -> f32 {
    finite_or(value, 0.0).clamp(0.0, 1.0)
}

/// Platform-independent `(sin, cos)` in radians, rounded once to `f32`.
///
/// The f64 kernel reduces to `[-pi/4, pi/4]` and evaluates Taylor series with only IEEE
/// arithmetic, `floor` and `sqrt`. Its absolute error is at most `2e-6` over `[-1000, 1000]`.
pub(crate) fn det_sin_cos(angle: f32) -> (f32, f32) {
    let (sin, cos) = det_sin_cos_f64(f64::from(angle));
    (sin as f32, cos as f32)
}

/// The f64 form of [`det_sin_cos`], also used before constructing a half-angle quaternion.
pub(crate) fn det_sin_cos_f64(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    // Past 1e6 rad the `k * pi/2` reduction below loses the remainder (1e18 gave values far
    // outside [-1, 1]; audit corr-animation). `%` is an exact IEEE remainder, so this first
    // reduction is platform-independent and leaves every angle under 1e6 bit-identical.
    let x = if x.abs() > 1.0e6 {
        x % std::f64::consts::TAU
    } else {
        x
    };
    // x = k * pi/2 + r with |r| <= pi/4.
    let k = (x / FRAC_PI_2 + 0.5).floor();
    let r = x - k * FRAC_PI_2;
    let r2 = r * r;
    let sin = r
        * (1.0
            + r2 * (-1.0 / 6.0
                + r2 * (1.0 / 120.0
                    + r2 * (-1.0 / 5040.0
                        + r2 * (1.0 / 362_880.0 + r2 * (-1.0 / 39_916_800.0 + r2 / 6_227_020_800.0))))));
    let cos = 1.0
        + r2 * (-0.5
            + r2 * (1.0 / 24.0
                + r2 * (-1.0 / 720.0
                    + r2 * (1.0 / 40_320.0 + r2 * (-1.0 / 3_628_800.0 + r2 / 479_001_600.0)))));
    let quadrant = k - 4.0 * (k / 4.0).floor();
    match quadrant as i32 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// `atan(t)` for `|t| <= 1`, using two half-angle steps before its Taylor series.
fn det_atan_unit(t: f64) -> f64 {
    let u1 = t / (1.0 + (1.0 + t * t).sqrt());
    let u = u1 / (1.0 + (1.0 + u1 * u1).sqrt());
    let u2 = u * u;
    let mut term = u;
    let mut sum = 0.0;
    let mut n = 1.0;
    for _ in 0..10 {
        sum += term / n;
        term *= -u2;
        n += 2.0;
    }
    4.0 * sum
}

/// Platform-independent `atan2(y, x)` in radians, rounded once to `f32`.
pub(crate) fn det_atan2(y: f32, x: f32) -> f32 {
    let (x, y) = (f64::from(x), f64::from(y));
    if !x.is_finite() || !y.is_finite() {
        return f32::NAN;
    }
    if y == 0.0 {
        return if x.is_sign_negative() {
            PI.copysign(y) as f32
        } else {
            y as f32
        };
    }
    if x == 0.0 {
        return FRAC_PI_2.copysign(y) as f32;
    }
    let (ax, ay) = (x.abs(), y.abs());
    let base = if ay <= ax {
        det_atan_unit(ay / ax)
    } else {
        FRAC_PI_2 - det_atan_unit(ax / ay)
    };
    let angle = if x < 0.0 { PI - base } else { base };
    angle.copysign(y) as f32
}

/// `v * 2^n` through exact exponent-field steps.
fn det_scale_pow2(mut value: f64, mut n: i32) -> f64 {
    while n > 1000 {
        value *= f64::from_bits(((1023 + 1000) as u64) << 52);
        n -= 1000;
    }
    while n < -1000 {
        value *= f64::from_bits(((1023 - 1000) as u64) << 52);
        n += 1000;
    }
    value * f64::from_bits(((1023 + n) as u64) << 52)
}

/// Platform-independent `2^x`, with a Taylor series over the fractional exponent.
fn det_exp2(x: f64) -> f64 {
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
    let fraction = (x - n) * std::f64::consts::LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..15 {
        term *= fraction / f64::from(k);
        sum += term;
    }
    det_scale_pow2(sum, n as i32)
}

/// Platform-independent `exp(x)`, rounded once to `f32`.
pub(crate) fn det_exp(x: f32) -> f32 {
    det_exp2(f64::from(x) * std::f64::consts::LOG2_E) as f32
}

/// A +Y yaw quaternion built from [`det_sin_cos_f64`], without platform trigonometry.
pub(crate) fn det_rotation_y(angle: f32) -> Quat {
    let (sin, cos) = det_sin_cos_f64(f64::from(angle) * 0.5);
    Quat::from_xyzw(0.0, sin as f32, 0.0, cos as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_between_orders_bounds_and_survives_nan() {
        assert_eq!(clamp_between(5.0, 1.0, 2.0), 2.0);
        assert_eq!(clamp_between(5.0, 2.0, 1.0), 2.0, "reversed bounds");
        assert_eq!(clamp_between(-5.0, 2.0, 1.0), 1.0);
        assert_eq!(clamp_between(f32::NAN, -1.0, 1.0), -1.0);
        assert_eq!(clamp_between(3.0, f32::NAN, 1.0), 1.0, "a NaN bound is ignored");
        assert_eq!(clamp_between(3.0, f32::NAN, f32::NAN), 3.0);
        assert_eq!(clamp_between(f32::NAN, f32::NAN, f32::NAN), 0.0);
        assert_eq!(clamp_between(0.5, f32::NEG_INFINITY, f32::INFINITY), 0.5);
    }

    /// audit-v034b-numerics H1: one NaN bound used to collapse both bounds onto the other one,
    /// pinning every value (a one-sided joint limit pinned every twist to that limit).
    #[test]
    fn a_single_nan_bound_leaves_the_other_side_open() {
        assert_eq!(
            clamp_between(0.0, f32::NAN, 1.0),
            0.0,
            "only the upper bound applies"
        );
        assert_eq!(
            clamp_between(5.0, 1.0, f32::NAN),
            5.0,
            "only the lower bound applies"
        );
        assert_eq!(clamp_between(-3.0, 1.0, f32::NAN), 1.0);
        assert_eq!(clamp_between(3.0, f32::NAN, 1.0), 1.0);
        // A NaN value still lands on a finite bound.
        assert_eq!(clamp_between(f32::NAN, f32::NAN, -5.0), -5.0);
        assert_eq!(clamp_between(f32::NAN, 2.0, f32::NAN), 2.0);
    }

    #[test]
    fn unit_weight_maps_non_finite_to_zero() {
        assert_eq!(unit_weight(f32::NAN), 0.0);
        assert_eq!(unit_weight(f32::INFINITY), 0.0);
        assert_eq!(unit_weight(2.0), 1.0);
        assert_eq!(unit_weight(-1.0), 0.0);
        assert_eq!(unit_weight(0.25), 0.25);
    }

    /// Audit (corr-animation): a huge finite angle (a crowd agent's yaw from user data) gave
    /// sine and cosine far outside [-1, 1], or infinities.
    #[test]
    fn huge_angles_stay_on_the_unit_circle() {
        for angle in [1.0e7f32, 1.0e15, 1.0e18, -1.0e30, f32::MAX] {
            let (s, c) = det_sin_cos(angle);
            assert!(s.abs() <= 1.0 && c.abs() <= 1.0, "{angle}: ({s}, {c})");
            assert!((s * s + c * c - 1.0).abs() < 1e-5, "{angle}: ({s}, {c})");
        }
    }

    #[test]
    fn portable_trig_matches_its_fixed_arithmetic_vectors() {
        let angle = f32::from_bits(0x3edf_285b);
        let (sin, cos) = det_sin_cos(angle);
        assert_eq!(sin.to_bits(), 0x3ed8_28b0);
        assert_eq!(cos.to_bits(), 0x3f68_1102);
        // This input is one ULP away from glibc's `sinf`, which gives the call-site tests a
        // concrete regression vector on that host without baking in its result as the oracle.
        assert_eq!(det_atan2(-0.0, -1.0).to_bits(), (-PI as f32).to_bits());
        assert_eq!(det_atan2(0.0, 0.0).to_bits(), 0.0f32.to_bits());
        assert!(det_atan2(f32::NAN, 1.0).is_nan());
        assert_eq!(det_exp(f32::from_bits(0xc13c_feab)).to_bits(), 0x36f8_c396);
        assert_eq!(det_exp(f32::NEG_INFINITY), 0.0);
        assert_eq!(det_exp(f32::INFINITY), f32::INFINITY);
        for i in 0..10_000 {
            let x = -1000.0 + 2000.0 * i as f64 / 9999.0;
            let (s, c) = det_sin_cos_f64(x);
            assert!((s - x.sin()).abs() <= 2e-6);
            assert!((c - x.cos()).abs() <= 2e-6);
        }
    }
}
