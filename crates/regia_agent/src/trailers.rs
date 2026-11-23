// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for trailers (`regia_physics::vehicle_trailer`, ADR-3281).
//!
//! - `physics.trailers` reports every trailer entity: coupled or not, the tow and front
//!   entities, the place in the train, the coupling kind and limits, the articulation angle and
//!   rate, the jackknife ratio and warning, the applied brake, the coupling gap, pose and speed.
//! - `physics.trailer_couple` couples a trailer entity to a tow entity's `TowHitch`.
//! - `physics.trailer_uncouple` lets a trailer go onto its stand.
//! - `physics.trailer_config_check` parses and validates a `.trailer.ron` text without spawning.
//!
//! The app needs `PhysicsPlugin` and `TrailerPlugin` (`NOT_FOUND`, `F0003`, without a physics
//! world or for an entity that is not a trailer). Trailer errors name their stable code in brackets
//! in the message (`[trailer_out_of_reach]`). See `book/agents/trailers.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::PhysicsWorld;
use regia_physics::vehicle_trailer::{
    CouplingKind, TrailerBrakes, TrailerController, TrailerError, TrailerState, couple_trailer,
    uncouple_trailer,
};
use regia_physics::vehicle_trailer_asset::TrailerAsset;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `[x, y, z]` as JSON.
fn arr3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// The `NOT_FOUND` answer when the app has no physics world.
fn no_physics() -> RpcError {
    RpcError::not_found(
        "no PhysicsWorld in this app",
        "add regia_physics::PhysicsPlugin and regia_physics::vehicle_trailer::TrailerPlugin",
    )
}

/// A trailer error as an agent error: a missing entity or body is `NOT_FOUND`, anything else is
/// bad parameters; the message starts with the stable code in brackets.
fn trailer_error(error: &TrailerError) -> RpcError {
    let message = format!("[{}] {error}", error.code());
    match error {
        TrailerError::NotATrailer | TrailerError::TowMissing => {
            RpcError::not_found(message, "physics.trailers lists the trailer entities")
        }
        TrailerError::NoPhysicsWorld => no_physics(),
        _ => RpcError::invalid_params(message),
    }
}

/// A required entity parameter.
fn entity_param(params: &Value, field: &str) -> Result<Entity, RpcError> {
    let value = params
        .get(field)
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (an entity id) is required")))?;
    value
        .as_u64()
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an entity id (a u64)")))
}

/// The `NOT_FOUND` answer for an entity that is not a trailer.
fn not_a_trailer(entity: Entity) -> RpcError {
    RpcError::not_found(
        format!(
            "[trailer_not_a_trailer] entity {} has no TrailerController",
            entity.to_bits()
        ),
        "physics.trailers lists the trailer entities",
    )
}

/// The coupling section of a row.
fn coupling_json(kind: &CouplingKind, warning_angle: f32, capture_radius: f32) -> Value {
    let (pitch, roll) = match *kind {
        CouplingKind::TowBall {
            pitch_limit,
            roll_limit,
            ..
        } => (pitch_limit, Some(roll_limit)),
        CouplingKind::FifthWheel { pitch_limit, .. } => (pitch_limit, None),
    };
    json!({
        "kind": kind.name(),
        "yaw_limit": kind.yaw_limit(),
        "pitch_limit": pitch,
        "roll_limit": roll,
        "jackknife_warning_angle": warning_angle,
        "capture_radius": capture_radius,
    })
}

/// `physics.trailers`: every trailer (or the one named by `entity`), in entity order.
pub fn trailers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(entity_param(params, "entity")?),
    };
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let mut query = world.query::<(Entity, &TrailerController, Option<&TrailerState>)>();
    let mut rows: Vec<(Entity, &TrailerController, Option<&TrailerState>)> = query.iter(world).collect();
    rows.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _, _)| *e == entity)
    {
        return Err(not_a_trailer(entity));
    }
    let physics = world.resource::<PhysicsWorld>();
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, controller, state)| {
            let trailer = &controller.trailer;
            let config = trailer.config();
            let state = state.copied().unwrap_or_default();
            let mut row = json!({
                "entity": entity.to_bits(),
                "coupled": trailer.is_coupled(),
                "tow": controller.tow.map(Entity::to_bits),
                "front": state.root.map(Entity::to_bits),
                "train_position": state.train_position,
                "coupling": coupling_json(
                    &config.coupling.kind,
                    config.coupling.jackknife_warning_angle,
                    config.coupling.capture_radius,
                ),
                "brakes": config.brakes.name(),
                "has_stand": config.stand.is_some(),
                "rear_hitch": config.rear_hitch.map(arr3),
                "last_error": controller.last_error.as_ref().map(TrailerError::code),
            });
            match (trailer.readout(physics), trailer.vehicle().state(physics)) {
                (Ok(readout), Ok(vehicle)) => {
                    row["articulation_angle"] = json!(readout.articulation_angle);
                    row["articulation_angle_deg"] = json!(readout.articulation_angle.to_degrees());
                    row["articulation_rate"] = json!(readout.articulation_rate);
                    row["relative_pitch"] = json!(readout.relative_pitch);
                    row["jackknife_ratio"] = json!(readout.jackknife_ratio);
                    row["jackknife_warning"] = json!(readout.jackknife_warning);
                    row["brake"] = json!(readout.brake);
                    row["coupling_gap"] = json!(readout.coupling_gap);
                    row["forward_speed"] = json!(readout.forward_speed);
                    row["position"] = arr3(vehicle.translation);
                    row["forward"] = arr3(vehicle.forward);
                    row["grounded_wheels"] = json!(vehicle.grounded_wheels);
                    row["error"] = Value::Null;
                }
                (Err(error), _) => row["error"] = json!(error.code()),
                (_, Err(error)) => row["error"] = json!(error.code()),
            }
            row
        })
        .collect();
    let count = list.len();
    Ok(json!({ "trailers": list, "count": count }))
}

/// `physics.trailer_couple`: couples `trailer` to `tow`'s hitch.
pub fn trailer_couple(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let trailer = entity_param(params, "trailer")?;
    let tow = entity_param(params, "tow")?;
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let Some(controller) = world.get::<TrailerController>(trailer) else {
        return Err(not_a_trailer(trailer));
    };
    let distance = match (
        world.get::<regia_physics::vehicle_trailer::TowHitch>(tow),
        world.get::<regia_physics::RigidBodyHandle>(tow),
    ) {
        (Some(hitch), Some(body)) => controller
            .trailer
            .hitch_distance(world.resource::<PhysicsWorld>(), *body, hitch.point)
            .ok(),
        _ => None,
    };
    couple_trailer(world, trailer, tow).map_err(|e| trailer_error(&e))?;
    Ok(json!({
        "trailer": trailer.to_bits(),
        "tow": tow.to_bits(),
        "coupled": true,
        "distance": distance,
    }))
}

/// `physics.trailer_uncouple`: lets the trailer go onto its stand.
pub fn trailer_uncouple(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let trailer = entity_param(params, "trailer")?;
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let Some(controller) = world.get::<TrailerController>(trailer) else {
        return Err(not_a_trailer(trailer));
    };
    let tow = controller.tow;
    uncouple_trailer(world, trailer).map_err(|e| trailer_error(&e))?;
    Ok(json!({
        "trailer": trailer.to_bits(),
        "was_towed_by": tow.map(Entity::to_bits),
        "coupled": false,
    }))
}

/// `physics.trailer_config_check`: parses and validates a `.trailer.ron` text.
pub fn trailer_config_check(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("ron")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`ron` (the text of a .trailer.ron) is required"))?;
    match TrailerAsset::from_ron(text) {
        Ok(asset) => {
            let config = &asset.config;
            let (delay, proportion) = match config.brakes {
                TrailerBrakes::Follow { proportion, delay } => (Some(delay), Some(proportion)),
                _ => (None, None),
            };
            Ok(json!({
                "ok": true,
                "name": asset.name,
                "format_version": asset.format_version,
                "chassis_mass": config.chassis.chassis_mass,
                "wheel_count": config.chassis.wheels.len(),
                "braked_wheels": config.chassis.wheels.iter().filter(|w| w.brake).count(),
                "coupling": coupling_json(
                    &config.coupling.kind,
                    config.coupling.jackknife_warning_angle,
                    config.coupling.capture_radius,
                ),
                "coupling_point": arr3(config.coupling.point),
                "brakes": config.brakes.name(),
                "brake_proportion": proportion,
                "brake_delay": delay,
                "parking_brake": config.parking_brake,
                "has_stand": config.stand.is_some(),
                "rear_hitch": config.rear_hitch.map(arr3),
            }))
        }
        Err(error) => Err(RpcError::invalid_params(format!("[{}] {error}", error.code()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;
    use regia_physics::vehicle_trailer::{TowHitch, TrailerConfig, car_tow_hitch, spawn_trailer};

    fn world_with_physics() -> World {
        let mut world = World::new();
        world.insert_resource(PhysicsWorld::default());
        world
    }

    #[test]
    fn config_check_summarizes_and_names_codes() {
        let mut world = World::new();
        let text = TrailerAsset::new("box", TrailerConfig::car_trailer())
            .to_ron()
            .expect("ron");
        let ok = trailer_config_check(&mut world, &json!({ "ron": text })).expect("check");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["coupling"]["kind"], "tow_ball");
        assert_eq!(ok["brakes"], "follow");
        assert_eq!(ok["wheel_count"], 2);
        let bad = trailer_config_check(
            &mut world,
            &json!({ "ron": "(format_version: 1, name: \"x\", config: (chassis: (wheels: [(drive: true)])))" }),
        )
        .expect_err("driven wheel");
        assert!(
            bad.message.starts_with("[trailer_driven_wheel]"),
            "{}",
            bad.message
        );
        assert!(trailer_config_check(&mut world, &json!({})).is_err());
    }

    #[test]
    fn couple_lists_and_uncouple_report_codes() {
        let mut world = World::new();
        assert!(trailers(&mut world, &json!({})).is_err(), "no physics world");
        let mut world = world_with_physics();
        let config = TrailerConfig::car_trailer();
        let tow = regia_physics::vehicle::spawn_vehicle(
            &mut world,
            regia_physics::vehicle::VehicleConfig::default(),
            Vec3::ZERO,
            Quat::IDENTITY,
        )
        .expect("tow vehicle");
        world.entity_mut(tow).insert(TowHitch {
            point: car_tow_hitch(),
        });
        let near = car_tow_hitch() - config.coupling.point;
        let trailer = spawn_trailer(&mut world, config.clone(), near, Quat::IDENTITY).expect("spawn");
        let far = spawn_trailer(
            &mut world,
            config,
            near + Vec3::new(0.0, 0.0, 3.0),
            Quat::IDENTITY,
        )
        .expect("spawn");

        let list = trailers(&mut world, &json!({})).expect("list");
        assert_eq!(list["count"], 2);
        assert_eq!(list["trailers"][0]["coupled"], false);

        let out_of_reach = trailer_couple(
            &mut world,
            &json!({ "trailer": far.to_bits(), "tow": tow.to_bits() }),
        )
        .expect_err("far");
        assert!(
            out_of_reach.message.starts_with("[trailer_out_of_reach]"),
            "{}",
            out_of_reach.message
        );
        let coupled = trailer_couple(
            &mut world,
            &json!({ "trailer": trailer.to_bits(), "tow": tow.to_bits() }),
        )
        .expect("couple");
        assert_eq!(coupled["coupled"], true);
        let one = trailers(&mut world, &json!({ "entity": trailer.to_bits() })).expect("one");
        assert_eq!(one["trailers"][0]["coupled"], true);
        assert_eq!(one["trailers"][0]["tow"], tow.to_bits());

        let not_trailer =
            trailer_uncouple(&mut world, &json!({ "trailer": tow.to_bits() })).expect_err("tow");
        assert!(not_trailer.message.starts_with("[trailer_not_a_trailer]"));
        let gone = trailer_uncouple(&mut world, &json!({ "trailer": trailer.to_bits() })).expect("uncouple");
        assert_eq!(gone["was_towed_by"], tow.to_bits());
        let again =
            trailer_uncouple(&mut world, &json!({ "trailer": trailer.to_bits() })).expect_err("again");
        assert!(
            again.message.starts_with("[trailer_not_coupled]"),
            "{}",
            again.message
        );
        assert!(trailers(&mut world, &json!({ "entity": tow.to_bits() })).is_err());
    }
}
