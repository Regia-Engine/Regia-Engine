// SPDX-License-Identifier: MPL-2.0
//! Agent surface of the world streaming runtime (`docs/specs/78-world-streaming.md` § 17,
//! `book/agents/streaming-runtime.md`): the `stream.*` methods.
//!
//! Errors are `F0002` with `data.reason`: `stream_not_enabled`, `stream_unknown_cell`,
//! `stream_unknown_source`, `stream_unknown_pin`, `stream_source_limit`, `stream_bad_budget`,
//! `stream_dev_only`, `stream_lockstep_stall`, `stream_format_newer`, `stream_rebase_blocked`,
//! `stream_settings_invalid`, `stream_index_corrupt`, `stream_index_decode`, `stream_spawn_failed`,
//! `stream_no_assets`, `stream_not_cooked`, `stream_wait_area_refused`.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Color, Vec3};
use regia_render::gizmos::Gizmos;
use regia_scene::components::EntityGuid;
use regia_scene::level::{LevelId, LoadMode};
use regia_scene::stream_grid::CellCoord;
use regia_scene::stream_plan::SourceId;
use regia_scene::streaming::{
    HlodCut, OriginOffset, Readiness, RebaseBlockers, RebaseHolds, StreamBudgets, StreamFrozen, StreamGates,
    StreamViews, StreamingSource,
};
use regia_world::{
    PinId, PinWait, StreamRuntimeError, StreamStats, StreamTiming, WorldOrigin, WorldStreaming,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most rows of `stream.cells` and `stream.trace`.
const MAX_ROWS: usize = 10_000;

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn hint_for(reason: &str) -> &'static str {
    match reason {
        "stream_not_enabled" => "open a cooked streamed level (regia_world::open_streamed_level)",
        "stream_unknown_cell" => "stream.cells lists the cells of the world",
        "stream_unknown_pin" => "stream.sources lists the pins",
        "stream_source_limit" => {
            "64 sources and pins at most, counting enabled entity sources; unpin or disable one"
        }
        "stream_bad_budget" => "max_inflight and apply_budget_entities must be at least 1",
        "stream_format_newer" => "recook the level with this engine; the always-loaded set is open",
        "stream_settings_invalid" => {
            "the cooked streaming settings failed validation; the level did not open"
        }
        "stream_index_corrupt" => "the world index is corrupt; the level did not open",
        "stream_index_decode" => "the world index did not decode; the level did not open",
        "stream_spawn_failed" => "the always-loaded set did not spawn",
        "stream_no_assets" => "the app has no asset server",
        "stream_not_cooked" => {
            "the level has no cooked world index (never cooked, or its last cook stopped); cook it again"
        }
        "stream_wait_area_refused" => {
            "ask for a smaller radius: the wait's area must fit in 2^22 cells of every open level's grid"
        }
        "stream_rebase_blocked" => {
            "stream.origin names the blocker; the origin stays put until it unregisters"
        }
        _ => "see book/agents/streaming-runtime.md",
    }
}

fn runtime_error(error: &StreamRuntimeError) -> RpcError {
    let reason = error.reason();
    f0002(reason, error.to_string(), hint_for(reason))
}

fn not_enabled() -> RpcError {
    runtime_error(&StreamRuntimeError::NotEnabled)
}

/// The runtime, when a streamed level is open.
fn active(world: &World) -> Result<&WorldStreaming, RpcError> {
    world
        .get_resource::<WorldStreaming>()
        .filter(|s| s.is_active())
        .ok_or_else(not_enabled)
}

/// Methods that change how the runtime behaves exist in development builds only. `allowed` is the
/// build's answer, a parameter so a test can see the refusal.
fn dev_gate(method: &str, allowed: bool) -> Result<(), RpcError> {
    if allowed {
        Ok(())
    } else {
        Err(f0002(
            "stream_dev_only",
            format!("{method} is not available in a shipping build"),
            "run a development build",
        ))
    }
}

fn dev_only(method: &str) -> Result<(), RpcError> {
    dev_gate(method, cfg!(debug_assertions))
}

fn position(params: &Value, name: &str) -> Result<[f64; 3], RpcError> {
    let values = crate::params::opt_array(params, name)?
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, y, z]")))?;
    let mut out = [0.0; 3];
    for (slot, value) in out.iter_mut().zip(values) {
        *slot = value
            .as_f64()
            .filter(|v| v.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must hold finite numbers")))?;
    }
    Ok(out)
}

fn radius(params: &Value, name: &str) -> Result<f64, RpcError> {
    crate::params::opt_f64(params, name)?
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number, not negative")))
}

fn readiness(params: &Value, name: &str, default: Option<Readiness>) -> Result<Readiness, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => default
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is required (sim_ready or visible)"))),
        Some(v) => v
            .as_str()
            .and_then(Readiness::parse)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be sim_ready or visible"))),
    }
}

fn limit(params: &Value) -> Result<usize, RpcError> {
    Ok(crate::params::opt_u64(params, "limit")?
        .map_or(MAX_ROWS, |l| usize::try_from(l).unwrap_or(MAX_ROWS))
        .min(MAX_ROWS))
}

fn cell_param(params: &Value) -> Result<CellCoord, RpcError> {
    let text = params
        .get("cell")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`cell` must be a cell label \"x_z\""))?;
    CellCoord::from_label(text).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn source_label(id: SourceId) -> String {
    match id {
        SourceId::Guid(guid) => regia_core::id::Guid::from_u128(guid).to_string(),
        SourceId::Entity(bits) => format!("entity:{bits}"),
        SourceId::Pin(pin) => format!("pin:{pin}"),
        SourceId::Implicit => "implicit".to_owned(),
    }
}

fn budgets_json(budgets: &StreamBudgets) -> Value {
    json!({
        "max_loads_per_step": budgets.max_loads_per_step,
        "max_inflight": budgets.max_inflight,
        "max_pending_bytes": budgets.max_pending_bytes,
        "apply_budget_ms": budgets.apply_budget_ms,
        "apply_budget_entities": budgets.apply_budget_entities,
        "memory_budget_bytes": budgets.memory_budget_bytes,
        "vram_budget_bytes": budgets.vram_budget_bytes,
        "hlod_budget_bytes": budgets.hlod_budget_bytes,
    })
}

fn stats_json(stats: &StreamStats) -> Value {
    json!({
        "step": stats.step,
        "pending_waits": stats.pending_waits,
        "resident_cells": stats.resident_cells,
        "resident_bytes_estimate": stats.resident_bytes_estimate,
        "resident_bytes_measured": stats.resident_bytes_measured,
        "resident_vram_estimate": stats.resident_vram_estimate,
        "vram_over_budget": stats.vram_over_budget,
        "inflight": stats.inflight,
        "pending_bytes": stats.pending_bytes,
        "loads_issued": stats.loads_issued,
        "loads_cancelled": stats.loads_cancelled,
        "stale_discards": stats.stale_discards,
        "failures": stats.failures,
        "holes": stats.holes,
        "frozen_bodies": stats.frozen_bodies,
        "source_frozen_steps": stats.source_frozen_steps,
        "main_thread_io_waits": stats.main_thread_io_waits,
        "apply_over_budget": stats.apply_over_budget,
        "rebases": stats.rebases,
        "rebases_blocked": stats.rebases_blocked,
        "teleports": stats.teleports,
        "plans": stats.plans,
        "lockstep_stalls": stats.lockstep_stalls,
        "sources_dropped": stats.sources_dropped,
        "last_apply_ms": stats.last_apply_ms,
        "last_apply_entities": stats.last_apply_entities,
        "last_plan_ms": stats.last_plan_ms,
        "plan_cells_evaluated": stats.plan_cells_evaluated,
        "peak_plan_cells": stats.peak_plan_cells,
        "peak_inflight": stats.peak_inflight,
        "peak_pending_bytes": stats.peak_pending_bytes,
        "peak_apply_entities": stats.peak_apply_entities,
        "peak_loads_per_step": stats.peak_loads_per_step,
        "trace_hash": stats.trace_hash.to_string(),
    })
}

/// The [`StreamStats`] predicate snapshot of `play.wait_until` and `play.expect`: the published
/// resource, which also answers (all zero) before any streamed level opened.
pub(crate) fn stats_snapshot(world: &World) -> Value {
    world
        .get_resource::<StreamStats>()
        .map_or_else(|| stats_json(&StreamStats::default()), stats_json)
}

/// `stream.open`: opens a cooked streamed level from the mounted paks and the cooked cache.
pub(crate) fn open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("level")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`level` (the level's asset guid) is required"))?;
    let guid: regia_core::id::Guid = text
        .parse()
        .map_err(|_| RpcError::invalid_params("`level` must be a guid"))?;
    let mode = match crate::params::opt_str(params, "mode")? {
        None | Some("replace") => LoadMode::Replace,
        Some("additive") => LoadMode::Additive,
        Some(other) => return Err(RpcError::invalid_params(format!("`{other}` is not a load mode"))),
    };
    let entities = regia_world::open_streamed_level(world, LevelId(guid), mode).map_err(|error| {
        let reason = error.reason();
        // A packaged game whose chunk holding the level is not mounted: name the chunk (ADR-8862).
        match error.unmounted_chunk() {
            Some(chunk) => f0002(
                reason,
                error.to_string(),
                &format!("content chunk `{chunk}` holds the level and is not mounted: content.request it, then content.mount"),
            ),
            None => f0002(reason, error.to_string(), hint_for(reason)),
        }
    })?;
    Ok(json!({ "level": text, "always_loaded_entities": entities }))
}

/// `stream.status`: whether streaming is on, the timing, cell counts by state, budgets and counters.
/// With no streamed level it answers `{ enabled: false }`.
pub(crate) fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Ok(streaming) = active(world) else {
        return Ok(json!({ "enabled": false }));
    };
    let level = streaming.first_level().map_err(|e| runtime_error(&e))?;
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (_, state) in streaming.cells(level) {
        *counts.entry(state.as_str()).or_insert(0) += 1;
    }
    let origin = world.get_resource::<WorldOrigin>().copied().unwrap_or_default();
    let terrain_tiles = world
        .get_resource::<regia_terrain::TerrainTileStream>()
        .map(|stream| {
            let stats = stream.stats();
            json!({
                "resident": stats.resident,
                "pending": stats.pending,
                "failed": stats.failed,
                "bytes": stats.bytes,
                "budget": stats.budget,
            })
        });
    Ok(json!({
        "enabled": true,
        "level": level.0.to_string(),
        "terrain_tiles": terrain_tiles,
        "timing": streaming.timing().as_str(),
        "suspended": streaming.is_suspended(),
        "origin_cell": origin.0.cell(),
        "cells": counts,
        "budgets": streaming.budgets(level).map(budgets_json),
        "stats": stats_json(streaming.stats()),
        "vram_bound": streaming.vram_bound(),
        "render_residency": residency_json(world),
        "collision_tiles": collision_tiles_json(world),
    }))
}

/// The render gate's residency switch and counters (ADR-2317), or null without a renderer.
fn residency_json(world: &World) -> Value {
    world
        .get_resource::<regia_render::StreamRenderResidency>()
        .map_or(Value::Null, |r| {
            json!({
                "on": r.enabled,
                "waits": r.waits,
                "waiting_cells": r.waiting_cells,
                "timeouts": r.timeouts,
            })
        })
}

/// The per-tile terrain collision counters (ADR-2317), or null without physics.
fn collision_tiles_json(world: &World) -> Value {
    world
        .get_resource::<regia_physics::TerrainTileStats>()
        .map_or(Value::Null, |t| {
            json!({
                "terrains": t.terrains,
                "resident_tiles": t.resident_tiles,
                "pending_tiles": t.pending_tiles,
                "built": t.built,
                "removed": t.removed,
                "held_cells": t.held_cells,
            })
        })
}

/// `stream.bounds_set`: turns the VRAM planner bound and the render gate's residency wait on or off
/// (ADR-2317). Both are off by default and apply to levels opened later too, so this works without
/// a streamed level.
pub(crate) fn bounds_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let flag = |name: &str| -> Result<Option<bool>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_bool()
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean"))),
        }
    };
    let (vram, residency) = (flag("vram_bound")?, flag("render_residency")?);
    if let Some(on) = vram {
        let mut streaming = world
            .get_resource_mut::<WorldStreaming>()
            .ok_or_else(not_enabled)?;
        streaming.set_vram_bound(on);
    }
    if let Some(on) = residency {
        let mut r = world
            .get_resource_mut::<regia_render::StreamRenderResidency>()
            .ok_or_else(|| RpcError::invalid_params("`render_residency` needs the render plugin"))?;
        r.enabled = on;
    }
    Ok(json!({
        "vram_bound": world.get_resource::<WorldStreaming>().is_some_and(|s| s.vram_bound()),
        "render_residency": residency_json(world),
    }))
}

/// `stream.collision_tiles`: the per-tile terrain collision counters (ADR-2317).
pub(crate) fn collision_tiles(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(collision_tiles_json(world))
}

/// `stream.cells`: the cells with their state, sizes, entity count and missing gates.
pub(crate) fn cells(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let streaming = active(world)?;
    let level = streaming.first_level().map_err(|e| runtime_error(&e))?;
    let gates = world.get_resource::<StreamGates>();
    let wanted = crate::params::opt_str(params, "state")?;
    let near = match params.get("near") {
        None | Some(Value::Null) => None,
        Some(_) => Some((position(params, "near")?, radius(params, "radius_m")?)),
    };
    let settings = streaming.settings(level).ok_or_else(not_enabled)?;
    let grid = settings
        .grid()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let cap = limit(params)?;
    let mut rows = Vec::new();
    for (cell, state) in streaming.cells(level) {
        if rows.len() >= cap {
            break;
        }
        if wanted.is_some_and(|w| w != state.as_str()) {
            continue;
        }
        if let Some((center, r)) = near
            && grid.distance_to_cell(center[0], center[2], cell) > r
        {
            continue;
        }
        let record = streaming.record(level, cell);
        let missing: Vec<&str> = if state.is_spawned() {
            gates.map(|g| g.missing(level, cell)).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut row = json!({
            "cell": cell.label(),
            "state": state.as_str(),
            "bytes_estimate": record.map_or(0, |r| r.own_bytes + r.dep_bytes),
            "bytes_measured": streaming.measured_bytes(level, cell),
            "entities": record.map_or(0, |r| r.entity_count),
            "missing_gates": missing,
        });
        if let regia_world::CellState::Failed(reason) = &state
            && let Some(object) = row.as_object_mut()
        {
            object.insert("reason".into(), json!(reason));
        }
        rows.push(row);
    }
    Ok(Value::Array(rows))
}

/// `stream.sources`: every sampled source and pin.
pub(crate) fn sources(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    active(world)?;
    let views = world
        .get_resource::<StreamViews>()
        .map(|v| v.0.clone())
        .unwrap_or_default();
    let mut query = world.query::<(
        Entity,
        Option<&EntityGuid>,
        &StreamingSource,
        Option<&StreamFrozen>,
    )>();
    let mut details: BTreeMap<SourceId, (&'static str, bool)> = BTreeMap::new();
    for (entity, guid, source, frozen) in query.iter(world) {
        let id = guid.map_or(SourceId::Entity(entity.to_bits()), |g| {
            SourceId::Guid(g.0.as_u128())
        });
        details.insert(id, (source.kind.as_str(), frozen.is_some()));
    }
    let mut rows = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for view in views {
        if !seen.insert(view.source) {
            continue;
        }
        let (kind, frozen) = match view.source {
            SourceId::Pin(_) => ("pin", false),
            SourceId::Implicit => ("camera", false),
            other => details.get(&other).copied().unwrap_or(("scripted", false)),
        };
        rows.push(json!({
            "id": source_label(view.source),
            "kind": kind,
            "pos": view.pos,
            "radius_scale": view.radius_scale,
            "priority": view.priority,
            "frozen": frozen,
        }));
    }
    Ok(Value::Array(rows))
}

fn runtime_mut(world: &mut World) -> Result<Mut<'_, WorldStreaming>, RpcError> {
    active(world)?;
    world.get_resource_mut::<WorldStreaming>().ok_or_else(not_enabled)
}

/// `stream.pin`: adds a scripted source, optionally with a wait.
pub(crate) fn pin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let center = position(params, "center")?;
    let name = crate::params::opt_str(params, "name")?.unwrap_or("agent");
    let scale = crate::params::opt_f64(params, "radius_scale")?.unwrap_or(1.0) as f32;
    let priority = crate::params::opt_i64(params, "priority")?
        .map_or(0, |p| p.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
    let ttl = crate::params::opt_u64(params, "ttl_steps")?.map(|t| u32::try_from(t).unwrap_or(u32::MAX));
    let wait = match params.get("wait") {
        None | Some(Value::Null) => None,
        Some(w) => Some(PinWait {
            radius_m: radius(w, "radius_m")?,
            readiness: readiness(w, "readiness", Some(Readiness::SimReady))?,
        }),
    };
    let enabled_sources = {
        let mut query = world.query::<&StreamingSource>();
        query.iter(world).filter(|source| source.enabled).count()
    };
    let mut streaming = runtime_mut(world)?;
    streaming.note_enabled_entity_sources(enabled_sources);
    let id = streaming
        .pin(name, center, scale, priority, ttl)
        .map_err(|e| runtime_error(&e))?;
    // A wait the grid refuses (`stream_wait_area_refused`) stores no pin either: the caller's
    // request was the pin with its wait.
    if let Err(e) = streaming.set_pin_wait(id, wait) {
        streaming.unpin(id);
        return Err(runtime_error(&e));
    }
    Ok(json!({ "pin": id.0 }))
}

/// `stream.unpin`: removes a pin.
pub(crate) fn unpin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params
        .get("pin")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`pin` (the id stream.pin returned) is required"))?;
    let mut streaming = runtime_mut(world)?;
    if !streaming.unpin(PinId(id)) {
        return Err(runtime_error(&StreamRuntimeError::UnknownPin(id)));
    }
    Ok(json!({ "removed": true }))
}

/// The entity behind a source id of `stream.sources`.
fn source_entity(world: &mut World, id: &str) -> Result<Entity, RpcError> {
    let mut query = world.query_filtered::<(Entity, Option<&EntityGuid>), With<StreamingSource>>();
    let found = query.iter(world).find_map(|(entity, guid)| {
        let label = guid.map_or(SourceId::Entity(entity.to_bits()), |g| {
            SourceId::Guid(g.0.as_u128())
        });
        (source_label(label) == id).then_some(entity)
    });
    found.ok_or_else(|| {
        f0002(
            "stream_unknown_source",
            format!("no source `{id}`"),
            "stream.sources lists the sources",
        )
    })
}

/// `stream.teleport`: pins the target with the highest priority and a wait; with `move_source`,
/// moves that source's entity once the target is ready.
pub(crate) fn teleport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = position(params, "target")?;
    let wait = readiness(params, "wait", Some(Readiness::SimReady))?;
    active(world)?;
    let mover = crate::params::opt_str(params, "move_source")?
        .map(|id| source_entity(world, id))
        .transpose()?;
    let mut streaming = runtime_mut(world)?;
    let pin = streaming
        .prepare_teleport(target)
        .map_err(|e| runtime_error(&e))?;
    let radius_m = streaming
        .pin_info(pin)
        .and_then(|p| p.wait)
        .map_or(0.0, |w| w.radius_m);
    streaming
        .set_pin_wait(
            pin,
            Some(PinWait {
                radius_m,
                readiness: wait,
            }),
        )
        .map_err(|e| runtime_error(&e))?;
    if let Some(entity) = mover {
        streaming.queue_move(pin, entity, target, wait, radius_m);
    }
    let gates = world.get_resource::<StreamGates>().cloned().unwrap_or_default();
    let streaming = active(world)?;
    Ok(json!({ "pin": pin.0, "ready": streaming.ready_near(&gates, target, radius_m, wait) }))
}

/// `stream.ready`: whether a region reached a readiness level, and the cells it waits on.
pub(crate) fn ready(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let center = position(params, "center")?;
    let radius_m = radius(params, "radius_m")?;
    let wait = readiness(params, "readiness", None)?;
    let streaming = active(world)?;
    if matches!(streaming.timing(), StreamTiming::Lockstep { .. }) && streaming.stats().lockstep_stalls > 0 {
        return Err(f0002(
            "stream_lockstep_stall",
            "a lockstep wait hit its cap: this run's determinism is void",
            "use a faster source or raise lockstep_wait_cap_ms; a test should fail here",
        ));
    }
    let empty = StreamGates::default();
    let gates = world.get_resource::<StreamGates>().unwrap_or(&empty);
    let waiting = streaming.waiting_near(gates, center, radius_m, wait, MAX_ROWS);
    // An area a grid refuses lists no cell but is not ready (fail closed).
    let ready = waiting.is_empty() && streaming.ready_near(gates, center, radius_m, wait);
    Ok(json!({
        "ready": ready,
        "waiting": waiting.iter().map(CellCoord::label).collect::<Vec<_>>(),
    }))
}

/// `stream.budget_set`: changes any subset of the budgets (development builds).
pub(crate) fn budget_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    dev_only("stream.budget_set")?;
    let changes = params
        .get("budgets")
        .and_then(Value::as_object)
        .ok_or_else(|| RpcError::invalid_params("`budgets` must be an object"))?;
    let streaming = active(world)?;
    let level = streaming.first_level().map_err(|e| runtime_error(&e))?;
    let mut budgets = streaming.budgets(level).copied().ok_or_else(not_enabled)?;
    for (key, value) in changes {
        let number = value
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| {
                f0002(
                    "stream_bad_budget",
                    format!("`{key}` must be a number, not negative"),
                    hint_for("stream_bad_budget"),
                )
            })?;
        // Integer budgets take whole numbers that fit their type; a fraction or an overflow is an
        // error (it used to truncate, and 0.4 became the "no limit" 0).
        let whole = |max: u64| -> Result<u64, RpcError> {
            if number.fract() == 0.0 && number <= max as f64 {
                Ok(number as u64)
            } else {
                Err(f0002(
                    "stream_bad_budget",
                    format!("`{key}` must be a whole number that fits {max}"),
                    hint_for("stream_bad_budget"),
                ))
            }
        };
        match key.as_str() {
            "max_loads_per_step" => budgets.max_loads_per_step = whole(u64::from(u32::MAX))? as u32,
            "max_inflight" => budgets.max_inflight = whole(u64::from(u32::MAX))? as u32,
            "max_pending_bytes" => budgets.max_pending_bytes = whole(1 << 53)?,
            "apply_budget_ms" => budgets.apply_budget_ms = number as f32,
            "apply_budget_entities" => budgets.apply_budget_entities = whole(u64::from(u32::MAX))? as u32,
            "memory_budget_bytes" => budgets.memory_budget_bytes = whole(1 << 53)?,
            "vram_budget_bytes" => budgets.vram_budget_bytes = whole(1 << 53)?,
            "hlod_budget_bytes" => budgets.hlod_budget_bytes = whole(1 << 53)?,
            other => {
                return Err(f0002(
                    "stream_bad_budget",
                    format!("`{other}` is not a budget"),
                    "see the budgets of stream.status",
                ));
            }
        }
    }
    let mut streaming = runtime_mut(world)?;
    streaming
        .set_budgets(level, budgets)
        .map_err(|e| runtime_error(&e))?;
    Ok(budgets_json(&budgets))
}

/// `stream.timing_set`: async or lockstep timing (development builds).
pub(crate) fn timing_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    dev_only("stream.timing_set")?;
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`mode` must be async or lockstep"))?;
    let streaming = active(world)?;
    let level = streaming.first_level().map_err(|e| runtime_error(&e))?;
    let default_latency = streaming
        .settings(level)
        .map_or(2, |s| s.tuning.lockstep_latency_steps);
    let timing = match mode {
        "async" => StreamTiming::Async,
        "lockstep" => StreamTiming::Lockstep {
            latency_steps: crate::params::opt_u64(params, "latency_steps")?.map_or(
                Ok(default_latency),
                |l| {
                    u32::try_from(l).map_err(|_| RpcError::invalid_params("`latency_steps` must fit 32 bits"))
                },
            )?,
        },
        other => {
            return Err(RpcError::invalid_params(format!(
                "`{other}` is not a timing mode"
            )));
        }
    };
    let mut streaming = runtime_mut(world)?;
    streaming.set_timing(timing);
    Ok(json!({ "mode": mode }))
}

/// `stream.retry`: lets a failed cell try again now.
pub(crate) fn retry(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cell = cell_param(params)?;
    let level = active(world)?.first_level().map_err(|e| runtime_error(&e))?;
    let mut streaming = runtime_mut(world)?;
    let state = streaming.retry(level, cell).map_err(|e| runtime_error(&e))?;
    Ok(json!({ "state": state.as_str() }))
}

/// `stream.hlod`: the repaired cut and the holes.
pub(crate) fn hlod(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    active(world)?;
    let cut = world.get_resource::<HlodCut>().cloned().unwrap_or_default();
    // The flat color each proxy draws with (ADR-2022); `null` for the default material. A baked
    // atlas material (ADR-2483) is named as well; `null` without one.
    type ProxyLook = (Option<[f32; 4]>, Option<regia_core::id::Guid>);
    let colors: BTreeMap<u32, ProxyLook> = world
        .query::<&regia_scene::streaming::HlodProxyMesh>()
        .iter(world)
        .map(|proxy| (proxy.node, (proxy.color, proxy.material)))
        .collect();
    let rows: Vec<Value> = cut
        .entries
        .iter()
        .map(|entry| {
            let mut row = json!({
                "node": entry.node,
                "kind": if entry.proxy { "proxy" } else { "full" },
                "cell": entry.cell.map(|c| c.label()),
                "fade": entry.fade,
            });
            if entry.proxy {
                let (color, material) = colors.get(&entry.node).copied().unwrap_or((None, None));
                row["color"] = json!(color);
                row["material"] = json!(material.map(|m| m.to_string()));
            }
            row
        })
        .collect();
    let leaving: Vec<Value> = cut
        .leaving
        .iter()
        .map(|entry| {
            json!({
                "node": entry.node,
                "kind": if entry.proxy { "proxy" } else { "full" },
                "cell": entry.cell.map(|c| c.label()),
                "share": entry.fade,
            })
        })
        .collect();
    Ok(json!({ "cut": rows, "holes": cut.holes, "leaving": leaving }))
}

/// `stream.origin`: the floating origin.
pub(crate) fn origin(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let streaming = active(world)?;
    let level = streaming.first_level().map_err(|e| runtime_error(&e))?;
    let origin = world.get_resource::<WorldOrigin>().copied().unwrap_or_default();
    let settings = streaming.settings(level).ok_or_else(not_enabled)?;
    Ok(json!({
        "cell": origin.0.cell(),
        "cell_size_m": settings.origin.cell_size_m,
        "threshold_m": settings.origin.threshold_m,
        "focus": streaming.focus(),
        "blocked_by": blocked_by(world),
        "holds": holds(world),
        "offset_m": world.get_resource::<OriginOffset>().map_or([0.0; 3], |o| o.0),
        "last_rebase": streaming
            .last_rebase()
            .map(|(step, delta)| json!({ "step": step, "cell_delta": delta })),
    }))
}

/// What keeps the origin where it is (ADR-2022), sorted.
fn blocked_by(world: &World) -> Vec<&'static str> {
    world
        .get_resource::<RebaseBlockers>()
        .map(|b| b.0.iter().copied().collect())
        .unwrap_or_default()
}

/// The engine's counted holds on blocker names (ADR-2435): `{ name: count }`.
fn holds(world: &World) -> Value {
    let holds: serde_json::Map<String, Value> = world
        .get_resource::<RebaseHolds>()
        .map(|h| {
            h.iter()
                .map(|(name, count)| (name.to_string(), json!(count)))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(holds)
}

/// `stream.rebase`: moves the origin on the next step (development builds).
pub(crate) fn rebase(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    dev_only("stream.rebase")?;
    let streaming = active(world)?;
    let origin = world.get_resource::<WorldOrigin>().copied().unwrap_or_default();
    let cell: [i64; 3] = match params.get("cell") {
        None | Some(Value::Null) => {
            let focus = streaming
                .focus()
                .ok_or_else(|| RpcError::invalid_params("no focus yet: pass `cell`"))?;
            origin
                .0
                .space()
                .from_world(focus)
                .map_err(|e| RpcError::invalid_params(e.to_string()))?
                .cell
        }
        Some(value) => {
            let values = value
                .as_array()
                .filter(|a| a.len() == 3)
                .ok_or_else(|| RpcError::invalid_params("`cell` must be [x, y, z]"))?;
            let mut out = [0i64; 3];
            for (slot, v) in out.iter_mut().zip(values) {
                *slot = v
                    .as_i64()
                    .filter(|c| c.unsigned_abs() < 1 << 40)
                    .ok_or_else(|| RpcError::invalid_params("`cell` must hold integers"))?;
            }
            out
        }
    };
    let blockers = blocked_by(world);
    if !blockers.is_empty() {
        return Err(f0002(
            "stream_rebase_blocked",
            format!("{blockers:?} cannot follow a rebase"),
            hint_for("stream_rebase_blocked"),
        ));
    }
    let now = origin.0.cell();
    let mut streaming = runtime_mut(world)?;
    streaming.request_rebase(cell);
    Ok(json!({ "cell_delta": [cell[0] - now[0], cell[1] - now[1], cell[2] - now[2]] }))
}

/// `stream.trace`: the event ring, oldest first.
pub(crate) fn trace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let streaming = active(world)?;
    let since = crate::params::opt_u64(params, "since_step")?.unwrap_or(0);
    let cap = limit(params)?;
    let rows: Vec<Value> = streaming
        .trace()
        .filter(|e| e.step >= since)
        .take(cap)
        .map(|e| {
            json!({
                "step": e.step,
                "cell": e.cell.map(|c| c.label()),
                "event": e.kind.as_str(),
                "detail": e.detail,
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

/// Run condition of [`draw_overlay`]: the overlay is on and the app has gizmos.
pub(crate) fn overlay_on(streaming: Option<Res<WorldStreaming>>) -> bool {
    streaming.is_some_and(|s| s.overlay() && s.is_active())
}

/// The color of a cell state in the overlay.
fn state_color(state: &regia_world::CellState) -> Color {
    use regia_world::CellState::{Applying, Failed, Loading, Queued, SimReady, Spawned, Unloading, Visible};
    match state {
        Visible => Color::rgb(0.2, 0.9, 0.3),
        SimReady => Color::rgb(0.2, 0.7, 0.9),
        Spawned => Color::rgb(0.3, 0.4, 0.9),
        Applying | Loading | Queued => Color::rgb(0.95, 0.8, 0.2),
        Unloading => Color::rgb(0.9, 0.5, 0.2),
        Failed(_) => Color::rgb(0.95, 0.15, 0.15),
        _ => Color::rgb(0.3, 0.3, 0.3),
    }
}

/// Draws cell borders colored by state, and a cross on cells a proxy covers. Cells beyond twice the
/// unload radius of the focus are left out.
pub(crate) fn draw_overlay(
    streaming: Res<WorldStreaming>,
    origin: Res<WorldOrigin>,
    cut: Res<HlodCut>,
    mut gizmos: Gizmos,
) {
    let Ok(level) = streaming.first_level() else {
        return;
    };
    let (Some(settings), Some(focus)) = (streaming.settings(level), streaming.focus()) else {
        return;
    };
    let Ok(grid) = settings.grid() else {
        return;
    };
    let reach = settings.unload_radius_m * 2.0;
    let y = focus[1];
    for (cell, state) in streaming.cells(level) {
        if grid.distance_to_cell(focus[0], focus[2], cell) > reach {
            continue;
        }
        let (lo, hi) = (grid.cell_min(cell), grid.cell_max(cell));
        let corners = [[lo[0], lo[1]], [hi[0], lo[1]], [hi[0], hi[1]], [lo[0], hi[1]]];
        let local = corners.map(|c| {
            let p = origin.from_world([c[0], y, c[1]]).unwrap_or_default();
            Vec3::from_array(p)
        });
        let color = state_color(&state);
        gizmos.lineloop(local, color);
        if cut.hidden.contains_key(&cell) {
            gizmos.line(local[0], local[2], color);
            gizmos.line(local[1], local[3], color);
        }
    }
}

/// `stream.overlay`: draws cell borders colored by state and the HLOD cut (development builds).
pub(crate) fn overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    dev_only("stream.overlay")?;
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`on` must be a boolean"))?;
    let mut streaming = runtime_mut(world)?;
    streaming.set_overlay(on);
    Ok(json!({ "on": on }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use regia_core::id::Guid;
    use regia_scene::ScenePlugin;
    use regia_scene::Transform;
    use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene, cook_scene};
    use regia_scene::format::SceneFile;
    use regia_scene::streaming::{LevelStreaming, SourceKind};
    use regia_world::format::{
        CELL_BLOB_FORMAT_VERSION, CellBlob, CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
    };
    use regia_world::{MemoryCellSource, WorldStreamingPlugin, open_streamed_from};

    fn level() -> LevelId {
        LevelId(Guid::from_u128(0xfeed))
    }

    fn reason(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    fn app_with_world(cells: i64) -> (App, MemoryCellSource) {
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        regia_app::enable_fixed_step(&mut app, 60.0);
        let source = MemoryCellSource::new();
        let mut records = Vec::new();
        for x in 0..cells {
            let coord = CellCoord::new(x, 0);
            let mut file = SceneFile::new();
            let text = format!(
                r#"(guid: "{}", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)) }})"#,
                Guid::from_u128(1000 + x as u128)
            );
            file.entities.push(ron::from_str(&text).expect("entity"));
            let (scene, _) = cook_scene(app.world(), &file).expect("cook");
            let blob = CellBlob {
                format_version: CELL_BLOB_FORMAT_VERSION,
                coord,
                scene,
                anchor: [0; 3],
                nav: None,
            };
            let id = cell_blob_id(level(), coord);
            let bytes = blob.to_bytes(id, vec![]).expect("bytes");
            records.push(CellRecord {
                coord,
                blob: id,
                blob_hash: *blake3::hash(&bytes).as_bytes(),
                entity_count: 1,
                own_bytes: bytes.len() as u64,
                dep_bytes: 0,
                vram_bytes: 0,
                min_y: 0.0,
                max_y: 1.0,
                priority: 0,
                deps: vec![],
                nav_tiles: 0,
                has_terrain_collision: false,
            });
            source.insert(id, bytes);
        }
        let index = WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: level().0,
            content_hash: WorldIndex::hash_cells(&records),
            settings: LevelStreaming {
                enabled: true,
                cell_size_m: 100.0,
                load_radius_m: 40.0,
                unload_radius_m: 60.0,
                hlod: None,
                tuning: regia_scene::streaming::StreamTuning {
                    wait_on_open: false,
                    ..Default::default()
                },
                ..LevelStreaming::default()
            },
            cells: records,
            bounds: [CellCoord::new(0, 0), CellCoord::new(cells - 1, 0)],
            hlod: None,
            replicated: vec![],
            nav: None,
        };
        let empty = CookedScene {
            format_version: COOKED_SCENE_VERSION,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        };
        let arc = Arc::new(source.clone());
        app.world_mut()
            .resource_mut::<WorldStreaming>()
            .set_timing(StreamTiming::Lockstep { latency_steps: 1 });
        open_streamed_from(app.world_mut(), level(), LoadMode::Replace, index, &empty, arc).expect("open");
        (app, source)
    }

    fn spawn_source(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(50.0, 0.0, 50.0)),
                EntityGuid(Guid::from_u128(7)),
                StreamingSource {
                    kind: SourceKind::Player,
                    ..StreamingSource::default()
                },
            ))
            .id()
    }

    fn step(app: &mut App, n: usize) {
        for _ in 0..n {
            app.update();
        }
    }

    /// ADR-2022: a proxy entry of the cut names the flat color its proxy draws with, `null` for the
    /// default material; a full entry has no color.
    #[test]
    fn hlod_names_each_proxy_color() {
        use regia_scene::streaming::{HlodCutEntry, HlodProxyMesh};
        let (mut app, _source) = app_with_world(2);
        let world = app.world_mut();
        let entry = |node: u32, proxy: bool| HlodCutEntry {
            node,
            proxy,
            cell: Some(CellCoord::new(i64::from(node), 0)),
            fade: 0.0,
        };
        world.insert_resource(HlodCut {
            entries: vec![entry(0, false), entry(1, true), entry(2, true)],
            holes: 0,
            hidden: Default::default(),
            leaving: vec![HlodCutEntry {
                fade: 0.75,
                ..entry(3, false)
            }],
        });
        for (node, color) in [(1, Some([0.5, 0.25, 0.125, 1.0])), (2, None)] {
            world.spawn(HlodProxyMesh {
                node,
                mesh: Guid::from_u128(u128::from(node) + 90),
                bytes: 10,
                color,
                material: None,
            });
        }
        let reply = hlod(world, &json!({})).expect("hlod");
        let cut = reply["cut"].as_array().expect("cut");
        assert_eq!(cut.len(), 3);
        assert_eq!(cut[0]["kind"], "full");
        assert!(cut[0].get("color").is_none());
        assert_eq!(cut[1]["color"], json!([0.5, 0.25, 0.125, 1.0]));
        assert!(cut[2]["color"].is_null());
        assert_eq!(reply["holes"], 0);
        assert_eq!(
            reply["leaving"],
            json!([{ "node": 3, "kind": "full", "cell": "3_0", "share": 0.75 }])
        );
    }

    #[test]
    fn opening_needs_an_asset_server_and_a_guid() {
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        let world = app.world_mut();
        assert!(open(world, &json!({})).is_err());
        assert!(open(world, &json!({ "level": "nope" })).is_err());
        let error = open(world, &json!({ "level": Guid::from_u128(1).to_string() })).expect_err("no assets");
        // No asset server in this app: docsfix-41 split this out of `stream_not_enabled`.
        assert_eq!(reason(&error), "stream_no_assets");
        let bad_mode = json!({ "level": Guid::from_u128(1).to_string(), "mode": "sideways" });
        assert!(open(world, &bad_mode).is_err());
    }

    /// ADR-8812: a level with no cooked world index (never cooked, or a stopped cook removed the
    /// old one) opens as `stream_not_cooked`, not as a corrupt index.
    #[test]
    fn opening_a_level_with_no_cooked_index_is_stream_not_cooked() {
        let root = regia_test_utils::unique_temp_dir("agent-stream-not-cooked");
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        let world = app.world_mut();
        world.insert_resource(server);
        let error = open(world, &json!({ "level": Guid::from_u128(7).to_string() })).expect_err("not cooked");
        assert_eq!(reason(&error), "stream_not_cooked", "{error:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-8862: in a packaged game whose chunk holding the level is not mounted, `stream.open`
    /// keeps `stream_not_cooked` but its message and hint name the chunk instead of "cook it again".
    #[test]
    fn opening_a_level_in_an_unmounted_chunk_names_the_chunk() {
        let root = regia_test_utils::unique_temp_dir("agent-stream-unmounted-chunk");
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        let level = LevelId(Guid::from_u128(8));
        let mut homes = std::collections::BTreeMap::new();
        homes.insert(regia_world::format::world_index_id(level), "forest".to_owned());
        server.set_chunk_map(Some(regia_asset::content::ChunkMap::from_assignment(&homes)));
        let world = app.world_mut();
        world.insert_resource(server);
        let error =
            open(world, &json!({ "level": Guid::from_u128(8).to_string() })).expect_err("not mounted");
        assert_eq!(reason(&error), "stream_not_cooked", "{error:?}");
        let hint = error
            .data
            .as_ref()
            .and_then(|d| d.get("hint"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        assert!(
            hint.contains("`forest`") && hint.contains("content.mount"),
            "{hint}"
        );
        assert!(error.message.contains("`forest`"), "{}", error.message);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_method_reports_stream_not_enabled_without_a_streamed_level() {
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        let world = app.world_mut();
        assert_eq!(status(world, &json!({})).expect("status")["enabled"], false);
        let calls: Vec<Result<Value, RpcError>> = vec![
            cells(world, &json!({})),
            sources(world, &json!({})),
            pin(world, &json!({ "center": [0.0, 0.0, 0.0] })),
            unpin(world, &json!({ "pin": 1 })),
            teleport(world, &json!({ "target": [0.0, 0.0, 0.0] })),
            ready(
                world,
                &json!({ "center": [0.0, 0.0, 0.0], "radius_m": 1.0, "readiness": "visible" }),
            ),
            budget_set(world, &json!({ "budgets": {} })),
            timing_set(world, &json!({ "mode": "async" })),
            retry(world, &json!({ "cell": "0_0" })),
            hlod(world, &json!({})),
            origin(world, &json!({})),
            rebase(world, &json!({ "cell": [1, 0, 0] })),
            trace(world, &json!({})),
            overlay(world, &json!({ "on": true })),
        ];
        for call in calls {
            assert_eq!(reason(&call.expect_err("no level")), "stream_not_enabled");
        }
    }

    #[test]
    fn bounds_set_turns_the_vram_bound_on_without_a_level_and_checks_its_params() {
        let mut app = App::new();
        app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
        let world = app.world_mut();
        let out = bounds_set(world, &json!({ "vram_bound": true })).expect("no level needed");
        assert_eq!(out["vram_bound"], true);
        // No renderer in this app: residency reads null and cannot be set.
        assert!(out["render_residency"].is_null());
        assert!(bounds_set(world, &json!({ "render_residency": true })).is_err());
        assert!(world.resource::<WorldStreaming>().vram_bound());
        let bad = bounds_set(world, &json!({ "vram_bound": "yes" })).expect_err("not a bool");
        assert_eq!(bad.code, RpcError::invalid_params("").code);
        let off = bounds_set(world, &json!({ "vram_bound": false })).expect("off");
        assert_eq!(off["vram_bound"], false);
        // No physics either: the tile counters read null.
        assert!(collision_tiles(world, &json!({})).expect("read").is_null());
    }

    #[test]
    fn the_read_methods_describe_a_running_world() {
        let (mut app, _source) = app_with_world(3);
        spawn_source(&mut app);
        step(&mut app, 6);
        let world = app.world_mut();
        let status = status(world, &json!({})).expect("status");
        assert_eq!(status["enabled"], true);
        assert_eq!(status["timing"], "lockstep");
        assert_eq!(status["cells"]["visible"], 1);
        assert_eq!(status["cells"]["unloaded"], 2);
        assert_eq!(status["stats"]["resident_cells"], 1);

        let rows = cells(world, &json!({ "state": "visible" })).expect("cells");
        assert_eq!(rows.as_array().expect("rows").len(), 1);
        assert_eq!(rows[0]["cell"], "0_0");
        assert_eq!(rows[0]["entities"], 1);
        let far = cells(world, &json!({ "near": [950.0, 0.0, 0.0], "radius_m": 10.0 })).expect("cells");
        assert!(far.as_array().expect("rows").is_empty());
        assert_eq!(
            cells(world, &json!({ "limit": 2 }))
                .expect("cells")
                .as_array()
                .expect("rows")
                .len(),
            2
        );

        let list = sources(world, &json!({})).expect("sources");
        assert_eq!(list[0]["kind"], "player");
        assert_eq!(list[0]["id"], Guid::from_u128(7).to_string());

        let trace_rows = trace(world, &json!({ "limit": 5 })).expect("trace");
        assert_eq!(trace_rows[0]["event"], "queued");
        assert_eq!(trace_rows[0]["cell"], "0_0");
        assert!(
            trace(world, &json!({ "since_step": 1_000_000 }))
                .expect("trace")
                .as_array()
                .expect("rows")
                .is_empty()
        );

        let answer = origin(world, &json!({})).expect("origin");
        assert_eq!(answer["cell"], json!([0, 0, 0]));
        assert_eq!(answer["cell_size_m"], 1024.0);
        assert_eq!(hlod(world, &json!({})).expect("hlod")["holes"], 0);
    }

    /// `stream.status` names the terrain tile stream's counters, and `null` without one.
    #[test]
    fn status_reports_the_terrain_tile_stream() {
        let (mut app, _) = app_with_world(1);
        let answer = status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(answer["terrain_tiles"], Value::Null);
        let cache =
            regia_terrain::stream::TileCache::new(4096, regia_terrain::stream::MemoryTileSource::new());
        app.world_mut()
            .insert_resource(regia_terrain::TerrainTileStream::new(
                cache,
                vec![0],
                4096.0,
                300.0,
                4,
            ));
        let answer = status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(answer["terrain_tiles"]["budget"], 4096);
        assert_eq!(answer["terrain_tiles"]["resident"], 0);
    }

    #[test]
    fn a_pin_waits_until_its_region_is_ready_and_the_predicate_field_counts_it() {
        let (mut app, _source) = app_with_world(5);
        let answer = pin(
            app.world_mut(),
            &json!({
                "name": "next stop",
                "center": [350.0, 0.0, 50.0],
                "wait": { "radius_m": 10.0, "readiness": "sim_ready" }
            }),
        )
        .expect("pin");
        let id = answer["pin"].as_u64().expect("id");
        step(&mut app, 1);
        assert_eq!(app.world().resource::<StreamStats>().pending_waits, 1);
        let probe = json!({ "center": [350.0, 0.0, 50.0], "radius_m": 10.0, "readiness": "visible" });
        let waiting = ready(app.world_mut(), &probe).expect("ready");
        assert_eq!(waiting["ready"], false);
        assert_eq!(waiting["waiting"], json!(["3_0"]));
        step(&mut app, 5);
        assert_eq!(app.world().resource::<StreamStats>().pending_waits, 0);
        assert_eq!(ready(app.world_mut(), &probe).expect("ready")["ready"], true);
        assert_eq!(
            unpin(app.world_mut(), &json!({ "pin": id })).expect("unpin")["removed"],
            true
        );
        assert_eq!(
            reason(&unpin(app.world_mut(), &json!({ "pin": id })).expect_err("gone")),
            "stream_unknown_pin"
        );
    }

    /// rev-zai-rc-b terrain 4 (w9-pack-stamp-and-content): a pin wait whose area the grid refuses
    /// (here a 1000 km radius over 100 m cells, far past the 2^22-cell cap) could never be met, and
    /// `play.wait_until pending_waits == 0` ran into its own timeout with no error. `stream.pin`
    /// refuses it with `stream_wait_area_refused` and stores no pin.
    #[test]
    fn a_pin_wait_the_grid_refuses_is_refused_and_stores_no_pin() {
        let (mut app, _source) = app_with_world(5);
        let error = pin(
            app.world_mut(),
            &json!({
                "name": "everything",
                "center": [50.0, 0.0, 50.0],
                "wait": { "radius_m": 1.0e6, "readiness": "sim_ready" }
            }),
        )
        .expect_err("a wait the grid refuses was accepted");
        assert_eq!(reason(&error), "stream_wait_area_refused");
        assert!(
            app.world().resource::<WorldStreaming>().pins().next().is_none(),
            "the refused pin was stored"
        );
        step(&mut app, 2);
        assert_eq!(app.world().resource::<StreamStats>().pending_waits, 0);
    }

    /// The verification flow of the guide page: `play.wait_until` and `play.expect` read the waits and
    /// the holes through a resource predicate.
    #[test]
    fn the_wait_predicate_reads_the_stream_stats_resource() {
        let (mut app, _source) = app_with_world(5);
        pin(
            app.world_mut(),
            &json!({
                "center": [350.0, 0.0, 50.0],
                "wait": { "radius_m": 10.0, "readiness": "sim_ready" }
            }),
        )
        .expect("pin");
        let waits = json!({ "resource": "StreamStats", "path": "pending_waits", "op": "eq", "value": 0 });
        let holes = json!({ "resource": "StreamStats", "path": "holes", "op": "eq", "value": 0 });
        step(&mut app, 1);
        let pending = crate::playtest::evaluate(app.world_mut(), &waits);
        assert!(!pending.holds, "{pending:?}");
        step(&mut app, 5);
        let met = crate::playtest::evaluate(app.world_mut(), &waits);
        assert!(met.holds, "{met:?}");
        let clean = crate::playtest::evaluate(app.world_mut(), &holes);
        assert!(clean.holds, "{clean:?}");
    }

    #[test]
    fn a_teleport_moves_its_source_once_the_target_is_ready() {
        let (mut app, _source) = app_with_world(40);
        let entity = spawn_source(&mut app);
        step(&mut app, 2);
        let id = Guid::from_u128(7).to_string();
        let answer = teleport(
            app.world_mut(),
            &json!({ "target": [3050.0, 0.0, 50.0], "move_source": id, "wait": "sim_ready" }),
        )
        .expect("teleport");
        assert_eq!(answer["ready"], false);
        step(&mut app, 8);
        let x = app
            .world()
            .get::<Transform>(entity)
            .expect("source")
            .translation
            .x;
        assert_eq!(x, 3050.0);
        assert_eq!(app.world().resource::<StreamStats>().pending_waits, 0);
        assert_eq!(
            reason(
                &teleport(
                    app.world_mut(),
                    &json!({ "target": [0.0, 0.0, 0.0], "move_source": "nobody" })
                )
                .expect_err("unknown")
            ),
            "stream_unknown_source"
        );
    }

    #[test]
    fn budgets_timing_retry_and_rebase_change_the_runtime() {
        let (mut app, source) = app_with_world(3);
        spawn_source(&mut app);
        let bad = cell_blob_id(level(), CellCoord::new(0, 0));
        source.set_error(bad, Some(regia_world::CellReadError::Missing));
        step(&mut app, 4);
        let world = app.world_mut();
        let failed = cells(world, &json!({ "state": "failed" })).expect("cells");
        assert_eq!(failed[0]["reason"], "cell_missing");

        let budgets = budget_set(world, &json!({ "budgets": { "max_inflight": 2 } })).expect("set");
        assert_eq!(budgets["max_inflight"], 2);
        for (patch, code) in [
            (json!({ "budgets": { "max_inflight": 0 } }), "stream_bad_budget"),
            (json!({ "budgets": { "no_such": 1 } }), "stream_bad_budget"),
            (json!({ "budgets": { "max_inflight": -1 } }), "stream_bad_budget"),
            (
                json!({ "budgets": { "max_loads_per_step": 0.4 } }),
                "stream_bad_budget",
            ),
            (
                json!({ "budgets": { "max_inflight": 5_000_000_000_u64 } }),
                "stream_bad_budget",
            ),
        ] {
            assert_eq!(reason(&budget_set(world, &patch).expect_err("bad")), code);
        }

        assert_eq!(
            timing_set(world, &json!({ "mode": "lockstep", "latency_steps": 3 })).expect("set")["mode"],
            "lockstep"
        );
        assert!(timing_set(world, &json!({ "mode": "sometimes" })).is_err());
        assert!(
            timing_set(
                world,
                &json!({ "mode": "lockstep", "latency_steps": 5_000_000_000_u64 })
            )
            .is_err(),
            "a latency that does not fit u32 is refused, not saturated"
        );

        source.set_error(bad, None);
        assert_eq!(
            retry(world, &json!({ "cell": "0_0" })).expect("retry")["state"],
            "unloaded"
        );
        assert_eq!(
            reason(&retry(world, &json!({ "cell": "9_9" })).expect_err("unknown")),
            "stream_unknown_cell"
        );
        assert!(retry(world, &json!({ "cell": "nope" })).is_err());

        assert_eq!(
            rebase(world, &json!({ "cell": [2, 0, 0] })).expect("rebase")["cell_delta"],
            json!([2, 0, 0])
        );
        step(&mut app, 1);
        let answer = origin(app.world_mut(), &json!({})).expect("origin");
        assert_eq!(answer["cell"], json!([2, 0, 0]));
        assert_eq!(answer["last_rebase"]["cell_delta"], json!([2, 0, 0]));
        assert_eq!(answer["blocked_by"], json!([]));
        assert_eq!(
            overlay(app.world_mut(), &json!({ "on": true })).expect("overlay")["on"],
            true
        );

        // ADR-2022: a registered blocker refuses the rebase and keeps the origin.
        app.world_mut()
            .get_resource_or_init::<RebaseBlockers>()
            .0
            .insert("terrain");
        let world = app.world_mut();
        assert_eq!(
            reason(&rebase(world, &json!({ "cell": [5, 0, 0] })).expect_err("blocked")),
            "stream_rebase_blocked"
        );
        assert_eq!(
            origin(world, &json!({})).expect("origin")["blocked_by"],
            json!(["terrain"])
        );
        // A rebase requested before the blocker arrived is not taken either, and is counted.
        world.resource_mut::<WorldStreaming>().request_rebase([7, 0, 0]);
        step(&mut app, 1);
        let answer = origin(app.world_mut(), &json!({})).expect("origin");
        assert_eq!(answer["cell"], json!([2, 0, 0]));
        let stats = status(app.world_mut(), &json!({})).expect("status")["stats"].clone();
        assert_eq!(stats["rebases_blocked"], 1);

        // ADR-2435: the engine's counted holds show beside the names, and the offset beside the cell.
        use regia_scene::streaming::{TERRAIN_STROKE_BLOCKER, acquire_rebase_hold, release_rebase_hold};
        let world = app.world_mut();
        world.resource_mut::<RebaseBlockers>().0.clear();
        world.insert_resource(OriginOffset([2048.0, 0.0, -1024.0]));
        acquire_rebase_hold(world, TERRAIN_STROKE_BLOCKER);
        acquire_rebase_hold(world, TERRAIN_STROKE_BLOCKER);
        let answer = origin(world, &json!({})).expect("origin");
        assert_eq!(answer["holds"], json!({ "terrain_brush_stroke": 2 }));
        assert_eq!(answer["blocked_by"], json!(["terrain_brush_stroke"]));
        assert_eq!(answer["offset_m"], json!([2048.0, 0.0, -1024.0]));
        assert_eq!(
            reason(&rebase(world, &json!({ "cell": [5, 0, 0] })).expect_err("blocked")),
            "stream_rebase_blocked"
        );
        assert!(release_rebase_hold(world, TERRAIN_STROKE_BLOCKER));
        assert!(release_rebase_hold(world, TERRAIN_STROKE_BLOCKER));
        let answer = origin(world, &json!({})).expect("origin");
        assert_eq!(answer["holds"], json!({}));
        assert_eq!(answer["blocked_by"], json!([]));
    }

    #[test]
    fn the_source_limit_stall_and_dev_only_have_their_codes() {
        let (mut app, source) = app_with_world(3);
        let world = app.world_mut();
        for _ in 0..regia_world::runtime::MAX_STREAM_SOURCES {
            pin(world, &json!({ "center": [0.0, 0.0, 0.0] })).expect("pin");
        }
        assert_eq!(
            reason(&pin(world, &json!({ "center": [0.0, 0.0, 0.0] })).expect_err("limit")),
            "stream_source_limit"
        );

        assert_eq!(
            reason(&dev_gate("stream.rebase", false).expect_err("shipping")),
            "stream_dev_only"
        );
        assert!(dev_gate("stream.rebase", true).is_ok());

        let (mut stalled, source2) = app_with_world(1);
        drop(source);
        let cell = cell_blob_id(level(), CellCoord::new(0, 0));
        source2.hold(cell);
        {
            let mut budgets = stalled
                .world()
                .resource::<WorldStreaming>()
                .budgets(level())
                .copied()
                .expect("level");
            budgets.max_inflight = 1;
            stalled
                .world_mut()
                .resource_mut::<WorldStreaming>()
                .set_budgets(level(), budgets)
                .expect("budgets");
        }
        spawn_source(&mut stalled);
        // A held read at a one-step latency hits the wait cap once; a low cap keeps the test quick.
        regia_world::runtime::set_wait_cap_for_tests(
            stalled.world_mut().resource_mut::<WorldStreaming>().as_mut(),
            level(),
            1,
        );
        step(&mut stalled, 3);
        let error = ready(
            stalled.world_mut(),
            &json!({ "center": [0.0, 0.0, 0.0], "radius_m": 1.0, "readiness": "visible" }),
        )
        .expect_err("stalled");
        assert_eq!(reason(&error), "stream_lockstep_stall");
        source2.release(cell);
    }

    #[test]
    fn bad_parameters_are_invalid_params_never_a_panic() {
        let (mut app, _source) = app_with_world(1);
        let world = app.world_mut();
        for bad in [
            json!({}),
            json!({ "center": [1.0] }),
            json!({ "center": ["a", 0, 0] }),
        ] {
            assert!(pin(world, &bad).is_err(), "{bad}");
        }
        assert!(
            ready(
                world,
                &json!({ "center": [0.0, 0.0, 0.0], "radius_m": -1.0, "readiness": "visible" })
            )
            .is_err()
        );
        assert!(
            ready(
                world,
                &json!({ "center": [0.0, 0.0, 0.0], "radius_m": 1.0, "readiness": "x" })
            )
            .is_err()
        );
        assert!(unpin(world, &json!({})).is_err());
        assert!(overlay(world, &json!({})).is_err());
        assert!(rebase(world, &json!({ "cell": [1] })).is_err());
    }
}
