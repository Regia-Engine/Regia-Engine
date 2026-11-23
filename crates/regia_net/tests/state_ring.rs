// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.1 (ADR-10051, KI-NT-009): the client's version ring on long round trips. An
//! entity that changes every tick on a round trip longer than the ring lost its baseline on
//! every acknowledgment and went as a full record each time; the ring is now sized for the
//! `rtt-extreme` profile (1,500 ms one way) and a client keeps only the versions the server can
//! still name. Loopback hub, fixed steps, seeded conditions, no wall clock.

use regia_app::FixedTime;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::CodecContext;
use regia_net::replication::{ClientReplication, ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{Datagram, NetEntities};
use regia_net::testing::{FrameFate, LoopbackHub};
use regia_net::*;

const TAG_STATE: u8 = 7;
/// The client's state acknowledgment frame.
const TAG_ACK: u8 = 4;
/// Bytes of the part of an entity that never changes: what a full record costs over a delta.
const KIT_BYTES: usize = 96;

/// The part of an entity's state that never changes after the spawn.
#[derive(Component, Clone, Debug, PartialEq, Default)]
struct Kit(Vec<u8>);

impl ReplicateCodec for Kit {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.0.len() as u64);
        for b in &self.0 {
            w.write_bits(u64::from(*b), 8);
        }
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(1 << 16)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(r.read_bits(8)? as u8);
        }
        Ok(Self(v))
    }
}

/// The part that changes every tick: the server tick that wrote it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Default)]
struct Beat(u32);

impl ReplicateCodec for Beat {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(u64::from(self.0));
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        u32::try_from(r.read_varint()?)
            .map(Self)
            .map_err(|_| DecodeError::Invalid("beat"))
    }
}

/// The state channel with the budget pipeline on and the default 49,152 B/s rate, every entity
/// relevant everywhere.
fn base_setup(world: &mut World) {
    register_replicated::<Kit>(world, ReplicateOptions::default());
    register_replicated::<Beat>(world, ReplicateOptions::default());
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

/// [`fast_setup`] on a 128 Hz tick.
fn fast_setup_128(world: &mut World) {
    fast_setup(world);
    let mut fixed = FixedTime::default();
    fixed.hz = 128.0;
    world.insert_resource(fixed);
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
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

fn server(hub: &LoopbackHub) -> &ServerReplication {
    hub.server.world.resource::<ServerReplication>()
}

fn client(hub: &LoopbackHub, c: usize) -> &ClientReplication {
    hub.clients[c].world.resource::<ClientReplication>()
}

/// `count` entities with a [`Kit`] and a [`Beat`].
fn spawn_beaters(hub: &mut LoopbackHub, count: usize) -> Vec<Entity> {
    (0..count)
        .map(|i| {
            hub.server
                .world
                .spawn((rep(), Kit(vec![i as u8; KIT_BYTES]), Beat(0)))
                .id()
        })
        .collect()
}

/// Writes `beat` into every entity (a new version of each this tick).
fn beat_all(hub: &mut LoopbackHub, ents: &[Entity], beat: u32) {
    for e in ents {
        hub.server.world.entity_mut(*e).insert(Beat(beat));
    }
}

/// The [`Beat`] each of client `c`'s copies shows, `None` for a copy it lacks.
fn shown_beats(hub: &LoopbackHub, c: usize, ents: &[Entity]) -> Vec<Option<u32>> {
    ents.iter()
        .map(|e| {
            let id = *hub.server.world.get::<NetId>(*e)?;
            let copy = hub.clients[c].world.resource::<NetEntities>().entity(id)?;
            hub.clients[c].world.get::<Beat>(copy).map(|b| b.0)
        })
        .collect()
}

/// What one run measured from `from` on.
#[derive(Debug)]
struct Measured {
    /// Baselines the server expired (a full record went instead of a delta).
    expired: u64,
    /// State-frame bytes delivered to the client per entity and tick.
    bytes_per_entity_tick: f64,
    /// The 99th percentile of how many ticks a copy's [`Beat`] is behind the server's, beyond
    /// the one-way path.
    staleness_p99: u32,
    /// The most versions the client stored per entity.
    versions_per_entity: usize,
    /// Deltas the client could not decode for a missing baseline.
    baseline_missing: u64,
}

/// Runs `ticks` steps with `count` entities that change every tick, and measures from `from`.
fn run_beaters(hub: &mut LoopbackHub, c: usize, count: usize, ticks: u32, from: u32) -> Measured {
    let ents = spawn_beaters(hub, count);
    let one_way = hub.down.delay_ticks as u32 + 1;
    let peer = hub.clients[c].peer;
    let (mut expired_at_from, mut bytes, mut log_from) = (0, 0usize, 0usize);
    let mut stale: Vec<u32> = Vec::new();
    let mut versions = 0usize;
    for t in 1..=ticks {
        beat_all(hub, &ents, t);
        hub.step();
        if t == from {
            expired_at_from = server(hub)
                .state_link(peer)
                .expect("link")
                .counters
                .baselines_expired;
            log_from = hub.client_logs[c].len();
        }
        if t > from {
            for shown in shown_beats(hub, c, &ents) {
                let shown = shown.expect("every copy is held");
                stale.push((t - shown).saturating_sub(one_way));
            }
            let (entities, stored) = client(hub, c).state.stored();
            versions = versions.max(stored.div_ceil(entities.max(1)));
        }
    }
    for (_, d) in &hub.client_logs[c][log_from..] {
        if tag(d) == TAG_STATE {
            bytes += d.bytes.len();
        }
    }
    let link = server(hub).state_link(peer).expect("link");
    assert_eq!(link.wire, ConnWire::Acked);
    stale.sort_unstable();
    Measured {
        expired: link.counters.baselines_expired - expired_at_from,
        bytes_per_entity_tick: bytes as f64 / f64::from(ticks - from) / count as f64,
        staleness_p99: stale[(stale.len() * 99 / 100).min(stale.len() - 1)],
        versions_per_entity: versions,
        baseline_missing: client(hub, c).state.counters.baseline_missing,
    }
}

/// KI-NT-009 on the `rtt-extreme` link at 60 Hz (1,500 ms and 20 % loss each way: a 180-tick
/// round trip): an entity that changes every tick keeps its acknowledged baseline, so its
/// records are deltas (a few bytes), not the full state every time. The base's 64-version ring
/// expired every baseline as its acknowledgment arrived.
#[test]
fn baselines_survive_the_rtt_extreme_round_trip() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10_051, fast_setup);
    hub.down.delay_ticks = 89;
    hub.up.delay_ticks = 89;
    hub.down.loss = 0.2;
    hub.up.loss = 0.2;
    let a = hub.add_client();
    let m = run_beaters(&mut hub, a, 8, 1_800, 600);
    assert_eq!(
        m.expired, 0,
        "no baseline expires on a 180-tick round trip: {m:?}"
    );
    assert_eq!(
        m.baseline_missing, 0,
        "the client holds every baseline named: {m:?}"
    );
    // A delta record is the id, a length, two `ago` codes and one changed component: under 16
    // bytes. A full record carries the 96-byte kit.
    assert!(
        m.bytes_per_entity_tick <= 16.0,
        "records are deltas, not full states: {m:?}"
    );
}

/// The same link on the default 49,152 B/s rate with 48 entities: full records are six times the
/// rate, so on the base each copy was refreshed only every few ticks; deltas fit every tick.
/// No loss here, so the bar is spec 120 § 15.2's staleness bar (4 ticks beyond the path).
#[test]
fn rtt_extreme_stays_fresh_within_the_default_rate() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10_052, base_setup);
    hub.down.delay_ticks = 89;
    hub.up.delay_ticks = 89;
    let a = hub.add_client();
    let m = run_beaters(&mut hub, a, 48, 1_500, 600);
    assert!(
        m.staleness_p99 <= 4,
        "staleness p99 beyond the path is within 4 ticks: {m:?}"
    );
    assert_eq!(m.expired, 0, "{m:?}");
}

/// The `high-rtt` profile's 650 ms round trip on a 128 Hz tick is 83 ticks (ADR-8419's known
/// issue named 77): past the base's 64-version ring, inside the new one.
#[test]
fn high_rtt_at_128_hz_keeps_its_baselines() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10_053, fast_setup_128);
    hub.down.delay_ticks = 41;
    hub.up.delay_ticks = 41;
    hub.down.loss = 0.01;
    hub.up.loss = 0.01;
    let a = hub.add_client();
    let m = run_beaters(&mut hub, a, 8, 1_500, 500);
    assert_eq!(
        m.expired, 0,
        "no baseline expires on an 84-tick round trip: {m:?}"
    );
    assert!(m.bytes_per_entity_tick <= 16.0, "records are deltas: {m:?}");
}

/// The memory side: a client keeps the versions the server can still name, not a fixed count. On
/// a 6-tick round trip that is the round trip plus the reorder margin (8 to 16 ticks) and a few
/// ticks of slack, under half of the 64 the base stored for every entity that changes each tick.
#[test]
fn a_short_link_stores_only_the_versions_in_flight() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10_054, fast_setup);
    hub.down.delay_ticks = 2;
    hub.up.delay_ticks = 2;
    let a = hub.add_client();
    let m = run_beaters(&mut hub, a, 8, 600, 300);
    assert!(
        m.versions_per_entity <= 6 + 16 + 6,
        "a 6-tick round trip stores at most 28 versions per entity: {m:?}"
    );
    assert_eq!(m.expired, 0, "{m:?}");
    assert_eq!(m.baseline_missing, 0, "{m:?}");
    assert!(m.staleness_p99 <= 1, "{m:?}");
}

/// When the acknowledgments stop, the baseline stays where it was and the ring fills: it stops at
/// its cap, the server falls back to full records, and the copies keep following the server.
#[test]
fn the_ring_stops_at_its_cap_when_acknowledgments_stop() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 10_055, fast_setup);
    hub.down.delay_ticks = 2;
    hub.up.delay_ticks = 2;
    let a = hub.add_client();
    let ents = spawn_beaters(&mut hub, 4);
    // From step 200 every state acknowledgment is lost (the client's `ACK` frames).
    hub.up_drop = Some(Box::new(|step, _, d| step >= 200 && tag(d) == TAG_ACK));
    let peer = hub.clients[a].peer;
    for t in 1..=1_400u32 {
        beat_all(&mut hub, &ents, t);
        hub.step();
        let (entities, stored) = client(&hub, a).state.stored();
        assert!(
            stored <= entities * regia_net::replication::STATE_VERSION_RING as usize,
            "tick {t}: {stored} versions of {entities} entities"
        );
        if t > 300 {
            for shown in shown_beats(&hub, a, &ents) {
                let shown = shown.expect("a copy");
                assert!(t - shown <= 3 + 30, "tick {t}: the copy shows beat {shown}");
            }
        }
    }
    let link = server(&hub).state_link(peer).expect("link");
    assert!(
        link.counters.baselines_expired > 0,
        "without acknowledgments the baselines expire by the ring"
    );
}

/// A frame of the given tick is held back `late` ticks; returns the client's counters and what
/// the copies show at the end, with the server's resync loops.
fn run_with_a_late_frame(seed: u64, late: u64) -> (u64, Vec<Option<u32>>, u64, u64, u64) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, fast_setup);
    hub.down.delay_ticks = 2;
    hub.up.delay_ticks = 2;
    let a = hub.add_client();
    let ents = spawn_beaters(&mut hub, 6);
    hub.down_filter = Some(Box::new(move |d, _| {
        if tag(d) == TAG_STATE && !d.channel.is_reliable() && frame_tick(d) == 300 {
            FrameFate::Delay(late)
        } else {
            FrameFate::Deliver
        }
    }));
    for t in 1..=500u32 {
        beat_all(&mut hub, &ents, t);
        hub.step();
    }
    let peer = hub.clients[a].peer;
    let expired = server(&hub)
        .state_link(peer)
        .expect("link")
        .counters
        .baselines_expired;
    (
        client(&hub, a).state.counters.baseline_missing,
        shown_beats(&hub, a, &ents),
        server(&hub).counters.resync_loops,
        expired,
        client(&hub, a).state.retired_baselines(),
    )
}

/// A frame reordered by less than the margin (8 ticks) decodes against the baseline it names:
/// the client has not let it go. One reordered past the margin names a version let go since and
/// is taken as lost. Neither asks the server for a full state, and the copies follow the server
/// either way.
#[test]
fn a_late_frame_never_costs_a_resync() {
    for (seed, late) in [(10_056, 3), (10_057, 7), (10_058, 40), (10_059, 120)] {
        let (missing, shown, loops, expired, retired) = run_with_a_late_frame(seed, late);
        assert_eq!(missing, 0, "a frame {late} ticks late asks for no resync");
        assert_eq!(loops, 0, "late {late}");
        assert_eq!(expired, 0, "late {late}");
        if late < u64::from(regia_net::replication::STATE_BASE_REORDER_TICKS) {
            assert_eq!(
                retired, 0,
                "inside the margin ({late} ticks) every baseline is held"
            );
        } else {
            // Six entities change every tick, so the late frame carried six deltas.
            assert_eq!(
                retired, 6,
                "past the margin ({late} ticks) the frame counts as lost"
            );
        }
        for s in shown {
            let s = s.expect("a copy");
            assert!(500 - s <= 4, "late {late}: the copy shows beat {s} at tick 500");
        }
    }
}
