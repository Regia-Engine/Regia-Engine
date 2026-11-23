// SPDX-License-Identifier: MPL-2.0
//! `net.external_stats` through the dispatcher (`dispatch::call_in_process`), after a seeded run of
//! an external replication (KI-NT-012, ADR-8892).

#![cfg(test)]

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_net::external_replication::{
    ExternalAck, ExternalClass, ExternalConfig, ExternalInterpolator, ExternalReplication, ExternalSource,
    InterpolatorConfig,
};
use regia_net::interest::{RateTiers, RelevanceShape};
use regia_net::snapshot_delta::{FieldDef, FieldKind, FieldValue, SnapshotSchema};
use regia_net::{NetExternalStats, NetTick, PeerId};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    crate::dispatch::call_in_process(world, method, params).expect("answers at once")
}

fn code(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

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
        FieldDef::new("state", FieldKind::U8),
    ])
    .expect("schema")
}

/// A deterministic xorshift64* stream.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Entities walking along X at their own speed, on rows 40 m apart.
struct Walkers {
    count: u32,
    tick: u32,
}

impl Walkers {
    fn position(&self, key: u32) -> Vec3 {
        let speed = 0.05 + (key % 9) as f32 * 0.02;
        Vec3::new(
            -1200.0 + (key % 30) as f32 * 80.0 + self.tick as f32 * speed,
            0.0,
            -1200.0 + (key / 30) as f32 * 40.0,
        )
    }
}

impl ExternalSource<u32> for Walkers {
    fn priority(&self, _key: u32, _client: PeerId, _distance: Option<f32>) -> f32 {
        1.0
    }
    fn record(&self, key: u32, out: &mut Vec<FieldValue>) {
        out.push(FieldValue::Vec3(self.position(key)));
        out.push(FieldValue::U8((key % 4) as u8));
    }
}

/// A seeded run: 600 walkers, 5 clients, 180 ticks, a budget that binds, and a seeded quarter of
/// the packets and acks lost (seeded), so the counters are not those of a clean link.
fn seeded_run(seed: u64) -> ExternalReplication<u32> {
    let config = ExternalConfig {
        send_every_ticks: 2,
        rate: 12_000,
        cell_m: 100.0,
        classes: vec![ExternalClass {
            relevance: RelevanceShape::Spatial {
                radius: 700.0,
                hysteresis: 0.1,
            },
            rate_tiers: Some(RateTiers::default()),
            ..ExternalClass::default()
        }],
        ..ExternalConfig::default()
    };
    let mut rep = ExternalReplication::new(schema(), config).expect("new");
    let mut sim = Walkers { count: 600, tick: 0 };
    let mut clients = Vec::new();
    for c in 0..5u32 {
        let peer = PeerId(c + 1);
        rep.add_client(peer).expect("client");
        let vp = Vec3::new(-1000.0 + 500.0 * c as f32, 0.0, -600.0 + 300.0 * (c % 2) as f32);
        rep.set_viewpoints(peer, &[vp]).expect("viewpoint");
        clients.push((
            peer,
            ExternalInterpolator::<u32>::new(schema(), InterpolatorConfig::default()),
        ));
    }
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut packets = Vec::new();
    for tick in 1..=180u32 {
        sim.tick = tick;
        for key in 0..sim.count {
            rep.upsert(key, sim.position(key), 0).expect("upsert");
        }
        packets.clear();
        rep.send_tick(NetTick(tick), &sim, &mut packets).expect("send");
        for p in &packets {
            if rng.next().is_multiple_of(4) {
                continue; // the packet is lost
            }
            let Some((_, interp)) = clients.iter_mut().find(|c| c.0 == p.peer) else {
                panic!("packet for an unknown client");
            };
            interp.receive(&p.bytes).expect("decode");
            interp.advance(1.0);
            let ack: Option<ExternalAck> = interp.ack();
            if let Some(ack) = ack
                && !rng.next().is_multiple_of(4)
            {
                rep.ack(p.peer, ack).expect("ack");
            }
        }
    }
    rep
}

/// After a seeded run published into `NetExternalStats`, `net.external_stats` returns exactly the
/// numbers of `ExternalReplication::stats()` (as serde writes them), the label, tick and publish
/// count, and totals that are the sums over the clients; a label filter returns one entry; an
/// unknown label is `external_unknown`; bad params are `external_bad_params`; no resource is an
/// empty list.
#[test]
fn net_external_stats_matches_stats_after_a_seeded_run() {
    let mut world = World::new();
    let empty = call(&mut world, "net.external_stats", json!({})).expect("no resource");
    assert_eq!(empty, json!({ "replications": [] }));

    let rep = seeded_run(0xe57a);
    let want = rep.stats();
    assert_eq!(want.clients.len(), 5);
    assert!(
        want.clients
            .iter()
            .all(|c| c.counters.packets > 0 && c.counters.acks > 0)
    );
    assert!(
        want.clients.iter().any(|c| c.counters.omitted > 0),
        "the budget never bound"
    );
    assert!(
        want.clients.iter().all(|c| c.counters.full_packets >= 1),
        "the first packet of a client is full"
    );

    let mut published = NetExternalStats::default();
    assert!(rep.publish_stats("world", NetTick(180), &mut published));
    // A second replication, so the filter has something to leave out.
    let other = seeded_run(0x0bad);
    assert!(other.publish_stats("ambient", NetTick(90), &mut published));
    world.insert_resource(published);

    let all = call(&mut world, "net.external_stats", json!({})).expect("all");
    let rows = all["replications"].as_array().expect("list");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["label"], "ambient", "labels sorted");
    assert_eq!(rows[1]["label"], "world");

    let one = call(
        &mut world,
        "net.external_stats",
        json!({ "replication": "world" }),
    )
    .expect("one");
    let rows = one["replications"].as_array().expect("list");
    assert_eq!(rows.len(), 1);
    let row = rows[0].as_object().expect("object");
    assert_eq!(row["label"], "world");
    assert_eq!(row["tick"], 180);
    assert_eq!(row["publishes"], 1);
    // The stats part is stats() exactly, every number.
    let Value::Object(stats) = serde_json::to_value(&want).expect("serde") else {
        panic!("ExternalStats serializes as an object");
    };
    for (key, value) in &stats {
        assert_eq!(&row[key], value, "`{key}` differs from stats()");
    }
    assert_eq!(
        row.len(),
        stats.len() + 4,
        "only label, tick, publishes and totals are added"
    );
    for (i, c) in want.clients.iter().enumerate() {
        let got = &row["clients"][i];
        assert_eq!(got["peer"], c.peer);
        assert_eq!(got["rate"], c.rate);
        assert_eq!(got["counters"]["packets"], c.counters.packets);
        assert_eq!(got["counters"]["bytes"], c.counters.bytes);
        assert_eq!(got["counters"]["omitted"], c.counters.omitted);
        assert_eq!(got["counters"]["acks"], c.counters.acks);
        assert_eq!(got["counters"]["tier_sent"], json!(c.counters.tier_sent));
    }
    // Totals are the sums over the clients.
    let total = |f: fn(&regia_net::external_replication::ExternalClientCounters) -> u64| -> u64 {
        want.clients.iter().map(|c| f(&c.counters)).sum()
    };
    let totals = &row["totals"];
    assert_eq!(totals["clients"], 5);
    assert_eq!(totals["packets"], total(|c| c.packets));
    assert_eq!(totals["bytes"], total(|c| c.bytes));
    assert_eq!(totals["full_packets"], total(|c| c.full_packets));
    assert_eq!(totals["records"], total(|c| c.records));
    assert_eq!(totals["omitted"], total(|c| c.omitted));
    assert_eq!(totals["tier_held"], total(|c| c.tier_held));
    assert_eq!(totals["budget_skips"], total(|c| c.budget_skips));
    assert_eq!(totals["acks"], total(|c| c.acks));
    assert_eq!(totals["stale_acks"], total(|c| c.stale_acks));
    assert_eq!(totals["relevant"], total(|c| u64::from(c.relevant)));
    let tier = |t: usize| want.clients.iter().map(|c| c.counters.tier_sent[t]).sum::<u64>();
    assert_eq!(totals["tier_sent"], json!([tier(0), tier(1), tier(2)]));

    // The same seed reproduces the same numbers (the run is the evidence, not the clock).
    assert_eq!(seeded_run(0xe57a).stats(), want);

    let err = call(&mut world, "net.external_stats", json!({ "replication": "nope" })).expect_err("unknown");
    assert_eq!(code(&err), "external_unknown");
    for bad in [
        json!({ "replication": "" }),
        json!({ "replication": 3 }),
        json!({ "label": "world" }),
    ] {
        let err = call(&mut world, "net.external_stats", bad.clone()).expect_err("bad params");
        assert_eq!(code(&err), "external_bad_params", "{bad}");
    }
    // Params that are not an object are refused by the dispatcher before the handler (F0002).
    let err = call(&mut world, "net.external_stats", json!([1])).expect_err("not an object");
    assert_eq!(code(&err), "F0002");
}
