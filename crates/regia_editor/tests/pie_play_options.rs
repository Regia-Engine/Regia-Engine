// SPDX-License-Identifier: MPL-2.0
//! Play start options, time scale and mute (spec addendum § 15.1, ADR-7829): play from the level's
//! player start, from the editor camera or from a chosen player start; a per-session time scale and
//! mute that reset on stop. Every test drives the `apply_request` / `frame` path the editor loops use
//! and the `editor.*` methods by name; play worlds step on a fixed frame duration (no wall clock).

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::GameExports;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack};
use regia_editor::level_io;
use regia_editor::pie::{
    CurrentGame, PieRequest, PieSession, PieStatus, PlayOptions, apply_request, sync_status,
};
use regia_editor::pie_controls::{PlayStart, PlayStartChoice, TimeScale};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_framework::player::{PlayerPlugin, PlayerStart};
use regia_scene::components::EntityGuid;
use regia_scene::prelude::{Name, Transform};
use serde_json::json;

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<PieStatus>();
    app.insert_resource(EditorCamera {
        target: Vec3::new(3.0, 1.0, 4.0),
        distance: 5.0,
        yaw: 0.3,
        pitch: 0.2,
        ..EditorCamera::default()
    });
    app.register_type::<PlayerStart>();
    regia_editor::mesh_source::register_level_types(app.world_mut());
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// A level entity made through the editor's spawn path; `start` adds the `PlayerStart` marker.
fn create(app: &mut App, name: &str, at: Vec3, start: bool) -> Guid {
    create_as(app, Guid::new_random(), name, at, start)
}

/// [`create`] with a chosen guid.
fn create_as(app: &mut App, guid: Guid, name: &str, at: Vec3, start: bool) -> Guid {
    let mut command =
        SpawnEntities::new_entity(app.world(), guid, name, Transform::from_translation(at)).expect("spawn");
    command.apply(app.world_mut());
    let entity = find(app.world_mut(), guid).expect("spawned");
    if start {
        app.world_mut().entity_mut(entity).insert(PlayerStart);
    }
    guid
}

fn find(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
}

fn no_world_hook(_: &mut World) {}

fn register_players(app: &mut App) {
    app.add_plugins(PlayerPlugin);
}

/// A game whose `on_load` spawns its pawn where the default spawn rule says (the first player start).
fn spawn_pawn(world: &mut World) {
    let at = regia_framework::rules::default_spawn_point(world, Entity::PLACEHOLDER);
    world.spawn((Name::new("Pawn"), Transform::from_translation(at)));
}

fn game(on_load: fn(&mut World)) -> CurrentGame {
    CurrentGame {
        exports: GameExports {
            register: register_players,
            on_unload: no_world_hook,
            on_load,
        },
        generation: 1,
        package: "pie_play_options_game".to_owned(),
    }
}

fn start(app: &mut App, request: PieRequest, game: Option<CurrentGame>) -> PieSession {
    let mut slot = None;
    let line = apply_request(app.world_mut(), &mut slot, request, game);
    let mut session = slot.unwrap_or_else(|| panic!("play did not start: {line}"));
    session
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    session
}

fn named(world: &mut World, name: &str) -> Vec3 {
    world
        .query::<(&Name, &Transform)>()
        .iter(world)
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, t)| t.translation)
        .unwrap_or_else(|| panic!("no {name}"))
}

fn near(a: Vec3, b: Vec3) -> bool {
    (a - b).length() < 1e-3
}

/// Two player starts, North (x = 10) before South (x = 20) in the level. A level lists its
/// entities in guid order (`snapshot_scene`, and so the saved file and the play copy), so the
/// guids are chosen in that order: random ones made "the first start" a coin toss.
fn two_starts(app: &mut App) -> (Guid, Guid) {
    let north = Guid::from_u128(0x1000_0000_0000_4000_8000_0000_0000_0001);
    let south = Guid::from_u128(0x1000_0000_0000_4000_8000_0000_0000_0002);
    let first = create_as(app, north, "North start", Vec3::new(10.0, 0.0, 0.0), true);
    let second = create_as(app, south, "South start", Vec3::new(20.0, 0.0, 0.0), true);
    (first, second)
}

fn edit_starts(app: &mut App) -> usize {
    let world = app.world_mut();
    world
        .query_filtered::<(), With<PlayerStart>>()
        .iter(world)
        .count()
}

#[test]
fn the_default_start_is_the_levels_first_player_start() {
    let mut app = edit_app();
    two_starts(&mut app);
    let mut session = start(&mut app, PieRequest::Play, Some(game(spawn_pawn)));
    assert!(near(
        named(session.app.world_mut(), "Pawn"),
        Vec3::new(10.0, 0.0, 0.0)
    ));
    sync_status(app.world_mut(), Some(&session));
    assert_eq!(
        app.world().resource::<PieStatus>().start,
        Some(PlayStart::PlayerStart)
    );
}

#[test]
fn from_editor_camera_spawns_the_player_at_the_camera_and_leaves_the_edit_world_alone() {
    let mut app = edit_app();
    two_starts(&mut app);
    let eye = app.world().resource::<EditorCamera>().eye();
    let options = PlayOptions {
        start: Some(PlayStart::Camera),
        ..PlayOptions::default()
    };
    let mut session = start(&mut app, PieRequest::PlayWith(options), Some(game(spawn_pawn)));
    assert!(
        near(named(session.app.world_mut(), "Pawn"), eye),
        "the pawn spawns at the eye {eye:?}"
    );
    assert_eq!(edit_starts(&mut app), 2, "the edit world keeps both starts");
    regia_editor::pie::stop_play(session);
    assert_eq!(edit_starts(&mut app), 2);
}

#[test]
fn from_a_chosen_player_start_spawns_there() {
    let mut app = edit_app();
    let (_, second) = two_starts(&mut app);
    let options = PlayOptions {
        start: Some(PlayStart::Entity(second)),
        ..PlayOptions::default()
    };
    let mut session = start(&mut app, PieRequest::PlayWith(options), Some(game(spawn_pawn)));
    assert!(near(
        named(session.app.world_mut(), "Pawn"),
        Vec3::new(20.0, 0.0, 0.0)
    ));
}

#[test]
fn the_dropdown_choice_applies_to_the_plain_play_request() {
    let mut app = edit_app();
    let (_, second) = two_starts(&mut app);
    app.world_mut()
        .insert_resource(PlayStartChoice(PlayStart::Entity(second)));
    let mut session = start(&mut app, PieRequest::Play, Some(game(spawn_pawn)));
    assert!(near(
        named(session.app.world_mut(), "Pawn"),
        Vec3::new(20.0, 0.0, 0.0)
    ));
    let starts = regia_editor::pie_controls::player_starts(app.world_mut());
    assert_eq!(
        starts.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>(),
        ["North start", "South start"],
        "the dropdown lists the level's starts in entity order"
    );
}

#[test]
fn from_editor_camera_without_a_player_places_the_play_camera() {
    let mut app = edit_app();
    let guid = create(&mut app, "Level camera", Vec3::new(0.0, 2.0, 9.0), false);
    let entity = find(app.world_mut(), guid).expect("camera");
    app.world_mut()
        .entity_mut(entity)
        .insert(regia_render::Camera::default());
    let eye = app.world().resource::<EditorCamera>().eye();
    let options = PlayOptions {
        start: Some(PlayStart::Camera),
        ..PlayOptions::default()
    };
    // No game: nothing spawns a player.
    let mut session = start(&mut app, PieRequest::PlayWith(options), None);
    assert!(near(named(session.app.world_mut(), "Level camera"), eye));
    // The edit world's camera did not move.
    let edit = find(app.world_mut(), guid).expect("camera");
    let at = app.world().get::<Transform>(edit).expect("transform").translation;
    assert!(near(at, Vec3::new(0.0, 2.0, 9.0)), "{at:?}");
}

#[test]
fn time_scale_scales_play_time_and_resets_on_stop() {
    let mut app = edit_app();
    let options = PlayOptions {
        time_scale: TimeScale::new(0.5).expect("in range"),
        ..PlayOptions::default()
    };
    let mut slot = Some(start(&mut app, PieRequest::PlayWith(options), None));
    let session = slot.as_mut().expect("session");
    session.frame();
    let delta = session.app.world().resource::<Time>().delta().as_secs_f64();
    assert!((delta - 0.5 / 60.0).abs() < 1e-6, "half speed: {delta}");
    apply_request(
        app.world_mut(),
        &mut slot,
        PieRequest::SetTimeScale(TimeScale::new(2.0).expect("in range")),
        None,
    );
    let session = slot.as_mut().expect("session");
    session.frame();
    let delta = session.app.world().resource::<Time>().delta().as_secs_f64();
    assert!((delta - 2.0 / 60.0).abs() < 1e-6, "double speed: {delta}");
    sync_status(app.world_mut(), slot.as_ref());
    assert_eq!(app.world().resource::<PieStatus>().time_scale, 2.0);
    // Stop and play again: real time.
    apply_request(app.world_mut(), &mut slot, PieRequest::Stop, None);
    let line = apply_request(app.world_mut(), &mut slot, PieRequest::Play, None);
    sync_status(app.world_mut(), slot.as_ref());
    assert_eq!(app.world().resource::<PieStatus>().time_scale, 1.0, "{line}");
}

/// A game that builds its audio (the offline backend) in `register`.
fn register_audio(app: &mut App) {
    app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
        headless: true,
        ..regia_audio::AudioSettings::default()
    }));
}

#[test]
fn mute_silences_the_play_worlds_root_bus_and_resets_on_stop() {
    let mut app = edit_app();
    let audio_game = CurrentGame {
        exports: GameExports {
            register: register_audio,
            on_unload: no_world_hook,
            on_load: no_world_hook,
        },
        generation: 1,
        package: "pie_audio_game".to_owned(),
    };
    let root = |session: &PieSession| {
        let audio = session.app.world().resource::<regia_audio::Audio>();
        audio.bus_muted("Master")
    };
    let mut slot = Some(start(&mut app, PieRequest::Play, Some(audio_game.clone())));
    assert_eq!(root(slot.as_ref().expect("session")), Some(false));
    apply_request(app.world_mut(), &mut slot, PieRequest::SetMute(true), None);
    assert_eq!(root(slot.as_ref().expect("session")), Some(true));
    // The game unmuting its root bus is overridden while PIE's mute is on.
    let session = slot.as_mut().expect("session");
    session
        .app
        .world_mut()
        .resource_mut::<regia_audio::Audio>()
        .set_bus_mute("Master", false)
        .expect("bus");
    session.frame();
    assert_eq!(root(session), Some(true));
    sync_status(app.world_mut(), slot.as_ref());
    assert!(app.world().resource::<PieStatus>().muted);
    apply_request(app.world_mut(), &mut slot, PieRequest::SetMute(false), None);
    assert_eq!(
        root(slot.as_ref().expect("session")),
        Some(false),
        "the game's own state is back"
    );
    // Muted at start, then a fresh session after stop is not.
    let options = PlayOptions {
        muted: true,
        ..PlayOptions::default()
    };
    apply_request(
        app.world_mut(),
        &mut slot,
        PieRequest::PlayWith(options),
        Some(audio_game.clone()),
    );
    assert_eq!(root(slot.as_ref().expect("session")), Some(true));
    apply_request(app.world_mut(), &mut slot, PieRequest::Stop, None);
    apply_request(app.world_mut(), &mut slot, PieRequest::Play, Some(audio_game));
    assert_eq!(root(slot.as_ref().expect("session")), Some(false));
}

fn call(
    world: &mut World,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(result: Result<serde_json::Value, regia_agent::protocol::RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn drain(app: &mut App, session: &mut Option<PieSession>, game: Option<CurrentGame>) {
    let requests = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = apply_request(app.world_mut(), session, request, game.clone());
    }
    sync_status(app.world_mut(), session.as_ref());
}

#[test]
fn the_agent_surface_starts_scales_and_mutes_by_name() {
    let mut app = edit_app();
    let (first, second) = two_starts(&mut app);
    let plain = create(&mut app, "Rock", Vec3::ZERO, false);
    let world = app.world_mut();
    assert_eq!(
        reason(call(world, "editor.play", json!({ "start": "spawn" }))),
        "play_start_unknown"
    );
    assert_eq!(
        reason(call(
            world,
            "editor.play",
            json!({ "start": "entity", "entity": plain.to_string() })
        )),
        "play_start_not_player_start"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "start": "entity" }))),
        "play_start_not_player_start"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "time_scale": 0.0 }))),
        "play_time_scale_out_of_range",
        "pause is editor.pause"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "time_scale": 5 }))),
        "play_time_scale_out_of_range"
    );
    assert_eq!(
        reason(call(world, "editor.play_controls", json!({ "mute": true }))),
        "not_playing"
    );

    let answer = call(
        world,
        "editor.play",
        json!({ "start": "entity", "entity": second.to_string(), "time_scale": 0.25, "mute": true }),
    )
    .expect("play");
    assert_eq!(answer["start"]["kind"], "entity");
    assert_eq!(answer["start"]["entity"], second.to_string());
    assert_eq!(answer["time_scale"], 0.25);
    assert_eq!(answer["mute"], true);
    let mut session = None;
    drain(&mut app, &mut session, Some(game(spawn_pawn)));
    let sess = session.as_mut().expect("session");
    assert!(near(
        named(sess.app.world_mut(), "Pawn"),
        Vec3::new(20.0, 0.0, 0.0)
    ));
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["time_scale"], 0.25);
    assert_eq!(state["muted"], true);
    assert_eq!(state["start"]["kind"], "entity");

    let answer = call(
        app.world_mut(),
        "editor.play_controls",
        json!({ "time_scale": 3, "mute": false }),
    )
    .expect("controls");
    assert_eq!(answer["queued"], true);
    drain(&mut app, &mut session, None);
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["time_scale"], 3.0);
    assert_eq!(state["muted"], false);
    assert_eq!(
        reason(call(
            app.world_mut(),
            "editor.play_controls",
            json!({ "time_scale": 0.05 })
        )),
        "play_time_scale_out_of_range"
    );

    // The camera start through the agent; the first start is untouched in the edit world.
    call(app.world_mut(), "editor.play", json!({ "start": "camera" })).expect("play");
    drain(&mut app, &mut session, Some(game(spawn_pawn)));
    let eye = app.world().resource::<EditorCamera>().eye();
    assert!(near(
        named(session.as_mut().expect("session").app.world_mut(), "Pawn"),
        eye
    ));
    assert!(find(app.world_mut(), first).is_some());
    assert_eq!(edit_starts(&mut app), 2);
}

// ---- Device simulation in the play window (ADR-7829) ----

use regia_editor::play_window::{
    PlayWindow, PlayWindowEvent, PlayWindowGeometry, PlayWindowPlatform, PlayWindowSupport, Routed,
};

#[derive(Default)]
struct Platform {
    captures: Vec<bool>,
}

impl PlayWindowPlatform for Platform {
    fn open(&mut self, _: PlayWindowGeometry) -> Result<u64, String> {
        Ok(100)
    }
    fn configure_surface(&mut self, _: u32, _: u32) {}
    fn recreate_surface(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn set_capture(&mut self, captured: bool) {
        self.captures.push(captured);
    }
    fn close(&mut self) {}
}

fn messages<M: Message + Clone>(session: &Option<PieSession>) -> Vec<M> {
    session
        .as_ref()
        .expect("session")
        .app
        .world()
        .get_resource::<Messages<M>>()
        .map(|m| m.iter_current_update_messages().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn a_simulated_phone_gets_its_screen_safe_area_touches_and_pad() {
    use regia_platform::{GamepadButton, GamepadButtonChanged, GamepadConnected, TouchInput, TouchPhase};
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    assert_eq!(
        reason(call(
            app.world_mut(),
            "editor.play",
            json!({ "window": true, "device": { "preset": "tablet" } })
        )),
        "play_device_preset_unknown"
    );
    call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "device": { "preset": "phone", "touch": true, "gamepad": true } }),
    )
    .expect("play");
    let mut session = None;
    drain(&mut app, &mut session, None);
    let mut platform = Platform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    let id = window.id().expect("open");
    // The phone's portrait screen, from the UI designer's preset, and its insets in the game.
    assert_eq!(window.render_size(), [1080, 2400]);
    let safe = *session
        .as_ref()
        .expect("session")
        .app
        .world()
        .resource::<regia_platform::SafeArea>();
    assert_eq!((safe.left, safe.top, safe.right, safe.bottom), (0, 110, 0, 60));
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["device"]["size"], json!([1080, 2400]), "{state}");
    assert_eq!(state["device"]["cutout"], json!([360, 0, 360, 66]), "{state}");

    // 1080 x 2400 into the default 1280 x 720 window: scale 0.3, the picture at x = 478.
    let picture = window.picture();
    let at = |px: f64, py: f64| PlayWindowEvent::CursorMoved {
        x: f64::from(picture.x) + px * 0.3,
        y: f64::from(picture.y) + py * 0.3,
    };
    let left = |pressed| PlayWindowEvent::MouseButton {
        button: regia_platform::MouseButton::Left,
        pressed,
    };
    let mut send = |window: &mut PlayWindow, event, app: &mut App, session: &mut Option<PieSession>| {
        window.on_event(id, event, app.world_mut(), session, &mut platform)
    };

    // The virtual pad's South button (bottom right).
    send(&mut window, at(950.0, 2088.0), &mut app, &mut session);
    assert_eq!(
        send(&mut window, left(true), &mut app, &mut session),
        Routed::Play
    );
    assert_eq!(
        send(&mut window, left(false), &mut app, &mut session),
        Routed::Play
    );
    assert_eq!(
        messages::<GamepadConnected>(&session).len(),
        1,
        "the pad announced itself"
    );
    let buttons: Vec<(GamepadButton, bool)> = messages::<GamepadButtonChanged>(&session)
        .iter()
        .map(|m| (m.button, m.state == regia_platform::ButtonState::Pressed))
        .collect();
    assert_eq!(
        buttons,
        vec![(GamepadButton::South, true), (GamepadButton::South, false)]
    );

    // A drag in the middle is one touch, not mouse input; the mouse is not captured.
    send(&mut window, at(540.0, 1200.0), &mut app, &mut session);
    send(&mut window, left(true), &mut app, &mut session);
    send(&mut window, at(640.0, 1200.0), &mut app, &mut session);
    send(&mut window, left(false), &mut app, &mut session);
    let touches: Vec<(u64, TouchPhase)> = messages::<TouchInput>(&session)
        .iter()
        .map(|t| (t.id, t.phase))
        .collect();
    assert_eq!(
        touches,
        vec![
            (0, TouchPhase::Started),
            (0, TouchPhase::Moved),
            (0, TouchPhase::Ended)
        ]
    );
    assert!(messages::<regia_platform::MouseButtonInput>(&session).is_empty());
    assert!(!window.captured(), "touches do not capture the mouse");

    // Ctrl at the press: a second, mirrored finger.
    let ctrl = PlayWindowEvent::Key {
        physical: "ControlLeft".to_owned(),
        logical: "Control".to_owned(),
        pressed: true,
        repeat: false,
        shift: false,
        ctrl: true,
        alt: false,
    };
    send(&mut window, ctrl, &mut app, &mut session);
    send(&mut window, left(true), &mut app, &mut session);
    let started: Vec<u64> = messages::<TouchInput>(&session)
        .iter()
        .filter(|t| t.phase == TouchPhase::Started)
        .map(|t| t.id)
        .collect();
    assert_eq!(started, vec![0, 0, 1], "the pinch adds finger 1");

    // Turned to landscape: the next sync gives the window and the game the landscape phone.
    app.world_mut()
        .resource_mut::<regia_editor::play_device::DeviceSim>()
        .landscape = true;
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(window.render_size(), [2400, 1080]);
    let safe = *session
        .as_ref()
        .expect("session")
        .app
        .world()
        .resource::<regia_platform::SafeArea>();
    assert_eq!((safe.left, safe.top, safe.right, safe.bottom), (110, 0, 60, 0));
}

/// The visible pad uses the existing window-input path, without mouse capture or touch leakage.
#[test]
fn a_pad_button_click_injects_its_input() {
    use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged, GamepadConnected, MouseButton};
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    call(
        app.world_mut(),
        "editor.play",
        json!({"window": true, "device": {"preset": "phone", "touch": true, "gamepad": true}}),
    )
    .expect("play");
    let mut session = None;
    drain(&mut app, &mut session, None);
    let mut platform = Platform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    let id = window.id().expect("window");
    let rect = window.picture();
    for event in [
        PlayWindowEvent::CursorMoved {
            x: f64::from(rect.x) + f64::from(rect.width) * 0.88,
            y: f64::from(rect.y) + f64::from(rect.height) * 0.87,
        },
        PlayWindowEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        },
        PlayWindowEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false,
        },
    ] {
        window.on_event(id, event, app.world_mut(), &mut session, &mut platform);
    }
    let buttons: Vec<_> = messages::<GamepadButtonChanged>(&session)
        .into_iter()
        .map(|m| (m.button, m.state))
        .collect();
    assert_eq!(
        buttons,
        [
            (GamepadButton::South, ButtonState::Pressed),
            (GamepadButton::South, ButtonState::Released)
        ]
    );
    assert_eq!(messages::<GamepadConnected>(&session).len(), 1);
    assert!(messages::<regia_platform::TouchInput>(&session).is_empty());
    assert!(!window.captured());
}

#[test]
fn the_play_window_pad_is_operable_from_the_keyboard() {
    use regia_platform::{ButtonState, GamepadButtonChanged};
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    call(
        app.world_mut(),
        "editor.play",
        json!({"window": true, "device": {"gamepad": true}}),
    )
    .expect("play");
    let mut session = None;
    drain(&mut app, &mut session, None);
    let mut platform = Platform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    let id = window.id().expect("window");
    let key = |name: &str, pressed: bool| PlayWindowEvent::Key {
        physical: name.into(),
        logical: name.into(),
        pressed,
        repeat: false,
        shift: false,
        ctrl: false,
        alt: false,
    };
    for _ in regia_editor::play_device::PAD_BUTTONS {
        for (name, pressed) in [("Tab", true), ("Tab", false), ("Space", true), ("Space", false)] {
            window.on_event(
                id,
                key(name, pressed),
                app.world_mut(),
                &mut session,
                &mut platform,
            );
        }
    }
    let buttons: Vec<_> = messages::<GamepadButtonChanged>(&session)
        .into_iter()
        .map(|m| (m.button, m.state))
        .collect();
    let expected: Vec<_> = regia_editor::play_device::PAD_BUTTONS
        .iter()
        .flat_map(|(button, _, _)| [(*button, ButtonState::Pressed), (*button, ButtonState::Released)])
        .collect();
    assert_eq!(
        buttons, expected,
        "every visible pad button can be reached and operated"
    );
    assert!(
        messages::<regia_platform::KeyboardInput>(&session).is_empty(),
        "overlay keys do not leak into the game"
    );
    // Focus loss releases a held key; its eventual release never leaks into gameplay.
    window.on_event(
        id,
        key("Enter", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    window.on_event(
        id,
        PlayWindowEvent::Focused(false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    window.on_event(
        id,
        key("Enter", false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(
        messages::<GamepadButtonChanged>(&session)
            .last()
            .expect("release")
            .state,
        ButtonState::Released
    );
    assert!(messages::<regia_platform::KeyboardInput>(&session).is_empty());
    // Turning the pad off while held also releases the gameplay input.
    window.on_event(id, key("Tab", true), app.world_mut(), &mut session, &mut platform);
    window.on_event(
        id,
        key("Tab", false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    window.on_event(
        id,
        key("Space", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    app.world_mut()
        .resource_mut::<regia_editor::play_device::DeviceSim>()
        .gamepad = false;
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    window.on_event(
        id,
        key("Space", false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(
        messages::<GamepadButtonChanged>(&session)
            .last()
            .expect("release")
            .state,
        ButtonState::Released
    );
    assert!(messages::<regia_platform::KeyboardInput>(&session).is_empty());
}

/// Uses the real dispatcher, including its schema and permission gate.
#[test]
fn device_controls_through_the_dispatcher_share_pad_input_and_do_not_restart_play() {
    use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged};
    fn rpc(world: &mut World, method: &str, params: serde_json::Value) -> serde_json::Value {
        match regia_agent::call_local(world, method, params, None) {
            regia_agent::LocalReply::Done(result) => result.expect("device RPC"),
            reply => panic!("unexpected deferred reply: {reply:?}"),
        }
    }
    let mut app = edit_app();
    app.world_mut()
        .insert_resource(regia_agent::AgentPermissions::default());
    app.world_mut().insert_resource(PlayWindowSupport);
    rpc(
        app.world_mut(),
        "editor.play",
        json!({"window": true, "device": {"preset": "phone", "gamepad": true}}),
    );
    let mut session = None;
    drain(&mut app, &mut session, None);
    let mut platform = Platform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    let world_id = session.as_ref().expect("play").app.world().id();
    for pressed in [true, true, false] {
        let result = rpc(
            app.world_mut(),
            "editor.play_controls",
            json!({"device": {"safe_area_outline": true, "button": "south", "pressed": pressed}}),
        );
        assert_eq!(result["queued"], true);
        window.sync(app.world_mut(), session.as_mut(), &mut platform);
        let state = rpc(app.world_mut(), "editor.play_state", json!({}));
        assert_eq!(state["device"]["safe_area_outline"], true);
        assert_eq!(
            state["device"]["held_button"],
            if pressed { json!("south") } else { json!(null) }
        );
    }
    let buttons: Vec<_> = messages::<GamepadButtonChanged>(&session)
        .into_iter()
        .map(|m| (m.button, m.state))
        .collect();
    assert_eq!(
        buttons,
        [
            (GamepadButton::South, ButtonState::Pressed),
            (GamepadButton::South, ButtonState::Released)
        ],
        "duplicate absolute presses do not duplicate input"
    );
    assert_eq!(session.as_ref().expect("play").app.world().id(), world_id);
    // Invalid compound requests do not partially change the outline.
    match regia_agent::call_local(
        app.world_mut(),
        "editor.play_controls",
        json!({"device": {"safe_area_outline": false, "button": "unknown", "pressed": true}}),
        None,
    ) {
        regia_agent::LocalReply::Done(Err(error)) => assert_eq!(error.code, -32602),
        reply => panic!("expected invalid_params: {reply:?}"),
    }
    assert!(
        app.world()
            .resource::<regia_editor::play_device::DeviceSim>()
            .safe_area_outline
    );
    // A queued press from the old world is discarded on restart.
    rpc(
        app.world_mut(),
        "editor.play_controls",
        json!({"device": {"button": "north", "pressed": true}}),
    );
    apply_request(app.world_mut(), &mut session, PieRequest::Stop, None);
    apply_request(
        app.world_mut(),
        &mut session,
        PieRequest::PlayWith(PlayOptions {
            window: true,
            ..Default::default()
        }),
        None,
    );
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert!(messages::<GamepadButtonChanged>(&session).is_empty());
}
