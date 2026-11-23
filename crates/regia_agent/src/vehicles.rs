// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for ray-cast vehicles (`regia_physics::vehicle`, specs/23 § 3).
//!
//! - `physics.vehicles` reports every vehicle entity: chassis pose and speed, driver input, the
//!   last update's error code, and each wheel (grounded, compression, steer and spin angles,
//!   world center, contact, ground entity, suspension force).
//! - `physics.vehicle_input` sets an entity's `VehicleInput` (throttle, brake, steer); a hover
//!   craft (`hovercraft`) reads the same component.
//! - `physics.vehicle_reset` puts a vehicle back at rest, at its spawn pose or a given one, and
//!   releases its input.
//! - `physics.vehicle_telemetry` reports the drivetrain (ADR-2598): engine speed, gear, clutch,
//!   aids and each wheel's spin and slip.
//! - `physics.vehicle_shift` changes gear or hands the gears to the automatic.
//! - `physics.vehicle_tire_curve` reads a slip tire's side force against slip angle (ADR-2719),
//!   from a spawned vehicle or a `.vehicle.ron` text, without stepping anything; each point also
//!   carries the pneumatic trail and the self-aligning torque (ADR-3276).
//!
//! `physics.vehicle_telemetry` also reports the tire transients (ADR-3276): per wheel the lagged
//! slip angle, the aligning torque, the camber and the camber thrust; per vehicle the steering
//! torque (raw, for a force-feedback wheel), its split per steered axle, and the rumble channels
//! of a `VehicleFeedback` when the entity has one.
//!
//! Every handler answers `NOT_FOUND` (`F0003`) when the app has no `PhysicsWorld` or the entity
//! carries no `VehicleController`; vehicle errors name their stable code in brackets in the
//! message (`[vehicle_chassis_missing]`). See `book/agents/vehicles.md`.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::vehicle::{VehicleController, VehicleError, VehicleInput, WheelState};
use regia_physics::vehicle_asset::VehicleAsset;
use regia_physics::vehicle_drivetrain::{DrivetrainState, ShiftRequest};
use regia_physics::vehicle_tire::{
    CombinedSlip, LateralCurve, TireModel, steering_torques_into, tire_warnings, total_steering_torque,
};
use serde_json::{Value, json};

use crate::physics::vec3_param;
use crate::protocol::RpcError;

/// `[x, y, z]` as JSON.
fn arr3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// The `NOT_FOUND` answer when the app has no physics world.
fn no_physics() -> RpcError {
    RpcError::not_found(
        "no PhysicsWorld in this app",
        "add regia_physics::PhysicsPlugin and regia_physics::vehicle::VehiclePlugin",
    )
}

/// A vehicle error as an agent error: a missing chassis is `NOT_FOUND`, a bad pose is bad
/// parameters; the message starts with the stable code in brackets.
fn vehicle_error(error: &VehicleError) -> RpcError {
    let message = format!("[{}] {error}", error.code());
    match error {
        VehicleError::InvalidPose
        | VehicleError::InvalidTimeStep
        | VehicleError::NoGearbox
        | VehicleError::InvalidGear { .. } => RpcError::invalid_params(message),
        _ => RpcError::not_found(
            message,
            "re-run physics.vehicles; the vehicle may have been despawned",
        ),
    }
}

/// The required `entity` parameter (entity bits from `physics.vehicles` or `scene.*`).
fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let value = params
        .get("entity")
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params("`entity` (an id from physics.vehicles) is required"))?;
    value.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
        RpcError::invalid_params("`entity` must be an entity id (a u64 from physics.vehicles)")
    })
}

/// The `NOT_FOUND` answer for an entity that is not a vehicle.
fn not_a_vehicle(entity: Entity) -> RpcError {
    RpcError::not_found(
        format!("entity {} has no VehicleController", entity.to_bits()),
        "physics.vehicles lists the vehicle entities",
    )
}

/// An optional finite number parameter.
fn number_param(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            let n = n as f32;
            if !n.is_finite() {
                return Err(RpcError::invalid_params(format!(
                    "`{field}` must be a finite number"
                )));
            }
            Ok(Some(n))
        }
    }
}

/// One wheel as JSON.
fn wheel_json(wheel: &WheelState) -> Value {
    json!({
        "index": wheel.index,
        "grounded": wheel.grounded,
        "compression": wheel.compression,
        "suspension_length": wheel.suspension_length,
        "steer_angle": wheel.steer_angle,
        "spin_angle": wheel.spin_angle,
        "center": arr3(wheel.world_translation),
        "contact_point": wheel.contact_point.map(arr3),
        "contact_normal": wheel.contact_normal.map(arr3),
        "ground_entity": wheel.ground_entity.map(Entity::to_bits),
        "suspension_force": wheel.suspension_force,
        "slip_ratio": wheel.slip_ratio,
        "slip_angle": wheel.slip_angle,
    })
}

/// Input as JSON.
fn input_json(input: VehicleInput) -> Value {
    json!({ "throttle": input.throttle, "brake": input.brake, "steer": input.steer })
}

/// `physics.vehicles`: every vehicle (or the one named by `entity`), in entity order.
pub fn vehicles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(entity_param(params)?),
    };
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let mut query = world.query::<(Entity, &VehicleController, Option<&VehicleInput>)>();
    let mut rows: Vec<(Entity, &VehicleController, Option<&VehicleInput>)> = query.iter(world).collect();
    rows.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _, _)| *e == entity)
    {
        return Err(not_a_vehicle(entity));
    }
    let physics = world.resource::<PhysicsWorld>();
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, controller, input)| {
            let vehicle = &controller.vehicle;
            let mut row = json!({
                "entity": entity.to_bits(),
                "input": input_json(input.copied().unwrap_or_default()),
                "applied": input_json(vehicle.input()),
                "steps": vehicle.steps(),
                "last_error": controller.last_error.as_ref().map(VehicleError::code),
                "wheel_count": vehicle.config().wheels.len(),
                "config_digest": format!("{:016x}", vehicle.config().digest()),
                "config_revision": vehicle.config_revision(),
            });
            match vehicle.state(physics) {
                Ok(state) => {
                    row["position"] = arr3(state.translation);
                    row["rotation"] = json!([
                        state.rotation.x,
                        state.rotation.y,
                        state.rotation.z,
                        state.rotation.w
                    ]);
                    row["forward"] = arr3(state.forward);
                    row["forward_speed"] = json!(state.forward_speed);
                    row["linear_velocity"] = arr3(state.linear_velocity);
                    row["angular_velocity"] = arr3(state.angular_velocity);
                    row["speed"] = json!(state.speed);
                    row["rpm"] = json!(state.rpm);
                    row["gear"] = json!(state.gear);
                    row["grounded_wheels"] = json!(state.grounded_wheels);
                    row["wheels"] = Value::Array(state.wheels.iter().map(wheel_json).collect());
                    row["error"] = Value::Null;
                }
                Err(error) => row["error"] = json!(error.code()),
            }
            row
        })
        .collect();
    let count = list.len();
    Ok(json!({ "vehicles": list, "count": count }))
}

/// The drivetrain half of a telemetry row: `drivetrain: "simple"` and zeros without a gearbox.
fn drivetrain_json(row: &mut Value, drivetrain: Option<&DrivetrainState>) {
    let Some(state) = drivetrain else {
        row["drivetrain"] = json!("simple");
        return;
    };
    row["drivetrain"] = json!("geared");
    row["target_gear"] = json!(state.target_gear());
    row["shifting"] = json!(state.is_shifting());
    row["automatic"] = json!(state.is_automatic());
    row["upshifts"] = json!(state.upshifts());
    row["downshifts"] = json!(state.downshifts());
    row["last_shift_rpm"] = json!(state.last_shift_rpm());
    row["clutch_torque"] = json!(state.clutch_torque());
    row["clutch_slipping"] = json!(state.is_clutch_slipping());
    row["engine_torque"] = json!(state.engine_torque());
    row["applied_throttle"] = json!(state.applied_throttle());
    row["traction_factor"] = json!(state.traction_factor());
}

/// `physics.vehicle_telemetry`: rpm, gear, speed, the drivetrain and every wheel's slip, for every
/// vehicle (or the one named by `entity`), in entity order.
pub fn vehicle_telemetry(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(entity_param(params)?),
    };
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let mut query = world.query::<(
        Entity,
        &VehicleController,
        Option<&regia_framework::vehicle_feedback::VehicleFeedback>,
    )>();
    let mut rows: Vec<_> = query.iter(world).collect();
    rows.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _, _)| *e == entity)
    {
        return Err(not_a_vehicle(entity));
    }
    let physics = world.resource::<PhysicsWorld>();
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, controller, feedback)| {
            let vehicle = &controller.vehicle;
            let drivetrain = vehicle.drivetrain();
            let tires = vehicle.tire_forces();
            let models = &vehicle.config().wheels;
            let mut row = json!({
                "entity": entity.to_bits(),
                "steps": vehicle.steps(),
                "digest": format!("{:016x}", vehicle.state_digest()),
            });
            drivetrain_json(&mut row, drivetrain);
            transients_json(&mut row, vehicle.config(), &tires, feedback);
            match vehicle.state(physics) {
                Ok(state) => {
                    row["speed"] = json!(state.speed);
                    row["forward_speed"] = json!(state.forward_speed);
                    row["rpm"] = json!(state.rpm);
                    row["gear"] = json!(state.gear);
                    row["wheels"] = Value::Array(
                        state
                            .wheels
                            .iter()
                            .map(|wheel| {
                                let spin = drivetrain.and_then(|d| d.wheels().get(wheel.index));
                                let tire = tires.get(wheel.index).copied().unwrap_or_default();
                                let model = models.get(wheel.index).map_or("damper", |w| w.tire_model.name());
                                json!({
                                    "index": wheel.index,
                                    "grounded": wheel.grounded,
                                    "slip_ratio": wheel.slip_ratio,
                                    "slip_angle": wheel.slip_angle,
                                    "angular_speed": spin.map(|s| s.angular_speed),
                                    "tire_force": spin.map(|s| s.tire_force),
                                    "load": spin.map(|s| s.load),
                                    "drive_torque": spin.map(|s| s.drive_torque),
                                    "abs_factor": spin.map(|s| s.abs_factor),
                                    "tire_model": model,
                                    "lateral_force": tire.lateral_force,
                                    "longitudinal_force": tire.longitudinal_force,
                                    "tire_load": tire.load,
                                    "tire_slip_angle": tire.slip_angle,
                                    "stuck": tire.stuck,
                                    "lagged_slip_angle": tire.lagged_slip_angle,
                                    "aligning_torque": tire.aligning_torque,
                                    "camber": tire.camber,
                                    "camber_thrust": tire.camber_thrust,
                                })
                            })
                            .collect(),
                    );
                    row["error"] = Value::Null;
                }
                Err(error) => row["error"] = json!(error.code()),
            }
            row
        })
        .collect();
    let count = list.len();
    Ok(json!({ "vehicles": list, "count": count }))
}

/// The vehicle half of the transients (ADR-3276): the steering torque, its split per steered axle,
/// and the rumble channels of a `VehicleFeedback` (`null` without one).
fn transients_json(
    row: &mut Value,
    config: &regia_physics::vehicle::VehicleConfig,
    tires: &[regia_physics::vehicle_tire::TireForces],
    feedback: Option<&regia_framework::vehicle_feedback::VehicleFeedback>,
) {
    let mut axles = Vec::new();
    steering_torques_into(config, tires, &mut axles);
    row["steering_torque"] = json!(total_steering_torque(config, tires));
    row["steering_axles"] = Value::Array(
        axles
            .iter()
            .map(
                |axle| json!({ "position_z": axle.position_z, "wheels": axle.wheels, "torque": axle.torque }),
            )
            .collect(),
    );
    row["feedback"] = feedback.map_or(Value::Null, |feedback| {
        json!({
            "low": feedback.low,
            "high": feedback.high,
            "strength": feedback.strength,
            "device": feedback.device,
            "requests": feedback.requests,
        })
    });
}

/// The `[vehicle_tire_damper]` answer: a damper tire has no slip curve.
fn damper_tire(wheel: usize) -> RpcError {
    RpcError::invalid_params(format!(
        "[vehicle_tire_damper] wheel {wheel} has the damper tire (rapier's velocity damper), which has no slip curve; \
         give it tire_model: Slip((..)) in a .vehicle.ron v3"
    ))
}

/// `physics.vehicle_tire_curve`: a wheel's slip tire as force against slip angle. The wheel comes
/// from a spawned vehicle (`entity`) or from the text of a `.vehicle.ron` (`ron`), exactly one of
/// them; `wheel` (default 0), `load` (newtons, default the tire's `nominal_load`),
/// `max_slip_angle` (radians, default 0.5) and `points` (default 41) shape the read.
pub fn vehicle_tire_curve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let has = |field: &str| params.get(field).is_some_and(|v| !v.is_null());
    let config = match (has("entity"), has("ron")) {
        (true, false) => {
            let entity = entity_param(params)?;
            if !world.contains_resource::<PhysicsWorld>() {
                return Err(no_physics());
            }
            world
                .get::<VehicleController>(entity)
                .map(|controller| controller.vehicle.config().clone())
                .ok_or_else(|| not_a_vehicle(entity))?
        }
        (false, true) => {
            let text = params
                .get("ron")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`ron` must be the text of a .vehicle.ron"))?;
            VehicleAsset::from_ron(text)
                .map_err(|error| RpcError::invalid_params(format!("[{}] {error}", error.code())))?
                .into_config()
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `entity` (a vehicle) or `ron` (the text of a .vehicle.ron)",
            ));
        }
    };
    let wheel = match params.get("wheel").filter(|v| !v.is_null()) {
        None => 0,
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`wheel` must be a wheel index (0 and up)"))?,
    };
    let Some(wheel_config) = config.wheels.get(wheel) else {
        return Err(RpcError::invalid_params(format!(
            "[vehicle_tire_no_wheel] the vehicle has {} wheels; there is no wheel {wheel}",
            config.wheels.len()
        )));
    };
    let TireModel::Slip(tire) = &wheel_config.tire_model else {
        return Err(damper_tire(wheel));
    };
    let load = number_param(params, "load")?.unwrap_or(tire.nominal_load);
    if load <= 0.0 {
        return Err(RpcError::invalid_params("`load` must be above 0 newtons"));
    }
    let max_angle = number_param(params, "max_slip_angle")?.unwrap_or(0.5);
    // The contract's f64 π/2 rounds to this same f32 constant.
    if !(max_angle > 0.0 && max_angle <= std::f32::consts::FRAC_PI_2) {
        return Err(RpcError::invalid_params(
            "`max_slip_angle` must be above 0 and at most π/2 radians",
        ));
    }
    let points = match params.get("points").filter(|v| !v.is_null()) {
        None => 41,
        Some(value) => value
            .as_u64()
            .filter(|n| (2..=361).contains(n))
            .ok_or_else(|| RpcError::invalid_params("`points` must be an integer from 2 to 361"))?
            as usize,
    };
    let samples: Vec<Value> = (0..points)
        .map(|index| {
            let angle = max_angle * index as f32 / (points - 1) as f32;
            json!({
                "slip_angle": angle,
                "lateral_force": tire.lateral_force(angle, load),
                "pneumatic_trail": tire.pneumatic_trail_at(angle, load),
                "aligning_torque": tire.aligning_torque(angle, load),
            })
        })
        .collect();
    let curve = match tire.curve {
        LateralCurve::Brush => json!({ "kind": "brush" }),
        LateralCurve::MagicFormula { shape, curvature } => {
            json!({ "kind": "magic_formula", "shape": shape, "curvature": curvature })
        }
    };
    Ok(json!({
        "wheel": wheel,
        "tire_model": "slip",
        "curve": curve,
        "load": load,
        "load_factor": tire.load_factor(load),
        "friction": tire.friction_at_load(load),
        "peak_force": tire.peak_force(load),
        "peak_slip_angle": tire.peak_slip_angle(load),
        "cornering_stiffness": tire.cornering_stiffness_at(load),
        "roll_influence": tire.roll_influence,
        "stick_speed": tire.stick_speed,
        "relaxation_length": tire.relaxation_length,
        "pneumatic_trail": tire.pneumatic_trail,
        "camber_stiffness": tire.camber_stiffness_at(load),
        "static_camber": tire.static_camber,
        "combined_slip": match tire.combined_slip {
            CombinedSlip::Ellipse => json!({ "kind": "ellipse" }),
            CombinedSlip::SlipWeighted { shape, stiffness } => {
                json!({ "kind": "slip_weighted", "shape": shape, "stiffness": stiffness })
            }
        },
        "points": samples,
    }))
}

/// The `warnings` of `physics.vehicle_config_check`: tuning hints about the tires (ADR-2719).
pub(crate) fn config_warnings(config: &regia_physics::vehicle::VehicleConfig) -> Value {
    Value::Array(
        tire_warnings(config)
            .into_iter()
            .map(
                |warning| json!({ "code": warning.code, "wheel": warning.wheel, "message": warning.message }),
            )
            .collect(),
    )
}

/// `physics.vehicle_shift`: `gear` (−1 reverse, 0 neutral, 1 and up) or `shift` (`up`, `down`)
/// changes gear and takes the gears from the automatic; `automatic` hands them over or takes them.
pub fn vehicle_shift(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let gear = match params.get("gear").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .and_then(|gear| i32::try_from(gear).ok())
                .ok_or_else(|| RpcError::invalid_params("`gear` must be an integer"))?,
        ),
    };
    let shift = match params.get("shift").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => match value.as_str() {
            Some("up") => Some(ShiftRequest::Up),
            Some("down") => Some(ShiftRequest::Down),
            _ => return Err(RpcError::invalid_params("`shift` must be \"up\" or \"down\"")),
        },
    };
    let automatic = match params.get("automatic").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`automatic` must be a boolean"))?,
        ),
    };
    let request = match (gear, shift) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params("give `gear` or `shift`, not both"));
        }
        (Some(gear), None) => Some(ShiftRequest::Gear(gear)),
        (None, shift) => shift,
    };
    if request.is_none() && automatic.is_none() {
        return Err(RpcError::invalid_params("give `gear`, `shift` or `automatic`"));
    }
    let Some(mut controller) = world.get_mut::<VehicleController>(entity) else {
        return Err(not_a_vehicle(entity));
    };
    if let Some(request) = request {
        controller
            .vehicle
            .request_shift(request)
            .map_err(|error| vehicle_error(&error))?;
    }
    if let Some(automatic) = automatic {
        controller
            .vehicle
            .set_automatic_shifting(automatic)
            .map_err(|error| vehicle_error(&error))?;
    }
    let Some(state) = controller.vehicle.drivetrain() else {
        return Err(vehicle_error(&VehicleError::NoGearbox));
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "gear": state.gear(),
        "target_gear": state.target_gear(),
        "shifting": state.is_shifting(),
        "automatic": state.is_automatic(),
    }))
}

/// `physics.vehicle_input`: sets the fields given of an entity's `VehicleInput` (the others
/// keep their value) and returns the input before and after.
pub fn vehicle_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let throttle = number_param(params, "throttle")?;
    let brake = number_param(params, "brake")?;
    let steer = number_param(params, "steer")?;
    if world.get::<VehicleController>(entity).is_none()
        && world
            .get::<regia_physics::hover::HoverController>(entity)
            .is_none()
    {
        return Err(not_a_vehicle(entity));
    }
    let before = world.get::<VehicleInput>(entity).copied().unwrap_or_default();
    let after = VehicleInput {
        throttle: throttle.unwrap_or(before.throttle),
        brake: brake.unwrap_or(before.brake),
        steer: steer.unwrap_or(before.steer),
    }
    .sanitized();
    world.entity_mut(entity).insert(after);
    Ok(json!({ "entity": entity.to_bits(), "before": input_json(before), "after": input_json(after) }))
}

/// `physics.vehicle_reset`: the vehicle at rest at `position` (default: its spawn position) with
/// yaw `yaw` radians about +Y (default: its spawn rotation when no position is given, else no
/// rotation), its wheels cleared and its input released.
pub fn vehicle_reset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let position = match params.get("position").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(vec3_param(params, "position")?),
    };
    let yaw = number_param(params, "yaw")?;
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    if world.get::<VehicleController>(entity).is_none() {
        return Err(not_a_vehicle(entity));
    }
    let result = world.resource_scope::<PhysicsWorld, _>(|world, mut physics| {
        let Some(mut controller) = world.get_mut::<VehicleController>(entity) else {
            return Err(not_a_vehicle(entity));
        };
        let (spawn_t, spawn_r) = controller.vehicle.spawn_pose();
        let translation = position.unwrap_or(spawn_t);
        let rotation = match (yaw, position) {
            (Some(yaw), _) => Quat::from_rotation_y(yaw),
            (None, Some(_)) => Quat::IDENTITY,
            (None, None) => spawn_r,
        };
        controller
            .vehicle
            .reset(&mut physics, translation, rotation)
            .map_err(|error| vehicle_error(&error))?;
        controller.last_error = None;
        Ok((translation, rotation))
    });
    let (translation, rotation) = result?;
    world.entity_mut(entity).insert(VehicleInput::default());
    Ok(json!({
        "entity": entity.to_bits(),
        "position": arr3(translation),
        "rotation": [rotation.x, rotation.y, rotation.z, rotation.w],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_physics::vehicle::{VehicleConfig, VehiclePlugin, spawn_vehicle};
    use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};

    /// Asserts a JSON `[x, y, z]` equals `want` within f32 precision.
    ///
    /// `arr3` serializes `f32` components. Depending on which crates unify `serde_json`
    /// features, that is the f32 widened to f64 (`0.8999999761581421`) or the shortest f32
    /// text (`0.9`), so exact equality against an f64 literal is build-dependent.
    fn assert_vec3_close(got: &Value, want: [f32; 3]) {
        let got = got.as_array().expect("a 3-array");
        assert_eq!(got.len(), 3, "{got:?}");
        for (g, w) in got.iter().zip(want) {
            let g = g.as_f64().expect("number") as f32;
            assert!(
                (g - w).abs() <= f32::EPSILON * w.abs().max(1.0) * 4.0,
                "{g} vs {w}"
            );
        }
    }

    /// A headless app with physics, vehicles and a flat ground entity (top at y = 0).
    fn app() -> (App, Entity) {
        use regia_physics::{Collider, ColliderShape, RigidBody};
        use regia_scene::Transform;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, VehiclePlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(1.0 / 60.0),
            ));
        let ground = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(100.0, 0.5, 100.0),
                    },
                    ..Collider::default()
                },
            ))
            .id();
        // The sync chain creates the ground's rapier collider.
        for _ in 0..3 {
            app.update();
        }
        (app, ground)
    }

    fn spawn_car(app: &mut App) -> Entity {
        spawn_vehicle(
            app.world_mut(),
            VehicleConfig::default(),
            Vec3::new(0.0, 0.9, 0.0),
            Quat::IDENTITY,
        )
        .expect("spawn")
    }

    fn spawn_geared(app: &mut App) -> Entity {
        spawn_vehicle(
            app.world_mut(),
            VehicleConfig {
                drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
                ..VehicleConfig::default()
            },
            Vec3::new(0.0, 0.9, 0.0),
            Quat::IDENTITY,
        )
        .expect("spawn")
    }

    #[test]
    fn telemetry_reports_the_engine_gear_and_slip_of_a_driven_car() {
        let (mut app, _) = app();
        let car = spawn_geared(&mut app);
        for _ in 0..60 {
            app.update();
        }
        let idle = vehicle_telemetry(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("idle");
        let row = &idle["vehicles"][0];
        assert_eq!(row["drivetrain"], "geared");
        assert_eq!(row["gear"], 1);
        assert_eq!(row["target_gear"], 1);
        assert_eq!(row["automatic"], true);
        assert!((row["rpm"].as_f64().expect("rpm") - 900.0).abs() < 1.0, "{row}");
        assert_eq!(row["wheels"].as_array().expect("wheels").len(), 4);
        assert_eq!(row["digest"].as_str().map(str::len), Some(16));

        vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "throttle": 1.0 }),
        )
        .expect("input");
        for _ in 0..60 {
            app.update();
        }
        let moving = vehicle_telemetry(app.world_mut(), &json!({})).expect("moving");
        let row = &moving["vehicles"][0];
        assert_eq!(moving["count"], 1);
        assert!(row["rpm"].as_f64().expect("rpm") > 1500.0, "{row}");
        assert!(row["speed"].as_f64().expect("speed") > 1.0, "{row}");
        assert!(
            row["wheels"][2]["slip_ratio"].as_f64().expect("slip") > 0.0,
            "{row}"
        );
        assert!(
            row["wheels"][2]["angular_speed"].as_f64().expect("spin") > 1.0,
            "{row}"
        );
        // physics.vehicles carries the headline numbers too.
        let report = vehicles(app.world_mut(), &json!({})).expect("report");
        assert_eq!(report["vehicles"][0]["gear"], row["gear"]);
        assert!(report["vehicles"][0]["wheels"][2]["slip_ratio"].is_number());
    }

    #[test]
    fn telemetry_of_a_simple_car_says_so() {
        let (mut app, _) = app();
        let car = spawn_car(&mut app);
        for _ in 0..3 {
            app.update();
        }
        let report =
            vehicle_telemetry(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("telemetry");
        let row = &report["vehicles"][0];
        assert_eq!(row["drivetrain"], "simple");
        assert_eq!(row["rpm"], 0.0);
        assert_eq!(row["gear"], 0);
        assert!(row["wheels"][0]["angular_speed"].is_null());
        assert!(row.get("clutch_torque").is_none());
    }

    #[test]
    fn shift_changes_gear_and_hands_the_gears_back() {
        let (mut app, _) = app();
        let car = spawn_geared(&mut app);
        for _ in 0..30 {
            app.update();
        }
        let id = car.to_bits();
        let set = vehicle_shift(app.world_mut(), &json!({ "entity": id, "gear": 3 })).expect("shift");
        assert_eq!(set["target_gear"], 3);
        assert_eq!(set["gear"], 1, "the old gear holds until the shift ends");
        assert_eq!(set["shifting"], true);
        assert_eq!(set["automatic"], false);
        for _ in 0..30 {
            app.update();
        }
        let row = vehicle_telemetry(app.world_mut(), &json!({ "entity": id })).expect("telemetry");
        assert_eq!(row["vehicles"][0]["gear"], 3);

        let up = vehicle_shift(app.world_mut(), &json!({ "entity": id, "shift": "up" })).expect("up");
        assert_eq!(up["target_gear"], 4);
        let down = vehicle_shift(app.world_mut(), &json!({ "entity": id, "shift": "down" })).expect("down");
        assert_eq!(down["target_gear"], 3);
        let back = vehicle_shift(app.world_mut(), &json!({ "entity": id, "automatic": true })).expect("auto");
        assert_eq!(back["automatic"], true);
        assert_eq!(back["target_gear"], 3);
        let reverse = vehicle_shift(app.world_mut(), &json!({ "entity": id, "gear": -1 })).expect("reverse");
        assert_eq!(reverse["target_gear"], -1);
    }

    #[test]
    fn the_contract_lets_a_bad_gear_reach_its_stable_code() {
        let contract = crate::contract::contract("physics.vehicle_shift");
        for gear in [99, -2] {
            crate::contract::check_params(contract, &json!({ "entity": 1, "gear": gear }))
                .expect("the handler answers for a gear, with vehicle_invalid_gear");
        }
    }

    #[test]
    fn shift_refuses_bad_requests_with_stable_codes() {
        let (mut app, _) = app();
        let geared = spawn_geared(&mut app);
        let simple = spawn_car(&mut app);
        let plain = app.world_mut().spawn_empty().id();
        let code = |error: RpcError| error.data.expect("data")["code"].clone();
        for params in [
            json!({ "entity": geared.to_bits() }),
            json!({ "entity": geared.to_bits(), "gear": "first" }),
            json!({ "entity": geared.to_bits(), "gear": 2.5 }),
            json!({ "entity": geared.to_bits(), "shift": "sideways" }),
            json!({ "entity": geared.to_bits(), "automatic": 1 }),
            json!({ "entity": geared.to_bits(), "gear": 2, "shift": "up" }),
            json!({ "gear": 2 }),
        ] {
            let error = vehicle_shift(app.world_mut(), &params).expect_err("refused");
            assert_eq!(code(error), json!("F0002"), "{params}");
        }
        let gear = vehicle_shift(
            app.world_mut(),
            &json!({ "entity": geared.to_bits(), "gear": 99 }),
        )
        .expect_err("no such gear");
        assert!(
            gear.message.starts_with("[vehicle_invalid_gear]"),
            "{}",
            gear.message
        );
        assert_eq!(code(gear), json!("F0002"));
        for params in [
            json!({ "entity": simple.to_bits(), "gear": 2 }),
            json!({ "entity": simple.to_bits(), "automatic": true }),
        ] {
            let none = vehicle_shift(app.world_mut(), &params).expect_err("no gearbox");
            assert!(
                none.message.starts_with("[vehicle_no_gearbox]"),
                "{}",
                none.message
            );
            assert_eq!(code(none), json!("F0002"));
        }
        for result in [
            vehicle_shift(app.world_mut(), &json!({ "entity": plain.to_bits(), "gear": 2 })),
            vehicle_telemetry(app.world_mut(), &json!({ "entity": plain.to_bits() })),
        ] {
            assert_eq!(code(result.expect_err("not a vehicle")), json!("F0003"));
        }
        let mut bare = App::new();
        let error = vehicle_telemetry(bare.world_mut(), &json!({})).expect_err("no physics");
        assert_eq!(code(error), json!("F0003"));
    }

    #[test]
    fn vehicles_reports_a_settled_car_on_its_wheels() {
        let (mut app, ground) = app();
        let car = spawn_car(&mut app);
        for _ in 0..200 {
            app.update();
        }
        let report = vehicles(app.world_mut(), &json!({})).expect("physics.vehicles");
        assert_eq!(report["count"], 1);
        let row = &report["vehicles"][0];
        assert_eq!(row["entity"], json!(car.to_bits()));
        assert_eq!(row["grounded_wheels"], 4);
        assert_eq!(row["wheel_count"], 4);
        assert!(row["last_error"].is_null());
        assert!(row["error"].is_null());
        let wheels = row["wheels"].as_array().expect("wheels");
        assert_eq!(wheels.len(), 4);
        for wheel in wheels {
            assert_eq!(wheel["grounded"], true);
            assert!(wheel["compression"].as_f64().expect("compression") > 0.0);
            assert_eq!(wheel["ground_entity"], json!(ground.to_bits()));
        }
        // Filtered by entity.
        let one = vehicles(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("one");
        assert_eq!(one["count"], 1);
    }

    #[test]
    fn vehicle_input_drives_the_car_forward_and_reset_brings_it_back() {
        let (mut app, _) = app();
        let car = spawn_car(&mut app);
        for _ in 0..120 {
            app.update();
        }
        let set = vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "throttle": 1.0 }),
        )
        .expect("physics.vehicle_input");
        assert_eq!(set["before"]["throttle"], 0.0);
        assert_eq!(set["after"]["throttle"], 1.0);
        assert_eq!(set["after"]["steer"], 0.0);
        for _ in 0..90 {
            app.update();
        }
        let report = vehicles(app.world_mut(), &json!({})).expect("report");
        let row = &report["vehicles"][0];
        assert!(row["forward_speed"].as_f64().expect("speed") > 3.0, "{row}");
        assert!(row["position"][2].as_f64().expect("z") < -2.0, "{row}");
        assert_eq!(row["applied"]["throttle"], 1.0);

        let reset = vehicle_reset(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("reset");
        assert_vec3_close(&reset["position"], [0.0, 0.9, 0.0]);
        let report = vehicles(app.world_mut(), &json!({})).expect("report");
        let row = &report["vehicles"][0];
        assert_eq!(row["forward_speed"], 0.0);
        assert_eq!(row["input"]["throttle"], 0.0);
        assert_eq!(row["steps"], 0);

        // An explicit pose with a yaw.
        let moved = vehicle_reset(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "position": [5.0, 1.0, 0.0], "yaw": 1.5707964 }),
        )
        .expect("reset to pose");
        assert_vec3_close(&moved["position"], [5.0, 1.0, 0.0]);
    }

    #[test]
    fn out_of_range_input_is_clamped_and_bad_params_are_refused() {
        let (mut app, _) = app();
        let car = spawn_car(&mut app);
        let set = vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "steer": -4.0, "brake": 0.5 }),
        )
        .expect("input");
        assert_eq!(set["after"]["steer"], -1.0);
        assert_eq!(set["after"]["brake"], 0.5);
        let bad = vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "throttle": "full" }),
        )
        .expect_err("a string throttle");
        assert_eq!(bad.data.expect("data")["code"], json!("F0002"));
        let missing = vehicle_input(app.world_mut(), &json!({})).expect_err("no entity");
        assert_eq!(missing.data.expect("data")["code"], json!("F0002"));
    }

    #[test]
    fn non_vehicles_and_missing_worlds_are_not_found() {
        let (mut app, _) = app();
        let plain = app.world_mut().spawn_empty().id();
        for result in [
            vehicles(app.world_mut(), &json!({ "entity": plain.to_bits() })),
            vehicle_input(
                app.world_mut(),
                &json!({ "entity": plain.to_bits(), "throttle": 1.0 }),
            ),
            vehicle_reset(app.world_mut(), &json!({ "entity": plain.to_bits() })),
        ] {
            let error = result.expect_err("not a vehicle");
            assert_eq!(error.data.expect("data")["code"], json!("F0003"));
        }
        let mut bare = App::new();
        let error = vehicles(bare.world_mut(), &json!({})).expect_err("no physics");
        assert_eq!(error.data.expect("data")["code"], json!("F0003"));
    }

    #[test]
    fn a_vanished_chassis_reports_its_error_code() {
        let (mut app, _) = app();
        let car = spawn_car(&mut app);
        let handle = app
            .world()
            .get::<regia_physics::RigidBodyHandle>(car)
            .expect("handle")
            .to_handle();
        app.world_mut().resource_mut::<PhysicsWorld>().remove_body(handle);
        for _ in 0..3 {
            app.update();
        }
        let report = vehicles(app.world_mut(), &json!({})).expect("report");
        let row = &report["vehicles"][0];
        assert_eq!(row["error"], "vehicle_chassis_missing");
        assert_eq!(row["last_error"], "vehicle_chassis_missing");
        let error = vehicle_reset(app.world_mut(), &json!({ "entity": car.to_bits() })).expect_err("gone");
        assert!(
            error.message.starts_with("[vehicle_chassis_missing]"),
            "{}",
            error.message
        );
        assert_eq!(error.data.expect("data")["code"], json!("F0003"));
    }

    const SLIP_CAR: &str = r#"(
        format_version: 3,
        name: "slip car",
        config: (
            wheels: [
                (position: (-0.8, -0.2, -1.3), steer: true, tire_model: Slip(())),
                (position: (0.8, -0.2, -1.3), steer: true, tire_model: Slip(())),
                (position: (-0.8, -0.2, 1.3), drive: true,
                 tire_model: Slip((curve: MagicFormula(shape: 1.3, curvature: -0.5)))),
                (position: (0.8, -0.2, 1.3), drive: true),
            ],
        ),
    )"#;

    fn code_of(error: &RpcError) -> Value {
        error.data.clone().expect("data")["code"].clone()
    }

    #[test]
    fn tire_curve_reads_force_against_slip_from_a_file_and_a_vehicle() {
        let mut world = World::new();
        let curve = vehicle_tire_curve(&mut world, &json!({ "ron": SLIP_CAR, "points": 21 })).expect("curve");
        assert_eq!(curve["tire_model"], "slip");
        assert_eq!(curve["curve"]["kind"], "brush");
        let points = curve["points"].as_array().expect("points");
        assert_eq!(points.len(), 21);
        assert_eq!(points[0]["lateral_force"].as_f64(), Some(0.0));
        let peak = curve["peak_force"].as_f64().expect("peak");
        assert!((peak - 3000.0).abs() < 1.0, "{peak}");
        // Opposes the slip, never past the peak, and flat once the brush model slides.
        let forces: Vec<f64> = points
            .iter()
            .map(|p| p["lateral_force"].as_f64().expect("f"))
            .collect();
        assert!(forces[1..].iter().all(|f| *f < 0.0 && -f <= peak + 1e-3));
        assert!((forces[20] + peak).abs() < 1e-3);
        let mf = vehicle_tire_curve(
            &mut world,
            &json!({ "ron": SLIP_CAR, "wheel": 2, "load": 4000.0 }),
        )
        .expect("mf");
        assert_eq!(mf["curve"]["kind"], "magic_formula");
        assert_eq!(mf["load"].as_f64(), Some(4000.0));
        assert_eq!(mf["points"].as_array().map(Vec::len), Some(41));
        // The contract's upper bound, π/2, is accepted and reads a fully sideways slide.
        let side = vehicle_tire_curve(
            &mut world,
            &json!({ "ron": SLIP_CAR, "max_slip_angle": std::f64::consts::FRAC_PI_2, "points": 3 }),
        )
        .expect("π/2");
        let last = side["points"][2]["lateral_force"].as_f64().expect("f");
        assert!((last + peak).abs() < 1e-3, "{last}");
        assert!(
            crate::contract::check_params(
                crate::contract::contract("physics.vehicle_tire_curve"),
                &json!({ "ron": SLIP_CAR, "max_slip_angle": std::f64::consts::FRAC_PI_2 })
            )
            .is_ok()
        );
        // A damper wheel, a missing wheel, both sources or neither: stable codes in F0002.
        let damper =
            vehicle_tire_curve(&mut world, &json!({ "ron": SLIP_CAR, "wheel": 3 })).expect_err("damper");
        assert_eq!(code_of(&damper), json!("F0002"));
        assert!(
            damper.message.contains("[vehicle_tire_damper]"),
            "{}",
            damper.message
        );
        let none = vehicle_tire_curve(&mut world, &json!({ "ron": SLIP_CAR, "wheel": 9 })).expect_err("none");
        assert!(
            none.message.contains("[vehicle_tire_no_wheel]"),
            "{}",
            none.message
        );
        for params in [
            json!({}),
            json!({ "ron": SLIP_CAR, "entity": 1 }),
            json!({ "ron": SLIP_CAR, "points": 1 }),
            json!({ "ron": SLIP_CAR, "max_slip_angle": 0.0 }),
            json!({ "ron": SLIP_CAR, "load": -1.0 }),
            json!({ "ron": "((" }),
        ] {
            let error = vehicle_tire_curve(&mut world, &params).expect_err("refused");
            assert_eq!(code_of(&error), json!("F0002"), "{params}");
        }
        // The same tire through a spawned vehicle.
        let (mut app, _) = app();
        let config = VehicleAsset::from_ron(SLIP_CAR).expect("asset").into_config();
        let car =
            spawn_vehicle(app.world_mut(), config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
        let spawned = vehicle_tire_curve(app.world_mut(), &json!({ "entity": car.to_bits(), "points": 21 }))
            .expect("spawned");
        assert_eq!(spawned["points"], curve["points"]);
        let plain = app.world_mut().spawn_empty().id();
        let error =
            vehicle_tire_curve(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("plain");
        assert_eq!(code_of(&error), json!("F0003"));
    }

    #[test]
    fn telemetry_reports_each_tire_s_model_side_force_and_load() {
        let (mut app, _) = app();
        let config = VehicleAsset::from_ron(SLIP_CAR).expect("asset").into_config();
        let car =
            spawn_vehicle(app.world_mut(), config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
        for _ in 0..60 {
            app.update();
        }
        vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "throttle": 1.0, "steer": 0.5 }),
        )
        .expect("input");
        for _ in 0..90 {
            app.update();
        }
        let row = vehicle_telemetry(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("telemetry");
        let wheels = row["vehicles"][0]["wheels"].as_array().expect("wheels").clone();
        let models: Vec<&str> = wheels
            .iter()
            .map(|w| w["tire_model"].as_str().expect("model"))
            .collect();
        assert_eq!(models, ["slip", "slip", "slip", "damper"]);
        for wheel in &wheels {
            assert!(wheel["tire_load"].as_f64().expect("load") > 500.0, "{wheel}");
            assert!(wheel["stuck"].is_boolean());
        }
        // Turning right, the slip tires pull toward the wheel's right.
        assert!(
            wheels[0]["lateral_force"].as_f64().expect("f") > 50.0,
            "{wheels:?}"
        );
        assert!(wheels[0]["tire_slip_angle"].as_f64().expect("a") < 0.0);
    }

    #[test]
    fn config_check_warns_about_odd_tire_tunings() {
        let mut world = World::new();
        let check =
            crate::hovercraft::vehicle_config_check(&mut world, &json!({ "ron": SLIP_CAR })).expect("check");
        assert_eq!(check["slip_tires"], 3);
        let warnings = check["warnings"].as_array().expect("warnings");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["code"], "tire_mixed_axle");
        assert_eq!(warnings[0]["wheel"], 2);
        let plain = crate::hovercraft::vehicle_config_check(
            &mut world,
            &json!({ "ron": r#"(format_version: 2, name: "x", config: ())"# }),
        )
        .expect("plain");
        assert_eq!(plain["warnings"], json!([]));
        assert_eq!(plain["slip_tires"], 0);
    }

    const TRANSIENT_CAR: &str = r#"(
        format_version: 4,
        name: "transient car",
        config: (
            wheels: [
                (position: (-0.8, -0.2, -1.3), steer: true,
                 tire_model: Slip((relaxation_length: 0.4, pneumatic_trail: 0.04,
                                   camber_stiffness: 4000.0, static_camber: -0.02))),
                (position: (0.8, -0.2, -1.3), steer: true,
                 tire_model: Slip((relaxation_length: 0.4, pneumatic_trail: 0.04,
                                   camber_stiffness: 4000.0, static_camber: -0.02))),
                (position: (-0.8, -0.2, 1.3), drive: true,
                 tire_model: Slip((combined_slip: SlipWeighted(shape: 1.0, stiffness: 10.0)))),
                (position: (0.8, -0.2, 1.3), drive: true,
                 tire_model: Slip((combined_slip: SlipWeighted(shape: 1.0, stiffness: 10.0)))),
            ],
        ),
    )"#;

    #[test]
    fn tire_curve_reports_the_trail_and_the_aligning_torque() {
        let mut world = World::new();
        let curve =
            vehicle_tire_curve(&mut world, &json!({ "ron": TRANSIENT_CAR, "points": 41 })).expect("curve");
        assert_eq!(curve["relaxation_length"].as_f64(), Some(f64::from(0.4_f32)));
        assert_eq!(curve["pneumatic_trail"].as_f64(), Some(f64::from(0.04_f32)));
        assert_eq!(curve["combined_slip"]["kind"], "ellipse");
        assert!(curve["camber_stiffness"].as_f64().expect("c") > 0.0);
        let points = curve["points"].as_array().expect("points");
        assert_eq!(points[0]["pneumatic_trail"].as_f64(), Some(f64::from(0.04_f32)));
        assert_eq!(points[0]["aligning_torque"], 0.0);
        // Positive slip: the torque turns the wheel right (negative), back toward its path; it
        // peaks before the force and is gone at full slide.
        let torques: Vec<f64> = points
            .iter()
            .map(|p| p["aligning_torque"].as_f64().expect("t"))
            .collect();
        assert!(torques[1] < 0.0);
        assert_eq!(*torques.last().expect("last"), 0.0);
        let rear =
            vehicle_tire_curve(&mut world, &json!({ "ron": TRANSIENT_CAR, "wheel": 2 })).expect("rear");
        assert_eq!(rear["combined_slip"]["kind"], "slip_weighted");
        assert_eq!(rear["points"][3]["aligning_torque"], 0.0);
    }

    #[test]
    fn telemetry_reports_the_transients_the_steering_torque_and_the_feedback() {
        let (mut app, _) = app();
        let config = VehicleAsset::from_ron(TRANSIENT_CAR)
            .expect("asset")
            .into_config();
        let car =
            spawn_vehicle(app.world_mut(), config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
        app.world_mut()
            .entity_mut(car)
            .insert(regia_framework::vehicle_feedback::VehicleFeedback::default());
        for _ in 0..60 {
            app.update();
        }
        vehicle_input(
            app.world_mut(),
            &json!({ "entity": car.to_bits(), "throttle": 0.6, "steer": 0.6 }),
        )
        .expect("input");
        for _ in 0..90 {
            app.update();
            // What `VehicleFeedbackPlugin` does after every fixed step.
            let world = app.world_mut();
            let controller = world.get::<VehicleController>(car).expect("car").clone();
            if let Some(mut feedback) =
                world.get_mut::<regia_framework::vehicle_feedback::VehicleFeedback>(car)
            {
                feedback.step(&controller, 1.0 / 60.0);
            }
        }
        let report =
            vehicle_telemetry(app.world_mut(), &json!({ "entity": car.to_bits() })).expect("telemetry");
        let row = &report["vehicles"][0];
        // Turning right: the tires push the wheels back left.
        let torque = row["steering_torque"].as_f64().expect("torque");
        assert!(torque > 0.0, "{row}");
        let axles = row["steering_axles"].as_array().expect("axles");
        assert_eq!(axles.len(), 1);
        assert_eq!(axles[0]["wheels"], 2);
        let feedback = &row["feedback"];
        assert!(feedback["low"].as_f64().expect("low") > 0.0, "{feedback}");
        assert!((0.0..=1.0).contains(&feedback["strength"].as_f64().expect("s")));
        assert_eq!(feedback["device"], Value::Null);
        let front = &row["wheels"][0];
        for field in ["lagged_slip_angle", "aligning_torque", "camber", "camber_thrust"] {
            assert!(front[field].is_number(), "{field}: {front}");
        }
        assert!(front["camber"].as_f64().expect("camber").abs() > 0.0);
        assert!(front["aligning_torque"].as_f64().expect("mz") > 0.0, "{front}");
        // A car with no feedback component reports null.
        let plain = spawn_vehicle(
            app.world_mut(),
            VehicleConfig::default(),
            Vec3::new(20.0, 0.9, 0.0),
            Quat::IDENTITY,
        )
        .expect("plain");
        app.update();
        let report =
            vehicle_telemetry(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect("plain");
        assert_eq!(report["vehicles"][0]["feedback"], Value::Null);
        assert_eq!(report["vehicles"][0]["steering_torque"], 0.0);
    }
}
