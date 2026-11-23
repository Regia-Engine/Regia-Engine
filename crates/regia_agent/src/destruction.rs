// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for destruction level 2 (ADR-1597, book `agents/destruction.md`):
//! `physics.destruction_define` places destructibles (the wall-and-bridge sample, a box grid, or an
//! inline `.destructible.ron`), `physics.destruction_damage` queues a hit for the next fixed tick,
//! `physics.destruction_state` reports chunks, bonds, hashes and the recent break events, and
//! `physics.destruction_budget` reads or changes the caps and reports the pool.
//!
//! Without `DestructionPlugin` every method answers `NOT_FOUND` naming the plugin. Errors from the
//! destruction API carry `data.reason` (`destruction_*`, listed in the book page).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_framework::destruction_net::{
    DestructionNet, MAX_OPS_PER_MESSAGE, MAX_SNAPSHOT_PART_BYTES, MAX_SNAPSHOT_PARTS_PER_TICK,
    MIN_SNAPSHOT_PART_BYTES,
};
use regia_net::model::NetMode;
use regia_physics::PhysicsWorld;
use regia_physics::destruction::sample::{self, SAMPLE_SEEDS};
use regia_physics::destruction::{
    Authority, BoxGrid, BreakEvent, BreakKind, ChunkState, Damage, DamageKind, DestructibleAsset,
    DestructibleId, DestructiblePose, Destruction, DestructionError, DestructionMaterial, DetachCause,
    EVENT_LOG_CAP, GridSupport,
};
use regia_physics::destruction::{DestructionImpact, ImpactSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Default number of recent events `physics.destruction_state` returns.
const DEFAULT_EVENTS: usize = 16;

fn not_installed() -> RpcError {
    RpcError::not_found(
        "no Destruction resource in this app",
        "add regia_physics::destruction::DestructionPlugin (with PhysicsPlugin) to break things",
    )
}

/// An `F0002` error carrying the destruction error code in `data.reason`.
fn destruction_error(err: &DestructionError) -> RpcError {
    reason_error(err.code(), err.to_string())
}

fn reason_error(reason: &str, message: String) -> RpcError {
    let mut e = RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        message,
        "physics.destruction_state lists the instances and chunks; see book/agents/destruction.md",
    );
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn vec3(params: &Value, field: &str) -> Result<[f32; 3], RpcError> {
    let v = crate::physics::vec3_param(params, field)?;
    Ok([v.x, v.y, v.z])
}

fn vec3_or(params: &Value, field: &str, default: [f32; 3]) -> Result<[f32; 3], RpcError> {
    if params.get(field).is_some() {
        vec3(params, field)
    } else {
        Ok(default)
    }
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && (*n as f32).is_finite())
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a finite number")))?;
            Ok(Some(n as f32))
        }
    }
}

fn required(params: &Value, field: &str) -> Result<f32, RpcError> {
    number(params, field)?.ok_or_else(|| RpcError::invalid_params(format!("`{field}` is required")))
}

fn uint(params: &Value, field: &str) -> Result<Option<u32>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an integer 0..=4294967295"))),
    }
}

fn id_param(params: &Value) -> Result<Option<DestructibleId>, RpcError> {
    Ok(uint(params, "id")?.map(DestructibleId))
}

fn hex(v: u64) -> String {
    format!("{v:016x}")
}

fn event_json(e: &BreakEvent) -> Value {
    let mut v = json!({
        "seq": e.seq,
        "tick": e.tick,
        "id": e.destructible.0,
        "seed": hex(e.seed),
    });
    let Some(map) = v.as_object_mut() else {
        return v;
    };
    match &e.kind {
        BreakKind::BondBroken { bond, a, b, position } => {
            map.insert("kind".into(), json!("bond_broken"));
            map.insert("bond".into(), json!(bond));
            map.insert("a".into(), json!(a));
            map.insert("b".into(), json!(b));
            map.insert("position".into(), json!(position));
        }
        BreakKind::ChunkDetached {
            chunk,
            cause,
            position,
            linvel,
            angvel,
        } => {
            map.insert("kind".into(), json!("chunk_detached"));
            map.insert("chunk".into(), json!(chunk));
            map.insert(
                "cause".into(),
                json!(match cause {
                    DetachCause::Damage => "damage",
                    DetachCause::Collapse => "collapse",
                }),
            );
            map.insert("position".into(), json!(position));
            map.insert("linvel".into(), json!(linvel));
            map.insert("angvel".into(), json!(angvel));
        }
    }
    v
}

/// `physics.destruction_define { sample | grid | asset, position?, seed? }`.
pub(crate) fn destruction_define(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<Destruction>() {
        return Err(not_installed());
    }
    let position = vec3_or(params, "position", [0.0; 3])?;
    let seed = params.get("seed").map_or(Ok(0), |v| {
        v.as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be a non-negative integer"))
    })?;
    let sources = ["sample", "grid", "asset"]
        .iter()
        .filter(|k| params.get(**k).is_some())
        .count();
    if sources != 1 {
        return Err(RpcError::invalid_params(
            "give exactly one of `sample`, `grid` or `asset`",
        ));
    }
    let mut assets: Vec<(Arc<DestructibleAsset>, [f32; 3], u64)> = Vec::new();
    if let Some(name) = params.get("sample") {
        if name.as_str() != Some("wall_bridge") {
            return Err(RpcError::invalid_params("`sample` must be \"wall_bridge\""));
        }
        let (wall, bridge) = sample::sample_assets().map_err(|e| destruction_error(&e))?;
        let at = |p: [f32; 3]| [p[0] + position[0], p[1] + position[1], p[2] + position[2]];
        assets.push((wall, at(sample::WALL_POSE), SAMPLE_SEEDS[0]));
        assets.push((bridge, at(sample::BRIDGE_POSE), SAMPLE_SEEDS[1]));
    } else if let Some(grid) = params.get("grid") {
        let counts = crate::params::opt_array(grid, "counts")?
            .filter(|a| a.len() == 3)
            .and_then(|a| {
                let c = |i: usize| a[i].as_u64().and_then(|n| u32::try_from(n).ok());
                Some([c(0)?, c(1)?, c(2)?])
            })
            .ok_or_else(|| RpcError::invalid_params("`grid.counts` must be three integers"))?;
        let cell = vec3_or(grid, "cell", [0.5, 0.25, 0.25])?;
        let text = |field: &str, default: &str| -> Result<String, RpcError> {
            match grid.get(field) {
                None | Some(Value::Null) => Ok(default.to_string()),
                Some(Value::String(s)) => Ok(s.clone()),
                Some(_) => Err(RpcError::invalid_params(format!(
                    "`grid.{field}` must be a string"
                ))),
            }
        };
        let support_name = text("support", "bottom")?;
        let name = text("name", "agent_grid")?;
        let fx_tag = text("fx_tag", "default")?;
        let support = match support_name.as_str() {
            "bottom" => GridSupport::Bottom,
            "ends" => GridSupport::Ends,
            "none" => GridSupport::None,
            _ => {
                return Err(RpcError::invalid_params(
                    "`grid.support` must be bottom, ends or none",
                ));
            }
        };
        let asset = DestructibleAsset::box_grid(&BoxGrid {
            name,
            counts,
            cell,
            density: number(grid, "density")?.unwrap_or(2000.0),
            strength_per_area: number(grid, "strength_per_area")?.unwrap_or(4000.0),
            support,
            material: DestructionMaterial {
                fx_tag,
                ..DestructionMaterial::default()
            },
        })
        .map_err(|e| destruction_error(&e.into()))?;
        assets.push((Arc::new(asset), position, seed));
    } else if let Some(text) = params.get("asset") {
        let text = text
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`asset` must be .destructible.ron text"))?;
        let asset = DestructibleAsset::from_ron(text).map_err(|e| destruction_error(&e.into()))?;
        assets.push((Arc::new(asset), position, seed));
    }
    let defined = world.resource_scope(|world, mut d: Mut<Destruction>| {
        let mut ids = Vec::with_capacity(assets.len());
        for (asset, at, seed) in assets {
            let pose = DestructiblePose::at(at);
            let id = match world.get_resource_mut::<PhysicsWorld>() {
                Some(mut physics) => d.define(&mut physics, asset, pose, seed),
                // No physics yet: the plugin builds the colliders when a PhysicsWorld appears.
                None => d.world.define(asset, pose, seed),
            }
            .map_err(|e| destruction_error(&e))?;
            ids.push(id.0);
        }
        Ok::<_, RpcError>(ids)
    })?;
    Ok(json!({ "ids": defined }))
}

/// `physics.destruction_damage { kind, id?, ... }`: queues one hit for the next fixed tick.
pub(crate) fn destruction_damage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mut d) = world.get_resource_mut::<Destruction>() else {
        return Err(not_installed());
    };
    let target = id_param(params)?;
    let kind = match crate::params::opt_str(params, "kind")? {
        Some("explosion") => DamageKind::Explosion {
            center: vec3(params, "center")?,
            radius: required(params, "radius")?,
            impulse: required(params, "impulse")?,
        },
        Some("projectile") => DamageKind::Projectile {
            origin: vec3(params, "origin")?,
            direction: vec3(params, "direction")?,
            impulse: required(params, "impulse")?,
            radius: number(params, "radius")?.unwrap_or(0.0),
            max_distance: number(params, "max_distance")?.unwrap_or(100.0),
        },
        Some("impulse") => DamageKind::Impulse {
            chunk: uint(params, "chunk")?.ok_or_else(|| RpcError::invalid_params("`chunk` is required"))?,
            impulse: vec3(params, "vector")?,
        },
        _ => {
            return Err(RpcError::invalid_params(
                "`kind` must be explosion, projectile or impulse",
            ));
        }
    };
    let n = d
        .world
        .damage(Damage { target, kind })
        .map_err(|e| destruction_error(&e))?;
    Ok(json!({
        "queued": n,
        "lands_on_tick": d.world.tick() + 1,
        "pending": d.world.queued_damage(),
    }))
}

/// `physics.destruction_state { id?, events? }`.
pub(crate) fn destruction_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(d) = world.get_resource::<Destruction>() else {
        return Err(not_installed());
    };
    let only = id_param(params)?;
    if let Some(id) = only
        && d.world.instance(id).is_none()
    {
        return Err(destruction_error(&DestructionError::UnknownId(id.0)));
    }
    let want = uint(params, "events")?.map_or(DEFAULT_EVENTS, |n| (n as usize).min(EVENT_LOG_CAP));
    let instances: Vec<Value> = d
        .world
        .instances()
        .filter(|(id, _)| only.is_none_or(|o| o == *id))
        .map(|(id, inst)| {
            let asset = inst.asset();
            let mut v = json!({
                "id": id.0,
                "name": asset.name,
                "fx_tag": asset.material.fx_tag,
                "position": inst.pose().translation,
                "chunks": asset.chunks.len(),
                "attached": inst.attached_count(),
                "bonds": asset.bonds.len(),
                "broken": inst.broken_count(),
                "supports": asset.chunks.iter().filter(|c| c.support).count(),
            });
            if only.is_some()
                && let Some(map) = v.as_object_mut()
            {
                let detached: Vec<usize> = inst
                    .chunk_states()
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| **s == ChunkState::Detached)
                    .map(|(i, _)| i)
                    .collect();
                map.insert("detached_chunks".into(), json!(detached));
                let debris: Vec<u32> = d
                    .physics
                    .active()
                    .filter(|a| a.destructible == id)
                    .map(|a| a.chunk)
                    .collect();
                map.insert("debris_chunks".into(), json!(debris));
            }
            v
        })
        .collect();
    let recent: Vec<Value> = {
        let all: Vec<&BreakEvent> = d
            .world
            .recent_events()
            .filter(|e| only.is_none_or(|o| o == e.destructible))
            .collect();
        all[all.len().saturating_sub(want)..]
            .iter()
            .copied()
            .map(event_json)
            .collect()
    };
    let c = d.world.counters();
    let last = d.last_tick();
    let totals = d.message_totals();
    Ok(json!({
        "authority": match d.world.authority() {
            Authority::Server => "server",
            Authority::Replica => "replica",
        },
        "tick": d.world.tick(),
        "structure_hash": hex(d.world.structure_hash()),
        "state_hash": hex(d.world.state_hash()),
        "queued_damage": d.world.queued_damage(),
        "pending_spawns": d.world.pending_spawns(),
        "event_count": d.world.event_count(),
        "counters": {
            "ticks": c.ticks,
            "hits": c.hits,
            "bonds_broken": c.bonds_broken,
            "chunks_detached": c.chunks_detached,
            "deferred_breaks": c.deferred_breaks,
        },
        "last_tick": {
            "tick": last.tick,
            "events": last.events,
            "hits": last.hits,
            "misses": last.misses,
            "deferred_breaks": last.deferred_breaks,
            "spawned": last.spawned,
            "despawned": last.despawned,
            "colliders_removed": last.colliders_removed,
            "nav_regions": last.nav_regions,
        },
        "messages": { "breaks": totals.breaks, "fx": totals.fx, "nav": totals.nav },
        "instances": instances,
        "events": recent,
    }))
}

/// `physics.destruction_budget { max_active_chunks?, max_breaks_per_tick?, max_spawns_per_tick?,
/// sleep_despawn_secs?, max_lifetime_secs?, kill_y? }`: changes the given caps, then reports the
/// budget and the pool.
pub(crate) fn destruction_budget(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mut d) = world.get_resource_mut::<Destruction>() else {
        return Err(not_installed());
    };
    let mut b = d.world.budget().clone();
    let mut changed = false;
    for (field, slot) in [
        ("max_active_chunks", &mut b.max_active_chunks),
        ("max_breaks_per_tick", &mut b.max_breaks_per_tick),
        ("max_spawns_per_tick", &mut b.max_spawns_per_tick),
    ] {
        if let Some(v) = uint(params, field)? {
            *slot = v;
            changed = true;
        }
    }
    for (field, slot) in [
        ("sleep_despawn_secs", &mut b.sleep_despawn_secs),
        ("max_lifetime_secs", &mut b.max_lifetime_secs),
    ] {
        if let Some(v) = number(params, field)? {
            if v < 0.0 {
                return Err(RpcError::invalid_params(format!("`{field}` must be >= 0")));
            }
            *slot = v;
            changed = true;
        }
    }
    if let Some(v) = number(params, "kill_y")? {
        b.kill_y = v;
        changed = true;
    }
    if changed {
        d.world.set_budget(b.clone());
    }
    let s = d.physics.stats();
    Ok(json!({
        "changed": changed,
        "budget": {
            "max_active_chunks": b.max_active_chunks,
            "max_breaks_per_tick": b.max_breaks_per_tick,
            "max_spawns_per_tick": b.max_spawns_per_tick,
            "sleep_despawn_secs": b.sleep_despawn_secs,
            "max_lifetime_secs": b.max_lifetime_secs,
            "kill_y": b.kill_y,
        },
        "pool": {
            "active": s.active,
            "pool_free": s.pool_free,
            "pending_spawns": d.world.pending_spawns(),
            "spawned_total": s.spawned_total,
            "pool_reuses": s.pool_reuses,
            "despawned_total": s.despawned_total,
            "evicted_total": s.evicted_total,
            "colliders_removed_total": s.colliders_removed_total,
            "last_sync_us": s.last_sync_us,
        },
    }))
}

/// `physics.destruction_net { max_ops_per_tick?, check_every_ticks?, snapshot_parts_per_tick?,
/// snapshot_part_bytes?, resync? }`: what destruction
/// over the network is doing on this process (`book/agents/destruction-net.md`).
pub(crate) fn destruction_net(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = world.get_resource::<NetMode>().copied().unwrap_or_default();
    let (structure_hash, event_count, authority) = {
        let Some(d) = world.get_resource::<Destruction>() else {
            return Err(not_installed());
        };
        (
            d.world.structure_hash(),
            d.world.event_count(),
            d.world.authority(),
        )
    };
    let Some(mut net) = world.get_resource_mut::<DestructionNet>() else {
        return Err(RpcError::not_found(
            "no DestructionNet resource in this app",
            "add regia_framework::destruction_net::DestructionNetPlugin after NetPlugin and DestructionPlugin",
        ));
    };
    // Every parameter is checked before any applies: a refused call changes nothing.
    let max_ops = uint(params, "max_ops_per_tick")?;
    if max_ops.is_some_and(|v| v == 0 || v as usize >= MAX_OPS_PER_MESSAGE) {
        return Err(RpcError::invalid_params(format!(
            "`max_ops_per_tick` must be 1 to {}",
            MAX_OPS_PER_MESSAGE - 1
        )));
    }
    let check_every = uint(params, "check_every_ticks")?;
    let snapshot_parts = uint(params, "snapshot_parts_per_tick")?;
    if snapshot_parts.is_some_and(|v| v == 0 || v > MAX_SNAPSHOT_PARTS_PER_TICK) {
        return Err(RpcError::invalid_params(format!(
            "`snapshot_parts_per_tick` must be 1 to {MAX_SNAPSHOT_PARTS_PER_TICK}"
        )));
    }
    let part_bytes = uint(params, "snapshot_part_bytes")?;
    if part_bytes.is_some_and(|v| !(MIN_SNAPSHOT_PART_BYTES..=MAX_SNAPSHOT_PART_BYTES).contains(&v)) {
        return Err(RpcError::invalid_params(format!(
            "`snapshot_part_bytes` must be {MIN_SNAPSHOT_PART_BYTES} to {MAX_SNAPSHOT_PART_BYTES}"
        )));
    }
    let resync = crate::params::opt_bool(params, "resync")? == Some(true);
    if resync && !mode.is_client() {
        let mut err = RpcError::invalid_params("only a client asks for a snapshot");
        if let Some(data) = err.data.as_mut() {
            data["reason"] = json!("destruction_not_client");
        }
        return Err(err);
    }
    let mut changed = resync;
    if let Some(v) = max_ops {
        net.config.max_ops_per_tick = v;
        changed = true;
    }
    if let Some(v) = check_every {
        net.config.check_every_ticks = v;
        changed = true;
    }
    if let Some(v) = snapshot_parts {
        net.config.snapshot_parts_per_tick = v;
        changed = true;
    }
    if let Some(v) = part_bytes {
        net.config.snapshot_part_bytes = v;
        changed = true;
    }
    if resync {
        net.request_resync();
    }
    let config = net.config;
    let anchors: Vec<Value> = net
        .relevance_anchors()
        .map(|(id, e)| json!({ "id": id.0, "anchor": e.to_bits() }))
        .collect();
    let filtered: Vec<Value> = net
        .filtered_peers()
        .into_iter()
        .map(|(peer, held)| json!({ "peer": peer.0, "held": held.iter().map(|id| id.0).collect::<Vec<_>>() }))
        .collect();
    Ok(json!({
        "changed": changed,
        "mode": format!("{mode:?}"),
        "authority": match authority {
            Authority::Server => "server",
            Authority::Replica => "replica",
        },
        "phase": if mode.is_client() { net.client_phase() } else { "server" },
        "in_step": net.in_step(mode),
        "structure_hash": hex(structure_hash),
        "event_count": event_count,
        "config": {
            "max_ops_per_tick": config.max_ops_per_tick,
            "check_every_ticks": config.check_every_ticks,
            "pose_refresh_ticks": config.pose_refresh_ticks,
            "resync_cooldown_ticks": config.resync_cooldown_ticks,
            "resync_retry_ticks": config.resync_retry_ticks,
            "snapshot_parts_per_tick": config.snapshot_parts_per_tick,
            "snapshot_part_bytes": config.snapshot_part_bytes,
        },
        "stats": serde_json::to_value(net.stats()).unwrap_or(Value::Null),
        "relevance": { "anchors": anchors, "filtered_peers": filtered },
    }))
}

/// `physics.destruction_playback { list_bonds? }`: what this process plays, in a form an agent can
/// compare between processes (`book/agents/destruction-net.md`, ADR-2573).
pub(crate) fn destruction_playback(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list_bonds = crate::params::opt_bool(params, "list_bonds")?.unwrap_or(true);
    let Some(d) = world.get_resource::<Destruction>() else {
        return Err(not_installed());
    };
    let playback = regia_framework::destruction_net::destruction_playback(
        d,
        world.get_resource::<regia_physics::world::PhysicsWorld>(),
    );
    let mut value = serde_json::to_value(&playback)
        .map_err(|e| RpcError::invalid_params(format!("playback report: {e}")))?;
    if !list_bonds && let Some(instances) = value.get_mut("instances").and_then(Value::as_array_mut) {
        for instance in instances {
            if let Some(fields) = instance.as_object_mut() {
                fields.remove("broken_bonds");
            }
        }
    }
    Ok(value)
}

/// Most bonds `physics.destruction_load_report` lists per instance.
const MAX_LOAD_TOP: u32 = 256;

/// `physics.destruction_load_report { id?, top? }`: the structural load of one instance (or every
/// instance), solved now on its current bonds: the `top` bonds nearest their limit, what each
/// support carries, and the solve's figures (ADR-2734, `book/agents/destruction.md`).
pub(crate) fn destruction_load_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(d) = world.get_resource::<Destruction>() else {
        return Err(not_installed());
    };
    let only = id_param(params)?;
    let top = uint(params, "top")?.unwrap_or(10).min(MAX_LOAD_TOP) as usize;
    let ids: Vec<DestructibleId> = match only {
        Some(id) => vec![id],
        None => d.world.instances().map(|(id, _)| id).collect(),
    };
    let mut reports = Vec::with_capacity(ids.len());
    for id in ids {
        let report = d.world.load_report(id, top).map_err(|e| destruction_error(&e))?;
        reports.push(
            serde_json::to_value(&report)
                .map_err(|e| RpcError::invalid_params(format!("load report: {e}")))?,
        );
    }
    let stats = d.world.load_stats();
    Ok(json!({
        "instances": reports,
        "stats": stats,
        "authority": format!("{:?}", d.world.authority()).to_lowercase(),
    }))
}

fn fx_not_installed() -> RpcError {
    RpcError::not_found(
        "no DestructionFxState in this app",
        "add regia_framework::destruction_fx::DestructionFxPlugin (next to DestructionPlugin) to play destruction effects",
    )
}

/// `physics.destruction_fx_table { ron? }`: the destruction effects table; with `ron`, replaces it
/// first (a `.destructionfx.ron` text; the old table stays on an error).
pub(crate) fn destruction_fx_table(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::destruction_fx::{DestructionFxState, DestructionFxTable};
    let Some(mut state) = world.get_resource_mut::<DestructionFxState>() else {
        return Err(fx_not_installed());
    };
    let mut changed = false;
    if let Some(text) = params.get("ron") {
        let Some(text) = text.as_str() else {
            return Err(RpcError::invalid_params("ron must be a string"));
        };
        let table = DestructionFxTable::from_ron(text).map_err(|e| reason_error(e.code(), e.to_string()))?;
        state
            .player
            .set_table(table)
            .map_err(|e| reason_error(e.code(), e.to_string()))?;
        state.load_error = None;
        changed = true;
    }
    let table = state
        .player
        .table()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| RpcError::invalid_params(format!("fx table: {e}")))?;
    Ok(json!({
        "changed": changed,
        "table": table,
        "load_error": state.load_error,
        "playback": regia_framework::destruction_fx::PLAYBACK,
    }))
}

/// `physics.destruction_fx_stats`: what the destruction effects planner and playback did.
pub(crate) fn destruction_fx_stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    use regia_framework::destruction_fx::DestructionFxState;
    let Some(state) = world.get_resource::<DestructionFxState>() else {
        return Err(fx_not_installed());
    };
    let plays: Vec<Value> = state
        .player
        .plays()
        .iter()
        .map(|p| json!({ "row": p.row, "kind": format!("{:?}", p.kind), "position": p.position, "scale": p.scale }))
        .collect();
    Ok(json!({
        "stats": state.player.stats(),
        "last_plays": plays,
        "table": state.player.table().map(|t| t.name.clone()),
        "load_error": state.load_error,
    }))
}

fn impact_not_installed() -> RpcError {
    RpcError::not_found(
        "no DestructionImpact resource in this app",
        "add regia_physics::destruction::DestructionPlugin (it inserts DestructionImpact) to turn contacts into hits",
    )
}

fn impact_json(impact: &DestructionImpact, authority: Authority) -> Value {
    let s = impact.settings();
    let hits: Vec<Value> = impact
        .last_hits()
        .iter()
        .map(|h| {
            json!({
                "id": h.destructible.0,
                "chunk": h.chunk,
                "impulse": h.impulse,
                "force": h.force,
                "speed": h.speed,
                "point": h.point,
                "direction": h.direction,
                "debris": h.debris,
                "queue": h.queue,
            })
        })
        .collect();
    let enabled: Vec<u32> = impact.enabled().map(|id| id.0).collect();
    json!({
        "settings": {
            "min_speed": s.min_speed,
            "cooldown_ticks": s.cooldown_ticks,
            "max_hits_per_tick": s.max_hits_per_tick,
            "debris_hits": s.debris_hits,
            "impulse_scale": s.impulse_scale,
        },
        "enabled": enabled,
        "authority": format!("{authority:?}").to_lowercase(),
        "last": impact.last_report(),
        "hits": hits,
        "cooling_pairs": impact.cooling_pairs(),
        "totals": impact.totals(),
    })
}

/// `physics.destruction_impact_damage`: impact damage (ADR-3272): the settings, the opted-in
/// structures, the last tick's hits and counters (dropped by the cap, below `min_impulse`, below
/// `min_speed`, cooling down, debris skipped) and the running totals.
pub(crate) fn destruction_impact_damage(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(authority) = world.get_resource::<Destruction>().map(|d| d.world.authority()) else {
        return Err(not_installed());
    };
    let Some(impact) = world.get_resource::<DestructionImpact>() else {
        return Err(impact_not_installed());
    };
    Ok(impact_json(impact, authority))
}

fn id_list(params: &Value, field: &str) -> Result<Vec<DestructibleId>, RpcError> {
    let Some(v) = params.get(field) else {
        return Ok(Vec::new());
    };
    let list = v
        .as_array()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an array of instance ids")))?;
    list.iter()
        .map(|x| {
            x.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(DestructibleId)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`{field}` must hold integers 0..=4294967295"))
                })
        })
        .collect()
}

/// `physics.destruction_impact_damage_set { min_speed?, cooldown_ticks?, max_hits_per_tick?,
/// debris_hits?, impulse_scale?, enable?, disable? }`: changes the given settings and opts
/// structures in or out (all checked before anything changes), then reports like
/// `physics.destruction_impact_damage`.
pub(crate) fn destruction_impact_damage_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(authority) = world.get_resource::<Destruction>().map(|d| d.world.authority()) else {
        return Err(not_installed());
    };
    let Some(current) = world
        .get_resource::<DestructionImpact>()
        .map(|i| i.settings().clone())
    else {
        return Err(impact_not_installed());
    };
    let mut s: ImpactSettings = current.clone();
    if let Some(v) = number(params, "min_speed")? {
        s.min_speed = v;
    }
    if let Some(v) = number(params, "impulse_scale")? {
        s.impulse_scale = v;
    }
    if let Some(v) = uint(params, "cooldown_ticks")? {
        s.cooldown_ticks = v;
    }
    if let Some(v) = uint(params, "max_hits_per_tick")? {
        s.max_hits_per_tick = v;
    }
    if let Some(v) = params.get("debris_hits") {
        s.debris_hits = v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`debris_hits` must be a boolean"))?;
    }
    s.validate().map_err(|e| reason_error(e.code(), e.to_string()))?;
    let enable = id_list(params, "enable")?;
    let disable = id_list(params, "disable")?;
    world.resource_scope(|world, mut impact: Mut<DestructionImpact>| {
        let Some(d) = world.get_resource::<Destruction>() else {
            return Err(not_installed());
        };
        if let Some(bad) = enable.iter().find(|id| d.world.instance(**id).is_none()) {
            let e = regia_physics::destruction::ImpactError::UnknownId(bad.0);
            return Err(reason_error(e.code(), e.to_string()));
        }
        let changed_settings = s != current;
        if changed_settings {
            impact
                .set_settings(s)
                .map_err(|e| reason_error(e.code(), e.to_string()))?;
        }
        let mut enabled = 0u32;
        for id in &enable {
            if impact
                .enable(&d.world, *id)
                .map_err(|e| reason_error(e.code(), e.to_string()))?
            {
                enabled += 1;
            }
        }
        let mut disabled = 0u32;
        for id in &disable {
            if impact.disable(*id) {
                disabled += 1;
            }
        }
        let mut v = impact_json(&impact, authority);
        if let Some(map) = v.as_object_mut() {
            map.insert(
                "changed".into(),
                json!({ "settings": changed_settings, "enabled": enabled, "disabled": disabled }),
            );
        }
        Ok(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_physics::PhysicsPlugin;
    use regia_physics::destruction::DestructionPlugin;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, PhysicsPlugin, DestructionPlugin::default()))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        app
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn run_ticks(app: &mut App, n: u64) {
        let target = app.world().resource::<Destruction>().world.tick() + n;
        let mut guard = 0;
        while app.world().resource::<Destruction>().world.tick() < target {
            app.update();
            guard += 1;
            assert!(guard < 1000, "fixed steps stopped");
        }
    }

    #[test]
    fn playback_reports_digests_and_leaves_the_list_out_on_request() {
        let mut world = World::new();
        let err = destruction_playback(&mut world, &json!({})).expect_err("no plugin");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        world.insert_resource(Destruction::new(Authority::Server));
        let v = destruction_playback(&mut world, &json!({})).expect("report");
        assert_eq!(v["broken_bond_count"], 0);
        assert_eq!(v["debris_count"], 0);
        assert_eq!(v["instances"], json!([]));
        for field in ["broken_bond_digest", "chunk_pose_digest", "structure_hash"] {
            assert!(
                v[field].as_str().is_some_and(|t| t.starts_with("0x")),
                "{field}: {v}"
            );
        }
        let asset = Arc::new(
            DestructibleAsset::box_grid(&BoxGrid {
                name: "wall".into(),
                counts: [2, 1, 1],
                cell: [0.5, 0.5, 0.5],
                density: 2000.0,
                strength_per_area: 200.0,
                support: regia_physics::destruction::GridSupport::Bottom,
                material: regia_physics::destruction::DestructionMaterial::default(),
            })
            .expect("grid"),
        );
        world
            .resource_mut::<Destruction>()
            .world
            .define(
                asset,
                regia_physics::destruction::DestructiblePose::at([0.0, 0.0, 0.0]),
                1,
            )
            .expect("define");
        let v = destruction_playback(&mut world, &json!({})).expect("report");
        assert_eq!(v["instances"][0]["broken_bonds"], json!([]));
        assert_eq!(v["instances"][0]["attached"], 2);
        let v = destruction_playback(&mut world, &json!({ "list_bonds": false })).expect("report");
        assert!(v["instances"][0].get("broken_bonds").is_none(), "{v}");
        assert_eq!(v["instances"][0]["attached"], 2);
    }

    #[test]
    fn define_damage_state_budget_round_trip() {
        let mut app = app();
        let r = destruction_define(app.world_mut(), &json!({ "sample": "wall_bridge" })).expect("define");
        assert_eq!(r["ids"], json!([0, 1]));
        let budget =
            destruction_budget(app.world_mut(), &json!({ "max_spawns_per_tick": 1 })).expect("budget");
        assert_eq!(budget["changed"], true);
        assert_eq!(budget["budget"]["max_spawns_per_tick"], 1);
        let q = destruction_damage(
            app.world_mut(),
            &json!({ "kind": "projectile", "origin": [0.1, 1.1, 5.0], "direction": [0, 0, -1], "impulse": 1500, "radius": 0.3 }),
        )
        .expect("damage");
        assert_eq!(q["pending"], 1);
        run_ticks(&mut app, 2);
        let s = destruction_state(app.world_mut(), &json!({ "id": 0 })).expect("state");
        assert_eq!(s["instances"][0]["name"], "sample_wall");
        assert_eq!(s["instances"][0]["detached_chunks"], json!([36]), "{s}");
        assert_eq!(s["instances"][0]["debris_chunks"], json!([36]), "{s}");
        assert_eq!(s["structure_hash"].as_str().map(str::len), Some(16));
        let kinds: Vec<&str> = s["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|e| e["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"chunk_detached"), "{s}");
        let all = destruction_state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(all["instances"].as_array().map(Vec::len), Some(2));
        assert_eq!(all["counters"]["chunks_detached"], 1);
        let pool = destruction_budget(app.world_mut(), &json!({})).expect("budget");
        assert_eq!(pool["changed"], false);
        assert_eq!(pool["pool"]["active"], 1);
    }

    #[test]
    fn grid_and_asset_define_and_errors_carry_reasons() {
        let mut app = app();
        let r = destruction_define(
            app.world_mut(),
            &json!({ "grid": { "counts": [2, 2, 1], "support": "none" }, "position": [0, 0, 3] }),
        )
        .expect("grid");
        assert_eq!(r["ids"], json!([0]));
        let e = destruction_define(app.world_mut(), &json!({ "grid": { "counts": [0, 1, 1] } }))
            .expect_err("zero");
        assert_eq!(reason(&e).as_deref(), Some("destruction_invalid_asset"));
        let e = destruction_define(app.world_mut(), &json!({ "asset": "(format_version: 9)" }))
            .expect_err("version");
        assert_eq!(reason(&e).as_deref(), Some("destruction_version"));
        let e = destruction_define(app.world_mut(), &json!({})).expect_err("no source");
        assert!(reason(&e).is_none());
        let e = destruction_damage(
            app.world_mut(),
            &json!({ "kind": "impulse", "id": 0, "chunk": 99, "vector": [0, 1, 0] }),
        )
        .expect_err("chunk");
        assert_eq!(reason(&e).as_deref(), Some("destruction_unknown_chunk"));
        let e = destruction_damage(
            app.world_mut(),
            &json!({ "kind": "explosion", "id": 7, "center": [0, 0, 0], "radius": 1, "impulse": 1 }),
        )
        .expect_err("id");
        assert_eq!(reason(&e).as_deref(), Some("destruction_unknown_id"));
        let e = destruction_damage(
            app.world_mut(),
            &json!({ "kind": "explosion", "center": [0, 0, 0], "radius": 0, "impulse": 1 }),
        )
        .expect_err("radius");
        assert_eq!(reason(&e).as_deref(), Some("destruction_invalid_damage"));
        let e = destruction_state(app.world_mut(), &json!({ "id": 5 })).expect_err("state id");
        assert_eq!(reason(&e).as_deref(), Some("destruction_unknown_id"));
        let e = destruction_budget(app.world_mut(), &json!({ "max_lifetime_secs": -1 })).expect_err("neg");
        assert!(format!("{e:?}").contains("max_lifetime_secs"));
    }

    #[test]
    fn without_the_plugin_every_method_is_not_found() {
        let mut app = App::new();
        for f in [
            destruction_define,
            destruction_damage,
            destruction_state,
            destruction_budget,
        ] {
            let e = f(app.world_mut(), &json!({})).expect_err("no plugin");
            assert!(format!("{e:?}").contains("DestructionPlugin"), "{e:?}");
        }
    }

    #[test]
    fn define_validates_the_source_and_the_grid_before_adding_anything() {
        let mut app = app();
        let count = |app: &mut App| {
            destruction_state(app.world_mut(), &json!({}))
                .map_or(0, |s| s["instances"].as_array().map_or(0, Vec::len))
        };
        for bad in [
            json!({}),
            json!({ "sample": "wall_bridge", "grid": { "counts": [1, 1, 1] } }),
            json!({ "sample": "castle" }),
            json!({ "sample": 5 }),
            json!({ "grid": { "counts": [0, 1, 1] } }),
            json!({ "grid": { "counts": [1, 1] } }),
            json!({ "grid": { "counts": [1, 1, -1] } }),
            json!({ "grid": { "counts": [4_294_967_295_u64, 4_294_967_295_u64, 4_294_967_295_u64] } }),
            json!({ "grid": { "counts": [4_294_967_296_u64, 1, 1] } }),
            json!({ "grid": { "counts": [2, 2, 2], "support": 7 } }),
            json!({ "grid": { "counts": [2, 2, 2], "support": "floating" } }),
            json!({ "grid": { "counts": [2, 2, 2], "name": 3 } }),
            json!({ "grid": { "counts": [2, 2, 2], "fx_tag": [] } }),
            json!({ "grid": { "counts": [2, 2, 2], "cell": [0, 1, 1] } }),
            json!({ "grid": { "counts": [2, 2, 2], "density": 0 } }),
            json!({ "grid": { "counts": [2, 2, 2], "density": -3 } }),
            json!({ "grid": { "counts": [2, 2, 2], "strength_per_area": "x" } }),
            json!({ "grid": { "counts": [2, 2, 2] }, "seed": -1 }),
            json!({ "grid": { "counts": [2, 2, 2] }, "position": [1, 2] }),
        ] {
            let err = destruction_define(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        assert_eq!(count(&mut app), 0, "a refused define adds no instance");
        for support in ["bottom", "ends", "none"] {
            destruction_define(
                app.world_mut(),
                &json!({ "grid": { "counts": [2, 2, 2], "support": support, "name": support } }),
            )
            .expect(support);
        }
        assert_eq!(count(&mut app), 3);
    }

    #[test]
    fn the_net_method_reports_and_tunes_this_process() {
        let mut app = app();
        let world = app.world_mut();
        let err = destruction_net(world, &json!({})).expect_err("no plugin yet");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        regia_framework::destruction_net::install_destruction_net(world);
        world.insert_resource(NetMode::DedicatedServer);
        let v = destruction_net(world, &json!({})).expect("status");
        assert_eq!(v["phase"], "server");
        assert_eq!(v["in_step"], true);
        assert_eq!(v["changed"], false);
        assert_eq!(v["authority"], "server");
        assert_eq!(v["stats"]["peers"], 0);
        let v =
            destruction_net(world, &json!({ "max_ops_per_tick": 32, "check_every_ticks": 0 })).expect("set");
        assert_eq!(v["changed"], true);
        assert_eq!(v["config"]["max_ops_per_tick"], 32);
        assert_eq!(v["config"]["check_every_ticks"], 0);
        let v = destruction_net(world, &json!({ "snapshot_parts_per_tick": 4 })).expect("set");
        assert_eq!(v["config"]["snapshot_parts_per_tick"], 4);
        assert_eq!(v["config"]["snapshot_part_bytes"], 48 * 1024);
        let v = destruction_net(world, &json!({ "snapshot_part_bytes": 2048 })).expect("set");
        assert_eq!(v["config"]["snapshot_part_bytes"], 2048);
        for bad in [0, 1023, 48 * 1024 + 1] {
            destruction_net(world, &json!({ "snapshot_part_bytes": bad })).expect_err("range");
        }
        let v = destruction_net(world, &json!({})).expect("status");
        assert_eq!(
            v["config"]["snapshot_part_bytes"], 2048,
            "a refused call changes nothing"
        );
        assert_eq!(v["stats"]["relevance_peers"], 0);
        assert_eq!(v["stats"]["breaks_skipped"], 0);
        assert_eq!(v["relevance"]["anchors"], json!([]));
        let anchor = world.spawn_empty().id();
        world
            .resource_mut::<DestructionNet>()
            .set_relevance_anchor(regia_physics::destruction::DestructibleId(3), anchor);
        let v = destruction_net(world, &json!({})).expect("status");
        assert_eq!(v["relevance"]["anchors"][0]["id"], 3);
        assert_eq!(v["relevance"]["anchors"][0]["anchor"], anchor.to_bits());
        assert_eq!(
            v["relevance"]["filtered_peers"],
            json!([]),
            "no session, no peers"
        );
        assert_eq!(v["stats"]["snapshots_in_progress"], 0);
        for bad in [0, MAX_SNAPSHOT_PARTS_PER_TICK + 1] {
            destruction_net(world, &json!({ "snapshot_parts_per_tick": bad })).expect_err("range");
        }
        let err = destruction_net(world, &json!({ "max_ops_per_tick": 0 })).expect_err("range");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let err = destruction_net(world, &json!({ "max_ops_per_tick": 100_000 })).expect_err("range");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let err = destruction_net(world, &json!({ "resync": true })).expect_err("server");
        assert_eq!(reason(&err).as_deref(), Some("destruction_not_client"));
        // A refused call changes nothing, whichever parameter is the bad one.
        for refused in [
            json!({ "max_ops_per_tick": 7, "snapshot_parts_per_tick": 0 }),
            json!({ "max_ops_per_tick": 7, "resync": true }),
            json!({ "snapshot_parts_per_tick": 9, "max_ops_per_tick": 0 }),
        ] {
            destruction_net(world, &refused).expect_err("refused");
            let v = destruction_net(world, &json!({})).expect("status");
            assert_eq!(v["config"]["max_ops_per_tick"], 32, "{refused}");
            assert_eq!(v["config"]["snapshot_parts_per_tick"], 4, "{refused}");
        }
        world.insert_resource(NetMode::Client);
        let v = destruction_net(world, &json!({})).expect("status");
        assert_eq!(v["phase"], "waiting");
        assert_eq!(v["in_step"], false);
        let v = destruction_net(world, &json!({ "resync": true })).expect("resync");
        assert_eq!(v["phase"], "resyncing");
        assert_eq!(v["stats"]["last_problem"], "destruction_resync_asked");
    }

    #[test]
    fn load_report_and_fx_methods_round_trip() {
        use regia_framework::destruction_fx::DestructionFxPlugin;
        let mut app = app();
        // Without the fx plugin both fx methods are not found.
        for f in [destruction_fx_stats, destruction_fx_table] {
            let e = f(app.world_mut(), &json!({})).expect_err("no fx plugin");
            assert!(format!("{e:?}").contains("DestructionFxPlugin"), "{e:?}");
        }
        app.add_plugins(DestructionFxPlugin::default());
        let v = destruction_define(
            app.world_mut(),
            &json!({ "grid": { "counts": [1, 5, 1], "support": "bottom" } }),
        )
        .expect("define");
        let id = v["ids"][0].as_u64().expect("id");
        run_ticks(&mut app, 2);
        let r = destruction_load_report(app.world_mut(), &json!({ "id": id, "top": 2 })).expect("report");
        let inst = &r["instances"][0];
        assert_eq!(inst["enabled"], false, "{r}");
        assert_eq!(inst["top"].as_array().map(Vec::len), Some(2));
        assert_eq!(inst["supports"].as_array().map(Vec::len), Some(1));
        let e = destruction_load_report(app.world_mut(), &json!({ "id": 99 })).expect_err("unknown");
        assert_eq!(reason(&e).as_deref(), Some("destruction_unknown_id"));
        // The fx table: empty, then set from RON, then a bad one refused with its code.
        let t = destruction_fx_table(app.world_mut(), &json!({})).expect("table");
        assert_eq!(t["table"], Value::Null);
        let ron = r#"(format_version: 1, name: "t", rows: [(tag: "default", sound: Some("crack"))])"#;
        let t = destruction_fx_table(app.world_mut(), &json!({ "ron": ron })).expect("set");
        assert_eq!(t["changed"], true);
        assert_eq!(t["table"]["rows"][0]["tag"], "default");
        let e =
            destruction_fx_table(app.world_mut(), &json!({ "ron": "(format_version: 9)" })).expect_err("v9");
        assert_eq!(reason(&e).as_deref(), Some("destruction_fx_version"));
        let e = destruction_fx_table(app.world_mut(), &json!({ "ron": "(nope" })).expect_err("bad");
        assert_eq!(reason(&e).as_deref(), Some("destruction_fx_parse"));
        let dup = r#"(format_version: 1, name: "t", rows: [(tag: "a"), (tag: "a")])"#;
        let e = destruction_fx_table(app.world_mut(), &json!({ "ron": dup })).expect_err("dup");
        assert_eq!(reason(&e).as_deref(), Some("destruction_fx_invalid"));
        // A hit that breaks bonds reaches the planner through the fx messages.
        destruction_damage(
            app.world_mut(),
            &json!({ "kind": "impulse", "id": id, "chunk": 2, "vector": [0, 0, 1e6] }),
        )
        .expect("hit");
        run_ticks(&mut app, 3);
        let s = destruction_fx_stats(app.world_mut(), &json!({})).expect("stats");
        assert!(s["stats"]["messages"].as_u64().unwrap_or(0) > 0, "{s}");
        assert!(s["stats"]["planned"].as_u64().unwrap_or(0) > 0, "{s}");
        assert_eq!(s["table"], "t");
    }

    /// The two fuzz targets of ADR-2734 under the registry's seeded mutation, on their own (the
    /// registry test runs every target in one loop): no panic, every accepted mutation round trips.
    #[test]
    fn destruction_fuzz_targets_survive_seeded_mutation() {
        for name in ["destructible", "destructible.load", "destructionfx"] {
            let target = crate::fuzz_targets::target(name).expect("registered");
            let report = crate::fuzz_targets::run(target, None, 0xF0_2734, 1000).expect("fixture");
            assert!(report.is_clean(), "{name}: {report:?}");
            assert!(report.accepted > 0, "{name}: no mutation loaded");
        }
    }

    #[test]
    fn a_world_without_destruction_is_not_found_for_define() {
        let err =
            destruction_define(&mut World::new(), &json!({ "sample": "wall_bridge" })).expect_err("bare");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
    }

    #[test]
    fn impact_damage_methods_round_trip_and_refuse_with_codes() {
        let mut app = app();
        let v = destruction_define(
            app.world_mut(),
            &json!({ "grid": { "counts": [2, 2, 1], "support": "bottom" } }),
        )
        .expect("define");
        let id = v["ids"][0].as_u64().expect("id");
        let r = destruction_impact_damage(app.world_mut(), &json!({})).expect("read");
        assert_eq!(r["enabled"], json!([]), "{r}");
        assert_eq!(r["settings"]["debris_hits"], false);
        assert_eq!(r["settings"]["max_hits_per_tick"], 32);
        assert_eq!(r["last"]["dropped_by_cap"], 0);
        assert_eq!(r["authority"], "server");
        let r = destruction_impact_damage_set(
            app.world_mut(),
            &json!({ "enable": [id], "min_speed": 3.5, "max_hits_per_tick": 4, "debris_hits": true }),
        )
        .expect("set");
        assert_eq!(r["enabled"], json!([id]), "{r}");
        assert_eq!(r["settings"]["min_speed"], 3.5);
        assert_eq!(r["settings"]["max_hits_per_tick"], 4);
        assert_eq!(r["settings"]["debris_hits"], true);
        assert_eq!(r["changed"]["enabled"], 1);
        // Refused as a whole: nothing changes.
        let e =
            destruction_impact_damage_set(app.world_mut(), &json!({ "min_speed": -1.0, "disable": [id] }))
                .expect_err("negative");
        assert_eq!(reason(&e).as_deref(), Some("destruction_impact_invalid"));
        let e = destruction_impact_damage_set(app.world_mut(), &json!({ "max_hits_per_tick": 100000 }))
            .expect_err("cap");
        assert_eq!(reason(&e).as_deref(), Some("destruction_impact_invalid"));
        let e = destruction_impact_damage_set(app.world_mut(), &json!({ "enable": [99], "min_speed": 1.0 }))
            .expect_err("unknown");
        assert_eq!(reason(&e).as_deref(), Some("destruction_unknown_id"));
        let r = destruction_impact_damage(app.world_mut(), &json!({})).expect("read");
        assert_eq!(r["settings"]["min_speed"], 3.5, "{r}");
        assert_eq!(r["enabled"], json!([id]));
        let r = destruction_impact_damage_set(app.world_mut(), &json!({ "disable": [id] })).expect("off");
        assert_eq!(r["enabled"], json!([]));
        assert_eq!(r["changed"]["disabled"], 1);
        run_ticks(&mut app, 3);
        let r = destruction_impact_damage(app.world_mut(), &json!({})).expect("read");
        assert_eq!(r["totals"]["ticks"], 0, "nothing enabled, nothing read: {r}");
    }
}
