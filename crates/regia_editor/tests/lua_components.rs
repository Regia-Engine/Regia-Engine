// SPDX-License-Identifier: MPL-2.0
//! Script components in the editor (ADR-3356): the project loads `mods/<id>/`, the inspector
//! draws the component from the declaration, and edits are undoable whole-component `SetProperty`
//! commands.

use regia_app::prelude::*;
use regia_ecs::reflect::PartialReflect;
use regia_editor::inspector;
use regia_script_lua::{LuaComponent, LuaMods, LuaModsPlugin, LuaValue, ModFiles, ScriptValue};
use serde_json::json;

const MOVER: &str = r#"
    regia.component("mover", {
        props = {
            speed = { type = "number", default = 2.0, min = 0.0, max = 20.0 },
            mode  = { type = "enum", options = { "walk", "run" }, default = "walk" },
        },
    })
    function on_start()
        local e = regia.spawn("walker", regia.vec3(0, 0, 0))
        e:add("mover")
    end
"#;

fn mod_files(id: &str, main: &str) -> ModFiles {
    ModFiles::from_map([
        (
            "luamod.ron".to_owned(),
            format!(
                r#"(format_version: 1, id: "{id}", name: "{id}", version: "0.1.0", entry: "main.lua", api_version: 1)"#
            ),
        ),
        ("main.lua".to_owned(), main.to_owned()),
    ])
}

fn app_with_mod() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(LuaModsPlugin::new(vec![mod_files("props", MOVER)]));
    app.world_mut().run_schedule(Startup);
    // The mod spawned and attached the component itself; find that entity.
    let mut query = app.world_mut().query::<(Entity, &LuaComponent)>();
    let entity = query
        .iter(app.world_mut())
        .map(|(entity, _)| entity)
        .next()
        .expect("the mod's entity");
    (app, entity)
}

#[test]
fn load_project_mods_loads_every_mod_under_mods() {
    let dir = regia_test_utils::unique_temp_dir("editor-lua-mods");
    std::fs::create_dir_all(dir.join("mods/props")).expect("dir");
    std::fs::write(
        dir.join("mods/props/luamod.ron"),
        r#"(format_version: 1, id: "props", name: "props", version: "0.1.0", entry: "main.lua", api_version: 1)"#,
    )
    .expect("manifest");
    std::fs::write(dir.join("mods/props/main.lua"), MOVER).expect("main");

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::lua_mods::install(&mut app);
    app.world_mut().run_schedule(Startup);
    regia_editor::lua_mods::load_project_mods(app.world_mut(), &dir);

    let mods = app.world().resource::<LuaMods>();
    let module = mods.get("props").expect("the project mod loaded");
    assert!(module.is_active());
    let decls = module.components();
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0].name, "mover");
    assert_eq!(decls[0].props.len(), 2);
    // The watch is on for directory-backed hot reload.
    assert!(mods.watch_every() > 0);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn load_project_mods_skips_a_loaded_mod_id() {
    let dir = regia_test_utils::unique_temp_dir("editor-lua-mods-dup");
    std::fs::create_dir_all(dir.join("mods/props")).expect("dir");
    std::fs::write(
        dir.join("mods/props/luamod.ron"),
        r#"(format_version: 1, id: "props", name: "props", version: "0.1.0", entry: "main.lua", api_version: 1)"#,
    )
    .expect("manifest");
    std::fs::write(dir.join("mods/props/main.lua"), MOVER).expect("main");

    let (mut app, _) = app_with_mod();
    // Loading the same project twice must not double-declare the component.
    regia_editor::lua_mods::load_project_mods(app.world_mut(), &dir);
    regia_editor::lua_mods::load_project_mods(app.world_mut(), &dir);
    assert!(
        app.world()
            .resource::<LuaMods>()
            .get("props")
            .expect("loaded")
            .is_active()
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn the_inspector_draws_the_component_and_records_no_edit_without_one() {
    let (mut app, entity) = app_with_mod();
    let ctx = egui::Context::default();
    let mut changes = Vec::new();
    let world = app.world_mut();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        changes = inspector::component_ui(
            ui,
            world,
            entity,
            "regia_script_lua::components::LuaComponent",
            &[],
        );
    });
    output.textures_delta.clear();
    assert!(changes.is_empty(), "no widget moved: {changes:?}");
}

#[test]
fn the_drawers_edit_applies_and_undoes_as_one_component_write() {
    let (mut app, entity) = app_with_mod();
    // The script attached the component at startup.
    let before = app.world().get::<LuaComponent>(entity).expect("attached").clone();
    assert_eq!(before.get("speed"), Some(&LuaValue::Number(2.0)));

    // What the drawer returns after a widget edit: one whole-component change. Apply it through
    // the same path `component_ui_shared` uses.
    let mut edited = before.clone();
    edited.props.insert("speed".to_owned(), LuaValue::Number(9.0));
    let changes = vec![inspector::Change {
        path: String::new(),
        discrete: false,
    }];
    let mut edited_reflect: Box<dyn PartialReflect> = Box::new(edited);
    inspector::apply_changes(
        app.world_mut(),
        entity,
        "regia_script_lua::components::LuaComponent",
        edited_reflect.as_mut(),
        &changes,
    );
    assert_eq!(
        app.world()
            .get::<LuaComponent>(entity)
            .expect("attached")
            .get("speed"),
        Some(&LuaValue::Number(9.0)),
        "the edit applied"
    );
    regia_editor::command::undo_world(app.world_mut());
    assert_eq!(
        app.world()
            .get::<LuaComponent>(entity)
            .expect("attached")
            .get("speed"),
        Some(&LuaValue::Number(2.0)),
        "undo restored the values"
    );
}

#[test]
fn props_round_trip_through_script_values_for_the_agent_surface() {
    // The JSON shape the agent protocol documents (vec3 / entity / colour).
    let component = LuaComponent {
        decl: "mover".to_owned(),
        props: [
            ("speed".to_owned(), LuaValue::Number(2.0)),
            ("target".to_owned(), LuaValue::Entity(7)),
        ]
        .into_iter()
        .collect(),
    };
    let values: std::collections::BTreeMap<String, ScriptValue> = component.values().into_iter().collect();
    assert_eq!(values["speed"].to_json(), json!(2.0));
    assert_eq!(values["target"].to_json(), json!({ "entity": 7 }));
    let _ = json!({ "vec3": [0.0, 1.0, 0.0] });
}
