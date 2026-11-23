// SPDX-License-Identifier: MPL-2.0
//! Vehicle audio on the agent protocol (`book/agents/vehicle-audio.md`, ADR-3271):
//! `audio.vehicle_audio` (state per vehicle: layers, weights, pitches, voices, the budget),
//! `audio.vehicle_audio_set` (attach, replace or remove a vehicle's sound; set the budget) and
//! `audio.vehicle_audio_probe` (offline: rpm, load and slip to layer weights and pitches).

use regia_app::prelude::*;
use regia_audio::{
    VehicleAudioDef, VehicleAudioError, VehicleAudioInputs, VehicleAudioModel, VehicleVoiceOut, gain_to_db,
    squeal_amount,
};
use regia_framework::vehicle_audio::{VehicleAudio, VehicleAudioBudget, VehicleAudioRuntime};
use regia_physics::vehicle::VehicleController;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn obj(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    match params {
        Value::Null => Ok(EMPTY.get_or_init(Map::new)),
        Value::Object(map) => Ok(map),
        _ => Err(RpcError::invalid_params("params must be an object")),
    }
}

fn num(p: &Map<String, Value>, field: &str, (lo, hi): (f64, f64)) -> Result<Option<f64>, RpcError> {
    let Some(value) = p.get(field) else {
        return Ok(None);
    };
    let Some(v) = value.as_f64() else {
        return Err(RpcError::invalid_params(format!("`{field}` must be a number")));
    };
    if !v.is_finite() || v < lo || v > hi {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be within {lo}..={hi}"
        )));
    }
    Ok(Some(v))
}

/// A vehicle count: the same range as [`num`], and a whole number (a fraction would truncate).
fn whole_count(p: &Map<String, Value>, field: &str, (lo, hi): (f64, f64)) -> Result<Option<usize>, RpcError> {
    let Some(v) = num(p, field, (lo, hi))? else {
        return Ok(None);
    };
    if v.fract() != 0.0 {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be a whole number within {lo}..={hi}"
        )));
    }
    Ok(Some(v as usize))
}

fn def_error(error: &VehicleAudioError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "see docs/formats/vehicle-audio.md; preset: placeholder is a valid start",
    )
}

/// The definition a call names: `ron` text, or `preset` (`placeholder`, the default).
fn definition(p: &Map<String, Value>) -> Result<VehicleAudioDef, RpcError> {
    if let Some(text) = p.get("ron") {
        let Some(text) = text.as_str() else {
            return Err(RpcError::invalid_params("`ron` must be a string"));
        };
        return VehicleAudioDef::from_ron(text).map_err(|e| def_error(&e));
    }
    match p.get("preset").map(Value::as_str) {
        None | Some(Some("placeholder")) => Ok(VehicleAudioDef::placeholder()),
        Some(Some(other)) => Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "vehicle_audio_unknown_preset",
            format!("unknown vehicle audio preset `{other}`"),
            "use placeholder, or pass the definition as ron",
        )),
        Some(None) => Err(RpcError::invalid_params("`preset` must be a string")),
    }
}

fn entity_of(world: &World, value: &Value) -> Result<Entity, RpcError> {
    let bits = value
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (integer)"))?;
    let entity = Entity::try_from_bits(bits).ok_or_else(|| {
        RpcError::not_found(format!("no entity {bits}"), "use scene.list or physics.vehicles")
    })?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("no entity {bits}"),
            "use scene.list or physics.vehicles",
        ));
    }
    Ok(entity)
}

fn voice_json(model: &VehicleAudioModel, index: usize, voice: &VehicleVoiceOut, live: Option<bool>) -> Value {
    let mut out = json!({
        "index": index,
        "role": voice.role.label(),
        "slot": match voice.slot {
            regia_audio::VehicleEmitterSlot::Engine => "engine".to_string(),
            regia_audio::VehicleEmitterSlot::Exhaust => "exhaust".to_string(),
            regia_audio::VehicleEmitterSlot::Wheel(i) => format!("wheel_{i}"),
        },
        "sound": model.sounds().get(usize::from(voice.sound)).cloned().unwrap_or_default(),
        "weight": voice.weight,
        "amplitude": voice.amplitude,
        "level_db": gain_to_db(voice.amplitude),
        "pitch": voice.pitch,
    });
    if let Some(live) = live {
        out["voice"] = json!(live);
    }
    out
}

fn params_json(model: &VehicleAudioModel) -> Value {
    let p = model.params();
    json!({
        "rpm": p.rpm,
        "load": p.load,
        "limiter": p.limiter,
        "limiter_cutting": p.limiter_cutting,
        "dip": p.dip,
    })
}

fn budget_json(budget: &VehicleAudioBudget) -> Value {
    json!({
        "full_vehicles": budget.full_vehicles,
        "reduced_vehicles": budget.reduced_vehicles,
        "max_distance": budget.max_distance,
    })
}

/// `audio.vehicle_audio`: every vehicle's sound state, the budget and its verdicts.
pub(crate) fn state(world: &mut World) -> Result<Value, RpcError> {
    let budget = world.get_resource::<VehicleAudioBudget>().copied();
    let Some(runtime) = world.get_resource::<VehicleAudioRuntime>() else {
        return Ok(json!({
            "plugin": false,
            "vehicles": [],
            "rejected": [],
            "budget": budget.map(|b| budget_json(&b)),
        }));
    };
    let vehicles: Vec<Value> = runtime
        .vehicles
        .iter()
        .map(|(entity, state)| {
            let model = &state.model;
            let layers: Vec<Value> = model
                .voices()
                .iter()
                .enumerate()
                .map(|(i, v)| voice_json(model, i, v, Some(state.voices.voice_id(i).is_some())))
                .collect();
            let (started, stopped) = state.voices.counters();
            json!({
                "entity": entity.to_bits(),
                "name": model.def().name,
                "mode": state.mode.label(),
                "distance": if state.distance.is_finite() { json!(state.distance) } else { Value::Null },
                "params": params_json(model),
                "inputs": {
                    "rpm": state.inputs.rpm,
                    "throttle": state.inputs.throttle,
                    "engine_torque": state.inputs.engine_torque,
                    "upshifts": state.inputs.upshifts,
                    "downshifts": state.inputs.downshifts,
                    "shifting": state.inputs.shifting,
                    "speed": state.inputs.speed,
                    "wheels": state.inputs.wheel_count,
                },
                "steps": model.steps(),
                "shifts_fired": model.shifts_fired(),
                "bumps_fired": model.bumps_fired(),
                "one_shots_played": state.shots_played,
                "one_shots_dropped": state.shots_dropped + model.dropped_one_shots(),
                "live_voices": state.voices.live_count(),
                "voices_started": started,
                "voices_stopped": stopped,
                "missing_sounds": state.bank.missing,
                "problem": state.problem,
                "digest": format!("{:016x}", model.parameter_digest()),
                "layers": layers,
            })
        })
        .collect();
    let rejected: Vec<Value> = runtime
        .rejected
        .iter()
        .map(|(entity, error)| json!({ "entity": entity.to_bits(), "error": error }))
        .collect();
    let (full, reduced, silent) = runtime.mode_counts();
    Ok(json!({
        "plugin": true,
        "budget": budget.map(|b| budget_json(&b)),
        "counts": { "full": full, "reduced": reduced, "silent": silent },
        "digest": format!("{:016x}", runtime.digest()),
        "vehicles": vehicles,
        "rejected": rejected,
    }))
}

/// `audio.vehicle_audio_set`: attach, replace or remove a vehicle's sound; set the budget.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let p = obj(params)?;
    if world.get_resource::<VehicleAudioRuntime>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "vehicle_audio_no_plugin",
            "vehicle audio is not running in this app",
            "add regia_framework::vehicle_audio::VehicleAudioPlugin",
        ));
    }
    let mut next_budget = None;
    if let Some(budget) = p.get("budget") {
        let Some(b) = budget.as_object() else {
            return Err(RpcError::invalid_params("`budget` must be an object"));
        };
        let mut next = world
            .get_resource::<VehicleAudioBudget>()
            .copied()
            .unwrap_or_default();
        if let Some(v) = whole_count(b, "full_vehicles", (0.0, 64.0))? {
            next.full_vehicles = v;
        }
        if let Some(v) = whole_count(b, "reduced_vehicles", (0.0, 256.0))? {
            next.reduced_vehicles = v;
        }
        if let Some(v) = num(b, "max_distance", (0.0, 100_000.0))? {
            next.max_distance = v as f32;
        }
        next_budget = Some(next);
    }
    // Validate the entity (and its definition) before any write, so a refused call leaves the
    // budget where it was.
    enum Pending {
        Remove,
        Definition(Box<VehicleAudioDef>),
    }
    let mut pending: Option<(Entity, Pending)> = None;
    if let Some(value) = p.get("entity") {
        let remove = match p.get("remove") {
            None => None,
            Some(value) => Some(
                value
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params("`remove` must be a boolean"))?,
            ),
        };
        let entity = entity_of(world, value)?;
        if world.get::<VehicleController>(entity).is_none() {
            return Err(RpcError::new(
                protocol::INVALID_PARAMS,
                "vehicle_audio_not_vehicle",
                format!("entity {} has no VehicleController", entity.to_bits()),
                "use physics.vehicles for vehicle entities",
            ));
        }
        if remove.unwrap_or(false) {
            pending = Some((entity, Pending::Remove));
        } else {
            let def = definition(p)?;
            pending = Some((entity, Pending::Definition(Box::new(def))));
        }
    } else if p.contains_key("ron") || p.contains_key("preset") || p.contains_key("remove") {
        return Err(RpcError::invalid_params(
            "pass `entity` with `ron`, `preset` or `remove`",
        ));
    }
    let mut changed = Vec::new();
    if next_budget.is_some() {
        changed.push("budget");
    }
    let mut entity_bits = None;
    if let Some((entity, change)) = &pending {
        entity_bits = Some(entity.to_bits());
        changed.push(match change {
            Pending::Remove => "removed",
            Pending::Definition(_) => "definition",
        });
    }
    if changed.is_empty() {
        return Err(RpcError::invalid_params(
            "pass `entity` (with `preset`, `ron` or `remove`) or `budget`",
        ));
    }
    if let Some(next) = next_budget {
        world.insert_resource(next);
    }
    if let Some((entity, change)) = pending {
        match change {
            Pending::Remove => {
                world.entity_mut(entity).remove::<VehicleAudio>();
            }
            Pending::Definition(def) => {
                world.entity_mut(entity).insert(VehicleAudio { def: *def });
            }
        }
    }
    let budget = world
        .get_resource::<VehicleAudioBudget>()
        .copied()
        .unwrap_or_default();
    Ok(json!({ "entity": entity_bits, "changed": changed, "budget": budget_json(&budget) }))
}

/// `audio.vehicle_audio_probe`: what a definition plays at a steady rpm, load and slip (offline:
/// no world, no voices).
pub(crate) fn probe(params: &Value) -> Result<Value, RpcError> {
    let p = obj(params)?;
    let def = definition(p)?;
    let rpm = num(p, "rpm", (0.0, 30_000.0))?.unwrap_or(3000.0) as f32;
    let load = num(p, "load", (0.0, 1.0))?;
    let throttle = num(p, "throttle", (0.0, 1.0))?;
    let torque = num(p, "engine_torque", (-100_000.0, 100_000.0))?;
    let slip_angle_deg = num(p, "slip_angle_deg", (-90.0, 90.0))?.unwrap_or(0.0) as f32;
    let slip_ratio = num(p, "slip_ratio", (-10.0, 10.0))?.unwrap_or(0.0) as f32;
    let speed = num(p, "speed", (0.0, 1_000.0))?.unwrap_or(0.0) as f32;
    let surface = match p.get("surface") {
        None => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(RpcError::invalid_params("`surface` must be a string")),
    };
    let mut model = VehicleAudioModel::new(def, 1).map_err(|e| def_error(&e))?;
    // `load` sets throttle and torque together (exactly that load); otherwise each is taken as given.
    let peak = model.def().engine.peak_torque_nm;
    let (throttle, torque) = match load {
        Some(load) => (load as f32, load as f32 * peak),
        None => (throttle.unwrap_or(1.0) as f32, torque.map_or(peak, |t| t as f32)),
    };
    let mut inputs = VehicleAudioInputs {
        rpm,
        throttle,
        engine_torque: torque,
        speed,
        wheel_count: 1,
        ..VehicleAudioInputs::default()
    };
    inputs.wheels[0].grounded = true;
    inputs.wheels[0].slip_angle = slip_angle_deg.to_radians();
    inputs.wheels[0].slip_ratio = slip_ratio;
    inputs.wheels[0].contact_speed = speed;
    inputs.wheels[0].surface = model.surface_index(surface);
    model.settle(&inputs);
    let squeal = model
        .def()
        .tires
        .squeal
        .as_ref()
        .map(|s| squeal_amount(s, inputs.wheels[0].slip_angle, slip_ratio));
    let layers: Vec<Value> = model
        .voices()
        .iter()
        .enumerate()
        .map(|(i, v)| voice_json(&model, i, v, None))
        .collect();
    let active_engine: Vec<Value> = model
        .voices()
        .iter()
        .filter(|v| v.role.is_engine() && v.weight > 0.0)
        .map(|v| json!({ "role": v.role.label(), "weight": v.weight, "pitch": v.pitch }))
        .collect();
    Ok(json!({
        "name": model.def().name,
        "params": params_json(&model),
        "squeal": squeal,
        "active_engine_layers": active_engine,
        "layers": layers,
        "digest": format!("{:016x}", model.parameter_digest()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn the_probe_reports_equal_power_layers_and_squeal() {
        let at_key = probe(&json!({ "rpm": 2500.0, "load": 1.0 })).expect("probe");
        let active = at_key["active_engine_layers"].as_array().expect("array");
        let on: Vec<&Value> = active
            .iter()
            .filter(|l| l["role"].as_str().is_some_and(|r| r.starts_with("engine_on")))
            .collect();
        assert_eq!(
            on.len(),
            1,
            "exactly one on-load layer at a recorded rpm: {active:?}"
        );
        assert_eq!(on[0]["weight"].as_f64(), Some(1.0));
        assert!((on[0]["pitch"].as_f64().expect("pitch") - 1.0).abs() < 1e-6);
        let between = probe(&json!({ "rpm": 3500.0, "load": 1.0 })).expect("probe");
        let power: f64 = between["active_engine_layers"]
            .as_array()
            .expect("array")
            .iter()
            .filter(|l| l["role"].as_str().is_some_and(|r| r.starts_with("engine_on")))
            .map(|l| l["weight"].as_f64().expect("w").powi(2))
            .sum();
        assert!((power - 1.0).abs() < 1e-5);
        let quiet = probe(&json!({ "slip_angle_deg": 2.0 })).expect("probe");
        assert_eq!(quiet["squeal"].as_f64(), Some(0.0));
        let loud = probe(&json!({ "slip_angle_deg": 12.0 })).expect("probe");
        assert!(loud["squeal"].as_f64().expect("squeal") > 0.0);
    }

    #[test]
    fn the_probe_rejects_bad_definitions_with_their_codes() {
        let err = probe(&json!({ "preset": "truck" })).expect_err("unknown preset");
        assert_eq!(code(&err), "vehicle_audio_unknown_preset");
        let err = probe(&json!({ "ron": "(format_version: 9, name: \"x\")" })).expect_err("version");
        assert_eq!(code(&err), "vehicle_audio_version");
        let err = probe(&json!({ "ron": "(nonsense" })).expect_err("parse");
        assert_eq!(code(&err), "vehicle_audio_parse");
        let bad = "(format_version: 1, name: \"x\", engine: (on_load: []))";
        let err = probe(&json!({ "ron": bad })).expect_err("value");
        assert_eq!(code(&err), "vehicle_audio_value");
    }

    #[test]
    fn state_and_set_without_the_plugin() {
        let mut world = World::new();
        let out = state(&mut world).expect("state");
        assert_eq!(out["plugin"], json!(false));
        let err = set(&mut world, &json!({ "budget": { "full_vehicles": 2 } })).expect_err("no plugin");
        assert_eq!(code(&err), "vehicle_audio_no_plugin");
    }

    #[test]
    fn set_changes_the_budget_and_refuses_non_vehicles() {
        let mut world = World::new();
        world.init_resource::<VehicleAudioRuntime>();
        let out = set(
            &mut world,
            &json!({ "budget": { "full_vehicles": 2, "max_distance": 90 } }),
        )
        .expect("set");
        assert_eq!(out["budget"]["full_vehicles"], json!(2));
        assert_eq!(world.resource::<VehicleAudioBudget>().max_distance, 90.0);
        let plain = world.spawn_empty().id();
        let err = set(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("not a vehicle");
        assert_eq!(code(&err), "vehicle_audio_not_vehicle");
        let err = set(&mut world, &json!({})).expect_err("nothing to do");
        assert_eq!(code(&err), "F0002");
        let err = set(&mut world, &json!({ "entity": 999_999_u64 })).expect_err("no such entity");
        assert_eq!(code(&err), "F0003");
    }

    #[test]
    fn set_leaves_the_budget_unchanged_when_the_entity_is_not_a_vehicle() {
        let mut world = World::new();
        world.init_resource::<VehicleAudioRuntime>();
        world.insert_resource(VehicleAudioBudget::default());
        let plain = world.spawn_empty().id();
        let err = set(
            &mut world,
            &json!({
                "budget": { "full_vehicles": 8, "reduced_vehicles": 20, "max_distance": 40 },
                "entity": plain.to_bits(),
            }),
        )
        .expect_err("not a vehicle");
        assert_eq!(code(&err), "vehicle_audio_not_vehicle");
        let budget = world.resource::<VehicleAudioBudget>();
        assert_eq!(
            budget.full_vehicles, 3,
            "a refused call must not store the budget"
        );
        assert_eq!(budget.reduced_vehicles, 12);
        assert_eq!(budget.max_distance, 250.0);
    }

    #[test]
    fn set_rejects_a_fractional_vehicle_count_without_changing_the_budget() {
        let mut world = World::new();
        world.init_resource::<VehicleAudioRuntime>();
        let err = set(&mut world, &json!({ "budget": { "full_vehicles": 1.5 } })).expect_err("full fraction");
        assert_eq!(code(&err), "F0002");
        assert!(
            err.message.contains("full_vehicles") && err.message.contains("whole number"),
            "{}",
            err.message
        );
        assert!(
            world.get_resource::<VehicleAudioBudget>().is_none(),
            "a refused count must not insert a budget"
        );

        world.insert_resource(VehicleAudioBudget::default());
        let err =
            set(&mut world, &json!({ "budget": { "reduced_vehicles": 2.5 } })).expect_err("reduced fraction");
        assert_eq!(code(&err), "F0002");
        assert!(err.message.contains("reduced_vehicles"), "{}", err.message);
        assert_eq!(world.resource::<VehicleAudioBudget>().reduced_vehicles, 12);

        let out = set(
            &mut world,
            &json!({
                "budget": { "full_vehicles": 64, "reduced_vehicles": 256, "max_distance": 10.5 }
            }),
        )
        .expect("integer limits and a fractional distance");
        assert_eq!(out["budget"]["full_vehicles"], json!(64));
        assert_eq!(out["budget"]["reduced_vehicles"], json!(256));
        assert_eq!(out["budget"]["max_distance"], json!(10.5));
    }

    #[test]
    fn set_rejects_a_non_boolean_remove_before_it_changes_anything() {
        let mut world = World::new();
        world.init_resource::<VehicleAudioRuntime>();
        let plain = world.spawn_empty().id();
        let err = set(&mut world, &json!({ "entity": plain.to_bits(), "remove": 1 })).expect_err("type");
        assert_eq!(code(&err), "F0002");
        assert!(err.message.contains("boolean"), "{}", err.message);
        assert!(world.get_resource::<VehicleAudioBudget>().is_none());
        assert!(world.get::<VehicleAudio>(plain).is_none());
    }
}
