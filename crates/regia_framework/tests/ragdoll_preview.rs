// SPDX-License-Identifier: MPL-2.0
//! The physics-asset editor's drop test (ADR-2133): `preview_drop` simulates a profile limp onto a
//! floor in a private world, deterministically, and reports rest, speed, depth and the final pose.

use std::sync::Arc;

use regia_animation::Skeleton;
use regia_animation::pose::translated;
use regia_core::math::Vec3;
use regia_framework::ragdoll::preview::{
    MAX_PREVIEW_STEPS, RagdollPreviewError, RagdollPreviewOptions, preview_drop,
};
use regia_framework::ragdoll::{RagdollProfile, RagdollShape};

/// A small T-posed humanoid facing -Z: hips, spine, chest, head, arms, legs.
fn humanoid() -> Arc<Skeleton> {
    let names = [
        "hips",
        "spine",
        "chest",
        "neck",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "upperarm_r",
        "lowerarm_r",
        "hand_r",
        "thigh_l",
        "calf_l",
        "foot_l",
        "thigh_r",
        "calf_r",
        "foot_r",
    ];
    let parents = vec![
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(3),
        Some(2),
        Some(5),
        Some(6),
        Some(2),
        Some(8),
        Some(9),
        Some(0),
        Some(11),
        Some(12),
        Some(0),
        Some(14),
        Some(15),
    ];
    let t = |x: f32, y: f32| translated(Vec3::new(x, y, 0.0));
    let binds = vec![
        t(0.0, 1.0),
        t(0.0, 0.15),
        t(0.0, 0.25),
        t(0.0, 0.25),
        t(0.0, 0.1),
        t(0.2, 0.2),
        t(0.3, 0.0),
        t(0.27, 0.0),
        t(-0.2, 0.2),
        t(-0.3, 0.0),
        t(-0.27, 0.0),
        t(0.1, -0.05),
        t(0.0, -0.45),
        t(0.0, -0.42),
        t(-0.1, -0.05),
        t(0.0, -0.45),
        t(0.0, -0.42),
    ];
    Arc::new(
        Skeleton::from_local_bind(names.iter().map(|n| n.to_string()).collect(), parents, binds)
            .expect("skeleton"),
    )
}

fn limited(skeleton: &Skeleton) -> RagdollProfile {
    RagdollProfile::humanoid_limited(skeleton, 75.0, Vec3::NEG_Z)
}

#[test]
fn a_limited_humanoid_falls_settles_on_the_floor_and_replays_exactly() {
    let skeleton = humanoid();
    let profile = limited(&skeleton);
    let options = RagdollPreviewOptions {
        steps: 900,
        push: Vec3::new(0.0, 0.0, 30.0),
        ..Default::default()
    };
    let a = preview_drop(skeleton.clone(), &profile, &options).expect("runs");
    assert!(a.finite);
    assert_eq!(a.bodies.len(), profile.bones.len());
    assert!(a.settled_at.is_some(), "it came to rest: {a:?}");
    assert_eq!(a.face_up, Some(true), "pushed backwards, it lies face up");
    assert!(a.lowest > -0.05, "no body sank through the floor: {}", a.lowest);
    assert!(
        a.bodies.iter().all(|b| b.position.y < 0.6),
        "it lies down: {:?}",
        a.bodies
    );
    assert_eq!(a.bodies[0].parent, None, "the root comes first");
    let b = preview_drop(skeleton, &profile, &options).expect("runs");
    assert_eq!(a, b, "the same inputs give the same fall");
}

#[test]
fn heavier_bodies_or_another_push_change_the_fall() {
    let skeleton = humanoid();
    let profile = limited(&skeleton);
    let options = RagdollPreviewOptions {
        steps: 120,
        ..Default::default()
    };
    let plain = preview_drop(skeleton.clone(), &profile, &options).expect("runs");
    let pushed = preview_drop(
        skeleton.clone(),
        &profile,
        &RagdollPreviewOptions {
            push: Vec3::new(0.0, 0.0, -30.0),
            ..options
        },
    )
    .expect("runs");
    assert_ne!(plain.digest, pushed.digest);
    let mut loose = profile.clone();
    for bone in &mut loose.bones {
        bone.limits = None;
    }
    let unlimited = preview_drop(skeleton, &loose, &options).expect("runs");
    assert_ne!(plain.digest, unlimited.digest, "limits shape the fall");
}

#[test]
fn bad_options_and_foreign_profiles_are_typed_errors() {
    let skeleton = humanoid();
    let profile = limited(&skeleton);
    let code = |o: RagdollPreviewOptions, p: &RagdollProfile| {
        preview_drop(skeleton.clone(), p, &o)
            .map(|_| ())
            .map_err(|e| e.code())
    };
    let bad = Err("ragdoll_preview_bad_options");
    let d = RagdollPreviewOptions::default();
    assert_eq!(code(RagdollPreviewOptions { steps: 0, ..d }, &profile), bad);
    assert_eq!(
        code(
            RagdollPreviewOptions {
                steps: MAX_PREVIEW_STEPS + 1,
                ..d
            },
            &profile
        ),
        bad
    );
    assert_eq!(
        code(
            RagdollPreviewOptions {
                drop_height: f32::NAN,
                ..d
            },
            &profile
        ),
        bad
    );
    assert_eq!(
        code(
            RagdollPreviewOptions {
                drop_height: 21.0,
                ..d
            },
            &profile
        ),
        bad
    );
    assert_eq!(
        code(
            RagdollPreviewOptions {
                push: Vec3::splat(f32::INFINITY),
                ..d
            },
            &profile
        ),
        bad
    );
    let mut foreign = profile.clone();
    for bone in &mut foreign.bones {
        bone.joint = format!("other_{}", bone.joint);
    }
    assert_eq!(code(d, &foreign), Err("ragdoll_preview_no_bodies"));
    assert_eq!(RagdollPreviewError::NoBodies.code(), "ragdoll_preview_no_bodies");
}

/// A profile with a box chest and a sphere head drops, settles above the floor, replays exactly and falls
/// differently from the all-capsule profile (ADR-2133 § 6).
#[test]
fn box_and_sphere_bodies_drop_settle_and_replay() {
    let skeleton = humanoid();
    let capsules = limited(&skeleton);
    let mut mixed = capsules.clone();
    for bone in &mut mixed.bones {
        match bone.joint.as_str() {
            "chest" => {
                bone.shape = RagdollShape::Box {
                    width: 0.34,
                    depth: 0.22,
                }
            }
            "head" => bone.shape = RagdollShape::Sphere,
            _ => {}
        }
    }
    assert_ne!(mixed, capsules, "the humanoid has a chest and a head");
    let options = RagdollPreviewOptions {
        steps: 900,
        ..Default::default()
    };
    let a = preview_drop(skeleton.clone(), &mixed, &options).expect("runs");
    assert!(a.finite);
    assert!(a.settled_at.is_some(), "it came to rest: {a:?}");
    assert!(a.lowest > -0.05, "no body sank through the floor: {}", a.lowest);
    let b = preview_drop(skeleton.clone(), &mixed, &options).expect("runs");
    assert_eq!(a, b, "the same inputs give the same fall");
    let c = preview_drop(skeleton, &capsules, &options).expect("runs");
    assert_ne!(a.digest, c.digest, "the shapes change the fall");
}
