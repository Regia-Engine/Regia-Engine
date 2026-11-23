// SPDX-License-Identifier: MPL-2.0
//! Ballistic link crossings (ADR-2589, `docs/specs/92-navmesh-offmesh.md` § 3.3): Jump links
//! along a jump arc, Drop links along a falling parabola, the crossing time from the arc, the
//! `NavLinkTraversal` message, and bit-identical replays. Fixed steps and deterministic inputs
//! only; nothing reads the clock.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavCrowdPlugin,
    NavLinkArcConfig, NavLinkKind, NavLinkPhase, NavLinkTraversal, NavLinkTraversalMode, NavMesh,
    NavMeshBuildParams, NavMeshBuilder, NavQuery, OffMeshLink,
};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// Two 5 x 8 m floors at `x ∈ [-6, -1]` (height `left`) and `x ∈ [1, 6]` (height `right`).
fn floors(left: f32, right: f32) -> Vec<[Vec3; 3]> {
    let mut t: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0, left).into();
    t.extend(rect(1.0, -4.0, 6.0, 4.0, right));
    t
}

fn mesh(left: f32, right: f32, link: OffMeshLink) -> NavMesh {
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&floors(left, right))
    .expect("the floors build")
    .with_off_mesh_links(vec![link])
}

fn jump(left: f32, right: f32) -> OffMeshLink {
    OffMeshLink::new(Vec3::new(-1.5, left, 0.0), Vec3::new(1.5, right, 0.0))
        .with_user_id(9)
        .with_radius(0.5)
}

fn arcs() -> NavCrowdConfig {
    NavCrowdConfig {
        link_arcs: NavLinkArcConfig::on(),
        ..Default::default()
    }
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

/// One step's record of a crossing.
#[derive(Debug, Clone, Copy)]
struct Sample {
    step: u32,
    position: Vec3,
    traversal: Option<NavLinkTraversal>,
}

/// An agent walks from `from` to `to`; returns the crowd, the agent, every step and every event.
fn run(
    mesh: &NavMesh,
    config: NavCrowdConfig,
    from: Vec3,
    to: Vec3,
    steps: u32,
) -> (NavCrowd, CrowdAgentId, Vec<Sample>, Vec<(u32, NavLinkPhase)>) {
    let mut crowd = NavCrowd::new(config).expect("config");
    let id = crowd.add_agent(mesh, from, agent()).expect("on the mesh");
    crowd.set_target(mesh, id, to).expect("target");
    let mut samples = Vec::new();
    let mut events = Vec::new();
    for k in 0..steps {
        crowd.step(mesh);
        events.extend(crowd.link_events().iter().map(|e| (k, e.phase)));
        let a = crowd.agent(id).expect("agent");
        samples.push(Sample {
            step: k,
            position: a.position(),
            traversal: a.link_traversal().copied(),
        });
    }
    (crowd, id, samples, events)
}

/// The analytic apex height of a jump from `start_y` to `end_y` under `arcs`.
fn analytic_apex(arcs: &NavLinkArcConfig, start_y: f32, end_y: f32) -> f32 {
    let up = (end_y - start_y).max(0.0);
    let reach = arcs.max_jump_speed * arcs.max_jump_speed / (2.0 * arcs.gravity);
    start_y + (up + arcs.apex_clearance).min(reach).max(up)
}

#[test]
fn a_gap_jump_follows_the_arc_and_lands_on_the_far_polygon() {
    let mesh = mesh(0.0, 0.0, jump(0.0, 0.0));
    let config = arcs();
    let (crowd, id, samples, events) = run(
        &mesh,
        config.clone(),
        Vec3::new(-4.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0),
        600,
    );
    let on_link: Vec<&Sample> = samples.iter().filter(|s| s.traversal.is_some()).collect();
    assert!(!on_link.is_empty(), "the agent crossed: {events:?}");
    let t = on_link[0].traversal.expect("crossing");
    assert_eq!(t.kind, NavLinkKind::Jump);
    let arc = t.arc.expect("a jump with arcs on has an arc");
    // The apex: analytic from the crossing's own ends, within 1 cm by the highest sampled step.
    let apex = analytic_apex(&config.link_arcs, t.start.y, t.end.y);
    assert!(
        (arc.apex.y - apex).abs() < 1e-4,
        "arc apex {} vs {apex}",
        arc.apex.y
    );
    let highest = on_link.iter().map(|s| s.position.y).fold(f32::MIN, f32::max);
    assert!(
        (highest - apex).abs() < 0.01,
        "sampled apex {highest} vs analytic {apex}"
    );
    // The flight time is the arc's: up for sqrt(2h/g), down for as long.
    let flight = 2.0 * (2.0 * config.link_arcs.apex_clearance / config.link_arcs.gravity).sqrt();
    assert!(
        (arc.flight_seconds - flight).abs() < 1e-4,
        "{} vs {flight}",
        arc.flight_seconds
    );
    let dt = config.step_seconds;
    assert_eq!(
        t.total_steps,
        ((t.walk_seconds + arc.flight_seconds) / dt).ceil() as u32
    );
    assert!(t.duration() >= t.crossing_seconds() && t.duration() - t.crossing_seconds() < dt + 1e-5);
    // Every flight step is on the parabola, and the horizontal motion is a straight line.
    for s in &on_link {
        let tr = s.traversal.expect("crossing");
        if let Some(ft) = tr.flight_time().filter(|&ft| ft > 0.0) {
            let p = arc.point_at(ft);
            assert!((s.position - p).length() < 1e-4, "step {} off the arc", s.step);
            let along = (s.position.x - arc.start.x) / (arc.end.x - arc.start.x);
            assert!(
                (along - ft / arc.flight_seconds).abs() < 1e-3,
                "constant horizontal speed"
            );
        }
    }
    // Landed on the far floor, on its polygon, and walked on to the target.
    let landed = samples
        .iter()
        .zip(&samples[1..])
        .find(|(a, b)| a.traversal.is_some() && b.traversal.is_none())
        .map(|(_, b)| b.position)
        .expect("a landing");
    assert!(
        landed.x > 1.0 && landed.y.abs() < 0.05,
        "on the far floor: {landed:?}"
    );
    let (poly, snapped) = NavQuery {
        mesh: &mesh,
        obstacles: None,
    }
    .find_nearest(landed, Vec3::new(0.1, 0.5, 0.1))
    .expect("on a polygon");
    assert!(mesh.polys[poly as usize].min.x >= 0.9, "the far island's polygon");
    assert!((snapped - landed).length() < 0.05);
    let phases: Vec<NavLinkPhase> = events.iter().map(|e| e.1).collect();
    assert_eq!(phases, vec![NavLinkPhase::Started, NavLinkPhase::Finished]);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn a_jump_up_rises_above_the_ledge_and_a_ledge_out_of_reach_is_speed_limited() {
    let config = NavLinkArcConfig::on();
    let up = config
        .arc(
            NavLinkKind::Jump,
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 0.0),
        )
        .expect("an arc");
    assert!(
        (up.apex.y - 1.5).abs() < 1e-5,
        "0.5 m over the higher end: {up:?}"
    );
    assert!(!up.speed_limited);
    assert!((up.point_at(up.flight_seconds) - Vec3::new(2.0, 1.0, 0.0)).length() < 1e-6);
    // Mid-flight the arc is continuous with its take-off velocity.
    let early = up.point_at(1e-3);
    assert!((early - (up.start + up.takeoff_velocity * 1e-3)).length() < 1e-4);
    // The take-off speed caps the apex: 6 m/s reaches 1.83 m, so 1.5 m + 0.5 m is cut to the reach.
    let reach = 36.0 / (2.0 * 9.81);
    let tall = config
        .arc(NavLinkKind::Jump, Vec3::ZERO, Vec3::new(2.0, 1.5, 0.0))
        .expect("an arc");
    assert!((tall.apex.y - reach).abs() < 1e-4, "{tall:?}");
    assert!(!tall.speed_limited);
    // Out of reach: the arc still lands, apex at the landing, flagged.
    let high = config
        .arc(NavLinkKind::Jump, Vec3::ZERO, Vec3::new(2.0, 3.0, 0.0))
        .expect("an arc");
    assert!(high.speed_limited && (high.apex.y - 3.0).abs() < 1e-5);
    assert!((high.point_at(high.flight_seconds).y - 3.0).abs() < 1e-5);
    // Straight kinds and arcs off have no arc.
    for kind in [NavLinkKind::Climb, NavLinkKind::Teleport, NavLinkKind::Custom(3)] {
        assert!(config.arc(kind, Vec3::ZERO, Vec3::X).is_none());
    }
    assert!(
        NavLinkArcConfig::default()
            .arc(NavLinkKind::Jump, Vec3::ZERO, Vec3::X)
            .is_none()
    );
}

#[test]
fn a_drop_falls_along_a_parabola_and_is_one_way() {
    let link = OffMeshLink::new(Vec3::new(-1.5, 2.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .one_way()
        .with_kind(NavLinkKind::Drop)
        .with_radius(0.5);
    let mesh = mesh(2.0, 0.0, link);
    let config = arcs();
    let (_, _, samples, events) = run(
        &mesh,
        config.clone(),
        Vec3::new(-4.0, 2.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0),
        600,
    );
    let flight: Vec<(f32, Vec3)> = samples
        .iter()
        .filter_map(|s| {
            s.traversal
                .and_then(|t| t.flight_time())
                .map(|ft| (ft, s.position))
        })
        .filter(|(ft, _)| *ft > 0.0)
        .collect();
    assert!(!flight.is_empty(), "{events:?}");
    let t = samples.iter().find_map(|s| s.traversal).expect("crossing");
    assert_eq!(t.kind, NavLinkKind::Drop);
    let arc = t.arc.expect("an arc");
    // Apex at the take-off (it steps off level); the fall takes sqrt(2 h / g).
    assert_eq!(arc.apex, arc.start);
    assert_eq!(arc.takeoff_velocity.y, 0.0);
    let fall = arc.start.y - arc.end.y;
    assert!((arc.flight_seconds - (2.0 * fall / config.link_arcs.gravity).sqrt()).abs() < 1e-4);
    // Height only goes down, along y = start - g t² / 2.
    for pair in flight.windows(2) {
        assert!(pair[1].1.y < pair[0].1.y, "falling: {pair:?}");
    }
    for (ft, p) in &flight {
        let y = arc.start.y - 0.5 * config.link_arcs.gravity * ft * ft;
        assert!((p.y - y.max(arc.end.y)).abs() < 1e-4, "on the parabola at {ft}");
    }
    let last = samples.last().expect("steps").position;
    assert!(
        last.x > 3.0 && last.y.abs() < 0.05,
        "down on the low floor: {last:?}"
    );
    // The way back up: no route over a one-way drop.
    let (crowd, id, back, _) = run(
        &mesh,
        config,
        Vec3::new(4.0, 0.0, 0.0),
        Vec3::new(-4.0, 2.0, 0.0),
        30,
    );
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::NoPath);
    assert!(back.iter().all(|s| s.traversal.is_none()));
}

#[test]
fn two_runs_give_the_same_crowd_digest_and_events() {
    let mesh = mesh(0.0, 1.0, jump(0.0, 1.0));
    let go = || {
        let mut crowd = NavCrowd::new(arcs()).expect("config");
        let mut ids = Vec::new();
        for (k, z) in [-2.0f32, 0.0, 2.0].iter().enumerate() {
            let id = crowd
                .add_agent(&mesh, Vec3::new(-4.0 - k as f32 * 0.4, 0.0, *z), agent())
                .expect("on the mesh");
            crowd
                .set_target(&mesh, id, Vec3::new(4.0, 1.0, -*z))
                .expect("target");
            ids.push(id);
        }
        let mut digests = Vec::new();
        let mut events = Vec::new();
        for _ in 0..900 {
            crowd.step(&mesh);
            digests.push(crowd.digest());
            events.extend(
                crowd
                    .link_events()
                    .iter()
                    .map(|e| (e.agent, e.phase, e.position.to_array())),
            );
        }
        (digests, events, crowd.stats().link_traversals)
    };
    let (a, ea, na) = go();
    let (b, eb, nb) = go();
    assert_eq!(a, b, "bit-identical digests");
    assert_eq!(ea, eb);
    assert_eq!(na, 3, "every agent crossed");
    assert_eq!(na, nb);
}

#[test]
fn the_arc_wins_over_an_authored_duration_and_straight_modes_have_none() {
    let mut link = jump(0.0, 0.0);
    link.duration = 5.0;
    let mesh = mesh(0.0, 0.0, link);
    let (_, _, samples, _) = run(
        &mesh,
        arcs(),
        Vec3::new(-4.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0),
        400,
    );
    let t = samples.iter().find_map(|s| s.traversal).expect("crossing");
    assert!(t.arc.is_some());
    assert!(
        t.duration() < 1.5,
        "the flight time, not the authored 5 s: {}",
        t.duration()
    );
    // Arcs off: the authored 5 s, no arc.
    let (_, _, samples, _) = run(
        &mesh,
        NavCrowdConfig::default(),
        Vec3::new(-4.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0),
        400,
    );
    let t = samples.iter().find_map(|s| s.traversal).expect("crossing");
    assert!(t.arc.is_none() && t.flight_time().is_none());
    assert!((t.duration() - 5.0).abs() < 1e-3);
    // Instant mode with arcs on: still one step, no arc.
    let config = NavCrowdConfig {
        link_traversal: NavLinkTraversalMode::Instant,
        ..arcs()
    };
    let (crowd, id, samples, events) = run(
        &mesh,
        config,
        Vec3::new(-4.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, 0.0),
        400,
    );
    assert!(samples.iter().all(|s| s.traversal.is_none()));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, events[1].0, "same step");
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn bad_arc_settings_are_refused() {
    for bad in [
        NavLinkArcConfig {
            gravity: 0.0,
            ..NavLinkArcConfig::on()
        },
        NavLinkArcConfig {
            max_jump_speed: f32::NAN,
            ..NavLinkArcConfig::on()
        },
        NavLinkArcConfig {
            apex_clearance: -0.1,
            ..NavLinkArcConfig::on()
        },
    ] {
        let e = NavCrowd::new(NavCrowdConfig {
            link_arcs: bad,
            ..Default::default()
        })
        .expect_err("refused");
        assert_eq!(e.code(), "nav_invalid_crowd_config");
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        assert!(crowd.set_link_arcs(bad).is_err());
        assert!(!crowd.config().link_arcs.enabled, "nothing changed");
    }
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    crowd.set_link_arcs(NavLinkArcConfig::on()).expect("valid");
    assert!(crowd.config().link_arcs.enabled);
}

#[test]
fn the_plugin_writes_a_traversal_message_at_each_take_off() {
    let mesh = mesh(0.0, 0.0, jump(0.0, 0.0));
    let mut crowd = NavCrowd::new(arcs()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut app = App::new();
    app.add_plugins(NavCrowdPlugin);
    app.insert_resource(mesh);
    app.insert_resource(crowd);
    let mut seen = Vec::new();
    let mut cursor = app.world().resource::<Messages<NavLinkTraversal>>().get_cursor();
    for _ in 0..400 {
        app.world_mut().run_schedule(FixedUpdate);
        let messages = app.world().resource::<Messages<NavLinkTraversal>>();
        seen.extend(cursor.read(messages).copied());
    }
    assert_eq!(seen.len(), 1, "one take-off: {seen:?}");
    let m = seen[0];
    assert_eq!((m.user_id, m.kind, m.steps), (9, NavLinkKind::Jump, 0));
    let arc = m.arc.expect("the arc rides on the message");
    assert!(arc.flight_seconds > 0.0 && arc.apex.y > arc.start.y);
    let crowd = app.world().resource::<NavCrowd>();
    assert_eq!(crowd.stats().link_traversals, 1);
    assert!(crowd.link_traversals().is_empty(), "nobody on a link at the end");
}

#[test]
fn an_origin_shift_mid_flight_moves_the_arc_with_the_agent() {
    let mesh = mesh(0.0, 0.0, jump(0.0, 0.0));
    let mut crowd = NavCrowd::new(arcs()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-4.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    // Walk until the agent is in the air, a few steps past take-off.
    let mut airborne = 0;
    for _ in 0..600 {
        crowd.step(&mesh);
        let flying = crowd
            .agent(id)
            .expect("agent")
            .link_traversal()
            .and_then(NavLinkTraversal::flight_time)
            .is_some_and(|t| t > 0.0);
        if flying {
            airborne += 1;
            if airborne == 3 {
                break;
            }
        }
    }
    assert_eq!(airborne, 3, "the agent never took off");
    let delta = Vec3::new(64.0, 0.0, 32.0);
    let mut shifted_mesh = mesh.clone();
    shifted_mesh.shift_origin(delta);
    let mut shifted = crowd.clone();
    shifted.shift_origin(delta);
    let arc = shifted
        .agent(id)
        .expect("agent")
        .link_traversal()
        .and_then(|t| t.arc)
        .expect("a jump with arcs on has an arc");
    let before = crowd
        .agent(id)
        .expect("agent")
        .link_traversal()
        .and_then(|t| t.arc)
        .expect("arc");
    assert!(
        (arc.start - (before.start + delta)).length() < 1e-4
            && (arc.apex - (before.apex + delta)).length() < 1e-4,
        "the arc stayed in the old frame: {arc:?} vs {before:?} + {delta:?}"
    );
    crowd.step(&mesh);
    shifted.step(&shifted_mesh);
    let a = crowd.agent(id).expect("agent").position();
    let b = shifted.agent(id).expect("agent").position();
    assert!(
        (b - (a + delta)).length() < 1e-3,
        "shifted agent at {b:?}, expected {:?}",
        a + delta
    );
}
