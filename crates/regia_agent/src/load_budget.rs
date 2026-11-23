// SPDX-License-Identifier: MPL-2.0
//! `render.load_budget` / `render.set_load_budget`: the runtime renderer's per-frame load budget
//! (ADR-7884, `book/agents/instant-load.md` "Load budget"). With `late: "defer"`, meshes past
//! `upload_bytes_per_frame` are uploaded on later frames (nearest first) and material-graph sets
//! past `new_pipelines_per_frame` build on worker threads; their draws are skipped until ready.
//! The default (`block`, no caps) changes nothing.

use regia_app::prelude::*;
use regia_render::{FrameLoadBudget, LateDraw, LoadBudgetReport, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn renderer_missing() -> RpcError {
    RpcError::new(
        crate::protocol::NOT_FOUND,
        "load_budget_renderer_missing",
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin with a window (the budget lives on the runtime renderer)",
    )
}

fn bad_param(message: &str) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "load_budget_bad_param",
        message,
        "upload_bytes_per_frame and new_pipelines_per_frame are positive integers or null; late is \"block\" or \"defer\"",
    )
}

fn budget_json(budget: FrameLoadBudget) -> Value {
    json!({
        "upload_bytes_per_frame": budget.upload_bytes_per_frame,
        "new_pipelines_per_frame": budget.new_pipelines_per_frame,
        "late": budget.late.name(),
        "active": budget.is_active(),
    })
}

fn report_json(report: LoadBudgetReport) -> Value {
    json!({
        "deferred_meshes": report.deferred_meshes,
        "deferred_bytes": report.deferred_bytes,
        "deferred_draws": report.deferred_draws,
        "async_pipelines_pending": report.async_pipelines_pending,
        "admitted_meshes": report.admitted_meshes,
        "admitted_bytes": report.admitted_bytes,
        "async_graph_sets": report.async_graph_sets,
        "bursts": report.bursts,
    })
}

/// `render.load_budget`: `{ budget: { upload_bytes_per_frame, new_pipelines_per_frame, late,
/// active }, last_frame: { deferred_meshes, deferred_bytes, deferred_draws,
/// async_pipelines_pending, admitted_meshes, admitted_bytes, async_graph_sets, bursts } }`.
pub(crate) fn get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let renderer = world
        .get_resource::<RuntimeRenderer>()
        .ok_or_else(renderer_missing)?;
    Ok(json!({
        "budget": budget_json(renderer.0.load_budget()),
        "last_frame": report_json(renderer.0.load_budget_report()),
    }))
}

/// A cap parameter: absent keeps `current`, null clears it, a positive integer sets it.
fn cap<T: TryFrom<u64>>(params: &Value, name: &str, current: Option<T>) -> Result<Option<T>, RpcError> {
    match params.get(name) {
        None => Ok(current),
        Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|&n| n > 0)
            .and_then(|n| T::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| bad_param(&format!("{name} must be a positive integer or null"))),
    }
}

/// The budget `params` asks for on top of `current`: `recommended: true` starts from
/// [`FrameLoadBudget::RECOMMENDED_60FPS`], `reset: true` from the default; the named fields then
/// apply.
fn parse(params: &Value, current: FrameLoadBudget) -> Result<FrameLoadBudget, RpcError> {
    let flag = |name: &str| -> Result<bool, RpcError> {
        match params.get(name) {
            None => Ok(false),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| bad_param(&format!("{name} must be a boolean"))),
        }
    };
    let mut budget = match (flag("recommended")?, flag("reset")?) {
        (true, true) => return Err(bad_param("recommended and reset cannot both be true")),
        (true, false) => FrameLoadBudget::RECOMMENDED_60FPS,
        (false, true) => FrameLoadBudget::default(),
        (false, false) => current,
    };
    budget.upload_bytes_per_frame = cap(params, "upload_bytes_per_frame", budget.upload_bytes_per_frame)?;
    budget.new_pipelines_per_frame = cap(params, "new_pipelines_per_frame", budget.new_pipelines_per_frame)?;
    if let Some(late) = params.get("late") {
        budget.late = late
            .as_str()
            .and_then(LateDraw::from_name)
            .ok_or_else(|| bad_param("late must be \"block\" or \"defer\""))?;
    }
    Ok(budget)
}

/// `render.set_load_budget { upload_bytes_per_frame?, new_pipelines_per_frame?, late?,
/// recommended?, reset? }`: `{ before, after }`, each a `budget` object as in
/// `render.load_budget`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(renderer_missing)?;
    let before = renderer.0.load_budget();
    let after = parse(params, before)?;
    renderer.0.set_load_budget(after);
    Ok(json!({ "before": budget_json(before), "after": budget_json(renderer.0.load_budget()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_budget_needs_the_runtime_renderer() {
        let mut world = World::new();
        let err = get(&mut world, &json!({})).expect_err("no renderer");
        assert_eq!(err.data.expect("data")["code"], "load_budget_renderer_missing");
        assert!(set(&mut world, &json!({ "late": "defer" })).is_err());
    }

    #[test]
    fn load_budget_params_are_checked() {
        let base = FrameLoadBudget::default();
        for bad in [
            json!({ "late": "later" }),
            json!({ "upload_bytes_per_frame": 0 }),
            json!({ "upload_bytes_per_frame": -1 }),
            json!({ "new_pipelines_per_frame": 1u64 << 40 }),
            json!({ "new_pipelines_per_frame": "8" }),
            json!({ "recommended": true, "reset": true }),
            json!({ "recommended": "yes" }),
        ] {
            let err = parse(&bad, base).expect_err("refused");
            assert_eq!(err.data.expect("data")["code"], "load_budget_bad_param", "{bad}");
        }
        let recommended = parse(&json!({ "recommended": true }), base).expect("recommended");
        assert_eq!(recommended, FrameLoadBudget::RECOMMENDED_60FPS);
        let cleared = parse(&json!({ "upload_bytes_per_frame": null }), recommended).expect("null clears");
        assert_eq!(cleared.upload_bytes_per_frame, None);
        assert_eq!(cleared.new_pipelines_per_frame, Some(8), "absent keeps");
        assert_eq!(
            parse(&json!({ "reset": true }), recommended).expect("reset"),
            base
        );
    }

    #[test]
    fn load_budget_methods_round_trip_through_the_protocol() {
        let mut world = World::new();
        world.insert_resource(regia_app::Time::default());
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let mut call = |method, params| {
            crate::dispatch::call_in_process(&mut world, method, params)
                .expect("synchronous method")
                .expect("protocol call")
        };
        let got = call("render.load_budget", json!({}));
        assert_eq!(got["budget"]["late"], "block", "{got}");
        assert_eq!(got["budget"]["active"], false);
        assert_eq!(got["last_frame"]["deferred_draws"], 0);
        let set = call(
            "render.set_load_budget",
            json!({ "upload_bytes_per_frame": 67_108_864u64, "new_pipelines_per_frame": 8, "late": "defer" }),
        );
        assert_eq!(set["before"]["late"], "block");
        assert_eq!(set["after"]["late"], "defer");
        assert_eq!(set["after"]["upload_bytes_per_frame"], 67_108_864u64);
        assert_eq!(set["after"]["active"], true);
        let got = call("render.load_budget", json!({}));
        assert_eq!(got["budget"]["new_pipelines_per_frame"], 8);
        let reset = call("render.set_load_budget", json!({ "reset": true }));
        assert_eq!(reset["after"]["active"], false);
    }
}
