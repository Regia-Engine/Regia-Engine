// SPDX-License-Identifier: MPL-2.0
//! Server send cost with the spec 72 pipeline (spec 72 § 14): N replicated entities spread over a
//! 2 km square, C connections with one viewpoint each, a spatial profile sized for about 2,000
//! relevant entities per connection, 10 % of the entities moving each tick. Times the server's
//! receive + apply + gather (relevance, priority, packing) + send on send ticks; no client worlds
//! run, and each connection acknowledges every state frame at once (`acked`) or stays on the
//! reliable tick stream (`stream`). With `walk` every viewpoint moves 0.1 m a tick, as players do.
//!
//! `cargo run --release -p regia_net --example net_relevance_bench -- [entities] [connections] [ticks] [rate] [workers] [acked|stream] [still|walk] [n214]`
//! (defaults 20000, 64, 120, 49152 bytes per second per connection, 8 workers, acked, still).
//!
//! `n214` adds the spec 120 § 8.4 scale case (N214, ADR-8031): a mixed profile set (most entities on
//! the near profile, 10 % vehicles at 400 m and 1 % radar contacts at 1 km, both with distance rate
//! tiers) and 64 projectile spawns per tick near the players, each despawned 30 ticks later. It
//! prints the gather's mean and p99 and the allocations per send tick (a counting allocator, every
//! thread), mean and p99, as `perf-count` lines for `cargo xtask perf-budgets`:
//! `-- 20000 128 300 49152 8 acked walk n214`.

// The counting global allocator is an `unsafe impl` by definition (ADR-8031); the shared one in
// `regia_test_utils` would pull the whole engine into this crate's dev graph.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use glam::Vec3;
use regia_core::tasks::{TaskPoolOptions, init};
use regia_ecs::prelude::*;
use regia_net::replication::{ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{NetNode, net_apply, net_receive, net_send, server_add_peer};
use regia_net::testing::SplitMix64;
use regia_net::*;
use regia_scene::transform::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// Counts allocations on every thread (the gather's workers too).
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to the system allocator unchanged; the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller's contract for `alloc` is passed through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: the caller's contract for `realloc` is passed through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The `q` quantile (0..=1) of a sample set, by the nearest-rank method.
fn quantile(samples: &[f64], q: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len().max(1));
    sorted.get(rank - 1).copied().unwrap_or(0.0)
}

fn arg(i: usize, default: usize) -> usize {
    std::env::args()
        .nth(i)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
}

fn stats(samples: &[f64]) -> (f64, f64) {
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    let worst = samples.iter().copied().fold(0.0, f64::max);
    (mean, worst)
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map_or(0.0, |pages| pages * 4096.0 / (1024.0 * 1024.0))
}

fn main() {
    let entities = arg(1, 20_000);
    let conns = arg(2, 64).max(1);
    let ticks = arg(3, 120).max(2);
    let rate = arg(4, 49_152) as u32;
    let workers = arg(5, 8).max(1);
    let acked = std::env::args().nth(6).as_deref() != Some("stream");
    let walking = std::env::args().nth(7).as_deref() == Some("walk");
    let n214 = std::env::args().nth(8).as_deref() == Some("n214");
    init(TaskPoolOptions {
        compute_threads: Some(workers),
        ..TaskPoolOptions::default()
    });
    let half = 1000.0f32;
    // About 2,000 relevant per connection: density × π r² = 2,000.
    let density = entities as f32 / (4.0 * half * half);
    let radius = (2000.0 / (density * std::f32::consts::PI)).sqrt();

    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(LocalPeer(Some(PeerId::SERVER)));
    register_replicated::<Hp>(&mut world, ReplicateOptions::default());
    regia_net::runtime::ensure_resources(&mut world);
    install_net_interest(&mut world);
    world
        .resource_mut::<NetProfiles>()
        .insert(
            "bench",
            NetProfile {
                relevance: RelevanceShape::Spatial {
                    radius,
                    hysteresis: 0.15,
                },
                ..NetProfile::default()
            },
        )
        .unwrap_or_else(|e| panic!("bench profile: {e}"));
    world.resource_mut::<NetProfiles>().set_default("bench");
    if n214 {
        // The players' near profile has the default distance tiers too (spec 120 § 8.3).
        let mut near = world
            .resource::<NetProfiles>()
            .get("bench")
            .cloned()
            .unwrap_or_default();
        near.rate_tiers = Some(RateTiers::default());
        world
            .resource_mut::<NetProfiles>()
            .insert("bench", near)
            .unwrap_or_else(|e| panic!("bench profile: {e}"));
        for (name, radius, linger) in [("vehicle", 400.0, 30), ("radar", 1000.0, 30), ("shot", 150.0, 0)] {
            world
                .resource_mut::<NetProfiles>()
                .insert(
                    name,
                    NetProfile {
                        relevance: RelevanceShape::Spatial {
                            radius,
                            hysteresis: 0.15,
                        },
                        linger_ticks: linger,
                        rate_tiers: (name != "shot").then(RateTiers::default),
                        ..NetProfile::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{name} profile: {e}"));
        }
    }
    let mut settings = NetSettings::default();
    settings.replication.client_rate_bytes = rate;
    // Fill every connection first, then measure with the normal spawn budget.
    settings.replication.spawns_per_send = 100_000;
    world.insert_resource(settings.clone());

    let mut rng = SplitMix64::new(5);
    let mut r = move |lo: f32, hi: f32| lo + (hi - lo) * rng.next_f64() as f32;
    let ids: Vec<Entity> = (0..entities)
        .map(|i| {
            let p = Vec3::new(r(-half, half), 0.0, r(-half, half));
            let profile = match i % 100 {
                _ if !n214 => None,
                0 => Some("radar".to_string()),
                1..=10 => Some("vehicle".to_string()),
                _ => None,
            };
            world
                .spawn((
                    Replicated {
                        profile,
                        ..Replicated::default()
                    },
                    Transform {
                        translation: p,
                        ..Transform::default()
                    },
                    Hp { hp: i as u32 },
                ))
                .id()
        })
        .collect();
    let wire = if acked { ConnWire::Acked } else { ConnWire::Stream };
    let peers: Vec<PeerId> = (0..conns).map(|c| PeerId(c as u32 + 1)).collect();
    let mut viewers: Vec<Entity> = Vec::new();
    for peer in &peers {
        server_add_peer(&mut world, *peer);
        world.resource_mut::<ServerReplication>().set_wire(*peer, wire);
        let p = Vec3::new(r(-half * 0.6, half * 0.6), 0.0, r(-half * 0.6, half * 0.6));
        let viewer = world.spawn((
            NetViewpoint {
                player: NetPlayerId {
                    peer: *peer,
                    local: 0,
                },
            },
            Transform {
                translation: p,
                ..Transform::default()
            },
        ));
        viewers.push(viewer.id());
    }
    let tick = |world: &mut World| {
        net_receive(world);
        net_apply(world);
        net_send(world);
        world.resource_mut::<NetNode>().outbox.clear();
        // Every state frame arrives and is acknowledged before the next send tick.
        let mut rep = world.resource_mut::<ServerReplication>();
        for p in &peers {
            rep.ack_all_state(*p);
        }
    };
    // The fill runs until no connection has a spawn left to send (at least 6 ticks): since every
    // record takes the byte test (ADR-8419), the spawns of a full view take several send ticks at
    // the bench rate, and a measurement started before they are out would time the join flood
    // instead of the steady load (ADR-8885).
    let mut fill_ticks = 0usize;
    loop {
        tick(&mut world);
        fill_ticks += 1;
        let pending: u64 = world
            .resource::<NetInterest>()
            .peers
            .values()
            .map(|p| u64::from(p.send.counters.spawns_pending))
            .sum();
        if fill_ticks >= 6 && pending == 0 {
            break;
        }
        if fill_ticks >= 2000 {
            println!("fill stopped after {fill_ticks} ticks with {pending} spawns still pending");
            break;
        }
    }
    println!("fill: {fill_ticks} ticks until every connection held its view");
    settings.replication.spawns_per_send = 24;
    let every = settings.replication.send_every_ticks.max(1) as usize;
    world.insert_resource(settings);
    let relevant: usize = world
        .resource::<NetInterest>()
        .peers
        .values()
        .map(|p| p.held_count())
        .sum::<usize>()
        / conns;

    let mut samples = Vec::new();
    let (mut changes, mut relevance, mut send) = (Vec::new(), Vec::new(), Vec::new());
    let mut cpu = [0.0f64; 5];
    let (mut allocs, mut alloc_bytes) = (Vec::new(), Vec::new());
    // Projectiles in flight: (entity, despawn tick).
    let mut shots: std::collections::VecDeque<(Entity, usize)> = std::collections::VecDeque::new();
    let mut shot_rng = SplitMix64::new(77);
    for t in 0..ticks {
        if n214 {
            while shots.front().is_some_and(|(_, at)| *at <= t) {
                if let Some((e, _)) = shots.pop_front() {
                    world.despawn(e);
                }
            }
            for k in 0..64usize {
                let from = viewers[k % viewers.len()];
                let at = world
                    .get::<Transform>(from)
                    .map_or(Vec3::ZERO, |tr| tr.translation);
                let dir = (shot_rng.next_f64() as f32) * std::f32::consts::TAU;
                let e = world
                    .spawn((
                        Replicated {
                            profile: Some("shot".to_string()),
                            ..Replicated::default()
                        },
                        Transform {
                            translation: at + Vec3::new(dir.cos(), 1.0, dir.sin()),
                            ..Transform::default()
                        },
                        Hp { hp: 1 },
                    ))
                    .id();
                shots.push_back((e, t + 30));
            }
            for (e, _) in shots.iter() {
                if let Some(mut tr) = world.get_mut::<Transform>(*e) {
                    let d = tr.translation.normalize_or_zero();
                    tr.translation += Vec3::new(d.x, 0.0, d.z);
                }
            }
        }
        for (i, e) in ids.iter().enumerate().skip(t % 10).step_by(10) {
            if let Some(mut tr) = world.get_mut::<Transform>(*e) {
                tr.translation.x += if i.is_multiple_of(2) { 0.5 } else { -0.5 };
            }
        }
        if walking {
            for (i, e) in viewers.iter().enumerate() {
                if let Some(mut tr) = world.get_mut::<Transform>(*e) {
                    tr.translation.z += if i.is_multiple_of(2) { 0.1 } else { -0.1 };
                }
            }
        }
        let sends = (world.resource::<NetNode>().tick.0 as usize).is_multiple_of(every);
        let (a0, b0) = (ALLOCS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
        let start = Instant::now();
        tick(&mut world);
        if sends {
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            allocs.push((ALLOCS.load(Ordering::Relaxed) - a0) as f64);
            alloc_bytes.push((BYTES.load(Ordering::Relaxed) - b0) as f64);
            let g = world.resource::<ServerReplication>().timing;
            changes.push(g.changes_ms);
            relevance.push(g.relevance_ms);
            send.push(g.send_ms);
            let parts = [
                g.relevance_shared_ms,
                g.walk_cpu_ms,
                g.priority_cpu_ms,
                g.pack_cpu_ms,
                g.commit_cpu_ms,
            ];
            for (sum, part) in cpu.iter_mut().zip(parts) {
                *sum += part;
            }
        }
    }
    let (mean, worst) = stats(&samples);
    let i = world.resource::<NetInterest>();
    let used: u64 = i
        .peers
        .values()
        .map(|p| u64::from(p.send.counters.budget_used))
        .sum();
    let deferred: u64 = i
        .peers
        .values()
        .map(|p| u64::from(p.send.counters.deferred))
        .sum();
    let (rel_mean, rel_worst) = stats(&relevance);
    let (send_mean, send_worst) = stats(&send);
    let (chg_mean, chg_worst) = stats(&changes);
    let budget: Vec<f64> = relevance.iter().zip(&send).map(|(a, b)| a + b).collect();
    let (bud_mean, bud_worst) = stats(&budget);
    let n = samples.len().max(1) as f64;
    let [shared, walk, priority, pack, commit] = cpu.map(|c| c / n);
    println!(
        "entities {entities}, connections {conns}, radius {radius:.0} m, ~{relevant} relevant per connection, \
         10 % moving per tick, viewpoints {}, rate {rate} B/s, {} send ticks, {workers} workers, wire {}\n\
         whole send tick (receive, apply, gather, send, acks): mean {mean:.3} ms, worst {worst:.3} ms\n\
         spec 72 § 14 budget (relevance + priority + packing): mean {bud_mean:.3} ms, median {:.3} ms, worst {bud_worst:.3} ms\n\
         phases mean/worst: changes {chg_mean:.3}/{chg_worst:.3} ms, relevance {rel_mean:.3}/{rel_worst:.3} ms, \
         priority+packing {send_mean:.3}/{send_worst:.3} ms\n\
         serial relevance {shared:.3} ms; CPU time over connections: walk {walk:.2}, priority {priority:.2}, \
         pack {pack:.2}, commit {commit:.2} ms\n\
         last tick: {} bytes used per connection on average, {} deltas deferred per connection, RSS {:.0} MB",
        if walking { "walking" } else { "still" },
        samples.len(),
        wire.as_str(),
        median(&budget),
        used / conns as u64,
        deferred / conns as u64,
        rss_mb()
    );
    let (alloc_mean, _) = stats(&allocs);
    let (bytes_mean, _) = stats(&alloc_bytes);
    let n = allocs.len().max(1);
    println!(
        "allocations per send tick (whole tick, every thread): mean {alloc_mean:.0}, p99 {:.0}, worst {:.0}; bytes mean {bytes_mean:.0}",
        quantile(&allocs, 0.99),
        quantile(&allocs, 1.0)
    );
    if n214 {
        let i = world.resource::<NetInterest>();
        let (tier_held, tier_sent) = i.peers.values().fold((0u64, [0u64; 3]), |(h, mut s), p| {
            for (sum, v) in s.iter_mut().zip(p.send.counters.tier_sent) {
                *sum += u64::from(v);
            }
            (h + u64::from(p.send.counters.tier_held), s)
        });
        println!(
            "N214 gather (relevance + priority + packing): mean {bud_mean:.3} ms, p99 {:.3} ms, worst {bud_worst:.3} ms \
             ({} projectiles in flight, last tick: {} updates held by rate tiers, sent near/mid/far {:?})",
            quantile(&budget, 0.99),
            shots.len(),
            tier_held,
            tier_sent
        );
        // `cargo xtask perf-budgets` reads these (AllocsPerOp): the mean over the send ticks, and the
        // p99 tick as one op.
        println!(
            "perf-count net_relevance_n214_allocs: ops={n} allocs={:.0} bytes={:.0}",
            allocs.iter().sum::<f64>(),
            alloc_bytes.iter().sum::<f64>()
        );
        println!(
            "perf-count net_relevance_n214_allocs_p99: ops=1 allocs={:.0} bytes={:.0}",
            quantile(&allocs, 0.99),
            quantile(&alloc_bytes, 0.99)
        );
    }
}
