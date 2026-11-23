// SPDX-License-Identifier: MIT-0
//! Camera shake in the showcase host (ADR-1448): the blast's shock reaches the camera after
//! `distance / 343` seconds and shakes it, scheduled with `camera.schedule_shock_shake`.
//!
//! [`ShowcaseShakePlugin`] installs `regia_framework`'s `ShockShakePlugin` (the schedule the agent
//! method fills, fired on the fixed step) and the director that holds the running shakes. The
//! showcase camera is moved by the shot player or the agent, not by a `CameraRig`, so the
//! director's offset is applied here: [`apply_camera_shake`] runs after the shot player in
//! `PostUpdate` and adds the offset, pitch, yaw and roll to the camera's pose, keeping the unshaken pose so
//! the offset never accumulates and the pose the player wrote is never lost. Everything runs on
//! the frame clock, so the same run shakes the same way.

use regia::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::CameraDirector;
use regia_framework::shock_shake::ShockShakePlugin;
use regia_framework::shots::ShotDriveSet;
use regia_render::Camera;
use regia_scene::Transform;

/// The camera's pose without the shake, and what was written on top of it.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct CameraShakeState {
    /// The pose before the shake was added (translation, rotation); `None` while idle.
    base: Option<(Vec3, Quat)>,
    /// The pose after the shake was added: when the camera still holds it, nobody moved it.
    shaken: Option<(Vec3, Quat)>,
    /// The offset added this frame, metres.
    pub offset: Vec3,
    /// The roll added this frame, radians.
    pub roll: f32,
    /// The pitch added this frame, radians.
    pub pitch: f32,
    /// The yaw added this frame, radians.
    pub yaw: f32,
    /// Largest rotation (pitch, yaw or roll) so far, radians.
    pub max_angle: f32,
    /// Frames the shake moved the camera.
    pub frames_shaken: u64,
    /// Largest offset length so far, metres.
    pub max_offset: f32,
}

/// Installs the shock-shake schedule, the camera director and [`apply_camera_shake`].
pub struct ShowcaseShakePlugin;

impl Plugin for ShowcaseShakePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ShockShakePlugin)
            .init_resource::<CameraDirector>()
            .init_resource::<CameraShakeState>()
            .add_systems(
                PostUpdate,
                apply_camera_shake
                    .after(ShotDriveSet)
                    .before(PostUpdateSet::TransformPropagate),
            );
    }
}

/// Adds the director's shake to the camera pose. Idle (no shake running, none applied): touches
/// nothing.
#[tracing::instrument(skip_all, target = "showcase", name = "showcase_camera_shake")]
pub fn apply_camera_shake(
    time: Res<Time>,
    mut director: ResMut<CameraDirector>,
    mut state: ResMut<CameraShakeState>,
    mut cameras: Query<&mut Transform, With<Camera>>,
) {
    let pose = director.sample_pose(time.delta_secs());
    let Some(mut transform) = cameras.iter_mut().next() else {
        return;
    };
    let idle = pose.is_idle();
    if idle && state.base.is_none() {
        return;
    }
    // The pose to shake: the stored unshaken one when the camera still holds what we wrote,
    // else what the player or the agent just wrote.
    let now = (transform.translation, transform.rotation);
    let (base_t, base_r) = match (state.base, state.shaken) {
        (Some(base), Some(shaken)) if shaken == now => base,
        _ => now,
    };
    transform.translation = base_t + pose.offset;
    transform.rotation = base_r * pose.rotation();
    state.offset = pose.offset;
    state.roll = pose.roll;
    state.pitch = pose.pitch;
    state.yaw = pose.yaw;
    if idle {
        state.base = None;
        state.shaken = None;
    } else {
        state.base = Some((base_t, base_r));
        state.shaken = Some((transform.translation, transform.rotation));
        state.frames_shaken += 1;
        state.max_offset = state.max_offset.max(pose.offset.length());
        state.max_angle = state
            .max_angle
            .max(pose.pitch.abs().max(pose.yaw.abs()).max(pose.roll.abs()));
    }
}

#[cfg(test)]
mod tests {
    use regia_app::enable_fixed_step;
    use regia_framework::ground_shock::{GroundShock, angle_to_pixels};
    use regia_framework::shock_shake::{ShockShake, ShockShakeSchedule};

    use super::*;
    use crate::STEP_HZ;

    const EYE: Vec3 = Vec3::new(10.0, 50.0, -20.0);

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        enable_fixed_step(&mut app, STEP_HZ);
        app.add_plugins(ShowcaseShakePlugin);
        app.world_mut().spawn((
            Transform {
                translation: EYE,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
            Camera::default(),
        ));
        app
    }

    fn eye(app: &mut App) -> Vec3 {
        let mut q = app.world_mut().query_filtered::<&Transform, With<Camera>>();
        q.single(app.world()).expect("camera").translation
    }

    /// Runs `frames` updates after scheduling a blast `distance_m` away; returns the camera
    /// offset from the unshaken eye on every frame.
    fn trace(distance_m: f32, frames: usize) -> Vec<Vec3> {
        let mut app = app();
        let arrival = app.world_mut().resource_mut::<ShockShakeSchedule>().schedule(
            &ShockShake::showcase(),
            distance_m,
            0,
        );
        assert!(arrival.is_some(), "the shock is not negligible");
        (0..frames)
            .map(|_| {
                app.update();
                eye(&mut app) - EYE
            })
            .collect()
    }

    #[test]
    fn the_camera_is_still_until_the_shock_arrives_then_shakes_then_settles() {
        // 343 m: one second, 60 steps. The shake lasts 3.5 s (210 steps).
        let offsets = trace(343.0, 330);
        assert!(
            offsets[..58].iter().all(|o| *o == Vec3::ZERO),
            "still before the front"
        );
        let shaken = offsets.iter().filter(|o| o.length() > 0.0).count();
        assert!(shaken > 150, "{shaken} frames moved");
        assert!(
            offsets[300..].iter().all(|o| *o == Vec3::ZERO),
            "settled after the shake"
        );
        let peak = offsets.iter().map(|o| o.length()).fold(0.0f32, f32::max);
        assert!(peak > 0.01 && peak < 0.6, "peak {peak} m");
        // Frame to frame the camera moves smoothly: the largest step is a small share of the peak.
        let step = offsets
            .windows(2)
            .map(|w| (w[1] - w[0]).length())
            .fold(0.0f32, f32::max);
        assert!(step > 0.0 && step < peak, "step {step} vs peak {peak}");
    }

    #[test]
    fn the_shot_range_arrives_at_the_speed_of_sound() {
        // 3.3 km: 9.62 s, 578 steps.
        let offsets = trace(3300.0, 640);
        let first = offsets.iter().position(|o| o.length() > 0.0).expect("shakes");
        assert!((577..=583).contains(&first), "first moved on frame {first}");
    }

    #[test]
    fn two_runs_shake_identically() {
        assert_eq!(trace(343.0, 200), trace(343.0, 200));
    }

    #[test]
    fn a_pose_written_every_frame_is_shaken_not_accumulated() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ShockShakeSchedule>()
            .schedule(&ShockShake::showcase(), 343.0, 0);
        let mut offsets = Vec::new();
        for frame in 0..200 {
            // The shot player's job: an absolute pose each frame, moving along +X.
            let base = EYE + Vec3::new(frame as f32 * 0.1, 0.0, 0.0);
            let mut q = app.world_mut().query_filtered::<&mut Transform, With<Camera>>();
            q.single_mut(app.world_mut()).expect("camera").translation = base;
            app.update();
            offsets.push(eye(&mut app) - base);
        }
        let reference = trace(343.0, 200);
        for (frame, (got, want)) in offsets.iter().zip(&reference).enumerate() {
            assert!(
                (*got - *want).length() < 1.0e-4,
                "frame {frame}: {got:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn the_state_counts_the_shaken_frames() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ShockShakeSchedule>()
            .schedule(&ShockShake::showcase(), 343.0, 0);
        for _ in 0..300 {
            app.update();
        }
        let state = *app.world().resource::<CameraShakeState>();
        assert!(state.frames_shaken > 150 && state.max_offset > 0.0, "{state:?}");
    }

    fn view_dir(app: &mut App) -> Vec3 {
        let mut q = app.world_mut().query_filtered::<&Transform, With<Camera>>();
        q.single(app.world()).expect("camera").rotation * Vec3::NEG_Z
    }

    /// Per frame distance (about the angle in radians) between the shaken view direction and the
    /// unshaken one (straight down -Z), for a ground shock `distance_m` away.
    fn tilt_trace(distance_m: f32, frames: usize) -> Vec<f32> {
        tilt_trace_with(&GroundShock::default(), distance_m, frames)
    }

    fn tilt_trace_with(ground: &GroundShock, distance_m: f32, frames: usize) -> Vec<f32> {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ShockShakeSchedule>()
            .schedule_ground(ground, distance_m, 0);
        (0..frames)
            .map(|_| {
                app.update();
                (view_dir(&mut app) - Vec3::NEG_Z).length()
            })
            .collect()
    }

    #[test]
    fn the_default_seismic_phase_is_near_invisible_and_the_airblast_one_short_jolt() {
        // 3.3 km on the showcase's shot-4 lens (20 degrees, 4K): seismic at step 36, airblast at 578.
        let tilt = tilt_trace(3300.0, 900);
        assert!(tilt[..35].iter().all(|a| *a == 0.0), "still before the P wave");
        let vfov = 20.0f32.to_radians();
        let seismic = tilt[36..500].iter().copied().fold(0.0f32, f32::max);
        let seismic_px = angle_to_pixels(seismic, vfov, 2160.0);
        assert!((0.1..=1.0).contains(&seismic_px), "seismic {seismic_px} px at 4K");
        let air = tilt[578..].iter().copied().fold(0.0f32, f32::max);
        let air_px = angle_to_pixels(air, vfov, 2160.0);
        // The view direction mixes pitch and yaw (0.8 of it), so a 3 to 4 px pitch reads up to 1.3x.
        assert!((2.5..=5.5).contains(&air_px), "airblast jolt {air_px} px at 4K");
        assert!(
            air_px > 3.0 * seismic_px,
            "the airblast is the event: {air_px} vs {seismic_px}"
        );
        // One jolt: the tilt peaks within a few frames of the arrival and has only decayed
        // since, so there is no ringing (a monotone tail after the peak, up to rotation mixing).
        let peak = tilt[578..].iter().copied().fold(0.0f32, f32::max);
        let at = 578 + tilt[578..].iter().position(|a| *a == peak).expect("peak");
        assert!(at < 578 + 12, "peak {} frames after the arrival", at - 578);
        assert!(tilt[at + 60] < 0.5 * peak, "decayed within a second");
        assert!(
            tilt[at..].windows(2).all(|w| w[1] <= w[0] + 1.0e-6),
            "only decays"
        );
        assert!(
            tilt[578 + 150..].iter().all(|a| *a < 0.02 * peak),
            "over in 2.5 s"
        );
    }

    #[test]
    fn the_legacy_ground_wave_tilts_the_camera_visibly_at_3_3_km_before_the_airblast() {
        // The ADR-1457 strengths, reachable as `GroundShock::legacy()`.
        let tilt = tilt_trace_with(&GroundShock::legacy(), 3300.0, 700);
        assert!(tilt[..35].iter().all(|a| *a == 0.0), "still before the P wave");
        let vfov = 60.0f32.to_radians();
        let seismic = tilt[36..500].iter().copied().fold(0.0f32, f32::max);
        let seismic_px = angle_to_pixels(seismic, vfov, 2160.0);
        assert!((1.5..8.0).contains(&seismic_px), "seismic {seismic_px} px at 4K");
        let air = tilt[578..].iter().copied().fold(0.0f32, f32::max);
        let air_px = angle_to_pixels(air, vfov, 2160.0);
        assert!((1.5..12.0).contains(&air_px), "airblast {air_px} px at 4K");
        assert!(
            tilt[500..578].iter().all(|a| *a < 0.05 * seismic),
            "calm between the waves"
        );
    }

    #[test]
    fn a_camera_300_m_away_shakes_hard() {
        let tilt = tilt_trace(300.0, 120);
        let peak = tilt.iter().copied().fold(0.0f32, f32::max);
        let far = tilt_trace(3300.0, 700).iter().copied().fold(0.0f32, f32::max);
        assert!(peak.to_degrees() > 1.0, "{} deg", peak.to_degrees());
        assert!(peak > 10.0 * far, "{peak} vs {far}");
    }

    #[test]
    fn the_ground_shake_replays_identically_and_settles() {
        assert_eq!(tilt_trace(1500.0, 400), tilt_trace(1500.0, 400));
        let tilt = tilt_trace(1500.0, 1200);
        assert!(tilt[1100..].iter().all(|a| *a == 0.0), "settled");
        let mut app = app();
        app.world_mut()
            .resource_mut::<ShockShakeSchedule>()
            .schedule_ground(&GroundShock::default(), 1500.0, 0);
        for _ in 0..600 {
            app.update();
        }
        assert!(app.world().resource::<CameraShakeState>().max_angle > 1.0e-4);
    }
}
