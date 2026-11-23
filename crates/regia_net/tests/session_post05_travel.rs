// SPDX-License-Identifier: MPL-2.0
//! Spec 77 § 6.2 level travel (acceptance 6, `NetId` part; ADR-7212): the server despawns every
//! replicated entity except `NetPersistAcrossTravel` ones (player entities carry it by default), and
//! a client keeps the persistent copy under its `NetId`. The other post-v0.5 session items are in
//! `session_post05.rs`. Fixed steps and a manual session clock only.

use regia_app::prelude::*;
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::session::{HostOptions, NetSession, SimInput};
use regia_net::testing::LoopbackHub;
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

fn setup(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

/// The server despawns the non-persistent replicated entity; the client loses that copy and keeps
/// the persistent copy: the same client entity under the same `NetId`.
#[test]
fn travel_keeps_persistent_net_ids_and_despawns_the_rest() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 9, setup);
    let level_crate = hub.server.world.spawn((rep(), Hp { hp: 1 })).id();
    let a = hub.add_client();
    let peer = hub.clients[a].peer;
    let controller = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 5 }, NetOwner(peer), NetPersistAcrossTravel))
        .id();
    hub.run(20);
    let id_before = *hub.server.world.get::<NetId>(controller).expect("net id");
    let copy_before = copy(&hub, a, controller).expect("controller copy");
    let crate_id = *hub.server.world.get::<NetId>(level_crate).expect("net id");
    assert!(copy(&hub, a, level_crate).is_some());

    let counts = regia_net::session::despawn_for_travel(&mut hub.server.world);
    assert_eq!(
        counts.despawned, 1,
        "only the non-persistent replicated entity goes"
    );
    assert_eq!(counts.persistent, 1);
    hub.run(20);
    assert!(hub.server.world.get_entity(level_crate).is_err());
    assert_eq!(hub.server.world.get::<NetId>(controller), Some(&id_before));
    let client_ids = hub.clients[a].world.resource::<NetEntities>();
    assert_eq!(
        client_ids.entity(id_before),
        Some(copy_before),
        "the client keeps the persistent copy under its NetId"
    );
    assert_eq!(
        client_ids.entity(crate_id),
        None,
        "the despawned entity's copy is gone"
    );
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.world_mut()
            .resource_mut::<NetSession>()
            .advance_manual_clock(16);
        app.update();
    }
}

/// Through `NetSession::travel`: a replicated entity is despawned, a `NetPersistAcrossTravel` one,
/// a local (not replicated) one and the player's `NetPlayer` entity stay, and the player entity
/// carries the marker by default. The session's travel counts report it (`net.session`).
#[test]
fn net_session_travel_despawns_non_persistent_replicated_entities() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((
        NetPlugin {
            mode: NetMode::DedicatedServer,
        },
        NetSessionPlugin,
    ));
    app.finish_plugins();
    {
        let mut s = app.world_mut().resource_mut::<NetSession>();
        s.set_manual_clock(0);
        s.host_local(
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
        s.add_simulated_clients(1, SimInput::Idle)
            .expect("simulated client");
    }
    frames(&mut app, 30);
    let player = NetPlayerId {
        peer: PeerId(1),
        local: 0,
    };
    let player_entity = app
        .world()
        .resource::<NetSession>()
        .player_entity(player)
        .expect("player entity");
    let level_thing = app.world_mut().spawn((rep(), Hp { hp: 1 })).id();
    let score = app
        .world_mut()
        .spawn((rep(), Hp { hp: 3 }, NetPersistAcrossTravel))
        .id();
    let local_only = app.world_mut().spawn(Hp { hp: 4 }).id();
    frames(&mut app, 2);
    app.world_mut()
        .resource_mut::<NetSession>()
        .travel("levels/b.level.ron")
        .expect("travel");
    frames(&mut app, 2);
    assert!(
        app.world().get_entity(level_thing).is_err(),
        "a replicated entity is despawned"
    );
    assert!(app.world().get_entity(score).is_ok(), "a persistent entity stays");
    assert!(
        app.world().get_entity(local_only).is_ok(),
        "a local entity is not touched"
    );
    assert!(
        app.world().get_entity(player_entity).is_ok(),
        "the player entity stays"
    );
    assert!(
        app.world().get::<NetPersistAcrossTravel>(player_entity).is_some(),
        "player entities persist by default"
    );
    let counts = app.world().resource::<NetSession>().travel_counts();
    assert_eq!(counts.despawned, 1);
    assert_eq!(
        counts.persistent, 1,
        "the score entity (the player entity is not replicated)"
    );
}

/// Join snapshot item 1 (spec 77 § 6.1) comes from replication: a level-placed replicated entity
/// the server despawned is named to a joiner by its `EntityGuid`; after a travel the old level's
/// removals are forgotten. Item 2 names the joiner's own entity.
#[test]
fn the_join_snapshot_names_removed_level_entities_until_a_travel() {
    use regia_net::session::{JoinRecord, JoinRecordKind, join_snapshot_for};
    use regia_scene::components::EntityGuid;
    use regia_scene::level::LevelMember;

    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
    let guid = EntityGuid::new_random();
    let wanted = guid.0.as_u128();
    let barrel = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 1 }, LevelMember(7), guid))
        .id();
    let joiner = PeerId(42);
    let own = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 2 }, NetOwner(joiner)))
        .id();
    hub.run(4);
    hub.server.world.despawn(barrel);
    hub.run(2);
    let snapshot = join_snapshot_for(&hub.server.world, joiner, 9);
    assert_eq!(snapshot.removed, vec![wanted]);
    assert_eq!(snapshot.tick, 9);
    let own_id = *hub.server.world.get::<NetId>(own).expect("net id");
    let records: Vec<JoinRecord> = snapshot
        .records
        .iter()
        .filter_map(|b| JoinRecord::decode(b))
        .collect();
    assert_eq!(
        records,
        vec![JoinRecord {
            net_id: own_id,
            kind: JoinRecordKind::Own
        }]
    );

    regia_net::session::despawn_for_travel(&mut hub.server.world);
    hub.run(2);
    assert!(
        join_snapshot_for(&hub.server.world, joiner, 10)
            .removed
            .is_empty(),
        "the old level's removals are forgotten"
    );
}

/// Review finding rev-claude-29-k #2: only the entities `despawn_for_travel` itself despawned are
/// forgotten once the travel's despawns are collected. A level-placed replicated entity that
/// survives the travel (it carries `NetPersistAcrossTravel`) but is despawned before the next
/// gather is a legitimate removal: a peer joining into the new level must be told of it, or it
/// keeps the entity its local level load spawned.
#[test]
fn a_level_entity_despawned_right_after_a_travel_is_still_named_to_joiners() {
    use regia_net::session::join_snapshot_for;
    use regia_scene::components::EntityGuid;
    use regia_scene::level::LevelMember;

    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 17, setup);
    let guid = EntityGuid::new_random();
    let wanted = guid.0.as_u128();
    // A persistent level-placed entity survives the travel ...
    let keeper = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 1 }, LevelMember(7), guid, NetPersistAcrossTravel))
        .id();
    hub.run(4);
    regia_net::session::despawn_for_travel(&mut hub.server.world);
    // ... and the game despawns it before the next gather (a fixed-tick script this frame).
    hub.server.world.despawn(keeper);
    hub.run(2);
    let snapshot = join_snapshot_for(&hub.server.world, PeerId(42), 12);
    assert_eq!(
        snapshot.removed,
        vec![wanted],
        "the post-travel removal reaches a joiner into the new level"
    );
}
