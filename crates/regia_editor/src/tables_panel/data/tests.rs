// SPDX-License-Identifier: MPL-2.0
//! The Data tab, driven headless through `editor.tables_panel_*` as an agent would: open a typed data
//! table, read its grid and schema errors, edit cells and rows (a refused edit changes nothing), undo,
//! save, and see the error badges drawn.

use std::path::Path;

use regia_asset::data_table::{DataTable, DataTables};
use regia_ecs::reflect::{AppTypeRegistry, Reflect};
use serde_json::{Value, json};

use super::super::{TablesPanel, methods, tables_panel};
use super::*;
use crate::data_panels::test_ui::{draw_frames, has_color};
use crate::viewport_agent::Handler;
use regia_agent::protocol::RpcError;

#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
enum Kind {
    #[default]
    Rifle,
    Pistol,
}

/// One weapon.
#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
struct Weapon {
    /// Damage per hit.
    damage: u32,
    kind: Kind,
    label: String,
    #[serde(default)]
    tier: Option<u8>,
}

const WEAPONS: &str = r#"(
    format_version: 1,
    row_type: "Weapon",
    rows: {
        "rifle": (damage: 30, kind: Rifle, label: "Rifle"),
        "pistol": (damage: 12, kind: Pistol, label: "Pistol", tier: Some(2)),
        "odd": (damage: 1, kind: Rifle, label: "Odd", colour: "red"),
    },
)"#;

const PATH: &str = "tables/weapons.table.ron";

fn project(name: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    let tables = dir.join("assets/tables");
    std::fs::create_dir_all(&tables).expect("dir");
    std::fs::write(tables.join("weapons.table.ron"), WEAPONS).expect("weapons");
    std::fs::write(
        tables.join("notes.table.csv"),
        "key,damage,kind,label\r\na,1,Rifle,42\r\nb,2,Pistol,plain text\r\n",
    )
    .expect("csv");
    dir
}

fn world_with(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

/// The editor world a game plugin set up: the table is loaded with its row struct and a rule.
fn with_loaded_table(world: &mut World) {
    let table = DataTable::<Weapon>::new().with_rule(|_, row: &Weapon| {
        if row.damage > 500 {
            Err("damage above 500".to_owned())
        } else {
            Ok(())
        }
    });
    let mut tables = DataTables::default();
    tables.insert(PATH, table);
    world.insert_resource(tables);
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let handler: Handler = methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered");
    handler(world, &params)
}

fn edit(world: &mut World, params: Value) -> Result<Value, RpcError> {
    call(world, "editor.tables_panel_data_edit", params)
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn row<'a>(state: &'a Value, key: &str) -> &'a Value {
    state["data"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["key"] == key))
        .unwrap_or_else(|| panic!("no row {key} in {}", state["data"]))
}

fn cell<'a>(state: &'a Value, key: &str, column: &str) -> &'a Value {
    row(state, key)["cells"]
        .as_array()
        .and_then(|cells| cells.iter().find(|c| c["column"] == column))
        .unwrap_or_else(|| panic!("no cell {key}.{column}"))
}

fn keys(state: &Value) -> Vec<String> {
    state["data"]["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["key"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn draw(world: &mut World) -> Vec<egui::ClippedPrimitive> {
    draw_frames(world, 2, tables_panel)
}

/// Open a typed table, see its schema error badged, edit a cell, have a bad edit refused, fix the error,
/// undo, save, and read the saved file back with the row struct.
#[test]
fn smoke_open_a_data_table_edit_cells_refuse_schema_errors_undo_and_save() {
    let dir = project("tables-panel-data-smoke");
    let mut world = world_with(&dir);
    with_loaded_table(&mut world);
    let v = call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    assert_eq!(v["tab"], "data");
    let data = &v["data"];
    assert_eq!(data["schema"], "loaded");
    assert_eq!(data["row_type"], "Weapon");
    let columns: Vec<&str> = data["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(columns, ["damage", "kind", "label", "tier", "colour"]);
    assert_eq!(data["columns"][0]["type_path"], "u32");
    assert_eq!(data["columns"][0]["doc"], "Damage per hit.");
    assert_eq!(data["columns"][2]["text"], true);
    assert_eq!(
        data["columns"][4]["known"], false,
        "`colour` is no field of the row struct"
    );
    // The file's own schema error: badged on its cell and listed, not hidden.
    assert_eq!(cell(&v, "odd", "colour")["state"], "error");
    assert_eq!(cell(&v, "odd", "colour")["issue"], "table_unknown_column");
    assert_eq!(cell(&v, "pistol", "tier")["text"], "Some(2)");
    assert_eq!(
        cell(&v, "rifle", "label")["text"],
        "Rifle",
        "a string shows as its text"
    );
    assert!(
        has_color(&draw(&mut world), ERROR_FILL),
        "the error cell is drawn"
    );

    // A cell that does not parse as its field's type is refused and nothing changes.
    let generation = v["data"]["generation"].clone();
    let e = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "damage", "text": "lots" }),
    )
    .expect_err("u32");
    assert_eq!(reason(&e), "table_bad_cell");
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(cell(&v, "rifle", "damage")["ron"], "30");
    assert_eq!(v["data"]["generation"], generation);
    assert_eq!(
        v["error"]["code"], "table_bad_cell",
        "the panel shows the refusal"
    );
    // The game's row rule is a schema error too.
    let e = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "damage", "text": 900 }),
    )
    .expect_err("rule");
    assert_eq!(reason(&e), "table_rule");
    // A valid edit applies; a String field takes plain text, an enum field its variant.
    let v = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "damage", "text": 40 }),
    )
    .expect("damage");
    assert_eq!(cell(&v, "rifle", "damage")["ron"], "40");
    assert_eq!(v["data"]["dirty"], true);
    let v = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "label", "text": "123" }),
    )
    .expect("label");
    assert_eq!(
        cell(&v, "rifle", "label")["ron"],
        "\"123\"",
        "plain text in a String field"
    );
    let e = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "kind", "text": "Cannon" }),
    )
    .expect_err("no such variant");
    assert_eq!(reason(&e), "table_bad_cell");
    // Fixing the file's error: clearing the unknown cell removes the badge.
    let v = edit(
        &mut world,
        json!({ "op": "clear_cell", "key": "odd", "column": "colour" }),
    )
    .expect("clear");
    assert_eq!(v["data"]["errors"], 0, "{}", v["data"]["issues"]);
    assert!(!has_color(&draw(&mut world), ERROR_FILL), "no error cell left");

    // Undo walks back one edit at a time: the error returns with its cell.
    assert_eq!(v["data"]["undo_depth"], 3);
    let v = call(&mut world, "editor.tables_panel_undo", json!({ "tab": "data" })).expect("undo");
    assert_eq!(cell(&v, "odd", "colour")["issue"], "table_unknown_column");
    let v = call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    assert_eq!(cell(&v, "rifle", "label")["ron"], "\"Rifle\"");
    let v = edit(
        &mut world,
        json!({ "op": "clear_cell", "key": "odd", "column": "colour" }),
    )
    .expect("clear");
    assert_eq!(v["data"]["errors"], 0);

    let written = call(&mut world, "editor.tables_panel_save", json!({ "tab": "data" })).expect("save");
    assert_eq!(written["written"], PATH);
    let text = std::fs::read_to_string(dir.join("assets").join(PATH)).expect("saved");
    let saved = DataTable::<Weapon>::from_ron_str(&text).expect("the saved file loads typed");
    assert_eq!(saved.get("rifle").expect("rifle").damage, 40);
    assert_eq!(saved.get("pistol").expect("pistol").tier, Some(2));
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(v["data"]["dirty"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rows_insert_rename_move_and_remove_with_key_errors_refused() {
    let dir = project("tables-panel-data-rows");
    let mut world = world_with(&dir);
    with_loaded_table(&mut world);
    call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    // A new row needs its required fields: `damage` and friends have no default.
    let e = edit(&mut world, json!({ "op": "insert_row", "key": "smg" })).expect_err("missing fields");
    assert_eq!(reason(&e), "table_missing_column");
    let v = edit(
        &mut world,
        json!({ "op": "insert_row", "key": "smg", "before": "pistol",
                "cells": { "damage": 9, "kind": "Pistol", "label": "SMG" } }),
    )
    .expect("insert");
    assert_eq!(keys(&v), ["rifle", "smg", "pistol", "odd"]);
    assert_eq!(cell(&v, "smg", "label")["ron"], "\"SMG\"");
    for (params, code) in [
        (
            json!({ "op": "rename_row", "key": "smg", "to": "rifle" }),
            "table_duplicate_key",
        ),
        (
            json!({ "op": "rename_row", "key": "smg", "to": "  " }),
            "table_empty_key",
        ),
        (
            json!({ "op": "insert_row", "key": "pistol", "cells": { "damage": 1, "kind": "Rifle", "label": "x" } }),
            "table_duplicate_key",
        ),
        (
            json!({ "op": "remove_row", "key": "nope" }),
            "tables_panel_row_not_found",
        ),
        (json!({ "op": "explode", "key": "smg" }), "tables_panel_bad_op"),
        (
            json!({ "op": "set_cell", "key": "smg", "column": "damage" }),
            "tables_panel_bad_value",
        ),
    ] {
        let e = edit(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), code, "{params}");
    }
    let v = edit(
        &mut world,
        json!({ "op": "rename_row", "key": "smg", "to": "uzi" }),
    )
    .expect("rename");
    assert_eq!(keys(&v), ["rifle", "uzi", "pistol", "odd"]);
    let v = edit(&mut world, json!({ "op": "move_row", "key": "uzi" })).expect("to the end");
    assert_eq!(keys(&v), ["rifle", "pistol", "odd", "uzi"]);
    let v = edit(
        &mut world,
        json!({ "op": "move_row", "key": "odd", "before": "rifle" }),
    )
    .expect("to the top");
    assert_eq!(keys(&v), ["odd", "rifle", "pistol", "uzi"]);
    let v = edit(&mut world, json!({ "op": "remove_row", "key": "pistol" })).expect("remove");
    assert_eq!(keys(&v), ["odd", "rifle", "uzi"]);
    // Every step undoes back to the file's rows.
    for expected in [
        vec!["odd", "rifle", "pistol", "uzi"],
        vec!["rifle", "pistol", "odd", "uzi"],
        vec!["rifle", "uzi", "pistol", "odd"],
        vec!["rifle", "smg", "pistol", "odd"],
        vec!["rifle", "pistol", "odd"],
    ] {
        let v = call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
        assert_eq!(keys(&v), expected);
    }
    let e = call(&mut world, "editor.tables_panel_undo", json!({})).expect_err("empty");
    assert_eq!(reason(&e), "tables_panel_nothing_to_undo");
    assert!(!draw(&mut world).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without the loaded table the type registry still gives the columns; without either the checks are
/// structural.
#[test]
fn the_schema_falls_back_to_reflection_then_to_structure() {
    let dir = project("tables-panel-data-schema");
    let mut world = world_with(&dir);
    let registry = AppTypeRegistry::default();
    registry.write().register::<Weapon>();
    world.insert_resource(registry);
    let v = call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    assert_eq!(v["data"]["schema"], "reflected");
    assert_eq!(cell(&v, "odd", "colour")["issue"], "table_unknown_column");
    assert_eq!(v["data"]["columns"][0]["type_path"], "u32");
    let e = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "colour", "text": "blue" }),
    )
    .expect_err("no such field");
    assert_eq!(reason(&e), "table_unknown_column");

    let mut world = world_with(&dir);
    let v = call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    assert_eq!(v["data"]["schema"], "structural");
    assert_eq!(v["data"]["errors"], 0);
    // Structural checks still hold: a cell must be one RON value, and a text column stays text.
    let e = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "damage", "ron": "(1, 2" }),
    )
    .expect_err("not one value");
    assert_eq!(reason(&e), "table_bad_cell");
    let v = edit(
        &mut world,
        json!({ "op": "set_cell", "key": "rifle", "column": "label", "text": "7" }),
    )
    .expect("text");
    assert_eq!(cell(&v, "rifle", "label")["ron"], "\"7\"");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_csv_table_keeps_its_text_cells_and_saves_as_csv() {
    let dir = project("tables-panel-data-csv");
    let mut world = world_with(&dir);
    let mut tables = DataTables::default();
    tables.insert("tables/notes.table.csv", DataTable::<Weapon>::new());
    world.insert_resource(tables);
    let v = call(
        &mut world,
        "editor.tables_panel_open",
        json!({ "path": "tables/notes.table.csv" }),
    )
    .expect("open");
    assert_eq!(v["data"]["format"], "csv");
    assert_eq!(v["data"]["errors"], 0, "{}", v["data"]["issues"]);
    assert_eq!(
        cell(&v, "a", "label")["ron"],
        "\"42\"",
        "the String field reads 42 as text"
    );
    edit(
        &mut world,
        json!({ "op": "set_cell", "key": "b", "column": "label", "text": "with, comma" }),
    )
    .expect("edit");
    call(&mut world, "editor.tables_panel_save", json!({})).expect("save");
    let text = std::fs::read_to_string(dir.join("assets/tables/notes.table.csv")).expect("saved");
    assert!(text.starts_with("key,damage,kind,label\r\n"), "{text}");
    assert!(text.contains("b,2,Pistol,\"with, comma\"\r\n"), "{text}");
    let saved = DataTable::<Weapon>::from_csv_str(&text).expect("typed");
    assert_eq!(saved.get("a").expect("a").label, "42");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 4,096-row table: the undo stack holds edits, not copies, and stops at 32 steps.
#[test]
fn undo_on_a_large_table_keeps_inverse_edits_up_to_the_depth() {
    let dir = regia_test_utils::unique_temp_dir("tables-panel-data-large");
    let mut text = String::from("(format_version: 1, row_type: \"Weapon\", rows: {\n");
    for i in 0..4096 {
        text.push_str(&format!(
            "\"w{i}\": (damage: {i}, kind: Rifle, label: \"W{i}\"),\n"
        ));
    }
    text.push_str("})\n");
    std::fs::create_dir_all(dir.join("assets/tables")).expect("dir");
    std::fs::write(dir.join("assets").join(PATH), &text).expect("write");
    let mut world = world_with(&dir);
    with_loaded_table(&mut world);
    call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    for i in 0..40u32 {
        edit(
            &mut world,
            json!({ "op": "set_cell", "key": "w7", "column": "damage", "text": i }),
        )
        .expect("edit");
    }
    let panel = world.resource::<TablesPanel>();
    let doc = panel.data.as_ref().expect("open");
    assert_eq!(doc.undo_depth(), UNDO_DEPTH);
    assert_eq!(doc.raw.rows.len(), 4096);
    for _ in 0..UNDO_DEPTH {
        call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    }
    let v = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    // 40 edits, 32 undone: the value of the eighth edit.
    assert_eq!(cell(&v, "w7", "damage")["ron"], "7");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Export a data table as CSV, JSON and RON (the bytes `regia table to-csv`, `to-json` and `to-ron` write), edit
/// the file, and import it back: one undo step, the row type and text columns kept; an import that adds a
/// schema error is refused and changes nothing.
#[test]
fn data_tables_export_and_import_csv_json_and_ron_with_undo() {
    use regia_asset::data_table::{CsvImport, RawTable};
    let dir = project("tables-panel-data-exchange");
    let mut world = world_with(&dir);
    with_loaded_table(&mut world);
    call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    // `odd` names a column the row struct lacks, and an empty CSV cell of a text column reads as `""`; drop
    // it so the table round-trips clean.
    edit(&mut world, json!({ "op": "remove_row", "key": "odd" })).expect("remove odd");
    let mut raw = RawTable::from_ron(&std::fs::read_to_string(dir.join("assets").join(PATH)).expect("file"))
        .expect("raw");
    raw.rows.retain(|r| r.key != "odd");

    for (name, expected) in [
        ("csv", raw.to_csv()),
        (
            "json",
            regia_asset_pipeline::table_json::to_json(&raw).expect("json"),
        ),
        ("ron", raw.to_ron()),
    ] {
        let out = call(
            &mut world,
            "editor.tables_panel_export",
            json!({ "path": format!("out/weapons.{name}") }),
        )
        .expect("export");
        assert_eq!(out["format"], name);
        assert_eq!(out["rows"], 2);
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("assets/out/weapons.{name}"))).expect("file"),
            expected,
            "{name}: the library's bytes"
        );
    }
    let exported = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(exported["data"]["undo_depth"], 1, "exporting changes nothing");

    // A spreadsheet edit: a new row, with a label that reads like a plain word.
    let csv = std::fs::read_to_string(dir.join("assets/out/weapons.csv")).expect("csv");
    std::fs::write(
        dir.join("assets/out/edited.csv"),
        format!("{csv}smg,9,Pistol,smg,,\r\n"),
    )
    .expect("edited");
    let before = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    let done = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/edited.csv" }),
    )
    .expect("import");
    assert_eq!(done["rows"], 3);
    assert_eq!(done["undo_depth"], 2, "the remove and the import");
    let after = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(keys(&after), ["rifle", "pistol", "smg"]);
    assert_eq!(
        after["data"]["row_type"], "Weapon",
        "the open table's row type is kept"
    );
    assert_eq!(
        cell(&after, "smg", "label")["ron"],
        "\"smg\"",
        "a text column stays a string"
    );
    assert_eq!(after["data"]["dirty"], true);
    call(&mut world, "editor.tables_panel_undo", json!({})).expect("undo");
    let undone = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(
        undone["data"]["rows"], before["data"]["rows"],
        "one undo step restores the rows"
    );

    // JSON in, with a number as a number and an unset column left out.
    std::fs::write(
        dir.join("assets/out/in.json"),
        r#"[{"key": "rifle", "damage": 31, "kind": "Rifle", "label": "Rifle"}, {"key": "bow", "damage": 4, "kind": "Pistol", "label": "Bow", "tier": "Some(1)"}]"#,
    )
    .expect("json in");
    call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/in.json" }),
    )
    .expect("json import");
    let json_in = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(keys(&json_in), ["rifle", "bow"]);
    assert_eq!(cell(&json_in, "rifle", "damage")["ron"], "31");
    assert_eq!(
        cell(&json_in, "bow", "tier")["ron"],
        "Some(1)",
        "a RON value goes in as a string"
    );
    // The same JSON through the library reads the same table as `regia table from-json`.
    let cli = regia_asset_pipeline::table_json::from_json(
        &std::fs::read_to_string(dir.join("assets/out/in.json")).expect("json"),
        &CsvImport::default(),
    )
    .expect("library");
    assert_eq!(cli.rows.len(), 2);

    // A refused import: the loaded row struct rejects damage "lots", so nothing changes.
    std::fs::write(
        dir.join("assets/out/bad.csv"),
        "key,damage,kind,label\r\nrifle,lots,Rifle,Rifle\r\n",
    )
    .expect("bad csv");
    let depth = json_in["data"]["undo_depth"].clone();
    let e = call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/bad.csv" }),
    )
    .expect_err("refused");
    assert!(!reason(&e).is_empty(), "{e:?}");
    let kept = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(keys(&kept), ["rifle", "bow"], "a refused import changes nothing");
    assert_eq!(kept["data"]["undo_depth"], depth);

    // RON in: a canonical `.table.ron` replaces the rows (the open table keeps its row type).
    std::fs::write(
        dir.join("assets/out/one.ron"),
        r#"(format_version: 1, row_type: "Other", rows: { "carbine": (damage: 5, kind: Rifle, label: "Carbine") })"#,
    )
    .expect("ron");
    call(
        &mut world,
        "editor.tables_panel_import",
        json!({ "path": "out/one.ron" }),
    )
    .expect("ron in");
    let ron_in = call(&mut world, "editor.tables_panel_get", json!({})).expect("get");
    assert_eq!(keys(&ron_in), ["carbine"]);
    assert_eq!(ron_in["data"]["row_type"], "Weapon");
}

/// The Exchange row's buttons are focusable and a key press runs them: Export… with the keyboard writes the
/// file, Import… reads it back as an undo step.
#[test]
fn the_exchange_row_is_operable_from_the_keyboard() {
    let dir = project("tables-panel-exchange-keys");
    let mut world = world_with(&dir);
    with_loaded_table(&mut world);
    call(&mut world, "editor.tables_panel_open", json!({ "path": PATH })).expect("open");
    // Drop the row with the column the row struct lacks, so the file round-trips clean.
    edit(&mut world, json!({ "op": "remove_row", "key": "odd" })).expect("remove odd");
    world.resource_mut::<TablesPanel>().exchange_path = "out/keys.json".to_owned();
    world.resource_mut::<TablesPanel>().exchange_format = super::super::exchange::ExchangeFormat::Json;
    let ctx = egui::Context::default();
    let mut frame = |world: &mut World, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| tables_panel(ui, world));
        output.textures_delta.clear();
    };
    let press = |world: &mut World, frame: &mut dyn FnMut(&mut World, Vec<egui::Event>), name: &str| {
        let id = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(format!("tables-exchange-test-{name}"))))
            .expect("button id");
        ctx.memory_mut(|m| m.request_focus(id));
        frame(world, Vec::new());
        for pressed in [true, false] {
            frame(
                world,
                vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        frame(world, Vec::new());
        frame(world, Vec::new());
    };
    frame(&mut world, Vec::new());
    frame(&mut world, Vec::new());
    press(&mut world, &mut frame, "export");
    let panel = world.resource::<TablesPanel>();
    assert!(panel.error.is_none(), "{:?}", panel.error);
    let file = dir.join("assets/out/keys.json");
    assert!(file.is_file(), "the keyboard ran Export…");
    let text = std::fs::read_to_string(&file).expect("json");
    assert!(text.contains("\"rifle\""), "{text}");
    // Edit the file, then Import… with the keyboard.
    std::fs::write(&file, text.replace("\"rifle\"", "\"carbine\"")).expect("edit");
    press(&mut world, &mut frame, "import");
    let panel = world.resource::<TablesPanel>();
    assert!(panel.error.is_none(), "{:?}", panel.error);
    let data = panel.data.as_ref().expect("open");
    assert!(
        data.raw.rows.iter().any(|r| r.key == "carbine"),
        "the keyboard ran Import…"
    );
    assert_eq!(data.undo_depth(), 2, "the remove and the import");
}
