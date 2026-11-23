// SPDX-License-Identifier: MPL-2.0
//! The incremental level autosave (ADR-7748) in the editor's own frame loop: the headless shell
//! draws its frames between the autosaves, so a system or panel that wrote a saved component every
//! frame would make every entity count as changed. Counted, never timed (review sprint-w11-rev-e-k
//! #5: the lane's acceptance ran on a bare scene app only).

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_editor::shell::EditorShell;
use serde_json::{Value, json};

const ENTITIES: usize = 40;

fn temp_project() -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir("autosave-incremental-shell");
    std::fs::create_dir_all(root.join("assets/levels")).expect("assets");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"A\"\nid = \"com.example.a\"\n",
    )
    .expect("manifest");
    let config = regia_test_utils::unique_temp_dir("autosave-incremental-shell-config");
    (root, config)
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

/// Shell frames without a GPU, each followed by an app update (the editor's loop).
fn frames(shell: &mut EditorShell, ctx: &egui::Context, app: &mut App, count: u32, start: u32) {
    for frame in start..start + count {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(f64::from(frame) / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, app.world_mut(), None));
        output.textures_delta.clear();
        app.update();
    }
}

fn call(world: &mut World, method: &str, params: Value) -> Value {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method} {params}: {e:?}"))
}

/// `level_snapshot` of `editor.autosave_state` after `editor.autosave_now`.
fn autosave_snapshot(app: &mut App) -> Value {
    let now = call(app.world_mut(), "editor.autosave_now", json!({}));
    assert!(now["level"].is_string(), "the dirty level autosaved: {now}");
    call(app.world_mut(), "editor.autosave_state", json!({}))["level_snapshot"].clone()
}

#[test]
fn shell_frames_between_autosaves_reflect_only_the_edited_entity() {
    let (root, config) = temp_project();
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    let mut entities = Vec::new();
    for i in 0..ENTITIES {
        let out = call(
            app.world_mut(),
            "editor.spawn",
            json!({ "name": format!("Rock{i}") }),
        );
        entities.push(out["entity"].as_u64().expect("entity"));
    }
    frames(&mut shell, &ctx, &mut app, 4, 0);
    let first = autosave_snapshot(&mut app);
    assert_eq!(
        first["full"], true,
        "the first snapshot starts the cache: {first}"
    );

    // Frames with no edit: nothing the shell or the app runs per frame touches a saved component.
    frames(&mut shell, &ctx, &mut app, 6, 4);
    let idle = autosave_snapshot(&mut app);
    assert_eq!(idle["full"], false, "{idle}");
    assert_eq!(
        idle["reflected"], 0,
        "shell frames alone re-reflected entities: {idle}"
    );

    // One edit, more frames: exactly that entity is reflected again.
    call(
        app.world_mut(),
        "editor.set_transform",
        json!({ "entity": entities[ENTITIES / 2], "translation": [1.0, 2.0, 3.0] }),
    );
    frames(&mut shell, &ctx, &mut app, 6, 10);
    let edited = autosave_snapshot(&mut app);
    assert_eq!(edited["full"], false, "{edited}");
    assert_eq!(edited["reflected"], 1, "one edit reflects one entity: {edited}");
    assert!(
        edited["reused"]
            .as_u64()
            .is_some_and(|reused| reused >= ENTITIES as u64 - 1),
        "{edited}"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&config);
}
