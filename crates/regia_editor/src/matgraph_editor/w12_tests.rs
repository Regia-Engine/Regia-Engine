// SPDX-License-Identifier: MPL-2.0
//! w12 parity acceptance tests (ADR-7851): the Parameters panel, reparent and the hierarchy,
//! per-profile stats, the tier preview, the node menu's new items and their agent twins
//! through the editor's agent dispatcher, and the keyboard route of each new control.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_matgraph::{MaterialGraph, ParamValue};
use serde_json::{Value, json};

use super::*;
use crate::canvas_keys::test_input::{frame, key, tab_to};
use crate::command::UndoStack;

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("dir");
    }
    std::fs::write(path, text).expect("write");
}

const GRAPH: &str = r#"(
    format_version: 4,
    name: "wall",
    params: [
        (name: "tint", value: Color((0.8, 0.2, 0.1))),
        (name: "rough", value: Float(0.5)),
    ],
    textures: [(name: "albedo", path: "", color_space: Srgb)],
    switches: [(name: "wet", default: false)],
    nodes: {
        "tint": Param("tint"),
        "r": Param("rough"),
        "half": Float(0.5),
        "mix": Mul("tint", "half"),
        "base": Switch(switch: "wet", on: "tint", off: "mix"),
    },
    output: (base_color: Some("base"), roughness: Some("r")),
)"#;

const OTHER: &str = r#"(
    format_version: 4,
    name: "floor",
    params: [(name: "tint", value: Color((0.1, 0.1, 0.1)))],
    nodes: { "tint": Param("tint"), "half": Float(0.5) },
    output: (base_color: Some("tint"), roughness: Some("half")),
)"#;

fn project() -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("matgraph-w12");
    write(&root.join("assets/wall.matgraph.ron"), GRAPH);
    write(&root.join("assets/floor.matgraph.ron"), OTHER);
    write(
        &root.join("assets/wall_red.matinst.ron"),
        r#"(format_version: 2, parent: "wall.matgraph.ron",
            params: { "tint": Color((1.0, 0.0, 0.0)), "rough": Float(0.9) },
            switches: { "wet": true })"#,
    );
    root
}

/// An app with the agent plugin and the editor's methods; `call` sends one request through the
/// inbox and returns the response.
struct Dispatch {
    app: crate::engine::app::App,
    sender: crossbeam_channel::Sender<regia_agent::server::Inbound>,
}

impl Dispatch {
    fn new(root: &Path) -> Self {
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        let (sender, inbox) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(inbox));
        Self { app, sender }
    }

    fn call(&mut self, method: &str, params: Value) -> regia_agent::protocol::Response {
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        self.app.update();
        answer.try_recv().expect("dispatch answers in one frame")
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    }

    fn code(&mut self, method: &str, params: Value) -> String {
        let response = self.call(method, params);
        let error = response.error.unwrap_or_else(|| panic!("{method} must fail"));
        error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }
}

/// Every w12 method answers through the dispatcher: params list/set/reorder/remove (with undo),
/// cost for every profile and one, find usages by name and from a node, the hierarchy, and an
/// instance reparent that keeps and drops overrides in one undo step.
#[test]
fn w12_methods_answer_through_the_agent_dispatcher() {
    let root = project();
    let mut d = Dispatch::new(&root);
    d.ok(
        "editor.matgraph_open",
        json!({ "path": "assets/wall.matgraph.ron" }),
    );

    let listed = d.ok("editor.matgraph_params", json!({}));
    let names: Vec<_> = listed["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| (r["kind"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        names,
        vec![
            ("param", "tint"),
            ("param", "rough"),
            ("texture", "albedo"),
            ("switch", "wet")
        ]
    );
    let set = d.ok(
        "editor.matgraph_params",
        json!({ "op": "set", "name": "rough", "value": 0.25 }),
    );
    assert_eq!(set["added"], false);
    assert_eq!(set["rows"][1]["default"], json!({ "Float": 0.25 }));
    let added = d.ok(
        "editor.matgraph_params",
        json!({ "op": "set", "kind": "param", "name": "metal", "value": { "Float": 1.0 } }),
    );
    assert_eq!(added["added"], true);
    assert_eq!(added["rows"][2]["name"], "metal");
    let moved = d.ok(
        "editor.matgraph_params",
        json!({ "op": "reorder", "kind": "param", "name": "metal", "to": 0 }),
    );
    assert_eq!(moved["rows"][0]["name"], "metal");
    assert_eq!(moved["ok"], true, "a reorder still compiles");
    let removed = d.ok(
        "editor.matgraph_params",
        json!({ "op": "remove", "kind": "param", "name": "metal" }),
    );
    assert_eq!(removed["readers"], json!([]));
    assert_eq!(
        d.code(
            "editor.matgraph_params",
            json!({ "op": "reorder", "name": "x", "kind": "param", "to": 0 })
        ),
        "matgraph_params_invalid"
    );
    assert_eq!(
        d.code("editor.matgraph_params", json!({ "op": "explode" })),
        "matgraph_params_invalid"
    );
    d.ok("editor.undo", json!({}));
    d.ok("editor.undo", json!({}));
    let back = d.ok("editor.matgraph_params", json!({}));
    assert_eq!(
        back["rows"][2]["name"], "metal",
        "undo brings the removed row back in its slot"
    );

    let all = d.ok("editor.matgraph_cost", json!({}));
    let profiles = all["profiles"].as_array().expect("profiles");
    assert_eq!(profiles.len(), super::cost::profiles().len());
    assert!(
        profiles
            .iter()
            .all(|p| p["error"].is_null() && p["instructions"].as_u64() > Some(0))
    );
    let one = d.ok("editor.matgraph_cost", json!({ "profile": "mobile_low" }));
    assert_eq!(one["profiles"][0]["profile"], "MobileLow");
    assert_eq!(
        d.code("editor.matgraph_cost", json!({ "profile": "Cinematic" })),
        "matgraph_profile_unknown"
    );

    let by_name = d.ok(
        "editor.matgraph_find_usages",
        json!({ "kind": "param", "name": "tint" }),
    );
    let paths: Vec<_> = by_name["usages"]
        .as_array()
        .expect("usages")
        .iter()
        .map(|u| {
            u["path"]
                .as_str()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(
        paths,
        ["floor.matgraph.ron", "wall.matgraph.ron", "wall_red.matinst.ron"]
    );
    let by_node = d.ok("editor.matgraph_find_usages", json!({ "node": "half" }));
    assert_eq!(by_node["kind"], "constant");
    // Both graphs hold the constant; the instance inherits it from `wall`.
    assert_eq!(by_node["usages"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        d.code("editor.matgraph_find_usages", json!({ "node": "mix" })),
        "matgraph_usage_invalid"
    );

    let tree = d.ok("editor.matgraph_hierarchy", json!({}));
    let wall = tree["graphs"]
        .as_array()
        .expect("graphs")
        .iter()
        .find(|g| g["name"] == "wall")
        .expect("wall");
    assert_eq!(wall["instances"].as_array().map(Vec::len), Some(1));

    assert_eq!(
        d.code(
            "editor.matgraph_instance_reparent",
            json!({ "parent": "assets/floor.matgraph.ron" })
        ),
        "matgraph_reparent_invalid",
        "a graph tab has no parent"
    );
    d.ok(
        "editor.matgraph_instance_open",
        json!({ "path": "assets/wall_red.matinst.ron" }),
    );
    let report = d.ok(
        "editor.matgraph_instance_reparent",
        json!({ "parent": "assets/floor.matgraph.ron" }),
    );
    assert_eq!(report["parent"], "floor.matgraph.ron");
    assert_eq!(report["kept"], json!(["param tint"]));
    assert_eq!(report["dropped"].as_array().map(Vec::len), Some(2));
    assert_eq!(report["ok"], true);
    assert_eq!(
        d.code(
            "editor.matgraph_params",
            json!({ "op": "set", "name": "x", "value": 1.0 })
        ),
        "matgraph_read_only"
    );
    d.ok("editor.undo", json!({}));
    let state = d.ok("editor.matgraph_state", json!({}));
    assert_eq!(state["instance"]["parent"], "wall.matgraph.ron", "{state}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `editor.matgraph_edit { op: "disconnect_all" }` unwires a node's inputs, its readers and the
/// outputs it drives, in one undo step.
#[test]
fn disconnect_all_unwires_every_wire_of_a_node() {
    let mut doc = MatGraphDoc::new(MaterialGraph::from_ron(GRAPH).expect("graph"), Default::default());
    let count = doc.disconnect_all("mix").expect("node");
    assert_eq!(count, 3, "tint and half into mix, mix into base");
    let mix = &doc.graph.nodes["mix"];
    assert_eq!(mix.inputs().iter().filter(|i| !i.is_empty()).count(), 0);
    assert!(doc.graph.nodes.contains_key("mix"), "the node stays");
    doc.disconnect_all("base").expect("base");
    assert_eq!(doc.graph.output.get("base_color"), None);
    assert!(doc.disconnect_all("nope").is_err());
}

fn world_with_graph(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor {
        root: Some(root.to_path_buf()),
        ..MatGraphEditor::default()
    };
    editor.open(&root.join("assets/wall.matgraph.ron")).expect("open");
    world.insert_resource(editor);
    world
}

/// The panel's whole side column, as `matgraph_editor_panel` draws it (no GPU).
fn panel(world: &mut World) -> impl FnMut(&mut egui::Ui) + '_ {
    move |ui| super::panel::matgraph_editor_panel(ui, world, None)
}

const SIZE: [f32; 2] = [1400.0, 2400.0];

/// Keyboard route of the Parameters panel: Tab to "Move rough up", Enter: one reorder edit;
/// Tab to "Remove tint", Enter: the row goes.
#[test]
fn the_parameters_panel_reorders_and_removes_from_the_keyboard() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = world_with_graph(&root);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Move rough up", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    let order: Vec<String> = world
        .resource::<MatGraphEditor>()
        .doc
        .graph
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(order, ["rough", "tint"]);
    tab_to(&ctx, SIZE, "Remove tint", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    assert_eq!(world.resource::<MatGraphEditor>().doc.graph.params.len(), 1);
    crate::command::undo_world(&mut world);
    crate::command::undo_world(&mut world);
    assert_eq!(
        world.resource::<MatGraphEditor>().doc.graph,
        MaterialGraph::from_ron(GRAPH).expect("graph"),
        "two edits, two undo steps"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Keyboard route of the node menu's items: with a node selected, Tab to "Find usages",
/// "Preview this node" and "Disconnect all" in the inspector; each does what the menu does.
#[test]
fn the_node_menu_items_are_reachable_from_the_keyboard() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = world_with_graph(&root);
    world.resource_mut::<MatGraphEditor>().ui.selected = Some("tint".into());
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Find usages", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    {
        let editor = world.resource::<MatGraphEditor>();
        let (query, found) = editor.ui.parity.usages.as_ref().expect("usages listed");
        assert_eq!(query.name, "tint");
        assert_eq!(found.as_ref().map(Vec::len), Ok(3));
    }
    // A usage row is a link: Enter opens the instance in a tab of its own.
    let row = format!(
        "instance {} (overrides, inherits)",
        root.join("assets/wall_red.matinst.ron").display()
    );
    tab_to(&ctx, SIZE, &row, false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    {
        let mut editor = world.resource_mut::<MatGraphEditor>();
        assert!(editor.doc.instance_meta().is_some(), "{}", editor.toast);
        assert_eq!(editor.tabs().len(), 2);
        editor.select_tab(0).expect("back to the graph");
        editor.ui.selected = Some("tint".into());
    }
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Preview this node", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    assert_eq!(
        world.resource::<MatGraphEditor>().ui.parity.pinned.as_deref(),
        Some("tint")
    );
    world.resource_mut::<MatGraphEditor>().ui.selected = Some("mix".into());
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Disconnect all", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    let editor = world.resource::<MatGraphEditor>();
    assert!(
        editor.doc.graph.nodes["mix"]
            .inputs()
            .iter()
            .all(|i| i.is_empty())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Keyboard route of the Hierarchy: Tab to its header, Enter opens it and scans the project.
#[test]
fn the_hierarchy_opens_and_scans_from_the_keyboard() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = world_with_graph(&root);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Hierarchy", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    frame(&ctx, SIZE, vec![], panel(&mut world));
    let editor = world.resource::<MatGraphEditor>();
    let tree = match &editor.ui.parity.hierarchy {
        Some(Ok(tree)) => tree,
        other => panic!(
            "scanned: {:?}",
            other.as_ref().map(|r| r.as_ref().map(|t| t.graphs.len()))
        ),
    };
    assert_eq!(tree.graphs.len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}

/// Keyboard route of the reparent picker: with a parent picked, Tab to "Reparent", Enter.
#[test]
fn the_reparent_button_works_from_the_keyboard() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor {
        root: Some(root.clone()),
        ..MatGraphEditor::default()
    };
    editor
        .open_instance(
            &root.join("assets/wall_red.matinst.ron"),
            &root.join("assets/wall.matgraph.ron"),
        )
        .expect("open");
    editor.ui.parity.reparent_pick = root.join("assets/floor.matgraph.ron").display().to_string();
    world.insert_resource(editor);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Reparent", false, 600, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    let editor = world.resource::<MatGraphEditor>();
    assert_eq!(
        editor.doc.instance_meta().map(|i| i.parent.as_str()),
        Some("floor.matgraph.ron"),
        "{}",
        editor.toast
    );
    assert!(editor.toast.contains("dropped"), "{}", editor.toast);
    let _ = std::fs::remove_dir_all(&root);
}

/// The preview key moves with the tier and the pinned node (the preview re-renders), and the
/// tier picker works from the keyboard.
#[test]
fn the_tier_and_pinned_node_move_the_preview_key() {
    let mut state = super::params_panel::ParityState::default();
    let base = state.preview_hash();
    state.profile = Some(regia_render::RenderProfilePreset::MobileLow);
    let tier = state.preview_hash();
    state.pinned = Some("tint".into());
    let pinned = state.preview_hash();
    assert!(base != tier && tier != pinned && base != pinned);
    let root = project();
    let mut world = world_with_graph(&root);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Preview tier", false, 600, panel(&mut world));
    // Space opens the list; Tab walks its options; Enter picks one.
    frame(
        &ctx,
        SIZE,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        panel(&mut world),
    );
    tab_to(&ctx, SIZE, "MobileLow", false, 40, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        panel(&mut world),
    );
    assert_eq!(
        world.resource::<MatGraphEditor>().ui.parity.profile,
        Some(regia_render::RenderProfilePreset::MobileLow)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The tier preview renders on every policy profile (lavapipe or a card; skipped without a
/// device), and a pinned constant draws its own value.
#[test]
fn the_tier_preview_renders_on_every_profile() {
    let device = match crate::render::headless_device() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip the_tier_preview_renders_on_every_profile: {err}");
            return;
        }
    };
    let graph = MaterialGraph::from_ron(OTHER).expect("graph");
    let geometry =
        crate::material_editor::preview_geometry(&crate::material_editor::PreviewShape::Sphere, None)
            .expect("sphere");
    let view = crate::material_editor::PreviewView::default();
    for profile in super::cost::profiles() {
        let image = super::preview::render_graph_preview_profile(
            &device,
            &graph,
            None,
            None,
            [96.0, 96.0],
            &geometry,
            &view,
            Some(profile),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", profile.name()));
        let stats = super::preview::measure(&image);
        assert!(stats.coverage > 0.05, "{}: {stats:?}", profile.name());
    }
    let pinned = super::node_preview::derived_graph(&graph, "half").expect("derived");
    let image = super::preview::render_graph_preview_profile(
        &device,
        &pinned,
        None,
        None,
        [96.0, 96.0],
        &geometry,
        &view,
        None,
    )
    .expect("pinned renders");
    let s = super::preview::measure(&image);
    let spread = s
        .mean_rgb
        .iter()
        .fold(0.0f32, |m, c| m.max((c - s.mean_rgb[0]).abs()));
    assert!(spread < 0.05, "a gray constant draws gray: {s:?}");
    let _ = ParamValue::Float(0.0);
}
