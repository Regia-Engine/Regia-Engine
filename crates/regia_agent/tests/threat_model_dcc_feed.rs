// SPDX-License-Identifier: MPL-2.0
//! `docs/security/threat-model.md`, the DCC feed row (finding 29): before its `hello` is accepted
//! a session's frames are held to `MAX_PRE_HELLO_BYTES`, on loopback TCP and on the WebSocket, so
//! a peer without the token cannot make a reader buffer `max_message_bytes`. After the hello the
//! full limit applies again.
//!
//! The client sockets carry a read timeout only as a guard against a hang: every result comes from
//! what the server sends, not from the clock.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_agent::dcc_feed::core::FeedCore;
use regia_agent::dcc_feed::net::{self, ListenerInfo, Listeners, MAX_PRE_HELLO_BYTES, lock};
use regia_agent::dcc_feed::session::Phase;
use regia_agent::dcc_feed::settings::DccFeedSettings;
use regia_agent::dcc_feed::wire::{
    self, FLAG_OPTIONAL, FrameHeader, FrameReader, Hello, Limits, Message, Ping, Reassembler,
};
use regia_agent::dcc_feed::ws;

const TOKEN: &str = "secret-token";
const HANG_GUARD: Duration = Duration::from_secs(120);
/// A type id this protocol does not define; sent OPTIONAL, so the server ignores it.
const UNKNOWN_TYPE: u16 = 0x7ff0;

fn server(listen: &str) -> (Arc<Mutex<FeedCore>>, Listeners) {
    let settings = DccFeedSettings {
        enabled: true,
        listen: vec![listen.to_owned()],
        auto_accept: vec!["tool".to_owned()],
        ..DccFeedSettings::default()
    };
    let core = Arc::new(Mutex::new(FeedCore::new(&settings, TOKEN)));
    let listeners = net::start(&settings, &core, None).expect("listeners");
    (core, listeners)
}

fn address(listeners: &Listeners, transport: &str) -> String {
    let found: Option<&ListenerInfo> = listeners.info.iter().find(|l| l.transport == transport);
    found.expect("listener").address.clone()
}

fn hello() -> Message {
    Message::Hello(Hello {
        min_version: 1,
        max_version: 1,
        client_name: "tool".to_owned(),
        client_version: "1".to_owned(),
        dcc: "test".to_owned(),
        space: "gltf".to_owned(),
        unit_scale: 1.0,
        caps: 0,
        token: TOKEN.to_owned(),
    })
}

fn ping() -> Message {
    Message::Ping(Ping {
        token: 7,
        sender_time_ns: 1,
        responder_time_ns: 0,
    })
}

fn encode(msg: &Message, seq: u32) -> Vec<u8> {
    let mut frame = Vec::new();
    wire::encode(msg, seq, &mut frame).expect("encode");
    frame
}

/// A whole frame of `payload_len` zero bytes with the given type and flags.
fn raw_frame(msg_type: u16, flags: u16, seq: u32, payload_len: u32) -> Vec<u8> {
    let header = FrameHeader {
        len: payload_len + 8,
        msg_type,
        flags,
        seq,
    };
    let mut frame = header.to_bytes().to_vec();
    frame.resize(frame.len() + payload_len as usize, 0);
    frame
}

/// The next message from a TCP stream, or `None` at its end.
fn read_tcp<R: Read>(reader: &mut FrameReader<R>) -> Option<Message> {
    let limits = Limits::default();
    let mut assembler = Reassembler::new();
    loop {
        let (header, payload) = reader.next(&limits).ok()??;
        if let Some((head, body)) = assembler.push(header, payload, &limits).expect("fragments") {
            return Some(wire::decode(head, &body, &limits).expect("decode"));
        }
    }
}

/// Reads until a message `pick` accepts; panics at the end of the stream.
fn read_tcp_until<R: Read>(reader: &mut FrameReader<R>, pick: impl Fn(&Message) -> bool) -> Message {
    loop {
        let msg = read_tcp(reader).expect("the stream ended first");
        if pick(&msg) {
            return msg;
        }
    }
}

fn ws_connect(url: &str) -> tungstenite::WebSocket<TcpStream> {
    use tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().expect("request");
    request.headers_mut().insert(
        "sec-websocket-protocol",
        ws::SUBPROTOCOL_BINARY.parse().expect("header"),
    );
    let host = url.trim_start_matches("ws://").split('/').next().expect("host");
    let stream = TcpStream::connect(host).expect("connect");
    stream.set_read_timeout(Some(HANG_GUARD)).expect("timeout");
    match tungstenite::client(request, stream) {
        Ok((socket, _)) => socket,
        Err(e) => panic!("handshake: {e}"),
    }
}

/// A binary WebSocket message is a frame without its length prefix.
fn ws_send(socket: &mut tungstenite::WebSocket<TcpStream>, frame: &[u8]) {
    socket
        .send(tungstenite::Message::Binary(frame[4..].to_vec().into()))
        .expect("send");
}

/// The next message from the WebSocket, or `None` once the server is gone.
fn ws_read(socket: &mut tungstenite::WebSocket<TcpStream>) -> Option<Message> {
    let data = socket.read().ok()?.into_data();
    let (head, payload) = data.split_first_chunk::<8>()?;
    let mut full = [0u8; wire::HEADER_BYTES];
    full[..4].copy_from_slice(&u32::try_from(payload.len() + 8).expect("len").to_le_bytes());
    full[4..].copy_from_slice(head);
    Some(wire::decode(FrameHeader::parse(&full), payload, &Limits::default()).expect("decode"))
}

fn ws_read_until(socket: &mut tungstenite::WebSocket<TcpStream>, pick: impl Fn(&Message) -> bool) -> Message {
    loop {
        let msg = ws_read(socket).expect("the socket closed first");
        if pick(&msg) {
            return msg;
        }
    }
}

fn is_error_or_pong(msg: &Message) -> bool {
    matches!(msg, Message::Error(_) | Message::Pong(_))
}

/// Only the header is sent: a server that still read up to `max_message_bytes` would wait for the
/// body and never answer.
#[test]
fn a_tcp_frame_before_hello_is_held_to_the_pre_hello_size() {
    let (core, listeners) = server("tcp:127.0.0.1:0");
    let mut stream = TcpStream::connect(address(&listeners, "tcp")).expect("connect");
    stream.set_read_timeout(Some(HANG_GUARD)).expect("timeout");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    let frame = raw_frame(wire::msg::HELLO, 0, 1, MAX_PRE_HELLO_BYTES + 1);
    stream.write_all(&frame[..wire::HEADER_BYTES]).expect("write");
    let Some(Message::Error(error)) = read_tcp(&mut reader) else {
        panic!("expected an error before the body was sent");
    };
    assert_eq!(error.code, "frame_too_large");
    assert!(error.fatal);
    assert!(
        lock(&core).sessions.iter().all(|s| s.phase == Phase::Closing),
        "the session is closing"
    );
}

#[test]
fn a_tcp_session_gets_the_full_limit_after_its_hello() {
    let (_core, listeners) = server("tcp:127.0.0.1:0");
    let mut stream = TcpStream::connect(address(&listeners, "tcp")).expect("connect");
    stream.set_read_timeout(Some(HANG_GUARD)).expect("timeout");
    let mut reader = FrameReader::new(stream.try_clone().expect("clone"));
    stream.write_all(&encode(&hello(), 1)).expect("hello");
    let ack = read_tcp_until(&mut reader, |m| matches!(m, Message::HelloAck(_)));
    assert!(matches!(ack, Message::HelloAck(a) if a.accepted));
    let big = raw_frame(UNKNOWN_TYPE, FLAG_OPTIONAL, 2, 4 * MAX_PRE_HELLO_BYTES);
    stream.write_all(&big).expect("big frame");
    stream.write_all(&encode(&ping(), 3)).expect("ping");
    let reply = read_tcp_until(&mut reader, is_error_or_pong);
    assert!(
        matches!(reply, Message::Pong(_)),
        "the session stayed open: {reply:?}"
    );
}

/// A binary message just over the pre-hello size, sent before any hello.
#[test]
fn a_websocket_message_before_hello_is_held_to_the_pre_hello_size() {
    let (core, listeners) = server("ws:127.0.0.1:0");
    let mut socket = ws_connect(&address(&listeners, "websocket"));
    ws_send(
        &mut socket,
        &raw_frame(wire::msg::HELLO, 0, 1, MAX_PRE_HELLO_BYTES + 1024),
    );
    let Some(Message::Error(error)) = ws_read(&mut socket) else {
        panic!("expected an error");
    };
    assert_eq!(error.code, "frame_too_large");
    assert!(
        lock(&core).sessions.iter().all(|s| s.phase == Phase::Closing),
        "the session is closing"
    );
}

#[test]
fn a_websocket_session_gets_the_full_limit_after_its_hello() {
    let (_core, listeners) = server("ws:127.0.0.1:0");
    let mut socket = ws_connect(&address(&listeners, "websocket"));
    ws_send(&mut socket, &encode(&hello(), 1));
    let ack = ws_read_until(&mut socket, |m| matches!(m, Message::HelloAck(_)));
    assert!(matches!(ack, Message::HelloAck(a) if a.accepted));
    ws_send(
        &mut socket,
        &raw_frame(UNKNOWN_TYPE, FLAG_OPTIONAL, 2, 4 * MAX_PRE_HELLO_BYTES),
    );
    ws_send(&mut socket, &encode(&ping(), 3));
    let reply = ws_read_until(&mut socket, is_error_or_pong);
    assert!(
        matches!(reply, Message::Pong(_)),
        "the session stayed open: {reply:?}"
    );
}
