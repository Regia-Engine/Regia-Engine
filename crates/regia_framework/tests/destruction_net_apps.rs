// SPDX-License-Identifier: MPL-2.0
//! Destruction over the network, end to end (ADR-1967): whole apps with rapier and `NetPlugin`, and
//! the real reliable transport (`NetServer` / `NetClient` over the in-memory datagram network, with
//! a seeded conditioner). Time is a step counter; nothing sleeps.

use std::net::SocketAddr;
use std::sync::Arc;

use regia_app::{App, enable_fixed_step};
use regia_ecs::ecs::message::Messages;
use regia_ecs::prelude::*;
use regia_framework::destruction_net::{
    DestructionNet, DestructionNetPlugin, destruction_net_client, destruction_net_server,
    install_destruction_net,
};
use regia_net::bridge::{ClientBridge, NetFrame, ServerBridge};
use regia_net::model::{LocalPeer, NetMode, PeerId};
use regia_net::runtime::{NetNode, ensure_resources, run_net_tick, server_add_peer};
use regia_net::{
    ClientConfig, ConditionerConfig, LoopbackNet, LoopbackSocket, NetClient, NetGame, NetPlugin, NetServer,
    ServerConfig,
};
use regia_physics::PhysicsPlugin;
use regia_physics::destruction::sample::{
    SAMPLE_TICKS, SampleIds, define_sample, sample_assets, sample_hits,
};
use regia_physics::destruction::{
    Authority, BoxGrid, ChunkState, DestructibleAsset, DestructiblePose, DestructibleRoot, Destruction,
    DestructionBroke, DestructionChunk, DestructionFxMessage, DestructionMaterial, DestructionPlugin,
    GridSupport, NavmeshInvalidated, step_destruction,
};

const PINNED_STRUCTURE_HASH: u64 = 0x4c7d_ecfd_e4c7_d135;

fn net_app(mode: NetMode, peer: PeerId) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    let authority = if mode.is_client() {
        Authority::Replica
    } else {
        Authority::Server
    };
    app.add_plugins((
        PhysicsPlugin,
        NetPlugin { mode },
        DestructionPlugin { authority },
        DestructionNetPlugin,
    ));
    app.insert_resource(LocalPeer(Some(peer)));
    let (wall, bridge) = sample_assets().expect("assets");
    let mut net = app.world_mut().resource_mut::<DestructionNet>();
    net.register_asset(wall);
    net.register_asset(bridge);
    app
}

/// One frame on both apps; datagrams cross at once.
fn pump(server: &mut App, client: &mut App) {
    server.update();
    let down = std::mem::take(&mut server.world_mut().resource_mut::<NetNode>().outbox);
    client.world_mut().resource_mut::<NetNode>().inbox.extend(down);
    client.update();
    let up = std::mem::take(&mut client.world_mut().resource_mut::<NetNode>().outbox);
    let mut node = server.world_mut().resource_mut::<NetNode>();
    node.inbox.extend(up.into_iter().map(|mut d| {
        d.peer = PeerId(1);
        d
    }));
}

fn hits(app: &mut App, ids: SampleIds) {
    let mut d = app.world_mut().resource_mut::<Destruction>();
    let tick = d.world.tick() + 1;
    for hit in sample_hits(tick, ids) {
        d.world.damage(hit).expect("hit");
    }
}

fn count_entities<C: Component>(app: &mut App) -> usize {
    app.world_mut().query::<&C>().iter(app.world()).count()
}

#[test]
fn a_client_app_gets_the_servers_debris_and_loses_the_servers_colliders() {
    let mut server = net_app(NetMode::DedicatedServer, PeerId::SERVER);
    let mut client = net_app(NetMode::Client, PeerId(1));
    server_add_peer(server.world_mut(), PeerId(1));
    let ids = {
        let world = server.world_mut();
        define_sample(&mut world.resource_mut::<Destruction>().world).expect("define")
    };
    pump(&mut server, &mut client);
    pump(&mut server, &mut client);
    for _ in 0..SAMPLE_TICKS {
        hits(&mut server, ids);
        pump(&mut server, &mut client);
    }
    for _ in 0..4 {
        pump(&mut server, &mut client);
    }
    let (s, c) = (
        server.world().resource::<Destruction>(),
        client.world().resource::<Destruction>(),
    );
    assert_eq!(s.world.structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(c.world.structure_hash(), PINNED_STRUCTURE_HASH);
    assert!(c.physics.stats().spawned_total > 0, "{:?}", c.physics.stats());
    assert_eq!(c.physics.stats().spawned_total, s.physics.stats().spawned_total);
    assert_eq!(
        c.physics.stats().colliders_removed_total,
        s.physics.stats().colliders_removed_total
    );
    for (id, inst) in c.world.instances() {
        for (chunk, state) in inst.chunk_states().iter().enumerate() {
            let collider = c.physics.intact_collider(id, chunk as u32);
            assert_eq!(
                collider.is_some(),
                *state == ChunkState::Attached,
                "{id:?} {chunk}"
            );
        }
    }
    assert_eq!(c.message_totals().breaks, s.message_totals().breaks);
    assert_eq!(c.message_totals().fx, s.message_totals().fx);
    let debris = count_entities::<DestructionChunk>(&mut client);
    assert_eq!(debris, count_entities::<DestructionChunk>(&mut server));
    assert!(debris > 0);
    assert_eq!(count_entities::<DestructibleRoot>(&mut client), 2);
}

#[test]
fn a_late_client_app_builds_the_holes_without_any_debris() {
    let mut server = net_app(NetMode::DedicatedServer, PeerId::SERVER);
    let mut client = net_app(NetMode::Client, PeerId(1));
    let ids = {
        let world = server.world_mut();
        define_sample(&mut world.resource_mut::<Destruction>().world).expect("define")
    };
    for _ in 0..SAMPLE_TICKS {
        hits(&mut server, ids);
        server.update();
    }
    server_add_peer(server.world_mut(), PeerId(1));
    for _ in 0..4 {
        pump(&mut server, &mut client);
    }
    let c = client.world().resource::<Destruction>();
    assert_eq!(c.world.structure_hash(), PINNED_STRUCTURE_HASH);
    assert_eq!(
        c.physics.stats().spawned_total,
        0,
        "what fell before the join is gone"
    );
    for (id, inst) in c.world.instances() {
        for (chunk, state) in inst.chunk_states().iter().enumerate() {
            let collider = c.physics.intact_collider(id, chunk as u32);
            assert_eq!(
                collider.is_some(),
                *state == ChunkState::Attached,
                "{id:?} {chunk}"
            );
        }
    }
    assert_eq!(count_entities::<DestructibleRoot>(&mut client), 2);
}

#[test]
fn a_resynced_client_app_has_one_root_per_instance_and_no_stale_debris() {
    let mut server = net_app(NetMode::DedicatedServer, PeerId::SERVER);
    let mut client = net_app(NetMode::Client, PeerId(1));
    server_add_peer(server.world_mut(), PeerId(1));
    let ids = {
        let world = server.world_mut();
        define_sample(&mut world.resource_mut::<Destruction>().world).expect("define")
    };
    for _ in 0..40 {
        hits(&mut server, ids);
        pump(&mut server, &mut client);
    }
    let debris_before = count_entities::<DestructionChunk>(&mut client);
    assert!(debris_before > 0);
    // A local corruption: the next check fails and a snapshot replaces the world.
    let (wall, _) = sample_assets().expect("assets");
    client
        .world_mut()
        .resource_mut::<Destruction>()
        .world
        .define_at(
            regia_physics::destruction::DestructibleId(40),
            wall,
            Default::default(),
            1,
        )
        .expect("define");
    for _ in 40..200 {
        hits(&mut server, ids);
        pump(&mut server, &mut client);
    }
    let c = client.world().resource::<Destruction>();
    assert_eq!(c.world.structure_hash(), PINNED_STRUCTURE_HASH);
    assert!(
        client
            .world()
            .resource::<DestructionNet>()
            .stats()
            .snapshots_applied
            >= 2
    );
    assert_eq!(count_entities::<DestructibleRoot>(&mut client), 2);
    let debris = {
        let c = client.world().resource::<Destruction>();
        c.physics.active().count()
    };
    assert_eq!(debris, count_entities::<DestructionChunk>(&mut client));
}

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.destruction";
    const PROTOCOL_VERSION: u32 = 1;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn endpoint_world(mode: NetMode) -> World {
    let mut world = World::new();
    world.insert_resource(mode);
    install_destruction_net(&mut world);
    ensure_resources(&mut world);
    let authority = if mode.is_client() {
        Authority::Replica
    } else {
        Authority::Server
    };
    world.insert_resource(Destruction::new(authority));
    world.insert_resource(regia_app::FixedTime::default());
    world.init_resource::<Messages<DestructionBroke>>();
    world.init_resource::<Messages<DestructionFxMessage>>();
    world.init_resource::<Messages<NavmeshInvalidated>>();
    let (wall, bridge) = sample_assets().expect("assets");
    let mut net = world.resource_mut::<DestructionNet>();
    net.register_asset(wall);
    net.register_asset(bridge);
    world
}

#[test]
fn the_real_transport_carries_a_script_through_loss_and_reordering() {
    for seed in 0..4u64 {
        let net = LoopbackNet::new();
        let lossy = ConditionerConfig {
            enabled: true,
            seed: 100 + seed,
            loss: 0.15,
            duplicate: 0.05,
            delay_ms: 40,
            jitter_ms: 20,
            reorder: 0.1,
            ..ConditionerConfig::default()
        };
        let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
            net.bind(addr(7100)).expect("bind"),
            ServerConfig {
                conditioner: lossy.clone(),
                ..ServerConfig::default()
            },
            [5; 32],
        );
        let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
            net.bind(addr(0)).expect("bind"),
            addr(7100),
            ClientConfig {
                conditioner: lossy,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("join fits");
        let (mut sw, mut cw) = (
            endpoint_world(NetMode::DedicatedServer),
            endpoint_world(NetMode::Client),
        );
        let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
        let chain = (destruction_net_client, step_destruction, destruction_net_server).chain();
        let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
        gs.add_systems(chain);
        gc.add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
        let ids = define_sample(&mut sw.resource_mut::<Destruction>().world).expect("define");
        let mut now = 0u64;
        let mut joined = 0;
        for step in 0..1600u64 {
            now += 16;
            server.receive(now);
            sb.pump_in(&mut sw, &mut server);
            // The script starts once the client is in, so it sees it live.
            let peers = sw.resource::<NetNode>().peers().len();
            if peers > 0 && joined == 0 {
                joined = step;
            }
            if peers > 0 && step < joined + SAMPLE_TICKS + 30 && step > joined + 20 {
                let mut d = sw.resource_mut::<Destruction>();
                let tick = d.world.tick() + 1;
                for hit in sample_hits(tick, ids) {
                    d.world.damage(hit).expect("hit");
                }
            }
            run_net_tick(&mut sw, &mut gs);
            sb.pump_out(&mut sw, &mut server);
            server.flush(now);
            client.receive(now);
            cb.pump_in(&mut cw, &mut client);
            run_net_tick(&mut cw, &mut gc);
            cb.pump_out(&mut cw, &mut client);
            client.flush(now);
        }
        assert!(joined > 0, "seed {seed}: the client joined");
        let (s, c) = (
            sw.resource::<Destruction>().world.structure_hash(),
            cw.resource::<Destruction>().world.structure_hash(),
        );
        assert_eq!(s, c, "seed {seed}");
        let stats = cw.resource::<DestructionNet>().stats().clone();
        assert_eq!(stats.checks_failed, 0, "seed {seed}: {stats:?}");
        assert!(
            stats.breaks_applied > 0 || stats.snapshots_applied > 0,
            "seed {seed}: {stats:?}"
        );
    }
}

#[test]
fn the_real_transport_carries_a_snapshot_past_one_ticks_queue() {
    const CROWD: usize = 30_000;
    let pebble = Arc::new(
        DestructibleAsset::box_grid(&BoxGrid {
            name: "pebble".into(),
            counts: [2, 1, 1],
            cell: [0.5, 0.5, 0.5],
            density: 2000.0,
            strength_per_area: 200.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid"),
    );
    let net = LoopbackNet::new();
    let lossy = ConditionerConfig {
        enabled: true,
        seed: 300,
        loss: 0.05,
        duplicate: 0.02,
        delay_ms: 40,
        jitter_ms: 20,
        reorder: 0.05,
        ..ConditionerConfig::default()
    };
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        net.bind(addr(7200)).expect("bind"),
        ServerConfig {
            conditioner: lossy.clone(),
            ..ServerConfig::default()
        },
        [6; 32],
    );
    let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
        net.bind(addr(0)).expect("bind"),
        addr(7200),
        ClientConfig {
            conditioner: lossy,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("join fits");
    let (mut sw, mut cw) = (
        endpoint_world(NetMode::DedicatedServer),
        endpoint_world(NetMode::Client),
    );
    cw.resource_mut::<DestructionNet>().register_asset(pebble.clone());
    let (mut sb, mut cb) = (ServerBridge::new(), ClientBridge::new());
    let (mut gs, mut gc) = (Schedule::default(), Schedule::default());
    gs.add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
    gc.add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
    let ids = define_sample(&mut sw.resource_mut::<Destruction>().world).expect("define");
    {
        let mut d = sw.resource_mut::<Destruction>();
        for i in 0..CROWD {
            let pose = DestructiblePose {
                translation: [(i % 200) as f32, 40.0, (i / 200) as f32],
                rotation: [0.0, 0.0, 0.0, 1.0],
            };
            d.world.define(pebble.clone(), pose, i as u64).expect("define");
        }
    }
    let mut now = 0u64;
    let mut joined = None;
    let mut most_in_progress = 0;
    for step in 0..2400u64 {
        now += 16;
        server.receive(now);
        sb.pump_in(&mut sw, &mut server);
        let peers = sw.resource::<NetNode>().peers().len();
        if peers > 0 && joined.is_none() {
            joined = Some(step);
        }
        // Breaks all through the send, so the stream runs behind the snapshot.
        if let Some(joined) = joined
            && step < joined + SAMPLE_TICKS
        {
            let mut d = sw.resource_mut::<Destruction>();
            let tick = d.world.tick() + 1;
            for hit in sample_hits(tick, ids) {
                d.world.damage(hit).expect("hit");
            }
        }
        run_net_tick(&mut sw, &mut gs);
        sb.pump_out(&mut sw, &mut server);
        server.flush(now);
        client.receive(now);
        cb.pump_in(&mut cw, &mut client);
        run_net_tick(&mut cw, &mut gc);
        cb.pump_out(&mut cw, &mut client);
        client.flush(now);
        most_in_progress =
            most_in_progress.max(sw.resource::<DestructionNet>().stats().snapshots_in_progress);
    }
    assert!(joined.is_some(), "the client joined");
    assert_eq!(most_in_progress, 1, "the snapshot took more than one tick");
    let server_stats = sw.resource::<DestructionNet>().stats().clone();
    assert_eq!(server_stats.peers, 1, "the joiner stayed: {server_stats:?}");
    let stats = cw.resource::<DestructionNet>().stats().clone();
    assert_eq!(stats.snapshots_applied, 1, "{stats:?}");
    assert_eq!(stats.resyncs_requested, 0, "{stats:?}");
    assert_eq!(stats.checks_failed, 0, "{stats:?}");
    let (s, c) = (
        sw.resource::<Destruction>().world.structure_hash(),
        cw.resource::<Destruction>().world.structure_hash(),
    );
    assert_eq!(s, c);
    assert_eq!(cw.resource::<Destruction>().world.instances().count(), CROWD + 2);
}

#[test]
fn the_plugin_orders_its_systems_around_the_net_sets() {
    // Builds and runs the whole schedule once: an ordering cycle would panic here.
    let mut app = net_app(NetMode::ListenServer, PeerId::SERVER);
    app.update();
    app.update();
    assert!(app.world().contains_resource::<DestructionNet>());
}
