// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the nav-world remainder (ADR-2420, `book/agents/navigation-authoring.md`):
//! link reservation and queues, lanes from road splines, per-triangle area paint, and the runtime
//! link and lane files.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{NavLaneGraph, NavLinkQueueConfig, NavRoadLaneParams, NavRoadNetwork, NavTileCache};
use regia_scene::spline::Spline;
use serde_json::{Value, json};

use crate::nav::{
    bool_field, f32_param, nav_error, no_crowd, no_tile_cache, tile_cache_error, u64_field, with_crowd_cache,
};
use crate::protocol::RpcError;

/// The most triangles one `nav.cook_triangle_areas` call takes.
const MAX_COOK_TRIANGLES: usize = 1 << 20;

/// The most lanes `nav.lanes_from_splines` lists point by point.
const MAX_LISTED_LANES: usize = 256;

fn authoring_error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    let mut err = nav_error(code, regia_code, reason, message);
    if let Some(data) = err.data.as_mut() {
        data["hint"] = json!("see book/agents/navigation-authoring.md");
    }
    err
}

fn lane_error(e: &regia_nav::NavLaneError) -> RpcError {
    authoring_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string())
}

// -- Link reservation (NavLinkQueueConfig) ---------------------------------------------------

fn queue_json(crowd: &regia_nav::NavCrowd, mesh: &regia_nav::NavMesh) -> Value {
    let q = crowd.config().link_queue;
    let user_id = |link: usize| mesh.off_mesh_links.get(link).map_or(0, |l| l.user_id);
    let holders: Vec<Value> = crowd
        .link_holders()
        .iter()
        .map(|&(link, agent)| json!({ "link": link, "user_id": user_id(link), "agent": agent.0 }))
        .collect();
    let mut links: Vec<usize> = crowd
        .agents()
        .iter()
        .filter_map(|a| a.link_wait().map(|w| w.link))
        .collect();
    links.sort_unstable();
    links.dedup();
    let queues: Vec<Value> = links
        .iter()
        .map(|&link| {
            let agents: Vec<u64> = crowd.link_queue(link).iter().map(|a| a.0).collect();
            json!({ "link": link, "user_id": user_id(link), "agents": agents })
        })
        .collect();
    let s = crowd.stats();
    json!({
        "exclusive": q.exclusive,
        "timeout_steps": q.timeout_steps,
        "holders": holders,
        "queues": queues,
        "waits": s.link_queue_waits,
        "timeouts": s.link_queue_timeouts,
    })
}

/// `nav.crowd_set_link_queue`: one agent per link at a time, with FIFO queues and a timeout.
/// Creates the crowd when there is none.
pub fn crowd_set_link_queue(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let exclusive = params
        .get("exclusive")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`exclusive` (true or false) is required"))?;
    let timeout_steps = u64_field(params, "timeout_steps", 0, u64::from(u32::MAX))? as u32;
    with_crowd_cache(world, true, |crowd, mesh, _| {
        crowd.set_link_queue(NavLinkQueueConfig {
            exclusive,
            timeout_steps,
        });
        Ok(queue_json(crowd, mesh))
    })
}

/// `nav.crowd_link_queues`: who crosses each reserved link and who waits for it.
pub fn crowd_link_queues(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<regia_nav::NavCrowd>().is_none() {
        return Err(no_crowd());
    }
    with_crowd_cache(world, false, |crowd, mesh, _| Ok(queue_json(crowd, mesh)))
}

// -- Lanes from road splines ------------------------------------------------------------------

fn road_params(params: &Value) -> Result<NavRoadLaneParams, RpcError> {
    let d = NavRoadLaneParams::default();
    let p = NavRoadLaneParams {
        width: f32_param(params, "width", d.width)?,
        lanes_each_way: u64_field(params, "lanes_each_way", u64::from(d.lanes_each_way), 1024)? as u32,
        two_way: bool_field(params, "two_way", d.two_way)?,
        center_offset: f32_param(params, "center_offset", d.center_offset)?,
        speed: f32_param(params, "speed", d.speed)?,
        flags: u64_field(params, "flags", u64::from(d.flags), u64::from(u16::MAX))? as u16,
        weight: f32_param(params, "weight", d.weight)?,
        max_error: f32_param(params, "max_error", d.max_error)?,
        first_user_id: u64_field(params, "first_user_id", d.first_user_id, u64::MAX >> 1)?,
    };
    p.validate().map_err(|e| lane_error(&e))?;
    Ok(p)
}

/// `nav.lanes_from_splines`: lanes along the scene's road splines (entities with a `Spline` and,
/// unless `roads_only` is false, a `RoadOf`), in entity order, joined end to start within
/// `join_distance`; `apply` makes them the lane graph (dropping a lane crowd). The splines are
/// read, never changed.
pub fn lanes_from_splines(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let road = road_params(params)?;
    let join = f32_param(params, "join_distance", 0.0)?;
    let roads_only = bool_field(params, "roads_only", true)?;
    let apply = bool_field(params, "apply", false)?;
    let wanted: Option<Vec<u64>> = match params.get("entities") {
        None => None,
        Some(v) => Some(
            v.as_array()
                .ok_or_else(|| RpcError::invalid_params("`entities` must be an array of entity ids"))?
                .iter()
                .map(|e| {
                    e.as_u64()
                        .ok_or_else(|| RpcError::invalid_params("`entities` must hold integer entity ids"))
                })
                .collect::<Result<_, _>>()?,
        ),
    };
    let mut splines: Vec<(Entity, Spline)> = {
        let mut q = world.query::<(Entity, &Spline, Has<regia_procgen::road::RoadOf>)>();
        q.iter(world)
            .filter(|(e, _, is_road)| {
                (*is_road || !roads_only) && wanted.as_ref().is_none_or(|w| w.contains(&e.to_bits()))
            })
            .map(|(e, s, _)| (e, s.clone()))
            .collect()
    };
    splines.sort_by_key(|(e, _)| e.to_bits());
    let mut net = NavRoadNetwork::new();
    let mut roads = Vec::with_capacity(splines.len());
    for (i, (entity, spline)) in splines.iter().enumerate() {
        let pose = regia_framework::nav_world::world_pose(world, *entity);
        let mut p = road;
        if road.first_user_id != 0 {
            p.first_user_id = road.first_user_id + (i as u64) * 64;
        }
        let range = net.add_road(spline, &pose, &p).map_err(|e| {
            let mut err = lane_error(&e);
            err.message = format!("entity {}: {}", entity.to_bits(), err.message);
            err
        })?;
        roads.push(json!({ "entity": entity.to_bits(), "lanes": [range.start, range.end] }));
    }
    let set = net.into_set(join).map_err(|e| lane_error(&e))?;
    let graph = set.graph().map_err(|e| lane_error(&e))?;
    let lanes: Vec<Value> = graph
        .lanes()
        .iter()
        .enumerate()
        .take(MAX_LISTED_LANES)
        .map(|(i, l)| {
            json!({
                "lane": i,
                "user_id": l.user_id,
                "length": l.length(),
                "width": l.width,
                "next": l.next,
                "points": l.points.iter().map(|p| json!([p.x, p.y, p.z])).collect::<Vec<_>>(),
            })
        })
        .collect();
    let out = json!({
        "roads": roads,
        "lane_count": graph.len(),
        "total_length": graph.total_length(),
        "lanes": lanes,
        "applied": apply,
    });
    if apply {
        world.remove_resource::<regia_nav::NavLaneCrowd>();
        world.insert_resource::<NavLaneGraph>(graph);
    }
    Ok(out)
}

// -- Per-triangle area paint ------------------------------------------------------------------

fn vec3_of(v: &Value) -> Option<Vec3> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let c = |i: usize| a[i].as_f64().map(|x| x as f32);
    Some(Vec3::new(c(0)?, c(1)?, c(2)?))
}

/// `nav.cook_triangle_areas`: rebuilds the NavTileCache from `triangles` with an area id per
/// triangle (`areas`), painted further by area `volumes` turned into triangle ids. The new cache
/// keeps the old one's settings, links and area costs; its obstacles and area volumes are not
/// carried over (`dropped_obstacles`, `dropped_volumes`).
pub fn cook_triangle_areas(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(old) = world.get_resource::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let items = params
        .get("triangles")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`triangles` (an array of [[x,y,z]x3]) is required"))?;
    if items.len() > MAX_COOK_TRIANGLES {
        return Err(RpcError::invalid_params("at most 1048576 triangles"));
    }
    let mut triangles = Vec::with_capacity(items.len());
    for t in items {
        let tri = t
            .as_array()
            .filter(|a| a.len() == 3)
            .and_then(|a| Some([vec3_of(&a[0])?, vec3_of(&a[1])?, vec3_of(&a[2])?]))
            .ok_or_else(|| RpcError::invalid_params("every triangle is three [x, y, z] points"))?;
        triangles.push(tri);
    }
    let mut areas: Vec<u8> = match params.get("areas") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`areas` must be an array of area ids"))?
            .iter()
            .map(|a| {
                a.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| RpcError::invalid_params("`areas` must hold integers 0..=63"))
            })
            .collect::<Result<_, _>>()?,
    };
    let mut volumes = Vec::new();
    if let Some(list) = params.get("volumes") {
        let list = list
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`volumes` must be an array of { shape..., area }"))?;
        for v in list {
            let shape = crate::nav::shape_param(v)?;
            let bad_area = || RpcError::invalid_params("every volume needs an `area` 0..=63");
            let area = v.get("area").and_then(Value::as_u64).ok_or_else(bad_area)?;
            let area = u8::try_from(area).map_err(|_| bad_area())?;
            let volume = regia_nav::NavAreaVolume::new(shape, area)
                .validated()
                .map_err(|e| tile_cache_error(&e))?;
            volumes.push(volume);
        }
    }
    let params_cache = old.params().clone();
    if !volumes.is_empty() {
        areas =
            regia_nav::paint_triangle_areas(&triangles, &areas, &volumes, params_cache.build.agent.max_climb);
    }
    let links = old.off_mesh_links().to_vec();
    let costs = *old.area_costs();
    let (dropped_obstacles, dropped_volumes) = (old.obstacle_count(), old.area_volumes().count());
    let mut cache = NavTileCache::build_with_triangle_areas(params_cache, &triangles, &areas)
        .map_err(|e| tile_cache_error(&e))?;
    cache.set_off_mesh_links(links);
    cache.set_area_costs(costs);
    let mut counts = std::collections::BTreeMap::<u8, usize>::new();
    for poly in &cache.mesh().polys {
        *counts.entry(poly.area).or_default() += 1;
    }
    let painted = areas.iter().filter(|&&a| a != 0).count();
    let out = json!({
        "triangles": triangles.len(),
        "painted": painted,
        "polys": cache.mesh().poly_count(),
        "poly_areas": counts.iter().map(|(a, n)| json!([a, n])).collect::<Vec<_>>(),
        "revision": cache.revision(),
        "dropped_obstacles": dropped_obstacles,
        "dropped_volumes": dropped_volumes,
    });
    world.insert_resource(cache);
    crate::nav::replan_crowd(world);
    Ok(out)
}

// -- Runtime link and lane files --------------------------------------------------------------

fn no_authored_links() -> RpcError {
    authoring_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_authored_links",
        "NavAuthoredLinksPlugin is not in this app: add regia_nav::NavAuthoredLinksPlugin",
    )
}

fn paths(params: &Value, field: &str) -> Result<Vec<String>, RpcError> {
    match params.get(field) {
        None => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an array of paths")))?
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must hold path strings")))
            })
            .collect(),
    }
}

fn status_json(world: &World) -> Value {
    let files = world
        .get_resource::<regia_nav::NavLinkFiles>()
        .cloned()
        .unwrap_or_default();
    let status = world
        .get_resource::<regia_nav::NavAuthoredLinksStatus>()
        .cloned()
        .unwrap_or_default();
    json!({
        "links": files.links,
        "lanes": files.lanes,
        "file_links": status.file_links,
        "file_lanes": status.file_lanes,
        "anchor_links": status.anchor_links,
        "invalid_anchors": status.invalid_anchors,
        "applied": status.applied,
        "errors": status.errors.iter().map(|e| json!({
            "path": e.path,
            "reason": e.code,
            "message": e.message,
        })).collect::<Vec<_>>(),
    })
}

/// `nav.set_link_files`: the `.navlinks.ron` and `.navlanes.ron` files `NavAuthoredLinksPlugin`
/// loads; they are read on the next frame and their links applied on the next fixed step.
pub fn set_link_files(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world
        .get_resource::<regia_nav::NavAuthoredLinksStatus>()
        .is_none()
    {
        return Err(no_authored_links());
    }
    let files = regia_nav::NavLinkFiles {
        links: paths(params, "links")?,
        lanes: paths(params, "lanes")?,
    };
    world.insert_resource(files);
    Ok(status_json(world))
}

/// `nav.authored_links_status`: the files, what they gave, the scene anchors and the errors.
pub fn authored_links_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if world
        .get_resource::<regia_nav::NavAuthoredLinksStatus>()
        .is_none()
    {
        return Err(no_authored_links());
    }
    Ok(status_json(world))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::{
        crowd_add_agent, crowd_set_target, crowd_status, crowd_step, generate_links, set_links,
    };

    fn reason(e: &RpcError) -> Option<&str> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
    }

    /// Two 5 x 8 m floors with a 2 m gap and a 5 s link over it, as a tile cache.
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
    fn two_agents_queue_for_a_reserved_link() {
        let mut app = islands();
        let w = app.world_mut();
        set_links(
            w,
            &json!({ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "radius": 0.5, "user_id": 9, "duration": 5 } ] }),
        )
        .expect("links");
        let q = crowd_set_link_queue(w, &json!({ "exclusive": true, "timeout_steps": 0 })).expect("sets");
        assert_eq!(q["exclusive"], true);
        for z in [-1.0, 1.0] {
            let a = crowd_add_agent(w, &json!({ "position": [-4, 0, z] })).expect("adds");
            crowd_set_target(w, &json!({ "id": a["id"], "target": [4, 0, z] })).expect("target");
        }
        let mut queued_seen = false;
        for _ in 0..20 {
            let stepped = crowd_step(w, &json!({ "steps": 10 })).expect("steps");
            let events = stepped["link_events"].as_array().cloned().unwrap_or_default();
            queued_seen |= events.iter().any(|e| e["phase"] == "queued");
            let q = crowd_link_queues(w, &json!({})).expect("status");
            if q["queues"].as_array().is_some_and(|a| !a.is_empty()) {
                assert_eq!(q["holders"][0]["user_id"], 9);
                assert_eq!(q["queues"][0]["agents"].as_array().map(Vec::len), Some(1));
                let status = crowd_status(w, &json!({})).expect("status");
                let agents = status["agents"].as_array().cloned().unwrap_or_default();
                assert!(
                    agents
                        .iter()
                        .any(|a| a["state"] == "link_queued" && a["link_wait"]["user_id"] == 9)
                );
                assert_eq!(status["stats"]["link_queue_waits"], 1);
            }
        }
        assert!(queued_seen, "one agent waited for the other");
        let bad = crowd_set_link_queue(w, &json!({})).expect_err("exclusive is required");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let mut bare = App::new();
        let e = crowd_link_queues(bare.world_mut(), &json!({})).expect_err("no crowd");
        assert_eq!(reason(&e), Some("nav_no_crowd"));
    }

    #[test]
    fn road_splines_become_lanes() {
        let mut app = App::new();
        let w = app.world_mut();
        let road = Spline::through([Vec3::ZERO, Vec3::new(20.0, 0.0, 0.0), Vec3::new(40.0, 0.0, 10.0)]);
        w.spawn((
            road.clone(),
            regia_procgen::road::RoadOf::new("roads/main.road.ron"),
            regia_scene::Transform::default(),
        ));
        // A camera path, not a road: left alone unless roads_only is false.
        w.spawn((
            road,
            regia_scene::Transform::from_translation(Vec3::new(0.0, 0.0, 50.0)),
        ));
        let listed = lanes_from_splines(w, &json!({ "width": 7 })).expect("lanes");
        assert_eq!(listed["lane_count"], 2);
        assert_eq!(listed["roads"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["lanes"][0]["width"], 3.5);
        assert!(
            w.get_resource::<NavLaneGraph>().is_none(),
            "listing applies nothing"
        );
        let all = lanes_from_splines(
            w,
            &json!({ "roads_only": false, "two_way": false, "apply": true }),
        )
        .expect("lanes");
        assert_eq!(
            (all["lane_count"].as_u64(), all["applied"].as_bool()),
            (Some(2), Some(true))
        );
        assert_eq!(w.resource::<NavLaneGraph>().len(), 2);
        let bad = lanes_from_splines(w, &json!({ "width": 0 })).expect_err("bad width");
        assert_eq!(reason(&bad), Some("nav_invalid_lane_config"));
        let bad = lanes_from_splines(w, &json!({ "lanes_each_way": 9 })).expect_err("too many lanes");
        assert_eq!(reason(&bad), Some("nav_invalid_lane_config"));
    }

    #[test]
    fn triangle_areas_paint_the_rebuilt_cache() {
        let mut app = islands();
        let w = app.world_mut();
        let floor = json!([
            [[-8, 0, -8], [8, 0, -8], [8, 0, 8]],
            [[-8, 0, -8], [8, 0, 8], [-8, 0, 8]]
        ]);
        let out = cook_triangle_areas(w, &json!({ "triangles": floor, "areas": [3, 0] })).expect("cooks");
        assert_eq!(out["painted"], 1);
        let areas = out["poly_areas"].as_array().cloned().unwrap_or_default();
        assert!(areas.iter().any(|a| a[0] == 3), "{areas:?}");
        let by_volume = cook_triangle_areas(
            w,
            &json!({ "triangles": floor, "volumes": [ { "shape": "box", "min": [-9, -1, -9], "max": [9, 1, 9], "area": 5 } ] }),
        )
        .expect("cooks");
        assert_eq!(by_volume["painted"], 2);
        let e = cook_triangle_areas(w, &json!({ "triangles": floor, "areas": [1] })).expect_err("short list");
        assert_eq!(reason(&e), Some("nav_invalid_build_input"));
        let mut bare = App::new();
        let e = cook_triangle_areas(bare.world_mut(), &json!({ "triangles": floor })).expect_err("no cache");
        assert_eq!(reason(&e), Some("nav_no_tile_cache"));
    }

    fn assert_rejected_volume_preserves_cache(volume: Value) {
        let mut app = islands();
        let w = app.world_mut();
        let before = {
            let mut cache = w.resource_mut::<NavTileCache>();
            cache
                .add_obstacle(regia_nav::NavObstacleShape::Box {
                    min: Vec3::new(-5.0, -1.0, -1.0),
                    max: Vec3::new(-3.0, 1.0, 1.0),
                })
                .expect("obstacle");
            cache.update();
            assert_eq!(cache.obstacle_count(), 1);
            assert!(cache.revision() > 0);
            (
                cache.obstacles().collect::<Vec<_>>(),
                cache.revision(),
                cache.total_rebuilds(),
            )
        };
        let result = cook_triangle_areas(
            w,
            &json!({
                "triangles": [
                    [[-8, 0, -8], [8, 0, -8], [8, 0, 8]],
                    [[-8, 0, -8], [8, 0, 8], [-8, 0, 8]]
                ],
                // A valid volume before the bad one must not be committed either.
                "volumes": [
                    { "shape": "box", "min": [-9, -1, -9], "max": [9, 1, 9], "area": 5 },
                    volume
                ]
            }),
        );
        let cache = w.resource::<NavTileCache>();
        assert_eq!(
            (
                cache.obstacles().collect::<Vec<_>>(),
                cache.revision(),
                cache.total_rebuilds()
            ),
            before,
            "a rejected volume replaced the existing cache: {result:?}"
        );
        let err = result.expect_err("invalid volume");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
    }

    #[test]
    fn invalid_paint_area_preserves_the_existing_cache() {
        for area in [64, 255, 256] {
            assert_rejected_volume_preserves_cache(json!({
                "shape": "box", "min": [-9, -1, -9], "max": [9, 1, 9], "area": area
            }));
        }
    }

    #[test]
    fn invalid_paint_shape_preserves_the_existing_cache() {
        for volume in [
            json!({ "shape": "cylinder", "base": [0, 0, 0], "height": 2, "area": 3 }),
            json!({ "shape": "cylinder", "base": [0, 0, 0], "radius": 2, "area": 3 }),
            json!({ "shape": "cylinder", "base": [0, 0, 0], "radius": 0, "height": 2, "area": 3 }),
            json!({ "shape": "box", "min": [0, 0, 0], "max": [0, 1, 2], "area": 3 }),
            json!({ "shape": "oriented_box", "center": [0, 0, 0], "half_extents": [1, -1, 1], "area": 3 }),
        ] {
            assert_rejected_volume_preserves_cache(volume);
        }
    }

    #[test]
    fn link_files_need_the_plugin_and_report_load_errors() {
        let mut bare = islands();
        let e = set_link_files(bare.world_mut(), &json!({ "links": [] })).expect_err("no plugin");
        assert_eq!(reason(&e), Some("nav_no_authored_links"));
        assert!(authored_links_status(bare.world_mut(), &json!({})).is_err());
        let mut app = islands();
        app.add_plugins(regia_nav::NavAuthoredLinksPlugin);
        let dir = regia_test_utils::unique_temp_dir("nav-agent-files");
        let missing = dir.join("none.navlinks.ron").display().to_string();
        set_link_files(app.world_mut(), &json!({ "links": [missing] })).expect("sets");
        app.world_mut().run_schedule(PreUpdate);
        let status = authored_links_status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(status["errors"][0]["reason"], "nav_link_io");
        assert_eq!(status["file_links"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generate_links_takes_a_level_clearance() {
        let mut app = islands();
        let w = app.world_mut();
        let level = generate_links(w, &json!({ "clearance": "level" })).expect("generates");
        let plain = generate_links(w, &json!({})).expect("generates");
        // No colliders in this world: the level test rejects nothing.
        assert_eq!(level["count"], plain["count"]);
        assert_eq!(
            (level["clearance"].as_str(), level["rejected"].as_u64()),
            (Some("level"), Some(0))
        );
        assert!(generate_links(w, &json!({ "clearance": "walls" })).is_err());
    }
}
