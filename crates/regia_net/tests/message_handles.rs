// SPDX-License-Identifier: MPL-2.0
//! Spec 71 § 3 for net messages (ADR-2665 (2)): a `Handle<T>` field inside a net message travels
//! as the asset's guid or path, exactly as in a replicated component, and resolves through the
//! receiver's own asset-server context. A key the receiver does not know leaves the handle unset
//! and is listed in `NetAssets` with `asset_unknown`, under the message's name.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_asset::{AssetId, Assets, Handle};
use regia_core::id::Guid;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::assets::{NetAssetTable, NetAssets};
use regia_net::codec::{ASSET_NO_RESOLVER, ASSET_UNKNOWN, AssetKey, AssetResolver};
use regia_net::message::{NetMessageRegistry, register_net_message};
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::testing::LoopbackHub;
use regia_net::*;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Sound(&'static str);

/// Server to every client: play this sound.
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "test.play_sound")]
struct PlaySound {
    sound: Handle<Sound>,
    volume: u8,
    #[replicate(max_len = 4)]
    layers: Vec<Handle<Sound>>,
}

/// Client to server, on the client's pawn: equip this sound as a taunt.
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "to_server", id = "test.taunt")]
struct Taunt {
    sound: Handle<Sound>,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Pawn {
    n: u32,
}

#[derive(Resource, Default)]
struct Heard(Vec<PlaySound>);

#[derive(Resource, Default)]
struct Taunts(Vec<Handle<Sound>>);

fn setup(world: &mut World) {
    register_net_message::<PlaySound>(world);
    register_net_message::<Taunt>(world);
    register_replicated::<Pawn>(world, ReplicateOptions::default());
}

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// One process's asset server: a slab plus the table its handles are registered in.
struct Store {
    slab: Assets<Sound>,
    table: Arc<NetAssetTable>,
}

impl Store {
    fn new(padding: usize) -> Self {
        let mut slab = Assets::new();
        for i in 0..padding {
            // Padding makes the slots differ per process.
            let _ = slab.insert(id(1000 + i as u128), Sound("pad"));
        }
        Self {
            slab,
            table: Arc::new(NetAssetTable::new()),
        }
    }

    fn load(&mut self, n: u128, name: &'static str) -> Handle<Sound> {
        let h = self.slab.insert(id(n), Sound(name));
        self.table.insert_id(&h, id(n));
        h
    }

    fn load_path(&mut self, n: u128, path: &str, name: &'static str) -> Handle<Sound> {
        let h = self.slab.insert(id(n), Sound(name));
        self.table.insert_path(&h, path);
        h
    }
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

fn listen(hub: &mut LoopbackHub, client: usize) {
    hub.clients[client].world.init_resource::<Heard>();
    hub.clients[client].gameplay.add_systems(
        |msgs: Res<NetMessages<PlaySound>>, mut heard: ResMut<Heard>| {
            for m in msgs.iter() {
                heard.0.push(m.msg.clone());
            }
        },
    );
}

#[test]
fn message_handles_round_trip_between_two_peers() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    listen(&mut hub, a);
    hub.server.world.init_resource::<Taunts>();
    hub.server
        .gameplay
        .add_systems(|msgs: Res<NetMessages<Taunt>>, mut seen: ResMut<Taunts>| {
            for m in msgs.iter() {
                seen.0.push(m.msg.sound);
            }
        });

    let mut sv = Store::new(3);
    let horn = sv.load(1, "horn");
    let bell = sv.load_path(2, "assets/sfx/bell.ogg", "bell");
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));
    let mut cl = Store::new(0);
    let c_horn = cl.load(1, "horn");
    let c_bell = cl.load_path(2, "assets/sfx/bell.ogg", "bell");
    hub.clients[a]
        .world
        .insert_resource(NetAssets::new(cl.table.clone()));
    assert_ne!(horn.index, c_horn.index, "the slots differ per process");

    // Server to client.
    send(&mut hub.server.world, move |n| {
        n.broadcast_session(PlaySound {
            sound: horn,
            volume: 80,
            layers: vec![bell, Handle::INVALID],
        })
    })
    .expect("broadcast");
    hub.run(3);
    let heard = &hub.clients[a].world.resource::<Heard>().0;
    assert_eq!(
        heard.as_slice(),
        &[PlaySound {
            sound: c_horn,
            volume: 80,
            layers: vec![c_bell, Handle::INVALID],
        }],
        "the guid and the path resolved through the client's table"
    );
    assert_eq!(cl.slab.get(&heard[0].sound), Some(&Sound("horn")));

    // Client to server, on its own pawn.
    let pawn = hub
        .server
        .world
        .spawn((Replicated::default(), Pawn { n: 1 }, NetOwner(pa)))
        .id();
    hub.run(3);
    let net_id = *hub.server.world.get::<NetId>(pawn).expect("id");
    let copy = hub.clients[a]
        .world
        .resource::<NetEntities>()
        .entity(net_id)
        .expect("copy");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(copy, Taunt { sound: c_bell })
    })
    .expect("sent");
    hub.run(3);
    assert_eq!(
        hub.server.world.resource::<Taunts>().0,
        vec![bell],
        "the client's handle resolved to the server's slot"
    );
    assert_eq!(hub.server.world.resource::<NetAssets>().unmapped(), 0);
    assert_eq!(hub.clients[a].world.resource::<NetAssets>().unmapped(), 0);
    assert_eq!(hub.clients[a].world.resource::<NetAssets>().missing().count(), 0);
}

/// A client chooses the key in its message: the server's loader never sees it. The same table
/// still loads for what it was built for, a key the process itself resolves.
#[test]
fn a_client_message_cannot_make_the_server_load() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 13, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    hub.server.world.init_resource::<Taunts>();
    hub.server
        .gameplay
        .add_systems(|msgs: Res<NetMessages<Taunt>>, mut seen: ResMut<Taunts>| {
            for m in msgs.iter() {
                seen.0.push(m.msg.sound);
            }
        });
    let loads = Arc::new(AtomicU32::new(0));
    let counted = loads.clone();
    let table = Arc::new(NetAssetTable::with_loader(move |_, _| {
        counted.fetch_add(1, Ordering::Relaxed);
        Ok((40, 1))
    }));
    hub.server.world.insert_resource(NetAssets::new(table.clone()));
    let mut cl = Store::new(0);
    let chosen = cl.load_path(2, "secrets/server_only.ron", "x");
    hub.clients[a]
        .world
        .insert_resource(NetAssets::new(cl.table.clone()));

    let pawn = hub
        .server
        .world
        .spawn((Replicated::default(), Pawn { n: 1 }, NetOwner(pa)))
        .id();
    hub.run(3);
    let net_id = *hub.server.world.get::<NetId>(pawn).expect("id");
    let copy = hub.clients[a]
        .world
        .resource::<NetEntities>()
        .entity(net_id)
        .expect("copy");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(copy, Taunt { sound: chosen })
    })
    .expect("sent");
    hub.run(3);

    let taunts = &hub.server.world.resource::<Taunts>().0;
    assert_eq!(taunts.len(), 1, "the message is still delivered");
    assert!(taunts[0].is_invalid(), "the key resolves to nothing");
    assert_eq!(loads.load(Ordering::Relaxed), 0, "the loader never ran");
    let key = AssetKey::Path("secrets/server_only.ron".into());
    let miss = hub
        .server
        .world
        .resource::<NetAssets>()
        .missing_key(&key)
        .expect("listed");
    assert_eq!(miss.code, ASSET_UNKNOWN);

    // The table's own loader still answers a resolve the process asked for.
    let any = std::any::TypeId::of::<Sound>();
    assert_eq!(table.resolve(any, &key), Ok((40, 1)));
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[test]
fn an_unknown_asset_in_a_message_is_unset_and_listed() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 12, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    listen(&mut hub, a);
    listen(&mut hub, b);
    let mut sv = Store::new(0);
    let secret = sv.load(7, "secret");
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));
    // Client a has a resolver that does not know the asset; client b has no resolver.
    hub.clients[a]
        .world
        .insert_resource(NetAssets::new(Arc::new(NetAssetTable::new())));

    send(&mut hub.server.world, move |n| {
        n.broadcast_session(PlaySound {
            sound: secret,
            volume: 3,
            layers: vec![],
        })
    })
    .expect("broadcast");
    hub.run(3);
    for c in [a, b] {
        let heard = &hub.clients[c].world.resource::<Heard>().0;
        assert_eq!(heard.len(), 1, "the message is still delivered");
        assert!(
            heard[0].sound.is_invalid(),
            "an unknown asset leaves the handle unset"
        );
        assert_eq!(heard[0].volume, 3, "the rest of the message applies");
        let reg = hub.clients[c].world.resource::<NetMessageRegistry>();
        let i = reg.find("test.play_sound").expect("registered");
        assert_eq!(reg.counters[usize::from(i)].delivered, 1);
    }
    let miss = hub.clients[a]
        .world
        .resource::<NetAssets>()
        .missing_key(&AssetKey::Guid(id(7)))
        .expect("the unknown asset is listed")
        .clone();
    assert_eq!(miss.code, ASSET_UNKNOWN);
    assert_eq!(miss.code, "asset_unknown", "the documented code");
    assert_eq!(miss.count, 1);
    let miss = hub.clients[b]
        .world
        .resource::<NetAssets>()
        .missing_key(&AssetKey::Guid(id(7)))
        .expect("listed without a resolver too")
        .clone();
    assert_eq!(miss.code, ASSET_NO_RESOLVER);
}

#[test]
fn a_handle_the_sender_never_registered_travels_as_none_and_counts() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 13, setup);
    let a = hub.add_client();
    listen(&mut hub, a);
    let mut sv = Store::new(0);
    let unregistered = sv.slab.insert(id(9), Sound("not in the table"));
    hub.server.world.insert_resource(NetAssets::new(sv.table.clone()));
    let mut cl = Store::new(0);
    let _ = cl.load(9, "known here");
    hub.clients[a]
        .world
        .insert_resource(NetAssets::new(cl.table.clone()));
    send(&mut hub.server.world, move |n| {
        n.broadcast_session(PlaySound {
            sound: unregistered,
            volume: 1,
            layers: vec![],
        })
    })
    .expect("broadcast");
    hub.run(3);
    let heard = &hub.clients[a].world.resource::<Heard>().0;
    assert!(heard[0].sound.is_invalid());
    assert_eq!(hub.server.world.resource::<NetAssets>().unmapped(), 1);
    assert_eq!(
        hub.clients[a].world.resource::<NetAssets>().missing().count(),
        0,
        "none is not a miss"
    );
}
