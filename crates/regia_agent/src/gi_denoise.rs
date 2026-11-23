// SPDX-License-Identifier: MPL-2.0
//! `seeing.gi_denoise_check` (`book/agents/gi-denoise.md`, ADR-864): runs the CPU reference GI
//! denoiser (`regia_render::gi_denoise`) on its deterministic synthetic scenarios and returns
//! the scores against the known ground truth: noise, flicker, bias, leak across a depth edge,
//! shadow-edge smearing, disocclusion error and lag after a lighting change. Optional settings
//! try a filter change before it is written into code; optional scenario fields resize or reseed
//! a run. Needs no renderer, no GPU and no scene: it is the same computation the regression tests
//! run, so an agent can reproduce a test's numbers and see how far a change moves them.
//!
//! `backend: "gpu"` runs the GPU twin on the app's `RenderDevice` instead (blocking, one
//! readback per frame: do not call it during a hitch measurement).
//!
//! Errors carry `data.error_code`: `gi_denoise_unknown_scenario`, `gi_denoise_unknown_setting`,
//! `gi_denoise_invalid_setting` (a value out of range, the field in the message),
//! `gi_denoise_too_large` (more than [`MAX_PIXEL_FRAMES`] pixel-frames per scenario),
//! `gi_denoise_gpu` (`backend: "gpu"` without a render device, or the GPU run failed).

use regia_app::prelude::*;
use regia_render::RenderDevice;
use regia_render::gi_denoise::scenario::{
    GiDenoiseReport, GiScenario, GiScenarioKind, run_scenario, run_scenario_gpu,
};
use regia_render::gi_denoise::{GiDenoiseError, GiDenoiseSettings};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// Most `width × height × frames` one scenario may run (seven times the default 64×48×48;
/// the call runs on the app's thread).
pub const MAX_PIXEL_FRAMES: u64 = 1 << 20;

/// The setting names `settings` accepts.
const SETTING_KEYS: &[&str] = &[
    "alpha_color",
    "alpha_moments",
    "max_history",
    "depth_tolerance",
    "normal_tolerance",
    "atrous_iterations",
    "sigma_depth",
    "depth_epsilon",
    "sigma_normal",
    "sigma_luminance",
    "variance_radius",
    "spatial_variance_frames",
    "history_from_first_pass",
    "antilag_sigma",
    "antilag_block",
];

/// The scenario fields the call accepts at the top level.
const SCENARIO_KEYS: &[&str] = &[
    "width",
    "height",
    "frames",
    "warmup",
    "spp",
    "seed",
    "pan_px",
    "box_speed",
    "step_frame",
    "box_irradiance",
    "blue_noise",
];

fn with_error_code(mut err: RpcError, error_code: &str) -> RpcError {
    if let Some(Value::Object(data)) = err.data.as_mut() {
        data.insert("error_code".into(), json!(error_code));
    }
    err
}

fn coded(error_code: &str, message: impl Into<String>) -> RpcError {
    with_error_code(RpcError::invalid_params(message), error_code)
}

/// Whether `backend` asks for the GPU twin (`"cpu"`, the default, or `"gpu"`).
fn parse_backend(value: Option<&Value>) -> Result<bool, RpcError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(name)) if name == "cpu" => Ok(false),
        Some(Value::String(name)) if name == "gpu" => Ok(true),
        Some(other) => Err(coded(
            "gi_denoise_invalid_setting",
            format!("`backend` must be \"cpu\" or \"gpu\", not {other}"),
        )),
    }
}

fn from_denoise(err: &GiDenoiseError) -> RpcError {
    coded(err.code(), err.to_string())
}

fn number(key: &str, v: &Value) -> Result<f64, RpcError> {
    v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| {
        coded(
            "gi_denoise_invalid_setting",
            format!("`{key}` must be a finite number"),
        )
    })
}

fn count(key: &str, v: &Value) -> Result<u32, RpcError> {
    v.as_u64().and_then(|x| u32::try_from(x).ok()).ok_or_else(|| {
        coded(
            "gi_denoise_invalid_setting",
            format!("`{key}` must be a whole number ≥ 0"),
        )
    })
}

fn signed(key: &str, v: &Value) -> Result<i32, RpcError> {
    v.as_i64().and_then(|x| i32::try_from(x).ok()).ok_or_else(|| {
        coded(
            "gi_denoise_invalid_setting",
            format!("`{key}` must be a whole number"),
        )
    })
}

/// Applies the `settings` object over the defaults.
fn parse_settings(value: Option<&Value>) -> Result<GiDenoiseSettings, RpcError> {
    let mut s = GiDenoiseSettings::default();
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(s);
    };
    let Some(map) = value.as_object() else {
        return Err(coded(
            "gi_denoise_invalid_setting",
            "`settings` must be an object of setting names to values",
        ));
    };
    for (key, v) in map {
        match key.as_str() {
            "alpha_color" => s.alpha_color = number(key, v)? as f32,
            "alpha_moments" => s.alpha_moments = number(key, v)? as f32,
            "max_history" => s.max_history = number(key, v)? as f32,
            "depth_tolerance" => s.depth_tolerance = number(key, v)? as f32,
            "normal_tolerance" => s.normal_tolerance = number(key, v)? as f32,
            "atrous_iterations" => s.atrous_iterations = count(key, v)?,
            "sigma_depth" => s.sigma_depth = number(key, v)? as f32,
            "depth_epsilon" => s.depth_epsilon = number(key, v)? as f32,
            "sigma_normal" => s.sigma_normal = number(key, v)? as f32,
            "sigma_luminance" => s.sigma_luminance = number(key, v)? as f32,
            "variance_radius" => s.variance_radius = count(key, v)?,
            "spatial_variance_frames" => s.spatial_variance_frames = number(key, v)? as f32,
            "antilag_sigma" => s.antilag_sigma = number(key, v)? as f32,
            "antilag_block" => s.antilag_block = count(key, v)?,
            "history_from_first_pass" => {
                s.history_from_first_pass = v.as_bool().ok_or_else(|| {
                    coded(
                        "gi_denoise_invalid_setting",
                        "`history_from_first_pass` must be a boolean",
                    )
                })?;
            }
            other => {
                return Err(coded(
                    "gi_denoise_unknown_setting",
                    format!("unknown setting `{other}`; known: {}", SETTING_KEYS.join(", ")),
                ));
            }
        }
    }
    s.validate().map_err(|e| from_denoise(&e))?;
    Ok(s)
}

/// The scenario kinds `scenario` names (a string, an array of strings, `"all"` or absent).
fn parse_kinds(value: Option<&Value>) -> Result<Vec<GiScenarioKind>, RpcError> {
    let unknown = |name: &str| {
        let known: Vec<&str> = GiScenarioKind::ALL.iter().map(|k| k.name()).collect();
        coded(
            "gi_denoise_unknown_scenario",
            format!("unknown scenario `{name}`; known: {}, or all", known.join(", ")),
        )
    };
    let one = |name: &str| -> Result<Vec<GiScenarioKind>, RpcError> {
        if name == "all" {
            Ok(GiScenarioKind::ALL.to_vec())
        } else {
            GiScenarioKind::from_name(name)
                .map(|k| vec![k])
                .ok_or_else(|| unknown(name))
        }
    };
    match value {
        None | Some(Value::Null) => Ok(GiScenarioKind::ALL.to_vec()),
        Some(Value::String(name)) => one(name),
        Some(Value::Array(items)) => {
            let mut kinds = Vec::new();
            for item in items {
                let name = item.as_str().ok_or_else(|| unknown(&item.to_string()))?;
                for kind in one(name)? {
                    if !kinds.contains(&kind) {
                        kinds.push(kind);
                    }
                }
            }
            Ok(kinds)
        }
        Some(other) => Err(unknown(&other.to_string())),
    }
}

/// Applies the top-level scenario fields over `kind`'s defaults.
fn scenario_for(kind: GiScenarioKind, params: &Map<String, Value>) -> Result<GiScenario, RpcError> {
    let mut sc = GiScenario::new(kind);
    for key in SCENARIO_KEYS {
        let Some(v) = params.get(*key).filter(|v| !v.is_null()) else {
            continue;
        };
        match *key {
            "width" => sc.width = count(key, v)?,
            "height" => sc.height = count(key, v)?,
            "frames" => sc.frames = count(key, v)?,
            "warmup" => sc.warmup = count(key, v)?,
            "spp" => sc.spp = count(key, v)?,
            "seed" => sc.seed = count(key, v)?,
            "pan_px" => sc.pan_px = signed(key, v)?,
            "box_speed" => sc.box_speed = signed(key, v)?,
            "step_frame" => sc.step_frame = count(key, v)?,
            "blue_noise" => {
                sc.blue_noise = v
                    .as_bool()
                    .ok_or_else(|| coded("gi_denoise_invalid_setting", "`blue_noise` must be a boolean"))?;
            }
            _ => sc.box_irradiance = number(key, v)? as f32,
        }
    }
    let pixel_frames = u64::from(sc.width) * u64::from(sc.height) * u64::from(sc.frames);
    if pixel_frames > MAX_PIXEL_FRAMES {
        return Err(coded(
            "gi_denoise_too_large",
            format!(
                "{}×{} × {} frames is {pixel_frames} pixel-frames; the limit is {MAX_PIXEL_FRAMES}",
                sc.width, sc.height, sc.frames
            ),
        ));
    }
    sc.validate().map_err(|e| from_denoise(&e))?;
    Ok(sc)
}

fn ratio(raw: f32, filtered: f32) -> Option<f32> {
    if filtered > 0.0 {
        Some(raw / filtered)
    } else {
        None
    }
}

fn report_json(r: &GiDenoiseReport) -> Value {
    json!({
        "scenario": r.scenario.name(),
        "width": r.width,
        "height": r.height,
        "frames": r.frames,
        "scored_frames": r.scored_frames,
        "raw_rel_rmse": r.raw_rel_rmse,
        "rel_rmse": r.rel_rmse,
        "max_rel_rmse": r.max_rel_rmse,
        "noise_reduction": ratio(r.raw_rel_rmse, r.rel_rmse),
        "raw_flicker": r.raw_flicker,
        "flicker": r.flicker,
        "flicker_reduction": ratio(r.raw_flicker, r.flicker),
        "bias": r.bias,
        "raw_shadow_edge_error": r.raw_shadow_edge_error,
        "shadow_edge_error": r.shadow_edge_error,
        "edge_leak": r.edge_leak,
        "disoccluded_rel_rmse": r.disoccluded_rel_rmse,
        "lag_frames": r.lag_frames,
        "mean_history": r.mean_history,
    })
}

fn settings_json(s: &GiDenoiseSettings) -> Value {
    json!({
        "alpha_color": s.alpha_color,
        "alpha_moments": s.alpha_moments,
        "max_history": s.max_history,
        "depth_tolerance": s.depth_tolerance,
        "normal_tolerance": s.normal_tolerance,
        "atrous_iterations": s.atrous_iterations,
        "sigma_depth": s.sigma_depth,
        "depth_epsilon": s.depth_epsilon,
        "sigma_normal": s.sigma_normal,
        "sigma_luminance": s.sigma_luminance,
        "variance_radius": s.variance_radius,
        "spatial_variance_frames": s.spatial_variance_frames,
        "history_from_first_pass": s.history_from_first_pass,
        "antilag_sigma": s.antilag_sigma,
        "antilag_block": s.antilag_block,
    })
}

/// `seeing.gi_denoise_check`: `{ scenario?, settings?, backend?, width?, height?, frames?,
/// warmup?, spp?, seed?, pan_px?, box_speed?, step_frame?, box_irradiance?, blue_noise? }` →
/// `{ backend, settings, scenarios: [...] }`.
pub(crate) fn gi_denoise_check(world: &World, params: &Value) -> Result<Value, RpcError> {
    let empty = Map::new();
    let map = match params {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => return Err(RpcError::invalid_params("params must be an object")),
    };
    let settings = parse_settings(map.get("settings"))?;
    let kinds = parse_kinds(map.get("scenario"))?;
    let device = if parse_backend(map.get("backend"))? {
        let Some(device) = world.get_resource::<RenderDevice>() else {
            return Err(with_error_code(
                RpcError::not_found(
                    "backend \"gpu\" needs a render device; this app has none",
                    "run it in an app with regia_render::RenderPlugin, or use backend \"cpu\"",
                ),
                "gi_denoise_gpu",
            ));
        };
        Some(device.clone())
    } else {
        None
    };
    let mut scenarios = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let sc = scenario_for(kind, map)?;
        let report = match device.as_ref() {
            Some(device) => run_scenario_gpu(&sc, &settings, device),
            None => run_scenario(&sc, &settings),
        }
        .map_err(|e| from_denoise(&e))?;
        scenarios.push(report_json(&report));
    }
    Ok(json!({
        "backend": if device.is_some() { "gpu" } else { "cpu" },
        "settings": settings_json(&settings),
        "scenarios": scenarios,
        "note": "deterministic synthetic scenes (book/agents/gi-denoise.md); ratios are relative to the \
                 ground truth's mean luminance",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_code(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|d| d.get("error_code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn default_call_scores_every_scenario() {
        let out = gi_denoise_check(&World::new(), &json!({})).expect("runs");
        let scenarios = out["scenarios"].as_array().expect("array");
        assert_eq!(scenarios.len(), 4);
        for s in scenarios {
            let raw = s["raw_rel_rmse"].as_f64().expect("raw");
            let filtered = s["rel_rmse"].as_f64().expect("filtered");
            assert!(filtered < raw / 5.0, "{s}");
        }
        assert_eq!(scenarios[3]["scenario"], "light_step");
        assert!(scenarios[3]["lag_frames"].as_u64().is_some());
        assert_eq!(out["settings"]["atrous_iterations"], 5);
        assert_eq!(out["backend"], "cpu");
    }

    #[test]
    fn one_scenario_with_settings_and_overrides() {
        let out = gi_denoise_check(
            &World::new(),
            &json!({
                "scenario": "static",
                "settings": { "atrous_iterations": 2, "history_from_first_pass": true, "antilag_block": 4 },
                "width": 32,
                "height": 24,
                "frames": 20,
                "warmup": 8,
                "seed": 7,
            }),
        )
        .expect("runs");
        let s = &out["scenarios"][0];
        assert_eq!(s["scenario"], "static");
        assert_eq!(s["width"], 32);
        assert_eq!(s["scored_frames"], 12);
        assert_eq!(out["settings"]["atrous_iterations"], 2);
        assert_eq!(out["settings"]["history_from_first_pass"], true);
        assert_eq!(out["settings"]["antilag_block"], 4);
        let err = gi_denoise_check(&World::new(), &json!({ "settings": { "antilag_block": 1 } }))
            .expect_err("block");
        assert_eq!(error_code(&err), Some("gi_denoise_invalid_setting"));
    }

    #[test]
    fn errors_carry_their_codes() {
        let err =
            gi_denoise_check(&World::new(), &json!({ "scenario": "nope" })).expect_err("unknown scenario");
        assert_eq!(error_code(&err), Some("gi_denoise_unknown_scenario"));
        let err = gi_denoise_check(&World::new(), &json!({ "settings": { "alpha": 0.5 } }))
            .expect_err("unknown setting");
        assert_eq!(error_code(&err), Some("gi_denoise_unknown_setting"));
        let err = gi_denoise_check(&World::new(), &json!({ "settings": { "alpha_color": 2.0 } }))
            .expect_err("range");
        assert_eq!(error_code(&err), Some("gi_denoise_invalid_setting"));
        assert!(err.message.contains("alpha_color"), "{}", err.message);
        let err = gi_denoise_check(
            &World::new(),
            &json!({ "width": 512, "height": 512, "frames": 64 }),
        )
        .expect_err("big");
        assert_eq!(error_code(&err), Some("gi_denoise_too_large"));
        let err =
            gi_denoise_check(&World::new(), &json!({ "scenario": "pan", "width": 8 })).expect_err("small");
        assert_eq!(error_code(&err), Some("gi_denoise_invalid_setting"));
        assert!(gi_denoise_check(&World::new(), &json!([1])).is_err());
        let err = gi_denoise_check(&World::new(), &json!({ "backend": "gpu" })).expect_err("no device");
        assert_eq!(error_code(&err), Some("gi_denoise_gpu"));
        let err = gi_denoise_check(&World::new(), &json!({ "backend": "tpu" })).expect_err("backend");
        assert_eq!(error_code(&err), Some("gi_denoise_invalid_setting"));
    }

    #[test]
    fn scenario_lists_deduplicate() {
        let out = gi_denoise_check(
            &World::new(),
            &json!({ "scenario": ["pan", "pan", "light_step"], "frames": 30 }),
        )
        .expect("runs");
        let names: Vec<&str> = out["scenarios"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|s| s["scenario"].as_str())
            .collect();
        assert_eq!(names, ["pan", "light_step"]);
    }
}
