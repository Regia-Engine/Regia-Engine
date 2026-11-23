// SPDX-License-Identifier: MPL-2.0
//! Contract tests for three public helpers that had no direct test (audit finding 10):
//! `move_anchored_character`, `capsule_has_headroom` and `WorldQueries::overlap_aabb`.
//!
//! Provenance: elementary geometry built here; sweeps are rapier3d's (Apache-2.0).

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::character::move_anchored_character;
use regia_physics::{
    CharacterBody, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, QueryFilter, RigidBody,
    WorldQueries, capsule_has_headroom, move_character,
};
use regia_scene::Transform;

const RADIUS: f32 = 0.3;
const HALF_HEIGHT: f32 = 0.6;
const DT: f32 = 1.0 / 60.0;
const REST_Y: f32 = HALF_HEIGHT + RADIUS + 0.02;

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

fn spawn_box(app: &mut App, body: RigidBody, half: Vec3, centre: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(centre),
            body,
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

fn settle(app: &mut App) {
    for _ in 0..5 {
        app.update();
    }
}

#[test]
fn anchored_character_ignores_ground_motion_that_a_free_one_inherits() {
    let mut app = physics_app();
    // A kinematic platform whose top is y = 0, sliding along +x at 3 m/s.
    let platform = spawn_box(
        &mut app,
        RigidBody::kinematic(),
        Vec3::new(20.0, 0.5, 20.0),
        Vec3::new(0.0, -0.5, 0.0),
    );
    // Kinematic velocity comes from the pose moving between steps.
    for _ in 0..10 {
        app.world_mut()
            .get_mut::<Transform>(platform)
            .expect("t")
            .translation
            .x += 3.0 * DT;
        app.update();
    }

    let pos = Vec3::new(0.0, REST_Y, 0.0);
    let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
    let args = |_: ()| {
        (
            CharacterBody::default(),
            (pos, Quat::IDENTITY),
            50.0_f32.to_radians(),
        )
    };
    let (body, pose, slope) = args(());
    let free = move_character(
        &mut physics,
        &body,
        RADIUS,
        HALF_HEIGHT,
        pose,
        Vec3::ZERO,
        slope,
        0.0,
        0.1,
        None,
        DT,
        DT,
    );
    let anchored = move_anchored_character(
        &mut physics,
        &body,
        RADIUS,
        HALF_HEIGHT,
        pose,
        Vec3::ZERO,
        slope,
        0.0,
        0.1,
        None,
        DT,
        DT,
    );
    assert!(free.grounded && anchored.grounded, "both report the ground");
    assert_eq!(free.ground_entity, platform);
    assert_eq!(anchored.ground_entity, platform);
    assert!(
        (anchored.ground_velocity.x - 3.0).abs() < 0.3,
        "the ground's motion is still reported: {:?}",
        anchored.ground_velocity
    );
    assert!(
        anchored.translation.x.abs() < 1.0e-3,
        "the anchored move does not carry the character: {:?}",
        anchored.translation
    );
    assert!(
        free.translation.x > 0.03,
        "the free move is carried with the platform: {:?}",
        free.translation
    );
}

#[test]
fn headroom_is_refused_under_a_ceiling_and_floor_contact_does_not_count() {
    let mut app = physics_app();
    spawn_box(
        &mut app,
        RigidBody::static_body(),
        Vec3::new(20.0, 0.5, 20.0),
        Vec3::new(0.0, -0.5, 0.0),
    );
    // A ceiling slab whose underside is at y = 2.0.
    let ceiling = spawn_box(
        &mut app,
        RigidBody::static_body(),
        Vec3::new(5.0, 0.5, 5.0),
        Vec3::new(10.0, 2.5, 0.0),
    );
    settle(&mut app);
    let physics = app.world().resource::<PhysicsWorld>();
    // Standing on the floor at the origin, open sky: may rise.
    let open = Vec3::new(0.0, REST_Y, 0.0);
    assert!(capsule_has_headroom(
        physics,
        RADIUS,
        HALF_HEIGHT,
        open,
        0.5,
        0.02,
        None
    ));
    // Under the slab: capsule top is REST_Y + 0.9 = 1.82, 0.18 below the slab; rising 0.7 reaches it.
    let under = Vec3::new(10.0, REST_Y, 0.0);
    assert!(!capsule_has_headroom(
        physics,
        RADIUS,
        HALF_HEIGHT,
        under,
        0.7,
        0.02,
        None
    ));
    // A rise (plus skin) that stops short of the slab is fine.
    assert!(capsule_has_headroom(
        physics,
        RADIUS,
        HALF_HEIGHT,
        under,
        0.1,
        0.02,
        None
    ));
    let _ = ceiling;
}

#[test]
fn overlap_aabb_sees_heightfields_and_skips_disjoint_boxes() {
    let mut app = physics_app();
    let n = 5usize;
    let field = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::HeightField {
                    nx: n,
                    nz: n,
                    heights: vec![0.0; n * n],
                    scale: Vec3::new(8.0, 1.0, 8.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let far = spawn_box(
        &mut app,
        RigidBody::static_body(),
        Vec3::splat(0.5),
        Vec3::new(100.0, 0.0, 100.0),
    );
    settle(&mut app);
    let world = app.world().resource::<PhysicsWorld>();
    let queries = WorldQueries { world, layers: None };
    let hits = queries.overlap_aabb(
        Vec3::new(-1.0, -1.0, -1.0),
        Vec3::new(1.0, 1.0, 1.0),
        QueryFilter::new(),
    );
    assert!(hits.contains(&field), "the heightfield is in the box: {hits:?}");
    assert!(!hits.contains(&far), "the disjoint box is not: {hits:?}");
    let none = queries.overlap_aabb(Vec3::splat(50.0), Vec3::splat(51.0), QueryFilter::new());
    assert!(none.is_empty(), "empty region: {none:?}");
}
