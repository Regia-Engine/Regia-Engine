// SPDX-License-Identifier: MPL-2.0
//! Popup layer and menus (ADR-273 lane 5): dropdown lists escape their owner's clip and flip at
//! the screen edge, menus traverse by keyboard and by gamepad, closing gives focus back to the
//! opener, outside presses close, a context menu at the edge shifts inside the safe area, and the
//! agent methods list and activate menu items. Headless: no window, no GPU, real layout, game time
//! advances by a fixed 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged, MouseButton};
use regia_ui::agent_bridge::call;
use regia_ui::focus::{FocusState, UiInputMode};
use regia_ui::menu::{
    ContextMenu, MenuBar, MenuEntry, MenuItem, MenuPopup, MenuRow, MenuSearch, MenuSearchField, PopupMenu,
};
use regia_ui::popup::{POPUP_SORT_ORDER, Popup, PopupKind, PopupPlacement, PopupStack};
use regia_ui::prelude::*;
use serde_json::json;

const STEP_MS: u64 = 100;

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    app.init_resource::<Seen>().add_systems(PostUpdate, record_events);
    app
}

/// Every `UiEvent` of the run, each read once.
#[derive(Resource, Default)]
struct Seen(Vec<UiEvent>);

fn record_events(mut events: MessageReader<UiEvent>, mut seen: ResMut<Seen>) {
    seen.0.extend(events.read().cloned());
}

/// Whether `host` reported the activation of the item at `path` (`ValueChanged(Text(path))`).
fn activated(app: &App, host: Entity, path: &str) -> bool {
    app.world()
        .resource::<Seen>()
        .0
        .iter()
        .any(|e| e.target == host && e.kind == UiEventKind::ValueChanged(UiValue::Text(path.to_owned())))
}

/// A full-window root (1280 x 720 at scale 1) with absolutely placed children.
fn root(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style::default()),
        ))
        .id()
}

/// An absolutely positioned box (UI px) under `parent`.
fn placed(left: f32, top: f32, w: f32, h: f32) -> NodeStyle {
    NodeStyle::new(Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(left),
            top: Dimension::Points(top),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size: [Dimension::Points(w), Dimension::Points(h)],
        ..Default::default()
    })
}

fn settle(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    app.world().get::<ComputedNode>(e).expect("laid out").rect
}

fn center(r: [f32; 4]) -> (f64, f64) {
    (f64::from(r[0] + r[2] / 2.0), f64::from(r[1] + r[3] / 2.0))
}

fn move_to(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
}

fn mouse(app: &mut App, button: MouseButton, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: Default::default(),
    });
    app.update();
}

fn click_with(app: &mut App, at: (f64, f64), button: MouseButton) {
    move_to(app, at.0, at.1);
    mouse(app, button, ButtonState::Pressed);
    mouse(app, button, ButtonState::Released);
}

fn click(app: &mut App, at: (f64, f64)) {
    click_with(app, at, MouseButton::Left);
}

/// Clicks the centre of a laid-out node.
fn click_node(app: &mut App, e: Entity) {
    let at = center(rect(app, e));
    click(app, at);
}

/// Moves the pointer to the centre of a laid-out node.
fn hover_node(app: &mut App, e: Entity) {
    let (x, y) = center(rect(app, e));
    move_to(app, x, y);
}

/// One key press and release in one frame.
fn key(app: &mut App, name: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.to_owned(),
            logical_key: name.to_owned(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
    }
    app.update();
}

/// One gamepad button press, then its release: two frames.
fn pad(app: &mut App, button: GamepadButton) {
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

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn focus(app: &mut App, e: Entity) {
    app.world_mut().entity_mut(e).insert(regia_ui::node::Focused);
    app.world_mut().resource_mut::<FocusState>().focused = Some(e);
}

fn stack(app: &App) -> Vec<Entity> {
    app.world()
        .get_resource::<PopupStack>()
        .map(|s| s.popups().to_vec())
        .unwrap_or_default()
}

fn panel_of(app: &App, popup: Entity) -> Entity {
    app.world().get::<Popup>(popup).expect("a popup").panel
}

/// The `(popup, index)` of the focused menu row.
fn focused_row(app: &App) -> Option<MenuRow> {
    focused(app).and_then(|f| app.world().get::<MenuRow>(f).copied())
}

fn fully_visible(app: &App, e: Entity) -> bool {
    let node = app.world().get::<ComputedNode>(e).expect("laid out");
    let (r, c) = (node.rect, node.clip_rect);
    r[2] > 0.0
        && r[3] > 0.0
        && c[0] <= r[0]
        && c[1] <= r[1]
        && c[0] + c[2] >= r[0] + r[2]
        && c[1] + c[3] >= r[1] + r[3]
}

fn dropdown(app: &mut App, parent: Entity, style: NodeStyle, options: &[&str]) -> Entity {
    let dd = app
        .world_mut()
        .spawn((
            UiNode,
            style,
            Interactable::default(),
            Focusable::default(),
            Dropdown {
                options: options.iter().map(|s| (*s).to_owned()).collect(),
                selected: Some(0),
                open: false,
            },
        ))
        .id();
    app.world_mut().entity_mut(parent).add_child(dd);
    dd
}

fn file_menu() -> Vec<MenuEntry> {
    vec![
        MenuItem::new("new", "New").with_shortcut("").into(),
        MenuItem::new("recent", "Recent")
            .with_submenu(vec![
                MenuItem::new("a", "a.txt").into(),
                MenuItem::new("b", "b.txt").into(),
            ])
            .into(),
        MenuEntry::Separator,
        MenuItem::new("grid", "Show grid").checkable(false).into(),
        MenuItem::new("quit", "Quit").disabled().into(),
    ]
}

/// A focusable button hosting a [`PopupMenu`] at (100, 100).
fn menu_host(app: &mut App) -> Entity {
    let root = root(app);
    let host = app
        .world_mut()
        .spawn((
            UiNode,
            placed(100.0, 100.0, 120.0, 32.0),
            Interactable::default(),
            Focusable::default(),
            PopupMenu {
                entries: file_menu(),
                ..Default::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(host);
    settle(app, 2);
    host
}

#[test]
fn a_dropdown_inside_a_clipped_scroll_view_opens_fully_visible() {
    let mut app = app();
    let root = root(&mut app);
    let scroll = app
        .world_mut()
        .spawn((
            UiNode,
            placed(40.0, 40.0, 300.0, 60.0),
            ClipChildren,
            ScrollView::default(),
            ScrollOffset::default(),
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(scroll);
    let options = ["One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight"];
    let dd = dropdown(&mut app, scroll, placed(10.0, 10.0, 200.0, 32.0), &options);
    settle(&mut app, 2);
    let scroll_rect = rect(&app, scroll);
    click_node(&mut app, dd);
    settle(&mut app, 3);
    assert!(app.world().get::<Dropdown>(dd).unwrap().open);
    let popups = stack(&app);
    assert_eq!(popups.len(), 1, "one popup open");
    let popup = popups[0];
    assert_eq!(app.world().get::<Popup>(popup).unwrap().kind, PopupKind::Dropdown);
    let sort = app.world().get::<UiRoot>(popup).unwrap().sort_order;
    assert!(
        sort >= POPUP_SORT_ORDER,
        "popup root above every app root ({sort})"
    );
    let panel = panel_of(&app, popup);
    let panel_rect = rect(&app, panel);
    assert!(
        panel_rect[1] + panel_rect[3] > scroll_rect[1] + scroll_rect[3] + 100.0,
        "the list reaches far past the scroll view's clip: {panel_rect:?} vs {scroll_rect:?}"
    );
    assert!(fully_visible(&app, panel), "the panel is not clipped");
    let rows: Vec<Entity> = app
        .world()
        .get::<Children>(panel)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    assert_eq!(rows.len(), options.len());
    for row in rows {
        assert!(fully_visible(&app, row), "every option row is fully visible");
    }
    assert!(
        panel_rect[2] >= 200.0 - 0.5,
        "the list is at least as wide as the dropdown"
    );
    let placed = app.world().get::<Popup>(popup).unwrap().placed.expect("placed");
    assert_eq!(placed.placement, PopupPlacement::Below);
    assert!(app.world().get::<Opacity>(panel).is_none(), "shown once placed");
}

#[test]
fn a_dropdown_near_the_bottom_edge_flips_above() {
    let mut app = app();
    let root = root(&mut app);
    let options = ["One", "Two", "Three", "Four", "Five", "Six"];
    let dd = dropdown(&mut app, root, placed(100.0, 676.0, 200.0, 32.0), &options);
    settle(&mut app, 2);
    let dd_rect = rect(&app, dd);
    click(&mut app, center(dd_rect));
    settle(&mut app, 3);
    let popup = stack(&app)[0];
    let placed = app.world().get::<Popup>(popup).unwrap().placed.expect("placed");
    assert_eq!(placed.placement, PopupPlacement::Above);
    assert!(placed.flipped);
    let panel_rect = rect(&app, panel_of(&app, popup));
    assert!(
        panel_rect[1] + panel_rect[3] <= dd_rect[1] + 0.5,
        "above the dropdown: {panel_rect:?} vs {dd_rect:?}"
    );
    assert!(panel_rect[1] >= -0.5, "on screen");
}

#[test]
fn an_outside_click_closes_and_focus_returns_to_the_opener() {
    let mut app = app();
    let root = root(&mut app);
    let dd = dropdown(
        &mut app,
        root,
        placed(100.0, 100.0, 200.0, 32.0),
        &["One", "Two", "Three"],
    );
    settle(&mut app, 2);
    click_node(&mut app, dd);
    settle(&mut app, 3);
    assert_eq!(stack(&app).len(), 1);
    let inside = focused(&app).expect("focus moved into the list");
    assert_ne!(inside, dd);
    // Press and release far away from the list.
    click(&mut app, (1000.0, 600.0));
    settle(&mut app, 1);
    assert!(stack(&app).is_empty(), "outside press closed the list");
    assert!(!app.world().get::<Dropdown>(dd).unwrap().open);
    assert_eq!(focused(&app), Some(dd), "focus went back to the dropdown");
    assert_eq!(
        app.world().get::<Dropdown>(dd).unwrap().selected,
        Some(0),
        "no selection change"
    );

    // Picking an option closes the list and also returns focus.
    click_node(&mut app, dd);
    settle(&mut app, 3);
    let popup = stack(&app)[0];
    let rows: Vec<Entity> = app
        .world()
        .get::<Children>(panel_of(&app, popup))
        .map(|c| c.iter().collect())
        .unwrap();
    click_node(&mut app, rows[2]);
    settle(&mut app, 1);
    assert!(stack(&app).is_empty());
    assert_eq!(app.world().get::<Dropdown>(dd).unwrap().selected, Some(2));
    assert_eq!(focused(&app), Some(dd));
}

#[test]
fn escape_closes_a_dropdown_list_and_type_ahead_selects() {
    let mut app = app();
    let root = root(&mut app);
    let dd = dropdown(
        &mut app,
        root,
        placed(100.0, 100.0, 200.0, 32.0),
        &["Apple", "Banana", "Cherry", "Cranberry"],
    );
    settle(&mut app, 2);
    focus(&mut app, dd);
    // Closed: typing selects.
    key(&mut app, "c");
    assert_eq!(app.world().get::<Dropdown>(dd).unwrap().selected, Some(2));
    key(&mut app, "c");
    assert_eq!(
        app.world().get::<Dropdown>(dd).unwrap().selected,
        Some(3),
        "the same letter cycles"
    );
    // Enter opens, Escape closes and focus is back on the dropdown.
    key(&mut app, "Enter");
    settle(&mut app, 3);
    assert_eq!(stack(&app).len(), 1);
    key(&mut app, "Escape");
    settle(&mut app, 1);
    assert!(stack(&app).is_empty());
    assert_eq!(focused(&app), Some(dd));
}

#[test]
fn submenu_traversal_by_keyboard() {
    let mut app = app();
    let host = menu_host(&mut app);
    focus(&mut app, host);
    key(&mut app, "Enter");
    settle(&mut app, 3);
    let menu = *stack(&app).first().expect("menu open");
    assert_eq!(app.world().get::<MenuPopup>(menu).unwrap().host, host);
    assert_eq!(
        focused_row(&app),
        Some(MenuRow {
            popup: menu,
            index: 0
        }),
        "the first item has focus"
    );
    key(&mut app, "ArrowDown");
    assert_eq!(focused_row(&app).map(|r| r.index), Some(1), "down to Recent");
    key(&mut app, "ArrowRight");
    settle(&mut app, 3);
    let popups = stack(&app);
    assert_eq!(popups.len(), 2, "the submenu opened");
    let sub = popups[1];
    assert_eq!(app.world().get::<Popup>(sub).unwrap().parent, Some(menu));
    assert_eq!(focused_row(&app), Some(MenuRow { popup: sub, index: 0 }));
    let placed = app.world().get::<Popup>(sub).unwrap().placed.expect("placed");
    assert_eq!(placed.placement, PopupPlacement::Right);
    key(&mut app, "ArrowLeft");
    settle(&mut app, 1);
    assert_eq!(stack(&app), vec![menu], "Left closed the submenu");
    assert_eq!(
        focused_row(&app),
        Some(MenuRow {
            popup: menu,
            index: 1
        }),
        "focus back on Recent"
    );
    key(&mut app, "Escape");
    settle(&mut app, 1);
    assert!(stack(&app).is_empty());
    assert_eq!(focused(&app), Some(host), "focus back on the opener");
}

#[test]
fn submenu_traversal_by_gamepad() {
    let mut app = app();
    let host = menu_host(&mut app);
    focus(&mut app, host);
    pad(&mut app, GamepadButton::South);
    settle(&mut app, 3);
    let menu = *stack(&app).first().expect("A opened the menu");
    assert_eq!(focused_row(&app).map(|r| (r.popup, r.index)), Some((menu, 0)));
    pad(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused_row(&app).map(|r| r.index), Some(1));
    // D-pad right opens the submenu.
    pad(&mut app, GamepadButton::DPadRight);
    settle(&mut app, 3);
    let popups = stack(&app);
    assert_eq!(popups.len(), 2);
    assert_eq!(
        focused_row(&app).map(|r| (r.popup, r.index)),
        Some((popups[1], 0))
    );
    // B closes the submenu only.
    pad(&mut app, GamepadButton::East);
    settle(&mut app, 1);
    assert_eq!(stack(&app), vec![menu]);
    assert_eq!(focused_row(&app).map(|r| (r.popup, r.index)), Some((menu, 1)));
    // A on the submenu item opens it too; A on a leaf activates it and closes everything.
    pad(&mut app, GamepadButton::South);
    settle(&mut app, 3);
    assert_eq!(stack(&app).len(), 2, "A opened the submenu");
    pad(&mut app, GamepadButton::DPadDown);
    pad(&mut app, GamepadButton::South);
    settle(&mut app, 1);
    assert!(stack(&app).is_empty(), "activating b.txt closed the menus");
    assert_eq!(focused(&app), Some(host));
    assert!(activated(&app, host, "recent/b"), "b.txt was activated");
}

#[test]
fn a_context_menu_at_the_right_edge_shifts_inside_the_safe_area() {
    let mut app = app();
    app.insert_resource(regia_platform::SafeArea {
        left: 0,
        top: 0,
        right: 40,
        bottom: 0,
    });
    let root = root(&mut app);
    let area = app
        .world_mut()
        .spawn((
            UiNode,
            placed(0.0, 0.0, 1280.0, 720.0),
            Interactable::default(),
            ContextMenu {
                entries: file_menu(),
                ..Default::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(area);
    settle(&mut app, 2);
    click_with(&mut app, (1270.0, 200.0), MouseButton::Right);
    settle(&mut app, 3);
    let popup = *stack(&app).first().expect("context menu open");
    let state = app.world().get::<Popup>(popup).unwrap();
    assert_eq!(state.point, Some([1270.0, 200.0]));
    let placed = state.placed.expect("placed");
    assert!(placed.shifted);
    let panel_rect = rect(&app, panel_of(&app, popup));
    assert!(
        panel_rect[0] + panel_rect[2] <= 1280.0 - 40.0 + 0.5,
        "inside the safe area: {panel_rect:?}"
    );
    assert!(panel_rect[0] >= 0.0);
    assert!((panel_rect[1] - 200.0).abs() < 0.5, "top stays at the pointer");
    // Left click elsewhere closes it.
    click(&mut app, (20.0, 700.0));
    settle(&mut app, 1);
    assert!(stack(&app).is_empty());
}

#[test]
fn hovering_opens_submenus_and_a_menu_bar_switches_menus() {
    let mut app = app();
    let root = root(&mut app);
    let bar = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                direction: regia_ui::style::FlexDirection::Row,
                ..Default::default()
            }),
            MenuBar {
                menus: vec![
                    MenuItem::new("file", "File").with_submenu(file_menu()),
                    MenuItem::new("view", "View").with_submenu(vec![MenuItem::new("zoom", "Zoom").into()]),
                ],
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(bar);
    settle(&mut app, 3);
    let titles: Vec<Entity> = app
        .world()
        .get::<Children>(bar)
        .map(|c| c.iter().collect())
        .unwrap();
    assert_eq!(titles.len(), 2, "one title per menu");
    click_node(&mut app, titles[0]);
    settle(&mut app, 3);
    let file = stack(&app)[0];
    // Hover Recent: its submenu opens.
    let recent = app
        .world_mut()
        .query::<(Entity, &MenuRow)>()
        .iter(app.world())
        .find(|(_, r)| r.popup == file && r.index == 1)
        .map(|(e, _)| e)
        .unwrap();
    hover_node(&mut app, recent);
    settle(&mut app, 2);
    assert_eq!(stack(&app).len(), 2, "hover opened the submenu");
    // Hover the View title: the bar switches menus.
    hover_node(&mut app, titles[1]);
    settle(&mut app, 3);
    let open = stack(&app);
    assert_eq!(open.len(), 1);
    assert_eq!(
        app.world().get::<MenuPopup>(open[0]).map(|m| m.source),
        Some(regia_ui::menu::MenuSource::Bar(1))
    );
}

#[test]
fn a_long_menu_has_a_search_field_that_filters() {
    let mut app = app();
    let root = root(&mut app);
    let names = [
        "Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel", "India", "Juliett", "Kilo",
        "Lima", "Mike", "November",
    ];
    let host = app
        .world_mut()
        .spawn((
            UiNode,
            placed(100.0, 40.0, 120.0, 32.0),
            Interactable::default(),
            Focusable::default(),
            PopupMenu {
                entries: names
                    .iter()
                    .map(|n| MenuItem::new(n.to_lowercase(), *n).into())
                    .collect(),
                search: MenuSearch::Auto,
                ..Default::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(host);
    settle(&mut app, 2);
    click_node(&mut app, host);
    settle(&mut app, 3);
    let field = app
        .world_mut()
        .query::<(Entity, &MenuSearchField)>()
        .iter(app.world())
        .map(|(e, _)| e)
        .next()
        .expect("a search field");
    assert_eq!(focused(&app), Some(field), "the search field has focus");
    // Type "ov" and press Enter through the agent bridge (the real keyboard and IME path).
    call(
        app.world_mut(),
        "ui.type",
        &json!({ "node": field.to_bits(), "text": "ov", "keys": ["Enter"] }),
    )
    .expect("queued");
    settle(&mut app, 2);
    assert!(
        activated(&app, host, "november"),
        "Enter activated the first match"
    );
    assert!(stack(&app).is_empty());
}

#[test]
fn a_tooltip_shows_on_focus_and_on_hover_on_the_popup_layer() {
    let mut app = app();
    let root = root(&mut app);
    let clip = app
        .world_mut()
        .spawn((UiNode, placed(100.0, 100.0, 200.0, 40.0), ClipChildren))
        .id();
    app.world_mut().entity_mut(root).add_child(clip);
    let host = app
        .world_mut()
        .spawn((
            UiNode,
            placed(0.0, 0.0, 120.0, 32.0),
            Interactable::default(),
            Focusable::default(),
            {
                let mut tip = Tooltip::default();
                tip.text = "Saves the file".into();
                tip
            },
        ))
        .id();
    app.world_mut().entity_mut(clip).add_child(host);
    settle(&mut app, 2);
    focus(&mut app, host);
    *app.world_mut().resource_mut::<UiInputMode>() = UiInputMode::Navigation;
    settle(&mut app, 4);
    assert!(stack(&app).is_empty(), "not before the focus delay");
    settle(&mut app, 8);
    let popup = *stack(&app).first().expect("tooltip shown on focus");
    assert_eq!(app.world().get::<Popup>(popup).unwrap().kind, PopupKind::Tooltip);
    assert!(!app.world().get::<Popup>(popup).unwrap().interactive);
    assert_eq!(focused(&app), Some(host), "a tooltip takes no focus");
    let panel = panel_of(&app, popup);
    assert!(fully_visible(&app, panel), "outside the owner's clip");
    assert!(rect(&app, panel)[1] >= rect(&app, host)[1] + rect(&app, host)[3]);
    // Focus leaves: it hides.
    app.world_mut()
        .entity_mut(host)
        .remove::<regia_ui::node::Focused>();
    app.world_mut().resource_mut::<FocusState>().focused = None;
    settle(&mut app, 1);
    assert!(stack(&app).is_empty());
    // Hover: after the pointer delay.
    hover_node(&mut app, host);
    settle(&mut app, 6);
    assert_eq!(stack(&app).len(), 1, "tooltip shown on hover");
}

#[test]
fn agent_lists_a_menu_and_activation_toggles_a_checkable_item() {
    let mut app = app();
    let host = menu_host(&mut app);
    let world = app.world_mut();
    let menu = call(world, "ui.menu", &json!({ "node": host.to_bits() })).expect("ui.menu");
    assert_eq!(menu["kind"], "popup_menu");
    assert_eq!(menu["open"], false);
    let items = menu["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[0]["id"], "new");
    assert_eq!(items[1]["submenu"][1]["path"], json!(["recent", "b"]));
    assert_eq!(items[2]["separator"], true);
    assert_eq!(items[3]["checkable"], true);
    assert_eq!(items[3]["checked"], false);
    assert_eq!(items[4]["enabled"], false);

    let out = call(
        world,
        "ui.menu_activate",
        &json!({ "node": host.to_bits(), "path": ["grid"] }),
    )
    .expect("activate");
    assert_eq!(out["checked"], true);
    let changed: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    assert!(
        changed.iter().any(
            |e| e.target == host && e.kind == UiEventKind::ValueChanged(UiValue::Text("grid".to_owned()))
        )
    );
    let menu = call(world, "ui.menu", &json!({ "node": host.to_bits() })).expect("ui.menu");
    assert_eq!(menu["items"][3]["checked"], true, "the item is checked now");
    let again = call(
        world,
        "ui.menu_activate",
        &json!({ "node": host.to_bits(), "path": "Show grid" }),
    )
    .expect("by label");
    assert_eq!(again["checked"], false, "a second activation unchecks");

    for (path, reason) in [
        (json!(["recent"]), "ui_menu_item_has_submenu"),
        (json!(["quit"]), "ui_menu_item_disabled"),
        (json!(["nope"]), "ui_menu_path_not_found"),
        (json!(5), "ui_bad_param"),
    ] {
        let err = call(
            world,
            "ui.menu_activate",
            &json!({ "node": host.to_bits(), "path": path }),
        )
        .expect_err("refused");
        assert_eq!(err.reason, reason);
    }
    let plain = world.spawn(UiNode).id();
    let err = call(world, "ui.menu", &json!({ "node": plain.to_bits() })).expect_err("no menu");
    assert_eq!(err.reason, "ui_menu_not_found");
    let err = call(world, "ui.close_popup", &json!({})).expect_err("nothing open");
    assert_eq!(err.reason, "ui_no_popup_open");
    let err = call(world, "ui.open_popup", &json!({ "anchor": plain.to_bits() })).expect_err("nothing");
    assert_eq!(err.reason, "ui_popup_unavailable");

    // Open through the agent, then close.
    let opened = call(world, "ui.open_popup", &json!({ "anchor": host.to_bits() })).expect("open");
    assert_eq!(opened["kind"], "menu");
    assert_eq!(opened["already_open"], false);
    settle(&mut app, 3);
    let world = app.world_mut();
    let again = call(world, "ui.open_popup", &json!({ "anchor": host.to_bits() })).expect("open");
    assert_eq!(again["already_open"], true);
    assert_eq!(again["placed"], "below");
    let err = call(world, "ui.close_popup", &json!({ "popup": 12345 })).expect_err("unknown");
    assert_eq!(err.reason, "ui_popup_not_found");
    let closed = call(world, "ui.close_popup", &json!({})).expect("close");
    assert_eq!(closed["closed"].as_array().map(Vec::len), Some(1));
    assert_eq!(closed["stack"], json!([]));
}

#[test]
fn closing_a_menu_despawns_with_its_anchor() {
    let mut app = app();
    let host = menu_host(&mut app);
    call(
        app.world_mut(),
        "ui.open_popup",
        &json!({ "anchor": host.to_bits() }),
    )
    .expect("open");
    settle(&mut app, 2);
    assert_eq!(stack(&app).len(), 1);
    app.world_mut().entity_mut(host).despawn();
    settle(&mut app, 1);
    assert!(stack(&app).is_empty(), "anchor despawn closes the popup");
}

#[test]
fn a_popup_carries_its_owner_theme_scope() {
    let mut app = app();
    let root = root(&mut app);
    app.world_mut()
        .entity_mut(root)
        .insert(regia_ui::ThemeScope::new("regia-light"));
    let dd = dropdown(&mut app, root, placed(100.0, 100.0, 200.0, 32.0), &["One", "Two"]);
    settle(&mut app, 2);
    click_node(&mut app, dd);
    settle(&mut app, 2);
    let popup = *stack(&app).first().expect("list open");
    assert_eq!(
        app.world().get::<regia_ui::ThemeScope>(popup),
        Some(&regia_ui::ThemeScope::new("regia-light")),
        "the list resolves against the dropdown's theme"
    );
}

/// ADR-374 / ADR-5665 (roadmap lanes 5 and 7, "widgets can show chords"): a menu item with an empty
/// shortcut slot shows the chord the shortcut registry binds to its key; an item's own text wins;
/// `ui.menu` reports what the row shows.
#[test]
fn menu_rows_show_the_registry_chord_unless_the_item_sets_its_own() {
    use regia_ui::menu::MenuShortcutSlot;
    use regia_ui::{Chord, UiShortcut, UiShortcuts};
    let mut app = app();
    {
        let mut shortcuts = app.world_mut().resource_mut::<UiShortcuts>();
        for (chord, action) in [("Ctrl+N", "new"), ("Ctrl+R", "reload")] {
            shortcuts
                .register(UiShortcut::new(Chord::parse(chord).expect("chord"), action))
                .expect("register");
        }
    }
    let ctrl_n = Chord::parse("Ctrl+N").expect("chord").platform_display();
    let root = root(&mut app);
    let host = app
        .world_mut()
        .spawn((
            UiNode,
            placed(100.0, 100.0, 120.0, 32.0),
            Interactable::default(),
            Focusable::default(),
            PopupMenu {
                entries: vec![
                    MenuItem::new("new", "New").into(),
                    MenuItem::new("reload", "Reload").with_shortcut("F5").into(),
                    MenuItem::new("close", "Close").into(),
                ],
                ..Default::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(host);
    settle(&mut app, 2);

    let menu = call(app.world_mut(), "ui.menu", &json!({ "node": host.to_bits() })).expect("ui.menu");
    assert_eq!(menu["items"][0]["shortcut"], ctrl_n.as_str(), "{menu}");
    assert_eq!(menu["items"][1]["shortcut"], "F5", "the item's own text wins");
    assert_eq!(menu["items"][2]["shortcut"], "", "nothing bound, nothing shown");

    call(
        app.world_mut(),
        "ui.open_popup",
        &json!({ "anchor": host.to_bits() }),
    )
    .expect("open");
    settle(&mut app, 3);
    let mut slots = app
        .world_mut()
        .query_filtered::<(&ChildOf, &Text), With<MenuShortcutSlot>>();
    let mut rows = app.world_mut().query::<&MenuRow>();
    let mut shown: Vec<(usize, String)> = slots
        .iter(app.world())
        .filter_map(|(parent, text)| {
            let row = rows.get(app.world(), parent.parent()).ok()?;
            Some((row.index, format!("{:?}", text.spans)))
        })
        .collect();
    shown.sort_by_key(|(i, _)| *i);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(shown[0].1.contains(&ctrl_n), "{shown:?}");
    assert!(shown[1].1.contains("F5"), "{shown:?}");
}
