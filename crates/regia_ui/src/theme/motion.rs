// SPDX-License-Identifier: MPL-2.0
//! Motion tokens: easing sampling and the reduced-motion rule (`docs/specs/ui-default-design.md`
//! § 2.6; lane D8, ADR-371).
//!
//! A theme's `motion.*` tokens are durations and its `ease.*` tokens are easing curves
//! ([`Easing`]); a widget's `transition` property names one of each. This module turns the curve
//! into a progress value ([`sample`]) and applies the reduced-motion rule ([`reduced_duration`]).
//!
//! The cubic Bézier solver is the standard one for CSS `cubic-bezier()` timing functions (W3C CSS
//! Easing Functions Level 1, public): the curve's x coordinate is inverted with a few Newton
//! steps, falling back to bisection where the slope is too flat for Newton.

use crate::theme_tokens::Easing;

/// What a duration drives, for the reduced-motion rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// Movement, scale, colour or sweeping loops: becomes instant under reduced motion.
    Transform,
    /// An opacity fade: capped at `motion.fast` under reduced motion instead of removed.
    Fade,
}

/// Cubic Bézier coordinate at parameter `s` for control values `p1`, `p2` (end points 0 and 1).
fn bezier(p1: f32, p2: f32, s: f32) -> f32 {
    let u = 1.0 - s;
    3.0 * u * u * s * p1 + 3.0 * u * s * s * p2 + s * s * s
}

/// Derivative of [`bezier`] in `s`.
fn bezier_slope(p1: f32, p2: f32, s: f32) -> f32 {
    let u = 1.0 - s;
    3.0 * u * u * p1 + 6.0 * u * s * (p2 - p1) + 3.0 * s * s * (1.0 - p2)
}

/// Progress (0 to 1, may overshoot when the y control points leave 0..1) of `easing` at linear
/// time `t`; `t` is clamped to 0..1.
pub fn sample(easing: Easing, t: f32) -> f32 {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 1.0 };
    match easing {
        Easing::Linear => t,
        Easing::CubicBezier([x1, y1, x2, y2]) => {
            if t <= 0.0 || t >= 1.0 {
                return t;
            }
            let mut s = t;
            let mut solved = false;
            for _ in 0..8 {
                let err = bezier(x1, x2, s) - t;
                if err.abs() < 1e-6 {
                    solved = true;
                    break;
                }
                let slope = bezier_slope(x1, x2, s);
                if slope.abs() < 1e-6 {
                    break;
                }
                s = (s - err / slope).clamp(0.0, 1.0);
            }
            if !solved {
                let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
                s = t;
                for _ in 0..32 {
                    if bezier(x1, x2, s) < t {
                        lo = s;
                    } else {
                        hi = s;
                    }
                    s = 0.5 * (lo + hi);
                }
            }
            bezier(y1, y2, s)
        }
    }
}

/// Like [`sample`] for the optional control points a resolved style carries (`None` = linear).
pub fn sample_points(points: Option<[f32; 4]>, t: f32) -> f32 {
    match points {
        Some(p) => sample(Easing::CubicBezier(p), t),
        None => sample(Easing::Linear, t),
    }
}

/// Control points of `easing` as stored on a resolved style (`None` = linear).
pub fn points_of(easing: Easing) -> Option<[f32; 4]> {
    match easing {
        Easing::Linear => None,
        Easing::CubicBezier(p) => Some(p),
    }
}

/// The reduced-motion rule: with `reduced` set every duration becomes `motion.instant` (0),
/// except a [`MotionKind::Fade`], which caps at `fast` (the theme's `motion.fast`).
pub fn reduced_duration(duration: f32, kind: MotionKind, reduced: bool, fast: f32) -> f32 {
    if !reduced {
        duration
    } else {
        match kind {
            MotionKind::Transform => 0.0,
            MotionKind::Fade => duration.min(fast),
        }
    }
}

/// Duration of a leaving transition: `ease.exit` runs at 0.75 of the entering duration.
pub fn exit_duration(duration: f32) -> f32 {
    duration * 0.75
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: Easing = Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]);

    #[test]
    fn linear_is_identity_and_clamped() {
        assert_eq!(sample(Easing::Linear, 0.25), 0.25);
        assert_eq!(sample(Easing::Linear, -1.0), 0.0);
        assert_eq!(sample(Easing::Linear, 7.0), 1.0);
    }

    #[test]
    fn bezier_end_points_are_exact() {
        assert_eq!(sample(STANDARD, 0.0), 0.0);
        assert_eq!(sample(STANDARD, 1.0), 1.0);
    }

    #[test]
    fn symmetric_curve_passes_through_the_middle() {
        // cubic-bezier(0.25, 0.1, 0.75, 0.9) is point-symmetric about (0.5, 0.5).
        let e = Easing::CubicBezier([0.25, 0.1, 0.75, 0.9]);
        assert!((sample(e, 0.5) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn diagonal_control_points_match_linear() {
        let e = Easing::CubicBezier([1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]);
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((sample(e, t) - t).abs() < 1e-4, "t = {t}");
        }
    }

    #[test]
    fn standard_ease_is_front_loaded_and_monotonic() {
        let mut prev = 0.0;
        for i in 1..=20 {
            let v = sample(STANDARD, i as f32 / 20.0);
            assert!(v >= prev, "monotonic at step {i}");
            prev = v;
        }
        assert!(sample(STANDARD, 0.25) > 0.25, "ease-out style curve leads linear");
        assert!(sample(STANDARD, 0.5) > 0.8);
    }

    #[test]
    fn exit_curve_is_back_loaded() {
        let exit = Easing::CubicBezier([0.3, 0.0, 1.0, 1.0]);
        assert!(sample(exit, 0.25) < 0.25);
    }

    #[test]
    fn sampled_values_of_a_known_curve() {
        // Compare against an independent bisection inversion of x(s) = t.
        let t = 0.2_f32;
        let s = {
            // invert x(s) = t by bisection in the test, independent of the solver under test
            let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
            for _ in 0..60 {
                let mid = 0.5 * (lo + hi);
                if bezier(0.2, 0.0, mid) < t {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            0.5 * (lo + hi)
        };
        assert!((sample(STANDARD, t) - bezier(0.0, 1.0, s)).abs() < 1e-4);
    }

    #[test]
    fn non_finite_time_ends_the_animation() {
        assert_eq!(sample(STANDARD, f32::NAN), 1.0);
    }

    #[test]
    fn points_round_trip() {
        assert_eq!(points_of(Easing::Linear), None);
        assert_eq!(points_of(STANDARD), Some([0.2, 0.0, 0.0, 1.0]));
        assert_eq!(sample_points(None, 0.4), 0.4);
        assert_eq!(
            sample_points(Some([0.2, 0.0, 0.0, 1.0]), 0.4),
            sample(STANDARD, 0.4)
        );
    }

    #[test]
    fn reduced_motion_jumps_except_fades() {
        assert_eq!(reduced_duration(0.24, MotionKind::Transform, true, 0.08), 0.0);
        assert_eq!(reduced_duration(0.24, MotionKind::Fade, true, 0.08), 0.08);
        assert_eq!(reduced_duration(0.05, MotionKind::Fade, true, 0.08), 0.05);
        assert_eq!(reduced_duration(0.24, MotionKind::Transform, false, 0.08), 0.24);
    }

    #[test]
    fn exit_runs_at_three_quarters() {
        assert!((exit_duration(0.24) - 0.18).abs() < 1e-6);
    }
}
