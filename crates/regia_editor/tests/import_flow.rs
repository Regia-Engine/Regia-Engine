// SPDX-License-Identifier: MPL-2.0
//! M11 integration: import → place → play without restarting (`docs/specs/19-editor.md`
//! § 8 M11). The files enter the project through the content browser's import path (the same
//! function the Import… button and `editor.import` call), land in the dev cache, are placed as
//! entities, and appear in the play world.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_editor::agent_ext;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack};
use regia_editor::content::ContentBrowserState;
use regia_editor::level_io;
use regia_editor::pie::{PieRequest, PieSession, PlayMode};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::prelude::{Name, Transform};
use regia_scene::spawn::SnapshotFilter;
use serde_json::json;

fn edit_app(project: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>()
        .insert_resource(agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    agent_ext::install(app.world_mut());
    app.update();
    app
}

fn snapshot_text(app: &mut App) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    let file = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.to_ron_string().expect("ron")
}

/// Drains the queued PIE requests like the editor loop does.
fn drain(app: &mut App, session: &mut Option<PieSession>) {
    let requests: Vec<PieRequest> = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = regia_editor::pie::apply_request(app.world_mut(), session, request, None);
    }
    regia_editor::pie::sync_status(app.world_mut(), session.as_ref());
}

/// A minimal valid PNG (1×1), encoded with the pipeline's own image dependency.
fn tiny_png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder
        .write_image(&[200u8, 40, 40, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png");
    bytes
}

#[test]
fn import_place_play_without_restart() {
    // A scratch project directory.
    let dir = std::env::temp_dir().join(format!("regia-editor-import-flow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets")).expect("project dir");

    let mut app = edit_app(&dir);

    // 1. Import: a PNG through the agent surface (the Import… button calls the same function).
    let source = dir.join("sign.png");
    std::fs::write(&source, tiny_png()).expect("png source");
    let out = agent_ext::test_support::import(
        app.world_mut(),
        &json!({
            "sources": [source.display().to_string()],
            "folder": "textures",
            "client": "m11-test",
        }),
    )
    .expect("import");
    assert_eq!(out["imported"][0], "assets/textures/sign.png", "{out}");
    assert!(out["errors"].as_array().is_some_and(|e| e.is_empty()), "{out}");

    // 2. The new asset appears in the content browser's scan without a restart.
    let browser = ContentBrowserState {
        project_root: Some(dir.clone()),
        ..Default::default()
    };
    assert!(
        browser
            .assets()
            .iter()
            .any(|(path, _)| path == "assets/textures/sign.png"),
        "the imported asset appears"
    );
    // And it cooked into the dev cache (the manifest exists and mentions it).
    let platform = regia_asset_pipeline::host_platform();
    let cache = regia_asset::cooked_cache_dir(&dir, platform);
    let manifest = std::fs::read(cache.join("asset_manifest.bin")).expect("manifest");
    let manifest = regia_asset::AssetManifest::from_postcard(&manifest).expect("parse manifest");
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.path == "assets/textures/sign.png"),
        "the cooked manifest lists the import"
    );

    // 3. Place: spawn entities (a mesh stand-in and a prefab) after the import — no restart.
    let out = agent_ext::test_support::create_asset(
        app.world_mut(),
        &json!({ "kind": "prefab", "path": "assets/props/crate.prefab.ron" }),
    )
    .expect("create prefab");
    assert_eq!(out["path"], "assets/props/crate.prefab.ron");
    let out = agent_ext::test_support::spawn_prefab(
        app.world_mut(),
        &json!({ "path": "assets/props/crate.prefab.ron", "translation": [1.0, 0.5, 0.0] }),
    )
    .expect("spawn prefab");
    assert!(out["entity"].as_u64().is_some(), "{out}");
    let mut command =
        SpawnEntities::new_entity(app.world(), Guid::new_random(), "SignMesh", Transform::default())
            .expect("spawn");
    command.apply(app.world_mut());
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));

    // 4. Play: the play world contains the placed entities (and the edit world is untouched).
    let before = snapshot_text(&mut app);
    let mut session = None;
    agent_ext::test_support::play(app.world_mut(), &json!({})).expect("play request");
    drain(&mut app, &mut session);
    let mut session = session.expect("play started");
    assert_eq!(session.mode, PlayMode::Playing);
    {
        let world = session.app.world_mut();
        let names: Vec<String> = world
            .query::<&Name>()
            .iter(world)
            .map(|n| n.as_str().to_string())
            .collect();
        assert!(
            names.contains(&"SignMesh".to_string()) && names.contains(&"crate".to_string()),
            "the placed content plays: {names:?}"
        );
        // The prefab instance resolved in the play world (its template crossed over).
        let mut pending = world.query::<&regia_scene::prelude::PendingPrefab>();
        assert_eq!(pending.iter(world).count(), 0, "no pending prefab left");
        let mut instances = world.query::<&regia_scene::prelude::PrefabInstance>();
        assert_eq!(instances.iter(world).count(), 1, "one prefab instance");
    }

    // 5. Stop restores the edit world byte-identically.
    regia_editor::pie::stop_play(session);
    let after = snapshot_text(&mut app);
    assert_eq!(before, after, "stop restored the edit world");

    let _ = std::fs::remove_dir_all(&dir);
}
