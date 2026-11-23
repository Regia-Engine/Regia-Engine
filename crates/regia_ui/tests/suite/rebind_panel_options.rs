// SPDX-License-Identifier: MPL-2.0
//! The optional parts of the rebinding panel (ADR-2129): a capture per device, axis tuning
//! controls, profile buttons, the guard against the click of a consumed press, and cancel inputs.
//! Headless and deterministic: manual 60 Hz time, no sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{AxisTuning, RebindSlot, axis_tuning, binding_label_in};
use regia_platform::{
    ButtonState, DirMount, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged,
    GamepadConnected, MountPoint, MouseButton, Vfs,
};
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::widgets::{Slider, Toggle};
use regia_ui::{ComputedNode, NodeName, RebindPanel, RebindPanelOptions, Text, UiScales};

fn config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Move", ActionKind::Axis2D)
        .action("Throttle", ActionKind::Axis1D)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(GamepadButton::South),
        ))
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(
            Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                .with_modifier(DeadZone::radial(0.2, 1.0)),
        )
        .binding(Binding::new(
            "Throttle",
            InputSource::keys2d("", "", "KeyQ", "KeyE"),
        ))
}

fn app_with(options: RebindPanelOptions, vfs: Option<Vfs>) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    if let Some(vfs) = vfs {
        app.insert_resource(vfs);
    }
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    app.add_plugins(InputPlugin::new(config()));
    app.insert_resource(UiScales {
        window_size: [1280.0, 800.0],
        scale_factor: 1.0,
    });
    app.world_mut().spawn((
        UiRoot::default(),
        RebindPanel {
            context: "Gameplay".into(),
            actions: Vec::new(),
        },
        options,
    ));
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
    app.update();
    app.update();
    app
}

fn full() -> RebindPanelOptions {
    RebindPanelOptions {
        per_device: true,
        axis_tuning: true,
        profiles: vec!["one".into()],
    }
}

fn node(app: &mut App, name: &str) -> Entity {
    app.world_mut()
        .query::<(Entity, &NodeName)>()
        .iter(app.world())
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node named {name}"))
}

fn exists(app: &mut App, name: &str) -> bool {
    app.world_mut()
        .query::<&NodeName>()
        .iter(app.world())
        .any(|n| n.0 == name)
}

fn label(app: &mut App, name: &str) -> String {
    let entity = node(app, name);
    let world = app.world();
    let text_entity = if world.get::<Text>(entity).is_some() {
        entity
    } else {
        world
            .get::<Children>(entity)
            .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()))
            .expect("text child")
    };
    match &world.get::<Text>(text_entity).unwrap().spans[0].content {
        TextContent::Lit(t) => t.clone(),
        other => panic!("{other:?}"),
    }
}

fn click(app: &mut App, name: &str) {
    let target = node(app, name);
    app.world_mut()
        .write_message(UiEvent::new(target, UiEventKind::Click));
    app.update();
    app.update();
}

/// Moves the real pointer to the center of the named node.
fn hover(app: &mut App, name: &str) {
    let entity = node(app, name);
    let rect = app.world().get::<ComputedNode>(entity).expect("laid out").rect;
    assert!(rect[2] > 0.0 && rect[3] > 0.0, "{name} has no size: {rect:?}");
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (
            f64::from(rect[0] + rect[2] * 0.5),
            f64::from(rect[1] + rect[3] * 0.5),
        ),
        timestamp: Default::default(),
    });
    app.update();
}

fn mouse(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
}

/// A real mouse click on the named node: pointer move, press, release, through hit testing.
fn pointer_click(app: &mut App, name: &str) {
    hover(app, name);
    mouse(app, ButtonState::Pressed);
    mouse(app, ButtonState::Released);
}

fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.into(),
        logical_key: name.into(),
        state,
        repeat: false,
        timestamp: Default::default(),
    });
}

fn tap(app: &mut App, name: &str) {
    key(app, name, ButtonState::Pressed);
    app.update();
    key(app, name, ButtonState::Released);
    app.update();
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

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

fn tuning(app: &App, action: &str, slot: RebindSlot) -> AxisTuning {
    axis_tuning(app.world().resource::<InputConfig>(), "Gameplay", action, slot).expect("tuning")
}

fn pending(app: &App) -> bool {
    app.world().resource::<InputRebinder>().pending().is_some()
}

#[test]
fn without_options_the_panel_has_one_capture_button_and_no_controls() {
    let mut app = app_with(RebindPanelOptions::default(), None);
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Space / South");
    for name in [
        "rebind.Gameplay.Jump.keyboard_mouse",
        "rebind.Gameplay.Jump.gamepad",
        "rebind.Gameplay.Move.tuning.all.row",
        "rebind.Gameplay.profile_status",
    ] {
        assert!(!exists(&mut app, name), "{name}");
    }
}

#[test]
fn a_panel_with_per_device_buttons_captures_one_device_at_a_time() {
    let mut app = app_with(full(), None);
    assert!(!exists(&mut app, "rebind.Gameplay.Jump.bind"));
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.gamepad"), "South");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.keyboard_mouse"), "W/S/A/D");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.gamepad"), "Left Stick");
    assert_eq!(label(&mut app, "rebind.Gameplay.Throttle.gamepad"), "Unbound");

    click(&mut app, "rebind.Gameplay.Jump.gamepad");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Jump.gamepad"),
        "Press a button or move a stick..."
    );
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");
    tap(&mut app, "KeyJ");
    assert!(pending(&app), "a key does not end a gamepad capture");
    pad_button(&mut app, GamepadButton::West, true);
    app.update();
    pad_button(&mut app, GamepadButton::West, false);
    settle(&mut app);
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.gamepad"), "West");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");

    // An axis action takes a stick from the gamepad button.
    click(&mut app, "rebind.Gameplay.Throttle.gamepad");
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::RightStickY,
        value: 0.9,
    });
    app.update();
    settle(&mut app);
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Throttle.gamepad"),
        "Right Stick Y"
    );
    assert_eq!(label(&mut app, "rebind.Gameplay.Throttle.keyboard_mouse"), "Q/E");
}

#[test]
fn tuning_controls_write_the_binding_and_follow_a_reset() {
    let mut app = app_with(full(), None);
    assert!(exists(
        &mut app,
        "rebind.Gameplay.Move.tuning.keyboard_mouse.invert_y"
    ));
    assert!(
        !exists(&mut app, "rebind.Gameplay.Move.tuning.keyboard_mouse.dead_zone"),
        "keys have no dead zone"
    );
    assert!(
        !exists(&mut app, "rebind.Gameplay.Jump.tuning.gamepad.row"),
        "a button has no axis"
    );
    let invert = node(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    let zone = node(&mut app, "rebind.Gameplay.Move.tuning.gamepad.dead_zone");
    assert!(!app.world().get::<Toggle>(invert).unwrap().checked);
    assert_eq!(app.world().get::<Slider>(zone).unwrap().value, 0.2);

    pointer_click(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    assert!(app.world().get::<Toggle>(invert).unwrap().checked);
    assert!(tuning(&app, "Move", RebindSlot::Gamepad).invert_y);
    assert!(!tuning(&app, "Move", RebindSlot::KeyboardMouse).invert_y);

    // A click in the middle of the slider, which runs from 0 to 0.95 in steps of 0.05.
    pointer_click(&mut app, "rebind.Gameplay.Move.tuning.gamepad.dead_zone");
    let value = app.world().get::<Slider>(zone).unwrap().value;
    assert!((0.4..=0.55).contains(&value), "{value}");
    assert_eq!(tuning(&app, "Move", RebindSlot::Gamepad).dead_zone, Some(value));
    assert!(
        tuning(&app, "Move", RebindSlot::Gamepad).invert_y,
        "the other field stays"
    );

    click(&mut app, "rebind.Gameplay.Move.reset");
    assert_eq!(tuning(&app, "Move", RebindSlot::Gamepad).dead_zone, Some(0.2));
    assert!(!tuning(&app, "Move", RebindSlot::Gamepad).invert_y);
    assert!(
        !app.world().get::<Toggle>(invert).unwrap().checked,
        "the control follows the reset"
    );
    assert_eq!(app.world().get::<Slider>(zone).unwrap().value, 0.2);
}

/// The release of the click that bound `Mouse Left` used to start the next capture. A real click
/// on the capture button, through hit testing, starts a capture; the next one is the input.
#[test]
fn the_click_of_a_consumed_press_does_not_restart_the_capture() {
    let mut app = app_with(full(), None);
    let button = "rebind.Gameplay.Jump.keyboard_mouse";
    pointer_click(&mut app, button);
    assert!(pending(&app), "a click on the button starts a capture");
    mouse(&mut app, ButtonState::Pressed);
    assert!(!pending(&app));
    assert_eq!(label(&mut app, button), "Mouse Left");
    assert!(app.world().resource::<InputRebinder>().click_guard());
    mouse(&mut app, ButtonState::Released);
    assert!(
        !pending(&app),
        "the click of the bound press is not a new request"
    );
    app.update();
    assert!(!app.world().resource::<InputRebinder>().click_guard());
    assert_eq!(label(&mut app, button), "Mouse Left");

    // Once it is up, the next click starts a capture as usual.
    pointer_click(&mut app, button);
    assert!(pending(&app));
}

/// The press of a group capture must not end it through its own click either.
#[test]
fn a_chord_can_be_captured_with_the_mouse_over_its_own_button() {
    let mut app = app_with(full(), None);
    let button = "rebind.Gameplay.Jump.alt";
    pointer_click(&mut app, button);
    assert!(pending(&app));
    mouse(&mut app, ButtonState::Pressed);
    mouse(&mut app, ButtonState::Released);
    assert!(
        pending(&app),
        "the click on the modifier's release does not cancel"
    );
    app.update();
    tap(&mut app, "KeyJ");
    settle(&mut app);
    assert!(!pending(&app));
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"),
        "Mouse Left + J"
    );
}

#[test]
fn a_consumed_press_does_not_flip_a_control_under_the_pointer() {
    let mut app = app_with(full(), None);
    pointer_click(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
    assert!(pending(&app));
    let invert = node(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    hover(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    mouse(&mut app, ButtonState::Pressed);
    assert!(!pending(&app));
    mouse(&mut app, ButtonState::Released);
    settle(&mut app);
    assert!(!tuning(&app, "Move", RebindSlot::Gamepad).invert_y);
    assert!(
        !app.world().get::<Toggle>(invert).unwrap().checked,
        "the toggle went back"
    );
    // With nothing captured the same click works.
    pointer_click(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    assert!(tuning(&app, "Move", RebindSlot::Gamepad).invert_y);
}

#[test]
fn escape_and_the_pad_start_button_leave_a_panel_capture() {
    let mut app = app_with(full(), None);
    click(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
    tap(&mut app, "Escape");
    assert!(!pending(&app));
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");
    settle(&mut app);

    click(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
    pad_button(&mut app, GamepadButton::Start, true);
    app.update();
    assert!(!pending(&app), "a pad-only player can leave a keyboard capture");
    pad_button(&mut app, GamepadButton::Start, false);
    settle(&mut app);
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");
}

#[test]
fn the_status_line_shows_a_conflict_from_the_live_bindings() {
    let mut app = app_with(full(), None);
    click(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
    tap(&mut app, "KeyW");
    settle(&mut app);
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Jump.status"),
        "Also used by: Move"
    );
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Move.status"),
        "Also used by: Jump"
    );
    click(&mut app, "rebind.Gameplay.Jump.reset");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.status"), "");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.status"), "");
}

/// Stages a damaged `input_overrides.ron` that start-up cannot move aside (ADR-3102): every
/// `.bad-<n>` copy is taken and the oldest is a directory, so it cannot be dropped to make room.
/// The input plugin then leaves the file alone and switches saving off.
fn stage_unmovable_damaged_overrides(dir: &std::path::Path) {
    std::fs::write(dir.join("input_overrides.ron"), "(contexts: [").expect("write");
    std::fs::create_dir(dir.join("input_overrides.ron.bad-1")).expect("oldest copy");
    for n in 2..=regia_input::MAX_BAD_OVERRIDE_COPIES {
        std::fs::write(dir.join(format!("input_overrides.ron.bad-{n}")), "x").expect("copy");
    }
}

/// A tuning change that cannot be saved still applies, and its row says so. Since ADR-3102 a
/// damaged file is moved aside at start-up and the next change saves (see the next test); the
/// case that still cannot save is a damaged file that cannot be moved (ADR-5667).
#[test]
fn a_tuning_change_that_is_not_saved_says_so_on_its_row() {
    let dir = regia_test_utils::unique_temp_dir("rebind-panel-not-saved");
    stage_unmovable_damaged_overrides(&dir);
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.clone()));
    let mut app = app_with(full(), Some(vfs));
    pointer_click(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    assert!(
        tuning(&app, "Move", RebindSlot::Gamepad).invert_y,
        "applies for the session"
    );
    assert!(
        label(&mut app, "rebind.Gameplay.Move.status").starts_with("Not saved:"),
        "{}",
        label(&mut app, "rebind.Gameplay.Move.status")
    );
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.status"), "");
    assert_eq!(
        std::fs::read_to_string(dir.join("input_overrides.ron")).expect("read"),
        "(contexts: [",
        "the unreadable file is left alone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A damaged file that start-up moved aside (ADR-3102) does not block the panel: the tuning
/// change saves, its row stays quiet, and the damaged bytes are kept in `.bad-1`.
#[test]
fn a_tuning_change_after_a_damaged_file_was_moved_aside_saves_quietly() {
    let dir = regia_test_utils::unique_temp_dir("rebind-panel-moved-aside");
    std::fs::write(dir.join("input_overrides.ron"), "(contexts: [").expect("write");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.clone()));
    let mut app = app_with(full(), Some(vfs));
    assert_eq!(
        std::fs::read_to_string(dir.join("input_overrides.ron.bad-1")).expect("moved aside"),
        "(contexts: ["
    );
    pointer_click(&mut app, "rebind.Gameplay.Move.tuning.gamepad.invert_y");
    assert!(tuning(&app, "Move", RebindSlot::Gamepad).invert_y);
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.status"), "");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.status"), "");
    let saved = std::fs::read_to_string(dir.join("input_overrides.ron")).expect("saved");
    let saved = InputOverrides::from_ron(&saved).expect("the saved file parses");
    let mut merged = config();
    merged.merge_overrides(&saved);
    assert!(
        axis_tuning(&merged, "Gameplay", "Move", RebindSlot::Gamepad).is_some_and(|t| t.invert_y),
        "the saved file holds the change"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_buttons_save_load_and_report() {
    let dir = regia_test_utils::unique_temp_dir("rebind-panel-profile");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.clone()));
    let mut app = app_with(full(), Some(vfs));
    click(&mut app, "rebind.Gameplay.profile.one.load");
    assert!(
        label(&mut app, "rebind.Gameplay.profile_status").contains("no input profile"),
        "{}",
        label(&mut app, "rebind.Gameplay.profile_status")
    );
    click(&mut app, "rebind.Gameplay.Jump.keyboard_mouse");
    tap(&mut app, "KeyK");
    settle(&mut app);
    click(&mut app, "rebind.Gameplay.profile.one.save");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.profile_status"),
        "Saved profile one"
    );
    click(&mut app, "rebind.Gameplay.reset_all");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "Space");
    click(&mut app, "rebind.Gameplay.profile.one.load");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.profile_status"),
        "Loaded profile one"
    );
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.keyboard_mouse"), "K");
    assert_eq!(
        binding_label_in(
            app.world().resource::<InputConfig>(),
            "Gameplay",
            "Jump",
            RebindSlot::Gamepad
        )
        .as_deref(),
        Some("South")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
