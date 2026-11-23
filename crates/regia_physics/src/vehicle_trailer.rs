// SPDX-License-Identifier: MPL-2.0
// Source: rapier3d 0.36 public API and documentation (Apache-2.0; rapier.rs/docs): impulse joints
// built with `GenericJointBuilder` (locked axes, per-axis angular limits, contacts between the
// jointed bodies off), `ImpulseJointSet::insert`/`remove`, `Collider::set_enabled`. The trailer
// chassis is Regia's own `Vehicle` (rapier's ray-cast vehicle controller) with no driven or steered
// wheel. The articulation readout, the brake delay line, the surge-brake estimate (Newton's second
// law on the trailer), the capture radius, the stand and the train walk are original design
// (ADR-3281).

//! Trailers: articulated vehicles coupled to a tow vehicle (ADR-3281).
//!
//! A [`Trailer`] is a chassis on unpowered ray-cast wheels (a [`Vehicle`] whose wheels neither
//! drive nor steer) plus a coupling: a [`CouplingKind::TowBall`] (a spherical joint with yaw,
//! pitch and roll limits, the yaw limit being the jackknife stop) or a
//! [`CouplingKind::FifthWheel`] (a revolute about the vertical with a pitch limit and a yaw stop).
//! The coupling joins the trailer's [`CouplingConfig::point`] to a hitch point on the tow body
//! (any rigid body: a [`Vehicle`]'s chassis, another trailer's [`TrailerConfig::rear_hitch`], a
//! dolly, a post).
//!
//! - [`Trailer::couple`] succeeds within [`CouplingConfig::capture_radius`] of the hitch
//!   ([`TrailerError::OutOfReach`] otherwise); [`Trailer::uncouple`] lets go, puts the stand down
//!   and sets the parking brake.
//! - Brakes ([`TrailerBrakes`]): follow the tow vehicle's brake input with a proportion and a
//!   delay, or a surge brake that reacts to the drawbar pushing on the hitch.
//! - [`Trailer::readout`]: the articulation angle and its rate, the jackknife ratio and warning,
//!   the applied brake: what games and AI drivers read. Allocation-free.
//! - Trains: a trailer with a [`TrailerConfig::rear_hitch`] tows the next trailer or a dolly, up
//!   to [`MAX_TRAIN_LENGTH`] trailers behind one vehicle.
//!
//! # Conventions
//!
//! The vehicle conventions of [`crate::vehicle`]: forward is the chassis' −Z, right +X, up +Y.
//! The articulation angle is the trailer's yaw relative to the tow body about the tow body's +Y,
//! radians, positive when the trailer's nose points to the tow's left (seen from above,
//! counter-clockwise). In a steady right turn the trailer reads a positive angle.
//!
//! # Use
//!
//! - Bare world: [`Trailer::spawn`] or [`Trailer::spawn_coupled`]; each fixed step call the tow
//!   vehicle's update, then [`Trailer::update`] with the tow vehicle's brake input, then step the
//!   world. Trailers of a train update front to back.
//! - ECS: add [`TrailerPlugin`] after `PhysicsPlugin` and `VehiclePlugin`; [`spawn_trailer`],
//!   [`couple_trailer`], [`uncouple_trailer`]; read the [`TrailerState`] component. A tow
//!   entity needs a [`TowHitch`].
//!
//! See `book/agents/trailers.md` and `docs/formats/trailer.md`.

use rapier3d::dynamics::{GenericJointBuilder, ImpulseJointHandle, JointAxesMask, JointAxis};
use rapier3d::geometry::ColliderBuilder;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug_span;

use crate::components::{ColliderHandle, RigidBodyHandle};
use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_vec3};
use crate::layers::PhysicsLayers;
use crate::plugin::PhysicsStep;
use crate::step::{spawn_key, step_physics};
use crate::vehicle::{
    Vehicle, VehicleConfig, VehicleError, VehicleInput, WheelConfig, checked_pose, drive_vehicles,
};
use crate::vehicle_drivetrain::Drivetrain;
use crate::world::PhysicsWorld;

/// The most trailers one tow vehicle may pull in a chain (a dolly counts as a trailer).
pub const MAX_TRAIN_LENGTH: usize = 8;

/// Capacity of the follow-brake delay line, in steps; the delay is capped one below it.
pub const BRAKE_DELAY_STEPS: usize = 256;

/// The longest follow-brake delay a configuration may ask for, seconds.
pub const MAX_BRAKE_DELAY: f32 = 2.0;

/// The largest yaw (jackknife) limit, radians (100°).
pub const MAX_YAW_LIMIT: f32 = 1.745_329_3;

/// The largest capture radius, meters.
pub const MAX_CAPTURE_RADIUS: f32 = 5.0;

/// Below this forward speed (m/s) a surge brake does not act: it is locked out in reverse and
/// when crawling, as a real reversing solenoid does.
const SURGE_MIN_SPEED: f32 = 0.5;

/// The chassis' local forward axis.
const FORWARD: Vec3 = Vec3::NEG_Z;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a trailer could not be created, coupled, uncoupled, updated or read.
///
/// [`TrailerError::code`] gives a stable lowercase identifier for logs and the agent protocol.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum TrailerError {
    /// The chassis configuration or the chassis body failed a [`Vehicle`] check.
    #[error("trailer chassis: {0}")]
    Chassis(#[from] VehicleError),
    /// A trailer-wide field is out of range or not finite.
    #[error("trailer config: `{field}` is out of range or not finite")]
    InvalidConfig {
        /// The offending field, dotted (`coupling.capture_radius`).
        field: &'static str,
    },
    /// A trailer wheel is driven: a trailer has no engine.
    #[error("trailer wheel {index} has `drive: true`; trailer wheels are unpowered")]
    DrivenWheel {
        /// The wheel's index in the chassis' wheels.
        index: usize,
    },
    /// A trailer wheel steers: the trailer follows its coupling.
    #[error("trailer wheel {index} has `steer: true`; trailer wheels do not steer")]
    SteeredWheel {
        /// The wheel's index in the chassis' wheels.
        index: usize,
    },
    /// The hitch is farther from the coupling point than the capture radius.
    #[error("the hitch is {distance:.3} m from the coupling; the capture radius is {capture_radius:.3} m")]
    OutOfReach {
        /// Distance between the hitch and the coupling point, meters.
        distance: f32,
        /// The configured capture radius, meters.
        capture_radius: f32,
    },
    /// The trailer is already coupled.
    #[error("the trailer is already coupled; uncouple it first")]
    AlreadyCoupled,
    /// The trailer is not coupled.
    #[error("the trailer is not coupled")]
    NotCoupled,
    /// The tow body is not in the physics world (or went away while coupled).
    #[error("the tow body is not in the physics world")]
    TowMissing,
    /// The tow entity has no `TowHitch`.
    #[error("the tow entity has no TowHitch")]
    NoHitch,
    /// Another trailer already hangs on that hitch.
    #[error("another trailer is already coupled to that hitch")]
    HitchOccupied,
    /// A trailer cannot tow itself.
    #[error("a trailer cannot be coupled to itself")]
    SelfCoupling,
    /// Coupling would close a loop in the train.
    #[error("coupling would make the train a loop")]
    ChainCycle,
    /// Coupling would make the train longer than [`MAX_TRAIN_LENGTH`] trailers.
    #[error("the train would be longer than {MAX_TRAIN_LENGTH} trailers")]
    ChainTooLong,
    /// The entity carries no `TrailerController`.
    #[error("the entity is not a trailer")]
    NotATrailer,
    /// The app has no `PhysicsWorld` resource (add `PhysicsPlugin`).
    #[error("no PhysicsWorld in this app; add regia_physics::PhysicsPlugin")]
    NoPhysicsWorld,
}

impl TrailerError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs). A chassis
    /// problem reports the [`VehicleError`] code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Chassis(error) => error.code(),
            Self::InvalidConfig { .. } => "trailer_invalid_config",
            Self::DrivenWheel { .. } => "trailer_driven_wheel",
            Self::SteeredWheel { .. } => "trailer_steered_wheel",
            Self::OutOfReach { .. } => "trailer_out_of_reach",
            Self::AlreadyCoupled => "trailer_already_coupled",
            Self::NotCoupled => "trailer_not_coupled",
            Self::TowMissing => "trailer_tow_missing",
            Self::NoHitch => "trailer_no_hitch",
            Self::HitchOccupied => "trailer_hitch_occupied",
            Self::SelfCoupling => "trailer_self_coupling",
            Self::ChainCycle => "trailer_chain_cycle",
            Self::ChainTooLong => "trailer_chain_too_long",
            Self::NotATrailer => "trailer_not_a_trailer",
            Self::NoPhysicsWorld => "trailer_no_physics_world",
        }
    }
}

/// Every [`TrailerError`] code this module can report (the `VehicleError` codes of a chassis
/// problem aside), sorted. The agent guide documents each.
pub const TRAILER_ERROR_CODES: [&str; 14] = [
    "trailer_already_coupled",
    "trailer_chain_cycle",
    "trailer_chain_too_long",
    "trailer_driven_wheel",
    "trailer_hitch_occupied",
    "trailer_invalid_config",
    "trailer_no_hitch",
    "trailer_no_physics_world",
    "trailer_not_a_trailer",
    "trailer_not_coupled",
    "trailer_out_of_reach",
    "trailer_self_coupling",
    "trailer_steered_wheel",
    "trailer_tow_missing",
];

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The kind of coupling and its limits (radians, each half of a symmetric range).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CouplingKind {
    /// A ball on the tow vehicle: free in every rotation within the limits.
    TowBall {
        /// The jackknife stop: the largest articulation angle, (0, [`MAX_YAW_LIMIT`]].
        yaw_limit: f32,
        /// The largest relative pitch, (0, 1].
        pitch_limit: f32,
        /// The largest relative roll, (0, 1].
        roll_limit: f32,
    },
    /// A fifth wheel (or a dolly's turntable): turns about the vertical up to the yaw stop,
    /// tilts fore and aft within the pitch limit, no roll.
    FifthWheel {
        /// The jackknife stop: the largest articulation angle, (0, [`MAX_YAW_LIMIT`]].
        yaw_limit: f32,
        /// The largest relative pitch, (0, 1].
        pitch_limit: f32,
    },
}

impl Default for CouplingKind {
    /// A tow ball with a 75° jackknife stop and 20° of pitch and roll.
    fn default() -> Self {
        Self::TowBall {
            yaw_limit: 75.0_f32.to_radians(),
            pitch_limit: 20.0_f32.to_radians(),
            roll_limit: 20.0_f32.to_radians(),
        }
    }
}

impl CouplingKind {
    /// The jackknife stop, radians.
    #[must_use]
    pub fn yaw_limit(&self) -> f32 {
        match *self {
            Self::TowBall { yaw_limit, .. } | Self::FifthWheel { yaw_limit, .. } => yaw_limit,
        }
    }

    /// `"tow_ball"` or `"fifth_wheel"` (agent protocol).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::TowBall { .. } => "tow_ball",
            Self::FifthWheel { .. } => "fifth_wheel",
        }
    }

    /// The first field out of range, if any.
    fn invalid_field(&self) -> Option<&'static str> {
        let angle = |value: f32, max: f32| value.is_finite() && value > 0.0 && value <= max;
        match *self {
            Self::TowBall {
                yaw_limit,
                pitch_limit,
                roll_limit,
            } => [
                (angle(yaw_limit, MAX_YAW_LIMIT), "coupling.kind.yaw_limit"),
                (angle(pitch_limit, 1.0), "coupling.kind.pitch_limit"),
                (angle(roll_limit, 1.0), "coupling.kind.roll_limit"),
            ]
            .into_iter()
            .find(|(ok, _)| !ok)
            .map(|(_, field)| field),
            Self::FifthWheel {
                yaw_limit,
                pitch_limit,
            } => [
                (angle(yaw_limit, MAX_YAW_LIMIT), "coupling.kind.yaw_limit"),
                (angle(pitch_limit, 1.0), "coupling.kind.pitch_limit"),
            ]
            .into_iter()
            .find(|(ok, _)| !ok)
            .map(|(_, field)| field),
        }
    }

    /// The rapier joint for this coupling: frame 1 on the tow body at `hitch`, frame 2 on the
    /// trailer at `point`, both with the chassis' own axes, so the joint angles are the
    /// trailer's rotation relative to the tow body. Contacts between the two bodies are off.
    fn joint(&self, hitch: Vec3, point: Vec3) -> rapier3d::dynamics::GenericJoint {
        let builder = match *self {
            Self::TowBall {
                yaw_limit,
                pitch_limit,
                roll_limit,
            } => GenericJointBuilder::new(JointAxesMask::LIN_AXES)
                .limits(JointAxis::AngX, [-pitch_limit, pitch_limit])
                .limits(JointAxis::AngY, [-yaw_limit, yaw_limit])
                .limits(JointAxis::AngZ, [-roll_limit, roll_limit]),
            Self::FifthWheel {
                yaw_limit,
                pitch_limit,
            } => GenericJointBuilder::new(JointAxesMask::LIN_AXES | JointAxesMask::ANG_Z)
                .limits(JointAxis::AngX, [-pitch_limit, pitch_limit])
                .limits(JointAxis::AngY, [-yaw_limit, yaw_limit]),
        };
        builder
            .local_anchor1(to_rapier_vec3(hitch))
            .local_anchor2(to_rapier_vec3(point))
            .contacts_enabled(false)
            .build()
    }
}

/// Where and how the trailer couples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CouplingConfig {
    /// The coupling kind and its limits.
    pub kind: CouplingKind,
    /// The coupling point in chassis space (the drawbar's eye or the kingpin), meters.
    pub point: Vec3,
    /// [`Trailer::couple`] needs the hitch within this distance of `point`, meters,
    /// (0, [`MAX_CAPTURE_RADIUS`]].
    pub capture_radius: f32,
    /// [`TrailerReadout::jackknife_warning`] turns on at this articulation angle, radians, in
    /// `[0, yaw limit]`.
    pub jackknife_warning_angle: f32,
}

impl Default for CouplingConfig {
    /// A tow-ball drawbar eye 2.45 m ahead of the chassis center, a 0.3 m capture radius and a
    /// warning at 55°.
    fn default() -> Self {
        Self {
            kind: CouplingKind::default(),
            point: Vec3::new(0.0, 0.0, -2.45),
            capture_radius: 0.3,
            jackknife_warning_angle: 55.0_f32.to_radians(),
        }
    }
}

/// The trailer's service brakes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TrailerBrakes {
    /// No service brakes (the parking brake still holds an uncoupled trailer).
    None,
    /// The trailer brakes with the tow vehicle's brake input (the train's front vehicle):
    /// `input × proportion`, `delay` seconds later.
    Follow {
        /// Share of the tow vehicle's brake input, `[0, 1]`.
        proportion: f32,
        /// Seconds before the trailer's brakes follow the input, `[0, MAX_BRAKE_DELAY]`; rounded
        /// to whole steps.
        delay: f32,
    },
    /// An inertia (surge) brake: the drawbar's push on the hitch, estimated from the trailer's
    /// deceleration, applies the brakes. Locked out below 0.5 m/s and in reverse.
    Surge {
        /// Push below which nothing brakes, newtons (≥ 0).
        threshold: f32,
        /// Push that brakes fully, newtons (> `threshold`).
        full_force: f32,
        /// The actuator's time constant, seconds, (0, 5].
        response: f32,
    },
}

impl Default for TrailerBrakes {
    /// Following the tow vehicle at 80 % after 0.1 s.
    fn default() -> Self {
        Self::Follow {
            proportion: 0.8,
            delay: 0.1,
        }
    }
}

impl TrailerBrakes {
    /// `"none"`, `"follow"` or `"surge"` (agent protocol).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Follow { .. } => "follow",
            Self::Surge { .. } => "surge",
        }
    }

    /// The first field out of range, if any.
    fn invalid_field(&self) -> Option<&'static str> {
        match *self {
            Self::None => None,
            Self::Follow { proportion, delay } => {
                if !(proportion.is_finite() && (0.0..=1.0).contains(&proportion)) {
                    Some("brakes.proportion")
                } else if !(delay.is_finite() && (0.0..=MAX_BRAKE_DELAY).contains(&delay)) {
                    Some("brakes.delay")
                } else {
                    None
                }
            }
            Self::Surge {
                threshold,
                full_force,
                response,
            } => {
                if !(threshold.is_finite() && threshold >= 0.0) {
                    Some("brakes.threshold")
                } else if !(full_force.is_finite() && full_force > threshold) {
                    Some("brakes.full_force")
                } else if !(response.is_finite() && response > 0.0 && response <= 5.0) {
                    Some("brakes.response")
                } else {
                    None
                }
            }
        }
    }
}

/// The stand (jockey wheel, prop stand or landing gear) the trailer rests on while uncoupled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StandConfig {
    /// The bottom of the stand's foot in chassis space, meters.
    pub foot: Vec3,
    /// Half the size of the foot's box, meters, (0, 1].
    pub half_size: f32,
}

impl Default for StandConfig {
    /// A 0.2 m foot near the drawbar, its bottom 4 cm above the ground while coupled at the
    /// default heights.
    fn default() -> Self {
        Self {
            foot: Vec3::new(0.0, -0.55, -2.1),
            half_size: 0.1,
        }
    }
}

/// A trailer: its chassis, coupling, brakes, stand and rear hitch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrailerConfig {
    /// The chassis and wheels, as a [`VehicleConfig`]: no wheel may drive or steer, and the
    /// drivetrain is `Simple`. `max_engine_force` is not used; `max_brake_force` is the service
    /// and parking brake per braking wheel.
    pub chassis: VehicleConfig,
    /// The coupling.
    pub coupling: CouplingConfig,
    /// The service brakes.
    pub brakes: TrailerBrakes,
    /// The parking brake applied while uncoupled, `[0, 1]`.
    pub parking_brake: f32,
    /// The stand the trailer rests on while uncoupled; `None` lets the coupling end drop to the
    /// ground (or onto the chassis box).
    pub stand: Option<StandConfig>,
    /// A hitch at the back for the next trailer or a dolly, chassis space; `None` tows nothing.
    pub rear_hitch: Option<Vec3>,
}

impl Default for TrailerConfig {
    /// [`TrailerConfig::car_trailer`].
    fn default() -> Self {
        Self::car_trailer()
    }
}

impl TrailerConfig {
    /// A 750 kg single-axle box trailer for a car (1.6 m × 0.6 m × 2.5 m box, 1.2 m drawbar),
    /// a tow-ball coupling at the drawbar's eye, follow brakes, a jockey stand. It fits the
    /// default car's [`car_tow_hitch`].
    #[must_use]
    pub fn car_trailer() -> Self {
        let wheel = |x: f32| WheelConfig {
            position: Vec3::new(x, -0.15, 0.2),
            radius: 0.3,
            suspension_rest_length: 0.25,
            suspension_travel: 0.2,
            suspension_stiffness: 40.0,
            suspension_compression_damping: 2.3,
            suspension_relaxation_damping: 3.3,
            steer: false,
            drive: false,
            brake: true,
            ..WheelConfig::default()
        };
        Self {
            chassis: VehicleConfig {
                chassis_half_extents: Vec3::new(0.8, 0.3, 1.25),
                chassis_mass: 750.0,
                center_of_mass: Vec3::new(0.0, -0.1, -0.1),
                max_engine_force: 0.0,
                max_brake_force: 2500.0,
                rolling_resistance: 30.0,
                max_steer_angle: 0.0,
                wheels: vec![wheel(-0.85), wheel(0.85)],
                ..VehicleConfig::default()
            },
            coupling: CouplingConfig::default(),
            brakes: TrailerBrakes::default(),
            parking_brake: 1.0,
            stand: Some(StandConfig::default()),
            rear_hitch: None,
        }
    }

    /// A 12 t, 13.6 m tri-axle semi-trailer on a fifth wheel (kingpin 1.2 m behind the front
    /// wall), follow brakes, landing gear. It fits [`tractor_config`]'s [`tractor_fifth_wheel`].
    #[must_use]
    pub fn semi_trailer() -> Self {
        let wheel = |x: f32, z: f32| WheelConfig {
            position: Vec3::new(x, -1.6, z),
            radius: 0.5,
            suspension_rest_length: 0.65,
            suspension_travel: 0.25,
            suspension_stiffness: 18.0,
            suspension_compression_damping: 2.3,
            suspension_relaxation_damping: 3.3,
            max_suspension_force: 200_000.0,
            friction_slip: 10.5,
            steer: false,
            drive: false,
            brake: true,
            ..WheelConfig::default()
        };
        let mut wheels = Vec::with_capacity(6);
        for z in [3.9, 4.8, 5.7] {
            wheels.push(wheel(-0.95, z));
            wheels.push(wheel(0.95, z));
        }
        Self {
            chassis: VehicleConfig {
                chassis_half_extents: Vec3::new(1.25, 1.0, 6.8),
                chassis_mass: 12_000.0,
                center_of_mass: Vec3::new(0.0, -0.3, 1.0),
                max_engine_force: 0.0,
                max_brake_force: 12_000.0,
                rolling_resistance: 150.0,
                max_steer_angle: 0.0,
                wheels,
                ..VehicleConfig::default()
            },
            coupling: CouplingConfig {
                kind: CouplingKind::FifthWheel {
                    yaw_limit: 80.0_f32.to_radians(),
                    pitch_limit: 12.0_f32.to_radians(),
                },
                point: Vec3::new(0.0, -1.0, -5.6),
                capture_radius: 0.3,
                jackknife_warning_angle: 60.0_f32.to_radians(),
            },
            brakes: TrailerBrakes::Follow {
                proportion: 1.0,
                delay: 0.25,
            },
            parking_brake: 1.0,
            stand: Some(StandConfig {
                foot: Vec3::new(0.0, -2.62, -4.0),
                half_size: 0.2,
            }),
            rear_hitch: None,
        }
    }

    /// A 1.5 t converter dolly: a short single-axle drawbar trailer with a fifth wheel on top
    /// ([`TrailerConfig::rear_hitch`]) for a [`TrailerConfig::semi_trailer`]. Couple its tow
    /// ball to a semi-trailer's `rear_hitch` to make a road train.
    #[must_use]
    pub fn dolly() -> Self {
        let wheel = |x: f32| WheelConfig {
            position: Vec3::new(x, -0.3, 0.0),
            radius: 0.5,
            suspension_rest_length: 0.45,
            suspension_travel: 0.25,
            suspension_stiffness: 120.0,
            suspension_compression_damping: 4.0,
            suspension_relaxation_damping: 5.0,
            max_suspension_force: 200_000.0,
            steer: false,
            drive: false,
            brake: true,
            ..WheelConfig::default()
        };
        Self {
            chassis: VehicleConfig {
                chassis_half_extents: Vec3::new(1.1, 0.25, 1.0),
                chassis_mass: 1500.0,
                center_of_mass: Vec3::new(0.0, -0.1, 0.0),
                max_engine_force: 0.0,
                max_brake_force: 8000.0,
                rolling_resistance: 60.0,
                max_steer_angle: 0.0,
                wheels: vec![wheel(-0.95), wheel(0.95)],
                ..VehicleConfig::default()
            },
            coupling: CouplingConfig {
                kind: CouplingKind::TowBall {
                    yaw_limit: 80.0_f32.to_radians(),
                    pitch_limit: 25.0_f32.to_radians(),
                    roll_limit: 10.0_f32.to_radians(),
                },
                point: Vec3::new(0.0, -0.2, -3.0),
                capture_radius: 0.3,
                jackknife_warning_angle: 60.0_f32.to_radians(),
            },
            brakes: TrailerBrakes::Follow {
                proportion: 1.0,
                delay: 0.35,
            },
            parking_brake: 1.0,
            stand: Some(StandConfig {
                foot: Vec3::new(0.0, -0.75, -2.6),
                half_size: 0.1,
            }),
            rear_hitch: Some(Vec3::new(0.0, 0.5, 0.0)),
        }
    }

    /// Checks every field: `Ok` when [`Trailer::spawn`] would accept the configuration.
    ///
    /// The chassis first ([`VehicleConfig::validate`]), then its wheels for drive and steer,
    /// then the coupling, the brakes, the parking brake, the stand and the rear hitch.
    ///
    /// # Errors
    /// The first problem found.
    pub fn validate(&self) -> Result<(), TrailerError> {
        self.chassis.validate()?;
        if self.chassis.drivetrain != Drivetrain::Simple {
            return Err(TrailerError::InvalidConfig {
                field: "chassis.drivetrain",
            });
        }
        if let Some(index) = self.chassis.wheels.iter().position(|wheel| wheel.drive) {
            return Err(TrailerError::DrivenWheel { index });
        }
        if let Some(index) = self.chassis.wheels.iter().position(|wheel| wheel.steer) {
            return Err(TrailerError::SteeredWheel { index });
        }
        let coupling = &self.coupling;
        let invalid = |field| Err(TrailerError::InvalidConfig { field });
        if let Some(field) = coupling.kind.invalid_field() {
            return invalid(field);
        }
        if !coupling.point.is_finite() {
            return invalid("coupling.point");
        }
        if !(coupling.capture_radius.is_finite()
            && coupling.capture_radius > 0.0
            && coupling.capture_radius <= MAX_CAPTURE_RADIUS)
        {
            return invalid("coupling.capture_radius");
        }
        let warning = coupling.jackknife_warning_angle;
        if !(warning.is_finite() && warning >= 0.0 && warning <= coupling.kind.yaw_limit()) {
            return invalid("coupling.jackknife_warning_angle");
        }
        if let Some(field) = self.brakes.invalid_field() {
            return invalid(field);
        }
        if !(self.parking_brake.is_finite() && (0.0..=1.0).contains(&self.parking_brake)) {
            return invalid("parking_brake");
        }
        if let Some(stand) = &self.stand {
            if !stand.foot.is_finite() {
                return invalid("stand.foot");
            }
            if !(stand.half_size.is_finite() && stand.half_size > 0.0 && stand.half_size <= 1.0) {
                return invalid("stand.half_size");
            }
        }
        if self.rear_hitch.is_some_and(|hitch| !hitch.is_finite()) {
            return invalid("rear_hitch");
        }
        Ok(())
    }
}

/// The hitch of [`VehicleConfig::default`]'s car: a tow ball 0.3 m behind the rear of the
/// chassis box, 0.25 m below its center line.
#[must_use]
pub fn car_tow_hitch() -> Vec3 {
    Vec3::new(0.0, -0.25, 2.4)
}

/// An 8 t three-axle (6×4) tractor unit for [`TrailerConfig::semi_trailer`]: steered front
/// axle, two driven rear axles, brakes on all six wheels.
#[must_use]
pub fn tractor_config() -> VehicleConfig {
    let wheel = |x: f32, z: f32, front: bool| WheelConfig {
        position: Vec3::new(x, -0.3, z),
        radius: 0.5,
        suspension_rest_length: 0.4,
        suspension_travel: 0.25,
        suspension_stiffness: 40.0,
        suspension_compression_damping: 2.3,
        suspension_relaxation_damping: 3.3,
        max_suspension_force: 200_000.0,
        friction_slip: 10.5,
        steer: front,
        drive: !front,
        brake: true,
        ..WheelConfig::default()
    };
    VehicleConfig {
        chassis_half_extents: Vec3::new(1.25, 0.5, 3.0),
        chassis_mass: 8000.0,
        center_of_mass: Vec3::new(0.0, -0.3, -0.5),
        max_engine_force: 9000.0,
        max_brake_force: 14_000.0,
        rolling_resistance: 120.0,
        max_steer_angle: 0.6,
        wheels: vec![
            wheel(-1.0, -2.0, true),
            wheel(1.0, -2.0, true),
            wheel(-1.0, 1.2, false),
            wheel(1.0, 1.2, false),
            wheel(-1.0, 2.5, false),
            wheel(1.0, 2.5, false),
        ],
        ..VehicleConfig::default()
    }
}

/// The fifth wheel of [`tractor_config`]: between the rear axles, on top of the frame.
#[must_use]
pub fn tractor_fifth_wheel() -> Vec3 {
    Vec3::new(0.0, 0.55, 1.85)
}

// ---------------------------------------------------------------------------
// Readout
// ---------------------------------------------------------------------------

/// What a trailer is doing, for games and AI drivers. All zeros (and `coupled: false`) while
/// uncoupled, except `brake` and `forward_speed`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Reflect)]
pub struct TrailerReadout {
    /// The trailer is on a hitch.
    pub coupled: bool,
    /// The trailer's yaw relative to the tow body about the tow body's up axis, radians;
    /// positive when the trailer points to the tow's left.
    pub articulation_angle: f32,
    /// The articulation angle's rate, rad/s, from the two bodies' angular velocities.
    pub articulation_rate: f32,
    /// The trailer's pitch relative to the tow body, radians; positive nose up.
    pub relative_pitch: f32,
    /// `|articulation_angle| / yaw limit`: 1 at the jackknife stop.
    pub jackknife_ratio: f32,
    /// `|articulation_angle|` has reached the configured `jackknife_warning_angle`.
    pub jackknife_warning: bool,
    /// The brake applied at the last update, 0 to 1.
    pub brake: f32,
    /// Distance between the hitch and the coupling point, meters (the joint's error; near 0).
    pub coupling_gap: f32,
    /// The trailer's speed along its forward axis, m/s.
    pub forward_speed: f32,
}

// ---------------------------------------------------------------------------
// Trailer
// ---------------------------------------------------------------------------

/// The live coupling.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Coupled {
    /// The tow body.
    tow: rapier3d::dynamics::RigidBodyHandle,
    /// The hitch point in the tow body's space.
    hitch: Vec3,
    /// The coupling joint.
    joint: ImpulseJointHandle,
}

/// A trailer in a [`PhysicsWorld`]: a [`Vehicle`] chassis on unpowered wheels, its coupling and
/// its brakes.
///
/// Never panics on a bad world: a chassis that was removed reports
/// [`VehicleError::ChassisMissing`] (as [`TrailerError::Chassis`]); a tow body that was removed
/// while coupled uncouples the trailer and reports [`TrailerError::TowMissing`] once.
#[derive(Debug, Clone)]
pub struct Trailer {
    /// The chassis and wheels.
    vehicle: Vehicle,
    /// The validated configuration.
    config: TrailerConfig,
    /// The coupling, while coupled.
    coupled: Option<Coupled>,
    /// The stand's collider (enabled only while uncoupled).
    stand: Option<rapier3d::geometry::ColliderHandle>,
    /// Follow-brake delay line: the brake input of the last [`BRAKE_DELAY_STEPS`] updates.
    delay_line: [f32; BRAKE_DELAY_STEPS],
    /// The delay line's next write slot.
    delay_head: usize,
    /// The surge brake's actuator level, 0 to 1.
    surge_level: f32,
    /// The forward speed at the last update (surge estimate).
    last_forward_speed: Option<f32>,
    /// The brake applied at the last update, 0 to 1.
    applied_brake: f32,
}

/// A body's pose and velocities in engine types.
struct BodyKinematics {
    translation: Vec3,
    rotation: Quat,
    linear: Vec3,
    angular: Vec3,
}

fn kinematics(physics: &PhysicsWorld, handle: rapier3d::dynamics::RigidBodyHandle) -> Option<BodyKinematics> {
    let body = physics.bodies.get(handle)?;
    let (translation, rotation) = from_rapier_pose(body.position());
    Some(BodyKinematics {
        translation,
        rotation,
        linear: from_rapier_vec3(body.linvel()),
        angular: from_rapier_vec3(body.angvel()),
    })
}

/// The trailer's yaw and pitch relative to the tow body: `(yaw about the tow's +Y, pitch)`,
/// radians. Yaw is positive when the trailer's forward turns toward the tow's left.
#[must_use]
pub fn relative_yaw_pitch(tow_rotation: Quat, trailer_rotation: Quat) -> (f32, f32) {
    let forward = (tow_rotation.inverse() * trailer_rotation) * FORWARD;
    // forward = (−sin yaw, ·, −cos yaw) for −Z turned by `yaw` about +Y.
    let yaw = (-forward.x).atan2(-forward.z);
    let horizontal = (forward.x * forward.x + forward.z * forward.z).sqrt();
    let pitch = forward.y.atan2(horizontal);
    (yaw, pitch)
}

impl Trailer {
    /// Creates an uncoupled trailer: its chassis at the given world pose (the box's center), its
    /// stand down and its parking brake on.
    ///
    /// # Errors
    /// [`TrailerError`] from [`TrailerConfig::validate`] or a bad pose; nothing is added then.
    pub fn spawn(
        physics: &mut PhysicsWorld,
        config: TrailerConfig,
        translation: Vec3,
        rotation: Quat,
    ) -> Result<Self, TrailerError> {
        config.validate()?;
        checked_pose(translation, rotation)?;
        let vehicle = Vehicle::spawn(physics, config.chassis.clone(), translation, rotation)?;
        let stand = config.stand.map(|stand| {
            let collider = ColliderBuilder::cuboid(stand.half_size, stand.half_size, stand.half_size)
                .translation(to_rapier_vec3(stand.foot + Vec3::Y * stand.half_size))
                .density(0.0)
                .friction(1.0)
                .build();
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            colliders.insert_with_parent(collider, vehicle.chassis().to_handle(), bodies)
        });
        Ok(Self {
            vehicle,
            config,
            coupled: None,
            stand,
            delay_line: [0.0; BRAKE_DELAY_STEPS],
            delay_head: 0,
            surge_level: 0.0,
            last_forward_speed: None,
            applied_brake: 0.0,
        })
    }

    /// Creates a trailer lined up behind a tow body, its coupling point on the tow body's hitch
    /// point and its rotation the tow body's, and couples it.
    ///
    /// # Errors
    /// [`TrailerError::TowMissing`] for a tow body not in the world, or what [`Trailer::spawn`]
    /// refuses; nothing is added then.
    pub fn spawn_coupled(
        physics: &mut PhysicsWorld,
        config: TrailerConfig,
        tow: RigidBodyHandle,
        hitch: Vec3,
    ) -> Result<Self, TrailerError> {
        if !hitch.is_finite() {
            return Err(TrailerError::InvalidConfig { field: "hitch" });
        }
        let tow_pose = kinematics(physics, tow.to_handle()).ok_or(TrailerError::TowMissing)?;
        let rotation = tow_pose.rotation;
        let at = tow_pose.translation + rotation * hitch - rotation * config.coupling.point;
        let mut trailer = Self::spawn(physics, config, at, rotation)?;
        let velocity = tow_pose.linear;
        if let Some(body) = physics.bodies.get_mut(trailer.chassis().to_handle()) {
            body.set_linvel(to_rapier_vec3(velocity), true);
        }
        trailer.couple(physics, tow, hitch)?;
        Ok(trailer)
    }

    /// The chassis body.
    #[must_use]
    pub fn chassis(&self) -> RigidBodyHandle {
        self.vehicle.chassis()
    }

    /// The chassis vehicle (wheel states, tire forces, digest).
    #[must_use]
    pub fn vehicle(&self) -> &Vehicle {
        &self.vehicle
    }

    /// The configuration the trailer was built with.
    #[must_use]
    pub fn config(&self) -> &TrailerConfig {
        &self.config
    }

    /// The stand's collider, if the configuration has a stand.
    #[must_use]
    pub fn stand_collider(&self) -> Option<ColliderHandle> {
        self.stand.map(ColliderHandle::from_rapier)
    }

    /// The tow body, while coupled.
    #[must_use]
    pub fn tow(&self) -> Option<RigidBodyHandle> {
        self.coupled
            .map(|coupled| RigidBodyHandle::from_rapier(coupled.tow))
    }

    /// The hitch point in the tow body's space, while coupled.
    #[must_use]
    pub fn hitch(&self) -> Option<Vec3> {
        self.coupled.map(|coupled| coupled.hitch)
    }

    /// The trailer is on a hitch.
    #[must_use]
    pub fn is_coupled(&self) -> bool {
        self.coupled.is_some()
    }

    /// Distance between the tow body's hitch point and this trailer's coupling point, meters.
    ///
    /// # Errors
    /// [`TrailerError::TowMissing`] or the chassis' [`VehicleError::ChassisMissing`].
    pub fn hitch_distance(
        &self,
        physics: &PhysicsWorld,
        tow: RigidBodyHandle,
        hitch: Vec3,
    ) -> Result<f32, TrailerError> {
        let tow_pose = kinematics(physics, tow.to_handle()).ok_or(TrailerError::TowMissing)?;
        let own = kinematics(physics, self.chassis().to_handle()).ok_or(VehicleError::ChassisMissing)?;
        let hitch_world = tow_pose.translation + tow_pose.rotation * hitch;
        let point_world = own.translation + own.rotation * self.config.coupling.point;
        Ok(hitch_world.distance(point_world))
    }

    /// Couples the trailer to `hitch` (in `tow`'s body space): the hitch must be within the
    /// capture radius of the coupling point. The joint then closes the remaining gap; the stand
    /// comes up and the parking brake releases.
    ///
    /// # Errors
    /// [`TrailerError::AlreadyCoupled`], [`TrailerError::SelfCoupling`] (the tow body is this
    /// chassis), [`TrailerError::TowMissing`], [`TrailerError::InvalidConfig`] for a non-finite
    /// hitch, [`TrailerError::OutOfReach`]. Nothing changes on error.
    pub fn couple(
        &mut self,
        physics: &mut PhysicsWorld,
        tow: RigidBodyHandle,
        hitch: Vec3,
    ) -> Result<(), TrailerError> {
        if self.coupled.is_some() {
            return Err(TrailerError::AlreadyCoupled);
        }
        if tow.to_handle() == self.chassis().to_handle() {
            return Err(TrailerError::SelfCoupling);
        }
        if !hitch.is_finite() {
            return Err(TrailerError::InvalidConfig { field: "hitch" });
        }
        let distance = self.hitch_distance(physics, tow, hitch)?;
        let capture_radius = self.config.coupling.capture_radius;
        if distance > capture_radius {
            return Err(TrailerError::OutOfReach {
                distance,
                capture_radius,
            });
        }
        let joint = self.config.coupling.kind.joint(hitch, self.config.coupling.point);
        let handle = physics
            .impulse_joints
            .insert(tow.to_handle(), self.chassis().to_handle(), joint, true);
        self.coupled = Some(Coupled {
            tow: tow.to_handle(),
            hitch,
            joint: handle,
        });
        self.set_stand(physics, false);
        self.reset_brake_state();
        Ok(())
    }

    /// Uncouples the trailer: the joint goes, the stand comes down and the parking brake holds.
    ///
    /// # Errors
    /// [`TrailerError::NotCoupled`].
    pub fn uncouple(&mut self, physics: &mut PhysicsWorld) -> Result<(), TrailerError> {
        let Some(coupled) = self.coupled.take() else {
            return Err(TrailerError::NotCoupled);
        };
        if physics.impulse_joints.contains(coupled.joint) {
            physics.impulse_joints.remove(coupled.joint, true);
        }
        self.set_stand(physics, true);
        self.reset_brake_state();
        Ok(())
    }

    /// Enables or disables the stand's collider.
    fn set_stand(&mut self, physics: &mut PhysicsWorld, down: bool) {
        if let Some(collider) = self.stand.and_then(|handle| physics.colliders.get_mut(handle)) {
            collider.set_enabled(down);
        }
        if let Some(body) = physics.bodies.get_mut(self.chassis().to_handle()) {
            body.wake_up(true);
        }
    }

    /// Clears the delay line and the surge state (on couple and uncouple).
    fn reset_brake_state(&mut self) {
        self.delay_line = [0.0; BRAKE_DELAY_STEPS];
        self.delay_head = 0;
        self.surge_level = 0.0;
        self.last_forward_speed = None;
    }

    /// Re-expresses the chassis vehicle's stored spawn pose and contacts after a floating-origin
    /// rebase (see [`Vehicle::shift_origin`]).
    pub fn shift_origin(&mut self, shift: Vec3) {
        self.vehicle.shift_origin(shift);
    }

    /// The service brake for this step from the tow vehicle's brake input (0 to 1).
    fn service_brake(&mut self, tow_brake: f32, forward_speed: f32, dt: f32) -> f32 {
        match self.config.brakes {
            TrailerBrakes::None => 0.0,
            TrailerBrakes::Follow { proportion, delay } => {
                self.delay_line[self.delay_head] = tow_brake;
                let lag = ((delay / dt).round() as usize).min(BRAKE_DELAY_STEPS - 1);
                let read = (self.delay_head + BRAKE_DELAY_STEPS - lag) % BRAKE_DELAY_STEPS;
                self.delay_head = (self.delay_head + 1) % BRAKE_DELAY_STEPS;
                self.delay_line[read] * proportion
            }
            TrailerBrakes::Surge {
                threshold,
                full_force,
                response,
            } => {
                let config = &self.config.chassis;
                let braked = config.wheels.iter().filter(|wheel| wheel.brake).count() as f32;
                let rolling = config.wheels.iter().filter(|wheel| !wheel.brake).count() as f32;
                let deceleration = self
                    .last_forward_speed
                    .map_or(0.0, |previous| (previous - forward_speed) / dt);
                // The drawbar's push: what slows the trailer beyond its own brakes and rolling.
                let own =
                    self.surge_level * config.max_brake_force * braked + config.rolling_resistance * rolling;
                let push = config.chassis_mass * deceleration - own;
                let demand = if forward_speed > SURGE_MIN_SPEED {
                    ((push - threshold) / (full_force - threshold)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                self.surge_level += (demand - self.surge_level) * (dt / response).min(1.0);
                self.surge_level
            }
        }
    }

    /// Applies one step: the brakes (the tow vehicle's brake input `tow_brake`, 0 to 1, through
    /// the configured brake model; the parking brake while uncoupled), then the chassis' wheel
    /// rays and impulses. Call once per fixed step, after the tow vehicle's update and before the
    /// world steps.
    ///
    /// # Errors
    /// The chassis' [`VehicleError`] (the world is unchanged then), or
    /// [`TrailerError::TowMissing`] when the tow body went away: the trailer is uncoupled first
    /// and stands on its stand from this step on.
    pub fn update(
        &mut self,
        physics: &mut PhysicsWorld,
        tow_brake: f32,
        dt: f32,
        layers: Option<&PhysicsLayers>,
    ) -> Result<(), TrailerError> {
        let _span = debug_span!("trailer_update").entered();
        if !(dt.is_finite() && dt > 0.0) {
            return Err(VehicleError::InvalidTimeStep.into());
        }
        let own = kinematics(physics, self.chassis().to_handle()).ok_or(VehicleError::ChassisMissing)?;
        let mut lost_tow = false;
        if let Some(coupled) = self.coupled
            && (!physics.bodies.contains(coupled.tow) || !physics.impulse_joints.contains(coupled.joint))
        {
            // The tow body (and with it the joint) is gone.
            self.coupled = None;
            self.set_stand(physics, true);
            self.reset_brake_state();
            lost_tow = true;
        }
        let tow_brake = if tow_brake.is_finite() {
            tow_brake.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let forward_speed = own.linear.dot(own.rotation * FORWARD);
        let brake = if self.coupled.is_some() {
            self.service_brake(tow_brake, forward_speed, dt)
        } else {
            self.config.parking_brake
        };
        self.last_forward_speed = Some(forward_speed);
        let input = VehicleInput {
            throttle: 0.0,
            brake,
            steer: 0.0,
        };
        self.vehicle.update_with_layers(physics, input, dt, layers)?;
        self.applied_brake = brake;
        if lost_tow {
            return Err(TrailerError::TowMissing);
        }
        Ok(())
    }

    /// The articulation, jackknife, brake and speed readout after the last update.
    ///
    /// # Errors
    /// The chassis' [`VehicleError::ChassisMissing`].
    pub fn readout(&self, physics: &PhysicsWorld) -> Result<TrailerReadout, TrailerError> {
        let own = kinematics(physics, self.chassis().to_handle()).ok_or(VehicleError::ChassisMissing)?;
        let mut readout = TrailerReadout {
            brake: self.applied_brake,
            forward_speed: own.linear.dot(own.rotation * FORWARD),
            ..TrailerReadout::default()
        };
        let Some(coupled) = self.coupled else {
            return Ok(readout);
        };
        let Some(tow) = kinematics(physics, coupled.tow) else {
            return Ok(readout);
        };
        let (yaw, pitch) = relative_yaw_pitch(tow.rotation, own.rotation);
        let up = tow.rotation * Vec3::Y;
        let limit = self.config.coupling.kind.yaw_limit();
        readout.coupled = true;
        readout.articulation_angle = yaw;
        readout.articulation_rate = (own.angular - tow.angular).dot(up);
        readout.relative_pitch = pitch;
        readout.jackknife_ratio = yaw.abs() / limit;
        readout.jackknife_warning = yaw.abs() >= self.config.coupling.jackknife_warning_angle;
        let hitch = tow.translation + tow.rotation * coupled.hitch;
        let point = own.translation + own.rotation * self.config.coupling.point;
        readout.coupling_gap = hitch.distance(point);
        Ok(readout)
    }

    /// A digest of the chassis vehicle's state plus the brake state: two runs with the same
    /// inputs give the same value. Compare it with `PhysicsWorld::state_digest` for the bodies.
    #[must_use]
    pub fn state_digest(&self) -> u64 {
        let mut hash = self.vehicle.state_digest() ^ 0xcbf2_9ce4_8422_2325;
        let words = [
            self.applied_brake.to_bits(),
            self.surge_level.to_bits(),
            u32::from(self.coupled.is_some()),
            self.delay_head as u32,
        ];
        for word in words {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    /// Removes the coupling joint (if any) and the chassis body with its colliders. Returns
    /// `false` when the chassis was already gone.
    pub fn remove(mut self, physics: &mut PhysicsWorld) -> bool {
        if let Some(coupled) = self.coupled.take()
            && physics.impulse_joints.contains(coupled.joint)
        {
            physics.impulse_joints.remove(coupled.joint, true);
        }
        self.vehicle.remove(physics)
    }
}

// ---------------------------------------------------------------------------
// ECS
// ---------------------------------------------------------------------------

/// A hitch on a tow entity (a vehicle, a trailer with a rear hitch, a dolly): the point in the
/// entity's body space that a trailer couples to.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct TowHitch {
    /// The hitch point in the tow body's space, meters.
    pub point: Vec3,
}

/// A trailer on an ECS entity, updated by [`TrailerPlugin`].
#[derive(Component, Debug, Clone)]
pub struct TrailerController {
    /// The trailer (read it with [`Trailer::readout`] against the `PhysicsWorld` resource).
    pub trailer: Trailer,
    /// The tow entity, while coupled through [`couple_trailer`].
    pub tow: Option<Entity>,
    /// The error from the last fixed-step update, `None` when it succeeded.
    pub last_error: Option<TrailerError>,
}

/// The readout of a trailer entity, written every fixed step by [`TrailerPlugin`]: what games
/// and AI drivers read.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct TrailerState {
    /// Articulation, jackknife, brake and speed.
    pub readout: TrailerReadout,
    /// The tow entity, while coupled.
    pub tow: Option<Entity>,
    /// The train's front entity, while coupled: the tow vehicle whose `VehicleInput` brake the
    /// train follows, or an uncoupled trailer heading the rest (no brake input then).
    pub root: Option<Entity>,
    /// Place in the train: 1 behind the front vehicle, 2 behind that trailer, …; 0 uncoupled.
    pub train_position: u32,
}

/// Creates a trailer in the app's `PhysicsWorld` and an entity that owns it, uncoupled.
///
/// The entity gets the chassis' [`RigidBodyHandle`] and [`ColliderHandle`], a `Transform` at the
/// pose, a [`TrailerController`], a [`TrailerState`] and, when the configuration has a
/// `rear_hitch`, a [`TowHitch`]. Despawning the entity removes the chassis from the world.
///
/// # Errors
/// [`TrailerError::NoPhysicsWorld`] or what [`Trailer::spawn`] refuses; nothing is spawned then.
pub fn spawn_trailer(
    world: &mut World,
    config: TrailerConfig,
    translation: Vec3,
    rotation: Quat,
) -> Result<Entity, TrailerError> {
    let trailer = {
        let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() else {
            return Err(TrailerError::NoPhysicsWorld);
        };
        Trailer::spawn(&mut physics, config, translation, rotation)?
    };
    let (translation, rotation) = trailer.vehicle().spawn_pose();
    let chassis = trailer.chassis();
    let collider = trailer.vehicle().chassis_collider();
    let stand = trailer.stand_collider();
    let rear_hitch = trailer.config().rear_hitch;
    let mut entity = world.spawn((
        chassis,
        Transform {
            translation,
            rotation,
            scale: Vec3::ONE,
        },
        TrailerController {
            trailer,
            tow: None,
            last_error: None,
        },
        TrailerState::default(),
    ));
    if let Some(collider) = collider {
        entity.insert(collider);
    }
    if let Some(point) = rear_hitch {
        entity.insert(TowHitch { point });
    }
    let entity = entity.id();
    if let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() {
        physics.register_body(chassis.to_handle(), entity);
        for collider in [collider, stand].into_iter().flatten() {
            physics.register_collider(collider.to_handle(), entity);
        }
    }
    Ok(entity)
}

/// The tow link of a trailer entity: `Some(Some(tow))` coupled, `Some(None)` uncoupled, `None`
/// not a trailer.
fn tow_of(world: &World, entity: Entity) -> Option<Option<Entity>> {
    world
        .get::<TrailerController>(entity)
        .map(|controller| controller.tow)
}

/// The number of trailers from the train's front vehicle back to `entity` (0 when `entity` is
/// not a trailer); `Err(ChainCycle)` when the walk comes back to `start`.
fn depth_to_front(world: &World, entity: Entity, start: Entity) -> Result<usize, TrailerError> {
    let mut depth = 0;
    let mut at = entity;
    while let Some(link) = tow_of(world, at) {
        depth += 1;
        if depth > MAX_TRAIN_LENGTH + 1 {
            return Err(TrailerError::ChainTooLong);
        }
        match link {
            Some(next) if next == start => return Err(TrailerError::ChainCycle),
            Some(next) => at = next,
            None => break,
        }
    }
    Ok(depth)
}

/// The number of trailers in the longest chain behind `entity`, counting `entity`.
fn depth_behind(world: &mut World, entity: Entity) -> usize {
    let mut query = world.query::<(Entity, &TrailerController)>();
    let links: Vec<(Entity, Option<Entity>)> = query
        .iter(world)
        .map(|(trailer, controller)| (trailer, controller.tow))
        .collect();
    let mut frontier = vec![entity];
    let mut depth = 0;
    while !frontier.is_empty() && depth <= MAX_TRAIN_LENGTH {
        depth += 1;
        frontier = links
            .iter()
            .filter(|(_, tow)| tow.is_some_and(|tow| frontier.contains(&tow)))
            .map(|(trailer, _)| *trailer)
            .collect();
    }
    depth
}

/// Couples the trailer entity to the tow entity's [`TowHitch`].
///
/// # Errors
/// [`TrailerError::NotATrailer`], [`TrailerError::SelfCoupling`], [`TrailerError::NoHitch`],
/// [`TrailerError::TowMissing`] (the tow entity has no body), [`TrailerError::HitchOccupied`],
/// [`TrailerError::ChainCycle`], [`TrailerError::ChainTooLong`], [`TrailerError::NoPhysicsWorld`]
/// and what [`Trailer::couple`] refuses (such as [`TrailerError::OutOfReach`]). Nothing changes on
/// error.
pub fn couple_trailer(world: &mut World, trailer: Entity, tow: Entity) -> Result<(), TrailerError> {
    if world.get::<TrailerController>(trailer).is_none() {
        return Err(TrailerError::NotATrailer);
    }
    if trailer == tow {
        return Err(TrailerError::SelfCoupling);
    }
    if world
        .get::<TrailerController>(trailer)
        .is_some_and(|c| c.trailer.is_coupled())
    {
        return Err(TrailerError::AlreadyCoupled);
    }
    let hitch = world.get::<TowHitch>(tow).copied().ok_or(TrailerError::NoHitch)?;
    let tow_body = world
        .get::<RigidBodyHandle>(tow)
        .copied()
        .ok_or(TrailerError::TowMissing)?;
    let mut occupied = world.query::<&TrailerController>();
    if occupied.iter(world).any(|controller| controller.tow == Some(tow)) {
        return Err(TrailerError::HitchOccupied);
    }
    let front = depth_to_front(world, tow, trailer)?;
    let behind = depth_behind(world, trailer);
    if front + behind > MAX_TRAIN_LENGTH {
        return Err(TrailerError::ChainTooLong);
    }
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(TrailerError::NoPhysicsWorld);
    }
    world.resource_scope(|world, mut physics: Mut<PhysicsWorld>| {
        let Some(mut controller) = world.get_mut::<TrailerController>(trailer) else {
            return Err(TrailerError::NotATrailer);
        };
        controller.trailer.couple(&mut physics, tow_body, hitch.point)?;
        controller.tow = Some(tow);
        controller.last_error = None;
        Ok(())
    })
}

/// Uncouples the trailer entity: its stand comes down and its parking brake holds.
///
/// # Errors
/// [`TrailerError::NotATrailer`], [`TrailerError::NotCoupled`], [`TrailerError::NoPhysicsWorld`].
pub fn uncouple_trailer(world: &mut World, trailer: Entity) -> Result<(), TrailerError> {
    if world.get::<TrailerController>(trailer).is_none() {
        return Err(TrailerError::NotATrailer);
    }
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(TrailerError::NoPhysicsWorld);
    }
    world.resource_scope(|world, mut physics: Mut<PhysicsWorld>| {
        let Some(mut controller) = world.get_mut::<TrailerController>(trailer) else {
            return Err(TrailerError::NotATrailer);
        };
        controller.trailer.uncouple(&mut physics)?;
        controller.tow = None;
        Ok(())
    })
}

/// One trailer's work for a step, gathered by [`drive_trailers`] before any trailer is updated
/// (its reused buffer).
#[derive(Debug, Clone, Copy)]
pub struct TrailerWork {
    key: regia_ecs::spawn_order::SpawnKey,
    entity: Entity,
    root: Option<Entity>,
    position: u32,
    brake: f32,
}

/// The query of trailer entities [`drive_trailers`] updates.
type TrailerQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static mut TrailerController, &'static mut TrailerState), Without<LayerParked>>;

/// Updates every [`TrailerController`] in spawn order: each trailer brakes with its train's
/// front vehicle's [`VehicleInput`] brake, then its [`TrailerState`] is written. Errors are
/// stored in [`TrailerController::last_error`]. A trailer of a parked data layer is not driven.
/// The work list is a reused buffer: nothing is allocated in steady state.
pub fn drive_trailers(
    fixed: Option<Res<FixedTime>>,
    physics: Option<ResMut<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    mut trailers: TrailerQuery,
    inputs: Query<&VehicleInput>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<TrailerController>>>,
    mut work: Local<Vec<TrailerWork>>,
) {
    let (Some(fixed), Some(mut physics)) = (fixed, physics) else {
        return;
    };
    let _span = debug_span!("drive_trailers").entered();
    let dt = fixed.step().as_secs_f32();
    work.clear();
    for (entity, controller, _) in trailers.iter() {
        // Walk to the train's front: the first tow that is not a trailer, or an uncoupled
        // trailer heading the rest.
        let (mut root, mut position, mut at) = (None, 0_u32, controller.tow);
        while let Some(tow) = at {
            position += 1;
            root = Some(tow);
            if position as usize > MAX_TRAIN_LENGTH {
                break;
            }
            at = trailers.get(tow).ok().and_then(|(_, next, _)| next.tow);
        }
        let brake = root
            .and_then(|root| inputs.get(root).ok())
            .map_or(0.0, |input| input.brake);
        work.push(TrailerWork {
            key: spawn_key(order.as_deref(), entity),
            entity,
            root,
            position: if controller.tow.is_some() { position } else { 0 },
            brake,
        });
    }
    work.sort_unstable_by_key(|item| item.key);
    for item in work.iter() {
        let Ok((_, mut controller, mut state)) = trailers.get_mut(item.entity) else {
            continue;
        };
        let result = controller
            .trailer
            .update(&mut physics, item.brake, dt, layers.as_deref());
        if !controller.trailer.is_coupled() && controller.tow.is_some() {
            controller.tow = None;
        }
        let error = result.err();
        if controller.last_error != error {
            controller.last_error = error;
        }
        let readout = controller.trailer.readout(&physics).unwrap_or_default();
        let coupled = controller.tow.is_some();
        let next = TrailerState {
            readout,
            tow: controller.tow,
            root: if coupled { item.root } else { None },
            train_position: if coupled { item.position } else { 0 },
        };
        if *state != next {
            *state = next;
        }
    }
}

/// A rebase moves every trailer's stored spawn pose and wheel contacts.
fn shift_trailers_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut trailers: Query<&mut TrailerController>,
) {
    for message in shifted.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        for mut controller in &mut trailers {
            controller.trailer.shift_origin(shift);
        }
    }
}

/// Registers [`TowHitch`], [`TrailerState`] and runs [`drive_trailers`] in `FixedUpdate`, inside
/// the physics chain after `drive_vehicles` (so the tow vehicles have pushed their chassis) and
/// before the step. Add it after `PhysicsPlugin` and `VehiclePlugin`.
pub struct TrailerPlugin;

impl Plugin for TrailerPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<TowHitch>()
            .register_type::<TrailerState>()
            .add_systems(
                FixedUpdate,
                drive_trailers
                    .after(drive_vehicles)
                    .after(crate::step::sync_velocity_and_forces)
                    .before(step_physics)
                    .in_set(PhysicsStep),
            );
        app.add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                shift_trailers_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
        // Trailers are driven in the order they arrived (ADR-2396's rule for vehicles).
        crate::plugin::must_track_spawn_order::<TrailerController>(app.world_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_presets_are_valid() {
        for config in [
            TrailerConfig::car_trailer(),
            TrailerConfig::semi_trailer(),
            TrailerConfig::dolly(),
        ] {
            assert_eq!(config.validate(), Ok(()));
        }
        assert_eq!(tractor_config().validate(), Ok(()));
    }

    #[test]
    fn bad_configs_name_their_field_and_code() {
        let field = |edit: &dyn Fn(&mut TrailerConfig)| {
            let mut config = TrailerConfig::car_trailer();
            edit(&mut config);
            config.validate().expect_err("rejected")
        };
        assert_eq!(
            field(&|c| c.chassis.wheels[1].drive = true),
            TrailerError::DrivenWheel { index: 1 }
        );
        assert_eq!(
            field(&|c| c.chassis.wheels[0].steer = true),
            TrailerError::SteeredWheel { index: 0 }
        );
        assert_eq!(field(&|c| c.chassis.wheels.clear()).code(), "vehicle_no_wheels");
        assert_eq!(
            field(&|c| c.coupling.capture_radius = 0.0),
            TrailerError::InvalidConfig {
                field: "coupling.capture_radius"
            }
        );
        assert_eq!(
            field(&|c| c.coupling.kind = CouplingKind::FifthWheel {
                yaw_limit: 2.0,
                pitch_limit: 0.2
            }),
            TrailerError::InvalidConfig {
                field: "coupling.kind.yaw_limit"
            }
        );
        assert_eq!(
            field(&|c| c.coupling.jackknife_warning_angle = 1.4),
            TrailerError::InvalidConfig {
                field: "coupling.jackknife_warning_angle"
            }
        );
        assert_eq!(
            field(&|c| c.brakes = TrailerBrakes::Follow {
                proportion: 0.5,
                delay: 3.0
            }),
            TrailerError::InvalidConfig {
                field: "brakes.delay"
            }
        );
        assert_eq!(
            field(&|c| c.brakes = TrailerBrakes::Surge {
                threshold: 500.0,
                full_force: 400.0,
                response: 0.1
            }),
            TrailerError::InvalidConfig {
                field: "brakes.full_force"
            }
        );
        assert_eq!(
            field(&|c| c.parking_brake = f32::NAN),
            TrailerError::InvalidConfig {
                field: "parking_brake"
            }
        );
        assert_eq!(
            field(&|c| c.rear_hitch = Some(Vec3::splat(f32::INFINITY))),
            TrailerError::InvalidConfig { field: "rear_hitch" }
        );
    }

    #[test]
    fn codes_are_lowercase_sorted_and_cover_every_variant() {
        let mut sorted = TRAILER_ERROR_CODES;
        sorted.sort_unstable();
        assert_eq!(sorted, TRAILER_ERROR_CODES);
        let errors = [
            TrailerError::InvalidConfig { field: "x" },
            TrailerError::DrivenWheel { index: 0 },
            TrailerError::SteeredWheel { index: 0 },
            TrailerError::OutOfReach {
                distance: 1.0,
                capture_radius: 0.3,
            },
            TrailerError::AlreadyCoupled,
            TrailerError::NotCoupled,
            TrailerError::TowMissing,
            TrailerError::NoHitch,
            TrailerError::HitchOccupied,
            TrailerError::SelfCoupling,
            TrailerError::ChainCycle,
            TrailerError::ChainTooLong,
            TrailerError::NotATrailer,
            TrailerError::NoPhysicsWorld,
        ];
        let codes: Vec<&str> = errors.iter().map(TrailerError::code).collect();
        for code in TRAILER_ERROR_CODES {
            assert!(codes.contains(&code), "{code}");
        }
        for code in codes {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
        assert_eq!(
            TrailerError::Chassis(VehicleError::ChassisMissing).code(),
            "vehicle_chassis_missing"
        );
    }

    #[test]
    fn relative_yaw_is_positive_when_the_trailer_points_left() {
        let tow = Quat::from_rotation_y(0.3);
        let (yaw, pitch) = relative_yaw_pitch(tow, tow * Quat::from_rotation_y(0.2));
        assert!((yaw - 0.2).abs() < 1.0e-5, "{yaw}");
        assert!(pitch.abs() < 1.0e-5);
        let (yaw, pitch) = relative_yaw_pitch(Quat::IDENTITY, Quat::from_rotation_x(0.1));
        assert!(yaw.abs() < 1.0e-5);
        assert!((pitch - 0.1).abs() < 1.0e-5, "{pitch}");
    }

    #[test]
    fn the_follow_brake_is_delayed_by_whole_steps_and_scaled() {
        let mut physics = PhysicsWorld::default();
        let config = TrailerConfig {
            brakes: TrailerBrakes::Follow {
                proportion: 0.5,
                delay: 0.05,
            },
            ..TrailerConfig::car_trailer()
        };
        let mut trailer = Trailer::spawn(&mut physics, config, Vec3::Y, Quat::IDENTITY).expect("spawn");
        let dt = 0.01;
        // 0.05 s at 100 Hz: 5 steps.
        let applied: Vec<f32> = (0..8)
            .map(|step| trailer.service_brake(if step >= 1 { 1.0 } else { 0.0 }, 10.0, dt))
            .collect();
        assert_eq!(applied, vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.5]);
    }

    #[test]
    fn a_surge_brake_answers_a_push_and_not_reverse() {
        let mut physics = PhysicsWorld::default();
        let config = TrailerConfig {
            brakes: TrailerBrakes::Surge {
                threshold: 200.0,
                full_force: 2200.0,
                response: 0.01,
            },
            ..TrailerConfig::car_trailer()
        };
        let mut trailer = Trailer::spawn(&mut physics, config, Vec3::Y, Quat::IDENTITY).expect("spawn");
        let dt = 0.01;
        // Steady speed: no push, no brake.
        trailer.last_forward_speed = Some(10.0);
        assert_eq!(trailer.service_brake(0.0, 10.0, dt), 0.0);
        // 4 m/s² of deceleration on 750 kg: 3000 N of push (both wheels brake: no rolling term).
        trailer.last_forward_speed = Some(10.0);
        let level = trailer.service_brake(0.0, 9.96, dt);
        assert!((level - 1.0).abs() < 1.0e-4, "{level}");
        // Reversing: locked out, the level falls back.
        trailer.last_forward_speed = Some(-2.0);
        assert_eq!(trailer.service_brake(0.0, -2.1, dt), 0.0);
    }
}
