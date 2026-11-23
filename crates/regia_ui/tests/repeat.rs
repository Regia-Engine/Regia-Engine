// SPDX-License-Identifier: MPL-2.0
//! Collection item templates (ADR-2489, `docs/specs/81-ui-designer.md` § 2.7): a `.ui.ron` v6
//! `repeat` over a reflected `Vec`, item-relative bindings, keyed diff with stable rows, nested
//! repeats, converters through real bindings, two-way writes into items, commands that name their
//! item, and `ui.collection_state`. Headless: no window, no GPU, fixed 100 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::agent_bridge::call;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::{
    Localization, NodeName, RepeatHost, UiCommand, UiMigrators, ViewBindings, ViewModelAppExt, ViewModels,
    parse_ui_ron,
};
use serde_json::json;

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
enum Rarity {
    #[default]
    Common,
    Epic,
}

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
struct Item {
    id: u32,
    name: String,
    count: u32,
    fill: f32,
    rarity: Rarity,
}

fn item(id: u32, name: &str) -> Item {
    Item {
        id,
        name: name.to_owned(),
        count: id,
        fill: 0.5,
        rarity: Rarity::Common,
    }
}

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
struct Section {
    title: String,
    items: Vec<Item>,
}

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct InvVm {
    items: Vec<Item>,
    words: Vec<String>,
    fixed: [u32; 3],
    sections: Vec<Section>,
    not_a_list: u32,
}

const ROWS: &str = r#"(version: 6, root: (
    style: (direction: column),
    children: [
        (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
            (name: "row", classes: ["row"], widget: toggle(false), command: "inv.pick",
                bindings: [(source: ".id", target: selected)],
                children: [
                    (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
                    (name: "qty", text: (spans: ["0"]), bindings: [(source: ".count", format: "x{:03}")]),
                    (name: "note", text: (spans: ["fixed text"])),
                ]),
        ]),
    ],
))"#;

fn app(layout: &str) -> App {
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
    app.register_view_model::<InvVm>();
    app.update();
    let file = parse_ui_ron(layout, "t.ui.ron", &UiMigrators::default()).expect("layout");
    let world = app.world_mut();
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert("t".to_owned(), file);
    spawn_ui(world, "t").expect("spawn");
    app
}

fn named(app: &mut App, name: &str) -> Vec<Entity> {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    let mut out: Vec<Entity> = q
        .iter(world)
        .filter(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .collect();
    out.sort_by_key(|e| e.to_bits());
    out
}

fn host_entity(app: &mut App, name: &str) -> Entity {
    named(app, name)[0]
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

/// The instance roots in the host's child order.
fn row_order(app: &App, entity: Entity) -> Vec<Entity> {
    app.world()
        .get::<Children>(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default()
}

/// Named descendant of an instance root.
fn child_named(app: &App, root: Entity, name: &str) -> Entity {
    let children = app.world().get::<Children>(root).expect("children");
    children
        .iter()
        .find(|e| app.world().get::<NodeName>(*e).is_some_and(|n| n.0 == name))
        .expect("child")
}

/// A list of 1,000 items instantiates 1,000 rows, each bound to its own item.
#[test]
fn a_thousand_items_make_a_thousand_rows() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items =
        (0..1000).map(|i| item(i, &format!("item {i}"))).collect();
    app.update();
    assert_eq!(named(&mut app, "row").len(), 1000);
    assert_eq!(named(&mut app, "label").len(), 1000);
    let rows = host_entity(&mut app, "rows");
    assert_eq!(host(&app, rows).instance_count(), 1000);
    assert_eq!(host(&app, rows).stats().spawned, 1000);
    assert_eq!(
        host(&app, rows).stats().touched,
        0,
        "new rows are not touched rows"
    );
    for index in [0usize, 1, 500, 999] {
        let root = host(&app, rows).roots(index)[0];
        assert_eq!(
            text(&app, child_named(&app, root, "label")),
            format!("item {index}")
        );
        assert_eq!(text(&app, child_named(&app, root, "qty")), format!("x{index:03}"));
    }
    // Stable after a frame with no change: no new pass.
    let passes = host(&app, rows).stats().passes;
    app.update();
    assert_eq!(host(&app, rows).stats().passes, passes);
}

/// Changing one item writes one row: only its widgets change, and every other row is left alone
/// (their change ticks do not move).
#[test]
fn changing_one_item_touches_one_row() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = (0..50).map(|i| item(i, &format!("item {i}"))).collect();
    app.update();
    let rows = host_entity(&mut app, "rows");
    let labels: Vec<Entity> = (0..50)
        .map(|i| child_named(&app, host(&app, rows).roots(i)[0], "label"))
        .collect();
    let before: Vec<Entity> = row_order(&app, rows);
    app.update();

    app.world_mut().resource_mut::<InvVm>().items[7].name = "renamed".to_owned();
    let tick = app.world().read_change_tick();
    app.update();

    let stats = host(&app, rows).stats();
    assert_eq!(
        (stats.spawned, stats.despawned, stats.moved, stats.touched),
        (0, 0, false, 1),
        "{stats:?}"
    );
    assert_eq!(row_order(&app, rows), before, "same entities, same order");
    assert_eq!(text(&app, labels[7]), "renamed");
    let this_run = app.world().read_change_tick();
    for (i, label) in labels.iter().enumerate() {
        let ticks = app
            .world()
            .get_entity(*label)
            .expect("label")
            .get_change_ticks::<Text>()
            .expect("ticks");
        assert_eq!(ticks.is_changed(tick, this_run), i == 7, "row {i}");
    }
}

/// Removing an item in the middle despawns one row and keeps the others' entities and any state
/// on them, with no writes to their widgets.
#[test]
fn removing_a_middle_item_keeps_the_other_rows_state() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = (0..5).map(|i| item(i, &format!("item {i}"))).collect();
    app.update();
    let rows = host_entity(&mut app, "rows");
    let before: Vec<Entity> = (0..5).map(|i| host(&app, rows).roots(i)[0]).collect();
    // State the model knows nothing about: the row's toggle and a marker on the row.
    #[derive(Component)]
    struct Mark(u32);
    for (i, row) in before.iter().enumerate() {
        let mut entity = app.world_mut().entity_mut(*row);
        entity.insert(Mark(i as u32));
        entity.get_mut::<Toggle>().expect("toggle").checked = i % 2 == 0;
    }

    app.world_mut().resource_mut::<InvVm>().items.remove(2);
    app.update();

    let stats = host(&app, rows).stats();
    assert_eq!(
        (stats.spawned, stats.despawned, stats.moved, stats.touched),
        (0, 1, false, 0),
        "{stats:?}"
    );
    assert!(app.world().get_entity(before[2]).is_err(), "row 2 is gone");
    assert_eq!(
        row_order(&app, rows),
        [before[0], before[1], before[3], before[4]]
    );
    for (index, old) in [(0, 0usize), (1, 1), (2, 3), (3, 4)] {
        let row = host(&app, rows).roots(index)[0];
        assert_eq!(row, before[old]);
        assert_eq!(app.world().get::<Mark>(row).expect("mark").0, old as u32);
        assert_eq!(
            app.world().get::<Toggle>(row).expect("toggle").checked,
            old % 2 == 0,
            "row {old}"
        );
    }
}

#[test]
fn reordering_moves_rows_without_rebuilding_them() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a"), item(2, "b"), item(3, "c")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let before: Vec<Entity> = row_order(&app, rows);

    app.world_mut().resource_mut::<InvVm>().items.reverse();
    app.update();

    let stats = host(&app, rows).stats();
    assert!(stats.moved);
    assert_eq!(
        (stats.spawned, stats.despawned, stats.touched),
        (0, 0, 0),
        "{stats:?}"
    );
    assert_eq!(row_order(&app, rows), [before[2], before[1], before[0]]);
    assert_eq!(host(&app, rows).keys().collect::<Vec<_>>(), ["3", "2", "1"]);
    assert_eq!(host(&app, rows).index_of("1"), Some(2));
}

#[test]
fn inserting_in_front_and_appending_land_in_collection_order() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a"), item(2, "b")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let before = row_order(&app, rows);

    app.world_mut().resource_mut::<InvVm>().items.push(item(3, "c"));
    app.update();
    let stats = host(&app, rows).stats();
    assert_eq!((stats.spawned, stats.moved), (1, false));
    let after_append = row_order(&app, rows);
    assert_eq!(&after_append[..2], &before[..]);

    app.world_mut()
        .resource_mut::<InvVm>()
        .items
        .insert(0, item(9, "first"));
    app.update();
    let stats = host(&app, rows).stats();
    assert_eq!((stats.spawned, stats.moved), (1, true));
    let order = row_order(&app, rows);
    assert_eq!(&order[1..], &after_append[..]);
    assert_eq!(text(&app, child_named(&app, order[0], "label")), "first");
}

/// Replacing an item's key spawns a new row; keeping the key with new fields keeps the row.
#[test]
fn a_new_key_is_a_new_row() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let first = row_order(&app, rows)[0];
    app.world_mut().resource_mut::<InvVm>().items[0] = item(2, "other");
    app.update();
    let second = row_order(&app, rows)[0];
    assert_ne!(first, second);
    assert!(app.world().get_entity(first).is_err());
    let stats = host(&app, rows).stats();
    assert_eq!((stats.spawned, stats.despawned), (1, 1));
}

/// Without `key`, items follow by position; `key: "."` keys a list of plain values by the value.
#[test]
fn position_keys_and_value_keys() {
    let layout = r#"(version: 6, root: (children: [
        (name: "by_index", repeat: (source: "InvVm.words"), children: [(name: "a", text: (spans: ["-"]), bindings: [(source: ".")])]),
        (name: "by_value", repeat: (source: "InvVm.words", key: "."), children: [(name: "b", text: (spans: ["-"]), bindings: [(source: ".")])]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().words = ["x", "y", "z"].map(str::to_owned).to_vec();
    app.update();
    let (by_index, by_value) = (
        host_entity(&mut app, "by_index"),
        host_entity(&mut app, "by_value"),
    );
    assert_eq!(host(&app, by_index).keys().collect::<Vec<_>>(), ["0", "1", "2"]);
    assert_eq!(host(&app, by_value).keys().collect::<Vec<_>>(), ["x", "y", "z"]);

    app.world_mut().resource_mut::<InvVm>().words.remove(0);
    app.update();
    // By position the tail goes and the others rewrite; by value one row goes and nothing rewrites.
    let stats = host(&app, by_index).stats();
    assert_eq!((stats.despawned, stats.touched), (1, 2), "{stats:?}");
    let stats = host(&app, by_value).stats();
    assert_eq!((stats.despawned, stats.touched), (1, 0), "{stats:?}");
}

#[test]
fn duplicate_keys_get_their_own_rows_and_report() {
    let layout = r#"(version: 6, root: (children: [
        (name: "rows", repeat: (source: "InvVm.words", key: "."), children: [(text: (spans: ["-"]), bindings: [(source: ".")])]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().words = ["a", "a", "b"].map(str::to_owned).to_vec();
    app.update();
    let rows = host_entity(&mut app, "rows");
    assert_eq!(host(&app, rows).instance_count(), 3);
    assert_eq!(host(&app, rows).keys().collect::<Vec<_>>(), ["a", "a#2", "b"]);
    let error = host(&app, rows).last_error().expect("reported");
    assert!(error.starts_with("repeat_duplicate_key"), "{error}");
}

/// Arrays repeat too, and a template of several top nodes spawns all of them per item.
#[test]
fn arrays_and_multi_node_templates() {
    let layout = r#"(version: 6, root: (children: [
        (name: "rows", repeat: (source: "InvVm.fixed"), children: [
            (name: "a", text: (spans: ["-"]), bindings: [(source: ".")]),
            (name: "b", text: (spans: ["-"])),
        ]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().fixed = [4, 5, 6];
    app.update();
    assert_eq!(named(&mut app, "a").len(), 3);
    assert_eq!(named(&mut app, "b").len(), 3);
    let rows = host_entity(&mut app, "rows");
    assert_eq!(host(&app, rows).roots(1).len(), 2);
    assert_eq!(text(&app, host(&app, rows).roots(2)[0]), "6");
}

/// A repeat inside a repeat's template follows its own part of the model.
#[test]
fn nested_repeats_read_relative_to_the_parent_item() {
    let layout = r#"(version: 6, root: (children: [
        (name: "sections", repeat: (source: "InvVm.sections", key: "title"), children: [
            (name: "section", children: [
                (name: "title", text: (spans: ["-"]), bindings: [(source: ".title")]),
                (name: "inner", repeat: (source: ".items", key: "id"), children: [
                    (name: "cell", text: (spans: ["-"]), bindings: [(source: ".name")]),
                ]),
            ]),
        ]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().sections = vec![
        Section {
            title: "A".to_owned(),
            items: vec![item(1, "a1"), item(2, "a2")],
        },
        Section {
            title: "B".to_owned(),
            items: vec![item(3, "b1")],
        },
    ];
    app.update();
    assert_eq!(named(&mut app, "section").len(), 2);
    assert_eq!(named(&mut app, "cell").len(), 3);
    assert_eq!(named(&mut app, "inner").len(), 2);
    let sections = host_entity(&mut app, "sections");
    let inner_a = child_named(&app, host(&app, sections).roots(0)[0], "inner");
    let inner_b = child_named(&app, host(&app, sections).roots(1)[0], "inner");
    let cell = host(&app, inner_a).roots(1)[0];
    assert_eq!(text(&app, cell), "a2");

    // Removing the first section keeps section B's inner host and rows; its index shifts.
    let cell_b = host(&app, inner_b).roots(0)[0];
    app.world_mut().resource_mut::<InvVm>().sections.remove(0);
    app.update();
    assert_eq!(named(&mut app, "section").len(), 1);
    assert_eq!(host(&app, inner_b).roots(0)[0], cell_b);
    assert_eq!(text(&app, cell_b), "b1");
    assert_eq!(
        app.world().get::<RepeatHost>(inner_a).map(|_| ()),
        None,
        "the removed section's inner host is gone"
    );

    // An inner edit reaches the right row through the shifted index.
    app.world_mut().resource_mut::<InvVm>().sections[0].items[0].name = "edited".to_owned();
    app.update();
    assert_eq!(text(&app, cell_b), "edited");
    assert_eq!(host(&app, inner_b).stats().touched, 1);

    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    assert_eq!(state["hosts"].as_array().map(Vec::len), Some(2), "{state}");
}

#[test]
fn bad_sources_report_codes_and_recover() {
    let layout = r#"(version: 6, root: (children: [
        (name: "missing", repeat: (source: "Nope.items"), children: [(name: "m")]),
        (name: "scalar", repeat: (source: "InvVm.not_a_list"), children: [(name: "s")]),
        (name: "orphan", repeat: (source: ".items"), children: [(name: "o")]),
        (name: "badkey", repeat: (source: "InvVm.items", key: "nope"), children: [(name: "k")]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a")];
    app.update();
    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    let codes: Vec<&str> = state["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .map(|h| h["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        [
            "repeat_unresolved",
            "repeat_not_a_list",
            "repeat_outside_template",
            "repeat_bad_key"
        ]
    );
    assert_eq!(state["errors"], 4);
    assert_eq!(state["instances"], 0);
    assert!(named(&mut app, "k").is_empty(), "a bad key spawns nothing");
}

/// Spec 81 § 2.7: a host in error keeps its instances, passes again only when its model's change
/// tick moves (§ 9: one tick check per frame while the model is unchanged, also in error), and
/// recovers on the next real change.
#[test]
fn a_host_in_error_keeps_its_instances_and_waits_for_a_model_change() {
    let layout = r#"(version: 6, root: (children: [
        (name: "secs", repeat: (source: "InvVm.sections", key: "items.0.id"), children: [(name: "sec")]),
    ]))"#;
    let section = |items: Vec<Item>| Section {
        title: "s".to_owned(),
        items,
    };
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().sections = vec![section(vec![item(7, "a")])];
    app.update();
    let secs = host_entity(&mut app, "secs");
    assert_eq!(host(&app, secs).instance_count(), 1);
    assert_eq!(host(&app, secs).last_error(), None);

    // The key path no longer resolves: the host reports and keeps what it had.
    app.world_mut().resource_mut::<InvVm>().sections[0].items.clear();
    app.update();
    let error = host(&app, secs).last_error().unwrap_or_default().to_owned();
    assert!(error.starts_with("repeat_bad_key"), "{error}");
    assert_eq!(
        host(&app, secs).instance_count(),
        1,
        "a host in error keeps its instances"
    );
    assert_eq!(named(&mut app, "sec").len(), 1);

    // A write that moves no change tick is not a model change: the host stays as it is instead of
    // reading the collection again every frame.
    app.world_mut()
        .resource_mut::<InvVm>()
        .bypass_change_detection()
        .sections = vec![section(vec![item(1, "x")]), section(vec![item(2, "y")])];
    app.update();
    app.update();
    assert!(
        host(&app, secs)
            .last_error()
            .is_some_and(|e| e.starts_with("repeat_bad_key")),
        "an unchanged model is not read again"
    );
    assert_eq!(host(&app, secs).instance_count(), 1);

    // A real change: the host passes, recovers and clears its error.
    app.world_mut().resource_mut::<InvVm>().set_changed();
    app.update();
    assert_eq!(host(&app, secs).last_error(), None);
    assert_eq!(host(&app, secs).keys().collect::<Vec<_>>(), ["1", "2"]);
    assert_eq!(named(&mut app, "sec").len(), 2);
}

/// Spec 81 § 2.7: `spawn_ui` ignores a `repeat` on the root, like the root's other fields: the
/// root's children spawn once and no host is made.
#[test]
fn a_repeat_on_the_root_is_ignored() {
    let layout = r#"(version: 6, root: (repeat: (source: "InvVm.items", key: "id"), children: [
        (name: "x", text: (spans: ["once"])),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a"), item(2, "b"), item(3, "c")];
    app.update();
    app.update();
    assert_eq!(named(&mut app, "x").len(), 1, "the root's children spawn once");
    let world = app.world_mut();
    assert_eq!(world.query::<&RepeatHost>().iter(world).count(), 0);
    assert!(
        world.get_resource::<regia_ui::repeat::RepeatHosts>().is_none(),
        "no repeat node, so no host registry"
    );
}

#[test]
fn item_binding_outside_a_repeat_reports() {
    let layout = r#"(version: 6, root: (children: [
        (name: "lost", text: (spans: ["kept"]), bindings: [(source: ".name")]),
    ]))"#;
    let mut app = app(layout);
    app.update();
    let lost = named(&mut app, "lost")[0];
    assert_eq!(text(&app, lost), "kept");
    let list = app.world().get::<ViewBindings>(lost).expect("bindings");
    let error = list.0[0].last_error().expect("error");
    assert!(error.starts_with("binding_item_outside_repeat"), "{error}");
}

#[test]
fn a_bad_item_field_reports_on_the_binding_and_keeps_the_row() {
    let layout = r#"(version: 6, root: (children: [
        (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
            (name: "row", text: (spans: ["-"]), bindings: [(source: ".nope"), (source: ".name", target: opacity)]),
        ]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a")];
    app.update();
    let row = named(&mut app, "row")[0];
    let list = app.world().get::<ViewBindings>(row).expect("bindings");
    let error = list.0[0].last_error().expect("error");
    assert!(error.starts_with("binding_unresolved"), "{error}");
    assert!(list.0[1].last_error().is_none());
}

/// Two-way bindings on a row write the row's own item, also after the item moved.
#[test]
fn two_way_item_bindings_write_the_right_item() {
    let layout = r#"(version: 6, root: (children: [
        (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
            (name: "row", widget: slider(0.0, 0.0, 1.0, 0.0), bindings: [(source: ".fill", target: value, mode: two_way)]),
        ]),
    ]))"#;
    let mut app = app(layout);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a"), item(2, "b"), item(3, "c")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let row_b = host(&app, rows).roots(1)[0];
    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": row_b.to_bits(), "value": 0.25 }),
    )
    .expect("set");
    app.update();
    app.update();
    let vm = app.world().resource::<InvVm>().clone();
    assert_eq!(vm.items[1].fill, 0.25);
    assert_eq!(vm.items[0].fill, 0.5);

    // The item moves to the front: the same row now writes index 0.
    app.world_mut().resource_mut::<InvVm>().items.swap(0, 1);
    app.update();
    assert_eq!(host(&app, rows).roots(0)[0], row_b, "the row follows its key");
    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": row_b.to_bits(), "value": 0.75 }),
    )
    .expect("set");
    app.update();
    app.update();
    let vm = app.world().resource::<InvVm>().clone();
    assert_eq!((vm.items[0].id, vm.items[0].fill), (2, 0.75));
}

/// A command from a generated row names its item.
#[test]
fn commands_carry_the_item_key() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(11, "a"), item(22, "b")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let row = host(&app, rows).roots(1)[0];
    assert_eq!(regia_ui::repeat::item_key_of(app.world(), row), Some("22"));
    app.init_resource::<SeenCommands>()
        .add_systems(PostUpdate, record_commands);
    app.world_mut()
        .write_message(regia_ui::UiEvent::new(row, regia_ui::UiEventKind::Click));
    app.update();
    let sent = &app.world().resource::<SeenCommands>().0;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name, "inv.pick");
    assert_eq!(sent[0].item.as_deref(), Some("22"));
}

#[derive(Resource, Default)]
struct SeenCommands(Vec<UiCommand>);

fn record_commands(mut events: MessageReader<UiCommand>, mut seen: ResMut<SeenCommands>) {
    seen.0.extend(events.read().cloned());
}

/// A node with a resource binding in a template still refreshes as an ordinary binding.
#[test]
fn resource_bindings_inside_templates_follow_the_resource() {
    let layout = r#"(version: 6, root: (children: [
        (name: "rows", repeat: (source: "InvVm.items", key: "id"), children: [
            (name: "row", text: (spans: ["-"]), bindings: [(source: "InvVm.not_a_list")]),
        ]),
    ]))"#;
    let mut app = app(layout);
    {
        let mut vm = app.world_mut().resource_mut::<InvVm>();
        vm.items = vec![item(1, "a"), item(2, "b")];
        vm.not_a_list = 7;
    }
    app.update();
    for row in named(&mut app, "row") {
        assert_eq!(text(&app, row), "7");
    }
    let status = ViewModels::status(app.world_mut());
    assert_eq!(status.len(), 2);
}

#[test]
fn converters_run_through_real_bindings() {
    let layout = r#"(version: 6, root: (children: [
        (name: "n", text: (spans: ["-"]), bindings: [(source: "ConvVm.score", converter: number(precision: 2, group: ","))]),
        (name: "c", text: (spans: ["-"]), bindings: [(source: "ConvVm.score", converter: clamp(min: 0, max: 100))]),
        (name: "m", widget: progress(0.0), bindings: [(source: "ConvVm.hp", target: progress, converter: map_range(from: (0, 200), to: (0, 1), clamp: true))]),
        (name: "v", bindings: [(source: "ConvVm.lives", target: visible, converter: bool_to_visibility)]),
        (name: "hid", bindings: [(source: "ConvVm.lives", target: visible, converter: bool_to_visibility, invert: true)]),
        (name: "e", text: (spans: ["-"]), bindings: [(source: "ConvVm.state", converter: enum_text(table: [("Common", "Plain"), ("Epic", "Great")], default: "?"))]),
        (name: "l", text: (spans: ["-"]), bindings: [(source: "ConvVm.state", converter: loc_key(prefix: "rarity-"))]),
        (name: "f", text: (spans: ["-"]), bindings: [(source: "ConvVm.hp", format: "{:.1} hp", converter: map_range(from: (0, 200), to: (0, 100)))]),
    ]))"#;
    #[derive(Resource, Reflect, Debug, Default)]
    #[reflect(Resource, Default)]
    struct ConvVm {
        score: f64,
        hp: f32,
        lives: u32,
        state: Rarity,
    }
    let mut app = app(layout);
    app.register_view_model::<ConvVm>();
    app.world_mut().insert_resource(
        Localization::new(
            vec![(
                "en".to_owned(),
                "rarity-Common = Common item\nrarity-Epic = Epic item".to_owned(),
            )],
            None,
        )
        .expect("loc"),
    );
    {
        let mut vm = app.world_mut().resource_mut::<ConvVm>();
        vm.score = 1234.5;
        vm.hp = 150.0;
        vm.lives = 2;
    }
    app.update();
    let node = |app: &mut App, n: &str| named(app, n)[0];
    let e = node(&mut app, "n");
    assert_eq!(text(&app, e), "1,234.50");
    let e = node(&mut app, "c");
    assert_eq!(text(&app, e), "100");
    let e = node(&mut app, "m");
    assert_eq!(app.world().get::<ProgressBar>(e).expect("bar").value, 0.75);
    let display = |app: &mut App, n: &str| {
        let e = named(app, n)[0];
        app.world()
            .get::<regia_ui::NodeStyle>(e)
            .expect("style")
            .0
            .display
    };
    assert_ne!(display(&mut app, "v"), regia_ui::Display::None);
    assert_eq!(display(&mut app, "hid"), regia_ui::Display::None);
    let e = node(&mut app, "e");
    assert_eq!(text(&app, e), "Plain");
    let e = node(&mut app, "l");
    assert_eq!(text(&app, e), "loc:rarity-Common", "the span holds the key");
    let resolved = {
        let world = app.world();
        let loc = world.resource::<Localization>();
        world.get::<Text>(e).expect("text").spans[0].content.resolve(loc)
    };
    assert_eq!(resolved, "Common item", "through the Fluent path");
    let e = node(&mut app, "f");
    assert_eq!(text(&app, e), "75.0 hp", "the converter runs before the format");

    {
        let mut vm = app.world_mut().resource_mut::<ConvVm>();
        vm.state = Rarity::Epic;
        vm.lives = 0;
    }
    app.update();
    let e = node(&mut app, "e");
    assert_eq!(text(&app, e), "Great");
    let e = node(&mut app, "l");
    assert_eq!(text(&app, e), "loc:rarity-Epic");
    assert_eq!(display(&mut app, "v"), regia_ui::Display::None);
    assert_ne!(display(&mut app, "hid"), regia_ui::Display::None);
}

/// A two-way binding writes back through `map_range`; a one-way converter reports and writes nothing.
#[test]
fn two_way_converters_invert_or_report() {
    let layout = r#"(version: 6, root: (children: [
        (name: "s", widget: slider(0.0, 0.0, 1.0, 0.0), bindings: [(source: "RangeVm.percent", target: value, mode: two_way, converter: map_range(from: (0, 100), to: (0, 1)))]),
        (name: "o", widget: slider(0.0, 0.0, 1.0, 0.0), bindings: [(source: "RangeVm.other", target: value, mode: two_way, converter: bool_to_visibility)]),
    ]))"#;
    #[derive(Resource, Reflect, Debug, Default)]
    #[reflect(Resource, Default)]
    struct RangeVm {
        percent: f32,
        other: f32,
    }
    let mut app = app(layout);
    app.register_view_model::<RangeVm>();
    app.world_mut().resource_mut::<RangeVm>().percent = 50.0;
    app.update();
    let s = named(&mut app, "s")[0];
    assert_eq!(app.world().get::<Slider>(s).expect("slider").value, 0.5);
    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": s.to_bits(), "value": 0.25 }),
    )
    .expect("set");
    app.update();
    app.update();
    assert_eq!(app.world().resource::<RangeVm>().percent, 25.0);
    assert_eq!(
        app.world().get::<Slider>(s).expect("slider").value,
        0.25,
        "no echo"
    );

    let o = named(&mut app, "o")[0];
    call(
        app.world_mut(),
        "ui.set_value",
        &json!({ "node": o.to_bits(), "value": 0.75 }),
    )
    .expect("set");
    app.update();
    app.update();
    assert_eq!(app.world().resource::<RangeVm>().other, 0.0, "nothing written");
    let list = app.world().get::<ViewBindings>(o).expect("bindings");
    let error = list.0[0].last_error().expect("error");
    assert!(error.starts_with("binding_converter_one_way"), "{error}");
}

/// The agent surface reports instance counts per repeat node, and keys on request.
#[test]
fn collection_state_reports_each_host() {
    let mut app = app(ROWS);
    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    assert_eq!(state["instances"], 0);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a"), item(2, "b")];
    app.update();
    let state = call(app.world_mut(), "ui.collection_state", &json!({ "keys": true })).expect("state");
    assert_eq!(state["instances"], 2);
    assert_eq!(state["errors"], 0);
    let row = &state["hosts"][0];
    assert_eq!(row["name"], "rows");
    assert_eq!(row["source"], "InvVm.items");
    assert_eq!(row["key"], "id");
    assert_eq!(row["instances"], 2);
    assert_eq!(row["spawned"], 2);
    assert_eq!(row["keys"], json!(["1", "2"]));
    assert!(row["code"].is_null());
    let bare = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    assert!(bare["hosts"][0].get("keys").is_none());
}

/// Hosts leave the registry with their root (a live reload or a closed screen), and a world
/// without any repeat node has no registry at all.
#[test]
fn a_despawned_root_drops_its_hosts() {
    let mut app = app(ROWS);
    app.world_mut().resource_mut::<InvVm>().items = vec![item(1, "a")];
    app.update();
    let rows = host_entity(&mut app, "rows");
    let root = app.world().get::<ChildOf>(rows).expect("child").parent();
    app.world_mut().despawn(root);
    app.update();
    let state = call(app.world_mut(), "ui.collection_state", &json!({})).expect("state");
    assert_eq!(state["hosts"].as_array().map(Vec::len), Some(0));

    let plain = self::app(r#"(version: 6, root: (children: [(name: "a")]))"#);
    assert!(
        plain
            .world()
            .get_resource::<regia_ui::repeat::RepeatHosts>()
            .is_none(),
        "no collections, no registry"
    );
}

/// The designer's schema: a list field is `list` and names its item's fields, and a recursive item
/// type does not recurse forever.
#[test]
fn view_model_schema_lists_item_fields() {
    #[derive(Reflect, Debug, Default, Clone)]
    #[reflect(Default)]
    struct Tree {
        children: Vec<Tree>,
    }
    #[derive(Resource, Reflect, Debug, Default)]
    #[reflect(Resource, Default)]
    struct TreeVm {
        root: Vec<Tree>,
    }
    let mut app = app(r#"(version: 6, root: ())"#);
    app.register_view_model::<TreeVm>();
    let models = app.world().resource::<ViewModels>();
    let inv = models.get("InvVm").expect("model");
    let items = inv.field("items").expect("items");
    assert_eq!(items.kind, "list");
    let names: Vec<&str> = items.item_fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "count", "fill", "rarity"]);
    assert_eq!(inv.field("words").expect("words").kind, "list");
    assert_eq!(inv.field("not_a_list").expect("scalar").kind, "integer");
    let sections = inv.field("sections").expect("sections");
    let nested = sections
        .item_fields
        .iter()
        .find(|f| f.name == "items")
        .expect("nested");
    assert_eq!(nested.item_fields.len(), 5);
    assert!(models.get("TreeVm").is_some());
}
