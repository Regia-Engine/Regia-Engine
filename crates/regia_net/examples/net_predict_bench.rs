// SPDX-License-Identifier: MPL-2.0
//! Spec 73 § 14 scale: the server's input handling for 64 predicting players (receive, buffer,
//! consume; span `net_input`).
//!
//! ```text
//! cargo run --release -p regia_net --example net_predict_bench -- [players] [ticks]
//! ```
//!
//! The input frames are captured from a real hub run (6-tick latency each way) and replayed into a
//! fresh server world, so the server number is the input path alone.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use regia_ecs::prelude::*;
use regia_net::predict::{ClientPrediction, set_transport_rtt};
use regia_net::replication::NetPredicted;
use regia_net::runtime::{Datagram, NetNode, net_apply, net_receive, server_add_peer};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::transform::Transform;

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jump: bool,
}

impl NetInput for Pad {}

/// A predicted body: its transform, owner and role.
type Body = (
    &'static mut Transform,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);

fn step(
    cx: PredictionCx,
    inputs: Query<(&NetPlayer, &InputCommand<Pad>)>,
    mut q: Query<Body, With<NetPredicted>>,
) {
    for (mut t, owner, role) in &mut q {
        if !cx.simulates(role) {
            continue;
        }
        let Some(p) = cx.owner_player(owner) else {
            continue;
        };
        let x = inputs
            .iter()
            .find(|(n, _)| n.id == p)
            .map_or(0, |(_, c)| c.value.x);
        t.translation.x += f32::from(x) * 0.1;
    }
}

fn setup(world: &mut World) {
    if add_net_input::<Pad>(world).is_err() {
        return;
    }
    let _ = predict::<Transform>(world, Tolerance::position(0.01));
    add_predicted_systems(world, step);
}

#[derive(Resource)]
struct Walk;

fn feed(cp: Res<ClientPrediction>, mut q: Query<&mut InputCommand<Pad>>) {
    let t = cp.client_tick().0;
    for mut c in &mut q {
        c.value = Pad {
            x: if (t / 40).is_multiple_of(2) { 1 } else { -1 },
            jump: t.is_multiple_of(90),
        };
    }
}

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let players = args.first().copied().unwrap_or(64);
    let ticks = args.get(1).copied().unwrap_or(600);

    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
    hub.down.delay_ticks = 6;
    hub.up.delay_ticks = 6;
    for _ in 0..players {
        let c = hub.add_client();
        set_transport_rtt(&mut hub.clients[c].world, 13.0 * 1000.0 / 60.0, 0.0);
        hub.clients[c].world.insert_resource(Walk);
        hub.clients[c].gameplay.add_systems(feed);
        let peer = hub.clients[c].peer;
        hub.server.world.spawn((
            Replicated::default(),
            Transform::default(),
            NetPredicted,
            NetOwner(peer),
        ));
    }
    let captured: Arc<Mutex<Vec<(u64, Datagram)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    hub.up_drop = Some(Box::new(move |step, _, d| {
        if d.bytes.first() == Some(&5)
            && let Ok(mut v) = sink.lock()
        {
            v.push((step, d.clone()));
        }
        false
    }));
    hub.run(ticks + 60);
    let corrections = hub.clients[0]
        .world
        .resource::<ClientPrediction>()
        .stats
        .corrections;

    // Server: replay the captured input frames into a fresh server, by step.
    let mut server = World::new();
    server.insert_resource(NetMode::DedicatedServer);
    server.insert_resource(regia_net::model::LocalPeer(Some(PeerId::SERVER)));
    setup(&mut server);
    regia_net::runtime::ensure_resources(&mut server);
    for p in 1..=players as u32 {
        server_add_peer(&mut server, PeerId(p));
    }
    let frames = captured.lock().map(|v| v.clone()).unwrap_or_default();
    let last = frames.last().map_or(0, |(s, _)| *s);
    let mut server_ns: Vec<u128> = Vec::new();
    let mut i = 0;
    for step in 1..=last {
        while let Some((s, d)) = frames.get(i) {
            if *s != step {
                break;
            }
            server.resource_mut::<NetNode>().inbox.push(d.clone());
            i += 1;
        }
        let start = Instant::now();
        net_receive(&mut server);
        net_apply(&mut server);
        server_ns.push(start.elapsed().as_nanos());
        let mut node = server.resource_mut::<NetNode>();
        node.tick = node.tick.next();
    }
    let stats = |v: &mut Vec<u128>| {
        v.sort_unstable();
        let mean = v.iter().sum::<u128>() as f64 / v.len().max(1) as f64 / 1e6;
        let p99 = v.get(v.len() * 99 / 100).copied().unwrap_or(0) as f64 / 1e6;
        (mean, p99)
    };
    let (sm, sp) = stats(&mut server_ns);
    println!("players {players}, ticks {ticks}, client corrections {corrections}");
    println!(
        "server input path (receive + apply, {} frames): mean {sm:.4} ms, p99 {sp:.4} ms per tick",
        frames.len()
    );
}
