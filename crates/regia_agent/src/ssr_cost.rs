// SPDX-License-Identifier: MPL-2.0
//! The stochastic SSR cost probe and cost bound on the agent protocol (ADR-2557,
//! `book/agents/ssr-cost.md`): `render.ssr_stats` reads the runtime renderer's newest reflection
//! counters (traced quads, glossy tiles and, with the probe on, the per-class pixels, rays and
//! hierarchical steps); `render.set_ssr_cost` turns the probe on or off and sets the opt-in cost
//! bound ([`regia_render::set_sssr_cost_bound`]) for every renderer in the process.

use regia_app::prelude::*;
use regia_render::{RuntimeRenderer, SssrCostBound, SssrCostCounters};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::sssr_stats)",
    )
}

/// A refused parameter: `invalid_params` with a reason code in `data.reason`.
fn bad(reason: &str, message: &str) -> RpcError {
    RpcError {
        code: crate::protocol::INVALID_PARAMS,
        message: message.to_string(),
        data: Some(json!({ "code": "F0002", "reason": reason, "hint": "see book/agents/ssr-cost.md" })),
    }
}

fn cost_json(cost: &SssrCostCounters) -> Value {
    json!({
        "mirror_pixels": cost.mirror_pixels,
        "glossy_pixels": cost.glossy_pixels,
        "skipped_pixels": cost.skipped_pixels,
        "rays": cost.rays,
        "variance_rays": cost.variance_rays,
        "budget_dropped_rays": cost.budget_dropped_rays,
        "shared_pixels": cost.shared_pixels,
        "steps": cost.steps,
        "steps_per_ray": cost.steps_per_ray(),
        "exhausted_rays": cost.exhausted_rays,
        "half_res_rays": cost.half_res_rays,
        "max_steps": cost.max_steps,
        "hits": cost.hits,
    })
}

fn bound_json(bound: Option<SssrCostBound>) -> Value {
    match bound {
        None => Value::Null,
        Some(b) => json!({
            "variance_rays": b.variance_rays,
            "tile_ray_budget": b.tile_ray_budget,
            "min_steps": b.min_steps,
            "step_falloff_depth_m": b.step_falloff_depth_m,
            "half_res_roughness": b.half_res_roughness,
            "block_roughness": b.block_roughness,
            "is_wet_preset": b == SssrCostBound::WET,
        }),
    }
}

/// The knobs in effect: the probe, the bound and the environment override.
fn knobs_json() -> Value {
    let env = match regia_render::sssr_cost_bound_env() {
        None => Value::Null,
        Some(None) => json!("off"),
        Some(Some(_)) => json!("wet"),
    };
    json!({
        "probe": regia_render::sssr_cost_probe(),
        "bound": bound_json(regia_render::sssr_cost_bound()),
        "bound_env": env,
        "env": regia_render::SSSR_COST_BOUND_ENV,
    })
}

/// `render.ssr_stats`: the newest stochastic SSR counters of the runtime renderer (a few frames
/// old; `stats: null` before the first readback or on a tier without the pass), whether the pass
/// is dormant, and the probe and bound in effect. `stats.cost` is `null` unless the probe was on
/// for that frame.
pub(crate) fn ssr_stats(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let stats = renderer.0.sssr_stats().map(|s| {
        json!({
            "ray_quads": s.ray_quads,
            "tiles": s.tiles,
            "cost": s.cost.as_ref().map_or(Value::Null, cost_json),
        })
    });
    let mut out = knobs_json();
    out["stats"] = stats.unwrap_or(Value::Null);
    out["dormant"] = json!(renderer.0.sssr_dormant());
    Ok(out)
}

/// Reads one bound field of `object` (`None` when absent).
fn field_f32(object: &serde_json::Map<String, Value>, name: &str) -> Result<Option<f32>, RpcError> {
    match object.get(name) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| Some(v as f32))
            .ok_or_else(|| bad("bad_bound", &format!("`bound.{name}` must be a finite number"))),
    }
}

fn field_u32(object: &serde_json::Map<String, Value>, name: &str, max: u64) -> Result<Option<u32>, RpcError> {
    match object.get(name) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|v| *v <= max)
            .map(|v| Some(v as u32))
            .ok_or_else(|| {
                bad(
                    "bad_bound",
                    &format!("`bound.{name}` must be an integer 0..={max}"),
                )
            }),
    }
}

/// The bound a `bound` parameter names: `"wet"`, `"off"` (or `null`), or an object of fields over
/// the wet preset.
fn parse_bound(value: &Value) -> Result<Option<SssrCostBound>, RpcError> {
    match value {
        Value::Null => Ok(None),
        Value::String(name) => match name.as_str() {
            "wet" => Ok(Some(SssrCostBound::WET)),
            "off" => Ok(None),
            _ => Err(bad(
                "bad_bound",
                "`bound` names \"wet\" or \"off\", or is an object of fields",
            )),
        },
        Value::Object(object) => {
            let known = [
                "variance_rays",
                "tile_ray_budget",
                "min_steps",
                "step_falloff_depth_m",
                "half_res_roughness",
                "block_roughness",
            ];
            if object.is_empty() {
                return Err(bad("bad_bound", "a `bound` object needs at least one field"));
            }
            if let Some(unknown) = object.keys().find(|k| !known.contains(&k.as_str())) {
                return Err(bad("bad_bound", &format!("unknown bound field `{unknown}`")));
            }
            let mut bound = SssrCostBound::WET;
            if let Some(v) = object.get("variance_rays") {
                bound.variance_rays = v
                    .as_bool()
                    .ok_or_else(|| bad("bad_bound", "`bound.variance_rays` must be a boolean"))?;
            }
            if let Some(v) = field_u32(object, "tile_ray_budget", 64)? {
                bound.tile_ray_budget = v;
            }
            if let Some(v) = field_u32(object, "min_steps", u64::from(regia_render::SSSR_MAX_STEPS))? {
                bound.min_steps = v;
            }
            if let Some(v) = field_f32(object, "step_falloff_depth_m")? {
                bound.step_falloff_depth_m = v;
            }
            if let Some(v) = field_f32(object, "half_res_roughness")? {
                bound.half_res_roughness = v;
            }
            if let Some(v) = field_f32(object, "block_roughness")? {
                bound.block_roughness = v;
            }
            Ok(Some(bound.sanitized()))
        }
        _ => Err(bad("bad_bound", "`bound` must be a string or an object")),
    }
}

/// `render.set_ssr_cost { probe?, bound? }`: turns the cost probe on or off and sets the cost
/// bound (`"wet"`, `"off"`, or an object of fields over the wet preset) for every renderer from
/// the next frame. Not persisted; `REGIA_SSSR_COST_BOUND` still wins while set. Returns
/// `{ before, after }`.
pub(crate) fn set_ssr_cost(params: &Value) -> Result<Value, RpcError> {
    let probe = params
        .get("probe")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| bad("bad_probe", "`probe` must be a boolean"))
        })
        .transpose()?;
    let bound = params.get("bound").map(parse_bound).transpose()?;
    if probe.is_none() && bound.is_none() {
        return Err(bad("nothing_to_set", "give `probe`, `bound` or both"));
    }
    let before = knobs_json();
    if let Some(on) = probe {
        regia_render::set_sssr_cost_probe(on);
    }
    if let Some(bound) = bound {
        regia_render::set_sssr_cost_bound(bound);
    }
    Ok(json!({ "before": before, "after": knobs_json() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn stats_need_a_renderer() {
        let mut world = World::new();
        assert!(ssr_stats(&mut world).is_err());
    }

    #[test]
    fn bound_names_and_objects_parse() -> Result<(), RpcError> {
        assert_eq!(parse_bound(&json!("wet"))?, Some(SssrCostBound::WET));
        assert_eq!(parse_bound(&json!("off"))?, None);
        assert_eq!(parse_bound(&Value::Null)?, None);
        let custom = parse_bound(&json!({ "tile_ray_budget": 8, "variance_rays": true }))?;
        assert_eq!(
            custom,
            Some(SssrCostBound {
                tile_ray_budget: 8,
                variance_rays: true,
                ..SssrCostBound::WET
            })
        );
        for bad_value in [
            json!("fast"),
            json!(3),
            json!({ "tile_ray_budget": 65 }),
            json!({ "min_steps": -1 }),
            json!({ "half_res_roughness": "low" }),
            json!({ "steps": 3 }),
            json!({}),
        ] {
            let error = parse_bound(&bad_value)
                .err()
                .ok_or_else(|| RpcError::internal("accepted"))?;
            assert_eq!(reason(&error), "bad_bound", "{bad_value}");
        }
        Ok(())
    }

    /// The knobs are process-wide: one test sets and restores them.
    #[test]
    fn set_reads_back_and_restores() -> Result<(), RpcError> {
        let error = set_ssr_cost(&json!({}))
            .err()
            .ok_or_else(|| RpcError::internal("accepted"))?;
        assert_eq!(reason(&error), "nothing_to_set");
        let error = set_ssr_cost(&json!({ "probe": 1 }))
            .err()
            .ok_or_else(|| RpcError::internal("accepted"))?;
        assert_eq!(reason(&error), "bad_probe");
        let out = set_ssr_cost(&json!({ "probe": true, "bound": "wet" }))?;
        assert_eq!(out["after"]["probe"], json!(true));
        if regia_render::sssr_cost_bound_env().is_none() {
            assert_eq!(out["after"]["bound"]["tile_ray_budget"], json!(16));
            assert_eq!(out["after"]["bound"]["is_wet_preset"], json!(true));
        }
        let out = set_ssr_cost(&json!({ "probe": false, "bound": "off" }))?;
        assert_eq!(out["before"]["probe"], json!(true));
        assert_eq!(out["after"]["probe"], json!(false));
        if regia_render::sssr_cost_bound_env().is_none() {
            assert_eq!(out["after"]["bound"], Value::Null);
        }
        Ok(())
    }
}
