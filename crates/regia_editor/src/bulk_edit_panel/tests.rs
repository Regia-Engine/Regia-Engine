// SPDX-License-Identifier: MPL-2.0
//! Bulk Edit grid tests: TSV, keys, the keyboard path through a headless frame, the GUI paste
//! against the agent paste, 10 000 virtualized rows, accessible names and asset rows.

use super::*;
use crate::canvas_keys::test_input::{focused_name, frame, key, raw, text};
use egui::{Key, Modifiers};
use regia_app::prelude::{Reflect, ReflectDefault};

/// A component with scalar fields and a nested struct.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Unit {
    hp: f32,
    speed: f32,
    armor: i32,
    title: String,
    aim: Aim,
}

#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Aim {
    yaw: f32,
    pitch: f32,
}

/// A data asset type (registered with `ReflectAsset`).
#[derive(Reflect, Default, Clone, PartialEq, Debug)]
#[reflect(Default)]
struct Loot {
    rolls: u32,
    label: String,
}
impl regia_asset::data_asset::Asset for Loot {}

const SIZE: [f32; 2] = [1400.0, 900.0];

fn world_with(n: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    let registry = AppTypeRegistry::default();
    {
        let mut r = registry.write();
        r.register::<Unit>();
        r.register::<Loot>();
        r.register_type_data::<Loot, regia_asset::data_asset::ReflectAsset>();
        r.register::<u32>();
        r.register::<f32>();
        r.register::<i32>();
        r.register::<String>();
    }
    world.insert_resource(registry);
    world.insert_resource(UndoStack::new());
    let rows = (0..n)
        .map(|i| {
            world
                .spawn(Unit {
                    hp: i as f32,
                    speed: 1.5,
                    armor: 0,
                    title: format!("u{i}"),
                    aim: Aim::default(),
                })
                .id()
        })
        .collect();
    (world, rows)
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let (_, handler) = methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method}"));
    handler(world, &params)
}

fn open(world: &mut World, rows: &[Entity]) {
    let bits: Vec<u64> = rows.iter().map(|e| e.to_bits()).collect();
    call(
        world,
        "editor.bulk_edit_open",
        json!({ "entities": bits, "component": "Unit", "columns": ["hp", "speed", "armor", "title"] }),
    )
    .expect("open");
}

fn bytes(world: &World, rows: &[Entity]) -> Vec<u8> {
    let mut out = Vec::new();
    for entity in rows {
        let unit = world.get::<Unit>(*entity).expect("unit");
        out.extend(unit.hp.to_le_bytes());
        out.extend(unit.speed.to_le_bytes());
        out.extend(unit.armor.to_le_bytes());
        out.extend(unit.title.as_bytes());
        out.push(0);
        out.extend(unit.aim.yaw.to_le_bytes());
        out.extend(unit.aim.pitch.to_le_bytes());
    }
    out
}

fn depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

fn grid(world: &World) -> &Grid {
    world.resource::<BulkEdit>().grid.as_ref().expect("grid open")
}

/// One frame of the panel.
fn draw(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
    frame(ctx, SIZE, events, |ui| {
        egui::CentralPanel::default().show(ui, |ui| bulk_edit_panel(ui, world));
    })
}

/// Presses Tab until the grid's focus target has the keys (no pointer).
fn tab_to_grid(ctx: &egui::Context, world: &mut World) {
    draw(ctx, world, vec![]);
    draw(ctx, world, vec![]);
    let mut seen = Vec::new();
    for _ in 0..40 {
        draw(ctx, world, vec![key(Key::Tab, Modifiers::NONE)]);
        let update = draw(ctx, world, vec![]);
        let name = focused_name(&update).unwrap_or_default();
        if name.starts_with("Bulk Edit grid") {
            return;
        }
        seen.push(name);
    }
    panic!("Tab never reached the grid: {seen:?}");
}

#[test]
fn tsv_round_trips_quoted_cells_and_crlf() {
    let rows = vec![
        vec!["1".to_owned(), "a\tb".to_owned(), "say \"hi\"".to_owned()],
        vec!["two\nlines".to_owned(), String::new(), "x".to_owned()],
    ];
    let text = write_tsv(&rows);
    assert_eq!(text, "1\t\"a\tb\"\t\"say \"\"hi\"\"\"\n\"two\nlines\"\t\tx\n");
    assert_eq!(parse_tsv(&text), rows);
    assert_eq!(
        parse_tsv("1\t2\r\n3\t4\r\n"),
        vec![vec!["1", "2"], vec!["3", "4"]]
    );
    assert_eq!(parse_tsv("1\t2"), vec![vec!["1", "2"]], "no trailing line break");
    assert!(parse_tsv("").is_empty());
}

#[test]
fn keys_map_to_grid_actions_and_the_cursor_wraps() {
    assert_eq!(
        grid_key(Key::ArrowDown, Modifiers::SHIFT),
        Some(GridKey::Move {
            dr: 1,
            dc: 0,
            extend: true
        })
    );
    assert_eq!(grid_key(Key::Tab, Modifiers::SHIFT), Some(GridKey::Previous));
    assert_eq!(grid_key(Key::Enter, Modifiers::NONE), Some(GridKey::Edit));
    assert_eq!(grid_key(Key::F2, Modifiers::NONE), Some(GridKey::Edit));
    assert_eq!(grid_key(Key::D, Modifiers::COMMAND), Some(GridKey::FillDown));
    assert_eq!(
        grid_key(Key::ArrowRight, Modifiers::ALT),
        Some(GridKey::Width(WIDTH_STEP))
    );
    assert_eq!(
        grid_key(Key::C, Modifiers::COMMAND),
        None,
        "copy arrives as Event::Copy"
    );
    let (mut world, rows) = world_with(2);
    open(&mut world, &rows);
    let mut panel = world.remove_resource::<BulkEdit>().expect("panel");
    let grid = panel.grid.as_mut().expect("grid");
    move_cursor(grid, GridKey::Previous, 2, 4);
    assert_eq!(grid.cursor, (0, 0), "stays at the first cell");
    for _ in 0..4 {
        move_cursor(grid, GridKey::Next, 2, 4);
    }
    assert_eq!(grid.cursor, (1, 0), "Tab wraps to the next row");
    move_cursor(
        grid,
        GridKey::Move {
            dr: 0,
            dc: 2,
            extend: true,
        },
        2,
        4,
    );
    assert_eq!(
        grid.range(),
        CellRange {
            row: 1,
            column: 0,
            rows: 1,
            columns: 3
        }
    );
    move_cursor(
        grid,
        GridKey::Move {
            dr: 5,
            dc: 9,
            extend: false,
        },
        2,
        4,
    );
    assert_eq!(
        (grid.cursor, grid.anchor),
        ((1, 3), None),
        "clamped, range dropped"
    );
}

#[test]
fn nested_structs_flatten_into_dotted_columns() {
    let (mut world, rows) = world_with(1);
    let bits = rows[0].to_bits();
    let state = call(
        &mut world,
        "editor.bulk_edit_open",
        json!({ "entities": [bits], "component": "Unit" }),
    )
    .expect("open");
    assert_eq!(
        state["available"],
        json!(["hp", "speed", "armor", "title", "aim.yaw", "aim.pitch"])
    );
    let quat = regia_core::math::Quat::IDENTITY;
    assert_eq!(leaf_paths(&quat), vec![String::new()], "a quaternion is one cell");
}

/// Acceptance: the GUI's Ctrl+V of a 3x4 block (a paste event while the grid has the keys) writes
/// the same bytes as `editor.bulk_edit_set { tsv }`, in one undo step.
#[test]
fn a_gui_paste_writes_the_same_bytes_as_the_agent_paste() {
    let tsv = "1\t2\t3\n4\t5\t6\n7\t8\t9\n10\t11\t12\n";
    let (mut gui, gui_rows) = world_with(4);
    open(&mut gui, &gui_rows);
    let ctx = egui::Context::default();
    tab_to_grid(&ctx, &mut gui);
    let before = depth(&gui);
    draw(&ctx, &mut gui, vec![egui::Event::Paste(tsv.to_owned())]);
    draw(&ctx, &mut gui, vec![]);
    assert_eq!(
        depth(&gui),
        before + 1,
        "one undo step: {}",
        gui.resource::<BulkEdit>().status
    );

    let (mut agent, agent_rows) = world_with(4);
    open(&mut agent, &agent_rows);
    let answer = call(
        &mut agent,
        "editor.bulk_edit_set",
        json!({ "tsv": tsv, "row": 0, "column": 0 }),
    )
    .expect("paste");
    assert_eq!(answer["set"], json!(12));
    assert_eq!(bytes(&gui, &gui_rows), bytes(&agent, &agent_rows));
    assert_eq!(gui.get::<Unit>(gui_rows[3]).expect("unit").armor, 12);
}

/// The keyboard path (no pointer): Tab to the grid, arrows move the cursor, Shift extends the
/// range, Ctrl+C copies it, Enter edits the cell as text and commits, Tab moves on, Escape
/// leaves the grid.
#[test]
fn the_grid_is_operable_from_the_keyboard() {
    let (mut world, rows) = world_with(3);
    open(&mut world, &rows);
    let ctx = egui::Context::default();
    tab_to_grid(&ctx, &mut world);
    draw(&ctx, &mut world, vec![key(Key::ArrowRight, Modifiers::NONE)]);
    draw(&ctx, &mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    assert_eq!(grid(&world).cursor, (1, 1), "one step per press");
    draw(&ctx, &mut world, vec![key(Key::ArrowDown, Modifiers::SHIFT)]);
    assert_eq!(
        grid(&world).range(),
        CellRange {
            row: 1,
            column: 1,
            rows: 2,
            columns: 1
        }
    );
    let mut out = ctx.run_ui(raw(SIZE, vec![egui::Event::Copy]), |ui| {
        egui::CentralPanel::default().show(ui, |ui| bulk_edit_panel(ui, &mut world));
    });
    out.textures_delta.clear();
    let copied: Vec<&String> = out
        .platform_output
        .commands
        .iter()
        .filter_map(|c| match c {
            egui::OutputCommand::CopyText(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(copied, [&"1.5\n1.5\n".to_owned()]);
    // Enter edits the cursor cell (speed of row 0) as text; select all, type, Enter commits.
    draw(&ctx, &mut world, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    draw(&ctx, &mut world, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    assert_eq!(grid(&world).cursor, (0, 1));
    let before = depth(&world);
    draw(&ctx, &mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    draw(&ctx, &mut world, vec![]);
    draw(&ctx, &mut world, vec![key(Key::A, Modifiers::COMMAND)]);
    draw(&ctx, &mut world, vec![text("42")]);
    draw(&ctx, &mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    draw(&ctx, &mut world, vec![]);
    assert_eq!(
        world.get::<Unit>(rows[0]).expect("unit").speed,
        42.0,
        "{}",
        world.resource::<BulkEdit>().status
    );
    assert_eq!(depth(&world), before + 1);
    assert_eq!(grid(&world).cursor, (1, 1), "Enter commits and moves down");
    let update = draw(&ctx, &mut world, vec![]);
    assert!(
        focused_name(&update)
            .unwrap_or_default()
            .starts_with("Bulk Edit grid"),
        "the grid has the keys again"
    );
    draw(&ctx, &mut world, vec![key(Key::Tab, Modifiers::NONE)]);
    assert_eq!(grid(&world).cursor, (1, 2), "Tab moves to the next cell");
    // Ctrl+D fills down: range rows 1..3 of column 2 (armor) from row 1.
    draw(&ctx, &mut world, vec![key(Key::ArrowDown, Modifiers::SHIFT)]);
    draw(&ctx, &mut world, vec![key(Key::D, Modifiers::COMMAND)]);
    assert_eq!(
        world.resource::<BulkEdit>().status,
        "Filled 0 cells",
        "armor is 0 everywhere"
    );
    draw(&ctx, &mut world, vec![key(Key::ArrowRight, Modifiers::ALT)]);
    assert_eq!(grid(&world).columns[2].width, DEFAULT_WIDTH + WIDTH_STEP);
    draw(&ctx, &mut world, vec![key(Key::Escape, Modifiers::NONE)]);
    let update = draw(&ctx, &mut world, vec![]);
    assert!(
        !focused_name(&update)
            .unwrap_or_default()
            .starts_with("Bulk Edit grid"),
        "Escape leaves the grid"
    );
}

/// 10 000 rows: the panel lays out only the rows in view.
#[test]
fn ten_thousand_rows_draw_only_the_visible_rows() {
    let (mut world, rows) = world_with(10_000);
    open(&mut world, &rows);
    let ctx = egui::Context::default();
    draw(&ctx, &mut world, vec![]);
    draw(&ctx, &mut world, vec![]);
    let drawn = world.resource::<BulkEdit>().drawn_rows;
    assert!(drawn > 0 && drawn < 100, "laid out {drawn} of 10000 rows");
    let state = call(&mut world, "editor.bulk_edit_state", json!({ "limit": 2 })).expect("state");
    assert_eq!(state["rows_total"], json!(10_000));
    assert_eq!(state["drawn_rows"], json!(drawn));
    assert_eq!(state["rows"].as_array().map(Vec::len), Some(2));
}

/// Every interactive widget of the panel has an accessible name.
#[test]
fn every_control_is_named() {
    let (mut world, rows) = world_with(3);
    open(&mut world, &rows);
    let ctx = egui::Context::default();
    draw(&ctx, &mut world, vec![]);
    let update = draw(&ctx, &mut world, vec![]);
    assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
}

/// Asset rows: the folder's data assets of one type; a paste is one undo step on the documents,
/// Save writes the files, undo restores the values.
#[test]
fn data_asset_rows_paste_undo_and_save() {
    let root = regia_test_utils::unique_temp_dir("bulk_edit_assets");
    let folder = root.join("assets/loot");
    std::fs::create_dir_all(&folder).expect("dir");
    let kind = std::any::type_name::<Loot>();
    for (name, rolls) in [("a", 1), ("b", 2)] {
        let text = format!(
            "DataAsset(format_version: 1, type: \"{kind}\", value: (rolls: {rolls}, label: \"{name}\"))\n"
        );
        std::fs::write(folder.join(format!("{name}.data.ron")), text).expect("write");
    }
    std::fs::write(folder.join("notes.txt"), "not an asset").expect("write");
    let (mut world, _) = world_with(0);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let status = edit_assets_in_grid(&mut world, &root, "assets/loot/a.data.ron");
    assert_eq!(status, "Opened 2 assets of Loot");
    assert_eq!(grid(&world).available, ["rolls", "label"]);
    let before = depth(&world);
    let answer = call(
        &mut world,
        "editor.bulk_edit_set",
        json!({ "tsv": "7\tseven\n8\teight\n" }),
    )
    .expect("paste");
    assert_eq!(answer["changed"], json!(4));
    assert_eq!(depth(&world), before + 1);
    let state = call(&mut world, "editor.bulk_edit_state", json!({})).expect("state");
    assert_eq!(
        state["dirty"],
        json!(["assets/loot/a.data.ron", "assets/loot/b.data.ron"])
    );
    assert_eq!(
        reason(call(
            &mut world,
            "editor.bulk_edit_view",
            json!({ "close": true })
        )),
        "bulk_edit_unsaved"
    );
    // The panel's Close refuses the same grid (review sprint-w12-rev-g4 #1).
    {
        let registry = registry_of(&world).expect("registry");
        let message = unsaved_message(grid(&world), &registry.read());
        assert!(
            message.as_deref().is_some_and(|m| m.contains("unsaved edits")),
            "{message:?}"
        );
    }
    assert!(crate::command::undo_world(&mut world));
    let state = call(&mut world, "editor.bulk_edit_state", json!({})).expect("state");
    assert_eq!(
        state["rows"][1]["cells"],
        json!(["2", "b"]),
        "undo restores the values"
    );
    {
        let registry = registry_of(&world).expect("registry");
        assert_eq!(
            unsaved_message(grid(&world), &registry.read()),
            None,
            "undone: nothing unsaved"
        );
    }
    assert!(crate::command::redo_world(&mut world));
    let saved = call(&mut world, "editor.bulk_edit_save", json!({})).expect("save");
    assert_eq!(
        saved["saved"],
        json!(["assets/loot/a.data.ron", "assets/loot/b.data.ron"])
    );
    let text = std::fs::read_to_string(folder.join("b.data.ron")).expect("read");
    assert!(text.contains("rolls: 8") && text.contains("\"eight\""), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

fn reason(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(e) => e
            .data
            .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default(),
    }
}

/// Writes `assets/loot/<name>.data.ron` of type `Loot`.
fn loot(root: &Path, name: &str, rolls: u32) {
    let folder = root.join("assets/loot");
    std::fs::create_dir_all(&folder).expect("dir");
    let kind = std::any::type_name::<Loot>();
    let text = format!(
        "DataAsset(format_version: 1, type: \"{kind}\", value: (rolls: {rolls}, label: \"{name}\"))\n"
    );
    std::fs::write(folder.join(format!("{name}.data.ron")), text).expect("write");
}

/// P-145: Edit selection in grid opens the Bulk Edit grid with the selected data assets as rows
/// (not the folder's other assets of the type); files that are not data assets are skipped and
/// listed; no data asset selected is refused.
#[test]
fn bulk_edit_in_grid_takes_the_selection() {
    let root = regia_test_utils::unique_temp_dir("bulk_edit_selection");
    for (name, rolls) in [("a", 1), ("b", 2), ("c", 3)] {
        loot(&root, name, rolls);
    }
    std::fs::write(root.join("assets/loot/notes.txt"), "not an asset").expect("write");
    let (mut world, _) = world_with(0);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    world
        .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
        .set([
            "assets/loot/c.data.ron".to_string(),
            "assets/loot/notes.txt".to_string(),
            "assets/loot/a.data.ron".to_string(),
        ]);
    // The agent's route: no `assets` takes the Content Browser selection.
    let out = call(&mut world, "editor.bulk_edit", json!({})).expect("bulk edit");
    assert_eq!(
        grid(&world).rows,
        [
            RowKey::Asset("assets/loot/c.data.ron".into()),
            RowKey::Asset("assets/loot/a.data.ron".into()),
        ],
        "the selection's data assets, b (a folder sibling) is not a row"
    );
    assert_eq!(out["skipped"], json!(["assets/loot/notes.txt"]), "{out}");
    assert_eq!(out["rows_total"], json!(2), "{out}");
    // The GUI's function with explicit paths gives the same grid.
    let opened = edit_selection_in_grid(
        &mut world,
        &root,
        &[
            "assets/loot/b.data.ron".to_string(),
            "assets/loot/a.data.ron".to_string(),
        ],
    )
    .expect("open");
    assert_eq!(opened.rows, ["assets/loot/b.data.ron", "assets/loot/a.data.ron"]);
    assert!(opened.skipped.is_empty());
    // Nothing editable selected.
    assert_eq!(
        reason(call(
            &mut world,
            "editor.bulk_edit",
            json!({ "assets": ["assets/loot/notes.txt"] })
        )),
        "bulk_edit_no_rows"
    );
    let _ = std::fs::remove_dir_all(&root);
}
