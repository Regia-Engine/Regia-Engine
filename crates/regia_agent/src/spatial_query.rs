// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for spatial queries (`docs/specs/33-spatial-queries.md` § 5,
//! `book/agents/spatial-queries.md`): `ai.query_list`, `ai.query_run`, `ai.query_submit`,
//! `ai.query_poll`, `ai.query_abort`, `ai.query_stats` and `ai.query_probe`.
//!
//! Every method answers `ai_query_unavailable` when `regia_ai::SpatialQueryPlugin` is not installed.
//! A missing parameter is not an error: `ai.query_run` returns `status: "missing_param"`.

use std::collections::BTreeMap;

use regia_ai::spatial_query::{
    ParamValue, Pick, QueryStatus, RequestId, SpatialQueryManager, SpatialQueryProbe, SpatialQueryProbeState,
    SpatialQueryRequest, SpatialQueryResult, TemplateError,
};
use regia_app::prelude::*;
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// The default (and largest) number of items `ai.query_run` returns.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 20_000;

/// Item caps from `ai.query_submit` (spec 33: the same params as `ai.query_run`, including
/// `limit`). `ai.query_poll` applies the stored cap when that call omits `limit`.
#[derive(Resource, Default)]
struct SubmitItemLimits {
    /// Request id → (owner bits, cap).
    by_request: BTreeMap<u64, (u64, usize)>,
}

fn remember_limit(world: &mut World, id: u64, owner: u64, cap: usize) {
    if world.get_resource::<SubmitItemLimits>().is_none() {
        world.insert_resource(SubmitItemLimits::default());
    }
    world
        .resource_mut::<SubmitItemLimits>()
        .by_request
        .insert(id, (owner, cap));
}

fn forget_limit(world: &mut World, id: u64) {
    if let Some(mut limits) = world.get_resource_mut::<SubmitItemLimits>() {
        limits.by_request.remove(&id);
    }
}

fn forget_owner_limits(world: &mut World, owner: u64) {
    // `abort_owner` leaves finished results in place. Their submit caps stay until poll.
    let gone: Vec<u64> = {
        let Some(manager) = world.get_resource::<SpatialQueryManager>() else {
            return;
        };
        let Some(limits) = world.get_resource::<SubmitItemLimits>() else {
            return;
        };
        limits
            .by_request
            .iter()
            .filter_map(|(id, (bits, _))| {
                if *bits != owner {
                    return None;
                }
                let request = RequestId(*id);
                if manager.is_active(request) || manager.is_finished(request) {
                    None
                } else {
                    Some(*id)
                }
            })
            .collect()
    };
    if let Some(mut limits) = world.get_resource_mut::<SubmitItemLimits>() {
        for id in gone {
            limits.by_request.remove(&id);
        }
    }
}

/// Drops the caps of requests the manager no longer holds (ADR-5597). It ends requests on its own:
/// a dead owner's results after the grace ticks, `forget_owner` from a tree driver, a result game
/// code polled. Their caps used to stay for the whole session.
fn forget_dropped_limits(world: &mut World) {
    let Some(manager) = world.get_resource::<SpatialQueryManager>() else {
        return;
    };
    let dropped: Vec<u64> = world
        .get_resource::<SubmitItemLimits>()
        .map(|limits| {
            limits
                .by_request
                .keys()
                .copied()
                .filter(|id| !manager.is_active(RequestId(*id)) && !manager.is_finished(RequestId(*id)))
                .collect()
        })
        .unwrap_or_default();
    if let Some(mut limits) = world.get_resource_mut::<SubmitItemLimits>() {
        for id in dropped {
            limits.by_request.remove(&id);
        }
    }
}

fn stored_limit(world: &World, id: u64) -> Option<usize> {
    world
        .get_resource::<SubmitItemLimits>()
        .and_then(|limits| limits.by_request.get(&id).map(|(_, cap)| *cap))
}

fn unavailable() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "ai_query_unavailable",
        "spatial queries are not installed in this app",
        "add regia_ai::SpatialQueryPlugin",
    )
}

fn bad_param(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "sq_bad_param",
        message,
        "see agents/spatial-queries in the book for the parameters",
    )
}

fn coded(code: &str, message: impl Into<String>) -> RpcError {
    let status = match code {
        "sq_unknown_template" | "sq_unknown_owner" | "sq_unknown_request" => NOT_FOUND,
        _ => INVALID_PARAMS,
    };
    RpcError::new(status, code, message, "see agents/spatial-queries in the book")
}

fn template_error(error: &TemplateError) -> RpcError {
    let mut err = coded(error.code(), error.to_string());
    if let TemplateError::Load { error, .. } = error
        && let Some(Value::Object(data)) = err.data.as_mut()
    {
        let (line, col) = error.position();
        data.insert("line".into(), json!(line));
        data.insert("column".into(), json!(col));
    }
    err
}

fn require_manager(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<SpatialQueryManager>() {
        Ok(())
    } else {
        Err(unavailable())
    }
}

fn entity_param(world: &World, params: &Value, field: &str) -> Result<Entity, RpcError> {
    let bits = params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_param(format!("`{field}` (an entity id) is required")))?;
    Entity::try_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| coded("sq_unknown_owner", format!("entity {bits} does not exist")))
}

fn param_value(name: &str, value: &Value) -> Result<ParamValue, RpcError> {
    match value {
        Value::Bool(b) => Ok(ParamValue::Bool(*b)),
        Value::Number(n) if n.is_i64() => Ok(ParamValue::Int(n.as_i64().unwrap_or_default())),
        Value::Number(n) => n
            .as_f64()
            .map(|v| ParamValue::Float(v as f32))
            .ok_or_else(|| bad_param(format!("parameter `{name}` is not a number"))),
        Value::String(s) => Ok(ParamValue::Name(s.clone())),
        other => Err(bad_param(format!(
            "parameter `{name}` must be a bool, number or string, not {other}"
        ))),
    }
}

fn pick_param(value: Option<&Value>) -> Result<Option<Pick>, RpcError> {
    let band = |v: &Value| -> Result<f32, RpcError> {
        let band = v
            .as_f64()
            .or_else(|| v.get("band").and_then(Value::as_f64))
            .ok_or_else(|| bad_param("`pick.sample` takes a band in 0..=1"))?;
        if (0.0..=1.0).contains(&band) {
            Ok(band as f32)
        } else {
            Err(bad_param(format!("sample band {band} is outside 0..=1")))
        }
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "best" => Ok(Some(Pick::Best)),
            "all" => Ok(Some(Pick::All)),
            "sample" => Ok(Some(Pick::Sample { band: 0.1 })),
            other => Err(bad_param(format!(
                "pick `{other}` is not one of best, sample, all"
            ))),
        },
        Some(Value::Object(map)) => match map.get("sample") {
            Some(v) if map.len() == 1 => Ok(Some(Pick::Sample { band: band(v)? })),
            _ => Err(bad_param("an object pick is `{ \"sample\": band }`")),
        },
        Some(other) => Err(bad_param(format!(
            "pick must be a string or an object, not {other}"
        ))),
    }
}

/// Reads and checks a run/submit request: template (resolved now, so load errors answer at once),
/// owner, params, pick, seed, keep_failed.
fn request(world: &mut World, params: &Value) -> Result<SpatialQueryRequest, RpcError> {
    require_manager(world)?;
    let template = params
        .get("template")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_param("`template` (a name or path) is required"))?
        .to_string();
    let owner = entity_param(world, params, "owner")?;
    let mut overrides = BTreeMap::new();
    match params.get("params") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                overrides.insert(name.clone(), param_value(name, value)?);
            }
        }
        Some(other) => return Err(bad_param(format!("`params` must be an object, not {other}"))),
    }
    let pick = pick_param(params.get("pick"))?;
    let seed = match params.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| bad_param("`seed` must be a non-negative integer"))?,
        ),
    };
    let keep_failed = match params.get("keep_failed") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad_param("`keep_failed` must be a boolean"))?,
    };
    world
        .resource_mut::<SpatialQueryManager>()
        .template(&template)
        .map_err(|e| template_error(&e))?;
    Ok(SpatialQueryRequest {
        template,
        owner,
        params: overrides,
        pick,
        seed,
        keep_failed,
    })
}

fn limit(params: &Value) -> Result<usize, RpcError> {
    match params.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok((n as usize).min(MAX_LIMIT)),
            _ => Err(bad_param("`limit` must be an integer >= 1")),
        },
    }
}

/// The JSON form of a result (shared by run, poll and probe). A failure with an error code
/// (missing system, too many items, no navmesh, bad parameter) is an error.
fn result_json(result: &SpatialQueryResult, limit: usize) -> Result<Value, RpcError> {
    if let Some(code) = result.code {
        return Err(coded(code, result.message.clone().unwrap_or_default()));
    }
    let items: Vec<Value> = result
        .items
        .iter()
        .take(limit)
        .map(|item| {
            let mut contributions = Map::new();
            for (id, value) in &item.contributions {
                contributions.insert(id.clone(), json!(value));
            }
            json!({
                "position": [item.position.x, item.position.y, item.position.z],
                "entity": item.entity.map_or(Value::Null, |e| json!(e.to_bits())),
                "score": item.score,
                "passed": item.passed,
                "failed_test": item.failed_test,
                "failed_reason": item.failed_reason,
                "generation_index": item.generation_index,
                "contributions": contributions,
            })
        })
        .collect();
    let tests: Vec<Value> = result
        .tests
        .iter()
        .map(|t| json!({ "id": t.id, "kind": t.kind, "in": t.input, "out": t.output }))
        .collect();
    let mut out = json!({
        "status": result.status.name(),
        "items": items,
        "item_count": result.item_count,
        "tests": tests,
        "units": result.units,
    });
    if let (Some(message), Value::Object(map)) = (&result.message, &mut out) {
        map.insert("message".into(), json!(message));
    }
    Ok(out)
}

/// `ai.query_list`: the loaded templates (name, path, item type, tests, declared parameters), the
/// anchor provider names and the tag matcher name.
pub fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_manager(world)?;
    let manager = world.resource::<SpatialQueryManager>();
    let templates: Vec<Value> = manager
        .templates()
        .map(|(name, template)| {
            let asset = &template.asset;
            let mut declared = Map::new();
            for (param, value) in &asset.params {
                declared.insert(
                    param.clone(),
                    match value {
                        ParamValue::Bool(b) => json!(b),
                        ParamValue::Int(i) => json!(i),
                        ParamValue::Float(f) => json!(f),
                        ParamValue::Name(s) => json!(s),
                    },
                );
            }
            json!({
                "name": name,
                "path": template.path,
                "item_type": asset.item_type.name(),
                "generator": asset.generator.kind(),
                "tests": asset.tests.iter().map(|t| json!({
                    "id": t.id(),
                    "kind": t.kind(),
                    "purpose": format!("{:?}", t.purpose()).to_lowercase(),
                })).collect::<Vec<_>>(),
                "params": declared,
                "server_only": asset.server_only,
            })
        })
        .collect();
    Ok(json!({
        "templates": templates,
        "providers": manager.provider_names(),
        "matcher": manager.tag_matcher_name(),
    }))
}

/// `ai.query_run`: runs a request at once (`run_instant`) and returns at most `limit` items.
pub fn run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let req = request(world, params)?;
    let limit = limit(params)?;
    let result =
        world.resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.run_instant(world, req));
    result_json(&result, limit)
}

/// `ai.query_submit`: queues a request; it advances on fixed ticks. `limit` is remembered and
/// applied by a later `ai.query_poll` that does not pass its own `limit` (spec 33).
pub fn submit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let req = request(world, params)?;
    let cap = limit(params)?;
    let owner = req.owner.to_bits();
    let id = world.resource_mut::<SpatialQueryManager>().submit(req);
    forget_dropped_limits(world);
    remember_limit(world, id.0, owner, cap);
    Ok(json!({ "request": id.0 }))
}

fn request_id(params: &Value) -> Result<RequestId, RpcError> {
    crate::params::opt_u64(params, "request")?
        .map(RequestId)
        .ok_or_else(|| bad_param("`request` (an id from ai.query_submit) is required"))
}

/// `ai.query_poll`: `processing` while the request runs; the result (taken: a second poll answers
/// `sq_unknown_request`) once it finished.
pub fn poll(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_manager(world)?;
    let id = request_id(params)?;
    let explicit = !matches!(params.get("limit"), None | Some(Value::Null));
    let cap = if explicit {
        limit(params)?
    } else {
        stored_limit(world, id.0).unwrap_or(DEFAULT_LIMIT)
    };
    let mut manager = world.resource_mut::<SpatialQueryManager>();
    if manager.is_active(id) {
        return Ok(json!({
            "status": QueryStatus::Processing.name(),
            "request": id.0,
            "units": manager.units_spent(id).unwrap_or(0),
        }));
    }
    let outcome = manager.poll(id);
    forget_limit(world, id.0);
    match outcome {
        Some(result) => result_json(&result, cap),
        None => Err(coded(
            "sq_unknown_request",
            format!("no request {} is processing or waiting to be polled", id.0),
        )),
    }
}

/// `ai.query_abort`: aborts one request (`request`) or every request of an owner (`owner`).
pub fn abort(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_manager(world)?;
    if params.get("request").is_some() {
        let id = request_id(params)?;
        let (aborted, still_held) = {
            let mut manager = world.resource_mut::<SpatialQueryManager>();
            let aborted = manager.abort(id);
            (aborted, manager.is_active(id) || manager.is_finished(id))
        };
        // A finished result stays pollable. Drop the cap only when the request is gone.
        if !still_held {
            forget_limit(world, id.0);
        }
        return Ok(json!({ "aborted": usize::from(aborted) }));
    }
    let Some(bits) = crate::params::opt_u64(params, "owner")? else {
        return Err(bad_param("give `request` or `owner`"));
    };
    let owner = Entity::try_from_bits(bits)
        .ok_or_else(|| coded("sq_unknown_owner", format!("{bits} is not an entity id")))?;
    let aborted = world.resource_mut::<SpatialQueryManager>().abort_owner(owner);
    forget_owner_limits(world, bits);
    Ok(json!({ "aborted": aborted }))
}

/// `ai.query_stats`: request counters and recent warnings.
pub fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_manager(world)?;
    let stats = world.resource::<SpatialQueryManager>().stats();
    Ok(json!({
        "active": stats.active,
        "submitted": stats.submitted,
        "completed": stats.completed,
        "aborted": stats.aborted,
        "failed": stats.failed,
        "units_last_tick": stats.units_last_tick,
        "warnings": stats.warnings,
    }))
}

/// `ai.query_probe`: a probe entity's last result (as `ai.query_run`) and its run count.
pub fn probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_manager(world)?;
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_param("`entity` (a probe entity id) is required"))?;
    let entity = Entity::try_from_bits(bits)
        .filter(|e| world.get::<SpatialQueryProbe>(*e).is_some())
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {bits} has no SpatialQueryProbe"),
                "add a regia_ai::SpatialQueryProbe component to the entity",
            )
        })?;
    let limit = limit(params)?;
    let state = world.get::<SpatialQueryProbeState>(entity);
    let runs = state.map_or(0, |s| s.runs);
    let mut out = match state.and_then(|s| s.result.as_ref()) {
        Some(result) => result_json(result, limit)?,
        None => json!({ "status": QueryStatus::Processing.name() }),
    };
    if let Value::Object(map) = &mut out {
        map.insert("runs".into(), json!(runs));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ai::spatial_query::SpatialQueryPlugin;
    use regia_core::math::{Affine3A, Vec3};
    use regia_scene::{GlobalTransform, Transform};

    const RING: &str = r#"(format_version: 1, name: "ring", item_type: Point,
        params: { "r": Float(2.0) },
        generator: Ring(radius: ParamF32("r"), count: 8),
        tests: [Distance(id: "ahead", purpose: Score, to: Point((0.0, 0.0, -9.0)), score: (weight: -1.0))])"#;

    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SpatialQueryPlugin));
        app.world_mut()
            .resource_mut::<SpatialQueryManager>()
            .register_template_text(RING)
            .expect("loads");
        let owner = app
            .world_mut()
            .spawn((Transform::default(), GlobalTransform(Affine3A::IDENTITY)))
            .id();
        (app, owner)
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn query_run_is_identical_for_the_same_seed() {
        let (mut app, owner) = app();
        let params =
            json!({ "template": "ring", "owner": owner.to_bits(), "pick": { "sample": 0.5 }, "seed": 9 });
        let a = run(app.world_mut(), &params).expect("runs");
        let b = run(app.world_mut(), &params).expect("runs");
        assert_eq!(a, b);
        assert_eq!(a["status"], "success");
        assert_eq!(a["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(a["tests"][0]["id"], "ahead");
        assert_eq!(a["tests"][0]["in"], 8);
    }

    #[test]
    fn query_run_all_honours_limit_and_params() {
        let (mut app, owner) = app();
        let result = run(
            app.world_mut(),
            &json!({ "template": "ring", "owner": owner.to_bits(), "pick": "all", "limit": 3, "params": { "r": 5 } }),
        )
        .expect("runs");
        assert_eq!(result["item_count"], 8);
        assert_eq!(result["items"].as_array().map(Vec::len), Some(3));
        // The best item is the ring point nearest (0, 0, -9): (0, 0, -5).
        assert!((result["items"][0]["position"][2].as_f64().unwrap_or(0.0) + 5.0).abs() < 1e-4);
    }

    #[test]
    fn missing_param_is_a_status_not_an_error() {
        let (mut app, owner) = app();
        app.world_mut()
            .resource_mut::<SpatialQueryManager>()
            .register_template_text(
                r#"(format_version: 1, name: "needs", item_type: Point, generator: Ring(radius: ParamF32("x"), count: 2))"#,
            )
            .expect("loads");
        let result = run(
            app.world_mut(),
            &json!({ "template": "needs", "owner": owner.to_bits() }),
        )
        .expect("answers");
        assert_eq!(result["status"], "missing_param");
        assert!(result["message"].as_str().is_some_and(|m| m.contains("`x`")));
    }

    #[test]
    fn every_error_code() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        for method in [list, run, submit, poll, abort, stats, probe] {
            let err = method(bare.world_mut(), &json!({})).expect_err("no plugin");
            assert_eq!(code(&err), "ai_query_unavailable");
        }

        let (mut app, owner) = app();
        let o = owner.to_bits();
        let err = run(app.world_mut(), &json!({ "template": "nope", "owner": o })).expect_err("unknown");
        assert_eq!(code(&err), "sq_unknown_template");
        let err = run(
            app.world_mut(),
            &json!({ "template": "ring", "owner": 99_999_999u64 }),
        )
        .expect_err("owner");
        assert_eq!(code(&err), "sq_unknown_owner");
        let err = run(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "pick": "worst" }),
        )
        .expect_err("pick");
        assert_eq!(code(&err), "sq_bad_param");
        let err = run(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "params": { "r": [1] } }),
        )
        .expect_err("param");
        assert_eq!(code(&err), "sq_bad_param");
        let err = poll(app.world_mut(), &json!({ "request": 12345 })).expect_err("request");
        assert_eq!(code(&err), "sq_unknown_request");

        // Load errors from files carry the code and the position.
        let dir = regia_test_utils::unique_temp_dir("agent-sq");
        std::fs::create_dir_all(&dir).expect("dir");
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).expect("write");
        write(
            "bad.ron",
            "(format_version: 1, name: \"b\", item_type: Point,\n  oops: 1, generator: Ring(radius: 1.0, count: 1))",
        );
        write(
            "mixed.ron",
            "(format_version: 1, name: \"m\", item_type: Entity, generator: Ring(radius: 1.0, count: 1))",
        );
        write(
            "threshold.ron",
            "(format_version: 1, name: \"t\", item_type: Point, generator: Ring(radius: 1.0, count: 1),\n  tests: [Distance(id: \"d\", purpose: Score, to: Asker, score: (clamp_min: FilterBound))])",
        );
        write(
            "cast.ron",
            "(format_version: 1, name: \"c\", item_type: Point, generator: Ring(radius: 1.0, count: 1),\n  tests: [Cast(id: \"c\", purpose: Filter, to: Asker, want_hit: false)])",
        );
        write(
            "lattice.ron",
            "(format_version: 1, name: \"l\", item_type: Point, generator: Lattice(half_size: 500.0, spacing: 1.0))",
        );
        write(
            "reach.ron",
            "(format_version: 1, name: \"r\", item_type: Point, generator: ReachLattice(half_size: 2.0, spacing: 1.0))",
        );
        app.world_mut().resource_mut::<SpatialQueryManager>().asset_root = dir.clone();
        for (file, want) in [
            ("bad.ron", "sq_unreadable"),
            ("mixed.ron", "sq_item_type_mismatch"),
            ("threshold.ron", "sq_no_threshold"),
            ("cast.ron", "sq_missing_system"),
            ("lattice.ron", "sq_too_many_items"),
            ("reach.ron", "sq_no_navmesh"),
        ] {
            let err = run(app.world_mut(), &json!({ "template": file, "owner": o })).expect_err(file);
            assert_eq!(code(&err), want, "{file}: {}", err.message);
        }
        let err = run(app.world_mut(), &json!({ "template": "bad.ron", "owner": o })).expect_err("bad");
        assert_eq!(err.data.as_ref().and_then(|d| d.get("line")), Some(&json!(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-agent (ADR-5597): a cap stayed in `SubmitItemLimits` when the manager dropped the
    /// request on its own (here `forget_owner`, as a tree driver does when its owner despawns).
    #[test]
    fn caps_of_requests_the_manager_dropped_are_forgotten() {
        let (mut app, owner) = app();
        let o = owner.to_bits();
        let first = submit(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "limit": 3 }),
        )
        .expect("submits");
        let first = first["request"].as_u64().expect("an id");
        app.world_mut()
            .resource_mut::<SpatialQueryManager>()
            .forget_owner(owner);
        let second = submit(app.world_mut(), &json!({ "template": "ring", "owner": o })).expect("submits");
        let second = second["request"].as_u64().expect("an id");
        let kept: Vec<u64> = app
            .world()
            .resource::<SubmitItemLimits>()
            .by_request
            .keys()
            .copied()
            .collect();
        assert_eq!(kept, vec![second], "the dropped request {first} keeps no cap");
    }

    #[test]
    fn submit_poll_abort_and_stats() {
        let (mut app, owner) = app();
        let o = owner.to_bits();
        let submitted = submit(app.world_mut(), &json!({ "template": "ring", "owner": o })).expect("submits");
        let request = submitted["request"].as_u64().expect("an id");
        let polled = poll(app.world_mut(), &json!({ "request": request })).expect("polls");
        assert_eq!(polled["status"], "processing");
        regia_ai::spatial_query::tick_spatial_queries(app.world_mut());
        let polled = poll(app.world_mut(), &json!({ "request": request })).expect("polls");
        assert_eq!(polled["status"], "success");

        let second = submit(app.world_mut(), &json!({ "template": "ring", "owner": o })).expect("submits");
        let aborted = abort(app.world_mut(), &json!({ "request": second["request"] })).expect("aborts");
        assert_eq!(aborted["aborted"], 1);
        submit(app.world_mut(), &json!({ "template": "ring", "owner": o })).expect("submits");
        submit(app.world_mut(), &json!({ "template": "ring", "owner": o })).expect("submits");
        let aborted = abort(app.world_mut(), &json!({ "owner": o })).expect("aborts");
        assert_eq!(aborted["aborted"], 2);

        let s = stats(app.world_mut(), &json!({})).expect("stats");
        assert_eq!(s["submitted"], 4);
        assert_eq!(s["completed"], 1);
        assert_eq!(s["aborted"], 3);
        assert_eq!(s["active"], 0);

        let capped = submit(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "pick": "all", "limit": 1 }),
        )
        .expect("submits with a limit");
        let capped_id = capped["request"].as_u64().expect("an id");
        regia_ai::spatial_query::tick_spatial_queries(app.world_mut());
        let polled = poll(app.world_mut(), &json!({ "request": capped_id })).expect("polls");
        assert_eq!(polled["status"], "success");
        assert_eq!(polled["item_count"], 8);
        assert_eq!(polled["items"].as_array().map(Vec::len), Some(1));

        let listed = list(app.world_mut(), &json!({})).expect("lists");
        assert_eq!(listed["templates"][0]["name"], "ring");
        assert_eq!(listed["templates"][0]["tests"][0]["purpose"], "score");
        assert_eq!(listed["templates"][0]["params"]["r"], 2.0);
    }

    #[test]
    fn abort_of_a_finished_query_keeps_the_submit_limit() {
        let (mut app, owner) = app();
        let o = owner.to_bits();
        let submitted = submit(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "pick": "all", "limit": 1 }),
        )
        .expect("submits");
        let request = submitted["request"].as_u64().expect("id");
        regia_ai::spatial_query::tick_spatial_queries(app.world_mut());
        let aborted = abort(app.world_mut(), &json!({ "request": request })).expect("abort");
        assert_eq!(aborted["aborted"], 0, "a finished request is not aborted");
        let polled = poll(app.world_mut(), &json!({ "request": request })).expect("poll");
        assert_eq!(polled["status"], "success");
        assert_eq!(polled["item_count"], 8);
        assert_eq!(
            polled["items"].as_array().map(Vec::len),
            Some(1),
            "abort must keep the submit cap of a result that is still pollable: {polled}"
        );

        let submitted = submit(
            app.world_mut(),
            &json!({ "template": "ring", "owner": o, "pick": "all", "limit": 1 }),
        )
        .expect("submits");
        let request = submitted["request"].as_u64().expect("id");
        regia_ai::spatial_query::tick_spatial_queries(app.world_mut());
        let aborted = abort(app.world_mut(), &json!({ "owner": o })).expect("abort owner");
        assert_eq!(aborted["aborted"], 0);
        let polled = poll(app.world_mut(), &json!({ "request": request })).expect("poll");
        assert_eq!(
            polled["items"].as_array().map(Vec::len),
            Some(1),
            "abort by owner must keep the submit cap: {polled}"
        );
    }

    #[test]
    fn query_probe_reports_the_last_result() {
        let (mut app, _owner) = app();
        let plain = app.world_mut().spawn_empty().id();
        let err = probe(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("not a probe");
        assert_eq!(code(&err), "F0003");
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::ONE),
                GlobalTransform(Affine3A::from_translation(Vec3::ONE)),
                SpatialQueryProbe::new("ring"),
            ))
            .id();
        let before = probe(app.world_mut(), &json!({ "entity": entity.to_bits() })).expect("answers");
        assert_eq!(before["status"], "processing");
        assert_eq!(before["runs"], 0);
        // Run it the way the probe system does and store the state.
        let result = app
            .world_mut()
            .resource_scope(|world, mut m: Mut<SpatialQueryManager>| {
                m.run_instant(world, SpatialQueryRequest::new("ring", entity))
            });
        app.world_mut().entity_mut(entity).insert(SpatialQueryProbeState {
            last_position: Some(Vec3::ONE),
            last_probe: None,
            result: Some(result),
            runs: 1,
        });
        let after = probe(app.world_mut(), &json!({ "entity": entity.to_bits() })).expect("answers");
        assert_eq!(after["status"], "success");
        assert_eq!(after["runs"], 1);
    }
}
