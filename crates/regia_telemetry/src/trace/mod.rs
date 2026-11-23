// SPDX-License-Identifier: MPL-2.0
//! The trace recorder (spec 54 § 2.4): marks, nestable ranges, screenshot marks, frame and pass
//! events, kept in a rolling in-memory window and/or written to a `.regiatrace` file
//! ([`mod@file`]), plus exports to JSON lines and the public Trace Event Format ([`export`]).
//!
//! States: [`RecorderState::Idle`] (nothing kept), `Windowed` (the newest events in memory),
//! `Writing` (events go to a file; the window keeps filling). Every [`Recorder`] is an instance
//! with an injectable clock; the process uses [`recorder`]. A disabled stream costs one relaxed
//! load. Times are monotonic nanoseconds since process start and never compared in tests.

pub mod events;
pub mod export;
pub mod file;
pub mod launch;
pub mod writer;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

pub use events::{Event, EventKind, Lane, TraceFile, read_trace, read_trace_bytes};
pub use file::{
    FORMAT_VERSION, Header, TraceReadError, regiatrace_migration_sources, regiatrace_schema_fingerprint,
};
pub use launch::{
    ANDROID_RECORD_TRACE_MARKER, LaunchRecording, RECORD_TRACE_ENV, RECORD_TRACE_FLAG, TracePlugin,
    default_trace_dir, record_trace_request,
};

/// Most marks per second per name; more are dropped and counted.
pub const MARKS_PER_SECOND: u32 = 100;
/// Upper bound on the distinct names the mark rate limiter tracks. Mark names are arbitrary (the
/// agent's `telemetry.mark` takes any 1-64-char string), so without a bound a session that keeps
/// marking new names would grow the table for the process lifetime. Past the cap the
/// least-recently-used name is evicted; its per-second count resets, which only re-arms the limit.
const MARK_RATE_NAMES_MAX: usize = 1024;
/// Deepest range nesting per lane; deeper opens are dropped and counted.
pub const MAX_RANGE_DEPTH: usize = 32;
/// Most bytes of a mark's data.
pub const MAX_MARK_DATA_BYTES: usize = 1024;
/// The file extension.
pub const EXTENSION: &str = "regiatrace";
/// Pending bytes of a file sink flushed to disk at this size.
const SINK_FLUSH_BYTES: usize = 256 * 1024;

/// Which streams record. A disabled stream costs one relaxed load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Streams {
    /// Marks, ranges, screenshot marks.
    pub marks: bool,
    /// Frame begin and end.
    pub frames: bool,
    /// Breadcrumb pass intervals.
    pub passes: bool,
    /// Counters.
    pub counters: bool,
    /// Log records (opt-in).
    pub logs: bool,
}

impl Default for Streams {
    fn default() -> Self {
        Self {
            marks: true,
            frames: true,
            passes: true,
            counters: false,
            logs: false,
        }
    }
}

const S_MARKS: u8 = 1;
const S_FRAMES: u8 = 2;
const S_PASSES: u8 = 4;
const S_COUNTERS: u8 = 8;
const S_LOGS: u8 = 16;

impl Streams {
    fn mask(self) -> u8 {
        [
            (self.marks, S_MARKS),
            (self.frames, S_FRAMES),
            (self.passes, S_PASSES),
            (self.counters, S_COUNTERS),
            (self.logs, S_LOGS),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |m, (_, bit)| m | bit)
    }

    /// From stream names (`marks`, `frames`, `passes`, `counters`, `logs`); `None` for an unknown
    /// name. `marks` is always on.
    #[must_use]
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut s = Self {
            marks: true,
            frames: false,
            passes: false,
            counters: false,
            logs: false,
        };
        for n in names {
            match n {
                "marks" => {}
                "frames" => s.frames = true,
                "passes" => s.passes = true,
                "counters" => s.counters = true,
                "logs" => s.logs = true,
                _ => return None,
            }
        }
        Some(s)
    }
}

/// The recorder's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderState {
    /// Nothing is kept.
    Idle,
    /// The newest events are kept in memory.
    Windowed,
    /// Events go to a file (the window keeps filling).
    Writing,
}

impl RecorderState {
    /// `idle`, `windowed` or `writing`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Windowed => "windowed",
            Self::Writing => "writing",
        }
    }
}

/// Recorder settings.
#[derive(Clone, Debug)]
pub struct TraceConfig {
    /// Seconds of events the window keeps; 0 turns the window off.
    pub window_seconds: f32,
    /// Byte cap of the window.
    pub window_bytes: usize,
    /// The folder trace files land in: the project's `profiling/` log directory, or on Android
    /// the app's external files directory's `profiling/` once [`TracePlugin`] ran
    /// ([`default_trace_dir`]).
    pub dir: PathBuf,
    /// Streams recorded by default.
    pub streams: Streams,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            window_seconds: 0.0,
            window_bytes: 2 * 1024 * 1024,
            dir: default_trace_dir(None),
            streams: Streams::default(),
        }
    }
}

impl TraceConfig {
    /// Whether the rolling window is on: a positive, finite duration. NaN and infinities mean
    /// "no window" like 0 — an infinite horizon would never expire events by time, so they would
    /// accumulate to the byte cap only.
    fn window_active(&self) -> bool {
        self.window_seconds.is_finite() && self.window_seconds > 0.0
    }
}

/// A range's id; [`RangeId::DROPPED`] when the open was refused or no stream wanted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RangeId(pub u64);

impl RangeId {
    /// The id of a range that was not opened.
    pub const DROPPED: Self = Self(0);
}

/// What opening a range did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeOpened {
    /// The id ([`RangeId::DROPPED`] when not recorded).
    pub id: RangeId,
    /// Depth on its lane.
    pub depth: u32,
    /// Whether it was recorded.
    pub recorded: bool,
}

/// What closing a range did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeClosed {
    /// Nanoseconds from open to close.
    pub duration_ns: u64,
    /// Inner ranges closed first because they were still open.
    pub forced_close: Vec<RangeId>,
}

/// A range id nobody opened (or already closed); counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownRange;

/// What a mark did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkResult {
    /// The time stamped.
    pub t_ns: u64,
    /// The app frame.
    pub frame: u64,
    /// Whether a stream kept it (false: no stream wanted it, or the rate limit dropped it).
    pub recorded: bool,
}

/// Why a recorder call failed. [`Self::reason`] is the agent's `data.reason`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceError {
    /// `start` while writing.
    Busy,
    /// `stop` while not writing.
    NotWriting,
    /// `save_window` with no window.
    NoWindow,
    /// A file name with separators or traversal.
    BadFileName,
    /// The file could not be created or written.
    Io(String),
}

impl TraceError {
    /// The agent `data.reason`.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Busy => "trace_busy",
            Self::NotWriting => "trace_not_writing",
            Self::NoWindow => "trace_not_writing",
            Self::BadFileName => "bad_file_name",
            Self::Io(_) => "trace_io",
        }
    }
}

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("a trace is already being written"),
            Self::NotWriting => f.write_str("no trace is being written"),
            Self::NoWindow => f.write_str("the rolling window is off"),
            Self::BadFileName => f.write_str("the file name must be a bare name (no separators or `..`)"),
            Self::Io(e) => write!(f, "trace file error: {e}"),
        }
    }
}

impl std::error::Error for TraceError {}

/// The recorder's status or a stop/save summary.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceSummary {
    /// State after the call.
    pub state: RecorderState,
    /// The file written or being written.
    pub file: Option<PathBuf>,
    /// Bytes written to it.
    pub bytes: u64,
    /// Events recorded in it.
    pub events: u64,
    /// Events dropped (rate limit, depth, window evictions are not counted).
    pub dropped: u64,
    /// Ranges still open.
    pub ranges_open: u32,
    /// A write error closed the file early.
    pub degraded: bool,
}

struct OpenRange {
    id: u64,
    begin_ns: u64,
}

struct Sink {
    path: PathBuf,
    /// The writer thread that owns the file (ADR-2307): the frame never touches it.
    writer: writer::Writer,
    enc: events::Encoder,
    /// Complete chunks not handed to the writer yet (cut, or held while its queue was full).
    pending: Vec<u8>,
    /// Bytes handed to the writer (the file's size once it drains).
    bytes: u64,
    events: u64,
    index: Vec<(u64, u64)>,
    last_second: u64,
    /// `pending` length that triggers the next flush (moves up while the writer's queue is full,
    /// so a stuck writer does not turn every event into its own chunk).
    next_flush_at: usize,
}

impl Sink {
    fn push(&mut self, e: &Event) -> std::io::Result<()> {
        let second = e.t_ns / 1_000_000_000;
        if second != self.last_second || self.index.is_empty() {
            self.last_second = second;
            // Cut first: everything encoded so far lands before the offset, so the offset is
            // the start of the first chunk that holds the new second.
            self.enc.cut(&mut self.pending);
            self.index.push((second, self.bytes + self.pending.len() as u64));
        }
        self.enc.push(e, &mut self.pending);
        self.events += 1;
        if self.pending.len() >= self.next_flush_at {
            self.flush()?;
        }
        Ok(())
    }

    /// Cuts the encoder and hands the chunks to the writer without blocking. A full queue keeps
    /// them here for the next flush; past `writer::MAX_HELD_BYTES` that is an error (the
    /// recording stops at its last complete chunk).
    fn flush(&mut self) -> std::io::Result<()> {
        let _span = tracing::trace_span!("trace_flush").entered();
        self.enc.cut(&mut self.pending);
        if self.pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.pending);
        let len = batch.len() as u64;
        match self.writer.try_send(batch) {
            Ok(()) => {
                self.bytes += len;
                self.next_flush_at = SINK_FLUSH_BYTES;
                Ok(())
            }
            Err(writer::SendError::Full(back)) => {
                self.pending = back;
                self.next_flush_at = self.pending.len() + SINK_FLUSH_BYTES / 4;
                if self.pending.len() > writer::MAX_HELD_BYTES {
                    return Err(std::io::Error::other(
                        "the trace writer fell behind (queue full past the held-bytes cap)",
                    ));
                }
                Ok(())
            }
            Err(writer::SendError::Failed(e)) => Err(std::io::Error::other(e)),
        }
    }

    /// Writes what is pending and the index chunk, waits for the writer to drain and flush.
    /// Stop and save only: this waits on the writer.
    fn close(mut self) -> (PathBuf, u64, u64, std::io::Result<()>) {
        let mut p = Vec::new();
        for &(second, offset) in &self.index {
            file::put_varint(&mut p, second);
            file::put_varint(&mut p, offset);
        }
        self.enc.cut(&mut self.pending);
        let mut tail = std::mem::take(&mut self.pending);
        tail.extend(file::encode_chunk(file::chunk::INDEX, &p));
        let len = tail.len() as u64;
        let sent = self.writer.send_blocking(tail);
        let closed = self.writer.close();
        let result = sent.and(closed);
        if result.is_ok() {
            self.bytes += len;
        }
        (self.path, self.bytes, self.events, result)
    }
}

#[derive(Default)]
struct Dropped {
    marks: u64,
    depth: u64,
    unknown: u64,
}

struct Inner {
    config: TraceConfig,
    state: RecorderState,
    window: VecDeque<Event>,
    window_bytes: usize,
    sink: Option<Sink>,
    open: HashMap<Lane, Vec<OpenRange>>,
    /// Per-name rate-limit slots: `(second, count in it, last-use tick)`. The table is bounded at
    /// [`MARK_RATE_NAMES_MAX`] names by last-use eviction.
    mark_rate: HashMap<String, (u64, u32, u64)>,
    mark_rate_lru: u64,
    dropped: Dropped,
    degraded: bool,
    header: Header,
    pass_names: Vec<Option<Arc<str>>>,
    last_pass_submission: u64,
}

/// A trace recorder.
pub struct Recorder {
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    start_unix_ms: u64,
    mask: AtomicU8,
    frame: AtomicU64,
    next_id: AtomicU64,
    inner: Mutex<Inner>,
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn process_clock_ns() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(std::time::Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Recorder {
    /// A recorder on the process clock.
    #[must_use]
    pub fn new(config: TraceConfig) -> Self {
        let _ = process_clock_ns();
        Self::with_clock(config, Box::new(process_clock_ns))
    }

    /// A recorder on `clock` (nanoseconds; tests inject one).
    #[must_use]
    pub fn with_clock(config: TraceConfig, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let start_unix_ms = unix_ms_now().saturating_sub(clock() / 1_000_000);
        let r = Self {
            clock,
            start_unix_ms,
            mask: AtomicU8::new(0),
            frame: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                config: config.clone(),
                state: RecorderState::Idle,
                window: VecDeque::new(),
                window_bytes: 0,
                sink: None,
                open: HashMap::new(),
                mark_rate: HashMap::new(),
                mark_rate_lru: 0,
                dropped: Dropped::default(),
                degraded: false,
                header: Header {
                    format_version: FORMAT_VERSION,
                    start_unix_ms,
                    engine_version: env!("CARGO_PKG_VERSION").to_owned(),
                    build_profile: if cfg!(debug_assertions) { "dev" } else { "release" }.to_owned(),
                    platform: std::env::consts::OS.to_owned(),
                    ..Header::default()
                },
                pass_names: Vec::new(),
                last_pass_submission: 0,
            }),
        };
        r.configure(config);
        r
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Nanoseconds now on the recorder's clock.
    #[must_use]
    pub fn now_ns(&self) -> u64 {
        (self.clock)()
    }

    /// Applies `config`: a window above 0 seconds makes the recorder `Windowed` when it is idle;
    /// 0 drops the window (unless writing).
    pub fn configure(&self, config: TraceConfig) {
        let mut i = self.lock();
        i.config = config;
        if !i.config.window_active() {
            i.window.clear();
            i.window_bytes = 0;
        }
        match (i.state, i.config.window_active()) {
            (RecorderState::Idle, true) => i.state = RecorderState::Windowed,
            (RecorderState::Windowed, false) => i.state = RecorderState::Idle,
            _ => {}
        }
        self.refresh_mask(&i);
    }

    /// A copy of the settings in force.
    #[must_use]
    pub fn config(&self) -> TraceConfig {
        self.lock().config.clone()
    }

    /// Resizes the rolling window (0 turns it off); an idle recorder becomes `Windowed`.
    pub fn set_window_seconds(&self, seconds: f32) {
        let mut config = self.lock().config.clone();
        config.window_seconds = seconds;
        self.configure(config);
    }

    /// The adapter and driver the header names.
    pub fn set_adapter(&self, adapter: &str) {
        self.lock().header.adapter = adapter.to_owned();
    }

    fn refresh_mask(&self, i: &Inner) {
        let m = if i.state == RecorderState::Idle {
            0
        } else {
            i.config.streams.mask() | S_MARKS
        };
        self.mask.store(m, Ordering::Relaxed);
    }

    /// Whether `stream` (`marks`, `frames`, `passes`, `counters`, `logs`) records: one relaxed load.
    #[must_use]
    pub fn wants(&self, stream: &str) -> bool {
        let bit = match stream {
            "marks" => S_MARKS,
            "frames" => S_FRAMES,
            "passes" => S_PASSES,
            "counters" => S_COUNTERS,
            "logs" => S_LOGS,
            _ => 0,
        };
        self.mask.load(Ordering::Relaxed) & bit != 0
    }

    /// The recorder's state.
    #[must_use]
    pub fn state(&self) -> RecorderState {
        self.lock().state
    }

    /// Sets the app frame number stamped on events.
    pub fn set_frame(&self, frame: u64) {
        self.frame.store(frame, Ordering::Relaxed);
    }

    /// The app frame number.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame.load(Ordering::Relaxed)
    }

    fn emit(&self, i: &mut Inner, event: Event) {
        if i.config.window_active() {
            i.window_bytes += event.approx_bytes();
            let horizon = event
                .t_ns
                .saturating_sub((f64::from(i.config.window_seconds) * 1e9) as u64);
            i.window.push_back(event.clone());
            while let Some(front) = i.window.front() {
                if front.t_ns < horizon || i.window_bytes > i.config.window_bytes {
                    i.window_bytes = i.window_bytes.saturating_sub(front.approx_bytes());
                    i.window.pop_front();
                } else {
                    break;
                }
            }
        }
        if let Some(sink) = i.sink.as_mut()
            && let Err(e) = sink.push(&event)
        {
            tracing::error!(target: "regia_telemetry", "trace write failed, recording stopped: {e}");
            i.degraded = true;
            i.sink = None;
            i.state = if i.config.window_active() {
                RecorderState::Windowed
            } else {
                RecorderState::Idle
            };
            self.refresh_mask(i);
        }
    }

    /// A named instant. Also emits a `regia::mark` tracing event. At most
    /// [`MARKS_PER_SECOND`] per second per name are kept. The limiter's name table is bounded
    /// (`MARK_RATE_NAMES_MAX` names, least-recently-used eviction), so arbitrary distinct names
    /// from a host do not grow it without limit.
    pub fn mark(&self, name: &str, data: Option<&serde_json::Value>) -> MarkResult {
        let frame = self.frame();
        tracing::info!(target: "regia::mark", name = %name, frame);
        // The process recorder's marks also go to the Profiler's mark listing (P-318), recording
        // or not.
        if std::ptr::eq(self, recorder()) {
            crate::counters::note_mark(name, "trace");
        }
        let t_ns = self.now_ns();
        if !self.wants("marks") {
            return MarkResult {
                t_ns,
                frame,
                recorded: false,
            };
        }
        let data = data
            .and_then(|d| serde_json::to_string(d).ok())
            .filter(|d| d.len() <= MAX_MARK_DATA_BYTES);
        let mut i = self.lock();
        let second = t_ns / 1_000_000_000;
        if i.mark_rate.len() >= MARK_RATE_NAMES_MAX && !i.mark_rate.contains_key(name) {
            // The table is full and this is a new name: evict the least-recently-used one so the
            // limiter stays bounded for the process lifetime.
            if let Some(oldest) = i
                .mark_rate
                .iter()
                .min_by_key(|(_, (_, _, tick))| *tick)
                .map(|(name, _)| name.clone())
            {
                i.mark_rate.remove(&oldest);
            }
        }
        i.mark_rate_lru += 1;
        let lru = i.mark_rate_lru;
        let slot = i.mark_rate.entry(name.to_owned()).or_insert((second, 0, lru));
        if slot.0 != second {
            *slot = (second, 0, lru);
        } else {
            slot.2 = lru;
        }
        if slot.1 >= MARKS_PER_SECOND {
            i.dropped.marks += 1;
            return MarkResult {
                t_ns,
                frame,
                recorded: false,
            };
        }
        slot.1 += 1;
        self.emit(
            &mut i,
            Event {
                t_ns,
                lane: Lane::Main,
                kind: EventKind::Mark {
                    name: name.to_owned(),
                    data,
                    frame,
                },
            },
        );
        MarkResult {
            t_ns,
            frame,
            recorded: true,
        }
    }

    /// Opens a range named `name` on `lane`.
    pub fn open_range(&self, name: &str, lane: Lane) -> RangeOpened {
        if !self.wants("marks") {
            return RangeOpened {
                id: RangeId::DROPPED,
                depth: 0,
                recorded: false,
            };
        }
        let t_ns = self.now_ns();
        let frame = self.frame();
        let mut i = self.lock();
        let depth = i.open.get(&lane).map_or(0, Vec::len);
        if depth >= MAX_RANGE_DEPTH {
            i.dropped.depth += 1;
            return RangeOpened {
                id: RangeId::DROPPED,
                depth: u32::try_from(depth).unwrap_or(u32::MAX),
                recorded: false,
            };
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let parent = i.open.get(&lane).and_then(|s| s.last()).map(|r| r.id);
        i.open
            .entry(lane)
            .or_default()
            .push(OpenRange { id, begin_ns: t_ns });
        let depth = u32::try_from(depth).unwrap_or(u32::MAX);
        self.emit(
            &mut i,
            Event {
                t_ns,
                lane,
                kind: EventKind::RangeOpen {
                    id,
                    name: name.to_owned(),
                    frame,
                    depth,
                    parent,
                },
            },
        );
        RangeOpened {
            id: RangeId(id),
            depth,
            recorded: true,
        }
    }

    /// Closes `id`; inner ranges still open on its lane close first, flagged `forced_close`.
    /// An unknown id is counted. Closing [`RangeId::DROPPED`] is a no-op.
    pub fn close_range(&self, id: RangeId) -> Result<RangeClosed, UnknownRange> {
        if id == RangeId::DROPPED {
            return Ok(RangeClosed {
                duration_ns: 0,
                forced_close: Vec::new(),
            });
        }
        let t_ns = self.now_ns();
        let frame = self.frame();
        let mut i = self.lock();
        let found = i
            .open
            .iter()
            .find_map(|(lane, stack)| stack.iter().position(|r| r.id == id.0).map(|p| (*lane, p)));
        let Some((lane, at)) = found else {
            i.dropped.unknown += 1;
            return Err(UnknownRange);
        };
        let closed: Vec<OpenRange> = i
            .open
            .get_mut(&lane)
            .map(|s| s.drain(at..).rev().collect())
            .unwrap_or_default();
        let mut forced = Vec::new();
        let mut duration_ns = 0;
        for r in closed {
            let is_target = r.id == id.0;
            if is_target {
                duration_ns = t_ns.saturating_sub(r.begin_ns);
            } else {
                forced.push(RangeId(r.id));
            }
            self.emit(
                &mut i,
                Event {
                    t_ns,
                    lane,
                    kind: EventKind::RangeClose {
                        id: r.id,
                        frame,
                        forced: !is_target,
                    },
                },
            );
        }
        Ok(RangeClosed {
            duration_ns,
            forced_close: forced,
        })
    }

    /// Frame begin (frames stream).
    pub fn frame_begin(&self, frame: u64) {
        self.set_frame(frame);
        if self.wants("frames") {
            let t_ns = self.now_ns();
            let mut i = self.lock();
            self.emit(
                &mut i,
                Event {
                    t_ns,
                    lane: Lane::Main,
                    kind: EventKind::FrameBegin { frame },
                },
            );
        }
    }

    /// Frame end (frames stream).
    pub fn frame_end(&self, frame: u64) {
        if self.wants("frames") {
            let t_ns = self.now_ns();
            let mut i = self.lock();
            self.emit(
                &mut i,
                Event {
                    t_ns,
                    lane: Lane::Main,
                    kind: EventKind::FrameEnd { frame },
                },
            );
        }
    }

    /// A counter sample (counters stream).
    pub fn counter(&self, name: &str, value: f64) {
        if self.wants("counters") {
            let t_ns = self.now_ns();
            let mut i = self.lock();
            self.emit(
                &mut i,
                Event {
                    t_ns,
                    lane: Lane::Main,
                    kind: EventKind::Counter {
                        name: name.to_owned(),
                        value,
                    },
                },
            );
        }
    }

    /// Records the breadcrumb passes of submissions not recorded yet (passes stream), on the
    /// `gpu` lane. Pass events carry GPU order, not times: they are stamped at the call.
    ///
    /// Callers that already hold a [`regia_render::breadcrumbs::BreadcrumbSnapshot`] use this.
    /// The frame observer uses `capture_fresh_passes`, which does not copy the ring.
    pub fn record_breadcrumbs(&self, snapshot: &regia_render::breadcrumbs::BreadcrumbSnapshot) {
        if !self.wants("passes") {
            return;
        }
        let t_ns = self.now_ns();
        let mut i = self.lock();
        let mut newest = i.last_pass_submission;
        for f in snapshot.frames.iter().rev() {
            for sub in &f.submissions {
                if sub.id.0 <= i.last_pass_submission {
                    continue;
                }
                newest = newest.max(sub.id.0);
                let (start, end) = (sub.start.min(f.markers.len()), sub.end.min(f.markers.len()));
                if start > end {
                    continue;
                }
                for m in &f.markers[start..end] {
                    let name_if_new = snapshot
                        .names
                        .get(usize::from(m.tag.0))
                        .map_or("", String::as_str);
                    self.emit_cached_pass(
                        &mut i,
                        t_ns,
                        &FreshMarker {
                            frame: f.frame,
                            submission: sub.id.0,
                            tag: m.tag.0,
                            open: m.open,
                        },
                        name_if_new,
                    );
                }
            }
        }
        i.last_pass_submission = newest;
    }

    /// Records submissions newer than the cursor from the global breadcrumb writer. Copies only
    /// those markers (into a reused thread-local buffer) and does not clone the name table.
    /// A tag already in `pass_names` does not build another `Arc<str>`.
    fn capture_fresh_passes(&self) {
        thread_local! {
            static FRESH: RefCell<Vec<FreshMarker>> = const { RefCell::new(Vec::new()) };
        }
        let after = self.lock().last_pass_submission;
        let t_ns = self.now_ns();
        let mut fresh = FRESH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        fresh.clear();
        let mut newest = after;
        let _skipped = regia_render::breadcrumbs::for_each_fresh_pass(after, |frame, submission, markers| {
            newest = newest.max(submission);
            for m in markers {
                fresh.push(FreshMarker {
                    frame,
                    submission,
                    tag: m.tag.0,
                    open: m.open,
                });
            }
        });
        // Slot index order is not submission order. Sort only when a later slot held an older
        // submission, so a steady frame (one submission) does not allocate a sort buffer.
        if fresh
            .windows(2)
            .any(|pair| pair[0].submission > pair[1].submission)
        {
            fresh.sort_by_key(|marker| marker.submission);
        }
        {
            let mut i = self.lock();
            for marker in &fresh {
                if marker.submission <= i.last_pass_submission {
                    continue;
                }
                let slot = usize::from(marker.tag);
                let known = i.pass_names.get(slot).is_some_and(Option::is_some);
                if known {
                    self.emit_cached_pass(&mut i, t_ns, marker, "");
                } else {
                    let owned = regia_render::breadcrumbs::global()
                        .name_of(regia_render::breadcrumbs::PassTag(marker.tag));
                    self.emit_cached_pass(&mut i, t_ns, marker, &owned);
                }
            }
            if newest > i.last_pass_submission {
                i.last_pass_submission = newest;
            }
        }
        fresh.clear();
        FRESH.with(|cell| *cell.borrow_mut() = fresh);
    }

    /// Emits one pass marker. `name_if_new` is used only the first time `tag` is seen; after that
    /// the cached `Arc<str>` is cloned (no heap allocation).
    fn emit_cached_pass(&self, i: &mut Inner, t_ns: u64, marker: &FreshMarker, name_if_new: &str) {
        let slot = usize::from(marker.tag);
        if i.pass_names.len() <= slot {
            i.pass_names.resize(slot + 1, None);
        }
        let name = if let Some(name) = i.pass_names[slot].clone() {
            name
        } else {
            let name: Arc<str> = Arc::from(name_if_new);
            i.pass_names[slot] = Some(Arc::clone(&name));
            name
        };
        let kind = if marker.open {
            EventKind::PassOpen {
                tag: marker.tag,
                name,
                frame: marker.frame,
                submission: marker.submission,
            }
        } else {
            EventKind::PassClose {
                tag: marker.tag,
                name,
                frame: marker.frame,
                submission: marker.submission,
            }
        };
        self.emit(
            i,
            Event {
                t_ns,
                lane: Lane::Gpu,
                kind,
            },
        );
    }

    /// The folder of the screenshot PNGs of the file being written (`<trace>.shots`), created on
    /// demand; `None` unless writing.
    pub fn shots_dir(&self) -> Option<PathBuf> {
        let i = self.lock();
        let sink = i.sink.as_ref()?;
        let mut name = sink.path.file_name()?.to_os_string();
        name.push(".shots");
        let dir = sink.path.with_file_name(name);
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    /// Records a screenshot mark for the PNG at `png` (already written; it should sit in
    /// [`Self::shots_dir`], and the file is referenced relative to the trace's folder).
    pub fn screenshot_mark(
        &self,
        name: Option<&str>,
        frame: u64,
        size: (u32, u32),
        png: &Path,
    ) -> MarkResult {
        let t_ns = self.now_ns();
        if !self.wants("marks") {
            return MarkResult {
                t_ns,
                frame,
                recorded: false,
            };
        }
        let mut i = self.lock();
        let rel = i
            .sink
            .as_ref()
            .and_then(|s| s.path.parent())
            .and_then(|dir| png.strip_prefix(dir).ok())
            .unwrap_or(png)
            .to_string_lossy()
            .replace('\\', "/");
        self.emit(
            &mut i,
            Event {
                t_ns,
                lane: Lane::Main,
                kind: EventKind::Screenshot {
                    name: name.map(str::to_owned),
                    frame,
                    width: size.0,
                    height: size.1,
                    file: rel,
                },
            },
        );
        MarkResult {
            t_ns,
            frame,
            recorded: true,
        }
    }

    fn file_path(i: &Inner, file: Option<&str>) -> Result<PathBuf, TraceError> {
        let name = match file {
            Some(f) => {
                // `:` too: on Windows `C:x` is drive-relative and replaces the trace directory on join
                // (ADR-1816).
                if f.is_empty()
                    || f.contains(['/', '\\', '\0', ':'])
                    || f.contains("..")
                    || f.starts_with('.')
                {
                    return Err(TraceError::BadFileName);
                }
                if Path::new(f).extension().is_some() {
                    f.to_owned()
                } else {
                    format!("{f}.{EXTENSION}")
                }
            }
            None => format!("trace-{}.{EXTENSION}", unix_ms_now()),
        };
        Ok(i.config.dir.join(name))
    }

    fn open_sink(i: &Inner, path: PathBuf, header: &Header) -> Result<Sink, TraceError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| TraceError::Io(e.to_string()))?;
        }
        // Creating the file and its header happen here (start and save, not the frame); every
        // chunk after them goes through the writer thread.
        let mut file = std::fs::File::create(&path).map_err(|e| TraceError::Io(e.to_string()))?;
        let head = file::encode_header(header);
        std::io::Write::write_all(&mut file, &head).map_err(|e| TraceError::Io(e.to_string()))?;
        let _ = i;
        let writer = writer::Writer::spawn(Box::new(file)).map_err(|e| TraceError::Io(e.to_string()))?;
        Ok(Sink {
            path,
            writer,
            enc: events::Encoder::new(),
            pending: Vec::new(),
            bytes: head.len() as u64,
            events: 0,
            index: Vec::new(),
            last_second: 0,
            next_flush_at: SINK_FLUSH_BYTES,
        })
    }

    fn summary_of(&self, i: &Inner) -> TraceSummary {
        TraceSummary {
            state: i.state,
            file: i.sink.as_ref().map(|s| s.path.clone()),
            bytes: i.sink.as_ref().map_or(0, |s| s.bytes + s.pending.len() as u64),
            events: i.sink.as_ref().map_or(0, |s| s.events),
            dropped: i.dropped.marks + i.dropped.depth + i.dropped.unknown,
            ranges_open: u32::try_from(i.open.values().map(Vec::len).sum::<usize>()).unwrap_or(u32::MAX),
            degraded: i.degraded,
        }
    }

    /// The current state as a summary.
    #[must_use]
    pub fn status(&self) -> TraceSummary {
        let i = self.lock();
        self.summary_of(&i)
    }

    /// Starts writing `file` (a bare name; default `trace-<unix_ms>.regiatrace`). The window, when
    /// there is one, is written first, so the file starts before the call. `streams` replaces the
    /// configured streams for this recording.
    pub fn start(&self, file: Option<&str>, streams: Option<Streams>) -> Result<TraceSummary, TraceError> {
        let mut i = self.lock();
        if i.state == RecorderState::Writing {
            return Err(TraceError::Busy);
        }
        let path = Self::file_path(&i, file)?;
        let mut header = i.header.clone();
        header.start_unix_ms = self.start_unix_ms;
        let mut sink = Self::open_sink(&i, path, &header)?;
        for e in &i.window {
            sink.push(e).map_err(|e| TraceError::Io(e.to_string()))?;
        }
        sink.flush().map_err(|e| TraceError::Io(e.to_string()))?;
        if let Some(s) = streams {
            i.config.streams = s;
        }
        i.sink = Some(sink);
        i.state = RecorderState::Writing;
        i.degraded = false;
        self.refresh_mask(&i);
        Ok(self.summary_of(&i))
    }

    /// Stops writing: open ranges close with `forced_close`, the index is written, the file is
    /// closed. Returns the summary of the finished file.
    pub fn stop(&self) -> Result<TraceSummary, TraceError> {
        let t_ns = self.now_ns();
        let frame = self.frame();
        let mut i = self.lock();
        if i.state != RecorderState::Writing || i.sink.is_none() {
            return Err(TraceError::NotWriting);
        }
        let lanes: Vec<Lane> = i.open.keys().copied().collect();
        for lane in lanes {
            let stack: Vec<OpenRange> = i
                .open
                .get_mut(&lane)
                .map(|s| s.drain(..).rev().collect())
                .unwrap_or_default();
            for r in stack {
                self.emit(
                    &mut i,
                    Event {
                        t_ns,
                        lane,
                        kind: EventKind::RangeClose {
                            id: r.id,
                            frame,
                            forced: true,
                        },
                    },
                );
            }
        }
        let Some(sink) = i.sink.take() else {
            return Err(TraceError::NotWriting);
        };
        let state = if i.config.window_active() {
            RecorderState::Windowed
        } else {
            RecorderState::Idle
        };
        i.state = state;
        self.refresh_mask(&i);
        let (dropped, degraded) = (i.dropped.marks + i.dropped.depth + i.dropped.unknown, i.degraded);
        // The writer drains outside the lock, so a frame recording meanwhile never waits on IO.
        drop(i);
        let (path, bytes, events, closed) = sink.close();
        closed.map_err(|e| TraceError::Io(e.to_string()))?;
        Ok(TraceSummary {
            state,
            file: Some(path),
            bytes,
            events,
            dropped,
            ranges_open: 0,
            degraded,
        })
    }

    /// Writes the window to a separate file; a running file is not touched.
    pub fn save_window(&self, file: Option<&str>) -> Result<TraceSummary, TraceError> {
        let i = self.lock();
        if i.state == RecorderState::Idle || !i.config.window_active() {
            return Err(TraceError::NoWindow);
        }
        let name = file.map_or_else(|| format!("window-{}.{EXTENSION}", unix_ms_now()), str::to_owned);
        let path = Self::file_path(&i, Some(&name))?;
        let mut header = i.header.clone();
        header.start_unix_ms = self.start_unix_ms;
        let mut sink = Self::open_sink(&i, path, &header)?;
        let window: Vec<Event> = i.window.iter().cloned().collect();
        let state = i.state;
        let dropped = i.dropped.marks + i.dropped.depth + i.dropped.unknown;
        let ranges_open = u32::try_from(i.open.values().map(Vec::len).sum::<usize>()).unwrap_or(u32::MAX);
        let degraded = i.degraded;
        drop(i);
        for e in &window {
            sink.push(e).map_err(|e| TraceError::Io(e.to_string()))?;
        }
        let (path, bytes, events, closed) = sink.close();
        closed.map_err(|e| TraceError::Io(e.to_string()))?;
        Ok(TraceSummary {
            state,
            file: Some(path),
            bytes,
            events,
            dropped,
            ranges_open,
            degraded,
        })
    }

    /// The writer counters of the file being written (`None` unless writing): batches handed
    /// over, bytes written, sends refused by a full queue. ADR-2307's count-based check reads it.
    #[must_use]
    pub fn writer_stats(&self) -> Option<writer::WriterStats> {
        self.lock().sink.as_ref().map(|s| s.writer.stats())
    }

    /// Hands every complete chunk recorded so far to the writer (it still writes them on its own
    /// thread). Tests call it before reading a file that is still being written.
    pub fn flush(&self) -> Result<(), TraceError> {
        let mut i = self.lock();
        match i.sink.as_mut() {
            Some(sink) => sink.flush().map_err(|e| TraceError::Io(e.to_string())),
            None => Err(TraceError::NotWriting),
        }
    }

    /// A copy of the window (tests, exports).
    #[must_use]
    pub fn window_events(&self) -> Vec<Event> {
        self.lock().window.iter().cloned().collect()
    }
}

/// The process-wide recorder, `Idle` until [`configure`] or `start`. Its first use also installs
/// the renderer's frame observer (`regia_render::breadcrumbs::set_frame_observer`), so the
/// recorder follows the frame number and records the `frames` and `passes` streams with no
/// system in the app's schedule (ADR-2307).
pub fn recorder() -> &'static Recorder {
    static GLOBAL: OnceLock<Recorder> = OnceLock::new();
    static OBSERVER: std::sync::Once = std::sync::Once::new();
    let r = GLOBAL.get_or_init(|| Recorder::new(TraceConfig::default()));
    OBSERVER.call_once(|| {
        let _ = regia_render::breadcrumbs::set_frame_observer(observe_frame);
    });
    r
}

/// One pass marker copied out of a breadcrumb slot before the recorder lock is taken.
struct FreshMarker {
    frame: u64,
    submission: u64,
    tag: u16,
    open: bool,
}

/// The renderer started recording `frame` (frame thread): the previous frame ended, this one
/// began, and the passes submitted since the last call are recorded on the `gpu` lane. Costs
/// two relaxed loads when no stream wants them; IO stays on the writer thread.
fn observe_frame(frame: u64) {
    let r = recorder();
    if r.wants("frames") {
        let previous = r.frame();
        if previous != 0 && previous < frame {
            r.frame_end(previous);
        }
        r.frame_begin(frame);
    } else {
        r.set_frame(frame);
    }
    if r.wants("passes") {
        // Target `regia` at debug: [`crate::RECOMMENDED_FILTER`] keeps it (`regia=debug`) and
        // drops it when the subscriber is at info. The span is not built when nothing is listening.
        let _span = tracing::enabled!(target: "regia", tracing::Level::DEBUG)
            .then(|| tracing::debug_span!(target: "regia", "trace_capture").entered());
        r.capture_fresh_passes();
    }
}

/// The header's adapter text for `info` (spec 54 § 4.2): `name (backend, driver driver_info)`,
/// the driver part left out when the backend reports none.
#[must_use]
pub fn adapter_label(info: &wgpu::AdapterInfo) -> String {
    let driver = [info.driver.as_str(), info.driver_info.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if driver.is_empty() {
        format!("{} ({:?})", info.name, info.backend)
    } else {
        format!("{} ({:?}, {driver})", info.name, info.backend)
    }
}

/// Applies `config` to the process recorder.
pub fn configure(config: TraceConfig) {
    recorder().configure(config);
}

/// A named instant on the process recorder: no-op (one load) when no stream wants it.
pub fn mark(name: &str, data: Option<serde_json::Value>) -> MarkResult {
    recorder().mark(name, data.as_ref())
}

thread_local! {
    static THREAD_LANE: std::cell::Cell<Lane> = const { std::cell::Cell::new(Lane::Main) };
}

/// Sets the lane the calling thread's [`range`] guards use (default `main`).
pub fn set_thread_lane(lane: Lane) {
    THREAD_LANE.with(|l| l.set(lane));
}

/// A range that closes when dropped.
#[must_use = "the range closes when the guard drops"]
pub struct RangeGuard {
    id: RangeId,
}

impl Drop for RangeGuard {
    fn drop(&mut self) {
        let _ = recorder().close_range(self.id);
    }
}

/// Opens a range on the calling thread's lane; RAII.
pub fn range(name: &'static str) -> RangeGuard {
    let lane = THREAD_LANE.with(std::cell::Cell::get);
    RangeGuard {
        id: recorder().open_range(name, lane).id,
    }
}

/// Opens a range on `lane` (process recorder).
pub fn range_open(name: &str, lane: Lane) -> RangeId {
    recorder().open_range(name, lane).id
}

/// Closes a range (process recorder).
pub fn range_close(id: RangeId) -> Result<RangeClosed, UnknownRange> {
    recorder().close_range(id)
}

/// A screenshot mark on the process recorder.
pub fn screenshot_mark(name: Option<&str>, frame: u64, size: (u32, u32), png: &Path) -> MarkResult {
    recorder().screenshot_mark(name, frame, size, png)
}

#[cfg(test)]
mod queue_overflow {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};

    use super::*;
    // Private to the parent module; a glob import does not bring it into scope.
    use super::Sink;

    /// An output that waits until the test opens the gate, so the writer queue fills.
    struct Hold {
        open: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for Hold {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.open;
            let mut go = lock.lock().unwrap_or_else(PoisonError::into_inner);
            while !*go {
                go = cv.wait(go).unwrap_or_else(PoisonError::into_inner);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Opens the gate on drop, including when an assertion fails.
    struct Release(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for Release {
        fn drop(&mut self) {
            let (lock, cv) = &*self.0;
            *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
            cv.notify_all();
        }
    }

    #[test]
    fn a_full_writer_queue_marks_the_summary_degraded() {
        let open = Arc::new((Mutex::new(false), Condvar::new()));
        let _release = Release(Arc::clone(&open));
        let writer = writer::Writer::spawn(Box::new(Hold {
            open: Arc::clone(&open),
        }))
        .expect("spawn");
        // Each mark is a new second, so the sink cuts the encoder into `pending`. A fixed
        // clock leaves the string bytes in the encoder and the held-byte cap never trips.
        let tick = AtomicU64::new(0);
        let recorder = Recorder::with_clock(
            TraceConfig::default(),
            Box::new(move || tick.fetch_add(1_000_000_000, Ordering::Relaxed)),
        );
        {
            let mut inner = recorder.lock();
            inner.state = RecorderState::Writing;
            inner.sink = Some(Sink {
                path: std::path::PathBuf::from("overflow.regiatrace"),
                writer,
                enc: events::Encoder::new(),
                pending: Vec::new(),
                bytes: 0,
                events: 0,
                index: Vec::new(),
                last_second: 0,
                next_flush_at: 1,
            });
            recorder.refresh_mask(&inner);
        }
        let mut degraded = false;
        for n in 0..12_000 {
            let name = format!("n{n}");
            let data = serde_json::Value::String(format!("{n:01000}"));
            let _ = recorder.mark(&name, Some(&data));
            if recorder.status().degraded {
                degraded = true;
                break;
            }
        }
        assert!(degraded, "held writer never passed the held-byte cap");
        assert!(recorder.status().degraded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark rate limiter's name table is bounded: mark names are arbitrary (the agent's
    /// `telemetry.mark` takes any 1-64-char string), so a host that keeps marking new names must
    /// not grow the table for the process lifetime.
    #[test]
    fn distinct_mark_names_are_capped() {
        let r = Recorder::new(TraceConfig {
            window_seconds: 5.0,
            ..TraceConfig::default()
        });
        for n in 0..MARK_RATE_NAMES_MAX + 64 {
            r.mark(&format!("m{n}"), None);
        }
        let len = r.lock().mark_rate.len();
        assert_eq!(
            len, MARK_RATE_NAMES_MAX,
            "the limiter's name table stays at its cap"
        );
    }
}
