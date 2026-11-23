// SPDX-License-Identifier: MPL-2.0
//! The lockstep hover on the agent protocol (`book/agents/det-hover.md`, plan row mp-hover-ecs-agent).
//!
//! Stateless (each call carries its own hull, world, state and floor, like `physics.track_step`):
//! `detsim.hover_step` steps a hull `steps` fixed ticks through
//! [`regia_framework::det_hover::tick_hull`]; `detsim.hover_probe` measures the feel numbers of a hull
//! ([`regia_framework::det_hover::probe`]). Stateful, over the `DetHoverHull` entities of the running
//! app: `detsim.hover_list`, `detsim.hover_get`, `detsim.hover_set_input`.
//!
//! Error codes: `hover_bad_params` (a hull, limits or world field is invalid; `data.field` and
//! `data.reason` carry the field and its validation code), `hover_bad_input` (an input, state, floor
//! or `dt` that is not a finite number of the right shape), `hover_no_hull` (the entity has no
//! hover hull), `hover_steps_limit` (`steps` outside 1 to 100000).

use regia_detsim::hover::host::{FlatFloor, FloorSampler, HeightGrid, HoverBody};
use regia_detsim::hover::{FlightMode, HoverLimits, HoverParams, HoverState, WorldPhysics};
use regia_detsim::tick::tick_dt;
use regia_ecs::prelude::{Entity, World};
use regia_framework::det_hover::probe::{ProbeNumbers, ProbeScript, probe};
use regia_framework::det_hover::{
    DetHoverConfig, DetHoverHull, DetHoverId, DetHoverInput, DetHoverLean, DetHoverState, DetHoverStats,
    compute_lean_of, hop_speed, lookahead_aim, tick_hull,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most ticks one `detsim.hover_step` call steps.
pub(crate) const MAX_HOVER_STEPS: u64 = 100_000;
/// Largest `dt` a call may step with, seconds.
const MAX_DT: f32 = 0.1;
/// Most posts of a `floor_profile` grid.
const MAX_FLOOR_POSTS: usize = 1 << 20;

const PAGE: &str = "see book/agents/det-hover.md";

fn bad_params(field: &str, reason: &str, message: impl Into<String>) -> RpcError {
    let mut err = RpcError::new(
        INVALID_PARAMS,
        "hover_bad_params",
        format!("[hover_bad_params] {}", message.into()),
        format!("send hull, limits and world fields inside their ranges; {PAGE}"),
    );
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("field".into(), Value::String(field.into()));
        data.insert("reason".into(), Value::String(reason.into()));
    }
    err
}

fn bad_input(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "hover_bad_input",
        format!("[hover_bad_input] {}", message.into()),
        format!("send finite numbers for the input, state, floor and dt; {PAGE}"),
    )
}

fn no_hull(entity: Entity) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "hover_no_hull",
        format!("[hover_no_hull] entity {} has no DetHoverHull", entity.to_bits()),
        "pick an entity from detsim.hover_list",
    )
}

/// A JSON object of `key`, or `None` when it is absent or null.
fn object<'a>(params: &'a Value, key: &str) -> Result<Option<&'a Map<String, Value>>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(RpcError::wrong_type(key, "an object")),
    }
}

/// `base` with the fields of `overlay` written over it, through serde: an unknown field or a wrong
/// type is `hover_bad_params` (or `hover_bad_input` for the state).
fn overlay<T: Serialize + DeserializeOwned>(
    base: &T,
    overlay: Option<&Map<String, Value>>,
    what: &str,
    params_error: bool,
) -> Result<T, RpcError> {
    let Some(overlay) = overlay else {
        return serde_json::to_value(base)
            .and_then(serde_json::from_value)
            .map_err(|e| RpcError::internal(e.to_string()));
    };
    let mut value = serde_json::to_value(base).map_err(|e| RpcError::internal(e.to_string()))?;
    let Value::Object(fields) = &mut value else {
        return Err(RpcError::internal(format!(
            "{what} does not serialize to an object"
        )));
    };
    for (key, v) in overlay {
        if !fields.contains_key(key) {
            let message = format!("`{what}` has no field `{key}`");
            return Err(if params_error {
                bad_params(&format!("{what}.{key}"), "unknown_field", message)
            } else {
                bad_input(message)
            });
        }
        fields.insert(key.clone(), v.clone());
    }
    serde_json::from_value(value).map_err(|e| {
        let message = format!("`{what}`: {e}");
        if params_error {
            bad_params(what, "wrong_type", message)
        } else {
            bad_input(message)
        }
    })
}

fn f32_of(value: &Value, what: &str) -> Result<f32, RpcError> {
    let n = value
        .as_f64()
        .ok_or_else(|| bad_input(format!("`{what}` must be a number")))?;
    let x = n as f32;
    if x.is_finite() {
        Ok(x)
    } else {
        Err(bad_input(format!("`{what}` = {n} is not a finite f32")))
    }
}

/// The hull of a call: `params`, `limits`, `flight`, `half_length`, `half_width` over
/// [`DetHoverHull::DEFAULT`], validated.
fn hull_from(params: &Value) -> Result<DetHoverHull, RpcError> {
    let hp: HoverParams = overlay(
        &HoverParams::DEFAULT_HULL,
        object(params, "params")?,
        "params",
        true,
    )?;
    let limits: HoverLimits = overlay(&HoverLimits::DEFAULT, object(params, "limits")?, "limits", true)?;
    let flight = match params.get("flight") {
        None | Some(Value::Null) => FlightMode::AirTime,
        Some(Value::String(name)) => FlightMode::from_name(name).ok_or_else(|| {
            bad_params(
                "flight",
                "out_of_range",
                format!("`flight` {name:?} is not air_time or clamped"),
            )
        })?,
        Some(_) => return Err(RpcError::wrong_type("flight", "a string")),
    };
    let mut hull = DetHoverHull::new(hp, limits, flight);
    for (key, slot) in [
        ("half_length", &mut hull.half_length),
        ("half_width", &mut hull.half_width),
    ] {
        if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
            let n = v.as_f64().ok_or_else(|| RpcError::wrong_type(key, "a number"))?;
            *slot = n as f32;
        }
    }
    hull.validate()
        .map_err(|e| bad_params(e.field, e.kind.code(), e.to_string()))?;
    Ok(hull)
}

/// The world of a call: `world` fields over [`WorldPhysics::EARTH`], validated.
fn world_from(params: &Value) -> Result<WorldPhysics, RpcError> {
    let mut world = WorldPhysics::EARTH;
    let Some(fields) = object(params, "world")? else {
        return Ok(world);
    };
    for (key, v) in fields {
        match key.as_str() {
            "gravity_m_s2" | "air_density_kg_m3" => {
                let n = v.as_f64().ok_or_else(|| {
                    bad_params(
                        &format!("world.{key}"),
                        "wrong_type",
                        format!("`world.{key}` must be a number"),
                    )
                })?;
                if key == "gravity_m_s2" {
                    world.gravity_m_s2 = n as f32;
                } else {
                    world.air_density_kg_m3 = n as f32;
                }
            }
            "vacuum" => {
                world.vacuum = v.as_bool().ok_or_else(|| {
                    bad_params("world.vacuum", "wrong_type", "`world.vacuum` must be a boolean")
                })?;
            }
            "low_g" => {
                world.low_g = match v.as_str() {
                    Some("governor") => regia_detsim::ballistic::LowG::Governor,
                    Some("physical") => regia_detsim::ballistic::LowG::Physical,
                    _ => {
                        return Err(bad_params(
                            "world.low_g",
                            "out_of_range",
                            "`world.low_g` must be \"governor\" or \"physical\"",
                        ));
                    }
                };
            }
            other => {
                return Err(bad_params(
                    &format!("world.{other}"),
                    "unknown_field",
                    format!("`world` has no field `{other}`"),
                ));
            }
        }
    }
    world
        .validate()
        .map_err(|e| bad_params(&format!("world.{}", e.field), e.kind.code(), e.to_string()))?;
    Ok(world)
}

/// The input object of a call (`throttle`, `strafe`, `steer`, `hop`, `jump`; missing fields are 0 or
/// false).
fn input_from(value: Option<&Value>) -> Result<DetHoverInput, RpcError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(DetHoverInput::default());
    };
    let Value::Object(fields) = value else {
        return Err(bad_input("`input` must be an object"));
    };
    let mut input = DetHoverInput::default();
    for (key, v) in fields {
        match key.as_str() {
            "throttle" => input.throttle = f32_of(v, "input.throttle")?,
            "strafe" => input.strafe = f32_of(v, "input.strafe")?,
            "steer" => input.steer = f32_of(v, "input.steer")?,
            "hop" | "jump" => {
                let b = v
                    .as_bool()
                    .ok_or_else(|| bad_input(format!("`input.{key}` must be a boolean")))?;
                if key == "hop" {
                    input.hop = b;
                } else {
                    input.jump = b;
                }
            }
            other => return Err(bad_input(format!("`input` has no field `{other}`"))),
        }
    }
    Ok(input)
}

/// The floor of a call: `floor` (a flat height) or `floor_profile` (a grid), flat 0 when neither.
fn floor_from(params: &Value) -> Result<Box<dyn FloorSampler>, RpcError> {
    let flat = params.get("floor").filter(|v| !v.is_null());
    let grid = params.get("floor_profile").filter(|v| !v.is_null());
    match (flat, grid) {
        (Some(_), Some(_)) => Err(bad_input("send `floor` or `floor_profile`, not both")),
        (Some(h), None) => Ok(Box::new(FlatFloor(f32_of(h, "floor")?))),
        (None, Some(Value::Object(g))) => {
            let origin = match g.get("origin") {
                None | Some(Value::Null) => [0.0, 0.0],
                Some(Value::Array(a)) if a.len() == 2 => [
                    f32_of(&a[0], "floor_profile.origin")?,
                    f32_of(&a[1], "floor_profile.origin")?,
                ],
                Some(_) => return Err(bad_input("`floor_profile.origin` must be [x, z]")),
            };
            let cell = f32_of(g.get("cell").unwrap_or(&Value::Null), "floor_profile.cell")?;
            // Required: a missing or wrong-typed count and one past `u32` are all refused.
            let count = |key: &str| -> Result<u32, RpcError> {
                let whole = || bad_input(format!("`floor_profile.{key}` must be a whole number"));
                let n = g.get(key).and_then(Value::as_u64).ok_or_else(whole)?;
                u32::try_from(n).map_err(|_| whole())
            };
            let (cols, rows) = (count("cols")?, count("rows")?);
            let Some(Value::Array(hs)) = g.get("heights") else {
                return Err(bad_input("`floor_profile.heights` must be an array of numbers"));
            };
            if hs.len() > MAX_FLOOR_POSTS {
                return Err(bad_input(format!(
                    "`floor_profile.heights` has {} posts (at most {MAX_FLOOR_POSTS})",
                    hs.len()
                )));
            }
            let heights = hs
                .iter()
                .map(|h| f32_of(h, "floor_profile.heights[]"))
                .collect::<Result<Vec<f32>, RpcError>>()?;
            let grid = HeightGrid::new(origin, cell, cols, rows, heights)
                .map_err(|e| bad_input(format!("`floor_profile`: {e}")))?;
            Ok(Box::new(grid))
        }
        (None, Some(_)) => Err(bad_input(
            "`floor_profile` must be an object { origin, cell, cols, rows, heights }",
        )),
        (None, None) => Ok(Box::new(FlatFloor(0.0))),
    }
}

fn dt_from(params: &Value) -> Result<f32, RpcError> {
    match params.get("dt").filter(|v| !v.is_null()) {
        None => Ok(tick_dt()),
        Some(v) => {
            let dt = f32_of(v, "dt")?;
            if dt > 0.0 && dt <= MAX_DT {
                Ok(dt)
            } else {
                Err(bad_input(format!("`dt` = {dt} is outside (0, {MAX_DT}]")))
            }
        }
    }
}

fn steps_from(params: &Value) -> Result<u32, RpcError> {
    match params.get("steps").filter(|v| !v.is_null()) {
        None => Ok(1),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_HOVER_STEPS).contains(&n) => Ok(n as u32),
            _ => Err(RpcError::new(
                INVALID_PARAMS,
                "hover_steps_limit",
                format!(
                    "[hover_steps_limit] `steps` = {v} is not a whole number from 1 to {MAX_HOVER_STEPS}"
                ),
                format!("split a longer run into calls and pass each result's `state` to the next; {PAGE}"),
            )),
        },
    }
}

/// The state of a call: `state` (`pos` plus `HoverState` fields over the default state), or the hull
/// at rest on the floor at the origin.
fn state_from(params: &Value, hull: &DetHoverHull, floor: &dyn FloorSampler) -> Result<HoverBody, RpcError> {
    let Some(fields) = object(params, "state")? else {
        let h = floor.floor(0.0, 0.0);
        return Ok(HoverBody::at_rest([0.0, h, 0.0], 0.0, &hull.params));
    };
    let mut rest = fields.clone();
    let pos = match rest.remove("pos") {
        None | Some(Value::Null) => [0.0; 3],
        Some(Value::Array(a)) if a.len() == 3 => [
            f32_of(&a[0], "state.pos")?,
            f32_of(&a[1], "state.pos")?,
            f32_of(&a[2], "state.pos")?,
        ],
        Some(_) => return Err(bad_input("`state.pos` must be [x, y, z]")),
    };
    let state: HoverState = overlay(&HoverState::default(), Some(&rest), "state", false)?;
    let body = HoverBody { pos, state };
    if !finite_body(&body) {
        return Err(bad_input("`state` holds a number that is not a finite f32"));
    }
    Ok(body)
}

fn finite_body(b: &HoverBody) -> bool {
    let s = &b.state;
    b.pos.iter().all(|x| x.is_finite())
        && s.velocity.iter().all(|x| x.is_finite())
        && [
            s.vertical_vel,
            s.yaw,
            s.yaw_rate,
            s.altitude,
            s.hop_cooldown,
            s.ground_climb,
            s.slope,
            s.roll,
            s.roll_rate,
            s.pitch,
            s.coast_drop_left,
            s.ground_pitch,
            s.ground_roll,
            s.cushion_pitch,
            s.cushion_pitch_rate,
        ]
        .iter()
        .all(|x| x.is_finite())
        && s.prev_terrain.is_none_or(f32::is_finite)
}

/// FNV-1a over the bits of every word of a body (`{:?}` of an `f32` round-trips, so the text is the
/// bits).
fn digest(body: &HoverBody) -> String {
    let text = format!("{body:?}");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn state_json(body: &HoverBody) -> Value {
    let mut state = serde_json::to_value(body.state).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut state {
        map.insert("pos".into(), json!(body.pos));
    }
    state
}

fn pose_json(hull: &DetHoverHull, body: &HoverBody, input: &DetHoverInput) -> Value {
    let lean = compute_lean_of(hull, &body.state, input);
    let s = &body.state;
    json!({
        "position": body.pos,
        "yaw": s.yaw,
        "cushion_pitch": s.cushion_pitch,
        "roll": s.roll,
        "lean_pitch": lean.pitch,
        "lean_roll": lean.roll,
        "speed": (s.velocity[0] * s.velocity[0] + s.velocity[2] * s.velocity[2]).sqrt(),
        "altitude": s.altitude,
        "aim_altitude": lookahead_aim(hull, s),
        "airborne": s.airborne,
        "in_air": hull.limits.hull_in_air(&hull.params, s),
    })
}

/// `detsim.hover_step`: step a hull `steps` fixed ticks with one held input.
pub(crate) fn hover_step(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let hull = hull_from(params)?;
    let world = world_from(params)?;
    let input = match params.get("input") {
        None | Some(Value::Null) => return Err(bad_input("`input` is required (an object; {} is neutral)")),
        Some(v) => input_from(Some(v))?,
    };
    let floor = floor_from(params)?;
    let dt = dt_from(params)?;
    let steps = steps_from(params)?;
    let mut body = state_from(params, &hull, floor.as_ref())?;
    let hop = if input.hop {
        hop_speed(&hull, &world, dt)
    } else {
        0.0
    };
    let (mut recovered, mut hops, mut jumps) = (0_u32, 0_u32, 0_u32);
    let mut last_floor = 0.0_f32;
    for _ in 0..steps {
        let tick = tick_hull(&hull, &mut body, &input, floor.as_ref(), &world, hop, dt);
        recovered += u32::from(tick.recovered);
        hops += u32::from(tick.hopped);
        jumps += u32::from(tick.jumped);
        last_floor = tick.floor;
    }
    Ok(json!({
        "state": state_json(&body),
        "pose": pose_json(&hull, &body, &input),
        "report": {
            "steps": steps,
            "dt": dt,
            "recovered": recovered,
            "hops": hops,
            "jumps": jumps,
            "floor": last_floor,
        },
        "digest": digest(&body),
    }))
}

fn numbers_json(n: &ProbeNumbers) -> Value {
    let mut out = Map::new();
    let mut put = |key: &str, v: Option<f32>| {
        if let Some(v) = v {
            out.insert(key.into(), json!(v));
        }
    };
    put("top_speed_m_s", n.top_speed_m_s);
    put("time_to_90_s", n.time_to_90_s);
    put("stop_distance_m", n.stop_distance_m);
    put("stop_time_s", n.stop_time_s);
    put("turn_radius_m", n.turn_radius_m);
    put("turn_speed_m_s", n.turn_speed_m_s);
    put("hop_apex_m", n.hop_apex_m);
    put("hop_air_s", n.hop_air_s);
    put("crest_air_s", n.crest_air_s);
    put("crest_peak_m", n.crest_peak_m);
    Value::Object(out)
}

/// `detsim.hover_probe`: the feel numbers of a hull, by a fixed-step scripted run.
pub(crate) fn hover_probe(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let hull = hull_from(params)?;
    let world = world_from(params)?;
    let dt = dt_from(params)?;
    let script = match params.get("script") {
        Some(Value::String(name)) => ProbeScript::from_name(name).ok_or_else(|| {
            bad_input(format!(
                "`script` {name:?} is not accel, brake, turn, hop or crest"
            ))
        })?,
        Some(_) => return Err(RpcError::wrong_type("script", "a string")),
        None => {
            return Err(bad_input(
                "`script` is required: accel, brake, turn, hop or crest",
            ));
        }
    };
    let numbers = probe(&hull, &world, dt, script);
    Ok(json!({
        "script": script.name(),
        "dt": dt,
        "ticks": numbers.ticks,
        "numbers": numbers_json(&numbers),
    }))
}

fn entity_from(params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .ok_or_else(|| RpcError::invalid_params("`entity` (an id from detsim.hover_list) is required"))?
        .as_u64()
        .ok_or_else(|| RpcError::wrong_type("entity", "an entity id"))?;
    Entity::try_from_bits(bits).ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

/// `detsim.hover_list`: every hull of the app, by sim id.
pub(crate) fn hover_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut q = world.query::<(Entity, &DetHoverHull, &DetHoverState, Option<&DetHoverId>)>();
    let mut rows: Vec<(Option<u64>, u64, Value)> = q
        .iter(world)
        .map(|(e, hull, state, id)| {
            let s = &state.0.state;
            let row = json!({
                "entity": e.to_bits(),
                "id": id.map(|i| i.0),
                "position": state.0.pos,
                "yaw": s.yaw,
                "speed": (s.velocity[0] * s.velocity[0] + s.velocity[2] * s.velocity[2]).sqrt(),
                "airborne": s.airborne,
                "in_air": hull.limits.hull_in_air(&hull.params, s),
            });
            (id.map(|i| i.0), e.to_bits(), row)
        })
        .collect();
    // Hulls with an id first, in step order; hulls without one after them.
    rows.sort_by_key(|(id, bits, _)| (id.is_none(), id.unwrap_or(0), *bits));
    let stats = world.get_resource::<DetHoverStats>().copied().unwrap_or_default();
    let config = world.get_resource::<DetHoverConfig>().copied();
    Ok(json!({
        "hulls": rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "stats": {
            "ticks": stats.ticks,
            "hulls": stats.hulls,
            "recovered": stats.recovered,
            "hops": stats.hops,
            "jumps": stats.jumps,
            "duplicate_ids": stats.duplicate_ids,
            "unordered": stats.unordered,
            "skipped": stats.skipped,
        },
        "plugin": config.is_some(),
        "dt": config.map(|c| c.dt),
    }))
}

/// `detsim.hover_get`: one hull's tuning, state, input and lean.
pub(crate) fn hover_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    let Ok(e) = world.get_entity(entity) else {
        return Err(no_hull(entity));
    };
    let (Some(hull), Some(state)) = (e.get::<DetHoverHull>(), e.get::<DetHoverState>()) else {
        return Err(no_hull(entity));
    };
    let input = e.get::<DetHoverInput>().copied().unwrap_or_default();
    let lean = e.get::<DetHoverLean>().copied().unwrap_or_default();
    let id = e.get::<DetHoverId>().map(|i| i.0);
    Ok(json!({
        "entity": entity.to_bits(),
        "id": id,
        "hull": {
            "params": serde_json::to_value(hull.params).unwrap_or(Value::Null),
            "limits": serde_json::to_value(hull.limits).unwrap_or(Value::Null),
            "flight": hull.flight.name(),
            "half_length": hull.half_length,
            "half_width": hull.half_width,
        },
        "state": state_json(&state.0),
        "pose": pose_json(hull, &state.0, &input),
        "input": {
            "throttle": input.throttle,
            "strafe": input.strafe,
            "steer": input.steer,
            "hop": input.hop,
            "jump": input.jump,
        },
        "lean": { "pitch": lean.pitch, "roll": lean.roll },
        "digest": digest(&state.0),
    }))
}

/// `detsim.hover_set_input`: replace one hull's input; the next fixed tick reads it.
pub(crate) fn hover_set_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_from(params)?;
    let input = match params.get("input") {
        None | Some(Value::Null) => return Err(bad_input("`input` is required (an object; {} is neutral)")),
        Some(v) => input_from(Some(v))?,
    };
    if world.get::<DetHoverHull>(entity).is_none() {
        return Err(no_hull(entity));
    }
    let before = world.get::<DetHoverInput>(entity).copied().unwrap_or_default();
    world.entity_mut(entity).insert(input);
    let as_json = |i: &DetHoverInput| json!({ "throttle": i.throttle, "strafe": i.strafe, "steer": i.steer, "hop": i.hop, "jump": i.jump });
    Ok(json!({ "entity": entity.to_bits(), "before": as_json(&before), "input": as_json(&input) }))
}

// `detsim.hover_hull_check`: the `.hoverhull.ron` load check (ADR-8512, ADR-8637).
mod hull_check;
pub(crate) use hull_check::hover_hull_check;

#[cfg(test)]
mod tests;
