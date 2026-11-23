// SPDX-License-Identifier: MPL-2.0
//! `regia mcp --http` end to end (`docs/specs/23-agent-native.md` § 2, ADR-2346): the real binary on
//! a loopback port, driven over raw HTTP/1.1. The handshake opens a session, `tools/list` answers
//! exactly the protocol schema's tools (the same list as stdio), `tools/call` reaches a fake app
//! through the session, and the refusals (token, Origin, Host, size, method, session) hold.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use regia_agent::protocol::{self, Response};
use regia_agent::{endpoint, server};
use serde_json::{Value, json};

/// A fake app that answers every protocol call with `{ echoed: method }`.
fn start_fake_app(project: &Path) {
    let token = "fake-app-token";
    let agent_dir = endpoint::agent_dir(project);
    let srv = server::start(token.into()).expect("fake app server");
    let advertised = protocol::Endpoint {
        url: format!("ws://{}", srv.addr),
        pid: std::process::id(),
        protocol: protocol::PROTOCOL_VERSION.into(),
        token_file: agent_dir.join("token").display().to_string(),
        app: "mcp-http-test".into(),
    };
    endpoint::write(&agent_dir, &advertised, token).expect("endpoint");
    let inbox = srv.inbox.clone();
    std::thread::spawn(move || {
        let _keep = srv;
        while let Ok(inbound) = inbox.recv() {
            let id = inbound.request.id.clone().unwrap_or(Value::Null);
            let method = inbound.request.method.clone();
            let _ = inbound.reply.send(Response::ok(id, json!({ "echoed": method })));
        }
    });
}

struct Server {
    child: Child,
    addr: String,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(project: &Path, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["mcp", "--project"])
        .arg(project)
        .args(["--http", "127.0.0.1:0"])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("regia mcp --http starts");
    let advert = endpoint::agent_dir(project).join("mcp_http.json");
    // Bounded wait for the advertisement the server writes after it binds.
    for _ in 0..1200 {
        if let Ok(text) = std::fs::read_to_string(&advert)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            let url = value["url"].as_str().unwrap_or_default().to_string();
            let token_file = PathBuf::from(value["token_file"].as_str().unwrap_or_default());
            let token = std::fs::read_to_string(token_file)
                .expect("token file")
                .trim()
                .to_string();
            let addr = url
                .trim_start_matches("http://")
                .trim_end_matches("/mcp")
                .to_string();
            return Server { child, addr, token };
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("regia mcp --http exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("no {} appeared", advert.display());
}

/// One raw request on a fresh connection: `(status, headers lowercased, body)`.
fn raw(addr: &str, request: &[u8]) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("timeout");
    stream.write_all(request).expect("send");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_len, length) = loop {
        let n = stream.read(&mut chunk).expect("read");
        assert!(
            n > 0,
            "connection closed before a response: {}",
            String::from_utf8_lossy(&buf)
        );
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = regia_agent::mcp_http::head_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end, length);
        }
    };
    while buf.len() < head_len + length {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "short body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_len]).to_string();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let body = String::from_utf8_lossy(&buf[head_len..head_len + length]).to_string();
    (status, headers, body)
}

fn post(
    server: &Server,
    session: Option<&str>,
    extra_headers: &str,
    body: &Value,
) -> (u16, Vec<(String, String)>, String) {
    let body = body.to_string();
    let session = session
        .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n{session}{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        server.addr,
        server.token,
        body.len()
    );
    raw(&server.addr, request.as_bytes())
}

fn reason(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .map(|v| v["error"]["reason"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

fn initialize(server: &Server) -> String {
    let (status, headers, body) = post(
        server,
        None,
        "",
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "http-test", "version": "1" } } }),
    );
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["result"]["serverInfo"]["name"], "regia-mcp", "{answer}");
    let session = headers
        .iter()
        .find(|(n, _)| n == "mcp-session-id")
        .map(|(_, v)| v.clone())
        .expect("a session id");
    let (status, _, _) = post(
        server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    assert_eq!(status, 202);
    session
}

#[test]
fn the_http_transport_serves_the_schema_tools_and_reaches_the_app() {
    let project = regia_test_utils::unique_temp_dir("mcp-http-e2e");
    start_fake_app(&project);
    let server = start(&project, &[]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let token_file = endpoint::agent_dir(&project).join("mcp_http_token");
        let mode = std::fs::metadata(token_file).expect("token").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the token is owner-only");
    }
    let session = initialize(&server);

    let (status, _, body) = post(
        &server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).expect("json");
    let mut names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    let mut expected: Vec<String> = regia_agent::schema::METHODS
        .iter()
        .filter(|m| !m.tool.is_empty())
        .map(|m| m.tool.to_string())
        .collect();
    names.sort();
    expected.sort();
    assert_eq!(names, expected, "the HTTP tool list is the schema's (as stdio's)");
    for tool in [
        "job_status",
        "run_tests",
        "open_in_editor",
        "review_propose",
        "review_accept",
    ] {
        assert!(names.iter().any(|n| n == tool), "{tool} listed");
    }

    let (status, _, body) = post(
        &server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "job_status", "arguments": {} } }),
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("agent.job_status"),
        "the call reached the app: {body}"
    );

    // A request id still in flight cannot be reused; a finished one can.
    let (status, _, _) = post(
        &server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }),
    );
    assert_eq!(status, 200);

    // Sessions: a wrong one is 404, none is 400, initialize with one is 400, DELETE ends one.
    let (status, _, body) = post(
        &server,
        Some("nope"),
        "",
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }),
    );
    assert_eq!((status, reason(&body).as_str()), (404, "session_not_found"));
    let (status, _, body) = post(
        &server,
        None,
        "",
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }),
    );
    assert_eq!((status, reason(&body).as_str()), (400, "session_required"));
    let (status, _, body) = post(
        &server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {} }),
    );
    assert_eq!((status, reason(&body).as_str()), (400, "session_on_initialize"));
    let delete = format!(
        "DELETE /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nMcp-Session-Id: {session}\r\nConnection: close\r\n\r\n",
        server.addr, server.token
    );
    assert_eq!(raw(&server.addr, delete.as_bytes()).0, 200);
    let (status, _, _) = post(
        &server,
        Some(&session),
        "",
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "ping" }),
    );
    assert_eq!(status, 404, "the ended session is gone");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn the_http_transport_refuses_what_its_policy_forbids() {
    let project = regia_test_utils::unique_temp_dir("mcp-http-refuse");
    let server = start(
        &project,
        &["--http-max-body", "2048", "--allow-origin", "https://ide.example"],
    );
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let request = |headers: &str| {
        format!(
            "POST /mcp HTTP/1.1\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let host = format!("Host: {}\r\n", server.addr);
    let auth = format!("Authorization: Bearer {}\r\n", server.token);
    let cases: Vec<(String, u16, &str)> = vec![
        (request(&host), 401, "unauthorized"),
        (
            request(&format!("{host}Authorization: Bearer wrong-token-0000\r\n")),
            401,
            "unauthorized",
        ),
        (
            request(&format!("{host}{auth}Origin: http://evil.example\r\n")),
            403,
            "origin_forbidden",
        ),
        (
            request(&format!("Host: rebound.example\r\n{auth}")),
            403,
            "host_forbidden",
        ),
        (
            format!("GET /mcp HTTP/1.1\r\n{host}{auth}Connection: close\r\n\r\n"),
            405,
            "no_event_stream",
        ),
        (
            format!("POST /other HTTP/1.1\r\n{host}{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"),
            404,
            "not_found",
        ),
        (
            format!(
                "POST /mcp HTTP/1.1\r\n{host}{auth}Content-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n"
            ),
            413,
            "body_too_large",
        ),
        (
            format!(
                "POST /mcp HTTP/1.1\r\n{host}{auth}Content-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
            415,
            "not_json",
        ),
        (
            format!(
                "POST /mcp HTTP/1.1\r\n{host}{auth}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n"
            ),
            411,
            "length_required",
        ),
    ];
    for (text, status, why) in cases {
        let (got, headers, answer) = raw(&server.addr, text.as_bytes());
        assert_eq!((got, reason(&answer).as_str()), (status, why), "{text}");
        if status == 401 {
            assert!(
                headers
                    .iter()
                    .any(|(n, v)| n == "www-authenticate" && v == "Bearer")
            );
        }
    }
    // An allowed extra origin and a batch.
    let allowed = request(&format!("{host}{auth}Origin: https://ide.example\r\n"));
    let (status, _, answer) = raw(&server.addr, allowed.as_bytes());
    assert_eq!(
        (status, reason(&answer).as_str()),
        (400, "session_required"),
        "the origin passed; the session rule spoke"
    );
    let batch = "[]";
    let text = format!(
        "POST /mcp HTTP/1.1\r\n{host}{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{batch}",
        batch.len()
    );
    let (status, _, answer) = raw(&server.addr, text.as_bytes());
    assert_eq!((status, reason(&answer).as_str()), (400, "batch_unsupported"));
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_non_loopback_bind_needs_allow_remote() {
    let project = regia_test_utils::unique_temp_dir("mcp-http-remote");
    let output = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["mcp", "--project"])
        .arg(&project)
        .args(["--http", "0.0.0.0:0"])
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--allow-remote"));
    let _ = std::fs::remove_dir_all(&project);
}
