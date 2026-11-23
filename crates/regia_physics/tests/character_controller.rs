// SPDX-License-Identifier: MPL-2.0
//! Kinematic character controller simulation tests (specs/15 § 4, § 6): ground report,
//! wall blocking, autostep up a low stair, refusal of a stair taller than `step_height`,
//! ceiling detection and pushing a dynamic box.
//!
//! Provenance: scenarios are elementary geometry built here; the sweep itself is rapier3d's
//! `KinematicCharacterController` (Apache-2.0).

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    BodyKind, CharacterBody, CharacterMove, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody,
    Velocity, move_character,
};
use regia_scene::Transform;

const RADIUS: f32 = 0.3;
const HALF_HEIGHT: f32 = 0.6;
const DT: f32 = 1.0 / 60.0;
/// Capsule centre height when resting on a y=0 surface (plus the default skin width).
const REST_Y: f32 = HALF_HEIGHT + RADIUS + 0.02;

/// One step per update: manual 1/60 frame clock at a 60 Hz fixed rate.
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

/// A static box with the given half extents and centre.
fn static_box(app: &mut App, half: Vec3, centre: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(centre),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

/// A wide ground plate whose top surface is y=0.
fn ground(app: &mut App) -> Entity {
    static_box(app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0))
}

/// Runs a few updates so the bodies and colliders reach the rapier world.
fn settle(app: &mut App) {
    for _ in 0..5 {
        app.update();
    }
}

/// One controller move with the default character settings.
fn step_character(app: &mut App, pos: Vec3, desired: Vec3, step_height: f32, snap: f32) -> CharacterMove {
    let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
    move_character(
        &mut physics,
        &CharacterBody::default(),
        RADIUS,
        HALF_HEIGHT,
        (pos, Quat::IDENTITY),
        desired,
        50.0_f32.to_radians(),
        step_height,
        snap,
        None,
        DT,
        DT,
    )
}

/// Walks `steps` moves of `forward` metres along +x (with a small downward pull) and returns
/// the final centre position.
fn walk(app: &mut App, start: Vec3, forward: f32, steps: usize, step_height: f32) -> Vec3 {
    let mut pos = start;
    for _ in 0..steps {
        let result = step_character(app, pos, Vec3::new(forward, -0.05, 0.0), step_height, 0.2);
        pos += result.translation;
    }
    pos
}

#[test]
fn standing_on_flat_ground_reports_ground_entity_and_up_normal() {
    let mut app = physics_app();
    let floor = ground(&mut app);
    settle(&mut app);

    let result = step_character(&mut app, Vec3::new(0.0, REST_Y, 0.0), Vec3::ZERO, 0.3, 0.2);
    assert!(result.grounded, "a character resting on the plate is grounded");
    assert_eq!(result.ground_entity, floor);
    assert!(
        result.ground_normal.dot(Vec3::Y) > 0.99,
        "{:?}",
        result.ground_normal
    );
    assert!(!result.ceiling_hit);
    assert!(!result.sliding);
}

#[test]
fn free_fall_is_not_grounded() {
    let mut app = physics_app();
    ground(&mut app);
    settle(&mut app);

    let result = step_character(
        &mut app,
        Vec3::new(0.0, 10.0, 0.0),
        Vec3::new(0.0, -0.1, 0.0),
        0.3,
        0.0,
    );
    assert!(!result.grounded);
    assert_eq!(result.ground_entity, Entity::PLACEHOLDER);
    assert!(result.hits.is_empty());
    assert!(
        (result.translation.y + 0.1).abs() < 1.0e-4,
        "{:?}",
        result.translation
    );
}

#[test]
fn a_wall_stops_forward_motion_and_is_reported_as_a_hit() {
    let mut app = physics_app();
    ground(&mut app);
    let wall = static_box(&mut app, Vec3::new(0.5, 2.0, 5.0), Vec3::new(2.0, 2.0, 0.0));
    settle(&mut app);

    let end = walk(&mut app, Vec3::new(0.0, REST_Y, 0.0), 0.1, 60, 0.3);
    // The wall face is at x = 1.5; the capsule centre must stay one radius (plus skin) short.
    assert!(end.x < 1.5 - RADIUS + 0.01, "passed through the wall: {end:?}");
    assert!(end.x > 0.5, "should have walked towards the wall: {end:?}");

    let result = step_character(&mut app, end, Vec3::new(0.3, -0.05, 0.0), 0.3, 0.2);
    assert!(result.hits.contains(&wall), "the wall is among the hits");
}

#[test]
fn autostep_climbs_a_stair_lower_than_step_height() {
    let mut app = physics_app();
    ground(&mut app);
    // A 0.2 m stair: top at y = 0.2, face at x = 0.5.
    static_box(&mut app, Vec3::new(1.0, 0.1, 3.0), Vec3::new(1.5, 0.1, 0.0));
    settle(&mut app);

    let end = walk(&mut app, Vec3::new(0.0, REST_Y, 0.0), 0.05, 50, 0.3);
    assert!(end.x > 0.9, "should be on top of the stair: {end:?}");
    assert!(
        (end.y - (REST_Y + 0.2)).abs() < 0.06,
        "centre should have risen by the 0.2 m stair: {end:?}"
    );
}

/// A slow walk (2 cm per step, 1.2 m/s) climbs the same stair: each step advances far less
/// than the capsule radius past the stair face, which rapier 0.36's autostep alone cannot
/// climb (ADR-1387).
#[test]
fn autostep_climbs_a_stair_at_a_slow_walk() {
    let mut app = physics_app();
    ground(&mut app);
    static_box(&mut app, Vec3::new(1.0, 0.1, 3.0), Vec3::new(1.5, 0.1, 0.0));
    settle(&mut app);

    let end = walk(&mut app, Vec3::new(0.0, REST_Y, 0.0), 0.02, 100, 0.3);
    assert!(end.x > 0.9, "should be on top of the stair: {end:?}");
    assert!(
        (end.y - (REST_Y + 0.2)).abs() < 0.06,
        "centre should have risen by the 0.2 m stair: {end:?}"
    );
}

/// A stair with a wall right behind its edge offers no room to stand: the character stays
/// below it (the step needs `autostep_min_width` of free space on top).
#[test]
fn a_stair_without_room_on_top_is_not_climbed() {
    let mut app = physics_app();
    ground(&mut app);
    // A 0.2 m stair from x = 0.5 to 0.6, then a 3 m wall from x = 0.6.
    static_box(&mut app, Vec3::new(0.05, 0.1, 3.0), Vec3::new(0.55, 0.1, 0.0));
    static_box(&mut app, Vec3::new(0.5, 1.5, 3.0), Vec3::new(1.1, 1.5, 0.0));
    settle(&mut app);

    let end = walk(&mut app, Vec3::new(0.0, REST_Y, 0.0), 0.05, 50, 0.3);
    assert!(end.x < 0.5, "must stay in front of the stair: {end:?}");
    assert!(end.y < REST_Y + 0.1, "must not have climbed: {end:?}");
}

#[test]
fn a_stair_taller_than_step_height_blocks_the_character() {
    let mut app = physics_app();
    ground(&mut app);
    // A 0.8 m step: top at y = 0.8, face at x = 0.5.
    static_box(&mut app, Vec3::new(1.0, 0.4, 3.0), Vec3::new(1.5, 0.4, 0.0));
    settle(&mut app);

    let end = walk(&mut app, Vec3::new(0.0, REST_Y, 0.0), 0.05, 50, 0.3);
    assert!(
        end.x < 0.5,
        "must be stopped by the face of the tall step: {end:?}"
    );
    assert!(end.y < REST_Y + 0.1, "must not have climbed: {end:?}");
}

#[test]
fn a_ceiling_above_the_head_sets_ceiling_hit() {
    let mut app = physics_app();
    ground(&mut app);
    // A slab whose underside is at y = 1.9; the capsule top rests at REST_Y + 0.9 = 1.82.
    static_box(&mut app, Vec3::new(5.0, 0.5, 5.0), Vec3::new(0.0, 2.4, 0.0));
    settle(&mut app);

    let result = step_character(
        &mut app,
        Vec3::new(0.0, REST_Y, 0.0),
        Vec3::new(0.0, 0.3, 0.0),
        0.0,
        0.0,
    );
    assert!(result.ceiling_hit, "head touched the slab");
    assert!(
        result.translation.y < 0.15,
        "upward motion is cut short: {:?}",
        result.translation
    );
}

#[test]
fn walking_into_a_dynamic_box_pushes_it() {
    let mut app = physics_app();
    ground(&mut app);
    let crate_e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.9, 0.5, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                friction: 0.0,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    settle(&mut app);

    let start_x = 0.0;
    let desired = 0.3;
    let result = step_character(
        &mut app,
        Vec3::new(start_x, REST_Y, 0.0),
        Vec3::new(desired, 0.0, 0.0),
        0.0,
        0.0,
    );
    assert!(result.hits.contains(&crate_e), "the crate is among the hits");
    assert!(
        result.translation.x < desired,
        "the crate blocks part of the move"
    );

    app.update();
    let velocity = app.world().get::<Velocity>(crate_e).cloned().unwrap_or_default();
    assert!(
        velocity.linear.x > 0.0,
        "the push impulse moves the crate: {velocity:?}"
    );
}

/// Audit g-physics2 finding 6: the platform velocity at the character turns about the platform's
/// center of mass (rapier's `linvel` is the center-of-mass velocity), not about the body origin.
/// A kinematic platform whose collider sits 1 m off its origin, its center of mass sliding at
/// 0.5 m/s along +X while it spins at 1 rad/s about Y, carries a character standing over its
/// origin at `v + ω × (p − com)` = (0.5, 0, 1) m/s; the old lever arm from the body origin gave
/// (0.5, 0, 0).
#[test]
fn a_spinning_platform_carries_the_character_about_its_center_of_mass() {
    use regia_physics::RigidBodyHandle;
    let mut app = physics_app();
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 0.1, 2.0),
                },
                offset: Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id();
    settle(&mut app);
    let handle = app
        .world()
        .get::<RigidBodyHandle>(platform)
        .expect("platform body")
        .to_handle();
    {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        let body = physics.bodies.get_mut(handle).expect("platform body");
        let com = body.center_of_mass();
        assert!(
            (com.x - 1.0).abs() < 1.0e-4 && com.z.abs() < 1.0e-4,
            "the platform's center of mass sits under its collider: {com:?}"
        );
        // Velocity-based for the test so the velocities can be set directly (a position-based
        // kinematic body ignores `set_linvel`); rapier stores the center-of-mass velocity either way.
        body.set_body_type(rapier3d::dynamics::RigidBodyType::KinematicVelocityBased, false);
        body.set_linvel(rapier3d::math::Vector::new(0.5, 0.0, 0.0), false);
        body.set_angvel(rapier3d::math::Vector::new(0.0, 1.0, 0.0), false);
    }
    let result = step_character(&mut app, Vec3::new(0.0, REST_Y, 0.0), Vec3::ZERO, 0.3, 0.2);
    assert_eq!(result.ground_entity, platform);
    let expected = Vec3::new(0.5, 0.0, 1.0);
    assert!(
        (result.ground_velocity - expected).length() < 1.0e-3,
        "ground velocity {:?}, expected {expected:?}",
        result.ground_velocity
    );
}

#[test]
fn a_rotating_platform_carries_the_character_about_the_center_of_mass() {
    // The box is centered 1 m to the side of the body origin, so the center of mass is not the
    // origin. The character stands on the vertical axis through that center. Angular velocity
    // about Y is then zero at the character; measuring r from the body origin would slide it.
    let mut app = physics_app();
    let platform = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(1.0, 0.5, 1.0),
                },
                offset: Transform::from_translation(Vec3::X),
                ..Collider::default()
            },
        ))
        .id();
    settle(&mut app);

    let handle = app
        .world()
        .get::<regia_physics::RigidBodyHandle>(platform)
        .expect("body")
        .to_handle();
    {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        let body = physics.bodies.get_mut(handle).expect("rapier body");
        let com = regia_physics::convert::from_rapier_vec3(body.center_of_mass());
        assert!((com - Vec3::X).length() < 1.0e-3, "center of mass {com:?}");
        body.set_linvel(regia_physics::convert::to_rapier_vec3(Vec3::ZERO), true);
        body.set_angvel(regia_physics::convert::to_rapier_vec3(Vec3::Y), true);
    }

    let pos = Vec3::new(1.0, 0.5 + HALF_HEIGHT + RADIUS + 0.02, 0.0);
    let result = step_character(&mut app, pos, Vec3::ZERO, 0.3, 0.2);
    assert!(result.grounded, "stands on the offset box");
    assert_eq!(result.ground_entity, platform);
    assert!(
        result.ground_velocity.length() < 1.0e-3,
        "velocity at the center of rotation is zero, got {}",
        result.ground_velocity
    );
    assert!(
        result.translation.x.abs() < 1.0e-4 && result.translation.z.abs() < 1.0e-4,
        "one step does not slide the character, got {}",
        result.translation
    );
}

/// One anchored controller move with the default character settings: the move
/// `NetCharacterPlugin`'s backend makes for a rider its caller already put on its base.
fn step_anchored(app: &mut App, pos: Vec3, desired: Vec3) -> CharacterMove {
    let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
    regia_physics::character::move_anchored_character(
        &mut physics,
        &CharacterBody::default(),
        RADIUS,
        HALF_HEIGHT,
        (pos, Quat::IDENTITY),
        desired,
        50.0_f32.to_radians(),
        0.0,
        0.2,
        None,
        DT,
        DT,
    )
}

/// An anchored move into a wall, on a deck that has moved at `velocity` for ten steps. The deck is
/// `body`: a kinematic one follows its `Transform` (rapier derives its velocity from each move), a
/// dynamic one is given the velocity, a static one stands still. The wall is static, runs along X
/// and starts above the deck; its face is 1 cm past the capsule's skin, so the move's cast stops
/// there. Returns the move and the wall.
fn anchored_move_into_a_wall(body: RigidBody, velocity: Vec3) -> (CharacterMove, Entity) {
    let kind = body.kind;
    let mut app = physics_app();
    let deck = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
            body,
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.1, 6.0),
                },
                friction: 0.8,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    let face = RADIUS + 0.02 + 0.01;
    let wall = static_box(
        &mut app,
        Vec3::new(6.0, 1.5, 0.25),
        Vec3::new(0.0, 2.0, face + 0.25),
    );
    settle(&mut app);
    for _ in 0..10 {
        match kind {
            BodyKind::Kinematic => {
                if let Some(mut t) = app.world_mut().get_mut::<Transform>(deck) {
                    t.translation += velocity * DT;
                }
            }
            BodyKind::Dynamic => {
                if let Some(mut v) = app.world_mut().get_mut::<Velocity>(deck) {
                    v.linear = velocity;
                    v.angular = Vec3::ZERO;
                }
            }
            BodyKind::Static => {}
        }
        app.update();
    }
    let top = 10.0 * velocity.y * DT;
    let result = step_anchored(
        &mut app,
        Vec3::new(0.0, top + REST_Y, 0.0),
        Vec3::new(0.0, 0.0, 0.1),
    );
    (result, wall)
}

/// Spec 73 § 6.6, ADR-5972: the caller of an anchored move has already moved the character with
/// its base, so the move must not carry it with the base again. rapier's controller carries a
/// character with kinematic ground on its own, in the iterations after its cast stopped at a
/// contact; an anchored move that stops at a wall used to gain the deck's motion of one step
/// (5 cm at 3 m/s) on top of the anchor. The move over a moving deck is the move over the same
/// deck standing still.
#[test]
fn an_anchored_move_that_stops_at_a_wall_is_not_carried_by_kinematic_ground() {
    let (still, wall) = anchored_move_into_a_wall(RigidBody::kinematic(), Vec3::ZERO);
    assert!(
        still.hits.contains(&wall),
        "the move stops at the wall: {still:?}"
    );
    assert!(still.grounded, "{still:?}");
    assert!(
        (still.translation.z - 0.01).abs() < 2.0e-3 && still.translation.x.abs() < 1.0e-4,
        "one centimeter to the wall, none along it: {:?}",
        still.translation
    );
    for velocity in [
        Vec3::new(3.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(-2.0, 0.5, 0.0),
    ] {
        let (moving, wall) = anchored_move_into_a_wall(RigidBody::kinematic(), velocity);
        assert!(
            moving.hits.contains(&wall),
            "deck velocity {velocity}: the move stops at the wall: {moving:?}"
        );
        assert!(
            (moving.ground_velocity - velocity).length() < 1.0e-2,
            "deck velocity {velocity}: the deck moves under the character: {:?}",
            moving.ground_velocity
        );
        assert!(
            (moving.translation - still.translation).length() < 1.0e-3,
            "deck velocity {velocity}: the move went {:?}, on the deck standing still {:?}",
            moving.translation,
            still.translation
        );
    }
}

/// Spec 73 § 6.6, ADR-5972: the server simulates a base as a dynamic body, a client follows it
/// with a kinematic proxy, and both step the rider with an anchored move; the prediction is only
/// right if the two moves agree. The same anchored move into a wall over the same deck as a static
/// body, as a kinematic body and as a weightless dynamic body, standing still or moving, agrees
/// within a millimeter. Before the fix the move over the moving kinematic deck alone gained the
/// deck's motion of one step.
#[test]
fn an_anchored_move_is_the_same_over_a_proxy_deck_and_over_a_simulated_one() {
    let weightless = || RigidBody {
        gravity_scale: 0.0,
        ..RigidBody::dynamic()
    };
    let (on_static, wall) = anchored_move_into_a_wall(RigidBody::static_body(), Vec3::ZERO);
    assert!(
        on_static.hits.contains(&wall) && on_static.grounded,
        "the move stops at the wall: {on_static:?}"
    );
    for velocity in [
        Vec3::ZERO,
        Vec3::new(3.0, 0.0, 0.0),
        Vec3::new(-2.0, 0.0, 4.0),
        Vec3::new(1.0, 0.5, 0.0),
    ] {
        let (proxy, _) = anchored_move_into_a_wall(RigidBody::kinematic(), velocity);
        let (simulated, _) = anchored_move_into_a_wall(weightless(), velocity);
        for (name, moved) in [("kinematic", &proxy), ("dynamic", &simulated)] {
            assert!(
                (moved.ground_velocity - velocity).length() < 1.0e-2,
                "deck velocity {velocity}: the {name} deck moves under the character: {:?}",
                moved.ground_velocity
            );
            assert!(
                (moved.translation - on_static.translation).length() < 1.0e-3,
                "deck velocity {velocity}: the move over the {name} deck went {:?}, over the static deck {:?}",
                moved.translation,
                on_static.translation
            );
        }
        assert!(
            (proxy.translation - simulated.translation).length() < 1.0e-3,
            "deck velocity {velocity}: over the kinematic deck {:?}, over the dynamic deck {:?}",
            proxy.translation,
            simulated.translation
        );
    }
}

/// What a character does while it rides in [`ride_a_kinematic_deck`].
#[derive(Clone, Copy, Debug, PartialEq)]
enum Riding {
    /// Stands still.
    Standing,
    /// Walks 5 cm a step along X, turning every 40 steps.
    Pacing,
    /// Pushes 5 cm a step into a railing that moves with the deck.
    Pressed,
}

/// The worst errors of one ride in [`ride_a_kinematic_deck`].
#[derive(Debug)]
struct Ride {
    /// Largest horizontal distance from the place on the deck the character should be at (m).
    place: f32,
    /// Largest distance from its resting height above the deck (m).
    height: f32,
    /// Steps whose move was not grounded.
    airborne: usize,
}

/// The framework's loop on a kinematic deck that moves at `velocity`, with a railing that moves
/// with it (inner face at z = 1 from `origin`, where the middle of the deck's top starts): every
/// step the deck and the railing move, physics steps, and the character makes one carried move.
fn ride_a_kinematic_deck(origin: Vec3, velocity: Vec3, riding: Riding, steps: usize) -> Ride {
    let mut app = physics_app();
    let mut spawn_part = |half: Vec3, centre: Vec3| {
        app.world_mut()
            .spawn((
                Transform::from_translation(origin + centre),
                RigidBody::kinematic(),
                Collider {
                    shape: ColliderShape::Box { half_extents: half },
                    friction: 0.8,
                    ..Collider::default()
                },
            ))
            .id()
    };
    let deck = spawn_part(Vec3::new(8.0, 0.5, 8.0), Vec3::new(0.0, -0.5, 0.0));
    // The railing's center is off the deck's along X as well, so far from the origin the two
    // positions round differently when gameplay moves them by the same amount.
    let railing = spawn_part(Vec3::new(8.0, 0.75, 0.25), Vec3::new(0.37, 0.05 + 0.75, 1.25));
    settle(&mut app);
    let start_z = if riding == Riding::Pressed {
        1.0 - RADIUS - 0.02 - 0.001
    } else {
        0.0
    };
    let mut pos = origin + Vec3::new(0.0, REST_Y, start_z);
    let mut walked = 0.0;
    let mut ride = Ride {
        place: 0.0,
        height: 0.0,
        airborne: 0,
    };
    for step in 0..steps {
        for part in [deck, railing] {
            if let Some(mut t) = app.world_mut().get_mut::<Transform>(part) {
                t.translation += velocity * DT;
            }
        }
        app.update();
        let own = match riding {
            Riding::Standing => Vec3::ZERO,
            Riding::Pacing if (step / 40).is_multiple_of(2) => Vec3::new(0.05, 0.0, 0.0),
            Riding::Pacing => Vec3::new(-0.05, 0.0, 0.0),
            Riding::Pressed => Vec3::new(0.0, 0.0, 0.05),
        };
        let moved = step_character(&mut app, pos, own, 0.0, 0.2);
        pos += moved.translation;
        walked += own.x;
        let top = app
            .world()
            .get::<Transform>(deck)
            .map_or(Vec3::ZERO, |t| t.translation + Vec3::new(0.0, 0.5, 0.0));
        // The place on the deck, measured from where the middle of its top is now.
        let on_deck = pos - top;
        // A pressed character rests where the railing's skin leaves it: only its place along
        // the railing is known beforehand.
        let across = if riding == Riding::Pressed {
            0.0
        } else {
            on_deck.z - start_z
        };
        ride.place = ride
            .place
            .max(Vec3::new(on_deck.x - walked, 0.0, across).length());
        ride.height = ride.height.max((on_deck.y - REST_Y).abs());
        if !moved.grounded {
            ride.airborne += 1;
        }
    }
    ride
}

/// Spec 15 § 4, ADR-5972: a moving kinematic platform carries a character once, whatever the
/// character's sweep touches. The carried move used to add the platform's motion to the desired
/// move, and rapier's controller added it again after every contact: a character pressed against
/// a railing slid along it at the platform's speed, one on a sinking platform (whose every sweep
/// ends at the deck) drifted ahead, and one pressed against the railing of a rising platform
/// counted as airborne. Standing, pacing and pressed against the railing, on decks that slide,
/// rise and sink, the character keeps its place and its height on the deck and stays grounded.
#[test]
fn a_character_keeps_its_place_on_a_moving_kinematic_deck() {
    for velocity in [
        Vec3::new(3.0, 0.0, 0.0),
        Vec3::new(-2.0, 0.0, 4.0),
        Vec3::new(2.0, 1.0, 0.0),
        Vec3::new(2.0, -1.0, 0.0),
        Vec3::new(0.0, 1.5, 0.0),
        Vec3::new(0.0, -1.5, 0.0),
        Vec3::new(1.0, 2.5, 0.0),
        Vec3::new(1.0, -2.5, 0.0),
    ] {
        for riding in [Riding::Standing, Riding::Pacing, Riding::Pressed] {
            let ride = ride_a_kinematic_deck(Vec3::ZERO, velocity, riding, 240);
            // A pacing character may lose a millimeter of its own step where its cast grazes
            // the deck; a second carry is 1.7 to 6.7 cm a step at these speeds.
            let place = if riding == Riding::Pacing { 0.02 } else { 0.005 };
            assert!(
                ride.place < place && ride.height < 0.005 && ride.airborne == 0,
                "deck velocity {velocity}, {riding:?}: {ride:?}"
            );
        }
    }
}

/// ADR-5972 (2): what moves with the ground is told by velocity, and a kinematic body's velocity
/// is its last move divided by the step. Three kilometers from the origin a coordinate has a
/// quarter of a millimeter of resolution, so a slow deck and the railing gameplay moves with it
/// can have velocities that differ by percents, and the move allows for that rounding when it
/// decides what moves with the deck. A guard that the ride holds there too. It does not show
/// that the allowance is needed: the ride held as well with the allowance taken out.
#[test]
fn a_character_keeps_its_place_on_a_slow_deck_far_from_the_origin() {
    let origin = Vec3::new(3000.0, 0.0, -2000.0);
    for velocity in [Vec3::new(0.3, 0.0, 0.0), Vec3::new(0.3, 0.0, 0.2)] {
        for riding in [Riding::Standing, Riding::Pacing, Riding::Pressed] {
            let ride = ride_a_kinematic_deck(origin, velocity, riding, 240);
            // Positions themselves round to a quarter of a millimeter here.
            assert!(
                ride.place < 0.02 && ride.height < 0.005 && ride.airborne == 0,
                "deck velocity {velocity}, {riding:?}: {ride:?}"
            );
        }
    }
}

/// Spec 15 § 4, ADR-5972: one carried move of a character pressed against a static wall, on a
/// kinematic deck that slides along the wall, is the move on the deck standing still plus the
/// deck's motion of one step, once. It used to be twice (10 cm instead of 5 cm at 3 m/s).
#[test]
fn a_carried_move_that_stops_at_a_wall_is_carried_once_by_kinematic_ground() {
    let carried_into_a_wall = |velocity: Vec3| {
        let mut app = physics_app();
        let deck = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
                RigidBody::kinematic(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(6.0, 0.1, 6.0),
                    },
                    friction: 0.8,
                    ..Collider::default()
                },
            ))
            .id();
        let face = RADIUS + 0.02 + 0.01;
        let wall = static_box(
            &mut app,
            Vec3::new(6.0, 1.5, 0.25),
            Vec3::new(0.0, 2.0, face + 0.25),
        );
        settle(&mut app);
        for _ in 0..10 {
            if let Some(mut t) = app.world_mut().get_mut::<Transform>(deck) {
                t.translation += velocity * DT;
            }
            app.update();
        }
        let moved = step_character(
            &mut app,
            Vec3::new(0.0, REST_Y, 0.0),
            Vec3::new(0.0, 0.0, 0.1),
            0.0,
            0.2,
        );
        assert!(
            moved.hits.contains(&wall) && moved.grounded,
            "deck velocity {velocity}: the move stops at the wall: {moved:?}"
        );
        moved.translation
    };
    let still = carried_into_a_wall(Vec3::ZERO);
    for velocity in [Vec3::new(3.0, 0.0, 0.0), Vec3::new(-2.0, 0.0, 0.0)] {
        let moving = carried_into_a_wall(velocity);
        assert!(
            (moving - (still + velocity * DT)).length() < 1.0e-3,
            "deck velocity {velocity}: the move went {moving:?}, on the deck standing still {still:?}"
        );
    }
}

/// Spec 15 § 4: a platform does not carry a character through a wall that does not move with
/// it. A kinematic deck slides under a static wall; the character on it stops at the wall and
/// stays on the deck, which goes on under it.
#[test]
fn a_kinematic_deck_does_not_carry_a_character_through_a_static_wall() {
    let mut app = physics_app();
    let deck = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.1, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(8.0, 0.1, 8.0),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id();
    // The wall's face is at z = 1 and it starts above the deck.
    let wall = static_box(&mut app, Vec3::new(8.0, 1.5, 0.25), Vec3::new(0.0, 2.0, 1.25));
    settle(&mut app);
    let mut pos = Vec3::new(0.0, REST_Y, 0.0);
    let mut stopped_at_the_wall = false;
    for _ in 0..120 {
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(deck) {
            t.translation.z += 2.0 * DT;
        }
        app.update();
        let moved = step_character(&mut app, pos, Vec3::ZERO, 0.0, 0.2);
        pos += moved.translation;
        stopped_at_the_wall |= moved.hits.contains(&wall);
        assert!(moved.grounded, "on the deck at {pos}");
        assert!(
            pos.z < 1.0 - RADIUS - 0.01,
            "the capsule stays in front of the wall's face: {pos}"
        );
    }
    assert!(
        stopped_at_the_wall,
        "the deck carried the character to the wall: {pos}"
    );
    // Sliding along the wall's skin may move it a millimeter or two sideways.
    assert!(
        pos.z > 1.0 - RADIUS - 0.05 && pos.x.abs() < 0.01,
        "it rests against the wall: {pos}"
    );
    assert!((pos.y - REST_Y).abs() < 0.005, "at its height on the deck: {pos}");
}
