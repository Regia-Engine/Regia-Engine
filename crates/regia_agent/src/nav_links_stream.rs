// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of jump arcs, streamed links and grid growth (ADR-2589,
//! `book/agents/navigation-links.md`): the agents on links with their arc progress, the ballistic
//! crossing settings, the links each tile streams, and tile-grid growth.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{NavLinkArcConfig, NavLinkTraversal, NavTileCache};
use serde_json::{Value, json};

use crate::nav::{
    bool_field, crowd_error, f32_param, no_crowd, no_tile_cache, tile_cache_error, update_json, vec3_param,
    with_crowd_cache, xz_pair,
};
use crate::protocol::RpcError;

fn v3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn arcs_json(a: &NavLinkArcConfig) -> Value {
    json!({
        "enabled": a.enabled,
        "gravity": a.gravity,
        "max_jump_speed": a.max_jump_speed,
        "apex_clearance": a.apex_clearance,
    })
}

/// A crossing's link index, or `null` once its link left the mesh ([`regia_nav::NAV_LINK_GONE`],
/// `usize::MAX`: above 2^53, so a JSON client would read a rounded number).
fn link_index_json(link: usize) -> Value {
    if link == regia_nav::NAV_LINK_GONE {
        Value::Null
    } else {
        json!(link)
    }
}

/// One take-off of the last step.
fn takeoff_json(t: &NavLinkTraversal) -> Value {
    json!({ "link": link_index_json(t.link), "user_id": t.user_id, "kind": t.kind.name(), "total_steps": t.total_steps })
}

/// One crossing: where it is, its timing, and its arc (`null` for a straight crossing).
fn traversal_json(agent: u64, t: &NavLinkTraversal) -> Value {
    json!({
        "agent": agent,
        "link": link_index_json(t.link),
        "user_id": t.user_id,
        "kind": t.kind.name(),
        "progress": t.progress(),
        "steps": t.steps,
        "total_steps": t.total_steps,
        "elapsed": t.elapsed(),
        "duration": t.duration(),
        "position": v3(t.position()),
        "origin": v3(t.origin),
        "start": v3(t.start),
        "end": v3(t.end),
        "arc": t.arc.map(|a| json!({
            "apex": v3(a.apex),
            "takeoff_velocity": v3(a.takeoff_velocity),
            "gravity": a.gravity,
            "flight_seconds": a.flight_seconds,
            "flight_time": t.flight_time(),
            "walk_seconds": t.walk_seconds,
            "velocity": t.flight_time().map(|ft| v3(a.velocity_at(ft))),
            "speed_limited": a.speed_limited,
        })),
    })
}

/// `nav.link_traversals`: every crowd agent on an off-mesh link with its arc progress, the
/// take-offs of the last step, and the arc settings.
pub fn link_traversals(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(crowd) = world.get_resource::<regia_nav::NavCrowd>() else {
        return Err(no_crowd());
    };
    let s = crowd.stats();
    Ok(json!({
        "agents": crowd
            .link_traversals()
            .iter()
            .map(|(id, t)| traversal_json(id.0, t))
            .collect::<Vec<_>>(),
        "takeoffs": crowd
            .link_takeoffs()
            .iter()
            .map(takeoff_json)
            .collect::<Vec<_>>(),
        "arcs": arcs_json(&crowd.config().link_arcs),
        "finished": s.link_traversals,
        "cancelled": s.link_cancellations,
    }))
}

/// `nav.crowd_set_link_arcs`: Jump links along a ballistic arc and Drop links along a falling
/// parabola (or straight again with `enabled` false). Creates the crowd when there is none.
pub fn crowd_set_link_arcs(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`enabled` (true or false) is required"))?;
    let d = NavLinkArcConfig::default();
    let arcs = NavLinkArcConfig {
        enabled,
        gravity: f32_param(params, "gravity", d.gravity)?,
        max_jump_speed: f32_param(params, "max_jump_speed", d.max_jump_speed)?,
        apex_clearance: f32_param(params, "apex_clearance", d.apex_clearance)?,
    };
    with_crowd_cache(world, true, |crowd, _, _| {
        crowd.set_link_arcs(arcs).map_err(|e| crowd_error(&e))?;
        Ok(arcs_json(&crowd.config().link_arcs))
    })
}

/// `nav.tile_links`: the off-mesh links each tile owns (the tile under the link's `from`), whether
/// it is loaded, and how many of its links are graph edges or wait for their landing tile.
pub fn tile_links(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(cache) = world.get_resource::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let rows = cache.link_tiles();
    let tiles: Vec<Value> = rows
        .iter()
        .map(|r| {
            let (tx, tz) = r.tile;
            json!({
                "tile": xz_pair(r.tile),
                "loaded": r.loaded,
                "links": r.links,
                "streamed": r.streamed,
                "active": r.active,
                "waiting": r.waiting,
                "user_ids": cache.tile_links(tx, tz).iter().map(|l| l.user_id).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "streamed": cache.is_streamed(),
        "tiles": tiles,
        "cache_links": cache.off_mesh_links().len(),
        "mesh_links": cache.mesh().off_mesh_links.len(),
        "waiting": rows.iter().map(|r| r.waiting).sum::<usize>(),
    }))
}

/// `nav.tile_cache_grow`: grows the tile grid by whole tiles to cover `min`..`max` (a cache built
/// whole; a streamed one is refused with `nav_grid_growth`).
pub fn tile_cache_grow(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let min = vec3_param(params, "min")?;
    let max = vec3_param(params, "max")?;
    let update = bool_field(params, "update", true)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let growth = cache.grow_to(min, max).map_err(|e| tile_cache_error(&e))?;
    let done = update_json(&mut cache, update);
    Ok(json!({
        "unchanged": growth.unchanged,
        "added": { "west": growth.added[0], "east": growth.added[1], "south": growth.added[2], "north": growth.added[3] },
        "tile_offset": xz_pair(growth.tile_offset),
        "tiles_before": xz_pair(growth.tiles_before),
        "tiles_after": xz_pair(growth.tiles_after),
        "repolygonised": growth.rebuilt.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "rebuilt": done["rebuilt"],
        "pending": done["pending"],
        "revision": cache.revision(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{crowd_add_agent, crowd_set_target, crowd_step, set_links};

    fn reason(e: &RpcError) -> Option<&str> {
        e.data.as_ref()?.get("reason")?.as_str()
    }

    /// Two 5 x 8 m floors with a 2 m gap, as a tile cache of 2 m tiles.
    fn islands() -> App {
        let mut tris = Vec::new();
        for (x0, x1) in [(-6.0f32, -1.0f32), (1.0, 6.0)] {
            tris.push([
                Vec3::new(x0, 0.0, -4.0),
                Vec3::new(x1, 0.0, -4.0),
                Vec3::new(x1, 0.0, 4.0),
            ]);
            tris.push([
                Vec3::new(x0, 0.0, -4.0),
                Vec3::new(x1, 0.0, 4.0),
                Vec3::new(x0, 0.0, 4.0),
            ]);
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let cache = NavTileCache::build(
            regia_nav::NavTileCacheParams {
                tile_size: 8,
                ..Default::default()
            },
            &tris,
        )
        .expect("builds");
        app.insert_resource(cache);
        app
    }

    #[test]
    fn an_agent_jumps_along_an_arc_the_agent_can_watch() {
        let mut app = islands();
        let w = app.world_mut();
        set_links(
            w,
            &json!({ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "radius": 0.5, "user_id": 4 } ] }),
        )
        .expect("links");
        let e = link_traversals(w, &json!({})).expect_err("no crowd yet");
        assert_eq!(reason(&e), Some("nav_no_crowd"));
        let arcs = crowd_set_link_arcs(w, &json!({ "enabled": true, "apex_clearance": 0.6 })).expect("sets");
        assert_eq!(arcs["enabled"], true);
        let a = crowd_add_agent(w, &json!({ "position": [-4, 0, 0] })).expect("adds");
        crowd_set_target(w, &json!({ "id": a["id"], "target": [4, 0, 0] })).expect("target");
        let mut highest = f64::MIN;
        let mut seen = false;
        for _ in 0..120 {
            crowd_step(w, &json!({ "steps": 2 })).expect("steps");
            let t = link_traversals(w, &json!({})).expect("status");
            if let Some(on) = t["agents"].as_array().and_then(|a| a.first()) {
                seen = true;
                assert_eq!(on["user_id"], 4);
                assert_eq!(on["kind"], "jump");
                let arc = &on["arc"];
                assert!(arc["flight_seconds"].as_f64().is_some_and(|f| f > 0.0), "{on}");
                highest = highest.max(on["position"][1].as_f64().unwrap_or(f64::MIN));
                let apex = arc["apex"][1].as_f64().unwrap_or(0.0);
                assert!(highest <= apex + 1e-3, "never above the apex");
            }
        }
        assert!(seen, "the agent crossed");
        let t = link_traversals(w, &json!({})).expect("status");
        assert_eq!(t["finished"], 1);
        assert!(highest > 0.3, "it rose along the arc: {highest}");
        let bad = crowd_set_link_arcs(w, &json!({ "enabled": true, "gravity": 0 })).expect_err("gravity 0");
        assert_eq!(reason(&bad), Some("nav_invalid_crowd_config"));
        let missing = crowd_set_link_arcs(w, &json!({})).expect_err("enabled is required");
        assert_eq!(missing.code, crate::protocol::INVALID_PARAMS);
    }

    #[test]
    fn tile_links_and_growth_report_per_tile() {
        let mut app = islands();
        let w = app.world_mut();
        set_links(
            w,
            &json!({ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "radius": 0.5, "user_id": 4 } ] }),
        )
        .expect("links");
        let t = tile_links(w, &json!({})).expect("status");
        assert_eq!(t["streamed"], false);
        assert_eq!(t["cache_links"], 1);
        let tiles = t["tiles"].as_array().cloned().unwrap_or_default();
        assert_eq!(tiles.len(), 1, "{t}");
        assert_eq!(
            (tiles[0]["links"].as_u64(), tiles[0]["active"].as_u64()),
            (Some(1), Some(1))
        );
        let g = tile_cache_grow(w, &json!({ "min": [6, 0, -4], "max": [12, 0, 4] })).expect("grows");
        assert_eq!(g["unchanged"], false);
        assert_eq!(g["tile_offset"], json!([0, 0]));
        assert!(g["added"]["east"].as_u64().is_some_and(|n| n > 0), "{g}");
        let same = tile_cache_grow(w, &json!({ "min": [0, 0, 0], "max": [1, 0, 1] })).expect("no-op");
        assert_eq!(same["unchanged"], true);
        let bad = tile_cache_grow(w, &json!({ "min": [0, 0, 0] })).expect_err("max is required");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let huge =
            tile_cache_grow(w, &json!({ "min": [0, 0, 0], "max": [1e9, 0, 1e9] })).expect_err("too big");
        assert_eq!(reason(&huge), Some("nav_grid_growth"));
        let mut bare = App::new();
        let e = tile_links(bare.world_mut(), &json!({})).expect_err("no cache");
        assert_eq!(reason(&e), Some("nav_no_tile_cache"));
    }

    /// A crossing whose link left the mesh reports `"link": null`, not `NAV_LINK_GONE`
    /// (`usize::MAX`, above 2^53: a JSON client would read a rounded number).
    #[test]
    fn a_crossing_whose_link_left_reports_a_null_link() {
        let mut app = islands();
        let w = app.world_mut();
        set_links(
            w,
            &json!({ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "radius": 0.5, "user_id": 4, "duration": 5 } ] }),
        )
        .expect("links");
        let a = crowd_add_agent(w, &json!({ "position": [-2, 0, 0] })).expect("adds");
        crowd_set_target(w, &json!({ "id": a["id"], "target": [4, 0, 0] })).expect("target");
        let mut on_link = false;
        for _ in 0..120 {
            crowd_step(w, &json!({ "steps": 1 })).expect("steps");
            let t = link_traversals(w, &json!({})).expect("status");
            if t["agents"].as_array().is_some_and(|a| !a.is_empty()) {
                assert_eq!(t["agents"][0]["link"], 0, "a live link reports its index: {t}");
                on_link = true;
                break;
            }
        }
        assert!(on_link, "the agent took off");
        // The link leaves the mesh under the crossing; the crossing goes on to its landing.
        set_links(w, &json!({ "links": [] })).expect("links");
        crowd_step(w, &json!({ "steps": 1 })).expect("steps");
        let t = link_traversals(w, &json!({})).expect("status");
        let on = &t["agents"][0];
        assert_eq!(on["user_id"], 4, "still crossing: {t}");
        assert_eq!(on["link"], Value::Null, "a gone link is null: {t}");

        let gone = NavLinkTraversal {
            link: regia_nav::NAV_LINK_GONE,
            user_id: 4,
            kind: regia_nav::NavLinkKind::Jump,
            origin: Vec3::ZERO,
            start: Vec3::ZERO,
            end: Vec3::X,
            steps: 0,
            total_steps: 1,
            step_seconds: 1.0 / 60.0,
            arc: None,
            walk_seconds: 0.0,
        };
        assert_eq!(traversal_json(7, &gone)["link"], Value::Null);
        assert_eq!(takeoff_json(&gone)["link"], Value::Null);
        assert_eq!(takeoff_json(&NavLinkTraversal { link: 3, ..gone })["link"], 3);
    }

    /// Spec 92 § 4: `nav.generate_links` with `apply` replaces the generated id range and keeps
    /// the rest. On a streamed cache the rest must not swallow the links streamed in with the
    /// tiles: those stay the tiles' own, so each is listed once in the mesh.
    #[test]
    fn applying_generated_links_on_a_streamed_cache_keeps_streamed_links_once() {
        let whole_app = islands();
        let mut whole = whole_app.world().resource::<NavTileCache>().clone();
        whole.set_off_mesh_links(vec![
            regia_nav::OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
                .with_radius(0.5)
                .with_user_id(4),
        ]);
        let (tx, tz) = whole.tile_grid();
        let mut cache = NavTileCache::streamed(
            regia_nav::NavTileCacheParams {
                tile_size: 8,
                ..Default::default()
            },
            whole.layout(),
        )
        .expect("streamed");
        for z in 0..tz {
            for x in 0..tx {
                let blob = whole.cook_cell(&[(x, z)]).expect("cooks");
                cache.load_cell(u64::from(z * tx + x), &blob).expect("loads");
            }
        }
        cache.update();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(cache);
        let w = app.world_mut();
        let before = tile_links(w, &json!({})).expect("status");
        assert_eq!(
            (before["cache_links"].as_u64(), before["mesh_links"].as_u64()),
            (Some(0), Some(1))
        );
        for round in 0..2 {
            let g = crate::nav::generate_links(w, &json!({ "apply": true })).expect("generates");
            let generated = g["count"].as_u64().unwrap_or(u64::MAX);
            let t = tile_links(w, &json!({})).expect("status");
            assert_eq!(t["cache_links"].as_u64(), Some(generated), "round {round}: {t}");
            assert_eq!(
                t["mesh_links"].as_u64(),
                Some(generated + 1),
                "round {round}: {t}"
            );
            let streamed_once = w
                .resource::<NavTileCache>()
                .mesh()
                .off_mesh_links
                .iter()
                .filter(|l| l.user_id == 4)
                .count();
            assert_eq!(streamed_once, 1, "round {round}");
        }
    }
}
