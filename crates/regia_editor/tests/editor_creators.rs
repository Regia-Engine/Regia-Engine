// SPDX-License-Identifier: MPL-2.0
//! The editor's creators end to end (specs/19 § 2, § 5, § 6; ADR-7234): each is driven headless
//! through its egui widgets (clicks found by the drawn label) and through the installed agent
//! method table, the one the dispatcher serves.
//!
//! - Content Browser New: anim graph, sound cue, UI layout and data asset files that their loaders
//!   read back.
//! - Data Asset panel: open, edit (one undo step), save, reopen.
//! - Project Settings pages: physics layers, localization, packaging, audio buses.
//! - New script wizard: name, template, Create, registration at the marker.
//! - Project browser: the template picker creates a Third Person project.

use std::path::Path;

use egui::{Event, Modifiers, PointerButton, Pos2, Rect, vec2};
use regia_app::prelude::{Reflect, ReflectDefault};
use regia_asset::data_asset::RegisterDataAssetExt;
use regia_ecs::prelude::*;
use regia_editor::content::{ContentBrowserState, content_browser_panel};
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A game-style data asset type: registered as one, so a game loads what the editor creates.
#[derive(Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Default)]
struct WaveSpec {
    count: u32,
    delay: f32,
    name: String,
}
impl regia_asset::data_asset::Asset for WaveSpec {}

/// A registered `ReflectDefault` struct that is not a data asset: the agent surface refuses it.
#[derive(Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Default)]
struct NotAnAsset {
    factor: f32,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// An editor world over a project at `root` with `WaveSpec` registered as a data asset and
/// `NotAnAsset` registered as a plain type.
fn editor_world(root: &Path) -> World {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_data_asset::<WaveSpec>();
    app.register_type::<NotAnAsset>();
    let mut world = std::mem::take(app.world_mut());
    let open = regia_editor::level_io::new_level(&mut world);
    world.insert_resource(open);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world.insert_resource(regia_editor::command::UndoStack::new());
    world
}

/// Feeds frames to a draw function and finds widgets by their drawn text.
struct Ui {
    ctx: egui::Context,
    time: f64,
    /// The screen height (points); a long panel needs a tall screen to draw every row.
    height: f32,
    /// `(text, rect)` of every text shape of the last frame.
    texts: Vec<(String, Rect)>,
}

impl Ui {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            time: 0.0,
            height: 900.0,
            texts: Vec::new(),
        }
    }

    /// A screen tall enough for the whole Project Settings panel.
    fn tall() -> Self {
        Self {
            height: 6000.0,
            ..Self::new()
        }
    }

    fn frame(&mut self, events: Vec<Event>, draw: &mut dyn FnMut(&mut egui::Ui)) {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(text) => {
                    out.push((
                        text.galley.text().to_owned(),
                        Rect::from_min_size(text.pos, text.galley.size()),
                    ));
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        self.time += 0.05;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, self.height))),
            events,
            time: Some(self.time),
            ..Default::default()
        };
        let mut output = self.ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| draw(ui));
        });
        output.textures_delta.clear();
        self.texts.clear();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut self.texts);
        }
    }

    fn settle(&mut self, draw: &mut dyn FnMut(&mut egui::Ui)) {
        self.frame(vec![], draw);
        self.frame(vec![], draw);
    }

    fn find(&self, label: &str) -> Option<Pos2> {
        self.texts
            .iter()
            .find(|(text, _)| text == label)
            .map(|(_, rect)| rect.center())
    }

    fn all_text(&self) -> String {
        self.texts
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Clicks the widget drawn with `label`.
    fn click(&mut self, label: &str, draw: &mut dyn FnMut(&mut egui::Ui)) {
        self.settle(draw);
        let at = self
            .find(label)
            .unwrap_or_else(|| panic!("`{label}` is drawn; drawn:\n{}", self.all_text()));
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        self.frame(vec![Event::PointerMoved(at)], draw);
        self.frame(vec![button(true)], draw);
        self.frame(vec![button(false)], draw);
        self.frame(vec![], draw);
    }
}

#[test]
fn the_content_browser_new_menu_creates_every_kind_its_loader_reads() {
    let root = regia_test_utils::unique_temp_dir("creators-new");
    write(
        &root.join("Regia.toml"),
        "[project]\nname = \"C\"\nid = \"com.example.c\"\n",
    );
    std::fs::create_dir_all(root.join("assets/new")).expect("assets");
    let mut world = editor_world(&root);
    let mut state = ContentBrowserState {
        project_root: Some(root.clone()),
        folder: "assets/new".into(),
        ..Default::default()
    };
    let mut ui = Ui::new();
    for label in ["Anim graph", "Sound cue", "UI layout"] {
        let mut draw = |ui: &mut egui::Ui| {
            let mut open = world
                .remove_resource::<regia_editor::level_io::OpenLevel>()
                .expect("open level");
            content_browser_panel(ui, &mut world, &mut state, &mut open);
            world.insert_resource(open);
        };
        ui.click("New", &mut draw);
        ui.click(label, &mut draw);
    }
    let graph =
        std::fs::read_to_string(root.join("assets/new/anim_graph-1.animgraph.ron")).expect("anim graph");
    regia_animation::graph::AnimGraph::from_ron_str(&graph).expect("the anim graph loader reads it");
    let cue = std::fs::read_to_string(root.join("assets/new/sound_cue-1.cue.ron")).expect("sound cue");
    let cue = regia_audio::SoundCueDef::from_ron(&cue).expect("the cue loader reads it");
    assert_eq!(cue.name, "sound_cue-1");
    let layout = std::fs::read_to_string(root.join("assets/new/ui_layout-1.ui.ron")).expect("ui layout");
    regia_ui::parse_ui_ron(&layout, "ui_layout-1.ui.ron", &regia_ui::UiMigrators::default())
        .expect("the UI loader reads it");

    // The menu's Data asset entry and the agent method create the same file shape.
    regia_editor::content::new_asset_from_menu(
        &mut world,
        &mut state,
        regia_editor::content::NewAssetKind::DataAsset,
        Some(std::any::type_name::<WaveSpec>()),
    );
    let data = std::fs::read_to_string(root.join("assets/new/data_asset-1.data.ron")).expect("data asset");
    regia_editor::data_asset::DataAssetFile::from_ron_str(&data).expect("a data asset file");
    for (kind, path) in [
        ("anim_graph", "assets/agent/a.animgraph.ron"),
        ("sound_cue", "assets/agent/a.cue.ron"),
        ("ui_layout", "assets/agent/a.ui.ron"),
    ] {
        let reply = call(
            &mut world,
            "editor.create_asset",
            json!({ "kind": kind, "path": path }),
        )
        .expect(kind);
        assert_eq!(reply["path"], path);
    }
    let reply = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_asset", "path": "assets/agent/a.data.ron", "type": std::any::type_name::<WaveSpec>() }),
    )
    .expect("data asset");
    assert_eq!(reply["path"], "assets/agent/a.data.ron");
    let refused = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_asset", "path": "assets/agent/b.data.ron" }),
    );
    assert!(refused.is_err(), "a data asset needs its type");
    // The list offers only registered data asset types, and creating one a game cannot load is
    // refused with a known code (nothing written).
    let plain = std::any::type_name::<NotAnAsset>();
    let listed = call(&mut world, "editor.data_asset_types", json!({})).expect("types");
    let listed = listed["types"].as_array().expect("list");
    assert!(
        !listed.iter().any(|t| t.as_str() == Some(plain)),
        "an unregistered type is not listed: {listed:?}"
    );
    let refused = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_asset", "path": "assets/agent/plain.data.ron", "type": plain }),
    )
    .expect_err("refused");
    assert!(
        refused.message.contains("data_asset_unknown_type"),
        "{}",
        refused.message
    );
    assert!(
        !root.join("assets/agent/plain.data.ron").exists(),
        "the refused create writes nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_data_asset_panel_edits_with_undo_saves_and_reopens() {
    let root = regia_test_utils::unique_temp_dir("creators-data");
    let mut world = editor_world(&root);
    let types = call(&mut world, "editor.data_asset_types", json!({})).expect("types");
    let type_path = std::any::type_name::<WaveSpec>();
    assert!(
        types["types"]
            .as_array()
            .expect("list")
            .iter()
            .any(|t| t == type_path),
        "{types}"
    );
    call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_asset", "path": "assets/waves/first.data.ron", "type": type_path }),
    )
    .expect("create");
    let opened = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": "assets/waves/first.data.ron" }),
    )
    .expect("open");
    assert_eq!(opened["type"], type_path);
    assert_eq!(opened["dirty"], false);
    let set = call(
        &mut world,
        "editor.data_asset_set",
        json!({ "field": "count", "value": 12 }),
    )
    .expect("set");
    assert_eq!(set["changed"], true);
    assert_eq!(set["dirty"], true);
    call(
        &mut world,
        "editor.data_asset_set",
        json!({ "field": "name", "value": "opening" }),
    )
    .expect("set name");
    // One undo step per edit: undo takes the name back, the count stays.
    call(&mut world, "editor.undo", json!({})).expect("undo");
    let state = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": "assets/waves/first.data.ron" }),
    );
    assert_eq!(
        reason(&state.expect_err("unsaved edits block a reopen")),
        "data_asset_unsaved"
    );
    let bad = call(
        &mut world,
        "editor.data_asset_set",
        json!({ "field": "nope", "value": 1 }),
    );
    assert_eq!(reason(&bad.expect_err("no field")), "data_asset_no_field");
    let saved = call(&mut world, "editor.data_asset_save", json!({})).expect("save");
    assert_eq!(saved["path"], "assets/waves/first.data.ron");

    // The panel draws the open asset headless (its type and fields) and makes no edit.
    let depth = world.resource::<regia_editor::command::UndoStack>().undo_depth();
    let mut ui = Ui::new();
    ui.settle(&mut |ui: &mut egui::Ui| regia_editor::data_asset::data_asset_panel(ui, &mut world));
    let drawn = ui.all_text();
    assert!(drawn.contains("WaveSpec"), "{drawn}");
    assert!(drawn.contains("count"), "{drawn}");
    assert_eq!(
        world.resource::<regia_editor::command::UndoStack>().undo_depth(),
        depth
    );

    // A fresh editor reads the saved value back.
    let mut fresh = editor_world(&root);
    let reopened = call(
        &mut fresh,
        "editor.data_asset_open",
        json!({ "path": "assets/waves/first.data.ron" }),
    )
    .expect("reopen");
    assert_eq!(reopened["value"]["count"], 12, "{reopened}");
    assert_eq!(
        reopened["value"]["name"], "",
        "the undone name edit was not saved: {reopened}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The Data Asset panel's Revert through the agent (ADR-7452): `editor.data_asset_open` with
/// `discard: true` drops the unsaved edits and reloads the file; without it the reopen is refused.
#[test]
fn the_data_asset_revert_discards_unsaved_edits_through_the_agent() {
    let root = regia_test_utils::unique_temp_dir("creators-data-revert");
    let mut world = editor_world(&root);
    let type_path = std::any::type_name::<WaveSpec>();
    let path = "assets/waves/revert.data.ron";
    call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_asset", "path": path, "type": type_path }),
    )
    .expect("create");
    let opened = call(&mut world, "editor.data_asset_open", json!({ "path": path })).expect("open");
    assert_eq!(opened["discarded"], false, "{opened}");
    call(
        &mut world,
        "editor.data_asset_set",
        json!({ "field": "count", "value": 7 }),
    )
    .expect("set");
    let refused = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": false }),
    );
    assert_eq!(
        reason(&refused.expect_err("unsaved edits block a plain reopen")),
        "data_asset_unsaved"
    );
    let bad = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": "yes" }),
    );
    assert_eq!(
        reason(&bad.expect_err("discard is a boolean")),
        "data_asset_bad_discard"
    );
    let reverted = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": true }),
    )
    .expect("revert");
    assert_eq!(reverted["discarded"], true, "{reverted}");
    assert_eq!(reverted["dirty"], false, "{reverted}");
    assert_eq!(
        reverted["value"]["count"], 0,
        "the file's value is back: {reverted}"
    );
    // rev-claude-58 parity finding 1 (ADR-7495): a refused discard-open (a missing file) keeps
    // the open asset's unsaved edits.
    call(
        &mut world,
        "editor.data_asset_set",
        json!({ "field": "count", "value": 9 }),
    )
    .expect("set");
    let missing = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": "assets/waves/missing.data.ron", "discard": true }),
    );
    assert!(missing.is_err(), "a missing file is refused");
    let state = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": false }),
    );
    assert_eq!(
        reason(&state.expect_err("the edits survived the refused call")),
        "data_asset_unsaved"
    );
    call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": true }),
    )
    .expect("revert");
    // A clean asset has nothing to discard.
    let again = call(
        &mut world,
        "editor.data_asset_open",
        json!({ "path": path, "discard": true }),
    )
    .expect("reopen clean");
    assert_eq!(again["discarded"], false, "{again}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_project_settings_pages_edit_physics_localization_packaging_and_list_buses() {
    let root = regia_test_utils::unique_temp_dir("creators-pages");
    write(
        &root.join("Regia.toml"),
        "[project]\nname = \"P\"\nid = \"com.example.p\"\n\n[platforms.android]\nmin_sdk = 30\n",
    );
    write(
        &root.join("config/physics.ron"),
        "(layers: [\"world\", \"debris\"], collides: [(\"world\", \"debris\")], queries: [])",
    );
    let mut world = editor_world(&root);
    let mut settings = regia_editor::project_settings::ProjectSettingsState::default();
    settings.load(&root).expect("load");

    // Physics layers: one editor, the Project Settings layer matrix (ADR-7267). Through its
    // widgets as the shell draws the panel: only the last layer has a remove button.
    let mut ui = Ui::tall();
    {
        let mut draw = |ui: &mut egui::Ui| {
            regia_editor::creators::project_settings_pages(ui, &mut world, &mut settings, &root);
            regia_editor::project_settings::project_settings_panel(ui, &mut settings, Some(&root));
        };
        ui.click("Physics layers (config/physics.ron)", &mut draw);
        assert!(ui.find("Remove world").is_none(), "{}", ui.all_text());
        ui.click("Remove debris", &mut draw);
    }
    let file = regia_physics::LayerFile::load(&root.join("config/physics.ron")).expect("loads");
    assert_eq!(file.layers, ["world"]);
    assert!(
        file.collides.is_empty(),
        "the removed layer's pairs go too: {file:?}"
    );

    // The same matrix through the agent.
    let physics = call(
        &mut world,
        "editor.physics_layers_edit",
        json!({ "edits": [
            { "op": "add_layer", "name": "player" },
            { "op": "set_collides", "a": "player", "b": "world", "on": true },
        ] }),
    )
    .expect("physics");
    assert_eq!(physics["layers"], json!(["world", "player"]));
    assert_eq!(physics["collides"], json!([["world", "player"]]));
    let refused = call(
        &mut world,
        "editor.physics_layers_edit",
        json!({ "edits": [{ "op": "set_collides", "a": "player", "b": "ghost", "on": true }] }),
    );
    assert_eq!(error_code(&refused.expect_err("unknown layer")), "unknown_layer");
    call(&mut world, "editor.undo", json!({})).expect("undo");
    let undone = regia_physics::LayerFile::load(&root.join("config/physics.ron")).expect("loads");
    assert_eq!(
        undone.layers,
        ["world"],
        "undo restores the file before the agent edit"
    );

    let loc = call(
        &mut world,
        "editor.project_localization_edit",
        json!({ "edits": [{ "op": "add_locale", "locale": "de" }, { "op": "source", "locale": "en-US" }] }),
    )
    .expect("localization");
    assert_eq!(loc["localization"]["locales"], json!(["de"]));
    assert_eq!(loc["localization"]["source"], "en-US");

    let pack = call(
        &mut world,
        "editor.project_packaging_edit",
        json!({ "packaging": { "android": { "orientation": "portrait", "key_password": "env:KEY_PASS" }, "linux": { "formats": ["tar", "appimage"] } } }),
    )
    .expect("packaging");
    assert_eq!(pack["packaging"]["android"]["orientation"], "portrait");
    assert_eq!(pack["packaging"]["android"]["min_sdk"], 30, "other keys kept");
    let literal = call(
        &mut world,
        "editor.project_packaging_edit",
        json!({ "packaging": { "android": { "keystore_password": "hunter2" } } }),
    );
    assert_eq!(
        reason(&literal.expect_err("literal password")),
        "packaging_secret_literal"
    );
    let manifest = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    assert!(!manifest.contains("hunter2"), "{manifest}");

    let got = call(&mut world, "editor.project_settings_get", json!({})).expect("get");
    assert_eq!(got["pages"]["physics"]["layers"], json!(["world"]));
    assert_eq!(got["pages"]["localization"]["locales"], json!(["de"]));
    assert_eq!(
        got["pages"]["packaging"]["linux"]["formats"],
        json!(["tar", "appimage"])
    );
    assert!(
        got["pages"]["audio"]["buses"]
            .as_array()
            .is_some_and(|b| b.iter().any(|n| n == "Master")),
        "{got}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_new_script_wizard_creates_a_component_script_at_the_marker() {
    let root = regia_test_utils::unique_temp_dir("creators-script");
    write(&root.join("game/src/lib.rs"), "//! game\n\n// REGIA:SCRIPTS\n");
    let mut world = editor_world(&root);
    let opened = call(&mut world, "editor.script_wizard", json!({ "open": true })).expect("open");
    assert_eq!(opened["open"], true);
    assert_eq!(opened["templates"], json!(["empty", "component"]));

    let mut wizard = regia_editor::creators::ScriptWizard {
        open: true,
        name: "Door Switch".into(),
        ..Default::default()
    };
    let mut created = None;
    let mut ui = Ui::new();
    {
        let mut draw = |ui: &mut egui::Ui| {
            if let Some(path) = regia_editor::creators::script_wizard_ui(ui, &mut wizard, Some(&root)) {
                created = Some(path);
            }
        };
        ui.click("component", &mut draw);
        ui.click("Create", &mut draw);
    }
    let path = created.expect("Create wrote a script");
    assert!(
        path.ends_with("game/src/scripts/door_switch.rs"),
        "{}",
        path.display()
    );
    let body = std::fs::read_to_string(&path).expect("script");
    assert!(body.contains("pub struct DoorSwitch {}"), "{body}");
    let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
    assert!(lib.contains("mod door_switch;\n// REGIA:SCRIPTS"), "{lib}");

    // The agent method takes the same templates.
    let reply = call(
        &mut world,
        "editor.create_script",
        json!({ "name": "spawner", "template": "component" }),
    )
    .expect("agent");
    assert!(
        reply["path"].as_str().is_some_and(|p| p.ends_with("spawner.rs")),
        "{reply}"
    );
    let bad = call(
        &mut world,
        "editor.create_script",
        json!({ "name": "x", "template": "nope" }),
    );
    assert_eq!(reason(&bad.expect_err("unknown template")), "script_bad_template");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_project_browser_template_picker_creates_a_third_person_project() {
    let dir = regia_test_utils::unique_temp_dir("creators-browser");
    regia_editor::project::set_recent_file_for_tests(dir.join("recent.ron"));
    let mut state = regia_editor::project::ProjectBrowserState {
        name: "Picker Game".into(),
        parent: dir.display().to_string(),
        ..Default::default()
    };
    let mut action = None;
    let mut ui = Ui::new();
    {
        let mut draw = |ui: &mut egui::Ui| {
            if let Some(a) = regia_editor::project::browser_panel(ui, &mut state) {
                action = Some(a);
            }
        };
        for (_, label) in regia_editor::project::BROWSER_TEMPLATES {
            ui.settle(&mut draw);
            assert!(ui.find(label).is_some(), "{label} offered: {}", ui.all_text());
        }
        ui.click("Third Person", &mut draw);
        ui.click("Create project", &mut draw);
    }
    assert_eq!(action, Some(regia_editor::project::BrowserAction::Create));
    assert_eq!(state.template, "third_person");
    // What the shell does with the action.
    let mut world = World::new();
    let opened = regia_editor::project::create_and_open(
        &mut world,
        &state.name,
        Path::new(&state.parent),
        &state.template,
    )
    .expect("create");
    assert!(
        opened.root.join("game/src/hud.rs").is_file(),
        "the Third Person template's files, not Blank's"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// One physics-layers editor (review rev-claude-33-k findings 1 and 2, ADR-7267)
// ---------------------------------------------------------------------------------------------

/// The agent error code of a refused call, whichever data key the method family uses.
fn error_code(err: &regia_agent::protocol::RpcError) -> String {
    let data = err.data.as_ref();
    data.and_then(|d| d.get("reason"))
        .or_else(|| data.and_then(|d| d.get("code")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A project whose `config/physics.ron` names three layers; a level's crate would sit on
/// `debris` (index 2).
fn three_layer_project(label: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(label);
    write(
        &root.join("Regia.toml"),
        "[project]\nname = \"P\"\nid = \"com.example.p\"\n",
    );
    write(
        &root.join("config/physics.ron"),
        "(layers: [\"world\", \"props\", \"debris\"], collides: [(\"world\", \"debris\")], queries: [])",
    );
    root
}

/// Removing a layer that is not the last would move every later layer's colliders onto their
/// neighbor's row (colliders and saved levels store layer indices). No editor agent method that
/// edits the physics config may do it: each refuses with `layer_not_last` and writes nothing.
#[test]
fn no_editor_method_removes_a_middle_physics_layer() {
    let root = three_layer_project("creators-physics-middle");
    let file = root.join("config/physics.ron");
    let before = std::fs::read_to_string(&file).expect("seeded");
    let mut world = editor_world(&root);
    let editors: Vec<&'static str> = regia_editor::agent_ext::all_methods()
        .into_iter()
        .filter(|(schema, _)| schema.mutating && schema.name.contains("physics"))
        .map(|(schema, _)| schema.name)
        .collect();
    assert!(!editors.is_empty(), "an agent method edits config/physics.ron");
    for method in editors {
        let result = call(
            &mut world,
            method,
            json!({ "edits": [{ "op": "remove_layer", "name": "props" }] }),
        );
        let err = result.expect_err(&format!("{method} refuses to remove a middle layer"));
        assert_eq!(error_code(&err), "layer_not_last", "{method}: {err:?}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("file"),
            before,
            "{method} wrote the file"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// One agent surface writes `config/physics.ron` (`editor.physics_layers_edit`), and the Project
/// Settings JSON (`editor.project_settings_get` `pages.physics`) is that surface's view.
#[test]
fn one_agent_surface_edits_the_physics_config() {
    let root = three_layer_project("creators-physics-one-agent");
    let mut world = editor_world(&root);
    let editors: Vec<&'static str> = regia_editor::agent_ext::all_methods()
        .into_iter()
        .filter(|(schema, _)| schema.mutating && schema.name.contains("physics"))
        .map(|(schema, _)| schema.name)
        .collect();
    assert_eq!(editors, ["editor.physics_layers_edit"]);

    let get = call(&mut world, "editor.physics_layers_get", json!({})).expect("get");
    let settings = call(&mut world, "editor.project_settings_get", json!({})).expect("settings");
    assert_eq!(
        settings["pages"]["physics"], get,
        "pages.physics is the matrix view"
    );

    // Edits through the one surface show in both views and are one undo step.
    let edited = call(
        &mut world,
        "editor.physics_layers_edit",
        json!({ "edits": [
            { "op": "rename_layer", "from": "props", "to": "crates" },
            { "op": "set_collides", "a": "crates", "b": "world", "on": true },
        ] }),
    )
    .expect("edit");
    assert_eq!(edited["layers"], json!(["world", "crates", "debris"]));
    assert_eq!(
        edited["collides"],
        json!([["world", "crates"], ["world", "debris"]])
    );
    let settings = call(&mut world, "editor.project_settings_get", json!({})).expect("settings");
    assert_eq!(settings["pages"]["physics"], edited);
    call(&mut world, "editor.undo", json!({})).expect("undo");
    let undone = call(&mut world, "editor.physics_layers_get", json!({})).expect("get");
    assert_eq!(undone, get, "undo restores the file before the edit");
    let _ = std::fs::remove_dir_all(&root);
}

/// The Project Settings panel (drawn as the shell draws it: the creators pages, then the panel)
/// shows one physics-layers section, and its only remove button removes the last layer.
#[test]
fn the_project_settings_panel_has_one_physics_matrix_that_removes_only_the_last_layer() {
    let root = three_layer_project("creators-physics-one-ui");
    let mut world = editor_world(&root);
    let mut settings = regia_editor::project_settings::ProjectSettingsState::default();
    let mut ui = Ui::tall();
    let mut draw = |ui: &mut egui::Ui| {
        regia_editor::creators::project_settings_pages(ui, &mut world, &mut settings, &root);
        regia_editor::project_settings::project_settings_panel(ui, &mut settings, Some(&root));
    };
    ui.settle(&mut draw);
    let headers: Vec<&str> = ui
        .texts
        .iter()
        .map(|(t, _)| t.as_str())
        .filter(|t| t.starts_with("Physics layers"))
        .collect();
    assert_eq!(
        headers,
        ["Physics layers (config/physics.ron)"],
        "{}",
        ui.all_text()
    );

    ui.click("Physics layers (config/physics.ron)", &mut draw);
    // Let the header finish opening: the rows below the grids appear once its animation ends.
    ui.settle(&mut draw);
    let removes: Vec<&str> = ui
        .texts
        .iter()
        .map(|(t, _)| t.as_str())
        .filter(|t| ["Remove world", "Remove props", "Remove debris"].contains(t))
        .collect();
    assert_eq!(removes, ["Remove debris"], "{}", ui.all_text());
    ui.click("Remove debris", &mut draw);
    let file = regia_physics::LayerFile::load(&root.join("config/physics.ron")).expect("loads");
    assert_eq!(file.layers, ["world", "props"]);
    assert!(
        file.collides.is_empty(),
        "the removed layer's pairs go too: {file:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn project_settings_set_writes_hygiene_limits_without_touching_other_keys() {
    let root = regia_test_utils::unique_temp_dir("creators-hygiene-limits");
    write(
        &root.join("Regia.toml"),
        "[project]\nname = \"P\"\nid = \"com.example.p\"\n\n[package_hygiene]\nforbidden_strings = [\"keep-me\"]\n",
    );
    let mut world = editor_world(&root);
    let got = call(&mut world, "editor.project_settings_get", json!({})).expect("get");
    assert_eq!(
        got["pages"]["package_hygiene"]["max_memory_bytes"],
        json!(32u64 << 20)
    );
    assert_eq!(
        got["pages"]["package_hygiene"]["scan_window_bytes"],
        json!(1u64 << 20)
    );
    assert_eq!(
        got["pages"]["package_hygiene"]["max_matcher_bytes"],
        json!(8u64 << 20)
    );
    assert_eq!(got["pages"]["package_hygiene"]["max_work"], json!(1u64 << 40));
    assert!(
        !std::fs::read_to_string(root.join("Regia.toml"))
            .expect("manifest")
            .contains("min_sdk"),
        "reading hygiene limits must not invent android keys"
    );

    let set = call(
        &mut world,
        "editor.project_settings_set",
        json!({
            "package_hygiene": { "max_memory_bytes": 4096, "scan_window_bytes": 1024 },
            "save": true
        }),
    )
    .expect("set");
    assert_eq!(set["pages"]["package_hygiene"]["max_memory_bytes"], json!(4096));
    assert_eq!(set["pages"]["package_hygiene"]["scan_window_bytes"], json!(1024));
    assert_eq!(
        set["pages"]["package_hygiene"]["max_file_bytes"],
        json!(1u64 << 30)
    );
    let manifest = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    assert!(manifest.contains("keep-me"), "{manifest}");
    assert!(manifest.contains("max_memory_bytes"), "{manifest}");
    assert!(!manifest.contains("min_sdk"), "{manifest}");

    let refused = call(
        &mut world,
        "editor.project_settings_set",
        json!({ "package_hygiene": { "max_work": 0 }, "save": true }),
    );
    assert_eq!(reason(&refused.expect_err("zero limit")), "bad_settings");
    let wide = call(
        &mut world,
        "editor.project_settings_set",
        json!({
            "package_hygiene": { "max_memory_bytes": 100, "scan_window_bytes": 200 },
            "save": true
        }),
    );
    assert_eq!(
        reason(&wide.expect_err("window larger than memory")),
        "bad_settings"
    );
    let disk = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    assert!(disk.contains("keep-me"), "{disk}");
    assert!(disk.contains("4096"), "{disk}");
    assert!(!disk.contains("= 0"), "{disk}");

    let preview = call(
        &mut world,
        "editor.project_settings_set",
        json!({ "package_hygiene": { "max_matcher_bytes": 2048 }, "save": false }),
    )
    .expect("validate only");
    assert_eq!(preview["saved"], false);
    assert_eq!(
        preview["pages"]["package_hygiene"]["max_matcher_bytes"],
        json!(2048)
    );
    let disk = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    assert!(!disk.contains("2048"), "{disk}");
    let _ = std::fs::remove_dir_all(&root);
}
