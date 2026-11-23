// SPDX-License-Identifier: MPL-2.0
//! Host navigation keymaps (ADR-7976, `docs/specs/18-runtime-ui.md` § 4): a host that wants
//! h/j/k/l (or any other keys) to move focus maps them onto [`UiNavActions`] from its own system
//! ordered before `regia_ui::focus::ui_focus_navigation`. The built-in arrows, Tab and Enter keep
//! working beside it, and the host map stands down while a text field is focused so typing an
//! `h` stays text. No engine code changes: this is the recipe book/agents/ui-input.md documents.
//! Headless: code-only UI with layout, no window, no GPU, fixed 16 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::ButtonState;
use regia_ui::focus::{DefaultFocus, FocusState, UiNavActions};
use regia_ui::prelude::*;
use regia_ui::style::{Align, FlexDirection};
use regia_ui::text_input::TextInput;

/// The recipe: vi-style keys onto the UI's navigation actions, before the focus pass reads them.
/// Only key presses count (auto-repeat included, as the arrows do); a focused text field keeps
/// its letters.
fn vi_nav_keys(
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut nav: ResMut<UiNavActions>,
    focus: Res<FocusState>,
    text_inputs: Query<(), With<TextInput>>,
) {
    let typing = focus.focused.is_some_and(|f| text_inputs.contains(f));
    for e in keys.read() {
        if e.state != ButtonState::Pressed || typing {
            continue;
        }
        match e.logical_key.as_str() {
            "h" => nav.left = true,
            "j" => nav.down = true,
            "k" => nav.up = true,
            "l" => nav.right = true,
            _ => {}
        }
    }
}

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
    app.add_systems(Update, vi_nav_keys.before(regia_ui::focus::ui_focus_navigation));
    app
}

fn column(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(40.0),
                gap: [Dimension::Points(40.0), Dimension::Points(40.0)],
                direction: FlexDirection::Column,
                align_items: Align::Start,
                ..Default::default()
            }),
        ))
        .id()
}

fn row(app: &mut App, parent: Entity) -> Entity {
    let world = app.world_mut();
    let e = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                gap: [Dimension::Points(40.0), Dimension::Points(40.0)],
                flex_shrink: 0.0,
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(parent).add_child(e);
    e
}

/// A focusable, clickable 120 x 44 px tile under `parent`.
fn tile(app: &mut App, parent: Entity) -> Entity {
    let world = app.world_mut();
    let e = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(120.0), Dimension::Points(44.0)],
                flex_shrink: 0.0,
                ..Default::default()
            }),
            Focusable::default(),
            Interactable::default(),
        ))
        .id();
    world.entity_mut(parent).add_child(e);
    e
}

fn settle(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

fn key(app: &mut App, logical: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: logical.into(),
            logical_key: logical.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

/// Loadout grid: `a b` on the first row, `c` under `a`.
fn grid(app: &mut App) -> [Entity; 3] {
    let root = column(app);
    let top = row(app, root);
    let a = tile(app, top);
    let b = tile(app, top);
    let c = tile(app, root);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    settle(app);
    [a, b, c]
}

#[test]
fn h_j_k_l_move_focus_like_the_arrows() {
    let mut app = app();
    let [a, b, c] = grid(&mut app);
    key(&mut app, "j");
    assert_eq!(focused(&app), Some(a), "the first move takes the default focus");
    key(&mut app, "l");
    assert_eq!(focused(&app), Some(b), "l moves right");
    key(&mut app, "h");
    assert_eq!(focused(&app), Some(a), "h moves left");
    key(&mut app, "j");
    assert_eq!(focused(&app), Some(c), "j moves down");
    key(&mut app, "k");
    assert_eq!(focused(&app), Some(a), "k moves up");
}

#[test]
fn the_arrows_and_enter_still_work_beside_the_host_map() {
    let mut app = app();
    let [a, b, _] = grid(&mut app);
    key(&mut app, "ArrowDown");
    assert_eq!(focused(&app), Some(a));
    key(&mut app, "ArrowRight");
    assert_eq!(focused(&app), Some(b));
    app.world_mut().resource_mut::<Messages<UiEvent>>().clear();
    key(&mut app, "Enter");
    let clicks = app
        .world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .drain()
        .filter(|e| e.target == b && e.kind == UiEventKind::Click)
        .count();
    assert_eq!(clicks, 1, "Enter clicks the focused tile");
    key(&mut app, "h");
    assert_eq!(focused(&app), Some(a), "and the host keys mix with the arrows");
}

#[test]
fn a_focused_text_field_keeps_its_letters() {
    let mut app = app();
    let root = column(&mut app);
    let field = tile(&mut app, root);
    app.world_mut()
        .entity_mut(field)
        .insert((TextInput::default(), DefaultFocus));
    let below = tile(&mut app, root);
    settle(&mut app);
    key(&mut app, "ArrowDown");
    assert_eq!(focused(&app), Some(field));
    key(&mut app, "j");
    assert_eq!(
        focused(&app),
        Some(field),
        "a `j` typed into a field does not move focus"
    );
    key(&mut app, "Tab");
    assert_eq!(focused(&app), Some(below), "Tab still leaves the field");
}
