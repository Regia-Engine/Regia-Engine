// SPDX-License-Identifier: MPL-2.0
//! Audit-3 regressions: dynamic trimesh refusal, fixed-joint rotation, joint contact marker.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    AuthoredCollisionStatus, Collider, ColliderHandle, ColliderShape, Joint, JointHandle, JointKind,
    JointNoContacts, PhysicsPlugin, PhysicsWorld, RigidBody,
};
use regia_scene::Transform;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

fn run(app: &mut App, steps: u32) {
    for _ in 0..steps {
        app.update();
    }
}

fn box_collider(half: f32) -> Collider {
    Collider {
        shape: ColliderShape::Box {
            half_extents: Vec3::splat(half),
        },
        ..Collider::default()
    }
}

fn spawn_body(app: &mut App, kind: RigidBody, translation: Vec3, rotation: Quat) -> Entity {
    let mut transform = Transform::from_translation(translation);
    transform.rotation = rotation;
    app.world_mut().spawn((transform, kind, box_collider(0.5))).id()
}

#[test]
fn dynamic_trimesh_is_not_inserted() {
    let mut app = physics_app();
    let shape = ColliderShape::trimesh_from_triangles(
        &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
        &[0, 1, 2],
    )
    .expect("trimesh");
    let dynamic = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::dynamic(),
            Collider {
                shape: shape.clone(),
                ..Collider::default()
            },
        ))
        .id();
    let fixed = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape,
                ..Collider::default()
            },
        ))
        .id();
    run(&mut app, 2);
    assert!(
        app.world().get::<ColliderHandle>(dynamic).is_none(),
        "a dynamic body takes no triangle-mesh collider"
    );
    assert!(
        app.world().get::<ColliderHandle>(fixed).is_some(),
        "a static body still does"
    );
}

#[test]
fn a_flat_dynamic_mesh_stays_attached_and_still_has_no_rapier_collider() {
    let mut app = physics_app();
    let shape = ColliderShape::trimesh_from_triangles(
        &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
        &[0, 1, 2, 0, 2, 3],
    )
    .expect("planar quad");
    let dynamic = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::dynamic(),
            Collider {
                shape,
                ..Collider::default()
            },
            AuthoredCollisionStatus::Attached {
                shapes: 1,
                trimesh_as_hull: 0,
            },
        ))
        .id();
    run(&mut app, 2);
    assert!(
        app.world().get::<ColliderHandle>(dynamic).is_none(),
        "the sync drops the dynamic triangle mesh"
    );
    assert_eq!(
        app.world().get::<AuthoredCollisionStatus>(dynamic).cloned(),
        Some(AuthoredCollisionStatus::Attached {
            shapes: 1,
            trimesh_as_hull: 0,
        }),
        "Attached stays, with no hull conversion"
    );
    assert!(app.world().get::<Collider>(dynamic).is_some());
}

#[test]
fn fixed_joint_keeps_a_spawned_relative_rotation() {
    let mut app = physics_app();
    let anchor = spawn_body(
        &mut app,
        RigidBody::static_body(),
        Vec3::new(0.0, 4.0, 0.0),
        Quat::IDENTITY,
    );
    let spawned = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    let sign = spawn_body(&mut app, RigidBody::dynamic(), Vec3::new(0.0, 3.0, 0.0), spawned);
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Fixed,
        connected: sign,
        anchor: Vec3::new(0.0, -0.5, 0.0),
        connected_anchor: Vec3::new(0.0, 0.5, 0.0),
        break_force: 0.0,
    });
    run(&mut app, 120);
    let rotation = app.world().get::<Transform>(sign).expect("transform").rotation;
    assert!(
        rotation.angle_between(spawned) < 0.05,
        "the weld keeps the spawned yaw: {rotation:?}"
    );
}

#[test]
fn joint_no_contacts_marker_toggles_contacts_on_a_live_joint() {
    let mut app = physics_app();
    let a = spawn_body(
        &mut app,
        RigidBody::static_body(),
        Vec3::new(0.0, 4.0, 0.0),
        Quat::IDENTITY,
    );
    let b = spawn_body(
        &mut app,
        RigidBody::dynamic(),
        Vec3::new(0.0, 3.0, 0.0),
        Quat::IDENTITY,
    );
    app.world_mut().entity_mut(a).insert(Joint {
        kind: JointKind::Distance { min: 0.0, max: 2.0 },
        connected: b,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::ZERO,
        break_force: 0.0,
    });
    run(&mut app, 2);
    let contacts = |app: &App| {
        let handle = app
            .world()
            .get::<JointHandle>(a)
            .expect("joint handle")
            .to_handle();
        let world = app.world().resource::<PhysicsWorld>();
        world
            .impulse_joints
            .get(handle)
            .expect("joint")
            .data
            .contacts_enabled
    };
    assert!(contacts(&app), "contacts are on by default");
    app.world_mut().entity_mut(a).insert(JointNoContacts);
    run(&mut app, 2);
    assert!(!contacts(&app), "adding the marker later disables contacts");
    app.world_mut().entity_mut(a).remove::<JointNoContacts>();
    run(&mut app, 2);
    assert!(contacts(&app), "removing the marker re-enables contacts");
}

#[test]
fn walkable_steep_slope_reports_ground_entity_and_normal() {
    use regia_physics::{CharacterBody, move_character};
    let mut app = physics_app();
    let tilt = 70.0f32.to_radians();
    let wedge = app
        .world_mut()
        .spawn((
            Transform {
                rotation: Quat::from_rotation_z(tilt),
                ..Transform::IDENTITY
            },
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(5.0, 0.5, 5.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    run(&mut app, 2);
    let normal = Quat::from_rotation_z(tilt) * Vec3::Y;
    let centre = normal * (0.5 + 0.3 + 0.005);
    let mut world = app.world_mut().resource_mut::<PhysicsWorld>();
    let result = move_character(
        &mut world,
        &CharacterBody::default(),
        0.3,
        0.5,
        (centre, Quat::IDENTITY),
        Vec3::new(0.0, -0.05, 0.0),
        80.0f32.to_radians(),
        0.0,
        0.0,
        None,
        1.0 / 60.0,
        1.0 / 60.0,
    );
    assert_eq!(
        result.ground_entity, wedge,
        "a slope within the climb angle is ground"
    );
    assert!(
        result.ground_normal.dot(normal) > 0.95,
        "{:?}",
        result.ground_normal
    );
}
