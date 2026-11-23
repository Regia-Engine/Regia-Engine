// SPDX-License-Identifier: MPL-2.0
//! The two state wires side by side on a conditioned link (spec 72 § 4, ADR-1950): a server and a
//! client over the real endpoints on the in-memory network, with the outbound network conditioner
//! on the server. N entities change every tick and each holds the tick of its last change, so the
//! client can say how old its copy is. Prints the staleness of the copies in ticks (mean, 99th
//! percentile, worst) and the bytes the server put on the link. Time is a step counter at 60 Hz;
//! nothing sleeps, and the same arguments give the same numbers.
//!
//! `cargo run --release -p regia_net --example net_state_bench -- [profile] [entities] [seconds]`
//! (defaults wan_stress, 64, 30; profiles: `ConditionerConfig::from_profile`).

use std::net::SocketAddr;

use regia_ecs::prelude::*;
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::conditioner::ConditionerConfig;
use regia_net::replication::register_replicated;
use regia_net::runtime::{NetEntities, run_net_tick};
use regia_net::*;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "bench.state";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Changed(u32);

fn setup(world: &mut World, mode: NetMode, channel: StateChannel) {
    world.insert_resource(mode);
    let mut settings = NetSettings::default();
    settings.replication.state_channel = channel;
    world.insert_resource(settings);
    register_replicated::<Changed>(world, ReplicateOptions::default());
    regia_net::runtime::ensure_resources(world);
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn run(profile: &str, channel: StateChannel, entities: usize, seconds: u64) {
    let conditioner =
        ConditionerConfig::from_profile(profile).unwrap_or_else(|| panic!("no profile {profile}"));
    let net = LoopbackNet::new();
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        net.bind(addr(7000)).expect("bind"),
        ServerConfig {
            conditioner,
            ..ServerConfig::default()
        },
        [3; 32],
    );
    let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
        net.bind(addr(0)).expect("bind"),
        addr(7000),
        ClientConfig::default(),
        &(),
    )
    .expect("join fits");
    let (mut sw, mut cw) = (World::new(), World::new());
    setup(&mut sw, NetMode::DedicatedServer, channel);
    setup(&mut cw, NetMode::Client, channel);
    let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
    let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
    let rep = Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    };
    let ents: Vec<Entity> = (0..entities)
        .map(|_| sw.spawn((rep.clone(), Changed(0))).id())
        .collect();
    let mut stale: Vec<u32> = Vec::new();
    let steps = seconds * 60;
    let warm = 180;
    for step in 0..steps {
        let now = 1000 + step * 1000 / 60;
        for e in &ents {
            if let Some(mut c) = sw.get_mut::<Changed>(*e) {
                c.0 = step as u32;
            }
        }
        server.receive(now);
        sb.pump_in(&mut sw, &mut server);
        run_net_tick(&mut sw, &mut gs);
        sb.pump_out(&mut sw, &mut server);
        server.flush(now);
        client.receive(now);
        cb.pump_in(&mut cw, &mut client);
        run_net_tick(&mut cw, &mut gc);
        cb.pump_out(&mut cw, &mut client);
        client.flush(now);
        if step < warm {
            continue;
        }
        for e in &ents {
            let id = sw.get::<NetId>(*e).copied();
            let copy = id.and_then(|id| cw.resource::<NetEntities>().entity(id));
            let held = copy.and_then(|c| cw.get::<Changed>(c)).map_or(0, |c| c.0);
            stale.push((step as u32).saturating_sub(held));
        }
    }
    stale.sort_unstable();
    let mean = stale.iter().map(|s| f64::from(*s)).sum::<f64>() / stale.len().max(1) as f64;
    let p99 = stale.get(stale.len() * 99 / 100).copied().unwrap_or(0);
    let worst = stale.last().copied().unwrap_or(0);
    let wire = match channel {
        StateChannel::Unreliable => "acked_deltas",
        StateChannel::Reliable => "reliable_stream",
    };
    let sent: u64 = server.peer_rows().iter().map(|p| p.link.bytes_sent).sum();
    println!(
        "{profile:>10} {wire:>16}: staleness mean {mean:.1} ticks, p99 {p99}, worst {worst}; {:.1} KiB/s out",
        sent as f64 / 1024.0 / seconds as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let profile = args.get(1).map_or("wan_stress", String::as_str);
    let entities = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(64);
    let seconds = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(30);
    for channel in [StateChannel::Reliable, StateChannel::Unreliable] {
        run(profile, channel, entities, seconds);
    }
}
