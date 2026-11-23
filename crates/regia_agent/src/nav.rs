// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for navigation (specs/22 § T2.2, specs/23 § 3): `nav.get_mesh`,
//! `nav.find_nearest`, `nav.find_path`, the dynamic-obstacle `nav.add_obstacle` /
//! `nav.remove_obstacle` methods, and `nav.check_stair` (one stair, no mesh resource).
//!
//! `nav.get_mesh`, `nav.add_obstacle` and `nav.remove_obstacle` return `NOT_FOUND` when no
//! [`regia_nav::NavMesh`] resource is present. The hint names `NavMeshBuilder` and does not name
//! `regia_nav::NavPlugin`. `nav.find_path` and `nav.find_nearest` use a
//! [`regia_nav::NavTileCache`] when no [`regia_nav::NavMesh`] is present, and the `NavMesh` when
//! both resources exist. Tile-cache and crowd handlers return `nav_no_tile_cache`, `nav_no_mesh`
//! or `nav_no_crowd`.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Extracts a 3-component vector from `params[field]`.
pub(crate) fn vec3_param(params: &Value, field: &str) -> Result<regia_core::math::Vec3, RpcError> {
    let value = params
        .get(field)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` ([x, y, z]) is required")))?;
    let components = match value {
        Value::Array(items) if items.len() == 3 => {
            let mut out = [0.0f64; 3];
            for (i, item) in items.iter().enumerate() {
                out[i] = item
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three numbers")))?;
            }
            out
        }
        Value::Object(map) => {
            let g = |k: &str| map.get(k).and_then(Value::as_f64);
            let invalid = || RpcError::invalid_params(format!("`{field}` must be three numbers"));
            [
                g("x").ok_or_else(invalid)?,
                g("y").ok_or_else(invalid)?,
                g("z").ok_or_else(invalid)?,
            ]
        }
        _ => return Err(RpcError::invalid_params(format!("`{field}` must be [x, y, z]"))),
    };
    let v = regia_core::math::Vec3::new(components[0] as f32, components[1] as f32, components[2] as f32);
    // A JSON number past f32's range becomes infinity here (and NaN in the queries).
    if !v.is_finite() {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be three finite numbers within f32 range"
        )));
    }
    Ok(v)
}

/// Extracts an optional non-negative vector parameter.
pub(crate) fn vec3_optional(
    params: &Value,
    field: &str,
    default: regia_core::math::Vec3,
) -> Result<regia_core::math::Vec3, RpcError> {
    match params.get(field) {
        None => Ok(default),
        Some(_) => vec3_param(params, field),
    }
}

/// Builds a [`regia_nav::NavQuery`] over the app's mesh and obstacles, or the `NOT_FOUND` error.
fn query<'a>(world: &'a mut World) -> Result<regia_nav::NavQuery<'a>, RpcError> {
    if world.get_resource::<regia_nav::NavMesh>().is_none() {
        if world.get_resource::<regia_nav::NavTileCache>().is_some() {
            return Ok(world.resource::<regia_nav::NavTileCache>().query());
        }
        return Err(RpcError::not_found(
            "no NavMesh in this app",
            "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource",
        ));
    }
    Ok(regia_nav::NavQuery {
        mesh: world.resource::<regia_nav::NavMesh>(),
        obstacles: world.get_resource::<regia_nav::NavObstacles>(),
    })
}

/// `nav.get_mesh`: a summary of the navmesh (counts, bounds, agent parameters).
pub fn get_mesh(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(mesh) = world.get_resource::<regia_nav::NavMesh>() else {
        return Err(RpcError::not_found(
            "no NavMesh in this app",
            "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource",
        ));
    };
    let agent = &mesh.config.agent;
    Ok(json!({
        "polys": mesh.poly_count(),
        "vertices": mesh.vertex_count(),
        "regions": mesh.region_count,
        "layers": mesh.layer_count(),
        "off_mesh_links": mesh.off_mesh_links.len(),
        "bounds": {
            "min": [mesh.min.x, mesh.min.y, mesh.min.z],
            "max": [mesh.max.x, mesh.max.y, mesh.max.z],
        },
        "agent": {
            "radius": agent.radius,
            "height": agent.height,
            "max_climb": agent.max_climb,
            "max_slope": agent.max_slope,
        },
        "cell_size": mesh.config.cell_size,
    }))
}

/// `nav.find_nearest`: the polygon and closest point on the mesh to a query point.
pub fn find_nearest(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point = vec3_param(params, "point")?;
    let extents = vec3_optional(params, "extents", regia_core::math::Vec3::splat(2.0))?;
    let result = query(world)?.find_nearest(point, extents);
    let Some((poly, nearest)) = result else {
        return Ok(json!({ "poly": null, "point": null }));
    };
    Ok(json!({
        "poly": poly,
        "point": [nearest.x, nearest.y, nearest.z],
    }))
}

/// `nav.find_path`: a path (waypoints + polygons + length) between two points, or `null` when no
/// route exists. An optional `filter` sets flags, area costs and left-out areas. With
/// `"partial": true` an unreachable goal gives the path to the nearest reachable point and a
/// `status` (NM-1).
pub fn find_path(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if crate::params::opt_bool(params, "partial")?.unwrap_or(false) {
        return crate::nav_query::find_path_partial(world, params);
    }
    let from = vec3_param(params, "from")?;
    let to = vec3_param(params, "to")?;
    let extents = vec3_optional(params, "extents", regia_core::math::Vec3::splat(2.0))?;
    let filter = crate::nav_lanes::filter_param(params)?.unwrap_or_default();
    let result = query(world)?.find_path_filtered(from, to, extents, &filter);
    let Some(path) = result else {
        return Ok(json!({ "path": null }));
    };
    Ok(json!({
        "path": {
            "waypoints": path.waypoints.iter().map(|w| [w.x, w.y, w.z]).collect::<Vec<_>>(),
            "polys": path.polys,
            "length": path.length,
        }
    }))
}

/// `nav.plan`: a deterministic plan on the layered bake of the app's `NavMesh` (ADR-8486): the
/// integer-millimetre runtime of `regia_nav::det::layers`, with the passage cost shares and the
/// lane rule, and the layer of every waypoint. `{ plan: null, reason }` when there is no route;
/// `reason` is a lowercase code (`plan_start_off_mesh`, `plan_goal_off_mesh`, `plan_unreachable`,
/// `plan_budget`).
pub fn plan(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_nav::det::layers::{DEFAULT_TILE_MM, LayerAgent, LayerScratch, LayerState, mm};
    let from = vec3_param(params, "from")?;
    let to = vec3_param(params, "to")?;
    let tunnel_pct = u64_field(params, "tunnel_pct", 75, 100)? as u32;
    let flank_pct = u64_field(params, "flank_pct", 20, 100)? as u32;
    let gap = params
        .get("passing_gap")
        .map(|_| f32_param(params, "passing_gap", 0.0))
        .transpose()?;
    let Some(mesh) = world.get_resource::<regia_nav::NavMesh>() else {
        return Err(RpcError::not_found(
            "no NavMesh in this app",
            "build a regia_nav::NavMesh (NavMeshBuilder::build_layered) and insert it as a resource",
        ));
    };
    let bake = mesh.det_layered_bake(DEFAULT_TILE_MM);
    let mut agent = LayerAgent::for_bake(&bake);
    agent.tunnel_pct = tunnel_pct;
    agent.flank_pct = flank_pct;
    if let Some(gap) = gap {
        agent.passing_gap_mm = mm(gap).max(0);
    }
    let state = LayerState::new(&bake);
    let point = |v: regia_core::math::Vec3| [mm(v.x), mm(v.y), mm(v.z)];
    let m = |v: i32| f64::from(v) / 1000.0;
    let base = json!({
        "bake_hash": format!("{:016x}", bake.content_hash()),
        "bits_version": regia_nav::det::layers::LAYERED_NAV_BITS_VERSION,
        "layers": bake.layer_count(),
    });
    let mut out = match bake.plan(
        &state,
        point(from),
        point(to),
        &agent,
        &mut LayerScratch::default(),
    ) {
        Ok(p) => json!({
            "plan": {
                "waypoints": p.waypoints.iter().map(|w| [m(w[0]), m(w[1]), m(w[2])]).collect::<Vec<_>>(),
                "waypoint_layers": p.layers,
                "layer": p.goal_layer(),
                "polys": p.polys,
                "cost_mm": p.cost,
                "length": p.length_mm() as f64 / 1000.0,
                "expansions": p.expansions,
            }
        }),
        Err(e) => json!({ "plan": null, "reason": e.code() }),
    };
    if let (Some(o), Some(b)) = (out.as_object_mut(), base.as_object()) {
        for (k, v) in b {
            o.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

/// `nav.add_obstacle`: carves an axis-aligned box out of the mesh; returns its id.
pub fn add_obstacle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let min = vec3_param(params, "min")?;
    let max = vec3_param(params, "max")?;
    if world.get_resource::<regia_nav::NavMesh>().is_none() {
        return Err(RpcError::not_found(
            "no NavMesh in this app",
            "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource",
        ));
    }
    let mut obstacles = world.get_resource_or_insert_with::<regia_nav::NavObstacles>(Default::default);
    let id = obstacles.add_box(min, max);
    Ok(json!({ "id": id, "obstacles": obstacles.obstacle_count() }))
}

/// `nav.remove_obstacle`: removes a previously added obstacle by id.
pub fn remove_obstacle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("`id` (a number) is required"))?;
    if world.get_resource::<regia_nav::NavMesh>().is_none() {
        return Err(RpcError::not_found(
            "no NavMesh in this app",
            "build a regia_nav::NavMesh (NavMeshBuilder) and insert it as a resource",
        ));
    }
    let removed = world
        .get_resource_mut::<regia_nav::NavObstacles>()
        .map(|mut o| o.remove(id))
        .unwrap_or(false);
    Ok(json!({ "removed": removed }))
}

// -- Tile cache and crowd (ADR-1484, book/agents/navigation-crowds.md) -------------------------

/// A navigation error with its stable lowercase `data.reason` (documented on the book page).
pub(crate) fn nav_error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({
            "code": regia_code,
            "reason": reason,
            "hint": "see book/agents/navigation-crowds.md",
        })),
    }
}

pub(crate) fn tile_cache_error(e: &regia_nav::NavTileCacheError) -> RpcError {
    match e {
        regia_nav::NavTileCacheError::UnknownObstacle(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string()),
    }
}

pub(crate) fn crowd_error(e: &regia_nav::NavCrowdError) -> RpcError {
    match e {
        regia_nav::NavCrowdError::UnknownAgent(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string()),
    }
}

pub(crate) fn no_tile_cache() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_tile_cache",
        "no NavTileCache in this app: build one with regia_nav::NavTileCache::build and insert it as a resource",
    )
}

fn no_crowd_mesh() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_mesh",
        "no NavTileCache or NavMesh in this app for the crowd to walk on",
    )
}

pub(crate) fn no_crowd() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_crowd",
        "no NavCrowd in this app: add an agent with nav.crowd_add_agent or insert a regia_nav::NavCrowd",
    )
}

pub(crate) fn xz_pair(t: (u32, u32)) -> Value {
    json!([t.0, t.1])
}

pub(crate) fn shape_json(shape: &regia_nav::NavObstacleShape) -> Value {
    match *shape {
        regia_nav::NavObstacleShape::Cylinder { base, radius, height } => json!({
            "shape": "cylinder",
            "base": [base.x, base.y, base.z],
            "radius": radius,
            "height": height,
        }),
        regia_nav::NavObstacleShape::Box { min, max } => json!({
            "shape": "box",
            "min": [min.x, min.y, min.z],
            "max": [max.x, max.y, max.z],
        }),
        regia_nav::NavObstacleShape::OrientedBox {
            center,
            half_extents,
            yaw,
        } => json!({
            "shape": "oriented_box",
            "center": [center.x, center.y, center.z],
            "half_extents": [half_extents.x, half_extents.y, half_extents.z],
            "yaw": yaw,
        }),
    }
}

/// The obstacle shape of `nav.tile_cache_add_obstacle` / `nav.tile_cache_move_obstacle`.
pub(crate) fn shape_param(params: &Value) -> Result<regia_nav::NavObstacleShape, RpcError> {
    Ok(match crate::params::opt_str(params, "shape")? {
        Some("cylinder") => regia_nav::NavObstacleShape::Cylinder {
            base: vec3_param(params, "base")?,
            radius: f32_param(params, "radius", f32::NAN)?,
            height: f32_param(params, "height", f32::NAN)?,
        },
        Some("box") => regia_nav::NavObstacleShape::Box {
            min: vec3_param(params, "min")?,
            max: vec3_param(params, "max")?,
        },
        Some("oriented_box") => regia_nav::NavObstacleShape::OrientedBox {
            center: vec3_param(params, "center")?,
            half_extents: vec3_param(params, "half_extents")?,
            yaw: f32_param(params, "yaw", 0.0)?,
        },
        _ => {
            return Err(RpcError::invalid_params(
                "`shape` must be \"cylinder\", \"box\" or \"oriented_box\"",
            ));
        }
    })
}

/// An `f32` parameter, `default` when absent; a non-number is invalid params.
pub(crate) fn f32_param(params: &Value, field: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number"))),
    }
}

/// `update` (default true): whether to rebuild the touched tiles now.
pub(crate) fn update_param(params: &Value) -> Result<bool, RpcError> {
    match params.get("update") {
        None => Ok(true),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`update` must be true or false")),
    }
}

pub(crate) fn update_json(cache: &mut regia_nav::NavTileCache, update: bool) -> Value {
    if !update {
        return json!({ "rebuilt": [], "pending": cache.pending_tiles().into_iter().map(xz_pair).collect::<Vec<_>>() });
    }
    let done = cache.update();
    json!({
        "rebuilt": done.rebuilt.into_iter().map(xz_pair).collect::<Vec<_>>(),
        "pending": cache.pending_tiles().into_iter().map(xz_pair).collect::<Vec<_>>(),
    })
}

/// `nav.tile_cache_status`: the tile grid, obstacles, queued tiles and rebuild counters.
pub fn tile_cache_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(cache) = world.get_resource::<regia_nav::NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let (tx, tz) = cache.tile_grid();
    let p = cache.params();
    Ok(json!({
        "tiles": [tx, tz],
        "tile_count": cache.tile_count(),
        "tile_size": p.tile_size,
        "tile_world_size": p.tile_size as f32 * p.build.cell_size,
        "polys": cache.mesh().poly_count(),
        "regions": cache.mesh().region_count,
        "obstacles": cache
            .obstacles()
            .map(|(id, shape)| {
                let mut o = shape_json(&shape);
                o["id"] = json!(id.0);
                o
            })
            .collect::<Vec<_>>(),
        "pending": cache.pending_tiles().into_iter().map(xz_pair).collect::<Vec<_>>(),
        "total_rebuilds": cache.total_rebuilds(),
        "revision": cache.revision(),
    }))
}

/// `nav.tile_cache_add_obstacle`: adds a cylinder, box or oriented-box obstacle; rebuilds the
/// tiles it touches unless `update` is false. `yaw` defaults to 0. A shape other than those three
/// is F0002 with no `data.reason`.
pub fn tile_cache_add_obstacle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let shape = shape_param(params)?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<regia_nav::NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let id = cache.add_obstacle(shape).map_err(|e| tile_cache_error(&e))?;
    let touched = cache.touched_tiles(id).unwrap_or_default();
    let mut out = update_json(&mut cache, update);
    out["id"] = json!(id.0);
    out["touched"] = json!(touched.into_iter().map(xz_pair).collect::<Vec<_>>());
    out["revision"] = json!(cache.revision());
    Ok(out)
}

/// `nav.tile_cache_remove_obstacle`: removes an obstacle; rebuilds the tiles it touched unless
/// `update` is false.
pub fn tile_cache_remove_obstacle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("`id` (a number) is required"))?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<regia_nav::NavTileCache>() else {
        return Err(no_tile_cache());
    };
    cache
        .remove_obstacle(regia_nav::NavObstacleId(id))
        .map_err(|e| tile_cache_error(&e))?;
    let mut out = update_json(&mut cache, update);
    out["removed"] = json!(true);
    out["revision"] = json!(cache.revision());
    Ok(out)
}

/// `nav.tile_cache_move_obstacle`: replaces an obstacle's shape (same id); rebuilds the tiles of
/// the old and the new footprint unless `update` is false.
pub fn tile_cache_move_obstacle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("`id` (a number) is required"))?;
    let shape = shape_param(params)?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<regia_nav::NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let touched = cache
        .move_obstacle(regia_nav::NavObstacleId(id), shape)
        .map_err(|e| tile_cache_error(&e))?;
    let mut out = update_json(&mut cache, update);
    out["id"] = json!(id);
    out["touched"] = json!(touched.into_iter().map(xz_pair).collect::<Vec<_>>());
    out["revision"] = json!(cache.revision());
    Ok(out)
}

/// Runs `f` with the crowd and the mesh it walks on (the tile cache's, else the `NavMesh`).
/// The tile cache's queued tiles are rebuilt first and the crowd follows its revision.
fn with_crowd<T>(
    world: &mut World,
    create: bool,
    f: impl FnOnce(&mut regia_nav::NavCrowd, &regia_nav::NavMesh) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    with_crowd_cache(world, create, |crowd, mesh, _| f(crowd, mesh))
}

/// [`with_crowd`] that also hands over the tile cache, when the crowd walks one.
pub(crate) fn with_crowd_cache<T>(
    world: &mut World,
    create: bool,
    f: impl FnOnce(
        &mut regia_nav::NavCrowd,
        &regia_nav::NavMesh,
        Option<&regia_nav::NavTileCache>,
    ) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    let has_cache = world.get_resource::<regia_nav::NavTileCache>().is_some();
    if !has_cache && world.get_resource::<regia_nav::NavMesh>().is_none() {
        return Err(no_crowd_mesh());
    }
    if world.get_resource::<regia_nav::NavCrowd>().is_none() {
        if !create {
            return Err(no_crowd());
        }
        let crowd =
            regia_nav::NavCrowd::new(regia_nav::NavCrowdConfig::default()).map_err(|e| crowd_error(&e))?;
        world.insert_resource(crowd);
    }
    world.resource_scope(|world, mut crowd: Mut<regia_nav::NavCrowd>| {
        if has_cache {
            let mut cache = world.resource_mut::<regia_nav::NavTileCache>();
            cache.update();
            crowd.sync_mesh_revision(cache.revision());
            crowd.sync_streaming(&cache);
            f(&mut crowd, cache.mesh(), Some(&*cache))
        } else {
            f(&mut crowd, world.resource::<regia_nav::NavMesh>(), None)
        }
    })
}

fn agent_json(a: &regia_nav::CrowdAgent) -> Value {
    let (p, v) = (a.position(), a.velocity());
    json!({
        "id": a.id().0,
        "state": a.state().name(),
        "position": [p.x, p.y, p.z],
        "velocity": [v.x, v.y, v.z],
        "target": a.target().map(|t| json!([t.x, t.y, t.z])),
        "corridor_len": a.corridor().len(),
        "radius": a.params().radius,
        "streamed": a.streamed_target(),
        "awaiting_tile": a.awaiting_tile().map(|(x, z)| json!([x, z])),
        "link": a.link_traversal().map(|t| json!({
            "user_id": t.user_id,
            "kind": t.kind.name(),
            "progress": t.progress(),
            "origin": [t.origin.x, t.origin.y, t.origin.z],
            "start": [t.start.x, t.start.y, t.start.z],
            "end": [t.end.x, t.end.y, t.end.z],
        })),
        "link_wait": a.link_wait().map(|w| json!({
            "user_id": w.user_id,
            "ticket": w.ticket,
            "steps": w.steps,
        })),
    })
}

fn link_event_json(e: &regia_nav::NavLinkEvent) -> Value {
    json!({
        "agent": e.agent.0,
        "user_id": e.user_id,
        "kind": e.kind.name(),
        "phase": e.phase.name(),
        "position": [e.position.x, e.position.y, e.position.z],
    })
}

/// `nav.crowd_status`: every crowd agent plus the step counters and the state digest.
pub fn crowd_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(crowd) = world.get_resource::<regia_nav::NavCrowd>() else {
        return Err(no_crowd());
    };
    let s = crowd.stats();
    Ok(json!({
        "agents": crowd.agents().iter().map(agent_json).collect::<Vec<_>>(),
        "stats": {
            "steps": s.steps,
            "replans": s.replans,
            "path_failures": s.path_failures,
            "velocity_samples": s.velocity_samples,
            "shortcuts": s.shortcuts,
            "link_traversals": s.link_traversals,
            "dormant_entries": s.dormant_entries,
            "tile_waits": s.tile_waits,
            "link_queue_waits": s.link_queue_waits,
            "link_queue_timeouts": s.link_queue_timeouts,
        },
        "stuck": crate::nav_query::crowd_stuck_json(crowd),
        "dormant": crowd.dormant_count(),
        "step_seconds": crowd.config().step_seconds,
        "digest": format!("{:016x}", crowd.digest()),
    }))
}

/// `nav.crowd_add_agent`: adds an agent (creating the crowd on first use).
pub fn crowd_add_agent(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let position = vec3_param(params, "position")?;
    let d = regia_nav::CrowdAgentParams::default();
    let mut agent = regia_nav::CrowdAgentParams::new(
        f32_param(params, "radius", d.radius)?,
        f32_param(params, "height", d.height)?,
        f32_param(params, "max_speed", d.max_speed)?,
        f32_param(params, "max_acceleration", d.max_acceleration)?,
    );
    if let Some(filter) = crate::nav_lanes::filter_param(params)? {
        agent.link_filter = filter;
    }
    with_crowd(world, true, |crowd, mesh| {
        let id = crowd
            .add_agent(mesh, position, agent)
            .map_err(|e| crowd_error(&e))?;
        let a = crowd.agent(id).map(agent_json).unwrap_or(Value::Null);
        Ok(json!({ "id": id.0, "agent": a, "agents": crowd.len() }))
    })
}

/// `nav.crowd_set_target`: sends an agent toward a target (an unreachable target is state
/// `no_path`, not an error). `streamed: true` plans through the tile cache (ADR-2133): a target on an
/// unloaded tile is walked to the loaded border, where the agent waits (`awaiting_tile`).
pub fn crowd_set_target(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("`id` (a number) is required"))?;
    let target = vec3_param(params, "target")?;
    let streamed = match params.get("streamed") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`streamed` must be a boolean"))?,
    };
    if streamed {
        return crowd_set_target_streamed(world, regia_nav::CrowdAgentId(id), target);
    }
    with_crowd(world, false, |crowd, mesh| {
        let id = regia_nav::CrowdAgentId(id);
        crowd.set_target(mesh, id, target).map_err(|e| crowd_error(&e))?;
        let a = crowd.agent(id).map(agent_json).unwrap_or(Value::Null);
        Ok(json!({ "agent": a }))
    })
}

/// The `streamed: true` form of `nav.crowd_set_target`: needs a `NavTileCache`.
fn crowd_set_target_streamed(
    world: &mut World,
    id: regia_nav::CrowdAgentId,
    target: regia_core::math::Vec3,
) -> Result<Value, RpcError> {
    if world.get_resource::<regia_nav::NavTileCache>().is_none() {
        let mut err = nav_error(
            crate::protocol::NOT_FOUND,
            "F0003",
            "nav_no_tile_cache",
            "`streamed: true` needs a NavTileCache resource",
        );
        if let Some(data) = err.data.as_mut() {
            data["hint"] = json!("see book/agents/navigation-world.md");
        }
        return Err(err);
    }
    with_crowd_cache(world, false, |crowd, _, cache| {
        let cache = cache.ok_or_else(no_crowd_mesh)?;
        let outcome = crowd
            .set_target_streamed(cache, id, target)
            .map_err(|e| crowd_error(&e))?;
        let a = crowd.agent(id).map(agent_json).unwrap_or(Value::Null);
        Ok(json!({ "agent": a, "outcome": outcome.map(|o| o.name()) }))
    })
}

/// `nav.crowd_set_filter`: replaces an agent's filter; it replans on the next step.
pub fn crowd_set_filter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("`id` (a number) is required"))?;
    let filter = crate::nav_lanes::filter_param(params)?
        .ok_or_else(|| RpcError::invalid_params("`filter` (an object) is required"))?;
    with_crowd(world, false, |crowd, _| {
        let id = regia_nav::CrowdAgentId(id);
        let unknown = regia_nav::NavCrowdError::UnknownAgent(id);
        let mut agent = *crowd.agent(id).ok_or_else(|| crowd_error(&unknown))?.params();
        agent.link_filter = filter;
        crowd.set_agent_params(id, agent).map_err(|e| crowd_error(&e))?;
        Ok(json!({ "agent": crowd.agent(id).map(agent_json) }))
    })
}

/// The most crowd steps one `nav.crowd_step` call runs.
const MAX_CROWD_STEPS_PER_CALL: u64 = 3600;

/// `nav.crowd_step`: advances the crowd by `steps` fixed steps (default 1).
pub fn crowd_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let steps = match params.get("steps") {
        None => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_CROWD_STEPS_PER_CALL).contains(n))
            .ok_or_else(|| RpcError::invalid_params("`steps` must be an integer in 1..=3600"))?,
    };
    with_crowd_cache(world, false, |crowd, mesh, cache| {
        let mut link_events = Vec::new();
        let mut stuck_events = Vec::new();
        for _ in 0..steps {
            // Sync before the step, as NavCrowdPlugin does: cancellations open this step's
            // events, and the digest is taken before the next tick's streaming sync.
            if let Some(cache) = cache {
                crowd.sync_streaming(cache);
            }
            crowd.step(mesh);
            link_events.extend(crowd.link_events().iter().map(link_event_json));
            stuck_events.extend(
                crowd
                    .stuck_events()
                    .iter()
                    .map(crate::nav_query::stuck_event_json),
            );
        }
        let arrived = crowd
            .agents()
            .iter()
            .filter(|a| a.state() == regia_nav::CrowdAgentState::Arrived)
            .count();
        Ok(json!({
            "steps": steps,
            "total_steps": crowd.stats().steps,
            "arrived": arrived,
            "agents": crowd.len(),
            "digest": format!("{:016x}", crowd.digest()),
            "link_events": link_events,
            "stuck_events": stuck_events,
            "stuck": crate::nav_query::crowd_stuck_json(crowd),
        }))
    })
}

// -- Off-mesh links (ADR-1821, book/agents/navigation-links.md) -------------------------------

pub(crate) fn link_error(e: &regia_nav::NavLinkError) -> RpcError {
    let mut err = match e {
        regia_nav::NavLinkError::UnknownLink(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string()),
    };
    if let Some(data) = err.data.as_mut() {
        data["hint"] = json!("see book/agents/navigation-links.md");
    }
    err
}

fn no_link_mesh() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_mesh",
        "no NavTileCache or NavMesh in this app to carry off-mesh links",
    )
}

/// The links' mesh: the tile cache's when there is one, else the `NavMesh` resource.
pub(crate) fn link_mesh(world: &World) -> Result<&regia_nav::NavMesh, RpcError> {
    if let Some(cache) = world.get_resource::<regia_nav::NavTileCache>() {
        return Ok(cache.mesh());
    }
    world
        .get_resource::<regia_nav::NavMesh>()
        .ok_or_else(no_link_mesh)
}

/// Crowd agents plan again after a link change: an agent with no path may have one now.
pub(crate) fn replan_crowd(world: &mut World) {
    if let Some(mut crowd) = world.get_resource_mut::<regia_nav::NavCrowd>() {
        crowd.request_replan_all();
    }
}

/// Replaces the links on the tile cache (kept through rebuilds) or on the `NavMesh`.
pub(crate) fn store_links(world: &mut World, links: Vec<regia_nav::OffMeshLink>) -> Result<(), RpcError> {
    if let Some(mut cache) = world.get_resource_mut::<regia_nav::NavTileCache>() {
        cache.set_off_mesh_links(links);
    } else if let Some(mut mesh) = world.get_resource_mut::<regia_nav::NavMesh>() {
        mesh.set_off_mesh_links(links);
    } else {
        return Err(no_link_mesh());
    }
    replan_crowd(world);
    Ok(())
}

fn link_json(
    link: &regia_nav::OffMeshLink,
    status: Option<&regia_nav::NavLinkStatus>,
    index: usize,
) -> Value {
    let (from_poly, to_poly) = match status.map(|s| s.state) {
        Some(regia_nav::NavLinkState::Active { from_poly, to_poly }) => (json!(from_poly), json!(to_poly)),
        _ => (Value::Null, Value::Null),
    };
    let mut out = json!({
        "index": index,
        "user_id": link.user_id,
        "kind": link.kind.name(),
        "from": [link.from.x, link.from.y, link.from.z],
        "to": [link.to.x, link.to.y, link.to.z],
        "bidirectional": link.bidirectional,
        "enabled": link.enabled,
        "flags": link.flags,
        "area": link.area,
        "radius": link.radius,
        "cost_scale": link.cost_scale,
        "duration": link.duration,
        "state": status.map(|s| s.state.name()),
        "from_poly": from_poly,
        "to_poly": to_poly,
    });
    if let regia_nav::NavLinkKind::Custom(c) = link.kind {
        out["custom"] = json!(c);
    }
    out
}

fn links_value(mesh: &regia_nav::NavMesh, statuses: &[regia_nav::NavLinkStatus]) -> Value {
    let mut counts = [0usize; 4];
    for s in statuses {
        let slot = match s.state {
            regia_nav::NavLinkState::Active { .. } => 0,
            regia_nav::NavLinkState::Blocked => 1,
            regia_nav::NavLinkState::Disabled => 2,
            regia_nav::NavLinkState::Unresolved { .. } => 3,
        };
        counts[slot] += 1;
    }
    json!({
        "links": mesh
            .off_mesh_links
            .iter()
            .enumerate()
            .map(|(i, l)| link_json(l, statuses.get(i), i))
            .collect::<Vec<_>>(),
        "active": counts[0],
        "blocked": counts[1],
        "disabled": counts[2],
        "unresolved": counts[3],
    })
}

/// `nav.links`: every off-mesh link with its state.
///
/// The link list is the tile cache's when one exists (its blockers already include its
/// obstacles). A plain [`regia_nav::NavMesh`] reports statuses with the same
/// [`regia_nav::NavObstacles`] boxes a path query carves, without rewriting the stored polygons.
pub fn links(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<regia_nav::NavTileCache>().is_some() {
        let mesh = link_mesh(world)?;
        let statuses = mesh.link_statuses();
        return Ok(links_value(mesh, &statuses));
    }
    if world.get_resource::<regia_nav::NavMesh>().is_none() {
        return Err(no_link_mesh());
    }
    let query = regia_nav::NavQuery {
        mesh: world.resource::<regia_nav::NavMesh>(),
        obstacles: world.get_resource::<regia_nav::NavObstacles>(),
    };
    let statuses = query.link_statuses();
    Ok(links_value(query.mesh, &statuses))
}

pub(crate) fn bool_field(obj: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    match obj.get(field) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

pub(crate) fn u64_field(obj: &Value, field: &str, default: u64, max: u64) -> Result<u64, RpcError> {
    match obj.get(field) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|&n| n <= max)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an integer in 0..={max}"))),
    }
}

/// One link object of `nav.set_links`.
fn link_param(obj: &Value) -> Result<regia_nav::OffMeshLink, RpcError> {
    if !obj.is_object() {
        return Err(RpcError::invalid_params(
            "every link must be an object with `from` and `to`",
        ));
    }
    let mut link = regia_nav::OffMeshLink::new(vec3_param(obj, "from")?, vec3_param(obj, "to")?);
    link.bidirectional = bool_field(obj, "bidirectional", true)?;
    link.enabled = bool_field(obj, "enabled", true)?;
    link.user_id = u64_field(obj, "user_id", 0, u64::MAX)?;
    link.flags = u64_field(
        obj,
        "flags",
        u64::from(regia_nav::NavLinkFlags::WALK),
        u64::from(u16::MAX),
    )? as u16;
    link.area = u64_field(obj, "area", 0, u64::from(u8::MAX))? as u8;
    link.radius = f32_param(obj, "radius", regia_nav::DEFAULT_LINK_RADIUS)?;
    link.cost_scale = f32_param(obj, "cost_scale", 1.0)?;
    link.duration = f32_param(obj, "duration", 0.0)?;
    let custom = u64_field(obj, "custom", 0, u64::from(u16::MAX))? as u16;
    link.kind = match obj.get("kind") {
        None => regia_nav::NavLinkKind::Jump,
        Some(v) => v
            .as_str()
            .and_then(|k| regia_nav::NavLinkKind::from_name(k, custom))
            .ok_or_else(|| {
                RpcError::invalid_params(
                    "`kind` must be \"jump\", \"drop\", \"climb\", \"teleport\" or \"custom\"",
                )
            })?,
    };
    Ok(link)
}

/// `nav.set_links`: replaces every off-mesh link, from a `links` array or a `.navlinks.ron`
/// text in `ron`.
pub fn set_links(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let links = match (params.get("links"), params.get("ron")) {
        (Some(Value::Array(items)), None) => {
            if items.len() > regia_nav::MAX_NAV_LINKS {
                return Err(link_error(&regia_nav::NavLinkError::TooManyLinks(items.len())));
            }
            let links = items.iter().map(link_param).collect::<Result<Vec<_>, _>>()?;
            regia_nav::validate_links(&links).map_err(|e| link_error(&e))?;
            links
        }
        (None, Some(Value::String(text))) => {
            regia_nav::NavLinkSet::from_ron_str(text).map_err(|e| link_error(&e))?
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give either `links` (an array of link objects) or `ron` (a .navlinks.ron text)",
            ));
        }
    };
    let count = links.len();
    store_links(world, links)?;
    let mesh = link_mesh(world)?;
    let active = mesh
        .link_statuses()
        .iter()
        .filter(|s| matches!(s.state, regia_nav::NavLinkState::Active { .. }))
        .count();
    Ok(json!({ "count": count, "active": active }))
}

/// `nav.set_link_enabled`: switches every link with `user_id` on or off.
pub fn set_link_enabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let user_id = crate::params::opt_u64(params, "user_id")?
        .ok_or_else(|| RpcError::invalid_params("`user_id` (a number) is required"))?;
    let enabled = crate::params::opt_bool(params, "enabled")?
        .ok_or_else(|| RpcError::invalid_params("`enabled` (true or false) is required"))?;
    let known = link_mesh(world)?
        .off_mesh_links
        .iter()
        .any(|l| l.user_id == user_id);
    if !known {
        return Err(link_error(&regia_nav::NavLinkError::UnknownLink(user_id)));
    }
    let changed = if let Some(mut cache) = world.get_resource_mut::<regia_nav::NavTileCache>() {
        cache.set_link_enabled(user_id, enabled)
    } else if let Some(mut mesh) = world.get_resource_mut::<regia_nav::NavMesh>() {
        mesh.set_link_enabled(user_id, enabled)
    } else {
        return Err(no_link_mesh());
    };
    if changed > 0 {
        replan_crowd(world);
    }
    Ok(json!({ "changed": changed }))
}

/// The user-id range `nav.generate_links` replaces when it applies its links: `2^32` ids from
/// `first_user_id`.
const GENERATED_ID_SPAN: u64 = 1 << 32;

/// `nav.generate_links`: finds drop and jump links on the mesh; with `apply` they replace the
/// links of an earlier generated set (same id range) and keep the rest.
pub fn generate_links(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let d = regia_nav::NavLinkGenParams::default();
    let gen_params = regia_nav::NavLinkGenParams {
        max_drop: f32_param(params, "max_drop", d.max_drop)?,
        max_jump_gap: f32_param(params, "max_jump_gap", d.max_jump_gap)?,
        max_jump_up: f32_param(params, "max_jump_up", d.max_jump_up)?,
        spacing: f32_param(params, "spacing", d.spacing)?,
        radius: f32_param(params, "radius", d.radius)?,
        first_user_id: u64_field(
            params,
            "first_user_id",
            d.first_user_id,
            u64::MAX - GENERATED_ID_SPAN,
        )?,
        flags: u64_field(params, "flags", u64::from(d.flags), u64::from(u16::MAX))? as u16,
    };
    let apply = bool_field(params, "apply", false)?;
    // `clearance: "level"` tests each candidate against the level's colliders (ADR-2420).
    let level = match params.get("clearance").map(|v| v.as_str()) {
        None | Some(Some("none")) => false,
        Some(Some("level")) => true,
        _ => {
            return Err(RpcError::invalid_params(
                "`clearance` must be \"none\" (default) or \"level\"",
            ));
        }
    };
    let mesh = link_mesh(world)?;
    let (generated, rejected) = if level {
        regia_framework::nav_world::generate_level_links(world, mesh, &gen_params)
            .map_err(|e| link_error(&e))?
    } else {
        let links = regia_nav::generate_links(mesh, &gen_params, |_| true).map_err(|e| link_error(&e))?;
        (links, 0)
    };
    let listed: Vec<Value> = generated
        .iter()
        .enumerate()
        .map(|(i, l)| link_json(l, None, i))
        .collect();
    let count = generated.len();
    if apply {
        let range = gen_params.first_user_id..gen_params.first_user_id + GENERATED_ID_SPAN;
        // The tile cache's own list, not its mesh's: the mesh also carries the links streamed in
        // with loaded tiles, which the cache adds back itself (spec 92 § 3.6), so storing them
        // as the cache's own would list every streamed link twice.
        let kept = match world.get_resource::<regia_nav::NavTileCache>() {
            Some(cache) => cache.off_mesh_links(),
            None => &mesh.off_mesh_links[..],
        };
        let mut links: Vec<regia_nav::OffMeshLink> = kept
            .iter()
            .filter(|l| !range.contains(&l.user_id))
            .copied()
            .collect();
        links.extend(generated);
        if links.len() > regia_nav::MAX_NAV_LINKS {
            return Err(link_error(&regia_nav::NavLinkError::TooManyLinks(links.len())));
        }
        store_links(world, links)?;
    }
    Ok(json!({
        "links": listed,
        "count": count,
        "applied": apply,
        "clearance": if level { "level" } else { "none" },
        "rejected": rejected,
    }))
}

/// `nav.check_stair`: whether a capsule can walk one stair. No navmesh resource is read.
///
/// An empty object fails with `nav_stair_input` before any mesh is built.
pub fn check_stair(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let x = req_f32(params, "x")?;
    let z = req_f32(params, "z")?;
    let width_m = req_f32(params, "width_m")?;
    let y0_m = req_f32(params, "y0_m")?;
    let rise_m = req_f32(params, "rise_m")?;
    let run_m = req_f32(params, "run_m")?;
    let step_rise_m = opt_f32(params, "step_rise_m")?;
    let direction = stair_direction(params)?;
    let mut settings = regia_nav::CapsuleWalkSettings::with_agent(stair_agent(params)?);
    if let Some(cell) = opt_f32(params, "cell_size_m")? {
        settings.cell_size_m = cell;
    }
    if let Some(cell) = opt_f32(params, "cell_height_m")? {
        settings.cell_height_m = cell;
    }
    let stair = regia_nav::StairProbe {
        x,
        z,
        direction,
        width_m,
        y0_m,
        rise_m,
        run_m,
        step_rise_m,
    };
    match regia_nav::check_stair(&settings, &stair) {
        Ok(verdict) => Ok(json!({
            "flagged": verdict.flagged,
            "flag": verdict.flag.as_str(),
            "connected": verdict.connected,
        })),
        Err(err) => Err(stair_input(err.to_string())),
    }
}

fn stair_input(message: impl Into<String>) -> RpcError {
    RpcError {
        code: crate::protocol::INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": "nav_stair_input",
            "hint": "see book/agents/decal-atlas-walkability.md",
        })),
    }
}

fn stair_direction(params: &Value) -> Result<regia_nav::StairDirection, RpcError> {
    let text = crate::params::opt_str(params, "direction").map_err(|err| stair_input(err.message))?;
    let Some(text) = text else {
        return Err(stair_input("`direction` is required"));
    };
    regia_nav::StairDirection::parse(text).ok_or_else(|| stair_input("`direction` must be +x, -x, +z or -z"))
}

fn stair_agent(params: &Value) -> Result<regia_nav::NavAgentParams, RpcError> {
    let mut agent = regia_nav::NavAgentParams::HUMANOID;
    let Some(map) = crate::params::opt_object(params, "agent").map_err(|err| stair_input(err.message))?
    else {
        return Ok(agent);
    };
    if let Some(radius) = opt_f32(map, "radius")? {
        agent.radius = radius;
    }
    if let Some(height) = opt_f32(map, "height")? {
        agent.height = height;
    }
    if let Some(max_climb) = opt_f32(map, "max_climb")? {
        agent.max_climb = max_climb;
    }
    if let Some(max_slope) = opt_f32(map, "max_slope")? {
        agent.max_slope = max_slope;
    }
    Ok(agent)
}

fn req_f32<'a>(src: impl crate::params::ParamSource<'a>, key: &str) -> Result<f32, RpcError> {
    opt_f32(src, key)?.ok_or_else(|| stair_input(format!("`{key}` is required")))
}

fn opt_f32<'a>(src: impl crate::params::ParamSource<'a>, key: &str) -> Result<Option<f32>, RpcError> {
    let Some(number) = crate::params::opt_f64(src, key).map_err(|err| stair_input(err.message))? else {
        return Ok(None);
    };
    let value = number as f32;
    if number.is_finite() && value.is_finite() {
        Ok(Some(value))
    } else {
        Err(stair_input(format!("`{key}` must be finite")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_nav::{NavMeshBuilder, NavObstacles};

    #[test]
    fn check_stair_refuses_an_empty_call_and_flags_a_steep_ramp() {
        let mut app = App::new();
        let err = check_stair(app.world_mut(), &json!({})).expect_err("x is required");
        assert_eq!(err.data.as_ref().expect("data")["reason"], "nav_stair_input");
        assert_eq!(err.data.as_ref().expect("data")["code"], "F0002");

        let verdict = check_stair(
            app.world_mut(),
            &json!({
                "x": 0.0,
                "z": 0.0,
                "direction": "+x",
                "width_m": 2.0,
                "y0_m": 0.0,
                "rise_m": 2.0,
                "run_m": 0.5
            }),
        )
        .expect("steep ramp");
        assert_eq!(verdict["flagged"], json!(true));
        assert_eq!(verdict["flag"], json!("too_steep"));
        assert_eq!(verdict["connected"], json!(false));
    }

    /// A flat 8x8 navmesh as a resource (via the full builder over a floor triangle soup).
    fn app_with_mesh() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let c = 4.0;
        let triangles = vec![
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, -c),
                Vec3::new(c, 0.0, c),
            ],
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, c),
                Vec3::new(-c, 0.0, c),
            ],
        ];
        let mesh = NavMeshBuilder::default().build(&triangles).expect("builds");
        app.insert_resource(mesh);
        app.insert_resource(NavObstacles::default());
        app
    }

    #[test]
    fn get_mesh_reports_counts() {
        let mut app = app_with_mesh();
        let result = get_mesh(app.world_mut(), &json!({})).expect("works");
        assert!(result["polys"].as_u64().unwrap_or(0) > 0);
        assert!(result["regions"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(result["layers"].as_u64(), Some(1), "a flat floor is one layer");
        assert!(result["agent"]["radius"].as_f64().is_some());
    }

    /// ADR-8076: a mesh with a floor under another walkable surface reports its layers.
    #[test]
    fn get_mesh_reports_stacked_layers() {
        let mut app = App::new();
        let quad = |y: f32| {
            let c = 6.0;
            [
                [Vec3::new(-c, y, -c), Vec3::new(c, y, -c), Vec3::new(c, y, c)],
                [Vec3::new(-c, y, -c), Vec3::new(c, y, c), Vec3::new(-c, y, c)],
            ]
        };
        let mut triangles = Vec::new();
        for y in [0.0, 3.0] {
            triangles.extend(quad(y));
        }
        let mesh = NavMeshBuilder::default()
            .build_layered(&triangles, &[], &[], 4)
            .expect("builds");
        app.insert_resource(mesh);
        let result = get_mesh(app.world_mut(), &json!({})).expect("works");
        assert_eq!(result["layers"].as_u64(), Some(2));
        assert_eq!(result["regions"].as_u64(), Some(2));
    }

    #[test]
    fn find_nearest_and_path_round_trip() {
        let mut app = app_with_mesh();
        let nearest = find_nearest(app.world_mut(), &json!({ "point": [0.0, 0.0, 0.0] })).expect("works");
        assert!(nearest["poly"].as_u64().is_some());

        let path = find_path(
            app.world_mut(),
            &json!({ "from": [-2.0, 0.0, -2.0], "to": [2.0, 0.0, 2.0] }),
        )
        .expect("works");
        assert!(path["path"].is_object(), "{path}");
        assert!(path["path"]["waypoints"].as_array().is_some_and(|w| w.len() >= 2));
    }

    /// ADR-8486: `nav.plan` through the dispatcher on a two-floor layered mesh with a pilot ladder
    /// link: the plan climbs, reports the layer of each waypoint and the goal's layer, and an
    /// off-mesh goal returns its reason code.
    #[test]
    fn plan_reports_layers_through_the_dispatcher() {
        let mut app = App::new();
        let quad = |y: f32| {
            let c = 6.0;
            [
                [Vec3::new(-c, y, -c), Vec3::new(c, y, -c), Vec3::new(c, y, c)],
                [Vec3::new(-c, y, -c), Vec3::new(c, y, c), Vec3::new(-c, y, c)],
            ]
        };
        let mut triangles = Vec::new();
        for y in [0.0, 15.0] {
            triangles.extend(quad(y));
        }
        let agent = regia_nav::NavAgentParams::pilot();
        let params = regia_nav::NavMeshBuildParams {
            agent: agent.clone(),
            ..Default::default()
        };
        let links = [regia_nav::NavLayerLink::ladder(
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(0.0, 15.0, 0.0),
        )];
        let mesh = NavMeshBuilder::new(params)
            .build_layered(&triangles, &[], &[], 4)
            .expect("builds")
            .with_off_mesh_links(regia_nav::pilot_layer_links(&agent, &links));
        app.insert_resource(mesh);
        let reply = crate::dispatch::call_in_process(
            app.world_mut(),
            "nav.plan",
            json!({ "from": [-3.0, 0.0, -3.0], "to": [3.0, 15.0, 3.0] }),
        )
        .expect("immediate answer")
        .expect("nav.plan works");
        let plan = &reply["plan"];
        assert!(plan.is_object(), "{reply}");
        assert_eq!(
            plan["layer"].as_u64(),
            Some(0),
            "the goal is on the top floor: {reply}"
        );
        assert_eq!(
            plan["waypoint_layers"][0].as_u64(),
            Some(1),
            "the start is under it: {reply}"
        );
        assert_eq!(reply["layers"].as_u64(), Some(2));
        assert_eq!(reply["bake_hash"].as_str().map(str::len), Some(16));
        let again = crate::dispatch::call_in_process(
            app.world_mut(),
            "nav.plan",
            json!({ "from": [-3.0, 0.0, -3.0], "to": [3.0, 15.0, 3.0] }),
        )
        .expect("immediate answer")
        .expect("nav.plan works");
        assert_eq!(again, reply, "the same bits twice");
        let off = crate::dispatch::call_in_process(
            app.world_mut(),
            "nav.plan",
            json!({ "from": [-3.0, 0.0, -3.0], "to": [300.0, 0.0, 300.0] }),
        )
        .expect("immediate answer")
        .expect("nav.plan works");
        assert!(off["plan"].is_null());
        assert_eq!(off["reason"], "plan_goal_off_mesh");
    }

    #[test]
    fn obstacles_block_paths() {
        let mut app = app_with_mesh();
        // A wall of obstacle across the middle separates left from right.
        let added = add_obstacle(
            app.world_mut(),
            &json!({ "min": [-0.5, -1.0, -4.0], "max": [0.5, 2.0, 4.0] }),
        )
        .expect("adds");
        let id = added["id"].as_u64().expect("id");

        let path = find_path(
            app.world_mut(),
            &json!({ "from": [-3.0, 0.0, 0.0], "to": [3.0, 0.0, 0.0] }),
        )
        .expect("works");
        assert!(path["path"].is_null(), "the obstacle blocks the only route");

        let removed = remove_obstacle(app.world_mut(), &json!({ "id": id })).expect("removes");
        assert_eq!(removed["removed"], true);

        let path = find_path(
            app.world_mut(),
            &json!({ "from": [-3.0, 0.0, 0.0], "to": [3.0, 0.0, 0.0] }),
        )
        .expect("works");
        assert!(path["path"].is_object(), "the route reopens after removal");
    }

    #[test]
    fn required_nav_ids_distinguish_wrong_types_from_missing_values() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        for (method, field) in [
            ("nav.remove_obstacle", "id"),
            ("nav.tile_cache_remove_obstacle", "id"),
            ("nav.tile_cache_move_obstacle", "id"),
            ("nav.crowd_set_target", "id"),
            ("nav.crowd_set_filter", "id"),
            ("nav.set_link_enabled", "user_id"),
        ] {
            for bad in [json!("5"), json!(1.5), json!(-1), json!(true)] {
                let params = json!({ field: bad });
                let error = crate::dispatch::call_in_process(app.world_mut(), method, params)
                    .expect("immediate answer")
                    .expect_err("wrong type");
                assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{method}");
                assert!(!error.message.contains("required"), "{method}: {error:?}");
                let data = error.data.expect("parameter details");
                assert_eq!(data["param"], field, "{method}");
                assert_eq!(data["expected"], "a non-negative integer", "{method}");
            }
            for missing in [json!({}), json!({ field: null })] {
                let error = crate::dispatch::call_in_process(app.world_mut(), method, missing)
                    .expect("immediate answer")
                    .expect_err("missing id");
                assert!(error.message.contains("required"), "{method}: {error:?}");
            }
        }
        for bad in [json!("false"), json!(0), json!([])] {
            let error = crate::dispatch::call_in_process(
                app.world_mut(),
                "nav.set_link_enabled",
                json!({ "user_id": 42, "enabled": bad }),
            )
            .expect("immediate answer")
            .expect_err("wrong boolean type");
            let data = error.data.expect("parameter details");
            assert_eq!(data["param"], "enabled");
            assert_eq!(data["expected"], "a boolean");
        }
    }

    #[test]
    fn remove_obstacle_without_a_mesh_is_not_found() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let error = remove_obstacle(app.world_mut(), &json!({ "id": 0 })).expect_err("no mesh");
        assert!(error.message.contains("NavMesh"), "{}", error.message);
        // With a mesh, an unknown id is a successful `removed: false`.
        let mut app = app_with_mesh();
        let result = remove_obstacle(app.world_mut(), &json!({ "id": 99 })).expect("works");
        assert_eq!(result["removed"], false);
    }

    #[test]
    fn without_a_mesh_the_handlers_hint_the_builder() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let error = get_mesh(app.world_mut(), &json!({})).expect_err("no mesh");
        assert!(error.message.contains("NavMesh"), "{}", error.message);
        assert!(
            error
                .data
                .as_ref()
                .and_then(|d| d.get("hint"))
                .and_then(Value::as_str)
                .is_some_and(|h| h.contains("NavMeshBuilder")),
            "the hint names the builder"
        );
    }

    fn floor(c: f32) -> Vec<[Vec3; 3]> {
        vec![
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, -c),
                Vec3::new(c, 0.0, c),
            ],
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, c),
                Vec3::new(-c, 0.0, c),
            ],
        ]
    }

    fn app_with_tile_cache() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let cache = regia_nav::NavTileCache::build(
            regia_nav::NavTileCacheParams {
                tile_size: 8,
                ..Default::default()
            },
            &floor(4.0),
        )
        .expect("builds");
        app.insert_resource(cache);
        app
    }

    fn reason(e: &RpcError) -> Option<&str> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
    }

    #[test]
    fn tile_cache_obstacles_rebuild_only_touched_tiles_and_block_paths() {
        let mut app = app_with_tile_cache();
        let status = tile_cache_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["total_rebuilds"], 0);
        let tiles = status["tile_count"].as_u64().expect("count");
        let added = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({ "shape": "box", "min": [-0.3, -1.0, -5.0], "max": [0.3, 2.0, 5.0] }),
        )
        .expect("adds");
        let rebuilt = added["rebuilt"].as_array().expect("rebuilt").len() as u64;
        assert!(rebuilt > 0 && rebuilt < tiles, "{added}");
        assert_eq!(
            added["touched"].as_array().map(Vec::len),
            added["rebuilt"].as_array().map(Vec::len)
        );
        // The wall spans the floor: no route across, through the mesh the cache publishes.
        let blocked = app
            .world()
            .resource::<regia_nav::NavTileCache>()
            .query()
            .find_path(
                Vec3::new(-3.0, 0.0, 0.0),
                Vec3::new(3.0, 0.0, 0.0),
                Vec3::splat(1.0),
            );
        assert!(blocked.is_none());
        let id = added["id"].as_u64().expect("id");
        let removed = tile_cache_remove_obstacle(app.world_mut(), &json!({ "id": id })).expect("removes");
        assert_eq!(removed["removed"], true);
        let status = tile_cache_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["total_rebuilds"].as_u64(), Some(rebuilt * 2));
        assert_eq!(status["obstacles"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn tile_cache_errors_carry_reasons() {
        let mut app = app_with_tile_cache();
        let e = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({ "shape": "cylinder", "base": [0, 0, 0], "radius": 0.0, "height": 1.0 }),
        )
        .expect_err("zero radius");
        assert_eq!(reason(&e), Some("nav_invalid_obstacle"));
        let e = tile_cache_remove_obstacle(app.world_mut(), &json!({ "id": 42 })).expect_err("unknown");
        assert_eq!(reason(&e), Some("nav_unknown_obstacle"));
        assert_eq!(e.code, crate::protocol::NOT_FOUND);
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let e = tile_cache_status(bare.world_mut(), &json!({})).expect_err("no cache");
        assert_eq!(reason(&e), Some("nav_no_tile_cache"));
        let e = crowd_status(bare.world_mut(), &json!({})).expect_err("no crowd");
        assert_eq!(reason(&e), Some("nav_no_crowd"));
        let e = crowd_add_agent(bare.world_mut(), &json!({ "position": [0, 0, 0] })).expect_err("no mesh");
        assert_eq!(reason(&e), Some("nav_no_mesh"));
    }

    #[test]
    fn crowd_agents_walk_to_their_targets_through_the_protocol() {
        let mut app = app_with_tile_cache();
        let added = crowd_add_agent(app.world_mut(), &json!({ "position": [-3, 0, 0] })).expect("adds");
        let id = added["id"].as_u64().expect("id");
        let set =
            crowd_set_target(app.world_mut(), &json!({ "id": id, "target": [3, 0, 0] })).expect("target");
        assert_eq!(set["agent"]["state"], "walking");
        let stepped = crowd_step(app.world_mut(), &json!({ "steps": 600 })).expect("steps");
        assert_eq!(stepped["arrived"], 1, "{stepped}");
        let status = crowd_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["agents"][0]["state"], "arrived");
        assert_eq!(status["stats"]["steps"], 600);
        assert!(status["digest"].as_str().is_some_and(|d| d.len() == 16));

        let e = crowd_add_agent(app.world_mut(), &json!({ "position": [0, 0, 0], "radius": 0.0 }))
            .expect_err("zero radius");
        assert_eq!(reason(&e), Some("nav_invalid_agent_params"));
        let e = crowd_set_target(app.world_mut(), &json!({ "id": 99, "target": [0, 0, 0] }))
            .expect_err("unknown");
        assert_eq!(reason(&e), Some("nav_unknown_agent"));
        let e = crowd_set_target(app.world_mut(), &json!({ "id": id, "target": [50, 0, 0] }))
            .expect_err("off mesh");
        assert_eq!(reason(&e), Some("nav_off_mesh"));
        assert!(crowd_step(app.world_mut(), &json!({ "steps": 0 })).is_err());
    }

    /// Two 5 x 8 m floors at `x ∈ [-6, -1]` and `x ∈ [1, 6]` with a 2 m gap, as a tile cache.
    fn app_with_islands() -> App {
        app_with_island_params(regia_nav::NavTileCacheParams {
            tile_size: 8,
            ..Default::default()
        })
    }

    fn app_with_island_params(params: regia_nav::NavTileCacheParams) -> App {
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
        let cache = regia_nav::NavTileCache::build(params, &tris).expect("builds");
        app.insert_resource(cache);
        app
    }

    /// A valid streamed version of the islands: each tile has its own cooked cell.
    fn app_with_streamed_islands() -> App {
        let mut link = regia_nav::OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
            .one_way()
            .with_user_id(42)
            .with_radius(0.5);
        link.duration = 1.0;
        stream_islands(app_with_islands(), link)
    }

    fn stream_islands(mut app: App, link: regia_nav::OffMeshLink) -> App {
        let mut whole = app
            .world_mut()
            .remove_resource::<regia_nav::NavTileCache>()
            .expect("cache");
        whole.set_off_mesh_links(vec![link]);
        let mut cache = regia_nav::NavTileCache::streamed(whole.params().clone(), whole.layout())
            .expect("streamed cache");
        let (nx, nz) = whole.tile_grid();
        for z in 0..nz {
            for x in 0..nx {
                let blob = whole.cook_cell(&[(x, z)]).expect("cooks");
                cache.load_cell(u64::from(z * nx + x), &blob).expect("loads");
            }
        }
        cache.update();
        assert!(matches!(
            cache.mesh().link_statuses()[0].state,
            regia_nav::NavLinkState::Active { .. }
        ));
        app.insert_resource(cache);
        let added = crowd_add_agent(app.world_mut(), &json!({ "position": [-1.5, 0, 0] })).expect("agent");
        crowd_set_target(
            app.world_mut(),
            &json!({ "id": added["id"], "target": [4, 0, 0] }),
        )
        .expect("target");
        app
    }

    fn unload_link_landing(app: &mut App) {
        let mut cache = app.world_mut().resource_mut::<regia_nav::NavTileCache>();
        let (x, z) = cache.tile_at(Vec3::new(1.5, 0.0, 0.0)).expect("landing tile");
        let (nx, _) = cache.tile_grid();
        cache.unload_cell(u64::from(z * nx + x)).expect("unloads");
        assert_eq!(cache.tile_loaded(x, z), Some(false));
        // Deliberately leave the mesh and corridor stale, as in the audit's setup.
    }

    #[test]
    fn an_unloaded_landing_cannot_start_a_crowd_link_traversal() {
        let mut app = app_with_streamed_islands();
        unload_link_landing(&mut app);
        for steps in [1, 10] {
            let result = crowd_step(app.world_mut(), &json!({ "steps": steps })).expect("steps");
            assert_eq!(result["link_events"], json!([]), "{result}");
            let crowd = app.world().resource::<regia_nav::NavCrowd>();
            assert!(crowd.agents()[0].link_traversal().is_none());
            assert_eq!(crowd.stats().link_cancellations, 0);
            assert_eq!(result["digest"], format!("{:016x}", crowd.digest()));
        }
        let cache = app.world().resource::<regia_nav::NavTileCache>();
        assert!(matches!(
            cache.mesh().link_statuses()[0].state,
            regia_nav::NavLinkState::Unresolved { to: false, .. }
        ));
    }

    #[test]
    fn an_unloaded_landing_reports_an_existing_crossings_cancel_once() {
        let mut app = app_with_streamed_islands();
        let started = crowd_step(app.world_mut(), &json!({ "steps": 1 })).expect("starts");
        assert_eq!(started["link_events"][0]["phase"], "started", "{started}");
        let takeoff = app.world().resource::<regia_nav::NavCrowd>().agents()[0]
            .link_traversal()
            .expect("crossing")
            .start;
        unload_link_landing(&mut app);
        let result = crowd_step(app.world_mut(), &json!({ "steps": 1 })).expect("cancels");
        assert_eq!(
            result["link_events"].as_array().map(Vec::len),
            Some(1),
            "{result}"
        );
        assert_eq!(result["link_events"][0]["phase"], "cancelled");
        assert_eq!(result["link_events"][0]["user_id"], 42);
        assert_eq!(
            result["link_events"][0]["position"],
            json!([takeoff.x, takeoff.y, takeoff.z])
        );
        let status = crowd_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(result["digest"], status["digest"]);
        assert_eq!(status["agents"][0]["link"], Value::Null);
        let next = crowd_step(app.world_mut(), &json!({ "steps": 1 })).expect("next step");
        assert_eq!(next["link_events"], json!([]), "the cancel was already returned");
        assert_eq!(
            app.world()
                .resource::<regia_nav::NavCrowd>()
                .stats()
                .link_cancellations,
            1
        );
    }

    #[test]
    fn crowd_step_events_and_digest_match_the_plugin_at_a_snapped_unloaded_landing() {
        for steps in [1, 2, 3] {
            let params = regia_nav::NavTileCacheParams {
                tile_size: 8,
                build: regia_nav::NavMeshBuildParams {
                    border_size: 3.0,
                    ..Default::default()
                },
            };
            // The authored endpoint is in a loaded empty tile east of the floor. A wide snap
            // radius reaches the floor across a missing tile; the snapped end is its border.
            let to = Vec3::new(7.25, 0.0, 0.0);
            let mut link = regia_nav::OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), to)
                .one_way()
                .with_user_id(42)
                .with_radius(3.0);
            link.duration = 1.0;
            let mut app = stream_islands(app_with_island_params(params), link);
            {
                let mut cache = app.world_mut().resource_mut::<regia_nav::NavTileCache>();
                let (x, _) = cache
                    .tile_at(Vec3::new(5.5, 0.0, 0.0))
                    .expect("tile east of landing");
                let (nx, nz) = cache.tile_grid();
                for z in 0..nz {
                    cache.unload_cell(u64::from(z * nx + x)).expect("unloads");
                }
                cache.update();
                let authored_tile = cache.tile_at(to).expect("authored tile");
                assert_eq!(cache.tile_loaded(authored_tile.0, authored_tile.1), Some(true));
                let regia_nav::NavLinkState::Active { to_poly, .. } = cache.mesh().link_statuses()[0].state
                else {
                    panic!("the authored endpoint is loaded and resolves");
                };
                let end = cache.mesh().polys[to_poly as usize].closest_point_xz(to.x, to.z);
                let end_tile = cache.tile_at(end).expect("snapped landing tile");
                assert_eq!(
                    cache.tile_loaded(end_tile.0, end_tile.1),
                    Some(false),
                    "snapped end {end:?}"
                );
            }
            let mut reference = App::new();
            reference
                .add_plugins(regia_nav::NavCrowdPlugin)
                .insert_resource(app.world().resource::<regia_nav::NavTileCache>().clone())
                .insert_resource(app.world().resource::<regia_nav::NavCrowd>().clone());
            let mut events = Vec::new();
            for _ in 0..steps {
                reference.world_mut().run_schedule(FixedUpdate);
                events.extend(
                    reference
                        .world()
                        .resource::<regia_nav::NavCrowd>()
                        .link_events()
                        .iter()
                        .map(link_event_json),
                );
            }
            let expected = reference.world().resource::<regia_nav::NavCrowd>();
            assert_eq!(events.first().expect("traversal starts")["phase"], "started");
            assert_eq!(expected.stats().link_cancellations, steps - 1);
            let result = crowd_step(app.world_mut(), &json!({ "steps": steps })).expect("steps");
            assert_eq!(result["link_events"], json!(events), "{result}");
            assert_eq!(
                result["digest"],
                format!("{:016x}", expected.digest()),
                "events and digest must describe the same final step, as in NavCrowdPlugin"
            );
            assert_eq!(
                app.world()
                    .resource::<regia_nav::NavCrowd>()
                    .stats()
                    .link_cancellations,
                expected.stats().link_cancellations
            );
        }
    }

    #[test]
    fn generated_links_carry_crowd_agents_across_a_gap() {
        let mut app = app_with_islands();
        let preview = generate_links(app.world_mut(), &json!({})).expect("generates");
        assert_eq!(preview["applied"], false);
        assert!(preview["count"].as_u64().is_some_and(|n| n > 0), "{preview}");
        assert_eq!(
            links(app.world_mut(), &json!({})).expect("links")["links"],
            json!([])
        );
        let applied = generate_links(app.world_mut(), &json!({ "apply": true })).expect("applies");
        let count = applied["count"].as_u64().expect("count");
        // Applying again replaces the earlier generated set instead of doubling it.
        generate_links(app.world_mut(), &json!({ "apply": true })).expect("applies");
        let listed = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(listed["links"].as_array().map(Vec::len), Some(count as usize));
        assert_eq!(listed["active"].as_u64(), Some(count), "{listed}");
        assert_eq!(listed["links"][0]["kind"], "jump");
        assert_eq!(listed["links"][0]["state"], "active");

        let added = crowd_add_agent(app.world_mut(), &json!({ "position": [-4, 0, 0] })).expect("adds");
        let id = added["id"].as_u64().expect("id");
        crowd_set_target(app.world_mut(), &json!({ "id": id, "target": [4, 0, 0] })).expect("target");
        let stepped = crowd_step(app.world_mut(), &json!({ "steps": 600 })).expect("steps");
        assert_eq!(stepped["arrived"], 1, "{stepped}");
        let events = stepped["link_events"].as_array().expect("events");
        let phases: Vec<&str> = events.iter().filter_map(|e| e["phase"].as_str()).collect();
        assert_eq!(phases, ["started", "finished"], "{stepped}");
        assert_eq!(events[0]["kind"], "jump");
        let status = crowd_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["stats"]["link_traversals"], 1);
        assert_eq!(status["agents"][0]["link"], Value::Null);
    }

    fn has_route(app: &App) -> bool {
        app.world()
            .resource::<regia_nav::NavTileCache>()
            .query()
            .find_path(
                Vec3::new(-4.0, 0.0, 0.0),
                Vec3::new(4.0, 0.0, 0.0),
                Vec3::splat(1.0),
            )
            .is_some()
    }

    #[test]
    fn links_switch_off_and_obstacles_block_their_ends() {
        let mut app = app_with_islands();
        let ron = "(format_version: 1, links: [(from: (-1.5, 0.0, 0.0), to: (1.5, 0.0, 0.0), user_id: 5, radius: 0.5)])";
        let set = set_links(app.world_mut(), &json!({ "ron": ron })).expect("sets");
        assert_eq!(set, json!({ "count": 1, "active": 1 }));
        assert!(has_route(&app));
        let off = set_link_enabled(app.world_mut(), &json!({ "user_id": 5, "enabled": false })).expect("off");
        assert_eq!(off["changed"], 1);
        assert_eq!(links(app.world_mut(), &json!({})).expect("links")["disabled"], 1);
        assert!(!has_route(&app));
        set_link_enabled(app.world_mut(), &json!({ "user_id": 5, "enabled": true })).expect("on");

        // A crate set down on the landing blocks the link; moving it away restores it.
        let added = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({ "shape": "oriented_box", "center": [1.6, 0.5, 0.0], "half_extents": [0.3, 0.5, 0.3], "yaw": 0.6 }),
        )
        .expect("adds");
        let id = added["id"].as_u64().expect("id");
        let listed = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(listed["links"][0]["state"], "blocked", "{listed}");
        assert!(!has_route(&app));
        let moved = tile_cache_move_obstacle(
            app.world_mut(),
            &json!({ "id": id, "shape": "box", "min": [4.0, -1.0, 2.0], "max": [5.0, 1.0, 3.0] }),
        )
        .expect("moves");
        assert_eq!(moved["id"], id);
        assert!(!moved["touched"].as_array().expect("touched").is_empty());
        assert_eq!(
            links(app.world_mut(), &json!({})).expect("links")["links"][0]["state"],
            "active"
        );
        assert!(has_route(&app));
        let status = tile_cache_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["obstacles"][0]["shape"], "box");
    }

    #[test]
    fn a_link_that_comes_back_replans_crowd_agents() {
        let ron = "(format_version: 1, links: [(from: (-1.5, 0.0, 0.0), to: (1.5, 0.0, 0.0), user_id: 5, radius: 0.5, enabled: false)])";
        // The links live on the tile cache in one app and on a plain `NavMesh` in the other.
        let cached = app_with_islands();
        let mut plain = app_with_islands();
        let mesh = plain
            .world_mut()
            .remove_resource::<regia_nav::NavTileCache>()
            .expect("the cache")
            .mesh()
            .clone();
        plain.world_mut().insert_resource(mesh);
        for mut app in [cached, plain] {
            set_links(app.world_mut(), &json!({ "ron": ron })).expect("sets");
            let added = crowd_add_agent(app.world_mut(), &json!({ "position": [-4, 0, 0] })).expect("adds");
            let id = added["id"].as_u64().expect("id");
            crowd_set_target(app.world_mut(), &json!({ "id": id, "target": [4, 0, 0] })).expect("target");
            crowd_step(app.world_mut(), &json!({ "steps": 10 })).expect("steps");
            let status = crowd_status(app.world_mut(), &json!({})).expect("status");
            assert_eq!(status["agents"][0]["state"], "no_path", "{status}");

            set_link_enabled(app.world_mut(), &json!({ "user_id": 5, "enabled": true })).expect("on");
            let stepped = crowd_step(app.world_mut(), &json!({ "steps": 600 })).expect("steps");
            assert_eq!(stepped["arrived"], 1, "{stepped}");
        }
    }

    #[test]
    fn add_obstacle_blocks_the_link_nav_links_reports() {
        let mut app = app_with_islands();
        let mesh = app
            .world_mut()
            .remove_resource::<regia_nav::NavTileCache>()
            .expect("cache")
            .mesh()
            .clone();
        let polys_before = mesh.poly_count();
        app.world_mut().insert_resource(mesh);
        let ron = "(format_version: 1, links: [(from: (-1.5, 0.0, 0.0), to: (1.5, 0.0, 0.0), user_id: 7, radius: 0.5)])";
        set_links(app.world_mut(), &json!({ "ron": ron })).expect("sets");
        let before = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(before["links"][0]["state"], "active", "{before}");
        let across = find_path(
            app.world_mut(),
            &json!({ "from": [-4.0, 0.0, 0.0], "to": [4.0, 0.0, 0.0] }),
        )
        .expect("path");
        assert!(across["path"].is_object(), "{across}");

        add_obstacle(
            app.world_mut(),
            &json!({ "min": [1.2, -1.0, -0.4], "max": [1.8, 1.0, 0.4] }),
        )
        .expect("obstacle");
        let after = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(after["links"][0]["state"], "blocked", "{after}");
        assert_eq!(after["blocked"], 1);
        let blocked = find_path(
            app.world_mut(),
            &json!({ "from": [-4.0, 0.0, 0.0], "to": [4.0, 0.0, 0.0] }),
        )
        .expect("query");
        assert!(blocked["path"].is_null(), "{blocked}");
        assert_eq!(
            get_mesh(app.world_mut(), &json!({})).expect("mesh")["polys"].as_u64(),
            Some(polys_before as u64)
        );
    }

    #[test]
    fn add_obstacle_applies_even_when_dry_run_is_true() {
        let mut app = app_with_mesh();
        let added = add_obstacle(
            app.world_mut(),
            &json!({ "min": [-0.5, -1.0, -4.0], "max": [0.5, 2.0, 4.0], "dry_run": true }),
        )
        .expect("applies");
        assert_eq!(added["obstacles"], 1);
        assert!(added.get("diff").is_none(), "{added}");
        assert!(added.get("undo_id").is_none(), "{added}");
        let path = find_path(
            app.world_mut(),
            &json!({ "from": [-3.0, 0.0, 0.0], "to": [3.0, 0.0, 0.0] }),
        )
        .expect("query");
        assert!(path["path"].is_null(), "{path}");
    }

    #[test]
    fn a_tile_cache_alone_serves_path_queries() {
        let mut app = app_with_islands();
        let path = find_path(
            app.world_mut(),
            &json!({ "from": [-4.0, 0.0, 0.0], "to": [-2.0, 0.0, 0.0] }),
        )
        .expect("cache path");
        assert!(path["path"].is_object(), "{path}");
        let err = get_mesh(app.world_mut(), &json!({})).expect_err("no NavMesh");
        let hint = err.data.as_ref().and_then(|d| d["hint"].as_str()).unwrap_or("");
        assert!(hint.contains("NavMeshBuilder"), "{hint}");
        assert!(!hint.contains("NavPlugin"), "{hint}");
    }

    #[test]
    fn path_queries_ignore_cache_links_when_a_navmesh_is_present() {
        let mut app = app_with_islands();
        let ron = "(format_version: 1, links: [(from: (-1.5, 0.0, 0.0), to: (1.5, 0.0, 0.0), user_id: 7, radius: 0.5)])";
        set_links(app.world_mut(), &json!({ "ron": ron })).expect("sets");
        let listed = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(listed["links"][0]["state"], "active", "{listed}");
        let via_cache = find_path(
            app.world_mut(),
            &json!({ "from": [-4.0, 0.0, 0.0], "to": [4.0, 0.0, 0.0] }),
        )
        .expect("cache");
        assert!(via_cache["path"].is_object(), "{via_cache}");

        let left = vec![
            [
                Vec3::new(-6.0, 0.0, -4.0),
                Vec3::new(-1.0, 0.0, -4.0),
                Vec3::new(-1.0, 0.0, 4.0),
            ],
            [
                Vec3::new(-6.0, 0.0, -4.0),
                Vec3::new(-1.0, 0.0, 4.0),
                Vec3::new(-6.0, 0.0, 4.0),
            ],
        ];
        let floor = NavMeshBuilder::default().build(&left).expect("left island");
        app.world_mut().insert_resource(floor);
        let via_mesh = find_path(
            app.world_mut(),
            &json!({ "from": [-4.0, 0.0, 0.0], "to": [4.0, 0.0, 0.0] }),
        )
        .expect("navmesh wins");
        assert!(via_mesh["path"].is_null(), "{via_mesh}");
        let still = links(app.world_mut(), &json!({})).expect("links");
        assert_eq!(still["links"][0]["state"], "active", "{still}");
    }

    #[test]
    fn oriented_box_obstacle_is_accepted() {
        let mut app = app_with_islands();
        let added = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({
                "shape": "oriented_box",
                "center": [0.0, 0.0, 0.0],
                "half_extents": [1.0, 1.0, 1.0],
                "yaw": 0.0
            }),
        )
        .expect("adds");
        assert!(added["id"].as_u64().is_some(), "{added}");
        let status = tile_cache_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["obstacles"][0]["shape"], "oriented_box");
    }

    #[test]
    fn a_non_finite_crowd_position_is_f0002_without_a_reason() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let err =
            crowd_add_agent(app.world_mut(), &json!({ "position": [1e40, 0.0, 0.0] })).expect_err("range");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let data = err.data.expect("data");
        assert_eq!(data["code"], "F0002");
        assert!(data.get("reason").is_none(), "{data}");
        let err = crowd_set_target(app.world_mut(), &json!({ "id": 0, "target": [1e40, 0.0, 0.0] }))
            .expect_err("range");
        assert!(err.data.expect("data").get("reason").is_none());
    }

    #[test]
    fn link_errors_carry_reasons() {
        let mut app = app_with_islands();
        let e = set_link_enabled(app.world_mut(), &json!({ "user_id": 9, "enabled": false }))
            .expect_err("unknown");
        assert_eq!(reason(&e), Some("nav_unknown_link"));
        assert_eq!(e.code, crate::protocol::NOT_FOUND);
        let e = set_links(
            app.world_mut(),
            &json!({ "ron": "(format_version: 7, links: [])" }),
        )
        .expect_err("newer");
        assert_eq!(reason(&e), Some("nav_link_version"));
        let e = set_links(app.world_mut(), &json!({ "ron": "(" })).expect_err("parse");
        assert_eq!(reason(&e), Some("nav_link_parse"));
        let e = set_links(
            app.world_mut(),
            &json!({ "links": [ { "from": [0, 0, 0], "to": [1, 0, 0], "radius": -1 } ] }),
        )
        .expect_err("invalid");
        assert_eq!(reason(&e), Some("nav_invalid_link"));
        assert!(
            set_links(
                app.world_mut(),
                &json!({ "links": [ { "from": [0, 0, 0], "to": [1, 0, 0], "kind": "fly" } ] })
            )
            .is_err()
        );
        assert!(
            set_links(app.world_mut(), &json!({})).is_err(),
            "neither links nor ron"
        );
        let e = generate_links(app.world_mut(), &json!({ "spacing": 0 })).expect_err("bad params");
        assert_eq!(reason(&e), Some("nav_invalid_link_params"));
        let e = tile_cache_move_obstacle(
            app.world_mut(),
            &json!({ "id": 3, "shape": "box", "min": [0, 0, 0], "max": [1, 1, 1] }),
        )
        .expect_err("unknown obstacle");
        assert_eq!(reason(&e), Some("nav_unknown_obstacle"));
        let e = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({ "shape": "oriented_box", "center": [0, 0, 0], "half_extents": [0, 1, 1] }),
        )
        .expect_err("flat box");
        assert_eq!(reason(&e), Some("nav_invalid_obstacle"));
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let e = links(bare.world_mut(), &json!({})).expect_err("no mesh");
        assert_eq!(reason(&e), Some("nav_no_mesh"));
    }

    /// Spec 92 § 4: every `nav.links` field of the table, the crowd agent's `link` object while
    /// it crosses, and the `rebuilt` / `pending` / `revision` results of
    /// `nav.tile_cache_move_obstacle` (deferred, then applied).
    #[test]
    fn link_fields_the_crossing_object_and_move_results_match_the_spec() {
        let mut app = app_with_islands();
        let link = json!({
            "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "user_id": 9, "kind": "climb", "flags": 2,
            "radius": 0.5, "cost_scale": 2.0, "duration": 1.5, "bidirectional": false,
        });
        set_links(app.world_mut(), &json!({ "links": [link] })).expect("sets");
        let listed = links(app.world_mut(), &json!({})).expect("links");
        let l = &listed["links"][0];
        assert_eq!(l["index"], 0);
        assert_eq!(l["user_id"], 9);
        assert_eq!(l["kind"], "climb");
        assert_eq!(l["from"], json!([-1.5, 0.0, 0.0]));
        assert_eq!(l["to"], json!([1.5, 0.0, 0.0]));
        assert_eq!(l["bidirectional"], false);
        assert_eq!(l["enabled"], true);
        assert_eq!(l["flags"], 2);
        assert_eq!(l["radius"], 0.5);
        assert_eq!(l["cost_scale"], 2.0);
        assert_eq!(l["duration"], 1.5);
        assert_eq!(l["state"], "active");
        assert!(l["from_poly"].is_u64() && l["to_poly"].is_u64(), "{l}");
        assert_ne!(l["from_poly"], l["to_poly"]);
        assert_eq!(
            (
                &listed["active"],
                &listed["blocked"],
                &listed["disabled"],
                &listed["unresolved"]
            ),
            (&json!(1), &json!(0), &json!(0), &json!(0))
        );

        let added = crowd_add_agent(app.world_mut(), &json!({ "position": [-4, 0, 0] })).expect("adds");
        let id = added["id"].as_u64().expect("id");
        crowd_set_target(app.world_mut(), &json!({ "id": id, "target": [4, 0, 0] })).expect("target");
        let mut last_progress = -1.0;
        let mut seen = 0;
        for _ in 0..600 {
            crowd_step(app.world_mut(), &json!({ "steps": 1 })).expect("steps");
            let status = crowd_status(app.world_mut(), &json!({})).expect("status");
            let on = &status["agents"][0]["link"];
            if on.is_null() {
                if seen > 0 {
                    break;
                }
                continue;
            }
            seen += 1;
            assert_eq!(status["agents"][0]["state"], "off_mesh_link");
            assert_eq!((&on["user_id"], &on["kind"]), (&json!(9), &json!("climb")));
            let progress = on["progress"].as_f64().expect("progress");
            assert!(progress > last_progress && progress <= 1.0, "{on}");
            last_progress = progress;
            // `start` is the take-off point, `end` the landing (clamped onto its polygon).
            let near = |p: &Value, x: f64| {
                (p[0].as_f64().unwrap_or(f64::NAN) - x).abs() < 1e-2
                    && p[2].as_f64().unwrap_or(f64::NAN).abs() < 1e-2
                    && p[1].as_f64().unwrap_or(f64::NAN).abs() < 0.3
            };
            assert!(near(&on["start"], -1.5), "{on}");
            assert!(near(&on["end"], 1.5), "{on}");
            assert!(on["origin"].as_array().is_some_and(|o| o.len() == 3), "{on}");
        }
        assert!(seen > 1, "the crossing took several steps");

        let added = tile_cache_add_obstacle(
            app.world_mut(),
            &json!({ "shape": "box", "min": [3.0, -1.0, 2.0], "max": [4.0, 1.0, 3.0] }),
        )
        .expect("adds");
        let obstacle = added["id"].as_u64().expect("id");
        let r0 = added["revision"].as_u64().expect("revision");
        let deferred = tile_cache_move_obstacle(
            app.world_mut(),
            &json!({ "id": obstacle, "shape": "box", "min": [-5.0, -1.0, 2.0], "max": [-4.0, 1.0, 3.0], "update": false }),
        )
        .expect("moves");
        assert_eq!(deferred["rebuilt"], json!([]));
        let touched = deferred["touched"].as_array().cloned().unwrap_or_default();
        assert!(!touched.is_empty());
        assert_eq!(
            deferred["pending"].as_array().map(Vec::len),
            Some(touched.len()),
            "{deferred}"
        );
        assert!(deferred["revision"].is_u64());
        let applied = tile_cache_move_obstacle(
            app.world_mut(),
            &json!({ "id": obstacle, "shape": "box", "min": [-5.0, -1.0, -3.0], "max": [-4.0, 1.0, -2.0] }),
        )
        .expect("moves");
        assert_eq!(applied["pending"], json!([]), "{applied}");
        assert!(
            applied["rebuilt"].as_array().is_some_and(|r| !r.is_empty()),
            "{applied}"
        );
        assert!(applied["revision"].as_u64().is_some_and(|r| r > r0), "{applied}");
    }
}
