// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 8 test 3 over real sockets: version and token refusals, the Unix socket needing no
//! token, the WebSocket `Origin` and subprotocol checks, and remote binds. Every wait here is a
//! blocking read on the socket, so nothing depends on the wall clock.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use regia_agent::dcc_feed::core::FeedCore;
use regia_agent::dcc_feed::net::{self, ListenerInfo, Listeners, lock};
use regia_agent::dcc_feed::session::Phase;
use regia_agent::dcc_feed::settings::DccFeedSettings;
use regia_agent::dcc_feed::wire::{self, FrameReader, Hello, Limits, Message, Reassembler};
use regia_agent::dcc_feed::ws;

const TOKEN: &str = "secret-token";

fn server(listen: &[&str]) -> (Arc<Mutex<FeedCore>>, Listeners, DccFeedSettings) {
    let settings = DccFeedSettings {
        enabled: true,
        listen: listen.iter().map(|s| (*s).to_owned()).collect(),
        auto_accept: vec!["tool".to_owned()],
        ..DccFeedSettings::default()
    };
    let core = Arc::new(Mutex::new(FeedCore::new(&settings, TOKEN)));
    let listeners = net::start(&settings, &core, None).expect("listeners");
    (core, listeners, settings)
}

fn address(listeners: &Listeners, transport: &str) -> String {
    let found: Option<&ListenerInfo> = listeners.info.iter().find(|l| l.transport == transport);
    found.expect("listener").address.clone()
}

fn hello(token: &str, min_version: u32) -> Message {
    Message::Hello(Hello {
        min_version,
        max_version: min_version.max(1),
        client_name: "tool".to_owned(),
        client_version: "1".to_owned(),
        dcc: "test".to_owned(),
        space: "gltf".to_owned(),
        unit_scale: 1.0,
        caps: 0,
        token: token.to_owned(),
    })
}

fn send<W: Write>(stream: &mut W, msg: &Message) {
    let mut frame = Vec::new();
    wire::encode(msg, 1, &mut frame).expect("encode");
    stream.write_all(&frame).expect("write");
}

/// The next message from the server, or `None` at the end of the stream.
fn read<R: Read>(reader: &mut FrameReader<R>) -> Option<Message> {
    let limits = Limits::default();
    let mut assembler = Reassembler::new();
    loop {
        let (header, payload) = reader.next(&limits).ok()??;
        if let Some((head, body)) = assembler.push(header, payload, &limits).expect("fragments") {
            return Some(wire::decode(head, &body, &limits).expect("decode"));
        }
    }
}

#[test]
fn tcp_refuses_a_wrong_or_missing_token_and_closes() {
    let (core, listeners, _) = server(&["tcp:127.0.0.1:0"]);
    let addr = address(&listeners, "tcp");
    for token in ["wrong", ""] {
        let mut stream = TcpStream::connect(&addr).expect("connect");
        let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
        send(&mut stream, &hello(token, 1));
        let Some(Message::Error(error)) = read(&mut reader) else {
            panic!("expected an error");
        };
        assert_eq!(error.code, "auth_failed");
        assert!(error.fatal);
        // Engine time has not moved, so the next hello from this peer is rate limited.
        let mut c = lock(&core);
        let later = c.now_s + 2.0;
        c.begin_tick(later);
        c.flush();
    }
    assert!(lock(&core).sessions.iter().all(|s| s.phase == Phase::Closed));
}

#[test]
fn the_right_token_is_accepted_and_a_resync_follows() {
    let (_core, listeners, _) = server(&["tcp:127.0.0.1:0"]);
    let mut stream = TcpStream::connect(address(&listeners, "tcp")).expect("connect");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    send(&mut stream, &hello(TOKEN, 1));
    let Some(Message::HelloAck(ack)) = read(&mut reader) else {
        panic!("expected hello_ack");
    };
    assert!(ack.accepted, "the client is on the allow list");
    assert!(matches!(read(&mut reader), Some(Message::Resync(r)) if r.handle == 0));
}

#[test]
fn a_newer_client_gets_the_supported_range() {
    let (_core, listeners, _) = server(&["tcp:127.0.0.1:0"]);
    let mut stream = TcpStream::connect(address(&listeners, "tcp")).expect("connect");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    send(&mut stream, &hello(TOKEN, 2));
    let Some(Message::Error(error)) = read(&mut reader) else {
        panic!("expected an error");
    };
    assert_eq!(error.code, "unsupported_version");
    assert!(error.message.contains("1..=1"), "{}", error.message);
}

#[cfg(unix)]
#[test]
fn the_unix_socket_needs_no_token_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_unix");
    let settings = DccFeedSettings {
        listen: vec!["unix".to_owned()],
        auto_accept: vec!["tool".to_owned()],
        ..DccFeedSettings::default()
    };
    let core = Arc::new(Mutex::new(FeedCore::new(&settings, TOKEN)));
    let listeners = net::start(&settings, &core, Some(&dir)).expect("listeners");
    let path = dir.join("feed.sock");
    let mode = std::fs::metadata(&path).expect("socket").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let endpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("endpoint.json")).expect("endpoint")).expect("json");
    assert_eq!(endpoint["transports"][0]["transport"], "unix");
    assert_eq!(std::fs::read_to_string(dir.join("token")).expect("token"), TOKEN);

    let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("connect");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    send(&mut stream, &hello("", 1));
    assert!(matches!(read(&mut reader), Some(Message::HelloAck(a)) if a.accepted));
    drop(listeners);
    assert!(!path.exists(), "the socket goes with the listener");
    let _ = std::fs::remove_dir_all(&dir);
}

/// sec-review-assets-agent (ADR-5869): the Unix listener created, chmodded and cleaned its folder
/// through a symbolic link (a cloned project's `.regia/dcc_feed -> elsewhere`): the target became
/// 0700, a `feed.sock` there was deleted and a socket bound in it. A linked folder is refused first.
#[cfg(unix)]
#[test]
fn a_linked_feed_folder_is_refused_before_anything_changes_through_it() {
    use std::os::unix::fs::PermissionsExt;
    let root = regia_test_utils::unique_temp_dir("dcc_feed_unix_link");
    let target = root.join("elsewhere");
    std::fs::create_dir_all(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("mode");
    std::fs::write(target.join("feed.sock"), b"keep me").expect("victim");
    let link = root.join("dcc_feed");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    let settings = DccFeedSettings {
        listen: vec!["unix".to_owned()],
        ..DccFeedSettings::default()
    };
    let core = Arc::new(Mutex::new(FeedCore::new(&settings, TOKEN)));
    assert!(
        net::start(&settings, &core, Some(&link)).is_err(),
        "a linked folder is refused"
    );
    assert_eq!(
        std::fs::read(target.join("feed.sock")).expect("victim"),
        b"keep me"
    );
    let mode = std::fs::metadata(&target).expect("target").permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "the link's target keeps its mode");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_remote_bind_needs_allow_remote() {
    let settings = DccFeedSettings {
        listen: vec!["tcp:0.0.0.0:0".to_owned()],
        ..DccFeedSettings::default()
    };
    let core = Arc::new(Mutex::new(FeedCore::new(&settings, TOKEN)));
    let err = net::start(&settings, &core, None).expect_err("refused");
    assert!(err.contains("allow_remote"), "{err}");
    let ws = DccFeedSettings {
        listen: vec!["ws:0.0.0.0:0".to_owned()],
        ..DccFeedSettings::default()
    };
    assert!(ws.validate().expect_err("refused").contains("allow_remote"));
}

fn ws_request(url: &str, origin: Option<&str>, protocol: &str) -> tungstenite::handshake::client::Request {
    use tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().expect("request");
    let headers = request.headers_mut();
    headers.insert("sec-websocket-protocol", protocol.parse().expect("header"));
    if let Some(origin) = origin {
        headers.insert("origin", origin.parse().expect("header"));
    }
    request
}

fn ws_connect(
    url: &str,
    origin: Option<&str>,
    protocol: &str,
) -> Result<tungstenite::WebSocket<TcpStream>, tungstenite::Error> {
    let host = url.trim_start_matches("ws://").split('/').next().expect("host");
    let stream = TcpStream::connect(host).expect("connect");
    match tungstenite::client(ws_request(url, origin, protocol), stream) {
        Ok((socket, _)) => Ok(socket),
        Err(tungstenite::HandshakeError::Failure(e)) => Err(e),
        Err(tungstenite::HandshakeError::Interrupted(_)) => panic!("blocking stream"),
    }
}

#[test]
fn a_foreign_origin_and_an_unknown_subprotocol_are_refused() {
    let (_core, listeners, _) = server(&["ws:127.0.0.1:0"]);
    let url = address(&listeners, "websocket");
    let foreign = ws_connect(&url, Some("https://evil.example"), ws::SUBPROTOCOL_BINARY);
    assert!(matches!(foreign, Err(tungstenite::Error::Http(r)) if r.status() == 403));
    let unknown = ws_connect(&url, None, "something-else");
    assert!(matches!(unknown, Err(tungstenite::Error::Http(r)) if r.status() == 400));
    let wrong_path = ws_connect(&url.replace("/dcc_feed", "/other"), None, ws::SUBPROTOCOL_BINARY);
    assert!(matches!(wrong_path, Err(tungstenite::Error::Http(r)) if r.status() == 404));
}

#[test]
fn a_binary_websocket_speaks_frames_without_the_length_prefix() {
    let (_core, listeners, _) = server(&["ws:127.0.0.1:0"]);
    let mut socket =
        ws_connect(&address(&listeners, "websocket"), None, ws::SUBPROTOCOL_BINARY).expect("connect");
    let mut frame = Vec::new();
    wire::encode(&hello(TOKEN, 1), 1, &mut frame).expect("encode");
    socket
        .send(tungstenite::Message::Binary(frame[4..].to_vec().into()))
        .expect("send");
    let reply = socket.read().expect("reply").into_data();
    let msg_type = u16::from_le_bytes([reply[0], reply[1]]);
    assert_eq!(msg_type, wire::msg::HELLO_ACK);
}

/// The read timeout is shared with the peek on the same socket, so idleness must not read as a
/// disconnect. The pause only makes the session idle for longer than one timeout; the result
/// does not depend on its length.
#[test]
fn an_idle_websocket_stays_open() {
    let (_core, listeners, _) = server(&["ws:127.0.0.1:0"]);
    let mut socket =
        ws_connect(&address(&listeners, "websocket"), None, ws::SUBPROTOCOL_TEXT).expect("connect");
    let hello = format!(r#"{{"t":"hello","client_name":"tool","token":"{TOKEN}"}}"#);
    socket
        .send(tungstenite::Message::Text(hello.into()))
        .expect("send");
    socket.read().expect("hello_ack");
    socket.read().expect("resync");
    std::thread::sleep(std::time::Duration::from_millis(150));
    let ping = r#"{"t":"ping","token":7,"time_ns":1}"#;
    socket
        .send(tungstenite::Message::Text(ping.into()))
        .expect("send");
    let reply = socket.read().expect("pong");
    let reply: serde_json::Value = serde_json::from_str(reply.to_text().expect("text")).expect("json");
    assert_eq!(reply["t"], "pong");
    assert_eq!(reply["token"], 7);
}

#[test]
fn the_json_websocket_answers_in_text() {
    let (_core, listeners, _) = server(&["ws:127.0.0.1:0"]);
    let mut socket =
        ws_connect(&address(&listeners, "websocket"), None, ws::SUBPROTOCOL_TEXT).expect("connect");
    let hello = format!(r#"{{"t":"hello","client_name":"tool","token":"{TOKEN}"}}"#);
    socket
        .send(tungstenite::Message::Text(hello.into()))
        .expect("send");
    let reply = socket.read().expect("reply");
    let reply: serde_json::Value = serde_json::from_str(reply.to_text().expect("text")).expect("json");
    assert_eq!(reply["t"], "hello_ack");
    assert_eq!(reply["accepted"], true);
}

#[test]
fn a_hostile_frame_length_closes_the_session_without_the_allocation() {
    let (core, listeners, _) = server(&["tcp:127.0.0.1:0"]);
    let mut stream = TcpStream::connect(address(&listeners, "tcp")).expect("connect");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    let mut header = [0u8; wire::HEADER_BYTES];
    header[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    stream.write_all(&header).expect("write");
    let Some(Message::Error(error)) = read(&mut reader) else {
        panic!("expected an error");
    };
    assert_eq!(error.code, "frame_too_large");
    assert!(
        lock(&core).sessions.iter().all(|s| s.phase == Phase::Closing),
        "fatal"
    );
}
