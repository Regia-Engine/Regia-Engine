// SPDX-License-Identifier: MPL-2.0
//! Spec 72 § 4 and § 13 on the acknowledged state channel (protocol 2, ADR-1950): baselines,
//! reorder, the version ring, spawn healing, negotiation and fallback, packet size, the send
//! cadence, and convergence under loss. Loopback hub, fixed steps, seeded conditions; frames are
//! named exactly through the hub's frame filters.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::CodecContext;
use regia_net::message::Channel;
use regia_net::replication::{
    ClientReplication, ClientStateCounters, ConnWire, IN_FLIGHT_TIMEOUT_TICKS, NetEvents, NetVecEventKind,
    STATE_PACKET_RESERVE, ServerReplication, netvec_events, register_netvec, register_replicated,
};
use regia_net::runtime::{Datagram, MAX_PENDING_STATE_FRAMES, NetEntities, NetNode, net_receive};
use regia_net::testing::{FrameFate, LinkConditions, LoopbackHub, SplitMix64, set_serial_gather};
use regia_net::*;
use regia_scene::transform::Transform;

const TAG_TICK: u8 = 1;
const TAG_ACK: u8 = 4;
const TAG_STATE: u8 = 7;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// A component whose codec clamps: `temp` above 100 trips the quantizer's clamp counter on every
/// encode (the fixture of `parallel_assembly_equals_serial`'s counter check).
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hot {
    #[replicate(quantize(min = 0.0, max = 100.0, bits = 8))]
    temp: f32,
}

/// A component holding an entity reference: every encode of a reference to a non-replicated
/// entity trips the unreplicated-ref counter (the other half of that counter check).
#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Linked {
    target: Entity,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Stats {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    e: u32,
    f: u32,
}

/// Raw bytes (records larger than a packet).
#[derive(Component, Clone, Debug, PartialEq, Default)]
struct Blob(Vec<u8>);

impl ReplicateCodec for Blob {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.0.len() as u64);
        for b in &self.0 {
            w.write_bits(u64::from(*b), 8);
        }
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(1 << 20)?;
        let mut v = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            v.push(r.read_bits(8)? as u8);
        }
        Ok(Self(v))
    }
}

#[derive(Clone, Debug, PartialEq, Replicate)]
struct Item {
    count: u32,
}

fn registered(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
    register_replicated::<Stats>(world, ReplicateOptions::default());
    register_replicated::<Blob>(world, ReplicateOptions::default());
    register_netvec::<Item>(world, Audience::All);
}

fn state_setup(world: &mut World) {
    registered(world);
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
}

/// With the budget pipeline on (a wide world profile, so everything is relevant).
fn budget_setup(world: &mut World) {
    state_setup(world);
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let everywhere = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            ..NetProfile::default()
        };
        world
            .resource_mut::<NetProfiles>()
            .insert("everywhere", everywhere)
            .expect("profile");
        world.resource_mut::<NetProfiles>().set_default("everywhere");
    }
}

fn hub(setup: fn(&mut World), seed: u64) -> LoopbackHub {
    LoopbackHub::new(NetMode::DedicatedServer, seed, setup)
}

fn peer_of(hub: &LoopbackHub, client: usize) -> PeerId {
    hub.clients[client].peer
}

fn wire(hub: &LoopbackHub, client: usize) -> ConnWire {
    hub.server
        .world
        .resource::<ServerReplication>()
        .state_link(peer_of(hub, client))
        .expect("connection")
        .wire
}

fn net_id(hub: &LoopbackHub, e: Entity) -> NetId {
    *hub.server.world.get::<NetId>(e).expect("net id")
}

fn copy_of(hub: &LoopbackHub, client: usize, id: NetId) -> Option<Entity> {
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn client_state(hub: &LoopbackHub, client: usize) -> ClientStateCounters {
    hub.clients[client]
        .world
        .resource::<ClientReplication>()
        .state
        .counters
}

fn tag(d: &Datagram) -> u8 {
    d.bytes.first().copied().unwrap_or(0)
}

fn frames(hub: &LoopbackHub, client: usize, t: u8) -> Vec<(u64, Datagram)> {
    hub.client_logs[client]
        .iter()
        .filter(|(_, d)| tag(d) == t)
        .cloned()
        .collect()
}

/// The frame's tick.
fn frame_tick(d: &Datagram) -> u32 {
    let mut r = BitReader::new(&d.bytes);
    r.read_bits(8).expect("tag");
    r.read_varint().expect("tick") as u32
}

/// Spec 72 § 4, ADR-1950: a client that speaks protocol 2 gets the state channel: spawns and
/// deltas come on the unreliable channel, and tick frames stop when they have nothing to say.
#[test]
fn a_protocol_2_client_gets_the_state_channel() {
    let mut hub = hub(state_setup, 1);
    hub.negotiate = true;
    let a = hub.add_client();
    assert_eq!(wire(&hub, a), ConnWire::Undecided);
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 7 })).id();
    hub.run(6);
    assert_eq!(wire(&hub, a), ConnWire::Acked);
    let id = net_id(&hub, e);
    let copy = copy_of(&hub, a, id).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 7 }));
    let states = frames(&hub, a, TAG_STATE);
    assert!(states.iter().all(|(_, d)| d.channel == Channel::Unreliable));
    assert!(!states.is_empty());
    // Tick frames carried only the empty frames that drew the first acknowledgment.
    let ticks = frames(&hub, a, TAG_TICK).len();
    hub.run(20);
    assert_eq!(
        frames(&hub, a, TAG_TICK).len(),
        ticks,
        "nothing to say, nothing reliable"
    );
    assert!(
        frames(&hub, a, TAG_STATE).len() >= states.len() + 19,
        "one frame per send tick"
    );
    let (sent, base) = hub
        .server
        .world
        .resource::<ServerReplication>()
        .copy_ticks(peer_of(&hub, a), id)
        .expect("copy");
    assert!(
        base.is_some_and(|b| b <= sent),
        "the spawn is the first baseline once acknowledged"
    );
}

fn strip_ack_extension(d: &mut Datagram) {
    if d.bytes.first() != Some(&TAG_ACK) {
        return;
    }
    let mut r = BitReader::new(&d.bytes);
    let _ = r.read_bits(8);
    let tick = r.read_varint().expect("tick");
    let mut w = BitWriter::new();
    w.write_bits(u64::from(TAG_ACK), 8);
    w.write_varint(tick);
    d.bytes = w.finish();
}

/// The fallback: an `ACK` that ends at the tick is a protocol 1 client. Its connection keeps the
/// reliable tick stream, and a protocol 2 client beside it is not affected.
#[test]
fn a_protocol_1_client_keeps_the_reliable_stream() {
    let mut hub = hub(state_setup, 2);
    hub.negotiate = true;
    hub.up_rewrite = Some(strip_ack_extension);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(6);
    assert_eq!(wire(&hub, a), ConnWire::Stream);
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 2;
    hub.run(4);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 2 }));
    assert!(frames(&hub, a, TAG_STATE).is_empty(), "no state frame reaches it");
}

/// The fallback the other way: a server on `state_channel: Reliable` (and so any protocol 1 server)
/// ignores the extension; a protocol 2 client lives on the tick stream.
#[test]
fn a_protocol_2_client_lives_on_a_reliable_server() {
    fn setup(world: &mut World) {
        registered(world);
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Reliable;
    }
    let mut hub = hub(setup, 3);
    hub.negotiate = true;
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(4);
    assert_eq!(wire(&hub, a), ConnWire::Stream);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned on the first frames");
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 2;
    hub.run(3);
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 2 }));
    assert!(frames(&hub, a, TAG_STATE).is_empty());
}

/// § 13 test 1: one field changes, the packet carrying it is lost; the change arrives in a later
/// packet with no resend logic, and the wire carries only that field.
#[test]
fn a_lost_delta_is_carried_by_the_next_one() {
    let mut hub = hub(state_setup, 4);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Stats {
                a: 1_000_000,
                b: 2_000_000,
                c: 3_000_000,
                d: 4_000_000,
                e: 5_000_000,
                f: 6_000_000,
            },
        ))
        .id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    let lost = Arc::new(AtomicU32::new(0));
    let seen = lost.clone();
    // Lose the first state frame that carries a record.
    hub.down_filter = Some(Box::new(move |d, _| {
        if tag(d) == TAG_STATE && d.bytes.len() > 8 && seen.fetch_add(1, Ordering::Relaxed) == 0 {
            FrameFate::Drop
        } else {
            FrameFate::Deliver
        }
    }));
    let before = hub.client_logs[a].len();
    hub.server.world.get_mut::<Stats>(e).expect("stats").c = 3_000_001;
    hub.run(1);
    hub.run(8);
    assert!(lost.load(Ordering::Relaxed) >= 2, "the change went twice");
    assert_eq!(
        hub.clients[a].world.get::<Stats>(copy).expect("stats").c,
        3_000_001
    );
    let delivered = hub.client_logs[a][before..]
        .iter()
        .filter(|(_, d)| tag(d) == TAG_STATE)
        .map(|(_, d)| d.bytes.len())
        .max()
        .expect("frames");
    assert!(delivered < 24, "only the field travels, {delivered} bytes");
}

/// § 13 test 2: deltas for ticks 12 and 14 arrive in reverse order: the client ends with tick 14's
/// state; tick 12's is stored, not applied.
#[test]
fn reordered_deltas_end_on_the_newest() {
    let mut hub = hub(state_setup, 5);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    let delayed = Arc::new(AtomicU32::new(0));
    let flag = delayed.clone();
    hub.down_filter = Some(Box::new(move |d, _| {
        if tag(d) == TAG_STATE && d.bytes.len() > 8 && flag.fetch_add(1, Ordering::Relaxed) == 0 {
            FrameFate::Delay(4)
        } else {
            FrameFate::Deliver
        }
    }));
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 12;
    hub.run(2);
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 14;
    let stored = client_state(&hub, a).stored_only;
    hub.run(8);
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 14 }));
    assert_eq!(
        client_state(&hub, a).stored_only,
        stored + 1,
        "stored, not applied"
    );
}

/// § 13 test 3: a connection that acknowledges nothing for an entity for more versions than the
/// client's ring holds (`STATE_VERSION_RING` plus 6: 70 when the ring was 64, 518 since ADR-10051)
/// gets a full state, then deltas again.
#[test]
fn a_baseline_out_of_the_ring_gets_a_full_state() {
    let silent_versions = regia_net::replication::STATE_VERSION_RING + 6;
    let mut hub = hub(state_setup, 6);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 0 })).id();
    hub.run(8);
    assert_eq!(wire(&hub, a), ConnWire::Acked);
    let peer = peer_of(&hub, a);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    let silent = Arc::new(Mutex::new(true));
    let on = silent.clone();
    hub.up_filter = Some(Box::new(move |d, _| {
        if tag(d) == TAG_ACK && *on.lock().expect("flag") {
            FrameFate::Drop
        } else {
            FrameFate::Deliver
        }
    }));
    for v in 1..=silent_versions {
        hub.server.world.get_mut::<Hp>(e).expect("hp").hp = v;
        hub.step();
    }
    hub.run(4);
    let link = hub.server.world.resource::<ServerReplication>();
    assert!(
        link.state_link(peer).expect("link").counters.baselines_expired >= 1,
        "the baseline left the ring"
    );
    assert_eq!(
        hub.clients[a].world.get::<Hp>(copy),
        Some(&Hp { hp: silent_versions })
    );
    *silent.lock().expect("flag") = false;
    hub.run(40);
    let before = client_state(&hub, a).applied;
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = silent_versions + 1;
    hub.run(4);
    assert_eq!(
        hub.clients[a].world.get::<Hp>(copy),
        Some(&Hp {
            hp: silent_versions + 1
        })
    );
    assert_eq!(client_state(&hub, a).applied, before + 1, "one delta");
    assert_eq!(hub.clients[a].world.resource::<NetEvents>().spawned.len(), 0);
}

/// § 13 test 4: a lost spawn is repeated until acknowledged; the client never spawns twice.
#[test]
fn a_lost_spawn_heals_and_spawns_once() {
    let mut hub = hub(state_setup, 7);
    let a = hub.add_client();
    hub.run(4);
    let lost = Arc::new(AtomicU32::new(0));
    let seen = lost.clone();
    hub.down_filter = Some(Box::new(move |d, _| {
        if tag(d) == TAG_STATE && d.bytes.len() > 8 && seen.fetch_add(1, Ordering::Relaxed) < 3 {
            FrameFate::Drop
        } else {
            FrameFate::Deliver
        }
    }));
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 5 })).id();
    let mut spawned = 0;
    for _ in 0..80 {
        hub.step();
        spawned += hub.clients[a].world.resource::<NetEvents>().spawned.len();
    }
    assert_eq!(spawned, 1, "healed, exactly once");
    assert!(lost.load(Ordering::Relaxed) >= 4, "repeated while lost");
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 5 }));
}

/// A record stays unsent while a packet with the same version may be on its way: at 100 ms of
/// latency one change is not sent six times (ADR-1950 (4)).
#[test]
fn a_version_in_flight_is_not_sent_again() {
    let mut hub = hub(state_setup, 8);
    hub.down.delay_ticks = 3;
    hub.up.delay_ticks = 3;
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(30);
    let peer = peer_of(&hub, a);
    let records = |hub: &LoopbackHub| {
        hub.server
            .world
            .resource::<ServerReplication>()
            .state_link(peer)
            .expect("link")
            .counters
            .records_sent
    };
    let before = records(&hub);
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 2;
    hub.run(20);
    assert_eq!(
        records(&hub),
        before + 1,
        "one record, however long the ack takes"
    );
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 2 }));
}

/// Spec 72 § 7: state frames stay within `mtu_bytes`, several go per send tick while budget
/// remains, and the world still arrives whole.
#[test]
fn state_frames_respect_the_packet_size() {
    fn setup(world: &mut World) {
        budget_setup(world);
        let mut s = world.resource_mut::<NetSettings>();
        s.replication.mtu_bytes = 300;
        s.replication.client_rate_bytes = 1_000_000;
        s.replication.spawns_per_send = 1000;
    }
    let mut hub = hub(setup, 9);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..200u32)
        .map(|i| {
            hub.server
                .world
                .spawn((
                    Replicated::default(),
                    Transform {
                        translation: Vec3::new(i as f32, 0.0, 0.0),
                        ..Transform::default()
                    },
                    Hp { hp: i },
                ))
                .id()
        })
        .collect();
    hub.run(10);
    for (i, e) in ents.iter().enumerate() {
        let copy = copy_of(&hub, a, net_id(&hub, *e)).expect("spawned");
        assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: i as u32 }));
    }
    let states = frames(&hub, a, TAG_STATE);
    assert!(
        states
            .iter()
            .all(|(_, d)| d.bytes.len() <= 300 - STATE_PACKET_RESERVE),
        "largest {}",
        states.iter().map(|(_, d)| d.bytes.len()).max().unwrap_or(0)
    );
    let mut per_tick: BTreeMap<u32, u32> = BTreeMap::new();
    for (_, d) in &states {
        *per_tick.entry(frame_tick(d)).or_default() += 1;
    }
    assert!(
        per_tick.values().any(|n| *n > 2),
        "several frames in one send tick"
    );
}

/// Spec 72 § 7: an entity's state larger than one packet goes as one reliable record, at most one
/// in flight per connection.
#[test]
fn a_record_larger_than_a_packet_goes_reliably_one_at_a_time() {
    fn setup(world: &mut World) {
        budget_setup(world);
        let mut s = world.resource_mut::<NetSettings>();
        s.replication.client_rate_bytes = 10_000_000;
    }
    let mut hub = hub(setup, 10);
    hub.down.delay_ticks = 2;
    hub.up.delay_ticks = 2;
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..3u8)
        .map(|i| {
            hub.server
                .world
                .spawn((Replicated::default(), Blob(vec![i; 5000])))
                .id()
        })
        .collect();
    hub.run(60);
    for (i, e) in ents.iter().enumerate() {
        let copy = copy_of(&hub, a, net_id(&hub, *e)).expect("spawned");
        assert_eq!(
            hub.clients[a].world.get::<Blob>(copy),
            Some(&Blob(vec![i as u8; 5000]))
        );
    }
    let big: Vec<(u64, Datagram)> = frames(&hub, a, TAG_STATE)
        .into_iter()
        .filter(|(_, d)| d.channel.is_reliable())
        .collect();
    assert_eq!(big.len(), 3, "each once");
    assert!(
        big.windows(2).all(|w| w[1].0 - w[0].0 >= 4),
        "the next waits for the acknowledgment of the last: {:?}",
        big.iter().map(|(s, _)| *s).collect::<Vec<_>>()
    );
}

/// Spec 72 § 3: with `send_every_ticks: 2` state goes on every other tick, ids and messages do not
/// wait for a send tick to be usable, and the budget is per send tick.
#[test]
fn the_send_cadence_holds_frames_to_send_ticks() {
    fn setup(world: &mut World) {
        state_setup(world);
        world.resource_mut::<NetSettings>().replication.send_every_ticks = 2;
    }
    let mut hub = hub(setup, 11);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(11);
    let ticks: Vec<u32> = hub.client_logs[a]
        .iter()
        .filter(|(_, d)| matches!(tag(d), TAG_STATE | TAG_TICK))
        .map(|(_, d)| frame_tick(d))
        .collect();
    assert!(ticks.iter().all(|t| t.is_multiple_of(2)), "{ticks:?}");
    assert!(ticks.len() >= 4);
    // A new entity has its id on the tick after its spawn, send tick or not.
    let n = hub.server_tick().0;
    assert_eq!(n % 2, 1, "this test spawns before an off tick");
    let f = hub.server.world.spawn((Replicated::default(), Hp { hp: 2 })).id();
    hub.step();
    assert!(
        hub.server.world.get::<NetId>(f).is_some(),
        "assigned on the off tick"
    );
    hub.run(8);
    assert!(copy_of(&hub, a, net_id(&hub, f)).is_some());
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 1 }));
}

/// Bytes per second stay within the rate on the state channel (one packet of slack), and a send
/// tick's budget is `rate / send ticks per second`.
#[test]
fn budgets_count_send_ticks() {
    fn setup(world: &mut World) {
        budget_setup(world);
        let mut s = world.resource_mut::<NetSettings>();
        s.replication.send_every_ticks = 2;
        s.replication.client_rate_bytes = 6_000;
    }
    let mut hub = hub(setup, 12);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..100u32)
        .map(|i| {
            hub.server
                .world
                .spawn((
                    Replicated {
                        transform: TransformReplication::None,
                        ..Replicated::default()
                    },
                    Hp { hp: i },
                ))
                .id()
        })
        .collect();
    hub.run(240);
    let start = hub.client_logs[a].len();
    for t in 0..600u32 {
        for e in &ents {
            hub.server.world.get_mut::<Hp>(*e).expect("hp").hp = 10_000 + t;
        }
        hub.step();
    }
    let bytes: usize = hub.client_logs[a][start..]
        .iter()
        .map(|(_, d)| d.bytes.len())
        .sum();
    // 10 s at 6 KB/s, plus frame headers at 30 Hz.
    assert!((45_000..=64_000).contains(&bytes), "{bytes} bytes in 10 s");
    let budget = hub.server.world.resource::<NetInterest>().peers[&peer_of(&hub, a)]
        .send
        .counters
        .budget;
    assert!((200..=400).contains(&budget), "6000 / 30 per send tick, {budget}");
}

/// A budget that fits a few records orders about that many candidates, however many wait: a tick
/// must not sort the whole ranking to find out that nothing below the head can go.
#[test]
fn the_ranking_orders_what_the_budget_can_reach() {
    fn setup(world: &mut World) {
        budget_setup(world);
        world.resource_mut::<NetSettings>().replication.client_rate_bytes = 6_000;
    }
    let mut hub = hub(setup, 13);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..1000u32)
        .map(|i| {
            hub.server
                .world
                .spawn((
                    Replicated {
                        transform: TransformReplication::None,
                        ..Replicated::default()
                    },
                    Hp { hp: i },
                ))
                .id()
        })
        .collect();
    // Every spawn is out and acknowledged first: only updates compete.
    hub.run(600);
    let peer = peer_of(&hub, a);
    assert_eq!(
        hub.server.world.resource::<NetInterest>().peers[&peer]
            .send
            .counters
            .spawns_pending,
        0
    );
    for t in 0..8u32 {
        for e in &ents {
            hub.server.world.get_mut::<Hp>(*e).expect("hp").hp = 20_000 + t;
        }
        hub.step();
        hub.step();
    }
    let ordered = hub
        .server
        .world
        .resource::<ServerReplication>()
        .timing
        .ordered_candidates;
    let counters = hub.server.world.resource::<NetInterest>().peers[&peer]
        .send
        .counters;
    assert!(
        counters.deferred > 500,
        "the budget holds most back: {counters:?}"
    );
    // 6000 B/s at 30 Hz is 200 bytes a tick, 25 of the shortest records, plus a margin.
    assert!(ordered <= 100, "{ordered} candidates ordered for one connection");
    assert!(ordered > 0);
}

/// What the client should end with: every server value on the copy.
fn assert_converged(hub: &LoopbackHub, client: usize, ents: &[Entity]) {
    let world = &hub.clients[client].world;
    for e in ents {
        let id = net_id(hub, *e);
        let copy = copy_of(hub, client, id).unwrap_or_else(|| panic!("copy of {id:?}"));
        assert_eq!(
            hub.server.world.get::<Hp>(*e),
            world.get::<Hp>(copy),
            "{id:?} component state"
        );
        assert_eq!(
            hub.server.world.get::<Stats>(*e),
            world.get::<Stats>(copy),
            "{id:?} stats"
        );
        let parent = hub
            .server
            .world
            .get::<ChildOf>(*e)
            .and_then(|p| hub.server.world.get::<NetId>(p.0).copied());
        // A root crosses the wire on its profile's position grid (ADR-2413 (4)); a child of a
        // replicated parent is sent as is.
        let sent = hub.server.world.get::<Transform>(*e).map(|t| {
            if parent.is_some() {
                t.translation
            } else {
                regia_net::profile::snap_root_translation(&hub.server.world, *e, t.translation)
            }
        });
        assert_eq!(
            sent,
            world.get::<Transform>(copy).map(|t| t.translation),
            "{id:?} transform"
        );
        let copy_parent = world
            .get::<ChildOf>(copy)
            .and_then(|p| world.get::<NetId>(p.0).copied());
        assert_eq!(parent, copy_parent, "{id:?} parent");
    }
}

/// Everything at once, on a bad link: values, added and removed components, transforms, parents,
/// spawns and despawns under loss, reorder and duplication. When the link clears, every client
/// holds exactly the server's world, and ids it should not hold are gone.
fn soak(seed: u64, setup: fn(&mut World)) {
    let mut hub = hub(setup, seed);
    hub.down = LinkConditions {
        loss: 0.3,
        delay_ticks: 2,
        duplicate: 0.1,
        reorder: 0.3,
        resend_ticks: 4,
        reliable_extra_ticks: 0,
    };
    hub.up = LinkConditions {
        loss: 0.2,
        delay_ticks: 2,
        ..hub.down
    };
    let a = hub.add_client();
    let b = hub.add_client();
    let mut rng = SplitMix64::new(seed ^ 0xfeed);
    let mut ents: Vec<Entity> = Vec::new();
    let mut gone: Vec<NetId> = Vec::new();
    for t in 0..900u32 {
        if ents.len() < 60 && rng.next_f64() < 0.3 {
            ents.push(
                hub.server
                    .world
                    .spawn((Replicated::default(), Hp { hp: t }, Transform::default()))
                    .id(),
            );
        }
        for _ in 0..4 {
            if ents.is_empty() {
                break;
            }
            let i = (rng.next_u64() % ents.len() as u64) as usize;
            let e = ents[i];
            match rng.next_u64() % 8 {
                0..=2 => {
                    if let Some(mut h) = hub.server.world.get_mut::<Hp>(e) {
                        h.hp = rng.next_u64() as u32;
                    }
                }
                3 => {
                    let p = Vec3::new(rng.next_f64() as f32, 0.0, rng.next_f64() as f32);
                    if let Some(mut tr) = hub.server.world.get_mut::<Transform>(e) {
                        tr.translation = p;
                    }
                }
                4 => {
                    let v = rng.next_u64() as u32;
                    hub.server.world.entity_mut(e).insert(Stats {
                        a: v,
                        ..Stats::default()
                    });
                }
                5 => {
                    hub.server.world.entity_mut(e).remove::<Stats>();
                }
                6 => {
                    let q = ents[(rng.next_u64() % ents.len() as u64) as usize];
                    let cycle = q == e || hub.server.world.get::<ChildOf>(q).is_some();
                    if cycle {
                        hub.server.world.entity_mut(e).remove::<ChildOf>();
                    } else if hub.server.world.get::<Children>(e).is_none() {
                        hub.server.world.entity_mut(e).insert(ChildOf(q));
                    }
                }
                _ => {
                    if ents.len() > 20 && hub.server.world.get::<Children>(e).is_none() {
                        if let Some(id) = hub.server.world.get::<NetId>(e) {
                            gone.push(*id);
                        }
                        ents.swap_remove(i);
                        hub.server.world.despawn(e);
                    }
                }
            }
        }
        hub.step();
    }
    hub.down = LinkConditions::default();
    hub.up = LinkConditions::default();
    hub.run(200);
    for c in [a, b] {
        assert_eq!(wire(&hub, c), ConnWire::Acked);
        assert_converged(&hub, c, &ents);
        for id in &gone {
            assert!(copy_of(&hub, c, *id).is_none(), "{id:?} despawned");
        }
    }
    let counters = client_state(&hub, a);
    assert!(
        counters.stored_only > 0,
        "the link really reordered: {counters:?}"
    );
    assert!(counters.record_errors == 0, "{counters:?}");
}

#[test]
fn every_client_converges_on_a_bad_link() {
    for seed in 0..6 {
        soak(100 + seed, state_setup);
    }
}

#[test]
fn every_client_converges_on_a_bad_link_through_the_budget_pipeline() {
    fn setup(world: &mut World) {
        budget_setup(world);
        world.resource_mut::<NetSettings>().replication.send_every_ticks = 2;
    }
    for seed in 0..4 {
        soak(200 + seed, setup);
    }
}

fn run_frames(serial: bool) -> (Vec<Vec<u8>>, u64, u64) {
    fn setup(world: &mut World) {
        budget_setup(world);
        world.resource_mut::<NetSettings>().replication.client_rate_bytes = 12_000;
        register_replicated::<Hot>(world, ReplicateOptions::default());
        register_replicated::<Linked>(world, ReplicateOptions::default());
    }
    set_serial_gather(serial);
    let mut hub = hub(setup, 77);
    for _ in 0..16 {
        hub.add_client();
    }
    let ents: Vec<Entity> = (0..300u32)
        .map(|i| hub.server.world.spawn((Replicated::default(), Hp { hp: i })).id())
        .collect();
    // Out-of-range temperatures, mutated every tick: the spawn reaches the full-encode cache
    // once and every later tick reaches the delta cache, so both folds of the clamp note are
    // exercised (audit fixrev-15-k net3-determinism L5: the fixture used to never mutate `Hot`,
    // leaving the delta path and its note folding untested).
    let hots: Vec<Entity> = (0..32u32)
        .map(|_| {
            hub.server
                .world
                .spawn((Replicated::default(), Hot { temp: 1000.0 }))
                .id()
        })
        .collect();
    // References to entities that are not replicated: every encode counts the
    // unreplicated-ref counter, on the delta path after the spawn tick. Two plain targets,
    // alternated per tick, keep the component changing so the delta cache folds the note too.
    let plain_a = hub.server.world.spawn_empty().id();
    let plain_b = hub.server.world.spawn_empty().id();
    let links: Vec<Entity> = (0..8u32)
        .map(|_| {
            hub.server
                .world
                .spawn((Replicated::default(), Linked { target: plain_a }))
                .id()
        })
        .collect();
    for t in 0..120u32 {
        for (i, e) in ents.iter().enumerate() {
            if (i as u32 + t).is_multiple_of(5) {
                hub.server.world.get_mut::<Hp>(*e).expect("hp").hp = t;
            }
        }
        for e in &hots {
            hub.server.world.get_mut::<Hot>(*e).expect("hot").temp = 1000.0 + t as f32;
        }
        let target = if t.is_multiple_of(2) { plain_a } else { plain_b };
        for e in &links {
            hub.server.world.get_mut::<Linked>(*e).expect("linked").target = target;
        }
        hub.step();
    }
    set_serial_gather(false);
    let counters = hub.server.world.resource::<ServerReplication>().counters;
    (
        hub.client_logs
            .iter()
            .flat_map(|log| log.iter().map(|(_, d)| d.bytes.clone()))
            .collect(),
        counters.quantize_clamped,
        counters.refs_not_replicated,
    )
}

/// § 13 test 11: packet bytes for 16 connections assembled in parallel equal the serial assembly,
/// and so do the codec counters — they fold per connection that received the bits, not per
/// worker chunk the encode happened on (audit fix-net3-determinism L4, ADR-4772).
#[test]
fn parallel_assembly_equals_serial() {
    let (parallel, parallel_clamped, parallel_refs) = run_frames(false);
    let (serial, serial_clamped, serial_refs) = run_frames(true);
    assert_eq!(parallel.len(), serial.len());
    assert_eq!(parallel, serial, "byte for byte");
    assert!(parallel.len() > 16 * 100);
    assert!(
        parallel_clamped > 0,
        "the fixture clamps: 16 connections x 32 hot entities"
    );
    assert_eq!(parallel_clamped, serial_clamped, "clamps independent of chunking");
    assert!(
        parallel_refs > 0,
        "the fixture holds references to non-replicated entities: 16 connections x 8 links"
    );
    assert_eq!(
        parallel_refs, serial_refs,
        "unreplicated refs independent of chunking"
    );
}

/// Puts two bad records in front of every state frame that carries one: a delta against a version
/// the client never had, and a record that does not decode (a spawn of no known spawn kind; until
/// ADR-10124 made record kind 3 the far update, this was a record of no known kind).
fn corrupt_records(d: &mut Datagram) {
    if tag(d) != TAG_STATE || d.bytes.len() <= 8 {
        return;
    }
    let mut r = BitReader::new(&d.bytes);
    let _ = r.read_bits(8);
    let tick = r.read_varint().expect("tick");
    let seq = r.read_varint().expect("seq");
    let count = r.read_varint().expect("count");
    let id = {
        let mut peek = r.clone();
        peek.read_varint().expect("id")
    };
    let mut w = BitWriter::new();
    w.write_bits(u64::from(TAG_STATE), 8);
    w.write_varint(tick);
    w.write_varint(seq);
    w.write_varint(count + 2);
    let mut missing = BitWriter::new();
    missing.write_bits(2, 2); // update
    missing.write_varint(0);
    missing.write_varint(1); // one tick back: never a version here
    missing.write_bool(false);
    missing.write_bool(false);
    missing.write_varint(0);
    w.write_varint(id);
    w.write_varint(missing.bit_len() as u64);
    w.append(&missing);
    let mut unknown = BitWriter::new();
    unknown.write_bits(0, 2); // spawn
    unknown.write_bool(false); // version_ago 0
    unknown.write_bits(3, 2); // no such spawn kind
    unknown.write_varint(0);
    w.write_varint(id);
    w.write_varint(unknown.bit_len() as u64);
    w.append(&unknown);
    while r.remaining_bits() >= 8 {
        w.write_bits(r.read_bits(8).expect("byte"), 8);
    }
    d.bytes = w.finish();
}

/// § 13 test 12: a corrupted record costs a resync of its entity, and the frame's other records
/// still apply.
#[test]
fn a_corrupt_record_is_resynced_alone() {
    let mut hub = hub(state_setup, 13);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    let f = hub.server.world.spawn((Replicated::default(), Hp { hp: 2 })).id();
    hub.run(8);
    let (ca, cf) = (
        copy_of(&hub, a, net_id(&hub, e)).expect("spawned"),
        copy_of(&hub, a, net_id(&hub, f)).expect("spawned"),
    );
    hub.down_rewrite = Some(corrupt_records);
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 3;
    hub.server.world.get_mut::<Hp>(f).expect("hp").hp = 4;
    hub.step();
    hub.down_rewrite = None;
    hub.run(8);
    let c = client_state(&hub, a);
    assert!(c.baseline_missing >= 1, "{c:?}");
    assert!(c.record_errors >= 1, "{c:?}");
    assert_eq!(hub.clients[a].world.get::<Hp>(ca), Some(&Hp { hp: 3 }));
    assert_eq!(hub.clients[a].world.get::<Hp>(cf), Some(&Hp { hp: 4 }));
    assert!(hub.clients[a].world.resource::<NetNode>().closed.is_none());
}

/// Profiles resolve only when something changes (ADR-1950 (6)): adding, changing and removing a
/// runtime `NetProfileRef`, and replacing a profile, still take effect on the next send tick.
#[test]
fn a_runtime_profile_change_takes_effect() {
    fn setup(world: &mut World) {
        budget_setup(world);
        if world.resource::<NetMode>().is_server() {
            let owner_only = NetProfile {
                owner_only: true,
                linger_ticks: 0,
                ..NetProfile::default()
            };
            world
                .resource_mut::<NetProfiles>()
                .insert("private", owner_only)
                .expect("profile");
        }
    }
    let mut hub = hub(setup, 14);
    let a = hub.add_client();
    let e = hub.server.world.spawn((Replicated::default(), Hp { hp: 1 })).id();
    hub.run(8);
    let id = net_id(&hub, e);
    assert!(
        copy_of(&hub, a, id).is_some(),
        "the default profile reaches everyone"
    );
    hub.server
        .world
        .entity_mut(e)
        .insert(NetProfileRef("private".into()));
    hub.run(6);
    assert!(copy_of(&hub, a, id).is_none(), "owner_only hides it");
    hub.server.world.entity_mut(e).remove::<NetProfileRef>();
    hub.run(6);
    assert!(copy_of(&hub, a, id).is_some(), "back on the default");
    hub.server
        .world
        .entity_mut(e)
        .insert(NetProfileRef("private".into()));
    hub.run(6);
    assert!(copy_of(&hub, a, id).is_none());
    hub.server
        .world
        .resource_mut::<NetProfiles>()
        .insert(
            "private",
            NetProfile {
                relevance: RelevanceShape::World,
                ..NetProfile::default()
            },
        )
        .expect("replaced");
    hub.run(6);
    assert!(
        copy_of(&hub, a, id).is_some(),
        "a replaced profile applies to its users"
    );
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Score {
    n: u32,
}

fn score_setup(world: &mut World) {
    state_setup(world);
    register_replicated::<Score>(
        world,
        ReplicateOptions {
            keep_previous: true,
            ..ReplicateOptions::default()
        },
    );
}

#[derive(Resource, Default)]
struct Seen {
    scores: Vec<Option<u32>>,
    items: Vec<(NetVecEventKind, u32)>,
}

fn collect_seen(world: &mut World) {
    let scores: Vec<Option<u32>> = world
        .get_resource::<NetChangedEvents<Score>>()
        .map_or(Vec::new(), |h| {
            h.iter().map(|c| c.previous.as_ref().map(|v| v.n)).collect()
        });
    let items = netvec_events::<Item>(world);
    let mut seen = world.resource_mut::<Seen>();
    seen.scores.extend(scores);
    seen.items.extend(items.into_iter().map(|e| (e.kind, e.id)));
}

/// An acknowledgment lost long enough sends the spawn again, with what changed since. To the copy
/// that already has it, that is a change: no second spawn notice, and `NetVec` events are the
/// difference.
#[test]
fn a_spawn_sent_again_is_a_change_to_the_copy_that_has_it() {
    let mut hub = hub(score_setup, 11);
    let a = hub.add_client();
    hub.run(6);
    assert_eq!(wire(&hub, a), ConnWire::Acked);
    hub.clients[a].world.init_resource::<Seen>();
    hub.clients[a].gameplay.add_systems(collect_seen);
    hub.up_filter = Some(Box::new(|d, _| {
        if tag(d) == TAG_ACK {
            FrameFate::Drop
        } else {
            FrameFate::Deliver
        }
    }));
    let mut list = NetVec::<Item>::new();
    let first = list.push(Item { count: 1 }).expect("room");
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Score { n: 1 },
            list,
        ))
        .id();
    hub.run(4);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    let (second, third) = {
        let mut list = hub.server.world.get_mut::<NetVec<Item>>(e).expect("list");
        (
            list.push(Item { count: 2 }).expect("room"),
            list.push(Item { count: 3 }).expect("room"),
        )
    };
    hub.server.world.get_mut::<Score>(e).expect("score").n = 2;
    hub.run(IN_FLIGHT_TIMEOUT_TICKS as usize + 4);
    hub.up_filter = None;
    hub.run(6);
    assert_eq!(copy_of(&hub, a, net_id(&hub, e)), Some(copy), "the same copy");
    let w = &hub.clients[a].world;
    assert_eq!(w.get::<Score>(copy), Some(&Score { n: 2 }));
    let seen = w.resource::<Seen>();
    assert_eq!(seen.scores, vec![None, Some(1)], "one spawn notice, one change");
    assert_eq!(
        seen.items,
        vec![
            (NetVecEventKind::Added, first),
            (NetVecEventKind::Added, second),
            (NetVecEventKind::Added, third),
        ]
    );
}

/// `net_resend` sends what the world holds now, also a write change detection never saw, on the
/// state channel like on the stream.
#[test]
fn a_resend_carries_a_write_change_detection_missed() {
    let mut hub = hub(state_setup, 12);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                sleep: NetSleep::Never,
                ..Replicated::default()
            },
            Hp { hp: 1 },
        ))
        .id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    hub.server
        .world
        .get_mut::<Hp>(e)
        .expect("hp")
        .bypass_change_detection()
        .hp = 5;
    hub.run(4);
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 1 }));
    net_resend(&mut hub.server.world, e);
    hub.run(6);
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 5 }));
}

/// State frames waiting for `NetSet::Apply` are bounded: what does not fit counts as lost.
#[test]
fn pending_state_frames_are_bounded() {
    let mut hub = hub(state_setup, 13);
    let a = hub.add_client();
    hub.run(8);
    let frame = {
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_STATE), 8);
        w.write_varint(10_000);
        w.write_varint(10_000);
        w.write_varint(0);
        w.finish()
    };
    let client = &mut hub.clients[a];
    let peer = PeerId::SERVER;
    for _ in 0..(MAX_PENDING_STATE_FRAMES + 100) {
        client.world.resource_mut::<NetNode>().inbox.push(Datagram {
            peer,
            channel: Channel::Unreliable,
            bytes: frame.clone(),
        });
    }
    net_receive(&mut client.world);
    assert_eq!(
        client.world.resource::<NetNode>().pending_state_frames(),
        MAX_PENDING_STATE_FRAMES
    );
    hub.run(2);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().pending_state_frames(),
        0
    );
    assert!(hub.clients[a].world.resource::<NetNode>().closed.is_none());
}

/// A value written again unchanged makes a new server version with nothing to send. The client
/// never sees that version's tick, so the next delta must name one it has.
#[test]
fn a_version_with_nothing_new_is_no_baseline_for_the_client() {
    let mut hub = hub(state_setup, 14);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Hp { hp: 1 },
        ))
        .id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    for _ in 0..3 {
        hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 1;
        hub.run(4);
    }
    hub.server.world.get_mut::<Hp>(e).expect("hp").hp = 2;
    hub.run(6);
    assert_eq!(hub.clients[a].world.get::<Hp>(copy), Some(&Hp { hp: 2 }));
    assert_eq!(client_state(&hub, a).baseline_missing, 0);
    assert_eq!(client_state(&hub, a).record_errors, 0);
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Secret {
    n: u32,
}

fn secret_setup(world: &mut World) {
    state_setup(world);
    register_replicated::<Secret>(
        world,
        ReplicateOptions {
            audience: Audience::OwnerOnly,
            ..ReplicateOptions::default()
        },
    );
}

/// Ownership opens an owner-only component without any new server version: the record for the
/// version the client already holds still brings it.
#[test]
fn an_audience_change_adds_to_the_version_the_client_has() {
    let mut hub = hub(secret_setup, 15);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Hp { hp: 1 },
            Secret { n: 7 },
        ))
        .id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert_eq!(hub.clients[a].world.get::<Secret>(copy), None);
    let peer = peer_of(&hub, a);
    hub.server.world.entity_mut(e).insert(NetOwner(peer));
    hub.run(6);
    assert_eq!(hub.clients[a].world.get::<Secret>(copy), Some(&Secret { n: 7 }));
    assert_eq!(client_state(&hub, a).baseline_missing, 0);
}

/// A copy the client's gameplay despawned comes back from the version the client already holds.
#[test]
fn a_lost_copy_returns_from_a_version_the_client_has() {
    let mut hub = hub(state_setup, 16);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Hp { hp: 3 },
        ))
        .id();
    hub.run(8);
    let copy = copy_of(&hub, a, net_id(&hub, e)).expect("spawned");
    assert!(hub.clients[a].world.despawn(copy));
    hub.run(8);
    let back = copy_of(&hub, a, net_id(&hub, e)).expect("back");
    assert_ne!(back, copy);
    assert_eq!(hub.clients[a].world.get::<Hp>(back), Some(&Hp { hp: 3 }));
}

/// Audit net1 perf H1: an acknowledged connection that already holds a large asleep set walks
/// the outstanding copy when one entity changes, not every copy it holds. The priority pass
/// reads that copy's packed slot, not every held slot.
#[test]
fn acked_quiet_set_visits_the_outstanding_copy() {
    let mut hub = hub(budget_setup, 41);
    hub.negotiate = true;
    let a = hub.add_client();
    const N: usize = 48;
    let mut entities = Vec::with_capacity(N);
    for i in 0..N {
        entities.push(
            hub.server
                .world
                .spawn((
                    Replicated {
                        sleep: NetSleep::StartAsleep,
                        ..Replicated::default()
                    },
                    Hp { hp: i as u32 },
                ))
                .id(),
        );
    }
    hub.run(10);
    assert_eq!(
        wire(&hub, a),
        ConnWire::Acked,
        "protocol 2 settles on the state channel"
    );
    let asleep = entities
        .iter()
        .filter(|e| {
            hub.server
                .world
                .resource::<ServerReplication>()
                .is_asleep(net_id(&hub, **e))
                == Some(true)
        })
        .count();
    assert!(asleep > N * 3 / 4, "asleep {asleep} of {N}");
    let changed = entities[0];
    hub.server.world.get_mut::<Hp>(changed).expect("hp").hp = 999;
    hub.run(1);
    let rep = hub.server.world.resource::<ServerReplication>();
    let visited = rep.copies_visited();
    let slots = rep.priority_slots();
    assert!(visited >= 1, "the changed copy was walked, got {visited}");
    assert!(
        visited < (N as u64) / 4,
        "walked {visited} copies of {N}; a quiet Acked set must not walk every held copy"
    );
    assert!(
        slots < (N as u64) / 4,
        "priority pass read {slots} packed slots of {N}"
    );
    let id = net_id(&hub, changed);
    for _ in 0..8 {
        if copy_of(&hub, a, id)
            .is_some_and(|c| hub.clients[a].world.get::<Hp>(c).is_some_and(|h| h.hp == 999))
        {
            return;
        }
        hub.run(1);
    }
    panic!("client never saw hp 999");
}

/// Audit net1 perf M5: despawning a replicated entity drops its bandwidth row. The map holds
/// the live set, not every id ever sent.
#[test]
fn despawn_drops_bandwidth_by_entity() {
    let mut hub = hub(budget_setup, 43);
    hub.negotiate = true;
    let a = hub.add_client();
    const N: usize = 8;
    let mut entities = Vec::with_capacity(N);
    for i in 0..N {
        entities.push(
            hub.server
                .world
                .spawn((Replicated::default(), Hp { hp: i as u32 }))
                .id(),
        );
    }
    hub.run(8);
    assert_eq!(wire(&hub, a), ConnWire::Acked);
    let peer = peer_of(&hub, a);
    let before = hub
        .server
        .world
        .resource::<NetInterest>()
        .peers
        .get(&peer)
        .map(|p| p.send.bandwidth.by_entity.len())
        .unwrap_or(0);
    assert!(before >= N, "sends charged one row per entity, got {before}");
    for e in &entities {
        let _ = hub.server.world.despawn(*e);
    }
    hub.run(4);
    let after = hub
        .server
        .world
        .resource::<NetInterest>()
        .peers
        .get(&peer)
        .map(|p| p.send.bandwidth.by_entity.len())
        .unwrap_or(0);
    assert_eq!(
        after, 0,
        "despawned entities left {after} bandwidth rows (charged {before})"
    );
}

thread_local! {
    static PRED_CALLS: Cell<u64> = const { Cell::new(0) };
}

fn count_pred(_world: &World, _entity: Entity) -> bool {
    PRED_CALLS.with(|c| c.set(c.get() + 1));
    true
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Marked(u32);

fn marked_setup(world: &mut World) {
    budget_setup(world);
    register_replicated::<Marked>(
        world,
        ReplicateOptions {
            audience: Audience::Predicate(count_pred),
            ..ReplicateOptions::default()
        },
    );
}

/// A predicate runs only for entities that carry the component, and a steady tick queues no
/// update candidate.
#[test]
fn predicate_runs_for_carriers_and_a_steady_tick_queues_no_update() {
    let mut hub = hub(marked_setup, 21);
    let _client = hub.add_client();
    for i in 0..20 {
        hub.server.world.spawn((
            Replicated::default(),
            Transform::from_translation(Vec3::new(i as f32, 0.0, 0.0)),
        ));
    }
    for i in 0..5 {
        hub.server.world.spawn((
            Replicated::default(),
            Transform::from_translation(Vec3::new(i as f32, 2.0, 0.0)),
            Marked(1),
        ));
    }
    hub.run(8);
    PRED_CALLS.with(|c| c.set(0));
    hub.run(6);
    let calls = PRED_CALLS.with(|c| c.get());
    assert!(
        calls <= 5 * 6,
        "predicate calls for 5 carriers over 6 steps: {calls}"
    );
    let queued = hub
        .server
        .world
        .resource::<ServerReplication>()
        .update_candidates_last_gather();
    assert_eq!(queued, 0, "a steady gather queued {queued} update candidates");
}

// ---- ADR-8760: the client entity cap -------------------------------------------------------

/// The client's three state-channel maps and its replicated entity count.
fn client_sizes(hub: &LoopbackHub, client: usize) -> (usize, usize, usize, usize) {
    let w = &hub.clients[client].world;
    let (rings, tombs, own) = w.resource::<ClientReplication>().state.map_lens();
    (rings, tombs, own, w.resource::<NetEntities>().len())
}

fn set_client_cap(hub: &mut LoopbackHub, client: usize, cap: usize) {
    hub.clients[client]
        .world
        .resource_mut::<ClientReplication>()
        .state
        .entity_cap = Some(cap);
}

fn spawn_hp(hub: &mut LoopbackHub, n: usize) -> Vec<Entity> {
    (0..n)
        .map(|i| {
            hub.server
                .world
                .spawn((Replicated::default(), Hp { hp: i as u32 }))
                .id()
        })
        .collect()
}

/// ADR-8760: a server that spawns cap + N fresh ids over the state channel leaves the client's
/// three maps and its replicated entity count at or under the cap; the spawn past the cap is
/// refused (counted) and the session ends with a protocol error.
#[test]
fn a_server_spawning_past_the_client_cap_is_refused() {
    const CAP: usize = 32;
    let mut hub = hub(state_setup, 60);
    let a = hub.add_client();
    set_client_cap(&mut hub, a, CAP);
    spawn_hp(&mut hub, CAP + 9);
    hub.run(24);
    let (rings, tombs, own, copies) = client_sizes(&hub, a);
    assert!(
        rings <= CAP && tombs <= CAP && own <= CAP && copies <= CAP,
        "maps ({rings}, {tombs}, {own}) and {copies} copies past the cap {CAP}"
    );
    assert!(client_state(&hub, a).over_cap >= 1, "the refusal is counted");
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError),
        "the session ends with a protocol error"
    );
}

/// ADR-8760, the reliable tick stream: the same cap holds for a client on protocol 1's stream.
#[test]
fn a_server_spawning_past_the_client_cap_is_refused_on_the_tick_stream() {
    fn setup(world: &mut World) {
        registered(world);
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Reliable;
    }
    const CAP: usize = 32;
    let mut hub = hub(setup, 61);
    let a = hub.add_client();
    set_client_cap(&mut hub, a, CAP);
    spawn_hp(&mut hub, CAP + 9);
    hub.run(24);
    let (rings, tombs, own, copies) = client_sizes(&hub, a);
    assert!(
        rings <= CAP && tombs <= CAP && own <= CAP && copies <= CAP,
        "maps ({rings}, {tombs}, {own}) and {copies} copies past the cap {CAP}"
    );
    assert!(client_state(&hub, a).over_cap >= 1);
    assert_eq!(
        hub.clients[a].world.resource::<NetNode>().closed,
        Some(DisconnectReason::ProtocolError)
    );
}

/// ADR-8760: a normal session at 90 % of the cap is unaffected: every copy arrives, updates
/// keep flowing, nothing is refused or evicted, and the session stays open.
#[test]
fn a_session_at_ninety_percent_of_the_cap_is_unaffected() {
    const CAP: usize = 40;
    let mut hub = hub(state_setup, 62);
    let a = hub.add_client();
    set_client_cap(&mut hub, a, CAP);
    let ents = spawn_hp(&mut hub, CAP * 9 / 10);
    hub.run(24);
    for (i, e) in ents.iter().enumerate() {
        hub.server.world.get_mut::<Hp>(*e).expect("hp").hp = 1000 + i as u32;
    }
    hub.run(12);
    for (i, e) in ents.iter().enumerate() {
        let copy = copy_of(&hub, a, net_id(&hub, *e)).expect("every copy arrives");
        assert_eq!(
            hub.clients[a].world.get::<Hp>(copy),
            Some(&Hp { hp: 1000 + i as u32 })
        );
    }
    let c = client_state(&hub, a);
    assert_eq!((c.over_cap, c.cap_evicted), (0, 0));
    assert_eq!(client_sizes(&hub, a).3, CAP * 9 / 10);
    assert!(hub.clients[a].world.resource::<NetNode>().closed.is_none());
}

/// ADR-8850: a host that sets `entity_cap` before connecting keeps it across the bridge's
/// `client_session_started` (rev-claude-55 finding 1: it ran
/// `client.state = ClientStateChannel::default()`, wiping the field `level_bind_cap`-style
/// survival was written for) and across a replay's reset. After either, 17 spawns still refuse
/// the 17th at a cap of 16.
#[test]
fn the_host_entity_cap_survives_a_session_start_and_a_replay_reset() {
    const CAP: usize = 16;
    // The bridge's connect path (bridge.rs calls `client_session_started` on every
    // `ClientEvent::Connected`) must not wipe the cap.
    {
        let mut hub = hub(state_setup, 64);
        let a = hub.add_client();
        set_client_cap(&mut hub, a, CAP);
        regia_net::replication::client_session_started(&mut hub.clients[a].world);
        spawn_hp(&mut hub, CAP + 1);
        hub.run(24);
        let (rings, tombs, own, copies) = client_sizes(&hub, a);
        assert!(
            rings <= CAP && tombs <= CAP && own <= CAP && copies <= CAP,
            "maps ({rings}, {tombs}, {own}) and {copies} copies past the cap {CAP}"
        );
        assert!(
            client_state(&hub, a).over_cap >= 1,
            "the 17th spawn is refused after the session start"
        );
    }
    // A replay's reset (`reset_client_world`: `forget_everything` and another
    // `client_session_started`) must not wipe it either.
    {
        let mut hub = hub(state_setup, 65);
        let a = hub.add_client();
        set_client_cap(&mut hub, a, CAP);
        regia_net::recorder::playback::reset_client_world(&mut hub.clients[a].world);
        spawn_hp(&mut hub, CAP + 1);
        hub.run(24);
        let (rings, tombs, own, copies) = client_sizes(&hub, a);
        assert!(
            rings <= CAP && tombs <= CAP && own <= CAP && copies <= CAP,
            "maps ({rings}, {tombs}, {own}) and {copies} copies past the cap {CAP}"
        );
        assert!(
            client_state(&hub, a).over_cap >= 1,
            "the 17th spawn is refused after the replay reset"
        );
    }
}

/// ADR-8760: tombstones still expire as before (`TOMBSTONE_TICKS` after the despawn), and churn
/// that keeps them under the cap evicts nothing.
#[test]
fn tombstones_still_expire_under_the_cap() {
    const CAP: usize = 64;
    let mut hub = hub(state_setup, 63);
    let a = hub.add_client();
    set_client_cap(&mut hub, a, CAP);
    let ents = spawn_hp(&mut hub, 20);
    hub.run(16);
    for e in &ents {
        let _ = hub.server.world.despawn(*e);
    }
    hub.run(8);
    let (rings, tombs, _, copies) = client_sizes(&hub, a);
    assert_eq!((rings, copies), (0, 0), "every copy despawned");
    assert_eq!(tombs, 20, "one tombstone per despawn");
    hub.run(regia_net::replication::TOMBSTONE_TICKS as usize / 2);
    assert_eq!(client_sizes(&hub, a).1, 20, "kept for TOMBSTONE_TICKS");
    hub.run(regia_net::replication::TOMBSTONE_TICKS as usize / 2 + 128);
    assert_eq!(client_sizes(&hub, a).1, 0, "expired");
    let c = client_state(&hub, a);
    assert_eq!((c.over_cap, c.cap_evicted), (0, 0));
    assert!(hub.clients[a].world.resource::<NetNode>().closed.is_none());
}

/// Spec 120 § 5.4: a large configured MTU must still fit the sealed transport.
#[test]
fn a_1400_byte_mtu_keeps_full_state_frames_unreliable_and_within_the_transport() {
    fn setup(world: &mut World) {
        state_setup(world);
        world.resource_mut::<NetSettings>().replication.mtu_bytes = 1400;
    }
    let mut hub = hub(setup, 7949);
    hub.negotiate = true;
    let client = hub.add_client();
    for i in 0..200 {
        hub.server.world.spawn((
            Replicated::default(),
            Stats {
                a: i,
                b: i,
                c: i,
                d: i,
                e: i,
                f: i,
            },
        ));
    }
    hub.run(30);
    let frames = frames(&hub, client, TAG_STATE);
    assert!(!frames.is_empty());
    for (_, frame) in frames {
        assert_eq!(frame.channel, Channel::Unreliable);
        // 1154 connection bytes, minus game lane (1) and NetFrame codec (3).
        assert!(
            frame.bytes.len() <= 1150,
            "state frame {} exceeds sealed transport",
            frame.bytes.len()
        );
    }
}
