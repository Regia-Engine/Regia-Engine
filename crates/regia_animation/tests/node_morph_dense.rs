// SPDX-License-Identifier: MPL-2.0
//! Per-node morph apply does not allocate a dense weight vector for a mesh no track names,
//! and it reuses the vector of a mesh a track does name.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{MorphNode, MorphTrack, TrackCurve};
use regia_animation::hot_count::{self, node_morph_dense_allocs};
use regia_animation::prelude::*;
use regia_app::prelude::*;
use regia_scene::Transform;

fn clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("face", 1.0);
    clip.morph_tracks.push(MorphTrack::for_node(
        MorphNode {
            index: 0,
            name: "a".into(),
        },
        0,
        TrackCurve::Static(0.5),
    ));
    clip
}

#[test]
fn unmatched_nodes_do_not_allocate_and_the_matched_vector_is_reused() {
    let _guard = hot_count::lock_for_test();
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let root = app.world_mut().spawn(Transform::default()).id();
    let matched = app
        .world_mut()
        .spawn((
            SourceNode {
                index: 0,
                name: "a".into(),
            },
            MorphWeights(vec![0.0]),
            ChildOf(root),
        ))
        .id();
    let other = app
        .world_mut()
        .spawn((
            SourceNode {
                index: 1,
                name: "b".into(),
            },
            MorphWeights(vec![7.0]),
            ChildOf(root),
        ))
        .id();
    app.world_mut()
        .entity_mut(root)
        .insert(NodeMorphPlayer::new(Arc::new(clip()), true));
    app.update();
    let before = node_morph_dense_allocs();
    app.update();
    let delta = node_morph_dense_allocs() - before;
    assert_eq!(delta, 0, "the second apply allocated {delta} dense morph vectors");
    let matched = app
        .world()
        .get::<MorphWeights>(matched)
        .expect("matched")
        .0
        .clone();
    let other = app.world().get::<MorphWeights>(other).expect("other").0.clone();
    assert!(
        (matched[0] - 0.5).abs() < 1e-6,
        "the matched mesh still receives its track ({matched:?})"
    );
    assert_eq!(other, vec![7.0], "an unmatched mesh keeps its weights");
}
