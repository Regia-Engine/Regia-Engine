// SPDX-License-Identifier: MPL-2.0
//! The source's sim trig, through `regia_detsim::math` (spec 121 § 4.2 seam).
//!
//! The source's `math::sin`, `math::cos` and `math::sin_cos` are libm 0.2.16's `sinf` and `cosf`, the
//! functions `regia_detsim::math::{sin, cos}` call. Its `math::atan2` is libm's `atan2f` with signed-zero
//! inputs taken as `+0` and a NaN result replaced by the canonical NaN (`0x7fc0_0000`). [`atan2`] is those
//! two steps around `regia_detsim::math::atan2_libm` (the bare `atan2f`), so the copied `atan2` call sites
//! keep the source's bits for every input (ADR-8167). It does not call `regia_detsim::math::atan2`: since
//! `MATH_BITS_VERSION` 1 that is the correctly rounded `atan2` (same zero and NaN policy, but up to 1 ulp
//! from `atan2f` elsewhere), and Stage A must keep the source's bits (KI-WD-029, ADR-8876).

use regia_detsim::float::canonical;
use regia_detsim::math;

/// The source's sim `atan2`: `atan2f(+0 for -0 inputs)`, NaN results canonical.
#[inline]
pub(crate) fn atan2(y: f32, x: f32) -> f32 {
    canonical(math::atan2_libm(canonical(y), canonical(x)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic spread of inputs: a ramp over several revolutions, large and tiny values, the
    /// axis values, infinities.
    fn inputs() -> Vec<f32> {
        let mut out: Vec<f32> = (0..2048).map(|i| i as f32 * 0.0137 - 14.0).collect();
        out.extend([
            0.0,
            1.0,
            -1.0,
            1.0e-30,
            -1.0e-30,
            1.0e30,
            -1.0e30,
            3.0e38,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ]);
        out.extend([
            core::f32::consts::PI,
            -core::f32::consts::PI,
            core::f32::consts::FRAC_PI_2,
            1.0e-40,
        ]);
        out
    }

    /// Plan row W-02, layout re-sync: `regia_detsim::math::{sin, cos}` (the source's `sin_cos`) and
    /// [`atan2`] call the libm 0.2.16 functions the source's math module calls, with its canonical steps.
    #[test]
    fn detsim_trig_is_the_source_libm() {
        let xs = inputs();
        for x in &xs {
            let x = *x;
            assert_eq!(math::sin(x).to_bits(), libm::sinf(x).to_bits(), "sin({x})");
            assert_eq!(math::cos(x).to_bits(), libm::cosf(x).to_bits(), "cos({x})");
        }
        for (i, y) in xs.iter().enumerate().step_by(7) {
            for x in xs.iter().skip(i % 5).step_by(11) {
                let (y, x) = (*y, *x);
                assert_eq!(
                    atan2(y, x).to_bits(),
                    libm::atan2f(y, x).to_bits(),
                    "atan2({y}, {x}) without signed zeros"
                );
            }
        }
        // The source's canonical steps: a -0 input is +0 and a NaN result is the canonical NaN.
        assert_eq!(atan2(-0.0, 1.0).to_bits(), 0);
        assert_eq!(atan2(-0.0, -1.0), core::f32::consts::PI);
        assert_eq!(atan2(-0.0, -0.0).to_bits(), 0);
        assert_eq!(atan2(f32::NAN, 1.0).to_bits(), 0x7fc0_0000);
        assert_eq!(atan2(1.0, -f32::NAN).to_bits(), 0x7fc0_0000);
        // Why the wrap exists: the bare libm call keeps the zero's sign.
        assert_eq!(math::atan2_libm(-0.0, -1.0), -core::f32::consts::PI);
        assert_eq!(math::atan2_libm(-0.0, 1.0).to_bits(), (-0.0f32).to_bits());
        // Why it cannot become `regia_detsim::math::atan2` (ADR-8876): that one has the same zero and NaN
        // steps but is correctly rounded, which is not libm's `atan2f` on every input (a published hard
        // case, CORE-MATH atan2f.wc: libm rounds it one ulp off).
        assert_eq!(math::atan2(-0.0, -1.0), core::f32::consts::PI);
        let (y, x) = (f32::from_bits(0x410a_e56e), f32::from_bits(0xcb06_6a82));
        assert_eq!(atan2(y, x).to_bits(), libm::atan2f(y, x).to_bits());
        assert_ne!(
            atan2(y, x).to_bits(),
            math::atan2(y, x).to_bits(),
            "libm's atan2f is CR here: pick another case"
        );
    }
}
