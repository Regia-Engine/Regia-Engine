// SPDX-License-Identifier: MPL-2.0
//! Spring bones through the plugin: a rig with an animator and a chain; the joint transforms
//! and the skin palette carry the simulated rotations, a rig without the component is unchanged.

use std::sync::Arc;

use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_animation::spring::{SpringChain, SpringCollider};
use regia_animation::{ColliderShape, JointGlobals};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

fn rope() -> Skeleton {
    let names: Vec<String> = ["anchor", "c0", "c1", "c2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let binds = (0..4)
        .map(|i| LocalBind {
            translation: if i == 0 { Vec3::ZERO } else { Vec3::X },
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, vec![None, Some(0), Some(1), Some(2)], binds).expect("valid skeleton")
}

fn app_with_rig(springs: Option<SpringBones>) -> (App, Entity, Vec<Entity>) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_micros(16_667),
    ));
    let skeleton = Arc::new(rope());
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton);
    let joints = binding.joints.clone();
    app.world_mut().entity_mut(rig).insert(binding);
    app.world_mut().entity_mut(rig).insert(Animator::single(
        regia_animation::AnimationClip::empty("hold", 1.0),
        true,
    ));
    if let Some(springs) = springs {
        app.world_mut().entity_mut(rig).insert(springs);
    }
    (app, rig, joints)
}

fn setup(stiffness: f32) -> SpringBoneSetup {
    SpringBoneSetup {
        chains: vec![SpringChain {
            name: "rope".into(),
            joints: vec!["c0".into(), "c1".into(), "c2".into()],
            stiffness,
            damping: 0.15,
            gravity_power: 9.81,
            hit_radius: 0.02,
            colliders: vec!["post".into()],
            ..SpringChain::default()
        }],
        colliders: vec![SpringCollider {
            name: "post".into(),
            joint: "anchor".into(),
            offset: Vec3::new(1.0, -1.4, 0.0),
            shape: ColliderShape::Capsule {
                radius: 0.2,
                tail: Vec3::new(1.0, -3.0, 0.0),
            },
        }],
        ..SpringBoneSetup::default()
    }
}

#[test]
fn a_spring_chain_falls_and_the_palette_follows() {
    let (mut app, rig, joints) = app_with_rig(Some(SpringBones::new(setup(0.0))));
    for _ in 0..360 {
        app.update();
    }
    // The chain hangs below the anchor (c1 is the second spring joint).
    let globals = app
        .world()
        .get::<JointGlobals>(rig)
        .expect("globals written")
        .globals
        .clone();
    let tip = globals[3].w_axis.truncate();
    assert!(tip.y < -1.5, "the tip fell: {tip:?}");
    let palette = app.world().get::<SkinPalette>(rig).expect("palette written");
    assert_eq!(palette.matrices.len(), 4);
    // The joint entity carries the simulated rotation, not the bind one.
    let c0 = app
        .world()
        .get::<Transform>(joints[1])
        .copied()
        .unwrap_or_default();
    assert!(c0.rotation.angle_between(regia_core::math::Quat::IDENTITY) > 0.5);
    let springs = app.world().get::<SpringBones>(rig).expect("springs");
    assert!(springs.problems().is_empty(), "{:?}", springs.problems());
    assert!(springs.chain_positions("rope").len() == 3);
}

#[test]
fn a_rig_without_springs_and_a_stiff_chain_keep_the_pose() {
    let (mut plain, plain_rig, _) = app_with_rig(None);
    let (mut stiff, stiff_rig, _) = app_with_rig(Some(SpringBones::new(setup(1.0))));
    for _ in 0..60 {
        plain.update();
        stiff.update();
    }
    let a = plain
        .world()
        .get::<JointGlobals>(plain_rig)
        .expect("plain globals")
        .globals
        .clone();
    let b = stiff
        .world()
        .get::<JointGlobals>(stiff_rig)
        .expect("stiff globals")
        .globals
        .clone();
    for (x, y) in a.iter().zip(&b) {
        assert!(x.abs_diff_eq(*y, 1e-4), "{x:?} vs {y:?}");
    }
}

#[test]
fn disabling_the_component_shows_the_animation() {
    let mut springs = SpringBones::new(setup(0.0));
    springs.enabled = false;
    let (mut app, rig, _) = app_with_rig(Some(springs));
    for _ in 0..120 {
        app.update();
    }
    let g = app
        .world()
        .get::<JointGlobals>(rig)
        .expect("globals")
        .globals
        .clone();
    assert!((g[3].w_axis.truncate() - Vec3::new(3.0, 0.0, 0.0)).length() < 1e-4);
}

/// audit-v040 M4: a hitch is caught up in later steps instead of dropping time. One 0.25 s
/// call and fifteen `1/60` s calls feed the same total time; after the accumulator drains
/// (zero-delta calls run the backlog out), both chains ran the same number of substeps and
/// reached bit-identical rotations. Before the fix the 0.25 s call ran 8 substeps and threw
/// the rest away, ending seven steps behind.
#[test]
fn a_hitch_catches_up_instead_of_dropping_substeps() {
    const STEP: f32 = 1.0 / 60.0;
    let skeleton = rope();
    let rig = Transform::default();

    let run = |deltas: &[f32]| {
        let mut springs = SpringBones::new(setup(0.0));
        let mut pose = skeleton.bind_pose_pose();
        let mut total_steps = 0u32;
        for &d in deltas {
            // The animator rewrites the pose every evaluation; the spring reads the fresh
            // animation as its target and writes simulated rotations onto it afterwards.
            pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, d, None);
            total_steps += springs.steps_last_frame();
        }
        // Drain the backlog with zero-delta calls (bounded: at most MAX_SUBSTEPS per call).
        for _ in 0..64 {
            pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, 0.0, None);
            total_steps += springs.steps_last_frame();
            if springs.steps_last_frame() == 0 {
                break;
            }
        }
        (total_steps, pose.rotations.clone())
    };

    // Equal total fed time (0.25 + 14/60 == 29/60), chunked two ways.
    let mut hitched = vec![0.25f32];
    hitched.extend(std::iter::repeat_n(STEP, 14));
    let (steps_hitched, rot_hitched) = run(&hitched);
    let (steps_smooth, rot_smooth) = run(&[STEP; 29]);

    assert_eq!(
        steps_hitched, steps_smooth,
        "both pacings run the same total number of substeps"
    );
    assert!(
        steps_smooth >= 29,
        "twenty-nine 1/60 s frames run at least twenty-nine substeps, got {steps_smooth}"
    );
    assert_eq!(
        rot_hitched, rot_smooth,
        "the end pose is bit-identical however the frame time was split"
    );
}
