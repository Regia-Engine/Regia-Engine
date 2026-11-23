// SPDX-License-Identifier: MPL-2.0
//! Ragdoll test scene (specs/17 § 4): a T-posed humanoid on a floor with a humanoid ragdoll.
//! Limp: it falls, comes to rest on the floor with its joints intact and the mesh pose follows.
//! Powered: it holds the animation; a hit on the forearm knocks only that arm off the pose
//! (partial blend), and it recovers. Kinematic: the bodies track the animation as hit boxes.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, JointGlobals, PhysicsPoseBlend, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::ragdoll::{
    Ragdoll, RagdollBodies, RagdollHit, RagdollMode, RagdollPlugin, RagdollProfile, RagdollShape,
};
use regia_physics::{Collider, ColliderShape, PhysicsPose, RigidBody, Velocity};
use regia_scene::Transform;

const DT: f64 = 1.0 / 62.5;

const NAMES: [&str; 17] = [
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

fn humanoid() -> Skeleton {
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
    Skeleton::from_local_bind(NAMES.iter().map(|n| n.to_string()).collect(), parents, binds)
        .expect("skeleton")
}

fn scene(mode: RagdollMode) -> (App, Entity) {
    scene_with_profile(mode, |_| {})
}

/// [`scene`] with a hook over the profile's bones.
fn scene_with_profile(
    mode: RagdollMode,
    tune: impl FnOnce(&mut [regia_framework::ragdoll::RagdollBone]),
) -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins((AnimationPlugin, RagdollPlugin));
    // Floor: top face at y = 0.
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(20.0, 0.5, 20.0),
            },
            ..Collider::default()
        },
    ));
    let skeleton = Arc::new(humanoid());
    let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
    assert_eq!(ragdoll.profile.bones.len(), 11, "every humanoid bone found");
    tune(&mut ragdoll.profile.bones);
    ragdoll.mode = mode;
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            ragdoll,
        ))
        .id();
    (app, rig)
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

fn joint(name: &str) -> usize {
    NAMES.iter().position(|n| *n == name).expect("joint")
}

fn bodies(app: &App, rig: Entity) -> RagdollBodies {
    app.world()
        .get::<RagdollBodies>(rig)
        .cloned()
        .expect("ragdoll built")
}

fn body_pose(app: &App, body: Entity) -> (Vec3, Quat) {
    // Dynamic bodies: the pose after the last step; kinematic bodies are driven by Transform.
    match app.world().get::<PhysicsPose>(body) {
        Some(p) => (p.translation, p.rotation),
        None => {
            let t = app
                .world()
                .get::<Transform>(body)
                .copied()
                .expect("body transform");
            (t.translation, t.rotation)
        }
    }
}

/// Animated world position of a joint (rig at the origin).
fn animated(app: &App, rig: Entity, name: &str) -> Vec3 {
    let animator = app.world().get::<Animator>(rig).expect("animator");
    humanoid().globals(animator.evaluated_pose(), None)[joint(name)]
        .w_axis
        .truncate()
}

/// Shown (blended) world position of a joint.
fn shown(app: &App, rig: Entity, name: &str) -> Vec3 {
    let globals = app.world().get::<JointGlobals>(rig).expect("joint globals");
    globals.globals[joint(name)].w_axis.truncate()
}

fn body_of(b: &RagdollBodies, name: &str) -> Entity {
    b.bodies
        .iter()
        .find(|x| x.joint as usize == joint(name))
        .map(|x| x.entity)
        .expect("bone")
}

#[test]
fn limp_ragdoll_falls_and_rests_on_the_floor_with_joints_intact() {
    let (mut app, rig) = scene(RagdollMode::Limp);
    run(&mut app, 200); // 3.2 s
    let b = bodies(&app, rig);
    assert_eq!(b.bodies.len(), 11);
    assert!((b.weight - 1.0).abs() < 1e-4, "limp shows the simulation fully");
    for body in &b.bodies {
        let (position, rotation) = body_pose(&app, body.entity);
        assert!(
            position.y > -0.05 && position.y < 0.45,
            "{} lies on the floor: {position}",
            NAMES[body.joint as usize]
        );
        let v = app
            .world()
            .get::<Velocity>(body.entity)
            .cloned()
            .unwrap_or_default();
        assert!(v.linear.length() < 0.3, "at rest: {}", v.linear);
        // The joint to the parent body holds: the child origin sits on the parent's anchor.
        if let Some(p) = body.parent {
            let (pp, pr) = body_pose(&app, b.bodies[p].entity);
            let parent_joint = b.bodies[p].joint as usize;
            let bind = humanoid().globals(&humanoid().bind_pose_pose(), None);
            let rel = bind[parent_joint].inverse() * bind[body.joint as usize];
            let anchor = Mat4::from_rotation_translation(pr, pp).transform_point3(rel.w_axis.truncate());
            assert!(
                (anchor - position).length() < 0.03,
                "joint holds: {}",
                (anchor - position).length()
            );
        }
        assert!(rotation.is_finite());
    }
    // The mesh pose follows: the head lies near the floor.
    let head = shown(&app, rig, "head");
    assert!(head.y < 0.5, "shown head fell: {head}");
    let blend = app.world().get::<PhysicsPoseBlend>(rig).expect("blend");
    assert!(blend.max_weight() > 0.99);
}

#[test]
fn powered_ragdoll_holds_the_pose_and_a_hit_reacts_partially_then_recovers() {
    let (mut app, rig) = scene(RagdollMode::Powered);
    run(&mut app, 60);
    let b = bodies(&app, rig);
    let forearm = body_of(&b, "lowerarm_l");
    let rest_error = (body_pose(&app, forearm).0 - animated(&app, rig, "lowerarm_l")).length();
    assert!(
        rest_error < 0.05,
        "powered forearm holds the animation: {rest_error} m"
    );
    let hips = body_of(&b, "hips");
    assert!(
        (body_pose(&app, hips).0 - animated(&app, rig, "hips")).length() < 1e-3,
        "root kinematic"
    );
    // Nothing shows while blend is 0 and nothing was hit.
    assert!((shown(&app, rig, "hand_l") - animated(&app, rig, "hand_l")).length() < 1e-4);

    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "hand_l".into(),
        impulse: Vec3::new(0.0, 0.0, -8.0),
        reaction: 1.0,
        recover: 1.0,
    });
    let mut peak = 0.0f32;
    let mut shown_peak = 0.0f32;
    for _ in 0..15 {
        app.update();
        peak = peak.max((body_pose(&app, forearm).0 - animated(&app, rig, "lowerarm_l")).length());
        shown_peak = shown_peak.max((shown(&app, rig, "hand_l") - animated(&app, rig, "hand_l")).length());
    }
    assert!(peak > 0.05, "the hit knocks the forearm off the pose: {peak} m");
    assert!(shown_peak > 0.03, "and the mesh shows it: {shown_peak} m");
    let blend = app.world().get::<PhysicsPoseBlend>(rig).expect("blend");
    assert!(blend.weights[joint("lowerarm_l")] > 0.5, "hit bone reacts");
    assert_eq!(
        blend.weights[joint("thigh_l")],
        0.0,
        "legs stay animated (partial blend)"
    );
    assert_eq!(
        blend.weights[joint("upperarm_l")],
        0.0,
        "only the hit bone and below react"
    );

    run(&mut app, 120);
    let after = (body_pose(&app, forearm).0 - animated(&app, rig, "lowerarm_l")).length();
    assert!(after < 0.05, "recovered: {after} m");
    let blend = app.world().get::<PhysicsPoseBlend>(rig).expect("blend");
    assert_eq!(blend.max_weight(), 0.0, "reaction faded");
}

#[test]
fn kinematic_ragdoll_bodies_track_the_animation() {
    let (mut app, rig) = scene(RagdollMode::Kinematic);
    // Move the rig: the hit boxes follow.
    run(&mut app, 5);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(rig) {
        t.translation = Vec3::new(2.0, 0.0, 1.0);
    }
    run(&mut app, 5);
    let b = bodies(&app, rig);
    for body in &b.bodies {
        let want = animated(&app, rig, NAMES[body.joint as usize]) + Vec3::new(2.0, 0.0, 1.0);
        let got = body_pose(&app, body.entity).0;
        assert!(
            (got - want).length() < 1e-3,
            "{}: {got} vs {want}",
            NAMES[body.joint as usize]
        );
    }
    assert_eq!(b.weight, 0.0);

    // Switching a live ragdoll to limp (death) turns the hit boxes into a falling body.
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Limp;
    }
    run(&mut app, 150);
    let hips = body_pose(&app, body_of(&b, "hips")).0;
    assert!(hips.y < 0.45, "went limp and fell: {hips}");
    assert!(
        app.world().get::<PhysicsPose>(body_of(&b, "head")).is_some(),
        "bodies are dynamic now"
    );
}

/// World matrix of the rig (the ragdoll's object space).
fn rig_matrix_of(t: &Transform) -> Mat4 {
    Mat4::from_scale_rotation_translation(t.scale, t.rotation, t.translation)
}

/// Animated world position of a joint with the rig's current transform applied.
fn animated_at(app: &App, rig: Entity, name: &str) -> Vec3 {
    let rig_t = app.world().get::<Transform>(rig).copied().expect("rig");
    let animator = app.world().get::<Animator>(rig).expect("animator");
    humanoid().globals(animator.evaluated_pose(), Some(rig_matrix_of(&rig_t)))[joint(name)]
        .w_axis
        .truncate()
}

#[test]
fn going_limp_mid_stride_preserves_the_rig_momentum() {
    let (mut app, rig) = scene(RagdollMode::Kinematic);
    run(&mut app, 10);
    // Walk the rig at 1 m/s along +X; the hit boxes follow.
    for _ in 0..30 {
        app.update();
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(rig) {
            t.translation.x += DT as f32 * 1.0;
        }
    }
    let walked = app
        .world()
        .get::<Transform>(rig)
        .copied()
        .expect("rig")
        .translation;
    assert!(walked.x > 0.2, "the rig walked: {walked}");
    // Die mid-stride: the bodies keep the animated velocity they had, not zero.
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Limp;
    }
    app.update();
    let b = bodies(&app, rig);
    for name in ["hips", "chest", "upperarm_l", "thigh_l"] {
        let v = app
            .world()
            .get::<Velocity>(body_of(&b, name))
            .cloned()
            .unwrap_or_default();
        assert!(
            (v.linear.x - 1.0).abs() < 0.25,
            "{name} carries the stride into the fall: {v:?}"
        );
    }
    // A same-frame rig teleport is not motion: the seed is dropped instead of firing the
    // body away.
    let (mut app, rig) = scene(RagdollMode::Kinematic);
    run(&mut app, 30);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(rig) {
        t.translation.x += 100.0;
    }
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Limp;
    }
    app.update();
    let b = bodies(&app, rig);
    let v = app
        .world()
        .get::<Velocity>(body_of(&b, "hips"))
        .cloned()
        .unwrap_or_default();
    assert!(
        v.linear.length() < 10.0,
        "a teleport does not fire the ragdoll away: {v:?}"
    );
}

#[test]
fn recover_teleports_the_bodies_snaps_the_rig_and_blends_the_fall_out() {
    let (mut app, rig) = scene(RagdollMode::Limp);
    run(&mut app, 30);
    // Shove it sideways so the recover has somewhere to snap to.
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "hips".into(),
        impulse: Vec3::new(10.0, 0.0, 0.0),
        reaction: 0.0,
        recover: 0.1,
    });
    run(&mut app, 170); // 3.2 s: fallen, slid and at rest
    let b = bodies(&app, rig);
    let fallen = body_pose(&app, body_of(&b, "hips")).0;
    assert!(fallen.x > 0.15, "the shove moved the body: {fallen}");
    let head_fallen = shown(&app, rig, "head");
    assert!(head_fallen.y < 0.6, "fallen: {head_fallen}");

    // Get up where it fell.
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Kinematic;
    }
    app.update();

    // The rig snapped: the animated hips land on the fallen spot (x/z), height kept.
    let hips = animated_at(&app, rig, "hips");
    assert!(
        (hips.x - fallen.x).abs() < 0.05 && (hips.z - fallen.z).abs() < 0.05,
        "the animation continues where the body lies: {hips} vs {fallen}"
    );
    assert!(hips.y > 0.9, "the height stays the animation's: {hips}");
    // Every body is pinned to the animation with no velocity.
    let b = bodies(&app, rig);
    for body in &b.bodies {
        let want = animated_at(&app, rig, NAMES[body.joint as usize]);
        let got = body_pose(&app, body.entity).0;
        assert!(
            (got - want).length() < 1e-3,
            "{} pinned: {got} vs {want}",
            NAMES[body.joint as usize]
        );
        let v = app
            .world()
            .get::<Velocity>(body.entity)
            .cloned()
            .unwrap_or_default();
        assert!(v.linear.length() < 1e-3, "no stale velocity: {v:?}");
    }
    // The frozen blend holds the fallen pose over the standing animation...
    let head = shown(&app, rig, "head");
    assert!(
        head.y < head_fallen.y + 0.15,
        "the recover does not pop to standing: {head}"
    );
    assert!(
        app.world()
            .get::<RagdollBodies>(rig)
            .expect("ragdoll")
            .recovering()
    );
    // ...and eases out over blend_time (0.25 s).
    run(&mut app, 40); // 0.64 s
    let head = shown(&app, rig, "head");
    assert!(head.y > 1.3, "standing again: {head}");
    assert!(
        !app.world()
            .get::<RagdollBodies>(rig)
            .expect("ragdoll")
            .recovering()
    );
    // The shown pose is exactly the animation once the recover is done.
    // `JointGlobals` (what `shown` reads) are rig-local, so compare with the animation in rig
    // space: the rig itself moved in the snap.
    let animated_head = {
        let animator = app.world().get::<Animator>(rig).expect("animator");
        humanoid().globals(animator.evaluated_pose(), None)[joint("head")]
            .w_axis
            .truncate()
    };
    let shown_head = shown(&app, rig, "head");
    assert!(
        (shown_head - animated_head).length() < 1e-4,
        "no leftover blend: shown {shown_head} vs animated {animated_head}, weight {}",
        app.world().get::<RagdollBodies>(rig).map_or(-1.0, |b| b.weight)
    );
}

#[test]
fn recover_without_snap_keeps_the_rig_origin() {
    let (mut app, rig) = scene(RagdollMode::Limp);
    run(&mut app, 30);
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "hips".into(),
        impulse: Vec3::new(10.0, 0.0, 0.0),
        reaction: 0.0,
        recover: 0.1,
    });
    run(&mut app, 170);
    let fallen = body_pose(&app, body_of(&bodies(&app, rig), "hips")).0;
    assert!(fallen.x > 0.15);
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.mode = RagdollMode::Kinematic;
        ragdoll.recover_snap = false;
    }
    app.update();
    let rig_t = app.world().get::<Transform>(rig).copied().expect("rig");
    assert!(
        rig_t.translation.x.abs() < 1e-4,
        "the rig stays where it was: {}",
        rig_t.translation
    );
    // The bodies still pin to the animation (at the old origin).
    let hips = animated_at(&app, rig, "hips");
    let got = body_pose(&app, body_of(&bodies(&app, rig), "hips")).0;
    assert!((got - hips).length() < 1e-3);
}

/// Audit v033b (High): a kinematic (and powered) ragdoll whose rig is scaled to zero decomposed
/// NaN rotations out of the singular rig matrix and wrote them onto every body. The bodies keep
/// their last pose while the scale is zero and follow the animation again once it is back.
#[test]
fn a_rig_scaled_to_zero_never_writes_nan_onto_the_bodies() {
    for mode in [RagdollMode::Kinematic, RagdollMode::Powered] {
        let (mut app, rig) = scene(mode);
        run(&mut app, 10);
        let b = bodies(&app, rig);
        app.world_mut().get_mut::<Transform>(rig).expect("rig").scale = Vec3::ZERO;
        run(&mut app, 10);
        for body in &b.bodies {
            let t = *app.world().get::<Transform>(body.entity).expect("body");
            assert!(
                t.translation.is_finite() && t.rotation.is_finite(),
                "{mode:?} {}: {t:?}",
                NAMES[body.joint as usize]
            );
        }
        if let Some(blend) = app.world().get::<PhysicsPoseBlend>(rig) {
            for (position, rotation) in blend.joints.iter().flatten() {
                assert!(position.is_finite() && rotation.is_finite(), "{mode:?}");
            }
        }
        app.world_mut().get_mut::<Transform>(rig).expect("rig").scale = Vec3::ONE;
        run(&mut app, 10);
        for body in &b.bodies {
            let (position, rotation) = body_pose(&app, body.entity);
            assert!(
                position.is_finite() && rotation.is_finite(),
                "{mode:?} after the scale is back"
            );
        }
    }
}

/// Box and sphere bodies (ADR-2133 § 6): placed along the bone like capsules, a box's depth along the
/// character's forward, the density giving the bone's mass; a limp ragdoll of them falls and rests.
#[test]
fn box_and_sphere_bodies_sit_along_their_bones_and_fall_limp() {
    let tune = |bones: &mut [regia_framework::ragdoll::RagdollBone]| {
        for bone in bones.iter_mut() {
            match bone.joint.as_str() {
                "hips" => {
                    bone.shape = RagdollShape::Box {
                        width: 0.3,
                        depth: 0.2,
                    }
                }
                "head" => bone.shape = RagdollShape::Sphere,
                _ => {}
            }
        }
    };
    let (mut app, rig) = scene_with_profile(RagdollMode::Kinematic, tune);
    run(&mut app, 3);
    let b = bodies(&app, rig);
    let body = |name: &str| {
        b.bodies
            .iter()
            .find(|x| x.joint as usize == joint(name))
            .map(|x| x.entity)
            .expect("body")
    };
    let collider = |name: &str| {
        app.world()
            .get::<Collider>(body(name))
            .cloned()
            .expect("collider")
    };
    let mass = |name: &str| {
        app.world()
            .get::<Ragdoll>(rig)
            .and_then(|r| r.profile.bones.iter().find(|x| x.joint == name).map(|x| x.mass))
            .expect("bone")
    };

    // Hips: towards the chest body (0.4 m up), a 0.3 × 0.4 × 0.2 box; depth faces -Z (forward).
    let hips = collider("hips");
    let ColliderShape::Box { half_extents } = hips.shape else {
        panic!("hips is a box: {:?}", hips.shape);
    };
    assert!(
        (half_extents - Vec3::new(0.15, 0.2, 0.1)).length() < 1e-5,
        "{half_extents}"
    );
    assert!((hips.offset.translation - Vec3::new(0.0, 0.2, 0.0)).length() < 1e-5);
    assert!(
        (hips.offset.rotation * Vec3::Y - Vec3::Y).length() < 1e-5,
        "length along the bone"
    );
    assert!(
        (hips.offset.rotation * Vec3::Z - Vec3::NEG_Z).length() < 1e-5,
        "depth along forward"
    );
    let volume = 8.0 * half_extents.x * half_extents.y * half_extents.z;
    assert!((hips.density * volume - mass("hips")).abs() < 1e-3);

    // Head: a leaf, so one diameter long; the sphere sits on the joint.
    let head = collider("head");
    let ColliderShape::Sphere { radius } = head.shape else {
        panic!("head is a sphere: {:?}", head.shape);
    };
    assert!((head.offset.translation - Vec3::new(0.0, radius, 0.0)).length() < 1e-5);
    let volume = 4.0 / 3.0 * std::f32::consts::PI * radius.powi(3);
    assert!((head.density * volume - mass("head")).abs() < 1e-3);

    // The other bodies stay capsules.
    assert!(matches!(collider("chest").shape, ColliderShape::Capsule { .. }));

    // Limp, the mixed ragdoll comes to rest on the floor, finite and above it.
    let (mut app, rig) = scene_with_profile(RagdollMode::Limp, tune);
    run(&mut app, 250);
    let b = bodies(&app, rig);
    for body in &b.bodies {
        let (position, _) = body_pose(&app, body.entity);
        assert!(
            position.is_finite() && position.y > -0.05 && position.y < 0.5,
            "{} lies on the floor: {position}",
            NAMES[body.joint as usize]
        );
    }
}

/// audit-v035-numerics M4 (fix-audit-wave12): a NaN `blend` on a powered ragdoll latched a NaN
/// shown weight (`w + (target - w).clamp(..)` keeps NaN), so the physics blend stayed broken
/// after the blend was fixed. The weight restarts at the target and eases on.
#[test]
fn a_nan_powered_blend_does_not_latch_the_shown_weight() {
    let (mut app, rig) = scene(RagdollMode::Powered);
    run(&mut app, 5);
    app.world_mut().get_mut::<Ragdoll>(rig).expect("ragdoll").blend = f32::NAN;
    run(&mut app, 3);
    app.world_mut().get_mut::<Ragdoll>(rig).expect("ragdoll").blend = 0.5;
    run(&mut app, 120);
    let w = bodies(&app, rig).weight;
    assert!(w.is_finite(), "shown weight latched: {w}");
    assert!((w - 0.5).abs() < 1e-3, "eases to the blend: {w}");
}

/// audit-v035-numerics L2 (fix-audit-wave12): a capsule bone with a zero or NaN radius built a
/// capsule the physics seam rejects, so the bone spawned without a collider. The radius takes
/// the sphere's 5 mm floor and every body has a collider.
#[test]
fn a_capsule_bone_with_a_degenerate_radius_still_gets_a_collider() {
    for radius in [0.0, -0.1, f32::NAN] {
        let tune = move |bones: &mut [regia_framework::ragdoll::RagdollBone]| {
            for bone in bones.iter_mut() {
                if bone.joint == "head" {
                    bone.shape = RagdollShape::Capsule;
                    bone.radius = radius;
                }
                if bone.joint == "chest" {
                    bone.mass = f32::INFINITY;
                }
            }
        };
        let (mut app, rig) = scene_with_profile(RagdollMode::Limp, tune);
        run(&mut app, 3);
        let b = bodies(&app, rig);
        for body in &b.bodies {
            let collider = app.world().get::<Collider>(body.entity);
            let name = NAMES[body.joint as usize];
            let Some(collider) = collider else {
                panic!("radius {radius}: {name} has no collider");
            };
            if let ColliderShape::Capsule {
                radius: r,
                half_height,
                ..
            } = collider.shape
            {
                assert!(r.is_finite() && r > 0.0 && half_height.is_finite(), "{name}: {r}");
            }
            assert!(
                collider.density.is_finite() && collider.density > 0.0,
                "radius {radius}: {name} density {}",
                collider.density
            );
        }
    }
}
