// SPDX-License-Identifier: MPL-2.0
//! The logic graph canvas's keyboard route (FU-5, ADR-8628): the Nodes list beside the canvas
//! selects nodes, moves between them, starts and completes a wire and cancels one, with keys only,
//! and every edit it makes is the one `editor.logic_edit` makes (checked through the agent
//! methods). Fixed frames, no pointer events.

use egui::{Key, Modifiers};
use regia_ai::logic::LogicEvent;
use regia_ai::logic_edit::LogicOp;
use regia_ai::state_tree::Condition;
use serde_json::{Value, json};

use super::panel::{self, find_node, node_row_name};
use super::*;
use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to, text};
use crate::command::UndoStack;

const SIZE: [f32; 2] = [1400.0, 900.0];

fn call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

fn ops_json(ops: &[LogicOp]) -> Value {
    Value::Array(
        ops.iter()
            .map(|op| serde_json::to_value(op).expect("op serializes"))
            .collect(),
    )
}

/// A new graph opened through the agent with an event `go: Start` wired into a `Key("door")`
/// condition, and an `Always` condition beside them. Returns the three node ids.
fn open_graph(world: &mut World) -> [u32; 3] {
    world.insert_resource(UndoStack::new());
    call(world, "editor.logic_open", json!({ "create": true }));
    let ops = [
        LogicOp::AddEvent {
            rule: "go".into(),
            event: LogicEvent::Start,
            priority: 0,
            cooldown_ticks: 0,
            id: None,
            at: Some([0.0, 0.0]),
        },
        LogicOp::AddCondition {
            condition: Condition::Key("door".into()),
            id: None,
            at: Some([240.0, 0.0]),
        },
        LogicOp::AddCondition {
            condition: Condition::Always,
            id: None,
            at: Some([480.0, 0.0]),
        },
    ];
    call(world, "editor.logic_edit", json!({ "ops": ops_json(&ops) }));
    let ids: Vec<u32> = world
        .resource::<LogicEditor>()
        .doc
        .nodes
        .keys()
        .copied()
        .collect();
    let [event, door, always] = ids[..] else {
        panic!("three nodes: {ids:?}");
    };
    call(
        world,
        "editor.logic_edit",
        json!({ "ops": ops_json(&[LogicOp::Connect { from: event, to: door }]) }),
    );
    [event, door, always]
}

fn state(world: &mut World) -> Value {
    call(world, "editor.logic_state", json!({}))
}

fn wires(world: &mut World) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = state(world)["wires"]
        .as_array()
        .expect("wires")
        .iter()
        .map(|w| (w["from"].as_u64().expect("from"), w["to"].as_u64().expect("to")))
        .collect();
    out.sort_unstable();
    out
}

fn depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

/// Select, move between nodes, start and complete a wire, cancel one, and nudge a node, from the
/// Nodes list with keys only; the result (wires, positions, selection) is read back through
/// `editor.logic_state` and equals the graph the same `editor.logic_edit` calls build.
#[test]
fn the_nodes_list_selects_wires_and_cancels_by_keyboard_like_the_agent() {
    let mut world = World::new();
    let [event, door, always] = open_graph(&mut world);
    let toast = world.resource::<LogicEditor>().toast.clone();
    let ctx = egui::Context::default();
    let draw = |world: &mut World, events: Vec<egui::Event>| {
        frame(&ctx, SIZE, events, |ui| panel::logic_graph_panel(ui, world))
    };
    let first = draw(&mut world, vec![]);
    assert!(
        labels_of(&first, egui::accesskit::Role::Canvas).contains(&"Logic graph canvas, 3 nodes".to_string()),
        "the canvas is named: {:?}",
        labels_of(&first, egui::accesskit::Role::Canvas)
    );
    let doc = world.resource::<LogicEditor>().doc.clone();
    let name = |id: u32| node_row_name(id, &doc.nodes[&id].kind);
    assert_eq!(name(event), format!("go: Start (#{event})"));

    // Tab reaches the event's row; Enter selects it and frames it on the canvas.
    tab_to(&ctx, SIZE, &name(event), false, 400, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    let epoch = world.resource::<LogicEditor>().ui.frame_epoch();
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(
        state(&mut world)["selected"],
        json!(event),
        "Enter selects the node"
    );
    assert_eq!(
        world.resource::<LogicEditor>().ui.frame_epoch(),
        epoch + 1,
        "and frames it (one fresh canvas id)"
    );

    // Down moves to the next node; Enter selects that one.
    draw(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    let down = draw(&mut world, vec![]);
    assert_eq!(focused_name(&down), Some(name(door)), "Down moves one node");
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(state(&mut world)["selected"], json!(door));

    // Ctrl+Shift+C starts a wire from `door`; the target typed by id, Enter completes it.
    let shortcut = Modifiers::COMMAND | Modifiers::SHIFT;
    let before = depth(&world);
    draw(&mut world, vec![key(Key::C, shortcut)]);
    assert!(
        world.resource::<LogicEditor>().ui.connect_open(),
        "the wire form opened"
    );
    let form = draw(&mut world, vec![]);
    let field = form
        .nodes
        .iter()
        .find(|(id, _)| *id == form.focus)
        .map(|(_, n)| (n.role(), n.labelled_by().len()));
    assert_eq!(
        field,
        Some((egui::accesskit::Role::TextInput, 1)),
        "the target field has the focus and a label"
    );
    draw(&mut world, vec![text(&format!("#{always}"))]);
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert!(
        !world.resource::<LogicEditor>().ui.connect_open(),
        "Enter closed the form"
    );
    assert_eq!(depth(&world), before + 1, "the wire is one undo step");
    assert_eq!(
        wires(&mut world),
        vec![
            (u64::from(event), u64::from(door)),
            (u64::from(door), u64::from(always))
        ],
        "door -> Always wired, through editor.logic_state"
    );
    draw(&mut world, vec![]);
    let back = draw(&mut world, vec![]);
    assert_eq!(
        focused_name(&back),
        Some(name(door)),
        "the focus is back on the row"
    );

    // Cancel: start a wire from `door` to the event, then Escape: nothing changes.
    let revision = state(&mut world)["revision"].clone();
    let before = depth(&world);
    draw(&mut world, vec![key(Key::C, shortcut)]);
    draw(&mut world, vec![]);
    draw(&mut world, vec![text(&format!("#{event}"))]);
    draw(&mut world, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(
        !world.resource::<LogicEditor>().ui.connect_open(),
        "Escape closed the form"
    );
    draw(&mut world, vec![]);
    let cancelled = draw(&mut world, vec![]);
    assert_eq!(depth(&world), before, "a cancelled wire makes no undo step");
    assert_eq!(state(&mut world)["revision"], revision, "and no edit");
    assert_eq!(
        focused_name(&cancelled),
        Some(name(door)),
        "the focus is back on the row"
    );

    // Down to `Always`, Alt+Right moves it one grid step (one undo step per press).
    draw(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    let on_always = draw(&mut world, vec![]);
    assert_eq!(focused_name(&on_always), Some(name(always)));
    let before = depth(&world);
    draw(&mut world, vec![key(Key::ArrowRight, Modifiers::ALT)]);
    assert_eq!(depth(&world), before + 1, "one move step per key press");
    let moved = state(&mut world)["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["id"] == json!(always))
        .map(|n| n["pos"].clone());
    assert_eq!(
        moved,
        Some(json!([480.0 + crate::canvas_keys::GRID_STEP, 0.0])),
        "through editor.logic_state"
    );
    assert_eq!(
        world.resource::<LogicEditor>().toast,
        toast,
        "no gesture was refused (a refusal writes the toast)"
    );

    // The agent builds the same graph with the same ops.
    let mut agent = World::new();
    open_graph(&mut agent);
    call(
        &mut agent,
        "editor.logic_edit",
        json!({ "ops": ops_json(&[
            LogicOp::Connect { from: door, to: always },
            LogicOp::MoveNode { node: always, at: [480.0 + crate::canvas_keys::GRID_STEP, 0.0] },
        ]), "select": door }),
    );
    assert_eq!(
        world.resource::<LogicEditor>().doc,
        agent.resource::<LogicEditor>().doc,
        "keys and agent build one graph"
    );
    assert_eq!(state(&mut world)["selected"], state(&mut agent)["selected"]);
}

/// A wire target that names no node is refused with a toast and changes nothing; Delete removes
/// the focused node (one undo step).
#[test]
fn an_unknown_wire_target_is_refused_and_delete_removes_the_node() {
    let mut world = World::new();
    let [_, door, always] = open_graph(&mut world);
    let ctx = egui::Context::default();
    let draw = |world: &mut World, events: Vec<egui::Event>| {
        frame(&ctx, SIZE, events, |ui| panel::logic_graph_panel(ui, world))
    };
    draw(&mut world, vec![]);
    let doc = world.resource::<LogicEditor>().doc.clone();
    let always_row = node_row_name(always, &doc.nodes[&always].kind);
    tab_to(&ctx, SIZE, &always_row, false, 400, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    let before = depth(&world);
    draw(
        &mut world,
        vec![key(Key::C, Modifiers::COMMAND | Modifiers::SHIFT)],
    );
    draw(&mut world, vec![]);
    draw(&mut world, vec![text("nowhere")]);
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(depth(&world), before, "nothing wired");
    assert!(
        world.resource::<LogicEditor>().toast.contains("nowhere"),
        "the toast names the target: {}",
        world.resource::<LogicEditor>().toast
    );
    draw(&mut world, vec![]);
    draw(&mut world, vec![]);
    draw(&mut world, vec![key(Key::Delete, Modifiers::NONE)]);
    assert_eq!(depth(&world), before + 1, "Delete is one undo step");
    let doc = &world.resource::<LogicEditor>().doc;
    assert!(!doc.nodes.contains_key(&always), "Always is gone");
    assert!(doc.nodes.contains_key(&door), "the others stay");
}

#[test]
fn a_wire_target_is_a_row_name_an_id_or_a_title_only_one_node_has() {
    let mut world = World::new();
    let [event, door, always] = open_graph(&mut world);
    let doc = world.resource::<LogicEditor>().doc.clone();
    assert_eq!(
        find_node(&doc, &node_row_name(door, &doc.nodes[&door].kind)),
        Some(door)
    );
    assert_eq!(find_node(&doc, &format!("#{always}")), Some(always));
    assert_eq!(find_node(&doc, &format!(" {event} ")), Some(event));
    assert_eq!(
        find_node(&doc, "GO: START"),
        Some(event),
        "a unique title, any case"
    );
    assert_eq!(find_node(&doc, "#999"), None, "no such id");
    assert_eq!(find_node(&doc, ""), None);
    let mut two = doc.clone();
    let dup = two.next_id;
    regia_ai::logic_edit::apply_op(
        &mut two,
        &LogicOp::AddCondition {
            condition: Condition::Always,
            id: None,
            at: None,
        },
    )
    .expect("add");
    let title = panel::node_title(&two.nodes[&dup].kind);
    assert_eq!(find_node(&two, &title), None, "two nodes share `{title}`");
}
