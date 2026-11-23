// SPDX-License-Identifier: MPL-2.0
//! The kinematic single-track (bicycle) model (ADR-8551).
//!
//! The two wheels of each axle are lumped into one at the axle's center; the rear wheel rolls
//! without lateral slip and the front wheel is steered by `steer` radians. For a wheelbase `L`
//! and speed `v` the yaw rate is
//!
//! ```text
//! yaw_rate = -v * tan(steer) / L      (turn radius L / tan(steer))
//! ```
//!
//! (public literature: the kinematic bicycle model, e.g. Rajamani, *Vehicle Dynamics and Control*,
//! ch. 2). Positive `steer` turns right (negative yaw, the nose swings toward +X), so a positive
//! steer input is a right turn, as a stick pushed right.
//!
//! **Grip.** The kinematic model has no tires; [`WheeledParams::lateral_grip_m_s2`] stands in for
//! them: the yaw rate is capped so that the lateral acceleration `|v * yaw_rate|` stays at or under
//! the grip, and the state's `slip` is the fraction of the requested yaw rate the cap removed (0
//! with full grip, toward 1 as the car plows straight on). The handbrake doubles the cap (the rear
//! steps out and the nose comes round) while it brakes.
//!
//! **Steering.** The steer angle moves toward `steer_input * max_steer_rad` by at most
//! `steer_rate_rad_s * dt` per step.
//!
//! **Speed.** In order of precedence:
//!
//! 1. handbrake: the speed moves toward 0 by `brake_m_s2 * dt`;
//! 2. brake (0..=1): toward 0 by `brake * brake_m_s2 * dt`;
//! 3. throttle (−1..=1, nonzero): a positive throttle asks for `throttle * max_speed_m_s`, a negative
//!    one for `throttle * reverse_speed_m_s`; at or past the grade limit toward the climb it asks
//!    for 0 (`grade_limited`). The speed moves toward the request by `accel_m_s2 * dt` when it
//!    speeds up, by `brake_m_s2 * dt` when the request is the other direction or the grade refused
//!    it, and by `coast_m_s2 * dt` when the throttle only eased off;
//! 4. coast: the gravity pull `-gravity * sin(pitch) * dt` (held within ±`max_speed_m_s`), then toward
//!    0 by `coast_m_s2 * dt`.
//!
//! **The pose.** The yaw rate uses the mid-step speed; the position moves along the heading at
//! mid-step by `v_mid * dt * cos(pitch)` horizontally; Y is the floor under the new position.

use crate::float::{canonical, det_clamp};
use crate::hover::host::FloorSampler;
use crate::math;

use super::{
    DEG_TO_RAD, FieldRange, GroundParamsError, GroundReport, MIN_LENGTH_M, WorldPhysics, attitude,
    finite_or_zero, floor_or, nose, step_gravity, toward_zero, uphill,
};

/// The tuning numbers of a wheeled vehicle. Every peer of a match must step the same ones.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WheeledParams {
    /// Distance between the axles, meters. Valid: 0.01 ([`MIN_LENGTH_M`]) to 20.
    pub wheelbase_m: f32,
    /// Largest front steer angle, radians. Valid: above 0, at most 1.2 (69°).
    pub max_steer_rad: f32,
    /// How fast the steer angle moves, rad/s. Valid: above 0, at most 100.
    pub steer_rate_rad_s: f32,
    /// Top forward speed, m/s. Valid: above 0, at most 200.
    pub max_speed_m_s: f32,
    /// Top reverse speed, m/s. Valid: above 0, at most 100.
    pub reverse_speed_m_s: f32,
    /// Acceleration toward a faster request, m/s². Valid: above 0, at most 1000.
    pub accel_m_s2: f32,
    /// Braking deceleration (brake, handbrake, direction change, grade refusal), m/s². Valid: above
    /// 0, at most 1000.
    pub brake_m_s2: f32,
    /// Coast deceleration (no throttle, or easing off), m/s². Valid: 0 to 1000.
    pub coast_m_s2: f32,
    /// The lateral acceleration the tires hold, m/s²: the yaw rate is capped so that
    /// `|v * yaw_rate|` stays at or under it. Valid: above 0, at most 1000.
    pub lateral_grip_m_s2: f32,
    /// The steepest grade the vehicle climbs, degrees. Valid: above 0, at most 89.
    pub grade_limit_deg: f32,
    /// Distance between the left and right wheels, meters (the roll sample's width). Valid: 0.01
    /// ([`MIN_LENGTH_M`]) to 20.
    pub track_width_m: f32,
}

impl WheeledParams {
    /// A generic road car: 2.7 m wheelbase, 0.6 rad of lock at 2.5 rad/s, 40 m/s forward and 8 m/s
    /// reverse, 6 m/s² acceleration, 12 m/s² brakes, 1.5 m/s² coast, 8 m/s² of grip, a 30° grade
    /// limit and a 1.6 m track.
    pub const DEFAULT: WheeledParams = WheeledParams {
        wheelbase_m: 2.7,
        max_steer_rad: 0.6,
        steer_rate_rad_s: 2.5,
        max_speed_m_s: 40.0,
        reverse_speed_m_s: 8.0,
        accel_m_s2: 6.0,
        brake_m_s2: 12.0,
        coast_m_s2: 1.5,
        lateral_grip_m_s2: 8.0,
        grade_limit_deg: 30.0,
        track_width_m: 1.6,
    };

    /// The fields and their valid ranges, in field order.
    pub const RANGES: [FieldRange; 11] = [
        FieldRange::closed("wheelbase_m", MIN_LENGTH_M, 20.0),
        FieldRange::open("max_steer_rad", 0.0, 1.2),
        FieldRange::open("steer_rate_rad_s", 0.0, 100.0),
        FieldRange::open("max_speed_m_s", 0.0, 200.0),
        FieldRange::open("reverse_speed_m_s", 0.0, 100.0),
        FieldRange::open("accel_m_s2", 0.0, 1000.0),
        FieldRange::open("brake_m_s2", 0.0, 1000.0),
        FieldRange::closed("coast_m_s2", 0.0, 1000.0),
        FieldRange::open("lateral_grip_m_s2", 0.0, 1000.0),
        FieldRange::open("grade_limit_deg", 0.0, 89.0),
        FieldRange::closed("track_width_m", MIN_LENGTH_M, 20.0),
    ];

    fn numbers(&self) -> [f32; 11] {
        [
            self.wheelbase_m,
            self.max_steer_rad,
            self.steer_rate_rad_s,
            self.max_speed_m_s,
            self.reverse_speed_m_s,
            self.accel_m_s2,
            self.brake_m_s2,
            self.coast_m_s2,
            self.lateral_grip_m_s2,
            self.grade_limit_deg,
            self.track_width_m,
        ]
    }

    /// `Ok` when every field is finite and in its range ([`WheeledParams::RANGES`]); otherwise the
    /// first bad field. Call it when parameters are loaded, never per tick.
    pub fn validate(&self) -> Result<(), GroundParamsError> {
        for (range, value) in Self::RANGES.iter().zip(self.numbers()) {
            range.check(value)?;
        }
        Ok(())
    }
}

impl Default for WheeledParams {
    fn default() -> Self {
        WheeledParams::DEFAULT
    }
}

/// The driver's input for one step.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WheeledInput {
    /// −1..=1: forward (positive) or reverse (negative) request; 0 coasts. NaN reads as 0.
    pub throttle: f32,
    /// 0..=1 brake pedal; above 0 it overrides the throttle. NaN reads as 0.
    pub brake: f32,
    /// −1..=1 steer: positive turns right. NaN reads as 0.
    pub steer: f32,
    /// Handbrake: brakes at full `brake_m_s2` and doubles the grip cap.
    pub handbrake: bool,
}

/// Pose, speed, steer and slip of one wheeled vehicle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WheeledState {
    /// World position `[x, y, z]` of the reference point (the rear axle's center), meters. Y is
    /// the floor under XZ after a step.
    pub pos: [f32; 3],
    /// Heading about +Y, radians, wrapped to (-PI, PI] after every step (ADR-10062). The nose is
    /// `(-sin(yaw), -cos(yaw))`.
    pub yaw: f32,
    /// Speed along the nose, m/s (negative in reverse).
    pub speed: f32,
    /// Front steer angle, radians (positive: right).
    pub steer: f32,
    /// The fraction of the requested yaw rate the grip cap removed last step, 0..1.
    pub slip: f32,
    /// Ground pitch under the vehicle at the start of the last step, radians.
    pub pitch: f32,
    /// Ground roll under the vehicle at the start of the last step, radians.
    pub roll: f32,
}

impl WheeledState {
    /// A vehicle at rest at `pos` facing `yaw`.
    pub fn at_rest(pos: [f32; 3], yaw: f32) -> Self {
        WheeledState {
            pos,
            yaw,
            ..WheeledState::default()
        }
    }

    /// Every `f32` word in a fixed order (the agent's digest and the golden read it).
    pub fn words(&self) -> [f32; 9] {
        [
            self.pos[0],
            self.pos[1],
            self.pos[2],
            self.yaw,
            self.speed,
            self.steer,
            self.slip,
            self.pitch,
            self.roll,
        ]
    }

    fn is_finite(&self) -> bool {
        self.words().iter().all(|w| w.is_finite())
    }
}

/// The kinematic turn radius `wheelbase / tan(steer)` in meters (infinite for a zero steer).
pub fn turn_radius(wheelbase_m: f32, steer_rad: f32) -> f32 {
    wheelbase_m * math::cos(steer_rad) / math::sin(steer_rad).abs()
}

/// One step of `dt` seconds on the floor `floor` (see the module docs). Never panics.
pub fn step<S: FloorSampler + ?Sized>(
    params: &WheeledParams,
    world: &WorldPhysics,
    floor: &S,
    state: &mut WheeledState,
    input: &WheeledInput,
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
    let throttle = det_clamp(finite_or_zero(input.throttle), -1.0, 1.0);
    let brake = det_clamp(finite_or_zero(input.brake), 0.0, 1.0);
    let steer_in = det_clamp(finite_or_zero(input.steer), -1.0, 1.0);

    // Steering.
    let steer_target = steer_in * params.max_steer_rad;
    let steer_step = params.steer_rate_rad_s * dt;
    let steer = state.steer + det_clamp(steer_target - state.steer, -steer_step, steer_step);

    // Attitude.
    let (pitch, roll) = attitude(
        floor,
        state.pos,
        state.yaw,
        0.5 * params.wheelbase_m,
        0.5 * params.track_width_m,
    );

    // Speed.
    let v0 = state.speed;
    let dir: i8 = if v0 > 0.0 {
        1
    } else if v0 < 0.0 {
        -1
    } else if throttle > 0.0 && !input.handbrake && brake == 0.0 {
        1
    } else if throttle < 0.0 && !input.handbrake && brake == 0.0 {
        -1
    } else {
        0
    };
    let grade = uphill(pitch, dir);
    let limit = params.grade_limit_deg * DEG_TO_RAD;
    let mut grade_limited = false;
    let v1 = if input.handbrake {
        toward_zero(v0, params.brake_m_s2 * dt)
    } else if brake > 0.0 {
        toward_zero(v0, brake * params.brake_m_s2 * dt)
    } else if throttle != 0.0 {
        let mut target = if throttle > 0.0 {
            throttle * params.max_speed_m_s
        } else {
            throttle * params.reverse_speed_m_s
        };
        // The climb is toward the request's direction when the vehicle is not already moving.
        let climb = if v0 == 0.0 || (target > 0.0) == (v0 > 0.0) {
            grade
        } else {
            0.0
        };
        if climb > 0.0 && climb >= limit {
            target = 0.0;
            grade_limited = true;
        }
        let reversing = v0 != 0.0 && target != 0.0 && (target > 0.0) != (v0 > 0.0);
        let rate = if grade_limited || reversing {
            params.brake_m_s2
        } else if target.abs() >= v0.abs() {
            params.accel_m_s2
        } else {
            params.coast_m_s2
        };
        let max_change = rate * dt;
        v0 + det_clamp(target - v0, -max_change, max_change)
    } else {
        let rolled = if pitch != 0.0 {
            let max = params.max_speed_m_s;
            det_clamp(v0 - step_gravity(world) * math::sin(pitch) * dt, -max, max)
        } else {
            v0
        };
        toward_zero(rolled, params.coast_m_s2 * dt)
    };

    // Yaw: the kinematic bicycle rate at the mid-step speed, capped by grip.
    let v_mid = 0.5 * (v0 + v1);
    let tan_steer = math::sin(steer) / math::cos(steer);
    let requested = canonical(-(v_mid * tan_steer) / params.wheelbase_m);
    let grip = if input.handbrake {
        2.0 * params.lateral_grip_m_s2
    } else {
        params.lateral_grip_m_s2
    };
    let lateral = (v_mid * requested).abs();
    let (yaw_rate, slip) = if lateral > grip {
        let keep = grip / lateral;
        (requested * keep, 1.0 - keep)
    } else {
        (requested, 0.0)
    };
    let yaw_after = state.yaw + yaw_rate * dt;
    let yaw_mid = 0.5 * (state.yaw + yaw_after);
    let (fx, fz) = nose(yaw_mid);
    let mut travel = v_mid * dt;
    if pitch != 0.0 {
        travel *= math::cos(pitch);
    }
    let x = state.pos[0] + fx * travel;
    let z = state.pos[2] + fz * travel;
    let next = WheeledState {
        pos: [x, floor_or(floor, x, z, state.pos[1]), z],
        // Wrapped to (-PI, PI] (ADR-10062); the mid-step nose above used the unwrapped sum.
        yaw: crate::hover::wrap_pi(yaw_after),
        speed: v1,
        steer,
        slip,
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

/// Reset a poisoned state to rest in place (finite X, Z and yaw kept, others 0; Y the floor).
/// After an overflowing step `state` is the finite state before the step, so the vehicle rests at
/// that pose.
fn recover<S: FloorSampler + ?Sized>(state: &mut WheeledState, floor: &S) {
    let x = finite_or_zero(state.pos[0]);
    let z = finite_or_zero(state.pos[2]);
    let y = floor_or(floor, x, z, finite_or_zero(state.pos[1]));
    *state = WheeledState::at_rest([x, y, z], finite_or_zero(state.yaw));
}
