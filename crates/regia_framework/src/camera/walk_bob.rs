// SPDX-License-Identifier: MPL-2.0
//! Walk bob for the first-person eye (ADR-2486): the camera rises, falls and sways with the steps, at a
//! stride length matched to the walking speed, so the steps the view shows land where feet would land.
//!
//! Opt-in. A [`CameraMode::FirstPerson`] rig bobs when its camera carries a [`WalkBob`], or when the mode's
//! [`FirstPerson::bob`] amplitude is above zero (the default [`WalkBob`] with that vertical amplitude).
//! Without either the eye stays rigid, as before.
//!
//! # Model
//!
//! - **Stride from speed.** Stride length (one full left-right cycle) follows the dynamic-similarity
//!   relation for legged gait: `stride = 2.3 · h · (v² / (g · h))^0.3`, with `h` the leg length and
//!   `v² / (g · h)` the Froude number. Walking turns into running at a Froude number of about 0.5.
//! - **Phase from distance, not time.** Each frame the phase advances by the distance the target moved
//!   divided by the stride at the smoothed speed. At a steady speed a footfall falls every half stride of
//!   ground covered, whatever the frame rate, so the view never "steps" faster or slower than the body moves
//!   (the fixed-frequency sine bob does: its steps slide along the ground when the speed changes).
//! - **Shape.** Walking is an inverted pendulum: the eye is lowest at each footfall (double support) and
//!   highest over the stance foot, two vertical cycles per stride. Running is a spring-mass bounce: lowest at
//!   mid-stance. The low point moves smoothly between the two around the walk-run Froude number. The eye sways
//!   toward the stance foot once per stride.
//! - **Amplitude from speed.** Amplitudes scale with the smoothed speed over [`WalkBob::reference_speed`]
//!   (capped by [`WalkBob::max_gain`]), so stopping fades the bob out instead of freezing it mid-step.
//! - **Gaze stabilization.** A walking head pitches against its own vertical motion so the eyes stay on a
//!   point ahead. With [`WalkBob::fixation_m`] above zero the camera pitches so a point that far ahead keeps
//!   its place on screen; the distant scene does not jiggle, only the near ground moves with the steps.
//!
//! The bob is added after the rig writes the eye pose each frame (the rig rewrites the pose from its target,
//! so nothing accumulates) and before shakes, which stack on top. Slopes: ground covered is measured along
//! the motion while it climbs at most [`WalkBob::max_slope_deg`], and horizontally above that (a lift, a
//! fall). A jump of more than [`WalkBob::teleport_m`] in one frame restarts the gait.
//!
//! A full-body first-person view (the eye on the head joint of an animated body) gets its bob from the
//! animation and should not carry a [`WalkBob`].
//!
//! Provenance (`docs/provenance.md`): stride relation and walk-run Froude number from R. McN. Alexander,
//! "Estimates of speeds of dinosaurs" (Nature 261, 1976) and "The gaits of bipedal and quadrupedal animals"
//! (Int. J. Robotics Research 3(2), 1984); walking as an inverted pendulum and running as a spring-mass system
//! from G. A. Cavagna et al., "Mechanical work in terrestrial locomotion" (Am. J. Physiol., 1977) and R.
//! Blickhan, "The spring-mass model for running and hopping" (J. Biomech. 22, 1989); head pitch compensating
//! vertical head motion during walking from T. Hirasaki et al., "Effects of walking velocity on vertical head
//! and body movements during locomotion" (Exp. Brain Res. 127, 1999). The code is original.

use std::f32::consts::TAU;

use regia_app::prelude::*;
use regia_core::math::{Vec3, det_atan, det_exp, det_powf, det_quat_from_yaw_pitch_roll, det_sin_cos};
use regia_scene::Transform;

use super::{CameraMode, CameraRig, ChaseCamera, FirstPerson, driver};

/// Standard gravity used by the Froude number, metres per second squared.
pub const GRAVITY: f32 = 9.81;
/// Froude number of the walk-run transition (Alexander 1984).
pub const WALK_RUN_FROUDE: f32 = 0.5;
/// Shortest stride the model returns, metres (very slow shuffling).
pub const MIN_STRIDE_M: f32 = 0.3;
/// Stride phase of the running low point after a footfall (mid-stance of a stance lasting about 0.36 of the
/// stride).
pub const RUN_LOW_PHASE: f32 = 0.18;

/// Stride length (one full left-right cycle, metres) at `speed` m/s for legs `leg_length` m long:
/// `2.3 · h · Fr^0.3`, never below [`MIN_STRIDE_M`]. Non-finite or non-positive input gives the minimum.
#[must_use]
pub fn stride_length(speed: f32, leg_length: f32) -> f32 {
    if !(speed.is_finite() && leg_length.is_finite()) || speed <= 0.0 || leg_length <= 0.0 {
        return MIN_STRIDE_M;
    }
    let froude = speed * speed / (GRAVITY * leg_length);
    (2.3 * leg_length * det_powf(froude, 0.3)).max(MIN_STRIDE_M)
}

/// The Froude number `v² / (g · h)` (0 for bad input).
#[must_use]
pub fn froude(speed: f32, leg_length: f32) -> f32 {
    if !(speed.is_finite() && leg_length.is_finite()) || leg_length <= 0.0 {
        return 0.0;
    }
    speed * speed / (GRAVITY * leg_length)
}

/// Steps per minute (two per stride) at `speed` for legs `leg_length` long.
#[must_use]
pub fn cadence_steps_per_minute(speed: f32, leg_length: f32) -> f32 {
    if !speed.is_finite() || speed <= 0.0 {
        return 0.0;
    }
    2.0 * 60.0 * speed / stride_length(speed, leg_length)
}

/// Walk bob settings of a first-person camera. See the module docs for the model.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct WalkBob {
    /// Vertical amplitude at [`Self::reference_speed`], metres (half the peak-to-peak travel).
    pub vertical_m: f32,
    /// Sideways sway amplitude at the reference speed, metres.
    pub lateral_m: f32,
    /// Roll amplitude at the reference speed, radians (0: no roll, the comfortable default).
    pub roll_rad: f32,
    /// The speed the amplitudes are given for, metres per second (a brisk walk).
    pub reference_speed: f32,
    /// Largest amplitude gain at high speed (amplitude · speed / reference, at most this).
    pub max_gain: f32,
    /// Leg length (hip height), metres: sets stride length and the walk-run speed.
    pub leg_length_m: f32,
    /// Gaze stabilization distance, metres: the camera pitches so a point this far ahead keeps its place on
    /// screen. 0 turns it off.
    pub fixation_m: f32,
    /// Speed smoothing rate, 1/second (exponential).
    pub smoothing: f32,
    /// Steepest motion counted along its length, degrees; steeper motion counts horizontally.
    pub max_slope_deg: f32,
    /// A move longer than this in one frame (metres) restarts the gait instead of stepping.
    pub teleport_m: f32,
}

impl Default for WalkBob {
    fn default() -> Self {
        Self {
            vertical_m: 0.012,
            lateral_m: 0.006,
            roll_rad: 0.0,
            reference_speed: 1.4,
            max_gain: 1.6,
            leg_length_m: 0.9,
            fixation_m: 12.0,
            smoothing: 10.0,
            max_slope_deg: 50.0,
            teleport_m: 3.0,
        }
    }
}

impl WalkBob {
    /// The settings a [`FirstPerson::bob`] amplitude above zero stands for: the defaults with that vertical
    /// amplitude and a sway of half of it.
    #[must_use]
    pub fn from_amplitude(vertical_m: f32) -> Self {
        Self {
            vertical_m,
            lateral_m: vertical_m * 0.5,
            ..Self::default()
        }
    }

    /// The settings with every value finite and in range (bad values fall back to the defaults or clamp).
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let pick =
            |v: f32, fallback: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { fallback };
        Self {
            vertical_m: pick(self.vertical_m, d.vertical_m, 0.0, 0.2),
            lateral_m: pick(self.lateral_m, d.lateral_m, 0.0, 0.2),
            roll_rad: pick(self.roll_rad, d.roll_rad, 0.0, 0.2),
            reference_speed: pick(self.reference_speed, d.reference_speed, 0.1, 20.0),
            max_gain: pick(self.max_gain, d.max_gain, 0.0, 10.0),
            leg_length_m: pick(self.leg_length_m, d.leg_length_m, 0.2, 3.0),
            fixation_m: pick(self.fixation_m, d.fixation_m, 0.0, 1000.0),
            smoothing: pick(self.smoothing, d.smoothing, 0.0, 100.0),
            max_slope_deg: pick(self.max_slope_deg, d.max_slope_deg, 0.0, 90.0),
            teleport_m: pick(self.teleport_m, d.teleport_m, 0.01, 1000.0),
        }
    }
}

/// Which foot is on the ground at the start of the current step.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StanceFoot {
    /// Left foot (stride phase 0 to 0.5).
    #[default]
    Left,
    /// Right foot (stride phase 0.5 to 1).
    Right,
}

impl StanceFoot {
    /// `"left"` or `"right"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// The walk bob's running state, kept on the camera ([`WalkBob`] or [`FirstPerson::bob`] adds it).
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct WalkBobState {
    /// Stride phase in `[0, 1)`: 0 is a left footfall, 0.5 a right one.
    pub phase: f32,
    /// Smoothed ground speed of the target, metres per second.
    pub speed: f32,
    /// Stride length at [`Self::speed`], metres.
    pub stride_m: f32,
    /// Footfalls since the gait started.
    pub steps: u64,
    /// Whether the gait is past the walk-run Froude number.
    pub running: bool,
    /// The bob offset added this frame (world axes), metres.
    pub offset: Vec3,
    /// The stabilizing pitch added this frame, radians (positive looks up).
    pub pitch: f32,
    /// The roll added this frame, radians.
    pub roll: f32,
    /// Whether [`Self::last_position`] holds the target's position on the previous frame.
    pub tracking: bool,
    /// The target's position on the previous frame.
    pub last_position: Vec3,
}

impl WalkBobState {
    /// The foot that landed last.
    #[must_use]
    pub fn stance_foot(&self) -> StanceFoot {
        if self.phase < 0.5 {
            StanceFoot::Left
        } else {
            StanceFoot::Right
        }
    }

    /// Ground covered by a move (metres): along the move while it climbs at most `max_slope_deg`,
    /// horizontally when steeper. The slope test compares squared climbs against a pinned
    /// `sin²/cos²`, so the branch is bit-identical across platforms, whatever their libm's `tan`
    /// says at the boundary (spec 35 § 12.13).
    #[must_use]
    pub fn ground_distance(displacement: Vec3, max_slope_deg: f32) -> f32 {
        let horizontal = Vec3::new(displacement.x, 0.0, displacement.z).length();
        let climb = displacement.y.abs();
        let (s, c) = det_sin_cos(max_slope_deg.to_radians());
        let slope_sq = (s * s) / (c * c);
        if climb * climb <= horizontal * horizontal * slope_sq {
            displacement.length()
        } else {
            horizontal
        }
    }

    /// Advances the gait by the target's move to `position` over `dt` seconds and returns the footfalls of
    /// this frame. The first call (or one after a teleport) only records the position.
    pub fn advance(&mut self, settings: &WalkBob, position: Vec3, dt: f32) -> u32 {
        let s = settings.sanitized();
        if !position.is_finite() {
            return 0;
        }
        if !self.tracking {
            self.tracking = true;
            self.last_position = position;
            return 0;
        }
        let displacement = position - self.last_position;
        self.last_position = position;
        if displacement.length() > s.teleport_m {
            self.speed = 0.0;
            self.phase = 0.0;
            return 0;
        }
        let distance = Self::ground_distance(displacement, s.max_slope_deg);
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        if dt > 0.0 {
            let instant = distance / dt;
            let k = if s.smoothing <= 0.0 {
                1.0
            } else {
                1.0 - det_exp(-s.smoothing * dt)
            };
            self.speed += (instant - self.speed) * k;
        }
        self.stride_m = stride_length(self.speed.max(0.05), s.leg_length_m);
        self.running = froude(self.speed, s.leg_length_m) > WALK_RUN_FROUDE;
        let before = self.phase;
        let advanced = before + distance / self.stride_m;
        // Footfalls: the half-stride boundaries crossed.
        let falls = ((advanced * 2.0).floor() - (before * 2.0).floor()).max(0.0) as u32;
        self.steps += u64::from(falls);
        self.phase = advanced.rem_euclid(1.0);
        falls
    }

    /// The eye's offset in its own heading frame (x right, y up) and roll, for the current phase
    /// and speed. The sinusoids are the pinned `det_sin_cos` series: bit-identical across
    /// platforms (spec 35 § 12.13).
    #[must_use]
    pub fn local_bob(&self, settings: &WalkBob) -> (f32, f32, f32) {
        let s = settings.sanitized();
        let gain = (self.speed / s.reference_speed).clamp(0.0, s.max_gain);
        let fr = froude(self.speed, s.leg_length_m);
        // The low point moves from the footfall (walk) to mid-stance (run) over Froude 0.4..0.6.
        let run_mix = ((fr - (WALK_RUN_FROUDE - 0.1)) / 0.2).clamp(0.0, 1.0);
        let low = run_mix * RUN_LOW_PHASE;
        let (sway_s, _) = det_sin_cos(TAU * self.phase);
        let (_, bounce_c) = det_sin_cos(2.0 * TAU * (self.phase - low));
        let vertical = -s.vertical_m * gain * bounce_c;
        // Toward the stance foot: left (−x) over the first half of the stride.
        let lateral = -s.lateral_m * gain * sway_s;
        let roll = s.roll_rad * gain * sway_s;
        (lateral, vertical, roll)
    }
}

/// What [`apply_walk_bob`] reads and writes on a camera.
type BobbedCamera<'a> = (
    Entity,
    &'a CameraRig,
    &'a ChaseCamera,
    &'a mut Transform,
    Option<&'a WalkBob>,
    Option<&'a mut WalkBobState>,
);

/// Adds the walk bob to every first-person rig that asks for it (after [`super::CameraSet::Update`]'s rig
/// pose, before the shakes).
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_walk_bob")]
fn apply_walk_bob(
    time: Res<Time>,
    motion: Option<Res<crate::settings::SceneMotion>>,
    mut commands: Commands,
    mut cameras: Query<BobbedCamera<'_>, Without<driver::CameraDriver>>,
    targets: Query<&Transform, (Without<CameraRig>, Without<ChaseCamera>)>,
) {
    let dt = time.delta_secs();
    let bob_scale = motion.map(|motion| motion.bob_scale).unwrap_or(1.0);
    for (entity, rig, chase, mut transform, bob, state) in &mut cameras {
        let CameraMode::FirstPerson(FirstPerson { bob: amplitude, .. }) = rig.mode else {
            continue;
        };
        let settings = match bob {
            Some(bob) => *bob,
            None if amplitude.is_finite() && amplitude > 0.0 => WalkBob::from_amplitude(amplitude),
            None => continue,
        };
        // The rig skipped the frame (no target): the pose is last frame's, already bobbed.
        let Ok(target) = targets.get(chase.target) else {
            continue;
        };
        let Some(mut state) = state else {
            let mut fresh = WalkBobState::default();
            fresh.advance(&settings, target.translation, dt);
            commands.entity(entity).try_insert(fresh);
            continue;
        };
        state.advance(&settings, target.translation, dt);
        if bob_scale == 0.0 {
            state.offset = Vec3::ZERO;
            state.pitch = 0.0;
            state.roll = 0.0;
            continue;
        }
        let (mut lateral, mut vertical, mut roll) = state.local_bob(&settings);
        if bob_scale != 1.0 {
            lateral *= bob_scale;
            vertical *= bob_scale;
            roll *= bob_scale;
        }
        let forward = transform.rotation * Vec3::NEG_Z;
        let flat = Vec3::new(forward.x, 0.0, forward.z);
        let right = if flat.length_squared() > 1e-8 {
            flat.normalize().cross(Vec3::Y)
        } else {
            transform.rotation * Vec3::X
        };
        let offset = right * lateral + Vec3::Y * vertical;
        let s = settings.sanitized();
        // Pitch against the vertical motion so a point `fixation_m` ahead stays put on screen.
        let pitch = if s.fixation_m > 0.0 {
            det_atan(-vertical / s.fixation_m)
        } else {
            0.0
        };
        transform.translation += offset;
        transform.rotation = transform.rotation
            * det_quat_from_yaw_pitch_roll(0.0, pitch, 0.0)
            * det_quat_from_yaw_pitch_roll(0.0, 0.0, roll);
        state.offset = offset;
        state.pitch = pitch;
        state.roll = roll;
    }
}

/// Registers the walk bob types and its system.
pub(super) fn install(app: &mut App) {
    app.register_type::<WalkBob>()
        .register_type::<WalkBobState>()
        .register_type::<StanceFoot>()
        .add_systems(
            PostUpdate,
            apply_walk_bob
                .in_set(super::CameraSet::Update)
                .after(super::update_rigs)
                .before(PostUpdateSet::TransformPropagate),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    /// Walks a straight line at `speed` for `seconds` and returns the ground distance of every footfall.
    fn footfalls(speed: f32, seconds: f32, settings: &WalkBob) -> (WalkBobState, Vec<f32>) {
        let mut state = WalkBobState::default();
        let mut x = 0.0f32;
        state.advance(settings, Vec3::ZERO, DT);
        let mut falls = Vec::new();
        let frames = (seconds / DT).round() as usize;
        for _ in 0..frames {
            x += speed * DT;
            if state.advance(settings, Vec3::new(x, 0.0, 0.0), DT) > 0 {
                falls.push(x);
            }
        }
        (state, falls)
    }

    #[test]
    fn stride_grows_with_speed_like_human_gait() {
        // A 0.9 m leg: about 1.3-1.5 m at 1.4 m/s (a normal walk), longer when running.
        let walk = stride_length(1.4, 0.9);
        assert!((1.2..1.6).contains(&walk), "walk stride {walk}");
        let slow = stride_length(0.7, 0.9);
        let run = stride_length(3.5, 0.9);
        assert!(slow < walk && walk < run, "{slow} {walk} {run}");
        // Cadence rises with speed too (stride grows slower than speed).
        assert!(cadence_steps_per_minute(0.7, 0.9) < cadence_steps_per_minute(1.4, 0.9));
        let cadence = cadence_steps_per_minute(1.4, 0.9);
        assert!((100.0..140.0).contains(&cadence), "cadence {cadence}");
        assert_eq!(stride_length(0.0, 0.9), MIN_STRIDE_M);
        assert_eq!(stride_length(f32::NAN, 0.9), MIN_STRIDE_M);
        // Walk-run transition near 2.1 m/s for a 0.9 m leg.
        assert!(froude(2.0, 0.9) < WALK_RUN_FROUDE && froude(2.2, 0.9) > WALK_RUN_FROUDE);
    }

    #[test]
    fn footfalls_land_every_half_stride_of_ground_at_every_speed() {
        let settings = WalkBob::default();
        for speed in [0.6f32, 1.0, 1.4, 2.0, 3.0] {
            let (state, falls) = footfalls(speed, 12.0, &settings);
            let expected = stride_length(speed, settings.leg_length_m) * 0.5;
            // Skip the speed ramp-up at the start.
            let spacing: Vec<f32> = falls.windows(2).skip(4).map(|w| w[1] - w[0]).collect();
            assert!(!spacing.is_empty(), "speed {speed}: no steps");
            for gap in &spacing {
                // One frame of travel is the quantization of a footfall.
                assert!(
                    (gap - expected).abs() <= speed * DT + 1e-3,
                    "speed {speed}: step {gap} vs {expected}"
                );
            }
            assert!((state.stride_m - expected * 2.0).abs() < 1e-3);
        }
    }

    #[test]
    fn steps_follow_distance_not_frame_rate() {
        let settings = WalkBob::default();
        // The same 10 m at 1.4 m/s, sampled at 30 and 144 Hz: the same number of footfalls.
        let mut counts = Vec::new();
        for hz in [30.0f32, 144.0] {
            let dt = 1.0 / hz;
            let mut state = WalkBobState::default();
            state.advance(&settings, Vec3::ZERO, dt);
            // Settle the speed first, then count over exactly 10 m.
            let mut x = 0.0;
            for _ in 0..(hz as usize * 2) {
                x += 1.4 * dt;
                state.advance(&settings, Vec3::new(x, 0.0, 0.0), dt);
            }
            let start = state.steps;
            let frames = (10.0 / (1.4 * dt)).round() as usize;
            for _ in 0..frames {
                x += 1.4 * dt;
                state.advance(&settings, Vec3::new(x, 0.0, 0.0), dt);
            }
            counts.push(state.steps - start);
        }
        assert!(counts[0].abs_diff(counts[1]) <= 1, "{counts:?}");
        let expected = 10.0 / (stride_length(1.4, 0.9) * 0.5);
        assert!(
            (counts[1] as f32 - expected).abs() <= 1.0,
            "{counts:?} vs {expected}"
        );
    }

    #[test]
    fn walking_eye_is_lowest_at_footfalls_and_running_eye_at_mid_stance() {
        let settings = WalkBob::default();
        let at = |speed: f32, phase: f32| {
            let state = WalkBobState {
                phase,
                speed,
                ..WalkBobState::default()
            };
            state.local_bob(&settings).1
        };
        // Walk at 1.4 m/s: minimum at phase 0 and 0.5, maximum at 0.25.
        assert!(at(1.4, 0.0) < at(1.4, 0.1) && at(1.4, 0.25) > at(1.4, 0.1));
        assert!((at(1.4, 0.0) - at(1.4, 0.5)).abs() < 1e-6);
        // The mean over a stride is zero: the bob never shifts the eye height.
        let mean: f32 = (0..100).map(|i| at(1.4, i as f32 / 100.0)).sum::<f32>() / 100.0;
        assert!(mean.abs() < 1e-5, "{mean}");
        // Run at 3.5 m/s: the lowest point is at mid-stance, not at the footfall.
        let run_low = (0..200)
            .map(|i| i as f32 / 400.0)
            .min_by(|a, b| at(3.5, *a).total_cmp(&at(3.5, *b)))
            .unwrap_or(0.0);
        assert!((run_low - RUN_LOW_PHASE).abs() < 0.01, "{run_low}");
        // Standing still: no bob.
        assert_eq!(at(0.0, 0.3), 0.0);
    }

    #[test]
    fn sway_leans_toward_the_stance_foot() {
        let settings = WalkBob::default();
        let state = |phase| WalkBobState {
            phase,
            speed: 1.4,
            ..WalkBobState::default()
        };
        assert!(state(0.25).local_bob(&settings).0 < 0.0, "left stance: sway left");
        assert!(
            state(0.75).local_bob(&settings).0 > 0.0,
            "right stance: sway right"
        );
        assert_eq!(state(0.25).stance_foot(), StanceFoot::Left);
        assert_eq!(state(0.75).stance_foot(), StanceFoot::Right);
    }

    #[test]
    fn slopes_count_along_the_ground_and_lifts_do_not_step() {
        // 30 degrees: along the slope.
        let up = Vec3::new(1.0, 30f32.to_radians().tan(), 0.0);
        assert!((WalkBobState::ground_distance(up, 50.0) - up.length()).abs() < 1e-5);
        // Straight up (a lift, a ladder fall): horizontal only.
        assert_eq!(WalkBobState::ground_distance(Vec3::new(0.0, 1.0, 0.0), 50.0), 0.0);
        let settings = WalkBob::default();
        let mut state = WalkBobState::default();
        state.advance(&settings, Vec3::ZERO, DT);
        for i in 1..120 {
            state.advance(&settings, Vec3::new(0.0, i as f32 * 0.05, 0.0), DT);
        }
        assert_eq!(state.steps, 0);
        assert!(state.speed < 1e-6);
    }

    #[test]
    fn a_teleport_restarts_the_gait_and_stopping_fades_the_bob() {
        let settings = WalkBob::default();
        let (mut state, _) = footfalls(1.4, 3.0, &settings);
        let here = state.last_position;
        assert_eq!(state.advance(&settings, here + Vec3::new(50.0, 0.0, 0.0), DT), 0);
        assert_eq!(state.speed, 0.0);
        // Walk again, then stop: the amplitude decays to nothing within a second.
        let (mut state, _) = footfalls(1.4, 3.0, &settings);
        let stop = state.last_position;
        for _ in 0..60 {
            state.advance(&settings, stop, DT);
        }
        let (lateral, vertical, _) = state.local_bob(&settings);
        assert!(
            vertical.abs() < 1e-4 && lateral.abs() < 1e-4,
            "{lateral} {vertical}"
        );
    }

    #[test]
    fn bad_settings_are_sanitized() {
        let bad = WalkBob {
            vertical_m: f32::NAN,
            reference_speed: -1.0,
            leg_length_m: f32::INFINITY,
            ..WalkBob::default()
        };
        let s = bad.sanitized();
        assert_eq!(s.vertical_m, WalkBob::default().vertical_m);
        assert!(s.reference_speed > 0.0);
        assert_eq!(s.leg_length_m, WalkBob::default().leg_length_m);
        let mut state = WalkBobState::default();
        state.advance(&bad, Vec3::ZERO, DT);
        state.advance(&bad, Vec3::new(0.02, 0.0, 0.0), DT);
        let (l, v, r) = state.local_bob(&bad);
        assert!(l.is_finite() && v.is_finite() && r.is_finite());
        // A NaN position is ignored.
        assert_eq!(state.advance(&bad, Vec3::NAN, DT), 0);
    }

    #[test]
    fn a_squashed_slope_test_picks_one_branch_pinned_by_bits() {
        // A 30-degree move judged against a 30-degree limit: the branch decision comes from the
        // pinned sin²/cos² comparison, so it is the same bits on every platform (the platform
        // `tan` used to decide it in the last digit). The 30-degree climb is built from the
        // pinned sin/cos (s/c = tan from pinned bits): the branch input itself is pinned (Muse
        // read-only review of fix-framework1-determinism, low finding 7).
        let (s30, c30) = det_sin_cos(30f32.to_radians());
        let up = Vec3::new(1.0, s30 / c30, 0.0);
        let d = WalkBobState::ground_distance(up, 30.0);
        assert!(
            d > up.length() - 1.0e-4,
            "a 30-degree slope counts along the ground"
        );
        assert_ne!(
            d.to_bits(),
            Vec3::new(up.x, 0.0, up.z).length().to_bits(),
            "and not as the horizontal projection"
        );
        // Steeper than the limit: the horizontal projection, exactly.
        let steep = WalkBobState::ground_distance(up * 2.0, 20.0);
        assert_eq!(steep, 2.0);
        // Straight up (a lift, a ladder fall): horizontal only.
        assert_eq!(WalkBobState::ground_distance(Vec3::new(0.0, 1.0, 0.0), 50.0), 0.0);
    }

    #[test]
    fn a_sixty_frame_walk_pins_phase_and_steps_bits() {
        let settings = WalkBob::default();
        let mut state = WalkBobState::default();
        state.advance(&settings, Vec3::ZERO, DT);
        let mut x = 0.0f32;
        for _ in 0..60 {
            x += 1.4 * DT;
            state.advance(&settings, Vec3::new(x, 0.0, 0.0), DT);
        }
        assert_eq!(state.phase.to_bits(), 0x3e38_5310);
        assert_eq!(state.steps, 2);
        assert_eq!(state.stride_m.to_bits(), 0x3fa8_aedf);
        assert_eq!(state.speed.to_bits(), 0x3fb3_3112);
    }

    mod in_app {
        use std::time::Duration;

        use super::super::super::{CameraPlugin, CameraRig, ChaseCamera};
        use super::*;

        const STEP: Duration = Duration::from_millis(16);
        const EYE: f32 = 1.6;

        /// A first-person camera on a pawn; `bob` on the camera, `amplitude` in the mode.
        fn app(bob: Option<WalkBob>, amplitude: f32) -> (App, Entity, Entity) {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, CameraPlugin))
                .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
            let target = app.world_mut().spawn(Transform::default()).id();
            let rig = CameraRig {
                mode: CameraMode::FirstPerson(FirstPerson {
                    eye_offset: Vec3::new(0.0, EYE, 0.0),
                    bob: amplitude,
                }),
                yaw: 0.0,
                pitch: 0.0,
                ..CameraRig::default()
            };
            let camera = app
                .world_mut()
                .spawn((Transform::default(), rig, ChaseCamera { target }))
                .id();
            if let Some(bob) = bob {
                app.world_mut().entity_mut(camera).insert(bob);
            }
            (app, camera, target)
        }

        /// Walks the pawn along -Z at `speed` for `frames` frames; returns the camera heights.
        fn walk(app: &mut App, camera: Entity, target: Entity, speed: f32, frames: usize) -> Vec<f32> {
            let mut heights = Vec::new();
            for _ in 0..frames {
                if let Some(mut t) = app.world_mut().get_mut::<Transform>(target) {
                    t.translation.z -= speed * STEP.as_secs_f32();
                }
                app.update();
                heights.push(
                    app.world()
                        .get::<Transform>(camera)
                        .map_or(f32::NAN, |t| t.translation.y),
                );
            }
            heights
        }

        #[test]
        fn without_opt_in_the_eye_stays_rigid() {
            let (mut app, camera, target) = app(None, 0.0);
            let heights = walk(&mut app, camera, target, 1.4, 120);
            assert!(heights.iter().all(|h| (h - EYE).abs() < 1e-5), "{heights:?}");
            assert!(app.world().get::<WalkBobState>(camera).is_none());
        }

        #[test]
        fn a_walking_eye_bobs_around_its_height_with_stride_matched_steps() {
            let bob = WalkBob::default();
            let (mut app, camera, target) = app(Some(bob), 0.0);
            // Two seconds to settle the speed, then six seconds measured.
            walk(&mut app, camera, target, 1.4, 120);
            let start_steps = app.world().get::<WalkBobState>(camera).map_or(0, |s| s.steps);
            let heights = walk(&mut app, camera, target, 1.4, 375);
            let state = *app.world().get::<WalkBobState>(camera).expect("state");
            let (lo, hi) = heights
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), h| (lo.min(*h), hi.max(*h)));
            let mean = heights.iter().sum::<f32>() / heights.len() as f32;
            assert!(
                (hi - lo - 2.0 * bob.vertical_m).abs() < 0.004,
                "peak to peak {}",
                hi - lo
            );
            assert!((mean - EYE).abs() < 0.002, "mean eye height {mean}");
            // 375 frames of 16 ms at 1.4 m/s = 8.4 m: one footfall per half stride.
            let expected = 8.4 / (stride_length(1.4, bob.leg_length_m) * 0.5);
            let steps = (state.steps - start_steps) as f32;
            assert!(
                (steps - expected).abs() <= 1.0,
                "{steps} steps, expected {expected}"
            );
            assert!(!state.running);
            // Gaze stabilization pitches against the drop.
            assert!(state.pitch.abs() <= (bob.vertical_m * 1.7 / bob.fixation_m) + 1e-4);
        }

        #[test]
        fn the_mode_amplitude_alone_turns_the_bob_on() {
            let (mut app, camera, target) = app(None, 0.02);
            walk(&mut app, camera, target, 1.4, 90);
            let heights = walk(&mut app, camera, target, 1.4, 120);
            let (lo, hi) = heights
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), h| (lo.min(*h), hi.max(*h)));
            assert!(hi - lo > 0.03, "peak to peak {}", hi - lo);
            assert!(app.world().get::<WalkBobState>(camera).is_some());
        }

        #[test]
        fn reduced_motion_scale_stops_the_walk_bob() {
            let (mut app, camera, target) = app(None, 0.02);
            app.insert_resource(crate::settings::SceneMotion {
                shake_scale: 0.0,
                bob_scale: 0.0,
                fov_override_deg: None,
            });
            let heights = walk(&mut app, camera, target, 1.4, 180);
            assert!(
                heights.iter().all(|h| (h - EYE).abs() < 1e-4),
                "reduced motion keeps the eye at {EYE}: {heights:?}"
            );
        }
    }
}
