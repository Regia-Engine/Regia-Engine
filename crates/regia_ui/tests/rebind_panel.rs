// SPDX-License-Identifier: MPL-2.0
//! The input rebinding panel (ADR-1801): rows, capture by click, key-group axes, chords, reset,
//! conflicts and persistence through the real `UiPlugin` and `InputPlugin`. Headless and
//! deterministic: manual 60 Hz time, no sleeps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_platform::ButtonState;
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::{NodeName, RebindButton, RebindPanel, Text};

fn config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Crouch", ActionKind::Button)
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new("Crouch", InputSource::key("KeyC")))
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    app.add_plugins(InputPlugin::new(config()));
    app.world_mut().spawn((
        UiRoot::default(),
        RebindPanel {
            context: "Gameplay".into(),
            actions: Vec::new(),
        },
    ));
    app.update();
    app
}

fn node(app: &mut App, name: &str) -> Entity {
    app.world_mut()
        .query::<(Entity, &NodeName)>()
        .iter(app.world())
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node named {name}"))
}

/// The label of the text under (or on) the node.
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

#[test]
fn panel_builds_a_row_per_action_with_current_bindings() {
    let mut app = app();
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Space");
    assert_eq!(label(&mut app, "rebind.Gameplay.Crouch.bind"), "C");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.bind"), "W/S/A/D");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.alt"), "Chord");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.alt"), "Keys");
    node(&mut app, "rebind.Gameplay.reset_all");
    // Built once: a second frame adds no rows.
    let before = app.world_mut().query::<&RebindButton>().iter(app.world()).count();
    app.update();
    let after = app.world_mut().query::<&RebindButton>().iter(app.world()).count();
    assert_eq!(before, after);
    assert_eq!(before, 3 + 3, "primary buttons plus one alt each");
}

#[test]
fn panel_with_an_action_list_shows_only_those_rows_in_order() {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    app.add_plugins(InputPlugin::new(config()));
    app.world_mut().spawn((
        UiRoot::default(),
        RebindPanel {
            context: "Gameplay".into(),
            actions: vec!["Move".into(), "Missing".into()],
        },
    ));
    app.update();
    node(&mut app, "rebind.Gameplay.Move.bind");
    let rows = app
        .world_mut()
        .query::<&NodeName>()
        .iter(app.world())
        .filter(|n| n.0.ends_with(".row"))
        .count();
    assert_eq!(rows, 1);
}

#[test]
fn clicking_the_binding_captures_the_next_key_and_shows_it() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Press an input...");
    tap(&mut app, "KeyJ");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "J");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.status"), "");
}

#[test]
fn clicking_while_capturing_cancels() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    assert!(app.world().resource::<InputRebinder>().pending().is_some());
    click(&mut app, "rebind.Gameplay.Jump.bind");
    assert!(app.world().resource::<InputRebinder>().pending().is_none());
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Space");
}

#[test]
fn axis_keys_capture_shows_progress_then_the_four_keys() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Move.alt");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.alt"), "Press up (1/4)");
    tap(&mut app, "ArrowUp");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Move.alt"),
        "Press down (2/4) after Up"
    );
    tap(&mut app, "ArrowDown");
    tap(&mut app, "ArrowLeft");
    tap(&mut app, "ArrowRight");
    assert_eq!(label(&mut app, "rebind.Gameplay.Move.bind"), "Up/Down/Left/Right");
}

#[test]
fn chord_capture_shows_modifier_plus_key() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.alt");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Jump.alt"),
        "Press modifier (1/2)"
    );
    tap(&mut app, "ControlLeft");
    tap(&mut app, "KeyJ");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Left Control + J");
}

#[test]
fn conflict_is_shown_in_the_row_status() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    tap(&mut app, "KeyC");
    assert_eq!(
        label(&mut app, "rebind.Gameplay.Jump.status"),
        "Also used by: Crouch"
    );
    // Capturing again clears it.
    click(&mut app, "rebind.Gameplay.Jump.bind");
    tap(&mut app, "KeyJ");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.status"), "");
}

#[test]
fn reset_buttons_restore_one_or_all_actions() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    tap(&mut app, "KeyJ");
    click(&mut app, "rebind.Gameplay.Crouch.bind");
    tap(&mut app, "KeyX");
    click(&mut app, "rebind.Gameplay.Jump.reset");
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Space");
    assert_eq!(label(&mut app, "rebind.Gameplay.Crouch.bind"), "X");
    click(&mut app, "rebind.Gameplay.reset_all");
    assert_eq!(label(&mut app, "rebind.Gameplay.Crouch.bind"), "C");
}

#[test]
fn a_rebind_through_the_panel_changes_what_the_game_reads() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    tap(&mut app, "KeyJ");
    key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("Jump"));
    key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Jump"));
}

/// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932): a phone player who started a capture by
/// touch and pressed the system Back bound Back to the action. Back leaves a capture, as Escape
/// and the pad's Start do, and is never captured.
#[test]
fn the_android_back_key_leaves_a_capture_and_is_not_bound() {
    let mut app = app();
    click(&mut app, "rebind.Gameplay.Jump.bind");
    assert!(app.world().resource::<InputRebinder>().pending().is_some());
    tap(&mut app, "BrowserBack");
    assert!(app.world().resource::<InputRebinder>().pending().is_none());
    assert_eq!(label(&mut app, "rebind.Gameplay.Jump.bind"), "Space");
}
