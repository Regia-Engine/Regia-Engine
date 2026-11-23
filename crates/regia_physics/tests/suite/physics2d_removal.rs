// SPDX-License-Identifier: MPL-2.0
//! Removing one 2D component must not strand the other rapier object, and a degenerate
//! polygon must not turn into a substitute ball.
#![cfg(feature = "physics2d")]

use regia_app::prelude::*;
use regia_core::glam::Vec2;
use regia_core::math::Vec3;
use regia_physics::physics2d::{
    Body2dHandle, Collider2d, Collider2dHandle, ColliderShape2d, Physics2dPlugin, PhysicsWorld2d, RigidBody2d,
};
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, Physics2dPlugin));
    enable_fixed_step(&mut app, 60.0);
    app
}

fn steps(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn counts(app: &App) -> (usize, usize) {
    let w = app.world().resource::<PhysicsWorld2d>();
    (w.world.bodies.len(), w.world.colliders.len())
}

#[test]
fn removing_collider_keeps_body_and_readding_rebuilds() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            RigidBody2d::default(),
            Collider2d::cuboid(1.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
        ))
        .id();
    steps(&mut app, 3);
    assert_eq!(counts(&app), (1, 1));
    app.world_mut().entity_mut(e).remove::<Collider2d>();
    steps(&mut app, 3);
    assert_eq!(counts(&app), (1, 0), "the body survives a removed collider");
    assert!(app.world().get::<Body2dHandle>(e).is_some());
    assert!(app.world().get::<Collider2dHandle>(e).is_none());
    app.world_mut().entity_mut(e).insert(Collider2d::cuboid(1.0, 1.0));
    steps(&mut app, 3);
    assert_eq!(counts(&app), (1, 1));
}

#[test]
fn removing_body_clears_both_handles_and_rebuilds_the_rest() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn((
            RigidBody2d::default(),
            Collider2d::cuboid(1.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
        ))
        .id();
    steps(&mut app, 3);
    app.world_mut().entity_mut(e).remove::<RigidBody2d>();
    steps(&mut app, 3);
    assert!(app.world().get::<Body2dHandle>(e).is_none());
    // The collider is rebuilt as a body-less collider, not left dangling.
    assert_eq!(counts(&app), (0, 1));
    assert!(app.world().get::<Collider2dHandle>(e).is_some());
    app.world_mut().entity_mut(e).insert(RigidBody2d::default());
    steps(&mut app, 3);
    assert!(
        app.world().get::<Body2dHandle>(e).is_some(),
        "the body simulates again"
    );
}

#[test]
fn degenerate_polygon_creates_no_collider() {
    let mut app = app();
    let e = app
        .world_mut()
        .spawn(Collider2d {
            shape: ColliderShape2d::ConvexPolygon {
                points: vec![Vec2::ZERO, Vec2::X],
            },
            ..Collider2d::default()
        })
        .id();
    steps(&mut app, 3);
    assert_eq!(counts(&app), (0, 0));
    assert!(app.world().get::<Collider2dHandle>(e).is_none());
}
