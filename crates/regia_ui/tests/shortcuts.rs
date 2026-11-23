// SPDX-License-Identifier: MPL-2.0
//! Shortcut registry (ADR-273 lane 7, ADR-374): key presses become `ShortcutTriggered`
//! messages through the real `UiPlugin`. Headless; no wall clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_platform::ButtonState;
use regia_ui::focus::{FocusState, Focusable};
use regia_ui::prelude::*;
use regia_ui::shortcuts::{Chord, ShortcutTriggered, UiShortcut, UiShortcuts, trigger_action};
use regia_ui::{ShortcutScope, TextInput};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn key(app: &mut App, logical: &str, state: ButtonState) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: logical.into(),
        logical_key: logical.into(),
        state,
        repeat: false,
        timestamp: Default::default(),
    });
}

/// Presses and releases `logical`, runs one update, returns the actions fired.
fn tap(app: &mut App, logical: &str) -> Vec<String> {
    key(app, logical, ButtonState::Pressed);
    key(app, logical, ButtonState::Released);
    app.update();
    drain(app)
}

fn drain(app: &mut App) -> Vec<String> {
    app.world_mut()
        .resource_mut::<Messages<ShortcutTriggered>>()
        .drain()
        .map(|t| t.action)
        .collect()
}

fn register(app: &mut App, chord: &str, action: &str, scope: ShortcutScope) {
    app.world_mut()
        .resource_mut::<UiShortcuts>()
        .register(UiShortcut::new(Chord::parse(chord).unwrap(), action).in_scope(scope))
        .unwrap();
}

#[test]
fn a_chord_fires_its_action_once() {
    let mut app = app();
    register(&mut app, "Ctrl+S", "save", ShortcutScope::Global);
    assert!(tap(&mut app, "s").is_empty(), "the bare key is not the chord");
    key(&mut app, "Control", ButtonState::Pressed);
    key(&mut app, "s", ButtonState::Pressed);
    key(&mut app, "s", ButtonState::Released);
    app.update();
    assert_eq!(drain(&mut app), ["save"]);
    key(&mut app, "Control", ButtonState::Released);
    app.update();
    assert!(tap(&mut app, "s").is_empty(), "modifier release is tracked");
}

#[test]
fn contexts_gate_shortcuts_and_conflicts_fire_nothing() {
    let mut app = app();
    register(&mut app, "F1", "help", ShortcutScope::Global);
    register(
        &mut app,
        "F1",
        "context_help",
        ShortcutScope::Context("editor".into()),
    );
    register(&mut app, "F2", "a", ShortcutScope::Global);
    register(&mut app, "F2", "b", ShortcutScope::Global);
    assert_eq!(tap(&mut app, "F1"), ["help"]);
    app.world_mut()
        .resource_mut::<UiShortcuts>()
        .set_contexts(["editor"]);
    assert_eq!(tap(&mut app, "F1"), ["context_help"]);
    assert!(tap(&mut app, "F2").is_empty(), "ambiguous chord triggers nothing");
}

#[test]
fn typing_in_a_text_input_blocks_bare_key_shortcuts_only() {
    let mut app = app();
    register(&mut app, "K", "bare", ShortcutScope::Global);
    register(&mut app, "Ctrl+K", "chorded", ShortcutScope::Global);
    let input = app
        .world_mut()
        .spawn((TextInput::default(), Focusable::default()))
        .id();
    app.world_mut().resource_mut::<FocusState>().focused = Some(input);
    assert!(tap(&mut app, "k").is_empty());
    key(&mut app, "Control", ButtonState::Pressed);
    key(&mut app, "k", ButtonState::Pressed);
    app.update();
    assert_eq!(drain(&mut app), ["chorded"]);
}

#[test]
fn trigger_action_writes_the_same_message() {
    let mut app = app();
    register(
        &mut app,
        "Ctrl+B",
        "bold",
        ShortcutScope::Context("editor".into()),
    );
    let err = trigger_action(app.world_mut(), "bold").expect_err("context inactive");
    assert_eq!(err.code(), "shortcut_inactive");
    app.world_mut()
        .resource_mut::<UiShortcuts>()
        .set_contexts(["editor"]);
    let fired = trigger_action(app.world_mut(), "bold").expect("live");
    assert_eq!(fired.chord.canonical(), "Ctrl+B");
    assert_eq!(drain(&mut app), ["bold"]);
}
