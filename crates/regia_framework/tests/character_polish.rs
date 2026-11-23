// SPDX-License-Identifier: MPL-2.0
//! Character controller polish (spec 93 § 2.2, ADR-1693) on real rapier colliders: crouch
//! shrinks the capsule (passes under a bar, refuses to stand under it), a ceiling stops the
//! jump, a jump from a moving platform keeps the platform's speed, the reported velocity is
//! the motion achieved (a wall reads as standing, open ground keeps full speed), and a limp
//! ragdoll character ignores its input.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    use_rapier_character_controller,
};
use regia_framework::ragdoll::{Ragdoll, RagdollMode, RagdollProfile};
use regia_physics::{CharacterBody, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

const DT: f64 = 1.0 / 62.5;
/// Resting capsule-centre height for the default capsule (radius 0.3, half height 0.9).
const REST_Y: f32 = 1.2;

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

fn static_box(app: &mut App, half: Vec3, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform {
                translation,
                rotation: Quat::IDENTITY,
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

/// A floor whose top face is y = 0.
fn floor(app: &mut App) {
    static_box(app, Vec3::new(30.0, 0.5, 30.0), Vec3::new(0.0, -0.5, 0.0));
}

fn spawn_character(app: &mut App, start: Vec3) -> Entity {
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

fn input(app: &mut App, e: Entity, f: impl FnOnce(&mut CharacterInput)) {
    f(&mut app.world_mut().get_mut::<CharacterInput>(e).expect("input"));
}

fn position(app: &App, e: Entity) -> Vec3 {
    app.world().get::<Transform>(e).expect("transform").translation
}

fn state(app: &App, e: Entity) -> CharacterState {
    app.world().get::<CharacterState>(e).cloned().expect("state")
}

fn run(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.update();
    }
}

/// A bar whose underside is 2.0 m up, from x = 2 to x = 6: a standing capsule (2.4 m) cannot
/// pass, a crouched one (1.6 m) can.
fn low_bar(app: &mut App) {
    static_box(app, Vec3::new(2.0, 0.5, 10.0), Vec3::new(4.0, 2.5, 0.0));
}

#[test]
fn standing_character_is_stopped_by_a_low_bar() {
    let mut app = character_app();
    floor(&mut app);
    low_bar(&mut app);
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    run(&mut app, 20);
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 120);
    let p = position(&app, c);
    assert!(p.x < 2.0, "the bar stops a standing capsule: {p}");
    assert!(!state(&app, c).crouched);
}

#[test]
fn crouch_passes_under_the_bar_and_stands_only_when_clear() {
    let mut app = character_app();
    floor(&mut app);
    low_bar(&mut app);
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    run(&mut app, 20);
    input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.crouch = true;
    });
    // Crouch speed is 3 m/s: x = 4 (mid-bar) in well under 2 s.
    let mut steps = 0;
    while position(&app, c).x < 4.0 && steps < 150 {
        app.update();
        steps += 1;
    }
    let p = position(&app, c);
    assert!(p.x >= 4.0, "a crouched capsule passes under the bar: {p}");
    assert!(state(&app, c).crouched);
    assert!(
        (p.y - REST_Y).abs() < 0.05,
        "the transform stays at the standing centre: {p}"
    );
    // Release crouch under the bar: no headroom, it stays crouched (and keeps moving).
    input(&mut app, c, |i| {
        i.crouch = false;
        i.move_dir = Vec3::ZERO;
    });
    run(&mut app, 10);
    assert!(state(&app, c).crouched, "no room to stand under the bar");
    // Walk out from under it: it stands up once clear.
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut steps = 0;
    while state(&app, c).crouched && steps < 200 {
        app.update();
        steps += 1;
    }
    let p = position(&app, c);
    assert!(!state(&app, c).crouched, "stands up once clear: {p}");
    assert!(p.x > 6.0 - 0.05, "stood up only past the bar's far edge: {p}");
}

#[test]
fn a_ceiling_ends_the_jump_at_once() {
    let mut app = character_app();
    floor(&mut app);
    // Underside 2.6 m: 0.2 m above the standing capsule's top.
    static_box(&mut app, Vec3::new(5.0, 0.5, 5.0), Vec3::new(0.0, 3.1, 0.0));
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    run(&mut app, 20);
    assert!(state(&app, c).grounded);
    input(&mut app, c, |i| i.jump = true);
    let mut hit_at = None;
    let mut landed_at = None;
    for step in 0..120 {
        app.update();
        let ceiling = app
            .world()
            .get::<CharacterBody>(c)
            .expect("body")
            .ground
            .ceiling_hit;
        if ceiling && hit_at.is_none() {
            hit_at = Some(step);
            assert!(
                state(&app, c).velocity.y <= 0.0,
                "the head hit kills the upward speed"
            );
        }
        if step > 2 && state(&app, c).grounded {
            landed_at = Some(step);
            break;
        }
    }
    assert!(hit_at.is_some(), "the jump reached the ceiling");
    // 0.2 m up then 0.2 m down is about 0.25 s; sticking to the ceiling until gravity cancels
    // 8 m/s would take over 0.8 s.
    let landed = landed_at.expect("landed");
    assert!(landed < 30, "landed {landed} steps after the jump");
}

#[test]
fn a_jump_from_a_moving_platform_keeps_its_speed() {
    let mut app = character_app();
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.25, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(6.0, 0.25, 6.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y + 0.5, 0.0));
    let speed = 2.0;
    let step = |app: &mut App| {
        app.world_mut()
            .get_mut::<Transform>(platform)
            .expect("t")
            .translation
            .x += speed * DT as f32;
        app.update();
    };
    for _ in 0..50 {
        step(&mut app);
    }
    assert!(state(&app, c).grounded, "riding the platform");
    input(&mut app, c, |i| i.jump = true);
    step(&mut app);
    let start = position(&app, c).x;
    for _ in 0..20 {
        step(&mut app);
        assert!(!state(&app, c).grounded, "still in the air");
    }
    let travelled = position(&app, c).x - start;
    let expected = speed * 20.0 * DT as f32;
    assert!(
        (travelled - expected).abs() < 0.15,
        "kept the platform's speed in the air: {travelled} m, expected {expected} m"
    );
    assert!(
        (state(&app, c).velocity.x - speed).abs() < 0.3,
        "{:?}",
        state(&app, c).velocity
    );
    // It lands back on the platform (which kept moving under it) without sliding off.
    for _ in 0..120 {
        step(&mut app);
    }
    let s = state(&app, c);
    assert!(s.grounded, "landed on the platform");
    assert!(
        (s.ground_velocity.x - speed).abs() < 0.3,
        "ground velocity {:?}",
        s.ground_velocity
    );
    assert!(
        s.velocity.x.abs() < 0.5,
        "velocity is relative to the platform again: {:?}",
        s.velocity
    );
}

#[test]
fn velocity_is_the_motion_achieved() {
    let mut app = character_app();
    floor(&mut app);
    // A wall at x = 3.
    static_box(&mut app, Vec3::new(0.25, 3.0, 10.0), Vec3::new(3.25, 3.0, 0.0));
    let c = spawn_character(&mut app, Vec3::new(-6.0, REST_Y, 0.0));
    run(&mut app, 20);
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 60);
    let open = state(&app, c).velocity;
    assert!(open.x > 5.5, "open ground keeps the full walking speed: {open:?}");
    run(&mut app, 120);
    let blocked = state(&app, c);
    assert!(position(&app, c).x < 3.0, "the wall stops it");
    assert!(
        Vec3::new(blocked.velocity.x, 0.0, blocked.velocity.z).length() < 0.5,
        "pushing into a wall reads as standing: {:?}",
        blocked.velocity
    );
    assert!(blocked.grounded);
}

#[test]
fn a_limp_ragdoll_character_ignores_its_input() {
    let mut app = character_app();
    floor(&mut app);
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    let mut ragdoll = Ragdoll::new(RagdollProfile::default());
    ragdoll.mode = RagdollMode::Limp;
    app.world_mut().entity_mut(c).insert(ragdoll);
    run(&mut app, 20);
    input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    run(&mut app, 60);
    assert!(position(&app, c).x.abs() < 1e-4, "the capsule waits while limp");
    assert_eq!(state(&app, c).velocity, Vec3::ZERO);
    // Recovered: input drives it again.
    app.world_mut().get_mut::<Ragdoll>(c).expect("ragdoll").mode = RagdollMode::Kinematic;
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 60);
    assert!(
        position(&app, c).x > 1.0,
        "walks after the recover: {}",
        position(&app, c)
    );
}

#[test]
fn a_crouched_character_against_a_wall_can_stand_up() {
    let mut app = character_app();
    floor(&mut app);
    // A tall wall at x = 1: the character crouch-walks into it, then releases crouch.
    static_box(&mut app, Vec3::new(0.25, 3.0, 10.0), Vec3::new(1.25, 3.0, 0.0));
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    run(&mut app, 20);
    input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.crouch = true;
    });
    run(&mut app, 60);
    assert!(state(&app, c).crouched);
    assert!(
        position(&app, c).x > 0.6,
        "pressed against the wall: {}",
        position(&app, c)
    );
    input(&mut app, c, |i| i.crouch = false);
    run(&mut app, 2);
    assert!(
        !state(&app, c).crouched,
        "a wall beside the capsule leaves headroom"
    );
}

/// Spec 93 § 10 (the § 2.2 hand-off): a character whose ragdoll is `Active` does not move, as
/// for `Limp`; switching back to `Kinematic` gives the input back.
#[test]
fn an_active_ragdoll_character_ignores_its_input() {
    let mut app = character_app();
    floor(&mut app);
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    let mut ragdoll = Ragdoll::new(RagdollProfile::default());
    ragdoll.mode = RagdollMode::Active;
    app.world_mut().entity_mut(c).insert(ragdoll);
    run(&mut app, 20);
    input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    run(&mut app, 60);
    assert!(position(&app, c).x.abs() < 1e-4, "the capsule waits while active");
    assert_eq!(state(&app, c).velocity, Vec3::ZERO);
    app.world_mut().get_mut::<Ragdoll>(c).expect("ragdoll").mode = RagdollMode::Kinematic;
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 60);
    assert!(
        position(&app, c).x > 1.0,
        "walks after the recover: {}",
        position(&app, c)
    );
}

/// Spec 93 § 2.2: `CollisionQuery::has_headroom` defaults to `true`, so a backend without
/// ceilings (the flat-plane stub) never refuses to stand up.
#[test]
fn the_default_headroom_test_never_refuses() {
    use regia_framework::character::{CollisionQuery, FlatPlaneQuery};
    let mut app = App::new();
    let e = app.world_mut().spawn_empty().id();
    for rise in [0.0, 0.8, 100.0] {
        assert!(FlatPlaneQuery.has_headroom(app.world_mut(), e, Vec3::new(0.0, 0.8, 0.0), 0.3, 0.5, rise));
    }
}

/// Spec 93 § 2.2: walking off the edge of a moving platform (a ledge, not a jump) also keeps the
/// platform's speed in the air. The platform moves along +Z and the character walks off its +X
/// edge, so all of its Z motion in the air comes from the platform.
#[test]
fn walking_off_a_moving_platform_keeps_its_speed() {
    let mut app = character_app();
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.25, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(1.5, 0.25, 30.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y + 0.5, 0.0));
    let speed = 2.0;
    let step = |app: &mut App| {
        app.world_mut()
            .get_mut::<Transform>(platform)
            .expect("t")
            .translation
            .z += speed * DT as f32;
        app.update();
    };
    for _ in 0..50 {
        step(&mut app);
    }
    let riding = state(&app, c);
    assert!(riding.grounded, "riding the platform");
    assert!(
        (riding.ground_velocity.z - speed).abs() < 0.3,
        "ground velocity {:?}",
        riding.ground_velocity
    );
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut steps = 0;
    while state(&app, c).grounded {
        step(&mut app);
        steps += 1;
        assert!(steps < 300, "walked off the edge within 300 steps");
    }
    // No input in the air: no air control and no air braking, so the Z speed is the platform's.
    input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let start = position(&app, c).z;
    for _ in 0..15 {
        step(&mut app);
        assert!(!state(&app, c).grounded, "still in the air");
    }
    let travelled = position(&app, c).z - start;
    let expected = speed * 15.0 * DT as f32;
    assert!(
        (travelled - expected).abs() < 0.15,
        "kept the platform's speed after the ledge: {travelled} m, expected {expected} m"
    );
    let s = state(&app, c);
    assert!((s.velocity.z - speed).abs() < 0.3, "{:?}", s.velocity);
    assert_eq!(s.ground_velocity, Vec3::ZERO, "no ground in the air");
}

/// A 30° ramp whose top face falls towards +X (normal `(sin 30°, cos 30°, 0)`), 20 m long,
/// through the origin.
fn downhill_ramp(app: &mut App) {
    app.world_mut().spawn((
        Transform {
            translation: Vec3::ZERO,
            rotation: Quat::from_rotation_z(-30.0_f32.to_radians()),
            ..Default::default()
        },
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
}

/// The ramp's top face height at `x`.
fn ramp_top(x: f32) -> f32 {
    let angle = 30.0_f32.to_radians();
    0.5 / angle.cos() - x * angle.tan()
}

/// rc1 physics review, finding 2: the achieved-velocity clamp compared the 3D motion with the
/// horizontal ask, so a descent masked a blocked walk. Walking down a slope into a wall reads as
/// standing, as on flat ground.
#[test]
fn walking_down_a_slope_into_a_wall_reads_as_standing() {
    let mut app = character_app();
    downhill_ramp(&mut app);
    // A tall wall across the ramp at x = 3.
    static_box(&mut app, Vec3::new(0.25, 8.0, 10.0), Vec3::new(3.25, 0.0, 0.0));
    let c = spawn_character(&mut app, Vec3::new(-4.0, ramp_top(-4.0) + REST_Y + 0.3, 0.0));
    run(&mut app, 30);
    assert!(
        state(&app, c).grounded,
        "standing on the ramp: {}",
        position(&app, c)
    );
    input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut horizontal_speeds = Vec::new();
    for _ in 0..180 {
        app.update();
        let s = state(&app, c);
        horizontal_speeds.push(Vec3::new(s.velocity.x, 0.0, s.velocity.z).length());
    }
    let p = position(&app, c);
    assert!(p.x < 3.0, "the wall stops it: {p}");
    let blocked = state(&app, c);
    assert!(blocked.grounded, "still on the ramp: {p}");
    // Pinned against the wall for the last 30 steps: the reported speed is what it achieved.
    let tail = &horizontal_speeds[horizontal_speeds.len() - 30..];
    assert!(
        tail.iter().all(|&v| v < 0.5),
        "pushing into a wall on a slope reads as standing: {tail:?} at {p}"
    );
}

/// The other side of finding 2's fix: an open slope keeps the walking speed, up and down.
#[test]
fn an_open_slope_keeps_the_walking_speed_both_ways() {
    for (start, dir) in [(-6.0_f32, Vec3::X), (6.0_f32, Vec3::NEG_X)] {
        let mut app = character_app();
        downhill_ramp(&mut app);
        let c = spawn_character(&mut app, Vec3::new(start, ramp_top(start) + REST_Y + 0.3, 0.0));
        run(&mut app, 30);
        input(&mut app, c, |i| i.move_dir = dir);
        run(&mut app, 40);
        let s = state(&app, c);
        let speed = Vec3::new(s.velocity.x, 0.0, s.velocity.z).length();
        assert!(s.grounded, "on the ramp: {}", position(&app, c));
        assert!(
            speed > 5.0,
            "an open slope keeps the walking speed ({dir}): {:?} at {}",
            s.velocity,
            position(&app, c)
        );
    }
}
