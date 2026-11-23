// SPDX-License-Identifier: MPL-2.0
//! Look sensitivity and Y invert (ADR-3464). Synthetic mouse and gamepad deltas, fixed frames.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_input::prelude::*;
use regia_platform::{GamepadAxis, GamepadAxisChanged, GamepadConnected, MouseMotion};

const EPS: f32 = 1e-4;

fn app(config: InputConfig, look: LookSettings) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app.insert_resource(look);
    app
}

fn close(value: Vec2, expected: (f32, f32)) {
    assert!(
        (value.x - expected.0).abs() < EPS && (value.y - expected.1).abs() < EPS,
        "{value:?} != {expected:?}"
    );
}

fn look_config() -> InputConfig {
    InputConfig::new()
        .action("Look", ActionKind::Axis2D)
        .action("Cursor", ActionKind::Axis2D)
        .look_action("Look")
        .binding(Binding::new("Look", InputSource::MouseDelta))
        .binding(Binding::new("Cursor", InputSource::MouseDelta))
        .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
        .binding(Binding::new("Cursor", InputSource::gamepad_stick(Stick::Right)))
}

#[test]
fn look_sensitivity_and_invert_y_scale_mouse_and_gamepad_when_the_action_is_named() {
    let look = LookSettings {
        sensitivity: 2.0,
        invert_y: true,
    };
    let mut mouse = app(look_config(), look);
    mouse.world_mut().write_message(MouseMotion {
        delta: (10.0, -4.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    mouse.update();
    let actions = mouse.world().resource::<Actions>();
    close(actions.axis2d("Look"), (20.0, 8.0));
    close(actions.axis2d("Cursor"), (10.0, -4.0));

    // An injected value replaces the device value and is not scaled.
    mouse
        .world_mut()
        .resource_mut::<InputInjection>()
        .hold("Look", Vec3::new(3.0, 4.0, 0.0));
    mouse.world_mut().write_message(MouseMotion {
        delta: (10.0, -4.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    mouse.update();
    close(mouse.world().resource::<Actions>().axis2d("Look"), (3.0, 4.0));

    let mut pad = app(look_config(), look);
    pad.world_mut().write_message(GamepadConnected {
        device: 1,
        name: "Test Pad".into(),
    });
    pad.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::RightStickX,
        value: 0.5,
    });
    pad.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::RightStickY,
        value: 1.0,
    });
    pad.update();
    let actions = pad.world().resource::<Actions>();
    close(actions.axis2d("Look"), (1.0, -2.0));
    close(actions.axis2d("Cursor"), (0.5, 1.0));

    let mut clamped = app(
        look_config(),
        LookSettings {
            sensitivity: 1000.0,
            invert_y: false,
        },
    );
    clamped.world_mut().write_message(MouseMotion {
        delta: (1.0, 0.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    clamped.update();
    close(
        clamped.world().resource::<Actions>().axis2d("Look"),
        (LookSettings::SENSITIVITY_MAX, 0.0),
    );

    let mut missing = app(
        look_config(),
        LookSettings {
            sensitivity: f32::NAN,
            invert_y: false,
        },
    );
    missing.world_mut().write_message(MouseMotion {
        delta: (4.0, -1.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    missing.update();
    close(missing.world().resource::<Actions>().axis2d("Look"), (4.0, -1.0));
}

#[test]
fn unset_look_actions_leave_sensitivity_and_invert_y_unread() {
    let config = InputConfig::new()
        .action("Look", ActionKind::Axis2D)
        .binding(Binding::new("Look", InputSource::MouseDelta));
    let text = ron::to_string(&config).expect("serializes");
    assert!(!text.contains("look_actions"), "{text}");
    let back: InputConfig = ron::from_str(&text).expect("parses");
    assert!(back.look_actions.is_empty());

    let mut app = app(
        config,
        LookSettings {
            sensitivity: 2.0,
            invert_y: true,
        },
    );
    app.world_mut().write_message(MouseMotion {
        delta: (10.0, -4.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    close(app.world().resource::<Actions>().axis2d("Look"), (10.0, -4.0));
}

#[test]
fn zero_step_frames_apply_look_settings_without_advancing_the_simulation() {
    let mut app = app(
        look_config(),
        LookSettings {
            sensitivity: 2.0,
            invert_y: true,
        },
    );
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
    app.world_mut().write_message(MouseMotion {
        delta: (10.0, -4.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    let actions = app.world().resource::<Actions>();
    close(actions.axis2d("Look"), (20.0, 8.0));
    close(actions.axis2d("Cursor"), (10.0, -4.0));
}
