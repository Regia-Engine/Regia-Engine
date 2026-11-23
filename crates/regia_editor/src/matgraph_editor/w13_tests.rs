// SPDX-License-Identifier: MPL-2.0
//! w13 parity acceptance tests (ADR-8105 (b), ADR-8187): the parameters panel's categories,
//! ranges and tooltips (P-169), the preview scene of the material graph editor (P-083), and the
//! compile errors of open graphs in the Problems panel (P-185), each through the agent
//! dispatcher and from the keyboard.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_matgraph::{MaterialGraph, ParamRange, ParamValue};
use serde_json::{Value, json};

use super::*;
use crate::canvas_keys::test_input::{frame, key, tab_to, text};
use crate::command::UndoStack;

fn write(path: &Path, body: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("dir");
    }
    std::fs::write(path, body).expect("write");
}

/// A v5 graph with metadata: `rough` ranged 0..1 in steps of 0.25 under "Surface", `tint`
/// ungrouped with a tooltip, `glow` under "Emission".
const GRAPH: &str = r#"(
    format_version: 5,
    name: "wall",
    params: [
        (name: "rough", value: Float(0.5), category: "Surface", range: Some((min: 0.0, max: 1.0, step: 0.25))),
        (name: "tint", value: Color((0.8, 0.2, 0.1)), tooltip: "Base color of the wall"),
        (name: "glow", value: Float(2.0), category: "Emission"),
        (name: "wet", value: Float(0.0), category: "Surface", range: Some((min: 0.0, max: 1.0))),
    ],
    nodes: {
        "tint": Param("tint"),
        "r": Param("rough"),
        "g": Param("glow"),
        "w": Param("wet"),
        "e": Mul("tint", "g"),
        "rw": Add("r", "w"),
    },
    output: (base_color: Some("tint"), roughness: Some("rw"), emissive: Some("e")),
)"#;

/// A graph whose node `bad` reads a node that does not exist.
const BROKEN: &str = r#"(
    format_version: 5,
    name: "broken",
    nodes: { "half": Float(0.5), "bad": Mul("half", "missing") },
    output: (base_color: Some("bad")),
)"#;

fn project() -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("matgraph-w13");
    write(&root.join("assets/wall.matgraph.ron"), GRAPH);
    write(&root.join("assets/broken.matgraph.ron"), BROKEN);
    write(
        &root.join("assets/wall_wet.matinst.ron"),
        r#"(format_version: 2, parent: "wall.matgraph.ron", params: { "rough": Float(0.75) })"#,
    );
    root
}

/// An app with the agent plugin and the editor's methods.
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

fn world_with(root: &Path, files: &[&str]) -> World {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    let mut editor = MatGraphEditor {
        root: Some(root.to_path_buf()),
        ..MatGraphEditor::default()
    };
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            editor.open_tab();
        }
        editor.open(&root.join("assets").join(file)).expect("open");
    }
    world.insert_resource(editor);
    world
}

fn panel(world: &mut World) -> impl FnMut(&mut egui::Ui) + '_ {
    move |ui| super::panel::matgraph_editor_panel(ui, world, None)
}

const SIZE: [f32; 2] = [1400.0, 3000.0];

fn param<'a>(world: &'a World, name: &str) -> &'a regia_matgraph::ParamDecl {
    world
        .resource::<MatGraphEditor>()
        .doc
        .graph
        .params
        .iter()
        .find(|p| p.name == name)
        .expect("declared")
}

/// P-169: the parameters group by category (ungrouped first, then categories by first
/// appearance, slot order inside), Up/Down move within the group, every edit clamps to the
/// range (a default, a new range, an instance override), the tooltip is listed, and the agent
/// twin reads and writes the same metadata. Keyboard: the ranged slider steps with the arrows
/// and stops at the range's end; the Category field commits on Tab.
#[test]
fn params_group_by_category_and_clamp_to_range() {
    use egui::{Key, Modifiers};
    let graph = MaterialGraph::from_ron(GRAPH).expect("v5 with metadata");
    let groups = graph.param_groups();
    assert_eq!(
        groups,
        vec![
            (String::new(), vec![1]),
            ("Surface".to_string(), vec![0, 3]),
            ("Emission".to_string(), vec![2]),
        ]
    );
    assert_eq!(params_panel::group_neighbour_slot(&graph, 3, true), Some(0));
    assert_eq!(params_panel::group_neighbour_slot(&graph, 0, true), None);
    assert_eq!(params_panel::group_neighbour_slot(&graph, 0, false), Some(3));

    let mut g = graph.clone();
    params_panel::set(
        &mut g,
        "rough",
        params_panel::DeclValue::Param(ParamValue::Float(7.0)),
    )
    .expect("set");
    assert_eq!(g.params[0].value, ParamValue::Float(1.0), "clamped to max");
    params_panel::set(
        &mut g,
        "rough",
        params_panel::DeclValue::Param(ParamValue::Float(0.3)),
    )
    .expect("set");
    assert_eq!(g.params[0].value, ParamValue::Float(0.25), "snapped to the step");
    let value = params_panel::set_meta(
        &mut g,
        "glow",
        params_panel::ParamMeta {
            range: Some(Some(ParamRange {
                min: 0.0,
                max: 1.5,
                step: 0.0,
            })),
            ..Default::default()
        },
    )
    .expect("range");
    assert_eq!(value, ParamValue::Float(1.5), "a new range clamps the default");
    let bad = ParamRange {
        min: 1.0,
        max: 1.0,
        step: 0.0,
    };
    assert!(
        params_panel::set_meta(
            &mut g,
            "glow",
            params_panel::ParamMeta {
                range: Some(Some(bad)),
                ..Default::default()
            }
        )
        .is_err()
    );
    let rows = params_panel::rows(&graph);
    assert_eq!(rows[1].tooltip, "Base color of the wall");
    let json = params_panel::row_json(&rows[0]);
    assert_eq!(json["category"], "Surface");
    assert_eq!(json["range"]["step"], 0.25);

    // An instance inherits: its override is clamped to the parent's range.
    let root = project();
    let mut world = world_with(&root, &["wall.matgraph.ron"]);
    {
        let mut editor = world.resource_mut::<MatGraphEditor>();
        editor
            .open_instance(
                &root.join("assets/wall_wet.matinst.ron"),
                &root.join("assets/wall.matgraph.ron"),
            )
            .expect("instance");
        editor
            .doc
            .set_param_override("rough", Some(ParamValue::Float(-3.0)))
            .expect("override");
        let DocMode::Instance(instance) = &editor.doc.mode else {
            panic!("an instance tab");
        };
        assert_eq!(instance.params["rough"], ParamValue::Float(0.0));
    }

    // Keyboard: the ranged slider of `rough` (0.5) steps by 0.25 and stops at 1.0.
    let mut world = world_with(&root, &["wall.matgraph.ron"]);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "rough", false, 800, panel(&mut world));
    for _ in 0..3 {
        frame(
            &ctx,
            SIZE,
            vec![key(Key::ArrowRight, Modifiers::NONE)],
            panel(&mut world),
        );
        frame(&ctx, SIZE, vec![], panel(&mut world));
    }
    assert_eq!(param(&world, "rough").value, ParamValue::Float(1.0));
    // Keyboard: open the details of `tint`, type a category, Tab away: one edit.
    tab_to(&ctx, SIZE, "tint details", false, 800, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        panel(&mut world),
    );
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Category of tint", false, 800, panel(&mut world));
    frame(&ctx, SIZE, vec![text("Look")], panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Tab, Modifiers::NONE)],
        panel(&mut world),
    );
    frame(&ctx, SIZE, vec![], panel(&mut world));
    assert_eq!(param(&world, "tint").category, "Look");

    // The agent twin.
    let mut d = Dispatch::new(&root);
    d.ok(
        "editor.matgraph_open",
        json!({ "path": "assets/wall.matgraph.ron" }),
    );
    let listed = d.ok("editor.matgraph_params", json!({}));
    assert_eq!(
        listed["groups"],
        json!([
            { "category": "", "params": ["tint"] },
            { "category": "Surface", "params": ["rough", "wet"] },
            { "category": "Emission", "params": ["glow"] },
        ])
    );
    assert_eq!(listed["rows"][1]["tooltip"], "Base color of the wall");
    let clamped = d.ok(
        "editor.matgraph_params",
        json!({ "op": "set", "name": "rough", "value": 9.0 }),
    );
    assert_eq!(clamped["rows"][0]["default"], json!({ "Float": 1.0 }));
    let meta = d.ok(
        "editor.matgraph_params",
        json!({ "op": "meta", "name": "glow", "category": "Surface", "tooltip": "Glow strength", "range": { "min": 0.0, "max": 1.0 } }),
    );
    assert_eq!(meta["value"], json!({ "Float": 1.0 }));
    assert_eq!(meta["groups"][1]["params"], json!(["rough", "glow", "wet"]));
    let cleared = d.ok(
        "editor.matgraph_params",
        json!({ "op": "meta", "name": "glow", "range": null }),
    );
    assert!(cleared["rows"][2]["range"].is_null());
    for bad in [
        json!({ "op": "meta", "name": "glow" }),
        json!({ "op": "meta", "name": "glow", "range": { "min": 2.0, "max": 1.0 } }),
        json!({ "op": "meta", "name": "glow", "category": 3 }),
        json!({ "op": "meta", "name": "nope", "tooltip": "x" }),
    ] {
        assert_eq!(d.code("editor.matgraph_params", bad), "matgraph_params_invalid");
    }
    d.ok("editor.undo", json!({}));
    let back = d.ok("editor.matgraph_params", json!({}));
    assert_eq!(back["rows"][2]["range"]["max"], 1.0, "one undo step per meta op");
    let _ = std::fs::remove_dir_all(&root);
}

/// P-083: the material graph editor's preview draws the shared preview scene. Its controls show
/// (without a GPU too) and the keyboard reaches them; `editor.preview_scene { editor: "matgraph" }`
/// reaches the panel through `preview_scene::sync`; the render with the floor differs from the
/// default scene, which is the old picture (lavapipe or a GPU; skipped without one).
#[test]
fn the_matgraph_preview_uses_the_preview_scene() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = world_with(&root, &["wall.matgraph.ron"]);
    let ctx = egui::Context::default();
    frame(&ctx, SIZE, vec![], panel(&mut world));
    tab_to(&ctx, SIZE, "Floor", false, 1200, panel(&mut world));
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Space, Modifiers::NONE)],
        panel(&mut world),
    );
    assert!(world.resource::<MatGraphEditor>().preview_scene.settings.floor);
    tab_to(&ctx, SIZE, "Preview lighting", true, 60, panel(&mut world));

    // The agent hand-off: a patch reaches the editor's own scene on the shell's sync.
    let mut d = Dispatch::new(&root);
    d.ok(
        "editor.matgraph_open",
        json!({ "path": "assets/wall.matgraph.ron" }),
    );
    let out = d.ok(
        "editor.preview_scene",
        json!({ "editor": "matgraph", "preset": "studio", "rotate": true }),
    );
    assert_eq!(out["editor"], "matgraph");
    assert_eq!(out["preset"], "studio");
    let other = d.ok("editor.preview_scene", json!({ "editor": "material" }));
    assert_eq!(other["preset"], "outdoor", "the Material editor keeps its own");
    let (mut a, mut b, mut c) = Default::default();
    crate::preview_scene::sync(d.app.world_mut(), &mut a, &mut b, &mut c);
    let scene = d.app.world().resource::<MatGraphEditor>().preview_scene;
    assert_eq!(
        scene.settings.lighting,
        crate::preview_scene::PreviewLighting::Studio
    );
    assert!(scene.settings.auto_rotate);

    let device = match crate::render::headless_device() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip the GPU half of the_matgraph_preview_uses_the_preview_scene: {err}");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
    };
    let graph = MaterialGraph::from_ron(GRAPH).expect("graph");
    let cube = crate::material_editor::preview_geometry(&crate::material_editor::PreviewShape::Cube, None)
        .expect("cube");
    let view = crate::material_editor::PreviewView::default();
    let render = |scene: &crate::preview_scene::PreviewContext| {
        super::preview::render_graph_preview_scene(
            &device,
            &graph,
            None,
            None,
            [96.0, 96.0],
            &cube,
            &view,
            None,
            scene,
        )
        .expect("renders")
    };
    let plain =
        super::preview::render_graph_preview_on(&device, &graph, None, None, [96.0, 96.0], &cube, &view)
            .expect("plain");
    let default = render(&crate::preview_scene::PreviewContext::default());
    assert_eq!(
        default.pixels, plain.pixels,
        "the default scene is the old preview"
    );
    let mut floor = crate::preview_scene::PreviewContext::default();
    floor.settings.floor = true;
    assert_ne!(render(&floor).pixels, plain.pixels, "the floor shows");
    // The dispatcher app has no viewport device: the agent preview answers only where one is.
    if let Some(answer) = d
        .call("editor.matgraph_preview", json!({ "width": 96, "height": 96 }))
        .result
    {
        assert!(answer["png"].as_str().is_some_and(|p| !p.is_empty()));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// P-185 (material part): a broken node of an open graph (here a parked tab) is listed in the
/// Problems panel; its Open link (keyboard: Tab to it, Enter) activates the tab and selects the
/// node; `editor.problems_panel` lists `graph_errors` and follows `open_graph_error`.
#[test]
fn a_broken_matgraph_node_appears_in_problems_and_opens() {
    use egui::{Key, Modifiers};
    let root = project();
    let mut world = world_with(&root, &["broken.matgraph.ron", "wall.matgraph.ron"]);
    let problems = super::problems::world_problems(&mut world);
    assert!(!problems.is_empty(), "the broken graph has an error");
    let first = &problems[0];
    assert_eq!(first.label, "broken.matgraph.ron");
    assert_eq!(first.tab, 0);
    assert_eq!(first.node.as_deref(), Some("bad"), "{problems:?}");
    assert!(
        problems.iter().all(|p| p.label == "broken.matgraph.ron"),
        "the wall compiles"
    );

    let ctx = egui::Context::default();
    fn draw(world: &mut World) -> impl FnMut(&mut egui::Ui) + '_ {
        move |ui| crate::asset_panels::problems_panel(ui, world)
    }
    frame(&ctx, SIZE, vec![], draw(&mut world));
    tab_to(
        &ctx,
        SIZE,
        "Open node:bad in broken.matgraph.ron",
        false,
        200,
        draw(&mut world),
    );
    frame(
        &ctx,
        SIZE,
        vec![key(Key::Enter, Modifiers::NONE)],
        draw(&mut world),
    );
    {
        let editor = world.resource::<MatGraphEditor>();
        assert!(editor.tabs()[0].active, "the broken tab is active");
        assert_eq!(editor.ui.selected.as_deref(), Some("bad"));
    }
    let requests = world
        .get_resource::<crate::editor_layout::LayoutRequests>()
        .map(|r| r.pending.clone())
        .unwrap_or_default();
    assert!(requests.contains(&crate::editor_layout::LayoutRequest::Open(
        crate::shell::Panel::MaterialGraph
    )));

    let mut d = Dispatch::new(&root);
    let editor = world_with(&root, &["broken.matgraph.ron", "wall.matgraph.ron"])
        .remove_resource::<MatGraphEditor>()
        .expect("editor");
    d.app.world_mut().insert_resource(editor);
    let listed = d.ok("editor.problems_panel", json!({}));
    let errors = listed["graph_errors"].as_array().expect("graph_errors");
    assert!(!errors.is_empty(), "{listed}");
    assert_eq!(errors[0]["node"], "bad");
    let opened = d.ok("editor.problems_panel", json!({ "open_graph_error": 0 }));
    assert_eq!(opened["opened_graph_error"]["node"], "bad");
    let tabs = d.ok("editor.matgraph_tabs", json!({}));
    assert_eq!(tabs["tabs"][0]["active"], true, "{tabs}");
    assert_eq!(
        d.code("editor.problems_panel", json!({ "open_graph_error": 99 })),
        "graph_error_missing"
    );
    let _ = std::fs::remove_dir_all(&root);
}
