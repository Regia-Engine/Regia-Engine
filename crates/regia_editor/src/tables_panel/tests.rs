// SPDX-License-Identifier: MPL-2.0
//! Tables panel tests: headless drawing, the agent surface, and spec 51 test 20 (open a table, edit a cell,
//! the Pass/Fail coloring follows the test context).

use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::{draw_frames, has_color};

const GAIT: &str = r#"(
    format_version: 1,
    mode: First,
    result: Value(Name),
    inputs: { "armed": Bool, "speed": Float },
    columns: [
        (id: "armed", input: "armed", kind: Bool),
        (id: "speed", input: "speed", kind: FloatRange),
    ],
    rows: [
        (id: "run", cells: { "armed": True, "speed": (2.0, 10.0) }, result: Value("run")),
        (id: "walk", cells: { "speed": (0.0, 2.0) }, result: Value("walk")),
        (id: "more", result: Chooser("tables/more.chooser.ron")),
    ],
    catch_all: Some((result: Value("idle"))),
)"#;

const MORE: &str = r#"(format_version: 1, result: Value(Name), rows: [(id: "any", result: Value("dance"))])"#;

const CURVES: &str = r#"(
    format_version: 1,
    interp: Linear,
    curves: {
        "falloff": (keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
        "xp": (interp: Cubic, post: Linear, keys: [(1, 0), (2, 100), (3, 250)]),
    },
)"#;

fn project(name: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    let tables = dir.join("assets/tables");
    std::fs::create_dir_all(&tables).expect("dir");
    std::fs::write(tables.join("gait.chooser.ron"), GAIT).expect("chooser");
    std::fs::write(tables.join("more.chooser.ron"), MORE).expect("nested");
    std::fs::write(tables.join("damage.curves.ron"), CURVES).expect("curves");
    std::fs::write(tables.join("ramp.curves.csv"), "name,0,1\nramp,0,2\n").expect("csv");
    dir
}

fn world_with(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    method(name)(world, &params)
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn cell<'a>(state: &'a Value, row: &str, column: &str) -> &'a Value {
    state["chooser"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == row))
        .and_then(|r| r["cells"].as_array())
        .and_then(|cells| cells.iter().find(|c| c["column"] == column))
        .expect("cell")
}

fn draw(world: &mut World) -> Vec<egui::ClippedPrimitive> {
    draw_frames(world, 2, tables_panel)
}

/// Spec 51 test 20: open a table, edit a cell, and the Pass/Fail coloring changes with the test context.
#[test]
fn smoke_open_edit_a_cell_and_the_coloring_follows_the_context() {
    let dir = project("tables-panel-smoke");
    let mut world = world_with(&dir);
    assert!(!draw(&mut world).is_empty(), "draws with nothing open");
    let v = call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("open");
    assert_eq!(v["tab"], "chooser");
    assert_eq!(v["chooser"]["nested"], json!(["tables/more.chooser.ron"]));

    // armed + fast: `run` passes both gates and is selected.
    let v = call(
        &mut world,
        "editor.tables_panel_set_context",
        json!({ "context": { "armed": true, "speed": 5.0 } }),
    )
    .expect("context");
    assert_eq!(v["chooser"]["outcome"]["row"], "run");
    assert_eq!(cell(&v, "run", "armed")["state"], "pass");
    assert_eq!(cell(&v, "run", "speed")["state"], "pass");
    let shapes = draw(&mut world);
    assert!(has_color(&shapes, PASS_FILL), "pass cells are drawn green");
    assert!(has_color(&shapes, SELECTED_FILL), "the selected row is marked");

    // Unarmed: `run` fails on `armed`, `walk` fails on `speed`, the nested `more` row answers.
    let v = call(
        &mut world,
        "editor.tables_panel_set_context",
        json!({ "context": { "armed": false } }),
    )
    .expect("context");
    assert_eq!(cell(&v, "run", "armed")["state"], "fail");
    assert_eq!(cell(&v, "walk", "speed")["state"], "fail");
    assert_eq!(v["chooser"]["outcome"]["row"], "more");
    assert!(
        v["chooser"]["outcome"]["result"]
            .as_str()
            .is_some_and(|r| r.contains("dance")),
        "{}",
        v["chooser"]["outcome"]
    );
    assert!(
        has_color(&draw(&mut world), FAIL_FILL),
        "fail cells are drawn red"
    );

    // Edit a cell: `walk` now covers speed 5, so it passes where it failed.
    let v = call(
        &mut world,
        "editor.tables_panel_chooser_edit",
        json!({ "op": "set_cell", "row": "walk", "column": "speed", "value": [0.0, 6.0] }),
    )
    .expect("edit");
    assert_eq!(cell(&v, "walk", "speed")["state"], "pass");
    assert_eq!(v["chooser"]["outcome"]["row"], "walk");
    assert_eq!(v["chooser"]["dirty"], true);
    assert_eq!(v["chooser"]["generation"], 1);

    // The same edit in RON text form, as the grid's text box sends it.
    let v = call(
        &mut world,
        "editor.tables_panel_chooser_edit",
        json!({ "op": "set_cell", "row": "walk", "column": "speed", "text": "(0.0, 1.0)" }),
    )
    .expect("text edit");
    assert_eq!(cell(&v, "walk", "speed")["state"], "fail");
    assert!(!draw(&mut world).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rows_and_columns_toggle_reorder_and_undo() {
    let dir = project("tables-panel-rows");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("open");
    call(
        &mut world,
        "editor.tables_panel_set_context",
        json!({ "context": { "armed": true, "speed": 1.0 } }),
    )
    .expect("context");
    // A disabled column is skipped: `run` no longer fails on speed 1.
    let v = call(
        &mut world,
        "editor.tables_panel_chooser_edit",
        json!({ "op": "set_column", "column": "speed", "disabled": true }),
    )
    .expect("column");
    assert_eq!(cell(&v, "run", "speed")["state"], "skipped");
    assert_eq!(v["chooser"]["outcome"]["row"], "run");
    // A disabled row is not a candidate.
    let v = call(
        &mut world,
        "editor.tables_panel_chooser_edit",
        json!({ "op": "set_row", "row": "run", "enabled": false }),
    )
    .expect("row");
    assert_eq!(v["chooser"]["rows"][0]["state"], "disabled");
    assert_eq!(v["chooser"]["outcome"]["row"], "walk");
    // Reorder: `more` first, then it wins in First mode.
    let v = call(
        &mut world,
        "editor.tables_panel_chooser_edit",
        json!({ "op": "move_row", "row": "more", "before": "run" }),
    )
    .expect("move");
    let ids: Vec<&str> = v["chooser"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(ids, ["more", "run", "walk"]);
    assert_eq!(v["chooser"]["outcome"]["row"], "more");
    // Three undos bring the table back.
    for _ in 0..3 {
        call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    }
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["chooser"]["outcome"]["row"], "walk", "speed 1 is a walk again");
    assert_eq!(v["chooser"]["undo_depth"], 0);
    let e = call(&mut world, "editor.tables_panel_undo", json!({})).expect_err("empty");
    assert_eq!(reason(&e), "tables_panel_nothing_to_undo");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refused_edits_and_bad_contexts_change_nothing() {
    let dir = project("tables-panel-refused");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("open");
    let before = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    for (params, expected) in [
        (
            json!({ "op": "set_cell", "row": "nope", "column": "speed", "value": [0, 1] }),
            "tables_panel_row_not_found",
        ),
        (
            json!({ "op": "set_cell", "row": "run", "column": "nope", "value": true }),
            "tables_panel_column_not_found",
        ),
        (
            json!({ "op": "set_cell", "row": "run", "column": "armed", "value": [1, 2, 3] }),
            "bad_value",
        ),
        (json!({ "op": "explode" }), "tables_panel_bad_op"),
        (
            json!({ "op": "move_row", "row": "run", "before": "nope" }),
            "tables_panel_row_not_found",
        ),
    ] {
        let e = call(&mut world, "editor.tables_panel_chooser_edit", params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    for (params, expected) in [
        (json!({ "context": { "nope": 1 } }), "tables_panel_unknown_input"),
        (
            json!({ "context": { "armed": "yes" } }),
            "tables_panel_bad_context",
        ),
        (json!({ "context": 3 }), "tables_panel_bad_context"),
        (json!({ "seed": -1 }), "tables_panel_bad_context"),
    ] {
        let e = call(&mut world, "editor.tables_panel_set_context", params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    let mut after = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    // The error line is the only difference.
    assert_eq!(after["error"]["code"], "tables_panel_bad_context");
    after["error"] = before["error"].clone();
    assert_eq!(after, before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn curve_edits_sample_with_the_library_and_save_round_trips() {
    let dir = project("tables-panel-curves");
    let mut world = world_with(&dir);
    let v = call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/damage.curves.ron" }),
    )
    .expect("open");
    assert_eq!(v["tab"], "curves");
    assert_eq!(v["curves"]["selected"], "falloff");
    assert!(!draw(&mut world).is_empty());
    let edit = |world: &mut World, params: Value| call(world, "editor.tables_panel_curve_edit", params);
    edit(
        &mut world,
        json!({ "op": "set_key", "name": "falloff", "x": 20.0, "y": 0.5 }),
    )
    .expect("add");
    let v = edit(
        &mut world,
        json!({ "op": "move_key", "name": "falloff", "from_x": 20.0, "x": 15.0, "y": 0.4 }),
    )
    .expect("move");
    assert_eq!(v["curves"]["selected_key"], 2);
    let keys = &v["curves"]["curves"][0]["keys"];
    assert_eq!(keys.as_array().map(Vec::len), Some(4));
    assert_eq!(keys[2]["x"], 15.0);
    edit(
        &mut world,
        json!({ "op": "set_curve", "name": "falloff", "post": "linear", "rename": "drop" }),
    )
    .expect("curve");
    let v = edit(
        &mut world,
        json!({ "op": "remove_key", "name": "drop", "x": 0.0 }),
    )
    .expect("remove");
    assert_eq!(
        v["curves"]["selected"], "drop",
        "the selection follows the rename"
    );
    // A duplicate x is refused and changes nothing.
    let e = edit(
        &mut world,
        json!({ "op": "move_key", "name": "drop", "from_x": 10.0, "x": 15.0, "y": 0.0 }),
    )
    .expect_err("duplicate");
    assert_eq!(reason(&e), "curve_duplicate_x");
    let e = edit(
        &mut world,
        json!({ "op": "set_key", "name": "nope", "x": 1.0, "y": 1.0 }),
    )
    .expect_err("missing");
    assert_eq!(reason(&e), "curve_not_found");
    assert!(!draw(&mut world).is_empty());
    let saved = call(&mut world, "editor.tables_panel_save", json!({})).expect("save");
    assert_eq!(saved["written"], "tables/damage.curves.ron");
    let text = std::fs::read_to_string(dir.join("assets/tables/damage.curves.ron")).expect("read");
    let table = CurveTable::from_ron_str(&text).expect("parses");
    let drop = table.curve("drop").expect("renamed curve");
    assert_eq!(drop.post, Extrap::Linear);
    assert_eq!(
        drop.keys.iter().map(|k| k.x).collect::<Vec<_>>(),
        [10.0, 15.0, 30.0]
    );
    // What the panel sampled is what the library samples from the saved file.
    let panel = world.resource::<TablesPanel>();
    let open = &panel.curves.as_ref().expect("open").table;
    for x in [-5.0, 12.5, 29.0, 45.0] {
        assert_eq!(
            open.sample_named("drop", x),
            table.sample_named("drop", x),
            "x = {x}"
        );
    }
    assert_eq!(world.resource::<TablesPanel>().json()["curves"]["dirty"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn csv_tables_save_as_csv_and_import_replaces_the_table() {
    let dir = project("tables-panel-csv");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/ramp.curves.csv" }),
    )
    .expect("open");
    let v = call(
        &mut world,
        "editor.tables_panel_curve_edit",
        json!({ "op": "import_csv", "csv": "name,0,2\nramp,0,4\nflat,1,1\n" }),
    )
    .expect("import");
    assert_eq!(v["curves"]["curves"].as_array().map(Vec::len), Some(2));
    let e = call(
        &mut world,
        "editor.tables_panel_curve_edit",
        json!({ "op": "import_csv", "csv": "name,0,1\nramp,0,not a number\n" }),
    )
    .expect_err("bad csv");
    assert!(reason(&e).starts_with("curve_"), "{}", reason(&e));
    call(&mut world, "editor.tables_panel_save", json!({ "tab": "curves" })).expect("save");
    let text = std::fs::read_to_string(dir.join("assets/tables/ramp.curves.csv")).expect("read");
    let table = CurveTable::from_csv_str(&text, TableInterp::Linear).expect("csv");
    assert_eq!(table.sample_named("ramp", 1.0), Some(2.0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn paths_are_confined_and_errors_are_coded() {
    let dir = project("tables-panel-paths");
    let mut world = world_with(&dir);
    for (path, expected) in [
        ("../outside.chooser.ron", "tables_panel_bad_path"),
        ("/etc/a.curves.ron", "tables_panel_bad_path"),
        ("tables/none.chooser.ron", "tables_panel_file_not_found"),
        ("tables/readme.txt", "tables_panel_bad_path"),
    ] {
        let e = call(&mut world, "editor.tables_panel_open", json!({ "path": path })).expect_err(path);
        assert_eq!(reason(&e), expected, "{path}");
    }
    std::fs::write(
        dir.join("assets/tables/broken.chooser.ron"),
        "(format_version: 99)",
    )
    .expect("write");
    let e = call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/broken.chooser.ron" }),
    )
    .expect_err("too new");
    assert!(reason(&e).starts_with("chooser_"), "{}", reason(&e));
    for name in [
        "editor.tables_panel_chooser_edit",
        "editor.tables_panel_set_context",
    ] {
        let e = call(&mut world, name, json!({ "op": "set_row", "row": "a" })).expect_err("nothing open");
        assert_eq!(reason(&e), "tables_panel_no_table", "{name}");
    }
    let e = call(&mut world, "editor.tables_panel_save", json!({ "tab": "nope" })).expect_err("tab");
    assert_eq!(reason(&e), "tables_panel_bad_value");
    let mut empty = World::new();
    let e = call(
        &mut empty,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect_err("no project");
    assert_eq!(reason(&e), "tables_panel_no_project");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_method_is_registered_once_with_its_class() {
    let list = methods();
    // Seven of spec 51 § 6, the Data tab's `editor.tables_panel_data_edit` (ADR-2132) and the Import and
    // Export pair (ADR-7855).
    assert_eq!(list.len(), 10);
    assert!(
        list.iter()
            .any(|(m, _)| m.name == "editor.tables_panel_data_edit" && m.permission == Permission::EditScene)
    );
    for (schema, _) in &list {
        assert!(schema.name.starts_with("editor.tables_panel_"), "{}", schema.name);
        assert_eq!(
            schema.mutating,
            schema.name != "editor.tables_panel_get",
            "{}",
            schema.name
        );
    }
    let mut names: Vec<_> = list.iter().map(|(m, _)| m.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 10);
}

#[test]
fn tangents_set_and_clear_and_a_drag_is_one_undo_step() {
    let dir = project("tables-panel-tangents");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/damage.curves.ron" }),
    )
    .expect("open");
    let edit = |world: &mut World, params: Value| call(world, "editor.tables_panel_curve_edit", params);
    // The derived slope of a linear segment is the segment's slope.
    {
        let panel = world.resource::<TablesPanel>();
        let doc = panel.curves.as_ref().expect("open");
        let curve = doc.table.curve("falloff").expect("curve");
        assert!((display_slope(&doc.table, curve, 0, true) - (-0.04)).abs() < 1e-4);
        assert_eq!(
            display_slope(&doc.table, curve, 0, false),
            0.0,
            "no segment before the first key"
        );
    }
    let v = edit(
        &mut world,
        json!({ "op": "set_tangent", "name": "falloff", "x": 10.0, "slope": -0.05 }),
    )
    .expect("both");
    let key = &v["curves"]["curves"][0]["keys"][1];
    assert_eq!(
        (key["in_tangent"].as_f64(), key["out_tangent"].as_f64()),
        (Some(-0.05_f32 as f64), Some(-0.05_f32 as f64))
    );
    let v = edit(
        &mut world,
        json!({ "op": "set_tangent", "name": "falloff", "x": 10.0, "side": "out", "slope": null }),
    )
    .expect("clear out");
    let key = &v["curves"]["curves"][0]["keys"][1];
    assert!(key["out_tangent"].is_null() && key["in_tangent"].as_f64().is_some());
    {
        let panel = world.resource::<TablesPanel>();
        let doc = panel.curves.as_ref().expect("open");
        let curve = doc.table.curve("falloff").expect("curve");
        assert_eq!(
            display_slope(&doc.table, curve, 1, false),
            -0.05,
            "an explicit tangent is shown as is"
        );
    }
    for (params, expected) in [
        (
            json!({ "op": "set_tangent", "name": "falloff", "x": 10.0, "side": "up", "slope": 1.0 }),
            "tables_panel_bad_value",
        ),
        (
            json!({ "op": "set_tangent", "name": "falloff", "x": 10.0 }),
            "tables_panel_bad_value",
        ),
        (
            json!({ "op": "set_tangent", "name": "falloff", "x": 11.0, "slope": 1.0 }),
            "key_not_found",
        ),
    ] {
        let e = edit(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    // Three steps of one drag are one undo step; the undo restores the key before the drag.
    let depth = |world: &mut World| {
        call(world, "editor.tables_panel_get", json!({})).expect("get")["curves"]["undo_depth"]
            .as_u64()
            .expect("depth")
    };
    let before = depth(&mut world);
    let mut from = 10.0;
    for x in [11.0, 12.0, 13.0] {
        edit(
            &mut world,
            json!({ "op": "move_key", "name": "falloff", "from_x": from, "x": x, "y": 0.5, "gesture": 7 }),
        )
        .expect("drag");
        from = x;
    }
    assert_eq!(depth(&mut world), before + 1);
    let v = call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    assert_eq!(v["curves"]["curves"][0]["keys"][1]["x"], 10.0);
    assert!(!draw(&mut world).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent side of a chooser byte-parity check: a world whose `Choosers` resource holds the
/// same table the panel opened, with the `chooser.*` methods installed.
fn chooser_agent_world() -> World {
    use regia_agent::choosers::install_chooser_methods;
    let mut world = World::new();
    let mut choosers = regia_framework::chooser::Choosers::default();
    choosers.insert(
        "tables/gait.chooser.ron",
        ChooserTable::from_ron_str(GAIT).expect("table"),
    );
    world.insert_resource(choosers);
    install_chooser_methods(&mut world);
    world
}

fn agent_call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let handler = world
        .resource::<regia_agent::extension::AgentExtension>()
        .method(method)
        .map(|(_, handler)| handler)
        .expect("registered");
    handler(world, &params)
}

/// The Chooser tab's add and remove row and column, result and options edits write the same file
/// bytes the `chooser.*` methods write for the same change, one undo step each.
#[test]
fn chooser_rows_columns_results_options_match_the_agent_bytes() {
    let dir = project("tables-panel-chooser-parity");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("open");
    // (label, panel params, agent params without the path)
    let edits: &[(&str, Value, Value)] = &[
        (
            "create a row with a result",
            json!({ "op": "set_row", "row": "sneak", "result": { "ron": "Value(\"sneak\")" } }),
            json!({ "row": "sneak", "result": { "ron": "Value(\"sneak\")" } }),
        ),
        (
            "update the row",
            json!({ "op": "set_row", "row": "sneak", "enabled": false, "note": "quiet" }),
            json!({ "row": "sneak", "enabled": false, "note": "quiet" }),
        ),
        (
            "edit a row's result",
            json!({ "op": "set_row", "row": "run", "result": { "ron": "Value(\"sprint\")" } }),
            json!({ "row": "run", "result": { "ron": "Value(\"sprint\")" } }),
        ),
        (
            "remove a row",
            json!({ "op": "remove_row", "row": "walk" }),
            json!({ "row": "walk" }),
        ),
        (
            "add a column",
            json!({ "op": "set_column", "column": { "id": "sneaky", "kind": "Bool", "input": "armed" } }),
            json!({ "column": { "id": "sneaky", "kind": "Bool", "input": "armed" } }),
        ),
        (
            "replace the column disabled",
            json!({ "op": "set_column", "column": { "id": "sneaky", "kind": "Bool", "input": "armed", "disabled": true } }),
            json!({ "column": { "id": "sneaky", "kind": "Bool", "input": "armed", "disabled": true } }),
        ),
        (
            "remove a column",
            json!({ "op": "remove_column", "column": "speed" }),
            json!({ "column": "speed" }),
        ),
        (
            "set the mode and draw options",
            json!({ "op": "set_options", "mode": "Best", "near_best": 0.5, "recency": { "window": 2, "penalty": 0.25 } }),
            json!({ "mode": "Best", "near_best": 0.5, "recency": { "window": 2, "penalty": 0.25 } }),
        ),
        (
            "set the catch-all",
            json!({ "op": "set_options", "catch_all": { "result": { "ron": "Value(\"idle\")" } } }),
            json!({ "catch_all": { "result": { "ron": "Value(\"idle\")" } } }),
        ),
    ];
    let mut agent = chooser_agent_world();
    for (label, panel_params, agent_params) in edits {
        let mut agent_params = agent_params.clone();
        call(
            &mut world,
            "editor.tables_panel_chooser_edit",
            panel_params.clone(),
        )
        .unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let method = match panel_params["op"].as_str() {
            Some("remove_row") => "chooser.remove_row",
            Some("remove_column") => "chooser.remove_column",
            Some("set_options") => "chooser.set_options",
            Some("set_column") => "chooser.set_column",
            _ => "chooser.set_row",
        };
        agent_params["path"] = json!("tables/gait.chooser.ron");
        agent_call(&mut agent, method, agent_params).unwrap_or_else(|e| panic!("agent {label}: {e:?}"));
    }
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(
        v["chooser"]["undo_depth"],
        edits.len() as u64,
        "one undo step per edit"
    );
    let saved = call(&mut world, "editor.tables_panel_save", json!({})).expect("save");
    assert_eq!(saved["written"], "tables/gait.chooser.ron");
    let panel_bytes = std::fs::read_to_string(dir.join("assets/tables/gait.chooser.ron")).expect("read");
    let agent_text = agent_call(
        &mut agent,
        "chooser.export",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("export")["text"]
        .as_str()
        .expect("text")
        .to_owned();
    assert_eq!(
        panel_bytes, agent_text,
        "the panel's save writes the agent's bytes"
    );
    // Every undo is one step back; after all of them the table is the file's again.
    for _ in 0..edits.len() {
        call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    }
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["chooser"]["undo_depth"], 0);
    let restored = ChooserTable::from_ron_str(&panel_bytes).expect("edited parses");
    let original = ChooserTable::from_ron_str(GAIT).expect("original parses");
    assert_ne!(restored, original, "the edits changed the table");
    {
        let panel = world.resource::<TablesPanel>();
        assert_eq!(panel.chooser.as_ref().expect("open").table, original);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Curves tab's add, rename and delete writes are the `curves.*` methods' writes; a rename to
/// an existing name is refused with the library's code on both sides.
#[test]
fn curve_add_rename_delete_match_the_agent_bytes() {
    let dir = project("tables-panel-curve-parity");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/damage.curves.ron" }),
    )
    .expect("open");
    let panel_edit = |world: &mut World, params: Value| call(world, "editor.tables_panel_curve_edit", params);
    // (label, panel params, agent method, agent params without the path). The panel's add button
    // creates the curve with one key at (0, 0), so the agent side passes that key explicitly.
    let edits: &[(&str, Value, &str, Value)] = &[
        (
            "add a curve",
            json!({ "op": "set_curve", "name": "hp" }),
            "curves.set_curve",
            json!({ "name": "hp", "keys": [[0.0, 0.0]] }),
        ),
        (
            "key it",
            json!({ "op": "set_key", "name": "hp", "x": 100.0, "y": 1.0 }),
            "curves.set_key",
            json!({ "name": "hp", "x": 100.0, "y": 1.0 }),
        ),
        (
            "rename it",
            json!({ "op": "set_curve", "name": "hp", "rename": "health" }),
            "curves.set_curve",
            json!({ "name": "hp", "rename": "health" }),
        ),
        (
            "add one to delete",
            json!({ "op": "set_curve", "name": "mana" }),
            "curves.set_curve",
            json!({ "name": "mana", "keys": [[0.0, 0.0]] }),
        ),
        (
            "delete it",
            json!({ "op": "delete", "name": "mana" }),
            "curves.delete",
            json!({ "name": "mana" }),
        ),
    ];
    let mut agent = {
        use regia_agent::curve_tables::install_curve_table_methods;
        let mut world = World::new();
        let mut tables = regia_framework::curve_table::CurveTables::default();
        tables.insert(
            "tables/damage.curves.ron",
            CurveTable::from_ron_str(CURVES).expect("table"),
        );
        world.insert_resource(tables);
        install_curve_table_methods(&mut world);
        world
    };
    for (label, panel_params, method, agent_params) in edits {
        let mut agent_params = agent_params.clone();
        panel_edit(&mut world, panel_params.clone()).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        agent_params["path"] = json!("tables/damage.curves.ron");
        agent_call(&mut agent, method, agent_params).unwrap_or_else(|e| panic!("agent {label}: {e:?}"));
    }
    // A rename to an existing name is refused with the library's code on both sides.
    let e = panel_edit(
        &mut world,
        json!({ "op": "set_curve", "name": "health", "rename": "xp" }),
    )
    .expect_err("duplicate");
    assert_eq!(reason(&e), "curve_duplicate_name");
    let mut agent_params = json!({ "name": "health", "rename": "xp" });
    agent_params["path"] = json!("tables/damage.curves.ron");
    let e = agent_call(&mut agent, "curves.set_curve", agent_params).expect_err("duplicate");
    assert_eq!(reason(&e), "curve_duplicate_name");
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(
        v["curves"]["undo_depth"],
        edits.len() as u64,
        "the refusal added no step"
    );
    call(&mut world, "editor.tables_panel_save", json!({})).expect("save");
    let panel_bytes = std::fs::read_to_string(dir.join("assets/tables/damage.curves.ron")).expect("read");
    let agent_text = agent_call(
        &mut agent,
        "curves.export",
        json!({ "path": "tables/damage.curves.ron", "format": "ron" }),
    )
    .expect("export")["text"]
        .as_str()
        .expect("text")
        .to_owned();
    assert_eq!(
        panel_bytes, agent_text,
        "the panel's save writes the agent's bytes"
    );
    // The rename refusal changed nothing on either side.
    assert_eq!(
        panel_edit(&mut world, json!({ "op": "select", "name": "health" })).expect("select")["curves"]["selected"],
        "health"
    );
    for _ in 0..edits.len() {
        call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    }
    {
        let panel = world.resource::<TablesPanel>();
        let restored = &panel.curves.as_ref().expect("open").table;
        assert_eq!(
            restored.to_ron_string(),
            CurveTable::from_ron_str(CURVES)
                .expect("original")
                .to_ron_string(),
            "undo restores the opened table"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_grid_axis_is_the_union_of_key_positions() {
    let table = CurveTable::from_ron_str(CURVES).expect("curves");
    assert_eq!(grid_axis(&table), [0.0, 1.0, 2.0, 3.0, 10.0, 30.0]);
    let wide = CurveTable::new(
        TableInterp::Linear,
        vec![NamedCurve::new(
            "wide",
            (0..400).map(|i| CurveKey::new(i as f32, 0.0)).collect(),
        )],
    )
    .expect("table");
    assert_eq!(grid_axis(&wide).len(), GRID_COLUMNS);
}

/// A chooser with a `Float` input for a distance column and two declared outputs the catch-all can emit.
const EMIT: &str = r#"(
    format_version: 1,
    mode: Best,
    result: Value(Name),
    inputs: { "angle": Float },
    outputs: { "rate": Float, "stance": Name },
    columns: [(id: "rate", output: "rate", kind: SetFloat)],
    rows: [(id: "slow", cells: { "rate": 0.5 }, result: Value("walk"))],
    catch_all: Some((result: Value("idle"), outputs: { "rate": 1.0 })),
)"#;

/// The Chooser tab driven by its drawn widgets: frames run on one context, and a widget is found by
/// the text it draws.
struct ChooserTab {
    ctx: egui::Context,
    world: World,
    texts: Vec<(String, egui::Pos2)>,
}

impl ChooserTab {
    fn frame(&mut self, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(2400.0, 1400.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let world = &mut self.world;
        let mut output = self.ctx.run_ui(input, |ui| tables_panel(ui, world));
        output.textures_delta.clear();
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_string(),
                    shape.visual_bounding_rect().center(),
                )),
                _ => {}
            }
        }
        self.texts.clear();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut self.texts);
        }
    }

    fn click(&mut self, label: &str) {
        self.frame(Vec::new());
        let pos = self
            .texts
            .iter()
            .find(|(t, _)| t == label)
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("no {label:?} in {:?}", self.texts));
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        self.frame(vec![press(true)]);
        self.frame(vec![press(false)]);
        self.frame(Vec::new());
    }

    /// Commits a focused single-line box (Enter).
    fn enter(&mut self) {
        self.frame(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        self.frame(Vec::new());
    }

    fn doc(&mut self) -> &mut ChooserDoc {
        self.world
            .resource_mut::<TablesPanel>()
            .into_inner()
            .chooser
            .as_mut()
            .expect("a chooser is open")
    }

    fn error(&self) -> Option<String> {
        self.world
            .resource::<TablesPanel>()
            .error
            .as_ref()
            .map(|e| e.code.clone())
    }
}

/// w10 forms-parity item 3: the add-column box adds a `FloatDistance` column with its parameters, and the
/// catch-all's `outputs` map is edited from its value boxes (set one, clear one), each from the drawn
/// controls as one undo step; the saved file is the bytes `chooser.set_column` / `chooser.set_options`
/// write for the same changes.
#[test]
fn chooser_distance_columns_and_catch_all_outputs_match_the_agent_bytes() {
    let dir = project("tables-panel-chooser-distance");
    std::fs::write(dir.join("assets/tables/emit.chooser.ron"), EMIT).expect("emit");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/emit.chooser.ron" }),
    )
    .expect("open");
    let mut tab = ChooserTab {
        ctx: egui::Context::default(),
        world,
        texts: Vec::new(),
    };
    tab.frame(Vec::new());
    assert!(
        tab.texts.iter().any(|(t, _)| t == "Catch-all outputs"),
        "{:?}",
        tab.texts
    );
    // A FloatDistance column, tuned, bound to the Float input.
    {
        let doc = tab.doc();
        doc.new_column_id = "facing".into();
        doc.new_column_kind = DISTANCE_KIND;
        doc.new_distance = DistanceForm {
            influence: 2.0,
            max_distance: 90.0,
            wrap: Some((-180.0, 180.0)),
            drop_beyond: true,
        };
    }
    tab.click("Add column");
    assert_eq!(tab.error(), None);
    // The catch-all's outputs: set `stance`, then clear `rate`.
    tab.doc()
        .outputs_text
        .insert("stance".into(), "\"crouch\"".into());
    tab.click("\"crouch\"");
    tab.enter();
    assert_eq!(tab.error(), None);
    tab.click("1.0");
    tab.doc().outputs_text.insert("rate".into(), String::new());
    tab.enter();
    assert_eq!(tab.error(), None);
    let mut world = tab.world;
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["chooser"]["undo_depth"], 3, "one undo step per change: {v}");
    call(&mut world, "editor.tables_panel_save", json!({})).expect("save");
    let panel_bytes = std::fs::read_to_string(dir.join("assets/tables/emit.chooser.ron")).expect("read");

    let mut agent = World::new();
    let mut choosers = regia_framework::chooser::Choosers::default();
    choosers.insert(
        "tables/emit.chooser.ron",
        ChooserTable::from_ron_str(EMIT).expect("table"),
    );
    agent.insert_resource(choosers);
    regia_agent::choosers::install_chooser_methods(&mut agent);
    let path = "tables/emit.chooser.ron";
    agent_call(
        &mut agent,
        "chooser.set_column",
        json!({ "path": path, "column": { "id": "facing", "input": "angle",
            "kind": "FloatDistance(influence: 2.0, max_distance: 90.0, wrap: Some((-180.0, 180.0)), drop_beyond: true)" } }),
    )
    .expect("agent column");
    agent_call(
        &mut agent,
        "chooser.set_options",
        json!({ "path": path, "catch_all": { "result": { "ron": "Value(\"idle\")" }, "outputs": { "stance": "crouch" } } }),
    )
    .expect("agent outputs");
    let agent_text =
        agent_call(&mut agent, "chooser.export", json!({ "path": path })).expect("export")["text"]
            .as_str()
            .expect("text")
            .to_owned();
    assert_eq!(
        panel_bytes, agent_text,
        "the panel's save writes the agent's bytes"
    );
    let saved = ChooserTable::from_ron_str(&panel_bytes).expect("parses");
    assert!(
        matches!(
            saved.column("facing").map(|c| &c.kind),
            Some(regia_framework::chooser::ColumnKind::FloatDistance { max_distance, drop_beyond: true, .. })
                if *max_distance == 90.0
        ),
        "{panel_bytes}"
    );
    let outputs = &saved.catch_all.as_ref().expect("catch-all").outputs;
    assert_eq!(outputs.len(), 1, "rate cleared, stance set: {panel_bytes}");
    // A value that does not fit its output is refused with bad_value and changes nothing.
    let mut tab = ChooserTab {
        ctx: egui::Context::default(),
        world,
        texts: Vec::new(),
    };
    tab.frame(Vec::new());
    tab.doc().outputs_text.insert("rate".into(), "fast".into());
    tab.click("fast");
    tab.enter();
    assert_eq!(tab.error().as_deref(), Some("tables_panel_bad_value"));
    let v = call(&mut tab.world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["chooser"]["undo_depth"], 3, "a refused value adds no step");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-7716: a catch-all output value that does not fit its output is refused (`tables_panel_bad_value`,
/// no undo step) and its box keeps the typed text, drawn again on the next frame, so it can be fixed.
/// Valid text typed afterwards is accepted. It used to be cleared, which showed the stored value again.
#[test]
fn a_refused_catch_all_output_keeps_the_typed_text() {
    let dir = project("tables-panel-chooser-bad-output");
    std::fs::write(dir.join("assets/tables/emit.chooser.ron"), EMIT).expect("emit");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/emit.chooser.ron" }),
    )
    .expect("open");
    let mut tab = ChooserTab {
        ctx: egui::Context::default(),
        world,
        texts: Vec::new(),
    };
    tab.frame(Vec::new());
    tab.doc().outputs_text.insert("rate".into(), "fast".into());
    tab.click("fast");
    tab.enter();
    assert_eq!(tab.error().as_deref(), Some("tables_panel_bad_value"));
    assert_eq!(
        tab.doc().outputs_text.get("rate").map(String::as_str),
        Some("fast"),
        "the refused text stays in its box"
    );
    tab.frame(Vec::new());
    assert!(
        tab.texts.iter().any(|(t, _)| t == "fast"),
        "the box draws the typed text: {:?}",
        tab.texts
    );
    let v = call(&mut tab.world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["chooser"]["undo_depth"], 0, "a refused value adds no step");
    // Fixing the text applies it.
    tab.doc().outputs_text.insert("rate".into(), "2.5".into());
    tab.click("2.5");
    tab.enter();
    // (The panel's error line is only cleared by an agent call, so it still shows the refusal.)
    let v = call(&mut tab.world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(
        v["chooser"]["undo_depth"], 1,
        "the corrected value is one undo step"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A curve table exports as CSV and JSON in the layout of its CSV form, and a JSON or CSV file replaces the
/// table as one undo step; a chooser and an unknown format are refused with codes.
#[test]
fn curve_tables_export_and_import_csv_and_json() {
    let dir = project("tables-panel-curve-exchange");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/damage.curves.ron" }),
    )
    .expect("open");
    let csv = call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/damage.csv" }),
    )
    .expect("csv");
    assert_eq!(csv["format"], "csv");
    assert_eq!(csv["rows"], 2);
    let csv_text = std::fs::read_to_string(dir.join("assets/out/damage.csv")).expect("csv file");
    let table =
        regia_framework::curve_table::parse_curve_table("tables/damage.curves.ron", CURVES).expect("parses");
    assert_eq!(
        csv_text,
        table.to_csv_string().expect("csv"),
        "the CSV form of the table"
    );
    call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/damage.json" }),
    )
    .expect("json");
    let json_text = std::fs::read_to_string(dir.join("assets/out/damage.json")).expect("json file");
    let value: Value = serde_json::from_str(&json_text).expect("valid json");
    assert_eq!(value[0]["key"], "falloff");
    assert_eq!(value.as_array().map(Vec::len), Some(2));

    // A JSON file with a new curve replaces the table; one undo step brings the old one back.
    std::fs::write(
        dir.join("assets/out/new.json"),
        r#"[{"key": "ramp", "0": 0, "10": 5}]"#,
    )
    .expect("json in");
    let before = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    let done = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/new.json" }),
    )
    .expect("import");
    assert_eq!(done["imported"], true);
    assert_eq!(done["rows"], 1);
    assert_eq!(done["undo_depth"], 1);
    let after = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(after["curves"]["curves"].as_array().map(Vec::len), Some(1));
    assert_eq!(after["curves"]["dirty"], true);
    call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    let undone = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(
        undone["curves"]["curves"], before["curves"]["curves"],
        "one undo step"
    );
    // The CSV file we exported imports back to the same table.
    call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/damage.csv" }),
    )
    .expect("csv in");
    let round = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(round["curves"]["curves"].as_array().map(Vec::len), Some(2));

    // Refusals.
    let e = call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/x.csv", "format": "yaml" }),
    )
    .expect_err("format");
    assert_eq!(reason(&e), "tables_panel_bad_format");
    let e = call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/x" }),
    )
    .expect_err("no format");
    assert_eq!(reason(&e), "tables_panel_bad_format");
    let e = call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/x.ron" }),
    )
    .expect_err("curve ron");
    assert_eq!(reason(&e), "tables_panel_not_exchangeable");
    std::fs::write(dir.join("assets/out/bad.json"), "{}").expect("bad json");
    let e = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/bad.json" }),
    )
    .expect_err("bad");
    assert_eq!(reason(&e), "table_bad_file");
    let e = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/missing.csv" }),
    )
    .expect_err("missing");
    assert_eq!(reason(&e), "tables_panel_file_not_found");
    let e = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "../outside.csv" }),
    )
    .expect_err("outside");
    assert!(reason(&e).contains("path_outside_project"), "{e:?}");
    call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/gait.chooser.ron" }),
    )
    .expect("chooser");
    let e = call(
        &mut world,
        "editor.tables_panel_export",
        json!({ "path": "out/g.csv" }),
    )
    .expect_err("chooser");
    assert_eq!(reason(&e), "tables_panel_not_exchangeable");
}
