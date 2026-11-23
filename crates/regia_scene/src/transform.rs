// SPDX-License-Identifier: MPL-2.0
//! Local transforms and their derived world-space forms (`docs/specs/14-scene-and-framework.md` A1).
//!
//! [`Transform`] is the authored value: translation, rotation and scale **relative to the parent**
//! entity. For entities without a parent it is the world transform. [`GlobalTransform`] is the
//! derived world-space form, recomputed each frame by the propagation system in
//! `PostUpdate/TransformPropagate`; [`PreviousGlobalTransform`] keeps the value of the previous
//! frame for motion vectors and interpolation.

use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;

/// Position, rotation and scale relative to the parent entity (identity parent ⇒ world space).
///
/// Conventions: right-handed, +Y up, forward −Z, metres, radians. Default: translation at the
/// origin, identity rotation, uniform scale of 1. Inserting a `Transform` also inserts
/// [`GlobalTransform`] and [`PreviousGlobalTransform`] (required components).
///
/// # Example
///
/// ```
/// use regia_core::math::Vec3;
/// use regia_scene::Transform;
///
/// let t = Transform::from_translation(Vec3::new(1.0, 2.0, 3.0));
/// let p = t.transform_point(Vec3::new(0.0, 1.0, 0.0));
/// assert_eq!(p, Vec3::new(1.0, 3.0, 3.0)); // metres, translated by the same vector
/// ```
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(GlobalTransform, PreviousGlobalTransform)]
pub struct Transform {
    /// Translation relative to the parent, in metres.
    pub translation: Vec3,
    /// Rotation relative to the parent, as a unit quaternion.
    pub rotation: Quat,
    /// Non-uniform scale relative to the parent (1 = original size).
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// The identity transform: no translation, rotation or scaling.
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    /// A transform that translates by `translation`, with identity rotation and scale.
    pub const fn from_translation(translation: Vec3) -> Self {
        Self {
            translation,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }

    /// A transform that rotates by `rotation`, at the origin with unit scale.
    ///
    /// ```
    /// use regia_core::math::{Quat, Vec3};
    /// use regia_scene::Transform;
    ///
    /// // A quarter turn about +Y sends +X to -Z (right-handed, +Y up).
    /// let t = Transform::from_rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
    /// let p = t.transform_point(Vec3::X);
    /// assert!((p - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-5);
    /// ```
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation,
            scale: Vec3::ONE,
        }
    }

    /// A uniform scale of `scale`, at the origin with identity rotation.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_scene::Transform;
    ///
    /// let t = Transform::from_scale(2.0);
    /// assert_eq!(t.transform_point(Vec3::new(1.0, 2.0, 3.0)), Vec3::new(2.0, 4.0, 6.0));
    /// ```
    pub const fn from_scale(scale: f32) -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::splat(scale),
        }
    }

    /// The transform as an affine matrix (translation-rotation-scale applied in that order).
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_scene::Transform;
    ///
    /// let t = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
    /// assert_eq!(t.compute_affine().translation, Vec3::new(1.0, 0.0, 0.0).into());
    /// ```
    pub fn compute_affine(&self) -> Affine3A {
        Affine3A::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Decomposes an affine matrix back into translation, rotation and scale.
    ///
    /// Exact for pure TRS inputs; sheared or projected matrices (which `Transform` cannot
    /// represent) decompose approximately. Used when reparenting with `keep_world_transform`.
    ///
    /// The rotation extraction is Shepperd's method (see `docs/provenance.md`): glam's
    /// `Quat::from_mat3` (the DirectXMath algorithm) is only accurate to ~1e-3 near axis-aligned
    /// 90° rotations, which would make `keep_world_transform` reparenting visibly drift.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_scene::Transform;
    ///
    /// let t = Transform {
    ///     translation: Vec3::new(1.0, 2.0, 3.0),
    ///     scale: Vec3::new(2.0, 2.0, 2.0),
    ///     ..Transform::IDENTITY
    /// };
    /// let back = Transform::from_affine(t.compute_affine());
    /// assert!((back.translation - t.translation).length() < 1e-5);
    /// assert!((back.scale - t.scale).length() < 1e-5);
    /// ```
    pub fn from_affine(affine: Affine3A) -> Self {
        let det = affine.matrix3.determinant();
        // A mirrored matrix carries its sign on the x scale; a singular one keeps +length.
        let sign = if det < 0.0 { -1.0 } else { 1.0 };
        let columns = [
            Vec3::from(affine.matrix3.x_axis),
            Vec3::from(affine.matrix3.y_axis),
            Vec3::from(affine.matrix3.z_axis),
        ];
        let lengths = Vec3::new(columns[0].length(), columns[1].length(), columns[2].length());
        let scale = Vec3::new(lengths.x * sign, lengths.y, lengths.z);
        let unit = |axis: Vec3, length: f32| (length > 0.0).then(|| axis / length);
        let x = unit(columns[0], lengths.x);
        let y = unit(columns[1], lengths.y);
        let z = unit(columns[2], lengths.z);
        // A single collapsed axis (flat decal) is rebuilt from the other two.
        let axes = match (x, y, z) {
            (Some(x), Some(y), Some(z)) => Some((x, y, z)),
            (Some(x), Some(y), None) => Some((x, y, x.cross(y))),
            (Some(x), None, Some(z)) => Some((x, z.cross(x), z)),
            (None, Some(y), Some(z)) => Some((y.cross(z), y, z)),
            _ => None,
        };
        let rotation = match (axes, (x, y, z)) {
            // The mirrored x axis points opposite its column.
            (Some((x, y, z)), _) => quat_from_orthonormal_axes(x * sign, y, z),
            // Only one axis survives: the roll about it is unknowable, but its direction is not,
            // so turn the matching basis vector onto it by the shortest arc.
            (None, (Some(x), None, None)) => Quat::from_rotation_arc(Vec3::X, x),
            (None, (None, Some(y), None)) => Quat::from_rotation_arc(Vec3::Y, y),
            (None, (None, None, Some(z))) => Quat::from_rotation_arc(Vec3::Z, z),
            // Fully degenerate: the decomposition is meaningless, keep identity.
            (None, _) => Quat::IDENTITY,
        };
        Self {
            translation: Vec3::from(affine.translation),
            rotation,
            scale,
        }
    }

    /// Applies `self` to a point expressed in local space, yielding the parent-space point.
    pub fn transform_point(&self, point: Vec3) -> Vec3 {
        self.compute_affine().transform_point3(point)
    }
}

/// Shepperd's method: the unit quaternion whose rotation matrix has the orthonormal columns
/// `x_axis`, `y_axis`, `z_axis` (`docs/provenance.md`: S. W. Shepperd, "Quaternion from Rotation
/// Matrix", AIAA Journal of Guidance and Control, 1978). Unlike glam's `Quat::from_mat3` (the
/// DirectXMath algorithm, ~1e-3 worst-case error), this is exact to f32 precision including the
/// axis-aligned 90° cases that `keep_world_transform` reparenting hits.
fn quat_from_orthonormal_axes(x_axis: Vec3, y_axis: Vec3, z_axis: Vec3) -> Quat {
    // Matrix entries in [row][col] form; glam exposes columns as axes, so `m<row><col>` is
    // `axis[col]`'s row-component (glam `Mat3` columns = image of the basis vectors).
    let (m00, m10, m20) = x_axis.into();
    let (m01, m11, m21) = y_axis.into();
    let (m02, m12, m22) = z_axis.into();

    // Branch on the largest accumulator to avoid subtracting nearly equal numbers.
    let trace = m00 + m11 + m22;
    let quat = if trace > 0.0 {
        let s = (1.0 + trace).sqrt() * 2.0;
        Quat::from_xyzw((m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s)
    } else if m00 >= m11 && m00 >= m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        Quat::from_xyzw(0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s)
    } else if m11 >= m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        Quat::from_xyzw((m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s)
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        Quat::from_xyzw((m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s)
    };
    // The inputs are orthonormal so the result is unit to f32 precision; normalize anyway so
    // slightly non-orthonormal inputs (f32 noise through a compose/decompose chain) stay valid.
    quat.normalize()
}

/// The world-space transform of an entity: the product of every [`Transform`] up the parent chain.
///
/// Derived data — never authored, never saved in scene files. Recomputed by
/// [`crate::propagate::propagate_transforms`] each frame; between propagation runs it may lag
/// behind a freshly changed [`Transform`].
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct GlobalTransform(pub Affine3A);

impl GlobalTransform {
    /// The identity (world-origin) transform.
    pub const IDENTITY: Self = Self(Affine3A::IDENTITY);

    /// World-space translation.
    pub fn translation(&self) -> Vec3 {
        Vec3::from(self.0.translation)
    }

    /// Decomposes into a [`Transform`] (approximate for sheared matrices; see
    /// [`Transform::from_affine`]).
    pub fn to_transform(&self) -> Transform {
        Transform::from_affine(self.0)
    }

    /// Composes `self` (parent, world space) with a child's local `Transform`.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_scene::{GlobalTransform, Transform};
    ///
    /// let parent = GlobalTransform::IDENTITY.mul_transform(&Transform::from_translation(Vec3::X));
    /// let child = parent.mul_transform(&Transform::from_translation(Vec3::Y));
    /// assert_eq!(child.translation(), Vec3::new(1.0, 1.0, 0.0));
    /// ```
    pub fn mul_transform(&self, local: &Transform) -> Self {
        Self(self.0 * local.compute_affine())
    }
}

impl std::ops::Mul<Transform> for GlobalTransform {
    type Output = GlobalTransform;

    fn mul(self, local: Transform) -> Self::Output {
        self.mul_transform(&local)
    }
}

/// The [`GlobalTransform`] of the previous frame's propagation run, for motion vectors,
/// temporal anti-aliasing and interpolation (`docs/specs/14-scene-and-framework.md` A1).
///
/// On the first frame after spawn it holds the identity, not the spawn pose.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct PreviousGlobalTransform(pub Affine3A);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_defaults() {
        let t = Transform::default();
        assert_eq!(t, Transform::IDENTITY);
        assert_eq!(t.compute_affine(), Affine3A::IDENTITY);
        assert_eq!(GlobalTransform::IDENTITY, GlobalTransform::default());
    }

    /// Decompose∘compose must reproduce the pose to within f32 noise. Near 90° rotations one
    /// ulp of quaternion components (≈6e-8) maps to ≈3e-4 rad in the angle metric, so the
    /// tolerance below is the f32 floor, not an algorithmic slack (Shepperd extraction;
    /// glam's `Quat::from_mat3` measures ≈9.8e-4 here — see `docs/provenance.md`).
    #[test]
    fn affine_round_trip() {
        for rotation in [
            Quat::IDENTITY,
            Quat::from_rotation_y(90f32.to_radians()),
            Quat::from_rotation_x(90f32.to_radians()),
            Quat::from_rotation_z(90f32.to_radians()),
            Quat::from_rotation_y(1.1) * Quat::from_rotation_x(0.5) * Quat::from_rotation_z(-0.3),
        ] {
            for scale in [Vec3::ONE, Vec3::new(2.0, 0.5, 1.0), Vec3::splat(0.25)] {
                let t = Transform {
                    translation: Vec3::new(1.0, -2.0, 3.0),
                    rotation,
                    scale,
                };
                let back = Transform::from_affine(t.compute_affine());
                assert!((back.translation - t.translation).length() < 1e-5, "{back:?}");
                // Compare applied rotations (what a pose actually is), not raw components.
                for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
                    let a = rotation * axis;
                    let b = back.rotation * axis;
                    assert!(
                        a.angle_between(b) < 5e-4,
                        "rotation: {rotation:?} -> {back:?} (axis {axis:?})"
                    );
                }
                assert!((back.scale - scale).length() < 1e-5, "{back:?}");
            }
        }
    }

    /// Mirrored and flat (zero-axis) scales keep their orientation through decomposition.
    #[test]
    fn affine_round_trip_non_positive_scale() {
        let rotation = Quat::from_rotation_y(1.1) * Quat::from_rotation_x(0.5);
        for scale in [
            Vec3::new(-2.0, 1.0, 1.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(1.5, 0.0, 1.0),
        ] {
            let t = Transform {
                translation: Vec3::new(1.0, 2.0, 3.0),
                rotation,
                scale,
            };
            let back = Transform::from_affine(t.compute_affine());
            assert!((back.scale - scale).length() < 1e-5, "{scale:?} -> {back:?}");
            let again = back.compute_affine();
            let original = t.compute_affine();
            let delta = (again.matrix3.x_axis - original.matrix3.x_axis).length()
                + (again.matrix3.y_axis - original.matrix3.y_axis).length()
                + (again.matrix3.z_axis - original.matrix3.z_axis).length();
            assert!(delta < 1e-4, "{scale:?}: matrix drift {delta}");
            assert!(
                back.rotation.angle_between(rotation) < 1e-3,
                "{scale:?}: {back:?}"
            );
        }
    }

    /// Shepperd extraction agrees with glam's on generic rotations (where both are precise).
    #[test]
    fn quat_extraction_matches_glam_on_generic_rotations() {
        let generic = Quat::from_rotation_y(1.1) * Quat::from_rotation_x(0.5);
        let (x, y, z) = (generic * Vec3::X, generic * Vec3::Y, generic * Vec3::Z);
        let shepperd = quat_from_orthonormal_axes(x, y, z);
        assert!(
            shepperd.angle_between(generic) < 1e-6,
            "{shepperd:?} vs {generic:?}"
        );
    }

    /// 90° yaw survives decompose∘compose at the f32 ulp floor, where glam's extraction
    /// loses ≈1e-3 rad (keep-world-transform reparenting must not visibly drift).
    #[test]
    fn quat_extraction_exact_at_90_degrees() {
        let expected = Quat::from_rotation_y(90f32.to_radians());
        let (x, y, z) = (expected * Vec3::X, expected * Vec3::Y, expected * Vec3::Z);
        let quat = quat_from_orthonormal_axes(x, y, z);
        // Apply both to a basis vector: the pose error is what propagation composes.
        let a = quat * Vec3::X;
        let b = expected * Vec3::X;
        assert!(a.angle_between(b) < 5e-4, "{quat:?} vs {expected:?}");
    }

    #[test]
    fn parent_times_child() {
        let parent = GlobalTransform(Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        let child = Transform::from_translation(Vec3::new(0.0, 5.0, 0.0));
        let world = parent * child;
        assert_eq!(world.translation(), Vec3::new(10.0, 5.0, 0.0));
    }
}
