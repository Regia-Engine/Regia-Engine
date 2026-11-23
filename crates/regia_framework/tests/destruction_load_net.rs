// SPDX-License-Identifier: MPL-2.0
//! Structural load over the network (ADR-2734): the server decides load breaks, two clients replay
//! them through `DestructionNetPlugin`'s stream over `LoopbackHub` (no sockets, fixed steps) and end
//! with the server's broken bonds and structure hash. No new message: the same break stream.

use std::sync::Arc;

use regia_app::FixedTime;
use regia_ecs::ecs::message::Messages;
use regia_ecs::prelude::*;
use regia_framework::destruction_net::{
    DestructionNet, destruction_net_client, destruction_net_server, install_destruction_net,
};
use regia_net::model::NetMode;
use regia_net::testing::{LoopbackHub, NetProcess};
use regia_physics::destruction::{
    Authority, Damage, DamageKind, DestructibleAsset, DestructibleBond, DestructibleChunk, DestructiblePose,
    Destruction, DestructionBroke, DestructionFxMessage, DestructionMaterial, LoadSettings,
    NavmeshInvalidated, step_destruction,
};

const W: f32 = 100.0 * 9.81;

/// A table: a 5-cell top on two 2-cell legs (feet held by the ground), with load on.
fn table() -> Arc<DestructibleAsset> {
    let at: [(i32, i32); 9] = [
        (0, 0),
        (0, 1),
        (4, 0),
        (4, 1),
        (0, 2),
        (1, 2),
        (2, 2),
        (3, 2),
        (4, 2),
    ];
    let hull: Vec<[f32; 3]> = (0..8u32)
        .map(|c| {
            [
                if c & 1 == 0 { -0.5 } else { 0.5 },
                if c & 2 == 0 { -0.5 } else { 0.5 },
                if c & 4 == 0 { -0.5 } else { 0.5 },
            ]
        })
        .collect();
    let chunks = at
        .iter()
        .enumerate()
        .map(|(i, &(x, y))| {
            let c = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
            DestructibleChunk {
                name: format!("t_{i}"),
                parent: None,
                centroid: c,
                mass: 100.0,
                inertia: None,
                hulls: vec![hull.clone()],
                bounds_min: [c[0] - 0.5, c[1] - 0.5, -0.5],
                bounds_max: [c[0] + 0.5, c[1] + 0.5, 0.5],
                support: i == 0 || i == 2,
                lods: Vec::new(),
                fx_tag: None,
                small: false,
                lifetime_secs: None,
                edge_heat: None,
            }
        })
        .collect();
    let mut bonds = Vec::new();
    for (i, &(xi, yi)) in at.iter().enumerate() {
        for (j, &(xj, yj)) in at.iter().enumerate().skip(i + 1) {
            if (xi - xj).abs() + (yi - yj).abs() == 1 {
                bonds.push(DestructibleBond {
                    a: i as u32,
                    b: j as u32,
                    area: 1.0,
                    anchor: [(xi + xj) as f32 * 0.5 + 0.5, (yi + yj) as f32 * 0.5 + 0.5, 0.0],
                    normal: [(xj - xi) as f32, (yj - yi) as f32, 0.0],
                    break_impulse: 100.0,
                    max_load: None,
                });
            }
        }
    }
    let asset = DestructibleAsset {
        format_version: regia_physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
        name: "table".into(),
        source: None,
        pattern_id: None,
        fracture_seed: None,
        replaces_stage: None,
        meshes: None,
        outer_material: None,
        inner_material: None,
        material: DestructionMaterial::default(),
        chunks,
        bonds,
        load: Some(LoadSettings {
            max_load_per_area: 4.0 * W,
            ..LoadSettings::default()
        }),
        falling: None,
    };
    asset.validate().expect("table");
    Arc::new(asset)
}

fn setup(world: &mut World) {
    let mode = *world.resource::<NetMode>();
    install_destruction_net(world);
    let authority = if mode.is_client() {
        Authority::Replica
    } else {
        Authority::Server
    };
    world.insert_resource(Destruction::new(authority));
    world.insert_resource(FixedTime::default());
    world.init_resource::<Messages<DestructionBroke>>();
    world.init_resource::<Messages<DestructionFxMessage>>();
    world.init_resource::<Messages<NavmeshInvalidated>>();
    world.resource_mut::<DestructionNet>().register_asset(table());
}

fn schedule(p: &mut NetProcess) {
    p.gameplay
        .add_systems((destruction_net_client, step_destruction, destruction_net_server).chain());
}

fn world_of(p: &NetProcess) -> &regia_physics::destruction::DestructionWorld {
    &p.world.resource::<Destruction>().world
}

#[test]
fn two_peers_replay_the_servers_load_collapse() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    schedule(&mut hub.server);
    for _ in 0..2 {
        let i = hub.add_client();
        schedule(&mut hub.clients[i]);
    }
    let id = hub
        .server
        .world
        .resource_mut::<Destruction>()
        .world
        .define(table(), DestructiblePose::default(), 11)
        .expect("define");
    hub.run(10);
    // Knock the right leg's upper cell out; the rest is load.
    hub.server
        .world
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 3,
                impulse: [1000.0, 0.0, 0.0],
            },
        })
        .expect("kick");
    hub.run(120);
    let server = world_of(&hub.server);
    let inst = server.instance(id).expect("server instance");
    assert_eq!(inst.attached_count(), 2, "the table collapsed on the server");
    assert!(server.load_stats().solves >= 2);
    for c in &hub.clients {
        let client = world_of(c);
        assert_eq!(client.structure_hash(), server.structure_hash());
        assert_eq!(
            client.instance(id).expect("client instance").bond_broken(),
            inst.bond_broken()
        );
        assert_eq!(client.load_stats().solves, 0, "a replica never decides");
    }
}
