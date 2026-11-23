// SPDX-License-Identifier: MPL-2.0
//! Pure math (`docs/specs/10-core.md` § 1.1): glam re-exports plus the engine's bounding volumes,
//! colour, curves, easing, isometries and a seedable random number generator.
//!
//! Conventions (`docs/02-architecture.md` § 3): right-handed, +Y up, forward −Z, radians, and
//! reversed-Z depth with an infinite far plane. Matrices are column-major and transform column vectors.

mod bounds;
mod color;
mod curve;
pub mod det;
pub mod detmath;
pub mod easing;
pub mod geom;
pub mod hash;
mod isometry;
mod rng;
pub mod wake;

pub use glam::{
    Affine2, Affine3A, EulerRot, IVec2, IVec3, IVec4, Mat2, Mat3, Mat3A, Mat4, Quat, UVec2, UVec3, UVec4,
    Vec2, Vec3, Vec3A, Vec4,
};

pub use bounds::{Aabb, Frustum, Plane, Ray, Rect, Sphere};
pub use color::{Color, ColorParseError, linear_to_srgb, srgb_to_linear};
pub use curve::{Curve, Interp, Interpolate, Keyframe};
pub use det::{det_asin, det_atan, det_log10, det_nlerp, det_quat_from_yaw_pitch_roll, det_yaw_pitch_roll};
pub use easing::Easing;
pub use isometry::Isometry;
pub use rng::{
    Rng, SampleRange, det_acos, det_acos_approx, det_atan2, det_cbrt, det_cbrt_f32, det_cos, det_exp,
    det_exp_f64, det_ln, det_ln_f64, det_powf, det_powf_f64, det_powf_nonnegative, det_sin, det_sin_cos,
    det_sin_cos_f32, det_slerp, det_tan, det_yaw_of, det_yaw_quat,
};

pub use detmath::{det_quat_from_axis_angle, det_quat_from_scaled_axis};

/// Linear interpolation: `a` at `t = 0`, `b` at `t = 1`. `t` is not clamped.
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Inverse of [`lerp`]: where `value` lies between `a` and `b` (0 at `a`, 1 at `b`, unclamped).
/// Returns 0 when `a == b`.
#[inline]
pub fn inverse_lerp(a: f32, b: f32, value: f32) -> f32 {
    let span = b - a;
    if span == 0.0 { 0.0 } else { (value - a) / span }
}

/// Maps `value` from the range `from` to the range `to`, unclamped. A degenerate `from` range maps to
/// `to.0`.
#[inline]
pub fn remap(value: f32, from: (f32, f32), to: (f32, f32)) -> f32 {
    lerp(to.0, to.1, inverse_lerp(from.0, from.1, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_inverse_lerp_remap() {
        assert_eq!(lerp(2.0, 6.0, 0.25), 3.0);
        assert_eq!(lerp(2.0, 6.0, 1.5), 8.0);
        assert_eq!(inverse_lerp(2.0, 6.0, 3.0), 0.25);
        assert_eq!(inverse_lerp(1.0, 1.0, 3.0), 0.0);
        assert_eq!(remap(5.0, (0.0, 10.0), (100.0, 200.0)), 150.0);
        assert_eq!(remap(-1.0, (0.0, 10.0), (0.0, 1.0)), -0.1);
        assert_eq!(remap(3.0, (2.0, 2.0), (7.0, 9.0)), 7.0);
    }
}
