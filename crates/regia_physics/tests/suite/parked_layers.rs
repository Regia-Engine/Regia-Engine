// SPDX-License-Identifier: MPL-2.0
//! Members of a parked data layer are not in the physics world (spec 37 § 13): a body spawned
//! parked is never inserted, a live one leaves and comes back with its pose and velocity, and the
//! 2D, platformer and vehicle paths follow.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehiclePlugin, spawn_vehicle};
use regia_physics::{
    BodyKind, Collider, ColliderHandle, ColliderShape, Joint, JointKind, PhysicsPlugin, PhysicsWorld,
    RigidBody, RigidBodyHandle, Velocity,
};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use regia_scene::prelude::Visibility;

fn at(x: f32, y: f32) -> Transform {
    Transform {
        translation: Vec3::new(x, y, 0.0),
        ..Transform::default()
    }
}

fn parked() -> LayerParked {
    LayerParked {
        restore: Visibility::Inherited,
        inherited: false,
    }
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(regia_app::FixedTime::default());
    app.add_plugins(PhysicsPlugin);
    app
}

/// Runs `n` fixed steps directly (no frame clock involved).
fn step(app: &mut App, n: usize) {
    for _ in 0..n {
        app.world_mut().run_schedule(FixedUpdate);
    }
}

fn ball() -> (RigidBody, Collider, Transform) {
    (
        RigidBody {
            kind: BodyKind::Dynamic,
            ..Default::default()
        },
        Collider {
            shape: ColliderShape::Sphere { radius: 0.5 },
            ..Default::default()
        },
        at(0.0, 10.0),
    )
}

#[test]
fn a_body_spawned_parked_is_not_inserted_until_live() {
    let mut app = app();
    let entity = app.world_mut().spawn((ball(), parked())).id();
    step(&mut app, 3);
    assert_eq!(app.world().resource::<PhysicsWorld>().bodies.len(), 0);
    assert!(app.world().get::<RigidBodyHandle>(entity).is_none());
    assert_eq!(
        app.world().get::<Transform>(entity).expect("t").translation.y,
        10.0
    );

    app.world_mut().entity_mut(entity).remove::<LayerParked>();
    step(&mut app, 3);
    assert_eq!(app.world().resource::<PhysicsWorld>().bodies.len(), 1);
    assert!(app.world().get::<RigidBodyHandle>(entity).is_some());
    assert!(app.world().get::<Transform>(entity).expect("t").translation.y < 10.0);
}

#[test]
fn a_parked_body_leaves_the_world_and_returns_with_its_velocity() {
    let mut app = app();
    let entity = app.world_mut().spawn(ball()).id();
    step(&mut app, 5);
    let before = *app.world().get::<Transform>(entity).expect("t");
    // `ball()` has no `Velocity`: the motion is only in rapier when it parks.
    assert!(app.world().get::<Velocity>(entity).is_none());
    let handle = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("handle")
        .to_handle();
    let velocity = app
        .world()
        .resource::<PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("body")
        .linvel();
    assert!(velocity.y < 0.0, "it fell: {velocity:?}");

    app.world_mut().entity_mut(entity).insert(parked());
    step(&mut app, 10);
    let world = app.world();
    assert_eq!(world.resource::<PhysicsWorld>().bodies.len(), 0, "no rapier body");
    assert_eq!(
        world.resource::<PhysicsWorld>().colliders.len(),
        0,
        "no rapier collider"
    );
    assert!(world.get::<RigidBody>(entity).is_some(), "the component stays");
    assert_eq!(
        world.get::<Transform>(entity).expect("t").translation,
        before.translation
    );

    app.world_mut().entity_mut(entity).remove::<LayerParked>();
    step(&mut app, 1);
    let world = app.world();
    assert_eq!(world.resource::<PhysicsWorld>().bodies.len(), 1);
    assert_eq!(world.resource::<PhysicsWorld>().colliders.len(), 1);
    let handle = world.get::<RigidBodyHandle>(entity).expect("handle").to_handle();
    let speed = world
        .resource::<PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("body")
        .linvel()
        .y;
    assert!(speed <= velocity.y, "resumes at {velocity:?}, moves at {speed}");
}

#[test]
fn a_body_parked_before_it_reached_rapier_starts_with_its_authored_velocity() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            ball(),
            parked(),
            Velocity {
                linear: Vec3::new(3.0, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
        ))
        .id();
    step(&mut app, 3);
    assert_eq!(app.world().resource::<PhysicsWorld>().bodies.len(), 0);

    app.world_mut().entity_mut(entity).remove::<LayerParked>();
    step(&mut app, 2);
    let velocity = app.world().get::<Velocity>(entity).expect("velocity");
    assert!(
        velocity.linear.x > 2.9,
        "the authored velocity survives the parked ticks: {velocity:?}"
    );
    let x = app.world().get::<Transform>(entity).expect("t").translation.x;
    assert!(x > 0.0, "the body moved with it: {x}");
}

#[test]
fn a_collider_on_a_parked_child_leaves_while_the_body_stays() {
    let mut app = app();
    let body = app.world_mut().spawn(ball()).id();
    let child = app
        .world_mut()
        .spawn((
            Collider {
                shape: ColliderShape::Sphere { radius: 0.2 },
                ..Default::default()
            },
            Transform::default(),
            ChildOf(body),
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().colliders.len(), 2);

    app.world_mut().entity_mut(child).insert(parked());
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().colliders.len(), 1);
    assert!(app.world().get::<ColliderHandle>(child).is_none());

    app.world_mut().entity_mut(child).remove::<LayerParked>();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().colliders.len(), 2);
    assert!(app.world().get::<ColliderHandle>(child).is_some());
}

#[test]
fn a_joint_comes_back_with_its_parked_body() {
    let mut app = app();
    let anchor = app
        .world_mut()
        .spawn((
            RigidBody {
                kind: BodyKind::Static,
                ..Default::default()
            },
            Transform::default(),
        ))
        .id();
    let swinging = app
        .world_mut()
        .spawn((
            ball(),
            Joint {
                connected: anchor,
                kind: JointKind::Fixed,
                ..Joint::default()
            },
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().impulse_joints.len(), 1);

    app.world_mut().entity_mut(swinging).insert(parked());
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().impulse_joints.len(), 0);

    app.world_mut().entity_mut(swinging).remove::<LayerParked>();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld>().impulse_joints.len(), 1);
}

#[test]
fn a_parked_vehicle_keeps_its_chassis_disabled() {
    let mut app = app();
    app.add_plugins(VehiclePlugin);
    let entity = spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::new(0.0, 5.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("vehicle");
    let chassis = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("handle")
        .to_handle();
    step(&mut app, 2);
    let height = |app: &App| {
        app.world()
            .resource::<PhysicsWorld>()
            .bodies
            .get(chassis)
            .expect("chassis")
            .translation()
            .y
    };
    let before = height(&app);

    app.world_mut().entity_mut(entity).insert(parked());
    step(&mut app, 10);
    let world = app.world();
    assert!(world.get::<VehicleController>(entity).is_some());
    assert!(
        world.get::<RigidBodyHandle>(entity).is_some(),
        "the vehicle owns the handle"
    );
    assert!(
        !world
            .resource::<PhysicsWorld>()
            .bodies
            .get(chassis)
            .expect("chassis")
            .is_enabled()
    );
    assert_eq!(height(&app), before, "disabled bodies do not move");

    app.world_mut().entity_mut(entity).remove::<LayerParked>();
    step(&mut app, 2);
    assert!(
        app.world()
            .resource::<PhysicsWorld>()
            .bodies
            .get(chassis)
            .expect("chassis")
            .is_enabled()
    );
    assert!(height(&app) < before, "it falls again");
}

#[cfg(feature = "physics2d")]
#[test]
fn parked_2d_bodies_leave_the_world_and_return() {
    use regia_core::math::Vec2;
    use regia_physics::physics2d::{
        Collider2d, ColliderShape2d, Physics2dPlugin, PhysicsWorld2d, RigidBody2d, Velocity2d,
    };

    let mut app = App::new();
    app.insert_resource(regia_app::FixedTime::default());
    app.add_plugins(Physics2dPlugin);
    let entity = app
        .world_mut()
        .spawn((
            RigidBody2d::default(),
            Collider2d {
                shape: ColliderShape2d::Ball { radius: 0.5 },
                ..Default::default()
            },
            at(0.0, 3.0),
            Velocity2d {
                linear: Vec2::new(1.0, 0.0),
                angular: 0.0,
            },
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld2d>().body_count(), 1);

    app.world_mut().entity_mut(entity).insert(parked());
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld2d>().body_count(), 0);

    app.world_mut().entity_mut(entity).remove::<LayerParked>();
    step(&mut app, 2);
    assert_eq!(app.world().resource::<PhysicsWorld2d>().body_count(), 1);
}
