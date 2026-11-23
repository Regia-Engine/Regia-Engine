// SPDX-License-Identifier: MPL-2.0
//! Sight hysteresis (ADR-4586): a remembered target stays in memory until it leaves a lose cone
//! that is at least as wide as the cone that acquired it.
//!
//! A lose radius or half-angle of zero means "the same as the acquire range", so an agent that
//! never sets the new fields matches [`crate::perception::in_sight_cone`] on both paths. The cone
//! test itself is unchanged; this module only chooses which radius and half-angle to pass. The
//! "at least as wide" invariant holds on both sides of the public fields:
//! [`assign_hysteresis`] refuses to store a narrower pair, and [`effective_lose_radius`] /
//! [`effective_lose_half_angle`] clamp a stored pair something wrote directly (the fields are
//! public; `net_bots` raises `sight_radius` every decision).

use regia_core::math::Vec3;

use crate::perception::{Perception, in_sight_cone};

/// Acquire and lose sight ranges for one observer.
///
/// A lose radius or half-angle that is not finite and greater than zero is treated as the
/// matching acquire value. The validator ([`assign_hysteresis`]) refuses to store a positive lose
/// range that is narrower than the acquire range, and the read path
/// ([`effective_lose_radius`], [`effective_lose_half_angle`]) clamps a narrower stored pair up,
/// so the lose cone never ends up inside the acquire cone whatever wrote the fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SightRanges {
    /// Radius that can acquire a target which is not yet in memory (metres).
    pub acquire_radius: f32,
    /// Half-angle that can acquire a target which is not yet in memory (radians).
    pub acquire_half_angle: f32,
    /// Stored lose radius (metres). Zero selects [`Self::acquire_radius`].
    pub lose_radius: f32,
    /// Stored lose half-angle (radians). Zero selects [`Self::acquire_half_angle`].
    pub lose_half_angle: f32,
}

/// [`assign_hysteresis`] refused the pair and wrote nothing.
///
/// The agent method reports this as `perception_bad_hysteresis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HysteresisReject;

impl HysteresisReject {
    /// Agent error code for a lose cone narrower than the acquire cone, or a value that is
    /// negative or not finite.
    pub const CODE: &'static str = "perception_bad_hysteresis";
}

/// The lose radius the keep path tests.
///
/// A stored value that is not finite and greater than zero selects the acquire radius, including
/// zero, negatives and NaN. A positive value is never narrower than the acquire radius: the
/// fields are public, so a direct write (not through [`assign_hysteresis`]) can pair a lose
/// radius below the acquire one, and the lose cone must stay at least as wide as the cone that
/// acquired the target.
#[must_use]
pub fn effective_lose_radius(ranges: SightRanges) -> f32 {
    let lose = if ranges.lose_radius.is_finite() && ranges.lose_radius > 0.0 {
        ranges.lose_radius
    } else {
        ranges.acquire_radius
    };
    lose.max(ranges.acquire_radius)
}

/// The lose half-angle the keep path tests.
///
/// A stored value that is not finite and greater than zero selects the acquire half-angle. Zero
/// is that sentinel, not a ray along the facing direction. A positive value is never narrower
/// than the acquire half-angle (see [`effective_lose_radius`]).
#[must_use]
pub fn effective_lose_half_angle(ranges: SightRanges) -> f32 {
    let lose = if ranges.lose_half_angle.is_finite() && ranges.lose_half_angle > 0.0 {
        ranges.lose_half_angle
    } else {
        ranges.acquire_half_angle
    };
    lose.max(ranges.acquire_half_angle)
}

/// Whether `target` is inside the cone this observer uses for it.
///
/// A target that is not `remembered` is acquired only inside the acquire cone. A target that is
/// already in memory is kept until it leaves the lose cone. Both paths call [`in_sight_cone`].
#[must_use]
pub fn sight_holds(eye: Vec3, forward: Vec3, target: Vec3, ranges: SightRanges, remembered: bool) -> bool {
    if remembered {
        in_sight_cone(
            eye,
            forward,
            target,
            effective_lose_half_angle(ranges),
            effective_lose_radius(ranges),
        )
    } else {
        in_sight_cone(
            eye,
            forward,
            target,
            ranges.acquire_half_angle,
            ranges.acquire_radius,
        )
    }
}

/// Whether a lose pair may be stored next to these acquire ranges.
///
/// Both lose values must be finite and at least zero. A positive lose radius must be at least
/// the acquire radius, and the acquire radius must be finite. A positive lose half-angle must be
/// at least the acquire half-angle. Zero is accepted even when the acquire value is not finite,
/// because zero means "use the acquire value" rather than a second number to compare.
#[must_use]
pub fn hysteresis_accepted(
    acquire_radius: f32,
    acquire_half_angle: f32,
    lose_radius: f32,
    lose_half_angle: f32,
) -> bool {
    if !lose_radius.is_finite() || lose_radius < 0.0 {
        return false;
    }
    if !lose_half_angle.is_finite() || lose_half_angle < 0.0 {
        return false;
    }
    if lose_radius > 0.0 && !(acquire_radius.is_finite() && lose_radius >= acquire_radius) {
        return false;
    }
    if lose_half_angle > 0.0 && !(acquire_half_angle.is_finite() && lose_half_angle >= acquire_half_angle) {
        return false;
    }
    true
}

/// Writes the lose ranges, or writes neither field when the resulting pair is refused.
///
/// `None` keeps that field's current value. On [`Err`] the component is unchanged.
pub fn assign_hysteresis(
    perception: &mut Perception,
    lose_radius: Option<f32>,
    lose_half_angle: Option<f32>,
) -> Result<(), HysteresisReject> {
    let radius = lose_radius.unwrap_or(perception.sight_lose_radius);
    let half = lose_half_angle.unwrap_or(perception.sight_lose_half_angle);
    if !hysteresis_accepted(perception.sight_radius, perception.sight_half_angle, radius, half) {
        return Err(HysteresisReject);
    }
    perception.sight_lose_radius = radius;
    perception.sight_lose_half_angle = half;
    Ok(())
}

/// One sample point on a target's bounds, in the target's local XY.
///
/// `count` of 0 or 1 is the center. Otherwise the samples fill a grid whose side is the smallest
/// integer whose square is at least `count`: index 0 is the negative-X, negative-Y corner and the
/// last index of a full square is the positive corner. Z is 0. The sight ray aims at the target's
/// aim point plus this offset.
#[must_use]
pub fn sample_offset(index: u32, count: u32, half: Vec3) -> Vec3 {
    if count <= 1 || index >= count {
        return Vec3::ZERO;
    }
    let mut side = 1u32;
    while side.saturating_mul(side) < count {
        side = side.saturating_add(1);
        if side > 64 {
            break;
        }
    }
    let x = index % side;
    let y = index / side;
    let axis = |i: u32, n: u32, extent: f32| -> f32 {
        if n <= 1 {
            return 0.0;
        }
        let t = i as f32 / (n - 1) as f32;
        (t * 2.0 - 1.0) * extent
    };
    Vec3::new(axis(x, side, half.x), axis(y, side, half.y), 0.0)
}

/// How much of the cone's width is left between dead ahead and the edge.
///
/// Dead ahead is 1. The cone edge is 0. Outside the cone is 0. A missing facing, or a half-angle
/// of a half-turn or more, is 1 (the same "no cone" case as [`in_sight_cone`]). Non-finite inputs
/// are 0. The horizontal fan matches [`in_sight_cone`].
#[must_use]
pub fn cone_falloff(eye: Vec3, forward: Vec3, target: Vec3, half_angle: f32) -> f32 {
    if !(eye.is_finite() && forward.is_finite() && target.is_finite() && half_angle.is_finite()) {
        return 0.0;
    }
    let half_angle = half_angle.clamp(0.0, std::f32::consts::PI);
    let to = target - eye;
    let horiz = Vec3::new(to.x, 0.0, to.z);
    let dist = horiz.length();
    let facing = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
    if facing.length_squared() == 0.0 || half_angle >= std::f32::consts::PI {
        return 1.0;
    }
    if dist < 1e-4 {
        return 0.0;
    }
    let cos = facing.dot(horiz / dist);
    if !cos.is_finite() {
        return 0.0;
    }
    let edge = regia_core::math::det_cos(half_angle);
    let span = (1.0 - edge).max(1e-6);
    ((cos - edge) / span).clamp(0.0, 1.0)
}

/// `visible_fraction * visibility * falloff`, each factor clamped to `[0, 1]`.
///
/// A non-finite factor contributes 0.
#[must_use]
pub fn detection_gain(visible_fraction: f32, visibility: f32, falloff: f32) -> f32 {
    fn unit(value: f32) -> f32 {
        if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
    unit(visible_fraction) * unit(visibility) * unit(falloff)
}

/// Adds `gain` to `evidence` and says whether the meter has reached `threshold`.
///
/// A threshold that is not finite and greater than zero acquires on any positive gain (today's
/// instant sight). Evidence is capped at 64 so a long stare stays finite. Non-finite evidence
/// starts again from 0.
#[must_use]
pub fn meter_acquires(evidence: f32, gain: f32, threshold: f32) -> (f32, bool) {
    let gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
    if !(threshold.is_finite() && threshold > 0.0) {
        return (evidence, gain > 0.0);
    }
    let evidence = if evidence.is_finite() {
        evidence.max(0.0)
    } else {
        0.0
    };
    let next = (evidence + gain).min(64.0);
    (next, next >= threshold)
}

/// Confidence after one tick of not being sensed.
///
/// `None` and `Some(0)` leave it unchanged (drop at age, the default). Otherwise it is multiplied
/// by `0.5 ^ (1 / half_life_ticks)` from [`regia_core::math::det_powf`]. A non-finite confidence
/// becomes 0. The result stays in `[0, 1]`.
#[must_use]
pub fn confidence_after_tick(confidence: f32, half_life_ticks: Option<u32>) -> f32 {
    let Some(half) = half_life_ticks.filter(|half| *half > 0) else {
        return if confidence.is_finite() {
            confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
    };
    if !confidence.is_finite() {
        return 0.0;
    }
    let factor = regia_core::math::det_powf(0.5, 1.0 / half as f32);
    if !factor.is_finite() {
        return 0.0;
    }
    (confidence * factor).clamp(0.0, 1.0)
}

/// Steps a last known position by one tick of `velocity` when a half-life is set.
///
/// `None` and `Some(0)` leave the position where it was sensed. A non-finite velocity, or a step
/// that would not be finite, keeps the old position.
#[must_use]
pub fn age_position(position: Vec3, velocity: Vec3, half_life_ticks: Option<u32>) -> Vec3 {
    if half_life_ticks.filter(|half| *half > 0).is_none() {
        return position;
    }
    if !velocity.is_finite() {
        return position;
    }
    let next = position + velocity;
    if next.is_finite() { next } else { position }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(lose_radius: f32, lose_half_angle: f32) -> SightRanges {
        SightRanges {
            acquire_radius: 10.0,
            acquire_half_angle: std::f32::consts::FRAC_PI_2,
            lose_radius,
            lose_half_angle,
        }
    }

    fn ahead(metres: f32) -> Vec3 {
        Vec3::new(0.0, 0.0, -metres)
    }

    /// FNV-1a over the bools, so the two cones can be compared without listing each point.
    fn digest(flags: &[bool]) -> u64 {
        let mut hash = 0xcbf29ce484222325;
        for flag in flags {
            hash ^= u64::from(*flag);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    #[test]
    fn a_target_in_the_band_is_kept_only_after_it_was_acquired() {
        let eye = Vec3::ZERO;
        let forward = Vec3::NEG_Z;
        let band = ranges(14.0, std::f32::consts::FRAC_PI_2);
        assert!(
            !sight_holds(eye, forward, ahead(12.0), band, false),
            "12 m is not an acquire"
        );
        assert!(sight_holds(eye, forward, ahead(9.0), band, false));
        assert!(
            sight_holds(eye, forward, ahead(12.0), band, true),
            "12 m stays once remembered"
        );
        assert!(
            sight_holds(eye, forward, ahead(14.0), band, true),
            "the lose radius is inside"
        );
        assert!(!sight_holds(eye, forward, ahead(15.0), band, true));
        assert!(!sight_holds(eye, forward, ahead(15.0), band, false));
    }

    #[test]
    fn equal_lose_ranges_match_the_acquire_cone_on_twenty_points() {
        let eye = Vec3::ZERO;
        let forward = Vec3::NEG_Z;
        let half = std::f32::consts::FRAC_PI_2;
        let radius = 10.0;
        let points = [
            ahead(5.0),
            ahead(10.0),
            ahead(10.01),
            ahead(12.0),
            ahead(14.0),
            ahead(15.0),
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::new(8.0, 0.0, -2.0),
            Vec3::new(1.0, 0.0, -1.0),
            Vec3::new(0.0, 4.0, -3.0),
            Vec3::new(0.0, 20.0, 0.0),
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -0.00001),
            Vec3::new(-6.0, 1.0, -6.0),
            ahead(9.0),
            Vec3::new(3.0, 0.0, -9.0),
            Vec3::new(9.0, 0.0, -1.0),
            Vec3::new(0.0, -2.0, -4.0),
            Vec3::new(100.0, 0.0, 0.0),
            Vec3::new(f32::NAN, 0.0, -1.0),
        ];
        assert_eq!(points.len(), 20);
        let old: Vec<bool> = points
            .iter()
            .map(|target| in_sight_cone(eye, forward, *target, half, radius))
            .collect();
        let equal = SightRanges {
            acquire_radius: radius,
            acquire_half_angle: half,
            lose_radius: radius,
            lose_half_angle: half,
        };
        let sentinel = SightRanges {
            acquire_radius: radius,
            acquire_half_angle: half,
            lose_radius: 0.0,
            lose_half_angle: 0.0,
        };
        for (ranges, remembered) in [(equal, false), (equal, true), (sentinel, false), (sentinel, true)] {
            let got: Vec<bool> = points
                .iter()
                .map(|target| sight_holds(eye, forward, *target, ranges, remembered))
                .collect();
            assert_eq!(
                digest(&got),
                digest(&old),
                "remembered {remembered}, ranges {ranges:?}"
            );
            assert_eq!(got, old);
        }
        assert!(old.iter().any(|hit| *hit));
        assert!(old.iter().any(|hit| !*hit));
    }

    #[test]
    fn the_lose_half_angle_is_the_keep_cone_and_zero_is_not_a_ray() {
        let eye = Vec3::ZERO;
        let forward = Vec3::NEG_Z;
        let target = Vec3::new(1.0, 0.0, -1.0);
        let wide = ranges(10.0, 1.2);
        let mut narrow = wide;
        narrow.acquire_half_angle = 0.3;
        narrow.lose_half_angle = 1.2;
        assert_eq!(
            sight_holds(eye, forward, target, narrow, false),
            in_sight_cone(eye, forward, target, 0.3, 10.0)
        );
        assert_eq!(
            sight_holds(eye, forward, target, narrow, true),
            in_sight_cone(eye, forward, target, 1.2, 10.0)
        );
        let mut sentinel = narrow;
        sentinel.lose_half_angle = 0.0;
        assert_eq!(
            sight_holds(eye, forward, target, sentinel, true),
            in_sight_cone(eye, forward, target, sentinel.acquire_half_angle, 10.0)
        );
    }

    #[test]
    fn a_lose_radius_below_the_acquire_radius_is_perception_bad_hysteresis() {
        assert_eq!(HysteresisReject::CODE, "perception_bad_hysteresis");
        let mut perception = Perception {
            sight_radius: 10.0,
            sight_half_angle: 1.0,
            sight_lose_radius: 12.0,
            sight_lose_half_angle: 1.2,
            ..Perception::default()
        };
        let before = perception.clone();
        assert!(assign_hysteresis(&mut perception, Some(9.0), None).is_err());
        assert_eq!(perception, before, "a narrow lose radius writes nothing");
        assert!(assign_hysteresis(&mut perception, None, Some(0.5)).is_err());
        assert_eq!(perception, before, "a narrow lose half-angle writes nothing");
        assert!(assign_hysteresis(&mut perception, Some(14.0), None).is_ok());
        assert_eq!(perception.sight_lose_radius, 14.0);
        assert_eq!(perception.sight_lose_half_angle, 1.2);

        assert!(hysteresis_accepted(10.0, 1.0, 10.0, 1.0));
        assert!(hysteresis_accepted(10.0, 1.0, 0.0, 0.0));
        assert!(hysteresis_accepted(f32::NAN, f32::NAN, 0.0, 0.0));
        assert!(!hysteresis_accepted(f32::NAN, 1.0, 10.0, 0.0));
        assert!(!hysteresis_accepted(10.0, f32::NAN, 0.0, 1.0));
        assert!(!hysteresis_accepted(10.0, 1.0, -1.0, 0.0));
        assert!(!hysteresis_accepted(10.0, 1.0, 0.0, -0.2));
        assert!(!hysteresis_accepted(10.0, 1.0, f32::NAN, 0.0));
        assert!(!hysteresis_accepted(10.0, 1.0, f32::INFINITY, 0.0));
        assert!(!hysteresis_accepted(10.0, 1.0, 0.0, f32::INFINITY));
        assert!(hysteresis_accepted(10.0, 1.0, -0.0, -0.0));
    }

    /// corr-low finding 15: the fields are public, so `assign_hysteresis` is not the only writer
    /// (`net_bots` raises `sight_radius` every decision). A stored lose range something wrote
    /// below the acquire range must still keep a remembered target: the lose cone is at least as
    /// wide as the cone that acquired it, however the pair came to be.
    #[test]
    fn a_stored_lose_range_below_the_acquire_keeps_the_target() {
        let eye = Vec3::ZERO;
        let forward = Vec3::NEG_Z;
        let mut band = ranges(12.0, 0.5);
        band.acquire_radius = 15.0;
        band.acquire_half_angle = 1.0;
        let target = ahead(13.0);
        assert!(
            sight_holds(eye, forward, target, band, false),
            "13 m is an acquire inside the 15 m cone"
        );
        assert!(
            sight_holds(eye, forward, target, band, true),
            "13 m is inside the acquire cone: a remembered target must not drop at the 12 m lose radius"
        );
        let off_axis = Vec3::new(9.0 * 0.75f32.sin(), 0.0, -(9.0 * 0.75f32.cos()));
        let mut angle = ranges(10.0, 0.5);
        angle.acquire_half_angle = 1.0;
        assert!(
            sight_holds(eye, forward, off_axis, angle, true),
            "0.75 rad is inside the acquire half-angle: a remembered target must not drop at the 0.5 rad lose half-angle"
        );
        assert!(
            sight_holds(eye, forward, off_axis, angle, false),
            "0.75 rad is an acquire inside the 1.0 rad half-angle"
        );
    }

    #[test]
    fn four_samples_are_the_corners_and_the_gain_is_the_product() {
        let half = Vec3::new(1.0, 2.0, 9.0);
        assert_eq!(sample_offset(0, 1, half), Vec3::ZERO);
        assert_eq!(sample_offset(0, 4, half), Vec3::new(-1.0, -2.0, 0.0));
        assert_eq!(sample_offset(1, 4, half), Vec3::new(1.0, -2.0, 0.0));
        assert_eq!(sample_offset(2, 4, half), Vec3::new(-1.0, 2.0, 0.0));
        assert_eq!(sample_offset(3, 4, half), Vec3::new(1.0, 2.0, 0.0));
        assert!((detection_gain(0.5, 0.3, 1.0) - 0.15).abs() < 1e-6);
        assert_eq!(detection_gain(f32::NAN, 1.0, 1.0), 0.0);
        let (evidence, acquired) = meter_acquires(0.6, 0.5, 1.0);
        assert!((evidence - 1.1).abs() < 1e-6 && acquired);
        assert!(!meter_acquires(0.3, 0.3, 1.0).1);
        assert_eq!(confidence_after_tick(1.0, None), 1.0);
        assert!((confidence_after_tick(1.0, Some(1)) - 0.5).abs() < 1e-4);
        assert_eq!(age_position(Vec3::Z, Vec3::X, None), Vec3::Z);
        assert_eq!(age_position(Vec3::ZERO, Vec3::X, Some(1)), Vec3::X);
        let eye = Vec3::ZERO;
        assert!(
            (cone_falloff(eye, Vec3::NEG_Z, Vec3::new(0.0, 0.0, -5.0), 45f32.to_radians()) - 1.0).abs()
                < 1e-4
        );
    }
}
