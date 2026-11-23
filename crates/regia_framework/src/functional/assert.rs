// SPDX-License-Identifier: MPL-2.0
//! Pure comparison helpers behind the functional-test assertions (spec 41 § 2.4). Each returns
//! `Ok(())` or a deterministic failure text (no addresses, no timestamps), so reports diff cleanly.

use regia_core::math::{Mat4, Quat, Vec2, Vec3, Vec4};
use regia_scene::Transform;

/// The six-way relational selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

impl Relation {
    /// The operator as text.
    pub fn symbol(self) -> &'static str {
        match self {
            Relation::Eq => "==",
            Relation::Ne => "!=",
            Relation::Lt => "<",
            Relation::Le => "<=",
            Relation::Gt => ">",
            Relation::Ge => ">=",
        }
    }

    /// Applies the relation to two partially ordered values (NaN never satisfies any relation but `Ne`).
    pub fn holds<T: PartialOrd>(self, a: &T, b: &T) -> bool {
        match self {
            Relation::Eq => a == b,
            Relation::Ne => a != b,
            Relation::Lt => a < b,
            Relation::Le => a <= b,
            Relation::Gt => a > b,
            Relation::Ge => a >= b,
        }
    }
}

fn label_prefix(label: &str) -> String {
    if label.is_empty() {
        String::new()
    } else {
        format!("{label}: ")
    }
}

/// `f64` equality within `tolerance` (inclusive: a difference equal to the tolerance passes).
pub fn near_f64(actual: f64, expected: f64, tolerance: f64, label: &str) -> Result<(), String> {
    let diff = (actual - expected).abs();
    if diff <= tolerance {
        Ok(())
    } else {
        Err(format!(
            "{}expected {expected} within {tolerance}, got {actual} (difference {diff})",
            label_prefix(label)
        ))
    }
}

/// `f32` equality within `tolerance`.
pub fn near_f32(actual: f32, expected: f32, tolerance: f32, label: &str) -> Result<(), String> {
    near_f64(
        f64::from(actual),
        f64::from(expected),
        f64::from(tolerance),
        label,
    )
}

fn near_components(
    actual: &[f32],
    expected: &[f32],
    tolerance: f32,
    label: &str,
    kind: &str,
) -> Result<(), String> {
    // `f32::max` drops NaN, so a NaN difference is carried explicitly: it never passes.
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, e)| (a - e).abs())
        .fold(0.0f32, |m, d| {
            if m.is_nan() || d.is_nan() {
                f32::NAN
            } else {
                m.max(d)
            }
        });
    if worst <= tolerance {
        Ok(())
    } else {
        Err(format!(
            "{}{kind} expected {expected:?} within {tolerance}, got {actual:?} (largest difference {worst})",
            label_prefix(label)
        ))
    }
}

/// `Vec2` equality within `tolerance` per component.
pub fn near_vec2(actual: Vec2, expected: Vec2, tolerance: f32, label: &str) -> Result<(), String> {
    near_components(&actual.to_array(), &expected.to_array(), tolerance, label, "Vec2")
}

/// `Vec3` equality within `tolerance` per component.
pub fn near_vec3(actual: Vec3, expected: Vec3, tolerance: f32, label: &str) -> Result<(), String> {
    near_components(&actual.to_array(), &expected.to_array(), tolerance, label, "Vec3")
}

/// `Vec4` equality within `tolerance` per component.
pub fn near_vec4(actual: Vec4, expected: Vec4, tolerance: f32, label: &str) -> Result<(), String> {
    near_components(&actual.to_array(), &expected.to_array(), tolerance, label, "Vec4")
}

/// Angle in radians between two rotations (`q` and `-q` are the same rotation).
pub fn quat_angle(a: Quat, b: Quat) -> f32 {
    let dot = a.normalize().dot(b.normalize()).abs().clamp(0.0, 1.0);
    2.0 * dot.acos()
}

/// `Quat` equality: the angle between the rotations is within `tolerance` radians.
pub fn near_quat(actual: Quat, expected: Quat, tolerance: f32, label: &str) -> Result<(), String> {
    let angle = quat_angle(actual, expected);
    if angle <= tolerance {
        Ok(())
    } else {
        Err(format!(
            "{}Quat expected {:?} within {tolerance} rad, got {:?} (angle {angle})",
            label_prefix(label),
            expected.to_array(),
            actual.to_array()
        ))
    }
}

/// Two rotations are equal when their forward directions (`-Z`) agree within `tolerance` per
/// component; stable near Euler singularities where the angles themselves jump.
pub fn same_facing(actual: Quat, expected: Quat, tolerance: f32, label: &str) -> Result<(), String> {
    let a = actual.normalize() * Vec3::NEG_Z;
    let e = expected.normalize() * Vec3::NEG_Z;
    near_components(&a.to_array(), &e.to_array(), tolerance, label, "forward")
}

/// `Transform` equality: translation, rotation angle and scale each within `tolerance`.
pub fn near_transform(
    actual: &Transform,
    expected: &Transform,
    tolerance: f32,
    label: &str,
) -> Result<(), String> {
    near_vec3(
        actual.translation,
        expected.translation,
        tolerance,
        &format!("{label} translation"),
    )?;
    near_quat(
        actual.rotation,
        expected.rotation,
        tolerance,
        &format!("{label} rotation"),
    )?;
    near_vec3(actual.scale, expected.scale, tolerance, &format!("{label} scale"))
}

/// `Mat4` equality, element-wise within `tolerance`.
pub fn near_mat4(actual: &Mat4, expected: &Mat4, tolerance: f32, label: &str) -> Result<(), String> {
    near_components(
        &actual.to_cols_array(),
        &expected.to_cols_array(),
        tolerance,
        label,
        "Mat4",
    )
}

/// Exact equality of any `PartialEq + Debug` value.
pub fn exact<T: PartialEq + std::fmt::Debug>(actual: &T, expected: &T, label: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{}expected {expected:?}, got {actual:?}",
            label_prefix(label)
        ))
    }
}

/// Relational check of two ordered values.
pub fn relate<T: PartialOrd + std::fmt::Display>(
    actual: T,
    relation: Relation,
    expected: T,
    label: &str,
) -> Result<(), String> {
    if relation.holds(&actual, &expected) {
        Ok(())
    } else {
        Err(format!(
            "{}expected {actual} {} {expected}",
            label_prefix(label),
            relation.symbol()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerance_edges() {
        assert!(near_f32(1.0, 1.5, 0.5, "").is_ok(), "on the edge passes");
        assert!(near_f32(1.0, 1.4, 0.5, "").is_ok(), "inside passes");
        let err = near_f32(1.0, 1.75, 0.5, "x").expect_err("outside fails");
        assert_eq!(err, "x: expected 1.75 within 0.5, got 1 (difference 0.75)");
    }

    #[test]
    fn quaternion_double_cover() {
        let q = Quat::from_rotation_y(0.7);
        assert!(near_quat(q, -q, 1e-3, "").is_ok());
        assert!(near_quat(q, Quat::from_rotation_y(0.9), 0.1, "").is_err());
        assert!(near_quat(q, Quat::from_rotation_y(0.9), 0.25, "").is_ok());
    }

    #[test]
    fn facing_is_stable_at_gimbal_singularity() {
        // Pitch of 90 degrees: yaw and roll are interchangeable; the facing is not.
        let a = Quat::from_euler(
            regia_core::math::EulerRot::YXZ,
            0.3,
            std::f32::consts::FRAC_PI_2,
            0.0,
        );
        let b = Quat::from_euler(
            regia_core::math::EulerRot::YXZ,
            0.0,
            std::f32::consts::FRAC_PI_2,
            0.3,
        );
        assert!(same_facing(a, b, 1e-3, "").is_ok());
        assert!(same_facing(a, Quat::IDENTITY, 1e-3, "").is_err());
    }

    #[test]
    fn all_six_relations() {
        for (rel, a, b, want) in [
            (Relation::Eq, 2, 2, true),
            (Relation::Eq, 2, 3, false),
            (Relation::Ne, 2, 3, true),
            (Relation::Ne, 2, 2, false),
            (Relation::Lt, 2, 3, true),
            (Relation::Lt, 3, 3, false),
            (Relation::Le, 3, 3, true),
            (Relation::Le, 4, 3, false),
            (Relation::Gt, 4, 3, true),
            (Relation::Gt, 3, 3, false),
            (Relation::Ge, 3, 3, true),
            (Relation::Ge, 2, 3, false),
        ] {
            assert_eq!(relate(a, rel, b, "").is_ok(), want, "{a} {} {b}", rel.symbol());
        }
        assert_eq!(
            relate(1, Relation::Gt, 2, "n").expect_err("fails"),
            "n: expected 1 > 2"
        );
    }

    #[test]
    fn vector_and_matrix_failure_text_is_deterministic() {
        let err = near_vec3(Vec3::new(0.0, 1.0, 0.0), Vec3::ZERO, 0.1, "pos").expect_err("fails");
        assert_eq!(
            err,
            "pos: Vec3 expected [0.0, 0.0, 0.0] within 0.1, got [0.0, 1.0, 0.0] (largest difference 1)"
        );
        assert!(near_mat4(&Mat4::IDENTITY, &Mat4::IDENTITY, 0.0, "").is_ok());
        assert!(near_mat4(&Mat4::IDENTITY, &Mat4::from_scale(Vec3::splat(2.0)), 0.5, "").is_err());
    }

    #[test]
    fn transform_checks_each_part() {
        let a = Transform::from_translation(Vec3::X);
        let b = Transform::from_translation(Vec3::X * 2.0);
        assert!(near_transform(&a, &a, 0.0, "t").is_ok());
        let err = near_transform(&a, &b, 0.5, "t").expect_err("fails");
        assert!(err.starts_with("t translation: Vec3"), "{err}");
    }
}
