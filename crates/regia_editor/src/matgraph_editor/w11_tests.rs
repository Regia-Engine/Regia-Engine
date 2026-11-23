// SPDX-License-Identifier: MPL-2.0
//! w11 acceptance tests of the material graph editor gaps (spec 80 § 11–12.1, ADR-7730):
//! function tabs refresh their callers and preview on their own, a multi-output collapse keeps
//! the code, the WGSL view and Function users panels report what the agent methods do, and
//! visible node swatches stay past the cache cap.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use regia_ecs::prelude::*;
use regia_matgraph::{FunctionInput, MaterialFunction, Node, ValueType};
use serde_json::{Value, json};

use super::*;
use crate::command::UndoStack;

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = agent::methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|e| panic!("{method}: {} ({:?})", e.message, e.data))
}

/// A project folder with `assets/m/wear.matfunc.ron` (out = x × k, k = 0.5) and
/// `assets/m/g.matgraph.ron`, whose base color is `wear(c)`.
fn project(label: &str) -> (PathBuf, World) {
    let dir = regia_test_utils::unique_temp_dir(label);
    let folder = dir.join("assets/m");
    std::fs::create_dir_all(&folder).expect("folder");
    let function = MaterialFunction {
        format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
        name: "wear".into(),
        inputs: vec![FunctionInput {
            name: "x".into(),
            ty: ValueType::Vec3,
        }],
        nodes: BTreeMap::from([
            ("x".to_string(), Node::Input("x".into())),
            ("k".to_string(), Node::Float(0.5)),
            ("out".to_string(), Node::Mul("x".into(), "k".into())),
        ]),
        outputs: BTreeMap::from([("out".to_string(), "out".to_string())]),
    };
    std::fs::write(
        folder.join("wear.matfunc.ron"),
        function.to_ron().expect("function ron"),
    )
    .expect("function");
    std::fs::write(
        folder.join("g.matgraph.ron"),
        r#"(
            format_version: 4,
            functions: { "wear": File("wear.matfunc.ron") },
            nodes: {
                "c": Vec3((1.0, 1.0, 1.0)),
                "w": Call(function: "wear", args: { "x": "c" }, output: None),
            },
            output: (base_color: Some("w")),
            shading: (unlit: true),
        )"#,
    )
    .expect("graph");
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    (dir, world)
}

fn wgsl(world: &mut World) -> String {
    let c = ok(world, "editor.matgraph_compile", json!({ "include_wgsl": true }));
    assert_eq!(c["ok"], true, "{c}");
    function::normalized_wgsl(c["wgsl"].as_str().expect("wgsl"))
}

fn select_kind(world: &mut World, kind: &str) {
    let tabs = ok(world, "editor.matgraph_tabs", json!({}));
    let index = tabs["tabs"]
        .as_array()
        .and_then(|t| t.iter().position(|t| t["kind"] == kind))
        .unwrap_or_else(|| panic!("no {kind} tab: {tabs}"));
    ok(world, "editor.matgraph_tabs", json!({ "select": index }));
}

// -- (1) Function tabs ------------------------------------------------------------------------

/// An edit in a function tab (not saved) changes the compiled WGSL and the diagnostics key of
/// the open graph that calls it; the file stays as it was, and closing the tab without saving
/// gives the file's code back.
#[test]
fn a_function_tab_edit_refreshes_the_calling_graph() {
    let (dir, mut world) = project("matgraph-w11-live-fn");
    ok(
        &mut world,
        "editor.matgraph_open",
        json!({ "path": "assets/m/g.matgraph.ron" }),
    );
    let before = wgsl(&mut world);
    let key_before = ok(&mut world, "editor.matgraph_state", json!({}))["key"].clone();
    ok(
        &mut world,
        "editor.matgraph_function_open",
        json!({ "path": "assets/m/wear.matfunc.ron" }),
    );
    ok(
        &mut world,
        "editor.matgraph_edit",
        json!({ "op": "set", "name": "k", "node": "Float(0.25)" }),
    );
    select_kind(&mut world, "graph");
    let after = wgsl(&mut world);
    assert_ne!(after, before, "the graph compiles the tab's definition");
    assert!(after.contains("0.25"), "{after}");
    let key_after = ok(&mut world, "editor.matgraph_state", json!({}))["key"].clone();
    assert_ne!(key_after, key_before, "the diagnostics follow the tab");
    let on_disk = std::fs::read_to_string(dir.join("assets/m/wear.matfunc.ron")).expect("read");
    assert!(on_disk.contains("0.5"), "the file is not written: {on_disk}");
    // Close the function tab without saving: the graph compiles the file again.
    select_kind(&mut world, "function");
    ok(&mut world, "editor.matgraph_tabs", json!({ "close": true }));
    select_kind(&mut world, "graph");
    assert_eq!(wgsl(&mut world), before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A function tab is checked against its calling graph as that graph is now (its open tab),
/// not as it was when the function opened: a parameter declared after the function opened is
/// known to the function.
#[test]
fn a_function_tab_checks_against_the_live_calling_graph() {
    let (dir, mut world) = project("matgraph-w11-live-ctx");
    ok(
        &mut world,
        "editor.matgraph_open",
        json!({ "path": "assets/m/g.matgraph.ron" }),
    );
    ok(
        &mut world,
        "editor.matgraph_function_open",
        json!({ "path": "assets/m/wear.matfunc.ron" }),
    );
    select_kind(&mut world, "graph");
    ok(
        &mut world,
        "editor.matgraph_edit",
        json!({ "op": "params", "params": [{ "name": "amount", "value": { "Float": 0.3 } }] }),
    );
    select_kind(&mut world, "function");
    let edit = ok(
        &mut world,
        "editor.matgraph_edit",
        json!({ "op": "set", "name": "k", "node": "Param(\"amount\")" }),
    );
    assert_eq!(edit["ok"], true, "the function sees `amount`: {edit}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A function tab previews on its own (the calling graph with the tab's definition), and the
/// calling graph's swatch of the `Call` node changes with an edit in the tab (lavapipe or
/// hardware; skips with no adapter).
#[test]
fn a_function_tab_previews_and_moves_the_callers_swatch() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip a_function_tab_previews_and_moves_the_callers_swatch: {err}");
            return;
        }
    };
    let (dir, mut world) = project("matgraph-w11-fn-preview");
    world.insert_resource(device);
    ok(
        &mut world,
        "editor.matgraph_open",
        json!({ "path": "assets/m/g.matgraph.ron" }),
    );
    let swatch_before = ok(
        &mut world,
        "editor.matgraph_node_preview",
        json!({ "node": "w", "size": 64 }),
    );
    ok(
        &mut world,
        "editor.matgraph_function_open",
        json!({ "path": "assets/m/wear.matfunc.ron" }),
    );
    let half = ok(&mut world, "editor.matgraph_preview", json!({ "shape": "cube" }));
    assert!(half["coverage"].as_f64().unwrap_or(0.0) > 0.1, "{half}");
    ok(
        &mut world,
        "editor.matgraph_edit",
        json!({ "op": "set", "name": "k", "node": "Float(1.0)" }),
    );
    let full = ok(&mut world, "editor.matgraph_preview", json!({ "shape": "cube" }));
    let brightness = |v: &Value| v["mean_rgb"][0].as_f64().unwrap_or(0.0);
    assert!(
        brightness(&full) > brightness(&half) + 0.03,
        "the tab's preview follows its edit: {} → {}",
        brightness(&half),
        brightness(&full)
    );
    select_kind(&mut world, "graph");
    let swatch_after = ok(
        &mut world,
        "editor.matgraph_node_preview",
        json!({ "node": "w", "size": 64 }),
    );
    assert_ne!(
        swatch_after["key"], swatch_before["key"],
        "the caller's swatch hash moves with the function tab"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -- (3) Collapse with several outputs ----------------------------------------------------------

/// uv.x feeds both the lerp and a roughness product: collapsing {uv, u, rough} makes a
/// function with two outputs (u, rough) that share uv and u. Each node is emitted once, so the
/// compiled WGSL equals the code before (comments aside).
#[test]
fn a_two_output_collapse_compiles_to_the_same_code() {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    ok(&mut world, "editor.matgraph_open", json!({ "create": true }));
    let graph = regia_matgraph::MaterialGraph::from_ron(
        r#"(
            format_version: 4,
            nodes: {
                "vcol": VertexColor,
                "rgb": Swizzle(input: "vcol", channels: "rgb"),
                "uv": TexCoord,
                "u": Swizzle(input: "uv", channels: "x"),
                "white": Vec3((1.0, 1.0, 1.0)),
                "base": Lerp(a: "rgb", b: "white", t: "u"),
                "half": Float(0.5),
                "rough": Mul("u", "half"),
            },
            output: (base_color: Some("base"), roughness: Some("rough")),
        )"#,
    )
    .expect("parses");
    edit(&mut world, "fixture", None, move |d| {
        d.graph = graph;
        Ok(())
    })
    .expect("fixture");
    let before = wgsl(&mut world);
    let r = ok(
        &mut world,
        "editor.matgraph_function_collapse",
        json!({ "nodes": ["uv", "u", "half", "rough"], "name": "coord" }),
    );
    assert_eq!(r["calls"], json!(["rough", "u"]), "{r}");
    assert_eq!(r["wgsl_identical"], true, "{r}");
    assert_eq!(wgsl(&mut world), before);
}

// -- (4) WGSL view and Function users -------------------------------------------------------------

/// The panel's WGSL view shows the text `editor.matgraph_compile { include_wgsl }` answers, and
/// its Function users list is `editor.matgraph_function_users`'s.
#[test]
fn the_wgsl_view_and_function_users_match_the_agent() {
    let (dir, mut world) = project("matgraph-w11-views");
    ok(
        &mut world,
        "editor.matgraph_open",
        json!({ "path": "assets/m/g.matgraph.ron" }),
    );
    let compiled = ok(
        &mut world,
        "editor.matgraph_compile",
        json!({ "include_wgsl": true }),
    );
    let shown = panel::wgsl_view_text(&mut world.resource_mut::<MatGraphEditor>()).expect("wgsl view");
    assert_eq!(Some(shown.as_str()), compiled["wgsl"].as_str());
    ok(
        &mut world,
        "editor.matgraph_function_open",
        json!({ "path": "assets/m/wear.matfunc.ron" }),
    );
    let users = ok(&mut world, "editor.matgraph_function_users", json!({}));
    let listed = panel::function_users_view(&mut world).expect("users view");
    let agent: Vec<(String, String, Value)> = users["users"]
        .as_array()
        .expect("users")
        .iter()
        .map(|u| {
            (
                u["path"].as_str().unwrap_or_default().to_string(),
                u["function"].as_str().unwrap_or_default().to_string(),
                u["calls"].clone(),
            )
        })
        .collect();
    let rows: Vec<(String, String, Value)> = listed
        .iter()
        .map(|u| (u.path.display().to_string(), u.function.clone(), json!(u.calls)))
        .collect();
    assert_eq!(rows, agent);
    assert_eq!(rows.len(), 1, "g.matgraph.ron calls wear: {users}");
    // A function compiles in its harness: the WGSL view shows that code too.
    let compiled = ok(
        &mut world,
        "editor.matgraph_compile",
        json!({ "include_wgsl": true }),
    );
    let shown = panel::wgsl_view_text(&mut world.resource_mut::<MatGraphEditor>()).expect("wgsl view");
    assert_eq!(Some(shown.as_str()), compiled["wgsl"].as_str());
    let _ = std::fs::remove_dir_all(&dir);
}

// -- (5) Visible swatches stay --------------------------------------------------------------------

/// A 400-node graph with 300 nodes on screen: all 300 visible swatches stay, whatever order
/// the swatches land in; only off-screen ones are evicted, down to the cap.
#[test]
fn visible_swatches_stay_past_the_cap() {
    let ctx = egui::Context::default();
    let texture = |name: &str| {
        ctx.load_texture(
            name,
            egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        )
    };
    let cap = node_preview::NODE_PREVIEW_SWATCH_CAP;
    let names: Vec<String> = (0..400).map(|i| format!("n{i}")).collect();
    let key = |i: usize| (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Visible: every node but each fourth (300), so on-screen and off-screen swatches interleave.
    let visible: BTreeSet<String> = names
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 4 != 3)
        .map(|(_, n)| n.clone())
        .collect();
    assert_eq!(visible.len(), 300);
    let mut cache = node_preview::NodePreviewCache::default();
    cache.set_visible(visible.iter().cloned());
    for (i, name) in names.iter().enumerate() {
        cache.settle(name, 1, Ok(key(i)), Some(texture(name)));
    }
    let kept = visible.iter().filter(|n| cache.swatch(n).is_some()).count();
    assert_eq!(kept, 300, "every visible swatch stays");
    assert_eq!(cache.swatch_count(), 300, "the off-screen ones went");
    // The view moves: the first 100 nodes only are on screen; the next settle trims to the cap
    // and keeps the 75 of them that still have a swatch (each fourth one went off-screen above).
    cache.set_visible(names[..100].iter().cloned());
    cache.settle("extra", 1, Ok(1), Some(texture("extra")));
    assert_eq!(cache.swatch_count(), cap);
    assert!(
        names[..100]
            .iter()
            .filter(|n| visible.contains(*n))
            .all(|n| cache.swatch(n).is_some()),
        "the visible ones stay"
    );
    assert!(cache.swatch("extra").is_some(), "the newest stays");
}
