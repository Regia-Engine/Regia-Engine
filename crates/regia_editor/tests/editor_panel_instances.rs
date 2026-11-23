// SPDX-License-Identifier: MPL-2.0
//! Panel instances (parity row P-003, ADR-8105 (a)) through the headless shell and the agent
//! methods: Window ▸ New Outliner / Content Browser / Inspector (`editor.panel_new_instance`, the
//! palette's `window.new_*`), each instance's own state, the state saved in the layout (version
//! 3) and given back after a restart, and closing an instance dropping its state.

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_editor::shell::EditorShell;
use serde_json::{Value, json};

fn unique_dir(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("panel-instances-{label}"))
}

fn temp_project(label: &str) -> PathBuf {
    let dir = unique_dir(label);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"P\"\nid = \"com.example.p\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("assets/levels/main.level.ron"),
        "( format_version: 1, entities: [] )",
    )
    .expect("level");
    dir
}

fn edit_app(config: &Path, root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.world_mut().insert_resource(EditorConfigPaths::at(config));
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    app.update();
    app
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

/// Frame numbers keep growing across calls (egui's time never goes back).
static FRAME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One editor frame (the root pass and the frame effects) with `events`.
fn frame(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 1000.0),
        )),
        time: Some(f64::from(n) / 60.0),
        predicted_dt: 1.0 / 60.0,
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
    shell.frame_effects(ctx, world);
    out.textures_delta.clear();
    out
}

fn frames(shell: &mut EditorShell, ctx: &egui::Context, world: &mut World, count: u32) -> egui::FullOutput {
    let mut last = frame(shell, ctx, world, Vec::new());
    for _ in 1..count {
        last = frame(shell, ctx, world, Vec::new());
    }
    last
}

/// Types `text` into the widget `id` (focused first, as a click or Tab would).
fn type_into(shell: &mut EditorShell, ctx: &egui::Context, world: &mut World, id: egui::Id, text: &str) {
    ctx.memory_mut(|m| m.request_focus(id));
    frames(shell, ctx, world, 1);
    frame(shell, ctx, world, vec![egui::Event::Text(text.to_owned())]);
    ctx.memory_mut(|m| m.surrender_focus(id));
    frames(shell, ctx, world, 2);
}

/// Whether `shapes` hold a text run containing `needle`.
fn has_text(shapes: &[egui::epaint::ClippedShape], needle: &str) -> bool {
    fn walk(shape: &egui::Shape, needle: &str) -> bool {
        match shape {
            egui::Shape::Text(text) => text.galley.text().contains(needle),
            egui::Shape::Vec(shapes) => shapes.iter().any(|s| walk(s, needle)),
            _ => false,
        }
    }
    shapes.iter().any(|clipped| walk(&clipped.shape, needle))
}

/// A shell on a project (a fresh egui context: a new editor process).
fn start(config: &Path, root: &Path) -> (App, EditorShell, egui::Context) {
    let app = edit_app(config, root);
    let shell = EditorShell::new(None, Some(root.to_path_buf()));
    (app, shell, egui::Context::default())
}

/// The layout `editor.layout_get` reports.
fn layout(world: &mut World) -> Value {
    call(world, "editor.layout_get", json!({})).expect("layout_get")["layout"].clone()
}

/// Every tab id of a reported layout: the main surface's and the floating windows'.
fn tabs(layout: &Value) -> Vec<String> {
    fn walk(node: &Value, out: &mut Vec<String>) {
        if let Some(tabs) = node.pointer("/Tabs/tabs").and_then(Value::as_array) {
            out.extend(tabs.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(split) = node.get("Split") {
            walk(&split["first"], out);
            walk(&split["second"], out);
        }
    }
    let mut out = Vec::new();
    walk(&layout["root"], &mut out);
    for window in layout["floating"].as_array().into_iter().flatten() {
        out.extend(
            window["tabs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    out
}

/// The search box id of Outliner instance `n` (`panel_instances::instance_id`'s rule, written out
/// so the test reads the same before the feature existed).
fn outliner_search(n: u8) -> egui::Id {
    match n {
        0 => egui::Id::new(regia_editor::outliner::SEARCH_BOX),
        n => egui::Id::new((regia_editor::outliner::SEARCH_BOX, n)),
    }
}

#[test]
fn two_outliners_keep_their_own_filters_across_a_restart() {
    let config = unique_dir("outliners-config");
    let root = temp_project("outliners");
    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);

    // Window ▸ New Outliner, through the agent.
    let reply = call(world, "editor.panel_new_instance", json!({ "panel": "outliner" })).expect("new");
    assert_eq!(reply["panel"], "outliner#1", "{reply}");
    assert_eq!(reply["instance"], 1);
    assert_eq!(reply["title"], "Outliner 2");
    frames(&mut shell, &ctx, world, 2);
    assert!(
        tabs(&layout(world)).contains(&"outliner#1".to_owned()),
        "{}",
        layout(world)
    );
    let instances = call(world, "editor.panel_instances", json!({})).expect("instances");
    let row = instances["panels"]
        .as_array()
        .expect("panels")
        .iter()
        .find(|row| row["panel"] == "outliner#1")
        .cloned()
        .expect("listed");
    assert_eq!(row["instance"], 1, "{row}");
    // Float it so both Outliners draw at once.
    call(world, "editor.panel_float", json!({ "panel": "outliner#1" })).expect("float");
    let both = frames(&mut shell, &ctx, world, 3);
    assert!(
        has_text(&both.shapes, "Outliner 2"),
        "the second Outliner's tab shows"
    );

    // Each search box is its own: typing in one leaves the other alone.
    type_into(&mut shell, &ctx, world, outliner_search(0), "kind:light");
    type_into(&mut shell, &ctx, world, outliner_search(1), "tag:enemy");
    // Columns of the second Outliner only (its own tree of the view state).
    call(
        world,
        "editor.outliner_columns",
        json!({ "tree": "outliner#1", "set": { "tags": true } }),
    )
    .expect("columns");
    frames(&mut shell, &ctx, world, 2);
    let saved = layout(world);
    assert_eq!(saved["format_version"], 3, "{saved}");
    assert_eq!(saved["instances"]["outliner"]["filter"], "kind:light", "{saved}");
    assert_eq!(saved["instances"]["outliner#1"]["filter"], "tag:enemy", "{saved}");
    let columns = saved["instances"]["outliner#1"]["columns"].clone();
    assert!(
        columns.as_array().is_some_and(|c| c.contains(&json!("tags"))),
        "{columns}"
    );
    assert!(
        saved["instances"]["outliner"]
            .get("columns")
            .is_none_or(|c| c.as_array().is_some_and(Vec::is_empty)),
        "the first Outliner's columns stay in its own tree: {saved}"
    );

    // Exit (the layout is saved into the project) and start again.
    let written = shell.save_layout_on_exit(world).expect("saved on exit");
    let text = std::fs::read_to_string(&written).expect("layout file");
    assert!(text.contains("format_version: 3"), "{text}");
    assert!(text.contains("outliner#1"), "{text}");
    drop(app);

    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);
    let restored = layout(world);
    assert!(tabs(&restored).contains(&"outliner#1".to_owned()), "{restored}");
    assert_eq!(
        restored["instances"]["outliner"]["filter"], "kind:light",
        "{restored}"
    );
    assert_eq!(
        restored["instances"]["outliner#1"]["filter"], "tag:enemy",
        "{restored}"
    );
    // The restored searches are what the boxes show.
    let shown = frames(&mut shell, &ctx, world, 2);
    assert!(
        has_text(&shown.shapes, "kind:light"),
        "the first Outliner's search"
    );
    assert!(
        has_text(&shown.shapes, "tag:enemy"),
        "the second Outliner's search"
    );
}

#[test]
fn closing_an_instance_drops_its_state() {
    let config = unique_dir("close-config");
    let root = temp_project("close");
    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);
    call(world, "editor.panel_new_instance", json!({ "panel": "outliner" })).expect("new");
    frames(&mut shell, &ctx, world, 2);
    call(world, "editor.panel_float", json!({ "panel": "outliner#1" })).expect("float");
    frames(&mut shell, &ctx, world, 3);
    type_into(&mut shell, &ctx, world, outliner_search(1), "tag:enemy");
    assert_eq!(layout(world)["instances"]["outliner#1"]["filter"], "tag:enemy");

    // Close it: show a layout without the instance.
    let mut without = layout(world);
    without["floating"] = json!([]);
    without["instances"] = json!({});
    call(world, "editor.layout_apply", json!({ "layout": without })).expect("apply");
    frames(&mut shell, &ctx, world, 2);
    let closed = layout(world);
    assert!(!tabs(&closed).contains(&"outliner#1".to_owned()), "{closed}");
    assert!(closed["instances"].get("outliner#1").is_none(), "{closed}");

    // A new second Outliner starts fresh.
    let reply = call(world, "editor.panel_new_instance", json!({ "panel": "outliner" })).expect("new");
    assert_eq!(reply["panel"], "outliner#1");
    frames(&mut shell, &ctx, world, 3);
    let fresh = layout(world);
    assert!(tabs(&fresh).contains(&"outliner#1".to_owned()), "{fresh}");
    assert!(fresh["instances"].get("outliner#1").is_none(), "{fresh}");
}

#[test]
fn two_content_browsers_browse_different_folders() {
    let config = unique_dir("content-config");
    let root = temp_project("content");
    for (dir, file) in [
        ("textures", "rockface.material.ron"),
        ("meshes", "cratebox.material.ron"),
    ] {
        std::fs::create_dir_all(root.join("assets").join(dir)).expect("folder");
        std::fs::write(root.join("assets").join(dir).join(file), "()").expect("asset");
    }
    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);
    let reply = call(world, "editor.panel_new_instance", json!({ "panel": "content" })).expect("new");
    assert_eq!(reply["panel"], "content#1", "{reply}");
    assert_eq!(reply["title"], "Content 2");
    frames(&mut shell, &ctx, world, 2);
    call(world, "editor.panel_float", json!({ "panel": "content#1" })).expect("float");
    // The first browser's tab is shown again in its group (floating its neighbour moved the group on).
    call(world, "editor.layout_apply", json!({ "open": "content" })).expect("open");
    let before = frames(&mut shell, &ctx, world, 3);
    // Both browsers start at `assets/`: neither lists a file of a subfolder yet.
    assert!(!has_text(&before.shapes, "cratebox") && !has_text(&before.shapes, "rockface"));

    // Each browser opens its own folder (the state a saved layout carries).
    let mut doc = layout(world);
    doc["instances"] = json!({
        "content": { "folder": "assets/meshes" },
        "content#1": { "folder": "assets/textures" },
    });
    call(world, "editor.layout_apply", json!({ "layout": doc })).expect("apply");
    let shown = frames(&mut shell, &ctx, world, 4);
    let now = layout(world);
    assert_eq!(now["instances"]["content"]["folder"], "assets/meshes", "{now}");
    assert_eq!(
        now["instances"]["content#1"]["folder"], "assets/textures",
        "{now}"
    );
    assert!(
        has_text(&shown.shapes, "cratebox"),
        "the first browser lists meshes/"
    );
    assert!(
        has_text(&shown.shapes, "rockface"),
        "the second browser lists textures/"
    );

    // Across a restart.
    shell.save_layout_on_exit(world).expect("saved on exit");
    drop(app);
    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);
    let restored = layout(world);
    assert_eq!(
        restored["instances"]["content"]["folder"], "assets/meshes",
        "{restored}"
    );
    assert_eq!(
        restored["instances"]["content#1"]["folder"], "assets/textures",
        "{restored}"
    );
}

#[test]
fn the_palette_opens_instances_and_the_agent_refuses_what_has_none() {
    let config = unique_dir("palette-config");
    let root = temp_project("palette");
    let (mut app, mut shell, ctx) = start(&config, &root);
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 3);
    // The keyboard path of Window ▸ New Inspector: the palette command.
    let out = call(world, "editor.command", json!({ "run": "window.new_inspector" })).expect("command");
    assert_eq!(out["ran"]["id"], "window.new_inspector", "{out}");
    frames(&mut shell, &ctx, world, 2);
    assert!(tabs(&layout(world)).contains(&"inspector#1".to_owned()));

    assert_eq!(
        call(world, "editor.panel_new_instance", json!({ "panel": "viewport" })).expect_err("refused"),
        "panel_not_instanced"
    );
    assert_eq!(
        call(
            world,
            "editor.panel_new_instance",
            json!({ "panel": "outliner#1" })
        )
        .expect_err("refused"),
        "panel_not_instanced"
    );
    assert_eq!(
        call(world, "editor.panel_new_instance", json!({ "panel": "nothing" })).expect_err("refused"),
        "unknown_panel"
    );
    // Seven more Inspectors at most: #1 is open, #2..#7 queue, the next is refused.
    for n in 2..=7 {
        let reply = call(
            world,
            "editor.panel_new_instance",
            json!({ "panel": "inspector" }),
        )
        .expect("new");
        assert_eq!(reply["instance"], n, "{reply}");
    }
    assert_eq!(
        call(
            world,
            "editor.panel_new_instance",
            json!({ "panel": "inspector" })
        )
        .expect_err("refused"),
        "panel_instances_full"
    );
    frames(&mut shell, &ctx, world, 2);
    let all = tabs(&layout(world));
    for n in 1..=7 {
        assert!(all.contains(&format!("inspector#{n}")), "{all:?}");
    }
}
