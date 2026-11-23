// SPDX-License-Identifier: MPL-2.0
//! Portable float `min`, `max` and `clamp`, and the canonical NaN and zero (ADR-8012 A-4, ADR-8085).
//!
//! IEEE 754 leaves two things open that a lockstep sim can trip over:
//!
//! - **The sign of a zero from `min`/`max`.** `f32::max` and `f32::min` lower to `maxNum`-style
//!   operations that may return either zero when the operands are `+0` and `-0`. Measured with
//!   inputs the optimizer cannot see: x86-64 returns the second operand (`0.0f32.max(-0.0)` is `-0`,
//!   and a standalone `-O3` build gave `-0` for `(-0.0f32).max(0.0)` too, where `-O0` gives `+0`);
//!   on aarch64, `max` of two zeros is always `+0` and `min` always `-0`.
//! - **The sign and payload of a NaN the hardware makes.** `0.0 / 0.0` or `sqrt(-1)` is
//!   `0xffc0_0000` on x86-64 and `0x7fc0_0000` on aarch64.
//!
//! Neither matters to `+ - * /` on its own, but a sign-sensitive step after it does: `atan2`, `1 / x`,
//! `copysign`, `signum`, `is_sign_negative`, or `to_bits` into a hash. `atan2(a.max(b), -1.0)` is `-π`
//! on one peer and `+π` on another, and the match desyncs.
//!
//! The functions here give one answer on every host and at every optimization level:
//!
//! - **Zero rule:** a zero result is `+0`, never `-0`.
//! - **NaN rule:** a NaN result is the canonical quiet NaN `0x7fc0_0000` (`f32`) or
//!   `0x7ff8_0000_0000_0000` (`f64`), whatever the sign or payload of the NaN that produced it.
//! - **NaN operands** of `det_min`/`det_max` are ignored, as std does: the other operand is the
//!   result; two NaNs give the canonical NaN.
//!
//! These are the same two rules as the desync hash's float rule (any NaN hashes as `0x7fc0_0000`,
//! any zero as `+0`), so a value that passes through [`canonical`] hashes the same as the raw value.
//! Away from a zero or NaN result, every function gives exactly std's bits, so swapping
//! `x.max(y)` for `det_max(x, y)` moves no other value.
//!
//! The selection uses only IEEE comparisons, and the result goes through an integer test on its bits,
//! so how the compiler lowers the comparison (a branch, a `select`, `maxss`, `fmaxnm`) cannot change
//! the answer.
//!
//! ```
//! use regia_detsim::float::{canonical, det_clamp, det_max, det_min};
//! use regia_detsim::math;
//!
//! assert_eq!(det_max(-0.0, 0.0).to_bits(), 0); // +0 on every host
//! assert_eq!(det_min(0.0, -0.0).to_bits(), 0);
//! assert_eq!(math::atan2(det_max(-0.0, 0.0), -1.0), core::f32::consts::PI);
//! assert_eq!(det_max(f32::NAN, 2.0), 2.0);
//! assert_eq!(det_clamp(-3.0, -0.0, 1.0).to_bits(), 0);
//! let zero = std::hint::black_box(0.0_f32);
//! assert_eq!(canonical(zero / zero).to_bits(), 0x7fc0_0000);
//! ```

/// The canonical `f32` NaN, a positive quiet NaN with an empty payload.
pub const CANONICAL_NAN_BITS: u32 = 0x7fc0_0000;

/// The canonical `f64` NaN, a positive quiet NaN with an empty payload.
pub const CANONICAL_NAN_BITS_F64: u64 = 0x7ff8_0000_0000_0000;

/// `x` with every NaN replaced by the canonical NaN ([`CANONICAL_NAN_BITS`]) and `-0` replaced by
/// `+0`; every other value (subnormals and infinities included) unchanged, bit for bit.
///
/// Use it on a value before a sign-sensitive step (`atan2`, `1 / x`, `copysign`, `signum`,
/// `is_sign_negative`, `to_bits`) when the value may be a zero or a NaN that came from `min`, `max`,
/// `0 / 0`, `inf - inf` or `sqrt` of a negative.
#[inline]
#[must_use]
pub const fn canonical(x: f32) -> f32 {
    // Integer tests on the bits: no float compare, so no lowering or float mode changes the answer.
    let bits = x.to_bits();
    f32::from_bits(match bits & 0x7fff_ffff {
        0 => 0,
        magnitude if magnitude > 0x7f80_0000 => CANONICAL_NAN_BITS,
        _ => bits,
    })
}

/// The `f64` form of [`canonical`]: NaN to [`CANONICAL_NAN_BITS_F64`], `-0` to `+0`.
#[inline]
#[must_use]
pub const fn canonical_f64(x: f64) -> f64 {
    let bits = x.to_bits();
    f64::from_bits(match bits & 0x7fff_ffff_ffff_ffff {
        0 => 0,
        magnitude if magnitude > 0x7ff0_0000_0000_0000 => CANONICAL_NAN_BITS_F64,
        _ => bits,
    })
}

/// The larger of `a` and `b`, the same on every host: use it in place of `f32::max` on the tick.
///
/// A NaN operand is ignored (the other one is the result); two NaNs give the canonical NaN; a zero
/// result is `+0` (so `det_max(-0.0, 0.0)` and `det_max(0.0, -0.0)` are both `+0`). Otherwise the
/// result equals `a.max(b)` bit for bit.
#[inline]
#[must_use]
pub const fn det_max(a: f32, b: f32) -> f32 {
    canonical(if a.is_nan() || a < b { b } else { a })
}

/// The smaller of `a` and `b`, the same on every host: use it in place of `f32::min` on the tick.
///
/// A NaN operand is ignored (the other one is the result); two NaNs give the canonical NaN; a zero
/// result is `+0` (so `det_min(-0.0, 0.0)` is `+0`, not `-0`). Otherwise the result equals
/// `a.min(b)` bit for bit.
#[inline]
#[must_use]
pub const fn det_min(a: f32, b: f32) -> f32 {
    canonical(if a.is_nan() || a > b { b } else { a })
}

/// `x` limited to `[lo, hi]`, the same on every host: use it in place of `f32::clamp` on the tick.
///
/// The steps are std's: `lo` if `x < lo`, then `hi` if that is `> hi`, else `x`. A NaN `x` gives the
/// canonical NaN (std gives a NaN of the host's sign); a zero result is `+0`. Unlike std it never
/// panics: a NaN bound is ignored and `lo > hi` gives `hi`, so a peer built with debug assertions and
/// one built without take the same path. Otherwise the result equals `x.clamp(lo, hi)` bit for bit.
#[inline]
#[must_use]
pub const fn det_clamp(x: f32, lo: f32, hi: f32) -> f32 {
    let mut r = x;
    if r < lo {
        r = lo;
    }
    if r > hi {
        r = hi;
    }
    canonical(r)
}

/// The `f64` form of [`det_max`] (zero result `+0`, NaN result [`CANONICAL_NAN_BITS_F64`]).
#[inline]
#[must_use]
pub const fn det_max_f64(a: f64, b: f64) -> f64 {
    canonical_f64(if a.is_nan() || a < b { b } else { a })
}

/// The `f64` form of [`det_min`] (zero result `+0`, NaN result [`CANONICAL_NAN_BITS_F64`]).
#[inline]
#[must_use]
pub const fn det_min_f64(a: f64, b: f64) -> f64 {
    canonical_f64(if a.is_nan() || a > b { b } else { a })
}

/// The `f64` form of [`det_clamp`] (never panics; zero result `+0`, NaN result canonical).
#[inline]
#[must_use]
pub const fn det_clamp_f64(x: f64, lo: f64, hi: f64) -> f64 {
    let mut r = x;
    if r < lo {
        r = lo;
    }
    if r > hi {
        r = hi;
    }
    canonical_f64(r)
}

/// A total order on `f32`, the same on every host: `total_cmp` of the [`canonical`] values. Use it for a
/// nearest-first pick or a sort key on the tick (ADR-8655).
///
/// `-0` and `+0` compare equal (both are `+0` after [`canonical`]), every NaN compares equal to every other NaN
/// and greater than every number (`+inf` included), and every other pair orders as `<` does. So a tie never
/// depends on the zero sign or the NaN payload a target produced, and unlike `partial_cmp` it never returns
/// `None`.
#[inline]
#[must_use]
pub fn det_cmp(a: f32, b: f32) -> core::cmp::Ordering {
    canonical(a).total_cmp(&canonical(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn det_cmp_is_a_total_order_that_ignores_zero_sign_and_nan_payload() {
        use core::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(det_cmp(-0.0, 0.0), Equal);
        assert_eq!(det_cmp(1.0, 2.0), Less);
        assert_eq!(det_cmp(-1.0, -2.0), Greater);
        assert_eq!(
            det_cmp(f32::from_bits(0xffc0_0000), f32::from_bits(0x7fc0_0001)),
            Equal
        );
        assert_eq!(det_cmp(f32::NAN, f32::INFINITY), Greater);
        assert_eq!(det_cmp(f32::NEG_INFINITY, -f32::MAX), Less);
    }

    // Compile-time evaluation agrees with the run-time rules (the bit tests with `black_box` inputs
    // are in `tests/float_order_bits.rs`).
    const MAX_ZEROS: f32 = det_max(-0.0, 0.0);
    const MIN_ZEROS: f32 = det_min(0.0, -0.0);
    const CLAMP_NEG: f32 = det_clamp(-2.0, -0.0, 1.0);

    #[test]
    fn const_evaluation_follows_the_zero_rule() {
        assert_eq!(MAX_ZEROS.to_bits(), 0);
        assert_eq!(MIN_ZEROS.to_bits(), 0);
        assert_eq!(CLAMP_NEG.to_bits(), 0);
        assert_eq!(canonical(f32::NAN).to_bits(), CANONICAL_NAN_BITS);
        assert_eq!(canonical_f64(-f64::NAN).to_bits(), CANONICAL_NAN_BITS_F64);
    }
}
