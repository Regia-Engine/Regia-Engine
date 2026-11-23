// SPDX-License-Identifier: MPL-2.0
//! The protocol server: a loopback WebSocket listener on its own thread (ADR-014). Each connection gets
//! a thread that authenticates the bearer token during the handshake, parses JSON-RPC requests and
//! forwards them to the main world through a channel; the world answers through a per-request channel.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use regia_ecs::prelude::{Resource, bevy_ecs};
use serde_json::Value;
use tungstenite::handshake::server::{ErrorResponse, Request as HttpRequest, Response as HttpResponse};
use tungstenite::http::StatusCode;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, accept_hdr_with_config};

use crate::protocol::{self, Request, Response, RpcError};
use crate::render_out::{BOARD_ANSWER_AFTER, RenderOutBoard};
use crate::scope::{ScopedInbound, ToolScope, ToolTokens};

/// How long a connection waits for the engine to answer, unless the request asks for more
/// (`params.timeout_ms`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Most simultaneous connections (handshaking or connected); more are dropped before a thread starts.
pub const MAX_CONNECTIONS: usize = 16;

/// Most requests waiting for the world; a full inbox blocks the sending connection (backpressure).
pub const INBOX_CAPACITY: usize = 1024;

/// Most notifications (requests without an `id`) one connection may have queued for the world
/// at once. A notification gets no answer, so the connection used to read the next frame at
/// once: one client could fill the whole inbox with 1024 parsed messages of up to 32 MiB each.
/// Past this many, the connection waits for the oldest to be handled before it reads on
/// (audit-agent-sec Medium).
pub const MAX_QUEUED_NOTIFICATIONS: usize = 8;

/// How long a connection may take to complete the WebSocket handshake. A slot is taken before
/// the handshake, so without a deadline sixteen local sockets that never send the HTTP upgrade
/// held every slot and locked out the token holder (audit-agent-sec Medium).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest WebSocket message and frame accepted. `MAX_FRAME_BYTES` is also the local
/// transport's per-frame cap (`crate::local`).
pub(crate) const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Most memory one request may take once parsed (128 MiB, four times the largest message), counted
/// as the parse builds it: 32 bytes per JSON value, the bytes of every string and key, and 32 more per
/// object entry. A message under the 32 MiB cap could still parse into about sixteen times its size
/// (`[0,0,0,...]` is two bytes of text per 32-byte value, audit zai M2), so the parse stops at this
/// budget and answers `request_too_large` (ADR-2306). The largest call a contract allows (one million
/// numbers) takes about 32 MiB.
pub const MAX_PARSED_REQUEST_BYTES: usize = 4 * MAX_MESSAGE_BYTES;

/// The `data.code` of a request whose parse passed [`MAX_PARSED_REQUEST_BYTES`].
pub const REQUEST_TOO_LARGE: &str = "request_too_large";

/// Most abandoned requests remembered at once (see `abandon`); past it the oldest is forgotten
/// (and would run late, as every request did before).
const MAX_ABANDONED: usize = 4096;

/// Reply channels of queued requests whose caller already got its answer from the connection
/// (F0006, no answer in time) or whose connection closed: the world must not run them when it
/// drains them later (the caller may have retried; a late `scene.spawn` would spawn twice,
/// audit-concurrency-1103 agent H1). Matched by channel identity (`Sender::same_channel`).
static ABANDONED: std::sync::Mutex<Vec<(Sender<Response>, Gone)>> = std::sync::Mutex::new(Vec::new());

/// Why a queued request's caller no longer waits for the world's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gone {
    /// The connection answered `F0006` (no answer in time) or closed.
    TimedOut,
    /// The connection answered a `render.out_progress` from the progress board while the app
    /// thread was inside a render-out step (ADR-2582).
    AnsweredFromBoard,
}

/// Marks the request answering on `reply` as given up by its caller.
pub(crate) fn abandon(reply: Sender<Response>) {
    give_up(reply, Gone::TimedOut);
}

/// Test-only: marks the request answering on `reply` as one the connection answered from the
/// progress board (ADR-2582), so a test can queue the skips a stalled render-out step leaves.
#[cfg(test)]
pub(crate) fn abandon_board_for_test(reply: Sender<Response>) {
    give_up(reply, Gone::AnsweredFromBoard);
}

fn give_up(reply: Sender<Response>, why: Gone) {
    let mut abandoned = ABANDONED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if abandoned.len() >= MAX_ABANDONED {
        abandoned.remove(0);
    }
    abandoned.push((reply, why));
}

/// Whether the request answering on `reply` was given up by its caller (a request the
/// connection already answered with F0006). Forgets it either way: each request is drained once.
pub(crate) fn take_abandoned(reply: &Sender<Response>) -> bool {
    take_gone(reply).is_some()
}

/// [`take_abandoned`] with the reason.
pub(crate) fn take_gone(reply: &Sender<Response>) -> Option<Gone> {
    let mut abandoned = ABANDONED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = abandoned.iter().position(|(kept, _)| kept.same_channel(reply))?;
    Some(abandoned.swap_remove(at).1)
}

/// Counts a live connection; releases its slot on drop.
pub(crate) struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    /// Takes a slot, or `None` when `MAX_CONNECTIONS` are in use.
    pub(crate) fn take(live: &Arc<AtomicUsize>) -> Option<Self> {
        if live.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            live.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(live)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A request on its way to the world, with the channel its answer goes back on.
pub struct Inbound {
    /// The request.
    pub request: Request,
    /// Where to send the response (dropped for notifications).
    pub reply: Sender<Response>,
}

/// A running server.
pub struct Server {
    /// Bound address (`127.0.0.1:<random port>`).
    pub addr: SocketAddr,
    /// Requests from all connections.
    pub inbox: Receiver<Inbound>,
    /// The render-out progress board the connections answer polls from while the app thread is
    /// inside a render-out step (ADR-2582); insert it into the world as a resource.
    pub progress: RenderOutBoard,
}

/// Binds `127.0.0.1:0` and starts accepting connections that present `token`.
pub fn start(token: String) -> std::io::Result<Server> {
    start_at(token, 0)
}

/// Binds `127.0.0.1:<port>` (`0`: a random port) and starts accepting connections that present
/// `token`. A device dev launch names its port so the editor can forward to it (ADR-8472).
pub fn start_at(token: String, port: u16) -> std::io::Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    let (tx, rx) = bounded(INBOX_CAPACITY);
    let live = Arc::new(AtomicUsize::new(0));
    let board = RenderOutBoard::default();
    let progress = board.clone();
    std::thread::Builder::new()
        .name("regia-agent-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let Some(slot) = ConnectionSlot::take(&live) else {
                            tracing::warn!(target: "regia_agent", "connection refused: {MAX_CONNECTIONS} already open");
                            continue; // dropping the socket closes it before any thread starts
                        };
                        let (tx, token, board) = (tx.clone(), token.clone(), board.clone());
                        let spawned = std::thread::Builder::new().name("regia-agent-conn".into()).spawn(move || {
                            let _slot = slot;
                            serve_connection(stream, &token, &tx, Some(&board));
                        });
                        if let Err(err) = spawned {
                            tracing::warn!(target: "regia_agent", "cannot start connection thread: {err}");
                        }
                    }
                    Err(err) => tracing::warn!(target: "regia_agent", "accept failed: {err}"),
                }
            }
        })?;
    Ok(Server {
        addr,
        inbox: rx,
        progress,
    })
}

/// A running scoped listener (ADR-1523): connections present a token from a
/// [`ToolTokens`] table instead of the master token.
pub struct ScopedServer {
    /// Bound address (`127.0.0.1:<random port>`).
    pub addr: SocketAddr,
    /// Requests from every scoped connection, each with its caller's scope.
    pub inbox: Receiver<ScopedInbound>,
}

/// Called after a scoped request is queued, so a host whose event loop sleeps can wake up.
pub type WakeHook = Arc<dyn Fn() + Send + Sync>;

/// The app host's wake hook for the master listener and the local transport (ADR-7393).
static APP_WAKE: std::sync::RwLock<Option<WakeHook>> = std::sync::RwLock::new(None);

/// Sets (or, with `None`, clears) the hook that runs after each request the master listener or
/// the local transport queues for the world (ADR-7393). A host whose event loop sleeps between
/// frames sets it: with the viewport's realtime off the editor idles at a 1 Hz keep-alive, so
/// without a wake every agent call waited up to a second for the next frame to read the inbox. One hook per
/// process; a later call replaces it. The scoped listener has its own hook ([`start_scoped`]).
pub fn set_app_wake(hook: Option<WakeHook>) {
    *APP_WAKE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

/// Runs the app wake hook, if a host set one.
fn wake_app() {
    let hook = APP_WAKE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Binds `127.0.0.1:0` and accepts connections that present a live token of `tokens`
/// (ADR-1523). Each request is checked against the table again before it is queued: a revoked
/// token answers `tool_token_revoked` and the connection closes. `wake` runs after each queued
/// request.
pub fn start_scoped(tokens: ToolTokens, wake: Option<WakeHook>) -> std::io::Result<ScopedServer> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let (tx, rx) = bounded(INBOX_CAPACITY);
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("regia-agent-scoped-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let Some(slot) = ConnectionSlot::take(&live) else {
                            tracing::warn!(target: "regia_agent", "scoped connection refused: {MAX_CONNECTIONS} already open");
                            continue;
                        };
                        let (tx, tokens, wake) = (tx.clone(), tokens.clone(), wake.clone());
                        let spawned = std::thread::Builder::new()
                            .name("regia-agent-scoped-conn".into())
                            .spawn(move || {
                                let _slot = slot;
                                let lookup = tokens.clone();
                                serve(
                                    stream,
                                    None,
                                    move |req| scoped_caller(req, &lookup),
                                    |scope: &Arc<ToolScope>, request, reply| {
                                        if !tokens.is_live(scope.id) {
                                            return Forwarded::Refused(crate::scope::revoked_error(&scope.tool));
                                        }
                                        let timeout = timeout_for(&request);
                                        let inbound = ScopedInbound {
                                            request,
                                            reply,
                                            scope: Arc::clone(scope),
                                        };
                                        // Within the request's own timeout, as `queue_within`.
                                        match tx.send_timeout(inbound, timeout) {
                                            Ok(()) => {}
                                            Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => {
                                                return Forwarded::Busy(RpcError::new(
                                                    protocol::TIMEOUT,
                                                    "F0006",
                                                    format!(
                                                        "the app did not take the request within {} ms",
                                                        timeout.as_millis()
                                                    ),
                                                    "the app may be paused by the OS or blocked; pass params.timeout_ms for long operations",
                                                ));
                                            }
                                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                                return Forwarded::Closed;
                                            }
                                        }
                                        if let Some(wake) = &wake {
                                            wake();
                                        }
                                        Forwarded::Sent
                                    },
                                );
                            });
                        if let Err(err) = spawned {
                            tracing::warn!(target: "regia_agent", "cannot start scoped connection thread: {err}");
                        }
                    }
                    Err(err) => tracing::warn!(target: "regia_agent", "scoped accept failed: {err}"),
                }
            }
        })?;
    Ok(ScopedServer { addr, inbox: rx })
}

/// The bearer token of a handshake, when it has no `Origin` header (browsers send one; a page
/// must never drive the engine even if it learnt the port).
fn bearer(req: &HttpRequest) -> Option<&str> {
    if req.headers().get("origin").is_some() {
        return None;
    }
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// The scope of a scoped handshake's token, when it is live.
fn scoped_caller(req: &HttpRequest, tokens: &ToolTokens) -> Option<Arc<ToolScope>> {
    tokens.lookup(bearer(req)?)
}

fn authorised(req: &HttpRequest, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    let header_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()));
    // Browsers send Origin; a page must never drive the engine even if it learnt the port.
    header_ok && req.headers().get("origin").is_none()
}

/// Byte equality whose time does not depend on where the inputs differ.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn reject(status: StatusCode, why: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(why.to_string()));
    *response.status_mut() = status;
    response
}

/// Extra time the connection waits beyond a request's own `params.timeout_ms`.
///
/// A handler that honours `timeout_ms` itself (`play.wait_until` answers
/// `{ satisfied: false, last_error, detail }` at that deadline) polls in `Last`, so its own
/// answer lands up to a frame after the deadline. Without the grace the connection's identical
/// deadline won that race and the caller got F0006 "no answer … the app may be blocked" instead
/// of the handler's diagnosis (2026-10-11: a missing rig read as a blocked app).
pub const HANDLER_DEADLINE_GRACE: Duration = Duration::from_secs(5);

/// The longest `timeout_ms` a request may ask for (24 hours): a client's own deadline, but one
/// connection slot of [`MAX_CONNECTIONS`] must not be parked for ever (audit v0.2.12).
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Methods whose schema documents a 60 s answer deadline when `timeout_ms` is absent.
const LONG_DEFAULT_METHODS: [&str; 2] = ["seeing.capture_sequence", "seeing.contact_sheet"];

pub(crate) fn timeout_for(request: &Request) -> Duration {
    let default = if LONG_DEFAULT_METHODS.contains(&request.method.as_str()) {
        Duration::from_secs(60)
    } else {
        DEFAULT_TIMEOUT
    };
    request
        .params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map_or(default, |ms| {
            Duration::from_millis(ms)
                .min(MAX_REQUEST_TIMEOUT)
                .saturating_add(HANDLER_DEADLINE_GRACE)
        })
}

fn serve_connection(stream: TcpStream, token: &str, inbox: &Sender<Inbound>, board: Option<&RenderOutBoard>) {
    serve(
        stream,
        board,
        |req| authorised(req, token).then_some(()),
        |_: &(), request, reply| queue_within(inbox, Inbound { request, reply }, board),
    );
}

/// Queues `inbound` for the world within the request's own timeout: a world that has not
/// polled the inbox for that long (a paused app with a full inbox) answers `F0006` instead of
/// parking the connection past the deadline its client asked for (audit-v035-conc M5).
///
/// On a full inbox (ADR-2728) a request that may cancel a render-out wakes an app thread waiting on
/// a stalled output before it waits for a place, since that thread is what would make the place;
/// and a progress poll the board can answer waits only [`BOARD_ANSWER_AFTER`], then the connection
/// answers it from the board.
pub(crate) fn queue_within(
    inbox: &Sender<Inbound>,
    inbound: Inbound,
    board: Option<&RenderOutBoard>,
) -> Forwarded {
    let timeout = timeout_for(&inbound.request);
    let inbound = match inbox.try_send(inbound) {
        Ok(()) => {
            wake_app();
            return Forwarded::Sent;
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => return Forwarded::Closed,
        Err(crossbeam_channel::TrySendError::Full(inbound)) => inbound,
    };
    if may_cancel(&inbound.request.method) {
        crate::render_out::note_cancel_request();
    }
    let on_the_board = inbound.request.method == "render.out_progress"
        && board.is_some_and(|board| board.answer(&inbound.request.params).is_some());
    let wait = if on_the_board {
        BOARD_ANSWER_AFTER.min(timeout)
    } else {
        timeout
    };
    match inbox.send_timeout(inbound, wait) {
        Ok(()) => {
            wake_app();
            Forwarded::Sent
        }
        Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => Forwarded::Busy(RpcError::new(
            protocol::TIMEOUT,
            "F0006",
            format!("the app did not take the request within {} ms", wait.as_millis()),
            "the app may be paused by the OS or blocked; pass params.timeout_ms for long operations",
        )),
        Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => Forwarded::Closed,
    }
}

/// Whether a request may cancel a render-out, so it also wakes an app thread waiting on a stalled
/// render-out output (ADR-2396; dispatch still decides).
pub(crate) fn may_cancel(method: &str) -> bool {
    matches!(method, "render.out_cancel" | "agent.job_cancel")
}

/// A connection's socket with one deadline over the whole handshake. A socket read timeout alone
/// restarts on every read, so a peer sending one byte every few seconds held its slot for as long as
/// the 64 KiB header limit lasted, and sixteen such peers locked out the token holder without ever
/// presenting a token (ADR-3206).
struct DeadlineStream {
    stream: TcpStream,
    /// Reads past this instant fail; `None` once the caller is authenticated.
    deadline: Option<std::time::Instant>,
}

impl std::io::Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.stream.set_read_timeout(Some(left))?;
        }
        std::io::Read::read(&mut self.stream, buf)
    }
}

impl std::io::Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut self.stream, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.stream)
    }
}

/// What became of a request handed to a connection's `forward`.
pub(crate) enum Forwarded {
    /// Queued for the world; the answer comes back on the reply channel.
    Sent,
    /// Refused before queueing: answer with this error and close the connection.
    Refused(RpcError),
    /// Not queued in time: answer with this error and read the next request.
    Busy(RpcError),
    /// The app is shutting down: close the connection.
    Closed,
}

/// One connection: the handshake (`authorise` turns the HTTP request into the caller, or refuses
/// it), then JSON-RPC requests in order, each handed to `forward` and answered from its reply
/// channel within the request's timeout.
fn serve<C>(
    stream: TcpStream,
    board: Option<&RenderOutBoard>,
    authorise: impl FnOnce(&HttpRequest) -> Option<C>,
    forward: impl FnMut(&C, Request, Sender<Response>) -> Forwarded,
) {
    serve_with_deadline(stream, HANDSHAKE_TIMEOUT, board, authorise, forward);
}

/// [`serve`] with the handshake deadline as a parameter (tests use a short one).
// tungstenite fixes the handshake callback's error type (an HTTP response), hence the large Err.
#[allow(clippy::result_large_err)]
fn serve_with_deadline<C>(
    stream: TcpStream,
    handshake_timeout: Duration,
    board: Option<&RenderOutBoard>,
    authorise: impl FnOnce(&HttpRequest) -> Option<C>,
    mut forward: impl FnMut(&C, Request, Sender<Response>) -> Forwarded,
) {
    let peer = stream.peer_addr().ok();
    let _ = stream.set_nodelay(true);
    // The deadline covers the whole handshake, however its bytes arrive; it is cleared once the
    // caller is authenticated (an idle authenticated client keeps its connection, as before).
    if stream.set_read_timeout(Some(handshake_timeout)).is_err() {
        return;
    }
    let stream = DeadlineStream {
        stream,
        deadline: Some(std::time::Instant::now() + handshake_timeout),
    };
    let mut caller: Option<C> = None;
    let callback = |req: &HttpRequest, response: HttpResponse| match authorise(req) {
        Some(found) => {
            caller = Some(found);
            Ok(response)
        }
        None => {
            tracing::warn!(target: "regia_agent", "rejected unauthorized connection from {peer:?}");
            Err(reject(StatusCode::UNAUTHORIZED, "missing or wrong bearer token"))
        }
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    let mut socket = match accept_hdr_with_config(stream, callback, Some(config)) {
        Ok(socket) => socket,
        Err(err) => {
            tracing::debug!(target: "regia_agent", "handshake failed: {err}");
            return;
        }
    };
    let Some(caller) = caller else {
        return;
    };
    socket.get_mut().deadline = None;
    if socket.get_ref().stream.set_read_timeout(None).is_err() {
        return;
    }
    tracing::info!(target: "regia_agent", "agent client connected from {peer:?}");
    let mut connection = ConnectionGuard::new();
    // Reply channels of the notifications still queued: the world drops the sender once it has
    // handled one, which disconnects the receiver.
    let mut queued_notes: std::collections::VecDeque<Receiver<Response>> = std::collections::VecDeque::new();
    loop {
        let text = match socket.read() {
            Ok(Message::Text(text)) => text.as_str().to_string(),
            Ok(Message::Binary(_)) => {
                let resp = Response::err(
                    Value::Null,
                    RpcError::new(
                        protocol::INVALID_REQUEST,
                        "F0004",
                        "binary messages are not supported",
                        "send JSON-RPC as text",
                    ),
                );
                if send(&mut socket, &resp).is_err() {
                    break;
                }
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue, // ping/pong handled by tungstenite
        };
        let request = match parse_request(&text) {
            Ok(r) => r,
            Err(resp) => {
                if send(&mut socket, &resp).is_err() {
                    break;
                }
                continue;
            }
        };
        let id = request.id.clone();
        let timeout = timeout_for(&request);
        // A request that may cancel a render-out also wakes an app thread waiting on a stalled
        // render-out output, once it is queued (ADR-2396; dispatch still decides).
        let wakes = may_cancel(&request.method);
        // A progress poll may be answered from the board while the app thread is inside a long
        // render-out step (ADR-2582); only a well-formed request with an id.
        let board_params = board
            .filter(|_| request.method == "render.out_progress" && request.jsonrpc == "2.0")
            .filter(|_| matches!(request.id, Some(Value::String(_) | Value::Number(_))))
            .map(|board| (board, request.params.clone()));
        let (reply_tx, reply_rx) = bounded(1);
        // A request (with an id) keeps a handle on its channel: if no answer comes in time it is
        // marked abandoned so the world does not run it later. A notification keeps none (its
        // receiver must disconnect once the world drops the sender).
        let kept = id.as_ref().map(|_| reply_tx.clone());
        let tracked = connection.track(&reply_tx, &request.method);
        let tracked_reply = reply_tx.clone();
        let forwarded = if tracked {
            forward(&caller, request, reply_tx)
        } else {
            Forwarded::Busy(connection_capacity_error())
        };
        if !matches!(forwarded, Forwarded::Sent) {
            take_connection(&tracked_reply);
        }
        drop(tracked_reply);
        match forwarded {
            Forwarded::Sent => {
                if wakes {
                    crate::render_out::note_cancel_request();
                }
            }
            Forwarded::Refused(error) => {
                let _ = send(&mut socket, &Response::err(id.unwrap_or(Value::Null), error));
                break;
            }
            Forwarded::Busy(error) => {
                // A notification gets no answer; a request gets the error, or a poll the board's
                // answer: the inbox is full of what a long step has not drained yet (ADR-2728).
                if let Some(id) = id {
                    let board_answer = board_params
                        .as_ref()
                        .and_then(|(board, params)| board.answer(params));
                    let response = match board_answer {
                        Some(answer) => Response::ok(id, answer),
                        None => Response::err(id, error),
                    };
                    if send(&mut socket, &response).is_err() {
                        break;
                    }
                }
                continue;
            }
            Forwarded::Closed => break,
        }
        let Some(id) = id else {
            // Notification: no response, but no more than `MAX_QUEUED_NOTIFICATIONS` in flight.
            queued_notes.push_back(reply_rx);
            queued_notes
                .retain(|rx| !matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
            while queued_notes.len() > MAX_QUEUED_NOTIFICATIONS {
                if let Some(oldest) = queued_notes.pop_front() {
                    // Handled (sender dropped), answered, or the world is stuck: move on either way.
                    let _ = oldest.recv_timeout(timeout);
                }
            }
            continue;
        };
        let answered = match &board_params {
            Some((board, params)) => await_progress(&reply_rx, board, params, timeout, kept.as_ref()),
            None => reply_rx.recv_timeout(timeout).ok().map(ProgressAnswer::App),
        };
        if let Some(kept) = &kept {
            take_connection(kept);
        }
        let response = match answered {
            Some(answer) => answer.with_id(&id),
            None => {
                // Marked before the caller hears F0006, so a retry can never race the original.
                if let Some(kept) = kept {
                    abandon(kept);
                }
                Response::err(
                    id.clone(),
                    RpcError::new(
                        protocol::TIMEOUT,
                        "F0006",
                        format!("no answer within {} ms", timeout.as_millis()),
                        "the app may be paused by the OS or blocked; pass params.timeout_ms for long operations",
                    ),
                )
            }
        };
        let sent = send(&mut socket, &response);
        recycle_reply(response);
        if sent.is_err() {
            break;
        }
    }
    tracing::info!(target: "regia_agent", "agent client disconnected ({peer:?})");
}

/// A request's answer: the app's, or the progress board's (ADR-2582).
pub(crate) enum ProgressAnswer {
    App(Response),
    Board(Value),
}

impl ProgressAnswer {
    pub(crate) fn with_id(self, id: &Value) -> Response {
        match self {
            Self::App(response) => response,
            Self::Board(value) => Response::ok(id.clone(), value),
        }
    }
}

/// Waits for the app's answer to a `render.out_progress`; whenever none came for
/// [`BOARD_ANSWER_AFTER`] and the board can answer (the app thread is inside a step of the job asked
/// about), answers from the board and marks the queued request as answered so the app skips it. The
/// board is read again after each wait: a long step may begin after the poll was queued (ADR-2728).
pub(crate) fn await_progress(
    reply_rx: &Receiver<Response>,
    board: &RenderOutBoard,
    params: &Value,
    timeout: Duration,
    kept: Option<&Sender<Response>>,
) -> Option<ProgressAnswer> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match reply_rx.recv_timeout(BOARD_ANSWER_AFTER.min(left)) {
            Ok(response) => return Some(ProgressAnswer::App(response)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return None,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if let Some(answer) = board.answer(params) {
            // The app may have answered between the wait and the board read: its answer wins.
            if let Ok(response) = reply_rx.try_recv() {
                return Some(ProgressAnswer::App(response));
            }
            if let Some(kept) = kept {
                give_up(kept.clone(), Gone::AnsweredFromBoard);
            }
            return Some(ProgressAnswer::Board(answer));
        }
        if left <= BOARD_ANSWER_AFTER {
            return None;
        }
    }
}

/// Parses one text frame into a request, or the answer (null id) to send back: `F0005` for text
/// that is not a JSON-RPC request, `request_too_large` when the parse passes
/// [`MAX_PARSED_REQUEST_BYTES`]. Nesting deeper than serde_json's recursion limit (128) is a parse
/// error, not a stack overflow. The error answer is boxed: a `Response` is large and the parse
/// succeeds on every well-formed frame.
pub(crate) fn parse_request(text: &str) -> Result<Request, Box<Response>> {
    parse_request_within(text, MAX_PARSED_REQUEST_BYTES)
}

/// [`parse_request`] with a parse budget of `max_parsed` bytes.
fn parse_request_within(text: &str, max_parsed: usize) -> Result<Request, Box<Response>> {
    let invalid = |err: &dyn std::fmt::Display| {
        Box::new(Response::err(
            Value::Null,
            RpcError::new(
                protocol::PARSE_ERROR,
                "F0005",
                format!("invalid JSON-RPC: {err}"),
                "send {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":...,\"params\":{...}}",
            ),
        ))
    };
    let budget = bounded_json::Budget::new(max_parsed);
    let value = match bounded_json::parse(text, &budget) {
        Ok(value) => value,
        Err(_) if budget.exhausted() => {
            return Err(Box::new(Response::err(
                Value::Null,
                RpcError::new(
                    protocol::INVALID_REQUEST,
                    REQUEST_TOO_LARGE,
                    format!(
                        "the request would take more than {} MiB once parsed",
                        max_parsed / (1024 * 1024)
                    ),
                    "split it into smaller calls; an array of numbers costs about 32 bytes per item parsed",
                ),
            )));
        }
        Err(err) => return Err(invalid(&err)),
    };
    // `params` is moved into the request as parsed (no second copy of the large part); the small
    // envelope goes through the derived shape check.
    let Value::Object(mut fields) = value else {
        return serde_json::from_value(value).map_err(|err| invalid(&err));
    };
    let params = fields.remove("params");
    let mut request: Request = serde_json::from_value(Value::Object(fields)).map_err(|err| invalid(&err))?;
    if let Some(params) = params {
        request.params = params;
    }
    Ok(request)
}

/// A JSON parse into [`Value`] that counts what it builds and stops at a budget (ADR-2306).
mod bounded_json {
    use std::cell::Cell;
    use std::fmt;

    use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use serde_json::{Map, Number, Value};

    /// Bytes charged per JSON value (the size of a [`Value`]).
    const NODE: usize = std::mem::size_of::<Value>();
    /// Bytes charged per object entry on top of its key and value (the map's own bookkeeping).
    const ENTRY: usize = 32;

    /// What one parse may still build.
    pub(super) struct Budget {
        left: Cell<usize>,
        exhausted: Cell<bool>,
    }

    impl Budget {
        pub(super) fn new(bytes: usize) -> Self {
            Self {
                left: Cell::new(bytes),
                exhausted: Cell::new(false),
            }
        }

        /// Whether a parse ran out of budget (its error is then this, not bad JSON).
        pub(super) fn exhausted(&self) -> bool {
            self.exhausted.get()
        }

        fn charge<E: de::Error>(&self, bytes: usize) -> Result<(), E> {
            match self.left.get().checked_sub(bytes) {
                Some(left) => {
                    self.left.set(left);
                    Ok(())
                }
                None => {
                    self.exhausted.set(true);
                    Err(E::custom("the parse budget is spent"))
                }
            }
        }
    }

    /// Parses `text` (one JSON value, nothing after it) within `budget`.
    pub(super) fn parse(text: &str, budget: &Budget) -> Result<Value, serde_json::Error> {
        let mut de = serde_json::Deserializer::from_str(text);
        let value = Bounded { budget, top: true }.deserialize(&mut de)?;
        de.end()?;
        Ok(value)
    }

    /// The request envelope's fields: repeated at the top level, the derived `Request` shape refuses
    /// them (other repeated keys keep the last value, as a plain parse does).
    const ENVELOPE: [&str; 4] = ["jsonrpc", "id", "method", "params"];

    /// The seed and visitor: `top` is the request envelope (see `ENVELOPE`).
    #[derive(Clone, Copy)]
    struct Bounded<'b> {
        budget: &'b Budget,
        top: bool,
    }

    impl Bounded<'_> {
        fn inner(self) -> Self {
            Self { top: false, ..self }
        }
    }

    impl<'de> DeserializeSeed<'de> for Bounded<'_> {
        type Value = Value;

        fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
            deserializer.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Bounded<'_> {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("any JSON value")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
            self.budget.charge::<E>(NODE)?;
            Ok(Value::Bool(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
            self.budget.charge::<E>(NODE)?;
            Ok(Value::Number(v.into()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
            self.budget.charge::<E>(NODE)?;
            Ok(Value::Number(v.into()))
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
            self.budget.charge::<E>(NODE)?;
            Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
            self.budget.charge::<E>(NODE.saturating_add(v.len()))?;
            Ok(Value::String(v.to_owned()))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
            self.budget.charge::<E>(NODE.saturating_add(v.len()))?;
            Ok(Value::String(v))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
            self.budget.charge::<E>(NODE)?;
            Ok(Value::Null)
        }

        fn visit_none<E: de::Error>(self) -> Result<Value, E> {
            self.visit_unit()
        }

        fn visit_some<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
            self.deserialize(deserializer)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            self.budget.charge::<A::Error>(NODE)?;
            let mut items = Vec::new();
            while let Some(item) = seq.next_element_seed(self.inner())? {
                items.push(item);
            }
            Ok(Value::Array(items))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            self.budget.charge::<A::Error>(NODE)?;
            let mut fields = Map::new();
            while let Some(key) = map.next_key::<String>()? {
                self.budget.charge::<A::Error>(ENTRY.saturating_add(key.len()))?;
                if self.top && ENVELOPE.contains(&key.as_str()) && fields.contains_key(&key) {
                    return Err(de::Error::custom(format_args!("duplicate field `{key}`")));
                }
                let value = map.next_value_seed(self.inner())?;
                fields.insert(key, value);
            }
            Ok(Value::Object(fields))
        }
    }
}

/// Replies whose top-level strings add up to at least this many bytes (a screenshot's base64)
/// are serialized into a [`regia_render::screenshot::REPLY_BUFFERS`] buffer and sent as
/// [`REPLY_FRAGMENT`]-byte frames of one text message (ADR-8848).
const POOLED_REPLY_MIN: usize = 64 * 1024;

/// The payload of each frame of a pooled reply: tungstenite copies a frame into its write
/// buffer, so a one-frame reply cost a reply-sized copy per call.
const REPLY_FRAGMENT: usize = 64 * 1024;

fn send(socket: &mut tungstenite::WebSocket<DeadlineStream>, response: &Response) -> Result<(), ()> {
    let hint = reply_size_hint(response);
    if hint < POOLED_REPLY_MIN {
        let text = serde_json::to_string(response).map_err(|_| ())?;
        return socket.send(Message::Text(text.into())).map_err(|_| ());
    }
    let _span = tracing::debug_span!("agent_send_pooled", bytes = hint).entered();
    let mut buffer = regia_render::screenshot::REPLY_BUFFERS.take(hint);
    if serde_json::to_writer(&mut buffer, response).is_err() {
        regia_render::screenshot::REPLY_BUFFERS.give(buffer);
        return Err(());
    }
    // The buffer goes back to the pool when the last frame drops its slice.
    let payload = tungstenite::Bytes::from_owner(PooledReply(buffer));
    send_text_fragments(socket, &payload, REPLY_FRAGMENT)
}

/// Sends `payload` (UTF-8 JSON) as one text message in frames of at most `fragment` bytes
/// (RFC 6455 § 5.4: a text frame, continuation frames, the last one final).
fn send_text_fragments<S: std::io::Read + std::io::Write>(
    socket: &mut tungstenite::WebSocket<S>,
    payload: &tungstenite::Bytes,
    fragment: usize,
) -> Result<(), ()> {
    use tungstenite::protocol::frame::Frame;
    use tungstenite::protocol::frame::coding::{Data, OpCode};
    let fragment = fragment.max(1);
    let mut start: usize = 0;
    loop {
        let end = start.saturating_add(fragment).min(payload.len());
        let opcode = if start == 0 {
            OpCode::Data(Data::Text)
        } else {
            OpCode::Data(Data::Continue)
        };
        let last = end == payload.len();
        let frame = Frame::message(payload.slice(start..end), opcode, last);
        socket.write(Message::Frame(frame)).map_err(|_| ())?;
        if last {
            break;
        }
        start = end;
    }
    socket.flush().map_err(|_| ())
}

/// A pooled reply buffer that returns to [`regia_render::screenshot::REPLY_BUFFERS`] on drop.
struct PooledReply(Vec<u8>);

impl AsRef<[u8]> for PooledReply {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PooledReply {
    fn drop(&mut self) {
        regia_render::screenshot::REPLY_BUFFERS.give(std::mem::take(&mut self.0));
    }
}

/// The serialized size of `response` from its result's top-level strings, plus the envelope.
fn reply_size_hint(response: &Response) -> usize {
    let strings = match &response.result {
        Some(Value::String(text)) => text.len(),
        Some(Value::Object(fields)) => fields
            .values()
            .map(|value| value.as_str().map_or(0, str::len))
            .sum(),
        _ => 0,
    };
    strings.saturating_add(1024)
}

/// Gives the large strings of a sent reply (a screenshot's base64) back to
/// [`regia_render::screenshot::REPLY_BUFFERS`] (ADR-8848).
fn recycle_reply(response: Response) {
    if let Some(Value::Object(fields)) = response.result {
        for (_, value) in fields {
            if let Value::String(text) = value
                && text.capacity() >= POOLED_REPLY_MIN
            {
                regia_render::screenshot::REPLY_BUFFERS.give(text.into_bytes());
            }
        }
    }
}

/// Identity and liveness of the transport connection currently being dispatched.
/// Hosts may keep a clone to finish abandoned work after the connection closes.
#[derive(Resource, Clone)]
pub struct ConnectionLease(Arc<std::sync::atomic::AtomicBool>);

impl ConnectionLease {
    /// Whether the authenticated transport connection is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    /// Whether this connection still has requests waiting for dispatcher admission.
    #[must_use]
    pub fn has_pending_requests(&self) -> bool {
        REQUEST_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(_, connection)| connection.same_connection(self))
    }
    /// Whether two leases name the same transport connection.
    #[must_use]
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

static REQUEST_CONNECTIONS: std::sync::Mutex<Vec<(Sender<Response>, ConnectionLease)>> =
    std::sync::Mutex::new(Vec::new());

/// Owns the transport liveness bit until the connection loop exits.
pub(crate) struct ConnectionGuard {
    lease: ConnectionLease,
    retain_pending: bool,
}
impl ConnectionGuard {
    /// Starts a live transport identity.
    pub(crate) fn new() -> Self {
        Self {
            lease: ConnectionLease(Arc::new(std::sync::atomic::AtomicBool::new(true))),
            retain_pending: false,
        }
    }
    /// Associates a queued response channel with this connection, bounded like abandonment records.
    pub(crate) fn track(&mut self, reply: &Sender<Response>, method: &str) -> bool {
        // Ordinary hosts may consume the inbox without the dispatcher. Do not keep their
        // notification senders alive: dropping one must still release transport backpressure.
        self.retain_pending |= method == "editor.transaction_begin";
        if !self.retain_pending {
            return true;
        }
        let mut requests = REQUEST_CONNECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if requests.len() >= MAX_ABANDONED {
            return false;
        }
        // This protocol boundary is declared in the host contract table. Its queued notifications
        // need identity even if the transport closes before the host runs the begin request.
        requests.push((reply.clone(), self.lease.clone()));
        true
    }
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.lease.0.store(false, Ordering::Release);
        if !self.retain_pending {
            REQUEST_CONNECTIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(_, connection)| !connection.same_connection(&self.lease));
        }
        wake_app();
    }
}

/// The ordinary busy refusal when request identity bookkeeping is full.
pub(crate) fn connection_capacity_error() -> RpcError {
    RpcError::new(
        protocol::TIMEOUT,
        "F0006",
        "the host has too many pending requests",
        "wait for the host to drain its request queues",
    )
}

/// Removes and returns the transport identity of one queued request.
pub(crate) fn take_connection(reply: &Sender<Response>) -> Option<ConnectionLease> {
    let mut pending = REQUEST_CONNECTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = pending.iter().position(|(found, _)| found.same_channel(reply))?;
    Some(pending.swap_remove(index).1)
}

/// Optional host transaction lifecycle. No editor code is linked into the runtime.
#[derive(Resource, Clone, Copy)]
pub struct RequestLifecycle {
    /// Called at dispatcher drain boundaries, including frames with no requests.
    pub idle: fn(&mut regia_ecs::prelude::World),
    /// Called after permissions and before a handler, with its parameters and mutating classification.
    /// The world's `ConnectionLease`, when present, identifies this request's transport.
    pub before: fn(&mut regia_ecs::prelude::World, &str, &Value, bool) -> Result<(), RpcError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_notifications_release_their_reply_without_a_dispatcher() {
        let mut connection = ConnectionGuard::new();
        let (reply, answer) = bounded(1);
        assert!(connection.track(&reply, "app.ping"));
        drop(reply);
        assert!(matches!(
            answer.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        ));
    }

    /// `docs/security/threat-model.md`: the listener is on loopback, on a port the OS picked. The
    /// other process-wide listener, the scoped one, keeps the same rule.
    #[test]
    fn the_server_binds_loopback_on_an_os_picked_port() {
        let server = start("t".into()).expect("binds");
        assert!(server.addr.ip().is_loopback(), "{}", server.addr);
        assert_ne!(server.addr.port(), 0);
        let scoped = start_scoped(ToolTokens::default(), None).expect("binds");
        assert!(scoped.addr.ip().is_loopback(), "{}", scoped.addr);
        assert_ne!(scoped.addr.port(), 0);
    }

    /// `docs/security/threat-model.md`: a frame over `MAX_FRAME_BYTES` ends that connection; the
    /// listener goes on serving the next one.
    #[test]
    fn a_frame_over_the_cap_closes_that_connection_only() {
        let (tx, rx) = bounded(4);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("accept");
                serve_connection(stream, "secret", &tx, None);
            }
        });
        let mut socket = raw_connect(&format!("ws://{addr}/"), "secret");
        // Bounds a hang only.
        socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(120)))
            .expect("read timeout");
        // A real request, so that without the cap it would reach the world.
        let huge = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"app.ping","params":{{"pad":"{}"}}}}"#,
            "x".repeat(MAX_FRAME_BYTES + 1)
        );
        let sent = socket.send(Message::Text(huge.into()));
        if sent.is_ok() {
            assert!(
                !matches!(socket.read(), Ok(Message::Text(_))),
                "an oversized frame got an answer"
            );
        }
        assert!(rx.try_recv().is_err(), "an oversized frame reached the world");
        drop(socket);
        let mut next = raw_connect(&format!("ws://{addr}/"), "secret");
        next.send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"app.ping","params":{}}"#.into(),
        ))
        .expect("send");
        let inbound = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the next connection works");
        assert_eq!(inbound.request.method, "app.ping");
        drop(inbound);
        drop(next);
        server.join().expect("server thread");
    }

    /// Audit ADR-3206 finding 6: the read timeout restarted on every read, so a peer trickling the
    /// handshake kept its slot. Bytes are waiting here, and the read still fails: the deadline does
    /// not depend on how the bytes arrive, and no clock race is involved.
    #[test]
    fn a_read_past_the_handshake_deadline_fails_even_with_bytes_waiting() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let mut client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        client.write_all(b"GET / HTTP/1.1\r\n").expect("bytes");
        let mut buf = [0_u8; 4];
        let mut stream = DeadlineStream {
            stream: server.try_clone().expect("clone"),
            deadline: Some(std::time::Instant::now()),
        };
        assert_eq!(
            stream.read(&mut buf).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::TimedOut)
        );
        // Cleared after authentication: the same socket reads the same bytes.
        stream.deadline = None;
        assert_eq!(stream.read(&mut buf).expect("reads"), 4);
    }

    use serde_json::json;

    #[test]
    fn handshake_with_an_origin_header_is_refused() {
        let build = |origin: Option<&str>| {
            let mut builder = HttpRequest::builder().header("authorization", "Bearer secret");
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            builder.body(()).expect("request")
        };
        assert!(authorised(&build(None), "secret"));
        assert!(!authorised(&build(Some("http://evil.test")), "secret"));
    }

    /// audit-agent-sec (Medium): a socket that never sends the HTTP upgrade used to block its
    /// connection thread (and its slot) forever. With the deadline the thread returns and the
    /// slot is released; an authenticated connection then has no read deadline.
    #[test]
    fn a_silent_handshake_gives_its_slot_back() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let live = Arc::new(AtomicUsize::new(0));
        let silent = TcpStream::connect(addr).expect("connect");
        let (stream, _) = listener.accept().expect("accept");
        let slot = ConnectionSlot::take(&live).expect("slot");
        assert_eq!(live.load(Ordering::Acquire), 1);
        let served = std::thread::spawn(move || {
            let _slot = slot;
            serve_with_deadline(
                stream,
                Duration::from_millis(50),
                None,
                |_| Some(()),
                |_: &(), _, _| Forwarded::Closed,
            );
        });
        // Joining proves the thread returned although the client never wrote a byte.
        served.join().expect("serve returned");
        assert_eq!(live.load(Ordering::Acquire), 0, "the slot is released");
        drop(silent);
    }

    /// The deadline is gone once the handshake succeeded: an authenticated client that is idle
    /// longer than the handshake deadline is still served.
    #[test]
    fn an_authenticated_idle_client_outlives_the_handshake_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = bounded::<String>(1);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            serve_with_deadline(
                stream,
                Duration::from_millis(20),
                None,
                |_| Some(()),
                move |_: &(), request, _| {
                    let _ = seen_tx.send(request.method);
                    Forwarded::Closed
                },
            );
        });
        use tungstenite::client::IntoClientRequest as _;
        let request = format!("ws://{addr}/").into_client_request().expect("request");
        let (mut client, _) =
            tungstenite::client(request, TcpStream::connect(addr).expect("connect")).expect("handshake");
        // Idle past the handshake deadline: the server side must not time the read out. The
        // client waits on a timed-out probe socket read rather than a sleep.
        let probe = TcpListener::bind(("127.0.0.1", 0)).expect("probe");
        let idle = TcpStream::connect(probe.local_addr().expect("probe addr")).expect("probe connect");
        idle.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("probe timeout");
        let _ = std::io::Read::read(&mut &idle, &mut [0u8; 1]);
        client
            .send(Message::Text(
                r#"{"jsonrpc":"2.0","id":1,"method":"agent.ping","params":{}}"#.into(),
            ))
            .expect("send after idling");
        assert_eq!(seen_rx.recv().expect("request reached forward"), "agent.ping");
        server.join().expect("server");
    }

    /// audit-agent-sec (Medium): notifications are not answered, so a connection read frame after
    /// frame and one client could queue the whole inbox. Now at most `MAX_QUEUED_NOTIFICATIONS`
    /// wait for the world; the next one is read once the oldest is handled.
    #[test]
    fn a_connection_queues_a_bounded_number_of_notifications() {
        use std::sync::Mutex;
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (seen_tx, seen_rx) = crossbeam_channel::unbounded::<String>();
        // The "world": holds every reply sender until the test lets them go.
        let held: Arc<Mutex<Vec<Sender<Response>>>> = Arc::default();
        let world = Arc::clone(&held);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            serve_with_deadline(
                stream,
                HANDSHAKE_TIMEOUT,
                None,
                |_| Some(()),
                move |_: &(), request, reply| {
                    world
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(reply);
                    let _ = seen_tx.send(request.method);
                    Forwarded::Sent
                },
            );
        });
        use tungstenite::client::IntoClientRequest as _;
        let request = format!("ws://{addr}/").into_client_request().expect("request");
        let (mut client, _) =
            tungstenite::client(request, TcpStream::connect(addr).expect("connect")).expect("handshake");
        let total = 3 * MAX_QUEUED_NOTIFICATIONS;
        for n in 0..total {
            client
                .send(Message::Text(
                    format!(r#"{{"jsonrpc":"2.0","method":"note.{n}","params":{{}}}}"#).into(),
                ))
                .expect("send");
        }
        let mut seen = Vec::new();
        while seen.len() < total {
            // A burst never exceeds the cap plus the one that waits on the oldest.
            let first = seen_rx.recv().expect("forwarded");
            seen.push(first);
            while let Ok(more) = seen_rx.try_recv() {
                seen.push(more);
            }
            let held_now = held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            assert!(held_now <= MAX_QUEUED_NOTIFICATIONS + 1, "{held_now} queued");
            // The world handles everything it holds.
            held.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        let expected: Vec<String> = (0..total).map(|n| format!("note.{n}")).collect();
        assert_eq!(seen, expected, "in order, none lost");
        drop(client);
        server.join().expect("server");
    }

    /// audit-v035-conc M5: forwarding used a blocking send, so while the world did not poll a
    /// full inbox a connection stayed parked past its request's own timeout. A request that is
    /// not taken within its `timeout_ms` (plus the grace) is answered `F0006`.
    #[test]
    fn a_request_the_world_does_not_take_in_time_is_answered() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        // A zero-capacity inbox nobody receives from: every send waits, as on a full inbox.
        let (inbox, _held) = bounded::<Inbound>(0);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            serve_connection(stream, "secret", &inbox, None);
        });
        let mut socket = raw_connect(&format!("ws://{addr}/"), "secret");
        // Bounds a hang only: the answer comes after the request's 1 ms plus the grace.
        socket
            .get_mut()
            .set_read_timeout(Some(HANDLER_DEADLINE_GRACE * 12))
            .expect("read timeout");
        let response = send_text(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":7,"method":"app.ping","params":{"timeout_ms":1}}"#,
        );
        let error = response.error.expect("an error answer");
        assert_eq!(error.code, protocol::TIMEOUT, "{error:?}");
        drop(socket);
        server.join().expect("server thread");
    }

    fn request_with(params: Value) -> Request {
        Request {
            jsonrpc: "2.0".into(),
            id: Some(Value::from(1)),
            method: "play.wait_until".into(),
            params,
        }
    }

    /// A request's own deadline gets the grace, so a handler that answers at `timeout_ms`
    /// (`play.wait_until`'s `satisfied: false`) is heard instead of an F0006 timeout.
    #[test]
    fn a_request_deadline_gets_the_handler_grace() {
        assert_eq!(timeout_for(&request_with(json!({}))), DEFAULT_TIMEOUT);
        assert_eq!(
            timeout_for(&request_with(json!({ "timeout_ms": 20_000 }))),
            Duration::from_millis(20_000) + HANDLER_DEADLINE_GRACE
        );
        // Audit v0.2.12: a huge deadline parked a connection slot for ever; it is capped.
        assert_eq!(
            timeout_for(&request_with(json!({ "timeout_ms": u64::MAX }))),
            MAX_REQUEST_TIMEOUT + HANDLER_DEADLINE_GRACE
        );
    }

    /// `seeing.capture_sequence` / `seeing.contact_sheet` document a 60 s answer deadline when
    /// `timeout_ms` is absent; the connection must wait that long, not the 30 s default.
    #[test]
    fn long_default_methods_wait_sixty_seconds() {
        let mut req = request_with(json!({}));
        req.method = "seeing.capture_sequence".into();
        assert_eq!(timeout_for(&req), Duration::from_secs(60));
        req.method = "seeing.contact_sheet".into();
        assert_eq!(timeout_for(&req), Duration::from_secs(60));
    }

    /// Seeded fuzz of the frame parser: random bytes, truncations and mutations of a valid request,
    /// and deep nesting never panic (or overflow the stack); every refusal is `F0005` with a null
    /// id.
    #[test]
    fn frame_parsing_survives_hostile_text() {
        let valid = r#"{"jsonrpc":"2.0","id":7,"method":"agent.hello","params":{"client":"fuzz"}}"#;
        assert!(parse_request(valid).is_ok());
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };
        let alphabet: &[u8] = br#"{}[]":,0123456789.eE+-truefalsnul \u00e9\"jsonrpcidmethodparams"#;
        let mut cases: Vec<String> = vec![
            String::new(),
            "null".into(),
            "[]".into(),
            "[{}]".into(),
            r#"{"jsonrpc":"2.0"}"#.into(),
            r#"{"jsonrpc":"2.0","method":5}"#.into(),
            r#"{"jsonrpc":"2.0","method":"x","id":1e999}"#.into(),
            format!(
                r#"{{"jsonrpc":"2.0","method":"x","params":{}{}}}"#,
                "[".repeat(100_000),
                "]".repeat(100_000)
            ),
            format!(
                r#"{{"jsonrpc":"2.0","method":"x","params":{}1{}}}"#,
                "{\"a\":".repeat(10_000),
                "}".repeat(10_000)
            ),
            "\u{feff}{}".into(),
        ];
        for cut in 0..valid.len() {
            if valid.is_char_boundary(cut) {
                cases.push(valid[..cut].to_string());
            }
        }
        for _ in 0..2000 {
            let len = (next() % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            cases.push(String::from_utf8_lossy(&bytes).into_owned());
            let mut mutated = valid.as_bytes().to_vec();
            let at = (next() % mutated.len() as u64) as usize;
            mutated[at] = alphabet[(next() % alphabet.len() as u64) as usize];
            cases.push(String::from_utf8_lossy(&mutated).into_owned());
        }
        for text in &cases {
            // The bounded parse answers exactly what a plain parse into the derived shape answers.
            let plain = serde_json::from_str::<Request>(text).ok();
            let bounded = parse_request(text).ok();
            assert_eq!(
                plain.and_then(|r| serde_json::to_string(&r).ok()),
                bounded.and_then(|r| serde_json::to_string(&r).ok()),
                "{text:?}"
            );
            match parse_request(text) {
                Ok(_) => {}
                Err(response) => {
                    let response = *response;
                    assert_eq!(response.id, Value::Null);
                    let error = response.error.expect("an error response");
                    assert_eq!(error.code, protocol::PARSE_ERROR);
                    assert_eq!(
                        error.data.as_ref().and_then(|d| d["code"].as_str()),
                        Some("F0005")
                    );
                }
            }
        }
    }

    /// A request whose parse would pass the budget answers `request_too_large` (INVALID_REQUEST,
    /// null id) instead of being built (audit zai M2, ADR-2306); one byte of budget more than it
    /// needs parses it.
    #[test]
    fn a_request_over_the_parse_budget_is_refused() {
        let items = 1000;
        let text = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"play.step","params":{{"list":[{}0]}}}}"#,
            "0,".repeat(items - 1)
        );
        let refused = *parse_request_within(&text, 8 * 1024).expect_err("over 8 KiB parsed");
        assert_eq!(refused.id, Value::Null);
        let error = refused.error.expect("an error");
        assert_eq!(error.code, protocol::INVALID_REQUEST);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some(REQUEST_TOO_LARGE)
        );
        let request = parse_request_within(&text, 1024 * 1024).expect("within 1 MiB");
        assert_eq!(request.params["list"].as_array().map(Vec::len), Some(items));
        // Strings and keys count their bytes.
        let long = format!(
            r#"{{"jsonrpc":"2.0","method":"x","params":{{"s":"{}"}}}}"#,
            "a".repeat(20_000)
        );
        assert!(parse_request_within(&long, 16 * 1024).is_err());
        assert!(parse_request_within(&long, 64 * 1024).is_ok());
    }

    /// At the real budget: a message of 4.5 million zeros (9 MB of text, well under the 32 MiB
    /// message cap) would build about 144 MB of values and is refused; the largest array a contract
    /// allows (1,048,576 numbers) still parses.
    #[test]
    fn the_real_budget_refuses_amplification_and_keeps_the_largest_contract_call() {
        let zeros = |n: usize| {
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"x","params":{{"heights":[{}0]}}}}"#,
                "0,".repeat(n - 1)
            )
        };
        let refused = *parse_request(&zeros(4_500_000)).expect_err("amplified");
        assert_eq!(
            refused
                .error
                .and_then(|e| e.data)
                .and_then(|d| d["code"].as_str().map(str::to_owned)),
            Some(REQUEST_TOO_LARGE.to_owned())
        );
        let kept = parse_request(&zeros(1_048_576)).expect("the contract's largest array");
        assert_eq!(kept.params["heights"].as_array().map(Vec::len), Some(1_048_576));
    }

    /// A repeated envelope field is refused as before; a repeated key elsewhere keeps the last value.
    #[test]
    fn repeated_keys_behave_as_the_plain_parse() {
        assert!(parse_request(r#"{"jsonrpc":"2.0","method":"a","method":"b"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"jsonrpc":"2.0","method":"a","method":"b"}"#).is_err());
        let request = parse_request(r#"{"jsonrpc":"2.0","method":"a","params":{"k":1,"k":2},"x":1,"x":2}"#)
            .expect("inner repeats are fine");
        assert_eq!(request.params["k"], 2);
    }

    use tungstenite::client::IntoClientRequest;

    /// Opens a raw WebSocket to the server with the bearer token (no JSON-RPC framing), so tests
    /// can send arbitrary malformed messages.
    fn raw_connect(url: &str, token: &str) -> tungstenite::WebSocket<TcpStream> {
        let mut req = url.into_client_request().expect("client request");
        let auth: tungstenite::http::HeaderValue = format!("Bearer {token}").parse().expect("auth");
        req.headers_mut().insert("authorization", auth);
        let host = req.uri().host().unwrap_or("127.0.0.1").to_string();
        let port = req.uri().port_u16().unwrap_or(80);
        let stream = TcpStream::connect((host.as_str(), port)).expect("tcp");
        let (socket, _) = tungstenite::client(req, stream).expect("handshake");
        socket
    }

    /// Sends `text` and reads the next text response.
    fn send_text(socket: &mut tungstenite::WebSocket<TcpStream>, text: &str) -> Response {
        socket.send(Message::Text(text.into())).expect("send");
        match socket.read() {
            Ok(Message::Text(t)) => serde_json::from_str(t.as_str()).expect("valid JSON-RPC response"),
            other => panic!("expected a text response, got {other:?}"),
        }
    }

    /// A progress poll on an inbox with one free place, over a real connection.
    fn serve_on_small_inbox(
        board: RenderOutBoard,
    ) -> (std::net::SocketAddr, Sender<Inbound>, Receiver<Inbound>) {
        let (inbox, drained) = bounded(1);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let conn_inbox = inbox.clone();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve_connection(stream, "secret", &conn_inbox, Some(&board));
            }
        });
        (addr, inbox, drained)
    }

    fn filler() -> Inbound {
        let (reply, _) = bounded(1);
        let request = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"agent.ping","params":{}}"#)
            .expect("well-formed");
        Inbound { request, reply }
    }

    /// ADR-2728: a poll finds a full inbox while the app thread is inside a long step (a stalled
    /// output, its inbox full of polls answered earlier): the board answers it, not `F0006`.
    #[test]
    fn a_full_inbox_does_not_keep_a_poll_from_the_board() {
        let board = RenderOutBoard::default();
        board.enter(
            4,
            serde_json::json!({ "job": 4, "status": "running", "frame": 3 }),
            &[],
            true,
        );
        let (addr, inbox, drained) = serve_on_small_inbox(board.clone());
        inbox.try_send(filler()).map_err(|_| ()).expect("fills the inbox");
        let mut socket = raw_connect(&format!("ws://{addr}/"), "secret");
        let response = send_text(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":9,"method":"render.out_progress","params":{"job":4}}"#,
        );
        let result = response.result.expect("answered from the board");
        assert_eq!(result["app_busy"], serde_json::json!(true), "{result}");
        assert_eq!(result["frame"], serde_json::json!(3), "{result}");
        assert_eq!(drained.len(), 1, "only the filler is queued");
        // Off the board, a full inbox is the app's to drain: the poll gets F0006 as any request.
        board.leave();
        let response = send_text(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":10,"method":"render.out_progress","params":{"timeout_ms":1}}"#,
        );
        let error = response.error.expect("no place");
        assert_eq!(error.data.expect("data")["code"], serde_json::json!("F0006"));
    }

    /// ADR-2728: a cancel on a full inbox wakes the app thread before it waits for a place: that
    /// thread, stalled on the output, is what would make the place. Other requests do not.
    #[test]
    fn a_cancel_on_a_full_inbox_wakes_the_stalled_app_thread() {
        let (inbox, _drained) = bounded(1);
        let request = |method: &str| Inbound {
            request: parse_request(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{"timeout_ms":1}}}}"#
            ))
            .expect("well-formed"),
            reply: bounded(1).0,
        };
        inbox.try_send(filler()).map_err(|_| ()).expect("fills the inbox");
        let before = crate::render_out::cancel_notes();
        assert!(matches!(
            queue_within(&inbox, request("render.out_cancel"), None),
            Forwarded::Busy(_)
        ));
        assert!(
            crate::render_out::cancel_notes() > before,
            "woken with no place free"
        );
    }

    /// ADR-2728: the board is read again while a poll waits: a long step that begins after the poll
    /// was queued (the frame ran past the first wait with no step running) still answers it.
    #[test]
    fn a_waiting_poll_finds_a_step_that_began_after_its_first_look() {
        let board = RenderOutBoard::default();
        let (keep, reply_rx) = bounded(1);
        let waiting = std::thread::spawn({
            let board = board.clone();
            move || {
                let params = serde_json::json!({});
                await_progress(&reply_rx, &board, &params, Duration::from_secs(600), Some(&keep))
            }
        });
        // The first look found no step.
        while board.reads() == 0 {
            std::thread::yield_now();
        }
        board.enter(
            4,
            serde_json::json!({ "job": 4, "status": "running", "frame": 0 }),
            &[],
            true,
        );
        match waiting.join().expect("the wait ends") {
            Some(ProgressAnswer::Board(answer)) => {
                assert_eq!(answer["app_busy"], serde_json::json!(true), "{answer}");
            }
            Some(ProgressAnswer::App(_)) => panic!("the app never answered"),
            None => panic!("the poll timed out"),
        }
    }

    /// ADR-2582: while the board says the app thread is inside a render-out step, a progress poll
    /// the world has not taken is answered from the board, and the queued request is marked so the
    /// world skips it. Here the world never drains its inbox at all.
    #[test]
    fn a_progress_poll_is_answered_from_a_busy_board_and_marked_for_the_world() {
        let server = start("secret".into()).expect("server");
        server.progress.enter(
            4,
            serde_json::json!({ "job": 4, "status": "running", "phase": "warmup", "frame": 0, "warming": true }),
            &[],
            true,
        );
        let mut socket = raw_connect(&format!("ws://{}/", server.addr), "secret");
        let response = send_text(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":9,"method":"render.out_progress","params":{"job":4}}"#,
        );
        let result = response.result.expect("answered from the board");
        assert_eq!(result["app_busy"], serde_json::json!(true), "{result}");
        assert_eq!(result["warming"], serde_json::json!(true), "{result}");
        assert_eq!(response.id, serde_json::json!(9));
        let queued = server
            .inbox
            .try_recv()
            .expect("the poll was queued for the world");
        assert_eq!(take_gone(&queued.reply), Some(Gone::AnsweredFromBoard));
        // Off the board (no step running), a poll waits for the world, which answers it.
        server.progress.leave();
        let world = std::thread::spawn({
            let inbox = server.inbox.clone();
            move || {
                let inbound = inbox.recv().expect("queued");
                let _ = inbound.reply.send(Response::ok(
                    serde_json::json!(10),
                    serde_json::json!({ "from": "world" }),
                ));
            }
        });
        let response = send_text(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":10,"method":"render.out_progress","params":{}}"#,
        );
        assert_eq!(
            response.result.expect("the world's answer")["from"],
            serde_json::json!("world")
        );
        world.join().expect("world");
    }

    #[test]
    fn connection_slots_are_capped_and_released() {
        let live = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| ConnectionSlot::take(&live).expect("under the cap"))
            .collect();
        assert!(ConnectionSlot::take(&live).is_none(), "over the cap");
        assert_eq!(live.load(Ordering::Acquire), MAX_CONNECTIONS);
        drop(slots);
        assert!(ConnectionSlot::take(&live).is_some(), "slots free on drop");
    }

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer ab"));
    }

    #[test]
    fn rejects_bad_token_and_answers_good_one() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        // Wrong token: handshake fails.
        assert!(crate::client::Client::connect(&url, "wrong").is_err());
        // Right token: a request reaches the inbox and the reply comes back.
        let inbox = server.inbox.clone();
        let echo = std::thread::spawn(move || {
            if let Ok(inbound) = inbox.recv_timeout(Duration::from_secs(5)) {
                let id = inbound.request.id.clone().unwrap_or(Value::Null);
                let _ = inbound.reply.send(Response::ok(
                    id,
                    serde_json::json!({ "method": inbound.request.method }),
                ));
            }
        });
        let mut client = crate::client::Client::connect(&url, "secret").expect("connects");
        let result = client
            .call("agent.hello", serde_json::json!({}))
            .expect("answered");
        assert_eq!(result["method"], "agent.hello");
        let _ = echo.join();
    }

    /// ADR-2396: a queued `render.out_cancel` is noted before the caller waits for its answer, so
    /// an app thread waiting on a stalled render-out output stops waiting and handles it.
    #[test]
    fn a_queued_render_out_cancel_is_noted_for_a_waiting_app_thread() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let inbox = server.inbox.clone();
        let echo = std::thread::spawn(move || {
            if let Ok(inbound) = inbox.recv_timeout(Duration::from_secs(30)) {
                let id = inbound.request.id.clone().unwrap_or(Value::Null);
                let _ = inbound.reply.send(Response::ok(id, serde_json::json!({})));
            }
        });
        let before = crate::render_out::cancel_notes();
        let mut client = crate::client::Client::connect(&url, "secret").expect("connects");
        client
            .call("render.out_cancel", serde_json::json!({}))
            .expect("answered");
        assert!(crate::render_out::cancel_notes() > before);
        let _ = echo.join();
    }

    #[test]
    fn malformed_json_is_answered_with_a_parse_error() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let mut socket = raw_connect(&url, "secret");
        let response = send_text(&mut socket, "{ not json");
        let error = response.error.expect("an error response");
        assert_eq!(error.code, protocol::PARSE_ERROR);
    }

    #[test]
    fn non_object_json_is_a_parse_error() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let mut socket = raw_connect(&url, "secret");
        for junk in ["[1, 2, 3]", "\"a string\"", "42", "null"] {
            let response = send_text(&mut socket, junk);
            let error = response.error.expect("an error response");
            assert_eq!(error.code, protocol::PARSE_ERROR, "{junk}");
        }
    }

    #[test]
    fn missing_method_field_is_a_parse_error() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let mut socket = raw_connect(&url, "secret");
        let response = send_text(&mut socket, r#"{"jsonrpc":"2.0","id":1}"#);
        let error = response.error.expect("an error response");
        assert_eq!(error.code, protocol::PARSE_ERROR);
    }

    #[test]
    fn binary_message_is_rejected() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let mut socket = raw_connect(&url, "secret");
        socket.send(Message::Binary(vec![1, 2, 3].into())).expect("send");
        let response = match socket.read() {
            Ok(Message::Text(t)) => serde_json::from_str::<Response>(t.as_str()).expect("response"),
            other => panic!("expected a text response, got {other:?}"),
        };
        let error = response.error.expect("an error response");
        assert_eq!(error.code, protocol::INVALID_REQUEST);
    }

    fn tool_scope() -> ToolScope {
        ToolScope::new(
            "fixture",
            [crate::schema::Permission::Read],
            vec!["agent.*".to_string()],
        )
    }

    /// Answers one scoped request with the caller's tool name (or never, when `inbox` closes).
    fn echo_scoped(inbox: Receiver<ScopedInbound>) -> std::thread::JoinHandle<Option<String>> {
        std::thread::spawn(move || {
            let inbound = inbox.recv_timeout(Duration::from_secs(30)).ok()?;
            let id = inbound.request.id.clone().unwrap_or(Value::Null);
            let tool = inbound.scope.tool.clone();
            let _ = inbound.reply.send(Response::ok(
                id,
                serde_json::json!({ "tool": tool, "method": inbound.request.method }),
            ));
            Some(tool)
        })
    }

    /// A live scoped token connects and its requests reach the scoped inbox with the scope; the
    /// master token, a wrong token and no token are refused at the handshake; the wake hook runs
    /// once per queued request.
    #[test]
    fn scoped_tokens_connect_with_their_scope_and_wake_the_host() {
        let tokens = ToolTokens::new();
        let (token, scope) = tokens.issue(tool_scope());
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        let wake: WakeHook = Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        });
        let server = start_scoped(tokens.clone(), Some(wake)).expect("scoped server starts");
        let url = format!("ws://{}", server.addr);
        assert!(crate::client::Client::connect(&url, "wrong").is_err());
        assert!(crate::client::Client::connect(&url, "").is_err());
        let master = start("master".into()).expect("master server starts");
        assert!(
            crate::client::Client::connect(&url, "master").is_err(),
            "the master token does not open the scoped listener"
        );
        assert!(
            crate::client::Client::connect(&format!("ws://{}", master.addr), &token).is_err(),
            "a scoped token does not open the master listener"
        );

        let echo = echo_scoped(server.inbox.clone());
        let mut client = crate::client::Client::connect(&url, &token).expect("scoped token connects");
        let result = client
            .call("agent.hello", serde_json::json!({}))
            .expect("answered");
        assert_eq!(result["tool"], "fixture");
        assert_eq!(result["method"], "agent.hello");
        assert_eq!(echo.join().ok().flatten().as_deref(), Some("fixture"));
        assert_eq!(woken.load(Ordering::Acquire), 1, "one queued request, one wake");
        assert!(tokens.is_live(scope.id));
    }

    /// ADR-7393: a request the master listener queues runs the app wake hook, so a host whose
    /// event loop sleeps (the editor's 1 Hz idle keep-alive) reads it on the next frame. Other
    /// tests' servers may wake the hook while it is set, so the count is a lower bound.
    #[test]
    fn a_queued_master_request_wakes_the_app_host() {
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        set_app_wake(Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        })));
        let server = start("app-wake".into()).expect("server starts");
        let inbox = server.inbox.clone();
        let answer = std::thread::spawn(move || {
            let inbound = inbox.recv_timeout(Duration::from_secs(30)).ok()?;
            let id = inbound.request.id.clone().unwrap_or(Value::Null);
            inbound.reply.send(Response::ok(id, serde_json::json!({}))).ok()
        });
        let mut client =
            crate::client::Client::connect(&format!("ws://{}", server.addr), "app-wake").expect("connects");
        client
            .call("agent.hello", serde_json::json!({}))
            .expect("answered");
        set_app_wake(None);
        assert!(answer.join().ok().flatten().is_some(), "the world answered");
        assert!(
            woken.load(Ordering::Acquire) >= 1,
            "the queued request woke the app host"
        );
    }

    /// Revoking a token refuses the connection's next request with `tool_token_revoked` before
    /// it reaches the world, and closes the connection; a new handshake with it is refused.
    #[test]
    fn a_revoked_token_is_refused_on_its_next_request() {
        let tokens = ToolTokens::new();
        let (token, scope) = tokens.issue(tool_scope());
        let server = start_scoped(tokens.clone(), None).expect("scoped server starts");
        let url = format!("ws://{}", server.addr);
        let mut client = crate::client::Client::connect(&url, &token).expect("connects");
        assert!(tokens.revoke(scope.id));
        let Err(crate::client::ClientError::Rpc(error)) = client.call("agent.hello", serde_json::json!({}))
        else {
            panic!("a revoked token must be refused");
        };
        assert_eq!(error.code, protocol::PERMISSION_DENIED);
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some(crate::scope::TOOL_TOKEN_REVOKED)
        );
        assert!(server.inbox.try_recv().is_err(), "nothing reached the world");
        assert!(crate::client::Client::connect(&url, &token).is_err());
    }

    #[test]
    fn bearer_needs_the_prefix_and_no_origin() {
        let build = |auth: &str, origin: bool| {
            let mut builder = HttpRequest::builder().header("authorization", auth);
            if origin {
                builder = builder.header("origin", "http://page.test");
            }
            builder.body(()).expect("request")
        };
        assert_eq!(bearer(&build("Bearer abc", false)), Some("abc"));
        assert_eq!(bearer(&build("abc", false)), None);
        assert_eq!(bearer(&build("Bearer abc", true)), None);
    }

    #[test]
    fn a_fragmented_reply_reassembles_into_the_same_text() {
        // ADR-8848: a pooled reply goes out as one text message in fixed-size frames; a frame
        // boundary may split a multi-byte character (only the whole message must be UTF-8).
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let text: String = (0..700).map(|i| format!("{i}:é∑;")).collect();
        let payload = tungstenite::Bytes::from(text.clone().into_bytes());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut socket = tungstenite::accept(stream).expect("handshake");
            send_text_fragments(&mut socket, &payload, 1000).expect("sent");
            send_text_fragments(&mut socket, &tungstenite::Bytes::from_static(b"{}"), 1000).expect("sent");
        });
        let stream = TcpStream::connect(addr).expect("tcp");
        let (mut socket, _) = tungstenite::client(format!("ws://{addr}/"), stream).expect("handshake");
        match socket.read() {
            Ok(Message::Text(got)) => assert!(got.as_str() == text, "the message reassembles unchanged"),
            other => panic!("expected the text message, got {other:?}"),
        }
        match socket.read() {
            Ok(Message::Text(got)) => assert_eq!(got.as_str(), "{}", "a one-frame message after it"),
            other => panic!("expected the second message, got {other:?}"),
        }
        server.join().expect("server thread");
    }

    #[test]
    fn only_replies_with_large_strings_take_the_pooled_path() {
        let small = Response::ok(json!(1), json!({ "ok": true }));
        assert!(reply_size_hint(&small) < POOLED_REPLY_MIN);
        let shot = Response::ok(
            json!(1),
            json!({ "width": 4, "mime": "image/png", "png_base64": "A".repeat(POOLED_REPLY_MIN) }),
        );
        assert!(reply_size_hint(&shot) > POOLED_REPLY_MIN + "image/png".len());
        let error = Response::err(json!(1), RpcError::internal("x".repeat(POOLED_REPLY_MIN)));
        assert!(
            reply_size_hint(&error) < POOLED_REPLY_MIN,
            "errors are small replies"
        );
    }

    #[test]
    fn a_large_payload_does_not_crash_the_server() {
        let server = start("secret".into()).expect("server starts");
        let url = format!("ws://{}", server.addr);
        let mut socket = raw_connect(&url, "secret");
        // A 1 MiB non-JSON payload must be answered with a parse error, not crash the listener.
        let blob = "a".repeat(1024 * 1024);
        let response = send_text(&mut socket, &blob);
        let error = response.error.expect("an error response");
        assert_eq!(error.code, protocol::PARSE_ERROR);
    }
}
