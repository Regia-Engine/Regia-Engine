// SPDX-License-Identifier: MPL-2.0
//! `render.temporal_quality`: temporal-quality metrics of the TAAU resolve on the built-in
//! synthetic moving-camera sequences (ADR-1151, book `agents/temporal-quality.md`, guide topic
//! `temporal_quality`).
//!
//! The numbers come from the CPU reference of the resolve (`regia_render::taau_quality`), not
//! from the live GPU: they need no renderer or window and are the same on every machine. The
//! tuning parameters let an agent try other history-rejection constants in the model before it
//! proposes a shader change; the shader itself does not read them.

use regia_render::taau_quality::{
    ResolveTuning, SCENARIOS, SequenceSettings, TemporalReport, run_sequence, scenario,
};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most frames one call resolves per scenario (each costs a few milliseconds of CPU).
const MAX_FRAMES: u64 = 64;

fn unknown_scenario(name: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "unknown_scenario",
        format!("no temporal-quality scenario {name:?}"),
        format!(
            "use one of {} or leave scenario out for all",
            SCENARIOS.join(", ")
        ),
    )
}

/// A finite number in `range`, or `None` when absent.
fn number(object: &Value, name: &str, range: std::ops::RangeInclusive<f64>) -> Result<Option<f32>, RpcError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && range.contains(f))
            .map(|f| Some(f as f32))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`{name}` must be a number from {} to {}",
                    range.start(),
                    range.end()
                ))
            }),
    }
}

fn boolean(object: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    }
}

/// The shipped tuning with the fields `tuning` names replaced.
fn tuning(params: &Value) -> Result<ResolveTuning, RpcError> {
    let mut t = ResolveTuning::SHIPPED;
    let object = match params.get("tuning") {
        None | Some(Value::Null) => return Ok(t),
        Some(object @ Value::Object(_)) => object,
        Some(_) => return Err(RpcError::invalid_params("`tuning` must be an object")),
    };
    if let Some(v) = number(
        object,
        "history_weight",
        0.0..=f64::from(regia_render::taau_quality::MAX_HISTORY_WEIGHT),
    )? {
        t.history_weight = v;
    }
    if let Some(v) = number(object, "variance_gamma", 0.1..=100.0)? {
        t.variance_gamma = v;
    }
    if let Some(v) = number(object, "fractional_penalty", 0.0..=1.0)? {
        t.fractional_penalty = v;
    }
    if let Some(v) = number(object, "velocity_trust_px", 0.0..=1.0e4)? {
        t.velocity_trust_px = v;
    }
    if let Some(v) = number(object, "velocity_reject_px", 0.0..=1.0e4)? {
        t.velocity_reject_px = v;
    }
    if let Some(v) = boolean(object, "wide_motion_test")? {
        t.wide_motion_test = v;
    }
    if t.velocity_reject_px <= t.velocity_trust_px {
        return Err(RpcError::invalid_params(
            "`tuning.velocity_reject_px` must be above `tuning.velocity_trust_px`",
        ));
    }
    Ok(t)
}

fn tuning_json(t: &ResolveTuning) -> Value {
    json!({
        "history_weight": t.history_weight,
        "variance_gamma": t.variance_gamma,
        "fractional_penalty": t.fractional_penalty,
        "velocity_trust_px": t.velocity_trust_px,
        "velocity_reject_px": t.velocity_reject_px,
        "wide_motion_test": t.wide_motion_test,
    })
}

fn report_json(name: &str, r: &TemporalReport) -> Value {
    json!({
        "name": name,
        "ghost_leak": r.ghost_leak,
        "disoccluded_pixels": r.disoccluded_pixels,
        "rms_error": r.rms_error,
        "peak_ratio": r.peak_ratio,
        "frames_measured": r.frames_measured,
    })
}

/// `render.temporal_quality { scenario?, render_scale?, frames?, sky_motion?, tuning? }`.
///
/// Runs one built-in scenario (or all of them) through the CPU reference of the resolve and
/// returns `{ model, render_scale, frames, warmup, width, height, sky_motion, tuning, shipped,
/// scenarios: [{ name, ghost_leak, disoccluded_pixels, rms_error, peak_ratio, frames_measured }] }`.
pub(crate) fn temporal_quality(params: &Value) -> Result<Value, RpcError> {
    if !params.is_null() && !params.is_object() {
        return Err(RpcError::invalid_params("params must be an object"));
    }
    let names: Vec<String> = match params.get("scenario") {
        None | Some(Value::Null) => SCENARIOS.iter().map(|s| (*s).to_string()).collect(),
        Some(Value::String(name)) if scenario(name).is_some() => vec![name.clone()],
        Some(Value::String(name)) => return Err(unknown_scenario(name)),
        Some(_) => return Err(RpcError::invalid_params("`scenario` must be a string")),
    };
    let mut settings = SequenceSettings::default();
    if let Some(v) = number(params, "render_scale", 0.25..=1.0)? {
        settings.render_scale = v;
    }
    match params.get("frames") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let frames = v
                .as_u64()
                .filter(|f| (2..=MAX_FRAMES).contains(f))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`frames` must be an integer from 2 to {MAX_FRAMES}"))
                })?;
            settings.frames = frames as u32;
            // A third of the sequence converges before the metrics start (8 of the default 24).
            settings.warmup = (settings.frames / 3).min(8);
        }
    }
    if let Some(v) = boolean(params, "sky_motion")? {
        settings.sky_motion = v;
    }
    settings.tuning = tuning(params)?;
    let mut scenarios = Vec::with_capacity(names.len());
    for name in &names {
        let Some(scene) = scenario(name) else {
            return Err(unknown_scenario(name));
        };
        scenarios.push(report_json(name, &run_sequence(&scene, &settings)));
    }
    let mut out = Map::new();
    out.insert("model".into(), json!("cpu_reference"));
    out.insert("width".into(), json!(settings.width));
    out.insert("height".into(), json!(settings.height));
    out.insert("render_scale".into(), json!(settings.render_scale));
    out.insert("frames".into(), json!(settings.frames));
    out.insert("warmup".into(), json!(settings.warmup));
    out.insert("sky_motion".into(), json!(settings.sky_motion));
    out.insert("tuning".into(), tuning_json(&settings.tuning));
    out.insert("shipped".into(), json!(settings.tuning == ResolveTuning::SHIPPED));
    out.insert("scenarios".into(), Value::Array(scenarios));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    #[test]
    fn one_scenario_reports_its_metrics() {
        let out = temporal_quality(&json!({ "scenario": "moving_object", "frames": 12 })).expect("run");
        assert_eq!(out["model"], "cpu_reference");
        assert_eq!(out["shipped"], true);
        assert_eq!(out["warmup"], 4);
        let scenarios = out["scenarios"].as_array().expect("scenarios");
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0]["name"], "moving_object");
        assert_eq!(scenarios[0]["frames_measured"], 8);
        let leak = scenarios[0]["ghost_leak"].as_f64().expect("ghost_leak");
        assert!(leak.abs() < 0.05, "{out}");
        assert!(scenarios[0]["disoccluded_pixels"].as_u64().expect("count") > 0);
    }

    #[test]
    fn the_old_sky_motion_and_tuning_are_selectable() {
        let out = temporal_quality(&json!({
            "scenario": "stars_turning",
            "sky_motion": false,
            "tuning": { "wide_motion_test": false, "variance_gamma": 1.5 },
        }))
        .expect("run");
        assert_eq!(out["sky_motion"], false);
        assert_eq!(out["shipped"], false);
        assert_eq!(out["tuning"]["wide_motion_test"], false);
        let peak = out["scenarios"][0]["peak_ratio"].as_f64().expect("peak_ratio");
        assert!(peak < 0.3, "{out}");
    }

    #[test]
    fn bad_parameters_are_rejected_with_codes() {
        let err = temporal_quality(&json!({ "scenario": "nope" })).expect_err("unknown");
        assert_eq!(code(err), "unknown_scenario");
        for bad in [
            json!({ "scenario": 3 }),
            json!({ "render_scale": 0.1 }),
            json!({ "render_scale": "half" }),
            json!({ "frames": 1 }),
            json!({ "frames": 1000 }),
            json!({ "sky_motion": "yes" }),
            json!({ "tuning": 1 }),
            json!({ "tuning": { "history_weight": 0.999 } }),
            json!({ "tuning": { "velocity_trust_px": 3.0, "velocity_reject_px": 2.0 } }),
            json!([1, 2]),
        ] {
            let err = temporal_quality(&bad).expect_err("bad params");
            assert_eq!(code(err), "F0002", "{bad}");
        }
    }
}
