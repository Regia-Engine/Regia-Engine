// SPDX-License-Identifier: MPL-2.0
//! Link reservation and queuing (ADR-2420): with `NavLinkQueueConfig::exclusive` one agent crosses
//! a link at a time, the others wait first come first served, a wait can time out, and a run
//! replays exactly. Fixed steps and deterministic inputs only; nothing reads the clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavLinkPhase,
    NavLinkQueueConfig, NavMesh, NavMeshBuildParams, NavMeshBuilder, OffMeshLink,
};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// Two 5 x 8 m floors with a 2 m gap and one link over it (the only way across), crossed in
/// `seconds`.
fn mesh(seconds: f32) -> NavMesh {
    let mut tris: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0, 0.0).into();
    tris.extend(rect(1.0, -4.0, 6.0, 4.0, 0.0));
    let mut link = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .with_user_id(9)
        .with_radius(0.5);
    link.duration = seconds;
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("the floors build")
    .with_off_mesh_links(vec![link])
}

fn exclusive(timeout_steps: u32) -> NavCrowdConfig {
    NavCrowdConfig {
        link_queue: NavLinkQueueConfig {
            exclusive: true,
            timeout_steps,
        },
        ..Default::default()
    }
}

/// A crowd with agents at `starts`, each sent to the far floor at its own `z`, so all can
/// arrive.
fn crowd(mesh: &NavMesh, config: NavCrowdConfig, starts: &[Vec3]) -> (NavCrowd, Vec<CrowdAgentId>) {
    let mut crowd = NavCrowd::new(config).expect("config");
    let ids: Vec<CrowdAgentId> = starts
        .iter()
        .map(|&p| {
            crowd
                .add_agent(mesh, p, CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0))
                .expect("on the mesh")
        })
        .collect();
    for (&id, start) in ids.iter().zip(starts) {
        crowd
            .set_target(mesh, id, Vec3::new(4.0, 0.0, start.z))
            .expect("target");
    }
    (crowd, ids)
}

/// Every link event of `steps` steps as `(step, agent, phase)`, checking each step that at most one
/// agent is on the link and that the link's holder is that agent.
fn run(mesh: &NavMesh, crowd: &mut NavCrowd, steps: u32) -> Vec<(u32, CrowdAgentId, NavLinkPhase)> {
    let mut events = Vec::new();
    for k in 0..steps {
        crowd.step(mesh);
        events.extend(crowd.link_events().iter().map(|e| (k, e.agent, e.phase)));
        let on_link: Vec<CrowdAgentId> = crowd
            .agents()
            .iter()
            .filter(|a| a.state() == CrowdAgentState::OffMeshLink)
            .map(|a| a.id())
            .collect();
        assert!(on_link.len() <= 1, "step {k}: {on_link:?} share the link");
        if crowd.config().link_queue.exclusive {
            let holders: Vec<CrowdAgentId> = crowd.link_holders().iter().map(|h| h.1).collect();
            assert_eq!(holders, on_link, "step {k}");
        }
    }
    events
}

fn phases(events: &[(u32, CrowdAgentId, NavLinkPhase)], phase: NavLinkPhase) -> Vec<(u32, CrowdAgentId)> {
    events
        .iter()
        .filter(|e| e.2 == phase)
        .map(|e| (e.0, e.1))
        .collect()
}

#[test]
fn two_agents_contend_for_one_link_deterministically() {
    let mesh = mesh(1.0);
    // Mirror images about the link's line: both reach the take-off point in the same step.
    let starts = [Vec3::new(-4.0, 0.0, -1.0), Vec3::new(-4.0, 0.0, 1.0)];
    let (mut a, ids) = crowd(&mesh, exclusive(0), &starts);
    let first = run(&mesh, &mut a, 900);
    let started = phases(&first, NavLinkPhase::Started);
    let finished = phases(&first, NavLinkPhase::Finished);
    let queued = phases(&first, NavLinkPhase::Queued);
    assert_eq!(started.len(), 2, "{first:?}");
    assert_eq!(finished.len(), 2, "{first:?}");
    assert_eq!(queued.len(), 1, "one agent waited: {first:?}");
    // The one that waited goes second, and only after the first landed.
    let waiter = queued[0].1;
    assert_eq!(started[1].1, waiter);
    assert!(started[1].0 >= finished[0].0, "{first:?}");
    assert_eq!(a.stats().link_queue_waits, 1);
    for &id in &ids {
        assert_eq!(a.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
    assert!(a.link_holders().is_empty());
    // The same inputs give the same events and the same digest.
    let (mut b, _) = crowd(&mesh, exclusive(0), &starts);
    assert_eq!(run(&mesh, &mut b, 900), first);
    assert_eq!(a.digest(), b.digest());
}

#[test]
fn without_reservation_both_cross_at_once() {
    let mesh = mesh(1.0);
    let starts = [Vec3::new(-4.0, 0.0, -1.0), Vec3::new(-4.0, 0.0, 1.0)];
    let (mut c, _) = crowd(&mesh, NavCrowdConfig::default(), &starts);
    let mut overlapped = false;
    for _ in 0..900 {
        c.step(&mesh);
        assert!(c.link_events().iter().all(|e| e.phase != NavLinkPhase::Queued));
        let on = c
            .agents()
            .iter()
            .filter(|a| a.state() == CrowdAgentState::OffMeshLink)
            .count();
        overlapped |= on == 2;
    }
    assert!(overlapped, "the default crowd lets both agents on the link");
    assert!(c.link_holders().is_empty());
}

#[test]
fn the_queue_is_first_come_first_served() {
    // A slow link (3 s) and three agents arriving one after another.
    let mesh = mesh(3.0);
    let starts = [
        Vec3::new(-3.0, 0.0, 0.0),
        Vec3::new(-4.5, 0.0, 2.5),
        Vec3::new(-5.5, 0.0, -3.0),
    ];
    let (mut c, ids) = crowd(&mesh, exclusive(0), &starts);
    let events = run(&mesh, &mut c, 1500);
    let queued: Vec<CrowdAgentId> = phases(&events, NavLinkPhase::Queued)
        .iter()
        .map(|q| q.1)
        .collect();
    let started: Vec<CrowdAgentId> = phases(&events, NavLinkPhase::Started)
        .iter()
        .map(|s| s.1)
        .collect();
    assert_eq!(started.len(), 3, "{events:?}");
    assert_eq!(queued.len(), 2, "{events:?}");
    // The first starts at once; the waiters cross in the order they queued.
    assert_eq!(started[1..], queued[..]);
    assert!(!queued.contains(&started[0]));
    for &id in &ids {
        assert_eq!(c.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
}

#[test]
fn a_wait_times_out_after_its_steps() {
    // A 5 s link: the waiter gives up long before the first agent lands.
    let mesh = mesh(5.0);
    let starts = [Vec3::new(-4.0, 0.0, -1.0), Vec3::new(-4.0, 0.0, 1.0)];
    let timeout = 20;
    let (mut c, _) = crowd(&mesh, exclusive(timeout), &starts);
    let mut events = Vec::new();
    let mut queue_seen = false;
    for k in 0..200 {
        c.step(&mesh);
        events.extend(c.link_events().iter().map(|e| (k, e.agent, e.phase)));
        if let Some(waiter) = c
            .agents()
            .iter()
            .find(|a| a.state() == CrowdAgentState::LinkQueued)
        {
            let wait = waiter.link_wait().expect("a queued agent has a wait");
            assert_eq!(wait.user_id, 9);
            assert_eq!(c.link_queue(wait.link), vec![waiter.id()]);
            queue_seen = true;
        }
    }
    assert!(queue_seen);
    let queued = phases(&events, NavLinkPhase::Queued);
    let timed_out = phases(&events, NavLinkPhase::TimedOut);
    assert_eq!((queued.len(), timed_out.len()), (1, 1), "{events:?}");
    assert_eq!(queued[0].1, timed_out[0].1);
    // It waits `timeout` whole steps and gives up on the next one.
    assert_eq!(timed_out[0].0 - queued[0].0, timeout + 1);
    let waiter = c.agent(queued[0].1).expect("agent");
    assert_eq!(waiter.state(), CrowdAgentState::LinkTimedOut);
    assert_eq!(waiter.state().name(), "link_timeout");
    assert!(waiter.link_wait().is_none());
    assert_eq!(c.stats().link_queue_timeouts, 1);
}

#[test]
fn removing_the_crossing_agent_frees_the_link() {
    let mesh = mesh(5.0);
    let starts = [Vec3::new(-4.0, 0.0, -1.0), Vec3::new(-4.0, 0.0, 1.0)];
    let (mut c, _) = crowd(&mesh, exclusive(0), &starts);
    let mut removed = None;
    let mut restarted = false;
    for _ in 0..600 {
        c.step(&mesh);
        if removed.is_none()
            && let Some(&(_, holder)) = c.link_holders().first()
            && c.agents()
                .iter()
                .any(|a| a.state() == CrowdAgentState::LinkQueued)
        {
            c.remove_agent(holder).expect("holder");
            removed = Some(holder);
            continue;
        }
        if removed.is_some()
            && c.link_events()
                .iter()
                .any(|e| e.phase == NavLinkPhase::Started && Some(e.agent) != removed)
        {
            restarted = true;
            break;
        }
    }
    assert!(removed.is_some(), "a queue formed");
    assert!(restarted, "the waiter took the freed link");
}

#[test]
fn turning_reservation_off_releases_the_queue() {
    let mesh = mesh(5.0);
    let starts = [Vec3::new(-4.0, 0.0, -1.0), Vec3::new(-4.0, 0.0, 1.0)];
    let (mut c, _) = crowd(&mesh, exclusive(0), &starts);
    for _ in 0..400 {
        c.step(&mesh);
        if c.agents()
            .iter()
            .any(|a| a.state() == CrowdAgentState::LinkQueued)
        {
            break;
        }
    }
    assert!(
        c.agents()
            .iter()
            .any(|a| a.state() == CrowdAgentState::LinkQueued)
    );
    c.set_link_queue(NavLinkQueueConfig::default());
    assert!(c.agents().iter().all(|a| a.link_wait().is_none()));
    assert!(c.link_holders().is_empty());
    let mut both = false;
    for _ in 0..60 {
        c.step(&mesh);
        both |= c
            .agents()
            .iter()
            .filter(|a| a.state() == CrowdAgentState::OffMeshLink)
            .count()
            == 2;
    }
    assert!(both, "the released agent crosses beside the first");
}

#[test]
fn a_replan_while_waiting_keeps_one_agent_per_link() {
    let mesh = mesh(2.0);
    let starts = [
        Vec3::new(-4.0, 0.0, -1.0),
        Vec3::new(-4.0, 0.0, 1.0),
        Vec3::new(-5.0, 0.0, 0.0),
    ];
    let (mut c, ids) = crowd(&mesh, exclusive(0), &starts);
    let mut replanned = false;
    let mut events = Vec::new();
    for k in 0..1500 {
        // The mesh "changed" (a tile rebuild): every agent replans, the waiting ones too.
        if !replanned
            && c.agents()
                .iter()
                .any(|a| a.state() == CrowdAgentState::LinkQueued)
        {
            c.request_replan_all();
            replanned = true;
        }
        events.extend(run(&mesh, &mut c, 1).into_iter().map(|(_, a, p)| (k, a, p)));
    }
    assert!(replanned, "a queue formed");
    assert_eq!(phases(&events, NavLinkPhase::Started).len(), 3, "{events:?}");
    for &id in &ids {
        assert_eq!(c.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
}
