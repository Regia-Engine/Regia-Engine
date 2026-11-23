// SPDX-License-Identifier: MPL-2.0
//! Rebinding beyond one key (`docs/specs/11-platform-and-input.md` § 7, ADR-1801): key-group
//! axes, chords, gamepad sticks, conflicts, labels, reset and persistence. Deterministic: fixed
//! 60 Hz frames, no sleeps.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{
    RebindError, RebindMode, binding_label, cancel_rebind, reset_action, reset_all, source_label,
    start_rebind,
};
use regia_platform::{
    ButtonState, DirMount, GamepadAxis, GamepadAxisChanged, GamepadConnected, KeyboardInput, MountPoint, Vfs,
    VfsPath,
};

fn config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Crouch", ActionKind::Button)
        .action("Move", ActionKind::Axis2D)
        .action("Throttle", ActionKind::Axis1D)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new("Crouch", InputSource::key("KeyC")))
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(Binding::new(
            "Throttle",
            InputSource::gamepad_axis(GamepadAxis::LeftStickX),
        ))
}

fn app_with(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app.update();
    app
}

fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// Presses `name`, runs one frame, releases it and runs another so the next press is an edge.
fn tap(app: &mut App, name: &str) {
    key(app, name, ButtonState::Pressed);
    app.update();
    key(app, name, ButtonState::Released);
    app.update();
}

fn rebinder(app: &App) -> &InputRebinder {
    app.world().resource::<InputRebinder>()
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-rebind-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn keys2d_capture_collects_four_distinct_keys_then_rebinds_the_axis() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Move", RebindMode::Keys2D).expect("starts");
    let progress = rebinder(&app).progress().expect("pending");
    assert_eq!((progress.step, progress.total, progress.prompt), (0, 4, "up"));

    tap(&mut app, "ArrowUp");
    assert_eq!(rebinder(&app).progress().expect("pending").prompt, "down");
    // The same key again is ignored, the capture keeps waiting for "down".
    tap(&mut app, "ArrowUp");
    assert_eq!(rebinder(&app).progress().expect("pending").prompt, "down");
    tap(&mut app, "ArrowDown");
    tap(&mut app, "ArrowLeft");
    assert_eq!(
        rebinder(&app).progress().expect("pending").captured,
        vec!["Up", "Down", "Left"]
    );
    assert!(rebinder(&app).outcome.is_none());
    tap(&mut app, "ArrowRight");

    assert!(rebinder(&app).pending().is_none());
    let expected = InputSource::keys2d("ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight");
    assert_eq!(rebinder(&app).outcome.as_ref().map(|o| &o.input), Some(&expected));
    // The new keys drive the axis, the old ones no longer do.
    key(&mut app, "ArrowRight", ButtonState::Pressed);
    key(&mut app, "KeyD", ButtonState::Pressed);
    app.update();
    let moved = app.world().resource::<Actions>().axis2d("Move");
    assert!((moved.x - 1.0).abs() < 1e-4 && moved.y.abs() < 1e-4, "{moved:?}");
    key(&mut app, "ArrowRight", ButtonState::Released);
    app.update();
    assert_eq!(app.world().resource::<Actions>().axis2d("Move").x, 0.0);
}

#[test]
fn keys2d_capture_ignores_non_key_inputs() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Move", RebindMode::Keys2D).expect("starts");
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
    app.update();
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::LeftStickX,
        value: 1.0,
    });
    app.update();
    assert_eq!(rebinder(&app).progress().expect("pending").step, 0);
}

#[test]
fn keys1d_capture_binds_negative_and_positive_keys() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Throttle", RebindMode::Keys1D).expect("starts");
    tap(&mut app, "KeyQ");
    tap(&mut app, "KeyE");
    assert!(rebinder(&app).pending().is_none());
    key(&mut app, "KeyE", ButtonState::Pressed);
    app.update();
    assert!((app.world().resource::<Actions>().axis1d("Throttle") - 1.0).abs() < 1e-4);
    key(&mut app, "KeyE", ButtonState::Released);
    key(&mut app, "KeyQ", ButtonState::Pressed);
    app.update();
    assert!((app.world().resource::<Actions>().axis1d("Throttle") + 1.0).abs() < 1e-4);
    let config = app.world().resource::<InputConfig>();
    assert_eq!(
        binding_label(config, "Gameplay", "Throttle").as_deref(),
        Some("Q/E")
    );
}

#[test]
fn stick_capture_binds_the_stick_in_single_mode() {
    let mut app = app_with(config());
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
    app.update();
    start_rebind(app.world_mut(), "Gameplay", "Move", RebindMode::Single).expect("starts");
    // One idle pass so a deflection afterwards is a new input, not a stick held through start.
    app.update();
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::RightStickY,
        value: 0.9,
    });
    app.update();
    let outcome = rebinder(&app).outcome.clone().expect("captured");
    assert_eq!(outcome.input, InputSource::gamepad_stick(Stick::Right));
    let config = app.world().resource::<InputConfig>();
    assert_eq!(
        binding_label(config, "Gameplay", "Move").as_deref(),
        Some("Right Stick")
    );
}

#[test]
fn chord_capture_writes_a_modifier_helper_and_a_chorded_binding() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Chord).expect("starts");
    assert_eq!(rebinder(&app).progress().expect("pending").prompt, "modifier");
    tap(&mut app, "ControlLeft");
    assert_eq!(rebinder(&app).progress().expect("pending").prompt, "key");
    tap(&mut app, "KeyJ");
    assert!(rebinder(&app).pending().is_none());
    assert_eq!(rebinder(&app).applied.len(), 2);

    let config = app.world().resource::<InputConfig>();
    assert_eq!(config.kind("Jump.chord"), Some(ActionKind::Button));
    assert_eq!(
        binding_label(config, "Gameplay", "Jump").as_deref(),
        Some("Left Control + J")
    );

    // J alone does nothing; Ctrl+J jumps.
    key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("Jump"));
    key(&mut app, "KeyJ", ButtonState::Released);
    key(&mut app, "ControlLeft", ButtonState::Pressed);
    app.update();
    key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Jump"));
}

#[test]
fn chord_capture_refuses_a_stick_and_the_same_input_twice() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Chord).expect("starts");
    tap(&mut app, "ShiftLeft");
    tap(&mut app, "ShiftLeft");
    assert_eq!(rebinder(&app).progress().expect("pending").step, 1);
    cancel_rebind(app.world_mut()).expect("cancels");
    assert!(rebinder(&app).progress().is_none());
    assert_eq!(cancel_rebind(app.world_mut()), Err(RebindError::NotPending));
}

#[test]
fn conflicts_are_reported_for_overlapping_keys_but_not_across_chords() {
    let mut app = app_with(config());
    // Jump -> KeyW overlaps the Move key group.
    start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    tap(&mut app, "KeyW");
    let outcome = rebinder(&app).outcome.clone().expect("captured");
    assert_eq!(outcome.conflicts, vec!["Move".to_string()]);

    // Crouch -> Ctrl+KeyW is a different chord than plain W: no conflict.
    start_rebind(app.world_mut(), "Gameplay", "Crouch", RebindMode::Chord).expect("starts");
    tap(&mut app, "ControlLeft");
    tap(&mut app, "KeyW");
    let outcome = rebinder(&app).outcome.clone().expect("captured");
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
}

#[test]
fn start_rebind_validates_context_action_and_mode() {
    let mut app = app_with(config());
    let w = app.world_mut();
    let err = start_rebind(w, "Menu", "Jump", RebindMode::Single).expect_err("context");
    assert_eq!(err.code(), "rebind_unknown_context");
    let err = start_rebind(w, "Gameplay", "Nope", RebindMode::Single).expect_err("action");
    assert_eq!(err.code(), "rebind_unknown_action");
    let err = start_rebind(w, "Gameplay", "Jump", RebindMode::Keys2D).expect_err("mode");
    assert_eq!(err.code(), "rebind_bad_mode");
    let err = start_rebind(w, "Gameplay", "Move", RebindMode::Chord).expect_err("mode");
    assert_eq!(err.code(), "rebind_bad_mode");
    let err = start_rebind(w, "Gameplay", "Jump.chord", RebindMode::Single).expect_err("helper");
    assert_eq!(err.code(), "rebind_unknown_action");
    assert!(start_rebind(w, "Gameplay", "Move", RebindMode::Keys2D).is_ok());
}

#[test]
fn reset_action_restores_the_project_binding_and_drops_the_chord_helper() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Chord).expect("starts");
    tap(&mut app, "ControlLeft");
    tap(&mut app, "KeyJ");
    assert!(
        app.world()
            .resource::<InputOverrides>()
            .contexts
            .get("Gameplay")
            .is_some_and(|c| c.contains_key("Jump") && c.contains_key("Jump.chord"))
    );

    assert_eq!(reset_action(app.world_mut(), "Gameplay", "Jump"), Ok(true));
    let config = app.world().resource::<InputConfig>();
    assert_eq!(
        binding_label(config, "Gameplay", "Jump").as_deref(),
        Some("Space")
    );
    assert!(binding_label(config, "Gameplay", "Jump.chord").is_none());
    assert!(app.world().resource::<InputOverrides>().contexts["Gameplay"].is_empty());
    // Nothing left to reset the second time.
    assert_eq!(reset_action(app.world_mut(), "Gameplay", "Jump"), Ok(false));
    key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Jump"));
    assert_eq!(
        reset_action(app.world_mut(), "Gameplay", "Nope"),
        Err(RebindError::NoDefault("Nope".into()))
    );
}

#[test]
fn reset_all_restores_every_context_and_counts_the_overrides() {
    let mut app = app_with(config());
    start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    tap(&mut app, "KeyJ");
    start_rebind(app.world_mut(), "Gameplay", "Move", RebindMode::Keys2D).expect("starts");
    for k in ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"] {
        tap(&mut app, k);
    }
    assert_eq!(reset_all(app.world_mut()), Ok(2));
    assert_eq!(app.world().resource::<InputConfig>().contexts, config().contexts);
    assert_eq!(reset_all(app.world_mut()), Ok(0));
}

#[test]
fn chord_and_key_group_overrides_survive_a_restart_through_the_vfs() {
    let dir = unique_dir("persist");
    let mount = |app: &mut App| {
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(dir.clone()));
        app.insert_resource(vfs);
    };
    {
        let mut app = App::new();
        mount(&mut app);
        app.add_plugins(InputPlugin::new(config()));
        app.update();
        start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Chord).expect("starts");
        tap(&mut app, "ControlLeft");
        tap(&mut app, "KeyJ");
        assert!(rebinder(&app).persist_error.is_none());
    }
    {
        let mut app = App::new();
        mount(&mut app);
        app.add_plugins(InputPlugin::new(config()));
        app.update();
        let config = app.world().resource::<InputConfig>();
        assert_eq!(
            binding_label(config, "Gameplay", "Jump").as_deref(),
            Some("Left Control + J")
        );
        assert_eq!(config.kind("Jump.chord"), Some(ActionKind::Button));
        // Resetting after a restart restores the project binding and saves the empty file.
        assert_eq!(reset_action(app.world_mut(), "Gameplay", "Jump"), Ok(true));
        assert!(rebinder(&app).persist_error.is_none());
    }
    {
        let mut app = App::new();
        mount(&mut app);
        app.add_plugins(InputPlugin::new(config()));
        app.update();
        let config = app.world().resource::<InputConfig>();
        assert_eq!(
            binding_label(config, "Gameplay", "Jump").as_deref(),
            Some("Space")
        );
        let path = VfsPath::parse(regia_input::OVERRIDES_PATH).expect("path");
        let vfs = app.world().resource::<Vfs>();
        let saved = InputOverrides::load(vfs, &path).expect("loads");
        assert!(saved.contexts.values().all(std::collections::BTreeMap::is_empty));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn source_labels_are_readable() {
    assert_eq!(source_label(&InputSource::key("KeyW")), "W");
    assert_eq!(source_label(&InputSource::key("Digit1")), "1");
    assert_eq!(source_label(&InputSource::key("Space")), "Space");
    assert_eq!(source_label(&InputSource::key("ArrowUp")), "Up");
    assert_eq!(source_label(&InputSource::key("ShiftRight")), "Right Shift");
    assert_eq!(source_label(&InputSource::key("F5")), "F5");
    assert_eq!(
        source_label(&InputSource::mouse_button(regia_platform::MouseButton::Left)),
        "Mouse Left"
    );
    assert_eq!(
        source_label(&InputSource::gamepad_button(regia_platform::GamepadButton::South)),
        "South"
    );
    assert_eq!(
        source_label(&InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")),
        "W/S/A/D"
    );
}

#[test]
fn rebind_mode_names_round_trip_and_fit_action_kinds() {
    for mode in [
        RebindMode::Single,
        RebindMode::Keys1D,
        RebindMode::Keys2D,
        RebindMode::Chord,
    ] {
        assert_eq!(RebindMode::parse(mode.name()), Some(mode));
        assert!(mode.fits(match mode {
            RebindMode::Keys1D => ActionKind::Axis1D,
            RebindMode::Keys2D => ActionKind::Axis2D,
            _ => ActionKind::Button,
        }));
    }
    assert_eq!(RebindMode::parse("nope"), None);
    assert!(!RebindMode::Chord.fits(ActionKind::Axis2D));
}
