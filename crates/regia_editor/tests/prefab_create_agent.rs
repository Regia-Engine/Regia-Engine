// SPDX-License-Identifier: MPL-2.0
//! Create prefab from selection end to end through the agent protocol (`editor.spawn`,
//! `editor.create_prefab`, `editor.save_level`, `editor.undo`; spec 19 § 2, ADR-7468):
//! the written prefab loads back through the runtime path, the level re-saves with the
//! instance link, undo restores the level's bytes exactly, and a path outside the project is
//! the confined-path code.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::prefab::{PrefabInstance, PrefabLibrary, PrefabTemplate, PrefabWorldExt as _};
use regia_scene::prelude::{SceneFile, SpawnOptions, Transform};
use serde_json::{Value, json};

fn call(app: &mut App, name: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    handler(app.world_mut(), &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> Option<&str> {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
}

#[test]
fn create_prefab_round_trip_through_the_agent_protocol() {
    let dir = regia_test_utils::unique_temp_dir("prefab-create-agent-e2e");
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<regia_editor::selection::Selection>();
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());

    let crate_entity = call(
        &mut app,
        "editor.spawn",
        json!({ "name": "Crate", "translation": [2.0, 0.0, 1.0] }),
    )
    .expect("spawn crate")["entity"]
        .as_u64()
        .expect("id");
    call(
        &mut app,
        "editor.spawn",
        json!({ "name": "Lid", "translation": [0.0, 1.0, 0.0], "parent": crate_entity }),
    )
    .expect("spawn lid");
    let level = dir.join("assets/yard.level.ron");
    let level_path = level.display().to_string();
    call(&mut app, "editor.save_level", json!({ "path": level_path })).expect("save");
    let before = std::fs::read_to_string(&level).expect("level");

    // Outside the project: the confined-path code, nothing replaced.
    let err = call(
        &mut app,
        "editor.create_prefab",
        json!({ "entities": [crate_entity], "path": "../../outside.prefab.ron" }),
    )
    .expect_err("outside");
    assert_eq!(reason(&err), Some("path_outside_project"), "{err:?}");

    let reply = call(
        &mut app,
        "editor.create_prefab",
        json!({ "entities": [crate_entity], "path": "props/crate.prefab.ron" }),
    )
    .expect("create");
    assert_eq!(reply["entities"], 2);
    let asset: Guid = reply["asset"].as_str().expect("asset").parse().expect("guid");

    // The runtime path: parse, bake under the .meta id, spawn in a fresh world.
    let file_path = dir.join("assets/props/crate.prefab.ron");
    let file =
        SceneFile::from_ron_str(&std::fs::read_to_string(&file_path).expect("prefab")).expect("parses");
    assert_eq!(file.format_version, regia_scene::format::SCENE_FORMAT_VERSION);
    let mut runtime = App::new();
    runtime.add_plugins(regia_scene::ScenePlugin);
    let registry = runtime.world().resource::<AppTypeRegistry>().clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    runtime
        .world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(template);
    let root = runtime
        .world_mut()
        .spawn_prefab(
            asset,
            Some(Transform::default()),
            &SpawnOptions::default(),
            Guid::new_random(),
        )
        .expect("runtime spawn");
    assert!(runtime.world().get::<PrefabInstance>(root).is_some());

    // The level re-saves with the instance link.
    call(&mut app, "editor.save_level", json!({ "path": level_path })).expect("save after");
    let after = SceneFile::from_ron_str(&std::fs::read_to_string(&level).expect("level")).expect("parses");
    assert!(
        after.entities.iter().any(|e| e
            .prefab_instance
            .as_ref()
            .is_some_and(|p| p.prefab.guid() == asset)),
        "the level holds the instance"
    );
    assert!(after.entities.iter().all(|e| e.name.as_deref() != Some("Lid")));

    // Undo restores the level bytes exactly.
    assert_eq!(
        call(&mut app, "editor.undo", json!({})).expect("undo")["undone"],
        true
    );
    call(&mut app, "editor.save_level", json!({ "path": level_path })).expect("save undone");
    assert_eq!(std::fs::read_to_string(&level).expect("level"), before);
    let _ = std::fs::remove_dir_all(&dir);
}
