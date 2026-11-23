// SPDX-License-Identifier: MPL-2.0
//! Device updates under one frame's mixed message batch (concurrency audit 11-03): a press
//! delivered in the same batch as a focus loss, a gamepad button change in the same batch as its
//! disconnect, and the order of the device systems themselves.
//!
//! Frames are manual 60 Hz. Nothing here sleeps or reads the wall clock.

use std::time::Duration;

use regia_app::prelude::bevy_ecs::message::{MessageCursor, Messages};
use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{
    ActionKind, ActionPhase, Binding, InputConfig, InputDevice, InputSource, TouchControlKind, TouchControls,
    TouchMouseEmulation, TouchRect, TouchZone,
};
use regia_platform::{
    ButtonState, GamepadButtonChanged, GamepadConnected, GamepadDisconnected, KeyboardInput,
    MouseButtonInput, TouchInput, TouchPhase, WindowFocused,
};

fn input_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::default());
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

fn mouse_button(app: &mut App, button: MouseButton, state: ButtonState) {
    app.world_mut().write_message(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn touch(app: &mut App, id: u64, phase: TouchPhase, position: (f64, f64)) {
    app.world_mut().write_message(TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn focus(app: &mut App, focused: bool) {
    app.world_mut().write_message(WindowFocused {
        window: Entity::PLACEHOLDER,
        focused,
    });
}

/// Alt-Tab: the Alt press and the focus loss arrive in one batch; the release goes to the other
/// app. The key must not stay held.
#[test]
fn a_press_in_the_same_batch_as_a_focus_loss_is_not_left_held() {
    let mut app = input_app();
    key(&mut app, "AltLeft", ButtonState::Pressed);
    mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
    focus(&mut app, false);
    app.update();
    let keyboard = app.world().resource::<Keyboard>();
    assert!(
        !keyboard.pressed("AltLeft"),
        "a key pressed before the loss stays held"
    );
    assert!(
        keyboard.just_released("AltLeft"),
        "the cleared key reports its release"
    );
    let mouse = app.world().resource::<Mouse>();
    assert!(
        !mouse.pressed(MouseButton::Left),
        "a button pressed before the loss stays held"
    );
    assert!(mouse.just_released(MouseButton::Left));
    // Nothing comes back on later frames.
    app.update();
    assert!(!app.world().resource::<Keyboard>().pressed("AltLeft"));
}

/// Focus lost and regained inside one batch, then a press: the press happened while focused and
/// stays held.
#[test]
fn a_press_after_focus_comes_back_in_the_same_batch_stays_held() {
    let mut app = input_app();
    key(&mut app, "KeyW", ButtonState::Pressed);
    app.update();
    focus(&mut app, false);
    focus(&mut app, true);
    key(&mut app, "KeyW", ButtonState::Pressed);
    app.update();
    let keyboard = app.world().resource::<Keyboard>();
    assert!(keyboard.pressed("KeyW"));
    assert!(
        keyboard.just_released("KeyW"),
        "the earlier hold was released by the loss"
    );
}

/// Focus moving between two of the app's windows is not an app focus loss, whichever order the
/// platform reports the two messages in: a press in that batch stays held.
#[test]
fn focus_moving_between_two_app_windows_keeps_the_press() {
    for lost_first in [true, false] {
        let mut app = input_app();
        let a = app.world_mut().spawn_empty().id();
        let b = app.world_mut().spawn_empty().id();
        key(&mut app, "KeyE", ButtonState::Pressed);
        let messages = if lost_first {
            [(a, false), (b, true)]
        } else {
            [(b, true), (a, false)]
        };
        for (window, focused) in messages {
            app.world_mut().write_message(WindowFocused { window, focused });
        }
        app.update();
        assert!(
            app.world().resource::<Keyboard>().pressed("KeyE"),
            "lost first: {lost_first}"
        );
    }
}

/// A finger that goes down in the batch that ends with a focus loss never gets its finger-up:
/// it is cancelled, and touch mouse emulation does not leave the left button held.
#[test]
fn a_touch_started_in_the_same_batch_as_a_focus_loss_is_cancelled() {
    let mut app = input_app();
    app.insert_resource(TouchMouseEmulation(true));
    touch(&mut app, 1, TouchPhase::Started, (10.0, 10.0));
    focus(&mut app, false);
    app.update();
    let touches = app.world().resource::<Touches>();
    assert_eq!(touches.count(), 0, "the new touch is live while unfocused");
    assert!(
        !app.world().resource::<Mouse>().pressed(MouseButton::Left),
        "emulation holds the left button for a cancelled touch"
    );
}

/// Cancellation is terminal for the current touch sequence, even if a delayed duplicate end
/// arrives in the same platform batch.
#[test]
fn cancel_then_duplicate_end_does_not_turn_into_a_tap() {
    let mut app = input_app();
    touch(&mut app, 7, TouchPhase::Started, (12.0, 18.0));
    app.update();

    touch(&mut app, 7, TouchPhase::Cancelled, (12.0, 18.0));
    touch(&mut app, 7, TouchPhase::Ended, (12.0, 18.0));
    touch(&mut app, 7, TouchPhase::Ended, (12.0, 18.0));
    app.update();

    let touches = app.world().resource::<Touches>();
    assert_eq!(
        touches.get(7).map(|point| point.phase),
        Some(TouchPhase::Cancelled)
    );
    let mut cursor = MessageCursor::<regia_input::GestureEvent>::default();
    assert!(
        cursor
            .read(app.world().resource::<Messages<regia_input::GestureEvent>>())
            .all(|event| !matches!(event, regia_input::GestureEvent::Tap { .. }))
    );
}

#[test]
fn independent_pointer_cancellation_reuse_and_focus_resume_have_exact_action_edges() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Fire", ActionKind::Button)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ));
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app.insert_resource(TouchMouseEmulation(true));
    app.world_mut().spawn((
        regia_platform::Window {
            physical_size: (1000, 500),
            ..Default::default()
        },
        regia_platform::PrimaryWindow,
    ));
    let mut touch_controls = TouchControls::default();
    touch_controls.zones = vec![
        TouchZone {
            name: "move".into(),
            rect: TouchRect::left_half(),
            action: "Move".into(),
            kind: TouchControlKind::Stick {
                radius_px: 100.0,
                dead_zone: 0.0,
            },
        },
        TouchZone {
            name: "fire".into(),
            rect: TouchRect::right_half(),
            action: "Fire".into(),
            kind: TouchControlKind::Button,
        },
    ];
    app.insert_resource(touch_controls);

    touch(&mut app, 11, TouchPhase::Started, (100.0, 100.0));
    touch(&mut app, 22, TouchPhase::Started, (800.0, 100.0));
    app.update();
    assert_eq!(app.world().resource::<Touches>().count(), 2);
    assert!(app.world().resource::<Actions>().pressed("Fire"));

    touch(&mut app, 11, TouchPhase::Moved, (150.0, 100.0));
    touch(&mut app, 22, TouchPhase::Moved, (810.0, 100.0));
    app.update();
    assert_eq!(app.world().resource::<Actions>().axis2d("Move").x, 0.5);

    touch(&mut app, 11, TouchPhase::Cancelled, (150.0, 100.0));
    touch(&mut app, 22, TouchPhase::Moved, (820.0, 100.0));
    app.update();
    assert_eq!(app.world().resource::<Touches>().count(), 1);
    assert_eq!(
        app.world().resource::<Touches>().get(11).map(|point| point.phase),
        Some(TouchPhase::Cancelled)
    );
    assert_eq!(
        app.world()
            .resource::<Touches>()
            .get(22)
            .map(|point| point.current.x),
        Some(820.0)
    );
    let actions = app.world().resource::<Actions>();
    assert_eq!(actions.phase("Move"), ActionPhase::Completed);
    assert!(!actions.pressed("Move"));
    assert!(actions.pressed("Fire"));

    touch(&mut app, 22, TouchPhase::Cancelled, (820.0, 100.0));
    app.update();
    assert_eq!(app.world().resource::<Touches>().count(), 0);
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Completed
    );
    assert!(app.world().resource::<Mouse>().just_released(MouseButton::Left));

    // An id may be reused after its prior terminal phase has been observed and pruned.
    app.update();
    touch(&mut app, 11, TouchPhase::Started, (800.0, 100.0));
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Fire"));

    // A suspension/focus-loss swallows the platform's end; the resumed stream starts cleanly.
    focus(&mut app, false);
    app.update();
    assert_eq!(app.world().resource::<Touches>().count(), 0);
    assert_eq!(
        app.world().resource::<Actions>().phase("Fire"),
        ActionPhase::Completed
    );
    focus(&mut app, true);
    touch(&mut app, 33, TouchPhase::Started, (820.0, 120.0));
    app.update();
    assert_eq!(app.world().resource::<Touches>().count(), 1);
    assert!(app.world().resource::<Actions>().pressed("Fire"));
}

/// A trigger change and the pad's disconnect land in one poll batch (unplug while playing): the
/// pad must stay gone instead of being recreated with the trigger stuck at full.
#[test]
fn a_button_change_in_the_same_batch_as_a_disconnect_does_not_resurrect_the_pad() {
    let mut app = input_app();
    app.world_mut().write_message(GamepadConnected {
        device: 4,
        name: "Test Pad".into(),
    });
    app.update();
    assert_eq!(app.world().resource::<Gamepads>().connected(), 1);
    app.world_mut().write_message(GamepadButtonChanged {
        device: 4,
        button: GamepadButton::RightTrigger,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.world_mut().write_message(GamepadDisconnected { device: 4 });
    app.update();
    let pads = app.world().resource::<Gamepads>();
    assert_eq!(
        pads.connected(),
        0,
        "a phantom pad was recreated: {:?}",
        pads.pad(4)
    );
    // A later event for the id (a reconnect that skipped its Connected) still brings it back.
    app.world_mut().write_message(GamepadButtonChanged {
        device: 4,
        button: GamepadButton::South,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.update();
    assert_eq!(app.world().resource::<Gamepads>().connected(), 1);
}

/// The device systems share `Mouse` (touch emulation) and `LastUsedDevice`; with no order between
/// them the result of a mixed frame depends on the executor. They must be ordered.
#[test]
fn the_device_systems_have_a_fixed_order() {
    let mut app = input_app();
    let report = app.ambiguity_report();
    let devices = [
        "update_keyboard",
        "update_mouse",
        "update_touches",
        "update_gamepads",
    ];
    let unordered: Vec<String> = report
        .schedules
        .iter()
        .filter(|s| s.schedule == "PreUpdate")
        .flat_map(|s| s.pairs.iter())
        .filter(|p| devices.contains(&p.a.as_str()) && devices.contains(&p.b.as_str()))
        .map(|p| format!("{} / {} on {:?}", p.a, p.b, p.conflicts))
        .collect();
    assert!(unordered.is_empty(), "unordered device systems: {unordered:?}");
}

/// With the order fixed, a frame with a key press and pad activity reports the pad (the last
/// system in the chain) as the last-used device, every time.
#[test]
fn last_used_device_is_stable_for_a_mixed_frame() {
    let mut app = input_app();
    app.world_mut().write_message(GamepadConnected {
        device: 2,
        name: "Test Pad".into(),
    });
    app.update();
    key(&mut app, "KeyA", ButtonState::Pressed);
    app.world_mut().write_message(GamepadButtonChanged {
        device: 2,
        button: GamepadButton::South,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.update();
    assert_eq!(
        app.world().resource::<LastUsedDevice>().0,
        Some(InputDevice::Gamepad(2))
    );
}
