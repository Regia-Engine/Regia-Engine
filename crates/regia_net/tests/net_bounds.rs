// SPDX-License-Identifier: MPL-2.0
//! Hostile-input bounds of the runtime (ADR-1892, the net audits of 2026-10-29): frames forged by a
//! joined client or a bad server are held to constant limits, at fixed steps with no sockets.

use regia_app::FixedTime;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bits::BitWriter;
use regia_net::flood::FloodState;
use regia_net::handshake::DisconnectReason;
use regia_net::message::{Channel, register_net_message};
use regia_net::replication::{ClientReplication, MAX_RESYNC_PER_TICK, register_replicated};
use regia_net::runtime::{
    Datagram, INBOUND_QUEUE_TICKS, MAX_PENDING_MESSAGES, MAX_RESYNC_IDS_PER_FRAME, MAX_TICK_JUMP,
    NetEntities, NetNode, tick_hz,
};
use regia_net::settings::MessageSettings;
use regia_net::testing::LoopbackHub;
use regia_net::*;

const TAG_TICK: u64 = 1;
const TAG_UMSG: u64 = 2;
const TAG_CMSG: u64 = 3;

#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Health {
    hp: u32,
}

/// An always-admitted to-server message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Beat;
impl NetMessage for Beat {
    const DIRECTION: Direction = Direction::ToServer;
    const ALWAYS_ADMIT: bool = true;
}
impl ToServerMessage for Beat {}

/// A large to-server message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Heavy {
    #[replicate(max_len = 65536)]
    bytes: Vec<u8>,
}
impl NetMessage for Heavy {
    const DIRECTION: Direction = Direction::ToServer;
    const ALWAYS_ADMIT: bool = true;
}
impl ToServerMessage for Heavy {}

/// A plain to-server message.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Fire;
impl NetMessage for Fire {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for Fire {}

/// A broadcast with an unreliable channel.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct Boom;
impl NetMessage for Boom {
    const DIRECTION: Direction = Direction::Broadcast;
    const CHANNEL: Channel = Channel::Unreliable;
}
impl BroadcastMessage for Boom {}

fn setup(world: &mut World) {
    register_net_message::<Beat>(world);
    register_net_message::<Heavy>(world);
    register_net_message::<Fire>(world);
    register_net_message::<Boom>(world);
    register_replicated::<Health>(world, ReplicateOptions::default());
}

fn hub() -> (LoopbackHub, usize, PeerId) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    hub.run(2);
    (hub, a, pa)
}

fn type_index<M: 'static>(world: &World) -> u16 {
    world
        .resource::<regia_net::message::NetMessageRegistry>()
        .index_of::<M>()
        .expect("registered")
}

fn counters<M: 'static>(world: &World) -> regia_net::message::MessageCounters {
    let reg = world.resource::<regia_net::message::NetMessageRegistry>();
    reg.counters[usize::from(reg.index_of::<M>().expect("registered"))]
}

/// A `CMSG` frame of `n` empty messages of one type.
fn cmsg(type_index: u16, n: usize, resync: &[u32]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(TAG_CMSG, 8);
    w.write_varint(1);
    w.write_varint(n as u64);
    for _ in 0..n {
        w.write_varint(u64::from(type_index));
        w.write_bool(false);
        w.write_varint(0);
    }
    w.write_varint(resync.len() as u64);
    for id in resync {
        w.write_varint(u64::from(*id));
    }
    w.finish()
}

fn inject(hub: &mut LoopbackHub, peer: PeerId, bytes: Vec<u8>) {
    hub.server.world.resource_mut::<NetNode>().inbox.push(Datagram {
        peer,
        channel: Channel::ReliableOrdered,
        bytes,
    });
}

fn queue_cap() -> usize {
    (MessageSettings::default().peer_cap_per_tick * INBOUND_QUEUE_TICKS) as usize
}

/// Audit H4 (muse, grok): a frame larger than the queue is shed undecoded, charged as a breach, and
/// nothing of it reaches a handler.
#[test]
fn a_frame_over_the_inbound_queue_is_shed_before_it_is_stored() {
    let (mut hub, _, pa) = hub();
    let fire = type_index::<Fire>(&hub.server.world);
    let n = queue_cap() + 1;
    inject(&mut hub, pa, cmsg(fire, n, &[]));
    hub.step();
    let node = hub.server.world.resource::<NetNode>();
    assert_eq!(node.peer_inbound_shed(pa), Some(n as u64));
    assert_eq!(node.peer_flood(pa).expect("peer").0, FloodState::Slowed);
    assert_eq!(counters::<Fire>(&hub.server.world).received, 0, "never decoded");
    assert!(node.has_peer(pa), "one burst is not a kick");
}

/// A frame that fits is still stored whole, and the flood guard deals with it as before.
#[test]
fn a_frame_that_fits_reaches_the_flood_guard() {
    let (mut hub, _, pa) = hub();
    let fire = type_index::<Fire>(&hub.server.world);
    let n = queue_cap();
    inject(&mut hub, pa, cmsg(fire, n, &[]));
    hub.step();
    assert_eq!(counters::<Fire>(&hub.server.world).received, n as u64);
    assert!(counters::<Fire>(&hub.server.world).rate_limited > 0);
    let node = hub.server.world.resource::<NetNode>();
    assert_eq!(node.peer_inbound_shed(pa), Some(0));
}

/// Audit M6 (muse) / M10 (grok): `ALWAYS_ADMIT` stays unblocked by the flood guard (spec 70 § 6.6)
/// but cannot pass the queue bound.
#[test]
fn always_admit_is_held_to_the_inbound_queue() {
    let (mut hub, _, pa) = hub();
    let beat = type_index::<Beat>(&hub.server.world);
    for _ in 0..10 {
        inject(&mut hub, pa, cmsg(beat, 1000, &[]));
    }
    hub.step();
    let delivered = counters::<Beat>(&hub.server.world).delivered as usize;
    assert_eq!(
        delivered,
        4000,
        "whole frames that fit a queue of {}",
        queue_cap()
    );
    let node = hub.server.world.resource::<NetNode>();
    assert_eq!(node.peer_inbound_shed(pa), Some(6000));
}

/// Audit H1 (muse2): sustained overflow ends in a `Flood` disconnect in two seconds, not ten.
#[test]
fn sustained_overflow_disconnects_the_peer() {
    let (mut hub, a, pa) = hub();
    let fire = type_index::<Fire>(&hub.server.world);
    let n = queue_cap() + 1;
    let mut steps = 0;
    while hub.clients[a].connected {
        inject(&mut hub, pa, cmsg(fire, n, &[]));
        hub.step();
        steps += 1;
        assert!(steps <= 130, "still connected after {steps} ticks");
    }
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::Flood)
    );
    assert!(steps >= 120, "two seconds of overflow, got {steps} ticks");
}

/// Audit M5 (muse): a resync list past the frame limit is malformed, and a long list from a correct
/// client goes out over several frames.
#[test]
fn resync_lists_are_bounded() {
    let (mut hub, _, pa) = hub();
    let fire = type_index::<Fire>(&hub.server.world);
    let ok: Vec<u32> = (0..MAX_RESYNC_IDS_PER_FRAME as u32).collect();
    inject(&mut hub, pa, cmsg(fire, 0, &ok));
    hub.step();
    assert!(hub.server.world.resource::<NetNode>().has_peer(pa));
    let long: Vec<u32> = (0..=MAX_RESYNC_IDS_PER_FRAME as u32).collect();
    inject(&mut hub, pa, cmsg(fire, 0, &long));
    hub.step();
    assert!(!hub.server.world.resource::<NetNode>().has_peer(pa));

    let (mut hub, b, pb) = self::hub();
    let world = &mut hub.clients[b].world;
    world
        .resource_mut::<ClientReplication>()
        .resync
        .extend((0..(MAX_RESYNC_IDS_PER_FRAME as u32 * 2 + 1)).map(NetId));
    hub.step();
    let left = hub.clients[b].world.resource::<ClientReplication>().resync.len();
    let total = MAX_RESYNC_IDS_PER_FRAME * 2 + 1;
    assert_eq!(
        left,
        total - MAX_RESYNC_PER_TICK,
        "one frame took what the server takes"
    );
    hub.run(total / MAX_RESYNC_PER_TICK + 1);
    assert!(
        hub.clients[b]
            .world
            .resource::<ClientReplication>()
            .resync
            .is_empty()
    );
    assert!(
        hub.server.world.resource::<NetNode>().has_peer(pb),
        "a correct client stays"
    );
}

/// A `TICK` frame with an empty replication section and no messages.
fn tick_frame(tick: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(TAG_TICK, 8);
    w.write_varint(u64::from(tick));
    for _ in 0..5 {
        w.write_varint(0);
    }
    w.write_varint(0);
    w.finish()
}

fn inject_client(hub: &mut LoopbackHub, client: usize, bytes: Vec<u8>) {
    hub.clients[client]
        .world
        .resource_mut::<NetNode>()
        .inbox
        .push(Datagram {
            peer: PeerId::SERVER,
            channel: Channel::ReliableOrdered,
            bytes,
        });
}

/// Audit M1 (muse2), M5 (grok): one forged tick cannot strand a client on a tick no later frame can
/// reach. It ends the connection instead.
#[test]
fn a_far_future_tick_closes_the_client_instead_of_bricking_it() {
    for tick in [u32::MAX, 1_000_000] {
        let (mut hub, a, _) = hub();
        let before = hub.clients[a].world.resource::<NetNode>().tick;
        inject_client(&mut hub, a, tick_frame(tick));
        regia_net::runtime::net_receive(&mut hub.clients[a].world);
        let node = hub.clients[a].world.resource::<NetNode>();
        assert_eq!(node.closed, Some(DisconnectReason::ProtocolError), "tick {tick}");
        assert_eq!(node.tick, before, "not applied");
        assert_eq!(node.pending_counts().0, 0, "not stored");
    }
    // A step inside the window is fine.
    let (mut hub, a, _) = hub();
    let at = hub.clients[a].world.resource::<NetNode>().tick.0;
    inject_client(&mut hub, a, tick_frame(at + MAX_TICK_JUMP));
    regia_net::runtime::net_receive(&mut hub.clients[a].world);
    assert_eq!(hub.clients[a].world.resource::<NetNode>().closed, None);
}

/// Even on the very first frame, when no window exists yet.
#[test]
fn the_last_tick_number_is_refused_on_a_first_frame() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let a = hub.add_client();
    inject_client(&mut hub, a, tick_frame(u32::MAX));
    regia_net::runtime::net_receive(&mut hub.clients[a].world);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError)
    );
}

/// A `UMSG` frame of `n` empty messages stamped with `tick`, sequence numbers from `first_seq`.
fn umsg(type_index: u16, tick: u32, n: usize, first_seq: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(TAG_UMSG, 8);
    w.write_varint(u64::from(tick));
    w.write_varint(n as u64);
    for i in 0..n {
        w.write_varint(u64::from(type_index));
        w.write_bool(false);
        w.write_varint(u64::from(first_seq + i as u32));
        w.write_varint(0);
    }
    w.finish()
}

/// Audit M5 (grok): messages stamped with a tick that never comes neither pile up nor stay.
#[test]
fn messages_for_a_tick_that_never_comes_are_bounded_and_age_out() {
    let (mut hub, a, _) = hub();
    let boom = type_index::<Boom>(&hub.clients[a].world);
    for f in 0..3 {
        let first = 1_000_000 + f * 5000;
        inject_client(&mut hub, a, umsg(boom, u32::MAX, 5000, first as u32));
    }
    hub.step();
    let node = hub.clients[a].world.resource::<NetNode>();
    assert_eq!(node.pending_counts().1, MAX_PENDING_MESSAGES);
    assert_eq!(
        counters::<Boom>(&hub.clients[a].world).dropped_overflow,
        15_000 - MAX_PENDING_MESSAGES as u64
    );
    hub.run(300);
    assert_eq!(hub.clients[a].world.resource::<NetNode>().pending_counts().1, 0);
}

/// Audit M5 (muse2) / L7 (muse): a to-server type from the server never reaches a client handler.
#[test]
fn a_client_refuses_to_server_types() {
    let (mut hub, a, _) = hub();
    let fire = type_index::<Fire>(&hub.clients[a].world);
    let boom = type_index::<Boom>(&hub.clients[a].world);
    let tick = hub.clients[a].world.resource::<NetNode>().tick.0;
    inject_client(&mut hub, a, umsg(fire, tick, 1, 2_000_000));
    inject_client(&mut hub, a, umsg(boom, tick, 1, 2_000_001));
    hub.step();
    let world = &hub.clients[a].world;
    assert_eq!(counters::<Fire>(world).delivered, 0);
    assert_eq!(counters::<Fire>(world).rejected, 1);
    assert_eq!(counters::<Boom>(world).delivered, 1, "other types flow");
}

/// A tick frame that starts with `n` valid bare spawn records and is cut off after them.
fn truncated_spawns(tick: u32, n: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(TAG_TICK, 8);
    w.write_varint(u64::from(tick));
    w.write_varint(0); // ownership
    w.write_varint(u64::from(n) + 1); // one spawn more than the frame holds
    for i in 0..n {
        w.write_varint(u64::from(5000 + i)); // id
        w.write_bits(0, 2); // bare
        w.write_bool(false); // no parent
        w.write_bool(false); // not owned
        w.write_bool(false); // not predicted
        w.write_bits(0, 2); // no transform
        w.write_varint(0); // no entries
    }
    for _ in 0..4 {
        w.write_bits(0xFF, 8); // not a record: the varint id never ends
    }
    w.finish()
}

/// Audit M1 (muse): a frame that is corrupt anywhere leaves the world as it was.
#[test]
fn a_corrupt_tick_frame_spawns_nothing() {
    let (mut hub, a, _) = hub();
    let tick = hub.clients[a].world.resource::<NetNode>().tick.0;
    let before = hub.clients[a].world.resource::<NetEntities>().len();
    let entities_before = hub.clients[a].world.entities().count_spawned();
    inject_client(&mut hub, a, truncated_spawns(tick + 5, 200));
    hub.step();
    let world = &hub.clients[a].world;
    assert_eq!(
        world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError)
    );
    assert_eq!(world.resource::<NetEntities>().len(), before);
    assert_eq!(world.entities().count_spawned(), entities_before);
}

/// A declared count the frame cannot hold fails before the first record.
#[test]
fn a_lying_record_count_is_refused() {
    let (mut hub, a, _) = hub();
    let tick = hub.clients[a].world.resource::<NetNode>().tick.0;
    let mut w = BitWriter::new();
    w.write_bits(TAG_TICK, 8);
    w.write_varint(u64::from(tick + 5));
    w.write_varint(0);
    w.write_varint(1 << 19); // half a million spawns in a dozen bytes
    let bytes = w.finish();
    assert!(bytes.len() < 16);
    inject_client(&mut hub, a, bytes);
    hub.step();
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError)
    );
}

/// A section entry with a kind no sender writes fails the walk, before earlier records apply.
#[test]
fn an_impossible_entry_kind_is_refused_before_anything_applies() {
    let (mut hub, a, _) = hub();
    let tick = hub.clients[a].world.resource::<NetNode>().tick.0;
    let before = hub.clients[a].world.entities().count_spawned();
    let mut w = BitWriter::new();
    w.write_bits(TAG_TICK, 8);
    w.write_varint(u64::from(tick + 5));
    w.write_varint(0); // ownership
    w.write_varint(2); // two spawns, the second with a bad entry
    for (id, entries) in [(7000u64, 0u64), (7001, 1)] {
        w.write_varint(id);
        w.write_bits(0, 2);
        w.write_bool(false);
        w.write_bool(false);
        w.write_bool(false);
        w.write_bits(0, 2);
        w.write_varint(entries);
    }
    w.write_varint(0); // component 0
    w.write_bits(3, 2); // no such kind
    w.write_varint(0);
    for _ in 0..4 {
        w.write_varint(0);
    }
    inject_client(&mut hub, a, w.finish());
    hub.step();
    let world = &hub.clients[a].world;
    assert_eq!(
        world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError)
    );
    assert_eq!(world.entities().count_spawned(), before);
}

/// Review: spawns and despawns are not budgeted without the interest pipeline, so a large despawn
/// is a real frame: a correct server must not lose every client to the record cap.
#[test]
fn a_large_despawn_frame_is_accepted() {
    let (mut hub, a, _) = hub();
    let tick = hub.clients[a].world.resource::<NetNode>().tick.0;
    let n = 300_000u64;
    let mut w = BitWriter::new();
    w.write_bits(TAG_TICK, 8);
    w.write_varint(u64::from(tick + 5));
    w.write_varint(0);
    w.write_varint(0);
    w.write_varint(0);
    w.write_varint(n);
    for i in 0..n {
        w.write_varint(1_000_000 + i);
        w.write_bool(false);
    }
    w.write_varint(0);
    w.write_varint(0);
    inject_client(&mut hub, a, w.finish());
    hub.step();
    assert_eq!(hub.clients[a].world.resource::<NetNode>().closed, None);
}

/// Review: a tick heavier than the client frame limit goes out as several frames, in order.
#[test]
fn a_heavy_client_tick_is_split_over_frames() {
    let (mut hub, a, pa) = hub();
    let world = &mut hub.clients[a].world;
    let msgs = 20usize;
    world
        .run_system_once(move |mut net: NetSender| {
            for _ in 0..msgs {
                net.send_session_to_server(Heavy {
                    bytes: vec![0; 60_000],
                })
                .expect("queued");
            }
        })
        .expect("system runs");
    regia_net::runtime::net_send(world);
    let frames: Vec<usize> = world
        .resource::<NetNode>()
        .outbox
        .iter()
        .filter(|d| d.channel == Channel::ReliableOrdered)
        .map(|d| d.bytes.len())
        .collect();
    assert!(frames.len() > 1, "{frames:?}");
    assert!(
        frames
            .iter()
            .all(|b| *b <= regia_net::bridge::MAX_CLIENT_FRAME_BYTES),
        "{frames:?}"
    );
    hub.run(4);
    let got = counters::<Heavy>(&hub.server.world).received;
    assert_eq!(got, msgs as u64);
    assert!(hub.server.world.resource::<NetNode>().has_peer(pa));
}

/// Audit L5 (muse) / L4 (muse2): the tick counter does not wrap and does not silently stick: the
/// session ends.
#[test]
fn the_last_tick_ends_the_session() {
    let (mut hub, a, pa) = hub();
    hub.server.world.resource_mut::<NetNode>().tick = NetTick(u32::MAX - 1);
    // Runs the server alone: the client would refuse the forged jump before the disconnect lands.
    let world = &mut hub.server.world;
    regia_net::runtime::net_receive(world);
    regia_net::runtime::net_apply(world);
    regia_net::runtime::net_send(world);
    let node = hub.server.world.resource::<NetNode>();
    assert_eq!(node.tick, NetTick(u32::MAX));
    assert!(
        node.disconnects
            .iter()
            .any(|d| d.peer == pa && d.reason == DisconnectReason::ServerShutdown),
        "the session ended: {:?}",
        node.disconnects
    );
    let _ = a;
}

/// Audit L3 (zai): the runtime reads the real fixed rate, for the flood guard and the send budget.
#[test]
fn the_tick_rate_comes_from_the_fixed_clock() {
    let mut world = World::new();
    assert_eq!(tick_hz(&world), 60.0, "a world with no app");
    let mut fixed = FixedTime::default();
    fixed.hz = 30.0;
    world.insert_resource(fixed);
    assert!((tick_hz(&world) - 30.0).abs() < 1e-6);
    regia_net::runtime::ensure_resources(&mut world);
    world.insert_resource(NetMode::DedicatedServer);
    regia_net::runtime::server_add_peer(&mut world, PeerId(7));
    let node = world.resource::<NetNode>();
    assert_eq!(node.peer_ticks_per_second(PeerId(7)), Some(30));
}

/// Random bytes behind every frame tag, on both sides: a clean refusal, never a panic. Seeded, so a
/// failure repeats.
#[test]
fn random_frames_never_panic() {
    let mut rng = regia_net::testing::SplitMix64::new(0x5EED);
    for round in 0..600u32 {
        let (mut hub, a, pa) = hub();
        for tag in 0..=5u64 {
            let mut w = BitWriter::new();
            w.write_bits(tag, 8);
            w.write_varint(u64::from(round % 8));
            for _ in 0..(round % 48) {
                w.write_bits(rng.next_u64(), 64);
            }
            let bytes = w.finish();
            inject(&mut hub, pa, bytes.clone());
            inject_client(&mut hub, a, bytes);
        }
        hub.step();
    }
}
