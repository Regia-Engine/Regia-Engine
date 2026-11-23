// SPDX-License-Identifier: MPL-2.0
//! Virtualised list v2 and interactive scrollbars (ADR-273 lane 8, ADR-344). Headless: the list
//! entities carry hand-written `ComputedNode` rects (no taffy layout), game time advances by a fixed
//! 100 ms per update, nothing waits on a clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::ButtonState;
use regia_ui::focus::FocusState;
use regia_ui::list::{SelectOp, list_info, list_items, request_scroll_to_item, select_items};
use regia_ui::prelude::*;
use regia_ui::{
    ItemCount, ListHeaders, ListHeights, ListItem, ListRow, ListSelection, ListState, ListTiles, ScrollAlign,
    ScrollAxis, ScrollbarNodes, Scrollbars, SelectionMode, UiModifiers, UiPointer,
};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
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

/// A list of `count` generic rows, `row_h` tall, in a `rect` viewport.
fn list(app: &mut App, count: usize, row_h: f32, rect: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((
            ListView::new(row_h, 2),
            ItemCount(count),
            ScrollOffset::default(),
            computed(rect),
        ))
        .id()
}

fn step(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn set_offset(app: &mut App, list: Entity, y: f32) {
    app.world_mut().get_mut::<ScrollOffset>(list).unwrap().0[1] = y;
}

/// The row entity currently showing `index`.
fn row_for(app: &mut App, index: usize) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &ListRow)>();
    q.iter(app.world())
        .find(|(_, r)| r.index == index)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no live row for item {index}"))
}

fn row_rect_top(app: &mut App, index: usize) -> f32 {
    let row = row_for(app, index);
    match app.world().get::<NodeStyle>(row).unwrap().0.inset.top {
        Dimension::Points(p) => p,
        other => panic!("row top is {other:?}"),
    }
}

fn row_inset_left(app: &mut App, index: usize) -> f32 {
    let row = row_for(app, index);
    match app.world().get::<NodeStyle>(row).unwrap().0.inset.left {
        Dimension::Points(p) => p,
        other => panic!("row left is {other:?}"),
    }
}

fn click(app: &mut App, row: Entity, modifiers: UiModifiers) {
    app.world_mut()
        .write_message(UiEvent::new(row, UiEventKind::Click).with_modifiers(modifiers));
    app.update();
}

fn key(app: &mut App, logical: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: logical.to_owned(),
            logical_key: logical.to_owned(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
    }
    app.update();
}

fn focus_row(app: &mut App, index: usize) {
    let row = row_for(app, index);
    app.world_mut().resource_mut::<FocusState>().focused = Some(row);
}

fn selected(app: &App, list: Entity) -> Vec<usize> {
    app.world().get::<ListSelection>(list).unwrap().to_vec()
}

#[test]
fn ten_thousand_rows_keep_a_bounded_pool() {
    let mut app = app();
    let l = list(&mut app, 10_000, 20.0, [0.0, 0.0, 200.0, 400.0]);
    step(&mut app, 3);
    let info = list_info(app.world(), l).unwrap();
    assert_eq!(info.count, 10_000);
    // 400 / 20 = 20 rows plus the boundary row and two rows of overscan below the top.
    assert_eq!(info.range, 0..23);
    assert_eq!(info.live_rows, 23);
    assert_eq!(info.total_height, 200_000.0);
    let mut rows = app.world_mut().query::<&ListRow>();
    assert_eq!(rows.iter(app.world()).count(), 23);
    // Deep in the list the pool does not grow past a few rows more.
    set_offset(&mut app, l, 100_000.0);
    step(&mut app, 2);
    let info = list_info(app.world(), l).unwrap();
    assert!(info.live_rows <= 26, "{} rows", info.live_rows);
    assert!(info.range.contains(&5000));
}

#[test]
fn scrolling_one_row_rebinds_one_entry() {
    let mut app = app();
    let l = list(&mut app, 10_000, 20.0, [0.0, 0.0, 200.0, 400.0]);
    step(&mut app, 2);
    // Settle the pool size first: the pool only grows.
    set_offset(&mut app, l, 200.0);
    step(&mut app, 2);
    let before = app.world().get::<ListState>(l).unwrap().rebinds();
    let spawned = app.world().get::<ListState>(l).unwrap().spawned_rows();
    set_offset(&mut app, l, 220.0);
    step(&mut app, 1);
    let state = app.world().get::<ListState>(l).unwrap();
    assert_eq!(state.last_rebinds(), 1);
    assert_eq!(state.rebinds(), before + 1);
    assert_eq!(
        state.spawned_rows(),
        spawned,
        "no entity was spawned by a one-row scroll"
    );
}

#[test]
fn rows_show_their_labels_and_positions() {
    let mut app = app();
    let items = ListItems(
        (0..50)
            .map(|i| ListItem {
                label: format!("Row {i}"),
                subtitle: String::new(),
            })
            .collect(),
    );
    let l = app
        .world_mut()
        .spawn((
            ListView::new(20.0, 2),
            items,
            ScrollOffset::default(),
            computed([0.0, 0.0, 200.0, 100.0]),
        ))
        .id();
    step(&mut app, 2);
    let row = row_for(&mut app, 3);
    let text = app.world().get::<Text>(row).unwrap();
    assert_eq!(text.spans.len(), 1);
    assert_eq!(row_rect_top(&mut app, 3), 60.0);
    let listed = list_items(app.world(), l, 2, 3).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|(i, s, _)| (*i, s.as_str()))
            .collect::<Vec<_>>(),
        [(2, "Row 2"), (3, "Row 3"), (4, "Row 4")]
    );
}

#[test]
fn variable_heights_use_prefix_sums() {
    let mut app = app();
    let l = list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 100.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListHeights(vec![10.0, 30.0, 20.0]));
    step(&mut app, 2);
    assert_eq!(row_rect_top(&mut app, 1), 10.0);
    assert_eq!(row_rect_top(&mut app, 2), 40.0);
    // Items past the table use the list's default height.
    assert_eq!(row_rect_top(&mut app, 4), 80.0);
    let info = list_info(app.world(), l).unwrap();
    assert_eq!(info.layout, "variable_rows");
    assert_eq!(info.total_height, 10.0 + 30.0 + 20.0 + 97.0 * 20.0);
    // Changing the table rebuilds the cache.
    app.world_mut().get_mut::<ListHeights>(l).unwrap().0[0] = 50.0;
    step(&mut app, 1);
    assert_eq!(row_rect_top(&mut app, 1), 50.0);
}

#[test]
fn tiles_wrap_into_columns() {
    let mut app = app();
    let l = list(&mut app, 30, 20.0, [0.0, 0.0, 300.0, 120.0]);
    app.world_mut().entity_mut(l).insert(ListTiles {
        tile_size: [100.0, 50.0],
        gap: [0.0, 0.0],
        columns: None,
    });
    step(&mut app, 2);
    assert_eq!(row_inset_left(&mut app, 4), 100.0);
    assert_eq!(row_rect_top(&mut app, 4), 50.0);
    assert_eq!(row_inset_left(&mut app, 3), 0.0);
    assert_eq!(row_rect_top(&mut app, 3), 50.0);
    let info = list_info(app.world(), l).unwrap();
    assert_eq!(info.layout, "tiles");
    assert_eq!(info.total_height, 500.0);
}

#[test]
fn clicks_select_with_shift_and_ctrl() {
    let mut app = app();
    let l = list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 200.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Multi));
    step(&mut app, 2);
    let none = UiModifiers::default();
    let row = row_for(&mut app, 2);
    click(&mut app, row, none);
    assert_eq!(selected(&app, l), [2]);
    let row = row_for(&mut app, 5);
    click(&mut app, row, UiModifiers { shift: true, ..none });
    assert_eq!(selected(&app, l), [2, 3, 4, 5]);
    let row = row_for(&mut app, 8);
    click(&mut app, row, UiModifiers { ctrl: true, ..none });
    assert_eq!(selected(&app, l), [2, 3, 4, 5, 8]);
    let row = row_for(&mut app, 3);
    click(&mut app, row, UiModifiers { ctrl: true, ..none });
    assert_eq!(selected(&app, l), [2, 4, 5, 8]);
    // The theme marker follows the selection after the next virtualise pass.
    step(&mut app, 1);
    let marked = |app: &mut App, i: usize| {
        let row = row_for(app, i);
        app.world().get::<Selected>(row).is_some()
    };
    assert!(marked(&mut app, 4));
    assert!(!marked(&mut app, 3));
}

#[test]
fn single_selection_replaces() {
    let mut app = app();
    let l = list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 200.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Single));
    step(&mut app, 2);
    let row = row_for(&mut app, 1);
    click(&mut app, row, UiModifiers::default());
    let row = row_for(&mut app, 4);
    click(
        &mut app,
        row,
        UiModifiers {
            shift: true,
            ..Default::default()
        },
    );
    assert_eq!(selected(&app, l), [4]);
}

#[test]
fn selection_survives_scrolling_away_and_back() {
    let mut app = app();
    let l = list(&mut app, 1000, 20.0, [0.0, 0.0, 200.0, 200.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Multi));
    step(&mut app, 2);
    let row = row_for(&mut app, 3);
    click(&mut app, row, UiModifiers::default());
    set_offset(&mut app, l, 10_000.0);
    step(&mut app, 2);
    assert_eq!(selected(&app, l), [3]);
    set_offset(&mut app, l, 0.0);
    step(&mut app, 2);
    let row = row_for(&mut app, 3);
    assert!(app.world().get::<Selected>(row).is_some());
}

#[test]
fn keyboard_home_end_and_paging_move_the_cursor() {
    let mut app = app();
    let l = list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 200.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Single));
    step(&mut app, 2);
    focus_row(&mut app, 0);
    step(&mut app, 1);
    key(&mut app, "End");
    assert_eq!(selected(&app, l), [99]);
    step(&mut app, 2);
    // The list scrolled so the last item is in view and the cursor row has focus.
    let info = list_info(app.world(), l).unwrap();
    assert!(info.range.contains(&99), "range {:?}", info.range);
    assert_eq!(info.cursor, Some(99));
    let focused = app.world().resource::<FocusState>().focused.unwrap();
    assert_eq!(app.world().get::<ListRow>(focused).unwrap().index, 99);
    key(&mut app, "Home");
    assert_eq!(selected(&app, l), [0]);
    // A page is the number of rows in the viewport (10 here).
    key(&mut app, "PageDown");
    assert_eq!(selected(&app, l), [10]);
}

#[test]
fn shift_arrows_extend_the_selection() {
    let mut app = app();
    let l = list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 200.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Multi));
    step(&mut app, 2);
    let row = row_for(&mut app, 4);
    click(&mut app, row, UiModifiers::default());
    focus_row(&mut app, 4);
    step(&mut app, 1);
    // Shift held, then the cursor jumps to the end of the page.
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "ShiftLeft".to_owned(),
        logical_key: "Shift".to_owned(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
    key(&mut app, "PageDown");
    assert_eq!(selected(&app, l), (4..=14).collect::<Vec<_>>());
}

#[test]
fn type_ahead_jumps_to_the_first_match() {
    let mut app = app();
    let names = ["Apple", "Avocado", "Banana", "Blueberry", "Cherry", "Date"];
    let items = ListItems(
        names
            .iter()
            .map(|n| ListItem {
                label: (*n).to_owned(),
                subtitle: String::new(),
            })
            .collect(),
    );
    let l = app
        .world_mut()
        .spawn((
            ListView::new(20.0, 2),
            items,
            ListSelection::new(SelectionMode::Single),
            ScrollOffset::default(),
            computed([0.0, 0.0, 200.0, 200.0]),
        ))
        .id();
    step(&mut app, 2);
    focus_row(&mut app, 0);
    step(&mut app, 1);
    key(&mut app, "c");
    assert_eq!(selected(&app, l), [4]);
    // A fresh prefix after the timeout restarts; "b" then "l" refine to Blueberry.
    step(&mut app, 12);
    key(&mut app, "b");
    assert_eq!(selected(&app, l), [2]);
    key(&mut app, "l");
    assert_eq!(selected(&app, l), [3]);
}

#[test]
fn scroll_to_item_aligns() {
    let mut app = app();
    let l = list(&mut app, 1000, 20.0, [0.0, 0.0, 200.0, 100.0]);
    step(&mut app, 2);
    request_scroll_to_item(app.world_mut(), l, 500, ScrollAlign::Start).unwrap();
    step(&mut app, 1);
    assert_eq!(app.world().get::<ScrollOffset>(l).unwrap().0[1], 10_000.0);
    step(&mut app, 1);
    assert!(list_info(app.world(), l).unwrap().range.contains(&500));
    request_scroll_to_item(app.world_mut(), l, 500, ScrollAlign::Center).unwrap();
    step(&mut app, 1);
    assert_eq!(
        app.world().get::<ScrollOffset>(l).unwrap().0[1],
        10_000.0 + 10.0 - 50.0
    );
    // Past the end: refused. The last item can only scroll as far as the content allows.
    assert!(request_scroll_to_item(app.world_mut(), l, 1000, ScrollAlign::Start).is_err());
    request_scroll_to_item(app.world_mut(), l, 999, ScrollAlign::Start).unwrap();
    step(&mut app, 1);
    assert_eq!(app.world().get::<ScrollOffset>(l).unwrap().0[1], 20_000.0 - 100.0);
}

#[test]
fn sticky_header_sticks_and_is_pushed_out() {
    let mut app = app();
    let l = list(&mut app, 10, 20.0, [0.0, 0.0, 200.0, 100.0]);
    app.world_mut().entity_mut(l).insert(ListHeaders(vec![0, 5]));
    step(&mut app, 3);
    let sticky = app
        .world()
        .get::<ListState>(l)
        .unwrap()
        .sticky_node()
        .expect("sticky node");
    let top = |app: &App| match app.world().get::<NodeStyle>(sticky).unwrap().0.inset.top {
        Dimension::Points(p) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(top(&app), 0.0);
    set_offset(&mut app, l, 70.0);
    step(&mut app, 1);
    assert_eq!(top(&app), 70.0);
    // The next header (top 100) pushes the sticky one up: 100 - 20.
    set_offset(&mut app, l, 90.0);
    step(&mut app, 1);
    assert_eq!(top(&app), 80.0);
    // At the next header the sticky node shows that section.
    set_offset(&mut app, l, 100.0);
    step(&mut app, 1);
    assert_eq!(top(&app), 100.0);
    // Header rows carry their own class.
    let header_row = row_for(&mut app, 5);
    assert_eq!(app.world().get::<Classes>(header_row).unwrap().0, ["list-header"]);
}

#[test]
fn selection_api_reports_and_validates() {
    let mut app = app();
    let l = list(&mut app, 20, 20.0, [0.0, 0.0, 200.0, 100.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Multi));
    step(&mut app, 2);
    assert_eq!(
        select_items(app.world_mut(), l, SelectOp::Set, &[1, 3]).unwrap(),
        [1, 3]
    );
    assert_eq!(
        select_items(app.world_mut(), l, SelectOp::Toggle, &[3, 4]).unwrap(),
        [1, 4]
    );
    assert_eq!(
        select_items(app.world_mut(), l, SelectOp::Range, &[6, 8]).unwrap(),
        [6, 7, 8]
    );
    assert_eq!(
        select_items(app.world_mut(), l, SelectOp::All, &[])
            .unwrap()
            .len(),
        20
    );
    assert!(
        select_items(app.world_mut(), l, SelectOp::Clear, &[])
            .unwrap()
            .is_empty()
    );
    let out_of_range = select_items(app.world_mut(), l, SelectOp::Set, &[20]).unwrap_err();
    assert_eq!(out_of_range.code(), "index_out_of_range");
    let no_selection = list(&mut app, 5, 20.0, [0.0, 0.0, 200.0, 100.0]);
    let err = select_items(app.world_mut(), no_selection, SelectOp::Set, &[1]).unwrap_err();
    assert_eq!(err.code(), "no_selection");
    let not_a_list = app.world_mut().spawn_empty().id();
    assert_eq!(
        select_items(app.world_mut(), not_a_list, SelectOp::Clear, &[])
            .unwrap_err()
            .code(),
        "not_a_list"
    );
}

#[test]
fn shrinking_the_list_drops_stale_selection() {
    let mut app = app();
    let l = list(&mut app, 50, 20.0, [0.0, 0.0, 200.0, 100.0]);
    app.world_mut()
        .entity_mut(l)
        .insert(ListSelection::new(SelectionMode::Multi));
    step(&mut app, 2);
    select_items(app.world_mut(), l, SelectOp::Set, &[10, 40]).unwrap();
    app.world_mut().entity_mut(l).insert(ItemCount(20));
    step(&mut app, 3);
    assert_eq!(selected(&app, l), [10]);
    let info = list_info(app.world(), l).unwrap();
    assert_eq!(info.count, 20);
    assert_eq!(info.total_height, 400.0);
}

// --- Scrollbars -----------------------------------------------------------------------------

/// A parent with one scroll view (200 x 300 viewport, `content` of content) and scrollbars.
fn scroll_view(app: &mut App, content: [f32; 2], bars: Scrollbars) -> (Entity, Entity) {
    let parent = app.world_mut().spawn(computed([0.0, 0.0, 400.0, 400.0])).id();
    let mut node = computed([0.0, 0.0, 200.0, 300.0]);
    node.content_size = content;
    let view = app
        .world_mut()
        .spawn((
            ScrollView::default(),
            ScrollOffset::default(),
            node,
            bars,
            ChildOf(parent),
        ))
        .id();
    (parent, view)
}

fn bar(app: &App, view: Entity, axis: ScrollAxis) -> (Entity, Entity) {
    app.world()
        .get::<ScrollbarNodes>(view)
        .unwrap()
        .bar(axis)
        .expect("bar spawned")
}

#[test]
fn scrollbars_spawn_as_siblings_sized_to_the_content() {
    let mut app = app();
    let (parent, view) = scroll_view(&mut app, [200.0, 1500.0], Scrollbars::default());
    step(&mut app, 3);
    let (track, thumb) = bar(&app, view, ScrollAxis::Vertical);
    assert_eq!(app.world().get::<ChildOf>(track).unwrap().0, parent);
    assert_eq!(app.world().get::<ChildOf>(thumb).unwrap().0, parent);
    // The thumb is a fifth of the track: 300 / 1500.
    let style = &app.world().get::<NodeStyle>(thumb).unwrap().0;
    assert_eq!(style.size[1], Dimension::Points(60.0));
    assert_eq!(style.size[0], Dimension::Points(8.0));
    assert_eq!(style.inset.left, Dimension::Points(192.0));
    // The built-in thin thumb stepped aside.
    assert!(!app.world().get::<ScrollView>(view).unwrap().show_scrollbar);
    // No horizontal bar: the content is as wide as the view.
    assert!(
        app.world()
            .get::<ScrollbarNodes>(view)
            .unwrap()
            .bar(ScrollAxis::Horizontal)
            .is_none()
    );
}

#[test]
fn thumb_drag_scrolls_the_view() {
    let mut app = app();
    let (_, view) = scroll_view(&mut app, [200.0, 1500.0], Scrollbars::default());
    step(&mut app, 3);
    let (track, thumb) = bar(&app, view, ScrollAxis::Vertical);
    app.world_mut()
        .entity_mut(track)
        .insert(computed([192.0, 0.0, 8.0, 300.0]));
    // 240 px of thumb travel (track 300 - thumb 60) covers the 1200 px scroll range.
    app.world_mut()
        .write_message(UiEvent::new(thumb, UiEventKind::Drag([0.0, 24.0])));
    step(&mut app, 1);
    assert_eq!(app.world().get::<ScrollOffset>(view).unwrap().0[1], 120.0);
    app.world_mut()
        .write_message(UiEvent::new(thumb, UiEventKind::Drag([0.0, 1000.0])));
    step(&mut app, 1);
    // Clamped to the end of the content.
    assert_eq!(app.world().get::<ScrollOffset>(view).unwrap().0[1], 1200.0);
}

#[test]
fn track_click_centers_the_thumb() {
    let mut app = app();
    let (_, view) = scroll_view(&mut app, [200.0, 1500.0], Scrollbars::default());
    step(&mut app, 3);
    let (track, _) = bar(&app, view, ScrollAxis::Vertical);
    app.world_mut()
        .entity_mut(track)
        .insert(computed([192.0, 0.0, 8.0, 300.0]));
    app.world_mut().resource_mut::<UiPointer>().cursor = Some([196.0, 150.0]);
    app.world_mut()
        .write_message(UiEvent::new(track, UiEventKind::Click));
    step(&mut app, 1);
    // (150 - 30) / 240 of 1200.
    assert_eq!(app.world().get::<ScrollOffset>(view).unwrap().0[1], 600.0);
}

#[test]
fn horizontal_bar_appears_for_wide_content() {
    let mut app = app();
    let bars = Scrollbars {
        horizontal: true,
        ..Default::default()
    };
    let (_, view) = scroll_view(&mut app, [800.0, 1500.0], bars);
    step(&mut app, 3);
    let (_, h_thumb) = bar(&app, view, ScrollAxis::Horizontal);
    let style = &app.world().get::<NodeStyle>(h_thumb).unwrap().0;
    // The vertical bar takes 8 px of the width: track 192, thumb 192 * 200 / 800.
    assert_eq!(style.size[0], Dimension::Points(48.0));
    assert_eq!(style.size[1], Dimension::Points(8.0));
}

#[test]
fn bars_hide_after_the_idle_time_and_return_on_scroll() {
    let mut app = app();
    let bars = Scrollbars {
        auto_hide: Some(0.5),
        ..Default::default()
    };
    let (_, view) = scroll_view(&mut app, [200.0, 1500.0], bars);
    step(&mut app, 3);
    let (_, thumb) = bar(&app, view, ScrollAxis::Vertical);
    let display = |app: &App| app.world().get::<NodeStyle>(thumb).unwrap().0.display;
    assert_ne!(display(&app), Display::None);
    // 100 ms per update: eight updates is well past the 0.5 s limit.
    step(&mut app, 8);
    assert_eq!(display(&app), Display::None);
    set_offset_view(&mut app, view, 100.0);
    step(&mut app, 1);
    assert_ne!(display(&app), Display::None);
}

fn set_offset_view(app: &mut App, view: Entity, y: f32) {
    app.world_mut().get_mut::<ScrollOffset>(view).unwrap().0[1] = y;
}

#[test]
fn removing_the_scrollbars_removes_the_bars() {
    let mut app = app();
    let (_, view) = scroll_view(&mut app, [200.0, 1500.0], Scrollbars::default());
    step(&mut app, 3);
    let (track, thumb) = bar(&app, view, ScrollAxis::Vertical);
    app.world_mut().entity_mut(view).remove::<Scrollbars>();
    step(&mut app, 2);
    assert!(app.world().get_entity(track).is_err());
    assert!(app.world().get_entity(thumb).is_err());
}

#[test]
fn focus_moving_into_a_view_scrolls_it_once() {
    let mut app = app();
    let (_, view) = scroll_view(&mut app, [200.0, 1500.0], Scrollbars::default());
    // A node below the viewport (bottom edge at 300).
    let node = app
        .world_mut()
        .spawn((computed([0.0, 380.0, 100.0, 40.0]), ChildOf(view)))
        .id();
    step(&mut app, 1);
    app.world_mut().resource_mut::<FocusState>().focused = Some(node);
    step(&mut app, 1);
    assert_eq!(app.world().get::<ScrollOffset>(view).unwrap().0[1], 120.0);
    // The wheel can then scroll away: focus does not pin the view.
    app.world_mut().resource_mut::<UiPointer>().wheel = [0.0, 200.0];
    step(&mut app, 1);
    assert!(app.world().get::<ScrollOffset>(view).unwrap().0[1] < 120.0);
}

/// Ported from the audit3 fix `unchanged_row_style_is_not_rewritten`, written for the v1 list in
/// `scroll.rs` that list v2 replaced: an idle list must not write its row styles again, or
/// `Changed<NodeStyle>` dirties layout and extract every frame.
#[test]
fn an_idle_list_does_not_rewrite_its_row_styles() {
    let mut app = app();
    list(&mut app, 100, 20.0, [0.0, 0.0, 200.0, 300.0]);
    step(&mut app, 3);
    let row = row_for(&mut app, 0);
    let changed = |app: &App| {
        app.world()
            .entity(row)
            .get_change_ticks::<NodeStyle>()
            .expect("row style")
            .changed
    };
    let before = changed(&app);
    step(&mut app, 3);
    assert_eq!(changed(&app), before, "an idle list re-inserted a row style");
}

/// A virtual list scrolls by moving a handful of pooled rows. That must not mark a full flexbox
/// pass (ADR-520): `full_runs` stays put and the frame is a scroll walk or an incremental one.
/// Covers a plain list and one with sticky headers, whose header `y` tracks the offset.
#[test]
fn scrolling_a_virtual_list_does_not_force_a_full_flexbox_pass() {
    fn run(headers: bool) -> (u64, u64, f32) {
        let mut app = app();
        app.world_mut().resource_mut::<regia_ui::UiScales>().window_size = [800.0, 600.0];
        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot {
                    scale_mode: ScaleMode::ConstantPixelSize,
                    ..Default::default()
                },
                NodeStyle::new(Style {
                    size: [Dimension::Points(800.0), Dimension::Points(600.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let mut view = ScrollView::vertical();
        view.show_scrollbar = false;
        let list = world
            .spawn((
                regia_ui::scroll_container(
                    Style {
                        size: [Dimension::Points(400.0), Dimension::Points(600.0)],
                        direction: regia_ui::style::FlexDirection::Column,
                        ..Default::default()
                    },
                    view,
                ),
                ListView::new(24.0, 2),
                ItemCount(4_000),
                ChildOf(root),
            ))
            .id();
        if headers {
            world
                .entity_mut(list)
                .insert(ListHeaders((0..4_000).step_by(40).collect()));
        }
        step(&mut app, 6);
        app.world_mut().write_message(regia_platform::CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (200.0, 300.0),
            timestamp: Default::default(),
        });
        app.update();
        let before = *app.world().resource::<regia_ui::UiLayoutStats>();
        const FRAMES: u64 = 12;
        for _ in 0..FRAMES {
            app.world_mut().write_message(regia_platform::MouseWheel {
                window: Entity::PLACEHOLDER,
                delta: (0.0, -80.0),
                unit: regia_platform::WheelUnit::Pixel,
                timestamp: Default::default(),
            });
            app.update();
        }
        let after = *app.world().resource::<regia_ui::UiLayoutStats>();
        let offset = app
            .world()
            .get::<ScrollOffset>(list)
            .map(|o| o.0[1])
            .unwrap_or(0.0);
        (
            after.full_runs.saturating_sub(before.full_runs),
            (after.scroll_runs + after.incremental_runs)
                .saturating_sub(before.scroll_runs + before.incremental_runs),
            offset,
        )
    }
    for headers in [false, true] {
        let (full, other, offset) = run(headers);
        assert!(
            offset > 100.0,
            "headers={headers}: the list did not scroll (offset {offset})"
        );
        assert_eq!(
            full, 0,
            "headers={headers}: {full} full flexbox passes while scrolling"
        );
        assert!(
            other > 0,
            "headers={headers}: expected scroll-only or incremental runs, got none"
        );
    }
}

/// Roadmap lane 8 acceptance ("single/multi selection with keyboard (Shift/Ctrl) and gamepad"):
/// with real layout under a window root, the d-pad moves focus from row to row and the selection
/// follows the focus (ADR-344), one row at a time, without a modifier.
#[test]
fn the_dpad_moves_the_selection_with_focus() {
    use regia_platform::{GamepadButton, GamepadButtonChanged};
    let mut app = app();
    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style::default()),
        ))
        .id();
    let l = app
        .world_mut()
        .spawn((
            ListView::new(20.0, 2),
            ItemCount(50),
            ScrollOffset::default(),
            ListSelection::new(SelectionMode::Multi),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(10.0),
                    top: Dimension::Points(10.0),
                    right: Dimension::Auto,
                    bottom: Dimension::Auto,
                },
                size: [Dimension::Points(200.0), Dimension::Points(200.0)],
                ..Default::default()
            }),
            ChildOf(root),
        ))
        .id();
    step(&mut app, 4);
    focus_row(&mut app, 3);
    step(&mut app, 1);
    assert_eq!(selected(&app, l), vec![3], "focus on a row selects it");
    let pad = |app: &mut App, button: GamepadButton| {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            app.world_mut().write_message(GamepadButtonChanged {
                device: 1,
                button,
                state,
                value: 1.0,
            });
            app.update();
        }
        app.update();
    };
    pad(&mut app, GamepadButton::DPadDown);
    let focused_row = app
        .world()
        .resource::<FocusState>()
        .focused
        .and_then(|f| app.world().get::<ListRow>(f))
        .map(|r| r.index);
    assert_eq!(focused_row, Some(4), "the d-pad moved focus to the next row");
    assert_eq!(selected(&app, l), vec![4], "the selection followed the focus");
    pad(&mut app, GamepadButton::DPadDown);
    pad(&mut app, GamepadButton::DPadUp);
    assert_eq!(selected(&app, l), vec![4]);
    assert_eq!(list_info(app.world(), l).unwrap().cursor, Some(4));
}
