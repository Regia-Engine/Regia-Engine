// SPDX-License-Identifier: MPL-2.0
//! The evaluate system builds the skeleton bind pose once and reuses it.
//!
//! `EvalContext::new` used to collect a fresh bind pose on every evaluation. The second fixed
//! step of one rig must not build another.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::hot_count::{self, bind_pose_builds};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

fn clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("move", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::ZERO, Vec3::X],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

#[test]
fn the_second_evaluation_does_not_rebuild_the_bind_pose() {
    let _guard = hot_count::lock_for_test();
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
        .expect("skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::single(clip(), true)))
        .id();
    app.update();
    let before = bind_pose_builds();
    let moved = app.world().get::<SkinnedMesh>(rig).expect("mesh").joints[0];
    let at = app.world().get::<Transform>(moved).expect("joint").translation;
    app.update();
    let again = bind_pose_builds();
    assert_eq!(
        again, before,
        "the second evaluation rebuilt the bind pose ({before} -> {again})"
    );
    let later = app.world().get::<Transform>(moved).expect("joint").translation;
    assert!(
        (later.x - at.x).abs() > 1e-4,
        "the second step must still advance the clip (at {at:?}, later {later:?})"
    );
}
