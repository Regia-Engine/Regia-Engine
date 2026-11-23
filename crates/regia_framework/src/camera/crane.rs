// SPDX-License-Identifier: MPL-2.0
//! The crane camera (`docs/specs/35-camera-framework.md` § B5 "Rail and crane"; ADR-3231): a
//! camera at the end of an arm that swings and tilts about a base.
//!
//! # Geometry
//!
//! The arm starts at the base point and has a `yaw` (about +Y, the camera convention: 0 points
//! along −Z, positive turns toward −X), a `pitch` (positive up) and a `length`. The camera sits at
//! the arm's tip. The *mount* is the camera head: its `mount_yaw` and `mount_pitch` are added to the
//! arm's, so `0, 0` looks along the arm away from the base and a mount yaw of 180° looks back at
//! the base. When the base rides a rail (see [`super::rail`]) the arm yaw can be relative to the
//! rail's heading, so the crane turns with a curving track.
//!
//! # Limits
//!
//! Each of the five axes has an optional `[min, max]` clamp ([`CraneLimits`]). Pitch is always held
//! inside ±89.9° and the length is never negative. The arm is a line only: it does not collide
//! with anything (a crane that must not enter geometry needs its limits authored for the set).

use regia_core::math::{EulerRot, Quat, Vec3};
use serde::{Deserialize, Serialize};

use super::asset::Value;

/// The arm pitch never reaches the pole (degrees), where yaw and roll are not separable.
pub const MAX_PITCH_DEG: f32 = 89.9;

/// Where the crane base is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum CraneBase {
    /// A fixed point on the ground (or anywhere), in world meters.
    Ground([f32; 3]),
    /// A point on a spline entity's rail.
    Rail {
        /// The spline entity's `Name`.
        spline: String,
        /// Distance along the rail in meters.
        position: Value<f32>,
        /// Add the rail's heading (its tangent's yaw) to the arm yaw.
        #[serde(default)]
        heading: bool,
    },
}

/// An optional `[min, max]` range, in degrees (angles) or meters (length).
pub type Range = Option<(f32, f32)>;

/// Per-axis limits; `None` = unlimited (pitch and length keep their hard limits).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct CraneLimits {
    /// Arm yaw range in degrees.
    #[serde(default)]
    pub yaw_deg: Range,
    /// Arm pitch range in degrees.
    #[serde(default)]
    pub pitch_deg: Range,
    /// Arm length range in meters.
    #[serde(default)]
    pub length: Range,
    /// Mount yaw range in degrees.
    #[serde(default)]
    pub mount_yaw_deg: Range,
    /// Mount pitch range in degrees.
    #[serde(default)]
    pub mount_pitch_deg: Range,
}

impl CraneLimits {
    /// The ranges in axis order.
    pub fn ranges(&self) -> [Range; 5] {
        [
            self.yaw_deg,
            self.pitch_deg,
            self.length,
            self.mount_yaw_deg,
            self.mount_pitch_deg,
        ]
    }

    /// Whether every range is finite with `min <= max`.
    pub fn is_valid(&self) -> bool {
        self.ranges()
            .iter()
            .flatten()
            .all(|(lo, hi)| lo.is_finite() && hi.is_finite() && lo <= hi)
    }
}

/// The five crane axes.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct CraneAxes {
    /// Arm yaw, degrees.
    pub yaw_deg: f32,
    /// Arm pitch, degrees.
    pub pitch_deg: f32,
    /// Arm length, meters.
    pub length: f32,
    /// Mount yaw relative to the arm, degrees.
    pub mount_yaw_deg: f32,
    /// Mount pitch relative to the arm, degrees.
    pub mount_pitch_deg: f32,
}

/// The names of the axes in the order of [`CraneAxes::to_array`] and [`CraneReport::clamped`].
pub const AXIS_NAMES: [&str; 5] = ["yaw", "pitch", "length", "mount_yaw", "mount_pitch"];

impl CraneAxes {
    /// The axes as an array in [`AXIS_NAMES`] order.
    pub fn to_array(self) -> [f32; 5] {
        [
            self.yaw_deg,
            self.pitch_deg,
            self.length,
            self.mount_yaw_deg,
            self.mount_pitch_deg,
        ]
    }

    fn from_array(a: [f32; 5]) -> Self {
        Self {
            yaw_deg: a[0],
            pitch_deg: a[1],
            length: a[2],
            mount_yaw_deg: a[3],
            mount_pitch_deg: a[4],
        }
    }

    /// The axes clamped to `limits` (and the hard pitch and length limits), plus which axes the
    /// clamp moved.
    ///
    /// The pitch range is *intersected* with the hard `±MAX_PITCH_DEG` range, so an authored
    /// range lying wholly above or below the hard limit clamps to the nearest hard bound instead
    /// of defeating it. Non-finite authored bounds (which [`CraneLimits::is_valid`] rejects on the
    /// asset path) are sanitized here so direct API callers cannot panic `f32::clamp` with NaN.
    pub fn clamped(self, limits: &CraneLimits) -> (Self, [bool; 5]) {
        let mut values = self.to_array();
        let mut moved = [false; 5];
        let ranges = limits.ranges();
        for (i, (value, range)) in values.iter_mut().zip(ranges).enumerate() {
            let (mut lo, mut hi) = range.unwrap_or((f32::NEG_INFINITY, f32::INFINITY));
            // NaN acts as unlimited; infinities are handled by the per-axis hard limits below.
            if lo.is_nan() {
                lo = f32::NEG_INFINITY;
            }
            if hi.is_nan() {
                hi = f32::INFINITY;
            }
            match i {
                1 => {
                    // Intersect with the hard range: clamp both bounds in, so an authored range
                    // wholly outside the hard limit still leaves pitch at the nearest hard bound.
                    lo = lo.clamp(-MAX_PITCH_DEG, MAX_PITCH_DEG);
                    hi = hi.clamp(-MAX_PITCH_DEG, MAX_PITCH_DEG);
                }
                2 => lo = lo.max(0.0),
                _ => {}
            }
            if lo > hi {
                hi = lo;
            }
            let clamped = value.clamp(lo, hi);
            moved[i] = clamped != *value;
            *value = clamped;
        }
        (Self::from_array(values), moved)
    }
}

/// What a [`super::asset::Node::Crane`] reported on its last evaluation (the agent's
/// `camera.crane_state`).
#[derive(Clone, Debug, PartialEq)]
pub struct CraneReport {
    /// Index of the node in evaluation order.
    pub node: usize,
    /// The base point in world meters.
    pub base: Vec3,
    /// The base heading added to the arm yaw, degrees (0 on the ground).
    pub heading_deg: f32,
    /// The requested axes, before the limits.
    pub requested: CraneAxes,
    /// The axes after the limits.
    pub axes: CraneAxes,
    /// Which axes the limits moved, in [`AXIS_NAMES`] order.
    pub clamped: [bool; 5],
    /// The camera position (the arm tip).
    pub tip: Vec3,
}

/// The arm tip and the camera rotation for a base, a heading (degrees, added to the arm yaw) and
/// clamped axes.
pub fn solve(base: Vec3, heading_deg: f32, axes: CraneAxes) -> (Vec3, Quat) {
    let arm_yaw = (heading_deg + axes.yaw_deg).to_radians();
    let arm_pitch = axes.pitch_deg.to_radians();
    let direction = Quat::from_euler(EulerRot::YXZ, arm_yaw, arm_pitch, 0.0) * Vec3::NEG_Z;
    let tip = base + direction * axes.length;
    let rotation = Quat::from_euler(
        EulerRot::YXZ,
        arm_yaw + axes.mount_yaw_deg.to_radians(),
        arm_pitch + axes.mount_pitch_deg.to_radians(),
        0.0,
    );
    (tip, rotation)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1.0e-4;

    fn axes(yaw: f32, pitch: f32, length: f32) -> CraneAxes {
        CraneAxes {
            yaw_deg: yaw,
            pitch_deg: pitch,
            length,
            ..CraneAxes::default()
        }
    }

    #[test]
    fn the_arm_points_along_minus_z_at_rest_and_yaw_turns_toward_minus_x() {
        let (tip, _) = solve(Vec3::new(1.0, 2.0, 3.0), 0.0, axes(0.0, 0.0, 4.0));
        assert!((tip - Vec3::new(1.0, 2.0, -1.0)).length() < EPS, "{tip}");
        let (tip, _) = solve(Vec3::ZERO, 0.0, axes(90.0, 0.0, 2.0));
        assert!((tip - Vec3::new(-2.0, 0.0, 0.0)).length() < EPS, "{tip}");
        let (tip, _) = solve(Vec3::ZERO, 0.0, axes(0.0, 90.0, 2.0));
        assert!((tip - Vec3::new(0.0, 2.0, 0.0)).length() < 1.0e-3, "{tip}");
    }

    #[test]
    fn the_heading_adds_to_the_arm_yaw() {
        let (a, _) = solve(Vec3::ZERO, 30.0, axes(60.0, 0.0, 2.0));
        let (b, _) = solve(Vec3::ZERO, 0.0, axes(90.0, 0.0, 2.0));
        assert!((a - b).length() < EPS);
    }

    #[test]
    fn the_mount_adds_to_the_arm_rotation_and_a_half_turn_looks_back_at_the_base() {
        let mut a = axes(0.0, 0.0, 3.0);
        a.mount_yaw_deg = 180.0;
        let (tip, rotation) = solve(Vec3::ZERO, 0.0, a);
        let forward = rotation * Vec3::NEG_Z;
        assert!((forward - Vec3::Z).length() < EPS, "{forward}");
        assert!((tip - Vec3::new(0.0, 0.0, -3.0)).length() < EPS);
    }

    #[test]
    fn limits_clamp_each_axis_and_report_which() {
        let limits = CraneLimits {
            yaw_deg: Some((-45.0, 45.0)),
            pitch_deg: Some((-10.0, 30.0)),
            length: Some((2.0, 6.0)),
            mount_yaw_deg: Some((-5.0, 5.0)),
            mount_pitch_deg: Some((0.0, 20.0)),
        };
        let wild = CraneAxes {
            yaw_deg: 100.0,
            pitch_deg: -50.0,
            length: 9.0,
            mount_yaw_deg: -9.0,
            mount_pitch_deg: 10.0,
        };
        let (got, moved) = wild.clamped(&limits);
        assert_eq!(got.to_array(), [45.0, -10.0, 6.0, -5.0, 10.0]);
        assert_eq!(moved, [true, true, true, true, false]);
        let (got, _) = CraneAxes { length: 0.5, ..wild }.clamped(&limits);
        assert_eq!(got.length, 2.0);
    }

    #[test]
    fn hard_limits_hold_without_authored_ones() {
        let (got, moved) = axes(0.0, 120.0, -3.0).clamped(&CraneLimits::default());
        assert_eq!(got.pitch_deg, MAX_PITCH_DEG);
        assert_eq!(got.length, 0.0);
        assert_eq!(moved, [false, true, true, false, false]);
    }

    #[test]
    fn invalid_limits_are_detected() {
        assert!(CraneLimits::default().is_valid());
        let inverted = CraneLimits {
            yaw_deg: Some((10.0, -10.0)),
            ..CraneLimits::default()
        };
        assert!(!inverted.is_valid());
        let nan = CraneLimits {
            length: Some((f32::NAN, 3.0)),
            ..CraneLimits::default()
        };
        assert!(!nan.is_valid());
    }

    #[test]
    fn a_pitch_range_above_the_hard_limit_clamps_to_the_hard_limit() {
        // Authored (100, 120) passes `is_valid` but lies wholly above ±89.9°: the intersection
        // must hold the arm at the pole-side hard bound, not at 100°.
        let limits = CraneLimits {
            pitch_deg: Some((100.0, 120.0)),
            ..CraneLimits::default()
        };
        assert!(limits.is_valid());
        let (got, moved) = axes(0.0, 105.0, 1.0).clamped(&limits);
        assert_eq!(got.pitch_deg, MAX_PITCH_DEG);
        assert!(moved[1]);
        // Symmetrically below the hard limit.
        let limits = CraneLimits {
            pitch_deg: Some((-120.0, -100.0)),
            ..CraneLimits::default()
        };
        let (got, _) = axes(0.0, -105.0, 1.0).clamped(&limits);
        assert_eq!(got.pitch_deg, -MAX_PITCH_DEG);
    }

    #[test]
    fn non_finite_limits_cannot_panic_clamp() {
        // `is_valid` rejects these, but `clamped` is public: NaN bounds must not reach
        // `f32::clamp`, and infinities are narrowed by the hard limits.
        let nan = CraneLimits {
            pitch_deg: Some((f32::NAN, f32::NAN)),
            ..CraneLimits::default()
        };
        let (got, _) = axes(0.0, 30.0, 1.0).clamped(&nan);
        assert_eq!(got.pitch_deg, 30.0);
        let infinite = CraneLimits {
            pitch_deg: Some((f32::NEG_INFINITY, f32::INFINITY)),
            length: Some((f32::NEG_INFINITY, f32::INFINITY)),
            ..CraneLimits::default()
        };
        let (got, _) = axes(0.0, 120.0, -3.0).clamped(&infinite);
        assert_eq!(got.pitch_deg, MAX_PITCH_DEG);
        assert_eq!(got.length, 0.0);
    }
}
