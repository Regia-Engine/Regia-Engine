// SPDX-License-Identifier: MPL-2.0
//! Editor tools end to end (`book/agents/editor-tools.md`, ADR-1523): a real child process,
//! launched from a manifest with a scoped token, talking over the real scoped listener.
//!
//! The tool is this test binary itself, started with `--exact tool_child`: `tool_child` does
//! nothing unless the launcher's `REGIA_TOOL_TOKEN` is in its environment. The agent side drives
//! the edit world through the real dispatcher from an in-process inbox (as the undo smoke test
//! does). Frames run until the awaited answer arrives; the only clock is a hang guard that fails
//! the test if nothing happens for minutes, never a timing assertion.

use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use regia_agent::client::{Client, ClientError};
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::tools::host::{ENDPOINT_ENV, TOKEN_ENV};
use serde_json::{Value, json};

/// Fails a test that stops making progress (a hang), long after any healthy run.
const HANG_GUARD: Duration = Duration::from_secs(180);

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d["code"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// The tool: says hello, checks that its scope refuses `scene.spawn`, sets a view, reports
/// progress, then answers polled work until it is asked to shut down.
#[test]
fn tool_child() {
    if std::env::var_os(ENDPOINT_ENV).is_none() || std::env::var_os(TOKEN_ENV).is_none() {
        return; // an ordinary test run: not launched as a tool
    }
    let (mut client, id) = Client::connect_tool_from_env().expect("the tool connects with its token");
    assert_eq!(id, "e2e");
    client
        .call("agent.hello", json!({ "client": "e2e-tool" }))
        .expect("hello is in every tool's scope");
    let scope_check = match client.call("scene.spawn", json!({ "name": "nope" })) {
        Err(ClientError::Rpc(error)) => code(&error),
        Err(other) => format!("transport: {other}"),
        Ok(_) => "allowed".to_string(),
    };
    client
        .call(
            "editor.tool_set_view",
            json!({ "items": [
                { "kind": "heading", "text": "E2E" },
                { "kind": "button", "id": "ping", "label": "Ping" },
                { "kind": "label", "text": format!("scope check: {scope_check}") },
            ] }),
        )
        .expect("view");
    client
        .call(
            "editor.tool_progress",
            json!({ "task": "boot", "fraction": 1.0, "state": "done", "message": "ready" }),
        )
        .expect("progress");
    loop {
        let Ok(polled) = client.call("editor.tool_poll", json!({ "timeout_ms": 20_000 })) else {
            return; // the token was revoked or the editor went away
        };
        for event in polled["events"].as_array().cloned().unwrap_or_default() {
            match event["kind"].as_str() {
                Some("call") => {
                    let call = event["call"].clone();
                    let answer = match event["method"].as_str() {
                        Some("echo.upper") => {
                            let text = event["params"]["text"].as_str().unwrap_or_default();
                            json!({ "call": call, "result": { "text": text.to_uppercase() } })
                        }
                        _ => {
                            json!({ "call": call, "error": { "message": "not supported", "code": "unsupported" } })
                        }
                    };
                    client.call("editor.tool_reply", answer).expect("reply");
                }
                Some("click") => {
                    client
                        .call(
                            "editor.tool_progress",
                            json!({ "task": "click", "state": "done", "message": event["item"] }),
                        )
                        .expect("progress");
                }
                Some("shutdown") => return,
                _ => {}
            }
        }
    }
}

/// An agent talking to the edit world through the dispatcher.
struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
        regia_editor::agent_ext::install(app.world_mut());
        app.add_systems(Last, regia_editor::tools::tick_tools);
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

    /// Runs frames until `done` returns a value.
    fn pump_until<T>(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> Option<T>) -> T {
        let guard = Instant::now() + HANG_GUARD;
        loop {
            self.app.update();
            if let Some(value) = done(self) {
                return value;
            }
            assert!(Instant::now() < guard, "hang guard: still waiting for {what}");
            std::thread::yield_now();
        }
    }

    /// One call; deferred methods are answered from a later frame.
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
        let response = self.pump_until(method, |_| answer.try_recv().ok());
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.try_call(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error:?}"))
    }

    fn status(&mut self) -> Value {
        self.call("editor.tool_status", json!({ "tool": "e2e", "log_lines": 200 }))
    }

    /// Frames (and status reads) until `ready` holds for the tool's status.
    fn wait_status(&mut self, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        self.pump_until(what, |agent| {
            let status = agent.status();
            ready(&status).then_some(status)
        })
    }
}

fn has_task(status: &Value, task: &str) -> bool {
    status["tasks"]
        .as_array()
        .is_some_and(|tasks| tasks.iter().any(|t| t["task"] == task && t["state"] == "done"))
}

#[test]
fn a_launched_tool_answers_proxied_calls_end_to_end() {
    let project = regia_test_utils::unique_temp_dir("tools-e2e");
    let tools = regia_core::brand::project_state_dir(&project).join("tools");
    std::fs::create_dir_all(&tools).expect("tools dir");
    let exe = std::env::current_exe().expect("test binary");
    let manifest = format!(
        r#"format = 1
id = "e2e"
name = "End to end"
description = "This test binary, as a tool."

[launch]
program = {program:?}
args = ["--exact", "tool_child", "--nocapture", "--test-threads", "1"]

[scope]
permissions = ["read"]
methods = ["agent.*"]

[[provides]]
name = "echo.upper"
description = "Upper-cases `text`."
params = [{{ name = "text", kind = "string", required = true }}]

[[provides]]
name = "echo.fail"
description = "Always answers with an error."
"#,
        program = exe.display().to_string()
    );
    let tmp = tools.join("e2e.tool.toml.tmp");
    std::fs::write(&tmp, manifest).expect("manifest");
    std::fs::rename(&tmp, tools.join("e2e.tool.toml")).expect("manifest in place");

    let mut agent = Agent::new(&project);
    let listed = agent.call("editor.tool_list", json!({ "refresh": true }));
    let entry = listed["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["id"] == "e2e"))
        .cloned()
        .expect("the manifest is found in the project");
    assert_eq!(entry["valid"], true, "{entry}");
    assert_eq!(entry["source"], "project");
    assert_eq!(entry["status"], "stopped");

    // Launching from the protocol needs the `tools` permission, which the default profile lacks.
    let refused = agent
        .try_call("editor.tool_launch", json!({ "tool": "e2e" }))
        .expect_err("needs tools");
    assert_eq!(code(&refused), "F0012");
    agent
        .app
        .world_mut()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .permissions
        .insert("tools".into());
    let launched = agent.call("editor.tool_launch", json!({ "tool": "e2e" }));
    assert_eq!(launched["status"], "running");
    assert!(
        launched["endpoint"]
            .as_str()
            .is_some_and(|e| e.starts_with("ws://127.0.0.1:"))
    );

    // The tool connects, finds scene.spawn outside its scope, sets its view and reports progress.
    let status = agent.wait_status("the tool's boot progress", |s| {
        has_task(s, "boot") && s["view"].as_array().is_some_and(|v| v.len() == 3)
    });
    assert_eq!(status["connected"], true);
    assert_eq!(status["view"][2]["text"], "scope check: tool_scope_denied");

    // A proxied call: the tool's result passes through unchanged.
    let upper = agent.call(
        "editor.tool_call",
        json!({ "tool": "e2e", "method": "echo.upper", "params": { "text": "regia" } }),
    );
    assert_eq!(upper, json!({ "text": "REGIA" }));
    // A tool error answers tool_error with the tool's own code.
    let failed = agent
        .try_call(
            "editor.tool_call",
            json!({ "tool": "e2e", "method": "echo.fail" }),
        )
        .expect_err("the tool refuses");
    assert_eq!(code(&failed), "tool_error");
    assert_eq!(
        failed.data.as_ref().map(|d| d["tool_code"].clone()),
        Some(json!("unsupported"))
    );
    // An undeclared method never reaches the tool.
    let undeclared = agent
        .try_call(
            "editor.tool_call",
            json!({ "tool": "e2e", "method": "echo.nope" }),
        )
        .expect_err("undeclared");
    assert_eq!(code(&undeclared), "tool_unknown_method");

    // A press of the view's button reaches the tool as a click event.
    agent.call("editor.tool_click", json!({ "tool": "e2e", "item": "ping" }));
    let status = agent.wait_status("the click's progress", |s| has_task(s, "click"));
    let click = status["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|t| t["task"] == "click"))
        .cloned()
        .expect("click task");
    assert_eq!(click["message"], "ping");

    // Progress is in events.read as tool_progress records.
    let events = agent.call("events.read", json!({}));
    let progress: Vec<&Value> = events["records"]
        .as_array()
        .map(|records| records.iter().filter(|r| r["kind"] == "tool_progress").collect())
        .unwrap_or_default();
    assert!(progress.len() >= 2, "{events}");
    assert!(progress.iter().all(|r| r["data"]["tool"] == "e2e"));

    // The panel method selects the tool and asks the shell to focus the tab.
    let panel = agent.call("editor.tool_panel", json!({ "tool": "e2e" }));
    assert_eq!(panel["selected"], "e2e");

    // Stop: the shutdown event ends the tool, which exits 0; its token is revoked.
    agent.call("editor.tool_stop", json!({ "tool": "e2e" }));
    let status = agent.wait_status("the tool's exit", |s| s["status"] == "exited");
    assert_eq!(status["exit_code"], 0, "{status}");
    let host = agent.app.world().resource::<regia_editor::tools::ToolHost>();
    assert!(host.tokens().is_empty(), "the token is revoked at exit");
    let after = agent
        .try_call(
            "editor.tool_call",
            json!({ "tool": "e2e", "method": "echo.upper", "params": { "text": "x" } }),
        )
        .expect_err("not running");
    assert_eq!(code(&after), "tool_not_running");

    drop(agent);
    let _ = std::fs::remove_dir_all(&project);
}
