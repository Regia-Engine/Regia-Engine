// SPDX-License-Identifier: MPL-2.0
//! Conversions between Regia Engine glam types and rapier (glamx) types.
//!
//! Rapier 0.36 uses `glamx` (glam 0.33) through `rapier3d::math`. Regia Engine uses
//! `regia_core::math` (glam 0.33). Both wrappers (`glam::Vec3` and `glamx::Vec3`)
//! share identical `#[repr(C)]` fields `{x, y, z}`; we bridge via field access.
//!
//! Provenance: standard layout-compatible field access; glam and glamx both
//! guarantee `#[repr(C)]` layouts with the same field order.

use regia_core::math::{Quat, Vec3};

/// Convert a Regia Engine [`Vec3`] to a rapier vector.
#[inline]
pub fn to_rapier_vec3(v: Vec3) -> rapier3d::math::Vector {
    rapier3d::math::Vector::new(v.x, v.y, v.z)
}

/// Convert a rapier vector to a Regia Engine [`Vec3`].
#[inline]
pub fn from_rapier_vec3(v: rapier3d::math::Vector) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

/// Convert a Regia Engine [`Quat`] to a rapier rotation.
#[inline]
pub fn to_rapier_rotation(q: Quat) -> rapier3d::math::Rotation {
    let gxq = rapier3d::math::glamx::Quat::from_xyzw(q.x, q.y, q.z, q.w);
    rapier3d::math::Rotation::from(gxq)
}

/// Convert a rapier rotation to a Regia Engine [`Quat`].
#[inline]
pub fn from_rapier_rotation(r: rapier3d::math::Rotation) -> Quat {
    Quat::from_xyzw(r.x, r.y, r.z, r.w)
}

/// Convert a Regia Engine Quat to an axis-angle vector for rapier builders.
///
/// The round trip uses `atan2` and rapier rebuilds the quaternion with `sin`/`cos`, so the
/// stored rotation bits differ per platform libm — never use this where the bits feed
/// `PhysicsWorld::state_digest` or another lockstep comparison (spec 15 § 5): spawn with
/// [`to_rapier_pose`] (which copies the quaternion bits) instead. Retained for tests and
/// diagnostics that compare against the platform round trip.
#[inline]
pub fn to_rapier_angvec(q: Quat) -> rapier3d::math::AngVector {
    let (axis, angle) = q.to_axis_angle();
    to_rapier_vec3(axis * angle)
}

/// Convert a Regia Engine translation+rotation to a rapier pose.
#[inline]
pub fn to_rapier_pose(translation: Vec3, rotation: Quat) -> rapier3d::math::Pose {
    rapier3d::math::Pose::from_parts(to_rapier_vec3(translation), to_rapier_rotation(rotation))
}

/// Convert a rapier pose to a Regia Engine translation and rotation.
#[inline]
pub fn from_rapier_pose(pose: &rapier3d::math::Pose) -> (Vec3, Quat) {
    (
        from_rapier_vec3(pose.translation),
        from_rapier_rotation(pose.rotation),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    #[test]
    fn vec3_round_trip() {
        let v = Vec3::new(1.0, -2.5, 3.0);
        let r = to_rapier_vec3(v);
        let back = from_rapier_vec3(r);
        assert!((v - back).length() < 1e-6);
    }

    #[test]
    fn rotation_round_trip() {
        let q = Quat::from_rotation_y(PI / 4.0);
        let r = to_rapier_rotation(q);
        let back = from_rapier_rotation(r);
        assert!(q.abs_diff_eq(back, 1e-6) || (-q).abs_diff_eq(back, 1e-6));
    }

    #[test]
    fn pose_round_trip() {
        let t = Vec3::new(10.0, 0.0, -5.0);
        let q = Quat::from_rotation_x(0.5);
        let iso = to_rapier_pose(t, q);
        let (t2, q2) = from_rapier_pose(&iso);
        assert!((t - t2).length() < 1e-5);
        assert!(q.abs_diff_eq(q2, 1e-6) || (-q).abs_diff_eq(q2, 1e-6));
    }

    #[test]
    fn angvec_round_trip() {
        let q = Quat::from_rotation_x(std::f32::consts::FRAC_PI_2);
        let av = to_rapier_angvec(q);
        // Axis-angle vector should be ~(PI/2, 0, 0)
        assert!((av.x - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        assert!(av.y.abs() < 1e-4);
        assert!(av.z.abs() < 1e-4);
    }
}
