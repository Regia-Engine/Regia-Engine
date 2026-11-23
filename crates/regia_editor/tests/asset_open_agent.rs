// SPDX-License-Identifier: MPL-2.0
//! The asset-open dispatcher, the New kinds and the folder audit actions over the agent protocol
//! (ADR-7725): every call goes through the editor's registered `AgentExtension`, as a client's does.
//! The test uses only protocol strings, so it also builds against an editor without the feature (where
//! it fails: that is the lane's fail-before check).

use std::path::Path;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use serde_json::{Value, json};

fn project(tag: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets/ai")).expect("dir");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    std::fs::write(root.join("assets/ai/guard.bt.ron"), "(root: 0, nodes: [])").expect("bt");
    std::fs::write(root.join("assets/notes.txt"), "x").expect("txt");
    root
}

fn world_in(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    // Procedural graphs read from the project, as the editor's start-up sets it.
    world.insert_resource(regia_procgen::world::ProcGenSettings {
        editor: true,
        source: regia_procgen::world::ProcGraphSource::Dir(root.to_path_buf()),
        ..regia_procgen::world::ProcGenSettings::default()
    });
    regia_editor::agent_ext::install(&mut world);
    world
}

/// Calls `method` through the registered extension; an error answers its `data.code`.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let Some((_, handler)) = extension.method(method) else {
        return Err(format!("{method} is not registered"));
    };
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })
}

#[test]
fn open_asset_opens_a_behavior_tree_in_its_panel_and_refuses_an_unknown_kind() {
    let root = project("asset-open-agent-e2e");
    let mut world = world_in(&root);
    let answer = call(
        &mut world,
        "editor.open_asset",
        json!({ "path": "assets/ai/guard.bt.ron" }),
    )
    .expect("open");
    assert_eq!(answer["kind"], json!("behavior_tree"));
    assert_eq!(answer["queued"], json!(false));
    let state = call(&mut world, "editor.bt_state", json!({})).expect("state");
    assert!(
        state["path"]
            .as_str()
            .is_some_and(|p| p.replace('\\', "/").ends_with("assets/ai/guard.bt.ron")),
        "{state}"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.open_asset",
            json!({ "path": "assets/notes.txt" })
        ),
        Err("asset_kind_unknown".to_owned())
    );
    let kinds = call(&mut world, "editor.asset_kinds", json!({})).expect("kinds");
    assert!(kinds["open"].to_string().contains("\"behavior_tree\""), "{kinds}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn create_asset_makes_the_six_new_kinds() {
    let root = project("asset-open-agent-new");
    let mut world = world_in(&root);
    for (kind, path) in [
        ("procgen_graph", "assets/pcg/forest.pgraph.ron"),
        ("vfx_system", "assets/vfx/sparks.vfx.ron"),
        ("conversation", "assets/conversations/intro.conv.ron"),
        ("curve_table", "assets/data/xp.curves.ron"),
        ("chooser", "assets/data/pick.chooser.ron"),
        ("data_table", "assets/data/weapons.table.ron"),
    ] {
        let answer = call(
            &mut world,
            "editor.create_asset",
            json!({ "kind": kind, "path": path }),
        )
        .unwrap_or_else(|code| panic!("{kind}: {code}"));
        assert_eq!(answer["path"], json!(path));
        assert!(root.join(path).is_file(), "{path}");
        // It opens in its panel through the dispatcher.
        let opened = call(&mut world, "editor.open_asset", json!({ "path": path }))
            .unwrap_or_else(|code| panic!("{kind} open: {code}"));
        assert_eq!(opened["path"], json!(path));
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validate_folder_lists_only_the_folders_problems() {
    const SET: &str = r##"#![enable(implicit_some)]
(
    format_version: 2,
    assets: [
        (path: "assets/props/bad.scene", kind: scene, references: ["assets/props/gone.png"], cooked_bytes: 10),
        (path: "assets/levels/broken.scene", kind: scene, references: ["assets/textures/missing.png"], cooked_bytes: 10),
    ],
)"##;
    let root = project("asset-open-agent-folder");
    let mut world = world_in(&root);
    let problems = call(
        &mut world,
        "editor.problems_panel",
        json!({ "set": SET, "run": true, "folder": "assets/props" }),
    )
    .expect("validate folder");
    let rows = problems["rows"].to_string();
    assert!(rows.contains("assets/props/bad.scene"), "{problems}");
    assert!(
        !rows.contains("assets/levels/broken.scene"),
        "a broken asset outside the folder is not listed: {problems}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
