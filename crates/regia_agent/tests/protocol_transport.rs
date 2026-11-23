// SPDX-License-Identifier: MPL-2.0
//! Transport-level behavior of the agent protocol that the unit tests do not reach: the handshake's
//! `Origin` refusal, the connection cap, the client's read timeout and late-answer skipping, and the
//! endpoint file's error paths. Everything uses the public API only and a loopback server on an
//! ephemeral port; no test waits on wall-clock time for synchronization.

use std::net::TcpStream;
use std::time::Duration;

use regia_agent::client::{Client, ClientError};
use regia_agent::endpoint;
use regia_agent::protocol::{Endpoint, Response};
use regia_agent::server::{self, MAX_CONNECTIONS};
use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;

/// Opens a raw WebSocket with the given extra headers, returning the handshake result.
fn raw_connect(
    url: &str,
    token: &str,
    origin: Option<&str>,
) -> Result<tungstenite::WebSocket<TcpStream>, tungstenite::Error> {
    let mut request = url.into_client_request()?;
    let auth: HeaderValue = format!("Bearer {token}").parse().expect("header value");
    request.headers_mut().insert("authorization", auth);
    if let Some(origin) = origin {
        let origin: HeaderValue = origin.parse().expect("header value");
        request.headers_mut().insert("origin", origin);
    }
    let host = request.uri().host().unwrap_or("127.0.0.1").to_string();
    let port = request.uri().port_u16().unwrap_or(80);
    let stream = TcpStream::connect((host.as_str(), port)).map_err(tungstenite::Error::Io)?;
    tungstenite::client(request, stream)
        .map(|(socket, _)| socket)
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => e,
            tungstenite::HandshakeError::Interrupted(_) => tungstenite::Error::ConnectionClosed,
        })
}

#[test]
fn handshake_with_an_origin_header_is_refused() {
    let server = server::start("secret".into()).expect("server starts");
    let url = format!("ws://{}", server.addr);
    assert!(
        raw_connect(&url, "secret", Some("http://evil.test")).is_err(),
        "a browser origin must not complete the handshake even with the token"
    );
    assert!(
        raw_connect(&url, "secret", None).is_ok(),
        "the same token without an Origin completes it"
    );
}

#[test]
fn a_wrong_or_missing_token_is_refused() {
    let server = server::start("secret".into()).expect("server starts");
    let url = format!("ws://{}", server.addr);
    assert!(raw_connect(&url, "secre", None).is_err(), "prefix of the token");
    assert!(raw_connect(&url, "secrets", None).is_err(), "token plus a suffix");
    assert!(raw_connect(&url, "", None).is_err(), "empty token");
    // No Authorization header at all.
    let request = url.as_str().into_client_request().expect("request");
    let stream = TcpStream::connect(server.addr).expect("tcp");
    assert!(tungstenite::client(request, stream).is_err());
}

#[test]
fn connections_over_the_cap_are_refused() {
    let server = server::start("secret".into()).expect("server starts");
    let url = format!("ws://{}", server.addr);
    // Each completed handshake holds its slot until the socket drops.
    let held: Vec<_> = (0..MAX_CONNECTIONS)
        .map(|i| raw_connect(&url, "secret", None).unwrap_or_else(|e| panic!("connection {i}: {e}")))
        .collect();
    assert!(
        raw_connect(&url, "secret", None).is_err(),
        "connection {} must be refused",
        MAX_CONNECTIONS + 1
    );
    drop(held);
}

#[test]
fn client_timeout_skips_the_late_reply_and_the_next_call_works() {
    let server = server::start("secret".into()).expect("server starts");
    let url = format!("ws://{}", server.addr);
    let mut client = Client::connect(&url, "secret").expect("connects");
    client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout set");

    // Nobody answers the first call: it times out instead of blocking.
    let first = client.call("agent.first", json!({}));
    assert!(matches!(first, Err(ClientError::TimedOut)), "{first:?}");
    let stalled = server
        .inbox
        .recv_timeout(Duration::from_secs(10))
        .expect("first request reached the world");
    assert_eq!(stalled.request.method, "agent.first");
    let stalled_id = stalled.request.id.clone().expect("request id");

    // The world answers the first call late. The connection thread serves one request at a time,
    // so this answer goes out on the socket before the second request is read; Client::call
    // must skip the stale answer by id and return only the second result.
    stalled
        .reply
        .send(Response::ok(stalled_id, json!({ "which": "late" })))
        .expect("the connection still waits for the first answer");
    let inbox = server.inbox.clone();
    let answering = std::thread::spawn(move || {
        let second = inbox
            .recv_timeout(Duration::from_secs(10))
            .expect("second request reached the world");
        let id = second.request.id.clone().expect("request id");
        let _ = second.reply.send(Response::ok(id, json!({ "which": "second" })));
    });
    client.set_read_timeout(None).expect("timeout cleared");
    let second = client
        .call("agent.second", json!({}))
        .expect("second call answered");
    assert_eq!(second["which"], "second");
    answering.join().expect("answering thread");
}

#[test]
fn client_reports_a_closed_server_and_bad_tokens() {
    // A token that cannot be an HTTP header value is rejected before any connection is made.
    let err = Client::connect("ws://127.0.0.1:9", "bad\ntoken");
    assert!(
        matches!(err, Err(ClientError::Connection(_))),
        "{}",
        describe(&err)
    );
    // A URL that is not a WebSocket URL.
    let err = Client::connect("not a url", "t");
    assert!(
        matches!(err, Err(ClientError::Connection(_))),
        "{}",
        describe(&err)
    );
}

fn describe(result: &Result<Client, ClientError>) -> String {
    match result {
        Ok(_) => "connected".into(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn connect_project_without_an_endpoint_is_no_endpoint() {
    let dir = regia_test_utils::unique_temp_dir("agent-no-endpoint");
    let err = Client::connect_project(&dir);
    assert!(
        matches!(err, Err(ClientError::NoEndpoint { .. })),
        "{}",
        describe(&err)
    );
    if let Err(ClientError::NoEndpoint { path, .. }) = err {
        assert!(
            path.contains(".regia"),
            "the message names the searched dir: {path}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn endpoint_read_error_paths() {
    let dir = regia_test_utils::unique_temp_dir("agent-endpoint-errors");
    // No endpoint.json.
    assert!(endpoint::read(&dir).is_err());
    // Malformed endpoint.json.
    std::fs::write(dir.join("endpoint.json"), b"{ not json").expect("write");
    assert!(endpoint::read(&dir).is_err());
    // A valid endpoint.json whose token file is missing (the client reads `<dir>/token` only;
    // `token_file` is informational, ADR-346).
    let missing = Endpoint {
        url: "ws://127.0.0.1:1".into(),
        pid: 1,
        protocol: "0.2".into(),
        token_file: dir.join("token").display().to_string(),
        app: "test".into(),
    };
    std::fs::write(
        dir.join("endpoint.json"),
        serde_json::to_vec(&missing).expect("serialize"),
    )
    .expect("write");
    assert!(endpoint::read(&dir).is_err());
    // Once the token exists, surrounding whitespace (a trailing newline) is trimmed.
    std::fs::write(dir.join("token"), b"  abc123\n").expect("write");
    let (back, token) = endpoint::read(&dir).expect("reads");
    assert_eq!(back, missing);
    assert_eq!(token, "abc123");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn endpoint_write_replaces_a_previous_advertisement_and_remove_is_idempotent() {
    let dir = regia_test_utils::unique_temp_dir("agent-endpoint-replace");
    let mut endpoint_a = Endpoint {
        url: "ws://127.0.0.1:1".into(),
        pid: 1,
        protocol: "0.2".into(),
        token_file: dir.join("token").display().to_string(),
        app: "a".into(),
    };
    endpoint::write(&dir, &endpoint_a, "tok-a").expect("first write");
    endpoint_a.url = "ws://127.0.0.1:2".into();
    endpoint_a.app = "b".into();
    endpoint::write(&dir, &endpoint_a, "tok-b").expect("second write replaces");
    let (back, token) = endpoint::read(&dir).expect("reads");
    assert_eq!(back, endpoint_a);
    assert_eq!(
        token, "tok-b",
        "a shorter or equal token must not leave stale bytes"
    );
    assert!(!dir.join("endpoint.json.tmp").exists(), "temp file renamed away");
    endpoint::remove(&dir);
    endpoint::remove(&dir); // removing twice is fine
    assert!(endpoint::read(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_tokens_are_64_hex_chars_and_distinct() {
    let a = endpoint::new_token();
    let b = endpoint::new_token();
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    assert_ne!(a, b);
}

// ## Local transport (spec 23 § 2)
//
// A Unix domain socket (Linux/macOS) or a Windows named pipe, alongside the WebSocket listener:
// the same JSON-RPC messages over length-prefixed frames (book/agents/local-transport.md,
// ADR-5708). The address is advertised in `.regia/agent/socket`, next to `endpoint.json`.

/// Calls `method` and answers it from a fake world on a thread (the connection thread waits
/// for the world's answer, so the answer must run concurrently with `call`).
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn call_answered_by_a_thread(
    inbox: &crossbeam_channel::Receiver<regia_agent::server::Inbound>,
    client: &mut Client,
    method: &str,
    transport: &'static str,
) -> serde_json::Value {
    let inbox = inbox.clone();
    let expected = method.to_string();
    let answering = std::thread::spawn(move || {
        let inbound = inbox
            .recv_timeout(Duration::from_secs(30))
            .expect("the request reached the world");
        assert_eq!(inbound.request.method, expected);
        let id = inbound.request.id.clone().expect("request id");
        inbound
            .reply
            .send(Response::ok(id, json!({ "transport": transport })))
            .expect("the connection still waits for the answer");
    });
    let result = client.call(method, json!({})).expect("answered");
    answering.join().expect("the fake world");
    result
}

/// spec 23 § 2: the local socket answers a call end to end, carrying the same JSON-RPC text
/// messages as the WebSocket path.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn spec23_sec2_local_socket_round_trips_a_call_end_to_end() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists on Linux/macOS/Windows");
    #[cfg(not(windows))]
    assert!(
        server.address.ends_with("agent.sock"),
        "the socket lives in the agent dir: {}",
        server.address
    );
    let mut client = Client::connect_local(&server.address, "secret").expect("connects");
    let result = call_answered_by_a_thread(&server.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    drop(client);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// spec 23 § 2: a wrong or truncated bearer token is refused during the handshake, before any
/// JSON-RPC message is accepted. The server acknowledges every handshake (ADR-5878), so the
/// refusal surfaces at `connect_local` itself, where `connect_project` can still fall back to
/// the WebSocket URL (sprint-rev-kimi-9 finding 3).
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn spec23_sec2_local_socket_refuses_a_wrong_bearer_token() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-token");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists on Linux/macOS/Windows");
    for wrong in ["secre", "secrets", ""] {
        let Err(err) = Client::connect_local(&server.address, wrong) else {
            panic!("{wrong:?}: connect_local must fail on a refused handshake");
        };
        assert!(matches!(err, ClientError::Connection(_)), "{wrong:?}: {err}");
    }
    // The right token still connects after the refusals.
    let mut client = Client::connect_local(&server.address, "secret").expect("connects");
    let result = call_answered_by_a_thread(&server.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    assert!(
        server.inbox.try_recv().is_err(),
        "no refused connection queued a request"
    );
    drop(client);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// sprint-rev-kimi-9 finding 2: `Client::set_read_timeout` works on the local transport of
/// every platform (a Windows pipe used to refuse it), and a call the app never answers ends
/// with `ClientError::TimedOut` instead of blocking. Two servers in one process get distinct
/// addresses where the platform allows it (Windows pipe names carry a serial).
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn spec23_sec2_local_client_read_timeout_fires() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-timeout");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists on Linux/macOS/Windows");
    let mut client = Client::connect_local(&server.address, "secret").expect("connects");
    client
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("the local transport takes a read timeout");
    // Nobody drains the inbox: the request waits in it, unanswered.
    let err = client.call("app.ping", json!({})).expect_err("no answer");
    assert!(matches!(err, ClientError::TimedOut), "{err}");
    assert!(
        server.inbox.recv_timeout(Duration::from_secs(30)).is_ok(),
        "the request reached the world's inbox"
    );
    client.set_read_timeout(None).expect("cleared");
    #[cfg(windows)]
    {
        let second_dir = regia_test_utils::unique_temp_dir("agent-local-transport-timeout-2");
        let second = regia_agent::local::start("other".into(), &second_dir, None)
            .expect("a second pipe in this process")
            .expect("a local transport exists");
        assert_ne!(second.address, server.address, "one pipe name per server");
    }
    drop(client);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// sprint-rev-kimi-9 finding 3: a live local listener holding another token (a second app in
/// the project, or an older WebSocket-only build started after a newer one) must not lock
/// clients out: `connect_project` falls back to the WebSocket URL the endpoint advertises.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn spec23_sec2_connect_project_falls_back_when_the_local_listener_refuses_the_token() {
    let project = regia_test_utils::unique_temp_dir("agent-local-transport-fallback");
    let dir = endpoint::agent_dir(&project);
    let other_app = regia_agent::local::start("other-token".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let ws = server::start("secret".into()).expect("server starts");
    let advertised = Endpoint {
        url: format!("ws://{}", ws.addr),
        pid: std::process::id(),
        protocol: "0.2".into(),
        token_file: dir.join("token").display().to_string(),
        app: "test".into(),
    };
    endpoint::write(&dir, &advertised, "secret").expect("endpoint written");
    endpoint::write_socket(&dir, &other_app.address).expect("socket advertised");
    let mut client = Client::connect_project(&project).expect("falls back to the WebSocket URL");
    let result = call_answered_by_a_thread(&ws.inbox, &mut client, "app.ping", "ws");
    assert_eq!(result["transport"], "ws");
    assert!(
        other_app.inbox.try_recv().is_err(),
        "nothing reached the other app"
    );
    drop(client);
    drop(other_app);
    let _ = std::fs::remove_dir_all(&project);
}

/// sprint-rev-kimi-9 finding 6: a second app in the same project never unlinks the first
/// app's live socket; its start fails (`AddrInUse`) and the first app keeps serving.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_a_second_server_never_unlinks_a_live_socket() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-live");
    let first = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let err = regia_agent::local::start("second".into(), &dir, None)
        .expect_err("a live socket is never taken over");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    let mut client = Client::connect_local(&first.address, "secret").expect("the first app still listens");
    let result = call_answered_by_a_thread(&first.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    drop(client);
    drop(first);
    let _ = std::fs::remove_dir_all(&dir);
}

/// spec 23 § 2: a dead socket file a crashed run left behind (nothing listens on it) is
/// still replaced; the live-socket probe of finding 6 must not lock a restarted app out.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_a_dead_socket_file_is_replaced() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-dead");
    let crashed = dir.join("agent.sock");
    drop(std::os::unix::net::UnixListener::bind(&crashed).expect("a crashed run's socket"));
    assert!(crashed.exists(), "the socket file outlives its listener");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("a dead socket is replaced")
        .expect("a local transport exists");
    let mut client = Client::connect_local(&server.address, "secret").expect("connects");
    let result = call_answered_by_a_thread(&server.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    drop(client);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// sprint-rev-kimi-9 finding 6: a server's cleanup removes only its own socket. When the path
/// already holds a successor's socket (the first one was unlinked and the path re-bound),
/// dropping the first server leaves the successor's socket in place and reachable.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_dropping_a_server_keeps_a_successors_socket() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-successor");
    let first = regia_agent::local::start("first".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    std::fs::remove_file(&first.address).expect("the first socket is unlinked");
    let successor = regia_agent::local::start("successor".into(), &dir, None)
        .expect("the path is free again")
        .expect("a local transport exists");
    assert_eq!(first.address, successor.address, "one path per agent dir");
    drop(first);
    assert!(
        std::path::Path::new(&successor.address).exists(),
        "the first server's drop removed the successor's socket"
    );
    let mut client = Client::connect_local(&successor.address, "successor").expect("successor reachable");
    let result = call_answered_by_a_thread(&successor.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    drop(client);
    drop(successor);
    let _ = std::fs::remove_dir_all(&dir);
}

/// sprint-rev-kimi-9 finding 6: exit cleanup (`endpoint::remove`) removes the `socket`
/// advertisement only when this process serves the address it names; another app's
/// advertisement stays.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_endpoint_remove_keeps_another_apps_socket_advertisement() {
    // A short tag: the other app binds `<dir>/agent.sock` itself, and macOS's `sun_path` takes 104
    // bytes, which the longer tag under its 49-byte `$TMPDIR` passed (ADR-7335).
    let dir = regia_test_utils::unique_temp_dir("advert");
    // Another app's listener: bound outside `local::start`, so this process does not serve it.
    let theirs = dir.join("agent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&theirs).expect("another app binds");
    endpoint::write_socket(&dir, theirs.to_str().expect("utf-8")).expect("advertised");
    endpoint::remove(&dir);
    assert_eq!(
        endpoint::read_socket(&dir).expect("reads").as_deref(),
        theirs.to_str(),
        "another app's advertisement was removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// sprint-rev-kimi-9 finding 7: the pre-auth handshake frame is capped far below the
/// post-auth message cap. A handshake padded past 256 bytes (still valid JSON with the right
/// token) is refused, and nothing it sends after reaches the world.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_an_oversized_handshake_is_refused() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-bighello");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let mut raw = UnixStream::connect(&server.address).expect("connect");
    // Bounds the old behavior (no cap) so the fail-before run ends; the fixed server closes
    // at once.
    raw.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let padded = format!("{{\"bearer\":\"secret\"}}{}", " ".repeat(300));
    // Both frames in one write: the server may close as soon as it has read the header.
    let mut both = Vec::new();
    write_frame(&mut both, padded.as_bytes());
    write_frame(
        &mut both,
        br#"{"jsonrpc":"2.0","id":1,"method":"app.ping","params":{}}"#,
    );
    std::io::Write::write_all(&mut raw, &both).expect("frames sent");
    let mut answer = Vec::new();
    let read = raw.read_to_end(&mut answer);
    // A close with our request still unread reads as a reset on Linux; either is a close.
    let closed = match &read {
        Ok(_) => true,
        Err(err) => err.kind() == std::io::ErrorKind::ConnectionReset,
    };
    assert!(closed, "the server closes the refused connection: {read:?}");
    assert!(
        !String::from_utf8_lossy(&answer).contains("\"authenticated\":true"),
        "an oversized handshake was accepted"
    );
    assert!(
        server.inbox.try_recv().is_err(),
        "a request after an oversized handshake reached the world"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// spec 23 § 2: the socket file is owner-only (0600) inside an owner-only agent dir (0700),
/// like the token it guards. The transport prepares the dir the way the plugin does, so an
/// existing dir keeps its mode (a shared rule with `endpoint::prepare_dir`) — the test sets
/// 0700 first to model that.
#[cfg(all(unix, not(target_os = "android")))]
#[test]
fn spec23_sec2_local_socket_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-mode").join("agent");
    endpoint::prepare_dir(&dir).expect("owner-only agent dir");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let mode = std::fs::metadata(&server.address)
        .expect("socket file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    let dir_mode = std::fs::metadata(&dir).expect("agent dir").permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "{dir_mode:o}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// spec 23 § 2: dropping the server removes the socket file, so the next start binds cleanly;
/// a symlink planted at the socket path is refused, never followed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_a_stale_socket_file_is_removed_and_a_symlink_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-stale");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let address = server.address.clone();
    drop(server);
    assert!(
        !std::path::Path::new(&address).exists(),
        "drop unlinks the socket"
    );
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("rebinds after the file is gone")
        .expect("a local transport exists");
    drop(server);

    std::os::unix::fs::symlink(&dir, &address).expect("plant a link at the socket path");
    let err = regia_agent::local::start("secret".into(), &dir, None).expect_err("a link is refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// spec 23 § 2 / ADR-5708: a frame over the cap ends that connection; the listener goes on
/// serving the next one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn spec23_sec2_an_oversized_frame_closes_that_connection_only() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-frame");
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let mut bad = UnixStream::connect(&server.address).expect("connect");
    write_frame(&mut bad, br#"{"bearer":"secret"}"#);
    // A header whose length is past the cap: the server refuses from the header alone and
    // never reads a body.
    let huge = (regia_agent::local::MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
    bad.write_all(&huge).expect("oversized header");
    bad.flush().expect("flush");
    drop(bad);
    assert!(
        server.inbox.recv_timeout(Duration::from_secs(10)).is_err(),
        "nothing reached the world"
    );
    // The next connection works.
    let mut client = Client::connect_local(&server.address, "secret").expect("connects");
    let result = call_answered_by_a_thread(&server.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writes one length-prefixed frame (the local transport's framing, ADR-5708).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_frame(stream: &mut impl std::io::Write, payload: &[u8]) {
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .expect("frame header");
    stream.write_all(payload).expect("frame body");
    stream.flush().expect("flush");
}

/// spec 23 § 2: `Client::connect_project` prefers the advertised local socket; here the
/// WebSocket URL in `endpoint.json` names a dead port, so only the local transport can answer.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn spec23_sec2_connect_project_prefers_the_local_socket() {
    let project = regia_test_utils::unique_temp_dir("agent-local-transport-project");
    let dir = endpoint::agent_dir(&project);
    let server = regia_agent::local::start("secret".into(), &dir, None)
        .expect("binds")
        .expect("a local transport exists");
    let advertised = Endpoint {
        url: "ws://127.0.0.1:1".into(),
        pid: std::process::id(),
        protocol: "0.2".into(),
        token_file: dir.join("token").display().to_string(),
        app: "test".into(),
    };
    endpoint::write(&dir, &advertised, "secret").expect("endpoint written");
    endpoint::write_socket(&dir, &server.address).expect("socket advertised");
    let mut client = Client::connect_project(&project).expect("connects over the local socket");
    let result = call_answered_by_a_thread(&server.inbox, &mut client, "app.ping", "local");
    assert_eq!(result["transport"], "local");
    drop(client);
    drop(server);
    let _ = std::fs::remove_dir_all(&project);
}

/// spec 23 § 2: the `socket` advertisement is validated like the endpoint: a relative address,
/// one outside the agent dir, or garbage is an error, never silently skipped to the WebSocket
/// fallback (a planted address must not choose where the bearer token goes).
#[test]
fn spec23_sec2_socket_advertisement_validation_fails_closed() {
    let dir = regia_test_utils::unique_temp_dir("agent-local-transport-advert");
    assert_eq!(endpoint::read_socket(&dir).expect("absent is fine"), None);
    #[cfg(not(windows))]
    {
        // Shape-valid (absolute, no parent-dir component) but outside the agent directory:
        // only the containment check can refuse it (sprint-rev-kimi-9 finding 8). Written
        // past `write_socket`'s own check, the way a hand-edited file would be.
        let outside = dir
            .parent()
            .expect("temp dirs have a parent")
            .join("agent-outside.sock");
        assert!(outside.is_absolute(), "{}", outside.display());
        std::fs::write(dir.join(endpoint::SOCKET_FILE), outside.to_str().expect("utf-8"))
            .expect("file written");
        assert!(
            endpoint::read_socket(&dir).is_err(),
            "an address outside the agent dir is refused"
        );
    }
    // Garbage written past `write_socket`'s own shape check: a relative path on Unix, plain
    // text on Windows.
    #[cfg(not(windows))]
    let garbage = "relative/agent.sock";
    #[cfg(windows)]
    let garbage = "not a pipe";
    std::fs::write(dir.join(endpoint::SOCKET_FILE), garbage).expect("garbage written");
    assert!(
        endpoint::read_socket(&dir).is_err(),
        "a malformed address is refused"
    );
    // A real server's address round-trips, and exit cleanup removes the advertisement of a
    // server this process runs (another app's stays: see the finding-6 test above).
    if let Some(server) = regia_agent::local::start("secret".into(), &dir, None).expect("binds") {
        endpoint::write_socket(&dir, &server.address).expect("file written");
        assert_eq!(
            endpoint::read_socket(&dir).expect("reads").as_deref(),
            Some(server.address.as_str())
        );
        endpoint::remove(&dir);
        assert_eq!(
            endpoint::read_socket(&dir).expect("removed with the endpoint"),
            None
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_dir_is_under_the_project() {
    let dir = endpoint::agent_dir(std::path::Path::new("/proj"));
    assert!(dir.ends_with(".regia/agent"), "{}", dir.display());
    assert!(dir.starts_with("/proj"));
}
