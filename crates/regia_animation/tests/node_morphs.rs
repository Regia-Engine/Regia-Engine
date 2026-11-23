// SPDX-License-Identifier: MPL-2.0
//! Per-node morph tracks end to end (ADR-297): a clip's tracks reach the `MorphWeights` of the
//! entity spawned for each glTF node, through a `NodeMorphPlayer` and through an `Animator`.

use std::sync::Arc;

use regia_animation::Skeleton;
use regia_animation::clip::{MorphNode, MorphTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::prelude::*;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(16),
    ));
    app
}

fn node(index: u32, name: &str) -> MorphNode {
    MorphNode {
        index,
        name: name.into(),
    }
}

fn ramp(a: f32, b: f32) -> TrackCurve<f32> {
    TrackCurve::Animated {
        times: vec![0.0, 1.0],
        values: vec![a, b],
        exact_rotation: false,
    }
}

/// Node 0 ("a", two targets) rises, node 1 ("b", one target) falls.
fn two_mesh_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("both", 1.0);
    clip.morph_tracks
        .push(MorphTrack::for_node(node(0, "a"), 0, ramp(0.0, 1.0)));
    clip.morph_tracks
        .push(MorphTrack::for_node(node(0, "a"), 1, TrackCurve::Static(0.5)));
    clip.morph_tracks
        .push(MorphTrack::for_node(node(1, "b"), 0, ramp(1.0, 0.0)));
    clip
}

fn spawn_mesh(app: &mut App, parent: Entity, index: u32, targets: usize) -> Entity {
    app.world_mut()
        .spawn((
            SourceNode {
                index,
                name: format!("n{index}"),
            },
            MorphWeights(vec![0.0; targets]),
            ChildOf(parent),
        ))
        .id()
}

fn weights(app: &App, entity: Entity) -> Vec<f32> {
    app.world()
        .get::<MorphWeights>(entity)
        .expect("weights")
        .0
        .clone()
}

fn assert_close(got: &[f32], want: &[f32], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: {got:?} vs {want:?}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-5, "{what}: {got:?} vs {want:?}");
    }
}

#[test]
fn each_mesh_gets_its_own_weights_at_start_mid_and_end() {
    let mut app = app();
    let root = app.world_mut().spawn(Transform::IDENTITY).id();
    let a = spawn_mesh(&mut app, root, 0, 2);
    let b = spawn_mesh(&mut app, root, 1, 1);
    let mut player = NodeMorphPlayer::new(Arc::new(two_mesh_clip()), false);
    player.playing = false;
    app.world_mut().entity_mut(root).insert(player);

    for (time, want_a, want_b) in [
        (0.0, [0.0, 0.5], [1.0]),
        (0.5, [0.5, 0.5], [0.5]),
        (1.0, [1.0, 0.5], [0.0]),
    ] {
        app.world_mut()
            .entity_mut(root)
            .get_mut::<NodeMorphPlayer>()
            .expect("player")
            .time = time;
        app.update();
        assert_close(&weights(&app, a), &want_a, &format!("mesh a at {time}"));
        assert_close(&weights(&app, b), &want_b, &format!("mesh b at {time}"));
    }
}

#[test]
fn a_playing_player_loops_and_holds_per_mode() {
    let mut app = app();
    let root = app.world_mut().spawn(Transform::IDENTITY).id();
    let b = spawn_mesh(&mut app, root, 1, 1);
    app.world_mut()
        .entity_mut(root)
        .insert(NodeMorphPlayer::new(Arc::new(two_mesh_clip()), false));
    // 70 steps of 16 ms pass the 1 s clip end: a one-shot holds the last value.
    for _ in 0..70 {
        app.update();
    }
    assert_close(&weights(&app, b), &[0.0], "held at the end");
}

#[test]
fn expression_layers_compose_over_the_node_weights() {
    let mut app = app();
    let root = app.world_mut().spawn(Transform::IDENTITY).id();
    let a = spawn_mesh(&mut app, root, 0, 2);
    app.world_mut().entity_mut(a).insert((
        MorphTargetNames::new(["lift", "blink"]),
        MorphLayers {
            layers: vec![MorphLayer::new("blink", MorphLayerMode::Override).with("blink", 1.0)],
            ..MorphLayers::default()
        },
    ));
    let mut player = NodeMorphPlayer::new(Arc::new(two_mesh_clip()), false);
    player.playing = false;
    player.time = 0.5;
    app.world_mut().entity_mut(root).insert(player);
    for _ in 0..2 {
        app.update();
    }
    // Clip: lift 0.5 (ramp at mid), blink 0.5 -> the override layer takes blink to 1.
    assert_close(&weights(&app, a), &[0.5, 1.0], "layer over clip");
}

#[test]
fn an_unbound_clip_keeps_driving_the_rig_and_bound_tracks_reach_their_nodes() {
    let mut app = app();
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![translated(Vec3::ZERO)])
            .expect("skeleton"),
    );
    let rig = app.world_mut().spawn(Transform::IDENTITY).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut clip = AnimationClip::empty("mixed", 1.0);
    // An old-style track (no node): the rig's own weights, as before.
    clip.morph_tracks
        .push(MorphTrack::new(0, TrackCurve::Static(0.2)));
    clip.morph_tracks
        .push(MorphTrack::for_node(node(2, "face"), 1, TrackCurve::Static(0.4)));
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::single(clip, true)));
    let face = spawn_mesh(&mut app, rig, 2, 2);
    let other = spawn_mesh(&mut app, rig, 7, 2);
    for _ in 0..3 {
        app.update();
    }
    assert_close(&weights(&app, rig), &[0.2], "rig keeps the unbound track");
    assert_close(&weights(&app, face), &[0.0, 0.4], "face gets its node track");
    assert_close(&weights(&app, other), &[0.0, 0.0], "an unnamed node is untouched");
}
