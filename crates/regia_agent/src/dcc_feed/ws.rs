// SPDX-License-Identifier: MPL-2.0
//! The WebSocket transports (`specs/60` § 4.2, § 4.12): one wire message per binary WebSocket
//! message without the length prefix, and a JSON text variant for control and small updates.
//!
//! The listener is its own, separate from the agent protocol's: another token and another
//! permission model. A foreign `Origin` is refused at the upgrade (RFC 6455 section 10.2).
//! JSON messages pass through the binary codec on the way in, so both variants share every check.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::handshake::server::{ErrorResponse, Request as HttpRequest, Response as HttpResponse};
use tungstenite::http::{HeaderValue, StatusCode};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message as WsMessage, WebSocket, accept_hdr_with_config};

use super::core::{FeedCore, SessionWriter};
use super::net::{ACCEPT_POLL, ConnectionSlot, MessageAssembler, lock, pre_hello_limits};
use super::session::{Phase, SessionId, Transport};
use super::wire::{
    self, CameraFrame, ErrorCode, ErrorFrame, FeedKind, FrameHeader, FrameInfo, Hello, ItemDeclare, Lens,
    LightFrame, Limits, Message, PROTOCOL_VERSION, Ping, TransformBlock, TransformFrame, WireError,
};

/// The binary subprotocol.
pub const SUBPROTOCOL_BINARY: &str = "regia-dcc-feed.v1";
/// The JSON subprotocol.
pub const SUBPROTOCOL_TEXT: &str = "regia-dcc-feed-json.v1";
/// The request path.
pub const PATH: &str = "/dcc_feed";
/// How long a read may hold the socket in the middle of a partial message.
const READ_SLICE: Duration = Duration::from_millis(50);

type SharedSocket = Arc<Mutex<WebSocket<TcpStream>>>;

fn reject(status: StatusCode, why: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(why.to_owned()));
    *response.status_mut() = status;
    response
}

/// The subprotocol a client offered that this server speaks.
fn pick_subprotocol(offered: &str) -> Option<&'static str> {
    offered.split(',').map(str::trim).find_map(|p| match p {
        SUBPROTOCOL_BINARY => Some(SUBPROTOCOL_BINARY),
        SUBPROTOCOL_TEXT => Some(SUBPROTOCOL_TEXT),
        _ => None,
    })
}

/// The acceptor of one WebSocket listener.
pub(super) fn accept_loop(
    listener: &TcpListener,
    core: &Arc<Mutex<FeedCore>>,
    stop: &AtomicBool,
    allowed_origins: &[String],
    limits: Limits,
) {
    let slots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // A slot before the thread (and before the handshake): see `MAX_FEED_CONNECTIONS`.
                let Some(slot) = ConnectionSlot::take(&slots) else {
                    continue;
                };
                let (core, origins) = (core.clone(), allowed_origins.to_vec());
                let spawned = std::thread::Builder::new()
                    .name("dcc-feed-ws".into())
                    .spawn(move || {
                        let _slot = slot;
                        serve(stream, peer, &core, &origins, limits);
                    });
                if let Err(err) = spawned {
                    tracing::warn!(target: "regia_agent", "dcc feed: no thread for a WebSocket: {err}");
                }
            }
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
    }
}

/// Upgrades one connection and runs it until it closes.
// tungstenite fixes the handshake callback's error type (an HTTP response), hence the large Err.
#[allow(clippy::result_large_err)]
fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    core: &Arc<Mutex<FeedCore>>,
    allowed_origins: &[String],
    limits: Limits,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_SLICE * 100));
    let Ok(watcher) = stream.try_clone() else {
        return;
    };
    let mut chosen = None;
    let callback = |request: &HttpRequest, mut response: HttpResponse| {
        if request.uri().path() != PATH {
            return Err(reject(StatusCode::NOT_FOUND, "not a DCC feed endpoint"));
        }
        if let Some(origin) = request.headers().get("origin") {
            let allowed = origin
                .to_str()
                .is_ok_and(|o| allowed_origins.iter().any(|a| a == o));
            if !allowed {
                return Err(reject(StatusCode::FORBIDDEN, "origin not allowed"));
            }
        }
        let offered = request
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let Some(protocol) = pick_subprotocol(offered) else {
            return Err(reject(StatusCode::BAD_REQUEST, "unknown subprotocol"));
        };
        chosen = Some(protocol);
        response
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
        Ok(response)
    };
    // Held to the pre-hello size until a hello is accepted; `read_loop` raises it then.
    let config = socket_config(pre_hello_limits(limits));
    let Ok(socket) = accept_hdr_with_config(stream, callback, Some(config)) else {
        return;
    };
    let text = chosen == Some(SUBPROTOCOL_TEXT);
    let _ = socket.get_ref().set_read_timeout(Some(READ_SLICE));
    let _ = socket
        .get_ref()
        .set_write_timeout(Some(Duration::from_millis(250)));
    let socket: SharedSocket = Arc::new(Mutex::new(socket));
    let writer = SessionWriter {
        out: Box::new(WsWriter {
            socket: socket.clone(),
            text,
            pending: Vec::new(),
        }),
        shutdown: {
            let Ok(stream) = watcher.try_clone() else {
                return;
            };
            Box::new(move || {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            })
        },
    };
    let id = lock(core).open(Transport::WebSocket, peer.ip().to_string(), Some(writer));
    read_loop(core, id, &socket, &watcher, text, limits);
}

/// The socket's size limits for `limits`: a binary message is a frame without its length prefix.
fn socket_config(limits: Limits) -> WebSocketConfig {
    let bytes = limits.max_message_bytes as usize + wire::HEADER_BYTES;
    WebSocketConfig::default()
        .max_message_size(Some(bytes))
        .max_frame_size(Some(bytes))
}

/// Reads messages. `peek` waits for bytes without the socket lock, so an idle reader never holds
/// up a reply. After each message the socket is read once more without waiting: the WebSocket
/// layer may already hold the next whole message from the same read of the stream, and `peek`
/// sees only what is still in the kernel (a ping sent right after a large message used to wait
/// there until the peer sent something else).
fn read_loop(
    core: &Mutex<FeedCore>,
    id: SessionId,
    socket: &SharedSocket,
    watcher: &TcpStream,
    text: bool,
    limits: Limits,
) {
    let mut assembler = MessageAssembler::new(limits);
    let mut negotiating = true;
    let mut drain = false;
    let mut probe = [0u8; 1];
    loop {
        // The timeout is shared with the socket (one open file), so an idle peek times out too.
        let peeked = if drain { Ok(1) } else { watcher.peek(&mut probe) };
        match peeked {
            Ok(n) if n > 0 => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if matches!(
                    lock(core).sessions.phase(id),
                    Some(Phase::Closing | Phase::Closed) | None
                ) {
                    return;
                }
                continue;
            }
            _ => {
                close(core, id, "end of stream");
                return;
            }
        }
        let read = match socket.lock() {
            Ok(mut socket) => {
                // A drain read must not wait: what is buffered or already in the kernel, else
                // `WouldBlock` and back to `peek`. The flag is shared with `watcher` (one open
                // file), so it is restored before the lock is released.
                if drain {
                    let _ = socket.get_ref().set_nonblocking(true);
                }
                let read = socket.read();
                if drain {
                    let _ = socket.get_ref().set_nonblocking(false);
                }
                read
            }
            Err(_) => return,
        };
        drain = read.is_ok();
        let keep_going = match read {
            Ok(WsMessage::Binary(bytes)) if !text => on_binary(core, id, &bytes, &mut assembler),
            Ok(WsMessage::Text(body)) if text => on_text(core, id, body.as_str(), &limits),
            Ok(WsMessage::Binary(_) | WsMessage::Text(_)) => {
                let err = WireError::new(
                    ErrorCode::MalformedFrame,
                    "wrong message type for this subprotocol",
                );
                lock(core).refuse_fatal(id, 0, &err);
                false
            }
            Ok(WsMessage::Close(_)) => {
                close(core, id, "end of stream");
                false
            }
            Ok(_) => true,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            Err(tungstenite::Error::Capacity(_)) => {
                let err = WireError::new(ErrorCode::FrameTooLarge, "message above the size limit");
                lock(core).refuse_fatal(id, 0, &err);
                false
            }
            Err(_) => {
                close(core, id, "end of stream");
                false
            }
        };
        if !keep_going {
            return;
        }
        // The full size limit applies once a hello is accepted (both subprotocols).
        if negotiating && lock(core).sessions.phase(id) != Some(Phase::Negotiating) {
            negotiating = false;
            if let Ok(mut socket) = socket.lock() {
                let full = socket_config(limits);
                socket.set_config(|config| *config = full);
            }
        }
    }
}

fn close(core: &Mutex<FeedCore>, id: SessionId, reason: &str) {
    let mut c = lock(core);
    let now = c.now_s;
    c.sessions.begin_close(id, reason, now);
}

fn on_binary(core: &Mutex<FeedCore>, id: SessionId, bytes: &[u8], assembler: &mut MessageAssembler) -> bool {
    // Binary messages are wire frames without the length prefix (spec § 4.2).
    let Some((head, payload)) = bytes.split_first_chunk::<8>() else {
        let err = WireError::new(ErrorCode::MalformedFrame, "message shorter than a frame header");
        lock(core).refuse_fatal(id, 0, &err);
        return false;
    };
    let mut full = [0u8; wire::HEADER_BYTES];
    full[..4].copy_from_slice(&u32::try_from(payload.len() + 8).unwrap_or(u32::MAX).to_le_bytes());
    full[4..].copy_from_slice(head);
    assembler.deliver(core, id, FrameHeader::parse(&full), payload.to_vec())
}

fn on_text(core: &Mutex<FeedCore>, id: SessionId, text: &str, limits: &Limits) -> bool {
    let mut c = lock(core);
    let bytes = u32::try_from(text.len()).unwrap_or(u32::MAX);
    match parse_text(text, limits) {
        Ok(msg) => c.submit(id, 0, bytes, msg),
        Err(err) if err.code.is_fatal() => c.refuse_fatal(id, 0, &err),
        Err(err) => c.send_error(id, 0, 0, &err),
    }
    c.flush_replies(id);
    !matches!(c.sessions.phase(id), Some(Phase::Closing | Phase::Closed) | None)
}

/// Writes frames the core encoded as WebSocket messages.
struct WsWriter {
    socket: SharedSocket,
    text: bool,
    pending: Vec<u8>,
}

impl std::io::Write for WsWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let pending = std::mem::take(&mut self.pending);
        let mut socket = self
            .socket
            .lock()
            .map_err(|_| std::io::Error::other("the socket lock is poisoned"))?;
        let mut rest = pending.as_slice();
        while let Some(head) = rest.first_chunk::<{ wire::HEADER_BYTES }>() {
            let header = FrameHeader::parse(head);
            let total = header
                .payload_len()
                .map_or(rest.len(), |n| n + wire::HEADER_BYTES);
            let Some((frame, tail)) = rest.split_at_checked(total) else {
                break;
            };
            rest = tail;
            let message = if self.text {
                let limits = Limits::default();
                let Ok(msg) = wire::decode(header, &frame[wire::HEADER_BYTES..], &limits) else {
                    continue;
                };
                match to_text(&msg) {
                    Some(text) => WsMessage::Text(text.into()),
                    None => continue,
                }
            } else {
                WsMessage::Binary(frame[4..].to_vec().into())
            };
            socket
                .send(message)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(())
    }
}

/// The JSON form of a server message.
#[must_use]
pub fn to_text(msg: &Message) -> Option<String> {
    let value = match msg {
        Message::HelloAck(a) => json!({
            "t": "hello_ack",
            "version": a.version,
            "session": a.session_id,
            "accepted": a.accepted,
            "caps": a.caps,
            "window_bytes": a.window_bytes,
            "server": a.server_name,
        }),
        Message::Ack(a) => {
            json!({ "t": "ack", "seq_applied": a.seq_applied, "credit_bytes": a.credit_bytes })
        }
        Message::SessionState(s) => json!({ "t": "session_state", "state": s.state, "reason": s.reason }),
        Message::Resync(r) => json!({ "t": "resync", "handle": r.handle }),
        Message::Pong(p) => json!({
            "t": "pong",
            "token": p.token,
            "sender_time_ns": p.sender_time_ns,
            "responder_time_ns": p.responder_time_ns,
        }),
        Message::Error(e) => json!({
            "t": "error",
            "ref_seq": e.ref_seq,
            "handle": e.handle,
            "code": e.code,
            "message": e.message,
            "fatal": e.fatal,
        }),
        Message::Goodbye(reason) => json!({ "t": "goodbye", "reason": reason }),
        _ => return None,
    };
    Some(value.to_string())
}

fn malformed(message: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::MalformedFrame, message)
}

fn u32_of(v: &Value, key: &str) -> Result<u32, WireError> {
    v[key]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| malformed(format!("`{key}` must be an unsigned integer")))
}

fn f32_of(v: &Value, key: &str, default: Option<f32>) -> Result<f32, WireError> {
    match (v[key].as_f64(), default) {
        (Some(n), _) => {
            let n = n as f32;
            if n.is_finite() {
                Ok(n)
            } else {
                Err(WireError::new(
                    ErrorCode::InvalidValue,
                    format!("`{key}` is not finite as f32"),
                ))
            }
        }
        (None, Some(d)) if v[key].is_null() => Ok(d),
        _ => Err(malformed(format!("`{key}` must be a number"))),
    }
}

fn floats<const N: usize>(v: &Value, key: &str, default: Option<[f32; N]>) -> Result<[f32; N], WireError> {
    let Some(array) = v[key].as_array() else {
        return default.ok_or_else(|| malformed(format!("`{key}` must be an array of {N} numbers")));
    };
    if array.len() != N {
        return Err(malformed(format!("`{key}` must hold {N} numbers")));
    }
    let mut out = [0.0; N];
    for (slot, value) in out.iter_mut().zip(array) {
        let n = value
            .as_f64()
            .ok_or_else(|| malformed(format!("`{key}` must hold numbers")))? as f32;
        if !n.is_finite() {
            return Err(WireError::new(
                ErrorCode::InvalidValue,
                format!("`{key}` is not finite as f32"),
            ));
        }
        *slot = n;
    }
    Ok(out)
}

fn string_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}

fn frame_info(v: &Value) -> Result<FrameInfo, WireError> {
    Ok(FrameInfo {
        frame_id: v["frame_id"]
            .as_u64()
            .ok_or_else(|| malformed("`frame_id` is required"))?,
        source_time_ns: v["time_ns"].as_u64().unwrap_or(0),
        timecode: None,
    })
}

fn block(v: &Value) -> Result<TransformBlock, WireError> {
    Ok(TransformBlock {
        translation: floats(v, "t", Some([0.0; 3]))?,
        rotation: floats(v, "quat", Some([0.0, 0.0, 0.0, 1.0]))?,
        scale: floats(v, "s", Some([1.0; 3]))?,
        world_space: v["world"].as_bool().unwrap_or(false),
    })
}

fn kind_of(name: &str) -> Result<FeedKind, WireError> {
    [
        FeedKind::Transform,
        FeedKind::Mesh,
        FeedKind::Camera,
        FeedKind::Light,
        FeedKind::Skeleton,
        FeedKind::Generic,
    ]
    .into_iter()
    .find(|k| k.name() == name)
    .ok_or_else(|| WireError::new(ErrorCode::InvalidValue, format!("unknown kind {name:?}")))
}

fn build_text(v: &Value) -> Result<Message, WireError> {
    let kind = v["t"].as_str().ok_or_else(|| malformed("`t` is required"))?;
    Ok(match kind {
        "hello" => Message::Hello(Hello {
            min_version: v["min_version"].as_u64().map_or(PROTOCOL_VERSION, |n| n as u32),
            max_version: v["max_version"].as_u64().map_or(PROTOCOL_VERSION, |n| n as u32),
            client_name: string_of(v, "client_name"),
            client_version: string_of(v, "client_version"),
            dcc: string_of(v, "dcc"),
            space: v["space"].as_str().unwrap_or("gltf").to_owned(),
            unit_scale: f32_of(v, "unit_scale", Some(1.0))?,
            caps: v["caps"].as_u64().map_or(0, |n| n as u32),
            token: string_of(v, "token"),
        }),
        "item" => Message::ItemDeclare(ItemDeclare {
            handle: u32_of(v, "handle")?,
            key: string_of(v, "key"),
            kind: kind_of(v["kind"].as_str().unwrap_or(""))?,
            parent_handle: v["parent"].as_u64().map_or(0, |n| n as u32),
            display_name: string_of(v, "display_name"),
            variant: v["variant"].as_u64().map_or(0, |n| n as u8),
            channels: Vec::new(),
        }),
        "delete" => Message::ItemDelete(u32_of(v, "handle")?),
        "transform" => Message::Transform(TransformFrame {
            handle: u32_of(v, "handle")?,
            info: frame_info(v)?,
            xf: block(v)?,
        }),
        "camera" => Message::Camera(CameraFrame {
            handle: u32_of(v, "handle")?,
            info: frame_info(v)?,
            xf: block(v)?,
            lens: Lens::Perspective {
                yfov_rad: f32_of(v, "yfov", None)?,
                aspect: f32_of(v, "aspect", Some(0.0))?,
                near: f32_of(v, "near", Some(0.1))?,
                far: f32_of(v, "far", Some(0.0))?,
            },
            physical: None,
        }),
        "light" => Message::Light(LightFrame {
            handle: u32_of(v, "handle")?,
            info: frame_info(v)?,
            xf: block(v)?,
            color: floats(v, "color", Some([1.0; 3]))?,
            intensity: f32_of(v, "intensity", None)?,
            range: f32_of(v, "range", Some(0.0))?,
            spot_inner_rad: f32_of(v, "inner", Some(0.0))?,
            spot_outer_rad: f32_of(v, "outer", Some(0.0))?,
        }),
        "ping" => Message::Ping(Ping {
            token: v["token"].as_u64().unwrap_or(0),
            sender_time_ns: v["time_ns"].as_u64().unwrap_or(0),
            responder_time_ns: 0,
        }),
        "pong" => Message::Pong(Ping {
            token: v["token"].as_u64().unwrap_or(0),
            sender_time_ns: v["sender_time_ns"].as_u64().unwrap_or(0),
            responder_time_ns: v["responder_time_ns"].as_u64().unwrap_or(0),
        }),
        "error" => Message::Error(ErrorFrame {
            ref_seq: 0,
            handle: 0,
            code: string_of(v, "code"),
            message: string_of(v, "message"),
            fatal: v["fatal"].as_bool().unwrap_or(false),
        }),
        "goodbye" => Message::Goodbye(string_of(v, "reason")),
        "mesh" | "texture" | "clip" | "pose" => {
            return Err(WireError::new(
                ErrorCode::KindNotSupported,
                format!("{kind} is binary only"),
            ));
        }
        other => return Err(malformed(format!("unknown message {other:?}"))),
    })
}

/// Parses one JSON message, then runs it through the binary codec so it takes every check of a
/// binary message.
///
/// # Errors
///
/// `malformed_frame`, `invalid_value`, `kind_not_supported` (mesh, texture, clip and pose are
/// binary only), or any error of [`wire::decode`].
pub fn parse_text(text: &str, limits: &Limits) -> Result<Message, WireError> {
    let value: Value = serde_json::from_str(text).map_err(|e| malformed(e.to_string()))?;
    let msg = build_text(&value)?;
    let mut frame = Vec::new();
    wire::encode(&msg, 0, &mut frame)?;
    let Some(head) = frame.first_chunk::<{ wire::HEADER_BYTES }>() else {
        return Err(malformed("empty frame"));
    };
    wire::decode(FrameHeader::parse(head), &frame[wire::HEADER_BYTES..], limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprotocols_are_picked_from_the_offer() {
        assert_eq!(pick_subprotocol("x, regia-dcc-feed.v1"), Some(SUBPROTOCOL_BINARY));
        assert_eq!(pick_subprotocol("regia-dcc-feed-json.v1"), Some(SUBPROTOCOL_TEXT));
        assert_eq!(pick_subprotocol("other"), None);
        assert_eq!(pick_subprotocol(""), None);
    }

    #[test]
    fn text_messages_take_the_binary_checks() {
        let limits = Limits::default();
        let ok = r#"{"t":"transform","handle":1,"frame_id":1,"quat":[0,0,0,1],"s":[1,1,1]}"#;
        assert!(matches!(parse_text(ok, &limits), Ok(Message::Transform(_))));
        let zero = r#"{"t":"transform","handle":1,"frame_id":1,"quat":[0,0,0,0]}"#;
        assert_eq!(
            parse_text(zero, &limits).expect_err("zero quat").code(),
            "invalid_value"
        );
        let big = r#"{"t":"light","handle":1,"frame_id":1,"intensity":1e300}"#;
        assert_eq!(
            parse_text(big, &limits).expect_err("overflow").code(),
            "invalid_value"
        );
        let mesh = r#"{"t":"mesh","handle":1}"#;
        assert_eq!(
            parse_text(mesh, &limits).expect_err("binary only").code(),
            "kind_not_supported"
        );
        assert_eq!(
            parse_text("[]", &limits).expect_err("no t").code(),
            "malformed_frame"
        );
        assert_eq!(
            parse_text("{\"t\":\"nope\"}", &limits)
                .expect_err("unknown")
                .code(),
            "malformed_frame"
        );
    }

    #[test]
    fn server_replies_have_a_text_form() {
        let text = to_text(&Message::Resync(wire::Resync { handle: 0 })).expect("resync");
        assert_eq!(text, r#"{"handle":0,"t":"resync"}"#);
        assert!(to_text(&Message::ItemDelete(1)).is_none());
    }
}
