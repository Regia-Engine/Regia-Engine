// SPDX-License-Identifier: MPL-2.0
//! The `anim.locomotion_simulate` target (ADR-8708): the untrusted parameters of
//! `anim.gait_simulate` and `anim.foot_plant_simulate` as one JSON object
//! `{ "gait": <params>, "plant": <params> }`.
//!
//! `load` runs both handlers on a bare world. Every answer must be a result or a refusal whose
//! `data.code` the method's contract lists (or a universal code); anything else is a bug. A
//! valid `steps` is capped at 64 (gait) and 32 (plant) so mutations stay cheap; an out-of-range
//! `steps` is passed through so the `simulate_too_long` path is fuzzed too. The canonical
//! re-encoding is the parsed object.

use regia_ecs::prelude::World;
use serde_json::{Value, json};

use super::{BUG_PREFIX, FuzzInput, FuzzTarget};
use crate::protocol::RpcError;

/// Largest `steps` a fuzzed gait run takes.
const GAIT_STEPS: u64 = 64;
/// Largest `steps` a fuzzed foot-plant run takes.
const PLANT_STEPS: u64 = 32;

fn fixture() -> String {
    json!({
        "gait": {
            "steps": 48,
            "dt": 0.016_666_668,
            "ground": { "kind": "rubble", "amplitude": 0.12, "cell": 0.35, "seed": 7 },
            "params": {
                "hip_offsets": [[-0.3, 0.5], [0.3, 0.5], [-0.3, -0.5], [0.3, -0.5]],
                "patterns": ["quadruped_walk", "trot"],
                "switch_speeds": [1.0],
                "hysteresis": 0.1,
                "max_reach": 1.0,
                "foothold_candidates": 16
            },
            "inputs": [[1.0, 0.0, 0.2, 20], [0.0, 0.0, 1.0]],
            "trace_every": 16
        },
        "plant": {
            "steps": 24,
            "speed_scale": 1.15,
            "ground": { "kind": "steps", "rise": 0.1, "run": 0.4, "axis": "z" },
            "settings": { "lock": true, "max_tilt": 0.4 }
        }
    })
    .to_string()
}

/// `params` with a valid `steps` capped at `cap` (absent becomes `cap`).
fn capped(params: &Value, cap: u64) -> Value {
    let mut params = params.clone();
    if let Value::Object(fields) = &mut params {
        match fields.get("steps").map(Value::as_u64) {
            None => {
                fields.insert("steps".into(), json!(cap));
            }
            Some(Some(n)) if n <= crate::locomotion::MAX_SIMULATE_STEPS => {
                fields.insert("steps".into(), json!(n.min(cap)));
            }
            Some(_) => {}
        }
    }
    params
}

/// Checks one answer against `method`'s contract.
fn check(method: &str, answer: Result<Value, RpcError>) -> Result<bool, String> {
    let error = match answer {
        Ok(_) => return Ok(true),
        Err(error) => error,
    };
    let code = error
        .data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let listed = crate::contract::contract(method).is_some_and(|c| c.errors.contains(&code.as_str()))
        || crate::contract::UNIVERSAL_ERRORS.contains(&code.as_str());
    if !listed {
        return Err(format!(
            "{BUG_PREFIX}{method} answered {code:?} outside its contract: {}",
            error.message
        ));
    }
    Ok(false)
}

fn load(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let fields = value.as_object().ok_or("the input must be an object")?;
    if let Some(key) = fields.keys().find(|k| !matches!(k.as_str(), "gait" | "plant")) {
        return Err(format!("unknown section `{key}`"));
    }
    let mut world = World::new();
    let mut accepted = true;
    if let Some(gait) = fields.get("gait") {
        let answer = crate::locomotion::gait_simulate(&mut world, &capped(gait, GAIT_STEPS));
        accepted &= check("anim.gait_simulate", answer)?;
    }
    if let Some(plant) = fields.get("plant") {
        let answer = crate::locomotion::foot_plant_simulate(&mut world, &capped(plant, PLANT_STEPS));
        accepted &= check("anim.foot_plant_simulate", answer)?;
    }
    if !accepted {
        return Err("a simulate call refused its parameters".into());
    }
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "anim.locomotion_simulate",
        format: None,
        format_version: 0,
        round_trip: true,
        input: FuzzInput::Text { fixture, load },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_loads_and_out_of_range_steps_are_refused_in_contract() {
        let canonical = load(&fixture()).expect("fixture loads");
        assert_eq!(load(&canonical).expect("canonical loads"), canonical);
        let long = json!({ "gait": { "steps": 20_001 }, "plant": { "steps": 1e12 } }).to_string();
        let refused = load(&long).expect_err("too long");
        assert!(!refused.starts_with(BUG_PREFIX), "{refused}");
        let lava = json!({ "gait": { "ground": { "kind": "lava" } } }).to_string();
        assert!(!load(&lava).expect_err("bad ground").starts_with(BUG_PREFIX));
    }
}
