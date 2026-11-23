// SPDX-License-Identifier: MPL-2.0
//! The session state machine (`specs/60` § 2.1) and the hello checks (§ 4.4, § 9.1).
//!
//! Pure data: reader threads and the main thread call it behind the server's lock, and every
//! timer reads engine time passed in by the caller (the fixed step during replay), so tests drive
//! it with numbers.

use std::collections::{BTreeMap, HashMap};

use super::wire::{ErrorCode, Hello, HelloAck, Limits, PROTOCOL_VERSION, WireError};

/// Session id (unique per server run, starting at 1).
pub type SessionId = u64;

/// Seconds a session may stay `negotiating` before it is closed (spec § 2.1).
pub const NEGOTIATE_TIMEOUT_S: f64 = 5.0;
/// Seconds a session may stay `pending` before it is closed.
pub const PENDING_TIMEOUT_S: f64 = 120.0;
/// Seconds a `closed` session record stays visible to `dcc_feed.status`.
pub const CLOSED_KEEP_S: f64 = 60.0;
/// Failed token checks that lock a peer out (spec § 9.1).
pub const AUTH_FAILURES_LOCKOUT: u32 = 5;
/// Seconds a locked-out peer is refused.
pub const LOCKOUT_S: f64 = 60.0;
/// Minimum seconds between two hello attempts of one peer.
pub const HELLO_INTERVAL_S: f64 = 1.0;

/// Session states; the discriminant is the `session_state.state` byte (spec § 2.1 index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Transport accepted; only `hello` is allowed.
    Negotiating = 0,
    /// Hello valid, waiting for approval; data frames are discarded and counted.
    Pending = 1,
    /// Frames are applied.
    Active = 2,
    /// Going away: the inbox is discarded and items detach on the next tick.
    Closing = 3,
    /// Terminal; the record stays [`CLOSED_KEEP_S`] seconds.
    Closed = 4,
}

impl Phase {
    /// The lowercase name used by `dcc_feed.status`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Negotiating => "negotiating",
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Closing => "closing",
            Self::Closed => "closed",
        }
    }
}

/// How a session reached the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    /// The 0600 Unix domain socket: OS permissions are the check, no token.
    Unix,
    /// Loopback (or allowed remote) TCP: token required.
    Tcp,
    /// WebSocket, binary or JSON: token required.
    WebSocket,
    /// A `.rfdrec` replayed by `dcc_feed.replay`: no socket, no token.
    Replay,
}

impl Transport {
    /// The lowercase name used by `dcc_feed.status`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unix => "unix",
            Self::Tcp => "tcp",
            Self::WebSocket => "websocket",
            Self::Replay => "replay",
        }
    }

    /// Whether the hello must carry the session token.
    #[must_use]
    pub fn needs_token(self) -> bool {
        matches!(self, Self::Tcp | Self::WebSocket)
    }
}

/// One session as `dcc_feed.status` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    /// Session id.
    pub id: SessionId,
    /// Transport.
    pub transport: Transport,
    /// Peer description (address, `unix` or the replay path).
    pub peer: String,
    /// Current state.
    pub phase: Phase,
    /// Engine time the state was entered.
    pub phase_since_s: f64,
    /// `hello.client_name` (empty before hello).
    pub client: String,
    /// `hello.client_version`.
    pub client_version: String,
    /// `hello.dcc`.
    pub dcc: String,
    /// True when the hello declared `gltf` space (default), false for `regia`.
    pub gltf_space: bool,
    /// `hello.unit_scale`.
    pub unit_scale: f32,
    /// `hello.caps`.
    pub caps: u32,
    /// Negotiated protocol version (0 before hello).
    pub protocol_version: u32,
    /// Data frames discarded because the session was not accepted.
    pub dropped_not_accepted: u64,
    /// Last error code sent or seen.
    pub last_error: Option<String>,
    /// Why the session closed.
    pub close_reason: Option<String>,
}

/// What a refused agent call on a session means (`F0002` `data.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// No session with that id.
    NotFound,
    /// The session is active already.
    AlreadyAccepted,
    /// The session is closing or closed.
    Closed,
    /// The session has not sent its hello yet.
    Negotiating,
}

impl SessionError {
    /// The lowercase reason code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "session_not_found",
            Self::AlreadyAccepted => "already_accepted",
            Self::Closed => "session_closed",
            Self::Negotiating => "session_negotiating",
        }
    }
}

/// The server-side inputs of a hello check.
#[derive(Debug, Clone, Copy)]
pub struct HelloPolicy<'a> {
    /// The session token (`.regia/dcc_feed/token`).
    pub token: &'a str,
    /// Client names accepted without a prompt.
    pub auto_accept: &'a [String],
    /// Most pending plus active sessions.
    pub max_sessions: u32,
    /// Limits sent back in `hello_ack`.
    pub limits: Limits,
    /// Byte window sent back in `hello_ack`.
    pub window_bytes: u32,
    /// `hello_ack.server_name`.
    pub server_name: &'a str,
}

/// Capability bits this receiver understands (`hello.caps`, spec § 4.4).
pub const SERVER_CAPS: u32 = 0b1_1111;

#[derive(Debug, Clone, Copy, Default)]
struct PeerGate {
    last_hello_s: Option<f64>,
    failures: u32,
    locked_until_s: f64,
}

/// What [`SessionTable::tick`] did to one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickEvent {
    /// The session stayed `negotiating` or `pending` too long and is now `closing`.
    TimedOut(SessionId),
    /// A closed record aged out and was dropped.
    Forgotten(SessionId),
}

/// Every session of one server.
#[derive(Debug, Default)]
pub struct SessionTable {
    next_id: SessionId,
    sessions: BTreeMap<SessionId, SessionRecord>,
    peers: HashMap<String, PeerGate>,
}

impl SessionTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A new `negotiating` session for a transport the server just accepted.
    pub fn open(&mut self, transport: Transport, peer: impl Into<String>, now_s: f64) -> SessionId {
        self.next_id += 1;
        let id = self.next_id;
        self.sessions.insert(
            id,
            SessionRecord {
                id,
                transport,
                peer: peer.into(),
                phase: Phase::Negotiating,
                phase_since_s: now_s,
                client: String::new(),
                client_version: String::new(),
                dcc: String::new(),
                gltf_space: true,
                unit_scale: 1.0,
                caps: 0,
                protocol_version: 0,
                dropped_not_accepted: 0,
                last_error: None,
                close_reason: None,
            },
        );
        id
    }

    /// The session `id`.
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<&SessionRecord> {
        self.sessions.get(&id)
    }

    /// Every session, by id.
    pub fn iter(&self) -> impl Iterator<Item = &SessionRecord> {
        self.sessions.values()
    }

    /// The state of `id`, if it exists.
    #[must_use]
    pub fn phase(&self, id: SessionId) -> Option<Phase> {
        self.sessions.get(&id).map(|s| s.phase)
    }

    fn set_phase(&mut self, id: SessionId, phase: Phase, now_s: f64) {
        if let Some(s) = self.sessions.get_mut(&id)
            && s.phase != phase
        {
            s.phase = phase;
            s.phase_since_s = now_s;
        }
    }

    fn live_count(&self, except: SessionId) -> usize {
        self.sessions
            .values()
            .filter(|s| s.id != except && matches!(s.phase, Phase::Pending | Phase::Active))
            .count()
    }

    /// Checks a hello (spec § 4.4, § 9.1) and moves the session to `pending` or `active`.
    /// Returns the `hello_ack` to send.
    ///
    /// # Errors
    ///
    /// Every hello refusal is fatal; the caller sends the error and closes the session:
    /// `malformed_frame` (a second hello), `unsupported_version` (with the supported range in the
    /// message), `auth_failed` (wrong or missing token, or the peer is locked out),
    /// `overloaded` (more than one hello per second from one peer), `invalid_value` (space or
    /// unit scale), `limit_exceeded` (`max_sessions`).
    pub fn hello(
        &mut self,
        id: SessionId,
        hello: &Hello,
        policy: &HelloPolicy<'_>,
        now_s: f64,
    ) -> Result<HelloAck, WireError> {
        let Some(record) = self.sessions.get(&id) else {
            return Err(WireError::new(ErrorCode::Internal, "unknown session"));
        };
        if record.phase != Phase::Negotiating {
            return Err(WireError::new(
                ErrorCode::MalformedFrame,
                "hello is only allowed once, first",
            ));
        }
        let transport = record.transport;
        let peer = record.peer.clone();
        // Rate limit and lockout guard the token, so a transport without one has neither: a tool
        // reconnecting on its own 0600 socket is not an attack, and a replay has no peer.
        if transport.needs_token() {
            let gate = self.peers.entry(peer.clone()).or_default();
            if now_s < gate.locked_until_s {
                return Err(WireError::new(
                    ErrorCode::AuthFailed,
                    "too many failed token checks; try again later",
                ));
            }
            if let Some(last) = gate.last_hello_s
                && now_s - last < HELLO_INTERVAL_S
            {
                return Err(WireError::new(
                    ErrorCode::Overloaded,
                    "one hello per second per peer",
                ));
            }
            gate.last_hello_s = Some(now_s);
        }
        if hello.min_version > PROTOCOL_VERSION || hello.max_version < PROTOCOL_VERSION {
            return Err(WireError::new(
                ErrorCode::UnsupportedVersion,
                format!("server supports {PROTOCOL_VERSION}..={PROTOCOL_VERSION}"),
            ));
        }
        if transport.needs_token() && !constant_time_eq(hello.token.as_bytes(), policy.token.as_bytes()) {
            let gate = self.peers.entry(peer).or_default();
            gate.failures += 1;
            if gate.failures >= AUTH_FAILURES_LOCKOUT {
                gate.failures = 0;
                gate.locked_until_s = now_s + LOCKOUT_S;
            }
            return Err(WireError::new(ErrorCode::AuthFailed, "missing or wrong token"));
        }
        let gltf_space = match hello.space.as_str() {
            "" | "gltf" => true,
            "regia" => false,
            other => {
                return Err(WireError::new(
                    ErrorCode::InvalidValue,
                    format!("space {other:?}: expected gltf or regia"),
                ));
            }
        };
        if !(hello.unit_scale.is_finite() && hello.unit_scale > 0.0) {
            return Err(WireError::new(
                ErrorCode::InvalidValue,
                "unit_scale must be finite and positive",
            ));
        }
        if self.live_count(id) >= policy.max_sessions as usize {
            return Err(WireError::new(
                ErrorCode::LimitExceeded,
                format!("max_sessions ({}) reached", policy.max_sessions),
            ));
        }
        if let Some(gate) = self.peers.get_mut(&peer) {
            gate.failures = 0;
        }
        let accepted = policy.auto_accept.contains(&hello.client_name);
        let Some(record) = self.sessions.get_mut(&id) else {
            return Err(WireError::new(ErrorCode::Internal, "unknown session"));
        };
        record.client = hello.client_name.clone();
        record.client_version = hello.client_version.clone();
        record.dcc = hello.dcc.clone();
        record.gltf_space = gltf_space;
        record.unit_scale = hello.unit_scale;
        record.caps = hello.caps & SERVER_CAPS;
        record.protocol_version = PROTOCOL_VERSION;
        let phase = if accepted { Phase::Active } else { Phase::Pending };
        self.set_phase(id, phase, now_s);
        Ok(HelloAck {
            version: PROTOCOL_VERSION,
            session_id: id,
            accepted,
            caps: hello.caps & SERVER_CAPS,
            limits: policy.limits,
            window_bytes: policy.window_bytes,
            server_name: policy.server_name.to_owned(),
        })
    }

    /// Accepts a `pending` session (`dcc_feed.accept`, the editor prompt).
    ///
    /// # Errors
    ///
    /// [`SessionError`] for a missing, negotiating, active or closed session.
    pub fn accept(&mut self, id: SessionId, now_s: f64) -> Result<(), SessionError> {
        match self.phase(id) {
            None => Err(SessionError::NotFound),
            Some(Phase::Negotiating) => Err(SessionError::Negotiating),
            Some(Phase::Active) => Err(SessionError::AlreadyAccepted),
            Some(Phase::Closing | Phase::Closed) => Err(SessionError::Closed),
            Some(Phase::Pending) => {
                self.set_phase(id, Phase::Active, now_s);
                Ok(())
            }
        }
    }

    /// Counts one data frame discarded because the session is not accepted.
    pub fn count_not_accepted(&mut self, id: SessionId) {
        if let Some(s) = self.sessions.get_mut(&id) {
            s.dropped_not_accepted += 1;
        }
    }

    /// Records the last error code of a session.
    pub fn note_error(&mut self, id: SessionId, code: &str) {
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_error = Some(code.to_owned());
        }
    }

    /// Starts closing `id` (goodbye, socket error, `dcc_feed.disconnect`, fatal error).
    /// Returns false when the session is missing or already closing or closed.
    pub fn begin_close(&mut self, id: SessionId, reason: &str, now_s: f64) -> bool {
        match self.sessions.get_mut(&id) {
            Some(s) if !matches!(s.phase, Phase::Closing | Phase::Closed) => {
                s.close_reason = Some(reason.to_owned());
                self.set_phase(id, Phase::Closing, now_s);
                true
            }
            _ => false,
        }
    }

    /// Sessions in `closing`, which the main thread finalizes (detach items, discard inbox).
    #[must_use]
    pub fn closing(&self) -> Vec<SessionId> {
        self.sessions
            .values()
            .filter(|s| s.phase == Phase::Closing)
            .map(|s| s.id)
            .collect()
    }

    /// Moves a `closing` session to `closed`.
    pub fn finish_close(&mut self, id: SessionId, now_s: f64) {
        if self.phase(id) == Some(Phase::Closing) {
            self.set_phase(id, Phase::Closed, now_s);
        }
    }

    /// Runs the engine-time timers: negotiate and pending timeouts (to `closing`), and the
    /// expiry of closed records.
    pub fn tick(&mut self, now_s: f64) -> Vec<TickEvent> {
        let mut events = Vec::new();
        let mut forget = Vec::new();
        for s in self.sessions.values_mut() {
            let age = now_s - s.phase_since_s;
            let timeout = match s.phase {
                Phase::Negotiating => age >= NEGOTIATE_TIMEOUT_S,
                Phase::Pending => age >= PENDING_TIMEOUT_S,
                Phase::Closed => {
                    if age >= CLOSED_KEEP_S {
                        forget.push(s.id);
                    }
                    false
                }
                _ => false,
            };
            if timeout {
                s.phase = Phase::Closing;
                s.phase_since_s = now_s;
                s.close_reason = Some("timeout".to_owned());
                s.last_error = Some("timeout".to_owned());
                events.push(TickEvent::TimedOut(s.id));
            }
        }
        for id in forget {
            self.sessions.remove(&id);
            events.push(TickEvent::Forgotten(id));
        }
        events
    }
}

/// Compares two byte strings in time that depends only on their lengths.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(client: &str, token: &str) -> Hello {
        Hello {
            min_version: 1,
            max_version: 1,
            client_name: client.to_owned(),
            client_version: "1".to_owned(),
            dcc: "test".to_owned(),
            space: "gltf".to_owned(),
            unit_scale: 1.0,
            caps: 0xff,
            token: token.to_owned(),
        }
    }

    fn policy(auto: &[String]) -> HelloPolicy<'_> {
        HelloPolicy {
            token: "secret",
            auto_accept: auto,
            max_sessions: 2,
            limits: Limits::default(),
            window_bytes: 1 << 20,
            server_name: "test",
        }
    }

    #[test]
    fn unknown_client_is_pending_until_accepted() {
        let mut t = SessionTable::new();
        let id = t.open(Transport::Unix, "unix", 0.0);
        let ack = t
            .hello(id, &hello("addon", ""), &policy(&[]), 0.0)
            .expect("hello");
        assert!(!ack.accepted);
        assert_eq!(ack.session_id, id);
        assert_eq!(ack.caps, SERVER_CAPS);
        assert_eq!(t.phase(id), Some(Phase::Pending));
        assert_eq!(t.accept(id, 1.0), Ok(()));
        assert_eq!(t.phase(id), Some(Phase::Active));
        assert_eq!(t.accept(id, 1.0), Err(SessionError::AlreadyAccepted));
        assert_eq!(t.accept(99, 1.0), Err(SessionError::NotFound));
    }

    #[test]
    fn allow_listed_client_is_active_at_once() {
        let mut t = SessionTable::new();
        let id = t.open(Transport::Unix, "unix", 0.0);
        let auto = vec!["addon".to_owned()];
        let ack = t
            .hello(id, &hello("addon", ""), &policy(&auto), 0.0)
            .expect("hello");
        assert!(ack.accepted);
        assert_eq!(t.phase(id), Some(Phase::Active));
    }

    #[test]
    fn version_range_is_checked() {
        let mut t = SessionTable::new();
        let id = t.open(Transport::Unix, "unix", 0.0);
        let mut h = hello("a", "");
        h.min_version = 2;
        h.max_version = 3;
        let err = t.hello(id, &h, &policy(&[]), 0.0).expect_err("refused");
        assert_eq!(err.code(), "unsupported_version");
        assert!(err.message.contains("1..=1"), "{}", err.message);
    }

    #[test]
    fn tcp_needs_the_token_and_locks_out_after_five_failures() {
        let mut t = SessionTable::new();
        let mut now = 0.0;
        for _ in 0..AUTH_FAILURES_LOCKOUT {
            let id = t.open(Transport::Tcp, "127.0.0.1:5000", now);
            let err = t
                .hello(id, &hello("a", "wrong"), &policy(&[]), now)
                .expect_err("auth");
            assert_eq!(err.code(), "auth_failed");
            now += 1.0;
        }
        // Locked out even with the right token.
        let id = t.open(Transport::Tcp, "127.0.0.1:5000", now);
        let err = t
            .hello(id, &hello("a", "secret"), &policy(&[]), now)
            .expect_err("locked");
        assert_eq!(err.code(), "auth_failed");
        // After the lockout the right token works.
        now += LOCKOUT_S;
        let id = t.open(Transport::Tcp, "127.0.0.1:5000", now);
        t.hello(id, &hello("a", "secret"), &policy(&[]), now)
            .expect("ok after lockout");
        // A Unix session needs no token.
        let id = t.open(Transport::Unix, "unix", now);
        t.hello(id, &hello("b", ""), &policy(&[]), now).expect("unix");
    }

    #[test]
    fn hello_rate_is_one_per_second_per_peer() {
        let mut t = SessionTable::new();
        let a = t.open(Transport::Tcp, "p", 0.0);
        t.hello(a, &hello("a", "secret"), &policy(&[]), 0.0)
            .expect("first");
        let b = t.open(Transport::Tcp, "p", 0.5);
        let err = t
            .hello(b, &hello("a", "secret"), &policy(&[]), 0.5)
            .expect_err("rate");
        assert_eq!(err.code(), "overloaded");
    }

    #[test]
    fn second_hello_space_scale_and_session_cap_are_refused() {
        let mut t = SessionTable::new();
        let a = t.open(Transport::Unix, "u1", 0.0);
        t.hello(a, &hello("a", ""), &policy(&[]), 0.0).expect("a");
        assert_eq!(
            t.hello(a, &hello("a", ""), &policy(&[]), 2.0)
                .expect_err("twice")
                .code(),
            "malformed_frame"
        );
        let b = t.open(Transport::Unix, "u2", 0.0);
        let mut h = hello("b", "");
        h.space = "zup".to_owned();
        assert_eq!(
            t.hello(b, &h, &policy(&[]), 0.0).expect_err("space").code(),
            "invalid_value"
        );
        let c = t.open(Transport::Unix, "u3", 0.0);
        let mut h = hello("c", "");
        h.unit_scale = f32::NAN;
        assert_eq!(
            t.hello(c, &h, &policy(&[]), 0.0).expect_err("scale").code(),
            "invalid_value"
        );
        let d = t.open(Transport::Unix, "u4", 0.0);
        t.hello(d, &hello("d", ""), &policy(&[]), 0.0)
            .expect("second live session");
        let e = t.open(Transport::Unix, "u5", 0.0);
        assert_eq!(
            t.hello(e, &hello("e", ""), &policy(&[]), 0.0)
                .expect_err("cap")
                .code(),
            "limit_exceeded"
        );
    }

    #[test]
    fn timers_run_on_engine_time() {
        let mut t = SessionTable::new();
        let negotiating = t.open(Transport::Unix, "u1", 0.0);
        let pending = t.open(Transport::Unix, "u2", 0.0);
        t.hello(pending, &hello("p", ""), &policy(&[]), 0.0)
            .expect("hello");
        assert!(t.tick(NEGOTIATE_TIMEOUT_S - 0.01).is_empty());
        assert_eq!(
            t.tick(NEGOTIATE_TIMEOUT_S),
            vec![TickEvent::TimedOut(negotiating)]
        );
        assert_eq!(t.phase(negotiating), Some(Phase::Closing));
        t.finish_close(negotiating, NEGOTIATE_TIMEOUT_S);
        let closed_at = NEGOTIATE_TIMEOUT_S;
        assert_eq!(
            t.tick(closed_at + CLOSED_KEEP_S),
            vec![TickEvent::Forgotten(negotiating)]
        );
        assert!(t.get(negotiating).is_none());
        assert_eq!(t.tick(PENDING_TIMEOUT_S), vec![TickEvent::TimedOut(pending)]);
    }

    #[test]
    fn close_path_and_accept_refusals() {
        let mut t = SessionTable::new();
        let id = t.open(Transport::Unix, "u", 0.0);
        assert_eq!(t.accept(id, 0.0), Err(SessionError::Negotiating));
        assert!(t.begin_close(id, "goodbye", 0.0));
        assert!(!t.begin_close(id, "again", 0.0));
        assert_eq!(t.closing(), vec![id]);
        assert_eq!(t.accept(id, 0.0), Err(SessionError::Closed));
        t.finish_close(id, 0.0);
        assert_eq!(t.phase(id), Some(Phase::Closed));
        assert_eq!(t.get(id).and_then(|s| s.close_reason.as_deref()), Some("goodbye"));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
