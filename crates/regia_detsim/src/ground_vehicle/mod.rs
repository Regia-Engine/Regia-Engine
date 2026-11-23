// SPDX-License-Identifier: MPL-2.0
//! Lockstep ground vehicles (ADR-8551, plan row mp-10): kinematic models only.
//!
//! Two models, each a pure `step(params, world, floor, state, input, dt) -> GroundReport`:
//!
//! - [`tracked`]: differential (skid-steer) drive. Two track speeds; the center speed is their
//!   average and the yaw rate their difference over the track separation. Each track chases its
//!   throttle's speed under a friction cap and coasts toward zero at zero throttle. This is the
//!   step `regia_physics::vehicle_track` shipped (ADR-4629), re-expressed in lockstep math, with
//!   its constants as [`tracked::TrackedParams`] fields; `regia_physics::vehicle_track` now delegates
//!   here.
//! - [`wheeled`]: the kinematic single-track (bicycle) model. A steer angle `delta` at the front
//!   axle and a wheelbase `L` give the yaw rate `v * tan(delta) / L` (turn radius `L / tan(delta)`),
//!   with a lateral grip cap: the yaw rate is limited so that `|v * yaw_rate| <= lateral_grip`.
//!
//! The full tire, drivetrain and suspension vehicles stay on the rigid-body side (rapier,
//! `regia_physics::vehicle*`): they are physical and per architecture, not lockstep code.
//!
//! # Conventions
//!
//! Y is up. Yaw is radians about +Y, right-handed, published wrapped to (-PI, PI] (ADR-10062). The nose at yaw 0 points
//! along −Z, so the nose is `(-sin(yaw), -cos(yaw))` in XZ and positive yaw turns the nose left
//! (toward −X): the convention of `regia_physics::vehicle_track`. A vehicle's right is
//! `(cos(yaw), -sin(yaw))`. Note that the hover step's nose is `(sin(yaw), cos(yaw))`: the two
//! families do not share a heading convention.
//!
//! # Slope
//!
//! Each step reads the ground's attitude under the vehicle with
//! [`crate::hover::host::ground_attitude`] (four floor samples on the vehicle's footprint) into
//! the state's `pitch` (positive when the ground rises ahead of the nose) and `roll` (positive when it
//! rises on the right). The uphill grade along the direction of travel then:
//!
//! - **limits the speed**: at or past the model's `grade_limit_deg` a drive request toward the
//!   climb asks for speed 0, so the vehicle stops on the slope (`grade_limited` in the report);
//!   the tracked model also loses `grade_speed_loss` of its speed linearly up to the limit;
//! - **pulls a coasting vehicle**: with zero throttle the speed changes by
//!   `-gravity * sin(pitch) * dt` before the coast drag, so a vehicle rolls back down a slope
//!   steeper than its drag can hold (gravity from the [`WorldPhysics`]).
//!
//! Horizontal travel is `speed * dt * cos(pitch)`, and the position's Y is the floor under the
//! new XZ. On flat ground (pitch exactly 0) these terms are exact no-ops, so the tracked model on
//! a flat floor is `vehicle_track`'s arithmetic step for step (the heading's sine and cosine now
//! come from [`crate::math`] rather than a quaternion rotation, so positions agree to a few ulp,
//! not bit for bit; see `tracked_defaults_match_vehicle_track`).
//!
//! # Position integration lives in the step
//!
//! The hover step leaves position to its host (DO27's call sites keep their shape). These models
//! integrate the position themselves: a kinematic vehicle's motion is defined by its pose (the turn
//! is an arc of the heading at mid-step), the slope terms need the floor at the new position, and a
//! host that integrated `speed * facing` on its own would re-derive the heading with its own trig.
//! One step owning the pose keeps every peer's pose on the same arithmetic.
//!
//! # Bad input
//!
//! The steps never panic and never validate their parameters (call `validate` when you load
//! them). A `dt` that is not finite and above 0 changes nothing. A NaN or infinite input axis reads
//! as 0. A NaN or infinite floor sample keeps the previous Y. A state with a NaN or infinite word
//! (written by a host, or stepped with invalid parameters) is reset to rest in place and the report
//! says `recovered`. So is a step whose result would hold a NaN or infinite word (an overflow, from a
//! host-written state near `f32::MAX` or parameters `validate` refuses): the vehicle comes to rest
//! at its pose before the step, on the same tick, so a step never publishes a non-finite word
//! (ADR-8638). Hardware NaN payloads differ between x86-64 and aarch64, so a published NaN would
//! split peers on raw bits even where a NaN-folding checksum stays equal.
//!
//! Every length parameter (`separation_m`, `half_length_m`, `wheelbase_m`, `track_width_m`) is at
//! least [`MIN_LENGTH_M`]: the yaw rate divides by the separation or the wheelbase, and a
//! subnormal length overflowed it (ADR-8638). With valid parameters, a `dt` of at most 1 s and a
//! start state whose words are within ±[`MAX_STATE_MAGNITUDE`], the next [`MAX_SIMULATE_STEPS`]
//! steps stay finite and never take the recovery.
//!
//! # Determinism
//!
//! Lockstep code (ADR-8012 A-4): IEEE `+ - * /` in source order, `sin`, `cos` and `atan2` through
//! [`crate::math`], sign-sensitive `min`/`max`/`clamp` through [`crate::float`], no allocation,
//! no threads. The `ground_vehicle_mix` golden scenario ([`GroundVehicleMix`]) pins both models on
//! four worlds.
//!
//! ```
//! use regia_detsim::ground_vehicle::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
//! use regia_detsim::ground_vehicle::WorldPhysics;
//! use regia_detsim::hover::host::FlatFloor;
//! use regia_detsim::tick::tick_dt;
//!
//! let params = WheeledParams::DEFAULT;
//! assert!(params.validate().is_ok());
//! let mut car = WheeledState::default();
//! let input = WheeledInput { throttle: 1.0, ..WheeledInput::default() };
//! for _ in 0..120 {
//!     let report = wheeled::step(&params, &WorldPhysics::EARTH, &FlatFloor(0.0), &mut car, &input, tick_dt());
//!     assert!(!report.recovered);
//! }
//! assert!(car.speed > 10.0 && car.pos[2] < -10.0, "two seconds of throttle along -Z");
//! ```

use core::fmt;

use crate::float::canonical;
use crate::hover::host::{FloorSampler, ground_attitude};
use crate::math;
use crate::site_physics::EARTH_G;

pub use crate::site_physics::WorldPhysics;

mod scenario;
pub mod tracked;
pub mod wheeled;

#[cfg(test)]
mod tests;

pub use scenario::{GroundField, GroundVehicleMix};

/// Most steps one agent `detsim.ground_vehicle_simulate` call runs (`steps_limit` past it): the
/// loop runs on the caller's thread. 10 000 steps is 166 s at 60 Hz.
pub const MAX_SIMULATE_STEPS: u32 = 10_000;

/// The smallest valid length parameter, meters (`separation_m`, `half_length_m`, `wheelbase_m`,
/// `track_width_m`): 1 cm. The yaw rate divides by the separation or the wheelbase; a length in
/// the old open range `(0, 20]` as small as `1e-40` overflowed it (ADR-8638).
pub const MIN_LENGTH_M: f32 = 0.01;

/// The largest state word magnitude the agent accepts (`detsim.ground_vehicle_simulate` refuses a
/// `state` word past it with `bad_params`): positions within ±1e9 m, speeds within ±1e9 m/s. With
/// valid parameters and `dt <= 1`, [`MAX_SIMULATE_STEPS`] steps from such a state stay finite
/// (ADR-8638).
pub const MAX_STATE_MAGNITUDE: f32 = 1.0e9;

/// Degrees to radians, as the steps convert `grade_limit_deg`.
pub(crate) const DEG_TO_RAD: f32 = core::f32::consts::PI / 180.0;

/// What one ground-vehicle step did besides stepping.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GroundReport {
    /// The state held a NaN or infinite word on entry, or the step's result would have (an
    /// overflow), and the vehicle was reset to rest in place (at its pose before the step) instead
    /// of stepped. Peers see the same reset on the same tick.
    pub recovered: bool,
    /// A drive request toward a climb at or past the grade limit was refused this step (the
    /// requested speed was 0).
    pub grade_limited: bool,
    /// The uphill grade along the direction of travel this step, radians (negative downhill, 0 when
    /// the vehicle neither moves nor drives).
    pub grade: f32,
}

/// Why a parameter value is invalid. [`GroundParamsErrorKind::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroundParamsErrorKind {
    /// NaN or infinite (`not_finite`).
    NotFinite,
    /// Outside the field's documented range (`out_of_range`).
    OutOfRange,
}

impl GroundParamsErrorKind {
    /// Every kind, in declaration order.
    pub const ALL: [GroundParamsErrorKind; 2] = [
        GroundParamsErrorKind::NotFinite,
        GroundParamsErrorKind::OutOfRange,
    ];

    /// The error code: lowercase ascii, documented in `book/agents/det-ground-vehicles.md`.
    pub const fn code(self) -> &'static str {
        match self {
            GroundParamsErrorKind::NotFinite => "not_finite",
            GroundParamsErrorKind::OutOfRange => "out_of_range",
        }
    }
}

/// The first invalid field of a [`tracked::TrackedParams`] or [`wheeled::WheeledParams`], in
/// field order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundParamsError {
    /// The field's name (`"wheelbase_m"`).
    pub field: &'static str,
    /// What is wrong with it.
    pub kind: GroundParamsErrorKind,
    /// The value it held.
    pub value: f32,
}

impl fmt::Display for GroundParamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: field `{}` is {}",
            self.kind.code(),
            self.field,
            self.value
        )
    }
}

impl std::error::Error for GroundParamsError {}

/// One numeric field and its valid range, as `validate` and the agent read them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldRange {
    /// The field's name.
    pub name: &'static str,
    /// Smallest valid value.
    pub min: f32,
    /// Largest valid value.
    pub max: f32,
    /// `min` itself is invalid (the range is `(min, max]`).
    pub min_exclusive: bool,
}

impl FieldRange {
    pub(crate) const fn open(name: &'static str, min: f32, max: f32) -> Self {
        FieldRange {
            name,
            min,
            max,
            min_exclusive: true,
        }
    }

    pub(crate) const fn closed(name: &'static str, min: f32, max: f32) -> Self {
        FieldRange {
            name,
            min,
            max,
            min_exclusive: false,
        }
    }

    /// `Ok` when `value` is finite and in range.
    pub fn check(&self, value: f32) -> Result<(), GroundParamsError> {
        let kind = if !value.is_finite() {
            Some(GroundParamsErrorKind::NotFinite)
        } else if value > self.max || value < self.min || (self.min_exclusive && value == self.min) {
            Some(GroundParamsErrorKind::OutOfRange)
        } else {
            None
        };
        match kind {
            Some(kind) => Err(GroundParamsError {
                field: self.name,
                kind,
                value,
            }),
            None => Ok(()),
        }
    }
}

/// A finite value, or 0 for NaN and the infinities (the input guard).
#[inline]
pub(crate) fn finite_or_zero(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// The gravity a step reads: the world's when it is finite and above 0, otherwise [`EARTH_G`].
#[inline]
pub(crate) fn step_gravity(world: &WorldPhysics) -> f32 {
    let g = world.gravity_m_s2;
    if g.is_finite() && g > 0.0 { g } else { EARTH_G }
}

/// The ground's `(pitch, roll)` in this module's convention (nose along −Z at yaw 0): the hover
/// pass's pitch is along `(sin yaw, cos yaw)`, the opposite of this nose, so its sign flips; its
/// right `(cos yaw, -sin yaw)` is this module's right, so the roll keeps its sign.
pub(crate) fn attitude<S: FloorSampler + ?Sized>(
    floor: &S,
    pos: [f32; 3],
    yaw: f32,
    half_length: f32,
    half_width: f32,
) -> (f32, f32) {
    let (pitch_hover, roll) = ground_attitude(floor, pos, yaw, half_length, half_width);
    (canonical(-pitch_hover), roll)
}

/// The nose direction `(x, z)` at heading `yaw`: `(-sin(yaw), -cos(yaw))`.
#[inline]
pub(crate) fn nose(yaw: f32) -> (f32, f32) {
    (-math::sin(yaw), -math::cos(yaw))
}

/// The floor under `(x, z)` when it is finite, otherwise `previous`.
#[inline]
pub(crate) fn floor_or<S: FloorSampler + ?Sized>(floor: &S, x: f32, z: f32, previous: f32) -> f32 {
    let y = floor.floor(x, z);
    if y.is_finite() { y } else { previous }
}

/// The uphill grade along a direction of travel `dir` (+1 forward, −1 backward, 0 none).
#[inline]
pub(crate) fn uphill(pitch: f32, dir: i8) -> f32 {
    match dir {
        1 => pitch,
        -1 => canonical(-pitch),
        _ => 0.0,
    }
}

/// `speed` moved toward 0 by at most `delta` (`delta >= 0`); a speed within `delta` of 0 stops.
#[inline]
pub(crate) fn toward_zero(speed: f32, delta: f32) -> f32 {
    if speed.abs() <= delta {
        0.0
    } else if speed > 0.0 {
        speed - delta
    } else {
        speed + delta
    }
}

/// FNV-1a 64 (Fowler–Noll–Vo, public domain) over `f32` bit patterns, little-endian, with NaN and
/// `-0` canonical: the digest the agent answers.
pub fn digest_words(words: &[f32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for w in words {
        for byte in canonical(*w).to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}
