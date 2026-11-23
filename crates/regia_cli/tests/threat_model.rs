// SPDX-License-Identifier: MPL-2.0
//! `regia mcp --http` and `regia content serve` limits named by `docs/security/threat-model.md` that
//! had no test of their own (ADR-3206): the real binary, driven over raw sockets. The harness starts
//! the server with no address, so the default one is under test everywhere.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use regia_agent::endpoint;
use serde_json::Value;

/// Bounds a hang in these tests; nothing depends on it passing quickly.
const HANG_GUARD: Duration = Duration::from_secs(120);

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

/// The status of the first response on `stream`.
fn status_of(stream: &mut TcpStream) -> u16 {
    let mut text = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !text.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => text.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&text)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn the_http_transport_listens_on_loopback_unless_told_otherwise() {
    let project = regia_test_utils::unique_temp_dir("threat-mcp-default");
    let server = start(&project);
    assert!(server.addr.starts_with("127.0.0.1:"), "{}", server.addr);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_request_head_over_the_limit_is_refused() {
    let project = regia_test_utils::unique_temp_dir("threat-mcp-head");
    let server = start(&project);
    let mut stream = connect(&server.addr);
    let filler = vec![b'a'; regia_agent::mcp_http::MAX_HEAD_BYTES + 4096];
    let mut request = b"POST /mcp HTTP/1.1\r\nX-Filler: ".to_vec();
    request.extend_from_slice(&filler);
    // The refusal may close the socket while this is being written.
    let _ = stream.write_all(&request);
    assert_eq!(status_of(&mut stream), 431);
    let _ = std::fs::remove_dir_all(&project);
}

/// The body is read only after the token: no bytes of it are sent here, and the answer still comes.
#[test]
fn a_request_without_the_token_is_answered_before_its_body_is_read() {
    let project = regia_test_utils::unique_temp_dir("threat-mcp-body");
    let server = start(&project);
    let mut stream = connect(&server.addr);
    stream
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n",
                server.addr
            )
            .as_bytes(),
        )
        .expect("send");
    assert_eq!(status_of(&mut stream), 401);
    // And the right token does wait for the body: this is the difference the test is about.
    let mut stream = connect(&server.addr);
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("timeout");
    stream
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n",
                server.addr, server.token
            )
            .as_bytes(),
        )
        .expect("send");
    assert_eq!(status_of(&mut stream), 0, "no answer before the body arrived");
    let _ = std::fs::remove_dir_all(&project);
}

/// A connection that sends nothing is closed at the read deadline (30 s): the read here ends with
/// the server closing, not with the hang guard.
#[test]
fn a_silent_connection_is_closed_at_the_read_deadline() {
    let project = regia_test_utils::unique_temp_dir("threat-mcp-silent");
    let server = start(&project);
    let mut stream = connect(&server.addr);
    let mut byte = [0_u8; 1];
    assert_eq!(stream.read(&mut byte).expect("closed, not timed out"), 0);
    let _ = std::fs::remove_dir_all(&project);
}

/// `regia content serve` with no `--bind` listens on loopback only.
#[test]
fn the_content_server_binds_loopback_unless_told_otherwise() {
    let dir = regia_test_utils::unique_temp_dir("threat-serve-default");
    let mut child = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["content", "serve"])
        .arg(&dir)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("the address line");
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.contains("127.0.0.1:"), "{line}");
    let _ = std::fs::remove_dir_all(&dir);
}
