// SPDX-License-Identifier: MPL-2.0
//! Pure math of the game widgets: where a world point lands on a screen layer, where a blip lands
//! on a radar, and where to aim to hit a moving target. No `World`, so every rule is tested here
//! without an app.
//!
//! Coordinates: world +Y is up and the ground plane is X/Z; "north" is world −Z (where a default
//! camera looks). Screen and radar positions are in UI px, +X right and +Y down.

use std::f32::consts::TAU;

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::{Affine3A, Mat4, Quat, Vec3, Vec4};
use regia_render::Camera;
use regia_render::view_math::{look_at_view, look_target, usable_projection};

/// The camera a marker layer projects through: the matrices the frame itself draws with (the view
/// from the transform's −Z axis and world +Y, so a roll is not applied; `specs/13` § 9), so a marker
/// sits on the pixel of its world point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewProjection {
    clip_from_world: Mat4,
    /// World position of the eye.
    pub eye: Vec3,
    /// `1 / tan(fov_y / 2)`: UI px per metre at depth 1, for half the layer's height.
    pub focal: f32,
}

impl ViewProjection {
    /// The view of `camera` at world transform `world`, for a layer of width over height `aspect`.
    #[must_use]
    pub fn new(camera: &Camera, world: &Affine3A, aspect: f32) -> Self {
        let (_, rotation, translation) = world.to_scale_rotation_translation();
        Self::look(
            translation,
            rotation,
            camera.fov_y,
            camera.near,
            camera.far,
            aspect,
        )
    }

    /// A view from `eye` looking along `rotation`'s −Z.
    #[must_use]
    pub fn look(eye: Vec3, rotation: Quat, fov_y: f32, near: f32, far: f32, aspect: f32) -> Self {
        let forward = rotation * Vec3::NEG_Z;
        let (fov_y, near, far) = usable_projection(fov_y, near, far);
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let view = look_at_view(eye, look_target(eye, forward), Vec3::Y);
        Self {
            clip_from_world: perspective(fov_y, aspect, near, far) * view,
            eye,
            focal: 1.0 / (fov_y * 0.5).tan(),
        }
    }

    /// Clip-space position of `point`.
    #[must_use]
    pub fn clip(&self, point: Vec3) -> Vec4 {
        self.clip_from_world * point.extend(1.0)
    }
}

/// Where a world point shows on a layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenAnchor {
    /// Position on the layer in UI px (on the point, or on the edge when [`Self::clamped`]).
    pub pos: [f32; 2],
    /// The point itself is inside the layer and in front of the camera.
    pub on_screen: bool,
    /// The point is behind the camera.
    pub behind: bool,
    /// The position was moved onto the edge margin.
    pub clamped: bool,
    /// Depth along the view axis in metres (negative behind the camera).
    pub depth: f32,
    /// Straight-line distance from the eye in metres.
    pub distance: f32,
}

/// Where `point` lands on a layer of `size` UI px. An off-screen point (or one behind the camera)
/// is placed where the line from the layer's center toward it leaves the rect inset by `margin`
/// when `clamp` is set; without `clamp` its `pos` is meaningless and `on_screen` says so. A non-finite
/// point or an empty layer answers `None`.
#[must_use]
pub fn anchor_on_layer(
    view: &ViewProjection,
    point: Vec3,
    size: [f32; 2],
    margin: f32,
    clamp: bool,
) -> Option<ScreenAnchor> {
    if !point.is_finite() || size[0] <= 0.0 || size[1] <= 0.0 {
        return None;
    }
    let clip = view.clip(point);
    if !clip.is_finite() {
        return None;
    }
    let distance = point.distance(view.eye);
    let depth = clip.w;
    let behind = depth <= 1e-4;
    let half = [size[0] * 0.5, size[1] * 0.5];
    if !behind {
        let (x, y) = (clip.x / depth, clip.y / depth);
        if x.abs() <= 1.0 && y.abs() <= 1.0 {
            return Some(ScreenAnchor {
                pos: [half[0] * (x + 1.0), half[1] * (1.0 - y)],
                on_screen: true,
                behind: false,
                clamped: false,
                depth,
                distance,
            });
        }
    }
    let mut pos = [0.0, 0.0];
    if clamp {
        // Clip x/y keep the view-space direction on both sides of the eye, so a point behind the
        // camera points the way the player has to turn. A point straight behind has no direction
        // and goes to the bottom edge.
        let dir = [clip.x * half[0], -clip.y * half[1]];
        let len = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
        let dir = if len > 1e-6 {
            [dir[0] / len, dir[1] / len]
        } else {
            [0.0, 1.0]
        };
        let box_half = [(half[0] - margin).max(0.0), (half[1] - margin).max(0.0)];
        let tx = if dir[0].abs() > 1e-6 {
            box_half[0] / dir[0].abs()
        } else {
            f32::INFINITY
        };
        let ty = if dir[1].abs() > 1e-6 {
            box_half[1] / dir[1].abs()
        } else {
            f32::INFINITY
        };
        let t = tx.min(ty);
        pos = [half[0] + dir[0] * t, half[1] + dir[1] * t];
    }
    Some(ScreenAnchor {
        pos,
        on_screen: false,
        behind,
        clamped: clamp,
        depth,
        distance,
    })
}

/// The radius in UI px that a sphere of `radius_m` at view depth `depth` covers on a layer `height`
/// UI px tall (small-angle pinhole projection).
#[must_use]
pub fn projected_radius(view: &ViewProjection, radius_m: f32, depth: f32, height: f32) -> f32 {
    if depth.is_nan() || depth <= 1e-4 || !radius_m.is_finite() {
        return 0.0;
    }
    (radius_m.max(0.0) / depth) * view.focal * height * 0.5
}

/// A solved intercept: when a projectile fired now meets the target, and where to aim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeadSolution {
    /// Flight time in seconds.
    pub time: f32,
    /// World point to aim at (the target's future position in the shooter's frame, raised to
    /// counter gravity's drop).
    pub aim_point: Vec3,
}

/// Earliest time `t > 0` at which a projectile of constant `speed` launched from the origin meets a
/// point at `rel_pos` moving at `rel_vel`: the smallest positive root of
/// `(v·v − s²) t² + 2 (r·v) t + r·r = 0`. `None` when no such time exists (a target outrunning the
/// projectile, or a speed that is not finite and positive). A target at the origin answers `0`.
#[must_use]
pub fn intercept_time(rel_pos: Vec3, rel_vel: Vec3, speed: f32) -> Option<f32> {
    if !speed.is_finite() || speed <= 0.0 || !rel_pos.is_finite() || !rel_vel.is_finite() {
        return None;
    }
    let c = rel_pos.dot(rel_pos);
    if c <= 1e-12 {
        return Some(0.0);
    }
    let a = rel_vel.dot(rel_vel) - speed * speed;
    let b = 2.0 * rel_pos.dot(rel_vel);
    if a.abs() <= 1e-6 * speed * speed {
        // Target and projectile are equally fast: the quadratic degenerates to a line.
        return (b < 0.0).then(|| -c / b).filter(|t| t.is_finite() && *t > 0.0);
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    // Citardauq form: the other root comes from c / q, which stays accurate when |4ac| << b².
    let q = -0.5 * (b + b.signum_or_one() * disc.sqrt());
    let (r0, r1) = (q / a, if q != 0.0 { c / q } else { f32::NAN });
    [r0, r1]
        .into_iter()
        .filter(|t| t.is_finite() && *t > 0.0)
        .min_by(f32::total_cmp)
}

trait SignumOrOne {
    fn signum_or_one(self) -> f32;
}

impl SignumOrOne for f32 {
    /// `-1` for negative values, `1` otherwise (so `-0.5 (b + sign(b) √disc)` never cancels).
    fn signum_or_one(self) -> f32 {
        if self < 0.0 { -1.0 } else { 1.0 }
    }
}

/// The aim point against a moving target. `speed` is the muzzle speed relative to the shooter (so
/// the projectile inherits the shooter's velocity); `gravity` is the downward acceleration in m/s²
/// (`0` for a flat trajectory), countered by raising the aim point `½ g t²`, which is the first-order
/// answer for a flat shot. `None` when [`intercept_time`] has none.
#[must_use]
pub fn lead_solution(
    shooter: Vec3,
    shooter_velocity: Vec3,
    target: Vec3,
    target_velocity: Vec3,
    speed: f32,
    gravity: f32,
) -> Option<LeadSolution> {
    let rel_vel = target_velocity - shooter_velocity;
    let time = intercept_time(target - shooter, rel_vel, speed)?;
    let drop = if gravity.is_finite() {
        0.5 * gravity.max(0.0) * time * time
    } else {
        0.0
    };
    Some(LeadSolution {
        time,
        aim_point: target + rel_vel * time + Vec3::Y * drop,
    })
}

/// How a radar turns world positions into widget positions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarFrame {
    /// World `[x, z]` at the radar's center.
    pub center: [f32; 2],
    /// Radians clockwise from north to the widget's "up" (0 keeps north up).
    pub heading: f32,
    /// World metres from the center to the widget's edge, for `[x, y]`.
    pub range: [f32; 2],
    /// Widget half-size in UI px.
    pub half_px: [f32; 2],
    /// The edge is a circle (else a rectangle).
    pub circular: bool,
}

/// Where a blip lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarPoint {
    /// Position from the widget's top-left in UI px (on the rim when out of range and clamped).
    pub pos: [f32; 2],
    /// The point is inside the range.
    pub inside: bool,
}

/// Places the world point `world` (`[x, z]`) on a radar: north up, turned by the frame's heading.
/// A point out of range goes on the rim along its bearing with `clamp`; without it its `pos` is
/// still where it would be, and `inside` is false. A non-finite point or an empty frame is `None`.
#[must_use]
pub fn radar_point(frame: &RadarFrame, world: [f32; 2], clamp: bool) -> Option<RadarPoint> {
    if !world[0].is_finite()
        || !world[1].is_finite()
        || frame.range[0] <= 0.0
        || frame.range[1] <= 0.0
        || frame.half_px[0] <= 0.0
        || frame.half_px[1] <= 0.0
    {
        return None;
    }
    let (dx, dz) = (world[0] - frame.center[0], world[1] - frame.center[1]);
    let (sin, cos) = frame.heading.rem_euclid(TAU).sin_cos();
    // Rotate the world offset so the heading points up (−Y on the widget).
    let along_x = dx * cos + dz * sin;
    let along_y = -dx * sin + dz * cos;
    // In units of the range: 1 is the edge.
    let (mut u, mut v) = (along_x / frame.range[0], along_y / frame.range[1]);
    let inside;
    if frame.circular {
        let len = (u * u + v * v).sqrt();
        inside = len <= 1.0;
        if !inside && clamp {
            u /= len;
            v /= len;
        }
    } else {
        let peak = u.abs().max(v.abs());
        inside = peak <= 1.0;
        if !inside && clamp {
            u /= peak;
            v /= peak;
        }
    }
    Some(RadarPoint {
        pos: [frame.half_px[0] * (1.0 + u), frame.half_px[1] * (1.0 + v)],
        inside,
    })
}

/// The heading of a world rotation: radians clockwise from north (−Z) of its −Z axis on the ground
/// plane; `0` for a rotation looking straight up or down, or one that is not finite (the rotation
/// of a transform scaled to zero).
#[must_use]
pub fn heading_of(rotation: Quat) -> f32 {
    let forward = rotation * Vec3::NEG_Z;
    if !forward.is_finite() || (forward.x.abs() < 1e-5 && forward.z.abs() < 1e-5) {
        return 0.0;
    }
    forward.x.atan2(-forward.z).rem_euclid(TAU)
}

/// The sub-rect of a map image covering `center ± range`, for an image covering `world_rect`
/// (`[min_x, min_z, max_x, max_z]`). `[u0, v0, u1, v1]` in the image's UV space (the shape
/// [`crate::UiImage::uv`] takes). An empty `world_rect` shows the whole image.
#[must_use]
pub fn map_window(center: [f32; 2], range: [f32; 2], world_rect: [f32; 4]) -> [f32; 4] {
    let w = world_rect[2] - world_rect[0];
    let h = world_rect[3] - world_rect[1];
    if !(w > 0.0 && h > 0.0) {
        return [0.0, 0.0, 1.0, 1.0];
    }
    [
        (center[0] - range[0] - world_rect[0]) / w,
        (center[1] - range[1] - world_rect[1]) / h,
        (center[0] + range[0] - world_rect[0]) / w,
        (center[1] + range[1] - world_rect[1]) / h,
    ]
}

/// Exponential smoothing of `value` toward `sample` over `dt` seconds with time constant `tau`:
/// frame-rate independent, so an estimate settles the same at 30 and 240 Hz.
#[must_use]
pub fn smooth_toward(value: Vec3, sample: Vec3, dt: f32, tau: f32) -> Vec3 {
    if dt.is_nan() || dt <= 0.0 || !sample.is_finite() {
        return value;
    }
    if tau.is_nan() || tau <= 0.0 {
        return sample;
    }
    value + (sample - value) * (1.0 - (-dt / tau).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    fn view() -> ViewProjection {
        // Eye at the origin looking down −Z with a 90° vertical field of view, square layer.
        ViewProjection::look(
            Vec3::ZERO,
            Quat::IDENTITY,
            std::f32::consts::FRAC_PI_2,
            0.1,
            1000.0,
            1.0,
        )
    }

    #[test]
    fn a_point_ahead_lands_on_the_center_and_the_edges_match_the_field_of_view() {
        let v = view();
        let center = anchor_on_layer(&v, Vec3::new(0.0, 0.0, -10.0), [200.0, 200.0], 0.0, false).unwrap();
        assert!(center.on_screen && !center.clamped);
        assert!(close(center.pos[0], 100.0, 1e-3) && close(center.pos[1], 100.0, 1e-3));
        assert!(close(center.depth, 10.0, 1e-3));
        // At 90° a point as far right as it is ahead is on the right edge; y up is up on screen.
        let right = anchor_on_layer(&v, Vec3::new(9.99, 0.0, -10.0), [200.0, 200.0], 0.0, false).unwrap();
        assert!(right.on_screen && close(right.pos[0], 199.9, 0.05), "{right:?}");
        let up = anchor_on_layer(&v, Vec3::new(0.0, 5.0, -10.0), [200.0, 200.0], 0.0, false).unwrap();
        assert!(close(up.pos[1], 50.0, 1e-3), "{up:?}");
    }

    #[test]
    fn off_screen_points_clamp_to_the_margin_toward_their_bearing() {
        let v = view();
        let far_right = anchor_on_layer(&v, Vec3::new(100.0, 0.0, -10.0), [200.0, 100.0], 8.0, true).unwrap();
        assert!(!far_right.on_screen && far_right.clamped && !far_right.behind);
        assert!(close(far_right.pos[0], 192.0, 1e-3), "{far_right:?}");
        assert!(close(far_right.pos[1], 50.0, 1e-3), "{far_right:?}");
        // Above the view: top edge, centered.
        let above = anchor_on_layer(&v, Vec3::new(0.0, 100.0, -10.0), [200.0, 100.0], 8.0, true).unwrap();
        assert!(close(above.pos[1], 8.0, 1e-3), "{above:?}");
        assert!(close(above.pos[0], 100.0, 1e-3), "{above:?}");
        // A corner bearing leaves through the nearer side, not the corner itself.
        let diag = anchor_on_layer(&v, Vec3::new(100.0, 100.0, -10.0), [200.0, 100.0], 0.0, true).unwrap();
        assert!(diag.pos[0] >= 0.0 && diag.pos[0] <= 200.0 && diag.pos[1] >= 0.0 && diag.pos[1] <= 100.0);
    }

    #[test]
    fn points_behind_the_camera_are_flagged_and_keep_their_side() {
        let v = view();
        let behind_right = anchor_on_layer(&v, Vec3::new(5.0, 0.0, 10.0), [200.0, 200.0], 0.0, true).unwrap();
        assert!(behind_right.behind && !behind_right.on_screen);
        assert!(
            behind_right.pos[0] > 100.0,
            "turn right to see it: {behind_right:?}"
        );
        // Exactly behind has no bearing: it goes to the bottom edge, not to NaN.
        let straight = anchor_on_layer(&v, Vec3::new(0.0, 0.0, 10.0), [200.0, 200.0], 0.0, true).unwrap();
        assert!(
            straight.pos[0].is_finite() && close(straight.pos[1], 200.0, 1e-3),
            "{straight:?}"
        );
        // Without clamp the marker is simply off.
        let off = anchor_on_layer(&v, Vec3::new(0.0, 0.0, 10.0), [200.0, 200.0], 0.0, false).unwrap();
        assert!(!off.on_screen && !off.clamped);
    }

    #[test]
    fn bad_input_answers_none_and_never_nan() {
        let v = view();
        assert!(anchor_on_layer(&v, Vec3::splat(f32::NAN), [100.0, 100.0], 0.0, true).is_none());
        assert!(anchor_on_layer(&v, Vec3::NEG_Z, [0.0, 100.0], 0.0, true).is_none());
        assert_eq!(projected_radius(&v, 1.0, 0.0, 100.0), 0.0);
        assert_eq!(projected_radius(&v, f32::NAN, 5.0, 100.0), 0.0);
    }

    #[test]
    fn a_rolled_camera_projects_like_the_frame_which_ignores_roll() {
        let roll = Quat::from_rotation_z(1.0);
        let rolled = ViewProjection::look(Vec3::ZERO, roll, 1.0, 0.1, 100.0, 1.0);
        let level = ViewProjection::look(Vec3::ZERO, Quat::IDENTITY, 1.0, 0.1, 100.0, 1.0);
        let p = Vec3::new(2.0, 1.0, -10.0);
        assert!(rolled.clip(p).abs_diff_eq(level.clip(p), 1e-4));
    }

    #[test]
    fn projected_radius_shrinks_with_depth_and_grows_with_the_layer() {
        let v = view();
        // 90°: focal 1; a 1 m sphere at 10 m on a 200 px layer covers 10 px.
        assert!(close(projected_radius(&v, 1.0, 10.0, 200.0), 10.0, 1e-3));
        assert!(close(projected_radius(&v, 1.0, 20.0, 200.0), 5.0, 1e-3));
        assert!(close(projected_radius(&v, 1.0, 10.0, 400.0), 20.0, 1e-3));
    }

    #[test]
    fn a_still_target_is_reached_in_distance_over_speed() {
        let t = intercept_time(Vec3::new(0.0, 0.0, -100.0), Vec3::ZERO, 50.0).unwrap();
        assert!(close(t, 2.0, 1e-5));
        assert_eq!(intercept_time(Vec3::ZERO, Vec3::X, 50.0), Some(0.0));
    }

    #[test]
    fn the_intercept_meets_the_target_for_every_geometry() {
        // Verify the definition itself: at time t the target is exactly speed * t away.
        let cases = [
            (Vec3::new(100.0, 0.0, 0.0), Vec3::new(0.0, 0.0, 20.0), 80.0),
            (Vec3::new(50.0, 10.0, -30.0), Vec3::new(-15.0, 2.0, 40.0), 120.0),
            (Vec3::new(0.0, 0.0, -500.0), Vec3::new(0.0, 0.0, 200.0), 900.0),
            (Vec3::new(5.0, 0.0, 0.0), Vec3::new(79.0, 0.0, 0.0), 80.0),
            // Closing head-on, near-equal speeds.
            (Vec3::new(100.0, 0.0, 0.0), Vec3::new(-80.0, 0.0, 0.0), 80.0),
        ];
        for (r, v, s) in cases {
            let t = intercept_time(r, v, s).unwrap_or_else(|| panic!("no intercept for {r} {v} {s}"));
            let gap = (r + v * t).length() - s * t;
            assert!(
                gap.abs() < 0.02 * r.length().max(1.0) / 10.0,
                "{r} {v} {s}: t {t} gap {gap}"
            );
            assert!(t > 0.0);
        }
    }

    #[test]
    fn a_target_that_outruns_the_projectile_has_no_solution() {
        assert_eq!(intercept_time(Vec3::X * 100.0, Vec3::X * 200.0, 100.0), None);
        // Equal speed, moving away: never.
        assert_eq!(intercept_time(Vec3::X * 100.0, Vec3::X * 100.0, 100.0), None);
        // A faster target that flies toward the shooter is met.
        assert!(intercept_time(Vec3::X * 100.0, Vec3::X * -200.0, 100.0).is_some());
        // A bad speed is no solution, not a NaN.
        assert_eq!(intercept_time(Vec3::X, Vec3::ZERO, 0.0), None);
        assert_eq!(intercept_time(Vec3::X, Vec3::ZERO, f32::NAN), None);
        assert_eq!(intercept_time(Vec3::X, Vec3::splat(f32::INFINITY), 10.0), None);
    }

    #[test]
    fn the_lead_uses_relative_motion_and_counters_gravity() {
        // A target crossing at 10 m/s, 100 m away, 100 m/s projectile.
        let flat = lead_solution(
            Vec3::ZERO,
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -100.0),
            Vec3::new(10.0, 0.0, 0.0),
            100.0,
            0.0,
        )
        .unwrap();
        assert!(flat.aim_point.x > 9.9 && flat.aim_point.x < 10.2, "{flat:?}");
        assert!(close(flat.aim_point.y, 0.0, 1e-5));
        // A shooter moving with the target needs no lead: same velocity, same aim as a still shot.
        let along = lead_solution(
            Vec3::ZERO,
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -100.0),
            Vec3::new(10.0, 0.0, 0.0),
            100.0,
            0.0,
        )
        .unwrap();
        assert!(close(along.aim_point.x, 0.0, 1e-4), "{along:?}");
        // Gravity raises the aim by ½ g t².
        let drop = lead_solution(
            Vec3::ZERO,
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -100.0),
            Vec3::ZERO,
            100.0,
            9.81,
        )
        .unwrap();
        assert!(close(drop.time, 1.0, 1e-5));
        assert!(close(drop.aim_point.y, 4.905, 1e-3), "{drop:?}");
    }

    fn frame(circular: bool, heading: f32) -> RadarFrame {
        RadarFrame {
            center: [10.0, 20.0],
            heading,
            range: [50.0, 50.0],
            half_px: [100.0, 100.0],
            circular,
        }
    }

    #[test]
    fn north_is_up_and_east_is_right_on_a_radar() {
        let f = frame(true, 0.0);
        let c = radar_point(&f, [10.0, 20.0], false).unwrap();
        assert_eq!(c.pos, [100.0, 100.0]);
        // 25 m north (−Z): halfway up.
        let n = radar_point(&f, [10.0, -5.0], false).unwrap();
        assert!(
            close(n.pos[0], 100.0, 1e-3) && close(n.pos[1], 50.0, 1e-3),
            "{n:?}"
        );
        // 25 m east (+X): halfway right.
        let e = radar_point(&f, [35.0, 20.0], false).unwrap();
        assert!(
            close(e.pos[0], 150.0, 1e-3) && close(e.pos[1], 100.0, 1e-3),
            "{e:?}"
        );
    }

    #[test]
    fn a_heading_turns_the_world_so_what_is_ahead_is_up() {
        // Facing east: a point to the east is ahead, so up.
        let f = frame(true, std::f32::consts::FRAC_PI_2);
        let ahead = radar_point(&f, [35.0, 20.0], false).unwrap();
        assert!(
            close(ahead.pos[0], 100.0, 1e-3) && close(ahead.pos[1], 50.0, 1e-3),
            "{ahead:?}"
        );
        // Facing east, south is on the right.
        let right = radar_point(&f, [10.0, 45.0], false).unwrap();
        assert!(
            close(right.pos[0], 150.0, 1e-3) && close(right.pos[1], 100.0, 1e-3),
            "{right:?}"
        );
    }

    #[test]
    fn out_of_range_points_clamp_onto_the_rim_or_stay_off() {
        let circle = frame(true, 0.0);
        let out = radar_point(&circle, [10.0 + 300.0, 20.0], true).unwrap();
        assert!(!out.inside && close(out.pos[0], 200.0, 1e-3), "{out:?}");
        let free = radar_point(&circle, [10.0 + 300.0, 20.0], false).unwrap();
        assert!(!free.inside && free.pos[0] > 200.0);
        // A square radar's corner is in range; the same point is outside a circle.
        let corner = [10.0 + 45.0, 20.0 + 45.0];
        assert!(radar_point(&frame(false, 0.0), corner, true).unwrap().inside);
        assert!(!radar_point(&circle, corner, true).unwrap().inside);
        // A square clamps to its own edge.
        let square = radar_point(&frame(false, 0.0), [10.0 + 200.0, 20.0 + 100.0], true).unwrap();
        assert!(
            close(square.pos[0], 200.0, 1e-3) && close(square.pos[1], 150.0, 1e-3),
            "{square:?}"
        );
    }

    #[test]
    fn a_bad_radar_frame_or_point_is_none() {
        let mut f = frame(true, 0.0);
        assert!(radar_point(&f, [f32::NAN, 0.0], true).is_none());
        f.range = [0.0, 0.0];
        assert!(radar_point(&f, [0.0, 0.0], true).is_none());
        f = frame(true, 0.0);
        f.half_px = [0.0, 10.0];
        assert!(radar_point(&f, [0.0, 0.0], true).is_none());
    }

    #[test]
    fn heading_reads_clockwise_from_north() {
        assert!(close(heading_of(Quat::IDENTITY), 0.0, 1e-5));
        // Rotating +90° about Y turns −Z toward −X: west, so 270° clockwise.
        let west = heading_of(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        assert!(close(west, 1.5 * std::f32::consts::PI, 1e-4), "{west}");
        let east = heading_of(Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2));
        assert!(close(east, std::f32::consts::FRAC_PI_2, 1e-4), "{east}");
        // Looking straight down has no heading.
        assert_eq!(
            heading_of(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
            0.0
        );
    }

    #[test]
    fn the_map_window_follows_the_center_and_an_empty_rect_shows_everything() {
        let uv = map_window([50.0, 50.0], [25.0, 25.0], [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(uv, [0.25, 0.25, 0.75, 0.75]);
        // East of the middle the window moves; it never loses its width.
        let east = map_window([75.0, 50.0], [25.0, 25.0], [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(east, [0.5, 0.25, 1.0, 0.75]);
        assert_eq!(
            map_window([0.0, 0.0], [1.0, 1.0], [0.0, 0.0, 0.0, 0.0]),
            [0.0, 0.0, 1.0, 1.0]
        );
    }

    /// A transform scaled to zero has a rotation of NaNs: nothing built on it may move a node to NaN.
    #[test]
    fn a_transform_scaled_to_zero_gives_no_nan() {
        let zero = Affine3A::from_scale(Vec3::ZERO);
        let (_, rotation, _) = zero.to_scale_rotation_translation();
        assert_eq!(heading_of(rotation), 0.0);
        let view = ViewProjection::new(&Camera::default(), &zero, 16.0 / 9.0);
        let anchor = anchor_on_layer(&view, Vec3::NEG_Z, [100.0, 100.0], 0.0, true);
        assert!(
            anchor.is_none_or(|a| a.pos[0].is_finite() && a.pos[1].is_finite()),
            "{anchor:?}"
        );
    }

    #[test]
    fn smoothing_is_frame_rate_independent() {
        let target = Vec3::X;
        let once = smooth_toward(Vec3::ZERO, target, 0.1, 0.1);
        let mut many = Vec3::ZERO;
        for _ in 0..10 {
            many = smooth_toward(many, target, 0.01, 0.1);
        }
        assert!(once.abs_diff_eq(many, 1e-4), "{once} {many}");
        assert_eq!(smooth_toward(Vec3::Y, target, 0.0, 0.1), Vec3::Y);
        assert_eq!(smooth_toward(Vec3::Y, target, 0.1, 0.0), target);
    }
}
