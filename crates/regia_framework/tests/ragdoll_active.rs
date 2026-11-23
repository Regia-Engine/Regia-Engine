// SPDX-License-Identifier: MPL-2.0
//! Active ragdolls (spec 93 § 10, ADR-2498): PD motors track the animated pose, zero gains are exactly limp, a
//! biped on box feet balances under a small push and falls under a large one, hit weakening recovers on time, a
//! prone ragdoll picks the face-down get-up clip, and two runs are bit-identical. Fixed steps only (one 62.5 Hz
//! fixed step per update), no wall clock.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::ragdoll::{
    Ragdoll, RagdollBalanceRole, RagdollBodies, RagdollBone, RagdollFacing, RagdollGetUp, RagdollGroup,
    RagdollHit, RagdollMode, RagdollMotor, RagdollPhase, RagdollPlugin, RagdollProfile, RagdollShape,
};
use regia_physics::{
    Collider, ColliderShape, ExternalImpulse, PhysicsPose, PhysicsWorld, RigidBody, Velocity,
};
use regia_scene::Transform;

const HZ: f64 = 62.5;
const DT: f32 = 1.0 / 62.5;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = HZ;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins((AnimationPlugin, RagdollPlugin));
    app
}

fn floor(app: &mut App) {
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
}

fn bone(joint: &str, radius: f32, mass: f32, shape: RagdollShape) -> RagdollBone {
    RagdollBone {
        joint: joint.into(),
        radius,
        mass,
        length: None,
        limits: None,
        shape,
    }
}

fn motor(joint: &str, group: RagdollGroup, kp: f32, kd: f32, balance: RagdollBalanceRole) -> RagdollMotor {
    RagdollMotor {
        joint: joint.into(),
        group,
        kp,
        kd,
        max_torque: 600.0,
        balance,
    }
}

fn spawn_rig(app: &mut App, skeleton: Skeleton, ragdoll: Ragdoll, transform: Transform) -> Entity {
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    app.world_mut()
        .spawn((
            transform,
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            ragdoll,
        ))
        .id()
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

fn rotation(app: &App, body: Entity) -> Quat {
    match app.world().get::<PhysicsPose>(body) {
        Some(p) => p.rotation,
        None => app.world().get::<Transform>(body).expect("body").rotation,
    }
}

fn position(app: &App, body: Entity) -> Vec3 {
    match app.world().get::<PhysicsPose>(body) {
        Some(p) => p.translation,
        None => app.world().get::<Transform>(body).expect("body").translation,
    }
}

fn set_mode(app: &mut App, rig: Entity, mode: RagdollMode) {
    app.world_mut().get_mut::<Ragdoll>(rig).expect("ragdoll").mode = mode;
}

// ---------------------------------------------------------------------------------------------------------------
// The 3-body chain
// ---------------------------------------------------------------------------------------------------------------

fn chain_skeleton() -> Skeleton {
    let names = ["a", "b", "c", "tip"].iter().map(|n| n.to_string()).collect();
    let parents = vec![None, Some(0), Some(1), Some(2)];
    let binds = vec![
        translated(Vec3::new(0.0, 10.0, 0.0)),
        translated(Vec3::new(0.0, -0.4, 0.0)),
        translated(Vec3::new(0.0, -0.4, 0.0)),
        translated(Vec3::new(0.0, -0.4, 0.0)),
    ];
    Skeleton::from_local_bind(names, parents, binds).expect("chain skeleton")
}

const CHAIN_KP: f32 = 200.0;
const CHAIN_KD: f32 = 20.0;

fn chain(kp: f32, kd: f32) -> (App, Entity) {
    let mut app = app();
    let mut profile = RagdollProfile {
        bones: vec![
            bone("a", 0.06, 4.0, RagdollShape::Capsule),
            bone("b", 0.05, 3.0, RagdollShape::Capsule),
            bone("c", 0.05, 2.0, RagdollShape::Capsule),
        ],
        ..RagdollProfile::default()
    };
    profile.active.motors = vec![
        motor("b", RagdollGroup::Arms, kp, kd, RagdollBalanceRole::None),
        motor("c", RagdollGroup::Arms, kp, kd, RagdollBalanceRole::None),
    ];
    profile.active.hit.full_impulse = 20.0;
    profile.active.hit.recover_time = 0.5;
    let mut ragdoll = Ragdoll::new(profile);
    ragdoll.mode = RagdollMode::Active;
    let rig = spawn_rig(&mut app, chain_skeleton(), ragdoll, Transform::default());
    (app, rig)
}

/// Largest angle (rad) between a chain body's rotation relative to its parent and the bind-pose relative rotation
/// (identity for this skeleton: every joint frame is unrotated).
fn chain_error(app: &App, rig: Entity) -> f32 {
    let b = bodies(app, rig);
    b.bodies
        .iter()
        .filter_map(|body| {
            let parent = b.bodies[body.parent?].entity;
            let relative = rotation(app, parent).inverse() * rotation(app, body.entity);
            Some(relative.angle_between(Quat::IDENTITY))
        })
        .fold(0.0, f32::max)
}

/// Kicks the chain's child bodies off the target pose with an angular velocity, once.
fn kick(app: &mut App, rig: Entity) {
    let b = bodies(app, rig);
    for (i, body) in b.bodies.iter().enumerate().skip(1) {
        let spin = if i == 1 {
            Vec3::new(6.0, 0.0, 2.0)
        } else {
            Vec3::new(-4.0, 3.0, 0.0)
        };
        if let Some(mut v) = app.world_mut().get_mut::<Velocity>(body.entity) {
            v.angular = spin;
        }
    }
}

/// Rotates the chain's middle body by 0.4 rad about X and the end body by a further 0.4 rad about Z (teleports,
/// joints kept together: each body turns about its own joint), so both joints start 0.4 rad off the target.
fn bend(app: &mut App, rig: Entity) {
    use regia_physics::Teleport;
    let b = bodies(app, rig);
    let (mid, end) = (b.bodies[1].entity, b.bodies[2].entity);
    let q1 = Quat::from_rotation_x(0.4);
    let q2 = Quat::from_rotation_z(0.4);
    let mid_at = position(app, mid);
    let mid_rot = rotation(app, mid) * q1;
    let end_at = mid_at + mid_rot * Vec3::new(0.0, -0.4, 0.0);
    let end_rot = mid_rot * q2;
    for (body, at, rot) in [(mid, mid_at, mid_rot), (end, end_at, end_rot)] {
        let world = app.world_mut();
        if let Some(mut t) = world.get_mut::<Transform>(body) {
            t.translation = at;
            t.rotation = rot;
        }
        world.entity_mut(body).insert(Teleport);
    }
}

/// ADR-2498 § bound: the stable PD's slow pole is `kp / (kd + kp dt)` = 8.6 /s for kp 200, kd 20 at 62.5 Hz, so an
/// error of 0.5 rad (here 0.4) falls under 0.01 rad in ln(50) / 8.6 = 0.46 s (29 steps). The test allows 120 steps (4x margin
/// for the coupling of the two joints and rapier's joint softness) and asserts the bound 0.01 rad.
#[test]
fn a_three_body_chain_tracks_a_static_target_pose() {
    const N: usize = 120;
    const BOUND: f32 = 0.01;
    let (mut app, rig) = chain(CHAIN_KP, CHAIN_KD);
    run(&mut app, 3);
    bend(&mut app, rig);
    run(&mut app, 1);
    let start = chain_error(&app, rig);
    assert!(start > 0.3, "the bend moved the chain off the pose ({start} rad)");
    run(&mut app, N);
    let tracked = chain_error(&app, rig);
    println!("chain error: {start:.4} rad after the bend, {tracked:.5} rad after {N} more steps");
    assert!(tracked < BOUND, "tracked error {tracked} rad >= bound {BOUND}");
    let state = bodies(&app, rig).active().clone();
    assert_eq!(
        state.phase(),
        RagdollPhase::Active,
        "no ankle motor: no balance, no fall"
    );
    // The same bend with zero gains stays off the pose: the motors did the work.
    let (mut limp, rig) = chain(0.0, 0.0);
    run(&mut limp, 3);
    bend(&mut limp, rig);
    run(&mut limp, 1 + N);
    let passive = chain_error(&limp, rig);
    assert!(
        passive > 10.0 * BOUND,
        "a passive chain does not return to the pose ({passive} rad)"
    );
}

/// The motors only exchange torque between connected bodies: with no gravity effect on relative motion (free fall)
/// and no contacts, the chain's total angular momentum about its center of mass is what the kick gave it.
#[test]
fn motor_torques_come_in_equal_and_opposite_pairs() {
    let (mut app, rig) = chain(CHAIN_KP, CHAIN_KD);
    run(&mut app, 3);
    kick(&mut app, rig);
    run(&mut app, 2);
    let momentum = |app: &App| -> Vec3 {
        let physics = app.world().resource::<PhysicsWorld>();
        let b = bodies(app, rig);
        let dynamics: Vec<_> = b
            .bodies
            .iter()
            .filter_map(|body| regia_physics::pd::body_dynamics(physics, body.entity))
            .collect();
        let mass: f32 = dynamics.iter().map(|d| d.mass).sum();
        let com = dynamics.iter().map(|d| d.center_of_mass * d.mass).sum::<Vec3>() / mass;
        let vel = dynamics.iter().map(|d| d.linear * d.mass).sum::<Vec3>() / mass;
        dynamics
            .iter()
            .map(|d| {
                let spin = d.inverse_inertia.inverse() * d.angular;
                spin + (d.center_of_mass - com).cross((d.linear - vel) * d.mass)
            })
            .sum()
    };
    let before = momentum(&app);
    run(&mut app, 15);
    let after = momentum(&app);
    // Rapier's damping (angular 0.5 /s, linear 0.1 /s) bleeds about 12 % in 15 steps; the motors themselves add
    // none. A one-sided torque of the size the motors apply would change it many times over.
    println!("angular momentum {before:?} -> {after:?}");
    assert!(
        (after - before).length() <= 0.25 * before.length() + 1e-3,
        "momentum moved from {before:?} to {after:?}"
    );
    assert!(bodies(&app, rig).active().driven_motors() > 0 || chain_error(&app, rig) < 1e-3);
}

/// A hit of half the full impulse halves the hit group's strength; it recovers linearly to exactly 1 after the
/// authored 0.5 s (32 fixed steps at 62.5 Hz), and the other groups are untouched.
#[test]
fn hit_strength_recovers_after_the_authored_time() {
    let (mut app, rig) = chain(CHAIN_KP, CHAIN_KD);
    run(&mut app, 3);
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "c".into(),
        impulse: Vec3::new(10.0, 0.0, 0.0),
        reaction: 0.5,
        recover: 0.5,
    });
    run(&mut app, 1);
    let s = |app: &App| bodies(app, rig).active().hit_strength(RagdollGroup::Arms, DT);
    let first = s(&app);
    assert!(
        (0.5..0.6).contains(&first),
        "half the full impulse halves the strength ({first})"
    );
    assert_eq!(
        bodies(&app, rig).active().hit_strength(RagdollGroup::Legs, DT),
        1.0
    );
    let mut last = first;
    let recover_steps = (0.5_f32 / DT).ceil() as usize;
    for step in 1..recover_steps + 2 {
        run(&mut app, 1);
        let now = s(&app);
        assert!(now >= last, "recovery is monotone (step {step}: {now} < {last})");
        last = now;
        if step + 1 < recover_steps {
            assert!(now < 1.0, "still recovering at step {step} ({now})");
        }
    }
    assert_eq!(last, 1.0, "back to full strength after the authored time");
}

/// A hit that recovers within one fixed step still weakens the motors for that step (audit framework5 #15): the
/// strength the hit sets is the one the next motor step uses, before the recovery clock moves.
#[test]
fn a_hit_shorter_than_one_fixed_step_weakens_the_next_motor_step() {
    let (mut app, rig) = chain(CHAIN_KP, CHAIN_KD);
    if let Some(mut ragdoll) = app.world_mut().get_mut::<Ragdoll>(rig) {
        ragdoll.profile.active.hit.recover_time = 0.01;
    }
    run(&mut app, 3);
    let driven = |app: &App| bodies(app, rig).active().driven_motors();
    assert_eq!(driven(&app), 2, "both arm motors drive before the hit");
    // The full impulse (20) takes the arms' strength to 0; the hit lands in `Update`, after this frame's step.
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "c".into(),
        impulse: Vec3::new(20.0, 0.0, 0.0),
        reaction: 0.0,
        recover: 0.01,
    });
    run(&mut app, 1);
    run(&mut app, 1);
    assert_eq!(
        driven(&app),
        0,
        "the step after the hit runs the arms at strength 0"
    );
    run(&mut app, 1);
    assert_eq!(
        driven(&app),
        2,
        "recovered after one step (0.01 s < one 16 ms step)"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Zero gains == limp
// ---------------------------------------------------------------------------------------------------------------

fn humanoid_drop(mode: RagdollMode) -> (App, Entity) {
    let mut app = app();
    floor(&mut app);
    let skeleton = biped_skeleton();
    let mut ragdoll = Ragdoll::new(biped_profile(0.0));
    ragdoll.mode = mode;
    let rig = spawn_rig(
        &mut app,
        skeleton,
        ragdoll,
        Transform {
            translation: Vec3::new(0.0, 0.3, 0.0),
            rotation: Quat::from_rotation_z(0.3),
            scale: Vec3::ONE,
        },
    );
    (app, rig)
}

fn body_bits(app: &App, rig: Entity) -> Vec<u32> {
    let b = bodies(app, rig);
    let mut bits = Vec::new();
    for body in &b.bodies {
        let p = position(app, body.entity);
        let r = rotation(app, body.entity);
        let v = app
            .world()
            .get::<Velocity>(body.entity)
            .cloned()
            .unwrap_or_default();
        for x in [
            p.x,
            p.y,
            p.z,
            r.x,
            r.y,
            r.z,
            r.w,
            v.linear.x,
            v.linear.y,
            v.linear.z,
            v.angular.x,
            v.angular.y,
            v.angular.z,
        ] {
            bits.push(x.to_bits());
        }
    }
    bits.push(u32::from(b.settled()));
    bits
}

/// An `Active` ragdoll whose profile has no driving motor (every v2 file) is bit-identical to a `Limp` one: same
/// poses, velocities and settle, and the same rapier state digest, step for step.
#[test]
fn zero_gains_are_bit_identical_to_limp() {
    let (mut limp, limp_rig) = humanoid_drop(RagdollMode::Limp);
    let (mut active, active_rig) = humanoid_drop(RagdollMode::Active);
    for step in 0..400 {
        limp.update();
        active.update();
        if limp.world().get::<RagdollBodies>(limp_rig).is_none() {
            continue;
        }
        assert_eq!(
            body_bits(&limp, limp_rig),
            body_bits(&active, active_rig),
            "step {step}"
        );
        assert_eq!(
            limp.world().resource::<PhysicsWorld>().state_digest(),
            active.world().resource::<PhysicsWorld>().state_digest(),
            "step {step}"
        );
    }
    let state = bodies(&active, active_rig).active().clone();
    assert_eq!(state.driven_motors(), 0);
    assert!(matches!(
        state.phase(),
        RagdollPhase::Falling | RagdollPhase::Passive
    ));
    assert!(
        bodies(&limp, limp_rig).settled(),
        "the drop settles within 400 steps"
    );
    assert_eq!(
        state.phase(),
        RagdollPhase::Passive,
        "settled active-without-motors is passive"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// The standing biped
// ---------------------------------------------------------------------------------------------------------------

const BIPED: [&str; 12] = [
    "pelvis", "spine", "head", "thigh_l", "shin_l", "foot_l", "toe_l", "thigh_r", "shin_r", "foot_r",
    "toe_r", "neck",
];

/// A biped with box feet: pelvis at 0.95 m, torso to the head joint, legs straight down to the heels (the foot
/// joint), feet forward to the toes. Faces -Z.
fn biped_skeleton() -> Skeleton {
    let names = BIPED.iter().map(|n| n.to_string()).collect();
    let parents = vec![
        None,    // pelvis
        Some(0), // spine
        Some(1), // head
        Some(0), // thigh_l
        Some(3), // shin_l
        Some(4), // foot_l (heel)
        Some(5), // toe_l
        Some(0), // thigh_r
        Some(7), // shin_r
        Some(8), // foot_r
        Some(9), // toe_r
        Some(2), // neck (leaf under the head)
    ];
    let t = |x: f32, y: f32, z: f32| translated(Vec3::new(x, y, z));
    let binds = vec![
        t(0.0, 0.95, 0.0),
        t(0.0, 0.1, 0.0),
        t(0.0, 0.5, -0.02),
        t(0.1, -0.05, 0.0),
        t(0.0, -0.42, 0.0),
        t(0.0, -0.428, 0.08),
        t(0.0, 0.0, -0.24),
        t(-0.1, -0.05, 0.0),
        t(0.0, -0.42, 0.0),
        t(0.0, -0.428, 0.08),
        t(0.0, 0.0, -0.24),
        t(0.0, 0.1, 0.0),
    ];
    Skeleton::from_local_bind(names, parents, binds).expect("biped skeleton")
}

fn biped_profile(gain: f32) -> RagdollProfile {
    let feet = RagdollShape::Box {
        width: 0.1,
        depth: 0.1,
    };
    let mut profile = RagdollProfile {
        bones: vec![
            bone(
                "pelvis",
                0.12,
                10.0,
                RagdollShape::Box {
                    width: 0.3,
                    depth: 0.2,
                },
            ),
            bone(
                "spine",
                0.14,
                30.0,
                RagdollShape::Box {
                    width: 0.34,
                    depth: 0.2,
                },
            ),
            bone("thigh_l", 0.07, 8.0, RagdollShape::Capsule),
            bone("shin_l", 0.05, 4.0, RagdollShape::Capsule),
            bone("foot_l", 0.05, 1.5, feet),
            bone("thigh_r", 0.07, 8.0, RagdollShape::Capsule),
            bone("shin_r", 0.05, 4.0, RagdollShape::Capsule),
            bone("foot_r", 0.05, 1.5, feet),
        ],
        ..RagdollProfile::default()
    };
    if gain > 0.0 {
        let k = gain;
        profile.active.motors = vec![
            motor(
                "spine",
                RagdollGroup::Spine,
                2700.0 * k,
                9.0 * k,
                RagdollBalanceRole::None,
            ),
            motor(
                "thigh_l",
                RagdollGroup::Legs,
                3600.0 * k,
                12.0 * k,
                RagdollBalanceRole::Hip,
            ),
            motor(
                "shin_l",
                RagdollGroup::Legs,
                3600.0 * k,
                12.0 * k,
                RagdollBalanceRole::None,
            ),
            motor(
                "foot_l",
                RagdollGroup::Legs,
                2700.0 * k,
                9.0 * k,
                RagdollBalanceRole::Ankle,
            ),
            motor(
                "thigh_r",
                RagdollGroup::Legs,
                3600.0 * k,
                12.0 * k,
                RagdollBalanceRole::Hip,
            ),
            motor(
                "shin_r",
                RagdollGroup::Legs,
                3600.0 * k,
                12.0 * k,
                RagdollBalanceRole::None,
            ),
            motor(
                "foot_r",
                RagdollGroup::Legs,
                2700.0 * k,
                9.0 * k,
                RagdollBalanceRole::Ankle,
            ),
        ];
    }
    let clip = |name: &str, facing| RagdollGetUp {
        clip: name.into(),
        facing,
        pose: Vec::new(),
    };
    profile.active.get_up = vec![
        clip("getup_back", RagdollFacing::FaceUp),
        clip("getup_front", RagdollFacing::FaceDown),
        clip("getup_left", RagdollFacing::SideLeft),
        clip("getup_right", RagdollFacing::SideRight),
    ];
    profile
}

fn standing() -> (App, Entity) {
    let mut app = app();
    floor(&mut app);
    let mut ragdoll = Ragdoll::new(biped_profile(1.0));
    ragdoll.mode = RagdollMode::Active;
    let rig = spawn_rig(
        &mut app,
        biped_skeleton(),
        ragdoll,
        Transform::from_translation(Vec3::new(0.0, 0.001, 0.0)),
    );
    run(&mut app, 30);
    (app, rig)
}

fn torso(app: &App, rig: Entity) -> Entity {
    let b = bodies(app, rig);
    b.bodies[1].entity
}

fn push(app: &mut App, rig: Entity, impulse: Vec3) {
    let body = torso(app, rig);
    app.world_mut().entity_mut(body).insert(ExternalImpulse {
        impulse,
        torque_impulse: Vec3::ZERO,
    });
}

fn pelvis_height(app: &App, rig: Entity) -> f32 {
    let b = bodies(app, rig);
    position(app, b.bodies[0].entity).y
}

/// The acceptance test of the balance controller: 600 steps through a small shove on the torso (12 kg·m/s, about
/// 0.18 m/s for the whole body), and a large one (160 kg·m/s) that knocks it down and leaves it passive on the floor.
#[test]
fn a_standing_ragdoll_holds_under_a_small_push_and_falls_under_a_large_one() {
    let (mut app, rig) = standing();
    let state = bodies(&app, rig).active().clone();
    assert_eq!(
        state.phase(),
        RagdollPhase::Active,
        "it stands up straight away: {state:?}"
    );
    assert!(
        state.support_points() >= 2,
        "both feet touch the floor: {state:?}"
    );
    push(&mut app, rig, Vec3::new(0.0, 0.0, -12.0));
    let mut lowest = f32::MAX;
    let mut worst = f32::MIN;
    for step in 0..600 {
        app.update();
        let s = bodies(&app, rig).active().clone();
        assert_eq!(s.phase(), RagdollPhase::Active, "step {step}: {s:?}");
        lowest = lowest.min(pelvis_height(&app, rig));
        worst = worst.max(s.support_distance().unwrap_or(f32::MAX));
    }
    println!("small push: lowest pelvis {lowest:.3} m, worst support distance {worst:.3} m");
    assert!(lowest > 0.8, "it stayed standing (pelvis {lowest} m)");

    let (mut app, rig) = standing();
    push(&mut app, rig, Vec3::new(0.0, 0.0, -160.0));
    let mut fell_at = None;
    for step in 0..240 {
        app.update();
        if bodies(&app, rig).active().phase() != RagdollPhase::Active {
            fell_at = Some(step);
            break;
        }
    }
    println!("large push: fell at step {fell_at:?}");
    let fell = fell_at.expect("a large push knocks it over");
    assert!(
        fell > 0,
        "the fall needs the center of mass outside the feet first"
    );
    run(&mut app, 400);
    let s = bodies(&app, rig).active().clone();
    assert!(
        matches!(s.phase(), RagdollPhase::Falling | RagdollPhase::Passive),
        "{s:?}"
    );
    assert_eq!(s.driven_motors(), 0, "fallen = passive (fall_strength 0)");
    assert!(pelvis_height(&app, rig) < 0.5, "it is on the floor");
}

/// World positions of the two feet.
fn feet(app: &App, rig: Entity) -> [Vec3; 2] {
    let b = bodies(app, rig);
    let foot = |name: &str| {
        let joint = biped_skeleton().joint_index(name).expect("foot joint");
        let body = b.bodies.iter().find(|x| x.joint == joint).expect("foot body");
        position(app, body.entity)
    };
    [foot("foot_l"), foot("foot_r")]
}

/// Largest angle (rad) of any body's joint from the animated (bind) relative rotation, identity for this skeleton.
fn pose_error(app: &App, rig: Entity) -> f32 {
    chain_error(app, rig)
}

/// With no push the biped stands for 3000 steps (48 s) at the world's default 4 solver iterations: the feet do not
/// move at all and the pose stays on target. The free-foot version of this creeps across the floor and falls.
#[test]
fn it_stands_for_a_long_time_at_the_default_solver_iterations() {
    let (mut app, rig) = standing();
    let start = feet(&app, rig);
    for step in 0..3000 {
        app.update();
        let s = bodies(&app, rig).active().clone();
        assert_eq!(s.phase(), RagdollPhase::Active, "step {step}: {s:?}");
        assert_eq!(s.planted_feet(), 2, "step {step}");
    }
    assert_eq!(feet(&app, rig), start, "planted feet do not creep");
    let error = pose_error(&app, rig);
    println!(
        "after 3000 steps: joint error {error:.4} rad, pelvis {:.3} m",
        pelvis_height(&app, rig)
    );
    assert!(error < 0.05, "the pose holds ({error} rad)");
    assert!(pelvis_height(&app, rig) > 0.9);
}

/// A small shove in any of the four directions is ridden out and the biped comes back to the pose.
#[test]
fn small_pushes_hold_in_every_direction_and_return_to_the_pose() {
    for direction in [Vec3::NEG_Z, Vec3::Z, Vec3::X, Vec3::NEG_X] {
        let (mut app, rig) = standing();
        push(&mut app, rig, direction * 12.0);
        for step in 0..600 {
            app.update();
            let s = bodies(&app, rig).active().clone();
            assert_eq!(s.phase(), RagdollPhase::Active, "{direction} step {step}: {s:?}");
        }
        let error = pose_error(&app, rig);
        println!("{direction}: joint error {error:.4} rad after the push");
        assert!(error < 0.05, "{direction}: back on the pose ({error} rad)");
        assert!(pelvis_height(&app, rig) > 0.9, "{direction}");
    }
}

/// Whether the push decides the outcome: along every axis 20 kg·m/s holds and 60 kg·m/s (about 0.9 m/s for the whole
/// body, the capture point well past the feet) falls, with the feet released.
#[test]
fn the_push_size_decides_along_every_axis() {
    for direction in [Vec3::NEG_Z, Vec3::Z, Vec3::X, Vec3::NEG_X] {
        let (mut app, rig) = standing();
        push(&mut app, rig, direction * 20.0);
        run(&mut app, 300);
        assert_eq!(
            bodies(&app, rig).active().phase(),
            RagdollPhase::Active,
            "{direction}: 20 kg m/s holds"
        );

        let (mut app, rig) = standing();
        let start = feet(&app, rig);
        push(&mut app, rig, direction * 60.0);
        let mut worst_capture = f32::MIN;
        for _ in 0..30 {
            app.update();
            let s = bodies(&app, rig).active().clone();
            worst_capture = worst_capture.max(s.capture_distance().unwrap_or(f32::MIN));
        }
        println!("{direction}: capture point up to {worst_capture:.3} m outside the feet");
        run(&mut app, 400);
        let s = bodies(&app, rig).active().clone();
        assert!(
            matches!(s.phase(), RagdollPhase::Falling | RagdollPhase::Passive),
            "{direction}: 60 kg m/s falls: {s:?}"
        );
        assert_eq!(s.planted_feet(), 0, "{direction}: the feet let go");
        assert!(pelvis_height(&app, rig) < 0.5, "{direction}: on the floor");
        let now = feet(&app, rig);
        assert!(
            now[0] != start[0] || now[1] != start[1],
            "{direction}: released feet move with the fall"
        );
    }
}

/// A foot only locks on a fixed body: on a dynamic crate it has to press on the crate, so it stays free.
#[test]
fn a_foot_only_plants_on_fixed_ground() {
    let mut app = app();
    floor(&mut app);
    let top = 0.3;
    let crate_body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, top * 0.5, 0.0)),
            RigidBody::default(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(1.0, top * 0.5, 1.0),
                },
                density: 2000.0,
                ..Collider::default()
            },
        ))
        .id();
    let mut ragdoll = Ragdoll::new(biped_profile(1.0));
    ragdoll.mode = RagdollMode::Active;
    let rig = spawn_rig(
        &mut app,
        biped_skeleton(),
        ragdoll,
        Transform::from_translation(Vec3::new(0.0, top + 0.001, 0.0)),
    );
    let mut touched = false;
    for _ in 0..12 {
        app.update();
        if let Some(b) = app.world().get::<RagdollBodies>(rig) {
            let s = b.active();
            touched |= s.support_points() > 0;
            assert_eq!(s.planted_feet(), 0, "{s:?}");
        }
    }
    assert!(touched, "the feet reached the crate");
    assert!(app.world().get::<RigidBody>(crate_body).is_some());
}

/// A hard hit on the legs takes their strength away: the planted biped sinks and falls, though its center of mass
/// never left the feet.
#[test]
fn a_hard_hit_on_the_legs_collapses_it() {
    let mut app = app();
    floor(&mut app);
    let mut profile = biped_profile(1.0);
    profile.active.hit.full_impulse = 2.0;
    profile.active.hit.recover_time = 30.0;
    let mut ragdoll = Ragdoll::new(profile);
    ragdoll.mode = RagdollMode::Active;
    let rig = spawn_rig(
        &mut app,
        biped_skeleton(),
        ragdoll,
        Transform::from_translation(Vec3::new(0.0, 0.001, 0.0)),
    );
    run(&mut app, 30);
    assert_eq!(bodies(&app, rig).active().phase(), RagdollPhase::Active);
    app.world_mut().write_message(RagdollHit {
        rig,
        joint: "thigh_l".into(),
        impulse: Vec3::new(0.0, 0.0, -2.0),
        reaction: 1.0,
        recover: 30.0,
    });
    run(&mut app, 240);
    let s = bodies(&app, rig).active().clone();
    assert!(
        matches!(s.phase(), RagdollPhase::Falling | RagdollPhase::Passive),
        "weak legs do not hold it: {s:?}"
    );
    assert!(pelvis_height(&app, rig) < 0.6);
}

/// Leaving `Active` clears the joint drives: the same bend then stays off the pose, as for a ragdoll that was never
/// active, and the feet of a standing biped let go.
#[test]
fn leaving_active_clears_the_drives_and_the_feet() {
    let (mut app, rig) = chain(CHAIN_KP, CHAIN_KD);
    run(&mut app, 3);
    set_mode(&mut app, rig, RagdollMode::Limp);
    run(&mut app, 2);
    bend(&mut app, rig);
    run(&mut app, 121);
    let error = chain_error(&app, rig);
    assert!(error > 0.1, "no motor pulls it back ({error} rad)");
    assert_eq!(bodies(&app, rig).active().driven_motors(), 0);

    let (mut app, rig) = standing();
    assert_eq!(bodies(&app, rig).active().planted_feet(), 2);
    set_mode(&mut app, rig, RagdollMode::Limp);
    run(&mut app, 300);
    assert_eq!(bodies(&app, rig).active().planted_feet(), 0);
    assert!(
        pelvis_height(&app, rig) < 0.5,
        "limp, it crumples: feet and joints are free"
    );
}

/// Switched to `Active` in the air and dropped: whatever the landing does, nothing blows up and it ends in a real phase.
/// A foot that lands plants where it lands.
#[test]
fn a_drop_lands_without_blowing_up() {
    for (height, tilt) in [(0.3, 0.0), (0.3, 0.3), (1.0, 0.0)] {
        let mut app = app();
        floor(&mut app);
        let mut ragdoll = Ragdoll::new(biped_profile(1.0));
        ragdoll.mode = RagdollMode::Active;
        let rig = spawn_rig(
            &mut app,
            biped_skeleton(),
            ragdoll,
            Transform {
                translation: Vec3::new(0.0, height, 0.0),
                rotation: Quat::from_rotation_z(tilt),
                scale: Vec3::ONE,
            },
        );
        run(&mut app, 400);
        let s = bodies(&app, rig).active().clone();
        println!(
            "drop {height} m, tilt {tilt} rad: {:?}, pelvis {:.3} m",
            s.phase(),
            pelvis_height(&app, rig)
        );
        assert!(
            body_bits(&app, rig)
                .iter()
                .all(|b| f32::from_bits(*b).is_finite())
        );
        assert!(pelvis_height(&app, rig).abs() < 3.0, "it stays near the floor");
        assert_ne!(s.phase(), RagdollPhase::Animated);
    }
}

/// Prints how large a shove the biped holds in each direction (ignored; ADR-2564 quotes the table).
#[test]
#[ignore]
fn push_limits_table() {
    for (name, direction) in [
        ("forward", Vec3::NEG_Z),
        ("backward", Vec3::Z),
        ("right", Vec3::X),
        ("left", Vec3::NEG_X),
    ] {
        let mut row = String::new();
        for impulse in (10..=70).step_by(5) {
            let (mut app, rig) = standing();
            push(&mut app, rig, direction * impulse as f32);
            run(&mut app, 600);
            let holds = bodies(&app, rig).active().phase() == RagdollPhase::Active;
            row.push_str(&format!(" {impulse}:{}", if holds { "hold" } else { "fall" }));
        }
        println!("{name}:{row}");
    }
}

/// Two runs of the same push give the same rapier digest and body bits at every checkpoint.
#[test]
fn two_active_runs_give_the_same_digest() {
    let digest = || {
        let (mut app, rig) = standing();
        push(&mut app, rig, Vec3::new(4.0, 0.0, -10.0));
        let mut trace = Vec::new();
        for step in 0..180 {
            app.update();
            if step % 30 == 0 {
                trace.push(app.world().resource::<PhysicsWorld>().state_digest());
                trace.extend(body_bits(&app, rig).iter().map(|b| u64::from(*b)));
            }
        }
        trace
    };
    assert_eq!(digest(), digest());
}

// ---------------------------------------------------------------------------------------------------------------
// Get-up selection
// ---------------------------------------------------------------------------------------------------------------

fn lying(rotation: Quat) -> (App, Entity) {
    let mut app = app();
    floor(&mut app);
    let mut ragdoll = Ragdoll::new(biped_profile(0.0));
    ragdoll.mode = RagdollMode::Limp;
    let rig = spawn_rig(
        &mut app,
        biped_skeleton(),
        ragdoll,
        Transform {
            translation: Vec3::new(0.0, 0.25, 0.0),
            rotation,
            scale: Vec3::ONE,
        },
    );
    (app, rig)
}

fn settle(app: &mut App, rig: Entity) {
    for _ in 0..600 {
        app.update();
        if app.world().get::<RagdollBodies>(rig).is_some_and(|b| b.settled()) {
            return;
        }
    }
    panic!("the ragdoll did not settle in 600 steps");
}

/// Lying face down (forward pointing at the floor), the pelvis' facing is `FaceDown` and the face-down clip is
/// chosen; on its back the face-up one. Moving it again clears the choice; going back to the animation is the
/// recover path (`GettingUp` while the fallen pose blends out).
#[test]
fn a_prone_pelvis_picks_the_face_down_clip() {
    // Rotating -90 degrees about X turns the forward (-Z) to -Y: face down, head towards -Z.
    let (mut app, rig) = lying(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2));
    settle(&mut app, rig);
    let s = bodies(&app, rig).active().clone();
    assert_eq!(s.phase(), RagdollPhase::Passive);
    assert_eq!(s.facing(), Some(RagdollFacing::FaceDown));
    assert_eq!(s.get_up_clip(), Some("getup_front"));
    set_mode(&mut app, rig, RagdollMode::Kinematic);
    run(&mut app, 2);
    assert_eq!(bodies(&app, rig).active().phase(), RagdollPhase::GettingUp);
    assert_eq!(bodies(&app, rig).active().get_up_clip(), None);
    run(&mut app, 60);
    assert_eq!(bodies(&app, rig).active().phase(), RagdollPhase::Animated);

    let (mut app, rig) = lying(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2));
    settle(&mut app, rig);
    let s = bodies(&app, rig).active().clone();
    assert_eq!(s.facing(), Some(RagdollFacing::FaceUp));
    assert_eq!(s.get_up_clip(), Some("getup_back"));
}

// ---------------------------------------------------------------------------------------------------------------
// Bench
// ---------------------------------------------------------------------------------------------------------------

/// Steps 20 standing active bipeds (8 bodies, 7 motors, balance each) for 300 fixed steps and prints the mean fixed
/// step cost. Ignored: run with `--release -- --ignored --nocapture`; the number goes in the lane report.
#[test]
#[ignore]
fn perf_active_ragdolls_step() {
    let mut app = app();
    floor(&mut app);
    let mut rigs = Vec::new();
    for i in 0..20 {
        let mut ragdoll = Ragdoll::new(biped_profile(1.0));
        ragdoll.mode = RagdollMode::Active;
        let x = (i % 5) as f32 * 1.5;
        let z = (i / 5) as f32 * 1.5;
        rigs.push(spawn_rig(
            &mut app,
            biped_skeleton(),
            ragdoll,
            Transform::from_translation(Vec3::new(x, 0.001, z)),
        ));
    }
    run(&mut app, 30);
    let start = std::time::Instant::now();
    run(&mut app, 300);
    let per_step = start.elapsed().as_secs_f64() * 1000.0 / 300.0;
    let standing = rigs
        .iter()
        .filter(|r| bodies(&app, **r).active().phase() == RagdollPhase::Active)
        .count();
    println!(
        "perf_active_ragdolls_step: 20 active bipeds, {per_step:.3} ms per fixed step (whole frame), {standing} standing"
    );
}
