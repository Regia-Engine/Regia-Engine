// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for hover craft and `.vehicle.ron` (ADR-2301, specs/23 § 3).
//!
//! - `physics.hovercraft` reports every hover craft entity (`regia_physics::hover`): chassis pose
//!   and speeds, driver input, the last update's error code and each thruster (grounded,
//!   distance, compression, force).
//! - `physics.vehicle_config_check` parses and validates a `.vehicle.ron` text without spawning
//!   anything and answers with a summary, or the stable error code.
//!
//! `physics.vehicle_input` (in `vehicles`) also drives hover craft: they read the same
//! `VehicleInput`. See `book/agents/vehicles.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::PhysicsWorld;
use regia_physics::hover::{HoverController, ThrusterState};
use regia_physics::vehicle::VehicleInput;
use regia_physics::vehicle_asset::VehicleAsset;
use regia_physics::vehicle_drivetrain::Drivetrain;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn arr3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn input_json(input: VehicleInput) -> Value {
    json!({ "throttle": input.throttle, "brake": input.brake, "steer": input.steer })
}

fn thruster_json(t: &ThrusterState) -> Value {
    json!({
        "index": t.index,
        "grounded": t.grounded,
        "distance": t.distance,
        "compression": t.compression,
        "force": t.force,
        "contact_point": t.contact_point.map(arr3),
    })
}

/// `physics.hovercraft`: every hover craft (or the one named by `entity`), in entity order.
pub fn hovercraft(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (a u64)"))?,
        ),
    };
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(RpcError::not_found(
            "no PhysicsWorld in this app",
            "add regia_physics::PhysicsPlugin and regia_physics::vehicle::VehiclePlugin",
        ));
    }
    let mut query = world.query::<(Entity, &HoverController, Option<&VehicleInput>)>();
    let mut rows: Vec<(Entity, &HoverController, Option<&VehicleInput>)> = query.iter(world).collect();
    rows.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _, _)| *e == entity)
    {
        return Err(RpcError::not_found(
            format!("entity {} has no HoverController", entity.to_bits()),
            "physics.hovercraft lists the hover craft entities",
        ));
    }
    let physics = world.resource::<PhysicsWorld>();
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, controller, input)| {
            let mut row = json!({
                "entity": entity.to_bits(),
                "input": input_json(input.copied().unwrap_or_default()),
                "steps": controller.craft.steps(),
                "last_error": controller.last_error.as_ref().map(regia_physics::hover::HoverError::code),
                "thruster_count": controller.craft.config().thrusters.len(),
                "hover_height": controller.craft.config().hover_height,
            });
            match controller.craft.state(physics) {
                Ok(state) => {
                    row["position"] = arr3(state.translation);
                    row["forward"] = arr3(state.forward);
                    row["forward_speed"] = json!(state.forward_speed);
                    row["lateral_speed"] = json!(state.lateral_speed);
                    row["linear_velocity"] = arr3(state.linear_velocity);
                    row["grounded_thrusters"] = json!(state.grounded_thrusters);
                    row["thrusters"] = Value::Array(state.thrusters.iter().map(thruster_json).collect());
                    row["error"] = Value::Null;
                }
                Err(error) => row["error"] = json!(error.code()),
            }
            row
        })
        .collect();
    let count = list.len();
    Ok(json!({ "hovercraft": list, "count": count }))
}

/// `physics.vehicle_config_check`: parses and validates a `.vehicle.ron` text.
pub fn vehicle_config_check(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("ron")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`ron` (the text of a .vehicle.ron) is required"))?;
    match VehicleAsset::from_ron(text) {
        Ok(asset) => {
            let config = &asset.config;
            let (drivetrain, gears, driven_wheels) = match &config.drivetrain {
                Drivetrain::Simple => ("simple", None, config.wheels.iter().filter(|w| w.drive).count()),
                Drivetrain::Geared(geared) => (
                    "geared",
                    Some(geared.gearbox.forward_ratios.len()),
                    geared.driven_wheel_count(),
                ),
            };
            Ok(json!({
                "ok": true,
                "name": asset.name,
                "format_version": asset.format_version,
                "wheel_count": config.wheels.len(),
                "chassis_mass": config.chassis_mass,
                "steered_wheels": config.wheels.iter().filter(|w| w.steer).count(),
                "driven_wheels": driven_wheels,
                "drivetrain": drivetrain,
                "gears": gears,
                "anti_roll_bars": config.anti_roll.len(),
                "aero": config.aero.is_some(),
                "abs": config.aids.abs.is_some(),
                "traction_control": config.aids.traction_control.is_some(),
                "braked_wheels": config.wheels.iter().filter(|w| w.brake).count(),
                "ground_layers": config.ground_layers,
                "query_layer": config.query_layer,
                "slip_tires": config.wheels.iter().filter(|w| w.tire_model.slip().is_some()).count(),
                "warnings": crate::vehicles::config_warnings(config),
            }))
        }
        Err(error) => Err(RpcError::invalid_params(format!("[{}] {error}", error.code()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;
    use regia_physics::hover::{HoverConfig, spawn_hovercraft};
    use regia_physics::vehicle::VehiclePlugin;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, VehiclePlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(1.0 / 60.0),
            ));
        {
            use regia_physics::{Collider, ColliderShape, RigidBody};
            use regia_scene::Transform;
            app.world_mut().spawn((
                Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(100.0, 0.5, 100.0),
                    },
                    ..Collider::default()
                },
            ));
        }
        for _ in 0..3 {
            app.update();
        }
        app
    }

    #[test]
    fn hovercraft_reports_a_settled_craft_and_vehicle_input_drives_it() {
        let mut app = app();
        let craft = spawn_hovercraft(
            app.world_mut(),
            HoverConfig::default(),
            Vec3::new(0.0, 1.2, 0.0),
            Quat::IDENTITY,
        )
        .expect("spawn");
        for _ in 0..300 {
            app.update();
        }
        let report = hovercraft(app.world_mut(), &json!({})).expect("physics.hovercraft");
        assert_eq!(report["count"], 1);
        let row = &report["hovercraft"][0];
        assert_eq!(row["entity"], json!(craft.to_bits()));
        assert_eq!(row["grounded_thrusters"], 4);
        assert_eq!(row["thruster_count"], 4);
        assert!(row["last_error"].is_null() && row["error"].is_null());
        let set = crate::vehicles::vehicle_input(
            app.world_mut(),
            &json!({ "entity": craft.to_bits(), "throttle": 1.0 }),
        )
        .expect("input on a hover craft");
        assert_eq!(set["after"]["throttle"], 1.0);
        for _ in 0..90 {
            app.update();
        }
        let row = hovercraft(app.world_mut(), &json!({ "entity": craft.to_bits() })).expect("one");
        assert!(row["hovercraft"][0]["forward_speed"].as_f64().expect("speed") > 2.0);
    }

    #[test]
    fn hovercraft_refuses_an_unknown_entity_and_a_missing_world() {
        let mut app = app();
        let nope = app.world_mut().spawn_empty().id();
        let err = hovercraft(app.world_mut(), &json!({ "entity": nope.to_bits() })).expect_err("not a craft");
        assert_eq!(err.data.expect("data")["code"], json!("F0003"));
        let mut bare = World::new();
        assert_eq!(
            hovercraft(&mut bare, &json!({}))
                .expect_err("no world")
                .data
                .expect("data")["code"],
            json!("F0003")
        );
        assert_eq!(
            hovercraft(&mut bare, &json!({ "entity": "x" }))
                .expect_err("bad id")
                .data
                .expect("data")["code"],
            json!("F0002")
        );
    }

    #[test]
    fn config_check_summarizes_a_good_file_and_names_the_code_of_a_bad_one() {
        let mut world = World::new();
        let good = VehicleAsset::new("car", regia_physics::vehicle::VehicleConfig::default())
            .to_ron()
            .expect("ron");
        let ok = vehicle_config_check(&mut world, &json!({ "ron": good })).expect("check");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["wheel_count"], 4);
        assert_eq!(ok["steered_wheels"], 2);
        assert_eq!(ok["drivetrain"], "simple");
        assert!(ok["gears"].is_null());
        let v1 = vehicle_config_check(
            &mut world,
            &json!({ "ron": r#"(format_version: 1, name: "old", config: ())"# }),
        )
        .expect("a v1 file migrates");
        assert_eq!(
            v1["format_version"],
            regia_physics::vehicle_asset::VEHICLE_FORMAT_VERSION
        );
        assert_eq!(v1["drivetrain"], "simple");
        let v2 = vehicle_config_check(
            &mut world,
            &json!({ "ron": r#"(format_version: 2, name: "new", config: (
                drivetrain: Geared((layout: Awd)),
                anti_roll: [(left: 0, right: 1, stiffness: 1000.0)],
                aero: Some(()),
                aids: (abs: Some(())),
            ))"# }),
        )
        .expect("v2");
        assert_eq!(v2["drivetrain"], "geared");
        assert_eq!(v2["gears"], 5);
        assert_eq!(v2["driven_wheels"], 4);
        assert_eq!(v2["anti_roll_bars"], 1);
        assert_eq!(v2["aero"], true);
        assert_eq!(v2["abs"], true);
        assert_eq!(v2["traction_control"], false);
        let bad = vehicle_config_check(
            &mut world,
            &json!({ "ron": r#"(format_version: 1, name: "x", config: (wheels: []))"# }),
        )
        .expect_err("no wheels");
        assert_eq!(bad.data.clone().expect("data")["code"], json!("F0002"));
        assert!(bad.message.contains("[vehicle_no_wheels]"), "{}", bad.message);
        let missing = vehicle_config_check(&mut world, &json!({})).expect_err("missing");
        assert_eq!(missing.data.expect("data")["code"], json!("F0002"));
    }
}
