// SPDX-License-Identifier: MPL-2.0
//! The engine's example extension (`extensions/hello_panel`, ADR-9850 (5)) in a headless editor:
//! discovered under `<engine>/extensions/`, built, and driven through the `editor.extension_*`
//! agent methods (the same functions the menu, toolbar and panel tab call); then overridden by a
//! project copy with the same id (ADR-9850 (a)). No window, no GPU, fixed app steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../../extensions/hello_panel/src/hello.rs"]
mod hello;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use regia_editor::extension::EditorExtensionPlugin;
use regia_framework::extension::discover::{EXTENSION_OVERRIDDEN, MANIFEST_FILE};
use regia_framework::extension::{ExtensionRegistry, ExtensionState};
use serde_json::{Value, json};

/// The engine checkout (`crates/regia_editor` → workspace root).
fn engine_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, text).expect("write");
}

struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project that enables and grants the example; with `copy`, it also holds its own copy of the
/// extension's manifest under another display name and version.
fn project(tag: &str, copy: bool) -> Project {
    let dir = regia_test_utils::unique_temp_dir(tag);
    write(
        &dir.join("Regia.toml"),
        "[project]\nname = \"ext\"\n\n[extensions]\nenabled = [\"hello_panel\"]\n\n[extensions.grants]\nhello_panel = [\"editor_panel\"]\n",
    );
    write(&dir.join("assets/data/table.ron"), "(\n    rows: [],\n)\n");
    if copy {
        let engine = std::fs::read_to_string(engine_dir().join("extensions/hello_panel").join(MANIFEST_FILE))
            .expect("engine manifest");
        let text = engine
            .replace("Hello panel (engine example)", "Hello panel (project copy)")
            .replace("version = \"0.1.0\"", "version = \"0.2.0\"");
        assert_ne!(text, engine, "the copy differs");
        write(&dir.join("extensions/hello_panel").join(MANIFEST_FILE), &text);
    }
    Project(dir)
}

/// A headless editor on `project`: the extension host with the engine directory, the example
/// linked in, and the editor's agent methods.
fn editor(project: &Project) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(EditorExtensionPlugin {
        project: Some(project.0.clone()),
        target_dir: Some(project.0.join("no-target")),
        engine_dir: Some(engine_dir()),
        linked: vec![Arc::new(hello::HelloPanel)],
    });
    let world = app.world_mut();
    world.init_resource::<regia_editor::anim::AnimEditor>();
    world.init_resource::<regia_editor::command::UndoStack>();
    regia_editor::agent_ext::install(world);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(project.0.clone()),
    });
    app.update();
    app
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = app.world().resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(app.world_mut(), &params)
}

fn code(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error.data.as_ref().expect("data")["code"]
            .as_str()
            .expect("code")
            .to_owned(),
    }
}

fn panel_text(app: &mut App) -> Vec<String> {
    let answer = call(
        app,
        "editor.extension_panel",
        json!({ "id": "hello_panel.greeting" }),
    )
    .expect("panel");
    assert_eq!(answer["owner"], "hello_panel");
    assert_eq!(answer["dock"], "bottom");
    answer["text"]
        .as_array()
        .expect("text")
        .iter()
        .map(|t| t.as_str().expect("string").to_owned())
        .collect()
}

#[test]
fn the_engine_example_loads_in_a_headless_editor_with_a_panel_an_action_and_a_tool() {
    let p = project("ext-example-engine", false);
    let mut app = editor(&p);
    {
        let registry = app.world().resource::<ExtensionRegistry>();
        assert_eq!(registry.state(hello::ID), Some(ExtensionState::Active));
        let found = registry.discovered(hello::ID).expect("discovered");
        assert!(
            Path::new(&found.path).starts_with(engine_dir().join("extensions")),
            "loaded from the engine: {}",
            found.path
        );
        assert!(
            registry.codes_for(hello::ID).is_empty(),
            "{:?}",
            registry.codes_for(hello::ID)
        );
    }

    let ui = call(&mut app, "editor.extension_ui", json!({})).expect("ui");
    assert_eq!(
        ui["panels"],
        json!([{ "id": "hello_panel.greeting", "title": "Hello", "dock": "bottom", "owner": "hello_panel" }])
    );
    assert_eq!(
        ui["actions"],
        json!([{
            "id": "hello_panel.say_hello",
            "title": "Say hello",
            "menu": "Hello",
            "toolbar": true,
            "owner": "hello_panel",
            "category": "Hello",
            "shortcut": "",
            "default_shortcut": "",
            "places": [],
            "visible": true,
            "enabled": true,
            "visible_when": null,
            "enabled_when": null,
        }])
    );
    assert_eq!(
        ui["asset_tools"],
        json!([{ "id": "hello_panel.line_count", "title": "Count lines", "suffixes": [".ron"], "owner": "hello_panel" }])
    );

    let text = panel_text(&mut app);
    assert!(
        text.iter()
            .any(|t| t == "Hello from Hello panel (engine example)"),
        "{text:?}"
    );
    assert!(text.iter().any(|t| t == "Said hello 0 times"), "{text:?}");

    let ran = call(
        &mut app,
        "editor.extension_action",
        json!({ "id": "hello_panel.say_hello" }),
    )
    .expect("action");
    assert_eq!(ran["message"], "Hello #1 from Hello panel (engine example)");
    assert_eq!(ran["owner"], "hello_panel");
    // The command palette (and `editor.command { run }`) reaches extension actions by id.
    assert_eq!(
        regia_editor::command_palette::run(app.world_mut(), "hello_panel.say_hello").as_deref(),
        Some("Hello #2 from Hello panel (engine example)")
    );
    assert_eq!(app.world().resource::<hello::HelloCount>().0, 2);
    assert!(panel_text(&mut app).iter().any(|t| t == "Said hello 2 times"));

    let tool = call(
        &mut app,
        "editor.extension_asset_tool",
        json!({ "id": "hello_panel.line_count", "path": "assets/data/table.ron" }),
    )
    .expect("tool");
    assert_eq!(tool["result"], json!({ "lines": 3 }));
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_asset_tool",
            json!({ "id": "hello_panel.line_count", "path": "Regia.toml" })
        )),
        "asset_tool_wrong_type"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_asset_tool",
            json!({ "id": "hello_panel.line_count", "path": "../outside.ron" })
        )),
        "bad_path"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_asset_tool",
            json!({ "id": "hello_panel.nope", "path": "assets/data/table.ron" })
        )),
        "asset_tool_not_found"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_action",
            json!({ "id": "hello_panel.nope" })
        )),
        "extension_action_not_found"
    );
    assert_eq!(
        code(call(
            &mut app,
            "editor.extension_panel",
            json!({ "id": "hello_panel.nope" })
        )),
        "extension_panel_not_found"
    );

    // `plugin.info` lists the three editor registrations by kind.
    let info = app
        .world()
        .resource::<ExtensionRegistry>()
        .info_json(hello::ID)
        .expect("info");
    assert_eq!(
        info["registrations"]["editor_panel"],
        json!(["hello_panel.greeting"])
    );
    assert_eq!(
        info["registrations"]["editor_action"],
        json!(["hello_panel.say_hello"])
    );
    assert_eq!(
        info["registrations"]["asset_tool"],
        json!(["hello_panel.line_count"])
    );
}

#[test]
fn a_project_copy_overrides_the_engine_example() {
    let p = project("ext-example-override", true);
    let mut app = editor(&p);
    {
        let registry = app.world().resource::<ExtensionRegistry>();
        assert_eq!(registry.state(hello::ID), Some(ExtensionState::Active));
        let found = registry.discovered(hello::ID).expect("discovered");
        assert!(
            Path::new(&found.path).starts_with(&p.0),
            "the project copy wins: {}",
            found.path
        );
        assert_eq!(found.manifest.version.to_string(), "0.2.0");
        assert_eq!(registry.codes_for(hello::ID), [EXTENSION_OVERRIDDEN]);
        let listed = registry.list_json(None);
        let rows = listed["extensions"].as_array().expect("rows");
        assert_eq!(
            rows.iter().filter(|row| row["id"] == hello::ID).count(),
            1,
            "one hello_panel: {listed}"
        );
    }
    let text = panel_text(&mut app);
    assert!(
        text.iter().any(|t| t == "Hello from Hello panel (project copy)"),
        "{text:?}"
    );
    let ran = call(
        &mut app,
        "editor.extension_action",
        json!({ "id": "hello_panel.say_hello" }),
    )
    .expect("action");
    assert_eq!(ran["message"], "Hello #1 from Hello panel (project copy)");
}
