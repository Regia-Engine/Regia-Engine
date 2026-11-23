// SPDX-License-Identifier: MPL-2.0
//! View and projection inputs that stay finite for every camera (ADR-869).
//!
//! A right-handed look-at builds its side axis as `normalize(forward × up)`. When the view
//! direction is parallel to `up` (a camera looking straight down, a sun at the zenith) that
//! cross product is zero and the whole matrix is NaN; when the eye sits on its target the
//! forward axis is NaN. A non-finite far plane, a non-positive near plane or a field of view
//! outside `(0, π)` make the perspective matrix NaN or infinite in the same way. Every NaN in
//! a view-projection reaches each pass of the frame (depth, clusters, cascades, TAA history).
//! [`look_at_view`] and [`usable_projection`] are the renderer's guards; for ordinary inputs
//! they return exactly what the plain glam functions return.

use regia_core::glam::camera::rh::view::look_to_mat4;
use regia_core::glam::{Mat4, Vec3};

/// The far plane used for a camera whose `far` is `f32::INFINITY`: 100 km.
///
/// The frame's depth is standard Z (near 0, far 1, `LessEqual`), so an infinite far plane is
/// stood in for by a finite one. Sun cascades already cap their own reach (`csm_far`).
pub const INFINITE_FAR_STANDIN: f32 = 100_000.0;

/// The near plane used when a camera's `near` is not a finite positive value (meters): the
/// [`crate::Camera`] default.
pub const FALLBACK_NEAR: f32 = 0.1;

/// The farthest far plane a perspective matrix is built with (meters). Beyond it the
/// matrix's `near × far` term can overflow `f32`.
pub const MAX_FAR: f32 = 1.0e9;

/// The near plane's range (meters): 1 µm to 1000 km.
const NEAR_RANGE: (f32, f32) = (1.0e-6, 1.0e6);

/// Squared length below which `forward × up` counts as zero (the two are within about
/// 1e-4 rad of parallel).
const PARALLEL_EPS: f32 = 1.0e-8;

/// The up vector a look-at along `forward` should use: `up` unless the two are parallel.
///
/// With `up` along ±Y (every ordinary camera) a view straight down takes −Z as its screen
/// up and a view straight up takes +Z: the limit of a camera that pitches from looking along
/// −Z, so the image does not spin at the pole. Any other parallel pair takes the world axis
/// least aligned with `forward`. A zero or non-finite `up` counts as +Y.
#[must_use]
pub fn stable_look_up(forward: Vec3, up: Vec3) -> Vec3 {
    let up = if up.is_finite() {
        up.normalize_or(Vec3::Y)
    } else {
        Vec3::Y
    };
    let f = if forward.is_finite() {
        forward.normalize_or_zero()
    } else {
        Vec3::ZERO
    };
    if f == Vec3::ZERO || f.cross(up).length_squared() >= PARALLEL_EPS {
        return up;
    }
    if up.y.abs() > 0.99 {
        return if f.y * up.y < 0.0 { Vec3::NEG_Z } else { Vec3::Z };
    }
    let a = f.abs();
    if a.x <= a.y && a.x <= a.z {
        Vec3::X
    } else if a.y <= a.z {
        Vec3::Y
    } else {
        Vec3::Z
    }
}

/// A right-handed look-at view matrix that is finite for every finite input, and for a
/// non-finite eye or target.
///
/// It equals glam's `look_at_mat4(eye, target, up)` whenever that is finite. An eye on its
/// target (or a non-finite direction) looks along −Z; a non-finite eye is taken as the
/// origin; a direction parallel to `up` uses [`stable_look_up`].
#[must_use]
pub fn look_at_view(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
    let dir = target - eye;
    let forward = if dir.is_finite() && dir.length_squared() > 1.0e-20 {
        dir.normalize()
    } else {
        Vec3::NEG_Z
    };
    let eye = if eye.is_finite() { eye } else { Vec3::ZERO };
    look_to_mat4(eye, forward, stable_look_up(forward, up))
}

/// Field of view, near and far planes a perspective matrix can be built from.
///
/// Returns `(fov_y, near, far)`, unchanged when they are already usable:
/// - `fov_y` finite and in `(1e-4, π − 1e-4)`; NaN takes π/4, anything else is clamped to
///   that range;
/// - `near` finite and positive, clamped to 1 µm ..= 1000 km; NaN, an infinity or a value
///   `<= 0` takes [`FALLBACK_NEAR`];
/// - `far` finite and more than 0.1 % beyond `near`, capped at [`MAX_FAR`]; `+∞` takes
///   [`INFINITE_FAR_STANDIN`] (at least twice `near`), and anything else (NaN, `−∞`,
///   `far <= near`) takes `near × 1e4` (capped at [`MAX_FAR`]).
///
/// Every result builds a finite perspective matrix for any finite positive aspect ratio.
#[must_use]
pub fn usable_projection(fov_y: f32, near: f32, far: f32) -> (f32, f32, f32) {
    const MIN_FOV: f32 = 1.0e-4;
    let fov_y = if fov_y.is_nan() {
        std::f32::consts::FRAC_PI_4
    } else if fov_y > MIN_FOV && fov_y < std::f32::consts::PI - MIN_FOV {
        fov_y
    } else {
        fov_y.clamp(MIN_FOV, std::f32::consts::PI - MIN_FOV)
    };
    let near = if near.is_finite() && near > 0.0 {
        near.clamp(NEAR_RANGE.0, NEAR_RANGE.1)
    } else {
        FALLBACK_NEAR
    };
    let far = if far.is_finite() && far > near * 1.001 {
        far.min(MAX_FAR)
    } else if far == f32::INFINITY {
        INFINITE_FAR_STANDIN.max(near * 2.0)
    } else {
        (near * 1.0e4).min(MAX_FAR)
    };
    (fov_y, near, far)
}

/// Shortest distance [`look_reach`] returns, metres.
const MIN_LOOK_REACH: f32 = 1.0;

/// Longest distance [`look_reach`] starts from before rounding up to a power of two (the result is
/// at most 2^60): its square stays finite in `f32`, so `(target − eye).length_squared()` does too.
const MAX_LOOK_REACH: f32 = 1.0e18;

/// How far ahead of `eye` [`look_target`] puts the look-at point: the smallest power of two at
/// least four times the eye's largest coordinate, and at least one metre (ADR-1488).
///
/// A look-at point carries the view direction as `target − eye`. Each coordinate of `target` is
/// rounded to the `f32` grid at its own magnitude, so the direction comes back off by up to half
/// that grid step divided by the reach. One metre ahead of an eye 2 km from the origin that is
/// 1.2e-4 rad, and the direction moved in those steps: a slow pan ticked by a pixel at a time.
/// A reach of at least four times the eye's coordinates keeps the error near `f32` rounding of a
/// unit vector (below 2.5e-7 rad). The power of two makes `forward × reach` exact.
#[must_use]
pub fn look_reach(eye: Vec3) -> f32 {
    let size = eye.abs().max_element();
    if !size.is_finite() {
        return MIN_LOOK_REACH;
    }
    let reach = (size * 4.0).clamp(MIN_LOOK_REACH, MAX_LOOK_REACH);
    // The power of two at or below `reach` (mantissa cleared), doubled when `reach` is above it.
    let floor = f32::from_bits(reach.to_bits() & 0xff80_0000);
    if floor < reach { floor * 2.0 } else { floor }
}

/// A look-at point on the ray from `eye` along `forward` that keeps the direction to `f32`
/// precision: `eye + normalize(forward) × look_reach(eye)`.
///
/// Use this, not `eye + forward`, wherever a view is handed on as an eye and a look-at point
/// ([`crate::ExtractedCamera`], [`crate::FrameScene`]): every consumer rebuilds the direction as
/// `normalize(target − eye)`, and a point one unit ahead of an eye far from the origin quantizes
/// it (ADR-1488). A zero or non-finite `forward` looks along −Z; a non-finite `eye` takes a reach
/// of one metre and is left to the consumers' own guards ([`look_at_view`]).
#[must_use]
pub fn look_target(eye: Vec3, forward: Vec3) -> Vec3 {
    let forward = if forward.is_finite() {
        forward.normalize_or(Vec3::NEG_Z)
    } else {
        Vec3::NEG_Z
    };
    eye + forward * look_reach(eye)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::camera::rh::proj::directx::perspective;
    use regia_core::glam::camera::rh::view::look_at_mat4;

    fn finite(m: Mat4) -> bool {
        m.to_cols_array().iter().all(|v| v.is_finite())
    }

    #[test]
    fn ordinary_views_match_glam_exactly() {
        for (eye, target) in [
            (Vec3::new(0.0, 2.0, 4.0), Vec3::ZERO),
            (Vec3::new(-3.0, 1.0, 0.5), Vec3::new(2.0, 0.0, -7.0)),
            (Vec3::new(1.0, 50.0, 1.0), Vec3::new(1.0, 0.0, 0.0)),
        ] {
            assert_eq!(
                look_at_view(eye, target, Vec3::Y),
                look_at_mat4(eye, target, Vec3::Y)
            );
        }
    }

    #[test]
    fn straight_down_and_up_views_are_finite_and_continuous() {
        let eye = Vec3::new(3.0, 10.0, -2.0);
        // glam's own matrix is NaN here: forward × up is zero.
        assert!(!finite(look_at_mat4(eye, eye - Vec3::Y, Vec3::Y)));
        let down = look_at_view(eye, eye - Vec3::Y, Vec3::Y);
        assert!(finite(down));
        // The eye maps to the view origin and the target lies straight ahead (−Z in view).
        assert!(down.transform_point3(eye).length() < 1e-4);
        let ahead = down.transform_point3(eye - Vec3::Y * 5.0);
        assert!((ahead - Vec3::new(0.0, 0.0, -5.0)).length() < 1e-4, "{ahead}");
        // Screen up is world −Z: the limit of a camera pitching down from looking along −Z.
        let nearly = look_at_view(eye, eye + Vec3::new(0.0, -1.0, -1.0e-3), Vec3::Y);
        let up_row = |m: Mat4| Vec3::new(m.x_axis.y, m.y_axis.y, m.z_axis.y);
        assert!((up_row(down) - up_row(nearly)).length() < 1e-2);
        assert!((up_row(down) - Vec3::NEG_Z).length() < 1e-6);
        let sky = look_at_view(eye, eye + Vec3::Y, Vec3::Y);
        assert!(finite(sky));
        assert!((up_row(sky) - Vec3::Z).length() < 1e-6);
    }

    #[test]
    fn degenerate_eye_and_target_are_finite() {
        let p = Vec3::new(1.0, 2.0, 3.0);
        assert!(!finite(look_at_mat4(p, p, Vec3::Y)));
        assert!(finite(look_at_view(p, p, Vec3::Y)));
        assert!(finite(look_at_view(Vec3::splat(f32::NAN), p, Vec3::Y)));
        assert!(finite(look_at_view(
            p,
            Vec3::new(f32::INFINITY, 0.0, 0.0),
            Vec3::Y
        )));
        assert!(finite(look_at_view(p, p + Vec3::X, Vec3::ZERO)));
        assert!(finite(look_at_view(p, p + Vec3::Z, Vec3::Z)));
        assert!(finite(look_at_view(
            p,
            p - Vec3::X,
            Vec3::new(f32::NAN, 1.0, 0.0)
        )));
    }

    #[test]
    fn usable_projection_keeps_good_values_and_fixes_bad_ones() {
        assert_eq!(usable_projection(1.0, 0.1, 1000.0), (1.0, 0.1, 1000.0));
        assert_eq!(
            usable_projection(1.0, 0.1, f32::INFINITY),
            (1.0, 0.1, INFINITE_FAR_STANDIN)
        );
        assert_eq!(usable_projection(1.0, 0.0, 50.0), (1.0, FALLBACK_NEAR, 50.0));
        assert_eq!(usable_projection(1.0, -1.0, 50.0).1, FALLBACK_NEAR);
        assert_eq!(usable_projection(1.0, f32::NAN, 50.0).1, FALLBACK_NEAR);
        assert_eq!(usable_projection(1.0, 2.0, 1.0), (1.0, 2.0, 2.0e4));
        assert_eq!(usable_projection(1.0, 0.1, f32::NAN).2, 1000.0);
        assert_eq!(usable_projection(1.0, 0.1, f32::NEG_INFINITY).2, 1000.0);
        assert_eq!(
            usable_projection(f32::NAN, 0.1, 10.0).0,
            std::f32::consts::FRAC_PI_4
        );
        assert_eq!(usable_projection(0.0, 0.1, 10.0).0, 1.0e-4);
        assert_eq!(
            usable_projection(f32::INFINITY, 0.1, 10.0).0,
            std::f32::consts::PI - 1.0e-4
        );
        // A huge near plane still gets a finite far.
        let (_, near, far) = usable_projection(1.0, 5.0e5, f32::INFINITY);
        assert_eq!(near, 5.0e5);
        assert!(far.is_finite() && far > near);
        assert_eq!(usable_projection(1.0, 1.0e-30, 10.0).1, 1.0e-6);
        assert_eq!(usable_projection(1.0, f32::MAX, 10.0).1, 1.0e6);
        assert_eq!(usable_projection(1.0, 0.1, 1.0e30).2, MAX_FAR);
    }

    #[test]
    fn every_usable_projection_is_a_finite_matrix() {
        let values = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            1.0e-30,
            0.1,
            1.0,
            1.0e5,
            1.0e30,
            f32::MAX,
            f32::MIN_POSITIVE,
        ];
        for fov in values {
            for near in values {
                for far in values {
                    let (fov, near, far) = usable_projection(fov, near, far);
                    let m = perspective(fov, 16.0 / 9.0, near, far);
                    assert!(finite(m), "fov {fov} near {near} far {far}: {m}");
                }
            }
        }
    }

    /// Angle between two directions, radians, in f64 (the cross product keeps small angles exact;
    /// `acos` of an f32 dot product cannot resolve below about 3e-4 rad).
    fn angle(a: Vec3, b: Vec3) -> f64 {
        let (a, b) = (a.as_dvec3().normalize(), b.as_dvec3().normalize());
        a.cross(b).length().atan2(a.dot(b))
    }

    #[test]
    fn look_reach_is_a_power_of_two_four_times_the_eye() {
        assert_eq!(look_reach(Vec3::ZERO), 1.0);
        assert_eq!(look_reach(Vec3::splat(0.1)), 1.0);
        assert_eq!(
            look_reach(Vec3::new(0.0, 0.0, -256.0)),
            1024.0,
            "exact powers stay"
        );
        // The s4 showcase eye (ADR-1488): 2203 × 4 = 8812 → 16384.
        assert_eq!(look_reach(Vec3::new(-1752.0, 353.2, -2203.0)), 16384.0);
        assert_eq!(look_reach(Vec3::new(f32::NAN, 0.0, 0.0)), 1.0);
        assert_eq!(look_reach(Vec3::new(f32::INFINITY, 0.0, 0.0)), 1.0);
        let huge = look_reach(Vec3::splat(f32::MAX));
        assert_eq!(huge, 2.0f32.powi(60));
        assert!((huge * huge).is_finite());
    }

    #[test]
    fn a_far_look_target_keeps_the_direction_where_one_unit_ahead_quantizes_it() {
        // An eye 2 km from the origin, as in the s4 showcase shot. One unit ahead, each target
        // coordinate rounds to 1.2e-4 (x) and 2.4e-4 (z): the direction comes back up to ~1e-4
        // rad off. `look_target` keeps it within f32 rounding of a unit vector.
        let eye = Vec3::new(-1758.69, 354.7, -2195.57);
        let (mut worst_unit, mut worst_far) = (0.0f64, 0.0f64);
        for i in 0..2000 {
            let yaw = 0.7 + f64::from(i) * 1.0e-6;
            let pitch = 0.0536 + f64::from(i) * 3.0e-8;
            let forward =
                regia_core::glam::DVec3::new(yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos())
                    .as_vec3();
            let unit = (eye + forward) - eye;
            let far = look_target(eye, forward) - eye;
            worst_unit = worst_unit.max(angle(unit, forward));
            worst_far = worst_far.max(angle(far, forward));
        }
        assert!(
            worst_unit > 5.0e-5,
            "the witness: one unit ahead is off by {worst_unit:e} rad"
        );
        assert!(worst_far < 2.5e-7, "look_target is off by {worst_far:e} rad");
    }

    #[test]
    fn a_far_look_target_turns_smoothly_with_its_forward() {
        // A pan of 1e-6 rad a frame (a 4K, 20° frame has ~1.6e-4 rad pixels): the view built
        // from `look_target` turns by the same step every frame, within f32 rounding.
        let eye = Vec3::new(-1765.38, 356.2, -2188.14);
        let mut last: Option<Vec3> = None;
        for i in 0..1000 {
            let yaw = 0.83 - f64::from(i) * 1.0e-6;
            let forward = regia_core::glam::DVec3::new(yaw.sin(), 0.05, yaw.cos())
                .normalize()
                .as_vec3();
            let view = look_at_view(eye, look_target(eye, forward), Vec3::Y);
            // Row 2 of the view's rotation is −forward.
            let seen = -Vec3::new(view.x_axis.z, view.y_axis.z, view.z_axis.z);
            if let Some(last) = last {
                let step = angle(last, seen);
                assert!((step - 1.0e-6).abs() < 5.0e-7, "frame {i}: turned {step:e} rad");
            }
            last = Some(seen);
        }
    }

    #[test]
    fn look_target_falls_back_for_degenerate_input() {
        let eye = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(look_target(eye, Vec3::ZERO), eye + Vec3::NEG_Z * look_reach(eye));
        assert_eq!(
            look_target(eye, Vec3::splat(f32::NAN)),
            eye + Vec3::NEG_Z * look_reach(eye)
        );
        assert!(
            look_at_view(
                Vec3::splat(f32::NAN),
                look_target(Vec3::splat(f32::NAN), Vec3::X),
                Vec3::Y
            )
            .to_cols_array()
            .iter()
            .all(|v| v.is_finite())
        );
        // Near the origin the point stays a metre ahead, as the old `eye + forward` did.
        assert_eq!(look_target(Vec3::ZERO, Vec3::X * 3.0), Vec3::X);
    }
}
