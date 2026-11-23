// SPDX-License-Identifier: MPL-2.0
//! The "photographic walk" lens preset: a short shutter, no blur on slow motion, and depth of
//! field only on focus pulls (ADR-2449, `book/agents/camera-photographic-walk.md`).
//!
//! A walk through a forest should not smear the nearby trunks. A [`PhotographicWalk`] on a camera
//! drives the camera's [`PhysicalCamera`]:
//!
//! - **Motion blur** is a short shutter ([`PhotographicWalk::shutter_fraction`], 0.12 of a frame
//!   by default, about a 43° shutter) scaled by how fast the camera moves: no blur below
//!   [`PhotographicWalk::blur_speed_min_m_s`] or [`PhotographicWalk::blur_turn_min_deg_s`], full
//!   (short) blur above the `full` values, smoothly between. A walk at 1.4 m/s does not blur; a
//!   sprint or a fast pan does.
//! - **Depth of field** is off, except while a focus pull runs ([`PhotographicWalk::pull_focus`])
//!   and for [`PhotographicWalk::focus_hold_s`] seconds after it: the focal distance eases from
//!   where it was to the target with a smoothstep, then the effect switches off again.
//!
//! The decision is a pure function of the camera's motion between frames and the pull state
//! ([`PhotographicWalk::step`]), so a replay gives the same lens frame by frame.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_acos, det_cos};
use regia_render::{DepthOfFieldMode, PhysicalCamera};
use regia_scene::Transform;

use super::CameraSet;

/// Largest focus distance a pull accepts, metres.
pub const MAX_FOCUS_DISTANCE_M: f32 = 10_000.0;

/// A focus pull on its way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusPull {
    /// Focal distance when the pull began, metres.
    pub from_m: f32,
    /// Focal distance it ends at, metres.
    pub to_m: f32,
    /// Length of the pull, seconds.
    pub duration_s: f32,
    /// Seconds into the pull.
    pub elapsed_s: f32,
}

/// What a step decided for the lens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalkLens {
    /// Fraction of a frame the shutter is open (`0` is no blur).
    pub shutter_fraction: f32,
    /// Depth of field: `Some(focal distance in metres)` while a pull runs or holds.
    pub focus_m: Option<f32>,
}

/// The photographic walk preset (see the module docs).
///
/// # Example
///
/// ```
/// use regia_core::math::{Quat, Vec3};
/// use regia_framework::camera::photographic::PhotographicWalk;
///
/// let mut walk = PhotographicWalk::default();
/// walk.step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY);
/// // Walking at 1.4 m/s: no blur, no depth of field.
/// let lens = walk.step(1.0 / 60.0, Vec3::new(1.4 / 60.0, 0.0, 0.0), Quat::IDENTITY);
/// assert_eq!(lens.shutter_fraction, 0.0);
/// assert_eq!(lens.focus_m, None);
/// ```
#[derive(Component, Debug, Clone, PartialEq)]
pub struct PhotographicWalk {
    /// Shutter fraction at full speed (the short shutter), `0` turns blur off.
    pub shutter_fraction: f32,
    /// Motion blur samples per direction.
    pub motion_blur_samples: u32,
    /// Linear speed below which there is no blur, metres per second.
    pub blur_speed_min_m_s: f32,
    /// Linear speed from which the blur is at full strength, metres per second.
    pub blur_speed_full_m_s: f32,
    /// Turn rate below which there is no blur, degrees per second.
    pub blur_turn_min_deg_s: f32,
    /// Turn rate from which the blur is at full strength, degrees per second.
    pub blur_turn_full_deg_s: f32,
    /// Seconds depth of field stays on after a pull ends.
    pub focus_hold_s: f32,
    /// Aperture while depth of field is on, f-stops.
    pub aperture_f_stops: f32,
    /// Kernel while depth of field is on.
    pub mode: DepthOfFieldMode,
    /// Focal distance the next pull starts from, metres.
    pub focal_distance_m: f32,
    last: Option<(Vec3, Quat)>,
    pull: Option<FocusPull>,
    hold_left_s: f32,
    lens: WalkLens,
}

impl Default for PhotographicWalk {
    fn default() -> Self {
        Self {
            shutter_fraction: 0.12,
            motion_blur_samples: 2,
            blur_speed_min_m_s: 3.0,
            blur_speed_full_m_s: 8.0,
            blur_turn_min_deg_s: 90.0,
            blur_turn_full_deg_s: 270.0,
            focus_hold_s: 0.6,
            aperture_f_stops: 2.8,
            mode: DepthOfFieldMode::Gaussian,
            focal_distance_m: 10.0,
            last: None,
            pull: None,
            hold_left_s: 0.0,
            lens: WalkLens {
                shutter_fraction: 0.0,
                focus_m: None,
            },
        }
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    if edge1 <= edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl PhotographicWalk {
    /// The lens last decided.
    pub fn lens(&self) -> WalkLens {
        self.lens
    }

    /// The focus pull running, if any.
    pub fn pull(&self) -> Option<FocusPull> {
        self.pull
    }

    /// Whether depth of field is on now (a pull runs or holds).
    pub fn focusing(&self) -> bool {
        self.pull.is_some() || self.hold_left_s > 0.0
    }

    /// Starts a focus pull to `to_m` metres over `duration_s` seconds, from the current focal
    /// distance. A non-finite or non-positive distance is refused (`false`); a distance above
    /// [`MAX_FOCUS_DISTANCE_M`] is clamped; a duration below one millisecond is an immediate cut.
    pub fn pull_focus(&mut self, to_m: f32, duration_s: f32) -> bool {
        if !to_m.is_finite() || to_m <= 0.0 || !duration_s.is_finite() || duration_s < 0.0 {
            return false;
        }
        self.pull = Some(FocusPull {
            from_m: self.focal_distance_m,
            to_m: to_m.min(MAX_FOCUS_DISTANCE_M),
            duration_s: duration_s.max(0.001),
            elapsed_s: 0.0,
        });
        true
    }

    /// Blur strength `0..=1` for a camera that moved `speed_m_s` and turned `turn_deg_s`.
    pub fn blur_weight(&self, speed_m_s: f32, turn_deg_s: f32) -> f32 {
        smoothstep(self.blur_speed_min_m_s, self.blur_speed_full_m_s, speed_m_s).max(smoothstep(
            self.blur_turn_min_deg_s,
            self.blur_turn_full_deg_s,
            turn_deg_s,
        ))
    }

    /// The motion-blur weight of a relative rotation with quaternion dot `dot` over `dt` seconds:
    /// `0` up to [`Self::blur_turn_min_deg_s`], `1` from [`Self::blur_turn_full_deg_s`], the
    /// smoothstep between. The edge decisions compare the dot to the pinned `det_cos` of the edge
    /// angle (the rotation angle is `2·acos(|dot|)`), so a camera turning exactly at a threshold
    /// takes the same branch on every platform; inside the ramp the smoothstep input comes from
    /// the pinned `acos` series (spec 35 § 12.13).
    fn turn_weight(&self, dot: f32, dt: f32) -> f32 {
        if dt <= 0.0 {
            return 0.0;
        }
        let min_half = self.blur_turn_min_deg_s.to_radians() * dt * 0.5;
        let full_half = self.blur_turn_full_deg_s.to_radians() * dt * 0.5;
        let d = dot.abs().clamp(-1.0, 1.0);
        if d >= det_cos(min_half) {
            return 0.0; // at most min_turn: no blur.
        }
        if d <= det_cos(full_half) {
            return 1.0; // at least full_turn: full (short) blur.
        }
        let turn_deg_s = 2.0 * det_acos(d) * (180.0 / std::f32::consts::PI) / dt;
        smoothstep(self.blur_turn_min_deg_s, self.blur_turn_full_deg_s, turn_deg_s)
    }

    /// Advances by `dt` seconds with the camera at `translation` and `rotation`, and decides the
    /// lens for this frame.
    pub fn step(&mut self, dt: f32, translation: Vec3, rotation: Quat) -> WalkLens {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let (mut speed, mut turn) = (0.0, 0.0);
        if let Some((last_t, last_r)) = self.last
            && dt > 0.0
        {
            speed = (translation - last_t).length() / dt;
            turn = self.turn_weight(last_r.dot(rotation), dt);
        }
        self.last = Some((translation, rotation));
        let weight = smoothstep(self.blur_speed_min_m_s, self.blur_speed_full_m_s, speed).max(turn);
        let shutter = if self.shutter_fraction.is_finite() {
            self.shutter_fraction.max(0.0) * weight
        } else {
            0.0
        };

        let mut focus_m = None;
        if let Some(mut pull) = self.pull {
            pull.elapsed_s += dt;
            let t = (pull.elapsed_s / pull.duration_s).clamp(0.0, 1.0);
            let eased = t * t * (3.0 - 2.0 * t);
            self.focal_distance_m = pull.from_m + (pull.to_m - pull.from_m) * eased;
            if t >= 1.0 {
                self.focal_distance_m = pull.to_m;
                self.pull = None;
                self.hold_left_s = self.focus_hold_s.max(0.0);
            } else {
                self.pull = Some(pull);
            }
            focus_m = Some(self.focal_distance_m);
        } else if self.hold_left_s > 0.0 {
            self.hold_left_s = (self.hold_left_s - dt).max(0.0);
            focus_m = Some(self.focal_distance_m);
        }
        self.lens = WalkLens {
            shutter_fraction: shutter,
            focus_m,
        };
        self.lens
    }

    /// Writes the decided lens into `camera`: shutter and samples for the motion blur, and the
    /// depth of field kernel and focal distance while focusing.
    pub fn write(&self, camera: &mut PhysicalCamera) {
        camera.shutter_angle = self.lens.shutter_fraction;
        camera.motion_blur_samples = if self.lens.shutter_fraction > 0.0 {
            self.motion_blur_samples
        } else {
            0
        };
        match self.lens.focus_m {
            Some(distance) => {
                camera.depth_of_field = Some(self.mode);
                camera.focal_distance = distance;
                camera.aperture_f_stops = self.aperture_f_stops;
            }
            None => camera.depth_of_field = None,
        }
    }
}

/// `CameraSet::Director`, after the shakes: decides every photographic walk's lens and writes it
/// into the camera's [`PhysicalCamera`] (added when missing).
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_photographic_walk")]
fn drive_walk(
    time: Res<Time>,
    mut commands: Commands,
    mut cameras: Query<(
        Entity,
        &Transform,
        &mut PhotographicWalk,
        Option<&mut PhysicalCamera>,
    )>,
) {
    let dt = time.delta_secs();
    for (entity, transform, mut walk, lens) in &mut cameras {
        walk.step(dt, transform.translation, transform.rotation);
        match lens {
            Some(mut lens) => {
                // Only write on change, so change detection stays quiet on a steady lens.
                let mut next = *lens;
                walk.write(&mut next);
                if next != *lens {
                    *lens = next;
                }
            }
            None => {
                let mut fresh = PhysicalCamera::default();
                walk.write(&mut fresh);
                commands.entity(entity).try_insert(fresh);
            }
        }
    }
}

/// Registers the system (called by [`CameraPlugin`](super::CameraPlugin)).
pub(super) fn install(app: &mut App) {
    app.add_systems(
        PostUpdate,
        drive_walk
            .in_set(CameraSet::Director)
            .after(super::sensor::apply_shakes)
            .before(PostUpdateSet::TransformPropagate),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::det_quat_from_yaw_pitch_roll;

    fn drive(walk: &mut PhotographicWalk, speed: f32, frames: u32) -> WalkLens {
        let mut x = 0.0;
        let mut lens = walk.lens();
        walk.step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY);
        for _ in 0..frames {
            x += speed / 60.0;
            lens = walk.step(1.0 / 60.0, Vec3::new(x, 0.0, 0.0), Quat::IDENTITY);
        }
        lens
    }

    /// Pin: the preset's numbers. A change here changes how every walk renders.
    /// Pin: a camera turning exactly at the blur threshold takes the same branch on every
    /// platform. The edge decision compares the rotation dot to the pinned `det_cos` of the edge
    /// angle; one thousandth of a radian of yaw on either side of the minimum turn separates the
    /// zero-sample side from the blurred side (far inside the 180 deg/s ramp, far above the
    /// pinned series' error).
    #[test]
    fn the_blur_threshold_is_pinned_around_the_minimum_turn() {
        let dt = 1.0 / 60.0;
        let min_angle = 90.0f32.to_radians() * dt;
        let mut below = PhotographicWalk::default();
        below.step(dt, Vec3::ZERO, Quat::IDENTITY);
        let lens = below.step(
            dt,
            Vec3::ZERO,
            det_quat_from_yaw_pitch_roll(min_angle - 0.001, 0.0, 0.0),
        );
        assert_eq!(lens.shutter_fraction, 0.0, "below the threshold: no blur at all");
        assert_eq!(lens.shutter_fraction.to_bits(), 0);

        let mut above = PhotographicWalk::default();
        above.step(dt, Vec3::ZERO, Quat::IDENTITY);
        let lens = above.step(
            dt,
            Vec3::ZERO,
            det_quat_from_yaw_pitch_roll(min_angle + 0.001, 0.0, 0.0),
        );
        assert_ne!(lens.shutter_fraction, 0.0, "one milli-radian above: blurred");
        assert_eq!(lens.shutter_fraction.to_bits(), 0x3907_5b4d);
        // A rate at the full threshold blurs at the full (short) shutter.
        let mut full = PhotographicWalk::default();
        full.step(dt, Vec3::ZERO, Quat::IDENTITY);
        let lens = full.step(
            dt,
            Vec3::ZERO,
            det_quat_from_yaw_pitch_roll(270.0f32.to_radians() * dt + 0.001, 0.0, 0.0),
        );
        assert!((lens.shutter_fraction - 0.12).abs() < 1.0e-7);
    }

    #[test]
    fn photographic_walk_pin() {
        let w = PhotographicWalk::default();
        assert_eq!(w.shutter_fraction, 0.12);
        assert_eq!(w.motion_blur_samples, 2);
        assert_eq!((w.blur_speed_min_m_s, w.blur_speed_full_m_s), (3.0, 8.0));
        assert_eq!((w.blur_turn_min_deg_s, w.blur_turn_full_deg_s), (90.0, 270.0));
        assert_eq!(w.focus_hold_s, 0.6);
        // A short shutter: under a quarter of a frame, shorter than the 180 degree default 0.5.
        assert!(w.shutter_fraction < 0.25);
    }

    #[test]
    fn walking_pace_does_not_blur_and_a_sprint_does() {
        let mut walk = PhotographicWalk::default();
        assert_eq!(drive(&mut walk, 1.4, 30).shutter_fraction, 0.0);
        assert_eq!(drive(&mut walk, 2.9, 30).shutter_fraction, 0.0);
        let sprint = drive(&mut walk, 9.0, 30).shutter_fraction;
        assert!((sprint - 0.12).abs() < 1e-5, "{sprint}");
        let mid = drive(&mut walk, 5.5, 30).shutter_fraction;
        assert!(mid > 0.0 && mid < 0.12, "{mid}");
    }

    #[test]
    fn a_fast_pan_blurs_and_a_slow_turn_does_not() {
        let mut walk = PhotographicWalk::default();
        walk.step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY);
        let slow = walk.step(
            1.0 / 60.0,
            Vec3::ZERO,
            Quat::from_rotation_y(30.0_f32.to_radians() / 60.0),
        );
        assert_eq!(slow.shutter_fraction, 0.0);
        let fast = walk.step(
            1.0 / 60.0,
            Vec3::ZERO,
            Quat::from_rotation_y(300.0_f32.to_radians() / 60.0),
        );
        assert!(fast.shutter_fraction > 0.1, "{}", fast.shutter_fraction);
    }

    #[test]
    fn depth_of_field_is_off_except_on_a_focus_pull_and_its_hold() {
        let mut walk = PhotographicWalk::default();
        assert_eq!(drive(&mut walk, 1.4, 10).focus_m, None);
        assert!(walk.pull_focus(3.0, 0.98));
        let mut seen_on = 0;
        let mut last = None;
        for _ in 0..60 {
            let lens = walk.step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY);
            if lens.focus_m.is_some() {
                seen_on += 1;
            }
            last = lens.focus_m;
        }
        assert_eq!(seen_on, 60);
        assert_eq!(last, Some(3.0));
        // The hold: 0.6 s more, then off.
        let mut held = 0;
        for _ in 0..120 {
            if walk
                .step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY)
                .focus_m
                .is_some()
            {
                held += 1;
            }
        }
        assert!((35..=37).contains(&held), "{held}");
        assert!(!walk.focusing());
        assert_eq!(walk.step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY).focus_m, None);
    }

    #[test]
    fn a_pull_eases_monotonically_between_the_distances() {
        let mut walk = PhotographicWalk {
            focal_distance_m: 20.0,
            ..PhotographicWalk::default()
        };
        assert!(walk.pull_focus(2.0, 0.5));
        let mut prev = 20.0;
        for _ in 0..30 {
            let d = walk
                .step(1.0 / 60.0, Vec3::ZERO, Quat::IDENTITY)
                .focus_m
                .expect("focusing");
            assert!(d <= prev + 1e-5 && d >= 2.0 - 1e-5, "{d} {prev}");
            prev = d;
        }
        assert_eq!(walk.focal_distance_m, 2.0);
    }

    #[test]
    fn bad_pulls_are_refused() {
        let mut walk = PhotographicWalk::default();
        assert!(!walk.pull_focus(0.0, 1.0));
        assert!(!walk.pull_focus(-3.0, 1.0));
        assert!(!walk.pull_focus(f32::NAN, 1.0));
        assert!(!walk.pull_focus(3.0, f32::NAN));
        assert!(!walk.pull_focus(3.0, -1.0));
        assert!(walk.pull().is_none());
        assert!(walk.pull_focus(1.0e9, 1.0));
        assert_eq!(walk.pull().expect("pull").to_m, MAX_FOCUS_DISTANCE_M);
    }

    #[test]
    fn write_sets_the_physical_camera() {
        let mut walk = PhotographicWalk::default();
        let mut camera = PhysicalCamera::default();
        assert!(camera.depth_of_field.is_none());
        drive(&mut walk, 9.0, 5);
        walk.write(&mut camera);
        assert_eq!(camera.shutter_angle, 0.12);
        assert_eq!(camera.motion_blur_samples, 2);
        assert!(camera.depth_of_field.is_none());
        assert!(camera.motion_blur().is_some());
        walk.pull_focus(4.0, 0.2);
        walk.step(1.0 / 60.0, Vec3::new(100.0, 0.0, 0.0), Quat::IDENTITY);
        walk.write(&mut camera);
        assert!(camera.depth_of_field.is_some());
        // Standing still after a sprint: blur off, samples 0.
        for _ in 0..3 {
            walk.step(1.0 / 60.0, Vec3::new(100.0, 0.0, 0.0), Quat::IDENTITY);
        }
        walk.write(&mut camera);
        assert_eq!(camera.shutter_angle, 0.0);
        assert!(camera.motion_blur().is_none());
    }

    mod app_tests {
        use regia_app::enable_fixed_step;

        use super::super::*;
        use crate::camera::CameraPlugin;

        #[test]
        fn the_system_drives_the_physical_camera_from_the_camera_motion() {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, CameraPlugin));
            enable_fixed_step(&mut app, 60.0);
            let cam = app
                .world_mut()
                .spawn((Transform::default(), PhotographicWalk::default()))
                .id();
            // Walking pace: the lens is added with no blur and no depth of field.
            let mut x = 0.0;
            for _ in 0..30 {
                x += 1.4 / 60.0;
                app.world_mut().get_mut::<Transform>(cam).expect("t").translation = Vec3::new(x, 0.0, 0.0);
                app.update();
            }
            let lens = *app.world().get::<PhysicalCamera>(cam).expect("lens added");
            assert_eq!(lens.shutter_angle, 0.0);
            assert!(lens.motion_blur().is_none());
            assert!(lens.depth_of_field().is_none());
            // A sprint: the short shutter.
            for _ in 0..30 {
                x += 9.0 / 60.0;
                app.world_mut().get_mut::<Transform>(cam).expect("t").translation = Vec3::new(x, 0.0, 0.0);
                app.update();
            }
            let lens = *app.world().get::<PhysicalCamera>(cam).expect("lens");
            assert!((lens.shutter_angle - 0.12).abs() < 1e-5, "{}", lens.shutter_angle);
            assert!(lens.motion_blur().is_some());
            // A focus pull turns depth of field on, at the pulled distance.
            app.world_mut()
                .get_mut::<PhotographicWalk>(cam)
                .expect("walk")
                .pull_focus(3.0, 0.2);
            for _ in 0..5 {
                app.update();
            }
            let lens = *app.world().get::<PhysicalCamera>(cam).expect("lens");
            assert!(lens.depth_of_field().is_some());
            assert!(
                lens.focal_distance < 10.0 && lens.focal_distance >= 3.0,
                "{}",
                lens.focal_distance
            );
        }
    }
}
