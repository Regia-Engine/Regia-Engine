// SPDX-License-Identifier: MPL-2.0
//! Where the Profiler panel reads from (`docs/specs/83-prefab-profiler-volumes.md` § 4.3,
//! `book/agents/profiler.md`, ADR-9740): the edit world, the in-process play world, a
//! separate-process PIE child, or a running dev build reached by address and token.
//!
//! - **edit / pie**: the process frame profiler (`regia::Profiler`). Play frames run inside the
//!   `pie_frame` / `pie_step` / `pie_client_frame` spans, so the play source is those subtrees and
//!   the edit source is everything else ([`crate::profiler_model::split_source`]). Entity counts come
//!   from the edit world, or the play world's counts the editor loop leaves in
//!   [`ProfilerView::pie_counts`].
//! - **process / remote**: the agent protocol of another process, through `regia_agent`'s own
//!   client. A worker thread connects, turns flame capture on (`profiler.set_capture`) and polls
//!   `profiler.frame` (the 60-frame tree, and for the history every frame that ended since the
//!   last poll through its `since_frame` cursor, with the ranked entity counts) and
//!   every fourth poll `memory.report`. The panel reads the worker's latest snapshot without
//!   waiting; a dropped connection shows as `disconnected` with the error, the worker retries
//!   every second, and the editor frame never blocks on the network. A separate-process PIE child
//!   advertises its endpoint in its own agent directory (`PieProcesses::agent_dir`).
//!
//! The Timeline tab (ADR-7325) reads one frame of the per-thread timeline over the same paths
//! ([`read_timeline`]): the process profiler for edit and play (split by the play spans), and for a
//! process source the worker's `profiler.timeline` poll, made only while the tab asks for it
//! ([`RemoteWorker::set_timeline_request`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use regia::telemetry::profile_capture::{
    CaptureStripFrame, CaptureTimelineFrame, DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES, ProfileCapture,
};
use regia::telemetry::profile_compare::{CaptureDiff, compare};
use regia_agent::client::{Client, ClientError};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::pie_process::{PieProcesses, ProcessRole};
use crate::profiler_model::{
    self, EntityCounts, FrameSample, HISTORY_FRAMES, TOP_COMPONENTS, hitch_threshold_ms, hitches,
};
use crate::viewport_agent::Handler;

/// Time between two polls of a connected process.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Time between two connection attempts after a failure.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Longest wait for one answer before the poll counts as failed (the connection is then redone).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Polls between two `memory.report` reads.
pub const MEMORY_EVERY_POLLS: u64 = 4;
/// Frames the panel's tree averages.
pub const TREE_FRAMES: usize = crate::profiler_panel::PANEL_FRAMES;
/// Spans in the top list.
pub const TOP_SPANS: usize = 10;
/// Most spans the worker asks `profiler.timeline` for (its maximum).
pub const TIMELINE_SPANS: u64 = 100_000;
/// The reason a process source shows as disconnected when its worker panicked while storing a
/// poll (the snapshot lock is poisoned; ADR-7427).
pub const WORKER_PANICKED: &str = "the profiler worker panicked";
/// `Shared::timeline_frame` value for the newest frame.
const LIVE_FRAME: u64 = u64::MAX;

/// What the panel profiles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProfilerSource {
    /// The editor's own world (every span outside play).
    #[default]
    Edit,
    /// The in-process play world and its client worlds.
    Pie,
    /// A separate-process PIE child, over the protocol.
    Process(ProcessRole),
    /// A running dev build at `address` (a loopback WebSocket URL) with `token`.
    Remote {
        /// `ws://127.0.0.1:<port>`.
        address: String,
        /// The bearer token from its `.regia/agent/token`.
        token: String,
    },
}

impl ProfilerSource {
    /// `edit`, `pie`, `process` or `remote`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Pie => "pie",
            Self::Process(_) => "process",
            Self::Remote { .. } => "remote",
        }
    }

    /// The picker's label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Edit => "Edit world".to_owned(),
            Self::Pie => "Play (in editor)".to_owned(),
            Self::Process(role) => format!("PIE process {}", role_name(*role)),
            Self::Remote { address, .. } => format!("Dev build {address}"),
        }
    }
}

/// `server` or `client <k>`: the name `editor.profiler_source` takes for a PIE process.
#[must_use]
pub fn role_name(role: ProcessRole) -> String {
    match role {
        ProcessRole::Server => "server".to_owned(),
        ProcessRole::Client(k) => format!("client {k}"),
    }
}

/// Parses `server`, `client <k>`, `client:<k>` or `client<k>`.
#[must_use]
pub fn parse_role(text: &str) -> Option<ProcessRole> {
    let text = text.trim();
    if text == "server" {
        return Some(ProcessRole::Server);
    }
    let rest = text.strip_prefix("client")?.trim_start_matches([' ', ':']);
    rest.parse().ok().map(ProcessRole::Client)
}

/// A loopback WebSocket URL for `address` (`127.0.0.1:<port>` gets `ws://`), or `None` when it
/// names another host: the token is never sent off the machine (the client refuses that too).
#[must_use]
pub fn remote_url(address: &str) -> Option<String> {
    let address = address.trim();
    let url = if address.contains("://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let rest = url.strip_prefix("ws://")?;
    let host = rest.split([':', '/']).next().unwrap_or_default();
    (host == "127.0.0.1").then_some(url)
}

/// The connection state of a process source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// No answer yet.
    Connecting,
    /// The last poll was answered.
    Connected,
    /// The connection failed or dropped; the worker retries.
    Disconnected(String),
}

impl LinkState {
    /// `connecting`, `connected` or `disconnected`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnected(_) => "disconnected",
        }
    }
}

/// What the worker last read from a process.
#[derive(Clone, Debug)]
pub struct RemoteSnapshot {
    /// Polls answered so far.
    pub polls: u64,
    /// The connection state.
    pub link: LinkState,
    /// The 60-frame call tree of the last poll.
    pub capture: Option<ProfileCapture>,
    /// Its `top` spans: `(name, ms)`.
    pub top: Vec<(String, f64)>,
    /// Every frame of the process, oldest first, at most [`HISTORY_FRAMES`]: each poll reads
    /// the frames that ended since the last one (`profiler.frame { since_frame }`); a build older
    /// than that cursor gives one sample per poll.
    pub history: Vec<FrameSample>,
    /// The process's ECS counts with its component types ranked (`profiler.frame { entities }`),
    /// read with `memory.report`; `None` from a build older than that field.
    pub entities: Option<EntityCounts>,
    /// The last `memory.report`.
    pub memory: Option<Value>,
    /// The last `profiler.timeline` frame, while the Timeline tab asks for it (ADR-7325). With a
    /// strip request this is the combined frame on the strip's axis.
    pub timeline: Option<Arc<CaptureTimelineFrame>>,
    /// The strip's frames (each with its start offset on the axis), single-frame requests
    /// included.
    pub timeline_frames: Vec<CaptureStripFrame>,
    /// Why the last `profiler.timeline` call was refused (`timeline_off`, a frame that left the
    /// history), when it was.
    pub timeline_error: Option<String>,
}

impl Default for RemoteSnapshot {
    fn default() -> Self {
        Self {
            polls: 0,
            link: LinkState::Connecting,
            capture: None,
            top: Vec::new(),
            history: Vec::new(),
            entities: None,
            memory: None,
            timeline: None,
            timeline_frames: Vec::new(),
            timeline_error: None,
        }
    }
}

/// Where the worker finds the process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTarget {
    /// A URL and token given by the user.
    Url {
        /// `ws://127.0.0.1:<port>`.
        url: String,
        /// The bearer token.
        token: String,
    },
    /// An agent directory (`endpoint.json` + `token`), read again on every connection attempt:
    /// a PIE child writes it once it is up.
    AgentDir(PathBuf),
}

struct Shared {
    snapshot: Mutex<RemoteSnapshot>,
    polled: Condvar,
    /// The Timeline tab is shown: poll `profiler.timeline` too.
    want_timeline: AtomicBool,
    /// The frame number the tab holds, or `LIVE_FRAME`.
    timeline_frame: AtomicU64,
    /// Consecutive frames the tab lays out as a strip (1 to [`MAX_TIMELINE_FRAMES`]).
    timeline_frames: AtomicUsize,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            snapshot: Mutex::default(),
            polled: Condvar::new(),
            want_timeline: AtomicBool::new(false),
            timeline_frame: AtomicU64::new(LIVE_FRAME),
            timeline_frames: AtomicUsize::new(1),
        }
    }
}

/// The polling thread of a process source. Dropping it stops the thread at its next wake-up
/// without waiting for it.
pub struct RemoteWorker {
    target: RemoteTarget,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    wake: Sender<()>,
}

impl std::fmt::Debug for RemoteWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteWorker")
            .field("target", &self.target)
            .finish()
    }
}

impl Drop for RemoteWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.wake.send(());
    }
}

impl RemoteWorker {
    /// Starts polling `target` on a new thread.
    ///
    /// # Errors
    /// The thread could not be spawned.
    pub fn start(target: RemoteTarget) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, woken) = channel();
        let (t, s, x) = (target.clone(), Arc::clone(&shared), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("profiler-remote".into())
            .spawn(move || run(&t, &s, &x, &woken))?;
        Ok(Self {
            target,
            shared,
            stop,
            wake,
        })
    }

    /// The target it polls.
    #[must_use]
    pub fn target(&self) -> &RemoteTarget {
        &self.target
    }

    /// A copy of the latest snapshot. Never waits on the network: the worker holds the lock only
    /// to store a finished poll.
    ///
    /// A poisoned lock (the worker panicked while storing a poll; it polls no more) reads as
    /// [`LinkState::Disconnected`] with [`WORKER_PANICKED`], keeping the last stored data (ADR-7427).
    #[must_use]
    pub fn snapshot(&self) -> RemoteSnapshot {
        match self.shared.snapshot.lock() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => worker_panicked(&poisoned.into_inner()),
        }
    }

    /// Asks the worker to poll `profiler.timeline` (`want`), for frame `frame` (`None`: the
    /// newest) with `frames` consecutive frames as a strip (1 to [`MAX_TIMELINE_FRAMES`]). The
    /// first poll that wants it turns the process's timeline on.
    pub fn set_timeline_request(&self, want: bool, frame: Option<u64>, frames: usize) {
        self.shared.want_timeline.store(want, Ordering::Relaxed);
        self.shared
            .timeline_frame
            .store(frame.unwrap_or(LIVE_FRAME), Ordering::Relaxed);
        self.shared
            .timeline_frames
            .store(frames.clamp(1, MAX_TIMELINE_FRAMES), Ordering::Relaxed);
    }

    /// Blocks until at least `polls` polls were answered or `timeout` passed; returns the snapshot
    /// then. For tests and tools (bounded waits on the worker, no sleeps).
    /// A poisoned lock answers at once, as [`Self::snapshot`] does.
    #[must_use]
    pub fn wait_for_polls(&self, polls: u64, timeout: Duration) -> RemoteSnapshot {
        let guard = match self.shared.snapshot.lock() {
            Ok(guard) => guard,
            Err(poisoned) => return worker_panicked(&poisoned.into_inner()),
        };
        let result = self
            .shared
            .polled
            .wait_timeout_while(guard, timeout, |s| s.polls < polls);
        match result {
            Ok((guard, _)) => guard.clone(),
            Err(poisoned) => worker_panicked(&poisoned.into_inner().0),
        }
    }
}

/// The snapshot of a worker that panicked while storing a poll: its last data, disconnected with
/// [`WORKER_PANICKED`].
fn worker_panicked(last: &RemoteSnapshot) -> RemoteSnapshot {
    RemoteSnapshot {
        link: LinkState::Disconnected(format!(
            "{WORKER_PANICKED} while storing a poll; pick another source and back (or call editor.profiler_source), or Connect again for a dev build, to restart it"
        )),
        ..last.clone()
    }
}

fn connect(target: &RemoteTarget) -> Result<Client, String> {
    let (url, token) = match target {
        RemoteTarget::Url { url, token } => (url.clone(), token.clone()),
        RemoteTarget::AgentDir(dir) => {
            let (endpoint, token) = regia_agent::endpoint::read(dir)
                .map_err(|e| format!("no endpoint in {} yet: {e}", dir.display()))?;
            (endpoint.url, token)
        }
    };
    let mut client = Client::connect(&url, &token).map_err(|e| e.to_string())?;
    client
        .set_read_timeout(Some(CALL_TIMEOUT))
        .map_err(|e| e.to_string())?;
    client
        .call("profiler.set_capture", json!({ "flame": true }))
        .map_err(|e| e.to_string())?;
    Ok(client)
}

/// A `profiler.frame` answer as a capture.
#[must_use]
pub fn capture_from_frame(frame: &Value) -> Option<ProfileCapture> {
    let value = json!({
        "format_version": 1,
        "frames": frame.get("frames")?,
        "frame_ms": frame.get("frame_ms")?,
        "cpu": frame.get("cpu")?,
        "truncated": frame.get("truncated").cloned().unwrap_or(json!(0)),
        "gpu": frame.get("gpu").cloned().unwrap_or(json!([])),
    });
    serde_json::from_value(value).ok()
}

/// The `top` list of a `profiler.frame` answer.
#[must_use]
pub fn top_from_frame(frame: &Value) -> Vec<(String, f64)> {
    frame["top"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| Some((r["name"].as_str()?.to_owned(), r["ms"].as_f64()?)))
                .collect()
        })
        .unwrap_or_default()
}

/// One `profiler.timeline` poll: turns the process's timeline on first (`*timeline_on`), then reads
/// the held or newest frame. A refusal (`timeline_off`, `bad_frame`) is kept in the snapshot and is
/// not a connection failure.
fn poll_timeline(
    client: &mut Client,
    shared: &Shared,
    timeline_on: &mut bool,
    snapshot: &mut RemoteSnapshot,
) -> Result<(), ClientError> {
    if !*timeline_on {
        match client.call("profiler.set_capture", json!({ "timeline": true })) {
            Ok(_) => *timeline_on = true,
            // A build that refuses the timeline switch (one older than ADR-7325 answers
            // `bad_flame`) keeps every other tab connected: the refusal shows on the Timeline
            // tab only, as a `profiler.timeline` refusal does (rev-zai-03 finding 1).
            Err(ClientError::Rpc(e)) => {
                snapshot.timeline = None;
                snapshot.timeline_error = Some(refusal_text(e.data.as_ref(), &e.message));
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    let frame = shared.timeline_frame.load(Ordering::Relaxed);
    let frames = shared.timeline_frames.load(Ordering::Relaxed).max(1);
    let mut params = json!({ "max_spans": TIMELINE_SPANS, "frames": frames });
    if frame != LIVE_FRAME
        && let Some(map) = params.as_object_mut()
    {
        map.insert("frame".into(), json!(frame));
    }
    match client.call("profiler.timeline", params) {
        Ok(answer) => {
            snapshot.timeline = crate::thread_timeline::frame_from_answer(&answer).map(Arc::new);
            snapshot.timeline_frames = answer
                .get("frames")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            snapshot.timeline_error = None;
            Ok(())
        }
        Err(ClientError::Rpc(e)) => {
            snapshot.timeline_error = Some(refusal_text(e.data.as_ref(), &e.message));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The Timeline tab's text for a refused call: the error data's `reason` (or `error`) and the
/// message.
fn refusal_text(data: Option<&Value>, message: &str) -> String {
    let reason = data
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("error");
    format!("{reason}: {message}")
}

/// The history cursor of a connection: the next frame number `profiler.frame { since_frame }` is
/// asked from, or `None` once the process refused the cursor (a build older than it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HistoryCursor(Option<u64>);

impl Default for HistoryCursor {
    fn default() -> Self {
        Self(Some(0))
    }
}

/// One frame sample from a `frame_times` row of `profiler.frame { since_frame }`.
fn sample_from_row(row: &Value) -> Option<(u64, FrameSample)> {
    let number = row["number"].as_u64()?;
    let ms = row["ms"].as_f64()?;
    let spans = row["spans"]
        .as_array()
        .map(|spans| {
            spans
                .iter()
                .filter_map(|s| Some((s["name"].as_str()?.to_owned(), s["ms"].as_f64()?)))
                .take(profiler_model::HITCH_TOP_SPANS)
                .collect()
        })
        .unwrap_or_default();
    Some((number, FrameSample { ms, spans }))
}

/// Folds one `profiler.frame` answer into the history: with `frame_times` every listed frame is
/// appended and the cursor moves past the newest; a process whose newest frame is behind the
/// cursor restarted, so the cursor goes back to 0 (its whole history comes next poll). Without
/// `frame_times` (an older build) the answer's one-frame time is one sample. The history keeps the
/// newest [`HISTORY_FRAMES`].
fn fold_history(history: &mut Vec<FrameSample>, cursor: &mut HistoryCursor, answer: &Value) {
    match (
        answer.get("frame_times").and_then(Value::as_array),
        cursor.0.as_mut(),
    ) {
        (Some(rows), Some(next)) => {
            if let Some(newest) = answer["newest_frame"].as_u64()
                && newest.saturating_add(1) < *next
            {
                *next = 0;
                return;
            }
            for (number, sample) in rows.iter().filter_map(sample_from_row) {
                if number >= *next {
                    history.push(sample);
                    *next = number + 1;
                }
            }
        }
        _ => {
            if let Some(ms) = answer["frame_ms"].as_f64() {
                history.push(FrameSample {
                    ms,
                    spans: top_from_frame(answer)
                        .into_iter()
                        .take(profiler_model::HITCH_TOP_SPANS)
                        .collect(),
                });
            }
        }
    }
    if history.len() > HISTORY_FRAMES {
        let extra = history.len() - HISTORY_FRAMES;
        history.drain(..extra);
    }
}

fn poll(
    client: &mut Client,
    polls: u64,
    cursor: &mut HistoryCursor,
    snapshot: &mut RemoteSnapshot,
) -> Result<(), ClientError> {
    let tree = client.call(
        "profiler.frame",
        json!({ "frames": TREE_FRAMES, "depth": DEFAULT_MAX_DEPTH, "max_nodes": DEFAULT_MAX_NODES }),
    )?;
    let read_memory = polls.is_multiple_of(MEMORY_EVERY_POLLS);
    let one = match cursor.0 {
        Some(next) => {
            let mut params = json!({ "frames": 1, "depth": 1, "since_frame": next });
            if read_memory && let Some(map) = params.as_object_mut() {
                map.insert("entities".into(), json!(TOP_COMPONENTS));
            }
            match client.call("profiler.frame", params) {
                Ok(answer) => answer,
                // A build older than the cursor refuses the parameter: one sample per poll.
                Err(ClientError::Rpc(_)) => {
                    *cursor = HistoryCursor(None);
                    client.call("profiler.frame", json!({ "frames": 1, "depth": 1 }))?
                }
                Err(e) => return Err(e),
            }
        }
        None => client.call("profiler.frame", json!({ "frames": 1, "depth": 1 }))?,
    };
    let memory = if read_memory {
        Some(client.call("memory.report", json!({ "top": 10 }))?)
    } else {
        None
    };
    snapshot.capture = capture_from_frame(&tree);
    snapshot.top = top_from_frame(&tree);
    fold_history(&mut snapshot.history, cursor, &one);
    if let Some(entities) = one
        .get("entities")
        .and_then(profiler_model::entity_counts_from_answer)
    {
        snapshot.entities = Some(entities);
    }
    if memory.is_some() {
        snapshot.memory = memory;
    }
    Ok(())
}

fn run(target: &RemoteTarget, shared: &Shared, stop: &AtomicBool, woken: &Receiver<()>) {
    let mut client: Option<Client> = None;
    let mut timeline_on = false;
    let mut polls = 0_u64;
    let mut cursor = HistoryCursor::default();
    let mut working = RemoteSnapshot::default();
    let publish = |snapshot: &RemoteSnapshot| {
        if let Ok(mut s) = shared.snapshot.lock() {
            *s = snapshot.clone();
        }
        shared.polled.notify_all();
    };
    while !stop.load(Ordering::Relaxed) {
        let wait = match client.as_mut() {
            None => match connect(target) {
                Ok(c) => {
                    client = Some(c);
                    timeline_on = false;
                    // A new connection may reach a newer build: try the history cursor again.
                    if cursor.0.is_none() {
                        cursor = HistoryCursor::default();
                    }
                    Duration::ZERO
                }
                Err(e) => {
                    working.link = LinkState::Disconnected(e);
                    publish(&working);
                    RETRY_INTERVAL
                }
            },
            Some(c) => {
                let _span = tracing::trace_span!("profiler_remote_poll").entered();
                let polled = poll(c, polls, &mut cursor, &mut working).and_then(|()| {
                    if shared.want_timeline.load(Ordering::Relaxed) {
                        poll_timeline(c, shared, &mut timeline_on, &mut working)
                    } else {
                        Ok(())
                    }
                });
                match polled {
                    Ok(()) => {
                        polls += 1;
                        working.polls = polls;
                        working.link = LinkState::Connected;
                    }
                    Err(e) => {
                        working.link = LinkState::Disconnected(e.to_string());
                        client = None;
                    }
                }
                publish(&working);
                POLL_INTERVAL
            }
        };
        if wait.is_zero() {
            continue;
        }
        match woken.recv_timeout(wait) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// The Profiler panel's view state beside `crate::profiler_panel::ProfilerPanel`: the source,
/// its worker, the frozen history frame, opened and compared captures.
#[derive(Resource, Debug, Default)]
pub struct ProfilerView {
    source: ProfilerSource,
    worker: Option<RemoteWorker>,
    /// The play world's counts, left by the editor loop while the play source is shown.
    pub pie_counts: Option<EntityCounts>,
    /// The history frame clicked (index into the shown history), when frozen. Set it through
    /// [`freeze_history`], which also stops the history's intake.
    pub frozen: Option<usize>,
    /// The history as it was when a frame was frozen: shown instead of the live one until
    /// unfrozen, so the frozen index keeps naming the same frame ([`freeze_history`]).
    frozen_history: Option<Vec<FrameSample>>,
    /// A capture opened from a file, shown instead of the live tree: (path, capture).
    pub opened: Option<(String, ProfileCapture)>,
    /// Compare A (baseline): (path, capture).
    pub compare_a: Option<(String, ProfileCapture)>,
    /// Compare B: (path, capture).
    pub compare_b: Option<(String, ProfileCapture)>,
    /// The last error of a source change or file open.
    pub error: Option<String>,
    /// The path field of the Open / Compare row.
    pub path_field: String,
    /// The last local `memory.report` and the editor frame it was read at.
    pub memory_cache: Option<(u64, Value)>,
    /// The tab shown.
    pub tab: ProfilerTab,
    /// The Dev build address field.
    pub remote_address: String,
    /// The Dev build token field.
    pub remote_token: String,
    /// The data read for the current repaint: (egui frame number, data). A second pass of the
    /// window in the same frame reuses it.
    pub frame_data: Option<(u64, SourceData)>,
    /// How many times the panel read its source (once per repaint; tests count it).
    pub layout_builds: u64,
    /// The Timeline tab's zoom and pan (the width follows the window).
    pub timeline_view: crate::trace_timeline::TimelineView,
    /// The frame number the Timeline tab holds (a click holds the shown frame); `None` follows
    /// the newest.
    pub timeline_frame: Option<u64>,
    /// Consecutive frames the Timeline tab lays out on one strip axis, 1 to
    /// [`MAX_TIMELINE_FRAMES`] (0 reads as 1, the default: one frame, today's view; ADR-7409).
    pub timeline_frames: usize,
    /// The Timeline tab's kept layout (by source, frame and view; ADR-7325).
    pub timeline_layout: crate::thread_timeline::ThreadLayoutCache,
    /// The timeline read for the current repaint: (egui frame number, data).
    pub timeline_data: Option<(u64, TimelineData)>,
    /// The last in-process timeline frame converted: (source kind, frame number, strip length,
    /// strip).
    timeline_converted: Option<(&'static str, u64, usize, Arc<ConvertedStrip>)>,
    /// The play world's tick profile, copied by the editor loop while the Lockstep tab is shown
    /// (ADR-8562; [`copy_lockstep_from_play`]).
    pub lockstep: Option<regia_framework::sim_host::ProfileReport>,
    /// The play world's det debug switch and frame counts, copied with [`Self::lockstep`].
    pub lockstep_draw: Option<LockstepDraw>,
    /// A det debug switch the Lockstep tab set, applied to the play world by the editor loop.
    pub lockstep_pending: Option<regia_framework::det_debug::DebugWhat>,
}

impl ProfilerView {
    /// Consecutive frames the Timeline tab lays out on its strip axis, in 1 to
    /// [`MAX_TIMELINE_FRAMES`] (a zero field reads as 1).
    #[must_use]
    pub fn strip_frames(&self) -> usize {
        self.timeline_frames.clamp(1, MAX_TIMELINE_FRAMES)
    }
}

/// The Profiler panel's tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProfilerTab {
    /// The span call tree and GPU passes.
    #[default]
    Flame,
    /// The frame-time history and hitches.
    History,
    /// ECS entity counts.
    Entities,
    /// Memory per subsystem, owner, GPU allocation and asset type.
    Memory,
    /// Two captures compared.
    Compare,
    /// One frame of the per-thread timeline (ADR-7325).
    Timeline,
    /// The deterministic tick profile per system and the det debug draw switch (ADR-8562).
    Lockstep,
}

impl ProfilerTab {
    /// Every tab, in the window's order.
    pub const ALL: [Self; 7] = [
        Self::Flame,
        Self::Timeline,
        Self::History,
        Self::Entities,
        Self::Memory,
        Self::Compare,
        Self::Lockstep,
    ];

    /// The tab's label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Flame => "Flame",
            Self::History => "History",
            Self::Entities => "Entities",
            Self::Memory => "Memory",
            Self::Compare => "Compare",
            Self::Timeline => "Timeline",
            Self::Lockstep => "Lockstep",
        }
    }
}

impl ProfilerView {
    /// The source shown.
    #[must_use]
    pub fn source(&self) -> &ProfilerSource {
        &self.source
    }

    /// The worker of a process source.
    #[must_use]
    pub fn worker(&self) -> Option<&RemoteWorker> {
        self.worker.as_ref()
    }

    /// The diff of the compared captures, when both are set.
    #[must_use]
    pub fn diff(&self) -> Option<CaptureDiff> {
        let (_, a) = self.compare_a.as_ref()?;
        let (_, b) = self.compare_b.as_ref()?;
        Some(compare(a, b))
    }
}

/// The agent directory of `role` in the running PIE process session, if one runs.
#[must_use]
pub fn process_agent_dir(world: &World, role: ProcessRole) -> Option<PathBuf> {
    world.get_resource::<PieProcesses>()?.agent_dir(role)
}

/// Selects `source`: starts or stops the worker. A process source whose session is not running
/// still starts (it shows `connecting` until the child advertises its endpoint).
///
/// # Errors
/// `profiler_bad_address` for a remote address that is not loopback; `profiler_no_session` for a
/// process source with no PIE process session; `profiler_worker_failed` when the thread cannot start.
pub fn set_source(world: &mut World, source: ProfilerSource) -> Result<(), (&'static str, String)> {
    let target = match &source {
        ProfilerSource::Edit | ProfilerSource::Pie => None,
        ProfilerSource::Remote { address, token } => {
            let url = remote_url(address).ok_or((
                "profiler_bad_address",
                format!("`{address}` is not a loopback ws://127.0.0.1:<port> address"),
            ))?;
            Some(RemoteTarget::Url {
                url,
                token: token.clone(),
            })
        }
        ProfilerSource::Process(role) => Some(RemoteTarget::AgentDir(
            process_agent_dir(world, *role).ok_or((
                "profiler_no_session",
                "no PIE process session runs (Net tab: separate processes, then Play)".to_owned(),
            ))?,
        )),
    };
    let worker = match target {
        Some(target) => {
            Some(RemoteWorker::start(target).map_err(|e| ("profiler_worker_failed", e.to_string()))?)
        }
        None => None,
    };
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    view.source = source;
    view.worker = worker;
    view.frozen = None;
    view.frozen_history = None;
    view.error = None;
    Ok(())
}

/// Freezes frame `index` of `history` (the history the panel shows), or unfreezes with `None`
/// or an index out of range. While a frame is frozen the History tab and `editor.profiler_state`
/// show the history as it was at the freeze: new frames are not taken in, so the frozen index
/// and the hitch rows keep naming the same frames. Unfreezing resumes the live history. Freezing
/// another frame while frozen keeps the frozen history (`history` is then that history).
pub fn freeze_history(world: &mut World, index: Option<usize>, history: &[FrameSample]) {
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    match index.filter(|i| *i < history.len()) {
        Some(i) => {
            if view.frozen_history.is_none() {
                view.frozen_history = Some(history.to_vec());
            }
            view.frozen = Some(i);
        }
        None => {
            view.frozen = None;
            view.frozen_history = None;
        }
    }
}

/// Follows a restarted PIE process session: a process source whose child's agent directory moved
/// gets a new worker. Called by the panel and `editor.profiler_state`.
pub fn sync(world: &mut World) {
    let Some(view) = world.get_resource::<ProfilerView>() else {
        return;
    };
    let ProfilerSource::Process(role) = view.source else {
        return;
    };
    let current = view.worker.as_ref().map(|w| w.target.clone());
    if let Some(dir) = process_agent_dir(world, role)
        && current != Some(RemoteTarget::AgentDir(dir.clone()))
        && let Ok(worker) = RemoteWorker::start(RemoteTarget::AgentDir(dir))
        && let Some(mut view) = world.get_resource_mut::<ProfilerView>()
    {
        view.worker = Some(worker);
    }
}

/// What the panel shows for the current source this frame.
#[derive(Clone, Debug, Default)]
pub struct SourceData {
    /// `live` (in-process), or a process link state.
    pub status: String,
    /// The link error, when disconnected.
    pub message: Option<String>,
    /// The call tree.
    pub capture: Option<ProfileCapture>,
    /// The costliest spans: `(name, ms)`.
    pub top: Vec<(String, f64)>,
    /// The frame history, oldest first.
    pub history: Vec<FrameSample>,
    /// Entity counts.
    pub entities: Option<EntityCounts>,
    /// The `memory.report` of the source (the process's own, or this editor's).
    pub memory: Option<Value>,
}

/// The top spans of a capture by inclusive time: `(name, ms)`.
fn capture_top(capture: &ProfileCapture) -> Vec<(String, f64)> {
    let names = profiler_model::top_span_names(capture, TOP_SPANS);
    names
        .into_iter()
        .map(|name| {
            let ms = max_inclusive(&capture.cpu, &name);
            (name, ms)
        })
        .collect()
}

fn max_inclusive(nodes: &[regia::telemetry::profile_capture::CaptureNode], name: &str) -> f64 {
    nodes
        .iter()
        .map(|n| {
            let own = if n.name == name { n.inclusive_ms } else { 0.0 };
            own.max(max_inclusive(&n.children, name))
        })
        .fold(0.0, f64::max)
}

/// Reads the current source. `memory` asks for the Memory view's report (for the in-process
/// sources a `memory.report` of this process, reused for `memory_every` editor frames).
#[must_use]
pub fn read_source(world: &mut World, frame: u64, memory: bool, memory_every: u64) -> SourceData {
    let _span = tracing::info_span!("profiler_read_source").entered();
    let mut data = read_live_source(world, frame, memory, memory_every);
    // A frozen frame stops the history's intake (`freeze_history`).
    if let Some(frozen) = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.frozen_history.clone())
    {
        data.history = frozen;
    }
    data
}

/// [`read_source`] without the frozen history.
fn read_live_source(world: &mut World, frame: u64, memory: bool, memory_every: u64) -> SourceData {
    let source = world
        .get_resource::<ProfilerView>()
        .map(|v| v.source.clone())
        .unwrap_or_default();
    match source {
        ProfilerSource::Edit | ProfilerSource::Pie => {
            let play = source == ProfilerSource::Pie;
            let profiler = world.get_resource::<regia::Profiler>().map(|p| p.0.clone());
            let whole = crate::profiler_panel::current_capture(world);
            let play_names = whole
                .as_ref()
                .map(|c| profiler_model::capture_names(&profiler_model::split_source(c, true)))
                .unwrap_or_default();
            let capture = whole.map(|c| profiler_model::split_source(&c, play));
            let history = profiler
                .as_ref()
                .map(|p| profiler_model::local_history(p, play, &play_names))
                .unwrap_or_default();
            let entities = if play {
                world
                    .get_resource::<ProfilerView>()
                    .and_then(|v| v.pie_counts.clone())
            } else {
                Some(profiler_model::entity_counts(world, TOP_COMPONENTS))
            };
            let memory = if memory {
                local_memory(world, frame, memory_every)
            } else {
                None
            };
            SourceData {
                status: if play && entities.is_none() {
                    "no_session".to_owned()
                } else {
                    "live".to_owned()
                },
                message: None,
                top: capture.as_ref().map(capture_top).unwrap_or_default(),
                capture,
                history,
                entities,
                memory,
            }
        }
        ProfilerSource::Process(_) | ProfilerSource::Remote { .. } => {
            sync(world);
            let snapshot = world
                .get_resource::<ProfilerView>()
                .and_then(|v| v.worker.as_ref().map(RemoteWorker::snapshot))
                .unwrap_or_default();
            // The ranked counts of `profiler.frame { entities }`; an older build's totals from
            // `memory.report` otherwise.
            let entities = snapshot.entities.clone().or_else(|| {
                snapshot.memory.as_ref().map(|m| EntityCounts {
                    entities: m["ecs"]["entities"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or(0),
                    archetypes: m["ecs"]["archetypes"].as_u64().unwrap_or(0) as usize,
                    components: Vec::new(),
                })
            });
            SourceData {
                status: snapshot.link.as_str().to_owned(),
                message: match &snapshot.link {
                    LinkState::Disconnected(e) => Some(e.clone()),
                    _ => None,
                },
                capture: snapshot.capture,
                top: snapshot.top,
                history: snapshot.history,
                entities,
                memory: snapshot.memory,
            }
        }
    }
}

/// Most frames the Timeline tab lays out on one strip axis (ADR-7409).
pub const MAX_TIMELINE_FRAMES: usize = 8;

/// What the Timeline tab shows for the current source.
#[derive(Clone, Debug, Default)]
pub struct TimelineData {
    /// `live` (in-process), `timeline_off`, `no_profiler`, `no_frame`, or a process link state.
    pub status: String,
    /// The reason, when there is no frame.
    pub message: Option<String>,
    /// The frame shown (the strip's combined frame when `frames` is set).
    pub frame: Option<Arc<CaptureTimelineFrame>>,
    /// The strip's frames, oldest first (one entry for a single frame view): each with its
    /// number and start offset on the axis ([`TimelineData::frame`]'s time base).
    pub frames: Vec<CaptureStripFrame>,
    /// The source's kind (`edit`, `pie`, `process`, `remote`): part of the layout cache key.
    pub source: &'static str,
}

/// Reads the Timeline tab's frame for the current source (the held frame, or the newest). For a
/// process source this also asks its worker to poll `profiler.timeline`.
pub fn read_timeline(world: &mut World) -> TimelineData {
    let frames = world
        .get_resource::<ProfilerView>()
        .map_or(1, ProfilerView::strip_frames);
    read_timeline_with(world, frames)
}

/// The in-process source's strip, cached in [`ProfilerView::timeline_converted`].
#[derive(Debug)]
struct ConvertedStrip {
    frame: Arc<CaptureTimelineFrame>,
    frames: Vec<CaptureStripFrame>,
}

/// Reads the Timeline tab's frame for the current source with `frames` consecutive retained
/// frames (ending at the held or newest frame) laid out on one strip axis (ADR-7409). `frames`
/// 1 reads exactly one frame, as before.
pub fn read_timeline_with(world: &mut World, frames: usize) -> TimelineData {
    let _span = tracing::info_span!("profiler_read_timeline").entered();
    let n = frames.clamp(1, MAX_TIMELINE_FRAMES);
    let (source, held) = world
        .get_resource::<ProfilerView>()
        .map(|v| (v.source.clone(), v.timeline_frame))
        .unwrap_or_default();
    let kind = source.kind();
    match source {
        ProfilerSource::Edit | ProfilerSource::Pie => {
            let Some(profiler) = world.get_resource::<regia::Profiler>().map(|p| p.0.clone()) else {
                return TimelineData {
                    status: "no_profiler".to_owned(),
                    message: Some("no frame profiler is installed".to_owned()),
                    frame: None,
                    frames: Vec::new(),
                    source: kind,
                };
            };
            if !profiler.timeline_enabled() {
                return TimelineData {
                    status: "timeline_off".to_owned(),
                    message: Some("Record timeline is off".to_owned()),
                    frame: None,
                    frames: Vec::new(),
                    source: kind,
                };
            }
            let number = held.or_else(|| profiler.newest_frame_number());
            let cached = world
                .get_resource::<ProfilerView>()
                .and_then(|v| v.timeline_converted.clone())
                .filter(|(k, num, len, _)| *k == kind && Some(*num) == number && *len == n)
                .map(|(_, _, _, strip)| strip);
            let converted = cached.or_else(|| {
                // The strip ends at the held (or newest) frame; fewer when the history is short.
                let numbers = profiler.frame_numbers();
                let end = match number {
                    Some(wanted) => numbers.iter().position(|&k| k == wanted)?,
                    None => return None,
                };
                let first = end.saturating_sub(n - 1);
                let pieces: Vec<CaptureTimelineFrame> = numbers[first..=end]
                    .iter()
                    .filter_map(|num| profiler.timeline_frame(Some(*num)))
                    .map(|frame| {
                        let whole = CaptureTimelineFrame::from_frame(&frame);
                        crate::thread_timeline::split_timeline(&whole, kind == "pie")
                    })
                    .collect();
                let strip = if n == 1 {
                    // One frame reads exactly as before: its own wall time, no axis shift.
                    let frame = pieces.into_iter().next()?;
                    let meta = CaptureStripFrame {
                        number: frame.number,
                        frame_ns: frame.frame_ns,
                        start_ns: 0,
                        span_ns: frame.span_ns(),
                        dropped: frame.dropped,
                    };
                    ConvertedStrip {
                        frame: Arc::new(frame),
                        frames: vec![meta],
                    }
                } else {
                    let strip = regia::telemetry::profile_capture::timeline_strip(&pieces);
                    ConvertedStrip {
                        frame: Arc::new(strip.combined),
                        frames: strip.frames,
                    }
                };
                let strip = Arc::new(strip);
                if let Some(mut view) = world.get_resource_mut::<ProfilerView>() {
                    view.timeline_converted = Some((kind, numbers[end], n, Arc::clone(&strip)));
                }
                Some(strip)
            });
            match converted {
                Some(strip) => TimelineData {
                    status: "live".to_owned(),
                    message: None,
                    frame: Some(Arc::clone(&strip.frame)),
                    frames: strip.frames.clone(),
                    source: kind,
                },
                None => TimelineData {
                    status: "no_frame".to_owned(),
                    message: Some(match held {
                        Some(n) => format!("frame {n} is not in the history (press Live)"),
                        None => "no frame recorded yet".to_owned(),
                    }),
                    frame: None,
                    frames: Vec::new(),
                    source: kind,
                },
            }
        }
        ProfilerSource::Process(_) | ProfilerSource::Remote { .. } => {
            sync(world);
            let snapshot = world.get_resource::<ProfilerView>().and_then(|v| {
                v.worker.as_ref().map(|w| {
                    w.set_timeline_request(true, held, n);
                    w.snapshot()
                })
            });
            let snapshot = snapshot.unwrap_or_default();
            let (status, message) = match (&snapshot.link, &snapshot.timeline_error) {
                (LinkState::Disconnected(e), _) => ("disconnected".to_owned(), Some(e.clone())),
                (_, Some(e)) => ("refused".to_owned(), Some(e.clone())),
                (link, None) => (link.as_str().to_owned(), None),
            };
            TimelineData {
                status,
                message,
                frame: snapshot.timeline,
                frames: snapshot.timeline_frames,
                source: kind,
            }
        }
    }
}

/// Stops a process source's worker from polling the timeline (the tab is hidden).
pub fn stop_timeline_polls(world: &World) {
    if let Some(worker) = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.worker.as_ref())
    {
        worker.set_timeline_request(false, None, 1);
    }
}

/// `editor.profiler_timeline { frame?, frames?, zoom?, pan?, width?, max_bars? }`: the Timeline
/// tab's frame for its source, laid out in the given view (default the tab's own), through the
/// tab's layout cache. `frames` (1-8, default the tab's) lays that many consecutive frames on
/// one strip axis and the answer's `frames` lists each with its start offset; `frame` holds the
/// tab on that frame (the strip ends at it), as the tab's click does.
fn profiler_timeline(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let max_bars = bounded_as(params, "max_bars", 200, 100_000, "profiler_bad_max_bars")?;
    let default_frames = world
        .get_resource::<ProfilerView>()
        .map_or(1, ProfilerView::strip_frames);
    let frames = bounded_as(
        params,
        "frames",
        default_frames,
        MAX_TIMELINE_FRAMES,
        "profiler_bad_frames",
    )?;
    let float = |key: &str| params.get(key).and_then(Value::as_f64).map(|v| v as f32);
    // The hold the tab had before this call: a refused `frame` puts it back, so a frame outside
    // the history never latches (review g16 finding 5).
    let previous_hold = match params.get("frame") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let held = v.as_u64().ok_or_else(|| {
                refused(
                    "profiler_bad_frame",
                    "`frame` must be a frame number (an integer from a previous answer)",
                )
            })?;
            let mut view = world.get_resource_or_insert_with(ProfilerView::default);
            Some(view.timeline_frame.replace(held))
        }
    };
    let data = read_timeline_with(world, frames);
    let Some(frame) = data.frame else {
        if let Some(previous) = previous_hold
            && let Some(mut view) = world.get_resource_mut::<ProfilerView>()
        {
            view.timeline_frame = previous;
        }
        return Err(refused(
            "profiler_no_timeline",
            format!(
                "{}: {}",
                data.status,
                data.message.unwrap_or_else(|| "no timeline frame yet".to_owned())
            ),
        ));
    };
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    let current = view.timeline_view;
    let layout_view = crate::trace_timeline::TimelineView {
        zoom: float("zoom").unwrap_or(current.zoom),
        pan: float("pan").unwrap_or(current.pan),
        width: float("width").unwrap_or(current.width),
    }
    .clamped();
    let layout =
        crate::thread_timeline::cached_layout(&mut view.timeline_layout, data.source, &frame, &layout_view);
    let mut out = crate::thread_timeline::layout_json(&frame, &layout, max_bars);
    if let Some(map) = out.as_object_mut() {
        map.insert("status".into(), json!(data.status));
        map.insert("held".into(), json!(view.timeline_frame));
        map.insert("frames".into(), json!(data.frames));
        map.insert(
            "layout_cache".into(),
            json!({ "builds": view.timeline_layout.builds(), "hits": view.timeline_layout.hits() }),
        );
    }
    Ok(out)
}

/// `editor.profiler_freeze { frame? }`: the History tab's freeze click as a method. `frame`
/// (an integer index into the shown history, as the History tab's bars and `editor.profiler_state`'s
/// hitches number them) freezes that frame and stops the history's intake ([`freeze_history`]);
/// an absent `frame` unfreezes and resumes it. Out of range is `profiler_bad_frame`.
fn profiler_freeze(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let shown = read_source(world, u64::MAX, false, 0).history;
    let history = shown.len();
    let frame = match params.get("frame") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            refused(
                "profiler_bad_frame",
                format!(
                    "`frame` must be an integer index into the shown history (0 to {})",
                    history.saturating_sub(1)
                ),
            )
        })?),
    };
    if let Some(n) = frame
        && !usize::try_from(n).is_ok_and(|i| i < history)
    {
        return Err(refused(
            "profiler_bad_frame",
            format!(
                "frame {n} is not in the shown history (0 to {})",
                history.saturating_sub(1)
            ),
        ));
    }
    freeze_history(world, frame.and_then(|n| usize::try_from(n).ok()), &shown);
    let frozen = world.get_resource::<ProfilerView>().and_then(|v| v.frozen);
    Ok(json!({
        "frozen": frozen,
        "history": { "frames": history },
    }))
}

/// This editor's `memory.report`, read again at most every `every` frames.
fn local_memory(world: &mut World, frame: u64, every: u64) -> Option<Value> {
    if let Some((at, report)) = world
        .get_resource::<ProfilerView>()
        .and_then(|v| v.memory_cache.clone())
        && frame.saturating_sub(at) < every
    {
        return Some(report);
    }
    let report = regia_agent::memory_report(world, &json!({ "top": 10 })).ok()?;
    if let Some(mut view) = world.get_resource_mut::<ProfilerView>() {
        view.memory_cache = Some((frame, report.clone()));
    }
    Some(report)
}

/// The JSON `editor.profiler_state` returns for `data`.
#[must_use]
pub fn state_json(view: Option<&ProfilerView>, data: &SourceData, top: usize) -> Value {
    let source = view.map(|v| v.source.clone()).unwrap_or_default();
    let threshold = hitch_threshold_ms(&data.history);
    let spans = |s: &[(String, f64)]| -> Vec<Value> {
        s.iter()
            .map(|(name, ms)| json!({ "name": name, "ms": ms }))
            .collect()
    };
    let hitch_rows: Vec<Value> = hitches(&data.history, threshold)
        .into_iter()
        .map(|i| json!({ "index": i, "ms": data.history[i].ms, "spans": spans(&data.history[i].spans) }))
        .collect();
    json!({
        "source": {
            "kind": source.kind(),
            "label": source.label(),
            "address": match &source { ProfilerSource::Remote { address, .. } => Some(address.clone()), _ => None },
            "process": match &source { ProfilerSource::Process(role) => Some(role_name(*role)), _ => None },
        },
        "status": data.status,
        "message": data.message,
        "frame_ms": data.capture.as_ref().map(|c| c.frame_ms),
        "frames": data.capture.as_ref().map(|c| c.frames),
        "top": spans(&data.top.iter().take(top).cloned().collect::<Vec<_>>()),
        "cpu": data.capture.as_ref().map(|c| &c.cpu),
        "history": {
            "frames": data.history.len(),
            "median_ms": profiler_model::median_ms(&data.history),
            "hitch_threshold_ms": threshold,
            "hitches": hitch_rows,
        },
        "frozen": view.and_then(|v| v.frozen),
        "entities": data.entities.as_ref().map(|e| json!({
            "entities": e.entities,
            "archetypes": e.archetypes,
            "components": e.components.iter().map(|(name, n)| json!({ "name": name, "entities": n })).collect::<Vec<_>>(),
        })),
        "memory": data.memory.as_ref().map(|m| json!({
            "subsystems": m["subsystems"],
            "owners": m["owners"],
            "top_gpu": m["top_gpu"],
            "assets_by_type": m["assets_by_type"],
            "heap": m["heap"],
            "gpu": m["gpu"],
        })),
        "opened": view.and_then(|v| v.opened.as_ref().map(|(p, _)| p.clone())),
        "compare": view.map(|v| json!({
            "a": v.compare_a.as_ref().map(|(p, _)| p.clone()),
            "b": v.compare_b.as_ref().map(|(p, _)| p.clone()),
        })),
    })
}

fn refused(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/profiler.md")
}

fn bounded(params: &Value, key: &str, default: usize, max: usize) -> Result<usize, RpcError> {
    bounded_as(params, key, default, max, "profiler_bad_top")
}

fn bounded_as(
    params: &Value,
    key: &str,
    default: usize,
    max: usize,
    code: &'static str,
) -> Result<usize, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(|| refused(code, format!("`{key}` must be an integer in 1..={max}"))),
    }
}

/// `editor.profiler_state { top? }`: what the Profiler panel shows for its source.
fn profiler_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = bounded(params, "top", TOP_SPANS, 200)?;
    let data = read_source(world, u64::MAX, true, 0);
    Ok(state_json(world.get_resource::<ProfilerView>(), &data, top))
}

/// Parses `editor.profiler_source`'s parameters into a source.
///
/// # Errors
/// `profiler_bad_kind`, `profiler_bad_process`, `profiler_bad_address`, `profiler_no_token`.
pub fn source_from_params(params: &Value) -> Result<ProfilerSource, RpcError> {
    let kind = params.get("kind").and_then(Value::as_str).unwrap_or_default();
    let address = params.get("address").and_then(Value::as_str);
    match kind {
        "edit" => Ok(ProfilerSource::Edit),
        "pie" => Ok(ProfilerSource::Pie),
        "process" => address
            .and_then(parse_role)
            .map(ProfilerSource::Process)
            .ok_or_else(|| {
                refused(
                    "profiler_bad_process",
                    "`address` must be `server` or `client <k>` for kind process",
                )
            }),
        "remote" => {
            let address = address.filter(|a| remote_url(a).is_some()).ok_or_else(|| {
                refused(
                    "profiler_bad_address",
                    "`address` must be ws://127.0.0.1:<port> (or 127.0.0.1:<port>)",
                )
            })?;
            let token = params
                .get("token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    refused(
                        "profiler_no_token",
                        "`token` is required for kind remote (the app's .regia/agent/token)",
                    )
                })?;
            Ok(ProfilerSource::Remote {
                address: address.to_owned(),
                token: token.to_owned(),
            })
        }
        _ => Err(refused(
            "profiler_bad_kind",
            "`kind` must be edit, pie, process or remote",
        )),
    }
}

/// `editor.profiler_source { kind, address?, token? }`.
fn profiler_source(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = source_from_params(params)?;
    set_source(world, source).map_err(|(code, message)| refused(code, message))?;
    profiler_state(world, &json!({}))
}

/// Reads a `.rprof.json` under the project (or an absolute path the user picked in the panel).
///
/// # Errors
/// The file cannot be read or is not a capture.
pub fn read_capture_file(world: &World, path: &str) -> Result<ProfileCapture, String> {
    let file = PathBuf::from(path);
    let file = if file.is_absolute() {
        file
    } else {
        let root = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|p| p.root.clone())
            .ok_or("no project is open")?;
        root.join(file)
    };
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    ProfileCapture::from_json(&text).map_err(|e| format!("{}: {e}", file.display()))
}

/// `editor.profiler_capture_slot { slot, path? }`: opens a capture into the view (`view`) or a
/// compare side (`a`, `b`); no `path` clears the slot.
fn profiler_capture_slot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let slot = params.get("slot").and_then(Value::as_str).unwrap_or_default();
    if !matches!(slot, "view" | "a" | "b") {
        return Err(refused("profiler_bad_slot", "`slot` must be view, a or b"));
    }
    let loaded = match params.get("path").and_then(Value::as_str) {
        Some(path) => Some((
            path.to_owned(),
            read_capture_file(world, path).map_err(|e| refused("profiler_bad_capture", e))?,
        )),
        None => None,
    };
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    match slot {
        "view" => view.opened = loaded,
        "a" => view.compare_a = loaded,
        _ => view.compare_b = loaded,
    }
    let diff = view.diff();
    Ok(json!({
        "view": view.opened.as_ref().map(|(p, _)| p.clone()),
        "a": view.compare_a.as_ref().map(|(p, _)| p.clone()),
        "b": view.compare_b.as_ref().map(|(p, _)| p.clone()),
        "diff": diff,
    }))
}

/// The Profiler panel's agent methods (ADR-9740).
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.profiler_state",
                tool: "profiler_state",
                description: "Read what the editor Profiler panel shows for its source (edit world, play, a PIE process or a dev build): the frame time, the top spans, the call tree, the frame history with its hitches and their top spans, entity counts (total, archetypes, top components), and memory per subsystem, owner, GPU allocation and asset type.",
                params: &[ParamSchema {
                    name: "top",
                    kind: "integer",
                    required: false,
                    description: "Spans in the top list, 1-200 (default 10).",
                }],
                result: "{ source: { kind, label, address, process }, status, message, frame_ms, frames, top: [{ name, ms }], cpu, history: { frames, median_ms, hitch_threshold_ms, hitches: [{ index, ms, spans }] }, frozen, entities: { entities, archetypes, components: [{ name, entities }] } | null, memory | null, opened, compare: { a, b } }",
                mutating: false,
                permission: Permission::Read,
            },
            profiler_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_source",
                tool: "profiler_source",
                description: "Choose what the editor Profiler panel profiles: edit (the editor world), pie (the in-process play world), process (a separate-process PIE child, address server or client <k>) or remote (a running dev build at a loopback ws:// address with its agent token). Process sources are read over the agent protocol on a worker thread; a dropped connection shows as disconnected and is retried.",
                params: &[
                    ParamSchema {
                        name: "kind",
                        kind: "string",
                        required: true,
                        description: "edit, pie, process or remote.",
                    },
                    ParamSchema {
                        name: "address",
                        kind: "string",
                        required: false,
                        description: "process: server or client <k>; remote: ws://127.0.0.1:<port>.",
                    },
                    ParamSchema {
                        name: "token",
                        kind: "string",
                        required: false,
                        description: "remote: the app's agent token (.regia/agent/token).",
                    },
                ],
                result: "the editor.profiler_state answer for the new source",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_source as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_capture_slot",
                tool: "profiler_capture_slot",
                description: "Open a .rprof.json capture in the editor Profiler panel: slot view shows it instead of the live tree, slots a and b are the two sides of Compare (the flame view is then colored by the per-span delta). No path clears the slot.",
                params: &[
                    ParamSchema {
                        name: "slot",
                        kind: "string",
                        required: true,
                        description: "view, a or b.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Project-relative (or absolute) .rprof.json; absent clears the slot.",
                    },
                ],
                result: "{ view, a, b, diff | null }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_capture_slot as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_timeline",
                tool: "profiler_timeline",
                description: "Read the editor Profiler panel's Timeline tab for its source (edit world, play, a PIE process or a dev build): the per-thread timeline (the held frame or the newest) laid out in a view, one block of rows per thread and one row per nesting depth; sub-pixel spans merge into one bar carrying their count. frames (1-8, default the tab's) lays that many consecutive frames on one strip axis: the answer's frames list each with its start offset and the boundary the panel draws. frame holds the tab on a frame (the strip ends at it). Answers profiler_no_timeline while the source's timeline is off or has no frame.",
                params: &[
                    ParamSchema {
                        name: "frame",
                        kind: "integer",
                        required: false,
                        description: "Frame number to hold (from a previous answer); absent keeps the tab's held frame.",
                    },
                    ParamSchema {
                        name: "frames",
                        kind: "integer",
                        required: false,
                        description: "Consecutive frames on one strip axis, 1-8 (default the tab's, initially 1).",
                    },
                    ParamSchema {
                        name: "zoom",
                        kind: "number",
                        required: false,
                        description: "1 to 10000 (default the tab's).",
                    },
                    ParamSchema {
                        name: "pan",
                        kind: "number",
                        required: false,
                        description: "0 (start) to 1 (end) of the scrollable rest (default the tab's).",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "View width, points (default the tab's).",
                    },
                    ParamSchema {
                        name: "max_bars",
                        kind: "integer",
                        required: false,
                        description: "Most bars listed, 1-100000 (default 200); rects counts them all.",
                    },
                ],
                result: "{ status, held, frame, frame_ms, dropped, spans, visible_spans, window_ns, frames: [{ number, frame_ns, start_ns, span_ns, dropped }], threads: [{ name, first_row, rows }], rects, merged_rects, bars: [{ label, thread, row, depth, x, width, start_ns, end_ns, count }], layout_cache: { builds, hits } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_timeline as Handler,
        ),
        (
            MethodSchema {
                name: "editor.profiler_freeze",
                tool: "profiler_freeze",
                description: "Freeze one frame of the editor Profiler panel's History tab (the freeze click as a method): the panel highlights the bar, shows that frame's spans and editor.profiler_state reports it as frozen. frame is an index into the shown history (the History tab's bars and hitch rows number them); an absent frame unfreezes. Out of range answers profiler_bad_frame.",
                params: &[ParamSchema {
                    name: "frame",
                    kind: "integer",
                    required: false,
                    description: "History frame index to freeze; absent unfreezes.",
                }],
                result: "{ frozen, history: { frames } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            profiler_freeze as Handler,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Lockstep (ADR-8562)
// ---------------------------------------------------------------------------

/// The det debug switch of a world and its latest frame's primitive count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LockstepDraw {
    /// What is drawn.
    pub what: regia_framework::det_debug::DebugWhat,
    /// Primitives in the latest frame.
    pub primitives: usize,
    /// Tick of the latest frame.
    pub tick: Option<u64>,
}

/// The tick profile of `world`: its sim host's report, else its `DetTickProfile` resource.
#[must_use]
pub fn world_tick_profile(world: &World) -> Option<regia_framework::sim_host::ProfileReport> {
    if let Some(report) = world
        .get_resource::<regia_framework::sim_host::SimHostHandle>()
        .and_then(regia_framework::sim_host::SimHostHandle::tick_profile)
    {
        return Some(report);
    }
    world
        .get_resource::<regia_framework::sim_host::DetTickProfile>()
        .map(|stored| stored.report.clone())
}

/// The det debug switch of `world`, when it has a `DetDebugDraw` resource or a sim host whose sim
/// draws.
#[must_use]
pub fn world_det_debug(world: &World) -> Option<LockstepDraw> {
    use regia_framework::det_debug::DetDebugDraw;
    use regia_framework::sim_host::SimHostHandle;
    let host = world
        .get_resource::<SimHostHandle>()
        .filter(|handle| handle.det_debug_supported());
    if let Some(host) = host {
        let frame = host.det_debug_frame();
        return Some(LockstepDraw {
            what: host.det_debug(),
            primitives: frame.as_ref().map_or(0, |(_, f)| f.len()),
            tick: frame.map(|(tick, _)| tick),
        });
    }
    world.get_resource::<DetDebugDraw>().map(|draw| LockstepDraw {
        what: draw.what,
        primitives: draw.frame.len(),
        tick: draw.tick,
    })
}

/// Set what `world` draws (its sim host and its `DetDebugDraw`, the way `sim.det_debug_draw`
/// does). Returns false when the world has neither.
pub fn set_world_det_debug(world: &mut World, what: regia_framework::det_debug::DebugWhat) -> bool {
    use regia_framework::det_debug::DetDebugDraw;
    use regia_framework::sim_host::SimHostHandle;
    let host = world
        .get_resource::<SimHostHandle>()
        .filter(|handle| handle.det_debug_supported())
        .cloned();
    if host.is_none() && world.get_resource::<DetDebugDraw>().is_none() {
        return false;
    }
    if let Some(host) = host {
        host.set_det_debug(what);
    }
    world.get_resource_or_insert_with(DetDebugDraw::default).what = what;
    true
}

/// The report and switch the Lockstep tab shows: the edit world's own (a game running its sim in
/// the editor process), else the copy [`copy_lockstep_from_play`] left.
#[must_use]
pub fn lockstep_source(
    world: &World,
) -> (
    Option<regia_framework::sim_host::ProfileReport>,
    Option<LockstepDraw>,
) {
    let view = world.get_resource::<ProfilerView>();
    let report = world_tick_profile(world).or_else(|| view.and_then(|v| v.lockstep.clone()));
    let draw = world_det_debug(world).or_else(|| view.and_then(|v| v.lockstep_draw));
    (report, draw)
}

/// Set the det debug switch from the Lockstep tab: on the edit world when it draws, else queued
/// for the play world ([`copy_lockstep_from_play`] applies it).
pub fn request_det_debug(world: &mut World, what: regia_framework::det_debug::DebugWhat) {
    if set_world_det_debug(world, what) {
        return;
    }
    let mut view = world.get_resource_or_insert_with(ProfilerView::default);
    view.lockstep_pending = Some(what);
    if let Some(draw) = view.lockstep_draw.as_mut() {
        draw.what = what;
    }
}

/// The editor loop's Lockstep hook: while the Profiler shows the Lockstep tab, apply a pending
/// switch to the play world and copy its tick profile and switch into [`ProfilerView`]. Reads
/// only; nothing is copied while the tab is hidden.
pub fn copy_lockstep_from_play(edit: &mut World, play: Option<&mut World>) {
    let shown = edit
        .get_resource::<crate::profiler_panel::ProfilerPanel>()
        .is_some_and(|p| p.open)
        && edit
            .get_resource::<ProfilerView>()
            .is_some_and(|v| v.tab == ProfilerTab::Lockstep);
    if !shown {
        return;
    }
    let Some(play) = play else {
        if let Some(mut view) = edit.get_resource_mut::<ProfilerView>() {
            view.lockstep = None;
            view.lockstep_draw = None;
        }
        return;
    };
    let pending = edit
        .get_resource_mut::<ProfilerView>()
        .and_then(|mut v| v.lockstep_pending.take());
    if let Some(what) = pending {
        set_world_det_debug(play, what);
    }
    let report = world_tick_profile(play);
    let draw = world_det_debug(play);
    if let Some(mut view) = edit.get_resource_mut::<ProfilerView>() {
        view.lockstep = report;
        view.lockstep_draw = draw;
    }
}

/// Test hooks shared with the panel tests of this crate.
#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;

    /// Poisons `worker`'s snapshot lock the way a worker that panicked mid-store would: a helper
    /// thread panics while it holds the guard, leaving `polls` at `polls`. Every read then answers
    /// `LinkState::Disconnected` with [`WORKER_PANICKED`] (ADR-7427).
    pub(crate) fn poison(worker: &RemoteWorker, polls: u64) {
        let shared = Arc::clone(&worker.shared);
        let panicked = std::thread::Builder::new()
            .name("poison-helper".into())
            .spawn(move || {
                let mut guard = shared.snapshot.lock().unwrap_or_else(|e| e.into_inner());
                guard.polls = polls;
                panic!("worker panicked mid-store (test)");
            })
            .expect("helper")
            .join()
            .is_err();
        assert!(panicked && worker.shared.snapshot.is_poisoned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_addresses_parse() {
        assert_eq!(parse_role("server"), Some(ProcessRole::Server));
        for text in ["client 2", "client:2", "client2"] {
            assert_eq!(parse_role(text), Some(ProcessRole::Client(2)), "{text}");
        }
        assert_eq!(parse_role("clientx"), None);
        assert_eq!(role_name(ProcessRole::Client(1)), "client 1");
        assert_eq!(
            remote_url("127.0.0.1:4100").as_deref(),
            Some("ws://127.0.0.1:4100")
        );
        assert_eq!(
            remote_url("ws://127.0.0.1:4100").as_deref(),
            Some("ws://127.0.0.1:4100")
        );
        assert_eq!(remote_url("ws://10.0.0.2:4100"), None, "never off the machine");
        assert_eq!(remote_url("localhost:4100"), None);
    }

    fn code(r: Result<ProfilerSource, RpcError>) -> String {
        r.expect_err("refused")
            .data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// rev-zai-03 finding 1: a refused `profiler.set_capture {timeline}` (a build older than
    /// ADR-7325 answers `bad_flame`) becomes the Timeline tab's error text, the same text a
    /// refused `profiler.timeline` gives, instead of a link error that drops the client.
    #[test]
    fn a_refused_timeline_switch_reads_as_the_tabs_error() {
        assert_eq!(
            refusal_text(
                Some(&json!({ "code": "bad_flame", "reason": "bad_flame" })),
                "flame is required"
            ),
            "bad_flame: flame is required"
        );
        assert_eq!(refusal_text(None, "no"), "error: no");
    }

    #[test]
    fn source_params_are_checked() {
        assert_eq!(
            source_from_params(&json!({ "kind": "edit" })).ok(),
            Some(ProfilerSource::Edit)
        );
        assert_eq!(
            source_from_params(&json!({ "kind": "process", "address": "client 0" })).ok(),
            Some(ProfilerSource::Process(ProcessRole::Client(0)))
        );
        assert_eq!(
            code(source_from_params(&json!({ "kind": "tv" }))),
            "profiler_bad_kind"
        );
        assert_eq!(
            code(source_from_params(&json!({ "kind": "process" }))),
            "profiler_bad_process"
        );
        assert_eq!(
            code(source_from_params(
                &json!({ "kind": "remote", "address": "8.8.8.8:1", "token": "t" })
            )),
            "profiler_bad_address"
        );
        assert_eq!(
            code(source_from_params(
                &json!({ "kind": "remote", "address": "127.0.0.1:1" })
            )),
            "profiler_no_token"
        );
    }

    /// A dropped (here: never answering) address shows as disconnected with the error, and the
    /// editor side never waits: the snapshot is read without touching the network.
    #[test]
    fn a_dead_address_shows_disconnected_and_never_blocks_the_reader() {
        // A port that was bound and closed again: nothing listens there.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .expect("port");
        let worker = RemoteWorker::start(RemoteTarget::Url {
            url: format!("ws://127.0.0.1:{port}"),
            token: "t".into(),
        })
        .expect("worker");
        assert_eq!(worker.snapshot().polls, 0, "the reader does not wait for a poll");
        let mut seen = LinkState::Connecting;
        for _ in 0..50 {
            let s = worker.wait_for_polls(1, Duration::from_millis(100));
            if matches!(s.link, LinkState::Disconnected(_)) {
                seen = s.link;
                break;
            }
        }
        assert!(matches!(seen, LinkState::Disconnected(_)), "{seen:?}");
        drop(worker);
    }

    /// Poisons a worker's snapshot lock the way a worker that panicked mid-store would (shared
    /// with the panel tests through [`test_hooks::poison`]).
    fn poison(worker: &RemoteWorker) {
        test_hooks::poison(worker, 7);
    }

    /// A target that never answers: an agent directory with no endpoint (no network).
    fn idle_target(dir: &regia_test_utils::TempDir) -> RemoteTarget {
        RemoteTarget::AgentDir(dir.join("none"))
    }

    /// ADR-7427 (3): a poisoned snapshot lock (the worker panicked while storing a poll) reads
    /// as disconnected with the reason, in the snapshot, the panel's source data and
    /// `editor.profiler_state`, instead of a default `connecting` snapshot forever.
    #[test]
    fn a_poisoned_snapshot_reads_disconnected_with_the_reason() {
        let dir = regia_test_utils::TempDir::new("profiler-poison");
        let worker = RemoteWorker::start(idle_target(&dir)).expect("worker");
        poison(&worker);
        let snapshot = worker.snapshot();
        match &snapshot.link {
            LinkState::Disconnected(reason) => {
                assert!(reason.contains(WORKER_PANICKED), "{reason}");
            }
            other => panic!("a poisoned snapshot read {other:?}"),
        }
        let waited = worker.wait_for_polls(u64::MAX, Duration::ZERO);
        assert_eq!(waited.link, snapshot.link, "wait_for_polls reads the same");

        let mut world = World::new();
        world.insert_resource(ProfilerView {
            source: ProfilerSource::Remote {
                address: "127.0.0.1:1".into(),
                token: "t".into(),
            },
            worker: Some(worker),
            ..ProfilerView::default()
        });
        let data = read_source(&mut world, 1, false, 30);
        assert_eq!(data.status, "disconnected");
        assert!(
            data.message
                .as_deref()
                .is_some_and(|m| m.contains(WORKER_PANICKED)),
            "{:?}",
            data.message
        );
        let state = handler("editor.profiler_state")(&mut world, &json!({})).expect("state");
        assert_eq!(state["status"], "disconnected", "{state:#}");
        assert!(
            state["message"]
                .as_str()
                .is_some_and(|m| m.contains(WORKER_PANICKED)),
            "{state:#}"
        );
        let timeline = read_timeline(&mut world);
        assert_eq!(timeline.status, "disconnected");
    }

    /// ADR-7427 (3): a healthy worker's snapshot is unchanged: its own link state, not the
    /// poison reason.
    #[test]
    fn a_healthy_snapshot_is_unchanged() {
        let dir = regia_test_utils::TempDir::new("profiler-healthy");
        let worker = RemoteWorker::start(idle_target(&dir)).expect("worker");
        let first = worker.snapshot();
        assert!(!worker.shared.snapshot.is_poisoned());
        assert!(
            matches!(&first.link, LinkState::Connecting | LinkState::Disconnected(_)),
            "{:?}",
            first.link
        );
        if let LinkState::Disconnected(reason) = &first.link {
            assert!(!reason.contains(WORKER_PANICKED), "{reason}");
        }
        let polled = worker.wait_for_polls(1, Duration::from_millis(200));
        if let LinkState::Disconnected(reason) = &polled.link {
            assert!(reason.contains("no endpoint"), "{reason}");
        }
    }

    #[test]
    fn frame_answers_convert_to_a_capture_and_a_top_list() {
        let frame = json!({
            "frames": 60, "frame_ms": 4.5, "truncated": 0, "gpu": [],
            "cpu": [{ "name": "tick", "inclusive_ms": 4.0, "self_ms": 1.0, "calls": 1.0,
                      "children": [{ "name": "work", "inclusive_ms": 3.0, "self_ms": 3.0, "calls": 1.0 }] }],
            "top": [{ "name": "tick", "ms": 4.0 }, { "name": "work", "ms": 3.0 }],
        });
        let capture = capture_from_frame(&frame).expect("capture");
        assert_eq!(capture.node_count(), 2);
        assert_eq!(
            top_from_frame(&frame),
            [("tick".to_owned(), 4.0), ("work".to_owned(), 3.0)]
        );
        assert!(capture_from_frame(&json!({})).is_none());
    }

    fn handler(name: &str) -> Handler {
        methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .map(|(_, h)| h)
            .unwrap_or_else(|| panic!("{name} registered"))
    }

    /// `editor.profiler_capture_slot` opens two captures as Compare's sides and answers their
    /// diff; `editor.profiler_source` and `editor.profiler_state` answer for the edit world.
    #[test]
    fn capture_slots_compare_and_the_state_answers() {
        use regia::telemetry::profile_capture::CaptureNode;
        let root = regia_test_utils::unique_temp_dir("profiler-slots");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let capture = |ms: f64| ProfileCapture {
            format_version: 1,
            frames: 60,
            frame_ms: ms,
            cpu: vec![CaptureNode {
                name: "update".into(),
                inclusive_ms: ms,
                self_ms: ms,
                calls: 1.0,
                children: Vec::new(),
            }],
            truncated: 0,
            gpu: Vec::new(),
            folded: String::new(),
            timeline: Vec::new(),
            timeline_truncated: 0,
        };
        capture(2.0)
            .write(&root.join("captures/a.rprof.json"))
            .expect("a");
        capture(3.5)
            .write(&root.join("captures/b.rprof.json"))
            .expect("b");
        let slot = handler("editor.profiler_capture_slot");
        let a = slot(
            &mut world,
            &json!({ "slot": "a", "path": "captures/a.rprof.json" }),
        )
        .expect("a");
        assert!(a["diff"].is_null(), "one side only");
        let b = slot(
            &mut world,
            &json!({ "slot": "b", "path": "captures/b.rprof.json" }),
        )
        .expect("b");
        assert_eq!(b["diff"]["spans"][0]["inclusive_delta"], 1.5, "{b:#}");
        let bad = slot(&mut world, &json!({ "slot": "c" })).expect_err("bad slot");
        assert_eq!(
            bad.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_slot")
        );
        let missing = slot(
            &mut world,
            &json!({ "slot": "view", "path": "captures/none.rprof.json" }),
        )
        .expect_err("missing file");
        assert_eq!(
            missing
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_capture")
        );
        let cleared = slot(&mut world, &json!({ "slot": "a" })).expect("clear");
        assert!(cleared["a"].is_null() && cleared["diff"].is_null());
        let state = handler("editor.profiler_source")(&mut world, &json!({ "kind": "edit" })).expect("edit");
        assert_eq!(state["source"]["kind"], "edit");
        assert_eq!(state["compare"]["b"], "captures/b.rprof.json");
        let state = handler("editor.profiler_state")(&mut world, &json!({ "top": 3 })).expect("state");
        assert_eq!(state["status"], "live");
        assert!(state["entities"]["entities"].as_u64().is_some());
        let refused =
            handler("editor.profiler_source")(&mut world, &json!({ "kind": "process", "address": "server" }))
                .expect_err("no session");
        assert_eq!(
            refused
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_no_session")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7325: `editor.profiler_timeline` (the Timeline tab through the editor's method
    /// registry) refuses while the timeline is off, then lays out the edit source's frame per
    /// thread; the same view again is a layout cache hit.
    #[test]
    fn the_timeline_tab_answers_through_the_registry_and_caches_its_layout() {
        use tracing_subscriber::layer::SubscriberExt;
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        world.insert_resource(regia::Profiler(profiler.clone()));
        let timeline = handler("editor.profiler_timeline");
        let off = timeline(&mut world, &json!({})).expect_err("off");
        assert_eq!(
            off.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_no_timeline")
        );
        profiler.set_timeline_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            let _frame = tracing::info_span!("timeline_tab_frame").entered();
            let _inner = tracing::info_span!("timeline_tab_inner").entered();
        });
        profiler.end_frame();
        let first = timeline(&mut world, &json!({ "width": 400.0 })).expect("layout");
        assert_eq!(first["threads"].as_array().map(Vec::len), Some(1), "{first:#}");
        assert_eq!(first["threads"][0]["rows"], 2);
        assert_eq!(first["bars"][0]["label"], "timeline_tab_frame");
        assert_eq!(first["layout_cache"]["builds"], 1);
        let second = timeline(&mut world, &json!({ "width": 400.0 })).expect("layout");
        assert_eq!(second["layout_cache"]["hits"], 1, "{second:#}");
        assert_eq!(second["layout_cache"]["builds"], 1);
        let bad = timeline(&mut world, &json!({ "max_bars": 0 })).expect_err("bad");
        assert_eq!(
            bad.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_max_bars")
        );
    }

    /// The Entities view's total equals `memory.report`'s `ecs.entities` for the edit source.
    #[test]
    fn the_edit_source_entity_count_equals_memory_report() {
        let mut world = World::new();
        for _ in 0..7 {
            world.spawn_empty();
        }
        world.insert_resource(ProfilerView::default());
        let data = read_source(&mut world, 1, true, 30);
        let entities = data.entities.as_ref().map(|e| u64::from(e.entities));
        let report = regia_agent::memory_report(&mut world, &json!({})).expect("report");
        assert_eq!(entities, report["ecs"]["entities"].as_u64());
        assert_eq!(
            data.memory.as_ref().map(|m| &m["ecs"]["entities"]),
            Some(&report["ecs"]["entities"])
        );
    }

    /// `editor.profiler_freeze { frame? }` freezes a history frame of the shown source (the
    /// history bar click's agent twin), `editor.profiler_state` reports it, an absent `frame`
    /// unfreezes, and a frame out of the history is `profiler_bad_frame`.
    #[test]
    fn the_freeze_method_freezes_reports_and_clears() {
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..16 {
                let _span = tracing::info_span!("freeze_history_work").entered();
                profiler.end_frame();
            }
        });
        world.insert_resource(regia::Profiler(profiler));
        let freeze = handler("editor.profiler_freeze");
        let frozen = freeze(&mut world, &json!({ "frame": 12 })).expect("frozen");
        assert_eq!(frozen["frozen"], 12, "{frozen:#}");
        assert!(
            frozen["history"]["frames"].as_u64().is_some_and(|n| n > 12),
            "{frozen:#}"
        );
        assert_eq!(
            world.get_resource::<ProfilerView>().and_then(|v| v.frozen),
            Some(12),
            "the view the panel draws from is frozen"
        );
        let state = handler("editor.profiler_state")(&mut world, &json!({})).expect("state");
        assert_eq!(state["frozen"], 12, "{state:#}");
        let cleared = freeze(&mut world, &json!({})).expect("unfrozen");
        assert!(cleared["frozen"].is_null(), "{cleared:#}");
        assert_eq!(world.get_resource::<ProfilerView>().and_then(|v| v.frozen), None);
        let refused = freeze(&mut world, &json!({ "frame": 999 })).expect_err("out of range");
        assert_eq!(
            refused
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_frame")
        );
        let refused = freeze(&mut world, &json!({ "frame": "x" })).expect_err("not a number");
        assert_eq!(
            refused
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_frame")
        );
    }

    /// `editor.profiler_timeline { frames }` lays N consecutive retained frames (ending at the
    /// held or newest frame) on one time axis: the answer lists each frame with its start
    /// offset, and the bars of a later frame start at or after the earlier frames' ends.
    #[test]
    fn the_timeline_tab_lays_out_a_multi_frame_strip() {
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        world.insert_resource(regia::Profiler(profiler.clone()));
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            for i in 0..4u64 {
                let _span = tracing::info_span!("strip_work", i = i).entered();
                profiler.end_frame();
            }
        });
        let timeline = handler("editor.profiler_timeline");
        let one = timeline(&mut world, &json!({ "width": 600.0 })).expect("one frame");
        assert_eq!(
            one["frames"].as_array().map(Vec::len),
            Some(1),
            "the default is one frame: {one:#}"
        );
        let three = timeline(&mut world, &json!({ "width": 600.0, "frames": 3 })).expect("strip");
        let frames = three["frames"].as_array().cloned().unwrap_or_default();
        assert_eq!(frames.len(), 3, "{three:#}");
        assert_eq!(frames[0]["number"], 1, "the three newest, oldest first");
        assert_eq!(frames[0]["start_ns"], 0);
        let extent: u64 = frames[0]["span_ns"].as_u64().unwrap_or(0);
        assert!(extent > 0);
        assert_eq!(frames[1]["start_ns"], extent, "each frame starts past the last");
        assert_eq!(
            three["spans"].as_u64().unwrap_or(0),
            3 * one["spans"].as_u64().unwrap_or(0),
            "the strip holds every frame's spans"
        );
        // The default view still shows one frame: today's answer is unchanged.
        assert_eq!(
            world.get_resource::<ProfilerView>().map(|v| v.strip_frames()),
            Some(1)
        );
        let held =
            timeline(&mut world, &json!({ "width": 600.0, "frames": 3, "frame": 2 })).expect("held strip");
        assert_eq!(
            held["frames"]
                .as_array()
                .and_then(|f| f.first().and_then(|f| f["number"].as_u64())),
            Some(0),
            "a held frame ends the strip: {held:#}"
        );
        let refused = timeline(&mut world, &json!({ "frames": 9 })).expect_err("out of range");
        assert_eq!(
            refused
                .data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .as_deref(),
            Some("profiler_bad_frames")
        );
    }

    /// A fake process: an agent server whose requests go to a minimal app with the real agent
    /// dispatcher (`AgentPlugin`), its own frame profiler and `entities` entities. Each poll's
    /// first call (the `TREE_FRAMES` tree, which every worker version sends once per poll) first
    /// ends `frames_per_poll` frames, so exactly that many frames end between two polls (the app
    /// itself ends none). The counts an in-process source shows for that world are published
    /// after every answer.
    struct FakeProcess {
        url: String,
        token: String,
        stop: Arc<AtomicBool>,
        local_counts: Arc<Mutex<Option<EntityCounts>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeProcess {
        fn start(frames_per_poll: usize, entities: usize) -> Self {
            #[derive(Component)]
            struct Unit;
            #[derive(Component)]
            struct Tagged;
            let token = "fake-process-token".to_owned();
            let server = regia_agent::server::start(token.clone()).expect("fake agent server");
            let url = format!("ws://{}", server.addr);
            let stop = Arc::new(AtomicBool::new(false));
            let local_counts = Arc::new(Mutex::new(None));
            let (stop_thread, counts) = (Arc::clone(&stop), Arc::clone(&local_counts));
            let thread = std::thread::Builder::new()
                .name("fake-process".into())
                .spawn(move || {
                    let dir = regia_test_utils::unique_temp_dir("profiler-fake-process");
                    let mut app = crate::engine::app::App::new();
                    app.add_plugins(crate::engine::app::MinimalPlugins);
                    app.insert_resource(regia_agent::AgentSettings {
                        project_dir: Some(dir.clone()),
                    });
                    app.add_plugins(regia_agent::AgentPlugin);
                    app.finish_plugins();
                    let (forward, inbox) = crossbeam_channel::unbounded();
                    app.insert_resource(regia_agent::AgentInbox(inbox));
                    let profiler = regia_core::profiling::FrameProfiler::new();
                    app.insert_resource(regia::telemetry::SpanProfiler(profiler.clone()));
                    for i in 0..entities {
                        if i % 3 == 0 {
                            app.world_mut().spawn((Unit, Tagged));
                        } else {
                            app.world_mut().spawn(Unit);
                        }
                    }
                    let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
                        tracing_subscriber::registry(),
                        profiler.clone(),
                    ));
                    while !stop_thread.load(Ordering::Relaxed) {
                        let Ok(inbound) = server.inbox.recv_timeout(Duration::from_millis(20)) else {
                            continue;
                        };
                        if inbound.request.method == "profiler.frame"
                            && inbound.request.params["frames"].as_u64() == Some(TREE_FRAMES as u64)
                        {
                            tracing::dispatcher::with_default(&dispatch, || {
                                for _ in 0..frames_per_poll {
                                    {
                                        let _work = tracing::info_span!("fake_process_work").entered();
                                    }
                                    profiler.end_frame();
                                }
                            });
                        }
                        if forward.send(inbound).is_err() {
                            break;
                        }
                        app.update();
                        if let Ok(mut slot) = counts.lock() {
                            *slot = Some(profiler_model::entity_counts(app.world(), TOP_COMPONENTS));
                        }
                    }
                    let _ = std::fs::remove_dir_all(&dir);
                })
                .expect("fake process thread");
            Self {
                url,
                token,
                stop,
                local_counts,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeProcess {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Item 2 (w11-profiler-finish): a process that ends 100 frames between two polls gives the
    /// History tab 100 samples per poll, every frame (the `since_frame` cursor), not one sample
    /// per poll.
    #[test]
    fn a_process_ending_100_frames_between_polls_fills_100_history_samples_per_poll() {
        let fake = FakeProcess::start(100, 0);
        let mut world = World::new();
        set_source(
            &mut world,
            ProfilerSource::Remote {
                address: fake.url.clone(),
                token: fake.token.clone(),
            },
        )
        .expect("remote source");
        let snapshot = world
            .resource::<ProfilerView>()
            .worker()
            .expect("worker")
            .wait_for_polls(2, Duration::from_secs(60));
        assert!(snapshot.polls >= 2, "{:?}", snapshot.link);
        let expected = (100 * snapshot.polls as usize).min(HISTORY_FRAMES);
        assert_eq!(
            snapshot.history.len(),
            expected,
            "every frame of {} polls",
            snapshot.polls
        );
        assert!(
            snapshot
                .history
                .iter()
                .all(|s| s.spans.iter().any(|(name, _)| name == "fake_process_work")),
            "each sample carries its own frame's spans"
        );
        let data = read_source(&mut world, 0, false, 0);
        assert!(data.history.len() >= expected, "the panel reads the same history");
        set_source(&mut world, ProfilerSource::Edit).expect("back to edit");
        drop(fake);
    }

    /// Item 3 (w11-profiler-finish): a process source's Entities tab ranks component types,
    /// with the counts an in-process source shows for the same world.
    #[test]
    fn a_process_source_ranks_components_like_an_in_process_source() {
        let fake = FakeProcess::start(1, 30);
        let mut world = World::new();
        set_source(
            &mut world,
            ProfilerSource::Remote {
                address: fake.url.clone(),
                token: fake.token.clone(),
            },
        )
        .expect("remote source");
        let snapshot = world
            .resource::<ProfilerView>()
            .worker()
            .expect("worker")
            .wait_for_polls(1, Duration::from_secs(60));
        assert!(snapshot.polls >= 1, "{:?}", snapshot.link);
        let local = fake
            .local_counts
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .expect("the fake answered a poll");
        let data = read_source(&mut world, 0, false, 0);
        let remote = data.entities.clone().expect("ranked counts from the process");
        assert_eq!(remote, local, "the process source equals the in-process counts");
        let of = |suffix: &str| {
            remote
                .components
                .iter()
                .find(|(n, _)| n.ends_with(suffix))
                .map(|(_, k)| *k)
        };
        assert_eq!(of("::Unit"), Some(30));
        assert_eq!(of("::Tagged"), Some(10));
        let state = state_json(world.get_resource::<ProfilerView>(), &data, 10);
        assert!(
            state["entities"]["components"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty()),
            "editor.profiler_state lists them: {state:#}"
        );
        set_source(&mut world, ProfilerSource::Edit).expect("back to edit");
        drop(fake);
    }

    /// The history folding of a `since_frame` answer: rows past the cursor are appended in
    /// order, rows already seen are skipped, a restarted process (newest frame behind the
    /// cursor) sends the cursor back to 0, an older build's answer is one sample, and the
    /// history keeps the newest `HISTORY_FRAMES`.
    #[test]
    fn since_frame_answers_fold_into_the_history() {
        let row = |n: u64| json!({ "number": n, "ms": n as f64, "spans": [{ "name": "w", "ms": 1.0 }] });
        let answer = |rows: Vec<Value>, newest: u64| json!({ "frame_times": rows, "newest_frame": newest });
        let mut history = Vec::new();
        let mut cursor = HistoryCursor::default();
        fold_history(&mut history, &mut cursor, &answer((0..5).map(row).collect(), 4));
        assert_eq!(history.len(), 5);
        assert_eq!(cursor, HistoryCursor(Some(5)));
        // An overlapping answer adds only the new frames.
        fold_history(&mut history, &mut cursor, &answer((3..8).map(row).collect(), 7));
        let ms: Vec<f64> = history.iter().map(|s| s.ms).collect();
        assert_eq!(ms, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(history[7].spans, [("w".to_owned(), 1.0)]);
        // The process restarted: its newest frame is behind the cursor.
        fold_history(&mut history, &mut cursor, &answer(Vec::new(), 2));
        assert_eq!(cursor, HistoryCursor(Some(0)));
        assert_eq!(history.len(), 8);
        // An older build: one sample from the answer's frame time.
        let mut legacy = HistoryCursor(None);
        fold_history(
            &mut history,
            &mut legacy,
            &json!({ "frame_ms": 9.5, "top": [{ "name": "t", "ms": 9.0 }] }),
        );
        assert_eq!(history.last().map(|s| s.ms), Some(9.5));
        // The newest HISTORY_FRAMES are kept.
        let mut long = Vec::new();
        let mut c = HistoryCursor::default();
        let n = HISTORY_FRAMES as u64 + 40;
        fold_history(&mut long, &mut c, &answer((0..n).map(row).collect(), n - 1));
        assert_eq!(long.len(), HISTORY_FRAMES);
        assert_eq!(long[0].ms, 40.0);
    }

    /// Item 1, the strip's freeze (w11-profiler-finish): freezing a history frame stops the
    /// history's intake, so the frozen index keeps naming the same frame while the process ends
    /// more frames; unfreezing resumes it.
    #[test]
    fn freezing_stops_the_history_intake_and_unfreezing_resumes_it() {
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        let frames = |n: usize| {
            tracing::dispatcher::with_default(&dispatch, || {
                for _ in 0..n {
                    {
                        let _span = tracing::info_span!("freeze_intake_work").entered();
                    }
                    profiler.end_frame();
                }
            });
        };
        frames(16);
        world.insert_resource(regia::Profiler(profiler.clone()));
        let state = handler("editor.profiler_state");
        let freeze = handler("editor.profiler_freeze");
        let before = state(&mut world, &json!({})).expect("state");
        assert_eq!(before["history"]["frames"], 16, "{before:#}");
        freeze(&mut world, &json!({ "frame": 3 })).expect("frozen");
        frames(5);
        let frozen = state(&mut world, &json!({})).expect("frozen state");
        assert_eq!(frozen["frozen"], 3);
        assert_eq!(
            frozen["history"], before["history"],
            "no frame is taken in while frozen"
        );
        // Freezing another frame keeps the frozen history.
        let again = freeze(&mut world, &json!({ "frame": 15 })).expect("refrozen");
        assert_eq!(again["history"]["frames"], 16, "{again:#}");
        let resumed = freeze(&mut world, &json!({})).expect("unfrozen");
        assert!(resumed["frozen"].is_null());
        let live = state(&mut world, &json!({})).expect("live state");
        assert_eq!(live["history"]["frames"], 21, "the intake resumed: {live:#}");
        // A new source drops the frozen history too.
        freeze(&mut world, &json!({ "frame": 1 })).expect("frozen again");
        set_source(&mut world, ProfilerSource::Edit).expect("edit");
        frames(1);
        let after = state(&mut world, &json!({})).expect("state");
        assert_eq!(after["history"]["frames"], 22);
        assert!(after["frozen"].is_null());
    }

    /// Item 4 (w11-profiler-finish): `editor.profiler_timeline { frame }` holds the Timeline tab on
    /// that frame (the click's twin): the answer shows it and reports it as held, later frames do
    /// not move it, a frame that left the history is `profiler_no_timeline`, and a non-integer is
    /// `profiler_bad_frame`.
    #[test]
    fn the_timeline_method_holds_the_frame_it_is_given() {
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        world.insert_resource(regia::Profiler(profiler.clone()));
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        let frames = |n: usize| {
            tracing::dispatcher::with_default(&dispatch, || {
                for _ in 0..n {
                    {
                        let _span = tracing::info_span!("held_frame_work").entered();
                    }
                    profiler.end_frame();
                }
            });
        };
        frames(5);
        let timeline = handler("editor.profiler_timeline");
        let held = timeline(&mut world, &json!({ "frame": 2, "width": 400.0 })).expect("held");
        assert_eq!(held["frame"], 2, "{held:#}");
        assert_eq!(held["held"], 2);
        frames(3);
        let still = timeline(&mut world, &json!({ "width": 400.0 })).expect("still held");
        assert_eq!(still["frame"], 2, "a held frame stays: {still:#}");
        let gone = timeline(&mut world, &json!({ "frame": 999 })).expect_err("not retained");
        let code = |e: RpcError| e.data.and_then(|d| d["code"].as_str().map(str::to_owned));
        assert_eq!(code(gone).as_deref(), Some("profiler_no_timeline"));
        let bad = timeline(&mut world, &json!({ "frame": "newest" })).expect_err("not a number");
        assert_eq!(code(bad).as_deref(), Some("profiler_bad_frame"));
    }

    /// Review g16 finding 5: a `frame` outside the history fails `profiler_no_timeline` and leaves
    /// the tab's hold as it was: Live stays Live, and a held frame stays held.
    #[test]
    fn a_frame_outside_the_history_does_not_latch_a_hold() {
        let mut world = World::new();
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        world.insert_resource(regia::Profiler(profiler.clone()));
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..5 {
                {
                    let _span = tracing::info_span!("latch_frame_work").entered();
                }
                profiler.end_frame();
            }
        });
        let timeline = handler("editor.profiler_timeline");
        let code = |e: RpcError| e.data.and_then(|d| d["code"].as_str().map(str::to_owned));
        let held = |world: &World| {
            world
                .get_resource::<ProfilerView>()
                .and_then(|v| v.timeline_frame)
        };
        // Live: a refused frame keeps the tab live.
        let gone = timeline(&mut world, &json!({ "frame": 999 })).expect_err("not retained");
        assert_eq!(code(gone).as_deref(), Some("profiler_no_timeline"));
        assert_eq!(held(&world), None, "a refused frame must not hold the tab");
        let live = timeline(&mut world, &json!({ "width": 400.0 })).expect("live");
        assert_eq!(live["held"], Value::Null, "{live:#}");
        assert_eq!(live["frame"], 4, "the newest frame: {live:#}");
        // Held: a refused frame keeps the earlier hold.
        timeline(&mut world, &json!({ "frame": 1, "width": 400.0 })).expect("hold 1");
        let gone = timeline(&mut world, &json!({ "frame": 999 })).expect_err("not retained");
        assert_eq!(code(gone).as_deref(), Some("profiler_no_timeline"));
        assert_eq!(held(&world), Some(1), "the earlier hold stays");
        let still = timeline(&mut world, &json!({ "width": 400.0 })).expect("still held");
        assert_eq!(still["frame"], 1, "{still:#}");
    }
}

#[cfg(test)]
mod lockstep_source_tests {
    use regia_framework::det_debug::{DebugWhat, DetDebugDraw};

    use super::*;

    /// The Lockstep tab's switch reaches the play world through the editor loop, and the tab's
    /// copy follows the play world (ADR-8562). Nothing is copied while the tab is hidden.
    #[test]
    fn lockstep_switch_reaches_play_and_the_copy_follows() {
        let mut edit = World::new();
        let mut play = World::new();
        play.insert_resource(DetDebugDraw::default());
        edit.insert_resource(crate::profiler_panel::ProfilerPanel {
            open: true,
            ..Default::default()
        });
        edit.insert_resource(ProfilerView {
            tab: ProfilerTab::Flame,
            ..ProfilerView::default()
        });
        copy_lockstep_from_play(&mut edit, Some(&mut play));
        assert!(
            edit.resource::<ProfilerView>().lockstep_draw.is_none(),
            "hidden tab: no copy"
        );

        edit.resource_mut::<ProfilerView>().tab = ProfilerTab::Lockstep;
        request_det_debug(&mut edit, DebugWhat::CONTACTS);
        assert_eq!(
            edit.resource::<ProfilerView>().lockstep_pending,
            Some(DebugWhat::CONTACTS)
        );
        copy_lockstep_from_play(&mut edit, Some(&mut play));
        assert_eq!(play.resource::<DetDebugDraw>().what, DebugWhat::CONTACTS);
        let view = edit.resource::<ProfilerView>();
        assert_eq!(view.lockstep_pending, None);
        assert_eq!(view.lockstep_draw.map(|d| d.what), Some(DebugWhat::CONTACTS));
        assert!(view.lockstep.is_none(), "play has no tick profile");
        let (report, draw) = lockstep_source(&edit);
        assert!(report.is_none());
        assert_eq!(draw.map(|d| d.what), Some(DebugWhat::CONTACTS));
    }
}
