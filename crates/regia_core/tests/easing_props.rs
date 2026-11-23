// SPDX-License-Identifier: MPL-2.0
//! Whole-curve properties of the easing set on a fixed grid: continuity (no jump at the joins of the
//! piecewise definitions), monotonicity of the non-overshooting families, bounded overshoot, and the
//! serialized names.

use regia_core::math::Easing;

const STEPS: usize = 4096;

fn grid() -> impl Iterator<Item = f32> {
    (0..=STEPS).map(|i| i as f32 / STEPS as f32)
}

fn overshoots(e: Easing) -> bool {
    matches!(
        e,
        Easing::BackIn
            | Easing::BackOut
            | Easing::BackInOut
            | Easing::ElasticIn
            | Easing::ElasticOut
            | Easing::ElasticInOut
    )
}

#[test]
fn every_easing_is_continuous_across_its_joins() {
    for &e in Easing::ALL {
        let mut prev = e.apply(0.0);
        for t in grid().skip(1) {
            let v = e.apply(t);
            assert!(v.is_finite(), "{e:?}({t}) = {v}");
            // The steepest curve (elastic / expo / circ at the end) moves well under 0.15 per 1/4096.
            assert!(
                (v - prev).abs() < 0.15,
                "{e:?} jumps from {prev} to {v} at t = {t}"
            );
            prev = v;
        }
    }
}

#[test]
fn smooth_easings_are_monotonic_and_stay_in_the_unit_interval() {
    for &e in Easing::ALL {
        if overshoots(e) || matches!(e, Easing::BounceIn | Easing::BounceOut | Easing::BounceInOut) {
            continue;
        }
        let mut prev = e.apply(0.0);
        for t in grid().skip(1) {
            let v = e.apply(t);
            assert!(v >= prev - 1e-6, "{e:?} goes backwards at t = {t}: {prev} -> {v}");
            assert!((-1e-6..=1.0 + 1e-6).contains(&v), "{e:?}({t}) = {v}");
            prev = v;
        }
    }
}

#[test]
fn overshoot_is_bounded() {
    for &e in Easing::ALL {
        for t in grid() {
            let v = e.apply(t);
            // Back overshoots about 10 %, elastic at most about 1.4x the span at its first peak.
            assert!((-0.5..=1.5).contains(&v), "{e:?}({t}) = {v}");
        }
    }
}

#[test]
fn bounce_never_leaves_the_unit_interval_and_touches_the_ends_between() {
    for e in [Easing::BounceIn, Easing::BounceOut, Easing::BounceInOut] {
        for t in grid() {
            let v = e.apply(t);
            assert!((-1e-6..=1.0 + 1e-6).contains(&v), "{e:?}({t}) = {v}");
        }
    }
    // bounce_out lands on 1.0 at the end of each of its arcs.
    for landing in [1.0f32 / 2.75, 2.0 / 2.75, 2.5 / 2.75] {
        assert!((Easing::BounceOut.apply(landing) - 1.0).abs() < 1.0 / 16.0 + 1e-3);
    }
}

#[test]
fn apply_is_deterministic_and_clamps_infinities_and_nan_never_becomes_infinite() {
    for &e in Easing::ALL {
        assert_eq!(e.apply(0.3).to_bits(), e.apply(0.3).to_bits());
        assert_eq!(e.apply(f32::INFINITY), e.apply(1.0), "{e:?}");
        assert_eq!(e.apply(f32::NEG_INFINITY), e.apply(0.0), "{e:?}");
        // NaN is not clamped away. Most curves return NaN; the circ pair's `.max(0.0)` turns it into a
        // finite value. Either is acceptable; an infinity is not.
        let nan = e.apply(f32::NAN);
        assert!(nan.is_nan() || nan.is_finite(), "{e:?}(NaN) = {nan}");
    }
}

/// The transcendental easing arms must evaluate on the deterministic f64 kernel (rounded once
/// to f32), not the platform libm: `Tween` writes `apply`'s result into reflected fields and a
/// replay feeding the same `t` must store the same bits on every target (audit-g-core-determinism
/// § 2). Each case is written once with a literal (foldable by LLVM) and once through
/// `black_box` (never folded), the pattern of `tests/float_folding.rs`.
///
/// The expected bits are the kernel's own output, pinned here. The two `libm_divergence` cases
/// are inputs where the build host's glibc returned a different bit than the correctly rounded
/// kernel (found by scanning a million-point grid), so this test fails on the pre-fix code that
/// called `sinf`/`powf` directly.
#[test]
fn transcendental_and_polynomial_arms_are_pinned_to_deterministic_bits() {
    use std::hint::black_box;

    // Audit pins.
    assert_eq!(Easing::SineOut.apply(0.5).to_bits(), 0x3f35_04f3);
    assert_eq!(Easing::SineOut.apply(black_box(0.5)).to_bits(), 0x3f35_04f3);
    assert_eq!(Easing::ExpoIn.apply(0.25).to_bits(), 0x3bb5_04f3);
    assert_eq!(Easing::ExpoIn.apply(black_box(0.25)).to_bits(), 0x3bb5_04f3);
    assert_eq!(Easing::ElasticIn.apply(0.25).to_bits(), 0xbbb5_04f3);
    assert_eq!(Easing::ElasticIn.apply(black_box(0.25)).to_bits(), 0xbbb5_04f3);
    assert_eq!(Easing::CubicIn.apply(0.3).to_bits(), 0x3cdd_2f1c);
    assert_eq!(Easing::CubicIn.apply(black_box(0.3)).to_bits(), 0x3cdd_2f1c);

    // Inputs where the host libm demonstrably disagreed with the deterministic kernel.
    let t_sin = 13060f32 / 1_000_000.0;
    assert_eq!(
        Easing::SineOut.apply(t_sin).to_bits(),
        0x3ca8_0b37,
        "sine_out libm divergence"
    );
    assert_eq!(
        Easing::SineOut.apply(black_box(t_sin)).to_bits(),
        0x3ca8_0b37,
        "sine_out libm divergence (black_box)"
    );
    let t_expo = 1581f32 / 1_000_000.0;
    assert_eq!(
        Easing::ExpoIn.apply(t_expo).to_bits(),
        0x3a81_6912,
        "expo_in libm divergence"
    );
    assert_eq!(
        Easing::ExpoIn.apply(black_box(t_expo)).to_bits(),
        0x3a81_6912,
        "expo_in libm divergence (black_box)"
    );
}

#[test]
fn easing_names_serialize_as_the_variant_name_and_round_trip() {
    for &e in Easing::ALL {
        let json = serde_json::to_string(&e).expect("an easing serializes");
        assert_eq!(json, format!("\"{e:?}\""));
        let back: Easing = serde_json::from_str(&json).expect("and parses back");
        assert_eq!(back, e);
    }
    assert!(serde_json::from_str::<Easing>("\"Wobble\"").is_err());
    assert_eq!(Easing::default(), Easing::Linear);
}
