// SPDX-License-Identifier: MPL-2.0
//! Software `f32` transcendentals for outputs that must be bit-identical on every OS.
//!
//! `f32::{sin, cos, asin, acos, exp, powf, atan2}` call the platform libm, whose
//! rounding differs by an ulp or more between glibc, the Windows UCRT and Android
//! bionic. Anything whose bits are stored or replayed cross-platform — the CPU sun
//! and atmosphere LUTs (`atmosphere.rs`), the built-in glass crack stamps
//! (`glass::procedural`, `glass::mask`) — must evaluate through these instead, or a
//! save/replay diverges per OS (audit G, render5 determinism).
//!
//! [`libm`] is a pure-Rust port of musl's math library: identical inputs give
//! identical bits on every target. IEEE `sqrt` is correctly rounded everywhere
//! already and keeps the builtin.

/// Sine of `x` radians (`libm::sinf`): bit-identical on every target.
pub(crate) fn sin(x: f32) -> f32 {
    libm::sinf(x)
}

/// Cosine of `x` radians (`libm::cosf`): bit-identical on every target.
pub(crate) fn cos(x: f32) -> f32 {
    libm::cosf(x)
}

/// Arcsine of `x` (`libm::asinf`): bit-identical on every target.
pub(crate) fn asin(x: f32) -> f32 {
    libm::asinf(x)
}

/// Arccosine of `x` (`libm::acosf`): bit-identical on every target.
pub(crate) fn acos(x: f32) -> f32 {
    libm::acosf(x)
}

/// `e^x` (`libm::expf`): bit-identical on every target.
pub(crate) fn exp(x: f32) -> f32 {
    libm::expf(x)
}

/// `x^y` (`libm::powf`): bit-identical on every target.
pub(crate) fn powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

/// `atan2(y, x)` (`libm::atan2f`): bit-identical on every target.
pub(crate) fn atan2(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit G finding 3 (render5 determinism): these pins are the `libm` (musl)
    /// bits. Every one of them disagrees with the Linux reference host's platform
    /// `f32` builtins by an ulp (checked when the pins were baked), which is the
    /// bug: saved tables and replays must not carry per-OS bits. Reverting a
    /// wrapper to the platform builtin fails these pins on that host.
    #[test]
    fn transcendental_bits_match_the_software_libm() {
        assert_eq!(sin(0.103_425_03).to_bits(), 0x3dd3_6fe2);
        assert_eq!(cos(0.785_832).to_bits(), 0x3f34_f0d8);
        assert_eq!(asin(0.027_156).to_bits(), 0x3cde_7d42);
        assert_eq!(acos(0.003_999).to_bits(), 0x3fc8_8cd0);
        assert_eq!(exp(-0.011_507_999).to_bits(), 0x3f7d_1222);
        assert_eq!(atan2(0.3, 0.025_481_999).to_bits(), 0x3fbe_3734);
        assert_eq!(powf(0.001_233, 1.5).to_bits(), 0x3835_9862);
    }
}
