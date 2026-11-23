// SPDX-License-Identifier: MPL-2.0
//! The focus graph through real gamepad and keyboard presses (UI lane 9, ADR-516): focus groups
//! that wrap, stop or remember, Tab indices, and focus repair when the focused node goes away.
//! Headless: no window, no GPU, fixed 16 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged};
use regia_ui::focus::{DefaultFocus, FocusScope, FocusState, Focusable, NavDirection, UiInputMode};
use regia_ui::focus_graph::{
    FocusGroup, FocusMemory, NavError, NavReason, NavStep, TabIndex, nav_move, world_graph,
};
use regia_ui::prelude::*;
use regia_ui::{Focused, UiEvent, UiEventKind};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn computed(rect: [f32; 4]) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        ..Default::default()
    }
}

fn node(app: &mut App, rect: [f32; 4]) -> Entity {
    app.world_mut().spawn((Focusable::default(), computed(rect))).id()
}

fn child(app: &mut App, parent: Entity, rect: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((Focusable::default(), computed(rect), ChildOf(parent)))
        .id()
}

fn group(app: &mut App, rules: FocusGroup) -> Entity {
    app.world_mut().spawn(rules).id()
}

fn press(app: &mut App, button: GamepadButton) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state,
            value: 1.0,
        });
        app.update();
    }
}

/// One key event, as the winit runner names keys (`Tab`, `ShiftLeft` / `Shift`), then a frame.
fn key(app: &mut App, physical: &str, logical: &str, state: ButtonState) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: physical.into(),
        logical_key: logical.into(),
        state,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
}

fn tab(app: &mut App) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        key(app, "Tab", "Tab", state);
    }
}

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn focus_gained(app: &mut App) -> Vec<Entity> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|e| e.kind == UiEventKind::FocusChanged(true))
        .map(|e| e.target)
        .collect()
}

#[test]
fn a_wrapping_row_jumps_from_its_last_card_to_its_first() {
    let mut app = app();
    let row = group(&mut app, FocusGroup::wrap_horizontal());
    let cards: Vec<Entity> = (0..4)
        .map(|i| child(&mut app, row, [100.0 + 150.0 * i as f32, 100.0, 120.0, 80.0]))
        .collect();
    app.world_mut().entity_mut(cards[0]).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadRight);
    assert_eq!(
        focused(&app),
        Some(cards[0]),
        "the first press reveals the default"
    );
    for &card in &cards[1..] {
        press(&mut app, GamepadButton::DPadRight);
        assert_eq!(focused(&app), Some(card));
    }
    press(&mut app, GamepadButton::DPadRight);
    assert_eq!(focused(&app), Some(cards[0]), "right on the last card wraps");
    press(&mut app, GamepadButton::DPadLeft);
    assert_eq!(focused(&app), Some(cards[3]), "left on the first card wraps back");
}

#[test]
fn a_trap_keeps_the_dpad_and_tab_inside_until_it_is_removed() {
    let mut app = app();
    let outside = node(&mut app, [100.0, 400.0, 200.0, 50.0]);
    let panel = group(&mut app, FocusGroup::trap());
    let a = child(&mut app, panel, [100.0, 100.0, 200.0, 50.0]);
    let b = child(&mut app, panel, [100.0, 170.0, 200.0, 50.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(b));
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(b), "the trap stops at its bottom edge");
    press(&mut app, GamepadButton::RightShoulder);
    assert_eq!(focused(&app), Some(a), "the shoulder cycles inside the trap");
    tab(&mut app);
    assert_eq!(focused(&app), Some(b), "so does Tab");
    // Without the trap the same press leaves the panel.
    app.world_mut().entity_mut(panel).remove::<FocusGroup>();
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(outside));
}

#[test]
fn coming_back_into_a_remembering_list_lands_on_the_row_left_last() {
    let mut app = app();
    let menu = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    let list = group(&mut app, FocusGroup::new().remembering());
    let rows: Vec<Entity> = (0..3)
        .map(|i| child(&mut app, list, [200.0, 60.0 * i as f32, 100.0, 40.0]))
        .collect();
    app.world_mut().entity_mut(menu).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadRight);
    assert_eq!(focused(&app), Some(menu));
    press(&mut app, GamepadButton::DPadRight);
    assert_eq!(focused(&app), Some(rows[0]), "first entry: geometry");
    press(&mut app, GamepadButton::DPadDown);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(rows[2]));
    press(&mut app, GamepadButton::DPadLeft);
    assert_eq!(focused(&app), Some(menu));
    assert_eq!(
        app.world().resource::<FocusMemory>().remembered(list),
        Some(rows[2])
    );
    press(&mut app, GamepadButton::DPadRight);
    assert_eq!(focused(&app), Some(rows[2]), "re-entry restores the last row");
}

#[test]
fn tab_index_orders_the_shoulders_and_a_negative_index_is_skipped() {
    let mut app = app();
    let top = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    let skipped = node(&mut app, [0.0, 60.0, 100.0, 40.0]);
    let first = node(&mut app, [0.0, 120.0, 100.0, 40.0]);
    app.world_mut().entity_mut(first).insert(TabIndex(1));
    app.world_mut().entity_mut(skipped).insert(TabIndex(-1));
    app.world_mut().entity_mut(top).insert(DefaultFocus);
    press(&mut app, GamepadButton::RightShoulder);
    assert_eq!(focused(&app), Some(top));
    press(&mut app, GamepadButton::RightShoulder);
    assert_eq!(
        focused(&app),
        Some(first),
        "order is [first, top]: wraps to the indexed node"
    );
    press(&mut app, GamepadButton::RightShoulder);
    assert_eq!(focused(&app), Some(top));
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(
        focused(&app),
        Some(skipped),
        "the d-pad still reaches a skipped node"
    );
}

/// Focuses the middle of three rows by d-pad (navigation mode) and returns them.
fn three_rows(app: &mut App) -> [Entity; 3] {
    // Uneven gaps so the nearest neighbor of the middle row is the bottom one.
    let a = node(app, [100.0, 100.0, 300.0, 60.0]);
    let b = node(app, [100.0, 180.0, 300.0, 60.0]);
    let c = node(app, [100.0, 250.0, 300.0, 60.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(app, GamepadButton::DPadDown);
    press(app, GamepadButton::DPadDown);
    assert_eq!(focused(app), Some(b));
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Navigation);
    [a, b, c]
}

#[test]
fn a_despawned_focus_moves_to_the_nearest_row_at_once_in_navigation_mode() {
    let mut app = app();
    let [_, b, c] = three_rows(&mut app);
    app.world_mut().despawn(b);
    app.update();
    assert_eq!(focused(&app), Some(c), "the ring never sits on nothing");
    assert!(app.world().get::<Focused>(c).is_some());
    assert_eq!(focus_gained(&mut app), vec![c]);
    press(&mut app, GamepadButton::DPadUp);
    assert_ne!(
        focused(&app),
        Some(c),
        "navigation works again from the new focus"
    );
}

#[test]
fn a_hidden_focus_moves_too_but_only_after_it_was_seen() {
    let mut app = app();
    let [_, b, c] = three_rows(&mut app);
    app.world_mut().get_mut::<ComputedNode>(b).unwrap().rect = [100.0, 180.0, 0.0, 0.0];
    app.update();
    assert_eq!(focused(&app), Some(c));
    assert!(
        app.world().get::<Focused>(b).is_none(),
        "the hidden node lost its marker"
    );

    // A node focused in code before its first layout has no rect yet: it keeps focus.
    let fresh = app
        .world_mut()
        .spawn((Focusable::default(), ComputedNode::default()))
        .id();
    app.world_mut().resource_mut::<FocusState>().focused = Some(fresh);
    app.update();
    app.update();
    assert_eq!(focused(&app), Some(fresh));
}

#[test]
fn in_pointer_mode_a_despawned_focus_is_cleared_not_moved() {
    let mut app = app();
    let [_, b, _] = three_rows(&mut app);
    *app.world_mut().resource_mut::<UiInputMode>() = UiInputMode::Pointer;
    app.world_mut().despawn(b);
    app.update();
    assert_eq!(focused(&app), None);
    assert!(focus_gained(&mut app).is_empty());
    // The next navigation press starts over at the default.
    press(&mut app, GamepadButton::DPadDown);
    assert!(focused(&app).is_some());
}

#[test]
fn focus_left_outside_a_dialog_recovers_at_the_dialog_default() {
    let mut app = app();
    let outside = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    let dialog = app
        .world_mut()
        .spawn((FocusScope, computed([400.0, 300.0, 300.0, 200.0])))
        .id();
    let cancel = child(&mut app, dialog, [420.0, 320.0, 100.0, 40.0]);
    let ok = child(&mut app, dialog, [560.0, 320.0, 100.0, 40.0]);
    app.world_mut().entity_mut(ok).insert(DefaultFocus);
    app.world_mut().resource_mut::<FocusState>().focused = Some(outside);
    press(&mut app, GamepadButton::DPadLeft);
    assert_eq!(focused(&app), Some(ok), "before the graph this press did nothing");
    press(&mut app, GamepadButton::DPadLeft);
    assert_eq!(focused(&app), Some(cancel));
}

#[test]
fn nav_move_drives_the_same_graph_the_pad_does() {
    let mut app = app();
    let row = group(&mut app, FocusGroup::wrap_horizontal());
    let a = child(&mut app, row, [0.0, 0.0, 100.0, 40.0]);
    let b = child(&mut app, row, [120.0, 0.0, 100.0, 40.0]);
    app.update();
    let first = nav_move(app.world_mut(), NavDirection::Right).expect("move");
    assert_eq!(first.from, None);
    assert_eq!(first.step.reason, NavReason::Initial);
    assert_eq!(first.focused, Some(a));
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Navigation);
    assert!(app.world().get::<Focused>(a).is_some());
    let second = nav_move(app.world_mut(), NavDirection::Right).expect("move");
    assert_eq!(
        second.step,
        NavStep {
            target: Some(b),
            reason: NavReason::Geometry
        }
    );
    let third = nav_move(app.world_mut(), NavDirection::Right).expect("move");
    assert_eq!(
        third.step,
        NavStep {
            target: Some(a),
            reason: NavReason::Wrap
        }
    );
    assert!(app.world().get::<Focused>(b).is_none());
    let graph = world_graph(app.world_mut());
    assert_eq!(graph.nodes().len(), 2);
    assert_eq!(graph.groups().len(), 1);

    // `app` is the local App here; the helper is reached by path.
    let mut empty = crate::app();
    empty.update();
    assert_eq!(
        nav_move(empty.world_mut(), NavDirection::Down),
        Err(NavError::NoFocusables)
    );
    assert_eq!(
        nav_move(&mut World::new(), NavDirection::Down),
        Err(NavError::NotInstalled)
    );
}

#[test]
fn tab_goes_forward_and_shift_tab_goes_back() {
    let mut app = app();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    let b = node(&mut app, [0.0, 60.0, 100.0, 40.0]);
    let c = node(&mut app, [0.0, 120.0, 100.0, 40.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    tab(&mut app);
    assert_eq!(focused(&app), Some(a), "the first Tab reveals the default");
    tab(&mut app);
    assert_eq!(
        focused(&app),
        Some(b),
        "winit names the key `Tab` (capital T): that is still forward"
    );
    tab(&mut app);
    assert_eq!(focused(&app), Some(c));
    key(&mut app, "ShiftLeft", "Shift", ButtonState::Pressed);
    tab(&mut app);
    assert_eq!(focused(&app), Some(b), "Shift held: back");
    key(&mut app, "ShiftLeft", "Shift", ButtonState::Released);
    tab(&mut app);
    assert_eq!(focused(&app), Some(c), "Shift released: forward again");
    // `ui.type` sends `Shift+Tab` as a Tab whose physical key is a Shift key.
    key(&mut app, "ShiftLeft", "Tab", ButtonState::Pressed);
    assert_eq!(focused(&app), Some(b));
}
