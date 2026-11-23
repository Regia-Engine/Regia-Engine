// SPDX-License-Identifier: MPL-2.0
//! The server core (`specs/60` § 3.2, § 3.4): what every decoded message does to the session
//! table and the inbox, the replies the server owes each session, recording and replay.
//!
//! Socket reader threads and the replay driver call the same [`FeedCore::submit`], so a replay
//! goes through exactly the checks a live sender does. Nothing here opens a socket; writers are
//! plugged in by `net` and replies wait in an outbox until [`FeedCore::flush`], so tests read the
//! outbox instead of a socket.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use super::inbox::{FeedInbox, InboxConfig, InboxEntry, PushOutcome};
use super::recording::{Recording, RecordingError};
use super::session::{HelloPolicy, Phase, SessionId, SessionTable, Transport};
use super::settings::DccFeedSettings;
use super::wire::{
    self, Ack, ErrorCode, ErrorFrame, FrameHeader, Hello, Limits, Message, Ping, Resync, WireError,
};

/// Seconds between two `unknown_item` errors for one handle (spec § 4.8).
const UNKNOWN_ITEM_INTERVAL_S: f64 = 1.0;
/// Clock samples kept per session (spec § 4.6).
const CLOCK_SAMPLES: usize = 16;
/// Most declarations held for one pending session (besides its byte window).
const MAX_HELD_DECLARATIONS: usize = 4096;
/// Whether this build carries mesh, texture and clip items (spec § 9.2).
pub const ASSET_KINDS: bool = cfg!(feature = "dcc-feed-assets");

/// A session's socket side: where replies are written and how to cut the connection.
pub struct SessionWriter {
    /// The write half.
    pub out: Box<dyn Write + Send>,
    /// Shuts the connection down (both halves), which also ends the reader thread.
    pub shutdown: Box<dyn FnMut() + Send>,
}

impl std::fmt::Debug for SessionWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionWriter")
    }
}

/// Encoded reply batches one session's writer thread may hold before the session is closed as
/// not reading (each batch is one `flush_replies`).
const WRITER_QUEUE: usize = 256;

/// What a session's writer thread is asked to do.
enum WriterCmd {
    /// Write and flush these bytes.
    Write(Vec<u8>),
    /// Cut the connection (after every write queued before it).
    Shutdown,
}

/// A session's socket side as the core holds it: a writer thread of its own (audit-v035-conc
/// M7: a client that stops reading must not stall every other session's reader and the main
/// thread for its write timeout while they wait for the core lock), or, when no thread could
/// start, the writer itself (written under the lock, as before).
enum WriterSlot {
    /// The session's writer thread.
    Thread {
        tx: SyncSender<WriterCmd>,
        /// A write failed (or timed out): the session is closed at the next flush.
        failed: Arc<AtomicBool>,
        /// The thread stops writing and cuts the connection at its next command.
        stop: Arc<AtomicBool>,
    },
    /// No thread: written inline.
    Inline(SessionWriter),
}

impl std::fmt::Debug for WriterSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Thread { .. } => "WriterSlot::Thread",
            Self::Inline(_) => "WriterSlot::Inline",
        })
    }
}

impl WriterSlot {
    /// Moves `w` to a writer thread of its own (inline when no thread can start).
    fn spawn(w: SessionWriter) -> Self {
        let (tx, rx) = sync_channel::<WriterCmd>(WRITER_QUEUE);
        let failed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (failed_flag, stop_flag) = (Arc::clone(&failed), Arc::clone(&stop));
        // Parked before the spawn: the thread takes it; if the spawn fails, the closure is dropped
        // and the writer is taken back from here.
        let slot = Arc::new(std::sync::Mutex::new(Some(w)));
        let parked = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("regia-dcc-feed-writer".into())
            .spawn(move || {
                let Some(mut w) = parked.lock().ok().and_then(|mut s| s.take()) else {
                    return;
                };
                drop(parked);
                while let Ok(cmd) = rx.recv() {
                    if stop_flag.load(Ordering::Acquire) {
                        (w.shutdown)();
                        return;
                    }
                    match cmd {
                        WriterCmd::Write(buf) => {
                            if !failed_flag.load(Ordering::Acquire)
                                && w.out.write_all(&buf).and_then(|()| w.out.flush()).is_err()
                            {
                                failed_flag.store(true, Ordering::Release);
                            }
                        }
                        WriterCmd::Shutdown => {
                            (w.shutdown)();
                            return;
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => Self::Thread { tx, failed, stop },
            Err(err) => {
                tracing::warn!(target: "regia_agent", "dcc feed writer thread not started ({err}); replies written inline");
                match slot.lock().ok().and_then(|mut s| s.take()) {
                    Some(w) => Self::Inline(w),
                    // The writer is gone only if the thread took it, which a failed spawn never runs.
                    None => Self::Thread { tx, failed, stop },
                }
            }
        }
    }

    /// Whether a write of this session failed.
    fn failed(&self) -> bool {
        match self {
            Self::Thread { failed, .. } => failed.load(Ordering::Acquire),
            Self::Inline(_) => false,
        }
    }

    /// Hands `buf` to the writer; `Err` names why the session must close.
    fn write(&mut self, buf: Vec<u8>) -> Result<(), &'static str> {
        match self {
            Self::Thread { tx, failed, .. } => {
                if failed.load(Ordering::Acquire) {
                    return Err("socket error");
                }
                match tx.try_send(WriterCmd::Write(buf)) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => Err("client not reading"),
                    Err(TrySendError::Disconnected(_)) => Err("socket error"),
                }
            }
            Self::Inline(w) => w
                .out
                .write_all(&buf)
                .and_then(|()| w.out.flush())
                .map_err(|_| "socket error"),
        }
    }

    /// Cuts the connection after the writes queued so far (at once when the queue is full).
    fn shutdown(self) {
        match self {
            Self::Thread { tx, stop, .. } => {
                if let Err(TrySendError::Full(_)) = tx.try_send(WriterCmd::Shutdown) {
                    // A queue this full belongs to a client that is not reading: stop writing.
                    stop.store(true, Ordering::Release);
                }
            }
            Self::Inline(mut w) => (w.shutdown)(),
        }
    }
}

/// A recording in progress (`dcc_feed.record`).
#[derive(Debug)]
pub struct ActiveRecording {
    /// The session recorded (the first one active, when the caller named none). A recording
    /// holds one sender's frames: handles are session-local.
    pub session: Option<SessionId>,
    /// Where `stop` writes it.
    pub path: PathBuf,
    /// The frames so far.
    pub recording: Recording,
    /// Bytes of the frames recorded so far.
    pub bytes: u64,
    /// Set once a cap was hit: later frames are not recorded (the take so far is still written).
    pub truncated: bool,
}

/// Most frame bytes one recording keeps in memory before it stops recording.
pub const MAX_RECORDING_BYTES: u64 = 512 << 20;
/// Most records one recording keeps.
pub const MAX_RECORDING_RECORDS: usize = 1 << 20;

impl ActiveRecording {
    /// Appends a frame unless a cap (`max_bytes`, `max_records`) would be crossed; then the
    /// recording is marked `truncated` and the frame is not kept. Returns whether it was kept.
    pub fn push_capped(&mut self, tick: u64, frame: Vec<u8>, max_bytes: u64, max_records: usize) -> bool {
        let next = self.bytes.saturating_add(frame.len() as u64);
        if self.truncated || next > max_bytes || self.recording.records.len() >= max_records {
            self.truncated = true;
            return false;
        }
        self.bytes = next;
        self.recording.push(tick, frame);
        true
    }
}

/// A replay in progress (`dcc_feed.replay`).
#[derive(Debug)]
pub struct ActiveReplay {
    /// The replay's session.
    pub session: SessionId,
    /// The file.
    pub path: PathBuf,
    /// Records with the core tick each is due.
    pub records: Vec<(u64, Vec<u8>)>,
    /// Records injected so far.
    pub done: usize,
    /// Restart at the end instead of closing.
    pub looping: bool,
    /// Ticks between records when the caller set `ticks_per_record` (else recorded spacing).
    pub ticks_per_record: Option<u32>,
}

/// Options of [`FeedCore::start_replay`].
#[derive(Debug, Clone, Default)]
pub struct ReplayOptions {
    /// Fixed ticks between records (default: the recorded spacing).
    pub ticks_per_record: Option<u32>,
    /// Restart at the end.
    pub looping: bool,
    /// Client name of the replay session (default: the recording's).
    pub as_client: Option<String>,
}

/// Why a record, replay or listener call was refused (`F0002` `data.reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreError {
    /// Lowercase reason code.
    pub code: &'static str,
    /// Human-readable text.
    pub message: String,
}

impl CoreError {
    /// `internal`, with `message`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What the main thread takes from the core once per tick.
#[derive(Debug, Default)]
pub struct TickOutput {
    /// Inbox entries to apply, in arrival order.
    pub entries: Vec<InboxEntry>,
    /// Sessions finalized this tick (their items detach).
    pub closed: Vec<SessionId>,
    /// `(gltf_space, unit_scale)` of every session that has entries.
    pub spaces: HashMap<SessionId, (bool, f32)>,
    /// Clock offset of every session with entries, when a `ping` gave one (spec § 4.6).
    pub offsets: HashMap<SessionId, f64>,
}

/// The shared server state. Wrap it in a mutex; reader threads and the main thread lock it briefly.
#[derive(Debug)]
pub struct FeedCore {
    /// Every session.
    pub sessions: SessionTable,
    inbox: FeedInbox,
    limits: Limits,
    token: String,
    auto_accept: Vec<String>,
    max_sessions: u32,
    window_bytes: u32,
    held: HashMap<SessionId, Vec<(u32, u32, Message)>>,
    outbox: BTreeMap<SessionId, Vec<Message>>,
    out_seq: HashMap<SessionId, u32>,
    writers: HashMap<SessionId, WriterSlot>,
    unknown_item_last: HashMap<(SessionId, u32), f64>,
    /// Closing sessions a tick already listed in [`TickOutput::closed`]. [`Self::flush`] finishes
    /// only these, so the main thread detaches every closing session's items before the session is
    /// cut: a `goodbye` a reader thread applies between the tick and the flush waits for the next
    /// tick instead of going straight to `closed` unseen (ADR-8795).
    reported_closing: BTreeSet<SessionId>,
    clock_samples: HashMap<SessionId, VecDeque<f64>>,
    live: Arc<AtomicUsize>,
    /// The recording in progress.
    pub recording: Option<ActiveRecording>,
    /// The replay in progress.
    pub replay: Option<ActiveReplay>,
    /// Engine seconds of the latest tick.
    pub now_s: f64,
    /// Ticks run so far.
    pub tick: u64,
    /// Warnings for `dcc_feed.status` (newest last, capped).
    pub warnings: Vec<(SessionId, String, String)>,
    /// Warnings not yet sent as messages.
    new_warnings: Vec<(SessionId, String, String)>,
}

/// Most warnings kept for `dcc_feed.status`.
const WARNINGS_CAP: usize = 64;

impl FeedCore {
    /// A core configured from `settings` with the session `token`.
    #[must_use]
    pub fn new(settings: &DccFeedSettings, token: impl Into<String>) -> Self {
        Self {
            sessions: SessionTable::new(),
            inbox: FeedInbox::new(InboxConfig {
                window_bytes: settings.window_bytes,
                coalesce_interpolated: settings.playout == super::settings::Playout::Snap,
            }),
            limits: settings.limits(),
            token: token.into(),
            auto_accept: settings.auto_accept.clone(),
            max_sessions: settings.max_sessions,
            window_bytes: settings.window_bytes,
            held: HashMap::new(),
            outbox: BTreeMap::new(),
            out_seq: HashMap::new(),
            writers: HashMap::new(),
            unknown_item_last: HashMap::new(),
            reported_closing: BTreeSet::new(),
            clock_samples: HashMap::new(),
            live: Arc::new(AtomicUsize::new(0)),
            recording: None,
            replay: None,
            now_s: 0.0,
            tick: 0,
            warnings: Vec::new(),
            new_warnings: Vec::new(),
        }
    }

    /// Sessions in the table. The main thread's systems read this one atomic while idle
    /// (spec § 10) and take the lock only when it is above zero.
    #[must_use]
    pub fn live_counter(&self) -> Arc<AtomicUsize> {
        self.live.clone()
    }

    /// The clock offset of `session`: server seconds minus sender seconds, the smallest of the
    /// last 16 `ping` samples, so the sample with the least transit delay wins (spec § 4.6).
    #[must_use]
    pub fn clock_offset(&self, session: SessionId) -> Option<f64> {
        self.clock_samples.get(&session)?.iter().copied().reduce(f64::min)
    }

    /// The wire limits in force.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The session token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Replaces the session token (`dcc_feed.rotate_token`); open sessions are not affected.
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// Adds a client name to the in-memory allow list.
    pub fn allow_client(&mut self, name: &str) {
        if !self.auto_accept.iter().any(|n| n == name) {
            self.auto_accept.push(name.to_owned());
        }
    }

    /// Inbox counters of `session` (for `dcc_feed.status`).
    #[must_use]
    pub fn counters(&self, session: SessionId) -> super::inbox::SessionCounters {
        self.inbox.counters(session)
    }

    /// Opens a session for a transport the server accepted.
    pub fn open(
        &mut self,
        transport: Transport,
        peer: impl Into<String>,
        writer: Option<SessionWriter>,
    ) -> SessionId {
        let id = self.sessions.open(transport, peer, self.now_s);
        self.live.fetch_add(1, Ordering::Relaxed);
        if let Some(w) = writer {
            self.writers.insert(id, WriterSlot::spawn(w));
        }
        id
    }

    /// The replies waiting for `session` (tests and the replay path read them; sockets get them
    /// on [`Self::flush`]).
    pub fn take_outbox(&mut self, session: SessionId) -> Vec<Message> {
        self.outbox.remove(&session).unwrap_or_default()
    }

    fn reply(&mut self, session: SessionId, msg: Message) {
        self.outbox.entry(session).or_default().push(msg);
    }

    /// Queues an error frame for `session`; a fatal code also starts closing it.
    pub fn send_error(&mut self, session: SessionId, ref_seq: u32, handle: u32, err: &WireError) {
        let fatal = err.code.is_fatal();
        self.sessions.note_error(session, err.code());
        self.reply(
            session,
            Message::Error(ErrorFrame {
                ref_seq,
                handle,
                code: err.code().to_owned(),
                message: err.message.clone(),
                fatal,
            }),
        );
        if fatal {
            self.sessions.begin_close(session, err.code(), self.now_s);
        }
    }

    /// Refuses a frame and closes the session, whatever the code says (a read error, a bad
    /// frame). Does nothing on a session already going away, and writes the reply at once: the
    /// reader thread is about to leave.
    pub fn refuse_fatal(&mut self, session: SessionId, seq: u32, err: &WireError) {
        if matches!(
            self.sessions.phase(session),
            Some(Phase::Closing | Phase::Closed) | None
        ) {
            return;
        }
        self.send_error(session, seq, 0, err);
        self.sessions.begin_close(session, err.code(), self.now_s);
        self.flush_replies(session);
    }

    /// `unknown_item` for `handle`, at most once per handle per second of engine time.
    pub fn send_unknown_item(&mut self, session: SessionId, ref_seq: u32, handle: u32) {
        let now = self.now_s;
        let due = self
            .unknown_item_last
            .get(&(session, handle))
            .is_none_or(|last| now - last >= UNKNOWN_ITEM_INTERVAL_S);
        if due {
            self.unknown_item_last.insert((session, handle), now);
            let err = WireError::new(ErrorCode::UnknownItem, format!("handle {handle} is not declared"));
            self.send_error(session, ref_seq, handle, &err);
        }
    }

    /// Notes a warning (`DccFeedWarning`, `dcc_feed.status`).
    pub fn warn(&mut self, session: SessionId, code: &str, text: impl Into<String>) {
        if self.warnings.len() >= WARNINGS_CAP {
            self.warnings.remove(0);
        }
        let warning = (session, code.to_owned(), text.into());
        if self.new_warnings.len() < WARNINGS_CAP {
            self.new_warnings.push(warning.clone());
        }
        self.warnings.push(warning);
    }

    /// The warnings added since the last call.
    pub fn take_new_warnings(&mut self) -> Vec<(SessionId, String, String)> {
        std::mem::take(&mut self.new_warnings)
    }

    /// Handles one decoded, validated message of `bytes` frame bytes from `session`.
    pub fn submit(&mut self, session: SessionId, seq: u32, bytes: u32, msg: Message) {
        let Some(phase) = self.sessions.phase(session) else {
            return;
        };
        match (&msg, phase) {
            (_, Phase::Closing | Phase::Closed) => {}
            (Message::Hello(hello), _) => self.on_hello(session, seq, hello),
            (_, Phase::Negotiating) => {
                let err = WireError::new(ErrorCode::MalformedFrame, "the first message must be hello");
                self.send_error(session, seq, 0, &err);
            }
            (Message::Ping(p), _) => {
                let samples = self.clock_samples.entry(session).or_default();
                if samples.len() == CLOCK_SAMPLES {
                    samples.pop_front();
                }
                samples.push_back(self.now_s - p.sender_time_ns as f64 / 1e9);
                let pong = Ping {
                    token: p.token,
                    sender_time_ns: p.sender_time_ns,
                    responder_time_ns: (self.now_s * 1e9) as u64,
                };
                self.reply(session, Message::Pong(pong));
            }
            (Message::Goodbye(reason), _) => {
                let reason = if reason.is_empty() {
                    "goodbye"
                } else {
                    reason.as_str()
                };
                self.sessions.begin_close(session, reason, self.now_s);
            }
            (Message::Error(e), _) => {
                self.sessions.note_error(session, &e.code);
                if e.fatal {
                    self.sessions.begin_close(session, &e.code, self.now_s);
                }
            }
            // Server-to-client types and optional unknown types from a client are ignored.
            (
                Message::Pong(_)
                | Message::Ack(_)
                | Message::HelloAck(_)
                | Message::SessionState(_)
                | Message::Resync(_)
                | Message::Unknown { .. },
                _,
            ) => {}
            // Refused before any path is opened (spec § 9.2).
            (Message::ClipPush(_) | Message::TexturePush(_), _) if !ASSET_KINDS => {
                let err = WireError::new(
                    ErrorCode::KindNotSupported,
                    "texture and clip pushes need the editor receiver",
                );
                self.send_error(session, seq, 0, &err);
            }
            (_, Phase::Pending) => {
                // Declarations are held for the accept; data frames are discarded and counted.
                if matches!(
                    msg,
                    Message::ItemDeclare(_) | Message::SkeletonStatic(_) | Message::ItemDelete(_)
                ) {
                    // Charged against the session's byte window and a count cap, like the inbox
                    // (audit-concurrency-1103 agent M3); the overflow is dropped, and the accept's
                    // resync asks the sender for its full state again.
                    let window = u64::from(self.window_bytes);
                    let held = self.held.entry(session).or_default();
                    let held_bytes: u64 = held.iter().map(|(_, b, _)| u64::from(*b)).sum();
                    let full = held.len() >= MAX_HELD_DECLARATIONS
                        || held_bytes.saturating_add(u64::from(bytes)) > window;
                    if full {
                        let handle = msg.handle().unwrap_or(0);
                        let err = WireError::new(
                            ErrorCode::Overloaded,
                            "pending session: held declarations past the byte window; dropped until the accept's resync",
                        );
                        self.send_error(session, seq, handle, &err);
                    } else {
                        held.push((seq, bytes, msg));
                    }
                } else {
                    self.sessions.count_not_accepted(session);
                }
            }
            (_, Phase::Active) => self.push_active(session, seq, bytes, msg),
        }
    }

    fn on_hello(&mut self, session: SessionId, seq: u32, hello: &Hello) {
        let policy = HelloPolicy {
            token: &self.token,
            auto_accept: &self.auto_accept,
            max_sessions: self.max_sessions,
            limits: self.limits,
            window_bytes: self.window_bytes,
            server_name: "regia",
        };
        match self.sessions.hello(session, hello, &policy, self.now_s) {
            Ok(ack) => {
                let accepted = ack.accepted;
                self.reply(session, Message::HelloAck(ack));
                if accepted {
                    self.reply(session, Message::Resync(Resync { handle: 0 }));
                }
            }
            Err(e) => {
                // Every hello refusal closes the session.
                self.send_error(session, seq, 0, &e);
                self.sessions.begin_close(session, e.code(), self.now_s);
            }
        }
    }

    /// Starts recording `session` at its hello, so the replay gets its space and unit scale.
    fn record_hello(&mut self, session: SessionId) {
        let Some(s) = self.sessions.get(session) else {
            return;
        };
        let hello = Message::Hello(Hello {
            min_version: s.protocol_version,
            max_version: s.protocol_version,
            client_name: s.client.clone(),
            client_version: s.client_version.clone(),
            dcc: s.dcc.clone(),
            space: if s.gltf_space { "gltf" } else { "regia" }.to_owned(),
            unit_scale: s.unit_scale,
            caps: s.caps,
            token: String::new(),
        });
        let (client, dcc) = (s.client.clone(), s.dcc.clone());
        let tick = self.tick;
        if let Some(rec) = &mut self.recording {
            rec.session = Some(session);
            rec.recording.client_name = client;
            rec.recording.dcc = dcc;
            let mut frame = Vec::new();
            if wire::encode(&hello, 0, &mut frame).is_ok() {
                rec.push_capped(tick, frame, MAX_RECORDING_BYTES, MAX_RECORDING_RECORDS);
            }
        }
    }

    fn push_active(&mut self, session: SessionId, seq: u32, bytes: u32, msg: Message) {
        if self.recording.as_ref().is_some_and(|r| r.session.is_none()) {
            self.record_hello(session);
        }
        if let Some(rec) = &mut self.recording
            && rec.session == Some(session)
        {
            let mut frame = Vec::new();
            if wire::encode(&msg, seq, &mut frame).is_ok() {
                rec.push_capped(self.tick, frame, MAX_RECORDING_BYTES, MAX_RECORDING_RECORDS);
            }
        }
        let handle = msg.handle().unwrap_or(0);
        match self.inbox.push(session, seq, bytes, msg) {
            PushOutcome::Accepted => {}
            PushOutcome::Overloaded => {
                let err = WireError::new(ErrorCode::Overloaded, "byte window exceeded; frame dropped");
                self.send_error(session, seq, handle, &err);
            }
            PushOutcome::CloseSession => {
                let err = WireError::new(ErrorCode::Overloaded, "byte window exceeded three times");
                self.send_error(session, seq, handle, &err);
                self.sessions.begin_close(session, "overloaded", self.now_s);
            }
        }
    }

    /// Accepts a pending session: held declarations go to the inbox and a `resync` asks the
    /// sender for its full state. Returns the resync count (1).
    ///
    /// # Errors
    ///
    /// The session error code (`session_not_found`, `already_accepted`, `session_closed`,
    /// `session_negotiating`).
    pub fn accept(&mut self, session: SessionId) -> Result<u32, &'static str> {
        self.sessions.accept(session, self.now_s).map_err(|e| e.code())?;
        for (seq, bytes, msg) in self.held.remove(&session).unwrap_or_default() {
            self.push_active(session, seq, bytes, msg);
        }
        self.reply(
            session,
            Message::SessionState(wire::SessionState {
                state: Phase::Active as u8,
                reason: "accepted".to_owned(),
            }),
        );
        self.reply(session, Message::Resync(Resync { handle: 0 }));
        Ok(1)
    }

    /// Closes `session` (`dcc_feed.disconnect`). The main thread finalizes it on the next tick.
    ///
    /// # Errors
    ///
    /// `session_not_found` for an unknown session.
    pub fn disconnect(&mut self, session: SessionId) -> Result<(), &'static str> {
        if self.sessions.get(session).is_none() {
            return Err("session_not_found");
        }
        if self.sessions.begin_close(session, "disconnect", self.now_s) {
            self.reply(session, Message::Goodbye("disconnect".to_owned()));
        }
        Ok(())
    }

    /// Runs one main-thread tick at engine time `now_s`: replay injection, session timers,
    /// finalizing closed sessions, and the inbox drain with credits.
    pub fn begin_tick(&mut self, now_s: f64) -> TickOutput {
        self.now_s = now_s;
        self.tick += 1;
        self.drive_replay();
        for event in self.sessions.tick(now_s) {
            match event {
                super::session::TickEvent::TimedOut(id) => {
                    let err = WireError::new(ErrorCode::Timeout, "session timed out");
                    self.send_error(id, 0, 0, &err);
                }
                super::session::TickEvent::Forgotten(id) => {
                    self.live.fetch_sub(1, Ordering::Relaxed);
                    self.clock_samples.remove(&id);
                    self.inbox.forget_session(id);
                }
            }
        }
        let mut out = TickOutput::default();
        for id in self.sessions.closing() {
            self.inbox.discard_session(id);
            self.held.remove(&id);
            self.unknown_item_last.retain(|(s, _), _| *s != id);
            self.reported_closing.insert(id);
            out.closed.push(id);
        }
        let drained = self.inbox.drain();
        for c in drained.credits {
            self.reply(
                c.session,
                Message::Ack(Ack {
                    seq_applied: c.seq_applied,
                    credit_bytes: c.credit_bytes,
                }),
            );
        }
        for e in &drained.entries {
            if let Some(s) = self.sessions.get(e.session) {
                out.spaces.insert(e.session, (s.gltf_space, s.unit_scale));
            }
            if let Some(offset) = self.clock_offset(e.session) {
                out.offsets.insert(e.session, offset);
            }
        }
        out.entries = drained.entries;
        out
    }

    /// Writes the replies queued for `session` to its socket. A reader thread calls this after
    /// each message, so a hello is answered without waiting for the main thread's tick (a
    /// `pending` session may have no tick for a while). A failed write starts closing the session.
    pub fn flush_replies(&mut self, session: SessionId) {
        let Some(w) = self.writers.get_mut(&session) else {
            // A replay has nobody to answer. Any other session without a socket keeps its
            // replies for `take_outbox`.
            if self
                .sessions
                .get(session)
                .is_some_and(|s| s.transport == Transport::Replay)
            {
                self.outbox.remove(&session);
            }
            return;
        };
        let Some(msgs) = self.outbox.remove(&session) else {
            return;
        };
        let seq = self.out_seq.entry(session).or_insert(0);
        let mut buf = Vec::new();
        for m in &msgs {
            *seq = seq.wrapping_add(1);
            // Encoding a reply the server built only fails on a string above the wire's length
            // field; that reply is dropped, the rest still go out.
            let _ = wire::encode(m, *seq, &mut buf);
        }
        if let Err(reason) = w.write(buf) {
            self.sessions.begin_close(session, reason, self.now_s);
        }
    }

    /// Writes every queued reply to its socket, then cuts the closing sessions the last tick
    /// reported (their items are detached by now). A session that began closing after that tick
    /// is cut by the flush after the next one.
    pub fn flush(&mut self) {
        let sessions: Vec<SessionId> = self.outbox.keys().copied().collect();
        for session in sessions {
            self.flush_replies(session);
        }
        // A write that failed on a writer thread closes its session here.
        let failed: Vec<SessionId> = self
            .writers
            .iter()
            .filter(|(_, w)| w.failed())
            .map(|(id, _)| *id)
            .collect();
        for id in failed {
            self.sessions.begin_close(id, "socket error", self.now_s);
        }
        for id in self.sessions.closing() {
            if !self.reported_closing.remove(&id) {
                continue;
            }
            if let Some(w) = self.writers.remove(&id) {
                w.shutdown();
            }
            self.out_seq.remove(&id);
            self.sessions.finish_close(id, self.now_s);
        }
    }

    /// Shuts every connection (server drop).
    pub fn shutdown_all(&mut self) {
        for (_, w) in self.writers.drain() {
            w.shutdown();
        }
    }

    /// Starts recording what `session` delivers (default: the first active session, or the first
    /// to become active). A `resync` asks the sender for its full state, so the recording stands
    /// on its own.
    ///
    /// # Errors
    ///
    /// `record_active` when a recording runs, `session_not_found` for an unknown session.
    pub fn start_recording(&mut self, path: PathBuf, session: Option<SessionId>) -> Result<(), CoreError> {
        if self.recording.is_some() {
            return Err(CoreError::new(
                "record_active",
                "a recording is running; stop it first",
            ));
        }
        let target = match session {
            Some(id) if self.sessions.get(id).is_none() => {
                return Err(CoreError::new("session_not_found", format!("no session {id}")));
            }
            Some(id) => Some(id),
            None => self
                .sessions
                .iter()
                .find(|s| s.phase == Phase::Active)
                .map(|s| s.id),
        };
        self.recording = Some(ActiveRecording {
            session: None,
            path,
            recording: Recording::new(String::new(), String::new()),
            bytes: 0,
            truncated: false,
        });
        if let Some(id) = target {
            self.record_hello(id);
            self.reply(id, Message::Resync(Resync { handle: 0 }));
        }
        Ok(())
    }

    /// Stops the recording and writes it (atomically). Returns the path and the record count.
    ///
    /// # Errors
    ///
    /// `invalid_argument` when nothing records, `internal` for a write failure.
    pub fn stop_recording(&mut self) -> Result<(PathBuf, usize), CoreError> {
        let Some(rec) = self.recording.as_ref() else {
            return Err(CoreError::new("invalid_argument", "no recording is running"));
        };
        let n = rec.recording.records.len();
        if let Some(dir) = rec.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::new("internal", e.to_string()))?;
        }
        // The take is dropped only after it was written, so a failed write can be retried.
        rec.recording
            .write_file(&rec.path)
            .map_err(|e| CoreError::new("internal", e.to_string()))?;
        let path = rec.path.clone();
        self.recording = None;
        Ok((path, n))
    }

    /// Starts replaying `path` as a new session (transport `replay`, no token). The session goes
    /// through the hello policy like any sender: an unknown client name waits in `pending`.
    ///
    /// # Errors
    ///
    /// `replay_not_found`, `replay_corrupt`, or `record_active` when a replay already runs.
    pub fn start_replay(
        &mut self,
        path: &Path,
        opts: &ReplayOptions,
    ) -> Result<(SessionId, usize), CoreError> {
        if self.replay.is_some() {
            return Err(CoreError::new("record_active", "a replay is running"));
        }
        let rec = Recording::read_file(path).map_err(|e| match e {
            RecordingError::Io(_) => CoreError::new("replay_not_found", e.to_string()),
            _ => CoreError::new("replay_corrupt", e.to_string()),
        })?;
        Ok(self.start_replay_recording(path, rec, opts))
    }

    /// [`Self::start_replay`] for a recording already in memory.
    pub fn start_replay_recording(
        &mut self,
        path: &Path,
        rec: Recording,
        opts: &ReplayOptions,
    ) -> (SessionId, usize) {
        let session = self.open(Transport::Replay, path.display().to_string(), None);
        let client = opts.as_client.clone().unwrap_or_else(|| rec.client_name.clone());
        let hello = Hello {
            min_version: rec.protocol_version,
            max_version: rec.protocol_version,
            client_name: client,
            client_version: String::new(),
            dcc: rec.dcc.clone(),
            space: "gltf".to_owned(),
            unit_scale: 1.0,
            caps: super::session::SERVER_CAPS,
            token: String::new(),
        };
        let mut records: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut first_hello = None;
        for (i, r) in rec.records.into_iter().enumerate() {
            let is_hello = r.header().is_some_and(|h| h.msg_type == wire::msg::HELLO);
            if is_hello && i == 0 {
                first_hello = Some(r.frame);
                continue;
            }
            records.push((r.sim_tick, r.frame));
        }
        // A recorded hello replaces the synthesized one (keeping the replay's client override).
        let mut hello = hello;
        if let Some(frame) = first_hello
            && let Some(Message::Hello(mut h)) = decode_frame(&frame, &self.limits)
        {
            if let Some(c) = &opts.as_client {
                h.client_name.clone_from(c);
            }
            h.token = String::new();
            hello = h;
        }
        let total = records.len();
        // Due ticks relative to the next tick.
        let base = self.tick + 1;
        let first = records.first().map_or(0, |r| r.0);
        let due: Vec<(u64, Vec<u8>)> = records
            .into_iter()
            .enumerate()
            .map(|(i, (t, f))| {
                let offset = match opts.ticks_per_record {
                    Some(n) => (i as u64) * u64::from(n),
                    None => t.saturating_sub(first),
                };
                (base + offset, f)
            })
            .collect();
        self.submit(session, 0, 0, Message::Hello(hello));
        self.replay = Some(ActiveReplay {
            session,
            path: path.to_path_buf(),
            records: due,
            done: 0,
            looping: opts.looping,
            ticks_per_record: opts.ticks_per_record,
        });
        (session, total)
    }

    /// Replay progress `(done, total)`.
    #[must_use]
    pub fn replay_progress(&self) -> Option<(usize, usize)> {
        self.replay.as_ref().map(|r| (r.done, r.records.len()))
    }

    fn drive_replay(&mut self) {
        let _span = tracing::trace_span!("dcc_feed_replay").entered();
        let Some(mut replay) = self.replay.take() else {
            return;
        };
        let session = replay.session;
        match self.sessions.phase(session) {
            // Nothing is injected until the accept: the records would be dropped or held.
            Some(Phase::Pending) => {
                self.replay = Some(replay);
                return;
            }
            Some(Phase::Active) => {}
            // The session closed (disconnect, fatal error): the replay ends with it.
            _ => return,
        }
        if replay.done >= replay.records.len() && !replay.looping {
            // Closed one tick after the last record, so the last frames are drained and applied
            // before the session's inbox is discarded.
            self.sessions.begin_close(session, "replay finished", self.now_s);
            return;
        }
        while let Some((due, frame)) = replay.records.get(replay.done) {
            if *due > self.tick {
                break;
            }
            let frame = frame.clone();
            replay.done += 1;
            match decode_frame_checked(&frame, &self.limits) {
                Ok((header, msg)) => {
                    let bytes = u32::try_from(frame.len()).unwrap_or(u32::MAX);
                    self.submit(session, header.seq, bytes, msg);
                }
                Err(e) => self.send_error(session, 0, 0, &e),
            }
        }
        if replay.done >= replay.records.len()
            && replay.looping
            && let Some(first) = replay.records.first().map(|r| r.0)
        {
            let shift = (self.tick + 1).saturating_sub(first);
            for r in &mut replay.records {
                r.0 += shift;
            }
            replay.done = 0;
        }
        self.replay = Some(replay);
    }
}

/// Decodes one complete wire frame (header included).
fn decode_frame_checked(frame: &[u8], limits: &Limits) -> Result<(FrameHeader, Message), WireError> {
    let Some(head) = frame.first_chunk::<{ wire::HEADER_BYTES }>() else {
        return Err(WireError::new(
            ErrorCode::MalformedFrame,
            "record shorter than a header",
        ));
    };
    let header = FrameHeader::parse(head);
    let payload = &frame[wire::HEADER_BYTES..];
    if header.payload_len() != Some(payload.len()) {
        return Err(WireError::new(
            ErrorCode::MalformedFrame,
            "record length disagrees with its header",
        ));
    }
    wire::decode(header, payload, limits).map(|m| (header, m))
}

fn decode_frame(frame: &[u8], limits: &Limits) -> Option<Message> {
    decode_frame_checked(frame, limits).ok().map(|(_, m)| m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcc_feed::wire::{FeedKind, FrameInfo, ItemDeclare, TransformBlock, TransformFrame};

    fn hello(client: &str) -> Message {
        Message::Hello(Hello {
            min_version: 1,
            max_version: 1,
            client_name: client.to_owned(),
            client_version: "1".to_owned(),
            dcc: "test".to_owned(),
            space: "gltf".to_owned(),
            unit_scale: 1.0,
            caps: 0,
            token: String::new(),
        })
    }

    fn declare(handle: u32, key: &str) -> Message {
        Message::ItemDeclare(ItemDeclare {
            handle,
            key: key.to_owned(),
            kind: FeedKind::Transform,
            parent_handle: 0,
            display_name: key.to_owned(),
            variant: 0,
            channels: Vec::new(),
        })
    }

    fn xf(handle: u32, frame_id: u64, x: f32) -> Message {
        Message::Transform(TransformFrame {
            handle,
            info: FrameInfo {
                frame_id,
                source_time_ns: frame_id,
                timecode: None,
            },
            xf: TransformBlock {
                translation: [x, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
                world_space: false,
            },
        })
    }

    #[test]
    fn pending_holds_declarations_and_drops_data_until_accept() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("addon"));
        let out = core.take_outbox(s);
        assert!(matches!(&out[0], Message::HelloAck(a) if !a.accepted));
        core.submit(s, 2, 30, declare(1, "A"));
        core.submit(s, 3, 60, xf(1, 1, 1.0));
        assert_eq!(core.sessions.get(s).map(|r| r.dropped_not_accepted), Some(1));
        assert!(
            core.begin_tick(0.0).entries.is_empty(),
            "nothing applies while pending"
        );
        assert_eq!(core.accept(s), Ok(1));
        let out = core.take_outbox(s);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Resync(r) if r.handle == 0))
        );
        let tick = core.begin_tick(0.1);
        assert_eq!(tick.entries.len(), 1, "the held declaration");
        assert!(matches!(tick.entries[0].msg, Message::ItemDeclare(_)));
    }

    /// A pending (not yet accepted) session's held declarations used to be stored in full, outside
    /// the byte window and uncapped in count, for the 120 s a session may stay pending
    /// (audit-concurrency-1103 agent M3): they are charged against the session's byte window and
    /// a count cap, and the overflow is dropped with `overloaded` (the accept's resync asks the
    /// sender for its full state again).
    #[test]
    fn a_pending_sessions_held_declarations_are_bounded() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("addon"));
        let _ = core.take_outbox(s);
        let window = u64::from(core.window_bytes);
        for seq in 0..2000u32 {
            core.submit(s, 2 + seq, 1 << 20, declare(seq + 1, &format!("item{seq}")));
        }
        let held = core.held.get(&s).map(Vec::as_slice).unwrap_or_default();
        let held_bytes: u64 = held.iter().map(|(_, bytes, _)| u64::from(*bytes)).sum();
        assert!(
            held_bytes <= window,
            "{held_bytes} held bytes over the {window} byte window"
        );
        assert!(
            core.take_outbox(s).iter().any(|m| matches!(m, Message::Error(_))),
            "the sender hears that declarations were dropped"
        );
        for seq in 0..10_000u32 {
            core.submit(s, 3000 + seq, 1, declare(seq + 5000, "small"));
        }
        let held = core.held.get(&s).map(Vec::len).unwrap_or_default();
        assert!(held <= 4096, "{held} held declarations");
    }

    #[test]
    fn data_before_hello_is_fatal_and_ping_is_answered() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 30, declare(1, "A"));
        assert_eq!(core.sessions.phase(s), Some(Phase::Closing));
        let out = core.take_outbox(s);
        assert!(matches!(&out[0], Message::Error(e) if e.code == "malformed_frame" && e.fatal));

        let s = core.open(Transport::Unix, "unix2", None);
        core.submit(s, 1, 40, hello("x"));
        core.take_outbox(s);
        core.submit(
            s,
            2,
            24,
            Message::Ping(Ping {
                token: 7,
                sender_time_ns: 5,
                responder_time_ns: 0,
            }),
        );
        let out = core.take_outbox(s);
        assert!(matches!(&out[0], Message::Pong(p) if p.token == 7 && p.sender_time_ns == 5));
    }

    /// Spec § 8 test 6 at the core level: 100 transforms before a tick apply as one, and the
    /// credit returned equals the bytes accepted.
    #[test]
    fn coalescing_and_credits_through_the_core() {
        let settings = DccFeedSettings {
            auto_accept: vec!["a".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut core = FeedCore::new(&settings, "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("a"));
        core.submit(s, 2, 30, declare(1, "A"));
        for i in 0..100u32 {
            core.submit(s, 3 + i, 64, xf(1, u64::from(i) + 1, i as f32));
        }
        core.take_outbox(s);
        let tick = core.begin_tick(0.0);
        assert_eq!(tick.entries.len(), 2);
        assert!(matches!(&tick.entries[1].msg, Message::Transform(t) if t.xf.translation[0] == 99.0));
        assert_eq!(core.counters(s).frames_coalesced, 99);
        let acks: u32 = core
            .take_outbox(s)
            .iter()
            .filter_map(|m| match m {
                Message::Ack(a) => Some(a.credit_bytes),
                _ => None,
            })
            .sum();
        assert_eq!(acks, 30 + 100 * 64);
    }

    /// A `Write` that blocks until its channel is dropped: a client that stopped reading.
    struct StalledWrite(std::sync::mpsc::Receiver<()>);

    impl Write for StalledWrite {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Err(std::io::Error::other("stalled client went away"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A `Write` that forwards every write to a channel.
    struct ChannelWrite(std::sync::mpsc::Sender<Vec<u8>>);

    impl Write for ChannelWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// audit-v035-conc M7: replies were written to the socket under the core lock, so one
    /// client that stopped reading held every other session's reader thread and the main
    /// thread's flush for its write timeout (here: for ever). The flush now returns while the
    /// stalled client's write is still pending, and the other session's reply arrives.
    #[test]
    fn a_stalled_client_does_not_hold_the_flush() {
        let settings = DccFeedSettings {
            auto_accept: vec!["a".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut core = FeedCore::new(&settings, "t");
        let (keep_stalled, stalled_rx) = std::sync::mpsc::channel::<()>();
        let stalled = core.open(
            Transport::Unix,
            "unix",
            Some(SessionWriter {
                out: Box::new(StalledWrite(stalled_rx)),
                shutdown: Box::new(|| {}),
            }),
        );
        let (bytes_tx, bytes_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let healthy = core.open(
            Transport::Unix,
            "unix",
            Some(SessionWriter {
                out: Box::new(ChannelWrite(bytes_tx)),
                shutdown: Box::new(|| {}),
            }),
        );
        core.submit(stalled, 1, 40, hello("a"));
        core.submit(healthy, 1, 40, hello("a"));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            core.flush();
            let _ = done_tx.send(());
            // Keep the core (and its writers) until the test ends.
            std::thread::park();
        });
        // Bounds a hang only.
        assert!(
            done_rx.recv_timeout(std::time::Duration::from_secs(120)).is_ok(),
            "the flush waited for a client that is not reading"
        );
        let reply = bytes_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the healthy session's reply");
        assert!(!reply.is_empty());
        drop(keep_stalled);
    }

    #[test]
    fn disconnect_closes_on_the_next_tick() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("a"));
        assert_eq!(core.disconnect(99), Err("session_not_found"));
        core.disconnect(s).expect("disconnect");
        let tick = core.begin_tick(0.0);
        assert_eq!(tick.closed, vec![s]);
        core.flush();
        assert_eq!(core.sessions.phase(s), Some(Phase::Closed));
    }

    /// ADR-8795: a `goodbye` that a reader thread applies after the main thread's tick but before
    /// its flush used to be cut by that flush without any tick listing it in `closed`, so its items
    /// were never detached (the gate's `dcc_feed_peer` "never reached: the detach" under load).
    #[test]
    fn a_goodbye_between_the_tick_and_the_flush_is_reported_before_the_cut() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("a"));
        assert!(core.begin_tick(0.0).closed.is_empty());
        core.submit(s, 2, 8, Message::Goodbye("bye".to_owned()));
        core.flush();
        assert_eq!(
            core.sessions.phase(s),
            Some(Phase::Closing),
            "no tick has reported the close yet"
        );
        let tick = core.begin_tick(0.1);
        assert_eq!(tick.closed, vec![s], "the next tick reports it");
        core.flush();
        assert_eq!(core.sessions.phase(s), Some(Phase::Closed));
        assert!(core.begin_tick(0.2).closed.is_empty(), "reported once");
    }

    /// Spec § 2.1: `dcc_feed.status` still reports a closed session, counters included.
    #[test]
    fn a_closed_session_keeps_its_counters_until_it_is_forgotten() {
        let settings = DccFeedSettings {
            auto_accept: vec!["a".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut core = FeedCore::new(&settings, "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("a"));
        core.submit(s, 2, 40, declare(1, "A"));
        core.begin_tick(0.0);
        assert_eq!(core.counters(s).frames_drained, 1);

        core.submit(s, 3, 64, xf(1, 1, 1.0));
        core.disconnect(s).expect("disconnect");
        core.begin_tick(1.0);
        core.flush();
        assert_eq!(core.sessions.phase(s), Some(Phase::Closed));
        let closed = core.counters(s);
        assert_eq!(closed.frames_in, 2);
        assert_eq!(closed.frames_drained, 1, "closing discards the pending frame");

        core.begin_tick(1.0 + super::super::session::CLOSED_KEEP_S + 1.0);
        assert_eq!(core.sessions.phase(s), None);
        assert_eq!(core.counters(s).frames_in, 0, "forgotten with the record");
    }

    #[test]
    fn a_recording_stops_keeping_frames_at_its_caps() {
        let mut rec = ActiveRecording {
            session: None,
            path: PathBuf::from("x.rfdrec"),
            recording: Recording::new("a", "b"),
            bytes: 0,
            truncated: false,
        };
        assert!(rec.push_capped(0, vec![0; 10], 25, 100));
        assert!(rec.push_capped(0, vec![0; 10], 25, 100));
        assert!(!rec.push_capped(0, vec![0; 10], 25, 100), "byte cap");
        assert!(rec.truncated);
        assert!(!rec.push_capped(0, vec![0; 1], 25, 100), "stays truncated");
        assert_eq!((rec.recording.records.len(), rec.bytes), (2, 20));
        let mut by_count = ActiveRecording {
            truncated: false,
            bytes: 0,
            recording: Recording::new("a", "b"),
            ..rec
        };
        assert!(by_count.push_capped(0, vec![], u64::MAX, 1));
        assert!(!by_count.push_capped(0, vec![], u64::MAX, 1), "record cap");
    }

    #[test]
    fn a_failed_stop_keeps_the_take_for_a_retry() {
        let settings = DccFeedSettings::default();
        let mut core = FeedCore::new(&settings, "t");
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_core_rec_fail");
        // A file where the parent directory should be: create_dir_all fails.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").expect("write");
        core.start_recording(blocker.join("take.rfdrec"), None)
            .expect("start");
        assert_eq!(core.stop_recording().expect_err("unwritable").code, "internal");
        assert!(core.recording.is_some(), "the take survives a failed write");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_then_replay_delivers_the_same_frames_at_the_recorded_ticks() {
        let settings = DccFeedSettings {
            auto_accept: vec!["a".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut core = FeedCore::new(&settings, "t");
        let s = core.open(Transport::Unix, "unix", None);
        core.submit(s, 1, 40, hello("a"));
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_core_rec");
        let path = dir.join("take.rfdrec");
        core.start_recording(path.clone(), None).expect("start");
        assert_eq!(
            core.start_recording(path.clone(), None).expect_err("twice").code,
            "record_active"
        );
        core.begin_tick(0.0);
        core.submit(s, 2, 30, declare(1, "A"));
        core.begin_tick(0.1);
        core.submit(s, 3, 64, xf(1, 1, 1.0));
        core.begin_tick(0.2);
        core.submit(s, 4, 64, xf(1, 2, 2.0));
        let (written, n) = core.stop_recording().expect("stop");
        // The hello plus the three frames.
        assert_eq!((written.as_path(), n), (path.as_path(), 4));

        let mut replay_core = FeedCore::new(&settings, "t");
        let (rs, total) = replay_core
            .start_replay(&path, &ReplayOptions::default())
            .expect("replay");
        assert_eq!(total, 3);
        let mut seen = Vec::new();
        for i in 0..6 {
            let tick = replay_core.begin_tick(f64::from(i) * 0.1);
            for e in tick.entries {
                seen.push((i, e.msg));
            }
        }
        assert_eq!(seen.len(), 3);
        assert!(matches!(seen[0].1, Message::ItemDeclare(_)));
        // Recorded one tick apart, replayed one tick apart.
        assert_eq!(seen[1].0 - seen[0].0, 1);
        assert_eq!(seen[2].0 - seen[1].0, 1);
        assert_eq!(seen[2].1, xf(1, 2, 2.0));
        assert_eq!(
            replay_core.sessions.phase(rs),
            Some(Phase::Closing),
            "closes at the end"
        );
        assert_eq!(
            replay_core
                .start_replay(&dir.join("missing.rfdrec"), &ReplayOptions::default())
                .expect_err("missing")
                .code,
            "replay_not_found"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_as_an_unknown_client_waits_for_accept() {
        let mut core = FeedCore::new(&DccFeedSettings::default(), "t");
        let mut rec = Recording::new("tool", "test");
        let mut frame = Vec::new();
        wire::encode(&declare(1, "A"), 1, &mut frame).expect("encode");
        rec.push(0, frame);
        let opts = ReplayOptions {
            as_client: Some("stranger".to_owned()),
            ..ReplayOptions::default()
        };
        let (s, _) = core.start_replay_recording(Path::new("mem.rfdrec"), rec, &opts);
        assert_eq!(core.sessions.phase(s), Some(Phase::Pending));
        assert_eq!(core.sessions.get(s).map(|r| r.client.as_str()), Some("stranger"));
        core.begin_tick(0.0);
        core.accept(s).expect("accept");
        let tick = core.begin_tick(0.1);
        assert!(
            tick.entries
                .iter()
                .any(|e| matches!(e.msg, Message::ItemDeclare(_)))
        );
    }
}
