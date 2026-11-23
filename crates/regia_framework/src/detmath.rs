// SPDX-License-Identifier: MPL-2.0
//! Fixed-count scalar kernels for simulation paths that must not follow the host libm.
//!
//! The algorithms use only IEEE arithmetic, `floor`, and exact bit construction. See ADR-4645 and
//! `docs/provenance.md` for the public series sources.

/// `(sin, cos)` of an angle in radians, from a fixed eight-step Taylor kernel on `[-π/4, π/4]`.
pub(crate) fn sin_cos(radians: f32) -> (f32, f32) {
    let q = f64::from(radians) / core::f64::consts::FRAC_PI_2;
    let k = (q + 0.5).floor();
    let x = (q - k) * core::f64::consts::FRAC_PI_2;
    let x2 = x * x;
    let (mut sin, mut cos) = (0.0, 0.0);
    for n in (0..8).rev() {
        let m = f64::from(2 * n);
        cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
        sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
    }
    let sin = x * sin;
    let (sin, cos) = match (k as i64).rem_euclid(4) {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    };
    (sin as f32, cos as f32)
}

/// `exp(x)` for `0 <= x <= 1.5`, evaluated as a fixed 24-term Taylor series in `f64`.
pub(crate) fn exp_positive(x: f32) -> f32 {
    if !(0.0..=1.5).contains(&x) {
        return f32::NAN;
    }
    let x = f64::from(x);
    let mut sum = 0.0;
    for n in (1..=24).rev() {
        sum = 1.0 + sum * x / f64::from(n);
    }
    sum as f32
}

/// `2^x` for `-10 <= x <= 0`: nearest-integer reduction, a fixed Taylor series, and exact scaling.
pub(crate) fn exp2(x: f32) -> f32 {
    if !(-10.0..=0.0).contains(&x) {
        return f32::NAN;
    }
    let x = f64::from(x);
    let k = x.round() as i64;
    let reduced = (x - k as f64) * core::f64::consts::LN_2;
    let mut sum = 0.0;
    for n in (1..=16).rev() {
        sum = 1.0 + sum * reduced / f64::from(n);
    }
    let scale = f64::from_bits(((1023 + k) as u64) << 52);
    (sum * scale) as f32
}
