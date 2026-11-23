// SPDX-License-Identifier: MPL-2.0
//! Utility panels end to end (ADR-8189): the starter sample renames the selection in one undo
//! step, a call outside the safe subset is refused with its code, the logic graph driver does the
//! same work as the Lua one, and the keyboard alone reaches the panel's button.

use super::*;
use crate::command::{UndoStack, undo_world};
use crate::selection::{SelectMode, Selection};
use regia_app::prelude::App;
use regia_scene::prelude::{EntityGuid, Transform};

fn app(project: Option<&Path>) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    crate::agent_ext::install(world);
    world.init_resource::<regia_app::prelude::Time>();
    world.init_resource::<UndoStack>();
    world.init_resource::<Selection>();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: project.map(Path::to_path_buf),
    });
    app
}

fn spawn(world: &mut World, name: &str) -> Entity {
    world
        .spawn((
            Name::new(name.to_string()),
            Transform::default(),
            EntityGuid::new_random(),
        ))
        .id()
}

fn select(world: &mut World, entities: &[Entity]) {
    let mut selection = world.resource_mut::<Selection>();
    selection.clear();
    for entity in entities {
        selection.select(*entity, SelectMode::Add);
    }
}

fn name_of(world: &World, entity: Entity) -> String {
    world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_default()
}

fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
    utility_panels(world, &params)
}

fn ok(world: &mut World, params: Value) -> Value {
    call(world, params.clone()).unwrap_or_else(|e| panic!("{params}: {} ({})", e.message, reason(&e)))
}

/// A project folder with one utility panel at `assets/tools/<stem>.*`: the starter document and
/// the given driver.
fn project_with(stem: &str, driver: &str, driver_file: &str, driver_text: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("utility_panel");
    let dir = root.join("assets/tools");
    std::fs::create_dir_all(&dir).expect("dir");
    let sidecar = format!(
        "(format_version: 1, title: \"{stem}\", layout: \"{stem}.ui.ron\", driver: {driver}(\"{driver_file}\"))"
    );
    std::fs::write(dir.join(format!("{stem}.editorpanel.ron")), sidecar).expect("sidecar");
    std::fs::write(dir.join(format!("{stem}.ui.ron")), STARTER_FILES[1].1).expect("layout");
    std::fs::write(dir.join(driver_file), driver_text).expect("driver");
    root
}

#[test]
fn utility_panel_starter_sample_renames_two_selected_entities_in_one_undo_step() {
    let mut app = app(None);
    let world = app.world_mut();
    let a = spawn(world, "Crate");
    let b = spawn(world, "Barrel");
    let other = spawn(world, "Lamp");
    select(world, &[a, b]);
    let listed = ok(world, json!({ "op": "list" }));
    assert_eq!(listed["panels"][0]["id"], STARTER_ID, "{listed}");
    assert_eq!(listed["panels"][0]["title"], "Rename selection with prefix");
    assert_eq!(listed["panels"][0]["driver"]["kind"], "lua");
    let opened = ok(world, json!({ "op": "open", "id": STARTER_ID }));
    assert_eq!(opened["title"], "Rename selection with prefix");
    // The tab is queued for the shell.
    assert!(
        world
            .resource::<crate::editor_layout::LayoutRequests>()
            .pending
            .iter()
            .any(|r| matches!(r, crate::editor_layout::LayoutRequest::Open(Panel::UtilityPanel(id, _)) if *id == STARTER_ID))
    );
    let before = world.resource::<UndoStack>().undo_depth();
    ok(
        world,
        json!({ "op": "input", "id": STARTER_ID, "input": { "action": "type", "node": "#prefix", "text": "Box_" } }),
    );
    let out = ok(
        world,
        json!({ "op": "input", "id": STARTER_ID, "input": { "action": "click", "node": "#rename" } }),
    );
    assert_eq!(out["ran"][0]["command"], "rename", "{out}");
    assert_eq!(out["ran"][0]["ok"], true, "{out}");
    assert_eq!(out["ran"][0]["calls"].as_array().map(Vec::len), Some(2), "{out}");
    assert_eq!(out["values"]["prefix"], "Box_", "{out}");
    assert_eq!(name_of(world, a), "Box_Crate");
    assert_eq!(name_of(world, b), "Box_Barrel");
    assert_eq!(name_of(world, other), "Lamp", "only the selection is renamed");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        before + 1,
        "one undo step"
    );
    assert!(undo_world(world));
    assert_eq!(name_of(world, a), "Crate");
    assert_eq!(name_of(world, b), "Barrel");
    let state = ok(world, json!({ "op": "state", "id": STARTER_ID }));
    assert_eq!(state["runs"], 1, "{state}");
}

#[test]
fn utility_panel_refuses_a_method_outside_the_safe_subset_with_its_code() {
    let script = r#"
regia.on("command", function(ctx)
    for _, item in ipairs(ctx.selection) do
        regia.emit("editor.call", { method = "editor.outliner_rename",
            params = { entity = item.entity, name = "Renamed" } })
    end
    regia.emit("editor.call", { method = "editor.tool_launch", params = { tool = "x" } })
end)
"#;
    let root = project_with("launcher", "Lua", "launcher.lua", script);
    let mut app = app(Some(&root));
    let world = app.world_mut();
    let a = spawn(world, "Crate");
    select(world, &[a]);
    let id = "assets/tools/launcher";
    let listed = ok(world, json!({ "op": "list" }));
    assert_eq!(listed["panels"][1]["id"], id, "{listed}");
    assert_eq!(listed["panels"][1]["source"], "project");
    ok(world, json!({ "op": "open", "id": id }));
    let before = world.resource::<UndoStack>().undo_depth();
    let out = ok(
        world,
        json!({ "op": "input", "id": id, "input": { "action": "click", "node": "#rename" } }),
    );
    assert_eq!(out["ran"][0]["ok"], false, "{out}");
    assert_eq!(out["ran"][0]["code"], "utility_panel_method_refused", "{out}");
    assert!(
        out["ran"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("editor.tool_launch") && m.contains("tools")),
        "{out}"
    );
    assert_eq!(out["status"]["code"], "utility_panel_method_refused");
    // Nothing ran: the allowed rename before the refused call did not either.
    assert_eq!(name_of(world, a), "Crate");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), before);
    // The subset itself.
    let check = |world: &World, method: &str, params: Value| {
        check_call(
            world,
            &PanelCall {
                method: method.into(),
                params,
            },
        )
    };
    assert!(check(world, "editor.get_selection", json!({})).is_ok());
    assert!(check(world, "editor.outliner_rename", json!({})).is_ok());
    assert!(check(world, "scene.spawn", json!({})).is_err(), "not editor.*");
    assert!(check(world, "editor.utility_panels", json!({})).is_err());
    assert!(check(world, "editor.console_run", json!({})).is_err());
    assert!(check(world, "editor.command", json!({ "run": "file.quit" })).is_err());
    assert!(check(world, "editor.command", json!({ "query": "x" })).is_err());
    assert!(check(world, "editor.no_such_method", json!({})).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn utility_panel_logic_graph_driver_renames_like_the_lua_one() {
    let graph = r#"(
    format_version: 1,
    rules: [(
        name: "rename",
        event: EntityEvent("rename"),
        condition: Compare(key: "selection_count", op: Gt, value: 0.0),
        actions: [(
            task: "editor::Call",
            set: {
                "method": Literal(String("editor.outliner_rename")),
                "for_each_selected": Literal(Bool(true)),
                "name": Literal(String("{prefix}{name}")),
            },
        )],
    )],
)"#;
    let root = project_with("rename_logic", "Logic", "rename_logic.logic.ron", graph);
    let mut app = app(Some(&root));
    let world = app.world_mut();
    let a = spawn(world, "Crate");
    let b = spawn(world, "Barrel");
    select(world, &[a, b]);
    let id = "assets/tools/rename_logic";
    let listed = ok(world, json!({ "op": "list" }));
    assert_eq!(listed["panels"][1]["driver"]["kind"], "logic", "{listed}");
    ok(world, json!({ "op": "open", "id": id }));
    let before = world.resource::<UndoStack>().undo_depth();
    ok(
        world,
        json!({ "op": "input", "id": id, "input": { "action": "type", "node": "#prefix", "text": "Lg_" } }),
    );
    let out = ok(
        world,
        json!({ "op": "input", "id": id, "input": { "action": "click", "node": "#rename" } }),
    );
    assert_eq!(out["ran"][0]["ok"], true, "{out}");
    assert_eq!(name_of(world, a), "Lg_Crate");
    assert_eq!(name_of(world, b), "Lg_Barrel");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), before + 1);
    // Errors the agent sees.
    let err = call(world, json!({ "op": "open", "id": "assets/tools/none" })).expect_err("refused");
    assert_eq!(reason(&err), "utility_panel_not_found");
    let err = call(world, json!({ "op": "state", "id": STARTER_ID })).expect_err("refused");
    assert_eq!(reason(&err), "utility_panel_not_open");
    let err = call(world, json!({ "op": "close" })).expect_err("refused");
    assert_eq!(reason(&err), "utility_panel_bad_params");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn utility_panel_refuses_a_bad_sidecar_and_lists_it_with_its_code() {
    let root = project_with("broken", "Lua", "broken.lua", "");
    std::fs::write(
        root.join("assets/tools/broken.editorpanel.ron"),
        "(format_version: 9, title: \"x\", layout: \"a.ui.ron\", driver: Lua(\"a.lua\"))",
    )
    .expect("write");
    let mut app = app(Some(&root));
    let world = app.world_mut();
    let listed = ok(world, json!({ "op": "list" }));
    assert_eq!(
        listed["panels"][1]["error"]["code"], "utility_panel_newer",
        "{listed}"
    );
    let err = call(world, json!({ "op": "open", "id": "assets/tools/broken" })).expect_err("refused");
    assert_eq!(reason(&err), "utility_panel_newer");
    let _ = std::fs::remove_dir_all(&root);
}

/// The keyboard path (the wave's accessibility bar): Tab reaches the panel's canvas, Tab moves
/// through the document's controls, typing fills the prefix and Enter presses the button.
#[test]
fn utility_panel_keyboard_reaches_the_panel_button() {
    let mut app = app(None);
    let world = app.world_mut();
    let a = spawn(world, "Crate");
    let b = spawn(world, "Barrel");
    select(world, &[a, b]);
    let ctx = egui::Context::default();
    let key = |key: egui::Key| {
        vec![
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    };
    let frame = |world: &mut World, events: Vec<egui::Event>| {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| utility_panel_tab(ui, world, STARTER_ID));
        output.textures_delta.clear();
    };
    // Whether the document's node has its runtime UI focus.
    let focused = |world: &mut World, node: &str| {
        with_panel(world, STARTER_ID, |_, panel| {
            panel
                .preview
                .inspect(node)
                .is_ok_and(|v| v["focus"]["focused"] == true)
        })
        .unwrap_or(false)
    };
    frame(world, Vec::new());
    // Tab moves editor focus onto the canvas and then through the document's controls (the Tab that
    // focuses the canvas may already reach the document): Tab until the prefix field has focus.
    let mut tabs = 0;
    while !focused(world, "#prefix") {
        assert!(tabs < 4, "Tab never reached the prefix field");
        frame(world, key(egui::Key::Tab));
        frame(world, Vec::new());
        tabs += 1;
    }
    frame(world, vec![egui::Event::Text("Kb_".into())]);
    frame(world, Vec::new());
    frame(world, key(egui::Key::Tab));
    frame(world, Vec::new());
    assert!(
        focused(world, "#rename"),
        "Tab moves from the field to the button"
    );
    frame(world, key(egui::Key::Enter));
    for _ in 0..3 {
        frame(world, Vec::new());
    }
    let state = ok(world, json!({ "op": "state", "id": STARTER_ID }));
    assert_eq!(state["values"]["prefix"], "Kb_", "{state}");
    assert_eq!(state["status"]["ok"], true, "{state}");
    assert_eq!(name_of(world, a), "Kb_Crate", "{state}");
    assert_eq!(name_of(world, b), "Kb_Barrel");
}
