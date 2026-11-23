// SPDX-License-Identifier: MPL-2.0
//! Crowd steering and the tile cache end to end (ADR-1484): agents crossing without overlapping,
//! bit-identical replays, corridors, walking around a block, replanning after a tile-cache
//! obstacle, and typed errors for nonsense input. Every run uses fixed steps and deterministic
//! inputs; nothing reads the wall clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavCrowdError, NavMesh,
    NavMeshBuildParams, NavMeshBuilder, NavObstacleShape, NavTileCache, NavTileCacheParams,
};

/// Two triangles covering the axis-aligned rectangle `[x0, x1] x [z0, z1]` at height `y`.
fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

fn build(params: NavMeshBuildParams, rects: &[[f32; 4]]) -> NavMesh {
    let tris: Vec<[Vec3; 3]> = rects
        .iter()
        .flat_map(|r| rect(r[0], r[1], r[2], r[3], 0.0))
        .collect();
    NavMeshBuilder::new(params)
        .build(&tris)
        .expect("the test floor builds")
}

fn params() -> NavMeshBuildParams {
    NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    }
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

/// The largest pairwise overlap `(r_i + r_j) - distance` in the crowd (0 when none overlap).
fn max_overlap(crowd: &NavCrowd) -> f32 {
    let agents = crowd.agents();
    let mut worst = 0.0f32;
    for (i, a) in agents.iter().enumerate() {
        for b in &agents[i + 1..] {
            let d = Vec3::new(
                a.position().x - b.position().x,
                0.0,
                a.position().z - b.position().z,
            )
            .length();
            worst = worst.max(a.params().radius + b.params().radius - d);
        }
    }
    worst
}

/// 50 agents on a circle, each sent to the opposite point: they all meet in the middle.
fn circle_swap(mesh: &NavMesh) -> (NavCrowd, Vec<CrowdAgentId>) {
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("valid config");
    let n = 50;
    let radius = 8.0;
    let mut ids = Vec::new();
    for k in 0..n {
        let a = k as f32 / n as f32 * std::f32::consts::TAU;
        let p = Vec3::new(a.cos() * radius, 0.0, a.sin() * radius);
        ids.push(crowd.add_agent(mesh, p, agent()).expect("on the floor"));
    }
    for (k, &id) in ids.iter().enumerate() {
        let a = k as f32 / n as f32 * std::f32::consts::TAU;
        let target = Vec3::new(-a.cos() * radius, 0.0, -a.sin() * radius);
        crowd.set_target(mesh, id, target).expect("target on the floor");
    }
    (crowd, ids)
}

/// Overlap allowed between two agents, metres. Collision resolution is a soft, few-iteration
/// push (like the upstream crowd), so a dense crossing may leave a sliver of overlap for a step;
/// this bounds it at ~3 % of an agent diameter.
const OVERLAP_EPSILON: f32 = 0.02;

#[test]
fn fifty_agents_crossing_do_not_overlap_and_all_arrive() {
    let mesh = build(params(), &[[-12.0, -12.0, 12.0, 12.0]]);
    let (mut crowd, ids) = circle_swap(&mesh);
    let mut worst = 0.0f32;
    let mut steps = 0;
    // 60 simulated seconds at 60 Hz at most.
    while steps < 3600 {
        crowd.step(&mesh);
        steps += 1;
        worst = worst.max(max_overlap(&crowd));
        if crowd
            .agents()
            .iter()
            .all(|a| a.state() == CrowdAgentState::Arrived)
        {
            break;
        }
    }
    let arrived = crowd
        .agents()
        .iter()
        .filter(|a| a.state() == CrowdAgentState::Arrived)
        .count();
    assert_eq!(
        arrived,
        ids.len(),
        "every agent reaches the opposite side ({steps} steps)"
    );
    println!("max overlap {worst} m, {steps} steps, {:?}", crowd.stats());
    assert!(
        worst <= OVERLAP_EPSILON,
        "max overlap {worst} m over {steps} steps"
    );
    for (k, &id) in ids.iter().enumerate() {
        let a = crowd.agent(id).expect("agent");
        let ang = k as f32 / ids.len() as f32 * std::f32::consts::TAU;
        let goal = Vec3::new(-ang.cos() * 8.0, 0.0, -ang.sin() * 8.0);
        let d = Vec3::new(a.position().x - goal.x, 0.0, a.position().z - goal.z).length();
        assert!(d <= a.params().radius, "agent {k} ends {d} m from its goal");
    }
    assert!(crowd.stats().velocity_samples > 0, "avoidance sampled velocities");
}

#[test]
fn two_identical_runs_are_bit_identical() {
    let mesh = build(params(), &[[-12.0, -12.0, 12.0, 12.0]]);
    let run = || {
        let (mut crowd, _) = circle_swap(&mesh);
        let mut digests = Vec::with_capacity(600);
        for _ in 0..600 {
            crowd.step(&mesh);
            digests.push(crowd.digest());
        }
        let bits: Vec<[u32; 6]> = crowd
            .agents()
            .iter()
            .map(|a| {
                let (p, v) = (a.position(), a.velocity());
                [
                    p.x.to_bits(),
                    p.y.to_bits(),
                    p.z.to_bits(),
                    v.x.to_bits(),
                    v.y.to_bits(),
                    v.z.to_bits(),
                ]
            })
            .collect();
        (digests, bits, crowd.stats())
    };
    let (d1, b1, s1) = run();
    let (d2, b2, s2) = run();
    assert_eq!(d1, d2, "per-step digests match");
    assert_eq!(b1, b2, "final positions and velocities match bit for bit");
    assert_eq!(s1, s2, "the same work was done");
    // The run did something: agents moved off their start digests.
    assert_ne!(d1.first(), d1.last());
}

#[test]
fn advance_runs_whole_fixed_steps_and_carries_the_remainder() {
    let mesh = build(params(), &[[-6.0, -6.0, 6.0, 6.0]]);
    // A step of 1/64 s and frame times in 1/128 s units are exact in binary floating point.
    let mut by_steps = NavCrowd::new(NavCrowdConfig {
        step_seconds: 1.0 / 64.0,
        ..Default::default()
    })
    .expect("config");
    let mut by_advance = by_steps.clone();
    for crowd in [&mut by_steps, &mut by_advance] {
        let id = crowd
            .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
            .expect("adds");
        crowd
            .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
            .expect("target");
    }
    for _ in 0..10 {
        by_steps.step(&mesh);
    }
    // 10 steps delivered as uneven frame times.
    let step = by_advance.config().step_seconds;
    let mut ran = 0;
    for dt in [step * 2.5, step * 0.5, step * 3.0, step * 4.0] {
        ran += by_advance.advance(&mesh, dt).expect("finite");
    }
    assert_eq!(ran, 10);
    assert_eq!(by_steps.stats().steps, by_advance.stats().steps);
    assert_eq!(by_steps.agents()[0].position(), by_advance.agents()[0].position());
}

#[test]
fn agents_reach_their_goals_along_an_l_shaped_corridor() {
    // A 2.5 m wide corridor: 12 m along X, then 11 m along Z.
    let mesh = build(
        params(),
        &[[-12.0, -1.25, 1.25, 1.25], [-1.25, -1.25, 1.25, 11.0]],
    );
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut goals = Vec::new();
    for k in 0..8 {
        let start = Vec3::new(-10.5 + k as f32 * 1.0, 0.0, 0.0);
        let id = crowd.add_agent(&mesh, start, agent()).expect("in the corridor");
        let goal = Vec3::new(0.0, 0.0, 9.5 - k as f32 * 1.0);
        crowd.set_target(&mesh, id, goal).expect("goal in the corridor");
        goals.push((id, goal));
    }
    let mut steps = 0;
    while steps < 4800
        && !crowd
            .agents()
            .iter()
            .all(|a| a.state() == CrowdAgentState::Arrived)
    {
        crowd.step(&mesh);
        steps += 1;
        for a in crowd.agents() {
            assert!(
                regia_nav::NavQuery {
                    mesh: &mesh,
                    obstacles: None
                }
                .find_nearest(a.position(), Vec3::new(0.01, 0.5, 0.01))
                .is_some(),
                "agent {:?} left the corridor at {:?}",
                a.id(),
                a.position()
            );
        }
    }
    for (id, goal) in goals {
        let a = crowd.agent(id).expect("agent");
        assert_eq!(
            a.state(),
            CrowdAgentState::Arrived,
            "agent {id} after {steps} steps at {:?}",
            a.position()
        );
        let d = Vec3::new(a.position().x - goal.x, 0.0, a.position().z - goal.z).length();
        assert!(d <= a.params().radius, "agent {id} is {d} m from its goal");
    }
}

#[test]
fn an_agent_walks_around_a_block() {
    // A 16 x 16 m floor with a 4 x 4 m hole (the block) in the middle.
    let mesh = build(
        params(),
        &[
            [-8.0, -8.0, 8.0, -2.0],
            [-8.0, 2.0, 8.0, 8.0],
            [-8.0, -2.0, -2.0, 2.0],
            [2.0, -2.0, 8.0, 2.0],
        ],
    );
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let a = crowd
        .add_agent(&mesh, Vec3::new(-6.0, 0.0, 0.3), agent())
        .expect("adds");
    let b = crowd
        .add_agent(&mesh, Vec3::new(6.0, 0.0, -0.3), agent())
        .expect("adds");
    crowd
        .set_target(&mesh, a, Vec3::new(6.0, 0.0, 0.3))
        .expect("goal");
    crowd
        .set_target(&mesh, b, Vec3::new(-6.0, 0.0, -0.3))
        .expect("goal");
    assert!(
        crowd.agent(a).expect("a").corridor().len() > 1,
        "the route bends around the block"
    );
    let mut steps = 0;
    while steps < 2400
        && !crowd
            .agents()
            .iter()
            .all(|x| x.state() == CrowdAgentState::Arrived)
    {
        crowd.step(&mesh);
        steps += 1;
        for x in crowd.agents() {
            let p = x.position();
            let inside = p.x.abs() < 2.0 && p.z.abs() < 2.0;
            assert!(!inside, "agent {} entered the block at {p:?}", x.id());
        }
    }
    for id in [a, b] {
        assert_eq!(
            crowd.agent(id).expect("agent").state(),
            CrowdAgentState::Arrived,
            "after {steps} steps"
        );
    }
}

#[test]
fn a_tile_cache_obstacle_reroutes_the_crowd_and_removal_restores_the_route() {
    let tris: Vec<[Vec3; 3]> = rect(-8.0, -8.0, 8.0, 8.0, 0.0).to_vec();
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: params(),
            tile_size: 16,
        },
        &tris,
    )
    .expect("builds");
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-6.0, 0.0, 0.0), agent())
        .expect("adds");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(6.0, 0.0, 0.0))
        .expect("goal");
    crowd.sync_mesh_revision(cache.revision());
    // A wall across the middle with a gap at the +Z end.
    let wall = cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-0.3, -0.5, -9.0),
            max: Vec3::new(0.3, 2.0, 4.0),
        })
        .expect("adds");
    let update = cache.update();
    assert!(!update.rebuilt.is_empty() && update.rebuilt.len() < cache.tile_count());
    crowd.sync_mesh_revision(cache.revision());
    let replans_before = crowd.stats().replans;
    let mut steps = 0;
    let mut max_z = f32::NEG_INFINITY;
    while steps < 2400 && crowd.agent(id).expect("agent").state() != CrowdAgentState::Arrived {
        crowd.step(cache.mesh());
        steps += 1;
        let p = crowd.agent(id).expect("agent").position();
        max_z = max_z.max(p.z);
        assert!(
            !(p.x.abs() < 0.3 && p.z < 4.0),
            "the agent crossed the wall at {p:?}"
        );
    }
    assert!(
        crowd.stats().replans > replans_before,
        "the revision change replanned the corridor"
    );
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    assert!(max_z > 4.0, "the agent went through the gap (max z {max_z})");

    // Remove the wall and walk back: the straight route is open again.
    cache.remove_obstacle(wall).expect("removes");
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    crowd
        .set_target(cache.mesh(), id, Vec3::new(-6.0, 0.0, 0.0))
        .expect("goal");
    let mut max_dev = 0.0f32;
    let mut steps = 0;
    while steps < 2400 && crowd.agent(id).expect("agent").state() != CrowdAgentState::Arrived {
        crowd.step(cache.mesh());
        steps += 1;
        max_dev = max_dev.max(crowd.agent(id).expect("agent").position().z.abs());
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    let end = crowd.agent(id).expect("agent").position();
    assert!(end.x < -5.5, "back at the start side: {end:?}");
    assert!(
        max_dev < 1.0,
        "the return trip is straight again, no detour through the old gap ({max_dev})"
    );
}

#[test]
fn an_unreachable_target_is_no_path_not_an_error() {
    // Two floors with a gap between them.
    let mesh = build(params(), &[[-8.0, -2.0, -1.0, 2.0], [1.0, -2.0, 8.0, 2.0]]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-5.0, 0.0, 0.0), agent())
        .expect("adds");
    crowd
        .set_target(&mesh, id, Vec3::new(5.0, 0.0, 0.0))
        .expect("the target is on the mesh");
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);
    let start = crowd.agent(id).expect("agent").position();
    for _ in 0..60 {
        crowd.step(&mesh);
    }
    assert_eq!(
        crowd.agent(id).expect("agent").position(),
        start,
        "a stranded agent stays put"
    );
    assert_eq!(crowd.stats().path_failures, crowd.stats().replans);
}

#[test]
fn zero_radius_nan_and_off_mesh_inputs_are_typed_errors() {
    let mesh = build(params(), &[[-6.0, -6.0, 6.0, 6.0]]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let p = Vec3::new(0.0, 0.0, 0.0);
    let mut bad = agent();
    bad.radius = 0.0;
    let e = crowd.add_agent(&mesh, p, bad).expect_err("zero radius");
    assert!(matches!(e, NavCrowdError::InvalidAgentParams(_)));
    assert_eq!(e.code(), "nav_invalid_agent_params");
    for edit in [
        |a: &mut CrowdAgentParams| a.radius = f32::NAN,
        |a: &mut CrowdAgentParams| a.height = 0.0,
        |a: &mut CrowdAgentParams| a.max_speed = f32::INFINITY,
        |a: &mut CrowdAgentParams| a.max_acceleration = -1.0,
        |a: &mut CrowdAgentParams| a.collision_query_range = 0.0,
    ] {
        let mut params = agent();
        edit(&mut params);
        let e = crowd.add_agent(&mesh, p, params).expect_err("invalid");
        assert_eq!(e.code(), "nav_invalid_agent_params", "{params:?}");
    }
    let e = crowd
        .add_agent(&mesh, Vec3::new(f32::NAN, 0.0, 0.0), agent())
        .expect_err("nan position");
    assert_eq!(e, NavCrowdError::InvalidInput("agent position must be finite"));
    assert_eq!(e.code(), "nav_invalid_input");
    let e = crowd
        .add_agent(&mesh, Vec3::new(100.0, 0.0, 0.0), agent())
        .expect_err("off mesh");
    assert_eq!(e, NavCrowdError::OffMesh);
    assert_eq!(e.code(), "nav_off_mesh");
    assert!(crowd.is_empty(), "no failed call added an agent");

    let id = crowd.add_agent(&mesh, p, agent()).expect("valid");
    let e = crowd
        .set_target(&mesh, id, Vec3::new(0.0, f32::NAN, 0.0))
        .expect_err("nan target");
    assert_eq!(e.code(), "nav_invalid_input");
    let e = crowd
        .set_target(&mesh, id, Vec3::new(0.0, 50.0, 0.0))
        .expect_err("target above the mesh");
    assert_eq!(e, NavCrowdError::OffMesh);
    let ghost = CrowdAgentId(999);
    assert_eq!(
        crowd.set_target(&mesh, ghost, p).expect_err("unknown"),
        NavCrowdError::UnknownAgent(ghost)
    );
    assert_eq!(
        crowd.remove_agent(ghost).expect_err("unknown").code(),
        "nav_unknown_agent"
    );
    for dt in [f32::NAN, -0.1, f32::INFINITY] {
        assert_eq!(
            crowd.advance(&mesh, dt).expect_err("bad dt").code(),
            "nav_invalid_input"
        );
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Idle);

    for config in [
        NavCrowdConfig {
            step_seconds: 0.0,
            ..Default::default()
        },
        NavCrowdConfig {
            step_seconds: f32::NAN,
            ..Default::default()
        },
        NavCrowdConfig {
            max_agents: 0,
            ..Default::default()
        },
    ] {
        let e = NavCrowd::new(config).expect_err("invalid config");
        assert_eq!(e.code(), "nav_invalid_crowd_config");
    }
    let mut config = NavCrowdConfig::default();
    config.avoidance.horizon = 0.0;
    assert_eq!(
        NavCrowd::new(config).expect_err("horizon").code(),
        "nav_invalid_crowd_config"
    );

    let mut small = NavCrowd::new(NavCrowdConfig {
        max_agents: 1,
        ..Default::default()
    })
    .expect("config");
    small.add_agent(&mesh, p, agent()).expect("first");
    assert_eq!(
        small
            .add_agent(&mesh, Vec3::new(1.0, 0.0, 0.0), agent())
            .expect_err("full"),
        NavCrowdError::TooManyAgents
    );
}

#[test]
fn an_idle_agent_is_pushed_aside_but_stays_on_the_mesh() {
    // A walker heads straight through an idle agent standing in the middle of a corridor.
    let mesh = build(params(), &[[-8.0, -1.5, 8.0, 1.5]]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let idle = crowd
        .add_agent(&mesh, Vec3::new(0.0, 0.0, 0.0), agent())
        .expect("adds");
    let walker = crowd
        .add_agent(&mesh, Vec3::new(-6.0, 0.0, 0.0), agent())
        .expect("adds");
    crowd
        .set_target(&mesh, walker, Vec3::new(6.0, 0.0, 0.0))
        .expect("goal");
    let mut steps = 0;
    while steps < 1800 && crowd.agent(walker).expect("walker").state() != CrowdAgentState::Arrived {
        crowd.step(&mesh);
        steps += 1;
        assert!(max_overlap(&crowd) <= OVERLAP_EPSILON);
    }
    assert_eq!(
        crowd.agent(walker).expect("walker").state(),
        CrowdAgentState::Arrived
    );
    assert_eq!(crowd.agent(idle).expect("idle").state(), CrowdAgentState::Idle);
    let p = crowd.agent(idle).expect("idle").position();
    assert!(
        regia_nav::NavQuery {
            mesh: &mesh,
            obstacles: None
        }
        .find_nearest(p, Vec3::new(0.01, 0.5, 0.01))
        .is_some(),
        "the idle agent is still on the mesh at {p:?}"
    );
}

/// audit sprint-audit-g-nav-perf H2: a rebuilt tile replans corridors that used it. An agent
/// whose corridor stays on another tile keeps the plan, including across the next step.
#[test]
fn a_tile_rebuild_replans_only_corridors_that_touch_it() {
    let tris = rect(-24.0, -8.0, 24.0, 8.0, 0.0);
    let mut cache = NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.5,
                ..Default::default()
            },
            tile_size: 16,
        },
        &tris,
    )
    .expect("floor");
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let far = crowd
        .add_agent(cache.mesh(), Vec3::new(-18.0, 0.0, 0.0), agent())
        .expect("far agent is on the floor");
    let cross = crowd
        .add_agent(cache.mesh(), Vec3::new(-2.0, 0.0, 0.0), agent())
        .expect("crossing agent is on the floor");
    crowd
        .set_target(cache.mesh(), far, Vec3::new(-14.0, 0.0, 0.0))
        .expect("far target");
    crowd
        .set_target(cache.mesh(), cross, Vec3::new(8.0, 0.0, 0.0))
        .expect("crossing target");
    assert!(!crowd.agent(far).expect("far").needs_replan());
    assert!(!crowd.agent(cross).expect("cross").needs_replan());
    // The first sync only records the revision. The obstacle update is the one that counts.
    crowd.sync_mesh_revision(cache.revision());
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(2.0, -1.0, -1.0),
            max: Vec3::new(4.0, 2.0, 1.0),
        })
        .expect("obstacle");
    let done = cache.update();
    assert!(!done.rebuilt.is_empty(), "the obstacle rebuilds a tile");
    let effect = cache
        .last_partial_revision()
        .expect("the rebuild records a revision")
        .clone();
    crowd.sync_tile_revision(&effect);
    let far_agent = crowd.agent(far).expect("far");
    assert!(
        !far_agent.needs_replan(),
        "corridor {:?} is off the rebuilt tiles {:?} but was marked; replaced {:?}",
        far_agent.corridor(),
        done.rebuilt,
        effect.replaced
    );
    let cross_agent = crowd.agent(cross).expect("cross");
    assert!(
        cross_agent.needs_replan(),
        "corridor {:?} crosses rebuilt tiles {:?} and must replan; replaced {:?}",
        cross_agent.corridor(),
        done.rebuilt,
        effect.replaced
    );
    let replans = crowd.stats().replans;
    crowd.step(cache.mesh());
    assert_eq!(
        crowd.stats().replans,
        replans + 1,
        "only the crossing agent replans (was {replans}, now {})",
        crowd.stats().replans
    );
    assert!(!crowd.agent(far).expect("far").needs_replan());
}

/// The 48 x 16 m floor of [`a_tile_rebuild_replans_only_corridors_that_touch_it`], in 8 m tiles.
fn tiled_floor() -> NavTileCache {
    let tris = rect(-24.0, -8.0, 24.0, 8.0, 0.0);
    NavTileCache::build(
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.5,
                ..Default::default()
            },
            tile_size: 16,
        },
        &tris,
    )
    .expect("floor")
}

/// What the crowd step of `NavCrowdPlugin` does with a tile cache: take the tile patch when
/// there is one, otherwise compare revisions.
fn plugin_sync(cache: &mut NavTileCache, crowd: &mut NavCrowd) {
    cache.update();
    if let Some(effect) = cache.take_partial() {
        crowd.sync_tile_revision(&effect);
    } else {
        crowd.sync_mesh_revision(cache.revision());
    }
}

#[test]
fn a_stranded_agent_replans_when_a_partial_rebuild_opens_its_way() {
    let mut cache = tiled_floor();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-18.0, 0.0, 0.0), agent())
        .expect("agent is on the floor");
    crowd.sync_mesh_revision(cache.revision());
    // A wall across the whole floor, far from the agent's own tile.
    let wall = cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(2.0, -1.0, -9.0),
            max: Vec3::new(4.0, 2.0, 9.0),
        })
        .expect("obstacle");
    plugin_sync(&mut cache, &mut crowd);
    crowd
        .set_target(cache.mesh(), id, Vec3::new(18.0, 0.0, 0.0))
        .expect("an unreachable target is not an error");
    crowd.step(cache.mesh());
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);
    // The wall goes: the rebuild is a partial patch on a tile the stranded agent's one-polygon
    // corridor does not touch. The agent must still try again.
    cache.remove_obstacle(wall).expect("remove");
    plugin_sync(&mut cache, &mut crowd);
    crowd.step(cache.mesh());
    let a = crowd.agent(id).expect("agent");
    assert_eq!(
        a.state(),
        CrowdAgentState::Walking,
        "stranded agent did not replan after the way opened; corridor {:?}",
        a.corridor()
    );
}

#[test]
fn a_partial_patch_after_an_unpatched_revision_replans_everyone() {
    let mut cache = tiled_floor();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let far = crowd
        .add_agent(cache.mesh(), Vec3::new(-18.0, 0.0, 0.0), agent())
        .expect("far agent is on the floor");
    crowd
        .set_target(cache.mesh(), far, Vec3::new(-14.0, 0.0, 0.0))
        .expect("far target");
    crowd.sync_mesh_revision(cache.revision());
    // Two changes in one frame: new area costs (not a tile patch: `set_area_costs` promises a
    // full replan) and an obstacle on a tile the agent's corridor does not touch.
    let mut costs = *cache.area_costs();
    costs.set(3, 100.0).expect("area 3 cost");
    cache.set_area_costs(costs);
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(2.0, -1.0, -1.0),
            max: Vec3::new(4.0, 2.0, 1.0),
        })
        .expect("obstacle");
    plugin_sync(&mut cache, &mut crowd);
    assert!(
        crowd.agent(far).expect("far").needs_replan(),
        "the area-cost revision was skipped by the later tile patch"
    );
}
