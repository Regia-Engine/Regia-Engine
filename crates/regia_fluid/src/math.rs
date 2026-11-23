// SPDX-License-Identifier: MPL-2.0
//! Software elementary functions with bit-identical results on every platform (ADR-4753).
//!
//! Rust documents `f32::sqrt`, `f32::sin_cos` and `f32::atan2` as functions whose results may
//! differ between platforms, toolchains and even calls (they lower to the platform libm). The
//! fluid solver pins its state with [`crate::FluidSim::state_digest`], so every float that
//! reaches a particle, a solid pose or a control-flow decision must come from software routines
//! built only on IEEE-basic operations (add, subtract, multiply, divide), which Rust evaluates
//! deterministically.
//!
//! - [`sqrt_f32`] / `sqrt_f64`: Newton iteration seeded from the bit pattern (the classic
//!   `0x1FBD1DF5` / `0x1FF7A3BEA91D9B1B` seeds), three to four halvings of the error per step.
//! - [`sin_cos_f32`]: Cody-Waite range reduction onto `[-pi/4, pi/4]` (a two-part `pi/2`) and a
//!   Taylor polynomial evaluated in f64, rounded once to f32.
//! - `atan2_f32`: two half-angle folds (`tan(t/2) = z / (1 + sqrt(1 + z^2))`, the tangent
//!   half-angle substitution) put the argument below `tan(pi/16)`, where a short Taylor sum is
//!   evaluated in f64.
//!
//! Everything here is original code; the range-reduction shape is the standard one from Cody and
//! Waite, *Software Manual for the Elementary Functions* (Prentice-Hall 1980).

use glam::{Quat, Vec3};

/// `2 / pi`, for the range reduction of [`sin_cos_f32`].
const FRAC_2_PI: f64 = std::f64::consts::FRAC_2_PI;
/// `pi / 2`, high part.
const PIO2_HI: f64 = std::f64::consts::FRAC_PI_2;
/// `pi / 2`, low part (the tail `pi/2 - PIO2_HI`).
const PIO2_LO: f64 = 6.123_233_995_736_766e-17;

/// The square root, identical on every platform (unlike `f32::sqrt`).
///
/// Supported domain: finite, non-negative inputs in the normal range
/// (`f32::MIN_POSITIVE..=f32::MAX`); every caller passes a `length_squared`, which is either
/// zero or normal in metre-scale scenes. Outside the domain the result is deterministic but
/// not the root: subnormal inputs land far off (the bit-seeded estimate floors near 1e-20)
/// and negative inputs (including `-0.0`) return the input itself instead of NaN. Three Newton
/// iterations from the bit-seeded estimate give a correctly rounded-or-one-ulp result,
/// deterministically.
#[must_use]
pub(crate) fn sqrt_f32(x: f32) -> f32 {
    if x <= 0.0 || !x.is_finite() {
        return x;
    }
    // Halving the biased exponent approximates halving log2(x); the additive constant corrects
    // the mantissa. Deterministic: shifts and adds on the bit pattern.
    let mut y = f32::from_bits((x.to_bits() >> 1).wrapping_add(0x1FBD_1DF5));
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// [`sqrt_f32`] for f64 (four iterations cover the wider mantissa). Same supported domain and
/// same out-of-domain behavior.
#[must_use]
pub(crate) fn sqrt_f64(x: f64) -> f64 {
    if x <= 0.0 || !x.is_finite() {
        return x;
    }
    let mut y = f64::from_bits((x.to_bits() >> 1).wrapping_add(0x1FF7_A3BE_A91D_9B1B));
    for _ in 0..4 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// `|v|` on the software sqrt: glam's `Vec3::length` calls `f32::sqrt`, whose result Rust
/// documents as platform-dependent (ADR-4753). Every CPU length that reaches a particle, a
/// solid pose or a control-flow decision goes through here.
#[must_use]
pub(crate) fn len(v: Vec3) -> f32 {
    sqrt_f32(v.length_squared())
}

/// `(sin(x), cos(x))` for `x` in radians, identical on every platform (unlike `f32::sin_cos`).
///
/// The argument is folded onto `[-pi/4, pi/4]` with a two-part `pi/2` (Cody-Waite); every caller
/// passes `|x| <= 100` (angular speeds at most 1000 rad/s, times at most one `dt`), so the
/// reduction constant `k` stays far inside the range where `k * PIO2_HI` rounds exactly.
#[must_use]
pub(crate) fn sin_cos_f32(x: f32) -> (f32, f32) {
    let k = (f64::from(x) * FRAC_2_PI).round();
    let r = (f64::from(x) - k * PIO2_HI) - k * PIO2_LO;
    let r2 = r * r;
    let sin = r
        * (1.0
            + r2 * (-1.0 / 6.0
                + r2 * (1.0 / 120.0
                    + r2 * (-1.0 / 5040.0 + r2 * (1.0 / 362_880.0 + r2 * (-1.0 / 39_916_800.0))))));
    let cos = 1.0
        + r2 * (-1.0 / 2.0
            + r2 * (1.0 / 24.0
                + r2 * (-1.0 / 720.0
                    + r2 * (1.0 / 40_320.0 + r2 * (-1.0 / 3_628_800.0 + r2 * (1.0 / 479_001_600.0))))));
    match (k as i64).rem_euclid(4) {
        0 => (sin as f32, cos as f32),
        1 => (cos as f32, -(sin as f32)),
        2 => (-(sin as f32), -(cos as f32)),
        _ => (-(cos as f32), sin as f32),
    }
}

/// `atan(y, x)` in radians, identical on every platform (unlike `f32::atan2`).
#[must_use]
#[cfg(any(feature = "physics", test))]
pub(crate) fn atan2_f32(y: f32, x: f32) -> f32 {
    atan2_f64(f64::from(y), f64::from(x)) as f32
}

/// The four-quadrant inverse tangent in f64, over arguments reduced so [`atan_unit`] stays in
/// its `[0, 1]` domain.
#[cfg(any(feature = "physics", test))]
fn atan2_f64(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    let (ax, ay) = (x.abs(), y.abs());
    // Swap so the ratio stays <= 1: atan of the smaller-over-larger angle.
    let (num, den, swapped) = if ay <= ax { (ay, ax, false) } else { (ax, ay, true) };
    let mut a = if den == 0.0 {
        std::f64::consts::FRAC_PI_2
    } else {
        atan_unit(num / den)
    };
    if swapped {
        a = std::f64::consts::FRAC_PI_2 - a;
    }
    if x > 0.0 {
        if y < 0.0 { -a } else { a }
    } else if y < 0.0 {
        a - std::f64::consts::PI
    } else {
        std::f64::consts::PI - a
    }
}

/// `atan(z)` for `0 <= z <= 1`: two tangent half-angle folds
/// (`tan(t/2) = z / (1 + sqrt(1 + z^2))`) land below `tan(pi/16) = 0.1989...`, where the
/// alternating Taylor sum through `t^13` is below 1e-11.
#[cfg(any(feature = "physics", test))]
fn atan_unit(z: f64) -> f64 {
    let t1 = z / (1.0 + sqrt_f64(1.0 + z * z));
    let t2 = t1 / (1.0 + sqrt_f64(1.0 + t1 * t1));
    let t2_sq = t2 * t2;
    let p = t2
        * (1.0
            + t2_sq
                * (-1.0 / 3.0
                    + t2_sq
                        * (1.0 / 5.0
                            + t2_sq
                                * (-1.0 / 7.0
                                    + t2_sq * (1.0 / 9.0 + t2_sq * (-1.0 / 11.0 + t2_sq * (1.0 / 13.0)))))));
    4.0 * p
}

/// `q / |q|`, with the software [`sqrt_f32`] (glam's `Quat::normalize` calls `f32::sqrt`).
#[must_use]
pub(crate) fn normalize_quat(q: Quat) -> Quat {
    let len = sqrt_f32(q.length_squared());
    if len > 0.0 { q / len } else { q }
}

/// The quaternion of `turn` radians about `turn`'s axis, identical on every platform (glam's
/// `Quat::from_scaled_axis` calls `f32::sqrt` and `f32::sin_cos`). Returns the identity for a
/// zero (or NaN) turn.
#[must_use]
pub(crate) fn quat_from_scaled_axis(turn: Vec3) -> Quat {
    let angle_sq = turn.length_squared();
    if angle_sq <= 0.0 {
        return Quat::IDENTITY;
    }
    let angle = sqrt_f32(angle_sq);
    let (s, c) = sin_cos_f32(0.5 * angle);
    let scale = s / angle;
    Quat::from_xyzw(turn.x * scale, turn.y * scale, turn.z * scale, c)
}

/// The scaled axis of `q` (`axis * angle`), identical on every platform (glam's
/// `Quat::to_scaled_axis` calls `f32::sqrt` and `f32::atan2`). A rotation with no vector part
/// reads as no turn. Turns above one revolution wrap into an equivalent axis * angle (the
/// quaternion cannot see them); the solver only ever integrates one `dt` at a time, far below
/// `2 * pi`.
#[must_use]
#[cfg(any(feature = "physics", test))]
pub(crate) fn quat_to_scaled_axis(q: Quat) -> Vec3 {
    let v = q.xyz();
    let len_sq = v.length_squared();
    if len_sq <= f32::MIN_POSITIVE {
        return Vec3::ZERO;
    }
    let len = sqrt_f32(len_sq);
    let angle = 2.0 * atan2_f32(len, q.w);
    v * (angle / len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_is_exact_where_it_can_be() {
        // Dyadic perfect squares: exact roots the iteration must land on bit for bit.
        for x in [
            0.0,
            1.0,
            4.0,
            144.0,
            4096.0,
            0.25,
            2.25,
            0.003_906_25,
            2.0f32.powi(-16),
        ] {
            assert_eq!(sqrt_f32(x), x.sqrt(), "sqrt({x})");
        }
        // Newton lands on one ulp at worst; never more. Sampled across the whole normal range
        // (random mantissa and exponent), not one fixed exponent: the accuracy contract is the
        // domain documented on `sqrt_f32`.
        let mut worst = 0.0f64;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..100_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let bits = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            let exponent = 1 + ((bits >> 32) % 254) as u32;
            let x = f32::from_bits(((bits >> 9) as u32 & 0x007f_ffff) | (exponent << 23));
            let want = f64::from(x).sqrt();
            let got = f64::from(sqrt_f32(x));
            worst = worst.max((got - want).abs() / want);
        }
        assert!(worst <= 1.5e-7, "sqrt worst relative error {worst}");
    }

    #[test]
    fn sqrt_out_of_domain_behavior_is_documented() {
        // Negative inputs (and -0.0) return the input itself, not NaN; out-of-domain results
        // are deterministic but not the root. Pin both so the documented contract cannot
        // drift silently.
        for x in [-4.0f32, -1.0e-20, -0.0] {
            assert_eq!(sqrt_f32(x), x, "sqrt({x})");
        }
        assert!(sqrt_f32(f32::NAN).is_nan());
        assert_eq!(sqrt_f32(f32::INFINITY), f32::INFINITY);
        // Subnormal input: far off, but deterministic.
        let sub = 1.0e-40f32;
        assert!(sub.is_subnormal());
        assert_eq!(sqrt_f32(sub), sqrt_f32(sub));
    }

    #[test]
    fn sqrt_f64_matches_the_platform_sqrt_over_the_normal_range() {
        let mut worst = 0.0f64;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..100_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let bits = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            let exponent = 1 + ((bits >> 32) % 2046);
            let x = f64::from_bits((bits >> 9 & 0x000f_ffff_ffff_ffff) | (exponent << 52));
            let want = x.sqrt();
            let got = f64::from_bits(sqrt_f64(x).to_bits());
            worst = worst.max((got - want).abs() / want);
        }
        // One ulp of f64 at 1.0 is 2.22e-16; allow a hair above, as the f32 test does.
        assert!(worst <= 2.5e-16, "sqrt_f64 worst relative error {worst}");
    }

    #[test]
    fn sin_cos_match_the_platform_libm_within_two_ulp() {
        let mut x = -315.0f32;
        while x <= 315.0 {
            let (s, c) = sin_cos_f32(x);
            let (ws, wc) = x.sin_cos();
            let err = (f64::from(s) - f64::from(ws))
                .abs()
                .max((f64::from(c) - f64::from(wc)).abs());
            assert!(err <= 1.2e-7, "sin_cos({x}): ({s}, {c}) vs ({ws}, {wc})");
            x += 0.013;
        }
        // The exact anchors.
        let (s, c) = sin_cos_f32(0.0);
        assert_eq!((s, c), (0.0, 1.0));
        let (s, c) = sin_cos_f32(std::f32::consts::FRAC_PI_2);
        assert!((s - 1.0).abs() < 1e-7 && c.abs() < 1e-7);
        let (s, c) = sin_cos_f32(std::f32::consts::PI);
        assert!(s.abs() < 1e-7 && (c + 1.0).abs() < 1e-7);
    }

    #[test]
    fn atan2_covers_the_quadrants() {
        let pi = std::f32::consts::PI;
        assert!((atan2_f32(1.0, 1.0) - pi / 4.0).abs() < 1e-6);
        assert!((atan2_f32(0.0, -1.0) - pi).abs() < 1e-6);
        assert!((atan2_f32(-1.0, 0.0) + pi / 2.0).abs() < 1e-6);
        assert!((atan2_f32(1.0, -1.0) - 3.0 * pi / 4.0).abs() < 1e-6);
        assert!((atan2_f32(-1.0, -1.0) + 3.0 * pi / 4.0).abs() < 1e-6);
        assert_eq!(atan2_f32(0.0, 0.0), 0.0);
        let mut worst = 0.0f32;
        let mut a = -4.0f32;
        while a <= 4.0 {
            let mut b = -4.0f32;
            while b <= 4.0 {
                if a != 0.0 || b != 0.0 {
                    let got = f64::from(atan2_f32(a, b));
                    let want = f64::from(a).atan2(f64::from(b));
                    worst = worst.max((got - want).abs() as f32);
                }
                b += 0.11;
            }
            a += 0.11;
        }
        assert!(worst < 1e-6, "atan2 worst error {worst}");
    }

    #[test]
    fn quaternion_helpers_round_trip() {
        // Under one revolution: the exp/log map is a true round trip. (Larger turns wrap into
        // an equivalent axis*angle; the solver only ever integrates one `dt` at a time.)
        for turn in [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.3, -0.7, 0.5),
            Vec3::new(0.0, 0.0, 3.0),
            Vec3::new(-5.0, 0.01, 0.0),
        ] {
            let q = quat_from_scaled_axis(turn);
            let back = quat_to_scaled_axis(q);
            let drift = (back - turn).length() / turn.length().max(1.0);
            assert!(drift < 1e-5, "{turn}: {back}");
        }
        assert_eq!(quat_from_scaled_axis(Vec3::ZERO), Quat::IDENTITY);
        assert_eq!(quat_to_scaled_axis(Quat::IDENTITY), Vec3::ZERO);
    }
}
