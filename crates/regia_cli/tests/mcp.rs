// SPDX-License-Identifier: MPL-2.0
//! Integration tests for `regia mcp` and `regia agent call` against a fake protocol server
//! (`docs/specs/23-agent-native.md` § 8 M2 acceptance).
//!
//! The fake server echoes method + params; for `seeing.screenshot` it returns a valid 1×1 PNG.

use std::process::Stdio;

use regia_agent::endpoint;
use regia_agent::protocol::{self, Response};
use regia_agent::server;
use rmcp::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
    ReadResourceResult, ResourceContents,
};
use serde_json::{Value, json};
use tokio::process::Command as AsyncCommand;

/// Spawns a fake protocol server, writes endpoint.json into `project_dir`.
fn start_fake_server(project_dir: &std::path::Path, token: &str) {
    start_fake_server_with_schema(project_dir, token, None);
}

/// Like [`start_fake_server`]; with `schema`, the fake answers `agent.schema` with that table (an
/// editor's live table lists its own methods after the built-in ones).
fn start_fake_server_with_schema(project_dir: &std::path::Path, token: &str, schema: Option<Value>) {
    let agent_dir = endpoint::agent_dir(project_dir);
    let srv = server::start(token.to_string()).expect("fake server starts");
    let url = format!("ws://{}", srv.addr);
    let ep = protocol::Endpoint {
        url,
        pid: std::process::id(),
        protocol: protocol::PROTOCOL_VERSION.into(),
        token_file: agent_dir.join("token").display().to_string(),
        app: "regia-cli-test".into(),
    };
    endpoint::write(&agent_dir, &ep, token).expect("endpoint written");

    let inbox = srv.inbox.clone();
    std::thread::Builder::new()
        .name("fake-server".into())
        .spawn(move || {
            while let Ok(inbound) = inbox.recv() {
                let id = inbound.request.id.clone().unwrap_or(Value::Null);
                let method = inbound.request.method.clone();
                let params = inbound.request.params.clone();
                let result = if let (Some(table), "agent.schema") = (&schema, method.as_str()) {
                    table.clone()
                } else if method == "seeing.screenshot" {
                    // A small valid 1×1 RGBA PNG (bytes kept for documentation).
                    let _png_bytes: Vec<u8> = vec![
                        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
                        0x00, 0x00, 0x00, 0x0D, // IHDR len
                        0x49, 0x48, 0x44, 0x52, // IHDR
                        0x00, 0x00, 0x00, 0x01, // w=1
                        0x00, 0x00, 0x00, 0x01, // h=1
                        0x08, 0x02, // 8-bit RGB
                        0x00, 0x00, 0x00, // no comp/filter/interlace
                        0x90, 0x77, 0x53, 0xDE, // IHDR CRC
                        0x00, 0x00, 0x00, 0x0C, // IDAT len
                        0x49, 0x44, 0x41, 0x54, // IDAT
                        0x08, 0xD7, 0x63, 0x60, 0x60, 0x00, 0x00, 0x00,
                        0x04, 0x00, 0x01, 0x47,
                        0x2B, 0xB7, 0xD3, 0x7F, // IDAT CRC
                        0x00, 0x00, 0x00, 0x00, // IEND len
                        0x49, 0x45, 0x4E, 0x44, // IEND
                        0xAE, 0x42, 0x60, 0x82, // IEND CRC
                    ];
                    // Pre-computed base64 of the 1×1 white RGBA PNG above.
                    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12NgYAAAAAQAAUcrt9N/AAAAAElFTkSuQmCC";
                    json!({"width": 1, "height": 1, "mime": "image/png", "png_base64": b64})
                } else if method == "scene.serialize" {
                    // The regia://scene/current resource serves the `ron` field.
                    json!({"ron": "(entities: [ // fake\n])", "entities": 0, "components": 0})
                } else {
                    json!({"method": method, "echoed_params": params})
                };
                let _ = inbound.reply.send(Response::ok(id, result));
            }
        })
        .expect("fake server thread");
}

/// Minimal client handler for integration tests.
#[derive(Clone)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("regia-cli-test", "0.1.0"),
        )
    }
}

#[tokio::test]
async fn mcp_list_tools_returns_the_full_schema_surface() {
    let project_dir = std::env::temp_dir().join(format!("regia-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&project_dir).expect("temp dir");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    start_fake_server(&project_dir, token);

    let regia_bin = env!("CARGO_BIN_EXE_regia");
    let mut cmd = AsyncCommand::new(regia_bin);
    cmd.args(["mcp", "--project"])
        .arg(&project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");

    let result = running.list_tools(None).await.expect("list_tools");
    let expected: Vec<&str> = regia_agent::schema::METHODS
        .iter()
        .filter(|m| !m.tool.is_empty())
        .map(|m| m.tool)
        .collect();
    assert_eq!(
        result.tools.len(),
        expected.len(),
        "one MCP tool per protocol method (specs/23 § 3)"
    );

    let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
    for tool in [
        "hello",
        "screenshot",
        "step",
        "read_logs",
        "inject_input",
        "spawn",
        "apply_edits",
        "wait_until",
        "expect",
        "capture_sequence",
        "compare_images",
        "list_types",
        "read_events",
        "explain_error",
    ] {
        assert!(names.contains(&tool), "tool {tool} present");
    }

    for tool in &result.tools {
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{} has description",
            tool.name
        );
        assert!(
            tool.input_schema.contains_key("type"),
            "{} has schema type",
            tool.name
        );
    }

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project_dir);
}

#[tokio::test]
async fn mcp_call_tool_hello_returns_echoed_method() {
    let project_dir = std::env::temp_dir().join(format!("regia-mcp-test2-{}", std::process::id()));
    std::fs::create_dir_all(&project_dir).expect("temp dir");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    start_fake_server(&project_dir, token);

    let regia_bin = env!("CARGO_BIN_EXE_regia");
    let mut cmd = AsyncCommand::new(regia_bin);
    cmd.args(["mcp", "--project"])
        .arg(&project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");

    let result: CallToolResult = running
        .call_tool(CallToolRequestParams::new("hello"))
        .await
        .expect("call_tool hello");
    let text = result
        .content
        .first()
        .and_then(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .expect("text content");
    assert!(text.contains("agent.hello"), "echoed method: {text}");

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project_dir);
}

/// An attached editor's own methods are MCP tools too (spec 23 § 2/§ 3, ADR-5628): `regia mcp`
/// reads the app's live `agent.schema`, lists each host method that has a tool name no built-in
/// method uses, and routes a call of that tool to the host method.
#[tokio::test]
async fn mcp_lists_and_calls_the_attached_editors_methods() {
    let project_dir = regia_test_utils::unique_temp_dir("regia-mcp-host-tools");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let table = json!({
        "protocol": protocol::PROTOCOL_VERSION,
        "methods": [
            { "name": "agent.hello", "tool": "hello", "description": "built in" },
            { "name": "editor.save_level", "tool": "save_level", "description": "Save the open level.",
              "params": { "type": "object", "properties": {}, "required": [], "additionalProperties": false } },
            { "name": "editor.pause", "tool": "pause", "description": "the runtime keeps this tool name" },
        ],
    });
    start_fake_server_with_schema(&project_dir, token, Some(table));

    let regia_bin = env!("CARGO_BIN_EXE_regia");
    let mut cmd = AsyncCommand::new(regia_bin);
    cmd.args(["mcp", "--project"])
        .arg(&project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");

    let listed = running.list_tools(None).await.expect("list_tools");
    let built_in = regia_agent::schema::METHODS
        .iter()
        .filter(|m| !m.tool.is_empty())
        .count();
    assert_eq!(
        listed.tools.len(),
        built_in + 1,
        "the built-in tools plus save_level"
    );
    let names: Vec<&str> = listed.tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names.iter().filter(|n| **n == "save_level").count(), 1);
    assert_eq!(
        names.iter().filter(|n| **n == "pause").count(),
        1,
        "one pause tool"
    );

    let text_of = |result: &CallToolResult| {
        result
            .content
            .first()
            .and_then(|b| {
                if let ContentBlock::Text(t) = b {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .expect("text content")
    };
    let saved = running
        .call_tool(CallToolRequestParams::new("save_level"))
        .await
        .expect("call_tool save_level");
    assert!(
        text_of(&saved).contains("\"editor.save_level\""),
        "{}",
        text_of(&saved)
    );
    // The shared tool name still calls the runtime method.
    let paused = running
        .call_tool(CallToolRequestParams::new("pause"))
        .await
        .expect("call_tool pause");
    assert!(
        text_of(&paused).contains("\"play.pause\""),
        "{}",
        text_of(&paused)
    );
    // A name neither table knows is refused.
    assert!(
        running
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .is_err()
    );

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project_dir);
}

#[tokio::test]
async fn mcp_call_tool_screenshot_returns_image_content() {
    let project_dir = std::env::temp_dir().join(format!("regia-mcp-test3-{}", std::process::id()));
    std::fs::create_dir_all(&project_dir).expect("temp dir");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    start_fake_server(&project_dir, token);

    let regia_bin = env!("CARGO_BIN_EXE_regia");
    let mut cmd = AsyncCommand::new(regia_bin);
    cmd.args(["mcp", "--project"])
        .arg(&project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");

    let result: CallToolResult = running
        .call_tool(CallToolRequestParams::new("screenshot"))
        .await
        .expect("call_tool screenshot");
    assert_eq!(result.content.len(), 2, "image + text");

    // Verify image content block.
    let (data, mime) = result
        .content
        .iter()
        .find_map(|b| {
            if let ContentBlock::Image(img) = b {
                Some((img.data.clone(), img.mime_type.clone()))
            } else {
                None
            }
        })
        .expect("image content block");
    assert_eq!(mime, "image/png");
    assert!(!data.is_empty(), "base64 data present");

    // Verify text content block.
    let text = result
        .content
        .iter()
        .find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .expect("text content block");
    assert!(text.contains("Screenshot"), "text mentions screenshot: {text}");

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project_dir);
}

#[tokio::test]
async fn agent_call_against_fake_server() {
    let project_dir = std::env::temp_dir().join(format!("regia-mcp-test4-{}", std::process::id()));
    std::fs::create_dir_all(&project_dir).expect("temp dir");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    start_fake_server(&project_dir, token);

    let regia_bin = env!("CARGO_BIN_EXE_regia");

    // agent.hello
    let output = AsyncCommand::new(regia_bin)
        .args(["agent", "call", "agent.hello", r#"{"name":"test"}"#, "--project"])
        .arg(&project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .expect("agent call runs");
    assert!(
        output.status.success(),
        "exit ok, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agent.hello"), "echoed method: {stdout}");
    assert!(stdout.contains("echoed_params"), "echoed params: {stdout}");

    // seeing.screenshot with --out
    let out_path = project_dir.join("shot.png");
    let output = AsyncCommand::new(regia_bin)
        .args(["agent", "call", "seeing.screenshot", "--project"])
        .arg(&project_dir)
        .arg("--out")
        .arg(&out_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .expect("agent screenshot runs");
    assert!(
        output.status.success(),
        "exit ok, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1x1"), "has dimensions: {stdout}");
    assert!(out_path.is_file(), "PNG written");
    let png_bytes = std::fs::read(&out_path).expect("read PNG");
    assert!(&png_bytes[..4] == b"\x89PNG", "PNG signature");

    let _ = std::fs::remove_dir_all(&project_dir);
}

#[tokio::test]
async fn mcp_resources_list_and_read_forward_to_the_protocol() {
    let project_dir = std::env::temp_dir().join(format!("regia-mcp-test5-{}", std::process::id()));
    std::fs::create_dir_all(&project_dir).expect("temp dir");
    let token = "test-token-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    start_fake_server(&project_dir, token);

    let regia_bin = env!("CARGO_BIN_EXE_regia");
    let mut cmd = AsyncCommand::new(regia_bin);
    cmd.args(["mcp", "--project"])
        .arg(&project_dir)
        .arg("--attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd).expect("child transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");

    // Static resources (specs/23 § 3.8).
    let listed = running.list_resources(None).await.expect("resources/list");
    let uris: Vec<String> = listed.resources.iter().map(|r| r.uri.clone()).collect();
    for expected in [
        "regia://project",
        "regia://scene/current",
        "regia://logs/recent",
        "regia://protocol/schema.json",
    ] {
        assert!(uris.contains(&expected.to_string()), "resource {expected} listed");
    }

    // Reading forwards to the protocol method; the fake server serves a stub RON payload
    // for scene.serialize, and the resource hands back its `ron` field as text.
    let read: ReadResourceResult = running
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "regia://scene/current",
        ))
        .await
        .expect("scene/current reads");
    let text = match read.contents.first() {
        Some(ResourceContents::TextResourceContents { text, .. }) => text.clone(),
        other => panic!("expected text contents, got {other:?}"),
    };
    assert!(text.contains("entities"), "the scene RON is served: {text}");

    // Templated resources carry their argument into the matching param.
    let read: ReadResourceResult = running
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "regia://types/regia_scene::Transform",
        ))
        .await
        .expect("types resource reads");
    let text = match read.contents.first() {
        Some(ResourceContents::TextResourceContents { text, .. }) => text.clone(),
        other => panic!("expected text contents, got {other:?}"),
    };
    assert!(
        text.contains("regia_scene::Transform"),
        "the path argument reached types.describe: {text}"
    );

    // Unknown resources answer with a protocol error, not a panic.
    let missing = running
        .read_resource(rmcp::model::ReadResourceRequestParams::new("regia://nope/x"))
        .await;
    assert!(missing.is_err(), "unknown resource is an error");

    running.cancel().await.expect("client closes");
    let _ = std::fs::remove_dir_all(&project_dir);
}
