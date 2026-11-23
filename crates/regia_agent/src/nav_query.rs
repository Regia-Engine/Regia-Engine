// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the NM-1/NM-2 navmesh queries (`book/agents/navigation.md`,
//! `book/agents/navigation-crowds.md`): a raycast along the mesh, seeded random points (on the
//! whole mesh or in a radius), the `partial` form of `nav.find_path`, and crowd stuck detection.

use regia_app::prelude::*;
use regia_core::math::{Rng, Vec3};
use regia_nav::{NavMesh, NavPath, NavQuery, NavQueryError, NavQueryFilter};
use serde_json::{Value, json};

use crate::nav::{crowd_error, nav_error, vec3_optional, vec3_param, with_crowd_cache};
use crate::protocol::RpcError;

/// The most points one `nav.random_point` call returns.
pub(crate) const MAX_RANDOM_POINTS: u64 = 4096;

fn v3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `nav_no_mesh`: neither a `NavMesh` nor a `NavTileCache` to query.
fn no_mesh() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_mesh",
        "no NavMesh or NavTileCache in this app: build one (NavMeshBuilder) and insert it as a resource",
    )
}

/// A [`NavQueryError`] with its documented reason.
pub(crate) fn query_error(e: &NavQueryError) -> RpcError {
    match e {
        NavQueryError::NoStartPoly => nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string()),
        _ => nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string()),
    }
}

/// Runs `f` with a query over the app's mesh (the `NavMesh` with its obstacles, else the tile
/// cache's mesh), or `nav_no_mesh`.
fn with_query<T>(world: &mut World, f: impl FnOnce(&NavQuery<'_>) -> T) -> Result<T, RpcError> {
    if world.get_resource::<NavMesh>().is_some() {
        let query = NavQuery {
            mesh: world.resource::<NavMesh>(),
            obstacles: world.get_resource::<regia_nav::NavObstacles>(),
        };
        return Ok(f(&query));
    }
    if let Some(cache) = world.get_resource::<regia_nav::NavTileCache>() {
        return Ok(f(&cache.query()));
    }
    Err(no_mesh())
}

/// The optional `extents` ([x, y, z], default [2, 2, 2]); a negative component is
/// `nav_bad_extents`.
fn extents_param(params: &Value) -> Result<Vec3, RpcError> {
    let extents = vec3_optional(params, "extents", Vec3::splat(2.0))?;
    if extents.cmplt(Vec3::ZERO).any() {
        return Err(query_error(&NavQueryError::BadExtents));
    }
    Ok(extents)
}

fn filter(params: &Value) -> Result<NavQueryFilter, RpcError> {
    Ok(crate::nav_lanes::filter_param(params)?.unwrap_or_default())
}

/// `nav.raycast`: walks the mesh from `from` toward `to` and reports the first wall.
pub fn raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = vec3_param(params, "from")?;
    let to = vec3_param(params, "to")?;
    let extents = extents_param(params)?;
    let filter = filter(params)?;
    let hit = with_query(world, |q| q.raycast(from, to, extents, &filter))?.map_err(|e| query_error(&e))?;
    Ok(json!({
        "hit": hit.hit,
        "t": hit.t,
        "normal": v3(hit.normal),
        "point": v3(hit.point),
        "start": v3(hit.start),
        "last_poly": hit.last_poly,
        "path_polys": hit.path_polys,
    }))
}

/// `nav.random_point`: `count` seeded random points, on the whole mesh or within `radius` of
/// `center` (`reachable_only` skips islands).
pub fn random_point(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let seed = crate::params::opt_u64(params, "seed")?.unwrap_or(0);
    let count = match crate::params::opt_u64(params, "count")? {
        None => 1,
        Some(n) if (1..=MAX_RANDOM_POINTS).contains(&n) => n,
        Some(_) => return Err(RpcError::invalid_params("`count` must be an integer in 1..=4096")),
    };
    let filter = filter(params)?;
    let extents = extents_param(params)?;
    let circle = match (params.get("center"), params.get("radius")) {
        (None, None) => None,
        (Some(_), Some(r)) => {
            let center = vec3_param(params, "center")?;
            let radius = r
                .as_f64()
                .map(|r| r as f32)
                .ok_or_else(|| RpcError::invalid_params("`radius` must be a number"))?;
            Some((center, radius))
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`center` and `radius` go together (both or neither)",
            ));
        }
    };
    let reachable_only = crate::params::opt_bool(params, "reachable_only")?.unwrap_or(false);
    let mut rng = Rng::new(seed);
    let points = with_query(world, |q| -> Result<Vec<Value>, NavQueryError> {
        let mut out = Vec::new();
        for _ in 0..count {
            let drawn = match circle {
                Some((center, radius)) => {
                    q.random_point_in_radius(center, radius, extents, reachable_only, &filter, &mut rng)?
                }
                None => q.random_point(&filter, &mut rng),
            };
            match drawn {
                Some((poly, p)) => out.push(json!({ "poly": poly, "point": v3(p) })),
                None => break,
            }
        }
        Ok(out)
    })?
    .map_err(|e| query_error(&e))?;
    Ok(json!({ "seed": seed, "points": points }))
}

fn path_json(path: &NavPath) -> Value {
    json!({
        "waypoints": path.waypoints.iter().map(|w| [w.x, w.y, w.z]).collect::<Vec<_>>(),
        "polys": path.polys,
        "length": path.length,
    })
}

/// `nav.find_path` with `"partial": true`: the path to the reachable point nearest an
/// unreachable goal, and how the search ended (`complete`, `partial`, `no_start`).
pub(crate) fn find_path_partial(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = vec3_param(params, "from")?;
    let to = vec3_param(params, "to")?;
    let extents = extents_param(params)?;
    let filter = filter(params)?;
    let result = with_query(world, |q| q.find_path_partial(from, to, extents, &filter))?;
    Ok(json!({
        "status": result.name(),
        "path": result.path().map(path_json),
    }))
}

/// `nav.crowd_set_stuck`: switches crowd stuck detection (`window_steps` 0 turns it off).
pub fn crowd_set_stuck(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let window = match crate::params::opt_u64(params, "window_steps")? {
        Some(n) if n <= u64::from(u32::MAX) => n as u32,
        Some(_) => return Err(RpcError::invalid_params("`window_steps` must fit in 32 bits")),
        None => {
            return Err(RpcError::invalid_params(
                "`window_steps` (an integer, 0 = off) is required",
            ));
        }
    };
    let min_progress = crate::params::opt_f64(params, "min_progress_m")?
        .map_or(regia_nav::DEFAULT_STUCK_MIN_PROGRESS_M, |v| v as f32);
    with_crowd_cache(world, true, |crowd, _, _| {
        crowd
            .set_stuck_detection(window, min_progress)
            .map_err(|e| crowd_error(&e))?;
        let c = crowd.config();
        Ok(json!({
            "window_steps": c.stuck_window_steps,
            "min_progress_m": c.stuck_min_progress_m,
            "enabled": c.stuck_window_steps > 0,
        }))
    })
}

/// The stuck part of `nav.crowd_status` and `nav.crowd_step`.
pub(crate) fn crowd_stuck_json(crowd: &regia_nav::NavCrowd) -> Value {
    let s = crowd.stats();
    let c = crowd.config();
    json!({
        "window_steps": c.stuck_window_steps,
        "min_progress_m": c.stuck_min_progress_m,
        "stuck_now": crowd.stuck_count(),
        "detections": s.stuck_detections,
        "repaths": s.stuck_repaths,
        "nudges": s.stuck_nudges,
        "reports": s.stuck_reports,
        "recoveries": s.stuck_recoveries,
    })
}

/// One stuck-ladder rung of the last step, for `nav.crowd_step`.
pub(crate) fn stuck_event_json(e: &regia_nav::NavStuckEvent) -> Value {
    json!({
        "agent": e.agent.0,
        "action": e.action.name(),
        "position": v3(e.position),
        "goal": v3(e.goal),
    })
}
