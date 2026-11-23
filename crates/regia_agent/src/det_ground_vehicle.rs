// SPDX-License-Identifier: MPL-2.0
//! `detsim.ground_vehicle_simulate`: run a lockstep ground vehicle for N steps
//! (`book/agents/det-ground-vehicles.md`, ADR-8551).
//!
//! Stateless: each call carries the model, its parameters, a start state, the inputs and the floor,
//! and answers the final state, the step counts and a digest. Refusals are `F0002` with
//! `data.reason`: `bad_model`, `bad_params` or `steps_limit`.
//!
//! ADR-8638: the boundary refuses what could overflow a step. `params` go through `validate`
//! (lengths at least `MIN_LENGTH_M`), every `state` word must be finite and within
//! ±`MAX_STATE_MAGNITUDE`, and every number the answer carries is finite: a non-finite one is an
//! internal error, never a JSON `null` (serde_json writes NaN and the infinities as `null`).

use regia_detsim::ground_vehicle::tracked::{self, TrackedInput, TrackedParams, TrackedState};
use regia_detsim::ground_vehicle::wheeled::{self, WheeledInput, WheeledParams, WheeledState};
use regia_detsim::ground_vehicle::{
    GroundReport, MAX_SIMULATE_STEPS, MAX_STATE_MAGNITUDE, WorldPhysics, digest_words,
};
use regia_detsim::hover::host::FloorFn;
use regia_detsim::tick::tick_dt;
use regia_ecs::prelude::World;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

const HINT: &str = "see book/agents/det-ground-vehicles.md for the parameters";

fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn bad_params(message: impl Into<String>) -> RpcError {
    refused("bad_params", message)
}

/// `base` with the keys of `user` (an object) written over it, read back as `T`. An unknown key, a
/// wrong type or a non-object is `bad_params`.
fn overlay<T: Serialize + DeserializeOwned>(
    base: &T,
    user: Option<&Value>,
    what: &str,
) -> Result<T, RpcError> {
    let mut merged = match serde_json::to_value(base) {
        Ok(Value::Object(map)) => map,
        _ => return Err(RpcError::internal(format!("{what}: defaults are not an object"))),
    };
    match user {
        None | Some(Value::Null) => {}
        Some(Value::Object(fields)) => {
            for (key, value) in fields {
                if !merged.contains_key(key) {
                    let known: Vec<&str> = merged.keys().map(String::as_str).collect();
                    return Err(bad_params(format!(
                        "`{what}` has no field `{key}` (fields: {})",
                        known.join(", ")
                    )));
                }
                merged.insert(key.clone(), value.clone());
            }
        }
        Some(_) => return Err(bad_params(format!("`{what}` must be an object"))),
    }
    serde_json::from_value(Value::Object(merged)).map_err(|e| bad_params(format!("`{what}`: {e}")))
}

fn opt_f32(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .ok_or_else(|| bad_params(format!("`{key}` must be a finite number"))),
    }
}

/// The step count: 1 by default, 1..=10000; past the limit is `steps_limit`.
fn steps(params: &Value) -> Result<u32, RpcError> {
    match params.get("steps") {
        None | Some(Value::Null) => Ok(1),
        Some(v) => match v.as_u64() {
            Some(0) | None => Err(bad_params("`steps` must be an integer from 1 to 10000")),
            Some(n) if n > u64::from(MAX_SIMULATE_STEPS) => Err(refused(
                "steps_limit",
                format!(
                    "`steps` is {n}; one call runs at most {MAX_SIMULATE_STEPS} steps (call again from the answered state)"
                ),
            )),
            Some(n) => Ok(n as u32),
        },
    }
}

/// The inputs: an array of input objects; step `i` reads `inputs[min(i, len - 1)]` (the last one
/// held), and an empty or absent array is no input.
fn inputs<T: Serialize + DeserializeOwned + Default>(params: &Value) -> Result<Vec<T>, RpcError> {
    match params.get("inputs") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| overlay(&T::default(), Some(item), "inputs[]"))
            .collect(),
        Some(_) => Err(bad_params("`inputs` must be an array of input objects")),
    }
}

/// The floor plane `height + rise_x * x + rise_z * z` (all default 0).
fn floor_plane(params: &Value) -> Result<[f32; 3], RpcError> {
    let floor = match params.get("floor") {
        None | Some(Value::Null) => return Ok([0.0; 3]),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(bad_params("`floor` must be an object { height, rise_x, rise_z }")),
    };
    if let Some(key) = floor
        .keys()
        .find(|k| !matches!(k.as_str(), "height" | "rise_x" | "rise_z"))
    {
        return Err(bad_params(format!(
            "`floor` has no field `{key}` (fields: height, rise_x, rise_z)"
        )));
    }
    let as_value = Value::Object(floor.clone());
    let rise_x = opt_f32(&as_value, "rise_x", 0.0)?;
    let rise_z = opt_f32(&as_value, "rise_z", 0.0)?;
    if rise_x.abs() > 100.0 || rise_z.abs() > 100.0 {
        return Err(bad_params(
            "`floor.rise_x` and `floor.rise_z` must be within -100 to 100",
        ));
    }
    Ok([opt_f32(&as_value, "height", 0.0)?, rise_x, rise_z])
}

fn world(params: &Value) -> Result<WorldPhysics, RpcError> {
    let world = WorldPhysics {
        gravity_m_s2: opt_f32(params, "gravity_m_s2", WorldPhysics::EARTH.gravity_m_s2)?,
        ..WorldPhysics::EARTH
    };
    world
        .validate()
        .map_err(|e| bad_params(format!("`gravity_m_s2`: {e}")))?;
    Ok(world)
}

fn dt(params: &Value) -> Result<f32, RpcError> {
    let dt = opt_f32(params, "dt", tick_dt())?;
    if dt > 0.0 && dt <= 1.0 {
        Ok(dt)
    } else {
        Err(bad_params("`dt` must be above 0 and at most 1 second"))
    }
}

/// What the steps added up to.
#[derive(Default)]
struct Totals {
    grade_limited: u32,
    recovered: u32,
}

impl Totals {
    fn add(&mut self, report: GroundReport) {
        self.grade_limited += u32::from(report.grade_limited);
        self.recovered += u32::from(report.recovered);
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The names of `TrackedState::words`, in order.
const TRACKED_WORDS: [&str; 8] = [
    "pos[0]",
    "pos[1]",
    "pos[2]",
    "yaw",
    "left_speed",
    "right_speed",
    "pitch",
    "roll",
];

/// The names of `WheeledState::words`, in order.
const WHEELED_WORDS: [&str; 9] = [
    "pos[0]", "pos[1]", "pos[2]", "yaw", "speed", "steer", "slip", "pitch", "roll",
];

/// The start state's words are finite and within ±[`MAX_STATE_MAGNITUDE`] (a JSON number past the
/// `f32` range reads as an infinity), else `bad_params` naming the first bad word.
fn check_state(names: &[&str], words: &[f32]) -> Result<(), RpcError> {
    for (name, word) in names.iter().zip(words) {
        if !(word.is_finite() && word.abs() <= MAX_STATE_MAGNITUDE) {
            return Err(bad_params(format!(
                "`state.{name}` is {word}; every state word must be finite and within \
                 -{MAX_STATE_MAGNITUDE:e} to {MAX_STATE_MAGNITUDE:e}"
            )));
        }
    }
    Ok(())
}

/// A finite answer number. The steps never publish a non-finite word (ADR-8638), so a non-finite
/// one here is an engine bug: answered as an internal error, never as a JSON `null`.
fn number(what: &str, value: f32) -> Result<Value, RpcError> {
    if value.is_finite() {
        Ok(json!(value))
    } else {
        Err(RpcError::internal(format!(
            "`{what}` came out {value}; the ground-vehicle step published a non-finite number"
        )))
    }
}

/// The answered state (every word finite, checked as [`number`] does) and its digest.
fn state_answer<T: Serialize>(state: &T, names: &[&str], words: &[f32]) -> Result<(Value, Value), RpcError> {
    for (name, word) in names.iter().zip(words) {
        number(&format!("state.{name}"), *word)?;
    }
    Ok((to_value(state), json!(format!("{:016x}", digest_words(words)))))
}

/// `detsim.ground_vehicle_simulate`.
pub(crate) fn simulate(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !(params.is_object() || params.is_null()) {
        return Err(bad_params("the parameters must be an object"));
    }
    // A present, wrong-typed `model` is `invalid_params` naming the parameter (ADR-2622), not "missing".
    let model = match crate::params::opt_str(params, "model")? {
        Some(m @ ("tracked" | "wheeled")) => m,
        Some(other) => {
            return Err(refused(
                "bad_model",
                format!("`model` is `{other}`; it must be tracked or wheeled"),
            ));
        }
        None => return Err(refused("bad_model", "`model` (tracked or wheeled) is required")),
    };
    let steps = steps(params)?;
    let dt = dt(params)?;
    let world = world(params)?;
    let [height, rise_x, rise_z] = floor_plane(params)?;
    let floor = FloorFn(move |x: f32, z: f32| height + rise_x * x + rise_z * z);
    let mut totals = Totals::default();
    let mut answer = Map::new();
    if model == "tracked" {
        let p: TrackedParams = overlay(&TrackedParams::DEFAULT, params.get("params"), "params")?;
        p.validate().map_err(|e| bad_params(format!("`params`: {e}")))?;
        let mut state: TrackedState = overlay(&TrackedState::default(), params.get("state"), "state")?;
        check_state(&TRACKED_WORDS, &state.words())?;
        let inputs: Vec<TrackedInput> = inputs(params)?;
        for i in 0..steps as usize {
            let input = inputs.get(i).or(inputs.last()).copied().unwrap_or_default();
            totals.add(tracked::step(&p, &world, &floor, &mut state, &input, dt));
        }
        let (state_value, digest) = state_answer(&state, &TRACKED_WORDS, &state.words())?;
        answer.insert("params".into(), to_value(&p));
        answer.insert("state".into(), state_value);
        answer.insert(
            "center_speed".into(),
            number("center_speed", state.center_speed())?,
        );
        answer.insert(
            "yaw_rate".into(),
            number("yaw_rate", state.yaw_rate(p.separation_m))?,
        );
        answer.insert("digest".into(), digest);
    } else {
        let p: WheeledParams = overlay(&WheeledParams::DEFAULT, params.get("params"), "params")?;
        p.validate().map_err(|e| bad_params(format!("`params`: {e}")))?;
        let mut state: WheeledState = overlay(&WheeledState::default(), params.get("state"), "state")?;
        check_state(&WHEELED_WORDS, &state.words())?;
        let inputs: Vec<WheeledInput> = inputs(params)?;
        let mut max_slip = 0.0_f32;
        for i in 0..steps as usize {
            let input = inputs.get(i).or(inputs.last()).copied().unwrap_or_default();
            totals.add(wheeled::step(&p, &world, &floor, &mut state, &input, dt));
            if state.slip > max_slip {
                max_slip = state.slip;
            }
        }
        let (state_value, digest) = state_answer(&state, &WHEELED_WORDS, &state.words())?;
        answer.insert("params".into(), to_value(&p));
        answer.insert("state".into(), state_value);
        answer.insert("max_slip".into(), number("max_slip", max_slip)?);
        answer.insert("digest".into(), digest);
    }
    answer.insert("model".into(), json!(model));
    answer.insert("steps".into(), json!(steps));
    answer.insert("dt".into(), json!(dt));
    answer.insert("grade_limited_steps".into(), json!(totals.grade_limited));
    answer.insert("recovered_steps".into(), json!(totals.recovered));
    Ok(Value::Object(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(params: Value) -> Result<Value, RpcError> {
        simulate(&mut World::new(), &params)
    }

    fn reason(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn simulate_runs_both_models_and_repeats() {
        let tracked = json!({
            "model": "tracked",
            "steps": 128,
            "dt": 0.015625,
            "inputs": [ { "left_throttle": 1.0, "right_throttle": 1.0 } ],
        });
        let once = call(tracked.clone()).expect("tracked");
        let twice = call(tracked).expect("tracked");
        assert_eq!(once["digest"], twice["digest"]);
        assert!(once["state"]["pos"][2].as_f64().unwrap_or(0.0) < -5.0);
        assert_eq!(once["params"]["separation_m"], json!(2.0));
        let wheeled = call(json!({
            "model": "wheeled",
            "steps": 60,
            "params": { "max_speed_m_s": 20.0 },
            "inputs": [ { "throttle": 1.0 }, { "throttle": 1.0, "steer": 0.5 } ],
        }))
        .expect("wheeled");
        assert!(wheeled["state"]["speed"].as_f64().unwrap_or(0.0) > 5.0);
        assert!(
            wheeled["state"]["yaw"].as_f64().unwrap_or(0.0) < 0.0,
            "positive steer turns right"
        );
        assert_eq!(wheeled["recovered_steps"], json!(0));
    }

    #[test]
    fn a_steep_floor_reports_the_grade_limit() {
        let answer = call(json!({
            "model": "wheeled",
            "steps": 30,
            "floor": { "rise_z": -1.0 },
            "inputs": [ { "throttle": 1.0 } ],
        }))
        .expect("wheeled");
        assert_eq!(answer["grade_limited_steps"], json!(30));
        assert_eq!(answer["state"]["speed"], json!(0.0));
    }

    #[test]
    fn refusals_carry_their_reason() {
        let cases = [
            (json!({}), "bad_model"),
            (json!({ "model": "hover" }), "bad_model"),
            (json!({ "model": "tracked", "steps": 10_001 }), "steps_limit"),
            (json!({ "model": "tracked", "steps": 0 }), "bad_params"),
            (
                json!({ "model": "tracked", "params": { "separation_m": 0.0 } }),
                "bad_params",
            ),
            (
                json!({ "model": "tracked", "params": { "wheelbase_m": 2.0 } }),
                "bad_params",
            ),
            (
                json!({ "model": "wheeled", "params": { "max_steer_rad": "wide" } }),
                "bad_params",
            ),
            (json!({ "model": "wheeled", "inputs": {} }), "bad_params"),
            (json!({ "model": "wheeled", "dt": 0.0 }), "bad_params"),
            (json!({ "model": "wheeled", "gravity_m_s2": -1.0 }), "bad_params"),
            (
                json!({ "model": "wheeled", "floor": { "tilt": 1.0 } }),
                "bad_params",
            ),
            (json!([]), "bad_params"),
        ];
        for (params, expected) in cases {
            let error = call(params.clone()).expect_err("refused");
            assert_eq!(reason(&error), expected, "{params}");
            assert_eq!(error.code, protocol::INVALID_PARAMS);
        }
    }

    /// ADR-2622: a present `model` of the wrong JSON type is refused naming the parameter, not
    /// answered as if it were missing.
    #[test]
    fn a_wrong_typed_model_is_refused_by_name() {
        let error = call(json!({ "model": 5 })).expect_err("refused");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        let data = error.data.as_ref().expect("data");
        assert_eq!(data["param"], json!("model"), "{data}");
        assert!(data["expected"].is_string(), "{data}");
    }

    /// Every number in an answer is a finite JSON number: no `null` anywhere (serde_json writes a
    /// NaN or an infinity as `null`).
    fn assert_all_finite(path: &str, value: &Value) {
        match value {
            Value::Null => panic!("`{path}` is null (a NaN or infinite word)"),
            Value::Number(n) => assert!(n.as_f64().is_some_and(f64::is_finite), "`{path}` is {n}"),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    assert_all_finite(&format!("{path}[{i}]"), item);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    assert_all_finite(&format!("{path}.{key}"), item);
                }
            }
            Value::Bool(_) | Value::String(_) => {}
        }
    }

    /// ADR-8638 (review finding A): a length in the old open range `(0, 20]` as small as `1e-40`
    /// overflowed the yaw rate and the answer carried `null` for the state's X, Z and yaw (and the
    /// tracked `yaw_rate`) with `recovered_steps` 0. The boundary now refuses it, and the smallest
    /// valid length answers finite numbers.
    #[test]
    fn a_tiny_length_is_refused_and_no_answer_holds_null() {
        let tiny = [
            json!({
                "model": "tracked",
                "params": { "separation_m": 1.0e-40 },
                "inputs": [ { "left_throttle": 1.0, "right_throttle": -1.0 } ],
            }),
            json!({
                "model": "wheeled",
                "params": { "wheelbase_m": 1.0e-40 },
                "state": { "speed": 10.0, "steer": 0.2 },
                "inputs": [ { "steer": 1.0 } ],
            }),
            json!({ "model": "tracked", "params": { "half_length_m": 0.009 } }),
            json!({ "model": "wheeled", "params": { "track_width_m": 1.0e-45 } }),
        ];
        for params in tiny {
            match call(params.clone()) {
                Ok(answer) => panic!("{params} answered {answer}"),
                Err(error) => {
                    assert_eq!(reason(&error), "bad_params", "{params}");
                    assert!(error.message.contains("out_of_range"), "{}", error.message);
                }
            }
        }
        let smallest = [
            json!({
                "model": "tracked",
                "steps": 600,
                "params": { "separation_m": 0.01, "half_length_m": 0.01, "max_track_speed_m_s": 100.0,
                            "friction_accel_m_s2": 1000.0 },
                "inputs": [ { "left_throttle": 1.0, "right_throttle": -1.0 } ],
            }),
            json!({
                "model": "wheeled",
                "steps": 600,
                "params": { "wheelbase_m": 0.01, "track_width_m": 0.01, "max_steer_rad": 1.2,
                            "max_speed_m_s": 200.0, "accel_m_s2": 1000.0, "lateral_grip_m_s2": 1000.0 },
                "inputs": [ { "throttle": 1.0, "steer": 1.0 } ],
            }),
        ];
        for params in smallest {
            let answer = call(params.clone()).unwrap_or_else(|e| panic!("{params}: {}", e.message));
            assert_all_finite("answer", &answer);
            assert_eq!(answer["recovered_steps"], json!(0), "{params}");
        }
    }

    /// ADR-8638: a start state word past ±1e9 (or a JSON number past the `f32` range, which reads
    /// as an infinity) is `bad_params`, not a step that overflows.
    #[test]
    fn a_state_word_past_the_bound_is_refused() {
        let cases = [
            json!({ "model": "tracked", "state": { "left_speed": 3.0e38, "right_speed": -3.0e38 } }),
            json!({ "model": "tracked", "state": { "yaw": 1.0e39 } }),
            json!({ "model": "wheeled", "state": { "speed": 2.0e9 } }),
            json!({ "model": "wheeled", "state": { "pos": [0.0, 0.0, -1.0e10] } }),
        ];
        for params in cases {
            match call(params.clone()) {
                Ok(answer) => panic!("{params} answered {answer}"),
                Err(error) => {
                    assert_eq!(reason(&error), "bad_params", "{params}");
                    assert!(error.message.contains("state."), "{}", error.message);
                }
            }
        }
        let at_bound = call(json!({
            "model": "tracked",
            "steps": 10_000,
            "dt": 1.0,
            "params": { "separation_m": 0.01 },
            "state": { "pos": [1.0e9, -1.0e9, -1.0e9], "yaw": 1.0e9,
                       "left_speed": 1.0e9, "right_speed": -1.0e9 },
        }))
        .unwrap_or_else(|e| panic!("{}", e.message));
        assert_all_finite("answer", &at_bound);
        assert_eq!(at_bound["recovered_steps"], json!(0));
    }

    /// ADR-8638: seed-stable random parameters, hostile values in every field, through the method.
    /// Every call ends in an answer whose numbers are all finite (no `null`) with no recovery, or in
    /// an `F0002` refusal with a documented reason; never a panic or an internal error.
    #[test]
    fn simulate_fuzz_answers_finite_numbers_or_refuses() {
        use regia_detsim::ground_vehicle::FieldRange;
        use regia_test_utils::fuzz::FuzzRng;
        use std::panic::{AssertUnwindSafe, catch_unwind};

        const CALLS: usize = 2_000;
        let numbers: [Value; 30] = [
            json!(0.0),
            json!(-0.0),
            json!(1.0e-45),
            json!(1.0e-40),
            json!(1.0e-30),
            json!(0.0099),
            json!(0.01),
            json!(0.5),
            json!(1.0),
            json!(-1.0),
            json!(2.0),
            json!(19.999),
            json!(20.0),
            json!(20.001),
            json!(89.0),
            json!(100.0),
            json!(200.0),
            json!(1000.0),
            json!(1000.5),
            json!(1.0e9),
            json!(-1.0e9),
            json!(1.000_001e9),
            json!(3.4e38),
            json!(-3.4e38),
            json!(1.0e39),
            json!(1.0e300),
            json!(1.2),
            json!(0.25),
            json!(10_000),
            json!(10_001),
        ];
        let others: [Value; 7] = [
            Value::Null,
            json!(true),
            json!("x"),
            json!([]),
            json!({}),
            json!([1.0, 2.0, 3.0]),
            json!(-7),
        ];
        let param_fields: Vec<&str> = TrackedParams::RANGES
            .iter()
            .chain(WheeledParams::RANGES.iter())
            .map(|r| r.name)
            .chain(["pivot_turn", "bogus"])
            .collect();
        let state_fields = [
            "pos",
            "yaw",
            "left_speed",
            "right_speed",
            "speed",
            "steer",
            "slip",
            "pitch",
            "roll",
        ];
        let input_fields = [
            "left_throttle",
            "right_throttle",
            "throttle",
            "brake",
            "steer",
            "handbrake",
        ];
        let mut rng = FuzzRng::new(0x8638_6E0D);
        let pick = |rng: &mut FuzzRng| -> Value {
            if rng.chance(8) {
                others[rng.below(others.len())].clone()
            } else {
                numbers[rng.below(numbers.len())].clone()
            }
        };
        let (mut answered, mut refused_calls) = (0, 0);
        for call_index in 0..CALLS {
            let mut request = Map::new();
            let model = match rng.below(12) {
                0 => json!("hover"),
                1 => Value::Null,
                n if n.is_multiple_of(2) => json!("tracked"),
                _ => json!("wheeled"),
            };
            let wheeled_model = model == json!("wheeled");
            request.insert("model".into(), model);
            let object = |rng: &mut FuzzRng, fields: &[&str], pos_is_array: bool| -> Value {
                let mut map = Map::new();
                for _ in 0..rng.below(4) {
                    let field = fields[rng.below(fields.len())];
                    let value = if pos_is_array && field == "pos" && rng.chance(2) {
                        json!([pick(rng), pick(rng), pick(rng)])
                    } else if field == "handbrake" || field == "pivot_turn" {
                        json!(rng.chance(2))
                    } else {
                        pick(rng)
                    };
                    map.insert(field.into(), value);
                }
                Value::Object(map)
            };
            if rng.chance(2) {
                // Valid values at the ends and the middle of the model's own ranges: these calls
                // step, so the fuzz reaches the arithmetic and not only the refusals.
                let ranges: &[FieldRange] = if wheeled_model {
                    &WheeledParams::RANGES
                } else {
                    &TrackedParams::RANGES
                };
                let mut map = Map::new();
                for _ in 0..rng.range(1, 5) {
                    let range = ranges[rng.below(ranges.len())];
                    let low = if range.min_exclusive {
                        range.min.next_up()
                    } else {
                        range.min
                    };
                    let value = [low, range.max, 0.5 * (low + range.max)][rng.below(3)];
                    map.insert(range.name.into(), json!(value));
                }
                request.insert("params".into(), Value::Object(map));
            } else if rng.chance(2) {
                let v = object(&mut rng, &param_fields, false);
                request.insert("params".into(), v);
            }
            if rng.chance(3) {
                let v = object(&mut rng, &state_fields, true);
                request.insert("state".into(), v);
            }
            if rng.chance(2) {
                let n = rng.below(4);
                let items: Vec<Value> = (0..n).map(|_| object(&mut rng, &input_fields, false)).collect();
                request.insert("inputs".into(), Value::Array(items));
            }
            if rng.chance(2) {
                let steps = [
                    json!(1),
                    json!(2),
                    json!(60),
                    json!(600),
                    json!(0),
                    json!(-1),
                    json!(1.5),
                ][rng.below(7)]
                .clone();
                request.insert("steps".into(), steps);
            }
            if rng.chance(3) {
                let dt = [
                    json!(1.0 / 60.0),
                    json!(1.0),
                    json!(1.0e-40),
                    json!(0.0),
                    json!(2.0),
                ][rng.below(5)]
                .clone();
                request.insert("dt".into(), dt);
            }
            if rng.chance(4) {
                let v = object(&mut rng, &["height", "rise_x", "rise_z"], false);
                request.insert("floor".into(), v);
            }
            if rng.chance(4) {
                request.insert("gravity_m_s2".into(), pick(&mut rng));
            }
            let params = Value::Object(request);
            let outcome = catch_unwind(AssertUnwindSafe(|| call(params.clone())));
            match outcome {
                Err(_) => panic!("call {call_index} panicked: {params}"),
                Ok(Ok(answer)) => {
                    answered += 1;
                    assert_all_finite(&format!("call {call_index} {params}"), &answer);
                    assert_eq!(answer["recovered_steps"], json!(0), "call {call_index} {params}");
                    let digest = answer["digest"].as_str().unwrap_or_default();
                    assert!(
                        digest.len() == 16 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                        "call {call_index} digest {digest}"
                    );
                }
                Ok(Err(error)) => {
                    refused_calls += 1;
                    assert_eq!(
                        error.code,
                        protocol::INVALID_PARAMS,
                        "call {call_index} {params}: {}",
                        error.message
                    );
                    assert!(
                        matches!(
                            reason(&error).as_str(),
                            "bad_model" | "bad_params" | "steps_limit"
                        ),
                        "call {call_index} {params}: reason {}",
                        reason(&error)
                    );
                }
            }
        }
        assert!(
            answered > CALLS / 10,
            "only {answered} of {CALLS} calls were answered"
        );
        assert!(
            refused_calls > CALLS / 10,
            "only {refused_calls} of {CALLS} calls were refused"
        );
    }
}
