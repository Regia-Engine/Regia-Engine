// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `regia_input::rebind` and `config` (ADR-3389): each one pins a
//! behavior a mutant broke while the older tests still passed.

use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{
    InputRebinder, RebindMode, RebindSlot, SKIP_VIDEO_ACTION, VIDEO_CONTEXT,
    input_overrides_migration_sources, input_overrides_schema_fingerprint,
};
use regia_platform::{ButtonState, GamepadButton, KeyboardInput};

#[test]
fn rebind_slots_have_stable_names_that_parse_back() {
    let names: Vec<&str> = [RebindSlot::All, RebindSlot::KeyboardMouse, RebindSlot::Gamepad]
        .into_iter()
        .map(RebindSlot::name)
        .collect();
    assert_eq!(names, ["all", "keyboard_mouse", "gamepad"]);
    for slot in [RebindSlot::All, RebindSlot::KeyboardMouse, RebindSlot::Gamepad] {
        assert_eq!(RebindSlot::parse(slot.name()), Some(slot));
    }
    assert_eq!(RebindSlot::parse("pad"), None);
    assert_eq!(RebindSlot::parse(""), None);
}

#[test]
fn the_rebinder_reports_its_slot_mode_and_persist_block() {
    let mut rebinder = InputRebinder::default();
    assert!(!rebinder.persist_blocked());
    rebinder.block_persist();
    assert!(rebinder.persist_blocked());
    rebinder.allow_persist();
    assert!(!rebinder.persist_blocked(), "allow_persist lifts the block");
    rebinder.start_slot("Gameplay", "Move", RebindMode::Keys2D, RebindSlot::Gamepad);
    assert_eq!(rebinder.slot(), RebindSlot::Gamepad);
    assert_eq!(rebinder.mode(), RebindMode::Keys2D);
}

#[test]
fn the_overrides_shape_names_its_fields_and_variants() {
    let fingerprint = input_overrides_schema_fingerprint();
    for name in [
        "format_version",
        "slots",
        "KeyboardMouse",
        "Keys2D",
        "South",
        "LeftStickX",
        "Negate",
        "Chord",
    ] {
        assert!(fingerprint.contains(name), "the fingerprint names `{name}`");
    }
    assert_eq!(input_overrides_migration_sources(), vec![1]);
}

#[test]
fn an_overrides_file_without_a_version_is_version_one() {
    let raw: InputOverrides = ron::from_str("(contexts: {})").expect("a version 1 file");
    assert_eq!(
        raw.format_version, 1,
        "the serde default of a file without the field"
    );
    let loaded = InputOverrides::from_ron("(contexts: {})").expect("loads");
    assert_eq!(
        loaded.format_version,
        regia_input::OVERRIDES_VERSION,
        "loading brings it to the current version"
    );
}

#[test]
fn merging_slot_overrides_counts_only_replaced_bindings() {
    let base = || {
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")))
    };
    let mut keyboard = InputOverrides::default();
    keyboard.set_slot(
        "Gameplay",
        "Jump",
        RebindSlot::KeyboardMouse,
        vec![Binding::new("Jump", InputSource::key("KeyJ"))],
    );
    let mut config = base();
    assert_eq!(
        config.merge_overrides(&keyboard),
        1,
        "the Space binding was replaced"
    );
    let mut pad = InputOverrides::default();
    pad.set_slot(
        "Gameplay",
        "Jump",
        RebindSlot::Gamepad,
        vec![Binding::new(
            "Jump",
            InputSource::GamepadButton(GamepadButton::South),
        )],
    );
    let mut config = base();
    assert_eq!(
        config.merge_overrides(&pad),
        0,
        "nothing on the gamepad was replaced, one binding was added"
    );
    assert_eq!(config.contexts["Gameplay"].bindings.len(), 2);
}

#[test]
fn the_skip_video_default_binds_only_when_the_project_does_not() {
    let added = InputConfig::new().with_skip_video_default();
    assert_eq!(added.actions.get(SKIP_VIDEO_ACTION), Some(&ActionKind::Button));
    let video = &added.contexts[VIDEO_CONTEXT];
    assert_eq!(video.priority, 100);
    assert!(video.bindings.len() >= 5, "Escape, Space, Enter, South and Start");
    assert!(video.bindings.iter().all(|b| b.action == SKIP_VIDEO_ACTION));

    let own = InputConfig::new()
        .action(SKIP_VIDEO_ACTION, ActionKind::Button)
        .binding(Binding::new(SKIP_VIDEO_ACTION, InputSource::key("KeyX")))
        .with_skip_video_default();
    assert!(
        !own.contexts.contains_key(VIDEO_CONTEXT),
        "the project's own binding wins"
    );
    let bound: usize = own.contexts.values().map(|c| c.bindings.len()).sum();
    assert_eq!(bound, 1);
}

fn press_frame(app: &mut App, keys: &[&str]) {
    for k in keys {
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: k.to_string(),
            logical_key: k.to_string(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
    }
    app.update();
}

#[test]
fn four_keys_feed_a_1d_axis_with_left_and_right_only_and_negate_y_flips_forward() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.125)));
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Turn", ActionKind::Axis1D)
            .action("Move", ActionKind::Axis2D)
            .binding(Binding::new(
                "Turn",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(
                Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")).with_modifier(
                    Modifier::Negate {
                        x: false,
                        y: true,
                        z: false,
                    },
                ),
            ),
    ));
    app.update();
    press_frame(&mut app, &["KeyW"]);
    let actions = app.world().resource::<Actions>();
    assert_eq!(
        actions.value("Turn"),
        regia_core::math::Vec3::ZERO,
        "up is not part of a 1D axis"
    );
    assert_eq!(
        actions.axis2d("Move"),
        regia_core::math::Vec2::new(0.0, -1.0),
        "forward, negated"
    );
    press_frame(&mut app, &["KeyD"]);
    assert_eq!(app.world().resource::<Actions>().axis1d("Turn"), 1.0);
}

#[test]
fn chord_helpers_are_recognized_for_every_slot() {
    use regia_input::{chord_helper, chord_helper_in, is_chord_helper};
    assert!(is_chord_helper(&chord_helper("Save")));
    for slot in [RebindSlot::All, RebindSlot::KeyboardMouse, RebindSlot::Gamepad] {
        assert!(is_chord_helper(&chord_helper_in("Save", slot)), "{slot:?}");
    }
    assert!(!is_chord_helper("Save"));
    assert!(!is_chord_helper("chord"));
}
