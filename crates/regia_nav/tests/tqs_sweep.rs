// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep (sprint-test-quality-sweep, ADR-2077): deterministic edge-case tests for
//! the least-covered public functions of `regia_nav` (link filters, lane graph length, lane
//! destinations, link traversal progress, the region queue source, area volumes, crowd desired
//! velocity, and the lane format metadata). A test that failed on the old code is the regression
//! test for the fix named next to it in `src`.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, MAX_NAV_AREAS, NavAreaVolume, NavCrowd, NavCrowdConfig, NavLaneAgentParams,
    NavLaneAgentState, NavLaneCrowd, NavLaneCrowdConfig, NavLaneDef, NavLaneError, NavLaneGraph,
    NavLinkFlags, NavLinkKind, NavLinkTraversal, NavMeshBuildParams, NavMeshBuilder, NavObstacleShape,
    NavQueryFilter, NavRegionQueue, NavTileCache, NavTileCacheParams, nav_lanes_migration_sources,
    nav_lanes_schema_fingerprint,
};
use regia_nav::{NavLaneSet, NavLinkSet, OffMeshLink};

fn floor(half: f32) -> Vec<[Vec3; 3]> {
    vec![
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, -half),
            Vec3::new(half, 0.0, half),
        ],
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, half),
            Vec3::new(-half, 0.0, half),
        ],
    ]
}

fn lane(points: &[(f32, f32)]) -> NavLaneDef {
    let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
    NavLaneDef::new(&p)
}

// ---------------------------------------------------------------------------------------------
// links.rs: NavQueryFilter

#[test]
fn passes_flags_needs_an_included_and_no_excluded_flag() {
    let all = NavQueryFilter::default();
    assert!(all.passes_flags(1));
    assert!(all.passes_flags(NavLinkFlags::ALL));
    assert!(!all.passes_flags(0), "a link with no flags passes no filter");
    let walk_only = NavQueryFilter {
        include_flags: 0b01,
        ..NavQueryFilter::default()
    };
    assert!(walk_only.passes_flags(0b01));
    assert!(walk_only.passes_flags(0b11), "one included flag is enough");
    assert!(!walk_only.passes_flags(0b10));
    let no_jump = NavQueryFilter {
        exclude_flags: 0b10,
        ..NavQueryFilter::default()
    };
    assert!(no_jump.passes_flags(0b01));
    assert!(!no_jump.passes_flags(0b11), "any excluded flag refuses the link");
    let nothing = NavQueryFilter {
        include_flags: 0,
        ..NavQueryFilter::default()
    };
    for flags in [0, 1, NavLinkFlags::ALL] {
        assert!(!nothing.passes_flags(flags));
    }
}

#[test]
fn allows_area_follows_without_area_for_every_valid_id() {
    let base = NavQueryFilter::default();
    for area in 0..MAX_NAV_AREAS as u8 {
        assert!(base.allows_area(area), "{area}");
        let f = base.without_area(area).expect("a valid id");
        assert!(!f.allows_area(area), "{area}");
        // Only that area is left out.
        for other in 0..MAX_NAV_AREAS as u8 {
            if other != area {
                assert!(f.allows_area(other), "{area} also excluded {other}");
            }
        }
        // Leaving it out twice is the same filter.
        assert_eq!(
            f.without_area(area).expect("valid").excluded_areas,
            f.excluded_areas
        );
    }
    for area in [MAX_NAV_AREAS as u8, u8::MAX] {
        assert!(base.without_area(area).is_err(), "{area}");
    }
}

// ---------------------------------------------------------------------------------------------
// lanes.rs: total_length, has_destination / clear_destination, format metadata

fn diamond() -> NavLaneGraph {
    let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    a.next = vec![1, 2];
    let mut slow = lane(&[(10.0, 0.0), (30.0, 0.0)]);
    slow.next = vec![3];
    let mut fast = lane(&[(10.0, 0.0), (20.0, 5.0), (30.0, 0.0)]);
    fast.next = vec![3];
    let out = lane(&[(30.0, 0.0), (40.0, 0.0)]);
    NavLaneGraph::build(&[a, slow, fast, out], 0.0).expect("valid lanes")
}

#[test]
fn total_length_is_the_sum_of_the_lane_polylines() {
    let g = diamond();
    let expected = 10.0 + 20.0 + 2.0 * 125.0_f32.sqrt() + 10.0;
    assert!(
        (g.total_length() - expected).abs() < 1e-3,
        "{} vs {expected}",
        g.total_length()
    );
    let sum: f32 = (0..4).map(|i| g.lane(i).expect("lane").length()).sum();
    assert!((g.total_length() - sum).abs() < 1e-4);
    // Closing a lane does not change how long the graph is.
    let mut closed = diamond();
    closed.set_lane_enabled(0, false);
    assert!((closed.total_length() - g.total_length()).abs() < 1e-6);
}

#[test]
fn destinations_set_and_clear_and_unknown_agents_are_typed_errors() {
    let g = diamond();
    let mut c = NavLaneCrowd::new(NavLaneCrowdConfig::default()).expect("config");
    let id = c
        .add_agent(&g, 0, 1.0, 0.0, NavLaneAgentParams::default(), 7)
        .expect("valid agent");
    let agent = |c: &NavLaneCrowd| c.agent(id).cloned().expect("agent");
    assert!(!agent(&c).has_destination(), "a new agent wanders");
    c.set_destination(&g, id, 3).expect("reachable");
    assert!(agent(&c).has_destination());
    assert!(!agent(&c).route().is_empty());
    c.clear_destination(id).expect("known agent");
    assert!(!agent(&c).has_destination());
    assert!(agent(&c).route().is_empty());
    // Clearing twice is harmless.
    c.clear_destination(id).expect("known agent");
    // A removed agent is unknown.
    c.remove_agent(id).expect("known agent");
    assert!(matches!(c.clear_destination(id), Err(NavLaneError::UnknownAgent(x)) if x == id));
}

#[test]
fn clearing_a_finished_route_lets_the_agent_wander_on() {
    let g = diamond();
    let mut c = NavLaneCrowd::new(NavLaneCrowdConfig::default()).expect("config");
    let id = c
        .add_agent(&g, 0, 9.0, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    // A destination on its own lane: it stops at the lane's end.
    c.set_destination(&g, id, 0).expect("own lane");
    for _ in 0..200 {
        c.step(&g);
    }
    assert_eq!(c.agent(id).map(|a| a.state()), Some(NavLaneAgentState::Stopped));
    c.clear_destination(id).expect("known agent");
    assert_eq!(
        c.agent(id).map(|a| a.state()),
        Some(NavLaneAgentState::Walking),
        "a cleared route sets a stopped agent walking"
    );
}

#[test]
fn nav_lanes_format_metadata_is_stable() {
    assert_eq!(
        nav_lanes_migration_sources(),
        vec![1],
        "v2 reads and upgrades v1 (ADR-2420)"
    );
    let f = nav_lanes_schema_fingerprint();
    assert!(!f.is_empty());
    assert_eq!(f, nav_lanes_schema_fingerprint());
}

// ---------------------------------------------------------------------------------------------
// crowd.rs: NavLinkTraversal::progress / position, CrowdAgent::desired_velocity

fn traversal(steps: u32, total_steps: u32, origin: Vec3, start: Vec3, end: Vec3) -> NavLinkTraversal {
    NavLinkTraversal {
        link: 0,
        user_id: 9,
        kind: NavLinkKind::Jump,
        origin,
        start,
        end,
        steps,
        total_steps,
        step_seconds: 1.0 / 30.0,
        arc: None,
        walk_seconds: 0.0,
    }
}

#[test]
fn link_traversal_progress_is_clamped_and_the_position_runs_origin_start_end() {
    let origin = Vec3::new(0.0, 0.0, 0.0);
    let start = Vec3::new(1.0, 0.0, 0.0);
    let end = Vec3::new(1.0, 3.0, 0.0);
    let total = 8;
    let mut last = origin;
    let mut travelled = 0.0;
    for s in 0..=total {
        let t = traversal(s, total, origin, start, end);
        let p = t.progress();
        assert!((p - s as f32 / total as f32).abs() < 1e-6);
        let pos = t.position();
        assert!(pos.is_finite());
        travelled += pos.distance(last);
        last = pos;
    }
    assert_eq!(traversal(0, total, origin, start, end).position(), origin);
    assert!(last.distance(end) < 1e-5);
    // One speed over the whole crossing: the path walked is the walk plus the link, no detours.
    assert!((travelled - 4.0).abs() < 1e-4, "{travelled}");
    // Past the end and a zero step count clamp.
    assert_eq!(traversal(20, total, origin, start, end).progress(), 1.0);
    assert_eq!(traversal(3, 0, origin, start, end).progress(), 1.0);
    assert_eq!(traversal(0, 0, origin, start, end).progress(), 0.0);
    // No walk (origin at the start) and a zero-length link.
    let no_walk = traversal(4, total, start, start, end);
    assert!((no_walk.position() - Vec3::new(1.0, 1.5, 0.0)).length() < 1e-5);
    let point = traversal(4, total, origin, start, start);
    assert!(point.position().is_finite());
    assert_eq!(traversal(total, total, start, start, start).position(), start);
    let t = traversal(4, total, origin, start, end);
    assert!((t.elapsed() - 4.0 / 30.0).abs() < 1e-6 && (t.duration() - 8.0 / 30.0).abs() < 1e-6);
}

#[test]
fn desired_velocity_is_zero_at_rest_and_bounded_by_the_max_speed_while_moving() {
    let mesh = NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&floor(6.0))
    .expect("floor builds");
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let params = CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0);
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), params)
        .expect("agent");
    crowd.step(&mesh);
    let at_rest = crowd.agent(id).expect("agent").desired_velocity();
    assert_eq!(at_rest, Vec3::ZERO, "no target, no desire");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut moved = false;
    for _ in 0..30 {
        crowd.step(&mesh);
        let a = crowd.agent(id).expect("agent");
        let d = a.desired_velocity();
        assert!(d.is_finite());
        assert!(d.length() <= 3.0 + 1e-3, "{d}");
        if d.length() > 0.1 {
            moved = true;
            assert!(d.x > 0.0, "it wants to go toward the target: {d}");
        }
    }
    assert!(moved);
}

// ---------------------------------------------------------------------------------------------
// region_queue.rs, tile_cache.rs

#[test]
fn region_queue_has_source_only_after_one_is_set() {
    let mut q = NavRegionQueue::default();
    assert!(!q.has_source());
    q.set_source(Arc::new(|_, _, _, _| false));
    assert!(q.has_source());
    assert!(format!("{q:?}").contains("has_source: true"));
}

#[test]
fn area_volume_reads_back_what_was_added_until_it_is_removed() {
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 16,
        },
        &floor(6.0),
    )
    .expect("cache builds");
    let shape = NavObstacleShape::Box {
        min: Vec3::new(-1.0, -1.0, -1.0),
        max: Vec3::new(1.0, 2.0, 1.0),
    };
    let a = cache
        .add_area_volume(NavAreaVolume::new(shape, 3))
        .expect("valid");
    let b = cache
        .add_area_volume(NavAreaVolume::new(shape, 4))
        .expect("valid");
    assert_ne!(a, b);
    assert_eq!(cache.area_volume(a).map(|v| v.area), Some(3));
    assert_eq!(cache.area_volume(b).map(|v| v.area), Some(4));
    let ids: Vec<_> = cache.area_volumes().map(|(id, _)| id).collect();
    assert_eq!(ids, vec![a, b], "id order is the paint order");
    assert_eq!(cache.remove_area_volume(a).map(|v| v.area).ok(), Some(3));
    assert_eq!(cache.area_volume(a), None);
    assert!(cache.remove_area_volume(a).is_err(), "removed twice");
    assert_eq!(cache.area_volume(b).map(|v| v.area), Some(4));
}

// ---------------------------------------------------------------------------------------------
// mesh.rs, tile_cache.rs, tile_cache/stream.rs: counts, tile ranges, a cache built whole

fn whole_cache() -> NavTileCache {
    NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 16,
        },
        &floor(6.0),
    )
    .expect("cache builds")
}

#[test]
fn vertex_count_sums_the_polygon_vertices_and_tile_poly_counts_cover_the_mesh() {
    let cache = whole_cache();
    let mesh = cache.mesh();
    assert!(mesh.poly_count() > 0);
    assert!(
        mesh.vertex_count() >= 3 * mesh.poly_count(),
        "every polygon has 3 or more vertices"
    );
    let (tx, tz) = cache.tile_grid();
    assert!(tx > 1 && tz > 1, "the floor spans several tiles");
    let mut total = 0;
    for z in 0..tz {
        for x in 0..tx {
            total += cache.tile_poly_count(x, z).expect("in range");
        }
    }
    assert!(total > 0);
    assert_eq!(cache.tile_poly_count(tx, 0), None);
    assert_eq!(cache.tile_poly_count(0, tz), None);
    assert_eq!(cache.tile_poly_count(u32::MAX, u32::MAX), None);
}

#[test]
fn tiles_in_lists_touched_tiles_in_tile_order_and_ignores_bad_boxes() {
    let cache = whole_cache();
    let (tx, tz) = cache.tile_grid();
    let all = cache.tiles_in(Vec3::splat(-100.0), Vec3::splat(100.0));
    assert_eq!(all.len(), (tx * tz) as usize);
    let mut expected = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            expected.push((x, z));
        }
    }
    assert_eq!(all, expected, "tile order: z rows, x within a row");
    // Swapped corners are the same box.
    let a = cache.tiles_in(Vec3::new(-5.0, 0.0, -5.0), Vec3::new(-2.0, 0.0, 1.0));
    let b = cache.tiles_in(Vec3::new(-2.0, 0.0, 1.0), Vec3::new(-5.0, 0.0, -5.0));
    assert_eq!(a, b);
    assert!(!a.is_empty() && a.len() < all.len());
    for bad in [Vec3::NAN, Vec3::new(f32::INFINITY, 0.0, 0.0)] {
        assert!(cache.tiles_in(bad, Vec3::ONE).is_empty());
        assert!(cache.tiles_in(Vec3::ONE, bad).is_empty());
    }
    assert!(
        cache.tiles_in(Vec3::splat(50.0), Vec3::splat(60.0)).is_empty(),
        "off the grid"
    );
}

#[test]
fn a_cache_built_whole_is_not_streamed_and_every_tile_is_ready() {
    let mut cache = whole_cache();
    assert!(!cache.is_streamed());
    let (tx, tz) = cache.tile_grid();
    for z in 0..tz {
        for x in 0..tx {
            assert!(cache.tile_ready(x, z), "({x}, {z})");
            assert_eq!(cache.tile_loaded(x, z), Some(true));
        }
    }
    assert!(!cache.tile_ready(tx, 0));
    assert!(!cache.tile_ready(0, tz));
    assert_eq!(cache.tile_loaded(tx, tz), None);
    // Abandoning replays on a cache with no stream changes nothing.
    let before = cache.mesh().poly_count();
    cache.abandon_replay(Vec3::splat(-100.0), Vec3::splat(100.0));
    assert_eq!(cache.mesh().poly_count(), before);
    assert!(cache.tile_ready(0, 0));
}

// ---------------------------------------------------------------------------------------------
// lanes.rs NavLaneSet::to_ron, link_file.rs NavLinkSet::to_ron_string

#[test]
fn lane_sets_round_trip_through_ron() {
    let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    a.next = vec![1];
    a.width = 3.5;
    let mut b = lane(&[(10.0, 0.0), (20.0, 5.0)]);
    b.speed = 0.75;
    let set = NavLaneSet {
        format_version: 1,
        join_distance: 0.5,
        lanes: vec![a, b],
    };
    let text = set.to_ron().expect("writes");
    let back = NavLaneSet::parse(&text).expect("reads back");
    assert_eq!(back, set);
    assert_eq!(back.to_ron().expect("writes"), text, "writing is deterministic");
    assert!((back.graph().expect("graph").total_length() - (10.0 + 125.0_f32.sqrt())).abs() < 1e-4);
}

#[test]
fn link_sets_round_trip_through_ron_and_refuse_invalid_links() {
    let mut l = OffMeshLink::new(Vec3::new(1.0, 0.0, 2.0), Vec3::new(3.0, 1.0, 2.0));
    l.user_id = 77;
    l.bidirectional = false;
    l.flags = 0b101;
    l.area = 2;
    let links = vec![l, OffMeshLink::new(Vec3::ZERO, Vec3::X)];
    let text = NavLinkSet::to_ron_string(&links).expect("writes");
    assert_eq!(NavLinkSet::from_ron_str(&text).expect("reads back"), links);
    assert_eq!(NavLinkSet::to_ron_string(&links).expect("writes"), text);
    assert_eq!(
        NavLinkSet::from_ron_str(&NavLinkSet::to_ron_string(&[]).expect("empty")).expect("reads"),
        vec![]
    );
    let mut bad = l;
    bad.from = Vec3::NAN;
    assert!(
        NavLinkSet::to_ron_string(&[bad]).is_err(),
        "a NaN endpoint is refused before writing"
    );
}
