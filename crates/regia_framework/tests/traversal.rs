// SPDX-License-Identifier: MPL-2.0
//! Traversal moves on real rapier colliders (`docs/specs/traversal.md` § 11, ADR-2971): fixed steps,
//! no wall clock. Pools, ladders, ledges, vaults, refusals and parameter errors.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState, MovementMode,
    RootMotionDriven, use_rapier_character_controller,
};
use regia_framework::traversal::{
    CharacterTraversal, FlatWater, Ladder, SwimPhase, TraversalEvent, TraversalKind, TraversalOrder,
    TraversalPlugin, TraversalRefusal, TraversalSettings, TraversalWater, probe_traversal,
};
use regia_physics::{
    CharacterBody, Collider, ColliderShape, PhysicsLayers, PhysicsWorld, QueryFilter, RigidBody, WorldQueries,
};
use regia_scene::Transform;

const DT: f64 = 1.0 / 62.5;
/// Resting capsule-center height of the default capsule (radius 0.3, half height 0.9) on y = 0.
const REST_Y: f32 = 1.2;

fn base_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin)
        .add_plugins(TraversalPlugin);
    use_rapier_character_controller(&mut app);
    app
}

fn static_box(app: &mut App, half: Vec3, translation: Vec3, rotation: Quat) -> Entity {
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

/// A floor whose top face is `y`.
fn floor(app: &mut App, y: f32) -> Entity {
    static_box(
        app,
        Vec3::new(40.0, 0.5, 40.0),
        Vec3::new(0.0, y - 0.5, 0.0),
        Quat::IDENTITY,
    )
}

fn spawn_character(app: &mut App, start: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(start),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            CharacterBody::default(),
            TraversalSettings::default(),
        ))
        .id()
}

fn set_input(app: &mut App, e: Entity, f: impl FnOnce(&mut CharacterInput)) {
    f(&mut app.world_mut().get_mut::<CharacterInput>(e).expect("input"));
}

fn position(app: &App, e: Entity) -> Vec3 {
    app.world().get::<Transform>(e).expect("transform").translation
}

fn state(app: &App, e: Entity) -> CharacterState {
    app.world().get::<CharacterState>(e).cloned().expect("state")
}

fn traversal(app: &App, e: Entity) -> CharacterTraversal {
    *app.world().get::<CharacterTraversal>(e).expect("traversal")
}

fn run(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.update();
    }
}

/// Runs until `done` holds, at most `steps` steps. Returns whether it held.
fn run_until(app: &mut App, steps: usize, mut done: impl FnMut(&App) -> bool) -> bool {
    for _ in 0..steps {
        app.update();
        if done(app) {
            return true;
        }
    }
    false
}

/// The traversal events still held.
fn events(app: &App) -> Vec<TraversalEvent> {
    let messages = app.world().resource::<Messages<TraversalEvent>>();
    let mut cursor = messages.get_cursor();
    cursor.read(messages).cloned().collect()
}

/// Whether the capsule at `at` is inside something, measured with a slightly smaller capsule so a
/// resting contact does not count.
fn embedded(app: &App, e: Entity, at: Vec3) -> bool {
    let queries = WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: app.world().get_resource::<PhysicsLayers>(),
    };
    let movement = app.world().get::<CharacterMovement>(e).expect("movement");
    let shape = ColliderShape::Capsule {
        half_height: movement.half_height,
        radius: movement.radius - 0.015,
        axis: regia_physics::ShapeAxis::Y,
    };
    let exclude = [e];
    !queries
        .overlap(
            &shape,
            Transform::from_translation(at),
            QueryFilter::new().exclude_sensors().exclude_entities(&exclude),
        )
        .is_empty()
}

// ---- swim -------------------------------------------------------------------------------------------

const POOL_SURFACE: f32 = -0.2;
/// The capsule center at the float line (default chest offset 0.5).
const FLOAT_Y: f32 = POOL_SURFACE - 0.5;

/// A pool: ground at y = 0 for x < 0 and x > 16; ramps down at x 0..8 and up at x 16..24 are one way to
/// make a basin. The basin floor is y = -4 between the ramps, 8 m long.
fn pool(app: &mut App) {
    let slope = (4.0f32 / 8.0).atan();
    let half_len = (16.0f32 + 64.0).sqrt() * 0.5;
    // Ground on both sides.
    static_box(
        app,
        Vec3::new(10.0, 0.5, 20.0),
        Vec3::new(-10.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    static_box(
        app,
        Vec3::new(10.0, 0.5, 20.0),
        Vec3::new(34.0, -0.5, 0.0),
        Quat::IDENTITY,
    );
    // The basin floor.
    static_box(
        app,
        Vec3::new(4.0, 0.5, 20.0),
        Vec3::new(12.0, -4.5, 0.0),
        Quat::IDENTITY,
    );
    // Down ramp (x 0..8, y 0..-4) and up ramp (x 16..24, y -4..0), as tilted slabs 1 m thick whose
    // top face is the slope.
    let normal_down = Vec3::new(slope.sin(), slope.cos(), 0.0);
    static_box(
        app,
        Vec3::new(half_len, 0.5, 20.0),
        Vec3::new(4.0, -2.0, 0.0) - normal_down * 0.5,
        Quat::from_rotation_z(-slope),
    );
    let normal_up = Vec3::new(-slope.sin(), slope.cos(), 0.0);
    static_box(
        app,
        Vec3::new(half_len, 0.5, 20.0),
        Vec3::new(20.0, -2.0, 0.0) - normal_up * 0.5,
        Quat::from_rotation_z(slope),
    );
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: POOL_SURFACE,
        min: [0.0, -20.0],
        max: [24.0, 20.0],
    })));
}

fn pool_character(app: &mut App) -> Entity {
    pool(app);
    let c = spawn_character(app, Vec3::new(-3.0, REST_Y, 0.0));
    run(app, 10);
    c
}

#[test]
fn a_character_walks_into_a_pool_floats_swims_across_and_walks_out() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);

    // In: down the ramp until the chest is under.
    let entered = run_until(&mut app, 800, |app| traversal(app, c).kind == TraversalKind::Swim);
    assert!(entered, "never swam: at {}", position(&app, c));
    let at = position(&app, c);
    assert!(at.x > 0.0 && at.x < 9.0, "entered on the ramp: {at}");
    assert_eq!(state(&app, c).mode, MovementMode::Swimming);

    // Across: the float line holds within 5 cm once settled, and horizontal speed is the swim speed.
    run(&mut app, 60);
    let mut worst: f32 = 0.0;
    while position(&app, c).x < 15.0 {
        app.update();
        worst = worst.max((position(&app, c).y - FLOAT_Y).abs());
        assert!(!embedded(&app, c, position(&app, c)), "{}", position(&app, c));
        assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);
        assert!(
            state(&app, c).velocity.x <= TraversalSettings::default().swim.speed + 0.01,
            "{:?}",
            state(&app, c).velocity
        );
    }
    assert!(worst <= 0.05, "floats within 5 cm of the line: worst {worst}");
    assert_eq!(traversal(&app, c).phase, SwimPhase::Surface);

    // Out: up the far ramp and walking on dry ground.
    let walked = run_until(&mut app, 1500, |app| {
        traversal(app, c).kind == TraversalKind::None && state(app, c).grounded
    });
    assert!(walked, "never walked out: at {}", position(&app, c));
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
    let chest = position(&app, c).y + 0.5;
    assert!(chest >= POOL_SURFACE + 0.09, "the chest is out: {chest}");
    let done = run_until(&mut app, 800, |app| position(app, c).x > 25.0);
    assert!(done, "kept walking: {}", position(&app, c));
    let end = position(&app, c);
    assert!((end.y - REST_Y).abs() < 0.08, "on the far ground: {end}");
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
}

#[test]
fn a_character_never_flips_between_walking_and_swimming_on_one_spot() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 800, |app| traversal(app, c).kind == TraversalKind::Swim));
    // Stop right after entering: on the ramp at the enter depth.
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let mut changes = 0;
    let mut last = traversal(&app, c).kind;
    for _ in 0..300 {
        app.update();
        let kind = traversal(&app, c).kind;
        if kind != last {
            changes += 1;
            last = kind;
        }
    }
    assert!(changes <= 1, "{changes} mode changes in place");
}

#[test]
fn a_held_dive_reaches_a_depth_and_returns_to_the_line() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    // Into the middle of the pool, over the deep floor.
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    run(&mut app, 120);
    assert!(
        (position(&app, c).y - FLOAT_Y).abs() <= 0.05,
        "settled: {}",
        position(&app, c)
    );
    assert_eq!(traversal(&app, c).phase, SwimPhase::Surface);

    // Hold the dive: it goes down at the dive speed until the floor stops it (y = -4, center -2.8).
    set_input(&mut app, c, |i| i.crouch = true);
    let mut deepest = position(&app, c).y;
    let mut seen_under = false;
    for _ in 0..200 {
        app.update();
        deepest = deepest.min(position(&app, c).y);
        seen_under |= traversal(&app, c).phase == SwimPhase::Underwater;
    }
    assert!(deepest < FLOAT_Y - 1.5, "dove: {deepest}");
    assert!(seen_under, "the phase said so");
    assert_eq!(state(&app, c).mode, MovementMode::Swimming);

    // Release: back to the line.
    set_input(&mut app, c, |i| i.crouch = false);
    let back = run_until(&mut app, 600, |app| (position(app, c).y - FLOAT_Y).abs() <= 0.05);
    assert!(back, "returned: {}", position(&app, c));
    run(&mut app, 120);
    assert!((position(&app, c).y - FLOAT_Y).abs() <= 0.05, "stays");
    assert_eq!(traversal(&app, c).phase, SwimPhase::Surface);
}

#[test]
fn a_dive_stops_at_its_limit() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    if let Some(mut settings) = app.world_mut().get_mut::<TraversalSettings>(c) {
        settings.swim.max_dive_depth = 0.75;
    }
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.crouch = true;
    });
    run(&mut app, 300);
    let y = position(&app, c).y;
    assert!((y - (FLOAT_Y - 0.75)).abs() < 0.02, "held at the limit: {y}");
}

#[test]
fn three_d_swim_input_rises_and_sinks_without_crouch() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| i.move_dir = Vec3::NEG_Y);
    run(&mut app, 60);
    assert!(position(&app, c).y < FLOAT_Y - 0.5, "{}", position(&app, c));
    set_input(&mut app, c, |i| i.move_dir = Vec3::new(0.0, 0.5, 0.0));
    let y0 = position(&app, c).y;
    run(&mut app, 30);
    assert!(position(&app, c).y > y0, "rises");
}

#[test]
fn a_hop_leaves_the_water_and_falls_back_in() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.jump = true;
    });
    app.update();
    assert_eq!(traversal(&app, c).kind, TraversalKind::None);
    assert_eq!(state(&app, c).mode, MovementMode::Falling);
    run(&mut app, 3);
    assert!(position(&app, c).y > FLOAT_Y + 0.05, "going up");
    let back = run_until(&mut app, 300, |app| traversal(app, c).kind == TraversalKind::Swim);
    assert!(back, "falls back in");
}

#[test]
fn walking_near_water_with_a_dry_chest_stays_walking() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 0.5,
        min: [-20.0, -20.0],
        max: [20.0, 20.0],
    })));
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 100);
    assert_eq!(traversal(&app, c).kind, TraversalKind::None, "wading is walking");
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
    assert!(position(&app, c).x > 2.0);
}

/// The real water path: a `WaterBody` plane, read as `WaterQuery` reads it.
#[cfg(feature = "client")]
#[test]
fn the_terrain_water_body_is_the_default_source() {
    use regia_terrain::water_body::{WaterBody, WaterLook};
    let mut app = base_app();
    floor(&mut app, -5.0);
    app.world_mut().spawn(WaterBody::plane(
        [0.0, 0.0, 0.0],
        [20.0, 20.0],
        WaterLook::default(),
    ));
    let c = spawn_character(&mut app, Vec3::new(0.0, 3.0, 0.0));
    let swam = run_until(&mut app, 300, |app| traversal(app, c).kind == TraversalKind::Swim);
    assert!(swam, "falls in: {}", position(&app, c));
    run(&mut app, 240);
    assert!(
        (position(&app, c).y - -0.5).abs() <= 0.05,
        "floats under the plane: {}",
        position(&app, c)
    );
    // Off the body's edge there is no water: it falls.
    let settings = TraversalSettings::default();
    assert_eq!(settings.swim.chest_offset, 0.5);
    let far = probe_traversal(app.world_mut(), c, Some(Vec3::new(30.0, 0.0, 0.0)), None).expect("probe");
    assert_eq!(far.water_surface, None);
}

// ---- ladder -----------------------------------------------------------------------------------------

const WALL_X: f32 = 5.0;
const UPPER: f32 = 3.0;

/// A ground floor at y = 0 and, from `x = WALL_X` on, an upper floor at y = 3 (a solid block), with
/// a ladder on its face, climbed going +X.
fn ladder_rig(app: &mut App) -> Entity {
    floor(app, 0.0);
    static_box(
        app,
        Vec3::new(10.0, UPPER * 0.5, 20.0),
        Vec3::new(WALL_X + 10.0, UPPER * 0.5, 0.0),
        Quat::IDENTITY,
    );
    app.world_mut()
        .spawn((
            // Local -Z (into) must point +X: a quarter turn about +Y sends -Z to -X, so the opposite.
            Transform {
                translation: Vec3::new(WALL_X, 0.0, 0.0),
                rotation: Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
                ..Default::default()
            },
            Ladder {
                height: UPPER,
                user_id: 77,
                ..Ladder::default()
            },
        ))
        .id()
}

#[test]
fn ladder_enter_climb_and_exit_stands_on_the_upper_floor() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(2.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);

    assert!(
        run_until(&mut app, 300, |app| traversal(app, c).kind
            == TraversalKind::Ladder),
        "attached: {}",
        position(&app, c)
    );
    assert_eq!(state(&app, c).mode, MovementMode::CLIMBING);
    let line_x = WALL_X - Ladder::default().standoff;
    assert!(
        (position(&app, c).x - line_x).abs() < 0.01,
        "on the line: {}",
        position(&app, c)
    );

    // Climbing: on the line, going up at the climb speed, the facing locked into the ladder.
    let y0 = position(&app, c).y;
    run(&mut app, 30);
    let climbed = position(&app, c).y - y0;
    assert!(
        (climbed - Ladder::default().climb_speed * 30.0 * DT as f32).abs() < 0.03,
        "climbed {climbed}"
    );
    let facing = app.world().get::<Transform>(c).expect("transform").rotation * Vec3::NEG_Z;
    assert!((facing - Vec3::X).length() < 1.0e-4, "faces the ladder: {facing}");

    // Off the top: standing on the upper floor, walking, not inside anything.
    let mut max_embed = false;
    let done = run_until(&mut app, 800, |app| {
        max_embed |= embedded(app, c, position(app, c));
        traversal(app, c).kind == TraversalKind::None
    });
    assert!(done, "finished at {}", position(&app, c));
    assert!(!max_embed, "never embedded");
    run(&mut app, 2);
    let at = position(&app, c);
    assert!(at.x > WALL_X, "over the upper floor: {at}");
    assert!((at.y - (UPPER + REST_Y)).abs() < 0.08, "on it: {at}");
    let s = state(&app, c);
    assert!(s.grounded && s.mode == MovementMode::Walking, "{s:?}");
    assert_eq!(traversal(&app, c).last_refusal, None);
}

#[test]
fn a_blocked_ladder_top_refuses_the_exit_stays_on_and_exits_once_clear() {
    let mut app = base_app();
    ladder_rig(&mut app);
    // A crate on the exit spot.
    let crate_box = static_box(
        &mut app,
        Vec3::new(0.5, 0.5, 2.0),
        Vec3::new(WALL_X + 0.6, UPPER + 0.5, 0.0),
        Quat::IDENTITY,
    );
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let refused = run_until(&mut app, 1000, |app| {
        traversal(app, c).last_refusal == Some(TraversalRefusal::LadderExitBlocked)
    });
    assert!(
        refused,
        "refused: at {} {:?}",
        position(&app, c),
        traversal(&app, c)
    );
    run(&mut app, 60);
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::Ladder, "stays on");
    assert_eq!(state(&app, c).mode, MovementMode::CLIMBING);
    let at = position(&app, c);
    assert!(at.x < WALL_X, "still on the line: {at}");
    assert!(!embedded(&app, c, at));

    // Clear the crate: the exit runs.
    app.world_mut().despawn(crate_box);
    let out = run_until(&mut app, 300, |app| {
        traversal(app, c).kind == TraversalKind::None && state(app, c).grounded
    });
    assert!(out, "exits once clear: {}", position(&app, c));
    assert!(position(&app, c).x > WALL_X);
}

#[test]
fn no_floor_at_the_top_refuses_with_its_own_code() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    // A thin wall with no floor behind it, and a ladder on it.
    static_box(
        &mut app,
        Vec3::new(0.1, UPPER * 0.5, 20.0),
        Vec3::new(WALL_X + 0.1, UPPER * 0.5, 0.0),
        Quat::IDENTITY,
    );
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(WALL_X, 0.0, 0.0),
            rotation: Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
            ..Default::default()
        },
        Ladder {
            height: UPPER,
            ..Ladder::default()
        },
    ));
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let refused = run_until(&mut app, 1000, |app| {
        traversal(app, c).last_refusal == Some(TraversalRefusal::LadderExitNoFloor)
    });
    assert!(refused, "{:?}", traversal(&app, c));
    assert_eq!(traversal(&app, c).kind, TraversalKind::Ladder);
}

#[test]
fn jumping_off_a_ladder_leaves_backward_and_does_not_reattach_at_once() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    run(&mut app, 40);
    // Keep pushing into the ladder while jumping off: the cooldown holds it off.
    set_input(&mut app, c, |i| i.jump = true);
    app.update();
    assert_eq!(traversal(&app, c).kind, TraversalKind::None);
    let s = state(&app, c);
    assert_eq!(s.mode, MovementMode::Falling);
    assert!(s.velocity.x < -1.0 && s.velocity.y > 1.0, "{:?}", s.velocity);
    run(&mut app, 5);
    assert_eq!(
        traversal(&app, c).kind,
        TraversalKind::None,
        "no instant re-attach"
    );
    assert!(position(&app, c).x < WALL_X - Ladder::default().standoff);
}

#[test]
fn a_character_attaches_at_the_top_and_climbs_down_to_walk_away() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(WALL_X + 1.5, UPPER + REST_Y, 0.0));
    run(&mut app, 10);
    assert!(state(&app, c).grounded);
    // Walk back over the edge.
    set_input(&mut app, c, |i| i.move_dir = Vec3::NEG_X);
    assert!(
        run_until(&mut app, 300, |app| traversal(app, c).kind
            == TraversalKind::Ladder),
        "attached at the top: {}",
        position(&app, c)
    );
    assert!(position(&app, c).y > UPPER, "at the top: {}", position(&app, c));
    let done = run_until(&mut app, 800, |app| {
        traversal(app, c).kind == TraversalKind::None && state(app, c).grounded
    });
    assert!(done, "down: {} {:?}", position(&app, c), traversal(&app, c));
    let at = position(&app, c);
    assert!((at.y - REST_Y).abs() < 0.08, "on the ground: {at}");
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
}

#[test]
fn a_mid_air_character_attaches_from_the_side() {
    let mut app = base_app();
    ladder_rig(&mut app);
    // Falling past the ladder, moving into it.
    let c = spawn_character(&mut app, Vec3::new(WALL_X - 0.7, 3.0, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.mantle.enabled = false;
    }
    if let Some(mut s) = app.world_mut().get_mut::<CharacterState>(c) {
        s.grounded = false;
    }
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let attached = run_until(&mut app, 120, |app| {
        traversal(app, c).kind == TraversalKind::Ladder
    });
    assert!(attached, "{}", position(&app, c));
    assert!(
        position(&app, c).y > REST_Y + 0.2,
        "mid-height: {}",
        position(&app, c)
    );
}

#[test]
fn a_disabled_or_invalid_ladder_attaches_nobody() {
    for ladder in [
        Ladder {
            height: UPPER,
            enabled: false,
            ..Ladder::default()
        },
        Ladder {
            height: UPPER,
            climb_speed: f32::NAN,
            ..Ladder::default()
        },
    ] {
        let mut app = base_app();
        let id = ladder_rig(&mut app);
        *app.world_mut().get_mut::<Ladder>(id).expect("ladder") = ladder;
        let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
        run(&mut app, 10);
        set_input(&mut app, c, |i| i.move_dir = Vec3::X);
        run(&mut app, 200);
        assert_eq!(traversal(&app, c).kind, TraversalKind::None, "{ladder:?}");
        assert_eq!(state(&app, c).mode, MovementMode::Walking);
    }
}

#[test]
fn a_ladder_with_no_room_on_its_line_refuses() {
    let mut app = base_app();
    ladder_rig(&mut app);
    // A slim post on the climb line: the character can reach the attach zone in front of it.
    static_box(
        &mut app,
        Vec3::new(0.1, 3.0, 0.1),
        Vec3::new(WALL_X - Ladder::default().standoff, 3.0, 0.0),
        Quat::IDENTITY,
    );
    let c = spawn_character(&mut app, Vec3::new(2.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let refused = run_until(&mut app, 200, |app| {
        traversal(app, c).last_refusal == Some(TraversalRefusal::LadderNoRoom)
    });
    assert!(refused, "{:?}", traversal(&app, c));
    assert_eq!(traversal(&app, c).kind, TraversalKind::None);
}

// ---- mantle and vault ---------------------------------------------------------------------------------

/// A floor at y = 0 and a block whose near face is `x = 2` and whose top is `height`.
fn ledge_rig(app: &mut App, height: f32) -> Entity {
    floor(app, 0.0);
    static_box(
        app,
        Vec3::new(5.0, height * 0.5, 20.0),
        Vec3::new(7.0, height * 0.5, 0.0),
        Quat::IDENTITY,
    )
}

fn at_wall(app: &mut App) -> Entity {
    let c = spawn_character(app, Vec3::new(1.4, REST_Y, 0.0));
    run(app, 10);
    c
}

/// Presses jump while moving at the wall and reports the result of that one attempt.
fn attempt(app: &mut App, c: Entity) -> (TraversalKind, Option<TraversalRefusal>) {
    set_input(app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    app.update();
    let t = traversal(app, c);
    (t.kind, t.last_refusal)
}

fn attempt_at_height(height: f32) -> (TraversalKind, Option<TraversalRefusal>) {
    let mut app = base_app();
    ledge_rig(&mut app, height);
    let c = at_wall(&mut app);
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.mantle.auto_in_air = false;
    }
    attempt(&mut app, c)
}

#[test]
fn mantles_succeed_only_inside_the_stated_height_range() {
    let settings = TraversalSettings::default().mantle;
    for height in [settings.min_height + 0.05, 1.0, 1.5, settings.max_height - 0.05] {
        let (kind, refusal) = attempt_at_height(height);
        assert_eq!(kind, TraversalKind::Mantle, "{height}: {refusal:?}");
        assert_eq!(refusal, None);
    }
    let (kind, refusal) = attempt_at_height(settings.min_height - 0.05);
    assert_eq!(
        (kind, refusal),
        (TraversalKind::None, Some(TraversalRefusal::LedgeTooLow))
    );
    let (kind, refusal) = attempt_at_height(settings.max_height + 0.05);
    assert_eq!(
        (kind, refusal),
        (TraversalKind::None, Some(TraversalRefusal::LedgeTooHigh))
    );
    let (kind, refusal) = attempt_at_height(4.0);
    assert_eq!(
        (kind, refusal),
        (TraversalKind::None, Some(TraversalRefusal::LedgeTooHigh))
    );
}

/// How far the feet of a capsule settled as `at_wall` settles it sit above the floor: the controller
/// keeps its skin gap (`CharacterBody::offset`), and the ledge probe measures heights from these feet.
fn settled_foot_gap() -> f32 {
    let mut app = base_app();
    floor(&mut app, 0.0);
    let c = at_wall(&mut app);
    let movement = app
        .world()
        .get::<CharacterMovement>(c)
        .cloned()
        .expect("movement");
    position(&app, c).y - (movement.half_height + movement.radius)
}

/// The top scan runs one 10 cm row past `max_height`, so a top just over the limit can be found by a
/// row and only the range check refuses it, while a height between rows never reaches that check.
/// A sweep of the band makes sure some height lands on a row, wherever the contact starts them.
/// Heights here are above the settled feet (spec § 5), so each block top adds the foot gap.
#[test]
fn the_height_limits_hold_all_the_way_up_to_the_limit() {
    let settings = TraversalSettings::default().mantle;
    let gap = settled_foot_gap();
    assert!((0.0..0.1).contains(&gap), "foot gap {gap}");
    for excess in [0.01, 0.02, 0.03, 0.04, 0.06, 0.07, 0.08, 0.09] {
        let height = settings.max_height + excess;
        assert_eq!(
            attempt_at_height(height + gap),
            (TraversalKind::None, Some(TraversalRefusal::LedgeTooHigh)),
            "a top {height} above the feet is over the limit"
        );
    }
    for short in [0.01, 0.02, 0.03] {
        let height = settings.min_height - short;
        assert_eq!(
            attempt_at_height(height + gap),
            (TraversalKind::None, Some(TraversalRefusal::LedgeTooLow)),
            "a top {height} above the feet is under the limit"
        );
        for height in [settings.min_height + short, settings.max_height - short] {
            let (kind, refusal) = attempt_at_height(height + gap);
            assert_eq!(
                kind,
                TraversalKind::Mantle,
                "{height} above the feet: {refusal:?}"
            );
            assert_eq!(refusal, None);
        }
    }
}

#[test]
fn a_mantle_ends_standing_on_the_ledge_and_fires_its_slot_and_targets() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    app.update();
    assert_eq!(traversal(&app, c).kind, TraversalKind::Mantle);
    assert_eq!(state(&app, c).mode, MovementMode::TRAVERSING);

    let started = events(&app)
        .into_iter()
        .find(|event| matches!(event, TraversalEvent::Started { .. }))
        .expect("a started event");
    let TraversalEvent::Started {
        entity,
        kind,
        slot,
        montage,
        duration,
        warp_targets,
    } = started
    else {
        panic!("found above")
    };
    assert_eq!((entity, kind), (c, TraversalKind::Mantle));
    assert_eq!((slot.as_str(), montage), ("traversal", "mantle"));
    assert!(duration > 0.0);
    let names: Vec<&str> = warp_targets.iter().map(|t| t.name).collect();
    assert_eq!(names, ["ledge_edge", "ledge_stand"]);
    let edge = warp_targets[0].target.position;
    assert!(
        (edge - Vec3::new(2.0, 1.5, 0.0)).length() < 0.02,
        "edge at the corner: {edge}"
    );
    let stand = warp_targets[1].target.position;
    assert!(
        stand.x > 2.0 && (stand.y - (1.5 + REST_Y)).abs() < 0.05,
        "{stand}"
    );
    assert!(warp_targets[0].target.facing.is_some());

    // The curve rises first: the capsule is never inside the block.
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let mut hit = false;
    let mut steps = 1;
    let done = run_until(&mut app, 400, |app| {
        hit |= embedded(app, c, position(app, c));
        steps += 1;
        traversal(app, c).kind == TraversalKind::None
    });
    assert!(done, "ended at {}", position(&app, c));
    assert!(!hit, "never embedded");
    assert!(
        steps as f32 * DT as f32 >= duration - 2.0 * DT as f32,
        "the stated time"
    );
    let at = position(&app, c);
    assert!(
        at.x > 2.0 && (at.y - (1.5 + REST_Y)).abs() < 0.08,
        "on the ledge: {at}"
    );
    let s = state(&app, c);
    assert!(s.grounded && s.mode == MovementMode::Walking, "{s:?}");
    assert_eq!(s.velocity, Vec3::ZERO);
    // It goes on walking from there.
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 30);
    assert!(position(&app, c).x > at.x + 0.5, "walks on");
}

#[test]
fn no_headroom_is_refused_with_its_reason() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.0);
    // A slab over the ledge: 1.5 m of gap above the 1.0 m ledge, the capsule stands 2.4 m. The slab's
    // front is flush with the wall so the column reads a gap at the ledge.
    static_box(
        &mut app,
        Vec3::new(5.0, 0.5, 20.0),
        Vec3::new(7.0, 3.0, 0.0),
        Quat::IDENTITY,
    );
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::None);
    assert_eq!(refusal, Some(TraversalRefusal::LedgeNoHeadroom));
}

#[test]
fn a_ceiling_over_the_climber_is_no_headroom_too() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    // A slab just over the head, ending before the wall: the rise is blocked.
    static_box(
        &mut app,
        Vec3::new(1.0, 0.25, 20.0),
        Vec3::new(1.0, 2.8, 0.0),
        Quat::IDENTITY,
    );
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::None);
    assert_eq!(refusal, Some(TraversalRefusal::LedgeNoHeadroom));
}

#[test]
fn a_too_steep_top_is_refused_with_its_reason() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    // A slab tilted 60 degrees, rising going +X, with its low end on the floor at x = 2 and its top at
    // about 1.7 m: the face reads as a wall, the top surface as too steep.
    let tilt = 60.0_f32.to_radians();
    let half_length = 1.0;
    let center = Vec3::new(2.0 + half_length * tilt.cos(), half_length * tilt.sin(), 0.0);
    let normal = Vec3::new(-tilt.sin(), tilt.cos(), 0.0);
    static_box(
        &mut app,
        Vec3::new(half_length, 0.5, 20.0),
        center - normal * 0.5,
        Quat::from_rotation_z(tilt),
    );
    // The face leans away at the bottom, so stand closer than at a vertical wall.
    let c = spawn_character(&mut app, Vec3::new(1.8, REST_Y, 0.0));
    run(&mut app, 10);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::None);
    assert_eq!(refusal, Some(TraversalRefusal::LedgeTooSteep));
}

#[test]
fn a_too_thin_ledge_is_refused_with_its_reason() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    // A plank 0.2 m thick at 1.5 m: too high to vault, with nothing to stand on.
    static_box(
        &mut app,
        Vec3::new(0.1, 0.75, 20.0),
        Vec3::new(2.1, 0.75, 0.0),
        Quat::IDENTITY,
    );
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::None);
    assert_eq!(refusal, Some(TraversalRefusal::LedgeTooThin));
}

#[test]
fn a_vault_goes_over_a_low_thin_wall_and_lands_beyond_it() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    static_box(
        &mut app,
        Vec3::new(0.15, 0.5, 20.0),
        Vec3::new(2.15, 0.5, 0.0),
        Quat::IDENTITY,
    );
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!((kind, refusal), (TraversalKind::Vault, None));
    let names: Vec<&str> = events(&app)
        .iter()
        .find_map(|event| match event {
            TraversalEvent::Started { warp_targets, .. } => {
                Some(warp_targets.iter().map(|t| t.name).collect())
            }
            _ => None,
        })
        .expect("started");
    assert_eq!(names, ["vault_over", "vault_land"]);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let mut hit = false;
    let done = run_until(&mut app, 300, |app| {
        hit |= embedded(app, c, position(app, c));
        traversal(app, c).kind == TraversalKind::None
    });
    assert!(done);
    assert!(!hit, "never embedded");
    let at = position(&app, c);
    assert!(at.x > 2.3, "beyond the wall: {at}");
    assert!((at.y - REST_Y).abs() < 0.08, "on the floor: {at}");
    assert!(state(&app, c).grounded);
}

#[test]
fn a_vaultable_wall_with_nowhere_to_land_says_so() {
    let mut app = base_app();
    // The floor stops under the wall: behind it is a deep drop.
    static_box(
        &mut app,
        Vec3::new(5.0, 0.5, 20.0),
        Vec3::new(-2.7, -0.5, 0.0),
        Quat::IDENTITY,
    );
    static_box(
        &mut app,
        Vec3::new(0.15, 0.5, 20.0),
        Vec3::new(2.15, 0.5, 0.0),
        Quat::IDENTITY,
    );
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::None);
    assert_eq!(refusal, Some(TraversalRefusal::VaultNoLanding));
}

#[test]
fn a_refused_jump_still_jumps_and_a_jump_with_no_wall_reports_nothing() {
    let mut app = base_app();
    ledge_rig(&mut app, 3.5);
    let c = at_wall(&mut app);
    let (_, refusal) = attempt(&mut app, c);
    assert_eq!(refusal, Some(TraversalRefusal::LedgeTooHigh));
    let y = position(&app, c).y;
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    run(&mut app, 5);
    assert!(position(&app, c).y > y, "the jump happened");
    assert_eq!(state(&app, c).mode, MovementMode::Falling);

    let mut app = base_app();
    floor(&mut app, 0.0);
    let c = at_wall(&mut app);
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!((kind, refusal), (TraversalKind::None, None));
    run(&mut app, 3);
    assert_eq!(state(&app, c).mode, MovementMode::Falling);
}

#[test]
fn a_falling_character_takes_a_ledge_by_itself() {
    let mut app = base_app();
    ledge_rig(&mut app, 2.8);
    // Feet at 1.5 going past a 2.8 m top: 1.3 m above the feet.
    let c = spawn_character(&mut app, Vec3::new(1.4, 2.7, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<CharacterState>(c) {
        s.grounded = false;
    }
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(
        run_until(&mut app, 30, |app| traversal(app, c).kind
            == TraversalKind::Mantle),
        "{} {:?}",
        position(&app, c),
        traversal(&app, c)
    );
    // Automatic attempts report nothing when refused, so a plain fall against a wall stays quiet.
    let mut app = base_app();
    ledge_rig(&mut app, 8.0);
    let c = spawn_character(&mut app, Vec3::new(1.4, 2.7, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<CharacterState>(c) {
        s.grounded = false;
    }
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 20);
    assert_eq!(traversal(&app, c).last_refusal, None);
}

#[test]
fn a_request_runs_a_mantle_without_input_and_a_bad_one_is_refused() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    let mut t = traversal(&app, c);
    t.request(TraversalOrder::Mantle { forward: Vec3::X });
    *app.world_mut()
        .get_mut::<CharacterTraversal>(c)
        .expect("traversal") = t;
    app.update();
    assert_eq!(traversal(&app, c).kind, TraversalKind::Mantle);

    let mut app = base_app();
    floor(&mut app, 0.0);
    let c = at_wall(&mut app);
    for order in [
        TraversalOrder::Mantle {
            forward: Vec3::new(f32::NAN, 0.0, 0.0),
        },
        TraversalOrder::Ladder {
            user_id: 1,
            near: Vec3::new(f32::INFINITY, 0.0, 0.0),
            up: true,
        },
        TraversalOrder::Jump {
            to: Vec3::new(0.0, f32::NAN, 0.0),
            arc_height: 1.0,
        },
    ] {
        let mut t = traversal(&app, c);
        t.last_refusal = None;
        t.request(order);
        *app.world_mut()
            .get_mut::<CharacterTraversal>(c)
            .expect("traversal") = t;
        app.update();
        assert_eq!(
            traversal(&app, c).last_refusal,
            Some(TraversalRefusal::TraversalBadRequest),
            "{order:?}"
        );
        assert_eq!(traversal(&app, c).kind, TraversalKind::None);
    }
    let mut t = traversal(&app, c);
    t.request(TraversalOrder::Ladder {
        user_id: 42,
        near: Vec3::ZERO,
        up: true,
    });
    *app.world_mut()
        .get_mut::<CharacterTraversal>(c)
        .expect("traversal") = t;
    app.update();
    assert_eq!(
        traversal(&app, c).last_refusal,
        Some(TraversalRefusal::LadderNoneInReach)
    );
}

#[test]
fn something_entering_the_path_interrupts_the_curve_with_no_embedding() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    let (kind, _) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::Mantle);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    run(&mut app, 5);
    // A slab drops into the way, over the capsule's head.
    let p = position(&app, c);
    static_box(
        &mut app,
        Vec3::new(2.0, 0.2, 20.0),
        Vec3::new(p.x, p.y + 1.2 + 0.4, 0.0),
        Quat::IDENTITY,
    );
    let stopped = run_until(&mut app, 200, |app| traversal(app, c).kind == TraversalKind::None);
    assert!(stopped);
    assert_eq!(
        traversal(&app, c).last_refusal,
        Some(TraversalRefusal::TraversalInterrupted)
    );
    assert!(!embedded(&app, c, position(&app, c)), "{}", position(&app, c));
    run(&mut app, 120);
    let s = state(&app, c);
    assert!(s.grounded, "fell back down: {s:?}");
    assert!(!embedded(&app, c, position(&app, c)), "{}", position(&app, c));
}

#[test]
fn a_shove_during_a_curve_does_not_push_the_capsule_into_geometry() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    let (kind, _) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::Mantle);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    run(&mut app, 8);
    // Moved 0.5 m away from the wall at waist height, halfway through the rise.
    app.world_mut()
        .get_mut::<Transform>(c)
        .expect("transform")
        .translation
        .x -= 0.5;
    let mut hit = false;
    run_until(&mut app, 200, |app| {
        hit |= embedded(app, c, position(app, c));
        traversal(app, c).kind == TraversalKind::None
    });
    assert!(!hit, "never embedded");
    assert_eq!(
        traversal(&app, c).last_refusal,
        Some(TraversalRefusal::TraversalInterrupted),
        "the shove ended the move"
    );
    run(&mut app, 60);
    assert!(!embedded(&app, c, position(&app, c)));
    assert!(state(&app, c).grounded, "fell back to the floor");
    assert!(position(&app, c).y < REST_Y + 0.1, "{}", position(&app, c));
}

// ---- opt-in, parameters, root motion ---------------------------------------------------------------

/// A scripted run of both kinds of character side by side in their own apps.
fn trace(with_settings: bool) -> Vec<(u32, u32, u32, u32, u32, u32)> {
    let mut app = base_app();
    floor(&mut app, 0.0);
    static_box(
        &mut app,
        Vec3::new(1.0, 0.1, 1.0),
        Vec3::new(4.0, 0.1, 0.0),
        Quat::IDENTITY,
    );
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    if !with_settings {
        app.world_mut().entity_mut(c).remove::<TraversalSettings>();
        app.world_mut().entity_mut(c).remove::<CharacterTraversal>();
    }
    let mut out = Vec::new();
    for step in 0..400u32 {
        set_input(&mut app, c, |i| {
            i.move_dir = if step < 200 { Vec3::X } else { Vec3::Z };
            i.jump = step == 100;
            i.crouch = (300..340).contains(&step);
            i.sprint = (50..80).contains(&step);
        });
        app.update();
        let p = position(&app, c);
        let v = state(&app, c).velocity;
        out.push((
            p.x.to_bits(),
            p.y.to_bits(),
            p.z.to_bits(),
            v.x.to_bits(),
            v.y.to_bits(),
            v.z.to_bits(),
        ));
    }
    out
}

/// A character with settings and nothing to use steps the same bits as one without (the no-settings
/// path has no traversal code at all; `character_polish` and the pins cover it against the past).
#[test]
fn settings_with_no_volumes_in_reach_step_the_same_bits_as_no_settings() {
    assert_eq!(trace(true), trace(false));
}

#[test]
fn a_character_with_no_settings_has_no_traversal_state_and_ignores_water() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 10.0,
        min: [-20.0, -20.0],
        max: [20.0, 20.0],
    })));
    let c = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, REST_Y, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState::default(),
        ))
        .id();
    run(&mut app, 30);
    assert!(app.world().get::<CharacterTraversal>(c).is_none());
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
    assert!(state(&app, c).grounded, "gravity and ground as always");
}

#[test]
fn non_finite_settings_are_refused_and_default_movement_runs() {
    let mut app = base_app();
    floor(&mut app, 0.0);
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 10.0,
        min: [-20.0, -20.0],
        max: [20.0, 20.0],
    })));
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.swim.drag = f32::NAN;
    }
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 60);
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::None, "no swim under 10 m of water");
    assert_eq!(t.last_refusal, Some(TraversalRefusal::TraversalBadSettings));
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
    let p = position(&app, c);
    assert!(p.x > 1.0 && p.is_finite(), "walks: {p}");

    // A bad value during a running move ends the move.
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.swim.drag = 6.0;
    }
    app.update();
    assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.mantle.speed = f32::INFINITY;
    }
    app.update();
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::None, "the move ended");
    assert_eq!(t.last_refusal, Some(TraversalRefusal::TraversalBadSettings));
    assert_ne!(state(&app, c).mode, MovementMode::Swimming);
}

#[test]
fn root_motion_piled_up_during_a_move_is_not_replayed() {
    use regia_animation::RootMotionOutput;
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    app.world_mut()
        .entity_mut(c)
        .insert((RootMotionDriven::default(), RootMotionOutput::default()));
    let (kind, _) = attempt(&mut app, c);
    assert_eq!(kind, TraversalKind::Mantle);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let done = run_until(&mut app, 300, |app| traversal(app, c).kind == TraversalKind::None);
    assert!(done);
    let (_, _, seconds) = app
        .world_mut()
        .get_mut::<RootMotionOutput>(c)
        .expect("output")
        .take();
    assert_eq!(seconds, 0.0, "nothing left over");
}

#[test]
fn the_probe_tells_what_the_capsule_could_use_and_changes_nothing() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(4.3, REST_Y, 0.0));
    run(&mut app, 5);
    let before = traversal(&app, c);
    let at = position(&app, c);
    let probe = probe_traversal(app.world_mut(), c, None, Some(Vec3::X)).expect("probe");
    assert_eq!(probe.water_surface, None);
    assert!(!probe.chest_under);
    let ladder = probe.ladder.expect("the ladder is in reach");
    assert_eq!((ladder.user_id, ladder.attach), (77, "bottom"));
    // The wall ahead is 3 m high: too high to mantle.
    assert_eq!(probe.ledge.err(), Some(TraversalRefusal::LedgeTooHigh));
    assert_eq!(traversal(&app, c), before);
    assert_eq!(position(&app, c), at);
    // Asked about another place: nothing there.
    let far = probe_traversal(
        app.world_mut(),
        c,
        Some(Vec3::new(-20.0, REST_Y, 0.0)),
        Some(Vec3::X),
    )
    .expect("probe");
    assert_eq!(far.ladder.err(), Some(TraversalRefusal::LadderNoneInReach));
    assert_eq!(far.ledge.err(), Some(TraversalRefusal::LedgeNoWall));
}

#[test]
fn a_non_finite_stick_never_reaches_a_position() {
    // In the water.
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::new(f32::NAN, f32::INFINITY, 0.0)
    });
    run(&mut app, 30);
    let at = position(&app, c);
    assert!(at.is_finite() && state(&app, c).velocity.is_finite(), "{at}");
    assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);

    // On a ladder.
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    set_input(&mut app, c, |i| i.move_dir = Vec3::splat(f32::NAN));
    let held = position(&app, c);
    run(&mut app, 30);
    assert_eq!(position(&app, c), held, "holds still");
    assert_eq!(traversal(&app, c).kind, TraversalKind::Ladder);
}

/// A move's mode with no move running (a host wrote it, a save restored only the settings): the
/// character is not stuck in it.
#[test]
fn a_stale_move_mode_with_no_move_running_lets_go() {
    for mode in [
        MovementMode::Swimming,
        MovementMode::CLIMBING,
        MovementMode::TRAVERSING,
    ] {
        let mut app = base_app();
        floor(&mut app, 0.0);
        let c = spawn_character(&mut app, Vec3::new(0.0, 3.0, 0.0));
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(c) {
            movement.mode = mode;
        }
        run(&mut app, 120);
        let s = state(&app, c);
        assert!(s.grounded && s.mode == MovementMode::Walking, "{mode:?}: {s:?}");
        assert!((position(&app, c).y - REST_Y).abs() < 0.08, "{mode:?}");
    }
}

/// The same without settings: only the reserved modes change, `Swimming` keeps its old meaning.
#[test]
fn with_no_settings_swimming_keeps_gravity_and_a_reserved_mode_falls() {
    let run_with = |mode| {
        let mut app = base_app();
        floor(&mut app, 0.0);
        let c = spawn_character(&mut app, Vec3::new(0.0, 3.0, 0.0));
        app.world_mut().entity_mut(c).remove::<TraversalSettings>();
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(c) {
            movement.mode = mode;
        }
        run(&mut app, 120);
        (position(&app, c), state(&app, c))
    };
    let (at, s) = run_with(MovementMode::Swimming);
    assert!((at.y - REST_Y).abs() < 0.08, "gravity as before: {at}");
    assert_eq!(s.mode, MovementMode::Swimming, "the mode stays, as before");
    let (at, s) = run_with(MovementMode::CLIMBING);
    assert!((at.y - REST_Y).abs() < 0.08, "falls: {at}");
    assert_eq!(s.mode, MovementMode::Walking);
}

/// Below the dive limit, a release rises gently: the limit only stops a dive.
#[test]
fn a_swimmer_below_the_dive_limit_rises_at_the_rise_speed() {
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 2000, |app| position(app, c).x >= 12.0));
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.crouch = true;
    });
    run(&mut app, 80);
    assert!(position(&app, c).y < FLOAT_Y - 1.5, "{}", position(&app, c));
    // The limit shrinks to less than the depth it is at (or it got there by a teleport).
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.swim.max_dive_depth = 0.5;
    }
    set_input(&mut app, c, |i| i.crouch = false);
    let rise = TraversalSettings::default().swim.rise_speed;
    for _ in 0..240 {
        app.update();
        let s = state(&app, c);
        assert!(s.velocity.y <= rise + 0.01, "rises gently: {:?}", s.velocity);
        assert_eq!(s.mode, MovementMode::Swimming, "never launched out");
    }
    assert!(
        (position(&app, c).y - FLOAT_Y).abs() <= 0.05,
        "{}",
        position(&app, c)
    );
    // Held under its limit, a dive does not pull it back up.
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.swim.max_dive_depth = 0.25;
    }
    set_input(&mut app, c, |i| i.crouch = true);
    run(&mut app, 120);
    assert!(
        (position(&app, c).y - (FLOAT_Y - 0.25)).abs() < 0.02,
        "{}",
        position(&app, c)
    );
}

/// A climber moved off the line (a respawn, a correction) stays where it was put.
#[test]
fn a_climber_moved_off_the_ladder_stays_off_it() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    run(&mut app, 30);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let moved = position(&app, c) - Vec3::new(2.0, 0.0, 0.0);
    app.world_mut()
        .get_mut::<Transform>(c)
        .expect("transform")
        .translation = moved;
    app.update();
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::None);
    assert_eq!(t.last_refusal, Some(TraversalRefusal::TraversalInterrupted));
    assert_eq!(state(&app, c).mode, MovementMode::Falling);
    assert!(
        (position(&app, c).x - moved.x).abs() < 0.01,
        "not pulled back: {}",
        position(&app, c)
    );
}

/// The ladder at a yaw that lines up with no axis, block and all: attach, climb, exit.
#[test]
fn a_ladder_at_any_yaw_works_the_same() {
    let yaw = 0.7_f32;
    let rotation = Quat::from_rotation_y(yaw);
    let into = rotation * Vec3::NEG_Z;
    let mut app = base_app();
    floor(&mut app, 0.0);
    let face = Vec3::new(1.0, 0.0, -2.0);
    static_box(
        &mut app,
        Vec3::new(6.0, UPPER * 0.5, 4.0),
        face + into * 4.0 + Vec3::Y * (UPPER * 0.5),
        rotation,
    );
    app.world_mut().spawn((
        Transform {
            translation: face,
            rotation,
            ..Default::default()
        },
        Ladder {
            height: UPPER,
            ..Ladder::default()
        },
    ));
    let c = spawn_character(&mut app, face - into * 1.5 + Vec3::Y * REST_Y);
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = into);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    let facing = app.world().get::<Transform>(c).expect("transform").rotation * Vec3::NEG_Z;
    assert!(
        (facing - into).length() < 1.0e-3,
        "faces into the ladder: {facing}"
    );
    let line = face - into * Ladder::default().standoff;
    assert!(
        position(&app, c).with_y(0.0).distance(line.with_y(0.0)) < 0.01,
        "{}",
        position(&app, c)
    );
    let mut hit = false;
    let done = run_until(&mut app, 800, |app| {
        hit |= embedded(app, c, position(app, c));
        traversal(app, c).kind == TraversalKind::None && state(app, c).grounded
    });
    assert!(done, "{}", position(&app, c));
    assert!(!hit, "never embedded");
    let at = position(&app, c);
    assert!((at - face).dot(into) > 0.0, "past the face: {at}");
    assert!((at.y - (UPPER + REST_Y)).abs() < 0.08, "on the upper floor: {at}");
}

/// A mantle at a diagonal approach to a turned wall.
#[test]
fn a_mantle_at_an_angle_stands_on_a_turned_block() {
    let rotation = Quat::from_rotation_y(0.5);
    let normal_in = rotation * Vec3::X;
    let mut app = base_app();
    floor(&mut app, 0.0);
    let center = Vec3::new(4.0, 0.75, 0.0);
    static_box(&mut app, Vec3::new(2.0, 0.75, 6.0), center, rotation);
    let face = center - normal_in * 2.0;
    let c = spawn_character(&mut app, (face - normal_in * 0.5).with_y(REST_Y));
    run(&mut app, 10);
    let approach = (normal_in + Vec3::Z * 0.3).normalize();
    set_input(&mut app, c, |i| {
        i.move_dir = approach;
        i.jump = true;
    });
    app.update();
    assert_eq!(
        traversal(&app, c).kind,
        TraversalKind::Mantle,
        "{:?}",
        traversal(&app, c)
    );
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    let mut hit = false;
    assert!(run_until(&mut app, 300, |app| {
        hit |= embedded(app, c, position(app, c));
        traversal(app, c).kind == TraversalKind::None
    }));
    assert!(!hit, "never embedded");
    let at = position(&app, c);
    assert!((at - face).dot(normal_in) > 0.0, "past the face: {at}");
    assert!((at.y - (1.5 + REST_Y)).abs() < 0.08, "on the block: {at}");
}

/// Collects every traversal event of a run.
fn event_log(app: &mut App, c: Entity, steps: usize, log: &mut Vec<TraversalEvent>) {
    let mut cursor = app.world().resource::<Messages<TraversalEvent>>().get_cursor();
    for _ in 0..steps {
        app.update();
        let messages = app.world().resource::<Messages<TraversalEvent>>();
        log.extend(cursor.read(messages).cloned());
        let _ = c;
    }
}

/// Checks that starts and finishes pair up in order, with nothing nested.
fn assert_balanced(log: &[TraversalEvent]) {
    let mut open: Option<TraversalKind> = None;
    for event in log {
        match event {
            TraversalEvent::Started { kind, .. } => {
                assert_eq!(open, None, "{kind:?} started inside {open:?}: {log:#?}");
                open = Some(*kind);
            }
            TraversalEvent::Finished { kind, .. } => {
                assert_eq!(open, Some(*kind), "{log:#?}");
                open = None;
            }
            TraversalEvent::Refused { .. } => {}
        }
    }
    assert_eq!(open, None, "still open at the end: {log:#?}");
}

fn kinds_started(log: &[TraversalEvent]) -> Vec<TraversalKind> {
    log.iter()
        .filter_map(|event| match event {
            TraversalEvent::Started { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

#[test]
fn every_start_is_finished_in_order_up_a_ladder_and_off_it() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut log = Vec::new();
    event_log(&mut app, c, 300, &mut log);
    assert_balanced(&log);
    assert_eq!(
        kinds_started(&log),
        [
            TraversalKind::LadderEnter,
            TraversalKind::Ladder,
            TraversalKind::LadderExit
        ],
        "{log:#?}"
    );

    // A jump off and a walk off the bottom close the climb too.
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut log = Vec::new();
    event_log(&mut app, c, 80, &mut log);
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.jump = true;
    });
    event_log(&mut app, c, 90, &mut log);
    assert_balanced(&log);

    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut log = Vec::new();
    event_log(&mut app, c, 70, &mut log);
    set_input(&mut app, c, |i| i.move_dir = Vec3::NEG_X);
    event_log(&mut app, c, 200, &mut log);
    assert_balanced(&log);
    assert_eq!(state(&app, c).mode, MovementMode::Walking);
}

#[test]
fn every_start_is_finished_when_a_move_is_cut_off() {
    // A shove during a mantle.
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    let mut log = Vec::new();
    event_log(&mut app, c, 5, &mut log);
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    app.world_mut()
        .get_mut::<Transform>(c)
        .expect("transform")
        .translation
        .x -= 0.5;
    event_log(&mut app, c, 60, &mut log);
    assert_balanced(&log);
    assert_eq!(kinds_started(&log), [TraversalKind::Mantle]);

    // Bad settings during a swim.
    let mut app = base_app();
    let c = pool_character(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    let mut log = Vec::new();
    event_log(&mut app, c, 300, &mut log);
    assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.swim.drag = f32::NAN;
    }
    event_log(&mut app, c, 5, &mut log);
    assert_balanced(&log);
}

#[test]
fn a_mantle_out_of_the_water_closes_the_swim_first() {
    let mut app = base_app();
    floor(&mut app, -3.0);
    // A dock whose top is 0.3 m above the water at x = 2.
    static_box(
        &mut app,
        Vec3::new(5.0, 1.65, 20.0),
        Vec3::new(7.0, -1.35, 0.0),
        Quat::IDENTITY,
    );
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 0.0,
        min: [-20.0, -20.0],
        max: [20.0, 20.0],
    })));
    let c = spawn_character(&mut app, Vec3::new(1.4, -0.5, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<CharacterState>(c) {
        s.grounded = false;
    }
    let mut log = Vec::new();
    event_log(&mut app, c, 60, &mut log);
    assert_eq!(
        traversal(&app, c).kind,
        TraversalKind::Swim,
        "{}",
        position(&app, c)
    );
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    event_log(&mut app, c, 120, &mut log);
    assert_balanced(&log);
    assert_eq!(
        kinds_started(&log),
        [TraversalKind::Swim, TraversalKind::Mantle],
        "{log:#?}"
    );
    let at = position(&app, c);
    assert!(
        at.x > 2.0 && (at.y - (0.3 + REST_Y)).abs() < 0.08,
        "on the dock: {at}"
    );
}

/// A character's own child colliders (hitboxes) are not obstacles.
#[test]
fn a_characters_own_child_colliders_do_not_block_its_moves() {
    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    let hitbox = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.2),
                },
                ..Collider::default()
            },
            ChildOf(c),
        ))
        .id();
    let (kind, refusal) = attempt(&mut app, c);
    assert_eq!((kind, refusal), (TraversalKind::Mantle, None));
    let _ = hitbox;
    set_input(&mut app, c, |i| i.move_dir = Vec3::ZERO);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind == TraversalKind::None));
    assert_eq!(traversal(&app, c).last_refusal, None, "no self-interruption");
    assert!(position(&app, c).x > 2.0, "{}", position(&app, c));
}

/// With two jumps allowed, the press that leaves a ladder is not also an air jump.
#[test]
fn the_press_that_leaves_a_ladder_or_the_water_is_spent() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    if let Some(mut m) = app.world_mut().get_mut::<CharacterMovement>(c) {
        m.max_jumps = 2;
    }
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    run(&mut app, 30);
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.jump = true;
    });
    app.update();
    let off = TraversalSettings::default().ladder.jump_off_up;
    let vy = state(&app, c).velocity.y;
    assert!(vy <= off + 0.01, "the jump off's speed, not a jump's: {vy}");
    assert!(
        state(&app, c).jump_buffer_time_left == 0.0,
        "and nothing buffered"
    );
}

/// An order during a climb is dropped and reported; it does not fire when the climb ends.
#[test]
fn an_order_during_a_climb_is_refused_and_never_fires_late() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    let mut t = traversal(&app, c);
    t.request(TraversalOrder::Jump {
        to: Vec3::new(-5.0, 0.0, 0.0),
        arc_height: 1.0,
    });
    *app.world_mut()
        .get_mut::<CharacterTraversal>(c)
        .expect("traversal") = t;
    app.update();
    let t = traversal(&app, c);
    assert_eq!(t.pending, None);
    assert_eq!(t.last_refusal, Some(TraversalRefusal::TraversalBusy));
    assert_eq!(t.kind, TraversalKind::Ladder);
}

/// A ladder order needs the character near the ladder, not only the crossing's start.
#[test]
fn a_ladder_order_for_a_far_character_is_refused() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(-30.0, REST_Y, 0.0));
    run(&mut app, 10);
    let mut t = traversal(&app, c);
    t.request(TraversalOrder::Ladder {
        user_id: 77,
        near: Vec3::new(WALL_X - 1.0, 0.0, 0.0),
        up: true,
    });
    *app.world_mut()
        .get_mut::<CharacterTraversal>(c)
        .expect("traversal") = t;
    app.update();
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::None);
    assert_eq!(t.last_refusal, Some(TraversalRefusal::LadderNoneInReach));
}

#[test]
fn switched_off_moves_refuse_requests_and_probes() {
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(4.3, REST_Y, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.mantle.enabled = false;
        s.ladder.enabled = false;
    }
    run(&mut app, 5);
    let probe = probe_traversal(app.world_mut(), c, None, Some(Vec3::X)).expect("probe");
    assert_eq!(probe.ledge.err(), Some(TraversalRefusal::TraversalDisabled));
    assert_eq!(probe.ladder.err(), Some(TraversalRefusal::TraversalDisabled));
    for order in [
        TraversalOrder::Mantle { forward: Vec3::X },
        TraversalOrder::Ladder {
            user_id: 77,
            near: Vec3::new(4.3, 0.0, 0.0),
            up: true,
        },
    ] {
        let mut t = traversal(&app, c);
        t.last_refusal = None;
        t.request(order);
        *app.world_mut()
            .get_mut::<CharacterTraversal>(c)
            .expect("traversal") = t;
        app.update();
        assert_eq!(
            traversal(&app, c).last_refusal,
            Some(TraversalRefusal::TraversalDisabled),
            "{order:?}"
        );
        assert_eq!(traversal(&app, c).kind, TraversalKind::None);
    }
}

/// A broken state written by something other than the step ends the move; it never panics or reaches
/// the `Transform`.
#[test]
fn a_broken_state_ends_the_move_without_a_panic_or_a_nan() {
    let line = regia_framework::traversal::LadderLine {
        line: Vec3::new(1.0, 1.2, 0.0),
        into: Vec3::X,
        top_y: f32::NAN,
        speed: 1.0,
        exit: Vec3::ZERO,
        id: 0,
    };
    let broken = [
        CharacterTraversal {
            kind: TraversalKind::Ladder,
            ladder: line,
            ..CharacterTraversal::default()
        },
        CharacterTraversal {
            kind: TraversalKind::Mantle,
            steps: 5,
            path: [Vec3::ZERO, Vec3::new(0.0, 1.0e30, 0.0), Vec3::ZERO, Vec3::ZERO],
            ..CharacterTraversal::default()
        },
        CharacterTraversal {
            kind: TraversalKind::Jump,
            step: u32::MAX,
            steps: u32::MAX,
            ..CharacterTraversal::default()
        },
    ];
    for bad in broken {
        let mut app = base_app();
        floor(&mut app, 0.0);
        let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0));
        run(&mut app, 5);
        *app.world_mut()
            .get_mut::<CharacterTraversal>(c)
            .expect("traversal") = bad;
        run(&mut app, 30);
        let t = traversal(&app, c);
        assert_eq!(t.kind, TraversalKind::None, "{bad:?}");
        assert_eq!(t.last_refusal, Some(TraversalRefusal::TraversalInterrupted));
        assert!(position(&app, c).is_finite());
        assert!(
            state(&app, c).grounded,
            "back on the floor: {:?}",
            position(&app, c)
        );
    }
}

/// A floating-origin rebase in the middle of a move shifts the world and the state's frame together:
/// neither the curve nor the ladder yanks the character.
#[test]
fn an_origin_rebase_during_a_move_does_not_yank_the_character() {
    use regia_scene::streaming::OriginOffset;

    let shift = Vec3::new(1024.0, 0.0, -512.0);
    let rebase = |app: &mut App, c: Entity| {
        // What the streaming runtime does: every local position moves, and the offset records it.
        let world = app.world_mut();
        let mut q = world.query::<&mut Transform>();
        for mut transform in q.iter_mut(world) {
            transform.translation -= shift;
        }
        let at = world.get_resource::<OriginOffset>().map_or([0.0; 3], |o| o.0);
        world.insert_resource(OriginOffset([
            at[0] + f64::from(shift.x),
            at[1] + f64::from(shift.y),
            at[2] + f64::from(shift.z),
        ]));
        let _ = c;
    };

    // Halfway up a ladder.
    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    run(&mut app, 10);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    assert!(run_until(&mut app, 300, |app| traversal(app, c).kind
        == TraversalKind::Ladder));
    run(&mut app, 20);
    let before = position(&app, c);
    rebase(&mut app, c);
    run(&mut app, 2);
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::Ladder, "still on it: {t:?}");
    let after = position(&app, c);
    assert!(
        (after + shift - before).length() < 0.2,
        "no yank: {before} {after}"
    );
    assert_eq!(t.frame, [1024.0, 0.0, -512.0]);
}

/// The acceptance's "a re-run gives the same result" at the bit level, with no tolerance in the way: at
/// every step of each move, the step runs twice from the same state and both answers are the same bits.
#[test]
fn every_step_of_every_move_reruns_to_the_same_bits() {
    use regia_framework::character::{CharacterCollisionQuery, step_character};

    type Snapshot = (Transform, CharacterState, CharacterMovement, CharacterTraversal);
    fn snapshot(app: &App, c: Entity) -> Snapshot {
        let world = app.world();
        (
            *world.get::<Transform>(c).expect("transform"),
            world.get::<CharacterState>(c).expect("state").clone(),
            world.get::<CharacterMovement>(c).expect("movement").clone(),
            *world.get::<CharacterTraversal>(c).expect("traversal"),
        )
    }
    fn restore(app: &mut App, c: Entity, s: &Snapshot) {
        let world = app.world_mut();
        *world.get_mut::<Transform>(c).expect("transform") = s.0;
        *world.get_mut::<CharacterState>(c).expect("state") = s.1.clone();
        *world.get_mut::<CharacterMovement>(c).expect("movement") = s.2.clone();
        *world.get_mut::<CharacterTraversal>(c).expect("traversal") = s.3;
    }
    fn bits(s: &Snapshot) -> (Vec<u32>, String, String, String) {
        let t = &s.0;
        let mut v: Vec<u32> = t.translation.to_array().iter().map(|f| f.to_bits()).collect();
        v.extend(t.rotation.to_array().iter().map(|f| f.to_bits()));
        v.extend(s.1.velocity.to_array().iter().map(|f| f.to_bits()));
        v.extend(s.3.path.iter().flat_map(|p| p.to_array()).map(f32::to_bits));
        v.push(s.3.depth.to_bits());
        v.push(s.3.step);
        (
            v,
            format!("{:?}", s.1),
            format!("{:?}", s.2.mode),
            format!("{:?}", s.3),
        )
    }
    /// Steps the character itself, twice per step, from the same start.
    fn drive(app: &mut App, c: Entity, steps: usize, input: &CharacterInput, seen: &mut Vec<TraversalKind>) {
        for _ in 0..steps {
            // The world moves on by the plain route, so physics is current; the character is marked
            // so the system leaves it alone.
            app.update();
            let start = snapshot(app, c);
            let dt = DT as f32;
            let query = app
                .world_mut()
                .remove_resource::<CharacterCollisionQuery>()
                .expect("backend");
            step_character(app.world_mut(), query.query.as_ref(), c, input, dt);
            let first = snapshot(app, c);
            restore(app, c, &start);
            step_character(app.world_mut(), query.query.as_ref(), c, input, dt);
            let second = snapshot(app, c);
            app.world_mut().insert_resource(query);
            assert_eq!(bits(&first), bits(&second), "a re-run differs");
            if first.3.kind != TraversalKind::None && !seen.contains(&first.3.kind) {
                seen.push(first.3.kind);
            }
        }
    }
    let stepped = regia_framework::character::CharacterSteppedExternally;
    let forward = CharacterInput {
        move_dir: Vec3::X,
        ..CharacterInput::default()
    };
    let mut seen = Vec::new();

    let mut app = base_app();
    let c = pool_character(&mut app);
    app.world_mut().entity_mut(c).insert(stepped.clone());
    drive(&mut app, c, 1400, &forward, &mut seen);
    assert!(seen.contains(&TraversalKind::Swim), "{seen:?}");

    let mut app = base_app();
    ladder_rig(&mut app);
    let c = spawn_character(&mut app, Vec3::new(3.0, REST_Y, 0.0));
    app.world_mut().entity_mut(c).insert(stepped.clone());
    drive(&mut app, c, 400, &forward, &mut seen);
    for kind in [
        TraversalKind::LadderEnter,
        TraversalKind::Ladder,
        TraversalKind::LadderExit,
    ] {
        assert!(seen.contains(&kind), "{kind:?}: {seen:?}");
    }

    let mut app = base_app();
    ledge_rig(&mut app, 1.5);
    let c = at_wall(&mut app);
    app.world_mut().entity_mut(c).insert(stepped);
    let press = CharacterInput {
        move_dir: Vec3::X,
        jump: true,
        ..CharacterInput::default()
    };
    drive(&mut app, c, 1, &press, &mut seen);
    drive(&mut app, c, 120, &CharacterInput::default(), &mut seen);
    assert!(seen.contains(&TraversalKind::Mantle), "{seen:?}");
}
