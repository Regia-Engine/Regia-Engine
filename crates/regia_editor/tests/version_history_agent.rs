// SPDX-License-Identifier: MPL-2.0
//! Version history and project duplicate over the real agent dispatcher (ADR-7928,
//! `book/agents/editor-autosave.md` "Version history"): `editor.versions`,
//! `editor.version_preview`, `editor.version_restore` and `editor.duplicate_project`.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::selection::Selection;
use regia_editor::viewport::{EditorCamera, ViewportSettings};
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    /// The editor's edit world with the agent plugin and an in-process inbox (as
    /// `agent_parity_shell` builds it).
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<ViewportSettings>();
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        regia_editor::agent_ext::install(app.world_mut());
        regia_editor::headless::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.try_call(method, params.clone())
            .unwrap_or_else(|err| panic!("{method} {params}: {err:?}"))
    }

    fn names(&mut self) -> Vec<String> {
        let world = self.app.world_mut();
        let mut query = world.query::<&regia_scene::prelude::Name>();
        let mut out: Vec<String> = query.iter(world).map(|n| n.as_str().to_owned()).collect();
        out.sort();
        out
    }
}

fn code(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// P-325 end to end: a save and an autosave each keep a version; the preview reads one; the
/// restore replaces the open level as one undo step that `editor.undo` reverts; the errors are the
/// documented codes.
#[test]
fn versions_preview_restore_and_undo_over_the_dispatcher() {
    let dir = regia_test_utils::unique_temp_dir("version-history-agent");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("assets");
    std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"History\"\n").expect("manifest");
    let mut agent = Agent::new(&dir);
    agent.call("editor.spawn", json!({ "name": "SavedRock" }));
    agent.call(
        "editor.save_level",
        json!({ "path": "assets/maps/main.level.ron" }),
    );
    agent.call("editor.spawn", json!({ "name": "AutosavedRock" }));
    agent.call("editor.autosave_now", json!({}));

    let listed = agent.call("editor.versions", json!({}));
    assert_eq!(listed["target"]["kind"], "level", "{listed}");
    assert_eq!(
        listed["target"]["original"], "assets/maps/main.level.ron",
        "{listed}"
    );
    let sources: Vec<&str> = listed["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .filter_map(|v| v["source"].as_str())
        .collect();
    assert_eq!(sources, ["autosave", "save"], "{listed}");
    let saved_id = listed["versions"][1]["id"].as_str().expect("id").to_owned();

    let shown = agent.call("editor.version_preview", json!({ "id": saved_id }));
    assert_eq!(shown["entities"], json!(["SavedRock"]), "{shown}");
    assert_eq!(shown["source"], "save");

    let restored = agent.call("editor.version_restore", json!({ "id": saved_id }));
    assert_eq!(restored["restored"], json!(saved_id), "{restored}");
    assert_eq!(agent.names(), ["SavedRock"]);
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent.names(),
        ["AutosavedRock", "SavedRock"],
        "undo reverts the restore"
    );

    let err = agent
        .try_call(
            "editor.version_restore",
            json!({ "id": "0123456789abcdef/0000000000001-000000-save" }),
        )
        .expect_err("missing");
    assert_eq!(code(&err), "no_such_version");
    let err = agent
        .try_call("editor.version_preview", json!({}))
        .expect_err("no id");
    assert_eq!(code(&err), "invalid_version_id");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-327 end to end: `editor.duplicate_project` copies the sources beside the project and refuses
/// an existing folder and one inside the project.
#[test]
fn duplicate_project_over_the_dispatcher() {
    let dir = regia_test_utils::unique_temp_dir("version-history-duplicate");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("assets");
    std::fs::create_dir_all(dir.join("target/debug")).expect("target");
    std::fs::write(dir.join("target/debug/out"), "x").expect("target file");
    std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"Dup\"\n").expect("manifest");
    let mut agent = Agent::new(&dir);
    let name = format!("{}-copy", dir.file_name().and_then(|n| n.to_str()).expect("name"));
    let out = agent.call("editor.duplicate_project", json!({ "dest": name }));
    let copy = dir.with_file_name(&name);
    assert!(copy.join("Regia.toml").is_file(), "{out}");
    assert!(!copy.join("target").exists(), "{out}");
    assert!(
        out["skipped"]
            .as_array()
            .is_some_and(|s| s.contains(&json!("target/"))),
        "{out}"
    );
    let err = agent
        .try_call("editor.duplicate_project", json!({ "dest": name }))
        .expect_err("exists");
    assert_eq!(code(&err), "duplicate_dest_exists");
    let inside = dir.join("assets/copy").display().to_string();
    let err = agent
        .try_call("editor.duplicate_project", json!({ "dest": inside }))
        .expect_err("inside");
    assert_eq!(code(&err), "duplicate_dest_inside");
    let err = agent
        .try_call("editor.duplicate_project", json!({}))
        .expect_err("no dest");
    assert_eq!(code(&err), "invalid_dest");
    let _ = std::fs::remove_dir_all(&copy);
    let _ = std::fs::remove_dir_all(&dir);
}
