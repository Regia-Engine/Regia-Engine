// SPDX-License-Identifier: MPL-2.0
//! The Outliner's hierarchy edits end to end (`specs/19` § 2, ADR-1330): the real panel driven
//! by synthetic egui input (click, Ctrl-click, Shift-click, drag a multi-selection onto a row,
//! drop on the root zone, a refused cyclic drop, hotkeys), and the agent methods that make the
//! same edits (`editor.hierarchy`, `editor.reparent`, `editor.group`, `editor.duplicate`,
//! `editor.delete`, `editor.select_hierarchy`). Every edit must be one undo entry.

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, vec2};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::hierarchy_ops::world_affine;
use regia_editor::outliner::{OutlinerRows, outliner_panel};
use regia_editor::selection::Selection;
use regia_scene::prelude::{EntityGuid, Name, Transform};
use serde_json::{Value, json};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    app
}

fn spawn(world: &mut World, name: &str, translation: Vec3) -> Entity {
    world
        .spawn((
            Name::new(name.to_string()),
            Transform::from_translation(translation),
            EntityGuid::new_random(),
        ))
        .id()
}

fn by_name(world: &World, name: &str) -> Entity {
    world
        .iter_entities()
        .map(|e| e.id())
        .find(|e| world.get::<Name>(*e).is_some_and(|n| n.as_str() == name))
        .unwrap_or_else(|| panic!("no entity {name:?}"))
}

fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<ChildOf>(entity).map(ChildOf::parent)
}

fn depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

fn world_pos(world: &World, entity: Entity) -> Vec3 {
    Vec3::from(world_affine(world, entity).translation)
}

/// Feeds frames of synthetic input to the Outliner panel.
struct Harness {
    ctx: egui::Context,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            time: 0.0,
        }
    }

    fn frame_with(&mut self, world: &mut World, events: Vec<Event>, modifiers: Modifiers) {
        // As the shell does every frame: resources are entities in bevy_ecs 0.20, and new ones
        // (the selection, the row record) must stay out of the tree.
        regia_editor::visibility::mark_internal_entities(world);
        self.time += 0.05;
        // egui 0.36 keeps the modifier state across frames and changes it only on this event.
        let mut all = vec![Event::ModifiersChanged(modifiers)];
        all.extend(events);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(360.0, 480.0))),
            events: all,
            time: Some(self.time),
            ..Default::default()
        };
        let mut output = self.ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| outliner_panel(ui, world));
        });
        output.textures_delta.clear();
    }

    fn frame(&mut self, world: &mut World, events: Vec<Event>) {
        self.frame_with(world, events, Modifiers::NONE);
    }

    /// Settles the layout (rows are recorded on every draw).
    fn settle(&mut self, world: &mut World) {
        self.frame(world, vec![]);
        self.frame(world, vec![]);
    }

    fn click(&mut self, world: &mut World, at: Pos2, modifiers: Modifiers) {
        self.frame_with(world, vec![Event::PointerMoved(at)], modifiers);
        self.frame_with(world, vec![button(at, true, modifiers)], modifiers);
        self.frame_with(world, vec![button(at, false, modifiers)], modifiers);
        self.frame(world, vec![]);
    }

    /// Presses at `from`, moves to `to` in steps (well past egui's drag threshold) and releases.
    fn drag(&mut self, world: &mut World, from: Pos2, to: Pos2) {
        self.frame(world, vec![Event::PointerMoved(from)]);
        self.frame(world, vec![button(from, true, Modifiers::NONE)]);
        for step in 1..=6 {
            let t = step as f32 / 6.0;
            self.frame(world, vec![Event::PointerMoved(from + (to - from) * t)]);
        }
        self.frame(world, vec![button(to, false, Modifiers::NONE)]);
        self.settle(world);
    }

    fn key(&mut self, world: &mut World, at: Pos2, key: Key, modifiers: Modifiers) {
        self.frame_with(world, vec![Event::PointerMoved(at)], modifiers);
        self.frame_with(
            world,
            vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            modifiers,
        );
        self.settle(world);
    }
}

fn button(pos: Pos2, pressed: bool, modifiers: Modifiers) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers,
    }
}

/// Ctrl as the platform reports it on Linux/Windows (`command` mirrors `ctrl`).
const CTRL: Modifiers = Modifiers {
    alt: false,
    ctrl: true,
    shift: false,
    mac_cmd: false,
    command: true,
};

const SHIFT: Modifiers = Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: false,
};

const CTRL_SHIFT: Modifiers = Modifiers {
    alt: false,
    ctrl: true,
    shift: true,
    mac_cmd: false,
    command: true,
};

fn row_center(world: &World, entity: Entity) -> Pos2 {
    world
        .resource::<OutlinerRows>()
        .row(entity)
        .unwrap_or_else(|| panic!("row of {entity:?} drawn"))
        .rect
        .center()
}

fn selected(world: &World) -> Vec<Entity> {
    world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default()
}

#[test]
fn drag_a_multi_selection_onto_a_row_then_undo_with_ctrl_z() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::new(10.0, 0.0, 0.0));
    let bravo = spawn(world, "Bravo", Vec3::new(0.0, 2.0, 0.0));
    let charlie = spawn(world, "Charlie", Vec3::new(0.0, 0.0, 3.0));
    let mut ui = Harness::new();
    ui.settle(world);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, bravo, charlie],
        "rows sorted by name"
    );

    // Click Bravo, Ctrl-click Charlie: both selected.
    let at = row_center(world, bravo);
    ui.click(world, at, Modifiers::NONE);
    let at = row_center(world, charlie);
    ui.click(world, at, CTRL);
    assert_eq!(selected(world), vec![bravo, charlie]);

    // Drag Bravo (selected, so the whole selection travels) onto Alpha.
    let (from, to) = (row_center(world, bravo), row_center(world, alpha));
    ui.drag(world, from, to);
    assert_eq!(parent_of(world, bravo), Some(alpha), "Bravo dropped under Alpha");
    assert_eq!(
        parent_of(world, charlie),
        Some(alpha),
        "Charlie travelled with the selection"
    );
    assert_eq!(depth(world), 1, "one drop = one undo entry");
    assert!(
        (world_pos(world, charlie) - Vec3::new(0.0, 0.0, 3.0)).length() < 1e-4,
        "the drop keeps world poses"
    );
    // The children are drawn indented under Alpha now.
    let rows = world.resource::<OutlinerRows>().clone();
    assert_eq!(rows.order(), vec![alpha, bravo, charlie]);
    assert_eq!(rows.row(bravo).map(|r| r.depth), Some(1));

    // Ctrl+Z over the panel undoes the whole drop.
    let at = row_center(world, alpha);
    ui.key(world, at, Key::Z, CTRL);
    assert_eq!(parent_of(world, bravo), None);
    assert_eq!(parent_of(world, charlie), None);
    // Ctrl+Shift+Z redoes it.
    ui.key(world, at, Key::Z, CTRL_SHIFT);
    assert_eq!(parent_of(world, bravo), Some(alpha));
    assert_eq!(parent_of(world, charlie), Some(alpha));
}

#[test]
fn drop_on_the_root_zone_unparents_and_cyclic_drops_are_refused() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::ZERO);
    let bravo = spawn(world, "Bravo", Vec3::new(1.0, 0.0, 0.0));
    world.entity_mut(bravo).insert(ChildOf(alpha));
    let mut ui = Harness::new();
    ui.settle(world);

    // Drag Alpha onto its own child Bravo: refused, nothing changes, no undo entry.
    let (from, to) = (row_center(world, alpha), row_center(world, bravo));
    ui.drag(world, from, to);
    assert_eq!(parent_of(world, alpha), None);
    assert_eq!(parent_of(world, bravo), Some(alpha));
    assert_eq!(depth(world), 0, "a refused drop pushes nothing");

    // Drag Bravo (unselected: it moves alone) to the empty space under the tree.
    let zone = world
        .resource::<OutlinerRows>()
        .root_drop
        .expect("root drop zone drawn");
    let (from, to) = (row_center(world, bravo), zone.center());
    ui.drag(world, from, to);
    assert_eq!(parent_of(world, bravo), None, "dropped on the root zone");
    assert_eq!(depth(world), 1);
    assert!(undo_world(world));
    assert_eq!(parent_of(world, bravo), Some(alpha));
}

#[test]
fn shift_click_selects_the_drawn_range_and_hotkeys_act_on_it() {
    let mut app = app();
    let world = app.world_mut();
    let names = ["Alpha", "Bravo", "Charlie", "Delta"];
    let entities: Vec<Entity> = names
        .iter()
        .enumerate()
        .map(|(i, name)| spawn(world, name, Vec3::new(i as f32, 0.0, 0.0)))
        .collect();
    let mut ui = Harness::new();
    ui.settle(world);

    let at = row_center(world, entities[1]);
    ui.click(world, at, Modifiers::NONE);
    let at = row_center(world, entities[3]);
    ui.click(world, at, SHIFT);
    assert_eq!(selected(world), entities[1..=3].to_vec(), "Bravo..Delta");
    // Ctrl+Shift-click adds a range; a plain Shift range replaces.
    let at = row_center(world, entities[0]);
    ui.click(world, at, CTRL_SHIFT);
    assert_eq!(selected(world).len(), 4);
    let at = row_center(world, entities[1]);
    ui.click(world, at, SHIFT);
    assert_eq!(
        selected(world),
        entities[0..=1].to_vec(),
        "range from the primary (Alpha)"
    );

    // Ctrl+G groups the selection; Ctrl+D duplicates the group; Delete removes the copy.
    let at = row_center(world, entities[2]);
    ui.key(world, at, Key::G, CTRL);
    let group = by_name(world, "Group");
    assert_eq!(parent_of(world, entities[0]), Some(group));
    assert_eq!(parent_of(world, entities[1]), Some(group));
    assert_eq!(selected(world), vec![group]);
    assert_eq!(depth(world), 1);

    let at = row_center(world, entities[2]);
    ui.key(world, at, Key::D, CTRL);
    let copies = selected(world);
    assert_eq!(copies.len(), 1);
    assert_ne!(copies[0], group);
    assert_eq!(
        world.get::<Children>(copies[0]).map(|c| c.len()),
        Some(2),
        "the copy has both members"
    );
    assert_eq!(depth(world), 2);

    let at = row_center(world, entities[2]);
    ui.key(world, at, Key::Delete, Modifiers::NONE);
    assert!(world.get_entity(copies[0]).is_err());
    assert!(selected(world).is_empty(), "deleted entities leave the selection");
    assert_eq!(depth(world), 3);
}

#[test]
fn the_search_box_filters_rows_and_keeps_typing_out_of_the_hotkeys() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::ZERO);
    let bravo = spawn(world, "Bravo", Vec3::X);
    let charlie = spawn(world, "Charlie", Vec3::Y);
    world.entity_mut(charlie).insert(ChildOf(alpha));
    let mut ui = Harness::new();
    ui.settle(world);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, charlie, bravo]
    );

    // Collapse Alpha: a search must still show its matching child.
    let toggle = world.resource::<OutlinerRows>().rows[0].toggle.center();
    ui.click(world, toggle, Modifiers::NONE);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, bravo],
        "Alpha collapsed"
    );

    // Select Bravo, then type into the search box.
    let at = row_center(world, bravo);
    ui.click(world, at, Modifiers::NONE);
    assert_eq!(selected(world), vec![bravo]);
    let search = world
        .resource::<OutlinerRows>()
        .search
        .expect("search box drawn")
        .center();
    ui.click(world, search, Modifiers::NONE);
    ui.frame(world, vec![Event::Text("CHAR".to_string())]);
    ui.settle(world);
    let rows = world.resource::<OutlinerRows>().clone();
    assert_eq!(rows.filter, "CHAR");
    assert_eq!(
        rows.order(),
        vec![alpha, charlie],
        "the match and its ancestor, opened"
    );

    // Backspace and Delete while the search box has focus edit the text, not the level.
    ui.key(world, search, Key::Backspace, Modifiers::NONE);
    ui.key(world, search, Key::Delete, Modifiers::NONE);
    assert!(
        world.get_entity(bravo).is_ok(),
        "typing never deletes the selection"
    );
    assert_eq!(world.resource::<OutlinerRows>().filter, "CHA");

    // Clearing the search restores Alpha's collapsed state.
    for _ in 0..3 {
        ui.key(world, search, Key::Backspace, Modifiers::NONE);
    }
    assert_eq!(world.resource::<OutlinerRows>().filter, "");
    assert_eq!(world.resource::<OutlinerRows>().order(), vec![alpha, bravo]);

    // A drop works on filtered rows: search again, drag Charlie to the root zone.
    ui.frame(world, vec![Event::Text("cha".to_string())]);
    ui.settle(world);
    assert_eq!(world.resource::<OutlinerRows>().order(), vec![alpha, charlie]);
    let zone = world.resource::<OutlinerRows>().root_drop.expect("zone").center();
    let from = row_center(world, charlie);
    ui.drag(world, from, zone);
    assert_eq!(parent_of(world, charlie), None, "dropped on the root zone");
    assert_eq!(depth(world), 1);
}

// --- The agent surface ------------------------------------------------------------------------

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    regia_editor::visibility::mark_internal_entities(world);
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|err| panic!("{method}: {err:?}"))
}

/// The refusal's `data.hierarchy_error` (the catalog code `F000x` is in `data.code`, ADR-1354).
fn error_code(world: &mut World, method: &str, params: Value) -> String {
    let err = call(world, method, params).expect_err("refused");
    err.data
        .as_ref()
        .and_then(|d| d.get("hierarchy_error"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn ids(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[test]
fn agent_reparent_group_duplicate_delete_and_select() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::new(4.0, 0.0, 0.0)).to_bits();
    let bravo = spawn(world, "Bravo", Vec3::new(0.0, 1.0, 0.0)).to_bits();
    let charlie = spawn(world, "Charlie", Vec3::new(0.0, 0.0, 1.0)).to_bits();

    // The tree as the panel shows it.
    let tree = ok(world, "editor.hierarchy", json!({}));
    assert_eq!(tree["count"], 3);
    assert_eq!(tree["roots"][0]["name"], "Alpha");

    // Dry run: validated, nothing applied.
    let dry = ok(
        world,
        "editor.reparent",
        json!({ "entities": [bravo, charlie], "parent": alpha, "dry_run": true }),
    );
    assert_eq!(ids(&dry["moved"]), vec![bravo, charlie]);
    assert_eq!(dry["applied"], false);
    assert_eq!(depth(world), 0);

    let moved = ok(
        world,
        "editor.reparent",
        json!({ "entities": [bravo, charlie], "parent": alpha }),
    );
    assert_eq!(moved["applied"], true);
    assert_eq!(moved["undoable"], 1);
    let tree = ok(world, "editor.hierarchy", json!({ "root": alpha }));
    assert_eq!(tree["roots"][0]["child_count"], 2);
    assert_eq!(tree["count"], 3);
    let filtered = ok(world, "editor.hierarchy", json!({ "filter": "char" }));
    assert_eq!(filtered["count"], 2, "Charlie and its ancestor Alpha");
    assert_eq!(filtered["roots"][0]["children"][0]["name"], "Charlie");
    let shallow = ok(world, "editor.hierarchy", json!({ "max_depth": 0 }));
    assert_eq!(shallow["roots"][0]["children"], json!([]));
    assert_eq!(shallow["roots"][0]["child_count"], 2);

    // Refusals carry stable lowercase codes.
    assert_eq!(
        error_code(
            world,
            "editor.reparent",
            json!({ "entities": [alpha], "parent": bravo })
        ),
        "hierarchy_cycle"
    );
    assert_eq!(
        error_code(world, "editor.reparent", json!({ "entities": [] })),
        "hierarchy_no_entities"
    );
    assert_eq!(depth(world), 1, "refusals push nothing");

    // Select the subtree, then group it by selection (no `entities`).
    let picked = ok(
        world,
        "editor.select_hierarchy",
        json!({ "entities": [alpha], "scope": "children" }),
    );
    assert_eq!(picked["entities"].as_array().map(Vec::len), Some(2));
    let grouped = ok(world, "editor.group", json!({ "name": "Pair" }));
    assert_eq!(ids(&grouped["members"]), vec![bravo, charlie]);
    assert_eq!(grouped["parent"], alpha, "the members' common parent");
    let group = grouped["group"].as_u64().expect("group id");

    let copies = ok(
        world,
        "editor.duplicate",
        json!({ "entities": [group], "offset": [0, 0, 5] }),
    );
    let copy = ids(&copies["entities"])[0];
    assert_ne!(copy, group);
    let tree = ok(world, "editor.hierarchy", json!({ "root": copy }));
    assert_eq!(tree["roots"][0]["parent"], alpha, "the copy stays under Alpha");
    assert_eq!(tree["roots"][0]["child_count"], 2);

    let deleted = ok(world, "editor.delete", json!({ "entities": [copy] }));
    assert_eq!(ids(&deleted["deleted"]), vec![copy]);
    assert_eq!(deleted["undoable"], 4, "reparent, group, duplicate, delete");

    // Undo everything back to the start.
    for _ in 0..4 {
        assert_eq!(ok(world, "editor.undo", json!({}))["undone"], true);
    }
    let tree = ok(world, "editor.hierarchy", json!({}));
    assert_eq!(tree["count"], 3);
    assert!(
        tree["roots"]
            .as_array()
            .is_some_and(|roots| roots.iter().all(|r| r["child_count"] == 0)),
        "all three are roots again: {tree}"
    );
}

// --- Virtualized rows (ADR-7306) ----------------------------------------------------------------

/// A 50-entity level: 5 roots, each with 3 children, each child with 2 grandchildren
/// (5 + 15 + 30 = 50), names out of spawn order so the sort matters.
fn fifty(world: &mut World) -> Vec<Entity> {
    let mut all = Vec::new();
    for r in (0..5).rev() {
        let root = spawn(world, &format!("Root {r}"), Vec3::ZERO);
        all.push(root);
        for c in (0..3).rev() {
            let child = spawn(world, &format!("Child {r}.{c}"), Vec3::X);
            world.entity_mut(child).insert(ChildOf(root));
            all.push(child);
            for g in 0..2 {
                let grand = spawn(world, &format!("Grand {r}.{c}.{g}"), Vec3::Y);
                world.entity_mut(grand).insert(ChildOf(child));
                all.push(grand);
            }
        }
    }
    assert_eq!(all.len(), 50);
    all
}

/// The rows the Outliner listed before virtualization: a pre-order walk of the (filtered) tree,
/// skipping the children of a closed row; every kept row is open while searching. This is the
/// old `draw_node` recursion, kept here as the reference.
fn reference_order(world: &World, collapsed: &[Entity], filter: &str) -> Vec<(Entity, usize)> {
    use regia_editor::outliner::{HierarchyNode, filter_tree, hierarchy_tree};
    fn walk(
        node: &HierarchyNode,
        depth: usize,
        collapsed: &[Entity],
        filtering: bool,
        out: &mut Vec<(Entity, usize)>,
    ) {
        out.push((node.entity, depth));
        let open = filtering || !collapsed.contains(&node.entity);
        if open {
            for child in &node.children {
                walk(child, depth + 1, collapsed, filtering, out);
            }
        }
    }
    let tree = filter_tree(&hierarchy_tree(world), filter);
    let mut out = Vec::new();
    for node in &tree {
        walk(node, 0, collapsed, !filter.trim().is_empty(), &mut out);
    }
    out
}

fn drawn(world: &World) -> Vec<(Entity, usize)> {
    world
        .resource::<OutlinerRows>()
        .rows
        .iter()
        .map(|row| (row.entity, row.depth))
        .collect()
}

#[test]
fn virtual_rows_match_the_old_tree_walk_on_a_fifty_entity_level() {
    use regia_editor::outliner::OutlinerView;
    let mut app = app();
    let world = app.world_mut();
    let all = fifty(world);
    let mut ui = Harness::new();
    ui.settle(world);

    // Everything open: the same 50 rows in the same order and depths; the drawn rows are the
    // reference's first screenful (the harness panel is 480 px high), each at its depth.
    let reference = reference_order(world, &[], "");
    assert_eq!(reference.len(), 50);
    let listed = world.resource::<OutlinerRows>().order();
    assert_eq!(
        listed,
        reference.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
        "same rows, same order"
    );
    let on_screen = drawn(world);
    assert!(
        on_screen.len() >= 10 && on_screen.len() < 50,
        "a screenful is drawn, not all 50: {}",
        on_screen.len()
    );
    assert_eq!(
        on_screen[..],
        reference[..on_screen.len()],
        "drawn rows are the top of the list"
    );

    // Close two roots and one child (through the row toggle and the view): the same as the walk.
    let root1 = by_name(world, "Root 1");
    let toggle = world
        .resource::<OutlinerRows>()
        .row(root1)
        .expect("Root 1 drawn")
        .toggle
        .center();
    ui.click(world, toggle, Modifiers::NONE);
    let root3 = by_name(world, "Root 3");
    let child = by_name(world, "Child 4.2");
    {
        let mut view = world.resource_mut::<OutlinerView>();
        view.set_open(root3, false);
        view.set_open(child, false);
    }
    ui.settle(world);
    let reference = reference_order(world, &[root1, root3, child], "");
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        reference.iter().map(|(e, _)| *e).collect::<Vec<_>>()
    );
    let on_screen = drawn(world);
    assert_eq!(on_screen[..], reference[..on_screen.len()]);

    // A search: the matches and their ancestors, all open, as the old filter drew them.
    let search = world.resource::<OutlinerRows>().search.expect("search").center();
    ui.click(world, search, Modifiers::NONE);
    ui.frame(world, vec![Event::Text("grand 3".to_string())]);
    ui.settle(world);
    let reference = reference_order(world, &[root1, root3, child], "grand 3");
    assert_eq!(
        reference.len(),
        1 + 3 + 6,
        "Root 3, its children, their six grandchildren"
    );
    assert_eq!(drawn(world), reference, "a short list is drawn whole");
    assert!(all.contains(&reference[0].0));
}

#[test]
fn the_tree_is_rebuilt_only_when_the_level_or_the_open_state_changes() {
    use regia_editor::outliner::OutlinerView;
    let mut app = app();
    let world = app.world_mut();
    fifty(world);
    let mut ui = Harness::new();
    ui.settle(world);
    let builds = |world: &World| world.resource::<OutlinerView>().tree_builds();
    let flattens = |world: &World| world.resource::<OutlinerView>().flattens();
    let (b0, f0) = (builds(world), flattens(world));
    for _ in 0..5 {
        ui.frame(world, vec![]);
    }
    assert_eq!(builds(world), b0, "steady frames reuse the tree");
    assert_eq!(flattens(world), f0, "and the rows");

    // A rename (a value change, no structural change) rebuilds once.
    let alpha = by_name(world, "Root 0");
    world.entity_mut(alpha).insert(Name::new("Aardvark"));
    ui.frame(world, vec![]);
    ui.frame(world, vec![]);
    assert_eq!(builds(world), b0 + 1, "rename");
    assert_eq!(
        world.resource::<OutlinerRows>().order()[0],
        alpha,
        "sorted by the new name"
    );

    // A reparent rebuilds once.
    let grand = by_name(world, "Grand 4.2.1");
    world.entity_mut(grand).insert(ChildOf(alpha));
    ui.frame(world, vec![]);
    ui.frame(world, vec![]);
    assert_eq!(builds(world), b0 + 2, "reparent");
    assert_eq!(
        world
            .resource::<OutlinerRows>()
            .rows
            .iter()
            .find(|r| r.entity == grand)
            .map(|r| r.depth),
        Some(1)
    );

    // A spawn and a despawn rebuild once each.
    let extra = spawn(world, "Zulu", Vec3::ZERO);
    ui.frame(world, vec![]);
    assert_eq!(builds(world), b0 + 3, "spawn");
    world.despawn(extra);
    ui.frame(world, vec![]);
    assert_eq!(builds(world), b0 + 4, "despawn");
    assert!(!world.resource::<OutlinerRows>().order().contains(&extra));

    // Opening or closing a row re-flattens the rows without rebuilding the tree.
    let f1 = flattens(world);
    world.resource_mut::<OutlinerView>().set_open(alpha, false);
    ui.frame(world, vec![]);
    assert_eq!(builds(world), b0 + 4, "an open/close keeps the tree");
    assert_eq!(flattens(world), f1 + 1, "and re-flattens once");
    ui.frame(world, vec![]);
    assert_eq!(flattens(world), f1 + 1);
}

#[test]
fn a_large_level_draws_a_screenful_and_reveals_an_outside_pick() {
    use regia_editor::outliner::OutlinerView;
    let mut app = app();
    let world = app.world_mut();
    // 40 groups of 50 children: 2 040 rows when open.
    let mut far = None;
    for g in 0..40 {
        let group = spawn(world, &format!("Group {g:02}"), Vec3::ZERO);
        for c in 0..50 {
            let child = spawn(world, &format!("Item {g:02}.{c:02}"), Vec3::X);
            world.entity_mut(child).insert(ChildOf(group));
            if g == 37 && c == 25 {
                far = Some((group, child));
            }
        }
    }
    let (group, target) = far.expect("target spawned");
    let mut ui = Harness::new();
    ui.settle(world);
    let rows = world.resource::<OutlinerRows>().clone();
    assert_eq!(rows.order().len(), 2040, "every row is listed");
    assert!(
        rows.rows.len() < 40,
        "only a screenful is drawn: {}",
        rows.rows.len()
    );
    assert!(rows.row(target).is_none(), "the far row is off screen");

    // Close the target's group, then select the target from outside the panel (a viewport pick):
    // the group opens and the row scrolls into view.
    world.resource_mut::<OutlinerView>().set_open(group, false);
    ui.settle(world);
    assert!(!world.resource::<OutlinerRows>().order().contains(&target));
    let mut selection = Selection::default();
    selection.select(target, regia_editor::selection::SelectMode::Replace);
    world.insert_resource(selection);
    ui.settle(world);
    ui.settle(world);
    assert!(
        world.resource::<OutlinerView>().is_open(group),
        "the pick opened its parent"
    );
    let row = world
        .resource::<OutlinerRows>()
        .row(target)
        .copied()
        .expect("the picked row is drawn after the scroll");
    assert_eq!(row.depth, 1);

    // Arrow keys walk the listed rows (off-screen ones too) and keep the selection in view.
    let at = row.rect.center();
    ui.key(world, at, Key::ArrowDown, Modifiers::NONE);
    let next = by_name(world, "Item 37.26");
    assert_eq!(selected(world), vec![next]);
    ui.key(world, at, Key::ArrowUp, Modifiers::NONE);
    ui.key(world, at, Key::ArrowUp, Modifiers::NONE);
    assert_eq!(selected(world), vec![by_name(world, "Item 37.24")]);
    ui.key(world, at, Key::ArrowDown, SHIFT);
    assert_eq!(
        selected(world),
        vec![by_name(world, "Item 37.24"), target],
        "Shift extends from the primary"
    );
    // Left closes the primary's row; the next Down from a group walks past its hidden children.
    let at = world.resource::<OutlinerRows>().rows[0].rect.center();
    let mut selection = Selection::default();
    selection.select(group, regia_editor::selection::SelectMode::Replace);
    world.insert_resource(selection);
    ui.settle(world);
    ui.key(world, at, Key::ArrowLeft, Modifiers::NONE);
    assert!(!world.resource::<OutlinerView>().is_open(group));
    ui.key(world, at, Key::ArrowDown, Modifiers::NONE);
    assert_eq!(selected(world), vec![by_name(world, "Group 38")]);
    // Down into Group 38's open children: each new primary is drawn (kept in view).
    for _ in 0..3 {
        ui.key(world, at, Key::ArrowDown, Modifiers::NONE);
    }
    let item = by_name(world, "Item 38.02");
    assert_eq!(selected(world), vec![item]);
    assert!(
        world.resource::<OutlinerRows>().row(item).is_some(),
        "the selection stays in view"
    );
}

// --- w12 outliner tree (ADR-7757): keyboard, rename in place, folders, query, paste --------

fn press(key: Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn name_of(world: &World, entity: Entity) -> String {
    world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default()
}

/// The whole tree from the keyboard: Home / End / Page Down / Up / Down, Right into the first
/// child, Left back to the parent and closed, Shift+Right opening the branch, Shift+Home extending
/// the selection, Ctrl+Shift+Minus / Plus collapsing and expanding all, F2 renaming in place
/// (Enter commits one undo step, Esc cancels, Tab moves to the next row's name), Enter framing,
/// Ctrl+F focusing the search box.
#[test]
fn the_tree_is_complete_from_the_keyboard() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::ZERO);
    let a1 = spawn(world, "A1", Vec3::X);
    let a2 = spawn(world, "A2", Vec3::Y);
    let a2x = spawn(world, "A2x", Vec3::Z);
    world.entity_mut(a1).insert(ChildOf(alpha));
    world.entity_mut(a2).insert(ChildOf(alpha));
    world.entity_mut(a2x).insert(ChildOf(a2));
    let bravo = spawn(world, "Bravo", Vec3::ZERO);
    let charlie = spawn(world, "Charlie", Vec3::ZERO);
    let mut ui = Harness::new();
    ui.settle(world);
    let order = world.resource::<OutlinerRows>().order();
    assert_eq!(order, vec![alpha, a1, a2, a2x, bravo, charlie]);
    let over = row_center(world, bravo);

    ui.key(world, over, Key::End, Modifiers::NONE);
    assert_eq!(selected(world), vec![charlie], "End: the last row");
    ui.key(world, over, Key::Home, Modifiers::NONE);
    assert_eq!(selected(world), vec![alpha], "Home: the first row");
    ui.key(world, over, Key::PageDown, Modifiers::NONE);
    assert_eq!(
        selected(world),
        vec![charlie],
        "Page Down past the end: the last row"
    );
    ui.key(world, over, Key::PageUp, Modifiers::NONE);
    assert_eq!(
        selected(world),
        vec![alpha],
        "Page Up past the top: the first row"
    );
    ui.key(world, over, Key::ArrowRight, Modifiers::NONE);
    assert_eq!(selected(world), vec![a1], "Right on an open row: its first child");
    ui.key(world, over, Key::ArrowLeft, Modifiers::NONE);
    assert_eq!(selected(world), vec![alpha], "Left on a leaf: its parent");
    ui.key(world, over, Key::ArrowLeft, Modifiers::NONE);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, bravo, charlie],
        "Left on an open row closes it"
    );
    ui.key(world, over, Key::ArrowRight, Modifiers::NONE);
    assert_eq!(
        world.resource::<OutlinerRows>().order().len(),
        6,
        "Right opens it"
    );

    // Collapse all, then Shift+Right opens Alpha's whole branch.
    ui.key(world, over, Key::Minus, CTRL_SHIFT);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, bravo, charlie]
    );
    ui.key(world, over, Key::ArrowRight, SHIFT);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![alpha, a1, a2, a2x, bravo, charlie],
        "Shift+Right opens the branch recursively"
    );
    ui.key(world, over, Key::ArrowLeft, SHIFT);
    ui.key(world, over, Key::Plus, CTRL_SHIFT);
    assert_eq!(world.resource::<OutlinerRows>().order().len(), 6, "Expand all");

    // Shift+End extends from the primary.
    ui.key(world, over, Key::ArrowDown, Modifiers::NONE);
    assert_eq!(selected(world), vec![a1]);
    ui.key(world, over, Key::End, SHIFT);
    assert_eq!(selected(world), vec![a1, a2, a2x, bravo, charlie]);

    // F2 renames in place; Enter commits one undo step.
    ui.key(world, over, Key::Home, Modifiers::NONE);
    let undo = depth(world);
    ui.key(world, over, Key::F2, Modifiers::NONE);
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("AlphaPrime".to_string())]);
    ui.key(world, over, Key::Enter, Modifiers::NONE);
    assert_eq!(name_of(world, alpha), "AlphaPrime");
    assert_eq!(depth(world), undo + 1, "a rename is one undo step");
    // Esc cancels.
    ui.key(world, over, Key::F2, Modifiers::NONE);
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("zz".to_string())]);
    ui.key(world, over, Key::Escape, Modifiers::NONE);
    assert_eq!(name_of(world, alpha), "AlphaPrime", "Esc leaves the name");
    assert_eq!(depth(world), undo + 1);
    // Tab commits and moves to the next row's name.
    ui.key(world, over, Key::F2, Modifiers::NONE);
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("AlphaPrime1".to_string())]);
    ui.key(world, over, Key::Tab, Modifiers::NONE);
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("A1b".to_string())]);
    ui.key(world, over, Key::Enter, Modifiers::NONE);
    assert_eq!(name_of(world, alpha), "AlphaPrime1");
    assert_eq!(name_of(world, a1), "A1b", "Tab went on to the next row");
    assert_eq!(depth(world), undo + 3);

    // Enter frames the selection: the main camera starts an ease.
    ui.key(world, over, Key::Enter, Modifiers::NONE);
    assert!(
        world
            .get_resource::<regia_editor::viewport_ops::CameraEases>()
            .is_some(),
        "Enter starts framing the selection"
    );

    // Ctrl+F focuses the search box: typed text goes into the filter.
    ui.key(world, over, Key::F, CTRL);
    ui.frame(world, vec![Event::Text("brav".to_string())]);
    ui.settle(world);
    assert_eq!(world.resource::<OutlinerRows>().filter, "brav");
    assert_eq!(world.resource::<OutlinerRows>().order(), vec![bravo]);
    // Enter in the search box leaves it without framing the selection (review row 9).
    world.remove_resource::<regia_editor::viewport_ops::CameraEases>();
    ui.key(world, over, Key::Enter, Modifiers::NONE);
    assert!(
        world
            .get_resource::<regia_editor::viewport_ops::CameraEases>()
            .is_none(),
        "Enter in the search box starts no camera ease"
    );
    assert_eq!(
        world.resource::<OutlinerRows>().filter,
        "brav",
        "the search stays"
    );
}

/// A plain click on the only selected row is a slow second click: it edits the name in place.
#[test]
fn a_second_click_on_the_selected_row_renames_it_in_place() {
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::ZERO);
    let _bravo = spawn(world, "Bravo", Vec3::X);
    let mut ui = Harness::new();
    ui.settle(world);
    let at = row_center(world, alpha);
    ui.click(world, at, Modifiers::NONE);
    assert_eq!(selected(world), vec![alpha]);
    // The harness's frames are 50 ms apart: wait out the double-click delay first.
    for _ in 0..10 {
        ui.frame(world, vec![]);
    }
    ui.click(world, at, Modifiers::NONE);
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("Alpha!".to_string())]);
    ui.key(world, at, Key::Enter, Modifiers::NONE);
    assert_eq!(name_of(world, alpha), "Alpha!");
}

/// While searching, Select all (Ctrl+A), a Shift range and the rubber band select only the rows
/// the search matches, never the ancestors kept to show where a match is.
#[test]
fn select_all_ranges_and_the_band_follow_the_search() {
    let mut app = app();
    let world = app.world_mut();
    let north = spawn(world, "North", Vec3::ZERO);
    let south = spawn(world, "South", Vec3::ZERO);
    let lamp_a = spawn(world, "Lamp A", Vec3::X);
    let lamp_b = spawn(world, "Lamp B", Vec3::Y);
    let _crate = spawn(world, "Crate", Vec3::Z);
    world.entity_mut(lamp_a).insert(ChildOf(north));
    world.entity_mut(lamp_b).insert(ChildOf(south));
    let mut ui = Harness::new();
    ui.settle(world);
    let search = world.resource::<OutlinerRows>().search.expect("search").center();
    ui.click(world, search, Modifiers::NONE);
    ui.frame(world, vec![Event::Text("lamp".to_string())]);
    ui.settle(world);
    assert_eq!(
        world.resource::<OutlinerRows>().order(),
        vec![north, lamp_a, south, lamp_b]
    );
    // Leave the search box (click a row), then Ctrl+A.
    let at = row_center(world, lamp_a);
    ui.click(world, at, Modifiers::NONE);
    ui.key(world, at, Key::A, CTRL);
    assert_eq!(selected(world), vec![lamp_a, lamp_b], "only the matching rows");

    // A Shift range over a kept ancestor (South) leaves it out.
    ui.click(world, at, Modifiers::NONE);
    let at_b = row_center(world, lamp_b);
    ui.click(world, at_b, SHIFT);
    assert_eq!(selected(world), vec![lamp_a, lamp_b]);

    // The rubber band from empty space right of the rows over all four rows.
    let rows = world.resource::<OutlinerRows>().clone();
    let top = rows.row(north).expect("north").rect.center().y;
    let bottom = rows.row(lamp_b).expect("lamp b").rect.center().y;
    ui.click(world, row_center(world, north), Modifiers::NONE);
    assert_eq!(selected(world), vec![north]);
    ui.drag(world, Pos2::new(350.0, top), Pos2::new(350.0, bottom));
    assert_eq!(
        selected(world),
        vec![lamp_a, lamp_b],
        "the band selects matching rows"
    );
}

/// Folders: New Folder (Ctrl+Shift+N) holds the selected top-level entities and its name is
/// edited in place; a drop on a folder row moves an entity in; a drop on the root zone moves it
/// out; each is one undo step and the level saves the membership.
#[test]
fn folders_from_the_panel_hold_entities_without_being_entities() {
    use regia_editor::outliner_folders::folder_of;
    let mut app = app();
    let world = app.world_mut();
    let alpha = spawn(world, "Alpha", Vec3::ZERO);
    let bravo = spawn(world, "Bravo", Vec3::X);
    let mut ui = Harness::new();
    ui.settle(world);
    let roots_before = regia_editor::outliner::hierarchy_tree(world).len();
    let at = row_center(world, alpha);
    ui.click(world, at, Modifiers::NONE);
    ui.key(world, at, Key::N, CTRL_SHIFT);
    assert_eq!(folder_of(world, alpha).as_deref(), Some("New Folder"));
    // The new folder's name is in edit: replace it.
    ui.frame(world, vec![press(Key::A, CTRL)]);
    ui.frame(world, vec![Event::Text("Lights".to_string())]);
    ui.key(world, at, Key::Enter, Modifiers::NONE);
    assert_eq!(folder_of(world, alpha).as_deref(), Some("Lights"));
    let rows = world.resource::<OutlinerRows>().clone();
    let (path, folder_rect) = rows.folders.first().cloned().expect("a folder row");
    assert_eq!(path, "Lights");
    assert_eq!(rows.order(), vec![alpha, bravo], "the folder lists Alpha first");
    assert_eq!(
        regia_editor::outliner::hierarchy_tree(world).len(),
        roots_before,
        "a folder is not an entity"
    );

    // Drag Bravo onto the folder row.
    let from = row_center(world, bravo);
    ui.drag(world, from, folder_rect.center());
    assert_eq!(folder_of(world, bravo).as_deref(), Some("Lights"));
    let undo = depth(world);
    // Drag it out to the root zone.
    let zone = world.resource::<OutlinerRows>().root_drop.expect("zone").center();
    let from = row_center(world, bravo);
    ui.drag(world, from, zone);
    assert_eq!(folder_of(world, bravo), None, "out of the folder");
    assert_eq!(depth(world), undo + 1);
    assert!(undo_world(world));
    assert_eq!(folder_of(world, bravo).as_deref(), Some("Lights"));
}

/// The agent surface of the same features.
#[test]
fn agent_query_rename_expand_paste_and_folders() {
    let mut app = app();
    let world = app.world_mut();
    let holder = spawn(world, "Holder", Vec3::new(10.0, 0.0, 0.0));
    let kid = spawn(world, "Kid", Vec3::new(1.0, 0.0, 0.0));
    world.entity_mut(kid).insert(ChildOf(holder));
    let other = spawn(world, "Other", Vec3::new(0.0, 5.0, 0.0));

    // editor.hierarchy takes the query language and reports parse errors with a column.
    let tree = ok(
        world,
        "editor.hierarchy",
        json!({ "filter": "Transform.translation.x>5" }),
    );
    assert_eq!(tree["count"], 1, "{tree}");
    let tree = ok(world, "editor.hierarchy", json!({ "filter": "kid or other" }));
    assert_eq!(tree["count"], 3, "Holder is kept as Kid's ancestor: {tree}");
    let err = call(world, "editor.hierarchy", json!({ "filter": "(kid" })).expect_err("malformed");
    let data = err.data.expect("data");
    assert_eq!(data["query_error"], "query_unclosed_paren");
    assert_eq!(data["column"], 1);

    // editor.outliner_rename: one undo step; a blank name is refused.
    let before = depth(world);
    ok(
        world,
        "editor.outliner_rename",
        json!({ "entity": kid.to_bits(), "name": "Child" }),
    );
    assert_eq!(name_of(world, kid), "Child");
    assert_eq!(depth(world), before + 1);
    let err = call(
        world,
        "editor.outliner_rename",
        json!({ "entity": kid.to_bits(), "name": "  " }),
    )
    .expect_err("blank");
    assert_eq!(err.data.expect("data")["outliner_error"], "rename_empty");

    // editor.outliner_expand: collapse all, then open Holder's branch.
    let shut = ok(
        world,
        "editor.outliner_expand",
        json!({ "all": true, "open": false }),
    );
    assert_eq!(shut["listed"], 2, "{shut}");
    let open = ok(
        world,
        "editor.outliner_expand",
        json!({ "entity": holder.to_bits(), "recursive": true }),
    );
    assert_eq!(open["listed"], 3, "{open}");
    let err = call(world, "editor.outliner_expand", json!({})).expect_err("no target");
    assert_eq!(err.data.expect("data")["outliner_error"], "expand_no_target");

    // editor.paste: an empty clipboard is refused; paste as child at the original position keeps
    // the world pose, as one undo step.
    let err = call(world, "editor.paste", json!({})).expect_err("empty");
    assert_eq!(err.data.expect("data")["outliner_error"], "paste_clipboard_empty");
    ok(world, "editor.select", json!({ "entities": [kid.to_bits()] }));
    ok(world, "editor.menu_invoke", json!({ "path": "Edit/Copy" }));
    let before = depth(world);
    let pasted = ok(
        world,
        "editor.paste",
        json!({ "into": other.to_bits(), "at_original": true }),
    );
    let copy = ids(&pasted["pasted"])[0];
    let copy = Entity::try_from_bits(copy).expect("id");
    assert_eq!(parent_of(world, copy), Some(other));
    assert!(
        (world_pos(world, copy) - Vec3::new(11.0, 0.0, 0.0)).length() < 1e-4,
        "at the original world position: {}",
        world_pos(world, copy)
    );
    assert_eq!(depth(world), before + 1, "paste as child is one undo step");
    assert!(undo_world(world));
    assert!(world.get_entity(copy).is_err(), "one undo removes the paste");
    // Without at_original the copied local transform applies under the new parent.
    let pasted = ok(world, "editor.paste", json!({ "into": other.to_bits() }));
    let copy = Entity::try_from_bits(ids(&pasted["pasted"])[0]).expect("id");
    assert!((world_pos(world, copy) - Vec3::new(1.0, 5.0, 0.0)).length() < 1e-4);

    // Folders.
    ok(world, "editor.folder_create", json!({ "path": "Props/Small" }));
    let moved = ok(
        world,
        "editor.folder_move",
        json!({ "entities": [holder.to_bits()], "path": "Props/Small" }),
    );
    assert_eq!(moved["folders"], json!(["Props", "Props/Small"]));
    let tree = ok(
        world,
        "editor.hierarchy",
        json!({ "filter": "under:Props/Small" }),
    );
    assert_eq!(tree["count"], 2, "folders count as ancestors for under: {tree}");
    let rows = ok(world, "editor.outliner_rows", json!({}));
    assert!(
        rows["rows"]
            .as_array()
            .is_some_and(|r| r.iter().any(|row| row["folder"] == "Props/Small")),
        "{rows}"
    );
    let err = call(
        world,
        "editor.folder_move",
        json!({ "entities": [kid.to_bits()], "path": "Props" }),
    )
    .expect_err("child");
    assert_eq!(
        err.data.expect("data")["folder_error"],
        "folder_entity_not_top_level"
    );
    ok(
        world,
        "editor.folder_rename",
        json!({ "path": "Props", "to": "Things" }),
    );
    assert_eq!(
        regia_editor::outliner_folders::folder_of(world, holder).as_deref(),
        Some("Things/Small")
    );
    let gone = ok(world, "editor.folder_delete", json!({ "path": "Things" }));
    assert_eq!(gone["moved"], 1);
    assert_eq!(
        regia_editor::outliner_folders::folder_of(world, holder).as_deref(),
        Some("Small"),
        "deleting a folder keeps its entities"
    );
    assert!(world.get_entity(holder).is_ok());
}
