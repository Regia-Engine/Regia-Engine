// SPDX-License-Identifier: MPL-2.0
//! The Bulk Edit grid through the agent protocol (parity row P-112, spec addendum § 7.13):
//! rows are entities, columns are a component's reflected fields, and a pasted TSV block is one
//! undo step through the shared stack.

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_editor::command::UndoStack;
use serde_json::{Value, json};

/// A component with a few scalar fields and a nested vector (flattened to `offset.x` and so on).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Stats {
    hp: f32,
    speed: f32,
    armor: i32,
    title: String,
    offset: Vec3,
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Stats>();
    app.init_resource::<UndoStack>();
    regia_editor::agent_ext::install(app.world_mut());
    app
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

fn spawn_rows(world: &mut World, n: usize) -> Vec<Entity> {
    (0..n)
        .map(|i| {
            world
                .spawn(Stats {
                    hp: 10.0 * i as f32,
                    speed: 1.5,
                    armor: i as i32,
                    title: format!("unit {i}"),
                    offset: Vec3::new(i as f32, 0.0, 0.0),
                })
                .id()
        })
        .collect()
}

/// The exact bytes of every row's component (field by field, little endian).
fn bytes(world: &World, rows: &[Entity]) -> Vec<u8> {
    let mut out = Vec::new();
    for entity in rows {
        let stats = world.get::<Stats>(*entity).expect("row has Stats");
        out.extend(stats.hp.to_le_bytes());
        out.extend(stats.speed.to_le_bytes());
        out.extend(stats.armor.to_le_bytes());
        out.extend(stats.title.as_bytes());
        out.push(0);
        for lane in stats.offset.to_array() {
            out.extend(lane.to_le_bytes());
        }
    }
    out
}

fn depth(world: &mut World) -> usize {
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth()
}

fn open(world: &mut World, rows: &[Entity]) -> Value {
    let bits: Vec<u64> = rows.iter().map(|e| e.to_bits()).collect();
    call(
        world,
        "editor.bulk_edit_open",
        json!({ "entities": bits, "component": "Stats", "columns": ["hp", "speed", "armor", "title"] }),
    )
    .expect("open")
}

/// Acceptance: a 3x4 TSV block (three columns, four rows) pasted at the top left of a grid of four
/// entities sets 12 fields in one undo step, and the undo restores the bytes.
#[test]
fn a_pasted_tsv_block_sets_twelve_fields_in_one_undo_step_and_undo_restores_the_bytes() {
    let mut app = app();
    let world = app.world_mut();
    let rows = spawn_rows(world, 4);
    let opened = open(world, &rows);
    assert_eq!(opened["rows_total"], json!(4));
    let before = bytes(world, &rows);
    let depth_before = depth(world);
    let tsv = "1\t2\t3\n4\t5\t6\n7\t8\t9\n10\t11\t12\n";
    let answer = call(
        world,
        "editor.bulk_edit_set",
        json!({ "tsv": tsv, "row": 0, "column": 0 }),
    )
    .expect("paste");
    assert_eq!(answer["set"], json!(12), "{answer}");
    assert_eq!(depth(world), depth_before + 1, "one undo step for the block");
    let expected = [(1.0, 2.0, 3), (4.0, 5.0, 6), (7.0, 8.0, 9), (10.0, 11.0, 12)];
    for (entity, (hp, speed, armor)) in rows.iter().zip(expected) {
        let stats = world.get::<Stats>(*entity).expect("stats");
        assert_eq!((stats.hp, stats.speed, stats.armor), (hp, speed, armor));
    }
    assert!(regia_editor::agent_ext::test_support::undo(world));
    assert_eq!(bytes(world, &rows), before, "undo restores every byte");
    assert_eq!(depth(world), depth_before);
}

/// The state lists flattened nested fields, copies a range as TSV that pastes back unchanged, and
/// a paste with one bad cell writes nothing.
#[test]
fn state_copies_a_range_as_tsv_and_a_bad_cell_refuses_the_whole_paste() {
    let mut app = app();
    let world = app.world_mut();
    let rows = spawn_rows(world, 3);
    let bits: Vec<u64> = rows.iter().map(|e| e.to_bits()).collect();
    let opened = call(
        world,
        "editor.bulk_edit_open",
        json!({ "entities": bits, "component": "Stats" }),
    )
    .expect("open");
    let available: Vec<&str> = opened["available"]
        .as_array()
        .expect("available")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for path in [
        "hp", "speed", "armor", "title", "offset.x", "offset.y", "offset.z",
    ] {
        assert!(available.contains(&path), "{path} in {available:?}");
    }
    let state = call(
        world,
        "editor.bulk_edit_state",
        json!({ "range": { "row": 0, "column": 0, "rows": 3, "columns": 4 } }),
    )
    .expect("state");
    let tsv = state["tsv"].as_str().expect("tsv").to_string();
    assert_eq!(
        tsv,
        "0.0\t1.5\t0\tunit 0\n10.0\t1.5\t1\tunit 1\n20.0\t1.5\t2\tunit 2\n"
    );
    let before = bytes(world, &rows);
    let depth_before = depth(world);
    let refused = call(
        world,
        "editor.bulk_edit_set",
        json!({ "tsv": "1\t2\n3\tnot a number\n", "row": 0, "column": 0 }),
    );
    assert_eq!(reason(refused), "bulk_edit_bad_value");
    assert_eq!(bytes(world, &rows), before, "nothing written");
    assert_eq!(depth(world), depth_before);
    // The copied block pastes back as no change.
    let same = call(
        world,
        "editor.bulk_edit_set",
        json!({ "tsv": tsv, "row": 0, "column": 0 }),
    )
    .expect("paste back");
    assert_eq!(same["changed"], json!(0));
    assert_eq!(bytes(world, &rows), before);
}

/// Sort, filter and pinning are view state; cells address rows in view order; fill down copies
/// the top row of a range into the rows below it in one undo step.
#[test]
fn view_order_and_fill_down() {
    let mut app = app();
    let world = app.world_mut();
    let rows = spawn_rows(world, 4);
    open(world, &rows);
    let view = call(
        world,
        "editor.bulk_edit_view",
        json!({ "sort": { "column": "hp", "descending": true }, "pinned": ["title"] }),
    )
    .expect("view");
    let titles: Vec<&str> = view["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["cells"][0].as_str().unwrap_or_default())
        .collect();
    assert_eq!(titles, ["unit 3", "unit 2", "unit 1", "unit 0"]);
    assert_eq!(
        view["columns"][0]["path"],
        json!("title"),
        "pinned columns come first"
    );
    let filtered = call(world, "editor.bulk_edit_view", json!({ "filter": "unit 2" })).expect("filter");
    assert_eq!(filtered["rows_shown"], json!(1));
    call(
        world,
        "editor.bulk_edit_view",
        json!({ "filter": "", "sort": null, "pinned": [] }),
    )
    .expect("reset");
    let depth_before = depth(world);
    let filled = call(
        world,
        "editor.bulk_edit_set",
        json!({ "fill_down": { "row": 0, "column": 1, "rows": 4, "columns": 1 } }),
    )
    .expect("fill down");
    assert_eq!(filled["set"], json!(3));
    assert_eq!(
        depth(world),
        depth_before,
        "speed is already 1.5 everywhere: no entry"
    );
    let filled = call(
        world,
        "editor.bulk_edit_set",
        json!({ "fill_down": { "row": 1, "column": 0, "rows": 3, "columns": 1 } }),
    )
    .expect("fill down");
    assert_eq!(filled["changed"], json!(2));
    assert_eq!(depth(world), depth_before + 1);
    for entity in &rows[1..] {
        assert_eq!(world.get::<Stats>(*entity).expect("stats").hp, 10.0);
    }
}

/// Error codes: nothing open, an unknown component, a column outside the grid.
#[test]
fn refusals_have_stable_reasons() {
    let mut app = app();
    let world = app.world_mut();
    assert_eq!(
        reason(call(world, "editor.bulk_edit_state", json!({}))),
        "bulk_edit_not_open"
    );
    let rows = spawn_rows(world, 2);
    let bits: Vec<u64> = rows.iter().map(|e| e.to_bits()).collect();
    assert_eq!(
        reason(call(
            world,
            "editor.bulk_edit_open",
            json!({ "entities": bits, "component": "NoSuchComponent" })
        )),
        "bulk_edit_unknown_component"
    );
    open(world, &rows);
    assert_eq!(
        reason(call(
            world,
            "editor.bulk_edit_set",
            json!({ "cells": [{ "row": 0, "column": "nope", "value": 1 }] })
        )),
        "bulk_edit_bad_column"
    );
    assert_eq!(
        reason(call(
            world,
            "editor.bulk_edit_set",
            json!({ "cells": [{ "row": 9, "column": "hp", "value": 1 }] })
        )),
        "bulk_edit_bad_row"
    );
    assert_eq!(
        reason(call(world, "editor.bulk_edit_save", json!({}))),
        "bulk_edit_not_assets"
    );
}
