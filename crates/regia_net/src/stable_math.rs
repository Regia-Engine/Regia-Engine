// SPDX-License-Identifier: MPL-2.0
//! Bit-stable scalar float math for predicted state (spec 73 § 11, ADR-4770).
//!
//! Prediction and reconciliation evaluate the same pose math on the server (Linux/Windows
//! x86_64) and on clients (Android/Apple Silicon aarch64), so every operation on predicted
//! state must round identically on every platform. glam's SIMD quaternion paths do not:
//! the SSE2 horizontal dot sums as `(x + z) + (y + w)` while NEON sums pairwise as
//! `(x + y) + (z + w)`; NEON multiplies fuse (`vfmaq`) where SSE2 rounds mul and add
//! separately; `Quat::slerp`, `Quat::angle_between` and `f32::powf` call per-ISA
//! approximations and per-libc libm. Any of these on predicted state makes one platform
//! flip a tolerance decision or a hemisphere where another does not, forcing perpetual
//! corrections. Every function here is plain IEEE `f32` arithmetic in a fixed operation
//! order — no SIMD horizontal sums, no fused multiply-add, no libm — matching the wire
//! codec's `normalize4` convention (spec 71 § 12).
//!
//! The polynomial coefficients are original to this crate: Chebyshev fits computed for
//! this module (see `docs/provenance.md`).

use glam::{Quat, Vec3};

/// Coefficients of `sin(x) = x * (c0 + c1 u + ... + c6 u^6)` with `u = x^2`, fitted on
/// `[0, FRAC_PI_2]`; max absolute error `1.6e-7` over the domain.
const SIN_C: [f32; 7] = [
    1.0,
    -0.166_666_67,
    0.008_333_334,
    -0.000_198_412_67,
    0.000_002_755_698,
    -0.000_000_025_031_38,
    0.000_000_000_154_280_26,
];

/// Coefficients of `2^x = c0 + c1 x + ... + c6 x^6` fitted on `[-0.5, 0.5]`; max relative
/// error `1e-7` over the domain.
const POW2_C: [f32; 7] = [
    1.0,
    // The Chebyshev fit's first-order coefficient rounds to exactly `LN_2`.
    core::f32::consts::LN_2,
    0.240_226_5,
    0.055_503_406,
    0.009_618_04,
    0.001_339_528_2,
    0.000_154_653_13,
];

/// 4-wide dot product summed left to right (`((x + y) + z) + w`), the same association the
/// wire codec's `normalize4` uses.
pub(crate) fn quat_dot4(a: Quat, b: Quat) -> f32 {
    let s = a.x * b.x + a.y * b.y;
    let s = s + a.z * b.z;
    s + a.w * b.w
}

/// Normalizes a quaternion with a left-fold length (see [`quat_dot4`]). Degenerate input
/// (zero or non-finite length) yields the identity, matching the wire codec's fallback.
pub(crate) fn quat_normalize(q: Quat) -> Quat {
    let len2 = q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w;
    if !len2.is_finite() || len2 <= 0.0 {
        return Quat::IDENTITY;
    }
    let len = len2.sqrt();
    Quat::from_xyzw(q.x / len, q.y / len, q.z / len, q.w / len)
}

/// Normalized linear interpolation of two (near-)unit quaternions: hemisphere flip by the
/// sign of the left-fold dot, then per-component `a + f * (b - a)`, then [`quat_normalize`].
/// Bit-identical on every platform, unlike `Quat::lerp`.
pub(crate) fn quat_nlerp(a: Quat, b: Quat, f: f32) -> Quat {
    let end = if quat_dot4(a, b) < 0.0 { -b } else { b };
    quat_normalize(Quat::from_xyzw(
        a.x + f * (end.x - a.x),
        a.y + f * (end.y - a.y),
        a.z + f * (end.z - a.z),
        a.w + f * (end.w - a.w),
    ))
}

/// Hamilton product with plain IEEE multiply/subtract in a fixed order (glam's NEON path
/// fuses multiply-adds, the SSE2 path does not, so `Quat * Quat` is not cross-platform
/// bit-stable).
pub(crate) fn quat_mul(a: Quat, b: Quat) -> Quat {
    let x = a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y;
    let y = a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x;
    let z = a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w;
    let w = a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z;
    Quat::from_xyzw(x, y, z, w)
}

/// Rotates `v` by the (near-)unit quaternion `q` with the two-cross formula
/// `t = 2 q_v × v; v + q_w t + q_v × t`, all scalar IEEE ops (glam's `Quat * Vec3` uses
/// SIMD dot products and fused multiply-adds).
pub(crate) fn quat_rotate_vec3(q: Quat, v: Vec3) -> Vec3 {
    let u = Vec3::new(q.x, q.y, q.z);
    let t = 2.0 * u.cross(v);
    let c = u.cross(t);
    Vec3::new(
        v.x + q.w * t.x + c.x,
        v.y + q.w * t.y + c.y,
        v.z + q.w * t.z + c.z,
    )
}

/// Hemisphere-aligned squared chord between two (near-)unit quaternions: the squared
/// Euclidean distance in R⁴ after flipping `b` into `a`'s hemisphere, summed left to right.
/// For unit quats this equals `2 - 2 |dot|` and is monotone in the rotation angle, so
/// `quat_rot_chord2(a, b) <= angle_chord2(r)` decides "rotation within `r` radians"
/// without `Quat::angle_between`'s ISA-specific dot and `acos`.
pub(crate) fn quat_rot_chord2(a: Quat, b: Quat) -> f32 {
    let b = if quat_dot4(a, b) < 0.0 { -b } else { b };
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    let dw = a.w - b.w;
    dx * dx + dy * dy + dz * dz + dw * dw
}

/// The squared-chord threshold corresponding to a rotation-angle tolerance of `radians`:
/// the chord between two unit quaternions at rotation angle `θ` is `2 sin(θ / 4)`, so the
/// threshold is `4 sin²(radians / 4)`. Negative `radians` yields a negative threshold
/// (nothing matches, as with `angle_between`); `radians >= PI` yields `4.0` (everything
/// matches); NaN propagates so nothing matches.
pub(crate) fn angle_chord2(radians: f32) -> f32 {
    if radians.is_nan() {
        return f32::NAN;
    }
    if radians < 0.0 {
        return -1.0;
    }
    if radians >= core::f32::consts::PI {
        return 4.0;
    }
    let s = stable_sin(radians * 0.25);
    4.0 * s * s
}

/// Sine for `|x| <= FRAC_PI_2` by a fixed-order odd polynomial ([`SIN_C`]); inputs outside
/// the fitted domain are clamped so the result stays finite and deterministic. Bit-identical
/// on every platform, unlike libm `sinf`.
pub(crate) fn stable_sin(x: f32) -> f32 {
    let sign = if x.is_sign_negative() { -1.0 } else { 1.0 };
    let x = x.abs().min(core::f32::consts::FRAC_PI_2);
    let u = x * x;
    let mut p = SIN_C[6];
    for c in SIN_C[0..6].iter().rev() {
        p = p * u + *c;
    }
    sign * (x * p)
}

/// `2^x` by argument reduction `x = round(x) + f` and a fixed-order degree-6 polynomial for
/// `2^f` on `[-0.5, 0.5]` ([`POW2_C`]), scaled by an exact power of two (relative error
/// `1e-7`, bit-identical on every platform, unlike libm `powf`). `x >= 128` overflows to
/// `+INF`; `x <= -150` underflows to `0.0`; NaN propagates. A value in `[127.5, 128)` rounds
/// half away from zero to 128, whose exponent field is infinity, so that one power is folded
/// back (`2^127 * 2^f * 2`) and the result stays finite.
pub(crate) fn stable_pow2(x: f32) -> f32 {
    if x.is_nan() {
        return f32::NAN;
    }
    if x >= 128.0 {
        return f32::INFINITY;
    }
    if x <= -150.0 {
        return 0.0;
    }
    let n = x.round();
    let f = x - n;
    let mut p = POW2_C[6];
    for c in POW2_C[0..6].iter().rev() {
        p = p * f + *c;
    }
    let n = n as i32;
    if n >= 128 {
        // `f` is in `[-0.5, 0)`. Multiply by `2^127` before the extra factor of two:
        // `2^127 * 2` is already infinity.
        let scale = f32::from_bits(254u32 << 23);
        return (scale * p) * 2.0;
    }
    if n >= -126 {
        // Normal range: 2^n is an exact bit pattern.
        f32::from_bits(((n + 127) as u32) << 23) * p
    } else {
        // n in (-150, -126): two exact multiplies reach the subnormal range.
        let hi = f32::from_bits(1 << 23); // 2^-126
        let lo = f32::from_bits(((n + 253) as u32) << 23); // 2^(n + 126)
        (p * hi) * lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_sin_matches_libm_within_2e_minus_7() {
        for i in 0..=10_000 {
            let x = (i as f32 / 10_000.0) * core::f32::consts::FRAC_PI_2;
            let got = f64::from(stable_sin(x));
            let want = f64::from(x).sin();
            assert!((got - want).abs() < 2e-7, "sin({x}) = {got}, want {want}");
        }
    }

    #[test]
    fn stable_pow2_matches_libm_within_1e_minus_6_relative() {
        for i in 0..=10_000 {
            let x = (i as f32 / 10_000.0) * 10.0 - 5.0;
            let got = f64::from(stable_pow2(x));
            let want = 2.0f64.powf(f64::from(x));
            assert!(
                (got - want).abs() <= 1e-6 * want.abs(),
                "pow2({x}) = {got}, want {want}"
            );
        }
    }

    #[test]
    fn stable_pow2_exact_powers_of_two() {
        for n in -20..20 {
            let x = n as f32;
            assert_eq!(stable_pow2(x), 2.0f32.powi(n), "2^{n}");
        }
        // 2^127 is the largest finite power of two in f32. It is an integer input, so the
        // reduction must not round it up into the infinity exponent.
        assert_eq!(stable_pow2(127.0), f32::from_bits(254u32 << 23));
    }

    /// `x` in `[127.5, 128)` rounds half away from zero to 128. Building that exponent field
    /// is infinity, but `2^127.5` is finite (about 2.40e38, under `f32::MAX`).
    #[test]
    fn stable_pow2_stays_finite_just_below_128() {
        for x in [127.5f32, 127.75, 127.9] {
            let got = stable_pow2(x);
            assert!(got.is_finite() && got > 0.0, "pow2({x}) = {got}");
            let want = 2.0f64.powf(f64::from(x));
            let rel = (f64::from(got) - want).abs() / want;
            assert!(rel < 1e-6, "pow2({x}) = {got}, want {want}, rel {rel}");
        }
        let lo = stable_pow2(-149.5);
        assert!(lo.is_finite() && lo >= 0.0, "pow2(-149.5) = {lo}");
        assert!(stable_pow2(128.0).is_infinite());
        assert_eq!(stable_pow2(-150.0), 0.0);
    }

    /// The overflow boundary (audit kimirev-fix-net1-determinism, finding 2): below 128 the
    /// result stays finite, at 128 it is +INF.
    #[test]
    fn stable_pow2_overflows_only_at_128() {
        for x in [127.5f32, 127.9] {
            let got = f64::from(stable_pow2(x));
            let want = 2.0f64.powf(f64::from(x));
            assert!(got.is_finite(), "2^{x} finite");
            assert!((got - want).abs() <= 1e-6 * want, "2^{x} = {got}, want {want}");
        }
        assert_eq!(stable_pow2(128.0), f32::INFINITY);
        assert_eq!(stable_pow2(f32::INFINITY), f32::INFINITY);
    }

    #[test]
    fn angle_chord2_monotone_and_bounded() {
        assert_eq!(angle_chord2(-1.0), -1.0);
        assert_eq!(angle_chord2(0.0), 0.0);
        assert!(angle_chord2(4.0) > 4.0 * angle_chord2(2.0));
        assert_eq!(angle_chord2(core::f32::consts::PI), 4.0);
        assert!(angle_chord2(f32::NAN).is_nan());
    }
}
