// SPDX-License-Identifier: MPL-2.0
//! Outliner rows end to end (ADR-7772): the real panel driven by synthetic egui input with the
//! AccessKit tree on (prefab link and override markers, configuration-warning badges, the Columns
//! menu from the keyboard), and the agent methods through the editor's method table
//! (`editor.config_warnings`, `editor.change_kind`, `editor.outliner_columns`,
//! `editor.tag_selection`, `editor.outliner_set` color and unique).

use egui::{Event, Key, Modifiers, Pos2, Rect, vec2};
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::outliner::outliner_panel;
use regia_scene::prelude::{EntityGuid, Name, Transform};
use serde_json::{Value, json};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<regia_render::PointLight>()
        .register_type::<regia_render::SpotLight>()
        .register_type::<regia_render::RectLight>()
        .register_type::<regia_render::TubeLight>()
        .register_type::<regia_render::DirectionalLight>()
        .register_type::<regia_render::Camera>()
        .register_type::<regia_physics::Collider>();
    app.init_resource::<UndoStack>();
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

/// One frame of the Outliner panel with AccessKit on; returns the tree update.
struct Harness {
    ctx: egui::Context,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Self { ctx, time: 0.0 }
    }

    fn frame(&mut self, world: &mut World, events: Vec<Event>) -> egui::accesskit::TreeUpdate {
        regia_editor::visibility::mark_internal_entities(world);
        self.time += 0.05;
        let mut all = vec![Event::ModifiersChanged(Modifiers::NONE)];
        all.extend(events);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 600.0))),
            events: all,
            time: Some(self.time),
            ..Default::default()
        };
        let mut output = self.ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| outliner_panel(ui, world));
        });
        output.textures_delta.clear();
        output.platform_output.accesskit_update.expect("accesskit is on")
    }

    fn settle(&mut self, world: &mut World) -> egui::accesskit::TreeUpdate {
        self.frame(world, vec![]);
        self.frame(world, vec![])
    }

    fn key(&mut self, world: &mut World, key: Key) -> egui::accesskit::TreeUpdate {
        self.frame(
            world,
            vec![
                Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
                Event::Key {
                    key,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
        self.frame(world, vec![])
    }
}

fn labels(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    regia_editor::a11y_audit::label_names(update)
}

fn focused_name(update: &egui::accesskit::TreeUpdate) -> String {
    update
        .nodes
        .iter()
        .find(|(id, _)| *id == update.focus)
        .and_then(|(_, node)| node.label().map(str::to_string))
        .unwrap_or_default()
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    regia_editor::visibility::mark_internal_entities(world);
    handler(world, &params)
}

fn ok(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|err| panic!("{method}: {err:?}"))
}

fn code(world: &mut World, method: &str, params: Value) -> String {
    let err = call(world, method, params).expect_err("refused");
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// P-098: a prefab instance's row shows the link icon; an instance with overrides also shows the
/// override dot; a plain entity shows neither.
#[test]
fn prefab_rows_show_the_link_icon_and_overridden_ones_the_override_dot() {
    let mut app = app();
    let world = app.world_mut();
    let plain = spawn(world, "Plain");
    let linked = spawn(world, "Linked");
    world
        .entity_mut(linked)
        .insert(regia_scene::prefab::PrefabInstance {
            prefab: 0x1234,
            ..Default::default()
        });
    let overridden = spawn(world, "Overridden");
    world
        .entity_mut(overridden)
        .insert(regia_scene::prefab::PrefabInstance {
            prefab: 0x1234,
            removed_children: vec![7],
            ..Default::default()
        });
    let mut harness = Harness::new();
    let update = harness.settle(world);
    let labels = labels(&update);
    let link = labels.iter().filter(|l| l.as_str() == "\u{1f517}").count();
    assert_eq!(link, 2, "one link icon per instance row: {labels:?}");
    // The override dot is a button (it opens the Overrides window, ADR-7790) whose spoken name is
    // "Overrides" (a reader never speaks the "\u{25cf}" glyph), so it is a widget, not a label.
    let widgets = regia_editor::a11y_audit::widget_names(&update);
    let dot = widgets
        .iter()
        .filter(|(role, name)| role == "Button" && name == "Overrides")
        .count();
    assert_eq!(dot, 1, "one override dot, on the overridden row: {widgets:?}");
    // The agent reads the same marks.
    let marks = |world: &mut World, e: Entity| {
        let reply = ok(world, "editor.outliner_set", json!({ "entity": e.to_bits() }));
        (reply["prefab"].as_bool(), reply["prefab_overrides"].as_bool())
    };
    assert_eq!(marks(world, plain), (Some(false), Some(false)));
    assert_eq!(marks(world, linked), (Some(true), Some(false)));
    assert_eq!(marks(world, overridden), (Some(true), Some(true)));
}

/// P-105: a warned row and its ancestor carry the warning badges (spoken text with the fix hint);
/// the agent lists the same warning; the checks do not re-run on idle frames.
#[test]
fn warning_badges_on_the_row_and_its_ancestor_and_checks_run_on_change_only() {
    let mut app = app();
    let world = app.world_mut();
    let group = spawn(world, "Lights");
    let lamp = spawn(world, "Lamp");
    world.entity_mut(lamp).insert((
        ChildOf(group),
        regia_render::PointLight {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: 0.0,
            radius: 4.0,
            cast_shadows: false,
        },
    ));
    let mut harness = Harness::new();
    harness.settle(world);
    let update = harness.settle(world);
    let labels = labels(&update);
    assert!(
        labels.iter().any(
            |l| l.starts_with("Configuration warning: This light has zero intensity")
                && l.contains("light_zero_intensity")
                && l.contains("Fix: ")
        ),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|l| l == "A child has a configuration warning"),
        "{labels:?}"
    );
    let evaluations = world
        .resource::<regia_editor::config_checks::ConfigChecks>()
        .evaluations();
    for _ in 0..4 {
        harness.frame(world, vec![]);
    }
    assert_eq!(
        world
            .resource::<regia_editor::config_checks::ConfigChecks>()
            .evaluations(),
        evaluations,
        "idle frames evaluate no entity"
    );
    let reply = ok(
        world,
        "editor.config_warnings",
        json!({ "entity": lamp.to_bits() }),
    );
    assert_eq!(
        reply["entities"][0]["warnings"][0]["code"],
        "light_zero_intensity"
    );
    assert_eq!(
        reply["evaluated"], 0,
        "nothing changed since the panel's last update"
    );
    let all = ok(world, "editor.config_warnings", json!({}));
    assert_eq!(all["warned"], 1);
    let rows = regia_editor::asset_panels::config_warning_rows(world);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].validator, "config_checks");
    assert_eq!(rows[0].asset, "Lamp");
    assert_eq!(
        code(world, "editor.config_warnings", json!({ "entity": 999_999_u64 })),
        "F0002"
    );
}

/// The Columns menu from the keyboard: Tab to it, open it, Tab to a column checkbox and toggle it
/// with Space; the Type column then shows in the view state.
#[test]
fn the_columns_menu_is_reachable_and_operable_from_the_keyboard() {
    let mut app = app();
    let world = app.world_mut();
    spawn(world, "Crate");
    let mut harness = Harness::new();
    harness.settle(world);
    let mut found = false;
    for _ in 0..40 {
        let update = harness.key(world, Key::Tab);
        if focused_name(&update) == "Columns" {
            found = true;
            break;
        }
    }
    assert!(found, "Tab reaches the Columns menu button");
    harness.key(world, Key::Space);
    let mut on_type = false;
    for _ in 0..40 {
        let update = harness.key(world, Key::Tab);
        if focused_name(&update) == "Type" {
            on_type = true;
            break;
        }
    }
    assert!(on_type, "Tab reaches the Type checkbox inside the open menu");
    harness.key(world, Key::Space);
    let reply = ok(world, "editor.outliner_columns", json!({}));
    let shown: Vec<&str> = reply["shown"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(shown.contains(&"type"), "{reply}");
}

/// The agent surface: columns (set, widths, cells), tags on the selection, change kind and the
/// row color / unique-name toggles of `editor.outliner_set`, each with its refusal codes.
#[test]
fn agent_columns_tags_change_kind_color_and_unique() {
    let mut app = app();
    let world = app.world_mut();
    let a = spawn(world, "A");
    let b = spawn(world, "B");
    let reply = ok(
        world,
        "editor.outliner_columns",
        json!({ "set": { "tags": true, "layer": true }, "widths": { "tags": 120 }, "entity": a.to_bits() }),
    );
    assert_eq!(reply["widths"]["tags"], 120.0);
    assert_eq!(reply["cells"]["layer"], "0");
    assert_eq!(
        code(
            world,
            "editor.outliner_columns",
            json!({ "set": { "nope": true } })
        ),
        "unknown_column"
    );

    let tagged = ok(
        world,
        "editor.tag_selection",
        json!({ "add": "loot", "entities": [a.to_bits(), b.to_bits()] }),
    );
    assert_eq!(tagged["changed"].as_array().map(Vec::len), Some(2));
    assert_eq!(tagged["undo_depth"], 1);
    let cells = ok(world, "editor.outliner_columns", json!({ "entity": b.to_bits() }));
    assert_eq!(cells["cells"]["tags"], "loot");
    assert_eq!(
        code(
            world,
            "editor.tag_selection",
            json!({ "add": "x", "remove": "y" })
        ),
        "tag_op_required"
    );
    assert_eq!(
        code(
            world,
            "editor.tag_selection",
            json!({ "add": " ", "entities": [a.to_bits()] })
        ),
        "empty_tag"
    );

    world.entity_mut(a).insert(regia_render::PointLight {
        position: [0.0; 3],
        color: [0.2, 0.4, 0.6],
        intensity: 50.0,
        radius: 3.0,
        cast_shadows: false,
    });
    let listed = ok(world, "editor.change_kind", json!({ "entity": a.to_bits() }));
    assert_eq!(listed["current"], json!(["point_light"]));
    let changed = ok(
        world,
        "editor.change_kind",
        json!({ "entity": a.to_bits(), "kind": "spot_light" }),
    );
    assert_eq!(changed["from"], "point_light");
    assert!(
        changed["kept"]
            .as_array()
            .is_some_and(|k| k.contains(&json!("range")))
    );
    assert_eq!(
        world.get::<regia_render::SpotLight>(a).map(|s| s.range),
        Some(3.0)
    );
    assert_eq!(
        code(
            world,
            "editor.change_kind",
            json!({ "entity": b.to_bits(), "kind": "spot_light" })
        ),
        "kind_not_compatible"
    );
    assert_eq!(
        code(
            world,
            "editor.change_kind",
            json!({ "entity": a.to_bits(), "kind": "lamp" })
        ),
        "unknown_kind"
    );

    let colored = ok(
        world,
        "editor.outliner_set",
        json!({ "entity": a.to_bits(), "color": "sky" }),
    );
    assert_eq!(colored["color"], "sky");
    assert_eq!(
        code(
            world,
            "editor.outliner_set",
            json!({ "entity": a.to_bits(), "color": "teal" })
        ),
        "unknown_color"
    );
    let cleared = ok(
        world,
        "editor.outliner_set",
        json!({ "entity": a.to_bits(), "color": null }),
    );
    assert_eq!(cleared["color"], Value::Null);

    world.entity_mut(b).insert(Name::new("A"));
    let unique = ok(
        world,
        "editor.outliner_set",
        json!({ "entity": a.to_bits(), "unique": true }),
    );
    assert_eq!(unique["unique"], true);
    assert!(
        unique["badges"]
            .as_array()
            .is_some_and(|b| b.contains(&json!("unique_name")))
    );
    assert_eq!(
        code(
            world,
            "editor.outliner_set",
            json!({ "entity": b.to_bits(), "unique": true })
        ),
        "unique_name_taken"
    );
    assert_eq!(regia_scene::unique_name::find_unique(world, "A"), Some(a));
}

/// Review rev-g3 finding 6: `editor.outliner_columns` writes the view file, so it is a mutating
/// method that needs `edit_scene`; reading the columns and a row's cells is the read-only
/// `editor.outliner_view`, which refuses to change anything.
#[test]
fn the_columns_setter_is_mutating_and_the_view_reader_is_read_only() {
    let methods = regia_editor::agent_ext::all_methods();
    let schema = |name: &str| {
        methods
            .iter()
            .find(|(schema, _)| schema.name == name)
            .map(|(schema, _)| (schema.mutating, schema.permission))
    };
    assert_eq!(
        schema("editor.outliner_columns"),
        Some((true, regia_agent::schema::Permission::EditScene)),
        "editor.outliner_columns writes .regia/outliner_view.ron"
    );
    assert_eq!(
        schema("editor.outliner_view"),
        Some((false, regia_agent::schema::Permission::Read))
    );
    let mut app = app();
    let world = app.world_mut();
    let a = spawn(world, "A");
    ok(
        world,
        "editor.outliner_columns",
        json!({ "set": { "layer": true } }),
    );
    let view = ok(world, "editor.outliner_view", json!({ "entity": a.to_bits() }));
    assert_eq!(view["cells"]["layer"], "0", "{view}");
    assert_eq!(view["file_error"], Value::Null);
    for params in [
        json!({ "set": { "tags": true } }),
        json!({ "widths": { "layer": 100 } }),
        json!({ "sort": "creation" }),
    ] {
        assert_eq!(code(world, "editor.outliner_view", params), "view_is_read_only");
    }
    let after = ok(world, "editor.outliner_view", json!({}));
    assert_eq!(
        after["shown"], view["shown"],
        "a refused call changed the columns"
    );
    assert_eq!(after["sort"], "name");
}

/// Review rev-g3 finding 4: the row's sequence badge (select the entity, open Sequence Curves) has
/// an agent route, `editor.open_sequence`.
#[test]
fn the_sequence_badge_has_an_agent_route() {
    let mut app = app();
    let world = app.world_mut();
    let plain = spawn(world, "Plain");
    let cutscene = spawn(world, "Cutscene");
    world
        .entity_mut(cutscene)
        .insert(regia_framework::sequencer::SequenceBindings::default());
    world.insert_resource(regia_editor::selection::Selection::default());
    let reply = ok(
        world,
        "editor.open_sequence",
        json!({ "entity": cutscene.to_bits() }),
    );
    assert_eq!(reply["selected"], json!([cutscene.to_bits()]));
    assert_eq!(
        world.resource::<regia_editor::selection::Selection>().entities(),
        [cutscene]
    );
    assert!(
        world
            .resource::<regia_editor::editor_layout::LayoutRequests>()
            .pending
            .contains(&regia_editor::editor_layout::LayoutRequest::Open(
                regia_editor::shell::Panel::SeqCurves
            ))
    );
    assert_eq!(
        code(
            world,
            "editor.open_sequence",
            json!({ "entity": plain.to_bits() })
        ),
        "no_sequence"
    );
    assert_eq!(
        code(world, "editor.open_sequence", json!({ "entity": 999_999_u64 })),
        "no_such_entity"
    );
}

const GATE_GRAPH: &str = r#"(format_version: 1, world: ["gatehouse"], rules: [
    (name: "creak", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "creak"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/alarm")), "value": Literal(Bool(true)) })]),
])"#;

/// A world with an entity that carries a compiled logic graph (the row's logic-graph badge).
fn spawn_with_graph(world: &mut World, name: &str) -> Entity {
    let def = regia_ai::logic::LogicGraphDef::from_ron(GATE_GRAPH).expect("graph parses");
    let graph =
        regia_ai::LogicGraph::compile(&def, &regia_ai::BtTaskRegistry::default()).expect("graph compiles");
    let entity = spawn(world, name);
    world.entity_mut(entity).insert(graph);
    entity
}

/// P-107: the logic-graph badge opens that graph in the Logic Graph panel, from the keyboard (Tab
/// to the badge button, Enter), and a Logic Graph panel with unsaved edits is not replaced.
#[test]
fn a_logic_badge_opens_its_graph() {
    let mut app = app();
    let world = app.world_mut();
    let gate = spawn_with_graph(world, "Gate");
    world.insert_resource(regia_editor::selection::Selection::default());
    let mut harness = Harness::new();
    let update = harness.settle(world);
    assert_eq!(
        regia_editor::a11y_audit::unnamed_widgets(&update),
        Vec::<String>::new(),
        "every badge button is named"
    );
    let mut found = false;
    for _ in 0..40 {
        let update = harness.key(world, Key::Tab);
        if focused_name(&update) == "Open this entity's logic graph" {
            found = true;
            break;
        }
    }
    assert!(found, "Tab reaches the logic-graph badge");
    harness.key(world, Key::Enter);
    harness.settle(world);
    let editor = world.resource::<regia_editor::logic_graph_editor::LogicEditor>();
    assert!(editor.is_open());
    assert!(
        editor.doc.nodes.len() >= 2,
        "the creak rule: an event and an action node"
    );
    assert!(editor.path.is_none(), "an entity's graph has no file");
    assert!(!editor.dirty(), "opening is not an edit");
    assert_eq!(
        world.resource::<regia_editor::selection::Selection>().entities(),
        [gate]
    );
    assert!(
        world
            .resource::<regia_editor::editor_layout::LayoutRequests>()
            .pending
            .contains(&regia_editor::editor_layout::LayoutRequest::Open(
                regia_editor::shell::Panel::LogicGraph
            ))
    );
    // The agent route is the same operation; unsaved edits in the panel are never replaced.
    world
        .resource_mut::<regia_editor::logic_graph_editor::LogicEditor>()
        .doc
        .description = "edited".into();
    let other = spawn_with_graph(world, "Other");
    assert_eq!(
        code(
            world,
            "editor.open_badge",
            json!({ "entity": other.to_bits(), "badge": "logic_graph" })
        ),
        "logic_graph_unsaved"
    );
    assert_eq!(
        world
            .resource::<regia_editor::logic_graph_editor::LogicEditor>()
            .doc
            .description,
        "edited"
    );
    let plain = spawn(world, "Plain");
    assert_eq!(
        code(
            world,
            "editor.open_badge",
            json!({ "entity": plain.to_bits(), "badge": "logic_graph" })
        ),
        "no_logic_graph"
    );
    assert_eq!(
        code(
            world,
            "editor.open_badge",
            json!({ "entity": gate.to_bits(), "badge": "bogus" })
        ),
        "unknown_badge"
    );
    assert_eq!(
        code(
            world,
            "editor.open_badge",
            json!({ "entity": 999_999_u64, "badge": "script" })
        ),
        "no_such_entity"
    );
}

/// P-107: a script badge resolves its component's declaration in the mod's folder (file and line,
/// through the configured code editor command; a dry run starts nothing), and when the mod is not
/// a folder of the project the error and the hover text say why.
#[test]
fn a_script_badge_opens_its_declaration_or_says_why_not() {
    use regia_editor::editor_prefs::{EditorPreferences, PrefValue};
    let project = regia_test_utils::unique_temp_dir("outliner-script-badge");
    std::fs::create_dir_all(project.join("mods/props")).expect("dir");
    std::fs::write(
        project.join("mods/props/luamod.ron"),
        r#"(format_version: 1, id: "props", name: "props", version: "0.1.0", entry: "main.lua", api_version: 1)"#,
    )
    .expect("manifest");
    std::fs::write(
        project.join("mods/props/main.lua"),
        "-- props\n\nregia.component(\"mover\", {\n    props = { speed = { type = \"number\", default = 2.0 } },\n})\n",
    )
    .expect("main");
    let mut app = app();
    regia_editor::lua_mods::install(&mut app);
    app.world_mut().run_schedule(Startup);
    let world = app.world_mut();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(project.clone()),
    });
    let mut prefs = EditorPreferences::default();
    prefs
        .set(
            "code.editor_command",
            &PrefValue::Text("myeditor {file}:{line}".into()),
        )
        .expect("pref");
    world.insert_resource(prefs);
    regia_editor::lua_mods::load_project_mods(world, &project);
    let scripted = spawn(world, "Scripted");
    world.entity_mut(scripted).insert(regia_script_lua::LuaComponent {
        decl: "mover".into(),
        ..Default::default()
    });
    let reply = ok(
        world,
        "editor.open_badge",
        json!({ "entity": scripted.to_bits(), "badge": "script", "dry_run": true }),
    );
    assert_eq!(reply["opened"], "code_editor", "{reply}");
    assert_eq!(reply["line"], 3, "{reply}");
    let file = project.join("mods/props/main.lua").display().to_string();
    assert_eq!(reply["path"], file.as_str(), "{reply}");
    assert_eq!(
        reply["command"],
        json!(["myeditor", format!("{file}:3")]),
        "{reply}"
    );
    assert_eq!(reply["selected"], json!([scripted.to_bits()]));
    assert!(
        regia_editor::outliner_columns::script_badge_hover(world, scripted).contains(&format!("{file}:3")),
        "the hover names the file and line"
    );
    // A script component whose mod is not loaded has no known file: the tooltip says why.
    let orphan = spawn(world, "Orphan");
    world.entity_mut(orphan).insert(regia_script_lua::LuaComponent {
        decl: "ghost".into(),
        ..Default::default()
    });
    assert_eq!(
        code(
            world,
            "editor.open_badge",
            json!({ "entity": orphan.to_bits(), "badge": "script", "dry_run": true })
        ),
        "script_path_unknown"
    );
    let hover = regia_editor::outliner_columns::script_badge_hover(world, orphan);
    assert!(
        hover.contains("cannot open") && hover.contains("not loaded"),
        "{hover}"
    );
    let _ = std::fs::remove_dir_all(&project);
}
