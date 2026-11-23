// SPDX-License-Identifier: MPL-2.0
//! Tests of the external replication (spec 120 § 8.6, ADR-8186). They are lib unit tests on
//! purpose: `cargo test -p regia_net --no-default-features --lib external_replication` runs them
//! with no ECS crate in the build (the crate's dev-dependencies hold none either).

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

use glam::{Quat, Vec3};

use super::*;
use crate::conditioner::{ConditionerConfig, NetConditioner};
use crate::snapshot_delta::{FieldDef, FieldKind, SnapshotDecoder};

const POS: usize = 0;
const STATE: usize = 2;

fn schema() -> SnapshotSchema {
    SnapshotSchema::new(vec![
        FieldDef::new(
            "pos",
            FieldKind::Vec3Quantised {
                min: Vec3::splat(-4096.0),
                max: Vec3::splat(4096.0),
                bits: 21,
            },
        ),
        FieldDef::new(
            "heading",
            FieldKind::Float {
                min: -std::f32::consts::PI,
                max: std::f32::consts::PI,
                bits: 12,
            },
        ),
        FieldDef::new("state", FieldKind::U8),
    ])
    .expect("schema")
}

/// The bad-wifi profile of the net-soak harness (spec 120 § 15.1): 8 ms + 0-30 ms jitter, 1 %
/// loss, 1 % reorder up to 30 ms, 0.5 % duplicates; one conditioner per direction per client.
fn bad_wifi(seed: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss: 0.01,
        duplicate: 0.005,
        reorder: 0.01,
        reorder_max_ms: 30,
        delay_ms: 8,
        jitter_ms: 30,
        ..Default::default()
    }
}

/// A deterministic xorshift64* stream.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A game sim outside the ECS: entities orbiting their own centers.
struct Sim {
    /// key -> (center, orbit radius, phase, state)
    entities: BTreeMap<u32, (Vec3, f32, f32, u8)>,
    time_s: f32,
    /// A key the game ranks above everything else (the priority-function test).
    hot: Option<u32>,
}

impl Sim {
    fn new(seed: u64, count: u32, half_extent: f32) -> Self {
        let mut rng = Rng::new(seed);
        let mut entities = BTreeMap::new();
        for key in 0..count {
            let center = Vec3::new(
                (rng.unit() * 2.0 - 1.0) * half_extent,
                0.0,
                (rng.unit() * 2.0 - 1.0) * half_extent,
            );
            entities.insert(
                key,
                (center, 2.0 + (key % 7) as f32, rng.unit() * 6.0, (key % 5) as u8),
            );
        }
        Self {
            entities,
            time_s: 0.0,
            hot: None,
        }
    }

    fn position(&self, key: u32) -> Vec3 {
        let Some((center, r, phase, _)) = self.entities.get(&key) else {
            return Vec3::ZERO;
        };
        let a = phase + self.time_s * 0.1;
        *center + Vec3::new(a.cos() * r, 0.0, a.sin() * r)
    }

    fn heading(&self, key: u32) -> f32 {
        let phase = self.entities.get(&key).map_or(0.0, |e| e.2);
        let a = phase + self.time_s * 0.1;
        (a.sin()).atan2(a.cos())
    }

    fn push_all(&self, rep: &mut ExternalReplication<u32>) {
        for key in self.entities.keys() {
            rep.upsert(*key, self.position(*key), 0).expect("upsert");
        }
    }
}

impl ExternalSource<u32> for Sim {
    fn priority(&self, key: u32, _client: PeerId, _distance: Option<f32>) -> f32 {
        if self.hot == Some(key) { 1000.0 } else { 1.0 }
    }

    fn record(&self, key: u32, out: &mut Vec<FieldValue>) {
        let state = self.entities.get(&key).map_or(0, |e| e.3);
        out.push(FieldValue::Vec3(self.position(key)));
        out.push(FieldValue::F32(self.heading(key)));
        out.push(FieldValue::U8(state));
    }
}

fn now_ms(tick: u32) -> u64 {
    u64::from(tick) * 50 / 3
}

fn addr() -> SocketAddr {
    "127.0.0.1:9".parse().expect("addr")
}

/// The DO27 shape of spec 120 § 8.6 (ADR-8186 acceptance): 2,000 entities of a sim outside the ECS,
/// 16 clients, every link on bad-wifi both ways (seeded, virtual time). Checks: every packet within
/// the client's byte budget and the sum within `sends × rate / send_hz` (the carry never adds
/// bytes); far entities at their tier rate (2 Hz: at most one record per 30 ticks), mid ones every
/// third send, near ones nearly every send; after the sim stops, every client converges to exactly
/// the entities relevant to it, at the server's quantized state, interpolated and decoded.
#[test]
fn two_thousand_entities_sixteen_clients_on_bad_wifi_converge_within_budget_and_tiers() {
    const CLIENTS: u32 = 16;
    const MOVE_UNTIL: u32 = 960;
    const END: u32 = 1200;
    const WINDOW: (u32, u32) = (360, 960);
    let config = ExternalConfig {
        tick_hz: 60.0,
        send_every_ticks: 2,
        rate: 32_000,
        max_packet_bytes: 1200,
        cell_m: 100.0,
        baseline_capacity: 64,
        classes: vec![ExternalClass {
            relevance: RelevanceShape::Spatial {
                radius: 800.0,
                hysteresis: 0.1,
            },
            priority: 1.0,
            rate_tiers: Some(RateTiers::default()),
            max_interval_ticks: None,
        }],
    };
    let send_hz = 30.0;
    let per_send = f64::from(config.rate) / send_hz;
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config.clone()).expect("new");
    let mut sim = Sim::new(5, 2000, 2000.0);
    sim.push_all(&mut rep);
    let mut clients = Vec::new();
    for c in 0..CLIENTS {
        let peer = PeerId(c + 1);
        rep.add_client(peer).expect("client");
        let vp = Vec3::new(
            -1500.0 + 1000.0 * (c % 4) as f32,
            0.0,
            -1500.0 + 1000.0 * (c / 4) as f32,
        );
        rep.set_viewpoints(peer, &[vp]).expect("viewpoint");
        clients.push((
            peer,
            NetConditioner::new(bad_wifi(100 + u64::from(c))),
            NetConditioner::new(bad_wifi(200 + u64::from(c))),
            ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default()),
            // A tap fed the same delivered packets: which records the client applied.
            SnapshotDecoder::new(schema()),
        ));
    }
    let mut sent_bytes: HashMap<PeerId, (u64, u32)> = HashMap::new();
    let mut max_packet = 0usize;
    // (peer, key) -> (always in the tier during the window, records in the window, sends held)
    let mut window: HashMap<(u32, u32), ([bool; 3], u32, u32)> = HashMap::new();
    let mut packets = Vec::new();
    let mut inbox: HashMap<PeerId, Vec<Vec<u8>>> = HashMap::new();
    let mut next_key = 10_000u32;
    for tick in 1..=END {
        if tick <= MOVE_UNTIL {
            sim.time_s = tick as f32 / 60.0;
            // Churn: a despawn and a spawn every 30 ticks.
            if tick.is_multiple_of(30) {
                let gone = (tick / 30) * 37 % 2000;
                if sim.entities.remove(&gone).is_some() {
                    rep.remove(gone);
                }
                let mut rng = Rng::new(u64::from(tick));
                let center = Vec3::new(rng.unit() * 3000.0 - 1500.0, 0.0, rng.unit() * 3000.0 - 1500.0);
                sim.entities.insert(next_key, (center, 3.0, 0.0, 1));
                next_key += 1;
            }
            sim.push_all(&mut rep);
        }
        packets.clear();
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
        let in_window = (WINDOW.0..WINDOW.1).contains(&tick);
        if tick.is_multiple_of(2) {
            for (peer, ..) in &clients {
                sent_bytes.entry(*peer).or_default().1 += 1;
                if in_window {
                    for key in rep.relevant(*peer) {
                        let tier = rep.tier_of(*peer, key).expect("held");
                        let e = window.entry((peer.0, key)).or_insert(([true; 3], 0, 0));
                        e.2 += 1;
                        for (t, always) in e.0.iter_mut().enumerate() {
                            *always &= t == tier as usize;
                        }
                    }
                }
            }
        }
        for packet in packets.drain(..) {
            let Some((_, down, ..)) = clients.iter_mut().find(|c| c.0 == packet.peer) else {
                panic!("packet for an unknown client");
            };
            let s = sent_bytes.entry(packet.peer).or_default();
            s.0 += packet.bytes.len() as u64;
            max_packet = max_packet.max(packet.bytes.len());
            // What the link passes at once arrives this tick; what it holds arrives on its release tick.
            let passed = down.process(packet.bytes, addr(), now_ms(tick));
            inbox
                .entry(packet.peer)
                .or_default()
                .extend(passed.into_iter().map(|(b, _)| b));
        }
        // Deliver what each link releases now, and the acks back.
        for (peer, down, up, interp, tap) in &mut clients {
            let mut arrived = inbox.remove(peer).unwrap_or_default();
            arrived.extend(down.release_due(now_ms(tick)).into_iter().map(|(b, _)| b));
            for bytes in arrived {
                interp.receive(&bytes).expect("a delivered packet decodes");
                let applied = tap
                    .decode(&bytes)
                    .expect("the tap decodes what the client decodes");
                if in_window {
                    // Updates applied (a late packet applies none). Spawns are not counted: a
                    // spawn is never held back by a tier.
                    for id in &applied.updated {
                        if let Some(e) = window.get_mut(&(peer.0, id.0)) {
                            e.1 += 1;
                        }
                    }
                }
            }
            interp.advance(1.0);
            if let Some(ack) = interp.ack() {
                let mut acks = up.process(ack.to_bytes().to_vec(), addr(), now_ms(tick));
                acks.extend(up.release_due(now_ms(tick)));
                for (bytes, _) in acks {
                    let ack = ExternalAck::from_bytes(&bytes).expect("16 bytes");
                    rep.ack(*peer, ack).expect("ack");
                }
            }
        }
    }

    // Budget held per client: the sum never exceeds sends × per-send rate, a packet never the cap.
    assert!(
        max_packet <= config.max_packet_bytes,
        "a packet of {max_packet} bytes"
    );
    for (peer, (bytes, sends)) in &sent_bytes {
        let allowed = f64::from(*sends) * per_send;
        assert!(
            *bytes as f64 <= allowed,
            "{peer:?} sent {bytes} bytes in {sends} sends, budget {allowed:.0}"
        );
        let c = rep.counters(*peer).expect("counters");
        assert!(
            c.omitted > 0,
            "{peer:?}: the budget never bound (the fill should be budget-limited)"
        );
        assert!(c.tier_held > 0, "{peer:?}: the tiers never held an update back");
    }

    // Tier rates over the 600-tick window (300 sends): far <= one record per 30 ticks (2 Hz),
    // mid <= one per 6 ticks (every third send), near nearly every send.
    let sends_in_window = (WINDOW.1 - WINDOW.0) / 2;
    let mut per_tier: [(u32, u64); 3] = [(0, 0); 3];
    for ((peer, key), (always, count, held)) in &window {
        for (t, a) in always.iter().enumerate() {
            // Only entities the client held, in this one tier, at every send of the window.
            if !*a || *held != sends_in_window {
                continue;
            }
            let bound = match t {
                0 => sends_in_window + 1,
                1 => sends_in_window / 3 + 1,
                _ => (WINDOW.1 - WINDOW.0) / 30 + 1,
            };
            assert!(
                *count <= bound,
                "client {peer} key {key} in tier {t} the whole window: {count} records, bound {bound}"
            );
            per_tier[t].0 += 1;
            per_tier[t].1 += u64::from(*count);
        }
    }
    let mean = |t: usize| per_tier[t].1 as f64 / f64::from(per_tier[t].0.max(1));
    assert!(per_tier[2].0 > 100, "far entities observed: {}", per_tier[2].0);
    assert!(
        per_tier[1].0 > 100 && per_tier[0].0 > 10,
        "tiers observed: {per_tier:?}"
    );
    let far_expected = f64::from((WINDOW.1 - WINDOW.0) / 30);
    assert!(
        mean(2) >= 0.9 * far_expected,
        "far entities averaged {:.1} records in the window, their tier rate gives {far_expected}",
        mean(2)
    );
    assert!(
        mean(1) >= 0.9 * f64::from(sends_in_window / 3),
        "mid mean {:.1}",
        mean(1)
    );
    assert!(
        mean(0) >= 0.9 * f64::from(sends_in_window),
        "near mean {:.1}",
        mean(0)
    );

    // Convergence: every client holds exactly its relevant set at the server's quantized state.
    let schema = schema();
    for (peer, _, _, interp, _) in &clients {
        let keys = rep.relevant(*peer);
        assert!(keys.len() > 50, "{peer:?} holds {} entities", keys.len());
        let mut expected = WorldSnapshot::new(
            keys.iter()
                .map(|k| {
                    let mut f = Vec::new();
                    sim.record(*k, &mut f);
                    EntitySnapshot::new(k.net_id(), f)
                })
                .collect(),
        );
        expected = schema.quantize_snapshot(&expected).expect("quantize");
        expected.entities.sort_by_key(|e| e.id);
        let held: Vec<u32> = interp.keys().collect();
        assert_eq!(
            held, keys,
            "{peer:?}: the client's entity set differs from its relevant set"
        );
        let mut latest = Vec::new();
        for e in &expected.entities {
            let key = u32::from_net_id(e.id).expect("key");
            assert!(
                interp.latest(key, &mut latest).is_some(),
                "{peer:?}: no state for {key}"
            );
            assert_eq!(
                latest,
                e.fields,
                "{peer:?}: entity {key} differs from the server's (tier {:?})",
                rep.tier_of(*peer, key)
            );
        }
        let mut out = Vec::new();
        for e in &expected.entities {
            let key = u32::from_net_id(e.id).expect("key");
            assert!(interp.sample(key, &mut out), "{peer:?}: no track for {key}");
            assert_eq!(out, e.fields, "{peer:?}: interpolated {key} has not settled");
        }
    }
    // After the sim stopped, nothing is re-sent: the last second carries no updates.
    for (peer, ..) in &clients {
        let c = rep.counters(*peer).expect("counters");
        assert!(
            c.last_bytes < 40,
            "{peer:?}: a still world still costs {} bytes per packet",
            c.last_bytes
        );
    }
}

fn one_class(radius: f32, hysteresis: f32) -> ExternalConfig {
    ExternalConfig {
        classes: vec![ExternalClass {
            relevance: RelevanceShape::Spatial { radius, hysteresis },
            ..ExternalClass::default()
        }],
        ..ExternalConfig::default()
    }
}

/// Runs `ticks` lossless ticks from `from`, acking every packet at once.
fn run_clean(
    rep: &mut ExternalReplication<u32>,
    sim: &Sim,
    clients: &mut [(PeerId, ExternalInterpolator<u32>)],
    from: u32,
    ticks: u32,
) {
    let mut packets = Vec::new();
    for tick in from..from + ticks {
        packets.clear();
        rep.send_tick(NetTick(tick), sim, &mut packets).expect("send");
        for p in &packets {
            if let Some((_, interp)) = clients.iter_mut().find(|c| c.0 == p.peer) {
                interp.receive(&p.bytes).expect("decode");
                interp.advance(1.0);
                if let Some(ack) = interp.ack() {
                    rep.ack(p.peer, ack).expect("ack");
                }
            }
        }
    }
}

/// The relevance rule is the ECS path's: enter at the radius, leave beyond `radius × (1 + h)`;
/// a client that loses an entity gets a remove record.
#[test]
fn hysteresis_keeps_an_entity_until_the_widened_radius() {
    let mut rep: ExternalReplication<u32> =
        ExternalReplication::new(schema(), one_class(100.0, 0.2)).expect("new");
    let mut sim = Sim::new(1, 0, 0.0);
    sim.entities.insert(7, (Vec3::new(150.0, 0.0, 0.0), 0.0, 0.0, 0));
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut clients = vec![(
        peer,
        ExternalInterpolator::new(schema(), InterpolatorConfig::default()),
    )];
    let at = |sim: &mut Sim, rep: &mut ExternalReplication<u32>, x: f32| {
        sim.entities.insert(7, (Vec3::new(x, 0.0, 0.0), 0.0, 0.0, 0));
        rep.upsert(7, Vec3::new(x, 0.0, 0.0), 0).expect("upsert");
    };
    at(&mut sim, &mut rep, 150.0);
    run_clean(&mut rep, &sim, &mut clients, 1, 3);
    assert!(rep.relevant(peer).is_empty(), "outside the radius");
    at(&mut sim, &mut rep, 99.0);
    run_clean(&mut rep, &sim, &mut clients, 4, 3);
    assert_eq!(rep.relevant(peer), vec![7]);
    assert_eq!(clients[0].1.keys().collect::<Vec<_>>(), vec![7]);
    at(&mut sim, &mut rep, 115.0);
    run_clean(&mut rep, &sim, &mut clients, 7, 3);
    assert_eq!(rep.relevant(peer), vec![7], "held inside radius × 1.2");
    at(&mut sim, &mut rep, 121.0);
    run_clean(&mut rep, &sim, &mut clients, 10, 3);
    assert!(rep.relevant(peer).is_empty(), "dropped beyond radius × 1.2");
    assert_eq!(clients[0].1.keys().count(), 0, "the client got the remove");
    assert!(clients[0].1.current().is_some_and(|w| w.is_empty()));
}

/// The budget is `min(project rate, declared rate, allowance)`, floored at `MIN_CLIENT_RATE`.
#[test]
fn the_allowance_and_the_declared_rate_lower_the_budget() {
    let mut config = one_class(5000.0, 0.1);
    config.rate = 20_000;
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config).expect("new");
    // About 55 KB of spawn records: more than any of the four budgets moves in 2 s.
    let sim = Sim::new(3, 5000, 1000.0);
    sim.push_all(&mut rep);
    for peer in [PeerId(1), PeerId(2), PeerId(3), PeerId(4)] {
        rep.add_client(peer).expect("client");
        rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    }
    rep.set_declared_rate(PeerId(2), Some(9_000)).expect("rate");
    rep.set_allowance(PeerId(3), Some(6_000)).expect("allowance");
    rep.set_allowance(PeerId(4), Some(10)).expect("allowance");
    let mut clients: Vec<_> = (1..=4)
        .map(|p| {
            (
                PeerId(p),
                ExternalInterpolator::new(schema(), InterpolatorConfig::default()),
            )
        })
        .collect();
    run_clean(&mut rep, &sim, &mut clients, 1, 120);
    let bytes = |p: u32| rep.counters(PeerId(p)).expect("counters").bytes as f64;
    // 120 ticks at 60 Hz = 2 s: each client moves at most 2 s of its rate, and at least 75 % of it.
    for (p, rate) in [
        (1, 20_000.0),
        (2, 9_000.0),
        (3, 6_000.0),
        (4, f64::from(MIN_CLIENT_RATE)),
    ] {
        assert!(
            bytes(p) <= 2.0 * rate && bytes(p) > 1.5 * rate,
            "client {p}: {} bytes in 2 s at {rate} B/s",
            bytes(p)
        );
    }
    let stats = rep.stats();
    let rates: Vec<u32> = stats.clients.iter().map(|c| c.rate).collect();
    assert_eq!(rates, vec![20_000, 9_000, 6_000, MIN_CLIENT_RATE]);
}

/// Under a tight budget the game's priority function decides what goes first.
#[test]
fn the_game_priority_orders_packing_under_a_tight_budget() {
    let mut config = one_class(5000.0, 0.1);
    config.rate = MIN_CLIENT_RATE;
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config).expect("new");
    let mut sim = Sim::new(9, 1000, 500.0);
    sim.hot = Some(777);
    sim.push_all(&mut rep);
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut interp = ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default());
    let mut packets = Vec::new();
    rep.send_tick(NetTick(1), &sim, &mut packets).expect("send");
    assert_eq!(packets.len(), 1);
    let got = interp.receive(&packets[0].bytes).expect("decode");
    assert!(
        got.added > 0 && got.added < 1000,
        "budget-limited first packet: {got:?}"
    );
    assert!(
        interp.keys().any(|k| k == 777),
        "the game's hot entity went in the first packet"
    );
}

/// A removed key reaches the client as a remove; its slot is reused by a new key without
/// inheriting any per-client state.
#[test]
fn a_removed_entity_is_removed_on_the_client_and_its_slot_reused_cleanly() {
    let mut rep: ExternalReplication<u32> =
        ExternalReplication::new(schema(), one_class(500.0, 0.1)).expect("new");
    let mut sim = Sim::new(4, 10, 50.0);
    sim.push_all(&mut rep);
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut clients = vec![(
        peer,
        ExternalInterpolator::new(schema(), InterpolatorConfig::default()),
    )];
    run_clean(&mut rep, &sim, &mut clients, 1, 4);
    assert_eq!(clients[0].1.keys().count(), 10);
    assert!(rep.remove(3));
    assert!(!rep.remove(3), "already gone");
    sim.entities.remove(&3);
    sim.entities
        .insert(500, (Vec3::new(10.0, 0.0, 10.0), 1.0, 0.0, 4));
    sim.push_all(&mut rep);
    assert_eq!(rep.entity_count(), 10);
    run_clean(&mut rep, &sim, &mut clients, 5, 4);
    let keys: Vec<u32> = clients[0].1.keys().collect();
    assert!(!keys.contains(&3) && keys.contains(&500), "{keys:?}");
    let mut out = Vec::new();
    assert!(clients[0].1.sample(500, &mut out));
    assert_eq!(out[STATE], FieldValue::U8(4));
}

/// A world-relevant class reaches a client that has no viewpoint; a spatial one does not.
#[test]
fn world_relevant_entities_reach_a_client_without_a_viewpoint() {
    let config = ExternalConfig {
        classes: vec![
            ExternalClass::default(),
            ExternalClass {
                relevance: RelevanceShape::World,
                ..ExternalClass::default()
            },
        ],
        ..ExternalConfig::default()
    };
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config).expect("new");
    let sim = Sim::new(2, 4, 10.0);
    rep.upsert(0, sim.position(0), 0).expect("spatial");
    rep.upsert(1, sim.position(1), 1).expect("world");
    assert_eq!(
        rep.upsert(2, Vec3::ZERO, 2),
        Err(ExternalError::UnknownClass { class: 2, count: 2 })
    );
    assert_eq!(
        rep.upsert(2, Vec3::NAN, 0).map_err(|e| e.code()),
        Err("bad_position")
    );
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    let mut clients = vec![(
        peer,
        ExternalInterpolator::new(schema(), InterpolatorConfig::default()),
    )];
    run_clean(&mut rep, &sim, &mut clients, 1, 2);
    assert_eq!(rep.relevant(peer), vec![1]);
    // Moving it to the spatial class makes it leave.
    rep.upsert(1, sim.position(1), 0).expect("reclass");
    run_clean(&mut rep, &sim, &mut clients, 3, 2);
    assert!(rep.relevant(peer).is_empty());
    assert_eq!(
        rep.stats().band_radii_m,
        vec![300.0],
        "the spatial class's band in the shared grid"
    );
}

/// A forced refresh (`max_interval_ticks`) overrides the rate tier: a far entity still gets a
/// record every `max_interval_ticks`.
#[test]
fn a_forced_refresh_overrides_the_tier() {
    let config = ExternalConfig {
        classes: vec![ExternalClass {
            relevance: RelevanceShape::Spatial {
                radius: 2000.0,
                hysteresis: 0.1,
            },
            rate_tiers: Some(RateTiers {
                far_hz: 0.5,
                ..RateTiers::default()
            }),
            max_interval_ticks: Some(20),
            ..ExternalClass::default()
        }],
        ..ExternalConfig::default()
    };
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config).expect("new");
    let mut sim = Sim::new(1, 0, 0.0);
    sim.entities.insert(1, (Vec3::new(1000.0, 0.0, 0.0), 5.0, 0.0, 0));
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut tap = SnapshotDecoder::new(schema());
    let mut interp = ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default());
    let mut packets = Vec::new();
    let mut records = 0;
    for tick in 1..=240u32 {
        sim.time_s = tick as f32 / 6.0;
        sim.push_all(&mut rep);
        packets.clear();
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
        for p in &packets {
            let d = tap.decode(&p.bytes).expect("decode");
            records += d.updated.len() + d.added.len();
            interp.receive(&p.bytes).expect("decode");
            if let Some(ack) = interp.ack() {
                rep.ack(peer, ack).expect("ack");
            }
        }
    }
    assert_eq!(rep.tier_of(peer, 1), Some(RateTier::Far));
    // 0.5 Hz alone gives 2 records in 4 s; the 20-tick refresh gives 12.
    assert!((11..=13).contains(&records), "{records} records in 240 ticks");
}

/// Old or unknown acks are ignored and counted; an unknown client is an error with a code.
#[test]
fn stale_and_unknown_acks_are_ignored() {
    let mut rep: ExternalReplication<u32> =
        ExternalReplication::new(schema(), one_class(500.0, 0.1)).expect("new");
    let sim = Sim::new(4, 10, 50.0);
    sim.push_all(&mut rep);
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    assert_eq!(
        rep.add_client(peer).map_err(|e| e.code()),
        Err("duplicate_client")
    );
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut packets = Vec::new();
    for tick in 1..=3 {
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
    }
    let ack = |s: u32| ExternalAck {
        baseline: Seq(s),
        applied: Seq(s),
        applied_bits: 0,
    };
    rep.ack(peer, ack(2)).expect("ack");
    rep.ack(peer, ack(1)).expect("older");
    rep.ack(peer, ack(999)).expect("never sent");
    let c = rep.counters(peer).expect("counters");
    assert_eq!((c.acks, c.stale_acks), (1, 2));
    assert_eq!(rep.stats().clients[0].ack, Some(2));
    assert_eq!(
        rep.ack(PeerId(9), ack(1)).map_err(|e| e.code()),
        Err("unknown_client")
    );
    let bytes = ack(7).to_bytes();
    assert_eq!(ExternalAck::from_bytes(&bytes), Some(ack(7)));
    assert_eq!(ExternalAck::from_bytes(&bytes[..15]), None);
    assert!(rep.remove_client(peer));
    assert_eq!(rep.clients().count(), 0);
}

/// Settings out of range are refused with `invalid_config`.
#[test]
fn bad_settings_are_refused() {
    for config in [
        ExternalConfig {
            send_every_ticks: 0,
            ..ExternalConfig::default()
        },
        ExternalConfig {
            rate: 10,
            ..ExternalConfig::default()
        },
        one_class(-1.0, 0.1),
        ExternalConfig {
            classes: vec![ExternalClass {
                rate_tiers: Some(RateTiers {
                    mid_every: 0,
                    ..RateTiers::default()
                }),
                ..ExternalClass::default()
            }],
            ..ExternalConfig::default()
        },
    ] {
        let err = ExternalReplication::<u32>::new(schema(), config).expect_err("refused");
        assert_eq!(err.code(), "invalid_config", "{err}");
    }
}

/// The interpolator blends floats, vectors and rotations between the two samples around
/// playback, steps the other fields, never moves playback backward and holds after the last
/// sample.
#[test]
fn the_interpolator_blends_between_samples_and_never_goes_back() {
    let rot_schema = SnapshotSchema::new(vec![
        FieldDef::new(
            "pos",
            FieldKind::Vec3Quantised {
                min: Vec3::splat(-100.0),
                max: Vec3::splat(100.0),
                bits: 24,
            },
        ),
        FieldDef::new("rot", FieldKind::QuatSmallestThree { bits: 14 }),
        FieldDef::new("state", FieldKind::U8),
    ])
    .expect("schema");
    let mut enc = SnapshotEncoder::new(rot_schema.clone());
    let mut interp = ExternalInterpolator::<u32>::new(
        rot_schema,
        InterpolatorConfig {
            delay_ticks: 10.0,
            ..InterpolatorConfig::default()
        },
    );
    let world = |x: f32, angle: f32, state: u8| {
        WorldSnapshot::new(vec![EntitySnapshot::new(
            NetId(5),
            vec![
                FieldValue::Vec3(Vec3::new(x, 0.0, 0.0)),
                FieldValue::Quat(Quat::from_rotation_y(angle)),
                FieldValue::U8(state),
            ],
        )])
    };
    let p1 = enc
        .encode(NetTick(100), &world(0.0, 0.0, 1), None, 4096)
        .expect("p1");
    interp.receive(&p1).expect("p1");
    let p2 = enc
        .encode(
            NetTick(110),
            &world(10.0, 1.0, 2),
            interp.ack().map(|a| a.baseline),
            4096,
        )
        .expect("p2");
    interp.receive(&p2).expect("p2");
    // First advance: playback = newest - delay = 100.
    interp.advance(1.0);
    assert_eq!(interp.playback_tick(), Some(100.0));
    for _ in 0..5 {
        interp.advance(1.0);
    }
    let t = interp.playback_tick().expect("playback");
    assert!(t > 100.0 && t < 110.0, "{t}");
    let mut out = Vec::new();
    assert!(interp.sample(5, &mut out));
    let s = ((t - 100.0) / 10.0) as f32;
    let FieldValue::Vec3(p) = out[POS] else {
        panic!("vec3")
    };
    assert!((p.x - 10.0 * s).abs() < 0.01, "{p} at {s}");
    let FieldValue::Quat(q) = out[1] else {
        panic!("quat")
    };
    assert!(q.angle_between(Quat::from_rotation_y(s)) < 0.01, "{q:?} at {s}");
    assert_eq!(
        out[STATE],
        FieldValue::U8(1),
        "a step field takes the older sample"
    );
    // A late packet (older tick) changes nothing.
    let before = interp.playback_tick();
    let late = interp.receive(&p1).expect("duplicate decodes");
    assert!(late.late);
    interp.advance(0.0);
    assert!(interp.playback_tick() >= before);
    // Far past the newest sample, playback holds at newest + max_ahead and shows the last state.
    for _ in 0..100 {
        interp.advance(1.0);
    }
    assert!(interp.playback_tick().expect("t") <= 112.0);
    assert!(interp.sample(5, &mut out));
    assert_eq!(out[STATE], FieldValue::U8(2));
    assert!(!interp.sample(6, &mut out) && out.is_empty());
    assert_eq!(
        interp.receive(&[1, 2, 3]).map_err(|e| e.code()),
        Err("decode_failed")
    );
    assert_eq!(interp.counters().decode_errors, 1);
}

/// The receipt window: applied, in flight (too recent to judge) and lost packets, across a
/// shift of more than one word and the 64-packet bitfield of an ack.
#[test]
fn receipts_tell_applied_in_flight_and_lost_packets() {
    let mut r = Receipts::default();
    assert_eq!(r.delivery(Seq(1)), Delivery::InFlight);
    r.take(&ExternalAck {
        baseline: Seq(10),
        applied: Seq(10),
        applied_bits: 0b1011, // 9, 8 and 6 applied; 7 missing
    });
    assert_eq!(r.delivery(Seq(10)), Delivery::Applied);
    assert_eq!(r.delivery(Seq(9)), Delivery::Applied);
    assert_eq!(r.delivery(Seq(7)), Delivery::Lost);
    assert_eq!(r.delivery(Seq(6)), Delivery::Applied);
    assert_eq!(r.delivery(Seq(11)), Delivery::InFlight);
    r.set(12);
    assert_eq!(
        r.delivery(Seq(11)),
        Delivery::InFlight,
        "one behind the newest: maybe reordered"
    );
    r.set(150);
    assert_eq!(
        r.delivery(Seq(12)),
        Delivery::Applied,
        "kept across a two-word shift"
    );
    assert_eq!(r.delivery(Seq(7)), Delivery::Lost);
    r.set(11);
    assert_eq!(
        r.delivery(Seq(11)),
        Delivery::Applied,
        "a late receipt still counts"
    );
    r.set(150 + 300);
    assert_eq!(r.delivery(Seq(12)), Delivery::Lost, "out of the window");
    // Wrap-around of the sequence space.
    let mut w = Receipts::default();
    w.set(u32::MAX - 1);
    w.set(2);
    assert_eq!(w.delivery(Seq(u32::MAX - 1)), Delivery::Applied);
    assert_eq!(w.delivery(Seq(u32::MAX)), Delivery::Lost);
}

/// A record whose packet is lost is sent again once the client's ack shows the gap; a still
/// entity whose state the client applied costs nothing afterwards.
#[test]
fn a_lost_record_is_sent_again_and_a_still_entity_costs_nothing() {
    let mut rep: ExternalReplication<u32> =
        ExternalReplication::new(schema(), one_class(500.0, 0.1)).expect("new");
    let mut sim = Sim::new(1, 0, 0.0);
    sim.entities.insert(1, (Vec3::new(10.0, 0.0, 0.0), 4.0, 0.0, 0));
    let peer = PeerId(1);
    rep.add_client(peer).expect("client");
    rep.set_viewpoints(peer, &[Vec3::ZERO]).expect("vp");
    let mut interp = ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default());
    let mut packets = Vec::new();
    let mut lost = 0;
    for tick in 1..=60u32 {
        // Moves until tick 20, then stands still.
        if tick <= 20 {
            sim.time_s = tick as f32;
        }
        sim.push_all(&mut rep);
        packets.clear();
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
        for p in &packets {
            // The packet with the last change is lost.
            if tick == 20 {
                lost += 1;
                continue;
            }
            interp.receive(&p.bytes).expect("decode");
            if let Some(ack) = interp.ack() {
                rep.ack(peer, ack).expect("ack");
            }
        }
    }
    assert_eq!(lost, 1);
    let mut want = Vec::new();
    sim.record(1, &mut want);
    let want = WorldSnapshot::new(vec![EntitySnapshot::new(NetId(1), want)]);
    let want = schema().quantize_snapshot(&want).expect("quantize");
    let mut got = Vec::new();
    assert!(interp.latest(1, &mut got).is_some());
    assert_eq!(got, want.entities[0].fields, "the lost change was sent again");
    let c = rep.counters(peer).expect("counters");
    assert!(
        c.last_bytes < 20,
        "a still, applied entity costs {} bytes per packet",
        c.last_bytes
    );
}

/// A seeded run for the stats tests: `count` entities of [`Sim`] on a 1,500 m square, `clients`
/// clients on bad-wifi both ways (seeded, virtual time), `ticks` ticks at 60 Hz with a send every
/// second tick and a budget that binds (the 4,096 B/s floor: about 136 bytes per send).
fn seeded_stats_run(seed: u64, count: u32, clients: u32, ticks: u32) -> ExternalReplication<u32> {
    let config = ExternalConfig {
        send_every_ticks: 2,
        rate: MIN_CLIENT_RATE,
        cell_m: 100.0,
        classes: vec![ExternalClass {
            relevance: RelevanceShape::Spatial {
                radius: 600.0,
                hysteresis: 0.1,
            },
            rate_tiers: Some(RateTiers::default()),
            ..ExternalClass::default()
        }],
        ..ExternalConfig::default()
    };
    let mut rep: ExternalReplication<u32> = ExternalReplication::new(schema(), config).expect("new");
    let mut sim = Sim::new(seed, count, 1500.0);
    sim.push_all(&mut rep);
    let mut links = Vec::new();
    for c in 0..clients {
        let peer = PeerId(c + 1);
        rep.add_client(peer).expect("client");
        let vp = Vec3::new(
            -900.0 + 600.0 * (c % 4) as f32,
            0.0,
            -900.0 + 600.0 * (c / 4) as f32,
        );
        rep.set_viewpoints(peer, &[vp]).expect("viewpoint");
        links.push((
            peer,
            NetConditioner::new(bad_wifi(seed ^ (100 + u64::from(c)))),
            NetConditioner::new(bad_wifi(seed ^ (200 + u64::from(c)))),
            ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default()),
        ));
    }
    let mut packets = Vec::new();
    let mut inbox: HashMap<PeerId, Vec<Vec<u8>>> = HashMap::new();
    for tick in 1..=ticks {
        sim.time_s = tick as f32 / 60.0;
        sim.push_all(&mut rep);
        packets.clear();
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
        for packet in packets.drain(..) {
            let Some((_, down, ..)) = links.iter_mut().find(|l| l.0 == packet.peer) else {
                panic!("packet for an unknown client");
            };
            let passed = down.process(packet.bytes, addr(), now_ms(tick));
            inbox
                .entry(packet.peer)
                .or_default()
                .extend(passed.into_iter().map(|(b, _)| b));
        }
        for (peer, down, up, interp) in &mut links {
            let mut arrived = inbox.remove(peer).unwrap_or_default();
            arrived.extend(down.release_due(now_ms(tick)).into_iter().map(|(b, _)| b));
            for bytes in arrived {
                interp.receive(&bytes).expect("a delivered packet decodes");
            }
            interp.advance(1.0);
            if let Some(ack) = interp.ack() {
                let mut acks = up.process(ack.to_bytes().to_vec(), addr(), now_ms(tick));
                acks.extend(up.release_due(now_ms(tick)));
                for (bytes, _) in acks {
                    let ack = ExternalAck::from_bytes(&bytes).expect("16 bytes");
                    rep.ack(*peer, ack).expect("ack");
                }
            }
        }
    }
    rep
}

/// `NetExternalStats` (KI-NT-012): after a seeded run, the published entry holds exactly what
/// `ExternalReplication::stats()` returns, with the tick and the publish count; a later publish
/// replaces it; at most `MAX_LABELS` labels are kept (an existing one still refreshes) and a
/// removed label is gone.
#[test]
fn net_external_stats_holds_what_stats_returns_after_a_seeded_run() {
    let mut rep = seeded_stats_run(0x5eed, 300, 6, 240);
    let want = rep.stats();
    // The run did something worth reading: packets, acks, records, a binding budget.
    assert_eq!(want.clients.len(), 6);
    assert_eq!(want.entities, 300);
    for c in &want.clients {
        assert!(
            c.counters.packets > 0 && c.counters.acks > 0 && c.counters.records > 0,
            "{c:?}"
        );
        assert!(c.ack.is_some(), "{c:?}");
    }
    assert!(
        want.clients.iter().any(|c| c.counters.omitted > 0),
        "the budget never bound"
    );

    let mut published = NetExternalStats::default();
    assert!(published.is_empty());
    assert!(rep.publish_stats("world", NetTick(240), &mut published));
    let entry = published.get("world").expect("published");
    assert_eq!(entry.tick, 240);
    assert_eq!(entry.publishes, 1);
    assert_eq!(entry.stats, want, "the resource holds stats() as it was");

    // A later publish replaces the snapshot and counts.
    rep.remove(0);
    rep.remove_client(PeerId(6));
    let later = rep.stats();
    assert_ne!(later, want);
    assert!(rep.publish_stats("world", NetTick(300), &mut published));
    let entry = published.get("world").expect("published");
    assert_eq!((entry.tick, entry.publishes), (300, 2));
    assert_eq!(entry.stats, later);
    assert_eq!(entry.stats.entities, 299);
    assert_eq!(entry.stats.clients.len(), 5);

    // The label cap: new labels past it are refused, existing ones still refresh.
    for i in 1..NetExternalStats::MAX_LABELS {
        assert!(published.publish(&format!("extra-{i:02}"), NetTick(1), later.clone()));
    }
    assert_eq!(published.len(), NetExternalStats::MAX_LABELS);
    assert!(!published.publish("one-too-many", NetTick(1), later.clone()));
    assert!(published.get("one-too-many").is_none());
    assert!(published.publish("world", NetTick(301), later.clone()));
    assert_eq!(published.get("world").map(|e| e.publishes), Some(3));
    assert!(published.remove("extra-01"));
    assert!(!published.remove("extra-01"));
    assert!(published.publish("one-too-many", NetTick(2), later));
    let labels: Vec<&str> = published.entries().keys().map(String::as_str).collect();
    assert_eq!(labels.first().copied(), Some("extra-02"), "{labels:?}");
    assert!(labels.contains(&"one-too-many") && labels.contains(&"world"));
}

/// The same seed gives the same stats (the agent test compares against a rerun).
#[test]
fn a_seeded_stats_run_is_deterministic() {
    let a = seeded_stats_run(7, 120, 3, 120).stats();
    let b = seeded_stats_run(7, 120, 3, 120).stats();
    assert_eq!(a, b);
    let c = seeded_stats_run(8, 120, 3, 120).stats();
    assert_ne!(a, c, "another seed changes the run");
}
