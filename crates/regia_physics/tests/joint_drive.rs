// SPDX-License-Identifier: MPL-2.0
//! The joint drive and foot lock helpers of `regia_physics::pd` (spec 93 § 10, ADR-2564): stiffness in real units, the
//! torque read-back, world-axis torque limits, clearing, locking and the fixed-ground test. Fixed steps, no wall clock.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::pd::{
    JointDrive, joint_drive_torque, on_fixed_ground, set_body_locked, set_body_locked_moving, set_joint_drive,
};
use regia_physics::{
    Collider, ColliderShape, Joint, JointKind, JointNoContacts, PhysicsPlugin, PhysicsPose, PhysicsWorld,
    RigidBody,
};
use regia_scene::Transform;

const HZ: f64 = 60.0;
const DT: f32 = 1.0 / 60.0;
const LENGTH: f32 = 0.5;
const MASS: f32 = 4.0;
const GRAVITY: f32 = 9.81;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = HZ;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// A rod of `MASS` hanging out horizontally along +X from a fixed anchor by a ball joint at the rod's end.
fn arm(app: &mut App) -> Entity {
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            RigidBody::static_body(),
        ))
        .id();
    let radius = 0.04;
    let density = MASS / (LENGTH * 2.0 * radius * 2.0 * radius);
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            RigidBody::default(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(LENGTH * 0.5, radius, radius),
                },
                offset: Transform::from_translation(Vec3::new(LENGTH * 0.5, 0.0, 0.0)),
                density,
                ..Collider::default()
            },
            Joint {
                kind: JointKind::Spherical { cone_limit: None },
                connected: anchor,
                anchor: Vec3::ZERO,
                connected_anchor: Vec3::ZERO,
                break_force: 0.0,
            },
            JointNoContacts,
        ))
        .id()
}

fn run(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.update();
    }
}

fn rotation(app: &App, body: Entity) -> Quat {
    app.world()
        .get::<PhysicsPose>(body)
        .map_or(Quat::IDENTITY, |p| p.rotation)
}

fn drive(app: &mut App, body: Entity, kp: f32, kd: f32, max_torque: Vec3) {
    let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
    set_joint_drive(
        &mut world,
        body,
        Some(JointDrive {
            target: Quat::IDENTITY,
            kp,
            kd,
            max_torque,
        }),
    );
}

/// The load of the arm about its joint is `m g L/2`. The drive is an implicit spring that rapier solves once per substep
/// without warm starting, so it holds the load with some fraction of `kp` (ADR-2564): the sag is at least `load / kp`
/// and within a small factor of it. The torque read-back reports the load itself.
#[test]
fn the_drive_holds_a_load_near_the_authored_stiffness() {
    let mut app = app();
    let body = arm(&mut app);
    run(&mut app, 2);
    let kp = 400.0;
    for _ in 0..300 {
        drive(&mut app, body, kp, 40.0, Vec3::splat(1000.0));
        app.update();
    }
    let load = MASS * GRAVITY * LENGTH * 0.5;
    let sag = rotation(&app, body).angle_between(Quat::IDENTITY);
    let expected = load / kp;
    let world = app.world().resource::<PhysicsWorld>();
    let torque = joint_drive_torque(world, body, DT / 4.0);
    println!("sag {sag:.5} rad, load / kp {expected:.5} rad; torque {torque:.3} N m, load {load:.3} N m");
    assert!(
        sag > expected * 0.8 && sag < expected * 3.0,
        "sag {sag} rad against load / kp = {expected} rad"
    );
    assert!(
        (torque - load).abs() < load * 0.25,
        "torque {torque} against load {load}"
    );
}

/// The limit is about world axes: the load is about world Z, so a small Z limit drops the arm and the same limit on the
/// other axes does not.
#[test]
fn the_torque_limit_is_per_world_axis() {
    let load = MASS * GRAVITY * LENGTH * 0.5;
    let settle = |limit: Vec3| {
        let mut app = app();
        let body = arm(&mut app);
        run(&mut app, 2);
        for _ in 0..120 {
            drive(&mut app, body, 400.0, 40.0, limit);
            app.update();
        }
        rotation(&app, body).angle_between(Quat::IDENTITY)
    };
    let held = settle(Vec3::new(1.0, 1.0, load * 2.0));
    let dropped = settle(Vec3::new(load * 2.0, load * 2.0, 1.0));
    println!("held {held:.3} rad, dropped {dropped:.3} rad");
    assert!(held < 0.1, "{held}");
    assert!(dropped > 0.5, "{dropped}");
}

/// Clearing a drive frees the joint: the arm swings down, and the read-back is zero.
#[test]
fn clearing_a_drive_frees_the_joint() {
    let mut app = app();
    let body = arm(&mut app);
    run(&mut app, 2);
    for _ in 0..60 {
        drive(&mut app, body, 400.0, 40.0, Vec3::splat(1000.0));
        app.update();
    }
    assert!(rotation(&app, body).angle_between(Quat::IDENTITY) < 0.1);
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_joint_drive(&mut world, body, None);
    }
    run(&mut app, 120);
    assert!(rotation(&app, body).angle_between(Quat::IDENTITY) > 0.5);
    let world = app.world().resource::<PhysicsWorld>();
    assert_eq!(joint_drive_torque(world, body, DT / 4.0), 0.0);
}

/// A drive on an entity with no joint, or a non-finite one, is ignored; so is clearing a joint that has no drive.
#[test]
fn bad_drive_input_is_ignored() {
    let mut app = app();
    let body = arm(&mut app);
    let stray = app.world_mut().spawn(Transform::default()).id();
    run(&mut app, 2);
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_joint_drive(&mut world, stray, None);
        set_joint_drive(
            &mut world,
            body,
            Some(JointDrive {
                target: Quat::IDENTITY,
                kp: f32::NAN,
                kd: 1.0,
                max_torque: Vec3::ONE,
            }),
        );
        set_body_locked(&mut world, stray, true);
        assert_eq!(joint_drive_torque(&world, stray, DT), 0.0);
    }
    run(&mut app, 60);
    assert!(
        rotation(&app, body).angle_between(Quat::IDENTITY) > 0.1,
        "the NaN drive set nothing"
    );
}

fn falling_ball(app: &mut App, y: f32) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, y, 0.0)),
            RigidBody::default(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.1 },
                ..Collider::default()
            },
        ))
        .id()
}

fn height(app: &App, body: Entity) -> f32 {
    app.world()
        .get::<PhysicsPose>(body)
        .map_or(f32::NAN, |p| p.translation.y)
}

/// Locking stops a moving body where it is (a locked axis alone would keep its speed), and unlocking lets it fall again.
#[test]
fn a_locked_body_stops_and_an_unlocked_one_moves() {
    let mut app = app();
    let ball = falling_ball(&mut app, 10.0);
    run(&mut app, 20);
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_body_locked(&mut world, ball, true);
    }
    run(&mut app, 1);
    let at = height(&app, ball);
    run(&mut app, 60);
    assert_eq!(height(&app, ball), at, "locked: not moving at all");
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_body_locked(&mut world, ball, false);
    }
    run(&mut app, 30);
    assert!(height(&app, ball) < at - 0.5, "free again");
}

/// A body that followed a moving support (locked, with a velocity) and then gets a stationary lock stops: the axes were
/// already locked, so only the velocity write can stop it (review fix for the ragdoll stepping lane).
#[test]
fn a_stationary_lock_stops_a_body_that_was_locked_and_moving() {
    let mut app = app();
    let ball = falling_ball(&mut app, 10.0);
    run(&mut app, 2);
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_body_locked_moving(
            &mut world,
            ball,
            Vec3::new(0.0, 0.5, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        );
    }
    run(&mut app, 10);
    let rising = height(&app, ball);
    run(&mut app, 10);
    assert!(height(&app, ball) > rising, "a moving lock carries the body");
    {
        let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
        set_body_locked(&mut world, ball, true);
    }
    run(&mut app, 1);
    let at = height(&app, ball);
    run(&mut app, 60);
    assert_eq!(
        height(&app, ball),
        at,
        "a stationary lock after a moving one: not moving at all"
    );
}

/// `on_fixed_ground` is true only when the body touches something and all of it is fixed; touching a dynamic body or
/// nothing is false; touching an excluded body does not count.
#[test]
fn fixed_ground_is_only_fixed_bodies() {
    let mut app = app();
    let floor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(5.0, 0.5, 5.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let under = falling_ball(&mut app, 0.1);
    let over = falling_ball(&mut app, 0.3);
    let in_air = falling_ball(&mut app, 3.0);
    let alone = falling_ball(&mut app, 0.1);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(alone) {
        t.translation.x = 2.0;
    }
    run(&mut app, 12);
    let world = app.world().resource::<PhysicsWorld>();
    assert!(on_fixed_ground(world, alone, &[]), "on the floor only");
    assert!(!on_fixed_ground(world, in_air, &[]), "touches nothing");
    assert!(!on_fixed_ground(world, over, &[]), "on a dynamic body");
    assert!(
        !on_fixed_ground(world, under, &[]),
        "the floor, and a dynamic body on it"
    );
    assert!(
        on_fixed_ground(world, under, &[over]),
        "the dynamic body is excluded"
    );
    assert!(
        !on_fixed_ground(world, under, &[floor, over]),
        "only excluded bodies"
    );
}
