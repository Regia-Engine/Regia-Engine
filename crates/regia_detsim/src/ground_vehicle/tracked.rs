// SPDX-License-Identifier: MPL-2.0
//! Differential (skid-steer) drive, kinematic (ADR-8551).
//!
//! Two tracks `separation_m` apart share a rigid body. Each track has a speed in meters per second,
//! positive forward (the nose, local −Z):
//!
//! ```text
//! center_speed = (left_speed + right_speed) / 2
//! yaw_rate     = (right_speed - left_speed) / separation_m
//! ```
//!
//! A faster left track gives a negative yaw rate: the nose swings toward +X, a right turn.
//!
//! **Track speeds.** A throttle is clamped to −1..=1. A nonzero throttle `t` asks for
//! `t * max_track_speed_m_s` (times the grade scale, below), and the speed moves toward it by at
//! most `friction_accel_m_s2 * dt`. A zero throttle (or −0) coasts: on a grade the speed first takes
//! the gravity pull `-gravity * sin(pitch) * dt` (held within ±`max_track_speed_m_s`), then moves
//! toward zero by `min(coast_drag_m_s2, friction_accel_m_s2) * dt` and stops at zero.
//!
//! **Pivot turns.** With `pivot_turn` the tracks may run in opposite directions and opposite
//! full throttles spin the body in place. Without it, a track whose throttle opposes the larger
//! one is braked (asked for speed 0 under the friction cap), so the body turns about that track
//! instead; equal and opposite throttles brake both.
//!
//! **Grade.** The uphill grade along the drive direction (the sign of the summed driven throttles,
//! or of the center speed when both tracks coast) scales every drive request by
//! `1 - grade_speed_loss * grade / grade_limit` below the limit, and by 0 at or past it, so the
//! body stops on a slope it cannot climb (`grade_limited`). A downhill grade does not scale.
//!
//! **The pose.** Each step updates the track speeds first, then moves the pose with the average of
//! the speeds at the start and the end of the step, along the heading at mid-step, by
//! `center * dt * cos(pitch)` horizontally; Y is the floor under the new position. From rest with
//! constant track accelerations, the yaw after `T` seconds is
//! `(accel_right - accel_left) * T² / (2 * separation_m)` ([`yaw_closed_form`]) until it passes PI: the
//! published yaw is wrapped to (-PI, PI] (ADR-10062).
//!
//! [`TrackedParams::DEFAULT`] is `regia_physics::vehicle_track`'s model (2 m separation, 8 m/s,
//! 4 m/s² friction, 1 m/s² coast drag, pivot turns): that module's `step` is this step on a flat
//! floor at height 0.

use crate::float::{det_clamp, det_min};
use crate::hover::host::FloorSampler;
use crate::math;

use super::{
    DEG_TO_RAD, FieldRange, GroundParamsError, GroundReport, MIN_LENGTH_M, WorldPhysics, attitude,
    finite_or_zero, floor_or, nose, step_gravity, toward_zero, uphill,
};

/// The tuning numbers of a tracked vehicle. Every peer of a match must step the same ones.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackedParams {
    /// Distance between the track centerlines, meters. Valid: 0.01 ([`MIN_LENGTH_M`]) to 20.
    pub separation_m: f32,
    /// Track speed at throttle ±1, meters per second. Valid: above 0, at most 100.
    pub max_track_speed_m_s: f32,
    /// The most a track's speed changes per second, m/s². Valid: above 0, at most 1000.
    pub friction_accel_m_s2: f32,
    /// Coast deceleration at zero throttle, m/s² (capped by the friction). Valid: 0 to 1000.
    pub coast_drag_m_s2: f32,
    /// The tracks may counter-rotate (spin in place). Without it an opposing track is braked.
    pub pivot_turn: bool,
    /// The steepest grade the vehicle climbs, degrees. Valid: above 0, at most 89.
    pub grade_limit_deg: f32,
    /// The fraction of the drive speed lost toward the grade limit (linear in the grade). Valid: 0 to 1.
    pub grade_speed_loss: f32,
    /// Half the footprint length the slope is sampled over, meters (the half width is half the
    /// separation). Valid: 0.01 ([`MIN_LENGTH_M`]) to 20.
    pub half_length_m: f32,
}

impl TrackedParams {
    /// `regia_physics::vehicle_track`'s model, plus a 35° grade limit, a 50 % speed loss toward it
    /// and a 2.5 m half length.
    pub const DEFAULT: TrackedParams = TrackedParams {
        separation_m: 2.0,
        max_track_speed_m_s: 8.0,
        friction_accel_m_s2: 4.0,
        coast_drag_m_s2: 1.0,
        pivot_turn: true,
        grade_limit_deg: 35.0,
        grade_speed_loss: 0.5,
        half_length_m: 2.5,
    };

    /// The numeric fields and their valid ranges, in field order.
    pub const RANGES: [FieldRange; 7] = [
        FieldRange::closed("separation_m", MIN_LENGTH_M, 20.0),
        FieldRange::open("max_track_speed_m_s", 0.0, 100.0),
        FieldRange::open("friction_accel_m_s2", 0.0, 1000.0),
        FieldRange::closed("coast_drag_m_s2", 0.0, 1000.0),
        FieldRange::open("grade_limit_deg", 0.0, 89.0),
        FieldRange::closed("grade_speed_loss", 0.0, 1.0),
        FieldRange::closed("half_length_m", MIN_LENGTH_M, 20.0),
    ];

    fn numbers(&self) -> [f32; 7] {
        [
            self.separation_m,
            self.max_track_speed_m_s,
            self.friction_accel_m_s2,
            self.coast_drag_m_s2,
            self.grade_limit_deg,
            self.grade_speed_loss,
            self.half_length_m,
        ]
    }

    /// `Ok` when every field is finite and in its range ([`TrackedParams::RANGES`]); otherwise the
    /// first bad field. Call it when parameters are loaded, never per tick.
    pub fn validate(&self) -> Result<(), GroundParamsError> {
        for (range, value) in Self::RANGES.iter().zip(self.numbers()) {
            range.check(value)?;
        }
        Ok(())
    }
}

impl Default for TrackedParams {
    fn default() -> Self {
        TrackedParams::DEFAULT
    }
}

/// Throttles for one step: +1 full forward, −1 full reverse, 0 coasts.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackedInput {
    /// Left track throttle (clamped to −1..=1; NaN or infinite reads as 0).
    pub left_throttle: f32,
    /// Right track throttle (clamped to −1..=1; NaN or infinite reads as 0).
    pub right_throttle: f32,
}

/// Pose, attitude and track speeds of one tracked vehicle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackedState {
    /// World position `[x, y, z]`, meters. Y is the floor under XZ after a step.
    pub pos: [f32; 3],
    /// Heading about +Y, radians, wrapped to (-PI, PI] after every step (ADR-10062). The nose is
    /// `(-sin(yaw), -cos(yaw))`.
    pub yaw: f32,
    /// Left track speed, m/s, positive forward.
    pub left_speed: f32,
    /// Right track speed, m/s, positive forward.
    pub right_speed: f32,
    /// Ground pitch under the vehicle at the start of the last step, radians (positive: rising
    /// ahead of the nose).
    pub pitch: f32,
    /// Ground roll under the vehicle at the start of the last step, radians (positive: rising on
    /// the right).
    pub roll: f32,
}

impl TrackedState {
    /// A vehicle at rest at `pos` facing `yaw`.
    pub fn at_rest(pos: [f32; 3], yaw: f32) -> Self {
        TrackedState {
            pos,
            yaw,
            ..TrackedState::default()
        }
    }

    /// Center (longitudinal) speed: the average of the two track speeds, m/s.
    pub fn center_speed(&self) -> f32 {
        0.5 * (self.left_speed + self.right_speed)
    }

    /// Yaw rate about +Y for a track separation, rad/s.
    pub fn yaw_rate(&self, separation_m: f32) -> f32 {
        (self.right_speed - self.left_speed) / separation_m
    }

    /// Every `f32` word in a fixed order (the agent's digest and the golden read it).
    pub fn words(&self) -> [f32; 8] {
        [
            self.pos[0],
            self.pos[1],
            self.pos[2],
            self.yaw,
            self.left_speed,
            self.right_speed,
            self.pitch,
            self.roll,
        ]
    }

    fn is_finite(&self) -> bool {
        self.words().iter().all(|w| w.is_finite())
    }
}

/// Yaw in radians after `time_s` seconds of constant track accelerations from rest:
/// `(accel_right - accel_left) * time_s² / (2 * separation_m)`.
pub fn yaw_closed_form(separation_m: f32, accel_left_m_s2: f32, accel_right_m_s2: f32, time_s: f32) -> f32 {
    (accel_right_m_s2 - accel_left_m_s2) * time_s * time_s / (2.0 * separation_m)
}

/// What a track is asked to do this step.
#[derive(Clone, Copy)]
enum Track {
    /// Zero throttle: gravity pull, then the coast drag.
    Coast,
    /// Chase `throttle * max_track_speed_m_s * grade_scale` (a throttle of 0 here is a brake).
    Drive(f32),
}

/// One step of `dt` seconds on the floor `floor` (see the module docs). Never panics.
pub fn step<S: FloorSampler + ?Sized>(
    params: &TrackedParams,
    world: &WorldPhysics,
    floor: &S,
    state: &mut TrackedState,
    input: &TrackedInput,
    dt: f32,
) -> GroundReport {
    if !(dt.is_finite() && dt > 0.0) {
        return GroundReport::default();
    }
    if !state.is_finite() {
        recover(state, floor);
        return GroundReport {
            recovered: true,
            ..GroundReport::default()
        };
    }
    let (left, right) = commands(params, input);
    let (pitch, roll) = attitude(
        floor,
        state.pos,
        state.yaw,
        params.half_length_m,
        0.5 * params.separation_m,
    );

    // The drive direction: the summed driven throttles, or the center speed when both coast.
    let driven = drive_sum(left) + drive_sum(right);
    let dir: i8 = if driven > 0.0 {
        1
    } else if driven < 0.0 {
        -1
    } else {
        let c = state.center_speed();
        if c > 0.0 {
            1
        } else if c < 0.0 {
            -1
        } else {
            0
        }
    };
    let grade = uphill(pitch, dir);
    let limit = params.grade_limit_deg * DEG_TO_RAD;
    let grade_limited = driven != 0.0 && grade > 0.0 && grade >= limit;
    let scale = if grade > 0.0 {
        if grade >= limit {
            0.0
        } else {
            1.0 - params.grade_speed_loss * (grade / limit)
        }
    } else {
        1.0
    };
    let pull = if pitch != 0.0 {
        step_gravity(world) * math::sin(pitch)
    } else {
        0.0
    };

    let left_after = track_speed(params, state.left_speed, left, scale, pull, dt);
    let right_after = track_speed(params, state.right_speed, right, scale, pull, dt);
    let left_avg = 0.5 * (state.left_speed + left_after);
    let right_avg = 0.5 * (state.right_speed + right_after);
    let center = 0.5 * (left_avg + right_avg);
    let yaw_rate = (right_avg - left_avg) / params.separation_m;
    let yaw_after = state.yaw + yaw_rate * dt;
    // The heading at mid-step makes a constant yaw rate trace a circular arc's chord, and a
    // constant yaw acceleration land on `yaw_closed_form`.
    let yaw_mid = 0.5 * (state.yaw + yaw_after);
    let (fx, fz) = nose(yaw_mid);
    let mut travel = center * dt;
    if pitch != 0.0 {
        travel *= math::cos(pitch);
    }
    let x = state.pos[0] + fx * travel;
    let z = state.pos[2] + fz * travel;
    let next = TrackedState {
        pos: [x, floor_or(floor, x, z, state.pos[1]), z],
        // Wrapped to (-PI, PI] (ADR-10062); the mid-step nose above used the unwrapped sum.
        yaw: crate::hover::wrap_pi(yaw_after),
        left_speed: left_after,
        right_speed: right_after,
        pitch,
        roll,
    };
    if !next.is_finite() {
        // An overflow (a host-written state near `f32::MAX`, or a length `validate` refuses):
        // rest at the pose before the step, on this tick, rather than publish a hardware NaN or
        // infinity whose bits differ between architectures (ADR-8638).
        recover(state, floor);
        return GroundReport {
            recovered: true,
            ..GroundReport::default()
        };
    }
    *state = next;
    GroundReport {
        recovered: false,
        grade_limited,
        grade,
    }
}

/// The two track commands after the clamp and the pivot rule.
fn commands(params: &TrackedParams, input: &TrackedInput) -> (Track, Track) {
    let lt = finite_or_zero(input.left_throttle);
    let rt = finite_or_zero(input.right_throttle);
    let track = |t: f32| {
        if t == 0.0 {
            Track::Coast
        } else {
            Track::Drive(det_clamp(t, -1.0, 1.0))
        }
    };
    let (left, right) = (track(lt), track(rt));
    if params.pivot_turn {
        return (left, right);
    }
    match (left, right) {
        (Track::Drive(l), Track::Drive(r)) if (l > 0.0 && r < 0.0) || (l < 0.0 && r > 0.0) => {
            let sum = l + r;
            if sum > 0.0 {
                if l < 0.0 {
                    (Track::Drive(0.0), right)
                } else {
                    (left, Track::Drive(0.0))
                }
            } else if sum < 0.0 {
                if l > 0.0 {
                    (Track::Drive(0.0), right)
                } else {
                    (left, Track::Drive(0.0))
                }
            } else {
                (Track::Drive(0.0), Track::Drive(0.0))
            }
        }
        other => other,
    }
}

fn drive_sum(track: Track) -> f32 {
    match track {
        Track::Coast => 0.0,
        Track::Drive(t) => t,
    }
}

/// The speed after one step: `vehicle_track`'s `track_speed`, with the grade scale and pull.
fn track_speed(params: &TrackedParams, speed: f32, track: Track, scale: f32, pull: f32, dt: f32) -> f32 {
    let cap = params.friction_accel_m_s2 * dt;
    match track {
        Track::Coast => {
            let speed = if pull != 0.0 {
                let max = params.max_track_speed_m_s;
                det_clamp(speed - pull * dt, -max, max)
            } else {
                speed
            };
            let delta = det_min(params.coast_drag_m_s2 * dt, cap);
            toward_zero(speed, delta)
        }
        Track::Drive(throttle) => {
            let mut target = throttle * params.max_track_speed_m_s;
            if scale != 1.0 {
                target *= scale;
            }
            let delta = det_clamp(target - speed, -cap, cap);
            speed + delta
        }
    }
}

/// Reset a poisoned state to rest in place (finite X, Z and yaw kept, others 0; Y the floor).
/// After an overflowing step `state` is the finite state before the step, so the vehicle rests at
/// that pose.
fn recover<S: FloorSampler + ?Sized>(state: &mut TrackedState, floor: &S) {
    let x = finite_or_zero(state.pos[0]);
    let z = finite_or_zero(state.pos[2]);
    let y = floor_or(floor, x, z, finite_or_zero(state.pos[1]));
    *state = TrackedState::at_rest([x, y, z], finite_or_zero(state.yaw));
}
