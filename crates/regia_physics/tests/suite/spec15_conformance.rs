// SPDX-License-Identifier: MPL-2.0
//! Spec 15 conformance gaps found by the 2026-11-10 audit (lane spec-05-14): items that were
//! implemented but had no test of their own. Gravity from `LevelSettings` (§ 1), the dominance
//! group (§ 2 `RigidBody`) and the prismatic motor (§ 2 `Joint`).
//!
//! Provenance: the pipeline exercised is rapier3d (Apache-2.0); the expectations are elementary
//! mechanics (a fall distance is linear in gravity for a fixed step count), not any engine's code.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, Joint, JointKind, JointMotorConfig, JointNoContacts, PhysicsPlugin, PhysicsPose,
    PhysicsSettings, RigidBody, Velocity,
};
use regia_scene::Transform;
use regia_scene::level::LevelSettings;

/// One fixed step per update: a manual 1/60 s frame clock at a 60 Hz fixed rate.
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

fn steps(app: &mut App, count: usize) {
    for _ in 0..count {
        app.update();
    }
}

/// The pose after the latest fixed step (the `Transform` is the interpolated one).
fn position(app: &App, entity: Entity) -> Vec3 {
    if let Some(pose) = app.world().get::<PhysicsPose>(entity) {
        return pose.translation;
    }
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

fn velocity(app: &App, entity: Entity) -> Velocity {
    app.world().get::<Velocity>(entity).cloned().unwrap_or_default()
}

fn ball(app: &mut App, at: Vec3, body: RigidBody, linear: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            body,
            Collider {
                shape: ColliderShape::Sphere { radius: 0.5 },
                restitution: 0.0,
                ..Collider::default()
            },
            Velocity {
                linear,
                angular: Vec3::ZERO,
            },
        ))
        .id()
}

/// How far a free body falls in `count` updates, with an optional level gravity.
fn fall_distance(level_gravity: Option<Vec3>, count: usize) -> f32 {
    let mut app = physics_app();
    if let Some(gravity) = level_gravity {
        app.insert_resource(LevelSettings {
            gravity,
            ..LevelSettings::default()
        });
    }
    let body = ball(
        &mut app,
        Vec3::new(0.0, 100.0, 0.0),
        RigidBody::dynamic(),
        Vec3::ZERO,
    );
    steps(&mut app, count);
    100.0 - position(&app, body).y
}

/// specs/15 § 1: "Gravity from `LevelSettings`": with a level loaded its gravity drives the step,
/// without one the `PhysicsSettings` gravity does. The fall distance after the same number of
/// steps is linear in gravity.
#[test]
fn gravity_comes_from_the_level_settings_when_a_level_is_loaded() {
    let settings_drop = fall_distance(None, 60);
    let level_drop = fall_distance(Some(Vec3::new(0.0, -20.0, 0.0)), 60);
    assert!(
        settings_drop > 1.0,
        "the body fell under the settings' gravity: {settings_drop}"
    );
    let ratio = level_drop / settings_drop;
    assert!(
        (ratio - 20.0 / 9.81).abs() < 1.0e-3,
        "the level's -20 m/s² replaced the settings' -9.81: drops {level_drop} vs {settings_drop}"
    );
    let sideways = {
        let mut app = physics_app();
        app.insert_resource(LevelSettings {
            gravity: Vec3::new(5.0, 0.0, 0.0),
            ..LevelSettings::default()
        });
        let body = ball(&mut app, Vec3::ZERO, RigidBody::dynamic(), Vec3::ZERO);
        steps(&mut app, 30);
        position(&app, body)
    };
    assert!(
        sideways.x > 0.1 && sideways.y.abs() < 1.0e-4,
        "the level gravity's direction is used too: {sideways}"
    );
}

/// Runs a moving ball into a resting one in zero gravity; returns the resting ball's end position
/// and the moving ball's end position.
fn collide(resting_dominance: i8) -> (Vec3, Vec3) {
    let mut app = physics_app();
    app.insert_resource(PhysicsSettings {
        gravity: Vec3::ZERO,
        ..PhysicsSettings::default()
    });
    let resting = ball(
        &mut app,
        Vec3::ZERO,
        RigidBody {
            dominance_group: resting_dominance,
            ..RigidBody::dynamic()
        },
        Vec3::ZERO,
    );
    let moving = ball(
        &mut app,
        Vec3::new(-2.0, 0.0, 0.0),
        RigidBody::dynamic(),
        Vec3::new(4.0, 0.0, 0.0),
    );
    steps(&mut app, 60);
    (position(&app, resting), position(&app, moving))
}

/// specs/15 § 2 `RigidBody` dominance group: a body is never pushed by a body in a lower group.
#[test]
fn a_dominant_body_is_not_pushed_by_a_lower_group() {
    let (pushed, _) = collide(0);
    assert!(
        pushed.x > 0.1,
        "control: in the same group the hit ball is pushed: {pushed}"
    );
    let (dominant, hitter) = collide(1);
    assert!(
        dominant.length() < 1.0e-3,
        "the dominant ball stays where it was: {dominant}"
    );
    assert!(
        hitter.x < -0.9,
        "the hitter stops at the dominant ball instead of passing it: {hitter}"
    );
}

/// specs/15 § 2 `Joint`: a prismatic motor drives the slider at its target velocity, along the
/// axis only (the revolute motor has its own test in `robustness.rs`).
#[test]
fn prismatic_motor_reaches_its_target_velocity() {
    let mut app = physics_app();
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.1),
                },
                ..Collider::default()
            },
        ))
        .id();
    let slider = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.3, 0.1, 0.3),
                },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    app.world_mut().entity_mut(anchor).insert((
        Joint {
            kind: JointKind::Prismatic {
                axis: Vec3::X,
                limits: None,
                motor: Some(JointMotorConfig {
                    target_position: 0.0,
                    target_velocity: 1.5,
                    stiffness: 0.0,
                    damping: 50.0,
                    max_force: 0.0,
                }),
            },
            connected: slider,
            ..Joint::default()
        },
        JointNoContacts,
    ));

    steps(&mut app, 120);

    let v = velocity(&app, slider).linear;
    assert!(
        (v.x.abs() - 1.5).abs() < 0.3,
        "the motor drives the slider at 1.5 m/s along X: {v:?}"
    );
    assert!(
        v.y.abs() < 0.1 && v.z.abs() < 0.1,
        "only along the axis (gravity is held by the joint): {v:?}"
    );
    let at = position(&app, slider);
    assert!(
        (at.y - 2.0).abs() < 0.05 && at.z.abs() < 0.05,
        "the slider stays on its rail: {at}"
    );
}

/// specs/15 § 5 (ADR-1507): a non-finite local transform in a compound-collider chain is skipped
/// (that node composes as the identity) and counted, instead of moving the collider to NaN.
#[test]
fn a_non_finite_node_in_a_compound_chain_is_skipped_and_counted() {
    use regia_ecs::prelude::ChildOf;
    use regia_physics::{PhysicsWorld, QueryFilter, WorldQueries};

    let mut app = physics_app();
    let body = app
        .world_mut()
        .spawn((Transform::default(), RigidBody::static_body()))
        .id();
    let broken = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)),
            ChildOf(body),
        ))
        .id();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::splat(0.5),
            },
            ..Collider::default()
        },
        ChildOf(broken),
    ));
    steps(&mut app, 6);

    let physics = app.world().resource::<PhysicsWorld>();
    assert!(
        physics.stats().non_finite_inputs >= 1,
        "the skipped node is counted: {:?}",
        physics.stats()
    );
    let queries = WorldQueries {
        world: physics,
        layers: None,
    };
    let hit = queries
        .raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
        .expect("the collider exists at a finite pose");
    assert!(
        (hit.distance - 7.5).abs() < 1.0e-3,
        "the NaN node composed as the identity, so the box top is at y = 2.5: {}",
        hit.distance
    );
}
