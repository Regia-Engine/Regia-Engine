// SPDX-License-Identifier: MPL-2.0
//! Crowd lane graphs (ADR-1934): routes over lanes, agents following and passing each other,
//! junctions, closed lanes, streaming and replays.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavLaneAgentId, NavLaneAgentParams,
    NavLaneAgentState, NavLaneCrowd, NavLaneCrowdConfig, NavLaneDef, NavLaneError, NavLaneGraph,
    NavLanePlugin, NavLaneSet, NavMeshBuildParams, NavMeshBuilder, NavTileCache, NavTileCacheParams,
};

fn lane(points: &[(f32, f32)]) -> NavLaneDef {
    let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
    NavLaneDef::new(&p)
}

/// Two parallel ways from a split at x = 10 to a merge at x = 30:
/// 0 → (1 short and slow, or 2 long and fast) → 3.
fn diamond() -> Vec<NavLaneDef> {
    let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    a.next = vec![1, 2];
    let mut slow = lane(&[(10.0, 0.0), (30.0, 0.0)]);
    slow.next = vec![3];
    slow.speed = 0.5;
    let mut fast = lane(&[(10.0, 0.0), (20.0, 5.0), (30.0, 0.0)]);
    fast.next = vec![3];
    fast.speed = 2.0;
    let out = lane(&[(30.0, 0.0), (40.0, 0.0)]);
    vec![a, slow, fast, out]
}

fn graph() -> NavLaneGraph {
    NavLaneGraph::build(&diamond(), 0.0).expect("valid lanes")
}

/// One straight lane 100 m long and `width` wide.
fn straight(width: f32) -> NavLaneGraph {
    let mut l = lane(&[(0.0, 0.0), (100.0, 0.0)]);
    l.width = width;
    NavLaneGraph::build(&[l], 0.0).expect("valid lanes")
}

fn crowd() -> NavLaneCrowd {
    NavLaneCrowd::new(NavLaneCrowdConfig::default()).expect("config")
}

fn steps(crowd: &mut NavLaneCrowd, graph: &NavLaneGraph, n: usize) {
    for _ in 0..n {
        crowd.step(graph);
    }
}

#[test]
fn stations_and_samples_follow_the_polyline() {
    let g = graph();
    let l = g.lane(2).expect("lane");
    let leg = (10.0f32 * 10.0 + 5.0 * 5.0).sqrt();
    assert!((l.length() - 2.0 * leg).abs() < 1e-4);
    let (p, dir) = l.sample(leg);
    assert!((p - Vec3::new(20.0, 0.0, 5.0)).length() < 1e-4, "{p:?}");
    assert!(dir.z < 0.0, "on the way down: {dir:?}");
    let (end, _) = l.sample(1e6);
    assert_eq!(end, Vec3::new(30.0, 0.0, 0.0), "clamped to the lane");
    assert_eq!(l.sample(-5.0).0, Vec3::new(10.0, 0.0, 0.0));
}

#[test]
fn bad_lanes_are_typed_errors() {
    let code = |d: NavLaneDef| NavLaneGraph::build(&[d], 0.0).map(|_| ()).map_err(|e| e.code());
    assert_eq!(code(lane(&[(0.0, 0.0)])), Err("nav_invalid_lane"));
    assert_eq!(code(lane(&[(0.0, 0.0), (0.0, 0.0)])), Err("nav_invalid_lane"));
    assert_eq!(
        code(lane(&[(0.0, 0.0), (f32::NAN, 0.0)])),
        Err("nav_invalid_lane")
    );
    let mut d = lane(&[(0.0, 0.0), (1.0, 0.0)]);
    d.width = 0.0;
    assert_eq!(code(d.clone()), Err("nav_invalid_lane"));
    d.width = 2.0;
    d.speed = f32::INFINITY;
    assert_eq!(code(d.clone()), Err("nav_invalid_lane"));
    d.speed = 1.0;
    d.next = vec![1];
    assert_eq!(code(d), Err("nav_invalid_lane"));
    let ok = lane(&[(0.0, 0.0), (1.0, 0.0)]);
    assert!(
        NavLaneGraph::build(&[ok], -1.0).is_err(),
        "negative join distance"
    );
}

#[test]
fn lanes_join_by_distance_when_asked() {
    let a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    let b = lane(&[(10.2, 0.0), (20.0, 0.0)]);
    let c = lane(&[(11.0, 0.0), (20.0, 5.0)]);
    let defs = [a, b, c];
    let separate = NavLaneGraph::build(&defs, 0.0).expect("valid");
    assert!(separate.lane(0).expect("lane").next.is_empty());
    let joined = NavLaneGraph::build(&defs, 0.5).expect("valid");
    assert_eq!(joined.lane(0).expect("lane").next, vec![1]);
    let wider = NavLaneGraph::build(&defs, 1.5).expect("valid");
    assert_eq!(wider.lane(0).expect("lane").next, vec![1, 2]);
}

#[test]
fn a_route_prefers_the_quicker_way_not_the_shorter_one() {
    let g = graph();
    let r = g.find_route(0, 3, u16::MAX).expect("a route");
    assert_eq!(r.lanes, vec![0, 2, 3], "the long fast lane");
    let from_self = g.find_route(1, 1, u16::MAX).expect("a route");
    assert_eq!(from_self.lanes, vec![1]);
    assert_eq!(
        g.find_route(3, 0, u16::MAX).map(|_| ()).map_err(|e| e.code()),
        Err("nav_no_lane_route"),
        "lanes are one-way"
    );
    assert_eq!(
        g.find_route(0, 9, u16::MAX).map(|_| ()).map_err(|e| e.code()),
        Err("nav_unknown_lane")
    );
}

#[test]
fn a_route_respects_closed_lanes_and_flags() {
    let mut defs = diamond();
    defs[2].user_id = 7;
    defs[1].flags = 2;
    let mut g = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    assert_eq!(g.find_route(0, 3, 1).expect("a route").lanes, vec![0, 2, 3]);
    assert_eq!(g.set_lane_enabled(7, false), 1);
    assert_eq!(g.set_lane_enabled(7, false), 0, "already closed");
    assert!(matches!(g.find_route(0, 3, 1), Err(NavLaneError::NoRoute { .. })));
    assert_eq!(g.find_route(0, 3, 3).expect("a route").lanes, vec![0, 1, 3]);
}

#[test]
fn nearest_finds_the_lane_and_the_distance_along_it() {
    let g = straight(2.0);
    let (lane, along, away) = g.nearest(Vec3::new(30.0, 0.0, 1.0), 3.0, 1).expect("in reach");
    assert_eq!(lane, 0);
    assert!((along - 30.0).abs() < 1e-4);
    assert!((away - 1.0).abs() < 1e-4);
    assert!(g.nearest(Vec3::new(30.0, 0.0, 9.0), 3.0, 1).is_none());
    assert!(g.nearest(Vec3::new(30.0, 0.0, 1.0), 3.0, 2).is_none(), "flags");
}

#[test]
fn route_points_run_along_the_lanes_and_a_crowd_agent_can_walk_them() {
    let g = graph();
    let route = g.find_route(0, 3, u16::MAX).expect("a route");
    let points = g.route_points(&route, 0.0);
    assert_eq!(points.first(), Some(&Vec3::ZERO));
    assert_eq!(points.last(), Some(&Vec3::new(40.0, 0.0, 0.0)));
    assert_eq!(points.len(), 5, "shared ends once: {points:?}");
    let right = g.route_points(&route, 1.0);
    assert!(
        right[0].z > 0.5,
        "right of +X is +Z in this frame: {:?}",
        right[0]
    );

    let c = 44.0;
    let floor = vec![
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
    let build = NavMeshBuildParams {
        cell_size: 1.0,
        ..Default::default()
    };
    let mesh = NavMeshBuilder::new(build).build(&floor).expect("builds");
    assert!(
        g.check_on_mesh(&mesh, 1.0).is_empty(),
        "the lanes lie on the floor"
    );
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, points[0], CrowdAgentParams::default())
        .expect("on the mesh");
    for &p in &points[1..] {
        crowd.set_target(&mesh, id, p).expect("on the mesh");
        for _ in 0..1200 {
            crowd.step(&mesh);
            if crowd.agent(id).expect("agent").state() == CrowdAgentState::Arrived {
                break;
            }
        }
        assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
}

#[test]
fn a_lane_off_the_mesh_is_reported() {
    let c = 20.0;
    let floor = vec![
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
    let build = NavMeshBuildParams {
        cell_size: 1.0,
        ..Default::default()
    };
    let mesh = NavMeshBuilder::new(build).build(&floor).expect("builds");
    let g = graph();
    let gaps = g.check_on_mesh(&mesh, 1.0);
    assert_eq!(
        gaps.iter().map(|g| (g.lane, g.point)).collect::<Vec<_>>(),
        vec![(1, 1), (2, 2), (3, 0), (3, 1)],
        "every point past x = 20"
    );
}

#[test]
fn an_agent_walks_at_the_lower_of_its_speed_and_the_limit() {
    let mut defs = vec![lane(&[(0.0, 0.0), (100.0, 0.0)])];
    defs[0].speed = 1.0;
    let g = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    let mut c = crowd();
    let params = NavLaneAgentParams {
        speed: 3.0,
        ..Default::default()
    };
    let id = c.add_agent(&g, 0, 0.0, 0.0, params, 1).expect("valid agent");
    steps(&mut c, &g, 300);
    let d = c.agent(id).expect("agent").distance();
    assert!((d - 10.0).abs() < 1e-2, "10 s at 1 m/s: {d}");
}

#[test]
fn a_follower_on_a_narrow_lane_never_overlaps_the_one_ahead() {
    let g = straight(0.6);
    let mut c = crowd();
    let slow = NavLaneAgentParams {
        speed: 0.5,
        ..Default::default()
    };
    let fast = NavLaneAgentParams {
        speed: 2.0,
        ..Default::default()
    };
    let lead = c.add_agent(&g, 0, 5.0, 0.0, slow, 1).expect("valid agent");
    let tail = c.add_agent(&g, 0, 0.0, 0.0, fast, 2).expect("valid agent");
    let reach = 2.0 * slow.radius;
    for _ in 0..1800 {
        c.step(&g);
        let (a, b) = (c.agent(lead).expect("agent"), c.agent(tail).expect("agent"));
        assert!(
            a.distance() - b.distance() >= reach,
            "{} vs {}",
            a.distance(),
            b.distance()
        );
    }
    let tail = c.agent(tail).expect("agent");
    assert!(
        (tail.speed() - 0.5).abs() < 0.05,
        "settles behind at the leader's speed: {}",
        tail.speed()
    );
    assert!(c.stats().slowed > 0);
}

#[test]
fn a_wide_lane_lets_a_faster_agent_pass_and_keeps_it_on_the_lane() {
    let g = straight(3.0);
    let mut c = crowd();
    let slow = NavLaneAgentParams {
        speed: 0.5,
        ..Default::default()
    };
    let fast = NavLaneAgentParams {
        speed: 2.0,
        ..Default::default()
    };
    let lead = c.add_agent(&g, 0, 5.0, 0.0, slow, 1).expect("valid agent");
    let tail = c.add_agent(&g, 0, 0.0, 0.0, fast, 2).expect("valid agent");
    for _ in 0..900 {
        c.step(&g);
        let (a, b) = (c.agent(lead).expect("agent"), c.agent(tail).expect("agent"));
        assert!(b.offset().abs() <= 1.5 - fast.radius + 1e-4, "{}", b.offset());
        let along = a.distance() - b.distance();
        let aside = a.offset() - b.offset();
        let apart = (along * along + aside * aside).sqrt();
        assert!(
            apart >= slow.radius + fast.radius,
            "bodies touched: {along}, {aside}"
        );
    }
    assert!(
        c.agent(tail).expect("agent").distance() > c.agent(lead).expect("agent").distance(),
        "it got past"
    );
}

#[test]
fn a_queue_forms_behind_a_stopped_agent() {
    let g = straight(0.6);
    let mut c = crowd();
    let still = NavLaneAgentParams {
        speed: 0.0,
        ..Default::default()
    };
    c.add_agent(&g, 0, 20.0, 0.0, still, 1).expect("valid agent");
    let ids: Vec<NavLaneAgentId> = (0..5)
        .map(|i| {
            c.add_agent(&g, 0, i as f32, 0.0, NavLaneAgentParams::default(), i)
                .expect("valid agent")
        })
        .collect();
    steps(&mut c, &g, 1800);
    let gap = 2.0 * 0.3 + c.config().min_gap;
    for (k, id) in ids.iter().rev().enumerate() {
        let want = 20.0 - gap * (k as f32 + 1.0);
        let d = c.agent(*id).expect("agent").distance();
        assert!(
            (d - want).abs() < 0.1,
            "agent {k} of the queue at {d}, wanted {want}"
        );
    }
}

#[test]
fn a_destination_takes_its_route_and_stops_at_the_end() {
    let g = graph();
    let mut c = crowd();
    let params = NavLaneAgentParams {
        speed: 2.0,
        ..Default::default()
    };
    let id = c.add_agent(&g, 0, 0.0, 0.0, params, 1).expect("valid agent");
    c.set_destination(&g, id, 3).expect("a route");
    let mut seen = vec![0];
    for _ in 0..3000 {
        c.step(&g);
        for t in c.transitions() {
            assert_eq!(t.agent, id);
            seen.push(t.to);
        }
    }
    assert_eq!(seen, vec![0, 2, 3], "the planned lanes in order");
    let a = c.agent(id).expect("agent");
    assert_eq!(a.state(), NavLaneAgentState::Stopped);
    assert_eq!(a.distance(), 10.0);
    assert_eq!(c.position(&g, id), Some(Vec3::new(40.0, 0.0, 0.0)));
    assert_eq!(c.stats().stopped, 1);
}

#[test]
fn a_destination_on_the_agents_own_lane_stops_at_its_end() {
    let g = graph();
    let mut c = crowd();
    let id = c
        .add_agent(&g, 0, 9.0, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    c.set_destination(&g, id, 0).expect("a route");
    steps(&mut c, &g, 300);
    let a = c.agent(id).expect("agent");
    assert_eq!((a.lane(), a.state()), (0, NavLaneAgentState::Stopped));
}

#[test]
fn a_wanderer_turns_by_its_seed_and_clearing_a_route_sets_it_free() {
    let g = graph();
    let turn = |seed: u64| {
        let mut c = crowd();
        c.add_agent(&g, 0, 9.9, 0.0, NavLaneAgentParams::default(), seed)
            .expect("valid agent");
        steps(&mut c, &g, 30);
        c.agents()[0].lane()
    };
    let lanes: Vec<u32> = (0..16).map(turn).collect();
    assert!(
        lanes.contains(&1) && lanes.contains(&2),
        "both ways get used: {lanes:?}"
    );
    assert_eq!(
        lanes,
        (0..16).map(turn).collect::<Vec<_>>(),
        "the same seeds, the same turns"
    );

    let mut c = crowd();
    let id = c
        .add_agent(&g, 3, 9.9, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    steps(&mut c, &g, 30);
    assert_eq!(
        c.agent(id).map(|a| a.state()),
        Some(NavLaneAgentState::Stopped),
        "a dead end stops a wanderer"
    );
}

#[test]
fn a_closed_lane_stops_a_route_and_turns_a_wanderer_away() {
    let mut defs = diamond();
    defs[2].user_id = 7;
    let mut g = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    let mut c = crowd();
    let routed = c
        .add_agent(&g, 0, 9.5, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    c.set_destination(&g, routed, 3).expect("a route");
    let mut free = crowd();
    let wanderers: Vec<_> = (0..8)
        .map(|s| {
            free.add_agent(&g, 0, 9.0 - s as f32, 0.0, NavLaneAgentParams::default(), s)
                .expect("valid agent")
        })
        .collect();
    g.set_lane_enabled(7, false);
    steps(&mut c, &g, 120);
    assert_eq!(
        c.agent(routed).map(|a| a.state()),
        Some(NavLaneAgentState::Stopped),
        "its next lane closed"
    );
    // Lane 1 walks at 0.5 m/s, so the last of the eight clears the junction in about 20 s.
    steps(&mut free, &g, 900);
    for id in wanderers {
        assert_eq!(free.agent(id).map(|a| a.lane()), Some(1), "only lane 1 is open");
    }
}

#[test]
fn a_blocker_near_one_wall_is_passed_on_the_side_with_room() {
    // A stopped agent just right of the centre. The right side has no room to clear it, so the
    // walker on that side must cross over and pass on the left.
    let g = straight(2.0);
    let mut c = crowd();
    let still = NavLaneAgentParams {
        speed: 0.0,
        ..Default::default()
    };
    let block = c.add_agent(&g, 0, 10.0, 0.1, still, 1).expect("valid agent");
    let walker = c
        .add_agent(&g, 0, 0.0, 0.2, NavLaneAgentParams::default(), 2)
        .expect("valid agent");
    steps(&mut c, &g, 900);
    let (a, b) = (c.agent(walker).expect("agent"), c.agent(block).expect("agent"));
    assert!(a.distance() > b.distance(), "got past, at {}", a.distance());
}

#[test]
fn a_lane_too_narrow_to_pass_holds_the_agent_still_with_no_jitter() {
    let g = straight(1.0);
    let mut c = crowd();
    let still = NavLaneAgentParams {
        speed: 0.0,
        ..Default::default()
    };
    c.add_agent(&g, 0, 10.0, 0.0, still, 1).expect("valid agent");
    let walker = c
        .add_agent(&g, 0, 0.0, 0.1, NavLaneAgentParams::default(), 2)
        .expect("valid agent");
    steps(&mut c, &g, 900);
    let settled = c.agent(walker).expect("agent").offset();
    steps(&mut c, &g, 30);
    assert_eq!(c.agent(walker).expect("agent").offset(), settled);
}

#[test]
fn an_agent_stopped_at_a_closed_lane_walks_on_when_it_reopens() {
    let mut a = lane(&[(0.0, 0.0), (5.0, 0.0)]);
    a.next = vec![1];
    let mut b = lane(&[(5.0, 0.0), (9.0, 0.0)]);
    b.user_id = 7;
    let mut g = NavLaneGraph::build(&[a, b], 0.0).expect("valid lanes");
    g.set_lane_enabled(7, false);
    let mut c = crowd();
    let id = c
        .add_agent(&g, 0, 4.5, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    steps(&mut c, &g, 60);
    assert_eq!(c.agent(id).map(|a| a.state()), Some(NavLaneAgentState::Stopped));
    g.set_lane_enabled(7, true);
    steps(&mut c, &g, 60);
    let at = c.agent(id).expect("agent");
    assert_eq!((at.lane(), at.state()), (1, NavLaneAgentState::Walking));
}

#[test]
fn an_agent_on_a_closed_lane_can_still_be_sent_somewhere() {
    let mut defs = diamond();
    defs[0].user_id = 7;
    let mut g = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    let mut c = crowd();
    let id = c
        .add_agent(&g, 0, 9.0, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    g.set_lane_enabled(7, false);
    c.set_destination(&g, id, 3).expect("its next lanes are open");
    assert!(matches!(
        g.find_route(1, 0, u16::MAX),
        Err(NavLaneError::NoRoute { .. })
    ));
}

#[test]
fn the_plugin_keeps_real_time_at_any_step() {
    let walked = |step_seconds: f32| {
        let g = straight(2.0);
        let config = NavLaneCrowdConfig {
            step_seconds,
            ..Default::default()
        };
        let mut c = NavLaneCrowd::new(config).expect("config");
        let id = c
            .add_agent(&g, 0, 0.0, 0.0, NavLaneAgentParams::default(), 1)
            .expect("valid agent");
        let mut app = App::new();
        app.add_plugins(NavLanePlugin);
        app.insert_resource(g);
        app.insert_resource(c);
        for _ in 0..600 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        app.world()
            .resource::<NavLaneCrowd>()
            .agent(id)
            .expect("agent")
            .distance()
    };
    // Ten seconds of 60 Hz ticks at 1.4 m/s, at the default 1/30 and at 1/60.
    assert!((walked(1.0 / 30.0) - 14.0).abs() < 0.1, "{}", walked(1.0 / 30.0));
    assert!((walked(1.0 / 60.0) - 14.0).abs() < 0.1, "{}", walked(1.0 / 60.0));
}

#[test]
fn a_full_junction_holds_the_agent_behind_it() {
    let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    a.next = vec![1];
    a.width = 0.6;
    let mut b = lane(&[(10.0, 0.0), (20.0, 0.0)]);
    b.width = 0.6;
    let g = NavLaneGraph::build(&[a, b], 0.0).expect("valid lanes");
    let mut c = crowd();
    let still = NavLaneAgentParams {
        speed: 0.0,
        ..Default::default()
    };
    c.add_agent(&g, 1, 0.2, 0.0, still, 1).expect("valid agent");
    let id = c
        .add_agent(&g, 0, 8.0, 0.0, NavLaneAgentParams::default(), 2)
        .expect("valid agent");
    steps(&mut c, &g, 300);
    let at = c.agent(id).expect("agent");
    assert_eq!(at.lane(), 0, "it waits on its own lane");
    let gap = 2.0 * 0.3 + c.config().min_gap;
    assert!(
        (at.distance() - (10.2 - gap)).abs() < 0.05,
        "stopped short: {}",
        at.distance()
    );
}

#[test]
fn agent_errors_are_typed() {
    let g = graph();
    let mut c = crowd();
    let p = NavLaneAgentParams::default();
    assert_eq!(
        c.add_agent(&g, 9, 0.0, 0.0, p, 0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_unknown_lane")
    );
    assert_eq!(
        c.add_agent(&g, 0, f32::NAN, 0.0, p, 0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_invalid_lane_agent")
    );
    let bad = NavLaneAgentParams { radius: 0.0, ..p };
    assert_eq!(
        c.add_agent(&g, 0, 0.0, 0.0, bad, 0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_invalid_lane_agent")
    );
    let other = NavLaneAgentParams { flags: 2, ..p };
    assert_eq!(
        c.add_agent(&g, 0, 0.0, 0.0, other, 0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_invalid_lane_agent")
    );
    assert_eq!(
        c.remove_agent(NavLaneAgentId(4)).map_err(|e| e.code()),
        Err("nav_unknown_lane_agent")
    );
    assert_eq!(
        c.advance(&g, -1.0).map(|_| ()).map_err(|e| e.code()),
        Err("nav_invalid_input")
    );
    let cfg = NavLaneCrowdConfig {
        look_ahead: 0.0,
        ..Default::default()
    };
    assert_eq!(
        NavLaneCrowd::new(cfg).map(|_| ()).map_err(|e| e.code()),
        Err("nav_invalid_lane_config")
    );
    let tiny = NavLaneCrowdConfig {
        max_agents: 1,
        ..Default::default()
    };
    let mut one = NavLaneCrowd::new(tiny).expect("config");
    one.add_agent(&g, 0, 0.0, 0.0, p, 0).expect("valid agent");
    assert_eq!(
        one.add_agent(&g, 0, 1.0, 0.0, p, 0)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("nav_too_many_lane_agents")
    );
}

#[test]
fn offset_and_distance_are_clamped_to_the_lane() {
    let g = straight(2.0);
    let mut c = crowd();
    let id = c
        .add_agent(&g, 0, 500.0, 9.0, NavLaneAgentParams::default(), 0)
        .expect("valid agent");
    let a = c.agent(id).expect("agent");
    assert_eq!(a.distance(), 100.0);
    assert!((a.offset() - 0.7).abs() < 1e-5, "{}", a.offset());
}

#[test]
fn advance_runs_whole_steps_and_keeps_the_events() {
    let mut a = lane(&[(0.0, 0.0), (1.0, 0.0)]);
    a.next = vec![1];
    let b = lane(&[(1.0, 0.0), (9.0, 0.0)]);
    let g = NavLaneGraph::build(&[a, b], 0.0).expect("valid lanes");
    let mut c = crowd();
    c.add_agent(&g, 0, 0.99, 0.0, NavLaneAgentParams::default(), 0)
        .expect("valid agent");
    let ran = c.advance(&g, 0.11).expect("valid time");
    assert_eq!(ran, 3, "0.11 s at 30 Hz");
    assert_eq!(
        c.transitions().len(),
        1,
        "the crossing in the first step is still listed"
    );
    assert_eq!(c.stats().steps, 3);
    assert_eq!(c.advance(&g, 100.0).expect("valid time"), 4, "capped");
}

#[test]
fn a_lane_agent_sleeps_on_an_unloaded_tile_and_blocks_those_behind() {
    let c = 16.0;
    let floor = vec![
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
    let params = || NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    };
    let whole = NavTileCache::build(params(), &floor).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let west: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx / 2).map(move |x| (x, z))).collect();
    let east: Vec<(u32, u32)> = (0..tz).flat_map(|z| (tx / 2..tx).map(move |x| (x, z))).collect();
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let east_blob = whole.cook_cell(&east).expect("cooks");
    cache
        .load_cell(0, &whole.cook_cell(&west).expect("cooks"))
        .expect("loads");
    cache.load_cell(1, &east_blob).expect("loads");
    cache.update();

    let mut l = lane(&[(-12.0, 0.0), (12.0, 0.0)]);
    l.width = 0.6;
    let g = NavLaneGraph::build(&[l], 0.0).expect("valid lanes");
    let mut lanes = crowd();
    let front = lanes
        .add_agent(&g, 0, 14.0, 0.0, NavLaneAgentParams::default(), 1)
        .expect("valid agent");
    let back = lanes
        .add_agent(&g, 0, 10.0, 0.0, NavLaneAgentParams::default(), 2)
        .expect("valid agent");
    cache.unload_cell(1).expect("loaded");
    cache.update();
    for _ in 0..300 {
        lanes.sync_streaming(&g, &cache);
        lanes.step(&g);
    }
    let held = lanes.agent(front).expect("agent").distance();
    assert_eq!(
        lanes.agent(front).map(|a| a.state()),
        Some(NavLaneAgentState::Dormant)
    );
    assert_eq!(lanes.dormant_count(), 2, "the one behind reached the same border");
    assert!(lanes.agent(back).expect("agent").distance() < held);
    cache.load_cell(1, &east_blob).expect("loads");
    cache.update();
    for _ in 0..300 {
        lanes.sync_streaming(&g, &cache);
        lanes.step(&g);
    }
    assert_eq!(lanes.dormant_count(), 0);
    assert!(
        lanes.agent(front).expect("agent").distance() > held,
        "it walked on"
    );
}

#[test]
fn a_big_crowd_replays_to_the_same_digest() {
    let run = || {
        let g = graph();
        let mut c = crowd();
        for i in 0..400u64 {
            let lane = (i % 3) as u32;
            let params = NavLaneAgentParams {
                speed: 0.8 + (i % 7) as f32 * 0.2,
                ..Default::default()
            };
            c.add_agent(
                &g,
                lane,
                (i % 50) as f32 * 0.4,
                ((i % 5) as f32 - 2.0) * 0.3,
                params,
                i,
            )
            .expect("valid agent");
        }
        steps(&mut c, &g, 600);
        c.digest()
    };
    assert_eq!(run(), run());
}

#[test]
fn the_order_agents_were_added_in_does_not_change_where_they_end() {
    // Jacobi planning: an agent reads last step's positions, so ids only break exact ties.
    let run = |reversed: bool| {
        let g = straight(3.0);
        let mut c = crowd();
        let mut spawn: Vec<(f32, f32)> = (0..20)
            .map(|i| (i as f32 * 1.5, 0.3 - (i % 3) as f32 * 0.3))
            .collect();
        if reversed {
            spawn.reverse();
        }
        for (d, o) in spawn {
            c.add_agent(&g, 0, d, o, NavLaneAgentParams::default(), 0)
                .expect("valid agent");
        }
        steps(&mut c, &g, 600);
        let mut out: Vec<(u32, u32)> = c
            .agents()
            .iter()
            .map(|a| (a.distance().to_bits(), a.offset().to_bits()))
            .collect();
        out.sort_unstable();
        out
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn the_file_round_trips_and_refuses_what_it_cannot_read() {
    let set = NavLaneSet {
        format_version: 1,
        join_distance: 0.25,
        lanes: diamond(),
    };
    let text = set.to_ron().expect("serializes");
    let back = NavLaneSet::parse(&text).expect("parses");
    assert_eq!(back, set);
    assert_eq!(
        back.graph().expect("valid"),
        NavLaneGraph::build(&set.lanes, 0.25).expect("valid")
    );

    let minimal = r#"(format_version: 1, lanes: [(points: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0)])])"#;
    let lanes = NavLaneSet::parse(minimal).expect("defaults fill in");
    assert_eq!(lanes.lanes[0].width, 2.0);
    assert_eq!(lanes.lanes[0].speed, 1.4);
    assert_eq!(lanes.lanes[0].flags, 1);

    let code = |t: &str| NavLaneSet::parse(t).map(|_| ()).map_err(|e| e.code());
    assert_eq!(code("(format_version: 1"), Err("nav_lane_parse"));
    assert_eq!(code("(format_version: 0, lanes: [])"), Err("nav_lane_parse"));
    assert_eq!(code("(format_version: 3, lanes: [])"), Err("nav_lane_version"));
    assert_eq!(
        code("(format_version: 1, lanes: [(points: [(0.0, 0.0, 0.0)])])"),
        Err("nav_invalid_lane")
    );
}

#[test]
fn a_long_step_carries_the_leftover_through_short_lanes() {
    // Eight 1 m lanes in a row; one step of 1 s at 6.5 m/s must end 0.5 m into lane 6.
    let mut defs: Vec<NavLaneDef> = (0..8)
        .map(|i| lane(&[(i as f32, 0.0), (i as f32 + 1.0, 0.0)]))
        .collect();
    for (i, d) in defs.iter_mut().enumerate().take(7) {
        d.next = vec![i as u32 + 1];
    }
    for d in &mut defs {
        d.speed = 10.0;
    }
    let g = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    let mut c = NavLaneCrowd::new(NavLaneCrowdConfig {
        step_seconds: 1.0,
        ..Default::default()
    })
    .expect("config");
    let params = NavLaneAgentParams {
        speed: 6.5,
        ..Default::default()
    };
    let id = c.add_agent(&g, 0, 0.0, 0.0, params, 1).expect("valid agent");
    c.step(&g);
    let a = c.agent(id).expect("agent");
    assert_eq!(a.lane(), 6, "six lanes crossed in one step");
    assert!((a.distance() - 0.5).abs() < 1e-4, "{}", a.distance());
}

#[test]
fn an_agent_far_to_the_side_on_the_next_lane_does_not_block_a_junction() {
    let mut first = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    first.next = vec![1];
    let mut wide = lane(&[(10.0, 0.0), (60.0, 0.0)]);
    wide.width = 12.0;
    let g = NavLaneGraph::build(&[first, wide], 0.0).expect("valid lanes");
    let mut c = crowd();
    let still = NavLaneAgentParams {
        speed: 0.0,
        ..Default::default()
    };
    // Standing 0.5 m into the wide lane and 4 m to the side.
    c.add_agent(&g, 1, 0.5, 4.0, still, 1).expect("valid agent");
    let walker = c
        .add_agent(&g, 0, 9.0, 0.0, NavLaneAgentParams::default(), 2)
        .expect("valid agent");
    c.step(&g);
    let a = c.agent(walker).expect("agent");
    assert!(
        a.speed() > 0.5,
        "not held back by a body 4 m aside: {}",
        a.speed()
    );
}

#[test]
fn a_route_may_take_a_fast_jump_to_a_distant_lane() {
    // Lane 2 sits 1 km away but is linked from lane 0 and costs 0.01 s: it beats the local lane 1.
    let mut l0 = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    l0.speed = 1.0;
    l0.next = vec![1, 2];
    let mut l1 = lane(&[(10.0, 0.0), (15.0, 0.0)]);
    l1.speed = 1.0;
    l1.next = vec![3];
    let mut l2 = lane(&[(1000.0, 0.0), (1000.1, 0.0)]);
    l2.speed = 10.0;
    l2.next = vec![3];
    let l3 = lane(&[(15.0, 0.0), (20.0, 0.0)]);
    let g = NavLaneGraph::build(&[l0, l1, l2, l3], 0.0).expect("valid lanes");
    let route = g.find_route(0, 3, u16::MAX).expect("route");
    assert_eq!(route.lanes, vec![0, 2, 3]);
}

#[test]
fn lane_graph_shift_origin_adds_the_delta_to_every_centre_line_point() {
    let mut g = graph();
    let before: Vec<Vec<Vec3>> = g.lanes().iter().map(|l| l.points.clone()).collect();
    let delta = Vec3::new(5.0, 2.0, -7.0);
    g.shift_origin(delta);
    for (lane, old) in g.lanes().iter().zip(&before) {
        assert_eq!(lane.points.len(), old.len());
        for (a, b) in lane.points.iter().zip(old) {
            assert_eq!(*a, *b + delta);
        }
    }
    // Non-finite deltas are ignored.
    let snapshot = g.lanes()[0].points.clone();
    g.shift_origin(Vec3::new(f32::NAN, 0.0, 0.0));
    assert_eq!(g.lanes()[0].points, snapshot);
}
