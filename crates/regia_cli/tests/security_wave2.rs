// SPDX-License-Identifier: MPL-2.0
//! Threat-model findings 24 and 31 (ADR-3285) against the real `regia` binary.
//!
//! 24: the connection limit of `regia mcp --http` was spent before the token was checked, so 64
//! idle sockets from anyone who can reach the port kept the token holder out. The first test is the
//! threat-model reproducer, un-ignored.
//!
//! 31: the book's `cat "$(jq -r .token_file ...)"` flow trusted a planted `mcp_http.json`;
//! `regia mcp token` checks it first.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use regia_agent::endpoint;
use serde_json::Value;

/// Bounds a hang in these tests; nothing depends on it passing quickly.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// `regia mcp --http`'s pre-auth pool (`MAX_PREAUTH_CONNECTIONS` in the binary).
const MAX_PREAUTH_CONNECTIONS: usize = 128;
/// `regia mcp --http`'s admitted connections (`MAX_CONNECTIONS` in the binary).
const MAX_CONNECTIONS: usize = 64;

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

/// `regia mcp --http` with no address, so the default is the one under test.
fn start(project: &Path) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["mcp", "--project"])
        .arg(project)
        .arg("--http")
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

fn connect(addr: &str) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(HANG_GUARD)).expect("timeout");
    stream
}

/// The status and body of the first response on `stream` (status 0 when it closed first).
fn response(stream: &mut TcpStream) -> (u16, String) {
    let mut text = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(end) = regia_agent::mcp_http::head_end(&text) {
            break end;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return (0, String::from_utf8_lossy(&text).into_owned()),
            Ok(n) => text.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&text[..head_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while text.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => text.extend_from_slice(&chunk[..n]),
        }
    }
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let end = text.len().min(head_end + length);
    (status, String::from_utf8_lossy(&text[head_end..end]).into_owned())
}

/// A POST with the token and `body`.
fn post(server: &Server, body: &str) -> Vec<u8> {
    format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
        server.addr,
        server.token,
        body.len()
    )
    .into_bytes()
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"wave2","version":"1"}}}"#;

/// Finding 24: the connection limit was spent before the token was checked, so 64 idle sockets
/// kept the token holder out, for as long as they reconnect.
#[test]
fn finding_24_idle_sockets_do_not_lock_out_the_token_holder() {
    let project = regia_test_utils::unique_temp_dir("threat-mcp-lockout");
    let server = start(&project);
    let held: Vec<TcpStream> = (0..64).map(|_| connect(&server.addr)).collect();
    let mut holder = connect(&server.addr);
    holder
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}",
                server.addr, server.token
            )
            .as_bytes(),
        )
        .expect("send");
    assert_ne!(response(&mut holder).0, 503);
    drop(held);
    let _ = std::fs::remove_dir_all(&project);
}

/// A full pre-auth pool closes its oldest socket for a newcomer: the token holder still opens a
/// session with more idle sockets open than the pool holds.
#[test]
fn the_token_holder_initializes_with_the_pre_auth_pool_full() {
    let project = regia_test_utils::unique_temp_dir("wave2-mcp-preauth-full");
    let server = start(&project);
    let held: Vec<TcpStream> = (0..MAX_PREAUTH_CONNECTIONS)
        .map(|_| connect(&server.addr))
        .collect();
    let mut holder = connect(&server.addr);
    holder.write_all(&post(&server, INITIALIZE)).expect("send");
    let (status, body) = response(&mut holder);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"result\""), "{body}");
    // The oldest idle socket was closed to make room (or by its head deadline; either way it is
    // not left holding the pool).
    let mut oldest = held.into_iter().next().expect("one held");
    let mut byte = [0_u8; 1];
    assert_eq!(oldest.read(&mut byte).unwrap_or(0), 0);
    let _ = std::fs::remove_dir_all(&project);
}

/// The admitted slots stay capped: with every slot held by a keep-alive session, the next
/// admitted connection is answered `503 too_many_connections`; a connection without the token
/// gets its `401` from the pre-auth pool, never a slot.
#[test]
fn admitted_connections_are_capped_apart_from_the_pre_auth_pool() {
    let project = regia_test_utils::unique_temp_dir("wave2-mcp-admitted");
    let server = start(&project);
    let mut kept = Vec::new();
    for n in 0..MAX_CONNECTIONS {
        let mut stream = connect(&server.addr);
        stream.write_all(&post(&server, INITIALIZE)).expect("send");
        let (status, body) = response(&mut stream);
        assert_eq!(status, 200, "connection {n}: {body}");
        kept.push(stream);
    }
    let mut over = connect(&server.addr);
    over.write_all(&post(&server, INITIALIZE)).expect("send");
    let (status, body) = response(&mut over);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("too_many_connections"), "{body}");
    let mut stranger = connect(&server.addr);
    stranger
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}",
                server.addr
            )
            .as_bytes(),
        )
        .expect("send");
    assert_eq!(response(&mut stranger).0, 401);
    drop(kept);
    let _ = std::fs::remove_dir_all(&project);
}

fn regia_mcp_token(project: &Path, url: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_regia"));
    command.args(["mcp", "token", "--project"]).arg(project);
    if url {
        command.arg("--url");
    }
    command.output().expect("regia mcp token runs")
}

/// Finding 31: `regia mcp token` prints what the running server advertised.
#[test]
fn regia_mcp_token_prints_the_running_servers_token_and_url() {
    let project = regia_test_utils::unique_temp_dir("wave2-mcp-token");
    let server = start(&project);
    let out = regia_mcp_token(&project, false);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), server.token);
    let out = regia_mcp_token(&project, true);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("http://{}/mcp", server.addr)
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Finding 31: a planted advertisement (any file as the token, any host as the URL) prints
/// nothing, and the planted file's contents never reach the output.
#[test]
fn regia_mcp_token_refuses_a_planted_advertisement() {
    let project = regia_test_utils::unique_temp_dir("wave2-mcp-planted");
    let agent = endpoint::agent_dir(&project);
    std::fs::create_dir_all(&agent).expect("agent dir");
    let secret = project.join("id_ed25519");
    std::fs::write(&secret, "PRIVATE-KEY-MATERIAL-0123456789").expect("secret");
    let advert = serde_json::json!({
        "url": "http://collector.example:80/mcp",
        "token_file": secret.display().to_string(),
        "pid": 1,
        "loopback": true,
    });
    std::fs::write(agent.join("mcp_http.json"), advert.to_string()).expect("advert");
    for url in [false, true] {
        let out = regia_mcp_token(&project, url);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("PRIVATE-KEY-MATERIAL"), "{stderr}");
        assert!(stderr.contains("refusing"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(&project);
}
