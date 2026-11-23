// SPDX-License-Identifier: MPL-2.0
//! Boundary and repeat-run tests added by the test-quality lane (ADR-1746): config and parameter
//! limits at their exact edges, error-code hygiene, id handling, the tile cache's queue and
//! bookkeeping, and the fixed-step accumulator. Every input is fixed; nothing reads a clock.

use regia_core::math::Vec3;
use regia_nav::{
    AvoidanceParams, CrowdAgentId, CrowdAgentParams, CrowdAgentState, MAX_CROWD_AGENTS,
    MAX_TILE_CACHE_OBSTACLES, MAX_TILE_SIZE, NavBuildError, NavCrowd, NavCrowdConfig, NavCrowdError, NavMesh,
    NavMeshBuildParams, NavMeshBuilder, NavObstacleId, NavObstacleShape, NavObstacles, NavTileCache,
    NavTileCacheError, NavTileCacheParams,
};

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

fn mesh() -> NavMesh {
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&floor(6.0))
    .expect("floor builds")
}

fn cache(tile_size: u32) -> NavTileCache {
    NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size,
        },
        &floor(6.0),
    )
    .expect("cache builds")
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

fn small_box(x: f32, z: f32) -> NavObstacleShape {
    NavObstacleShape::Box {
        min: Vec3::new(x, 0.0, z),
        max: Vec3::new(x + 0.5, 2.0, z + 0.5),
    }
}

#[test]
fn crowd_config_accepts_its_exact_limits_and_rejects_one_past() {
    let ok = |c: NavCrowdConfig| NavCrowd::new(c).is_ok();
    let base = NavCrowdConfig::default;
    assert!(
        ok(NavCrowdConfig {
            step_seconds: 1.0,
            ..base()
        }),
        "step 1.0 is the top"
    );
    assert!(ok(NavCrowdConfig {
        step_seconds: f32::MIN_POSITIVE,
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        step_seconds: 1.0 + f32::EPSILON,
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        step_seconds: -0.01,
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        step_seconds: f32::INFINITY,
        ..base()
    }));
    assert!(ok(NavCrowdConfig {
        max_agents: 1,
        ..base()
    }));
    assert!(ok(NavCrowdConfig {
        max_agents: MAX_CROWD_AGENTS,
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        max_agents: MAX_CROWD_AGENTS + 1,
        ..base()
    }));
    assert!(
        ok(NavCrowdConfig {
            snap_extents: Vec3::ZERO,
            ..base()
        }),
        "zero snap box is legal"
    );
    assert!(!ok(NavCrowdConfig {
        snap_extents: Vec3::new(-0.1, 1.0, 1.0),
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        snap_extents: Vec3::new(1.0, f32::NAN, 1.0),
        ..base()
    }));
    assert!(!ok(NavCrowdConfig {
        snap_extents: Vec3::new(1.0, 1.0, f32::INFINITY),
        ..base()
    }));
    assert!(
        ok(NavCrowdConfig {
            max_steps_per_advance: 0,
            ..base()
        }),
        "0 steps is accepted"
    );
}

#[test]
fn avoidance_pattern_limits_are_inclusive() {
    let with = |f: fn(&mut AvoidanceParams)| {
        let mut c = NavCrowdConfig::default();
        f(&mut c.avoidance);
        NavCrowd::new(c)
    };
    assert!(with(|a| a.divisions = 1).is_ok());
    assert!(with(|a| a.divisions = 32).is_ok());
    assert!(with(|a| a.divisions = 0).is_err());
    assert!(with(|a| a.divisions = 33).is_err());
    assert!(with(|a| a.rings = 4).is_ok());
    assert!(with(|a| a.rings = 5).is_err());
    assert!(with(|a| a.rings = 0).is_err());
    assert!(with(|a| a.depth = 8).is_ok());
    assert!(with(|a| a.depth = 9).is_err());
    assert!(with(|a| a.depth = 0).is_err());
    assert!(with(|a| a.velocity_bias = 1.0).is_ok());
    assert!(with(|a| a.velocity_bias = 0.0).is_ok());
    assert!(with(|a| a.velocity_bias = 1.0001).is_err());
    assert!(with(|a| a.velocity_bias = -0.0001).is_err());
    assert!(with(|a| a.weight_side = f32::NAN).is_err());
    assert!(with(|a| a.weight_impact = f32::INFINITY).is_err());
    assert!(with(|a| a.weight_current = -1.0).is_err());
    assert!(
        with(|a| a.weight_desired = 0.0).is_ok(),
        "a zero weight is allowed"
    );
    assert!(with(|a| a.horizon = f32::MIN_POSITIVE).is_ok());
    let e = with(|a| a.divisions = 0).expect_err("bad pattern");
    assert_eq!(e.code(), "nav_invalid_crowd_config");
}

#[test]
fn agent_params_new_scales_ranges_and_validates_each_field_on_its_boundary() {
    let p = CrowdAgentParams::new(0.5, 2.0, 4.0, 9.0);
    assert_eq!(p.collision_query_range, 6.0);
    assert_eq!(p.path_optimization_range, 15.0);
    assert_eq!(p.separation_weight, 2.0);
    assert!(p.avoidance && p.separation && p.optimize_visibility && p.anticipate_turns);
    assert_eq!(p.validate(), Ok(()));
    assert_eq!(CrowdAgentParams::default().validate(), Ok(()));

    // Zero is legal for the non-strict fields, illegal for the strict ones.
    for edit in [
        (|a: &mut CrowdAgentParams| a.max_speed = 0.0) as fn(&mut CrowdAgentParams),
        |a| a.max_acceleration = 0.0,
        |a| a.path_optimization_range = 0.0,
        |a| a.separation_weight = 0.0,
    ] {
        let mut q = agent();
        edit(&mut q);
        assert_eq!(q.validate(), Ok(()), "{q:?}");
    }
    for edit in [
        (|a: &mut CrowdAgentParams| a.radius = 0.0) as fn(&mut CrowdAgentParams),
        |a| a.radius = -0.5,
        |a| a.height = -1.0,
        |a| a.collision_query_range = -1.0,
        |a| a.path_optimization_range = -0.001,
        |a| a.separation_weight = -2.0,
        |a| a.separation_weight = f32::NAN,
        |a| a.path_optimization_range = f32::INFINITY,
        |a| a.collision_query_range = f32::NEG_INFINITY,
    ] {
        let mut q = agent();
        edit(&mut q);
        assert!(
            matches!(q.validate(), Err(NavCrowdError::InvalidAgentParams(_))),
            "{q:?}"
        );
    }
}

#[test]
fn every_error_code_is_unique_lowercase_protocol_text() {
    let crowd = [
        NavCrowdError::InvalidAgentParams("x"),
        NavCrowdError::InvalidConfig("x"),
        NavCrowdError::InvalidInput("x"),
        NavCrowdError::OffMesh,
        NavCrowdError::UnknownAgent(CrowdAgentId(1)),
        NavCrowdError::TooManyAgents,
    ];
    let tile = [
        NavTileCacheError::Build(NavBuildError::NoGeometry),
        NavTileCacheError::Build(NavBuildError::NoWalkableSurface),
        NavTileCacheError::Build(NavBuildError::InvalidInput("x")),
        NavTileCacheError::InvalidTileSize(0),
        NavTileCacheError::InvalidObstacle("x"),
        NavTileCacheError::UnknownObstacle(NavObstacleId(1)),
        NavTileCacheError::TooManyObstacles,
    ];
    let mut codes: Vec<&str> = crowd.iter().map(NavCrowdError::code).collect();
    codes.extend(tile.iter().map(NavTileCacheError::code));
    for c in &codes {
        assert!(
            !c.is_empty()
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{c}"
        );
        assert!(c.starts_with("nav_"), "{c}");
    }
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), codes.len(), "codes collide: {codes:?}");
    // Display text is never empty either (the agent protocol forwards it as `message`).
    for e in &crowd {
        assert!(!e.to_string().is_empty());
    }
    for e in &tile {
        assert!(!e.to_string().is_empty());
    }
    let names = [
        CrowdAgentState::Idle,
        CrowdAgentState::Walking,
        CrowdAgentState::Arrived,
        CrowdAgentState::NoPath,
    ]
    .map(|s| s.name());
    assert_eq!(names, ["idle", "walking", "arrived", "no_path"]);
}

#[test]
fn agent_ids_are_never_reused_and_stay_sorted_after_removals() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let a = crowd
        .add_agent(&mesh, Vec3::new(-2.0, 0.0, 0.0), agent())
        .expect("a");
    let b = crowd
        .add_agent(&mesh, Vec3::new(0.0, 0.0, 0.0), agent())
        .expect("b");
    let c = crowd
        .add_agent(&mesh, Vec3::new(2.0, 0.0, 0.0), agent())
        .expect("c");
    assert!(a < b && b < c);
    crowd.remove_agent(b).expect("remove middle");
    assert!(crowd.agent(b).is_none());
    assert_eq!(
        crowd.remove_agent(b).expect_err("twice"),
        NavCrowdError::UnknownAgent(b)
    );
    let d = crowd
        .add_agent(&mesh, Vec3::new(0.0, 0.0, 2.0), agent())
        .expect("d");
    assert!(d > c, "a removed id is not handed out again");
    let ids: Vec<_> = crowd.agents().iter().map(|x| x.id()).collect();
    assert_eq!(ids, vec![a, c, d]);
    assert_eq!(crowd.len(), 3);
    for id in [a, c, d] {
        assert_eq!(crowd.agent(id).expect("present").id(), id);
    }
}

#[test]
fn a_full_crowd_accepts_again_after_a_removal_and_failed_adds_leave_it_unchanged() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        max_agents: 2,
        ..Default::default()
    })
    .expect("config");
    let a = crowd
        .add_agent(&mesh, Vec3::new(-2.0, 0.0, 0.0), agent())
        .expect("a");
    crowd
        .add_agent(&mesh, Vec3::new(2.0, 0.0, 0.0), agent())
        .expect("b");
    let digest = crowd.digest();
    for _ in 0..3 {
        assert_eq!(
            crowd.add_agent(&mesh, Vec3::ZERO, agent()).expect_err("full"),
            NavCrowdError::TooManyAgents
        );
    }
    assert_eq!(crowd.digest(), digest, "failed adds change nothing");
    assert_eq!(crowd.len(), 2);
    crowd.remove_agent(a).expect("remove");
    assert!(crowd.add_agent(&mesh, Vec3::ZERO, agent()).is_ok());
    assert_eq!(crowd.len(), 2);
}

#[test]
fn set_agent_params_rejects_bad_params_without_touching_the_agent() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd.add_agent(&mesh, Vec3::ZERO, agent()).expect("agent");
    let mut bad = agent();
    bad.radius = f32::NAN;
    assert_eq!(
        crowd.set_agent_params(id, bad).expect_err("nan").code(),
        "nav_invalid_agent_params"
    );
    assert_eq!(*crowd.agent(id).expect("agent").params(), agent());
    let ghost = CrowdAgentId(77);
    assert_eq!(
        crowd.set_agent_params(ghost, agent()).expect_err("ghost"),
        NavCrowdError::UnknownAgent(ghost)
    );
    // Params are validated before the id lookup, so bad params on a ghost report the params.
    assert_eq!(
        crowd.set_agent_params(ghost, bad).expect_err("both").code(),
        "nav_invalid_agent_params"
    );
    let wide = CrowdAgentParams::new(0.6, 1.8, 2.0, 4.0);
    crowd.set_agent_params(id, wide).expect("valid");
    assert_eq!(*crowd.agent(id).expect("agent").params(), wide);
}

#[test]
fn clear_target_returns_to_idle_and_unknown_ids_are_typed() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("agent");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    assert_eq!(crowd.agent(id).expect("a").state(), CrowdAgentState::Walking);
    assert!(crowd.agent(id).expect("a").target().is_some());
    crowd.advance(&mesh, 0.5).expect("advance");
    crowd.clear_target(id).expect("clear");
    let a = crowd.agent(id).expect("a");
    assert_eq!(a.state(), CrowdAgentState::Idle);
    assert_eq!(a.target(), None);
    assert!(a.corners().is_empty());
    assert_eq!(a.corridor().len(), 1);
    // Clearing twice is fine; an unknown id is not.
    crowd.clear_target(id).expect("clear again");
    assert_eq!(
        crowd.clear_target(CrowdAgentId(99)).expect_err("ghost").code(),
        "nav_unknown_agent"
    );
    // The idle agent decelerates to a stop.
    for _ in 0..120 {
        crowd.advance(&mesh, 1.0 / 60.0).expect("advance");
    }
    assert!(crowd.agent(id).expect("a").velocity().length() < 1e-3);
}

#[test]
fn advance_accumulates_caps_and_drops_leftover_time() {
    let mesh = mesh();
    let step = 0.1_f32;
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        step_seconds: step,
        max_steps_per_advance: 3,
        ..Default::default()
    })
    .expect("config");
    assert_eq!(crowd.advance(&mesh, 0.0), Ok(0), "zero time runs nothing");
    assert_eq!(crowd.advance(&mesh, 0.05), Ok(0), "below one step is carried");
    assert_eq!(
        crowd.advance(&mesh, 0.06),
        Ok(1),
        "carry + new time crosses one step"
    );
    // A huge hitch is capped at 3 steps, and the extra time is dropped rather than queued.
    assert_eq!(crowd.advance(&mesh, 100.0), Ok(3));
    assert_eq!(
        crowd.advance(&mesh, 0.0),
        Ok(0),
        "dropped time does not come back"
    );
    assert_eq!(crowd.stats().steps, 4, "stats count the steps run");
}

#[test]
fn zero_max_steps_per_advance_runs_no_steps_and_never_loops_forever() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        max_steps_per_advance: 0,
        ..Default::default()
    })
    .expect("config");
    for _ in 0..3 {
        assert_eq!(crowd.advance(&mesh, 1.0), Ok(0));
    }
    assert_eq!(crowd.stats().steps, 0);
}

#[test]
fn digest_is_stable_for_an_empty_crowd_and_reacts_to_state() {
    let mesh = mesh();
    let empty = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    assert_eq!(
        empty.digest(),
        0xcbf2_9ce4_8422_2325,
        "empty digest is the FNV offset basis"
    );
    let mut a = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut b = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let ia = a.add_agent(&mesh, Vec3::new(1.0, 0.0, 1.0), agent()).expect("a");
    b.add_agent(&mesh, Vec3::new(1.0, 0.0, 1.0), agent()).expect("b");
    assert_eq!(a.digest(), b.digest());
    assert_ne!(a.digest(), empty.digest());
    a.set_target(&mesh, ia, Vec3::new(5.0, 0.0, 5.0)).expect("target");
    assert_ne!(a.digest(), b.digest(), "state change shows in the digest");
    a.advance(&mesh, 0.25).expect("advance");
    b.advance(&mesh, 0.25).expect("advance");
    assert_ne!(a.digest(), b.digest(), "a walking agent differs from an idle one");
}

#[test]
fn sync_mesh_revision_first_call_never_replans_and_repeat_values_are_noops() {
    let mesh = mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-3.0, 0.0, 0.0), agent())
        .expect("agent");
    crowd
        .set_target(&mesh, id, Vec3::new(3.0, 0.0, 0.0))
        .expect("target");
    let mut reference = crowd.clone_for_test(&mesh, id);
    crowd.sync_mesh_revision(u64::MAX);
    crowd.sync_mesh_revision(u64::MAX);
    reference.sync_mesh_revision(0);
    crowd.advance(&mesh, 0.5).expect("advance");
    reference.advance(&mesh, 0.5).expect("advance");
    assert_eq!(
        crowd.digest(),
        reference.digest(),
        "first revision values do not matter"
    );
}

/// Builds a second crowd that is in the same state as `crowd` (same agent, same target).
trait CloneForTest {
    fn clone_for_test(&self, mesh: &NavMesh, id: CrowdAgentId) -> NavCrowd;
}
impl CloneForTest for NavCrowd {
    fn clone_for_test(&self, mesh: &NavMesh, id: CrowdAgentId) -> NavCrowd {
        let a = self.agent(id).expect("agent");
        let mut c = NavCrowd::new(self.config().clone()).expect("config");
        c.add_agent(mesh, a.position(), *a.params()).expect("agent");
        c.set_target(mesh, id, a.target().expect("target"))
            .expect("target");
        c
    }
}

#[test]
fn tile_cache_rejects_every_out_of_range_tile_size() {
    let build = |tile_size| {
        NavTileCache::build(
            NavTileCacheParams {
                tile_size,
                ..Default::default()
            },
            &floor(2.0),
        )
    };
    for bad in [0, MAX_TILE_SIZE + 1, u32::MAX] {
        let e = build(bad).expect_err("bad tile size");
        assert_eq!(e, NavTileCacheError::InvalidTileSize(bad));
        assert_eq!(e.code(), "nav_invalid_tile_size");
    }
    assert!(build(1).is_ok(), "1-cell tiles are legal");
    assert!(build(MAX_TILE_SIZE).is_ok(), "the maximum is a single tile here");
    assert_eq!(build(MAX_TILE_SIZE).expect("one").tile_count(), 1);
}

#[test]
fn tile_cache_empty_and_flat_failures_carry_build_codes() {
    let e = NavTileCache::build(NavTileCacheParams::default(), &[]).expect_err("no geometry");
    assert_eq!(e.code(), "nav_no_geometry");
    let wall = [[
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.0, 5.0, 0.0),
        Vec3::new(0.0, 0.0, 5.0),
    ]];
    assert!(NavTileCache::build(NavTileCacheParams::default(), &wall).is_err());
    let nan = [[
        Vec3::new(f32::NAN, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
    ]];
    assert_eq!(
        NavTileCache::build(NavTileCacheParams::default(), &nan)
            .expect_err("nan")
            .code(),
        "nav_invalid_build_input"
    );
}

#[test]
fn tile_lookup_at_the_grid_edges_and_tile_boundaries() {
    let c = cache(16);
    let (tx, tz) = c.tile_grid();
    assert_eq!(c.tile_count(), (tx * tz) as usize);
    assert!(tx >= 2 && tz >= 2);
    // Every tile's min corner maps back to that tile and its bounds tile exactly.
    for z in 0..tz {
        for x in 0..tx {
            let (lo, hi) = c.tile_bounds(x, z).expect("bounds");
            assert_eq!(
                c.tile_at(Vec3::new(lo.x, 3.0, lo.z)),
                Some((x, z)),
                "min corner of ({x},{z})"
            );
            let mid = (lo + hi) * 0.5;
            assert_eq!(
                c.tile_at(Vec3::new(mid.x, -9.0, mid.z)),
                Some((x, z)),
                "y is ignored"
            );
            assert!(hi.x > lo.x && hi.z > lo.z);
        }
    }
    // The max edge of the last tile is one past the grid (half-open).
    let (_, last_hi) = c.tile_bounds(tx - 1, tz - 1).expect("last");
    assert_eq!(c.tile_at(Vec3::new(last_hi.x, 0.0, last_hi.z)), None);
    assert_eq!(
        c.tile_at(Vec3::new(last_hi.x - 0.01, 0.0, last_hi.z - 0.01)),
        Some((tx - 1, tz - 1))
    );
    // Out of range tile indices answer None everywhere.
    for (x, z) in [(tx, 0), (0, tz), (u32::MAX, 0), (0, u32::MAX)] {
        assert!(c.tile_bounds(x, z).is_none());
        assert!(c.tile_rebuild_count(x, z).is_none());
        assert!(c.tile_poly_count(x, z).is_none());
    }
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(c.tile_at(Vec3::new(bad, 0.0, 0.0)), None);
        assert_eq!(c.tile_at(Vec3::new(0.0, 0.0, bad)), None);
    }
}

#[test]
fn obstacle_ids_are_not_reused_and_double_removal_is_typed() {
    let mut c = cache(16);
    let a = c.add_obstacle(small_box(-1.0, -1.0)).expect("a");
    let b = c.add_obstacle(small_box(2.0, 2.0)).expect("b");
    assert!(a < b);
    assert_eq!(c.remove_obstacle(a), Ok(small_box(-1.0, -1.0)));
    assert_eq!(
        c.remove_obstacle(a).expect_err("twice"),
        NavTileCacheError::UnknownObstacle(a)
    );
    assert_eq!(c.touched_tiles(a), None);
    let d = c.add_obstacle(small_box(-4.0, 1.0)).expect("d");
    assert!(d > b, "ids only grow");
    let listed: Vec<_> = c.obstacles().map(|(id, _)| id).collect();
    assert_eq!(listed, vec![b, d], "listed in id order");
    assert_eq!(c.obstacle_count(), 2);
}

#[test]
fn tile_cache_obstacle_limit_is_exact_and_removal_frees_a_slot() {
    let mut c = cache(16);
    let mut ids = Vec::new();
    for _ in 0..MAX_TILE_CACHE_OBSTACLES {
        ids.push(c.add_obstacle(small_box(0.0, 0.0)).expect("within the limit"));
    }
    assert_eq!(c.obstacle_count(), MAX_TILE_CACHE_OBSTACLES);
    let e = c.add_obstacle(small_box(1.0, 1.0)).expect_err("full");
    assert_eq!(e, NavTileCacheError::TooManyObstacles);
    assert_eq!(e.code(), "nav_too_many_obstacles");
    // A full cache still reports an invalid shape as invalid, not as full (shape is checked first).
    let bad = NavObstacleShape::Box {
        min: Vec3::ZERO,
        max: Vec3::ZERO,
    };
    assert_eq!(
        c.add_obstacle(bad).expect_err("bad").code(),
        "nav_invalid_obstacle"
    );
    c.remove_obstacle(ids[10]).expect("remove");
    assert!(c.add_obstacle(small_box(1.0, 1.0)).is_ok());
    assert_eq!(c.obstacle_count(), MAX_TILE_CACHE_OBSTACLES);
}

#[test]
fn obstacle_shape_validation_edges() {
    let cyl = |radius, height| NavObstacleShape::Cylinder {
        base: Vec3::ZERO,
        radius,
        height,
    };
    assert!(cyl(f32::MIN_POSITIVE, f32::MIN_POSITIVE).validated().is_ok());
    for (r, h) in [
        (0.0, 1.0),
        (1.0, 0.0),
        (-1.0, 1.0),
        (1.0, -1.0),
        (f32::NAN, 1.0),
        (1.0, f32::INFINITY),
    ] {
        assert_eq!(
            cyl(r, h).validated().expect_err("bad").code(),
            "nav_invalid_obstacle",
            "r={r} h={h}"
        );
    }
    let nan_base = NavObstacleShape::Cylinder {
        base: Vec3::new(0.0, f32::NAN, 0.0),
        radius: 1.0,
        height: 1.0,
    };
    assert!(nan_base.validated().is_err());
    // Box corners in any order normalise; a zero-height box is fine (only the footprint counts).
    let flat = NavObstacleShape::Box {
        min: Vec3::new(2.0, 1.0, 2.0),
        max: Vec3::new(1.0, 1.0, 0.0),
    };
    assert_eq!(
        flat.validated(),
        Ok(NavObstacleShape::Box {
            min: Vec3::new(1.0, 1.0, 0.0),
            max: Vec3::new(2.0, 1.0, 2.0)
        })
    );
    for (min, max) in [
        (Vec3::ZERO, Vec3::new(1.0, 1.0, 0.0)),
        (Vec3::ZERO, Vec3::new(0.0, 1.0, 1.0)),
        (Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ONE),
        (Vec3::ZERO, Vec3::new(1.0, f32::INFINITY, 1.0)),
    ] {
        assert!(
            NavObstacleShape::Box { min, max }.validated().is_err(),
            "{min:?} {max:?}"
        );
    }
    // Bounds of a cylinder and an unordered box.
    let (lo, hi) = NavObstacleShape::Cylinder {
        base: Vec3::new(1.0, 2.0, 3.0),
        radius: 0.5,
        height: 4.0,
    }
    .bounds();
    assert_eq!((lo, hi), (Vec3::new(0.5, 2.0, 2.5), Vec3::new(1.5, 6.0, 3.5)));
}

#[test]
fn pending_queue_dedups_orders_and_drains_with_limits() {
    let mut c = cache(16);
    assert!(c.pending_tiles().is_empty());
    let id = c.add_obstacle(small_box(-5.5, -5.5)).expect("a");
    c.add_obstacle(small_box(-5.4, -5.4)).expect("same tile again");
    let pending = c.pending_tiles();
    let mut sorted = pending.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        pending.len(),
        sorted.len(),
        "no tile is queued twice: {pending:?}"
    );
    assert_eq!(c.touched_tiles(id).expect("known"), vec![(0, 0)]);
    let revision = c.revision();
    // Zero-limit update rebuilds nothing, keeps the queue and the revision.
    let none = c.update_limited(0);
    assert!(none.rebuilt.is_empty());
    assert_eq!(none.remaining, pending.len());
    assert_eq!(c.pending_tiles(), pending);
    assert_eq!(c.revision(), revision);
    assert_eq!(c.total_rebuilds(), 0);
    let done = c.update();
    assert_eq!(done.rebuilt, pending);
    assert_eq!(done.remaining, 0);
    assert_eq!(c.revision(), revision + 1);
    assert_eq!(c.total_rebuilds(), pending.len() as u64);
    assert_eq!(c.tile_rebuild_count(0, 0), Some(1));
    // An update with an empty queue does not bump the revision.
    c.update();
    assert_eq!(c.revision(), revision + 1);
}

#[test]
fn an_obstacle_spanning_tiles_queues_each_once_in_ascending_order() {
    let mut c = cache(16);
    let id = c
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-5.9, 0.0, -5.9),
            max: Vec3::new(5.9, 2.0, 5.9),
        })
        .expect("covers the grid");
    let (tx, tz) = c.tile_grid();
    let touched = c.touched_tiles(id).expect("known");
    assert_eq!(touched.len() as u32, tx * tz, "every tile is touched");
    let up = c.update_limited(2);
    assert_eq!(up.rebuilt.len(), 2);
    assert_eq!(up.rebuilt, vec![(0, 0), (1, 0)], "lowest tile index first");
    assert_eq!(up.remaining as u32, tx * tz - 2);
    c.update();
    assert!(c.pending_tiles().is_empty());
}

#[test]
fn cylinder_obstacle_blocks_a_round_hole_not_its_bounding_box_corners() {
    let mut c = cache(64);
    c.add_obstacle(NavObstacleShape::Cylinder {
        base: Vec3::new(0.0, 0.0, 0.0),
        radius: 2.0,
        height: 2.0,
    })
    .expect("cyl");
    c.update();
    let q = c.query();
    let ext = Vec3::new(0.05, 1.0, 0.05);
    assert!(
        q.find_nearest(Vec3::new(0.0, 0.0, 0.0), ext).is_none(),
        "centre is blocked"
    );
    assert!(
        q.find_nearest(Vec3::new(1.4, 0.0, 0.0), ext).is_none(),
        "inside the radius is blocked"
    );
    // The bounding-box corner (1.9, 1.9) is 2.69 m from the centre: outside the cylinder (plus the
    // agent-radius inflate of < 0.6 m), so it must still be walkable.
    assert!(
        q.find_nearest(Vec3::new(1.95, 0.0, 1.95), ext).is_some(),
        "corner stays open"
    );
    assert!(
        q.find_nearest(Vec3::new(4.5, 0.0, 0.0), ext).is_some(),
        "far side stays open"
    );
}

#[test]
fn identical_obstacle_histories_give_identical_meshes() {
    let history = |c: &mut NavTileCache| {
        let a = c.add_obstacle(small_box(-1.0, 0.0)).expect("a");
        let b = c.add_obstacle(small_box(2.0, 2.0)).expect("b");
        c.update_limited(1);
        c.remove_obstacle(a).expect("remove");
        c.update();
        c.add_obstacle(small_box(-4.0, -4.0)).expect("c");
        c.update();
        let _ = b;
    };
    let (mut x, mut y) = (cache(16), cache(16));
    history(&mut x);
    history(&mut y);
    assert_eq!(x.revision(), y.revision());
    assert_eq!(x.total_rebuilds(), y.total_rebuilds());
    let (mx, my) = (x.mesh(), y.mesh());
    assert_eq!(mx.poly_count(), my.poly_count());
    for (p, q) in mx.polys.iter().zip(&my.polys) {
        assert_eq!(p.min, q.min);
        assert_eq!(p.max, q.max);
    }
}

#[test]
fn nav_obstacles_ids_are_unique_and_never_recycled() {
    let mut set = NavObstacles::default();
    let a = set.add_box(Vec3::ZERO, Vec3::ONE);
    let b = set.add_box(Vec3::ZERO, Vec3::ONE);
    assert_ne!(a, b);
    assert!(set.remove(a));
    assert!(!set.remove(a), "second removal reports false");
    let c = set.add_box(Vec3::ZERO, Vec3::ONE);
    assert!(c != a && c != b, "a removed id is not reused");
    assert_eq!(set.obstacle_count(), 2);
}

// ---- off-mesh links, polygon geometry and ORCA ----

use regia_nav::{NavPoly, NavQuery, OffMeshLink, orca};

fn quad(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
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

/// Two 4 m x 4 m islands, 4 m apart on the x axis (no walkable connection between them).
fn islands() -> NavMesh {
    let tris: Vec<[Vec3; 3]> = quad(-6.0, -2.0, -2.0, 2.0)
        .into_iter()
        .chain(quad(2.0, -2.0, 6.0, 2.0))
        .collect();
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("islands build")
}

const EXT: Vec3 = Vec3::new(1.0, 1.0, 1.0);

fn link(from: Vec3, to: Vec3, bidirectional: bool) -> OffMeshLink {
    OffMeshLink {
        bidirectional,
        area: 0,
        ..OffMeshLink::new(from, to)
    }
}

fn path_exists(mesh: &NavMesh, a: Vec3, b: Vec3) -> bool {
    NavQuery {
        mesh,
        obstacles: None,
    }
    .find_path(a, b, EXT)
    .is_some()
}

#[test]
fn islands_are_unreachable_without_a_link() {
    let m = islands();
    assert!(!path_exists(
        &m,
        Vec3::new(-4.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0)
    ));
    assert!(path_exists(
        &m,
        Vec3::new(-5.0, 0.0, -1.0),
        Vec3::new(-3.0, 0.0, 1.0)
    ));
}

#[test]
fn links_with_unusable_endpoints_are_ignored_not_fatal() {
    let a = Vec3::new(-4.0, 0.0, 0.0);
    let b = Vec3::new(4.0, 0.0, 0.0);
    let bad = [
        link(Vec3::new(f32::NAN, 0.0, 0.0), b, true),
        link(a, Vec3::new(0.0, f32::INFINITY, 0.0), true),
        link(a, Vec3::new(0.0, 0.0, 0.0), true), // lands in the gap, off both islands
        link(a, Vec3::new(4.0, 30.0, 0.0), true), // far above the island
        link(Vec3::new(100.0, 0.0, 0.0), b, false), // starts nowhere
    ];
    for l in bad {
        let m = islands().with_off_mesh_links(vec![l]);
        assert!(!path_exists(&m, a, b), "{l:?} must not connect the islands");
        // The query still works on the island the start is on.
        assert!(path_exists(&m, a, Vec3::new(-3.0, 0.0, 1.0)));
    }
}

#[test]
fn one_way_and_two_way_links_differ_only_in_the_reverse_direction() {
    let a = Vec3::new(-3.0, 0.0, 0.0);
    let b = Vec3::new(3.0, 0.0, 0.0);
    let one = islands().with_off_mesh_links(vec![link(a, b, false)]);
    assert!(path_exists(&one, a, b));
    assert!(!path_exists(&one, b, a));
    let two = islands().with_off_mesh_links(vec![link(a, b, true)]);
    assert!(path_exists(&two, a, b));
    assert!(path_exists(&two, b, a));
    // The path across a link includes both link endpoints as waypoints.
    let p = NavQuery {
        mesh: &two,
        obstacles: None,
    }
    .find_path(Vec3::new(-5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.0), EXT)
    .expect("path");
    assert_eq!(p.polys.len(), 2);
    assert!(
        p.waypoints.iter().any(|w| (*w - a).length() < 1e-3),
        "{:?}",
        p.waypoints
    );
    assert!(
        p.waypoints.iter().any(|w| (*w - b).length() < 1e-3),
        "{:?}",
        p.waypoints
    );
}

#[test]
fn a_link_within_one_island_does_not_break_paths_there() {
    let m = islands().with_off_mesh_links(vec![link(
        Vec3::new(-5.0, 0.0, 0.0),
        Vec3::new(-3.0, 0.0, 0.0),
        true,
    )]);
    let q = NavQuery {
        mesh: &m,
        obstacles: None,
    };
    let p = q
        .find_path(Vec3::new(-5.5, 0.0, 0.0), Vec3::new(-2.5, 0.0, 0.0), EXT)
        .expect("path");
    assert!(p.waypoints.len() >= 2);
    assert!(p.waypoints.iter().all(|w| w.is_finite()));
}

#[test]
fn repeated_path_queries_are_bit_identical() {
    let m = islands().with_off_mesh_links(vec![
        link(Vec3::new(-3.0, 0.0, -1.0), Vec3::new(3.0, 0.0, -1.0), true),
        link(Vec3::new(-3.0, 0.0, 1.0), Vec3::new(3.0, 0.0, 1.0), true),
    ]);
    let q = NavQuery {
        mesh: &m,
        obstacles: None,
    };
    let first = q
        .find_path(Vec3::new(-5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.5), EXT)
        .expect("path");
    for _ in 0..5 {
        let again = q
            .find_path(Vec3::new(-5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.5), EXT)
            .expect("path");
        assert_eq!(again.polys, first.polys);
        assert_eq!(again.waypoints, first.waypoints);
    }
}

fn square_poly() -> NavPoly {
    NavPoly {
        vertices: vec![
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(2.0, 1.0, 0.0),
            Vec3::new(2.0, 1.0, 2.0),
            Vec3::new(0.0, 1.0, 2.0),
        ],
        area: 0,
        portals: Vec::new(),
        min: Vec3::ZERO,
        max: Vec3::new(2.0, 2.0, 2.0),
    }
}

#[test]
fn poly_geometry_on_the_boundary_corners_and_with_nonfinite_points() {
    let p = square_poly();
    assert_eq!(p.height(), 1.0);
    assert_eq!(p.center_xz(), Vec3::new(1.0, 0.0, 1.0));
    assert!(p.contains_xz(1.0, 1.0));
    assert!(!p.contains_xz(-0.01, 1.0));
    assert!(!p.contains_xz(2.01, 1.0));
    assert!(!p.contains_xz(f32::NAN, 1.0), "NaN is never inside");
    assert!(!p.contains_xz(1.0, f32::NAN));
    // Outside points clamp to the nearest edge or corner, at the polygon's height.
    assert_eq!(p.closest_point_xz(5.0, 1.0), Vec3::new(2.0, 1.0, 1.0));
    assert_eq!(p.closest_point_xz(-3.0, -4.0), Vec3::new(0.0, 1.0, 0.0));
    assert_eq!(p.closest_point_xz(1.0, 1.0), Vec3::new(1.0, 1.0, 1.0));
    assert_eq!(p.distance_sq_xz(1.0, 1.0), 0.0);
    assert_eq!(p.distance_sq_xz(5.0, 2.0), 9.0);
    assert_eq!(p.distance_sq_xz(-3.0, -4.0), 25.0);
    // The distance and the closest point agree.
    let c = p.closest_point_xz(7.0, -2.0);
    let d = (c.x - 7.0).powi(2) + (c.z + 2.0).powi(2);
    assert!((d - p.distance_sq_xz(7.0, -2.0)).abs() < 1e-4);
}

#[test]
fn a_vertexless_poly_has_neutral_geometry() {
    let p = NavPoly {
        vertices: Vec::new(),
        area: 0,
        portals: Vec::new(),
        min: Vec3::ZERO,
        max: Vec3::ZERO,
    };
    assert_eq!(p.height(), 0.0);
    assert_eq!(p.center_xz(), Vec3::ZERO);
    assert!(!p.contains_xz(0.0, 0.0));
    assert!(p.closest_point_xz(1.0, 2.0).is_finite());
    assert_eq!(p.distance_sq_xz(1.0, 2.0), f32::INFINITY);
}

fn neighbor(x: f32, z: f32, vx: f32, vz: f32) -> (Vec3, Vec3, f32) {
    (Vec3::new(x, 0.0, z), Vec3::new(vx, 0.0, vz), 0.5)
}

#[test]
fn orca_without_neighbours_returns_the_preferred_velocity_clamped_to_max_speed() {
    let v = orca::orca_velocity(
        Vec3::ZERO,
        Vec3::ZERO,
        Vec3::new(1.0, 0.0, 0.0),
        0.5,
        3.0,
        2.0,
        &[],
    );
    assert!((v - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-5);
    let v = orca::orca_velocity(
        Vec3::ZERO,
        Vec3::ZERO,
        Vec3::new(30.0, 0.0, 40.0),
        0.5,
        3.0,
        2.0,
        &[],
    );
    assert!((v.length() - 3.0).abs() < 1e-4, "{v:?}");
    assert!(
        (v.normalize() - Vec3::new(0.6, 0.0, 0.8)).length() < 1e-4,
        "direction is kept"
    );
    let v = orca::orca_velocity(
        Vec3::ZERO,
        Vec3::ZERO,
        Vec3::new(1.0, 0.0, 0.0),
        0.5,
        -2.0,
        2.0,
        &[],
    );
    assert_eq!(v, Vec3::ZERO, "a negative top speed means standing still");
    let v = orca::orca_velocity(
        Vec3::ZERO,
        Vec3::ZERO,
        Vec3::new(1.0, 0.0, 0.0),
        0.5,
        0.0,
        2.0,
        &[],
    );
    assert_eq!(v, Vec3::ZERO);
}

#[test]
fn orca_survives_zero_and_negative_time_horizons() {
    let n = [neighbor(2.0, 0.0, -1.0, 0.0)];
    for h in [0.0, -1.0, 1e-9, f32::MIN_POSITIVE] {
        let v = orca::orca_velocity(
            Vec3::ZERO,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            0.5,
            2.0,
            h,
            &n,
        );
        assert!(v.is_finite(), "h={h}: {v:?}");
        assert!(v.length() <= 2.0 + 1e-4, "h={h}: {v:?}");
    }
}

#[test]
fn orca_skips_non_finite_neighbours_and_keeps_the_good_ones() {
    let good = neighbor(1.5, 0.0, -1.0, 0.0);
    let bad = [
        (Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ZERO, 0.5),
        (Vec3::new(1.0, 0.0, 0.0), Vec3::new(f32::INFINITY, 0.0, 0.0), 0.5),
        (Vec3::new(1.0, 0.0, 1.0), Vec3::ZERO, f32::NAN),
    ];
    let run = |n: &[(Vec3, Vec3, f32)]| {
        orca::orca_velocity(
            Vec3::ZERO,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            0.5,
            2.0,
            2.0,
            n,
        )
    };
    let alone = run(&[good]);
    let mut mixed = bad.to_vec();
    mixed.push(good);
    assert_eq!(run(&mixed), alone, "bad neighbours do not change the answer");
    assert_eq!(run(&bad), run(&[]), "only bad neighbours act like none");
    assert!(alone.is_finite());
}

#[test]
fn orca_head_on_pair_steers_apart_symmetrically_and_replays_identically() {
    let step = |me: Vec3, other: Vec3, vme: Vec3, vo: Vec3, goal: Vec3| {
        orca::orca_velocity(me, vme, goal, 0.4, 2.0, 3.0, &[(other, vo, 0.4)])
    };
    let (mut pa, mut pb) = (Vec3::new(-4.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 0.0));
    let (mut va, mut vb) = (Vec3::new(1.5, 0.0, 0.0), Vec3::new(-1.5, 0.0, 0.0));
    let mut trace = Vec::new();
    let mut closest = f32::INFINITY;
    for _ in 0..200 {
        let na = step(pa, pb, va, vb, Vec3::new(2.0, 0.0, 0.0));
        let nb = step(pb, pa, vb, va, Vec3::new(-2.0, 0.0, 0.0));
        (va, vb) = (na, nb);
        pa += va * 0.05;
        pb += vb * 0.05;
        closest = closest.min((pa - pb).length());
        trace.push((pa.to_array(), pb.to_array()));
    }
    assert!(
        closest > 0.8 - 1e-2,
        "agents of radius 0.4 never overlap: {closest}"
    );
    assert!(
        pa.x > 3.0 && pb.x < -3.0,
        "both got past each other: {pa:?} {pb:?}"
    );
    assert!(pa.z.abs() > 1e-4, "they sidestepped");
    assert!((pa.z + pb.z).abs() < 1e-3, "mirror image: {} {}", pa.z, pb.z);
    assert!(
        trace
            .iter()
            .all(|(a, b)| a.iter().chain(b).all(|c| c.is_finite()))
    );
}

// ---- the mesh changing under a running crowd ----

fn grid_mesh(rects: &[[f32; 4]]) -> NavMesh {
    let tris: Vec<[Vec3; 3]> = rects.iter().flat_map(|r| quad(r[0], r[1], r[2], r[3])).collect();
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("builds")
}

#[test]
fn stepping_on_a_smaller_unrelated_mesh_without_a_replan_request_does_not_panic() {
    // A host that swaps the mesh and forgets request_replan_all must get a degraded crowd, never
    // a panic from a stale polygon index.
    let big = grid_mesh(&[
        [-8.0, -8.0, 8.0, 8.0],
        [10.0, -8.0, 14.0, 8.0],
        [-8.0, 10.0, 8.0, 14.0],
    ]);
    let small = grid_mesh(&[[-1.0, -1.0, 1.0, 1.0]]);
    assert!(small.poly_count() < big.poly_count());
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let ids: Vec<_> = [(-6.0, -6.0), (12.0, 0.0), (0.0, 12.0)]
        .iter()
        .map(|&(x, z)| {
            crowd
                .add_agent(&big, Vec3::new(x, 0.0, z), agent())
                .expect("agent")
        })
        .collect();
    for (i, id) in ids.iter().enumerate() {
        let goal = [(6.0, 6.0), (12.0, 6.0), (6.0, 12.0)][i];
        crowd
            .set_target(&big, *id, Vec3::new(goal.0, 0.0, goal.1))
            .expect("target");
    }
    crowd.advance(&big, 0.5).expect("advance");
    for _ in 0..60 {
        crowd
            .advance(&small, 1.0 / 30.0)
            .expect("advance on the small mesh");
    }
    for a in crowd.agents() {
        assert!(
            a.position().is_finite() && a.velocity().is_finite(),
            "{:?}",
            a.position()
        );
    }
}

#[test]
fn replan_all_after_a_mesh_swap_resnaps_agents_onto_the_new_mesh() {
    let old = grid_mesh(&[[-6.0, -6.0, 6.0, 6.0]]);
    let new = grid_mesh(&[[-6.0, -6.0, 1.0, 6.0]]); // the east part is gone
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&old, Vec3::new(2.0, 0.0, 0.0), agent())
        .expect("agent");
    crowd.request_replan_all();
    crowd.advance(&new, 0.25).expect("advance");
    let pos = crowd.agent(id).expect("agent").position();
    assert!(pos.is_finite());
    assert!(pos.x <= 1.0, "pulled back onto the remaining floor: {pos:?}");
}

#[test]
fn a_target_that_becomes_unreachable_after_the_swap_ends_in_no_path_not_a_panic() {
    let old = grid_mesh(&[[-6.0, -6.0, 6.0, 6.0]]);
    let new = islands();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&old, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("agent");
    crowd
        .set_target(&old, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    crowd.request_replan_all();
    for _ in 0..30 {
        crowd.advance(&new, 1.0 / 30.0).expect("advance");
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::NoPath);
    assert!(crowd.stats().path_failures >= 1);
    assert!(
        a.position().x < -1.0,
        "stayed on the west island: {:?}",
        a.position()
    );
}

// ---- the builder ----

fn build_with(f: impl FnOnce(&mut NavMeshBuildParams), tris: &[[Vec3; 3]]) -> Result<NavMesh, NavBuildError> {
    let mut p = NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    };
    f(&mut p);
    NavMeshBuilder::new(p).build(tris)
}

fn signature(m: &NavMesh) -> Vec<([u32; 3], [u32; 3], usize)> {
    m.polys
        .iter()
        .map(|p| {
            (
                p.min.to_array().map(f32::to_bits),
                p.max.to_array().map(f32::to_bits),
                p.portals.len(),
            )
        })
        .collect()
}

#[test]
fn the_same_soup_always_builds_the_same_mesh() {
    let tris = floor(5.0);
    let a = build_with(|_| {}, &tris).expect("a");
    let b = build_with(|_| {}, &tris).expect("b");
    assert_eq!(signature(&a), signature(&b));
    assert_eq!((a.min, a.max), (b.min, b.max));
}

#[test]
fn triangle_order_and_winding_do_not_change_the_walkable_area() {
    let tris = grid_tris(&[[-5.0, -5.0, 0.0, 5.0], [0.0, -5.0, 5.0, 5.0]]);
    let mut reversed = tris.clone();
    reversed.reverse();
    let mut flipped = tris.clone();
    for t in &mut flipped {
        t.swap(1, 2);
    }
    let area = |m: &NavMesh| -> f32 {
        m.polys
            .iter()
            .map(|p| (p.max.x - p.min.x) * (p.max.z - p.min.z))
            .sum()
    };
    let base = build_with(|_| {}, &tris).expect("base");
    let rev = build_with(|_| {}, &reversed).expect("reversed order");
    assert!((area(&base) - area(&rev)).abs() < 1e-3, "order changed the area");
    // A floor seen from below is still the same floor to a voxeliser that tests geometry, not
    // facing: the rasteriser turns every normal upward, so the flipped soup must build (finding 15
    // of sprint-deep-zai-nav-ai: an `if let Ok` here passed when the build was refused).
    let f = build_with(|_| {}, &flipped).expect("a flipped floor is still a floor");
    assert!((area(&base) - area(&f)).abs() < 1e-3, "winding changed the area");
}

fn grid_tris(rects: &[[f32; 4]]) -> Vec<[Vec3; 3]> {
    rects.iter().flat_map(|r| quad(r[0], r[1], r[2], r[3])).collect()
}

#[test]
fn non_finite_and_negative_agent_and_build_params_are_errors_or_safe() {
    let tris = floor(4.0);
    let cases: [fn(&mut NavMeshBuildParams); 6] = [
        |p| p.agent.radius = f32::NAN,
        |p| p.agent.height = f32::NAN,
        |p| p.agent.max_climb = f32::NAN,
        |p| p.agent.max_slope = f32::NAN,
        |p| p.border_size = f32::NAN,
        |p| p.border_size = -1.0,
    ];
    for (i, f) in cases.iter().enumerate() {
        // Must not panic or hang; an error or a finite mesh are both acceptable answers.
        match build_with(f, &tris) {
            Ok(m) => assert!(
                m.polys.iter().all(|p| p.min.is_finite() && p.max.is_finite()),
                "case {i}: non-finite polygon bounds"
            ),
            Err(e) => assert!(
                matches!(
                    e,
                    NavBuildError::InvalidInput(_) | NavBuildError::NoWalkableSurface
                ),
                "case {i}: {e:?}"
            ),
        }
    }
}

#[test]
fn a_floor_narrower_than_the_agent_has_no_walkable_surface() {
    let tris = grid_tris(&[[-0.3, -4.0, 0.3, 4.0]]);
    assert_eq!(
        build_with(|_| {}, &tris).expect_err("too thin"),
        NavBuildError::NoWalkableSurface
    );
    // The same strip is walkable for a point-sized agent.
    assert!(build_with(|p| p.agent.radius = 0.0, &tris).is_ok());
}

#[test]
fn a_slab_below_a_low_ceiling_is_not_walkable_but_the_floor_next_to_it_is() {
    let mut tris = grid_tris(&[[-6.0, -3.0, 6.0, 3.0]]);
    // A ceiling at 1.0 m over x in [0, 6]: less than the 1.6 m agent height.
    for t in quad(0.0, -3.0, 6.0, 3.0) {
        tris.push(t.map(|v| Vec3::new(v.x, 1.0, v.z)));
    }
    let m = build_with(|_| {}, &tris).expect("builds");
    let q = NavQuery {
        mesh: &m,
        obstacles: None,
    };
    // Tight in y: the ceiling slab's top face (y = 1) is itself a walkable floor and must not match.
    let tight = Vec3::new(0.3, 0.3, 0.3);
    assert!(
        q.find_nearest(Vec3::new(-3.0, 0.0, 0.0), tight).is_some(),
        "open floor"
    );
    assert!(
        q.find_nearest(Vec3::new(3.0, 0.0, 0.0), tight).is_none(),
        "under the low ceiling"
    );
}

#[test]
fn a_ramp_is_walkable_below_the_slope_limit_and_not_above() {
    let ramp = |rise: f32| -> Vec<[Vec3; 3]> {
        // 4 m run along x, `rise` metres up.
        vec![
            [
                Vec3::new(0.0, 0.0, -2.0),
                Vec3::new(4.0, rise, -2.0),
                Vec3::new(4.0, rise, 2.0),
            ],
            [
                Vec3::new(0.0, 0.0, -2.0),
                Vec3::new(4.0, rise, 2.0),
                Vec3::new(0.0, 0.0, 2.0),
            ],
        ]
    };
    let gentle = build_with(|p| p.cell_height = 0.05, &ramp(1.0)).expect("14 degree ramp");
    assert!(gentle.poly_count() > 0);
    let wall = build_with(|p| p.cell_height = 0.05, &ramp(8.0));
    assert!(
        matches!(wall, Err(NavBuildError::NoWalkableSurface)),
        "a 63 degree slope is above the 45 degree limit: {:?}",
        wall.map(|m| m.poly_count())
    );
}

#[test]
fn a_grid_too_large_to_allocate_is_an_error_not_an_abort() {
    let tris = grid_tris(&[[-1.0e6, -1.0e6, 1.0e6, 1.0e6]]);
    let e = build_with(|_| {}, &tris).expect_err("too big");
    assert!(matches!(e, NavBuildError::InvalidInput(_)), "{e:?}");
}

#[test]
fn degenerate_inputs_of_the_extreme_kind() {
    // All vertices identical, collinear triangles, and a tiny floor under a coarse voxel.
    let pt = Vec3::new(1.0, 0.0, 1.0);
    let e = build_with(|_| {}, &[[pt, pt, pt]]);
    assert!(e.is_err(), "a point has no walkable area");
    let line = [[Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), Vec3::new(2.0, 0.0, 0.0)]];
    assert!(build_with(|_| {}, &line).is_err());
    let tiny = grid_tris(&[[0.0, 0.0, 0.01, 0.01]]);
    // A floor smaller than one coarse voxel either fails cleanly or yields finite bounds.
    if let Ok(m) = build_with(|p| p.cell_size = 2.0, &tiny) {
        assert!(m.min.is_finite() && m.max.is_finite());
    }
}
