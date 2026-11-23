// SPDX-License-Identifier: MPL-2.0
//! Vehicles in the predicted schedule (ADR-3080): the `net` feature.
//!
//! The server simulates every vehicle in the shared physics world. The owning client predicts its
//! own in a [`VehicleSimWorld`] of its own, which can go back to any tick; other clients interpolate
//! theirs like any proxy. ADR-3080 in `docs/decisions.md` records the measurement that chose this.
//!
//! | Piece | What |
//! |---|---|
//! | [`VehicleNetInput`] / [`VehicleCommand`] | the input command and how any command maps to it |
//! | [`NetVehicle`] | the marker: names the shared tuning and which local player drives |
//! | [`NetVehicleState`] | the state a `Transform` cannot carry, predicted and replicated, quantized on the wire |
//! | [`NetVehicleVisual`] / [`NetVehicleSmoothing`] | what is drawn: the simulated pose plus a decaying, rate-limited offset, with a snap threshold |
//! | [`VehicleSeats`] / [`NetSeated`] / [`VehicleSeatRequest`] | seats, on the server: who sits where, who owns the car |
//! | [`net_vehicle_rows`] | what `net.vehicles` reports |
//!
//! # The order of one tick
//!
//! On the server: [`net_vehicle_server_apply`] (seat requests, then the owner's command becomes the
//! car's [`VehicleInput`]), `drive_vehicles` and `step_physics`, then the predicted schedule, which
//! copies the result into [`NetVehicleState`]. A client predicts the same tick in the predicted
//! schedule itself. Both results are the state after the command of that tick, so they compare.
//!
//! # Corrections
//!
//! A rollback writes the server's `Transform` and [`NetVehicleState`]. The step sees values it did
//! not leave and puts the private world on them before it goes on, so a re-run needs no help from
//! `regia_net`. What is drawn does not jump: [`NetVehicleVisual`] takes the difference and pays it
//! back over the next steps, never more than [`NetVehicleSmoothing::max_step_m`] in one step.
//! A difference over [`NetVehicleSmoothing::snap_m`] snaps.

use std::any::TypeId;
use std::collections::HashMap;

use regia_app::FixedTime;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{Angle, CodecContext, LayoutHasher, ReplicateCodec};
use regia_net::message::{ValidateContext, Verdict, register_net_message};
use regia_net::predict::{
    EntityTolerance, InputCommand, NetInput, NetVisualOffset, PredictError, PredictedUpdate, PredictedValues,
    PredictionRegistry, PredictionState, Tolerance, add_net_input, add_predicted_systems, predict,
    predict_entity_tolerance, predict_local_writes, tick_step_seconds_of,
};
use regia_net::replication::{
    NetPredicted, ReplicateOptions, Replicated, TransformReplication, register_replicated,
};
use regia_net::runtime::NetEntities;
use regia_net::{
    NetId, NetMessage, NetMessages, NetOwner, NetPlayer, NetPlayerId, NetRole, NetSet, PeerId, Replicate,
};
use regia_physics::layers::PhysicsLayers;
use regia_physics::vehicle::{
    MAX_WHEELS, VehicleConfig, VehicleController, VehicleError, VehicleInput, spawn_vehicle,
};
use regia_physics::vehicle_drivetrain::{DrivetrainCore, ShiftRequest, WheelCore};
use regia_physics::vehicle_snapshot::{VehicleCore, VehicleSimSettings, VehicleSimWorld, WheelCoreState};
use regia_physics::{PhysicsSettings, PhysicsStep, PhysicsWorld};
use regia_scene::Transform;
use thiserror::Error;

use crate::character::{CharacterState, MovementMode};
use crate::net_character::{
    CharacterCommand, CharacterNetInput, NetCharacter, NetCharacterBase, net_character_step,
};

/// Translation tolerance of the vehicle comparison, meters. A car covers 0.33 m per tick at 20 m/s,
/// and the wire's grid is 1 mm.
pub const TOLERANCE_POSITION_M: f32 = 0.02;
/// Rotation tolerance of the vehicle comparison, radians (the wire's rotation is good to 0.0025).
pub const TOLERANCE_ROTATION_RAD: f32 = 0.02;
/// Linear velocity tolerance, m/s.
pub const TOLERANCE_VELOCITY: f32 = 0.1;
/// Angular velocity tolerance, rad/s.
pub const TOLERANCE_ANGULAR: f32 = 0.1;
/// Engine and wheel speed tolerance, rad/s.
pub const TOLERANCE_SPIN: f32 = 2.0;
/// Shift and dwell timer tolerance, seconds (one drivetrain step).
pub const TOLERANCE_TIMER_S: f32 = 1.0 / 120.0;

/// Steps of a `throttle` or `steer` axis on each side of zero: zero is exact.
const AXIS_STEPS: f32 = 127.0;
/// Steps of `brake` above zero.
const BRAKE_STEPS: f32 = 255.0;

/// Refusal code of an input command with an axis the encoder cannot write (`-128`).
pub const CODE_INPUT_RANGE: &str = "vehicle_input_range";
/// Refusal code of an input command with the unused shift value.
pub const CODE_INPUT_SHIFT: &str = "vehicle_input_shift";
/// Refusal code of a [`NetVehicleState`] with more wheels than a vehicle can have, or two wheel
/// lists of different lengths.
pub const CODE_STATE_WHEELS: &str = "vehicle_state_wheels";
/// Refusal code of a [`NetVehicleState`] gear outside the wire's range.
pub const CODE_STATE_GEAR: &str = "vehicle_state_gear";

// ---- quantization -------------------------------------------------------------------------------

/// A signed quantizer: `value / lsb`, rounded, in `bits` two's-complement bits. The lowest
/// pattern is never written, so a reader may refuse it.
#[derive(Debug, Clone, Copy)]
struct Fixed {
    lsb: f32,
    bits: u32,
}

impl Fixed {
    const fn new(lsb: f32, bits: u32) -> Self {
        Self { lsb, bits }
    }

    fn limit(self) -> i64 {
        (1i64 << (self.bits - 1)) - 1
    }

    fn quantize(self, value: f32) -> i64 {
        if !value.is_finite() {
            return 0;
        }
        let limit = self.limit();
        ((value / self.lsb).round() as i64).clamp(-limit, limit)
    }

    fn dequantize(self, q: i64) -> f32 {
        q as f32 * self.lsb
    }

    fn write(self, value: f32, w: &mut BitWriter, cx: &CodecContext) {
        let q = self.quantize(value);
        if !value.is_finite() || (value / self.lsb).round().abs() > self.limit() as f32 {
            cx.count_clamped();
        }
        w.write_bits((q as u64) & ((1u64 << self.bits) - 1), self.bits);
    }

    fn read(self, r: &mut BitReader) -> Result<f32, DecodeError> {
        let raw = r.read_bits(self.bits)?;
        let shift = 64 - self.bits;
        let q = ((raw << shift) as i64) >> shift;
        Ok(self.dequantize(q))
    }

    /// The value as it reads back.
    #[cfg(test)]
    fn on_wire(self, value: f32) -> f32 {
        self.dequantize(self.quantize(value))
    }
}

const LINEAR: Fixed = Fixed::new(1.0 / 128.0, 16); // ±256 m/s to 8 mm/s
const ANGULAR: Fixed = Fixed::new(1.0 / 512.0, 15); // ±32 rad/s to 2 mrad/s
const SUSPENSION: Fixed = Fixed::new(1.0 / 64.0, 9); // ±4 m to 16 mm
const STEER: Fixed = Fixed::new(1.0 / AXIS_STEPS, 9);
const SPIN_ANGLE: Angle = Angle { bits: 10 };
const ENGINE_SPEED: Fixed = Fixed::new(1.0 / 16.0, 16); // ±2,048 rad/s (19,500 rpm)
const WHEEL_SPEED: Fixed = Fixed::new(1.0 / 32.0, 16); // ±1,024 rad/s
const TORQUE: Fixed = Fixed::new(0.5, 16); // ±16 kN·m
const TIMER: Fixed = Fixed::new(1.0 / 256.0, 12); // ±8 s
const FACTOR: Fixed = Fixed::new(1.0 / 255.0, 10); // a 0..=1 factor
const SLIP: Fixed = Fixed::new(1.0 / 64.0, 12); // ±32
const FORCE: Fixed = Fixed::new(4.0, 15); // ±65 kN
const GEAR_OFFSET: i32 = 1;
const GEAR_BITS: u32 = 6;

// ---- the input command --------------------------------------------------------------------------

/// A shift the driver asks for on one tick. A one-shot: a missing command does not repeat it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VehicleShift {
    /// None.
    #[default]
    None,
    /// One gear up.
    Up,
    /// One gear down.
    Down,
}

fn quantize_axis(v: f32) -> i8 {
    if v.is_finite() {
        (v.clamp(-1.0, 1.0) * AXIS_STEPS).round() as i8
    } else {
        0
    }
}

fn quantize_brake(v: f32) -> u8 {
    if v.is_finite() {
        (v.clamp(0.0, 1.0) * BRAKE_STEPS).round() as u8
    } else {
        0
    }
}

/// The default input command of a vehicle.
///
/// Quantized on the wire: 8 bits per axis, 2 for the shift, 26 bits in all. The step runs on
/// [`VehicleNetInput::quantized`] on both sides, so the client predicts with exactly what the
/// server receives. A non-finite axis reads as released.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VehicleNetInput {
    /// Engine: +1 full forward, -1 full reverse.
    pub throttle: f32,
    /// Brake: 0 off to 1 full.
    pub brake: f32,
    /// Steering: +1 full right, -1 full left.
    pub steer: f32,
    /// A gear change for this tick.
    pub shift: VehicleShift,
}

impl VehicleNetInput {
    /// The command as it travels. Idempotent.
    #[must_use]
    pub fn quantized(&self) -> Self {
        Self {
            throttle: f32::from(quantize_axis(self.throttle)) / AXIS_STEPS,
            brake: f32::from(quantize_brake(self.brake)) / BRAKE_STEPS,
            steer: f32::from(quantize_axis(self.steer)) / AXIS_STEPS,
            shift: self.shift,
        }
    }

    /// The vehicle's input for this command, as it travels.
    #[must_use]
    pub fn vehicle_input(&self) -> VehicleInput {
        let q = self.quantized();
        VehicleInput {
            throttle: q.throttle,
            brake: q.brake,
            steer: q.steer,
        }
    }

    /// A command from a vehicle input.
    #[must_use]
    pub fn from_vehicle_input(input: VehicleInput) -> Self {
        Self {
            throttle: input.throttle,
            brake: input.brake,
            steer: input.steer,
            shift: VehicleShift::None,
        }
    }

    fn write_axes(&self, w: &mut BitWriter) {
        w.write_bits(u64::from(quantize_axis(self.throttle) as u8), 8);
        w.write_bits(u64::from(quantize_brake(self.brake)), 8);
        w.write_bits(u64::from(quantize_axis(self.steer) as u8), 8);
    }

    fn read_axes(r: &mut BitReader) -> Result<(f32, f32, f32), DecodeError> {
        let axis = |raw: u64| -> Result<f32, DecodeError> {
            let q = raw as u8 as i8;
            if q == i8::MIN {
                return Err(DecodeError::Invalid(CODE_INPUT_RANGE));
            }
            Ok(f32::from(q) / AXIS_STEPS)
        };
        let throttle = axis(r.read_bits(8)?)?;
        let brake = r.read_bits(8)? as f32 / BRAKE_STEPS;
        let steer = axis(r.read_bits(8)?)?;
        Ok((throttle, brake, steer))
    }

    fn write_shift(&self, w: &mut BitWriter) {
        let code = match self.shift {
            VehicleShift::None => 0,
            VehicleShift::Up => 1,
            VehicleShift::Down => 2,
        };
        w.write_bits(code, 2);
    }

    fn read_shift(r: &mut BitReader) -> Result<VehicleShift, DecodeError> {
        match r.read_bits(2)? {
            0 => Ok(VehicleShift::None),
            1 => Ok(VehicleShift::Up),
            2 => Ok(VehicleShift::Down),
            _ => Err(DecodeError::Invalid(CODE_INPUT_SHIFT)),
        }
    }

    fn axes_key(&self) -> (i8, u8, i8) {
        (
            quantize_axis(self.throttle),
            quantize_brake(self.brake),
            quantize_axis(self.steer),
        )
    }
}

impl ReplicateCodec for VehicleNetInput {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        self.write_axes(w);
        self.write_shift(w);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let (throttle, brake, steer) = Self::read_axes(r)?;
        Ok(Self {
            throttle,
            brake,
            steer,
            shift: Self::read_shift(r)?,
        })
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, _cx: &CodecContext) {
        let moved = self.axes_key() != base.axes_key();
        w.write_bool(moved);
        if moved {
            self.write_axes(w);
        }
        self.write_shift(w);
    }

    fn decode_delta(base: &Self, r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let (throttle, brake, steer) = if r.read_bool()? {
            Self::read_axes(r)?
        } else {
            (base.throttle, base.brake, base.steer)
        };
        Ok(Self {
            throttle,
            brake,
            steer,
            shift: Self::read_shift(r)?,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        h.write_u64(8);
        h.write_u64(2);
    }
}

impl NetInput for VehicleNetInput {
    fn on_missing(last: &Self) -> Self {
        Self {
            shift: VehicleShift::None,
            ..*last
        }
    }
}

/// How a game's input command maps to a vehicle's. Games with richer input than
/// [`VehicleNetInput`] implement this on their own [`NetInput`] type and install it with
/// [`install_net_vehicle`]; [`DriverNetInput`] is the one a game with characters in cars uses.
pub trait VehicleCommand: NetInput {
    /// The vehicle part of this command, as it travels. Must be the same on both sides.
    fn vehicle_command(&self) -> VehicleNetInput;
}

impl VehicleCommand for VehicleNetInput {
    fn vehicle_command(&self) -> VehicleNetInput {
        self.quantized()
    }
}

// ---- the state on the wire ----------------------------------------------------------------------

/// One wheel of a [`NetVehicleState`]: how it looks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetWheelLook {
    /// Rolling angle, radians.
    pub spin_angle: f32,
    /// Suspension length, meters.
    pub suspension_length: f32,
}

/// What a vehicle's next step reads and what its looks need that the `Transform` cannot carry.
/// Predicted and replicated. Full precision on the writer's side, quantized on the wire
/// (`docs/formats/net-vehicle-wire.md`).
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct NetVehicleState {
    /// Chassis linear velocity at the center of mass, m/s.
    pub linear_velocity: Vec3,
    /// Chassis angular velocity, rad/s.
    pub angular_velocity: Vec3,
    /// The steering input of the last step, for looks.
    pub steer: f32,
    /// One entry per wheel.
    pub wheels: Vec<NetWheelLook>,
    /// The geared drivetrain's next-step state; `None` with the `Simple` drivetrain.
    pub drivetrain: Option<DrivetrainCore>,
}

impl NetVehicleState {
    /// The state of a core, with the steering of the last step.
    #[must_use]
    pub fn from_core(core: &VehicleCore, steer: f32) -> Self {
        Self {
            linear_velocity: core.linear_velocity,
            angular_velocity: core.angular_velocity,
            steer,
            wheels: core
                .wheels
                .iter()
                .map(|wheel| NetWheelLook {
                    spin_angle: wheel.spin_angle,
                    suspension_length: wheel.suspension_length,
                })
                .collect(),
            drivetrain: core.drivetrain.clone(),
        }
    }

    /// The core this state and a pose describe.
    #[must_use]
    pub fn to_core(&self, pose: &Transform) -> VehicleCore {
        VehicleCore {
            translation: pose.translation,
            rotation: pose.rotation,
            linear_velocity: self.linear_velocity,
            angular_velocity: self.angular_velocity,
            wheels: self
                .wheels
                .iter()
                .map(|wheel| WheelCoreState {
                    spin_angle: wheel.spin_angle,
                    suspension_length: wheel.suspension_length,
                })
                .collect(),
            drivetrain: self.drivetrain.clone(),
        }
    }

    fn write_velocities(&self, w: &mut BitWriter, cx: &CodecContext) {
        for value in self.linear_velocity.to_array() {
            LINEAR.write(value, w, cx);
        }
        for value in self.angular_velocity.to_array() {
            ANGULAR.write(value, w, cx);
        }
    }

    fn read_velocities(r: &mut BitReader) -> Result<(Vec3, Vec3), DecodeError> {
        let mut read = |codec: Fixed| -> Result<Vec3, DecodeError> {
            Ok(Vec3::new(codec.read(r)?, codec.read(r)?, codec.read(r)?))
        };
        Ok((read(LINEAR)?, read(ANGULAR)?))
    }

    fn write_looks(&self, w: &mut BitWriter, cx: &CodecContext) {
        STEER.write(self.steer, w, cx);
        w.write_varint(self.wheels.len() as u64);
        for wheel in &self.wheels {
            w.write_bits(SPIN_ANGLE.quantize(wheel.spin_angle), SPIN_ANGLE.bits);
            SUSPENSION.write(wheel.suspension_length, w, cx);
        }
    }

    fn read_looks(r: &mut BitReader) -> Result<(f32, Vec<NetWheelLook>), DecodeError> {
        let steer = STEER.read(r)?;
        let count = usize::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid(CODE_STATE_WHEELS))?;
        if count > MAX_WHEELS {
            return Err(DecodeError::Invalid(CODE_STATE_WHEELS));
        }
        let mut wheels = Vec::with_capacity(count);
        for _ in 0..count {
            wheels.push(NetWheelLook {
                spin_angle: SPIN_ANGLE.dequantize(r.read_bits(SPIN_ANGLE.bits)?),
                suspension_length: SUSPENSION.read(r)?,
            });
        }
        Ok((steer, wheels))
    }

    fn write_drivetrain(&self, w: &mut BitWriter, cx: &CodecContext) {
        w.write_bool(self.drivetrain.is_some());
        if let Some(core) = &self.drivetrain {
            write_engine(core, w, cx);
            write_gears(core, w, cx);
            write_wheel_cores(core, w, cx);
        }
    }

    fn read_drivetrain(r: &mut BitReader, wheels: usize) -> Result<Option<DrivetrainCore>, DecodeError> {
        if !r.read_bool()? {
            return Ok(None);
        }
        let mut core = DrivetrainCore::default();
        read_engine(&mut core, r)?;
        read_gears(&mut core, r)?;
        read_wheel_cores(&mut core, r, wheels)?;
        Ok(Some(core))
    }
}

fn write_engine(core: &DrivetrainCore, w: &mut BitWriter, cx: &CodecContext) {
    ENGINE_SPEED.write(core.engine_speed, w, cx);
    TORQUE.write(core.clutch_torque, w, cx);
    FACTOR.write(core.tc_factor, w, cx);
    for hint in core.torque_hints {
        TORQUE.write(hint, w, cx);
    }
}

fn read_engine(core: &mut DrivetrainCore, r: &mut BitReader) -> Result<(), DecodeError> {
    core.engine_speed = ENGINE_SPEED.read(r)?;
    core.clutch_torque = TORQUE.read(r)?;
    core.tc_factor = FACTOR.read(r)?;
    for hint in &mut core.torque_hints {
        *hint = TORQUE.read(r)?;
    }
    Ok(())
}

fn write_gear(gear: i32, w: &mut BitWriter) {
    w.write_bits(
        gear.saturating_add(GEAR_OFFSET).clamp(0, (1 << GEAR_BITS) - 1) as u64,
        GEAR_BITS,
    );
}

fn read_gear(r: &mut BitReader) -> Result<i32, DecodeError> {
    Ok(r.read_bits(GEAR_BITS)? as i32 - GEAR_OFFSET)
}

fn write_gears(core: &DrivetrainCore, w: &mut BitWriter, cx: &CodecContext) {
    write_gear(core.gear, w);
    write_gear(core.target_gear, w);
    TIMER.write(core.shift_timer, w, cx);
    TIMER.write(core.dwell, w, cx);
    w.write_bool(core.automatic);
}

fn read_gears(core: &mut DrivetrainCore, r: &mut BitReader) -> Result<(), DecodeError> {
    core.gear = read_gear(r)?;
    core.target_gear = read_gear(r)?;
    core.shift_timer = TIMER.read(r)?;
    core.dwell = TIMER.read(r)?;
    core.automatic = r.read_bool()?;
    Ok(())
}

fn write_wheel_cores(core: &DrivetrainCore, w: &mut BitWriter, cx: &CodecContext) {
    w.write_varint(core.wheels.len() as u64);
    for wheel in &core.wheels {
        WHEEL_SPEED.write(wheel.angular_speed, w, cx);
        w.write_bits(SPIN_ANGLE.quantize(wheel.spin_angle), SPIN_ANGLE.bits);
        SLIP.write(wheel.slip_ratio, w, cx);
        FORCE.write(wheel.tire_force, w, cx);
        FORCE.write(wheel.load, w, cx);
        FACTOR.write(wheel.abs_factor, w, cx);
    }
}

fn read_wheel_cores(core: &mut DrivetrainCore, r: &mut BitReader, wheels: usize) -> Result<(), DecodeError> {
    let count = usize::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid(CODE_STATE_WHEELS))?;
    if count != wheels || count > MAX_WHEELS {
        return Err(DecodeError::Invalid(CODE_STATE_WHEELS));
    }
    core.wheels = Vec::with_capacity(count);
    for _ in 0..count {
        core.wheels.push(WheelCore {
            angular_speed: WHEEL_SPEED.read(r)?,
            spin_angle: SPIN_ANGLE.dequantize(r.read_bits(SPIN_ANGLE.bits)?),
            slip_ratio: SLIP.read(r)?,
            tire_force: FORCE.read(r)?,
            load: FORCE.read(r)?,
            abs_factor: FACTOR.read(r)?,
        });
    }
    Ok(())
}

/// Writes a group when it differs from the base, with one change bit in front.
fn write_group(
    w: &mut BitWriter,
    cx: &CodecContext,
    write: impl Fn(&mut BitWriter, &CodecContext),
    base: impl Fn(&mut BitWriter, &CodecContext),
) {
    let scratch = cx.scratch();
    let (mut mine, mut theirs) = (BitWriter::new(), BitWriter::new());
    write(&mut mine, &scratch);
    base(&mut theirs, &scratch);
    let changed = mine.bit_len() != theirs.bit_len() || mine.as_bytes() != theirs.as_bytes();
    w.write_bool(changed);
    if changed {
        write(w, cx);
    }
}

impl ReplicateCodec for NetVehicleState {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        self.write_velocities(w, cx);
        self.write_looks(w, cx);
        self.write_drivetrain(w, cx);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let (linear_velocity, angular_velocity) = Self::read_velocities(r)?;
        let (steer, wheels) = Self::read_looks(r)?;
        let drivetrain = Self::read_drivetrain(r, wheels.len())?;
        Ok(Self {
            linear_velocity,
            angular_velocity,
            steer,
            wheels,
            drivetrain,
        })
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, cx: &CodecContext) {
        write_group(
            w,
            cx,
            |w, cx| self.write_velocities(w, cx),
            |w, cx| base.write_velocities(w, cx),
        );
        write_group(
            w,
            cx,
            |w, cx| self.write_looks(w, cx),
            |w, cx| base.write_looks(w, cx),
        );
        // A drivetrain's three parts change at different rates: gears hardly ever.
        match (&self.drivetrain, &base.drivetrain) {
            (Some(core), Some(old)) if core.wheels.len() == old.wheels.len() => {
                w.write_bool(true);
                write_group(
                    w,
                    cx,
                    |w, cx| write_engine(core, w, cx),
                    |w, cx| write_engine(old, w, cx),
                );
                write_group(
                    w,
                    cx,
                    |w, cx| write_gears(core, w, cx),
                    |w, cx| write_gears(old, w, cx),
                );
                write_group(
                    w,
                    cx,
                    |w, cx| write_wheel_cores(core, w, cx),
                    |w, cx| write_wheel_cores(old, w, cx),
                );
            }
            _ => {
                w.write_bool(false);
                self.write_drivetrain(w, cx);
            }
        }
    }

    fn decode_delta(base: &Self, r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let (linear_velocity, angular_velocity) = if r.read_bool()? {
            Self::read_velocities(r)?
        } else {
            (base.linear_velocity, base.angular_velocity)
        };
        let (steer, wheels) = if r.read_bool()? {
            Self::read_looks(r)?
        } else {
            (base.steer, base.wheels.clone())
        };
        let drivetrain = if r.read_bool()? {
            let Some(old) = &base.drivetrain else {
                return Err(DecodeError::Invalid(CODE_STATE_WHEELS));
            };
            let mut core = old.clone();
            if r.read_bool()? {
                read_engine(&mut core, r)?;
            }
            if r.read_bool()? {
                read_gears(&mut core, r)?;
            }
            if r.read_bool()? {
                read_wheel_cores(&mut core, r, wheels.len())?;
            }
            if core.wheels.len() != wheels.len() {
                return Err(DecodeError::Invalid(CODE_STATE_WHEELS));
            }
            Some(core)
        } else {
            Self::read_drivetrain(r, wheels.len())?
        };
        Ok(Self {
            linear_velocity,
            angular_velocity,
            steer,
            wheels,
            drivetrain,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        for codec in [
            LINEAR,
            ANGULAR,
            SUSPENSION,
            STEER,
            ENGINE_SPEED,
            WHEEL_SPEED,
            TORQUE,
            TIMER,
            FACTOR,
            SLIP,
            FORCE,
        ] {
            h.write_u64(u64::from(codec.lsb.to_bits()));
            h.write_u64(u64::from(codec.bits));
        }
        h.write_u64(u64::from(SPIN_ANGLE.bits));
        h.write_u64(u64::from(GEAR_BITS));
    }
}

// ---- components ---------------------------------------------------------------------------------

/// Makes a vehicle a networked one. Spawn it with [`spawn_net_vehicle`]: the server simulates it, its
/// owner predicts it, and other clients interpolate. A vehicle with no owner is server-driven and
/// reads its own [`VehicleInput`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq, Replicate)]
#[reflect(Component, Default)]
#[require(NetVehicleState)]
pub struct NetVehicle {
    /// The key of its tuning in [`NetVehicleTunings`], which both sides hold: it does not travel.
    #[replicate(max_len = 64)]
    pub tuning: String,
    /// Which of the owning peer's local players drives it.
    pub local_player: u8,
    /// The owner's peer id, for clients to read (`NetOwner` stays on the server). The server keeps it
    /// in step; `None` when the server drives it.
    pub owner_peer: Option<u32>,
}

/// The tunings networked vehicles name. Fill it the same on every process, before the first spawn.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetVehicleTunings {
    /// Tuning key to configuration.
    pub by_name: HashMap<String, VehicleConfig>,
}

impl NetVehicleTunings {
    /// Adds a tuning.
    pub fn insert(&mut self, name: impl Into<String>, config: VehicleConfig) {
        self.by_name.insert(name.into(), config);
    }
}

/// How the drawn pose pays back a correction. A resource; the defaults keep every step's visible
/// correction under 0.1 m.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct NetVehicleSmoothing {
    /// The half life of the offset, milliseconds. 0 shows corrections at once.
    pub half_life_ms: f32,
    /// The most offset one step pays back, meters.
    pub max_step_m: f32,
    /// The most offset rotation one step pays back, radians.
    pub max_step_rad: f32,
    /// A correction past this many meters snaps: the drawn pose goes straight to the new one.
    pub snap_m: f32,
    /// A rotation correction past this many radians snaps.
    pub snap_rad: f32,
}

impl Default for NetVehicleSmoothing {
    fn default() -> Self {
        Self {
            half_life_ms: 80.0,
            max_step_m: 0.09,
            max_step_rad: 0.05,
            snap_m: 2.0,
            snap_rad: std::f32::consts::FRAC_PI_2,
        }
    }
}

/// What is drawn for a predicted vehicle: [`Self::rendered`]. Separate from the simulated state, which
/// jumps to a correction at once.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct NetVehicleVisual {
    /// The simulated position plus this is drawn.
    pub offset: Vec3,
    /// This times the simulated rotation is drawn.
    pub rotation_offset: Quat,
    /// The size of the last correction, meters.
    pub last_correction_m: f32,
    /// Corrections this client has seen.
    pub corrections: u64,
    /// How many of them snapped.
    pub snaps: u64,
}

impl Default for NetVehicleVisual {
    fn default() -> Self {
        Self {
            offset: Vec3::ZERO,
            rotation_offset: Quat::IDENTITY,
            last_correction_m: 0.0,
            corrections: 0,
            snaps: 0,
        }
    }
}

impl NetVehicleVisual {
    /// The drawn pose of a simulated transform.
    #[must_use]
    pub fn rendered(&self, simulated: &Transform) -> Transform {
        Transform {
            translation: simulated.translation + self.offset,
            rotation: (self.rotation_offset * simulated.rotation).normalize(),
            scale: simulated.scale,
        }
    }

    /// Takes a correction from `old` to `new` into the offset, or snaps past the limits.
    fn take_correction(&mut self, old: &Transform, new: &Transform, smoothing: &NetVehicleSmoothing) {
        let moved = old.translation.distance(new.translation);
        let turned = old.rotation.angle_between(new.rotation);
        self.last_correction_m = moved;
        self.corrections += 1;
        self.offset += old.translation - new.translation;
        self.rotation_offset = (old.rotation * new.rotation.inverse() * self.rotation_offset).normalize();
        if moved > smoothing.snap_m
            || turned > smoothing.snap_rad
            || self.offset.length() > smoothing.snap_m
            || self.rotation_offset.angle_between(Quat::IDENTITY) > smoothing.snap_rad
        {
            self.snaps += 1;
            self.offset = Vec3::ZERO;
            self.rotation_offset = Quat::IDENTITY;
        }
    }

    /// One step's payback: exponential, never over the limits.
    fn decay(&mut self, smoothing: &NetVehicleSmoothing, step_ms: f32) {
        let keep = if smoothing.half_life_ms > 0.0 {
            0.5f32.powf(step_ms / smoothing.half_life_ms)
        } else {
            0.0
        };
        let size = self.offset.length();
        if size > 0.0 {
            let pay = (size * (1.0 - keep)).min(smoothing.max_step_m);
            let left = size - pay;
            self.offset = if left < 1.0e-5 {
                Vec3::ZERO
            } else {
                self.offset * (left / size)
            };
        }
        let angle = self.rotation_offset.angle_between(Quat::IDENTITY);
        if angle > 0.0 {
            let pay = (angle * (1.0 - keep)).min(smoothing.max_step_rad);
            let left = angle - pay;
            self.rotation_offset = if left < 1.0e-5 {
                Quat::IDENTITY
            } else {
                Quat::IDENTITY
                    .slerp(self.rotation_offset, left / angle)
                    .normalize()
            };
        }
    }

    /// Whether the offset has been paid back.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.offset == Vec3::ZERO && self.rotation_offset == Quat::IDENTITY
    }
}

/// The owning client's private world and what its last step left.
#[derive(Component, Debug)]
pub struct NetVehiclePredictor {
    sim: VehicleSimWorld,
    /// What the step left: a difference later means a rollback wrote the entity.
    left: Option<(Transform, NetVehicleState)>,
    /// The pose after the last first run of a tick: the drawn pose follows from here.
    first_run: Option<Transform>,
    /// Ticks since the statics were last compared.
    statics_age: u32,
}

impl NetVehiclePredictor {
    /// The private world.
    #[must_use]
    pub fn sim(&self) -> &VehicleSimWorld {
        &self.sim
    }
}

/// Server: what the vehicle's last send cost by the codec, for `net.vehicles` when the relevance
/// pipeline with its own counters does not run.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct NetVehicleWireMeter {
    /// The state as of the last send.
    sent: NetVehicleState,
    /// Bytes of the last ticks' records, oldest first.
    bytes: std::collections::VecDeque<u32>,
    /// Ticks counted since the last send.
    since_send: u32,
    /// The pose at the last capture: the physics step writes `Transform` unnoticed.
    pose: Option<Transform>,
}

/// Records a meter keeps: a little over a second at 30 Hz.
const METER_RECORDS: usize = 32;
/// A root's fast-path transform: 12 bytes of position, 32 bits of rotation, one flag bit.
const METER_TRANSFORM_BYTES: u32 = 17;
/// A record's framing: id, length, kind and tick offsets (the send pipeline's own figure).
const METER_FRAMING_BYTES: u32 = 6;

impl NetVehicleWireMeter {
    /// Counts this tick. `every` is the send interval in ticks.
    fn note(&mut self, state: &NetVehicleState, every: u32) {
        self.since_send += 1;
        if self.since_send < every.max(1) {
            return;
        }
        self.since_send = 0;
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        state.encode_delta(&self.sent, &mut w, &cx);
        let bytes = w.bit_len().div_ceil(8) as u32 + METER_TRANSFORM_BYTES + METER_FRAMING_BYTES;
        self.sent = state.clone();
        self.bytes.push_back(bytes);
        while self.bytes.len() > METER_RECORDS {
            self.bytes.pop_front();
        }
    }

    /// Bytes per second to one connection: the mean record at the send rate.
    #[must_use]
    pub fn per_second(&self, tick_hz: f64, every: u32) -> f64 {
        if self.bytes.is_empty() {
            return 0.0;
        }
        let mean = self.bytes.iter().map(|b| f64::from(*b)).sum::<f64>() / self.bytes.len() as f64;
        mean * tick_hz / f64::from(every.max(1))
    }
}

/// The last thing that went wrong with a networked vehicle on this process: a stable code.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct NetVehicleFault(pub Option<&'static str>);

// ---- the comparison -----------------------------------------------------------------------------

fn drivetrains_match(a: &DrivetrainCore, b: &DrivetrainCore) -> bool {
    let close = |x: f32, y: f32, tolerance: f32| (x - y).abs() <= tolerance;
    a.gear == b.gear
        && a.target_gear == b.target_gear
        && a.automatic == b.automatic
        && close(a.engine_speed, b.engine_speed, TOLERANCE_SPIN)
        && close(a.shift_timer, b.shift_timer, TOLERANCE_TIMER_S)
        && close(a.dwell, b.dwell, TOLERANCE_TIMER_S)
        && a.wheels.len() == b.wheels.len()
        && a.wheels
            .iter()
            .zip(&b.wheels)
            .all(|(x, y)| close(x.angular_speed, y.angular_speed, TOLERANCE_SPIN))
}

/// The vehicle comparison over one tick's values: pose, velocities, the drivetrain's gears and speeds.
/// A wheel's looks are never compared: nothing reads them.
fn pose_and_state_match(
    (ta, sa): (&Transform, &NetVehicleState),
    (tb, sb): (&Transform, &NetVehicleState),
) -> bool {
    ta.translation.distance(tb.translation) <= TOLERANCE_POSITION_M
        && ta.rotation.angle_between(tb.rotation) <= TOLERANCE_ROTATION_RAD
        && sa.linear_velocity.distance(sb.linear_velocity) <= TOLERANCE_VELOCITY
        && sa.angular_velocity.distance(sb.angular_velocity) <= TOLERANCE_ANGULAR
        && match (&sa.drivetrain, &sb.drivetrain) {
            (Some(a), Some(b)) => drivetrains_match(a, b),
            (None, None) => true,
            _ => false,
        }
}

fn vehicle_matches(predicted: &PredictedValues<'_>, server: &PredictedValues<'_>) -> Option<bool> {
    Some(pose_and_state_match(
        (predicted.get::<Transform>()?, predicted.get::<NetVehicleState>()?),
        (server.get::<Transform>()?, server.get::<NetVehicleState>()?),
    ))
}

fn vehicle_error(_: &PredictedValues<'_>, _: &PredictedValues<'_>) -> Option<f32> {
    None
}

/// The vehicle tolerance over `Transform` and [`NetVehicleState`].
pub fn vehicle_tolerance() -> EntityTolerance {
    EntityTolerance {
        covers: vec![TypeId::of::<Transform>(), TypeId::of::<NetVehicleState>()],
        matches: vehicle_matches,
        error_m: vehicle_error,
    }
}

// ---- the step -----------------------------------------------------------------------------------

/// A fresh predictor on the entity's current values.
fn new_predictor(world: &World, entity: Entity, step: f32) -> Result<NetVehiclePredictor, &'static str> {
    let vehicle = world.get::<NetVehicle>(entity).ok_or("vehicle_not_networked")?;
    let tunings = world
        .get_resource::<NetVehicleTunings>()
        .ok_or("vehicle_tuning_unknown")?;
    let config = tunings
        .by_name
        .get(&vehicle.tuning)
        .cloned()
        .ok_or("vehicle_tuning_unknown")?;
    let shared = world
        .get_resource::<PhysicsWorld>()
        .ok_or(VehicleError::NoPhysicsWorld.code())?;
    let settings = world
        .get_resource::<PhysicsSettings>()
        .map(VehicleSimSettings::from_settings)
        .unwrap_or_default();
    let pose = world
        .get::<Transform>(entity)
        .map_or((Vec3::ZERO, Quat::IDENTITY), |t| (t.translation, t.rotation));
    let sim = VehicleSimWorld::new(config, shared, settings, pose, step).map_err(|error| error.code())?;
    Ok(NetVehiclePredictor {
        sim,
        left: None,
        first_run: None,
        statics_age: 0,
    })
}

/// Records a fault on the entity.
fn note_fault(world: &mut World, entity: Entity, code: Option<&'static str>) {
    if let Ok(mut ent) = world.get_entity_mut(entity) {
        match ent.get_mut::<NetVehicleFault>() {
            Some(mut fault) if fault.0 != code => fault.0 = code,
            Some(_) => {}
            None => {
                ent.insert(NetVehicleFault(code));
            }
        }
    }
}

/// How many ticks pass between comparisons of the shared world's static colliders.
const STATICS_CHECK_TICKS: u32 = 30;

/// Compares the private world's statics with the shared world's, every so many ticks.
fn refresh_statics(world: &mut World, entity: Entity, dt: f32) {
    let due = world
        .get_mut::<NetVehiclePredictor>(entity)
        .is_some_and(|mut predictor| {
            predictor.statics_age += 1;
            predictor.statics_age >= STATICS_CHECK_TICKS
        });
    if !due || !world.contains_resource::<PhysicsWorld>() {
        return;
    }
    world.resource_scope(|world, shared: Mut<PhysicsWorld>| {
        if let Some(mut predictor) = world.get_mut::<NetVehiclePredictor>(entity) {
            predictor.statics_age = 0;
            if let Err(error) = predictor.sim.sync_statics(&shared, dt) {
                tracing::warn!(target: "net", code = error.code(), "net.vehicle_statics_sync_failed");
            }
        }
    });
}

/// One predicted tick of one vehicle.
fn step_predicted(
    world: &mut World,
    entity: Entity,
    command: VehicleNetInput,
    first_time: bool,
    dt: f32,
    smoothing: NetVehicleSmoothing,
    layers: Option<&PhysicsLayers>,
) {
    if world.get::<NetVehiclePredictor>(entity).is_none() {
        match new_predictor(world, entity, dt) {
            Ok(predictor) => {
                world
                    .entity_mut(entity)
                    .insert((predictor, NetVehicleVisual::default()));
            }
            Err(code) => {
                note_fault(world, entity, Some(code));
                return;
            }
        }
    }
    if first_time {
        refresh_statics(world, entity, dt);
    }
    let (Some(pose), Some(state)) = (
        world.get::<Transform>(entity).copied(),
        world.get::<NetVehicleState>(entity).cloned(),
    ) else {
        return;
    };
    let mut visual = world.get::<NetVehicleVisual>(entity).copied().unwrap_or_default();
    let Some(mut predictor) = world.get_mut::<NetVehiclePredictor>(entity) else {
        return;
    };
    let predictor = &mut *predictor;
    let mut fault = None;

    // Someone wrote the entity since the last step: a rollback, an adoption, the first server state.
    let written = predictor
        .left
        .as_ref()
        .is_none_or(|(p, s)| *p != pose || *s != state);
    if written && let Err(error) = predictor.sim.apply_core(&state.to_core(&pose)) {
        fault = Some(error.code());
    }

    // The drawn pose follows the first run of each tick; a correction is what that run finds changed.
    if first_time
        && let Some(old) = predictor.first_run
        && (old.translation != pose.translation || old.rotation != pose.rotation)
    {
        visual.take_correction(&old, &pose, &smoothing);
    }

    if command.shift != VehicleShift::None {
        let request = shift_request(command.shift);
        // A gearbox that refuses keeps its gear, and refuses the same way on the server.
        let _ = predictor.sim.request_shift(request);
    }
    if let Err(error) = predictor.sim.step(command.vehicle_input(), dt, layers) {
        fault = Some(error.code());
    }

    let (next_pose, next_state) = match predictor.sim.vehicle().core(predictor.sim.physics()) {
        Ok(core) => (
            Transform {
                translation: core.translation,
                rotation: core.rotation,
                scale: pose.scale,
            },
            NetVehicleState::from_core(&core, predictor.sim.vehicle().input().steer),
        ),
        Err(error) => {
            fault = Some(error.code());
            (pose, state)
        }
    };
    predictor.left = Some((next_pose, next_state.clone()));
    if first_time {
        predictor.first_run = Some(next_pose);
        visual.decay(&smoothing, dt * 1000.0);
    }
    if let Some(mut stored) = world.get_mut::<Transform>(entity) {
        *stored = next_pose;
    }
    if let Some(mut stored) = world.get_mut::<NetVehicleState>(entity) {
        *stored = next_state;
    }
    if let Some(mut stored) = world.get_mut::<NetVehicleVisual>(entity) {
        *stored = visual;
    }
    if first_time {
        write_net_offset(world, entity, &visual, dt);
    }
    note_fault(world, entity, fault);
}

fn shift_request(shift: VehicleShift) -> ShiftRequest {
    match shift {
        VehicleShift::Down => ShiftRequest::Down,
        _ => ShiftRequest::Up,
    }
}

/// Keeps `regia_net`'s own offset, which visual children and the camera follow, on the drawn pose:
/// `regia_net` decays it once more after the schedule, so this stores what decays to the right value.
fn write_net_offset(world: &mut World, entity: Entity, visual: &NetVehicleVisual, dt: f32) {
    let half_life = world
        .get_resource::<regia_net::NetSettings>()
        .map_or(0.0, |s| s.prediction.smooth_half_life_ms) as f32;
    let keep = if half_life > 0.0 {
        0.5f32.powf(dt * 1000.0 / half_life)
    } else {
        0.0
    };
    let offset = if keep > 0.0 {
        // Invert `regia_net`'s nlerp decay (`decay_offsets`, ADR-4770), not the old slerp one:
        // nlerp(IDENTITY, q, keep) normalizes `(1 - keep)·IDENTITY + keep·q`, so storing
        // `(q - (1 - keep)·IDENTITY) / keep` makes the decayed rotation land back on the drawn
        // `visual.rotation_offset`. The previous slerp inverse rendered up to ~2° off at large
        // offsets (audit kimirev-fix-net1-determinism, finding 1).
        let inv = 1.0 / keep;
        let stored = Quat::from_xyzw(
            visual.rotation_offset.x * inv,
            visual.rotation_offset.y * inv,
            visual.rotation_offset.z * inv,
            (visual.rotation_offset.w - (1.0 - keep)) * inv,
        );
        NetVisualOffset {
            translation: visual.offset / keep,
            rotation: stored,
        }
    } else {
        NetVisualOffset::default()
    };
    if let Ok(mut ent) = world.get_entity_mut(entity) {
        ent.insert(offset);
    }
}

/// Server: copies each vehicle's state into its replicated components, after the physics step. The
/// step writes `Transform` without change detection (a rendered pose is not a change for the game), so
/// a pose that moved is marked here, or replication would never see it.
fn capture_server_state(world: &mut World) {
    let rows: Vec<Entity> = {
        let mut q = world.query_filtered::<Entity, (With<NetVehicle>, With<VehicleController>)>();
        q.iter(world).collect()
    };
    let every = world
        .get_resource::<regia_net::NetSettings>()
        .map_or(1, |settings| settings.replication.send_every_ticks);
    for entity in rows {
        let next = {
            let Some(physics) = world.get_resource::<PhysicsWorld>() else {
                return;
            };
            let Some(controller) = world.get::<VehicleController>(entity) else {
                continue;
            };
            controller
                .vehicle
                .core(physics)
                .map(|core| NetVehicleState::from_core(&core, controller.vehicle.input().steer))
        };
        match next {
            Ok(state) => {
                if let Some(mut stored) = world.get_mut::<NetVehicleState>(entity)
                    && *stored != state
                {
                    stored.clone_from(&state);
                }
                if world.get::<NetVehicleWireMeter>(entity).is_none() {
                    world.entity_mut(entity).insert(NetVehicleWireMeter::default());
                }
                let pose = world.get::<Transform>(entity).copied();
                let moved = world
                    .get_mut::<NetVehicleWireMeter>(entity)
                    .is_some_and(|mut meter| {
                        meter.note(&state, every);
                        let moved = meter.pose != pose;
                        meter.pose = pose;
                        moved
                    });
                if moved && let Some(mut pose) = world.get_mut::<Transform>(entity) {
                    pose.set_changed();
                }
            }
            Err(error) => note_fault(world, entity, Some(error.code())),
        }
    }
}

/// The predicted system: on the server it captures the state the physics step left; on a client it
/// steps every vehicle this client predicts, in `NetId` order.
pub fn net_vehicle_step<I: VehicleCommand>(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_framework", "net_vehicle_step").entered();
    let Some(state) = world.get_resource::<PredictionState>().copied() else {
        return;
    };
    if state.server {
        capture_server_state(world);
        return;
    }
    let commands: HashMap<NetPlayerId, I> = {
        let mut q = world.query::<(&NetPlayer, &InputCommand<I>)>();
        q.iter(world).map(|(p, c)| (p.id, c.value.clone())).collect()
    };
    let mut rows: Vec<(Option<NetId>, Entity, u8, bool)> = {
        let mut q = world.query::<(Entity, &NetVehicle, Option<&NetId>, Option<&NetRole>)>();
        q.iter(world)
            .map(|(entity, vehicle, id, role)| {
                (
                    id.copied(),
                    entity,
                    vehicle.local_player,
                    matches!(role, Some(NetRole::Predicted)),
                )
            })
            .collect()
    };
    rows.sort_unstable_by_key(|(id, entity, _, _)| (*id, *entity));
    // The physics step's own length, to the bit: `tick_step_seconds_of` rounds differently.
    let dt = world
        .get_resource::<FixedTime>()
        .map_or_else(|| tick_step_seconds_of(world), |fixed| fixed.step().as_secs_f32());
    let smoothing = world
        .get_resource::<NetVehicleSmoothing>()
        .copied()
        .unwrap_or_default();
    let layers = world.get_resource::<PhysicsLayers>().cloned();
    for (_, entity, local, predicted) in rows {
        if !predicted {
            // It stopped being ours: an owner change hands it to the interpolator.
            if world.get::<NetVehiclePredictor>(entity).is_some()
                && let Ok(mut ent) = world.get_entity_mut(entity)
            {
                ent.remove::<(NetVehiclePredictor, NetVehicleVisual)>();
            }
            continue;
        }
        let command = state.local_peer.map_or_else(VehicleNetInput::default, |peer| {
            commands
                .get(&NetPlayerId { peer, local })
                .map_or_else(VehicleNetInput::default, I::vehicle_command)
        });
        step_predicted(
            world,
            entity,
            command,
            state.first_time,
            dt,
            smoothing,
            layers.as_ref(),
        );
    }
}

/// Server, before the physics step: handles seat requests, then writes each owned vehicle's
/// [`VehicleInput`] from its owner's command. A vehicle with no owner keeps the input the server wrote.
/// A vehicle whose owner went away without a seat change (the session's hand-off when its peer left:
/// `NetOwner` removed) has its controls released once, its driver's seat freed of a pawn that left or
/// was handed to the server, and `NetVehicle::owner_peer` brought in step.
/// In a game with an [`App`], [`NetVehiclePlugin`] runs it after `NetSet::Apply` and before the physics
/// step.
pub fn net_vehicle_server_apply<I: VehicleCommand>(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_framework", "net_vehicle_server_apply").entered();
    if !world
        .get_resource::<regia_net::NetMode>()
        .is_some_and(|mode| mode.is_server())
    {
        return;
    }
    handle_seat_requests(world);
    release_departed_drivers(world);
    free_departed_occupants(world);
    let commands: HashMap<NetPlayerId, I> = {
        let mut q = world.query::<(&NetPlayer, &InputCommand<I>)>();
        q.iter(world).map(|(p, c)| (p.id, c.value.clone())).collect()
    };
    let rows: Vec<(Entity, NetPlayerId)> = {
        let mut q = world.query_filtered::<(Entity, &NetVehicle, &NetOwner), With<VehicleController>>();
        q.iter(world)
            .map(|(entity, vehicle, owner)| {
                (
                    entity,
                    NetPlayerId {
                        peer: owner.0,
                        local: vehicle.local_player,
                    },
                )
            })
            .collect()
    };
    for (entity, player) in rows {
        let command = commands
            .get(&player)
            .map_or_else(VehicleNetInput::default, I::vehicle_command);
        if command.shift != VehicleShift::None
            && let Some(mut controller) = world.get_mut::<VehicleController>(entity)
        {
            let _ = controller.vehicle.request_shift(shift_request(command.shift));
        }
        let input = command.vehicle_input();
        if let Some(mut stored) = world.get_mut::<VehicleInput>(entity)
            && *stored != input
        {
            *stored = input;
        }
    }
}

/// Server: the peer whose command [`net_vehicle_server_apply`] last drove each vehicle. The session's
/// hand-off of a departed peer (rc1-rev-net-k 1) removes `NetOwner` behind the seats' back; this is
/// how the next tick tells that from a car the server always drove. Not replicated.
#[derive(Resource, Debug, Default)]
struct VehicleDrivers(HashMap<Entity, PeerId>);

/// Server: keeps [`VehicleDrivers`] in step with the owner a seat change wrote in
/// [`refresh_vehicle_owner`], so the next [`release_departed_drivers`] tick tells an ordinary seat
/// exit (the record left with the owner) from a peer that actually left (the session removed
/// `NetOwner` without a seat change, so the record still names the old owner).
fn note_seat_owner(world: &mut World, vehicle: Entity, owner: Option<PeerId>) {
    let mut drivers = world.get_resource_or_insert_with(VehicleDrivers::default);
    match owner {
        Some(peer) => {
            drivers.0.insert(vehicle, peer);
        }
        None => {
            drivers.0.remove(&vehicle);
        }
    }
}

/// Server: brings each vehicle's driver record in step with its `NetOwner`. A vehicle that lost its
/// owner this way releases its controls (`VehicleNetInput::on_missing` held the last pedals through
/// the reconnect grace, and nothing else writes them again) and its driver's seat; a car the server
/// drives, which never had an owner, is left alone. A seat change keeps the record in step itself
/// (`refresh_vehicle_owner` through [`note_seat_owner`]), so only the session's hand-off — `NetOwner`
/// gone while the record still names the old owner — reads as a departure.
fn release_departed_drivers(world: &mut World) {
    let rows: Vec<(Entity, Option<PeerId>)> = {
        let mut q = world
            .query_filtered::<(Entity, Option<&NetOwner>), (With<NetVehicle>, With<VehicleController>)>();
        q.iter(world)
            .map(|(entity, owner)| (entity, owner.map(|o| o.0)))
            .collect()
    };
    let mut drivers = std::mem::take(&mut world.get_resource_or_insert_with(VehicleDrivers::default).0);
    // Forget despawned vehicles, so the record stays as large as the fleet.
    if drivers.len() > rows.len() {
        let live: std::collections::HashSet<Entity> = rows.iter().map(|(entity, _)| *entity).collect();
        drivers.retain(|entity, _| live.contains(entity));
    }
    for (vehicle, owner) in rows {
        let before = drivers.get(&vehicle).copied();
        if before == owner {
            continue;
        }
        if before.is_some() && owner.is_none() {
            release_vehicle(world, vehicle);
        }
        let owner = world.get::<NetOwner>(vehicle).map(|o| o.0);
        match owner {
            Some(peer) => drivers.insert(vehicle, peer),
            None => drivers.remove(&vehicle),
        };
        if let Some(net) = world.get::<NetVehicle>(vehicle)
            && net.owner_peer != owner.map(|p| p.0)
            && let Some(mut net) = world.get_mut::<NetVehicle>(vehicle)
        {
            net.owner_peer = owner.map(|p| p.0);
        }
    }
    world.resource_mut::<VehicleDrivers>().0 = drivers;
}

/// Server: a vehicle whose owner was taken away without a seat change. Its driver's seat held the
/// owner's pawn (`refresh_vehicle_owner` keeps them so): a pawn that is gone leaves the seat, one
/// handed to the server gets out, one handed to a live peer keeps driving for that peer. Then the
/// controls are released unless someone owns the car again.
fn release_vehicle(world: &mut World, vehicle: Entity) {
    let driver_seat = world.get::<VehicleSeats>(vehicle).and_then(|seats| {
        seats
            .seats
            .iter()
            .enumerate()
            .find_map(|(index, seat)| seat.occupant.filter(|_| seat.driver).map(|id| (index, id)))
    });
    if let Some((index, id)) = driver_seat {
        let pawn = world
            .get_resource::<NetEntities>()
            .and_then(|entities| entities.entity(id))
            .filter(|pawn| world.get_entity(*pawn).is_ok());
        match pawn {
            None => {
                if let Some(mut seats) = world.get_mut::<VehicleSeats>(vehicle)
                    && let Some(seat) = seats.seats.get_mut(index)
                {
                    seat.occupant = None;
                }
            }
            Some(pawn) if world.get::<NetOwner>(pawn).is_none() => {
                let vehicle_id = net_id_of(world, vehicle);
                let seated_here = vehicle_id.is_some()
                    && world
                        .get::<NetSeated>(pawn)
                        .is_some_and(|s| s.vehicle == vehicle_id && usize::from(s.seat) == index);
                if seated_here {
                    let _ = exit_seat(world, pawn);
                } else if let Some(mut seats) = world.get_mut::<VehicleSeats>(vehicle)
                    && let Some(seat) = seats.seats.get_mut(index)
                {
                    // The pawn sits elsewhere: the seat's record was stale.
                    seat.occupant = None;
                }
            }
            Some(_) => {}
        }
        refresh_vehicle_owner(world, vehicle);
    }
    if world.get::<NetOwner>(vehicle).is_none()
        && world
            .get::<VehicleInput>(vehicle)
            .is_some_and(|input| *input != VehicleInput::default())
        && let Some(mut input) = world.get_mut::<VehicleInput>(vehicle)
    {
        *input = VehicleInput::default();
    }
    tracing::info!(target: "net", vehicle = vehicle.to_bits(), "net.vehicle_owner_left");
}

/// Server: frees every seat whose occupant no longer exists (its pawn was despawned, for example by
/// a game's rule when its peer left), passengers included; ADR-7399 freed only the driver's seat, and
/// only when the car lost its owner. An occupant that is still there keeps its seat whoever owns it
/// now (a pawn handed to a live peer). A freed driver's seat gives the car back to the server.
/// Nothing is written while every occupant resolves, so a steady tick changes no component.
fn free_departed_occupants(world: &mut World) {
    // A cheap shared copy (the map is copy on write), so the query below can borrow the world.
    let Some(entities) = world.get_resource::<NetEntities>().cloned() else {
        return;
    };
    let mut stale: Vec<(Entity, usize, bool)> = Vec::new();
    // `try_query`: a world that never had seats registers nothing here.
    let Some(mut q) = world.try_query::<(Entity, &VehicleSeats)>() else {
        return;
    };
    for (vehicle, seats) in q.iter(world) {
        for (index, seat) in seats.seats.iter().enumerate() {
            let Some(id) = seat.occupant else {
                continue;
            };
            let live = entities
                .entity(id)
                .is_some_and(|pawn| world.get_entity(pawn).is_ok());
            if !live {
                stale.push((vehicle, index, seat.driver));
            }
        }
    }
    let mut drivers_freed: Vec<Entity> = Vec::new();
    for (vehicle, index, driver) in stale {
        if let Some(mut seats) = world.get_mut::<VehicleSeats>(vehicle)
            && let Some(seat) = seats.seats.get_mut(index)
        {
            seat.occupant = None;
        }
        if driver && !drivers_freed.contains(&vehicle) {
            drivers_freed.push(vehicle);
        }
        tracing::info!(target: "net", vehicle = vehicle.to_bits(), seat = index, "net.vehicle_seat_freed");
    }
    for vehicle in drivers_freed {
        refresh_vehicle_owner(world, vehicle);
    }
}

// ---- seats --------------------------------------------------------------------------------------

/// The [`MovementMode::Custom`] id of a seated character.
pub const SEATED_MODE_ID: u32 = 0x5EA7;

/// One seat of a vehicle.
#[derive(Debug, Clone, Default, PartialEq, Replicate)]
pub struct VehicleSeat {
    /// Where the character sits, in chassis space.
    pub offset: Vec3,
    /// Where a character leaving this seat stands, in chassis space.
    pub exit_offset: Vec3,
    /// Whoever sits here owns the vehicle and drives it.
    pub driver: bool,
    /// The character in the seat.
    pub occupant: Option<NetId>,
}

/// A vehicle's seats. Replicated; only the server writes them. Change who sits where with
/// [`enter_seat`] and [`exit_seat`].
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct VehicleSeats {
    /// The seats.
    #[replicate(max_len = 16)]
    pub seats: Vec<VehicleSeat>,
}

/// On a character: the vehicle it sits in. Replicated; only the server writes it. While `vehicle` is
/// set the character rides the seat and its own movement does nothing.
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct NetSeated {
    /// The vehicle; `None` standing.
    pub vehicle: Option<NetId>,
    /// The seat's index.
    pub seat: u8,
}

/// How far a character may be from a seat to enter it, meters. A resource on the server.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct VehicleSeatRules {
    /// The most distance between a standing character and the seat.
    pub reach_m: f32,
}

impl Default for VehicleSeatRules {
    fn default() -> Self {
        Self { reach_m: 6.0 }
    }
}

/// Why a seat change was refused. [`VehicleSeatError::code`] is the stable identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VehicleSeatError {
    /// The target is not a vehicle with seats.
    #[error("the target is not a vehicle with seats")]
    NoVehicle,
    /// The vehicle has no such seat.
    #[error("the vehicle has no seat {0}")]
    NoSuchSeat(u8),
    /// Someone else sits there.
    #[error("seat {0} is taken")]
    Taken(u8),
    /// The sender has no networked character.
    #[error("no networked character to seat")]
    NoCharacter,
    /// The character is too far from the seat.
    #[error("the character is too far from the seat")]
    TooFar,
    /// The character is in no seat to leave.
    #[error("the character is not seated")]
    NotSeated,
}

impl VehicleSeatError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoVehicle => "vehicle_seat_no_vehicle",
            Self::NoSuchSeat(_) => "vehicle_seat_no_such_seat",
            Self::Taken(_) => "vehicle_seat_taken",
            Self::NoCharacter => "vehicle_seat_no_character",
            Self::TooFar => "vehicle_seat_too_far",
            Self::NotSeated => "vehicle_seat_not_seated",
        }
    }
}

/// Client to server: put my character in a seat, or take it out.
#[derive(Debug, Clone, Default, PartialEq, NetMessage)]
#[net(direction = "to_server", id = "regia.vehicle_seat_request", validate = Self::check)]
pub struct VehicleSeatRequest {
    /// The vehicle; ignored when leaving.
    pub vehicle: Option<NetId>,
    /// The seat's index; ignored when leaving.
    pub seat: u8,
    /// Whether to enter (else leave).
    pub enter: bool,
}

impl VehicleSeatRequest {
    fn check(&self, _cx: &ValidateContext) -> Verdict {
        if self.enter && self.vehicle.is_none() {
            return Verdict::Reject(VehicleSeatError::NoVehicle.code());
        }
        Verdict::Accept
    }
}

/// What the server last did with seat requests. A resource on the server.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct VehicleSeatLog {
    /// Requests carried out.
    pub accepted: u64,
    /// Requests refused.
    pub refused: u64,
    /// The code of the last refusal.
    pub last_refusal: Option<&'static str>,
}

fn net_id_of(world: &World, entity: Entity) -> Option<NetId> {
    world.get::<NetId>(entity).copied()
}

/// Whoever sits in a driver's seat owns the vehicle; an empty driver's seat leaves it to the server,
/// with its controls released. The one place a seat change writes `NetOwner`, so a car always has at
/// most one.
fn refresh_vehicle_owner(world: &mut World, vehicle: Entity) {
    let driver = world
        .get::<VehicleSeats>(vehicle)
        .and_then(|seats| {
            seats
                .seats
                .iter()
                .find(|seat| seat.driver && seat.occupant.is_some())
        })
        .and_then(|seat| seat.occupant);
    let driver = driver
        .and_then(|id| world.get_resource::<NetEntities>()?.entity(id))
        .and_then(|character| world.get::<NetOwner>(character).copied());
    let current = world.get::<NetOwner>(vehicle).copied();
    if driver.map(|o| o.0) == current.map(|o| o.0) {
        return;
    }
    if let Ok(mut ent) = world.get_entity_mut(vehicle) {
        match driver {
            Some(owner) => {
                ent.insert(owner);
            }
            None => {
                ent.remove::<NetOwner>();
                ent.insert(VehicleInput::default());
            }
        }
    }
    if let Some(mut net) = world.get_mut::<NetVehicle>(vehicle) {
        net.owner_peer = driver.map(|o| o.0.0);
    }
    // This was an ordinary seat change, not a departure: the driver record follows it, or the
    // next `release_departed_drivers` tick would release and log a driver that merely got out.
    note_seat_owner(world, vehicle, driver.map(|owner| owner.0));
}

/// Server: frees the seat a character sits in, if any.
fn vacate(world: &mut World, character: Entity) -> Option<(Entity, u8)> {
    let seated = world.get::<NetSeated>(character).cloned()?;
    let id = seated.vehicle?;
    let vehicle = world.get_resource::<NetEntities>()?.entity(id)?;
    if let Some(mut seats) = world.get_mut::<VehicleSeats>(vehicle)
        && let Some(seat) = seats.seats.get_mut(usize::from(seated.seat))
    {
        seat.occupant = None;
    }
    Some((vehicle, seated.seat))
}

/// Server: puts a character in a seat and, for a driver's seat, makes its owner the vehicle's. A
/// character seated elsewhere moves: a swap inside a car at speed needs no reach and no empty tick.
/// The new owner's command drives the very tick of the change, so no input goes unused.
///
/// # Errors
/// A refusal; nothing changed then.
pub fn enter_seat(
    world: &mut World,
    vehicle: Entity,
    character: Entity,
    seat: u8,
) -> Result<(), VehicleSeatError> {
    let vehicle_id = net_id_of(world, vehicle).ok_or(VehicleSeatError::NoVehicle)?;
    let character_id = net_id_of(world, character).ok_or(VehicleSeatError::NoCharacter)?;
    let seats = world
        .get::<VehicleSeats>(vehicle)
        .ok_or(VehicleSeatError::NoVehicle)?;
    let target = seats
        .seats
        .get(usize::from(seat))
        .ok_or(VehicleSeatError::NoSuchSeat(seat))?;
    if target.occupant.is_some_and(|id| id != character_id) {
        return Err(VehicleSeatError::Taken(seat));
    }
    let offset = target.offset;
    let already_inside = world
        .get::<NetSeated>(character)
        .is_some_and(|s| s.vehicle == Some(vehicle_id));
    if !already_inside {
        let reach = world
            .get_resource::<VehicleSeatRules>()
            .copied()
            .unwrap_or_default()
            .reach_m;
        let at = world
            .get::<Transform>(vehicle)
            .ok_or(VehicleSeatError::NoVehicle)?;
        let seat_at = at.translation + at.rotation * offset;
        let stands = world
            .get::<Transform>(character)
            .ok_or(VehicleSeatError::NoCharacter)?;
        if stands.translation.distance(seat_at) > reach {
            return Err(VehicleSeatError::TooFar);
        }
    }
    let old = vacate(world, character);
    if let Some(mut seats) = world.get_mut::<VehicleSeats>(vehicle)
        && let Some(target) = seats.seats.get_mut(usize::from(seat))
    {
        target.occupant = Some(character_id);
    }
    if let Ok(mut ent) = world.get_entity_mut(character) {
        ent.insert(NetSeated {
            vehicle: Some(vehicle_id),
            seat,
        });
    }
    if let Some((old_vehicle, _)) = old
        && old_vehicle != vehicle
    {
        refresh_vehicle_owner(world, old_vehicle);
    }
    refresh_vehicle_owner(world, vehicle);
    Ok(())
}

/// Server: takes a character out of its seat and stands it at the seat's exit, moving with the car it
/// left. Returns the vehicle.
///
/// # Errors
/// [`VehicleSeatError::NotSeated`]; nothing changed then.
pub fn exit_seat(world: &mut World, character: Entity) -> Result<Entity, VehicleSeatError> {
    let (vehicle, seat) = vacate(world, character).ok_or(VehicleSeatError::NotSeated)?;
    let exit = world
        .get::<VehicleSeats>(vehicle)
        .and_then(|seats| seats.seats.get(usize::from(seat)))
        .map_or(Vec3::ZERO, |seat| seat.exit_offset);
    let (at, velocity) = (
        world.get::<Transform>(vehicle).copied().unwrap_or_default(),
        world
            .get::<NetVehicleState>(vehicle)
            .map_or(Vec3::ZERO, |state| state.linear_velocity),
    );
    if let Ok(mut ent) = world.get_entity_mut(character) {
        ent.insert(NetSeated::default());
    }
    if let Some(mut pose) = world.get_mut::<Transform>(character) {
        pose.translation = at.translation + at.rotation * exit;
    }
    if let Some(mut base) = world.get_mut::<NetCharacterBase>(character) {
        *base = NetCharacterBase::default();
    }
    if let Some(mut state) = world.get_mut::<CharacterState>(character) {
        *state = CharacterState {
            velocity,
            mode: MovementMode::Falling,
            ..CharacterState::default()
        };
    }
    refresh_vehicle_owner(world, vehicle);
    Ok(vehicle)
}

/// The networked character a peer plays: its lowest `NetId`.
fn character_of_peer(world: &mut World, peer: PeerId) -> Option<Entity> {
    let mut q = world.query_filtered::<(Entity, &NetId, &NetOwner), With<NetCharacter>>();
    let mut rows: Vec<(NetId, Entity)> = q
        .iter(world)
        .filter(|(_, _, owner)| owner.0 == peer)
        .map(|(entity, id, _)| (*id, entity))
        .collect();
    rows.sort_unstable();
    rows.first().map(|(_, entity)| *entity)
}

fn handle_one_request(
    world: &mut World,
    peer: PeerId,
    request: &VehicleSeatRequest,
) -> Result<(), VehicleSeatError> {
    let character = character_of_peer(world, peer).ok_or(VehicleSeatError::NoCharacter)?;
    if !request.enter {
        return exit_seat(world, character).map(|_| ());
    }
    let vehicle = request
        .vehicle
        .and_then(|id| world.get_resource::<NetEntities>()?.entity(id))
        .ok_or(VehicleSeatError::NoVehicle)?;
    enter_seat(world, vehicle, character, request.seat)
}

/// Server: carries out this tick's [`VehicleSeatRequest`]s, in arrival order.
fn handle_seat_requests(world: &mut World) {
    let Some(requests) = world.get_resource::<NetMessages<VehicleSeatRequest>>() else {
        return;
    };
    let requests: Vec<(PeerId, VehicleSeatRequest)> =
        requests.iter().map(|r| (r.from, r.msg.clone())).collect();
    for (peer, request) in requests {
        let result = handle_one_request(world, peer, &request);
        let mut log = world.get_resource_or_insert_with(VehicleSeatLog::default);
        match result {
            Ok(()) => log.accepted += 1,
            Err(error) => {
                log.refused += 1;
                log.last_refusal = Some(error.code());
                tracing::info!(target: "net", peer = peer.0, code = error.code(), "net.vehicle_seat_refused");
            }
        }
    }
}

/// The set of [`net_vehicle_seats`] in the predicted schedule.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetVehicleSeatSet;

/// The predicted system that holds seated characters on their seats: after the vehicles and the
/// characters have stepped, on every side that simulates the character.
pub fn net_vehicle_seats(world: &mut World) {
    let Some(state) = world.get_resource::<PredictionState>().copied() else {
        return;
    };
    let rows: Vec<(Entity, NetId, u8)> = {
        let mut q = world.query::<(Entity, &NetSeated, Option<&NetRole>)>();
        q.iter(world)
            .filter(|(_, _, role)| state.simulates(*role))
            .filter_map(|(entity, seated, _)| seated.vehicle.map(|id| (entity, id, seated.seat)))
            .collect()
    };
    for (character, id, seat) in rows {
        let Some(vehicle) = world.get_resource::<NetEntities>().and_then(|e| e.entity(id)) else {
            continue;
        };
        let (Some(at), Some(offset)) = (
            world.get::<Transform>(vehicle).copied(),
            world
                .get::<VehicleSeats>(vehicle)
                .and_then(|seats| seats.seats.get(usize::from(seat)))
                .map(|seat| seat.offset),
        ) else {
            continue;
        };
        let position = at.translation + at.rotation * offset;
        if let Some(mut pose) = world.get_mut::<Transform>(character) {
            pose.translation = position;
            pose.rotation = at.rotation;
        }
        let (stepped_yaw, _, _) = at.rotation.to_euler(regia_core::glam::EulerRot::YXZ);
        if let Some(mut base) = world.get_mut::<NetCharacterBase>(character) {
            *base = NetCharacterBase {
                base: Some(id),
                local: offset,
                stepped_at: position,
                stepped_yaw,
            };
        }
        if let Some(mut stored) = world.get_mut::<CharacterState>(character) {
            *stored = CharacterState {
                mode: MovementMode::Custom(SEATED_MODE_ID),
                grounded: true,
                ..CharacterState::default()
            };
        }
    }
}

// ---- the input of a game with characters in cars ------------------------------------------------

/// One command for a game where a character walks and drives. One input type per game: this is the
/// one to install with [`install_net_driver`]. Both parts travel every tick, so a character that just
/// took the driver's seat has its driving input already on the way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriverNetInput {
    /// What the character would do on foot.
    pub character: CharacterNetInput,
    /// What the car would do.
    pub vehicle: VehicleNetInput,
}

impl ReplicateCodec for DriverNetInput {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        self.character.encode(w, cx);
        self.vehicle.encode(w, cx);
    }

    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self {
            character: CharacterNetInput::decode(r, cx)?,
            vehicle: VehicleNetInput::decode(r, cx)?,
        })
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, cx: &CodecContext) {
        self.character.encode_delta(&base.character, w, cx);
        self.vehicle.encode_delta(&base.vehicle, w, cx);
    }

    fn decode_delta(base: &Self, r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self {
            character: CharacterNetInput::decode_delta(&base.character, r, cx)?,
            vehicle: VehicleNetInput::decode_delta(&base.vehicle, r, cx)?,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        CharacterNetInput::layout(h);
        VehicleNetInput::layout(h);
    }
}

impl NetInput for DriverNetInput {
    fn on_missing(last: &Self) -> Self {
        Self {
            character: CharacterNetInput::on_missing(&last.character),
            vehicle: VehicleNetInput::on_missing(&last.vehicle),
        }
    }
}

impl VehicleCommand for DriverNetInput {
    fn vehicle_command(&self) -> VehicleNetInput {
        self.vehicle.quantized()
    }
}

impl CharacterCommand for DriverNetInput {
    fn character_input(&self) -> (crate::character::CharacterInput, f32) {
        self.character.character_input()
    }
}

// ---- installation -------------------------------------------------------------------------------

/// Installs networked vehicles on a world with input command `I`: what [`NetVehiclePlugin`] does for
/// [`VehicleNetInput`]. A `predict::<Transform>` made first is kept, and so is `I` already registered
/// by [`install_net_character`](crate::net_character::install_net_character).
///
/// # Errors
/// [`PredictError::InputAlreadyRegistered`]: the game registered another input type.
pub fn install_net_vehicle<I: VehicleCommand>(world: &mut World) -> Result<(), PredictError> {
    register_replicated::<NetVehicle>(world, ReplicateOptions::default());
    register_replicated::<NetVehicleState>(world, ReplicateOptions::default());
    register_replicated::<VehicleSeats>(world, ReplicateOptions::default());
    register_replicated::<NetSeated>(world, ReplicateOptions::default());
    register_net_message::<VehicleSeatRequest>(world);
    match add_net_input::<I>(world) {
        Ok(()) => {}
        Err(PredictError::InputAlreadyRegistered(name)) if name == std::any::type_name::<I>() => {}
        Err(error) => return Err(error),
    }
    let pose = predict::<Transform>(
        world,
        Tolerance::pose(TOLERANCE_POSITION_M, TOLERANCE_ROTATION_RAD),
    );
    if !matches!(pose, Ok(()) | Err(PredictError::AlreadyPredicted(_))) {
        return pose;
    }
    predict::<NetVehicleState>(world, Tolerance::Exact)?;
    predict_entity_tolerance(world, vehicle_tolerance());
    predict_local_writes::<NetVehiclePredictor>(world);
    predict_local_writes::<NetVehicleVisual>(world);
    predict_local_writes::<NetVehicleFault>(world);
    predict_local_writes::<VehicleInput>(world);
    world.init_resource::<NetVehicleTunings>();
    world.init_resource::<NetVehicleSmoothing>();
    world.init_resource::<VehicleSeatRules>();
    world.init_resource::<VehicleSeatLog>();
    world.init_resource::<VehicleDrivers>();
    add_predicted_systems(
        world,
        (
            net_vehicle_step::<I>.before(NetVehicleSeatSet),
            net_vehicle_seats.in_set(NetVehicleSeatSet),
        ),
    );
    Ok(())
}

/// Installs a game where characters walk and drive on one input command: characters first (their
/// installer refuses a registered input), then vehicles, with the seat system after both steps.
///
/// # Errors
/// As [`install_net_vehicle`].
pub fn install_net_driver<I: VehicleCommand + CharacterCommand>(
    world: &mut World,
) -> Result<(), PredictError> {
    crate::net_character::install_net_character::<I>(world)?;
    install_net_vehicle::<I>(world)?;
    world
        .resource_mut::<Schedules>()
        .configure_sets(PredictedUpdate, NetVehicleSeatSet.after(net_character_step::<I>));
    Ok(())
}

/// `FixedUpdate`, on a client: a copy of a vehicle gets a kinematic chassis in the shared world, which
/// follows its `Transform`, so characters and other bodies on this client meet the car where it is
/// shown. The prediction itself runs in the private world and does not see this body.
fn client_vehicle_bodies(world: &mut World) {
    if world
        .get_resource::<regia_net::NetMode>()
        .is_some_and(|mode| mode.is_server())
    {
        return;
    }
    let rows: Vec<(Entity, String)> = {
        let mut q = world.query_filtered::<(Entity, &NetVehicle), (
            Without<VehicleController>,
            Without<regia_physics::RigidBody>,
        )>();
        q.iter(world)
            .map(|(entity, v)| (entity, v.tuning.clone()))
            .collect()
    };
    for (entity, tuning) in rows {
        let half = world
            .get_resource::<NetVehicleTunings>()
            .and_then(|tunings| tunings.by_name.get(&tuning))
            .map(|config| config.chassis_half_extents);
        let Some(half_extents) = half else {
            note_fault(world, entity, Some("vehicle_tuning_unknown"));
            continue;
        };
        world.entity_mut(entity).insert((
            regia_physics::RigidBody::kinematic(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box { half_extents },
                ..regia_physics::Collider::default()
            },
        ));
    }
}

/// Orders the server's input system between `NetSet::Apply` and the physics step, and the predicted
/// schedule after it, and gives clients' copies their kinematic chassis. Games with their own input
/// type call this after [`install_net_vehicle`].
pub fn add_net_vehicle_systems<I: VehicleCommand>(app: &mut App) {
    app.register_type::<NetVehicle>()
        .configure_sets(
            FixedUpdate,
            (
                NetSet::Apply.before(PhysicsStep),
                NetSet::Predict.after(PhysicsStep),
            ),
        )
        .add_systems(
            FixedUpdate,
            (
                net_vehicle_server_apply::<I>
                    .after(NetSet::Apply)
                    .before(PhysicsStep),
                client_vehicle_bodies.after(NetSet::Apply).before(PhysicsStep),
            ),
        );
}

/// Networked vehicles with [`VehicleNetInput`]. Add it after `NetPlugin`, `PhysicsPlugin` and
/// `VehiclePlugin`, on every process.
pub struct NetVehiclePlugin;

impl Plugin for NetVehiclePlugin {
    fn build(&self, app: &mut App) {
        if let Err(error) = install_net_vehicle::<VehicleNetInput>(app.world_mut()) {
            tracing::error!(target: "net", %error, "net.vehicle_input_refused");
        }
        add_net_vehicle_systems::<VehicleNetInput>(app);
    }
}

/// Why [`spawn_net_vehicle`] refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NetVehicleSpawnError {
    /// [`NetVehicleTunings`] has no such tuning.
    #[error("no vehicle tuning named `{0}`")]
    UnknownTuning(String),
    /// The vehicle refused the tuning or the pose.
    #[error(transparent)]
    Vehicle(#[from] VehicleError),
}

impl NetVehicleSpawnError {
    /// A stable identifier: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownTuning(_) => "vehicle_tuning_unknown",
            Self::Vehicle(error) => error.code(),
        }
    }
}

/// Server: a replicated vehicle with a tuning from [`NetVehicleTunings`], at a pose, owned by `owner`
/// (`None`: server-driven).
///
/// # Errors
/// An unknown tuning, or what [`spawn_vehicle`] refuses. Nothing is spawned then.
pub fn spawn_net_vehicle(
    world: &mut World,
    tuning: &str,
    pose: (Vec3, Quat),
    owner: Option<PeerId>,
) -> Result<Entity, NetVehicleSpawnError> {
    let config = world
        .get_resource::<NetVehicleTunings>()
        .and_then(|tunings| tunings.by_name.get(tuning).cloned())
        .ok_or_else(|| NetVehicleSpawnError::UnknownTuning(tuning.to_string()))?;
    let entity = spawn_vehicle(world, config, pose.0, pose.1)?;
    let mut ent = world.entity_mut(entity);
    ent.insert((
        Replicated {
            transform: TransformReplication::Movement,
            ..Replicated::default()
        },
        NetVehicle {
            tuning: tuning.to_string(),
            local_player: 0,
            owner_peer: owner.map(|peer| peer.0),
        },
        NetPredicted,
    ));
    if let Some(peer) = owner {
        ent.insert(NetOwner(peer));
    }
    Ok(entity)
}

// ---- the agent surface --------------------------------------------------------------------------

/// One vehicle of [`net_vehicle_rows`].
#[derive(Debug, Clone, PartialEq)]
pub struct NetVehicleRow {
    /// The entity.
    pub entity: Entity,
    /// Its network id, once assigned.
    pub net_id: Option<NetId>,
    /// The tuning's name.
    pub tuning: String,
    /// The owning peer; `None` when the server drives it.
    pub owner: Option<u32>,
    /// `server` (this process simulates it), `predicted` (this client's own) or `interpolated`.
    pub mode: &'static str,
    /// The simulated position.
    pub position: Vec3,
    /// The drawn position: the simulated one plus the correction offset.
    pub rendered_position: Vec3,
    /// The chassis' speed, m/s.
    pub speed: f32,
    /// The size of the last correction, meters.
    pub last_correction_m: f32,
    /// Corrections this client has seen.
    pub corrections: u64,
    /// How many snapped.
    pub snaps: u64,
    /// The server's bytes per second for this entity over all connections; `None` on a client.
    pub bytes_per_second: Option<f64>,
    /// Where that comes from: `pipeline` (the relevance pipeline's own byte counters) or `codec` (the
    /// encoded size of what it sends, at the send rate). `None` on a client.
    pub bytes_source: Option<&'static str>,
    /// The engaged gear (0 with the `Simple` drivetrain).
    pub gear: i32,
    /// Engine speed, rpm (0 with the `Simple` drivetrain).
    pub rpm: f32,
    /// The character in each seat, by `NetId`.
    pub seats: Vec<Option<NetId>>,
    /// The last fault's code.
    pub fault: Option<&'static str>,
}

/// Whether [`install_net_vehicle`] ran on this world.
pub fn net_vehicles_installed(world: &World) -> bool {
    world.get_resource::<PredictionRegistry>().is_some_and(|r| {
        r.component_names()
            .contains(&std::any::type_name::<NetVehicleState>())
    })
}

/// The server's bytes per second for one vehicle and where the number comes from.
fn bytes_per_second_of(world: &World, id: NetId, entity: Entity) -> Option<(f64, &'static str)> {
    if let Some(interest) = world.get_resource::<regia_net::NetInterest>() {
        let tick = world.get_resource::<regia_net::runtime::NetNode>()?.tick;
        let second = (f64::from(tick.0) / regia_net::send::DEFAULT_TICK_HZ) as u64;
        let total: f64 = interest
            .peers
            .values()
            .filter_map(|peer| peer.send.bandwidth.by_entity.get(&id))
            .map(|window| window.per_second(second))
            .sum();
        return Some((total, "pipeline"));
    }
    let meter = world.get::<NetVehicleWireMeter>(entity)?;
    let every = world
        .get_resource::<regia_net::NetSettings>()
        .map_or(1, |settings| settings.replication.send_every_ticks);
    let peers = world.get_resource::<regia_net::runtime::NetNode>()?.peers().len();
    let tick_hz = world
        .get_resource::<FixedTime>()
        .map_or(regia_net::send::DEFAULT_TICK_HZ, |fixed| fixed.hz);
    Some((meter.per_second(tick_hz, every) * peers as f64, "codec"))
}

/// Every networked vehicle in this process, in entity order.
pub fn net_vehicle_rows(world: &mut World) -> Vec<NetVehicleRow> {
    let server = world
        .get_resource::<regia_net::NetMode>()
        .is_some_and(|mode| mode.is_server());
    let mut q = world.query::<(
        Entity,
        &NetVehicle,
        &Transform,
        &NetVehicleState,
        Option<&NetId>,
        Option<&NetOwner>,
        Option<&NetRole>,
        Option<&NetVehicleVisual>,
        Option<&VehicleSeats>,
        Option<&NetVehicleFault>,
    )>();
    let mut rows: Vec<NetVehicleRow> = q
        .iter(world)
        .map(
            |(entity, vehicle, pose, state, id, owner, role, visual, seats, fault)| NetVehicleRow {
                entity,
                net_id: id.copied(),
                tuning: vehicle.tuning.clone(),
                owner: owner.map(|o| o.0.0).or(vehicle.owner_peer),
                mode: match role {
                    Some(NetRole::Predicted) => "predicted",
                    Some(NetRole::Proxy) => "interpolated",
                    _ => "server",
                },
                position: pose.translation,
                rendered_position: visual.map_or(pose.translation, |v| v.rendered(pose).translation),
                speed: state.linear_velocity.length(),
                last_correction_m: visual.map_or(0.0, |v| v.last_correction_m),
                corrections: visual.map_or(0, |v| v.corrections),
                snaps: visual.map_or(0, |v| v.snaps),
                bytes_per_second: None,
                bytes_source: None,
                gear: state.drivetrain.as_ref().map_or(0, |d| d.gear),
                rpm: state
                    .drivetrain
                    .as_ref()
                    .map_or(0.0, |d| d.engine_speed * 60.0 / std::f32::consts::TAU),
                seats: seats.map_or_else(Vec::new, |s| s.seats.iter().map(|seat| seat.occupant).collect()),
                fault: fault.and_then(|f| f.0),
            },
        )
        .collect();
    if server {
        for row in &mut rows {
            if let Some((bytes, source)) = row
                .net_id
                .and_then(|id| bytes_per_second_of(world, id, row.entity))
            {
                row.bytes_per_second = Some(bytes);
                row.bytes_source = Some(source);
            }
        }
    }
    rows.sort_by_key(|r| r.entity);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::codec::encode_to_vec;

    /// w9-net-vehicle-steam-z 2: installing networked vehicles prepares the server's driver
    /// record up front (it was inserted lazily on the first server tick).
    #[test]
    fn install_prepares_the_driver_record() {
        let mut world = World::new();
        install_net_vehicle::<VehicleNetInput>(&mut world).expect("install");
        assert!(
            world.get_resource::<VehicleDrivers>().is_some(),
            "VehicleDrivers exists the moment the plugin is installed"
        );
    }

    /// A seat change that writes an owner also writes the driver record, and one that frees the
    /// seat clears it, so `release_departed_drivers` cannot read either as a departure.
    #[test]
    fn seat_owner_changes_keep_the_driver_record_in_step() {
        let mut world = World::new();
        let vehicle = world.spawn_empty().id();
        note_seat_owner(&mut world, vehicle, Some(PeerId(3)));
        assert_eq!(
            world.resource::<VehicleDrivers>().0.get(&vehicle),
            Some(&PeerId(3))
        );
        note_seat_owner(&mut world, vehicle, None);
        assert!(!world.resource::<VehicleDrivers>().0.contains_key(&vehicle));
    }

    fn round_trip(v: &VehicleNetInput) -> VehicleNetInput {
        let cx = CodecContext::detached();
        let bytes = encode_to_vec(v, &cx);
        let mut r = BitReader::new(&bytes);
        VehicleNetInput::decode(&mut r, &cx).expect("decode")
    }

    /// The wire value is what `quantized` gives, twice changes nothing, zero is exact, and a value
    /// that is not finite reads as released.
    #[test]
    fn input_quantization_is_the_wire_value_and_idempotent() {
        let v = VehicleNetInput {
            throttle: 0.337,
            brake: 0.5,
            steer: -1.0,
            shift: VehicleShift::Up,
        };
        let q = v.quantized();
        assert_eq!(round_trip(&v), q);
        assert_eq!(q.quantized(), q);
        assert_eq!(round_trip(&q), q);
        assert_eq!(VehicleNetInput::default().quantized().throttle, 0.0);
        let bad = VehicleNetInput {
            throttle: f32::NAN,
            brake: f32::INFINITY,
            steer: 9.0,
            shift: VehicleShift::None,
        }
        .quantized();
        assert_eq!((bad.throttle, bad.brake, bad.steer), (0.0, 0.0, 1.0));
    }

    /// Every axis value the wire can carry is a fixed point of a server's second round.
    #[test]
    fn every_wire_value_is_a_fixed_point() {
        for q in -127..=127i8 {
            let v = f32::from(q) / AXIS_STEPS;
            assert_eq!(quantize_axis(v), q);
        }
        for q in 0..=255u8 {
            let v = f32::from(q) / BRAKE_STEPS;
            assert_eq!(quantize_brake(v), q);
        }
    }

    /// The one axis pattern the encoder never writes and the one shift value are refused by name.
    #[test]
    fn malformed_commands_are_refused_with_their_codes() {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        w.write_bits(0x80, 8);
        w.write_bits(0, 8);
        w.write_bits(0, 8);
        w.write_bits(0, 2);
        let bytes = w.finish();
        assert_eq!(
            VehicleNetInput::decode(&mut BitReader::new(&bytes), &cx),
            Err(DecodeError::Invalid(CODE_INPUT_RANGE))
        );
        let mut w = BitWriter::new();
        w.write_bits(0, 8);
        w.write_bits(0, 8);
        w.write_bits(0x80, 8);
        w.write_bits(0, 2);
        let bytes = w.finish();
        assert_eq!(
            VehicleNetInput::decode(&mut BitReader::new(&bytes), &cx),
            Err(DecodeError::Invalid(CODE_INPUT_RANGE))
        );
        let mut w = BitWriter::new();
        w.write_bits(0, 24);
        w.write_bits(3, 2);
        let bytes = w.finish();
        assert_eq!(
            VehicleNetInput::decode(&mut BitReader::new(&bytes), &cx),
            Err(DecodeError::Invalid(CODE_INPUT_SHIFT))
        );
    }

    /// § 5.2: a held pedal repeats, a shift does not.
    #[test]
    fn a_missing_command_repeats_the_pedals_and_not_the_shift() {
        let last = VehicleNetInput {
            throttle: 1.0,
            steer: 0.5,
            shift: VehicleShift::Down,
            ..VehicleNetInput::default()
        };
        let next = VehicleNetInput::on_missing(&last);
        assert_eq!(next.shift, VehicleShift::None);
        assert_eq!(next.throttle, 1.0);
        assert_eq!(next.steer, 0.5);
    }

    fn sample_state(geared: bool) -> NetVehicleState {
        NetVehicleState {
            linear_velocity: Vec3::new(20.0, -0.06, -3.25),
            angular_velocity: Vec3::new(0.01, 0.42, -0.02),
            steer: 0.25,
            wheels: vec![
                NetWheelLook {
                    spin_angle: 3.0,
                    suspension_length: 0.31,
                };
                4
            ],
            drivetrain: geared.then(|| DrivetrainCore {
                engine_speed: 412.5,
                gear: 3,
                target_gear: 4,
                shift_timer: 0.125,
                dwell: 0.0,
                automatic: true,
                clutch_torque: 310.0,
                tc_factor: 1.0,
                torque_hints: [0.0, 620.5, 0.0],
                wheels: vec![
                    WheelCore {
                        angular_speed: 57.25,
                        spin_angle: 1.5,
                        slip_ratio: 0.0625,
                        tire_force: 1600.0,
                        load: 3000.0,
                        abs_factor: 1.0,
                    };
                    4
                ],
            }),
        }
    }

    fn encode_state(state: &NetVehicleState) -> Vec<u8> {
        encode_to_vec(state, &CodecContext::detached())
    }

    /// Decoding a state gives its quantized value, and writing that again gives the same bytes: a
    /// server's copy and a client's agree on what travelled.
    #[test]
    fn a_state_reads_back_quantized_and_stable() {
        for geared in [false, true] {
            let state = sample_state(geared);
            let bytes = encode_state(&state);
            let cx = CodecContext::detached();
            let read = NetVehicleState::decode(&mut BitReader::new(&bytes), &cx).expect("decode");
            assert_eq!(encode_state(&read), bytes, "a wire value is a fixed point");
            assert!(read.linear_velocity.distance(state.linear_velocity) <= 0.01);
            assert!(read.angular_velocity.distance(state.angular_velocity) <= 0.004);
            assert_eq!(read.drivetrain.is_some(), geared);
            assert!(
                pose_and_state_match((&Transform::default(), &state), (&Transform::default(), &read),),
                "the wire's errors sit inside the tolerance"
            );
            if let Some(core) = &read.drivetrain {
                assert_eq!((core.gear, core.target_gear), (3, 4));
                assert!(core.automatic);
            }
        }
    }

    /// A delta reads back the same value a full record does, and an unchanged state costs flags only.
    #[test]
    fn a_delta_reads_back_and_is_small() {
        let cx = CodecContext::detached();
        for geared in [false, true] {
            let base_full = encode_state(&sample_state(geared));
            let base = NetVehicleState::decode(&mut BitReader::new(&base_full), &cx).expect("base");
            let mut w = BitWriter::new();
            base.encode_delta(&base, &mut w, &cx);
            assert!(w.bit_len() <= 8, "{}", w.bit_len());
            let bytes = w.finish();
            assert_eq!(
                NetVehicleState::decode_delta(&base, &mut BitReader::new(&bytes), &cx).expect("delta"),
                base
            );
            let mut next = sample_state(geared);
            next.linear_velocity.x += 1.5;
            next.wheels[2].suspension_length = 0.2;
            if let Some(core) = &mut next.drivetrain {
                core.engine_speed += 30.0;
                core.wheels[1].load = 2600.0;
            }
            let mut w = BitWriter::new();
            next.encode_delta(&base, &mut w, &cx);
            let bytes = w.finish();
            let read = NetVehicleState::decode_delta(&base, &mut BitReader::new(&bytes), &cx).expect("delta");
            assert_eq!(encode_state(&read), encode_state(&next));
        }
    }

    /// Hostile lengths and a drivetrain without a base are refused, by name, and never panic.
    #[test]
    fn hostile_states_are_refused() {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        // Zero velocities at each codec's own width, so the count lands where the reader looks.
        for codec in [LINEAR, LINEAR, LINEAR, ANGULAR, ANGULAR, ANGULAR] {
            w.write_bits(0, codec.bits);
        }
        w.write_bits(0, STEER.bits);
        w.write_varint(MAX_WHEELS as u64 + 1);
        let bytes = w.finish();
        assert_eq!(
            NetVehicleState::decode(&mut BitReader::new(&bytes), &cx),
            Err(DecodeError::Invalid(CODE_STATE_WHEELS))
        );
        // Group flags off, drivetrain delta on: the base has no drivetrain to change.
        let base = sample_state(false);
        let mut w = BitWriter::new();
        w.write_bool(false);
        w.write_bool(false);
        w.write_bool(true);
        let bytes = w.finish();
        assert_eq!(
            NetVehicleState::decode_delta(&base, &mut BitReader::new(&bytes), &cx),
            Err(DecodeError::Invalid(CODE_STATE_WHEELS))
        );
    }

    /// Past the range a quantizer clamps and counts; NaN reads as zero.
    #[test]
    fn quantizers_clamp_and_count() {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        LINEAR.write(1.0e9, &mut w, &cx);
        LINEAR.write(f32::NAN, &mut w, &cx);
        assert_eq!(cx.take_stats().clamped, 2);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(LINEAR.read(&mut r).expect("max"), LINEAR.on_wire(1.0e9));
        assert_eq!(LINEAR.read(&mut r).expect("nan"), 0.0);
    }

    fn check(change: impl FnOnce(&mut Transform, &mut NetVehicleState)) -> bool {
        let (a, sa) = (Transform::default(), sample_state(true));
        let (mut b, mut sb) = (a, sa.clone());
        change(&mut b, &mut sb);
        pose_and_state_match((&a, &sa), (&b, &sb))
    }

    /// Each field's tolerance, inside and outside; looks are never compared.
    #[test]
    fn tolerance_matrix() {
        assert!(check(|_, _| {}));
        assert!(check(|t, _| t.translation.x += 0.019));
        assert!(!check(|t, _| t.translation.x += 0.021));
        assert!(check(|t, _| t.rotation = Quat::from_rotation_y(0.019)));
        assert!(!check(|t, _| t.rotation = Quat::from_rotation_y(0.03)));
        assert!(check(|_, s| s.linear_velocity.x += 0.09));
        assert!(!check(|_, s| s.linear_velocity.x += 0.11));
        assert!(check(|_, s| s.angular_velocity.y += 0.09));
        assert!(!check(|_, s| s.angular_velocity.y += 0.11));
        assert!(check(|_, s| s.wheels[0].spin_angle += 1.0), "looks never correct");
        assert!(check(|_, s| s.wheels[0].suspension_length += 0.1));
        assert!(!check(|_, s| s.drivetrain.as_mut().expect("d").gear = 2));
        assert!(!check(|_, s| s.drivetrain.as_mut().expect("d").target_gear = 2));
        assert!(!check(|_, s| s.drivetrain.as_mut().expect("d").automatic = false));
        assert!(check(|_, s| s.drivetrain.as_mut().expect("d").engine_speed += 1.5));
        assert!(!check(
            |_, s| s.drivetrain.as_mut().expect("d").engine_speed += 2.5
        ));
        assert!(!check(
            |_, s| s.drivetrain.as_mut().expect("d").shift_timer += 0.01
        ));
        assert!(!check(|_, s| s.drivetrain = None));
    }

    /// A correction moves the offset so the drawn pose stands still; payback never exceeds the limit;
    /// past the threshold it snaps and is counted.
    #[test]
    fn the_drawn_pose_pays_back_at_a_limited_rate_and_snaps() {
        let smoothing = NetVehicleSmoothing::default();
        let old = Transform::from_translation(Vec3::new(1.5, 0.0, 0.0));
        let new = Transform::from_translation(Vec3::ZERO);
        let mut visual = NetVehicleVisual::default();
        visual.take_correction(&old, &new, &smoothing);
        assert_eq!(visual.rendered(&new).translation, old.translation);
        assert_eq!((visual.corrections, visual.snaps), (1, 0));
        assert_eq!(visual.last_correction_m, 1.5);
        let mut steps = 0;
        while !visual.is_settled() {
            let before = visual.offset.length();
            visual.decay(&smoothing, 1000.0 / 60.0);
            assert!(before - visual.offset.length() <= smoothing.max_step_m + 1.0e-5);
            steps += 1;
            assert!(steps < 600, "it settles");
        }
        let far = Transform::from_translation(Vec3::new(5.0, 0.0, 0.0));
        visual.take_correction(&far, &new, &smoothing);
        assert_eq!((visual.corrections, visual.snaps), (2, 1));
        assert!(visual.is_settled());
        assert_eq!(visual.last_correction_m, 5.0);
        let turned = Transform::from_rotation(Quat::from_rotation_y(2.0));
        visual.take_correction(&turned, &new, &smoothing);
        assert_eq!(visual.snaps, 2);
        let mut rotate = NetVehicleVisual::default();
        rotate.take_correction(
            &Transform::from_rotation(Quat::from_rotation_y(0.5)),
            &new,
            &smoothing,
        );
        let before = rotate.rotation_offset.angle_between(Quat::IDENTITY);
        rotate.decay(&smoothing, 1000.0 / 60.0);
        assert!(
            before - rotate.rotation_offset.angle_between(Quat::IDENTITY) <= smoothing.max_step_rad + 1.0e-5
        );
    }

    /// A seat refusal's codes are protocol shaped; a hand-made request with no vehicle is rejected.
    #[test]
    fn seat_codes_are_protocol_shaped() {
        for error in [
            VehicleSeatError::NoVehicle,
            VehicleSeatError::NoSuchSeat(1),
            VehicleSeatError::Taken(1),
            VehicleSeatError::NoCharacter,
            VehicleSeatError::TooFar,
            VehicleSeatError::NotSeated,
        ] {
            assert!(
                error
                    .code()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{}",
                error.code()
            );
        }
    }

    /// Finding 1 (audit kimirev-fix-net1-determinism): `write_net_offset` stores the offset that
    /// `regia_net`'s `decay_offsets` decays once more, and that decay is a `quat_nlerp` since
    /// ADR-4770, not the `slerp` the old inverse composed with. The stored rotation must come
    /// back to `visual.rotation_offset` after one decay; a 120° correction (inside the smoothed
    /// range) used to render ~2° off.
    #[test]
    fn net_offset_rotation_reproduces_the_visual_offset_after_decay() {
        let mut world = World::new();
        let mut settings = regia_net::NetSettings::default();
        settings.prediction.smooth_half_life_ms = 40.0;
        world.insert_resource(settings);
        let entity = world.spawn_empty().id();

        let dt = 1.0 / 60.0;
        let keep = 0.5f32.powf(dt * 1000.0 / 40.0);
        // Exactly unit (0.25 per component): a 120° correction about (1, 1, 1).
        let rotation_offset = Quat::from_xyzw(0.5, 0.5, 0.5, 0.5);
        let visual = NetVehicleVisual {
            offset: Vec3::new(0.3, 0.0, 0.2),
            rotation_offset,
            ..NetVehicleVisual::default()
        };
        write_net_offset(&mut world, entity, &visual, dt);

        // `decay_offsets`' rotation step, emulated with plain ops: hemisphere by the w sign,
        // per-component lerp from IDENTITY, then normalize.
        let stored = world.get::<regia_net::NetVisualOffset>(entity).expect("offset");
        let end = if stored.rotation.w < 0.0 {
            -stored.rotation
        } else {
            stored.rotation
        };
        let decayed = Quat::from_xyzw(
            keep * end.x,
            keep * end.y,
            keep * end.z,
            (1.0 - keep) + keep * end.w,
        )
        .normalize();
        let chord2 = 2.0 - 2.0 * decayed.dot(rotation_offset).abs();
        assert!(
            chord2 < 1.0e-10,
            "one decay returns to the drawn rotation: chord² {chord2}"
        );
        // The drawn translation comes back exactly too: decay multiplies the stored offset/keep
        // by keep once.
        let expected = stored.translation * keep;
        assert!(
            expected.distance(visual.offset) <= 1.0e-6 * visual.offset.length().max(1.0),
            "{expected} vs {}",
            visual.offset
        );
    }
}
