// SPDX-License-Identifier: MPL-2.0
//! Profiling (`docs/specs/10-core.md` § 1.3): the [`scope!`] macro and the in-process [`FrameProfiler`].
//!
//! Every `tracing` span is a profiling scope. [`FrameProfiler`] is a `tracing_subscriber` layer that
//! sums the CPU time (enter → exit, inclusive of children) of each span name per frame and keeps the last
//! [`HISTORY_FRAMES`] frames for the editor profiler panel and the in-game stats overlay. A span exit
//! adds its time to a per-thread table; [`FrameProfiler::end_frame`] folds those tables once, so exits
//! do not take the frame-state lock. With flame capture enabled ([`FrameProfiler::set_flame_enabled`])
//! each exit also folds its call path into that thread's tree, up to [`MAX_FLAME_EDGES_PER_FRAME`] exits
//! per frame, and [`FrameProfiler::flame_graph`] merges the trees into a [`FlameGraph`]. With the
//! timeline on ([`FrameProfiler::set_timeline_enabled`], ADR-7325) each exit also appends its thread,
//! name, start, end and depth to a reused per-thread buffer, up to [`MAX_TIMELINE_SPANS_PER_FRAME`]
//! spans per frame (the rest are counted as dropped), and [`FrameProfiler::timeline_frame`] reads one
//! retained frame per thread. Install it with
//! [`crate::log::LogConfig::profiler`] and call [`FrameProfiler::end_frame`] once per frame. With the
//! `tracy` feature the same spans also stream to Tracy.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::Subscriber;
use tracing::span::Id;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

#[doc(hidden)]
pub use tracing as __tracing;

/// Opens a profiling scope that lasts until the end of the enclosing block: an `info`-level `tracing`
/// span named `$name` (a string literal), with optional `tracing` fields.
///
/// ```
/// fn update_particles() {
///     regia_core::profiling::scope!("update_particles");
///     // ... work measured until the end of the function ...
/// }
/// # update_particles();
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! __profiling_scope {
    ($name:expr $(, $($fields:tt)*)?) => {
        let _regia_profiling_scope =
            $crate::profiling::__tracing::info_span!($name $(, $($fields)*)?).entered();
    };
}

#[doc(inline)]
pub use crate::__profiling_scope as scope;

/// Per-span-name times of one frame, most expensive first ([`FrameProfiler::last_frame_split`]).
pub type SpanTimes = Vec<(&'static str, Duration)>;

/// Number of frames [`FrameProfiler`] keeps.
pub const HISTORY_FRAMES: usize = 300;

/// One recorded frame.
#[derive(Debug, Clone)]
struct FrameRecord {
    /// Wall time since the previous `end_frame`.
    duration: Duration,
    /// Per span name: summed times and number of enter/exit pairs.
    spans: Vec<SpanTime>,
    /// Call tree of the flame exits kept this frame. Empty when capture was off.
    flame: Arc<flamegraph::FrameFold>,
    /// Frames ended before this one by this profiler (since it was made or cleared).
    number: u64,
    /// Start and end of the frame, ns since the profiler's epoch.
    start_ns: u64,
    end_ns: u64,
    /// Timeline spans of this frame, every thread, in exit order. Empty when the timeline was off.
    timeline: Vec<TimelineEvent>,
    /// `(thread index, thread name)` of the threads with timeline spans this frame.
    timeline_threads: Vec<(u32, Arc<str>)>,
    /// Timeline spans past [`MAX_TIMELINE_SPANS_PER_FRAME`] this frame, not kept.
    timeline_dropped: u64,
}

impl Default for FrameRecord {
    fn default() -> Self {
        Self {
            duration: Duration::ZERO,
            spans: Vec::new(),
            flame: flamegraph::FrameFold::empty_arc(),
            number: 0,
            start_ns: 0,
            end_ns: 0,
            timeline: Vec::new(),
            timeline_threads: Vec::new(),
            timeline_dropped: 0,
        }
    }
}

/// A span's name in the timeline: the span's own name, or an ECS system slot ([`system_name`]),
/// resolved when the frame is read.
#[derive(Debug, Clone, Copy)]
enum TimelineLabel {
    Static(&'static str),
    System(u32),
}

/// One timeline span as recorded on a thread.
#[derive(Debug, Clone, Copy)]
struct TimelineEvent {
    thread: u32,
    depth: u16,
    label: TimelineLabel,
    /// Enter and exit, ns since the profiler's epoch.
    start_ns: u64,
    end_ns: u64,
}

/// Most timeline spans kept per frame while the timeline is on (all threads); later exits that
/// frame are counted in [`TimelineFrame::dropped`]. With 48 bytes a span and [`HISTORY_FRAMES`]
/// frames, a history full at this cap holds about 118 MB; a frame that runs fewer spans holds less.
pub const MAX_TIMELINE_SPANS_PER_FRAME: usize = 8192;

/// One span of a [`TimelineFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineSpan {
    /// The span name (an ECS system's `module::path::function`).
    pub name: Cow<'static, str>,
    /// Enter, ns from the frame's start (0 for a span entered in an earlier frame).
    pub start_ns: u64,
    /// Exit, ns from the frame's start; never before `start_ns`.
    pub end_ns: u64,
    /// Recorded spans of the same thread open around it (0 = a root on its thread).
    pub depth: u16,
}

/// One thread's spans in a [`TimelineFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineThread {
    /// The profiler's index of the thread (in order of first recording; stable while it runs).
    pub index: u32,
    /// The thread's name, or `thread <index>` for an unnamed thread.
    pub name: Arc<str>,
    /// Its spans, by start, then depth (outer first), then end.
    pub spans: Vec<TimelineSpan>,
}

/// The per-thread timeline of one retained frame ([`FrameProfiler::timeline_frame`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineFrame {
    /// The frame's number: frames ended before it since the profiler was made or cleared.
    pub number: u64,
    /// Wall time of the frame, ns.
    pub duration_ns: u64,
    /// Threads that recorded spans this frame, by index.
    pub threads: Vec<TimelineThread>,
    /// Spans past [`MAX_TIMELINE_SPANS_PER_FRAME`] this frame, not kept.
    pub dropped: u64,
}

impl TimelineFrame {
    /// Spans kept, every thread.
    #[must_use]
    pub fn span_count(&self) -> usize {
        self.threads.iter().map(|t| t.spans.len()).sum()
    }
}

/// One span name's times in a [`FrameRecord`].
#[derive(Debug, Clone, Copy)]
struct SpanTime {
    name: &'static str,
    /// Enter → exit, inclusive of child spans.
    total: Duration,
    /// `total` minus the time of child spans entered on the same thread inside it.
    self_time: Duration,
    calls: u32,
    /// The part of `total` recorded on a frame thread ([`mark_main_thread`],
    /// [`mark_render_thread`]); the rest ran on other threads (workers).
    frame_thread: Duration,
}

/// Most flame edges kept per frame while capture is on (one per span exit).
const MAX_EDGES_PER_FRAME: usize = 1 << 16;

/// Most flame edges kept per frame while capture is on (one per span exit).
///
/// Not part of the host API. Exits past this cap are dropped.
#[doc(hidden)]
pub const MAX_FLAME_EDGES_PER_FRAME: usize = MAX_EDGES_PER_FRAME;

#[derive(Debug, Default, Clone, Copy)]
struct Accum {
    total: Duration,
    self_time: Duration,
    calls: u32,
    frame_thread: Duration,
}

#[derive(Debug)]
struct State {
    current: HashMap<&'static str, Accum>,
    frames: VecDeque<FrameRecord>,
    last_end: Instant,
    /// Frames ended since the profiler was made or cleared.
    ended: u64,
}

/// Buffers [`FrameProfiler::end_frame`] reuses, so a warmed-up frame allocates nothing.
#[derive(Default)]
struct EndScratch {
    shards: Vec<Arc<Shard>>,
    batch: Vec<(&'static str, Accum)>,
    timeline: Vec<TimelineEvent>,
    threads: Vec<(u32, Arc<str>)>,
}

/// Span totals and the flame tree for one thread of one [`FrameProfiler`].
///
/// The owning thread is the only writer between folds. [`FrameProfiler::end_frame`] drains every
/// shard once.
struct Shard {
    totals: Mutex<HashMap<&'static str, Accum>>,
    flame: Mutex<flamegraph::FrameFold>,
    /// The profiler's index of the owning thread.
    thread: u32,
    /// The owning thread's name, read on its first timeline span.
    thread_name: std::sync::OnceLock<Arc<str>>,
    /// Timeline spans since the last fold (ADR-7325). Keeps its capacity across frames.
    timeline: Mutex<Vec<TimelineEvent>>,
}

struct Shared {
    id: usize,
    enabled: AtomicBool,
    flame_enabled: AtomicBool,
    state: Mutex<State>,
    /// Shards registered by threads that have recorded into this profiler.
    shards: Mutex<Vec<Arc<Shard>>>,
    /// Flame edges accepted into the open frame. The cap is checked here, before a path is built.
    edge_count: AtomicUsize,
    /// Locks of `state` taken to fold one frame. Span exits do not take `state`.
    state_locks: AtomicU64,
    /// Call-path buffers built for flame edges. The fold stores names in a reused tree, so this
    /// stays zero.
    path_allocs: AtomicU64,
    /// Flame edges copied while `state` was held inside [`FrameProfiler::flame_graph`].
    flame_copied_under_lock: AtomicU64,
    /// Per-thread timeline recording (ADR-7325).
    timeline_enabled: AtomicBool,
    /// Timeline spans accepted into the open frame (the cap is checked here).
    timeline_count: AtomicUsize,
    /// Timeline spans refused in the open frame.
    timeline_dropped: AtomicU64,
    /// Timeline spans refused since the profiler was made.
    timeline_dropped_total: AtomicU64,
    /// Next thread index.
    next_thread: AtomicU32,
    /// Time zero of the timeline's ns stamps.
    epoch: Instant,
    /// `end_frame`'s reused buffers.
    scratch: Mutex<EndScratch>,
}

/// A span opened on this thread and not yet exited.
struct Open {
    profiler: usize,
    span: u64,
    name: &'static str,
    start: Instant,
    /// Time of the spans of the same profiler that ran inside this one on this thread.
    children: Duration,
    /// The ECS system slot ([`system_name`]) of a `bevy_ecs` system span, else [`NO_SYSTEM`].
    system: u32,
    /// The ECS system slot is past [`MAX_SYSTEMS`]: no per-slot counter, the system is timed by
    /// its interned name like any named span (ADR-7409).
    over_cap: bool,
    /// The host-section slot ([`host_span_name`]) of a host span, else [`NO_SYSTEM`].
    host: u32,
    /// A host span entered on the main thread with no host span open around it.
    host_top: bool,
    /// Whether the thread was marked main when the span opened; the exit charges the same
    /// bucket the entry decided `host_top` for, even if the mark flips in between.
    main: bool,
    /// Whether the thread was a frame thread (main or render) when the span opened.
    frame_thread: bool,
    /// A `bevy_ecs` scheduling span other than `system` (`schedule`, `multithreaded executor`, ...):
    /// it nests whole schedules, so recording it would top the frame table with no cause.
    bevy_umbrella: bool,
}

impl Open {
    /// Whether this span is recorded in the flame tree and in its children's call paths: any
    /// span timed by its name, and an ECS system past the slot cap (timed by its interned
    /// name). A system with a slot is timed per slot instead, and skipped (ADR-7409).
    fn flame_visible(&self) -> bool {
        self.over_cap || (self.system == NO_SYSTEM && !self.bevy_umbrella)
    }
}

const NO_SYSTEM: u32 = u32::MAX;

/// Most distinct ECS system names [`FrameProfiler`] keeps per-slot counters for. Names past
/// this cap are still interned and shown by their own name in the per-span records (timeline
/// and flame) and timed by name in the frame table; only the aggregated per-slot counters
/// ([`take_system_times`]) stay capped. See [`systems_over_cap`].
pub const MAX_SYSTEMS: usize = 2048;

static SYSTEM_NS: [std::sync::atomic::AtomicU64; MAX_SYSTEMS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MAX_SYSTEMS];
static SYSTEM_CALLS: [std::sync::atomic::AtomicU32; MAX_SYSTEMS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAX_SYSTEMS];
static SYSTEM_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Distinct system names interned past [`MAX_SYSTEMS`] (read by [`systems_over_cap`]).
static SYSTEMS_OVER_CAP: AtomicUsize = AtomicUsize::new(0);

struct SystemNames {
    names: Vec<&'static str>,
    index: HashMap<&'static str, u32>,
}

static SYSTEM_NAMES: Mutex<Option<SystemNames>> = Mutex::new(None);

/// The slot of system `name`, interned on first sight (once per system, when bevy creates its
/// span). Every distinct name gets a slot; slots from [`MAX_SYSTEMS`] on have no per-slot
/// counter, and the system is then timed by its interned name like any named span (ADR-7409).
/// The interned copy lives as long as the table (the process), like the span-name keys the
/// frame table already keeps.
fn system_slot(name: &str) -> u32 {
    let mut guard = SYSTEM_NAMES.lock();
    let names = guard.get_or_insert_with(|| SystemNames {
        names: Vec::new(),
        index: HashMap::new(),
    });
    if let Some(&slot) = names.index.get(name) {
        return slot;
    }
    let interned: &'static str = Box::leak(name.to_owned().into_boxed_str());
    let slot = names.names.len() as u32;
    names.names.push(interned);
    names.index.insert(interned, slot);
    SYSTEM_COUNT.store(names.names.len(), Ordering::Release);
    if names.names.len() > MAX_SYSTEMS {
        SYSTEMS_OVER_CAP.store(names.names.len() - MAX_SYSTEMS, Ordering::Release);
    }
    slot
}

/// Number of ECS systems [`FrameProfiler`] has seen (slots `0..system_count()`).
///
/// Systems are timed when `bevy_ecs` creates its per-system `system` spans (the `trace` feature,
/// on in the workspace) and the process subscriber includes a [`FrameProfiler`]. Zero otherwise.
/// Slots from [`MAX_SYSTEMS`] on are interned by name only: they have no per-slot counter
/// ([`take_system_times`] stops at the cap; [`systems_over_cap`] counts them).
#[must_use]
pub fn system_count() -> usize {
    SYSTEM_COUNT.load(Ordering::Acquire)
}

/// Distinct ECS system names interned past [`MAX_SYSTEMS`]: these systems show by their own
/// name in the per-span records (timeline and flame) and are timed by name in the frame table,
/// but the aggregated per-slot counters stay capped.
#[must_use]
pub fn systems_over_cap() -> usize {
    SYSTEMS_OVER_CAP.load(Ordering::Acquire)
}

/// The name of system slot `slot` (`module::path::function`, as bevy names it).
#[must_use]
pub fn system_name(slot: u32) -> Option<&'static str> {
    SYSTEM_NAMES
        .lock()
        .as_ref()
        .and_then(|n| n.names.get(slot as usize).copied())
}

/// Moves every system's run time and run count since the last call into `out` as
/// `(slot, time, runs)`, skipping systems that did not run. Telemetry calls this once a frame.
/// Allocates only while `out` grows to the number of systems.
pub fn take_system_times(out: &mut Vec<(u32, Duration, u32)>) {
    out.clear();
    for slot in 0..system_count().min(MAX_SYSTEMS) {
        // The pair is not one atomic: an exit racing this call (time added, count not yet) is
        // split across two drains, `(time, 0)` now and `(0, 1)` on the next. Totals over drains
        // are exact; a per-drain mean must allow a zero count (ADR-2396).
        let ns = SYSTEM_NS[slot].swap(0, Ordering::Relaxed);
        let calls = SYSTEM_CALLS[slot].swap(0, Ordering::Relaxed);
        if calls == 0 && ns == 0 {
            continue;
        }
        out.push((slot as u32, Duration::from_nanos(ns), calls));
    }
}

/// Slot of a `bevy_ecs` system span, kept in the span's extensions.
struct SystemSlot(u32);

/// Most distinct host section names timed ([`set_host_span_prefixes`]).
pub const MAX_HOST_SPANS: usize = 256;

static HOST_MAIN_NS: [std::sync::atomic::AtomicU64; MAX_HOST_SPANS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MAX_HOST_SPANS];
static HOST_MAIN_CALLS: [std::sync::atomic::AtomicU32; MAX_HOST_SPANS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAX_HOST_SPANS];
static HOST_OTHER_NS: [std::sync::atomic::AtomicU64; MAX_HOST_SPANS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MAX_HOST_SPANS];
static HOST_OTHER_CALLS: [std::sync::atomic::AtomicU32; MAX_HOST_SPANS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAX_HOST_SPANS];
static HOST_MAIN_TOP_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HOST_COUNT: AtomicUsize = AtomicUsize::new(0);
/// The host's prefix list, set once ([`set_host_span_prefixes`]). Unset means
/// [`DEFAULT_HOST_SPAN_PREFIXES`] and [`default_host_span_prefix`]. A successful set replaces
/// both. A span enter reads it without a lock.
static HOST_PREFIXES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
static HOST_NAMES: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());

/// Span-target prefixes timed as host sections when the host sets none, besides the running
/// executable's own name ([`default_host_span_prefix`]). Kept for hosts written before the
/// executable's name was the default.
pub const DEFAULT_HOST_SPAN_PREFIXES: &[&str] = &["do27::"];

/// The default host-section prefix: the running executable's name as a crate path
/// (`direct-order.exe` → `direct_order::`), which is what the game's own spans use as their
/// target. `None` when that name is unknown, empty, starts with `regia`, or starts with a
/// reserved crate name (`wgpu`, `naga`, `winit`, `egui`, `bevy`, `tracing`, `std`, `core`,
/// `alloc`, `rapier`, `kira`). Non-alphanumeric characters in the stem become `_`.
#[must_use]
pub fn default_host_span_prefix() -> Option<&'static str> {
    static PREFIX: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PREFIX
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            let stem = exe.file_stem()?.to_str()?;
            let name: String = stem
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            // The engine's own spans (`regia::…`, `regia_render::…`) are never host sections.
            // Nor a dependency's: an executable named like one (`tracing`, `wgpu`) must not
            // swallow that crate's spans.
            let reserved = [
                "wgpu", "naga", "winit", "egui", "bevy", "tracing", "std", "core", "alloc", "rapier", "kira",
            ];
            (!name.is_empty() && !name.starts_with("regia") && !reserved.iter().any(|r| name.starts_with(r)))
                .then(|| format!("{name}::"))
        })
        .as_deref()
}

thread_local! {
    /// This thread runs `App::update` ([`mark_main_thread`]).
    static IS_MAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread records the renderer's frames ([`mark_render_thread`]).
    static IS_RENDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Host spans open on this thread.
    static HOST_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

mod thread_clock;

mod run_totals;
pub use run_totals::{
    DEFAULT_REPORT_ROWS, RunTotals, SPAN_REPORT_ENV, SpanKind, SpanRow, parse_report_rows,
    report_rows_from_env,
};

mod flamegraph;
pub use flamegraph::{FlameGraph, FlameNode};

/// CPU time the calling thread has run so far (Linux and Android: `CLOCK_THREAD_CPUTIME_ID`,
/// current to the call). `None` where the platform has no such clock. Time spent descheduled
/// or waiting does not count, so the difference of two readings is the work done between them.
#[must_use]
pub fn thread_cpu_time() -> Option<Duration> {
    thread_clock::thread_cpu_time()
}

/// Marks the calling thread as the main (frame) thread: its host sections are reported apart
/// from other threads'. `regia_app::App::update` calls this.
pub fn mark_main_thread() {
    let _ = IS_MAIN.try_with(|m| m.set(true));
}

/// Marks the calling thread as the render thread (`regia_render::with_render_thread`): like the
/// main thread, its spans are frame-thread time in [`FrameProfiler::last_frame_split`], not
/// background work. Host sections are unaffected (they stay keyed on [`mark_main_thread`]).
pub fn mark_render_thread() {
    let _ = IS_RENDER.try_with(|m| m.set(true));
}

/// Whether the calling thread is a frame thread (marked main or render).
fn on_frame_thread() -> bool {
    IS_MAIN.try_with(std::cell::Cell::get).unwrap_or(false)
        || IS_RENDER.try_with(std::cell::Cell::get).unwrap_or(false)
}

/// Sets the span-target prefixes whose spans telemetry reports as host sections (by span name).
/// Call it once at startup, before the spans are created: the list can be set once per process,
/// and a later call returns `false` and changes nothing.
///
/// When this has not succeeded, a span matches [`DEFAULT_HOST_SPAN_PREFIXES`] (`do27::`) or
/// [`default_host_span_prefix`] (the executable's name plus `::`, when that name is usable).
/// A successful call replaces both.
pub fn set_host_span_prefixes<I, P>(prefixes: I) -> bool
where
    I: IntoIterator<Item = P>,
    P: Into<String>,
{
    HOST_PREFIXES
        .set(prefixes.into_iter().map(Into::into).collect())
        .is_ok()
}

/// Whether `target` is a host section.
///
/// `set` is the process list. `None` (the list was never set) matches
/// [`DEFAULT_HOST_SPAN_PREFIXES`] and `executable_prefix`. `Some` replaces both, including an
/// empty list, which matches nothing.
fn host_prefixes_match(target: &str, set: Option<&[String]>, executable_prefix: Option<&str>) -> bool {
    match set {
        Some(list) => list.iter().any(|prefix| target.starts_with(prefix.as_str())),
        None => {
            DEFAULT_HOST_SPAN_PREFIXES
                .iter()
                .any(|prefix| target.starts_with(prefix))
                || executable_prefix.is_some_and(|prefix| target.starts_with(prefix))
        }
    }
}

fn host_prefix_matches(target: &str) -> bool {
    host_prefixes_match(
        target,
        HOST_PREFIXES.get().map(Vec::as_slice),
        default_host_span_prefix(),
    )
}

/// The slot of host span name `name` (target `target`), interned on first sight.
fn host_slot(name: &'static str, target: &'static str) -> u32 {
    let mut names = HOST_NAMES.lock();
    if let Some(i) = names.iter().position(|&(n, _)| n == name) {
        return i as u32;
    }
    if names.len() >= MAX_HOST_SPANS {
        return NO_SYSTEM;
    }
    names.push((name, target));
    HOST_COUNT.store(names.len(), Ordering::Release);
    (names.len() - 1) as u32
}

/// Number of host section names seen (slots `0..host_span_count()`).
#[must_use]
pub fn host_span_count() -> usize {
    HOST_COUNT.load(Ordering::Acquire)
}

/// The span name and target of host slot `slot`.
#[must_use]
pub fn host_span_name(slot: u32) -> Option<(&'static str, &'static str)> {
    HOST_NAMES.lock().get(slot as usize).copied()
}

/// Host-section times since the last call, taken once a frame by telemetry: `main` and `other`
/// get `(slot, inclusive time, entries)` for the main thread and every other thread; the return
/// value is the main thread's time inside top-level host sections (nested ones counted once).
pub fn take_host_times(
    main: &mut Vec<(u32, Duration, u32)>,
    other: &mut Vec<(u32, Duration, u32)>,
) -> Duration {
    main.clear();
    other.clear();
    for slot in 0..host_span_count().min(MAX_HOST_SPANS) {
        for (calls, ns, out) in [
            (&HOST_MAIN_CALLS[slot], &HOST_MAIN_NS[slot], &mut *main),
            (&HOST_OTHER_CALLS[slot], &HOST_OTHER_NS[slot], &mut *other),
        ] {
            let nanos = ns.swap(0, Ordering::Relaxed);
            let n = calls.swap(0, Ordering::Relaxed);
            if n > 0 || nanos > 0 {
                out.push((slot as u32, Duration::from_nanos(nanos), n));
            }
        }
    }
    Duration::from_nanos(HOST_MAIN_TOP_NS.swap(0, Ordering::Relaxed))
}

/// Host-section slot of a span, kept in its extensions.
struct HostSlot(u32);

fn is_system_span(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.name() == "system" && metadata.target().starts_with("bevy_ecs")
}

/// Reads a span's `name` field.
struct NameVisitor(Option<String>);

impl tracing::field::Visit for NameVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "name" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "name" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

thread_local! {
    /// Spans currently entered on this thread. Grows to the deepest nesting once, then never allocates.
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static OPEN: RefCell<Vec<Open>> = const { RefCell::new(Vec::new()) };
    /// Reused call path for a flame exit. Names are copied into the thread's tree; the buffer is not.
    #[allow(clippy::missing_const_for_thread_local)]
    static FLAME_PATH: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// The live flame contexts ([`flame_context`]), one `(token, name)` per live guard, oldest first:
/// while one is live, a root span exiting on a compute task-pool thread is filed under the newest
/// name in the flame tree. A stack, not one slot, so guards may drop in any order (ADR-7427). Its
/// capacity grows to the deepest nesting once; pushing and removing after that never allocates.
static FLAME_CONTEXTS: parking_lot::RwLock<Vec<(u64, &'static str)>> = parking_lot::RwLock::new(Vec::new());

/// The token of the next [`FlameContextGuard`].
static NEXT_FLAME_TOKEN: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Whether this thread is a compute task-pool worker, read once per thread.
    static TASK_POOL_THREAD: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether a thread name is a compute task-pool worker, where a multi-threaded schedule runs its
/// ECS systems: the engine's own pool (`crate::tasks`: `regia-compute (n)`) or bevy's default
/// (`Compute Task Pool (n)`). The async-compute pool (`regia-async (n)`, `Async Compute Task Pool
/// (n)`) and the IO pool run editor imports, network polls and loaders, which are not play work,
/// so they are not context workers (ADR-7427).
fn is_task_pool_thread_name(name: &str) -> bool {
    name.starts_with("regia-compute") || name.starts_with("Compute Task Pool")
}

fn on_task_pool_thread() -> bool {
    TASK_POOL_THREAD
        .try_with(|cell| {
            if let Some(known) = cell.get() {
                return known;
            }
            let pool = std::thread::current()
                .name()
                .is_some_and(is_task_pool_thread_name);
            cell.set(Some(pool));
            pool
        })
        .unwrap_or(false)
}

/// Ends its flame context when dropped ([`flame_context`]). Guards may drop in any order and on
/// any thread: the context is then the newest one still held, and none once every guard dropped.
#[must_use = "the context ends when the guard drops"]
pub struct FlameContextGuard {
    token: u64,
}

impl Drop for FlameContextGuard {
    fn drop(&mut self) {
        let mut contexts = FLAME_CONTEXTS.write();
        if let Some(pos) = contexts.iter().rposition(|&(token, _)| token == self.token) {
            contexts.remove(pos);
        }
    }
}

/// Files the work of the compute task pool's threads under `name` in the flame tree until the
/// guard drops (ADR-9740, ADR-7427). The frame profiler builds a span's call path from the spans
/// entered on its own thread, so an ECS system that a multi-threaded schedule runs on a compute
/// worker would otherwise show as a root, outside the span that ran the schedule. The editor sets
/// the context around a play frame (`pie_frame`), so play systems show under play. Roots on the
/// async-compute and IO pools (asset imports, network polls) are not filed under it. Costs nothing
/// unless flame capture is on; then one read per root span exit on a compute worker.
/// Process-wide: every [`FrameProfiler`] sees the same context. Contexts nest: the newest live
/// guard's name is the context, whatever order the guards drop in.
pub fn flame_context(name: &'static str) -> FlameContextGuard {
    let token = NEXT_FLAME_TOKEN.fetch_add(1, Ordering::Relaxed);
    FLAME_CONTEXTS.write().push((token, name));
    FlameContextGuard { token }
}

/// The context a compute worker's root span is filed under now: the newest live guard's name.
fn current_flame_context() -> Option<&'static str> {
    FLAME_CONTEXTS.read().last().map(|&(_, name)| name)
}

struct LocalShard {
    profiler: usize,
    shard: Arc<Shard>,
}

thread_local! {
    /// Shards this thread has registered, one per [`FrameProfiler`].
    #[allow(clippy::missing_const_for_thread_local)]
    static THREAD_SHARDS: RefCell<Vec<LocalShard>> = const { RefCell::new(Vec::new()) };
}

/// How many profiler tables the calling thread holds (tests).
#[cfg(test)]
fn local_shard_count() -> usize {
    THREAD_SHARDS.with(|cell| cell.borrow().len())
}

/// The calling thread's shard for `shared`, registered on first use. `None` while the thread's
/// locals are being destroyed: a span that exits from another thread-local's destructor is then
/// not recorded (`with` would panic there, and a panic in a thread-local destructor aborts).
fn thread_shard(shared: &Shared) -> Option<Arc<Shard>> {
    THREAD_SHARDS
        .try_with(|cell| {
            let mut locals = cell.borrow_mut();
            if let Some(found) = locals.iter().find(|s| s.profiler == shared.id) {
                return Arc::clone(&found.shard);
            }
            // A table only this list still holds belongs to a dropped profiler (a live one keeps
            // every live thread's table in its `shards`), so registering a new table first lets
            // those go: the list stays as long as the live profilers, not every profiler this
            // thread ever recorded into (corr-core L9).
            locals.retain(|local| Arc::strong_count(&local.shard) > 1);
            let shard = Arc::new(Shard {
                totals: Mutex::new(HashMap::new()),
                flame: Mutex::new(flamegraph::FrameFold::new()),
                thread: shared.next_thread.fetch_add(1, Ordering::Relaxed),
                thread_name: std::sync::OnceLock::new(),
                timeline: Mutex::new(Vec::new()),
            });
            shared.shards.lock().push(Arc::clone(&shard));
            locals.push(LocalShard {
                profiler: shared.id,
                shard: Arc::clone(&shard),
            });
            shard
        })
        .ok()
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// In-process frame profiler: per-span-name CPU time for the last [`HISTORY_FRAMES`] frames.
///
/// Cloning is cheap and every clone shares the same data, so one clone can be installed as a layer
/// (via [`crate::log::LogConfig::profiler`]) while another is kept as an ECS resource by `regia_app`.
/// Recording takes no allocation per span after warm-up: span names are the `&'static str` from the
/// span's metadata, and per-thread bookkeeping reuses its buffer.
#[derive(Clone)]
pub struct FrameProfiler {
    shared: Arc<Shared>,
}

impl Default for FrameProfiler {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FrameProfiler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameProfiler")
            .field("enabled", &self.is_enabled())
            .field("frames", &self.frame_count())
            .finish()
    }
}

impl FrameProfiler {
    /// Creates an enabled profiler with an empty history.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            shared: Arc::new(Shared {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                enabled: AtomicBool::new(true),
                flame_enabled: AtomicBool::new(false),
                state: Mutex::new(State {
                    current: HashMap::new(),
                    frames: VecDeque::with_capacity(HISTORY_FRAMES),
                    last_end: now,
                    ended: 0,
                }),
                shards: Mutex::new(Vec::new()),
                edge_count: AtomicUsize::new(0),
                state_locks: AtomicU64::new(0),
                path_allocs: AtomicU64::new(0),
                flame_copied_under_lock: AtomicU64::new(0),
                timeline_enabled: AtomicBool::new(false),
                timeline_count: AtomicUsize::new(0),
                timeline_dropped: AtomicU64::new(0),
                timeline_dropped_total: AtomicU64::new(0),
                next_thread: AtomicU32::new(0),
                epoch: now,
                scratch: Mutex::new(EndScratch::default()),
            }),
        }
    }

    /// Times the frame-state lock was taken to record a span exit or to fold a frame.
    ///
    /// Not part of the host API. Span exits add to a per-thread table and do not take this
    /// lock; [`Self::end_frame`] takes it once to fold those tables.
    #[doc(hidden)]
    #[must_use]
    pub fn state_lock_count(&self) -> u64 {
        self.shared.state_locks.load(Ordering::Relaxed)
    }

    /// Call-path buffers built for flame edges.
    ///
    /// Not part of the host API. Always 0: call paths go through one reused per-thread buffer
    /// and the fold copies names into a reused tree, so nothing increments it. Past
    /// [`MAX_FLAME_EDGES_PER_FRAME`] edges a frame builds no path at all.
    #[doc(hidden)]
    #[must_use]
    pub fn flame_path_allocs(&self) -> u64 {
        self.shared.path_allocs.load(Ordering::Relaxed)
    }

    /// Flame edges copied while the frame-state lock was held, across every [`Self::flame_graph`].
    ///
    /// Not part of the host API. The tree is folded after that lock is released, so this stays zero.
    #[doc(hidden)]
    #[must_use]
    pub fn flame_edges_copied_under_lock(&self) -> u64 {
        self.shared.flame_copied_under_lock.load(Ordering::Relaxed)
    }

    /// Flame edges kept in the newest recorded frame.
    ///
    /// Not part of the host API.
    #[doc(hidden)]
    #[must_use]
    pub fn flame_edges_retained(&self) -> u64 {
        self.shared
            .state
            .lock()
            .frames
            .back()
            .map(|f| f.flame.exits())
            .unwrap_or(0)
    }

    /// Per-thread tables registered with this profiler.
    ///
    /// Not part of the host API. A thread that has exited keeps its table until the next
    /// [`Self::end_frame`] folds it.
    #[doc(hidden)]
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shared.shards.lock().len()
    }

    /// Locks the frame state and counts the acquisition. [`Self::end_frame`] uses this once per
    /// frame; span exits do not.
    fn lock_state(&self) -> parking_lot::MutexGuard<'_, State> {
        self.shared.state_locks.fetch_add(1, Ordering::Relaxed);
        self.shared.state.lock()
    }

    /// Starts or stops recording. Spans entered while disabled are not measured.
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::Relaxed);
    }

    /// `true` while recording.
    pub fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// Starts or stops flame-graph capture (the call paths behind [`Self::flame_graph`]). Off by
    /// default. While on, each span exit folds its ancestor names into a per-thread tree, up to
    /// [`MAX_FLAME_EDGES_PER_FRAME`] exits per frame; further exits in that frame are dropped.
    pub fn set_flame_enabled(&self, enabled: bool) {
        self.shared.flame_enabled.store(enabled, Ordering::Relaxed);
    }

    /// `true` while flame-graph capture is on.
    pub fn flame_enabled(&self) -> bool {
        self.shared.flame_enabled.load(Ordering::Relaxed)
    }

    /// Starts or stops the per-thread timeline (ADR-7325). Off by default; while off a span exit
    /// does no timeline work. While on, each exit appends its thread, name, start, end and depth to
    /// a reused per-thread buffer, up to [`MAX_TIMELINE_SPANS_PER_FRAME`] spans per frame (later
    /// exits that frame are counted as dropped), and [`Self::end_frame`] folds the buffers into the
    /// history.
    pub fn set_timeline_enabled(&self, enabled: bool) {
        self.shared.timeline_enabled.store(enabled, Ordering::Relaxed);
    }

    /// `true` while the per-thread timeline is recorded.
    pub fn timeline_enabled(&self) -> bool {
        self.shared.timeline_enabled.load(Ordering::Relaxed)
    }

    /// Timeline spans dropped by the per-frame cap since the profiler was made.
    #[must_use]
    pub fn timeline_dropped_total(&self) -> u64 {
        self.shared.timeline_dropped_total.load(Ordering::Relaxed)
    }

    /// The numbers of the retained frames, oldest first (`frame_times` order); a frame's number
    /// is the count of frames ended before it.
    #[must_use]
    pub fn frame_numbers(&self) -> Vec<u64> {
        self.shared.state.lock().frames.iter().map(|f| f.number).collect()
    }

    /// The number of the newest retained frame.
    #[must_use]
    pub fn newest_frame_number(&self) -> Option<u64> {
        self.shared.state.lock().frames.back().map(|f| f.number)
    }

    /// The per-thread timeline of retained frame `number` (`None`: the newest), or `None` when that
    /// frame is not in the history. A frame recorded with the timeline off has no threads. Each
    /// thread's spans are sorted by start, then depth, then end.
    #[must_use]
    pub fn timeline_frame(&self, number: Option<u64>) -> Option<TimelineFrame> {
        // Copy the frame's events under the lock; names and the sort come after it.
        let (number, start_ns, end_ns, mut events, threads, dropped) = {
            let state = self.shared.state.lock();
            let frame = match number {
                None => state.frames.back()?,
                Some(n) => state.frames.iter().find(|f| f.number == n)?,
            };
            (
                frame.number,
                frame.start_ns,
                frame.end_ns,
                frame.timeline.clone(),
                frame.timeline_threads.clone(),
                frame.timeline_dropped,
            )
        };
        events.sort_unstable_by_key(|e| (e.thread, e.start_ns, e.depth, e.end_ns));
        let mut out: Vec<TimelineThread> = Vec::new();
        for event in events {
            if out.last().is_none_or(|t| t.index != event.thread) {
                let name = threads
                    .iter()
                    .find(|(index, _)| *index == event.thread)
                    .map_or_else(
                        || Arc::from(format!("thread {}", event.thread)),
                        |(_, n)| Arc::clone(n),
                    );
                out.push(TimelineThread {
                    index: event.thread,
                    name,
                    spans: Vec::new(),
                });
            }
            let name: Cow<'static, str> = match event.label {
                TimelineLabel::Static(name) => Cow::Borrowed(name),
                TimelineLabel::System(slot) => {
                    Cow::Owned(system_name(slot).map_or_else(|| "system".to_owned(), |n| n.to_string()))
                }
            };
            let start = event.start_ns.saturating_sub(start_ns);
            let end = event.end_ns.saturating_sub(start_ns).max(start);
            if let Some(thread) = out.last_mut() {
                thread.spans.push(TimelineSpan {
                    name,
                    start_ns: start,
                    end_ns: end,
                    depth: event.depth,
                });
            }
        }
        Some(TimelineFrame {
            number,
            duration_ns: end_ns.saturating_sub(start_ns),
            threads: out,
            dropped,
        })
    }

    /// ns since this profiler's epoch.
    fn stamp(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.shared.epoch).as_nanos()).unwrap_or(u64::MAX)
    }

    /// Appends one timeline span to the calling thread's buffer, or counts it as dropped past
    /// the frame's cap (no buffer is touched then).
    fn record_timeline(&self, label: TimelineLabel, start: Instant, end: Instant, depth: usize) {
        let mut n = self.shared.timeline_count.load(Ordering::Relaxed);
        loop {
            if n >= MAX_TIMELINE_SPANS_PER_FRAME {
                self.shared.timeline_dropped.fetch_add(1, Ordering::Relaxed);
                self.shared.timeline_dropped_total.fetch_add(1, Ordering::Relaxed);
                return;
            }
            match self.shared.timeline_count.compare_exchange_weak(
                n,
                n + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(now) => n = now,
            }
        }
        let Some(shard) = thread_shard(&self.shared) else {
            return;
        };
        shard
            .thread_name
            .get_or_init(|| match std::thread::current().name() {
                Some(name) => Arc::from(name),
                None => Arc::from(format!("thread {}", shard.thread)),
            });
        shard.timeline.lock().push(TimelineEvent {
            thread: shard.thread,
            depth: u16::try_from(depth).unwrap_or(u16::MAX),
            label,
            start_ns: self.stamp(start),
            end_ns: self.stamp(end),
        });
    }

    /// Closes the current frame: its span totals become the newest history entry (the oldest is dropped
    /// beyond [`HISTORY_FRAMES`]). Spans still open are counted in the frame in which they exit.
    /// Its buffers are reused, so once the history is full and every name has been seen a frame
    /// allocates nothing (with flame capture off).
    pub fn end_frame(&self) {
        let now = Instant::now();
        let end_ns = self.stamp(now);
        let mut scratch = self.shared.scratch.lock();
        let EndScratch {
            shards,
            batch,
            timeline,
            threads,
        } = &mut *scratch;
        shards.clear();
        batch.clear();
        timeline.clear();
        threads.clear();
        // Drain per-thread tables before taking `state`, so a span exit never waits on the
        // frame-state lock. An exit that lands after its shard was drained is counted next frame.
        // A table only this list (and `shards`) still holds belongs to a thread that has exited (its
        // thread-local handle is gone, and nothing else can reach the table to add to it): it is
        // drained this once and released, so short-lived threads do not grow the list.
        {
            let mut list = self.shared.shards.lock();
            shards.extend(list.iter().cloned());
            list.retain(|shard| Arc::strong_count(shard) > 2);
        }
        let mut merged: Option<flamegraph::FrameFold> = None;
        for shard in shards.iter() {
            let mut totals = shard.totals.lock();
            for (name, accum) in totals.iter_mut() {
                if accum.calls == 0 {
                    continue;
                }
                batch.push((*name, *accum));
                *accum = Accum::default();
            }
            drop(totals);
            let mut flame = shard.flame.lock();
            if flame.exits() > 0 {
                merged
                    .get_or_insert_with(flamegraph::FrameFold::new)
                    .merge(&flame);
                flame.clear();
            }
            drop(flame);
            let mut events = shard.timeline.lock();
            if !events.is_empty() {
                timeline.extend_from_slice(&events);
                events.clear();
                if let Some(name) = shard.thread_name.get() {
                    threads.push((shard.thread, Arc::clone(name)));
                }
            }
        }
        shards.clear();
        let flame = merged.map_or_else(flamegraph::FrameFold::empty_arc, Arc::new);
        self.shared.edge_count.store(0, Ordering::Relaxed);
        self.shared.timeline_count.store(0, Ordering::Relaxed);
        let dropped = self.shared.timeline_dropped.swap(0, Ordering::Relaxed);

        let mut state = self.lock_state();
        for (name, accum) in batch.drain(..) {
            let dst = state.current.entry(name).or_default();
            dst.total += accum.total;
            dst.self_time += accum.self_time;
            dst.calls = dst.calls.saturating_add(accum.calls);
            dst.frame_thread += accum.frame_thread;
        }
        let mut record = if state.frames.len() >= HISTORY_FRAMES {
            state.frames.pop_front().unwrap_or_default()
        } else {
            FrameRecord::default()
        };
        record.spans.clear();
        record.flame = flame;
        record.duration = now.saturating_duration_since(state.last_end);
        record.start_ns = self.stamp(state.last_end);
        record.end_ns = end_ns;
        record.number = state.ended;
        state.ended += 1;
        state.last_end = now;
        // The record's buffers come back as the scratch, so their capacity is reused.
        std::mem::swap(&mut record.timeline, timeline);
        std::mem::swap(&mut record.timeline_threads, threads);
        record.timeline_dropped = dropped;
        for (name, accum) in state.current.iter_mut() {
            if accum.calls > 0 {
                record.spans.push(SpanTime {
                    name,
                    total: accum.total,
                    self_time: accum.self_time,
                    calls: accum.calls,
                    frame_thread: accum.frame_thread,
                });
                *accum = Accum::default();
            }
        }
        state.frames.push_back(record);
        drop(state);
        timeline.clear();
        threads.clear();
    }

    /// Drops all recorded frames and the partially recorded current frame.
    pub fn clear(&self) {
        let shards = self.shared.shards.lock().clone();
        for shard in &shards {
            shard.totals.lock().clear();
            shard.flame.lock().clear();
            shard.timeline.lock().clear();
        }
        self.shared.edge_count.store(0, Ordering::Relaxed);
        self.shared.timeline_count.store(0, Ordering::Relaxed);
        self.shared.timeline_dropped.store(0, Ordering::Relaxed);
        let mut state = self.shared.state.lock();
        state.frames.clear();
        state.current.clear();
        state.last_end = Instant::now();
        state.ended = 0;
    }

    /// Number of frames in the history (at most [`HISTORY_FRAMES`]).
    pub fn frame_count(&self) -> usize {
        self.shared.state.lock().frames.len()
    }

    /// Wall time of the newest recorded frame (between the last two `end_frame` calls).
    pub fn last_frame_time(&self) -> Option<Duration> {
        self.shared.state.lock().frames.back().map(|f| f.duration)
    }

    /// Span enter/exit pairs recorded in the newest frame (all names, all threads).
    pub fn last_frame_span_count(&self) -> u64 {
        self.shared
            .state
            .lock()
            .frames
            .back()
            .map_or(0, |f| f.spans.iter().map(|t| u64::from(t.calls)).sum())
    }

    /// Span enter/exit pairs recorded across every retained frame.
    pub fn history_span_count(&self) -> u64 {
        self.shared
            .state
            .lock()
            .frames
            .iter()
            .flat_map(|f| f.spans.iter())
            .map(|t| u64::from(t.calls))
            .sum()
    }

    /// Wall time of every retained frame, oldest first (at most [`HISTORY_FRAMES`]); the editor
    /// Profiler's frame-time graph (ADR-9740). A read under the frame-state lock: no cost unless
    /// called.
    pub fn frame_times(&self) -> Vec<Duration> {
        self.shared
            .state
            .lock()
            .frames
            .iter()
            .map(|f| f.duration)
            .collect()
    }

    /// Per-span-name inclusive time and calls of retained frame `index` (0 = the oldest, as in
    /// [`Self::frame_times`]), most expensive first; empty when there is no such frame.
    pub fn frame_spans(&self, index: usize) -> Vec<(&'static str, Duration, u32)> {
        let mut out: Vec<(&'static str, Duration, u32)> = {
            let state = self.shared.state.lock();
            state
                .frames
                .get(index)
                .map(|f| f.spans.iter().map(|t| (t.name, t.total, t.calls)).collect())
                .unwrap_or_default()
        };
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        out
    }

    /// Per-span-name time in the newest recorded frame, most expensive first.
    pub fn last_frame(&self) -> Vec<(&'static str, Duration)> {
        let mut out: Vec<_> = {
            let state = self.shared.state.lock();
            state
                .frames
                .back()
                .map(|f| f.spans.iter().map(|t| (t.name, t.total)).collect())
                .unwrap_or_default()
        };
        sort_desc(&mut out);
        out
    }

    /// The newest recorded frame's span times split by thread, each list most expensive first:
    /// time on a frame thread (the main thread, [`mark_main_thread`], and the render thread,
    /// [`mark_render_thread`]) and time on every other thread (workers: a warm, a bake, the
    /// asset loader). A name that ran on both appears in both lists with its share. A hitch is
    /// caused by what the frame thread did; a long worker span beside it is background.
    pub fn last_frame_split(&self) -> (SpanTimes, SpanTimes) {
        let (mut frame, mut background): (Vec<_>, Vec<_>) = {
            let state = self.shared.state.lock();
            let Some(f) = state.frames.back() else {
                return (Vec::new(), Vec::new());
            };
            (
                f.spans
                    .iter()
                    .filter(|t| !t.frame_thread.is_zero())
                    .map(|t| (t.name, t.frame_thread))
                    .collect(),
                f.spans
                    .iter()
                    .map(|t| (t.name, t.total.saturating_sub(t.frame_thread)))
                    .filter(|(_, d)| !d.is_zero())
                    .collect(),
            )
        };
        sort_desc(&mut frame);
        sort_desc(&mut background);
        (frame, background)
    }

    /// Mean per-frame time of each span name over the newest `n_frames` frames (fewer if the history is
    /// shorter), most expensive first. Frames in which a span did not run count as zero.
    pub fn averages(&self, n_frames: usize) -> Vec<(&'static str, Duration)> {
        let (n, sums) = {
            let state = self.shared.state.lock();
            let n = n_frames.min(state.frames.len());
            if n == 0 {
                return Vec::new();
            }
            let mut sums: HashMap<&'static str, Duration> = HashMap::new();
            for frame in state.frames.iter().rev().take(n) {
                for t in &frame.spans {
                    *sums.entry(t.name).or_default() += t.total;
                }
            }
            (n, sums)
        };
        // The lock is released: span exits on other threads no longer wait for the sort.
        let divisor = u32::try_from(n).unwrap_or(u32::MAX);
        let mut out: Vec<_> = sums
            .into_iter()
            .map(|(name, sum)| (name, sum / divisor))
            .collect();
        sort_desc(&mut out);
        out
    }

    /// The `n` most expensive span names, by mean per-frame time over the whole history.
    pub fn top(&self, n: usize) -> Vec<(&'static str, Duration)> {
        let mut all = self.averages(HISTORY_FRAMES);
        all.truncate(n);
        all
    }

    /// The flame graph (span call tree) of the newest `n_frames` frames, averaged over them. Spans
    /// with no timed ancestor (system and schedule spans are skipped) appear as roots. Returns an empty
    /// graph (with `frames` set) when flame capture is off or no span ran in the window.
    ///
    /// [`Self::set_flame_enabled`] must be on while the frames were recorded for this to have data.
    pub fn flame_graph(&self, n_frames: usize) -> FlameGraph {
        // Clone the per-frame trees (an `Arc`) under the lock, then merge outside it.
        // `flame_copied_under_lock` stays zero: no edge buffer is copied here.
        let (folds, frames, frame_time) = {
            let state = self.shared.state.lock();
            let n = n_frames.min(state.frames.len());
            let mut folds = Vec::with_capacity(n);
            let mut frame_time = Duration::ZERO;
            for frame in state.frames.iter().rev().take(n) {
                folds.push(Arc::clone(&frame.flame));
                frame_time += frame.duration;
            }
            let divisor = u32::try_from(n).unwrap_or(u32::MAX).max(1);
            (folds, n, frame_time / divisor)
        };
        let mut merged = flamegraph::FrameFold::new();
        for fold in &folds {
            merged.merge(fold);
        }
        merged.into_graph(frames, frame_time)
    }

    fn record(&self, name: &'static str, elapsed: Duration, self_time: Duration, frame_thread: bool) {
        let Some(shard) = thread_shard(&self.shared) else {
            return;
        };
        let mut totals = shard.totals.lock();
        let accum = totals.entry(name).or_default();
        accum.total += elapsed;
        accum.self_time += self_time;
        accum.calls += 1;
        if frame_thread {
            accum.frame_thread += elapsed;
        }
    }

    /// Reserves one flame edge for this frame. `false` once the frame is at the cap, before any
    /// path buffer is built.
    fn try_reserve_edge(&self) -> bool {
        let mut n = self.shared.edge_count.load(Ordering::Relaxed);
        loop {
            if n >= MAX_EDGES_PER_FRAME {
                return false;
            }
            match self
                .shared
                .edge_count
                .compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
    }

    fn record_edge(&self, path: &[&'static str], name: &'static str, elapsed: Duration) {
        if let Some(shard) = thread_shard(&self.shared) {
            shard.flame.lock().add(path, name, elapsed);
        }
    }
}

fn sort_desc(v: &mut [(&'static str, Duration)]) {
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
}

impl<S> Layer<S> for FrameProfiler
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        // Host sections (`set_host_span_prefixes`): timed by span name.
        if !is_system_span(metadata) {
            if host_prefix_matches(metadata.target())
                && let Some(span) = ctx.span(id)
            {
                let slot = host_slot(metadata.name(), metadata.target());
                if slot != NO_SYSTEM {
                    span.extensions_mut().insert(HostSlot(slot));
                }
            }
            return;
        }
        // Once per ECS system (bevy creates the span with the system): intern its name.
        let mut visitor = NameVisitor(None);
        attrs.record(&mut visitor);
        if let (Some(name), Some(span)) = (visitor.0, ctx.span(id)) {
            let slot = system_slot(&name);
            span.extensions_mut().insert(SystemSlot(slot));
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if !self.is_enabled() {
            return;
        }
        // One registry lookup serves the name and, for an ECS system, its slot.
        let Some(span) = ctx.span(id) else {
            return;
        };
        let metadata = span.metadata();
        // Only ECS-system and host spans carry a slot; other spans skip the extensions lock.
        let (system, host) = if !is_system_span(metadata) && !host_prefix_matches(metadata.target()) {
            (NO_SYSTEM, NO_SYSTEM)
        } else {
            let ext = span.extensions();
            (
                ext.get::<SystemSlot>().map_or(NO_SYSTEM, |s| s.0),
                ext.get::<HostSlot>().map_or(NO_SYSTEM, |s| s.0),
            )
        };
        let main = IS_MAIN.try_with(std::cell::Cell::get).unwrap_or(false);
        let host_top = host != NO_SYSTEM
            && HOST_DEPTH
                .try_with(|d| {
                    let depth = d.get();
                    d.set(depth + 1);
                    depth == 0
                })
                .unwrap_or(false)
            && main;
        let open = Open {
            profiler: self.shared.id,
            span: id.into_u64(),
            name: metadata.name(),
            start: Instant::now(),
            children: Duration::ZERO,
            system,
            over_cap: system != NO_SYSTEM && system as usize >= MAX_SYSTEMS,
            host,
            host_top,
            main,
            frame_thread: main || on_frame_thread(),
            bevy_umbrella: system == NO_SYSTEM && metadata.target().starts_with("bevy_ecs"),
        };
        // `try_with` fails only during thread teardown; the span is then not measured.
        let _ = OPEN.try_with(|stack| stack.borrow_mut().push(open));
    }

    fn on_exit(&self, id: &Id, _ctx: Context<'_, S>) {
        let now = Instant::now();
        let (profiler, span) = (self.shared.id, id.into_u64());
        let want_flame = self.flame_enabled();
        let want_timeline = self.timeline_enabled();
        let open = OPEN
            .try_with(|stack| {
                let mut stack = stack.borrow_mut();
                let pos = stack
                    .iter()
                    .rposition(|o| o.profiler == profiler && o.span == span)?;
                let open = stack.remove(pos);
                // The enclosing span of this profiler gets this span's time as child time.
                let elapsed = now.saturating_duration_since(open.start);
                if let Some(parent) = stack[..pos].iter_mut().rev().find(|o| o.profiler == profiler) {
                    parent.children += elapsed;
                }
                // Fold the call path only when this exit is recorded by name and the frame
                // still has room. The path is written into a reused buffer; past the cap nothing
                // is allocated and the exit is not added to the tree.
                let fold_flame = want_flame && open.flame_visible() && self.try_reserve_edge();
                if fold_flame {
                    // `try_with`: during thread teardown the buffer may already be gone.
                    let _ = FLAME_PATH.try_with(|buf| {
                        let mut buf = buf.borrow_mut();
                        buf.clear();
                        buf.extend(
                            stack[..pos]
                                .iter()
                                .filter(|o| o.profiler == profiler && o.flame_visible())
                                .map(|o| {
                                    // A system past the slot cap is filed under its own name.
                                    if o.over_cap {
                                        system_name(o.system).unwrap_or(o.name)
                                    } else {
                                        o.name
                                    }
                                }),
                        );
                        // A root on a task-pool thread inside a flame context goes under it.
                        if buf.is_empty()
                            && on_task_pool_thread()
                            && let Some(context) = current_flame_context()
                        {
                            buf.push(context);
                        }
                        // A system past the slot cap folds under its own interned name.
                        let name = if open.over_cap {
                            system_name(open.system).unwrap_or(open.name)
                        } else {
                            open.name
                        };
                        self.record_edge(buf.as_slice(), name, elapsed);
                    });
                }
                // Timeline depth: the recorded spans of this profiler open around this one on this
                // thread. Counted only while the timeline is on.
                let depth = if want_timeline && !open.bevy_umbrella {
                    stack[..pos]
                        .iter()
                        .filter(|o| o.profiler == profiler && !o.bevy_umbrella)
                        .count()
                } else {
                    0
                };
                Some((open, elapsed, depth))
            })
            .ok()
            .flatten();
        if let Some((open, elapsed, depth)) = open {
            if want_timeline && !open.bevy_umbrella {
                let label = if open.system == NO_SYSTEM {
                    TimelineLabel::Static(open.name)
                } else {
                    TimelineLabel::System(open.system)
                };
                self.record_timeline(label, open.start, now, depth);
            }
            if open.host != NO_SYSTEM {
                let _ = HOST_DEPTH.try_with(|d| d.set(d.get().saturating_sub(1)));
                let slot = open.host as usize;
                let (ns, calls) = if open.main {
                    (&HOST_MAIN_NS[slot], &HOST_MAIN_CALLS[slot])
                } else {
                    (&HOST_OTHER_NS[slot], &HOST_OTHER_CALLS[slot])
                };
                ns.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
                calls.fetch_add(1, Ordering::Relaxed);
                if open.host_top {
                    HOST_MAIN_TOP_NS.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
                }
            }
            if open.system == NO_SYSTEM {
                if !open.bevy_umbrella {
                    self.record(
                        open.name,
                        elapsed,
                        elapsed.saturating_sub(open.children),
                        open.frame_thread,
                    );
                }
            } else if open.over_cap {
                // Past the slot cap a system is timed by its interned name, like any named
                // span (ADR-7409): no per-slot counter exists for it.
                let name = system_name(open.system).unwrap_or(open.name);
                self.record(
                    name,
                    elapsed,
                    elapsed.saturating_sub(open.children),
                    open.frame_thread,
                );
            } else if let Some(ns) = SYSTEM_NS.get(open.system as usize) {
                // ECS systems are timed per system (`take_system_times`), not under one
                // `system` name in the frame table (it would sum every thread's systems).
                ns.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
                SYSTEM_CALLS[open.system as usize].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// Unset host prefixes match `do27::` and the executable stem. A set list replaces both.
    /// The process lock stays unset: nothing in this crate calls [`set_host_span_prefixes`].
    #[test]
    fn unset_host_prefixes_match_do27_and_the_executable_stem() {
        assert!(
            HOST_PREFIXES.get().is_none(),
            "set_host_span_prefixes ran in this process; the unset path was not what this test saw"
        );
        assert!(host_prefix_matches("do27::tick"));
        match default_host_span_prefix() {
            Some(prefix) => {
                let target = format!("{prefix}tick");
                assert!(host_prefix_matches(&target), "{target}");
            }
            None => {
                // This test binary's stem starts with `regia` or a reserved name, so the live
                // stem is not a host prefix. The helper below covers a stem that is.
                assert!(!host_prefix_matches("arena::tick"));
            }
        }
        assert!(!host_prefix_matches("mods::tick"));

        let mods = ["mods::".to_owned()];
        let mods: &[String] = &mods;
        let none: &[String] = &[];
        assert!(host_prefixes_match("arena::tick", None, Some("arena::")));
        assert!(host_prefixes_match("do27::tick", None, Some("arena::")));
        assert!(!host_prefixes_match("mods::tick", None, Some("arena::")));
        assert!(!host_prefixes_match("arena::tick", None, None));
        assert!(host_prefixes_match("do27::tick", None, None));
        assert!(host_prefixes_match("mods::tick", Some(mods), Some("arena::")));
        assert!(!host_prefixes_match("arena::tick", Some(mods), Some("arena::")));
        assert!(!host_prefixes_match("do27::tick", Some(mods), Some("arena::")));
        assert!(!host_prefixes_match("mods::tick", Some(none), Some("arena::")));
    }

    /// ADR-9740: inside a flame context, a root span of a task-pool thread is filed under the
    /// context; other threads and spans outside the context are unchanged.
    #[test]
    fn a_flame_context_files_task_pool_roots_under_it() {
        let _serial = CONTEXT_TESTS.lock();
        let profiler = FrameProfiler::new();
        profiler.set_flame_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        let run_on = |thread: &str, span: &'static str| {
            let d = dispatch.clone();
            std::thread::Builder::new()
                .name(thread.to_owned())
                .spawn(move || {
                    tracing::dispatcher::with_default(&d, || {
                        let _s = match span {
                            "worker_in_context" => tracing::info_span!("worker_in_context").entered(),
                            "worker_outside" => tracing::info_span!("worker_outside").entered(),
                            _ => tracing::info_span!("other_thread_root").entered(),
                        };
                    });
                })
                .and_then(|h| h.join().map_err(|_| std::io::Error::other("panicked")))
                .expect("thread");
        };
        run_on("regia-compute (0)", "worker_outside");
        {
            let _context = flame_context("ctx_frame");
            run_on("regia-compute (1)", "worker_in_context");
            run_on("render", "other_thread_root");
        }
        profiler.end_frame();
        let graph = profiler.flame_graph(1);
        let roots: Vec<&str> = graph.roots.iter().map(|n| n.name).collect();
        assert!(roots.contains(&"worker_outside"), "{roots:?}");
        assert!(roots.contains(&"other_thread_root"), "not a task pool: {roots:?}");
        let ctx = graph
            .roots
            .iter()
            .find(|n| n.name == "ctx_frame")
            .expect("context root");
        assert_eq!(ctx.children[0].name, "worker_in_context");
        assert!(
            current_flame_context().is_none(),
            "the guard restored the context"
        );
        assert!(is_task_pool_thread_name("Compute Task Pool (3)"));
        assert!(!is_task_pool_thread_name("regia-io (0)"), "loaders are not play");
        assert!(!is_task_pool_thread_name("render"));
    }

    /// The flame context is process-wide, so tests that set one must not overlap.
    static CONTEXT_TESTS: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// ADR-7427 (1): only the compute pool, where a multi-threaded schedule runs its systems,
    /// is play work. The async-compute and IO pools (editor asset imports, network polls) are not.
    #[test]
    fn only_compute_pool_threads_count_as_context_workers() {
        assert!(is_task_pool_thread_name("regia-compute (0)"));
        assert!(is_task_pool_thread_name("Compute Task Pool (3)"));
        for name in [
            "regia-async (0)",
            "Async Compute Task Pool (1)",
            "regia-io (0)",
            "IO Task Pool (2)",
            "render",
            "profiler-remote",
        ] {
            assert!(!is_task_pool_thread_name(name), "{name} is not play work");
        }
    }

    /// ADR-7427 (1): a span opened and closed on a `regia-async` thread while a context is held
    /// is a root of its own, not under the context; a compute worker's root still is.
    #[test]
    fn an_async_pool_root_inside_a_context_is_not_filed_under_it() {
        let _serial = CONTEXT_TESTS.lock();
        let profiler = FrameProfiler::new();
        profiler.set_flame_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        let run_on = |thread: &str, async_span: bool| {
            let d = dispatch.clone();
            std::thread::Builder::new()
                .name(thread.to_owned())
                .spawn(move || {
                    tracing::dispatcher::with_default(&d, || {
                        let _s = if async_span {
                            tracing::info_span!("async_import").entered()
                        } else {
                            tracing::info_span!("play_system_work").entered()
                        };
                    });
                })
                .and_then(|h| h.join().map_err(|_| std::io::Error::other("panicked")))
                .expect("thread");
        };
        {
            let _context = flame_context("pie_frame");
            run_on("regia-async (0)", true);
            run_on("regia-compute (2)", false);
        }
        profiler.end_frame();
        let graph = profiler.flame_graph(1);
        let roots: Vec<&str> = graph.roots.iter().map(|n| n.name).collect();
        assert!(
            roots.contains(&"async_import"),
            "async work is its own root: {roots:?}"
        );
        let play = graph
            .roots
            .iter()
            .find(|n| n.name == "pie_frame")
            .expect("context root");
        let under: Vec<&str> = play.children.iter().map(|n| n.name).collect();
        assert_eq!(under, ["play_system_work"], "only compute work under play");
    }

    /// ADR-7427 (2): nested guards dropped in order restore each outer context in turn.
    #[test]
    fn nested_flame_contexts_dropped_in_order_restore_the_outer_one() {
        let _serial = CONTEXT_TESTS.lock();
        assert_eq!(current_flame_context(), None);
        let outer = flame_context("outer_ctx");
        assert_eq!(current_flame_context(), Some("outer_ctx"));
        let inner = flame_context("inner_ctx");
        assert_eq!(current_flame_context(), Some("inner_ctx"));
        drop(inner);
        assert_eq!(current_flame_context(), Some("outer_ctx"));
        drop(outer);
        assert_eq!(current_flame_context(), None);
    }

    /// ADR-7427 (2): guards dropped out of order (A, then B created; A dropped first) leave B's
    /// context while B lives and no context after both drop.
    #[test]
    fn flame_contexts_dropped_out_of_order_end_with_no_context() {
        let _serial = CONTEXT_TESTS.lock();
        let a = flame_context("ctx_a");
        let b = flame_context("ctx_b");
        drop(a);
        assert_eq!(
            current_flame_context(),
            Some("ctx_b"),
            "B still holds its context"
        );
        drop(b);
        assert_eq!(
            current_flame_context(),
            None,
            "no stale context after the last guard"
        );

        // Three deep, middle first.
        let a = flame_context("ctx_a");
        let b = flame_context("ctx_b");
        let c = flame_context("ctx_c");
        drop(b);
        assert_eq!(current_flame_context(), Some("ctx_c"));
        drop(c);
        assert_eq!(
            current_flame_context(),
            Some("ctx_a"),
            "the outer context is restored"
        );
        drop(a);
        assert_eq!(current_flame_context(), None);
    }

    /// ADR-7427 (2): guards held on two threads, dropped in either order, end with no context.
    #[test]
    fn flame_contexts_on_two_threads_end_with_no_context() {
        let _serial = CONTEXT_TESTS.lock();
        for main_first in [true, false] {
            let main = flame_context("main_ctx");
            let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let other = std::thread::spawn(move || {
                let guard = flame_context("other_ctx");
                let _ = held_tx.send(());
                let _ = hold_rx.recv();
                drop(guard);
            });
            held_rx.recv().expect("the other thread holds its guard");
            assert_eq!(current_flame_context(), Some("other_ctx"));
            if main_first {
                drop(main);
                assert_eq!(current_flame_context(), Some("other_ctx"));
                let _ = hold_tx.send(());
                other.join().expect("other thread");
            } else {
                let _ = hold_tx.send(());
                other.join().expect("other thread");
                assert_eq!(current_flame_context(), Some("main_ctx"));
                drop(main);
            }
            assert_eq!(current_flame_context(), None, "main_first = {main_first}");
        }
    }

    /// ADR-9740: the editor Profiler's history reads every retained frame's time and spans,
    /// oldest first, by index.
    #[test]
    fn frame_times_and_spans_read_the_history_by_index() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            for calls in 1..=3_u32 {
                for _ in 0..calls {
                    let _s = tracing::info_span!("history_span").entered();
                }
                if calls == 2 {
                    let _only = tracing::info_span!("only_in_frame_two").entered();
                }
                profiler.end_frame();
            }
        });
        assert_eq!(profiler.frame_times().len(), 3);
        let calls_of = |index: usize, name: &str| {
            profiler
                .frame_spans(index)
                .iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, _, calls)| *calls)
        };
        assert_eq!(calls_of(0, "history_span"), Some(1), "index 0 is the oldest");
        assert_eq!(calls_of(2, "history_span"), Some(3));
        assert_eq!(calls_of(1, "only_in_frame_two"), Some(1));
        assert_eq!(calls_of(0, "only_in_frame_two"), None);
        assert!(profiler.frame_spans(3).is_empty(), "no such frame");
    }

    #[test]
    fn host_spans_are_timed_by_name_main_thread_apart() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        let mut main = Vec::new();
        let mut other = Vec::new();
        tracing::dispatcher::with_default(&dispatch, || {
            mark_main_thread();
            let _ = take_host_times(&mut main, &mut other);
            {
                let _outer = tracing::info_span!(target: "do27::frame", "do27.test_outer").entered();
                spin(Duration::from_millis(2));
                let _inner = tracing::info_span!(target: "do27::frame", "do27.test_inner").entered();
                spin(Duration::from_millis(1));
            }
            let _ignored = tracing::info_span!(target: "regia_render", "not_host").entered();
            let d = dispatch.clone();
            std::thread::spawn(move || {
                tracing::dispatcher::with_default(&d, || {
                    let _sim = tracing::info_span!(target: "do27::sim", "do27.test_sim").entered();
                    spin(Duration::from_millis(1));
                });
            })
            .join()
            .expect("sim thread");
        });
        let top = take_host_times(&mut main, &mut other);
        let name = |slot: u32| host_span_name(slot).map(|(n, _)| n);
        let main_names: Vec<_> = main.iter().filter_map(|&(s, _, _)| name(s)).collect();
        assert!(main_names.contains(&"do27.test_outer") && main_names.contains(&"do27.test_inner"));
        assert!(!main_names.contains(&"not_host"));
        assert!(other.iter().any(|&(s, _, _)| name(s) == Some("do27.test_sim")));
        // Top-level time counts the outer section once (the inner one nests in it).
        let outer = main
            .iter()
            .find(|&&(s, _, _)| name(s) == Some("do27.test_outer"))
            .expect("outer")
            .1;
        assert!(
            top >= Duration::from_millis(3) && top <= outer + Duration::from_micros(50),
            "{top:?} {outer:?}"
        );
    }

    /// A host span entered before the thread is marked main and exited after is charged to one
    /// bucket only: the "other" one it was opened under, never main without top-level time.
    #[test]
    fn host_span_straddling_the_main_mark_uses_one_bucket() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        // A fresh thread: `IS_MAIN` is thread-local and starts false.
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let span = tracing::info_span!(target: "do27::frame", "do27.test_straddle").entered();
                mark_main_thread();
                drop(span);
            });
            // Read without draining: other tests take the shared counters.
            let slot = (0..host_span_count() as u32)
                .find(|&slot| host_span_name(slot).map(|(n, _)| n) == Some("do27.test_straddle"))
                .expect("host span interned");
            assert_eq!(
                HOST_MAIN_CALLS[slot as usize].load(Ordering::Relaxed),
                0,
                "straddling span charged to main"
            );
        })
        .join()
        .expect("straddle thread");
    }

    /// A thread that exits leaves its table behind for one fold only: its last exits are counted
    /// once, then the table is released, so short-lived threads do not grow the list every
    /// `end_frame` walks (corr-core audit).
    #[test]
    fn exited_threads_release_their_tables_after_one_fold() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        for _ in 0..8 {
            let d = dispatch.clone();
            std::thread::spawn(move || {
                tracing::dispatcher::with_default(&d, || {
                    let _span = tracing::info_span!("short_lived_worker").entered();
                });
            })
            .join()
            .expect("worker thread");
        }
        assert_eq!(profiler.shard_count(), 8);
        profiler.end_frame();
        assert_eq!(
            profiler.last_frame_span_count(),
            8,
            "the exited threads' exits are counted"
        );
        assert_eq!(profiler.shard_count(), 0, "exited threads' tables are released");
        profiler.end_frame();
        assert_eq!(profiler.last_frame_span_count(), 0, "and counted only once");
        // A live thread keeps its table.
        tracing::dispatcher::with_default(&dispatch, || {
            let _span = tracing::info_span!("live_worker").entered();
        });
        profiler.end_frame();
        assert_eq!(profiler.last_frame_span_count(), 1);
        assert_eq!(profiler.shard_count(), 1);
    }

    /// corr-low-framework (corr-core L9): a long-lived thread does not keep a table for every
    /// profiler it ever recorded into. Once a profiler is dropped, only this thread's list still
    /// held its table; the list grew by one per profiler and every span exit searched it.
    #[test]
    fn a_thread_lets_go_of_the_tables_of_dropped_profilers() {
        std::thread::spawn(|| {
            for _ in 0..8 {
                let profiler = FrameProfiler::new();
                let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
                tracing::dispatcher::with_default(&dispatch, || {
                    let _span = tracing::info_span!("one_profiler_of_many").entered();
                });
            }
            let live = FrameProfiler::new();
            let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(live.clone()));
            tracing::dispatcher::with_default(&dispatch, || {
                let _span = tracing::info_span!("the_live_profiler").entered();
            });
            assert_eq!(
                local_shard_count(),
                1,
                "only the live profiler's table is kept on this thread"
            );
            live.end_frame();
            assert_eq!(live.last_frame_span_count(), 1);
        })
        .join()
        .expect("worker thread");
    }

    /// Environment variable that makes [`span_exit_during_thread_teardown_is_not_a_panic`] run
    /// its scenario in this process instead of re-running itself as a child.
    const TEARDOWN_CHILD_ENV: &str = "REGIA_PROFILING_TEARDOWN_CHILD";

    /// A span that exits while its thread's locals are being destroyed is dropped quietly. A panic
    /// there is inside a thread-local destructor, which aborts the whole process, so the scenario
    /// runs in a child copy of this test binary and the parent checks its exit status
    /// (corr-core audit).
    #[test]
    fn span_exit_during_thread_teardown_is_not_a_panic() {
        if std::env::var_os(TEARDOWN_CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("test binary path");
            let output = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "profiling::tests::span_exit_during_thread_teardown_is_not_a_panic",
                    "--test-threads=1",
                ])
                .env(TEARDOWN_CHILD_ENV, "1")
                .output()
                .expect("run the child test");
            assert!(
                output.status.success(),
                "child aborted: {:?}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child did not run the scenario: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        thread_local! {
            // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion.
            #[allow(clippy::missing_const_for_thread_local)]
            static LATE: RefCell<Option<tracing::span::EnteredSpan>> = const { RefCell::new(None) };
        }
        let profiler = FrameProfiler::new();
        profiler.set_flame_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                // Thread locals are destroyed newest first. Entering `late` registers the
                // profiler's open-span stack; `LATE` comes next; the exit of `early` registers
                // the per-thread tables last. At thread exit the tables go first, then `LATE`
                // drops `late`, whose exit runs while the open-span stack is still alive.
                let late = tracing::info_span!("late").entered();
                LATE.with(|slot| *slot.borrow_mut() = Some(late));
                tracing::info_span!("early").in_scope(|| {});
            });
        })
        .join()
        .expect("teardown thread");
        assert!(profiler.shard_count() <= 1);
    }

    #[test]
    fn bevy_system_spans_are_timed_per_system() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            // As `bevy_ecs` (feature `trace`) names a system's span.
            let span = tracing::info_span!(
                target: "bevy_ecs::system::function_system",
                parent: None,
                "system",
                name = "profiling_test::my_system"
            );
            for _ in 0..3 {
                let _g = span.enter();
                spin(Duration::from_millis(1));
            }
        });
        let slot = (0..system_count() as u32)
            .find(|&s| system_name(s) == Some("profiling_test::my_system"))
            .expect("system interned");
        let mut out = Vec::new();
        take_system_times(&mut out);
        let &(_, time, runs) = out.iter().find(|(s, _, _)| *s == slot).expect("system timed");
        assert_eq!(runs, 3);
        assert!(time >= Duration::from_millis(3), "{time:?}");
        profiler.end_frame();
        assert!(
            time_of(&profiler.last_frame(), "system").is_none(),
            "systems are not summed under one span name"
        );
    }

    fn with_profiler(profiler: &FrameProfiler, f: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry().with(profiler.clone());
        tracing::subscriber::with_default(subscriber, f);
    }

    fn spin(d: Duration) {
        let start = Instant::now();
        while start.elapsed() < d {
            std::hint::spin_loop();
        }
    }

    fn time_of(list: &[(&'static str, Duration)], name: &str) -> Option<Duration> {
        list.iter().find(|(n, _)| *n == name).map(|&(_, d)| d)
    }

    #[test]
    fn records_nested_scopes_per_frame() {
        let profiler = FrameProfiler::new();
        with_profiler(&profiler, || {
            {
                scope!("outer");
                spin(Duration::from_millis(2));
                for _ in 0..3 {
                    scope!("inner", iteration = 1);
                    spin(Duration::from_millis(1));
                }
            }
            profiler.end_frame();
            {
                scope!("inner");
            }
            profiler.end_frame();
        });
        assert_eq!(profiler.frame_count(), 2);
        let last = profiler.last_frame();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].0, "inner");

        let both = profiler.averages(2);
        let outer = time_of(&both, "outer").expect("outer recorded");
        let inner = time_of(&both, "inner").expect("inner recorded");
        // Averaged over two frames, outer (≥ 5 ms in frame 1) is ≥ 2.5 ms; inclusive of inner's 3 ms.
        assert!(outer >= Duration::from_micros(2500), "{outer:?}");
        assert!(inner >= Duration::from_micros(1500), "{inner:?}");
        assert!(outer > inner);
        assert_eq!(profiler.top(1)[0].0, "outer");
        assert_eq!(profiler.averages(0), Vec::new());
        assert!(profiler.last_frame_time().is_some());
    }

    #[test]
    fn history_is_bounded_and_spans_on_other_threads_count() {
        let profiler = FrameProfiler::new();
        with_profiler(&profiler, || {
            for _ in 0..HISTORY_FRAMES + 20 {
                scope!("frame_work");
                profiler.end_frame();
            }
        });
        assert_eq!(profiler.frame_count(), HISTORY_FRAMES);

        let dispatch = tracing_subscriber::registry().with(profiler.clone());
        let dispatch = tracing::Dispatch::new(dispatch);
        std::thread::scope(|s| {
            for _ in 0..4 {
                let dispatch = dispatch.clone();
                s.spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        scope!("worker");
                        spin(Duration::from_millis(1));
                    });
                });
            }
        });
        profiler.end_frame();
        let worker = time_of(&profiler.last_frame(), "worker").expect("worker spans recorded");
        assert!(worker >= Duration::from_millis(4), "{worker:?}");
    }

    #[test]
    fn disabled_profiler_records_nothing_and_two_profilers_are_independent() {
        let a = FrameProfiler::new();
        let b = FrameProfiler::new();
        b.set_enabled(false);
        let subscriber = tracing_subscriber::registry().with(a.clone()).with(b.clone());
        tracing::subscriber::with_default(subscriber, || {
            scope!("work");
        });
        a.end_frame();
        b.end_frame();
        assert_eq!(a.last_frame().len(), 1);
        assert!(b.last_frame().is_empty());
        a.clear();
        assert_eq!(a.frame_count(), 0);
        assert!(format!("{a:?}").contains("FrameProfiler"));
    }

    /// Waits until the clock moves, so a span's end is after its start on every platform.
    fn tick() {
        let t = Instant::now();
        while Instant::now() == t {
            std::hint::spin_loop();
        }
    }

    /// ADR-7325: a named span on a spawned worker and spans on this thread read back as two
    /// thread rows with nested depths and start < end for every span.
    #[test]
    fn the_timeline_reads_one_row_per_thread_with_nested_depths() {
        let profiler = FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            let outer = tracing::info_span!("tl_main_outer").entered();
            tick();
            {
                let _inner = tracing::info_span!("tl_main_inner").entered();
                tick();
            }
            let d = dispatch.clone();
            std::thread::Builder::new()
                .name("timeline-worker".into())
                .spawn(move || {
                    tracing::dispatcher::with_default(&d, || {
                        let _w = tracing::info_span!("tl_worker").entered();
                        tick();
                        let _c = tracing::info_span!("tl_worker_child").entered();
                        tick();
                    });
                })
                .and_then(|h| h.join().map_err(|_| std::io::Error::other("worker panicked")))
                .expect("worker");
            drop(outer);
            profiler.end_frame();
        });
        let frame = profiler.timeline_frame(None).expect("a frame");
        assert_eq!(frame.threads.len(), 2, "{frame:#?}");
        let worker = frame
            .threads
            .iter()
            .find(|t| &*t.name == "timeline-worker")
            .expect("the worker's row");
        let main = frame
            .threads
            .iter()
            .find(|t| t.index != worker.index)
            .expect("this thread's row");
        let depth = |t: &TimelineThread, name: &str| t.spans.iter().find(|s| s.name == name).map(|s| s.depth);
        assert_eq!(depth(main, "tl_main_outer"), Some(0));
        assert_eq!(depth(main, "tl_main_inner"), Some(1));
        assert_eq!(depth(worker, "tl_worker"), Some(0));
        assert_eq!(depth(worker, "tl_worker_child"), Some(1));
        assert_eq!(main.spans[0].name, "tl_main_outer", "sorted by start");
        for thread in &frame.threads {
            for span in &thread.spans {
                assert!(span.start_ns < span.end_ns, "{span:?}");
                assert!(
                    span.end_ns <= frame.duration_ns,
                    "{span:?} in {}",
                    frame.duration_ns
                );
            }
        }
        let outer = main
            .spans
            .iter()
            .find(|s| s.name == "tl_main_outer")
            .expect("outer");
        let inner = main
            .spans
            .iter()
            .find(|s| s.name == "tl_main_inner")
            .expect("inner");
        assert!(outer.start_ns <= inner.start_ns && inner.end_ns <= outer.end_ns);
        assert_eq!(frame.dropped, 0);
        assert_eq!(frame.number, 0);
    }

    /// ADR-7325: off by default, a frame has no timeline; the cap keeps the first spans and
    /// counts the rest as dropped; frames are read by number while they are retained.
    #[test]
    fn the_timeline_is_off_by_default_capped_and_read_by_number() {
        let profiler = FrameProfiler::new();
        assert!(!profiler.timeline_enabled());
        with_profiler(&profiler, || {
            scope!("tl_off");
            profiler.end_frame();
        });
        let off = profiler.timeline_frame(None).expect("frame");
        assert!(off.threads.is_empty() && off.dropped == 0, "{off:?}");
        profiler.set_timeline_enabled(true);
        with_profiler(&profiler, || {
            for _ in 0..MAX_TIMELINE_SPANS_PER_FRAME + 5 {
                scope!("tl_capped");
            }
            profiler.end_frame();
        });
        let capped = profiler.timeline_frame(None).expect("frame");
        assert_eq!(capped.span_count(), MAX_TIMELINE_SPANS_PER_FRAME);
        assert_eq!(capped.dropped, 5);
        assert_eq!(profiler.timeline_dropped_total(), 5);
        assert_eq!(profiler.frame_numbers(), [0, 1]);
        assert_eq!(profiler.newest_frame_number(), Some(1));
        let first = profiler.timeline_frame(Some(0)).expect("frame 0");
        assert!(first.threads.is_empty());
        with_profiler(&profiler, || {
            scope!("tl_next");
            profiler.end_frame();
        });
        assert_eq!(
            profiler.timeline_frame(None).map(|f| f.dropped),
            Some(0),
            "the count is per frame"
        );
        assert!(profiler.timeline_frame(Some(99)).is_none(), "not in the history");
        profiler.clear();
        assert!(profiler.timeline_frame(None).is_none());
        assert_eq!(profiler.frame_numbers(), Vec::<u64>::new());
    }

    /// ADR-7325: an ECS system span is named by its system on the timeline.
    #[test]
    fn a_system_span_is_named_by_its_system_on_the_timeline() {
        let profiler = FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        with_profiler(&profiler, || {
            let span = tracing::info_span!(
                target: "bevy_ecs::system::function_system",
                parent: None,
                "system",
                name = "profiling_test::timeline_system"
            );
            {
                let _g = span.enter();
            }
            profiler.end_frame();
        });
        let frame = profiler.timeline_frame(None).expect("frame");
        let names: Vec<&str> = frame.threads[0].spans.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(names, ["profiling_test::timeline_system"]);
    }

    /// Environment variable that makes [`systems_past_the_cap_keep_their_names_in_the_timeline_and_flame`]
    /// run its scenario in a child copy of this test binary: the system-name table is
    /// process-global and other tests intern names too, which would move the overflow count.
    const OVER_CAP_CHILD_ENV: &str = "REGIA_PROFILING_OVER_CAP_CHILD";

    /// Past `MAX_SYSTEMS` a system keeps its own name in the timeline and the flame records (only
    /// the per-slot counters stay capped), and the overflow is counted.
    #[test]
    fn systems_past_the_cap_keep_their_names_in_the_timeline_and_flame() {
        if std::env::var_os(OVER_CAP_CHILD_ENV).is_none() {
            let exe = std::env::current_exe().expect("test binary path");
            let output = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "profiling::tests::systems_past_the_cap_keep_their_names_in_the_timeline_and_flame",
                    "--test-threads=1",
                ])
                .env(OVER_CAP_CHILD_ENV, "1")
                .output()
                .expect("run the child test");
            assert!(
                output.status.success(),
                "child failed: {:?}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child did not run the scenario: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let profiler = FrameProfiler::new();
        profiler.set_timeline_enabled(true);
        profiler.set_flame_enabled(true);
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        let name_of = |i: usize| format!("over_cap_system_{i:05}");
        let before = systems_over_cap();
        tracing::dispatcher::with_default(&dispatch, || {
            for i in 0..MAX_SYSTEMS + 2 {
                let name = name_of(i);
                let span = tracing::info_span!(
                    target: "bevy_ecs::system::function_system",
                    parent: None,
                    "system",
                    name = name.as_str()
                );
                let _guard = span.enter();
            }
            profiler.end_frame();
        });
        // The 2050th distinct system is past the cap and shows by its own name.
        let last = name_of(MAX_SYSTEMS + 1);
        let frame = profiler.timeline_frame(None).expect("frame");
        let names: Vec<&str> = frame
            .threads
            .iter()
            .flat_map(|t| t.spans.iter().map(|s| s.name.as_ref()))
            .collect();
        assert_eq!(names.len(), MAX_SYSTEMS + 2, "{:?}", &names[..4]);
        assert!(names.contains(&last.as_str()), "the last name is in the timeline");
        let graph = profiler.flame_graph(1);
        let mut flame_names: Vec<&str> = graph.roots.iter().map(|n| n.name).collect();
        for node in &graph.roots {
            flame_names.extend(node.children.iter().map(|c| c.name));
        }
        assert!(
            flame_names.contains(&last.as_str()),
            "the last name is in the flame: {:?}",
            &flame_names[..4]
        );
        assert!(
            !flame_names.contains(&"system"),
            "no past-cap system shows under the bare span name `system`"
        );
        assert_eq!(
            systems_over_cap(),
            before + 2,
            "two systems past the cap (the child process interns exactly these)"
        );
        // The per-slot counters stay capped: the past-cap systems are not timed per slot.
        let mut times = Vec::new();
        take_system_times(&mut times);
        assert!(times.len() <= MAX_SYSTEMS);
        assert!(system_name((MAX_SYSTEMS + 1) as u32).is_some_and(|n| n == last));
        assert!(system_count() >= MAX_SYSTEMS + 2);
    }

    #[test]
    fn flame_graph_records_the_span_call_tree_and_off_records_nothing() {
        let profiler = FrameProfiler::new();
        profiler.set_flame_enabled(true);
        with_profiler(&profiler, || {
            for _ in 0..2 {
                scope!("fg_outer");
                spin(Duration::from_millis(2));
                scope!("fg_inner");
                spin(Duration::from_millis(1));
            }
            profiler.end_frame();
        });
        let graph = profiler.flame_graph(1);
        assert_eq!(graph.frames, 1);
        let outer = graph
            .roots
            .iter()
            .find(|r| r.name == "fg_outer")
            .expect("fg_outer is a root");
        assert_eq!(outer.calls, 2.0);
        assert_eq!(outer.children.len(), 1);
        assert_eq!(outer.children[0].name, "fg_inner");
        assert_eq!(outer.children[0].calls, 2.0);
        // Outer's self time excludes the inner child.
        assert!(outer.self_time < outer.inclusive);
        assert!(outer.self_time > Duration::ZERO);

        // Flame capture off by default: nothing is recorded.
        let off = FrameProfiler::new();
        with_profiler(&off, || {
            scope!("fg_off");
            off.end_frame();
        });
        assert!(off.flame_graph(1).roots.is_empty());
    }

    /// v0.5.1 (DO27 telemetry-hitch-floor): a worker's span is background, never frame-thread
    /// time. The launch hitch was named after a warm worker's 692 ms span. No wall-clock bar: the
    /// worker's time is only asserted to be in the background list and absent from the frame list.
    #[test]
    fn last_frame_split_keeps_worker_spans_out_of_the_frame_thread() {
        let profiler = FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        let d = dispatch.clone();
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&d, || {
                mark_main_thread();
                let _main = tracing::info_span!("split_main_work").entered();
                spin(Duration::from_millis(1));
            });
        })
        .join()
        .expect("main thread");
        let d = dispatch.clone();
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&d, || {
                let _job = tracing::info_span!("split_worker_job").entered();
                spin(Duration::from_millis(1));
            });
        })
        .join()
        .expect("worker thread");
        let d = dispatch.clone();
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&d, || {
                mark_render_thread();
                let _render = tracing::info_span!("split_render_work").entered();
                spin(Duration::from_millis(1));
            });
        })
        .join()
        .expect("render thread");
        profiler.end_frame();
        let (frame, background) = profiler.last_frame_split();
        assert!(time_of(&frame, "split_main_work").is_some(), "{frame:?}");
        assert!(time_of(&frame, "split_render_work").is_some(), "{frame:?}");
        assert_eq!(time_of(&frame, "split_worker_job"), None, "{frame:?}");
        assert!(
            time_of(&background, "split_worker_job").is_some(),
            "{background:?}"
        );
        assert_eq!(time_of(&background, "split_main_work"), None, "{background:?}");
        // The unsplit view still sees all three.
        let all = profiler.last_frame();
        for name in ["split_main_work", "split_worker_job", "split_render_work"] {
            assert!(time_of(&all, name).is_some(), "{name}: {all:?}");
        }
    }
}
