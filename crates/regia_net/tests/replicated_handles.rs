// SPDX-License-Identifier: MPL-2.0
//! Spec 71 § 3 (ADR-2415): a replicated component holding a `Handle<T>` travels as the asset's
//! stable key and resolves on the receiver through its own asset-server context; a key the
//! receiver does not know leaves the handle unset and is listed with its code.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_asset::{AssetId, Assets, Handle};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_net::assets::{NetAssetTable, NetAssets};
use regia_net::codec::{ASSET_NO_RESOLVER, ASSET_UNKNOWN, AssetKey, AssetResolveError};
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::testing::LoopbackHub;
use regia_net::*;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mesh(&'static str);

#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Look {
    mesh: Handle<Mesh>,
    tint: u32,
    #[replicate(max_len = 4)]
    extras: Vec<Handle<Mesh>>,
}

fn setup(world: &mut World) {
    register_replicated::<Look>(world, ReplicateOptions::default());
}

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// One process's asset server: a slab plus the table its handles are registered in.
struct Server {
    slab: Assets<Mesh>,
    table: Arc<NetAssetTable>,
}

impl Server {
    fn new() -> Self {
        Self {
            slab: Assets::new(),
            table: Arc::new(NetAssetTable::new()),
        }
    }

    fn load(&mut self, n: u128, name: &'static str) -> Handle<Mesh> {
        let h = self.slab.insert(id(n), Mesh(name));
        self.table.insert_id(&h, id(n));
        h
    }

    fn load_path(&mut self, n: u128, path: &str, name: &'static str) -> Handle<Mesh> {
        let h = self.slab.insert(id(n), Mesh(name));
        self.table.insert_path(&h, path);
        h
    }
}

fn copy_of(hub: &LoopbackHub, client: usize, server_entity: Entity) -> Look {
    let id = *hub.server.world.get::<NetId>(server_entity).expect("replicated");
    let e = hub.clients[client]
        .world
        .resource::<NetEntities>()
        .entity(id)
        .expect("copy spawned");
    hub.clients[client]
        .world
        .get::<Look>(e)
        .cloned()
        .expect("look replicated")
}

#[test]
fn a_handle_component_replicates_between_two_peers() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let a = hub.add_client();

    // The server loads three meshes; padding makes its slots differ from the client's.
    let mut sv = Server::new();
    let _pad = sv.slab.insert(id(99), Mesh("pad"));
    let crate_mesh = sv.load(1, "crate");
    let barrel = sv.load_path(2, "assets/props/barrel.glb", "barrel");
    let only_on_server = sv.load(3, "secret");
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));

    // The client has the crate and the barrel (by path), not the third.
    let mut cl = Server::new();
    let c_crate = cl.load(1, "crate");
    let c_barrel = cl.load_path(2, "assets/props/barrel.glb", "barrel");
    hub.clients[a]
        .world
        .insert_resource(NetAssets::new(cl.table.clone()));
    assert_ne!(crate_mesh.index, c_crate.index, "the slots differ per process");

    let e = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Look {
                mesh: crate_mesh,
                tint: 7,
                extras: vec![barrel, Handle::INVALID],
            },
        ))
        .id();
    hub.run(4);
    let look = copy_of(&hub, a, e);
    assert_eq!(look.mesh, c_crate, "the guid resolved through the client's table");
    assert_eq!(cl.slab.get(&look.mesh), Some(&Mesh("crate")));
    assert_eq!(
        look.extras,
        vec![c_barrel, Handle::INVALID],
        "the path resolved too"
    );
    assert_eq!(look.tint, 7);

    // A change to the handle alone travels as a delta and resolves.
    hub.server.world.get_mut::<Look>(e).expect("look").mesh = barrel;
    hub.run(3);
    assert_eq!(copy_of(&hub, a, e).mesh, c_barrel);

    // A handle to an asset the client does not have: unset there, listed with its code.
    hub.server.world.get_mut::<Look>(e).expect("look").mesh = only_on_server;
    hub.run(3);
    let look = copy_of(&hub, a, e);
    assert!(look.mesh.is_invalid(), "an unknown asset leaves the handle unset");
    assert_eq!(look.tint, 7, "the rest of the component still applies");
    let assets = hub.clients[a].world.resource::<NetAssets>();
    let miss = assets
        .missing_key(&AssetKey::Guid(id(3)))
        .expect("the unknown asset is listed");
    assert_eq!(miss.code, ASSET_UNKNOWN);
    assert_eq!(miss.code, "asset_unknown");
    assert_eq!(assets.missing().count(), 1);
    assert_eq!(hub.server.world.resource::<NetAssets>().unmapped(), 0);
}

#[test]
fn the_receiver_loads_through_its_loader() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 6, setup);
    let a = hub.add_client();
    let mut sv = Server::new();
    let h = sv.load(5, "rock");
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));

    // The client's table starts empty and loads on first sight (as an asset server would).
    let loads = Arc::new(AtomicU32::new(0));
    let seen = loads.clone();
    let table = Arc::new(NetAssetTable::with_loader(move |_ty, key| {
        seen.fetch_add(1, Ordering::Relaxed);
        match key {
            AssetKey::Guid(g) if *g == id(5) => Ok((42, 1)),
            _ => Err(AssetResolveError::Unknown),
        }
    }));
    hub.clients[a].world.insert_resource(NetAssets::new(table));
    let e = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Look {
                mesh: h,
                tint: 1,
                extras: vec![h, h],
            },
        ))
        .id();
    hub.run(4);
    let look = copy_of(&hub, a, e);
    assert_eq!((look.mesh.index, look.mesh.generation), (42, 1));
    assert!(look.extras.iter().all(|x| *x == look.mesh));
    assert_eq!(loads.load(Ordering::Relaxed), 1, "loaded once, then remembered");
}

#[test]
fn without_a_context_handles_are_unset_with_a_code() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
    let a = hub.add_client();
    let mut sv = Server::new();
    let h = sv.load(8, "tree");
    let unmapped = sv.slab.insert(id(9), Mesh("not registered"));
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));
    // The client never installed a resolver.
    let e = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Look {
                mesh: h,
                tint: 2,
                extras: vec![unmapped],
            },
        ))
        .id();
    hub.run(4);
    let look = copy_of(&hub, a, e);
    assert!(look.mesh.is_invalid());
    assert_eq!(look.extras, vec![Handle::INVALID]);
    let assets = hub.clients[a].world.resource::<NetAssets>();
    let miss = assets.missing_key(&AssetKey::Guid(id(8))).expect("listed");
    assert_eq!(miss.code, ASSET_NO_RESOLVER);
    // The handle the server's table did not know was sent as none and counted.
    assert!(hub.server.world.resource::<NetAssets>().unmapped() >= 1);
}
