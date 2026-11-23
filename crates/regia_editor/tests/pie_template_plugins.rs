// SPDX-License-Identifier: MPL-2.0
//! Play in editor with a game whose `register` adds the engine plugins itself, the way the
//! shipped `third_person` and `top_down` templates do (`ScenePlugin`, `PhysicsPlugin`, the
//! character, camera, script, save and input plugins). PIE pre-adds `ScenePlugin` and spawns the
//! edit level into the play world before it calls the game's `register`; the game's own systems
//! must still run, also over a level that holds colliders (ADR-9180).

use std::sync::atomic::{AtomicU32, Ordering};

use regia::physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack};
use regia_editor::level_io;
use regia_editor::pie::{CurrentGame, PieSession, start_play, stop_play};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_framework::camera::CameraPlugin;
use regia_framework::character::CharacterPlugin;
use regia_framework::game::GameExports;
use regia_framework::save_flow::SaveFlowPlugin;
use regia_framework::save_game::SavePlugin;
use regia_framework::script::ScriptPlugin;
use regia_input::prelude::{ActionKind, Binding, InputConfig, InputPlugin, InputSource};
use regia_scene::prelude::{Name, Transform};

/// Frames the game's `Update` system ran, across every test in this binary (each test reads
/// its own delta; tests in one binary may run in parallel, so each uses its own counter).
static PLAIN_FRAMES: AtomicU32 = AtomicU32::new(0);
static COLLIDER_FRAMES: AtomicU32 = AtomicU32::new(0);
static TWICE_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Startup systems that ran (the template spawns its level and character in `Startup`).
#[derive(Resource, Default)]
struct StartupRan(u32);

/// The plugin tuple of `templates/third_person/game/src/lib.rs` `Game::build`, minus the
/// template-private ones (game flow, settings and sound need the template's own crate).
fn add_template_plugins(app: &mut App) {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(Binding::new("Jump", InputSource::key("Space")));
    app.add_plugins((
        regia_scene::ScenePlugin,
        PhysicsPlugin,
        CharacterPlugin,
        CameraPlugin,
        ScriptPlugin,
        SavePlugin::default(),
        SaveFlowPlugin,
        InputPlugin::new(config),
    ));
}

/// The template's `setup_level`: a static ground body with a box collider, spawned in `Startup`.
fn setup_level(mut commands: Commands, mut ran: ResMut<StartupRan>) {
    ran.0 += 1;
    commands.spawn((
        Name::new("TemplateGround"),
        Transform::default(),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
}

fn count_plain(_: Res<Time>) {
    PLAIN_FRAMES.fetch_add(1, Ordering::SeqCst);
}

fn count_collider(_: Res<Time>) {
    COLLIDER_FRAMES.fetch_add(1, Ordering::SeqCst);
}

fn count_twice(_: Res<Time>) {
    TWICE_FRAMES.fetch_add(1, Ordering::SeqCst);
}

fn register_plain(app: &mut App) {
    add_template_plugins(app);
    app.init_resource::<StartupRan>()
        .add_systems(Startup, setup_level)
        .add_systems(Update, count_plain);
}

fn register_collider(app: &mut App) {
    add_template_plugins(app);
    app.init_resource::<StartupRan>()
        .add_systems(Startup, setup_level)
        .add_systems(Update, count_collider);
}

/// A game that adds the plugins twice itself (two of its own sub-plugins each add them).
fn register_twice(app: &mut App) {
    add_template_plugins(app);
    add_template_plugins(app);
    app.init_resource::<StartupRan>()
        .add_systems(Startup, setup_level)
        .add_systems(Update, count_twice);
}

fn on_unload(_: &mut World) {}
fn on_load(_: &mut World) {}

fn game(register: fn(&mut App)) -> CurrentGame {
    CurrentGame {
        exports: GameExports {
            register,
            on_unload,
            on_load,
        },
        generation: 1,
        package: "pie_template_plugins_test".to_owned(),
    }
}

/// The edit world the editor binary builds (scene + editor resources), with an empty level.
fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>();
    regia_editor::mesh_source::register_level_types(app.world_mut());
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.update();
    app
}

/// Places an entity through the editor's own spawn path (undoable, guid-tagged, level-member)
/// and gives it a collider, as the editor's collider handles do (ADR-2696).
fn place_collider(app: &mut App, name: &str) {
    let mut command = SpawnEntities::new_entity(
        app.world(),
        Guid::new_random(),
        name,
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
    )
    .expect("spawn");
    command.apply(app.world_mut());
    let world = app.world_mut();
    let mut names = world.query::<(Entity, &Name)>();
    let entity = names
        .iter(world)
        .find(|(_, found)| found.as_str() == name)
        .map(|(entity, _)| entity)
        .expect("placed");
    world.entity_mut(entity).insert(Collider {
        shape: ColliderShape::Box {
            half_extents: Vec3::new(5.0, 0.5, 5.0),
        },
        ..Collider::default()
    });
}

fn run_frames(session: &mut PieSession, frames: u32) {
    for _ in 0..frames {
        assert_eq!(session.frame(), None, "the play world must not panic");
    }
}

fn assert_game_ran(session: &mut PieSession, counter: &AtomicU32, before: u32) {
    assert_eq!(session.panic, None, "no panic during play");
    let ran = counter.load(Ordering::SeqCst) - before;
    assert!(
        ran >= 4,
        "the game's Update system must run every play frame (ran {ran} of {} frames): its \
         register was abandoned",
        session.frames
    );
    let world = session.app.world_mut();
    assert_eq!(
        world.get_resource::<StartupRan>().map(|r| r.0),
        Some(1),
        "the game's Startup system ran exactly once"
    );
    assert!(
        world.contains_resource::<regia::physics::PhysicsWorld>(),
        "the physics plugin is in the play world"
    );
    let mut bodies = world.query::<(&Name, &RigidBody)>();
    let grounds = bodies
        .iter(world)
        .filter(|(name, _)| name.as_str() == "TemplateGround")
        .count();
    assert_eq!(grounds, 1, "the game's level body was spawned once");
}

#[test]
fn template_game_registers_in_play_with_an_empty_level() {
    let mut edit = edit_app();
    let before = PLAIN_FRAMES.load(Ordering::SeqCst);
    let mut session = start_play(edit.world_mut(), Some(game(register_plain))).expect("play");
    run_frames(&mut session, 4);
    assert_game_ran(&mut session, &PLAIN_FRAMES, before);
    stop_play(session);
}

#[test]
fn template_game_registers_in_play_over_a_level_with_colliders() {
    // An editor-authored level with collider content (ADR-2696): the play world holds live
    // `Collider`s before the game's `register` runs.
    let mut edit = edit_app();
    place_collider(&mut edit, "AuthoredFloor");
    let before = COLLIDER_FRAMES.load(Ordering::SeqCst);
    let mut session = start_play(edit.world_mut(), Some(game(register_collider))).expect("play");
    run_frames(&mut session, 4);
    assert_game_ran(&mut session, &COLLIDER_FRAMES, before);
    let world = session.app.world_mut();
    let mut query = world.query::<(Entity, &Name, &Collider)>();
    let colliders: Vec<(Entity, String)> = query
        .iter(world)
        .map(|(entity, name, _)| (entity, name.as_str().to_owned()))
        .collect();
    assert_eq!(colliders.len(), 2, "level floor + game ground: {colliders:?}");
    let find = |name: &str| {
        colliders
            .iter()
            .find(|(_, found)| found == name)
            .map(|(entity, _)| *entity)
    };
    let floor = find("AuthoredFloor").expect("the authored collider reached the play world");
    let ground = find("TemplateGround").expect("the game's ground");
    // ADR-1347 holds in play: the level's collider is numbered as it spawned, before the
    // game's own body, so rapier inserts them in that order on every run.
    let order = world.resource::<regia_ecs::spawn_order::SpawnOrder<Collider>>();
    assert!(order.is_tracking());
    let (Some(floor_n), Some(ground_n)) = (order.get(floor), order.get(ground)) else {
        panic!(
            "both colliders are numbered: {:?} {:?}",
            order.get(floor),
            order.get(ground)
        );
    };
    assert!(
        floor_n < ground_n,
        "level content first ({floor_n} vs {ground_n})"
    );
    stop_play(session);
}

static NO_PHYSICS_FRAMES: AtomicU32 = AtomicU32::new(0);

fn count_no_physics(_: Res<Time>) {
    NO_PHYSICS_FRAMES.fetch_add(1, Ordering::SeqCst);
}

/// A game that never adds physics.
fn register_without_physics(app: &mut App) {
    app.add_plugins(regia_scene::ScenePlugin)
        .add_systems(Update, count_no_physics);
}

#[test]
fn play_adds_no_physics_behind_a_game_that_has_none() {
    let mut edit = edit_app();
    place_collider(&mut edit, "AuthoredFloor");
    let before = NO_PHYSICS_FRAMES.load(Ordering::SeqCst);
    let mut session = start_play(edit.world_mut(), Some(game(register_without_physics))).expect("play");
    run_frames(&mut session, 4);
    assert_eq!(session.panic, None);
    assert!(
        NO_PHYSICS_FRAMES.load(Ordering::SeqCst) - before >= 4,
        "the game ran"
    );
    let world = session.app.world();
    assert!(
        !world.contains_resource::<regia::physics::PhysicsWorld>(),
        "only the arrival numbering is installed, no physics plugin"
    );
    assert!(!session.app.has_plugin(std::any::type_name::<PhysicsPlugin>()));
    stop_play(session);
}

#[test]
fn a_game_that_adds_the_template_plugins_twice_still_registers() {
    let mut edit = edit_app();
    place_collider(&mut edit, "AuthoredFloor");
    let before = TWICE_FRAMES.load(Ordering::SeqCst);
    let mut session = start_play(edit.world_mut(), Some(game(register_twice))).expect("play");
    run_frames(&mut session, 4);
    assert_game_ran(&mut session, &TWICE_FRAMES, before);
    stop_play(session);
}

/// Systems registered per schedule label, before the first update (the schedule graph).
fn system_counts(app: &App) -> Vec<(String, usize)> {
    let schedules = app.world().resource::<regia_ecs::ecs::schedule::Schedules>();
    let mut counts: Vec<(String, usize)> = schedules
        .iter()
        .map(|(label, schedule)| (format!("{label:?}"), schedule.systems_len()))
        .collect();
    counts.sort();
    counts
}

/// The contract both PIE and `regia::run_with` rely on (ADR-9180): adding `ScenePlugin` or
/// `PhysicsPlugin` a second time builds nothing (`regia_app::App::add_plugins` skips a
/// duplicate name): no system is registered twice and no resource is reset.
#[test]
fn a_plain_app_that_adds_scene_and_physics_twice_builds_them_once() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((regia_scene::ScenePlugin, PhysicsPlugin));
    let systems = system_counts(&app);
    app.world_mut().resource_mut::<regia::physics::Gravity>().0.y = -1.62;
    let level = regia_scene::level::LevelId(Guid::new_random());
    app.world_mut()
        .resource_mut::<regia_scene::level::LevelLibrary>()
        .insert(
            regia_scene::level::Level::from_ron_str(level, "( format_version: 1, entities: [] )")
                .expect("parse"),
        );
    app.add_plugins((regia_scene::ScenePlugin, PhysicsPlugin));
    app.add_plugins(PhysicsPlugin);
    assert_eq!(system_counts(&app), systems, "no system registered twice");
    assert_eq!(
        app.world().resource::<regia::physics::Gravity>().0.y,
        -1.62,
        "the second add must not reset physics resources"
    );
    assert!(
        app.world()
            .resource::<regia_scene::level::LevelLibrary>()
            .contains(level),
        "the second add must not reset the scene's level library"
    );
    app.update();
    app.update();
    assert!(
        app.has_plugin(std::any::type_name::<PhysicsPlugin>()),
        "the first registration stays"
    );
}
