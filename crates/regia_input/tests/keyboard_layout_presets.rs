// SPDX-License-Identifier: MPL-2.0
//! Physical positions stay independent of the active text-input layout.

use regia_input::{InputSource, KeyCode, KeyboardLayout};

#[test]
fn colemak_preset_maps_physical_keys() {
    let layout = KeyboardLayout::Colemak;
    for (label, physical) in [
        ("W", "KeyW"),
        ("A", "KeyA"),
        ("R", "KeyS"),
        ("S", "KeyD"),
        ("F", "KeyE"),
        ("N", "KeyJ"),
        (";", "KeyP"),
        ("O", "Semicolon"),
        ("K", "KeyN"),
    ] {
        assert_eq!(layout.physical_key(label), Some(KeyCode::new(physical)));
        assert_eq!(layout.key_label(&KeyCode::new(physical)), Some(label));
    }
    assert_eq!(
        layout.movement_keys(),
        InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")
    );
}

#[test]
fn azerty_preset_maps_physical_keys() {
    let layout = KeyboardLayout::Azerty;
    for (label, physical) in [
        ("Z", "KeyW"),
        ("Q", "KeyA"),
        ("S", "KeyS"),
        ("D", "KeyD"),
        ("A", "KeyQ"),
        ("W", "KeyZ"),
        ("M", "Semicolon"),
        (",", "KeyM"),
        (";", "Comma"),
        (":", "Period"),
        ("!", "Slash"),
    ] {
        assert_eq!(layout.physical_key(label), Some(KeyCode::new(physical)));
        assert_eq!(layout.key_label(&KeyCode::new(physical)), Some(label));
    }
    assert_eq!(
        layout.movement_keys(),
        InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")
    );
}

#[test]
fn presets_are_bijective_and_reject_ambiguous_or_unknown_labels() {
    use std::collections::BTreeSet;

    for layout in KeyboardLayout::ALL {
        let mut labels = BTreeSet::new();
        let mut codes = BTreeSet::new();
        assert_eq!(layout.keys().len(), 30);
        for (label, physical) in layout.keys() {
            assert!(labels.insert(label), "duplicate label in {layout:?}");
            assert!(codes.insert(physical), "duplicate physical code in {layout:?}");
            let key = KeyCode::new(physical);
            assert_eq!(layout.physical_key(label), Some(key.clone()));
            assert_eq!(
                layout.physical_key(&label.to_ascii_lowercase()),
                Some(key.clone())
            );
            assert_eq!(layout.key_label(&key), Some(label));
        }
        for invalid in [
            "",
            "KeyW",
            "Shift+F",
            "Backspace",
            "CapsLock",
            "1",
            "é",
            " F",
            "FF",
        ] {
            assert_eq!(layout.physical_key(invalid), None, "{layout:?}: {invalid:?}");
        }
        for physical in ["Space", "CapsLock", "Backspace", "Digit1", "Unknown", "keyw"] {
            assert_eq!(layout.key_label(&KeyCode::new(physical)), None);
        }
        assert_eq!(KeyboardLayout::parse(layout.name()), Some(layout));
        assert_eq!(
            KeyboardLayout::parse(&layout.name().to_ascii_uppercase()),
            Some(layout)
        );
    }
    for unknown in ["", " colemak", "colemak-dh", "azerty-belgian", "other"] {
        assert_eq!(KeyboardLayout::parse(unknown), None);
    }
}

#[test]
fn layout_bindings_follow_physical_events_and_leave_text_and_overrides_alone() {
    use std::time::Duration;

    use regia_app::prelude::*;
    use regia_core::math::Vec2;
    use regia_input::{
        ActionKind, Actions, Binding, GAMEPLAY_CONTEXT, InputConfig, InputOverrides, InputPlugin,
    };
    use regia_platform::{ButtonState, InputTimestamp, KeyboardInput};

    for (layout, forward_label, jump_label) in [
        (KeyboardLayout::Qwerty, "W", "E"),
        (KeyboardLayout::Colemak, "W", "F"),
        (KeyboardLayout::Azerty, "Z", "E"),
    ] {
        let mut config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button)
            .action("Custom", ActionKind::Button)
            .binding(Binding::new("Move", layout.movement_keys()))
            .binding(Binding::new(
                "Jump",
                InputSource::Key(layout.physical_key(jump_label).expect("label")),
            ))
            .binding(Binding::new("Custom", InputSource::key("Space")));
        let mut overrides = InputOverrides::default();
        overrides.set(
            GAMEPLAY_CONTEXT,
            "Custom",
            Binding::new("Custom", InputSource::key("KeyP")),
        );
        assert_eq!(config.merge_overrides(&overrides), 1);
        let saved = config.try_to_ron().expect("serialize physical bindings");
        assert!(
            !saved.contains(layout.name()),
            "no layout field in the binding format"
        );
        let config = InputConfig::from_ron(&saved).expect("round-trip physical bindings");
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(InputPlugin::new(config));
        let send = |app: &mut App, physical: &str, logical: &str, state| {
            app.world_mut().write_message(KeyboardInput {
                window: Entity::PLACEHOLDER,
                physical_key: physical.into(),
                logical_key: logical.into(),
                state,
                repeat: false,
                timestamp: InputTimestamp::ZERO,
            });
        };
        send(&mut app, "KeyW", forward_label, ButtonState::Pressed);
        send(&mut app, "KeyE", jump_label, ButtonState::Pressed);
        send(&mut app, "KeyP", "unrelated text", ButtonState::Pressed);
        app.update();
        let actions = app.world().resource::<Actions>();
        assert_eq!(actions.axis2d("Move"), Vec2::Y, "{layout:?}");
        assert!(actions.pressed("Jump"));
        assert!(
            actions.pressed("Custom"),
            "physical custom bindings remain physical"
        );

        // A release still matches its physical press after the OS text layout changes.
        send(&mut app, "KeyW", "different text", ButtonState::Released);
        send(&mut app, "KeyE", "different text", ButtonState::Released);
        send(&mut app, "KeyP", "different text", ButtonState::Released);
        app.update();
        let actions = app.world().resource::<Actions>();
        assert_eq!(actions.axis2d("Move"), Vec2::ZERO);
        assert!(!actions.pressed("Jump"));
        assert!(!actions.pressed("Custom"));

        // A logical label alone must never actuate the movement binding.
        send(&mut app, "KeyQ", forward_label, ButtonState::Pressed);
        app.update();
        assert_eq!(app.world().resource::<Actions>().axis2d("Move"), Vec2::ZERO);
    }
}
