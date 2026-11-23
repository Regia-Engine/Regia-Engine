// SPDX-License-Identifier: MPL-2.0
//! Locomotion quality on the agent protocol (ADR-8708, `book/agents/locomotion-quality.md`):
//!
//! - `anim.locomotion_state`: one rig's procedural gait, foot-plant report and foot-placement
//!   status in one JSON object.
//! - `anim.gait_simulate`: a stateless headless run of `regia_animation::gait` over a seeded
//!   synthetic ground, answering quality metrics (foot jumps, planted counts, reach) and a
//!   final pose hash.
//! - `anim.foot_plant_simulate`: the foot planter on the built-in demo rig and walk over the
//!   same grounds, answering slide, penetration, float and tilt.
//! - `anim.ik_report`: per IK graph node, what its solve reached (residual, iterations,
//!   clamp, weight), recorded only while the rig's IK debug flag is on.
//!
//! The two simulate methods are presentation tools: deterministic for identical inputs on one
//! target, never a simulation checksum.

use regia_animation::Animator;
use regia_animation::foot_ik::{
    FootGround, FootIkLeg, FootPlacementStatus, FootPlantInput, FootPlantLeg, FootPlantReport,
    FootPlantSettings, FootPlanter, apply_foot_plant,
};
use regia_animation::gait::{
    FootMode, GaitBody, GaitError, GaitGround, GaitMode, GaitParams, GaitPattern, GaitPatternSet,
    GaitPosture, GaitState, GroundHit, MAX_LEGS, MAX_PATTERNS, leg_array, step_gait, step_gait_driven,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// Most steps one simulate call runs.
pub const MAX_SIMULATE_STEPS: u64 = 20_000;
/// Most trace rows one simulate call answers.
pub const MAX_TRACE_ROWS: u64 = 2_000;
/// Most points of a `body_path`.
pub const MAX_BODY_PATH_POINTS: usize = 4_096;

const PAGE: &str = "see book/agents/locomotion-quality.md";

fn code_error(jsonrpc: i64, code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(jsonrpc, code, message, hint)
}

fn too_long(message: impl Into<String>) -> RpcError {
    code_error(
        protocol::INVALID_PARAMS,
        "simulate_too_long",
        message,
        "run at most 20000 steps and 2000 trace rows per call; split a longer run",
    )
}

fn bad_ground(message: impl Into<String>) -> RpcError {
    code_error(
        protocol::INVALID_PARAMS,
        "bad_ground",
        message,
        "ground.kind is flat, slope, steps or rubble with finite fields in range; see book/agents/locomotion-quality.md",
    )
}

fn gait_code(error: GaitError) -> &'static str {
    match error {
        GaitError::Settings => "gait_settings",
        GaitError::Input => "gait_input",
        GaitError::State => "gait_state",
        GaitError::Floor => "gait_floor",
        GaitError::StepTooLarge => "gait_step_too_large",
        GaitError::Pattern => "gait_pattern",
    }
}

/// A stepper refusal as a protocol error; `step` is the step it happened on, if any.
fn gait_error(error: GaitError, step: Option<u64>, detail: &str) -> RpcError {
    let message = match (step, detail.is_empty()) {
        (Some(step), _) => format!("step {step}: {error}"),
        (None, false) => format!("{detail}: {error}"),
        (None, true) => error.to_string(),
    };
    let mut rpc = code_error(protocol::INVALID_PARAMS, gait_code(error), message, PAGE);
    if let (Some(step), Some(Value::Object(data))) = (step, rpc.data.as_mut()) {
        data.insert("step".into(), json!(step));
    }
    rpc
}

fn settings_error(message: impl Into<String>) -> RpcError {
    let mut rpc = gait_error(GaitError::Settings, None, "");
    rpc.message = message.into();
    rpc
}

fn input_error(message: impl Into<String>) -> RpcError {
    let mut rpc = gait_error(GaitError::Input, None, "");
    rpc.message = message.into();
    rpc
}

/// `gait_input` for a required field read through `crate::params` (ADR-2622): a missing, `null` or
/// wrong-typed value keeps the code the page lists and names the field (`data.param`) and the
/// wanted type (`data.expected`), like the typed readers' `invalid_params`.
fn typed_input_error(param: &str, expected: &str, message: &str) -> RpcError {
    let mut rpc = input_error(message);
    if let Some(Value::Object(data)) = rpc.data.as_mut() {
        data.insert("param".into(), json!(param));
        data.insert("expected".into(), json!(expected));
    }
    rpc
}

fn pattern_error(message: impl Into<String>) -> RpcError {
    let mut rpc = gait_error(GaitError::Pattern, None, "");
    rpc.message = message.into();
    rpc
}

/// A finite number as JSON, `null` otherwise (documented per field on the page).
fn num(value: f32) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn vec3_json(v: [f32; 3]) -> Value {
    json!([v[0], v[1], v[2]])
}

/// Whether `value` holds a JSON `null` anywhere. `json!` writes a non-finite `f32` as `null`,
/// so an echo of a component or a settings struct with no optional fields has one exactly
/// when a number in it is not finite (ADR-10053).
fn has_null(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.iter().any(has_null),
        Value::Object(map) => map.values().any(has_null),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// The first key of `object` whose value holds a `null` (see [`has_null`]).
fn null_key(object: &Value) -> Option<&str> {
    object
        .as_object()?
        .iter()
        .find(|(_, v)| has_null(v))
        .map(|(k, _)| k.as_str())
}

/// The rig entity of `params.entity`; `no_such_entity` when it does not exist.
fn rig_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(code_error(
            protocol::NOT_FOUND,
            "no_such_entity",
            format!("entity {} does not exist", entity.to_bits()),
            "find rigs with scene.query_entities or anim.list",
        ));
    }
    Ok(entity)
}

// ---------------------------------------------------------------------------------------------
// anim.locomotion_state

fn foot_mode_name(mode: FootMode) -> &'static str {
    match mode {
        FootMode::Tripod => "pattern",
        FootMode::Forced => "forced",
        FootMode::Waiting => "waiting",
        FootMode::Hanging => "hanging",
    }
}

fn gait_json(state: &GaitState) -> Value {
    let legs = state.leg_count();
    let feet: Vec<Value> = (0..legs)
        .map(|i| {
            let foot = state.feet[i];
            json!({
                "position": vec3_json(foot.position),
                "planted": foot.planted,
                "normal": vec3_json(foot.normal),
                "mode": foot_mode_name(state.foot_modes[i]),
                "phase": state.foot_phase[i],
                "swinging": state.is_swinging(i),
            })
        })
        .collect();
    let pattern = state.pattern;
    json!({
        "mode": match state.mode {
            GaitMode::Integrating => "integrating",
            GaitMode::Driven => "driven",
        },
        "legs": legs,
        "pattern": {
            "index": state.pattern_index,
            "legs": pattern.legs,
            "duty_factor": pattern.duty_factor,
            "phase_offset": &pattern.phase_offset[..legs.min(MAX_LEGS)],
        },
        "blending": state.blending,
        "blend": state.blend,
        "beat": state.beat,
        "phase": state.phase,
        "position": vec3_json(state.position),
        "yaw": state.yaw,
        "velocity": vec3_json(state.velocity),
        "feet": feet,
        "planted_count": state.feet().iter().filter(|f| f.planted).count(),
        "refused": state.refused,
        "forced_steps": state.forced_steps,
        "replants": state.replants,
        "settled": state.settled,
        "power_lost": state.power_lost,
        "stability_margin": num(state.stability_margin),
        // Body posture (ADR-8706, wired by the roll-up ADR-8737): radians and the tilt
        // quaternion in the stepper's local frame.
        "attitude": {
            "pitch": state.pitch,
            "roll": state.roll,
            "quat": state.attitude,
            "body_height_target": num(state.body_height_target),
        },
    })
}

fn foot_plant_json(report: &FootPlantReport) -> Value {
    json!({
        "weight": report.weight,
        "pelvis_offset": report.pelvis_offset,
        "planted": report.planted,
        "lock": report.lock,
        "tilt_deg": report.tilt_deg,
        "slide": report.slide,
        "max_slide": report.max_slide,
        "heel_gap": report.heel_gap,
        "toe_gap": report.toe_gap,
    })
}

/// `anim.locomotion_state`.
pub(crate) fn locomotion_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_entity(world, params)?;
    let gait = world.get::<GaitState>(entity).map(gait_json);
    let plant = world.get::<FootPlantReport>(entity).map(foot_plant_json);
    // A component a scene file or a reflected write left non-finite is refused, not published
    // as `null` (which the page reserves for an absent component or an empty support set).
    for (name, echo) in [("GaitState", &gait), ("FootPlantReport", &plant)] {
        if let Some(key) = echo.as_ref().and_then(null_key) {
            return Err(gait_error(
                GaitError::State,
                None,
                &format!("entity {} {name}.{key} is not finite", entity.to_bits()),
            ));
        }
    }
    let status = world.get::<FootPlacementStatus>(entity).map(|s| {
        // `lod` is the IK level of detail (ADR-8728), wired by the roll-up (ADR-8737).
        json!({
            "probe_installed": s.probe_installed,
            "legs_resolved": s.legs_resolved,
            "lod": s.lod.name(),
        })
    });
    if gait.is_none() && plant.is_none() && status.is_none() {
        return Err(code_error(
            protocol::NOT_FOUND,
            "no_locomotion",
            format!(
                "entity {} has no GaitState, FootPlantReport or FootPlacementStatus",
                entity.to_bits()
            ),
            "spawn a GaitState, or give the rig a FootPlacement (or insert FootPlantReport each frame)",
        ));
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "gait": gait.unwrap_or(Value::Null),
        "foot_plant": plant.unwrap_or(Value::Null),
        "foot_placement": status.unwrap_or(Value::Null),
    }))
}

// ---------------------------------------------------------------------------------------------
// Synthetic grounds

/// A seeded synthetic ground for the simulate methods.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ground {
    /// A level floor.
    Flat { height: f32 },
    /// A plane rising `rise_x` per meter along +X and `rise_z` along +Z.
    Slope { height: f32, rise_x: f32, rise_z: f32 },
    /// Stairs: treads `run` meters deep, each `rise` higher along +X or +Z.
    Steps {
        height: f32,
        rise: f32,
        run: f32,
        along_x: bool,
    },
    /// Square blocks `cell` meters wide, each at a seeded height in `[0, amplitude)`.
    Rubble {
        height: f32,
        amplitude: f32,
        cell: f32,
        seed: u64,
    },
}

/// Mixes a seed and a cell index into 64 well-spread bits (SplitMix64 finalizer).
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl Ground {
    /// Height and unit normal at world `(x, z)`.
    fn sample(&self, x: f32, z: f32) -> (f32, [f32; 3]) {
        match *self {
            Self::Flat { height } => (height, [0.0, 1.0, 0.0]),
            Self::Slope {
                height,
                rise_x,
                rise_z,
            } => {
                let n = Vec3::new(-rise_x, 1.0, -rise_z).normalize();
                (height + rise_x * x + rise_z * z, n.to_array())
            }
            Self::Steps {
                height,
                rise,
                run,
                along_x,
            } => {
                let coord = if along_x { x } else { z };
                (height + rise * (coord / run).floor(), [0.0, 1.0, 0.0])
            }
            Self::Rubble {
                height,
                amplitude,
                cell,
                seed,
            } => {
                let ix = (x / cell).floor() as i64 as u64;
                let iz = (z / cell).floor() as i64 as u64;
                let bits = mix(seed ^ mix(ix ^ mix(iz)));
                let unit = (bits >> 40) as f32 / (1u64 << 24) as f32;
                (height + amplitude * unit, [0.0, 1.0, 0.0])
            }
        }
    }
}

/// The stepper's view of a synthetic ground.
#[derive(Clone, Copy)]
struct GaitFloor(Ground);

impl GaitGround for GaitFloor {
    fn sample(&self, x: f32, z: f32, _expected_y: f32, _band: f32) -> Option<GroundHit> {
        let (y, normal) = self.0.sample(x, z);
        Some(GroundHit { y, normal })
    }
}

fn ground_number(
    fields: &Map<String, Value>,
    key: &str,
    default: f32,
    range: (f32, f32),
) -> Result<f32, RpcError> {
    match fields.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite() && *v >= range.0 && *v <= range.1)
            .ok_or_else(|| {
                bad_ground(format!(
                    "`ground.{key}` must be a number in {}..={}",
                    range.0, range.1
                ))
            }),
    }
}

/// `ground` of a simulate call: absent is flat at height 0.
fn parse_ground(params: &Value) -> Result<Ground, RpcError> {
    let fields = match params.get("ground") {
        None | Some(Value::Null) => return Ok(Ground::Flat { height: 0.0 }),
        Some(Value::Object(fields)) => fields,
        Some(_) => return Err(bad_ground("`ground` must be an object with a `kind`")),
    };
    let kind = match fields.get("kind") {
        None | Some(Value::Null) => "flat",
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => {
            return Err(bad_ground(format!(
                "`ground.kind` is {other}; it must be the string flat, slope, steps or rubble"
            )));
        }
    };
    let allowed: &[&str] = match kind {
        "flat" => &["kind", "height"],
        "slope" => &["kind", "height", "rise_x", "rise_z"],
        "steps" => &["kind", "height", "rise", "run", "axis"],
        "rubble" => &["kind", "height", "amplitude", "cell", "seed"],
        other => {
            return Err(bad_ground(format!(
                "`ground.kind` is `{other}`; it must be flat, slope, steps or rubble"
            )));
        }
    };
    if let Some(key) = fields.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(bad_ground(format!(
            "a {kind} ground has no field `{key}` (fields: {})",
            allowed.join(", ")
        )));
    }
    let height = ground_number(fields, "height", 0.0, (-1.0e5, 1.0e5))?;
    Ok(match kind {
        "slope" => Ground::Slope {
            height,
            rise_x: ground_number(fields, "rise_x", 0.0, (-5.0, 5.0))?,
            rise_z: ground_number(fields, "rise_z", 0.0, (-5.0, 5.0))?,
        },
        "steps" => {
            let along_x = match fields.get("axis").map(Value::as_str) {
                None | Some(Some("z")) => false,
                Some(Some("x")) => true,
                Some(_) => return Err(bad_ground("`ground.axis` must be \"x\" or \"z\"")),
            };
            Ground::Steps {
                height,
                rise: ground_number(fields, "rise", 0.15, (-2.0, 2.0))?,
                run: ground_number(fields, "run", 0.3, (0.05, 100.0))?,
                along_x,
            }
        }
        "rubble" => {
            let seed = match fields.get("seed") {
                None | Some(Value::Null) => 0,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| bad_ground("`ground.seed` must be a nonnegative integer"))?,
            };
            Ground::Rubble {
                height,
                amplitude: ground_number(fields, "amplitude", 0.1, (0.0, 2.0))?,
                cell: ground_number(fields, "cell", 0.4, (0.05, 100.0))?,
                seed,
            }
        }
        _ => Ground::Flat { height },
    })
}

/// `steps` of a simulate call (default `default`); more than 20000 is `simulate_too_long`.
fn parse_steps(params: &Value, default: u64) -> Result<u64, RpcError> {
    let steps = match params.get("steps") {
        None | Some(Value::Null) => default,
        Some(value) => {
            if let Some(n) = value.as_u64() {
                n
            } else if value
                .as_f64()
                .is_some_and(|v| v.is_finite() && v > MAX_SIMULATE_STEPS as f64)
            {
                return Err(too_long(format!(
                    "`steps` is {value}; at most {MAX_SIMULATE_STEPS} per call"
                )));
            } else {
                return Err(RpcError::wrong_type("steps", "an integer from 1 to 20000"));
            }
        }
    };
    if steps == 0 {
        return Err(RpcError::invalid_params("`steps` must be at least 1"));
    }
    if steps > MAX_SIMULATE_STEPS {
        return Err(too_long(format!(
            "`steps` is {steps}; at most {MAX_SIMULATE_STEPS} per call"
        )));
    }
    Ok(steps)
}

fn parse_dt(params: &Value, error: impl Fn(String) -> RpcError) -> Result<f32, RpcError> {
    match params.get("dt") {
        None | Some(Value::Null) => Ok(1.0 / 60.0),
        Some(value) => value
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite() && *v > 0.0 && *v <= 0.1)
            .ok_or_else(|| error("`dt` must be a number above 0 and at most 0.1 seconds".into())),
    }
}

/// `trace_every` (0 or absent: no trace); `simulate_too_long` past 2000 rows.
fn parse_trace(params: &Value, steps: u64) -> Result<u64, RpcError> {
    let every = match params.get("trace_every") {
        None | Some(Value::Null) => return Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| RpcError::wrong_type("trace_every", "a nonnegative integer"))?,
    };
    if every > 0 && steps / every > MAX_TRACE_ROWS {
        return Err(too_long(format!(
            "{steps} steps every {every} is {} trace rows; at most {MAX_TRACE_ROWS}",
            steps / every
        )));
    }
    Ok(every)
}

// ---------------------------------------------------------------------------------------------
// anim.gait_simulate

/// The default walker: a six-leg tripod with the agent guide's fixture values.
fn default_gait() -> GaitParams {
    GaitParams {
        forward_speed: 2.4,
        reverse_speed: 1.1,
        strafe_speed: 0.8,
        acceleration: 3.0,
        step_length: 0.6,
        clearance: 0.7,
        swing_lift: 0.18,
        max_slope: 0.9,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array(
            [
                [-0.4, 0.5],
                [0.4, 0.5],
                [-0.5, 0.0],
                [0.5, 0.0],
                [-0.4, -0.5],
                [0.4, -0.5],
            ],
            [0.0; 2],
        ),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.15,
        turn_step_angle: 0.35,
        ground_band: 2.0,
        foothold_candidates: 16,
        foothold_spacing: 0.25,
        max_reach: [1.2; MAX_LEGS],
        hang_drop: 0.3,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

fn preset(name: &str, legs: usize) -> Result<GaitPattern, RpcError> {
    Ok(match name {
        "tripod" => GaitPattern::tripod(),
        "ripple" => GaitPattern::ripple(legs).map_err(|e| gait_error(e, None, "ripple"))?,
        "wave" => GaitPattern::wave(legs).map_err(|e| gait_error(e, None, "wave"))?,
        "quadruped_walk" => GaitPattern::quadruped_walk(),
        "trot" => GaitPattern::trot(),
        "pace" => GaitPattern::pace(),
        "bound" => GaitPattern::bound(),
        "biped_walk" => GaitPattern::biped_walk(),
        other => {
            return Err(pattern_error(format!(
                "unknown pattern `{other}` (tripod, ripple, wave, quadruped_walk, trot, pace, bound, biped_walk)"
            )));
        }
    })
}

const GAIT_SCALARS: &[&str] = &[
    "forward_speed",
    "reverse_speed",
    "strafe_speed",
    "acceleration",
    "step_length",
    "clearance",
    "swing_lift",
    "max_slope",
    "idle_speed",
    "max_yaw_rate",
    "settle_distance",
    "settle_period",
    "turn_step_distance",
    "turn_step_angle",
    "ground_band",
    "foothold_spacing",
    "hang_drop",
];

const GAIT_KEYS: &[&str] = &[
    "hip_offsets",
    "max_reach",
    "foothold_candidates",
    "patterns",
    "switch_speeds",
    "hysteresis",
    "posture",
];

/// Fields of `params.posture` (the `.gait.ron` posture fields, `GaitPostureSpec`), each written
/// over [`GaitPosture::UPRIGHT`] (ADR-10130).
const POSTURE_FIELDS: &[&str] = &[
    "swing_weight",
    "height_tau",
    "height_slew",
    "attitude_weight",
    "max_pitch",
    "max_roll",
    "attitude_tau",
];

/// `params.posture`: an object of posture fields over [`GaitPosture::UPRIGHT`]; `height_slew`
/// `null` is no limit. A non-object, an unknown field or a non-number is `gait_settings`;
/// ranges are checked with the rest of the settings by `GaitParams::validate`.
fn parse_posture(value: &Value) -> Result<GaitPosture, RpcError> {
    let mut posture = GaitPosture::UPRIGHT;
    let Some(fields) = value.as_object() else {
        return Err(settings_error(format!(
            "`params.posture` must be an object of posture fields ({})",
            POSTURE_FIELDS.join(", ")
        )));
    };
    for (key, value) in fields {
        let slot = match key.as_str() {
            "swing_weight" => &mut posture.swing_weight,
            "height_tau" => &mut posture.height_tau,
            "height_slew" => &mut posture.height_slew,
            "attitude_weight" => &mut posture.attitude_weight,
            "max_pitch" => &mut posture.max_pitch,
            "max_roll" => &mut posture.max_roll,
            "attitude_tau" => &mut posture.attitude_tau,
            _ => {
                return Err(settings_error(format!(
                    "`params.posture` has no field `{key}` (fields: {})",
                    POSTURE_FIELDS.join(", ")
                )));
            }
        };
        *slot = if key == "height_slew" && value.is_null() {
            f32::INFINITY
        } else {
            f32_of(value, &format!("posture.{key}"))?
        };
    }
    Ok(posture)
}

fn scalar_slot<'a>(p: &'a mut GaitParams, key: &str) -> Option<&'a mut f32> {
    Some(match key {
        "forward_speed" => &mut p.forward_speed,
        "reverse_speed" => &mut p.reverse_speed,
        "strafe_speed" => &mut p.strafe_speed,
        "acceleration" => &mut p.acceleration,
        "step_length" => &mut p.step_length,
        "clearance" => &mut p.clearance,
        "swing_lift" => &mut p.swing_lift,
        "max_slope" => &mut p.max_slope,
        "idle_speed" => &mut p.idle_speed,
        "max_yaw_rate" => &mut p.max_yaw_rate,
        "settle_distance" => &mut p.settle_distance,
        "settle_period" => &mut p.settle_period,
        "turn_step_distance" => &mut p.turn_step_distance,
        "turn_step_angle" => &mut p.turn_step_angle,
        "ground_band" => &mut p.ground_band,
        "foothold_spacing" => &mut p.foothold_spacing,
        "hang_drop" => &mut p.hang_drop,
        _ => return None,
    })
}

fn f32_of(value: &Value, what: &str) -> Result<f32, RpcError> {
    value
        .as_f64()
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
        .ok_or_else(|| settings_error(format!("`params.{what}` must be a finite number")))
}

/// `params` of `anim.gait_simulate` written over [`default_gait`], validated.
fn parse_gait(params: &Value) -> Result<GaitParams, RpcError> {
    let mut gait = default_gait();
    let fields = match params.get("params") {
        None | Some(Value::Null) => {
            gait.validate().map_err(|e| gait_error(e, None, "params"))?;
            return Ok(gait);
        }
        Some(Value::Object(fields)) => fields,
        Some(_) => return Err(settings_error("`params` must be an object of GaitParams fields")),
    };
    for (key, value) in fields {
        if let Some(slot) = scalar_slot(&mut gait, key) {
            *slot = f32_of(value, key)?;
        } else if !GAIT_KEYS.contains(&key.as_str()) {
            return Err(settings_error(format!(
                "`params` has no field `{key}` (fields: {}, {})",
                GAIT_SCALARS.join(", "),
                GAIT_KEYS.join(", ")
            )));
        }
    }
    // Hips first: their count is the leg count ripple and wave presets use.
    let mut legs = 6;
    if let Some(hips) = fields.get("hip_offsets") {
        let list = hips
            .as_array()
            .filter(|l| !l.is_empty() && l.len() <= MAX_LEGS)
            .ok_or_else(|| pattern_error("`params.hip_offsets` must list 1 to 16 [right, forward] pairs"))?;
        let mut offsets = [[0.0_f32; 2]; MAX_LEGS];
        for (slot, entry) in offsets.iter_mut().zip(list) {
            let pair = entry
                .as_array()
                .filter(|p| p.len() == 2)
                .ok_or_else(|| settings_error("each hip offset is a [right, forward] pair"))?;
            *slot = [f32_of(&pair[0], "hip_offsets")?, f32_of(&pair[1], "hip_offsets")?];
        }
        gait.hip_offsets = offsets;
        legs = list.len();
    }
    if let Some(value) = fields.get("foothold_candidates") {
        gait.foothold_candidates = value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| settings_error("`params.foothold_candidates` must be an integer 0..=64"))?;
    }
    match fields.get("max_reach") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            if list.len() != legs {
                return Err(settings_error(format!(
                    "`params.max_reach` lists {} values for {legs} legs",
                    list.len()
                )));
            }
            for (slot, value) in gait.max_reach.iter_mut().zip(list) {
                *slot = f32_of(value, "max_reach")?;
            }
        }
        Some(value) => gait.max_reach = [f32_of(value, "max_reach")?; MAX_LEGS],
    }
    let names: Vec<&str> = match fields.get("patterns") {
        None | Some(Value::Null) => vec!["tripod"],
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(list)) if !list.is_empty() && list.len() <= MAX_PATTERNS => list
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| pattern_error("pattern names are strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(pattern_error(
                "`params.patterns` is a pattern name or a list of 1 to 4",
            ));
        }
    };
    let patterns = names
        .iter()
        .map(|name| preset(name, legs))
        .collect::<Result<Vec<_>, _>>()?;
    if patterns.iter().any(|p| p.leg_count() != legs) {
        return Err(pattern_error(format!(
            "the patterns drive {} legs but `params.hip_offsets` lists {legs} hips",
            patterns.first().map_or(0, GaitPattern::leg_count)
        )));
    }
    let switch: Vec<f32> = match fields.get("switch_speeds") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) if list.len() < MAX_PATTERNS => list
            .iter()
            .map(|v| f32_of(v, "switch_speeds"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(pattern_error("`params.switch_speeds` lists up to 3 speeds")),
    };
    let hysteresis = match fields.get("hysteresis") {
        None | Some(Value::Null) => 0.0,
        Some(value) => f32_of(value, "hysteresis")?,
    };
    match fields.get("posture") {
        None | Some(Value::Null) => {}
        Some(value) => gait.posture = parse_posture(value)?,
    }
    gait.patterns = if patterns.len() == 1 && switch.is_empty() {
        GaitPatternSet::single(patterns[0])
    } else {
        GaitPatternSet::new(&patterns, &switch, hysteresis).map_err(|e| gait_error(e, None, "patterns"))?
    };
    gait.validate().map_err(|e| gait_error(e, None, "params"))?;
    Ok(gait)
}

/// Controls `[forward, strafe, steer]`, each held for its repeat count.
fn parse_inputs(params: &Value) -> Result<Vec<([f32; 3], u64)>, RpcError> {
    let Some(value) = params.get("inputs").filter(|v| !v.is_null()) else {
        return Ok(vec![([1.0, 0.0, 0.0], 1)]);
    };
    let list = value.as_array().filter(|l| !l.is_empty()).ok_or_else(|| {
        input_error(
            "`inputs` must be a nonempty list of [forward, strafe, steer] or [forward, strafe, steer, steps]",
        )
    })?;
    list.iter()
        .map(|entry| {
            let row = entry
                .as_array()
                .filter(|r| r.len() == 3 || r.len() == 4)
                .ok_or_else(|| {
                    input_error("each input is [forward, strafe, steer] or [forward, strafe, steer, steps]")
                })?;
            let mut control = [0.0_f32; 3];
            for (slot, v) in control.iter_mut().zip(row) {
                *slot = v
                    .as_f64()
                    .map(|x| x as f32)
                    .filter(|x| x.is_finite())
                    .ok_or_else(|| input_error("input controls must be finite numbers"))?;
            }
            let repeat = match row.get(3) {
                None => 1,
                Some(n) => n
                    .as_u64()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| input_error("an input's repeat count is an integer of at least 1"))?,
            };
            Ok((control, repeat))
        })
        .collect()
}

/// A driven body moving along a polyline at a constant speed, then stopping.
struct BodyPath {
    points: Vec<[f32; 2]>,
    speed: f32,
}

impl BodyPath {
    fn parse(value: &Value) -> Result<Self, RpcError> {
        let fields = value
            .as_object()
            .ok_or_else(|| input_error("`body_path` must be an object { points, speed }"))?;
        if let Some(key) = fields.keys().find(|k| !matches!(k.as_str(), "points" | "speed")) {
            return Err(input_error(format!(
                "`body_path` has no field `{key}` (points, speed)"
            )));
        }
        let points_error = || {
            typed_input_error(
                "body_path.points",
                "an array of 2 to 4096 [x, z] points",
                "`body_path.points` lists 2 to 4096 [x, z] points",
            )
        };
        let list = crate::params::opt_array(fields, "points")
            .map_err(|_| points_error())?
            .ok_or_else(points_error)?;
        if !(2..=MAX_BODY_PATH_POINTS).contains(&list.len()) {
            return Err(points_error());
        }
        let mut points = Vec::with_capacity(list.len());
        for entry in list {
            let pair = entry
                .as_array()
                .filter(|p| p.len() == 2)
                .ok_or_else(|| input_error("each body path point is [x, z]"))?;
            let mut point = [0.0_f32; 2];
            for (slot, v) in point.iter_mut().zip(pair) {
                *slot = v
                    .as_f64()
                    .map(|x| x as f32)
                    .filter(|x| x.is_finite() && x.abs() <= 1.0e5)
                    .ok_or_else(|| input_error("body path coordinates are finite, within 1e5 m"))?;
            }
            points.push(point);
        }
        let speed_error = || {
            typed_input_error(
                "body_path.speed",
                "a number in 0..=50",
                "`body_path.speed` is a number in 0..=50 m/s",
            )
        };
        let speed = crate::params::opt_f64(fields, "speed")
            .map_err(|_| speed_error())?
            .ok_or_else(speed_error)? as f32;
        if !(speed.is_finite() && (0.0..=50.0).contains(&speed)) {
            return Err(speed_error());
        }
        Ok(Self { points, speed })
    }

    /// Position, heading (object convention: forward `[-sin, 0, -cos]`) and velocity after
    /// travelling `distance` meters.
    fn at(&self, distance: f32) -> ([f32; 2], f32, [f32; 2]) {
        let mut left = distance.max(0.0);
        let mut yaw = 0.0;
        for pair in self.points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
            let length = (dx * dx + dz * dz).sqrt();
            if length <= 1.0e-6 {
                continue;
            }
            yaw = (-dx).atan2(-dz);
            if left <= length {
                let t = left / length;
                let velocity = [dx / length * self.speed, dz / length * self.speed];
                return ([a[0] + dx * t, a[1] + dz * t], yaw, velocity);
            }
            left -= length;
        }
        let end = self.points.last().copied().unwrap_or([0.0; 2]);
        (end, yaw, [0.0, 0.0])
    }
}

/// FNV-1a over 32-bit words.
fn fnv(words: impl Iterator<Item = u32>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for word in words {
        for byte in word.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn pose_hash(state: &GaitState) -> String {
    let feet = state
        .feet()
        .iter()
        .flat_map(|f| f.position.into_iter().chain([f32::from(u8::from(f.planted))]))
        .map(f32::to_bits);
    let body = state
        .position
        .into_iter()
        .chain(state.velocity)
        .chain([state.yaw, state.phase, state.stability_margin])
        .map(f32::to_bits);
    let counters = [
        state.refused,
        state.forced_steps,
        state.replants,
        u32::from(state.settled),
    ];
    format!("{:016x}", fnv(body.chain(feet).chain(counters)))
}

/// World hip of foot `leg` at body height, in the frame of the state's mode.
fn hip_world(params: &GaitParams, state: &GaitState, leg: usize) -> [f32; 3] {
    let [r, f] = params.hip_offsets[leg];
    let (sin, cos) = state.yaw.sin_cos();
    let (dx, dz) = match state.mode {
        // +Z forward: local (r, f) rotated by yaw about +Y.
        GaitMode::Integrating => (r * cos + f * sin, -r * sin + f * cos),
        // -Z forward: local (r, -f) rotated by yaw about +Y.
        GaitMode::Driven => (r * cos - f * sin, -r * sin - f * cos),
    };
    [state.position[0] + dx, state.position[1], state.position[2] + dz]
}

/// Running quality metrics of a gait run.
struct GaitMetrics {
    max_foot_jump: f32,
    max_planted_slip: f32,
    min_planted: usize,
    max_reach_ratio: Option<f32>,
    max_hip_distance: f32,
    max_speed: f32,
    min_stability_margin: f32,
}

impl GaitMetrics {
    /// Metrics before the first step. With a finite `max_reach` on any leg the reach ratio
    /// starts at 0 (no planted foot measured yet); `None` (`null`) is only "no finite reach"
    /// (ADR-10053).
    fn new(params: &GaitParams, legs: usize) -> Self {
        let finite_reach = params.max_reach[..legs.min(MAX_LEGS)]
            .iter()
            .any(|r| r.is_finite());
        Self {
            max_foot_jump: 0.0,
            max_planted_slip: 0.0,
            min_planted: legs,
            max_reach_ratio: finite_reach.then_some(0.0),
            max_hip_distance: 0.0,
            max_speed: 0.0,
            min_stability_margin: f32::INFINITY,
        }
    }

    fn observe(&mut self, params: &GaitParams, before: &GaitState, after: &GaitState) {
        let legs = after.leg_count();
        let mut planted = 0;
        for (i, (a, b)) in before.feet.iter().zip(&after.feet).enumerate().take(legs) {
            let d = [0, 1, 2].map(|k| b.position[k] - a.position[k]);
            let jump = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            self.max_foot_jump = self.max_foot_jump.max(jump);
            if a.planted && b.planted && !before.is_swinging(i) && !after.is_swinging(i) {
                self.max_planted_slip = self.max_planted_slip.max((d[0] * d[0] + d[2] * d[2]).sqrt());
            }
            if b.planted {
                planted += 1;
                let hip = hip_world(params, after, i);
                let h = [0, 1, 2].map(|k| b.position[k] - hip[k]);
                let distance = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
                self.max_hip_distance = self.max_hip_distance.max(distance);
                if params.max_reach[i].is_finite() {
                    // A tiny finite reach can overflow the ratio; `gait_simulate` refuses that.
                    let ratio = distance / params.max_reach[i];
                    self.max_reach_ratio = Some(self.max_reach_ratio.map_or(ratio, |r| r.max(ratio)));
                }
            }
        }
        self.min_planted = self.min_planted.min(planted);
        let v = after.velocity;
        self.max_speed = self.max_speed.max((v[0] * v[0] + v[2] * v[2]).sqrt());
        if after.stability_margin.is_finite() {
            self.min_stability_margin = self.min_stability_margin.min(after.stability_margin);
        }
    }

    /// The most a foot can move in one step on its swing arc (the page's formula): the
    /// longest swing (a stride, two step lengths of re-aim and the foothold search radius)
    /// plus the arc's lift, over the shortest swing time, plus the body's own motion and
    /// turn.
    fn swing_bound(&self, params: &GaitParams, dt: f32) -> f32 {
        let set = &params.patterns;
        let duty = set.patterns[..usize::from(set.count).max(1)]
            .iter()
            .map(|p| p.duty_factor)
            .fold(1.0_f32, f32::min)
            .max(1.0e-3);
        let rings = params.foothold_candidates.div_ceil(8) as f32;
        let longest = params.step_length / duty + 2.0 * params.step_length + rings * params.foothold_spacing;
        let speed = self.max_speed.max(1.0e-3);
        let walking = (1.0 - duty) / duty * params.step_length / speed;
        let shortest = params.settle_period.min(walking).max(1.0e-4);
        let radius = params.hip_offsets[..params.legs()]
            .iter()
            .map(|[r, f]| (r * r + f * f).sqrt())
            .fold(0.0_f32, f32::max)
            + params.step_length;
        dt * ((longest + 4.0 * params.swing_lift) / shortest + self.max_speed + params.max_yaw_rate * radius)
    }
}

/// `gait_asset`: a project-relative `.gait.ron` read through the loader the engine uses
/// (`GaitAsset::from_ron`), so the simulation steps exactly the settings a `GaitRig` on that file
/// would. A file the loader refuses answers the loader's code (`gait_parse`, `gait_version`,
/// `gait_legs`, `gait_settings`, `gait_pattern`, `gait_io`).
fn load_gait_asset(world: &World, rel: &str) -> Result<GaitParams, RpcError> {
    use regia_animation::gait_asset::{GaitAsset, GaitAssetError};
    let full = crate::req_path::project_file(world, "gait_asset", rel)?;
    let loaded = std::fs::read_to_string(&full)
        .map_err(|error| GaitAssetError::Io(format!("{rel}: {error}")))
        .and_then(|text| GaitAsset::from_ron(&text));
    loaded.map(|asset| asset.params).map_err(|error| {
        code_error(
            protocol::INVALID_PARAMS,
            error.code(),
            format!("gait_asset {rel}: {error}"),
            "check the file with anim.gait_validate; see book/agents/procedural-gait.md",
        )
    })
}

fn trace_row(step: u64, state: &GaitState) -> Value {
    json!({
        "step": step,
        "position": vec3_json(state.position),
        "yaw": state.yaw,
        "phase": state.phase,
        "pattern_index": state.pattern_index,
        "planted": state.feet().iter().map(|f| f.planted).collect::<Vec<_>>(),
        "feet": state.feet().iter().map(|f| vec3_json(f.position)).collect::<Vec<_>>(),
    })
}

/// `anim.gait_simulate`.
pub(crate) fn gait_simulate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_gait_simulate").entered();
    if !(params.is_object() || params.is_null()) {
        return Err(RpcError::invalid_params("the parameters must be an object"));
    }
    let steps = parse_steps(params, 600)?;
    let trace_every = parse_trace(params, steps)?;
    let dt = parse_dt(params, input_error)?;
    let ground = parse_ground(params)?;
    let gait = match crate::params::opt_str(params, "gait_asset")? {
        Some(rel) => {
            if params.get("params").is_some_and(|v| !v.is_null()) {
                return Err(settings_error("pass either `params` or `gait_asset`, not both"));
            }
            load_gait_asset(world, rel)?
        }
        None => parse_gait(params)?,
    };
    let floor = GaitFloor(ground);
    let path = match params.get("body_path") {
        None | Some(Value::Null) => None,
        Some(value) => {
            if params.get("inputs").is_some_and(|v| !v.is_null()) {
                return Err(input_error("pass either `inputs` or `body_path`, not both"));
            }
            Some(BodyPath::parse(value)?)
        }
    };
    let inputs = if path.is_none() {
        parse_inputs(params)?
    } else {
        Vec::new()
    };
    let mut state = match &path {
        Some(path) => {
            let (xz, yaw, _) = path.at(0.0);
            let y = ground.sample(xz[0], xz[1]).0 + gait.clearance;
            GaitState::at_rest_driven(
                &gait,
                GaitBody {
                    position: [xz[0], y, xz[1]],
                    yaw,
                    velocity: [0.0; 3],
                },
                floor,
            )
        }
        None => {
            let y = ground.sample(0.0, 0.0).0 + gait.clearance;
            GaitState::at_rest(&gait, [0.0, y, 0.0], 0.0, floor)
        }
    }
    .map_err(|e| gait_error(e, None, "initial stance"))?;
    let mut metrics = GaitMetrics::new(&gait, state.leg_count());
    let mut trace = Vec::new();
    // Input cursor: (index, steps left on it); past the end the last input is held.
    let (mut cursor, mut left) = (0usize, inputs.first().map_or(0, |i| i.1));
    for step in 0..steps {
        let before = state;
        let result = match &path {
            Some(path) => {
                let time = (step + 1) as f32 * dt;
                let (xz, yaw, v) = path.at(path.speed * time);
                let y = ground.sample(xz[0], xz[1]).0 + gait.clearance;
                let body = GaitBody {
                    position: [xz[0], y, xz[1]],
                    yaw,
                    velocity: [v[0], 0.0, v[1]],
                };
                step_gait_driven(&gait, &mut state, body, dt, floor)
            }
            None => {
                if left == 0 && cursor + 1 < inputs.len() {
                    cursor += 1;
                    left = inputs[cursor].1;
                }
                left = left.saturating_sub(1);
                let control = inputs.get(cursor).map_or([0.0; 3], |i| i.0);
                step_gait(&gait, &mut state, control, dt, floor)
            }
        };
        result.map_err(|e| gait_error(e, Some(step), ""))?;
        metrics.observe(&gait, &before, &state);
        if trace_every > 0 && (step + 1).is_multiple_of(trace_every) {
            trace.push(trace_row(step + 1, &state));
        }
    }
    let swing_bound = metrics.swing_bound(&gait, dt);
    // `null` in a metric is reserved for "no finite reach" and "never planted": a finite
    // setting that overflows a metric (a huge `step_length`, a tiny `max_reach`) is refused
    // as out of the range that keeps the numbers finite (ADR-10053).
    let overflowed = [
        ("swing_bound", swing_bound),
        ("max_foot_jump", metrics.max_foot_jump),
        ("max_planted_slip", metrics.max_planted_slip),
        ("max_reach_ratio", metrics.max_reach_ratio.unwrap_or(0.0)),
        ("max_hip_distance", metrics.max_hip_distance),
        ("max_speed", metrics.max_speed),
    ]
    .into_iter()
    .find(|(_, v)| !v.is_finite());
    if let Some((name, _)) = overflowed {
        return Err(settings_error(format!(
            "metrics.{name} overflowed f32: `params` are outside the range that keeps the measurements finite"
        )));
    }
    Ok(json!({
        "mode": if path.is_some() { "driven" } else { "integrating" },
        "steps": steps,
        "dt": dt,
        "legs": state.leg_count(),
        "metrics": {
            "max_foot_jump": metrics.max_foot_jump,
            "max_planted_slip": metrics.max_planted_slip,
            "swing_bound": swing_bound,
            "min_planted": metrics.min_planted,
            "max_reach_ratio": metrics.max_reach_ratio.map_or(Value::Null, num),
            "max_hip_distance": metrics.max_hip_distance,
            "max_speed": metrics.max_speed,
            "refused": state.refused,
            "forced_steps": state.forced_steps,
            "replants": state.replants,
            "settled": state.settled,
            "stability_margin": num(state.stability_margin),
            "min_stability_margin": num(metrics.min_stability_margin),
            "final_pose_hash": pose_hash(&state),
        },
        "final": gait_json(&state),
        "trace": if trace_every > 0 { Value::Array(trace) } else { Value::Null },
    }))
}

// ---------------------------------------------------------------------------------------------
// anim.foot_plant_simulate

/// `settings` of a foot-plant run written over [`FootPlantSettings::default`].
fn parse_plant_settings(params: &Value) -> Result<FootPlantSettings, RpcError> {
    let base = FootPlantSettings::default();
    let Some(user) = params.get("settings").filter(|v| !v.is_null()) else {
        return Ok(base);
    };
    let Some(fields) = user.as_object() else {
        return Err(RpcError::invalid_params(
            "`settings` must be an object of FootPlantSettings fields",
        ));
    };
    let mut merged = match serde_json::to_value(base) {
        Ok(Value::Object(map)) => map,
        _ => return Err(RpcError::internal("foot plant settings did not serialize")),
    };
    for (key, value) in fields {
        if !merged.contains_key(key) {
            let known: Vec<&str> = merged.keys().map(String::as_str).collect();
            return Err(RpcError::invalid_params(format!(
                "`settings` has no field `{key}` (fields: {})",
                known.join(", ")
            )));
        }
        merged.insert(key.clone(), value.clone());
    }
    let settings: FootPlantSettings = serde_json::from_value(Value::Object(merged))
        .map_err(|e| RpcError::invalid_params(format!("`settings`: {e}")))?;
    // A number past the f32 range deserializes to an infinity; the planter would replace it
    // with a fallback (or read it as unlimited), so it is refused (ADR-10053).
    let echo = serde_json::to_value(settings)
        .map_err(|_| RpcError::internal("foot plant settings did not serialize"))?;
    if let Some(key) = null_key(&echo) {
        return Err(RpcError::invalid_params(format!(
            "`settings.{key}` must be a finite number within the f32 range"
        )));
    }
    Ok(settings)
}

/// The demo walk's quality numbers.
struct PlantWalk {
    max_slide: f32,
    target_slide: f32,
    stances: u32,
    max_penetration: f32,
    max_float: f32,
    max_tilt_deg: f32,
    tilt_sum: f64,
    tilt_samples: u32,
    largest_step: f32,
    pelvis_min: f32,
}

fn horizontal(a: Vec3, b: Vec3) -> f32 {
    Vec3::new(a.x - b.x, 0.0, a.z - b.z).length()
}

/// `anim.foot_plant_simulate`: the foot planter of `regia_animation::foot_ik` on the demo
/// humanoid's walk, played in place while the rig moves `speed_scale` times the authored speed
/// along −Z (rotated by `yaw`) over the ground. Measurements start after half a second.
pub(crate) fn foot_plant_simulate(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_animation::demo_rig::{
        DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_in_place, demo_locomotion,
    };
    use regia_animation::ik::IkChain;

    let _span = tracing::info_span!("agent_foot_plant_simulate").entered();
    if !(params.is_object() || params.is_null()) {
        return Err(RpcError::invalid_params("the parameters must be an object"));
    }
    let steps = parse_steps(params, 180)?;
    let dt = parse_dt(params, RpcError::invalid_params)?;
    let ground = parse_ground(params)?;
    let settings = parse_plant_settings(params)?;
    let speed_scale = match params.get("speed_scale") {
        None | Some(Value::Null) => 1.15,
        Some(v) => v
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite() && (0.0..=4.0).contains(x))
            .ok_or_else(|| RpcError::invalid_params("`speed_scale` must be a number in 0..=4"))?,
    };
    let yaw = match params.get("yaw") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite() && x.abs() <= 100.0)
            .ok_or_else(|| RpcError::invalid_params("`yaw` must be a finite number of radians"))?,
    };

    let skeleton =
        demo_humanoid(&DemoRig::default()).ok_or_else(|| RpcError::internal("the demo rig did not build"))?;
    let gait = DemoGait::standard()
        .into_iter()
        .find(|g| g.name == "walk")
        .ok_or_else(|| RpcError::internal("the demo walk is missing"))?;
    let clip = demo_locomotion(&skeleton, std::slice::from_ref(&gait))
        .pop()
        .map(|c| demo_in_place(&c))
        .ok_or_else(|| RpcError::internal("the demo walk clip did not build"))?;
    let legs: Vec<FootIkLeg> = DEMO_LEGS
        .iter()
        .map(|[hip, knee, ankle]| {
            FootIkLeg::new(IkChain {
                root: *hip as u32,
                mid: *knee as u32,
                tip: *ankle as u32,
            })
        })
        .collect();
    let bind = skeleton.bind_pose_pose();
    let bind_globals = skeleton.globals(&bind, None);
    let point = |globals: &[regia_core::math::Mat4], joint: u32| {
        globals
            .get(joint as usize)
            .map_or(Vec3::ZERO, |m| m.w_axis.truncate())
    };
    let ankle_height = point(&bind_globals, DEMO_LEGS[0][2] as u32).y;
    let length: Vec<f32> = legs
        .iter()
        .map(|l| {
            let hip = point(&bind_globals, l.chain.root);
            let knee = point(&bind_globals, l.chain.mid);
            let ankle = point(&bind_globals, l.chain.tip);
            (knee - hip).length() + (ankle - knee).length()
        })
        .collect();
    let rotation = Quat::from_rotation_y(yaw);
    let forward = rotation * Vec3::NEG_Z;
    let height = |p: Vec3| ground.sample(p.x, p.z).0;
    let normal = |p: Vec3| Vec3::from_array(ground.sample(p.x, p.z).1);

    let mut planter = FootPlanter::new(settings);
    let mut walk = PlantWalk {
        max_slide: 0.0,
        target_slide: 0.0,
        stances: 0,
        max_penetration: 0.0,
        max_float: 0.0,
        max_tilt_deg: 0.0,
        tilt_sum: 0.0,
        tilt_samples: 0,
        largest_step: 0.0,
        pelvis_min: 0.0,
    };
    let mut touchdown: Vec<Option<(Vec3, Vec3)>> = vec![None; legs.len()];
    let mut previous: Vec<Option<Vec3>> = vec![None; legs.len()];
    let settle = (0.5 / dt) as u64;
    let mut pose = bind.clone();
    let mut solved = bind.clone();
    for frame in 0..steps {
        let t = frame as f32 * dt;
        let along = forward * (gait.speed * speed_scale * t);
        let rig = Vec3::new(along.x, height(along), along.z);
        pose.clone_from(&bind);
        clip.sample_pose(t % gait.cycle, &mut pose);
        let globals = skeleton.globals(&pose, None);
        let to_world = |p: Vec3| rotation * p + rig;
        let inputs: Vec<FootPlantLeg> = legs
            .iter()
            .enumerate()
            .zip(&length)
            .map(|((i, leg), &length)| {
                let foot = to_world(point(&globals, leg.chain.tip));
                let probe = planter.pin(i).unwrap_or(foot);
                FootPlantLeg {
                    hip: to_world(point(&globals, leg.chain.root)),
                    foot,
                    length,
                    ankle_height,
                    ground: FootGround::Contact(Vec3::new(probe.x, height(probe), probe.z)),
                    normal: normal(probe),
                    contact: None,
                }
            })
            .collect();
        let authored: Vec<bool> = legs
            .iter()
            .map(|l| (point(&globals, l.chain.tip).y - ankle_height).abs() < 1.0e-3)
            .collect();
        let plan = planter.plan(&FootPlantInput {
            rig_translation: rig,
            rig_rotation: rotation,
            legs: &inputs,
            active: true,
            dt,
        });
        let report = planter.report(&plan);
        solved.clone_from(&pose);
        apply_foot_plant(&skeleton, &mut solved, &plan, &legs, Some(Vec3::NEG_Z));
        let solved_globals = skeleton.globals(&solved, None);
        for (i, leg) in legs.iter().enumerate() {
            let ankle = to_world(point(&solved_globals, leg.chain.tip));
            let target = plan.legs.get(i).map_or(ankle, |l| to_world(l.target));
            if frame > settle
                && let Some(prev) = previous[i]
            {
                walk.largest_step = walk.largest_step.max((ankle - prev).length());
            }
            previous[i] = Some(ankle);
            if frame <= settle || !authored[i] {
                touchdown[i] = None;
                continue;
            }
            let (ankle0, target0) = *touchdown[i].get_or_insert_with(|| {
                walk.stances += 1;
                (ankle, target)
            });
            walk.max_slide = walk.max_slide.max(horizontal(ankle, ankle0));
            walk.target_slide = walk.target_slide.max(horizontal(target, target0));
            walk.pelvis_min = walk.pelvis_min.min(plan.pelvis_offset);
            let tilt = report.tilt_deg.get(i).copied().unwrap_or(0.0);
            walk.max_tilt_deg = walk.max_tilt_deg.max(tilt);
            walk.tilt_sum += f64::from(tilt);
            walk.tilt_samples += 1;
            let cos = tilt.to_radians().cos().max(0.5);
            let expected = height(ankle) + ankle_height / cos;
            walk.max_penetration = walk.max_penetration.max(expected - ankle.y);
            walk.max_float = walk.max_float.max(ankle.y - expected);
        }
    }
    let mean_tilt = if walk.tilt_samples > 0 {
        (walk.tilt_sum / f64::from(walk.tilt_samples)) as f32
    } else {
        0.0
    };
    Ok(json!({
        "rig": "demo_humanoid",
        "clip": "walk (in place)",
        "steps": steps,
        "dt": dt,
        "speed_scale": speed_scale,
        "settings": serde_json::to_value(settings).unwrap_or(Value::Null),
        "metrics": {
            "stances": walk.stances,
            "max_slide": walk.max_slide,
            "target_slide": walk.target_slide,
            "max_penetration": walk.max_penetration,
            "max_float": walk.max_float,
            "max_tilt_deg": walk.max_tilt_deg,
            "mean_tilt_deg": mean_tilt,
            "largest_step": walk.largest_step,
            "pelvis_min": walk.pelvis_min,
        },
    }))
}

// ---------------------------------------------------------------------------------------------
// anim.ik_report

/// `anim.ik_report`: optionally turns the rig's IK debug flag on or off (`enable`), then
/// answers the IK rows of its latest evaluation.
pub(crate) fn ik_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_entity(world, params)?;
    let enable = crate::params::opt_bool(params, "enable")?;
    let Some(mut animator) = world.get_mut::<Animator>(entity) else {
        return Err(code_error(
            protocol::NOT_FOUND,
            "no_animator",
            format!("entity {} has no Animator", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    };
    if let Some(on) = enable {
        animator.set_ik_debug(on);
    }
    let snapshot = animator.debug_snapshot();
    let nodes: Vec<Value> = snapshot.as_ref().map_or_else(Vec::new, |debug| {
        debug
            .ik
            .iter()
            .map(|row| {
                let graph_weight = debug.node(&row.path).map_or(0.0, |n| n.weight);
                json!({
                    "path": row.path,
                    "kind": serde_json::to_value(row.kind).unwrap_or(Value::Null),
                    "solved": row.solved,
                    "residual": num(row.residual),
                    "iterations": row.iterations,
                    "clamped": row.clamped,
                    "weight": row.weight,
                    "graph_weight": graph_weight,
                })
            })
            .collect()
    });
    Ok(json!({
        "entity": entity.to_bits(),
        "debug": animator.ik_debug(),
        "captures": animator.ik_debug_captures(),
        "graph": animator.graph_def().is_some(),
        "nodes": nodes,
    }))
}
