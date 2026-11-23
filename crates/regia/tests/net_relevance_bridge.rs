// SPDX-License-Identifier: MPL-2.0
//! The `net_relevance` gizmo group (spec 72 § 10, ADR-2665 (5)): registered under its protocol
//! name and off by default, absent on a world without gizmos, and drawing each replicated
//! entity in its state's color for the chosen connection, an asleep one outlined, plus the
//! viewpoints, their radius and the interest grid.

use regia::net::model::{NetMode, NetPlayerId};
use regia::net::profile::{NetProfile, NetProfiles, RelevanceShape};
use regia::net::replication::register_replicated;
use regia::net::replication::relevance_view::{
    NetRelevanceView, RelevanceMarkState, cached_relevance_snapshot, relevance_snapshot, view_peer,
};
use regia::net::testing::LoopbackHub;
use regia::net::{NetAsleep, NetProfileRef, NetViewpoint, Replicate, ReplicateOptions, Replicated};
use regia::net_relevance_bridge::{
    ASLEEP, FILTERED_OUT, NetRelevanceBridgePlugin, NetRelevanceGizmos, RELEVANT, VIEWPOINT, draw_relevance,
};
use regia_app::prelude::*;
// `#[derive(Replicate)]` names `regia_net`; a game that only depends on `regia` aliases it.
use regia::net as regia_net;
use regia_core::math::{Color, Vec3};
use regia_render::ExtractedFrame;
use regia_render::gizmos::{GizmoBuffer, GizmoConfigGroup, GizmoConfigStore, GizmoPlugin};
use regia_scene::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp(u32);

fn setup(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        regia::net::install_net_interest(world);
        let near = NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 10.0,
                hysteresis: 0.0,
            },
            linger_ticks: 0,
            ..NetProfile::default()
        };
        let _ = world.resource_mut::<NetProfiles>().insert("near", near);
    }
}

fn at(x: f32) -> Transform {
    Transform::from_translation(Vec3::new(x, 0.0, 0.0))
}

fn rep() -> Replicated {
    Replicated {
        transform: regia::net::TransformReplication::None,
        ..Replicated::default()
    }
}

fn has_color(colors: &[Color], c: Color) -> bool {
    colors.contains(&c)
}

#[test]
fn the_group_is_registered_off_and_only_where_gizmos_are() {
    assert_eq!(NetRelevanceGizmos::NAME, "net_relevance");
    // A world without a renderer: the view resource, no group.
    let mut server = App::new();
    server.add_plugins(NetRelevanceBridgePlugin);
    server.finish_plugins();
    assert!(server.world().contains_resource::<NetRelevanceView>());
    assert!(!server.world().contains_resource::<GizmoConfigStore>());
    // With gizmos: registered, off until the agent turns it on.
    let mut app = App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.init_resource::<ExtractedFrame>();
    app.add_plugins((GizmoPlugin, NetRelevanceBridgePlugin));
    app.finish_plugins();
    app.update();
    let store = app.world().resource::<GizmoConfigStore>();
    let (config, _) = store.config::<NetRelevanceGizmos>().expect("registered");
    assert!(!config.enabled, "off by default");
}

#[test]
fn each_entity_is_drawn_in_its_states_color_for_the_chosen_peer() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 31, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    for (peer, x) in [(pa, 0.0), (pb, 200.0)] {
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            at(x),
        ));
    }
    let near = hub
        .server
        .world
        .spawn((rep(), Hp(1), at(2.0), NetProfileRef("near".into())))
        .id();
    let far = hub
        .server
        .world
        .spawn((rep(), Hp(2), at(201.0), NetProfileRef("near".into()), NetAsleep))
        .id();
    hub.run(4);
    let world = &mut hub.server.world;
    assert_eq!(view_peer(world), Some(pa.min(pb)), "the lowest peer by default");
    let mut chosen = NetRelevanceView::default();
    chosen.peer = Some(pa);
    world.insert_resource(chosen);
    assert_eq!(view_peer(world), Some(pa));
    let snap = relevance_snapshot(world, pa).expect("a server world");
    assert!(snap.pipeline);
    assert_eq!(snap.viewpoints.len(), 1);
    let state = |e| {
        snap.marks
            .iter()
            .find(|m| m.entity == e)
            .map(|m| (m.state, m.asleep))
    };
    assert_eq!(state(near), Some((RelevanceMarkState::Relevant, false)));
    assert_eq!(state(far), Some((RelevanceMarkState::FilteredOut, true)));

    let position = |e| -> Option<Vec3> { world.get::<Transform>(e).map(|t| t.translation) };
    let mut buffer = GizmoBuffer::<NetRelevanceGizmos>::default();
    draw_relevance(&mut buffer, &snap, position);
    let colors: Vec<Color> = buffer
        .list_colors
        .iter()
        .chain(buffer.strip_colors.iter())
        .copied()
        .collect();
    assert!(has_color(&colors, RELEVANT), "the near entity");
    assert!(has_color(&colors, FILTERED_OUT), "the far entity");
    assert!(has_color(&colors, ASLEEP), "the far entity is asleep");
    assert!(has_color(&colors, VIEWPOINT), "the viewpoint");
    // Nothing positioned, nothing drawn but the viewpoint and grid.
    let mut bare = GizmoBuffer::<NetRelevanceGizmos>::default();
    draw_relevance(&mut bare, &snap, |_| None);
    let bare_colors: Vec<Color> = bare
        .list_colors
        .iter()
        .chain(bare.strip_colors.iter())
        .copied()
        .collect();
    assert!(!has_color(&bare_colors, RELEVANT));
    assert!(has_color(&bare_colors, VIEWPOINT));

    // Peer b sees the other way round.
    let snap = relevance_snapshot(world, pb).expect("a server world");
    let state = |e| snap.marks.iter().find(|m| m.entity == e).map(|m| m.state);
    assert_eq!(state(near), Some(RelevanceMarkState::FilteredOut));
    assert_eq!(state(far), Some(RelevanceMarkState::Relevant));
}

/// Two reads with the interest resource left untouched draw the same marks, in net-id order,
/// and the second read does not rebuild the scratch. A new replicated entity refills it.
#[test]
fn a_second_frame_reuses_the_sorted_mark_list() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 32, setup);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    for (peer, x) in [(pa, 0.0), (pb, 200.0)] {
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            at(x),
        ));
    }
    hub.server
        .world
        .spawn((rep(), Hp(1), at(2.0), NetProfileRef("near".into())));
    hub.server
        .world
        .spawn((rep(), Hp(2), at(201.0), NetProfileRef("near".into())));
    hub.run(4);
    let peer = pa.min(pb);
    let world = &mut hub.server.world;
    let mut chosen = NetRelevanceView::default();
    chosen.peer = Some(peer);
    world.insert_resource(chosen);

    let read = |world: &mut World| {
        let snap = cached_relevance_snapshot(world, peer).expect("a server world");
        let ids: Vec<_> = snap.marks.iter().map(|mark| mark.net_id).collect();
        let position = |entity| world.get::<Transform>(entity).map(|t| t.translation);
        let mut buffer = GizmoBuffer::<NetRelevanceGizmos>::default();
        draw_relevance(&mut buffer, &snap, position);
        let drawn = buffer.list_colors.len() + buffer.strip_colors.len();
        let fills = world.resource::<NetRelevanceView>().snapshot_fills();
        (ids, drawn, fills)
    };
    let (ids, drawn, fills) = read(world);
    assert!(ids.windows(2).all(|pair| pair[0] <= pair[1]), "net-id order");
    assert!(!ids.is_empty(), "the replicated entities are marks");
    let (ids_again, drawn_again, fills_again) = read(world);
    assert_eq!(ids_again, ids, "the same set is drawn");
    assert_eq!(drawn_again, drawn, "the second frame still draws");
    assert_eq!(fills_again, fills, "interest was left untouched");
    assert_eq!(fills, 1, "the first read fills the scratch once");

    hub.server
        .world
        .spawn((rep(), Hp(3), at(1.0), NetProfileRef("near".into())));
    hub.run(4);
    let (ids_new, _, fills_new) = read(&mut hub.server.world);
    assert!(ids_new.len() > ids.len(), "the new entity is a mark");
    assert!(fills_new > fills, "a new replicated entity refills the scratch");
    assert!(ids_new.windows(2).all(|pair| pair[0] <= pair[1]));
}
