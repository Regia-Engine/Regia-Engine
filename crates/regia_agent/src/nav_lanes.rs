// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of polygon areas (`nav.area_*`) and crowd lane graphs (`nav.lane*`):
//! ADR-1934, `book/agents/navigation-lanes.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    NAV_LANE_WALK, NavAreaCosts, NavAreaVolume, NavAreaVolumeId, NavLaneAgent, NavLaneAgentId,
    NavLaneAgentParams, NavLaneCrowd, NavLaneCrowdConfig, NavLaneDef, NavLaneError, NavLaneGraph, NavLaneSet,
    NavQueryFilter, NavTileCache,
};
use serde_json::{Value, json};

use crate::nav::{
    f32_param, link_mesh, nav_error, no_tile_cache, replan_crowd, shape_json, shape_param, tile_cache_error,
    update_json, update_param, vec3_param,
};
use crate::protocol::RpcError;

/// The most lane steps one `nav.lane_step` call runs.
const MAX_LANE_STEPS_PER_CALL: u64 = 3600;

/// The most agents one `nav.lane_agents` call lists.
const MAX_LANE_AGENT_PAGE: u64 = 4096;

/// The most transitions one `nav.lane_step` call lists.
const MAX_LISTED_TRANSITIONS: usize = 256;

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn u64_param(params: &Value, field: &str) -> Result<u64, RpcError> {
    params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (a non-negative integer) is required")))
}

fn u32_param(params: &Value, field: &str) -> Result<u32, RpcError> {
    u32::try_from(u64_param(params, field)?)
        .map_err(|_| RpcError::invalid_params(format!("`{field}` must fit in 32 bits")))
}

fn optional_u64(params: &Value, field: &str, default: u64) -> Result<u64, RpcError> {
    match params.get(field) {
        None => Ok(default),
        Some(_) => u64_param(params, field),
    }
}

fn u16_of(value: &Value, field: &str) -> Result<u16, RpcError> {
    value
        .as_u64()
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an integer in 0..=65535")))
}

fn area_error(e: &regia_nav::NavAreaError) -> RpcError {
    nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string())
}

/// An area id of a request: errors out of range say so with the protocol's reason.
fn area_of(value: Option<&Value>, field: &str) -> Result<u8, RpcError> {
    let n = value
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (an area id) is required")))?;
    match u8::try_from(n) {
        Ok(area) if (area as usize) < regia_nav::MAX_NAV_AREAS => Ok(area),
        _ => {
            let shown = u32::try_from(n).unwrap_or(u32::MAX);
            Err(area_error(&regia_nav::NavAreaError::InvalidArea(shown)))
        }
    }
}

/// The optional `filter` object of `nav.find_path`, `nav.crowd_add_agent` and
/// `nav.crowd_set_filter`: `include_flags`, `exclude_flags`, `area_costs` (`[[area, scale]...]`)
/// and `exclude_areas`.
pub(crate) fn filter_param(params: &Value) -> Result<Option<NavQueryFilter>, RpcError> {
    let Some(object) = params.get("filter") else {
        return Ok(None);
    };
    if !object.is_object() {
        return Err(RpcError::invalid_params("`filter` must be an object"));
    }
    let mut filter = NavQueryFilter::default();
    if let Some(v) = object.get("include_flags") {
        filter.include_flags = u16_of(v, "include_flags")?;
    }
    if let Some(v) = object.get("exclude_flags") {
        filter.exclude_flags = u16_of(v, "exclude_flags")?;
    }
    if let Some(v) = object.get("area_costs") {
        let invalid = || RpcError::invalid_params("`area_costs` must be [[area, scale]...]");
        for pair in v.as_array().ok_or_else(invalid)? {
            let pair = pair.as_array().filter(|p| p.len() == 2).ok_or_else(invalid)?;
            let area = area_of(pair.first(), "area_costs")?;
            let scale = pair.get(1).and_then(Value::as_f64).ok_or_else(invalid)? as f32;
            filter = filter.with_area_cost(area, scale).map_err(|e| area_error(&e))?;
        }
    }
    if let Some(v) = object.get("exclude_areas") {
        let invalid = || RpcError::invalid_params("`exclude_areas` must be an array of area ids");
        for area in v.as_array().ok_or_else(invalid)? {
            let area = area_of(Some(area), "exclude_areas")?;
            filter = filter.without_area(area).map_err(|e| area_error(&e))?;
        }
    }
    Ok(Some(filter))
}

fn costs_json(costs: &NavAreaCosts) -> Value {
    costs
        .changed()
        .map(|(area, cost)| json!({ "area": area, "cost": cost }))
        .collect()
}

/// `nav.area_status`: the cost table, how many polygons each area has, and the cache's volumes.
pub fn area_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mesh = link_mesh(world)?;
    let mut polys = [0u64; regia_nav::MAX_NAV_AREAS];
    for p in &mesh.polys {
        if let Some(slot) = polys.get_mut(p.area as usize) {
            *slot += 1;
        }
    }
    let by_area: Vec<Value> = polys
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(area, n)| json!({ "area": area, "polys": n }))
        .collect();
    let volumes: Vec<Value> = world
        .get_resource::<NavTileCache>()
        .map(|cache| {
            cache
                .area_volumes()
                .map(|(id, v)| json!({ "id": id.0, "area": v.area, "shape": shape_json(&v.shape) }))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "costs": costs_json(mesh.area_costs()), "polys_by_area": by_area, "volumes": volumes }))
}

/// `nav.set_area_cost`: sets what one area costs per metre on the live mesh; crowds replan.
pub fn set_area_cost(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let area = area_of(params.get("area"), "area")?;
    let cost = f32_param(params, "cost", f32::NAN)?;
    let mut costs = *link_mesh(world)?.area_costs();
    costs.set(area, cost).map_err(|e| area_error(&e))?;
    if let Some(mut cache) = world.get_resource_mut::<NavTileCache>() {
        cache.set_area_costs(costs);
    } else if let Some(mut mesh) = world.get_resource_mut::<regia_nav::NavMesh>() {
        mesh.set_area_costs(costs);
    }
    replan_crowd(world);
    Ok(json!({ "costs": costs_json(&costs) }))
}

fn area_table_json(table: &regia_framework::nav_world::NavAreaCostTable) -> Value {
    table
        .areas
        .iter()
        .map(|row| json!({ "area": row.area, "cost": row.cost, "name": row.name }))
        .collect()
}

/// `nav.area_table`: the level's authored area costs (`NavAreaCostTable`, ADR-2133), or null.
pub fn area_table(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let table = world.get_resource::<regia_framework::nav_world::NavAreaCostTable>();
    Ok(json!({ "areas": table.map(area_table_json) }))
}

/// `nav.set_area_table`: replaces the level's authored area costs (a level resource: the editor
/// saves it with the level) and applies them to the live mesh now; crowds replan.
pub fn set_area_table(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::nav_world::{NavAreaCostEntry, NavAreaCostTable};
    let invalid = || RpcError::invalid_params("`areas` must be an array of { area, cost, name? }");
    let items = params
        .get("areas")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if items.len() > regia_nav::MAX_NAV_AREAS * 4 {
        return Err(RpcError::invalid_params("`areas` holds at most 256 rows"));
    }
    let mut table = NavAreaCostTable::default();
    for item in items {
        let area = area_of(item.get("area"), "area")?;
        let cost = f32_param(item, "cost", f32::NAN)?;
        let name = match item.get("name") {
            None | Some(Value::Null) => String::new(),
            Some(v) => v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("an area's `name` must be a string"))?
                .to_owned(),
        };
        table.areas.push(NavAreaCostEntry { area, cost, name });
    }
    let costs = table.costs().map_err(|e| area_error(&e))?;
    let out = json!({ "areas": area_table_json(&table), "costs": costs_json(&costs) });
    world.insert_resource(table);
    if let Some(mut cache) = world.get_resource_mut::<NavTileCache>() {
        cache.set_area_costs(costs);
    } else if let Some(mut mesh) = world.get_resource_mut::<regia_nav::NavMesh>() {
        mesh.set_area_costs(costs);
    }
    replan_crowd(world);
    Ok(out)
}

/// `nav.add_area_volume`: paints an area over a shape; rebuilds the tiles unless `update` is false.
pub fn add_area_volume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let shape = shape_param(params)?;
    let area = area_of(params.get("area"), "area")?;
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let id = cache
        .add_area_volume(NavAreaVolume::new(shape, area))
        .map_err(|e| tile_cache_error(&e))?;
    let mut out = update_json(&mut cache, update);
    out["id"] = json!(id.0);
    out["revision"] = json!(cache.revision());
    Ok(out)
}

/// `nav.remove_area_volume`: removes a volume by id.
pub fn remove_area_volume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = NavAreaVolumeId(u64_param(params, "id")?);
    let update = update_param(params)?;
    let Some(mut cache) = world.get_resource_mut::<NavTileCache>() else {
        return Err(no_tile_cache());
    };
    let volume = cache.remove_area_volume(id).map_err(|e| match e {
        regia_nav::NavTileCacheError::UnknownAreaVolume(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => tile_cache_error(&e),
    })?;
    let mut out = update_json(&mut cache, update);
    out["area"] = json!(volume.area);
    out["revision"] = json!(cache.revision());
    Ok(out)
}

fn lane_error(e: &NavLaneError) -> RpcError {
    match e {
        NavLaneError::UnknownLane(_) | NavLaneError::UnknownAgent(_) => {
            nav_error(crate::protocol::NOT_FOUND, "F0003", e.code(), e.to_string())
        }
        _ => nav_error(crate::protocol::INVALID_PARAMS, "F0002", e.code(), e.to_string()),
    }
}

fn no_graph() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_lane_graph",
        "no NavLaneGraph in this app: send one with nav.lanes_set or insert the resource",
    )
}

fn no_lane_crowd() -> RpcError {
    nav_error(
        crate::protocol::NOT_FOUND,
        "F0003",
        "nav_no_lane_crowd",
        "no NavLaneCrowd in this app: nav.lane_add_agent makes one, or insert the resource",
    )
}

fn graph_ref(world: &World) -> Result<&NavLaneGraph, RpcError> {
    world.get_resource::<NavLaneGraph>().ok_or_else(no_graph)
}

fn lane_def(value: &Value) -> Result<NavLaneDef, RpcError> {
    let invalid = |what: &str| RpcError::invalid_params(format!("a lane's `{what}` is invalid"));
    let items = value
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("points"))?;
    let mut points = Vec::with_capacity(items.len());
    for item in items {
        let c = item
            .as_array()
            .filter(|c| c.len() == 3)
            .ok_or_else(|| invalid("points"))?;
        let n = |i: usize| c[i].as_f64().map(|f| f as f32).ok_or_else(|| invalid("points"));
        points.push(Vec3::new(n(0)?, n(1)?, n(2)?));
    }
    let mut def = NavLaneDef::new(&points);
    def.width = f32_param(value, "width", def.width)?;
    def.speed = f32_param(value, "speed", def.speed)?;
    def.user_id = optional_u64(value, "user_id", 0)?;
    if let Some(flags) = value.get("flags") {
        def.flags = u16_of(flags, "flags")?;
    }
    if let Some(next) = value.get("next") {
        let list = next.as_array().ok_or_else(|| invalid("next"))?;
        def.next = list
            .iter()
            .map(|n| {
                n.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("next"))
            })
            .collect::<Result<_, _>>()?;
    }
    def.weight = f32_param(value, "weight", def.weight)?;
    match value.get("opposite") {
        None | Some(Value::Null) => {}
        Some(v) => {
            def.opposite = Some(
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("opposite"))?,
            );
        }
    }
    Ok(def)
}

/// `nav.lanes_set`: replaces the lane graph, from `lanes` or a `.navlanes.ron` text. Agents on the
/// old graph are dropped: lane indices do not carry over.
pub fn lanes_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let graph = match (params.get("lanes"), crate::params::opt_str(params, "ron")?) {
        (Some(items), None) => {
            let items = items
                .as_array()
                .ok_or_else(|| RpcError::invalid_params("`lanes` must be an array of lane objects"))?;
            let defs = items.iter().map(lane_def).collect::<Result<Vec<_>, _>>()?;
            let join = f32_param(params, "join_distance", 0.0)?;
            NavLaneGraph::build(&defs, join).map_err(|e| lane_error(&e))?
        }
        (None, Some(text)) => NavLaneSet::parse(text)
            .and_then(|set| set.graph())
            .map_err(|e| lane_error(&e))?,
        _ => {
            return Err(RpcError::invalid_params(
                "give either `lanes` (an array of lane objects) or `ron` (a .navlanes.ron text)",
            ));
        }
    };
    let dropped = world
        .remove_resource::<NavLaneCrowd>()
        .map_or(0, |crowd| crowd.len());
    let out = json!({
        "lanes": graph.len(),
        "total_length": graph.total_length(),
        "agents_dropped": dropped,
    });
    world.insert_resource(graph);
    Ok(out)
}

fn lane_json(index: usize, lane: &regia_nav::NavLane) -> Value {
    json!({
        "lane": index,
        "user_id": lane.user_id,
        "length": lane.length(),
        "width": lane.width,
        "speed": lane.speed,
        "flags": lane.flags,
        "next": lane.next,
        "enabled": lane.enabled,
        "weight": lane.weight,
        "opposite": lane.opposite,
    })
}

/// `nav.lanes_status`: the graph and, when there is one, the crowd's counters.
pub fn lanes_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let graph = graph_ref(world)?;
    let lanes: Vec<Value> = graph
        .lanes()
        .iter()
        .enumerate()
        .map(|(i, l)| lane_json(i, l))
        .collect();
    let crowd = world.get_resource::<NavLaneCrowd>().map(|c| {
        let s = c.stats();
        json!({
            "agents": c.len(),
            "dormant": c.dormant_count(),
            "steps": s.steps,
            "transitions": s.transitions,
            "slowed": s.slowed,
            "oncoming_yields": s.oncoming_yields,
            "stopped": s.stopped,
            "dormant_entries": s.dormant_entries,
            "step_seconds": c.config().step_seconds,
            "digest": format!("{:016x}", c.digest()),
        })
    });
    Ok(json!({
        "format_version": regia_nav::NAV_LANES_FORMAT_VERSION,
        "total_length": graph.total_length(),
        "lanes": lanes,
        "crowd": crowd,
    }))
}

/// `nav.lane_route`: the quickest route between two lanes, or `null`.
pub fn lane_route(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (from, to) = (u32_param(params, "from")?, u32_param(params, "to")?);
    let flags = match params.get("flags") {
        None => u16::MAX,
        Some(v) => u16_of(v, "flags")?,
    };
    let offset = f32_param(params, "offset", 0.0)?;
    let graph = graph_ref(world)?;
    match graph.find_route(from, to, flags) {
        Ok(route) => Ok(json!({
            "route": {
                "points": graph
                    .route_points(&route, offset)
                    .into_iter()
                    .map(vec3_json)
                    .collect::<Vec<_>>(),
                "lanes": route.lanes,
                "length": route.length,
            }
        })),
        Err(NavLaneError::NoRoute { .. }) => Ok(json!({ "route": null })),
        Err(e) => Err(lane_error(&e)),
    }
}

/// `nav.lane_nearest`: the lane nearest to a point.
pub fn lane_nearest(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point = vec3_param(params, "point")?;
    let reach = f32_param(params, "reach", 2.0)?;
    let flags = match params.get("flags") {
        None => u16::MAX,
        Some(v) => u16_of(v, "flags")?,
    };
    Ok(match graph_ref(world)?.nearest(point, reach, flags) {
        Some((lane, distance, away)) => json!({ "lane": lane, "distance": distance, "away": away }),
        None => json!({ "lane": null }),
    })
}

/// `nav.lane_check`: the lane points with no navmesh under them.
pub fn lane_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let reach = f32_param(params, "reach", 1.0)?;
    let graph = graph_ref(world)?;
    let gaps: Vec<Value> = graph
        .check_on_mesh(link_mesh(world)?, reach)
        .into_iter()
        .map(|g| json!({ "lane": g.lane, "point": g.point, "position": vec3_json(g.position) }))
        .collect();
    Ok(json!({ "on_mesh": gaps.is_empty(), "gaps": gaps }))
}

/// `nav.lane_set_enabled`: opens or closes every lane with a user id.
pub fn lane_set_enabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let user_id = u64_param(params, "user_id")?;
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`enabled` (true or false) is required"))?;
    let Some(mut graph) = world.get_resource_mut::<NavLaneGraph>() else {
        return Err(no_graph());
    };
    Ok(json!({ "changed": graph.set_lane_enabled(user_id, enabled) }))
}

fn lane_agent_json(world: &World, a: &NavLaneAgent) -> Value {
    let position = world
        .get_resource::<NavLaneGraph>()
        .and_then(|g| g.lane(a.lane()).map(|lane| (lane, a)))
        .map(|(lane, a)| {
            let (p, dir) = lane.sample(a.distance());
            vec3_json(p + Vec3::new(-dir.z, 0.0, dir.x).normalize_or_zero() * a.offset())
        });
    json!({
        "id": a.id().0,
        "state": a.state().name(),
        "lane": a.lane(),
        "distance": a.distance(),
        "offset": a.offset(),
        "speed": a.speed(),
        "route": a.route(),
        "has_destination": a.has_destination(),
        "position": position,
    })
}

/// `nav.lane_add_agent`: adds an agent to a lane (creating the crowd on first use).
pub fn lane_add_agent(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let lane = u32_param(params, "lane")?;
    let d = NavLaneAgentParams::default();
    let agent = NavLaneAgentParams {
        radius: f32_param(params, "radius", d.radius)?,
        speed: f32_param(params, "speed", d.speed)?,
        side_speed: f32_param(params, "side_speed", d.side_speed)?,
        flags: match params.get("flags") {
            None => NAV_LANE_WALK,
            Some(v) => u16_of(v, "flags")?,
        },
        prefer_flags: match params.get("prefer_flags") {
            None => 0,
            Some(v) => u16_of(v, "prefer_flags")?,
        },
    };
    let distance = f32_param(params, "distance", 0.0)?;
    let offset = f32_param(params, "offset", 0.0)?;
    let seed = optional_u64(params, "seed", 0)?;
    graph_ref(world)?;
    if world.get_resource::<NavLaneCrowd>().is_none() {
        let crowd = NavLaneCrowd::new(NavLaneCrowdConfig::default()).map_err(|e| lane_error(&e))?;
        world.insert_resource(crowd);
    }
    let id = world.resource_scope(|world, mut crowd: Mut<NavLaneCrowd>| {
        crowd
            .add_agent(
                world.resource::<NavLaneGraph>(),
                lane,
                distance,
                offset,
                agent,
                seed,
            )
            .map_err(|e| lane_error(&e))
    })?;
    let crowd = world.resource::<NavLaneCrowd>();
    let a = crowd.agent(id).map(|a| lane_agent_json(world, a));
    Ok(json!({ "id": id.0, "agent": a, "agents": crowd.len() }))
}

/// `nav.lane_remove_agent`: removes an agent.
pub fn lane_remove_agent(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = NavLaneAgentId(u64_param(params, "id")?);
    let Some(mut crowd) = world.get_resource_mut::<NavLaneCrowd>() else {
        return Err(no_lane_crowd());
    };
    crowd.remove_agent(id).map_err(|e| lane_error(&e))?;
    Ok(json!({ "agents": crowd.len() }))
}

/// `nav.lane_set_destination`: sends an agent to a lane, or back to wandering with `lane: null`.
pub fn lane_set_destination(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = NavLaneAgentId(u64_param(params, "id")?);
    let lane = match params.get("lane") {
        Some(Value::Null) => None,
        _ => Some(u32_param(params, "lane")?),
    };
    graph_ref(world)?;
    if world.get_resource::<NavLaneCrowd>().is_none() {
        return Err(no_lane_crowd());
    }
    world.resource_scope(|world, mut crowd: Mut<NavLaneCrowd>| {
        match lane {
            Some(lane) => crowd.set_destination(world.resource::<NavLaneGraph>(), id, lane),
            None => crowd.clear_destination(id),
        }
        .map_err(|e| lane_error(&e))
    })?;
    let crowd = world.resource::<NavLaneCrowd>();
    Ok(json!({ "agent": crowd.agent(id).map(|a| lane_agent_json(world, a)) }))
}

/// `nav.lane_step`: advances the lane crowd by `steps` fixed steps (default 1).
pub fn lane_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let steps = match params.get("steps") {
        None => 1,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=MAX_LANE_STEPS_PER_CALL).contains(n))
            .ok_or_else(|| RpcError::invalid_params("`steps` must be an integer in 1..=3600"))?,
    };
    graph_ref(world)?;
    if world.get_resource::<NavLaneCrowd>().is_none() {
        return Err(no_lane_crowd());
    }
    if let Some(mut cache) = world.get_resource_mut::<NavTileCache>() {
        cache.update();
    }
    world.resource_scope(|world, mut crowd: Mut<NavLaneCrowd>| {
        let graph = world.resource::<NavLaneGraph>();
        let cache = world.get_resource::<NavTileCache>();
        let mut listed = Vec::new();
        let mut total = 0usize;
        for _ in 0..steps {
            if let Some(cache) = cache {
                crowd.sync_streaming(graph, cache);
            }
            crowd.step(graph);
            total += crowd.transitions().len();
            for t in crowd.transitions() {
                if listed.len() < MAX_LISTED_TRANSITIONS {
                    listed.push(json!({ "agent": t.agent.0, "from": t.from, "to": t.to }));
                }
            }
        }
        Ok(json!({
            "steps": steps,
            "total_steps": crowd.stats().steps,
            "agents": crowd.len(),
            "dormant": crowd.dormant_count(),
            "transitions": listed,
            "transitions_total": total,
            "digest": format!("{:016x}", crowd.digest()),
        }))
    })
}

/// `nav.lane_agents`: one page of agents, in id order.
pub fn lane_agents(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let offset = optional_u64(params, "offset", 0)?;
    let limit = optional_u64(params, "limit", 256)?;
    if limit == 0 || limit > MAX_LANE_AGENT_PAGE {
        return Err(RpcError::invalid_params("`limit` must be an integer in 1..=4096"));
    }
    let Some(crowd) = world.get_resource::<NavLaneCrowd>() else {
        return Err(no_lane_crowd());
    };
    let page: Vec<Value> = crowd
        .agents()
        .iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(limit as usize)
        .map(|a| lane_agent_json(world, a))
        .collect();
    Ok(json!({ "total": crowd.len(), "agents": page }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::nav::{
        crowd_add_agent, crowd_set_filter, crowd_set_target, crowd_status, crowd_step, find_path,
    };

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

    fn params() -> regia_nav::NavMeshBuildParams {
        let mut p = regia_nav::NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        };
        p.agent.radius = 0.0;
        p
    }

    fn cache_app() -> App {
        let mut app = App::new();
        let cache = NavTileCache::build(
            regia_nav::NavTileCacheParams {
                build: params(),
                tile_size: 16,
            },
            &floor(16.0),
        )
        .expect("builds");
        app.insert_resource(cache);
        app
    }

    fn pit() -> Value {
        json!({ "shape": "box", "min": [-6, -1, -8], "max": [6, 1, 8], "area": 3 })
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn crosses(app: &App, path: &Value, area: u8) -> bool {
        let mesh = app.world().resource::<NavTileCache>().mesh();
        path["polys"]
            .as_array()
            .expect("polys")
            .iter()
            .filter_map(Value::as_u64)
            .any(|p| mesh.polys[p as usize].area == area)
    }

    #[test]
    fn an_area_volume_and_a_cost_move_a_path_through_the_protocol() {
        let mut app = cache_app();
        let world = app.world_mut();
        let added = add_area_volume(world, &pit()).expect("valid volume");
        assert_eq!(added["id"], 0);
        assert!(!added["rebuilt"].as_array().expect("list").is_empty());
        let status = area_status(world, &json!({})).expect("status");
        assert_eq!(status["volumes"][0]["area"], 3);
        assert_eq!(status["volumes"][0]["shape"]["shape"], "box");
        assert!(status["polys_by_area"].as_array().expect("list").len() >= 2);

        let trip = json!({ "from": [-12, 0, 0], "to": [12, 0, 0] });
        let straight = find_path(world, &trip).expect("valid")["path"].clone();
        assert!(crosses(&app, &straight, 3));
        let world = app.world_mut();
        let set = set_area_cost(world, &json!({ "area": 3, "cost": 8.0 })).expect("valid cost");
        assert_eq!(set["costs"][0]["cost"], 8.0);
        let around = find_path(world, &trip).expect("valid")["path"].clone();
        assert!(!crosses(&app, &around, 3), "{around}");
        let world = app.world_mut();
        let cheap = json!({
            "from": [-12, 0, 0],
            "to": [12, 0, 0],
            "filter": { "area_costs": [[3, 0.125]] },
        });
        let filtered = find_path(world, &cheap).expect("valid")["path"].clone();
        assert!(
            crosses(&app, &filtered, 3),
            "the filter scales the cost back to 1"
        );

        let world = app.world_mut();
        let removed = remove_area_volume(world, &json!({ "id": 0 })).expect("known volume");
        assert_eq!(removed["area"], 3);
        let status = area_status(world, &json!({})).expect("status");
        assert_eq!(status["volumes"].as_array().expect("list").len(), 0);
    }

    #[test]
    fn an_agent_with_a_filter_walks_around_what_it_leaves_out() {
        let mut app = cache_app();
        let world = app.world_mut();
        add_area_volume(world, &pit()).expect("valid volume");
        let id = crowd_add_agent(
            world,
            &json!({ "position": [-12, 0, 0], "filter": { "exclude_areas": [3] } }),
        )
        .expect("on the mesh")["id"]
            .clone();
        crowd_set_target(world, &json!({ "id": id, "target": [12, 0, 0] })).expect("on the mesh");
        crowd_step(world, &json!({ "steps": 1800 })).expect("steps");
        let status = crowd_status(world, &json!({})).expect("status");
        assert_eq!(status["agents"][0]["state"], "arrived");
        assert_eq!(status["stats"]["dormant_entries"], 0);
        let changed = crowd_set_filter(world, &json!({ "id": id, "filter": {} })).expect("known agent");
        assert_eq!(changed["agent"]["id"], id);
        let e = crowd_set_filter(world, &json!({ "id": 99, "filter": {} })).expect_err("unknown agent");
        assert_eq!(reason(&e), "nav_unknown_agent");
    }

    #[test]
    fn area_errors_carry_reasons() {
        let mut app = App::new();
        let world = app.world_mut();
        let e = area_status(world, &json!({})).expect_err("no mesh");
        assert_eq!(reason(&e), "nav_no_mesh");
        let e = add_area_volume(world, &pit()).expect_err("no cache");
        assert_eq!(reason(&e), "nav_no_tile_cache");

        let mut app = cache_app();
        let world = app.world_mut();
        let e = set_area_cost(world, &json!({ "area": 64, "cost": 2.0 })).expect_err("bad id");
        assert_eq!(reason(&e), "nav_invalid_area");
        let e = set_area_cost(world, &json!({ "area": 2, "cost": 0.0 })).expect_err("bad cost");
        assert_eq!(reason(&e), "nav_invalid_area_cost");
        let e = set_area_cost(world, &json!({ "area": 2 })).expect_err("no cost");
        assert_eq!(reason(&e), "nav_invalid_area_cost");
        let e = remove_area_volume(world, &json!({ "id": 9 })).expect_err("unknown volume");
        assert_eq!(reason(&e), "nav_unknown_area_volume");
        let bad = json!({ "from": [0, 0, 0], "to": [1, 0, 0], "filter": { "exclude_areas": [70] } });
        let e = find_path(world, &bad).expect_err("bad id");
        assert_eq!(reason(&e), "nav_invalid_area");
        let bad = json!({ "from": [0, 0, 0], "to": [1, 0, 0], "filter": 1 });
        assert!(find_path(world, &bad).is_err());
    }

    #[test]
    fn the_area_table_is_read_set_and_applied_through_the_protocol() {
        let mut app = cache_app();
        let world = app.world_mut();
        assert_eq!(
            area_table(world, &json!({})).expect("reads")["areas"],
            Value::Null
        );
        let set = set_area_table(
            world,
            &json!({ "areas": [
                { "area": 3, "cost": 4.0, "name": "mud" },
                { "area": 5, "cost": 0.5 },
                { "area": 3, "cost": 6.0, "name": "deep mud" }
            ] }),
        )
        .expect("valid table");
        assert_eq!(set["areas"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            set["costs"],
            json!([{ "area": 3, "cost": 6.0 }, { "area": 5, "cost": 0.5 }])
        );
        let live = world.resource::<NavTileCache>().area_costs();
        assert_eq!((live.get(3), live.get(5), live.get(0)), (6.0, 0.5, 1.0));
        let read = area_table(world, &json!({})).expect("reads");
        assert_eq!(read["areas"][0]["name"], "mud");
        assert_eq!(read["areas"][2]["cost"], 6.0);

        let e =
            set_area_table(world, &json!({ "areas": [{ "area": 64, "cost": 2.0 }] })).expect_err("bad id");
        assert_eq!(reason(&e), "nav_invalid_area");
        let e =
            set_area_table(world, &json!({ "areas": [{ "area": 2, "cost": -1.0 }] })).expect_err("bad cost");
        assert_eq!(reason(&e), "nav_invalid_area_cost");
        assert!(set_area_table(world, &json!({ "areas": 3 })).is_err());
        assert!(
            set_area_table(
                world,
                &json!({ "areas": [{ "area": 2, "cost": 2.0, "name": 7 }] })
            )
            .is_err()
        );
        // A refused table changes nothing.
        assert_eq!(world.resource::<NavTileCache>().area_costs().get(3), 6.0);
    }

    #[test]
    fn lane_weights_pairs_and_preferences_go_through_the_protocol() {
        let mut app = cache_app();
        let world = app.world_mut();
        let lanes = json!({ "lanes": [
            { "points": [[-10, 0, 0], [10, 0, 0]], "width": 3.0, "opposite": 1, "weight": 2.0 },
            { "points": [[10, 0, 0], [-10, 0, 0]], "width": 3.0 }
        ] });
        lanes_set(world, &lanes).expect("valid lanes");
        let status = lanes_status(world, &json!({})).expect("status");
        assert_eq!(status["format_version"], 2);
        assert_eq!(status["lanes"][0]["opposite"], 1);
        assert_eq!(status["lanes"][1]["opposite"], 0, "pairing is mutual");
        assert_eq!(status["lanes"][0]["weight"], 2.0);
        lane_add_agent(world, &json!({ "lane": 0, "prefer_flags": 4 })).expect("valid agent");
        lane_add_agent(world, &json!({ "lane": 1 })).expect("valid agent");
        lane_step(world, &json!({ "steps": 300 })).expect("steps");
        let status = lanes_status(world, &json!({})).expect("status");
        assert!(
            status["crowd"]["oncoming_yields"].as_u64().unwrap_or(0) > 0,
            "{status}"
        );

        let bad = json!({ "lanes": [{ "points": [[0, 0, 0], [1, 0, 0]], "opposite": 0 }] });
        assert_eq!(
            reason(&lanes_set(world, &bad).expect_err("itself")),
            "nav_invalid_lane"
        );
        let bad = json!({ "lanes": [{ "points": [[0, 0, 0], [1, 0, 0]], "weight": 0.0 }] });
        assert_eq!(
            reason(&lanes_set(world, &bad).expect_err("weight")),
            "nav_invalid_lane"
        );
        let bad = json!({ "lanes": [{ "points": [[0, 0, 0], [1, 0, 0]], "opposite": "x" }] });
        assert!(lanes_set(world, &bad).is_err());
        assert!(lane_add_agent(world, &json!({ "lane": 0, "prefer_flags": 70000 })).is_err());
    }

    #[test]
    fn a_streamed_target_waits_at_an_unloaded_edge_through_the_protocol() {
        let mut app = App::new();
        let whole = NavTileCache::build(
            regia_nav::NavTileCacheParams {
                build: params(),
                tile_size: 16,
            },
            &floor(16.0),
        )
        .expect("builds");
        let mut cache = NavTileCache::streamed(whole.params().clone(), whole.layout()).expect("streamed");
        let (tx, tz) = whole.tile_grid();
        let west: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx / 2).map(move |x| (x, z))).collect();
        cache
            .load_cell(0, &whole.cook_cell(&west).expect("cooks"))
            .expect("loads");
        cache.update();
        app.insert_resource(cache);
        let world = app.world_mut();
        let added = crowd_add_agent(world, &json!({ "position": [-10, 0, 0] })).expect("on the mesh");
        let id = added["id"].clone();
        let e =
            crowd_set_target(world, &json!({ "id": id, "target": [12, 0, 0] })).expect_err("off the mesh");
        assert_eq!(reason(&e), "nav_off_mesh");
        let set = crowd_set_target(
            world,
            &json!({ "id": id, "target": [12, 0, 0], "streamed": true }),
        )
        .expect("streamed");
        assert_eq!(set["outcome"], "nav_tile_unloaded");
        assert!(set["agent"]["awaiting_tile"].is_array(), "{set}");
        assert_eq!(set["agent"]["streamed"], true);
        crowd_step(world, &json!({ "steps": 900 })).expect("steps");
        let status = crowd_status(world, &json!({})).expect("status");
        assert_eq!(status["agents"][0]["state"], "awaiting_tile", "{status}");
        assert_eq!(status["stats"]["tile_waits"], 1);
        assert!(crowd_set_target(world, &json!({ "id": id, "target": [12, 0, 0], "streamed": 1 })).is_err());
    }

    fn lanes_app() -> App {
        let mut app = cache_app();
        let lanes = json!({ "lanes": [
            { "points": [[0, 0, 0], [10, 0, 0]], "next": [1, 2], "user_id": 4 },
            { "points": [[10, 0, 0], [15, 0, 0]], "next": [] , "speed": 0.5 },
            { "points": [[10, 0, 0], [15, 0, 5]], "next": [] },
        ] });
        lanes_set(app.world_mut(), &lanes).expect("valid lanes");
        app
    }

    #[test]
    fn a_lane_agent_walks_its_route_through_the_protocol() {
        let mut app = lanes_app();
        let world = app.world_mut();
        let status = lanes_status(world, &json!({})).expect("status");
        assert_eq!(status["lanes"].as_array().expect("list").len(), 3);
        assert_eq!(status["crowd"], Value::Null);

        let route = lane_route(world, &json!({ "from": 0, "to": 2 })).expect("valid");
        assert_eq!(route["route"]["lanes"], json!([0, 2]));
        let none = lane_route(world, &json!({ "from": 2, "to": 0 })).expect("valid");
        assert_eq!(none["route"], Value::Null);
        let near = lane_nearest(world, &json!({ "point": [5, 0, 1] })).expect("valid");
        assert_eq!(near["lane"], 0);
        assert_eq!(lane_check(world, &json!({})).expect("valid")["on_mesh"], true);

        let agent =
            lane_add_agent(world, &json!({ "lane": 0, "distance": 8, "speed": 2 })).expect("valid agent");
        let id = agent["id"].clone();
        let out = lane_set_destination(world, &json!({ "id": id, "lane": 2 })).expect("a route");
        assert_eq!(out["agent"]["route"], json!([2]));
        let run = lane_step(world, &json!({ "steps": 600 })).expect("steps");
        assert_eq!(run["transitions"], json!([{ "agent": 0, "from": 0, "to": 2 }]));
        assert_eq!(run["transitions_total"], 1);
        let page = lane_agents(world, &json!({})).expect("page");
        assert_eq!(page["agents"][0]["state"], "stopped");
        assert_eq!(page["agents"][0]["lane"], 2);
        assert_eq!(page["agents"][0]["position"], json!([15.0, 0.0, 5.0]));
        assert_eq!(
            lanes_status(world, &json!({})).expect("status")["crowd"]["stopped"],
            1
        );
        let free = lane_set_destination(world, &json!({ "id": id, "lane": null })).expect("known agent");
        assert_eq!(free["agent"]["has_destination"], false);
        lane_remove_agent(world, &json!({ "id": id })).expect("known agent");
    }

    #[test]
    fn a_closed_lane_and_a_ron_text_go_through_the_protocol() {
        let mut app = lanes_app();
        let world = app.world_mut();
        lane_add_agent(world, &json!({ "lane": 0 })).expect("valid agent");
        let closed = lane_set_enabled(world, &json!({ "user_id": 4, "enabled": false })).expect("valid");
        assert_eq!(closed["changed"], 1);
        let ron = "(format_version: 1, lanes: [(points: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0)])])";
        let set = lanes_set(world, &json!({ "ron": ron })).expect("valid text");
        assert_eq!(set["lanes"], 1);
        assert_eq!(set["agents_dropped"], 1);
    }

    #[test]
    fn lane_errors_carry_reasons() {
        let mut app = App::new();
        let world = app.world_mut();
        let e = lanes_status(world, &json!({})).expect_err("no graph");
        assert_eq!(reason(&e), "nav_no_lane_graph");
        assert!(lanes_set(world, &json!({})).is_err());
        let bad = json!({ "ron": "(format_version: 3, lanes: [])" });
        assert_eq!(
            reason(&lanes_set(world, &bad).expect_err("newer")),
            "nav_lane_version"
        );
        let bad = json!({ "lanes": [{ "points": [[0, 0, 0]] }] });
        assert_eq!(
            reason(&lanes_set(world, &bad).expect_err("one point")),
            "nav_invalid_lane"
        );

        let mut app = lanes_app();
        let world = app.world_mut();
        let e = lane_step(world, &json!({})).expect_err("no crowd");
        assert_eq!(reason(&e), "nav_no_lane_crowd");
        let e = lane_add_agent(world, &json!({ "lane": 9 })).expect_err("no lane");
        assert_eq!(reason(&e), "nav_unknown_lane");
        let e = lane_add_agent(world, &json!({ "lane": 0, "radius": 0 })).expect_err("bad radius");
        assert_eq!(reason(&e), "nav_invalid_lane_agent");
        let e = lane_set_destination(world, &json!({ "id": 7, "lane": 1 })).expect_err("no agent");
        assert_eq!(reason(&e), "nav_unknown_lane_agent");
        lane_add_agent(world, &json!({ "lane": 2 })).expect("valid agent");
        let e = lane_set_destination(world, &json!({ "id": 0, "lane": 0 })).expect_err("no route");
        assert_eq!(reason(&e), "nav_no_lane_route");
        assert!(lane_agents(world, &json!({ "limit": 0 })).is_err());
        assert!(lane_step(world, &json!({ "steps": 0 })).is_err());
    }

    #[test]
    fn a_dormant_crowd_agent_shows_in_the_status() {
        let whole = cache_app();
        let cache = whole.world().resource::<NavTileCache>();
        let (tx, tz) = cache.tile_grid();
        let tiles: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx).map(move |x| (x, z))).collect();
        let blob = cache.cook_cell(&tiles).expect("cooks");
        let mut streamed = NavTileCache::streamed(cache.params().clone(), cache.layout()).expect("streamed");
        streamed.load_cell(0, &blob).expect("loads");
        streamed.update();
        let mut app = App::new();
        app.insert_resource(streamed);
        let world = app.world_mut();
        crowd_add_agent(world, &json!({ "position": [2, 0, 2] })).expect("on the mesh");
        world
            .resource_mut::<NavTileCache>()
            .unload_cell(0)
            .expect("loaded");
        crowd_step(world, &json!({ "steps": 3 })).expect("steps");
        let status = crowd_status(world, &json!({})).expect("status");
        assert_eq!(status["agents"][0]["state"], "dormant");
        assert_eq!(status["dormant"], 1);
    }
}
