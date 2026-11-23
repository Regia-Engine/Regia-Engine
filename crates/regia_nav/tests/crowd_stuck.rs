// SPDX-License-Identifier: MPL-2.0
//! Stuck crowd agents (NM-2): a fixture that wedges an agent, shown stuck for good without
//! recovery, then detected and recovered (or reported) with it. Fixed steps and deterministic
//! inputs only; nothing reads the wall clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavMesh, NavMeshBuilder, NavStuckAction,
};

/// Two triangles covering the axis-aligned rectangle `[x0, x1] x [z0, z1]` at height 0.
fn rect(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
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

/// A 6 m room with a 1 m wide, 10 m long dead-end corridor off its east wall. The mesh is eroded
/// by the 0.35 m build radius, so the corridor's walkable strip is far too narrow for two 0.35 m
/// agents to pass each other.
fn pocket_mesh() -> NavMesh {
    let mut tris: Vec<[Vec3; 3]> = Vec::new();
    tris.extend(rect(0.0, 0.0, 6.0, 6.0));
    tris.extend(rect(6.0, 2.5, 16.0, 3.5));
    NavMeshBuilder::default().build(&tris).expect("the pocket builds")
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

/// Agent `pinned` stands at the dead end of the corridor and wants out into the room; agent
/// `pusher` comes the other way into the dead end. They meet head on in the strip and press
/// against each other along its axis.
fn wedge(
    config: NavCrowdConfig,
) -> (
    NavMesh,
    NavCrowd,
    regia_nav::CrowdAgentId,
    regia_nav::CrowdAgentId,
) {
    let mesh = pocket_mesh();
    let mut crowd = NavCrowd::new(config).expect("valid config");
    let pinned = crowd
        .add_agent(&mesh, Vec3::new(15.0, 0.0, 3.0), agent())
        .expect("dead end on the mesh");
    let pusher = crowd
        .add_agent(&mesh, Vec3::new(2.0, 0.0, 3.0), agent())
        .expect("room on the mesh");
    crowd
        .set_target(&mesh, pinned, Vec3::new(2.0, 0.0, 3.0))
        .expect("room target");
    crowd
        .set_target(&mesh, pusher, Vec3::new(15.0, 0.0, 3.0))
        .expect("dead-end target");
    (mesh, crowd, pinned, pusher)
}

#[test]
fn a_crowd_agent_pinned_in_a_corner_stays_stuck_without_recovery() {
    let (mesh, mut crowd, pinned, pusher) = wedge(NavCrowdConfig::default());
    let mut trace = Vec::new();
    for step in 0..2400 {
        crowd.step(&mesh);
        if step % 300 == 299 {
            let a = crowd.agent(pinned).expect("pinned");
            let b = crowd.agent(pusher).expect("pusher");
            trace.push((step + 1, a.position(), a.state(), b.position(), b.state()));
        }
    }
    for t in &trace {
        eprintln!("{t:?}");
    }
    // Wedged: from 5 s to 20 s the pinned agent moves less than 5 cm.
    let (at_5s, at_20s) = (trace[0].1, trace[3].1);
    assert!(
        (at_20s - at_5s).length() < 0.05,
        "no progress from 5 s to 20 s: {at_5s:?} -> {at_20s:?}"
    );
    // A clear walk to the room takes about 4.3 s; after 40 s it still walks, in the corridor
    // (x > 6), more than 4 m from its goal, and nothing ever reported it.
    let a = crowd.agent(pinned).expect("pinned");
    assert_eq!(a.state(), CrowdAgentState::Walking);
    assert!(
        a.position().x > 6.0,
        "the pinned agent never reaches the room: {:?}",
        a.position()
    );
    assert!((a.position() - Vec3::new(2.0, 0.0, 3.0)).length() > 4.0);
}

/// The stuck settings the acceptance test uses: one second at 60 Hz per rung.
fn stuck_config() -> NavCrowdConfig {
    NavCrowdConfig {
        stuck_window_steps: 60,
        stuck_min_progress_m: 0.5,
        ..NavCrowdConfig::default()
    }
}

#[test]
fn a_crowd_agent_pinned_in_a_corner_is_detected_and_recovers() {
    let (mesh, mut crowd, pinned, pusher) = wedge(stuck_config());
    let window = 60;
    let mut events = Vec::new();
    let mut first_rung = None;
    let mut settled = None;
    for step in 0..1200u32 {
        crowd.step(&mesh);
        for e in crowd.stuck_events() {
            events.push((step, *e));
            if e.agent == pinned && first_rung.is_none() {
                first_rung = Some(step);
            }
        }
        let a = crowd.agent(pinned).expect("pinned");
        let recovered = a.position().x < 6.0 || a.state() == CrowdAgentState::Arrived;
        if settled.is_none() && (a.state() == CrowdAgentState::Stuck || recovered) {
            settled = Some((step, a.state(), a.position()));
        }
    }
    for e in &events {
        eprintln!("{e:?}");
    }
    eprintln!("settled {settled:?} stats {:?}", crowd.stats());
    // Detected: the first rung is a re-path one window after the two agents lock up (they slow
    // for each other and meet after about 3.5 s), well inside 6 s.
    let first_rung = first_rung.expect("the pinned agent is found stuck");
    assert!(first_rung <= 360, "first rung at step {first_rung}");
    let first = events
        .iter()
        .find(|(_, e)| e.agent == pinned)
        .map(|(_, e)| e.action);
    assert_eq!(first, Some(NavStuckAction::Repath));
    // Recovered or reported within the ladder: the agents lock up by about 3.5 s (step 210),
    // then three windows of rungs, plus one window for the creep before the lock-up (a creep of
    // `stuck_min_progress_m` restarts the ladder, as real progress should). Without detection
    // the agent is still wedged at 40 s (the test above).
    let (at, state, position) = settled.expect("recovered or reported");
    assert!(
        at <= 210 + 4 * window,
        "settled at step {at} ({state:?} at {position:?}), first rung at {first_rung}"
    );
    let s = crowd.stats();
    assert!(s.stuck_detections >= 3 && s.stuck_repaths >= 1);
    if state == CrowdAgentState::Stuck {
        // The full ladder, in order: re-path, nudge, report.
        let ladder: Vec<NavStuckAction> = events
            .iter()
            .filter(|(step, e)| e.agent == pinned && *step <= at)
            .map(|(_, e)| e.action)
            .collect();
        assert_eq!(
            ladder[ladder.len().saturating_sub(3)..],
            [
                NavStuckAction::Repath,
                NavStuckAction::Nudge,
                NavStuckAction::Reported
            ],
            "{ladder:?}"
        );
        assert!(crowd.stuck_count() >= 1);
    }
    // A stuck agent walks again on a new target.
    if crowd.agent(pinned).expect("pinned").state() == CrowdAgentState::Stuck {
        crowd
            .set_target(&mesh, pinned, Vec3::new(15.0, 0.0, 3.0))
            .expect("dead-end target");
        assert_ne!(
            crowd.agent(pinned).expect("pinned").state(),
            CrowdAgentState::Stuck
        );
    }
    let _ = pusher;
}

#[test]
fn stuck_detection_replays_bit_identically_and_off_changes_nothing() {
    let run = |config: NavCrowdConfig| {
        let (mesh, mut crowd, _, _) = wedge(config);
        for _ in 0..600 {
            crowd.step(&mesh);
        }
        (crowd.digest(), crowd.stats())
    };
    let (a, sa) = run(stuck_config());
    let (b, sb) = run(stuck_config());
    assert_eq!(a, b, "two runs with stuck detection give one digest");
    assert_eq!(sa, sb);
    // Off (the default) and switched off at run time are the same crowd.
    let (off, _) = run(NavCrowdConfig::default());
    let (mesh, mut crowd, _, _) = wedge(stuck_config());
    crowd.set_stuck_detection(0, 0.5).expect("valid settings");
    for _ in 0..600 {
        crowd.step(&mesh);
    }
    assert_eq!(crowd.digest(), off);
    assert_eq!(crowd.stats().stuck_detections, 0);
    // Bad settings are refused.
    assert_eq!(
        crowd.set_stuck_detection(60, 0.0).map_err(|e| e.code()),
        Err("nav_invalid_crowd_config")
    );
    assert!(
        NavCrowd::new(NavCrowdConfig {
            stuck_min_progress_m: f32::NAN,
            ..stuck_config()
        })
        .is_err()
    );
}
