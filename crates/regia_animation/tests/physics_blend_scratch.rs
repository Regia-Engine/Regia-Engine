// SPDX-License-Identifier: MPL-2.0
//! An active ragdoll blend reuses its matrix scratch.
//!
//! Two evaluating frames with one simulated joint must not allocate the animated-global
//! buffer or the blended-matrix buffer the second time. The blended rotation still matches
//! a fresh solve.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::hot_count::{self, physics_scratch_allocs};
use regia_animation::physics_blend::{PhysicsPoseBlend, blend_physics_pose};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

fn skeleton() -> Skeleton {
    let mut names = Vec::new();
    let mut parents = Vec::new();
    let mut binds = Vec::new();
    for i in 0..32 {
        names.push(format!("j{i}"));
        parents.push(if i == 0 { None } else { Some((i - 1) as u32) });
        binds.push(LocalBind {
            translation: Vec3::Y * 0.05,
            ..LocalBind::default()
        });
    }
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

#[test]
fn the_second_ragdoll_frame_reuses_its_scratch() {
    let _guard = hot_count::lock_for_test();
    let skeleton = skeleton();
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let mut blend = PhysicsPoseBlend::new(skeleton.len());
    blend.joints[0] = Some((Vec3::new(0.1, 0.0, 0.0), Quat::from_rotation_y(0.4)));
    blend.weights[0] = 1.0;
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton.clone()));
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            blend.clone(),
        ))
        .id();
    app.update();
    let before = physics_scratch_allocs();
    app.update();
    let delta = physics_scratch_allocs() - before;
    assert_eq!(
        delta, 0,
        "the second ragdoll frame allocated {delta} matrix buffers"
    );

    let joint = app.world().get::<SkinnedMesh>(rig).expect("mesh").joints[0];
    let got = app.world().get::<Transform>(joint).expect("joint").rotation;
    let mut pose = skeleton.bind_pose_pose();
    blend_physics_pose(&skeleton, &mut pose, &blend);
    let want = pose.rotations[0];
    assert!(
        got.dot(want).abs() > 1.0 - 1e-4,
        "the published rotation does not match a fresh solve"
    );
}
