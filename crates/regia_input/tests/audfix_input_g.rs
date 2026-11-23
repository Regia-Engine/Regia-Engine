// SPDX-License-Identifier: MPL-2.0
//! Regressions for the post-v040 input audit (lane sprint-audfix-input-g).
//!
//! Frames are manual 60 Hz. Nothing here sleeps or reads the wall clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_input::prelude::*;
use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadConnected, KeyboardInput};

fn input_app(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app
}

fn send_key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// Two Tap bindings are alternatives. The one that is up must not cancel the tap the other
/// binding is performing.
#[test]
fn audfix_two_tap_bindings_still_tap() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")).with_trigger(Trigger::Tap(0.5)))
        .binding(Binding::new("Jump", InputSource::key("KeyF")).with_trigger(Trigger::Tap(0.5)));
    let mut app = input_app(config);
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Jump"),
        ActionPhase::Started,
        "the press opens the tap"
    );
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Jump"),
        ActionPhase::Triggered,
        "releasing Space inside the window is a tap; KeyF staying up must not cancel it"
    );
}

/// Two Pulse bindings that are held together advance the action's pulse timer once per step.
#[test]
fn audfix_two_pulse_bindings_share_one_timer() {
    let config = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("Space")).with_trigger(Trigger::Pulse(0.5)))
        .binding(Binding::new("Fire", InputSource::key("KeyF")).with_trigger(Trigger::Pulse(0.5)));
    let mut app = input_app(config);
    send_key(&mut app, "Space", ButtonState::Pressed);
    send_key(&mut app, "KeyF", ButtonState::Pressed);
    let mut fired = 0u32;
    // 40 steps of 1/60 s. A 0.5 s pulse fires on the press and again at step 30: twice.
    for _ in 0..40 {
        app.update();
        if app.world().resource::<Actions>().triggered("Fire") {
            fired += 1;
        }
    }
    assert_eq!(
        fired, 2,
        "0.5 s pulse over 40 steps of 1/60 s fires twice, not once per binding"
    );
}

/// Releasing one of two held `Hold` bindings must not fire the hold a second time.
/// The binding that is still down owns the hold.
#[test]
fn audfix_releasing_one_hold_does_not_refire_the_other() {
    let config = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("Space")).with_trigger(Trigger::Hold(0.2)))
        .binding(Binding::new("Charge", InputSource::key("KeyF")).with_trigger(Trigger::Hold(0.2)));
    let mut app = input_app(config);
    send_key(&mut app, "Space", ButtonState::Pressed);
    send_key(&mut app, "KeyF", ButtonState::Pressed);
    let mut fires = 0u32;
    for _ in 0..30 {
        app.update();
        if app.world().resource::<Actions>().triggered("Charge") {
            fires += 1;
        }
    }
    assert_eq!(
        fires, 1,
        "the hold fires once while both keys are down, got {fires}"
    );
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("Charge"), "KeyF is still held");
    assert!(
        !actions.triggered("Charge"),
        "releasing Space must not fire the hold again, phase {:?}",
        actions.phase("Charge")
    );
}

/// A non-finite stick sample must not actuate the action or stick in the value.
#[test]
fn audfix_non_finite_pad_axis_does_not_actuate() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)));
    let mut app = input_app(config);
    app.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Pad".into(),
    });
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::LeftStickX,
        value: f32::NAN,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.value("Move").is_finite(),
            "NaN axis stored into the action: {:?}",
            actions.value("Move")
        );
        assert!(!actions.pressed("Move"), "a NaN stick sample is not a deflection");
        assert_eq!(actions.value("Move"), Vec3::ZERO);
    }
    assert_eq!(
        app.world()
            .resource::<Gamepads>()
            .axis(1, GamepadAxis::LeftStickX),
        0.0,
        "NaN must not be stored on the pad"
    );
}
