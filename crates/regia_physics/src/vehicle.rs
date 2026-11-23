// SPDX-License-Identifier: MPL-2.0
// Source: rapier3d 0.36 `control::DynamicRayCastVehicleController`, `Wheel` and `WheelTuning`
// public API and documentation (Apache-2.0; rapier.rs/docs). The model is rapier's: one ray per
// wheel along its suspension, a spring-damper force per wheel scaled by the chassis mass, and
// clamped forward/side tire impulses. Box inertia I = m/3 (b² + c²) per axis is the textbook
// solid-cuboid formula. The wrapper (validation, input mapping, readout, reset, ECS system) is
// Regia Engine's own.

//! Ray-cast vehicles: a safe wrapper over rapier's `DynamicRayCastVehicleController`.
//!
//! A [`Vehicle`] is a dynamic chassis body in the [`PhysicsWorld`] plus one ray-cast wheel per
//! [`WheelConfig`]. Each fixed step, [`Vehicle::update`] casts every wheel's suspension ray,
//! applies the spring, damper, engine, brake and tire-friction impulses to the chassis, and the
//! world step then integrates them. [`Vehicle::wheel_states`] reads each wheel back for
//! rendering (chassis-space and world-space pose, steer and spin angles) and for gameplay
//! (grounded, suspension compression, contact point and normal, the ground's entity).
//!
//! # Conventions
//!
//! The engine's axes (`docs/02-architecture.md` § 3): the chassis' forward is local −Z, right
//! is +X and up is +Y. Every wheel's suspension points down the chassis' −Y and its axle along
//! +X, so a wheel mesh is modeled with its axle on +X. [`VehicleInput::steer`] is +1 for a full
//! right turn (a stick's x axis); the readout's `steer_angle` is the wheel's rotation about the
//! chassis' +Y (right-hand rule), so a right turn reads negative. A wheel rolling forward has a
//! growing `spin_angle`; its rendered rotation about +X is `-spin_angle`.
//!
//! # Units
//!
//! Meters, kilograms, seconds, newtons. The suspension constants are rapier's, which scale with
//! the chassis mass: a wheel pushes with `stiffness · compression · chassis_mass` newtons, so
//! `suspension_stiffness` is a spring rate per kilogram of chassis (1/s²). A car with `n` equal
//! wheels settles with each suspension compressed by `g / (n · stiffness)`.
//!
//! # Use
//!
//! - Bare world: [`Vehicle::spawn`] (or [`Vehicle::attach`] to an existing dynamic body), then
//!   call [`Vehicle::update`] once per fixed step before the world steps.
//! - ECS: add [`VehiclePlugin`] after `PhysicsPlugin` and create vehicles with
//!   [`spawn_vehicle`]; write the entity's [`VehicleInput`] and read its
//!   [`VehicleController`]. The plugin updates every vehicle in the `FixedUpdate` physics
//!   chain, after the velocity sync and before the step.
//!
//! # Drivetrain, anti-roll, aero, aids (ADR-2598)
//!
//! [`VehicleConfig::drivetrain`] picks the drive model. [`Drivetrain::Simple`] (the default, and
//! what every `.vehicle.ron` v1 file migrates to) is `max_engine_force` per driven wheel with
//! rapier's own forward and brake impulses, bit for bit. [`Drivetrain::Geared`] runs an engine,
//! clutch, gearbox and differentials ([`crate::vehicle_drivetrain`]): wheels have their own spin,
//! the tire's lengthwise force follows the slip ratio and the driver aids can act.
//! [`VehicleConfig::anti_roll`] and [`VehicleConfig::aero`] add chassis forces to either model.
//!
//! See `book/user-guide/vehicles.md` and `book/agents/vehicles.md`.

use rapier3d::control::{DynamicRayCastVehicleController, WheelTuning};
use rapier3d::dynamics::{MassProperties, RigidBodyActivation, RigidBodyBuilder};
use rapier3d::geometry::{ColliderBuilder, Group, InteractionGroups, InteractionTestMode};
use rapier3d::pipeline::QueryFilter;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_atan2, det_quat_from_axis_angle};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use regia_scene::streaming::{RebaseBlockers, RebaseHolds, VEHICLE_CONTACT_BLOCKER};
use thiserror::Error;
use tracing::debug_span;

use crate::components::{ColliderHandle, RigidBodyHandle};
use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::layers::PhysicsLayers;
use crate::plugin::PhysicsStep;
use crate::step::{spawn_key, step_physics, sync_velocity_and_forces};
use crate::vehicle_drivetrain::{
    AeroConfig, AidsConfig, AntiRollBar, Drivetrain, DrivetrainInput, DrivetrainState, ShiftRequest,
    WheelContact, brake_torque_in_range,
};
use crate::vehicle_tire::{SlipContact, SlipImpulse, SlipTire, TireForces, TireModel, TireState};
use crate::world::PhysicsWorld;

/// The most wheels one vehicle may have (keeps per-step work and agent reports bounded).
pub const MAX_WHEELS: usize = 32;

/// The chassis' local forward axis (engine convention: forward is −Z).
const FORWARD: Vec3 = Vec3::NEG_Z;

/// The suspension direction of every wheel in chassis space.
const SUSPENSION_DIRECTION: Vec3 = Vec3::NEG_Y;

/// The axle of every wheel in chassis space. With the contact normal up, rapier rolls a wheel
/// along `normal × axle` = `+Y × +X` = −Z, the chassis' forward.
const AXLE: Vec3 = Vec3::X;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a vehicle could not be created, updated or read.
///
/// [`VehicleError::code`] gives a stable lowercase identifier for logs and the agent protocol.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VehicleError {
    /// The configuration lists no wheels.
    #[error("vehicle config has no wheels")]
    NoWheels,
    /// The configuration lists more than [`MAX_WHEELS`] wheels.
    #[error("vehicle config has {count} wheels; the most is {MAX_WHEELS}")]
    TooManyWheels {
        /// How many wheels the configuration listed.
        count: usize,
    },
    /// A wheel field is out of range or not finite.
    #[error("wheel {index}: `{field}` is out of range or not finite")]
    InvalidWheel {
        /// The wheel's index in [`VehicleConfig::wheels`].
        index: usize,
        /// The offending [`WheelConfig`] field.
        field: &'static str,
    },
    /// A vehicle-wide field is out of range or not finite.
    #[error("vehicle config: `{field}` is out of range or not finite")]
    InvalidConfig {
        /// The offending [`VehicleConfig`] field.
        field: &'static str,
    },
    /// A spawn or reset pose has a non-finite translation or a degenerate rotation.
    #[error("vehicle pose must be a finite translation and a non-zero finite rotation")]
    InvalidPose,
    /// The time step is zero, negative or not finite.
    #[error("vehicle time step must be finite and positive")]
    InvalidTimeStep,
    /// The chassis body is no longer in the physics world (removed or despawned).
    #[error("the vehicle's chassis body is not in the physics world")]
    ChassisMissing,
    /// The chassis body is fixed or kinematic: wheel impulses would not move it.
    #[error("the vehicle's chassis body is not dynamic")]
    ChassisNotDynamic,
    /// The chassis body has no mass (no collider with mass, no additional mass): rapier's tire
    /// impulses divide by it.
    #[error("the vehicle's chassis body has no mass")]
    ChassisMassless,
    /// The app has no `PhysicsWorld` resource (add `PhysicsPlugin`).
    #[error("no PhysicsWorld in this app; add regia_physics::PhysicsPlugin")]
    NoPhysicsWorld,
    /// A gear change was asked of a vehicle with the `Simple` drivetrain.
    #[error("the vehicle has the Simple drivetrain: it has no gearbox")]
    NoGearbox,
    /// A gear change named a gear the gearbox does not have.
    #[error("the gearbox has no gear {gear} (-1 is reverse, 0 neutral, 1 and up forward)")]
    InvalidGear {
        /// The gear asked for.
        gear: i32,
    },
    /// The vehicle's own state (chassis velocity, wheel or drivetrain state) is not finite, so it
    /// cannot be carried into a rebuilt world (`VehicleSimWorld::sync_statics`, ADR-7417).
    #[error("the vehicle's state is not finite")]
    StateNotFinite,
}

impl VehicleError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoWheels => "vehicle_no_wheels",
            Self::TooManyWheels { .. } => "vehicle_too_many_wheels",
            Self::InvalidWheel { .. } => "vehicle_invalid_wheel",
            Self::InvalidConfig { .. } => "vehicle_invalid_config",
            Self::InvalidPose => "vehicle_invalid_pose",
            Self::InvalidTimeStep => "vehicle_invalid_time_step",
            Self::ChassisMissing => "vehicle_chassis_missing",
            Self::ChassisNotDynamic => "vehicle_chassis_not_dynamic",
            Self::ChassisMassless => "vehicle_chassis_massless",
            Self::NoPhysicsWorld => "vehicle_no_physics_world",
            Self::NoGearbox => "vehicle_no_gearbox",
            Self::InvalidGear { .. } => "vehicle_invalid_gear",
            Self::StateNotFinite => "vehicle_state_not_finite",
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One wheel: where it hangs, its size, its suspension and tire, and what it does.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WheelConfig {
    /// The suspension's top (hard point) in chassis space, meters. The wheel hangs below it
    /// along the chassis' −Y. It may sit inside the chassis collider: the wheel rays ignore the
    /// chassis.
    pub position: Vec3,
    /// Wheel radius, meters (> 0).
    pub radius: f32,
    /// Suspension length at rest, meters (≥ 0). The ray reaches `rest length + radius` below
    /// the hard point; a wheel finds no ground beyond that.
    pub suspension_rest_length: f32,
    /// How far the suspension may compress (and extend) from its rest length, meters (≥ 0).
    pub suspension_travel: f32,
    /// Spring rate per kilogram of chassis mass (1/s², ≥ 0): the wheel pushes with
    /// `stiffness · compression · chassis_mass` newtons.
    pub suspension_stiffness: f32,
    /// Damping while the suspension compresses, per kilogram of chassis (1/s, ≥ 0).
    pub suspension_compression_damping: f32,
    /// Damping while the suspension extends, per kilogram of chassis (1/s, ≥ 0). Raise it if
    /// the car bounces.
    pub suspension_relaxation_damping: f32,
    /// The most force one suspension may apply, newtons (≥ 0).
    pub max_suspension_force: f32,
    /// Tire grip: the most tire impulse per step is `suspension force · dt · friction_slip`.
    /// Larger values brake and corner harder (and can flip the car). ≥ 0.
    pub friction_slip: f32,
    /// Multiplier on the sideways (cornering) tire impulse (≥ 0; 1 = rapier's model).
    pub side_friction_stiffness: f32,
    /// The wheel turns with [`VehicleInput::steer`].
    pub steer: bool,
    /// The engine drives this wheel with [`VehicleInput::throttle`].
    pub drive: bool,
    /// The wheel brakes with [`VehicleInput::brake`].
    pub brake: bool,
    /// How the tire makes its sideways force: [`TireModel::Damper`] (rapier's, the default) or
    /// [`TireModel::Slip`] (a slip-angle curve, ADR-2719). `.vehicle.ron` v3.
    pub tire_model: TireModel,
}

impl Default for WheelConfig {
    /// A 0.35 m road wheel on a 0.35 m suspension tuned for four wheels, brake only.
    fn default() -> Self {
        Self {
            position: Vec3::new(0.0, -0.2, 0.0),
            radius: 0.35,
            suspension_rest_length: 0.35,
            suspension_travel: 0.25,
            suspension_stiffness: 30.0,
            suspension_compression_damping: 2.3,
            suspension_relaxation_damping: 3.3,
            max_suspension_force: 50_000.0,
            friction_slip: 10.5,
            side_friction_stiffness: 1.0,
            steer: false,
            drive: false,
            brake: true,
            tire_model: TireModel::Damper,
        }
    }
}

/// A vehicle: its chassis box, mass, drive limits and wheels.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VehicleConfig {
    /// Half extents of the chassis collider box, meters (each > 0). Used by
    /// [`Vehicle::spawn`]; [`Vehicle::attach`] keeps the body's own colliders.
    pub chassis_half_extents: Vec3,
    /// Chassis mass, kilograms (> 0). Used by [`Vehicle::spawn`].
    pub chassis_mass: f32,
    /// Center of mass in chassis space, meters. Lower it to keep the car on its wheels in
    /// hard turns. Used by [`Vehicle::spawn`].
    pub center_of_mass: Vec3,
    /// Engine force per driven wheel at full throttle, newtons (≥ 0).
    pub max_engine_force: f32,
    /// Brake force per braking wheel at full brake, newtons (≥ 0).
    pub max_brake_force: f32,
    /// Rolling resistance per wheel that is neither driving nor braking, newtons (≥ 0), so a
    /// coasting car slows down. 0 lets it roll forever on flat ground.
    pub rolling_resistance: f32,
    /// Steering angle of the steered wheels at full lock, radians, in `[0, π/2)`.
    pub max_steer_angle: f32,
    /// The wheels, 1 to [`MAX_WHEELS`]. Their order is the readout order.
    pub wheels: Vec<WheelConfig>,
    /// Layers the wheel rays can hit, a bitmask of layer indices (`1 << index`); `None` (the
    /// default) hits every layer. With [`Self::query_layer`] set, the mask is intersected with
    /// that layer's query row of the [`PhysicsLayers`] matrix.
    pub ground_layers: Option<u32>,
    /// The layer the wheel rays belong to (a row of the [`PhysicsLayers`] query matrix), or
    /// `None` for rays that belong to no layer. Needs the layer matrix: [`Vehicle::update_with_layers`]
    /// or [`VehiclePlugin`], which reads the `PhysicsLayers` resource.
    pub query_layer: Option<u16>,
    /// The drive model: [`Drivetrain::Simple`] by default.
    pub drivetrain: Drivetrain,
    /// Anti-roll bars, usually one per axle; none by default.
    pub anti_roll: Vec<AntiRollBar>,
    /// Aerodynamic drag and downforce; `None` (the default) has no air.
    pub aero: Option<AeroConfig>,
    /// Driver aids: both off by default. They need [`Drivetrain::Geared`].
    pub aids: AidsConfig,
}

impl Default for VehicleConfig {
    /// A 1,200 kg four-wheel car (1.8 m × 0.7 m × 4.2 m box, 2.6 m wheelbase, 1.6 m track):
    /// front-wheel steering, rear-wheel drive, brakes on all four. Wheels 0 and 1 are the front
    /// left and right, 2 and 3 the rear left and right.
    fn default() -> Self {
        let wheel = |x: f32, z: f32, front: bool| WheelConfig {
            position: Vec3::new(x, -0.2, z),
            steer: front,
            drive: !front,
            brake: true,
            ..WheelConfig::default()
        };
        Self {
            chassis_half_extents: Vec3::new(0.9, 0.35, 2.1),
            chassis_mass: 1200.0,
            center_of_mass: Vec3::new(0.0, -0.3, 0.0),
            max_engine_force: 3000.0,
            max_brake_force: 3000.0,
            rolling_resistance: 40.0,
            max_steer_angle: 0.5,
            wheels: vec![
                wheel(-0.8, -1.3, true),
                wheel(0.8, -1.3, true),
                wheel(-0.8, 1.3, false),
                wheel(0.8, 1.3, false),
            ],
            ground_layers: None,
            query_layer: None,
            drivetrain: Drivetrain::Simple,
            anti_roll: Vec::new(),
            aero: None,
            aids: AidsConfig::default(),
        }
    }
}

/// The largest value a wheel setting of [`WheelConfig`] may take (forces in newtons, stiffnesses
/// in newtons per meter, lengths in meters): far beyond any vehicle, and low enough that the
/// step's products of settings stay finite in `f32`. A larger value is refused with
/// `vehicle_invalid_wheel` naming the field (ADR-7431); the drivetrain, anti-roll and aero
/// sections already used the same limit. The vehicle-wide fields of the `Simple` drivetrain keep
/// their v1 rules (finite, in range).
pub const MAX_VEHICLE_SETTING: f32 = 1.0e9;

/// `value` is finite and ≥ 0 (the v1 rule of the vehicle-wide fields).
fn non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

/// A wheel setting: ≥ 0 and at most [`MAX_VEHICLE_SETTING`] (never NaN).
fn wheel_setting(value: f32) -> bool {
    (0.0..=MAX_VEHICLE_SETTING).contains(&value)
}

/// A positive wheel setting: > 0 and at most [`MAX_VEHICLE_SETTING`].
fn positive_wheel_setting(value: f32) -> bool {
    value > 0.0 && value <= MAX_VEHICLE_SETTING
}

/// `value` is finite and > 0.
fn positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

impl WheelConfig {
    /// The first field that is out of range, if any.
    fn invalid_field(&self) -> Option<&'static str> {
        let checks: [(bool, &'static str); 10] = [
            (self.position.is_finite(), "position"),
            (positive_wheel_setting(self.radius), "radius"),
            (
                wheel_setting(self.suspension_rest_length),
                "suspension_rest_length",
            ),
            (wheel_setting(self.suspension_travel), "suspension_travel"),
            (wheel_setting(self.suspension_stiffness), "suspension_stiffness"),
            (
                wheel_setting(self.suspension_compression_damping),
                "suspension_compression_damping",
            ),
            (
                wheel_setting(self.suspension_relaxation_damping),
                "suspension_relaxation_damping",
            ),
            (wheel_setting(self.max_suspension_force), "max_suspension_force"),
            (wheel_setting(self.friction_slip), "friction_slip"),
            (
                wheel_setting(self.side_friction_stiffness),
                "side_friction_stiffness",
            ),
        ];
        checks
            .into_iter()
            .find(|(ok, _)| !ok)
            .map(|(_, field)| field)
            .or_else(|| self.tire_model.slip().and_then(SlipTire::invalid_field))
    }

    /// The rapier tuning for this wheel. A slip tire turns rapier's side force off: Regia applies
    /// its own (ADR-2719).
    fn tuning(&self) -> WheelTuning {
        WheelTuning {
            suspension_stiffness: self.suspension_stiffness,
            suspension_compression: self.suspension_compression_damping,
            suspension_damping: self.suspension_relaxation_damping,
            max_suspension_travel: self.suspension_travel,
            side_friction_stiffness: match self.tire_model {
                TireModel::Damper => self.side_friction_stiffness,
                TireModel::Slip(_) => 0.0,
            },
            friction_slip: self.friction_slip,
            max_suspension_force: self.max_suspension_force,
        }
    }
}

impl VehicleConfig {
    /// Checks every field: `Ok` when [`Vehicle::spawn`] would accept the configuration.
    ///
    /// Vehicle-wide fields are checked before the wheels, wheels in order, fields in
    /// declaration order; the first problem is reported.
    pub fn validate(&self) -> Result<(), VehicleError> {
        let half = self.chassis_half_extents;
        let checks: [(bool, &'static str); 7] = [
            (
                half.is_finite() && half.min_element() > 0.0,
                "chassis_half_extents",
            ),
            (positive(self.chassis_mass), "chassis_mass"),
            (self.center_of_mass.is_finite(), "center_of_mass"),
            (non_negative(self.max_engine_force), "max_engine_force"),
            (non_negative(self.max_brake_force), "max_brake_force"),
            (non_negative(self.rolling_resistance), "rolling_resistance"),
            (
                non_negative(self.max_steer_angle) && self.max_steer_angle < std::f32::consts::FRAC_PI_2,
                "max_steer_angle",
            ),
        ];
        if let Some((_, field)) = checks.into_iter().find(|(ok, _)| !ok) {
            return Err(VehicleError::InvalidConfig { field });
        }
        if self
            .query_layer
            .is_some_and(|layer| usize::from(layer) >= crate::layers::MAX_LAYERS)
        {
            return Err(VehicleError::InvalidConfig { field: "query_layer" });
        }
        if self.wheels.is_empty() {
            return Err(VehicleError::NoWheels);
        }
        if self.wheels.len() > MAX_WHEELS {
            return Err(VehicleError::TooManyWheels {
                count: self.wheels.len(),
            });
        }
        for (index, wheel) in self.wheels.iter().enumerate() {
            if let Some(field) = wheel.invalid_field() {
                return Err(VehicleError::InvalidWheel { index, field });
            }
        }
        self.validate_chassis_systems()
    }

    /// Checks the drivetrain, anti-roll, aero and aids sections.
    fn validate_chassis_systems(&self) -> Result<(), VehicleError> {
        let wheel_count = self.wheels.len();
        if let Drivetrain::Geared(drivetrain) = &self.drivetrain {
            if let Some(field) = drivetrain.invalid_field(wheel_count) {
                return Err(VehicleError::InvalidConfig { field });
            }
            let brake_ok = |wheel: &WheelConfig| brake_torque_in_range(self.max_brake_force * wheel.radius);
            if !self.wheels.iter().all(brake_ok) {
                return Err(VehicleError::InvalidConfig {
                    field: "max_brake_force",
                });
            }
        }
        if self.anti_roll.iter().any(|bar| !bar.is_valid(wheel_count)) {
            return Err(VehicleError::InvalidConfig { field: "anti_roll" });
        }
        if let Some(field) = self.aero.as_ref().and_then(AeroConfig::invalid_field) {
            return Err(VehicleError::InvalidConfig { field });
        }
        if let Some(field) = self.aids.invalid_field() {
            return Err(VehicleError::InvalidConfig { field });
        }
        if self.aids.any() && self.drivetrain == Drivetrain::Simple {
            return Err(VehicleError::InvalidConfig { field: "aids" });
        }
        Ok(())
    }

    /// Some wheel has a [`TireModel::Slip`] tire.
    #[must_use]
    pub fn has_slip_tires(&self) -> bool {
        self.wheels.iter().any(|wheel| wheel.tire_model.slip().is_some())
    }

    /// A 64-bit FNV-1a digest of the configuration's compact RON text: two configurations with the
    /// same digest drive alike. `physics.vehicles` reports it, so a tool can tell which configuration
    /// a running vehicle has (ADR-9760).
    #[must_use]
    pub fn digest(&self) -> u64 {
        let text = ron::ser::to_string(self).unwrap_or_default();
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Fresh slip-tire states: one per wheel when any wheel has a slip tire, none otherwise.
    fn tire_states(&self) -> Vec<TireState> {
        if self.has_slip_tires() {
            vec![TireState::default(); self.wheels.len()]
        } else {
            Vec::new()
        }
    }

    /// A fresh drivetrain state for the geared model; `None` for [`Drivetrain::Simple`].
    fn drivetrain_state(&self) -> Option<Box<DrivetrainState>> {
        match &self.drivetrain {
            Drivetrain::Simple => None,
            Drivetrain::Geared(drivetrain) => Some(Box::new(DrivetrainState::new(
                drivetrain,
                &self.wheels,
                self.max_brake_force,
            ))),
        }
    }
}

/// The interaction groups a downward ray (a wheel's or a hover thruster's) filters by: the
/// `ground_layers` mask intersected with the `query_layer` row of the matrix when both exist;
/// `None` when nothing restricts the ray.
pub(crate) fn ray_groups(
    ground_layers: Option<u32>,
    query_layer: Option<u16>,
    layers: Option<&PhysicsLayers>,
) -> Option<InteractionGroups> {
    let row = match (query_layer, layers) {
        (Some(layer), Some(layers)) => {
            Some(layers.query_rows[usize::from(layer) % crate::layers::MAX_LAYERS])
        }
        _ => None,
    };
    let mask = match (ground_layers, row) {
        (Some(mask), Some(row)) => mask & row,
        (Some(mask), None) => mask,
        (None, Some(row)) => row,
        (None, None) => return None,
    };
    Some(InteractionGroups::new(
        Group::all(),
        Group::from_bits_retain(mask),
        InteractionTestMode::And,
    ))
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Driver input for one step. Also a component: [`VehiclePlugin`] reads it from the vehicle's
/// entity every fixed step.
#[derive(
    Component, Reflect, Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize,
)]
#[reflect(Component, Default)]
pub struct VehicleInput {
    /// Engine: +1 full forward, −1 full reverse, 0 off.
    pub throttle: f32,
    /// Brake: 0 off to 1 full. On a wheel that can brake, braking cancels its engine force.
    pub brake: f32,
    /// Steering: +1 full right, −1 full left, 0 straight.
    pub steer: f32,
}

impl VehicleInput {
    /// The input clamped to its ranges, with non-finite values read as 0 (released).
    pub fn sanitized(self) -> Self {
        let clean = |value: f32, lo: f32, hi: f32| {
            if value.is_finite() {
                value.clamp(lo, hi)
            } else {
                0.0
            }
        };
        Self {
            throttle: clean(self.throttle, -1.0, 1.0),
            brake: clean(self.brake, 0.0, 1.0),
            steer: clean(self.steer, -1.0, 1.0),
        }
    }
}

// ---------------------------------------------------------------------------
// Readout
// ---------------------------------------------------------------------------

/// One wheel after the last [`Vehicle::update`], posed against the chassis' current pose.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelState {
    /// The wheel's index in [`VehicleConfig::wheels`].
    pub index: usize,
    /// Wheel center in chassis space: the hard point moved down by `suspension_length`.
    pub local_translation: Vec3,
    /// Wheel rotation in chassis space: steer about +Y, then spin about +X.
    pub local_rotation: Quat,
    /// Wheel center in world space.
    pub world_translation: Vec3,
    /// Wheel rotation in world space (chassis rotation × `local_rotation`).
    pub world_rotation: Quat,
    /// Steering angle about the chassis' +Y, radians (positive turns the wheel left).
    pub steer_angle: f32,
    /// Accumulated rolling angle, radians (grows while rolling forward).
    pub spin_angle: f32,
    /// Current suspension length, meters (the rest length when the wheel is in the air).
    pub suspension_length: f32,
    /// `rest length − suspension_length`, meters: positive when compressed, 0 in the air.
    pub compression: f32,
    /// The wheel's ray hit ground within `rest length + radius` at the last update.
    pub grounded: bool,
    /// Where the wheel touches the ground (world space), if grounded.
    pub contact_point: Option<Vec3>,
    /// The ground's normal at the contact (world space), if grounded.
    pub contact_normal: Option<Vec3>,
    /// The ground collider under the wheel, if grounded.
    pub ground_collider: Option<rapier3d::geometry::ColliderHandle>,
    /// The ECS entity that owns the ground collider, if grounded on an entity's collider.
    pub ground_entity: Option<Entity>,
    /// The suspension force the wheel applied at the last update, newtons (0 in the air).
    pub suspension_force: f32,
    /// The slip ratio at the last update (Pacejka): `(ω·r − v) / max(|v|, 1 m/s)`, positive
    /// driving, negative braking. Always 0 with the `Simple` drivetrain, whose wheels roll with the
    /// chassis, and in the air.
    pub slip_ratio: f32,
    /// The slip angle now (Pacejka), radians: `atan(v_side / |v_along|)` at the contact against the
    /// ground, positive when the contact moves toward the wheel's right. 0 in the air or
    /// below 0.1 m/s.
    pub slip_angle: f32,
}

/// The chassis and its wheels.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleState {
    /// Chassis body origin, world space.
    pub translation: Vec3,
    /// Chassis rotation, world space.
    pub rotation: Quat,
    /// The chassis' forward axis (local −Z) in world space.
    pub forward: Vec3,
    /// Chassis linear velocity (at the center of mass), m/s, as rapier leaves it after the
    /// step. The suspension impulse lands once at the start of a step while gravity acts over
    /// the solver's substeps, so a car resting on its springs reads a small downward vertical
    /// velocity (about −0.06 m/s at 60 Hz with the default 4 solver iterations) while its
    /// position holds still. `forward_speed` is not affected.
    pub linear_velocity: Vec3,
    /// Chassis angular velocity, rad/s.
    pub angular_velocity: Vec3,
    /// Speed along `forward`, m/s (negative when reversing).
    pub forward_speed: f32,
    /// Wheels on the ground at the last update.
    pub grounded_wheels: usize,
    /// Every wheel, in configuration order.
    pub wheels: Vec<WheelState>,
    /// Engine speed, rpm; 0 with the `Simple` drivetrain.
    pub rpm: f32,
    /// The engaged gear: −1 reverse, 0 neutral, 1 and up forward; 0 with the `Simple` drivetrain.
    pub gear: i32,
    /// The chassis' speed in its own plane, m/s, never negative (the vertical bob is left out).
    pub speed: f32,
}

// ---------------------------------------------------------------------------
// Vehicle
// ---------------------------------------------------------------------------

/// A ray-cast vehicle in a [`PhysicsWorld`]: a dynamic chassis body and its wheels.
///
/// Never panics on a bad world: a chassis that was removed reports
/// [`VehicleError::ChassisMissing`] from every call.
#[derive(Debug, Clone)]
pub struct Vehicle {
    /// rapier's controller (wheel state, impulses).
    controller: DynamicRayCastVehicleController,
    /// The validated configuration.
    config: VehicleConfig,
    /// The chassis body.
    chassis: RigidBodyHandle,
    /// The chassis collider [`Vehicle::spawn`] created (`None` when attached).
    chassis_collider: Option<rapier3d::geometry::ColliderHandle>,
    /// The pose the chassis was spawned or attached at (for [`Vehicle::reset_to_spawn`]).
    spawn_pose: (Vec3, Quat),
    /// The last input applied (sanitized).
    input: VehicleInput,
    /// Updates since spawn, attach or reset.
    steps: u64,
    /// The geared drivetrain's live state; `None` for [`Drivetrain::Simple`].
    drivetrain: Option<Box<DrivetrainState>>,
    /// Floating-origin shift since the last update: rapier keeps the wheels' world-space contact
    /// record until the next ray cast, so the readout moves it by this much (ADR-2435).
    contact_shift: Vec3,
    /// Slip-tire states, one per wheel; empty when no wheel has a slip tire (ADR-2719).
    tires: Vec<TireState>,
    /// The time step of the last update (seconds; 0 before the first).
    last_dt: f32,
    /// [`Vehicle::set_config`] calls since spawn or attach (ADR-9760).
    config_revision: u64,
}

/// What [`Vehicle::set_config`] carried over and what it clamped (ADR-9760).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ConfigChange {
    /// The chassis box, mass, center of mass and inertia changed on a spawned vehicle's body.
    pub chassis_updated: bool,
    /// The wheel count held, so every wheel kept its live record.
    pub wheels_kept: bool,
    /// The geared drivetrain's live state carried over (else it started fresh or there is none).
    pub drivetrain_kept: bool,
    /// The engaged or target gear was above the new top gear and was clamped to it.
    pub gear_clamped: bool,
    /// The engine speed was outside the new idle to redline range and was clamped into it.
    pub rpm_clamped: bool,
    /// How many slip tires kept their state.
    pub tires_kept: usize,
}

/// A validated pose: finite translation, finite non-zero rotation (normalized).
pub(crate) fn checked_pose(translation: Vec3, rotation: Quat) -> Result<(Vec3, Quat), VehicleError> {
    if !translation.is_finite() || !rotation.is_finite() || rotation.length_squared() < 1.0e-12 {
        return Err(VehicleError::InvalidPose);
    }
    Ok((translation, rotation.normalize()))
}

/// A fresh rapier controller for `chassis` with one wheel per configured wheel.
fn build_controller(
    chassis: rapier3d::dynamics::RigidBodyHandle,
    config: &VehicleConfig,
) -> DynamicRayCastVehicleController {
    let mut controller = DynamicRayCastVehicleController::new(chassis);
    // Forward is −Z; rapier's index axes are positive, so its own speed sign is not used (the
    // readout projects on −Z itself). Up is +Y.
    controller.index_up_axis = 1;
    controller.index_forward_axis = 2;
    for wheel in &config.wheels {
        controller.add_wheel(
            to_rapier_vec3(wheel.position),
            to_rapier_vec3(SUSPENSION_DIRECTION),
            to_rapier_vec3(AXLE),
            wheel.suspension_rest_length,
            wheel.radius,
            &wheel.tuning(),
        );
    }
    controller
}

/// The ground's velocity at `point` when it is a moving body; zero for static ground.
fn ground_velocity(
    bodies: &rapier3d::dynamics::RigidBodySet,
    colliders: &rapier3d::geometry::ColliderSet,
    ground: Option<rapier3d::geometry::ColliderHandle>,
    point: rapier3d::math::Vector,
) -> Vec3 {
    ground
        .and_then(|handle| colliders.get(handle))
        .and_then(|collider| collider.parent())
        .and_then(|parent| bodies.get(parent))
        .map_or(Vec3::ZERO, |body| from_rapier_vec3(body.velocity_at_point(point)))
}

/// A steered wheel's axle in chassis space. The deterministic rotation keeps the slip solver's
/// inputs bit-stable across platform libms (ADR-4773).
fn local_rotation_axle(steer_angle: f32) -> Vec3 {
    det_quat_from_axis_angle(Vec3::Y, steer_angle) * AXLE
}

/// The slip angle of a contact moving at `velocity` against the ground (Pacejka): the angle
/// between the wheel's rolling direction and the velocity, positive toward the wheel's right.
fn contact_slip_angle(velocity: Vec3, normal: Vec3, axle: Vec3) -> f32 {
    let axle = (axle - normal * axle.dot(normal)).normalize_or_zero();
    let rolling = normal.cross(axle).normalize_or_zero();
    let along = velocity.dot(rolling);
    let side = velocity.dot(axle);
    if along * along + side * side < 0.01 {
        return 0.0;
    }
    det_atan2(side, along.abs())
}

/// The mean mount point of the front (forward of the chassis center, −Z) or the rear wheels; the
/// center when that end has no wheel.
fn wheel_center(wheels: &[WheelConfig], front: bool) -> Vec3 {
    let (mut sum, mut count) = (Vec3::ZERO, 0.0_f32);
    for wheel in wheels.iter().filter(|wheel| (wheel.position.z < 0.0) == front) {
        sum += wheel.position;
        count += 1.0;
    }
    if count > 0.0 { sum / count } else { Vec3::ZERO }
}

/// Principal inertia of a solid box with the given half extents and mass, about its center.
fn box_inertia(half: Vec3, mass: f32) -> Vec3 {
    let sq = half * half;
    Vec3::new(sq.y + sq.z, sq.x + sq.z, sq.x + sq.y) * (mass / 3.0)
}

/// The inverse of the mass a body shows to an impulse at `point` along the unit `direction`
/// (rapier's own formula: `n·M⁻¹n + n·((I⁻¹(r × n)) × r)`); 0 for a body that is not dynamic.
fn inverse_mass_at(
    body: &rapier3d::dynamics::RigidBody,
    point: rapier3d::math::Vector,
    direction: rapier3d::math::Vector,
) -> f32 {
    if !body.is_dynamic() {
        return 0.0;
    }
    let props = body.mass_properties();
    let arm = point - props.world_com;
    let turn = props.effective_world_inv_inertia * arm.cross(direction);
    direction.dot(props.effective_inv_mass * direction) + direction.dot(turn.cross(arm))
}

/// The dynamic body that owns the ground collider, if it is one.
fn dynamic_ground(
    bodies: &rapier3d::dynamics::RigidBodySet,
    colliders: &rapier3d::geometry::ColliderSet,
    ground: Option<rapier3d::geometry::ColliderHandle>,
) -> Option<rapier3d::dynamics::RigidBodyHandle> {
    ground
        .and_then(|handle| colliders.get(handle))
        .and_then(|collider| collider.parent())
        .filter(|parent| bodies.get(*parent).is_some_and(|body| body.is_dynamic()))
}

/// What the slip-tire pass reads.
struct SlipPass<'a> {
    config: &'a VehicleConfig,
    wheels: &'a [rapier3d::control::Wheel],
    geared: Option<(
        &'a crate::vehicle_drivetrain::DrivetrainConfig,
        &'a DrivetrainState,
    )>,
    chassis: rapier3d::dynamics::RigidBodyHandle,
    input: VehicleInput,
    dt: f32,
    bar_force: &'a [f32; MAX_WHEELS],
}

/// One slip tire's impulses for the step, gathered before any is applied.
#[derive(Debug, Clone, Copy, Default)]
struct SlipPush {
    /// The wheel has a slip tire on the ground this step.
    active: bool,
    contact: rapier3d::math::Vector,
    /// Where the side impulse acts on the chassis (`roll_influence`).
    side_point: rapier3d::math::Vector,
    side: rapier3d::math::Vector,
    along: rapier3d::math::Vector,
    /// Everything this wheel pushed into the chassis this step, for the ground's reaction.
    total: rapier3d::math::Vector,
    ground: Option<rapier3d::dynamics::RigidBodyHandle>,
}

/// Slip tires (ADR-2719): measures every slip wheel's contact after rapier's pass and the
/// drivetrain's impulse, solves each tire, then applies the side and hold impulses to the chassis
/// and, on a dynamic ground body, the opposite of every impulse the wheel gave the chassis. The
/// contacts are measured together and applied together, so wheel order does not matter.
fn apply_slip_tires(
    pass: SlipPass<'_>,
    tires: &mut [TireState],
    bodies: &mut rapier3d::dynamics::RigidBodySet,
    colliders: &rapier3d::geometry::ColliderSet,
) {
    let _span = debug_span!("vehicle_slip_tires").entered();
    let SlipPass {
        config,
        wheels,
        geared,
        chassis,
        input,
        dt,
        bar_force,
    } = pass;
    let Some(body) = bodies.get(chassis) else {
        return;
    };
    let count = config
        .wheels
        .len()
        .min(MAX_WHEELS)
        .min(wheels.len())
        .min(tires.len());
    let up = body.position().rotation * to_rapier_vec3(Vec3::Y);
    let com = body.mass_properties().world_com;
    let grounded = wheels
        .iter()
        .take(count)
        .filter(|wheel| wheel.raycast_info().is_in_contact)
        .count()
        .max(1) as f32;
    let mut pushes = [SlipPush::default(); MAX_WHEELS];
    let rows = wheels.iter().zip(&config.wheels).zip(tires.iter_mut());
    for (index, ((wheel, wheel_config), state)) in rows.enumerate().take(count) {
        let Some(tire) = wheel_config.tire_model.slip() else {
            continue;
        };
        let info = wheel.raycast_info();
        if !info.is_in_contact {
            state.release();
            continue;
        }
        let normal = info.contact_normal_ws;
        let axle = wheel.axle();
        // The wheel's lean against the ground, for the camber thrust (ADR-3276).
        let camber = crate::vehicle_tire::contact_camber(
            tire.static_camber,
            wheel_config.position.x,
            axle.dot(normal),
        );
        let axle = (axle - normal * axle.dot(normal)).normalize_or_zero();
        let rolling = normal.cross(axle).normalize_or_zero();
        let point = info.contact_point_ws;
        let ground = dynamic_ground(bodies, colliders, info.ground_object);
        // The stick anchor lives in the frame of the ground's body, if it has one.
        let ground_body = info
            .ground_object
            .and_then(|handle| colliders.get(handle))
            .and_then(|collider| collider.parent())
            .filter(|parent| bodies.contains(*parent));
        if state.anchor.is_some() && state.anchor_body != ground_body {
            state.anchor = None;
        }
        let to_world = |local: rapier3d::math::Vector| {
            ground_body
                .and_then(|handle| bodies.get(handle))
                .map_or(local, |ground| ground.position().transform_point(local))
        };
        let anchor = state.anchor.map(to_world);
        let offset = anchor.map_or(rapier3d::math::Vector::ZERO, |anchor| point - anchor);
        let relative = from_rapier_vec3(body.velocity_at_point(point))
            - ground_velocity(bodies, colliders, info.ground_object, point);
        let ground_inverse = |direction| {
            ground
                .and_then(|handle| bodies.get(handle))
                .map_or(0.0, |ground| inverse_mass_at(ground, point, direction))
        };
        let mass_share = |direction| {
            let inverse = inverse_mass_at(body, point, direction) + ground_inverse(direction);
            if inverse > 0.0 && inverse.is_finite() {
                1.0 / (inverse * grounded)
            } else {
                0.0
            }
        };
        let suspension = wheel
            .wheel_suspension_force
            .min(wheel_config.max_suspension_force);
        let spin = geared.and_then(|(_, state)| state.wheels().get(index));
        let drivetrain_impulse = spin.map_or(0.0, |spin| {
            if spin.tire_impulse.is_finite() {
                spin.tire_impulse
            } else {
                0.0
            }
        });
        let rapier_impulse = wheel.forward_impulse;
        let locked = spin.is_none_or(|spin| spin.angular_speed == 0.0);
        let hold_force = if wheel_config.brake && input.brake > 0.0 && locked {
            config.max_brake_force * input.brake
        } else {
            0.0
        };
        let contact = SlipContact {
            along_speed: from_rapier_vec3(rolling).dot(relative),
            side_speed: from_rapier_vec3(axle).dot(relative),
            load: (suspension + bar_force[index]).max(0.0),
            longitudinal_impulse: drivetrain_impulse + rapier_impulse,
            longitudinal_friction: geared.map_or(tire.peak_friction, |(drivetrain, _)| {
                drivetrain.tire.peak_friction
            }),
            hold_force,
            side_mass: mass_share(axle),
            along_mass: mass_share(rolling),
            along_offset: offset.dot(rolling),
            side_offset: offset.dot(axle),
            camber,
            slip_ratio: spin
                .map(|spin| spin.slip_ratio)
                .filter(|ratio| ratio.is_finite())
                .map_or(0.0, |ratio| ratio.clamp(-1.0e3, 1.0e3)),
        };
        let SlipImpulse {
            side,
            along_extra,
            along_stuck,
            side_stuck,
        } = state.solve(tire, &contact, dt);
        // A held direction keeps the anchor's place; a free one moves it to the contact.
        if along_stuck || side_stuck {
            let mut kept = anchor.unwrap_or(point);
            if !along_stuck {
                kept += rolling * (point - kept).dot(rolling);
            }
            if !side_stuck {
                kept += axle * (point - kept).dot(axle);
            }
            state.anchor = Some(
                ground_body
                    .and_then(|handle| bodies.get(handle))
                    .map_or(kept, |ground| ground.position().inverse_transform_point(kept)),
            );
            state.anchor_body = ground_body;
        } else {
            state.anchor = None;
            state.anchor_body = None;
        }
        let side = axle * side;
        let along = rolling * along_extra;
        let lever = up * (up.dot(point - com) * (1.0 - tire.roll_influence));
        pushes[index] = SlipPush {
            active: true,
            contact: point,
            side_point: point - lever,
            side,
            along,
            total: normal * ((suspension + bar_force[index]) * dt)
                + rolling * (drivetrain_impulse + rapier_impulse)
                + side
                + along,
            ground,
        };
    }
    if let Some(body) = bodies.get_mut(chassis) {
        for push in pushes.iter().filter(|push| push.active) {
            if push.side != rapier3d::math::Vector::ZERO {
                body.apply_impulse_at_point(push.side, push.side_point, true);
            }
            if push.along != rapier3d::math::Vector::ZERO {
                body.apply_impulse_at_point(push.along, push.contact, true);
            }
        }
    }
    for push in pushes.iter().filter(|push| push.active) {
        if let Some(ground) = push.ground.and_then(|handle| bodies.get_mut(handle))
            && push.total.is_finite()
        {
            ground.apply_impulse_at_point(-push.total, push.contact, true);
        }
    }
}

impl Vehicle {
    /// Creates a vehicle: a dynamic box chassis at the given world pose (the box's center) and
    /// its wheels. The chassis never sleeps (its wheels push it every step).
    ///
    /// Nothing is added to the world when the configuration or the pose is invalid.
    pub fn spawn(
        physics: &mut PhysicsWorld,
        config: VehicleConfig,
        translation: Vec3,
        rotation: Quat,
    ) -> Result<Self, VehicleError> {
        config.validate()?;
        let pose = checked_pose(translation, rotation)?;
        let body = RigidBodyBuilder::dynamic()
            .pose(to_rapier_pose(pose.0, pose.1))
            .can_sleep(false)
            .build();
        let chassis = physics.bodies.insert(body);
        let mass = MassProperties::new(
            to_rapier_vec3(config.center_of_mass),
            config.chassis_mass,
            to_rapier_vec3(box_inertia(config.chassis_half_extents, config.chassis_mass)),
        );
        let half = config.chassis_half_extents;
        let collider = ColliderBuilder::cuboid(half.x, half.y, half.z)
            .mass_properties(mass)
            .build();
        let chassis_collider = {
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            colliders.insert_with_parent(collider, chassis, bodies)
        };
        let config_drivetrain = config.drivetrain_state();
        let tires = config.tire_states();
        Ok(Self {
            controller: build_controller(chassis, &config),
            config,
            chassis: RigidBodyHandle::from_rapier(chassis),
            chassis_collider: Some(chassis_collider),
            spawn_pose: pose,
            input: VehicleInput::default(),
            steps: 0,
            drivetrain: config_drivetrain,
            contact_shift: Vec3::ZERO,
            tires,
            last_dt: 0.0,
            config_revision: 0,
        })
    }

    /// Turns an existing dynamic body into a vehicle chassis, keeping its colliders and mass.
    /// `chassis_half_extents`, `chassis_mass` and `center_of_mass` in `config` are not used.
    /// The body is set to never sleep. Its current pose becomes the spawn pose.
    pub fn attach(
        physics: &mut PhysicsWorld,
        chassis: RigidBodyHandle,
        config: VehicleConfig,
    ) -> Result<Self, VehicleError> {
        config.validate()?;
        let handle = chassis.to_handle();
        let Some(body) = physics.bodies.get_mut(handle) else {
            return Err(VehicleError::ChassisMissing);
        };
        if !body.is_dynamic() {
            return Err(VehicleError::ChassisNotDynamic);
        }
        if !positive(body.mass()) {
            return Err(VehicleError::ChassisMassless);
        }
        let keep_sleeping = body.is_sleeping();
        *body.activation_mut() = RigidBodyActivation::cannot_sleep();
        if keep_sleeping {
            body.wake_up(true);
        }
        let spawn_pose = from_rapier_pose(body.position());
        let drivetrain = config.drivetrain_state();
        let tires = config.tire_states();
        Ok(Self {
            controller: build_controller(handle, &config),
            config,
            chassis,
            chassis_collider: None,
            spawn_pose,
            input: VehicleInput::default(),
            steps: 0,
            drivetrain,
            contact_shift: Vec3::ZERO,
            tires,
            last_dt: 0.0,
            config_revision: 0,
        })
    }

    /// The chassis body.
    pub fn chassis(&self) -> RigidBodyHandle {
        self.chassis
    }

    /// The chassis collider [`Vehicle::spawn`] created; `None` for an attached body.
    pub fn chassis_collider(&self) -> Option<ColliderHandle> {
        self.chassis_collider.map(ColliderHandle::from_rapier)
    }

    /// The configuration the vehicle was built with.
    pub fn config(&self) -> &VehicleConfig {
        &self.config
    }

    /// How many times [`Vehicle::set_config`] changed the configuration since spawn or attach.
    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }

    /// Retunes the vehicle in place: the chassis keeps its pose and velocity and the car keeps
    /// driving with `config` from the next [`Vehicle::update`] (ADR-9760).
    ///
    /// The rules:
    /// - **Refused** when `config` fails [`VehicleConfig::validate`] or the chassis is gone; then
    ///   nothing changes.
    /// - **Kept**: the chassis pose, linear and angular velocity (velocity, not momentum, when the
    ///   mass changes), the spawn pose, the step counter, the last input and time step and a pending
    ///   origin shift.
    /// - **Chassis**: a [spawned](Vehicle::spawn) vehicle whose `chassis_half_extents`,
    ///   `chassis_mass` or `center_of_mass` changed gets the new box, mass, center of mass and box
    ///   inertia, and the body's mass is recomputed at once. An [attached](Vehicle::attach) vehicle
    ///   keeps its own body, as `attach` does.
    /// - **Wheels**: with the same wheel count each wheel keeps its live record (spin angle,
    ///   steering, the last ray cast's contact, suspension force, impulses) and takes the new mount
    ///   point, radius, rest length, travel and tuning; the next update casts with them. A different
    ///   count rebuilds the wheels as at spawn.
    /// - **Drivetrain**: geared to geared with the same wheel count rebuilds the state from the new
    ///   configuration and carries the engine speed **clamped** into the new idle to redline range,
    ///   the engaged and target gear **clamped** to the new top forward gear (reverse and neutral
    ///   stay), the shift timer **clamped** to the new `shift_time`, the automatic flag, the shift
    ///   counts, the clutch and traction-control state and each wheel's spin. Any other change starts
    ///   the drivetrain fresh: idle, first gear, wheels at rest.
    /// - **Slip tires**: a wheel with a slip tire in both configurations keeps its tire state
    ///   (transient lag, stick anchor); every other wheel starts fresh.
    ///
    /// The result reads only the vehicle, the world and `config`, so two runs that apply the same
    /// configuration at the same step stay bit for bit alike.
    ///
    /// # Errors
    /// The [`VehicleError`] of [`VehicleConfig::validate`], or
    /// [`VehicleError::ChassisMissing`] / [`VehicleError::ChassisNotDynamic`] /
    /// [`VehicleError::ChassisMassless`] when the chassis no longer drives.
    pub fn set_config(
        &mut self,
        physics: &mut PhysicsWorld,
        config: VehicleConfig,
    ) -> Result<ConfigChange, VehicleError> {
        let _span = debug_span!("vehicle_set_config").entered();
        config.validate()?;
        self.check_chassis(physics)?;
        let chassis = self.chassis.to_handle();
        let same_wheels = config.wheels.len() == self.config.wheels.len();
        let mut change = ConfigChange {
            wheels_kept: same_wheels,
            ..ConfigChange::default()
        };

        let chassis_changed = config.chassis_half_extents != self.config.chassis_half_extents
            || config.chassis_mass != self.config.chassis_mass
            || config.center_of_mass != self.config.center_of_mass;
        if let (Some(collider), true) = (self.chassis_collider, chassis_changed) {
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            if let Some(collider) = colliders.get_mut(collider) {
                let half = config.chassis_half_extents;
                collider.set_shape(rapier3d::geometry::SharedShape::cuboid(half.x, half.y, half.z));
                collider.set_mass_properties(MassProperties::new(
                    to_rapier_vec3(config.center_of_mass),
                    config.chassis_mass,
                    to_rapier_vec3(box_inertia(half, config.chassis_mass)),
                ));
                change.chassis_updated = true;
            }
            if let Some(body) = bodies.get_mut(chassis) {
                body.recompute_mass_properties_from_colliders(colliders);
            }
        }

        if same_wheels {
            for (wheel, wheel_config) in self.controller.wheels_mut().iter_mut().zip(&config.wheels) {
                let tuning = wheel_config.tuning();
                wheel.chassis_connection_point_cs = to_rapier_vec3(wheel_config.position);
                wheel.suspension_rest_length = wheel_config.suspension_rest_length;
                wheel.radius = wheel_config.radius;
                wheel.max_suspension_travel = tuning.max_suspension_travel;
                wheel.suspension_stiffness = tuning.suspension_stiffness;
                wheel.damping_compression = tuning.suspension_compression;
                wheel.damping_relaxation = tuning.suspension_damping;
                wheel.friction_slip = tuning.friction_slip;
                wheel.side_friction_stiffness = tuning.side_friction_stiffness;
                wheel.max_suspension_force = tuning.max_suspension_force;
            }
        } else {
            self.controller = build_controller(chassis, &config);
        }

        self.drivetrain = match (&config.drivetrain, self.drivetrain.as_deref()) {
            (Drivetrain::Geared(drivetrain), Some(old)) if same_wheels => {
                let carried = old.carried_into(drivetrain, &config.wheels, config.max_brake_force);
                change.drivetrain_kept = true;
                change.gear_clamped = carried.gear_clamped;
                change.rpm_clamped = carried.rpm_clamped;
                Some(Box::new(carried.state))
            }
            _ => config.drivetrain_state(),
        };

        let mut tires = config.tire_states();
        if same_wheels {
            for (index, tire) in tires.iter_mut().enumerate() {
                let was_slip = self
                    .config
                    .wheels
                    .get(index)
                    .is_some_and(|wheel| wheel.tire_model.slip().is_some());
                let is_slip = config.wheels[index].tire_model.slip().is_some();
                if let (true, true, Some(old)) = (was_slip, is_slip, self.tires.get(index)) {
                    *tire = *old;
                    change.tires_kept += 1;
                }
            }
        }
        self.tires = tires;
        self.config = config;
        self.config_revision += 1;
        Ok(change)
    }

    /// The last input [`Vehicle::update`] applied, after sanitizing.
    pub fn input(&self) -> VehicleInput {
        self.input
    }

    /// Updates since spawn, attach or the last reset.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Re-expresses the stored spawn pose and the wheels' last contacts after a floating-origin
    /// rebase (the chassis body moves with the rest of the physics world). [`Self::wheel_states`]
    /// reports the contacts in the new space at once; the next update casts the rays afresh
    /// (ADR-2435).
    pub fn shift_origin(&mut self, shift: Vec3) {
        self.spawn_pose.0 -= shift;
        self.contact_shift += shift;
        // A slip tire's stick anchor in world space moves with the world; one in a body's frame
        // moves with that body.
        for tire in &mut self.tires {
            if let (Some(anchor), None) = (tire.anchor.as_mut(), tire.anchor_body) {
                *anchor -= to_rapier_vec3(shift);
            }
        }
    }

    /// The geared drivetrain's state; `None` with the `Simple` drivetrain.
    pub fn drivetrain(&self) -> Option<&DrivetrainState> {
        self.drivetrain.as_deref()
    }

    /// A driver's gear change: the automatic turns off until [`Self::set_automatic_shifting`]
    /// turns it on. Returns the gear it is moving to.
    ///
    /// # Errors
    /// [`VehicleError::NoGearbox`] with the `Simple` drivetrain; [`VehicleError::InvalidGear`] for
    /// a gear the gearbox does not have.
    pub fn request_shift(&mut self, request: ShiftRequest) -> Result<i32, VehicleError> {
        match (&self.config.drivetrain, self.drivetrain.as_deref_mut()) {
            (Drivetrain::Geared(drivetrain), Some(state)) => state.request_shift(drivetrain, request),
            _ => Err(VehicleError::NoGearbox),
        }
    }

    /// Hands the gears to the automatic (`true`) or to the driver (`false`).
    ///
    /// # Errors
    /// [`VehicleError::NoGearbox`] with the `Simple` drivetrain.
    pub fn set_automatic_shifting(&mut self, automatic: bool) -> Result<(), VehicleError> {
        let state = self.drivetrain.as_deref_mut().ok_or(VehicleError::NoGearbox)?;
        state.set_automatic(automatic);
        Ok(())
    }

    /// A digest of the drivetrain's state and the step count: two runs with the same inputs give
    /// the same value. Compare it with `PhysicsWorld::state_digest`, which covers the chassis.
    /// With slip tires it also covers their state; a car with none keeps the digest it had before
    /// slip tires existed.
    pub fn state_digest(&self) -> u64 {
        let drivetrain = self.drivetrain.as_deref().map_or(0, DrivetrainState::digest);
        let digest = drivetrain ^ self.steps.wrapping_mul(0x0000_0100_0000_01b3);
        if self.tires.is_empty() {
            return digest;
        }
        let mut hash = digest;
        for tire in &self.tires {
            for word in tire.digest_words() {
                for byte in word.to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
        hash
    }

    /// What every wheel's tire did on the last update, in configuration order: a slip tire's
    /// forces and hold, or rapier's side and forward impulses over the step for a damper tire.
    /// All zeros before the first update.
    pub fn tire_forces(&self) -> Vec<TireForces> {
        let mut forces = Vec::new();
        self.tire_forces_into(&mut forces);
        forces
    }

    /// [`Vehicle::tire_forces`] into `out` (cleared first): reuse one buffer every frame to read
    /// the tires without allocating (vehicle feedback, ADR-3276).
    pub fn tire_forces_into(&self, out: &mut Vec<TireForces>) {
        let dt = self.last_dt;
        out.clear();
        out.extend(
            self.controller
                .wheels()
                .iter()
                .zip(&self.config.wheels)
                .enumerate()
                .map(|(index, (wheel, config))| {
                    if let Some(state) = self
                        .tires
                        .get(index)
                        .filter(|_| config.tire_model.slip().is_some())
                    {
                        return state.forces;
                    }
                    let grounded = self.steps > 0 && wheel.raycast_info().is_in_contact;
                    if !grounded || dt <= 0.0 {
                        return TireForces::default();
                    }
                    let geared = self
                        .drivetrain
                        .as_deref()
                        .and_then(|state| state.wheels().get(index));
                    TireForces {
                        slip: false,
                        lateral_force: wheel.side_impulse / dt,
                        longitudinal_force: geared.map_or(wheel.forward_impulse / dt, |spin| spin.tire_force),
                        load: wheel.wheel_suspension_force.min(config.max_suspension_force),
                        slip_angle: 0.0,
                        stuck: false,
                        ..TireForces::default()
                    }
                }),
        );
    }

    /// The pose [`Vehicle::reset_to_spawn`] returns to.
    pub fn spawn_pose(&self) -> (Vec3, Quat) {
        self.spawn_pose
    }

    /// rapier's wheel records and speed, for [`crate::vehicle_snapshot`] (ADR-3080).
    pub(crate) fn controller_state(&self) -> (&[rapier3d::control::Wheel], f32) {
        (self.controller.wheels(), self.controller.current_vehicle_speed)
    }

    /// The simulation fields a snapshot keeps besides the chassis and the wheels: the last input,
    /// the step count and the contact record's pending rebase (ADR-3080).
    pub(crate) fn step_fields(&self) -> (VehicleInput, u64, Vec3) {
        (self.input, self.steps, self.contact_shift)
    }

    /// Writes back what [`Self::controller_state`] and [`Self::step_fields`] read (ADR-3080).
    pub(crate) fn restore_fields(
        &mut self,
        (wheels, speed): (&[rapier3d::control::Wheel], f32),
        (input, steps, contact_shift): (VehicleInput, u64, Vec3),
    ) {
        for (wheel, saved) in self.controller.wheels_mut().iter_mut().zip(wheels) {
            *wheel = *saved;
        }
        self.controller.current_vehicle_speed = speed;
        self.input = input;
        self.steps = steps;
        self.contact_shift = contact_shift;
    }

    /// rapier's wheel records to write: a correction sets their spin (ADR-3080).
    pub(crate) fn wheels_mut(&mut self) -> &mut [rapier3d::control::Wheel] {
        self.controller.wheels_mut()
    }

    /// The geared drivetrain's state to write: a restore replaces it (ADR-3080).
    pub(crate) fn drivetrain_slot(&mut self) -> &mut Option<Box<DrivetrainState>> {
        &mut self.drivetrain
    }

    /// The slip tires' states, empty when no wheel has a slip tire, for a snapshot (ADR-3080).
    pub(crate) fn tires(&self) -> &[TireState] {
        &self.tires
    }

    /// The slip tires' states to write: a restore puts them back, a correction drops the stick
    /// anchors (ADR-3080).
    pub(crate) fn tires_mut(&mut self) -> &mut [TireState] {
        &mut self.tires
    }

    /// Checks the chassis is still a dynamic body with mass in `physics`.
    fn check_chassis(&self, physics: &PhysicsWorld) -> Result<(), VehicleError> {
        match physics.bodies.get(self.chassis.to_handle()) {
            None => Err(VehicleError::ChassisMissing),
            Some(body) if !body.is_dynamic() => Err(VehicleError::ChassisNotDynamic),
            Some(body) if !positive(body.mass()) => Err(VehicleError::ChassisMassless),
            Some(_) => Ok(()),
        }
    }

    /// Applies one step of driver input: casts the wheel rays and pushes the chassis with the
    /// suspension, engine, brake and tire impulses. Call once per fixed step, before the world
    /// steps, with that step's `dt` (seconds).
    ///
    /// The input is [sanitized](VehicleInput::sanitized). On error the world is unchanged.
    pub fn update(
        &mut self,
        physics: &mut PhysicsWorld,
        input: VehicleInput,
        dt: f32,
    ) -> Result<(), VehicleError> {
        self.update_with_layers(physics, input, dt, None)
    }

    /// [`Self::update`] with the layer matrix the wheel rays filter by
    /// ([`VehicleConfig::ground_layers`] and [`VehicleConfig::query_layer`]). Without `layers`
    /// a `query_layer` has no matrix to read and only `ground_layers` applies.
    pub fn update_with_layers(
        &mut self,
        physics: &mut PhysicsWorld,
        input: VehicleInput,
        dt: f32,
        layers: Option<&PhysicsLayers>,
    ) -> Result<(), VehicleError> {
        if !(dt.is_finite() && dt > 0.0) {
            return Err(VehicleError::InvalidTimeStep);
        }
        self.check_chassis(physics)?;
        let chassis = self.chassis.to_handle();
        let input = input.sanitized();
        self.apply_input(input, dt);
        // A body attached through the ECS may have had its activation rewritten.
        if let Some(body) = physics.bodies.get_mut(chassis)
            && body.is_sleeping()
        {
            body.wake_up(true);
        }
        let mut filter = QueryFilter::default()
            .exclude_sensors()
            .exclude_rigid_body(chassis);
        if let Some(groups) = self.ground_groups(layers) {
            filter = filter.groups(groups);
        }
        let queries = physics.query_pipeline_mut(filter);
        self.controller.update_vehicle(dt, queries);
        // The rays were cast in the current space: the record is current again.
        self.contact_shift = Vec3::ZERO;
        self.apply_chassis_systems(physics, input, dt);
        self.input = input;
        self.last_dt = dt;
        self.steps += 1;
        Ok(())
    }

    /// The interaction groups of the wheel rays: `None` when they hit every layer.
    fn ground_groups(&self, layers: Option<&PhysicsLayers>) -> Option<InteractionGroups> {
        ray_groups(self.config.ground_layers, self.config.query_layer, layers)
    }

    /// Anti-roll, the geared drivetrain's tire impulses and aero, after rapier's suspension and side
    /// friction. Nothing here touches a vehicle that uses none of them, so its bits stay rapier's.
    fn apply_chassis_systems(&mut self, physics: &mut PhysicsWorld, input: VehicleInput, dt: f32) {
        if self.drivetrain.is_none()
            && self.config.anti_roll.is_empty()
            && self.config.aero.is_none()
            && self.tires.is_empty()
        {
            return;
        }
        let chassis = self.chassis.to_handle();
        let count = self.config.wheels.len().min(MAX_WHEELS);
        let wheels = self.controller.wheels();

        // Anti-roll bars push on the wheel contacts, up on the compressed side.
        let mut bar_force = [0.0_f32; MAX_WHEELS];
        for bar in &self.config.anti_roll {
            let (Some(left), Some(right)) = (wheels.get(bar.left), wheels.get(bar.right)) else {
                continue;
            };
            if left.raycast_info().is_in_contact && right.raycast_info().is_in_contact {
                let squash = |wheel: &rapier3d::control::Wheel| {
                    wheel.suspension_rest_length - wheel.raycast_info().suspension_length
                };
                let force = bar.force(squash(left), squash(right));
                bar_force[bar.left] += force;
                bar_force[bar.right] -= force;
            }
        }
        let PhysicsWorld {
            bodies, colliders, ..
        } = physics;
        if let Some(body) = bodies.get_mut(chassis) {
            for (wheel, force) in wheels.iter().zip(&bar_force) {
                if *force != 0.0 {
                    let info = wheel.raycast_info();
                    body.apply_impulse_at_point(
                        info.contact_normal_ws * (*force * dt),
                        info.contact_point_ws,
                        true,
                    );
                }
            }
        }

        // The drivetrain measures the contacts, then the tires push the chassis.
        if let (Drivetrain::Geared(drivetrain), Some(state), Some(body)) = (
            &self.config.drivetrain,
            self.drivetrain.as_deref_mut(),
            bodies.get(chassis),
        ) {
            let rotation = from_rapier_pose(body.position()).1;
            let mass = body.mass();
            let forward_speed = from_rapier_vec3(body.linvel()).dot(rotation * FORWARD);
            let mut contacts = [WheelContact::default(); MAX_WHEELS];
            let mut rolling = [Vec3::ZERO; MAX_WHEELS];
            for (index, (wheel, config)) in wheels.iter().zip(&self.config.wheels).enumerate().take(count) {
                let info = wheel.raycast_info();
                if !info.is_in_contact {
                    continue;
                }
                let normal = from_rapier_vec3(info.contact_normal_ws);
                let axle = from_rapier_vec3(wheel.axle());
                let axle = (axle - normal * axle.dot(normal)).normalize_or_zero();
                rolling[index] = normal.cross(axle).normalize_or_zero();
                let relative = from_rapier_vec3(body.velocity_at_point(info.contact_point_ws))
                    - ground_velocity(bodies, colliders, info.ground_object, info.contact_point_ws);
                contacts[index] = WheelContact {
                    grounded: true,
                    load: (wheel.wheel_suspension_force.min(config.max_suspension_force) + bar_force[index])
                        .max(0.0),
                    ground_speed: relative.dot(rolling[index]),
                };
            }
            let driver = DrivetrainInput {
                throttle: input.throttle,
                brake: input.brake,
                forward_speed,
                chassis_mass: mass,
            };
            state.step(drivetrain, &self.config.aids, &driver, &contacts[..count], dt);
            if let Some(body) = bodies.get_mut(chassis) {
                for (index, (wheel, spin)) in wheels.iter().zip(state.wheels()).enumerate().take(count) {
                    // A setting at the edge of its range must not put NaN into the world's bodies.
                    if contacts[index].grounded && spin.tire_impulse != 0.0 && spin.tire_impulse.is_finite() {
                        body.apply_impulse_at_point(
                            to_rapier_vec3(rolling[index] * spin.tire_impulse),
                            wheel.raycast_info().contact_point_ws,
                            true,
                        );
                    }
                }
            }
        }

        // Slip tires: the side force from the slip angle, the static hold, the ground's reaction.
        if !self.tires.is_empty() {
            let geared = match (&self.config.drivetrain, self.drivetrain.as_deref()) {
                (Drivetrain::Geared(drivetrain), Some(state)) => Some((drivetrain, state)),
                _ => None,
            };
            apply_slip_tires(
                SlipPass {
                    config: &self.config,
                    wheels,
                    geared,
                    chassis,
                    input,
                    dt,
                    bar_force: &bar_force,
                },
                &mut self.tires,
                bodies,
                colliders,
            );
        }

        // Drag at the center of mass, downforce at the axles.
        if let (Some(aero), Some(body)) = (&self.config.aero, bodies.get_mut(chassis)) {
            let (center, rotation) = from_rapier_pose(body.position());
            let velocity = from_rapier_vec3(body.linvel());
            body.apply_impulse(to_rapier_vec3(aero.drag_force(velocity) * dt), true);
            let forward_speed = velocity.dot(rotation * FORWARD);
            let down = rotation * Vec3::NEG_Y;
            for (coefficient, point) in [
                (aero.front_downforce, wheel_center(&self.config.wheels, true)),
                (aero.rear_downforce, wheel_center(&self.config.wheels, false)),
            ] {
                let force = aero.downforce(coefficient, forward_speed);
                if force != 0.0 {
                    body.apply_impulse_at_point(
                        to_rapier_vec3(down * (force * dt)),
                        to_rapier_vec3(center + rotation * point),
                        true,
                    );
                }
            }
        }
    }

    /// Maps the input onto rapier's per-wheel steering, engine force and brake impulse.
    fn apply_input(&mut self, input: VehicleInput, dt: f32) {
        let steering = -input.steer * self.config.max_steer_angle;
        if let (Drivetrain::Geared(drivetrain), Some(state)) =
            (&self.config.drivetrain, self.drivetrain.as_deref())
        {
            // The tires' lengthwise force is the drivetrain's: rapier keeps the steering and the
            // side friction, limited by what the lengthwise force leaves of the friction circle.
            for (index, (wheel, config)) in self
                .controller
                .wheels_mut()
                .iter_mut()
                .zip(&self.config.wheels)
                .enumerate()
            {
                wheel.steering = if config.steer { steering } else { 0.0 };
                wheel.engine_force = 0.0;
                wheel.brake = 0.0;
                wheel.friction_slip = state.lateral_friction(drivetrain, index);
            }
            return;
        }
        let engine = input.throttle * self.config.max_engine_force;
        let brake = input.brake * self.config.max_brake_force * dt;
        let rolling = self.config.rolling_resistance * dt;
        for (wheel, config) in self.controller.wheels_mut().iter_mut().zip(&self.config.wheels) {
            let braking = config.brake && brake > 0.0;
            let driving = config.drive && !braking && engine != 0.0;
            wheel.steering = if config.steer { steering } else { 0.0 };
            wheel.engine_force = if driving { engine } else { 0.0 };
            // rapier brakes only wheels without engine force; `brake` caps the impulse that
            // stops the wheel's contact from sliding along its rolling direction.
            wheel.brake = if braking {
                brake
            } else if driving {
                0.0
            } else {
                rolling
            };
        }
    }

    /// Every wheel after the last update, posed against the chassis' current pose. Allocates a
    /// fresh `Vec` per call; per-frame callers should keep one buffer and use
    /// [`Vehicle::wheel_states_into`] instead.
    pub fn wheel_states(&self, physics: &PhysicsWorld) -> Result<Vec<WheelState>, VehicleError> {
        let mut states = Vec::new();
        self.wheel_states_into(physics, &mut states)?;
        Ok(states)
    }

    /// Fills `out` with every wheel after the last update, posed against the chassis' current
    /// pose. `out` is cleared first and its capacity is reused, so a warm caller allocates
    /// nothing. The contents equal [`Vehicle::wheel_states`]'s.
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis body is gone (the readout panics on
    /// nothing; `out` is left empty).
    pub fn wheel_states_into(
        &self,
        physics: &PhysicsWorld,
        out: &mut Vec<WheelState>,
    ) -> Result<(), VehicleError> {
        let Some(body) = physics.bodies.get(self.chassis.to_handle()) else {
            out.clear();
            return Err(VehicleError::ChassisMissing);
        };
        let (chassis_t, chassis_r) = from_rapier_pose(body.position());
        let updated = self.steps > 0;
        out.clear();
        out.extend(
            self.controller
                .wheels()
                .iter()
                .zip(&self.config.wheels)
                .enumerate()
                .map(|(index, (wheel, config))| {
                    let info = wheel.raycast_info();
                    let grounded = updated && info.is_in_contact;
                    // Before the first update rapier's ray-cast record is empty: report rest.
                    let suspension_length = if updated {
                        info.suspension_length
                    } else {
                        config.suspension_rest_length
                    };
                    let steer_angle = if updated { wheel.steering } else { 0.0 };
                    let spin = self
                        .drivetrain
                        .as_deref()
                        .and_then(|state| state.wheels().get(index));
                    let spin_angle = spin.map_or(wheel.rotation, |spin| spin.spin_angle);
                    let local_translation = config.position + SUSPENSION_DIRECTION * suspension_length;
                    // Deterministic half-angle kernels, not platform sin/cos: the readout feeds
                    // snapshots and debug views that must agree across hosts (ADR-4773).
                    let local_rotation = det_quat_from_axis_angle(Vec3::Y, steer_angle)
                        * det_quat_from_axis_angle(Vec3::X, -spin_angle);
                    let ground_collider = if grounded { info.ground_object } else { None };
                    let contact_point =
                        grounded.then(|| from_rapier_vec3(info.contact_point_ws) - self.contact_shift);
                    let contact_normal = grounded.then(|| from_rapier_vec3(info.contact_normal_ws));
                    let slip_angle = match (contact_point, contact_normal) {
                        (Some(point), Some(normal)) => {
                            let at = to_rapier_vec3(point);
                            let velocity = from_rapier_vec3(body.velocity_at_point(at))
                                - ground_velocity(&physics.bodies, &physics.colliders, ground_collider, at);
                            contact_slip_angle(velocity, normal, chassis_r * local_rotation_axle(steer_angle))
                        }
                        _ => 0.0,
                    };
                    WheelState {
                        index,
                        local_translation,
                        local_rotation,
                        world_translation: chassis_t + chassis_r * local_translation,
                        world_rotation: (chassis_r * local_rotation).normalize(),
                        steer_angle,
                        spin_angle,
                        suspension_length,
                        compression: config.suspension_rest_length - suspension_length,
                        grounded,
                        contact_point,
                        contact_normal,
                        ground_collider,
                        ground_entity: ground_collider.and_then(|handle| physics.entity_of_collider(handle)),
                        suspension_force: if grounded {
                            wheel.wheel_suspension_force.min(config.max_suspension_force)
                        } else {
                            0.0
                        },
                        slip_ratio: if grounded {
                            spin.map_or(0.0, |spin| spin.slip_ratio)
                        } else {
                            0.0
                        },
                        slip_angle,
                    }
                }),
        );
        Ok(())
    }

    /// The chassis pose and velocity plus every wheel ([`Vehicle::wheel_states`]).
    pub fn state(&self, physics: &PhysicsWorld) -> Result<VehicleState, VehicleError> {
        let wheels = self.wheel_states(physics)?;
        let Some(body) = physics.bodies.get(self.chassis.to_handle()) else {
            return Err(VehicleError::ChassisMissing);
        };
        let (translation, rotation) = from_rapier_pose(body.position());
        let forward = rotation * FORWARD;
        let linear_velocity = from_rapier_vec3(body.linvel());
        let up = rotation * Vec3::Y;
        let drivetrain = self.drivetrain.as_deref();
        Ok(VehicleState {
            translation,
            rotation,
            forward,
            linear_velocity,
            angular_velocity: from_rapier_vec3(body.angvel()),
            forward_speed: linear_velocity.dot(forward),
            grounded_wheels: wheels.iter().filter(|wheel| wheel.grounded).count(),
            wheels,
            rpm: drivetrain.map_or(0.0, DrivetrainState::rpm),
            gear: drivetrain.map_or(0, DrivetrainState::gear),
            speed: (linear_velocity - up * linear_velocity.dot(up)).length(),
        })
    }

    /// Teleports the chassis to a pose at rest (no velocity, no accumulated forces) and clears
    /// the wheels (spin, steering, suspension record). The step counter restarts at 0.
    ///
    /// On error nothing changes.
    pub fn reset(
        &mut self,
        physics: &mut PhysicsWorld,
        translation: Vec3,
        rotation: Quat,
    ) -> Result<(), VehicleError> {
        let pose = checked_pose(translation, rotation)?;
        let chassis = self.chassis.to_handle();
        let Some(body) = physics.bodies.get_mut(chassis) else {
            return Err(VehicleError::ChassisMissing);
        };
        body.set_position(to_rapier_pose(pose.0, pose.1), true);
        body.set_linvel(to_rapier_vec3(Vec3::ZERO), true);
        body.set_angvel(to_rapier_vec3(Vec3::ZERO), true);
        body.reset_forces(true);
        body.reset_torques(true);
        self.controller = build_controller(chassis, &self.config);
        self.input = VehicleInput::default();
        self.steps = 0;
        self.drivetrain = self.config.drivetrain_state();
        self.contact_shift = Vec3::ZERO;
        self.tires = self.config.tire_states();
        self.last_dt = 0.0;
        Ok(())
    }

    /// [`Vehicle::reset`] to the pose the vehicle was spawned or attached at.
    pub fn reset_to_spawn(&mut self, physics: &mut PhysicsWorld) -> Result<(), VehicleError> {
        let (translation, rotation) = self.spawn_pose;
        self.reset(physics, translation, rotation)
    }

    /// Removes the chassis body and its colliders from the world. Returns `false` when the
    /// body was already gone.
    pub fn remove(self, physics: &mut PhysicsWorld) -> bool {
        let handle = self.chassis.to_handle();
        if !physics.bodies.contains(handle) {
            return false;
        }
        physics.remove_body(handle);
        true
    }
}

// ---------------------------------------------------------------------------
// ECS
// ---------------------------------------------------------------------------

/// A vehicle on an ECS entity, updated by [`VehiclePlugin`] from the entity's
/// [`VehicleInput`].
#[derive(Component, Debug, Clone)]
pub struct VehicleController {
    /// The vehicle (read it with [`Vehicle::state`] against the `PhysicsWorld` resource).
    pub vehicle: Vehicle,
    /// The error from the last fixed-step update, `None` when it succeeded.
    pub last_error: Option<VehicleError>,
}

impl VehicleController {
    /// Wraps a vehicle (no error yet).
    pub fn new(vehicle: Vehicle) -> Self {
        Self {
            vehicle,
            last_error: None,
        }
    }
}

/// Creates a vehicle in the app's `PhysicsWorld` and an entity that owns it.
///
/// The entity gets the chassis' [`RigidBodyHandle`] and [`ColliderHandle`] (so the physics
/// step writes its `Transform`, and queries and collision events name it), a `Transform` at the
/// spawn pose, a [`VehicleController`] and a released [`VehicleInput`]. Despawning the entity
/// removes the chassis from the world. Nothing is spawned on error.
pub fn spawn_vehicle(
    world: &mut World,
    config: VehicleConfig,
    translation: Vec3,
    rotation: Quat,
) -> Result<Entity, VehicleError> {
    let vehicle = {
        let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() else {
            return Err(VehicleError::NoPhysicsWorld);
        };
        Vehicle::spawn(&mut physics, config, translation, rotation)?
    };
    let (translation, rotation) = vehicle.spawn_pose();
    let chassis = vehicle.chassis();
    let collider = vehicle.chassis_collider;
    let mut entity = world.spawn((
        chassis,
        Transform {
            translation,
            rotation,
            scale: Vec3::ONE,
        },
        VehicleController::new(vehicle),
        VehicleInput::default(),
    ));
    if let Some(collider) = collider {
        entity.insert(ColliderHandle::from_rapier(collider));
    }
    let entity = entity.id();
    if let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() {
        physics.register_body(chassis.to_handle(), entity);
        if let Some(collider) = collider {
            physics.register_collider(collider, entity);
        }
    }
    Ok(entity)
}

/// Updates every [`VehicleController`] from its entity's [`VehicleInput`] (a missing input
/// reads as released), in entity spawn order so runs are reproducible. Errors are stored in
/// [`VehicleController::last_error`]. A vehicle of a parked data layer is not driven.
pub fn drive_vehicles(
    fixed: Option<Res<FixedTime>>,
    physics: Option<ResMut<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    mut vehicles: Query<(Entity, &mut VehicleController, Option<&VehicleInput>), Without<LayerParked>>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<VehicleController>>>,
    rebase: (Option<ResMut<RebaseHolds>>, Option<ResMut<RebaseBlockers>>),
) {
    let (Some(fixed), Some(mut physics)) = (fixed, physics) else {
        return;
    };
    let _span = debug_span!("drive_vehicles").entered();
    // The contact solve holds `vehicle_contact_solve` while it runs (ADR-2435). The schedule
    // already keeps `StreamSet::Rebase` after the physics step, so in the engine's order this hold
    // never meets a rebase; it marks the window for a host that orders its own sets.
    let (mut holds, mut blockers) = rebase;
    if let (Some(holds), Some(blockers)) = (holds.as_deref_mut(), blockers.as_deref_mut()) {
        holds.acquire(blockers, VEHICLE_CONTACT_BLOCKER);
    }
    let dt = fixed.step().as_secs_f32();
    // Spawn order (component arrival), not entity ids: ids from parallel `Commands::spawn`
    // follow thread timing, and wheel impulses on a shared ground body add up in this order
    // (ADR-2396). Entity ids are the fallback when tracking is off.
    let mut drive: Vec<Entity> = vehicles.iter().map(|(entity, _, _)| entity).collect();
    drive.sort_by_cached_key(|entity| spawn_key(order.as_deref(), *entity));
    for entity in drive {
        let Ok((_, mut controller, input)) = vehicles.get_mut(entity) else {
            continue;
        };
        let input = input.copied().unwrap_or_default();
        let result = controller
            .vehicle
            .update_with_layers(&mut physics, input, dt, layers.as_deref());
        let error = result.err();
        if controller.last_error != error {
            controller.last_error = error;
        }
    }
    if let (Some(holds), Some(blockers)) = (holds.as_deref_mut(), blockers.as_deref_mut()) {
        holds.release(blockers, VEHICLE_CONTACT_BLOCKER);
    }
}

/// A rebase moves the stored spawn pose and the last contact points of every vehicle, or a respawn
/// would put the car back in the old frame and a report would show contacts there. A hover craft's
/// last contact points move the same way.
fn shift_vehicles_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut vehicles: Query<&mut VehicleController>,
    mut craft: Query<&mut crate::hover::HoverController>,
) {
    for message in shifted.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        for mut controller in &mut vehicles {
            controller.vehicle.shift_origin(shift);
        }
        for mut controller in &mut craft {
            controller.craft.shift_origin(shift);
        }
    }
}

/// Registers [`VehicleInput`] and runs [`drive_vehicles`] in `FixedUpdate`, inside the
/// physics chain after the velocity sync and before the step. Add it after `PhysicsPlugin`.
pub struct VehiclePlugin;

impl Plugin for VehiclePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<VehicleInput>().add_systems(
            FixedUpdate,
            (drive_vehicles, crate::hover::drive_hovercraft)
                .after(sync_velocity_and_forces)
                .before(step_physics)
                .in_set(PhysicsStep),
        );
        app.add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                shift_vehicles_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
        // Vehicles are driven in the order they arrived, not in entity-id order (ADR-2396).
        crate::plugin::must_track_spawn_order::<VehicleController>(app.world_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_origin_moves_the_spawn_pose() {
        let mut physics = PhysicsWorld::default();
        let mut vehicle = Vehicle::spawn(
            &mut physics,
            VehicleConfig::default(),
            Vec3::new(2000.0, 1.0, 0.0),
            Quat::IDENTITY,
        )
        .expect("spawn");
        vehicle.shift_origin(Vec3::new(1024.0, 0.0, 0.0));
        assert_eq!(vehicle.spawn_pose().0, Vec3::new(976.0, 1.0, 0.0));
    }

    #[test]
    fn box_inertia_matches_the_solid_cuboid_formula() {
        // A 2 × 4 × 6 m box of 12 kg: I = m/12 (h² + d²) per axis.
        let inertia = box_inertia(Vec3::new(1.0, 2.0, 3.0), 12.0);
        assert!(
            inertia.abs_diff_eq(Vec3::new(52.0, 40.0, 20.0), 1.0e-4),
            "{inertia}"
        );
    }

    #[test]
    fn checked_pose_normalizes_and_rejects_degenerate_rotations() {
        let (_, rotation) = checked_pose(Vec3::ZERO, Quat::from_xyzw(0.0, 0.0, 0.0, 2.0)).expect("pose");
        assert!(rotation.abs_diff_eq(Quat::IDENTITY, 1.0e-6));
        assert_eq!(
            checked_pose(Vec3::ZERO, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
            Err(VehicleError::InvalidPose)
        );
        assert_eq!(
            checked_pose(Vec3::ZERO, Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0)),
            Err(VehicleError::InvalidPose)
        );
    }

    #[test]
    fn input_mapping_follows_the_wheel_flags() {
        let mut physics = PhysicsWorld::default();
        let mut vehicle =
            Vehicle::spawn(&mut physics, VehicleConfig::default(), Vec3::Y, Quat::IDENTITY).expect("spawn");
        let dt = 0.5;
        let max_steer = vehicle.config.max_steer_angle;
        let engine = vehicle.config.max_engine_force;
        let brake = vehicle.config.max_brake_force;
        let rolling = vehicle.config.rolling_resistance;

        // Throttle and a right turn: front wheels steer right (negative angle) and roll with
        // resistance, rear wheels drive.
        vehicle.apply_input(
            VehicleInput {
                throttle: 0.5,
                brake: 0.0,
                steer: 1.0,
            },
            dt,
        );
        let wheels = vehicle.controller.wheels();
        for wheel in &wheels[..2] {
            assert_eq!(wheel.steering, -max_steer);
            assert_eq!(wheel.engine_force, 0.0);
            assert_eq!(wheel.brake, rolling * dt);
        }
        for wheel in &wheels[2..] {
            assert_eq!(wheel.steering, 0.0);
            assert_eq!(wheel.engine_force, 0.5 * engine);
            assert_eq!(wheel.brake, 0.0);
        }

        // Brake with throttle held: every wheel brakes, no wheel drives.
        vehicle.apply_input(
            VehicleInput {
                throttle: 1.0,
                brake: 0.5,
                steer: -1.0,
            },
            dt,
        );
        let wheels = vehicle.controller.wheels();
        assert_eq!(wheels[0].steering, max_steer);
        for wheel in wheels {
            assert_eq!(wheel.engine_force, 0.0);
            assert_eq!(wheel.brake, 0.5 * brake * dt);
        }

        // A drive wheel without a brake keeps driving while the others brake.
        let mut config = VehicleConfig::default();
        config.wheels[2].brake = false;
        let mut vehicle = Vehicle::spawn(&mut physics, config, Vec3::Y, Quat::IDENTITY).expect("spawn");
        vehicle.apply_input(
            VehicleInput {
                throttle: 1.0,
                brake: 1.0,
                steer: 0.0,
            },
            dt,
        );
        let wheels = vehicle.controller.wheels();
        assert_eq!(wheels[2].engine_force, engine);
        assert_eq!(wheels[2].brake, 0.0);
        assert_eq!(wheels[3].engine_force, 0.0);
        assert_eq!(wheels[3].brake, brake * dt);
    }

    #[test]
    fn spawn_builds_a_never_sleeping_chassis_with_the_configured_mass() {
        let mut physics = PhysicsWorld::default();
        let config = VehicleConfig::default();
        let vehicle = Vehicle::spawn(
            &mut physics,
            config.clone(),
            Vec3::new(1.0, 2.0, 3.0),
            Quat::IDENTITY,
        )
        .expect("spawn");
        let body = &physics.bodies[vehicle.chassis().to_handle()];
        assert!(body.is_dynamic());
        assert!(body.activation().normalized_linear_threshold < 0.0);
        assert!(body.activation().angular_threshold < 0.0);
        assert!((body.mass() - config.chassis_mass).abs() < 1.0e-3);
        // The body takes its mass from the collider at the next step; the collider carries it.
        let collider_handle = vehicle.chassis_collider.expect("spawn creates a collider");
        let collider = &physics.colliders[collider_handle];
        assert_eq!(collider.parent(), Some(vehicle.chassis().to_handle()));
        assert!((collider.mass() - config.chassis_mass).abs() < 1.0e-3);
        let com = from_rapier_vec3(collider.mass_properties().local_com);
        assert!(com.abs_diff_eq(config.center_of_mass, 1.0e-6), "{com}");
        assert_eq!(vehicle.controller.wheels().len(), 4);
        assert_eq!(
            vehicle.chassis_collider().map(|c| c.to_handle()),
            vehicle.chassis_collider
        );
        assert!(vehicle.remove(&mut physics));
        assert_eq!(physics.bodies.len(), 0);
        assert_eq!(physics.colliders.len(), 0);
    }

    // --- ADR-4773 determinism pins (audit G finding 7): the readout helpers behind
    // `wheel_states` are bit-identical on every host.
    const STEERED_AXLE_BITS: [u32; 3] = [1_064_028_840, 0, 3_200_737_751];
    const CONTACT_SLIP_ANGLE_BITS: u32 = 1_065_619_138;

    #[test]
    fn slip_readout_helpers_are_deterministic() {
        let axle = local_rotation_axle(0.4);
        assert_eq!(
            [axle.x.to_bits(), axle.y.to_bits(), axle.z.to_bits()],
            STEERED_AXLE_BITS,
            "the steered axle's quaternion must come from the deterministic kernels"
        );
        let angle = contact_slip_angle(Vec3::new(2.5, -0.1, 0.35), Vec3::Y, axle);
        assert_eq!(
            angle.to_bits(),
            CONTACT_SLIP_ANGLE_BITS,
            "the contact slip angle's atan2 must be the deterministic kernel"
        );
    }
}
