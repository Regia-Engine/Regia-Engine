// SPDX-License-Identifier: MPL-2.0
//! Edge cases from the deep-zai-animation review (v0.5.1): malformed skeleton hierarchies never
//! panic (finding 2), a negative LOD rate does not freeze an animator for good (finding 3), and
//! a synced blend space without a usable shared clock still lets only its dominant sample fire
//! notifies (finding 4).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::aim_ik::{AimChain, solve_body_aim};
use regia_animation::bind_pose::{BindPoseError, InstanceBindPose};
use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::chain_ik::{ChainIkSettings, JointChain, solve_ccd, solve_fabrik};
use regia_animation::components::spawn_skeleton;
use regia_animation::eval::{EvalContext, evaluate_graph};
use regia_animation::graph::BlendSpace1DNode;
use regia_animation::ik::IkChain;
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::physics_blend::{PhysicsPoseBlend, blend_physics_pose};
use regia_animation::pose::{Pose, translated};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_scene::Transform;

// ---------------------------------------------------------------------------------------------
// Finding 2: a skeleton that skipped validation.

/// Three joints whose parent list is malformed: `b`'s parent is `c`, a joint after it, and
/// `c` has no parent entry at all (the list is one short).
fn forward_parent_skeleton() -> Skeleton {
    Skeleton {
        joint_names: vec!["a".into(), "b".into(), "c".into()],
        parents: vec![None, Some(2)],
        bind_pose: vec![translated(Vec3::Y); 3],
        inverse_bind_matrices: Vec::new(),
        sockets: Vec::new(),
    }
}

/// Three joints and no parent list.
fn parentless_skeleton() -> Skeleton {
    Skeleton {
        joint_names: vec!["a".into(), "b".into(), "c".into()],
        parents: Vec::new(),
        bind_pose: vec![translated(Vec3::Y); 3],
        inverse_bind_matrices: Vec::new(),
        sockets: Vec::new(),
    }
}

#[test]
fn chain_solvers_refuse_a_chain_through_a_forward_parent() {
    let skeleton = forward_parent_skeleton();
    // `between` follows the parent links it finds: b → c, so the chain is [c, b].
    let chain = JointChain::between(&skeleton, "c", "b").expect("the links form a chain");
    let target = Vec3::new(1.0, 1.0, 0.0);
    let mut pose = Pose::from_identity(3);
    let before = pose.clone();
    let fabrik = solve_fabrik(&skeleton, &mut pose, &chain, target, ChainIkSettings::default());
    assert_eq!(
        fabrik.error,
        f32::INFINITY,
        "an invalid chain reports infinite error"
    );
    assert_eq!(pose, before, "the pose stays unchanged");
    let ccd = solve_ccd(&skeleton, &mut pose, &chain, target, ChainIkSettings::default());
    assert_eq!(ccd.error, f32::INFINITY);
    assert_eq!(pose, before, "the pose stays unchanged");
}

#[test]
fn body_aim_on_a_skeleton_without_parents_does_not_panic() {
    let skeleton = parentless_skeleton();
    let aim = AimChain {
        joints: vec![0, 1],
        shares: Vec::new(),
        limits: Vec::new(),
        aim_joint: 2,
        aim_axis: Vec3::NEG_Z,
    };
    let mut pose = skeleton.bind_pose_pose();
    let report = solve_body_aim(&skeleton, &mut pose, &aim, Vec3::new(1.0, 2.0, -3.0), &[], 1.0);
    assert!(!report.angle_error.is_nan(), "{report:?}");
    assert!(pose.rotations.iter().all(|q| q.is_finite()));
}

#[test]
fn place_hand_on_a_skeleton_without_parents_does_not_panic() {
    let skeleton = parentless_skeleton();
    let chain = IkChain {
        root: 0,
        mid: 1,
        tip: 2,
    };
    let mut pose = skeleton.bind_pose_pose();
    let grip = Quat::from_rotation_y(0.5);
    let target = Mat4::from_rotation_translation(grip, Vec3::new(0.5, 1.0, 0.0));
    let _error = regia_animation::aim_ik::place_hand(&skeleton, &mut pose, &chain, target, None, 1.0);
    // The tip has no parent: its local rotation is the grip rotation itself.
    assert!(
        pose.rotations[2].abs_diff_eq(grip, 1e-6),
        "{:?}",
        pose.rotations[2]
    );
}

#[test]
fn ragdoll_blend_on_a_skeleton_without_parents_does_not_panic() {
    let skeleton = parentless_skeleton();
    let mut pose = skeleton.bind_pose_pose();
    let mut blend = PhysicsPoseBlend::new(3);
    blend.joints[1] = Some((Vec3::new(0.0, 4.0, 0.0), Quat::IDENTITY));
    blend.weights[1] = 1.0;
    blend_physics_pose(&skeleton, &mut pose, &blend);
    // Every joint is a root: the simulated object-space translation is the local one.
    assert!(
        pose.translations[1].abs_diff_eq(Vec3::new(0.0, 4.0, 0.0), 1e-6),
        "{:?}",
        pose.translations[1]
    );
}

#[test]
fn instance_bind_pose_over_a_short_parent_list_is_a_typed_error() {
    for (what, skeleton) in [
        ("two of three parents", forward_parent_skeleton()),
        ("no parents", parentless_skeleton()),
    ] {
        let short = |result: &Result<InstanceBindPose, BindPoseError>| {
            matches!(
                result,
                Err(BindPoseError::WrongLength {
                    what: "parents",
                    expected: 3,
                    ..
                })
            )
        };
        let positions = [Vec3::ZERO, Vec3::Y, Vec3::new(0.0, 2.0, 0.0)];
        let refit = InstanceBindPose::from_joint_positions(&skeleton, &positions);
        assert!(short(&refit), "{what}: refit: {refit:?}");
        let new = InstanceBindPose::new(&skeleton, vec![LocalBind::default(); 3]);
        assert!(short(&new), "{what}: new: {new:?}");
    }
}

#[test]
fn instance_bind_pose_refit_treats_a_forward_parent_as_a_root() {
    // A full parent list, but `b` names `c`, which comes after it: like `Skeleton::globals`,
    // the refit treats `b` as a root instead of reading a global it has not built yet.
    let mut skeleton = forward_parent_skeleton();
    skeleton.parents.push(Some(0));
    let positions = [Vec3::ZERO, Vec3::new(1.0, 2.0, 3.0), Vec3::new(0.0, 2.0, 0.0)];
    let refit = InstanceBindPose::from_joint_positions(&skeleton, &positions)
        .expect("a forward parent counts as a root");
    assert!(
        refit.bind_pose[1].translation.abs_diff_eq(positions[1], 1e-6),
        "{:?}",
        refit.bind_pose[1]
    );
    assert!(refit.bind_pose[2].translation.abs_diff_eq(positions[2], 1e-6));
}

// ---------------------------------------------------------------------------------------------
// Finding 3: a negative LOD rate.

#[derive(Resource, Default)]
struct Seen(Vec<String>);

fn collect(mut reader: MessageReader<AnimNotify>, mut seen: ResMut<Seen>) {
    seen.0.extend(reader.read().map(|n| n.name.clone()));
}

#[test]
fn a_negative_lod_rate_does_not_freeze_the_animator_after_it_is_fixed() {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.init_resource::<Seen>();
    app.add_systems(
        Update,
        collect.after(regia_animation::systems::evaluate_animators),
    );
    app.update();
    let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
        .expect("valid skeleton");
    // A notify every 0.1 s of a looping clip: each evaluated 0.1 s step crosses one.
    let mut clip = AnimationClip::empty("clip", 1.0);
    clip.notifies = (0..10)
        .map(|i| Notify {
            time: 0.05 + 0.1 * i as f32,
            name: "tick".into(),
            payload: None,
        })
        .collect();
    let mut animator = Animator::single(clip, true);
    // A sign error in game data: far animators are meant to run at a quarter rate.
    animator.update_mode = UpdateMode::Lod {
        near: 1.0,
        mid: 2.0,
        far_rate: -0.25,
    };
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator))
        .id();
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(10.0, 0.0, 0.0),
    });
    for _ in 0..40 {
        app.update();
    }
    assert!(
        app.world().resource::<Seen>().0.is_empty(),
        "a far animator with a negative rate does not advance"
    );
    // The camera walks up to the rig: full rate, so the very next frame steps the clip.
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(0.5, 0.0, 0.0),
    });
    app.update();
    assert_eq!(
        app.world().resource::<Seen>().0.len(),
        1,
        "the first near frame evaluates; the negative far frames left no debt to pay back"
    );
    assert!(app.world().get::<Animator>(rig).is_some());
}

// ---------------------------------------------------------------------------------------------
// Finding 4: a synced blend space without a shared clock.

#[test]
fn a_synced_blend_space_without_a_shared_clock_fires_only_the_dominant_samples_notifies() {
    let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![translated(Vec3::ZERO)])
        .expect("skeleton");
    let mut clips = ClipLibrary::new();
    // The dominant sample is a zero-length pose clip (no clock to share); the other sample
    // carries a footstep near its start.
    clips.insert(AnimationClip::empty("pose", 0.0));
    let mut walk = AnimationClip::empty("walk", 1.0);
    walk.notifies.push(Notify {
        time: 0.02,
        name: "walk_step".into(),
        payload: None,
    });
    clips.insert(walk);
    let graph = AnimGraph {
        // Just short of the midpoint: `pose` dominates, `walk` is still audible (≥ 0.5 − 1e-4).
        parameters: vec![ParamDecl("Speed".into(), ParamValue::Float(0.49995))],
        root: AnimNode::BlendSpace1D(BlendSpace1DNode {
            space: BlendSpace1D {
                param: "Speed".into(),
                samples: vec![
                    BlendSample1D {
                        value: 0.0,
                        clip: "pose".into(),
                    },
                    BlendSample1D {
                        value: 1.0,
                        clip: "walk".into(),
                    },
                ],
            },
            sync_group: Some("gait".into()),
        }),
    };
    let params = graph.default_parameters();
    let mut instance = GraphInstance::from_node(&graph.root);
    let sockets = HashMap::new();
    let mut fired = Vec::new();
    let mut scratch = Vec::new();
    for _ in 0..3 {
        let mut pose = Pose::from_identity(1);
        let mut ctx = EvalContext::new(
            &skeleton,
            &clips,
            &params,
            1.0 / 30.0,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        );
        evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose).expect("evaluate");
        fired.extend(ctx.notifies.iter().map(|n| n.name.clone()));
        scratch = ctx.finish().scratch;
    }
    assert!(
        fired.is_empty(),
        "a synced space hears only its dominant sample (ADR-871), even without a shared clock: \
         {fired:?}"
    );
}
