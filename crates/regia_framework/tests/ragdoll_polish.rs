// SPDX-License-Identifier: MPL-2.0
//! Ragdoll polish (spec 93 § 2.3, ADR-1693): joint limits from the bind pose (a knee does not
//! bend forwards), the runtime limits switch, settle detection with face up / face down, and
//! the simulated-ragdoll budget (freezes the oldest settled ragdoll, never an unsettled one).

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::Messages;
use regia_ecs::prelude::Entity;
use regia_framework::ragdoll::{
    Ragdoll, RagdollBodies, RagdollHit, RagdollMode, RagdollPlugin, RagdollProfile, RagdollSettings,
    RagdollSettled,
};
use regia_physics::{
    BodyKind, Collider, ColliderShape, ExternalForce, Joint, JointKind, PhysicsPose, RigidBody,
};
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

/// The T-posed test humanoid of `tests/ragdoll.rs` (identity bind rotations, facing -Z).
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

fn app(settings: RagdollSettings) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins((AnimationPlugin, RagdollPlugin))
        .insert_resource(settings);
    // Floor: top face at y = 0.
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(40.0, 0.5, 40.0),
            },
            ..Collider::default()
        },
    ));
    app
}

fn spawn_rig(app: &mut App, at: Vec3, profile: RagdollProfile) -> Entity {
    let skeleton = Arc::new(humanoid());
    let mut ragdoll = Ragdoll::new(profile);
    ragdoll.mode = RagdollMode::Kinematic;
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            ragdoll,
        ))
        .id()
}

fn limited() -> RagdollProfile {
    RagdollProfile::humanoid_limited(&humanoid(), 75.0, Vec3::NEG_Z)
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

fn bodies(app: &App, rig: Entity) -> RagdollBodies {
    app.world()
        .get::<RagdollBodies>(rig)
        .cloned()
        .expect("ragdoll built")
}

fn body_of(b: &RagdollBodies, name: &str) -> Entity {
    let index = NAMES.iter().position(|n| *n == name).expect("joint") as u32;
    b.bodies
        .iter()
        .find(|x| x.joint == index)
        .map(|x| x.entity)
        .expect("bone")
}

fn rotation(app: &App, body: Entity) -> Quat {
    app.world().get::<PhysicsPose>(body).map_or_else(
        || app.world().get::<Transform>(body).expect("transform").rotation,
        |p| p.rotation,
    )
}

fn set_mode(app: &mut App, rig: Entity, mode: RagdollMode) {
    app.world_mut().get_mut::<Ragdoll>(rig).expect("ragdoll").mode = mode;
}

/// Signed angle of `q` about `axis` (twist part).
fn angle_about(q: Quat, axis: Vec3) -> f32 {
    let q = if q.w < 0.0 { -q } else { q };
    2.0 * Vec3::new(q.x, q.y, q.z).dot(axis).atan2(q.w)
}

#[test]
fn humanoid_limited_limits_every_bone_below_the_hips() {
    let profile = limited();
    assert_eq!(profile.bones.len(), 11);
    for bone in &profile.bones {
        assert_eq!(bone.limits.is_some(), bone.joint != "hips", "{}", bone.joint);
    }
    assert!(
        RagdollProfile::humanoid(&humanoid(), 75.0)
            .bones
            .iter()
            .all(|b| b.limits.is_none())
    );
}

#[test]
fn limits_build_swing_twist_joints_and_the_switch_builds_ball_joints() {
    for limits_on in [true, false] {
        let mut app = app(RagdollSettings {
            joint_limits: limits_on,
            ..RagdollSettings::default()
        });
        let rig = spawn_rig(&mut app, Vec3::ZERO, limited());
        run(&mut app, 3);
        let b = bodies(&app, rig);
        let mut joints = 0;
        for body in b.bodies.iter().filter(|b| b.parent.is_some()) {
            let kind = &app.world().get::<Joint>(body.entity).expect("joint").kind;
            joints += 1;
            if limits_on {
                assert!(
                    matches!(kind, JointKind::SwingTwist { rest: Some(_), .. }),
                    "{kind:?}"
                );
            } else {
                assert_eq!(*kind, JointKind::Spherical { cone_limit: None });
            }
        }
        assert_eq!(joints, 10);
    }
}

/// The knee's flexion angle (thigh → calf) about its bend axis: the calf bone points down
/// (-Y) and flexes backwards (+Z for a rig facing -Z), so the axis is (-Y) × (+Z) = -X.
fn knee_bend(app: &App, b: &RagdollBodies) -> f32 {
    let thigh = rotation(app, body_of(b, "thigh_l"));
    let calf = rotation(app, body_of(b, "calf_l"));
    angle_about(thigh.inverse() * calf, Vec3::NEG_X)
}

/// Twists the left calf forwards (a hyperextending torque about +X) in free fall and returns
/// the most negative knee flexion seen.
fn hyperextend(profile: RagdollProfile) -> f32 {
    let mut app = app(RagdollSettings::default());
    let rig = spawn_rig(&mut app, Vec3::new(0.0, 12.0, 0.0), profile);
    run(&mut app, 3);
    set_mode(&mut app, rig, RagdollMode::Limp);
    run(&mut app, 2);
    let b = bodies(&app, rig);
    app.world_mut()
        .entity_mut(body_of(&b, "calf_l"))
        .insert(ExternalForce {
            force: Vec3::ZERO,
            torque: Vec3::X * 40.0,
        });
    let mut worst = 0.0f32;
    for _ in 0..60 {
        app.update();
        worst = worst.min(knee_bend(&app, &b));
    }
    worst
}

#[test]
fn knee_limit_stops_a_forward_bend_that_an_unlimited_joint_allows() {
    let free = hyperextend(RagdollProfile::humanoid(&humanoid(), 75.0));
    assert!(free < -0.5, "the unlimited knee bends forwards: {free}");
    let held = hyperextend(limited());
    assert!(
        held > -0.15,
        "the limited knee stays at or behind straight: {held}"
    );
}

/// Limp with a shove on the chest; returns (face_up, settled message count, frames taken).
fn fall(push: Vec3) -> (bool, usize, usize) {
    let mut app = app(RagdollSettings::default());
    let rig = spawn_rig(&mut app, Vec3::ZERO, limited());
    run(&mut app, 3);
    set_mode(&mut app, rig, RagdollMode::Limp);
    run(&mut app, 2);
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "chest".into(),
        impulse: push,
        reaction: 0.0,
        recover: 0.1,
    });
    let mut messages = 0;
    let mut face_up = None;
    let mut frames = 0;
    for frame in 0..900 {
        app.update();
        for m in app.world_mut().resource_mut::<Messages<RagdollSettled>>().drain() {
            assert_eq!(m.rig, rig);
            messages += 1;
            face_up.get_or_insert(m.face_up);
            frames = frame;
        }
        if messages > 0 && frame > frames + 120 {
            break;
        }
    }
    let b = bodies(&app, rig);
    assert!(b.settled(), "the ragdoll came to rest (messages {messages})");
    let face_up = face_up.expect("a RagdollSettled message");
    assert_eq!(b.face_up(), face_up);
    (face_up, messages, frames)
}

#[test]
fn pushed_backwards_it_settles_face_up_and_forwards_face_down() {
    // The rig faces -Z, so +Z pushes the chest backwards and it falls on its back.
    let (up, messages, _) = fall(Vec3::new(0.0, 0.0, 30.0));
    assert!(up, "fell backwards, lies face up");
    assert_eq!(messages, 1, "one RagdollSettled per fall");
    let (up, messages, _) = fall(Vec3::new(0.0, 0.0, -30.0));
    assert!(!up, "fell forwards, lies face down");
    assert_eq!(messages, 1);
}

#[test]
fn budget_freezes_the_oldest_settled_ragdoll_and_never_an_unsettled_one() {
    let mut app = app(RagdollSettings {
        max_simulated: 1,
        ..RagdollSettings::default()
    });
    let first = spawn_rig(&mut app, Vec3::new(-3.0, 0.0, 0.0), limited());
    let second = spawn_rig(&mut app, Vec3::new(3.0, 0.0, 0.0), limited());
    run(&mut app, 3);
    set_mode(&mut app, first, RagdollMode::Limp);
    run(&mut app, 120);
    set_mode(&mut app, second, RagdollMode::Limp);
    let mut first_frozen_at = None;
    for frame in 0..900 {
        app.update();
        for rig in [first, second] {
            let b = bodies(&app, rig);
            assert!(
                !b.frozen() || b.settled(),
                "frame {frame}: a frozen ragdoll has settled"
            );
        }
        let a = bodies(&app, first);
        if a.frozen() && first_frozen_at.is_none() {
            first_frozen_at = Some(frame);
        }
        if a.frozen() && bodies(&app, second).settled() {
            break;
        }
    }
    let (a, b) = (bodies(&app, first), bodies(&app, second));
    assert!(a.frozen(), "the oldest settled ragdoll froze");
    assert!(!b.frozen(), "within budget again: the newer one keeps simulating");
    assert!(b.simulating() && !a.simulating());
    for body in &a.bodies {
        assert_eq!(
            app.world().get::<RigidBody>(body.entity).expect("body").kind,
            BodyKind::Kinematic
        );
    }
    for body in &b.bodies {
        assert_eq!(
            app.world().get::<RigidBody>(body.entity).expect("body").kind,
            BodyKind::Dynamic
        );
    }
    // A frozen ragdoll that changes mode leaves the freeze.
    set_mode(&mut app, first, RagdollMode::Kinematic);
    run(&mut app, 2);
    assert!(!bodies(&app, first).frozen());
}

#[test]
fn tier_budgets_grow_with_the_tier() {
    use regia_render::QualityTier;
    let budgets: Vec<usize> = QualityTier::ALL
        .iter()
        .map(|&t| RagdollSettings::for_tier(t).max_simulated)
        .collect();
    assert_eq!(budgets, vec![2, 4, 6, 8, 12, 16, 24]);
    assert!(
        QualityTier::ALL
            .iter()
            .all(|&t| RagdollSettings::for_tier(t).joint_limits)
    );
}

/// Hand-run cost probe (spec 93 § 8): the physics step time of 16 limp humanoids falling and
/// settling, with and without joint limits, from `PhysicsWorld::stats().last_step_ms`. Prints
/// the numbers; asserts only that every ragdoll was built. Run with
/// `cargo test -p regia_framework --release --test ragdoll_polish perf_ragdolls_step -- --ignored --nocapture`.
#[test]
#[ignore = "hand-run perf probe; prints step times"]
fn perf_ragdolls_step() {
    for limits_on in [false, true] {
        let mut app = app(RagdollSettings {
            joint_limits: limits_on,
            max_simulated: 64,
        });
        let rigs: Vec<Entity> = (0..16)
            .map(|i| {
                let at = Vec3::new((i % 4) as f32 * 3.0, 0.0, (i / 4) as f32 * 3.0);
                spawn_rig(&mut app, at, limited())
            })
            .collect();
        run(&mut app, 3);
        for &rig in &rigs {
            set_mode(&mut app, rig, RagdollMode::Limp);
        }
        let mut falling = Vec::new();
        let mut resting = Vec::new();
        for frame in 0..400 {
            app.update();
            let ms = app
                .world()
                .resource::<regia_physics::PhysicsWorld>()
                .stats()
                .last_step_ms;
            if frame < 60 {
                falling.push(ms as f32);
            } else if frame >= 300 {
                resting.push(ms as f32);
            }
        }
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
        let peak = |v: &[f32]| v.iter().copied().fold(0.0f32, f32::max);
        let settled = rigs.iter().filter(|&&r| bodies(&app, r).settled()).count();
        assert!(rigs.iter().all(|&r| bodies(&app, r).bodies.len() == 11));
        println!(
            "perf_ragdolls_step limits={limits_on}: 16 ragdolls x 11 bodies; falling mean {:.3} ms peak {:.3} ms; \
             resting mean {:.3} ms; settled {settled}/16",
            mean(&falling),
            peak(&falling),
            mean(&resting)
        );
    }
}

/// Six shoves, forwards and backwards at three strengths: every fall settles within 6 s and
/// stays settled (rest damping stops limbs rocking), lying face up after a backward shove and
/// face down after a forward one.
#[test]
fn every_shove_settles_on_the_side_it_was_pushed_to() {
    for push in [-45.0f32, -30.0, -20.0, 20.0, 30.0, 45.0] {
        let mut app = app(RagdollSettings::default());
        let rig = spawn_rig(&mut app, Vec3::ZERO, limited());
        run(&mut app, 3);
        set_mode(&mut app, rig, RagdollMode::Limp);
        run(&mut app, 2);
        app.world_mut().write_message(RagdollHit {
            rig,
            joint: "chest".into(),
            impulse: Vec3::new(0.0, 0.0, push),
            reaction: 0.0,
            recover: 0.1,
        });
        let mut settled_at = None;
        for frame in 0..375 {
            app.update();
            let settled = bodies(&app, rig).settled();
            if settled_at.is_none() && settled {
                settled_at = Some(frame);
            }
            assert!(
                settled_at.is_none() || settled,
                "push {push}: unsettled again at frame {frame}"
            );
        }
        assert!(settled_at.is_some(), "push {push}: settled within 6 s");
        assert_eq!(
            bodies(&app, rig).face_up(),
            push > 0.0,
            "push {push}: lies on the side it was pushed to"
        );
    }
}

fn damping(app: &App, b: &RagdollBodies) -> Vec<(f32, f32)> {
    b.bodies
        .iter()
        .map(|x| {
            let r = app.world().get::<RigidBody>(x.entity).expect("body");
            (r.linear_damping, r.angular_damping)
        })
        .collect()
}

/// Spec 93 § 2.3: a settled ragdoll's bodies get the rest damping (linear 2, angular 8), its
/// `RagdollSettled` carries the root body's position, and a hit that moves it again clears the
/// settle and restores the moving damping (0.1, 0.5). The profile faces `-Z` by default.
#[test]
fn settling_sets_rest_damping_and_a_hit_restores_the_moving_damping() {
    assert_eq!(RagdollProfile::default().forward, Vec3::NEG_Z);
    let mut app = app(RagdollSettings::default());
    let rig = spawn_rig(&mut app, Vec3::ZERO, limited());
    run(&mut app, 3);
    let moving = vec![(0.1, 0.5); bodies(&app, rig).bodies.len()];
    assert_eq!(
        damping(&app, &bodies(&app, rig)),
        moving,
        "bodies start with the moving damping"
    );
    set_mode(&mut app, rig, RagdollMode::Limp);
    run(&mut app, 2);
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "chest".into(),
        impulse: Vec3::new(0.0, 0.0, 30.0),
        reaction: 0.0,
        recover: 0.1,
    });
    let mut settled = None;
    for _ in 0..900 {
        app.update();
        if let Some(m) = app
            .world_mut()
            .resource_mut::<Messages<RagdollSettled>>()
            .drain()
            .last()
        {
            settled = Some(m);
            break;
        }
    }
    let m = settled.expect("a RagdollSettled message");
    let b = bodies(&app, rig);
    assert!(b.settled());
    let root = b.bodies.iter().find(|x| x.parent.is_none()).expect("a root body");
    let at = app.world().get::<PhysicsPose>(root.entity).map_or_else(
        || {
            app.world()
                .get::<Transform>(root.entity)
                .expect("transform")
                .translation
        },
        |p| p.translation,
    );
    assert!(
        (m.root_position - at).length() < 0.05,
        "root_position {:?} is the root body ({at:?})",
        m.root_position
    );
    assert!(
        m.root_position.y < 0.5,
        "it lies on the floor: {:?}",
        m.root_position
    );
    assert_eq!(
        damping(&app, &b),
        vec![(2.0, 8.0); b.bodies.len()],
        "rest damping"
    );
    // A hard shove moves it again: no longer settled, back to the moving damping.
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "chest".into(),
        impulse: Vec3::new(60.0, 0.0, 0.0),
        reaction: 0.0,
        recover: 0.1,
    });
    run(&mut app, 3);
    let b = bodies(&app, rig);
    assert!(!b.settled(), "the hit cleared the settle");
    assert_eq!(damping(&app, &b), moving, "moving damping again");
}
