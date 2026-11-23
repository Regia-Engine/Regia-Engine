// SPDX-License-Identifier: MPL-2.0
//! The client's version ring on spec 120's `high-rtt` and `rtt-extreme` profiles (ADR-10051,
//! KI-NT-009): `regia_net`'s replication through the real transport and the profiles' links, in
//! virtual time at 60 Hz. Every entity changes every tick, the worst case for the ring: its
//! acknowledged baseline is one round trip of versions behind.
//!
//! Each run prints one `state-ring` line per client (`--nocapture` shows them): staleness, the
//! server's bytes to the client, baselines the server expired and the versions the client
//! stores. The 32-client runs are measurements for the lane report and are ignored by default.
// The time harness runs ECS worlds (feature `ecs`, on by default).
#![cfg(feature = "ecs")]

use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::bridge::NetFrame;
use regia_net::codec::CodecContext;
use regia_net::replication::{ClientReplication, ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::NetNode;
use regia_net::*;
use regia_net_soak::profiles::Profile;
use regia_net_soak::timed::{TICK_US, TimedHub, TimedOptions, path_rtt_ms};

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.state_ring";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Entities that change every tick.
const ENTITIES: usize = 24;
/// Bytes of the part of an entity that never changes: what a full record costs over a delta.
const KIT_BYTES: usize = 96;
/// Virtual time the clients get to connect (the extreme peer's handshake crosses a 3 s round trip
/// with 20 % loss), then to settle, then the measured window.
const CONNECT_MS: u64 = 60_000;
const SETTLE_MS: u64 = 20_000;
const MEASURE_MS: u64 = 60_000;

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

/// `regia_net`'s default settings (the state channel, 49,152 B/s per client) with the budget
/// pipeline on and every entity relevant everywhere.
fn setup(world: &mut World) {
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

/// Server: every entity's beat is the tick being simulated.
fn beat(node: Res<NetNode>, mut q: Query<&mut Beat>) {
    for mut b in &mut q {
        b.0 = node.tick.0;
    }
}

/// What one client measured over the window.
#[derive(Debug, Clone)]
struct ClientRow {
    client: usize,
    /// The path's round trip, milliseconds (latencies and mean jitter).
    rtt_ms: f64,
    /// The 99th percentile of how many ticks a copy's beat is behind the server's tick.
    staleness_p99: u32,
    /// The same beyond the path's one-way delay (the spec 120 § 15.2 sense).
    beyond_path_p99: u32,
    /// The server transport's bytes to this client, kbit/s over the window.
    down_kbps: f64,
    /// Baselines the server expired in the window.
    expired: u64,
    /// The most versions the client stored per entity in the window.
    versions_per_entity: usize,
    /// Deltas the client could not decode for a missing baseline, whole run.
    baseline_missing: u64,
}

fn p99(sorted: &[u32]) -> u32 {
    sorted
        .get((sorted.len() * 99 / 100).min(sorted.len().saturating_sub(1)))
        .copied()
        .unwrap_or(u32::MAX)
}

/// Runs `clients` clients of `profile` with [`ENTITIES`] entities that change every tick.
fn run(profile: &str, clients: usize, seed: u64) -> Vec<ClientRow> {
    let total_ms = CONNECT_MS + SETTLE_MS + MEASURE_MS;
    let prof = Profile::named(profile).unwrap_or_else(|| panic!("profile {profile}"));
    let options = TimedOptions {
        seed,
        ..TimedOptions::default()
    };
    let server_link = options.server_link.clone();
    let mut hub = TimedHub::with_max_peers::<Game>(options, setup, clients.max(16)).expect("server");
    hub.server().gameplay.add_systems(beat);
    let links: Vec<_> = (0..clients).map(|c| prof.client_link(c, total_ms)).collect();
    for link in &links {
        hub.add_client::<Game>(link.clone()).expect("client");
    }
    assert!(
        hub.run_until(CONNECT_MS, |h| (0..clients).all(|c| h.client_peer(c).is_some())),
        "{profile}/{clients}: every client connects within {CONNECT_MS} ms"
    );
    for i in 0..ENTITIES {
        hub.server().world.spawn((
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
            Kit(vec![i as u8; KIT_BYTES]),
            Beat(0),
        ));
    }
    hub.run_ms(SETTLE_MS);
    let peers: Vec<PeerId> = (0..clients).map(|c| hub.client_peer(c).expect("peer")).collect();
    let expired_of = |hub: &TimedHub, c: usize| {
        let link = hub
            .server_world()
            .resource::<ServerReplication>()
            .state_link(peers[c])
            .expect("a state link");
        assert_eq!(link.wire, ConnWire::Acked, "client {c} is on the state channel");
        link.counters.baselines_expired
    };
    let bytes_of = |hub: &TimedHub, c: usize| hub.server_link_stats(c).map_or(0, |s| s.bytes_sent);
    let expired0: Vec<u64> = (0..clients).map(|c| expired_of(&hub, c)).collect();
    let bytes0: Vec<u64> = (0..clients).map(|c| bytes_of(&hub, c)).collect();
    let mut stale: Vec<Vec<u32>> = vec![Vec::new(); clients];
    let mut versions = vec![0usize; clients];
    let end_us = hub.now_us() + MEASURE_MS * 1000;
    let mut seen = hub.server_updates();
    while hub.now_us() < end_us {
        hub.step();
        if hub.server_updates() == seen {
            continue;
        }
        seen = hub.server_updates();
        let now = hub.server_tick();
        for c in 0..clients {
            let w = &mut hub.client(c).world;
            let mut q = w.query::<&Beat>();
            let mut copies = 0;
            for b in q.iter(w) {
                copies += 1;
                stale[c].push(now.saturating_sub(b.0));
            }
            assert_eq!(
                copies, ENTITIES,
                "{profile}/{clients}: client {c} holds every copy"
            );
            let (entities, stored) = w.resource::<ClientReplication>().state.stored();
            versions[c] = versions[c].max(stored.div_ceil(entities.max(1)));
        }
    }
    let now_ms = hub.now_us() / 1000;
    (0..clients)
        .map(|c| {
            stale[c].sort_unstable();
            let rtt_ms = path_rtt_ms(&links[c], &server_link, now_ms);
            let one_way_ticks = (rtt_ms * 500.0 / TICK_US as f64).ceil() as u32;
            let raw = p99(&stale[c]);
            let row = ClientRow {
                client: c,
                rtt_ms,
                staleness_p99: raw,
                beyond_path_p99: raw.saturating_sub(one_way_ticks),
                down_kbps: (bytes_of(&hub, c) - bytes0[c]) as f64 * 8.0 / MEASURE_MS as f64,
                expired: expired_of(&hub, c) - expired0[c],
                versions_per_entity: versions[c],
                baseline_missing: hub
                    .client_world(c)
                    .resource::<ClientReplication>()
                    .state
                    .counters
                    .baseline_missing,
            };
            println!(
                "state-ring {profile}/{clients} client {c}: rtt {:.0} ms, staleness p99 {} ticks ({} beyond the path), \
                 down {:.1} kbit/s, baselines expired {}, versions per entity {}, baselines missing {}",
                row.rtt_ms,
                row.staleness_p99,
                row.beyond_path_p99,
                row.down_kbps,
                row.expired,
                row.versions_per_entity,
                row.baseline_missing
            );
            row
        })
        .collect()
}

/// One summary line per run for the report: the worst client's staleness, the mean and largest
/// bytes, the baselines expired in total and the longest ring.
fn summary(profile: &str, rows: &[ClientRow]) {
    let n = rows.len();
    println!(
        "state-ring-summary {profile}/{n}: staleness p99 max {} ticks ({} beyond the path), down mean {:.1} max {:.1} kbit/s, \
         baselines expired {}, versions per entity max {}",
        rows.iter().map(|r| r.staleness_p99).max().unwrap_or(0),
        rows.iter().map(|r| r.beyond_path_p99).max().unwrap_or(0),
        rows.iter().map(|r| r.down_kbps).sum::<f64>() / n as f64,
        rows.iter().map(|r| r.down_kbps).fold(0.0, f64::max),
        rows.iter().map(|r| r.expired).sum::<u64>(),
        rows.iter().map(|r| r.versions_per_entity).max().unwrap_or(0),
    );
}

/// No baseline expires by the ring, and no client is asked to decode against a version it lacks.
fn assert_ring_holds(profile: &str, rows: &[ClientRow]) {
    for r in rows {
        assert_eq!(
            r.expired, 0,
            "{profile}: client {} on a {:.0} ms round trip keeps its baselines: {r:?}",
            r.client, r.rtt_ms
        );
        assert_eq!(r.baseline_missing, 0, "{profile}: {r:?}");
        assert!(
            r.versions_per_entity <= regia_net::replication::STATE_VERSION_RING as usize,
            "{profile}: {r:?}"
        );
    }
}

/// `rtt-extreme` at 8 clients: the peer on 1,500 ms one way with 20 % loss (a 180-tick round
/// trip) keeps its baselines. The base's 64-version ring expired them on every acknowledgment
/// and sent the full state of every entity each time.
#[test]
fn rtt_extreme_at_8_keeps_its_baselines() {
    let rows = run("rtt-extreme", 8, 0x0A51_0001);
    summary("rtt-extreme", &rows);
    assert!(
        rows.iter().any(|r| r.rtt_ms >= 3_000.0),
        "one client in eight is the extreme peer"
    );
    assert_ring_holds("rtt-extreme", &rows);
}

/// `high-rtt` at 8 clients (450 and 650 ms round trips, 20 ms jitter, 1 % loss): 41 ticks at
/// 60 Hz, inside the old ring too; the run pins that the change costs it nothing.
#[test]
fn high_rtt_at_8_keeps_its_baselines() {
    let rows = run("high-rtt", 8, 0x0A51_0002);
    summary("high-rtt", &rows);
    assert_ring_holds("high-rtt", &rows);
    for r in &rows {
        // One round trip of versions (41 ticks at 650 ms and 40 ms of jitter) plus the margin.
        assert!(r.versions_per_entity <= 41 + 16 + 12, "{r:?}");
    }
}

/// The 32-client measurement of `rtt-extreme` (four extreme peers).
#[test]
#[ignore = "measurement for the lane report: 32 clients for 140 s of virtual time"]
fn rtt_extreme_at_32_reports_its_numbers() {
    let rows = run("rtt-extreme", 32, 0x0A51_0003);
    summary("rtt-extreme", &rows);
    assert_ring_holds("rtt-extreme", &rows);
}

/// The 32-client measurement of `high-rtt`.
#[test]
#[ignore = "measurement for the lane report: 32 clients for 140 s of virtual time"]
fn high_rtt_at_32_reports_its_numbers() {
    let rows = run("high-rtt", 32, 0x0A51_0004);
    summary("high-rtt", &rows);
    assert_ring_holds("high-rtt", &rows);
}
