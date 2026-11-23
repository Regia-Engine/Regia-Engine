// SPDX-License-Identifier: MPL-2.0
//! Camera shake: trauma-decayed, noise-driven offsets driven by world events.
//!
//! The model follows Squirrel Eiseroh's public GDC 2013 talk "Juice Your Game With Math": each
//! shake adds *trauma* (clamped to 1); offsets are `trauma²` times a noise signal, so shakes blend
//! non-linearly and fade smoothly to zero. Shake strength falls off with distance from the camera
//! (`radius` = full strength inside, linear to zero at `2 * radius`).
//!
//! Regia Engine has no camera transform of its own here yet (`regia_framework::camera` owns rigs), so
//! this module exposes the offset and the camera owner applies it — the integration hook for M5/M6.

use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;

use crate::plugin::VfxView;

/// Request a camera shake (send as a message; the VFX plugin applies it).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct CameraShakeRequest {
    /// World position of the shake source.
    pub position: Vec3,
    /// Trauma added at the source (0..=1 range sensible).
    pub strength: f32,
    /// Full-strength radius in metres; linear falloff to zero at `2 * radius`.
    pub radius: f32,
}

/// The accumulated shake state for one viewer.
///
/// # Example
///
/// ```
/// use regia_vfx::shake::CameraShake;
///
/// let mut shake = CameraShake::default();
/// shake.add_trauma(1.0);
/// assert_eq!(shake.trauma(), 1.0); // clamped to [0, 1]
/// shake.update(0.5, 1.0, 1.0, 0.1); // half a second, full decay/sec
/// assert!((shake.trauma() - 0.5).abs() < 1e-6);
/// ```
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct CameraShake {
    trauma: f32,
    seed: u64,
    time: f32,
    /// Last computed offset (updated in `update`).
    offset: Vec3,
    roll: f32,
}

impl CameraShake {
    /// Current positional offset (metres).
    pub fn offset(&self) -> Vec3 {
        self.offset
    }

    /// Current roll offset (radians) around the view axis.
    pub fn roll(&self) -> f32 {
        self.roll
    }

    /// Applies the offset to a camera eye position.
    pub fn apply(&self, eye: Vec3) -> Vec3 {
        eye + self.offset
    }

    /// Applies the roll to a camera rotation (around its forward axis).
    pub fn apply_rotation(&self, rotation: Quat, forward: Vec3) -> Quat {
        // A zero or non-finite forward has no roll axis: `from_axis_angle` with a zero axis is
        // not a unit quaternion and would scale the camera. Regression:
        // tests/tqs_sweep.rs::camera_shake_apply_rotation_keeps_a_unit_quaternion.
        let axis = forward.normalize_or_zero();
        if self.roll.is_nan() || self.roll.abs() < 1e-6 || axis == Vec3::ZERO {
            return rotation;
        }
        Quat::from_axis_angle(axis, self.roll) * rotation
    }

    /// Current trauma in `[0, 1]`.
    pub fn trauma(&self) -> f32 {
        self.trauma
    }

    /// Starts the noise clock over when the shake is idle (no trauma), so a shake added next
    /// samples the same noise in every render-out job (ADR-2687). Returns whether it did.
    pub fn restart_idle_clock(&mut self) -> bool {
        if self.trauma > 0.0 {
            return false;
        }
        self.time = 0.0;
        true
    }

    /// Adds trauma (clamped to 1).
    pub fn add_trauma(&mut self, amount: f32) {
        self.trauma = (self.trauma + amount.max(0.0)).min(1.0);
    }

    /// Advances the shake by `dt` seconds and recomputes the offset.
    ///
    /// `decay` is trauma lost per second (1.0 = a 1 s shake); `amplitude` the positional amplitude
    /// in metres at full trauma; `roll_amplitude` the roll amplitude in radians.
    pub fn update(&mut self, dt: f32, decay: f32, amplitude: f32, roll_amplitude: f32) {
        // A NaN, infinite or negative step is no time at all: it must neither freeze the noise
        // clock (NaN) nor pump trauma past 1 (a negative step or decay). Regressions:
        // tests/tqs_sweep.rs::camera_shake_trauma_stays_in_unit_range_for_hostile_steps and
        // tests/tqs_sweep.rs::camera_shake_keeps_shaking_after_a_hostile_step.
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        let decay = if decay.is_finite() { decay.max(0.0) } else { 0.0 };
        self.time += dt;
        self.trauma = (self.trauma - decay * dt).clamp(0.0, 1.0);
        let shake = self.trauma * self.trauma;
        if shake <= 1e-6 {
            self.offset = Vec3::ZERO;
            self.roll = 0.0;
            return;
        }
        // Three decorrelated PCG streams, one per axis; frequency ~35 Hz max for punchy shakes.
        let n = |stream: u64| -> f32 {
            let mut rng =
                regia_core::math::Rng::with_stream(self.seed, stream.wrapping_add((self.time * 35.0) as u64));
            rng.f32() * 2.0 - 1.0
        };
        self.offset = Vec3::new(n(1), n(2), n(3)) * (amplitude * shake);
        self.roll = n(4) * roll_amplitude * shake;
    }

    /// Distance-based trauma from a request, given the camera position.
    pub fn trauma_from(request: &CameraShakeRequest, camera_pos: Vec3) -> f32 {
        let d = request.position.distance(camera_pos);
        let falloff = if d <= request.radius {
            1.0
        } else if d >= request.radius * 2.0 {
            0.0
        } else {
            1.0 - (d - request.radius) / request.radius
        };
        request.strength * falloff
    }
}

/// System: consume [`CameraShakeRequest`] messages into [`CameraShake`] trauma, with the
/// distance falloff of [`CameraShake::trauma_from`] measured from the [`VfxView`] camera.
pub fn apply_shake_requests(
    mut requests: MessageReader<CameraShakeRequest>,
    view: Res<VfxView>,
    mut shake: ResMut<CameraShake>,
) {
    for req in requests.read() {
        shake.add_trauma(CameraShake::trauma_from(req, view.camera.eye));
    }
}

/// System: advance the shake state each frame.
pub fn update_camera_shake(time: Res<regia_app::Time>, mut shake: ResMut<CameraShake>) {
    shake.update(time.delta_secs(), 1.2, 0.35, 0.02);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trauma_decays_to_silence() {
        let mut shake = CameraShake {
            seed: 7,
            ..Default::default()
        };
        shake.add_trauma(1.0);
        shake.update(1.0 / 60.0, 1.2, 0.35, 0.02);
        assert!(shake.offset().length() > 1e-3, "shaking at full trauma");
        for _ in 0..120 {
            shake.update(1.0 / 60.0, 1.2, 0.35, 0.02);
        }
        assert_eq!(shake.offset(), Vec3::ZERO, "2 s of 1.2/s decay silences it");
        assert_eq!(shake.trauma(), 0.0);
    }

    #[test]
    fn trauma_squares_so_small_shakes_stay_small() {
        let mut a = CameraShake {
            seed: 1,
            ..Default::default()
        };
        a.add_trauma(0.5);
        a.update(1.0 / 60.0, 0.0, 1.0, 1.0); // no decay: isolate the square law
        let oa = a.offset().length();
        let mut b = CameraShake {
            seed: 1,
            ..Default::default()
        };
        b.add_trauma(1.0);
        b.update(1.0 / 60.0, 0.0, 1.0, 1.0);
        let ob = b.offset().length();
        assert!(oa < ob * 0.35, "half trauma → ~quarter amplitude ({oa} vs {ob})");
    }

    #[test]
    fn distance_falloff() {
        let req = CameraShakeRequest {
            position: Vec3::ZERO,
            strength: 0.8,
            radius: 10.0,
        };
        assert!((CameraShake::trauma_from(&req, Vec3::ZERO) - 0.8).abs() < 1e-6);
        // 12 m on a 10 m radius: falloff 0.8 → 0.8 × 0.8 strength = 0.64.
        assert!((CameraShake::trauma_from(&req, Vec3::new(12.0, 0.0, 0.0)) - 0.64).abs() < 1e-6);
        assert_eq!(CameraShake::trauma_from(&req, Vec3::new(30.0, 0.0, 0.0)), 0.0);
    }

    #[test]
    fn the_public_system_applies_distance_falloff() {
        use regia_app::prelude::*;
        let mut app = App::new();
        app.insert_resource(VfxView::default())
            .insert_resource(CameraShake::default())
            .add_message::<CameraShakeRequest>()
            .add_systems(Update, apply_shake_requests);
        let eye = app.world().resource::<VfxView>().camera.eye;
        let send = |app: &mut App, offset: f32| {
            app.world_mut().write_message(CameraShakeRequest {
                position: eye + Vec3::new(offset, 0.0, 0.0),
                strength: 0.8,
                radius: 10.0,
            });
            app.update();
        };
        send(&mut app, 1000.0);
        assert_eq!(
            app.world().resource::<CameraShake>().trauma(),
            0.0,
            "far blast adds nothing"
        );
        send(&mut app, 12.0);
        let t = app.world().resource::<CameraShake>().trauma();
        assert!((t - 0.64).abs() < 1e-5, "12 m on a 10 m radius: {t}");
    }

    #[test]
    fn apply_helpers_compose() {
        let shake = CameraShake {
            offset: Vec3::new(0.1, 0.0, 0.0),
            roll: 0.05,
            ..Default::default()
        };
        assert_eq!(shake.apply(Vec3::ONE), Vec3::new(1.1, 1.0, 1.0));
        let q = shake.apply_rotation(Quat::IDENTITY, Vec3::Z);
        assert!(q.angle_between(Quat::IDENTITY) > 0.04);
    }
}
