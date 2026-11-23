// SPDX-License-Identifier: MPL-2.0
//! Virtualized repeats (ADR-2715, `docs/specs/81-ui-designer.md` § 2.8): a `repeat` on a `list`
//! widget materializes only the visible items plus the overscan, gives the rows of items that
//! scroll out to items that scroll in, keeps the first visible item on screen when the collection
//! changes (anchor key), and its item bindings follow the row's current item. Headless: no
//! window, no GPU, fixed 100 ms steps; every check is a counter or a laid-out rect.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::agent_bridge::call;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::{
    ComputedNode, ListRow, ListSelection, NodeName, RepeatHost, SelectionMode, UiMigrators, ViewModelAppExt,
    parse_ui_ron,
};
use serde_json::json;

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
struct Item {
    id: u32,
    name: String,
    count: u32,
}

fn item(id: u32) -> Item {
    Item {
        id,
        name: format!("item {id}"),
        count: id,
    }
}

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct FeedVm {
    items: Vec<Item>,
}

/// Row height and viewport of the list below (UI px).
const ROW: f32 = 40.0;
const VIEWPORT: f32 = 400.0;
const OVERSCAN: usize = 2;

const LIST: &str = r#"(version: 6, root: (
    style: (direction: column),
    children: [
        (name: "feed", widget: list(0, 40.0), style: (size: (300, 400)), clip: true,
            repeat: (source: "FeedVm.items", key: "id"),
            children: [
                (name: "row", style: (direction: row, size: (Percent(100), Percent(100))), children: [
                    (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
                    (name: "qty", text: (spans: ["0"]), bindings: [(source: ".count", format: "x{:03}")]),
                    (name: "first", text: (spans: ["?"]), bindings: [(source: ".name", mode: one_time)]),
                ]),
            ]),
    ],
))"#;

fn app(count: u32) -> App {
    app_with(LIST, count)
}

/// An app showing `layout` (template `t`) over `count` items.
fn app_with(layout: &str, count: u32) -> App {
    let mut app = App::new();
    app.add_plugins(regia_ui::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.register_view_model::<FeedVm>();
    app.update();
    app.world_mut().resource_mut::<FeedVm>().items = (0..count).map(item).collect();
    let file = parse_ui_ron(layout, "t.ui.ron", &UiMigrators::default()).expect("layout");
    let world = app.world_mut();
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert("t".to_owned(), file);
    spawn_ui(world, "t").expect("spawn");
    // Spawn, lay out, then the pass that sees the laid-out list.
    for _ in 0..3 {
        app.update();
    }
    app
}

fn named(app: &mut App, name: &str) -> Vec<Entity> {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    q.iter(world)
        .filter(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .collect()
}

fn feed(app: &mut App) -> Entity {
    named(app, "feed")[0]
}

fn host(app: &App, entity: Entity) -> &RepeatHost {
    app.world().get::<RepeatHost>(entity).expect("repeat host")
}

fn text(app: &App, entity: Entity) -> String {
    match &app.world().get::<Text>(entity).expect("text").spans[0].content {
        TextContent::Lit(s) => s.clone(),
        TextContent::Loc { key, .. } => format!("loc:{key}"),
    }
}

/// The row of collection index `index` (it must be materialized).
fn row_of(app: &App, list: Entity, index: usize) -> Entity {
    let roots = host(app, list).roots(index);
    assert_eq!(roots.len(), 1, "item {index} has no row");
    roots[0]
}

/// A named node inside a row (row → template root → leaf).
fn leaf(app: &App, row: Entity, name: &str) -> Entity {
    let world = app.world();
    let mut stack = vec![row];
    while let Some(e) = stack.pop() {
        if world.get::<NodeName>(e).is_some_and(|n| n.0 == name) {
            return e;
        }
        if let Some(children) = world.get::<Children>(e) {
            stack.extend(children.iter());
        }
    }
    panic!("no {name} under {row:?}");
}

fn scale(app: &App, list: Entity) -> f32 {
    app.world().get::<ComputedNode>(list).expect("laid out").scale
}

/// Scrolls the list to `y` UI px.
fn scroll_to(app: &mut App, list: Entity, y: f32) {
    let s = scale(app, list);
    app.world_mut().get_mut::<ScrollOffset>(list).expect("offset").0[1] = y * s;
    app.update();
    app.update();
}

fn offset(app: &App, list: Entity) -> f32 {
    app.world().get::<ScrollOffset>(list).expect("offset").0[1] / scale(app, list)
}

/// Rows the list's viewport can show at once (a partly visible row at each edge included).
fn visible_rows() -> usize {
    (VIEWPORT / ROW).ceil() as usize + 1
}

/// A 10,000-item collection in a list keeps at most visible + 2 x overscan rows, at the top and
/// after a long scroll. Once the window has its full length (overscan above and below), scrolling
/// spawns nothing: every row comes back recycled.
#[test]
fn ten_thousand_items_materialize_only_the_visible_rows() {
    let mut app = app(10_000);
    let list = feed(&mut app);
    let bound = visible_rows() + 2 * OVERSCAN;
    {
        let h = host(&app, list);
        assert!(h.is_virtual());
        assert_eq!(h.item_count(), 10_000);
        assert!(h.instance_count() > 0, "the laid-out list shows rows");
        assert!(
            h.instance_count() <= bound,
            "{} rows > {bound}",
            h.instance_count()
        );
        assert_eq!(h.first_index(), 0);
        assert_eq!(h.anchor_key(), Some("0"));
    }
    // Counter over the whole world, not just the host's bookkeeping.
    assert!(named(&mut app, "label").len() <= bound);
    assert_eq!(text(&app, leaf(&app, row_of(&app, list, 0), "label")), "item 0");
    assert_eq!(
        host(&app, list).spawned_total() as usize,
        host(&app, list).instance_count()
    );

    scroll_to(&mut app, list, 4000.0);
    let spawned = {
        let h = host(&app, list);
        assert_eq!(
            h.anchor_key(),
            Some("100"),
            "4000 / 40 = item 100 is the first visible"
        );
        assert_eq!(h.first_index(), 100 - OVERSCAN);
        assert!(h.instance_count() <= bound);
        // At the top there is no overscan above the view: the window grows by that much once.
        assert_eq!(h.spawned_total() as usize, h.instance_count());
        assert!(h.recycled_total() > 0);
        h.spawned_total()
    };
    for y in [4013.0, 9000.0, 120_000.0, 20.0, 4000.0] {
        scroll_to(&mut app, list, y);
        let h = host(&app, list);
        assert_eq!(
            h.spawned_total(),
            spawned,
            "scrolling to {y} recycles, it never spawns"
        );
        assert!(h.instance_count() <= bound);
        assert!(
            h.instance_count() + h.spare_rows() <= bound,
            "rows alive, spares included"
        );
        assert!(named(&mut app, "label").len() <= bound);
    }
    assert!(named(&mut app, "label").len() <= bound);
    let row = row_of(&app, list, 105);
    assert_eq!(text(&app, leaf(&app, row, "label")), "item 105");
    assert_eq!(text(&app, leaf(&app, row, "qty")), "x105");
    assert_eq!(app.world().get::<ListRow>(row).expect("row").index, 105);

    // The list's scroll extent is the whole collection, and its own row pool stays empty.
    let state = app.world().get::<regia_ui::ListState>(list).expect("state");
    assert_eq!(state.item_count(), 10_000);
    assert_eq!(state.spawned_rows(), 0, "the list widget spawned no pool rows");

    // An idle frame does nothing.
    let passes = host(&app, list).stats().passes;
    app.update();
    assert_eq!(host(&app, list).stats().passes, passes);
}

/// Inserting items above the view keeps the first visible item where it was on screen: the scroll
/// offset moves by the inserted rows.
#[test]
fn inserting_at_the_top_keeps_the_anchored_row_on_screen() {
    let mut app = app(1000);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 50.0 * ROW + 10.0);
    assert_eq!(host(&app, list).anchor_key(), Some("50"));
    let before_y = app
        .world()
        .get::<ComputedNode>(row_of(&app, list, 50))
        .expect("rect")
        .rect[1];
    let before_offset = offset(&app, list);

    app.world_mut()
        .resource_mut::<FeedVm>()
        .items
        .splice(0..0, (10_000..10_003).map(item));
    app.update();
    app.update();

    let h = host(&app, list);
    assert_eq!(h.anchor_key(), Some("50"), "the anchor follows its key");
    assert_eq!(h.index_of("50"), Some(53));
    assert_eq!(h.item_count(), 1003);
    assert!((offset(&app, list) - (before_offset + 3.0 * ROW)).abs() < 0.01);
    let row = row_of(&app, list, 53);
    assert_eq!(text(&app, leaf(&app, row, "label")), "item 50");
    let after = app.world().get::<ComputedNode>(row).expect("rect").rect;
    assert!(
        (after[1] - before_y).abs() < 0.5,
        "row moved on screen: {before_y} -> {}",
        after[1]
    );
    // On screen: the anchor is the first visible row (its top 10 px are scrolled past the edge).
    let list_rect = app.world().get::<ComputedNode>(list).expect("list").rect;
    assert!(
        after[1] + after[3] > list_rect[1] && after[1] < list_rect[1] + list_rect[3],
        "{after:?} outside {list_rect:?}"
    );
    assert!((list_rect[1] - after[1] - 10.0 * scale(&app, list)).abs() < 0.5);

    // Removing the anchor item hands the anchor to the next one, still in place.
    let before_y = after[1];
    let next_y = app
        .world()
        .get::<ComputedNode>(row_of(&app, list, 54))
        .expect("rect")
        .rect[1];
    app.world_mut().resource_mut::<FeedVm>().items.remove(53);
    app.update();
    app.update();
    assert_eq!(host(&app, list).anchor_key(), Some("51"));
    let y = app
        .world()
        .get::<ComputedNode>(row_of(&app, list, 53))
        .expect("rect")
        .rect[1];
    assert!(
        (y - before_y).abs() < 0.5,
        "51 took 50's place: {y} vs {before_y} (was at {next_y})"
    );
}

/// Removing a visible item spawns nothing: its row goes to the item that enters at the bottom,
/// the other rows keep their entities and items.
#[test]
fn removing_a_visible_row_reuses_rows_without_spawning() {
    let mut app = app(1000);
    let list = feed(&mut app);
    let spawned = host(&app, list).spawned_total();
    let rows_before = host(&app, list).instance_count();
    let kept: Vec<(usize, Entity)> = [0usize, 1, 2, 4, 5]
        .iter()
        .map(|&i| (i, row_of(&app, list, i)))
        .collect();

    app.world_mut().resource_mut::<FeedVm>().items.remove(3);
    app.update();

    let stats = host(&app, list).stats();
    assert_eq!(stats.spawned, 0, "{stats:?}");
    assert_eq!(stats.despawned, 0, "{stats:?}");
    assert_eq!(stats.recycled, 1, "{stats:?}");
    assert_eq!(host(&app, list).spawned_total(), spawned);
    assert_eq!(host(&app, list).instance_count(), rows_before);
    // Items 0..=2 keep their rows and index; 4 and 5 keep their rows one index up.
    for (old, row) in kept {
        let id = old as u32;
        let now = host(&app, list).index_of(&id.to_string()).expect("still there");
        assert_eq!(row_of(&app, list, now), row, "item {old} kept its row");
        assert_eq!(text(&app, leaf(&app, row, "label")), format!("item {old}"));
    }
    assert_eq!(text(&app, leaf(&app, row_of(&app, list, 3), "label")), "item 4");
}

/// Bindings inside rows follow their item: a change to a materialized item rewrites its row only,
/// a recycled row shows its new item (one-time bindings included), and an item without a row
/// costs nothing until it scrolls in.
#[test]
fn bindings_inside_rows_update() {
    let mut app = app(1000);
    let list = feed(&mut app);
    let row = row_of(&app, list, 1);

    {
        let mut vm = app.world_mut().resource_mut::<FeedVm>();
        vm.items[1].name = "renamed".to_owned();
        vm.items[1].count = 7;
        // Off screen: no row to write.
        vm.items[900].name = "far away".to_owned();
    }
    app.update();
    let stats = host(&app, list).stats();
    assert_eq!(
        (stats.spawned, stats.recycled, stats.touched),
        (0, 0, 1),
        "{stats:?}"
    );
    assert_eq!(text(&app, leaf(&app, row, "label")), "renamed");
    assert_eq!(text(&app, leaf(&app, row, "qty")), "x007");
    assert_eq!(
        text(&app, leaf(&app, row, "first")),
        "item 1",
        "one-time keeps its first value"
    );

    scroll_to(&mut app, list, 900.0 * ROW);
    let row = row_of(&app, list, 900);
    assert_eq!(text(&app, leaf(&app, row, "label")), "far away");
    assert_eq!(text(&app, leaf(&app, row, "qty")), "x900");
    assert_eq!(
        text(&app, leaf(&app, row, "first")),
        "far away",
        "a recycled row pushes its one-time binding for the new item"
    );
}

/// The list's selection marks the repeat rows that show selected items, through recycling.
#[test]
fn list_selection_marks_repeat_rows() {
    let mut app = app(100);
    let list = feed(&mut app);
    let mut selection = ListSelection::new(SelectionMode::Single);
    assert!(selection.select_only(2));
    app.world_mut().entity_mut(list).insert(selection);
    app.update();
    app.update();
    for i in 0..5 {
        let row = row_of(&app, list, i);
        assert_eq!(app.world().get::<Selected>(row).is_some(), i == 2, "row {i}");
    }
    scroll_to(&mut app, list, 50.0 * ROW);
    let first = host(&app, list).first_index();
    for i in first..first + host(&app, list).instance_count() {
        assert!(
            app.world().get::<Selected>(row_of(&app, list, i)).is_none(),
            "row {i}"
        );
    }
}

/// `ui.collection_state` reports the window of a virtualized repeat.
#[test]
fn collection_state_reports_the_window() {
    let mut app = app(10_000);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 200.0 * ROW);
    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    let row = &state["hosts"][0];
    assert_eq!(row["virtual"], json!(true));
    assert_eq!(row["items"], json!(10_000));
    assert_eq!(row["first_visible_key"], json!("200"));
    assert_eq!(row["first_index"], json!(200 - OVERSCAN));
    let materialized = row["materialized"].as_u64().expect("materialized") as usize;
    assert!(materialized <= visible_rows() + 2 * OVERSCAN);
    assert_eq!(row["instances"], json!(materialized));
    assert!(row["recycled_total"].as_u64().expect("recycled") > 0);
    // Spec 81 § 5: the window fields agree with the host, and spares stay within 2 x overscan + 2.
    let h = host(&app, list);
    assert_eq!(row["spare_rows"], json!(h.spare_rows()));
    assert!(h.spare_rows() <= 2 * OVERSCAN + 2, "{}", h.spare_rows());
    assert_eq!(row["spawned_total"], json!(h.spawned_total()));
    assert_eq!(row["recycled_total"], json!(h.recycled_total()));
    assert!(
        h.spawned_total() as usize <= materialized + h.spare_rows(),
        "a long scroll spawns no more rows than it shows: {}",
        h.spawned_total()
    );
}

/// Spec 81 § 2.8 "Refresh": with the model unchanged, a change of the list's overscan, row height
/// or rect alone re-windows the rows (the view key moved). Every check is the host's own window.
#[test]
fn the_window_follows_overscan_row_height_and_rect_changes() {
    let mut app = app(10_000);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 4000.0);
    assert_eq!(host(&app, list).first_index(), 100 - OVERSCAN);
    let rows = host(&app, list).instance_count();

    // Overscan 2 -> 4: two more rows above and below.
    app.world_mut().get_mut::<ListView>(list).expect("list").overscan = 4;
    app.update();
    app.update();
    {
        let h = host(&app, list);
        assert_eq!(h.first_index(), 100 - 4, "the window reaches 4 rows above");
        assert_eq!(h.instance_count(), rows + 4, "and 2 more each side");
        assert_eq!(h.anchor_key(), Some("100"));
    }

    // Row height 40 -> 20 at the same offset: item 200 is now the first visible.
    app.world_mut()
        .get_mut::<ListView>(list)
        .expect("list")
        .item_height = 20.0;
    app.update();
    app.update();
    let rows_at_20 = {
        let h = host(&app, list);
        assert_eq!(h.anchor_key(), Some("200"), "4000 / 20 = item 200");
        assert_eq!(h.first_index(), 200 - 4);
        assert!(h.instance_count() > rows + 4, "a shorter row shows more items");
        assert!(h.instance_count() <= (VIEWPORT / 20.0).ceil() as usize + 1 + 2 * 4);
        h.instance_count()
    };
    let row = row_of(&app, list, 200);
    let rect = app.world().get::<ComputedNode>(row).expect("rect").rect;
    assert!(
        (rect[3] - 20.0 * scale(&app, list)).abs() < 0.5,
        "rows are placed at the new height: {rect:?}"
    );

    // The list's rect 400 -> 800 tall: the window grows with the viewport.
    app.world_mut().get_mut::<NodeStyle>(list).expect("style").0.size[1] = Dimension::Points(800.0);
    for _ in 0..3 {
        app.update();
    }
    let h = host(&app, list);
    assert_eq!(h.first_index(), 200 - 4, "the top of the view did not move");
    assert!(
        h.instance_count() > rows_at_20,
        "a taller list shows more rows: {} vs {rows_at_20}",
        h.instance_count()
    );
    assert!(h.instance_count() <= (800.0f32 / 20.0).ceil() as usize + 1 + 2 * 4);
    assert_eq!(
        text(&app, leaf(&app, row_of(&app, list, 230), "label")),
        "item 230"
    );
}

/// Spec 81 § 2.8 "Anchor": when the anchor item and every item after it are gone, the offset
/// stays and is clamped to the new scroll range; the window then shows the end of the list.
#[test]
fn a_lost_anchor_keeps_the_offset_clamped_to_the_new_range() {
    let mut app = app(1000);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 900.0 * ROW);
    assert_eq!(host(&app, list).anchor_key(), Some("900"));

    app.world_mut().resource_mut::<FeedVm>().items.truncate(20);
    app.update();
    app.update();
    // The new scroll range: 20 rows less the laid-out viewport (layout snaps the rect).
    let viewport = app.world().get::<ComputedNode>(list).expect("list").rect[3] / scale(&app, list);
    assert!((viewport - VIEWPORT).abs() <= 1.0, "viewport {viewport}");
    let max = 20.0 * ROW - viewport;
    let at = offset(&app, list);
    assert!(
        (at - max).abs() < 0.01,
        "offset {at} clamped to the new range {max}"
    );
    let h = host(&app, list);
    assert_eq!(h.item_count(), 20);
    let first_visible = (at / ROW).floor() as u32;
    assert_eq!(
        h.anchor_key(),
        Some(first_visible.to_string().as_str()),
        "the first visible item at the clamped offset"
    );
    assert!(h.first_index() + h.instance_count() <= 20);
    assert_eq!(text(&app, leaf(&app, row_of(&app, list, 19), "label")), "item 19");
}

const EDIT_LIST: &str = r#"(version: 6, root: (
    style: (direction: column),
    children: [
        (name: "feed", widget: list(0, 40.0), style: (size: (300, 400)), clip: true,
            repeat: (source: "FeedVm.items", key: "id"),
            children: [
                (name: "qty", widget: slider(0.0, 0.0, 10000.0, 0.0), command: "feed.pick",
                    style: (size: (Percent(100), Percent(100))),
                    bindings: [(source: ".count", target: value, mode: two_way)]),
            ]),
    ],
))"#;

#[derive(Resource, Default)]
struct SeenCommands(Vec<regia_ui::UiCommand>);

fn record_commands(mut events: MessageReader<regia_ui::UiCommand>, mut seen: ResMut<SeenCommands>) {
    seen.0.extend(events.read().cloned());
}

/// Spec 81 § 2.8: two-way writes and commands in a virtual row name the row's current item (its
/// key, its collection index), also after the row was recycled by a scroll.
#[test]
fn two_way_writes_and_commands_in_virtual_rows_name_the_current_item() {
    let mut app = app_with(EDIT_LIST, 1000);
    app.init_resource::<SeenCommands>()
        .add_systems(PostUpdate, record_commands);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 500.0 * ROW);
    assert!(host(&app, list).recycled_total() > 0);
    let row = row_of(&app, list, 503);
    let slider = leaf(&app, row, "qty");
    assert_eq!(
        app.world().get::<Slider>(slider).expect("slider").value,
        503.0,
        "the recycled row shows its new item"
    );

    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": slider.to_bits(), "value": 7.0 }),
    )
    .expect("set");
    app.update();
    app.update();
    {
        let vm = app.world().resource::<FeedVm>();
        assert_eq!(vm.items[503].count, 7, "the row's current item is written");
        assert_eq!(vm.items[1].count, 1, "not the item the row showed first");
        assert_eq!(vm.items[502].count, 502);
        assert_eq!(vm.items[504].count, 504);
    }

    app.world_mut()
        .write_message(regia_ui::UiEvent::new(slider, regia_ui::UiEventKind::Click));
    app.update();
    let sent = &app.world().resource::<SeenCommands>().0;
    let picks: Vec<_> = sent.iter().filter(|c| c.name == "feed.pick").collect();
    assert_eq!(picks.len(), 1, "{sent:?}");
    assert_eq!(picks[0].item.as_deref(), Some("503"));
}

/// Spec 81 § 2.8 "Refresh": a `ListHeights` change alone re-windows the rows. The first 100 items
/// become 20 px tall, so offset 4000 now falls on item 100 + 2000 / 40 = 150; a later edit of the
/// heights moves the window again.
#[test]
fn the_window_follows_a_list_heights_change() {
    let mut app = app(10_000);
    let list = feed(&mut app);
    scroll_to(&mut app, list, 4000.0);
    assert_eq!(host(&app, list).anchor_key(), Some("100"));

    app.world_mut()
        .entity_mut(list)
        .insert(regia_ui::ListHeights(vec![20.0; 100]));
    app.update();
    app.update();
    {
        let h = host(&app, list);
        assert_eq!(
            h.anchor_key(),
            Some("150"),
            "2000 px of short rows, then 50 rows of 40"
        );
        assert_eq!(h.first_index(), 150 - OVERSCAN);
    }
    // Item 150 starts exactly at the offset, so its row sits at the list's top, one 40 px row tall
    // (within the one device pixel that layout snapping moves an edge at this scale).
    let rect = app
        .world()
        .get::<ComputedNode>(row_of(&app, list, 150))
        .expect("rect")
        .rect;
    let list_rect = app.world().get::<ComputedNode>(list).expect("list").rect;
    assert!((rect[1] - list_rect[1]).abs() <= 1.0, "{rect:?} vs {list_rect:?}");
    assert!((rect[3] - ROW * scale(&app, list)).abs() <= 1.0, "{rect:?}");

    // Editing the heights in place (no insert) is a change too: 200 short rows reach 4000 exactly.
    app.world_mut()
        .get_mut::<regia_ui::ListHeights>(list)
        .expect("heights")
        .0 = vec![20.0; 200];
    app.update();
    app.update();
    let h = host(&app, list);
    assert_eq!(h.anchor_key(), Some("200"));
    assert_eq!(h.first_index(), 200 - OVERSCAN);
}

/// `.ui.ron` v11 fold (ADR-7751): `row_height: template` virtualizes at the template's fixed
/// height (100 UI px here) instead of the list's `row_height` (40): the rows are placed 100 tall,
/// the window holds the rows a 400 px view shows at that height plus the overscan, and a scroll
/// to 4000 px makes item 40 the first visible. The same list without the option keeps 40.
#[test]
fn a_repeat_with_template_row_height_virtualizes_at_the_template_height() {
    const TEMPLATE: &str = r#"(version: 11, root: (
        style: (direction: column),
        children: [
            (name: "feed", widget: list(0, 40.0), style: (size: (300, 400)), clip: true,
                repeat: (source: "FeedVm.items", key: "id", row_height: template),
                children: [
                    (name: "row", style: (direction: row, size: (Percent(100), 100)), children: [
                        (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
                    ]),
                ]),
        ],
    ))"#;
    let mut app = app_with(TEMPLATE, 10_000);
    let list = feed(&mut app);
    assert_eq!(
        app.world().get::<ListView>(list).expect("list").item_height,
        100.0
    );
    let bound = (VIEWPORT / 100.0).ceil() as usize + 1 + 2 * OVERSCAN;
    {
        let h = host(&app, list);
        assert_eq!(
            h.row_height_mode(),
            regia_ui::authoring::RepeatRowHeight::Template
        );
        assert!(h.instance_count() > 0);
        assert!(
            h.instance_count() <= bound,
            "{} rows > {bound}",
            h.instance_count()
        );
    }
    let row = row_of(&app, list, 1);
    let rect = app.world().get::<ComputedNode>(row).expect("rect").rect;
    // Layout snaps rects to whole pixels: 100 UI px at scale 2/3 lays out as 66 or 67 px.
    assert!(
        (rect[3] - 100.0 * scale(&app, list)).abs() < 1.0,
        "rows take the template height: {rect:?}"
    );
    scroll_to(&mut app, list, 4000.0);
    let h = host(&app, list);
    assert_eq!(h.anchor_key(), Some("40"), "4000 / 100 = item 40");
    assert!(h.instance_count() <= bound);
    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    assert_eq!(state["hosts"][0]["row_height"], "template", "{state}");
    assert_eq!(state["hosts"][0]["row_px"], 100.0, "{state}");

    // Control: the same template under the default keeps the list's 40 px rows.
    let plain = TEMPLATE.replace(", row_height: template", "");
    let mut app = app_with(&plain, 10_000);
    let list = feed(&mut app);
    assert_eq!(app.world().get::<ListView>(list).expect("list").item_height, 40.0);
}
