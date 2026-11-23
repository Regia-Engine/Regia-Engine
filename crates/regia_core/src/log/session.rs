// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-log/src/lib.rs and ir-log/src/session.rs, commit d6ed37677), ADR-8012.
// Adapted from DO27 ir-log/src/lib.rs:1252-1277,1532-1558 and session.rs:1-247,
// d6ed37677 (regia-absorb-1). Owner-authorized copy, 2026-11-16, absorb plan A-1;
// class P3. Regia adaptations: per-peer ownership, fallible IO, OS locks, configurable
// bounds and atomic recovery publication; no uploader or game-specific heuristics.
// Async writer (ADR-8493): original bounded queue. The synchronous path is unchanged.
//! Opt-in peer logs, diffable checkpoints, and evidence of an unfinished previous session.
//!
//! The caller supplies a sortable, unique session name and a peer id. Hold the [`PeerLog`]
//! until shutdown, and call [`PeerLog::finish`] only after the host finishes teardown.
//! Dropping it flushes buffers but deliberately does not mark the session clean.
//!
//! Writes run on the caller unless [`SessionOptions::with_async_writer`] installs one ordered
//! worker. [`PeerLog::flush`] and [`PeerLog::finish`] drain records accepted before the call,
//! and drop joins that worker. Human lines that do not fit are counted and dropped. A
//! checkpoint that cannot be queued or written marks the session incomplete, so `finish`
//! does not publish `clean`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Arguments;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle, ThreadId};

use parking_lot::Mutex;

/// `logs.read` target for queue, drop, error and completeness counters.
pub const PEER_ASYNC_TARGET: &str = "regia_core::log::peer_async";

/// Configurable resource bounds for a peer session and its recovery report.
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    /// Maximum distinct named checkpoint files per peer (default 64).
    pub max_hash_traces: usize,
    /// Maximum bytes read from the previous session log, and the maximum length of the
    /// scrubbed tail written afterwards (default 512 KiB). A request above
    /// [`RECOVERY_REPORT_BYTE_CAP`] is clamped to that cap. The cap is applied after
    /// privacy replacement, and a line that would be cut is omitted whole.
    pub tail_bytes: u64,
    /// Maximum nonempty lines retained in an unclean report (default 50).
    pub tail_lines: usize,
    /// Opt-in ordered worker. `None` (the default) keeps synchronous writes on the caller.
    pub async_writer: Option<AsyncWriterBounds>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            max_hash_traces: 64,
            tail_bytes: 512 * 1024,
            tail_lines: 50,
            async_writer: None,
        }
    }
}

impl SessionOptions {
    /// Returns options that queue accepted records onto one owned worker.
    ///
    /// A bound of zero is raised to one so the session can accept its begin line.
    #[must_use]
    pub fn with_async_writer(mut self, bounds: AsyncWriterBounds) -> Self {
        self.async_writer = Some(bounds.normalized());
        self
    }
}

/// Caps for the opt-in peer writer. The queue is bounded by records and by bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncWriterBounds {
    /// Maximum accepted records waiting on the worker, including one write in progress.
    pub max_records: usize,
    /// Maximum payload bytes charged to the queue.
    pub max_queued_bytes: usize,
    /// Maximum encoded bytes of one record, including its trailing newline.
    /// A longer human line is dropped. A longer checkpoint is rejected.
    pub max_message_bytes: usize,
}

impl Default for AsyncWriterBounds {
    fn default() -> Self {
        Self {
            max_records: 256,
            max_queued_bytes: 256 * 1024,
            max_message_bytes: 8 * 1024,
        }
    }
}

impl AsyncWriterBounds {
    fn normalized(self) -> Self {
        Self {
            max_records: self.max_records.max(1),
            max_queued_bytes: self.max_queued_bytes.max(1),
            max_message_bytes: self.max_message_bytes.max(1),
        }
    }
}

/// Tick and per-peer sequence captured when a record was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerEnqueue {
    /// Tick stored for this record. Checkpoint ticks are the `hash_trace` argument.
    pub tick: u64,
    /// Per-peer sequence. The session-begin line is sequence 1.
    pub sequence: u64,
}

/// Queue, drop, error and completeness counters for one peer session.
///
/// `queued_*` is zero for the synchronous writer. `complete` is true only after `finish`
/// publishes `clean`. `incomplete` means a checkpoint was lost or a write failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLogDiagnostics {
    /// Records accepted and not yet finished by the worker.
    pub queued_records: u64,
    /// Payload bytes charged for [`Self::queued_records`].
    pub queued_bytes: u64,
    /// Records the writer accepted into the log or a checkpoint file.
    pub written_records: u64,
    /// Payload bytes counted in [`Self::written_records`].
    pub written_bytes: u64,
    /// Human lines that were not queued.
    pub dropped_lines: u64,
    /// Encoded bytes of [`Self::dropped_lines`].
    pub dropped_bytes: u64,
    /// Human lines dropped because they exceeded the message cap. They are included in
    /// [`Self::dropped_lines`].
    pub oversize_lines: u64,
    /// Checkpoint rejections and write failures.
    pub errors: u64,
    /// A lost checkpoint or a failed write. `finish` will not publish `clean`.
    pub incomplete: bool,
    /// `finish` published the clean marker.
    pub complete: bool,
    /// Tick last stored by `set_tick` or `line_at`.
    pub tick: u64,
    /// Sequence number the next accepted record will receive.
    pub next_sequence: u64,
}

struct State {
    log: BufWriter<File>,
    hashes: BTreeMap<String, BufWriter<File>>,
    tick: u64,
    next_sequence: u64,
    finished: bool,
    failed: bool,
    written_records: u64,
    written_bytes: u64,
    errors: u64,
    last_enqueue: HashMap<ThreadId, PeerEnqueue>,
}

struct Diag {
    queued_records: u64,
    queued_bytes: u64,
    written_records: u64,
    written_bytes: u64,
    dropped_lines: u64,
    dropped_bytes: u64,
    oversize_lines: u64,
    errors: u64,
    incomplete: bool,
    complete: bool,
    failed: bool,
    finished: bool,
    closing: bool,
    tick: u64,
    next_sequence: u64,
    written_through: u64,
    trace_names: BTreeSet<String>,
}

impl Diag {
    fn new() -> Self {
        Self {
            queued_records: 0,
            queued_bytes: 0,
            written_records: 0,
            written_bytes: 0,
            dropped_lines: 0,
            dropped_bytes: 0,
            oversize_lines: 0,
            errors: 0,
            incomplete: false,
            complete: false,
            failed: false,
            finished: false,
            closing: false,
            tick: 0,
            next_sequence: 1,
            written_through: 0,
            trace_names: BTreeSet::new(),
        }
    }
}

struct Waiter {
    through: u64,
    ack: mpsc::Sender<io::Result<()>>,
}

struct Shared {
    gate: Mutex<()>,
    diag: Mutex<Diag>,
    last_enqueue: Mutex<HashMap<ThreadId, PeerEnqueue>>,
    waiters: Mutex<Vec<Waiter>>,
    bounds: AsyncWriterBounds,
    max_hash_traces: usize,
    peer: u32,
}

enum Job {
    Wake,
    Line {
        sequence: u64,
        nbytes: u64,
        line: Vec<u8>,
        level: String,
        target: String,
        ring: String,
    },
    Hash {
        sequence: u64,
        nbytes: u64,
        name: String,
        row: Vec<u8>,
        line: Vec<u8>,
        ring: String,
    },
}

enum LogOut {
    File(BufWriter<File>),
    Sink(Box<dyn Write + Send>),
}

impl LogOut {
    fn write_line(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::File(writer) => writer.write_all(bytes),
            Self::Sink(writer) => {
                writer.write_all(bytes)?;
                writer.flush()
            }
        }
    }

    fn persist(&mut self) -> io::Result<()> {
        match self {
            Self::File(writer) => {
                writer.flush()?;
                writer.get_ref().sync_all()
            }
            Self::Sink(writer) => writer.flush(),
        }
    }
}

struct SessionIo {
    log: LogOut,
    hashes: BTreeMap<String, BufWriter<File>>,
    directory: PathBuf,
    max_hash_traces: usize,
}

struct AsyncPeer {
    shared: std::sync::Arc<Shared>,
    tx: Mutex<Option<SyncSender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

enum PeerInner {
    Sync(Mutex<State>),
    Async(AsyncPeer),
}

/// A peer's session directory, log, and checkpoint files.
///
/// The default writer runs on the caller, under one mutex. [`SessionOptions::with_async_writer`]
/// moves accepted records to a single worker owned by this value. `flush` and `finish` wait
/// until that worker has drained the records accepted before the call. Drop joins the worker
/// and still does not publish `clean`.
pub struct PeerLog {
    directory: PathBuf,
    peer: u32,
    options: SessionOptions,
    // Kept open and locked for the entire lifetime, including after finish.
    // The mutex lets producers on other threads share the handle; the file is not used again.
    _lease: Mutex<File>,
    recovered: Option<UncleanSession>,
    inner: PeerInner,
}

impl PeerLog {
    /// Creates a fresh `<session>-peer<id>` directory and recovers the newest inactive older
    /// session for this peer. Existing directories are never reused or truncated.
    /// Session and trace names must be 1–96 ASCII letters, digits, hyphens or underscores.
    /// Session prefixes `unclean-` and `pending-` are reserved (case insensitive).
    pub fn create(root: &Path, session: &str, peer: u32, options: SessionOptions) -> io::Result<Self> {
        Self::create_inner(root, session, peer, options, None)
    }

    /// [`Self::create`] with the session log bytes written to `sink` by the async worker.
    ///
    /// `options.async_writer` is required. Checkpoint files still go to the session directory.
    /// The worker owns `sink`. `flush`, `finish` and drop run until the sink returns, so a
    /// stuck sink stalls those calls and not `line` or `hash_trace`.
    pub fn create_with_log_sink(
        root: &Path,
        session: &str,
        peer: u32,
        options: SessionOptions,
        sink: Box<dyn Write + Send>,
    ) -> io::Result<Self> {
        if options.async_writer.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a log sink requires SessionOptions::with_async_writer",
            ));
        }
        Self::create_inner(root, session, peer, options, Some(sink))
    }

    fn create_inner(
        root: &Path,
        session: &str,
        peer: u32,
        options: SessionOptions,
        sink: Option<Box<dyn Write + Send>>,
    ) -> io::Result<Self> {
        validate_session(session)?;
        fs::create_dir_all(root)?;
        let recovered = check_previous(root, session, peer, options)?;
        let directory = root.join(format!("{session}-peer{peer}"));
        fs::create_dir(&directory)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.join("active.lock"))?;
        lease.lock()?;
        let log_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("session.log"))?;
        // Recovery must not race the gap between creating the lease file and locking it.
        // Publish readiness only once both the lock and the log exist. An interrupted
        // creation without this marker is skipped, so it cannot block later launches.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("ready"))?;
        let inner = match options.async_writer {
            Some(bounds) => {
                let bounds = bounds.normalized();
                let log_out = match sink {
                    Some(sink) => LogOut::Sink(sink),
                    None => LogOut::File(BufWriter::new(log_file)),
                };
                PeerInner::Async(spawn_async(
                    peer,
                    directory.clone(),
                    options.max_hash_traces,
                    bounds,
                    log_out,
                )?)
            }
            None => {
                if sink.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a log sink requires SessionOptions::with_async_writer",
                    ));
                }
                PeerInner::Sync(Mutex::new(State {
                    log: BufWriter::new(log_file),
                    hashes: BTreeMap::new(),
                    tick: 0,
                    next_sequence: 1,
                    finished: false,
                    failed: false,
                    written_records: 0,
                    written_bytes: 0,
                    errors: 0,
                    last_enqueue: HashMap::new(),
                }))
            }
        };
        let logger = Self {
            directory,
            peer,
            options,
            _lease: Mutex::new(lease),
            recovered,
            inner,
        };
        logger.emit(
            tracing::Level::INFO,
            "regia_core::log::session",
            &format!("session begin dev_env_enabled={}", super::devenv::ENABLED),
            false,
            None,
        )?;
        Ok(logger)
    }

    /// Directory containing `session.log`, named `.hash` files and lifecycle markers.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Evidence recovered at creation, if the newest inactive older session was unclean.
    pub fn recovered(&self) -> Option<&UncleanSession> {
        self.recovered.as_ref()
    }

    /// Sets the tick stamped on subsequent human log lines for this peer only.
    pub fn set_tick(&self, tick: u64) {
        match &self.inner {
            PeerInner::Sync(state) => state.lock().tick = tick,
            PeerInner::Async(async_peer) => {
                let _gate = async_peer.shared.gate.lock();
                async_peer.shared.diag.lock().tick = tick;
            }
        }
    }

    /// Writes one event, escaping control characters so every physical line has a tick.
    /// The same message is visible through the existing `logs.read` protocol method after
    /// a synchronous write, or after [`Self::flush`] when the async worker is enabled.
    /// A human line that does not fit an async queue is dropped, counted and returns `Ok`.
    pub fn line(&self, level: tracing::Level, target: &str, message: Arguments<'_>) -> io::Result<()> {
        self.emit(level, target, &message.to_string(), true, None)
    }

    /// [`Self::line`] after storing `tick` and accepting the record under one lock.
    ///
    /// Concurrent producers use this when the tick must be the one paired with the message.
    /// The stored tick is also what a later [`Self::line`] on this peer will stamp.
    pub fn line_at(
        &self,
        tick: u64,
        level: tracing::Level,
        target: &str,
        message: Arguments<'_>,
    ) -> io::Result<()> {
        self.emit(level, target, &message.to_string(), true, Some(tick))
    }

    /// Appends an exact `tick hash` checkpoint: decimal tick and 16 lowercase hex digits.
    /// Files are peer-local and created exclusively. No wall time or peer id contaminates
    /// the diffable bytes. This does not alter this peer's current human-log tick.
    ///
    /// With the async worker, a checkpoint that does not fit the queue returns an error and
    /// marks the session incomplete. A later write error surfaces from [`Self::flush`].
    pub fn hash_trace(&self, name: &str, tick: u64, hash: u64) -> io::Result<()> {
        let _span = tracing::trace_span!("peer_hash_trace").entered();
        validate_name(name)?;
        let row = format!("{tick} {hash:016x}\n");
        let text = format!(
            "t={tick} peer={} hash_trace name={name} hash={hash:016x}",
            self.peer
        );
        match &self.inner {
            PeerInner::Sync(state) => {
                let mut state = state.lock();
                self.sync_hash(&mut state, name, tick, &row, &text)
            }
            PeerInner::Async(async_peer) => {
                let outcome = {
                    let _gate = async_peer.shared.gate.lock();
                    if let Err(error) = reserve_trace(async_peer, name) {
                        EnqueueOutcome {
                            result: Err(error),
                            diag: None,
                            warn: false,
                        }
                    } else {
                        enqueue_hash(async_peer, tick, name, row, text)
                    }
                };
                publish_outcome(self.peer, &outcome);
                outcome.result.map(|_| ())
            }
        }
    }

    /// Queue, drop, error and completeness counters. This does not wait for the worker.
    #[must_use]
    pub fn diagnostics(&self) -> PeerLogDiagnostics {
        match &self.inner {
            PeerInner::Sync(state) => sync_diagnostics(&state.lock()),
            PeerInner::Async(async_peer) => async_diagnostics(&async_peer.shared.diag.lock()),
        }
    }

    /// Tick and sequence of the calling thread's latest accepted record.
    #[must_use]
    pub fn last_enqueue(&self) -> Option<PeerEnqueue> {
        let id = thread::current().id();
        match &self.inner {
            PeerInner::Sync(state) => state.lock().last_enqueue.get(&id).copied(),
            PeerInner::Async(async_peer) => async_peer.shared.last_enqueue.lock().get(&id).copied(),
        }
    }

    /// Pushes [`Self::diagnostics`] to the `logs.read` ring and returns the same snapshot.
    ///
    /// The target is [`PEER_ASYNC_TARGET`]. The message carries `queued_records`,
    /// `queued_bytes`, `written_records`, `written_bytes`, `dropped_lines`, `dropped_bytes`,
    /// `oversize_lines`, `errors`, `incomplete`, `complete` and `sequence`.
    pub fn publish_diagnostics(&self) -> PeerLogDiagnostics {
        let diag = self.diagnostics();
        publish_async_diag(
            self.peer,
            &diag,
            diag.incomplete || diag.errors > 0 || diag.dropped_lines > 0,
        );
        diag
    }

    /// Flushes and syncs all files. A failure prevents a later clean marker.
    ///
    /// The async worker drains records accepted before this call, then syncs. It does not
    /// publish `clean`.
    pub fn flush(&self) -> io::Result<()> {
        let _span = tracing::trace_span!("peer_log_flush").entered();
        match &self.inner {
            PeerInner::Sync(state) => {
                let mut state = state.lock();
                let result = sync_state(&mut state);
                if result.is_err() {
                    let diag = sync_diagnostics(&state);
                    drop(state);
                    publish_async_diag(self.peer, &diag, true);
                }
                result
            }
            PeerInner::Async(async_peer) => {
                let result = drain_async(async_peer, false);
                if result.is_err() {
                    let _ = self.publish_diagnostics();
                }
                result
            }
        }
    }

    /// Syncs all evidence, then atomically publishes `clean`. Idempotent after success;
    /// further lines/checkpoints are rejected. Never call this from a panic handler.
    ///
    /// The async worker drains first. A dropped human line does not by itself refuse the
    /// marker. A lost checkpoint or a failed write does.
    pub fn finish(&self) -> io::Result<()> {
        let _span = tracing::trace_span!("peer_log_finish").entered();
        match &self.inner {
            PeerInner::Sync(state) => self.finish_sync(state),
            PeerInner::Async(async_peer) => self.finish_async(async_peer),
        }
    }

    fn emit(
        &self,
        level: tracing::Level,
        target: &str,
        message: &str,
        lossy: bool,
        tick: Option<u64>,
    ) -> io::Result<()> {
        let _span = tracing::trace_span!("peer_log_line").entered();
        match &self.inner {
            PeerInner::Sync(state) => {
                let mut state = state.lock();
                if let Some(tick) = tick {
                    state.tick = tick;
                }
                let failed_before = state.failed;
                let result = self.sync_line(&mut state, level, target, message);
                let publish = result.is_err() && !failed_before;
                let diag = sync_diagnostics(&state);
                drop(state);
                if publish {
                    publish_async_diag(self.peer, &diag, true);
                }
                result
            }
            PeerInner::Async(async_peer) => {
                let outcome = {
                    let _gate = async_peer.shared.gate.lock();
                    let stamped = {
                        let mut diag = async_peer.shared.diag.lock();
                        if let Some(tick) = tick {
                            diag.tick = tick;
                        }
                        diag.tick
                    };
                    let text = format!(
                        "t={stamped} peer={} {level} {} {}",
                        self.peer,
                        escaped(target),
                        escaped(message)
                    );
                    enqueue_line(async_peer, lossy, stamped, level, target, text)
                };
                publish_outcome(self.peer, &outcome);
                outcome.result.map(|_| ())
            }
        }
    }

    fn sync_line(
        &self,
        state: &mut State,
        level: tracing::Level,
        target: &str,
        message: &str,
    ) -> io::Result<()> {
        ensure_open(state)?;
        let text = format!(
            "t={} peer={} {level} {} {}",
            state.tick,
            self.peer,
            escaped(target),
            escaped(message)
        );
        let nbytes = text.len() + 1;
        let result = writeln!(state.log, "{text}");
        if result.is_err() {
            state.failed = true;
            state.errors += 1;
            return result;
        }
        note_sync_accept(state, state.tick, nbytes);
        super::ring::push(level.as_str(), target, text);
        Ok(())
    }

    fn sync_hash(&self, state: &mut State, name: &str, tick: u64, row: &str, text: &str) -> io::Result<()> {
        ensure_open(state)?;
        if state.hashes.len() >= self.options.max_hash_traces && !state.hashes.contains_key(name) {
            // A new name over the cap is a caller error. Existing traces can still be
            // appended, and finish can still publish clean.
            return Err(io::Error::other("checkpoint file limit reached"));
        }
        let file = match state.hashes.entry(name.to_owned()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let opened = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(self.directory.join(format!("{name}.hash")));
                let file = match opened {
                    Ok(file) => file,
                    Err(error) => {
                        state.failed = true;
                        state.errors += 1;
                        return Err(error);
                    }
                };
                entry.insert(BufWriter::new(file))
            }
        };
        let result = write!(file, "{row}");
        if result.is_err() {
            state.failed = true;
            state.errors += 1;
            return result;
        }
        let result = writeln!(state.log, "{text}");
        if result.is_err() {
            state.failed = true;
            state.errors += 1;
            return result;
        }
        note_sync_accept(state, tick, row.len() + text.len() + 1);
        super::ring::push("INFO", "regia_core::log::hash_trace", text.to_owned());
        Ok(())
    }

    fn finish_sync(&self, state: &Mutex<State>) -> io::Result<()> {
        let mut state = state.lock();
        if state.finished {
            return Ok(());
        }
        ensure_open(&state)?;
        sync_state(&mut state)?;
        let path = self.directory.join("clean.pending");
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(b"session end clean=1\n")?;
        file.sync_all()?;
        fs::rename(path, self.directory.join("clean"))?;
        state.finished = true;
        let tick = state.tick;
        let diag = sync_diagnostics(&state);
        drop(state);
        super::ring::push(
            "INFO",
            "regia_core::log::session",
            format!("t={tick} peer={} session end clean=1", self.peer),
        );
        publish_async_diag(self.peer, &diag, false);
        Ok(())
    }

    fn finish_async(&self, async_peer: &AsyncPeer) -> io::Result<()> {
        if async_peer.shared.diag.lock().finished {
            return Ok(());
        }
        let result = drain_async(async_peer, true);
        if result.is_err() {
            let _ = self.publish_diagnostics();
            return result;
        }
        let diag = async_diagnostics(&async_peer.shared.diag.lock());
        if diag.incomplete {
            publish_async_diag(self.peer, &diag, true);
            return Err(io::Error::other("peer session had an IO failure"));
        }
        let path = self.directory.join("clean.pending");
        let published = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
            file.write_all(b"session end clean=1\n")?;
            file.sync_all()?;
            fs::rename(&path, self.directory.join("clean"))?;
            Ok(())
        })();
        if let Err(error) = published {
            let _gate = async_peer.shared.gate.lock();
            let mut diag = async_peer.shared.diag.lock();
            diag.failed = true;
            diag.incomplete = true;
            diag.errors += 1;
            let snap = async_diagnostics(&diag);
            drop(diag);
            drop(_gate);
            publish_async_diag(self.peer, &snap, true);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let tick = {
            let _gate = async_peer.shared.gate.lock();
            let mut diag = async_peer.shared.diag.lock();
            diag.finished = true;
            diag.complete = true;
            diag.tick
        };
        super::ring::push(
            "INFO",
            "regia_core::log::session",
            format!("t={tick} peer={} session end clean=1", self.peer),
        );
        let _ = self.publish_diagnostics();
        Ok(())
    }
}

impl Drop for PeerLog {
    fn drop(&mut self) {
        match &mut self.inner {
            PeerInner::Sync(state) => {
                // Flush while the lease is still held. Field drop order alone would unlock before
                // BufWriter's best-effort flush and let recovery read an incomplete tail.
                let state = state.get_mut();
                let _ = state.log.flush();
                for file in state.hashes.values_mut() {
                    let _ = file.flush();
                }
            }
            PeerInner::Async(async_peer) => {
                {
                    let mut tx = async_peer.tx.lock();
                    *tx = None;
                }
                if let Some(handle) = async_peer.worker.lock().take() {
                    let _ = handle.join();
                }
            }
        }
    }
}

fn note_sync_accept(state: &mut State, tick: u64, nbytes: usize) {
    let sequence = state.next_sequence;
    state.next_sequence += 1;
    state.written_records += 1;
    state.written_bytes += nbytes as u64;
    state
        .last_enqueue
        .insert(thread::current().id(), PeerEnqueue { tick, sequence });
}

fn sync_diagnostics(state: &State) -> PeerLogDiagnostics {
    PeerLogDiagnostics {
        queued_records: 0,
        queued_bytes: 0,
        written_records: state.written_records,
        written_bytes: state.written_bytes,
        dropped_lines: 0,
        dropped_bytes: 0,
        oversize_lines: 0,
        errors: state.errors,
        incomplete: state.failed,
        complete: state.finished,
        tick: state.tick,
        next_sequence: state.next_sequence,
    }
}

fn async_diagnostics(diag: &Diag) -> PeerLogDiagnostics {
    PeerLogDiagnostics {
        queued_records: diag.queued_records,
        queued_bytes: diag.queued_bytes,
        written_records: diag.written_records,
        written_bytes: diag.written_bytes,
        dropped_lines: diag.dropped_lines,
        dropped_bytes: diag.dropped_bytes,
        oversize_lines: diag.oversize_lines,
        errors: diag.errors,
        incomplete: diag.incomplete,
        complete: diag.complete,
        tick: diag.tick,
        next_sequence: diag.next_sequence,
    }
}

fn publish_async_diag(peer: u32, diag: &PeerLogDiagnostics, warn: bool) {
    let level = if warn { "WARN" } else { "INFO" };
    super::ring::push(
        level,
        PEER_ASYNC_TARGET,
        format!(
            "t={} peer={peer} peer_async queued_records={} queued_bytes={} written_records={} written_bytes={} dropped_lines={} dropped_bytes={} oversize_lines={} errors={} incomplete={} complete={} sequence={}",
            diag.tick,
            diag.queued_records,
            diag.queued_bytes,
            diag.written_records,
            diag.written_bytes,
            diag.dropped_lines,
            diag.dropped_bytes,
            diag.oversize_lines,
            diag.errors,
            u8::from(diag.incomplete),
            u8::from(diag.complete),
            diag.next_sequence.saturating_sub(1),
        ),
    );
}

struct EnqueueOutcome {
    result: io::Result<Option<PeerEnqueue>>,
    diag: Option<PeerLogDiagnostics>,
    warn: bool,
}

impl EnqueueOutcome {
    fn ok(receipt: Option<PeerEnqueue>) -> Self {
        Self {
            result: Ok(receipt),
            diag: None,
            warn: false,
        }
    }
}

fn publish_outcome(peer: u32, outcome: &EnqueueOutcome) {
    if let Some(diag) = &outcome.diag {
        publish_async_diag(peer, diag, outcome.warn);
    }
}

fn enqueue_line(
    async_peer: &AsyncPeer,
    lossy: bool,
    tick: u64,
    level: tracing::Level,
    target: &str,
    text: String,
) -> EnqueueOutcome {
    let nbytes = (text.len() + 1) as u64;
    let line = {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
        bytes
    };
    let ring = text;
    decide_enqueue(async_peer, lossy, nbytes, tick, |sequence| Job::Line {
        sequence,
        nbytes,
        line,
        level: level.as_str().to_owned(),
        target: target.to_owned(),
        ring,
    })
}

fn reserve_trace(async_peer: &AsyncPeer, name: &str) -> io::Result<()> {
    let mut diag = async_peer.shared.diag.lock();
    if diag.trace_names.contains(name) {
        return Ok(());
    }
    if diag.trace_names.len() >= async_peer.shared.max_hash_traces {
        return Err(io::Error::other("checkpoint file limit reached"));
    }
    diag.trace_names.insert(name.to_owned());
    Ok(())
}

fn enqueue_hash(async_peer: &AsyncPeer, tick: u64, name: &str, row: String, text: String) -> EnqueueOutcome {
    let nbytes = (row.len() + text.len() + 1) as u64;
    let ring = text.clone();
    let line = {
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(b'\n');
        bytes
    };
    let row = row.into_bytes();
    let name = name.to_owned();
    decide_enqueue(async_peer, false, nbytes, tick, |sequence| Job::Hash {
        sequence,
        nbytes,
        name,
        row,
        line,
        ring,
    })
}

fn decide_enqueue(
    async_peer: &AsyncPeer,
    lossy: bool,
    nbytes: u64,
    tick: u64,
    build: impl FnOnce(u64) -> Job,
) -> EnqueueOutcome {
    let mut diag = async_peer.shared.diag.lock();
    if diag.finished || diag.closing {
        return EnqueueOutcome {
            result: Err(io::Error::other("peer session already finished")),
            diag: None,
            warn: false,
        };
    }
    if diag.failed || diag.incomplete {
        return EnqueueOutcome {
            result: Err(io::Error::other("peer session had an IO failure")),
            diag: None,
            warn: false,
        };
    }
    let bounds = async_peer.shared.bounds;
    if nbytes > bounds.max_message_bytes as u64 || nbytes > bounds.max_queued_bytes as u64 {
        return reject_enqueue(&mut diag, lossy, nbytes, true);
    }
    let full = diag.queued_records >= bounds.max_records as u64
        || diag.queued_bytes.saturating_add(nbytes) > bounds.max_queued_bytes as u64;
    if full {
        return reject_enqueue(&mut diag, lossy, nbytes, false);
    }
    let sequence = diag.next_sequence;
    let job = build(sequence);
    let sender = async_peer.tx.lock().as_ref().cloned();
    let Some(sender) = sender else {
        diag.failed = true;
        diag.incomplete = true;
        diag.errors += 1;
        let snap = async_diagnostics(&diag);
        return EnqueueOutcome {
            result: Err(io::Error::other("peer log worker stopped")),
            diag: Some(snap),
            warn: true,
        };
    };
    match sender.try_send(job) {
        Ok(()) => {
            diag.queued_records += 1;
            diag.queued_bytes += nbytes;
            diag.next_sequence += 1;
            let receipt = PeerEnqueue { tick, sequence };
            drop(diag);
            async_peer
                .shared
                .last_enqueue
                .lock()
                .insert(thread::current().id(), receipt);
            EnqueueOutcome::ok(Some(receipt))
        }
        Err(TrySendError::Full(job)) => {
            drop(job);
            reject_enqueue(&mut diag, lossy, nbytes, false)
        }
        Err(TrySendError::Disconnected(_)) => {
            diag.failed = true;
            diag.incomplete = true;
            diag.errors += 1;
            let snap = async_diagnostics(&diag);
            EnqueueOutcome {
                result: Err(io::Error::other("peer log worker stopped")),
                diag: Some(snap),
                warn: true,
            }
        }
    }
}

fn reject_enqueue(diag: &mut Diag, lossy: bool, nbytes: u64, oversize: bool) -> EnqueueOutcome {
    if lossy {
        diag.dropped_lines += 1;
        diag.dropped_bytes += nbytes;
        if oversize {
            diag.oversize_lines += 1;
        }
        let snap = async_diagnostics(diag);
        return EnqueueOutcome {
            result: Ok(None),
            diag: Some(snap),
            warn: true,
        };
    }
    diag.failed = true;
    diag.incomplete = true;
    diag.errors += 1;
    let snap = async_diagnostics(diag);
    let message = if oversize {
        "async writer record exceeds max_message_bytes"
    } else {
        "async writer queue is full"
    };
    EnqueueOutcome {
        result: Err(io::Error::other(message)),
        diag: Some(snap),
        warn: true,
    }
}

fn drain_async(async_peer: &AsyncPeer, closing: bool) -> io::Result<()> {
    let through = {
        let _gate = async_peer.shared.gate.lock();
        let mut diag = async_peer.shared.diag.lock();
        if diag.finished {
            return Ok(());
        }
        if closing {
            diag.closing = true;
        }
        diag.next_sequence.saturating_sub(1)
    };
    let (ack_tx, ack_rx) = mpsc::channel();
    async_peer
        .shared
        .waiters
        .lock()
        .push(Waiter { through, ack: ack_tx });
    let sender = async_peer.tx.lock().as_ref().cloned();
    let Some(sender) = sender else {
        return Err(io::Error::other("peer log worker stopped"));
    };
    match sender.try_send(Job::Wake) {
        Ok(()) | Err(TrySendError::Full(_)) => {}
        Err(TrySendError::Disconnected(_)) => {
            return Err(io::Error::other("peer log worker stopped"));
        }
    }
    match ack_rx.recv() {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("peer log worker stopped")),
    }
}

fn spawn_async(
    peer: u32,
    directory: PathBuf,
    max_hash_traces: usize,
    bounds: AsyncWriterBounds,
    log: LogOut,
) -> io::Result<AsyncPeer> {
    let shared = std::sync::Arc::new(Shared {
        gate: Mutex::new(()),
        diag: Mutex::new(Diag::new()),
        last_enqueue: Mutex::new(HashMap::new()),
        waiters: Mutex::new(Vec::new()),
        bounds,
        max_hash_traces,
        peer,
    });
    let (tx, rx) = sync_channel(bounds.max_records.saturating_add(8));
    let thread_shared = std::sync::Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("regia-peer-log".into())
        .spawn(move || {
            worker_loop(
                rx,
                thread_shared,
                SessionIo {
                    log,
                    hashes: BTreeMap::new(),
                    directory,
                    max_hash_traces,
                },
            );
        })?;
    Ok(AsyncPeer {
        shared,
        tx: Mutex::new(Some(tx)),
        worker: Mutex::new(Some(worker)),
    })
}

fn worker_loop(rx: mpsc::Receiver<Job>, shared: std::sync::Arc<Shared>, mut io: SessionIo) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Wake => poke(&shared, &mut io),
            Job::Line {
                sequence,
                nbytes,
                line,
                level,
                target,
                ring,
            } => {
                let result = io.log.write_line(&line);
                account(&shared, sequence, nbytes, result.is_ok());
                if result.is_ok() {
                    super::ring::push(&level, &target, ring);
                } else {
                    let snap = async_diagnostics(&shared.diag.lock());
                    publish_async_diag(shared.peer, &snap, true);
                }
                poke(&shared, &mut io);
            }
            Job::Hash {
                sequence,
                nbytes,
                name,
                row,
                line,
                ring,
            } => {
                let result = append_hash(&mut io, &name, &row).and_then(|_| io.log.write_line(&line));
                account(&shared, sequence, nbytes, result.is_ok());
                if result.is_ok() {
                    super::ring::push("INFO", "regia_core::log::hash_trace", ring);
                } else {
                    let snap = async_diagnostics(&shared.diag.lock());
                    publish_async_diag(shared.peer, &snap, true);
                }
                poke(&shared, &mut io);
            }
        }
    }
    if persist(&mut io).is_err() {
        let _gate = shared.gate.lock();
        let mut diag = shared.diag.lock();
        diag.failed = true;
        diag.incomplete = true;
        diag.errors += 1;
    }
    poke(&shared, &mut io);
}

fn account(shared: &Shared, sequence: u64, nbytes: u64, ok: bool) {
    let _gate = shared.gate.lock();
    let mut diag = shared.diag.lock();
    diag.queued_records = diag.queued_records.saturating_sub(1);
    diag.queued_bytes = diag.queued_bytes.saturating_sub(nbytes);
    diag.written_through = sequence;
    if ok {
        diag.written_records += 1;
        diag.written_bytes += nbytes;
    } else {
        diag.errors += 1;
        diag.failed = true;
        diag.incomplete = true;
    }
}

fn append_hash(io: &mut SessionIo, name: &str, row: &[u8]) -> io::Result<()> {
    if !io.hashes.contains_key(name) {
        if io.hashes.len() >= io.max_hash_traces {
            return Err(io::Error::other("checkpoint file limit reached"));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(io.directory.join(format!("{name}.hash")))?;
        io.hashes.insert(name.to_owned(), BufWriter::new(file));
    }
    let Some(file) = io.hashes.get_mut(name) else {
        return Err(io::Error::other("checkpoint file missing"));
    };
    file.write_all(row)
}

fn poke(shared: &Shared, io: &mut SessionIo) {
    let through = shared.diag.lock().written_through;
    let due = {
        let mut waiters = shared.waiters.lock();
        let mut due = Vec::new();
        let mut keep = Vec::new();
        for waiter in waiters.drain(..) {
            if waiter.through <= through {
                due.push(waiter);
            } else {
                keep.push(waiter);
            }
        }
        *waiters = keep;
        due
    };
    if due.is_empty() {
        return;
    }
    let persist_result = persist(io);
    let failed = {
        let mut diag = shared.diag.lock();
        if persist_result.is_err() {
            diag.errors += 1;
            diag.failed = true;
            diag.incomplete = true;
        }
        diag.failed || diag.incomplete
    };
    let detail = persist_result.err().map(|err| err.to_string());
    for waiter in due {
        let result = if failed {
            Err(io::Error::other(
                detail
                    .clone()
                    .unwrap_or_else(|| "peer session had an IO failure".to_owned()),
            ))
        } else {
            Ok(())
        };
        let _ = waiter.ack.send(result);
    }
}

fn persist(io: &mut SessionIo) -> io::Result<()> {
    io.log.persist()?;
    for file in io.hashes.values_mut() {
        file.flush()?;
        file.get_ref().sync_all()?;
    }
    Ok(())
}

fn ensure_open(state: &State) -> io::Result<()> {
    if state.finished {
        return Err(io::Error::other("peer session already finished"));
    }
    if state.failed {
        return Err(io::Error::other("peer session had an IO failure"));
    }
    Ok(())
}

fn sync_state(state: &mut State) -> io::Result<()> {
    let result = (|| {
        state.log.flush()?;
        state.log.get_ref().sync_all()?;
        for file in state.hashes.values_mut() {
            file.flush()?;
            file.get_ref().sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        state.failed = true;
        state.errors += 1;
    }
    result
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session or trace name",
        ));
    }
    Ok(())
}

fn validate_session(name: &str) -> io::Result<()> {
    validate_name(name)?;
    let folded = name.to_ascii_lowercase();
    if folded.starts_with("unclean-") || folded.starts_with("pending-") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved session prefix",
        ));
    }
    Ok(())
}

fn escaped(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            let _ = write!(out, "{}", c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Policy name in `privacy.txt`. Report-source and outbox jobs treat only this name as a
/// sanitized recovery payload.
pub const RECOVERY_PRIVACY_POLICY: &str = "regia.recovery.privacy";

/// `privacy.txt` schema. A reader accepts this version; older directories have no manifest.
pub const RECOVERY_PRIVACY_POLICY_VERSION: u32 = 1;

/// Upper bound on bytes read from a session log and on bytes written to `lastlines.log`,
/// applied after privacy replacement. `SessionOptions::tail_bytes` is clamped to this.
pub const RECOVERY_REPORT_BYTE_CAP: u64 = 8 * 1024 * 1024;

/// File names a published recovery report may contain. Anything else is unknown content.
pub const RECOVERY_REPORT_FILES: [&str; 3] = ["report.txt", "lastlines.log", "privacy.txt"];

/// Evidence of a previous peer session without a successful clean completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncleanSession {
    /// Previous session directory name, including the peer suffix.
    pub session: String,
    /// Atomically published `unclean-<session>-peer<id>` directory.
    pub directory: PathBuf,
    /// Number of nonempty human log lines retained in the published tail.
    pub lines: usize,
    /// Report fields and tail lines passed through `privacy::scrub_report` before publication.
    pub sanitized: bool,
    /// The published tail kept every selected line: no partial line, no invalid UTF-8, and the
    /// scrubbed text fit in the byte cap without dropping a line.
    pub complete: bool,
    /// [`RECOVERY_PRIVACY_POLICY_VERSION`] written into `privacy.txt`.
    pub policy_version: u32,
}

/// Parsed `privacy.txt` for one published recovery directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryManifest {
    /// [`RECOVERY_PRIVACY_POLICY`] when the file is one of ours.
    pub policy: String,
    /// [`RECOVERY_PRIVACY_POLICY_VERSION`] the file claims.
    pub policy_version: u32,
    /// Report text was passed through the privacy scrubber.
    pub sanitized: bool,
    /// See [`UncleanSession::complete`].
    pub complete: bool,
    /// The selected window was not valid UTF-8. Replacement characters are explicit.
    pub invalid_utf8: bool,
    /// The byte window started mid-line and that fragment was omitted.
    pub partial_line: bool,
    /// Byte length of the published `lastlines.log`, measured after scrubbing and the cap.
    pub published_bytes: u64,
    /// Bytes read from `session.log` for this report, at most the clamped tail cap.
    pub source_bytes: u64,
    /// Nonempty scrubbed lines in `lastlines.log`.
    pub lines: u64,
}

/// Arms the next recovery publication to fail after the staged files are synced and before
/// the directory is renamed. Staging is removed. One shot. Hosts do not call this.
#[doc(hidden)]
pub fn arm_recovery_publish_interrupt() {
    PUBLISH_INTERRUPT.store(true, Ordering::SeqCst);
}

/// Whether `dir` is a finished sanitized recovery report an outbox job may select.
///
/// True only when the directory name starts with `unclean-`, the directory is a real directory,
/// and `privacy.txt` names [`RECOVERY_PRIVACY_POLICY`] at [`RECOVERY_PRIVACY_POLICY_VERSION`]
/// with `sanitized=1`. A `pending-*` staging directory is not published. Tail completeness is
/// [`RecoveryManifest::complete`] and is not required here.
#[must_use]
pub fn is_published_recovery_report(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.starts_with("unclean-") {
        return false;
    }
    match read_recovery_manifest(dir) {
        Ok(manifest) => {
            manifest.sanitized
                && manifest.policy == RECOVERY_PRIVACY_POLICY
                && manifest.policy_version == RECOVERY_PRIVACY_POLICY_VERSION
        }
        Err(_) => false,
    }
}

/// Reads one allowlisted report file. Symlinks, other file types, names outside
/// [`RECOVERY_REPORT_FILES`] and paths that resolve outside `dir` are refused.
/// The returned bytes are whatever the file holds; publication writes only scrubbed text.
///
/// # Errors
/// Returns an error when the name is unknown, the file is a link or not a regular file,
/// the file resolves outside `dir`, or it is larger than [`RECOVERY_REPORT_BYTE_CAP`].
pub fn read_recovery_file(dir: &Path, name: &str) -> io::Result<Vec<u8>> {
    if !RECOVERY_REPORT_FILES.contains(&name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown recovery report file",
        ));
    }
    let (file, len) = open_regular_inside(dir, name)?;
    if len > RECOVERY_REPORT_BYTE_CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report file exceeds the byte cap",
        ));
    }
    let mut bytes = Vec::new();
    file.take(RECOVERY_REPORT_BYTE_CAP + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECOVERY_REPORT_BYTE_CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report file exceeds the byte cap",
        ));
    }
    Ok(bytes)
}

/// Parses `privacy.txt` in a recovery directory.
///
/// # Errors
/// Returns an error when the directory or the manifest is a link, not a regular file,
/// outside the directory, or not the key set this version writes.
pub fn read_recovery_manifest(dir: &Path) -> io::Result<RecoveryManifest> {
    let dir_meta = fs::symlink_metadata(dir)?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report directory is a link or not a directory",
        ));
    }
    let bytes = read_recovery_file(dir, "privacy.txt")?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "recovery manifest is not utf-8"))?;
    let mut policy = None;
    let mut policy_version = None;
    let mut sanitized = None;
    let mut complete = None;
    let mut invalid_utf8 = None;
    let mut partial_line = None;
    let mut published_bytes = None;
    let mut source_bytes = None;
    let mut lines = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recovery manifest line",
            ));
        };
        let parsed_flag = match value {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
        match key {
            "policy" => policy = Some(value.to_owned()),
            "policy_version" => {
                policy_version = Some(value.parse::<u32>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "recovery manifest policy_version")
                })?);
            }
            "sanitized" => sanitized = parsed_flag,
            "complete" => complete = parsed_flag,
            "invalid_utf8" => invalid_utf8 = parsed_flag,
            "partial_line" => partial_line = parsed_flag,
            "published_bytes" => {
                published_bytes = Some(value.parse::<u64>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "recovery manifest published_bytes")
                })?);
            }
            "source_bytes" => {
                source_bytes = Some(value.parse::<u64>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "recovery manifest source_bytes")
                })?);
            }
            "lines" => {
                lines =
                    Some(value.parse::<u64>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "recovery manifest lines")
                    })?);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown recovery manifest field",
                ));
            }
        }
    }
    Ok(RecoveryManifest {
        policy: policy.ok_or_else(manifest_field)?,
        policy_version: policy_version.ok_or_else(manifest_field)?,
        sanitized: sanitized.ok_or_else(manifest_field)?,
        complete: complete.ok_or_else(manifest_field)?,
        invalid_utf8: invalid_utf8.ok_or_else(manifest_field)?,
        partial_line: partial_line.ok_or_else(manifest_field)?,
        published_bytes: published_bytes.ok_or_else(manifest_field)?,
        source_bytes: source_bytes.ok_or_else(manifest_field)?,
        lines: lines.ok_or_else(manifest_field)?,
    })
}

fn manifest_field() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "recovery manifest field")
}

/// Checks the newest older inactive session for this peer. Active OS leases are skipped.
/// Existing evidence is not overwritten. IO failures are returned, never called a clean exit.
/// A marker contains `report.txt`, bounded `lastlines.log` and `privacy.txt`. It uploads nothing.
/// Report fields and tail lines are scrubbed; the peer's `session.log` and `.hash` files are not.
pub fn check_previous(
    root: &Path,
    current: &str,
    peer: u32,
    options: SessionOptions,
) -> io::Result<Option<UncleanSession>> {
    let _span = tracing::trace_span!("recover_peer_session").entered();
    validate_session(current)?;
    let suffix = format!("-peer{peer}");
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(session) = name.strip_suffix(&suffix) else {
            continue;
        };
        if session < current && validate_session(session).is_ok() {
            candidates.push((session.to_owned(), name));
        }
    }
    // Sort identifiers, not the directory suffix: "a-" follows "a", but
    // "a--peer0" sorts before "a-peer0".
    candidates.sort();
    for (_, name) in candidates.into_iter().rev() {
        let source = root.join(&name);
        if !source.join("ready").try_exists()? {
            continue;
        }
        let lease = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(source.join("active.lock"))
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // creation not complete
            Err(e) => return Err(e),
        };
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(e)) => return Err(e),
        }
        let marker = root.join(format!("unclean-{name}"));
        if source.join("clean").try_exists()? || marker.try_exists()? {
            return Ok(None);
        }
        let prepared = prepare_report(&source, options)?;
        let found = UncleanSession {
            session: name.clone(),
            directory: marker,
            lines: prepared.lines.len(),
            sanitized: true,
            complete: prepared.complete,
            policy_version: RECOVERY_PRIVACY_POLICY_VERSION,
        };
        publish_report(root, &found, &prepared)?;
        let notice = crate::privacy::scrub_report(&format!(
            "t={} peer={peer} unclean_session session={name} lines={} sanitized=1 complete={} policy_version={} invalid_utf8={} partial_line={}",
            super::tick(),
            found.lines,
            u8::from(prepared.complete),
            RECOVERY_PRIVACY_POLICY_VERSION,
            u8::from(prepared.invalid_utf8),
            u8::from(prepared.partial_line),
        ));
        super::ring::push("WARN", "regia_core::log::unclean", notice);
        return Ok(Some(found));
    }
    Ok(None)
}

static PUBLISH_INTERRUPT: AtomicBool = AtomicBool::new(false);

struct PreparedReport {
    lines: Vec<String>,
    source_bytes: u64,
    published_bytes: u64,
    invalid_utf8: bool,
    partial_line: bool,
    complete: bool,
}

fn tail_cap(requested: u64) -> u64 {
    requested.min(RECOVERY_REPORT_BYTE_CAP)
}

fn prepare_report(dir: &Path, options: SessionOptions) -> io::Result<PreparedReport> {
    let cap = tail_cap(options.tail_bytes);
    let (mut file, len) = open_regular_inside(dir, "session.log")?;
    let from = len.saturating_sub(cap);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes)?;
    let source_bytes = bytes.len() as u64;
    let invalid_utf8 = std::str::from_utf8(&bytes).is_err();
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    let partial_line = if from > 0 {
        match text.split_once('\n') {
            Some((_, rest)) => text = rest.to_owned(),
            None => text.clear(),
        }
        true
    } else {
        false
    };
    let all: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    let window = &all[all.len().saturating_sub(options.tail_lines)..];
    let scrubbed: Vec<String> = window
        .iter()
        .map(|line| crate::privacy::scrub_report(line))
        .collect();
    let (lines, dropped) = cap_scrubbed(scrubbed, cap);
    let published_bytes = published_len(&lines);
    let complete = !invalid_utf8 && !partial_line && !dropped;
    Ok(PreparedReport {
        lines,
        source_bytes,
        published_bytes,
        invalid_utf8,
        partial_line,
        complete,
    })
}

fn published_len(lines: &[String]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

/// Drops oldest scrubbed lines until the tail fits. A single over-long line is omitted
/// whole so a cap cannot cut through a replacement token.
fn cap_scrubbed(mut lines: Vec<String>, max_bytes: u64) -> (Vec<String>, bool) {
    let mut dropped = false;
    loop {
        if published_len(&lines) <= max_bytes {
            return (lines, dropped);
        }
        dropped = true;
        if lines.len() <= 1 {
            lines.clear();
            return (lines, true);
        }
        lines.remove(0);
    }
}

fn open_regular_inside(dir: &Path, name: &str) -> io::Result<(File, u64)> {
    if name.is_empty() || name.contains(['/', '\\', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report source name",
        ));
    }
    let dir_meta = fs::symlink_metadata(dir)?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report directory is a link or not a directory",
        ));
    }
    let path = dir.join(name);
    let meta = fs::symlink_metadata(&path)?;
    let kind = meta.file_type();
    if kind.is_symlink() || !kind.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report source is a link or not a regular file",
        ));
    }
    let root = fs::canonicalize(dir)?;
    let canon = fs::canonicalize(&path)?;
    if !canon.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery report source is outside the session directory",
        ));
    }
    let file = File::open(&path)?;
    Ok((file, meta.len()))
}

fn publish_report(root: &Path, found: &UncleanSession, prepared: &PreparedReport) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staging = root.join(format!(
        "pending-{}-{}-{}",
        found.session,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&staging)?;
    let result = (|| {
        let report_body = crate::privacy::scrub_report(&format!(
            "REGIA UNCLEAN SESSION\nsession={}\nlines={}\npolicy={}\npolicy_version={}\nsanitized=1\ncomplete={}\nNo successful clean completion marker.\n",
            found.session,
            found.lines,
            RECOVERY_PRIVACY_POLICY,
            found.policy_version,
            u8::from(prepared.complete),
        ));
        {
            let mut report = File::create(staging.join("report.txt"))?;
            report.write_all(report_body.as_bytes())?;
            report.sync_all()?;
        }
        {
            let mut tail = File::create(staging.join("lastlines.log"))?;
            for line in &prepared.lines {
                writeln!(tail, "{line}")?;
            }
            tail.sync_all()?;
        }
        let manifest = crate::privacy::scrub_report(&format!(
            "policy={}\npolicy_version={}\nsanitized=1\ncomplete={}\ninvalid_utf8={}\npartial_line={}\npublished_bytes={}\nsource_bytes={}\nlines={}\n",
            RECOVERY_PRIVACY_POLICY,
            found.policy_version,
            u8::from(prepared.complete),
            u8::from(prepared.invalid_utf8),
            u8::from(prepared.partial_line),
            prepared.published_bytes,
            prepared.source_bytes,
            prepared.lines.len(),
        ));
        {
            let mut file = File::create(staging.join("privacy.txt"))?;
            file.write_all(manifest.as_bytes())?;
            file.sync_all()?;
        }
        if PUBLISH_INTERRUPT.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("recovery publication interrupted"));
        }
        fs::rename(&staging, &found.directory)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_test_utils::TempDir;

    #[test]
    #[cfg(target_os = "linux")]
    fn a_disk_full_flush_never_publishes_clean() {
        let _serial = crate::log::ring::RING_TESTS.lock();
        let root = TempDir::new("peer-disk-full");
        let log = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
        let PeerInner::Sync(state) = &log.inner else {
            panic!("the default writer is synchronous");
        };
        state.lock().log = BufWriter::new(
            OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .expect("test operation"),
        );
        log.line(tracing::Level::INFO, "net", format_args!("buffered event"))
            .expect("test operation");
        assert!(log.flush().is_err());
        assert!(log.finish().is_err());
        assert!(!log.directory().join("clean").exists());
        assert!(
            log.line(tracing::Level::INFO, "net", format_args!("after failure"))
                .is_err()
        );
    }

    #[test]
    fn failed_recovery_does_not_publish_a_partial_marker() {
        let _serial = crate::log::ring::RING_TESTS.lock();
        let root = TempDir::new("peer-recovery-error");
        let log = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
        let directory = log.directory().to_owned();
        drop(log);
        fs::remove_file(directory.join("session.log")).expect("test operation");
        assert!(check_previous(&root, "002", 0, SessionOptions::default()).is_err());
        assert!(!root.join("unclean-001-peer0").exists());
    }

    #[test]
    fn dropping_flushes_before_releasing_the_lease_without_claiming_clean() {
        let _serial = crate::log::ring::RING_TESTS.lock();
        let root = TempDir::new("peer-drop");
        let log = PeerLog::create(&root, "001", 0, SessionOptions::default()).expect("test operation");
        log.set_tick(9);
        log.line(tracing::Level::INFO, "net", format_args!("last buffered event"))
            .expect("test operation");
        drop(log);
        let found = check_previous(&root, "002", 0, SessionOptions::default())
            .expect("test operation")
            .expect("test operation");
        let tail = fs::read_to_string(found.directory.join("lastlines.log")).expect("test operation");
        assert!(tail.ends_with("t=9 peer=0 INFO net last buffered event\n"));
        assert!(!root.join("001-peer0/clean").exists());
    }
}
