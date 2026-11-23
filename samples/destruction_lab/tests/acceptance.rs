// SPDX-License-Identifier: MIT-0
//! The lab without a renderer or a socket: a server and two clients in one process, at fixed steps.

#[path = "../kit/mod.rs"]
mod kit;

use destruction_lab::{
    LabKit, LabReport, LabSession, default_script, lab_app, lab_report, tower_blast, wall_blast,
    wall_second_blast,
};
use regia::physics::destruction::Destruction;
use regia::physics::world::PhysicsWorld;
use regia::prelude::*;

/// Ticks after which the default script's chunks are still in the air.
const FLYING: u64 = 75;

fn run(app: &mut App, ticks: u64) -> LabReport {
    for _ in 0..ticks {
        app.update();
    }
    lab_report(app.world()).expect("a lab")
}

fn scripted() -> (App, LabKit) {
    let kit = kit::fracture_kit().expect("kit");
    let app = lab_app(&kit, default_script()).expect("lab");
    (app, kit)
}

fn baseline(app: &mut App, kit: &LabKit) -> LabReport {
    let report = run(app, 1);
    assert_eq!(report.server.explosions, 0, "fresh server has no blast history");
    assert_eq!(report.server.playback.broken_bond_count, 0);
    assert_eq!(report.server.playback.debris_count, 0);
    assert_eq!(report.server.playback.instances.len(), kit.structures.len());
    assert!(report.server.playback.instances.iter().all(|i| i.detached == 0));
    for client in &report.clients {
        assert_eq!(client.explosions, 0);
        assert_eq!(client.playback.broken_bond_count, 0);
        assert_eq!(client.playback.debris_count, 0);
        assert_eq!(client.playback.instances.len(), kit.structures.len());
        assert!(client.playback.instances.iter().all(|i| i.detached == 0));
        assert!(client.in_step);
    }
    assert!(report.clients_agree);
    let session = app.world().get_non_send::<LabSession>().expect("lab session");
    let server_bodies = session.server_world().resource::<PhysicsWorld>().bodies.len();
    let host_bodies = app.world().resource::<PhysicsWorld>().bodies.len();
    let other_bodies = session.other_world().resource::<PhysicsWorld>().bodies.len();
    assert!(server_bodies > 0 && host_bodies > 0 && other_bodies > 0);
    assert_eq!(host_bodies, other_bodies, "client body counts agree at baseline");
    assert_world_counts(app);
    report
}

fn alternate(kit: &LabKit) -> LabKit {
    let mut alternate = kit.clone();
    for structure in &mut alternate.structures {
        structure.seed = structure.seed.wrapping_add(0x51_0a);
    }
    alternate
}

fn alternate_script() -> Vec<destruction_lab::ScriptedBlast> {
    vec![
        destruction_lab::ScriptedBlast {
            tick: 12,
            explosion: tower_blast(),
        },
        destruction_lab::ScriptedBlast {
            tick: 28,
            explosion: wall_second_blast(),
        },
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct PendingCounts {
    inbox_datagrams: usize,
    outbox_datagrams: usize,
    blast_messages: usize,
    destruction_items: u32,
}

fn pending_counts(worlds: [&World; 3]) -> [PendingCounts; 3] {
    std::array::from_fn(|index| {
        let world = worlds[index];
        let node = world.resource::<regia::net::runtime::NetNode>();
        assert!(node.inbox.is_empty(), "undelivered datagrams remain: {node:?}");
        let net = world.resource::<regia::framework::destruction_net::DestructionNet>();
        let blast_messages = world
            .resource::<regia::net::message::NetMessages<destruction_lab::LabBlast>>()
            .len();
        let destruction_items = net.stats().pending_items;
        assert_eq!(destruction_items, 0, "destruction stream items remain");
        assert_eq!(blast_messages, 0, "delivered blast messages remain");
        PendingCounts {
            inbox_datagrams: node.inbox.len(),
            outbox_datagrams: node.outbox.len(),
            blast_messages,
            destruction_items,
        }
    })
}

fn assert_world_counts(app: &App) {
    let session = app.world().get_non_send::<LabSession>().expect("lab session");
    let worlds = [app.world(), session.server_world(), session.other_world()];
    for world in worlds {
        assert!(world.entities().count_spawned() > 0, "lab process has entities");
        assert!(
            !world.resource::<PhysicsWorld>().bodies.is_empty(),
            "lab process has bodies"
        );
    }
    assert_eq!(
        app.world().resource::<PhysicsWorld>().bodies.len(),
        session.other_world().resource::<PhysicsWorld>().bodies.len(),
        "client physics body counts agree"
    );
    assert_eq!(
        app.world().entities().count_spawned(),
        session.other_world().entities().count_spawned(),
        "client entity counts agree"
    );
}

#[test]
fn queued_blast_does_not_cross_an_app_boundary() {
    let (mut abandoned, kit) = scripted();
    abandoned
        .world_mut()
        .non_send_mut::<LabSession>()
        .fire(wall_blast());
    let queued = lab_report(abandoned.world()).expect("abandoned report");
    assert_eq!(queued.ticks, 0);
    assert_eq!(queued.server.explosions, 0, "blast waits for the next fixed tick");
    let session = abandoned
        .world()
        .get_non_send::<LabSession>()
        .expect("lab session");
    let abandoned_pending =
        pending_counts([abandoned.world(), session.server_world(), session.other_world()]);
    assert!(
        abandoned_pending
            .iter()
            .all(|counts| counts.outbox_datagrams == 0)
    );
    // No fixed tick has consumed this pending blast. Dropping the whole app is the session end.
    drop(abandoned);

    let mut next = lab_app(&kit, Vec::new()).expect("second lab");
    let report = baseline(&mut next, &kit);
    let session = next.world().get_non_send::<LabSession>().expect("lab session");
    let next_pending = pending_counts([next.world(), session.server_world(), session.other_world()]);
    assert!(report.clients_agree);
    drop(next);

    let mut control = lab_app(&kit, Vec::new()).expect("fresh baseline control");
    let _ = baseline(&mut control, &kit);
    let session = control.world().get_non_send::<LabSession>().expect("lab session");
    let control_pending = pending_counts([control.world(), session.server_world(), session.other_world()]);
    assert_eq!(next_pending, control_pending, "no stale datagrams in the new app");
}

#[test]
fn playback_from_a_dropped_session_does_not_change_the_next_script() {
    let kit = kit::fracture_kit().expect("kit");
    let alternate_kit = alternate(&kit);
    let script = alternate_script();
    let mut interrupted = lab_app(&kit, default_script()).expect("first lab");
    let active = run(&mut interrupted, FLYING);
    assert!(
        active.server.playback.debris_count > 0,
        "first session is playing chunks"
    );
    assert!(active.server.explosions > 0);
    assert!(active.clients_agree);
    assert_world_counts(&interrupted);
    let active_session = interrupted
        .world()
        .get_non_send::<LabSession>()
        .expect("lab session");
    let _ = pending_counts([
        interrupted.world(),
        active_session.server_world(),
        active_session.other_world(),
    ]);
    drop(interrupted);

    let mut second = lab_app(&alternate_kit, script.clone()).expect("second lab");
    let second_report = run(&mut second, FLYING);
    let second_session = second.world().get_non_send::<LabSession>().expect("lab session");
    let second_pending = pending_counts([
        second.world(),
        second_session.server_world(),
        second_session.other_world(),
    ]);
    assert!(second_report.clients_agree, "second clients: {second_report:?}");
    for client in &second_report.clients {
        assert_eq!(
            client.playback.structure_hash,
            second_report.server.playback.structure_hash
        );
    }
    drop(second);

    let mut control = lab_app(&alternate_kit, script).expect("fresh control");
    let control_report = run(&mut control, FLYING);
    let control_session = control.world().get_non_send::<LabSession>().expect("lab session");
    let control_pending = pending_counts([
        control.world(),
        control_session.server_world(),
        control_session.other_world(),
    ]);
    assert_eq!(
        second_pending, control_pending,
        "pending datagram counts match a fresh run"
    );
    assert_eq!(
        second_report, control_report,
        "old playback/events must not reach the new app"
    );
}

#[test]
fn sessions_a_b_a_have_the_same_canonical_reports() {
    let kit = kit::fracture_kit().expect("kit");
    let a_script = default_script();
    let b_kit = alternate(&kit);
    let b_script = alternate_script();
    let execute = |kit: &LabKit, script: Vec<destruction_lab::ScriptedBlast>| {
        let mut app = lab_app(kit, script).expect("lab");
        let report = run(&mut app, FLYING);
        assert_world_counts(&app);
        let session = app.world().get_non_send::<LabSession>().expect("lab session");
        let pending = pending_counts([app.world(), session.server_world(), session.other_world()]);
        assert!(report.clients_agree, "clients diverged: {report:?}");
        for client in &report.clients {
            assert_eq!(
                client.playback.structure_hash,
                report.server.playback.structure_hash
            );
            assert_eq!(client.explosions, report.server.explosions);
        }
        (report, pending)
    };
    let (a1, a1_pending) = execute(&kit, a_script.clone());
    let (b, _) = execute(&b_kit, b_script);
    let (a2, a2_pending) = execute(&kit, a_script);
    assert_ne!(a1, b, "B uses a distinct script and structure seed");
    assert_eq!(a1, a2, "A/B/A app sessions are independent");
    assert_eq!(a1_pending, a2_pending, "A sessions have the same pending counts");
}

#[test]
fn the_kit_is_really_fractured() {
    let kit = kit::fracture_kit().expect("kit");
    assert_eq!(kit.structures.len(), 2);
    for s in &kit.structures {
        assert!(s.asset.chunks.len() > 16, "{}", s.asset.name);
        assert!(s.asset.bonds.len() > s.asset.chunks.len(), "{}", s.asset.name);
        assert!(s.asset.chunks.iter().any(|c| c.support), "{}", s.asset.name);
        let look = s.look.as_ref().expect("look");
        assert_eq!(look.set.chunks.len(), s.asset.chunks.len());
        assert!(look.intact.indices.len() >= 36, "{}", s.asset.name);
    }
}

#[test]
fn both_clients_play_what_the_server_decided() {
    let (mut app, kit) = scripted();
    let report = run(&mut app, FLYING);
    // The blasts did something, on the wall and on the tower.
    assert_eq!(report.server.explosions, 3);
    assert_eq!(report.server.playback.instances.len(), kit.structures.len());
    for instance in &report.server.playback.instances {
        assert!(!instance.broken_bonds.is_empty(), "{instance:?}");
        assert!(instance.detached > 0, "{instance:?}");
        assert!(instance.attached > 0, "{instance:?}");
    }
    assert!(report.server.playback.debris_count > 0, "{report:?}");
    // The effects table played the breaks on every process, capped per step (ADR-2734).
    for process in [&report.server, &report.clients[0], &report.clients[1]] {
        assert!(process.fx.planned > 0, "{:?}", process.fx);
        assert!(process.fx.messages >= process.fx.planned, "{:?}", process.fx);
    }
    let fx = report.server.fx;
    assert!(
        fx.dropped_tag_cap + fx.dropped_step_cap > 0,
        "a blast is a burst: {fx:?}"
    );
    // Clients run the same ticks and hold the server's decisions, bond for bond.
    for client in &report.clients {
        assert_eq!(client.playback.tick, report.clients[0].playback.tick);
        assert_eq!(
            client.playback.broken_bond_digest,
            report.server.playback.broken_bond_digest
        );
        assert_eq!(
            client.playback.structure_hash,
            report.server.playback.structure_hash
        );
        assert_eq!(
            client.playback.instances[0].broken_bonds,
            report.server.playback.instances[0].broken_bonds
        );
        assert!(client.in_step, "{client:?}");
        assert_eq!(client.checks_failed, 0, "{client:?}");
        assert_eq!(client.explosions, 3, "the blasts reached the client");
        assert!(client.playback.debris_count > 0, "{client:?}");
    }
    // And they agree with each other on where every chunk is, exactly.
    assert_eq!(report.clients[0].playback, report.clients[1].playback);
    assert!(report.clients_agree);
}

#[test]
fn the_second_blast_pushes_debris_on_the_server_and_on_both_clients() {
    let (mut app, _) = scripted();
    let before = run(&mut app, 41);
    assert_eq!(before.server.chunks_pushed, 0, "nothing flew at the first blast");
    let after = run(&mut app, 2);
    for process in [&after.server, &after.clients[0], &after.clients[1]] {
        assert!(process.chunks_pushed > 0, "{process:?}");
        assert_eq!(process.explosions, 2, "{process:?}");
    }
    assert_eq!(after.clients[0].chunks_pushed, after.clients[1].chunks_pushed);
    assert_eq!(after.clients[0].chunks_pushed, after.server.chunks_pushed);
    assert_eq!(
        after.clients[0].chunks_hit, after.clients[0].chunks_pushed,
        "replicas take no damage"
    );
    assert!(
        after.server.chunks_hit > after.server.chunks_pushed,
        "{:?}",
        after.server
    );
}

#[test]
fn clients_do_not_decide_anything() {
    let (mut app, _) = scripted();
    let _ = run(&mut app, 2);
    assert_eq!(
        app.world_mut()
            .resource_mut::<Destruction>()
            .world
            .damage(regia::physics::destruction::Damage {
                target: None,
                kind: regia::physics::destruction::DamageKind::Explosion {
                    center: [0.0, 1.0, 0.0],
                    radius: 10.0,
                    impulse: 1e6,
                },
            })
            .map_err(|e| e.code()),
        Err("destruction_replica")
    );
    let report = run(&mut app, 2);
    assert_eq!(report.clients[0].playback.broken_bond_count, 0);
}

#[test]
fn a_run_repeats_bit_for_bit() {
    let digests = |_: ()| {
        let (mut app, _) = scripted();
        let report = run(&mut app, FLYING);
        (
            report.clients[0].playback.chunk_pose_digest,
            report.clients[0].playback.broken_bond_digest,
        )
    };
    assert_eq!(digests(()), digests(()));
}

#[test]
fn the_pose_digest_notices_one_chunk_off_by_a_hair() {
    let (mut app, _) = scripted();
    let report = run(&mut app, FLYING);
    assert!(report.clients_agree);
    {
        let mut lab = app.world_mut().non_send_mut::<LabSession>();
        let other = lab.other_world_mut();
        let body = other
            .resource::<Destruction>()
            .physics
            .active()
            .next()
            .map(|a| a.body)
            .expect("debris");
        let mut physics = other.resource_mut::<PhysicsWorld>();
        let rb = physics.bodies.get_mut(body).expect("body");
        let mut at = *rb.position();
        at.translation.y += 1e-4;
        rb.set_position(at, true);
    }
    let shifted = lab_report(app.world()).expect("lab");
    assert!(!shifted.clients_agree);
    assert_eq!(
        shifted.clients[0].playback.broken_bond_digest,
        shifted.clients[1].playback.broken_bond_digest
    );
    assert_ne!(
        shifted.clients[0].playback.chunk_pose_digest,
        shifted.clients[1].playback.chunk_pose_digest
    );
}

#[test]
fn the_script_stays_put_while_paused_and_a_lab_ticks_with_its_host() {
    let (mut app, _) = scripted();
    let at_ten = run(&mut app, 10);
    assert_eq!(at_ten.ticks, 10);
    assert_eq!(at_ten.clients[0].playback.tick, at_ten.clients[1].playback.tick);
    app.world_mut().resource_mut::<Time>().paused = true;
    let still = run(&mut app, 50);
    assert_eq!(
        still.ticks, 10,
        "a paused host runs no fixed tick, so nothing else does"
    );
    assert_eq!(still.server.explosions, 0);
    app.world_mut().resource_mut::<Time>().paused = false;
    let resumed = run(&mut app, 25);
    assert_eq!(resumed.ticks, 35);
    assert_eq!(resumed.server.explosions, 1, "tick 30 came, once");
}

#[test]
fn the_blasts_are_sized_for_the_lab() {
    for blast in [wall_blast(), wall_second_blast(), tower_blast()] {
        assert_eq!(blast.validate(), Ok(()));
    }
    let (mut app, _) = scripted();
    let report = run(&mut app, FLYING);
    let tower = &report.clients[0].playback.instances[1];
    assert!(tower.detached >= 20, "the tower comes down: {tower:?}");
    assert!(tower.attached > 0, "its base stands: {tower:?}");
}
