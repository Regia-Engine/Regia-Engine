// SPDX-License-Identifier: MPL-2.0
//! Device slots, axis tuning, capture guards and profiles of the rebinding backend (ADR-2129).
//! Deterministic: fixed 60 Hz frames, no sleeps.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{
    AxisTuning, DEFAULT_DEAD_ZONE, InputDefaults, OVERRIDES_VERSION, RebindError, RebindMode, RebindSlot,
    ResponseCurve, axis_tuning, binding_conflicts, binding_label, binding_label_in, list_profiles,
    load_profile, profile_path, reset_action, save_profile, set_axis_tuning, start_rebind_slot,
};
use regia_platform::{
    ButtonState, DirMount, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged,
    GamepadConnected, KeyboardInput, MountPoint, MouseButton, MouseButtonInput, Vfs, VfsPath,
};

fn config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .action("Throttle", ActionKind::Axis1D)
        .action("Move", ActionKind::Axis2D)
        .action("Look", ActionKind::Axis2D)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(GamepadButton::South),
        ))
        .binding(Binding::new("Fire", InputSource::key("KeyF")).with_trigger(Trigger::Pressed))
        .binding(Binding::new(
            "Throttle",
            InputSource::keys2d("", "", "KeyQ", "KeyE"),
        ))
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(
            Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                .with_modifier(DeadZone::radial(0.3, 1.0))
                .with_modifier(Modifier::ResponseCurve(ResponseCurve::Square)),
        )
        .binding(Binding::new("Look", InputSource::MouseDelta))
        .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
}

fn app_with(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
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

fn tap(app: &mut App, name: &str) {
    key(app, name, ButtonState::Pressed);
    app.update();
    key(app, name, ButtonState::Released);
    app.update();
}

fn mouse(app: &mut App, button: MouseButton, state: ButtonState) {
    app.world_mut().write_message(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn pad_button(app: &mut App, button: GamepadButton, down: bool) {
    app.world_mut().write_message(GamepadButtonChanged {
        device: 1,
        button,
        state: if down {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        value: f32::from(u8::from(down)),
    });
}

fn pad_axis(app: &mut App, axis: GamepadAxis, value: f32) {
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis,
        value,
    });
}

fn rebinder(app: &App) -> &InputRebinder {
    app.world().resource::<InputRebinder>()
}

fn live(app: &App) -> &InputConfig {
    app.world().resource::<InputConfig>()
}

fn label(app: &App, action: &str, slot: RebindSlot) -> Option<String> {
    binding_label_in(live(app), "Gameplay", action, slot)
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-rebind-axes-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn app_on_disk(dir: &std::path::Path) -> App {
    let mut app = App::new();
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.to_path_buf()));
    app.insert_resource(vfs);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config()));
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
    app.update();
    app
}

#[test]
fn a_keyboard_slot_capture_keeps_the_gamepad_binding_and_the_other_way_round() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindMode::Single,
        RebindSlot::KeyboardMouse,
    )
    .expect("starts");
    tap(&mut app, "KeyJ");
    assert_eq!(
        label(&app, "Jump", RebindSlot::KeyboardMouse).as_deref(),
        Some("J")
    );
    assert_eq!(label(&app, "Jump", RebindSlot::Gamepad).as_deref(), Some("South"));
    assert_eq!(
        rebinder(&app).outcome.as_ref().map(|o| o.slot),
        Some(RebindSlot::KeyboardMouse)
    );

    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    pad_button(&mut app, GamepadButton::North, true);
    app.update();
    assert_eq!(
        label(&app, "Jump", RebindSlot::KeyboardMouse).as_deref(),
        Some("J")
    );
    assert_eq!(label(&app, "Jump", RebindSlot::Gamepad).as_deref(), Some("North"));
    assert_eq!(
        live(&app).contexts["Gameplay"]
            .bindings
            .iter()
            .filter(|b| b.action == "Jump")
            .count(),
        2,
        "one binding per device, however often it merged"
    );
}

#[test]
fn a_slot_capture_ignores_the_other_devices_and_refuses_keys_on_the_pad() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    tap(&mut app, "KeyJ");
    mouse(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(
        rebinder(&app).pending().is_some(),
        "still waiting for a pad input"
    );
    pad_button(&mut app, GamepadButton::West, true);
    app.update();
    assert!(rebinder(&app).pending().is_none());
    assert_eq!(label(&app, "Jump", RebindSlot::Gamepad).as_deref(), Some("West"));

    let err = start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Move",
        RebindMode::Keys2D,
        RebindSlot::Gamepad,
    )
    .expect_err("no key group on a pad");
    assert_eq!(err.code(), "rebind_bad_mode");
}

#[test]
fn mouse_motion_binds_only_a_two_axis_action_on_the_mouse_slot_and_only_past_its_travel() {
    let mut app = app_with(config());
    // Bound away from the mouse first, so this also proves the way back.
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Look",
        RebindMode::Single,
        RebindSlot::KeyboardMouse,
    )
    .expect("starts");
    tap(&mut app, "KeyL");
    assert_eq!(
        label(&app, "Look", RebindSlot::KeyboardMouse).as_deref(),
        Some("L")
    );

    let moved = |app: &mut App, dx: f64| {
        app.world_mut().write_message(regia_platform::MouseMotion {
            delta: (dx, 0.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
    };
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Look",
        RebindMode::Single,
        RebindSlot::KeyboardMouse,
    )
    .expect("starts");
    moved(&mut app, 80.0);
    moved(&mut app, -80.0);
    assert!(
        rebinder(&app).pending().is_some(),
        "a small movement is not an input"
    );
    moved(&mut app, 80.0);
    assert!(rebinder(&app).pending().is_none());
    assert_eq!(
        label(&app, "Look", RebindSlot::KeyboardMouse).as_deref(),
        Some("Mouse Move")
    );
    assert_eq!(
        label(&app, "Look", RebindSlot::Gamepad).as_deref(),
        Some("Right Stick")
    );

    // Not on a button, not on the pad slot, not in the default whole-action capture.
    for (action, slot) in [
        ("Jump", RebindSlot::KeyboardMouse),
        ("Look", RebindSlot::Gamepad),
        ("Look", RebindSlot::All),
    ] {
        start_rebind_slot(app.world_mut(), "Gameplay", action, RebindMode::Single, slot).expect("starts");
        moved(&mut app, 500.0);
        assert!(rebinder(&app).pending().is_some(), "{action} {slot:?}");
        regia_input::cancel_rebind(app.world_mut()).expect("cancels");
    }
}

#[test]
fn a_whole_action_capture_replaces_every_device_and_supersedes_slots() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    pad_button(&mut app, GamepadButton::West, true);
    app.update();
    pad_button(&mut app, GamepadButton::West, false);
    app.update();
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    tap(&mut app, "KeyK");
    assert_eq!(
        binding_label(live(&app), "Gameplay", "Jump").as_deref(),
        Some("K")
    );
    assert!(app.world().resource::<InputOverrides>().slots.is_empty());
}

#[test]
fn a_stick_push_on_a_one_value_action_binds_the_axis_that_moved_and_reads_on_x() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Throttle",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    app.update();
    pad_axis(&mut app, GamepadAxis::RightStickY, 0.9);
    app.update();
    let outcome = rebinder(&app).outcome.clone().expect("captured");
    assert_eq!(outcome.input, InputSource::gamepad_axis(GamepadAxis::RightStickY));
    pad_axis(&mut app, GamepadAxis::RightStickY, 0.0);
    app.update();
    assert_eq!(app.world().resource::<Actions>().axis1d("Throttle"), 0.0);
    pad_axis(&mut app, GamepadAxis::RightStickY, 1.0);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().axis1d("Throttle"),
        1.0,
        "a push up drives the action, not a stick's unread x"
    );
    assert_eq!(
        label(&app, "Throttle", RebindSlot::KeyboardMouse).as_deref(),
        Some("Q/E")
    );
}

#[test]
fn a_pad_axis_on_a_button_action_presses_past_half_travel_only() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    app.update();
    pad_axis(&mut app, GamepadAxis::RightTrigger, 1.0);
    app.update();
    pad_axis(&mut app, GamepadAxis::RightTrigger, 0.4);
    app.update();
    assert!(
        !app.world().resource::<Actions>().pressed("Jump"),
        "resting drift is not a press"
    );
    pad_axis(&mut app, GamepadAxis::RightTrigger, 0.9);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Jump"));
}

#[test]
fn a_rebind_keeps_the_action_trigger_and_the_same_shape_modifiers() {
    let mut app = app_with(config());
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Fire", RebindMode::Single).expect("starts");
    tap(&mut app, "KeyG");
    let fire = rebinder(&app).applied[0].clone();
    assert_eq!(fire.input, InputSource::key("KeyG"));
    assert_eq!(
        fire.triggers,
        vec![Trigger::Pressed],
        "a pressed action must not start to autofire"
    );

    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Move",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    app.update();
    pad_axis(&mut app, GamepadAxis::RightStickX, 0.9);
    app.update();
    let moved = rebinder(&app).applied[0].clone();
    assert_eq!(moved.input, InputSource::gamepad_stick(Stick::Right));
    assert_eq!(
        moved.modifiers,
        vec![
            Modifier::DeadZone(DeadZone::radial(0.3, 1.0)),
            Modifier::ResponseCurve(ResponseCurve::Square)
        ],
        "a stick keeps the dead zone and curve of the stick it replaces"
    );
}

#[test]
fn a_first_analog_binding_gets_the_default_dead_zone() {
    let mut app = app_with(config());
    start_rebind_slot(
        app.world_mut(),
        "Gameplay",
        "Throttle",
        RebindMode::Single,
        RebindSlot::Gamepad,
    )
    .expect("starts");
    app.update();
    pad_axis(&mut app, GamepadAxis::LeftTrigger, 1.0);
    app.update();
    let binding = rebinder(&app).applied[0].clone();
    assert_eq!(
        binding.modifiers,
        vec![Modifier::DeadZone(DeadZone::axial(DEFAULT_DEAD_ZONE, 1.0))]
    );
}

#[test]
fn tuning_changes_the_action_value_and_only_the_named_device() {
    let mut app = app_with(config());
    let before = axis_tuning(live(&app), "Gameplay", "Move", RebindSlot::Gamepad).expect("stick");
    assert_eq!(before.dead_zone, Some(0.3));

    let tuning = AxisTuning {
        dead_zone: Some(0.6),
        invert_x: false,
        invert_y: true,
    };
    let now = set_axis_tuning(app.world_mut(), "Gameplay", "Move", RebindSlot::Gamepad, tuning).expect("set");
    assert_eq!(now, tuning);

    pad_axis(&mut app, GamepadAxis::LeftStickY, 0.5);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().axis2d("Move").y,
        0.0,
        "0.5 is inside the larger dead zone"
    );
    pad_axis(&mut app, GamepadAxis::LeftStickY, 1.0);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().axis2d("Move").y,
        -1.0,
        "inverted"
    );

    // The keyboard binding of the same action stays as it was.
    assert_eq!(
        axis_tuning(live(&app), "Gameplay", "Move", RebindSlot::KeyboardMouse),
        Some(AxisTuning::default())
    );
    // The curve between dead zone and invert stays.
    let stick = live(&app).contexts["Gameplay"]
        .bindings
        .iter()
        .find(|b| b.input == InputSource::gamepad_stick(Stick::Left))
        .expect("stick binding");
    assert_eq!(
        stick.modifiers,
        vec![
            Modifier::DeadZone(DeadZone::radial(0.6, 1.0)),
            Modifier::ResponseCurve(ResponseCurve::Square),
            Modifier::Negate {
                x: false,
                y: true,
                z: false
            },
        ]
    );
}

#[test]
fn tuning_all_covers_mouse_look_and_the_stick_and_clearing_invert_removes_it() {
    let mut app = app_with(config());
    let tuning = AxisTuning {
        dead_zone: Some(0.2),
        invert_x: false,
        invert_y: true,
    };
    set_axis_tuning(app.world_mut(), "Gameplay", "Look", RebindSlot::All, tuning).expect("set");
    let mouse_side = axis_tuning(live(&app), "Gameplay", "Look", RebindSlot::KeyboardMouse).expect("mouse");
    assert!(mouse_side.invert_y);
    assert_eq!(mouse_side.dead_zone, None, "mouse motion has no dead zone");
    let pad_side = axis_tuning(live(&app), "Gameplay", "Look", RebindSlot::Gamepad).expect("pad");
    assert_eq!(pad_side, tuning);

    let off = AxisTuning {
        dead_zone: Some(0.2),
        ..AxisTuning::default()
    };
    set_axis_tuning(app.world_mut(), "Gameplay", "Look", RebindSlot::All, off).expect("set");
    let mouse = live(&app).contexts["Gameplay"]
        .bindings
        .iter()
        .find(|b| b.input == InputSource::MouseDelta)
        .expect("mouse binding");
    assert!(mouse.modifiers.is_empty(), "{:?}", mouse.modifiers);
}

#[test]
fn tuning_refuses_bad_values_and_inputs_with_no_axis() {
    let mut app = app_with(config());
    let bad = |dead_zone| AxisTuning {
        dead_zone: Some(dead_zone),
        ..AxisTuning::default()
    };
    for value in [-0.1, 0.96, f32::NAN, f32::INFINITY] {
        let err = set_axis_tuning(app.world_mut(), "Gameplay", "Move", RebindSlot::All, bad(value))
            .expect_err("range");
        assert_eq!(err.code(), "rebind_bad_tuning");
    }
    let err = set_axis_tuning(
        app.world_mut(),
        "Gameplay",
        "Jump",
        RebindSlot::Gamepad,
        AxisTuning::default(),
    )
    .expect_err("a button");
    assert_eq!(err, RebindError::NotTunable("Jump".to_string()));
    assert_eq!(err.code(), "rebind_not_tunable");
    let err = set_axis_tuning(
        app.world_mut(),
        "Gameplay",
        "Nope",
        RebindSlot::All,
        AxisTuning::default(),
    )
    .expect_err("unknown");
    assert_eq!(err.code(), "rebind_unknown_action");
    let err = set_axis_tuning(
        app.world_mut(),
        "Menu",
        "Move",
        RebindSlot::All,
        AxisTuning::default(),
    )
    .expect_err("context");
    assert_eq!(err.code(), "rebind_unknown_context");
    assert_eq!(
        app.world().resource::<InputOverrides>(),
        &InputOverrides::default()
    );
}

#[test]
fn tuning_and_slots_survive_a_restart_and_reset_restores_the_project_binding() {
    let dir = unique_dir("restart");
    let tuning = AxisTuning {
        dead_zone: Some(0.4),
        invert_x: true,
        invert_y: false,
    };
    {
        let mut app = app_on_disk(&dir);
        set_axis_tuning(app.world_mut(), "Gameplay", "Move", RebindSlot::Gamepad, tuning).expect("set");
        start_rebind_slot(
            app.world_mut(),
            "Gameplay",
            "Jump",
            RebindMode::Chord,
            RebindSlot::Gamepad,
        )
        .expect("starts");
        pad_button(&mut app, GamepadButton::LeftShoulder, true);
        app.update();
        pad_button(&mut app, GamepadButton::LeftShoulder, false);
        app.update();
        pad_button(&mut app, GamepadButton::East, true);
        app.update();
        assert!(rebinder(&app).persist_error.is_none());
        assert_eq!(
            label(&app, "Jump", RebindSlot::Gamepad).as_deref(),
            Some("Left Shoulder + East")
        );
        assert_eq!(
            label(&app, "Jump", RebindSlot::KeyboardMouse).as_deref(),
            Some("Space")
        );
    }
    {
        let mut app = app_on_disk(&dir);
        assert_eq!(
            axis_tuning(live(&app), "Gameplay", "Move", RebindSlot::Gamepad),
            Some(tuning)
        );
        assert_eq!(
            label(&app, "Jump", RebindSlot::Gamepad).as_deref(),
            Some("Left Shoulder + East")
        );
        assert_eq!(
            label(&app, "Jump", RebindSlot::KeyboardMouse).as_deref(),
            Some("Space")
        );
        assert_eq!(reset_action(app.world_mut(), "Gameplay", "Move"), Ok(true));
        assert_eq!(reset_action(app.world_mut(), "Gameplay", "Jump"), Ok(true));
        let defaults = app.world().resource::<InputDefaults>().0.clone();
        let sorted = |config: &InputConfig| {
            let mut names: Vec<String> = config.contexts["Gameplay"]
                .bindings
                .iter()
                .map(|b| format!("{b:?}"))
                .collect();
            names.sort();
            names
        };
        assert_eq!(sorted(live(&app)), sorted(&defaults));
        let path = VfsPath::parse(regia_input::OVERRIDES_PATH).expect("path");
        let saved = InputOverrides::load(app.world().resource::<Vfs>(), &path).expect("loads");
        assert!(saved.slots.is_empty(), "{saved:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_version_1_file_loads_and_is_written_back_as_version_2() {
    let dir = unique_dir("v1");
    std::fs::write(
        dir.join("input_overrides.ron"),
        "(contexts:{\"Gameplay\":{\"Jump\":(action:\"Jump\",input:Key(\"KeyJ\"))}})",
    )
    .expect("write");
    let mut app = app_on_disk(&dir);
    assert_eq!(
        binding_label(live(&app), "Gameplay", "Jump").as_deref(),
        Some("J")
    );
    assert_eq!(
        app.world().resource::<InputOverrides>().format_version,
        OVERRIDES_VERSION
    );
    set_axis_tuning(
        app.world_mut(),
        "Gameplay",
        "Move",
        RebindSlot::Gamepad,
        AxisTuning::default(),
    )
    .expect("set");
    let written = std::fs::read_to_string(dir.join("input_overrides.ron")).expect("read");
    assert!(written.contains("format_version: 2"), "{written}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_from_a_newer_build_is_moved_aside_and_saves_continue() {
    let dir = unique_dir("v9");
    let text = "(format_version:9,contexts:{})";
    std::fs::write(dir.join("input_overrides.ron"), text).expect("write");
    let mut app = app_on_disk(&dir);
    // ADR-3102: a newer build's file is damage like any other unreadable file: it moves aside
    // (its bytes are kept, never overwritten), the project bindings load, and saving works.
    // Before, the file was refused and left in place, which blocked every later save.
    assert!(
        !rebinder(&app).persist_blocked(),
        "the newer file moved aside, so saves are open"
    );
    set_axis_tuning(
        app.world_mut(),
        "Gameplay",
        "Move",
        RebindSlot::Gamepad,
        AxisTuning::default(),
    )
    .expect("applies for the session");
    assert!(
        rebinder(&app).persist_error.is_none(),
        "the tuning saved (the damaged copy stayed aside)"
    );
    save_profile(app.world(), None).expect("the active file saves");
    assert_eq!(
        std::fs::read_to_string(dir.join("input_overrides.ron.bad-1")).expect("read"),
        text
    );
    let fresh = std::fs::read_to_string(dir.join("input_overrides.ron")).expect("the fresh file");
    assert_ne!(fresh, text, "the fresh file is not the newer build's file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stick_and_axis_conflict_and_the_live_list_reads_any_binding() {
    let config = config().action("Steer", ActionKind::Axis1D).binding(Binding::new(
        "Steer",
        InputSource::gamepad_axis(GamepadAxis::LeftStickX),
    ));
    let conflicts = binding_conflicts(&config, "Gameplay", "Steer");
    assert_eq!(conflicts, vec!["Move".to_string()]);
    assert_eq!(
        binding_conflicts(&config, "Gameplay", "Move"),
        vec!["Steer".to_string()]
    );
    assert!(binding_conflicts(&config, "Gameplay", "Jump").is_empty());
    assert!(binding_conflicts(&config, "Menu", "Jump").is_empty());
}

#[test]
fn the_press_that_finished_a_capture_keeps_the_click_guard_until_it_is_up() {
    let mut app = app_with(config());
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    assert!(!rebinder(&app).click_guard());
    mouse(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(rebinder(&app).pending().is_none());
    assert!(
        rebinder(&app).click_guard(),
        "the release of this press sends a click"
    );
    app.update();
    assert!(rebinder(&app).click_guard(), "still held");
    mouse(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    assert!(rebinder(&app).click_guard(), "the release frame");
    app.update();
    assert!(!rebinder(&app).click_guard(), "up since the last frame");
}

#[test]
fn a_cancel_input_ends_the_capture_binds_nothing_and_guards_its_click() {
    let mut app = app_with(config());
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .set_cancel_inputs(vec![
            InputSource::key("Escape"),
            InputSource::gamepad_button(GamepadButton::Start),
        ]);
    key(&mut app, "Escape", ButtonState::Pressed);
    app.update();
    assert!(rebinder(&app).pending().is_none());
    assert!(rebinder(&app).outcome.is_none());
    assert!(rebinder(&app).click_guard());
    assert_eq!(
        app.world().resource::<InputOverrides>(),
        &InputOverrides::default()
    );
    key(&mut app, "Escape", ButtonState::Released);
    app.update();

    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .set_cancel_inputs(vec![InputSource::gamepad_button(GamepadButton::Start)]);
    pad_button(&mut app, GamepadButton::Start, true);
    app.update();
    assert!(rebinder(&app).pending().is_none());
    assert!(rebinder(&app).outcome.is_none());
    pad_button(&mut app, GamepadButton::Start, false);
    app.update();

    // Without cancel inputs Escape binds like any key, as before.
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    tap(&mut app, "Escape");
    assert_eq!(
        binding_label(live(&app), "Gameplay", "Jump").as_deref(),
        Some("Escape")
    );
}

#[test]
fn profiles_save_list_load_and_become_active() {
    let dir = unique_dir("profiles");
    {
        let mut app = app_on_disk(&dir);
        assert!(list_profiles(app.world()).is_empty());
        regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
        tap(&mut app, "KeyK");
        assert_eq!(
            save_profile(app.world(), Some("left_hand")).expect("saved"),
            "user://input_profiles/left_hand.ron"
        );
        regia_input::reset_all(app.world_mut()).expect("reset");
        assert_eq!(
            binding_label(live(&app), "Gameplay", "Jump").as_deref(),
            Some("Space / South")
        );
        assert_eq!(list_profiles(app.world()), vec!["left_hand".to_string()]);
        assert_eq!(load_profile(app.world_mut(), Some("left_hand")), Ok(1));
        assert_eq!(
            binding_label(live(&app), "Gameplay", "Jump").as_deref(),
            Some("K")
        );
        // Loading is not additive: an override that is not in the profile goes away.
        regia_input::start_rebind(app.world_mut(), "Gameplay", "Fire", RebindMode::Single).expect("starts");
        tap(&mut app, "KeyH");
        assert_eq!(load_profile(app.world_mut(), Some("left_hand")), Ok(1));
        assert_eq!(
            binding_label(live(&app), "Gameplay", "Fire").as_deref(),
            Some("F")
        );
    }
    {
        let app = app_on_disk(&dir);
        assert_eq!(
            binding_label(live(&app), "Gameplay", "Jump").as_deref(),
            Some("K"),
            "the loaded profile is the active one after a restart"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bad_missing_or_corrupt_profile_changes_nothing() {
    let dir = unique_dir("bad-profile");
    let mut app = app_on_disk(&dir);
    regia_input::start_rebind(app.world_mut(), "Gameplay", "Jump", RebindMode::Single).expect("starts");
    tap(&mut app, "KeyK");
    let long = "x".repeat(33);
    for name in ["", "UP", "a/b", "../x", "a.b", long.as_str()] {
        assert_eq!(
            profile_path(Some(name)).expect_err("name").code(),
            "rebind_bad_profile",
            "{name:?}"
        );
    }
    assert_eq!(
        load_profile(app.world_mut(), Some("nope"))
            .expect_err("missing")
            .code(),
        "rebind_profile_missing"
    );
    std::fs::create_dir_all(dir.join("input_profiles")).expect("dir");
    std::fs::write(dir.join("input_profiles/broken.ron"), "(contexts: [").expect("write");
    assert_eq!(
        load_profile(app.world_mut(), Some("broken"))
            .expect_err("corrupt")
            .code(),
        "rebind_profile_corrupt"
    );
    assert_eq!(
        binding_label(live(&app), "Gameplay", "Jump").as_deref(),
        Some("K")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profiles_without_a_vfs_report_it() {
    let mut app = app_with(config());
    assert_eq!(
        save_profile(app.world(), Some("a")).expect_err("no vfs").code(),
        "rebind_save_failed"
    );
    assert_eq!(
        load_profile(app.world_mut(), Some("a"))
            .expect_err("no vfs")
            .code(),
        "rebind_profile_missing"
    );
    assert!(list_profiles(app.world()).is_empty());
}
