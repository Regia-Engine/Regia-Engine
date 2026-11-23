// SPDX-License-Identifier: MPL-2.0
//! The rail camera (`docs/specs/35-camera-framework.md` § B5 "Rail and crane"; ADR-3231): a
//! camera that rides a [`Spline`](regia_scene::spline::Spline) entity at a distance in meters along the
//! curve.
//!
//! The distance is *arc length*, so a drive that advances by `v · dt` moves the camera at exactly
//! `v` meters per second whatever the curve does ([`regia_scene::spline::SplineCurve::at_distance`] does the
//! parameterization; `docs/provenance.md` "Rail camera"). This file holds the data types of the
//! [`Node::Rail`](super::asset::Node::Rail) node and the pure functions it uses; the node itself
//! is evaluated in [`super::nodes`].
//!
//! # Drives
//!
//! - [`RailDrive::Value`]: the distance is a node value (a literal, a parameter or a runtime
//!   variable such as `rail_position`, which a sequence track can key, see
//!   [`super::rail_keys`]).
//! - [`RailDrive::Time`]: `start + speed · elapsed`, stateless.
//! - [`RailDrive::Follow`]: the distance of the target's closest point on the rail plus a `lead`,
//!   eased toward with an exponential rate (`1 − exp(−rate · dt)`, which never overshoots).
//!
//! # Ends
//!
//! An open spline clamps the distance to `0..=length`; a closed spline wraps it (the follow ease
//! takes the shorter way round).

use regia_core::math::Vec3;
use regia_scene::spline::SplineCurve;
use serde::{Deserialize, Serialize};

use super::asset::{TargetRef, Value};

/// A spline shorter than this (meters) is not a rail: the node is skipped.
pub const MIN_RAIL_LENGTH_M: f32 = 1.0e-3;

/// How the rail distance is produced (see the module docs).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RailDrive {
    /// The distance in meters along the rail.
    Value(Value<f32>),
    /// `start + speed · elapsed`.
    Time {
        /// Meters per second (negative rides backwards).
        speed: f32,
        /// Distance at elapsed 0, meters.
        #[serde(default)]
        start: f32,
    },
    /// Follow a target's closest point on the rail.
    Follow {
        /// The followed target.
        target: TargetRef,
        /// Meters ahead of the closest point (along increasing distance; negative trails).
        #[serde(default)]
        lead: f32,
        /// Ease rate in 1/s; 0 = the distance snaps to the goal.
        #[serde(default)]
        damping: f32,
    },
}

/// How the rail camera is oriented.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum RailOrient {
    /// Along the spline frame: forward is the tangent, up the frame's up vector.
    #[default]
    Frame,
    /// Toward a target (the rail frame is used while the target cannot be resolved).
    LookAt(TargetRef),
    /// The rotation is left as the earlier nodes set it.
    Free,
}

/// What a [`super::asset::Node::Rail`] reported on its last evaluation (the agent's
/// `camera.rail_state`).
#[derive(Clone, Debug, PartialEq)]
pub struct RailReport {
    /// Index of the node in evaluation order.
    pub node: usize,
    /// The spline entity's name.
    pub spline: String,
    /// Distance along the rail in meters, after clamping or wrapping.
    pub distance_m: f32,
    /// Length of the rail in meters.
    pub length_m: f32,
    /// The rail is a closed loop.
    pub closed: bool,
    /// The requested distance lay outside an open rail and was clamped.
    pub clamped: bool,
    /// The camera is at the far end of an open rail.
    pub at_end: bool,
    /// The camera's position on the rail (before the node's offset).
    pub point: Vec3,
    /// The rail's unit tangent there.
    pub tangent: Vec3,
    /// The follow goal distance in meters, when the drive is `Follow`.
    pub goal_m: Option<f32>,
}

/// Maps a distance onto a rail of `length` meters: wrapped for a closed rail, clamped for an open
/// one. Returns the distance and whether it was clamped at an end (open rails only). A non-finite
/// input maps to 0.
pub fn normalize_distance(length: f32, closed: bool, s: f32) -> (f32, bool) {
    if !s.is_finite() || !length.is_finite() || length <= 0.0 {
        return (0.0, false);
    }
    if closed {
        let wrapped = s.rem_euclid(length);
        // rem_euclid can return `length` itself for tiny negative inputs in f32.
        (if wrapped >= length { 0.0 } else { wrapped }, false)
    } else {
        let clamped = s.clamp(0.0, length);
        (clamped, clamped != s)
    }
}

/// The follow goal: where the target's closest point on the rail is, plus `lead` meters.
/// A non-finite target is `NaN`, so a caller can keep the distance it already stored.
pub fn follow_goal(curve: &SplineCurve, target: Vec3, lead: f32) -> f32 {
    if !target.is_finite() {
        return f32::NAN;
    }
    let along = curve.closest(target).along_m;
    let lead = if lead.is_finite() { lead } else { 0.0 };
    normalize_distance(curve.length(), curve.is_closed(), along + lead).0
}

/// Eases `current` toward `goal` along a rail of `length` meters. `rate` is 1/s (0 = snap). The
/// step is `1 − exp(−rate · dt)` of the remaining distance, so it never passes the goal; on a
/// closed rail the shorter way round is taken. The result stays on the rail.
pub fn damp_distance(current: f32, goal: f32, rate: f32, dt: f32, length: f32, closed: bool) -> f32 {
    let mut delta = goal - current;
    if closed && length > 0.0 {
        delta = (delta + 0.5 * length).rem_euclid(length) - 0.5 * length;
    }
    let factor = if rate <= 0.0 || !rate.is_finite() {
        1.0
    } else {
        1.0 - (-rate * dt.max(0.0)).exp()
    };
    normalize_distance(length, closed, current + delta * factor).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::spline::Spline;

    fn arc() -> SplineCurve {
        // A quarter-ish bend: clearly curved, so parameter speed is not arc speed.
        let spline = Spline::through([
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(14.0, 0.0, -8.0),
            Vec3::new(14.0, 0.0, -30.0),
        ]);
        SplineCurve::new(&spline, None)
    }

    #[test]
    fn constant_speed_gives_equal_arc_steps() {
        let curve = arc();
        let dt = 1.0 / 60.0;
        let speed = 3.0;
        let mut previous = curve.at_distance(0.0).position;
        let mut s = 0.0;
        let mut worst = 0.0f32;
        while s + speed * dt < curve.length() {
            s += speed * dt;
            let p = curve.at_distance(s).position;
            // Chord length vs arc length: chords shorten with curvature, so the tolerance is
            // the sagitta bound, a fraction of a percent at this step size.
            let step = p.distance(previous);
            worst = worst.max((step - speed * dt).abs());
            previous = p;
        }
        assert!(worst < 2.0e-3, "chord/arc step error {worst} m");
    }

    #[test]
    fn open_rails_clamp_and_closed_rails_wrap() {
        assert_eq!(normalize_distance(10.0, false, 12.0), (10.0, true));
        assert_eq!(normalize_distance(10.0, false, -1.0), (0.0, true));
        assert_eq!(normalize_distance(10.0, false, 4.0), (4.0, false));
        let (wrapped, clamped) = normalize_distance(10.0, true, 12.5);
        assert!((wrapped - 2.5).abs() < 1.0e-5 && !clamped);
        let (wrapped, _) = normalize_distance(10.0, true, -0.5);
        assert!((wrapped - 9.5).abs() < 1.0e-5);
        assert_eq!(normalize_distance(10.0, true, f32::NAN), (0.0, false));
    }

    #[test]
    fn follow_goal_is_the_closest_point_plus_the_lead() {
        let curve = arc();
        let probe = curve.at_distance(8.0).position + Vec3::new(0.0, 5.0, 0.0);
        let goal = follow_goal(&curve, probe, 3.0);
        assert!((goal - 11.0).abs() < 0.05, "{goal}");
        // The lead runs off the end: clamped on an open rail.
        let end = curve.at_distance(curve.length()).position;
        assert!((follow_goal(&curve, end, 5.0) - curve.length()).abs() < 1.0e-3);
    }

    #[test]
    fn damping_converges_without_overshooting_the_goal_or_the_end() {
        let length = 20.0;
        let mut s = 0.0;
        let goal = 20.0;
        let mut last = s;
        for _ in 0..600 {
            s = damp_distance(s, goal, 4.0, 1.0 / 60.0, length, false);
            assert!(s >= last && s <= goal, "{s} passes the goal");
            last = s;
        }
        assert!((goal - s).abs() < 1.0e-3);
        // Rate 0 snaps.
        assert_eq!(damp_distance(0.0, 7.0, 0.0, 0.01, length, false), 7.0);
    }

    #[test]
    fn damping_on_a_closed_rail_takes_the_short_way() {
        let next = damp_distance(19.0, 1.0, 0.0, 0.01, 20.0, true);
        assert!((next - 1.0).abs() < 1.0e-4);
        let half = damp_distance(19.0, 1.0, 1000.0, 1.0, 20.0, true);
        assert!((half - 1.0).abs() < 1.0e-3);
        let part = damp_distance(19.0, 1.0, 2.0, 0.1, 20.0, true);
        assert!(
            part > 19.0 || part < 0.5,
            "moved forward through the seam: {part}"
        );
    }

    #[test]
    fn a_closed_spline_curve_reports_closed() {
        let spline = Spline::through([
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(5.0, 0.0, 0.0),
            Vec3::new(5.0, 0.0, 5.0),
        ])
        .closed();
        assert!(SplineCurve::new(&spline, None).is_closed());
    }

    #[test]
    fn a_non_finite_follow_target_is_not_a_rail_distance() {
        let goal = follow_goal(&arc(), Vec3::NAN, 5.0);
        assert!(
            !goal.is_finite(),
            "a NaN target must not become a distance along the rail: {goal}"
        );
    }
}
