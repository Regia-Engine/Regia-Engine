// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for match-flow inputs and `rules.match_*` parameters (ADR-8503).
//!
//! The fixtures are the book examples in `book/agents/match-rules.md`: a flow input is an object
//! tagged by `input`, and a parameter object carries `setup`, `objective`, `input` and `step`.

use regia_app::prelude::World;
use regia_framework::rules::match_flow::FlowInput;
use regia_framework::rules::session::MatchSession;
use serde_json::{Map, Value, json};

use crate::match_rules::{self, MAX_INPUTS, MAX_STEPS, MAX_UNITS};
use crate::protocol::RpcError;

use super::FuzzTarget;
use super::formats::text;

/// Book examples from `book/agents/match-rules.md`, in an order the host can apply: a live pause
/// and resume, then the ready gate, an observer's join, and the seats that never readied.
const FLOW_FIXTURE: &str = r#"[
  {"input":"seat","team":1,"slot":0},
  {"input":"seat","team":2,"slot":0},
  {"input":"host_pause","on":true},
  {"input":"host_pause","on":false},
  {"input":"pause","name":"ada"},
  {"input":"resume","name":"bev","host":false},
  {"input":"open_between"},
  {"input":"ready","team":1,"slot":0},
  {"input":"unready","team":1,"slot":0},
  {"input":"ready","team":1,"slot":0},
  {"input":"observe","team":2,"slot":0},
  {"input":"ready","team":2,"slot":0},
  {"input":"approve_join","team":2,"slot":0},
  {"input":"leave","team":2,"slot":0},
  {"input":"override_afk"}
]"#;

/// Steps `rules_match_params` will run. The agent allows `MAX_STEPS`; a value mutation can ask
/// for 65536, and that batch does not finish inside the harness call budget. The agent method is
/// unchanged. The loader still checks the agent cap, then refuses the oversized batch.
const FUZZ_EVAL_STEPS: u64 = 64;

const PARAMS_FIXTURE: &str = r#"{
  "setup": {"settings":{"flow":{"start_delay_ticks":5}},"playlist":[{"map_id":"harbor","mode":"ctf"}],"between":true},
  "objective": {"place_flags":{"teams":[1,2],"offset":40}},
  "input": {"inputs":[{"input":"seat","team":1,"slot":0},{"input":"ready","team":1,"slot":0}]},
  "step": {"steps":1,"units":[{"id":1,"team":1,"position":[0,0,0],"alive":true}]}
}"#;

fn load_flow(text: &str) -> Result<String, String> {
    let inputs: Vec<FlowInput> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if inputs.len() > MAX_INPUTS {
        return Err(format!("at most {MAX_INPUTS} inputs"));
    }
    let mut session = MatchSession::default();
    for input in &inputs {
        session.apply(input).map_err(|e| e.to_string())?;
    }
    serde_json::to_string(&inputs).map_err(|e| e.to_string())
}

fn load_params(text: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let obj = match value {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => return Err("rules match params must be an object".to_string()),
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "setup" | "objective" | "input" | "step") {
            return Err(format!("unknown field {key}"));
        }
    }
    if let Some(step) = obj.get("step") {
        reject_over_cap(step, "steps", MAX_STEPS)?;
        reject_over_len(step, "units", MAX_UNITS)?;
        if let Some(n) = step.get("steps").and_then(Value::as_u64)
            && n > FUZZ_EVAL_STEPS
        {
            return Err(format!("fuzz evaluates at most {FUZZ_EVAL_STEPS} steps"));
        }
    }
    if let Some(input) = obj.get("input") {
        reject_over_len(input, "inputs", MAX_INPUTS)?;
    }
    let mut world = World::new();
    let setup = obj.get("setup").cloned().unwrap_or_else(|| json!({}));
    match_rules::match_setup(&mut world, &setup).map_err(rpc_err)?;
    if let Some(objective) = obj.get("objective") {
        match_rules::match_objective(&mut world, objective).map_err(rpc_err)?;
    }
    if let Some(input) = obj.get("input") {
        match_rules::match_input(&mut world, input).map_err(rpc_err)?;
    }
    if let Some(step) = obj.get("step") {
        match_rules::match_step(&mut world, step).map_err(rpc_err)?;
    }
    let mut canon = Map::new();
    for key in ["setup", "objective", "input", "step"] {
        if let Some(value) = obj.get(key) {
            canon.insert(key.to_string(), value.clone());
        }
    }
    serde_json::to_string(&Value::Object(canon)).map_err(|e| e.to_string())
}

fn reject_over_cap(params: &Value, field: &str, cap: u64) -> Result<(), String> {
    let Some(value) = params.get(field).filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let n = value
        .as_u64()
        .ok_or_else(|| format!("`{field}` must be an integer"))?;
    if n > cap {
        return Err(format!("`{field}` must be at most {cap}"));
    }
    Ok(())
}

fn reject_over_len(params: &Value, field: &str, cap: usize) -> Result<(), String> {
    let Some(value) = params.get(field).filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let n = value
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| format!("`{field}` must be an array"))?;
    if n > cap {
        return Err(format!("`{field}` holds at most {cap}"));
    }
    Ok(())
}

fn rpc_err(err: RpcError) -> String {
    if err.message.is_empty() {
        "rejected".to_string()
    } else {
        err.message
    }
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "match_flow_input_json",
            None,
            0,
            true,
            || FLOW_FIXTURE.to_string(),
            load_flow,
        ),
        text(
            "rules_match_params",
            None,
            0,
            true,
            || PARAMS_FIXTURE.to_string(),
            load_params,
        ),
    ]
}
