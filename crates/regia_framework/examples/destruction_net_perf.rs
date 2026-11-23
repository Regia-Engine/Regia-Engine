// SPDX-License-Identifier: MPL-2.0
//! What destruction over the network costs on a big structure: a 64 x 64 grid (4096 chunks) blown
//! apart with `clients` replicas on the in-memory hub.
//!
//! ```text
//! cargo run -p regia_framework --features net --example destruction_net_perf -- [clients] [relevance]
//! ```
//!
//! Prints the ticks a replica took to catch up, the bytes one client received (frames, as sent)
//! and the time of a hub step for all processes together, the size of a late joiner's first frames,
//! and the time of a quiet tick. Timings are informational; no test depends on them.
//!
//! With `relevance` the grid gets a relevance anchor (ADR-2305): client 0 stands by it, the others
//! 1 km away, and the line also prints the bytes the last (far) client received.

use std::sync::Arc;
use std::time::Instant;

use regia_app::FixedTime;
use regia_ecs::ecs::message::Messages;
use regia_ecs::prelude::*;
use regia_framework::destruction_net::{
    DestructionNet, destruction_net_client, destruction_net_server, install_destruction_net,
};
use regia_net::model::NetMode;
use regia_net::testing::{LoopbackHub, NetProcess};
use regia_physics::destruction::{
    Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructiblePose, Destruction,
    DestructionBroke, DestructionFxMessage, DestructionMaterial, GridSupport, NavmeshInvalidated,
    step_destruction,
};

fn grid() -> Option<Arc<DestructibleAsset>> {
    DestructibleAsset::box_grid(&BoxGrid {
        name: "perf".into(),
        counts: [64, 64, 1],
        cell: [0.5, 0.5, 0.25],
        density: 2000.0,
        strength_per_area: 100.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .ok()
    .map(Arc::new)
}

fn setup(world: &mut World) {
    let client = world.resource::<NetMode>().is_client();
    install_destruction_net(world);
    world.insert_resource(Destruction::new(if client {
        Authority::Replica
    } else {
        Authority::Server
    }));
    world.insert_resource(FixedTime::default());
    world.init_resource::<Messages<DestructionBroke>>();
    world.init_resource::<Messages<DestructionFxMessage>>();
    world.init_resource::<Messages<NavmeshInvalidated>>();
    if let Some(asset) = grid() {
        world.resource_mut::<DestructionNet>().register_asset(asset);
    }
}

/// `setup`, plus spatial relevance on the server: anchors within 50 m of a viewpoint are held.
fn setup_relevant(world: &mut World) {
    setup(world);
    if world.resource::<NetMode>().is_server() {
        regia_net::relevance::install_net_interest(world);
        let near = regia_net::profile::NetProfile {
            relevance: regia_net::profile::RelevanceShape::Spatial {
                radius: 50.0,
                hysteresis: 0.1,
            },
            ..regia_net::profile::NetProfile::default()
        };
        if let Err(e) = world
            .resource_mut::<regia_net::profile::NetProfiles>()
            .insert("near", near)
        {
            eprintln!("profile refused: {e}");
        }
    }
}

fn at_x(x: f32) -> regia_scene::Transform {
    regia_scene::Transform {
        translation: regia_core::math::Vec3::new(x, 0.0, 0.0),
        ..regia_scene::Transform::default()
    }
}

fn schedule(p: &mut NetProcess) {
    p.gameplay
        .add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
}

fn received(hub: &LoopbackHub, client: usize) -> usize {
    hub.client_logs[client].iter().map(|(_, d)| d.bytes.len()).sum()
}

fn main() {
    let clients = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8usize);
    let relevance = std::env::args().nth(2).as_deref() == Some("relevance");
    let Some(asset) = grid() else {
        eprintln!("the grid does not build");
        return;
    };
    let mut hub = LoopbackHub::new(
        NetMode::DedicatedServer,
        1,
        if relevance { setup_relevant } else { setup },
    );
    schedule(&mut hub.server);
    for _ in 0..clients {
        let i = hub.add_client();
        schedule(&mut hub.clients[i]);
    }
    let defined =
        hub.server
            .world
            .resource_mut::<Destruction>()
            .world
            .define(asset, DestructiblePose::default(), 1);
    let Ok(id) = defined else {
        eprintln!("define failed");
        return;
    };
    if relevance {
        let anchor = hub
            .server
            .world
            .spawn((
                regia_net::replication::Replicated {
                    transform: regia_net::replication::TransformReplication::None,
                    ..regia_net::replication::Replicated::default()
                },
                at_x(0.0),
                regia_net::profile::NetProfileRef("near".into()),
            ))
            .id();
        hub.server
            .world
            .resource_mut::<DestructionNet>()
            .set_relevance_anchor(id, anchor);
        for (i, client) in hub.clients.iter().enumerate() {
            let peer = client.peer;
            hub.server.world.spawn((
                regia_net::relevance::NetViewpoint {
                    player: regia_net::model::NetPlayerId { peer, local: 0 },
                },
                at_x(if i == 0 { 0.0 } else { 1000.0 }),
            ));
        }
    }
    let far = clients.saturating_sub(1);
    hub.run(3);
    let quiet = Instant::now();
    hub.run(600);
    let quiet_us = quiet.elapsed().as_micros() as f64 / 600.0;
    let before = received(&hub, 0);
    let far_before = received(&hub, far);
    let hit = Damage {
        target: None,
        kind: DamageKind::Explosion {
            center: [0.0, 16.0, 0.0],
            radius: 40.0,
            impulse: 40_000.0,
        },
    };
    if hub
        .server
        .world
        .resource_mut::<Destruction>()
        .world
        .damage(hit)
        .is_err()
    {
        eprintln!("the blast was refused");
        return;
    }
    let (mut ticks, mut worst, mut total) = (0u32, 0u128, 0u128);
    let mut seen = (0u64, 0u32);
    loop {
        let t = Instant::now();
        hub.step();
        let us = t.elapsed().as_micros();
        worst = worst.max(us);
        total += us;
        ticks += 1;
        let server = &hub.server.world.resource::<Destruction>().world;
        let replica = &hub.clients[0].world.resource::<Destruction>().world;
        let queued = hub.server.world.resource::<DestructionNet>().stats().queued_ops;
        // The per-tick break cap spreads the decisions over ticks too: done is two quiet ticks in a row.
        let now = server.event_count();
        let settled = now == seen.0 && queued == 0 && replica.event_count() == now;
        seen = (now, if settled { seen.1 + 1 } else { 0 });
        if seen.1 >= 2 || ticks >= 20_000 {
            break;
        }
    }
    hub.run(4);
    let bytes = received(&hub, 0) - before;
    let far_bytes = received(&hub, far) - far_before;
    let world = &hub.server.world.resource::<Destruction>().world;
    let events = world.event_count();
    let same = world.structure_hash()
        == hub.clients[0]
            .world
            .resource::<Destruction>()
            .world
            .structure_hash();
    let late = hub.add_client();
    schedule(&mut hub.clients[late]);
    hub.run(4);
    println!(
        "DESTRUCTION_NET_PERF clients={clients} events={events} replica_same={same} ticks_to_catch_up={ticks} \
         bytes_one_client={bytes} bytes_per_event={:.1} step_mean_us={} step_worst_us={worst} quiet_step_us={quiet_us:.1} \
         late_joiner_bytes={} relevance={relevance} bytes_far_client={far_bytes}",
        bytes as f64 / events.max(1) as f64,
        total / u128::from(ticks.max(1)),
        received(&hub, late),
    );
}
