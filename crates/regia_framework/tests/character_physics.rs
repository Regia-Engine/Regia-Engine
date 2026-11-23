// SPDX-License-Identifier: MPL-2.0
//! Character-controller acceptance scenarios on real rapier colliders (specs/15 § 6):
//! stairs, walkable vs un-walkable slopes, moving and rotating platforms (jitter under
//! 1 cm), and pushing dynamic bodies — all through the framework's movement state
//! machine with the `RapierCharacterQuery` backend installed.
//!
//! Provenance: the scenarios are the spec's own; the controller exercised is rapier3d's
//! public `KinematicCharacterController` (Apache-2.0) through `regia_physics`.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::CharacterPlugin;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterState, use_rapier_character_controller,
};
use regia_physics::{CharacterBody, CharacterGround, Collider, ColliderShape, RigidBody, Velocity};
use regia_scene::Transform;

/// One fixed step per update: manual 16 ms frame clock at 62.5 Hz.
const DT: f64 = 1.0 / 62.5;

/// A headless app with physics + the character plugin and the rapier backend installed.
fn character_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin);
    use_rapier_character_controller(&mut app);
    app
}

/// A static box collider body.
fn static_box(app: &mut App, half: Vec3, translation: Vec3, rotation: Quat) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform {
                translation,
                rotation,
                ..Default::default()
            },
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

/// A kinematic body with a box collider the tests move by writing its `Transform`.
fn kinematic_box(app: &mut App, half: Vec3, translation: Vec3) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

/// A dynamic box body (for pushing).
fn dynamic_box(app: &mut App, half: Vec3, translation: Vec3) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

/// Spawns a character resting at `start` and returns its entity.
fn spawn_character(app: &mut App, start: Vec3) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(start),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState::default(),
            CharacterBody::default(),
        ))
        .id()
}

/// Writes the character's input for the next steps.
fn set_input(app: &mut App, entity: regia_ecs::prelude::Entity, move_dir: Vec3) {
    if let Some(mut input) = app.world_mut().get_mut::<CharacterInput>(entity) {
        input.move_dir = move_dir;
        input.jump = false;
    }
}

fn translation_of(app: &App, entity: regia_ecs::prelude::Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

fn ground_of(app: &App, entity: regia_ecs::prelude::Entity) -> CharacterGround {
    app.world()
        .get::<CharacterBody>(entity)
        .expect("character body")
        .ground
}

/// Resting capsule-centre height for the default capsule (radius 0.3, half height 0.9).
const REST_Y: f32 = 1.2;

/// Lets the world settle: colliders appear on the first `FixedUpdate`, and the character
/// needs a few steps to discover the ground before scenario assertions begin.
fn settle(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.update();
    }
}

// ---------------------------------------------------------------------------
// Stairs (specs/15 § 6: reaches the top of the stairs)
// ---------------------------------------------------------------------------

/// Four 20 cm steps (within the 30 cm step height) are climbed without leaving the ground.
#[test]
fn character_walks_up_stairs() {
    let mut app = character_app();
    let _ground = static_box(
        &mut app,
        Vec3::new(20.0, 0.5, 5.0),
        Vec3::new(8.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    for step in 0..4u32 {
        let height = 0.2 * (step as f32 + 1.0);
        static_box(
            &mut app,
            Vec3::new(1.0, height / 2.0, 5.0),
            Vec3::new(step as f32 + 0.5, height / 2.0 - 0.0, 0.0) + Vec3::new(0.0, 0.0, 0.0),
            Quat::IDENTITY,
        );
    }
    let character = spawn_character(&mut app, Vec3::new(-2.0, REST_Y, 0.0));
    set_input(&mut app, character, Vec3::X);
    settle(&mut app, 20);

    let mut reached_top = false;
    for _ in 0..600 {
        app.update();
        let ground = ground_of(&app, character);
        let p = translation_of(&app, character);
        assert!(ground.grounded, "stairs are climbed without falling off: {p:?}");
        if p.x > 3.6 {
            // Past the last step's edge, resting on its top at 0.8 m.
            assert!(
                (p.y - (0.8 + REST_Y)).abs() < 0.1,
                "resting on the stair top: {p:?}"
            );
            assert!(
                ground.ground_entity != regia_ecs::prelude::Entity::PLACEHOLDER,
                "the ground entity is reported"
            );
            reached_top = true;
            break;
        }
    }
    assert!(reached_top, "the character reached the top of the stairs");
}

// ---------------------------------------------------------------------------
// Slopes (specs/15 § 6: climbs the walkable angle, does not climb 60°)
// ---------------------------------------------------------------------------

/// A ramp climbing +X at `angle_deg`, long enough to walk for several metres.
fn ramp(app: &mut App, angle_deg: f32, x_offset: f32, z_offset: f32) {
    let angle = angle_deg.to_radians();
    let length = 10.0;
    let center = Vec3::new(
        x_offset + (length / 2.0) * angle.cos(),
        (length / 2.0) * angle.sin(),
        z_offset,
    );
    static_box(
        app,
        Vec3::new(length / 2.0, 0.25, 2.0),
        center,
        Quat::from_rotation_z(angle),
    );
}

/// A 30° slope (under the 45° limit) is walked up: the height follows the ramp.
#[test]
fn character_climbs_a_walkable_slope() {
    let mut app = character_app();
    let _approach = static_box(
        &mut app,
        Vec3::new(4.0, 0.5, 2.0),
        Vec3::new(-2.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    ramp(&mut app, 30.0, 0.0, 0.0);
    let character = spawn_character(&mut app, Vec3::new(-1.0, REST_Y + 0.1, 0.0));
    set_input(&mut app, character, Vec3::X);
    settle(&mut app, 20);

    let mut climbed = false;
    for _ in 0..600 {
        app.update();
        let p = translation_of(&app, character);
        let ground = ground_of(&app, character);
        assert!(ground.grounded, "stays grounded on the ramp: {p:?}");
        if p.x > 4.0 {
            // The ramp's walkable surface is its box top: centre line + 0.25/cos(30°).
            let expected = p.x * 30.0_f32.to_radians().tan() + REST_Y + 0.25 / 30.0_f32.to_radians().cos();
            assert!(
                (p.y - expected).abs() < 0.15,
                "follows the 30° ramp: y={} expected≈{expected}",
                p.y
            );
            assert!(
                ground.ground_normal.y > 0.8,
                "the ground normal reports the ramp: {:?}",
                ground.ground_normal
            );
            climbed = true;
            break;
        }
    }
    assert!(climbed, "the character climbed the walkable slope");
}

/// A 60° slope (over the limit) is a wall: the character does not climb it.
#[test]
fn character_does_not_climb_a_steep_slope() {
    let mut app = character_app();
    let _approach = static_box(
        &mut app,
        Vec3::new(4.0, 0.5, 2.0),
        Vec3::new(-2.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    ramp(&mut app, 60.0, 0.0, 0.0);
    let character = spawn_character(&mut app, Vec3::new(-1.0, REST_Y + 0.1, 0.0));
    set_input(&mut app, character, Vec3::X);
    settle(&mut app, 30);

    for _ in 0..300 {
        app.update();
    }
    let p = translation_of(&app, character);
    assert!(p.x < 0.5, "the 60° slope blocks the walk: {p:?}");
    assert!(
        p.y < REST_Y + 0.6,
        "the character did not gain the slope's height: {p:?}"
    );
    let ground = ground_of(&app, character);
    assert!(
        ground.grounded,
        "the character stands on the approach floor, blocked by the wall"
    );
}

// ---------------------------------------------------------------------------
// Moving platform (specs/15 § 6: rides without jitter > 1 cm)
// ---------------------------------------------------------------------------

/// A character standing on a platform sliding at 1 m/s rides along with it.
#[test]
fn character_rides_a_moving_platform_without_jitter() {
    let mut app = character_app();
    let platform = kinematic_box(&mut app, Vec3::new(3.0, 0.25, 3.0), Vec3::new(0.0, 0.25, 0.0));
    let character = spawn_character(&mut app, Vec3::new(0.0, REST_Y + 0.5 + 0.5, 0.0));
    set_input(&mut app, character, Vec3::ZERO); // stand still
    settle(&mut app, 20);

    // Warm up: the first platform steps transiently lag one fixed step (the kinematic
    // velocity is only derived after the first move) — excluded from the jitter metric.
    for _ in 0..30 {
        {
            let mut t = app.world_mut().get_mut::<Transform>(platform).expect("t");
            t.translation.x += 1.0 * DT as f32;
        }
        app.update();
    }
    let mut offsets = Vec::new();
    for _ in 0..200 {
        // Slide the platform at 1 m/s by writing its Transform (kinematic drive).
        {
            let mut t = app.world_mut().get_mut::<Transform>(platform).expect("t");
            t.translation.x += 1.0 * DT as f32;
        }
        app.update();
        let char_x = translation_of(&app, character).x;
        let plat_x = app.world().get::<Transform>(platform).expect("t").translation.x;
        offsets.push(char_x - plat_x);
        let ground = ground_of(&app, character);
        assert!(
            ground.grounded,
            "stays on the platform: {:?}",
            translation_of(&app, character)
        );
    }

    let initial = offsets[0];
    let worst = offsets.iter().fold(0.0f32, |acc, o| acc.max((o - initial).abs()));
    assert!(
        worst <= 0.01,
        "rides the platform without jitter > 1 cm: worst deviation {worst:.4} m"
    );
    let plat_x = app.world().get::<Transform>(platform).expect("t").translation.x;
    let slid = 30.0 * DT as f32 + 200.0 * DT as f32;
    assert!(
        (plat_x - slid).abs() < 0.05,
        "the platform slid {slid} m: {plat_x}"
    );
    let char_x = translation_of(&app, character).x;
    assert!(
        (char_x - plat_x).abs() < 0.2,
        "the character traveled with the platform: char {char_x} platform {plat_x}"
    );
}

// ---------------------------------------------------------------------------
// Rotating platform (specs/15 § 4: platform yaw through ground point velocity)
// ---------------------------------------------------------------------------

/// A character standing off-centre on a platform rotating around Y is carried around
/// with it (ground velocity ω × r, specs/15 § 4 rotating platforms).
#[test]
fn character_rides_a_rotating_platform() {
    let mut app = character_app();
    let platform = kinematic_box(&mut app, Vec3::new(3.0, 0.25, 3.0), Vec3::new(0.0, 0.25, 0.0));
    let character = spawn_character(&mut app, Vec3::new(0.0, REST_Y + 0.5 + 0.5, 2.0));
    set_input(&mut app, character, Vec3::ZERO); // stand still
    settle(&mut app, 20);

    let start = translation_of(&app, character);
    let mut angle = 0.0f32;
    let omega = 1.0f32; // rad/s
    for _ in 0..125 {
        angle += omega * DT as f32;
        {
            let mut t = app.world_mut().get_mut::<Transform>(platform).expect("t");
            t.rotation = Quat::from_rotation_y(angle);
        }
        app.update();
        let ground = ground_of(&app, character);
        assert!(ground.grounded, "stays on the rotating platform");
    }

    let end = translation_of(&app, character);
    let swept = (Vec3::new(start.x, 0.0, start.z) - Vec3::new(end.x, 0.0, end.z)).length();
    assert!(
        swept > 0.3,
        "the platform's rotation carried the character tangentially: moved {swept:.3} m ({start:?} → {end:?})"
    );
    let radius_start = Vec3::new(start.x, 0.0, start.z).length();
    let radius_end = Vec3::new(end.x, 0.0, end.z).length();
    assert!(
        (radius_start - radius_end).abs() < 0.1,
        "the character orbits instead of drifting off: r {radius_start:.3} → {radius_end:.3}"
    );
}

// ---------------------------------------------------------------------------
// Pushing (specs/15 § 4: dynamic bodies get impulses ∝ character mass)
// ---------------------------------------------------------------------------

/// Walking into a dynamic box pushes it ahead of the character.
#[test]
fn character_pushes_a_dynamic_box() {
    let mut app = character_app();
    let _ground = static_box(
        &mut app,
        Vec3::new(20.0, 0.5, 5.0),
        Vec3::new(0.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    let box_start = 2.0;
    let box_e = dynamic_box(&mut app, Vec3::splat(0.4), Vec3::new(box_start, 0.4 + 0.05, 0.0));
    let character = spawn_character(&mut app, Vec3::new(-2.0, REST_Y, 0.0));
    set_input(&mut app, character, Vec3::X);
    settle(&mut app, 10);

    for _ in 0..300 {
        app.update();
    }
    let box_x = translation_of(&app, box_e).x;
    let char_x = translation_of(&app, character).x;
    assert!(
        box_x > box_start + 0.8,
        "the box was pushed well ahead: {box_x} (start {box_start})"
    );
    assert!(
        char_x > box_start + 0.4,
        "the character walked up to (and past) the box: {char_x}"
    );
    assert!(
        ground_of(&app, character).grounded,
        "pushing keeps the character grounded"
    );
}

/// Walking on a kinematic platform: rapier's platform friction used to undo the nudge that frees a
/// capsule resting inside its skin width, and the character stood stuck after a few steps, at
/// rest or on a moving platform (ADR-1905).
#[test]
fn character_walks_across_a_kinematic_platform() {
    for platform_speed in [0.0, 1.0] {
        let mut app = character_app();
        let platform = kinematic_box(&mut app, Vec3::new(8.0, 0.25, 8.0), Vec3::new(0.0, 0.25, 0.0));
        let character = spawn_character(&mut app, Vec3::new(-6.0, 0.5 + 1.25, 0.0));
        for _ in 0..30 {
            app.update();
        }
        let start = app.world().get::<Transform>(character).expect("t").translation;
        set_input(&mut app, character, Vec3::X);
        for _ in 0..125 {
            if let Some(mut t) = app.world_mut().get_mut::<Transform>(platform) {
                t.translation.x += platform_speed * DT as f32;
            }
            app.update();
        }
        let walked = app.world().get::<Transform>(character).expect("t").translation.x - start.x;
        // Two seconds at up to 6 m/s, less the start of the run, plus the platform's own motion.
        assert!(walked > 9.0, "platform speed {platform_speed}: walked {walked}");
    }
}
