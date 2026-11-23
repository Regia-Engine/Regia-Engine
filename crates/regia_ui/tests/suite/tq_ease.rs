// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): easing curves at their boundaries and on hostile progress values.
//! Pure functions; no goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_test_utils::fuzz::FuzzRng;
use regia_ui::tween::Ease;

#[test]
fn every_named_curve_starts_at_zero_and_ends_at_one() {
    for (name, ease) in Ease::NAMED {
        assert!(ease.apply(0.0).abs() < 1e-4, "{name} at 0: {}", ease.apply(0.0));
        assert!(
            (ease.apply(1.0) - 1.0).abs() < 1e-4,
            "{name} at 1: {}",
            ease.apply(1.0)
        );
    }
}

#[test]
fn progress_outside_the_range_clamps_and_nan_is_the_start() {
    for (name, ease) in Ease::NAMED {
        let (start, end) = (ease.apply(0.0), ease.apply(1.0));
        assert_eq!(ease.apply(-5.0), start, "{name}");
        assert_eq!(ease.apply(f32::NEG_INFINITY), start, "{name}");
        assert_eq!(ease.apply(7.0), end, "{name}");
        assert_eq!(
            ease.apply(f32::INFINITY),
            end,
            "{name}: infinite progress is finished"
        );
        assert_eq!(ease.apply(f32::NAN), start, "{name}");
    }
}

#[test]
fn every_curve_is_finite_and_repeatable_across_a_fixed_sweep() {
    for (name, ease) in Ease::NAMED {
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let v = ease.apply(t);
            assert!(v.is_finite() && (-1.0..=2.0).contains(&v), "{name}({t}) = {v}");
            assert_eq!(v, ease.apply(t), "{name}({t}) is not repeatable");
        }
    }
}

#[test]
fn plain_in_out_curves_never_go_backwards() {
    for name in [
        "linear",
        "quad_in",
        "quad_out",
        "quad_in_out",
        "cubic_in",
        "cubic_out",
        "cubic_in_out",
        "expo_in",
        "expo_out",
        "expo_in_out",
    ] {
        let ease = Ease::parse(name).expect("named curve");
        let mut last = ease.apply(0.0);
        for i in 1..=1000 {
            let v = ease.apply(i as f32 / 1000.0);
            assert!(v >= last - 1e-6, "{name} steps back at {i}: {last} -> {v}");
            last = v;
        }
    }
}

#[test]
fn overshoot_curves_overshoot_and_the_others_do_not() {
    let peak = |ease: Ease, lo: f32| {
        (0..=1000)
            .map(|i| ease.apply(i as f32 / 1000.0))
            .fold(lo, f32::max)
    };
    assert!(peak(Ease::BackOut, 0.0) > 1.0);
    assert!(peak(Ease::ElasticOut, 0.0) > 1.0);
    for ease in [Ease::QuadOut, Ease::CubicInOut, Ease::ExpoOut, Ease::BounceOut] {
        assert!(peak(ease, 0.0) <= 1.0 + 1e-5, "{}", ease.name());
    }
}

#[test]
fn names_parse_back_and_unknown_names_are_none() {
    for (name, ease) in Ease::NAMED {
        assert_eq!(Ease::parse(name), Some(ease));
        assert_eq!(ease.name(), name);
    }
    for bad in ["", "Linear", "quad", "quad_in ", "cubic_bezier"] {
        assert_eq!(Ease::parse(bad), None, "{bad:?}");
    }
    assert_eq!(Ease::CubicBezier([0.1, 0.2, 0.3, 0.4]).name(), "cubic_bezier");
}

#[test]
fn cubic_bezier_hits_its_ends_and_survives_hostile_control_points() {
    let hostile = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 0.0, 1.0, 4.0];
    for x1 in hostile {
        for y1 in hostile {
            for x2 in hostile {
                for y2 in hostile {
                    let ease = Ease::CubicBezier([x1, y1, x2, y2]);
                    for t in [0.0, 0.25, 0.5, 1.0, f32::NAN] {
                        let v = ease.apply(t);
                        assert!(v.is_finite(), "bezier({x1}, {y1}, {x2}, {y2})({t}) = {v}");
                    }
                }
            }
        }
    }
    // The CSS "ease" curve: fast start, ends at 1; a straight line through (0.25,0.25), (0.75,0.75) is linear.
    let linear = Ease::CubicBezier([0.25, 0.25, 0.75, 0.75]);
    let mut rng = FuzzRng::new(5);
    for _ in 0..200 {
        let t = rng.below(1001) as f32 / 1000.0;
        assert!((linear.apply(t) - t).abs() < 1e-3, "{t}");
    }
    // x control points outside 0..=1 are clamped, so they equal their clamped twin.
    assert_eq!(
        Ease::CubicBezier([-4.0, 0.3, 9.0, 0.9]).apply(0.4),
        Ease::CubicBezier([0.0, 0.3, 1.0, 0.9]).apply(0.4)
    );
}
