// SPDX-License-Identifier: MPL-2.0
//! Level templates (File ▸ New level, `editor.new_level`, `editor.level_templates`; ADR-7856,
//! `book/agents/level-templates.md`). CPU only.
//!
//! - Each built-in template opens a level whose entity kinds are the template's list.
//! - A `.level.ron` under `templates/levels/` is listed, copied into a new unsaved level, and the
//!   template file is never changed.
//! - Open world names the project's terrain when it has one, else it has a large floor.
//! - `file.new_level` (no template) still makes an empty level.
//! - The dialog's keyboard path: Down moves the highlight, Enter creates, Escape closes.

use std::path::Path;

use egui::{Event, Key, Modifiers, Pos2, Rect, vec2};
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io::OpenLevel;
use regia_editor::level_templates::{self, NewLevelDialog};
use regia_render::{DirectionalLight, PointLight, Primitive, PrimitiveShape};
use serde_json::{Value, json};

fn edit_app(root: Option<&Path>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: root.map(Path::to_path_buf),
        });
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

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
        .and_then(|d| d.get("reason").or_else(|| d.get("code")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The entity kinds of the open level, sorted (`plane` and `cuboid` from the primitive's shape).
fn kinds_in(world: &mut World) -> Vec<String> {
    let mut kinds = Vec::new();
    for (shape,) in world.query::<(&Primitive,)>().iter(world) {
        kinds.push(match shape.shape {
            PrimitiveShape::Plane { .. } => "plane".to_owned(),
            PrimitiveShape::Cuboid { .. } => "cuboid".to_owned(),
            ref other => other.kind().to_owned(),
        });
    }
    kinds.extend(
        world
            .query::<&DirectionalLight>()
            .iter(world)
            .map(|_| "directional_light".to_owned()),
    );
    kinds.extend(
        world
            .query::<&PointLight>()
            .iter(world)
            .map(|_| "point_light".to_owned()),
    );
    kinds.extend(
        world
            .query::<&regia_framework::player::PlayerStart>()
            .iter(world)
            .map(|_| "player_start".to_owned()),
    );
    kinds.sort();
    kinds
}

fn project(label: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"T\"\n").expect("manifest");
    root
}

fn sorted(mut kinds: Vec<String>) -> Vec<String> {
    kinds.sort();
    kinds
}

#[test]
fn each_builtin_template_opens_a_level_with_its_kinds() {
    let root = project("lt-builtin");
    let mut app = edit_app(Some(&root));
    let listed = call(app.world_mut(), "editor.level_templates", json!({})).expect("list");
    let templates = listed["templates"].as_array().expect("array").clone();
    let ids: Vec<&str> = templates.iter().filter_map(|t| t["id"].as_str()).collect();
    assert_eq!(ids, ["empty", "basic_lit", "interior", "open_world"]);
    for template in &templates {
        let id = template["id"].as_str().expect("id");
        let want: Vec<String> = sorted(
            template["kinds"]
                .as_array()
                .expect("kinds")
                .iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect(),
        );
        let made = call(
            app.world_mut(),
            "editor.new_level",
            json!({ "template": id, "discard": true }),
        )
        .unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!(made["entities"].as_u64(), Some(want.len() as u64), "{id}: {made}");
        assert_eq!(kinds_in(app.world_mut()), want, "{id}");
        let open = app.world().resource::<OpenLevel>();
        assert!(open.path.is_none(), "{id}: an unsaved level");
        assert!(
            !app.world().resource::<UndoStack>().is_dirty(),
            "{id}: a clean start"
        );
    }
    // The spec of each template, written out.
    let kinds = |id: &str| -> Vec<String> {
        let t = templates.iter().find(|t| t["id"] == id).expect("template");
        sorted(
            t["kinds"]
                .as_array()
                .expect("kinds")
                .iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect(),
        )
    };
    assert!(kinds("empty").is_empty());
    assert_eq!(kinds("basic_lit"), ["directional_light", "plane", "player_start"]);
    assert_eq!(
        kinds("interior"),
        [
            "cuboid",
            "cuboid",
            "cuboid",
            "cuboid",
            "cuboid",
            "plane",
            "player_start",
            "point_light",
            "point_light"
        ]
    );
    // No terrain in this project: Open world has the large floor.
    assert_eq!(
        kinds("open_world"),
        ["directional_light", "plane", "player_start"]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_world_names_the_projects_terrain_instead_of_a_floor() {
    let root = project("lt-terrain");
    let layers = root.join("assets/maps/hills/hills.terrain.layers.ron");
    std::fs::create_dir_all(layers.parent().expect("dir")).expect("dir");
    std::fs::write(&layers, "()").expect("layers");
    let mut app = edit_app(Some(&root));
    let made = call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "open_world" }),
    )
    .expect("create");
    assert_eq!(made["terrain"], "maps/hills/hills.terrain.layers.ron", "{made}");
    assert_eq!(kinds_in(app.world_mut()), ["directional_light", "player_start"]);
    let terrain = app
        .world()
        .resource::<regia_scene::level::LevelSettings>()
        .terrain
        .clone()
        .expect("the level names the terrain");
    assert_eq!(terrain.layers, "maps/hills/hills.terrain.layers.ron");
    assert_eq!(terrain.map, "maps/hills/hills");
    let listed = call(app.world_mut(), "editor.level_templates", json!({})).expect("list");
    let open_world = listed["templates"]
        .as_array()
        .expect("array")
        .iter()
        .find(|t| t["id"] == "open_world")
        .expect("open world");
    assert_eq!(open_world["kinds"], json!(["directional_light", "player_start"]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_project_template_is_listed_and_copied_never_edited() {
    let root = project("lt-project");
    let template = root.join("templates/levels/arena/small.level.ron");
    std::fs::create_dir_all(template.parent().expect("dir")).expect("dir");
    // A level the editor itself wrote, with one entity.
    let mut seed = edit_app(Some(&root));
    call(seed.world_mut(), "editor.spawn", json!({ "name": "Marker" })).expect("spawn");
    let open = seed.world().resource::<OpenLevel>().clone();
    regia_editor::level_io::save_level_file(seed.world_mut(), &open, &template).expect("save template");
    let before = std::fs::read(&template).expect("template bytes");
    std::fs::write(root.join("templates/levels/notes.txt"), "not a level").expect("note");

    let mut app = edit_app(Some(&root));
    let listed = call(app.world_mut(), "editor.level_templates", json!({})).expect("list");
    let entry = listed["templates"]
        .as_array()
        .expect("array")
        .iter()
        .find(|t| t["id"] == "project:arena/small")
        .unwrap_or_else(|| panic!("the project template is listed: {listed}"))
        .clone();
    assert_eq!(entry["label"], "small");
    assert_eq!(entry["builtin"], false);
    assert_eq!(
        listed["templates"].as_array().expect("array").len(),
        5,
        "four built-ins and the one level file, not notes.txt: {listed}"
    );

    let made = call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "project:arena/small", "discard": true }),
    )
    .expect("copy");
    assert_eq!(made["entities"], 1, "{made}");
    let open = app.world().resource::<OpenLevel>().clone();
    assert!(
        open.path.is_none(),
        "the copy is unsaved: it never saves over the template"
    );
    assert!(!app.world().resource::<UndoStack>().is_dirty());
    let world = app.world_mut();
    let names: Vec<String> = world
        .query::<&regia_scene::prelude::Name>()
        .iter(world)
        .map(|n| n.as_str().to_owned())
        .collect();
    assert!(names.iter().any(|n| n == "Marker"), "{names:?}");
    // Edit the copy and save it elsewhere: the template stays byte for byte.
    call(world, "editor.spawn", json!({ "name": "Extra" })).expect("edit the copy");
    let out = root.join("assets/maps/copy.level.ron");
    std::fs::create_dir_all(out.parent().expect("dir")).expect("dir");
    regia_editor::level_io::save_level_file(world, &open, &out).expect("save copy");
    assert_eq!(
        std::fs::read(&template).expect("template"),
        before,
        "template untouched"
    );
    // Two levels from the same template are two levels.
    let first = open.id;
    call(
        world,
        "editor.new_level",
        json!({ "template": "project:arena/small", "discard": true }),
    )
    .expect("again");
    assert_ne!(
        world.resource::<OpenLevel>().id,
        first,
        "a fresh level id each time"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn new_level_is_guarded_like_the_command_and_names_its_errors() {
    let root = project("lt-guard");
    let mut app = edit_app(Some(&root));
    // No template: the old empty behavior.
    let made = call(app.world_mut(), "editor.new_level", json!({})).expect("empty");
    assert_eq!(made["template"], "empty");
    assert_eq!(made["entities"], 0);
    let bad = call(app.world_mut(), "editor.new_level", json!({ "template": "nope" })).expect_err("unknown");
    assert_eq!(reason(&bad), "level_template_unknown");
    let wrong = call(app.world_mut(), "editor.new_level", json!({ "template": 3 })).expect_err("type");
    assert_eq!(wrong.code, regia_agent::protocol::INVALID_PARAMS);
    // Unsaved edits refuse; discard proceeds.
    call(app.world_mut(), "editor.spawn", json!({ "name": "Unsaved" })).expect("spawn");
    let refused = call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "basic_lit" }),
    )
    .expect_err("dirty");
    assert_eq!(reason(&refused), "level_dirty");
    call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "basic_lit", "discard": true }),
    )
    .expect("discard");
    // A broken template file names its error and leaves the open level alone.
    let broken = root.join("templates/levels/broken.level.ron");
    std::fs::create_dir_all(broken.parent().expect("dir")).expect("dir");
    std::fs::write(&broken, "this is not a level").expect("broken");
    let level = app.world().resource::<OpenLevel>().id;
    let err = call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "project:broken" }),
    )
    .expect_err("invalid");
    assert_eq!(reason(&err), "level_template_invalid");
    assert_eq!(
        app.world().resource::<OpenLevel>().id,
        level,
        "the open level is unchanged"
    );
    // The palette command keeps its old meaning: an empty level.
    let out = regia_editor::command_palette::run(app.world_mut(), "file.new_level_discard")
        .expect("the command exists");
    assert_eq!(out, "New level");
    assert!(kinds_in(app.world_mut()).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_dialog_is_keyboard_operable() {
    let root = project("lt-dialog");
    let mut app = edit_app(Some(&root));
    level_templates::open_dialog(app.world_mut());
    let ctx = egui::Context::default();
    let mut time = 0.0;
    let mut frame = |app: &mut App, keys: &[Key]| {
        time += 0.05;
        let events = keys
            .iter()
            .map(|key| Event::Key {
                key: *key,
                physical_key: Some(*key),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            })
            .collect();
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            level_templates::dialog_window(ui.ctx(), app.world_mut());
        });
        output.textures_delta.clear();
    };
    frame(&mut app, &[]);
    assert_eq!(app.world().resource::<NewLevelDialog>().selected, "empty");
    frame(&mut app, &[Key::ArrowDown]);
    assert_eq!(app.world().resource::<NewLevelDialog>().selected, "basic_lit");
    frame(&mut app, &[Key::ArrowDown, Key::ArrowDown]);
    assert_eq!(app.world().resource::<NewLevelDialog>().selected, "open_world");
    frame(&mut app, &[Key::ArrowUp]);
    assert_eq!(app.world().resource::<NewLevelDialog>().selected, "interior");
    frame(&mut app, &[Key::Enter]);
    let dialog = app.world().resource::<NewLevelDialog>();
    assert!(!dialog.open, "Enter created the level and closed the dialog");
    assert_eq!(dialog.created.as_deref(), Some("New level from interior"));
    assert_eq!(kinds_in(app.world_mut()).len(), 9);
    // Escape closes without creating.
    level_templates::open_dialog(app.world_mut());
    frame(&mut app, &[]);
    frame(&mut app, &[Key::Escape]);
    assert!(!app.world().resource::<NewLevelDialog>().open);
    assert_eq!(kinds_in(app.world_mut()).len(), 9, "the level is unchanged");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_dialog_widget_has_a_spoken_name() {
    let root = project("lt-a11y");
    let mut app = edit_app(Some(&root));
    // Unsaved edits show the discard checkbox too.
    call(app.world_mut(), "editor.spawn", json!({ "name": "Dirty" })).expect("spawn");
    level_templates::open_dialog(app.world_mut());
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut update = None;
    for step in 0..3 {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
            time: Some(f64::from(step) * 0.05),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            level_templates::dialog_window(ui.ctx(), app.world_mut())
        });
        output.textures_delta.clear();
        update = output.platform_output.accesskit_update;
    }
    let update = update.expect("accesskit is on");
    let names = regia_editor::a11y_audit::widget_names(&update);
    assert_eq!(
        regia_editor::a11y_audit::unnamed_widgets(&update),
        Vec::<String>::new(),
        "{names:?}"
    );
    for want in [
        "Empty",
        "Basic lit",
        "Interior",
        "Open world",
        "Create level",
        "Cancel",
    ] {
        assert!(names.iter().any(|(_, n)| n == want), "{want} in {names:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The names of the entities in the world, sorted.
fn names_in(world: &mut World) -> Vec<String> {
    let mut names: Vec<String> = world
        .query::<&regia_scene::prelude::Name>()
        .iter(world)
        .map(|n| n.as_str().to_owned())
        .collect();
    names.sort();
    names
}

/// Review g17 finding 4: a project template that parses but fails to spawn (it places a prefab
/// that nests itself) is refused, and the level that was open stays open: its id, and the
/// entity spawned into it after it opened (so not only what its library copy holds).
#[test]
fn a_project_template_that_fails_to_open_leaves_the_open_level_current() {
    use regia_core::id::Guid;
    let root = project("lt-open-fails");
    let mut app = edit_app(Some(&root));
    call(app.world_mut(), "editor.spawn", json!({ "name": "Keep" })).expect("spawn");
    app.world_mut().resource_mut::<UndoStack>().mark_saved();
    let level = app.world().resource::<OpenLevel>().id;
    let names_before = names_in(app.world_mut());
    assert!(names_before.iter().any(|n| n == "Keep"), "{names_before:?}");

    let asset = Guid::new_random();
    let entity = Guid::new_random();
    let text = format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "{entity}", name: "Loop", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#
    );
    let file = regia_scene::format::SceneFile::from_ron_str(&text).expect("parse");
    let registry = app
        .world()
        .resource::<regia_ecs::prelude::AppTypeRegistry>()
        .0
        .clone();
    let template =
        regia_scene::prefab::PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    app.world_mut()
        .get_resource_or_insert_with(regia_scene::prefab::PrefabLibrary::default)
        .insert(template);
    let path = root.join("templates/levels/loop.level.ron");
    std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    std::fs::write(&path, &text).expect("template");

    let err = call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "project:loop" }),
    )
    .expect_err("a prefab that nests itself does not open");
    assert_eq!(reason(&err), "level_template_invalid", "{err:?}");
    assert_eq!(
        app.world().resource::<OpenLevel>().id,
        level,
        "OpenLevel unchanged"
    );
    assert_eq!(
        app.world()
            .resource::<regia_scene::prelude::LevelManager>()
            .current(),
        Some(level),
        "the open level is still the current level"
    );
    assert_eq!(
        names_in(app.world_mut()),
        names_before,
        "the open level's entities are still there"
    );
    // The level still works: a template that does open replaces it.
    call(
        app.world_mut(),
        "editor.new_level",
        json!({ "template": "basic_lit" }),
    )
    .expect("a good template opens");
    assert_ne!(app.world().resource::<OpenLevel>().id, level);
    let _ = std::fs::remove_dir_all(&root);
}
