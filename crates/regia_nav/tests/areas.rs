// SPDX-License-Identifier: MPL-2.0
//! Polygon area costs (ADR-1934): painted areas split polygons, paths weigh them, filters leave
//! them out, and the tile cache and cooked cells keep them.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, NavAreaCosts, NavAreaVolume, NavBuildError, NavCrowd, NavCrowdConfig,
    NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacleShape, NavObstacles, NavPath, NavQuery,
    NavQueryFilter, NavTileCache, NavTileCacheError, NavTileCacheParams, OffMeshLink,
};

const REACH: Vec3 = Vec3::splat(2.0);
const MUD: u8 = 3;

fn params() -> NavMeshBuildParams {
    let mut p = NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    p.agent.radius = 0.0;
    p
}

fn quad(x0: f32, x1: f32, z0: f32, z1: f32) -> Vec<[Vec3; 3]> {
    vec![
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

fn floor() -> Vec<[Vec3; 3]> {
    quad(-16.0, 16.0, -16.0, 16.0)
}

fn area_box(x0: f32, x1: f32, z0: f32, z1: f32, area: u8) -> NavAreaVolume {
    NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(x0, -1.0, z0),
            max: Vec3::new(x1, 1.0, z1),
        },
        area,
    )
}

/// A pit of mud across the straight line from `(-12, 0)` to `(12, 0)`, with ground around it.
fn pit() -> NavAreaVolume {
    area_box(-6.0, 6.0, -8.0, 8.0, MUD)
}

fn mesh_with(volumes: &[NavAreaVolume]) -> NavMesh {
    NavMeshBuilder::new(params())
        .build_with_areas(&floor(), volumes)
        .expect("builds")
}

fn plan(mesh: &NavMesh, filter: &NavQueryFilter) -> Option<NavPath> {
    let q = NavQuery {
        mesh,
        obstacles: None,
    };
    q.find_path_filtered(
        Vec3::new(-12.0, 0.0, 0.0),
        Vec3::new(12.0, 0.0, 0.0),
        REACH,
        filter,
    )
}

fn crosses(mesh: &NavMesh, path: &NavPath, area: u8) -> bool {
    path.polys.iter().any(|&p| mesh.polys[p as usize].area == area)
}

fn costs(area: u8, cost: f32) -> NavAreaCosts {
    let mut t = NavAreaCosts::default();
    t.set(area, cost).expect("valid cost");
    t
}

#[test]
fn a_volume_splits_the_polygons_and_they_carry_its_id() {
    let plain = NavMeshBuilder::new(params()).build(&floor()).expect("builds");
    let painted = mesh_with(&[pit()]);
    assert_eq!(plain.poly_count(), 1);
    assert!(painted.poly_count() > 1, "the pit has its own polygon");
    let q = NavQuery {
        mesh: &painted,
        obstacles: None,
    };
    let area_at = |x: f32, z: f32| {
        let (p, _) = q.find_nearest(Vec3::new(x, 0.0, z), REACH).expect("on the mesh");
        painted.polys[p as usize].area
    };
    assert_eq!(area_at(0.0, 0.0), MUD);
    assert_eq!(area_at(5.75, 7.75), MUD);
    assert_eq!(area_at(6.25, 0.0), 0);
    assert_eq!(area_at(0.0, 8.25), 0);
}

#[test]
fn the_later_volume_wins_where_two_overlap() {
    let mesh = mesh_with(&[
        area_box(-4.0, 4.0, -4.0, 4.0, 1),
        area_box(0.0, 8.0, -4.0, 4.0, 2),
    ]);
    let q = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let area_at = |x: f32| {
        let (p, _) = q
            .find_nearest(Vec3::new(x, 0.0, 0.0), REACH)
            .expect("on the mesh");
        mesh.polys[p as usize].area
    };
    assert_eq!(area_at(-2.0), 1);
    assert_eq!(area_at(2.0), 2);
    assert_eq!(area_at(6.0), 2);
}

#[test]
fn a_volume_only_paints_the_surface_inside_its_height() {
    let high = NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(-6.0, 5.0, -8.0),
            max: Vec3::new(6.0, 9.0, 8.0),
        },
        MUD,
    );
    let mesh = mesh_with(&[high]);
    assert!(
        mesh.polys.iter().all(|p| p.area == 0),
        "the floor is below the volume"
    );
}

#[test]
fn bad_volumes_are_rejected_by_the_builder() {
    let builder = NavMeshBuilder::new(params());
    let bad_area = area_box(0.0, 1.0, 0.0, 1.0, 64);
    assert!(matches!(
        builder.build_with_areas(&floor(), &[bad_area]),
        Err(NavBuildError::InvalidInput(_))
    ));
    let bad_shape = area_box(1.0, 1.0, 0.0, 1.0, 1);
    assert!(matches!(
        builder.build_with_areas(&floor(), &[bad_shape]),
        Err(NavBuildError::InvalidInput(_))
    ));
}

#[test]
fn the_cost_decides_between_across_and_around() {
    let mut mesh = mesh_with(&[pit()]);
    let open = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(crosses(&mesh, &open, MUD), "at cost 1 the straight line wins");
    assert!((open.length - 24.0).abs() < 1e-3, "straight: {}", open.length);

    mesh.set_area_costs(costs(MUD, 8.0));
    let around = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(!crosses(&mesh, &around, MUD), "polys {:?}", around.polys);
    assert!(
        around.length > 26.0,
        "the way around is longer: {}",
        around.length
    );
}

#[test]
fn a_cheap_link_far_behind_the_start_is_still_found() {
    // The heuristic may not assume a metre costs a metre: a 76 m link at a hundredth of that lands
    // beside the goal, and the search has to go backward to take it.
    let mut tris = quad(-16.0, 16.0, -4.0, 4.0);
    tris.extend(quad(58.0, 80.0, -4.0, 4.0));
    tris.extend(quad(16.0, 58.0, -0.5, 0.5));
    let mut link = OffMeshLink::new(Vec3::new(-14.0, 0.0, 0.0), Vec3::new(62.0, 0.0, 0.0));
    link.cost_scale = 0.01;
    let mesh = NavMeshBuilder::new(params())
        .build(&tris)
        .expect("builds")
        .with_off_mesh_links(vec![link]);
    let q = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let path = q
        .find_path(Vec3::new(10.0, 0.0, 0.0), Vec3::new(66.0, 0.0, 0.0), REACH)
        .expect("a path");
    assert!(
        path.waypoints.contains(&Vec3::new(62.0, 0.0, 0.0)),
        "24 m back and a cheap hop beats 56 m on foot: {:?}",
        path.waypoints
    );
}

#[test]
fn a_cost_the_detour_does_not_justify_stays_on_the_line() {
    // Around the pit is about 34 m, across 24 m with 12 m of mud: at 1.2 the mud is still cheaper.
    let mut mesh = mesh_with(&[pit()]);
    mesh.set_area_costs(costs(MUD, 1.2));
    let path = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(crosses(&mesh, &path, MUD), "polys {:?}", path.polys);
}

#[test]
fn a_filter_scales_the_mesh_cost_per_query() {
    let mut mesh = mesh_with(&[pit()]);
    mesh.set_area_costs(costs(MUD, 8.0));
    let amphibious = NavQueryFilter::default()
        .with_area_cost(MUD, 0.125)
        .expect("valid scale");
    let path = plan(&mesh, &amphibious).expect("a path");
    assert!(
        crosses(&mesh, &path, MUD),
        "8 x 0.125 is plain ground to this query"
    );
    let default = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(!crosses(&mesh, &default, MUD));
}

#[test]
fn a_cheap_road_draws_the_path_off_the_straight_line() {
    // West and east ground, joined by a strip at cost 2 and by a road along z = 6..8 at cost 0.05.
    // Over polygon centres the strip costs about 19 and the road about 17, but a search whose
    // heuristic ignores the road's low cost reaches the goal through the strip first and stops
    // there: the heuristic has to scale with the cheapest area.
    let mut mesh = mesh_with(&[
        area_box(-1.0, 1.0, -16.5, 6.0, 8),
        area_box(-16.5, 16.5, 6.0, 8.0, 7),
    ]);
    let mut table = costs(7, 0.05);
    table.set(8, 2.0).expect("valid cost");
    mesh.set_area_costs(table);
    let path = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(crosses(&mesh, &path, 7), "polys {:?}", path.polys);
    assert!(!crosses(&mesh, &path, 8), "polys {:?}", path.polys);
}

#[test]
fn a_left_out_area_is_never_crossed_and_can_close_the_way() {
    let mesh = mesh_with(&[pit()]);
    let no_mud = NavQueryFilter::default().without_area(MUD).expect("valid id");
    let path = plan(&mesh, &no_mud).expect("around the pit");
    assert!(!crosses(&mesh, &path, MUD));

    let wall = mesh_with(&[area_box(-6.0, 6.0, -16.5, 16.5, MUD)]);
    assert!(plan(&wall, &no_mud).is_none(), "the area spans the floor");
    assert!(
        plan(&wall, &NavQueryFilter::default()).is_some(),
        "it closes the way for that filter only"
    );
}

#[test]
fn an_end_in_a_left_out_area_snaps_off_it() {
    let mesh = mesh_with(&[pit()]);
    let no_mud = NavQueryFilter::default().without_area(MUD).expect("valid id");
    let q = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let inside = Vec3::new(5.0, 0.0, 0.0);
    let (poly, point) = q
        .find_nearest_filtered(inside, REACH, &no_mud)
        .expect("ground within reach");
    assert_eq!(mesh.polys[poly as usize].area, 0);
    assert!((point.x - 6.0).abs() < 1e-3, "the edge of the pit: {point:?}");
    assert!(
        q.find_nearest_filtered(Vec3::ZERO, REACH, &no_mud).is_none(),
        "the middle of the pit is out of reach of any ground"
    );
    let path = q
        .find_path_filtered(Vec3::new(12.0, 0.0, 0.0), inside, REACH, &no_mud)
        .expect("a path to the edge");
    assert!(!crosses(&mesh, &path, MUD));
}

#[test]
fn an_excluded_id_past_the_table_is_an_error() {
    let f = NavQueryFilter::default();
    assert_eq!(
        f.without_area(64).map(|_| ()).map_err(|e| e.code()),
        Err("nav_invalid_area")
    );
    assert_eq!(
        f.with_area_cost(1, 0.0).map(|_| ()).map_err(|e| e.code()),
        Err("nav_invalid_area_cost")
    );
}

#[test]
fn a_link_pays_its_area_and_a_left_out_area_closes_it() {
    // Two islands; the only way over is a link of area 5.
    let mut tris = quad(-12.0, -2.0, -4.0, 4.0);
    tris.extend(quad(2.0, 12.0, -4.0, 4.0));
    let mut link = OffMeshLink::new(Vec3::new(-2.5, 0.0, 0.0), Vec3::new(2.5, 0.0, 0.0));
    link.area = 5;
    let mesh = NavMeshBuilder::new(params())
        .build(&tris)
        .expect("builds")
        .with_off_mesh_links(vec![link]);
    assert!(plan(&mesh, &NavQueryFilter::default()).is_some());
    let closed = NavQueryFilter::default().without_area(5).expect("valid id");
    assert!(plan(&mesh, &closed).is_none());
    assert!(!closed.passes(&link));
}

#[test]
fn a_dear_link_area_loses_to_a_walk() {
    // A bridge joins the islands at z = 6..8; the link is the short way until its area costs.
    let mut tris = quad(-12.0, -2.0, -4.0, 8.0);
    tris.extend(quad(2.0, 12.0, -4.0, 8.0));
    tris.extend(quad(-2.0, 2.0, 6.0, 8.0));
    let mut link = OffMeshLink::new(Vec3::new(-2.5, 0.0, 0.0), Vec3::new(2.5, 0.0, 0.0));
    link.area = 5;
    let mut mesh = NavMeshBuilder::new(params())
        .build(&tris)
        .expect("builds")
        .with_off_mesh_links(vec![link]);
    let cheap = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(
        (cheap.length - 24.0).abs() < 1.0,
        "over the link: {}",
        cheap.length
    );
    mesh.set_area_costs(costs(5, 40.0));
    let dear = plan(&mesh, &NavQueryFilter::default()).expect("a path");
    assert!(dear.length > 26.0, "over the bridge: {}", dear.length);
}

#[test]
fn carving_obstacles_keeps_areas_and_costs() {
    let mut mesh = mesh_with(&[pit()]);
    mesh.set_area_costs(costs(MUD, 8.0));
    let mut obstacles = NavObstacles::default();
    obstacles.add_box(Vec3::new(-14.0, -1.0, -14.0), Vec3::new(-13.0, 1.0, -13.0));
    let q = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    };
    let path = q
        .find_path(Vec3::new(-12.0, 0.0, 0.0), Vec3::new(12.0, 0.0, 0.0), REACH)
        .expect("a path");
    assert!(path.length > 26.0, "still goes around the pit: {}", path.length);
}

fn cache_params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: params(),
        tile_size: 16,
    }
}

#[test]
fn a_tile_cache_volume_rebuilds_its_tiles_only_and_can_go() {
    let mut cache = NavTileCache::build(cache_params(), &floor()).expect("builds");
    let revision = cache.revision();
    let id = cache
        .add_area_volume(area_box(1.0, 3.0, 1.0, 3.0, MUD))
        .expect("valid volume");
    assert_eq!(cache.pending_tiles().len(), 1, "the volume lies in one tile");
    assert!(cache.mesh().polys.iter().all(|p| p.area == 0), "nothing yet");
    cache.update();
    assert_eq!(cache.total_rebuilds(), 1);
    assert!(cache.revision() > revision);
    assert!(cache.mesh().polys.iter().any(|p| p.area == MUD));
    assert_eq!(cache.area_volumes().count(), 1);

    assert_eq!(cache.remove_area_volume(id).map(|v| v.area), Ok(MUD));
    cache.update();
    assert!(cache.mesh().polys.iter().all(|p| p.area == 0));
    assert_eq!(
        cache.remove_area_volume(id).map(|_| ()).map_err(|e| e.code()),
        Err("nav_unknown_area_volume")
    );
}

#[test]
fn tile_cache_errors_are_typed() {
    let mut cache = NavTileCache::build(cache_params(), &floor()).expect("builds");
    assert_eq!(
        cache
            .add_area_volume(area_box(0.0, 1.0, 0.0, 1.0, 64))
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_invalid_area")
    );
    assert!(matches!(
        cache.add_area_volume(area_box(1.0, 1.0, 0.0, 1.0, 1)),
        Err(NavTileCacheError::InvalidObstacle(_))
    ));
}

#[test]
fn a_tile_cache_paints_what_a_whole_build_paints() {
    let volumes = [pit(), area_box(2.0, 10.0, -2.0, 2.0, 2)];
    let whole = mesh_with(&volumes);
    let mut cache = NavTileCache::build(cache_params(), &floor()).expect("builds");
    for v in volumes {
        cache.add_area_volume(v).expect("valid volume");
    }
    cache.update();
    for ix in 0..64 {
        for iz in 0..64 {
            let p = Vec3::new(-15.875 + ix as f32 * 0.5, 0.0, -15.875 + iz as f32 * 0.5);
            let a = NavQuery {
                mesh: &whole,
                obstacles: None,
            }
            .find_nearest(p, REACH)
            .map(|(poly, _)| whole.polys[poly as usize].area);
            let b = cache
                .query()
                .find_nearest(p, REACH)
                .map(|(poly, _)| cache.mesh().polys[poly as usize].area);
            assert_eq!(a, b, "at {p:?}");
        }
    }
}

#[test]
fn costs_survive_a_tile_rebuild_and_setting_them_moves_the_revision() {
    let mut cache = NavTileCache::build(cache_params(), &floor()).expect("builds");
    cache.add_area_volume(pit()).expect("valid volume");
    cache.update();
    let before = cache.revision();
    cache.set_area_costs(costs(MUD, 8.0));
    assert!(cache.revision() > before, "a crowd must replan");
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-14.0, -1.0, -14.0),
            max: Vec3::new(-13.0, 1.0, -13.0),
        })
        .expect("valid obstacle");
    cache.update();
    assert_eq!(cache.mesh().area_costs().get(MUD), 8.0);
    let path = plan(cache.mesh(), &NavQueryFilter::default()).expect("a path");
    assert!(!crosses(cache.mesh(), &path, MUD));
}

#[test]
fn a_region_rebuild_keeps_the_painted_areas() {
    let mut cache = NavTileCache::build(cache_params(), &floor()).expect("builds");
    cache.add_area_volume(pit()).expect("valid volume");
    cache.update();
    let (lo, hi) = (Vec3::new(-2.0, -1.0, -2.0), Vec3::new(2.0, 1.0, 2.0));
    cache
        .rebuild_region(lo, hi, &quad(-16.0, 1.0, -16.0, 16.0))
        .expect("rebuilds");
    cache.update();
    assert!(cache.mesh().polys.iter().any(|p| p.area == MUD));
    let (poly, _) = cache
        .query()
        .find_nearest(Vec3::new(-1.0, 0.0, 0.0), REACH)
        .expect("still floor");
    assert_eq!(cache.mesh().polys[poly as usize].area, MUD);
}

fn walk(crowd: &mut NavCrowd, mesh: &NavMesh, id: regia_nav::CrowdAgentId, steps: usize) -> Vec<Vec3> {
    let mut out = Vec::with_capacity(steps);
    for _ in 0..steps {
        crowd.step(mesh);
        let agent = crowd.agent(id).expect("agent");
        out.push(agent.position());
        if agent.state() == CrowdAgentState::Arrived {
            break;
        }
    }
    out
}

fn in_pit(p: Vec3) -> bool {
    p.x.abs() < 5.9 && p.z.abs() < 7.9
}

#[test]
fn a_crowd_agent_walks_around_a_dear_area_shortcuts_and_all() {
    let mut mesh = mesh_with(&[pit()]);
    mesh.set_area_costs(costs(MUD, 8.0));
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-12.0, 0.0, 0.0), CrowdAgentParams::default())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(12.0, 0.0, 0.0))
        .expect("on the mesh");
    let track = walk(&mut crowd, &mesh, id, 1200);
    assert_eq!(
        crowd.agent(id).map(|a| a.state()),
        Some(CrowdAgentState::Arrived),
        "ended at {:?}",
        track.last()
    );
    assert!(
        !track.iter().any(|&p| in_pit(p)),
        "a step of the walk crossed the mud"
    );
}

#[test]
fn an_agent_that_does_not_mind_mud_goes_straight_on_the_same_mesh() {
    let mut mesh = mesh_with(&[pit()]);
    mesh.set_area_costs(costs(MUD, 8.0));
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut p = CrowdAgentParams::default();
    p.link_filter = p.link_filter.with_area_cost(MUD, 0.125).expect("valid scale");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-12.0, 0.0, 0.0), p)
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(12.0, 0.0, 0.0))
        .expect("on the mesh");
    let track = walk(&mut crowd, &mesh, id, 1200);
    assert!(track.iter().any(|&p| in_pit(p)));
}

#[test]
fn an_agent_with_no_way_around_a_left_out_area_stands_and_says_so() {
    // The wall itself is proved in `crowd::tests`; a no_path agent gets no steering at all.
    let mesh = mesh_with(&[area_box(-6.0, 6.0, -16.5, 16.5, MUD)]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut p = CrowdAgentParams::default();
    p.link_filter = p.link_filter.without_area(MUD).expect("valid id");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-12.0, 0.0, 0.0), p)
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(12.0, 0.0, 0.0))
        .expect("on the mesh");
    assert_eq!(crowd.agent(id).map(|a| a.state()), Some(CrowdAgentState::NoPath));
    for _ in 0..300 {
        crowd.step(&mesh);
        let at = crowd.agent(id).expect("agent").position();
        assert!(at.x <= -6.0, "entered the area at {at:?}");
    }
}

#[test]
fn a_crowd_of_two_cannot_push_an_agent_into_a_left_out_area() {
    let mesh = mesh_with(&[pit()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut careful = CrowdAgentParams::default();
    careful.link_filter = careful.link_filter.without_area(MUD).expect("valid id");
    // Standing against the pit's edge, in the way of a walker heading through it.
    let standing = crowd
        .add_agent(&mesh, Vec3::new(-6.1, 0.0, 0.0), careful)
        .expect("on the mesh");
    let walker = crowd
        .add_agent(&mesh, Vec3::new(-12.0, 0.0, 0.0), CrowdAgentParams::default())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, walker, Vec3::new(12.0, 0.0, 0.0))
        .expect("on the mesh");
    for _ in 0..400 {
        crowd.step(&mesh);
        let at = crowd.agent(standing).expect("agent").position();
        assert!(at.x <= -6.0 + 1e-3 || at.z.abs() >= 8.0, "pushed in at {at:?}");
    }
}

#[test]
fn changing_an_agents_filter_replans_it() {
    let mesh = mesh_with(&[pit()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-12.0, 0.0, 0.0), CrowdAgentParams::default())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(12.0, 0.0, 0.0))
        .expect("on the mesh");
    let crosses_mud = |crowd: &NavCrowd| {
        crowd
            .agent(id)
            .expect("agent")
            .corridor()
            .iter()
            .any(|&p| mesh.polys[p as usize].area == MUD)
    };
    assert!(crosses_mud(&crowd));
    let mut p = CrowdAgentParams::default();
    p.link_filter = p.link_filter.without_area(MUD).expect("valid id");
    crowd.set_agent_params(id, p).expect("valid params");
    crowd.step(&mesh);
    assert!(!crosses_mud(&crowd), "the route changed on the next step");
}

#[test]
fn walks_at_area_costs_replay_to_the_same_digest() {
    let run = || {
        let mut mesh = mesh_with(&[pit()]);
        mesh.set_area_costs(costs(MUD, 3.0));
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        for i in 0..8 {
            let z = -7.0 + i as f32 * 2.0;
            let id = crowd
                .add_agent(&mesh, Vec3::new(-12.0, 0.0, z), CrowdAgentParams::default())
                .expect("on the mesh");
            crowd
                .set_target(&mesh, id, Vec3::new(12.0, 0.0, -z))
                .expect("on the mesh");
        }
        for _ in 0..240 {
            crowd.step(&mesh);
        }
        crowd.digest()
    };
    assert_eq!(run(), run());
}
