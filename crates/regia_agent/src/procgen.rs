// SPDX-License-Identifier: MPL-2.0
//! `procgen.*`: procedural generators and graphs (specs/62 § 9, book `agents/procgen.md`, guide
//! topic `procgen`).
//!
//! Errors answer `F0002` (invalid params) or `F0003` (unknown entity or path) with `data.reason`
//! set to a lowercase code listed on the book page; compile problems arrive in `data.errors`.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_procgen::data::{AttrColumn, DataItem, Payload};
use regia_procgen::graph::{GraphError, GraphPin, ParamDecl, ParamSource, resolve_graph};
use regia_procgen::world::{
    ProcGenRegistry, ProcGenRunState, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphOverlay,
    ProcGraphSource, ProcOutput, read_graph_text,
};
use regia_procgen::{AttrType, DataKind, Edge, KindSet, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::{EntityGuid, Name};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most `graph_edit` ops per call.
pub(crate) const MAX_OPS: usize = 512;
/// Most rows `procgen.inspect` returns.
pub(crate) const MAX_ROWS: usize = 10_000;

fn reason(code: i64, regia_code: &str, why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": why, "hint": "see book/agents/procgen.md" })),
    }
}

fn bad(why: &str, message: impl Into<String>) -> RpcError {
    reason(INVALID_PARAMS, "F0002", why, message)
}

/// A graph path that is absolute, prefixed, holds `..`, or leads out of the project (ADR-1816).
fn bad_path(e: regia_core::project_path::ProjectPathError) -> RpcError {
    bad(
        "procgen_bad_path",
        format!("`path` must be project-relative ({}): {e}", e.code()),
    )
}

fn missing(why: &str, message: impl Into<String>) -> RpcError {
    reason(NOT_FOUND, "F0003", why, message)
}

fn graph_errors(why: &str, message: impl Into<String>, errors: &[GraphError]) -> RpcError {
    let mut e = bad(why, message);
    if let Some(Value::Object(d)) = e.data.as_mut() {
        d.insert("errors".into(), errors_json(errors));
    }
    e
}

fn errors_json(errors: &[GraphError]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| json!({ "node": e.node, "reason": e.code, "message": e.message }))
            .collect(),
    )
}

fn available(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<ProcGenSettings>() && world.contains_resource::<ProcGenRegistry>() {
        Ok(())
    } else {
        Err(missing(
            "procgen_unavailable",
            "procedural generation is not installed (ProcGenPlugin)",
        ))
    }
}

fn generator(world: &World, params: &Value) -> Result<Entity, RpcError> {
    available(world)?;
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("procgen_not_generator", "`entity` (an entity id) is required"))?;
    let e = Entity::try_from_bits(bits)
        .ok_or_else(|| bad("procgen_not_generator", format!("{bits} is not an entity id")))?;
    if world.get::<ProcGenerator>(e).is_none() {
        return Err(missing(
            "procgen_not_generator",
            format!("entity {bits} has no ProcGenerator"),
        ));
    }
    Ok(e)
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// JSON → untyped RON value: `{"bind": "p"}` binds a parameter, `{"ron": "..."}` is literal RON
/// (enum variants with fields), objects are records, arrays lists.
pub(crate) fn json_to_setting(v: &Value) -> Result<SettingValue, String> {
    Ok(match v {
        Value::Null => SettingValue::Ident("None".into()),
        Value::Bool(b) => SettingValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SettingValue::Int(i),
            None => SettingValue::Float(n.as_f64().ok_or("number out of range")?),
        },
        Value::String(s) => SettingValue::Str(s.clone()),
        Value::Array(a) => SettingValue::List(a.iter().map(json_to_setting).collect::<Result<_, _>>()?),
        Value::Object(o) => {
            if let (1, Some(Value::String(p))) = (o.len(), o.get("bind")) {
                return Ok(SettingValue::Call(
                    "Bind".into(),
                    vec![SettingValue::Str(p.clone())],
                ));
            }
            if let (1, Some(Value::String(r))) = (o.len(), o.get("ron")) {
                return SettingValue::parse(r).map_err(|e| e.to_string());
            }
            SettingValue::Record(
                None,
                o.iter()
                    .map(|(k, x)| json_to_setting(x).map(|v| (k.clone(), v)))
                    .collect::<Result<_, _>>()?,
            )
        }
    })
}

/// Untyped RON value → JSON (the inverse of [`json_to_setting`] where it can be).
pub(crate) fn setting_to_json(v: &SettingValue) -> Value {
    match v {
        SettingValue::Bool(b) => json!(b),
        SettingValue::Int(i) => json!(i),
        // JSON has no inf or NaN (`json!` would write null): keep the RON text (audit L6).
        SettingValue::Float(f) if !f.is_finite() => json!({ "ron": v.write() }),
        SettingValue::Float(f) => json!(f),
        SettingValue::Str(s) | SettingValue::Ident(s) => json!(s),
        SettingValue::Tuple(items) | SettingValue::List(items) => {
            Value::Array(items.iter().map(setting_to_json).collect())
        }
        SettingValue::Record(None, fields) => Value::Object(
            fields
                .iter()
                .map(|(k, x)| (k.clone(), setting_to_json(x)))
                .collect(),
        ),
        other => match other.as_bind() {
            Some(p) => json!({ "bind": p }),
            None => json!({ "ron": other.write() }),
        },
    }
}

fn state_json(world: &World, e: Entity) -> Value {
    let st = world.get::<ProcGenState>(e);
    let out = world.get::<ProcOutput>(e);
    // The generator's data layers and presence (spec 37; its output follows it, ADR-7214).
    let (layers, presence) = regia_procgen::world::generator_presence(world, e);
    json!({
        "layers": layers,
        "presence": presence.as_str(),
        "state": st.map_or("idle", |s| s.state.name()),
        "run_id": st.map_or(0, |s| s.run_id),
        "last_ok_run": st.map_or(0, |s| s.last_ok_run),
        "output_hash": st.and_then(|s| s.output_hash.as_ref().map(hex)),
        "stale": st.is_some_and(|s| s.stale),
        "stale_cells": st.map_or(0, |s| s.stale_cells.len()),
        "evaluations": st.map_or(0, |s| s.evaluations),
        "error": st.and_then(|s| s.last_error.as_ref()).map(|(node, code, msg)| json!({ "node": node, "reason": code, "message": msg })),
        "bake": world.get::<regia_procgen::world::ProcBakeOrigin>(e).map(|o| {
            let cells = world
                .get::<ProcGenerator>(e)
                .is_some_and(regia_procgen::tiles::is_baked_streaming);
            bake_json(o, cells)
        }),
        "output": {
            "instances": out.map_or(0, |o| o.0.instance_count()),
            "instance_sets": out.map_or(0, |o| o.0.instance_sets.len()),
            "entities": out.map_or(0, |o| o.0.prefabs.len()),
            "tables": out.map_or(0, |o| o.0.tables.len()),
        },
    })
}

/// Where a loaded bake came from (ADR-7262): its file and directory, the level whose bake
/// directory held it (`null`: the opened level's, `ProcGenSettings::bake_dir`), `disk` or `pak`,
/// and `kind`: `whole` (a published output) or `cells` (an anchored generator streaming its cells
/// from the file, ADR-7287).
fn bake_json(o: &regia_procgen::world::ProcBakeOrigin, cells: bool) -> Value {
    json!({
        "path": o.path.to_string_lossy().replace('\\', "/"),
        "dir": o.path.parent().map(|d| d.to_string_lossy().replace('\\', "/")),
        "level": o.level.map(|l| l.0.to_string()),
        "source": o.source.name(),
        "kind": if cells { "cells" } else { "whole" },
    })
}

/// The levels whose bakes the loaded generators came from (ADR-7262): the opened level's directory
/// (`level: null`) and each placed or additive level's, with how many bakes loaded from disk and
/// from the pak. A packaged game lists here the levels whose bakes its pak carries.
fn bake_levels_json(world: &mut World) -> Value {
    let primary = world
        .get_resource::<regia_procgen::world::ProcGenSettings>()
        .and_then(|s| s.bake_dir.clone());
    let dirs = world
        .get_resource::<regia_procgen::world::ProcLevelBakeDirs>()
        .cloned()
        .unwrap_or_default();
    let mut by_level: std::collections::BTreeMap<Option<u128>, (usize, usize)> =
        std::collections::BTreeMap::new();
    for o in world.query::<&regia_procgen::world::ProcBakeOrigin>().iter(world) {
        let entry = by_level.entry(o.level.map(|l| l.0.0)).or_default();
        match o.source {
            regia_procgen::world::ProcBakeSource::Disk => entry.0 += 1,
            regia_procgen::world::ProcBakeSource::Pak => entry.1 += 1,
        }
    }
    let rows: Vec<Value> = by_level
        .into_iter()
        .map(|(level, (disk, pak))| {
            let id = level.map(|l| regia_scene::level::LevelId(regia_core::id::Guid(l)));
            let dir = match id {
                None => primary.clone(),
                Some(id) => dirs.0.get(&id).cloned(),
            };
            json!({
                "level": id.map(|l| l.0.to_string()),
                "dir": dir.map(|d| d.to_string_lossy().replace('\\', "/")),
                "disk": disk,
                "pak": pak,
            })
        })
        .collect();
    Value::Array(rows)
}

/// `procgen.list {}`: every generator with its state.
pub(crate) fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let mut rows: Vec<(u128, Value)> = Vec::new();
    let gens: Vec<Entity> = world
        .query_filtered::<Entity, With<ProcGenerator>>()
        .iter(world)
        .collect();
    for e in gens {
        let Some(g) = world.get::<ProcGenerator>(e) else {
            continue;
        };
        let guid = world.get::<EntityGuid>(e).map_or(0, |g| g.0.0);
        let mut row = json!({
            "entity": e.to_bits(),
            "guid": regia_core::id::Guid(guid).to_string(),
            "name": world.get::<Name>(e).map(|n| n.as_str().to_string()),
            "graph": g.graph,
            "mode": g.mode.name(),
        });
        if let (Value::Object(r), Value::Object(s)) = (&mut row, state_json(world, e)) {
            r.extend(s);
        }
        rows.push((guid, row));
    }
    rows.sort_by_key(|(g, _)| *g);
    let draw = world
        .get_resource::<regia_procgen::draw::ProcDrawStats>()
        .map(|d| {
            json!({
                "drawn": d.drawn,
                "culled": d.culled,
                "missing_meshes": d.missing_meshes,
                "generated_meshes": d.generated_meshes,
                "generated_triangles": d.generated_triangles,
                "missing_materials": d.missing_materials,
            })
        });
    let bake_levels = bake_levels_json(world);
    Ok(json!({
        "generators": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
        "draw": draw,
        "bake_levels": bake_levels,
    }))
}

/// `procgen.get { entity }`: component fields, effective parameters with their source, state.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    let g = world.get::<ProcGenerator>(e).cloned().unwrap_or_default();
    let mut effective = Map::new();
    if let Ok(resolved) = resolve_graph(&g.graph, &|p| read_graph_text(world, p)) {
        for p in &resolved.graph.params {
            let (value, source) = match (g.params.get(&p.name), resolved.overrides.get(&p.name)) {
                (Some(text), _) => (
                    SettingValue::parse(text).unwrap_or(SettingValue::Str(text.clone())),
                    "generator".to_string(),
                ),
                (None, Some((v, ParamSource::Variant(path)))) => (v.clone(), format!("variant:{path}")),
                _ => (p.default.clone(), "default".to_string()),
            };
            effective.insert(
                p.name.clone(),
                json!({ "value": setting_to_json(&value), "source": source, "type": p.ty.name() }),
            );
        }
    }
    let mut out = json!({
        "entity": e.to_bits(),
        "graph": g.graph,
        "params": g.params,
        "seed": g.seed,
        "effective_seed": regia_procgen::world::generator_seed(world, e),
        "mode": g.mode.name(),
        "live": g.live,
        "refresh_on_change": g.refresh_on_change,
        "editor_only": g.editor_only,
        "dependency_priority": g.dependency_priority,
        "format_version": g.format_version,
        "effective_params": effective,
    });
    if let (Value::Object(r), Value::Object(s)) = (&mut out, state_json(world, e)) {
        r.extend(s);
    }
    if let (Value::Object(r), Value::Object(s)) = (&mut out, runtime_json(world, e)) {
        r.extend(s);
    }
    Ok(out)
}

/// The runtime side of `procgen.get`: world bounds, instance colliders, the loaded tier of a
/// `PerTier` bake and the content hashes of the files the latest run read (ADR-2421).
fn runtime_json(world: &mut World, e: Entity) -> Value {
    let bounds = regia_procgen::world::generator_bounds(world, e);
    let (lo, hi) = bounds.world_aabb();
    let colliders: Vec<Value> = world
        .query::<&regia_procgen::world::ProcInstanceCollider>()
        .iter(world)
        .filter(|c| c.generator == e)
        .map(|c| {
            json!({
                "kind": match c.kind {
                    regia_procgen::world::InstanceCollision::None => "none",
                    regia_procgen::world::InstanceCollision::Simple => "simple",
                    regia_procgen::world::InstanceCollision::Mesh => "mesh",
                },
                "instances": c.instances,
            })
        })
        .collect();
    let st = world.get::<ProcGenState>(e);
    let files: Vec<Value> = st
        .map(|s| {
            s.file_hashes
                .iter()
                .map(|(path, h)| json!({ "path": path, "hash": h.as_ref().map(hex) }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "bounds": { "min": lo, "max": hi },
        "bounds_problem": regia_procgen::world::bounds_problem(world, e),
        "colliders": colliders,
        "baked_tier": st.and_then(|s| s.baked_tier).map(|t| t.name()),
        "files": files,
        "editor_only_refused": regia_procgen::world::editor_only_refused(world, e),
    })
}

fn load_report_json(r: &regia_procgen::world::ProcGenLoadReport) -> Value {
    json!({
        "level": r.level.map(|l| l.0.to_string()),
        "ok": r.ok(),
        "timed_out": r.timed_out,
        "frames": r.frames,
        "ms": r.ms,
        "generators": r.entries.iter().map(|x| json!({
            "entity": x.entity.to_bits(),
            "guid": x.guid.to_string(),
            "mode": x.mode,
            "state": x.state.name(),
            "frames": x.frames,
            "ms": x.ms,
            "reason": x.code,
        })).collect::<Vec<_>>(),
    })
}

/// `procgen.load_report {}`: the level-load gate (spec 62 § 7.4): whether a load is still generating,
/// what it waits for, and the last finished load's report.
pub(crate) fn load_report(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let Some(gate) = world.get_resource::<regia_procgen::world::ProcGenLoadGate>() else {
        return Ok(json!({ "pending": false, "waiting": [], "open": null, "last": null }));
    };
    let waiting: Vec<Value> = gate
        .waiting
        .iter()
        .map(|(e, mode)| {
            json!({
                "entity": e.to_bits(),
                "guid": world.get::<EntityGuid>(*e).map(|g| g.0.to_string()),
                "mode": mode,
            })
        })
        .collect();
    Ok(json!({
        "pending": gate.pending(),
        "waiting": waiting,
        "open": gate.open.as_ref().map(load_report_json),
        "last": gate.last.as_ref().map(load_report_json),
    }))
}

/// The `threads` of `procgen.run`: absent or null means the configured schedule, 0 is serial.
fn run_threads(params: &Value) -> Result<Option<usize>, RpcError> {
    let Some(v) = params.get("threads").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    v.as_u64()
        .filter(|n| *n <= 256)
        .map(|n| Some(n as usize))
        .ok_or_else(|| {
            bad(
                "procgen_param_type",
                "`threads` is an integer 0..=256 (0 = serial)",
            )
        })
}

/// `procgen.run { entity, params?, seed?, wait?, keep_debug?, threads? }`.
pub(crate) fn run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    if regia_procgen::world::editor_only_refused(world, e) {
        return Err(bad(
            "procgen_editor_only",
            "the generator is editor_only: it runs only in the editor and in bakes",
        ));
    }
    // Checked before `params` and `seed` are stored, so a refused value changes nothing
    // (ADR-5597: `seed`, `keep_debug` and `wait` were checked after `params` was stored).
    let threads = run_threads(params)?;
    let seed = match params.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|s| u32::try_from(s).ok())
                .ok_or_else(|| bad("procgen_param_type", "`seed` must be a u32"))?,
        ),
    };
    let keep_debug = crate::params::opt_bool(params, "keep_debug")?.unwrap_or(false);
    let wait = crate::params::opt_bool(params, "wait")?.unwrap_or(false);
    if let Some(Value::Object(p)) = params.get("params") {
        let mut texts = BTreeMap::new();
        for (k, v) in p {
            let s =
                json_to_setting(v).map_err(|m| bad("procgen_param_type", format!("`params.{k}`: {m}")))?;
            texts.insert(k.clone(), s.write());
        }
        // Keep the new values only when the graph accepts them: a stored bad value used to fail
        // every later run of the generator (audit L5). A graph that fails either way keeps them, as
        // before, and the run reports why.
        let old = world
            .get::<ProcGenerator>(e)
            .map(|g| g.params.clone())
            .unwrap_or_default();
        if let Some(mut g) = world.get_mut::<ProcGenerator>(e) {
            g.params.extend(texts);
        }
        if let Err((code, message, errors)) = regia_procgen::world::compile_for(world, e) {
            let new = world
                .get_mut::<ProcGenerator>(e)
                .map(|mut g| std::mem::replace(&mut g.params, old));
            if regia_procgen::world::compile_for(world, e).is_ok() {
                let mut err = bad(
                    "procgen_param_type",
                    format!("`params` rejected: {code}: {message}"),
                );
                if let Some(Value::Object(d)) = err.data.as_mut() {
                    let list: Vec<Value> = errors
                        .iter()
                        .map(|x| json!({ "node": x.node, "reason": x.code, "message": x.message }))
                        .collect();
                    d.insert("errors".into(), json!(list));
                }
                return Err(err);
            }
            if let (Some(new), Some(mut g)) = (new, world.get_mut::<ProcGenerator>(e)) {
                g.params = new;
            }
        }
    }
    if let Some(s) = seed
        && let Some(mut g) = world.get_mut::<ProcGenerator>(e)
    {
        g.seed = Some(s);
    }
    // Anchored and baked-streaming generators are cell-scheduled (spec 66 § 4.2a). A run request
    // re-queues resident cells. It does not allocate a whole-graph run id, and `wait` does not
    // block until those cells finish.
    if world
        .get::<ProcGenerator>(e)
        .is_some_and(regia_procgen::tiles::is_scheduled)
    {
        let keys = regia_procgen::world::requeue_scheduled(world, e);
        let st = world.get::<ProcGenState>(e);
        return Ok(json!({
            "run_id": st.map_or(0, |s| s.run_id),
            "state": st.map_or("idle", |s| s.state.name()),
            "scheduled": true,
            "requeued": keys.iter().map(|k| json!([k.cell.x, k.cell.z, k.tier])).collect::<Vec<_>>(),
            "run_requested": false,
        }));
    }
    if !wait {
        regia_procgen::world::request_run(world, e);
        let run_id = world.get::<ProcGenState>(e).map_or(0, |s| s.run_id) + 1;
        return Ok(json!({ "run_id": run_id, "state": "queued" }));
    }
    let before = world.resource::<ProcGenSettings>().keep_debug;
    let threads_before = world.resource::<ProcGenSettings>().threads;
    {
        let mut settings = world.resource_mut::<ProcGenSettings>();
        settings.keep_debug = before || keep_debug;
        settings.threads = threads.or(threads_before);
    }
    let state = regia_procgen::world::run_now(world, e);
    {
        let mut settings = world.resource_mut::<ProcGenSettings>();
        settings.keep_debug = before;
        settings.threads = threads_before;
    }
    let st = world.get::<ProcGenState>(e);
    if state == Some(ProcGenRunState::Failed) {
        let (node, code, msg) = st.and_then(|s| s.last_error.clone()).unwrap_or((
            None,
            "procgen_run_failed".into(),
            String::new(),
        ));
        let mut err = bad("procgen_run_failed", format!("run failed: {code}: {msg}"));
        if let Some(Value::Object(d)) = err.data.as_mut() {
            d.insert("node".into(), json!(node));
            d.insert(
                "errors".into(),
                json!([{ "node": node, "reason": code, "message": msg }]),
            );
            if code == "limit_exceeded" {
                d.insert("reason".into(), json!("procgen_limit_exceeded"));
            }
        }
        return Err(err);
    }
    let out = state_json(world, e);
    Ok(json!({
        "run_id": out["run_id"],
        "state": out["state"],
        "output_hash": out["output_hash"],
        "counts": out["output"],
        "report": report_json(world, e)["totals"],
    }))
}

/// `procgen.cancel { entity }`.
pub(crate) fn cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    let cancelled = regia_procgen::world::cancel(world, e);
    let st = world.get::<ProcGenState>(e);
    Ok(json!({
        "run_id": cancelled.unwrap_or_else(|| st.map_or(0, |s| s.run_id)),
        "cancelled": cancelled.is_some(),
        "state": st.map_or("idle", |s| s.state.name()),
    }))
}

/// `procgen.clear { entity }`.
pub(crate) fn clear(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    Ok(json!({ "cleared": regia_procgen::world::clear(world, e) }))
}

fn report_json(world: &World, e: Entity) -> Value {
    let Some(r) = world.get::<ProcGenState>(e).and_then(|s| s.report.as_ref()) else {
        return json!({ "nodes": [], "totals": Value::Null });
    };
    let roads: Vec<Value> = r
        .roads
        .iter()
        .map(|(road, l)| {
            json!({
                "road": road,
                "length_m": l.length_m,
                "junctions": l.junctions,
                "flatten_rect": l.flatten_rect,
                "flatten_limit_at_m": l.flatten_limit_at_m,
            })
        })
        .collect();
    let nodes: Vec<Value> = r
        .nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.id, "ty": n.ty, "ms": n.ms, "items": n.items, "elements": n.elements,
                "cache_hit": n.cache_hit, "inactive": n.inactive, "bypassed": n.bypassed,
                "warnings": n.warnings.iter().map(|(c, m)| json!({ "reason": c, "message": m })).collect::<Vec<_>>(),
                "counters": n.counters,
            })
        })
        .collect();
    json!({
        "nodes": nodes,
        "totals": {
            "nodes": r.nodes.len(),
            "ms": r.total_ms,
            "cache_hits": r.cache_hits,
            "inactive": r.nodes.iter().filter(|n| n.inactive).count(),
            "warnings": r.warning_codes(),
            "junctions": r.nodes.iter().filter_map(|n| n.counters.get("junctions")).sum::<u64>(),
        },
        "roads": roads,
        "compile_warnings": errors_json(&r.compile_warnings),
    })
}

/// `procgen.detach { entity }` (editor only): the output becomes ordinary level content.
pub(crate) fn detach(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    if !world.resource::<ProcGenSettings>().editor {
        return Err(bad(
            "procgen_editor_only",
            "procgen.detach is an editor action (ProcGenSettings::editor)",
        ));
    }
    let d = regia_procgen::world::detach(world, e).map_err(|(code, msg)| missing(&code, msg))?;
    Ok(json!({
        "root": d.root.to_bits(),
        "entities": d.entities.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        "instance_sets": d.instance_sets.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        "meshes": d.meshes.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        "unsaved_meshes": d.unsaved_meshes,
    }))
}

/// `procgen.report { entity }`: per node `{ id, ty, ms, items, elements, cache_hit, inactive, warnings }`.
pub(crate) fn report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    Ok(report_json(world, e))
}

fn column_rows(col: &AttrColumn, offset: usize, limit: usize) -> Vec<Value> {
    (offset..col.len().min(offset + limit))
        .filter_map(|i| col.get(i))
        .map(|v| match v {
            regia_procgen::AttrValue::Name(s) | regia_procgen::AttrValue::Asset(s) => json!(s),
            regia_procgen::AttrValue::Bool(b) => json!(b),
            regia_procgen::AttrValue::Int(i) => json!(i),
            regia_procgen::AttrValue::Transform(t) => json!({ "t": t.t, "r": t.r, "s": t.s }),
            other => json!(other.components().unwrap_or_default()),
        })
        .collect()
}

fn item_json(item: &DataItem, offset: usize, limit: usize, attrs: Option<&[String]>) -> Value {
    let want = |n: &str| attrs.is_none_or(|a| a.iter().any(|x| x == n));
    let mut columns = Vec::new();
    let mut rows = Map::new();
    match &item.payload {
        Payload::Points(p) => {
            let end = p.len().min(offset + limit);
            let builtin: [(&str, &str, Vec<Value>); 5] = [
                (
                    "position",
                    "Vec3",
                    (offset..end).map(|i| json!(p.position[i])).collect(),
                ),
                (
                    "rotation",
                    "Quat",
                    (offset..end).map(|i| json!(p.rotation[i])).collect(),
                ),
                (
                    "scale",
                    "Vec3",
                    (offset..end).map(|i| json!(p.scale[i])).collect(),
                ),
                (
                    "density",
                    "Float",
                    (offset..end).map(|i| json!(p.density[i])).collect(),
                ),
                ("seed", "Int", (offset..end).map(|i| json!(p.seed[i])).collect()),
            ];
            for (n, t, v) in builtin {
                if want(n) {
                    columns.push(json!([n, t]));
                    rows.insert(n.to_string(), Value::Array(v));
                }
            }
            for (n, c) in &p.attrs {
                if want(n) {
                    columns.push(json!([n, c.ty().name()]));
                    rows.insert(n.clone(), Value::Array(column_rows(c, offset, limit)));
                }
            }
        }
        Payload::Table(t) => {
            for (n, c) in &t.columns {
                if want(n) {
                    columns.push(json!([n, c.ty().name()]));
                    rows.insert(n.clone(), Value::Array(column_rows(c, offset, limit)));
                }
            }
        }
        Payload::Shape(s) => {
            rows.insert(
                "shape".into(),
                json!({ "position": s.position, "rotation": s.rotation, "edge_m": s.edge_m }),
            );
        }
        Payload::Ground(g) => {
            rows.insert("ground".into(), json!({ "rect": g.rect, "samples": g.size }));
        }
        Payload::Mask(m) => {
            rows.insert(
                "mask".into(),
                json!({ "rect": m.rect, "size": [m.width, m.height] }),
            );
        }
        Payload::Mesh(m) => {
            rows.insert(
                "mesh".into(),
                json!({ "asset": m.asset, "vertices": m.positions.len(), "triangles": m.indices.len() / 3 }),
            );
        }
        Payload::Spline(s) => {
            let positions: Vec<[f32; 3]> = s.spline.points.iter().map(|p| p.position.to_array()).collect();
            rows.insert(
                "spline".into(),
                json!({ "points": positions, "closed": s.spline.closed, "length": s.spline.length() }),
            );
        }
        Payload::Polygon(p) => {
            rows.insert(
                "polygon".into(),
                json!({ "origin": p.origin, "rings": p.world_rings(), "area": p.area() }),
            );
        }
        Payload::PlantSkeleton(s) => {
            rows.insert("plants".into(), plant_stats_json(s, None));
        }
        Payload::PlantMesh(m) => {
            rows.insert("plants".into(), plant_stats_json(&m.skeleton, Some(m)));
            rows.insert(
                "mesh".into(),
                json!({ "slots": m.slots.iter().map(|s| json!({ "material": s.material, "vertices": s.positions.len(), "triangles": s.triangles() })).collect::<Vec<_>>() }),
            );
        }
    }
    json!({
        "kind": item.payload.kind().name(),
        "tags": item.tags,
        "scope": item.scope.iter().map(|(k, v)| (k.clone(), json!(v.as_key_text().or_else(|| v.components().map(|c| format!("{c:?}")))))).collect::<Map<_, _>>(),
        "len": item.payload.len(),
        "columns": columns,
        "rows": rows,
    })
}

/// `procgen.inspect { entity, node, pin?, item?, offset?, limit?, attributes? }`: a node's output
/// from the last run made with `keep_debug: true`.
/// The `cell: [i, j]` and `tier` of `procgen.inspect`, when given.
fn cell_key(params: &Value) -> Result<Option<regia_procgen::tiles::TileKey>, RpcError> {
    let Some(cell) = params.get("cell").filter(|c| !c.is_null()) else {
        return Ok(None);
    };
    let pair = cell.as_array().filter(|a| a.len() == 2);
    let Some((Some(i), Some(j))) = pair.map(|a| (a[0].as_i64(), a[1].as_i64())) else {
        return Err(bad("procgen_unknown_cell", "`cell` is an [i, j] pair"));
    };
    let tier = crate::params::opt_u64(params, "tier")?
        .and_then(|t| u8::try_from(t).ok())
        .unwrap_or(0);
    Ok(Some(regia_procgen::tiles::TileKey {
        tier,
        cell: regia_procgen::tiles::CellCoord::new(i, j),
    }))
}

/// A cell's debug outputs: from the last whole tiled run, or a resident anchored cell.
fn cell_debug(
    world: &World,
    e: Entity,
    key: regia_procgen::tiles::TileKey,
) -> Option<std::sync::Arc<regia_procgen::world::DebugOutputs>> {
    let whole = world
        .get::<ProcGenState>(e)
        .and_then(|s| s.tiled.as_ref())
        .and_then(|t| t.cells.get(&key))
        .and_then(|c| c.debug.clone());
    whole.or_else(|| {
        world
            .get::<regia_procgen::tiles::ProcGenTileHost>(e)
            .and_then(|h| h.cell(key))
            .and_then(|c| c.debug.clone())
    })
}

pub(crate) fn inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = generator(world, params)?;
    let node = crate::params::opt_u64(params, "node")?
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| bad("procgen_unknown_node", "`node` (a node id) is required"))?;
    let debug = match cell_key(params)? {
        Some(key) => cell_debug(world, e, key),
        None => world.get::<ProcGenState>(e).and_then(|s| s.debug.clone()),
    };
    let Some(debug) = debug else {
        return Err(missing(
            "procgen_no_debug_data",
            "no debug data: run with procgen.run { wait: true, keep_debug: true } first (a tiled generator: name `cell` and `tier`)",
        ));
    };
    let outs = debug.get(&node).ok_or_else(|| {
        missing(
            "procgen_unknown_node",
            format!("node {node} has no recorded output"),
        )
    })?;
    let offset = crate::params::opt_u64(params, "offset")?.unwrap_or(0) as usize;
    let limit = (crate::params::opt_u64(params, "limit")?.unwrap_or(100) as usize).min(MAX_ROWS);
    let attrs: Option<Vec<String>> = crate::params::opt_array(params, "attributes")?
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
    let pin = crate::params::opt_u64(params, "pin")?.map(|p| p as usize);
    let only_item = crate::params::opt_u64(params, "item")?.map(|p| p as usize);
    if let Some(p) = pin
        && p >= outs.len()
    {
        return Err(missing(
            "procgen_unknown_pin",
            format!("node {node} has {} output pins", outs.len()),
        ));
    }
    let mut items = Vec::new();
    for (k, b) in outs.iter().enumerate() {
        if pin.is_some_and(|p| p != k) {
            continue;
        }
        for (j, it) in b.0.iter().enumerate() {
            if only_item.is_some_and(|x| x != j) {
                continue;
            }
            let mut v = item_json(it, offset, limit, attrs.as_deref());
            if let Value::Object(o) = &mut v {
                o.insert("pin".into(), json!(k));
            }
            items.push(v);
        }
    }
    Ok(json!({ "items": items }))
}

fn load_graph(world: &World, path: &str) -> Result<ProcGraph, RpcError> {
    let text = read_graph_text(world, path)
        .ok_or_else(|| missing("procgen_unknown_graph", format!("cannot read `{path}`")))?;
    ProcGraph::from_ron(&text).map_err(|e| bad("procgen_graph_invalid", format!("{path}: {e}")))
}

fn graph_json(g: &ProcGraph, errors: &[GraphError]) -> Value {
    json!({
        "description": g.description,
        "params": g.params.iter().map(|p| json!({
            "name": p.name, "ty": p.ty.name(), "default": setting_to_json(&p.default),
            "min": p.min, "max": p.max, "doc": p.doc,
        })).collect::<Vec<_>>(),
        "inputs": g.inputs.iter().map(|p| json!({ "name": p.name, "kinds": p.kinds.names() })).collect::<Vec<_>>(),
        "outputs": g.outputs.iter().map(|p| json!({ "name": p.name, "kinds": p.kinds.names() })).collect::<Vec<_>>(),
        "nodes": g.nodes.iter().map(|(id, n)| json!({
            "id": id, "ty": n.ty, "enabled": n.enabled, "at": [n.at.0, n.at.1], "seed_salt": n.seed_salt,
            "settings": n.settings.iter().map(|(k, v)| (k.clone(), setting_to_json(v))).collect::<Map<_, _>>(),
        })).collect::<Vec<_>>(),
        "edges": g.edges.iter().map(|e| json!({ "from": [e.from.0, e.from.1], "to": [e.to.0, e.to.1] })).collect::<Vec<_>>(),
        "flags": { "starter": g.flags.starter, "tool": g.flags.tool, "domain": g.flags.domain.name() },
        "errors": errors_json(errors),
    })
}

/// `procgen.graph_get { path }`: params, pins, nodes, edges and compile errors.
pub(crate) fn graph_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("procgen_unknown_graph", "`path` is required"))?;
    regia_core::project_path::check_relative(path).map_err(bad_path)?;
    let g = load_graph(world, path)?;
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let errors = g.compile(&registry).err().map(|e| e.0).unwrap_or_default();
    let mut v = graph_json(&g, &errors);
    if let Value::Object(o) = &mut v {
        o.insert("graph_hash".into(), json!(hex(&g.graph_hash(&registry))));
    }
    Ok(v)
}

fn pin_end(v: Option<&Value>, what: &str) -> Result<(u32, String), RpcError> {
    let a = v.and_then(Value::as_array).filter(|a| a.len() == 2);
    let parsed = a.and_then(|a| Some((u32::try_from(a[0].as_u64()?).ok()?, a[1].as_str()?.to_string())));
    parsed.ok_or_else(|| {
        bad(
            "procgen_unknown_pin",
            format!("`{what}` must be [node id, \"pin\"]"),
        )
    })
}

fn str_field<'a>(op: &'a Value, k: &str) -> Result<&'a str, RpcError> {
    op.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| bad("procgen_graph_invalid", format!("op needs string `{k}`")))
}

fn id_field(op: &Value, k: &str) -> Result<u32, RpcError> {
    crate::params::opt_u64(op, k)?
        .and_then(|i| u32::try_from(i).ok())
        .ok_or_else(|| bad("procgen_unknown_node", format!("op needs node id `{k}`")))
}

fn node_mut(g: &mut ProcGraph, id: u32) -> Result<&mut NodeEntry, RpcError> {
    g.nodes
        .get_mut(&id)
        .ok_or_else(|| missing("procgen_unknown_node", format!("no node {id}")))
}

fn at(op: &Value) -> Result<(f64, f64), RpcError> {
    let coord = |a: &[Value], i: usize| -> Result<f64, RpcError> {
        match a.get(i) {
            None | Some(Value::Null) => Ok(0.0),
            Some(v) => v
                .as_f64()
                .ok_or_else(|| RpcError::wrong_type(&format!("at[{i}]"), "a number")),
        }
    };
    match crate::params::opt_array(op, "at")? {
        Some(a) => Ok((coord(a, 0)?, coord(a, 1)?)),
        None => Ok((0.0, 0.0)),
    }
}

fn pins_from(v: &Value) -> Result<Vec<GraphPin>, RpcError> {
    let a = v
        .as_array()
        .ok_or_else(|| bad("procgen_graph_invalid", "graph pins are a list"))?;
    a.iter()
        .map(|p| {
            let name = str_field(p, "name")?.to_string();
            let kinds: Vec<DataKind> = crate::params::opt_array(p, "kinds")?
                .map(|k| {
                    k.iter()
                        .filter_map(|x| x.as_str().and_then(DataKind::from_name))
                        .collect()
                })
                .unwrap_or_else(|| DataKind::ALL.to_vec());
            Ok(GraphPin {
                name,
                kinds: KindSet::of(&kinds),
            })
        })
        .collect()
}

fn rename_binds(v: &mut SettingValue, from: &str, to: &str) {
    if v.as_bind() == Some(from) {
        *v = SettingValue::Call("Bind".into(), vec![SettingValue::Str(to.to_string())]);
        return;
    }
    match v {
        SettingValue::Call(_, items) | SettingValue::Tuple(items) | SettingValue::List(items) => {
            items.iter_mut().for_each(|x| rename_binds(x, from, to));
        }
        SettingValue::Record(_, fields) => fields.iter_mut().for_each(|(_, x)| rename_binds(x, from, to)),
        SettingValue::Map(entries) => entries.iter_mut().for_each(|(_, x)| rename_binds(x, from, to)),
        _ => {}
    }
}

/// Applies one `graph_edit` op; returns a new node id for `add_node` / `add_note`.
fn apply_op(g: &mut ProcGraph, op: &Value) -> Result<Option<u32>, RpcError> {
    let kind = str_field(op, "op")?;
    match kind {
        "add_node" | "add_note" => {
            let ty = if kind == "add_note" {
                "note"
            } else {
                str_field(op, "ty")?
            };
            let mut n = NodeEntry::new(ty);
            n.at = at(op)?;
            if kind == "add_note" {
                n.settings.insert(
                    "text".into(),
                    SettingValue::Str(str_field(op, "text")?.to_string()),
                );
            }
            if let Some(Value::Object(s)) = op.get("settings") {
                for (k, v) in s {
                    let v =
                        json_to_setting(v).map_err(|m| bad("procgen_param_type", format!("`{k}`: {m}")))?;
                    n.settings.insert(k.clone(), v);
                }
            }
            let id = g.next_node_id();
            g.nodes.insert(id, n);
            return Ok(Some(id));
        }
        "remove_node" => {
            let id = id_field(op, "id")?;
            g.nodes
                .remove(&id)
                .ok_or_else(|| missing("procgen_unknown_node", format!("no node {id}")))?;
            g.edges.retain(|e| e.from.0 != id && e.to.0 != id);
        }
        "set_setting" => {
            let id = id_field(op, "id")?;
            let name = str_field(op, "name")?.to_string();
            let v = json_to_setting(op.get("value").unwrap_or(&Value::Null))
                .map_err(|m| bad("procgen_param_type", format!("`{name}`: {m}")))?;
            node_mut(g, id)?.settings.insert(name, v);
        }
        "connect" | "disconnect" => {
            let (from, to) = (pin_end(op.get("from"), "from")?, pin_end(op.get("to"), "to")?);
            let edge = Edge { from, to };
            if kind == "connect" {
                if !g.edges.contains(&edge) {
                    g.edges.push(edge);
                }
            } else {
                g.edges.retain(|e| *e != edge);
            }
        }
        "set_enabled" => {
            let id = id_field(op, "id")?;
            let on = op
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| bad("procgen_graph_invalid", "`enabled` is a boolean"))?;
            node_mut(g, id)?.enabled = on;
        }
        "set_seed_salt" => {
            let id = id_field(op, "id")?;
            let salt = crate::params::opt_u64(op, "salt")?
                .and_then(|s| u32::try_from(s).ok())
                .ok_or_else(|| bad("procgen_param_type", "`salt` must be a u32"))?;
            node_mut(g, id)?.seed_salt = Some(salt);
        }
        "move" => {
            let id = id_field(op, "id")?;
            node_mut(g, id)?.at = at(op)?;
        }
        "add_param" => {
            let name = str_field(op, "name")?.to_string();
            if !regia_procgen::graph::valid_param_name(&name) || g.param(&name).is_some() {
                return Err(bad(
                    "procgen_unknown_param",
                    format!("`{name}` is not a new valid parameter name"),
                ));
            }
            let ty_name = str_field(op, "ty")?;
            let ty = AttrType::from_name(ty_name)
                .ok_or_else(|| bad("procgen_param_type", format!("unknown type `{ty_name}`")))?;
            g.params.push(ParamDecl {
                name,
                ty,
                default: json_to_setting(op.get("default").unwrap_or(&Value::Null))
                    .map_err(|m| bad("procgen_param_type", m))?,
                min: crate::params::opt_f64(op, "min")?,
                max: crate::params::opt_f64(op, "max")?,
                doc: crate::params::opt_str(op, "doc")?.unwrap_or("").to_string(),
            });
        }
        "set_param_default" => {
            let name = str_field(op, "name")?;
            let v = json_to_setting(op.get("value").unwrap_or(&Value::Null))
                .map_err(|m| bad("procgen_param_type", m))?;
            let p = g
                .params
                .iter_mut()
                .find(|p| p.name == name)
                .ok_or_else(|| missing("procgen_unknown_param", format!("no parameter `{name}`")))?;
            p.default = v;
        }
        "rename_param" => {
            let (from, to) = (
                str_field(op, "from")?.to_string(),
                str_field(op, "to")?.to_string(),
            );
            if !regia_procgen::graph::valid_param_name(&to) || g.param(&to).is_some() {
                return Err(bad(
                    "procgen_unknown_param",
                    format!("`{to}` is not a new valid parameter name"),
                ));
            }
            let p = g
                .params
                .iter_mut()
                .find(|p| p.name == from)
                .ok_or_else(|| missing("procgen_unknown_param", format!("no parameter `{from}`")))?;
            p.name = to.clone();
            for n in g.nodes.values_mut() {
                n.settings.values_mut().for_each(|v| rename_binds(v, &from, &to));
            }
        }
        "remove_param" => {
            let name = str_field(op, "name")?;
            let before = g.params.len();
            g.params.retain(|p| p.name != name);
            if g.params.len() == before {
                return Err(missing("procgen_unknown_param", format!("no parameter `{name}`")));
            }
        }
        "set_graph_pins" => {
            if let Some(v) = op.get("inputs") {
                g.inputs = pins_from(v)?;
            }
            if let Some(v) = op.get("outputs") {
                g.outputs = pins_from(v)?;
            }
        }
        "set_flags" => {
            if let Some(s) = crate::params::opt_bool(op, "starter")? {
                g.flags.starter = s;
            }
            if let Some(t) = op.get("tool") {
                g.flags.tool = t.as_str().map(str::to_string);
            }
            if let Some(d) = crate::params::opt_str(op, "domain")? {
                g.flags.domain = regia_procgen::nodes::Domain::from_name(d)
                    .ok_or_else(|| bad("procgen_wrong_domain", format!("unknown domain `{d}`")))?;
            }
        }
        "set_description" => g.description = str_field(op, "text")?.to_string(),
        other => return Err(bad("procgen_graph_invalid", format!("unknown op `{other}`"))),
    }
    Ok(None)
}

/// `procgen.graph_edit { path, ops, save? }`: applies ops in order (all or none), compiles, and keeps
/// the result as an unsaved overlay or writes it (`save: true`, desktop tools).
pub(crate) fn graph_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("procgen_unknown_graph", "`path` is required"))?
        .to_string();
    regia_core::project_path::check_relative(&path).map_err(bad_path)?;
    let ops = params
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("procgen_graph_invalid", "`ops` must be a list"))?;
    if ops.len() > MAX_OPS {
        return Err(bad(
            "procgen_graph_invalid",
            format!("at most {MAX_OPS} ops per call"),
        ));
    }
    // A graph open in the editor is edited there (spec 67 § 6); a missing file starts a new graph.
    // An open variant refuses both its own path and its parent before any load or save.
    let route = world.get_resource::<ProcGraphDocRoute>().copied();
    let open_doc = match route {
        Some(route) => match (route.open)(world, &path) {
            Ok(graph) => graph,
            Err(deny) => return Err(bad(deny.reason, deny.message)),
        },
        None => None,
    };
    let routed = open_doc.is_some();
    let mut g = match (open_doc, read_graph_text(world, &path)) {
        (Some(doc), _) => doc,
        (None, Some(_)) => load_graph(world, &path)?,
        (None, None) => ProcGraph::default(),
    };
    let mut added = Vec::new();
    for (k, op) in ops.iter().enumerate() {
        match apply_op(&mut g, op) {
            Ok(Some(id)) => added.push(json!({ "op": k, "id": id })),
            Ok(None) => {}
            Err(mut e) => {
                e.message = format!("op {k}: {}", e.message);
                return Err(e);
            }
        }
    }
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    if let Err(errors) = g.compile(&registry) {
        return Err(graph_errors(
            "procgen_graph_invalid",
            format!("{} compile error(s); nothing changed", errors.0.len()),
            &errors.0,
        ));
    }
    let save = crate::params::opt_bool(params, "save")?.unwrap_or(false);
    if let Some(route) = route.filter(|_| routed) {
        let hash = hex(&g.graph_hash(&registry));
        let saved = (route.commit)(world, &path, g, save).map_err(commit_error)?;
        return Ok(
            json!({ "graph_hash": hash, "errors": [], "added": added, "saved": saved, "document": true }),
        );
    }
    let text = g.to_ron();
    let mut saved = false;
    if save {
        let source = world.resource::<ProcGenSettings>().source.clone();
        match source {
            ProcGraphSource::Dir(root) => {
                // The parent (symlinks resolved) must stay in the project; nothing is created
                // before this check (ADR-1816).
                let file = regia_core::project_path::resolve_write(&root, &path).map_err(bad_path)?;
                regia_procgen::pgout::write_atomic(&file, text.as_bytes()).map_err(|e| {
                    if e.code == "procgen_editor_only" {
                        bad("procgen_editor_only", e.message)
                    } else {
                        RpcError::internal(e.to_string())
                    }
                })?;
            }
            ProcGraphSource::Memory(_) => {
                if let ProcGraphSource::Memory(files) = &mut world.resource_mut::<ProcGenSettings>().source {
                    files.insert(path.clone(), text.clone());
                }
            }
        }
        world.resource_mut::<ProcGraphOverlay>().0.remove(&path);
        saved = true;
    } else {
        world
            .resource_mut::<ProcGraphOverlay>()
            .0
            .insert(path.clone(), text);
    }
    Ok(json!({ "graph_hash": hex(&g.graph_hash(&registry)), "errors": [], "added": added, "saved": saved }))
}

/// Why `procgen.graph_edit` must stop before it loads or writes `path`.
///
/// `reason` is the agent `data.reason` (lowercase ascii, digits and `_`). `message` is the one-line
/// detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcGraphRouteDeny {
    /// Agent reason code.
    pub reason: &'static str,
    /// One-line message.
    pub message: &'static str,
}

/// A host `commit` error. A message `reason: detail`, where `reason` is a protocol code, keeps
/// that reason. Anything else stays `procgen_graph_invalid`.
fn commit_error(message: String) -> RpcError {
    match message.split_once(": ") {
        Some((code, detail)) if is_reason_code(code) => bad(code, detail),
        _ => bad("procgen_graph_invalid", message),
    }
}

/// A protocol reason: a lowercase letter, then lowercase letters, digits and `_`.
fn is_reason_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Where `procgen.graph_edit` lands when the host has the graph open in a document (the editor's
/// procedural graph panel, spec 67 § 6). The editor inserts this resource; without it, or for a
/// graph that is not open, edits go to the overlay or the file as before.
#[derive(Resource, Clone, Copy)]
pub struct ProcGraphDocRoute {
    /// `Ok(Some(graph))` when `path` is the open editable document. `Ok(None)` when it is not open.
    /// `Err` when the path is open but must not be loaded or written (an open variant, and its parent).
    pub open: fn(&World, &str) -> Result<Option<ProcGraph>, ProcGraphRouteDeny>,
    /// Replaces the document's graph with the edited one as one undo entry; `true` (the last
    /// argument) also saves the document. Returns whether it saved, or `reason: detail`.
    pub commit: fn(&mut World, &str, ProcGraph, bool) -> Result<bool, String>,
}

/// `procgen.node_types { category? }`: the registry with pins, settings, defaults and ranges.
pub(crate) fn node_types(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let category = crate::params::opt_str(params, "category")?;
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let types: Vec<Value> = registry
        .iter()
        .filter(|k| category.is_none_or(|c| k.category() == c))
        .map(|k| {
            let defaults = regia_procgen::nodes::Settings(
                k.settings()
                    .iter()
                    .filter_map(|d| SettingValue::parse(d.default).ok().map(|v| (d.name.to_string(), v)))
                    .collect(),
            );
            let pins = k.pins(&defaults);
            let pin = |p: &regia_procgen::nodes::PinDesc| json!({ "name": p.name, "kinds": p.kinds.names(), "multi": p.multi, "required": p.required });
            json!({
                "ty": k.id(),
                "category": k.category(),
                "doc": k.doc(),
                "settings_version": k.settings_version(),
                "domains": k.domains().iter().map(|d| d.name()).collect::<Vec<_>>(),
                "inputs": pins.inputs.iter().map(pin).collect::<Vec<_>>(),
                "outputs": pins.outputs.iter().map(pin).collect::<Vec<_>>(),
                "settings": k.settings().iter().map(|d| json!({
                    "name": d.name, "type": d.ty.name(), "default": d.default, "min": d.min, "max": d.max, "doc": d.doc,
                    "options": match d.ty { regia_procgen::nodes::SettingType::Enum(o) => json!(o), _ => Value::Null },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "count": types.len(), "types": types }))
}

fn baked_generators(world: &mut World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    if params.get("entity").is_some_and(|v| !v.is_null()) {
        return Ok(vec![generator(world, params)?]);
    }
    available(world)?;
    let mut gens: Vec<(u128, Entity)> = world
        .query::<(Entity, &ProcGenerator, Option<&EntityGuid>)>()
        .iter(world)
        .filter(|(_, g, _)| g.mode == regia_procgen::world::ProcGenMode::Baked)
        .map(|(e, _, guid)| (guid.map_or(0, |g| g.0.0), e))
        .collect();
    gens.sort();
    Ok(gens.into_iter().map(|(_, e)| e).collect())
}

/// `procgen.bake { entity?, cells?, tier? }`: runs `Baked` generators and writes their `.pgout`
/// files; with `cells`, re-bakes those cells of one tiled generator.
pub(crate) fn bake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(cells) = crate::procgen_tiles::cells_param(params)? {
        let e = generator(world, params)?;
        let tier = crate::procgen_tiles::tier_param(world, e, params)?;
        let keys: Vec<regia_procgen::tiles::TileKey> = cells
            .into_iter()
            .map(|cell| regia_procgen::tiles::TileKey {
                tier: tier.unwrap_or(0),
                cell,
            })
            .collect();
        return Ok(match regia_procgen::world::bake_cells(world, e, &keys) {
            Ok(path) => {
                crate::events::push_bake_written(world, e, &[path.display().to_string()], Some(keys.len()));
                json!({
                "ok": true,
                "baked": [{ "entity": e.to_bits(), "path": path.display().to_string(), "cells": keys.len() }],
                "failed": [],
                })
            }
            Err((code, msg)) => json!({
                "ok": false,
                "baked": [],
                "failed": [{ "entity": e.to_bits(), "reason": code, "message": msg }],
            }),
        });
    }
    let gens = baked_generators(world, params)?;
    let mut baked = Vec::new();
    let mut failed = Vec::new();
    for e in gens {
        match regia_procgen::world::bake(world, e) {
            Ok(path) => {
                // A `PerTier` generator writes one file per tier (ADR-2421), named after the guid
                // of the file it wrote: a placed member's source guid in its source level's
                // directory (ADR-7331).
                let guid = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(regia_procgen::world::bake_file_guid)
                    .unwrap_or_else(|| {
                        world
                            .get::<EntityGuid>(e)
                            .map_or(regia_core::id::Guid::NIL, |g| g.0)
                    });
                let files: Vec<String> = path
                    .parent()
                    .map(|d| regia_procgen::world::bake_files(d, guid))
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                crate::events::push_bake_written(world, e, &files, None);
                baked.push(
                    json!({ "entity": e.to_bits(), "path": path.display().to_string(), "files": files }),
                );
            }
            Err((code, msg)) => failed.push(json!({ "entity": e.to_bits(), "reason": code, "message": msg })),
        }
    }
    Ok(json!({ "ok": failed.is_empty(), "baked": baked, "failed": failed }))
}

/// `procgen.verify { entity? }`: re-runs `Baked` generators and compares with their files. Without
/// `entity` it also names every entity that lost its anchored generator but still has a cell bake
/// (`findings`, code `anchored_generator_missing`, ADR-7331).
pub(crate) fn verify(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let whole_world = params.get("entity").is_none_or(Value::is_null);
    let gens = baked_generators(world, params)?;
    let mut mismatches = Vec::new();
    for e in gens {
        match regia_procgen::world::verify(world, e) {
            Ok(v) if v.ok => {}
            Ok(v) => mismatches.push(json!({
                "entity": e.to_bits(),
                "file_hash": v.file_hash.as_ref().map(hex),
                "run_hash": hex(&v.run_hash),
                "file_error": v.file_error,
                "tiers": v.tiers.iter().map(|(t, ok)| json!({ "tier": t.name(), "ok": ok })).collect::<Vec<_>>(),
            })),
            Err((code, msg)) => {
                mismatches.push(json!({ "entity": e.to_bits(), "reason": code, "message": msg }))
            }
        }
    }
    let findings: Vec<Value> = if whole_world {
        regia_procgen::world::lost_anchored_generators(world)
            .iter()
            .map(|l| {
                json!({
                    "code": "anchored_generator_missing",
                    "entity": l.entity.to_bits(),
                    "guid": l.guid.to_string(),
                    "level": l.level.map(|id| id.0.to_string()),
                    "path": l.path.display().to_string(),
                    "message": format!(
                        "{} has a cell bake but no ProcGenerator (a level saved while ADR-7287 (5) was open?): add the anchored generator again",
                        l.guid
                    ),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(json!({
        "ok": mismatches.is_empty() && findings.is_empty(),
        "mismatches": mismatches,
        "findings": findings,
    }))
}

/// The pipeline half of `procgen.plant_build`, installed by a host that links the asset pipeline (the
/// editor): the agent crate ships in runtime builds and must not depend on it.
#[derive(Resource, Clone, Copy)]
pub struct PlantExportHook {
    /// Writes one export job to a path (`regia_asset_pipeline::plant_export::host_write`).
    pub write: fn(
        &regia_procgen::plant::ExportJob,
        &std::path::Path,
    ) -> Result<regia_procgen::plant::WrittenPlant, regia_procgen::plant::PlantBuildError>,
    /// Loads an envelope mesh file (`regia_asset_pipeline::plant_export::host_load_mesh`).
    pub load_mesh: fn(&std::path::Path, &str) -> Result<regia_procgen::data::MeshData, String>,
}

/// Per-plant statistics as JSON (`procgen.plant_stats`, `procgen.inspect` of plant items).
fn plant_stats_json(
    skeleton: &regia_procgen::plant::PlantSkeleton,
    mesh: Option<&regia_procgen::plant::PlantMesh>,
) -> Value {
    json!(
        regia_procgen::plant::plant_stats(skeleton, mesh)
            .iter()
            .map(|s| json!({
                "seed": s.seed,
                "nodes": s.nodes,
                "max_order": s.max_order,
                "height_m": s.height_m,
                "crown_width_m": s.crown_width_m,
                "branches_shed": s.branches_shed,
                "triangles": s.triangles,
            }))
            .collect::<Vec<_>>()
    )
}

/// A Plant graph ready to run: what `procgen.plant_stats` and `procgen.plant_build` share.
struct PlantGraph {
    path: String,
    seed: u32,
    compiled: regia_procgen::CompiledGraph,
    subgraphs: BTreeMap<String, String>,
    /// Holds the meshes the graph's `plant.envelope_mesh` nodes name.
    snapshot: std::sync::Arc<regia_procgen::exec::StaticSnapshot>,
}

/// Reads, checks and compiles the Plant graph a request names, and loads its envelope meshes: the
/// host's mesh library first, then the file through [`PlantExportHook`]. A mesh neither has stays
/// out, and the node fails the run with `envelope_empty`.
fn plant_graph(world: &mut World, params: &Value, method: &str) -> Result<PlantGraph, RpcError> {
    use std::sync::Arc;

    use regia_procgen::exec::StaticSnapshot;
    use regia_procgen::nodes::Domain;

    available(world)?;
    let path = params.get("graph").and_then(Value::as_str).ok_or_else(|| {
        bad(
            "procgen_unknown_graph",
            "`graph` (a .pgraph.ron path) is required",
        )
    })?;
    let text = read_graph_text(world, path)
        .or_else(|| regia_procgen::plant::starter(path).map(str::to_string))
        .ok_or_else(|| missing("procgen_unknown_graph", format!("cannot read `{path}`")))?;
    let g = ProcGraph::from_ron(&text).map_err(|e| bad("procgen_graph_invalid", format!("{path}: {e}")))?;
    if g.flags.domain != Domain::Plant {
        return Err(bad(
            "procgen_wrong_domain",
            format!(
                "`{path}` is a {} graph; {method} runs Plant graphs",
                g.flags.domain.name()
            ),
        ));
    }
    let mut overrides = BTreeMap::new();
    if let Some(Value::Object(p)) = params.get("params") {
        for (k, v) in p {
            let s =
                json_to_setting(v).map_err(|m| bad("procgen_param_type", format!("`params.{k}`: {m}")))?;
            overrides.insert(k.clone(), s);
        }
    }
    let seed = match params.get("seed") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .and_then(|s| u32::try_from(s).ok())
            .ok_or_else(|| bad("procgen_param_type", "`seed` must be a u32"))?,
    };
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let compiled = g
        .compile_full(&registry, &overrides, &|p| read_graph_text(world, p))
        .map_err(|e| {
            if e.has("plant_runtime_forbidden") {
                bad("procgen_plant_runtime_forbidden", e.to_string())
            } else {
                graph_errors("procgen_graph_invalid", e.to_string(), &e.0)
            }
        })?;
    let subgraphs =
        regia_procgen::graph::collect_subgraphs_with(&g, &overrides, &|p| read_graph_text(world, p));
    let mut snapshot = StaticSnapshot::default();
    let root = crate::req_path::project_root(world);
    for asset in regia_procgen::plant::envelope_assets(&compiled) {
        let library = world
            .get_resource::<regia_procgen::draw::ProcMeshLibrary>()
            .and_then(|lib| lib.0.get(&asset))
            .map(|(mesh, _)| regia_procgen::data::MeshData {
                asset: asset.clone(),
                positions: mesh.vertices.iter().map(|v| v.position).collect(),
                normals: mesh.vertices.iter().map(|v| v.normal).collect(),
                indices: mesh.indices.clone(),
            });
        let mesh = match library {
            Some(mesh) => Some(mesh),
            None => match (
                world.get_resource::<PlantExportHook>().copied(),
                regia_core::project_path::resolve_read(&root, &asset),
            ) {
                (Some(hook), Ok(file)) => match (hook.load_mesh)(&file, &asset) {
                    Ok(mesh) => Some(mesh),
                    Err(e) => return Err(bad("procgen_run_failed", format!("envelope mesh: {e}"))),
                },
                _ => None,
            },
        };
        if let Some(mesh) = mesh {
            snapshot.meshes.insert(asset, Arc::new(mesh));
        }
    }
    Ok(PlantGraph {
        path: path.to_string(),
        seed,
        compiled,
        subgraphs,
        snapshot: Arc::new(snapshot),
    })
}

/// `procgen.plant_build { graph, out, seed?, variants?, params? }` (spec 68 § 8): runs a Plant graph
/// and writes each variant's export job through the host's [`PlantExportHook`], plus the
/// `<name>.plants.ron` index, under the project-relative directory `out`.
pub(crate) fn plant_build(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_procgen::plant::{BuiltPlants, build_plant_variants};
    use regia_procgen::{CancelToken, RunInputs};

    let out = params
        .get("out")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`out` (a project-relative directory) is required"))?;
    let variants = match params.get("variants") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=256).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| bad("procgen_param_type", "`variants` is an integer 1..=256"))?,
    };
    let plant = plant_graph(world, params, "plant_build")?;
    let Some(hook) = world.get_resource::<PlantExportHook>().copied() else {
        return Err(bad(
            "procgen_plant_host_required",
            "this host cannot write plant exports: use the editor, or `regia procgen plant`",
        ));
    };
    let dir = crate::req_path::project_path(world, "out", out)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| bad("procgen_plant_write_failed", format!("creating `{out}`: {e}")))?;
    let mut files = Vec::new();
    let built = build_plant_variants(
        &plant.compiled,
        &plant.path,
        &|seed| RunInputs::new(plant.snapshot.clone(), seed).with_graphs(plant.subgraphs.clone()),
        (plant.seed, variants),
        &CancelToken::new(),
        &mut |job, file| {
            let written = (hook.write)(job, &dir.join(file))?;
            files.push(json!({
                "path": format!("{}/{file}", out.trim_end_matches('/')),
                "triangles_per_lod": written.triangles_per_lod,
                "bounds": [written.bounds.0, written.bounds.1],
            }));
            Ok(written)
        },
    );
    let BuiltPlants { stem, index } = built.map_err(|e| {
        let reason = match e.code.as_str() {
            "plant_not_meshed" => "procgen_plant_not_meshed",
            "plant_no_export" => "procgen_plant_no_export",
            "plant_bad_name" => "procgen_plant_bad_name",
            "limit_exceeded" => "procgen_limit_exceeded",
            "envelope_empty" => "procgen_envelope_empty",
            "envelope_not_manifold" => "procgen_envelope_not_manifold",
            "envelope_invalid" => "procgen_envelope_invalid",
            "io" | "export_invalid" => "procgen_plant_write_failed",
            _ => "procgen_run_failed",
        };
        bad(reason, e.to_string())
    })?;
    for (file, entry) in files.iter_mut().zip(&index.variants) {
        if let Some(object) = file.as_object_mut() {
            object.insert("seed".to_string(), json!(entry.seed));
        }
    }
    let index_file = format!("{stem}.plants.ron");
    regia_procgen::pgout::write_atomic(&dir.join(&index_file), index.to_ron().as_bytes())
        .map_err(|e| bad("procgen_plant_write_failed", format!("{index_file}: {e}")))?;
    Ok(json!({
        "graph": plant.path,
        "files": files,
        "index": format!("{}/{index_file}", out.trim_end_matches('/')),
        "graph_hash": index.graph_hash,
    }))
}

/// `procgen.plant_stats { graph, seed?, node?, params? }` (spec 68 § 8): runs a Plant graph in
/// memory (nothing is published or written) and answers per-plant statistics of the plant data
/// on `node` (default: the last node that produced plant data).
pub(crate) fn plant_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_procgen::{CancelToken, Evaluator, RunInputs};

    let plant = plant_graph(world, params, "plant_stats")?;
    let (compiled, path, seed) = (&plant.compiled, plant.path.as_str(), plant.seed);
    let mut inputs = RunInputs::new(plant.snapshot.clone(), seed).with_graphs(plant.subgraphs.clone());
    inputs.keep_debug = true;
    let r = Evaluator::new()
        .run(compiled, &inputs, &CancelToken::new())
        .map_err(|e| {
            let why = if e.code() == "limit_exceeded" {
                "procgen_limit_exceeded"
            } else {
                "procgen_run_failed"
            };
            bad(why, format!("run failed: {e}"))
        })?;
    let debug = r.debug.unwrap_or_default();
    let plant_item = |outs: &Vec<regia_procgen::data::Bundle>| {
        outs.iter()
            .flat_map(|b| &b.0)
            .find(|i| matches!(i.payload, Payload::PlantSkeleton(_) | Payload::PlantMesh(_)))
            .cloned()
    };
    let (node, item) = match crate::params::opt_u64(params, "node")? {
        Some(n) => {
            let id = u32::try_from(n).map_err(|_| bad("procgen_unknown_node", "`node` is a node id"))?;
            let outs = debug
                .get(&id)
                .ok_or_else(|| missing("procgen_unknown_node", format!("no node {id}")))?;
            let item = plant_item(outs).ok_or_else(|| {
                missing(
                    "procgen_unknown_node",
                    format!("node {id} produced no plant data"),
                )
            })?;
            (id, item)
        }
        None => compiled
            .nodes
            .iter()
            .rev()
            .find_map(|n| debug.get(&n.id).and_then(plant_item).map(|i| (n.id, i)))
            .ok_or_else(|| missing("procgen_unknown_node", "the graph produced no plant data"))?,
    };
    let plants = match &item.payload {
        Payload::PlantSkeleton(s) => plant_stats_json(s, None),
        Payload::PlantMesh(m) => plant_stats_json(&m.skeleton, Some(m)),
        _ => json!([]),
    };
    Ok(json!({
        "graph": path,
        "seed": seed,
        "node": node,
        "kind": item.payload.kind().name(),
        "plants": plants,
        "exports": r.output.exports.iter().map(|j| json!({ "node": j.node, "out": j.out, "status": "not_written" })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_procgen::world::ProcGenPlugin;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(BTreeMap::new()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app
    }

    fn reason_of(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// A host that writes through the asset pipeline, with its project in a temp directory.
    fn plant_app(files: BTreeMap<String, String>) -> (App, std::path::PathBuf) {
        let dir = regia_test_utils::unique_temp_dir("agent_plant_build");
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.insert_resource(PlantExportHook {
            write: regia_asset_pipeline::plant_export::host_write,
            load_mesh: regia_asset_pipeline::plant_export::host_load_mesh,
        });
        (app, dir)
    }

    /// The JSON chunk of a `.glb`.
    fn glb_json(bytes: &[u8]) -> Value {
        let len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        serde_json::from_slice(&bytes[20..20 + len]).expect("JSON chunk")
    }

    /// A cell-scheduled generator does not take a whole-graph run id (spec 66 § 4.2a).
    #[test]
    fn run_on_a_scheduled_generator_requeues_cells() {
        use regia_procgen::tiles::{
            AnchorSettings, GeneratorTiles, ProcGenTiles, TileEntry, TileKey, TileSettings, TileState,
        };
        use regia_procgen::world::{ProcGenMode, ProcGenQueue, ProcGenRunState};

        let mut app = app();
        let w = app.world_mut();
        // `job` is crate-private, so the state is built from `Default` and the public fields.
        let mut proc_state = ProcGenState::default();
        proc_state.state = ProcGenRunState::Queued;
        proc_state.run_id = 4;
        let entity = w
            .spawn((
                ProcGenerator {
                    mode: ProcGenMode::Anchored,
                    tiles: Some(TileSettings {
                        runtime: Some(AnchorSettings::default()),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                proc_state,
            ))
            .id();
        w.resource_mut::<ProcGenQueue>().0.push(entity);
        let mut tiles = GeneratorTiles::new([0.0, 0.0, 128.0, 128.0], Vec::new());
        tiles.entries.insert(
            TileKey::default(),
            TileEntry {
                state: TileState::Resident,
                priority: 0.0,
                queued_at: 0,
                elements: 1,
                applied: 1,
                bytes: 0,
                ms: 0.0,
                output_hash: None,
            },
        );
        w.resource_mut::<ProcGenTiles>().generators.insert(entity, tiles);
        for wait in [false, true] {
            let got = run(w, &json!({ "entity": entity.to_bits(), "wait": wait })).expect("run");
            assert_eq!(got["run_id"], 4, "wait={wait}");
            assert_ne!(got["state"], "queued", "wait={wait}");
            assert_eq!(got["scheduled"], true);
            assert_eq!(got["run_requested"], false);
            assert_eq!(got["requeued"], json!([[0, 0, 0]]));
            assert!(w.resource::<ProcGenQueue>().0.is_empty());
            assert_eq!(
                w.get::<ProcGenState>(entity).map(|s| s.state),
                Some(ProcGenRunState::Idle)
            );
            // Put the cell back to resident so the next call re-queues it again.
            w.resource_mut::<ProcGenTiles>()
                .generators
                .get_mut(&entity)
                .expect("tiles")
                .entries
                .get_mut(&TileKey::default())
                .expect("cell")
                .state = TileState::Resident;
        }
    }

    /// `set_seed_salt` stores a u32. A missing or oversized salt is `procgen_param_type` and
    /// changes nothing (spec 62).
    #[test]
    fn set_seed_salt_rejects_a_missing_or_oversized_salt() {
        let mut app = app();
        let w = app.world_mut();
        let added = graph_edit(
            w,
            &json!({ "path": "salt.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.grid" }
            ], "save": true }),
        )
        .expect("node");
        let id = added["added"][0]["id"].as_u64().expect("id");
        for params in [
            json!({ "path": "salt.pgraph.ron", "ops": [{ "op": "set_seed_salt", "id": id }] }),
            json!({ "path": "salt.pgraph.ron", "ops": [
                { "op": "set_seed_salt", "id": id, "salt": 4_294_967_296_u64 }
            ] }),
        ] {
            let err = graph_edit(w, &params).expect_err("refused");
            assert_eq!(reason_of(&err).as_deref(), Some("procgen_param_type"), "{params}");
        }
        let ok = graph_edit(
            w,
            &json!({ "path": "salt.pgraph.ron", "ops": [
                { "op": "set_seed_salt", "id": id, "salt": 7 }
            ], "save": true }),
        )
        .expect("salt");
        assert_eq!(ok["errors"].as_array().map(Vec::len), Some(0));
        let got = graph_get(w, &json!({ "path": "salt.pgraph.ron" })).expect("get");
        assert_eq!(got["nodes"][0]["seed_salt"], json!(7));
    }

    /// Spec 68 § 8: the shipped starter through `procgen.plant_build`: LODs with strictly decreasing
    /// triangles, the billboard part on the last only, wind attributes on every primitive, capsules
    /// when asked for, and the same bytes on a second run.
    #[test]
    fn plant_build_writes_the_starter_with_lods_billboard_wind_and_capsules() {
        let text = regia_procgen::plant::starter("starter/plant_broadleaf.pgraph.ron")
            .expect("starter")
            .replace(
                "\"name\": \"plant_broadleaf\"",
                "\"name\": \"plant_broadleaf\", \"collision\": \"capsules\"",
            );
        let files: BTreeMap<String, String> = [("plants/b.pgraph.ron".to_string(), text)].into();
        let (mut app, dir) = plant_app(files);
        let w = app.world_mut();
        let request = |out: &str| {
            json!({
                "graph": "plants/b.pgraph.ron",
                "out": out,
                "seed": 7,
                "variants": 2,
            })
        };
        let first = plant_build(w, &request("generated/a")).expect("builds");
        let second = plant_build(w, &request("generated/b/")).expect("builds again");
        let files = first["files"].as_array().expect("files");
        assert_eq!(files.len(), 2);
        assert_eq!(first["index"], "generated/a/plant_broadleaf.plants.ron");
        assert!(dir.join("generated/a/plant_broadleaf.plants.ron").is_file());
        for (k, file) in files.iter().enumerate() {
            assert_eq!(file["seed"], 7 + k as u64);
            let tris: Vec<u64> = file["triangles_per_lod"]
                .as_array()
                .expect("triangles")
                .iter()
                .filter_map(Value::as_u64)
                .collect();
            assert_eq!(tris.len(), 4);
            assert!(
                tris.windows(2).all(|t| t[1] < t[0]),
                "strictly decreasing: {tris:?}"
            );
            let name = format!("plant_broadleaf_{k}.glb");
            assert_eq!(file["path"], format!("generated/a/{name}"));
            let a = std::fs::read(dir.join("generated/a").join(&name)).expect("first file");
            let b = std::fs::read(dir.join("generated/b").join(&name)).expect("second file");
            assert!(a == b, "{name} differs between two runs");
            let doc = glb_json(&a);
            let names: Vec<&str> = doc["materials"]
                .as_array()
                .expect("materials")
                .iter()
                .filter_map(|m| m["name"].as_str())
                .collect();
            let billboard = names
                .iter()
                .position(|m| m.ends_with("_billboard"))
                .expect("a billboard material");
            let mut levels = 0;
            for mesh in doc["meshes"].as_array().expect("meshes") {
                let mesh_name = mesh["name"].as_str().unwrap_or_default();
                let Some(level) = mesh_name
                    .rsplit("_LOD")
                    .next()
                    .filter(|_| mesh_name.contains("_LOD"))
                else {
                    continue;
                };
                levels += 1;
                let mut has_billboard = false;
                for prim in mesh["primitives"].as_array().expect("primitives") {
                    assert!(prim["attributes"]["_WIND"].is_u64(), "{mesh_name}: _WIND");
                    assert!(
                        prim["attributes"]["_WIND_PIVOT"].is_u64(),
                        "{mesh_name}: _WIND_PIVOT"
                    );
                    has_billboard |= prim["material"].as_u64() == Some(billboard as u64);
                }
                assert_eq!(has_billboard, level == "3", "{mesh_name}");
            }
            assert_eq!(levels, 4);
            let capsules = doc["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .filter(|n| n["extras"]["regia_capsule"].is_array())
                .count();
            assert!(capsules >= 1, "the trunk has a capsule");
        }
        assert_eq!(first["graph_hash"], second["graph_hash"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The envelope comes from a project file through the hook; a mesh with no inside answers its
    /// documented reason, and neither method grows a plant without its envelope.
    #[test]
    fn plant_build_reads_an_envelope_file_and_refuses_a_bad_one() {
        use regia_procgen::nodes::Domain;
        use regia_procgen::{Edge, NodeEntry};
        let node = |ty: &str, settings: &[(&str, &str)]| {
            let mut n = NodeEntry::new(ty);
            for (k, v) in settings {
                n = n.with(k, regia_procgen::SettingValue::parse(v).expect("setting"));
            }
            n
        };
        let mut g = ProcGraph {
            nodes: [
                (1, node("gen.one", &[])),
                (2, node("plant.seed", &[])),
                (3, node("plant.envelope_mesh", &[("asset", "\"env/leaky.glb\"")])),
                (4, node("plant.grow", &[("growth_steps", "3")])),
                (5, node("plant.radius", &[])),
                (6, node("plant.mesh", &[])),
                (7, node("plant.export", &[("name", "\"p\"")])),
            ]
            .into(),
            edges: vec![
                Edge::new(1, "out", 2, "in"),
                Edge::new(2, "out", 4, "in"),
                Edge::new(3, "out", 4, "envelope"),
                Edge::new(4, "out", 5, "in"),
                Edge::new(5, "out", 6, "in"),
                Edge::new(6, "out", 7, "in"),
            ],
            ..ProcGraph::default()
        };
        g.flags.domain = Domain::Plant;
        let files: BTreeMap<String, String> = [("e.pgraph.ron".to_string(), g.to_ron())].into();
        let (mut app, dir) = plant_app(files);
        let w = app.world_mut();
        let request = json!({ "graph": "e.pgraph.ron", "out": "generated/e" });
        // No such file: the node has no mesh.
        let e = plant_build(w, &request).expect_err("no file");
        assert_eq!(reason_of(&e).as_deref(), Some("procgen_envelope_empty"));
        // An exported plant is real glTF and has open tubes: it parses, and it has no inside.
        let bush = plant_build(
            w,
            &json!({ "graph": "starter/plant_bush.pgraph.ron", "out": "env", "params": { "growth_steps": 3 } }),
        )
        .expect("a real glb");
        assert!(bush["files"][0]["path"].is_string());
        std::fs::rename(dir.join("env/plant_bush_0.glb"), dir.join("env/leaky.glb")).expect("rename");
        let e = plant_build(w, &request).expect_err("open mesh");
        assert_eq!(reason_of(&e).as_deref(), Some("procgen_envelope_not_manifold"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plant_build_refusals_have_their_reasons() {
        let world_graph = "(format_version: 1, nodes: {}, edges: [])".to_string();
        let no_export = regia_procgen::plant::starter("starter/plant_bush.pgraph.ron")
            .expect("starter")
            .replace("ty: \"plant.export\"", "ty: \"reroute\"");
        let bush = regia_procgen::plant::starter("starter/plant_bush.pgraph.ron").expect("starter");
        assert!(
            bush.contains("\"name\": \"plant_bush\""),
            "the starter names its export"
        );
        // Names that are paths: the export must never leave `out`.
        let escape = std::env::temp_dir().join(format!("regia_plant_escape_{}", std::process::id()));
        let named = |name: &str| bush.replace("\"name\": \"plant_bush\"", &format!("\"name\": {name:?}"));
        let files: BTreeMap<String, String> = [
            ("w.pgraph.ron".to_string(), world_graph),
            ("n.pgraph.ron".to_string(), no_export),
            ("abs.pgraph.ron".to_string(), named(&escape.to_string_lossy())),
            ("up.pgraph.ron".to_string(), named("../../up")),
            ("dots.pgraph.ron".to_string(), named("..")),
        ]
        .into();
        let (mut app, dir) = plant_app(files);
        let w = app.world_mut();
        let reason = |w: &mut World, p: Value| {
            let e = plant_build(w, &p).expect_err("refused");
            reason_of(&e)
        };
        assert_eq!(
            reason(w, json!({ "graph": "nope.pgraph.ron", "out": "o" })).as_deref(),
            Some("procgen_unknown_graph")
        );
        assert_eq!(
            reason(w, json!({ "graph": "w.pgraph.ron", "out": "o" })).as_deref(),
            Some("procgen_wrong_domain")
        );
        assert_eq!(
            reason(
                w,
                json!({ "graph": "starter/plant_bush.pgraph.ron", "out": "../outside" })
            )
            .as_deref(),
            Some("path_outside_project")
        );
        assert_eq!(
            reason(
                w,
                json!({ "graph": "starter/plant_bush.pgraph.ron", "out": "o", "variants": 0 })
            )
            .as_deref(),
            Some("procgen_param_type")
        );
        assert!(plant_build(w, &json!({ "graph": "starter/plant_bush.pgraph.ron" })).is_err());
        for graph in ["abs.pgraph.ron", "up.pgraph.ron", "dots.pgraph.ron"] {
            assert_eq!(
                reason(
                    w,
                    json!({ "graph": graph, "out": "o/deep", "params": { "growth_steps": 2 } })
                )
                .as_deref(),
                Some("procgen_plant_bad_name"),
                "{graph}"
            );
        }
        // `o/deep` + `../../up` would land at the project root; an absolute name beside `escape`.
        assert!(!std::path::PathBuf::from(format!("{}_0.glb", escape.display())).exists());
        assert!(!dir.join("up_0.glb").exists() && !dir.join("up.plants.ron").exists());
        w.remove_resource::<PlantExportHook>();
        assert_eq!(
            reason(w, json!({ "graph": "starter/plant_bush.pgraph.ron", "out": "o" })).as_deref(),
            Some("procgen_plant_host_required")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `procgen.run { threads }`: the schedule of one run, restored afterward, same output.
    #[test]
    fn run_threads_sets_the_schedule_of_that_run_and_restores_it() {
        let mut app = app();
        let w = app.world_mut();
        graph_edit(
            w,
            &json!({ "path": "g.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.grid", "settings": { "count": { "ron": "(8.0, 1.0, 8.0)" } } },
                { "op": "add_node", "ty": "density.remap", "settings": {} },
                { "op": "add_node", "ty": "out.points", "settings": {} },
                { "op": "connect", "from": [1, "out"], "to": [2, "in"] },
                { "op": "connect", "from": [2, "out"], "to": [3, "in"] },
            ], "save": true }),
        )
        .expect("edit");
        let e = w
            .spawn((
                ProcGenerator::new("g.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(9)),
            ))
            .id();
        let mut hashes = Vec::new();
        for threads in [json!(null), json!(0), json!(1), json!(2), json!(8)] {
            let r = run(
                w,
                &json!({ "entity": e.to_bits(), "wait": true, "threads": threads.clone() }),
            )
            .expect("run");
            assert_eq!(r["state"], "ready");
            assert_eq!(w.resource::<ProcGenSettings>().threads, None, "restored");
            if let Some(n) = threads.as_u64() {
                let st = w.get::<ProcGenState>(e).expect("state");
                let report = st.report.as_ref().expect("report");
                assert_eq!(report.threads as u64, n.max(1));
            }
            hashes.push(w.get::<ProcGenState>(e).and_then(|s| s.output_hash));
        }
        assert!(
            hashes[0].is_some() && hashes.windows(2).all(|h| h[0] == h[1]),
            "{hashes:?}"
        );
        let refused = run(
            w,
            &json!({ "entity": e.to_bits(), "wait": true, "threads": 257, "seed": 5 }),
        )
        .expect_err("too many");
        assert_eq!(reason_of(&refused).as_deref(), Some("procgen_param_type"));
        // A refused call stores nothing: the seed it carried is not kept.
        assert_eq!(w.get::<ProcGenerator>(e).and_then(|g| g.seed), None);
        // corr-agent (ADR-5597): a bad `seed` (or `wait`) was checked after `params` was stored.
        for bad_tail in [json!({ "seed": -1 }), json!({ "wait": "yes" })] {
            let mut call = json!({ "entity": e.to_bits(), "params": { "count": 3.0 } });
            if let (Some(call), Some(tail)) = (call.as_object_mut(), bad_tail.as_object()) {
                call.extend(tail.clone());
            }
            let refused = run(w, &call).expect_err("refused");
            let key = bad_tail
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .unwrap_or_default();
            assert!(refused.message.contains(&key), "refused for `{key}`: {refused:?}");
            assert!(
                w.get::<ProcGenerator>(e)
                    .is_some_and(|g| !g.params.contains_key("count")),
                "a refused run stores no params: {call}"
            );
        }
    }

    /// ADR-2421: `procgen.run` refuses an `editor_only` generator outside the editor;
    /// `procgen.load_report` follows an `OnLoad` generator's load; `procgen.get` reports the runtime
    /// side (bounds, colliders, files).
    #[test]
    fn runtime_gaps_surface_through_the_agent() {
        let mut app = app();
        let w = app.world_mut();
        graph_edit(
            w,
            &json!({ "path": "g.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.grid", "settings": { "count": { "ron": "(2.0, 1.0, 1.0)" } } },
                { "op": "add_node", "ty": "spawn.mesh", "settings": { "entries": { "ron": "[(mesh: \"m.glb\", collision: \"Simple\")]" } } },
                { "op": "connect", "from": [1, "out"], "to": [2, "in"] },
            ], "save": true }),
        )
        .expect("edit");
        let editor_only = w
            .spawn((
                ProcGenerator {
                    editor_only: true,
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(7)),
            ))
            .id();
        let refused = run(w, &json!({ "entity": editor_only.to_bits(), "wait": true })).expect_err("refused");
        assert_eq!(reason_of(&refused).as_deref(), Some("procgen_editor_only"));
        let got = get(w, &json!({ "entity": editor_only.to_bits() })).expect("get");
        assert_eq!(got["editor_only_refused"], true);

        let report = load_report(w, &json!({})).expect("report");
        assert_eq!(report["pending"], false);
        assert!(report["last"].is_null());
        let on_load = w
            .spawn((
                ProcGenerator {
                    mode: regia_procgen::world::ProcGenMode::OnLoad,
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(8)),
            ))
            .id();
        app.update();
        let w = app.world_mut();
        let report = load_report(w, &json!({})).expect("report");
        assert_eq!(report["pending"], true, "{report}");
        assert_eq!(report["waiting"][0]["mode"], "on_load");
        app.update();
        let w = app.world_mut();
        let report = load_report(w, &json!({})).expect("report");
        assert_eq!(report["pending"], false, "{report}");
        assert_eq!(report["last"]["ok"], true);
        assert_eq!(report["last"]["generators"][0]["state"], "ready");
        let got = get(w, &json!({ "entity": on_load.to_bits() })).expect("get");
        assert_eq!(got["colliders"][0]["kind"], "simple");
        assert_eq!(got["colliders"][0]["instances"], 2);
        assert!(
            got["files"]
                .as_array()
                .is_some_and(|f| f.iter().any(|x| x["path"] == "g.pgraph.ron"))
        );
        assert!(got["bounds"]["min"].is_array());
    }

    /// `procgen.list` and `procgen.get` through the dispatcher report the generator's data layers
    /// and its presence (ADR-7214): `live` without a membership, the layer's effective state with one.
    #[test]
    fn list_and_get_report_layers_and_presence_through_the_dispatcher() {
        use regia_scene::layers::{LayerMembership, LayerPeers, LayerState, LayerTable, Side};
        let mut app = app();
        let w = app.world_mut();
        let mut table = LayerTable::default();
        table.insert_layer("quest", None, LayerState::Live, LayerPeers::Everyone);
        table
            .request("quest", LayerState::Parked, false, Side::Local)
            .expect("known layer");
        w.insert_resource(table);
        let member = w
            .spawn((
                ProcGenerator::new("a.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(1)),
                LayerMembership {
                    layers: vec!["quest".into()],
                },
            ))
            .id();
        w.spawn((
            ProcGenerator::new("b.pgraph.ron"),
            EntityGuid(regia_core::id::Guid(2)),
        ));
        let listed = crate::dispatch::call_in_process(w, "procgen.list", json!({}))
            .expect("synchronous method")
            .expect("list");
        let rows = listed["generators"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["layers"], json!(["quest"]));
        assert_eq!(rows[0]["presence"], "parked");
        assert_eq!(rows[1]["layers"], json!([]));
        assert_eq!(rows[1]["presence"], "live");
        let got = crate::dispatch::call_in_process(w, "procgen.get", json!({ "entity": member.to_bits() }))
            .expect("synchronous method")
            .expect("get");
        assert_eq!(got["presence"], "parked");
    }

    /// `procgen.list` and `procgen.get` through the dispatcher name where each loaded bake came from
    /// and, per level, how many bakes loaded from disk and from the pak (ADR-7262).
    #[test]
    fn list_and_get_name_the_levels_whose_bakes_loaded_through_the_dispatcher() {
        use regia_procgen::world::{ProcBakeOrigin, ProcBakeSource, ProcLevelBakeDirs};
        use regia_scene::level::LevelId;
        let mut app = app();
        let w = app.world_mut();
        let sub = LevelId(regia_core::id::Guid(0x5b2));
        w.resource_mut::<ProcLevelBakeDirs>()
            .0
            .insert(sub, std::path::PathBuf::from("assets/levels/sub.procgen"));
        w.spawn((
            ProcGenerator::new("a.pgraph.ron"),
            EntityGuid(regia_core::id::Guid(1)),
            ProcBakeOrigin {
                path: "assets/levels/main.procgen/1.pgout".into(),
                level: None,
                source: ProcBakeSource::Disk,
            },
        ));
        let placed = w
            .spawn((
                ProcGenerator::new("b.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(2)),
                ProcBakeOrigin {
                    path: "assets/levels/sub.procgen/6e6.pgout".into(),
                    level: Some(sub),
                    source: ProcBakeSource::Pak,
                },
            ))
            .id();
        w.spawn((
            ProcGenerator::new("c.pgraph.ron"),
            EntityGuid(regia_core::id::Guid(3)),
        ));
        let listed = crate::dispatch::call_in_process(w, "procgen.list", json!({}))
            .expect("synchronous method")
            .expect("list");
        let rows = listed["generators"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["bake"]["source"], "disk");
        assert_eq!(rows[0]["bake"]["level"], Value::Null);
        assert_eq!(rows[1]["bake"]["source"], "pak");
        assert_eq!(rows[1]["bake"]["level"], sub.0.to_string());
        assert_eq!(rows[2]["bake"], Value::Null, "nothing loaded");
        let levels = listed["bake_levels"].as_array().cloned().unwrap_or_default();
        assert_eq!(levels.len(), 2, "{levels:?}");
        assert_eq!(levels[0]["level"], Value::Null);
        assert_eq!(levels[0]["disk"], 1);
        assert_eq!(levels[1]["level"], sub.0.to_string());
        assert_eq!(levels[1]["dir"], "assets/levels/sub.procgen");
        assert_eq!(levels[1]["pak"], 1);
        let got = crate::dispatch::call_in_process(w, "procgen.get", json!({ "entity": placed.to_bits() }))
            .expect("synchronous method")
            .expect("get");
        assert_eq!(got["bake"]["path"], "assets/levels/sub.procgen/6e6.pgout");
        assert_eq!(got["bake"]["source"], "pak");
    }

    /// `procgen.get` and `procgen.list` through the dispatcher name the directory an anchored
    /// generator of a placed level streamed its cells from, with `kind: cells` (ADR-7287).
    #[test]
    fn get_names_the_directory_an_anchored_generator_streams_cells_from_through_the_dispatcher() {
        use regia_procgen::tiles::{AnchorSettings, TileSettings};
        use regia_procgen::world::{ProcBakeOrigin, ProcBakeSource};
        use regia_scene::level::LevelId;
        let mut app = app();
        let w = app.world_mut();
        let sub = LevelId(regia_core::id::Guid(0x5b2));
        let anchored = w
            .spawn((
                ProcGenerator {
                    tiles: Some(TileSettings {
                        runtime: Some(AnchorSettings::default()),
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("s.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(7)),
                ProcBakeOrigin {
                    path: "assets/levels/sub.procgen/7c7.pgout".into(),
                    level: Some(sub),
                    source: ProcBakeSource::Disk,
                },
            ))
            .id();
        let whole = w
            .spawn((
                ProcGenerator::new("a.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(8)),
                ProcBakeOrigin {
                    path: "assets/levels/main.procgen/8.pgout".into(),
                    level: None,
                    source: ProcBakeSource::Pak,
                },
            ))
            .id();
        let got = crate::dispatch::call_in_process(w, "procgen.get", json!({ "entity": anchored.to_bits() }))
            .expect("synchronous method")
            .expect("get");
        assert_eq!(got["bake"]["kind"], "cells");
        assert_eq!(got["bake"]["dir"], "assets/levels/sub.procgen");
        assert_eq!(got["bake"]["level"], sub.0.to_string());
        let got = crate::dispatch::call_in_process(w, "procgen.get", json!({ "entity": whole.to_bits() }))
            .expect("synchronous method")
            .expect("get");
        assert_eq!(got["bake"]["kind"], "whole");
        assert_eq!(got["bake"]["dir"], "assets/levels/main.procgen");
        let listed = crate::dispatch::call_in_process(w, "procgen.list", json!({}))
            .expect("synchronous method")
            .expect("list");
        let rows = listed["generators"].as_array().cloned().unwrap_or_default();
        let kinds: Vec<&str> = rows.iter().filter_map(|r| r["bake"]["kind"].as_str()).collect();
        assert_eq!(kinds, ["cells", "whole"], "{rows:?}");
    }

    #[test]
    fn graph_edit_builds_a_graph_runs_it_and_inspects_a_node() {
        let mut app = app();
        let w = app.world_mut();
        let edit = graph_edit(
            w,
            &json!({ "path": "g.pgraph.ron", "ops": [
                { "op": "add_param", "name": "n", "ty": "Float", "default": 3.0 },
                { "op": "add_node", "ty": "gen.grid", "settings": { "count": { "ron": "(4.0, 1.0, 2.0)" } } },
                { "op": "add_node", "ty": "points.keep_random", "settings": { "ratio": 1.0 } },
                { "op": "add_node", "ty": "spawn.mesh", "settings": { "entries": { "ron": "[(mesh: \"m.glb\")]" } } },
                { "op": "connect", "from": [1, "out"], "to": [2, "in"] },
                { "op": "connect", "from": [2, "out"], "to": [3, "in"] },
            ], "save": true }),
        )
        .expect("edit");
        assert_eq!(edit["added"].as_array().map(Vec::len), Some(3));
        let got = graph_get(w, &json!({ "path": "g.pgraph.ron" })).expect("get");
        assert_eq!(got["nodes"].as_array().map(Vec::len), Some(3));
        assert_eq!(got["graph_hash"], edit["graph_hash"]);

        let e = w
            .spawn((
                ProcGenerator::new("g.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(5)),
            ))
            .id();
        let r = run(
            w,
            &json!({ "entity": e.to_bits(), "wait": true, "keep_debug": true }),
        )
        .expect("run");
        assert_eq!(r["state"], "ready");
        assert_eq!(r["counts"]["instances"], 8);
        let listed = list(w, &json!({})).expect("list");
        assert_eq!(listed["generators"][0]["state"], "ready");
        let ins = inspect(w, &json!({ "entity": e.to_bits(), "node": 1, "limit": 2 })).expect("inspect");
        assert_eq!(ins["items"][0]["len"], 8);
        assert_eq!(
            ins["items"][0]["rows"]["position"].as_array().map(Vec::len),
            Some(2)
        );
        let rep = report(w, &json!({ "entity": e.to_bits() })).expect("report");
        assert_eq!(rep["nodes"].as_array().map(Vec::len), Some(3));
        let types = node_types(w, &json!({ "category": "sample" })).expect("types");
        assert!(types["count"].as_u64().unwrap_or(0) >= 3);
    }

    #[test]
    fn detach_is_editor_only_and_converts_the_output() {
        let mut app = app();
        let w = app.world_mut();
        graph_edit(
            w,
            &json!({ "path": "d.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.grid", "settings": { "count": { "ron": "(2.0, 1.0, 1.0)" } } },
                { "op": "add_node", "ty": "spawn.mesh", "settings": { "entries": { "ron": "[(mesh: \"m.glb\")]" } } },
                { "op": "connect", "from": [1, "out"], "to": [2, "in"] },
            ], "save": true }),
        )
        .expect("edit");
        let e = w
            .spawn((
                ProcGenerator::new("d.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(9)),
            ))
            .id();
        run(w, &json!({ "entity": e.to_bits(), "wait": true })).expect("run");
        let refused = detach(w, &json!({ "entity": e.to_bits() })).expect_err("runtime");
        assert_eq!(reason_of(&refused).as_deref(), Some("procgen_editor_only"));
        w.resource_mut::<ProcGenSettings>().editor = true;
        let d = detach(w, &json!({ "entity": e.to_bits() })).expect("detach");
        assert_eq!(d["instance_sets"].as_array().map(Vec::len), Some(1));
        let again = detach(w, &json!({ "entity": e.to_bits() })).expect_err("nothing left");
        assert_eq!(reason_of(&again).as_deref(), Some("procgen_no_output"));
    }

    /// Spec 62 § 10 agent test: `procgen.run { wait: true }` on the acceptance generator (the
    /// "rocks on slopes" graph over the 256 x 256 bowl, seed 7, a 200 m box) answers `ready` with the
    /// output hash recorded by `regia_procgen`'s evaluator test (`tests/eval_core.rs` `ROCKS_HASH`): the
    /// ECS host's capture and the agent surface add nothing to the bytes.
    #[test]
    fn run_wait_on_the_acceptance_generator_returns_ready_and_the_recorded_hash() {
        const ROCKS: &str = r#"ProcGraph(
    format_version: 1,
    nodes: {
        1: (ty: "in.self", settings: {}),
        2: (ty: "sample.ground", settings: { "spacing_m": 4.0, "jitter": 0.8, "align": normal }),
        3: (ty: "density.facing", settings: { "lo_deg": 1.5, "hi_deg": 3.0 }),
        4: (ty: "filter.range", settings: { "attr": "density", "min": 0.5, "max": 1.0 }),
        5: (ty: "filter.range", settings: { "attr": "position:y", "min": 0.5, "max": 30.0 }),
        6: (ty: "points.jitter", settings: {
            "rotate_deg": [(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)],
            "scale": [(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)],
        }),
        7: (ty: "spawn.mesh", settings: {
            "entries": [(mesh: "meshes/rock_a.glb", weight: 3.0), (mesh: "meshes/rock_b.glb", weight: 1.0)],
        }),
        8: (ty: "graph.output", settings: { "pin": "out" }),
    },
    edges: [
        (from: (1, "out"), to: (2, "region")), (from: (2, "out"), to: (3, "in")),
        (from: (3, "out"), to: (4, "in")), (from: (4, "inside"), to: (5, "in")),
        (from: (5, "inside"), to: (6, "in")), (from: (6, "out"), to: (7, "in")),
        (from: (7, "out"), to: (8, "in")),
    ],
)"#;
        /// `regia_procgen` `tests/eval_core.rs` `ROCKS_HASH` (spec 62 § 6.3).
        const ROCKS_HASH: &str = "73b3e8cdd925a98b0af9a35d172514c81dc50fd597f1b8797009fcd4cf8437d8";
        // The bowl of `regia_procgen` `tests/common` `bowl_heights`: 2 m per sample, f32 polynomial.
        let n = 256u32;
        let heights: Vec<f32> = (0..n * n)
            .map(|i| {
                let (c, r) = ((i % n) as f32, (i / n) as f32);
                let (dc, dr) = (c - 128.0, r - 128.0);
                0.0012 * dc * dc + 0.0007 * dr * dr + 0.03 * dc
            })
            .collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 510.0, 1.0, 0.0).expect("bowl");

        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(
                [("procgen/rocks.pgraph.ron".to_string(), ROCKS.to_string())].into(),
            ),
            ..ProcGenSettings::default()
        });
        app.insert_resource(regia_procgen::world::ProcGenTerrain(map));
        app.add_plugins(ProcGenPlugin);
        let w = app.world_mut();
        let e = w
            .spawn((
                ProcGenerator {
                    seed: Some(7),
                    bounds: regia_procgen::world::ProcBounds::Box {
                        half_extents: [100.0, 64.0, 100.0].into(),
                    },
                    ..ProcGenerator::new("procgen/rocks.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(0x1234_5678_9abc_def0_1122_3344_5566_7788)),
                regia_scene::components::Name::new("gen"),
            ))
            .id();
        let got = run(w, &json!({ "entity": e.to_bits(), "wait": true })).expect("run");
        assert_eq!(got["state"], "ready", "{got}");
        assert_eq!(got["output_hash"], ROCKS_HASH, "{got}");
        assert!(
            got["counts"]["instances"].as_u64().is_some_and(|n| n > 100),
            "rocks were placed: {got}"
        );
    }

    #[test]
    fn errors_carry_reasons() {
        let mut app = app();
        let w = app.world_mut();
        let bad_connect = graph_edit(
            w,
            &json!({ "path": "x.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.one" },
                { "op": "add_node", "ty": "items.count" },
                { "op": "add_node", "ty": "points.jitter" },
                { "op": "connect", "from": [2, "out"], "to": [3, "in"] },
            ] }),
        )
        .expect_err("kind mismatch");
        assert_eq!(reason_of(&bad_connect).as_deref(), Some("procgen_graph_invalid"));
        let errs = bad_connect
            .data
            .as_ref()
            .map(|d| d["errors"].clone())
            .unwrap_or_default();
        assert!(
            errs.as_array()
                .is_some_and(|a| a.iter().any(|x| x["reason"] == "pin_kind_mismatch")),
            "{errs}"
        );

        let e = w.spawn(ProcGenerator::new("missing.pgraph.ron")).id();
        let none = inspect(w, &json!({ "entity": e.to_bits(), "node": 1 })).expect_err("no debug");
        assert_eq!(reason_of(&none).as_deref(), Some("procgen_no_debug_data"));
        let not_gen = w.spawn_empty().id();
        let ng = get(w, &json!({ "entity": not_gen.to_bits() })).expect_err("not a generator");
        assert_eq!(reason_of(&ng).as_deref(), Some("procgen_not_generator"));
        let mut bare = World::new();
        let un = list(&mut bare, &json!({})).expect_err("no plugin");
        assert_eq!(reason_of(&un).as_deref(), Some("procgen_unavailable"));
    }

    /// Audit L5: `procgen.run` keeps a parameter value only when the graph accepts it, so one bad
    /// value does not break every later run of the generator.
    #[test]
    fn run_with_a_mistyped_param_keeps_the_old_params() {
        let mut app = app();
        let w = app.world_mut();
        graph_edit(
            w,
            &json!({ "path": "p.pgraph.ron", "ops": [
                { "op": "add_param", "name": "n", "ty": "Float", "default": 3.0 },
                { "op": "add_node", "ty": "gen.one" },
            ], "save": true }),
        )
        .expect("edit");
        let e = w
            .spawn((
                ProcGenerator::new("p.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(6)),
            ))
            .id();
        let bad = run(
            w,
            &json!({ "entity": e.to_bits(), "wait": true, "params": { "n": "oops" } }),
        )
        .expect_err("a string for a Float parameter");
        assert!(reason_of(&bad).is_some(), "{bad:?}");
        let kept = w
            .get::<ProcGenerator>(e)
            .map(|g| g.params.clone())
            .unwrap_or_default();
        assert!(!kept.contains_key("n"), "the bad value was stored: {kept:?}");
        let ok = run(w, &json!({ "entity": e.to_bits(), "wait": true })).expect("a later run");
        assert_eq!(ok["state"], "ready");
        // A good value is still stored.
        run(
            w,
            &json!({ "entity": e.to_bits(), "wait": true, "params": { "n": 5.0 } }),
        )
        .expect("a good value");
        let stored = w
            .get::<ProcGenerator>(e)
            .map(|g| g.params.clone())
            .unwrap_or_default();
        assert!(stored.contains_key("n"), "{stored:?}");
    }

    /// A host document the route hook edits (`ProcGraphDocRoute`, spec 67 § 6).
    #[derive(Resource, Default)]
    struct FakeDoc(Option<ProcGraph>, u32);

    #[test]
    fn graph_edit_on_an_open_document_goes_through_the_route() {
        let mut app = app();
        let w = app.world_mut();
        w.insert_resource(FakeDoc(Some(ProcGraph::default()), 0));
        w.insert_resource(ProcGraphDocRoute {
            open: |world, path| {
                Ok((path == "open.pgraph.ron")
                    .then(|| world.get_resource::<FakeDoc>().and_then(|d| d.0.clone()))
                    .flatten())
            },
            commit: |world, _path, graph, save| {
                let mut d = world.resource_mut::<FakeDoc>();
                d.0 = Some(graph);
                d.1 += 1;
                Ok(save)
            },
        });
        let out = graph_edit(
            w,
            &json!({ "path": "open.pgraph.ron", "ops": [{ "op": "add_node", "ty": "gen.one" }], "save": true }),
        )
        .expect("edit");
        assert_eq!(out["document"], true);
        assert_eq!(out["saved"], true);
        let d = w.resource::<FakeDoc>();
        assert_eq!(d.1, 1, "committed once");
        assert_eq!(d.0.as_ref().map(|g| g.nodes.len()), Some(1));
        assert!(
            w.resource::<ProcGraphOverlay>().0.is_empty(),
            "the overlay is not touched"
        );
        // A graph the host does not have open goes to the overlay as before.
        let other = graph_edit(
            w,
            &json!({ "path": "other.pgraph.ron", "ops": [{ "op": "add_node", "ty": "gen.one" }] }),
        )
        .expect("edit");
        assert!(other.get("document").is_none());
        assert!(
            w.resource::<ProcGraphOverlay>()
                .0
                .contains_key("other.pgraph.ron")
        );
    }

    #[test]
    fn a_read_only_route_keeps_its_reason_and_does_not_write() {
        let mut app = app();
        let w = app.world_mut();
        w.insert_resource(FakeDoc(Some(ProcGraph::default()), 0));
        w.insert_resource(ProcGraphDocRoute {
            open: |_world, path| {
                if path == "ro.pgraph.ron" {
                    Err(ProcGraphRouteDeny {
                        reason: "procgen_read_only",
                        message: "a variant is open: the parent graph is read-only",
                    })
                } else if path == "open.pgraph.ron" {
                    Ok(Some(ProcGraph::default()))
                } else {
                    Ok(None)
                }
            },
            commit: |_world, _path, _graph, _save| {
                Err("procgen_read_only: a variant is open: the parent graph is read-only".into())
            },
        });
        let denied = graph_edit(
            w,
            &json!({ "path": "ro.pgraph.ron", "ops": [{ "op": "add_node", "ty": "gen.one" }], "save": true }),
        )
        .expect_err("open deny");
        assert_eq!(reason_of(&denied).as_deref(), Some("procgen_read_only"));
        assert!(w.resource::<ProcGraphOverlay>().0.is_empty(), "no overlay write");
        let committed = graph_edit(
            w,
            &json!({ "path": "open.pgraph.ron", "ops": [{ "op": "add_node", "ty": "gen.one" }], "save": true }),
        )
        .expect_err("commit deny");
        assert_eq!(reason_of(&committed).as_deref(), Some("procgen_read_only"));
        assert_eq!(
            w.resource::<FakeDoc>().1,
            0,
            "commit refused before it stored the graph"
        );
        assert!(w.resource::<ProcGraphOverlay>().0.is_empty(), "no overlay write");
    }

    #[test]
    fn json_and_ron_settings_convert_both_ways() {
        for v in [
            json!(1),
            json!(2.5),
            json!("a"),
            json!({ "bind": "p" }),
            json!([1, 2]),
            json!({ "ron": "ByAttr(attr: \"k\")" }),
        ] {
            let s = json_to_setting(&v).expect("to ron");
            assert_eq!(setting_to_json(&s), v, "{v}");
        }
        // Audit L6: a non-finite default survives the round trip instead of becoming null.
        let inf = setting_to_json(&SettingValue::Float(f64::INFINITY));
        assert!(inf["ron"].is_string(), "{inf}");
        assert_eq!(
            json_to_setting(&inf).expect("back"),
            SettingValue::Float(f64::INFINITY)
        );
    }

    /// A project dir with a `ProcGraphSource::Dir` source and a file outside the project.
    fn dir_app(tag: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let base = regia_test_utils::unique_temp_dir(tag);
        let root = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("secret.pgraph.ron"), ProcGraph::default().to_ron()).expect("secret");
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Dir(root.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        (app, root, outside)
    }

    #[test]
    fn graph_paths_outside_the_project_are_refused() {
        let (mut app, root, outside) = dir_app("procgen-path-escape");
        let w = app.world_mut();
        let abs = outside.join("secret.pgraph.ron").display().to_string();
        let abs_new = outside.join("new.pgraph.ron").display().to_string();
        let ops = json!([{ "op": "add_node", "ty": "gen.one" }]);
        for p in [
            abs.as_str(),
            abs_new.as_str(),
            "/etc/hostname",
            "C:\\Windows\\win.ini",
            "C:/x.pgraph.ron",
            "\\\\server\\share\\x.pgraph.ron",
            "../outside/secret.pgraph.ron",
            "a/../../outside/new.pgraph.ron",
            "a\0b.pgraph.ron",
            "NUL.pgraph.ron",
        ] {
            let e = graph_get(w, &json!({ "path": p })).expect_err(p);
            assert_eq!(reason_of(&e).as_deref(), Some("procgen_bad_path"), "get {p}");
            let e = graph_edit(w, &json!({ "path": p, "ops": ops, "save": true })).expect_err(p);
            assert_eq!(reason_of(&e).as_deref(), Some("procgen_bad_path"), "edit {p}");
        }
        assert!(!outside.join("new.pgraph.ron").exists());
        // The source refuses the same paths for generator, `graph.call` and variant reads.
        let source = w.resource::<ProcGenSettings>().source.clone();
        assert!(source.read(&abs).is_none());
        assert!(source.read("../outside/secret.pgraph.ron").is_none());
        // A plain project-relative save still works and lands under the root.
        graph_edit(
            w,
            &json!({ "path": "sub/g.pgraph.ron", "ops": ops, "save": true }),
        )
        .expect("save");
        assert!(root.join("sub/g.pgraph.ron").is_file());
        assert!(graph_get(w, &json!({ "path": "sub/g.pgraph.ron" })).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn graph_paths_through_a_symlink_out_of_the_project_are_refused() {
        let (mut app, root, outside) = dir_app("procgen-path-symlink");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("link");
        let w = app.world_mut();
        let ops = json!([{ "op": "add_node", "ty": "gen.one" }]);
        let e = graph_edit(
            w,
            &json!({ "path": "link/new.pgraph.ron", "ops": ops, "save": true }),
        )
        .expect_err("symlinked save");
        assert_eq!(reason_of(&e).as_deref(), Some("procgen_bad_path"));
        let e = graph_edit(
            w,
            &json!({ "path": "link/deep/new.pgraph.ron", "ops": ops, "save": true }),
        )
        .expect_err("symlinked save, new dirs");
        assert_eq!(reason_of(&e).as_deref(), Some("procgen_bad_path"));
        assert!(!outside.join("new.pgraph.ron").exists());
        assert!(!outside.join("deep").exists());
        // Reading through the link: the source refuses, so the graph is unknown.
        let e = graph_get(w, &json!({ "path": "link/secret.pgraph.ron" })).expect_err("symlinked read");
        assert_eq!(reason_of(&e).as_deref(), Some("procgen_unknown_graph"));
    }

    /// `procgen.plant_stats` (spec 68 § 8, lane spec-28-68): per plant node count, max order,
    /// height, crown width, branches shed and triangles; `triangles` is the whole mesh for one plant
    /// with a mesh, 0 for a skeleton and 0 on every plant of a clump; jobs are `not_written`.
    #[test]
    fn plant_stats_answers_per_plant_numbers_and_refusals() {
        let world_graph = "(format_version: 1, nodes: {}, edges: [])".to_string();
        let (mut app, dir) = plant_app([("w.pgraph.ron".to_string(), world_graph)].into());
        let w = app.world_mut();
        let stats = |w: &mut World, p: Value| plant_stats(w, &p).expect("plant_stats answers");
        let tree = "starter/plant_broadleaf.pgraph.ron";
        let params = json!({ "growth_steps": 6 });

        // Node 2 is `plant.grow`: a skeleton, so no triangles.
        let skel = stats(
            w,
            json!({ "graph": tree, "seed": 7, "node": 2, "params": params }),
        );
        assert_eq!(skel["kind"], "PlantSkeleton");
        assert_eq!(skel["node"], 2);
        assert_eq!(skel["seed"], 7);
        let plants = skel["plants"].as_array().expect("plants");
        assert_eq!(plants.len(), 1, "one plant per seed point");
        let p = &plants[0];
        assert!(p["nodes"].as_u64().is_some_and(|n| n > 10), "{p}");
        assert!(p["max_order"].as_u64().is_some_and(|o| o >= 1), "{p}");
        assert!(p["height_m"].as_f64().is_some_and(|h| h > 0.5), "{p}");
        assert!(p["crown_width_m"].as_f64().is_some_and(|c| c > 0.0), "{p}");
        assert!(p["branches_shed"].is_u64(), "{p}");
        assert_eq!(p["triangles"], 0, "a skeleton has no triangles");

        // Node 6 is `plant.export`: the meshed plant, and the job is not written by this method.
        let meshed = stats(
            w,
            json!({ "graph": tree, "seed": 7, "node": 6, "params": params }),
        );
        assert_eq!(meshed["kind"], "PlantMesh");
        let m = &meshed["plants"][0];
        assert!(m["triangles"].as_u64().is_some_and(|t| t > 0), "{m}");
        assert_eq!(m["nodes"], p["nodes"], "meshing keeps the skeleton");
        let exports = meshed["exports"].as_array().expect("exports");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["status"], "not_written");

        // Same request, same numbers; another seed, another tree.
        let again = stats(
            w,
            json!({ "graph": tree, "seed": 7, "node": 6, "params": params }),
        );
        assert_eq!(again["plants"], meshed["plants"]);
        let other = stats(
            w,
            json!({ "graph": tree, "seed": 8, "node": 6, "params": params }),
        );
        assert_ne!(other["plants"], meshed["plants"]);

        // A clump of six: triangles 0 on every plant (no per-plant triangle range).
        let bush = stats(
            w,
            json!({ "graph": "starter/plant_bush.pgraph.ron", "seed": 7, "node": 8, "params": params }),
        );
        let plants = bush["plants"].as_array().expect("plants");
        assert_eq!(plants.len(), 6);
        assert!(plants.iter().all(|p| p["triangles"] == 0), "{bush}");

        // Refusals.
        let reason = |w: &mut World, p: Value| reason_of(&plant_stats(w, &p).expect_err("refused"));
        assert_eq!(
            reason(w, json!({ "graph": "nope.pgraph.ron" })).as_deref(),
            Some("procgen_unknown_graph")
        );
        assert_eq!(
            reason(w, json!({ "graph": "w.pgraph.ron" })).as_deref(),
            Some("procgen_wrong_domain")
        );
        assert_eq!(
            reason(w, json!({ "graph": tree, "node": 999, "params": params })).as_deref(),
            Some("procgen_unknown_node")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Acceptance (ADR-7331 (3)) through the dispatcher: `procgen.verify` names each entity that has
    /// a cell bake but lost its `ProcGenerator` (a level saved while ADR-7287 (5) was open) as an
    /// `anchored_generator_missing` finding with its guid and level, in the primary directory and in
    /// a placed level's directory, and rewrites nothing.
    #[test]
    fn verify_names_lost_anchored_generators_through_the_dispatcher() {
        use regia_procgen::exec::RunOutput;
        use regia_procgen::pgout::{PgoutHeader, encode_cells};
        use regia_procgen::world::ProcLevelBakeDirs;
        use regia_scene::level::{LevelId, LevelMember};
        let root = regia_test_utils::unique_temp_dir("agent_procgen_anchored_missing");
        let main_dir = root.join("main.procgen");
        let sub_dir = root.join("sub.procgen");
        let sub = LevelId(regia_core::id::Guid(0x5b2));
        let empty = RunOutput::default();
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 3,
            input_hash: [4; 32],
            output_hash: empty.hash(),
            quality: 5,
        };
        let cells = encode_cells(&header, &[(0, 0, 0, &empty), (0, 1, 0, &empty)]);
        let lost = regia_core::id::Guid(0x7c7);
        let lost_sub = regia_core::id::Guid(0x7c8);
        for (dir, guid) in [(&main_dir, lost), (&sub_dir, lost_sub)] {
            std::fs::create_dir_all(dir).expect("bake dir");
            std::fs::write(dir.join(format!("{guid}.pgout")), &cells).expect("cell bake");
        }
        let mut app = app();
        let w = app.world_mut();
        w.resource_mut::<ProcGenSettings>().bake_dir = Some(main_dir.clone());
        w.resource_mut::<ProcLevelBakeDirs>()
            .0
            .insert(sub, sub_dir.clone());
        w.spawn(EntityGuid(lost));
        w.spawn((EntityGuid(lost_sub), LevelMember::from_level(sub)));
        w.spawn(EntityGuid(regia_core::id::Guid(0x999)));
        let got = crate::dispatch::call_in_process(w, "procgen.verify", json!({}))
            .expect("synchronous method")
            .expect("verify");
        assert_eq!(got["ok"], false, "{got}");
        let findings = got["findings"].as_array().cloned().unwrap_or_default();
        assert_eq!(findings.len(), 2, "{got}");
        assert_eq!(findings[0]["code"], "anchored_generator_missing");
        assert_eq!(findings[0]["guid"], lost.to_string());
        assert_eq!(findings[0]["level"], Value::Null, "the primary directory");
        assert_eq!(findings[1]["code"], "anchored_generator_missing");
        assert_eq!(findings[1]["guid"], lost_sub.to_string());
        assert_eq!(findings[1]["level"], sub.0.to_string());
        assert_eq!(
            findings[1]["path"],
            sub_dir.join(format!("{lost_sub}.pgout")).display().to_string()
        );
        assert_eq!(
            std::fs::read(main_dir.join(format!("{lost}.pgout"))).expect("bake"),
            cells,
            "no rewrite"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod run_threads_tests {
    use serde_json::json;

    use super::run_threads;

    #[test]
    fn run_threads_takes_absent_null_and_integers_up_to_the_cap_only() {
        assert_eq!(run_threads(&json!({})).ok(), Some(None));
        assert_eq!(run_threads(&json!({ "threads": null })).ok(), Some(None));
        assert_eq!(run_threads(&json!({ "threads": 0 })).ok(), Some(Some(0)));
        assert_eq!(run_threads(&json!({ "threads": 256 })).ok(), Some(Some(256)));
        for bad in [json!(257), json!(-1), json!(1.5), json!("2"), json!([2])] {
            let err = run_threads(&json!({ "threads": bad.clone() }));
            assert!(err.is_err(), "{bad}");
        }
    }
}

/// rev-claude-61 finding 4: the v0.5.1 `pattern_too_large` code reaches the agent protocol.
#[cfg(test)]
mod pattern_size_tests {
    use std::collections::BTreeMap;

    use regia_app::prelude::*;
    use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource};
    use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
    use serde_json::{Value, json};

    use super::{get, graph_edit, graph_get, run};

    /// `graph.input → spline.modules { pattern } → graph.output`, as RON.
    fn fence(pattern: &str) -> String {
        let setting = |text: &str| SettingValue::parse(text).expect("setting");
        let modules = r#"{ "post": (mesh: "fence/post.glb", length_m: 0.2) }"#;
        let mut g = ProcGraph::default();
        g.nodes
            .insert(1, NodeEntry::new("graph.input").with("pin", setting("\"in\"")));
        g.nodes.insert(
            2,
            NodeEntry::new("spline.modules")
                .with("modules", setting(modules))
                .with("pattern", SettingValue::Str(pattern.to_string())),
        );
        g.nodes.insert(3, NodeEntry::new("graph.output"));
        g.edges = vec![Edge::new(1, "out", 2, "in"), Edge::new(2, "out", 3, "in")];
        g.to_ron()
    }

    const HUGE: &str = "repeat(repeat(post, 1000000), 1000000)";

    fn reasons(errors: &Value) -> Vec<String> {
        errors
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["reason"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_too_large_literal_pattern_is_named_on_the_agent_surface() {
        let mut files = BTreeMap::new();
        files.insert("ok.pgraph.ron".to_string(), fence("post"));
        files.insert("huge.pgraph.ron".to_string(), fence(HUGE));
        let mut app = App::new();
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        let w = app.world_mut();

        // `procgen.graph_get` lists the compile error with the node's code.
        let ok = graph_get(w, &json!({ "path": "ok.pgraph.ron" })).expect("get");
        assert!(reasons(&ok["errors"]).is_empty(), "{}", ok["errors"]);
        let huge = graph_get(w, &json!({ "path": "huge.pgraph.ron" })).expect("get");
        assert_eq!(
            reasons(&huge["errors"]),
            vec!["pattern_too_large"],
            "{}",
            huge["errors"]
        );

        // `procgen.graph_edit` refuses to write the pattern: `procgen_graph_invalid` with the code.
        let err = graph_edit(
            w,
            &json!({ "path": "ok.pgraph.ron", "ops": [
                { "op": "set_setting", "id": 2, "name": "pattern", "value": HUGE }
            ] }),
        )
        .expect_err("refused");
        let data = err.data.clone().unwrap_or_default();
        assert_eq!(data["reason"], "procgen_graph_invalid", "{data}");
        assert!(
            reasons(&data["errors"]).iter().any(|r| r == "pattern_too_large"),
            "{data}"
        );

        // `procgen.run` fails the run with the compile error, and the message names the code.
        let e = w.spawn(ProcGenerator::new("huge.pgraph.ron")).id();
        let err = run(w, &json!({ "entity": e.to_bits(), "wait": true })).expect_err("fails");
        let data = err.data.clone().unwrap_or_default();
        assert_eq!(data["reason"], "procgen_run_failed", "{data}");
        assert!(err.message.contains("pattern_too_large"), "{}", err.message);
        let got = get(w, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["state"], "failed");
        assert!(
            got["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("pattern_too_large")),
            "{}",
            got["error"]
        );
    }
}
