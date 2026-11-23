// SPDX-License-Identifier: MPL-2.0
//! The Outliner and Inspector on a running game outside the editor (`docs/specs/19-editor.md` § 4,
//! addendum § 15.5, ADR-7833, `book/agents/live-edit.md`).
//!
//! Both panels carry a **source switch**: *Edit* (the edit world), *Play* (the in-process play
//! world, while play runs) and one entry per connected runtime: a standalone launch
//! (`editor.launch`, which gives the game its own agent directory), each separate-process PIE child
//! (`PieProcesses::agent_dir`) and any agent directory attached through [`attach`]. A runtime is
//! reached through its own agent server with `regia_agent`'s client, the way the Profiler's process
//! source is: the endpoint and token are read from the agent directory, and only a loopback
//! endpoint is ever dialled.
//!
//! With a runtime selected:
//!
//! - the **Outliner** lists the runtime's entities from `scene.query_entities`, one page of
//!   [`PAGE`] rows at a time (`More` asks for the next page), read again at most every
//!   [`REFRESH_INTERVAL`] while the panel is drawn. The rows form a **tree** by the `parent` each
//!   listed row carries (a child whose parent is not on the listed pages shows at the root). The
//!   rows are keyboard rows ([`crate::canvas_keys::list_rows`]): Up/Down move, Enter or Space
//!   selects, Ctrl+Enter or Ctrl+click adds to the selection, Left hides a row's children (or
//!   moves to its parent), Right shows them; *Show or hide children* does the same for the
//!   selected row. Which rows are collapsed is kept per source for the editor session.
//! - the **Inspector** reads each selected entity with `scene.get_entity` and shows its components'
//!   fields: numbers, switches, text (asset paths are text: a project-relative path) and, for a
//!   field whose type the editor knows as an enum, a choice of its unit variants. An edit sets the
//!   whole component through one `scene.apply_edits` transaction, on every selected entity that has
//!   the component (a multi-select edit applies to all), and reads the result back. A gesture is one
//!   transaction: a number dragged over many frames, or text typed into a field, is sent once, when
//!   the drag ends or the field is left (Enter or Tab); the panel shows the value meanwhile.
//! - a banner says that changes are not saved: the runtime's world is not the level on disk.
//!
//! Remote calls never run on the editor thread: each connection has its own thread that dials the
//! runtime and runs the calls in order, fed by a queue of at most [`QUEUE_DEPTH`] jobs. The panels
//! queue a read or an edit and draw what has come back; a full queue drops a read (the next frame
//! asks again) and keeps an edit for the next frame. The agent methods queue their call and wait
//! for its answer. A call has a [`CALL_TIMEOUT`] read timeout; a refused or dropped connection
//! shows as an error on the panel and is retried at the next read.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use regia_agent::client::{Client, ClientError};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::enums::VariantInfo;
use regia_ecs::reflect::{AppTypeRegistry, TypeInfo};
use serde_json::{Map, Value, json};

use crate::viewport_agent::Handler;

/// Rows of the remote Outliner per page (`scene.query_entities { limit }` grows by this much).
pub const PAGE: usize = 200;
/// The most pages the remote Outliner asks for (`PAGE * MAX_PAGES` rows).
pub const MAX_PAGES: usize = 50;
/// The shortest time between two reads of the remote tree while a panel shows it.
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
/// The longest a remote call may take before it counts as failed.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// The most selected entities the Inspector reads and an edit writes.
pub const MAX_SELECTED: usize = 64;
/// The most jobs waiting for a connection's thread; a panel read past it is dropped, an edit waits.
pub const QUEUE_DEPTH: usize = 8;
/// The longest an agent method waits for one queued call to come back (the jobs before it included).
pub const WAIT_LIMIT: Duration = Duration::from_secs(30);

/// The source id of the edit world.
pub const EDIT: &str = "edit";
/// The source id of the in-process play world.
pub const PLAY: &str = "play";
/// The source id of the game an Android launch runs on a device (ADR-8472).
pub const DEVICE: &str = "device";

/// The protocol's error codes (lowercase ascii, listed on `book/agents/live-edit.md`).
pub mod codes {
    /// The named source is not listed by `editor.remote_sources`.
    pub const NO_SOURCE: &str = "remote_no_source";
    /// The runtime could not be reached (no endpoint yet, refused, dropped or timed out).
    pub const NOT_CONNECTED: &str = "remote_not_connected";
    /// The runtime refused a call (its own error is in the message).
    pub const CALL_FAILED: &str = "remote_call_failed";
    /// `editor.remote_set` without a remote source selected, or with nothing selected there.
    pub const NO_SELECTION: &str = "remote_no_selection";
    /// The field path does not name a field of the component's value.
    pub const BAD_PATH: &str = "remote_bad_path";

    /// Every code, for the book page and its test.
    pub const ALL: &[&str] = &[NO_SOURCE, NOT_CONNECTED, CALL_FAILED, NO_SELECTION, BAD_PATH];
}

fn error(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/live-edit.md")
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A connection to a runtime's agent server: one JSON-RPC call at a time.
pub trait RemoteTransport: Send {
    /// Calls `method` and returns its result, or the runtime's (or the connection's) error text.
    ///
    /// # Errors
    ///
    /// [`RemoteError::Connection`] when the connection failed or timed out (the caller reconnects),
    /// [`RemoteError::Refused`] when the runtime answered with an error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RemoteError>;
}

/// Why a remote call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// The connection failed, dropped or timed out.
    Connection(String),
    /// The runtime answered with an error.
    Refused(String),
}

impl RemoteError {
    /// The protocol error of this failure.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        match self {
            Self::Connection(text) => error(codes::NOT_CONNECTED, text.clone()),
            Self::Refused(text) => error(codes::CALL_FAILED, text.clone()),
        }
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(text) => write!(f, "not connected: {text}"),
            Self::Refused(text) => write!(f, "the runtime refused: {text}"),
        }
    }
}

/// A runtime reached through `regia_agent`'s client.
pub struct AgentTransport(Client);

impl RemoteTransport for AgentTransport {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RemoteError> {
        self.0.call(method, params).map_err(|err| match err {
            ClientError::Rpc(rpc) => RemoteError::Refused(format!("{} ({})", rpc.message, rpc.code)),
            other => RemoteError::Connection(other.to_string()),
        })
    }
}

/// Connects to the runtime advertised in agent directory `dir` (`endpoint.json` + `token`), with a
/// [`CALL_TIMEOUT`] read timeout.
///
/// # Errors
///
/// [`RemoteError::Connection`] when the directory has no endpoint yet or the connection fails
/// (the client refuses any endpoint that is not loopback).
pub fn connect_agent_dir(dir: &std::path::Path) -> Result<AgentTransport, RemoteError> {
    let (endpoint, token) = regia_agent::endpoint::read(dir)
        .map_err(|e| RemoteError::Connection(format!("no endpoint in {} yet: {e}", dir.display())))?;
    let mut client =
        Client::connect(&endpoint.url, &token).map_err(|e| RemoteError::Connection(e.to_string()))?;
    client
        .set_read_timeout(Some(CALL_TIMEOUT))
        .map_err(|e| RemoteError::Connection(e.to_string()))?;
    Ok(AgentTransport(client))
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// One entry of the source switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSource {
    /// Stable id (`edit`, `play`, `launch`, `device`, `pie-server`, `pie-client-<k>`, `attached-<n>`).
    pub id: String,
    /// What the switch shows.
    pub label: String,
    /// `edit`, `play`, `launch`, `device`, `pie_process` or `attached`.
    pub kind: &'static str,
    /// The runtime's agent directory (remote sources only).
    pub dir: Option<PathBuf>,
}

impl RemoteSource {
    /// Whether the source is a runtime outside the editor.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.dir.is_some()
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "kind": self.kind,
            "remote": self.is_remote(),
            "agent_dir": self.dir.as_ref().map(|d| d.display().to_string()),
        })
    }
}

/// One row of the remote Outliner.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRow {
    /// The runtime's entity id (`Entity::to_bits`).
    pub entity: u64,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// Its kind, when it has one.
    pub kind: Option<String>,
    /// Its parent's entity id, when it has one (`scene.query_entities` names it).
    pub parent: Option<u64>,
}

impl RemoteRow {
    /// The row's text: the name, else `Entity <id>`, with the kind after it.
    #[must_use]
    pub fn label(&self) -> String {
        let name = self
            .name
            .clone()
            .unwrap_or_else(|| format!("Entity {}", self.entity));
        match &self.kind {
            Some(kind) => format!("{name} ({kind})"),
            None => name,
        }
    }
}

// ---------------------------------------------------------------------------
// The connection's thread
// ---------------------------------------------------------------------------

/// What a job answers (downcast by whoever queued it).
pub type Answer = Box<dyn Any + Send + Sync>;

/// A unit of work for a connection's thread: it runs against the connection and answers anything.
pub type Job = Box<dyn FnOnce(&mut dyn RemoteTransport) -> Answer + Send>;

/// What a queued job was for, so its answer reaches the right state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// A read of the tree and the selected entities (the panels').
    Refresh,
    /// An Inspector edit (one transaction) and the read after it.
    Edit,
    /// A live link pass (`crate::live_link`).
    Link,
    /// A call an agent method waits for.
    Call,
}

/// A finished job.
pub struct Finished {
    ticket: u64,
    /// What the job was for.
    pub kind: JobKind,
    /// The job's answer.
    pub out: Answer,
}

struct Queued {
    ticket: u64,
    kind: JobKind,
    job: Job,
}

/// Why a job was not queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// [`QUEUE_DEPTH`] jobs are waiting already.
    Full,
    /// No runtime source is selected, or its thread is gone.
    Closed(RemoteError),
}

/// A transport the thread hands to a job: marks a dropped connection, and stands in (failing every
/// call) while the runtime cannot be reached.
struct Tracked<'t> {
    inner: Option<&'t mut dyn RemoteTransport>,
    unreachable: Option<RemoteError>,
    lost: bool,
}

impl RemoteTransport for Tracked<'_> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RemoteError> {
        let Some(inner) = self.inner.as_deref_mut() else {
            return Err(self
                .unreachable
                .clone()
                .unwrap_or_else(|| RemoteError::Connection("no connection".into())));
        };
        let result = inner.call(method, params);
        if matches!(result, Err(RemoteError::Connection(_))) {
            self.lost = true;
        }
        result
    }
}

/// One runtime's connection: a thread that owns the transport and runs queued jobs in order.
struct Worker {
    source: String,
    jobs: SyncSender<Queued>,
    /// Behind a lock only so the resource is `Sync`; only the editor thread takes from it.
    done: Mutex<Receiver<Finished>>,
    connected: Arc<AtomicBool>,
    next: u64,
}

impl Worker {
    /// Starts the thread for `source`: it dials `dir` (again after a dropped connection) unless a
    /// `preset` transport is given.
    fn start(source: &str, dir: Option<PathBuf>, preset: Option<Box<dyn RemoteTransport>>) -> Self {
        let (jobs, queue) = mpsc::sync_channel::<Queued>(QUEUE_DEPTH);
        let (answers, done) = mpsc::channel::<Finished>();
        let connected = Arc::new(AtomicBool::new(preset.is_some()));
        let flag = connected.clone();
        let started = std::thread::Builder::new()
            .name(format!("regia-remote-{source}"))
            .spawn(move || {
                let mut transport = preset;
                while let Ok(Queued { ticket, kind, job }) = queue.recv() {
                    let _span = tracing::debug_span!(target: "regia_editor", "remote_job", ?kind).entered();
                    let mut unreachable = None;
                    if transport.is_none()
                        && let Some(dir) = &dir
                    {
                        match connect_agent_dir(dir) {
                            Ok(dialled) => transport = Some(Box::new(dialled)),
                            Err(err) => unreachable = Some(err),
                        }
                    }
                    let mut tracked = Tracked {
                        inner: transport.as_deref_mut().map(|t| t as &mut dyn RemoteTransport),
                        unreachable,
                        lost: false,
                    };
                    let out = job(&mut tracked);
                    // A dropped connection is dialled again for the next job (a preset one is kept).
                    if tracked.lost && dir.is_some() {
                        transport = None;
                    }
                    flag.store(transport.is_some(), Ordering::Relaxed);
                    if answers.send(Finished { ticket, kind, out }).is_err() {
                        break;
                    }
                }
            });
        if let Err(err) = started {
            tracing::warn!(target: "regia_editor", "remote connection thread: {err}");
        }
        Self {
            source: source.to_owned(),
            jobs,
            done: Mutex::new(done),
            connected,
            next: 0,
        }
    }

    /// The next answer, waiting up to `timeout`.
    fn recv(&self, timeout: Duration) -> Result<Finished, RecvTimeoutError> {
        match self.done.lock() {
            Ok(done) => done.recv_timeout(timeout),
            Err(_) => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// Every answer that came back, without waiting.
    fn drain(&self) -> Vec<Finished> {
        match self.done.lock() {
            Ok(done) => done.try_iter().collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The remote Inspector's edit in progress: kept (and shown) while the gesture lasts, sent as one
/// transaction when it ends.
#[derive(Debug, Clone, PartialEq)]
struct PendingEdit {
    component: String,
    path: String,
    value: Value,
}

/// What a read of the runtime answered.
struct RefreshOut {
    /// The listed rows; `None` when the listing failed.
    rows: Option<Vec<RemoteRow>>,
    /// The entities the read asked for.
    asked: Vec<u64>,
    inspected: Vec<(u64, Map<String, Value>)>,
    /// Component short name → type path.
    types: Vec<(String, String)>,
    error: Option<String>,
}

/// What an edit answered: the transaction's result and the read after it.
struct EditOut {
    result: Result<Value, RemoteError>,
    read: RefreshOut,
}

/// Reads the tree (`limit` rows) and `selection`'s components through `transport`.
fn read_runtime(transport: &mut dyn RemoteTransport, limit: usize, selection: Vec<u64>) -> RefreshOut {
    let mut out = RefreshOut {
        rows: None,
        asked: selection.clone(),
        inspected: Vec::new(),
        types: Vec::new(),
        error: None,
    };
    let rows = match transport.call("scene.query_entities", json!({ "limit": limit })) {
        Ok(answer) => rows_of(&answer),
        Err(err) => {
            out.error = Some(err.to_string());
            return out;
        }
    };
    let alive: BTreeSet<u64> = rows.iter().map(|r| r.entity).collect();
    out.rows = Some(rows);
    // A selected entity past the listed pages is still read (its own `get_entity` says if it is gone).
    for entity in selection {
        match transport.call("scene.get_entity", json!({ "entity": entity })) {
            Ok(answer) => {
                let mut components = Map::new();
                if let Some(listed) = answer.get("components").and_then(Value::as_object) {
                    for (name, entry) in listed {
                        if let Some(path) = entry.get("type").and_then(Value::as_str) {
                            out.types.push((name.clone(), path.to_owned()));
                        }
                        if let Some(value) = entry.get("value") {
                            components.insert(name.clone(), value.clone());
                        }
                    }
                }
                out.inspected.push((entity, components));
            }
            Err(RemoteError::Refused(_)) if !alive.contains(&entity) => {}
            Err(err) => out.error = Some(err.to_string()),
        }
    }
    out
}

/// The source switch and the remote view's state (an edit-world resource).
#[derive(Resource, Default)]
pub struct RemoteWorld {
    /// The selected source id; empty means automatic (the play world while play runs, else the
    /// edit world).
    selected: String,
    /// Agent directories attached by [`attach`]: `(label, dir)`.
    attached: Vec<(String, PathBuf)>,
    worker: Option<Worker>,
    /// Answers taken off the queue while waiting for another, not handled yet.
    stash: Vec<Finished>,
    /// Live link answers, for `crate::live_link::pump`.
    link_answers: Vec<Answer>,
    /// The panel read in flight.
    refreshing: Option<u64>,
    /// Edits in flight: a read queued before one of them is not shown over its result.
    edits: BTreeSet<u64>,
    /// The remote tree as last read.
    pub rows: Vec<RemoteRow>,
    /// Pages of the tree asked for (at least 1).
    pages: usize,
    /// The selected remote entities.
    pub selection: Vec<u64>,
    /// The selected entities' components as last read: `(entity, { short name: value })`.
    pub inspected: Vec<(u64, Map<String, Value>)>,
    /// Component short name → type path, as the runtime last named them.
    types: HashMap<String, String>,
    /// The last connection or call error.
    pub error: Option<String>,
    last_refresh: Option<Instant>,
    refreshed_frame: Option<u64>,
    keys: crate::canvas_keys::ListKeys,
    /// The Inspector's edit in progress (one transaction per gesture).
    pending: Option<PendingEdit>,
    /// The collapsed rows of each source (its id → entity ids), kept for the editor session.
    collapsed: HashMap<String, BTreeSet<u64>>,
    /// Counts the connection threads started (a job of an older one never answers).
    generation: u64,
    /// The egui frame whose Outliner click (More, a row Activate) already acted: a repeated pass of
    /// the frame must not ask for a second page or toggle a Ctrl pick back (review sprint-w12-rev-g10
    /// rows 7-8).
    outliner_frame: Option<u64>,
}

impl std::fmt::Debug for RemoteWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteWorld")
            .field("selected", &self.selected)
            .field("rows", &self.rows.len())
            .field("selection", &self.selection)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl RemoteWorld {
    /// The selected source id as set (empty: automatic).
    #[must_use]
    pub fn selected(&self) -> &str {
        &self.selected
    }

    /// Whether a connection to the selected runtime is open.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|w| w.source == self.selected && w.connected.load(Ordering::Relaxed))
    }

    /// The collapsed rows of the selected source.
    #[must_use]
    pub fn collapsed(&self) -> BTreeSet<u64> {
        self.collapsed.get(&self.selected).cloned().unwrap_or_default()
    }

    /// Hides (`collapse`) or shows the children of `entity` in the selected source's tree.
    pub fn set_collapsed(&mut self, entity: u64, collapse: bool) {
        let set = self.collapsed.entry(self.selected.clone()).or_default();
        if collapse {
            set.insert(entity);
        } else {
            set.remove(&entity);
        }
    }

    /// The selected source's thread, started (dialling `dir`) when there is none for it.
    fn worker(&mut self, dir: Option<&Path>) -> &mut Worker {
        let stale = self.worker.as_ref().is_none_or(|w| w.source != self.selected);
        if stale {
            self.generation += 1;
            self.worker = None;
            self.stash.clear();
            self.link_answers.clear();
            self.refreshing = None;
            self.edits.clear();
        }
        let selected = &self.selected;
        self.worker
            .get_or_insert_with(|| Worker::start(selected, dir.map(Path::to_path_buf), None))
    }

    /// Queues `job` for the selected runtime at agent directory `dir` without waiting; answers its
    /// ticket.
    ///
    /// # Errors
    ///
    /// [`QueueError::Full`] when [`QUEUE_DEPTH`] jobs wait already, [`QueueError::Closed`] when the
    /// connection's thread is gone (the next job starts a new one).
    pub fn try_queue(&mut self, dir: &Path, kind: JobKind, job: Job) -> Result<u64, QueueError> {
        let worker = self.worker(Some(dir));
        let ticket = worker.next;
        match worker.jobs.try_send(Queued { ticket, kind, job }) {
            Ok(()) => {
                worker.next += 1;
                Ok(ticket)
            }
            Err(TrySendError::Full(_)) => Err(QueueError::Full),
            Err(TrySendError::Disconnected(_)) => {
                self.worker = None;
                Err(QueueError::Closed(RemoteError::Connection(
                    "the connection's thread stopped".into(),
                )))
            }
        }
    }

    /// Queues `job` (waiting for room in the queue) and waits for its answer; answers that come back
    /// meanwhile are kept for [`Self::poll`].
    fn queue_and_wait(&mut self, dir: &Path, kind: JobKind, job: Job) -> Result<Answer, RemoteError> {
        let gone = || RemoteError::Connection("the connection's thread stopped".into());
        let worker = self.worker(Some(dir));
        let ticket = worker.next;
        worker.next += 1;
        if worker.jobs.send(Queued { ticket, kind, job }).is_err() {
            self.worker = None;
            return Err(gone());
        }
        loop {
            let Some(worker) = self.worker.as_ref() else {
                return Err(gone());
            };
            match worker.recv(WAIT_LIMIT) {
                Ok(finished) if finished.ticket == ticket => return Ok(finished.out),
                Ok(other) => self.stash.push(other),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RemoteError::Connection(format!(
                        "no answer within {} s",
                        WAIT_LIMIT.as_secs()
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.worker = None;
                    return Err(gone());
                }
            }
        }
    }

    /// Takes the answers that came back, without waiting: reads and edits update the view, live
    /// link answers are kept for [`Self::take_link_answers`].
    pub fn poll(&mut self) {
        let mut finished = std::mem::take(&mut self.stash);
        if let Some(worker) = self.worker.as_ref() {
            finished.extend(worker.drain());
        }
        for done in finished {
            self.absorb(done);
        }
    }

    fn absorb(&mut self, done: Finished) {
        match done.kind {
            JobKind::Refresh => {
                if self.refreshing == Some(done.ticket) {
                    self.refreshing = None;
                }
                if let Ok(read) = done.out.downcast::<RefreshOut>() {
                    // A read queued before an edit still in flight would show the old values.
                    let stale = self.edits.iter().any(|edit| *edit > done.ticket);
                    self.take_read(*read, stale);
                }
            }
            JobKind::Edit => {
                self.edits.remove(&done.ticket);
                if let Ok(edit) = done.out.downcast::<EditOut>() {
                    let EditOut { result, read } = *edit;
                    self.take_read(read, false);
                    if let Err(err) = result {
                        self.error = Some(err.to_string());
                    }
                }
            }
            JobKind::Link => self.link_answers.push(done.out),
            JobKind::Call => {}
        }
    }

    /// Which connection thread is current: it changes whenever a new one starts.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Waits (up to [`WAIT_LIMIT`]) for the next answer of the connection's thread and takes it.
    /// Answers `false` when nothing came (no thread, or the wait ran out).
    pub fn wait_any(&mut self) -> bool {
        if !self.stash.is_empty() {
            self.poll();
            return true;
        }
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        match worker.recv(WAIT_LIMIT) {
            Ok(done) => {
                self.absorb(done);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => {
                self.worker = None;
                false
            }
        }
    }

    /// The live link answers that came back since the last call.
    pub fn take_link_answers(&mut self) -> Vec<Answer> {
        self.poll();
        std::mem::take(&mut self.link_answers)
    }

    /// Whether a job of the selected source's connection is still out (queued or running).
    #[must_use]
    pub fn busy(&self) -> bool {
        self.refreshing.is_some() || !self.edits.is_empty()
    }

    /// Shows a read: the rows, and (unless `stale`) the selected entities' components.
    fn take_read(&mut self, read: RefreshOut, stale: bool) {
        let Some(rows) = read.rows else {
            self.error = read.error;
            return;
        };
        self.rows = rows;
        self.error = read.error;
        if stale {
            return;
        }
        let kept: BTreeSet<u64> = read.inspected.iter().map(|(e, _)| *e).collect();
        let asked: BTreeSet<u64> = read.asked.iter().copied().collect();
        // An entity selected after the read was queued is kept (the next read has it).
        self.selection.retain(|e| !asked.contains(e) || kept.contains(e));
        if self.selection.iter().any(|e| !asked.contains(e)) {
            self.last_refresh = None;
        }
        self.types.extend(read.types);
        self.inspected = read
            .inspected
            .into_iter()
            .filter(|(e, _)| self.selection.contains(e))
            .collect();
        self.preview_pending();
    }

    /// Shows the pending edit's value over the last read (the field keeps it while the gesture
    /// lasts).
    fn preview_pending(&mut self) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let steps = path_steps(&pending.path);
        for (_, components) in &mut self.inspected {
            if let Some(slot) = components
                .get_mut(&pending.component)
                .and_then(|value| field_mut(value, &steps))
            {
                *slot = pending.value.clone();
            }
        }
    }

    /// Calls `method` on the selected runtime at agent directory `dir` (connecting first when
    /// needed) on the connection's thread, and waits for the answer.
    ///
    /// # Errors
    ///
    /// The transport's [`RemoteError`].
    pub fn call_runtime(&mut self, dir: &Path, method: &str, params: Value) -> Result<Value, RemoteError> {
        let method = method.to_owned();
        let job: Job = Box::new(move |transport: &mut dyn RemoteTransport| -> Answer {
            Box::new(transport.call(&method, params))
        });
        let out = self.queue_and_wait(dir, JobKind::Call, job)?;
        match out.downcast::<Result<Value, RemoteError>>() {
            Ok(result) => *result,
            Err(_) => Err(RemoteError::Connection("a call answered the wrong type".into())),
        }
    }

    /// Uses `transport` for source `source` instead of dialling its agent directory (tests and
    /// hosts with their own transport). It runs on its own thread like a dialled connection.
    pub fn set_transport(&mut self, source: &str, transport: Box<dyn RemoteTransport>) {
        self.generation += 1;
        self.worker = Some(Worker::start(source, None, Some(transport)));
        self.stash.clear();
        self.link_answers.clear();
        self.refreshing = None;
        self.edits.clear();
    }

    /// The edits that set field `path` of `component` to `value` on each inspected entity that has
    /// the component, with the component's name for `scene.apply_edits` (its type path when known).
    fn edits_for(&self, component: &str, path: &str, value: &Value) -> Result<Vec<Value>, RpcError> {
        let steps = path_steps(path);
        let name = self.types.get(component).map_or(component, String::as_str);
        let mut edits = Vec::new();
        for (entity, components) in &self.inspected {
            let Some(current) = components.get(component) else {
                continue;
            };
            let mut whole = current.clone();
            let Some(slot) = field_mut(&mut whole, &steps) else {
                return Err(error(
                    codes::BAD_PATH,
                    format!("`{path}` is not a field of {component}"),
                ));
            };
            *slot = value.clone();
            edits.push(json!({
                "op": "set_property", "entity": entity, "component": name, "value": whole,
            }));
        }
        if edits.is_empty() {
            return Err(error(
                codes::NO_SELECTION,
                format!("no selected remote entity has {component}"),
            ));
        }
        Ok(edits)
    }

    /// The job that applies `edits` in one transaction and reads the view back.
    fn edit_job(&self, edits: Vec<Value>) -> Job {
        let limit = self.pages.clamp(1, MAX_PAGES) * PAGE;
        let selection: Vec<u64> = self.selection.iter().copied().take(MAX_SELECTED).collect();
        Box::new(move |transport: &mut dyn RemoteTransport| -> Answer {
            let result = transport.call("scene.apply_edits", json!({ "edits": edits }));
            let read = read_runtime(transport, limit, selection);
            Box::new(EditOut { result, read })
        })
    }

    /// The job that reads the view.
    fn read_job(&self) -> Job {
        let limit = self.pages.clamp(1, MAX_PAGES) * PAGE;
        let selection: Vec<u64> = self.selection.iter().copied().take(MAX_SELECTED).collect();
        Box::new(move |transport: &mut dyn RemoteTransport| -> Answer {
            Box::new(read_runtime(transport, limit, selection))
        })
    }

    /// Sends the pending edit as one transaction, unless the queue is full (it waits for the next
    /// frame then).
    fn send_pending(&mut self, dir: &Path) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let edits = match self.edits_for(&pending.component, &pending.path, &pending.value) {
            Ok(edits) => edits,
            Err(err) => {
                self.pending = None;
                self.error = Some(err.message);
                return;
            }
        };
        let job = self.edit_job(edits);
        match self.try_queue(dir, JobKind::Edit, job) {
            Ok(ticket) => {
                self.edits.insert(ticket);
                self.pending = None;
            }
            Err(QueueError::Full) => {}
            Err(QueueError::Closed(err)) => {
                self.pending = None;
                self.error = Some(err.to_string());
            }
        }
    }
}

/// Attaches the runtime advertised in agent directory `dir` as a source labelled `label`, and
/// answers its id (`attached-<n>`). Attaching the same directory again answers the existing id.
pub fn attach(world: &mut World, label: &str, dir: PathBuf) -> String {
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    if let Some(index) = remote.attached.iter().position(|(_, d)| *d == dir) {
        return format!("attached-{index}");
    }
    remote.attached.push((label.to_owned(), dir));
    format!("attached-{}", remote.attached.len() - 1)
}

/// Every source the switch lists now: the edit world, the play world while play runs, the
/// running launch, the separate-process PIE children and the attached runtimes.
#[must_use]
pub fn sources(world: &World) -> Vec<RemoteSource> {
    let mut out = vec![RemoteSource {
        id: EDIT.into(),
        label: "Edit".into(),
        kind: "edit",
        dir: None,
    }];
    if playing(world) {
        out.push(RemoteSource {
            id: PLAY.into(),
            label: "Play".into(),
            kind: "play",
            dir: None,
        });
    }
    if let Some((id, dir)) = crate::launch::launch_agent_dir(world) {
        out.push(RemoteSource {
            id: "launch".into(),
            label: format!("Launched game {id}"),
            kind: "launch",
            dir: Some(dir),
        });
    }
    if let Some(processes) = world.get_resource::<crate::pie_process::PieProcesses>() {
        use crate::pie_process::ProcessRole;
        let mut roles = vec![ProcessRole::Server];
        let clients = processes
            .processes()
            .iter()
            .filter(|p| matches!(p.role, ProcessRole::Client(_)))
            .count();
        roles.extend((0..clients).map(|k| ProcessRole::Client(k as u32)));
        for role in roles {
            if let Some(dir) = processes.agent_dir(role) {
                let id = match role {
                    ProcessRole::Server => "pie-server".to_owned(),
                    ProcessRole::Client(k) => format!("pie-client-{k}"),
                };
                out.push(RemoteSource {
                    id,
                    label: role.title(),
                    kind: "pie_process",
                    dir: Some(dir),
                });
            }
        }
    }
    // ADR-8472: the game an Android launch runs on a device, through its forwarded agent port.
    if let Some((id, serial, dir)) = crate::launch::device_agent_dir(world) {
        out.push(RemoteSource {
            id: DEVICE.into(),
            label: format!("Device game {id} ({serial})"),
            kind: "device",
            dir: Some(dir),
        });
    }
    if let Some(remote) = world.get_resource::<RemoteWorld>() {
        for (index, (label, dir)) in remote.attached.iter().enumerate() {
            out.push(RemoteSource {
                id: format!("attached-{index}"),
                label: label.clone(),
                kind: "attached",
                dir: Some(dir.clone()),
            });
        }
    }
    out
}

fn playing(world: &World) -> bool {
    world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some())
}

/// The source the panels show now: the selected one when it is still listed, else the play world
/// while play runs, else the edit world.
#[must_use]
pub fn current(world: &World) -> RemoteSource {
    let list = sources(world);
    let selected = world
        .get_resource::<RemoteWorld>()
        .map(|r| r.selected.clone())
        .unwrap_or_default();
    if let Some(found) = list.iter().find(|s| s.id == selected) {
        return found.clone();
    }
    let fallback = if playing(world) { PLAY } else { EDIT };
    list.into_iter()
        .find(|s| s.id == fallback)
        .unwrap_or(RemoteSource {
            id: EDIT.into(),
            label: "Edit".into(),
            kind: "edit",
            dir: None,
        })
}

/// Selects source `id` for both panels; a remote source is read at once.
///
/// # Errors
///
/// [`codes::NO_SOURCE`] when `id` is not listed.
pub fn select_source(world: &mut World, id: &str) -> Result<(), RpcError> {
    if !sources(world).iter().any(|s| s.id == id) {
        return Err(error(
            codes::NO_SOURCE,
            format!("no source `{id}` (see editor.remote_sources)"),
        ));
    }
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    if remote.selected != id {
        remote.selected = id.to_owned();
        remote.worker = None;
        remote.stash.clear();
        remote.link_answers.clear();
        remote.refreshing = None;
        remote.edits.clear();
        remote.pending = None;
        remote.rows.clear();
        remote.selection.clear();
        remote.inspected.clear();
        remote.error = None;
        remote.pages = 1;
        remote.last_refresh = None;
    }
    Ok(())
}

/// Reads the selected runtime's tree and the selected entities' components, and waits for the
/// answer (the agent methods' path; the panels queue reads instead, see [`refresh_async`]). Does
/// nothing for the edit or play source. With `force` false it reads at most every
/// [`REFRESH_INTERVAL`].
pub fn refresh(world: &mut World, force: bool) {
    let source = current(world);
    let Some(dir) = source.dir else {
        return;
    };
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    if !force
        && remote
            .last_refresh
            .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL)
    {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "remote_world_refresh").entered();
    remote.last_refresh = Some(Instant::now());
    let job = remote.read_job();
    match remote.queue_and_wait(&dir, JobKind::Refresh, job) {
        Ok(out) => {
            // Answers queued before this read (an edit) are shown first.
            remote.poll();
            if let Ok(read) = out.downcast::<RefreshOut>() {
                remote.take_read(*read, false);
            }
        }
        Err(err) => remote.error = Some(err.to_string()),
    }
}

/// The panels' read: takes the answers that came back and, at most every [`REFRESH_INTERVAL`] and
/// with no read in flight, queues the next read. Never waits for the runtime.
pub fn refresh_async(world: &mut World) {
    let source = current(world);
    let Some(dir) = source.dir else {
        return;
    };
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    remote.poll();
    let due = remote
        .last_refresh
        .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL);
    if !due || remote.refreshing.is_some() {
        return;
    }
    let job = remote.read_job();
    match remote.try_queue(&dir, JobKind::Refresh, job) {
        Ok(ticket) => {
            remote.refreshing = Some(ticket);
            remote.last_refresh = Some(Instant::now());
        }
        // The queue is full: the next frame asks again.
        Err(QueueError::Full) => {}
        Err(QueueError::Closed(err)) => remote.error = Some(err.to_string()),
    }
}

fn rows_of(answer: &Value) -> Vec<RemoteRow> {
    answer
        .get("entities")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|row| {
                    Some(RemoteRow {
                        entity: row.get("entity")?.as_u64()?,
                        name: row.get("name").and_then(Value::as_str).map(str::to_owned),
                        kind: row.get("kind").and_then(Value::as_str).map(str::to_owned),
                        parent: row.get("parent").and_then(Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Sets the remote selection (`add`: toggles `entity` in it instead) and reads the inspected
/// components again, waiting for the answer.
pub fn select_entities(world: &mut World, entities: &[u64], add: bool) {
    pick_entities(world, entities, add);
    refresh(world, true);
}

/// Sets the remote selection (`add`: toggles) and asks for a read on the next frame (the panel's
/// path: it does not wait).
fn pick_entities(world: &mut World, entities: &[u64], add: bool) {
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    remote.last_refresh = None;
    if add {
        for entity in entities {
            if let Some(at) = remote.selection.iter().position(|e| e == entity) {
                remote.selection.remove(at);
            } else {
                remote.selection.push(*entity);
            }
        }
    } else {
        remote.selection = entities.iter().copied().take(MAX_SELECTED).collect();
    }
}

/// The rows of `rows` in tree order: `(index into rows, depth, has children)`, children right under
/// their parent in listing order. A row whose parent is not listed is a root. With `collapsed`, the
/// children of a collapsed row are left out.
#[must_use]
pub fn tree_order(rows: &[RemoteRow], collapsed: Option<&BTreeSet<u64>>) -> Vec<(usize, usize, bool)> {
    let index: HashMap<u64, usize> = rows.iter().enumerate().map(|(i, r)| (r.entity, i)).collect();
    let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut roots = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match row.parent.and_then(|p| index.get(&p)).filter(|p| **p != i) {
            Some(parent) => children.entry(*parent).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut seen = vec![false; rows.len()];
    // Depth first without recursion (a deep hierarchy cannot overflow the stack); the whole tree is
    // walked, the children of a collapsed row are walked hidden.
    let mut stack: Vec<(usize, usize, bool)> = roots.iter().rev().map(|i| (*i, 0, false)).collect();
    while let Some((i, depth, hidden)) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        let kids = children.get(&i).map_or(&[][..], Vec::as_slice);
        if !hidden {
            out.push((i, depth, !kids.is_empty()));
        }
        let hide = hidden || collapsed.is_some_and(|c| c.contains(&rows[i].entity));
        stack.extend(kids.iter().rev().map(|k| (*k, depth + 1, hide)));
    }
    // A parent cycle (a listing taken mid-change) leaves rows unreached: show them at the root.
    for (i, done) in seen.iter().enumerate() {
        if !done {
            out.push((i, 0, children.contains_key(&i)));
        }
    }
    out
}

/// Splits a field path (`translation.x`, `color[2]`, `/translation/0`) into JSON keys and indices.
fn path_steps(path: &str) -> Vec<String> {
    path.split(['.', '/', '[', ']'])
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The value at `steps` inside `value`, mutable.
fn field_mut<'v>(value: &'v mut Value, steps: &[String]) -> Option<&'v mut Value> {
    let mut cursor = value;
    for step in steps {
        cursor = match cursor {
            Value::Object(map) => map.get_mut(step)?,
            Value::Array(items) => items.get_mut(step.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// Sets field `path` (empty: the whole component) of component `component` to `value` on every
/// selected remote entity that has the component, in one `scene.apply_edits` transaction, then
/// reads the entities back. Answers `{ applied, entities, values }`.
///
/// # Errors
///
/// [`codes::NO_SELECTION`] without a remote source or selection, [`codes::BAD_PATH`] when the path
/// names no field, and the transport's codes.
pub fn set_field(world: &mut World, component: &str, path: &str, value: Value) -> Result<Value, RpcError> {
    let source = current(world);
    let Some(dir) = source.dir else {
        return Err(error(
            codes::NO_SELECTION,
            "select a launched game or another runtime first (editor.remote_select)",
        ));
    };
    let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
    if remote.selection.is_empty() {
        return Err(error(codes::NO_SELECTION, "no remote entity is selected"));
    }
    let edits = remote.edits_for(component, path, &value)?;
    let entities: Vec<Value> = edits.iter().map(|e| e["entity"].clone()).collect();
    let count = edits.len();
    let job = remote.edit_job(edits);
    let out = remote
        .queue_and_wait(&dir, JobKind::Edit, job)
        .map_err(|e| e.rpc())?;
    remote.poll();
    let Ok(edit) = out.downcast::<EditOut>() else {
        return Err(error(codes::CALL_FAILED, "the edit answered the wrong type"));
    };
    let EditOut { result, read } = *edit;
    remote.last_refresh = Some(Instant::now());
    remote.take_read(read, false);
    result.map_err(|e| e.rpc())?;
    let values: Vec<Value> = remote
        .inspected
        .iter()
        .map(|(entity, components)| json!({ "entity": entity, "value": components.get(component) }))
        .collect();
    Ok(json!({ "applied": count, "entities": entities, "values": values }))
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

/// Which panel draws the source switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The Outliner.
    Outliner,
    /// The Inspector.
    Inspector,
}

/// What the panel should show under the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// The edit world.
    Edit,
    /// The in-process play world.
    Play,
    /// A runtime; the switch already drew the panel's remote view.
    Remote,
}

/// Draws the source switch and the Live link toggle above the Outliner or the Inspector. With a
/// runtime selected it also draws the panel's remote view and answers [`Shown::Remote`]; otherwise
/// the caller draws its panel on the world [`Shown`] names.
pub fn source_switch(ui: &mut egui::Ui, world: &mut World, pane: Pane) -> Shown {
    let list = sources(world);
    let shown = current(world);
    if list.len() > 1
        || world
            .get_resource::<crate::live_link::LiveLink>()
            .is_some_and(|l| l.on)
    {
        ui.horizontal(|ui| {
            let mut chosen = shown.id.clone();
            egui::ComboBox::from_label("Source")
                .selected_text(shown.label.clone())
                .show_ui(ui, |ui| {
                    for source in &list {
                        ui.selectable_value(&mut chosen, source.id.clone(), source.label.clone());
                    }
                });
            if chosen != shown.id {
                let _ = select_source(world, &chosen);
            }
            crate::live_link::toggle_ui(ui, world);
        });
    }
    if !shown.is_remote() {
        return if shown.id == PLAY {
            Shown::Play
        } else {
            Shown::Edit
        };
    }
    let warn = crate::theme::tokens(world).warn;
    ui.colored_label(
        warn,
        format!(
            "{}: a running game. Changes are not saved to the level.",
            shown.label
        ),
    );
    // One read per egui frame, however many passes and panels draw it.
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = world
        .get_resource::<RemoteWorld>()
        .is_some_and(|r| r.refreshed_frame == Some(frame));
    if !fresh {
        refresh_async(world);
        let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
        remote.refreshed_frame = Some(frame);
        // Keep the remote view live while it is shown; answers in flight come back sooner.
        let soon = if remote.busy() {
            Duration::from_millis(50)
        } else {
            REFRESH_INTERVAL
        };
        ui.ctx().request_repaint_after(soon);
    }
    if let Some(err) = world.get_resource::<RemoteWorld>().and_then(|r| r.error.clone()) {
        let color = crate::theme::tokens(world).error;
        ui.colored_label(color, err);
    }
    match pane {
        Pane::Outliner => remote_outliner(ui, world),
        Pane::Inspector => remote_inspector(ui, world),
    }
    Shown::Remote
}

impl RemoteWorld {
    /// Whether an Outliner action (`acting`) in egui frame `frame` is the frame's first; a repeated
    /// pass of the frame is ignored.
    fn first_outliner_action(&mut self, frame: u64, acting: bool) -> bool {
        if !acting {
            return true;
        }
        if self.outliner_frame == Some(frame) {
            return false;
        }
        self.outliner_frame = Some(frame);
        true
    }
}

/// The text of a tree row: indented by `depth`, a marker on a row with children (open or closed).
fn tree_label(row: &RemoteRow, depth: usize, has_children: bool, open: bool) -> String {
    let indent = "    ".repeat(depth);
    let marker = match (has_children, open) {
        (false, _) => "",
        (true, true) => "\u{25be} ",
        (true, false) => "\u{25b8} ",
    };
    format!("{indent}{marker}{}", row.label())
}

/// The remote Outliner: the runtime's entities as a tree of keyboard rows, a page at a time.
fn remote_outliner(ui: &mut egui::Ui, world: &mut World) {
    let mut remote = world.remove_resource::<RemoteWorld>().unwrap_or_default();
    let collapsed = remote.collapsed();
    let visible = tree_order(&remote.rows, Some(&collapsed));
    let labels: Vec<String> = visible
        .iter()
        .map(|(i, depth, kids)| {
            let row = &remote.rows[*i];
            tree_label(row, *depth, *kids, !collapsed.contains(&row.entity))
        })
        .collect();
    ui.label(format!("{} entities", remote.rows.len()));
    let selected = remote.selection.first().and_then(|first| {
        visible
            .iter()
            .position(|(i, _, _)| remote.rows[*i].entity == *first)
    });
    // Show or hide the selected row's children (the mouse path; Left / Right on a row from the
    // keyboard).
    let toggle_target = selected
        .filter(|at| visible[*at].2)
        .map(|at| remote.rows[visible[at].0].entity);
    let toggle = ui
        .add_enabled_ui(toggle_target.is_some(), |ui| {
            crate::agent_parity::action_button(ui, &crate::agent_parity::REMOTE_TOGGLE_CHILDREN)
        })
        .inner
        .clicked();
    let keys = crate::canvas_keys::list_rows(
        ui,
        "remote-outliner",
        &mut remote.keys,
        labels.len(),
        crate::canvas_keys::LIST_HEIGHT,
        selected,
        &|i| labels.get(i).cloned().unwrap_or_default(),
    );
    let add = ui.input(|i| i.modifiers.command);
    let mut pick = None;
    let mut fold: Option<(u64, bool)> = toggle_target
        .filter(|_| toggle)
        .map(|entity| (entity, !collapsed.contains(&entity)));
    let mut focus_parent = None;
    for key in keys {
        match key {
            crate::canvas_keys::RowKey::Activate(i) => {
                if let Some((row, _, _)) = visible.get(i) {
                    pick = Some(remote.rows[*row].entity);
                }
            }
            crate::canvas_keys::RowKey::Arrow {
                row: i,
                dx,
                alt: false,
                ..
            } if dx != 0 => {
                let Some((row, _, kids)) = visible.get(i).copied() else {
                    continue;
                };
                let entity = remote.rows[row].entity;
                let open = !collapsed.contains(&entity);
                match (dx, kids, open) {
                    (-1, true, true) => fold = Some((entity, true)),
                    (-1, _, _) => {
                        focus_parent = remote.rows[row]
                            .parent
                            .and_then(|p| visible.iter().position(|(r, _, _)| remote.rows[*r].entity == p));
                    }
                    (1, true, false) => fold = Some((entity, false)),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let full = remote.rows.len() >= remote.pages.max(1) * PAGE && remote.pages < MAX_PAGES;
    let more = full && crate::agent_parity::action_button(ui, &crate::agent_parity::REMOTE_MORE).clicked();
    let acting = more || pick.is_some() || fold.is_some();
    if !remote.first_outliner_action(ui.ctx().cumulative_frame_nr(), acting) {
        pick = None;
        fold = None;
    } else if more {
        remote.pages = remote.pages.max(1) + 1;
        remote.last_refresh = None;
    }
    if let Some((entity, collapse)) = fold {
        remote.set_collapsed(entity, collapse);
    }
    if let Some(at) = focus_parent {
        remote.keys.focus(at);
    }
    world.insert_resource(remote);
    if let Some(entity) = pick {
        pick_entities(world, &[entity], add);
    }
}

/// What the Inspector's fields did this pass.
#[derive(Default)]
struct FieldPass {
    /// The field changed: `(path, value)`.
    changed: Option<(String, Value)>,
    /// A field is mid-gesture: a number being dragged or a text field being typed in.
    active: bool,
}

/// The remote Inspector: the selected entities' components, field by field. An edit is shown at
/// once and sent as one transaction when its gesture ends.
fn remote_inspector(ui: &mut egui::Ui, world: &mut World) {
    let dir = current(world).dir;
    let Some(remote) = world.get_resource::<RemoteWorld>() else {
        return;
    };
    let Some((entity, components)) = remote.inspected.first().cloned() else {
        ui.label("Select an entity in the Outliner.");
        return;
    };
    let count = remote.inspected.len();
    let types = remote.types.clone();
    let name = remote
        .rows
        .iter()
        .find(|r| r.entity == entity)
        .map_or_else(|| format!("Entity {entity}"), RemoteRow::label);
    ui.strong(name);
    if count > 1 {
        ui.label(format!(
            "Edits apply to the {count} selected entities that have the component."
        ));
    }
    let registry = world.get_resource::<AppTypeRegistry>().cloned();
    let registry = registry.as_ref().map(|r| r.read());
    let mut edit: Option<(String, String, Value)> = None;
    let mut active = false;
    egui::ScrollArea::vertical()
        .id_salt("remote-inspector")
        .show(ui, |ui| {
            for (component, value) in &components {
                egui::CollapsingHeader::new(component.as_str())
                    .id_salt(("remote-component", component))
                    .default_open(true)
                    .show(ui, |ui| {
                        let info = registry.as_deref().and_then(|registry| {
                            let path = types.get(component)?;
                            Some(registry.get_with_type_path(path)?.type_info())
                        });
                        let mut value = value.clone();
                        let mut pass = FieldPass::default();
                        field_ui(ui, component, "", &mut value, info, &mut pass);
                        active |= pass.active;
                        if let Some((path, value)) = pass.changed {
                            edit = Some((component.clone(), path, value));
                        }
                    });
            }
        });
    drop(registry);
    let dragging = ui.ctx().dragged_id().is_some() || ui.input(|i| i.pointer.any_down());
    let mut remote = world.resource_mut::<RemoteWorld>();
    if let Some((component, path, value)) = edit {
        // A different field's edit ends the one before it (sent first, in order).
        let other = remote
            .pending
            .as_ref()
            .is_some_and(|p| p.component != component || p.path != path);
        if other && let Some(dir) = dir.as_deref() {
            remote.send_pending(dir);
        }
        remote.pending = Some(PendingEdit {
            component,
            path,
            value,
        });
        remote.preview_pending();
    }
    // The gesture is over (the drag ended, the text field was left): one transaction.
    if !active
        && !dragging
        && remote.pending.is_some()
        && let Some(dir) = dir.as_deref()
    {
        remote.send_pending(dir);
    }
}

/// The type of field `step` inside a value of type `info` (a struct field by name or index, a
/// tuple field, a list or array item, a map value, an option's inner type).
fn field_info(info: &'static TypeInfo, step: &str) -> Option<&'static TypeInfo> {
    let index = step.parse::<usize>().ok();
    match info {
        TypeInfo::Struct(s) => match index {
            Some(i) => s.field_at(i),
            None => s.field(step),
        }?
        .type_info(),
        TypeInfo::TupleStruct(s) => s.field_at(index?)?.type_info(),
        TypeInfo::Tuple(t) => t.field_at(index?)?.type_info(),
        TypeInfo::List(l) => l.item_info(),
        TypeInfo::Array(a) => a.item_info(),
        TypeInfo::Map(m) => m.value_info(),
        _ => None,
    }
}

/// The unit variants of `info` when it is an enum (`None` otherwise).
fn unit_variants(info: Option<&'static TypeInfo>) -> Option<Vec<&'static str>> {
    let TypeInfo::Enum(e) = info? else {
        return None;
    };
    let names: Vec<&'static str> = e
        .iter()
        .filter(|v| matches!(v, VariantInfo::Unit(_)))
        .map(VariantInfo::name)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// Draws `value`'s leaves as editable fields (`info`: its type, when the editor knows it) and
/// records in `pass` the field the user changed and whether a gesture is under way.
fn field_ui(
    ui: &mut egui::Ui,
    component: &str,
    path: &str,
    value: &mut Value,
    info: Option<&'static TypeInfo>,
    pass: &mut FieldPass,
) {
    let label = path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(component)
        .to_owned();
    let id = ui.id().with(("remote-field", component, path));
    match value {
        Value::Number(number) => {
            let mut v = number.as_f64().unwrap_or(0.0);
            let integer = number.is_i64() || number.is_u64();
            ui.horizontal(|ui| {
                ui.label(&label);
                // Typed text is taken on Enter or when the field is left, not per keystroke.
                let mut drag = egui::DragValue::new(&mut v)
                    .speed(if integer { 1.0 } else { 0.01 })
                    .update_while_editing(false);
                if integer {
                    drag = drag.max_decimals(0);
                }
                let response = ui.add(drag).on_hover_text(format!("{component}{path}"));
                let response = crate::a11y_audit::named_drag_value(response, &label, v);
                if response.dragged() {
                    pass.active = true;
                }
                if response.changed() {
                    let next = if integer {
                        json!(v.round() as i64)
                    } else {
                        serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
                    };
                    if !next.is_null() {
                        *value = next.clone();
                        pass.changed = Some((path.to_owned(), next));
                    }
                }
            });
        }
        Value::Bool(flag) => {
            if ui.checkbox(flag, &label).changed() {
                pass.changed = Some((path.to_owned(), Value::Bool(*flag)));
            }
        }
        Value::String(text) => match unit_variants(info) {
            // An enum the editor knows: a choice of its unit variants.
            Some(variants) => {
                let mut chosen = text.clone();
                egui::ComboBox::new(id, &label)
                    .selected_text(chosen.clone())
                    .show_ui(ui, |ui| {
                        for variant in &variants {
                            ui.selectable_value(&mut chosen, (*variant).to_owned(), *variant);
                        }
                    });
                if chosen != *text {
                    *text = chosen.clone();
                    pass.changed = Some((path.to_owned(), Value::String(chosen)));
                }
            }
            // Text (an asset path is a project-relative path): sent when the field is left.
            None => {
                ui.horizontal(|ui| {
                    ui.label(&label);
                    let response = ui.add(egui::TextEdit::singleline(text).id(id));
                    let response = crate::a11y_audit::name(response, egui::WidgetType::TextEdit, &label);
                    if response.has_focus() {
                        pass.active = true;
                    }
                    if response.changed() {
                        pass.changed = Some((path.to_owned(), Value::String(text.clone())));
                    }
                });
            }
        },
        Value::Array(items) => {
            ui.label(&label);
            ui.indent(("remote-array", component, path), |ui| {
                for (index, item) in items.iter_mut().enumerate() {
                    let item_info = info.and_then(|i| field_info(i, &index.to_string()));
                    field_ui(ui, component, &format!("{path}/{index}"), item, item_info, pass);
                }
            });
        }
        Value::Object(map) => {
            if !path.is_empty() {
                ui.label(&label);
            }
            // A data-carrying enum variant (`{ "Variant": ... }`) is drawn with its fields.
            let variant_of = |key: &str| match info {
                Some(TypeInfo::Enum(e)) => e.variant(key),
                _ => None,
            };
            ui.indent(("remote-object", component, path), |ui| {
                for (key, item) in map.iter_mut() {
                    let item_info = match variant_of(key) {
                        Some(VariantInfo::Tuple(t)) if t.field_len() == 1 => {
                            t.field_at(0).and_then(|f| f.type_info())
                        }
                        Some(_) => None,
                        None => info.and_then(|i| field_info(i, key)),
                    };
                    field_ui(ui, component, &format!("{path}/{key}"), item, item_info, pass);
                }
            });
        }
        Value::Null => {
            ui.label(format!("{label}: none"));
        }
    }
}

// ---------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------

fn status_json(world: &World) -> Value {
    let source = current(world);
    let remote = world.get_resource::<RemoteWorld>();
    let pages = remote.map_or(1, |r| r.pages.max(1));
    // Every listed row in tree order (collapsed rows' children included), with its depth.
    let entities: Vec<Value> = remote.map_or_else(Vec::new, |r| {
        tree_order(&r.rows, None)
            .into_iter()
            .map(|(i, depth, _)| {
                let row = &r.rows[i];
                json!({
                    "entity": row.entity, "name": row.name, "kind": row.kind,
                    "parent": row.parent, "depth": depth,
                })
            })
            .collect()
    });
    json!({
        "source": source.to_json(),
        "connected": remote.is_some_and(RemoteWorld::connected),
        "error": remote.and_then(|r| r.error.clone()),
        "entities": entities,
        "collapsed": remote.map_or_else(Vec::new, |r| r.collapsed().into_iter().collect()),
        "pages": pages,
        "selection": remote.map_or_else(Vec::new, |r| r.selection.clone()),
        "inspected": remote.map_or_else(Vec::new, |r| r.inspected.iter().map(|(entity, components)| json!({
            "entity": entity, "components": components,
        })).collect()),
    })
}

/// `editor.remote_sources`.
fn editor_remote_sources(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let list: Vec<Value> = sources(world).iter().map(RemoteSource::to_json).collect();
    Ok(json!({ "current": current(world).id, "sources": list }))
}

/// The entity ids of array param `name` (absent or null: none).
fn id_list(params: &Value, name: &str) -> Result<Vec<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_u64()
                    .ok_or_else(|| RpcError::wrong_type(name, "an array of entity ids"))
            })
            .collect(),
        Some(_) => Err(RpcError::wrong_type(name, "an array of entity ids")),
    }
}

/// `editor.remote_select { source?, entities?, add?, pages?, collapse?, expand? }`.
fn editor_remote_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match params.get("source") {
        None | Some(Value::Null) => {}
        Some(Value::String(id)) => select_source(world, id)?,
        Some(_) => return Err(RpcError::wrong_type("source", "a source id string")),
    }
    let (collapse, expand) = (id_list(params, "collapse")?, id_list(params, "expand")?);
    if !collapse.is_empty() || !expand.is_empty() {
        let mut remote = world.get_resource_or_insert_with(RemoteWorld::default);
        for entity in collapse {
            remote.set_collapsed(entity, true);
        }
        for entity in expand {
            remote.set_collapsed(entity, false);
        }
    }
    match params.get("pages") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let pages = v
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or_else(|| RpcError::wrong_type("pages", "a whole number from 1"))?;
            world.get_resource_or_insert_with(RemoteWorld::default).pages =
                usize::try_from(pages).unwrap_or(MAX_PAGES).min(MAX_PAGES);
        }
    }
    let add = params.get("add").and_then(Value::as_bool).unwrap_or(false);
    match params.get("entities") {
        None | Some(Value::Null) => refresh(world, true),
        Some(Value::Array(items)) => {
            let entities: Vec<u64> = items
                .iter()
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| RpcError::wrong_type("entities", "an array of entity ids"))
                })
                .collect::<Result<_, _>>()?;
            if !current(world).is_remote() {
                return Err(error(
                    codes::NO_SELECTION,
                    "`entities` selects in a runtime: select a launched game or another runtime first",
                ));
            }
            select_entities(world, &entities, add);
        }
        Some(_) => return Err(RpcError::wrong_type("entities", "an array of entity ids")),
    }
    let status = status_json(world);
    if current(world).is_remote()
        && let Some(err) = world.get_resource::<RemoteWorld>().and_then(|r| r.error.clone())
        && !world
            .get_resource::<RemoteWorld>()
            .is_some_and(RemoteWorld::connected)
    {
        return Err(error(codes::NOT_CONNECTED, err));
    }
    Ok(status)
}

/// `editor.remote_set { component, path?, value }`.
fn editor_remote_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let component = params
        .get("component")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RpcError::invalid_params("`component` (the short name scene.get_entity shows) is required")
        })?
        .to_owned();
    let path = match params.get("path") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(RpcError::wrong_type("path", "a field path string")),
    };
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    set_field(world, &component, &path, value)
}

const STATUS_RESULT: &str = "{ source: { id, label, kind, remote, agent_dir }, connected, error, \
    entities: [{ entity, name, kind, parent, depth }], collapsed, pages, selection, \
    inspected: [{ entity, components }] }";

/// The remote view's methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.remote_sources",
                tool: "remote_sources",
                description: "The sources the Outliner and Inspector can show: `edit`, `play` while play runs, \
                    `launch` for a running editor.launch, `device` for a running Android launch's game (its dev transport, \
                    ADR-8472), `pie-server` / `pie-client-<k>` for separate-process PIE \
                    children and `attached-<n>` runtimes. `current` is the one shown now.",
                params: &[],
                result: "{ current, sources: [{ id, label, kind, remote, agent_dir }] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_remote_sources as Handler,
        ),
        (
            MethodSchema {
                name: "editor.remote_select",
                tool: "remote_select",
                description: "Pick the Outliner and Inspector source (a runtime is read through its own agent \
                    server: scene.query_entities, scene.get_entity) and, for a runtime, select its entities. \
                    Answers the remote tree (every row in tree order with its parent and depth; `collapsed` \
                    rows hide their children in the panel, kept per source), the selection and the selected \
                    entities' components. remote_no_source, remote_not_connected, remote_no_selection.",
                params: &[
                    ParamSchema {
                        name: "source",
                        kind: "string",
                        required: false,
                        description: "A source id from editor.remote_sources (default: keep the current one).",
                    },
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Runtime entity ids to select (a runtime source only).",
                    },
                    ParamSchema {
                        name: "add",
                        kind: "boolean",
                        required: false,
                        description: "With `entities`: toggle them in the selection instead of replacing it.",
                    },
                    ParamSchema {
                        name: "pages",
                        kind: "integer",
                        required: false,
                        description: "Pages of 200 rows to read (default 1, at most 50).",
                    },
                    ParamSchema {
                        name: "collapse",
                        kind: "array",
                        required: false,
                        description: "Entity ids whose children the remote Outliner hides (Show or hide children).",
                    },
                    ParamSchema {
                        name: "expand",
                        kind: "array",
                        required: false,
                        description: "Entity ids whose children the remote Outliner shows again.",
                    },
                ],
                result: STATUS_RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            editor_remote_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.remote_set",
                tool: "remote_set",
                description: "The remote Inspector's edit: set a field (or the whole component) on every selected \
                    runtime entity that has the component, in one scene.apply_edits transaction, then read them \
                    back. Changes are not saved to the level. remote_no_selection, remote_bad_path, \
                    remote_not_connected, remote_call_failed.",
                params: &[
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "The component's short name, as editor.remote_select shows it (\"Transform\").",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The field inside the component's value (`translation/0`, `color.r`); default \
                            the whole value.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: true,
                        description: "The new value, in the form editor.remote_select shows.",
                    },
                ],
                result: "{ applied, entities, values: [{ entity, value }] }",
                mutating: true,
                permission: Permission::Play,
            },
            editor_remote_set as Handler,
        ),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Review sprint-w12-rev-g10 rows 7-8: More and a row pick act once per egui frame.
    #[test]
    fn an_outliner_click_acts_once_per_frame() {
        let mut remote = RemoteWorld::default();
        assert!(remote.first_outliner_action(5, true), "the frame's first click");
        assert!(
            !remote.first_outliner_action(5, true),
            "a repeated pass of frame 5"
        );
        assert!(
            remote.first_outliner_action(5, false),
            "no action: nothing to drop"
        );
        assert!(remote.first_outliner_action(6, true));
    }

    /// A transport over a fixed world: answers the three scene calls the remote view makes.
    pub(crate) struct FakeRuntime {
        pub(crate) entities: Vec<(u64, String, Value)>,
        pub(crate) calls: Vec<String>,
        /// `(child, parent)` pairs the listing reports.
        pub(crate) parents: Vec<(u64, u64)>,
    }

    impl RemoteTransport for std::sync::Arc<Mutex<FakeRuntime>> {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, RemoteError> {
            let mut rt = self
                .lock()
                .map_err(|_| RemoteError::Connection("poisoned".into()))?;
            rt.calls.push(method.to_owned());
            match method {
                "scene.query_entities" => Ok(json!({ "entities": rt.entities.iter().map(|(e, n, _)| json!({
                    "entity": e, "name": n, "kind": null,
                    "parent": rt.parents.iter().find(|(c, _)| c == e).map(|(_, p)| *p) })).collect::<Vec<_>>() })),
                "scene.get_entity" => {
                    let id = params["entity"].as_u64().unwrap_or(0);
                    let (_, _, t) = rt
                        .entities
                        .iter()
                        .find(|(e, _, _)| *e == id)
                        .ok_or_else(|| RemoteError::Refused("no such entity".into()))?;
                    Ok(json!({ "components": { "Transform": { "value": t } } }))
                }
                "scene.apply_edits" => {
                    for edit in params["edits"].as_array().cloned().unwrap_or_default() {
                        let id = edit["entity"].as_u64().unwrap_or(0);
                        if let Some(row) = rt.entities.iter_mut().find(|(e, _, _)| *e == id) {
                            row.2 = edit["value"].clone();
                        }
                    }
                    Ok(json!({ "applied": true }))
                }
                other => Err(RemoteError::Refused(format!("unknown {other}"))),
            }
        }
    }

    fn world_with_fake() -> (World, std::sync::Arc<Mutex<FakeRuntime>>, String) {
        let mut world = World::new();
        let dir = std::env::temp_dir().join("regia-remote-world-fake-never-read");
        let id = attach(&mut world, "Fake game", dir);
        select_source(&mut world, &id).expect("listed");
        let runtime = std::sync::Arc::new(Mutex::new(FakeRuntime {
            entities: vec![
                (5, "Crate".into(), json!({ "translation": [0.0, 1.0, 0.0] })),
                (9, "Lamp".into(), json!({ "translation": [2.0, 0.0, 0.0] })),
            ],
            calls: Vec::new(),
            parents: Vec::new(),
        }));
        world
            .resource_mut::<RemoteWorld>()
            .set_transport(&id, Box::new(runtime.clone()));
        (world, runtime, id)
    }

    #[test]
    fn path_steps_accept_dots_slashes_and_indices() {
        assert_eq!(path_steps("translation.x"), vec!["translation", "x"]);
        assert_eq!(path_steps("/translation/0"), vec!["translation", "0"]);
        assert_eq!(path_steps("color[2]"), vec!["color", "2"]);
        let mut v = json!({ "translation": [1.0, 2.0, 3.0] });
        *field_mut(&mut v, &path_steps("translation/1")).expect("field") = json!(9.0);
        assert_eq!(v, json!({ "translation": [1.0, 9.0, 3.0] }));
        assert!(field_mut(&mut v, &path_steps("rotation")).is_none());
    }

    #[test]
    fn sources_list_edit_and_attached_runtimes() {
        let (world, _rt, id) = world_with_fake();
        let list = sources(&world);
        assert_eq!(list[0].id, EDIT);
        assert!(list.iter().any(|s| s.id == id && s.is_remote()));
        assert_eq!(current(&world).id, id);
    }

    #[test]
    fn selecting_an_unknown_source_is_refused() {
        let mut world = World::new();
        let err = select_source(&mut world, "launch").expect_err("no launch running");
        assert!(err.message.contains("no source"), "{err:?}");
    }

    #[test]
    fn remote_tree_and_multi_select_edit_apply_to_all() {
        let (mut world, rt, _id) = world_with_fake();
        refresh(&mut world, true);
        let remote = world.resource::<RemoteWorld>();
        assert_eq!(remote.rows.len(), 2, "{:?}", remote.error);
        assert_eq!(remote.rows[0].label(), "Crate");
        select_entities(&mut world, &[5, 9], false);
        assert_eq!(world.resource::<RemoteWorld>().inspected.len(), 2);
        let answer = set_field(&mut world, "Transform", "translation/1", json!(4.0)).expect("set");
        assert_eq!(answer["applied"], json!(2));
        let rt = rt.lock().expect("lock");
        assert_eq!(rt.entities[0].2, json!({ "translation": [0.0, 4.0, 0.0] }));
        assert_eq!(rt.entities[1].2, json!({ "translation": [2.0, 4.0, 0.0] }));
        assert!(
            rt.calls.iter().filter(|c| *c == "scene.apply_edits").count() == 1,
            "one transaction"
        );
    }

    #[test]
    fn a_bad_path_is_refused_before_any_call() {
        let (mut world, rt, _id) = world_with_fake();
        select_entities(&mut world, &[5], false);
        let err = set_field(&mut world, "Transform", "rotation/9", json!(1.0)).expect_err("bad path");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some(codes::BAD_PATH)
        );
        assert!(
            !rt.lock()
                .expect("lock")
                .calls
                .iter()
                .any(|c| c == "scene.apply_edits")
        );
    }

    /// A headless runtime: a scene world served by a real loopback agent server on its own thread,
    /// advertised in a unique agent directory (what a launched game with the agent feature does).
    pub(crate) struct HeadlessRuntime {
        pub(crate) dir: PathBuf,
        project: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<RuntimeEnd>>,
    }

    /// What a stopped headless runtime reports: the Crate's translation and name.
    pub(crate) type RuntimeEnd = (Option<[f32; 3]>, Option<String>);

    /// The guid the runtime's "Crate" carries.
    pub(crate) const CRATE_GUID: u128 = 0x7833_0000_0000_4000_8000_0000_0000_0c0a;

    impl HeadlessRuntime {
        pub(crate) fn start(tag: &str) -> Self {
            Self::start_with(tag, None)
        }

        /// A runtime started by a device dev launch (ADR-8472): its agent server binds the launch's
        /// port and accepts the launch's token.
        pub(crate) fn start_with(tag: &str, dev: Option<regia_framework::dev_launch::DevLaunch>) -> Self {
            let project = regia_test_utils::unique_temp_dir(&format!("remote-world-{tag}"));
            std::fs::create_dir_all(&project).expect("project dir");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (thread_stop, thread_project) = (stop.clone(), project.clone());
            let thread = std::thread::spawn(move || {
                use regia_app::prelude::*;
                use regia_core::id::Guid;
                use regia_scene::prelude::{EntityGuid, Transform};
                // A game with the agent feature: its own server, advertised in its project.
                let mut app = App::new();
                app.add_plugins(MinimalPlugins);
                app.add_plugins(regia_scene::ScenePlugin);
                app.insert_resource(regia_agent::AgentSettings {
                    project_dir: Some(thread_project),
                });
                if let Some(dev) = dev {
                    app.insert_resource(dev);
                }
                app.add_plugins(regia_agent::AgentPlugin);
                app.finish_plugins();
                app.update();
                let mut permissions = regia_agent::AgentPermissions::default();
                permissions.config.rate_limit_per_second = 10_000;
                app.world_mut().insert_resource(permissions);
                let world = app.world_mut();
                let crate_entity = world
                    .spawn((
                        regia_scene::components::Name::new("Crate"),
                        Transform::from_translation(regia_core::math::Vec3::new(1.0, 0.0, 0.0)),
                        EntityGuid(Guid::from_u128(CRATE_GUID)),
                    ))
                    .id();
                world.spawn((
                    regia_scene::components::Name::new("Lamp"),
                    Transform::default(),
                    EntityGuid(Guid::new_random()),
                ));
                let dir = world
                    .get_resource::<regia_agent::AgentEndpoint>()
                    .map(|endpoint| endpoint.dir.clone());
                ready_tx.send(dir).expect("ready");
                // The runtime's frame loop: each update answers the queued requests.
                while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    app.update();
                    std::thread::yield_now();
                }
                let world = app.world();
                (
                    world
                        .get::<Transform>(crate_entity)
                        .map(|t| t.translation.to_array()),
                    world
                        .get::<regia_scene::components::Name>(crate_entity)
                        .map(|n| n.as_str().to_owned()),
                )
            });
            let dir = ready_rx
                .recv()
                .expect("the runtime starts")
                .expect("the runtime advertises its agent endpoint");
            Self {
                dir,
                project,
                stop,
                thread: Some(thread),
            }
        }

        /// Stops the runtime and answers the Crate's translation as its world has it.
        pub(crate) fn finish(self) -> Option<[f32; 3]> {
            self.finish_with_name().0
        }

        /// Stops the runtime and answers the Crate's translation and name as its world has them.
        pub(crate) fn finish_with_name(mut self) -> RuntimeEnd {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let out = self
                .thread
                .take()
                .and_then(|t| t.join().ok())
                .unwrap_or((None, None));
            std::fs::remove_dir_all(&self.project).ok();
            out
        }
    }

    impl Drop for HeadlessRuntime {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The path of translation component `axis` (0..3) inside `transform`'s JSON form, whichever
    /// form the reflect serializer gives a vector (an array or `{ x, y, z }`).
    pub(crate) fn axis_path(transform: &Value, axis: usize) -> String {
        if transform["translation"].is_array() {
            format!("translation/{axis}")
        } else {
            format!("translation/{}", ["x", "y", "z"][axis])
        }
    }

    /// ADR-8472 (P-302 device half): a game started by a device dev launch serves its agent on the
    /// launch's port with the launch's token; the editor reaches it through the host endpoint the
    /// device transport writes (here the forward is the identity: host and device share loopback)
    /// and lists its entities, and a connection with another token is refused.
    #[test]
    fn a_dev_launch_runtime_is_reached_with_the_handed_over_token() {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("a free loopback port")
            .port();
        let token = crate::device_transport::new_launch_token();
        let dev = regia_framework::dev_launch::DevLaunch {
            agent: Some(regia_framework::dev_launch::DevAgent {
                port,
                token: token.clone(),
            }),
            ..Default::default()
        };
        let runtime = HeadlessRuntime::start_with("dev-launch", Some(dev));
        let (own, _) = regia_agent::endpoint::read(&runtime.dir).expect("the game's endpoint");
        assert_eq!(
            own.url,
            format!("ws://127.0.0.1:{port}"),
            "the launch's port is bound"
        );

        let host = regia_test_utils::unique_temp_dir("remote-world-device-host").join("agent-1");
        crate::device_transport::write_host_endpoint(&host, port, 4242, "dev.regia.test", &token)
            .expect("host endpoint");
        let mut world = World::new();
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        let id = attach(&mut world, "Device game", host.clone());
        let state = call_method(&mut world, "editor.remote_select", json!({ "source": id })).expect("select");
        assert_eq!(state["connected"], json!(true), "{state}");
        let names: Vec<&str> = state["entities"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["name"].as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"Crate") && names.contains(&"Lamp"), "{state}");

        // Only the handed-over token opens it.
        let other = crate::device_transport::new_launch_token();
        assert!(regia_agent::client::Client::connect(&own.url, &other).is_err());
        assert!(regia_agent::client::Client::connect(&own.url, &token).is_ok());
        drop(world);
        let _ = runtime.finish();
        let _ = std::fs::remove_dir_all(host.parent().expect("parent"));
    }

    /// Acceptance (P-302): a headless launched runtime shows its entities in the remote Outliner,
    /// and a field set in the remote Inspector changes the runtime (read back through the protocol
    /// and from the runtime's own world).
    #[test]
    fn a_headless_runtime_shows_in_the_remote_outliner_and_takes_inspector_edits() {
        use crate::canvas_keys::test_input::{frame, labels_of};
        let runtime = HeadlessRuntime::start("acceptance");
        let mut world = World::new();
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        let id = attach(&mut world, "Headless game", runtime.dir.clone());
        let listed = call_method(&mut world, "editor.remote_sources", json!({})).expect("sources");
        assert!(
            listed["sources"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["id"] == json!(id))),
            "{listed}"
        );
        let state = call_method(&mut world, "editor.remote_select", json!({ "source": id })).expect("select");
        assert_eq!(state["connected"], json!(true), "{state}");

        // The Outliner draws the runtime's entities (as accessible rows).
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Outliner), Shown::Remote);
            });
        };
        frame(&ctx, [500.0, 600.0], vec![], &mut draw);
        let update = frame(&ctx, [500.0, 600.0], vec![], &mut draw);
        drop(draw);
        let labels = labels_of(&update, egui::accesskit::Role::Button);
        let all: Vec<String> = update
            .nodes
            .iter()
            .filter_map(|(_, n)| n.label().map(str::to_owned))
            .collect();
        assert!(
            labels.iter().chain(&all).any(|l| l == "Crate") && all.iter().any(|l| l == "Lamp"),
            "rows: {all:?}"
        );

        // Select Crate and set its translation through the Inspector's edit path.
        let crate_id = world
            .resource::<RemoteWorld>()
            .rows
            .iter()
            .find(|r| r.name.as_deref() == Some("Crate"))
            .map(|r| r.entity)
            .expect("Crate row");
        let state = call_method(
            &mut world,
            "editor.remote_select",
            json!({ "entities": [crate_id] }),
        )
        .expect("select entity");
        let transform = state["inspected"][0]["components"]["Transform"].clone();
        assert!(!transform.is_null(), "{state}");
        let path = axis_path(&transform, 1);
        let answer = call_method(
            &mut world,
            "editor.remote_set",
            json!({ "component": "Transform", "path": path, "value": 2.5 }),
        )
        .expect("set");
        assert_eq!(answer["applied"], json!(1), "{answer}");
        let back = &answer["values"][0]["value"];
        let mut read = back.clone();
        let y = field_mut(&mut read, &path_steps(&path)).cloned();
        assert_eq!(y, Some(json!(2.5)), "read back: {back}");
        assert_eq!(
            runtime.finish(),
            Some([1.0, 2.5, 0.0]),
            "the runtime's own world changed"
        );
    }

    /// Calls an editor method the way the agent dispatcher does (through `AgentExtension`).
    pub(crate) fn call_method(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = world
            .get_resource::<regia_agent::extension::AgentExtension>()
            .cloned()
            .unwrap_or_default();
        for (schema, handler) in methods().into_iter().chain(crate::live_link::methods()) {
            extension.register(schema, handler);
        }
        world.insert_resource(extension);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    /// The keyboard path (accessibility bar): Tab reaches the source switch, and a remote row is
    /// selected with Enter, no pointer involved.
    #[test]
    fn keyboard_selects_a_remote_row() {
        use crate::canvas_keys::test_input::{frame, key};
        let (mut world, _rt, _id) = world_with_fake();
        refresh(&mut world, true);
        let ctx = egui::Context::default();
        let size = [500.0, 600.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Outliner), Shown::Remote);
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let mut on_row = false;
        for _ in 0..12 {
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
                &mut draw,
            );
            let update = frame(&ctx, size, vec![], &mut draw);
            if crate::canvas_keys::test_input::focused_name(&update).as_deref() == Some("Crate") {
                on_row = true;
                break;
            }
        }
        assert!(on_row, "Tab reaches the first remote row");
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        assert_eq!(
            world.resource::<RemoteWorld>().selection,
            vec![9],
            "Down then Enter selects Lamp"
        );
    }

    /// Every label and value text of `update`.
    fn texts(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
        update
            .nodes
            .iter()
            .flat_map(|(_, node)| [node.label(), node.value()])
            .flatten()
            .map(str::to_owned)
            .collect()
    }

    /// P-302 (item 3): while play runs, the Outliner and Inspector the shell draws (through the
    /// source switch) show and edit the in-process play world: the Outliner lists an entity only
    /// the play world has, and a number typed into the Inspector changes the play entity and leaves
    /// the edit world alone. Switching the source to Edit shows the edit world again.
    #[test]
    fn the_panels_show_and_edit_the_in_process_play_world() {
        use crate::canvas_keys::test_input::{frame, key, text};
        use crate::command::{EditorCommand as _, SpawnEntities};
        use regia_core::id::Guid;
        use regia_scene::prelude::{EntityGuid, Transform};
        let mut edit = crate::live_link::tests::edit_app();
        edit.world_mut()
            .insert_resource(crate::theme::ThemeTokens::regia_default());
        let guid = Guid::from_u128(CRATE_GUID);
        let mut spawn =
            SpawnEntities::new_entity(edit.world(), guid, "Crate", Transform::default()).expect("spawn");
        spawn.apply(edit.world_mut());
        let mut session = crate::pie::start_play(edit.world_mut(), None).expect("play starts");
        crate::pie::sync_status(edit.world_mut(), Some(&session));
        session
            .app
            .world_mut()
            .spawn(regia_scene::components::Name::new("Spawned in play"));
        let played = session
            .app
            .world_mut()
            .query::<(Entity, &EntityGuid)>()
            .iter(session.app.world())
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("the play copy");
        let mut selection = crate::selection::Selection::default();
        selection.select(played, crate::selection::SelectMode::Replace);
        session.app.world_mut().insert_resource(selection);

        // The shell's Outliner and Inspector arms: the switch, then the panel on the world it names.
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let size = [700.0, 1200.0];
        let mut draw = |ui: &mut egui::Ui| {
            let (edit, play) = (edit.world_mut(), session.app.world_mut());
            egui::Panel::left("outliner").show(ui, |ui| match source_switch(ui, edit, Pane::Outliner) {
                Shown::Play => crate::outliner::outliner_panel(ui, play),
                Shown::Edit => crate::outliner::outliner_panel(ui, edit),
                Shown::Remote => {}
            });
            egui::CentralPanel::default().show(ui, |ui| match source_switch(ui, edit, Pane::Inspector) {
                Shown::Play => crate::inspector::inspector_panel(ui, play),
                Shown::Edit => crate::inspector::inspector_panel(ui, edit),
                Shown::Remote => {}
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        let shown = texts(&update);
        assert!(
            shown.iter().any(|t| t.contains("Spawned in play")),
            "the Outliner shows the play world: {shown:?}"
        );
        assert!(
            shown.iter().any(|t| t == "Transform"),
            "the Inspector shows the play entity: {shown:?}"
        );

        // Keyboard: Tab to the first number field of the Inspector, select its text, type, Enter.
        // Every field label is a Tab stop since 3216 (a frameless button) and every Outliner row
        // has its column toggles (3214), so the walk is longer than this test's first 60 presses;
        // the stops it passed are kept for the failure message.
        let mut on_number = false;
        let mut walked: Vec<String> = Vec::new();
        for _ in 0..240 {
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
                &mut draw,
            );
            let update = frame(&ctx, size, vec![], &mut draw);
            // A number field: a drag value, or the text field it turns into while typed in (the
            // Outliner's filter and the Name field hold no number).
            let focused = update
                .nodes
                .iter()
                .find(|(id, _)| *id == update.focus)
                .map(|(_, n)| n);
            let number = focused.is_some_and(|node| {
                node.role() == egui::accesskit::Role::SpinButton
                    || (node.role() == egui::accesskit::Role::TextInput
                        && node.value().is_some_and(|v| v.trim().parse::<f64>().is_ok()))
            });
            if number {
                on_number = true;
                break;
            }
            walked.push(focused.map_or_else(
                || "(none)".to_owned(),
                |node| format!("{:?} {:?}", node.role(), node.label().or(node.value())),
            ));
        }
        assert!(on_number, "Tab reaches an Inspector field; stops: {walked:?}");
        frame(
            &ctx,
            size,
            vec![key(egui::Key::A, egui::Modifiers::COMMAND)],
            &mut draw,
        );
        frame(&ctx, size, vec![text("5")], &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        let after = session
            .app
            .world()
            .get::<Transform>(played)
            .expect("transform")
            .translation;
        assert!(
            after.to_array().contains(&5.0),
            "the typed value reached the play entity: {after:?}"
        );
        let edit_entity = edit
            .world_mut()
            .query::<(Entity, &EntityGuid)>()
            .iter(edit.world())
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("edit entity");
        assert_eq!(
            edit.world().get::<Transform>(edit_entity).expect("t").translation,
            regia_core::math::Vec3::ZERO,
            "the edit world is untouched"
        );

        // Source Edit: the Outliner shows the edit world (no play-only entity).
        select_source(edit.world_mut(), EDIT).expect("edit source");
        let mut draw = |ui: &mut egui::Ui| {
            let (edit, play) = (edit.world_mut(), session.app.world_mut());
            match source_switch(ui, edit, Pane::Outliner) {
                Shown::Play => crate::outliner::outliner_panel(ui, play),
                Shown::Edit => crate::outliner::outliner_panel(ui, edit),
                Shown::Remote => {}
            }
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        let shown = texts(&update);
        assert!(shown.iter().any(|t| t.contains("Crate")), "{shown:?}");
        assert!(!shown.iter().any(|t| t.contains("Spawned in play")), "{shown:?}");
        crate::pie::stop_play(session);
    }

    /// The labels of `update`'s nodes, top to bottom.
    fn rows_top_down(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
        let mut rows: Vec<(f64, String)> = update
            .nodes
            .iter()
            .filter_map(|(_, node)| Some((node.bounds()?.y0, node.label()?.to_owned())))
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.into_iter().map(|(_, label)| label).collect()
    }

    /// Acceptance (P-302 item 1): the remote Outliner is a tree. A child the runtime lists before
    /// its parent is drawn right under the parent, indented; the agent sees its parent and depth;
    /// Left on the parent's row hides the child, and that state is kept when the source changes
    /// and comes back.
    #[test]
    fn the_remote_outliner_shows_a_child_under_its_parent() {
        use crate::canvas_keys::test_input::{frame, key};
        let (mut world, rt, id) = world_with_fake();
        {
            let mut rt = rt.lock().expect("lock");
            rt.entities = vec![
                (3, "Wheel".into(), json!({ "translation": [0.0, 0.0, 0.0] })),
                (5, "Cart".into(), json!({ "translation": [0.0, 0.0, 0.0] })),
                (9, "Lamp".into(), json!({ "translation": [0.0, 0.0, 0.0] })),
            ];
            rt.parents = vec![(3, 5)];
        }
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        let state = call_method(&mut world, "editor.remote_select", json!({})).expect("read");
        let wheel = state["entities"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["entity"] == json!(3)).cloned())
            .unwrap_or(Value::Null);
        assert_eq!(wheel["parent"], json!(5), "the listing names the parent: {state}");
        assert_eq!(wheel["depth"], json!(1), "{state}");

        let ctx = egui::Context::default();
        let size = [500.0, 600.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Outliner), Shown::Remote);
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        let rows: Vec<String> = rows_top_down(&update)
            .into_iter()
            .filter(|l| ["Wheel", "Cart", "Lamp"].iter().any(|n| l.ends_with(n)))
            .collect();
        let at = |name: &str| rows.iter().position(|l| l.ends_with(name));
        assert_eq!(
            at("Wheel"),
            at("Cart").map(|c| c + 1),
            "the child is drawn right under its parent: {rows:?}"
        );
        let wheel_row = rows
            .iter()
            .find(|l| l.ends_with("Wheel"))
            .cloned()
            .unwrap_or_default();
        assert!(
            wheel_row.starts_with(char::is_whitespace),
            "the child row is indented: {wheel_row:?}"
        );

        // Keyboard: Tab to the parent's row, Left hides its children.
        let mut on_cart = false;
        for _ in 0..12 {
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
                &mut draw,
            );
            let update = frame(&ctx, size, vec![], &mut draw);
            if crate::canvas_keys::test_input::focused_name(&update).is_some_and(|n| n.ends_with("Cart")) {
                on_cart = true;
                break;
            }
        }
        assert!(on_cart, "Tab reaches the parent's row");
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowLeft, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        let rows = rows_top_down(&update);
        assert!(
            !rows.iter().any(|l| l.ends_with("Wheel")),
            "Left collapses the parent: {rows:?}"
        );
        // The expand state belongs to the source: kept across a switch away and back.
        select_source(&mut world, EDIT).expect("edit");
        select_source(&mut world, &id).expect("back");
        // The fake is not dialled from its directory: hand it over again.
        world
            .resource_mut::<RemoteWorld>()
            .set_transport(&id, Box::new(rt.clone()));
        let state = call_method(&mut world, "editor.remote_select", json!({})).expect("read");
        assert_eq!(state["collapsed"], json!([5]), "{state}");
        let state =
            call_method(&mut world, "editor.remote_select", json!({ "expand": [5] })).expect("expand");
        assert_eq!(state["collapsed"], json!([]), "{state}");
    }

    /// Acceptance (P-302 item 2): a text field of the remote Inspector is edited from the keyboard
    /// (Tab, Ctrl+A, type, Enter) and the launched game's entity takes the new text.
    #[test]
    fn a_text_field_edit_reaches_the_launched_game() {
        use crate::canvas_keys::test_input::{frame, key, text};
        let runtime = HeadlessRuntime::start("text-edit");
        let mut world = World::new();
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        let id = attach(&mut world, "Headless game", runtime.dir.clone());
        call_method(&mut world, "editor.remote_select", json!({ "source": id })).expect("select");
        let crate_id = world
            .resource::<RemoteWorld>()
            .rows
            .iter()
            .find(|r| r.name.as_deref() == Some("Crate"))
            .map(|r| r.entity)
            .expect("Crate row");
        call_method(
            &mut world,
            "editor.remote_select",
            json!({ "entities": [crate_id] }),
        )
        .expect("entity");

        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let size = [600.0, 1200.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Inspector), Shown::Remote);
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let mut on_text = false;
        let mut walked = Vec::new();
        for _ in 0..80 {
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
                &mut draw,
            );
            let update = frame(&ctx, size, vec![], &mut draw);
            let focused = update
                .nodes
                .iter()
                .find(|(id, _)| *id == update.focus)
                .map(|(_, n)| n);
            if focused
                .is_some_and(|n| n.role() == egui::accesskit::Role::TextInput && n.value() == Some("Crate"))
            {
                on_text = true;
                break;
            }
            walked.push(focused.map(|n| format!("{:?} {:?}", n.role(), n.label())));
        }
        assert!(on_text, "Tab reaches the Name text field; stops: {walked:?}");
        frame(
            &ctx,
            size,
            vec![key(egui::Key::A, egui::Modifiers::COMMAND)],
            &mut draw,
        );
        frame(&ctx, size, vec![text("Barrel")], &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        // A forced read waits behind the edit (one connection, calls in order).
        refresh(&mut world, true);
        let name = world
            .resource::<RemoteWorld>()
            .inspected
            .first()
            .and_then(|(_, c)| c.get("Name").cloned());
        assert_eq!(name, Some(json!("Barrel")), "read back through the protocol");
        assert_eq!(
            runtime.finish_with_name().1.as_deref(),
            Some("Barrel"),
            "the runtime's own world changed"
        );
    }

    /// Acceptance (P-303 item 4): dragging a number in the remote Inspector over several frames is
    /// one `scene.apply_edits` transaction, sent when the drag ends, carrying the final value.
    #[test]
    fn a_drag_is_one_remote_transaction() {
        use crate::canvas_keys::test_input::frame;
        let (mut world, rt, _id) = world_with_fake();
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        select_entities(&mut world, &[5], false);
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let size = [600.0, 900.0];
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Inspector), Shown::Remote);
            });
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        let spin = update
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == egui::accesskit::Role::SpinButton)
            .filter_map(|(_, n)| n.bounds())
            .min_by(|a, b| a.y0.total_cmp(&b.y0).then(a.x0.total_cmp(&b.x0)))
            .expect("a number field");
        let at = egui::pos2(
            ((spin.x0 + spin.x1) / 2.0) as f32,
            ((spin.y0 + spin.y1) / 2.0) as f32,
        );
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(&ctx, size, vec![egui::Event::PointerMoved(at)], &mut draw);
        frame(&ctx, size, vec![button(at, true)], &mut draw);
        for k in 1..=4 {
            frame(
                &ctx,
                size,
                vec![egui::Event::PointerMoved(at + egui::vec2(25.0 * k as f32, 0.0))],
                &mut draw,
            );
        }
        let end = at + egui::vec2(100.0, 0.0);
        frame(&ctx, size, vec![button(end, false)], &mut draw);
        frame(&ctx, size, vec![], &mut draw);
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        refresh(&mut world, true);
        let rt = rt.lock().expect("lock");
        let edits = rt.calls.iter().filter(|c| *c == "scene.apply_edits").count();
        assert_eq!(edits, 1, "one transaction per drag gesture: {:?}", rt.calls);
        assert_ne!(
            rt.entities[0].2,
            json!({ "translation": [0.0, 1.0, 0.0] }),
            "the drag's value reached the runtime"
        );
    }

    /// The agent route of Show or hide children (the `remote.toggle_children` parity arm):
    /// `editor.remote_select { collapse }` then `{ expand }` on the edit source answers the
    /// current source's collapsed rows, with no runtime.
    #[test]
    fn toggle_children_route_answers_the_collapsed_rows() {
        let mut world = World::new();
        let out =
            call_method(&mut world, "editor.remote_select", json!({ "collapse": [7, 3] })).expect("collapse");
        assert_eq!(out["source"]["id"], json!(EDIT), "{out}");
        assert_eq!(out["collapsed"], json!([3, 7]), "{out}");
        let out = call_method(&mut world, "editor.remote_select", json!({ "expand": [7] })).expect("expand");
        assert_eq!(out["collapsed"], json!([3]), "{out}");
        let err = call_method(&mut world, "editor.remote_select", json!({ "collapse": "7" }))
            .expect_err("not an array");
        assert!(err.message.contains("collapse"), "{err:?}");
    }

    /// A runtime that holds every call until the test opens its gate (a 10 s safety cap releases it
    /// and marks the call as released by the cap).
    struct GatedRuntime {
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        started: Arc<std::sync::atomic::AtomicUsize>,
        capped: Arc<AtomicBool>,
    }

    impl RemoteTransport for GatedRuntime {
        fn call(&mut self, _method: &str, _params: Value) -> Result<Value, RemoteError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let (lock, wake) = &*self.gate;
            let mut open = lock
                .lock()
                .map_err(|_| RemoteError::Connection("poisoned".into()))?;
            while !*open {
                let (next, timeout) = wake
                    .wait_timeout(open, Duration::from_secs(10))
                    .map_err(|_| RemoteError::Connection("poisoned".into()))?;
                open = next;
                if timeout.timed_out() {
                    self.capped.store(true, Ordering::SeqCst);
                    break;
                }
            }
            Ok(json!({ "entities": [] }))
        }
    }

    /// Item 4: remote calls run off the editor thread. While the runtime holds a call, the panel's
    /// frame still returns (the call is not released by the cap), and the queue takes at most
    /// `QUEUE_DEPTH` waiting jobs.
    #[test]
    fn a_slow_runtime_does_not_hold_the_editor_frame() {
        use crate::canvas_keys::test_input::frame;
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capped = Arc::new(AtomicBool::new(false));
        let mut world = World::new();
        world.insert_resource(crate::theme::ThemeTokens::regia_default());
        let dir = std::env::temp_dir().join("regia-remote-world-gated-never-read");
        let id = attach(&mut world, "Slow game", dir.clone());
        select_source(&mut world, &id).expect("listed");
        world.resource_mut::<RemoteWorld>().set_transport(
            &id,
            Box::new(GatedRuntime {
                gate: gate.clone(),
                started: started.clone(),
                capped: capped.clone(),
            }),
        );
        let ctx = egui::Context::default();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(source_switch(ui, &mut world, Pane::Outliner), Shown::Remote);
            });
        };
        frame(&ctx, [500.0, 600.0], vec![], &mut draw);
        frame(&ctx, [500.0, 600.0], vec![], &mut draw);
        drop(draw);
        // The read the panel queued is held by the runtime: wait until its thread has taken it.
        while started.load(Ordering::SeqCst) == 0 && !capped.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        assert!(
            !capped.load(Ordering::SeqCst),
            "the frames returned while the runtime held the call (it was not released by the cap)"
        );
        // The thread is busy with the held read: the queue takes QUEUE_DEPTH more jobs, then refuses.
        {
            let mut remote = world.resource_mut::<RemoteWorld>();
            let job =
                || -> Job { Box::new(|_transport: &mut dyn RemoteTransport| -> Answer { Box::new(()) }) };
            for k in 0..QUEUE_DEPTH {
                assert!(
                    remote.try_queue(&dir, JobKind::Call, job()).is_ok(),
                    "job {k} queued"
                );
            }
            assert_eq!(
                remote.try_queue(&dir, JobKind::Call, job()),
                Err(QueueError::Full)
            );
        }
        // Open the gate: everything drains.
        {
            let (lock, wake) = &*gate;
            *lock.lock().expect("gate") = true;
            wake.notify_all();
        }
        refresh(&mut world, true);
        assert!(!capped.load(Ordering::SeqCst));
        assert_eq!(world.resource::<RemoteWorld>().error, None);
    }

    /// The tree order: children right under their parent, depth-first in listing order; a collapsed
    /// row hides its whole subtree; a row whose parent is not listed is a root; a cycle shows at
    /// the root instead of vanishing.
    #[test]
    fn tree_order_nests_collapses_and_survives_bad_listings() {
        let row = |entity: u64, parent: Option<u64>| RemoteRow {
            entity,
            name: Some(format!("e{entity}")),
            kind: None,
            parent,
        };
        // 4 under 2 under 1; 3 under 1; 9's parent is not listed; 7 and 8 are each other's parent.
        let rows = vec![
            row(4, Some(2)),
            row(1, None),
            row(2, Some(1)),
            row(3, Some(1)),
            row(9, Some(100)),
            row(7, Some(8)),
            row(8, Some(7)),
        ];
        let order = |collapsed: &[u64]| -> Vec<(u64, usize)> {
            let set: BTreeSet<u64> = collapsed.iter().copied().collect();
            tree_order(&rows, Some(&set))
                .into_iter()
                .map(|(i, depth, _)| (rows[i].entity, depth))
                .collect()
        };
        assert_eq!(
            order(&[]),
            vec![(1, 0), (2, 1), (4, 2), (3, 1), (9, 0), (7, 0), (8, 0)]
        );
        assert_eq!(order(&[2]), vec![(1, 0), (2, 1), (3, 1), (9, 0), (7, 0), (8, 0)]);
        assert_eq!(order(&[1]), vec![(1, 0), (9, 0), (7, 0), (8, 0)]);
        let kids: Vec<bool> = tree_order(&rows, None).into_iter().map(|(_, _, k)| k).collect();
        assert_eq!(kids, vec![true, true, false, false, false, true, true]);
    }
}
