// SPDX-License-Identifier: MPL-2.0
//! Pose sampling into an arbitrary world, without the animation schedule.
//!
//! [`sample_into`] writes one clip's joint TRS at one time into the joint entities of a rig
//! (a [`SkinnedMesh`]), for hosts that scrub or preview clips outside an
//! [`Animator`](crate::Animator): a timeline widget, a thumbnail, a motion-stage preview. It
//! uses the same sampling and the same additive math as the animation path (bind pose, clip
//! tracks on top, [`Pose::delta_from`] / [`Pose::additive_from`]), so a scrubbed pose equals
//! the pose an animator plays at that time.

use regia_ecs::prelude::*;
use regia_scene::Transform;
use tracing::warn;

use crate::clip::AnimationClip;
use crate::components::SkinnedMesh;
use crate::pose::Pose;

/// How [`sample_into`] combines the clip with the joints' current transforms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SampleMode {
    /// The clip's local TRS replaces the joint's transform.
    #[default]
    Replace,
    /// The clip's delta from the skeleton's bind pose is layered on the joint's current
    /// transform at full weight (the graph's `Additive` node with weight 1).
    Additive,
}

/// Write `clip` sampled at `time` into the joint entities of the rig `skeleton_root`
/// (an entity with a [`SkinnedMesh`]) and return how many joints were written.
///
/// Only joints the clip animates are touched: joints without a track (a clip may animate the
/// first 67 of 109 joints), track targets past the skeleton or the binding, and joint entities
/// without a [`Transform`] are skipped. A root without a binding writes nothing. `time` is in
/// seconds from the clip start; it is clamped to the keys (wrap it first with
/// [`AnimationClip::advance`] to loop), and a non-finite time samples the start.
///
/// The animation schedule is not needed and not run: the rig's `SkinPalette`, `JointGlobals`,
/// notifies and root motion are not produced.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn sample_into(
    world: &mut World,
    skeleton_root: Entity,
    clip: &AnimationClip,
    time: f32,
    mode: SampleMode,
) -> usize {
    let Some(binding) = world.get::<SkinnedMesh>(skeleton_root) else {
        warn!(target: "regia_animation", "sample_into: {skeleton_root:?} has no SkinnedMesh");
        return 0;
    };
    let skeleton = binding.skeleton.clone();
    let joints = binding.joints.clone();
    let time = if time.is_finite() { time } else { 0.0 };
    let n = skeleton.len().min(joints.len());

    let bind = skeleton.bind_pose_pose();
    let mut sampled = bind.clone();
    clip.sample_pose(time, &mut sampled);

    let mut result = match mode {
        SampleMode::Replace => sampled,
        SampleMode::Additive => {
            // The joints' current transforms; a joint that cannot be read is written nowhere.
            let mut current = bind.clone();
            for (i, &entity) in joints.iter().enumerate().take(n) {
                if let Some(t) = world.get::<Transform>(entity) {
                    current.translations[i] = t.translation;
                    current.rotations[i] = t.rotation;
                    current.scales[i] = t.scale;
                }
            }
            current.additive_from(&sampled.delta_from(&bind), 1.0);
            current
        }
    };
    normalize_rotations(&mut result);

    let mut written = 0;
    for track in &clip.tracks {
        let i = track.joint as usize;
        if i >= n {
            continue;
        }
        let Some(mut transform) = world.get_mut::<Transform>(joints[i]) else {
            continue;
        };
        transform.translation = result.translations[i];
        transform.rotation = result.rotations[i];
        transform.scale = result.scales[i];
        written += 1;
    }
    written
}

fn normalize_rotations(pose: &mut Pose) {
    for rotation in &mut pose.rotations {
        *rotation = rotation.normalize();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_app::prelude::*;
    use regia_core::math::{Quat, Vec3};

    use super::*;
    use crate::clip::{JointTrack, TrackCurve};
    use crate::components::{Animator, spawn_skeleton};
    use crate::eval::{ClipLibrary, EvalContext, GraphInstance, evaluate_graph};
    use crate::graph::{AdditiveNode, AnimGraph, AnimNode, ClipNode};
    use crate::params::ParameterSet;
    use crate::pose::translated;
    use crate::skeleton::Skeleton;
    use crate::{AnimationPlugin, SkinnedMesh};

    const JOINTS: usize = 109;
    const ANIMATED: u32 = 67;

    /// A 109-joint chain whose bind pose is not identity.
    fn skeleton() -> Arc<Skeleton> {
        let names = (0..JOINTS).map(|i| format!("j{i}")).collect();
        let parents = (0..JOINTS).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
        let bind = (0..JOINTS)
            .map(|i| translated(Vec3::new(0.0, 0.1 + i as f32 * 0.01, 0.0)))
            .collect();
        Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("valid skeleton"))
    }

    /// A 1 s clip animating the first 67 joints (translation, rotation and scale keys).
    fn clip(name: &str) -> AnimationClip {
        let mut clip = AnimationClip::empty(name, 1.0);
        for joint in 0..ANIMATED {
            let f = joint as f32;
            clip.tracks.push(JointTrack {
                joint,
                translations: TrackCurve::Animated {
                    times: vec![0.0, 0.5, 1.0],
                    values: vec![
                        Vec3::new(f, 0.0, 0.0),
                        Vec3::new(f, 1.0 + f * 0.01, 0.5),
                        Vec3::new(f, 2.0, 0.0),
                    ],
                    exact_rotation: false,
                },
                rotations: TrackCurve::Animated {
                    times: vec![0.0, 1.0],
                    values: vec![Quat::IDENTITY, Quat::from_rotation_y(1.0 + f * 0.01)],
                    exact_rotation: false,
                },
                scales: TrackCurve::Animated {
                    times: vec![0.0, 1.0],
                    values: vec![Vec3::ONE, Vec3::new(1.5, 2.0, 0.5)],
                    exact_rotation: false,
                },
            });
        }
        clip
    }

    fn rig(world: &mut World, skeleton: &Arc<Skeleton>) -> (Entity, Vec<Entity>) {
        let root = world.spawn(Transform::default()).id();
        let binding = spawn_skeleton(world, skeleton.clone());
        let joints = binding.joints.clone();
        world.entity_mut(root).insert(binding);
        (root, joints)
    }

    fn transforms(world: &World, joints: &[Entity]) -> Vec<Transform> {
        joints
            .iter()
            .map(|&j| *world.get::<Transform>(j).expect("joint"))
            .collect()
    }

    fn assert_same(a: &Transform, b: &Transform, what: &str) {
        assert!(
            a.translation.abs_diff_eq(b.translation, 1e-5),
            "{what}: {a:?} vs {b:?}"
        );
        assert!(a.rotation.abs_diff_eq(b.rotation, 1e-5), "{what}: {a:?} vs {b:?}");
        assert!(a.scale.abs_diff_eq(b.scale, 1e-5), "{what}: {a:?} vs {b:?}");
    }

    /// The clip time an animator is at (read from the runtime instance).
    fn animator_time(app: &App, rig: Entity) -> f32 {
        let animator = app.world().get::<Animator>(rig).expect("animator");
        match animator.instance.as_ref() {
            Some(GraphInstance::Clip(state)) => state.time,
            other => panic!("not a clip instance: {other:?}"),
        }
    }

    /// Replace matches the animation schedule at every frame of a looping clip, including the
    /// frames around the wrap; the 42 trailing joints stay at bind.
    #[test]
    fn replace_matches_the_animation_path_across_the_loop() {
        let skeleton = skeleton();
        let clip = clip("walk");
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(370),
        ));
        let (played, played_joints) = rig(app.world_mut(), &skeleton);
        app.world_mut()
            .entity_mut(played)
            .insert(Animator::single(clip.clone(), true));
        let (sampled, sampled_joints) = rig(app.world_mut(), &skeleton);

        let mut wrapped = false;
        let mut last_time = 0.0;
        for frame in 0..10 {
            app.update();
            let time = animator_time(&app, played);
            wrapped |= time < last_time;
            last_time = time;
            let written = sample_into(app.world_mut(), sampled, &clip, time, SampleMode::Replace);
            assert_eq!(written, ANIMATED as usize);
            let expected = transforms(app.world(), &played_joints);
            let actual = transforms(app.world(), &sampled_joints);
            for joint in 0..ANIMATED as usize {
                assert_same(
                    &actual[joint],
                    &expected[joint],
                    &format!("frame {frame} t={time} joint {joint}"),
                );
            }
        }
        assert!(wrapped, "the test must cross the loop point");
        // The joints beyond the clip's tracks were never written.
        let bind = skeleton.bind_pose_pose();
        for (joint, t) in transforms(app.world(), &sampled_joints)
            .iter()
            .enumerate()
            .skip(ANIMATED as usize)
        {
            assert_eq!(t.translation, bind.translations[joint], "joint {joint} untouched");
            assert_eq!(t.rotation, bind.rotations[joint]);
            assert_eq!(t.scale, bind.scales[joint]);
        }
    }

    /// Additive matches the graph's Additive node at weight 1, at several times.
    #[test]
    fn additive_matches_the_graph_additive_node() {
        let skeleton = skeleton();
        let lean = clip("lean");
        // A time-independent base pose on the animated joints.
        let mut base = AnimationClip::empty("base", 1.0);
        for joint in 0..ANIMATED {
            base.tracks.push(JointTrack {
                joint,
                translations: TrackCurve::Static(Vec3::new(0.3, 0.2 + joint as f32 * 0.02, -0.1)),
                rotations: TrackCurve::Static(Quat::from_rotation_x(0.4)),
                scales: TrackCurve::Static(Vec3::new(1.0, 1.2, 1.0)),
            });
        }
        let mut library = ClipLibrary::new();
        library.insert(base.clone());
        library.insert(lean.clone());
        let graph = AnimGraph {
            parameters: vec![],
            root: AnimNode::Additive(AdditiveNode {
                base: Box::new(AnimNode::Clip(ClipNode {
                    clip: "base".into(),
                    looping: false,
                    speed: 1.0,
                    sync_group: None,
                })),
                additive: Box::new(AnimNode::Clip(ClipNode {
                    clip: "lean".into(),
                    looping: false,
                    speed: 1.0,
                    sync_group: None,
                })),
                weight_param: "w".into(),
            }),
        };
        let mut params = ParameterSet::new();
        params.set_float("w", 1.0);
        let sockets = Default::default();

        for time in [0.0_f32, 0.2, 0.5, 0.77, 1.0] {
            let mut world = World::new();
            let (root, joints) = rig(&mut world, &skeleton);
            // Put the base on the rig, then layer the additive clip on top.
            assert_eq!(
                sample_into(&mut world, root, &base, 0.0, SampleMode::Replace),
                ANIMATED as usize
            );
            sample_into(&mut world, root, &lean, time, SampleMode::Additive);

            let mut instance = GraphInstance::from_node(&graph.root);
            let mut ctx = EvalContext::new(
                &skeleton,
                &library,
                &params,
                time,
                Entity::PLACEHOLDER,
                &sockets,
                Vec::new(),
            );
            let mut out = Pose::from_identity(skeleton.len());
            evaluate_graph(&graph, &mut instance, &mut ctx, &mut out).expect("eval");
            ctx.finish();

            let actual = transforms(&world, &joints);
            for (joint, got) in actual.iter().enumerate().take(ANIMATED as usize) {
                let expected = Transform {
                    translation: out.translations[joint],
                    rotation: out.rotations[joint].normalize(),
                    scale: out.scales[joint],
                };
                assert_same(got, &expected, &format!("t={time} joint {joint}"));
            }
        }
    }

    #[test]
    fn out_of_range_times_clamp_and_bad_input_is_skipped() {
        let skeleton = skeleton();
        let clip = clip("walk");
        let mut world = World::new();
        let (root, joints) = rig(&mut world, &skeleton);

        let at = |world: &mut World, time: f32| {
            sample_into(world, root, &clip, time, SampleMode::Replace);
            transforms(world, &joints)
        };
        let start = at(&mut world, 0.0);
        let end = at(&mut world, 1.0);
        assert_ne!(start[3].translation, end[3].translation);
        for (time, expect) in [
            (-4.0, &start),
            (f32::NAN, &start),
            (f32::NEG_INFINITY, &start),
            (9.0, &end),
            (f32::INFINITY, &start),
        ] {
            let got = at(&mut world, time);
            assert_same(&got[3], &expect[3], &format!("t={time}"));
        }

        // A root without a binding writes nothing; a clip with a track past the skeleton, a
        // despawned joint and a track-less clip do not panic.
        let bare = world.spawn(Transform::default()).id();
        assert_eq!(sample_into(&mut world, bare, &clip, 0.5, SampleMode::Replace), 0);
        let mut wild = clip.clone();
        wild.tracks.push(JointTrack {
            joint: 9_999,
            translations: TrackCurve::Static(Vec3::ONE),
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        world.despawn(joints[5]);
        assert_eq!(
            sample_into(&mut world, root, &wild, 0.5, SampleMode::Additive),
            ANIMATED as usize - 1
        );
        let empty = AnimationClip::empty("empty", 1.0);
        assert_eq!(sample_into(&mut world, root, &empty, 0.5, SampleMode::Replace), 0);
        let binding = world.get::<SkinnedMesh>(root).expect("binding");
        assert_eq!(binding.joints.len(), JOINTS);
    }
}
