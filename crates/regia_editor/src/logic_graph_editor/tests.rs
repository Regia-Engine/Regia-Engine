// SPDX-License-Identifier: MPL-2.0
//! Phase-2 acceptance of spec 102 § 13 on the editor side: the canvas API builds a graph that
//! saves, reopens and runs in the phase-1 runtime (the lane's acceptance), P2.1 (doc ↔ snarl),
//! P2.3 (ops and undo, panel and agent share the op path), P2.4 (issue codes in the panel list and
//! `editor.logic_validate`) and P2.5 (live activity, the three variable scopes, reload on save).
//! Fixed steps and counters only.

use std::collections::BTreeMap;

use regia_ai::logic::{Binding, LogicEvent, LogicGraph, LogicGraphDef, LogicGraphStatus, LogicInbox};
use regia_ai::logic_edit::{CondMode, LogicOp};
use regia_ai::state_tree::{Condition, Literal};
use regia_ai::{BbValue, Blackboard, BtTaskRegistry};
use regia_app::prelude::*;
use serde_json::{Value, json};

use super::panel::{self, PanelEdit};
use super::*;
use crate::command::{UndoStack, undo_world};

fn edit_world() -> World {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world
}

fn gesture(world: &mut World, label: &str, ops: Vec<LogicOp>) {
    panel::apply(world, vec![PanelEdit::Ops(label.into(), None, ops)], false);
}

fn doc(world: &World) -> LogicGraphDoc {
    world.resource::<LogicEditor>().doc.clone()
}

fn lit_s(s: &str) -> Binding {
    Binding::Literal(Literal::String(s.into()))
}

/// The play world: the phase-1 runtime (`AiPlugin`) ticked by hand, one fixed step per call.
fn play_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(regia_ai::AiPlugin);
    app
}

fn spawn_from_file(app: &mut App, path: &std::path::Path) -> Entity {
    let text = std::fs::read_to_string(path).expect("saved file reads");
    let def = LogicGraphDef::from_ron(&text).expect("saved file parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("saved file compiles");
    let mut bb = Blackboard::new();
    bb.insert("door", 5_i64);
    app.world_mut().spawn((graph, bb, LogicInbox::default())).id()
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

fn emit(app: &mut App, e: Entity, name: &str) {
    app.world_mut()
        .get_mut::<LogicInbox>(e)
        .expect("inbox")
        .push(name);
}

fn status(app: &App, e: Entity) -> LogicGraphStatus {
    app.world().get::<LogicGraphStatus>(e).cloned().expect("status")
}

/// Builds the counter graph through panel gestures (palette add, wire drags, inspector edits):
/// rule `count` on `go`, guarded by `Not(Key("stop"))`, increments `local/n`; rule `blocked` on
/// `go` is guarded by `Key("stop")` and never fires.
fn build_with_gestures(world: &mut World) {
    gesture(
        world,
        "add local",
        vec![LogicOp::SetLocal {
            name: "n".into(),
            default: Some(Literal::Int(0)),
        }],
    );
    // Palette: an entity-event head (placed with a free rule name), renamed in the inspector.
    let tasks = palette_tasks(world);
    let palette = panel::palette(&tasks);
    let pick = |label: &str| {
        palette
            .iter()
            .find(|(_, l, _)| l == label)
            .map(|(_, _, op)| op.clone())
            .expect("palette entry")
    };
    for (label, at) in [
        ("Entity event", [0.0, 0.0]),
        ("Key", [240.0, 0.0]),
        ("logic::SetBlackboard", [480.0, 0.0]),
        ("Entity event", [0.0, 160.0]),
        ("Key", [240.0, 160.0]),
        ("logic::EmitEvent", [480.0, 160.0]),
    ] {
        let mut op = pick(label);
        match &mut op {
            LogicOp::AddEvent { rule, at: a, .. } => {
                *rule = format!("rule_{}", world.resource::<LogicEditor>().doc.rules.len() + 1);
                *a = Some(at);
            }
            LogicOp::AddCondition { at: a, .. } | LogicOp::AddAction { at: a, .. } => *a = Some(at),
            _ => {}
        }
        gesture(world, "add node", vec![op]);
    }
    let d = doc(world);
    assert_eq!(d.nodes.len(), 6);
    let ids: Vec<u32> = d.nodes.keys().copied().collect();
    let (e1, c1, a1, e2, c2, a2) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
    for (from, to) in [(e1, c1), (c1, a1), (e2, c2), (c2, a2)] {
        let ops = panel::connect_ops(&doc(world), from, to);
        gesture(world, "connect", ops);
    }
    // Inspector edits.
    let inspector = vec![
        LogicOp::SetEvent {
            node: e1,
            rule: Some("count".into()),
            event: Some(LogicEvent::EntityEvent("go".into())),
            priority: None,
            cooldown_ticks: None,
        },
        LogicOp::SetCondition {
            node: c1,
            condition: Condition::Key("stop".into()),
        },
        LogicOp::SetConditionMode {
            node: c1,
            mode: CondMode::Not,
        },
        LogicOp::SetBinding {
            node: a1,
            key: "key".into(),
            value: Some(lit_s("local/n")),
        },
        LogicOp::SetBinding {
            node: a1,
            key: "op".into(),
            value: Some(lit_s("increment")),
        },
        LogicOp::SetEvent {
            node: e2,
            rule: Some("blocked".into()),
            event: Some(LogicEvent::EntityEvent("go".into())),
            priority: Some(5),
            cooldown_ticks: None,
        },
        LogicOp::SetCondition {
            node: c2,
            condition: Condition::Key("stop".into()),
        },
        LogicOp::SetBinding {
            node: a2,
            key: "event".into(),
            value: Some(lit_s("never")),
        },
    ];
    for op in inspector {
        gesture(world, "inspect", vec![op]);
    }
}

/// The lane acceptance: a graph built through the canvas API saves, reopens identical (graph and
/// layout) in a fresh editor, and the phase-1 runtime executes the saved file.
#[test]
fn p2_acceptance_canvas_built_graph_saves_reopens_and_runs() {
    let dir = regia_test_utils::unique_temp_dir("logic-editor-acceptance");
    let path = dir.join("counter.logic.ron");
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(Some(path.clone()));
    build_with_gestures(&mut world);
    assert!(logic_edit::is_valid(&diagnose(&world)), "{:?}", diagnose(&world));
    panel::apply(&mut world, vec![PanelEdit::Save], false);
    assert!(path.exists(), "{}", world.resource::<LogicEditor>().toast);
    assert!(!world.resource::<LogicEditor>().dirty());

    let mut fresh = edit_world();
    fresh
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("reopens");
    let (a, b) = (doc(&world).to_def(), doc(&fresh).to_def());
    assert_eq!(a, b, "the reopened graph is the saved one");
    assert_eq!(doc(&world).layout(), doc(&fresh).layout(), "positions survive");
    assert_eq!(
        b.rules[0].condition,
        Condition::Not(Box::new(Condition::Key("stop".into())))
    );

    let mut app = play_app();
    let e = spawn_from_file(&mut app, &path);
    emit(&mut app, e, "go");
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    let s = status(&app, e);
    let fires: BTreeMap<_, _> = s.rules.iter().map(|r| (r.name.as_str(), r.fires)).collect();
    assert_eq!(fires, BTreeMap::from([("blocked", 0), ("count", 2)]));
    assert_eq!(
        app.world()
            .get::<LogicGraph>(e)
            .and_then(|g| g.locals().get("n").cloned()),
        Some(BbValue::Int(2))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// P2.1: doc → snarl → doc preserves graph and layout; a moved canvas node reads back moved.
#[test]
fn p2_1_doc_to_snarl_to_doc_preserves_graph_and_layout() {
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(None);
    build_with_gestures(&mut world);
    let d = doc(&world);
    let snarl = panel::build_snarl(&d);
    assert_eq!(panel::doc_from_snarl(&snarl, &d), d);
    let mut snarl = snarl;
    let first = snarl.node_ids().next().map(|(id, _)| id).expect("a node");
    if let Some(info) = snarl.get_node_info_mut(first) {
        info.pos = egui::pos2(-50.0, 75.0);
    }
    let moved = panel::doc_from_snarl(&snarl, &d);
    let node = *snarl.get_node(first).expect("node");
    assert_eq!(moved.nodes[&node].pos, [-50.0, 75.0]);
    assert_eq!(moved.wires, d.wires);
}

fn call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

/// Every `LogicOp` kind once, in an order that applies.
fn every_op() -> Vec<LogicOp> {
    vec![
        LogicOp::SetGraph {
            description: Some("doors".into()),
            seed: Some(3),
            world: Some(vec!["gatehouse".into()]),
        },
        LogicOp::SetLocal {
            name: "n".into(),
            default: Some(Literal::Int(1)),
        },
        LogicOp::AddEvent {
            rule: "a".into(),
            event: LogicEvent::Start,
            priority: 0,
            cooldown_ticks: 0,
            id: Some(1),
            at: Some([0.0, 0.0]),
        },
        LogicOp::AddEvent {
            rule: "b".into(),
            event: LogicEvent::EntityEvent("x".into()),
            priority: 0,
            cooldown_ticks: 0,
            id: Some(2),
            at: None,
        },
        LogicOp::AddCondition {
            condition: Condition::Has("k".into()),
            id: Some(3),
            at: None,
        },
        LogicOp::AddAction {
            task: "logic::EmitEvent".into(),
            set: BTreeMap::new(),
            id: Some(4),
            at: None,
        },
        LogicOp::Connect { from: 1, to: 3 },
        LogicOp::Connect { from: 3, to: 4 },
        LogicOp::MoveNode {
            node: 4,
            at: [500.0, 20.0],
        },
        LogicOp::SetEvent {
            node: 1,
            rule: Some("first".into()),
            event: None,
            priority: Some(2),
            cooldown_ticks: Some(4),
        },
        LogicOp::SetCondition {
            node: 3,
            condition: Condition::All(vec![Condition::Has("k".into()), Condition::Key("z".into())]),
        },
        LogicOp::SetConditionMode {
            node: 3,
            mode: CondMode::Any,
        },
        LogicOp::SetTask {
            node: 4,
            task: "logic::SetBlackboard".into(),
        },
        LogicOp::SetBinding {
            node: 4,
            key: "key".into(),
            value: Some(lit_s("local/n")),
        },
        LogicOp::MoveRule { node: 2, index: 0 },
        LogicOp::Disconnect { from: 3, to: 4 },
        LogicOp::RemoveNode { node: 4 },
    ]
}

/// P2.3: every op through the panel is one undo step, and the same ops through `editor.logic_edit`
/// give the identical document; undo walks every step back.
#[test]
fn p2_3_every_op_is_one_undo_step_and_matches_the_agent_path() {
    let mut panel_world = edit_world();
    panel_world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(None);
    let mut agent_world = edit_world();
    call(&mut agent_world, "editor.logic_open", json!({ "create": true }));
    let ops = every_op();
    let mut docs = vec![doc(&panel_world)];
    for (i, op) in ops.iter().enumerate() {
        let depth = panel_world.resource::<UndoStack>().undo_depth();
        gesture(&mut panel_world, "op", vec![op.clone()]);
        assert_eq!(
            panel_world.resource::<UndoStack>().undo_depth(),
            depth + 1,
            "op {i} ({op:?}) is one undo step: {}",
            panel_world.resource::<LogicEditor>().toast
        );
        let json_op = serde_json::to_value(op).expect("op serializes");
        call(&mut agent_world, "editor.logic_edit", json!({ "ops": [json_op] }));
        assert_eq!(doc(&panel_world), doc(&agent_world), "op {i} ({op:?})");
        docs.push(doc(&panel_world));
    }
    // Undo walks back through every intermediate document.
    for expected in docs.iter().rev().skip(1) {
        assert!(undo_world(&mut panel_world));
        assert_eq!(&doc(&panel_world), expected);
    }
    // A refused gesture changes nothing and adds no step.
    let depth = panel_world.resource::<UndoStack>().undo_depth();
    gesture(&mut panel_world, "bad", vec![LogicOp::Connect { from: 7, to: 8 }]);
    assert_eq!(panel_world.resource::<UndoStack>().undo_depth(), depth);
    assert!(
        panel_world
            .resource::<LogicEditor>()
            .toast
            .contains("unknown_node")
    );
}

/// P2.4: each § 6 issue code shows in the panel's issue list and in `editor.logic_validate`.
#[test]
fn p2_4_each_issue_code_appears_in_the_panel_list_and_logic_validate() {
    let mut deep = "Has(\"a\")".to_string();
    for _ in 0..17 {
        deep = format!("Not({deep})");
    }
    let emit = "(task: \"logic::EmitEvent\", set: { \"event\": Literal(String(\"x\")) })";
    let actions = vec![emit; 17].join(",");
    let cases: Vec<(&str, String)> = vec![
        (
            "duplicate_name",
            "(name: \"r\", event: Start), (name: \"r\", event: Start)".into(),
        ),
        (
            "too_many_actions",
            format!("(name: \"r\", event: Start, actions: [{actions}])"),
        ),
        (
            "condition_too_deep",
            format!("(name: \"r\", event: Start, condition: {deep})"),
        ),
        (
            "unknown_task",
            "(name: \"r\", event: Start, actions: [(task: \"game::Nope\")])".into(),
        ),
        (
            "unknown_scope",
            "(name: \"r\", event: Start, condition: Key(\"zone/x\"))".into(),
        ),
        ("bad_value", "(name: \"r\", event: Timer(every_ticks: 0))".into()),
        (
            "too_many_rules",
            (0..=regia_ai::logic::MAX_RULES)
                .map(|i| format!("(name: \"r{i}\", event: Start)"))
                .collect::<Vec<_>>()
                .join(","),
        ),
    ];
    for (code, rules) in cases {
        let text = format!("(format_version: 1, rules: [{rules}])");
        let mut world = edit_world();
        let validated = call(&mut world, "editor.logic_validate", json!({ "text": text }));
        assert_eq!(validated["issues"][0]["code"], code, "{validated}");
        let dir = regia_test_utils::unique_temp_dir("logic-editor-issues");
        let path = dir.join("case.logic.ron");
        std::fs::write(&path, &text).expect("write case");
        world
            .get_resource_or_insert_with(LogicEditor::default)
            .open(&path)
            .expect("opens");
        let rows = panel::issue_rows(&diagnose(&world));
        assert_eq!(rows.first().map(|r| r.0), Some(code), "{rows:?}");
        let open = call(&mut world, "editor.logic_validate", json!({}));
        assert_eq!(open["issues"][0]["code"], code);
        let _ = std::fs::remove_dir_all(&dir);
    }
    // dangling_wire only exists in the canvas (a file has no unwired nodes).
    let mut world = edit_world();
    call(&mut world, "editor.logic_open", json!({ "create": true }));
    gesture(
        &mut world,
        "add",
        vec![LogicOp::AddAction {
            task: "logic::EmitEvent".into(),
            set: BTreeMap::new(),
            id: None,
            at: None,
        }],
    );
    let rows = panel::issue_rows(&diagnose(&world));
    assert_eq!(rows[0].0, "dangling_wire");
    assert_eq!(rows[0].2, Some(1));
    let open = call(&mut world, "editor.logic_validate", json!({}));
    assert_eq!(open["issues"][0]["code"], "dangling_wire");
}

/// P2.5: during play the watched graph's activity colors its rules (fired / rejected), the
/// variable table shows the local and entity scopes (world scope is phase 3, empty), and a
/// name-preserving save reloads the running graph `kept` with its fire counts.
#[test]
fn p2_5_live_activity_variables_and_reload_on_save() {
    let dir = regia_test_utils::unique_temp_dir("logic-editor-live");
    let path = dir.join("counter.logic.ron");
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(Some(path.clone()));
    build_with_gestures(&mut world);
    super::save(&mut world, None).expect("saves");
    let mut app = play_app();
    let e = spawn_from_file(&mut app, &path);
    // The first save of a new graph reloads nothing (no graph ran an earlier version).
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(
        world.resource::<LogicEditor>().reload.as_ref().map(|r| r.queued),
        Some(0)
    );
    // The first tick only starts the graph (its `Start` event is traced), so the next one holds `go`
    // alone.
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let live = world.resource::<LogicEditor>().live.clone().expect("live view");
    assert_eq!(live.entity, e.to_bits());
    assert_eq!(live.activity.get("count"), Some(&RuleActivity::Fired));
    assert_eq!(live.activity.get("blocked"), Some(&RuleActivity::Rejected));
    assert_eq!(live.actions_started.get("count"), Some(&0));
    assert_eq!(live.events, vec!["go".to_string()]);
    assert!(live.matches_document);
    assert_eq!(
        live.locals,
        vec![("n".to_string(), format!("{:?}", BbValue::Int(1)))]
    );
    assert!(live.entity_vars.iter().any(|(k, _)| k == "door"));
    assert!(live.world_vars.is_empty());

    // A name-preserving edit (priority) saved during play reloads the graph: kept, counts kept.
    let count = world
        .resource::<LogicEditor>()
        .doc
        .find_rule("count")
        .expect("rule");
    gesture(
        &mut world,
        "priority",
        vec![LogicOp::SetEvent {
            node: count,
            rule: None,
            event: None,
            priority: Some(9),
            cooldown_ticks: None,
        }],
    );
    super::save(&mut world, None).expect("saves again");
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(
        world.resource::<LogicEditor>().reload.as_ref().map(|r| r.queued),
        Some(1)
    );
    emit(&mut app, e, "go");
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let reload = world
        .resource::<LogicEditor>()
        .reload
        .clone()
        .expect("reload view");
    assert_eq!(
        (reload.kept, reload.restarted, reload.rejected),
        (1, 0, 0),
        "{reload:?}"
    );
    let s = status(&app, e);
    assert_eq!(
        s.rules.iter().find(|r| r.name == "count").map(|r| r.fires),
        Some(2)
    );
    assert_eq!(
        app.world().get::<LogicGraph>(e).map(|g| g
            .def()
            .rules
            .iter()
            .find(|r| r.name == "count")
            .map(|r| r.priority)),
        Some(Some(9))
    );
    assert!(
        world
            .resource::<LogicEditor>()
            .live
            .as_ref()
            .is_some_and(|l| l.matches_document)
    );
    // With no play session the live view clears.
    pump_world(&mut world, None);
    assert!(world.resource::<LogicEditor>().live.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The live view's pause / step / resume buttons act on the watched play-world graph (spec 102 § 9).
#[test]
fn the_live_view_pauses_steps_and_resumes_the_watched_graph() {
    let dir = regia_test_utils::unique_temp_dir("logic-editor-controls");
    let path = dir.join("counter.logic.ron");
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(Some(path.clone()));
    build_with_gestures(&mut world);
    super::save(&mut world, None).expect("saves");
    let mut app = play_app();
    let e = spawn_from_file(&mut app, &path);
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let ticks = |app: &App| app.world().get::<LogicGraph>(e).map_or(0, LogicGraph::ticks);
    let start = ticks(&app);
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Pause)], false);
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    step(&mut app);
    assert_eq!(ticks(&app), start, "paused");
    pump_world(&mut world, Some(app.world_mut()));
    assert!(
        world
            .resource::<LogicEditor>()
            .live
            .as_ref()
            .is_some_and(|l| !l.running)
    );
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Step)], false);
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    step(&mut app);
    assert_eq!(ticks(&app), start + 1, "one step is one tick");
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Resume)], false);
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    assert_eq!(ticks(&app), start + 2);
    pump_world(&mut world, Some(app.world_mut()));
    assert!(
        world
            .resource::<LogicEditor>()
            .live
            .as_ref()
            .is_some_and(|l| l.running)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `editor.logic_edit { control }` does what the live view's Pause / Step / Resume buttons do
/// (ADR-7774): the same ticks on the watched graph, and a bad value is refused with `bad_control`.
#[test]
fn the_agent_control_pauses_steps_and_resumes_like_the_buttons() {
    let dir = regia_test_utils::unique_temp_dir("logic-editor-agent-controls");
    let path = dir.join("counter.logic.ron");
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(Some(path.clone()));
    build_with_gestures(&mut world);
    super::save(&mut world, None).expect("saves");
    let mut app = play_app();
    let e = spawn_from_file(&mut app, &path);
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let ticks = |app: &App| app.world().get::<LogicGraph>(e).map_or(0, LogicGraph::ticks);
    let start = ticks(&app);
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == "editor.logic_edit")
        .expect("editor.logic_edit");
    let err = handler(&mut world, &json!({ "control": "rewind" })).expect_err("bad control");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("bad_control"),
        "{err:?}"
    );
    call(&mut world, "editor.logic_edit", json!({ "control": "pause" }));
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    step(&mut app);
    assert_eq!(ticks(&app), start, "paused");
    call(&mut world, "editor.logic_edit", json!({ "control": "step" }));
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    step(&mut app);
    assert_eq!(ticks(&app), start + 1, "one step is one tick");
    call(&mut world, "editor.logic_edit", json!({ "control": "resume" }));
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    assert_eq!(ticks(&app), start + 2);
    let _ = std::fs::remove_dir_all(&dir);
}

fn draw_frames(world: &mut World) -> usize {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1100.0, 760.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| panel::logic_graph_panel(ui, world));
    output.textures_delta.clear();
    ctx.tessellate(output.shapes, output.pixels_per_point).len()
}

/// The panel draws headless (egui without a renderer) in every state: nothing open, each node kind
/// selected in the inspector, a graph with issues, and the live view during play with rule activity,
/// variables and a reload result. Drawing alone changes nothing.
#[test]
fn the_panel_draws_headless_in_every_state() {
    let mut world = edit_world();
    assert!(draw_frames(&mut world) > 0, "nothing open");
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .new_graph(None);
    build_with_gestures(&mut world);
    let before = doc(&world);
    let ids: Vec<u32> = before.nodes.keys().copied().collect();
    for id in &ids[..3] {
        world.resource_mut::<LogicEditor>().selected = Some(*id);
        assert!(draw_frames(&mut world) > 0, "node {id} selected");
    }
    assert_eq!(doc(&world), before, "drawing makes no edit");
    gesture(
        &mut world,
        "stray",
        vec![LogicOp::AddAction {
            task: "logic::EmitEvent".into(),
            set: BTreeMap::new(),
            id: None,
            at: None,
        }],
    );
    assert!(draw_frames(&mut world) > 0, "a graph with issues");
    {
        let mut editor = world.resource_mut::<LogicEditor>();
        editor.agents = vec![7];
        editor.live = Some(LiveView {
            entity: 7,
            running: true,
            ticks: 12,
            activity_tick: Some(12),
            events: vec!["go".into()],
            activity: BTreeMap::from([
                ("count".to_string(), RuleActivity::Fired),
                ("blocked".to_string(), RuleActivity::Rejected),
            ]),
            actions_started: BTreeMap::from([("count".to_string(), 0)]),
            fires: BTreeMap::from([("count".to_string(), 3)]),
            locals: vec![("n".into(), "Int(3)".into())],
            entity_vars: vec![("door".into(), "Int(5)".into())],
            world_vars: Vec::new(),
            matches_document: false,
        });
        editor.reload = Some(ReloadView {
            save: 1,
            queued: 1,
            kept: 1,
            ..ReloadView::default()
        });
    }
    assert!(draw_frames(&mut world) > 0, "while playing");
}

/// Spec 102 § 13 P3 in the live view: a graph that declares a world board shows the board's values
/// as `world/` variables, and an `OnSequenceMarker` arm's inbox spelling as the event it received.
#[test]
fn p3_the_live_view_shows_world_boards_and_arm_events() {
    let dir = regia_test_utils::unique_temp_dir("logic-editor-p3");
    let path = dir.join("gate.logic.ron");
    let text = r#"(format_version: 1, world: ["gatehouse"], rules: [
        (name: "creak", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "creak"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/alarm")), "value": Literal(Bool(true)) })]),
    ])"#;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, &path).expect("rename");
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("opens");
    let mut app = play_app();
    let e = spawn_from_file(&mut app, &path);
    step(&mut app);
    emit(&mut app, e, "sequence_marker:door.seq.ron#creak");
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let live = world.resource::<LogicEditor>().live.clone().expect("live view");
    assert_eq!(live.activity.get("creak"), Some(&RuleActivity::Fired));
    assert_eq!(
        live.events,
        vec!["sequence_marker:door.seq.ron#creak".to_string()]
    );
    assert_eq!(
        live.world_vars,
        vec![(
            "gatehouse/alarm".to_string(),
            format!("{:?}", BbValue::Bool(true))
        )]
    );
}

/// Any editor agent method by name (the editor's whole list, as `editor.open_asset` finds them).
fn editor_call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

/// An edit world on a project at `root`, like the shell's: the scene registry, an open level, the
/// undo stack and the editor's agent methods.
fn project_world(root: &std::path::Path) -> World {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world.insert_resource(UndoStack::new());
    let level = crate::level_io::new_level(&mut world);
    world.insert_resource(level);
    crate::agent_ext::install(&mut world);
    world
}

/// w11-logic-graph-finish item 1: Content ▸ New ▸ Logic graph and `editor.create_asset` write the
/// same empty graph at format v1 that the runtime loads; a double click (and so a log link and
/// `editor.open_asset`, one dispatcher) opens it in the Logic Graph panel; one edit and a save
/// later the runtime loads and runs the saved file; `editor.open_asset` reopens it unchanged.
#[test]
fn content_new_logic_graph_opens_edits_saves_and_the_runtime_runs_it() {
    use crate::content::{ContentBrowserState, NewAssetKind};
    let root = regia_test_utils::unique_temp_dir("logic-new-open");
    std::fs::create_dir_all(root.join("assets/logic")).expect("assets");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    let mut world = project_world(&root);

    // The New menu's kind.
    let kind = NewAssetKind::parse("logic_graph").expect("Content ▸ New has a Logic graph kind");
    assert!(NewAssetKind::ALL.contains(&kind), "in the New menu");
    assert_eq!(kind.as_str(), "logic_graph");
    assert_eq!(kind.label(), "Logic graph");
    assert_eq!(kind.extension(), Some("logic.ron"));
    let mut state = ContentBrowserState {
        project_root: Some(root.clone()),
        folder: "assets/logic".to_owned(),
        ..ContentBrowserState::default()
    };
    crate::content::new_asset_from_menu(&mut world, &mut state, kind, None);
    let created = state
        .selected
        .clone()
        .unwrap_or_else(|| panic!("not created: {:?}", state.toast));
    assert!(
        created.starts_with("assets/logic/") && created.ends_with(".logic.ron"),
        "{created}"
    );
    let menu_text = std::fs::read_to_string(root.join(&created)).expect("created file");
    // The runtime loads the new file at the current version, and it is what the panel saves for
    // an empty graph (re-saving it unchanged is byte-identical).
    let def = LogicGraphDef::from_ron(&menu_text).expect("the new file parses");
    assert_eq!(def.format_version, regia_ai::logic::LOGIC_FORMAT_VERSION);
    LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("the new file compiles");
    assert_eq!(
        Some(menu_text.clone()),
        LogicGraphDoc::new().to_ron().ok(),
        "the panel's empty graph"
    );
    // The agent creates the same bytes.
    let answer = editor_call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "logic_graph", "path": "assets/logic/agent.logic.ron" }),
    );
    assert_eq!(answer["path"], json!("assets/logic/agent.logic.ron"));
    assert_eq!(
        std::fs::read_to_string(root.join("assets/logic/agent.logic.ron")).ok(),
        Some(menu_text.clone())
    );
    // editor.asset_kinds lists it in both tables.
    let kinds = editor_call(&mut world, "editor.asset_kinds", json!({}));
    let open_row = kinds["open"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["kind"] == json!("logic_graph")))
        .cloned()
        .unwrap_or_else(|| panic!("editor.asset_kinds open: {}", kinds["open"]));
    assert_eq!(open_row["suffixes"], json!([".logic.ron"]));
    assert_eq!(open_row["panel"], json!("Logic Graph"));
    assert_eq!(
        open_row["state"],
        json!({ "method": "editor.logic_state", "field": "path" })
    );
    assert!(
        kinds["create"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["kind"] == json!("logic_graph"))),
        "{}",
        kinds["create"]
    );
    // An Output Log link opens it in its editor; the layout sidecar is not a graph.
    assert_eq!(
        crate::log_links::asset_action(&created),
        crate::log_links::AssetAction::Editor
    );
    let links = crate::log_links::parse_links(&format!("failed to load {created}"));
    assert_eq!(
        links.iter().map(|l| l.path()).collect::<Vec<_>>(),
        vec![created.as_str()],
        "an Output Log line links the file"
    );
    assert_eq!(
        crate::asset_open::kind_of(&created).map(|k| k.name),
        Some("logic_graph")
    );
    assert!(crate::asset_open::kind_of("assets/logic/gate.logic.layout.ron").is_none());

    // The double click opens it in the Logic Graph panel.
    let mut level = world.resource::<crate::level_io::OpenLevel>().clone();
    let toast = crate::asset_open::open_from_browser(&mut world, &mut state, &mut level, &created);
    assert_eq!(toast, None);
    world.insert_resource(level);
    let shown = editor_call(&mut world, "editor.logic_state", json!({}));
    let tail = created.trim_start_matches("assets/");
    assert!(
        shown["path"]
            .as_str()
            .is_some_and(|p| p.replace('\\', "/").ends_with(tail)),
        "{shown}"
    );
    assert_eq!(shown["dirty"], json!(false));
    let requests = world
        .get_resource::<crate::editor_layout::LayoutRequests>()
        .map(|r| format!("{r:?}"))
        .unwrap_or_default();
    assert!(requests.contains("LogicGraph"), "{requests}");

    // One edit (a palette add and a wire, as gestures): on `go`, increment `local/n`.
    gesture(
        &mut world,
        "count",
        vec![
            LogicOp::SetLocal {
                name: "n".into(),
                default: Some(Literal::Int(0)),
            },
            LogicOp::AddEvent {
                rule: "count".into(),
                event: LogicEvent::EntityEvent("go".into()),
                priority: 0,
                cooldown_ticks: 0,
                id: Some(1),
                at: Some([0.0, 0.0]),
            },
            LogicOp::AddAction {
                task: "logic::SetBlackboard".into(),
                set: BTreeMap::from([
                    ("key".to_string(), lit_s("local/n")),
                    ("op".to_string(), lit_s("increment")),
                ]),
                id: Some(2),
                at: Some([240.0, 0.0]),
            },
            LogicOp::Connect { from: 1, to: 2 },
        ],
    );
    assert_eq!(
        editor_call(&mut world, "editor.logic_state", json!({}))["dirty"],
        json!(true)
    );
    let saved = save(&mut world, None).unwrap_or_else(|e| panic!("save: {e:?}"));
    assert!(saved.ends_with(tail), "{}", saved.display());

    // The runtime loads and runs the saved file.
    let mut app = play_app();
    let e = spawn_from_file(&mut app, &root.join(&created));
    emit(&mut app, e, "go");
    step(&mut app);
    let st = status(&app, e);
    assert_eq!(
        st.rules.iter().find(|r| r.name == "count").map(|r| r.fires),
        Some(1),
        "{st:?}"
    );

    // editor.open_asset reopens the saved graph unchanged.
    let reopened = editor_call(&mut world, "editor.open_asset", json!({ "path": created }));
    assert_eq!(reopened["kind"], json!("logic_graph"));
    assert_eq!(reopened["queued"], json!(false));
    assert_eq!(reopened["reply"]["rules"], json!(1));
    let state_after = editor_call(&mut world, "editor.logic_state", json!({}));
    assert_eq!(state_after["dirty"], json!(false));
    assert_eq!(state_after["rules"][0]["name"], json!("count"));
    let _ = std::fs::remove_dir_all(&root);
}

// ---- w13 (P-183 find usages, P-181 debug-instance filter) ----

/// Writes `text` to `path` atomically (tmp + rename), creating the directory.
fn write_file(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// `alarm` is emitted by `guard` and `tower`, received by `bell`; `door` (bare and `entity/`) is
/// read by `guard`'s condition, written by `tower`, watched by `bell`; `Speed` is set by `tower`'s
/// registered task, declared and read by the animation graph and set in code.
const GUARD: &str = r#"(format_version: 1, rules: [
    (name: "spot", event: EntityEvent("seen"), condition: Compare(key: "door", op: Gt, value: 0.0), actions: [
        (task: "logic::EmitEvent", set: { "event": Literal(String("alarm")) })]),
])"#;
const TOWER: &str = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
    (name: "ring", event: Timer(every_ticks: 10), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("entity/door")), "value": Literal(Int(0)) }),
        (task: "logic::EmitEvent", set: { "event": Literal(String("alarm")) }),
        (task: "game::Locomotion", set: { "Speed": BbRef("local/n") })]),
])"#;
const BELL: &str = r#"(format_version: 1, rules: [
    (name: "toll", event: EntityEvent("alarm"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/tolls")), "op": Literal(String("increment")) })]),
    (name: "creak", event: BlackboardChanged(key: "door")),
])"#;
const QUIET: &str = r#"(format_version: 1, rules: [(name: "idle", event: Start)])"#;
const WALK: &str = r#"(format_version: 1, parameters: [("Speed", Float(0.0)), ("Unused", Bool(false))],
    root: BlendSpace1D((space: (param: "Speed", samples: [(value: 0.0, clip: "Idle"), (value: 2.0, clip: "Walk")]), sync_group: None)))"#;
const CODE: &str =
    "fn drive(animator: &mut Animator) {\n    animator.parameters.set_float(\"Speed\", 3.2);\n}\n";

/// A project with the graphs above, the animation graph and a code file, and an edit world on it.
fn usage_project(tag: &str) -> (std::path::PathBuf, World) {
    let root = regia_test_utils::unique_temp_dir(tag);
    write_file(&root.join("assets/logic/guard.logic.ron"), GUARD);
    write_file(&root.join("assets/logic/tower.logic.ron"), TOWER);
    write_file(&root.join("assets/logic/bell.logic.ron"), BELL);
    write_file(&root.join("assets/logic/quiet.logic.ron"), QUIET);
    write_file(&root.join("assets/logic/broken.logic.ron"), "not ron");
    write_file(&root.join("assets/anim/walk.animgraph.ron"), WALK);
    write_file(&root.join("src/drive.rs"), CODE);
    let mut world = edit_world();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    (root, world)
}

/// `(file under the root, how, rule)` of each row.
fn usage_rows(answer: &Value, root: &std::path::Path) -> Vec<(String, String, Option<String>)> {
    let prefix = format!("{}/", root.display());
    answer["usages"]
        .as_array()
        .expect("usages")
        .iter()
        .map(|u| {
            (
                u["path"]
                    .as_str()
                    .unwrap_or("")
                    .trim_start_matches(&prefix)
                    .to_string(),
                u["how"].as_str().unwrap_or("").to_string(),
                u["rule"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn row(path: &str, how: &str, rule: Option<&str>) -> (String, String, Option<String>) {
    (path.to_string(), how.to_string(), rule.map(str::to_string))
}

/// P-183 (1): an event's usages list every graph that emits it and every graph that receives it,
/// with the rule and node; a graph that names it nowhere is not listed. The node's context menu
/// offers the same search.
#[test]
fn find_usages_lists_every_graph_that_emits_the_event() {
    let (root, mut world) = usage_project("logic-usages-event");
    let answer = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "event": "alarm" }),
    );
    assert_eq!(
        usage_rows(&answer, &root),
        vec![
            row("assets/logic/bell.logic.ron", "receives", Some("toll")),
            row("assets/logic/guard.logic.ron", "emits", Some("spot")),
            row("assets/logic/tower.logic.ron", "emits", Some("ring")),
        ],
        "{answer}"
    );
    // The guard's emitting node is the action of `spot`, as the panel opens the file.
    let guard = LogicGraphDoc::from_ron(GUARD, &BTreeMap::new()).expect("guard");
    let spot = guard.chain(guard.find_rule("spot").expect("spot"));
    assert_eq!(answer["usages"][1]["node"], json!(spot.actions[0]));
    // The context menu's queries of that node: the event it emits.
    assert_eq!(
        usages::queries_for_node(&guard, spot.actions[0]),
        vec![usages::UsageQuery::new(usages::UsageKind::Event, "alarm")]
    );
    // The panel's menu item runs the same search and shows the Usages tab.
    panel::apply(
        &mut world,
        vec![PanelEdit::FindUsages(usages::UsageQuery::new(
            usages::UsageKind::Event,
            "alarm",
        ))],
        false,
    );
    let last = world
        .resource::<usages::LogicUsages>()
        .last
        .clone()
        .expect("answer kept");
    assert_eq!(last.usages.len(), 3);
    assert_eq!(world.resource::<LogicEditor>().ui.tab(), panel::BottomTab::Usages);
    // An event nobody uses answers an empty list; bad requests are refused with stable codes.
    let none = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "event": "nobody" }),
    );
    assert_eq!(none["usages"], json!([]));
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == "editor.logic_find_usages")
        .expect("method");
    let reason = |e: regia_agent::protocol::RpcError| {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(
        reason(handler(&mut world, &json!({})).expect_err("none")),
        "missing_param"
    );
    assert_eq!(
        reason(handler(&mut world, &json!({ "event": "a", "key": "b" })).expect_err("two")),
        "bad_value"
    );
    assert_eq!(
        reason(handler(&mut world, &json!({ "event": "alarm", "open": 99 })).expect_err("row")),
        "usage_missing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// P-183 (2): a key's usages find its reads (conditions, `BbRef`), writes (`SetBlackboard`,
/// whichever spelling: `door` and `entity/door` are one key) and watches; a graph local is per
/// graph (`local/n` is declared by `tower` only and read there by a binding).
#[test]
fn find_usages_of_a_key_finds_reads_and_writes() {
    let (root, mut world) = usage_project("logic-usages-key");
    let answer = editor_call(&mut world, "editor.logic_find_usages", json!({ "key": "door" }));
    assert_eq!(
        usage_rows(&answer, &root),
        vec![
            row("assets/logic/bell.logic.ron", "watches", Some("creak")),
            row("assets/logic/guard.logic.ron", "reads", Some("spot")),
            row("assets/logic/tower.logic.ron", "writes", Some("ring")),
        ],
        "{answer}"
    );
    let same = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "key": "entity/door" }),
    );
    assert_eq!(same["usages"], answer["usages"], "both spellings name one key");
    let local = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "key": "local/n" }),
    );
    assert_eq!(
        usage_rows(&local, &root),
        vec![
            row("assets/logic/tower.logic.ron", "declares", None),
            row("assets/logic/tower.logic.ron", "reads", Some("ring")),
        ],
        "{local}"
    );
    // The keys a condition node names are its context-menu queries.
    let guard = LogicGraphDoc::from_ron(GUARD, &BTreeMap::new()).expect("guard");
    let spot = guard.chain(guard.find_rule("spot").expect("spot"));
    assert_eq!(
        usages::queries_for_node(&guard, spot.conditions[0]),
        vec![usages::UsageQuery::new(usages::UsageKind::Key, "door")]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// P-183 (3): an animation parameter's usages: the animation graph that declares and reads it,
/// the logic action that sets it through a registered task, and the code line that sets it.
#[test]
fn find_usages_of_an_anim_parameter_lists_graphs_logic_and_code() {
    let (root, mut world) = usage_project("logic-usages-param");
    let answer = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "param": "Speed" }),
    );
    assert_eq!(
        usage_rows(&answer, &root),
        vec![
            row("assets/anim/walk.animgraph.ron", "declares", None),
            row("assets/anim/walk.animgraph.ron", "reads", None),
            row("assets/logic/tower.logic.ron", "sets", Some("ring")),
            row("src/drive.rs", "sets", None),
        ],
        "{answer}"
    );
    assert_eq!(answer["usages"][3]["line"], json!(2));
    assert_eq!(answer["usages"][3]["doc"], json!("code"));
    // A code row has no editor here: following it says so.
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == "editor.logic_find_usages")
        .expect("method");
    let err = handler(&mut world, &json!({ "param": "Speed", "open": 3 })).expect_err("code");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("no_editor")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The index is built on demand and cached by content hash: a second search parses nothing, an
/// edited file is parsed again (and its new uses show), a deleted file leaves the index.
#[test]
fn the_usage_index_reparses_only_changed_files() {
    let (root, mut world) = usage_project("logic-usages-cache");
    let first = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "event": "alarm" }),
    );
    assert_eq!(first["files"], json!(7));
    assert_eq!(first["parsed"], json!(7));
    let again = editor_call(&mut world, "editor.logic_find_usages", json!({ "key": "door" }));
    assert_eq!(again["parsed"], json!(0), "nothing changed: nothing parsed");
    write_file(
        &root.join("assets/logic/quiet.logic.ron"),
        r#"(format_version: 1, rules: [(name: "idle", event: Start, actions: [
            (task: "logic::EmitEvent", set: { "event": Literal(String("alarm")) })])])"#,
    );
    std::fs::remove_file(root.join("assets/logic/broken.logic.ron")).expect("rm");
    let changed = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "event": "alarm" }),
    );
    assert_eq!(changed["parsed"], json!(1), "{changed}");
    assert_eq!(changed["files"], json!(6));
    assert_eq!(changed["usages"].as_array().map(Vec::len), Some(4), "{changed}");
    assert_eq!(world.resource::<usages::LogicUsages>().parses, 8);
    let _ = std::fs::remove_dir_all(&root);
}

/// A Usages row opens the graph at the node (selected, framed, the panel brought forward); another
/// graph's unsaved work is never replaced (`unsaved_changes`), and a row of the open graph only
/// moves the selection.
#[test]
fn a_usage_row_opens_the_graph_at_the_node() {
    let (root, mut world) = usage_project("logic-usages-open");
    let answer = editor_call(
        &mut world,
        "editor.logic_find_usages",
        json!({ "event": "alarm", "open": 2 }),
    );
    let node = answer["usages"][2]["node"].as_u64().expect("node") as NodeId;
    assert_eq!(answer["opened"]["node"], json!(node));
    {
        let editor = world.resource::<LogicEditor>();
        assert!(
            editor
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("assets/logic/tower.logic.ron")),
            "{:?}",
            editor.path
        );
        assert_eq!(editor.selected, Some(node));
    }
    // Unsaved work in tower: the bell row is refused, tower's own row only selects.
    gesture(
        &mut world,
        "add local",
        vec![LogicOp::SetLocal {
            name: "extra".into(),
            default: Some(Literal::Int(1)),
        }],
    );
    panel::apply(&mut world, vec![PanelEdit::OpenUsage(0)], false);
    {
        let editor = world.resource::<LogicEditor>();
        assert!(editor.toast.contains("unsaved_changes"), "{}", editor.toast);
        assert!(
            editor
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("tower.logic.ron"))
        );
    }
    panel::apply(&mut world, vec![PanelEdit::OpenUsage(2)], false);
    assert_eq!(world.resource::<LogicEditor>().selected, Some(node));
    assert!(
        world.resource::<LogicEditor>().dirty(),
        "the open graph kept its edit"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The keyboard path (accessibility bar): with a node selected, Tab reaches the inspector's Find
/// usages button, Enter runs it, Tab reaches the Usages row, Enter opens it; every control of the
/// tab is named.
#[test]
fn find_usages_is_keyboard_operable_from_the_inspector() {
    use crate::canvas_keys::test_input::{self, key};
    let (root, mut world) = usage_project("logic-usages-keys");
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&root.join("assets/logic/guard.logic.ron"))
        .expect("opens");
    let guard = doc(&world);
    let spot = guard.chain(guard.find_rule("spot").expect("spot"));
    world.resource_mut::<LogicEditor>().selected = Some(spot.actions[0]);
    let ctx = egui::Context::default();
    let size = [1300.0, 900.0];
    let frame = |ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>| {
        test_input::frame(ctx, size, events, |ui| panel::logic_graph_panel(ui, world))
    };
    frame(&ctx, &mut world, vec![]);
    test_input::tab_to(&ctx, size, "Find usages of event \"alarm\"", false, 200, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    let last = world
        .resource::<usages::LogicUsages>()
        .last
        .clone()
        .expect("the button ran the search");
    assert_eq!(last.usages.len(), 3);
    assert_eq!(world.resource::<LogicEditor>().ui.tab(), panel::BottomTab::Usages);
    frame(&ctx, &mut world, vec![]);
    let bell_row = last.usages[0].row_text(&root);
    test_input::tab_to(&ctx, size, &bell_row, false, 300, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    let editor = world.resource::<LogicEditor>();
    assert!(
        editor
            .path
            .as_deref()
            .is_some_and(|p| p.ends_with("bell.logic.ron")),
        "{:?} ({})",
        editor.path,
        editor.toast
    );
    assert_eq!(editor.selected, last.usages[0].node);
    let update = frame(&ctx, &mut world, vec![]);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&root);
}

/// P-181: two running instances of one graph; the debug-instance filter watches the second, and
/// a rule breakpoint pauses only that instance (the first runs on); moving the filter to the first
/// moves the breakpoint with it.
#[test]
fn the_instance_filter_watches_the_second_of_two_instances() {
    const COUNTER: &str = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
        (name: "count", event: EntityEvent("go"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })]),
    ])"#;
    let root = regia_test_utils::unique_temp_dir("logic-instance-filter");
    let path = root.join("assets/logic/counter.logic.ron");
    write_file(&path, COUNTER);
    let mut world = edit_world();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("opens");
    let mut app = play_app();
    let a = spawn_from_file(&mut app, &path);
    let b = spawn_from_file(&mut app, &path);
    // First and second as the instance pick lists them (by entity bits).
    let (first, second) = if a.to_bits() < b.to_bits() { (a, b) } else { (b, a) };
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(
        world.resource::<LogicEditor>().agents,
        vec![first.to_bits(), second.to_bits()]
    );
    // Unfiltered, the live view follows the first instance running the file.
    assert_eq!(
        world.resource::<LogicEditor>().live.as_ref().map(|l| l.entity),
        Some(first.to_bits())
    );
    // The filter picks the second (the panel's instance combo).
    panel::apply(&mut world, vec![PanelEdit::Watch(Some(second.to_bits()))], false);
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(
        world.resource::<LogicEditor>().live.as_ref().map(|l| l.entity),
        Some(second.to_bits())
    );
    panel::apply(
        &mut world,
        vec![PanelEdit::Breakpoint(
            super::breakpoints::BreakpointKind::Rule,
            "count".into(),
            super::breakpoints::BreakpointChange::Enable(true),
        )],
        false,
    );
    pump_world(&mut world, Some(app.world_mut()));
    let paused = |app: &App, e: Entity| {
        app.world()
            .get::<LogicGraph>(e)
            .is_some_and(LogicGraph::is_paused)
    };
    emit(&mut app, first, "go");
    emit(&mut app, second, "go");
    step(&mut app);
    if cfg!(debug_assertions) {
        assert!(paused(&app, second), "the breakpoint hit the watched instance");
        assert!(!paused(&app, first), "the other instance runs on");
        pump_world(&mut world, Some(app.world_mut()));
        let editor = world.resource::<LogicEditor>();
        assert_eq!(editor.breakpoints.hits, vec!["rule count".to_string()]);
        assert_eq!(editor.live.as_ref().map(|l| l.entity), Some(second.to_bits()));
        // The filter moves to the first: the breakpoint follows it.
        panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Resume)], false);
        pump_world(&mut world, Some(app.world_mut()));
        panel::apply(&mut world, vec![PanelEdit::Watch(Some(first.to_bits()))], false);
        pump_world(&mut world, Some(app.world_mut()));
        let debug = |app: &App, e: Entity| {
            app.world()
                .get::<regia_ai::logic::LogicGraphDebug>(e)
                .map(|d| d.pause_on_rule.clone())
                .unwrap_or_default()
        };
        assert_eq!(debug(&app, first), vec!["count".to_string()]);
        assert_eq!(debug(&app, second), Vec::<String>::new());
        // Resume (the watched second, before the filter moved) and fire both again: only the
        // first pauses now.
        assert!(!paused(&app, second));
        emit(&mut app, first, "go");
        emit(&mut app, second, "go");
        step(&mut app);
        assert!(paused(&app, first));
        assert!(!paused(&app, second));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Review 3616 L3: a breakpoint file this editor cannot read (another format version) is never
/// overwritten: a toggle is refused with `save_failed` and the file stays byte for byte.
#[test]
fn an_unreadable_breakpoint_file_is_never_overwritten() {
    let root = regia_test_utils::unique_temp_dir("logic-bp-newer");
    let path = root.join("assets/logic/gate.logic.ron");
    write_file(&path, GUARD);
    let file = root.join(super::breakpoints::BREAKPOINTS_FILE);
    let newer =
        "(format_version: 2, graphs: {\"assets/other.logic.ron\": (rules: [(name: \"x\", enabled: true)])})";
    write_file(&file, newer);
    let mut world = edit_world();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("opens");
    let out = super::set_breakpoint(
        &mut world,
        super::breakpoints::BreakpointKind::Rule,
        "spot",
        super::breakpoints::BreakpointChange::Toggle,
    );
    assert_eq!(out.map_err(|e| e.code), Err("save_failed"));
    assert_eq!(std::fs::read_to_string(&file).expect("file"), newer, "untouched");
    assert!(
        world.resource::<LogicEditor>().toast.contains("save_failed"),
        "{}",
        world.resource::<LogicEditor>().toast
    );
    let _ = std::fs::remove_dir_all(&root);
}
