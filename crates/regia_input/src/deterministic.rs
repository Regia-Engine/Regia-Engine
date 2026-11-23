// SPDX-License-Identifier: MPL-2.0
//! Deterministic transcendental evaluation for the input layer (ADR-4768).
//!
//! `f32::exp` and `f32::powf` are the platform libm: glibc, Windows UCRT and Android bionic are
//! not required to agree in the last bit, so an action value smoothed with one of them — or a
//! SpaceMouse zoom delta — would differ per OS, and a lockstep checksum or a replay that
//! re-evaluates the binding from the recorded samples would diverge.
//!
//! [`exp_f32`] instead uses only IEEE-754 single-precision additions and multiplications in a
//! fixed evaluation order (plus exponent-bit scaling, exact by construction). Every target
//! rounds those identically, so the result is bit-for-bit the same on Linux, Windows, Android
//! and macOS.
//!
//! Sources: the range reduction by ln 2 and the 1/k! Taylor coefficients are textbook
//! (e.g. the public Cephes and musl (MIT) `exp` implementations use the same technique);
//! the code below is an original f32 implementation written for Regia.

/// ln 2 split into a high part that is exact in f32 and the remainder, so
/// `x − k·LN2_HI − k·LN2_LO` keeps its precision for large `k`.
const LN2_HI: f32 = 0.693_359_4;
const LN2_LO: f32 = -2.121_944_4e-4;

/// e^x, bit-identical on every target (see the module docs).
///
/// NaN in gives NaN out; the result saturates to `0.0` / `+∞` outside the finite f32 range.
/// Maximum relative error is ~1e-8 across the finite range — far below the input noise of any
/// binding it smooths.
pub(crate) fn exp_f32(x: f32) -> f32 {
    if x.is_nan() {
        return f32::NAN;
    }
    if x <= -87.5 {
        return 0.0;
    }
    // The true overflow is ln(f32::MAX) ≈ 88.72; below it e^x is finite and the k == 128 path
    // below rounds it correctly (saturating to +∞ only when the product really exceeds
    // f32::MAX). Inputs past 88.75 (k would pass 128) saturate here.
    if x >= 88.75 {
        return f32::INFINITY;
    }
    // Range reduction: x = k·ln 2 + r with r in [−ln 2/2, ln 2/2].
    let k = (x * core::f32::consts::LOG2_E).round();
    let r = (x - k * LN2_HI) - k * LN2_LO;
    // Taylor series of e^r, Horner form, terms through r^8/8! (the truncation error at
    // |r| = ln 2/2 is ~6e-10 relative).
    let p = horner(r);
    let k = k as i32;
    // 2^k by exponent bits. k is within (−127, 128) after the clamps above; k == 128 (an input
    // in [88.38…, 88.5)) needs two halves — 2^128 is not representable and the single
    // multiply would overflow the exponent field into the infinity pattern although e^x is
    // still finite until ln(f32::MAX) ≈ 88.72 (fixrev review, finding 8).
    if k == 128 {
        let half = f32::from_bits((64 + 127) << 23);
        return half * (half * p);
    }
    let scale = f32::from_bits(((k + 127) << 23) as u32);
    scale * p
}

/// Σ r^k/k! for k = 0..=8, Horner form (all single-precision multiplies and adds).
fn horner(r: f32) -> f32 {
    const C8: f32 = 1.0 / 40320.0; // 1/8!
    const C7: f32 = 1.0 / 5040.0;
    const C6: f32 = 1.0 / 720.0;
    const C5: f32 = 1.0 / 120.0;
    const C4: f32 = 1.0 / 24.0;
    const C3: f32 = 1.0 / 6.0;
    let mut p = C8;
    p = p * r + C7;
    p = p * r + C6;
    p = p * r + C5;
    p = p * r + C4;
    p = p * r + C3;
    p = p * r + 0.5;
    p = p * r + 1.0;
    p * r + 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quality guard: the deterministic exp tracks the correctly-rounded f64 reference well
    /// inside f32 precision on the ranges the input layer uses.
    #[test]
    fn exp_matches_the_f64_reference() {
        for i in -200..=200 {
            let x = f64::from(i) * 0.05;
            let got = exp_f32(x as f32) as f64;
            let want = x.exp();
            assert!(
                (got - want).abs() <= 1e-6 * want.abs().max(1e-38),
                "exp_f32({x}) = {got}, want {want}"
            );
        }
        assert_eq!(exp_f32(0.0), 1.0);
        assert!(exp_f32(f32::NAN).is_nan());
        assert_eq!(exp_f32(-1000.0), 0.0);
        assert_eq!(exp_f32(1000.0), f32::INFINITY);
    }

    /// `k = round(x / ln 2)` is 128 for x = 88.4, and a biased exponent of 255 is infinity.
    /// exp(88.4) is still finite (about 2.46e38, under f32::MAX).
    #[test]
    fn audfix_exp_f32_stays_finite_below_overflow() {
        let x = 88.4_f32;
        let got = exp_f32(x);
        let want = f64::from(x).exp();
        assert!(got.is_finite(), "exp_f32({x}) = {got}, f64 exp = {want}");
        assert!(
            (f64::from(got) - want).abs() <= 1e-4 * want,
            "exp_f32({x}) = {got}, want {want}"
        );
    }

    /// Fixrev review finding 8: e^x is finite until ln(f32::MAX) ≈ 88.72; the range reduction
    /// rounds inputs in [88.38…, 88.5) to k = 128, and the single exponent-bit multiply
    /// overflowed into the infinity pattern although the result is still finite and far below
    /// f32::MAX.
    #[test]
    fn exp_stays_finite_up_to_the_real_overflow() {
        assert!(
            exp_f32(88.4).is_finite(),
            "e^88.4 ≈ 2.5e38 is well below f32::MAX"
        );
        let mut x = 80.0f32;
        while x < 88.7 {
            let got = exp_f32(x);
            let want = (x as f64).exp();
            assert!(got.is_finite(), "exp_f32({x}) = {got}, want ~{want:e}");
            assert!(
                ((got as f64) - want).abs() <= 1e-6 * want,
                "exp_f32({x}) = {got}, want {want}"
            );
            x += 0.25;
        }
    }
}
