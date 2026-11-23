// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for fixed-wing aircraft and `.aircraft.ron` (ADR-3374, specs/23 § 3).
//!
//! - `physics.aircraft` reports every aircraft entity (`regia_physics::aero`): pose, attitude, body
//!   rates, air data (airspeed, angle of attack, sideslip, altitude, density), the aircraft's lift
//!   and drag coefficients, the stall flag, thrust, input, each surface and each gear leg.
//! - `physics.aircraft_input` sets some or all of an aircraft's controls (and, optionally, the
//!   world's wind).
//! - `physics.aircraft_trim` solves steady level flight (or a glide at a fixed throttle) offline for
//!   an aircraft entity or a `.aircraft.ron` text.
//! - `physics.aircraft_config_check` parses and validates a `.aircraft.ron` text without spawning
//!   anything and answers with a summary (best glide ratio, stall speed) or the stable error code.
//!
//! Errors carry the `aircraft_*` code in `data.code`; see `book/agents/aircraft.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::aero::asset::AircraftAsset;
use regia_physics::aero::trim::{TrimTarget, max_lift_to_drag, max_trimmed_lift_to_drag, polar, solve};
use regia_physics::aero::{
    AeroEnvironment, AeroError, AircraftConfig, AircraftController, AircraftInput, ISA_SEA_LEVEL_DENSITY,
};
use regia_physics::{Gravity, PhysicsWorld};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn arr3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn input_json(input: &AircraftInput) -> Value {
    json!({
        "pitch": input.pitch,
        "roll": input.roll,
        "yaw": input.yaw,
        "throttle": input.throttle,
        "flaps": input.flaps,
        "brakes": input.brakes,
    })
}

/// An aero error as an agent error: `data.code` is the stable `aircraft_*` code.
fn aero_error(error: &AeroError) -> RpcError {
    let hint = match error {
        AeroError::TrimFailed { .. } | AeroError::TrimUnreachable { .. } => {
            "try another speed or altitude; physics.aircraft_config_check reports the stall speed"
        }
        AeroError::TrimInvalidTarget { .. } => {
            "speed 1-1000 m/s, altitude -2000-20000 m, throttle and flaps 0-1"
        }
        _ => "see book/agents/aircraft.md and docs/formats/aircraft-ron.md",
    };
    RpcError::new(INVALID_PARAMS, error.code(), error.to_string(), hint)
}

fn no_physics() -> RpcError {
    RpcError::not_found(
        "no PhysicsWorld in this app",
        "add regia_physics::PhysicsPlugin and regia_framework::aircraft::AircraftPlugin",
    )
}

fn not_an_aircraft(entity: Entity) -> RpcError {
    RpcError::not_found(
        format!("entity {} has no AircraftController", entity.to_bits()),
        "physics.aircraft lists the aircraft entities",
    )
}

fn entity_opt(params: &Value) -> Result<Option<Entity>, RpcError> {
    match params.get("entity").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params("`entity` must be an entity id (a u64 from physics.aircraft)")
            }),
    }
}

/// An optional finite number parameter.
fn number_param(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .map(|n| n as f32)
                .filter(|n| n.is_finite())
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a finite number")))?;
            Ok(Some(n))
        }
    }
}

fn vec3_param(params: &Value, field: &str) -> Result<Option<Vec3>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => {
            let items = value
                .as_array()
                .filter(|a| a.len() == 3)
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be [x, y, z]")))?;
            let mut out = [0.0_f32; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                *slot = item
                    .as_f64()
                    .map(|n| n as f32)
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must hold finite numbers")))?;
            }
            Ok(Some(Vec3::from_array(out)))
        }
    }
}

/// `physics.aircraft`: every aircraft (or the one named by `entity`), in entity order.
pub fn aircraft(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = entity_opt(params)?;
    if !world.contains_resource::<PhysicsWorld>() {
        return Err(no_physics());
    }
    let env = world
        .get_resource::<AeroEnvironment>()
        .copied()
        .unwrap_or_default();
    let mut query = world.query::<(Entity, &AircraftController, Option<&AircraftInput>)>();
    let mut rows: Vec<(Entity, &AircraftController, Option<&AircraftInput>)> = query.iter(world).collect();
    rows.sort_by_key(|(entity, _, _)| (entity.index().index(), entity.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _, _)| *e == entity)
    {
        return Err(not_an_aircraft(entity));
    }
    let physics = world.resource::<PhysicsWorld>();
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, controller, input)| {
            let plane = &controller.aircraft;
            let config = plane.config();
            let surfaces: Vec<Value> = config
                .surfaces
                .iter()
                .zip(plane.surfaces())
                .map(|(surface, state)| {
                    json!({
                        "name": surface.name,
                        "kind": surface.kind.name(),
                        "alpha_deg": state.alpha.to_degrees(),
                        "deflection_deg": state.deflection.to_degrees(),
                        "cl": state.coefficients.cl,
                        "cd": state.coefficients.cd,
                        "cm": state.coefficients.cm,
                        "lift": state.lift,
                        "drag": state.drag,
                        "stalled": state.coefficients.stalled,
                    })
                })
                .collect();
            let gear: Vec<Value> = config
                .gear
                .iter()
                .zip(plane.gear())
                .map(|(leg, state)| {
                    json!({
                        "name": leg.name,
                        "grounded": state.grounded,
                        "compression": state.compression,
                        "normal_force": state.normal_force,
                        "friction_force": state.friction_force,
                        "steer_deg": state.steer_angle.to_degrees(),
                        "rolling_speed": state.rolling_speed,
                    })
                })
                .collect();
            let mut row = json!({
                "entity": entity.to_bits(),
                "input": input_json(&input.copied().unwrap_or_default()),
                "applied_input": input_json(&plane.last_input()),
                "steps": plane.steps(),
                "last_error": controller.last_error.as_ref().map(AeroError::code),
                "surfaces": surfaces,
                "gear": gear,
                "engines": plane.thrusts(),
            });
            match plane.state(physics) {
                Ok(state) => {
                    row["position"] = arr3(state.translation);
                    row["linear_velocity"] = arr3(state.linear_velocity);
                    row["body_rates"] = arr3(state.body_rates);
                    row["heading_deg"] = json!(state.heading.to_degrees());
                    row["pitch_deg"] = json!(state.pitch.to_degrees());
                    row["bank_deg"] = json!(state.bank.to_degrees());
                    row["airspeed"] = json!(state.air.airspeed);
                    row["alpha_deg"] = json!(state.air.alpha.to_degrees());
                    row["sideslip_deg"] = json!(state.air.beta.to_degrees());
                    row["altitude"] = json!(state.translation.y - env.sea_level_y);
                    row["density"] = json!(state.air.density);
                    row["dynamic_pressure"] = json!(state.air.dynamic_pressure);
                    row["cl"] = json!(state.cl);
                    row["cd"] = json!(state.cd);
                    row["stalled"] = json!(state.stalled);
                    row["thrust"] = json!(state.thrust);
                    row["grounded_gear"] = json!(state.grounded_gear);
                    row["on_ground"] = json!(state.grounded_gear > 0);
                    row["error"] = Value::Null;
                }
                Err(error) => row["error"] = json!(error.code()),
            }
            row
        })
        .collect();
    let count = list.len();
    Ok(json!({
        "aircraft": list,
        "count": count,
        "wind": arr3(env.wind),
        "sea_level_y": env.sea_level_y,
    }))
}

/// `physics.aircraft_input`: changes the named controls of one aircraft (the rest keep their value)
/// and, with `wind`, the world's wind.
pub fn aircraft_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_opt(params)?
        .ok_or_else(|| RpcError::invalid_params("`entity` (an id from physics.aircraft) is required"))?;
    let fields = ["pitch", "roll", "yaw", "throttle", "flaps", "brakes"];
    let mut values = [None; 6];
    for (slot, field) in values.iter_mut().zip(fields) {
        *slot = number_param(params, field)?;
    }
    let wind = vec3_param(params, "wind")?;
    if world.get::<AircraftController>(entity).is_none() {
        return Err(not_an_aircraft(entity));
    }
    let before = world.get::<AircraftInput>(entity).copied().unwrap_or_default();
    let [pitch, roll, yaw, throttle, flaps, brakes] = values;
    let after = AircraftInput {
        pitch: pitch.unwrap_or(before.pitch),
        roll: roll.unwrap_or(before.roll),
        yaw: yaw.unwrap_or(before.yaw),
        throttle: throttle.unwrap_or(before.throttle),
        flaps: flaps.unwrap_or(before.flaps),
        brakes: brakes.unwrap_or(before.brakes),
    }
    .sanitized();
    world.entity_mut(entity).insert(after);
    if let Some(wind) = wind {
        let mut env = world
            .get_resource::<AeroEnvironment>()
            .copied()
            .unwrap_or_default();
        env.wind = wind;
        world.insert_resource(env);
    }
    let wind = world
        .get_resource::<AeroEnvironment>()
        .copied()
        .unwrap_or_default()
        .wind;
    Ok(json!({
        "entity": entity.to_bits(),
        "before": input_json(&before),
        "after": input_json(&after),
        "wind": arr3(wind),
    }))
}

/// The configuration an agent call names: an aircraft entity's, or a `.aircraft.ron` text's.
fn config_from(world: &mut World, params: &Value) -> Result<(AircraftConfig, Option<String>), RpcError> {
    if let Some(entity) = entity_opt(params)? {
        let controller = world
            .get::<AircraftController>(entity)
            .ok_or_else(|| not_an_aircraft(entity))?;
        return Ok((controller.aircraft.config().clone(), None));
    }
    let text = params.get("ron").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("give `entity` (from physics.aircraft) or `ron` (a .aircraft.ron text)")
    })?;
    let asset = AircraftAsset::from_ron(text).map_err(|e| aero_error(&e))?;
    Ok((asset.config, Some(asset.name)))
}

/// `physics.aircraft_trim`: level flight (or a fixed-throttle glide) at a speed and altitude.
pub fn aircraft_trim(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let speed = number_param(params, "speed")?
        .ok_or_else(|| RpcError::invalid_params("`speed` (m/s) is required"))?;
    let altitude = number_param(params, "altitude")?.unwrap_or(0.0);
    let throttle = number_param(params, "throttle")?;
    let flaps = number_param(params, "flaps")?.unwrap_or(0.0);
    let (config, name) = config_from(world, params)?;
    let gravity = world
        .get_resource::<Gravity>()
        .map_or(9.81, |g| (-g.0.y).max(0.01));
    let target = TrimTarget {
        speed,
        altitude,
        throttle,
        flaps,
        gravity,
    };
    let trim = solve(&config, &target).map_err(|e| aero_error(&e))?;
    Ok(json!({
        "name": name,
        "mode": if throttle.is_some() { "fixed_throttle" } else { "level" },
        "speed": trim.speed,
        "altitude": altitude,
        "pitch_deg": trim.pitch_attitude.to_degrees(),
        "alpha_deg": trim.alpha.to_degrees(),
        "flight_path_deg": trim.flight_path.to_degrees(),
        "pitch_input": trim.pitch_input,
        "throttle": trim.throttle,
        "flaps": trim.flaps,
        "cl": trim.cl,
        "cd": trim.cd,
        "lift_to_drag": if trim.cd > 0.0 { trim.cl / trim.cd } else { 0.0 },
        "stalled": trim.stalled,
        "iterations": trim.iterations,
        "residual": trim.residual,
        "gravity": gravity,
    }))
}

/// `physics.aircraft_config_check`: parses and validates a `.aircraft.ron` text.
pub fn aircraft_config_check(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("ron")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`ron` (the text of a .aircraft.ron) is required"))?;
    let asset = AircraftAsset::from_ron(text).map_err(|e| aero_error(&e))?;
    let config = &asset.config;
    let (best_ratio, best_alpha) = max_lift_to_drag(config);
    let (trimmed_ratio, trimmed_alpha) = max_trimmed_lift_to_drag(config);
    let cl_max = (0..=300)
        .map(|i| polar(config, (i as f32 * 0.1).to_radians()).0)
        .fold(0.0_f32, f32::max);
    let area = config.reference_area();
    let stall_speed = if cl_max > 0.0 {
        Some((2.0 * config.mass * 9.81 / (ISA_SEA_LEVEL_DENSITY * area * cl_max)).sqrt())
    } else {
        None
    };
    let mut warnings = Vec::new();
    if config.gear.is_empty() {
        warnings.push(
            json!({ "code": "no_gear", "message": "no landing gear: the plane cannot stand on the ground" }),
        );
    }
    if config.engines.is_empty() {
        warnings.push(json!({ "code": "no_engines", "message": "no engines: a glider" }));
    }
    if !config
        .surfaces
        .iter()
        .any(|s| s.kind == regia_physics::aero::SurfaceKind::Wing)
    {
        warnings.push(json!({ "code": "no_wing", "message": "no wing surface: the stall flag never sets" }));
    }
    Ok(json!({
        "ok": true,
        "name": asset.name,
        "format_version": asset.format_version,
        "mass": config.mass,
        "surfaces": config.surfaces.len(),
        "engines": config.engines.len(),
        "gear": config.gear.len(),
        "reference_area": area,
        "static_thrust": config.engines.iter().map(|e| e.static_thrust).sum::<f32>(),
        "max_lift_to_drag": best_ratio,
        "best_glide_alpha_deg": best_alpha.to_degrees(),
        "max_trimmed_lift_to_drag": trimmed_ratio,
        "trimmed_glide_alpha_deg": trimmed_alpha.to_degrees(),
        "cl_max": cl_max,
        "stall_speed": stall_speed,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;
    use regia_physics::aero::{spawn_aircraft, trainer};

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_physics::PhysicsPlugin,
            regia_framework::aircraft::AircraftPlugin,
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.update();
        app
    }

    const RON: &str = include_str!("fuzz_targets/fixtures/aircraft.ron");

    #[test]
    fn aircraft_reports_a_flying_plane_and_input_moves_its_controls() {
        let mut app = app();
        let plane = spawn_aircraft(
            app.world_mut(),
            trainer(),
            Vec3::new(0.0, 800.0, 0.0),
            Quat::IDENTITY,
            Vec3::new(0.0, 0.0, -50.0),
        )
        .expect("spawn");
        for _ in 0..5 {
            app.update();
        }
        let report = aircraft(app.world_mut(), &json!({})).expect("physics.aircraft");
        assert_eq!(report["count"], 1);
        let row = &report["aircraft"][0];
        assert!(row["airspeed"].as_f64().expect("airspeed") > 40.0, "{row}");
        assert_eq!(row["stalled"], false);
        assert_eq!(row["surfaces"].as_array().map(Vec::len), Some(4));
        let changed = aircraft_input(
            app.world_mut(),
            &json!({ "entity": plane.to_bits(), "throttle": 0.7, "roll": 3.0, "wind": [1.0, 0.0, 0.0] }),
        )
        .expect("input");
        assert_eq!(changed["after"]["roll"], 1.0);
        assert!((changed["after"]["throttle"].as_f64().expect("throttle") - 0.7).abs() < 1.0e-6);
        assert_eq!(changed["wind"][0], 1.0);
        let missing = aircraft(app.world_mut(), &json!({ "entity": 12345u64 })).expect_err("not an aircraft");
        assert_eq!(missing.data.expect("data")["code"], "F0003");
    }

    #[test]
    fn trim_and_config_check_answer_with_codes() {
        let mut app = app();
        let trim = aircraft_trim(
            app.world_mut(),
            &json!({ "ron": RON, "speed": 50.0, "altitude": 1000.0 }),
        )
        .expect("trim");
        assert_eq!(trim["mode"], "level");
        assert!(trim["throttle"].as_f64().expect("throttle") > 0.0);
        let bad =
            aircraft_trim(app.world_mut(), &json!({ "ron": RON, "speed": -5.0 })).expect_err("bad speed");
        assert_eq!(bad.data.expect("data")["code"], "aircraft_trim_invalid_target");
        let check = aircraft_config_check(app.world_mut(), &json!({ "ron": RON })).expect("check");
        assert_eq!(check["ok"], true);
        assert!(check["stall_speed"].as_f64().expect("stall") > 15.0);
        let broken = aircraft_config_check(app.world_mut(), &json!({ "ron": "((" })).expect_err("parse");
        assert_eq!(broken.data.expect("data")["code"], "aircraft_parse");
        let old = aircraft_config_check(
            app.world_mut(),
            &json!({ "ron": "(format_version: 9, name: \"x\")" }),
        )
        .expect_err("version");
        assert_eq!(old.data.expect("data")["code"], "aircraft_version");
    }
}
