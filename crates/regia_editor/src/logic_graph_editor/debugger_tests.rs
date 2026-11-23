// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph debugger and authoring helpers of ADR-8021 (w12-logic-debugger, parity rows
//! P-177..P-188): palette search, the Variables list, breakpoints (per user, applied to running
//! graphs), Step to the next fired rule, the Watch list and its Live cost, wire pulses, the
//! evaluate box and graph issues in the editor-wide Problems panel. Each panel control is driven
//! through the same function as its agent method, and a keyboard path is shown for each new list.
//! Fixed steps and counters only.

use regia_ai::logic::{LogicGraph, LogicGraphDef, LogicInbox};
use regia_ai::logic_edit::LogicOp;
use regia_ai::state_tree::Literal;
use regia_ai::{BbValue, Blackboard, BtTaskRegistry};
use regia_app::prelude::*;
use serde_json::{Value, json};

use super::breakpoints::{BREAKPOINTS_FILE, BreakpointChange, BreakpointKind};
use super::panel::{self, PanelEdit, WireFrom};
use super::variables::{VarScope, VarType, VariableEdit};
use super::watch::{PULSE_FRAMES, WatchItem};
use super::*;
use crate::canvas_keys::test_input::{self, key, text};
use crate::command::{UndoStack, undo_world};

/// `count` on `go` (guard `Not(Key("stop"))`) increments `local/n`; `blocked` on `go` (guard
/// `Key("stop")`) never fires; `slow` fires every 4 ticks and does nothing.
const GRAPH: &str = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
    (name: "count", event: EntityEvent("go"), condition: Not(Key("stop")), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })]),
    (name: "blocked", event: EntityEvent("go"), priority: 5, condition: Key("stop"), actions: [
        (task: "logic::EmitEvent", set: { "event": Literal(String("never")) })]),
    (name: "slow", event: Timer(every_ticks: 4)),
])"#;

/// A project at a temp root with `assets/logic/gate.logic.ron` open in the editor.
fn project(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, World) {
    let root = regia_test_utils::unique_temp_dir(tag);
    let path = root.join("assets/logic/gate.logic.ron");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, GRAPH).expect("write");
    std::fs::rename(&tmp, &path).expect("rename");
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("opens");
    (root, path, world)
}

fn play_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(regia_ai::AiPlugin);
    app
}

fn spawn(app: &mut App, path: &std::path::Path) -> Entity {
    let text = std::fs::read_to_string(path).expect("reads");
    let def = LogicGraphDef::from_ron(&text).expect("parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
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

fn ticks(app: &App, e: Entity) -> u64 {
    app.world().get::<LogicGraph>(e).map_or(0, LogicGraph::ticks)
}

fn paused(app: &App, e: Entity) -> bool {
    app.world()
        .get::<LogicGraph>(e)
        .is_some_and(LogicGraph::is_paused)
}

fn editor(world: &World) -> &LogicEditor {
    world.resource::<LogicEditor>()
}

/// Any editor agent method by name (the editor's whole list).
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

fn reason(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or("")
        .to_string()
}

/// One headless frame of the panel with `events`; returns the AccessKit tree.
fn panel_frame(
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
) -> egui::accesskit::TreeUpdate {
    test_input::frame(ctx, [1300.0, 900.0], events, |ui| {
        panel::logic_graph_panel(ui, world)
    })
}

/// P-177 (1): the palette search is fuzzy, best first, and a dropped wire filters it to what its
/// pin can connect to; the agent answers the same hits with the ops that add them wired.
#[test]
fn palette_search_is_fuzzy_and_filtered_by_the_dragged_wire() {
    let (root, _path, mut world) = project("logic-dbg-search");
    let doc = editor(&world).doc.clone();
    let tasks = palette_tasks(&world);
    let top = |q: &str, from| {
        panel::palette_search(&doc, &tasks, q, from)
            .first()
            .map(|(g, l, _)| format!("{g}: {l}"))
    };
    assert_eq!(top("tmr", None).as_deref(), Some("Events: Timer"));
    assert_eq!(top("cmp", None).as_deref(), Some("Conditions: Compare"));
    assert_eq!(top("emit", None).as_deref(), Some("Actions: logic::EmitEvent"));
    assert!(panel::palette_search(&doc, &tasks, "zzqx", None).is_empty());
    let count = doc.find_rule("count").expect("count");
    let chain = doc
        .chains()
        .into_iter()
        .find(|c| c.event == count)
        .expect("chain");
    let (cond, action) = (chain.conditions[0], chain.actions[0]);
    let groups = |from| {
        let mut g: Vec<&str> = panel::palette_search(&doc, &tasks, "", Some(from))
            .iter()
            .map(|(g, _, _)| *g)
            .collect();
        g.dedup();
        g
    };
    assert_eq!(groups(WireFrom::Output(count)), vec!["Conditions", "Actions"]);
    assert_eq!(groups(WireFrom::Output(action)), vec!["Actions"]);
    assert_eq!(groups(WireFrom::Input(cond)), vec!["Events", "Conditions"]);
    assert_eq!(
        groups(WireFrom::Input(action)),
        vec!["Events", "Conditions", "Actions"]
    );

    // The agent: the same filter, and the hit's ops add the node wired (one undo step).
    let out = ok(
        &mut world,
        "editor.logic_palette_search",
        json!({ "query": "emit", "from": action, "side": "output" }),
    );
    assert_eq!(out["hits"][0]["label"], "logic::EmitEvent", "{out}");
    assert!(
        out["hits"]
            .as_array()
            .is_some_and(|h| h.iter().all(|h| h["group"] == "Actions"))
    );
    let undo_before = world.resource::<UndoStack>().undo_depth();
    let next = editor(&world).doc.next_id;
    ok(
        &mut world,
        "editor.logic_edit",
        json!({ "ops": out["hits"][0]["ops"] }),
    );
    assert_eq!(
        editor(&world).doc.wires.get(&action),
        Some(&next),
        "added after the action"
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), undo_before + 1);
    let err = call(
        &mut world,
        "editor.logic_palette_search",
        json!({ "query": "x", "from": 999 }),
    )
    .expect_err("node");
    assert_eq!(reason(&err), "unknown_node");
    let err = call(
        &mut world,
        "editor.logic_palette_search",
        json!({ "query": "x", "from": action, "side": "up" }),
    )
    .expect_err("side");
    assert_eq!(reason(&err), "bad_value");

    // Keyboard: Tab to the search box, type, Enter adds the best match.
    let ctx = egui::Context::default();
    panel_frame(&ctx, &mut world, vec![]);
    test_input::tab_to(&ctx, [1300.0, 900.0], "Search nodes", false, 60, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    let nodes = editor(&world).doc.nodes.len();
    panel_frame(&ctx, &mut world, vec![text("tmr")]);
    panel_frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    let doc = &editor(&world).doc;
    assert_eq!(doc.nodes.len(), nodes + 1, "Enter added the best match");
    assert!(
        doc.nodes.values().any(|n| matches!(
            &n.kind,
            regia_ai::logic_edit::LogicNodeKind::Event {
                event: regia_ai::logic::LogicEvent::Timer { .. },
                rule,
                ..
            } if rule != "slow"
        )),
        "a new Timer rule"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// P-177 (1): the Variables list declares, renames (rewriting every reference), retypes and
/// re-defaults locals and declares and renames world boards and entity keys, each one undo step,
/// the panel and the agent through one function; the keyboard declares a local.
#[test]
fn variables_declare_rename_type_and_default_one_undo_step_each() {
    let (root, _path, mut world) = project("logic-dbg-vars");
    let undo_len = |w: &World| w.resource::<UndoStack>().undo_depth();
    let rows = |w: &World| variables::rows(&editor(w).doc);
    assert_eq!(
        rows(&world)
            .iter()
            .map(|r| (r.scope, r.name.as_str(), r.uses))
            .collect::<Vec<_>>(),
        vec![(VarScope::Local, "n", 1), (VarScope::Entity, "stop", 2)]
    );
    // Panel: declare, rename (a reference rewrites), retype, default.
    let mut n = undo_len(&world);
    for change in [
        VariableEdit::Set {
            scope: VarScope::Local,
            name: "hits".into(),
            ty: Some(VarType::Float),
            default: None,
        },
        VariableEdit::Rename {
            scope: VarScope::Local,
            from: "n".into(),
            to: "count_n".into(),
        },
        VariableEdit::Set {
            scope: VarScope::Local,
            name: "count_n".into(),
            ty: Some(VarType::Float),
            default: None,
        },
        VariableEdit::Set {
            scope: VarScope::Local,
            name: "count_n".into(),
            ty: None,
            default: Some(Literal::Float(2.5)),
        },
        VariableEdit::Set {
            scope: VarScope::World,
            name: "gate".into(),
            ty: None,
            default: None,
        },
        VariableEdit::Rename {
            scope: VarScope::Entity,
            from: "stop".into(),
            to: "halt".into(),
        },
    ] {
        panel::apply(&mut world, vec![PanelEdit::Variable(change.clone())], false);
        n += 1;
        assert_eq!(undo_len(&world), n, "{change:?} is one undo step");
    }
    let doc = editor(&world).doc.clone();
    assert_eq!(
        doc.locals
            .iter()
            .map(|l| (l.name.as_str(), l.default.clone()))
            .collect::<Vec<_>>(),
        vec![("count_n", Literal::Float(2.5)), ("hits", Literal::Float(0.0))],
        "the rename keeps the declaration order"
    );
    assert_eq!(doc.world, vec!["gate".to_string()]);
    let ron = doc.to_ron().expect("ron");
    assert!(
        ron.contains("local/count_n") && !ron.contains("local/n\""),
        "{ron}"
    );
    assert!(
        ron.contains("Key(\"halt\")") && !ron.contains("\"stop\""),
        "{ron}"
    );
    assert!(logic_edit::is_valid(&diagnose(&world)), "{:?}", diagnose(&world));
    // Undo walks them back one at a time.
    undo_world(&mut world);
    assert!(
        editor(&world)
            .doc
            .to_ron()
            .expect("ron")
            .contains("Key(\"stop\")")
    );
    // Refusals.
    for (change, code) in [
        (
            VariableEdit::Rename {
                scope: VarScope::Local,
                from: "count_n".into(),
                to: "hits".into(),
            },
            "duplicate_name",
        ),
        (
            VariableEdit::Set {
                scope: VarScope::Local,
                name: "a/b".into(),
                ty: None,
                default: None,
            },
            "bad_name",
        ),
        (
            VariableEdit::Remove {
                scope: VarScope::Local,
                name: "nope".into(),
            },
            "unknown_variable",
        ),
        // `.logic.ron` v2 (ADR-8900) declares entity variables, so an entity Set is no longer refused
        // (`bad_scope` was the v1 limit); a malformed `<board>/<key>` world variable is.
        (
            VariableEdit::Set {
                scope: VarScope::World,
                name: "board/".into(),
                ty: None,
                default: None,
            },
            "bad_name",
        ),
    ] {
        assert_eq!(variables::apply(&mut world, &change).map_err(|e| e.0), Err(code));
    }
    // The agent: the same edits.
    let out = ok(
        &mut world,
        "editor.logic_variable_set",
        json!({ "scope": "local", "name": "armed", "type": "bool", "default": true }),
    );
    assert!(
        out["variables"].as_array().is_some_and(|v| v
            .iter()
            .any(|r| r["name"] == "armed" && r["type"] == "bool" && r["default"] == true)),
        "{out}"
    );
    ok(
        &mut world,
        "editor.logic_variable_set",
        json!({ "scope": "local", "name": "armed", "rename_to": "ready" }),
    );
    ok(
        &mut world,
        "editor.logic_variable_remove",
        json!({ "scope": "local", "name": "ready" }),
    );
    let err = call(
        &mut world,
        "editor.logic_variable_set",
        json!({ "scope": "galaxy", "name": "x" }),
    )
    .expect_err("scope");
    assert_eq!(reason(&err), "bad_scope");
    let listed = ok(&mut world, "editor.logic_variables", json!({}));
    assert!(
        listed["variables"]
            .as_array()
            .is_some_and(|v| v.iter().all(|r| r["name"] != "ready"))
    );

    // Keyboard: the Variables tab, Tab to "New local name", type, Enter declares it.
    world
        .resource_mut::<LogicEditor>()
        .ui
        .show_tab(panel::BottomTab::Variables);
    let ctx = egui::Context::default();
    panel_frame(&ctx, &mut world, vec![]);
    test_input::tab_to(&ctx, [1300.0, 900.0], "New local name", false, 120, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    let before = undo_len(&world);
    panel_frame(&ctx, &mut world, vec![text("score")]);
    panel_frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    assert!(editor(&world).doc.locals.iter().any(|l| l.name == "score"));
    assert_eq!(undo_len(&world), before + 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// P-178 / P-187 (2): a rule breakpoint set in the panel persists per user in
/// `.regia/logic_breakpoints.ron` (never in the asset), pauses the running graph at the end of
/// the tick its rule fires in, lists with enable / disable / remove / go to, and reloads in a fresh
/// editor; an event breakpoint pauses on its inbox event; the agent's `logic.debug` entries stay.
#[test]
fn breakpoints_persist_per_user_pause_the_graph_and_list() {
    let (root, path, mut world) = project("logic-dbg-bp");
    let asset_before = std::fs::read_to_string(&path).expect("asset");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    // An agent-set entry the editor must keep.
    app.world_mut()
        .entity_mut(e)
        .insert(regia_ai::logic::LogicGraphDebug {
            pause_on_rule: Vec::new(),
            pause_on_event: vec!["agent_event".into()],
            disabled: Vec::new(),
        });
    // The gutter dot (panel edit): set on `count`.
    panel::apply(
        &mut world,
        vec![PanelEdit::Breakpoint(
            BreakpointKind::Rule,
            "count".into(),
            BreakpointChange::Enable(true),
        )],
        false,
    );
    let file = root.join(BREAKPOINTS_FILE);
    let saved = std::fs::read_to_string(&file).expect("breakpoint file written");
    assert!(
        saved.contains("assets/logic/gate.logic.ron") && saved.contains("count"),
        "{saved}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("asset"),
        asset_before,
        "the asset is untouched"
    );
    pump_world(&mut world, Some(app.world_mut()));
    let debug = app
        .world()
        .get::<regia_ai::logic::LogicGraphDebug>(e)
        .cloned()
        .expect("debug lists");
    assert_eq!(debug.pause_on_rule, vec!["count".to_string()]);
    assert_eq!(
        debug.pause_on_event,
        vec!["agent_event".to_string()],
        "the agent's entry stays"
    );
    // `go` fires `count`: the graph pauses at the end of that tick and the list shows the hit.
    emit(&mut app, e, "go");
    step(&mut app);
    assert!(paused(&app, e), "the rule breakpoint paused the graph");
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(editor(&world).breakpoints.hits, vec!["rule count".to_string()]);
    let listed = ok(&mut world, "editor.logic_breakpoint_list", json!({}));
    assert_eq!(listed["rules"][0]["name"], "count", "{listed}");
    assert_eq!(listed["hits"], json!(["rule count"]));
    assert_eq!(listed["graph"], "assets/logic/gate.logic.ron");
    // Disable: resumed, `go` no longer pauses.
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "rule", "name": "count", "enabled": false }),
    );
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Resume)], false);
    pump_world(&mut world, Some(app.world_mut()));
    emit(&mut app, e, "go");
    step(&mut app);
    assert!(!paused(&app, e), "a disabled breakpoint does not pause");
    // Go to selects the rule's event node and brings the panel forward.
    let node = listed["rules"][0]["node"].as_u64().expect("node") as u32;
    panel::apply(&mut world, vec![PanelEdit::GoToNode(node)], false);
    assert_eq!(editor(&world).selected, Some(node));
    // An event breakpoint (an event node's menu) pauses on its inbox event.
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "event", "name": "go" }),
    );
    pump_world(&mut world, Some(app.world_mut()));
    emit(&mut app, e, "go");
    step(&mut app);
    assert!(paused(&app, e), "the event breakpoint paused the graph");
    // A fresh editor (another session) reads the same breakpoints from the user file.
    let mut again = World::new();
    again.insert_resource(UndoStack::new());
    again.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    again
        .get_resource_or_insert_with(LogicEditor::default)
        .open(&path)
        .expect("opens");
    let listed = ok(&mut again, "editor.logic_breakpoint_list", json!({}));
    assert_eq!(listed["rules"][0]["enabled"], false, "{listed}");
    assert_eq!(listed["events"][0]["name"], "go");
    // Remove both: the editor's entries leave the play graph, the agent's stay.
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "rule", "name": "count", "remove": true }),
    );
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "event", "name": "go", "remove": true }),
    );
    pump_world(&mut world, Some(app.world_mut()));
    let debug = app
        .world()
        .get::<regia_ai::logic::LogicGraphDebug>(e)
        .cloned()
        .expect("debug lists");
    assert!(debug.pause_on_rule.is_empty());
    assert_eq!(debug.pause_on_event, vec!["agent_event".to_string()]);
    // Refusals.
    let err = call(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "rule", "name": "nope" }),
    )
    .expect_err("rule");
    assert_eq!(reason(&err), "rule_not_found");
    let err = call(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "rule", "name": "count", "remove": true }),
    )
    .expect_err("gone");
    assert_eq!(reason(&err), "not_found");
    let err = call(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "watch", "name": "x" }),
    )
    .expect_err("kind");
    assert_eq!(reason(&err), "bad_value");
    world.resource_mut::<LogicEditor>().new_graph(None);
    let err = call(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "event", "name": "go" }),
    )
    .expect_err("no path");
    assert_eq!(reason(&err), "no_graph_path");
    let _ = std::fs::remove_dir_all(&root);
}

/// (2) keyboard: F9 on the selected node toggles its rule's breakpoint once per frame (egui may run
/// the panel twice in a frame), and the Breakpoints list's Remove button is reached with Tab.
#[test]
fn f9_toggles_the_selected_rule_breakpoint_and_the_list_is_keyboard_reachable() {
    let (root, _path, mut world) = project("logic-dbg-f9");
    let count = editor(&world).doc.find_rule("count").expect("count");
    let chain = editor(&world)
        .doc
        .chains()
        .into_iter()
        .find(|c| c.event == count)
        .expect("chain");
    // The condition node is selected: F9 sets the breakpoint of its rule.
    world.resource_mut::<LogicEditor>().selected = Some(chain.conditions[0]);
    let ctx = egui::Context::default();
    panel_frame(&ctx, &mut world, vec![]);
    panel_frame(&ctx, &mut world, vec![key(egui::Key::F9, egui::Modifiers::NONE)]);
    let listed = ok(&mut world, "editor.logic_breakpoint_list", json!({}));
    assert_eq!(
        listed["rules"],
        json!([{ "name": "count", "enabled": true, "node": count }])
    );
    // Breakpoints tab: Tab to the Remove button, Enter removes it.
    world
        .resource_mut::<LogicEditor>()
        .ui
        .show_tab(panel::BottomTab::Breakpoints);
    panel_frame(&ctx, &mut world, vec![]);
    test_input::tab_to(
        &ctx,
        [1300.0, 900.0],
        "Remove the breakpoint on rule count",
        false,
        120,
        |ui| panel::logic_graph_panel(ui, &mut world),
    );
    panel_frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    let listed = ok(&mut world, "editor.logic_breakpoint_list", json!({}));
    assert_eq!(listed["rules"], json!([]), "{listed}");
    // F9 again sets it again (and only once in the frame).
    panel_frame(&ctx, &mut world, vec![key(egui::Key::F9, egui::Modifiers::NONE)]);
    let listed = ok(&mut world, "editor.logic_breakpoint_list", json!({}));
    assert_eq!(listed["rules"].as_array().map(Vec::len), Some(1), "{listed}");
    let update = panel_frame(&ctx, &mut world, vec![]);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&root);
}

/// P-178 (2): the runtime stops between ticks only, so the finest step past one tick is Step to the
/// next fired rule: ticks in which no rule fires run on, the tick in which one fires is the last,
/// and the editor's temporary all-rules entries leave the debug lists afterwards.
#[test]
fn step_to_next_fired_rule_runs_until_a_rule_fires() {
    let (root, path, mut world) = project("logic-dbg-stepfire");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Pause)], false);
    pump_world(&mut world, Some(app.world_mut()));
    step(&mut app);
    let start = ticks(&app, e);
    // `slow` fires every 4 ticks; nothing else fires without a `go`.
    ok(
        &mut world,
        "editor.logic_edit",
        json!({ "control": "step_to_fire" }),
    );
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(editor(&world).step_fire, Some(e.to_bits()));
    let mut ran = 0;
    while !paused(&app, e) && ran < 16 {
        step(&mut app);
        ran += 1;
    }
    assert!(paused(&app, e), "it paused");
    let fired = app
        .world()
        .get::<regia_ai::logic::LogicGraphStatus>(e)
        .and_then(|s| {
            s.rules
                .iter()
                .find(|r| r.name == "slow")
                .and_then(|r| r.last_fired_tick)
        });
    assert_eq!(fired, Some(ticks(&app, e)), "it stopped on the tick `slow` fired");
    assert!(ticks(&app, e) > start + 1, "ticks without a fire ran on");
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(editor(&world).step_fire, None);
    pump_world(&mut world, Some(app.world_mut()));
    let debug = app
        .world()
        .get::<regia_ai::logic::LogicGraphDebug>(e)
        .cloned()
        .unwrap_or_default();
    assert!(debug.pause_on_rule.is_empty(), "{debug:?}");
    let err = call(&mut world, "editor.logic_edit", json!({ "control": "jump" })).expect_err("bad");
    assert_eq!(reason(&err), "bad_control");
    let _ = std::fs::remove_dir_all(&root);
}

/// P-179 (3), P-188 (5): the Watch list pins variables and condition results; with Live off a
/// running graph costs no snapshot, a pause takes one; Live on refreshes every frame; a condition
/// node's hover value reads the snapshot; the evaluate box answers only while paused.
#[test]
fn watch_live_cost_hover_values_and_evaluate_while_paused() {
    let (root, path, mut world) = project("logic-dbg-watch");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    let doc = editor(&world).doc.clone();
    let count = doc.find_rule("count").expect("count");
    let cond = doc
        .chains()
        .into_iter()
        .find(|c| c.event == count)
        .map(|c| c.conditions[0])
        .expect("condition");
    ok(&mut world, "editor.logic_watch", json!({ "add": "local/n" }));
    ok(&mut world, "editor.logic_watch", json!({ "add": "door" }));
    ok(&mut world, "editor.logic_watch", json!({ "add_condition": cond }));
    let err = call(
        &mut world,
        "editor.logic_watch",
        json!({ "add_condition": count }),
    )
    .expect_err("event node");
    assert_eq!(reason(&err), "not_a_condition");
    // Running, Live off: no snapshot is taken.
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    for _ in 0..5 {
        pump_world(&mut world, Some(app.world_mut()));
    }
    assert_eq!(
        editor(&world).watches.refreshes,
        0,
        "a running graph with Live off costs nothing"
    );
    let err = call(
        &mut world,
        "editor.logic_evaluate",
        json!({ "expression": "Key(\"x\")" }),
    )
    .expect_err("running");
    assert_eq!(reason(&err), "not_paused");
    // Paused: one snapshot, however many frames.
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Pause)], false);
    for _ in 0..5 {
        pump_world(&mut world, Some(app.world_mut()));
    }
    assert_eq!(editor(&world).watches.refreshes, 1, "one copy per pause");
    let out = ok(&mut world, "editor.logic_watch", json!({}));
    assert_eq!(
        out["items"][0]["value"],
        format!("{:?}", BbValue::Int(1)),
        "{out}"
    );
    assert_eq!(out["items"][1]["value"], format!("{:?}", BbValue::Int(5)));
    assert_eq!(out["items"][2]["value"], "true", "Not(Key(\"stop\")) holds");
    assert_eq!(
        watch::hover_value(editor(&world).watches.snapshot.as_ref(), &doc, cond).as_deref(),
        Some(format!("now: true (tick {})", ticks(&app, e)).as_str())
    );
    // Evaluate: the graph's condition syntax over the paused scopes.
    let out = ok(
        &mut world,
        "editor.logic_evaluate",
        json!({ "expression": "All([Compare(key: \"local/n\", op: Gt, value: 0.0), Compare(key: \"door\", op: Eq, value: 5.0)])" }),
    );
    assert_eq!(out["value"], true, "{out}");
    let out = ok(
        &mut world,
        "editor.logic_evaluate",
        json!({ "expression": "Key(\"stop\")" }),
    );
    assert_eq!(out["value"], false);
    for (expression, code) in [("Key(", "parse"), ("Key(\"world/nope/x\")", "unknown_scope")] {
        let err = call(
            &mut world,
            "editor.logic_evaluate",
            json!({ "expression": expression }),
        )
        .expect_err("refused");
        assert_eq!(reason(&err), code, "{expression}");
    }
    // Live on: every frame copies.
    ok(&mut world, "editor.logic_watch", json!({ "live": true }));
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Resume)], false);
    pump_world(&mut world, Some(app.world_mut()));
    let base = editor(&world).watches.refreshes;
    for _ in 0..3 {
        step(&mut app);
        pump_world(&mut world, Some(app.world_mut()));
    }
    assert_eq!(editor(&world).watches.refreshes, base + 3);
    // Unpin; with no play session the snapshot clears.
    ok(
        &mut world,
        "editor.logic_watch",
        json!({ "remove": "door", "remove_condition": cond }),
    );
    assert_eq!(
        editor(&world).watches.items,
        vec![WatchItem::Key("local/n".into())]
    );
    pump_world(&mut world, None);
    assert!(editor(&world).watches.snapshot.is_none());

    // Keyboard: the Watch tab, Tab to "Watch a variable", type, Enter pins it.
    world
        .resource_mut::<LogicEditor>()
        .ui
        .show_tab(panel::BottomTab::Watch);
    let ctx = egui::Context::default();
    panel_frame(&ctx, &mut world, vec![]);
    test_input::tab_to(&ctx, [1300.0, 900.0], "Watch a variable", false, 120, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    panel_frame(&ctx, &mut world, vec![text("door")]);
    panel_frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    assert!(
        editor(&world)
            .watches
            .items
            .contains(&WatchItem::Key("door".into()))
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// (5) keyboard: while paused, the evaluate box is reached with Tab and Enter evaluates.
#[test]
fn the_evaluate_box_is_keyboard_operable_while_paused() {
    let (root, path, mut world) = project("logic-dbg-eval-keys");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Pause)], false);
    pump_world(&mut world, Some(app.world_mut()));
    pump_world(&mut world, Some(app.world_mut()));
    assert!(editor(&world).watches.snapshot.as_ref().is_some_and(|s| s.paused));
    let ctx = egui::Context::default();
    panel_frame(&ctx, &mut world, vec![]);
    test_input::tab_to(&ctx, [1300.0, 900.0], "Evaluate expression", false, 160, |ui| {
        panel::logic_graph_panel(ui, &mut world)
    });
    panel_frame(
        &ctx,
        &mut world,
        vec![text("Compare(key: \"local/n\", op: Eq, value: 1.0)")],
    );
    let update = panel_frame(
        &ctx,
        &mut world,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    );
    assert_eq!(
        editor(&world).watches.evaluation.as_ref().map(|(_, r)| r.clone()),
        Some(Ok(true))
    );
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&root);
}

/// P-180 (4): the wires of a rule that fired pulse for `PULSE_FRAMES` editor frames, once per
/// fired tick, and nothing runs while no graph is watched (no play session).
#[test]
fn firing_wires_pulse_for_n_frames_and_cost_nothing_unwatched() {
    let (root, path, mut world) = project("logic-dbg-pulse");
    for _ in 0..4 {
        pump_world(&mut world, None);
    }
    assert_eq!(editor(&world).pulse.updates, 0, "no play session, no pulse work");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    pump_world(&mut world, Some(app.world_mut()));
    let doc = editor(&world).doc.clone();
    let count = doc.find_rule("count").expect("count");
    let chain = doc
        .chains()
        .into_iter()
        .find(|c| c.event == count)
        .expect("chain");
    let (cond, action) = (chain.conditions[0], chain.actions[0]);
    let pulse = &editor(&world).pulse;
    assert!(pulse.strength(count, cond) > 0.99 && pulse.strength(cond, action) > 0.99);
    let blocked = doc.find_rule("blocked").expect("blocked");
    let blocked_cond = doc.wires.get(&blocked).copied().expect("wired");
    assert_eq!(
        pulse.strength(blocked, blocked_cond),
        0.0,
        "a rejected rule does not pulse"
    );
    // Without another fire it fades: it pulses for PULSE_FRAMES frames in all, then ends.
    for _ in 1..PULSE_FRAMES {
        pump_world(&mut world, Some(app.world_mut()));
    }
    let last = editor(&world).pulse.strength(count, cond);
    assert!(last > 0.0 && last < 0.1, "the last pulsing frame: {last}");
    pump_world(&mut world, Some(app.world_mut()));
    assert_eq!(editor(&world).pulse.strength(count, cond), 0.0);
    assert!(editor(&world).pulse.wires.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// P-185 (6): the open graph's issues appear in the editor-wide Problems panel (its agent answer
/// and the panel's rows) with a link that opens the graph at the node.
#[test]
fn graph_issues_reach_the_editor_problems_with_a_node_link() {
    let (root, _path, mut world) = project("logic-dbg-problems");
    // A stray condition: a dangling_wire issue on its node.
    let created = super::edit(
        &mut world,
        "stray",
        None,
        &[LogicOp::AddCondition {
            condition: regia_ai::state_tree::Condition::Always,
            id: None,
            at: None,
        }],
    )
    .expect("adds");
    let stray = created[0];
    let out = ok(&mut world, "editor.problems_panel", json!({}));
    let rows = out["logic_graph"].as_array().cloned().unwrap_or_default();
    assert!(
        rows.iter().any(|r| r["node"] == stray
            && r["code"] == "dangling_wire"
            && r["path"].as_str().is_some_and(|p| p.ends_with("gate.logic.ron"))),
        "{out}"
    );
    // The panel shows the row; activating it (Enter on the focused row) opens the graph at the node.
    world.resource_mut::<LogicEditor>().selected = None;
    let line = problems::rows(&mut world)
        .into_iter()
        .find(|r| r.node == Some(stray))
        .map(|r| r.line())
        .expect("row");
    let ctx = egui::Context::default();
    let draw = |ui: &mut egui::Ui, world: &mut World| crate::asset_panels::problems_panel(ui, world);
    test_input::frame(&ctx, [1000.0, 700.0], vec![], |ui| draw(ui, &mut world));
    test_input::tab_to(&ctx, [1000.0, 700.0], &line, false, 40, |ui| draw(ui, &mut world));
    test_input::frame(
        &ctx,
        [1000.0, 700.0],
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        |ui| draw(ui, &mut world),
    );
    assert_eq!(editor(&world).selected, Some(stray));
    assert!(
        world
            .get_resource::<crate::editor_layout::LayoutRequests>()
            .is_some_and(|r| format!("{r:?}").contains("LogicGraph")),
        "the Logic Graph panel is brought forward"
    );
    // The agent route of the link.
    world.resource_mut::<LogicEditor>().selected = None;
    ok(&mut world, "editor.logic_goto_node", json!({ "node": stray }));
    assert_eq!(editor(&world).selected, Some(stray));
    let err = call(&mut world, "editor.logic_goto_node", json!({ "node": 999 })).expect_err("unknown");
    assert_eq!(reason(&err), "unknown_node");
    let _ = std::fs::remove_dir_all(&root);
}

/// The new panel parts draw headless with every control named (a11y_audit), in every tab, while
/// paused with watches, breakpoints and an evaluation, and drawing alone edits nothing.
#[test]
fn the_debugger_parts_draw_with_named_controls_in_every_tab() {
    let (root, path, mut world) = project("logic-dbg-draw");
    let mut app = play_app();
    let e = spawn(&mut app, &path);
    step(&mut app);
    emit(&mut app, e, "go");
    step(&mut app);
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "rule", "name": "count" }),
    );
    ok(
        &mut world,
        "editor.logic_breakpoint_set",
        json!({ "kind": "event", "name": "go" }),
    );
    ok(&mut world, "editor.logic_watch", json!({ "add": "local/n" }));
    panel::apply(&mut world, vec![PanelEdit::Control(LiveControl::Pause)], false);
    pump_world(&mut world, Some(app.world_mut()));
    pump_world(&mut world, Some(app.world_mut()));
    panel::apply(&mut world, vec![PanelEdit::Evaluate("Always".into())], false);
    let before = editor(&world).doc.clone();
    let ctx = egui::Context::default();
    for tab in [
        panel::BottomTab::Issues,
        panel::BottomTab::Breakpoints,
        panel::BottomTab::Variables,
        panel::BottomTab::Watch,
    ] {
        world.resource_mut::<LogicEditor>().ui.show_tab(tab);
        let update = panel_frame(&ctx, &mut world, vec![]);
        assert_eq!(
            crate::a11y_audit::unnamed_widgets(&update),
            Vec::<String>::new(),
            "{tab:?}"
        );
    }
    assert_eq!(editor(&world).doc, before, "drawing makes no edit");
    let _ = std::fs::remove_dir_all(&root);
}

/// P-177 / L9 (`.logic.ron` v2, ADR-8900): entity and world variables take a type and a default.
/// An entity key is declared, retyped, re-defaulted, renamed (declaration and references) and
/// removed (the used key stays listed); a `<board>/<key>` world variable declares its board,
/// renames its key and moves with a board rename; each change is one undo step; the agent makes the
/// same edits and the saved definition compiles and reads back.
#[test]
fn entity_and_world_variables_take_a_type_and_a_default() {
    let (root, _path, mut world) = project("logic-dbg-typed-vars");
    let undo_len = |w: &World| w.resource::<UndoStack>().undo_depth();
    let declared = |w: &World| {
        let doc = &editor(w).doc;
        (
            doc.entity_vars
                .iter()
                .map(|v| (v.name.clone(), v.default.clone()))
                .collect::<Vec<_>>(),
            doc.world_vars
                .iter()
                .map(|v| (format!("{}/{}", v.board, v.name), v.default.clone()))
                .collect::<Vec<_>>(),
            doc.world.clone(),
        )
    };
    let mut n = undo_len(&world);
    for change in [
        VariableEdit::Set {
            scope: VarScope::Entity,
            name: "stop".into(),
            ty: Some(VarType::Bool),
            default: None,
        },
        VariableEdit::Set {
            scope: VarScope::Entity,
            name: "stop".into(),
            ty: None,
            default: Some(Literal::Bool(true)),
        },
        VariableEdit::Set {
            scope: VarScope::World,
            name: "score/red".into(),
            ty: Some(VarType::Int),
            default: None,
        },
        VariableEdit::Rename {
            scope: VarScope::Entity,
            from: "stop".into(),
            to: "halt".into(),
        },
        VariableEdit::Rename {
            scope: VarScope::World,
            from: "score/red".into(),
            to: "score/blue".into(),
        },
        VariableEdit::Rename {
            scope: VarScope::World,
            from: "score".into(),
            to: "pts".into(),
        },
    ] {
        panel::apply(&mut world, vec![PanelEdit::Variable(change.clone())], false);
        n += 1;
        assert_eq!(undo_len(&world), n, "{change:?} is one undo step");
    }
    assert_eq!(
        declared(&world),
        (
            vec![("halt".to_string(), Literal::Bool(true))],
            vec![("pts/blue".to_string(), Literal::Int(0))],
            vec!["pts".to_string()],
        )
    );
    let rows = variables::rows(&editor(&world).doc);
    let row = |scope: VarScope, name: &str| {
        rows.iter()
            .find(|r| r.scope == scope && r.name == name)
            .map(|r| (r.default.clone(), r.uses))
    };
    assert_eq!(
        row(VarScope::Entity, "halt"),
        Some((Some(Literal::Bool(true)), 2)),
        "the rename rewrote both references: {rows:?}"
    );
    assert_eq!(row(VarScope::World, "pts/blue"), Some((Some(Literal::Int(0)), 0)));
    // The saved definition is v2, compiles and reads back with the declarations.
    let def = editor(&world).doc.to_def();
    let text = def.to_ron().expect("ron");
    assert!(
        text.contains("entity_vars") && text.contains("world_vars"),
        "{text}"
    );
    let back = LogicGraphDef::from_ron(&text).expect("reads");
    assert_eq!(back, def);
    assert_eq!(back.format_version, regia_ai::logic::LOGIC_FORMAT_VERSION);
    LogicGraph::compile(&back, &BtTaskRegistry::default()).expect("compiles");
    // Refusals change nothing.
    let before = declared(&world);
    for (change, code) in [
        (
            VariableEdit::Rename {
                scope: VarScope::World,
                from: "pts/blue".into(),
                to: "other/blue".into(),
            },
            "bad_name",
        ),
        (
            VariableEdit::Remove {
                scope: VarScope::Entity,
                name: "never".into(),
            },
            "unknown_variable",
        ),
        (
            VariableEdit::Set {
                scope: VarScope::Entity,
                name: "halt".into(),
                ty: Some(VarType::Int),
                default: Some(Literal::String("x".into())),
            },
            "bad_value",
        ),
    ] {
        assert_eq!(variables::apply(&mut world, &change).map_err(|e| e.0), Err(code));
    }
    assert_eq!(declared(&world), before);
    // Removing a declaration keeps the used key listed, untyped.
    panel::apply(
        &mut world,
        vec![PanelEdit::Variable(VariableEdit::Remove {
            scope: VarScope::Entity,
            name: "halt".into(),
        })],
        false,
    );
    assert_eq!(
        variables::rows(&editor(&world).doc)
            .iter()
            .find(|r| r.name == "halt")
            .map(|r| (r.default.clone(), r.uses)),
        Some((None, 2))
    );
    // The agent: the same edits.
    let out = ok(
        &mut world,
        "editor.logic_variable_set",
        json!({ "scope": "entity", "name": "hp", "type": "int", "default": 100 }),
    );
    assert!(
        out["variables"]
            .as_array()
            .is_some_and(|v| v.iter().any(|r| r["scope"] == "entity"
                && r["name"] == "hp"
                && r["type"] == "int"
                && r["default"] == 100)),
        "{out}"
    );
    let out = ok(
        &mut world,
        "editor.logic_variable_set",
        json!({ "scope": "world", "name": "pts/round", "type": "float", "default": 1.5 }),
    );
    assert!(
        out["variables"].as_array().is_some_and(|v| v
            .iter()
            .any(|r| r["name"] == "pts/round" && r["type"] == "float" && r["default"] == 1.5)),
        "{out}"
    );
    ok(
        &mut world,
        "editor.logic_variable_remove",
        json!({ "scope": "world", "name": "pts/round" }),
    );
    assert!(editor(&world).doc.world_vars.iter().all(|v| v.name != "round"));
    let _ = std::fs::remove_dir_all(&root);
}
