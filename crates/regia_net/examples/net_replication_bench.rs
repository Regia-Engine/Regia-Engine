// SPDX-License-Identifier: MPL-2.0
//! Replication cost per tick (spec 71 § 15): server change gathering for N replicated entities
//! with 5 registered components and 10 % changed per tick, and the client apply of those deltas,
//! over the in-memory hub (no sockets).
//!
//! `cargo run --release -p regia_net --example net_replication_bench -- [entities] [clients] [ticks]`
//! `[change_every]` (defaults 10000, 1, 120, 10: one entity in ten changes per tick; 0 = none). Prints the mean and worst time of the server's gather + send and of one
//! client's apply.

use std::time::Instant;

use regia_ecs::prelude::*;
use regia_net::replication::register_replicated;
use regia_net::runtime::{NetNode, net_apply, net_receive, net_send};
use regia_net::testing::LoopbackHub;
use regia_net::*;

macro_rules! comps {
    ($($name:ident),*) => {$(
        #[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
        struct $name {
            a: u32,
            b: f32,
        }
    )*};
}
comps!(C0, C1, C2, C3, C4);

fn setup(world: &mut World) {
    register_replicated::<C0>(world, ReplicateOptions::default());
    register_replicated::<C1>(world, ReplicateOptions::default());
    register_replicated::<C2>(world, ReplicateOptions::default());
    register_replicated::<C3>(world, ReplicateOptions::default());
    register_replicated::<C4>(world, ReplicateOptions::default());
}

fn arg(i: usize, default: usize) -> usize {
    std::env::args()
        .nth(i)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn stats(samples: &[f64]) -> (f64, f64) {
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    let worst = samples.iter().copied().fold(0.0, f64::max);
    (mean, worst)
}

fn main() {
    let entities = arg(1, 10_000);
    let clients = arg(2, 1).max(1);
    let ticks = arg(3, 120).max(2);
    let step = arg(4, 10);
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    for _ in 0..clients {
        hub.add_client();
    }
    let rep = Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    };
    let ids: Vec<Entity> = (0..entities)
        .map(|i| {
            let v = i as u32;
            hub.server
                .world
                .spawn((
                    rep.clone(),
                    C0 { a: v, b: 0.0 },
                    C1 { a: v, b: 1.0 },
                    C2 { a: v, b: 2.0 },
                    C3 { a: v, b: 3.0 },
                    C4 { a: v, b: 4.0 },
                ))
                .id()
        })
        .collect();
    // Spawn everything and let the copies settle.
    hub.run(4);
    let mut gather = Vec::new();
    let mut apply = Vec::new();
    let mut send_only = Vec::new();
    // Where the gather goes (`ServerReplication::timing`): changes, relevance, send.
    let mut phases = [Vec::new(), Vec::new(), Vec::new()];
    for t in 0..ticks {
        // One in `step` entities changes one component each tick (default 10 %; 0 = none).
        if step == 0 {
            // No changes: the fixed per-entity cost only.
        } else {
            for (i, e) in ids.iter().enumerate().skip(t % step).step_by(step) {
                if let Some(mut c) = hub.server.world.get_mut::<C2>(*e) {
                    c.a = c.a.wrapping_add(1 + i as u32);
                }
            }
        }
        // Server tick, timed: receive + apply + send (gather included).
        let w = &mut hub.server.world;
        let start = Instant::now();
        net_receive(w);
        net_apply(w);
        let mid = Instant::now();
        net_send(w);
        gather.push(start.elapsed().as_secs_f64() * 1000.0);
        send_only.push(mid.elapsed().as_secs_f64() * 1000.0);
        let t = w.resource::<regia_net::replication::ServerReplication>().timing;
        phases[0].push(t.changes_ms);
        phases[1].push(t.relevance_ms);
        phases[2].push(t.send_ms);
        let out = std::mem::take(&mut w.resource_mut::<NetNode>().outbox);
        // Hand the frames to the first client directly and time its apply.
        let peer = hub.clients[0].peer;
        let cw = &mut hub.clients[0].world;
        cw.resource_mut::<NetNode>()
            .inbox
            .extend(out.into_iter().filter(|d| d.peer == peer).map(|mut d| {
                d.peer = PeerId::SERVER;
                d
            }));
        let start = Instant::now();
        net_receive(cw);
        net_apply(cw);
        apply.push(start.elapsed().as_secs_f64() * 1000.0);
        net_send(cw);
        cw.resource_mut::<NetNode>().outbox.clear();
    }
    let (gm, gw) = stats(&gather);
    let (am, aw) = stats(&apply);
    let (sm, _) = stats(&send_only);
    let (changes, _) = stats(&phases[0]);
    let (relevance, _) = stats(&phases[1]);
    let (send, _) = stats(&phases[2]);
    println!("gather phases (mean): changes {changes:.3} ms, relevance {relevance:.3} ms, send {send:.3} ms");
    println!(
        "entities {entities}, components 5, one in {step} changed per tick, clients {clients}, ticks {ticks}\n\
         server receive+apply+gather+send: mean {gm:.3} ms, worst {gw:.3} ms (gather+send {sm:.3} ms)\n\
         client receive+apply ({} deltas per tick): mean {am:.3} ms, worst {aw:.3} ms",
        entities / 10
    );
}
