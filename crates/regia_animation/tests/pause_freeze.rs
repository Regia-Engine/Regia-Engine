// SPDX-License-Identifier: MPL-2.0
//! The gameplay pause freezes animation (ADR-10105): a rig mid-motion (a falling spring chain over
//! a playing animator) keeps every joint's global matrix and its skin palette bit for bit across
//! 300 paused frames, and after the pause the pose is the one a never-paused rig has after the same
//! number of unpaused frames.

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

fn pose_bits(app: &App, rig: Entity) -> Vec<u32> {
    let globals = &app.world().get::<JointGlobals>(rig).expect("globals").globals;
    let palette = &app.world().get::<SkinPalette>(rig).expect("palette").matrices;
    globals
        .iter()
        .chain(palette.iter())
        .flat_map(|m| m.to_cols_array())
        .map(f32::to_bits)
        .collect()
}

#[test]
fn a_paused_rig_holds_its_pose_and_resumes_where_it_stopped() {
    const BEFORE: usize = 20;
    const AFTER: usize = 30;
    let (mut app, rig, _) = app_with_rig(Some(SpringBones::new(setup(0.0))));
    for _ in 0..BEFORE {
        app.update();
    }
    let moving = pose_bits(&app, rig);
    app.update();
    assert_ne!(pose_bits(&app, rig), moving, "the chain is still falling");
    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    let held = pose_bits(&app, rig);
    for frame in 1..=300 {
        app.update();
        assert_eq!(pose_bits(&app, rig), held, "paused frame {frame}: the pose moved");
    }
    app.world_mut().resource_mut::<Time>().paused = false;
    for _ in 0..AFTER {
        app.update();
    }
    let (mut control, control_rig, _) = app_with_rig(Some(SpringBones::new(setup(0.0))));
    for _ in 0..BEFORE + 1 + AFTER {
        control.update();
    }
    assert_eq!(
        pose_bits(&app, rig),
        pose_bits(&control, control_rig),
        "after the pause the pose is where a never-paused rig is"
    );
}
