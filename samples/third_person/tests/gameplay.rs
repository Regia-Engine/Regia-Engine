// SPDX-License-Identifier: MIT-0
//! Headless end-to-end gameplay tests for `third_person` (M20): movement + jump through
//! the rapier controller, collectibles via trigger scripts, save/load restoring collected
//! state, foot-IK planning on the stairs, and the boom camera shortening at a wall.
//!
//! No window, no renderer: these run the same plugins the windowed sample runs minus
//! `RenderPlugin`'s surface path.

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::CharacterState;
use regia_framework::player::PlayerManager;
use third_person::{ThirdPersonSample, collectible};

/// A headless app with the sample's gameplay, one step per update.
fn app() -> App {
    let mut app = App::new();
    // Straight into play (the title screen is covered by tests/combat.rs), saves and settings in a
    // temporary directory.
    app.insert_resource(regia_framework::prelude::GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    })
    .insert_resource(third_person::SampleDataDir(regia_test_utils::unique_temp_dir(
        "tp-gameplay",
    )));
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    // Startup loads the Fox (cooked cache or not — tests tolerate its absence).
    app.update();
    app
}

/// The sample-state entity.
fn state_entity(app: &App) -> Entity {
    app.world()
        .get_resource::<collectible::StateHandle>()
        .and_then(|h| h.0)
        .expect("state handle wired at startup")
}

/// The character entity possessed by `player`.
fn character_of(app: &App, player: u8) -> Entity {
    let manager = app.world().resource::<PlayerManager>();
    let player = manager.player_entity(player).expect("player exists");
    let controller = app
        .world()
        .get::<regia_framework::player::Player>(player)
        .expect("Player component")
        .controller;
    app.world()
        .get::<regia_framework::player::PlayerController>(controller)
        .expect("controller")
        .possessed
}

/// The player 0 character entity.
fn character(app: &App) -> Entity {
    character_of(app, 0)
}

/// Holds a movement key (the same path `drive_input` reads) so tests drive the real input
/// chain instead of bypassing it.
fn hold(app: &mut App, key: &str, held: bool) {
    let mut keys = app
        .world_mut()
        .get_resource_or_insert_with(third_person::input::ActionKeys::default);
    if held {
        keys.held.insert(key.to_owned());
    } else {
        keys.held.remove(key);
    }
}

fn set_input(app: &mut App, move_dir: Vec3, jump: bool) {
    // Translate the world-space direction into the keys `drive_input` reads (the rig yaw is
    // level at start, so +X is KeyD, -Z is KeyW).
    hold(app, "KeyW", move_dir.z < -0.5);
    hold(app, "KeyS", move_dir.z > 0.5);
    hold(app, "KeyD", move_dir.x > 0.5);
    hold(app, "KeyA", move_dir.x < -0.5);
    hold(app, "Space", jump);
}

fn state(app: &App) -> CharacterState {
    app.world()
        .get::<CharacterState>(character(app))
        .cloned()
        .expect("character state")
}

fn sample_state(app: &App, entity: Entity) -> third_person::SampleState {
    app.world()
        .get::<third_person::SampleState>(entity)
        .copied()
        .expect("sample state")
}

fn translation(app: &App) -> Vec3 {
    app.world()
        .get::<regia_scene::Transform>(character(app))
        .expect("transform")
        .translation
}

#[test]
fn character_walks_and_jumps() {
    let mut app = app();
    // Let it settle onto the ground.
    for _ in 0..30 {
        app.update();
    }
    assert!(state(&app).grounded, "starts grounded");

    // Walk forward (+X) for a second.
    set_input(&mut app, Vec3::X, false);
    let start = translation(&app);
    for _ in 0..60 {
        app.update();
    }
    let walked = translation(&app) - start;
    assert!(walked.x > 3.0, "walked forward, moved {walked:?}");

    // Jump: airborne then lands.
    set_input(&mut app, Vec3::ZERO, true);
    let mut jumped = false;
    for _ in 0..5 {
        app.update();
        let s = state(&app);
        if !s.grounded || s.velocity.y > 0.0 {
            jumped = true;
            break;
        }
    }
    assert!(jumped, "jump fired");
    let mut landed = false;
    for _ in 0..120 {
        app.update();
        let s = state(&app);
        if s.grounded && s.landed_this_frame {
            landed = true;
            break;
        }
    }
    assert!(landed, "landed after the jump");
}

#[test]
fn character_climbs_the_stairs() {
    let mut app = app();
    for _ in 0..10 {
        app.update();
    }
    // Walk towards the stairs at x ≈ 8.
    set_input(&mut app, Vec3::X, false);
    // Last step (i = 4): top y = 1.15, x about 11.15–12.05, capsule centre ≈ 1.95.
    // y > 1.1 is still true on the first step. Require the top of the flight, then
    // the +X slope past it (surface ≈ 1.35 at x = 14.5, capsule above 2).
    let mut topped = false;
    let mut covered = false;
    for _ in 0..900 {
        app.update();
        let p = translation(&app);
        let on_top = p.x > 11.2 && p.x < 12.6 && p.y > 1.85;
        if on_top {
            topped = true;
        }
        if topped && p.x > 14.5 && p.y > 1.70 && state(&app).grounded {
            covered = true;
            break;
        }
    }
    let p = translation(&app);
    assert!(topped, "reached the top of the flight, at {p:?}");
    assert!(
        covered,
        "continued onto the slope, at {p:?} grounded={}",
        state(&app).grounded
    );
}

#[test]
fn collectibles_collect_via_trigger_scripts_and_count() {
    let mut app = app();
    for _ in 0..30 {
        app.update();
    }
    let state_entity = state_entity(&app);
    let total = sample_state(&app, state_entity).total;
    assert_eq!(total, 8, "eight collectibles spawned");

    // Walk into the first collectible at (0, 1, -3): teleport next to it first (the walk
    // there is covered by the movement tests).
    let entity = character(&app);
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::Transform>(entity) {
        transform.translation = Vec3::new(-1.5, 1.2, -3.0);
    }
    set_input(&mut app, Vec3::X, false);
    let mut collected = 0;
    for _ in 0..600 {
        app.update();
        collected = sample_state(&app, state_entity).collected;
        if collected > 0 {
            break;
        }
    }
    assert!(collected >= 1, "trigger script collected a coin");
    let progress = app.world().resource::<collectible::CollectProgress>();
    assert_eq!(progress.collected.len(), collected as usize);
    let hud = app
        .world()
        .resource::<third_person::hud::HudText>()
        .0
        .expect("hud counter");
    let text = app
        .world()
        .get::<regia_ui::text::Text>(hud)
        .expect("counter text");
    let shown = match &text.spans[0].content {
        regia_ui::text::TextContent::Lit(value) => value.clone(),
        other => panic!("counter is not literal text: {other:?}"),
    };
    assert_eq!(shown, format!("Coins: {collected} / {total}"), "HUD string");
}

#[test]
fn save_and_load_restores_collected_state() {
    let mut app = app();
    for _ in 0..30 {
        app.update();
    }
    let state_entity = state_entity(&app);

    // Collect one coin (teleport next to the first one first).
    let entity = character(&app);
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::Transform>(entity) {
        transform.translation = Vec3::new(-1.5, 1.2, -3.0);
    }
    set_input(&mut app, Vec3::X, false);
    for _ in 0..600 {
        app.update();
        if sample_state(&app, state_entity).collected >= 1 {
            break;
        }
    }
    let after_first = sample_state(&app, state_entity).collected;
    assert!(after_first >= 1);

    third_person::save::save_now(app.world_mut());
    // The save flow writes the slot at the end of the next frame.
    app.update();

    // Collect more (the second coin sits at (4, 1, -2)).
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::Transform>(entity) {
        transform.translation = Vec3::new(2.5, 1.2, -2.2);
    }
    for _ in 0..600 {
        app.update();
        if sample_state(&app, state_entity).collected >= 2 {
            break;
        }
    }
    assert!(
        sample_state(&app, state_entity).collected >= 2,
        "collected a second coin"
    );

    // Load: back to the saved state.
    third_person::save::load_now(app.world_mut());
    // Loaded at the end of the next frame, reconciled on the one after.
    app.update();
    app.update();
    assert_eq!(
        sample_state(&app, state_entity).collected,
        after_first,
        "collected state restored to the save"
    );
    let progress = app.world().resource::<collectible::CollectProgress>().clone();
    assert_eq!(progress.collected.len(), after_first as usize);
    // The collected entity is gone again (despawned by apply_progress).
    let live: Vec<_> = app
        .world_mut()
        .query_filtered::<&collectible::CollectibleId, ()>()
        .iter(app.world())
        .map(|id| id.0)
        .collect();
    assert_eq!(
        live.len(),
        8 - after_first as usize,
        "collected coins are despawned"
    );
}

#[test]
fn second_player_joins_split_screen() {
    let mut app = app();
    for _ in 0..5 {
        app.update();
    }
    // Join through the same helper F2 uses.
    app.world_mut().get_resource::<PlayerManager>().expect("manager");
    let before = app.world().resource::<PlayerManager>().count();
    assert_eq!(before, 1, "player 0 auto-joins");

    // Drive the join path directly (F2 writes a KeyboardInput message in the real app).
    app.world_mut().write_message(regia_platform::KeyboardInput {
        physical_key: "F2".into(),
        logical_key: "F2".into(),
        state: regia_platform::ButtonState::Pressed,
        repeat: false,
        window: Entity::PLACEHOLDER,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    // The layout is recomputed in PostUpdate; give the viewport sync a frame to catch up.
    app.update();
    app.update();

    let manager = app.world().resource::<PlayerManager>();
    assert_eq!(manager.count(), 2, "second player joined");
    assert_eq!(manager.layout.viewports.len(), 2, "split layout computed");
    assert_eq!(manager.layout.viewports[&0], (0.0, 0.0, 0.5, 1.0));
    assert_eq!(manager.layout.viewports[&1], (0.5, 0.0, 0.5, 1.0));

    // Both players have cameras, and the viewports were written into them.
    let viewports: Vec<[f32; 4]> = app
        .world_mut()
        .query_filtered::<&regia_render::Camera, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .map(|c| c.viewport)
        .collect();
    assert_eq!(viewports.len(), 2, "two player cameras");
    assert!(viewpoints_contain(&viewports, [0.0, 0.0, 0.5, 1.0]));
    assert!(viewpoints_contain(&viewports, [0.5, 0.0, 0.5, 1.0]));

    let splits: Vec<third_person::camera::SplitViewport> = app
        .world_mut()
        .query_filtered::<&third_person::camera::SplitViewport, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .copied()
        .collect();
    assert_eq!(splits.len(), 2);
    assert!(
        splits
            .iter()
            .any(|s| (s.x - 0.0).abs() < 1e-6 && (s.w - 0.5).abs() < 1e-6)
    );
    assert!(
        splits
            .iter()
            .any(|s| (s.x - 0.5).abs() < 1e-6 && (s.w - 0.5).abs() < 1e-6)
    );

    // Player 0's boom is the +Z ray through x = −4. Player 1 must not stand on it.
    let p1 = app
        .world()
        .get::<regia_scene::Transform>(character_of(&app, 1))
        .expect("player 1 transform")
        .translation;
    assert!(
        (p1.x + 4.0).abs() > 6.0,
        "player 1 is off player 0's +Z boom, at {p1:?}"
    );
}

fn viewpoints_contain(list: &[[f32; 4]], want: [f32; 4]) -> bool {
    list.iter()
        .any(|v| (v[0] - want[0]).abs() < 1e-6 && (v[2] - want[2]).abs() < 1e-6)
}

#[test]
fn character_spawns_at_player_start() {
    let mut app = app();
    for _ in 0..20 {
        app.update();
    }
    let p = translation(&app);
    assert!(
        (p.x + 4.0).abs() < 0.6,
        "player 0 starts at the first PlayerStart, at {p:?}"
    );
}

#[test]
fn pause_freezes_the_character() {
    let mut app = app();
    for _ in 0..20 {
        app.update();
    }
    set_input(&mut app, Vec3::X, false);
    app.world_mut().write_message(regia_platform::KeyboardInput {
        physical_key: "Escape".into(),
        logical_key: "Escape".into(),
        state: regia_platform::ButtonState::Pressed,
        repeat: false,
        window: Entity::PLACEHOLDER,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    let state_entity = state_entity(&app);
    assert!(sample_state(&app, state_entity).paused);
    assert!(app.world().resource::<Time>().paused);
    let frozen = translation(&app);
    for _ in 0..30 {
        app.update();
    }
    let after = translation(&app);
    assert!(
        (after - frozen).length() < 0.05,
        "paused character stayed put: {frozen:?} -> {after:?}"
    );
    let menu = app
        .world()
        .resource::<third_person::hud::PauseMenu>()
        .0
        .expect("pause menu");
    let style = app
        .world()
        .get::<regia_ui::node::NodeStyle>(menu)
        .expect("pause style");
    assert_eq!(
        style.0.display,
        regia_ui::style::Display::Flex,
        "pause menu is shown"
    );
}

#[test]
fn game_flow_tracks_the_pause_menu() {
    use regia_framework::flow::{GameFlow, InGame};

    let flow = |app: &App| *app.world().resource::<State<GameFlow>>().get();
    let mut app = app();
    // Boot (startup frame) → Loading → Playing.
    app.update();
    app.update();
    assert_eq!(flow(&app), GameFlow::Playing);
    assert!(app.world().get_resource::<State<InGame>>().is_some());

    // Esc pauses immediately and moves the flow to Paused on the next transition.
    app.world_mut().write_message(regia_platform::KeyboardInput {
        physical_key: "Escape".into(),
        logical_key: "Escape".into(),
        state: regia_platform::ButtonState::Pressed,
        repeat: false,
        window: Entity::PLACEHOLDER,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    assert!(app.world().resource::<Time>().paused);
    app.update();
    assert_eq!(flow(&app), GameFlow::Paused);

    // Driving the flow directly (what the agent's state.set does) closes the menu.
    app.world_mut()
        .resource_mut::<NextState<GameFlow>>()
        .set(GameFlow::Playing);
    app.update();
    assert_eq!(flow(&app), GameFlow::Playing);
    assert!(!app.world().resource::<Time>().paused);
    assert!(!sample_state(&app, state_entity(&app)).paused);
    let menu = app
        .world()
        .resource::<third_person::hud::PauseMenu>()
        .0
        .expect("pause menu");
    let style = app
        .world()
        .get::<regia_ui::node::NodeStyle>(menu)
        .expect("pause style");
    assert_eq!(style.0.display, regia_ui::style::Display::None, "menu hidden");

    // And back: setting Paused opens it.
    app.world_mut()
        .resource_mut::<NextState<GameFlow>>()
        .set(GameFlow::Paused);
    app.update();
    assert!(sample_state(&app, state_entity(&app)).paused);
    assert!(app.world().resource::<Time>().paused);
}

#[test]
fn boom_shortens_against_a_wall() {
    let mut app = app();
    for _ in 0..15 {
        app.update();
    }
    // The +Z boundary wall's inner face is at z = 29.5. Yaw 0 puts the camera
    // boom_length (4.5 m) towards +Z, so a character at z = 27 forces a shorter boom.
    let entity = character(&app);
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::Transform>(entity) {
        transform.translation = Vec3::new(0.0, 1.2, 27.0);
    }
    set_input(&mut app, Vec3::ZERO, false);
    for _ in 0..20 {
        app.update();
    }
    let boom = app
        .world_mut()
        .query_filtered::<&regia_framework::camera::CameraRuntime, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .map(|runtime| runtime.boom)
        .fold(f32::MAX, f32::min);
    assert!(boom < 3.5, "boom shortened against the wall, length {boom}");
}

/// World-space span of solved foot tips that sit within 8 cm of a ground probe.
///
/// Reads [`JointGlobals`] (the pose two-bone IK wrote) and raycasts, the same exclusion
/// the sample's planner uses. A weight parameter or a pelvis offset the planner stored
/// without solving does not move these joints, so removing the rig's `FootPlacement` or the
/// `TwoBoneIK` nodes collapses the span.
fn planted_foot_span(app: &mut App) -> f32 {
    let rigs: Vec<(Entity, third_person::anim::RigOf)> = app
        .world_mut()
        .query_filtered::<(Entity, &third_person::anim::RigOf), ()>()
        .iter(app.world())
        .map(|(entity, rig_of)| (entity, *rig_of))
        .collect();
    let Some((rig, rig_of)) = rigs
        .into_iter()
        .find(|(_, rig_of)| rig_of.character == character(app))
    else {
        return 0.0;
    };
    let Some(feet) = app.world().get::<third_person::anim::FoxFeet>(rig).copied() else {
        return 0.0;
    };
    let Some(globals) = app.world().get::<regia_animation::components::JointGlobals>(rig) else {
        return 0.0;
    };
    let Some(rig_transform) = app.world().get::<regia_scene::Transform>(rig).copied() else {
        return 0.0;
    };
    let Some(physics) = app.world().get_resource::<regia_physics::PhysicsWorld>() else {
        return 0.0;
    };
    let queries = regia_physics::queries::WorldQueries {
        world: physics,
        layers: None,
    };
    let excluded = [rig_of.character];
    let mut planted = Vec::new();
    for tip in feet.tips {
        let Some(joint) = globals.globals.get(tip as usize) else {
            continue;
        };
        let object = joint.w_axis.truncate();
        let foot = rig_transform.translation + rig_transform.rotation * (object * rig_transform.scale);
        let Some(hit) = queries.raycast(
            foot + Vec3::Y * 0.6,
            Vec3::NEG_Y,
            1.2,
            regia_physics::queries::QueryFilter::new()
                .exclude_sensors()
                .exclude_entities(&excluded),
        ) else {
            continue;
        };
        if (foot.y - hit.point.y).abs() <= 0.08 {
            planted.push(hit.point.y);
        }
    }
    if planted.len() < 2 {
        return 0.0;
    }
    let low = planted.iter().copied().fold(f32::MAX, f32::min);
    let high = planted.iter().copied().fold(f32::MIN, f32::max);
    high - low
}

#[test]
fn foot_ik_plants_feet_on_different_steps() {
    // This test needs the rig. Cook it here (exclusive across processes) instead of relying on another
    // test binary of this package having cooked the gitignored cache first.
    third_person::assets::ensure_cooked()
        .expect("Fox.glb (set REGIA_KHRONOS_ASSETS or allow the one-time download)");
    let mut app = app();
    for _ in 0..10 {
        app.update();
    }
    let rigs: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<third_person::anim::RigOf>>()
        .iter(app.world())
        .collect();
    assert!(
        !rigs.is_empty(),
        "Fox rig missing — cook assets/Fox.glb (see third_person::assets)"
    );

    // Walk +X so OrientToMovement yaws the fox along the flight (asset +X is the
    // fox's length; the mount turns it with the character). The body then spans
    // more than one 0.2 m step.
    set_input(&mut app, Vec3::X, false);
    let mut best = 0.0f32;
    let mut best_published = 0.0f32;
    let mut on_stairs = false;
    for _ in 0..900 {
        app.update();
        let p = translation(&app);
        // Upper steps: step 3 starts near x = 10.25 (top 0.95), step 4 near x = 11.15.
        if p.x > 10.2 && p.x < 12.3 && p.y > 1.55 {
            on_stairs = true;
            best = best.max(planted_foot_span(&mut app));
            let published = app
                .world_mut()
                .query_filtered::<&third_person::anim::SolvedFoot, With<third_person::anim::RigOf>>()
                .iter(app.world())
                .map(|solved| solved.span)
                .fold(0.0f32, f32::max);
            best_published = best_published.max(published);
        }
        if on_stairs && p.x > 12.5 {
            break;
        }
    }
    let p = translation(&app);
    assert!(on_stairs, "walked onto the upper stairs, at {p:?}");
    assert!(
        best >= 0.15,
        "solved foot joints span two steps (best span {best} m) at {p:?}"
    );
    assert!(
        best_published >= 0.15,
        "SolvedFoot.span (what play.expect reads) spans two steps, best {best_published} at {p:?}"
    );
}

/// The car (ADR-2134): out of reach `E` does nothing; next to it the player gets in, the player
/// camera runs the `vehicle_chase` driver asset, `W` drives the car while the character stands
/// still, and getting out removes the driver and puts the character beside the car.
#[test]
fn the_car_is_entered_driven_with_its_chase_camera_and_left() {
    use regia_framework::camera::driver::CameraDriver;
    use regia_framework::tags::{Tag, TagContainer};
    use third_person::car::{CAR_CAMERA, Car, Driving, toggle};

    let mut app = app();
    for _ in 0..15 {
        app.update();
    }
    let car = app
        .world_mut()
        .query_filtered::<Entity, With<Car>>()
        .iter(app.world())
        .next()
        .expect("the car is parked");
    let car_start = app
        .world()
        .get::<regia_scene::Transform>(car)
        .expect("car transform")
        .translation;
    assert!(
        !toggle(app.world_mut(), 0),
        "the start is out of reach of the car"
    );

    let me = character(&app);
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::Transform>(me) {
        transform.translation = car_start + Vec3::new(2.4, 0.3, 0.0);
    }
    set_input(&mut app, Vec3::ZERO, false);
    for _ in 0..10 {
        app.update();
    }
    assert!(toggle(app.world_mut(), 0), "next to the car, E gets in");
    assert_eq!(app.world().get::<Driving>(me).map(|d| d.car), Some(car));
    let driving_tag = Tag::new("State.Driving").expect("valid");
    assert!(
        app.world()
            .get::<TagContainer>(me)
            .is_some_and(|tags| tags.has(driving_tag)),
        "the character is tagged State.Driving"
    );
    let camera_asset = app
        .world_mut()
        .query_filtered::<&CameraDriver, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .next()
        .map(|driver| driver.asset.clone());
    assert_eq!(camera_asset.as_deref(), Some(CAR_CAMERA));

    let parked = translation(&app);
    hold(&mut app, "KeyW", true);
    for _ in 0..150 {
        app.update();
    }
    hold(&mut app, "KeyW", false);
    let car_now = app
        .world()
        .get::<regia_scene::Transform>(car)
        .expect("car transform")
        .translation;
    assert!(
        car_start.z - car_now.z > 2.0,
        "full throttle drove the car forward (−Z): {car_start} -> {car_now}"
    );
    let stood = translation(&app);
    assert!(
        Vec3::new(stood.x - parked.x, 0.0, stood.z - parked.z).length() < 0.5,
        "the character did not walk while driving: {parked} -> {stood}"
    );
    // The chase camera follows the car.
    let camera_pos = app
        .world_mut()
        .query_filtered::<&regia_scene::Transform, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .next()
        .map(|t| t.translation)
        .expect("player camera");
    assert!(
        (camera_pos - car_now).length() < 14.0,
        "the chase camera {camera_pos} stays near the car {car_now}"
    );

    assert!(!toggle(app.world_mut(), 0), "E again gets out");
    assert!(app.world().get::<Driving>(me).is_none());
    let has_driver = app
        .world_mut()
        .query_filtered::<&CameraDriver, With<third_person::camera::PlayerCamera>>()
        .iter(app.world())
        .next()
        .is_some();
    assert!(!has_driver, "on foot the legacy boom rig is back");
    let out = translation(&app);
    assert!(
        (Vec3::new(out.x, car_now.y, out.z) - car_now).length() < 4.0,
        "the character got out beside the car: {out} vs {car_now}"
    );
}

/// Presses one key for a frame (the raw event the sample and the engine pause both read).
fn press(app: &mut App, key: &str) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        physical_key: key.into(),
        logical_key: key.into(),
        state: regia_platform::ButtonState::Pressed,
        repeat: false,
        window: Entity::PLACEHOLDER,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
}

/// The engine pause key (ADR-10104) as the sample ships it: `[game] pause = true` in its `Regia.toml`.
/// Pause/Break freezes the character and shows the built-in Paused screen; the sample's own keys
/// (Esc) wait while it holds; the key again resumes.
#[test]
fn the_engine_pause_key_freezes_play_and_the_sample_keys_wait() {
    use regia_framework::game_pause::{GamePause, GamePausePlugin};

    let mut app = App::new();
    app.insert_resource(regia_framework::prelude::GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    })
    .insert_resource(third_person::SampleDataDir(regia_test_utils::unique_temp_dir(
        "tp-engine-pause",
    )));
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .add_plugins(GamePausePlugin {
            project_root: Some(env!("CARGO_MANIFEST_DIR").into()),
            config: None,
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    app.update();
    assert!(
        app.world().resource::<GamePause>().is_enabled(),
        "the sample's Regia.toml turns the pause on"
    );
    for _ in 0..20 {
        app.update();
    }
    set_input(&mut app, Vec3::X, false);
    press(&mut app, "Pause");
    let pause = app.world().resource::<GamePause>();
    assert!(pause.is_paused());
    assert_eq!(pause.screen_shown(), Some("default"));
    assert!(app.world().resource::<Time>().paused);
    let frozen = translation(&app);
    for _ in 0..30 {
        app.update();
    }
    let after = translation(&app);
    assert!(
        (after - frozen).length() < 1e-4,
        "the engine pause froze the character: {frozen:?} -> {after:?}"
    );
    // Esc would open the sample's own menu and unfreeze time; it waits for the resume.
    press(&mut app, "Escape");
    assert!(
        !sample_state(&app, state_entity(&app)).paused,
        "the sample's menu stays shut"
    );
    assert!(app.world().resource::<Time>().paused, "still frozen");
    press(&mut app, "Pause");
    assert!(!app.world().resource::<GamePause>().is_paused());
    assert!(!app.world().resource::<Time>().paused);
    assert_eq!(app.world().resource::<GamePause>().screen_shown(), None);
    for _ in 0..30 {
        app.update();
    }
    assert!(
        (translation(&app) - after).length() > 0.05,
        "the character walks again after the resume"
    );
}
