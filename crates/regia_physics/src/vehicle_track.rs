// SPDX-License-Identifier: MPL-2.0
//! Planar skid-steer tracks (ADR-4629).
//!
//! Two tracks share a rigid body on the XZ plane. Each track has a speed in meters per second,
//! positive forward (local −Z). The body does not use the wheeled vehicle: [`step`] and
//! [`step_for`] are pure functions.
//!
//! # Speeds
//!
//! A throttle is a finite number. Values outside −1..=1 are clamped. Throttle `t` requests the
//! track speed `t` times [`MAX_TRACK_SPEED_M_S`]. While the throttle is not zero, the speed moves
//! toward that request by at most [`FRICTION_ACCEL_M_S2`] meters per second each second, so one
//! step of length `dt` changes a driven track by at most `FRICTION_ACCEL_M_S2 * dt`.
//!
//! A throttle of zero (including −0) does not request a speed. The track coasts toward zero at
//! [`COAST_DRAG_M_S2`] meters per second each second, and that coast is also capped by the
//! friction limit. The speed stops at zero. It does not reverse under drag alone.
//!
//! # Yaw and the center speed
//!
//! The center (longitudinal) speed is the average of the two track speeds. The yaw rate, in
//! radians per second about +Y, is their difference divided by the track separation:
//!
//! ```text
//! longitudinal_speed = (left_speed + right_speed) / 2
//! yaw_rate           = (right_speed - left_speed) / TRACK_SEPARATION_M
//! ```
//!
//! [`TRACK_SEPARATION_M`] is 2 m, the distance between the track centerlines. Positive yaw is a
//! right-handed turn about +Y: the nose (local −Z) swings toward −X, which is a left turn. The
//! left track faster than the right makes `yaw_rate` negative, and the nose swings toward +X,
//! a right turn.
//!
//! # The step
//!
//! Each step updates the track speeds first, then moves the pose with the average of the speeds
//! at the start and the end of the step (and the yaw rate of those averages). From rest, while
//! both accelerations stay constant, the yaw after `T` seconds (until it wraps past PI; the step
//! publishes it wrapped to (-PI, PI], ADR-10062) is
//!
//! ```text
//! yaw(T) = (accel_right - accel_left) * T² / (2 * TRACK_SEPARATION_M)
//! ```
//!
//! [`yaw_closed_form`] is that expression. A full forward throttle from rest accelerates at
//! [`FRICTION_ACCEL_M_S2`] until [`MAX_TRACK_SPEED_M_S`]. With the right throttle held at zero
//! the right acceleration is 0, so one second of that pair is −1 rad.
//!
//! The same inputs produce the same [`TrackState::digest`].
//!
//! A non-finite throttle, speed, pose or time step, or a time step that is not positive, is
//! [`TrackError::BadInput`] (`track_bad_input`).
//!
//! # Lockstep model (ADR-8551)
//!
//! The step is `regia_detsim::ground_vehicle::tracked::step` with this module's constants (its
//! `TrackedParams::DEFAULT`) on a flat floor at height 0: lockstep arithmetic, the same bits on
//! every target. This module keeps its API, its checks and its error. A lockstep game that wants
//! slopes, other numbers or the pose in 3D calls `regia_detsim::ground_vehicle::tracked` directly.

use regia_core::math::Vec3;
use regia_detsim::ground_vehicle::WorldPhysics;
use regia_detsim::ground_vehicle::tracked::{self, TrackedInput, TrackedParams, TrackedState};
use regia_detsim::hover::host::FlatFloor;
use regia_detsim::math as det_math;
use thiserror::Error;
use tracing::debug_span;

/// Distance between the track centerlines, in meters.
///
/// The yaw rate is `(right_speed - left_speed) / TRACK_SEPARATION_M`.
pub const TRACK_SEPARATION_M: f32 = 2.0;

/// Most steps one [`step_for`] call runs (the agent's `physics.track_step` limit too): the loop
/// runs on the caller's thread, so a larger count is refused as [`TrackError::BadInput`] before
/// any step (ADR-7431). 10 000 steps is 156 s at 64 Hz; call again to go further.
pub const MAX_TRACK_STEPS: u32 = 10_000;

/// Track speed at throttle +1, in meters per second. Throttle −1 requests the negation.
pub const MAX_TRACK_SPEED_M_S: f32 = 8.0;

/// The most a track's speed may change in one second, in meters per second squared.
///
/// A driven step changes a track by at most `FRICTION_ACCEL_M_S2 * dt`. The coast uses the
/// same cap.
pub const FRICTION_ACCEL_M_S2: f32 = 4.0;

/// Coast deceleration at zero throttle, in meters per second squared.
///
/// The speed moves toward zero by `COAST_DRAG_M_S2 * dt` each step, and by no more than the
/// friction cap. With the shipped numbers the drag is the tighter of the two.
pub const COAST_DRAG_M_S2: f32 = 1.0;

/// Why a track step was refused. [`TrackError::code`] is the stable agent code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TrackError {
    /// A throttle, speed, pose or time step is not finite, the time step is not positive, or a
    /// step count is past [`MAX_TRACK_STEPS`].
    #[error(
        "a track throttle, speed, pose or time step is not finite, the time step is not positive, or the step \
         count is past the limit"
    )]
    BadInput,
}

impl TrackError {
    /// The stable identifier: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadInput => "track_bad_input",
        }
    }
}

/// Throttles for one step. +1 is full forward, −1 is full reverse, 0 coasts.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrackCommand {
    /// Left track throttle. Finite values are clamped to −1..=1.
    pub left_throttle: f32,
    /// Right track throttle. Finite values are clamped to −1..=1.
    pub right_throttle: f32,
}

/// Pose and track speeds of one skid-steer body.
///
/// `x` and `z` are meters on the world XZ plane (Y is up and stays 0). `yaw` is radians about
/// +Y, right-handed. Speeds are meters per second, positive forward.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrackState {
    /// Left track speed, meters per second.
    pub left_speed: f32,
    /// Right track speed, meters per second.
    pub right_speed: f32,
    /// Heading about +Y, radians, wrapped to (-PI, PI] after every step (ADR-10062).
    pub yaw: f32,
    /// World X, meters.
    pub x: f32,
    /// World Z, meters.
    pub z: f32,
    /// How many steps have been applied to this state.
    pub steps: u64,
}

impl TrackState {
    /// Center speed: the average of the two track speeds, meters per second.
    #[must_use]
    pub fn longitudinal_speed(self) -> f32 {
        0.5 * (self.left_speed + self.right_speed)
    }

    /// Yaw rate about +Y, radians per second.
    ///
    /// `(right_speed - left_speed) / TRACK_SEPARATION_M`. Negative when the left track is faster,
    /// which turns the nose to the right.
    #[must_use]
    pub fn yaw_rate(self) -> f32 {
        (self.right_speed - self.left_speed) / TRACK_SEPARATION_M
    }

    /// World-space nose direction (local −Z turned by [`Self::yaw`]): `(-sin(yaw), 0, -cos(yaw))`
    /// with the lockstep sine and cosine (`regia_detsim::math`), the heading [`step`] integrates.
    /// Before ADR-8638 it came from a quaternion rotation (platform trig), one ulp off the
    /// integrated nose on some yaws.
    #[must_use]
    pub fn forward(self) -> Vec3 {
        Vec3::new(-det_math::sin(self.yaw), 0.0, -det_math::cos(self.yaw))
    }

    /// A platform-independent digest of this state. Two runs with the same inputs match.
    #[must_use]
    pub fn digest(self) -> u64 {
        // FNV-1a 64 (Fowler–Noll–Vo, public domain), the same mixing the physics world uses.
        // Encoding 1: five f32 bit patterns, then the step counter, little-endian.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ 1;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for value in [self.left_speed, self.right_speed, self.yaw, self.x, self.z] {
            hash = fnv_bytes(hash, &value.to_bits().to_le_bytes());
        }
        fnv_bytes(hash, &self.steps.to_le_bytes())
    }
}

fn fnv_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Yaw in radians after `time_s` seconds of constant track accelerations from rest, while
/// neither track has reached [`MAX_TRACK_SPEED_M_S`].
///
/// `yaw = (accel_right_m_s2 - accel_left_m_s2) * time_s² / (2 * TRACK_SEPARATION_M)`.
///
/// [`step_for`] uses the average speed over each step, so a run that stays in this regime
/// lands on this value.
#[must_use]
pub fn yaw_closed_form(accel_left_m_s2: f32, accel_right_m_s2: f32, time_s: f32) -> f32 {
    (accel_right_m_s2 - accel_left_m_s2) * time_s * time_s / (2.0 * TRACK_SEPARATION_M)
}

/// One step of `dt` seconds.
///
/// # Errors
///
/// [`TrackError::BadInput`] when a throttle, a speed, the pose or `dt` is not finite, or when
/// `dt` is not positive. `steps == 0` in [`step_for`] still checks the inputs and changes nothing.
pub fn step(state: &TrackState, command: TrackCommand, dt: f32) -> Result<TrackState, TrackError> {
    step_for(state, command, dt, 1)
}

/// `steps` repeats of [`step`].
///
/// The loop is the per-frame entry a game calls, so it sits in a `track_drive` span.
///
/// # Errors
///
/// [`TrackError::BadInput`], including when a step would leave a non-finite pose or `steps` is
/// past [`MAX_TRACK_STEPS`]. Zero steps returns `state` unchanged after the same check.
pub fn step_for(
    state: &TrackState,
    command: TrackCommand,
    dt: f32,
    steps: u32,
) -> Result<TrackState, TrackError> {
    let _span = debug_span!("track_drive", steps).entered();
    check_inputs(state, command, dt)?;
    if steps > MAX_TRACK_STEPS {
        return Err(TrackError::BadInput);
    }
    let mut state = *state;
    for _ in 0..steps {
        state = advance(state, command, dt).ok_or(TrackError::BadInput)?;
        if !pose_is_finite(&state) {
            return Err(TrackError::BadInput);
        }
    }
    Ok(state)
}

fn check_inputs(state: &TrackState, command: TrackCommand, dt: f32) -> Result<(), TrackError> {
    if dt.is_finite()
        && dt > 0.0
        && command.left_throttle.is_finite()
        && command.right_throttle.is_finite()
        && pose_is_finite(state)
    {
        Ok(())
    } else {
        Err(TrackError::BadInput)
    }
}

fn pose_is_finite(state: &TrackState) -> bool {
    state.left_speed.is_finite()
        && state.right_speed.is_finite()
        && state.yaw.is_finite()
        && state.x.is_finite()
        && state.z.is_finite()
}

/// This module's constants as the lockstep model's parameters (equal to `TrackedParams::DEFAULT`).
const DET_PARAMS: TrackedParams = TrackedParams {
    separation_m: TRACK_SEPARATION_M,
    max_track_speed_m_s: MAX_TRACK_SPEED_M_S,
    friction_accel_m_s2: FRICTION_ACCEL_M_S2,
    coast_drag_m_s2: COAST_DRAG_M_S2,
    ..TrackedParams::DEFAULT
};

/// Speeds, then the pose, from inputs [`check_inputs`] has already accepted: one lockstep tracked
/// step on a flat floor at height 0 (ADR-8551). `None` when the step overflowed: the lockstep step
/// then rests the vehicle (`recovered`, ADR-8638), and this API keeps answering `BadInput` for it.
fn advance(state: TrackState, command: TrackCommand, dt: f32) -> Option<TrackState> {
    let mut det = TrackedState {
        pos: [state.x, 0.0, state.z],
        yaw: state.yaw,
        left_speed: state.left_speed,
        right_speed: state.right_speed,
        pitch: 0.0,
        roll: 0.0,
    };
    let input = TrackedInput {
        left_throttle: command.left_throttle,
        right_throttle: command.right_throttle,
    };
    let report = tracked::step(
        &DET_PARAMS,
        &WorldPhysics::EARTH,
        &FlatFloor(0.0),
        &mut det,
        &input,
        dt,
    );
    if report.recovered {
        return None;
    }
    Some(TrackState {
        left_speed: det.left_speed,
        right_speed: det.right_speed,
        yaw: det.yaw,
        x: det.pos[0],
        z: det.pos[2],
        steps: state.steps.saturating_add(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    /// 1/64 s is exact in f32, so 64 steps is one second and 128 steps is two.
    const DT: f32 = 1.0 / 64.0;
    const STEPS_1S: u32 = 64;
    const STEPS_2S: u32 = 128;

    fn drive(left: f32, right: f32, steps: u32) -> TrackState {
        match step_for(
            &TrackState::default(),
            TrackCommand {
                left_throttle: left,
                right_throttle: right,
            },
            DT,
            steps,
        ) {
            Ok(state) => state,
            Err(error) => panic!("step failed with {}", error.code()),
        }
    }

    #[test]
    fn the_shipped_numbers_match_the_documented_model() {
        assert_eq!(TRACK_SEPARATION_M, 2.0);
        assert_eq!(MAX_TRACK_SPEED_M_S, 8.0);
        assert_eq!(FRICTION_ACCEL_M_S2, 4.0);
        assert_eq!(COAST_DRAG_M_S2, 1.0);
        let sample = TrackState {
            left_speed: 3.0,
            right_speed: 1.0,
            ..TrackState::default()
        };
        assert!((sample.longitudinal_speed() - 2.0).abs() < 1e-6);
        assert!((sample.yaw_rate() - (1.0 - 3.0) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn positive_yaw_turns_the_nose_left() {
        let nose = Quat::from_rotation_y(0.5) * Vec3::NEG_Z;
        assert!(
            nose.x < 0.0,
            "positive yaw swings local -Z toward -X, a left turn"
        );
    }

    #[test]
    fn equal_throttle_for_two_seconds_moves_forward_with_no_yaw() {
        let state = drive(1.0, 1.0, STEPS_2S);
        assert!(
            state.yaw.abs() < 1e-4,
            "equal tracks hold a straight heading, yaw {}",
            state.yaw
        );
        // The displacement along the nose is the closed-form distance ½ a T² with
        // a = FRICTION_ACCEL and T = 2, which is 8 m. Equal tracks keep the nose on −Z.
        let moved = Vec3::new(state.x, 0.0, state.z);
        let along = moved.dot(state.forward());
        assert!(along > 1.0, "the body moved forward, along-nose {along}");
        let time = f64::from(DT) * f64::from(STEPS_2S);
        let expected = 0.5 * f64::from(FRICTION_ACCEL_M_S2) * time * time;
        assert!(
            (f64::from(along) - expected).abs() <= 0.01 * expected,
            "forward {along} m, closed form {expected} m"
        );
        assert!(state.x.abs() < 1e-4, "no side slip on equal throttles");
    }

    #[test]
    fn left_track_faster_yaws_right_and_matches_the_closed_form() {
        let state = drive(1.0, 0.0, STEPS_1S);
        let time = f64::from(DT) * f64::from(STEPS_1S);
        // Left accelerates at the friction cap; the right throttle is zero so it stays at rest.
        // yaw = (a_right - a_left) * T² / (2 * separation).
        let expected =
            (0.0 - f64::from(FRICTION_ACCEL_M_S2)) * time * time / (2.0 * f64::from(TRACK_SEPARATION_M));
        let yaw = f64::from(state.yaw);
        assert!(yaw < 0.0, "left track faster yaws right, yaw {yaw}");
        assert!(
            (yaw - expected).abs() <= 0.01 * expected.abs(),
            "yaw {yaw} is not within 1 percent of the closed form {expected}"
        );
        let nose = state.forward();
        assert!(nose.x > 0.0, "the nose turned toward +X, the right");
        let published = f64::from(yaw_closed_form(FRICTION_ACCEL_M_S2, 0.0, DT * (STEPS_1S as f32)));
        assert!((published - expected).abs() < 1e-6);
    }

    #[test]
    fn opposite_throttles_spin_in_place() {
        let state = drive(1.0, -1.0, STEPS_1S);
        assert!(
            state.longitudinal_speed().abs() < 1e-3,
            "center speed {}",
            state.longitudinal_speed()
        );
        assert!(
            state.x.abs() < 1e-3 && state.z.abs() < 1e-3,
            "the center did not translate"
        );
        assert!(state.yaw < -0.5, "opposite tracks still yaw, yaw {}", state.yaw);
    }

    #[test]
    fn a_nan_throttle_is_track_bad_input() {
        let cases = [
            TrackCommand {
                left_throttle: f32::NAN,
                right_throttle: 0.0,
            },
            TrackCommand {
                left_throttle: 0.0,
                right_throttle: f32::NAN,
            },
            TrackCommand {
                left_throttle: f32::INFINITY,
                right_throttle: 1.0,
            },
            TrackCommand {
                left_throttle: 1.0,
                right_throttle: f32::NEG_INFINITY,
            },
        ];
        for command in cases {
            let error = step(&TrackState::default(), command, DT).expect_err("refused");
            assert_eq!(error.code(), "track_bad_input");
        }
        let poisoned = TrackState {
            left_speed: f32::NAN,
            ..TrackState::default()
        };
        let error = step(
            &poisoned,
            TrackCommand {
                left_throttle: 0.0,
                right_throttle: 0.0,
            },
            DT,
        )
        .expect_err("a NaN speed is refused");
        assert_eq!(error.code(), "track_bad_input");
        assert!(step(&TrackState::default(), TrackCommand::default(), 0.0).is_err());
        assert!(step(&TrackState::default(), TrackCommand::default(), -DT).is_err());
    }

    #[test]
    fn two_runs_share_a_digest() {
        let command = TrackCommand {
            left_throttle: 0.5,
            right_throttle: -0.25,
        };
        let once = step_for(&TrackState::default(), command, DT, STEPS_2S).expect("run");
        let twice = step_for(&TrackState::default(), command, DT, STEPS_2S).expect("run");
        assert_eq!(once.digest(), twice.digest());
        assert_eq!(once, twice);
        let other = drive(1.0, 1.0, STEPS_2S);
        assert_ne!(once.digest(), other.digest());
    }

    #[test]
    fn zero_throttle_coasts_by_the_documented_drag() {
        let moving = TrackState {
            left_speed: 4.0,
            right_speed: 4.0,
            ..TrackState::default()
        };
        let coasted = step(&moving, TrackCommand::default(), 0.5).expect("coast");
        assert!((coasted.left_speed - 3.5).abs() < 1e-6);
        assert!((coasted.right_speed - 3.5).abs() < 1e-6);
        let crawling = TrackState {
            left_speed: 0.2,
            right_speed: -0.2,
            ..TrackState::default()
        };
        let stopped = step(&crawling, TrackCommand::default(), 1.0).expect("stop");
        assert_eq!(stopped.left_speed, 0.0);
        assert_eq!(stopped.right_speed, 0.0);
    }

    #[test]
    fn friction_caps_the_speed_change() {
        let stepped = step(
            &TrackState::default(),
            TrackCommand {
                left_throttle: 1.0,
                right_throttle: 1.0,
            },
            1.0,
        )
        .expect("step");
        assert!((stepped.left_speed - FRICTION_ACCEL_M_S2).abs() < 1e-5);
        assert!(stepped.left_speed < MAX_TRACK_SPEED_M_S);
    }

    /// ADR-7431 (5): `step_for` loops `steps` times, so an unbounded count is a stall (2^32
    /// steps on a game thread). Past `MAX_TRACK_STEPS` it answers `track_bad_input` at once
    /// and does not step. Before ADR-7431 `u32::MAX` ran every step and returned `Ok`.
    #[test]
    fn an_unbounded_step_count_is_track_bad_input() {
        let command = TrackCommand {
            left_throttle: 1.0,
            right_throttle: 0.5,
        };
        let error = step_for(&TrackState::default(), command, DT, u32::MAX).expect_err("u32::MAX");
        assert_eq!(error, TrackError::BadInput);
        assert_eq!(error.code(), "track_bad_input");
    }

    /// The step as it was before ADR-8551 (std `min`/`clamp`, the heading from a quaternion
    /// rotation): the reference the lockstep delegate is compared with.
    fn legacy_advance(state: TrackState, command: TrackCommand, dt: f32) -> TrackState {
        fn speed_after(speed: f32, throttle: f32, dt: f32) -> f32 {
            let cap = FRICTION_ACCEL_M_S2 * dt;
            if throttle == 0.0 {
                let delta = (COAST_DRAG_M_S2 * dt).min(cap);
                if speed.abs() <= delta {
                    0.0
                } else if speed > 0.0 {
                    speed - delta
                } else {
                    speed + delta
                }
            } else {
                let target = throttle.clamp(-1.0, 1.0) * MAX_TRACK_SPEED_M_S;
                speed + (target - speed).clamp(-cap, cap)
            }
        }
        let left_after = speed_after(state.left_speed, command.left_throttle, dt);
        let right_after = speed_after(state.right_speed, command.right_throttle, dt);
        let left_avg = 0.5 * (state.left_speed + left_after);
        let right_avg = 0.5 * (state.right_speed + right_after);
        let center = 0.5 * (left_avg + right_avg);
        let yaw_after = state.yaw + (right_avg - left_avg) / TRACK_SEPARATION_M * dt;
        let facing = Quat::from_rotation_y(0.5 * (state.yaw + yaw_after)) * Vec3::NEG_Z;
        let travel = center * dt;
        TrackState {
            left_speed: left_after,
            right_speed: right_after,
            yaw: yaw_after,
            x: state.x + facing.x * travel,
            z: state.z + facing.z * travel,
            steps: state.steps.saturating_add(1),
        }
    }

    /// Wrap an angle to (-PI, PI] by whole turns, as the lockstep step does (ADR-10062).
    fn wrap_turns(mut a: f32) -> f32 {
        while a > core::f32::consts::PI {
            a -= core::f32::consts::TAU;
        }
        while a <= -core::f32::consts::PI {
            a += core::f32::consts::TAU;
        }
        a
    }

    /// ADR-8551: the delegate (the lockstep tracked step with the default parameters) against the
    /// pre-ADR-8551 step over 10 000 seeded steps. Speeds and yaw are the same arithmetic in the same
    /// order: equal (`==`; only a zero's sign may differ, from `det_clamp`'s `+0`). X and Z take
    /// the heading's sine and cosine from `regia_detsim::math` instead of a quaternion rotation, so
    /// they agree to rounding (a few ulp per step), not bit for bit.
    #[test]
    fn tracked_defaults_match_vehicle_track() {
        assert_eq!(DET_PARAMS, TrackedParams::DEFAULT);
        let mut rng = regia_detsim::golden::scenarios::SplitMix64::new(0x7EAC_0551);
        let mut det = TrackState::default();
        let mut legacy = TrackState::default();
        let mut command = TrackCommand::default();
        let (mut zero_sign, mut worst) = (0_u32, 0.0_f32);
        for i in 0..10_000_u32 {
            if i.is_multiple_of(50) {
                let pick = |v: f32| if v.abs() < 0.2 { 0.0 } else { v * 1.3 };
                command = TrackCommand {
                    left_throttle: pick(rng.signed()),
                    right_throttle: pick(rng.signed()),
                };
            }
            det = step(&det, command, DT).expect("finite");
            legacy = legacy_advance(legacy, command, DT);
            // ADR-10062: the lockstep step publishes its yaw wrapped to (-PI, PI] by whole turns
            // (the same loop as the hover step's `wrap_pi`); the reference wraps its result the same
            // way, so the yaw stays an exact comparison.
            legacy.yaw = wrap_turns(legacy.yaw);
            assert!(
                det.left_speed == legacy.left_speed
                    && det.right_speed == legacy.right_speed
                    && det.yaw == legacy.yaw,
                "step {i}: {det:?} vs {legacy:?}"
            );
            for (a, b) in [
                (det.left_speed, legacy.left_speed),
                (det.right_speed, legacy.right_speed),
                (det.yaw, legacy.yaw),
            ] {
                zero_sign += u32::from(a.to_bits() != b.to_bits());
            }
            worst = worst.max((det.x - legacy.x).abs()).max((det.z - legacy.z).abs());
            // Re-sync the pose so the comparison measures one step's rounding, not a drift.
            assert!(worst < 1e-3, "step {i}: position differs by {worst} m");
            legacy.x = det.x;
            legacy.z = det.z;
            legacy.left_speed = det.left_speed;
            legacy.right_speed = det.right_speed;
            legacy.yaw = det.yaw;
        }
        println!(
            "tracked_defaults_match_vehicle_track: zero-sign words {zero_sign}, worst one-step position delta {worst} m"
        );
        assert_eq!(det.steps, 10_000);
    }

    /// ADR-7431 (5): the limit itself runs; one past it is refused.
    #[test]
    fn the_track_step_limit_is_inclusive() {
        let command = TrackCommand {
            left_throttle: 1.0,
            right_throttle: 1.0,
        };
        let state = step_for(&TrackState::default(), command, DT, MAX_TRACK_STEPS).expect("at the limit");
        assert_eq!(state.steps, u64::from(MAX_TRACK_STEPS));
        assert_eq!(
            step_for(&TrackState::default(), command, DT, MAX_TRACK_STEPS + 1),
            Err(TrackError::BadInput)
        );
    }
    /// ADR-8638 (review finding B): `forward` is the nose the step integrated, bit for bit. Before
    /// the fix it was a quaternion rotation (platform trig) and after 256 steps of a left-only
    /// throttle its X was one ulp off `-regia_detsim::math::sin(yaw)` on x86-64.
    #[test]
    fn forward_is_the_heading_the_step_integrated() {
        let command = TrackCommand {
            left_throttle: 1.0,
            right_throttle: 0.0,
        };
        let mut state = TrackState::default();
        for step_index in 0..1024 {
            state = match step(&state, command, DT) {
                Ok(next) => next,
                Err(error) => panic!("step {step_index} failed with {}", error.code()),
            };
            let nose = state.forward();
            assert_eq!(
                [nose.x.to_bits(), nose.y.to_bits(), nose.z.to_bits()],
                [
                    (-regia_detsim::math::sin(state.yaw)).to_bits(),
                    0.0_f32.to_bits(),
                    (-regia_detsim::math::cos(state.yaw)).to_bits()
                ],
                "forward is not the lockstep nose after {} steps (yaw {})",
                step_index + 1,
                state.yaw
            );
        }
    }

    /// ADR-8638: the lockstep step now rests a vehicle whose step overflows (`recovered`) instead
    /// of publishing a non-finite pose; this API still answers `track_bad_input` for it, as it did
    /// when the pose came back non-finite.
    #[test]
    fn an_overflowing_step_is_still_track_bad_input() {
        let huge = TrackState {
            left_speed: f32::MAX,
            right_speed: -f32::MAX,
            ..TrackState::default()
        };
        let command = TrackCommand {
            left_throttle: 1.0,
            right_throttle: -1.0,
        };
        assert_eq!(step(&huge, command, DT), Err(TrackError::BadInput));
        let both = TrackState {
            left_speed: f32::MAX,
            right_speed: f32::MAX,
            ..TrackState::default()
        };
        assert_eq!(
            step(&both, TrackCommand::default(), DT),
            Err(TrackError::BadInput)
        );
    }
}
