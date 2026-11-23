// SPDX-License-Identifier: MPL-2.0
//! Spec 70 § 10: the network model and net messages, over the in-memory `LoopbackHub` at fixed
//! steps (no sockets, no wall clock).

use glam::Vec3;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::handshake::DisconnectReason;
use regia_net::message::{Channel, register_net_message};
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::testing::{FrameFate, LinkConditions, LoopbackHub, replay_client};
use regia_net::*;

// ---- the test game ------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Fire {
    power: u32,
}
impl NetMessage for Fire {
    const DIRECTION: Direction = Direction::ToServer;
    fn validate(&self, _cx: &ValidateContext) -> Verdict {
        match self.power {
            0 => Verdict::Reject("no power"),
            99 => Verdict::Kick("cheat"),
            _ => Verdict::Accept,
        }
    }
}
impl ToServerMessage for Fire {}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct HitConfirmed {
    damage: u32,
}
impl NetMessage for HitConfirmed {
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for HitConfirmed {}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Explosion {
    at: Vec3,
}
impl NetMessage for Explosion {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for Explosion {}

/// An unreliable-sequenced counter stream.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Ping {
    n: u32,
}
impl NetMessage for Ping {
    const DIRECTION: Direction = Direction::ToOwner;
    const CHANNEL: Channel = Channel::UnreliableSequenced;
}
impl ToOwnerMessage for Ping {}

/// An unreliable message checked against state.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct HpCheck {
    expect: u32,
}
impl NetMessage for HpCheck {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: Channel = Channel::Unreliable;
}
impl BroadcastMessage for HpCheck {}

/// A large message for the size bound.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Blob {
    #[replicate(max_len = 200000)]
    bytes: Vec<u8>,
}
impl NetMessage for Blob {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for Blob {}

/// An always-admitted to-server message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Beat;
impl NetMessage for Beat {
    const DIRECTION: Direction = Direction::ToServer;
    const ALWAYS_ADMIT: bool = true;
}
impl ToServerMessage for Beat {}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: u32,
}

#[derive(Component)]
struct Pawn;

fn setup(world: &mut World) {
    register_net_message::<Fire>(world);
    register_net_message::<HitConfirmed>(world);
    register_net_message::<Explosion>(world);
    register_net_message::<Ping>(world);
    register_net_message::<HpCheck>(world);
    register_net_message::<Blob>(world);
    register_net_message::<Beat>(world);
    register_replicated::<Health>(world, ReplicateOptions::default());
}

/// What a process's handlers saw: `(tick, kind, value)`.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct Log(Vec<(u32, &'static str, u32)>);

fn log_system(
    node: Res<NetNode>,
    fire: Res<NetMessages<Fire>>,
    hit: Res<NetMessages<HitConfirmed>>,
    boom: Res<NetMessages<Explosion>>,
    ping: Res<NetMessages<Ping>>,
    mut log: ResMut<Log>,
) {
    let t = node.tick.0;
    for m in fire.iter() {
        log.0.push((t, "fire", m.msg.power));
    }
    for m in hit.iter() {
        log.0.push((t, "hit", m.msg.damage));
    }
    for m in boom.iter() {
        log.0.push((t, "boom", m.msg.at.x as u32));
    }
    for m in ping.iter() {
        log.0.push((t, "ping", m.msg.n));
    }
}

/// Server rule: a fire confirms a hit to the shooter's owner and broadcasts an explosion.
fn on_fire(fire: Res<NetMessages<Fire>>, mut net: NetSender) {
    let shots: Vec<(Entity, u32)> = fire
        .iter()
        .filter_map(|m| m.target.map(|t| (t, m.msg.power)))
        .collect();
    for (t, power) in shots {
        let _ = net.send_to_owner(t, HitConfirmed { damage: power * 2 });
        let _ = net.broadcast(
            t,
            Explosion {
                at: Vec3::new(power as f32, 0.0, 0.0),
            },
        );
    }
}

fn add_log(world: &mut World, schedule: &mut Schedule) {
    world.init_resource::<Log>();
    schedule.add_systems(log_system);
}

fn log_of(world: &World) -> Vec<(u32, &'static str, u32)> {
    world.resource::<Log>().0.clone()
}

fn send<M: Send + 'static>(
    world: &mut World,
    f: impl FnOnce(&mut NetSender) -> M + Send + Sync + 'static,
) -> M {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| (f.take().expect("runs once"))(&mut net))
        .expect("system runs")
}

fn spawn_pawn(world: &mut World, owner: Option<PeerId>) -> Entity {
    let mut e = world.spawn((Pawn, Replicated::default(), Health { hp: 100 }));
    if let Some(o) = owner {
        e.insert(NetOwner(o));
    }
    e.id()
}

fn client_entity(world: &World, id: NetId) -> Option<Entity> {
    world.resource::<NetEntities>().entity(id)
}

fn counters<M: 'static>(world: &World) -> regia_net::message::MessageCounters {
    let reg = world.resource::<regia_net::message::NetMessageRegistry>();
    let i = reg.index_of::<M>().expect("registered");
    reg.counters[usize::from(i)]
}

// ---- tests --------------------------------------------------------------------------------------

/// Net tools (ADR-1909): `bytes_sent` counts one payload copy per receiving peer, `bytes_received`
/// the payload bytes that arrived, so a lossless run balances.
#[test]
fn message_bytes_are_counted_per_copy() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    send(&mut hub.server.world, move |n| {
        for i in 0..3 {
            n.broadcast(pawn, HpCheck { expect: i }).expect("queued");
        }
    });
    hub.run(4);
    let sv = counters::<HpCheck>(&hub.server.world);
    let ca = counters::<HpCheck>(&hub.clients[a].world);
    let cb = counters::<HpCheck>(&hub.clients[b].world);
    assert_eq!(sv.sent, 3);
    assert!(
        sv.bytes_sent >= 3 * 2,
        "two copies of three payloads: {}",
        sv.bytes_sent
    );
    assert_eq!(sv.bytes_received, 0);
    assert_eq!(ca.bytes_received, cb.bytes_received);
    assert_eq!(sv.bytes_sent, ca.bytes_received + cb.bytes_received);
    assert_eq!(ca.bytes_sent, 0, "clients sent none");
}

/// § 10 test 1: offline and listen-host runs of the same game produce the same event log, tick by
/// tick.
#[test]
fn offline_parity_with_the_listen_host() {
    fn run(mode: NetMode) -> Vec<(u32, &'static str, u32)> {
        let mut hub = LoopbackHub::new(mode, 7, setup);
        if mode == NetMode::ListenServer {
            hub.add_client();
        }
        let s = &mut hub.server;
        add_log(&mut s.world, &mut s.gameplay);
        s.gameplay.add_systems(on_fire.run_if(on_server()));
        let pawn = spawn_pawn(&mut s.world, None);
        for step in 0..12 {
            if step == 3 || step == 7 {
                let power = step as u32;
                send(&mut hub.server.world, move |net| {
                    net.send_to_server(pawn, Fire { power })
                })
                .expect("host fires");
            }
            hub.step();
        }
        log_of(&hub.server.world)
    }
    let offline = run(NetMode::Offline);
    let listen = run(NetMode::ListenServer);
    assert!(!offline.is_empty());
    assert_eq!(offline, listen);
    // Fire runs at the next tick's Apply; its replies one tick later.
    assert!(offline.contains(&(4, "fire", 3)), "{offline:?}");
    assert!(offline.contains(&(5, "hit", 6)), "{offline:?}");
    assert!(offline.contains(&(5, "boom", 3)), "{offline:?}");
}

/// § 10 test 4: possession sets `NetOwner`; the owning client sees `NetOwnedLocally` and
/// `NetRole::Predicted` at the same tick; the other client never sees `NetOwnedLocally`.
#[test]
fn possession_marks_the_owner_copy_only() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.server.world.entity_mut(pawn).insert(NetPredicted);
    hub.run(3);
    let id = *hub.server.world.get::<NetId>(pawn).expect("net id");
    let ca = client_entity(&hub.clients[a].world, id).expect("copy a");
    let cb = client_entity(&hub.clients[b].world, id).expect("copy b");
    assert_eq!(hub.clients[a].world.get::<NetRole>(ca), Some(&NetRole::Proxy));

    regia_net::model::set_possession_owner(&mut hub.server.world, pawn, Some(hub.clients[a].peer));
    hub.run(2);
    let wa = &hub.clients[a].world;
    assert!(wa.get::<NetOwnedLocally>(ca).is_some());
    assert_eq!(wa.get::<NetRole>(ca), Some(&NetRole::Predicted));
    let wb = &hub.clients[b].world;
    assert!(wb.get::<NetOwnedLocally>(cb).is_none());
    assert_eq!(wb.get::<NetRole>(cb), Some(&NetRole::Proxy));

    regia_net::model::set_possession_owner(&mut hub.server.world, pawn, None);
    hub.run(2);
    let wa = &hub.clients[a].world;
    assert!(wa.get::<NetOwnedLocally>(ca).is_none());
    assert_eq!(wa.get::<NetRole>(ca), Some(&NetRole::Proxy));
}

/// § 10 test 5: the § 6.3 table, one case per row.
#[test]
fn where_a_message_runs() {
    let mut hub = LoopbackHub::new(NetMode::ListenServer, 2, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let pa = hub.clients[a].peer;
    for p in std::iter::once(&mut hub.server).chain(hub.clients.iter_mut()) {
        add_log(&mut p.world, &mut p.gameplay);
    }
    let owned_a = spawn_pawn(&mut hub.server.world, Some(pa));
    let server_owned = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    let id_a = *hub.server.world.get::<NetId>(owned_a).expect("id");
    let id_s = *hub.server.world.get::<NetId>(server_owned).expect("id");
    let ca = client_entity(&hub.clients[a].world, id_a).expect("copy");
    let cs_on_a = client_entity(&hub.clients[a].world, id_s).expect("copy");

    // Row 1: ToServer from the owning client runs on the server only.
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(ca, Fire { power: 5 })
    })
    .expect("sent");
    // Row 2: ToServer from a client on an entity it does not own: dropped as not_owner.
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(cs_on_a, Fire { power: 6 })
    })
    .expect("sent");
    // Row 3: ToServer sent by the listen host: runs on the server next tick.
    send(&mut hub.server.world, move |n| {
        n.send_to_server(server_owned, Fire { power: 7 })
    })
    .expect("sent");
    // Row 4: ToOwner to a client peer: that client only.
    send(&mut hub.server.world, move |n| {
        n.send_to_owner(owned_a, HitConfirmed { damage: 1 })
    })
    .expect("sent");
    // Row 5: ToOwner to a server-owned target: the server.
    send(&mut hub.server.world, move |n| {
        n.send_to_owner(server_owned, HitConfirmed { damage: 2 })
    })
    .expect("sent");
    // Row 6: ToOwner from a client: refused.
    let r = send(&mut hub.clients[a].world, move |n| {
        n.send_to_owner(ca, HitConfirmed { damage: 3 })
    });
    assert_eq!(r, Err(NetSendError::NotServer));
    // Row 7: Broadcast from the server: server and every client.
    send(&mut hub.server.world, move |n| {
        n.broadcast(
            server_owned,
            Explosion {
                at: Vec3::new(8.0, 0.0, 0.0),
            },
        )
    })
    .expect("sent");
    // Row 8: Broadcast from a client: that client only.
    let cs_on_b = client_entity(&hub.clients[b].world, id_s).expect("copy");
    send(&mut hub.clients[b].world, move |n| {
        n.broadcast(
            cs_on_b,
            Explosion {
                at: Vec3::new(9.0, 0.0, 0.0),
            },
        )
    })
    .expect("sent");
    hub.run(4);

    let kinds = |w: &World, kind: &str| -> Vec<u32> {
        log_of(w)
            .into_iter()
            .filter(|(_, k, _)| *k == kind)
            .map(|(_, _, v)| v)
            .collect()
    };
    let (sv, wa, wb) = (&hub.server.world, &hub.clients[a].world, &hub.clients[b].world);
    assert_eq!(
        kinds(sv, "fire"),
        vec![7, 5],
        "host fire runs next tick, client fire on arrival"
    );
    assert!(kinds(wa, "fire").is_empty() && kinds(wb, "fire").is_empty());
    assert_eq!(counters::<Fire>(sv).not_owner, 1);
    assert_eq!(kinds(wa, "hit"), vec![1]);
    assert!(kinds(wb, "hit").is_empty());
    assert_eq!(kinds(sv, "hit"), vec![2]);
    assert_eq!(kinds(sv, "boom"), vec![8]);
    assert_eq!(kinds(wa, "boom"), vec![8]);
    let mut b_booms = kinds(wb, "boom");
    b_booms.sort_unstable();
    assert_eq!(b_booms, vec![8, 9], "the client broadcast stays local");
}

/// § 10 test 6: `Reject` drops and counts; `Kick` disconnects with `InvalidMessage`; malformed
/// bytes disconnect with `MalformedPacket`.
#[test]
fn validation_rejects_kicks_and_malformed_bytes_disconnect() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let pa = hub.clients[a].peer;
    let pb = hub.clients[b].peer;
    let pawn_a = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(3);
    let id = *hub.server.world.get::<NetId>(pawn_a).expect("id");
    let ca = client_entity(&hub.clients[a].world, id).expect("copy");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(ca, Fire { power: 0 })
    })
    .expect("sent");
    hub.run(3);
    assert_eq!(counters::<Fire>(&hub.server.world).rejected, 1);
    assert!(hub.clients[a].connected);
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(ca, Fire { power: 99 })
    })
    .expect("sent");
    hub.run(3);
    assert!(!hub.clients[a].connected);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::InvalidMessage)
    );
    // Malformed: a client frame whose Fire payload is empty (a u32 varint is missing).
    let mut w = BitWriter::new();
    w.write_bits(3, 8); // CMSG
    w.write_varint(0);
    w.write_varint(1); // one message
    w.write_varint(0); // type 0 = Fire
    w.write_bool(false);
    w.write_varint(0); // empty payload
    w.write_varint(0); // no resync
    hub.server
        .world
        .resource_mut::<NetNode>()
        .inbox
        .push(regia_net::runtime::Datagram {
            peer: pb,
            channel: Channel::ReliableOrdered,
            bytes: w.finish(),
        });
    hub.run(2);
    assert!(!hub.clients[b].connected);
    assert_eq!(
        hub.clients[b].world.resource::<NetNode>().closed,
        Some(DisconnectReason::MalformedPacket)
    );
}

fn ordering_session(seed: u64) -> (LoopbackHub, Vec<u32>, Vec<u32>) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    let a = hub.add_client();
    let lossy = LinkConditions {
        loss: 0.2,
        reorder: 0.2,
        duplicate: 0.05,
        delay_ticks: 1,
        ..LinkConditions::default()
    };
    hub.down = lossy;
    hub.up = lossy;
    hub.clients[a].world.resource_mut::<NetNode>().record = true;
    let c = &mut hub.clients[a];
    add_log(&mut c.world, &mut c.gameplay);
    let pa = hub.clients[a].peer;
    let e1 = spawn_pawn(&mut hub.server.world, Some(pa));
    let e2 = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(2);
    let mut n = 0u32;
    for tick in 0..100u32 {
        send(&mut hub.server.world, move |net| {
            for i in 0..100u32 {
                let target = if i % 2 == 0 { e1 } else { e2 };
                net.send_to_owner(target, HitConfirmed { damage: n + i })
                    .expect("queued");
            }
            net.send_to_owner(e1, Ping { n: tick }).expect("queued");
        });
        n += 100;
        hub.step();
    }
    hub.down = LinkConditions::default();
    hub.run(60);
    let log = log_of(&hub.clients[a].world);
    let hits: Vec<u32> = log
        .iter()
        .filter(|(_, k, _)| *k == "hit")
        .map(|(_, _, v)| *v)
        .collect();
    let pings: Vec<u32> = log
        .iter()
        .filter(|(_, k, _)| *k == "ping")
        .map(|(_, _, v)| *v)
        .collect();
    (hub, hits, pings)
}

/// § 10 test 7: with 20 % loss and reordering, reliable-ordered messages to two entities arrive in
/// send order, none lost or duplicated over 10,000; a sequenced stream never goes backwards.
#[test]
fn ordering_under_loss_and_reorder() {
    let (_hub, hits, pings) = ordering_session(11);
    assert_eq!(hits.len(), 10_000);
    assert!(
        hits.windows(2).all(|w| w[0] + 1 == w[1]),
        "send order, no gaps or duplicates"
    );
    assert!(!pings.is_empty());
    assert!(
        pings.windows(2).all(|w| w[0] < w[1]),
        "sequenced: never older after newer"
    );
    assert!(pings.len() < 100, "some unreliable pings were lost at 20 % loss");
}

/// § 10 test 13: the packet log of test 7 fed to a fresh client twice gives identical delivered
/// logs.
#[test]
fn replay_of_a_packet_log_is_deterministic() {
    let (hub, _, _) = ordering_session(12);
    let log = hub.client_logs[0].clone();
    let steps = hub.step_count();
    let peer = hub.clients[0].peer;
    let first = replay_client(setup, peer, &log, steps);
    let second = replay_client(setup, peer, &log, steps);
    assert!(first.len() > 10_000);
    assert_eq!(first, second);
    let live = hub.clients[0].world.resource::<NetNode>().delivered_log.clone();
    assert_eq!(first, live, "the replay matches what the live client delivered");
}

#[derive(Resource, Default)]
struct Checks {
    ok: u32,
    bad: u32,
}

fn hp_check(msgs: Res<NetMessages<HpCheck>>, q: Query<&Health>, mut checks: ResMut<Checks>) {
    for m in msgs.iter() {
        let Some(hp) = m.target.and_then(|t| q.get(t).ok()) else {
            checks.bad += 1;
            continue;
        };
        if hp.hp >= m.msg.expect {
            checks.ok += 1;
        } else {
            checks.bad += 1;
        }
    }
}

/// § 10 test 8: a message sent at tick T sees state at least T; with the state held back behind
/// the message, the message waits and runs after the state.
#[test]
fn messages_see_their_tick_state() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 4, setup);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<Checks>();
    hub.clients[a].gameplay.add_systems(hp_check);
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    hub.down.reliable_extra_ticks = 4;
    for _ in 0..20 {
        let t = hub.server_tick().0;
        hub.server.world.get_mut::<Health>(pawn).expect("health").hp = t;
        send(&mut hub.server.world, move |n| {
            n.broadcast(pawn, HpCheck { expect: t })
        })
        .expect("sent");
        hub.step();
    }
    hub.down.reliable_extra_ticks = 0;
    hub.run(10);
    let checks = hub.clients[a].world.resource::<Checks>();
    assert_eq!(checks.bad, 0);
    assert_eq!(checks.ok, 20);
}

/// § 10 test 9: a message whose target spawn is 10 ticks late runs after the spawn; 40 ticks late,
/// it is dropped as unresolved.
#[test]
fn unresolved_targets_wait_then_drop() {
    for (delay, expect_run) in [(10u64, true), (40, false)] {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let a = hub.add_client();
        let c = &mut hub.clients[a];
        add_log(&mut c.world, &mut c.gameplay);
        hub.run(2);
        hub.down.reliable_extra_ticks = delay;
        // On the state channel the spawn is a state frame, sent again until it is acknowledged.
        let due = hub.step_count() + delay;
        hub.down_filter = Some(Box::new(move |d, step| {
            if d.bytes.first() == Some(&7) {
                FrameFate::Delay(due.saturating_sub(step))
            } else {
                FrameFate::Deliver
            }
        }));
        let pawn = spawn_pawn(&mut hub.server.world, None);
        hub.step(); // assigns the NetId and sends the spawn (held back)
        send(&mut hub.server.world, move |n| {
            n.broadcast(pawn, HpCheck { expect: 0 })
        })
        .expect("sent");
        hub.clients[a].world.init_resource::<Checks>();
        hub.clients[a].gameplay.add_systems(hp_check);
        hub.step();
        hub.down.reliable_extra_ticks = 0;
        hub.run(delay as usize + 10);
        let w = &hub.clients[a].world;
        let ran = w.resource::<Checks>().ok;
        assert_eq!(ran == 1, expect_run, "delay {delay}");
        assert_eq!(
            counters::<HpCheck>(w).unresolved,
            u64::from(!expect_run),
            "delay {delay}"
        );
    }
}

/// § 10 test 10: ownership moves at tick T; a to-server message from the old owner processed after
/// that is dropped as not_owner.
#[test]
fn old_owner_messages_are_dropped() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 6, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    add_log(&mut hub.server.world, &mut hub.server.gameplay);
    let pawn = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(3);
    let id = *hub.server.world.get::<NetId>(pawn).expect("id");
    let ca = client_entity(&hub.clients[a].world, id).expect("copy");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(ca, Fire { power: 1 })
    })
    .expect("sent");
    hub.server.world.entity_mut(pawn).insert(NetOwner(pb));
    hub.run(3);
    assert_eq!(counters::<Fire>(&hub.server.world).not_owner, 1);
    assert!(log_of(&hub.server.world).is_empty());
}

/// § 10 test 11: 17 unreliable messages in one tick to one entity drop the oldest; a reliable queue
/// over 4,096 disconnects with `ReliableOverflow`; a 65 KiB message is `TooLarge`.
#[test]
fn queue_bounds() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let pa = hub.clients[a].peer;
    let pawn = spawn_pawn(&mut hub.server.world, None);
    let pawn_a = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(3);
    send(&mut hub.server.world, move |n| {
        for i in 0..17 {
            n.broadcast(pawn, HpCheck { expect: i }).expect("queued");
        }
    });
    hub.run(3);
    let sv = &hub.server.world;
    assert_eq!(counters::<HpCheck>(sv).dropped_overflow, 2, "one per client");
    let wa = &hub.clients[a].world;
    assert_eq!(counters::<HpCheck>(wa).delivered, 16);

    let too_large = send(&mut hub.server.world, move |n| {
        n.broadcast(
            pawn,
            Blob {
                bytes: vec![7; 65 * 1024],
            },
        )
    });
    assert_eq!(too_large, Err(NetSendError::TooLarge));

    send(&mut hub.server.world, move |n| {
        for i in 0..4097 {
            n.send_to_owner(pawn_a, HitConfirmed { damage: i })
                .expect("queued");
        }
    });
    hub.run(2);
    assert!(!hub.clients[a].connected);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ReliableOverflow)
    );
    assert!(hub.clients[b].connected, "only the overflowing connection goes");
}

/// § 10 test 12 (end to end; the escalation timings are unit-tested in `flood.rs`): a client
/// sending 3x its rate gets drops and becomes `Slowed`; an `ALWAYS_ADMIT` type is never blocked.
#[test]
fn flood_guard_end_to_end() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 9, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let pawn = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(3);
    let id = *hub.server.world.get::<NetId>(pawn).expect("id");
    let ca = client_entity(&hub.clients[a].world, id).expect("copy");
    for _ in 0..120 {
        send(&mut hub.clients[a].world, move |n| {
            for _ in 0..6 {
                n.send_to_server(ca, Fire { power: 1 }).expect("queued");
            }
            for _ in 0..20 {
                n.send_session_to_server(Beat).expect("queued");
            }
        });
        hub.step();
    }
    hub.run(3);
    let sv = &hub.server.world;
    assert!(counters::<Fire>(sv).rate_limited > 0);
    assert_eq!(counters::<Beat>(sv).rate_limited, 0);
    assert_eq!(counters::<Beat>(sv).delivered, 120 * 20);
    let (state, _, last) = sv.resource::<NetNode>().peer_flood(pa).expect("peer");
    assert_eq!(state, regia_net::flood::FloodState::Slowed);
    assert!(last.is_some());
}

/// § 10 test 2 (with an app): `on_server()` systems are skipped on a client hub process.
#[test]
fn client_processes_skip_server_systems() {
    #[derive(Resource, Default)]
    struct Ran(u32);
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10, setup);
    let a = hub.add_client();
    for p in std::iter::once(&mut hub.server).chain(hub.clients.iter_mut()) {
        p.world.init_resource::<Ran>();
        p.gameplay
            .add_systems((|mut r: ResMut<Ran>| r.0 += 1).run_if(on_server()));
    }
    hub.run(5);
    assert_eq!(hub.server.world.resource::<Ran>().0, 5);
    assert_eq!(hub.clients[a].world.resource::<Ran>().0, 0);
}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Aligned {
    n: u32,
}
impl NetMessage for Aligned {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: Channel = Channel::Unreliable;
    const ALIGN_TO_VIEW: bool = true;
}
impl BroadcastMessage for Aligned {}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Cosmetic {
    n: u32,
}
impl NetMessage for Cosmetic {
    const DIRECTION: Direction = Direction::Broadcast;
    const SKIP_PREDICTING_OWNER: bool = true;
}
impl BroadcastMessage for Cosmetic {}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Notice {
    n: u32,
}
impl NetMessage for Notice {
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for Notice {}

fn setup_extra(world: &mut World) {
    setup(world);
    register_net_message::<Aligned>(world);
    register_net_message::<Cosmetic>(world);
    register_net_message::<Notice>(world);
}

/// `ALIGN_TO_VIEW` holds a message until the interpolation tick reaches its tick.
#[test]
fn align_to_view_waits_for_the_view() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 30, setup_extra);
    let a = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    hub.clients[a]
        .world
        .insert_resource(regia_net::runtime::NetInterpTick(NetTick(0)));
    send(&mut hub.server.world, move |n| {
        n.broadcast(pawn, Aligned { n: 1 })
    })
    .expect("sent");
    hub.run(3);
    assert_eq!(
        counters::<Aligned>(&hub.clients[a].world).delivered,
        0,
        "held for the view"
    );
    let applied = hub.clients[a].world.resource::<NetNode>().tick;
    hub.clients[a]
        .world
        .insert_resource(regia_net::runtime::NetInterpTick(applied));
    hub.run(1);
    assert_eq!(counters::<Aligned>(&hub.clients[a].world).delivered, 1);
}

/// `SKIP_PREDICTING_OWNER` broadcasts skip the owner of a predicted target only.
#[test]
fn skip_predicting_owner() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 31, setup_extra);
    let a = hub.add_client();
    let b = hub.add_client();
    let pa = hub.clients[a].peer;
    let pawn = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.server.world.entity_mut(pawn).insert(NetPredicted);
    hub.run(3);
    send(&mut hub.server.world, move |n| {
        n.broadcast(pawn, Cosmetic { n: 1 })
    })
    .expect("sent");
    hub.run(3);
    assert_eq!(
        counters::<Cosmetic>(&hub.clients[a].world).delivered,
        0,
        "the predicting owner played it already"
    );
    assert_eq!(counters::<Cosmetic>(&hub.clients[b].world).delivered, 1);
}

/// Session messages: to one named peer, and broadcast to every client and the server.
#[test]
fn session_messages() {
    let mut hub = LoopbackHub::new(NetMode::ListenServer, 32, setup_extra);
    let a = hub.add_client();
    let b = hub.add_client();
    let pb = hub.clients[b].peer;
    send(&mut hub.server.world, move |n| {
        n.send_session_to_peer(pb, Notice { n: 1 })
    })
    .expect("sent");
    send(&mut hub.server.world, move |n| {
        n.broadcast_session(Explosion { at: Vec3::ZERO })
    })
    .expect("sent");
    let unknown = send(&mut hub.server.world, move |n| {
        n.send_session_to_peer(PeerId(99), Notice { n: 2 })
    });
    assert_eq!(unknown, Err(NetSendError::UnknownPeer));
    hub.run(3);
    assert_eq!(counters::<Notice>(&hub.clients[a].world).delivered, 0);
    assert_eq!(counters::<Notice>(&hub.clients[b].world).delivered, 1);
    for w in [&hub.server.world, &hub.clients[a].world, &hub.clients[b].world] {
        assert_eq!(counters::<Explosion>(w).delivered, 1);
    }
}

// ---- conformance additions (lane spec-28-68, 2026-11-10) -----------------------------------------

/// § 6.2: a send from the wrong kind of process fails with `NotClient`, and a target that is not
/// replicated fails with `NotReplicated`; nothing is sent.
#[test]
fn send_errors_name_the_wrong_process_and_unreplicated_targets() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 12, setup);
    let a = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    // A dedicated server has no client side.
    let r = send(&mut hub.server.world, move |n| {
        n.send_to_server(pawn, Fire { power: 1 })
    });
    assert_eq!(r, Err(NetSendError::NotClient));
    let r = send(&mut hub.server.world, |n| {
        n.send_session_to_server(Fire { power: 1 })
    });
    assert_eq!(r, Err(NetSendError::NotClient));
    // Entities that are not replicated.
    let local = hub.server.world.spawn_empty().id();
    let r = send(&mut hub.server.world, move |n| {
        n.send_to_owner(local, HitConfirmed { damage: 1 })
    });
    assert_eq!(r, Err(NetSendError::NotReplicated));
    let r = send(&mut hub.server.world, move |n| {
        n.broadcast(local, Explosion { at: Vec3::ZERO })
    });
    assert_eq!(r, Err(NetSendError::NotReplicated));
    let mine = hub.clients[a].world.spawn_empty().id();
    let r = send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(mine, Fire { power: 1 })
    });
    assert_eq!(r, Err(NetSendError::NotReplicated));
    hub.run(3);
    assert_eq!(counters::<Fire>(&hub.server.world).sent, 0);
    assert_eq!(counters::<Fire>(&hub.clients[a].world).sent, 0);
    assert_eq!(counters::<HitConfirmed>(&hub.server.world).sent, 0);
    assert_eq!(counters::<Explosion>(&hub.server.world).sent, 0);
}

/// § 6.5 queue bounds: at most 1,024 broadcasts per server tick; the rest are dropped and counted
/// `dropped_overflow`.
#[test]
fn broadcasts_past_1024_in_one_tick_are_dropped_and_counted() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 13, setup);
    let a = hub.add_client();
    hub.run(3);
    send(&mut hub.server.world, |n| {
        for i in 0..1025u32 {
            n.broadcast_session(Explosion {
                at: Vec3::new(i as f32, 0.0, 0.0),
            })
            .expect("queued");
        }
    });
    hub.run(6);
    let sv = counters::<Explosion>(&hub.server.world);
    assert_eq!(sv.sent, 1025);
    assert_eq!(sv.dropped_overflow, 1, "the 1,025th broadcast of the tick");
    assert_eq!(counters::<Explosion>(&hub.clients[a].world).delivered, 1024);
    assert!(
        hub.clients[a].connected,
        "a dropped broadcast is not a disconnect"
    );
}

/// A to-owner message for the 1 MiB reliable-queue bound.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Bulk {
    #[replicate(max_len = 200000)]
    bytes: Vec<u8>,
}
impl NetMessage for Bulk {
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for Bulk {}

fn setup_bulk(world: &mut World) {
    setup(world);
    register_net_message::<Bulk>(world);
}

/// § 6.5 queue bounds: the reliable send queue of a connection also holds at most 1 MiB; past it the
/// peer is disconnected with `ReliableOverflow` (well under the 4,096-message bound here).
#[test]
fn a_reliable_queue_over_one_mebibyte_disconnects_the_peer() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 14, setup_bulk);
    let a = hub.add_client();
    let b = hub.add_client();
    let pa = hub.clients[a].peer;
    let pawn_a = spawn_pawn(&mut hub.server.world, Some(pa));
    hub.run(3);
    send(&mut hub.server.world, move |n| {
        // 18 x 60 KiB = 1,080 KiB queued in one tick: over 1 MiB with 18 messages.
        for _ in 0..18 {
            n.send_to_owner(
                pawn_a,
                Bulk {
                    bytes: vec![3; 60 * 1024],
                },
            )
            .expect("queued");
        }
    });
    hub.run(2);
    assert!(!hub.clients[a].connected);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ReliableOverflow)
    );
    assert!(hub.clients[b].connected, "only the overflowing connection goes");
}

/// § 6.5 item 3: a message that arrives after newer state of the tick it was sent in still runs,
/// with `late = true`; an on-time message is not late.
#[test]
fn a_message_behind_newer_state_runs_late() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 15, setup);
    let a = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    send(&mut hub.server.world, move |n| {
        n.broadcast(pawn, HpCheck { expect: 1 })
    })
    .expect("sent");
    hub.run(3);
    let c = counters::<HpCheck>(&hub.clients[a].world);
    assert_eq!((c.delivered, c.late), (1, 0), "on time: delivered, not late");
    // Hold the unreliable message frames (tag 2) for 4 ticks while state keeps arriving.
    let due = hub.step_count() + 4;
    hub.down_filter = Some(Box::new(move |d, step| {
        if d.bytes.first() == Some(&2) {
            FrameFate::Delay(due.saturating_sub(step))
        } else {
            FrameFate::Deliver
        }
    }));
    send(&mut hub.server.world, move |n| {
        n.broadcast(pawn, HpCheck { expect: 2 })
    })
    .expect("sent");
    // Newer state of the target every tick while the message is held.
    for hp in 0..10 {
        hub.server.world.get_mut::<Health>(pawn).expect("health").hp = 200 + hp;
        hub.step();
    }
    let c = counters::<HpCheck>(&hub.clients[a].world);
    assert_eq!(c.delivered, 2, "the late message still runs");
    assert_eq!(c.late, 1, "and is marked late");
}

/// § 6.5 item 3 (ADR-5647): an `ALIGN_TO_VIEW` hold lasts at most `max_delay_ms` (spec 74,
/// default 250 ms = 15 ticks at 60 Hz). A view stuck behind the message's tick releases it then.
#[test]
fn align_to_view_holds_at_most_the_max_delay() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 31, setup_extra);
    let a = hub.add_client();
    let pawn = spawn_pawn(&mut hub.server.world, None);
    hub.run(3);
    let hz = regia_net::runtime::tick_hz(&hub.clients[a].world);
    let max_ms = hub.clients[a]
        .world
        .resource::<regia_net::settings::NetSettings>()
        .interpolation
        .max_delay_ms;
    let hold = (max_ms / 1000.0 * hz).ceil() as usize;
    assert!(hold > 5, "a hold of {hold} ticks");
    // The view never moves.
    hub.clients[a]
        .world
        .insert_resource(regia_net::runtime::NetInterpTick(NetTick(0)));
    send(&mut hub.server.world, move |n| {
        n.broadcast(pawn, Aligned { n: 1 })
    })
    .expect("sent");
    hub.run(3);
    assert_eq!(
        counters::<Aligned>(&hub.clients[a].world).delivered,
        0,
        "held for the view"
    );
    hub.run(hold + 3);
    assert_eq!(
        counters::<Aligned>(&hub.clients[a].world).delivered,
        1,
        "released after max_delay_ms even though the view is stuck"
    );
}
