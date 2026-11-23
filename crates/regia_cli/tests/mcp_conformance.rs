// SPDX-License-Identifier: MPL-2.0
//! MCP conformance of `regia mcp` over stdio, driven by `rmcp`'s own client
//! (`docs/specs/23-agent-native.md` § 8, `docs/06-testing-and-ci.md` § 5 `agent-protocol`).
//!
//! Where `tests/mcp.rs` checks that the bridge forwards calls, this file checks what any MCP client
//! may rely on:
//!
//! - the `initialize` handshake: protocol version, server identity, advertised capabilities;
//! - `tools/list` is the protocol schema's method set, with an object input schema each;
//! - `tools/call` round trips, and the error mapping: an unknown tool is a JSON-RPC
//!   `method not found`, a protocol error is a tool error that carries the engine's code and fix
//!   hint, a missing app is a tool error that says how to start one;
//! - `resources/list`, `resources/templates/list` and `resources/read` of
//!   `regia://protocol/schema.json`, `regia://scene/current` and `regia://guide/{topic}`, and the
//!   errors of unknown or empty resource names;
//! - cancellation: a cancelled request does not wedge the server.
//!
//! Stdio only: rmcp's HTTP client needs `reqwest`, which is not approved
//! (`book/agents/agent-benchmark.md`). The HTTP transport is covered by `tests/mcp_http.rs`.
//!
//! The app behind the bridge is a fake protocol server, as in `tests/mcp.rs`. No test sleeps or
//! reads a clock: the slow call of the cancellation test waits on a channel the test releases.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::mpsc;

use regia_agent::endpoint;
use regia_agent::protocol::{self, Response, RpcError};
use regia_agent::server;
use regia_test_utils::unique_temp_dir;
use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ClientRequest,
    ContentBlock, ErrorCode, Implementation, ProtocolVersion, ReadResourceRequestParams, ReadResourceResult,
    ResourceContents,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::{RoleClient, serve_client};
use serde_json::{Value, json};
use tokio::process::Command as AsyncCommand;

const TOKEN: &str = "conformance-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

/// What the fake app does with a request, for the methods the tests care about.
fn answer(method: &str, params: &Value, hold: &Mutex<mpsc::Receiver<()>>) -> Result<Value, RpcError> {
    match method {
        "agent.schema" => {
            let methods: Vec<Value> = regia_agent::schema::METHODS
                .iter()
                .map(|m| json!({ "name": m.name, "tool": m.tool }))
                .collect();
            Ok(json!({ "protocol": protocol::PROTOCOL_VERSION, "methods": methods }))
        }
        "scene.serialize" => {
            Ok(json!({ "ron": "(entities: [\n    // fake scene\n])", "entities": 0, "components": 0 }))
        }
        "agent.guide" => Ok(json!({ "topic": params["topic"], "markdown": "# Guide\nfake guide page" })),
        "play.step" => match params.get("frames") {
            Some(frames) if frames.is_u64() => Ok(json!({ "frame": frames })),
            _ => Err(RpcError::invalid_params("frames must be a whole number")),
        },
        "play.wait_until" => {
            // Held until the test releases it, so the call is in flight while the client cancels.
            let release = hold.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = release.recv();
            Ok(json!({ "satisfied": true }))
        }
        other => Ok(json!({ "method": other, "echoed_params": params })),
    }
}

/// A fake protocol server advertising under `project_dir`; returns the sender that releases a held
/// `play.wait_until`.
fn start_fake_app(project_dir: &Path) -> mpsc::Sender<()> {
    let agent_dir = endpoint::agent_dir(project_dir);
    let srv = server::start(TOKEN.to_string()).expect("fake server starts");
    let ep = protocol::Endpoint {
        url: format!("ws://{}", srv.addr),
        pid: std::process::id(),
        protocol: protocol::PROTOCOL_VERSION.into(),
        token_file: agent_dir.join("token").display().to_string(),
        app: "regia-cli-conformance".into(),
    };
    endpoint::write(&agent_dir, &ep, TOKEN).expect("endpoint written");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let hold = Mutex::new(release_rx);
    let inbox = srv.inbox.clone();
    std::thread::Builder::new()
        .name("fake-app".into())
        .spawn(move || {
            // Keeps the server (and its listener) alive as long as the loop runs.
            let _srv = srv;
            while let Ok(inbound) = inbox.recv() {
                let id = inbound.request.id.clone().unwrap_or(Value::Null);
                let reply = match answer(&inbound.request.method, &inbound.request.params, &hold) {
                    Ok(result) => Response::ok(id, result),
                    Err(error) => Response::err(id, error),
                };
                let _ = inbound.reply.send(reply);
            }
        })
        .expect("fake app thread");
    release_tx
}

/// The client side: identifies itself and nothing more.
#[derive(Clone)]
struct Client;

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("regia-mcp-conformance", "0.1.0"),
        )
    }
}

type Running = RunningService<RoleClient, Client>;

/// Spawns `regia mcp --attach` on `project_dir` and completes the handshake.
async fn connect(project_dir: &Path) -> Running {
    let mut cmd = AsyncCommand::new(env!("CARGO_BIN_EXE_regia"));
    cmd.args(["mcp", "--project"])
        .arg(project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    serve_client(Client, transport)
        .await
        .expect("the handshake completes")
}

/// The text blocks of a tool result, joined.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of a text resource.
fn resource_text(read: &ReadResourceResult) -> String {
    match read.contents.first() {
        Some(ResourceContents::TextResourceContents { text, .. }) => text.clone(),
        other => panic!("expected text contents, got {other:?}"),
    }
}

/// A project directory with a running fake app, and the client connected to it.
async fn session(tag: &str) -> (PathBuf, mpsc::Sender<()>, Running) {
    let project = unique_temp_dir(tag);
    let release = start_fake_app(&project);
    let running = connect(&project).await;
    (project, release, running)
}

#[tokio::test]
async fn the_handshake_names_the_server_and_its_capabilities() {
    let (project, _release, running) = session("mcp-conf-init").await;
    let info = running.peer_info().expect("the server answered initialize");

    let server = info.server_info.as_ref().expect("the server names itself");
    assert_eq!(server.name, "regia-mcp");
    assert!(!server.version.is_empty(), "the server reports its version");
    assert!(
        info.protocol_version == ProtocolVersion::LATEST
            || info.protocol_version == ProtocolVersion::LATEST_WITH_INITIALIZE,
        "the negotiated protocol version is one this SDK speaks: {:?}",
        info.protocol_version
    );
    assert!(info.capabilities.tools.is_some(), "tools capability advertised");
    assert!(
        info.capabilities.resources.is_some(),
        "resources capability advertised"
    );
    assert!(info.capabilities.prompts.is_none(), "no prompts are offered");
    assert!(
        info.instructions
            .as_deref()
            .is_some_and(|i| i.contains("agent.hello")),
        "the instructions point at agent.hello"
    );

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn tools_list_is_the_protocol_schema() {
    let (project, _release, running) = session("mcp-conf-tools").await;
    let tools = running.list_all_tools().await.expect("tools/list");

    let mut listed: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    let mut expected: Vec<String> = regia_agent::schema::METHODS
        .iter()
        .filter(|m| !m.tool.is_empty())
        .map(|m| m.tool.to_string())
        .collect();
    listed.sort();
    expected.sort();
    assert_eq!(listed, expected, "one tool per schema method that names a tool");
    let unique: std::collections::BTreeSet<&String> = listed.iter().collect();
    assert_eq!(unique.len(), listed.len(), "tool names are unique");

    for tool in &tools {
        assert_eq!(
            tool.input_schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{} takes an object",
            tool.name
        );
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{} has a description",
            tool.name
        );
    }

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn tool_calls_round_trip_and_errors_map() {
    let (project, _release, running) = session("mcp-conf-call").await;

    // A call that works: the result is JSON text and not an error.
    let ok = running
        .call_tool(
            CallToolRequestParams::new("step")
                .with_arguments(json!({ "frames": 3 }).as_object().cloned().expect("object")),
        )
        .await
        .expect("tools/call step");
    assert_ne!(ok.is_error, Some(true));
    let body: Value = serde_json::from_str(&text_of(&ok)).expect("the text is the JSON result");
    assert_eq!(body["frame"], 3);

    // Bad parameters: the app's protocol error becomes a tool error that keeps its code and hint.
    let bad = running
        .call_tool(
            CallToolRequestParams::new("step")
                .with_arguments(json!({ "frames": "many" }).as_object().cloned().expect("object")),
        )
        .await
        .expect("a protocol error is a tool result, not a transport error");
    assert_eq!(bad.is_error, Some(true), "bad params flag the result as an error");
    let text = text_of(&bad);
    assert!(text.contains("frames must be a whole number"), "{text}");
    assert!(
        text.contains("[F0002]"),
        "the engine's error code survives: {text}"
    );
    assert!(
        text.contains("check the method's parameters"),
        "the fix hint survives: {text}"
    );

    // An unknown tool is a JSON-RPC method-not-found error.
    let unknown = running
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await;
    match unknown {
        Err(ServiceError::McpError(error)) => {
            assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND, "{error:?}");
        }
        other => panic!("expected a method-not-found error, got {other:?}"),
    }

    // The server stays usable after both errors.
    let again = running
        .call_tool(CallToolRequestParams::new("hello"))
        .await
        .expect("hello");
    assert!(text_of(&again).contains("agent.hello"));

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_missing_app_is_a_tool_error_that_says_how_to_start_one() {
    // No fake app: the project has no endpoint.
    let project = unique_temp_dir("mcp-conf-noapp");
    let running = connect(&project).await;

    // The protocol surface does not need the app.
    assert!(!running.list_all_tools().await.expect("tools/list").is_empty());

    let result = running
        .call_tool(CallToolRequestParams::new("hello"))
        .await
        .expect("a missing app is a tool result");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains("no running Regia Engine app"), "{text}");
    assert!(text.contains("cargo run"), "it says how to start one: {text}");

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn resources_list_and_read() {
    let (project, _release, running) = session("mcp-conf-res").await;

    let resources = running.list_all_resources().await.expect("resources/list");
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    for uri in [
        "regia://project",
        "regia://scene/current",
        "regia://logs/recent",
        "regia://protocol/schema.json",
    ] {
        assert!(uris.contains(&uri), "{uri} listed: {uris:?}");
    }
    for resource in &resources {
        assert!(
            resource.uri.starts_with("regia://"),
            "{} is under the regia:// scheme",
            resource.uri
        );
    }

    let templates = running
        .list_resource_templates(None)
        .await
        .expect("resources/templates/list");
    let patterns: Vec<&str> = templates
        .resource_templates
        .iter()
        .map(|t| t.uri_template.as_str())
        .collect();
    for pattern in [
        "regia://types/{path}",
        "regia://assets/{id}",
        "regia://guide/{topic}",
    ] {
        assert!(patterns.contains(&pattern), "{pattern} listed: {patterns:?}");
    }

    // The schema resource lists the same methods as the tools do.
    let schema = running
        .read_resource(ReadResourceRequestParams::new("regia://protocol/schema.json"))
        .await
        .expect("schema resource reads");
    let schema: Value = serde_json::from_str(&resource_text(&schema)).expect("the schema resource is JSON");
    let mut served: Vec<&str> = schema["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    let mut expected: Vec<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();
    served.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        served, expected,
        "regia://protocol/schema.json carries every schema method"
    );

    // The scene resource serves the RON text itself, not a JSON wrapper.
    let scene = running
        .read_resource(ReadResourceRequestParams::new("regia://scene/current"))
        .await
        .expect("scene resource reads");
    let scene_text = resource_text(&scene);
    assert!(scene_text.starts_with("(entities:"), "{scene_text}");
    assert!(
        matches!(scene.contents.first(), Some(ResourceContents::TextResourceContents { uri, .. }) if uri == "regia://scene/current"),
        "the contents name the URI that was read"
    );

    // A guide topic reaches `agent.guide` with the topic as its parameter.
    let guide = running
        .read_resource(ReadResourceRequestParams::new("regia://guide/testing"))
        .await
        .expect("guide resource reads");
    assert!(
        resource_text(&guide).contains("testing"),
        "the topic reached agent.guide"
    );

    // Unknown names and templates with no argument are errors, not empty answers.
    for uri in [
        "regia://nope",
        "regia://guide/",
        "regia://types/",
        "other://scene/current",
    ] {
        let missing = running.read_resource(ReadResourceRequestParams::new(uri)).await;
        assert!(missing.is_err(), "{uri} is an error");
    }

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

/// Spec 23 § 3.8: `regia://project`, `regia://logs/recent` and `regia://assets/{id}` are read
/// through the protocol end to end, each by its method, with the id as the `asset` parameter
/// (ADR-5628; the other four resources are read in `resources_list_and_read`).
#[tokio::test]
async fn project_logs_and_asset_resources_read_through_their_methods() {
    let (project, _release, running) = session("mcp-conf-res-rest").await;
    for (uri, method) in [
        ("regia://project", "project.info"),
        ("regia://logs/recent", "logs.read"),
        ("regia://assets/meshes/rock.glb", "assets.describe"),
    ] {
        let read = running
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .unwrap_or_else(|e| panic!("{uri} reads: {e}"));
        assert!(
            matches!(read.contents.first(), Some(ResourceContents::TextResourceContents { uri: named, .. }) if named == uri),
            "the contents name {uri}"
        );
        // The fake app echoes the method and params it received; the resource serves that JSON.
        let served: Value = serde_json::from_str(&resource_text(&read)).expect("a JSON resource");
        assert_eq!(served["method"], method, "{uri}: {served}");
        if method == "assets.describe" {
            assert_eq!(served["echoed_params"]["asset"], "meshes/rock.glb", "{served}");
        }
    }
    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}

#[tokio::test]
async fn a_cancelled_request_does_not_wedge_the_server() {
    let (project, release, running) = session("mcp-conf-cancel").await;

    // `wait_until` stays in the app until the test releases it.
    let handle = running
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new("wait_until"))),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("the request is sent");
    handle
        .cancel(Some("the conformance test gave up".to_string()))
        .await
        .expect("notifications/cancelled is sent");

    // Let the app answer the abandoned call, then show the server still serves new ones.
    release.send(()).expect("release the held call");
    let after = running
        .call_tool(CallToolRequestParams::new("hello"))
        .await
        .expect("the server answers after a cancellation");
    assert_ne!(after.is_error, Some(true));
    assert!(text_of(&after).contains("agent.hello"));

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project);
}
