// SPDX-License-Identifier: MPL-2.0
//! Rigid transforms (rotation + translation, no scale).

use std::ops::Mul;

use glam::{Affine3A, Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use super::rng::det_slerp;

/// A rigid transform: rotate by `rotation`, then translate by `translation`. Used for physics poses,
/// camera rigs and network snapshots where scale is meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Isometry {
    /// Rotation (unit quaternion).
    pub rotation: Quat,
    /// Translation applied after the rotation.
    pub translation: Vec3,
}

impl Default for Isometry {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Isometry {
    /// The identity transform.
    pub const IDENTITY: Self = Self {
        rotation: Quat::IDENTITY,
        translation: Vec3::ZERO,
    };

    /// Creates an isometry.
    #[inline]
    pub const fn new(rotation: Quat, translation: Vec3) -> Self {
        Self {
            rotation,
            translation,
        }
    }

    /// A pure translation.
    #[inline]
    pub const fn from_translation(translation: Vec3) -> Self {
        Self::new(Quat::IDENTITY, translation)
    }

    /// A pure rotation.
    #[inline]
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self::new(rotation, Vec3::ZERO)
    }

    /// The rigid part of an affine transform (scale and shear discarded).
    pub fn from_affine(affine: &Affine3A) -> Self {
        let (_, rotation, translation) = affine.to_scale_rotation_translation();
        Self::new(rotation.normalize(), translation)
    }

    /// An isometry at `eye` looking towards `target` (forward −Z, `up` hint +Y-like). Returns the
    /// world-from-local transform; `None` if `eye == target` or `up` is parallel to the view direction.
    pub fn looking_at(eye: Vec3, target: Vec3, up: Vec3) -> Option<Self> {
        let forward = (target - eye).try_normalize()?;
        let right = forward.cross(up).try_normalize()?;
        let up = right.cross(forward);
        let rotation = Quat::from_mat3(&glam::Mat3::from_cols(right, up, -forward));
        Some(Self::new(rotation, eye))
    }

    /// The inverse transform.
    #[inline]
    pub fn inverse(&self) -> Self {
        let rotation = self.rotation.inverse();
        Self::new(rotation, -(rotation * self.translation))
    }

    /// Transforms a point (rotation and translation).
    #[inline]
    pub fn transform_point(&self, point: Vec3) -> Vec3 {
        self.rotation * point + self.translation
    }

    /// Transforms a direction (rotation only).
    #[inline]
    pub fn transform_vector(&self, vector: Vec3) -> Vec3 {
        self.rotation * vector
    }

    /// Interpolates translation linearly and rotation by the deterministic slerp kernel
    /// ([`crate::math::det_slerp`]): bit-identical on every target, so swept physics poses and
    /// network snapshots do not split on FMA or libm differences.
    pub fn interpolate(&self, other: &Self, t: f32) -> Self {
        Self::new(
            det_slerp(self.rotation, other.rotation, t),
            self.translation.lerp(other.translation, t),
        )
    }

    /// As an [`Affine3A`].
    #[inline]
    pub fn to_affine(&self) -> Affine3A {
        Affine3A::from_rotation_translation(self.rotation, self.translation)
    }

    /// As a [`Mat4`].
    #[inline]
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from_rotation_translation(self.rotation, self.translation)
    }
}

impl Mul for Isometry {
    type Output = Isometry;

    /// `a * b` applies `b` first, then `a` (like matrices).
    #[inline]
    fn mul(self, rhs: Isometry) -> Isometry {
        Isometry::new(
            self.rotation * rhs.rotation,
            self.rotation * rhs.translation + self.translation,
        )
    }
}

impl From<Isometry> for Affine3A {
    fn from(iso: Isometry) -> Self {
        iso.to_affine()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_inverse_and_matrix_agree() {
        let a = Isometry::new(Quat::from_rotation_y(0.8), Vec3::new(1.0, 2.0, 3.0));
        let b = Isometry::new(Quat::from_rotation_x(-0.4), Vec3::new(-2.0, 0.5, 4.0));
        let p = Vec3::new(0.3, -1.2, 5.0);
        let ab = a * b;
        assert!(
            ab.transform_point(p)
                .abs_diff_eq(a.transform_point(b.transform_point(p)), 1e-5)
        );
        assert!(
            ab.to_mat4()
                .transform_point3(p)
                .abs_diff_eq(ab.transform_point(p), 1e-5)
        );
        assert!(
            ab.to_affine()
                .transform_point3(p)
                .abs_diff_eq(ab.transform_point(p), 1e-5)
        );
        let back = ab.inverse().transform_point(ab.transform_point(p));
        assert!(back.abs_diff_eq(p, 1e-5));
        let round = Isometry::from_affine(&ab.to_affine());
        assert!(round.translation.abs_diff_eq(ab.translation, 1e-5));
        assert!(
            round.rotation.abs_diff_eq(ab.rotation, 1e-5) || round.rotation.abs_diff_eq(-ab.rotation, 1e-5)
        );
        assert_eq!(a.transform_vector(Vec3::ZERO), Vec3::ZERO);
        let half = Isometry::IDENTITY.interpolate(&Isometry::from_translation(Vec3::X * 2.0), 0.5);
        assert_eq!(half.translation, Vec3::X);
    }

    #[test]
    fn looking_at_points_forward_minus_z() {
        let iso = Isometry::looking_at(Vec3::new(0.0, 0.0, 5.0), Vec3::new(5.0, 0.0, 5.0), Vec3::Y)
            .expect("valid basis");
        assert!(iso.transform_vector(Vec3::NEG_Z).abs_diff_eq(Vec3::X, 1e-6));
        assert!(iso.transform_vector(Vec3::Y).abs_diff_eq(Vec3::Y, 1e-6));
        assert!(Isometry::looking_at(Vec3::ZERO, Vec3::ZERO, Vec3::Y).is_none());
        assert!(Isometry::looking_at(Vec3::ZERO, Vec3::Y, Vec3::Y).is_none());
    }
}
