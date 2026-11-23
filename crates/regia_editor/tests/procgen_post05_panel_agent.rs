// SPDX-License-Identifier: MPL-2.0
//! Post-v0.5 procedural graph panel remainder (spec 67 § 2) through the real agent dispatcher: the
//! Parameters tab (`editor.procgen_param` declare / rename / index / ty / min / max / doc), the
//! typed setting widgets (`editor.procgen_widgets`), the data table's panel view
//! (`editor.procgen_table` without `node`, `toggle_sort`, a refused filter keeps the last good
//! one), pin colors and node badges (`editor.procgen_badges`, theme key `data_kinds`), promote to
//! parameter with a name (`editor.procgen_promote`), the collapse boundary check
//! (`procgen_collapse_invalid`) and the shipped `tool_fence` preset. Headless: fixed calls and
//! editor ticks, no GPU, no wall clock, no golden images.

use std::collections::BTreeMap;

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::UndoStack;
use regia_editor::procgen::ProcGraphDoc;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, ProcNodeRegistry};
use regia_scene::components::{EntityGuid, Name};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

const FENCE: &str = "starter/tool_fence.pgraph.ron";

fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    n.at = (at, 0.0);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

/// `in.self → sample.region (spacing bound) → density.curve → attr.remove → out.points`, with three
/// parameters.
fn param_graph() -> String {
    let text = r#"ProcGraph(
    format_version: 1,
    description: "panel test",
    params: [
        (name: "spacing", ty: Float, default: 4.0, min: Some(0.5), max: Some(16.0), doc: "Cell size"),
        (name: "count", ty: Int, default: 3, min: None, max: None, doc: ""),
        (name: "mask", ty: Asset, default: "", min: None, max: None, doc: ""),
    ],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "sample.region", settings: {spacing_m: Bind("spacing")}, at: (200.0, 0.0)),
        3: (ty: "density.curve", settings: {}, at: (400.0, 0.0)),
        4: (ty: "attr.remove", settings: {drop: ["a"]}, at: (600.0, 0.0)),
        5: (ty: "out.points", settings: {}, at: (800.0, 0.0)),
    },
    edges: [
        (from: (1, "out"), to: (2, "region")),
        (from: (2, "out"), to: (3, "in")),
        (from: (3, "out"), to: (4, "in")),
        (from: (4, "out"), to: (5, "in")),
    ],
)"#;
    let g = ProcGraph::from_ron(text).expect("test graph parses");
    g.to_ron()
}

/// Two branches from `in.self`: `2 → 3 → 4` and `5 → 6`.
fn branch_graph() -> String {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "4.0")]));
    g.nodes.insert(3, node("density.curve", 400.0, &[]));
    g.nodes.insert(4, node("out.points", 600.0, &[]));
    g.nodes
        .insert(5, node("sample.region", 200.0, &[("spacing_m", "8.0")]));
    g.nodes.insert(6, node("out.points", 400.0, &[("name", "\"b\"")]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
        Edge::new(1, "out", 5, "region"),
        Edge::new(5, "out", 6, "in"),
    ];
    g.to_ron()
}

/// A graph with an asset setting (`in.mask`).
fn mask_graph() -> String {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.mask", 0.0, &[]));
    g.to_ron()
}

/// `in.self → sample.region` feeding two tiers: `tiles.tier(256 m) → out.points` and
/// `tiles.tier(32 m) → out.points`. Nodes 1 and 2 are tier-neutral (they run in both tiers).
fn two_tier_graph() -> String {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "8.0")]));
    g.nodes
        .insert(3, node("tiles.tier", 400.0, &[("cell_m", "Meters(256.0)")]));
    g.nodes
        .insert(4, node("out.points", 600.0, &[("name", "\"coarse\"")]));
    g.nodes
        .insert(5, node("tiles.tier", 400.0, &[("cell_m", "Meters(32.0)")]));
    g.nodes
        .insert(6, node("out.points", 600.0, &[("name", "\"fine\"")]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
        Edge::new(2, "out", 5, "in"),
        Edge::new(5, "out", 6, "in"),
    ];
    g.to_ron()
}

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("p.pgraph.ron".to_string(), param_graph()),
        ("c.pgraph.ron".to_string(), branch_graph()),
        ("w.pgraph.ron".to_string(), mask_graph()),
        ("t.pgraph.ron".to_string(), two_tier_graph()),
        ("t1.pgraph.ron".to_string(), one_tier_graph()),
    ])
}

/// `in.self → sample.region → tiles.tier(32 m) → out.points`: nodes 1 and 2 are tier-neutral and
/// feed the one tier.
fn one_tier_graph() -> String {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "8.0")]));
    g.nodes
        .insert(3, node("tiles.tier", 400.0, &[("cell_m", "Meters(32.0)")]));
    g.nodes
        .insert(4, node("out.points", 600.0, &[("name", "\"fine\"")]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
    ];
    g.to_ron()
}

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    dir: std::path::PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Agent {
    fn new() -> Self {
        let dir = regia_test_utils::unique_temp_dir("procgen-post05-panel");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<regia_editor::selection::Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.heavy_rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
            dir,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        regia_editor::procgen::tick(self.app.world_mut());
        self.app.update();
        let response = answer.try_recv().expect("answered within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(v) => v,
            Err(e) => panic!("{method} {params}: {e:?}"),
        }
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            regia_editor::procgen::tick(self.app.world_mut());
            self.app.update();
        }
    }

    fn undo_depth(&self) -> usize {
        self.app.world().resource::<UndoStack>().undo_depth()
    }

    fn graph(&self) -> ProcGraph {
        self.app.world().resource::<ProcGraphDoc>().graph.clone()
    }

    fn generator(&mut self, graph: &str, tiles: Option<f32>) -> Entity {
        let e = self
            .app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(8.0, 2.0, 8.0),
                    },
                    tiles: tiles.map(|cell_m| regia_procgen::tiles::TileSettings {
                        cell_m,
                        ..regia_procgen::tiles::TileSettings::default()
                    }),
                    ..ProcGenerator::new(graph)
                },
                EntityGuid(Guid(0x00ab_cdef_0123_4567_89ab_cdef_0123_4567)),
                Transform::IDENTITY,
                Name::new("gen"),
            ))
            .id();
        self.app.update();
        e
    }
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn param_names(g: &ProcGraph) -> Vec<String> {
    g.params.iter().map(|p| p.name.clone()).collect()
}

// ---- Item 1: the Parameters tab ----

#[test]
fn the_parameters_tab_declares_renames_reorders_and_types_through_the_agent() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let depth = a.undo_depth();

    // Rename updates every Bind, one undo entry.
    let out = a.call(
        "editor.procgen_param",
        json!({ "name": "spacing", "rename": "cell" }),
    );
    assert_eq!(out["name"], "cell");
    let g = a.graph();
    assert_eq!(g.nodes[&2].settings["spacing_m"].as_bind(), Some("cell"));
    assert_eq!(param_names(&g), ["cell", "count", "mask"]);
    assert_eq!(a.undo_depth(), depth + 1, "rename is one undo entry");

    // Reorder.
    a.call("editor.procgen_param", json!({ "name": "mask", "index": 0 }));
    assert_eq!(param_names(&a.graph()), ["mask", "cell", "count"]);

    // Type change converts the default.
    a.call("editor.procgen_param", json!({ "name": "count", "ty": "Float" }));
    let g = a.graph();
    let count = g.param("count").expect("count");
    assert_eq!(count.ty, regia_procgen::AttrType::Float);
    assert_eq!(count.default, regia_procgen::SettingValue::Float(3.0));

    // Range and doc together are one entry.
    let before = a.undo_depth();
    let out = a.call(
        "editor.procgen_param",
        json!({ "name": "cell", "min": 1.0, "max": 8.0, "doc": "Grid cell" }),
    );
    assert_eq!(a.undo_depth(), before + 1);
    let cell = out["params"]
        .as_array()
        .expect("params listed")
        .iter()
        .find(|p| p["name"] == "cell")
        .cloned()
        .expect("cell row");
    assert_eq!(
        (cell["min"].clone(), cell["max"].clone()),
        (json!(1.0), json!(8.0))
    );
    assert_eq!(cell["doc"], "Grid cell");

    // Refused edits leave the graph as it is.
    let before = a.graph();
    let bad = a
        .try_call(
            "editor.procgen_param",
            json!({ "name": "cell", "min": 9.0, "max": 1.0 }),
        )
        .expect_err("min > max");
    assert_eq!(reason(&bad), "procgen_param_range");
    let bad = a
        .try_call(
            "editor.procgen_param",
            json!({ "name": "cell", "rename": "not valid" }),
        )
        .expect_err("bad name");
    assert_eq!(reason(&bad), "procgen_param_name");
    let bad = a
        .try_call("editor.procgen_param", json!({ "name": "cell", "ty": "Matrix" }))
        .expect_err("bad type");
    assert_eq!(reason(&bad), "procgen_param_type");
    assert_eq!(a.graph(), before, "nothing written");

    // Declare a typed parameter.
    a.call(
        "editor.procgen_param",
        json!({ "name": "offset", "declare": true, "ty": "Vec3" }),
    );
    let g = a.graph();
    assert_eq!(
        g.param("offset").map(|p| p.ty),
        Some(regia_procgen::AttrType::Vec3)
    );
    a.call("editor.undo", json!({}));
    assert!(
        a.graph().param("offset").is_none(),
        "one undo removes the declaration"
    );
}

/// The Parameters tab's "remove" (ADR-7452, agent parity of `procgen/params_tab.rs`'s remove
/// button): `remove: true` drops the declaration and unbinds every bound setting to the default,
/// one undo entry; it takes no other edit key.
#[test]
fn the_parameters_tab_removes_a_parameter_through_the_agent() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let depth = a.undo_depth();
    let bad = a
        .try_call(
            "editor.procgen_param",
            json!({ "name": "spacing", "remove": true, "rename": "cell" }),
        )
        .expect_err("remove takes no other edit");
    assert_eq!(reason(&bad), "procgen_param_value");
    let bad = a
        .try_call("editor.procgen_param", json!({ "name": "nope", "remove": true }))
        .expect_err("unknown parameter");
    assert_eq!(reason(&bad), "procgen_unknown_param");
    // rev-claude-58 parity finding 2 (ADR-7495): a non-boolean `remove` is refused by name, not
    // silently read as false; `remove` with `generator` is refused, not ignored.
    let bad = a
        .try_call(
            "editor.procgen_param",
            json!({ "name": "spacing", "remove": "yes" }),
        )
        .expect_err("remove is a boolean");
    assert_eq!(reason(&bad), "procgen_param_value");
    assert!(bad.message.contains("`remove` is a boolean"), "{}", bad.message);
    let bad = a
        .try_call(
            "editor.procgen_param",
            json!({ "name": "spacing", "remove": true, "generator": 1 }),
        )
        .expect_err("remove takes no generator");
    assert_eq!(reason(&bad), "procgen_param_value");
    assert_eq!(a.undo_depth(), depth, "refused calls write nothing");

    let out = a.call(
        "editor.procgen_param",
        json!({ "name": "spacing", "remove": true }),
    );
    let listed: Vec<&str> = out["params"]
        .as_array()
        .expect("params listed")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(listed, ["count", "mask"]);
    let g = a.graph();
    assert_eq!(param_names(&g), ["count", "mask"]);
    let spacing = &g.nodes[&2].settings["spacing_m"];
    assert_eq!(spacing.as_bind(), None, "the bound setting is unbound");
    assert_eq!(
        *spacing,
        regia_procgen::SettingValue::Float(4.0),
        "to the default"
    );
    assert_eq!(a.undo_depth(), depth + 1, "remove is one undo entry");

    a.call("editor.undo", json!({}));
    let g = a.graph();
    assert_eq!(param_names(&g), ["spacing", "count", "mask"]);
    assert_eq!(g.nodes[&2].settings["spacing_m"].as_bind(), Some("spacing"));
}

// ---- Item 2: typed setting widgets ----

#[test]
fn typed_widgets_describe_settings_and_check_values_without_writing() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let w = a.call("editor.procgen_widgets", json!({ "node": 3 }));
    let curve = &w["settings"][0];
    assert_eq!(
        (curve["name"].clone(), curve["widget"].clone()),
        (json!("curve"), json!("curve"))
    );
    assert_eq!(curve["points"], json!([[0.0, 0.0], [1.0, 1.0]]));

    let w = a.call("editor.procgen_widgets", json!({ "node": 2 }));
    let by_name = |name: &str| -> Value {
        w["settings"]
            .as_array()
            .expect("settings")
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no setting {name}: {w}"))
    };
    assert_eq!(by_name("spacing_m")["bound"], "spacing");
    assert_eq!(by_name("jitter")["widget"], "number");
    assert_eq!(
        (by_name("jitter")["min"].clone(), by_name("jitter")["max"].clone()),
        (json!(0.0), json!(1.0))
    );
    assert_eq!(by_name("height")["widget"], "enum");
    assert_eq!(by_name("height")["options"], json!(["region", "value"]));

    let w = a.call("editor.procgen_widgets", json!({ "node": 4 }));
    assert_eq!(w["settings"][0]["widget"], "name_list");
    assert_eq!(w["settings"][0]["items"], json!(["a"]));

    // An invalid value is reported with its message and not written.
    let before = a.graph();
    let checked = a.call(
        "editor.procgen_widgets",
        json!({ "node": 2, "check": { "setting": "jitter", "value": 4.0 } }),
    );
    assert_eq!(checked["check"]["ok"], false);
    assert_eq!(checked["check"]["reason"], "setting_out_of_range");
    assert!(
        checked["check"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{checked}"
    );
    let fine = a.call(
        "editor.procgen_widgets",
        json!({ "node": 2, "check": { "setting": "jitter", "value": 0.5 } }),
    );
    assert_eq!(fine["check"]["ok"], true);
    assert_eq!(a.graph(), before, "checking never writes");

    // The asset picker lists project files matching a search.
    std::fs::create_dir_all(a.dir.join("textures")).expect("dir");
    std::fs::write(a.dir.join("textures/mask.png"), b"x").expect("write");
    std::fs::write(a.dir.join("textures/rock.png"), b"x").expect("write");
    a.call("editor.procgen_open", json!({ "path": "w.pgraph.ron" }));
    let w = a.call("editor.procgen_widgets", json!({ "node": 1, "search": "mask" }));
    let asset = w["settings"]
        .as_array()
        .expect("settings")
        .iter()
        .find(|s| s["name"] == "asset")
        .cloned()
        .expect("asset setting");
    assert_eq!(asset["widget"], "asset");
    assert_eq!(asset["choices"], json!(["textures/mask.png"]));
}

// ---- Item 3: the data table's panel view ----

#[test]
fn the_table_view_is_readable_toggles_sort_and_keeps_the_last_good_filter() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let e = a.generator("p.pgraph.ron", None);
    a.call("editor.procgen_preview", json!({ "generator": e.to_bits() }));
    a.frames(14);
    a.call(
        "editor.procgen_table",
        json!({ "node": 3, "filter": "position.x >= 0" }),
    );
    // Without `node`: the view the panel shows.
    let view = a.call("editor.procgen_table", json!({}));
    assert_eq!(view["node"], 3);
    assert_eq!(view["filter"], "position.x >= 0");
    let total = view["total"].as_u64().expect("total");
    assert!(total > 0);
    // A header click cycles ascending, descending, off.
    let up = a.call("editor.procgen_table", json!({ "toggle_sort": "position.z" }));
    assert_eq!(up["sort"], json!({ "column": "position.z", "descending": false }));
    let down = a.call("editor.procgen_table", json!({ "toggle_sort": "position.z" }));
    assert_eq!(
        down["sort"],
        json!({ "column": "position.z", "descending": true })
    );
    let off = a.call("editor.procgen_table", json!({ "toggle_sort": "position.z" }));
    assert_eq!(off["sort"], Value::Null);
    // A bad filter is refused and the last good one stays.
    let bad = a
        .try_call("editor.procgen_table", json!({ "filter": "position.x >=" }))
        .expect_err("bad filter");
    assert_eq!(reason(&bad), "procgen_filter_invalid");
    let unknown = a
        .try_call("editor.procgen_table", json!({ "filter": "nope > 1" }))
        .expect_err("unknown column");
    assert_eq!(reason(&unknown), "procgen_unknown_column");
    let view = a.call("editor.procgen_table", json!({}));
    assert_eq!(view["filter"], "position.x >= 0");
    assert_eq!(view["total"].as_u64(), Some(total));
}

// ---- Item 4: pin colors and badges ----

#[test]
fn badges_and_theme_pin_colors_answer_through_the_agent() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let e = a.generator("p.pgraph.ron", Some(32.0));
    a.call("editor.procgen_preview", json!({ "generator": e.to_bits() }));
    a.frames(4);
    a.call(
        "procgen.graph_edit",
        json!({ "path": "p.pgraph.ron", "ops": [{ "op": "set_enabled", "id": 3, "enabled": false }] }),
    );
    a.call("editor.procgen_visualize", json!({ "node": 2, "on": true }));
    let badges = a.call("editor.procgen_badges", json!({}));
    let row = |id: u32| -> Value {
        badges["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no node {id}: {badges}"))
    };
    let has = |id: u32, b: &str| {
        row(id)["badges"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x == b))
    };
    assert!(has(3, "bypassed"), "{badges}");
    assert!(has(2, "visualized"), "{badges}");
    assert!(!has(2, "bypassed"));
    assert_eq!(row(2)["tier"]["cell_m"], json!(32.0), "{badges}");
    assert!(row(2)["warnings"].is_u64());
    // A multi-kind input pin is striped (several colors); an output of one kind has one.
    let region = &row(2)["pins"]["inputs"][0];
    assert_eq!(region["name"], "region");
    assert!(
        region["colors"].as_array().is_some_and(|c| c.len() >= 2),
        "{region}"
    );
    let out = &row(2)["pins"]["outputs"][0];
    assert_eq!(out["colors"].as_array().map(Vec::len), Some(1));
    // The colors come from the theme.
    a.call(
        "editor.set_theme",
        json!({ "tokens": { "data_kinds": { "points": "#ff0000" } } }),
    );
    let badges = a.call("editor.procgen_badges", json!({ "nodes": [2] }));
    assert_eq!(
        badges["nodes"][0]["pins"]["outputs"][0]["colors"],
        json!(["#ff0000"])
    );
    let theme = a.call("editor.get_theme", json!({}));
    assert_eq!(theme["tokens"]["data_kinds"]["points"], "#ff0000");
    // A node with a compile error carries an error badge: retyping the parameter that node 2's
    // `spacing_m` binds leaves the bind mismatched (the panel keeps invalid documents; only the run
    // waits for a valid graph).
    a.call("editor.procgen_param", json!({ "name": "spacing", "ty": "Name" }));
    let badges = a.call("editor.procgen_badges", json!({ "nodes": [2] }));
    let err = &badges["nodes"][0];
    assert!(
        err["badges"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x == "error")),
        "{badges}"
    );
    assert!(
        err["error"]["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{badges}"
    );
}

// ---- Item 5: promote with a name ----

#[test]
fn promote_to_parameter_takes_a_name_and_refuses_a_bad_one() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "p.pgraph.ron" }));
    let depth = a.undo_depth();
    let out = a.call(
        "editor.procgen_promote",
        json!({ "node": 2, "setting": "jitter", "name": "wiggle" }),
    );
    assert_eq!(
        (out["param"].clone(), out["created"].clone()),
        (json!("wiggle"), json!(true))
    );
    let g = a.graph();
    assert_eq!(g.nodes[&2].settings["jitter"].as_bind(), Some("wiggle"));
    let p = g.param("wiggle").expect("declared");
    assert_eq!(p.ty, regia_procgen::AttrType::Float);
    assert_eq!(p.default, regia_procgen::SettingValue::Float(0.0));
    assert_eq!(a.undo_depth(), depth + 1, "one undo entry");

    // No name: the suggestion (the setting's name, made unique).
    let suggested = a.call("editor.procgen_promote", json!({ "node": 2, "setting": "y" }));
    assert_eq!(suggested["param"], "y");

    // A taken or invalid name is refused and nothing is written.
    let before = a.graph();
    let taken = a
        .try_call(
            "editor.procgen_promote",
            json!({ "node": 2, "setting": "height", "name": "count" }),
        )
        .expect_err("taken");
    assert_eq!(reason(&taken), "procgen_param_name");
    let invalid = a
        .try_call(
            "editor.procgen_promote",
            json!({ "node": 2, "setting": "height", "name": "2x" }),
        )
        .expect_err("invalid");
    assert_eq!(reason(&invalid), "procgen_param_name");
    assert_eq!(a.graph(), before);

    // Picking an existing parameter binds without declaring.
    let picked = a.call(
        "editor.procgen_promote",
        json!({ "node": 2, "setting": "y", "name": "spacing", "existing": true }),
    );
    // `y` is bound to `y` already: re-binding to `spacing` replaces it.
    assert_eq!(
        (picked["param"].clone(), picked["created"].clone()),
        (json!("spacing"), json!(false))
    );
    assert_eq!(a.graph().nodes[&2].settings["y"].as_bind(), Some("spacing"));
}

// ---- Item 6: the collapse boundary check ----

#[test]
fn collapse_refuses_a_selection_without_one_connected_boundary() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "c.pgraph.ron" }));
    let before = a.graph();
    // Two unconnected groups.
    let split = a
        .try_call(
            "editor.procgen_collapse",
            json!({ "nodes": [3, 5], "path": "split.pgraph.ron" }),
        )
        .expect_err("two groups");
    assert_eq!(reason(&split), "procgen_collapse_invalid");
    // A path leaves the selection and comes back (2 → 3 → 4 with 3 outside): a cycle.
    let cycle = a
        .try_call(
            "editor.procgen_collapse",
            json!({ "nodes": [2, 4], "path": "cycle.pgraph.ron" }),
        )
        .expect_err("re-entering path");
    assert_eq!(reason(&cycle), "procgen_collapse_invalid");
    assert_eq!(a.graph(), before, "a refused collapse writes nothing");
    let written = a.app.world().resource::<ProcGenSettings>();
    if let ProcGraphSource::Memory(files) = &written.source {
        assert!(!files.contains_key("split.pgraph.ron") && !files.contains_key("cycle.pgraph.ron"));
    }
    // A connected selection still collapses.
    let ok = a.call(
        "editor.procgen_collapse",
        json!({ "nodes": [2, 3], "path": "ok.pgraph.ron" }),
    );
    assert_eq!(ok["call_node"], 2);
}

// ---- Item 7: the shipped fence preset ----

fn ground(app: &mut App) -> Entity {
    use std::sync::Arc;
    let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    let material = Arc::new(regia_render::LabMaterial::default());
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            regia_render::MeshRenderer::new(mesh, material),
            Name::new("ground"),
            EntityGuid(Guid(77)),
        ))
        .id()
}

#[test]
fn the_fence_preset_draws_a_fence_with_the_path_tool() {
    let mut a = Agent::new();
    ground(&mut a.app);
    a.app.update();
    let out = a.call(
        "editor.procgen_tool",
        json!({
            "tool": "path",
            "preset": FENCE,
            "points": [[-8, 5, 0], [-3, 5, 0], [3, 5, 1], [8, 5, 1]],
            "apply": true,
        }),
    );
    assert_eq!(
        (out["spline_points"].clone(), out["applied"].clone()),
        (json!(4), json!(true))
    );
    let e = Entity::try_from_bits(out["entity"].as_u64().expect("entity")).expect("entity bits");
    let mut ready = false;
    for _ in 0..12 {
        a.frames(1);
        if let Some(st) = a.app.world().get::<ProcGenState>(e)
            && st.state == regia_procgen::world::ProcGenRunState::Ready
        {
            ready = true;
            break;
        }
    }
    let st = a.app.world().get::<ProcGenState>(e).expect("state");
    assert!(ready, "fence generator ready: {:?} {:?}", st.state, st.last_error);
    let modules = st
        .report
        .as_ref()
        .and_then(|r| r.nodes.iter().find(|n| n.ty == "spline.modules"))
        .map_or(0, |n| n.elements);
    assert!(modules >= 2, "posts and panels along 16 m: {modules}");
    // The starter compiles and is a Path preset.
    let text = regia_editor::procgen::presets::preset(FENCE).expect("shipped");
    let g = ProcGraph::from_ron(text).expect("parses");
    assert_eq!(g.flags.tool.as_deref(), Some("path"));
    g.compile(&ProcNodeRegistry::with_builtins()).expect("compiles");
}

/// post05w4 item 3b (the procgen-panel lane's known issue "a tier-neutral node shows no tier
/// badge in a graph with several tiers"): a node shared by two tiers carries a tier badge naming
/// both, while the nodes placed at one tier keep theirs.
#[test]
fn a_tier_neutral_node_in_a_two_tier_graph_shows_the_tiers_it_feeds() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "t.pgraph.ron" }));
    let e = a.generator("t.pgraph.ron", Some(64.0));
    a.call("editor.procgen_preview", json!({ "generator": e.to_bits() }));
    a.frames(2);
    let badges = a.call("editor.procgen_badges", json!({}));
    let row = |id: u32| -> Value {
        badges["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no node {id}: {badges}"))
    };
    // Tiers: 256 m is index 0, 32 m index 1 (coarse to fine).
    assert_eq!(row(3)["tier"]["cell_m"], json!(256.0), "{badges}");
    assert_eq!(row(5)["tier"]["cell_m"], json!(32.0), "{badges}");
    for id in [1, 2] {
        let tier = &row(id)["tier"];
        assert!(
            row(id)["badges"]
                .as_array()
                .is_some_and(|l| l.iter().any(|x| x == "tier")),
            "node {id} is shared by both tiers and carries a tier badge: {badges}"
        );
        assert_eq!(tier["shared"], json!(true), "node {id}: {tier}");
        assert_eq!(tier["feeds"], json!([0, 1]), "node {id}: {tier}");
    }
    assert_eq!(row(3)["tier"]["shared"], json!(false), "{badges}");
    assert_eq!(row(3)["tier"]["feeds"], json!([0]), "{badges}");
}

/// w11-biome-procgen-tooling item 5: a tier-neutral node that feeds a single tier was badged exactly
/// like a node placed at that tier (`shared` is false with one tier). Its badge now says `neutral`
/// (agent) and `~<size>` (canvas), while the placed `tiles.tier` node is not neutral.
#[test]
fn a_tier_neutral_node_that_feeds_one_tier_is_badged_neutral() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "t1.pgraph.ron" }));
    let e = a.generator("t1.pgraph.ron", Some(64.0));
    a.call("editor.procgen_preview", json!({ "generator": e.to_bits() }));
    a.frames(2);
    let badges = a.call("editor.procgen_badges", json!({}));
    let row = |id: u32| -> Value {
        badges["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no node {id}: {badges}"))
    };
    for id in [1, 2] {
        let tier = &row(id)["tier"];
        assert_eq!(tier["neutral"], json!(true), "node {id}: {badges}");
        assert_eq!(tier["shared"], json!(false), "one tier only: {tier}");
        assert_eq!(
            tier["index"],
            row(3)["tier"]["index"],
            "it adopts its consumer's tier: {badges}"
        );
    }
    assert_eq!(row(3)["tier"]["neutral"], json!(false), "{badges}");
    let label = |neutral: bool, shared: bool, feeds: u16| {
        regia_editor::procgen::badges::TierBadge {
            index: 0,
            cell_m: Some(32.0),
            shared,
            feeds,
            neutral,
        }
        .label()
    };
    assert_eq!(label(true, false, 1), "~32 m");
    assert_eq!(label(false, false, 1), "32 m");
    assert_eq!(label(true, true, 3), "T0+T1");
}
