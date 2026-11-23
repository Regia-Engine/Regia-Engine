// SPDX-License-Identifier: MPL-2.0
//! Platform-independent transcendental functions for placement paths (spec 62 § 6.1 rule 3).
//!
//! The platform libm's `sin`, `atan2`, `exp` and `ln` may differ in the last bit between Linux,
//! Windows, Android and macOS. These versions use only `+ - * /`, `floor`, `sqrt` and exact bit
//! manipulation, which IEEE 754 defines exactly, so every platform gives the same bits. Method:
//! range reduction plus truncated Taylor series evaluated in f64 (textbook numerical analysis);
//! the error bounds are tests.

use std::f64::consts::{FRAC_PI_2, LN_2, PI};

/// `x.floor()` without a call: on baseline x86-64 `f64::floor` is a library function, and noise and
/// rotation nodes need several per point. The same bits for every number, infinity and `-0.0`; a NaN
/// stays that NaN on every platform (library floors differ on whether they quiet a signaling one).
#[inline]
pub fn floor(x: f64) -> f64 {
    // At 2^52 and above every float is an integer; NaN fails the test too and is returned as is.
    if x.abs() < 4_503_599_627_370_496.0 {
        let t = x as i64 as f64;
        (t - f64::from(u8::from(t > x))).copysign(x)
    } else {
        x
    }
}

/// `(sin x, cos x)` for `x` in radians. Absolute error ≤ 2e-6 over [−1000, 1000] (tested; the
/// series error alone is below 1e-11 there).
#[inline]
pub fn sin_cos(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    // x = k · π/2 + r with |r| ≤ π/4.
    let k = floor(x / FRAC_PI_2 + 0.5);
    let r = x - k * FRAC_PI_2;
    let r2 = r * r;
    // Horner forms of the Taylor series to r^13 / r^12.
    let s = r
        * (1.0
            + r2 * (-1.0 / 6.0
                + r2 * (1.0 / 120.0
                    + r2 * (-1.0 / 5040.0
                        + r2 * (1.0 / 362_880.0 + r2 * (-1.0 / 39_916_800.0 + r2 / 6_227_020_800.0))))));
    let c = 1.0
        + r2 * (-0.5
            + r2 * (1.0 / 24.0
                + r2 * (-1.0 / 720.0
                    + r2 * (1.0 / 40_320.0 + r2 * (-1.0 / 3_628_800.0 + r2 / 479_001_600.0)))));
    let q = k - 4.0 * floor(k / 4.0);
    match q as i32 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// `sin_cos` of an angle in degrees.
pub fn sin_cos_deg(deg: f64) -> (f64, f64) {
    sin_cos(deg * (PI / 180.0))
}

/// `atan(t)` for `|t| ≤ 1`.
fn atan_unit(t: f64) -> f64 {
    // Two halvings: atan(u) = 2 atan(u / (1 + sqrt(1 + u²))), so |u| ≤ tan(π/16) ≈ 0.199.
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

/// `atan2(y, x)` in radians, in (−π, π]. Absolute error ≤ 4e-6 (tested).
pub fn atan2(y: f64, x: f64) -> f64 {
    if !x.is_finite() || !y.is_finite() {
        return f64::NAN;
    }
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    let (ax, ay) = (x.abs(), y.abs());
    let base = if ay <= ax {
        atan_unit(ay / ax)
    } else {
        FRAC_PI_2 - atan_unit(ax / ay)
    };
    let a = if x < 0.0 { PI - base } else { base };
    if y < 0.0 { -a } else { a }
}

/// `2^x`. Returns 0 below −1074 and +∞ above 1024.
pub fn exp2(x: f64) -> f64 {
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
    let f = (x - n) * LN_2; // |f| ≤ ln2 / 2
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

/// `log2(x)`; NaN for negative input, −∞ at 0.
pub fn log2(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    // Normalize subnormals.
    let (x, bias) = if x < f64::MIN_POSITIVE {
        (x * f64::from_bits((1023 + 64) << 52), -64.0)
    } else {
        (x, 0.0)
    };
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52)); // [1, 2)
    if m > std::f64::consts::SQRT_2 {
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
    e as f64 + bias + 2.0 * sum / LN_2
}

/// `x^y`. Negative `x` needs an integral `y`; `0^y` is 0 (y > 0), 1 (y = 0) or +∞ (y < 0).
pub fn pow(x: f64, y: f64) -> f64 {
    // IEEE 754 `pow(x, 0)` and `pow(1, y)` are 1 for every other argument, NaN included.
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f64::INFINITY };
    }
    if x < 0.0 {
        if y.floor() != y {
            return f64::NAN;
        }
        let r = exp2(y * log2(-x));
        let odd = (y / 2.0).floor() * 2.0 != y;
        return if odd { -r } else { r };
    }
    exp2(y * log2(x))
}

/// `x^(1/3)` (the geometric mean of three scales): the f64 power rounded to f32 once, so every
/// platform gives the same bits. Platform `cbrt`/`cbrtf` may differ in the last ulp between
/// Linux, Windows, Android and macOS (spec 62 § 6.1 rule 3), and one f32 ulp near 1.0 (~6e-8)
/// crosses the ecology thinning slack. Negative `x` gives the negative root.
pub fn cbrt(x: f32) -> f32 {
    if x.is_nan() || x == 0.0 || x.is_infinite() {
        return x;
    }
    let r = pow(f64::from(x.abs()), 1.0 / 3.0);
    (if x < 0.0 { -r } else { r }) as f32
}

/// `atan` of a ratio for internal use: same bound as [`atan2`].
pub fn atan(t: f64) -> f64 {
    atan2(t, 1.0)
}

/// `asin(v)` for `|v| ≤ 1` (NaN outside).
pub fn asin(v: f64) -> f64 {
    if !(-1.0..=1.0).contains(&v) {
        return f64::NAN;
    }
    atan2(v, (1.0 - v * v).sqrt())
}

/// `acos(v)` for `|v| ≤ 1` (NaN outside).
pub fn acos(v: f64) -> f64 {
    if !(-1.0..=1.0).contains(&v) {
        return f64::NAN;
    }
    atan2((1.0 - v * v).sqrt(), v)
}

#[cfg(test)]
mod tests {
    #[test]
    fn floor_matches_the_library_floor_for_every_number() {
        let edges = [
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            5e-324,
            -5e-324,
            1.0 - f64::EPSILON,
            -1.0 + f64::EPSILON,
            4_503_599_627_370_495.5,
            -4_503_599_627_370_495.5,
            4_503_599_627_370_496.0,
            -4_503_599_627_370_496.0,
            9.3e18,
            -9.3e18,
            f64::MAX,
            f64::MIN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        let mut bits = 0x9E37_79B9_7F4A_7C15u64;
        let random = (0..200_000).map(|_| {
            bits = bits
                .wrapping_mul(0xD134_2543_DE82_EF95)
                .wrapping_add(0x2545_F491_4F6C_DD1D);
            f64::from_bits(bits)
        });
        // Ordinary magnitudes as well: most random bit patterns are huge or tiny.
        let mut x = 0x1234_5678u64;
        let ordinary = (0..200_000).map(|_| {
            x = x.wrapping_mul(0xD134_2543_DE82_EF95).wrapping_add(1);
            (x >> 12) as f64 / 1024.0 - 2.0e9
        });
        for v in edges.into_iter().chain(random).chain(ordinary) {
            if v.is_nan() {
                assert_eq!(super::floor(v).to_bits(), v.to_bits(), "a NaN stays as it is");
            } else {
                assert_eq!(super::floor(v).to_bits(), v.floor().to_bits(), "{v:e}");
            }
        }
    }

    use super::*;

    /// Spec 62 § 10 test 5: 100 000 fixed inputs in [−1000, 1000].
    #[test]
    fn sin_cos_within_2e6_of_the_reference() {
        let mut worst = 0.0f64;
        for i in 0..100_000 {
            let x = -1000.0 + 2000.0 * (i as f64) / 99_999.0;
            let (s, c) = sin_cos(x);
            worst = worst.max((s - x.sin()).abs()).max((c - x.cos()).abs());
        }
        assert!(worst <= 2e-6, "worst sin_cos error {worst}");
    }

    #[test]
    fn atan2_within_4e6_in_every_quadrant() {
        let mut worst = 0.0f64;
        for i in 0..100_000 {
            let a = -1000.0 + 2000.0 * (i as f64) / 99_999.0;
            let (y, x) = (
                a.sin() * (1.0 + (i % 7) as f64),
                (a * 0.37).cos() * (1.0 + (i % 5) as f64),
            );
            worst = worst.max((atan2(y, x) - y.atan2(x)).abs());
        }
        assert!(worst <= 4e-6, "worst atan2 error {worst}");
        assert_eq!(atan2(0.0, 0.0), 0.0);
    }

    #[test]
    fn exp2_log2_pow_are_accurate() {
        for i in 0..2000 {
            let x = -40.0 + 80.0 * (i as f64) / 1999.0;
            let rel = (exp2(x) - x.exp2()).abs() / x.exp2();
            assert!(rel < 1e-12, "exp2({x})");
            let v = 1e-6 + (i as f64) * 0.731;
            assert!((log2(v) - v.log2()).abs() < 1e-12, "log2({v})");
        }
        assert_eq!(exp2(3.0), 8.0);
        assert!((pow(-2.0, 3.0) + 8.0).abs() < 1e-12);
        assert!(pow(-2.0, 0.5).is_nan());
        assert_eq!(pow(0.0, 2.0), 0.0);
        assert!(log2(-1.0).is_nan());
    }

    #[test]
    fn pow_of_one_is_one_for_a_nan_exponent() {
        // IEEE 754: pow(1, y) is 1 for every y, NaN included (platform `powf` agrees). The
        // exponent reaches here from user data: `attr.calc` evaluates `pow` on attribute
        // columns, which can carry NaN (e.g. a `sqrt(-1)` upstream), and the result feeds
        // density and scale decisions. The early return changes no other input's bits:
        // exp2(y * log2(1)) = exp2(0) is exactly 1 already.
        assert_eq!(pow(1.0, f64::NAN), 1.0);
        assert_eq!(pow(1.0, 0.0), 1.0);
        assert_eq!(pow(1.0, -3.5), 1.0);
        assert!(pow(2.0, f64::NAN).is_nan());
        assert_eq!(pow(f64::NAN, 0.0), 1.0);
    }

    #[test]
    fn cbrt_is_deterministic_and_signed() {
        // The ecology geometric mean's non-cube case: (1.1 * 1.2 * 1.3)^(1/3).
        let product = 1.1f32 * 1.2 * 1.3;
        let s = cbrt(product);
        assert_eq!(s, pow(f64::from(product), 1.0 / 3.0) as f32);
        assert!((f64::from(s) - f64::from(product).cbrt()).abs() < 1e-6);
        assert_eq!(cbrt(27.0), 3.0);
        assert_eq!(cbrt(-27.0), -3.0);
        assert_eq!(cbrt(0.0), 0.0);
        assert!(cbrt(f32::NAN).is_nan());
        // Across the 0.5..8 range the result tracks the platform cbrt (a coarse sample: the full
        // 33 M-value scan belongs in the probe, not the unit test).
        let mut bits = 0x3f00_0000u32;
        while bits < 0x4100_0000 {
            let x = f32::from_bits(bits);
            assert!(
                (f64::from(cbrt(x)) - f64::from(x).cbrt()).abs() <= 6e-8,
                "cbrt({x:e})"
            );
            bits += 9973;
        }
    }

    /// Exact output bits for fixed inputs: reverting the call sites (or `cbrt` itself) to the
    /// platform libm can pass tolerance checks on a host whose libm agrees, so the kernel's
    /// results are pinned bit-exact (the kernel is platform-independent by construction).
    /// Values from the fixrev-17 review of the procgen determinism lane (finding 4).
    #[test]
    fn cbrt_pins_exact_output_bits() {
        // (input, input bits, expected output bits).
        let pins: &[(f32, u32, u32)] = &[
            (1.0, 0x3f80_0000, 0x3f80_0000),
            (8.0, 0x4100_0000, 0x4000_0000),
            (27.0, 0x41d8_0000, 0x4040_0000),
            (7.3, 0x40e9_999a, 0x3ff8_4de7),
            (1.0e-3, 0x3a83_126f, 0x3dcc_cccd),
            (0.999_999_94, 0x3f7f_ffff, 0x3f80_0000),
            (-27.0, 0xc1d8_0000, 0xc040_0000),
            (-7.3, 0xc0e9_999a, 0xbff8_4de7),
            (1234.5678, 0x449a_522b, 0x412b_a47e),
            (6.02e-2, 0x3d76_9446, 0x3ec8_a9e7),
        ];
        for &(x, x_bits, expected) in pins {
            assert_eq!(x.to_bits(), x_bits, "the input literal {x:e} moved");
            assert_eq!(cbrt(x).to_bits(), expected, "cbrt({x:e}) bits moved");
        }
    }
}
