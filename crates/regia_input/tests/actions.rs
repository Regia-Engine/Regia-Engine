// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests for the input action layer (`docs/specs/11-platform-and-input.md` § 7):
//! device resources, action mapping, modifiers, contexts, triggers and the `Actions` query API.
//!
//! Axis convention everywhere in this crate: **x = right, y = forward** (screen "up" is forward).

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_input::Swizzle;
use regia_input::prelude::*;
use regia_platform::{
    ButtonState, GamepadAxisChanged, GamepadButton, GamepadButtonChanged, GamepadConnected, KeyboardInput,
    MouseButtonInput, MouseMotion, MouseWheel, WheelUnit,
};

const EPS: f32 = 1e-4;

/// Builds a headless app with the input plugin and deterministic 60 Hz frames.
fn input_app(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app
}

fn assert_close(value: Vec2, expected: (f32, f32)) {
    assert!(
        (value.x - expected.0).abs() < EPS && (value.y - expected.1).abs() < EPS,
        "{value:?} != {expected:?}"
    );
}

fn send_key(app: &mut App, name: &str, state: ButtonState) {
    send_key_repeat(app, name, state, false);
}

fn send_key_repeat(app: &mut App, name: &str, state: ButtonState, repeat: bool) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn send_motion(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(MouseMotion {
        delta: (x, y),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn send_wheel(app: &mut App, x: f32, y: f32, unit: WheelUnit) {
    app.world_mut().write_message(MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (x, y),
        unit,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn send_mouse_button(app: &mut App, button: regia_platform::MouseButton, state: ButtonState) {
    app.world_mut().write_message(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn connect_pad(app: &mut App, id: u64) {
    app.world_mut().write_message(GamepadConnected {
        device: id,
        name: "Test Pad".into(),
    });
}

fn pad_button(app: &mut App, id: u64, button: GamepadButton, value: f32) {
    app.world_mut().write_message(GamepadButtonChanged {
        device: id,
        button,
        state: if value > 0.5 {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        value,
    });
}

fn pad_axis(app: &mut App, id: u64, axis: regia_platform::GamepadAxis, value: f32) {
    app.world_mut().write_message(GamepadAxisChanged {
        device: id,
        axis,
        value,
    });
}

/// `Move` from WASD, `Look` from the scaled mouse delta, `Jump` from Space, `Fire` from LMB.
fn gameplay_config() -> InputConfig {
    InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Look", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(
            Binding::new("Look", InputSource::MouseDelta)
                .with_modifier(Modifier::Scale(Vec3::new(0.5, 2.0, 0.0))),
        )
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new("Fire", InputSource::mouse_button(MouseButton::Left)))
}

#[test]
fn wasd_diagonal_move_is_unit_length_x_right_y_forward() {
    let mut app = input_app(gameplay_config());
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    send_key(&mut app, "KeyD", ButtonState::Pressed);
    app.update();
    let actions = app.world().resource::<Actions>();
    let diagonal = actions.axis2d("Move");
    assert_close(diagonal, (0.5f32.sqrt(), 0.5f32.sqrt()));
    assert!((diagonal.length() - 1.0).abs() < EPS, "{diagonal:?}");
    assert!(actions.pressed("Move"), "a non-zero axis counts as pressed");
}

#[test]
fn single_forward_key_gives_unit_forward() {
    let mut app = input_app(gameplay_config());
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Move"), (0.0, 1.0));
}

#[test]
fn radial_dead_zone_zeroes_small_motion_and_keeps_full() {
    let config = InputConfig::new()
        .action("Look", ActionKind::Axis2D)
        .binding(Binding::new("Look", InputSource::MouseDelta).with_modifier(DeadZone::radial(0.2, 1.0)));
    let mut app = input_app(config);

    send_motion(&mut app, 0.1, 0.0);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Look"), (0.0, 0.0));

    send_motion(&mut app, 1.0, 0.0);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Look"), (1.0, 0.0));

    // Between lo and hi the magnitude is rescaled linearly: (0.6 - 0.2) / (1.0 - 0.2) = 0.5.
    send_motion(&mut app, 0.6, 0.0);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Look"), (0.5, 0.0));
}

#[test]
fn jump_is_started_one_frame_then_ongoing_then_idle() {
    let mut app = input_app(gameplay_config());

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Jump"));
        assert!(actions.just_pressed("Jump"));
        assert_eq!(actions.phase("Jump"), ActionPhase::Started);
    }

    app.update(); // no new messages: still held
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Jump"));
        assert!(!actions.just_pressed("Jump"));
        assert_eq!(actions.phase("Jump"), ActionPhase::Ongoing);
    }

    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(!actions.pressed("Jump"));
        assert!(!actions.just_pressed("Jump"));
        assert_eq!(actions.phase("Jump"), ActionPhase::Completed);
    }

    app.update();
    assert_eq!(app.world().resource::<Actions>().phase("Jump"), ActionPhase::Idle);
}

#[test]
fn auto_repeat_does_not_retrigger_just_pressed() {
    let mut app = input_app(gameplay_config());
    send_key_repeat(&mut app, "Space", ButtonState::Pressed, false);
    app.update();
    send_key_repeat(&mut app, "Space", ButtonState::Pressed, true);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Jump"));
        assert!(!actions.just_pressed("Jump"), "auto-repeat is not a new press");
    }
}

#[test]
fn unknown_action_reads_zero_and_false() {
    let mut app = input_app(gameplay_config());
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(!actions.pressed("Nope"));
    assert!(!actions.just_pressed("Nope"));
    assert_eq!(actions.axis1d("Nope"), 0.0);
    assert_eq!(actions.axis2d("Nope"), Vec2::ZERO);
    assert_eq!(actions.value("Nope"), Vec3::ZERO);
    assert_eq!(actions.phase("Nope"), ActionPhase::Idle);
    assert_eq!(actions.kind("Nope"), None);
    assert_eq!(actions.elapsed("Nope"), 0.0);
}

#[test]
fn look_scales_the_mouse_delta_and_delta_is_cleared_after_evaluation() {
    let mut app = input_app(gameplay_config());
    send_motion(&mut app, 10.0, -5.0);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Look"), (5.0, -10.0));
    assert_eq!(
        app.world().resource::<Mouse>().delta(),
        Vec2::ZERO,
        "delta is cleared after actions are evaluated"
    );

    app.update(); // no motion this frame: Look is zero again
    assert_close(app.world().resource::<Actions>().axis2d("Look"), (0.0, 0.0));
}

#[test]
fn mouse_button_action_tracks_press_and_release() {
    let mut app = input_app(gameplay_config());
    send_mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Fire"));
        assert!(actions.just_pressed("Fire"));
        assert!(app.world().resource::<Mouse>().pressed(MouseButton::Left));
    }

    send_mouse_button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(!actions.pressed("Fire"));
        assert!(!app.world().resource::<Mouse>().pressed(MouseButton::Left));
    }
}

#[test]
fn wheel_normalises_pixels_to_lines_and_resets_next_frame() {
    let mut app = input_app(gameplay_config());
    send_wheel(&mut app, 0.0, 3.0, WheelUnit::Line);
    send_wheel(&mut app, -80.0, 0.0, WheelUnit::Pixel); // 2 lines
    app.update();
    assert_close(app.world().resource::<Mouse>().wheel(), (-2.0, 3.0));
    app.update(); // no wheel this frame
    assert_eq!(app.world().resource::<Mouse>().wheel(), Vec2::ZERO);
}

#[test]
fn keyboard_resource_tracks_press_hold_release() {
    let mut app = input_app(gameplay_config());
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    send_key(&mut app, "KeyW", ButtonState::Released);
    app.update();
    let keyboard = app.world().resource::<Keyboard>();
    assert!(!keyboard.pressed("KeyW"));
    assert!(keyboard.just_released("KeyW"));
    assert!(keyboard.pressed("KeyA"));
    assert!(keyboard.just_pressed("KeyA"));
    assert!(!keyboard.just_released("KeyA"));
}

#[test]
fn binding_to_an_unknown_action_is_ignored_without_panicking() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Ghost", InputSource::key("Space")));
    let mut app = input_app(config);
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update(); // must not panic
    assert!(!app.world().resource::<Actions>().pressed("Ghost"));
}

#[test]
fn gamepad_stick_and_button_drive_actions() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .action("Throttle", ActionKind::Axis1D)
        .binding(
            Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                .with_modifier(DeadZone::radial(0.15, 0.95)),
        )
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(GamepadButton::South),
        ))
        .binding(
            Binding::new(
                "Throttle",
                InputSource::gamepad_axis(regia_platform::GamepadAxis::RightTrigger),
            )
            .with_modifier(DeadZone::axial(0.1, 1.0)),
        );
    let mut app = input_app(config);

    connect_pad(&mut app, 3);
    app.update();
    assert_eq!(
        app.world().resource::<Gamepads>().name(3),
        Some("Test Pad"),
        "pads connect from messages"
    );

    // Left stick: raw (0.6, 0.4) → radial dead zone(0.15, 0.95):
    //   length = sqrt(0.6² + 0.4²) ≈ 0.7211
    //   rescaled = (0.7211 - 0.15) / (0.95 - 0.15) ≈ 0.7139
    //   direction preserved → (0.6/0.7211, 0.4/0.7211) * 0.7139 ≈ (0.594, 0.396)
    pad_axis(&mut app, 3, regia_platform::GamepadAxis::LeftStickX, 0.6);
    pad_axis(&mut app, 3, regia_platform::GamepadAxis::LeftStickY, 0.4);
    // Trigger analog 1.0; button pressed.
    pad_axis(&mut app, 3, regia_platform::GamepadAxis::RightTrigger, 1.0);
    pad_button(&mut app, 3, GamepadButton::South, 1.0);
    app.update();

    let actions = app.world().resource::<Actions>();
    let move_vec = actions.axis2d("Move");
    assert!(
        (move_vec.x - 0.594).abs() < EPS && (move_vec.y - 0.396).abs() < EPS,
        "{move_vec:?}"
    );
    assert!(actions.pressed("Jump") && actions.just_pressed("Jump"));
    assert!((actions.axis1d("Throttle") - 1.0).abs() < EPS);

    // Stick through the dead zone reads zero.
    pad_axis(&mut app, 3, regia_platform::GamepadAxis::LeftStickX, 0.1);
    pad_axis(&mut app, 3, regia_platform::GamepadAxis::LeftStickY, 0.0);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Move"), (0.0, 0.0));
}

// ---------------------------------------------------------------------------
// Contexts: priority, consumption
// ---------------------------------------------------------------------------

fn menu_config() -> InputConfig {
    InputConfig::new()
        .action("MenuBack", ActionKind::Button)
        .action("Pause", ActionKind::Button)
        .action("Jump", ActionKind::Button)
        .context(
            "Gameplay",
            InputContext::new(0).with_binding(Binding::new("Jump", InputSource::key("Space"))),
        )
        .context(
            "Menu",
            InputContext::consuming(10)
                .with_binding(Binding::new("MenuBack", InputSource::key("Escape")))
                .with_binding(Binding::new("Jump", InputSource::key("Space")))
                .with_binding(Binding::new("Pause", InputSource::key("Escape"))),
        )
}

#[test]
fn higher_priority_context_evaluates_and_lower_context_still_runs_when_not_consuming() {
    let mut app = input_app(menu_config());
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Gameplay"]);
    app.insert_resource(contexts);

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(
        app.world().resource::<Actions>().pressed("Jump"),
        "gameplay context is enabled directly"
    );
}

#[test]
fn consume_lower_hides_actuated_inputs_from_lower_contexts() {
    let mut app = input_app(menu_config());
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Gameplay", "Menu"]);
    app.insert_resource(contexts);

    // Space is bound by both contexts; the Menu's binding consumes it, so only MenuBack-style
    // handling wins — but Jump is *also* bound in Menu, so it stays active from the Menu.
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(
        actions.pressed("Jump"),
        "Jump is bound in the Menu context itself"
    );

    // Remove Jump from the consuming context's view by testing with Escape: bound twice in Menu
    // (MenuBack + Pause) and not in Gameplay; consumption only crosses contexts.
    send_key(&mut app, "Escape", ButtonState::Pressed);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("MenuBack") && actions.pressed("Pause"));

    // Now a lower context binding of the same input: pause-style. Rebuild with Escape in
    // Gameplay too; while Menu is enabled and consuming, the lower binding is hidden.
    let config = InputConfig::new()
        .action("Lower", ActionKind::Button)
        .action("Upper", ActionKind::Button)
        .context(
            "Lower",
            InputContext::new(0).with_binding(Binding::new("Lower", InputSource::key("Escape"))),
        )
        .context(
            "Upper",
            InputContext::consuming(10).with_binding(Binding::new("Upper", InputSource::key("Escape"))),
        );
    let mut app2 = input_app(config);
    let mut contexts2 = InputContexts::default();
    contexts2.set(0, ["Lower"]);
    app2.insert_resource(contexts2);
    send_key(&mut app2, "Escape", ButtonState::Pressed);
    app2.update();
    assert!(
        app2.world().resource::<Actions>().pressed("Lower"),
        "without the upper context, the lower binding fires"
    );

    let mut contexts3 = InputContexts::default();
    contexts3.set(0, ["Lower", "Upper"]);
    app2.insert_resource(contexts3);
    send_key(&mut app2, "Escape", ButtonState::Released);
    app2.update();
    send_key(&mut app2, "Escape", ButtonState::Pressed);
    app2.update();
    let actions = app2.world().resource::<Actions>();
    assert!(
        actions.pressed("Upper"),
        "upper context consumes the input it binds"
    );
    assert!(
        !actions.pressed("Lower"),
        "consume_lower hides the actuated input from lower contexts"
    );
}

#[test]
fn non_consuming_contexts_stack_their_values() {
    let config = InputConfig::new()
        .action("Zoom", ActionKind::Axis1D)
        .context(
            "Keys",
            InputContext::new(5).with_binding(Binding::new("Zoom", InputSource::key("ArrowUp"))),
        )
        .context(
            "Pad",
            InputContext::new(3).with_binding(Binding::new(
                "Zoom",
                InputSource::gamepad_axis(regia_platform::GamepadAxis::LeftTrigger),
            )),
        );
    let mut app = input_app(config);
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Keys", "Pad"]);
    app.insert_resource(contexts);

    connect_pad(&mut app, 0);
    app.update();
    pad_axis(&mut app, 0, regia_platform::GamepadAxis::LeftTrigger, 0.25);
    send_key(&mut app, "ArrowUp", ButtonState::Pressed);
    app.update();
    assert!((app.world().resource::<Actions>().axis1d("Zoom") - 1.25).abs() < EPS);
}

// ---------------------------------------------------------------------------
// Triggers
// ---------------------------------------------------------------------------

fn trigger_config(action: &str, trigger: Option<Trigger>) -> InputConfig {
    let mut binding = Binding::new(action, InputSource::key("Space"));
    if let Some(trigger) = trigger {
        binding = binding.with_trigger(trigger);
    }
    InputConfig::new()
        .action(action, ActionKind::Button)
        .binding(binding)
}

#[test]
fn pressed_trigger_fires_once_on_press() {
    let mut app = input_app(trigger_config("Tap", Some(Trigger::Pressed)));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.triggered("Tap"));
        assert!(actions.just_pressed("Tap"));
    }
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(!actions.triggered("Tap"));
        assert!(!actions.pressed("Tap"), "not actuated outside the fire frame");
    }
}

#[test]
fn released_trigger_fires_on_release() {
    let mut app = input_app(trigger_config("Shoot", Some(Trigger::Released)));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(!app.world().resource::<Actions>().triggered("Shoot"));

    app.update();
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.triggered("Shoot"), "fires exactly on release");
    assert_eq!(actions.phase("Shoot"), ActionPhase::Triggered);
}

#[test]
fn hold_trigger_fires_after_the_threshold_and_cancels_when_early() {
    let mut app = input_app(trigger_config("Charge", Some(Trigger::Hold(0.25))));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert_eq!(actions.phase("Charge"), ActionPhase::Started);
        assert!(actions.pressed("Charge"));
    }
    // 0.25 s at 60 Hz = 15 frames; hold through frame 16.
    for _ in 0..14 {
        app.update();
    }
    assert!(
        !app.world().resource::<Actions>().triggered("Charge"),
        "not yet held long enough"
    );
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert_eq!(actions.phase("Charge"), ActionPhase::Triggered);
        assert!(
            (actions.elapsed("Charge") - 0.2666).abs() < 0.01,
            "{}",
            actions.elapsed("Charge")
        );
    }
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Ongoing,
        "stays ongoing after firing"
    );
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Completed,
        "release after the hold completes"
    );

    // Early release: canceled.
    let mut early = input_app(trigger_config("Charge", Some(Trigger::Hold(0.25))));
    send_key(&mut early, "Space", ButtonState::Pressed);
    early.update();
    early.update();
    send_key(&mut early, "Space", ButtonState::Released);
    early.update();
    assert_eq!(
        early.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Canceled
    );
}

/// Hold and Tap on one binding. A finished hold must report `Completed` on release.
/// A quick tap still reports `Triggered` (that rank stays above both).
#[test]
fn a_finished_hold_outranks_a_failed_tap_on_the_release_frame() {
    let config = InputConfig::new().action("Charge", ActionKind::Button).binding(
        Binding::new("Charge", InputSource::key("Space"))
            .with_trigger(Trigger::Hold(0.5))
            .with_trigger(Trigger::Tap(0.2)),
    );
    let mut held = input_app(config.clone());
    send_key(&mut held, "Space", ButtonState::Pressed);
    // 40 frames at 60 Hz is about 0.67 s: past the hold and past the tap window.
    for _ in 0..40 {
        held.update();
    }
    send_key(&mut held, "Space", ButtonState::Released);
    held.update();
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Completed,
        "a finished hold is not canceled by the tap window closing"
    );

    let mut tapped = input_app(config);
    send_key(&mut tapped, "Space", ButtonState::Pressed);
    tapped.update();
    tapped.update();
    send_key(&mut tapped, "Space", ButtonState::Released);
    tapped.update();
    assert_eq!(
        tapped.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Triggered,
        "a quick tap still fires"
    );
}

#[test]
fn tap_trigger_fires_on_quick_release_and_cancels_when_slow() {
    let mut app = input_app(trigger_config("Dodge", Some(Trigger::Tap(0.15))));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    // Release within 0.15 s (9 frames): trigger on the release frame.
    for _ in 0..4 {
        app.update();
    }
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Dodge"),
        ActionPhase::Triggered
    );

    // Held too long: canceled on release.
    let mut slow = input_app(trigger_config("Dodge", Some(Trigger::Tap(0.15))));
    send_key(&mut slow, "Space", ButtonState::Pressed);
    slow.update();
    for _ in 0..12 {
        slow.update();
    }
    send_key(&mut slow, "Space", ButtonState::Released);
    slow.update();
    assert_eq!(
        slow.world().resource::<Actions>().phase("Dodge"),
        ActionPhase::Canceled
    );
}

#[test]
fn pulse_trigger_fires_immediately_and_every_interval() {
    let mut app = input_app(trigger_config("FireRate", Some(Trigger::Pulse(0.1))));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().triggered("FireRate"));

    // 0.1 s at 60 Hz = 6 frames between pulses.
    let mut fired = 1;
    for _ in 0..13 {
        app.update();
        if app.world().resource::<Actions>().triggered("FireRate") {
            fired += 1;
        }
    }
    assert!(
        (2..=4).contains(&fired),
        "fires at press and roughly every 6 frames: {fired}"
    );
}

#[test]
fn chord_trigger_needs_both_actions_and_powers_ctrl_digit_chords() {
    // The DO27 pattern: a "Ctrl" button action + digit actions chorded onto it.
    let config = InputConfig::new()
        .action("Ctrl", ActionKind::Button)
        .action("SelectCategory1", ActionKind::Button)
        .action("SelectCategory2", ActionKind::Button)
        .binding(Binding::new("Ctrl", InputSource::key("ControlLeft")))
        .binding(
            Binding::new("SelectCategory1", InputSource::key("Digit1"))
                .with_trigger(Trigger::Chord("Ctrl".into())),
        )
        .binding(
            Binding::new("SelectCategory2", InputSource::key("Digit2"))
                .with_trigger(Trigger::Chord("Ctrl".into())),
        );
    let mut app = input_app(config);

    // Digit1 alone: no chord (modifier not held).
    send_key(&mut app, "Digit1", ButtonState::Pressed);
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("SelectCategory1"));
    send_key(&mut app, "Digit1", ButtonState::Released);
    app.update();

    // Hold Ctrl, then Digit2: the chord fires one frame later at the earliest (the partner's
    // previous-frame actuation gates it), which matches how modifiers are physically held.
    send_key(&mut app, "ControlLeft", ButtonState::Pressed);
    app.update();
    send_key(&mut app, "Digit2", ButtonState::Pressed);
    app.update();
    assert!(
        app.world().resource::<Actions>().pressed("SelectCategory2"),
        "Ctrl+Digit2 chord actuates"
    );
    assert!(
        app.world().resource::<Actions>().just_pressed("SelectCategory2"),
        "the first frame the chord is satisfied is a press edge"
    );
    assert!(
        !app.world().resource::<Actions>().pressed("SelectCategory1"),
        "Digit1 is no longer held"
    );

    // Release Ctrl: the chord stops even though Digit2 is still held.
    send_key(&mut app, "ControlLeft", ButtonState::Released);
    app.update();
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("SelectCategory2"));
    send_key(&mut app, "Digit2", ButtonState::Released);
    app.update();

    // Key first: hold Digit2, then press ControlLeft. The chord stays idle on the
    // ControlLeft frame because the partner is the previous frame. The next frame,
    // with both still held, is the first frame `pressed` is true, and it is
    // `just_pressed`.
    send_key(&mut app, "Digit2", ButtonState::Pressed);
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("SelectCategory2"));
    send_key(&mut app, "ControlLeft", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(!actions.pressed("SelectCategory2"));
        assert!(!actions.just_pressed("SelectCategory2"));
    }
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.pressed("SelectCategory2") && actions.just_pressed("SelectCategory2"),
            "key-first chord: just_pressed on the first satisfied frame"
        );
    }

    // Same frame: both keys arrive in one update. That frame is idle (the partner
    // is not actuated yet). The next frame both are still held is just_pressed.
    send_key(&mut app, "ControlLeft", ButtonState::Released);
    send_key(&mut app, "Digit2", ButtonState::Released);
    app.update();
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("SelectCategory2"));
    send_key(&mut app, "ControlLeft", ButtonState::Pressed);
    send_key(&mut app, "Digit2", ButtonState::Pressed);
    app.update();
    assert!(
        !app.world().resource::<Actions>().pressed("SelectCategory2"),
        "same-frame chord waits for the partner's previous frame"
    );
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.pressed("SelectCategory2") && actions.just_pressed("SelectCategory2"),
            "same-frame chord: just_pressed on the first satisfied frame"
        );
    }
}

/// The published spec 11 § 7 sample. Kept byte-identical to the fenced block in that spec.
const SPEC_11_SAMPLE: &str = r#"InputConfig(
    actions: {
        "Move": Axis2D,
        "Look": Axis2D,
        "Jump": Button,
        "Fire": Button,
    },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Move", input: Keys2D(up: "KeyW", down: "KeyS", left: "KeyA", right: "KeyD")),
                (action: "Move", input: GamepadStick(Left), modifiers: [DeadZone((mode: Radial, lo: 0.15, hi: 0.95))]),
                (action: "Look", input: MouseDelta, modifiers: [Scale((0.1, 0.1, 0.0))]),
                (action: "Jump", input: Key("Space")),
                (action: "Fire", input: MouseButton(Left), triggers: [Pressed]),
            ],
        ),
        "Menu": (priority: 10, consume_lower: true, bindings: []),
    },
)
"#;

#[test]
fn the_spec_11_sample_parses() {
    let config = InputConfig::from_ron(SPEC_11_SAMPLE).expect("the published sample parses");
    assert_eq!(config.kind("Move"), Some(ActionKind::Axis2D));
    assert_eq!(config.kind("Look"), Some(ActionKind::Axis2D));
    assert_eq!(config.kind("Jump"), Some(ActionKind::Button));
    assert_eq!(config.kind("Fire"), Some(ActionKind::Button));
    let fire = config
        .gameplay()
        .iter()
        .find(|binding| binding.action == "Fire")
        .expect("Fire");
    assert_eq!(fire.triggers, vec![Trigger::Pressed]);
    let menu = &config.contexts["Menu"];
    assert!(menu.consume_lower && menu.priority == 10 && menu.bindings.is_empty());
}

/// Each binding runs its own triggers on its own value (an empty list is `Down`), and the
/// action takes the strongest phase of its bindings (28ce61ef7; ADR-5456 kept it, ADR-5517
/// corrected spec 11, which described the older union-on-the-combined-value rule).
#[test]
fn each_binding_runs_its_own_triggers_on_its_own_value() {
    let pressed_and_empty = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("KeyA")).with_trigger(Trigger::Pressed))
        .binding(Binding::new("Fire", InputSource::key("KeyB")));
    let mut app = input_app(pressed_and_empty);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Started,
        "the empty sibling is a Down binding: pressing it starts the action"
    );
    app.update();
    assert!(
        app.world().resource::<Actions>().pressed("Fire"),
        "holding the Down sibling holds the action"
    );
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Ongoing
    );

    let hold_on_sibling = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(1.0)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")));
    let mut held = input_app(hold_on_sibling);
    send_key(&mut held, "KeyB", ButtonState::Pressed);
    held.update();
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Started
    );
    for frame in 2..=90 {
        held.update();
        assert!(
            !held.world().resource::<Actions>().triggered("Charge"),
            "frame {frame}: KeyA's Hold never sees KeyB's value, however long KeyB is held"
        );
        assert!(held.world().resource::<Actions>().pressed("Charge"));
    }
    send_key(&mut held, "KeyB", ButtonState::Released);
    held.update();
    held.update();
    assert!(!held.world().resource::<Actions>().pressed("Charge"));

    send_key(&mut held, "KeyA", ButtonState::Pressed);
    held.update();
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Started,
        "KeyA's own Hold starts on its own press"
    );
    let mut fired_on = None;
    for frame in 2..=70 {
        held.update();
        if held.world().resource::<Actions>().triggered("Charge") {
            fired_on = Some(frame);
            break;
        }
    }
    assert_eq!(
        fired_on,
        Some(62),
        "Hold(1.0) at 60 Hz fires on frame 62 of KeyA's own press: elapsed is checked before \
         this frame's dt, and sixty f32 steps of 1/60 stay just under 1"
    );
}

#[test]
fn axis1d_stick_y_stays_on_y_until_swizzled() {
    let plain = InputConfig::new()
        .action("Throttle", ActionKind::Axis1D)
        .binding(Binding::new(
            "Throttle",
            InputSource::gamepad_axis(GamepadAxis::LeftStickY),
        ));
    let mut app = input_app(plain);
    connect_pad(&mut app, 0);
    app.update();
    pad_axis(&mut app, 0, GamepadAxis::LeftStickY, 1.0);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.axis1d("Throttle").abs() < EPS,
            "axis1d reads x, which is 0"
        );
        assert!((actions.value("Throttle").y - 1.0).abs() < EPS);
    }

    let swizzled = InputConfig::new().action("Throttle", ActionKind::Axis1D).binding(
        Binding::new("Throttle", InputSource::gamepad_axis(GamepadAxis::LeftStickY))
            .with_modifier(Modifier::Swizzle(Swizzle::Yxz)),
    );
    let mut app = input_app(swizzled);
    connect_pad(&mut app, 0);
    app.update();
    pad_axis(&mut app, 0, GamepadAxis::LeftStickY, 1.0);
    app.update();
    assert!(
        (app.world().resource::<Actions>().axis1d("Throttle") - 1.0).abs() < EPS,
        "Swizzle::Yxz moves stick Y onto x"
    );
}

#[test]
fn a_pressed_partner_does_not_chord_on_a_later_frame() {
    let config = InputConfig::new()
        .action("Ctrl", ActionKind::Button)
        .action("Slot1", ActionKind::Button)
        .binding(Binding::new("Ctrl", InputSource::key("ControlLeft")).with_trigger(Trigger::Pressed))
        .binding(
            Binding::new("Slot1", InputSource::key("Digit1")).with_trigger(Trigger::Chord("Ctrl".into())),
        );
    let mut app = input_app(config);
    send_key(&mut app, "ControlLeft", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Ctrl"),
        ActionPhase::Triggered
    );
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Ctrl"),
        ActionPhase::Idle,
        "Pressed is actuated only on its fire frame"
    );
    send_key(&mut app, "Digit1", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Slot1"),
        ActionPhase::Idle,
        "the partner's previous actuated flag is false"
    );
}

#[test]
fn releasing_a_chord_partner_ends_idle_not_completed() {
    let config = InputConfig::new()
        .action("Ctrl", ActionKind::Button)
        .action("Slot1", ActionKind::Button)
        .binding(Binding::new("Ctrl", InputSource::key("ControlLeft")))
        .binding(
            Binding::new("Slot1", InputSource::key("Digit1")).with_trigger(Trigger::Chord("Ctrl".into())),
        );
    let mut app = input_app(config);
    send_key(&mut app, "ControlLeft", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Ctrl"),
        ActionPhase::Started
    );
    send_key(&mut app, "Digit1", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Slot1"),
        ActionPhase::Started
    );
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Slot1"),
        ActionPhase::Ongoing
    );
    send_key(&mut app, "ControlLeft", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Ctrl"),
        ActionPhase::Completed
    );
    assert_eq!(
        app.world().resource::<Actions>().phase("Slot1"),
        ActionPhase::Ongoing,
        "the partner flag is the previous frame"
    );
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Slot1"),
        ActionPhase::Idle,
        "a live chord ends as Idle, not Completed"
    );
}

// ---------------------------------------------------------------------------
// Per-player device assignment
// ---------------------------------------------------------------------------

#[test]
fn per_player_device_assignment_filters_sources() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis1D)
        .action("Paddle", ActionKind::Button)
        .binding(Binding::new("Move", InputSource::key("KeyD")))
        .binding(
            Binding::new("Paddle", InputSource::gamepad_button(GamepadButton::South))
                .with_trigger(Trigger::Pressed),
        );
    let mut app = input_app(config);
    app.insert_resource(
        PlayerInputs::default()
            .with(0, DeviceAssignment::KeyboardMouse)
            .with(1, DeviceAssignment::Gamepad(7)),
    );
    // Player 1 needs its own enabled context stack.
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Gameplay"]);
    contexts.set(1, ["Gameplay"]);
    app.insert_resource(contexts);

    connect_pad(&mut app, 7);
    app.update();
    send_key(&mut app, "KeyD", ButtonState::Pressed);
    pad_button(&mut app, 7, GamepadButton::South, 1.0);
    app.update();

    let actions = app.world().resource::<Actions>();
    assert!(
        actions.player(0).states.get("Move").is_some_and(|s| s.actuated()),
        "player 0 (keyboard+mouse) sees the key"
    );
    assert!(
        !actions.player(1).states.get("Move").is_some_and(|s| s.actuated()),
        "player 1 (gamepad 7) does not see the key"
    );
    assert!(
        actions
            .player(1)
            .states
            .get("Paddle")
            .is_some_and(|s| s.actuated()),
        "player 1 sees its own pad"
    );
    assert!(
        !actions
            .player(0)
            .states
            .get("Paddle")
            .is_some_and(|s| s.actuated()),
        "player 0 does not see the pad"
    );
}

// ---------------------------------------------------------------------------
// Injection: agent protocol and virtual controls
// ---------------------------------------------------------------------------

#[test]
fn scheduled_injection_drives_actions_with_full_phase_semantics() {
    let config = InputConfig::new().action("Jump", ActionKind::Button);
    let mut app = input_app(config);
    {
        let mut injection = app.world_mut().resource_mut::<InputInjection>();
        // frame() during the first update is 1.
        injection.schedule("Jump", Vec3::X, 2, 3);
    }
    app.update(); // frame 1: idle
    assert!(!app.world().resource::<Actions>().pressed("Jump"));

    app.update(); // frame 2: Started
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Jump") && actions.just_pressed("Jump"));
        assert_eq!(actions.phase("Jump"), ActionPhase::Started);
    }
    app.update(); // frame 3: Ongoing
    app.update(); // frame 4: Ongoing (last frame of the window: start 2 + 3 = [2, 5))
    assert_eq!(
        app.world().resource::<Actions>().phase("Jump"),
        ActionPhase::Ongoing
    );
    app.update(); // frame 5: window over
    assert_eq!(
        app.world().resource::<Actions>().phase("Jump"),
        ActionPhase::Completed
    );

    // Injected unknown actions are ignored (no panic).
    app.world_mut()
        .resource_mut::<InputInjection>()
        .schedule("Ghost", Vec3::X, 100, 10);
    app.update();
}

#[test]
fn held_injection_works_like_a_virtual_stick() {
    let config = InputConfig::new().action("Move", ActionKind::Axis2D);
    let mut app = input_app(config);
    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold("Move", Vec3::new(0.0, 1.0, 0.0));
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Move"), (0.0, 1.0));
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Move"),
        ActionPhase::Ongoing
    );
    app.world_mut().resource_mut::<InputInjection>().release("Move");
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Move"), (0.0, 0.0));
}

// ---------------------------------------------------------------------------
// Rebinding: capture, conflicts, persistence across a "restart"
// ---------------------------------------------------------------------------

fn rebind_config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")))
}

#[test]
fn rebinder_captures_the_next_key_and_updates_the_live_config() {
    let mut app = input_app(rebind_config());
    app.update();
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("Gameplay", "Jump");

    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update(); // Rebinding set runs between devices and evaluation

    let outcome = app.world().resource::<InputRebinder>().outcome.clone();
    assert_eq!(
        outcome,
        Some(RebindOutcome {
            context: "Gameplay".into(),
            action: "Jump".into(),
            input: InputSource::key("KeyJ"),
            conflicts: vec![],
            slot: regia_input::RebindSlot::All,
        })
    );
    // The key still being held immediately actuates the action through the *new* binding.
    assert!(app.world().resource::<Actions>().pressed("Jump"));

    // Old binding is gone: Space no longer jumps.
    send_key(&mut app, "KeyJ", ButtonState::Released);
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("Jump"));
}

#[test]
fn rebinder_ignores_inputs_already_held_when_the_capture_starts() {
    let mut app = input_app(rebind_config());
    // The "Rebind" click: the left button goes down before the capture starts.
    send_mouse_button(&mut app, regia_platform::MouseButton::Left, ButtonState::Pressed);
    app.update();
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("Gameplay", "Jump");
    app.update();
    app.update();
    assert!(app.world().resource::<InputRebinder>().outcome.is_none());
    assert!(app.world().resource::<InputRebinder>().pending().is_some());

    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    let outcome = app.world().resource::<InputRebinder>().outcome.clone();
    assert_eq!(outcome.map(|o| o.input), Some(InputSource::key("KeyJ")));
}

#[test]
fn rebinding_persists_across_a_restart_through_the_vfs() {
    use regia_platform::{DirMount, MountPoint, Vfs, VfsPath};

    let user_dir = std::env::temp_dir().join(format!("regia-input-rebind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&user_dir);
    std::fs::create_dir_all(&user_dir).expect("temp dir");
    let path = VfsPath::parse("user://input_overrides.ron").expect("valid path");

    // Session 1: rebind Jump to KeyJ with a Vfs mounted; the overrides are saved on the spot.
    {
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(user_dir.clone()));
        let mut app = App::new();
        app.insert_resource(vfs.clone());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(InputPlugin::new(rebind_config()));
        app.update();
        app.world_mut()
            .resource_mut::<InputRebinder>()
            .start("Gameplay", "Jump");
        send_key(&mut app, "KeyJ", ButtonState::Pressed);
        app.update();
        assert_eq!(
            app.world()
                .resource::<InputRebinder>()
                .outcome
                .as_ref()
                .map(|o| &o.input),
            Some(&InputSource::key("KeyJ"))
        );
        assert!(vfs.exists(&path), "overrides written synchronously");
    }

    // Session 2 ("restart"): same Vfs, fresh app — the plugin merges the overrides at build.
    {
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(user_dir.clone()));
        let mut app = App::new();
        app.insert_resource(vfs);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(InputPlugin::new(rebind_config()));

        send_key(&mut app, "KeyJ", ButtonState::Pressed);
        app.update();
        assert!(
            app.world().resource::<Actions>().pressed("Jump"),
            "the rebound key works immediately after a restart"
        );
        send_key(&mut app, "KeyJ", ButtonState::Released);
        send_key(&mut app, "Space", ButtonState::Pressed);
        app.update();
        assert!(
            !app.world().resource::<Actions>().pressed("Jump"),
            "the original binding is replaced, not added to"
        );
    }

    let _ = std::fs::remove_dir_all(&user_dir);
}

#[test]
fn corrupt_overrides_file_is_moved_aside_and_a_rebind_still_saves() {
    use regia_platform::{DirMount, MountPoint, Vfs};

    let user_dir = std::env::temp_dir().join(format!("regia-input-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&user_dir);
    std::fs::create_dir_all(&user_dir).expect("temp dir");
    let file = user_dir.join("input_overrides.ron");
    std::fs::write(&file, b"(contexts: {broken").expect("write corrupt file");

    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(user_dir.clone()));
    let mut app = App::new();
    app.insert_resource(vfs);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(rebind_config()));
    app.update();
    // ADR-3102: the damaged file is moved aside, so its bytes are never overwritten by a save
    // and the rebind save path is open again (before, the damaged file blocked every save).
    assert!(
        !app.world().resource::<InputRebinder>().persist_blocked(),
        "the damaged file moved aside, so rebinds save"
    );
    let moved = user_dir.join("input_overrides.ron.bad-1");
    assert_eq!(
        std::fs::read(&moved).expect("the damaged copy is kept for support"),
        b"(contexts: {broken"
    );
    assert!(!file.exists(), "the damaged file no longer sits at its own path");

    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("Gameplay", "Jump");
    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();

    let rebinder = app.world().resource::<InputRebinder>();
    assert!(
        rebinder.outcome.is_some(),
        "the rebind still applies for the session"
    );
    assert!(
        rebinder.persist_error.is_none(),
        "the rebind saved (the damaged copy stayed aside)"
    );
    let saved = std::fs::read(&file).expect("the rebind wrote a fresh overrides file");
    assert_ne!(saved, b"(contexts: {broken", "the fresh file is not the damage");
    let _ = std::fs::remove_dir_all(&user_dir);
}

// ---------------------------------------------------------------------------
// Config RON
// ---------------------------------------------------------------------------

#[test]
fn config_round_trips_through_ron_and_drives_actions() {
    let text = r#"
InputConfig(
    actions: { "Move": Axis2D },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Move", input: Keys2D(up: "KeyW", down: "KeyS", left: "KeyA", right: "KeyD")),
                (action: "Move", input: GamepadStick(Left), modifiers: [DeadZone((mode: Radial, lo: 0.15, hi: 0.95))]),
            ],
        ),
    },
)
"#;
    let config = InputConfig::from_ron(text).expect("parses");
    let mut app = input_app(config);
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    app.update();
    assert_close(app.world().resource::<Actions>().axis2d("Move"), (0.0, 1.0));
}

#[test]
fn losing_window_focus_releases_held_keys_and_buttons() {
    let mut app = input_app(InputConfig::default());
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: regia_platform::MouseButton::Left,
        state: ButtonState::Pressed,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    assert!(app.world().resource::<regia_input::Keyboard>().pressed("KeyW"));
    assert!(
        app.world()
            .resource::<regia_input::Mouse>()
            .pressed(regia_platform::MouseButton::Left)
    );
    app.world_mut().write_message(regia_platform::WindowFocused {
        window: Entity::PLACEHOLDER,
        focused: false,
    });
    app.update();
    let keyboard = app.world().resource::<regia_input::Keyboard>();
    assert!(!keyboard.pressed("KeyW"));
    assert!(keyboard.just_released("KeyW"));
    assert!(
        !app.world()
            .resource::<regia_input::Mouse>()
            .pressed(regia_platform::MouseButton::Left)
    );
}

/// Audit finding 2 (ADR-4768): a press and a release inside one frame's message batch must
/// actuate the action for that one evaluation, matching the two-frame case — a fast click is
/// not kept or dropped by where the frame boundary falls. Keyboard, mouse button and gamepad
/// button all behave the same.
#[test]
fn press_and_release_in_one_batch_actuates_once() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .action("Alt", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new(
            "Fire",
            InputSource::mouse_button(regia_platform::MouseButton::Left),
        ))
        .binding(Binding::new(
            "Alt",
            InputSource::gamepad_button(GamepadButton::East),
        ));

    // Two frames: press, then release.
    let mut two_frame = input_app(config.clone());
    two_frame.world_mut().write_message(GamepadConnected {
        device: 7,
        name: "Test Pad".into(),
    });
    send_key(&mut two_frame, "Space", ButtonState::Pressed);
    send_mouse_button(
        &mut two_frame,
        regia_platform::MouseButton::Left,
        ButtonState::Pressed,
    );
    two_frame.world_mut().write_message(GamepadButtonChanged {
        device: 7,
        button: GamepadButton::East,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    two_frame.update();
    assert!(two_frame.world().resource::<Actions>().just_pressed("Jump"));
    assert!(two_frame.world().resource::<Actions>().just_pressed("Fire"));
    assert!(two_frame.world().resource::<Actions>().just_pressed("Alt"));
    two_frame.update(); // releases land here
    send_key(&mut two_frame, "Space", ButtonState::Released);
    send_mouse_button(
        &mut two_frame,
        regia_platform::MouseButton::Left,
        ButtonState::Released,
    );
    two_frame.world_mut().write_message(GamepadButtonChanged {
        device: 7,
        button: GamepadButton::East,
        state: ButtonState::Released,
        value: 0.0,
    });
    two_frame.update();
    assert!(
        !two_frame.world().resource::<Actions>().just_pressed("Jump"),
        "the edge fired on the press frame only"
    );

    // One frame: press and release in the same batch.
    let mut one_frame = input_app(config);
    one_frame.world_mut().write_message(GamepadConnected {
        device: 7,
        name: "Test Pad".into(),
    });
    send_key(&mut one_frame, "Space", ButtonState::Pressed);
    send_key(&mut one_frame, "Space", ButtonState::Released);
    send_mouse_button(
        &mut one_frame,
        regia_platform::MouseButton::Left,
        ButtonState::Pressed,
    );
    send_mouse_button(
        &mut one_frame,
        regia_platform::MouseButton::Left,
        ButtonState::Released,
    );
    one_frame.world_mut().write_message(GamepadButtonChanged {
        device: 7,
        button: GamepadButton::East,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    one_frame.world_mut().write_message(GamepadButtonChanged {
        device: 7,
        button: GamepadButton::East,
        state: ButtonState::Released,
        value: 0.0,
    });
    one_frame.update();
    let actions = one_frame.world().resource::<Actions>();
    assert!(
        actions.just_pressed("Jump"),
        "the key was down at some point in the batch: the press counts"
    );
    assert!(
        actions.just_pressed("Fire"),
        "the mouse button was down at some point in the batch"
    );
    assert!(
        actions.just_pressed("Alt"),
        "the pad button was down at some point in the batch"
    );
    one_frame.update();
    let actions = one_frame.world().resource::<Actions>();
    assert!(
        !actions.just_pressed("Jump") && !actions.pressed("Jump"),
        "the release wins from the next frame that holds no press"
    );
}

/// Each binding runs its own triggers on its own value (an empty list is `Down`), the design
/// integ-v047 kept (ADR-5456). This test replaced `triggers_are_the_union_on_the_combined_value`,
/// which a docs lane wrote against the older union rule and which the merge left failing.
#[test]
fn triggers_run_per_binding_on_that_bindings_value() {
    let pressed_and_empty = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("KeyA")).with_trigger(Trigger::Pressed))
        .binding(Binding::new("Fire", InputSource::key("KeyB")));
    let mut app = input_app(pressed_and_empty);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Started,
        "the empty-list sibling is its own Down; KeyA's Pressed does not see KeyB"
    );
    app.update();
    assert!(
        app.world().resource::<Actions>().pressed("Fire"),
        "holding the Down sibling holds the action"
    );
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Ongoing
    );

    let hold_on_sibling = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(1.0)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")));
    let mut held = input_app(hold_on_sibling);
    send_key(&mut held, "KeyB", ButtonState::Pressed);
    held.update();
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Started
    );
    for frame in 2..=90 {
        held.update();
        assert!(
            !held.world().resource::<Actions>().triggered("Charge"),
            "frame {frame}: KeyA's Hold never sees the sibling KeyB"
        );
        assert!(held.world().resource::<Actions>().pressed("Charge"));
    }
}

/// A `Hold` times its own binding: a sibling that was already held does not pre-charge it, so
/// the Hold fires about one second after ITS key goes down, not the moment it is pressed
/// (corr-input-world: `Hold` read the action-level `elapsed`, which the sibling had advanced).
#[test]
fn a_hold_times_its_own_binding_not_a_held_sibling() {
    let config = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(1.0)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")));
    let mut app = input_app(config);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    for _ in 0..120 {
        app.update();
    }
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    let mut fired_on = None;
    for frame in 1..=90 {
        app.update();
        if app.world().resource::<Actions>().triggered("Charge") {
            fired_on = Some(frame);
            break;
        }
    }
    let fired_on = fired_on.expect("the Hold on KeyA fires while KeyA is held");
    assert!(
        (55..=70).contains(&fired_on),
        "Hold(1.0) fires about 60 frames after KeyA went down, not at once: frame {fired_on}"
    );
}

/// A `Tap` times its own binding: a quick tap on KeyA still triggers while a sibling has been
/// held for a second (it read the action-level `elapsed` and called the tap too long).
#[test]
fn a_tap_times_its_own_binding_not_a_held_sibling() {
    let config = InputConfig::new()
        .action("Use", ActionKind::Button)
        .binding(Binding::new("Use", InputSource::key("KeyA")).with_trigger(Trigger::Tap(0.3)))
        .binding(Binding::new("Use", InputSource::key("KeyB")));
    let mut app = input_app(config);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    for _ in 0..60 {
        app.update();
    }
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    app.update();
    app.update();
    send_key(&mut app, "KeyA", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Use"),
        ActionPhase::Triggered,
        "a two-frame tap on KeyA triggers whatever KeyB has been doing"
    );
}

/// Each `Tap` binding keeps its own window: KeyA held far past the tap time and released while a
/// fresh KeyB tap is down is not a tap (it read the window KeyB's press opened).
#[test]
fn a_long_press_does_not_borrow_a_siblings_tap_window() {
    let config = InputConfig::new()
        .action("Use", ActionKind::Button)
        .binding(Binding::new("Use", InputSource::key("KeyA")).with_trigger(Trigger::Tap(0.3)))
        .binding(Binding::new("Use", InputSource::key("KeyB")).with_trigger(Trigger::Tap(0.3)));
    let mut app = input_app(config);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    for _ in 0..60 {
        app.update();
    }
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    app.update();
    send_key(&mut app, "KeyA", ButtonState::Released);
    app.update();
    assert_ne!(
        app.world().resource::<Actions>().phase("Use"),
        ActionPhase::Triggered,
        "a one-second press on KeyA is not a tap because KeyB was just pressed"
    );
    // KeyB's own quick tap still triggers.
    send_key(&mut app, "KeyB", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Use"),
        ActionPhase::Triggered,
        "KeyB, down for two frames, is a tap"
    );
}

/// An empty trigger list is `Down` on that binding. It does not become the sibling's
/// `Pressed` or `Hold`. The v046 docsfix test expected the opposite (a union on the
/// combined value) and failed against the per-binding evaluator: pressing the empty
/// binding reported `Started`, not `Triggered`.
#[test]
fn an_empty_trigger_list_is_down_for_that_binding_only() {
    let pressed_and_empty = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("KeyA")).with_trigger(Trigger::Pressed))
        .binding(Binding::new("Fire", InputSource::key("KeyB")));
    let mut app = input_app(pressed_and_empty);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Started,
        "the empty list is Down on KeyB; KeyA's Pressed does not see it"
    );
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Fire"));
        assert_eq!(actions.phase("Fire"), ActionPhase::Ongoing);
    }

    let hold_on_sibling = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(1.0)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")));
    let mut held = input_app(hold_on_sibling);
    send_key(&mut held, "KeyB", ButtonState::Pressed);
    held.update();
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Started
    );
    for _ in 2..=70 {
        held.update();
        assert!(
            !held.world().resource::<Actions>().triggered("Charge"),
            "holding KeyB must not run KeyA's Hold"
        );
    }
    assert_eq!(
        held.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Ongoing
    );
}

/// Each Hold binding fires once after its own threshold, even while a sibling stays down.
#[test]
fn a_second_hold_fires_while_the_first_stays_held() {
    for (first, second) in [("KeyA", "KeyB"), ("KeyB", "KeyA")] {
        let config = InputConfig::new()
            .action("Charge", ActionKind::Button)
            .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(0.2)))
            .binding(Binding::new("Charge", InputSource::key("KeyB")).with_trigger(Trigger::Hold(0.2)));
        let mut app = input_app(config);
        send_key(&mut app, first, ButtonState::Pressed);
        let mut first_fires = 0;
        for _ in 0..30 {
            app.update();
            first_fires += u32::from(app.world().resource::<Actions>().triggered("Charge"));
        }
        assert_eq!(first_fires, 1);
        send_key(&mut app, second, ButtonState::Pressed);
        let mut second_fires = 0;
        for step in 0..30 {
            app.update();
            let fired = app.world().resource::<Actions>().triggered("Charge");
            if step < 10 {
                assert!(!fired, "{second} must earn its own hold time");
            }
            second_fires += u32::from(fired);
        }
        assert_eq!(second_fires, 1, "{second} fires independently of held {first}");
        send_key(&mut app, second, ButtonState::Released);
        app.update();
        assert!(!app.world().resource::<Actions>().triggered("Charge"));
        send_key(&mut app, second, ButtonState::Pressed);
        let mut repeat_fires = 0;
        for _ in 0..30 {
            app.update();
            repeat_fires += u32::from(app.world().resource::<Actions>().triggered("Charge"));
        }
        assert_eq!(repeat_fires, 1, "releasing {second} rearms only its own latch");
    }
}
