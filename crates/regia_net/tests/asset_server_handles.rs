// SPDX-License-Identifier: MPL-2.0
//! ADR-2665 (3): a handle loaded through the world's `AssetServer` replicates by its guid without
//! the game registering it. `register_asset_server_handles` (run by `net_receive` and
//! `net_send`) fills a `NetAssetServerTable` from the server's slabs and installs it as the
//! resolver when the world has none; a resolver the game installed itself is left alone.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh,
    Handle, cooked_cache_dir, cooked_file_name, encode_cooked_file, pack_importer_version,
};
use regia_core::id::Guid;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::assets::{NetAssetServerTable, NetAssetTable, NetAssets};
use regia_net::message::register_net_message;
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::testing::LoopbackHub;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Prop {
    mesh: Handle<CpuMesh>,
    n: u32,
}

#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "test.show_mesh")]
struct ShowMesh {
    mesh: Handle<CpuMesh>,
}

#[derive(Resource, Default)]
struct Shown(Vec<Handle<CpuMesh>>);

fn setup(world: &mut World) {
    register_replicated::<Prop>(world, ReplicateOptions::default());
    register_net_message::<ShowMesh>(world);
}

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// A fresh directory per call (pid + counter), never in the source tree.
fn temp_project(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-net-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A cooked one-triangle mesh in the project's dev cache.
fn cook_mesh(root: &std::path::Path, asset: AssetId, z: f32) {
    let mesh = CpuMesh {
        positions: vec![[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0, 0.0]; 3],
        tangents: Vec::new(),
        indices: vec![0, 1, 2],
        ..Default::default()
    };
    let payload = mesh.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: asset,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, &payload).expect("encode");
    let dir = cooked_cache_dir(root, "linux");
    std::fs::create_dir_all(&dir).expect("cache dir");
    std::fs::write(dir.join(cooked_file_name(asset)), bytes).expect("write cooked");
}

fn open(root: &std::path::Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("open the dev cache")
}

fn send<M: Send + 'static>(
    world: &mut World,
    f: impl FnOnce(&mut NetSender) -> M + Send + Sync + 'static,
) -> M {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| (f.take().expect("runs once"))(&mut net))
        .expect("system runs")
}

#[test]
fn a_handle_loaded_through_the_server_replicates_unregistered() {
    let root = temp_project("asset-server-handles");
    for (n, z) in [(1, 0.0), (2, 1.0), (3, 2.0)] {
        cook_mesh(&root, id(n), z);
    }
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 21, setup);
    let a = hub.add_client();
    hub.clients[a].world.init_resource::<Shown>();
    hub.clients[a]
        .gameplay
        .add_systems(|msgs: Res<NetMessages<ShowMesh>>, mut shown: ResMut<Shown>| {
            for m in msgs.iter() {
                shown.0.push(m.msg.mesh.clone());
            }
        });

    // Server: a padding mesh first, so its slots differ from the client's.
    let mut server_assets = open(&root);
    let _pad = server_assets.load_mesh(id(3)).expect("pad loads");
    let rock = server_assets.load_mesh(id(1)).expect("rock loads");
    hub.server.world.insert_resource(server_assets);
    // Client: the same content, loaded in another order.
    let mut client_assets = open(&root);
    let c_rock = client_assets.load_mesh(id(1)).expect("rock loads");
    hub.clients[a].world.insert_resource(client_assets);
    assert_ne!(rock.index, c_rock.index, "the slots differ per process");
    // Nobody inserted a NetAssets resolver or registered a handle.
    assert!(!hub.server.world.contains_resource::<NetAssets>());

    let e = hub
        .server
        .world
        .spawn((Replicated::default(), Prop { mesh: rock, n: 5 }))
        .id();
    hub.run(4);
    let net_id = *hub.server.world.get::<NetId>(e).expect("replicated");
    let copy = hub.clients[a]
        .world
        .resource::<NetEntities>()
        .entity(net_id)
        .expect("copy spawned");
    let prop = hub.clients[a].world.get::<Prop>(copy).cloned().expect("prop");
    assert_eq!(
        prop.mesh, c_rock,
        "the guid resolved through the client's asset server"
    );
    assert_eq!(
        hub.clients[a]
            .world
            .resource::<AssetServer>()
            .meshes()
            .get(&prop.mesh)
            .map(|m| m.positions[0][2]),
        Some(0.0)
    );
    let table = hub.server.world.resource::<NetAssetServerTable>();
    assert!(table.is_resolver());
    assert_eq!(table.table().len(), 2, "both loaded meshes registered");
    assert_eq!(hub.server.world.resource::<NetAssets>().unmapped(), 0);

    // A mesh loaded later on both sides travels too, in a component and in a message.
    let tree = hub
        .server
        .world
        .resource_mut::<AssetServer>()
        .load_mesh(id(2))
        .expect("tree loads");
    let c_tree = hub.clients[a]
        .world
        .resource_mut::<AssetServer>()
        .load_mesh(id(2))
        .expect("tree loads");
    hub.server.world.get_mut::<Prop>(e).expect("prop").mesh = tree.clone();
    hub.run(3);
    assert_eq!(hub.clients[a].world.get::<Prop>(copy).expect("prop").mesh, c_tree);
    send(&mut hub.server.world, move |n| {
        n.broadcast_session(ShowMesh { mesh: tree })
    })
    .expect("broadcast");
    hub.run(3);
    assert_eq!(hub.clients[a].world.resource::<Shown>().0, vec![c_tree]);
    assert_eq!(hub.clients[a].world.resource::<NetAssets>().missing().count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_game_resolver_is_left_alone() {
    let root = temp_project("asset-server-game-resolver");
    cook_mesh(&root, id(4), 0.0);
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 22, setup);
    let _a = hub.add_client();
    let mut assets = open(&root);
    let h = assets.load_mesh(id(4)).expect("loads");
    hub.server.world.insert_resource(assets);
    let game_table = Arc::new(NetAssetTable::new());
    hub.server
        .world
        .insert_resource(NetAssets::new(game_table.clone()));
    hub.server
        .world
        .spawn((Replicated::default(), Prop { mesh: h, n: 1 }));
    hub.run(3);
    let table = hub.server.world.resource::<NetAssetServerTable>();
    assert!(!table.is_resolver(), "the game's resolver wins");
    assert!(table.table().is_empty(), "and the server table is not filled");
    assert!(
        game_table.is_empty(),
        "nothing registers into the game's own table"
    );
    assert!(
        hub.server.world.resource::<NetAssets>().unmapped() >= 1,
        "the game did not register it, so it travels as none, as before"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_world_without_an_asset_server_is_untouched() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 23, setup);
    let _a = hub.add_client();
    hub.run(2);
    assert!(!hub.server.world.contains_resource::<NetAssetServerTable>());
    assert!(!hub.server.world.contains_resource::<NetAssets>());
}
