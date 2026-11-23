// SPDX-License-Identifier: MPL-2.0
//! Off-mesh links and moving obstacles end to end (ADR-1821, `docs/specs/92-navmesh-offmesh.md`
//! § 6): timed crowd crossings with events, instant mode, per-agent filters, targets changed
//! mid-crossing, bit-identical replays, oriented boxes, `move_obstacle`, and links blocked by
//! tile-cache obstacles. Fixed steps and deterministic inputs only; nothing reads the clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavLinkKind, NavLinkPhase,
    NavLinkState, NavLinkTraversal, NavLinkTraversalMode, NavMesh, NavMeshBuildParams, NavMeshBuilder,
    NavObstacleShape, NavQueryFilter, NavTileCache, NavTileCacheParams, OffMeshLink,
};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// Two 5 x 8 m floors at `x ∈ [-6, -1]` and `x ∈ [1, 6]` with a 2 m gap.
fn islands() -> Vec<[Vec3; 3]> {
    let mut t: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0, 0.0).into();
    t.extend(rect(1.0, -4.0, 6.0, 4.0, 0.0));
    t
}

fn params() -> NavMeshBuildParams {
    NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    }
}

/// The gap link: 3 m long, crossed in 1 s.
fn gap_link() -> OffMeshLink {
    let mut l = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .with_user_id(5)
        .with_radius(0.5);
    l.duration = 1.0;
    l
}

fn mesh_with(link: OffMeshLink) -> NavMesh {
    NavMeshBuilder::new(params())
        .build(&islands())
        .expect("the islands build")
        .with_off_mesh_links(vec![link])
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

/// One run: an agent walks from the left floor to the right one. Returns the crowd, the agent and
/// the step index of every link event.
fn cross(
    mesh: &NavMesh,
    config: NavCrowdConfig,
    steps: u32,
) -> (NavCrowd, CrowdAgentId, Vec<(u32, NavLinkPhase)>) {
    let mut crowd = NavCrowd::new(config).expect("config");
    let id = crowd
        .add_agent(mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut events = Vec::new();
    for k in 0..steps {
        crowd.step(mesh);
        for e in crowd.link_events() {
            assert_eq!(e.agent, id);
            assert_eq!((e.user_id, e.kind), (5, NavLinkKind::Jump));
            events.push((k, e.phase));
        }
    }
    (crowd, id, events)
}

/// The distance from `p` to the segment `a → b`.
fn off_segment(p: Vec3, a: Vec3, b: Vec3) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

#[test]
fn a_timed_crossing_takes_its_duration_in_whole_steps() {
    let mesh = mesh_with(gap_link());
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Walking);
    let mut started = None;
    let mut finished = None;
    for k in 0..600u32 {
        crowd.step(&mesh);
        for e in crowd.link_events() {
            match e.phase {
                NavLinkPhase::Started => started = Some(k),
                NavLinkPhase::Finished => finished = Some(k),
                NavLinkPhase::Queued | NavLinkPhase::TimedOut | NavLinkPhase::Cancelled => {
                    panic!("no queue without reservation, no streaming")
                }
            }
        }
        let a = crowd.agent(id).expect("agent");
        if let Some(t) = a.link_traversal() {
            assert_eq!(a.state(), CrowdAgentState::OffMeshLink);
            assert_eq!(a.state().name(), "off_mesh_link");
            assert!(
                off_segment(a.position(), t.origin, t.start).min(off_segment(a.position(), t.start, t.end))
                    < 1e-4,
                "on the link: {:?}",
                a.position()
            );
            assert_eq!(t.total_steps, 60, "1 s at 60 Hz");
            assert!((t.duration() - 1.0).abs() < 1e-4);
            assert_eq!(a.velocity(), Vec3::ZERO);
        }
        if a.state() == CrowdAgentState::Arrived {
            break;
        }
    }
    let (s, f) = (started.expect("took off"), finished.expect("landed"));
    assert_eq!(f - s, 60, "landed 60 steps after take-off");
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert!(a.position().x > 3.5);
    assert_eq!(crowd.stats().link_traversals, 1);
}

#[test]
fn an_automatic_duration_is_length_over_speed() {
    let mut link = gap_link();
    link.duration = 0.0;
    let mesh = mesh_with(link);
    let (_, _, events) = cross(&mesh, NavCrowdConfig::default(), 600);
    let s = events
        .iter()
        .find(|e| e.1 == NavLinkPhase::Started)
        .expect("took off")
        .0;
    let f = events
        .iter()
        .find(|e| e.1 == NavLinkPhase::Finished)
        .expect("landed")
        .0;
    // 3 m at 3 m/s: 1 s, 60 steps (the take-off point is the link's start).
    assert_eq!(f - s, 60);
}

#[test]
fn instant_mode_crosses_in_one_step() {
    let mesh = mesh_with(gap_link());
    let config = NavCrowdConfig {
        link_traversal: NavLinkTraversalMode::Instant,
        ..Default::default()
    };
    let (crowd, id, events) = cross(&mesh, config, 600);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].0, events[1].0, "same step");
    assert_eq!(
        (events[0].1, events[1].1),
        (NavLinkPhase::Started, NavLinkPhase::Finished)
    );
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn a_filtered_agent_finds_no_path() {
    let mesh = mesh_with(gap_link().with_flags(4));
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut p = agent();
    p.link_filter = NavQueryFilter {
        exclude_flags: 4,
        ..Default::default()
    };
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), p)
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);
    // The default filter takes the link.
    let other = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 2.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, other, Vec3::new(4.0, 0.0, 2.0))
        .expect("target");
    assert_eq!(
        crowd.agent(other).expect("agent").state(),
        CrowdAgentState::Walking
    );
}

#[test]
fn a_crossing_agent_is_not_pushed_and_replays_are_identical() {
    let mesh = mesh_with(gap_link());
    let run = || {
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        let id = crowd
            .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
            .expect("on the mesh");
        // A bystander standing right at the take-off point.
        crowd
            .add_agent(&mesh, Vec3::new(-1.4, 0.0, 0.2), agent())
            .expect("on the mesh");
        crowd
            .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
            .expect("target");
        let mut digests = Vec::new();
        let mut crossed = 0;
        for _ in 0..600 {
            crowd.step(&mesh);
            digests.push(crowd.digest());
            let a = crowd.agent(id).expect("agent");
            if let Some(t) = a.link_traversal() {
                crossed += 1;
                assert!(
                    (a.position() - t.position()).length() < 1e-5,
                    "exactly on schedule"
                );
            }
        }
        assert!(crossed > 0, "the agent crossed");
        (digests, crowd.agent(id).expect("agent").state())
    };
    let (a, state) = run();
    let (b, _) = run();
    assert_eq!(a, b, "bit-identical replays");
    assert_eq!(state, CrowdAgentState::Arrived);
}

#[test]
fn targets_changed_mid_crossing_apply_on_landing() {
    let mesh = mesh_with(gap_link());
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut k = 0;
    while crowd.agent(id).expect("agent").link_traversal().is_none() {
        crowd.step(&mesh);
        k += 1;
        assert!(k < 600, "reaches the link");
    }
    // A new target on the landing side: still on the link, then walking there.
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 3.0))
        .expect("target");
    assert_eq!(
        crowd.agent(id).expect("agent").state(),
        CrowdAgentState::OffMeshLink
    );
    for _ in 0..600 {
        crowd.step(&mesh);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert!(
        (a.position() - Vec3::new(4.0, 0.0, 3.0)).length() < 0.2,
        "{:?}",
        a.position()
    );

    // Clearing the target mid-crossing: the agent lands and stands.
    crowd
        .set_target(&mesh, id, Vec3::new(-4.0, 0.0, 0.0))
        .expect("target");
    let mut k = 0;
    while crowd.agent(id).expect("agent").link_traversal().is_none() {
        crowd.step(&mesh);
        k += 1;
        assert!(k < 600, "reaches the link back");
    }
    crowd.clear_target(id).expect("clears");
    assert_eq!(
        crowd.agent(id).expect("agent").state(),
        CrowdAgentState::OffMeshLink
    );
    for _ in 0..120 {
        crowd.step(&mesh);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Idle);
    assert!(a.link_traversal().is_none());
    assert!(a.position().x < -1.0, "landed on the left: {:?}", a.position());
}

fn cache() -> NavTileCache {
    NavTileCache::build(
        NavTileCacheParams {
            build: params(),
            tile_size: 8,
        },
        &islands(),
    )
    .expect("the islands build")
}

#[test]
fn a_tile_cache_obstacle_on_a_landing_blocks_the_link_until_it_moves() {
    let mut cache = cache();
    cache.set_off_mesh_links(vec![gap_link()]);
    let route = |c: &NavTileCache| {
        c.query()
            .find_path(
                Vec3::new(-4.0, 0.0, 0.0),
                Vec3::new(4.0, 0.0, 0.0),
                Vec3::splat(1.0),
            )
            .is_some()
    };
    assert!(route(&cache));
    let id = cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: Vec3::new(1.6, 0.0, 0.0),
            radius: 0.3,
            height: 1.0,
        })
        .expect("valid");
    cache.update();
    assert_eq!(cache.mesh().link_statuses()[0].state, NavLinkState::Blocked);
    assert!(!route(&cache));
    let queued = cache
        .move_obstacle(
            id,
            NavObstacleShape::Cylinder {
                base: Vec3::new(4.0, 0.0, 3.0),
                radius: 0.3,
                height: 1.0,
            },
        )
        .expect("moves");
    assert!(!queued.is_empty());
    assert_eq!(cache.pending_tiles(), queued, "the old and new tiles are queued");
    cache.update();
    assert!(matches!(
        cache.mesh().link_statuses()[0].state,
        NavLinkState::Active { .. }
    ));
    assert!(route(&cache));
    assert_eq!(cache.obstacles().count(), 1);
    assert_eq!(
        cache.obstacles().next().map(|(i, _)| i),
        Some(id),
        "the id is kept"
    );
    // The links survive the tile rebuilds and can be switched off on the cache.
    assert_eq!(cache.off_mesh_links().len(), 1);
    assert_eq!(cache.set_link_enabled(5, false), 1);
    assert!(!route(&cache));
}

#[test]
fn move_obstacle_rebuilds_only_the_old_and_new_tiles() {
    let mut cache = cache();
    let id = cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-5.0, -1.0, -3.5),
            max: Vec3::new(-4.5, 1.0, -3.0),
        })
        .expect("valid");
    cache.update();
    let before: Vec<u64> = tiles(&cache)
        .map(|(x, z)| cache.tile_rebuild_count(x, z).unwrap_or(0))
        .collect();
    let old = cache.touched_tiles(id).expect("known");
    let queued = cache
        .move_obstacle(
            id,
            NavObstacleShape::Box {
                min: Vec3::new(4.5, -1.0, 3.0),
                max: Vec3::new(5.0, 1.0, 3.5),
            },
        )
        .expect("moves");
    let new = cache.touched_tiles(id).expect("known");
    let done = cache.update();
    let mut expect: Vec<(u32, u32)> = old.iter().chain(&new).copied().collect();
    expect.sort_by_key(|&(x, z)| (z, x));
    expect.dedup();
    let mut rebuilt = done.rebuilt.clone();
    rebuilt.sort_by_key(|&(x, z)| (z, x));
    assert_eq!(rebuilt, expect);
    let mut q = queued.clone();
    q.sort_by_key(|&(x, z)| (z, x));
    assert_eq!(q, expect);
    for ((x, z), b) in tiles(&cache).zip(before) {
        let after = cache.tile_rebuild_count(x, z).unwrap_or(0);
        let touched = expect.contains(&(x, z));
        assert_eq!(after - b, u64::from(touched), "tile ({x}, {z})");
    }
    let e = cache
        .move_obstacle(
            regia_nav::NavObstacleId(99),
            NavObstacleShape::Box {
                min: Vec3::ZERO,
                max: Vec3::ONE,
            },
        )
        .expect_err("unknown");
    assert_eq!(e.code(), "nav_unknown_obstacle");
    // audit sprint-audit-g-nav-perf H2: the stitch copies the rebuilt tiles' cells, not the grid.
    let full: usize = tiles(&cache).map(|(x, z)| tile_cells(&cache, x, z)).sum();
    let dirty: usize = expect.iter().map(|&(x, z)| tile_cells(&cache, x, z)).sum();
    let copied = cache.cells_copied();
    assert!(dirty > 0 && dirty < full, "dirty {dirty} cells, full grid {full}");
    assert_eq!(
        copied, dirty,
        "copied {copied} cells; the rebuilt tiles hold {dirty} and the grid holds {full}"
    );
}

/// Cells in tile `(tx, tz)`, from its world bounds and the cache cell size.
fn tile_cells(cache: &NavTileCache, tx: u32, tz: u32) -> usize {
    let (min, max) = cache.tile_bounds(tx, tz).expect("tile");
    let c = cache.params().build.cell_size;
    let w = ((max.x - min.x) / c).round() as usize;
    let h = ((max.z - min.z) / c).round() as usize;
    w * h
}

fn tiles(cache: &NavTileCache) -> impl Iterator<Item = (u32, u32)> {
    let (tx, tz) = cache.tile_grid();
    (0..tz).flat_map(move |z| (0..tx).map(move |x| (x, z)))
}

#[test]
fn an_oriented_box_blocks_its_footprint_not_its_bounding_box_corners() {
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: params(),
            tile_size: 16,
        },
        &rect(-6.0, -6.0, 6.0, 6.0, 0.0),
    )
    .expect("the floor builds");
    // A 4 x 0.4 m plank turned 45 degrees: its bounding box is about 3.1 m square.
    cache
        .add_obstacle(NavObstacleShape::OrientedBox {
            center: Vec3::new(0.0, 0.5, 0.0),
            half_extents: Vec3::new(2.0, 0.5, 0.2),
            yaw: std::f32::consts::FRAC_PI_4,
        })
        .expect("valid");
    cache.update();
    let q = cache.query();
    let near = |x: f32, z: f32| {
        q.find_nearest(Vec3::new(x, 0.0, z), Vec3::new(0.05, 1.0, 0.05))
            .is_some()
    };
    // A positive yaw turns +X toward -Z: the plank runs from (-1.4, 1.4) to (1.4, -1.4).
    assert!(!near(0.0, 0.0), "the centre is blocked");
    assert!(!near(1.0, -1.0), "along the plank is blocked");
    assert!(near(1.2, 1.2), "the empty corner of the bounding box stays open");
    assert!(near(-1.2, -1.2), "the other empty corner stays open");
    let invalid = cache.add_obstacle(NavObstacleShape::OrientedBox {
        center: Vec3::ZERO,
        half_extents: Vec3::new(1.0, 1.0, 0.0),
        yaw: 0.0,
    });
    assert_eq!(invalid.expect_err("flat").code(), "nav_invalid_obstacle");
    let invalid = cache.add_obstacle(NavObstacleShape::OrientedBox {
        center: Vec3::ZERO,
        half_extents: Vec3::ONE,
        yaw: f32::NAN,
    });
    assert_eq!(invalid.expect_err("nan").code(), "nav_invalid_obstacle");
}

#[test]
fn a_crossing_walks_to_the_take_off_then_along_the_link() {
    let crossing = |steps| NavLinkTraversal {
        link: 0,
        user_id: 5,
        kind: NavLinkKind::Jump,
        origin: Vec3::new(-1.0, 0.0, 0.0),
        start: Vec3::ZERO,
        end: Vec3::new(3.0, 0.0, 0.0),
        steps,
        total_steps: 4,
        step_seconds: 0.25,
        arc: None,
        walk_seconds: 0.0,
    };
    // One metre of walk and three of link in four steps: one metre per step.
    assert_eq!(crossing(0).position(), Vec3::new(-1.0, 0.0, 0.0));
    assert_eq!(crossing(1).position(), Vec3::ZERO);
    assert_eq!(crossing(2).position(), Vec3::new(1.0, 0.0, 0.0));
    assert_eq!(crossing(4).position(), Vec3::new(3.0, 0.0, 0.0));
    // An agent already at the take-off, and a link with no length, stay well defined.
    let mut still = crossing(2);
    still.origin = still.start;
    assert_eq!(still.position(), Vec3::new(1.5, 0.0, 0.0));
    still.end = still.start;
    assert_eq!(still.position(), still.end);
}

#[test]
fn one_advance_keeps_the_events_of_all_its_steps() {
    // A crossing of two steps inside frames of four: the take-off and the landing both happen on
    // a step that is not the last of its `advance`.
    let config = NavCrowdConfig::default();
    let frame = config.step_seconds * 4.0;
    let mut link = gap_link();
    link.duration = config.step_seconds * 2.0;
    let mesh = mesh_with(link);
    let mut crowd = NavCrowd::new(config).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut phases = Vec::new();
    for _ in 0..150 {
        crowd.advance(&mesh, frame).expect("finite");
        phases.extend(crowd.link_events().iter().map(|e| e.phase));
    }
    assert_eq!(phases, [NavLinkPhase::Started, NavLinkPhase::Finished]);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn an_agent_does_not_pop_at_take_off_or_landing() {
    let mesh = mesh_with(gap_link());
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut last = crowd.agent(id).expect("agent").position();
    let mut largest = 0.0f32;
    for _ in 0..600 {
        crowd.step(&mesh);
        let now = crowd.agent(id).expect("agent").position();
        largest = largest.max((now - last).length());
        last = now;
    }
    assert_eq!(crowd.stats().link_traversals, 1);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    // 3 m/s on foot and 3.8 m over the one-second crossing are about 0.06 m per step.
    assert!(largest < 0.1, "the largest move in one step was {largest} m");
}

#[test]
fn a_tile_rebuild_during_a_crossing_lands_on_the_new_polygons() {
    let mut cache = cache();
    cache.set_off_mesh_links(vec![gap_link()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    crowd.sync_mesh_revision(cache.revision());
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    for _ in 0..600 {
        crowd.step(cache.mesh());
        if crowd.agent(id).expect("agent").link_traversal().is_some() {
            break;
        }
    }
    assert!(crowd.agent(id).expect("agent").link_traversal().is_some());

    // An obstacle far from the link adds polygons to an early tile, so the later indices shift
    // and the polygon the crossing was planned to land on now has another number.
    let polys = cache.mesh().polys.len();
    cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: Vec3::new(-5.0, 0.0, -3.0),
            radius: 0.3,
            height: 1.0,
        })
        .expect("valid");
    cache.update();
    assert_ne!(cache.mesh().polys.len(), polys, "the polygons were renumbered");
    // The number the crossing still holds now names a polygon far from the landing.
    let landing = Vec3::new(1.5, 0.0, 0.0);
    let held = crowd.agent(id).expect("agent").corridor()[0] as usize;
    let stale = cache.mesh().polys[held].closest_point_xz(landing.x, landing.z);
    assert!((stale - landing).length() > 2.0, "{stale:?}");
    crowd.sync_mesh_revision(cache.revision());

    let mut landed = None;
    for _ in 0..600 {
        crowd.step(cache.mesh());
        for e in crowd.link_events() {
            if e.phase == NavLinkPhase::Finished {
                landed.get_or_insert(e.position);
            }
        }
    }
    let landed = landed.expect("landed");
    assert_eq!(crowd.stats().link_traversals, 1, "one crossing, not a second try");
    assert!(
        (landed - landing).length() < 0.6,
        "landed at the link's end, not on a stale polygon: {landed:?}"
    );
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn switching_a_link_back_on_replans_agents_with_no_path() {
    let mut cache = cache();
    cache.set_off_mesh_links(vec![gap_link()]);
    assert_eq!(cache.set_link_enabled(5, false), 1);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    crowd.sync_mesh_revision(cache.revision());
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    crowd.step(cache.mesh());
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);

    // No tile was rebuilt, yet the graph changed: the revision tells the crowd.
    let revision = cache.revision();
    assert_eq!(cache.set_link_enabled(5, true), 1);
    assert_ne!(cache.revision(), revision);
    crowd.sync_mesh_revision(cache.revision());
    for _ in 0..600 {
        crowd.step(cache.mesh());
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);

    // A switch that changes nothing leaves the revision alone.
    let revision = cache.revision();
    assert_eq!(cache.set_link_enabled(5, true), 0);
    assert_eq!(cache.revision(), revision);
}

/// Steps `crowd` until `id` is on a link.
fn until_crossing(crowd: &mut NavCrowd, mesh: &NavMesh, id: CrowdAgentId) {
    for _ in 0..600 {
        crowd.step(mesh);
        if crowd.agent(id).expect("agent").link_traversal().is_some() {
            return;
        }
    }
    panic!("the agent never reached the link");
}

#[test]
fn a_walking_agent_stops_for_a_blocked_link_and_goes_when_it_clears() {
    let mut cache = cache();
    cache.set_off_mesh_links(vec![gap_link()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    crowd.sync_mesh_revision(cache.revision());
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    crowd.step(cache.mesh());
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Walking);

    // A crate lands on the far end while the agent is on its way.
    let obstacle = cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: Vec3::new(1.6, 0.0, 0.0),
            radius: 0.3,
            height: 1.0,
        })
        .expect("valid");
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    for _ in 0..240 {
        crowd.step(cache.mesh());
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);
    assert_eq!(
        crowd.stats().link_traversals,
        0,
        "it did not use the blocked link"
    );

    cache.remove_obstacle(obstacle).expect("known");
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    for _ in 0..600 {
        crowd.step(cache.mesh());
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    assert_eq!(crowd.stats().link_traversals, 1);
}

#[test]
fn a_crossing_goes_on_when_its_link_is_switched_off() {
    let mut mesh = mesh_with(gap_link());
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    until_crossing(&mut crowd, &mesh, id);
    assert_eq!(mesh.set_link_enabled(5, false), 1);
    for _ in 0..600 {
        crowd.step(&mesh);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(crowd.stats().link_traversals, 1);
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert!(a.position().x > 3.5, "{:?}", a.position());
}

#[test]
fn an_obstacle_on_the_landing_during_a_crossing_does_not_strand_the_agent() {
    let mut cache = cache();
    cache.set_off_mesh_links(vec![gap_link()]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    crowd.sync_mesh_revision(cache.revision());
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    until_crossing(&mut crowd, cache.mesh(), id);

    cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: Vec3::new(1.6, 0.0, 0.0),
            radius: 0.3,
            height: 1.0,
        })
        .expect("valid");
    cache.update();
    assert_eq!(cache.mesh().link_statuses()[0].state, NavLinkState::Blocked);
    crowd.sync_mesh_revision(cache.revision());
    for _ in 0..600 {
        crowd.step(cache.mesh());
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(crowd.stats().link_traversals, 1, "one crossing");
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert!(a.position().x > 3.5, "{:?}", a.position());
}
