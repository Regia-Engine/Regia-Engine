// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 14, ADR-2022: each peer has its own floating origin and root positions cross the wire
//! in the shared world frame. A child's position is relative to its parent and crosses unchanged.

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::runtime::NetEntities;
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::streaming::OriginOffset;
use regia_scene::transform::Transform;

fn setup(_: &mut World) {}

fn moving() -> Replicated {
    Replicated {
        transform: TransformReplication::Movement,
        ..Replicated::default()
    }
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

#[test]
fn a_root_arrives_at_the_same_world_point_under_another_origin() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
    let a = hub.add_client();
    // The server's origin is four cells out, the client's one cell.
    hub.server.world.insert_resource(OriginOffset([4096.0, 0.0, 0.0]));
    hub.clients[a]
        .world
        .insert_resource(OriginOffset([1024.0, 0.0, -2048.0]));
    let root = hub
        .server
        .world
        .spawn((moving(), Transform::from_translation(Vec3::new(10.0, 2.0, 3.0))))
        .id();
    let child = hub
        .server
        .world
        .spawn((
            moving(),
            Transform::from_translation(Vec3::new(1.0, 0.5, 0.0)),
            ChildOf(root),
        ))
        .id();
    hub.run(4);
    let at = |hub: &LoopbackHub, e: Entity| {
        let c = copy(hub, a, e).expect("copy");
        hub.clients[a]
            .world
            .get::<Transform>(c)
            .expect("transform")
            .translation
    };
    // World (4106, 2, 3) is (3082, 2, 2051) around the client's origin.
    assert_eq!(at(&hub, root), Vec3::new(3082.0, 2.0, 2051.0));
    assert_eq!(at(&hub, child), Vec3::new(1.0, 0.5, 0.0), "relative, unchanged");

    // The server moves the root; the update converts the same way.
    hub.server
        .world
        .get_mut::<Transform>(root)
        .expect("root")
        .translation = Vec3::new(20.0, 2.0, 3.0);
    hub.run(4);
    assert_eq!(at(&hub, root), Vec3::new(3092.0, 2.0, 2051.0));
}

#[test]
fn without_an_origin_the_wire_carries_local_positions() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 8, setup);
    let a = hub.add_client();
    let root = hub
        .server
        .world
        .spawn((moving(), Transform::from_translation(Vec3::new(-0.0, 2.0, 3.0))))
        .id();
    hub.run(4);
    let c = copy(&hub, a, root).expect("copy");
    let got = hub.clients[a]
        .world
        .get::<Transform>(c)
        .expect("transform")
        .translation;
    assert_eq!(got, Vec3::new(-0.0, 2.0, 3.0));
    assert!(got.x.is_sign_negative(), "no conversion touched the bits");
}

/// A child that leaves its replicated parent becomes a root, so its next transform crosses in the
/// shared frame. Removing `ChildOf` has no change tick: the server must still re-read the
/// transform when the parent goes (ADR-2413 (4) follow-up).
#[test]
fn an_unparented_child_crosses_in_the_shared_frame() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 9, setup);
    let a = hub.add_client();
    hub.server.world.insert_resource(OriginOffset([4096.0, 0.0, 0.0]));
    hub.clients[a]
        .world
        .insert_resource(OriginOffset([1024.0, 0.0, -2048.0]));
    let root = hub
        .server
        .world
        .spawn((moving(), Transform::from_translation(Vec3::new(10.0, 2.0, 3.0))))
        .id();
    let child = hub
        .server
        .world
        .spawn((
            moving(),
            Transform::from_translation(Vec3::new(1.0, 0.5, 0.0)),
            ChildOf(root),
        ))
        .id();
    hub.run(4);
    let at = |hub: &LoopbackHub, e: Entity| {
        let c = copy(hub, a, e).expect("copy");
        let w = &hub.clients[a].world;
        (
            w.get::<Transform>(c).expect("transform").translation,
            w.get::<ChildOf>(c).is_some(),
        )
    };
    assert_eq!(at(&hub, child), (Vec3::new(1.0, 0.5, 0.0), true), "relative");
    // The local value stays (1, 0.5, 0), now a root position: world (4097, 0.5, 0).
    hub.server.world.entity_mut(child).remove::<ChildOf>();
    hub.run(4);
    assert_eq!(
        at(&hub, child),
        (Vec3::new(3073.0, 0.5, 2048.0), false),
        "a root now"
    );
}

/// The acknowledged state channel (protocol 2, the default wire) converts the same way as the tick
/// stream: spawn records and updates of a root land in the client's local frame (ADR-2902 found
/// the state channel writing wire positions unconverted).
#[test]
fn the_state_channel_converts_roots_to_the_local_frame() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
    hub.server
        .world
        .resource_mut::<NetSettings>()
        .replication
        .state_channel = StateChannel::default();
    let a = hub.add_client();
    hub.clients[a]
        .world
        .resource_mut::<NetSettings>()
        .replication
        .state_channel = StateChannel::default();
    let peer = hub.clients[a].peer;
    hub.server
        .world
        .resource_mut::<regia_net::replication::ServerReplication>()
        .set_wire(peer, regia_net::replication::ConnWire::Acked);
    hub.server.world.insert_resource(OriginOffset([4096.0, 0.0, 0.0]));
    hub.clients[a]
        .world
        .insert_resource(OriginOffset([1024.0, 0.0, -2048.0]));
    let root = hub
        .server
        .world
        .spawn((moving(), Transform::from_translation(Vec3::new(10.0, 2.0, 3.0))))
        .id();
    hub.run(6);
    let at = |hub: &LoopbackHub| {
        let c = copy(hub, a, root).expect("copy");
        hub.clients[a]
            .world
            .get::<Transform>(c)
            .expect("transform")
            .translation
    };
    assert_eq!(at(&hub), Vec3::new(3082.0, 2.0, 2051.0), "the spawn record");
    hub.server
        .world
        .get_mut::<Transform>(root)
        .expect("root")
        .translation = Vec3::new(20.0, 2.0, 3.0);
    hub.run(6);
    assert_eq!(at(&hub), Vec3::new(3092.0, 2.0, 2051.0), "an update");
}
