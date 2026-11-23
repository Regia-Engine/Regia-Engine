// SPDX-License-Identifier: MPL-2.0
//! `sample_into` through the public API only, the way a host with its own world uses it: a
//! 109-joint rig, a clip over the first 67 joints, Replace then Additive.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::skeleton::Skeleton;
use regia_animation::{SampleMode, sample_into};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

fn rig(world: &mut World) -> (Entity, Vec<Entity>, Arc<Skeleton>) {
    let names = (0..109).map(|i| format!("bone_{i}")).collect();
    let parents = (0..109u32).map(|i| i.checked_sub(1)).collect();
    let bind = (0..109).map(|_| translated(Vec3::new(0.0, 0.25, 0.0))).collect();
    let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("skeleton"));
    let root = world.spawn(Transform::default()).id();
    let binding = spawn_skeleton(world, skeleton.clone());
    let joints = binding.joints.clone();
    world.entity_mut(root).insert(binding);
    (root, joints, skeleton)
}

fn partial_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("partial", 2.0);
    for joint in 0..67 {
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: vec![0.0, 2.0],
                values: vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::from_rotation_z(0.5)),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip
}

#[test]
fn replace_writes_the_first_67_joints_and_leaves_the_rest() {
    let mut world = World::new();
    let (root, joints, _) = rig(&mut world);
    let written = sample_into(&mut world, root, &partial_clip(), 1.0, SampleMode::Replace);
    assert_eq!(written, 67);
    for (i, joint) in joints.iter().enumerate() {
        let t = world.get::<Transform>(*joint).expect("joint");
        if i < 67 {
            assert!((t.translation.x - 1.0).abs() < 1e-5, "joint {i}");
            assert!(t.rotation.abs_diff_eq(Quat::from_rotation_z(0.5), 1e-5));
        } else {
            assert_eq!(
                t.translation,
                Vec3::new(0.0, 0.25, 0.0),
                "joint {i} must stay at bind"
            );
            assert_eq!(t.rotation, Quat::IDENTITY);
        }
    }
}

#[test]
fn additive_layers_the_delta_from_bind_and_stacks() {
    let mut world = World::new();
    let (root, joints, _) = rig(&mut world);
    let clip = partial_clip();
    sample_into(&mut world, root, &clip, 1.0, SampleMode::Replace);
    // The clip's delta from bind at t = 1: x +1, y −0.25 (the track replaces y with 0), z-roll 0.5.
    sample_into(&mut world, root, &clip, 1.0, SampleMode::Additive);
    let t = world.get::<Transform>(joints[0]).expect("joint");
    assert!((t.translation.x - 2.0).abs() < 1e-5, "{t:?}");
    assert!((t.translation.y - (-0.25)).abs() < 1e-5, "{t:?}");
    assert!(t.rotation.abs_diff_eq(Quat::from_rotation_z(1.0), 1e-4), "{t:?}");
    let tail = world.get::<Transform>(joints[100]).expect("joint");
    assert_eq!(tail.translation, Vec3::new(0.0, 0.25, 0.0));
}
