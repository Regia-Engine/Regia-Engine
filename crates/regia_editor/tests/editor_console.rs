// SPDX-License-Identifier: MPL-2.0
//! The status bar, drawer and console methods over the real dispatcher (rule 8, ADR-7754): every
//! call goes through registration, the parameter schema and permissions, fed from an in-process
//! inbox so each one is answered within one frame. A console line answers what the agent call it
//! names answers, and the history survives a restart.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::selection::Selection;
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
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
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource::<OpenLevel>(open);
        level_io::reset_edit_state(app.world_mut());
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        regia_editor::agent_ext::install(app.world_mut());
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
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }
}

fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn a_console_line_answers_what_the_agent_call_answers() {
    let project = regia_test_utils::unique_temp_dir("editor_console_reply");
    let mut client = Client::new(&project);
    let direct = client.call("editor.get_selection", json!({}));
    let via_console = client.call("editor.console_run", json!({ "line": "editor.get_selection {}" }));
    assert_eq!(via_console["reply"], direct);
    assert_eq!(via_console["pending"], false);
    // A command id runs as editor.command { run }.
    let grid = client
        .app
        .world()
        .resource::<regia_editor::viewport::ViewportSettings>()
        .grid;
    let ran = client.call("editor.console_run", json!({ "line": "view.toggle_grid" }));
    assert_eq!(ran["reply"]["ran"]["id"], "view.toggle_grid");
    assert_ne!(
        client
            .app
            .world()
            .resource::<regia_editor::viewport::ViewportSettings>()
            .grid,
        grid
    );
    // The called method's error comes back as that error.
    let err = client
        .try_call(
            "editor.console_run",
            json!({ "line": "editor.select {\"entities\": \"x\"}" }),
        )
        .expect_err("bad entities");
    assert!(!code(&err).is_empty());
    let err = client
        .try_call("editor.console_run", json!({ "line": "no.such_thing" }))
        .expect_err("unknown");
    assert_eq!(code(&err), "console_unknown_name");
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn completion_names_methods_with_their_parameters() {
    let project = regia_test_utils::unique_temp_dir("editor_console_complete");
    let mut client = Client::new(&project);
    let out = client.call(
        "editor.console_complete",
        json!({ "prefix": "editor.get_selectio" }),
    );
    assert_eq!(out["line"], "editor.get_selection ");
    assert_eq!(out["completions"][0]["kind"], "method");
    let out = client.call("editor.console_complete", json!({ "prefix": "editor.drawer" }));
    assert_eq!(out["line"], "editor.drawer ");
    let hint = out["completions"][0]["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("drawer: string") && hint.contains("open?: boolean"),
        "{hint}"
    );
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn the_history_survives_a_restart() {
    let project = regia_test_utils::unique_temp_dir("editor_console_history");
    {
        let mut client = Client::new(&project);
        client.call("editor.console_run", json!({ "line": "editor.get_selection" }));
        client.call("editor.console_run", json!({ "line": "view.toggle_labels" }));
    }
    let client = Client::new(&project);
    let path = regia_editor::console::history_path(&project);
    assert_eq!(
        regia_editor::console::read_history(&path),
        ["editor.get_selection", "view.toggle_labels"]
    );
    drop(client);
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn drawer_and_status_bar_over_the_dispatcher() {
    let project = regia_test_utils::unique_temp_dir("editor_console_drawer");
    let mut client = Client::new(&project);
    let out = client.call("editor.drawer", json!({ "drawer": "log", "open": true }));
    assert_eq!(out["open_drawer"], "log");
    let status = client.call("editor.status_bar", json!({}));
    assert_eq!(status["drawer"], "log");
    assert_eq!(status["jobs"]["count"], 0);
    let out = client.call("editor.drawer", json!({ "drawer": "log", "dock": true }));
    assert_eq!(out["open_drawer"], Value::Null);
    let err = client
        .try_call("editor.drawer", json!({ "drawer": "side" }))
        .expect_err("bad drawer");
    assert_eq!(code(&err), "bad_drawer");
    // Two running jobs show as two; cancelling one through editor.cancel_task reaches its token.
    let token = regia_editor::tasks::CancelToken::new();
    {
        let world = client.app.world_mut();
        let mut tasks = world.get_resource_or_insert_with(regia_editor::tasks::BackgroundTasks::default);
        for id in ["package", "clear_caches"] {
            tasks.publish(regia_editor::tasks::TaskRow {
                id: id.into(),
                label: id.into(),
                progress: None,
                cancel: token.clone(),
            });
        }
    }
    let status = client.call("editor.status_bar", json!({}));
    assert_eq!(status["jobs"]["count"], 2);
    client.call("editor.cancel_task", json!({ "id": "package" }));
    assert!(token.is_cancelled());
    let _ = std::fs::remove_dir_all(project);
}
