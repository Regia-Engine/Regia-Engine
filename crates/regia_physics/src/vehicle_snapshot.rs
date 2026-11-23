// SPDX-License-Identifier: MPL-2.0
// Source: rapier3d 0.36 `Wheel` and `DynamicRayCastVehicleController` (Apache-2.0): which fields carry
// across a step was read from that crate's public source. The snapshot, the isolated world and the file
// form are original design (ADR-3080; `docs/provenance.md` says what was and was not checked).

//! Saving and restoring a [`Vehicle`], and a world of its own to simulate one in (ADR-3080).
//!
//! | Piece | What |
//! |---|---|
//! | [`VehicleSnapshot`] | everything the next step reads, at full precision; restoring it and stepping equals never having saved |
//! | [`VehicleCore`] | the same state without the last step's readouts: what a correction travels with |
//! | [`VehicleSimWorld`] | a private physics world with one chassis and copies of the static colliders, which can be put back to any tick |
//!
//! # Exactness
//!
//! A vehicle's next step reads its chassis (pose, velocities), its drivetrain, rapier's wheel
//! records and the slip tires' stick anchors. All of them are in the snapshot, and rapier's `Wheel`
//! is written back whole, private fields included. So [`Vehicle::restore`] followed by a step gives the same bits as the step
//! without the save.
//!
//! The exception is a chassis **body** touching something when it was saved. rapier warm-starts its
//! contact solver from an impulse cache that lives in the world's narrow phase, which has no public
//! way to write one pair. [`VehicleSnapshot::chassis_in_contact`] says when this applies. There the
//! steps after a restore differ in the last bits until the contact ends.
//! [`VehicleSimWorld::checkpoint`] covers that case, because the whole world is private to the car.

use rapier3d::control::Wheel;
use rapier3d::dynamics::{IntegrationParameters, IslandManager};
use rapier3d::geometry::{BroadPhaseBvh, NarrowPhase};
use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::layers::PhysicsLayers;
use crate::vehicle::{Vehicle, VehicleConfig, VehicleError, VehicleInput};
use crate::vehicle_drivetrain::{DrivetrainCore, DrivetrainState, ShiftRequest};
use crate::vehicle_tire::{TireForces, TireState};
use crate::world::{MAX_CCD_SUBSTEPS, MAX_SOLVER_ITERATIONS, PhysicsSettings, PhysicsWorld};

/// Current version of the [`VehicleSnapshot`] text form (`docs/formats/vehicle-snapshot.md`).
pub const VEHICLE_SNAPSHOT_FORMAT_VERSION: u32 = 2;

/// Largest [`VehicleSnapshot`] text the reader accepts, in bytes.
pub const VEHICLE_SNAPSHOT_MAX_BYTES: usize = 1 << 20;

/// Why a snapshot could not be read or restored.
///
/// [`VehicleSnapshotError::code`] gives a stable lowercase identifier for logs and the agent protocol.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VehicleSnapshotError {
    /// The vehicle itself refused (its chassis is gone, not dynamic, ...).
    #[error(transparent)]
    Vehicle(#[from] VehicleError),
    /// The snapshot's wheel count is not the vehicle's.
    #[error("the snapshot has {found} wheels; the vehicle has {expected}")]
    WheelCount {
        /// The vehicle's wheels.
        expected: usize,
        /// The snapshot's wheels.
        found: usize,
    },
    /// The snapshot was taken with another drive model, or a wheel's fixed data differs.
    #[error("the snapshot was taken from a vehicle with another configuration: `{field}`")]
    ConfigMismatch {
        /// What differs.
        field: &'static str,
    },
    /// A value is not finite, or a rotation is degenerate.
    #[error("the snapshot's `{field}` is not finite or out of range")]
    NotFinite {
        /// The offending field.
        field: &'static str,
    },
    /// The text form has another version.
    #[error(
        "vehicle snapshot format {found} is not supported (this build reads 1..={VEHICLE_SNAPSHOT_FORMAT_VERSION})"
    )]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
    },
    /// The text form is larger than [`VEHICLE_SNAPSHOT_MAX_BYTES`].
    #[error("the vehicle snapshot is {bytes} bytes; the most is {VEHICLE_SNAPSHOT_MAX_BYTES}")]
    TooLarge {
        /// Its size.
        bytes: usize,
    },
    /// The text form does not parse.
    #[error("the vehicle snapshot does not parse: {0}")]
    Parse(String),
}

impl VehicleSnapshotError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Vehicle(error) => error.code(),
            Self::WheelCount { .. } => "vehicle_snapshot_wheel_count",
            Self::ConfigMismatch { .. } => "vehicle_snapshot_config_mismatch",
            Self::NotFinite { .. } => "vehicle_snapshot_not_finite",
            Self::UnsupportedVersion { .. } => "vehicle_snapshot_unsupported_version",
            Self::TooLarge { .. } => "vehicle_snapshot_too_large",
            Self::Parse(_) => "vehicle_snapshot_parse",
        }
    }
}

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

/// One slip tire in a [`VehicleSnapshot`]: its stick anchor, which the next step reads, and what it
/// did on the last step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TireSnapshot {
    /// Where the contact stuck, in `anchor_body`'s frame when that is set and in world space
    /// otherwise; `None` while the tire is not stuck.
    pub anchor: Option<Vec3>,
    /// The ground body whose frame `anchor` is in, as rapier's handle index and generation. A later
    /// step on any other ground lets go of the anchor.
    pub anchor_body: Option<(u32, u32)>,
    /// The wheel has a slip tire.
    pub slip: bool,
    /// Mean side force over the last step, newtons.
    pub lateral_force: f32,
    /// Mean lengthwise force over the last step, newtons.
    pub longitudinal_force: f32,
    /// The tire's load on the last step, newtons.
    pub load: f32,
    /// The slip angle on the last step, radians.
    pub slip_angle: f32,
    /// The tire stuck on the last step.
    pub stuck: bool,
    /// The lagged slip's tangent the next step starts from (ADR-3276); 0 without a relaxation
    /// length. Absent in snapshots written before tire transients, which read as 0: the state such
    /// a build stepped from.
    #[serde(default)]
    pub lag: f32,
    /// The slip angle the side force used on the last step, radians (ADR-3276).
    #[serde(default)]
    pub lagged_slip_angle: f32,
    /// The self-aligning torque on the last step, newton-meters (ADR-3276).
    #[serde(default)]
    pub aligning_torque: f32,
    /// The camber the tire saw on the last step, radians.
    #[serde(default)]
    pub camber: f32,
    /// The camber thrust inside `lateral_force` on the last step, newtons.
    #[serde(default)]
    pub camber_thrust: f32,
}

impl TireSnapshot {
    fn of(state: &TireState) -> Self {
        Self {
            anchor: state.anchor.map(from_rapier_vec3),
            anchor_body: state.anchor_body.map(|handle| handle.into_raw_parts()),
            slip: state.forces.slip,
            lateral_force: state.forces.lateral_force,
            longitudinal_force: state.forces.longitudinal_force,
            load: state.forces.load,
            slip_angle: state.forces.slip_angle,
            stuck: state.forces.stuck,
            lag: state.lag,
            lagged_slip_angle: state.forces.lagged_slip_angle,
            aligning_torque: state.forces.aligning_torque,
            camber: state.forces.camber,
            camber_thrust: state.forces.camber_thrust,
        }
    }

    fn state(&self) -> TireState {
        TireState {
            anchor: self.anchor.map(to_rapier_vec3),
            anchor_body: self.anchor_body.map(|(index, generation)| {
                rapier3d::dynamics::RigidBodyHandle::from_raw_parts(index, generation)
            }),
            forces: TireForces {
                slip: self.slip,
                lateral_force: self.lateral_force,
                longitudinal_force: self.longitudinal_force,
                load: self.load,
                slip_angle: self.slip_angle,
                stuck: self.stuck,
                lagged_slip_angle: self.lagged_slip_angle,
                aligning_torque: self.aligning_torque,
                camber: self.camber,
                camber_thrust: self.camber_thrust,
            },
            lag: self.lag,
            // The peak cache is derived from the tire, which a snapshot does not store.
            peak_angle: 0.0,
            peak_key: 0,
        }
    }

    fn is_finite(&self) -> bool {
        self.anchor.is_none_or(Vec3::is_finite)
            && [
                self.lateral_force,
                self.longitudinal_force,
                self.load,
                self.slip_angle,
                self.lag,
                self.lagged_slip_angle,
                self.aligning_torque,
                self.camber,
                self.camber_thrust,
            ]
            .iter()
            .all(|value| value.is_finite())
    }
}

/// Everything a vehicle's next step reads, at full precision. See the module docs for what is exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VehicleSnapshot {
    /// [`VEHICLE_SNAPSHOT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Chassis body origin, world space.
    pub translation: Vec3,
    /// Chassis rotation, world space.
    pub rotation: Quat,
    /// Chassis linear velocity at the center of mass, m/s.
    pub linear_velocity: Vec3,
    /// Chassis angular velocity, rad/s.
    pub angular_velocity: Vec3,
    /// rapier's whole record of each wheel: spin, suspension, contact and the private rotation
    /// delta a wheel in the air coasts on.
    pub wheels: Vec<Wheel>,
    /// rapier's own speed record.
    pub vehicle_speed: f32,
    /// The geared drivetrain's state, `None` with the `Simple` drivetrain. Boxed: most of a
    /// rollback's copies are of cars that do not change gear.
    pub drivetrain: Option<Box<DrivetrainState>>,
    /// The slip tires, one per wheel; empty when no wheel has a slip tire.
    pub tires: Vec<TireSnapshot>,
    /// The last input applied.
    pub input: VehicleInput,
    /// Updates since spawn, attach or reset.
    pub steps: u64,
    /// The contact record's pending floating-origin shift.
    pub contact_shift: Vec3,
    /// The chassis body touched something when this was saved: see the module docs.
    pub chassis_in_contact: bool,
}

/// `value`'s bits: positive and negative zero differ, and so do two NaNs.
fn hash_bits(hash: &mut u64, value: u32) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

impl VehicleSnapshot {
    /// A digest of the bits of the state the next step reads and the readouts: two snapshots with
    /// the same digest are the same vehicle.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let vectors = [
            self.translation,
            self.linear_velocity,
            self.angular_velocity,
            self.contact_shift,
        ];
        for vector in vectors {
            for value in vector.to_array() {
                hash_bits(&mut hash, value.to_bits());
            }
        }
        for value in self.rotation.to_array() {
            hash_bits(&mut hash, value.to_bits());
        }
        hash_bits(&mut hash, self.vehicle_speed.to_bits());
        for value in [self.input.throttle, self.input.brake, self.input.steer] {
            hash_bits(&mut hash, value.to_bits());
        }
        hash_bits(&mut hash, self.steps as u32);
        hash_bits(&mut hash, (self.steps >> 32) as u32);
        for wheel in &self.wheels {
            let info = wheel.raycast_info();
            let contact = from_rapier_vec3(info.contact_point_ws);
            for value in [
                wheel.rotation,
                wheel.steering,
                wheel.engine_force,
                wheel.brake,
                wheel.wheel_suspension_force,
                info.suspension_length,
                contact.x,
                contact.y,
                contact.z,
            ] {
                hash_bits(&mut hash, value.to_bits());
            }
            hash_bits(&mut hash, u32::from(info.is_in_contact));
        }
        if let Some(drivetrain) = &self.drivetrain {
            let digest = drivetrain.digest();
            hash_bits(&mut hash, digest as u32);
            hash_bits(&mut hash, (digest >> 32) as u32);
        }
        for tire in &self.tires {
            for word in tire.state().digest_words() {
                hash_bits(&mut hash, word);
            }
            hash_bits(&mut hash, u32::from(tire.anchor.is_some()));
            let (index, generation) = tire.anchor_body.unwrap_or((u32::MAX, u32::MAX));
            hash_bits(&mut hash, index);
            hash_bits(&mut hash, generation);
        }
        hash
    }

    /// Wheel `index`'s rolling angle, radians: the drivetrain's with a geared car, rapier's otherwise.
    #[must_use]
    pub fn spin_angle(&self, index: usize) -> Option<f32> {
        if let Some(drivetrain) = &self.drivetrain {
            return drivetrain.wheels().get(index).map(|wheel| wheel.spin_angle);
        }
        self.wheels.get(index).map(|wheel| wheel.rotation)
    }

    /// Wheel `index`'s suspension length, meters.
    #[must_use]
    pub fn suspension_length(&self, index: usize) -> Option<f32> {
        self.wheels
            .get(index)
            .map(|wheel| wheel.raycast_info().suspension_length)
    }

    /// Engine speed, rpm; 0 with the `Simple` drivetrain.
    #[must_use]
    pub fn rpm(&self) -> f32 {
        self.drivetrain.as_deref().map_or(0.0, DrivetrainState::rpm)
    }

    /// The engaged gear; 0 with the `Simple` drivetrain.
    #[must_use]
    pub fn gear(&self) -> i32 {
        self.drivetrain.as_deref().map_or(0, DrivetrainState::gear)
    }

    /// The torque through the clutch on the last substep, newton-meters; 0 with the `Simple`
    /// drivetrain.
    #[must_use]
    pub fn clutch_torque(&self) -> f32 {
        self.drivetrain
            .as_deref()
            .map_or(0.0, DrivetrainState::clutch_torque)
    }

    /// Checks everything a reader can check on its own: version, finiteness.
    ///
    /// # Errors
    /// The first problem found.
    pub fn validate(&self) -> Result<(), VehicleSnapshotError> {
        if self.format_version != VEHICLE_SNAPSHOT_FORMAT_VERSION {
            return Err(VehicleSnapshotError::UnsupportedVersion {
                found: self.format_version,
            });
        }
        let not_finite = |field| Err(VehicleSnapshotError::NotFinite { field });
        if !self.translation.is_finite() {
            return not_finite("translation");
        }
        if !self.rotation.is_finite() || self.rotation.length_squared() < 1.0e-12 {
            return not_finite("rotation");
        }
        if !self.linear_velocity.is_finite() {
            return not_finite("linear_velocity");
        }
        if !self.angular_velocity.is_finite() {
            return not_finite("angular_velocity");
        }
        if !self.contact_shift.is_finite() {
            return not_finite("contact_shift");
        }
        if !self.vehicle_speed.is_finite() {
            return not_finite("vehicle_speed");
        }
        if ![self.input.throttle, self.input.brake, self.input.steer]
            .iter()
            .all(|value| value.is_finite())
        {
            return not_finite("input");
        }
        for wheel in &self.wheels {
            if !wheel.rotation.is_finite() {
                return not_finite("wheels.rotation");
            }
        }
        if self
            .drivetrain
            .as_deref()
            .is_some_and(|drivetrain| !drivetrain.is_sound(self.wheels.len()))
        {
            return not_finite("drivetrain");
        }
        if !self.tires.iter().all(TireSnapshot::is_finite) {
            return not_finite("tires");
        }
        Ok(())
    }

    /// Whether `self` fits `config`: wheel count, fixed wheel data and drive model.
    fn check_fits(
        &self,
        config: &VehicleConfig,
        physics_wheels: &[Wheel],
    ) -> Result<(), VehicleSnapshotError> {
        if self.wheels.len() != physics_wheels.len() {
            return Err(VehicleSnapshotError::WheelCount {
                expected: physics_wheels.len(),
                found: self.wheels.len(),
            });
        }
        let same = |a: f32, b: f32| a.to_bits() == b.to_bits();
        for (saved, own) in self.wheels.iter().zip(physics_wheels) {
            let fixed = same(saved.radius, own.radius)
                && same(saved.suspension_rest_length, own.suspension_rest_length)
                && same(saved.max_suspension_travel, own.max_suspension_travel)
                && same(saved.suspension_stiffness, own.suspension_stiffness)
                && same(saved.damping_compression, own.damping_compression)
                && same(saved.damping_relaxation, own.damping_relaxation)
                && same(saved.side_friction_stiffness, own.side_friction_stiffness)
                && same(saved.max_suspension_force, own.max_suspension_force)
                && saved.chassis_connection_point_cs == own.chassis_connection_point_cs
                && saved.direction_cs == own.direction_cs
                && saved.axle_cs == own.axle_cs;
            if !fixed {
                return Err(VehicleSnapshotError::ConfigMismatch { field: "wheels" });
            }
            if !saved.friction_slip.is_finite() {
                return Err(VehicleSnapshotError::NotFinite {
                    field: "wheels.friction_slip",
                });
            }
        }
        let geared = matches!(
            config.drivetrain,
            crate::vehicle_drivetrain::Drivetrain::Geared(_)
        );
        if geared != self.drivetrain.is_some() {
            return Err(VehicleSnapshotError::ConfigMismatch { field: "drivetrain" });
        }
        let tires = if config.has_slip_tires() {
            physics_wheels.len()
        } else {
            0
        };
        if self.tires.len() != tires {
            return Err(VehicleSnapshotError::ConfigMismatch { field: "tires" });
        }
        Ok(())
    }

    /// The text form (`docs/formats/vehicle-snapshot.md`).
    ///
    /// # Errors
    /// [`VehicleSnapshotError::Parse`] when a value cannot be written.
    pub fn to_ron(&self) -> Result<String, VehicleSnapshotError> {
        ron::to_string(self).map_err(|error| VehicleSnapshotError::Parse(error.to_string()))
    }

    /// Reads the text form: upgrades v1 tire history to zero, refuses an unsupported version,
    /// an oversized file and any value that is not
    /// finite, including ones in rapier's private wheel fields. The result still has to fit the
    /// vehicle it is restored into: [`Vehicle::restore`] checks that.
    ///
    /// # Errors
    /// The first problem found; [`VehicleSnapshotError::code`] names it.
    pub fn from_ron(text: &str) -> Result<Self, VehicleSnapshotError> {
        if text.len() > VEHICLE_SNAPSHOT_MAX_BYTES {
            return Err(VehicleSnapshotError::TooLarge { bytes: text.len() });
        }
        // The version comes first: another version's fields are not a parse error to report.
        let found = regia_core::version::peek_ron_version(text).map_err(VehicleSnapshotError::Parse)?;
        if !(1..=VEHICLE_SNAPSHOT_FORMAT_VERSION).contains(&found) {
            return Err(VehicleSnapshotError::UnsupportedVersion { found });
        }
        let mut snapshot: Self =
            ron::from_str(text).map_err(|error| VehicleSnapshotError::Parse(error.to_string()))?;
        snapshot.format_version = VEHICLE_SNAPSHOT_FORMAT_VERSION;
        snapshot.validate()?;
        for wheel in &snapshot.wheels {
            let written =
                ron::to_string(wheel).map_err(|error| VehicleSnapshotError::Parse(error.to_string()))?;
            if has_non_finite_token(&written) {
                return Err(VehicleSnapshotError::NotFinite { field: "wheels" });
            }
        }
        Ok(snapshot)
    }
}

/// Whether a written RON value holds a NaN or an infinity.
fn has_non_finite_token(written: &str) -> bool {
    written.contains("NaN") || written.contains(":inf") || written.contains("-inf")
}

// ---------------------------------------------------------------------------
// The core
// ---------------------------------------------------------------------------

/// One wheel of a [`VehicleCore`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelCoreState {
    /// Rolling angle, radians.
    pub spin_angle: f32,
    /// Suspension length, meters.
    pub suspension_length: f32,
}

/// What a correction travels with: the chassis, the wheels' looks and the drivetrain's next-step
/// state. Leaves out the last step's readouts, which the next step recomputes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VehicleCore {
    /// Chassis body origin, world space.
    pub translation: Vec3,
    /// Chassis rotation, world space.
    pub rotation: Quat,
    /// Chassis linear velocity at the center of mass, m/s.
    pub linear_velocity: Vec3,
    /// Chassis angular velocity, rad/s.
    pub angular_velocity: Vec3,
    /// One entry per wheel.
    pub wheels: Vec<WheelCoreState>,
    /// The geared drivetrain's next-step state.
    pub drivetrain: Option<DrivetrainCore>,
}

impl Vehicle {
    /// Saves the vehicle: see [`VehicleSnapshot`].
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis is gone.
    pub fn snapshot(&self, physics: &PhysicsWorld) -> Result<VehicleSnapshot, VehicleError> {
        let handle = self.chassis().to_handle();
        let Some(body) = physics.bodies.get(handle) else {
            return Err(VehicleError::ChassisMissing);
        };
        let (translation, rotation) = from_rapier_pose(body.position());
        let (wheels, vehicle_speed) = self.controller_state();
        let (input, steps, contact_shift) = self.step_fields();
        let chassis_in_contact = body.colliders().iter().any(|collider| {
            physics
                .narrow_phase
                .contact_pairs_with(*collider)
                .any(|pair| pair.has_any_active_contact())
        });
        Ok(VehicleSnapshot {
            format_version: VEHICLE_SNAPSHOT_FORMAT_VERSION,
            translation,
            rotation,
            linear_velocity: from_rapier_vec3(body.linvel()),
            angular_velocity: from_rapier_vec3(body.angvel()),
            wheels: wheels.to_vec(),
            vehicle_speed,
            drivetrain: self.drivetrain().cloned().map(Box::new),
            tires: self.tires().iter().map(TireSnapshot::of).collect(),
            input,
            steps,
            contact_shift,
            chassis_in_contact,
        })
    }

    /// Goes back to a saved state. The next step gives the bits it gave after the save (the module
    /// docs name the one exception). Nothing changes on error.
    ///
    /// # Errors
    /// A snapshot that does not fit this vehicle, or a value that is not finite.
    pub fn restore(
        &mut self,
        physics: &mut PhysicsWorld,
        snapshot: &VehicleSnapshot,
    ) -> Result<(), VehicleSnapshotError> {
        snapshot.validate()?;
        snapshot.check_fits(self.config(), self.controller_state().0)?;
        self.write_chassis(
            physics,
            (snapshot.translation, snapshot.rotation),
            (snapshot.linear_velocity, snapshot.angular_velocity),
        )?;
        self.restore_fields(
            (&snapshot.wheels, snapshot.vehicle_speed),
            (snapshot.input, snapshot.steps, snapshot.contact_shift),
        );
        *self.drivetrain_slot() = snapshot.drivetrain.clone();
        for (tire, saved) in self.tires_mut().iter_mut().zip(&snapshot.tires) {
            *tire = saved.state();
        }
        Ok(())
    }

    /// The vehicle's state without the last step's readouts.
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis is gone.
    pub fn core(&self, physics: &PhysicsWorld) -> Result<VehicleCore, VehicleError> {
        let Some(body) = physics.bodies.get(self.chassis().to_handle()) else {
            return Err(VehicleError::ChassisMissing);
        };
        let (translation, rotation) = from_rapier_pose(body.position());
        let (wheels, _) = self.controller_state();
        let drivetrain = self.drivetrain();
        let core = wheels
            .iter()
            .enumerate()
            .map(|(index, wheel)| WheelCoreState {
                spin_angle: drivetrain
                    .and_then(|state| state.wheels().get(index))
                    .map_or(wheel.rotation, |spin| spin.spin_angle),
                suspension_length: wheel.raycast_info().suspension_length,
            })
            .collect();
        Ok(VehicleCore {
            translation,
            rotation,
            linear_velocity: from_rapier_vec3(body.linvel()),
            angular_velocity: from_rapier_vec3(body.angvel()),
            wheels: core,
            drivetrain: drivetrain.map(DrivetrainState::core),
        })
    }

    /// Goes on from a corrected state. The chassis and the drivetrain take the values as they are;
    /// the wheels take their spin, and the next step casts their rays afresh. The slip tires let go
    /// of their stick anchors: an anchor measures from the old pose, so keeping it would pull the
    /// corrected chassis back as a hold, and the next step sticks afresh. Nothing changes on error.
    ///
    /// # Errors
    /// A wheel count or drive model that does not fit, or a value that is not finite.
    pub fn apply_core(
        &mut self,
        physics: &mut PhysicsWorld,
        core: &VehicleCore,
    ) -> Result<(), VehicleSnapshotError> {
        let own = self.controller_state().0.len();
        if core.wheels.len() != own {
            return Err(VehicleSnapshotError::WheelCount {
                expected: own,
                found: core.wheels.len(),
            });
        }
        if core.drivetrain.is_some() != self.drivetrain().is_some() {
            return Err(VehicleSnapshotError::ConfigMismatch { field: "drivetrain" });
        }
        let not_finite = |field| Err(VehicleSnapshotError::NotFinite { field });
        if !core.rotation.is_finite() || core.rotation.length_squared() < 1.0e-12 {
            return not_finite("rotation");
        }
        if !core.translation.is_finite()
            || !core.linear_velocity.is_finite()
            || !core.angular_velocity.is_finite()
        {
            return not_finite("chassis");
        }
        if core.wheels.iter().any(|wheel| !wheel.spin_angle.is_finite()) {
            return not_finite("wheels");
        }
        if let Some(drivetrain) = &core.drivetrain
            && !drivetrain_core_is_finite(drivetrain)
        {
            return not_finite("drivetrain");
        }
        self.write_chassis(
            physics,
            (core.translation, core.rotation),
            (core.linear_velocity, core.angular_velocity),
        )?;
        for (wheel, saved) in self.wheels_mut().iter_mut().zip(&core.wheels) {
            wheel.rotation = saved.spin_angle;
        }
        if let (Some(state), Some(saved)) = (self.drivetrain_slot().as_deref_mut(), &core.drivetrain) {
            state.set_core(saved);
        }
        for tire in self.tires_mut() {
            tire.anchor = None;
            tire.anchor_body = None;
        }
        Ok(())
    }

    /// Puts the chassis at a pose and velocities, with no forces pending.
    fn write_chassis(
        &self,
        physics: &mut PhysicsWorld,
        (translation, rotation): (Vec3, Quat),
        (linear, angular): (Vec3, Vec3),
    ) -> Result<(), VehicleError> {
        let Some(body) = physics.bodies.get_mut(self.chassis().to_handle()) else {
            return Err(VehicleError::ChassisMissing);
        };
        body.set_position(to_rapier_pose(translation, rotation), true);
        body.set_linvel(to_rapier_vec3(linear), true);
        body.set_angvel(to_rapier_vec3(angular), true);
        body.reset_forces(true);
        body.reset_torques(true);
        Ok(())
    }
}

fn drivetrain_core_is_finite(core: &DrivetrainCore) -> bool {
    let scalars = [
        core.engine_speed,
        core.shift_timer,
        core.dwell,
        core.clutch_torque,
        core.tc_factor,
        core.torque_hints[0],
        core.torque_hints[1],
        core.torque_hints[2],
    ];
    scalars.iter().all(|value| value.is_finite())
        && core.wheels.iter().all(|wheel| {
            [
                wheel.angular_speed,
                wheel.spin_angle,
                wheel.slip_ratio,
                wheel.tire_force,
                wheel.load,
                wheel.abs_factor,
            ]
            .iter()
            .all(|value| value.is_finite())
        })
}

// ---------------------------------------------------------------------------
// The isolated world
// ---------------------------------------------------------------------------

/// What [`VehicleSimWorld::checkpoint`] keeps: the snapshot and the world's contact caches.
#[derive(Clone)]
pub struct VehicleCheckpoint {
    snapshot: VehicleSnapshot,
    islands: IslandManager,
    broad_phase: BroadPhaseBvh,
    narrow_phase: NarrowPhase,
}

impl std::fmt::Debug for VehicleCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VehicleCheckpoint")
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

impl VehicleCheckpoint {
    /// The vehicle part of the checkpoint.
    #[must_use]
    pub fn snapshot(&self) -> &VehicleSnapshot {
        &self.snapshot
    }
}

/// How a [`VehicleSimWorld`] steps: the shared world's settings, so the same input gives the same bits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VehicleSimSettings {
    /// Gravity, m/s².
    pub gravity: Vec3,
    /// Constraint-solver iterations per step.
    pub solver_iterations: usize,
    /// Maximum CCD substeps per step.
    pub ccd_substeps: usize,
}

impl Default for VehicleSimSettings {
    fn default() -> Self {
        Self::from_settings(&PhysicsSettings::default())
    }
}

impl VehicleSimSettings {
    /// The settings `step_physics` steps the shared world with.
    #[must_use]
    pub fn from_settings(settings: &PhysicsSettings) -> Self {
        Self {
            gravity: settings.gravity,
            solver_iterations: settings.solver_iterations,
            ccd_substeps: settings.ccd_substeps,
        }
    }
}

/// One chassis in a physics world of its own, with a copy of the shared world's static colliders
/// (rapier shapes are shared, so a copy costs its bounding volume, not its geometry).
///
/// The shared world cannot go back for one entity. This world can, because the car is all that moves
/// in it. A static collider added later reaches it through [`Self::sync_statics`]. A moving body of
/// the shared world is not here: a bump against one is the server's to report.
pub struct VehicleSimWorld {
    physics: PhysicsWorld,
    vehicle: Vehicle,
    settings: VehicleSimSettings,
    /// How many colliders of the shared world were static at the last copy, and their revision.
    statics_signature: u64,
}

impl std::fmt::Debug for VehicleSimWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VehicleSimWorld")
            .field("colliders", &self.physics.colliders.len())
            .field("steps", &self.vehicle.steps())
            .finish_non_exhaustive()
    }
}

/// Adds a fixed box to a world with no entity behind it: a test track, a bench's level.
pub fn add_static_box(physics: &mut PhysicsWorld, center: Vec3, half_extents: Vec3) {
    physics.colliders.insert(
        rapier3d::geometry::ColliderBuilder::cuboid(half_extents.x, half_extents.y, half_extents.z)
            .translation(to_rapier_vec3(center)),
    );
}

/// Whether a collider of the shared world belongs in a car's private world.
fn is_static(physics: &PhysicsWorld, collider: &rapier3d::geometry::Collider) -> bool {
    !collider.is_sensor()
        && collider
            .parent()
            .and_then(|parent| physics.bodies.get(parent))
            .is_none_or(rapier3d::dynamics::RigidBody::is_fixed)
}

/// An order-sensitive signature of the shared world's static colliders: their handles, poses and
/// shapes. A shape counts by its type and its local bounding box (ADR-7431): a collider resized or
/// replaced in place (`Collider::set_shape`) forces one rebuild. A new shape of the same type with
/// the same local bounds (a mesh edited strictly inside its box) does not; replace the collider
/// for that.
fn statics_signature(shared: &PhysicsWorld) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for (handle, collider) in shared.colliders.iter() {
        if !is_static(shared, collider) {
            continue;
        }
        let (index, generation) = handle.into_raw_parts();
        hash_bits(&mut hash, index);
        hash_bits(&mut hash, generation);
        let (at, rotation) = from_rapier_pose(collider.position());
        for value in at.to_array().into_iter().chain(rotation.to_array()) {
            hash_bits(&mut hash, value.to_bits());
        }
        let shape = collider.shape();
        hash_bits(&mut hash, shape.shape_type() as u32);
        let bounds = shape.compute_local_aabb();
        let (mins, maxs) = (from_rapier_vec3(bounds.mins), from_rapier_vec3(bounds.maxs));
        for value in mins.to_array().into_iter().chain(maxs.to_array()) {
            hash_bits(&mut hash, value.to_bits());
        }
    }
    hash
}

fn step_world(physics: &mut PhysicsWorld, settings: &VehicleSimSettings, dt: f32) {
    // The live step clamps these to MAX_SOLVER_ITERATIONS / MAX_CCD_SUBSTEPS (world.rs); the
    // private world must clamp the same way or settings past the clamp step a car differently
    // from the shared world it exists to reproduce.
    let integration = IntegrationParameters {
        dt,
        num_solver_iterations: settings.solver_iterations.clamp(1, MAX_SOLVER_ITERATIONS),
        max_ccd_substeps: settings.ccd_substeps.clamp(1, MAX_CCD_SUBSTEPS),
        ..Default::default()
    };
    let PhysicsWorld {
        pipeline,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        ..
    } = physics;
    pipeline.step(
        to_rapier_vec3(settings.gravity),
        &integration,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        &(),
        &(),
    );
}

/// Copies the static colliders and builds their bounding volume, so the first wheel ray finds them.
fn copy_statics(into: &mut PhysicsWorld, shared: &PhysicsWorld, settings: &VehicleSimSettings, dt: f32) {
    for (_, collider) in shared.colliders.iter() {
        if is_static(shared, collider) {
            // `insert` drops the parent and keeps the world pose.
            into.colliders.insert(collider.clone());
        }
    }
    step_world(into, settings, dt);
}

impl VehicleSimWorld {
    /// A private world holding `shared`'s static colliders and a vehicle at a pose. `dt` is the
    /// fixed step.
    ///
    /// # Errors
    /// A configuration or pose [`Vehicle::spawn`] refuses.
    pub fn new(
        config: VehicleConfig,
        shared: &PhysicsWorld,
        settings: VehicleSimSettings,
        (translation, rotation): (Vec3, Quat),
        dt: f32,
    ) -> Result<Self, VehicleError> {
        config.validate()?;
        crate::vehicle::checked_pose(translation, rotation)?;
        let mut physics = PhysicsWorld::default();
        copy_statics(&mut physics, shared, &settings, dt);
        let vehicle = Vehicle::spawn(&mut physics, config, translation, rotation)?;
        Ok(Self {
            physics,
            vehicle,
            settings,
            statics_signature: statics_signature(shared),
        })
    }

    /// The vehicle.
    #[must_use]
    pub fn vehicle(&self) -> &Vehicle {
        &self.vehicle
    }

    /// The private world.
    #[must_use]
    pub fn physics(&self) -> &PhysicsWorld {
        &self.physics
    }

    /// Colliders in the private world (the chassis' included).
    #[must_use]
    pub fn collider_count(&self) -> usize {
        self.physics.colliders.len()
    }

    /// Brings the static colliders up to date when the shared world's changed (a streamed tile, a
    /// removed wall), keeping the vehicle where it is. Returns whether anything changed. Cost: one
    /// walk over the shared world's colliders; a rebuild when the set changed.
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis is gone, and
    /// [`VehicleError::StateNotFinite`] when the car's state is not finite (it cannot be carried
    /// into the rebuilt world). Nothing changes on error: the old world and car stay until an
    /// authoritative state makes the car finite again.
    pub fn sync_statics(&mut self, shared: &PhysicsWorld, dt: f32) -> Result<bool, VehicleError> {
        let signature = statics_signature(shared);
        if signature == self.statics_signature {
            return Ok(false);
        }
        let saved = self.vehicle.snapshot(&self.physics)?;
        // Checked before the old world goes: a restore that fails after it would leave a fresh car
        // standing at the saved pose while the call reported success (ADR-7417).
        saved.validate().map_err(|error| match error {
            VehicleSnapshotError::Vehicle(error) => error,
            _ => VehicleError::StateNotFinite,
        })?;
        let config = self.vehicle.config().clone();
        let mut physics = PhysicsWorld::default();
        copy_statics(&mut physics, shared, &self.settings, dt);
        let vehicle = Vehicle::spawn(&mut physics, config, saved.translation, saved.rotation)?;
        self.physics = physics;
        self.vehicle = vehicle;
        self.statics_signature = signature;
        // The same configuration took the snapshot, so it fits, and it was validated above.
        self.vehicle
            .restore(&mut self.physics, &saved)
            .map_err(|error| match error {
                VehicleSnapshotError::Vehicle(error) => error,
                _ => VehicleError::StateNotFinite,
            })?;
        Ok(true)
    }

    /// One fixed step: applies the input, then steps the private world.
    ///
    /// # Errors
    /// What [`Vehicle::update_with_layers`] reports; the world did not step then.
    pub fn step(
        &mut self,
        input: VehicleInput,
        dt: f32,
        layers: Option<&PhysicsLayers>,
    ) -> Result<(), VehicleError> {
        self.vehicle
            .update_with_layers(&mut self.physics, input, dt, layers)?;
        step_world(&mut self.physics, &self.settings, dt);
        Ok(())
    }

    /// A driver's gear change: see [`Vehicle::request_shift`].
    ///
    /// # Errors
    /// See [`Vehicle::request_shift`].
    pub fn request_shift(&mut self, request: ShiftRequest) -> Result<i32, VehicleError> {
        self.vehicle.request_shift(request)
    }

    /// Saves the vehicle.
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis is gone.
    pub fn snapshot(&self) -> Result<VehicleSnapshot, VehicleError> {
        self.vehicle.snapshot(&self.physics)
    }

    /// Restores a [`VehicleSnapshot`] (the module docs say when that is exact; a checkpoint always is).
    ///
    /// # Errors
    /// See [`Vehicle::restore`].
    pub fn restore(&mut self, snapshot: &VehicleSnapshot) -> Result<(), VehicleSnapshotError> {
        self.vehicle.restore(&mut self.physics, snapshot)
    }

    /// Goes on from a corrected state: see [`Vehicle::apply_core`].
    ///
    /// # Errors
    /// See [`Vehicle::apply_core`].
    pub fn apply_core(&mut self, core: &VehicleCore) -> Result<(), VehicleSnapshotError> {
        self.vehicle.apply_core(&mut self.physics, core)
    }

    /// Saves the vehicle and this world's contact caches, so [`Self::rewind`] is exact even with the
    /// chassis body in contact. Costs a copy of the world's bounding volume.
    ///
    /// # Errors
    /// [`VehicleError::ChassisMissing`] when the chassis is gone.
    pub fn checkpoint(&self) -> Result<VehicleCheckpoint, VehicleError> {
        Ok(VehicleCheckpoint {
            snapshot: self.snapshot()?,
            islands: self.physics.islands.clone(),
            broad_phase: self.physics.broad_phase.clone(),
            narrow_phase: self.physics.narrow_phase.clone(),
        })
    }

    /// Goes back to a checkpoint of this world: the next step gives the bits it gave after the save.
    ///
    /// # Errors
    /// See [`Vehicle::restore`]. A checkpoint taken before [`Self::sync_statics`] rebuilt the world
    /// names colliders that are gone: take a new one after a rebuild.
    pub fn rewind(&mut self, checkpoint: &VehicleCheckpoint) -> Result<(), VehicleSnapshotError> {
        self.restore(&checkpoint.snapshot)?;
        self.physics.islands = checkpoint.islands.clone();
        self.physics.broad_phase = checkpoint.broad_phase.clone();
        self.physics.narrow_phase = checkpoint.narrow_phase.clone();
        Ok(())
    }
}

/// `.vehicle-snapshot.ron`'s schema fingerprint for `cargo xtask check-formats`: the snapshot's own
/// fields and nested tire state. Its rapier wheel record can change with rapier, requiring a
/// separate version and migration decision when that happens.
#[must_use]
pub fn vehicle_snapshot_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<VehicleSnapshot>(),
        regia_core::schema::serde_field_names::<TireSnapshot>(),
    ])
}

/// Rewrites a v1 saved vehicle as v2, defaulting its tire transient state to zero.
/// Valid current files are returned unchanged, including comments.
///
/// # Errors
/// The same size, version, parse and state validation errors as [`VehicleSnapshot::from_ron`].
pub fn migrate_vehicle_snapshot(text: &str) -> Result<String, VehicleSnapshotError> {
    let snapshot = VehicleSnapshot::from_ron(text)?;
    let found = regia_core::version::peek_ron_version(text).map_err(VehicleSnapshotError::Parse)?;
    if found == VEHICLE_SNAPSHOT_FORMAT_VERSION {
        Ok(text.to_owned())
    } else {
        snapshot.to_ron()
    }
}

/// Older versions the reader migrates from: v1, before tire transient state.
#[must_use]
pub fn vehicle_snapshot_migration_sources() -> Vec<u32> {
    vec![1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot's error codes are the protocol's shape: lowercase ascii, digits and `_`.
    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            VehicleSnapshotError::Vehicle(VehicleError::ChassisMissing),
            VehicleSnapshotError::WheelCount {
                expected: 4,
                found: 2,
            },
            VehicleSnapshotError::ConfigMismatch { field: "wheels" },
            VehicleSnapshotError::NotFinite { field: "translation" },
            VehicleSnapshotError::UnsupportedVersion { found: 9 },
            VehicleSnapshotError::TooLarge { bytes: 9 },
            VehicleSnapshotError::Parse("x".into()),
        ];
        for error in errors {
            let code = error.code();
            assert!(!code.is_empty());
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{code}"
            );
        }
    }

    /// The tire-transient state (ADR-3276) survives a snapshot: the lag the next step starts from
    /// and the readouts. A snapshot written before tire transients reads them as 0.
    #[test]
    fn a_tire_snapshot_keeps_the_lag_and_the_transient_readouts() {
        let state = TireState {
            anchor: None,
            anchor_body: None,
            lag: 0.125,
            forces: TireForces {
                slip: true,
                lateral_force: 900.0,
                longitudinal_force: -40.0,
                load: 3500.0,
                slip_angle: 0.05,
                stuck: false,
                lagged_slip_angle: 0.03,
                aligning_torque: 12.5,
                camber: -0.02,
                camber_thrust: 18.0,
            },
            peak_angle: 0.0,
            peak_key: 0,
        };
        let snapshot = TireSnapshot::of(&state);
        assert!(snapshot.is_finite());
        assert_eq!(snapshot.state(), state);
        let text = ron::to_string(&snapshot).expect("tire snapshot serializes");
        let back: TireSnapshot = ron::from_str(&text).expect("tire snapshot parses");
        assert_eq!(back, snapshot);
        let old = "(anchor:None,anchor_body:None,slip:true,lateral_force:1.0,longitudinal_force:2.0,\
                   load:3.0,slip_angle:0.5,stuck:false)";
        let old: TireSnapshot = ron::from_str(old).expect("a pre-transient tire snapshot parses");
        assert_eq!(old.lag, 0.0);
        assert_eq!(old.state().lag, 0.0);
        assert_eq!(old.state().forces.aligning_torque, 0.0);
    }

    /// corr-low P7: `step_physics` clamps the solver iterations to 64 and the CCD substeps to 16,
    /// but the private world handed rapier the raw counts, so settings past the clamp stepped a
    /// car differently from the shared world it is meant to match. Both clamp now: a private
    /// world at the clamp and one far past it give the same bits, through a wall contact.
    #[test]
    fn the_private_world_clamps_the_solver_and_ccd_counts_like_the_live_step() {
        use crate::world::{MAX_CCD_SUBSTEPS, MAX_SOLVER_ITERATIONS};
        let dt = 1.0 / 60.0;
        let mut statics = PhysicsWorld::default();
        add_static_box(
            &mut statics,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::new(400.0, 1.0, 400.0),
        );
        add_static_box(
            &mut statics,
            Vec3::new(0.0, 3.0, -30.0),
            Vec3::new(10.0, 3.0, 0.5),
        );
        let world = |solver_iterations, ccd_substeps| {
            VehicleSimWorld::new(
                VehicleConfig::default(),
                &statics,
                VehicleSimSettings {
                    gravity: Vec3::new(0.0, -9.81, 0.0),
                    solver_iterations,
                    ccd_substeps,
                },
                (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
                dt,
            )
            .expect("private world")
        };
        let mut clamped = world(MAX_SOLVER_ITERATIONS, MAX_CCD_SUBSTEPS);
        let mut past = world(4 * MAX_SOLVER_ITERATIONS, 8 * MAX_CCD_SUBSTEPS);
        let drive = VehicleInput {
            throttle: 1.0,
            ..VehicleInput::default()
        };
        let mut after_contact = None;
        for t in 0..900 {
            clamped.step(drive, dt, None).expect("step");
            past.step(drive, dt, None).expect("step");
            let a = clamped.snapshot().expect("snapshot");
            let b = past.snapshot().expect("snapshot");
            assert_eq!(a.digest(), b.digest(), "step {t}");
            if a.chassis_in_contact && after_contact.is_none() {
                after_contact = Some(t);
            }
            if after_contact.is_some_and(|start| t >= start + 30) {
                break;
            }
        }
        assert!(after_contact.is_some(), "the car reached the wall");
    }

    /// Only the two non-finite spellings count; a field name with `inf` in it does not.
    #[test]
    fn the_token_scan_finds_only_values() {
        assert!(has_non_finite_token("(a:NaN)"));
        assert!(has_non_finite_token("(a:inf)"));
        assert!(has_non_finite_token("(a:-inf)"));
        assert!(!has_non_finite_token("(raycast_info:(x:1.0))"));
    }

    /// rc1 physics review, finding 3: `sync_statics` replaced the private world with a fresh
    /// car at the saved pose, then ignored every restore error but `Vehicle(..)`, so a car whose
    /// velocity had gone non-finite came back standing still while the call reported success.
    /// It refuses before touching anything now: the old world stays (statics and car) and the
    /// caller hears `vehicle_state_not_finite`.
    #[test]
    fn sync_statics_refuses_a_non_finite_car_without_replacing_the_world() {
        let dt = 1.0 / 60.0;
        let mut shared = PhysicsWorld::default();
        add_static_box(
            &mut shared,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::new(400.0, 1.0, 400.0),
        );
        let mut private = VehicleSimWorld::new(
            VehicleConfig::default(),
            &shared,
            VehicleSimSettings::default(),
            (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
            dt,
        )
        .expect("private world");
        for _ in 0..10 {
            private.step(VehicleInput::default(), dt, None).expect("step");
        }
        let chassis = private.vehicle.chassis().to_handle();
        private
            .physics
            .bodies
            .get_mut(chassis)
            .expect("chassis")
            .set_linvel(to_rapier_vec3(Vec3::new(f32::INFINITY, 0.0, 0.0)), true);
        let colliders = private.collider_count();
        add_static_box(&mut shared, Vec3::new(0.0, 3.0, -10.0), Vec3::new(10.0, 3.0, 0.5));
        let result = private.sync_statics(&shared, dt);
        assert_eq!(
            result.as_ref().map_err(VehicleError::code),
            Err("vehicle_state_not_finite"),
            "{result:?}"
        );
        assert_eq!(private.collider_count(), colliders, "the old world stays");
        assert_eq!(
            private.vehicle.chassis().to_handle(),
            chassis,
            "the old car stays"
        );
        assert!(
            !private.snapshot().expect("snapshot").linear_velocity.is_finite(),
            "nothing was silently reset"
        );
        // Once the car is finite again (an authoritative state arrives), the sync goes through.
        private
            .physics
            .bodies
            .get_mut(chassis)
            .expect("chassis")
            .set_linvel(to_rapier_vec3(Vec3::ZERO), true);
        assert_eq!(private.sync_statics(&shared, dt), Ok(true));
        assert_eq!(private.collider_count(), colliders + 1);
    }

    /// ADR-7431 (5): a static collider whose shape changes in place (same handle, same pose) is
    /// a change of the statics: the private world rebuilds once and carries the new shape. Before
    /// ADR-7431 the signature hashed handles and poses only, so the car kept driving against the
    /// old wall.
    #[test]
    fn an_in_place_shape_change_forces_one_rebuild() {
        let dt = 1.0 / 60.0;
        let mut shared = PhysicsWorld::default();
        add_static_box(
            &mut shared,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::new(400.0, 1.0, 400.0),
        );
        let wall = shared.colliders.insert(
            rapier3d::geometry::ColliderBuilder::cuboid(10.0, 3.0, 0.5)
                .translation(to_rapier_vec3(Vec3::new(0.0, 3.0, -10.0))),
        );
        let mut private = VehicleSimWorld::new(
            VehicleConfig::default(),
            &shared,
            VehicleSimSettings::default(),
            (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
            dt,
        )
        .expect("private world");
        assert_eq!(private.sync_statics(&shared, dt), Ok(false), "nothing changed");
        shared
            .colliders
            .get_mut(wall)
            .expect("wall")
            .set_shape(rapier3d::geometry::SharedShape::cuboid(10.0, 6.0, 0.5));
        assert_eq!(
            private.sync_statics(&shared, dt),
            Ok(true),
            "the wall grew in place"
        );
        assert_eq!(
            private.sync_statics(&shared, dt),
            Ok(false),
            "one rebuild, not one per call"
        );
        let tallest = private
            .physics()
            .colliders
            .iter()
            .filter_map(|(_, c)| c.shape().as_cuboid().map(|b| b.half_extents.y))
            .fold(0.0_f32, f32::max);
        assert_eq!(tallest, 6.0, "the private world carries the new shape");
    }
}
