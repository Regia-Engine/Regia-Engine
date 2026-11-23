// SPDX-License-Identifier: MPL-2.0
//! Lag-compensation cost (spec 75 § 14, spec 90 § 9): N lag-compensated entities with S capsule
//! shapes each, walking on a 100 m square, recorded every tick; then Q rewound raycasts per tick
//! from random shooters at random fractional view ticks inside the rewind window.
//!
//! `cargo run --release -p regia_net --example net_lagcomp_bench -- [entities] [shapes] [queries] [ticks]`
//! (defaults 64, 8, 200, 600). Budgets on the Desktop reference class: record under 0.1 ms per tick,
//! 200 queries under 0.3 ms per tick.

use std::time::Instant;

use glam::Vec3;
use regia_core::math::Ray;
use regia_ecs::prelude::*;
use regia_net::lagcomp::{install_lag_compensation, lagcomp_record};
use regia_net::runtime::{NetNode, ensure_resources, server_add_peer};
use regia_net::testing::SplitMix64;
use regia_net::*;
use regia_scene::transform::Transform;

fn arg(i: usize, default: usize) -> usize {
    std::env::args()
        .nth(i)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn stats(samples: &mut [f64]) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    let median = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
    let worst = samples.last().copied().unwrap_or(0.0);
    (mean, median, worst)
}

fn main() {
    let entities = arg(1, 64);
    let shapes = arg(2, 8).max(1);
    let queries = arg(3, 200);
    let ticks = arg(4, 600).max(20) as u32;
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    ensure_resources(&mut world);
    install_lag_compensation(&mut world);
    let peers = 8u32;
    for p in 1..=peers {
        server_add_peer(&mut world, PeerId(p));
        world.spawn(NetPlayer {
            id: NetPlayerId {
                peer: PeerId(p),
                local: 0,
            },
        });
    }
    let mut rng = SplitMix64::new(75);
    let mut walkers = Vec::new();
    for i in 0..entities {
        let pos = Vec3::new(
            rng.next_f64() as f32 * 100.0 - 50.0,
            0.0,
            rng.next_f64() as f32 * 100.0 - 50.0,
        );
        let vel = Vec3::new(rng.next_f64() as f32 - 0.5, 0.0, rng.next_f64() as f32 - 0.5) * 12.0;
        let hit_shapes: Vec<HitShape> = (0..shapes)
            .map(|s| {
                HitShape::new(
                    HitShapeKind::Capsule {
                        radius: 0.15,
                        half_height: 0.2,
                    },
                    "limb",
                )
                .at(Vec3::new(0.0, 0.2 * s as f32, 0.0))
            })
            .collect();
        let e = world
            .spawn((
                NetId(i as u32 + 1),
                Transform::from_translation(pos),
                HitShapes {
                    shapes: hit_shapes,
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        walkers.push((e, vel));
    }
    let hz = 60.0f32;
    let mut record_ms = Vec::new();
    let mut query_ms = Vec::new();
    let mut hits = 0u64;
    for tick in 0..ticks {
        for (e, v) in &walkers {
            if let Some(mut t) = world.get_mut::<Transform>(*e) {
                t.translation += *v / hz;
                if t.translation.x.abs() > 50.0 || t.translation.z.abs() > 50.0 {
                    t.translation = Vec3::ZERO;
                }
            }
        }
        world.resource_mut::<NetNode>().tick = NetTick(tick);
        let t0 = Instant::now();
        lagcomp_record(&mut world);
        record_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        world.resource_mut::<NetNode>().tick = NetTick(tick + 1);
        if tick < 20 {
            continue;
        }
        // Shooters' view ticks somewhere 4..14 ticks back.
        let mut q = world.query::<(Entity, &NetPlayer)>();
        let players: Vec<Entity> = q.iter(&world).map(|(e, _)| e).collect();
        for p in players {
            let back = 4.0 + rng.next_f64() * 10.0;
            world.entity_mut(p).insert(NetViewTick {
                tick: f64::from(tick + 1) - back,
                delay_ms: 100.0,
            });
        }
        let rays: Vec<(NetPlayerId, Ray)> = (0..queries)
            .map(|_| {
                let shooter = NetPlayerId {
                    peer: PeerId(1 + (rng.next_u64() % u64::from(peers)) as u32),
                    local: 0,
                };
                let origin = Vec3::new(
                    rng.next_f64() as f32 * 100.0 - 50.0,
                    1.0,
                    rng.next_f64() as f32 * 100.0 - 50.0,
                );
                let dir = Vec3::new(rng.next_f64() as f32 - 0.5, 0.0, rng.next_f64() as f32 - 0.5);
                (shooter, Ray::new(origin, dir))
            })
            .collect();
        let t0 = Instant::now();
        let Ok(lc) = LagCompensation::new(&world) else {
            eprintln!("not a server");
            return;
        };
        let filter = HitFilter::default();
        for (shooter, ray) in &rays {
            if let Ok(Some(_)) = lc.raycast(*shooter, *ray, 150.0, &filter) {
                hits += 1;
            }
        }
        query_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let (rm, rmed, rw) = stats(&mut record_ms);
    let (qm, qmed, qw) = stats(&mut query_ms);
    let h = world.resource::<LagCompHistory>();
    println!(
        "net_lagcomp_bench: {entities} entities x {shapes} shapes, {queries} raycasts/tick, {ticks} ticks, \
         {} ticks kept",
        h.ticks_kept()
    );
    println!("record per tick:  mean {rm:.4} ms  median {rmed:.4} ms  worst {rw:.4} ms  (budget 0.1 ms)");
    println!("queries per tick: mean {qm:.4} ms  median {qmed:.4} ms  worst {qw:.4} ms  (budget 0.3 ms)");
    println!("hits: {hits}");
}
