// SPDX-License-Identifier: MPL-2.0
// Source: linear blend skinning (N. Magnenat-Thalmann, R. Laperrière, D. Thalmann,
// "Joint-dependent local deformations for hand animation and object grasping", Graphics
// Interface 1988): a vertex is the weighted sum of its rest position transformed by each
// influencing joint's skinning matrix. Original code (spec 55 § 2.2, pose sync).

//! CPU linear blend skinning of a small point set, for cloth pose targets (spec 55 § 2.2).
//!
//! A cloth bound to a skinned character needs, per sim vertex, the position and normal the
//! vertex would have if it were only skinned. [`skin_points`] computes them from a
//! [`SkinPalette`](crate::SkinPalette)-style matrix list (`model_global · inverse_bind` per
//! joint) and up to four influences per vertex, then applies the mesh's world matrix. The cloth
//! glue feeds the result to `regia_physics::cloth::ClothSolver::set_pose_targets`.
//!
//! Normals use that blended matrix's 3×3, then are renormalized. A mirrored world (negative
//! determinant) is already in the matrix, so the normal follows the surface and still points
//! outward. There is no second flip: negating again would point the normal into the body, and
//! the standoff half-space (spec 55 § 2.2) would hold the cloth inside.

use regia_core::math::{Mat4, Vec3};
use thiserror::Error;
use tracing::debug_span;

/// Up to four joint influences of one vertex. Weights need not sum to 1 (they are
/// normalized); a vertex whose weights sum to 0 keeps its rest position (identity).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SkinInfluence {
    /// Joint indices into the palette.
    pub joints: [u16; 4],
    /// Weights (zero or more).
    pub weights: [f32; 4],
}

/// A [`skin_points`] input it refused. Nothing is written then.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkinPointsError {
    /// The rest positions, normals and influences differ in length.
    #[error("skin_points: {positions} positions, {normals} normals, {influences} influences")]
    LengthMismatch {
        /// Rest positions given.
        positions: usize,
        /// Rest normals given.
        normals: usize,
        /// Influences given.
        influences: usize,
    },
    /// An influence names a joint the palette does not have (with a non-zero weight).
    #[error("skin_points: joint {joint} is out of range ({len} palette matrices)")]
    JointOutOfRange {
        /// The joint index.
        joint: u16,
        /// Palette length.
        len: usize,
    },
    /// A weight is negative, NaN or infinite, or a matrix or rest value is not finite.
    #[error("skin_points: a weight, matrix or rest value is not finite or is negative")]
    BadValue,
}

impl SkinPointsError {
    /// Stable lowercase code (`skin_length_mismatch`, `skin_joint_out_of_range`,
    /// `skin_bad_value`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LengthMismatch { .. } => "skin_length_mismatch",
            Self::JointOutOfRange { .. } => "skin_joint_out_of_range",
            Self::BadValue => "skin_bad_value",
        }
    }
}

/// Skins `rest_positions` / `rest_normals` (model space) with `palette` and `influences`, then
/// applies `world`, writing world-space `(position, normal)` pairs into `out` (cleared first;
/// its allocation is reused). Normals follow the blended matrix's inverse-transpose (glTF
/// 2.0 § 3.7.3), falling back to the plain matrix when the blend is singular.
///
/// # Errors
/// [`SkinPointsError`] for mismatched lengths, an out-of-range joint with a non-zero weight,
/// or a non-finite or negative value; `out` is left empty then.
pub fn skin_points(
    palette: &[Mat4],
    world: Mat4,
    rest_positions: &[Vec3],
    rest_normals: &[Vec3],
    influences: &[SkinInfluence],
    out: &mut Vec<(Vec3, Vec3)>,
) -> Result<(), SkinPointsError> {
    let _span = debug_span!("cloth_pose_sync", vertices = rest_positions.len()).entered();
    out.clear();
    if rest_positions.len() != rest_normals.len() || rest_positions.len() != influences.len() {
        return Err(SkinPointsError::LengthMismatch {
            positions: rest_positions.len(),
            normals: rest_normals.len(),
            influences: influences.len(),
        });
    }
    if !world.is_finite() || palette.iter().any(|m| !m.is_finite()) {
        return Err(SkinPointsError::BadValue);
    }
    for inf in influences {
        for (&joint, &w) in inf.joints.iter().zip(&inf.weights) {
            if !(w.is_finite() && w >= 0.0) {
                return Err(SkinPointsError::BadValue);
            }
            if w > 0.0 && usize::from(joint) >= palette.len() {
                return Err(SkinPointsError::JointOutOfRange {
                    joint,
                    len: palette.len(),
                });
            }
        }
        // Each weight is finite, but huge finite weights can sum to +inf; the normalization
        // division would then write NaN and still return Ok.
        if !inf.weights.iter().sum::<f32>().is_finite() {
            return Err(SkinPointsError::BadValue);
        }
    }
    if rest_positions.iter().chain(rest_normals).any(|v| !v.is_finite()) {
        return Err(SkinPointsError::BadValue);
    }
    for ((&p, &n), inf) in rest_positions.iter().zip(rest_normals).zip(influences) {
        let total: f32 = inf.weights.iter().sum();
        let blended = if total > 0.0 {
            let mut m = Mat4::ZERO;
            for (&joint, &w) in inf.joints.iter().zip(&inf.weights) {
                if w > 0.0 {
                    m += palette[usize::from(joint)] * (w / total);
                }
            }
            world * m
        } else {
            world
        };
        let position = blended.transform_point3(p);
        // glTF 2.0 § 3.7.3: normals follow the inverse-transpose, so they stay perpendicular
        // under a non-uniform blend scale (the plain matrix transform leans them toward the
        // stretched axis). A singular blend keeps the plain transform.
        let normal = {
            let inv_t = blended.inverse().transpose();
            if inv_t.is_finite() {
                inv_t.transform_vector3(n).normalize_or_zero()
            } else {
                blended.transform_vector3(n).normalize_or_zero()
            }
        };
        out.push((position, normal));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    fn one(joint: u16) -> SkinInfluence {
        SkinInfluence {
            joints: [joint, 0, 0, 0],
            weights: [1.0, 0.0, 0.0, 0.0],
        }
    }

    #[test]
    fn identity_palette_keeps_the_rest_pose() {
        let mut out = Vec::new();
        skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[Vec3::Y],
            &[one(0)],
            &mut out,
        )
        .expect("skin");
        assert_eq!(out, vec![(Vec3::X, Vec3::Y)]);
    }

    #[test]
    fn two_joints_blend_by_weight_and_the_world_matrix_applies_last() {
        let palette = [Mat4::IDENTITY, Mat4::from_translation(Vec3::new(0.0, 2.0, 0.0))];
        let half = SkinInfluence {
            joints: [0, 1, 0, 0],
            weights: [1.0, 1.0, 0.0, 0.0],
        };
        let world = Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0));
        let mut out = Vec::new();
        skin_points(&palette, world, &[Vec3::ZERO], &[Vec3::Z], &[half], &mut out).expect("skin");
        assert!(
            (out[0].0 - Vec3::new(10.0, 1.0, 0.0)).length() < 1e-6,
            "{:?}",
            out[0]
        );
        assert!((out[0].1 - Vec3::Z).length() < 1e-6);
        // A rotated joint rotates the normal.
        let rot = [Mat4::from_quat(Quat::from_rotation_y(
            std::f32::consts::FRAC_PI_2,
        ))];
        skin_points(&rot, Mat4::IDENTITY, &[Vec3::X], &[Vec3::X], &[one(0)], &mut out).expect("skin");
        assert!((out[0].1 - Vec3::NEG_Z).length() < 1e-6, "{:?}", out[0]);
    }

    /// audit-g animation3 finding 1: under a mirrored root the linear transform already reflects
    /// the normal. Rest `+X` mirrored across the origin lands at `-X`; the stored normal must
    /// point away from the origin there, `-X`, not back toward the body.
    #[test]
    fn a_mirrored_world_flips_normals() {
        let world = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let mut out = Vec::new();
        skin_points(
            &[Mat4::IDENTITY],
            world,
            &[Vec3::X],
            &[Vec3::X],
            &[one(0)],
            &mut out,
        )
        .expect("skin");
        // The reflected point sits on -X. Outward is -X. The 3×3 already did that;
        // a second negation would store +X, toward the origin.
        assert!((out[0].0 + Vec3::X).length() < 1e-6);
        assert!(
            (out[0].1 - Vec3::NEG_X).length() < 1e-6,
            "outward after the mirror: {:?}",
            out[0]
        );
    }

    #[test]
    fn a_non_uniform_mirror_keeps_the_outward_normal() {
        // scale(-2, 1, 1): position scales, the axis normal still points out after
        // renormalize. A normal off that axis is not negated as a whole vector.
        let world = Mat4::from_scale(Vec3::new(-2.0, 1.0, 1.0));
        let mut out = Vec::new();
        skin_points(
            &[Mat4::IDENTITY],
            world,
            &[Vec3::X, Vec3::ZERO],
            &[Vec3::X, Vec3::Z],
            &[one(0), one(0)],
            &mut out,
        )
        .expect("skin");
        assert!(
            (out[0].0 - Vec3::new(-2.0, 0.0, 0.0)).length() < 1e-5,
            "{:?}",
            out[0]
        );
        assert!((out[0].1 - Vec3::NEG_X).length() < 1e-5, "{:?}", out[0]);
        assert!((out[1].1 - Vec3::Z).length() < 1e-5, "{:?}", out[1]);
    }

    #[test]
    fn a_non_uniform_scale_bends_normals_by_the_inverse_transpose() {
        // Stretch x by 4: a normal off the axis must lean toward the un-stretched axis
        // (inverse-transpose), not hug the stretched one (plain matrix transform).
        // (1, 1, 0) -> (1/4, 1, 0), renormalised ≈ (0.2425, 0.9701, 0).
        let palette = [Mat4::from_scale(Vec3::new(4.0, 1.0, 1.0))];
        let mut out = Vec::new();
        skin_points(
            &palette,
            Mat4::IDENTITY,
            &[Vec3::ZERO],
            &[Vec3::new(1.0, 1.0, 0.0).normalize()],
            &[one(0)],
            &mut out,
        )
        .expect("skin");
        let expected = Vec3::new(0.25, 1.0, 0.0).normalize();
        assert!(
            (out[0].1 - expected).length() < 1e-5,
            "normal {:?}, want {expected:?}",
            out[0].1
        );
    }

    #[test]
    fn bad_input_is_refused_and_writes_nothing() {
        let mut out = vec![(Vec3::ONE, Vec3::ONE)];
        let err = skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[],
            &[one(0)],
            &mut out,
        )
        .expect_err("lengths");
        assert_eq!(err.code(), "skin_length_mismatch");
        assert!(out.is_empty());
        let err = skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[Vec3::Y],
            &[one(3)],
            &mut out,
        )
        .expect_err("joint");
        assert_eq!(err.code(), "skin_joint_out_of_range");
        let mut negative = one(0);
        negative.weights[0] = -1.0;
        let err = skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[Vec3::Y],
            &[negative],
            &mut out,
        )
        .expect_err("weight");
        assert_eq!(err.code(), "skin_bad_value");
        // audit-g animation3 finding 6: finite weights that overflow to +inf when summed are
        // refused too, not written as NaN with Ok.
        let mut overflowing = one(0);
        overflowing.weights = [f32::MAX, f32::MAX, 0.0, 0.0];
        let err = skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[Vec3::Y],
            &[overflowing],
            &mut out,
        )
        .expect_err("overflowing weights");
        assert_eq!(err.code(), "skin_bad_value");
        assert!(out.is_empty(), "a refused input writes nothing");
        // Zero weights keep the rest pose.
        let none = SkinInfluence::default();
        skin_points(
            &[Mat4::IDENTITY],
            Mat4::IDENTITY,
            &[Vec3::X],
            &[Vec3::Y],
            &[none],
            &mut out,
        )
        .expect("rest");
        assert_eq!(out, vec![(Vec3::X, Vec3::Y)]);
    }
}
