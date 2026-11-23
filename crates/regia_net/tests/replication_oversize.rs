// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.1 (N212, ADR-8419): large and growing entity state, in-flight timeouts from the
//! round trip, the state log sized to the frames in flight, and the byte budget for every record
//! (audit row R5, adversarial row 4). Loopback hub, fixed steps, seeded conditions, no wall clock.

use regia_app::FixedTime;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::CodecContext;
use regia_net::replication::{ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{Datagram, NetEntities};
use regia_net::testing::LoopbackHub;
use regia_net::*;

const TAG_STATE: u8 = 7;

/// Raw bytes: the record is as long as the payload.
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

/// The state channel with the budget pipeline on, every entity relevant everywhere.
fn base_setup(world: &mut World) {
    register_replicated::<Blob>(world, ReplicateOptions::default());
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
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

/// A rate that never limits these tests' records.
fn fast_setup(world: &mut World) {
    base_setup(world);
    world.resource_mut::<NetSettings>().replication.client_rate_bytes = 10_000_000;
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn net_id(hub: &LoopbackHub, e: Entity) -> NetId {
    *hub.server.world.get::<NetId>(e).expect("net id")
}

fn copy_of(hub: &LoopbackHub, client: usize, id: NetId) -> Option<Entity> {
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

/// A client's copy as `(len, first byte)`: compact in assertion messages.
fn client_blob(hub: &LoopbackHub, client: usize, id: NetId) -> Option<(usize, u8)> {
    let copy = copy_of(hub, client, id)?;
    hub.clients[client]
        .world
        .get::<Blob>(copy)
        .map(|b| (b.0.len(), b.0.first().copied().unwrap_or(0)))
}

/// A blob that is `bytes` of `fill`, as [`client_blob`] shows it.
fn shown(bytes: usize, fill: u8) -> (usize, u8) {
    (bytes, fill)
}

fn server(hub: &LoopbackHub) -> &ServerReplication {
    hub.server.world.resource::<ServerReplication>()
}

fn tag(d: &Datagram) -> u8 {
    d.bytes.first().copied().unwrap_or(0)
}

/// The server tick in a frame's header.
fn frame_tick(d: &Datagram) -> u32 {
    let mut r = BitReader::new(&d.bytes);
    r.read_bits(8).expect("tag");
    r.read_varint().expect("tick") as u32
}

fn set_blob(hub: &mut LoopbackHub, e: Entity, bytes: usize, fill: u8) {
    hub.server.world.entity_mut(e).insert(Blob(vec![fill; bytes]));
}

/// R5: an entity that spawns under the 16 KiB entity-state cap and grows past it through
/// updates, on a connection that then loses its baseline (a resync), is never frozen on any
/// client: each client either keeps the server's state or has had the copy taken away with the
/// server reporting `state_too_large`. The base refused the full record every tick and sent no
/// update without a baseline, so the resynced client kept the 12 KiB copy forever. Shrinking
/// brings it back, and a second crossing is warned about again (once per crossing).
#[test]
fn an_entity_growing_past_the_cap_is_never_frozen() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8419, fast_setup);
    hub.down.delay_ticks = 2;
    hub.up.delay_ticks = 2;
    let a = hub.add_client();
    let b = hub.add_client();
    let e = hub.server.world.spawn((rep(), Blob(vec![1; 12 * 1024]))).id();
    hub.run(40);
    let id = net_id(&hub, e);
    for c in [a, b] {
        assert_eq!(
            client_blob(&hub, c, id),
            Some(shown(12 * 1024, 1)),
            "client {c} has the 12 KiB spawn"
        );
    }
    let peer_a = hub.clients[a].peer;
    for t in 0..60u32 {
        set_blob(&mut hub, e, 20 * 1024, 2 + (t % 200) as u8);
        if t == 5 {
            // The client lost its baseline: it asks for the full state again.
            hub.server
                .world
                .resource_mut::<ServerReplication>()
                .request_resync(peer_a, [id]);
        }
        hub.step();
    }
    hub.run(30);
    let truth = hub
        .server
        .world
        .get::<Blob>(e)
        .map(|b| (b.0.len(), b.0.first().copied().unwrap_or(0)))
        .expect("server blob");
    let reported = server(&hub).counters.state_too_large;
    for c in [a, b] {
        match client_blob(&hub, c, id) {
            Some(blob) => assert!(
                blob == truth,
                "client {c} holds a frozen copy (bytes, fill) {blob:?} while the server has {truth:?}"
            ),
            None => assert!(
                reported >= 1,
                "client {c} lost the copy without a state_too_large report"
            ),
        }
    }
    assert_eq!(reported, 1, "one report for one crossing of the cap");

    // Back under the cap: every client has the entity again, at the server's state.
    set_blob(&mut hub, e, 8 * 1024, 77);
    hub.run(40);
    for c in [a, b] {
        assert_eq!(
            client_blob(&hub, c, id),
            Some(shown(8 * 1024, 77)),
            "client {c} follows the entity once it fits"
        );
    }
    // A second crossing is a second report.
    for t in 0..20u32 {
        set_blob(&mut hub, e, 18 * 1024, 100 + t as u8);
        hub.step();
    }
    hub.run(20);
    assert_eq!(
        server(&hub).counters.state_too_large,
        2,
        "warned once per crossing"
    );
    assert_eq!(server(&hub).too_large_entities(), vec![id]);
    for c in [a, b] {
        assert!(
            client_blob(&hub, c, id).is_none_or(|blob| blob == shown(18 * 1024, 119)),
            "client {c} is not frozen after the second crossing"
        );
    }
}

/// R5 at 650 ms: an entity of 4 KiB that changes every tick goes as one reliable record at a
/// time, and the next waits for the acknowledgment of the last. The base freed the slot after a
/// fixed 30 ticks (500 ms at 60 Hz), so above that round trip a new 4 KiB record was queued while
/// the last was still on its way.
#[test]
fn no_duplicate_oversize_records_in_flight_at_650_ms() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8421, fast_setup);
    // 60 Hz: 19 ticks each way plus one hop tick each way is a 650 ms round trip.
    hub.down.delay_ticks = 19;
    hub.up.delay_ticks = 19;
    let a = hub.add_client();
    let e = hub.server.world.spawn((rep(), Blob(vec![0; 4096]))).id();
    for t in 0..600u32 {
        set_blob(&mut hub, e, 4096, (t % 251) as u8);
        hub.step();
    }
    let sends: Vec<u32> = hub.client_logs[a]
        .iter()
        .filter(|(_, d)| tag(d) == TAG_STATE && d.channel.is_reliable())
        .map(|(_, d)| frame_tick(d))
        .collect();
    assert!(
        sends.len() >= 8,
        "the entity keeps updating: {} reliable records",
        sends.len()
    );
    let gaps: Vec<u32> = sends.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= 2 * 19),
        "a record larger than a packet waits for the last one's acknowledgment (round trip >= 38 ticks): gaps {gaps:?}"
    );
    let link = server(&hub).state_link(hub.clients[a].peer).expect("link");
    assert_eq!(link.wire, ConnWire::Acked);
    let rtt = link.rtt_ticks().expect("a round-trip sample");
    assert!((38.0..=46.0).contains(&rtt), "measured round trip {rtt} ticks");
}

/// The state log keeps the frames in flight: 4 frames per tick at a 600 ms round trip on a
/// 128 Hz tick is about 310 frames on the way, more than the 256 the base remembered, so the
/// base dropped every frame before its acknowledgment came back and no baseline ever advanced
/// (every record went again as a spawn). At 60 Hz the same rate is 144 frames, inside the old log.
#[test]
fn baselines_advance_at_four_frames_per_tick_and_600_ms() {
    fn setup(world: &mut World) {
        fast_setup(world);
        let mut fixed = FixedTime::default();
        fixed.hz = 128.0;
        world.insert_resource(fixed);
    }
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8422, setup);
    // 128 Hz: 38 ticks each way plus a hop tick each way is about 610 ms.
    hub.down.delay_ticks = 38;
    hub.up.delay_ticks = 38;
    let a = hub.add_client();
    // 32 entities of 250 bytes, half of them changing each tick: four frames of the 1200-byte
    // state MTU every tick. Each entity changes every other tick, so a round trip is about 39 of
    // its versions, inside the client's 64-version ring (a change every tick at 77 ticks would
    // expire the baselines by the ring, a separate limit: ADR-8419 known issue).
    let ents: Vec<Entity> = (0..32u8)
        .map(|i| hub.server.world.spawn((rep(), Blob(vec![i; 250]))).id())
        .collect();
    let mut per_tick: Vec<usize> = Vec::new();
    for t in 0..700u32 {
        for (i, e) in ents.iter().enumerate() {
            if (t as usize + i).is_multiple_of(2) {
                set_blob(&mut hub, *e, 250, (t as usize + i) as u8);
            }
        }
        let before = hub.client_logs[a].len();
        hub.step();
        if t >= 300 {
            per_tick.push(
                hub.client_logs[a][before..]
                    .iter()
                    .filter(|(_, d)| tag(d) == TAG_STATE)
                    .count(),
            );
        }
    }
    let mean = per_tick.iter().sum::<usize>() as f64 / per_tick.len() as f64;
    assert!(mean >= 4.0, "about four state frames per tick: {mean}");
    let peer = hub.clients[a].peer;
    let now = hub.server_tick().0;
    let link = server(&hub).state_link(peer).expect("link");
    assert!(
        link.counters.packets_acked * 10 >= link.counters.packets_sent * 8,
        "most frames are acknowledged: {} of {}",
        link.counters.packets_acked,
        link.counters.packets_sent
    );
    for e in &ents {
        let (_, base) = server(&hub).copy_ticks(peer, net_id(&hub, *e)).expect("a copy");
        let base = base.expect("an acknowledged baseline");
        assert!(
            now - base.0 <= 3 * 78,
            "the baseline advances: {} ticks behind at tick {now}",
            now - base.0
        );
    }
}

/// The spawn-burst check: `count` spawns of `bytes` each on the default 49,152 B/s rate (819 bytes
/// per 60 Hz send tick). Returns the most spawns one tick delivered and how many ticks delivered any.
fn spawn_burst(setup: fn(&mut World), seed: u64, count: u8, bytes: usize) -> (usize, usize) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    let a = hub.add_client();
    hub.run(10);
    let ents: Vec<Entity> = (0..count)
        .map(|i| hub.server.world.spawn((rep(), Blob(vec![i; bytes]))).id())
        .collect();
    let mut most_in_a_tick = 0usize;
    let mut ticks_with_spawns = 0usize;
    for _ in 0..240 {
        hub.step();
        let spawned = hub.clients[a]
            .world
            .resource::<regia_net::replication::NetEvents>()
            .spawned
            .len();
        most_in_a_tick = most_in_a_tick.max(spawned);
        ticks_with_spawns += usize::from(spawned > 0);
    }
    for e in &ents {
        assert!(
            copy_of(&hub, a, net_id(&hub, *e)).is_some(),
            "every spawn arrives"
        );
    }
    (most_in_a_tick, ticks_with_spawns)
}

/// Adversarial row 4: with the default 49,152 B/s rate, 24 spawns of 2 KiB do not all go in one
/// tick. On the reliable tick stream (where no one-at-a-time rule for large records applies) the
/// base let spawns skip the byte test: all 24 (49 KB) left in the first frame, a second's worth of
/// the rate in one tick.
#[test]
fn a_spawn_burst_stays_within_the_byte_budget() {
    fn setup(world: &mut World) {
        base_setup(world);
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Reliable;
    }
    let (most, ticks) = spawn_burst(setup, 8423, 24, 2048);
    assert_eq!(
        most, 1,
        "a 2 KiB spawn is over an 819-byte tick budget: one goes per tick at most"
    );
    assert_eq!(
        ticks, 24,
        "the debt spaces them out, one per send tick with budget"
    );
}

/// The same on the state channel with records under a packet: 24 spawns of 600 bytes are 14 KB
/// against an 819-byte tick; the base sent all 24 in one tick.
#[test]
fn a_spawn_burst_stays_within_the_byte_budget_on_the_state_channel() {
    let (most, ticks) = spawn_burst(base_setup, 8425, 24, 600);
    assert!(
        most <= 2,
        "819 bytes a tick (plus carry) fit at most two 600-byte spawns, saw {most}"
    );
    assert!(ticks >= 12, "spread over at least 12 ticks: {ticks}");
}

/// Adversarial row 4: with `max_interval_ticks = 1` every record is forced every tick; forced
/// records obey the byte test and the debt carries over, so the bytes per second stay at the
/// rate. The base sent every forced record whatever the budget (and clamped the debt to one
/// tick): 40 entities of 300 bytes changing every tick were 12 KB per tick, 720 KB/s on a
/// 49,152 B/s rate.
#[test]
fn forced_records_hold_the_rate() {
    fn setup(world: &mut World) {
        base_setup(world);
        if world.resource::<NetMode>().is_server() {
            let every_tick = NetProfile {
                relevance: RelevanceShape::World,
                linger_ticks: 0,
                max_interval_ticks: Some(1),
                ..NetProfile::default()
            };
            world
                .resource_mut::<NetProfiles>()
                .insert("every_tick", every_tick)
                .expect("profile");
            world.resource_mut::<NetProfiles>().set_default("every_tick");
        }
    }
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8424, setup);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..40u8)
        .map(|i| hub.server.world.spawn((rep(), Blob(vec![i; 300]))).id())
        .collect();
    let mut t = 0u32;
    let mut churn = |hub: &mut LoopbackHub, n: u32| {
        for _ in 0..n {
            for (i, e) in ents.iter().enumerate() {
                set_blob(hub, *e, 300, (t as usize + i) as u8);
            }
            hub.step();
            t += 1;
        }
    };
    churn(&mut hub, 300);
    let start = hub.client_logs[a].len();
    // Ten seconds at 60 Hz.
    churn(&mut hub, 600);
    let state_bytes: usize = hub.client_logs[a][start..]
        .iter()
        .filter(|(_, d)| tag(d) == TAG_STATE)
        .map(|(_, d)| d.bytes.len())
        .sum();
    let per_second = state_bytes as f64 / 10.0;
    // The budget charges each record's bytes and framing; frame headers (a few bytes a frame,
    // one or two frames a tick) ride on top: at most 60 x 2 x 16 bytes a second.
    assert!(
        per_second <= 49_152.0 + 60.0 * 2.0 * 16.0,
        "{per_second} B/s on a 49,152 B/s rate"
    );
    assert!(per_second >= 0.8 * 49_152.0, "the rate is used: {per_second} B/s");
}
