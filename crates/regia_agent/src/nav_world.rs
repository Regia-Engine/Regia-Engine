// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the nav world work (ADR-1934, `book/agents/navigation-world.md`):
//! region rebuilds (`nav.region_*`) and tile streaming (`nav.stream_*`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{NavDirtySource, NavRegionQueue, NavTileCache, NavTileCacheError, NavTileCacheParams};
use serde_json::{Value, json};

use crate::nav::{nav_error, no_tile_cache, tile_cache_error, vec3_optional, vec3_param, xz_pair};
use crate::protocol::RpcError;

/// The largest cooked cell `nav.stream_load_cell` takes, in bytes of decoded blob.
const MAX_CELL_BLOB: usize = 64 * 1024 * 1024;

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn cache_error(e: &NavTileCacheError) -> RpcError {
    match e {
        NavTileCacheError::UnknownCell(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => tile_cache_error(e),
    }
}

fn no_region_queue() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_region_queue",
        "no NavRegionQueue in this app: add regia_nav::NavRegionPlugin or regia_framework::nav_world::NavWorldPlugin",
    )
}

/// The optional `update` switch (default true).
fn update_param(params: &Value) -> Result<bool, RpcError> {
    match params.get("update") {
        None => Ok(true),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`update` must be true or false")),
    }
}

fn u64_param(params: &Value, field: &str) -> Result<u64, RpcError> {
    params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (a non-negative integer) is required")))
}

/// Parses `triangles`: an array of triangles, each three `[x, y, z]`.
fn triangles_param(value: &Value) -> Result<Vec<[Vec3; 3]>, RpcError> {
    let invalid = || RpcError::invalid_params("`triangles` must be an array of three [x, y, z] corners each");
    let items = value.as_array().ok_or_else(invalid)?;
    let mut out = Vec::with_capacity(items.len());
    for tri in items {
        let corners = tri.as_array().filter(|c| c.len() == 3).ok_or_else(invalid)?;
        let mut v = [Vec3::ZERO; 3];
        for (slot, corner) in v.iter_mut().zip(corners) {
            let c = corner.as_array().filter(|c| c.len() == 3).ok_or_else(invalid)?;
            let n = |i: usize| c[i].as_f64().map(|f| f as f32).ok_or_else(invalid);
            *slot = Vec3::new(n(0)?, n(1)?, n(2)?);
        }
        out.push(v);
    }
    Ok(out)
}

/// `nav.region_status`: the queue, its budgets and its counters.
pub fn region_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(queue) = world.get_resource::<NavRegionQueue>() else {
        return Err(no_region_queue());
    };
    let s = queue.stats();
    Ok(json!({
        "queued": queue
            .boxes()
            .map(|(min, max, source)| json!({ "min": vec3_json(min), "max": vec3_json(max), "source": source }))
            .collect::<Vec<_>>(),
        "has_source": queue.has_source(),
        "max_slices_per_step": queue.config.max_slices_per_step,
        "max_tiles_per_step": queue.config.max_tiles_per_step,
        "max_queued": queue.config.max_queued,
        "received": s.received,
        "covered": s.covered,
        "overflowed": s.overflowed,
        "rejected": s.rejected,
        "slices": s.slices,
        "cells_changed": s.cells_changed,
        "source_waits": s.source_waits,
        "failed": s.failed,
    }))
}

/// `nav.region_rebuild`: with `triangles`, re-voxelizes the box at once from them; without, queues
/// the box for the next fixed step.
pub fn region_rebuild(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (min, max) = (vec3_param(params, "min")?, vec3_param(params, "max")?);
    let Some(triangles) = params.get("triangles") else {
        let Some(mut queue) = world.get_resource_mut::<NavRegionQueue>() else {
            return Err(no_region_queue());
        };
        queue.push(min, max, NavDirtySource::Agent);
        return Ok(json!({ "queued": true, "waiting": queue.len() }));
    };
    let triangles = triangles_param(triangles)?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let done = cache
        .rebuild_region(min, max, &triangles)
        .map_err(|e| cache_error(&e))?;
    let rebuilt = if update {
        cache.update().rebuilt
    } else {
        Vec::new()
    };
    Ok(json!({
        "queued": false,
        "tiles": done.queued.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "cells_changed": done.cells_changed,
        "outside": done.outside,
        "deferred": done.deferred.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "rebuilt": rebuilt.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "pending": cache.pending_tiles().into_iter().map(xz_pair).collect::<Vec<_>>(),
        "revision": cache.revision(),
    }))
}

fn cache_ref(world: &World) -> Result<&NavTileCache, RpcError> {
    world.get_resource::<NavTileCache>().ok_or_else(no_tile_cache)
}

/// `nav.stream_status`: the streaming counters and the cells with their gate state.
pub fn stream_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let cache = cache_ref(world)?;
    let s = cache.stream_status();
    let layout = cache.layout();
    Ok(json!({
        "streamed": s.streamed,
        "tiles_total": s.tiles_total,
        "tiles_loaded": s.tiles_loaded,
        "cells_loaded": s.cells_loaded,
        "edited_tiles": s.edited_tiles,
        "replay_pending": s.replay_pending,
        "cells": cache
            .loaded_cells()
            .into_iter()
            .map(|cell| json!({ "cell": cell, "ready": cache.cell_ready(cell) }))
            .collect::<Vec<_>>(),
        "origin": vec3_json(layout.origin),
        "width": layout.width,
        "height": layout.height,
        "format_version": regia_nav::NAV_CELL_FORMAT_VERSION,
    }))
}

/// `nav.stream_begin`: replaces the cache with an empty streamed cache of the same layout.
pub fn stream_begin(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let cache = cache_ref(world)?;
    let params: NavTileCacheParams = cache.params().clone();
    let layout = cache.layout();
    let fresh = NavTileCache::streamed(params, layout).map_err(|e| cache_error(&e))?;
    let tiles = fresh.tile_count();
    world.insert_resource(fresh);
    Ok(json!({ "streamed": true, "tiles_total": tiles }))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digits = text.as_bytes();
    let value = |d: u8| char::from(d).to_digit(16);
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[hi, lo]| Some((value(*hi)? * 16 + value(*lo)?) as u8))
        .collect()
}

/// `nav.stream_cook_cell`: the tiles under `min`..`max` as one cooked cell, in hex.
pub fn stream_cook_cell(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (min, max) = (vec3_param(params, "min")?, vec3_param(params, "max")?);
    let cache = cache_ref(world)?;
    let tiles = cache.tiles_in(min, max);
    let blob = cache.cook_cell(&tiles).map_err(|e| cache_error(&e))?;
    Ok(json!({
        "tiles": tiles.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "bytes": blob.len(),
        "blob": hex_encode(&blob),
    }))
}

/// `nav.stream_load_cell`: loads a cooked cell (`blob` in hex) and rebuilds its tiles unless
/// `update` is false.
pub fn stream_load_cell(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cell = u64_param(params, "cell")?;
    let text = params
        .get("blob")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`blob` (hex from nav.stream_cook_cell) is required"))?;
    if text.len() / 2 > MAX_CELL_BLOB {
        return Err(RpcError::invalid_params("`blob` is larger than 64 MiB"));
    }
    let blob = hex_decode(text).ok_or_else(|| RpcError::invalid_params("`blob` must be hex"))?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let load = cache.load_cell(cell, &blob).map_err(|e| cache_error(&e))?;
    if update {
        cache.update();
    }
    Ok(json!({
        "tiles": load.tiles.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "added": load.added.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "already_loaded": load.already_loaded,
        "ready": cache.cell_ready(cell),
        "revision": cache.revision(),
    }))
}

/// `nav.stream_unload_cell`: unloads a cell and rebuilds its tiles unless `update` is false.
pub fn stream_unload_cell(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cell = u64_param(params, "cell")?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let removed = cache.unload_cell(cell).map_err(|e| cache_error(&e))?;
    if update {
        cache.update();
    }
    Ok(json!({
        "removed": removed.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "revision": cache.revision(),
    }))
}

/// `nav.stream_find_path`: a path that stops at the loaded surface when its end is not reachable.
pub fn stream_find_path(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let start = vec3_param(params, "start")?;
    let end = vec3_param(params, "end")?;
    let extents = vec3_optional(params, "extents", Vec3::new(2.0, 4.0, 2.0))?;
    let filter = crate::nav_lanes::filter_param(params)?.unwrap_or_default();
    let cache = cache_ref(world)?;
    let result = cache.find_path_streamed_filtered(start, end, extents, &filter);
    Ok(json!({
        "status": result.outcome.name(),
        "waypoints": result.path.as_ref().map(|p| p.waypoints.iter().copied().map(vec3_json).collect::<Vec<_>>()),
        "length": result.path.as_ref().map(|p| p.length),
        "unloaded_tile": result.unloaded_tile.map(xz_pair),
    }))
}

#[cfg(test)]
mod tests {
    use regia_nav::{NavMeshBuildParams, NavRegionPlugin};

    use super::*;

    fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<[Vec3; 3]> {
        vec![
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
        ]
    }

    fn params() -> NavTileCacheParams {
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 64,
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(NavRegionPlugin);
        let floor = quad(-16.0, 16.0, -16.0, 16.0, 0.0);
        app.insert_resource(NavTileCache::build(params(), &floor).expect("builds"));
        app
    }

    fn triangles_json(tris: &[[Vec3; 3]]) -> Value {
        json!(
            tris.iter()
                .map(|t| t.map(|v| [v.x, v.y, v.z]))
                .collect::<Vec<_>>()
        )
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn a_region_rebuild_with_triangles_changes_the_mesh_at_once() {
        let mut app = app();
        let w = app.world_mut();
        let mut tris = quad(-16.0, 16.0, -16.0, 16.0, 0.0);
        tris.extend(quad(-0.5, 0.5, -16.0, 16.0, 3.0));
        let out = region_rebuild(
            w,
            &json!({ "min": [-0.5, 0, -16], "max": [0.5, 3, 16], "triangles": triangles_json(&tris) }),
        )
        .expect("rebuilds");
        assert_eq!(out["queued"], json!(false));
        assert!(out["cells_changed"].as_u64().expect("a count") > 0);
        assert!(!out["rebuilt"].as_array().expect("tiles").is_empty());
        let cache = w.resource::<NavTileCache>();
        assert!(cache.revision() > 0);
        let (a, b) = (Vec3::new(-8.0, 0.0, 0.0), Vec3::new(8.0, 0.0, 0.0));
        assert!(
            cache.query().find_path(a, b, Vec3::splat(2.0)).is_none(),
            "the wall stands"
        );
    }

    #[test]
    fn a_region_rebuild_without_triangles_queues_and_the_status_shows_it() {
        let mut app = app();
        let w = app.world_mut();
        let out = region_rebuild(w, &json!({ "min": [0, 0, 0], "max": [1, 1, 1] })).expect("queues");
        assert_eq!(out, json!({ "queued": true, "waiting": 1 }));
        let status = region_status(w, &json!({})).expect("status");
        assert_eq!(status["queued"][0]["source"], json!("agent"));
        assert_eq!(status["received"], json!(1));
        assert_eq!(status["has_source"], json!(false));
    }

    #[test]
    fn region_errors_carry_reasons() {
        let mut bare = App::new();
        let e = region_status(bare.world_mut(), &json!({})).expect_err("no queue");
        assert_eq!(reason(&e), "nav_no_region_queue");
        let e = region_rebuild(bare.world_mut(), &json!({ "min": [0, 0, 0], "max": [1, 1, 1] }))
            .expect_err("no queue");
        assert_eq!(reason(&e), "nav_no_region_queue");
        let e = region_rebuild(
            bare.world_mut(),
            &json!({ "min": [0, 0, 0], "max": [1, 1, 1], "triangles": [] }),
        )
        .expect_err("no cache");
        assert_eq!(reason(&e), "nav_no_tile_cache");
        let mut app = app();
        let e = region_rebuild(
            app.world_mut(),
            &json!({ "min": [0, 0, 0], "max": [1, 1, 1], "triangles": [[1, 2]] }),
        )
        .expect_err("bad triangles");
        assert!(e.message.contains("triangles"));
    }

    #[test]
    fn cells_cook_load_and_unload_through_the_protocol() {
        let mut app = app();
        let w = app.world_mut();
        let cooked =
            stream_cook_cell(w, &json!({ "min": [-16, 0, -16], "max": [-2, 0, -2] })).expect("cooks");
        let east = stream_cook_cell(w, &json!({ "min": [2, 0, -16], "max": [12, 0, -2] })).expect("cooks");
        assert_eq!(cooked["tiles"], json!([[0, 0]]));
        assert_eq!(
            cooked["bytes"].as_u64().map(|b| b as usize),
            cooked["blob"].as_str().map(|h| h.len() / 2)
        );

        assert_eq!(
            stream_status(w, &json!({})).expect("status")["streamed"],
            json!(false)
        );
        assert_eq!(
            stream_begin(w, &json!({})).expect("begins")["streamed"],
            json!(true)
        );
        assert_eq!(w.resource::<NavTileCache>().mesh().poly_count(), 0);

        let load = stream_load_cell(w, &json!({ "cell": 7, "blob": cooked["blob"] })).expect("loads");
        assert_eq!(load["added"], json!([[0, 0]]));
        assert_eq!(load["ready"], json!(true));
        assert_eq!(
            stream_status(w, &json!({})).expect("status")["cells"],
            json!([{ "cell": 7, "ready": true }])
        );

        let path = stream_find_path(w, &json!({ "start": [-8, 0, -8], "end": [8, 0, -8] })).expect("a query");
        assert_eq!(path["status"], json!("nav_tile_unloaded"));
        assert_eq!(path["unloaded_tile"], json!([1, 0]));
        stream_load_cell(w, &json!({ "cell": 8, "blob": east["blob"] })).expect("loads");
        let path = stream_find_path(w, &json!({ "start": [-8, 0, -8], "end": [8, 0, -8] })).expect("a query");
        assert_eq!(path["status"], json!("complete"));

        let gone = stream_unload_cell(w, &json!({ "cell": 7 })).expect("unloads");
        assert_eq!(gone["removed"], json!([[0, 0]]));
        let e = stream_unload_cell(w, &json!({ "cell": 7 })).expect_err("gone");
        assert_eq!(reason(&e), "nav_unknown_cell");
        assert_eq!(e.code, crate::protocol::NOT_FOUND);
    }

    #[test]
    fn stream_errors_carry_reasons() {
        let mut app = app();
        let w = app.world_mut();
        let blob = stream_cook_cell(w, &json!({ "min": [-16, 0, -16], "max": [-2, 0, -2] })).expect("cooks");
        let e = stream_load_cell(w, &json!({ "cell": 1, "blob": blob["blob"] })).expect_err("whole cache");
        assert_eq!(reason(&e), "nav_not_streamed");
        stream_begin(w, &json!({})).expect("begins");
        let e = stream_load_cell(w, &json!({ "cell": 1, "blob": "00ff" })).expect_err("garbage");
        assert_eq!(reason(&e), "nav_cell_corrupt");
        assert!(
            stream_load_cell(w, &json!({ "cell": 1, "blob": "zz" })).is_err(),
            "not hex"
        );
        assert!(
            stream_load_cell(w, &json!({ "cell": 1, "blob": "abc" })).is_err(),
            "odd length"
        );
        assert!(stream_load_cell(w, &json!({ "blob": "00" })).is_err(), "no cell");
        let e = stream_unload_cell(w, &json!({ "cell": 1, "update": "yes" })).expect_err("a bad switch");
        assert!(e.message.contains("update"));
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&bytes)).as_deref(), Some(bytes.as_slice()));
    }
}
