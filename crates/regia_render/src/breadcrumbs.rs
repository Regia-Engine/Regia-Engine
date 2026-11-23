// SPDX-License-Identifier: MPL-2.0
//! GPU breadcrumbs (spec 54): a small ring of per-frame pass markers recorded around every GPU
//! pass in the order the GPU receives them, plus submission counters, so a device-lost bundle can
//! name the passes the GPU was most likely inside.
//!
//! - A pass site opens a scope ([`pass`] on a [`CrumbList`], or [`frame_pass`] on the recording
//!   thread's own list). The scope writes `Open` now and `Close` on drop; names are interned once
//!   into a process-wide append-only table ([`PassTag`], at most [`MAX_NAMES`] names, then
//!   [`OVERFLOW_NAME`]).
//! - The submit path ([`Breadcrumbs::note_submit`]) appends the submitted lists to the frame
//!   record in buffer order and stamps them with the submission index. The record is therefore in
//!   GPU order, and only the submit path writes it.
//! - The [`BreadcrumbRing`] keeps the newest records (default 8). Each slot is a `Mutex`. A full
//!   [`Breadcrumbs::snapshot`] retries a busy slot briefly, then skips it. The frame-thread trace
//!   observer uses [`Breadcrumbs::for_each_fresh_pass`], which skips a busy slot at once and does
//!   not copy frames the caller has already recorded.
//! - Submission counters: `submitted`, `completed` (highest index such that it and every earlier
//!   one completed; a 64-entry bitmap window) and `acknowledged_frame`.
//! - [`BreadcrumbReport::capture`] states the evidence level it achieved ([`EvidenceLevel`]) and
//!   never implies more.
//!
//! Every [`Breadcrumbs`] is an instance; the process uses [`global`]. Tests build their own.
//! Timestamp backing (spec 54 § 2.3, ADR-2307): with [`TimestampMode::WhenSupported`] on a
//! device with `timestamp_query`, the frame turns its `GpuTimings` query set on and every
//! resolved frame calls [`Breadcrumbs::note_resolved_frame`]; tests script it through
//! [`GpuProgressSource`] and [`Breadcrumbs::note_timestamps`]. [`crash_trail`] is the text the
//! crash report (`regia_platform::crash`, uploaded by `regia_crash`) carries.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, TryLockError};

use serde::{Deserialize, Serialize};

/// Format version of [`BreadcrumbReport`] (`gpu_breadcrumbs.json`).
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// Most markers one [`FrameCrumbs`] keeps (a frame of about 150 passes uses 300).
pub const MAX_MARKERS: usize = 512;

/// Most distinct pass names; the next one maps to [`OVERFLOW_NAME`].
pub const MAX_NAMES: usize = 1024;

/// The name of tag 0, which every name past [`MAX_NAMES`] maps to.
pub const OVERFLOW_NAME: &str = "overflow";

/// Default number of frames the ring keeps.
pub const DEFAULT_RING_FRAMES: u8 = 8;

/// Range of [`BreadcrumbConfig::ring_frames`].
pub const RING_FRAMES_RANGE: std::ops::RangeInclusive<u8> = 2..=64;

/// Size of the completion bitmap window, submissions.
pub const COMPLETION_WINDOW: u64 = 64;

/// Most bytes of the bundle entry (`gpu_breadcrumbs.json`).
pub const REPORT_MAX_BYTES: usize = 64 * 1024;

/// Interned pass name; stable for the process life. Reports carry names, never bare tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PassTag(pub u16);

/// Index of one `queue.submit` call, starting at 1; 0 is "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SubmissionId(pub u64);

/// Whether scopes also write GPU timestamps (v0.3.x; the setting is read by the timestamp backing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimestampMode {
    /// No timestamps.
    #[default]
    Off,
    /// Timestamps when the device supports them in encoders.
    WhenSupported,
}

/// Breadcrumb settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreadcrumbConfig {
    /// Whether scopes record. Off: one relaxed load per pass, nothing else.
    pub enabled: bool,
    /// Frames the ring keeps ([`RING_FRAMES_RANGE`], clamped).
    pub ring_frames: u8,
    /// Timestamp backing.
    pub timestamps: TimestampMode,
}

impl Default for BreadcrumbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ring_frames: DEFAULT_RING_FRAMES,
            timestamps: TimestampMode::Off,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct NameTable {
    names: Vec<String>,
    index: HashMap<String, u16>,
}

impl NameTable {
    fn new() -> Self {
        let mut t = Self::default();
        t.names.push(OVERFLOW_NAME.to_owned());
        t
    }

    fn intern(&mut self, name: &str) -> PassTag {
        if let Some(&tag) = self.index.get(name) {
            return PassTag(tag);
        }
        if self.names.len() > MAX_NAMES {
            return PassTag(0);
        }
        // Tag 0 is the overflow name; real names are 1..=MAX_NAMES.
        let tag = u16::try_from(self.names.len()).unwrap_or(0);
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), tag);
        PassTag(tag)
    }

    fn name(&self, tag: PassTag) -> &str {
        self.names
            .get(usize::from(tag.0))
            .map_or(OVERFLOW_NAME, String::as_str)
    }
}

// ---------------------------------------------------------------------------------------------
// Marker lists
// ---------------------------------------------------------------------------------------------

/// One marker: a pass opened or closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    /// The pass.
    pub tag: PassTag,
    /// `true` for `Open`, `false` for `Close`.
    pub open: bool,
}

/// Markers of one command-buffer segment, owned by whoever records it. Preallocated, so a write
/// is one store into spare capacity; past [`MAX_MARKERS`] the newest markers are dropped.
#[derive(Debug)]
pub struct CrumbList {
    markers: Vec<u64>,
    truncated: bool,
}

const OPEN_BIT: u64 = 1 << 32;

impl CrumbList {
    /// An empty list with room for [`MAX_MARKERS`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            markers: Vec::with_capacity(MAX_MARKERS),
            truncated: false,
        }
    }

    fn push(&mut self, tag: PassTag, open: bool) {
        if self.markers.len() >= MAX_MARKERS {
            self.truncated = true;
            return;
        }
        self.markers
            .push(u64::from(tag.0) | if open { OPEN_BIT } else { 0 });
    }

    /// Markers written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.markers.len()
    }

    /// Whether no marker was written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// Forgets all markers (keeps the allocation).
    pub fn clear(&mut self) {
        self.markers.clear();
        self.truncated = false;
    }

    /// The markers in write order.
    pub fn markers(&self) -> impl Iterator<Item = Marker> + '_ {
        self.markers.iter().map(|&m| Marker {
            tag: PassTag((m & 0xFFFF) as u16),
            open: m & OPEN_BIT != 0,
        })
    }
}

impl Default for CrumbList {
    fn default() -> Self {
        Self::new()
    }
}

/// A scope that writes `Open` on creation and `Close` on drop. Inactive (no write) when
/// breadcrumbs are off.
#[must_use = "the pass is closed when the scope drops"]
pub struct PassScope<'a> {
    active: Option<(&'a mut CrumbList, PassTag)>,
}

impl Drop for PassScope<'_> {
    fn drop(&mut self) {
        if let Some((list, tag)) = self.active.take() {
            list.push(tag, false);
        }
    }
}

/// Opens a pass scope named `name` on `list` in the [`global`] instance.
pub fn pass<'a>(list: &'a mut CrumbList, name: &'static str) -> PassScope<'a> {
    global().pass(list, name)
}

thread_local! {
    static FRAME_LIST: RefCell<CrumbList> = RefCell::new(CrumbList::new());
    /// Tags already interned by this thread, keyed by the name's address and length: after
    /// warm-up a frame-path scope takes no lock and allocates nothing (ADR-2307).
    static TAG_CACHE: RefCell<HashMap<(usize, usize), PassTag>> = RefCell::new(HashMap::new());
}

/// Name-table lookups taken by [`frame_pass`] (cache misses), process-wide. A steady frame adds
/// none after its first frame (the count-based budget test reads this).
static FRAME_PASS_INTERNS: AtomicU64 = AtomicU64::new(0);

/// How many times [`frame_pass`] had to intern a name (a thread's first use of that name).
#[must_use]
pub fn frame_pass_interns() -> u64 {
    FRAME_PASS_INTERNS.load(Ordering::Relaxed)
}

fn cached_tag(b: &Breadcrumbs, name: &'static str) -> PassTag {
    let key = (name.as_ptr() as usize, name.len());
    if let Some(tag) = TAG_CACHE.with(|c| c.borrow().get(&key).copied()) {
        return tag;
    }
    FRAME_PASS_INTERNS.fetch_add(1, Ordering::Relaxed);
    let tag = b.intern(name);
    TAG_CACHE.with(|c| c.borrow_mut().insert(key, tag));
    tag
}

/// A scope on the calling thread's own list (the frame thread's), drained at the next submit by
/// the tracked queue. Writes `Close` on drop.
#[must_use = "the pass is closed when the scope drops"]
pub struct FramePass {
    tag: Option<PassTag>,
}

impl Drop for FramePass {
    fn drop(&mut self) {
        if let Some(tag) = self.tag {
            FRAME_LIST.with(|l| l.borrow_mut().push(tag, false));
        }
    }
}

/// Opens a pass scope on the calling thread's list in the [`global`] instance. Pair of
/// [`pass`] for code that does not own a segment list.
pub fn frame_pass(name: &'static str) -> FramePass {
    let b = global();
    if !b.enabled() {
        return FramePass { tag: None };
    }
    let tag = cached_tag(b, name);
    FRAME_LIST.with(|l| l.borrow_mut().push(tag, true));
    #[cfg(feature = "test-hooks")]
    fault::note_pass_opened(name);
    FramePass { tag: Some(tag) }
}

/// [`frame_pass`] with a tag interned beforehand: the render graph interns its node ids when it
/// compiles (spec 54 § 2.2) and opens one of these per node. Inactive when breadcrumbs are off.
pub fn frame_pass_tag(tag: PassTag) -> FramePass {
    if !global().enabled() {
        return FramePass { tag: None };
    }
    FRAME_LIST.with(|l| l.borrow_mut().push(tag, true));
    #[cfg(feature = "test-hooks")]
    fault::note_tag_opened(tag);
    FramePass { tag: Some(tag) }
}

// ---------------------------------------------------------------------------------------------
// Records and the ring
// ---------------------------------------------------------------------------------------------

/// One submission's slice of a frame record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmissionRange {
    /// The submission.
    pub id: SubmissionId,
    /// Start marker index (inclusive).
    pub start: usize,
    /// End marker index (exclusive).
    pub end: usize,
}

/// The record of one frame, in GPU order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameCrumbs {
    /// The app frame counter value.
    pub frame: u64,
    /// Markers in GPU order.
    pub markers: Vec<Marker>,
    /// Marker ranges per submission.
    pub submissions: Vec<SubmissionRange>,
    /// Markers were dropped (more than [`MAX_MARKERS`]); the oldest were kept.
    pub truncated: bool,
}

impl FrameCrumbs {
    /// Names of the passes opened in `range`, in order.
    fn opened(&self, table: &NameTable, range: std::ops::Range<usize>) -> Vec<String> {
        self.markers[range.start.min(self.markers.len())..range.end.min(self.markers.len())]
            .iter()
            .filter(|m| m.open)
            .map(|m| table.name(m.tag).to_owned())
            .collect()
    }
}

/// The newest frame records. Slot `i` holds record number `n` with `n % ring_frames == i`.
pub struct BreadcrumbRing {
    slots: Vec<Mutex<FrameCrumbs>>,
}

impl BreadcrumbRing {
    fn new() -> Self {
        Self {
            slots: (0..*RING_FRAMES_RANGE.end())
                .map(|_| Mutex::new(FrameCrumbs::default()))
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Submission tracking
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Tracker {
    /// Highest `n` such that submissions 1..=n completed.
    completed: u64,
    /// Bit `i`: submission `completed + 1 + i` completed.
    window: u64,
    /// Last submission of each recent frame, oldest first.
    frame_last: VecDeque<(u64, u64)>,
    acknowledged_frame: u64,
    /// After a resume: the submission count at resume; the first completion past it is a barrier.
    barrier: Option<u64>,
    completions_seen: u64,
}

impl Tracker {
    fn note_submit(&mut self, id: u64, frame: u64) {
        match self.frame_last.back_mut() {
            Some(last) if last.0 == frame => last.1 = id,
            _ => {
                if self.frame_last.len() >= 64 {
                    self.frame_last.pop_front();
                }
                self.frame_last.push_back((frame, id));
            }
        }
    }

    fn note_completed(&mut self, id: u64) {
        if let Some(at_resume) = self.barrier
            && id > at_resume
        {
            self.barrier = None;
            self.completed = self.completed.max(at_resume);
            self.window = 0;
        }
        if id <= self.completed {
            return;
        }
        let mut offset = id - self.completed - 1;
        if offset >= COMPLETION_WINDOW {
            // Too far ahead: everything older than the window is taken as completed.
            let advance = offset - (COMPLETION_WINDOW - 1);
            self.completed += advance;
            self.window = self
                .window
                .checked_shr(u32::try_from(advance).unwrap_or(64))
                .unwrap_or(0);
            offset = id - self.completed - 1;
        }
        if self.window & (1 << offset) == 0 {
            self.completions_seen += 1;
        }
        self.window |= 1 << offset;
        while self.window & 1 == 1 {
            self.window >>= 1;
            self.completed += 1;
        }
        while let Some(&(frame, last)) = self.frame_last.front() {
            if last > self.completed {
                break;
            }
            self.acknowledged_frame = frame;
            self.frame_last.pop_front();
        }
    }
}

/// The submission counters at a point in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionCounters {
    /// Submissions made.
    pub submitted: u64,
    /// Highest index such that it and every earlier one completed.
    pub completed: u64,
    /// The newest frame whose last submission completed.
    pub acknowledged_frame: u64,
}

impl SubmissionCounters {
    /// `submitted - completed`: the in-flight depth.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.submitted.saturating_sub(self.completed)
    }
}

/// GPU timestamp evidence: how many passes of `frame` (GPU order) had a resolved closing
/// timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampEvidence {
    /// The frame the timestamps belong to.
    pub frame: u64,
    /// Passes whose closing timestamp resolved, in GPU order.
    pub resolved_passes: u32,
}

/// A copy of the ring and counters for readers.
#[derive(Clone, Debug)]
pub struct BreadcrumbSnapshot {
    /// Whether the writer was enabled.
    pub enabled: bool,
    /// Records, newest first.
    pub frames: Vec<FrameCrumbs>,
    /// The counters.
    pub counters: SubmissionCounters,
    /// Completions ever seen (0: the completion callbacks never fired, so no fenced evidence).
    pub completions_seen: u64,
    /// Timestamp evidence, when the backing resolved any.
    pub timestamps: Option<TimestampEvidence>,
    /// Slots a reader could not lock.
    pub skipped_slots: u32,
    /// The name table (index = tag).
    pub names: Vec<String>,
    /// Adapter summary set by [`Breadcrumbs::set_adapter`].
    pub adapter: AdapterReport,
}

impl BreadcrumbSnapshot {
    fn name(&self, tag: PassTag) -> &str {
        self.names
            .get(usize::from(tag.0))
            .map_or(OVERFLOW_NAME, String::as_str)
    }
}

// ---------------------------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------------------------

struct Builder {
    frame: u64,
    record_no: u64,
    current: FrameCrumbs,
    started: bool,
}

/// One breadcrumb writer: names, ring and counters.
pub struct Breadcrumbs {
    enabled: AtomicBool,
    ring_frames: AtomicU8,
    timestamps: AtomicU8,
    names: Mutex<NameTable>,
    ring: BreadcrumbRing,
    builder: Mutex<Builder>,
    submitted: AtomicU64,
    completed: AtomicU64,
    acknowledged: AtomicU64,
    tracker: Mutex<Tracker>,
    timestamp_evidence: Mutex<Option<TimestampEvidence>>,
    adapter: Mutex<AdapterReport>,
}

impl Default for Breadcrumbs {
    fn default() -> Self {
        Self::new(BreadcrumbConfig::default())
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Breadcrumbs {
    /// A writer with `config`.
    #[must_use]
    pub fn new(config: BreadcrumbConfig) -> Self {
        let b = Self {
            enabled: AtomicBool::new(config.enabled),
            ring_frames: AtomicU8::new(DEFAULT_RING_FRAMES),
            timestamps: AtomicU8::new(0),
            names: Mutex::new(NameTable::new()),
            ring: BreadcrumbRing::new(),
            builder: Mutex::new(Builder {
                frame: 0,
                record_no: 0,
                current: FrameCrumbs::default(),
                started: false,
            }),
            submitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            tracker: Mutex::new(Tracker::default()),
            timestamp_evidence: Mutex::new(None),
            adapter: Mutex::new(AdapterReport::default()),
        };
        b.configure(config);
        b
    }

    /// Applies `config`. Shrinking the ring hides older records; they are overwritten in time.
    pub fn configure(&self, config: BreadcrumbConfig) {
        let frames = config
            .ring_frames
            .clamp(*RING_FRAMES_RANGE.start(), *RING_FRAMES_RANGE.end());
        self.ring_frames.store(frames, Ordering::Relaxed);
        self.timestamps.store(
            u8::from(config.timestamps == TimestampMode::WhenSupported),
            Ordering::Relaxed,
        );
        self.enabled.store(config.enabled, Ordering::Relaxed);
    }

    /// Whether scopes record: one relaxed load.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The timestamp mode (v0.3.x backing reads it).
    #[must_use]
    pub fn timestamp_mode(&self) -> TimestampMode {
        if self.timestamps.load(Ordering::Relaxed) == 0 {
            TimestampMode::Off
        } else {
            TimestampMode::WhenSupported
        }
    }

    /// Interns `name` (graph node ids at compile, pass names at first use).
    pub fn intern(&self, name: &str) -> PassTag {
        locked(&self.names).intern(name)
    }

    /// The name of `tag`.
    #[must_use]
    pub fn name_of(&self, tag: PassTag) -> String {
        locked(&self.names).name(tag).to_owned()
    }

    /// Opens a scope named `name` on `list`.
    pub fn pass<'a>(&self, list: &'a mut CrumbList, name: &'static str) -> PassScope<'a> {
        if !self.enabled() {
            return PassScope { active: None };
        }
        let tag = self.intern(name);
        list.push(tag, true);
        PassScope {
            active: Some((list, tag)),
        }
    }

    /// Records the adapter the report names.
    pub fn set_adapter(&self, info: &wgpu::AdapterInfo) {
        *locked(&self.adapter) = AdapterReport::from_info(info);
    }

    /// Records `lists`, in order, as one submission of `frame`. Returns its index. Called by the
    /// submit path (frame thread) once per `queue.submit`; bumps `submitted` after the record
    /// is published.
    pub fn note_submit(&self, lists: &[&CrumbList], frame: u64) -> SubmissionId {
        let _span = tracing::trace_span!("breadcrumbs_publish").entered();
        let id = self.submitted.fetch_add(1, Ordering::Relaxed) + 1;
        let ring = usize::from(self.ring_frames.load(Ordering::Relaxed));
        {
            let mut b = locked(&self.builder);
            if !b.started || b.frame != frame {
                if b.started {
                    b.record_no += 1;
                }
                b.started = true;
                b.frame = frame;
                b.current = FrameCrumbs {
                    frame,
                    ..FrameCrumbs::default()
                };
            }
            let start = b.current.markers.len();
            for list in lists {
                for m in list.markers() {
                    if b.current.markers.len() >= MAX_MARKERS {
                        b.current.truncated = true;
                        break;
                    }
                    b.current.markers.push(m);
                }
                if list.truncated {
                    b.current.truncated = true;
                }
            }
            let end = b.current.markers.len();
            b.current.submissions.push(SubmissionRange {
                id: SubmissionId(id),
                start,
                end,
            });
            let slot = usize::try_from(b.record_no % ring as u64).unwrap_or(0);
            *locked(&self.ring.slots[slot]) = b.current.clone();
        }
        locked(&self.tracker).note_submit(id, frame);
        SubmissionId(id)
    }

    /// A submission finished on the GPU (from `Queue::on_submitted_work_done`). Duplicates and
    /// late arrivals are handled.
    pub fn note_completed(&self, id: SubmissionId) {
        let mut t = locked(&self.tracker);
        t.note_completed(id.0);
        self.completed.store(t.completed, Ordering::Relaxed);
        self.acknowledged.store(t.acknowledged_frame, Ordering::Relaxed);
    }

    /// The app resumed after a suspension (Android): work in flight at the suspension may never
    /// report, so the first completion past the current submission count is a barrier.
    pub fn note_resumed(&self) {
        locked(&self.tracker).barrier = Some(self.submitted.load(Ordering::Relaxed));
    }

    /// The pending resume barrier (the submission count at the last [`Self::note_resumed`]),
    /// until the first completion past it; and how many completions were seen so far.
    #[cfg(test)]
    pub(crate) fn resume_barrier(&self) -> (Option<u64>, u64) {
        let t = locked(&self.tracker);
        (t.barrier, t.completions_seen)
    }

    /// Records resolved timestamp evidence (the timestamp backing calls this).
    pub fn note_timestamps(&self, evidence: TimestampEvidence) {
        *locked(&self.timestamp_evidence) = Some(evidence);
    }

    /// The timestamp backing (spec 54 § 2.3, ADR-2307): the GPU wrote the closing timestamp of
    /// `frame`'s last stamped pass (`GpuTimings`' query set resolved with its end stamp set), so
    /// every pass of that frame's record finished. Records [`TimestampEvidence`] with all of its
    /// passes resolved. Ignored when the ring no longer holds the frame, when the timestamp mode
    /// is off, or when newer evidence is already stored. Returns whether it was recorded.
    pub fn note_resolved_frame(&self, frame: u64) -> bool {
        if self.timestamp_mode() == TimestampMode::Off || !self.enabled() {
            return false;
        }
        if locked(&self.timestamp_evidence).is_some_and(|e| e.frame >= frame) {
            return false;
        }
        let ring = usize::from(self.ring_frames.load(Ordering::Relaxed));
        let mut opened = None;
        for slot in &self.ring.slots[..ring] {
            let Ok(rec) = slot.try_lock() else {
                continue;
            };
            if rec.frame == frame && !rec.submissions.is_empty() {
                opened = Some(rec.markers.iter().filter(|m| m.open).count());
                break;
            }
        }
        let Some(passes) = opened else {
            return false;
        };
        self.note_timestamps(TimestampEvidence {
            frame,
            resolved_passes: u32::try_from(passes).unwrap_or(u32::MAX),
        });
        true
    }

    /// The counters now.
    #[must_use]
    pub fn counters(&self) -> SubmissionCounters {
        SubmissionCounters {
            submitted: self.submitted.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            acknowledged_frame: self.acknowledged.load(Ordering::Relaxed),
        }
    }

    /// A copy of the ring and counters. Never blocks for good: a slot another thread holds is
    /// retried briefly, then skipped (counted in `skipped_slots`).
    ///
    /// Copies every retained frame and the whole name table. The per-frame trace observer uses
    /// [`Self::for_each_fresh_pass`] instead.
    #[must_use]
    pub fn snapshot(&self) -> BreadcrumbSnapshot {
        let _span = tracing::trace_span!("breadcrumbs_resolve").entered();
        let mut frames = Vec::new();
        let mut skipped = 0;
        for slot in &self.ring.slots[..usize::from(self.ring_frames.load(Ordering::Relaxed))] {
            let mut tries = 0;
            loop {
                match slot.try_lock() {
                    Ok(rec) => {
                        if !rec.submissions.is_empty() {
                            frames.push(rec.clone());
                        }
                        break;
                    }
                    Err(TryLockError::Poisoned(p)) => {
                        let rec = p.into_inner();
                        if !rec.submissions.is_empty() {
                            frames.push(rec.clone());
                        }
                        break;
                    }
                    Err(TryLockError::WouldBlock) => {
                        tries += 1;
                        if tries > 50 {
                            skipped += 1;
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            }
        }
        // Newest first: by frame, then by the newest submission.
        frames.sort_by(|a, b| {
            let key = |f: &FrameCrumbs| (f.frame, f.submissions.last().map_or(0, |s| s.id.0));
            key(b).cmp(&key(a))
        });
        BreadcrumbSnapshot {
            enabled: self.enabled(),
            frames,
            counters: self.counters(),
            completions_seen: locked(&self.tracker).completions_seen,
            timestamps: *locked(&self.timestamp_evidence),
            skipped_slots: skipped,
            names: locked(&self.names).names.clone(),
            adapter: locked(&self.adapter).clone(),
        }
    }

    /// Calls `visit(frame, submission, markers)` for each submission whose id is greater than
    /// `after`. `markers` is that submission's slice only (empty when the recorded range is
    /// empty or inverted). A slot another thread holds is skipped at once, with no yield, and
    /// counted in the returned total.
    ///
    /// Does not clone the name table, the adapter, or a frame whose newest submission is already
    /// at or below `after`. Reports keep [`Self::snapshot`].
    #[must_use]
    pub fn for_each_fresh_pass(&self, after: u64, mut visit: impl FnMut(u64, u64, &[Marker])) -> u32 {
        let mut skipped = 0u32;
        let ring = usize::from(self.ring_frames.load(Ordering::Relaxed));
        for slot in &self.ring.slots[..ring] {
            match slot.try_lock() {
                Ok(rec) => visit_fresh(&rec, after, &mut visit),
                Err(TryLockError::Poisoned(p)) => visit_fresh(&p.into_inner(), after, &mut visit),
                Err(TryLockError::WouldBlock) => skipped = skipped.saturating_add(1),
            }
        }
        skipped
    }
}

/// Visits submissions in `rec` newer than `after`. A frame whose newest id is already recorded
/// is not copied.
fn visit_fresh(rec: &FrameCrumbs, after: u64, visit: &mut impl FnMut(u64, u64, &[Marker])) {
    let Some(newest) = rec.submissions.last() else {
        return;
    };
    if newest.id.0 <= after {
        return;
    }
    let n = rec.markers.len();
    for sub in &rec.submissions {
        if sub.id.0 <= after {
            continue;
        }
        let start = sub.start.min(n);
        let end = sub.end.min(n);
        if start > end {
            visit(rec.frame, sub.id.0, &[]);
            continue;
        }
        visit(rec.frame, sub.id.0, &rec.markers[start..end]);
    }
}

/// The process-wide writer.
pub fn global() -> &'static Breadcrumbs {
    static GLOBAL: OnceLock<Breadcrumbs> = OnceLock::new();
    GLOBAL.get_or_init(Breadcrumbs::default)
}

/// Whether the global writer records: one relaxed load.
#[must_use]
pub fn enabled() -> bool {
    global().enabled()
}

/// Applies `config` to the global writer.
pub fn configure(config: BreadcrumbConfig) {
    global().configure(config);
}

/// Interns `name` in the global table (graph node ids at compile).
pub fn intern(name: &str) -> PassTag {
    global().intern(name)
}

/// Records `lists` as one submission of `frame` in the global writer.
pub fn note_submit(lists: &[&CrumbList], frame: u64) -> SubmissionId {
    global().note_submit(lists, frame)
}

/// A submission completed (global writer).
pub fn note_completed(id: SubmissionId) {
    global().note_completed(id);
}

/// A copy of the global ring and counters.
#[must_use]
pub fn snapshot() -> BreadcrumbSnapshot {
    global().snapshot()
}

/// Walks the global writer's submissions newer than `after`. See
/// [`Breadcrumbs::for_each_fresh_pass`]. The returned count is slots skipped because they were busy.
#[must_use]
pub fn for_each_fresh_pass(after: u64, visit: impl FnMut(u64, u64, &[Marker])) -> u32 {
    global().for_each_fresh_pass(after, visit)
}

static CURRENT_FRAME: AtomicU64 = AtomicU64::new(0);

/// The frame being recorded (the frame path calls this once at frame start). Also clears the
/// calling thread's list, so passes recorded outside a tracked submit never pile up.
pub fn begin_frame(frame: u64) {
    CURRENT_FRAME.store(frame, Ordering::Relaxed);
    FRAME_LIST.with(|l| l.borrow_mut().clear());
    if let Some(observer) = FRAME_OBSERVER.get() {
        observer(frame);
    }
}

static FRAME_OBSERVER: OnceLock<fn(u64)> = OnceLock::new();

/// Installs the process's frame observer, called by [`begin_frame`] with the new frame number
/// (the trace recorder installs one, so its `frames` and `passes` streams follow the frame
/// without a system in the app's schedule). Only the first call wins; returns whether this one
/// did. Without an observer, [`begin_frame`] costs one acquire load more.
pub fn set_frame_observer(observer: fn(u64)) -> bool {
    FRAME_OBSERVER.set(observer).is_ok()
}

/// The `gpu breadcrumbs:` block for a crash report (ADR-2307): the report of the global ring
/// ([`BreadcrumbReport::crash_text`]: level, suspects, counters, the last 3 frames), with the
/// device loss when there was one. Empty when the writer is off or nothing was recorded, so a
/// report never claims evidence it lacks. Never blocks for good (the ring is read with
/// `try_lock`).
#[must_use]
pub fn crash_trail() -> String {
    let lost = crate::gpu_health::device_lost_info();
    let report = BreadcrumbReport::capture(&snapshot(), lost.as_ref());
    if report.level == EvidenceLevel::None {
        return String::new();
    }
    report.crash_text()
}

/// What the tracked queue calls for every submit: publishes the calling thread's list as one
/// submission of the current frame and returns a callback to run when the GPU finishes it.
/// `None` when breadcrumbs are off.
pub(crate) fn on_submit() -> Option<Box<dyn FnOnce() + Send + 'static>> {
    on_submit_with_id().map(|(_, done)| done)
}

/// [`on_submit`] with the submission's id: a test reads its own submission back by it
/// ([`SubmissionRange::id`]), whatever frame another test made current meanwhile (`begin_frame`
/// sets one process-wide frame number).
pub(crate) fn on_submit_with_id() -> Option<(SubmissionId, Box<dyn FnOnce() + Send + 'static>)> {
    let b = global();
    if !b.enabled() {
        return None;
    }
    let frame = CURRENT_FRAME.load(Ordering::Relaxed);
    #[cfg(feature = "test-hooks")]
    fault::fire_if_due(frame, b.counters().submitted + 1);
    let id = FRAME_LIST.with(|l| {
        let mut l = l.borrow_mut();
        let id = b.note_submit(&[&*l], frame);
        l.clear();
        id
    });
    #[cfg(feature = "test-hooks")]
    fault::fire_in_pass_if_due(b, frame, id);
    Some((
        id,
        Box::new(move || {
            #[cfg(feature = "test-hooks")]
            if fault::stalled(id.0) {
                return;
            }
            global().note_completed(id);
        }),
    ))
}

/// For a submit that does not go through the tracked queue (the window's frame tail, or a host
/// that runs a [`crate::RenderGraph`] on its own encoder and submits it itself): publishes the
/// calling thread's markers as one submission of the current frame and counts its completion,
/// like `TrackedQueue::submit`, without touching the frame's submit statistics. Call it right
/// after `queue.submit`. Nothing happens when breadcrumbs are off.
pub fn note_untracked_submit(queue: &wgpu::Queue) {
    if let Some(done) = on_submit() {
        queue.on_submitted_work_done(done);
    }
}

// ---------------------------------------------------------------------------------------------
// Progress source (test seam)
// ---------------------------------------------------------------------------------------------

/// Where completion and timestamp information comes from. wgpu in the engine
/// ([`WgpuProgress`]); a scripted source in tests ([`ScriptedProgress`]).
pub trait GpuProgressSource: Send + Sync {
    /// Runs `f` when submission `id` finished on the GPU (wgpu: `Queue::on_submitted_work_done`).
    fn on_work_done(&self, id: SubmissionId, f: Box<dyn FnOnce() + Send>);
    /// Nanoseconds per timestamp tick, when timestamps exist.
    fn timestamp_period_ns(&self) -> Option<f32>;
    /// Resolved timestamps of a frame slot (zero entries are unresolved).
    fn resolved_timestamps(&self, frame_slot: u32) -> Option<Vec<u64>>;
}

/// [`GpuProgressSource`] on a wgpu queue.
pub struct WgpuProgress {
    queue: wgpu::Queue,
}

impl WgpuProgress {
    /// Wraps `queue`.
    #[must_use]
    pub fn new(queue: wgpu::Queue) -> Self {
        Self { queue }
    }
}

impl GpuProgressSource for WgpuProgress {
    fn on_work_done(&self, _id: SubmissionId, f: Box<dyn FnOnce() + Send>) {
        self.queue.on_submitted_work_done(f);
    }
    fn timestamp_period_ns(&self) -> Option<f32> {
        Some(self.queue.get_timestamp_period())
    }
    fn resolved_timestamps(&self, _frame_slot: u32) -> Option<Vec<u64>> {
        None
    }
}

#[derive(Default)]
struct Script {
    stall_at: Option<u64>,
    held: Vec<(SubmissionId, Box<dyn FnOnce() + Send>)>,
    timestamps: Option<Vec<u64>>,
    period: Option<f32>,
    lost: Option<(&'static str, String)>,
}

/// A scripted [`GpuProgressSource`]: completes submissions normally, stops completing at one
/// (a stall), returns canned timestamps, and holds a scripted loss.
#[derive(Clone, Default)]
pub struct ScriptedProgress {
    script: Arc<Mutex<Script>>,
}

impl ScriptedProgress {
    /// A source that completes everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Submissions `>= id` stop completing (their callbacks are held).
    pub fn stall_at_submission(&self, id: u64) {
        locked(&self.script).stall_at = Some(id);
    }

    /// Canned timestamps: nonzero entries are resolved, in pass order.
    pub fn set_timestamps(&self, period_ns: f32, ticks: Vec<u64>) {
        let mut s = locked(&self.script);
        s.period = Some(period_ns);
        s.timestamps = Some(ticks);
    }

    /// Scripts the loss `reason` and `message` (see [`Self::take_loss`]).
    pub fn script_loss(&self, reason: &'static str, message: &str) {
        locked(&self.script).lost = Some((reason, message.to_owned()));
    }

    /// The scripted loss, once.
    #[must_use]
    pub fn take_loss(&self) -> Option<(&'static str, String)> {
        locked(&self.script).lost.take()
    }

    /// Callbacks currently held by the stall.
    #[must_use]
    pub fn held(&self) -> usize {
        locked(&self.script).held.len()
    }
}

impl GpuProgressSource for ScriptedProgress {
    fn on_work_done(&self, id: SubmissionId, f: Box<dyn FnOnce() + Send>) {
        let mut s = locked(&self.script);
        if s.stall_at.is_some_and(|at| id.0 >= at) {
            s.held.push((id, f));
        } else {
            drop(s);
            f();
        }
    }
    fn timestamp_period_ns(&self) -> Option<f32> {
        locked(&self.script).period
    }
    fn resolved_timestamps(&self, _frame_slot: u32) -> Option<Vec<u64>> {
        locked(&self.script).timestamps.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

/// What a report may claim, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    /// The last pass whose closing timestamp resolved before the loss is known.
    Timestamped,
    /// Submissions up to `completed` finished; the later ones were in flight.
    Fenced,
    /// CPU pass order only.
    Recorded,
    /// Writer disabled or nothing recorded.
    None,
}

impl EvidenceLevel {
    /// The level number of the spec's table (3 best, 0 none).
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::Timestamped => 3,
            Self::Fenced => 2,
            Self::Recorded => 1,
            Self::None => 0,
        }
    }

    /// The snake case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Timestamped => "timestamped",
            Self::Fenced => "fenced",
            Self::Recorded => "recorded",
            Self::None => "none",
        }
    }
}

/// The adapter a report names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterReport {
    /// Adapter name.
    pub name: String,
    /// Backend name.
    pub backend: String,
    /// Device type name.
    pub device_type: String,
    /// Driver name.
    pub driver: String,
    /// Driver version text.
    pub driver_info: String,
    /// PCI vendor id.
    pub vendor: u32,
    /// PCI device id.
    pub device: u32,
}

impl AdapterReport {
    /// From wgpu's adapter info.
    #[must_use]
    pub fn from_info(info: &wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            backend: format!("{:?}", info.backend),
            device_type: format!("{:?}", info.device_type),
            driver: info.driver.clone(),
            driver_info: info.driver_info.clone(),
            vendor: info.vendor,
            device: info.device,
        }
    }
}

/// The loss a report describes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LostSummary {
    /// [`crate::gpu_health::DeviceLostInfo::reason`].
    pub reason: String,
    /// The driver's message, verbatim (may be empty).
    pub message: String,
    /// The frame at the callback (the newest recorded frame).
    pub frame: u64,
    /// Unix time of the capture, milliseconds.
    pub unix_ms: u64,
}

/// A pass the GPU was possibly inside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspectPass {
    /// Pass name.
    pub name: String,
    /// Frame.
    pub frame: u64,
    /// Submission.
    pub submission: u64,
}

/// One frame of a report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameReport {
    /// Frame.
    pub frame: u64,
    /// Pass names in GPU order (Open markers).
    pub passes: Vec<String>,
    /// Submissions of the frame: `[id, first pass index, end pass index]`.
    pub submissions: Vec<[u64; 3]>,
    /// Markers were dropped.
    pub truncated: bool,
}

/// The `gpu_breadcrumbs.json` bundle entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreadcrumbReport {
    /// [`REPORT_FORMAT_VERSION`].
    pub format_version: u32,
    /// The achieved evidence level.
    pub level: EvidenceLevel,
    /// The adapter.
    pub adapter: AdapterReport,
    /// The loss, when the report is for one.
    pub lost: Option<LostSummary>,
    /// Submission counters.
    pub counters: SubmissionCounters,
    /// Passes the GPU was most likely inside, in order.
    pub suspects: Vec<SuspectPass>,
    /// Frames, newest first.
    pub frames: Vec<FrameReport>,
    /// Frames or markers were dropped.
    pub truncated: bool,
}

/// The `gpu-breadcrumbs` field-list fingerprint for `cargo xtask check-formats` (spec 54 § 4.4,
/// ADR-9070): every serde struct of [`BreadcrumbReport`] and the [`EvidenceLevel`] names.
#[must_use]
pub fn gpu_breadcrumbs_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<BreadcrumbReport>(),
        regia_core::schema::serde_field_names::<AdapterReport>(),
        regia_core::schema::serde_field_names::<LostSummary>(),
        regia_core::schema::serde_field_names::<SubmissionCounters>(),
        regia_core::schema::serde_field_names::<SuspectPass>(),
        regia_core::schema::serde_field_names::<FrameReport>(),
        regia_core::schema::serde_variant_names::<EvidenceLevel>(),
    ])
}

/// The versions a `gpu-breadcrumbs` migration step leaves: none. It is a diagnostic output, never
/// rewritten; a layout change bumps [`REPORT_FORMAT_VERSION`] and the reader keeps the old
/// decoder (spec 54 § 4.4).
#[must_use]
pub fn gpu_breadcrumbs_migration_sources() -> Vec<u32> {
    Vec::new()
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl BreadcrumbReport {
    /// Parses a `gpu_breadcrumbs.json` entry (`gpu-breadcrumbs` v1, ADR-9070). Unknown keys are
    /// ignored (additive changes keep the version, spec 54 § 4.4).
    ///
    /// # Errors
    /// The JSON error, or a `format_version` newer than [`REPORT_FORMAT_VERSION`] (named in the
    /// message: this build reads only up to it).
    pub fn from_json(text: &str) -> Result<Self, String> {
        let report: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if report.format_version > REPORT_FORMAT_VERSION {
            return Err(format!(
                "gpu-breadcrumbs format_version {} is newer than this build reads ({REPORT_FORMAT_VERSION})",
                report.format_version
            ));
        }
        Ok(report)
    }

    /// Builds a report from `snapshot`. `lost` makes it a device-lost report.
    #[must_use]
    pub fn capture(snapshot: &BreadcrumbSnapshot, lost: Option<&crate::gpu_health::DeviceLostInfo>) -> Self {
        let table = NameTable {
            names: snapshot.names.clone(),
            index: HashMap::new(),
        };
        let level = if !snapshot.enabled || snapshot.frames.is_empty() {
            EvidenceLevel::None
        } else if snapshot.timestamps.is_some() {
            EvidenceLevel::Timestamped
        } else if snapshot.completions_seen > 0 {
            EvidenceLevel::Fenced
        } else {
            EvidenceLevel::Recorded
        };
        let mut suspects = Vec::new();
        match level {
            EvidenceLevel::Timestamped => {
                if let Some(ts) = snapshot.timestamps
                    && let Some(f) = snapshot.frames.iter().find(|f| f.frame == ts.frame)
                {
                    let mut index = 0u32;
                    for sub in &f.submissions {
                        for m in &f.markers[sub.start.min(f.markers.len())..sub.end.min(f.markers.len())] {
                            if m.open {
                                if index >= ts.resolved_passes {
                                    suspects.push(SuspectPass {
                                        name: snapshot.name(m.tag).to_owned(),
                                        frame: f.frame,
                                        submission: sub.id.0,
                                    });
                                }
                                index += 1;
                            }
                        }
                    }
                }
                // Frames after the resolved one (the timestamp backing reads frames two late,
                // so the loss frame itself is usually newer): their passes that had not
                // completed follow, oldest first (ADR-2307).
                if let Some(ts) = snapshot.timestamps {
                    for f in snapshot.frames.iter().rev().filter(|f| f.frame > ts.frame) {
                        for sub in &f.submissions {
                            if snapshot.completions_seen > 0 && sub.id.0 <= snapshot.counters.completed {
                                continue;
                            }
                            for name in f.opened(&table, sub.start..sub.end) {
                                suspects.push(SuspectPass {
                                    name,
                                    frame: f.frame,
                                    submission: sub.id.0,
                                });
                            }
                        }
                    }
                }
            }
            EvidenceLevel::Fenced => {
                // Oldest first: the passes of every submission that had not completed.
                for f in snapshot.frames.iter().rev() {
                    for sub in &f.submissions {
                        if sub.id.0 > snapshot.counters.completed {
                            for name in f.opened(&table, sub.start..sub.end) {
                                suspects.push(SuspectPass {
                                    name,
                                    frame: f.frame,
                                    submission: sub.id.0,
                                });
                            }
                        }
                    }
                }
            }
            EvidenceLevel::Recorded => {
                if let Some(f) = snapshot.frames.first() {
                    for sub in &f.submissions {
                        for name in f.opened(&table, sub.start..sub.end) {
                            suspects.push(SuspectPass {
                                name,
                                frame: f.frame,
                                submission: sub.id.0,
                            });
                        }
                    }
                }
            }
            EvidenceLevel::None => {}
        }
        let frames: Vec<FrameReport> = snapshot
            .frames
            .iter()
            .map(|f| {
                let mut passes = Vec::new();
                let mut subs = Vec::new();
                for sub in &f.submissions {
                    let first = passes.len() as u64;
                    passes.extend(f.opened(&table, sub.start..sub.end));
                    subs.push([sub.id.0, first, passes.len() as u64]);
                }
                FrameReport {
                    frame: f.frame,
                    passes,
                    submissions: subs,
                    truncated: f.truncated,
                }
            })
            .collect();
        let mut report = Self {
            format_version: REPORT_FORMAT_VERSION,
            level,
            adapter: snapshot.adapter.clone(),
            lost: lost.map(|l| LostSummary {
                reason: l.reason.to_owned(),
                message: l.message.clone(),
                frame: snapshot.frames.first().map_or(0, |f| f.frame),
                unix_ms: unix_ms(),
            }),
            counters: snapshot.counters,
            suspects,
            truncated: snapshot.skipped_slots > 0 || frames.iter().any(|f| f.truncated),
            frames,
        };
        report.fit(REPORT_MAX_BYTES);
        report
    }

    /// Drops the oldest frames (and caps the suspects) until the JSON fits `max_bytes`; sets
    /// `truncated` when it dropped anything.
    pub fn fit(&mut self, max_bytes: usize) {
        let size = |r: &Self| serde_json::to_vec(r).map_or(0, |v| v.len());
        while size(self) > max_bytes && self.frames.len() > 1 {
            self.frames.pop();
            self.truncated = true;
        }
        while size(self) > max_bytes && self.suspects.len() > 8 {
            self.suspects.truncate(self.suspects.len() / 2);
            self.truncated = true;
        }
        while size(self) > max_bytes && self.frames.first().is_some_and(|f| f.passes.len() > 8) {
            if let Some(f) = self.frames.first_mut() {
                f.passes.truncate(f.passes.len() / 2);
                f.submissions.clear();
                f.truncated = true;
            }
            self.truncated = true;
        }
    }

    /// The `gpu breadcrumbs:` block of `crash.txt`: level, suspects, counters and the last 3
    /// frames one line each.
    #[must_use]
    pub fn crash_text(&self) -> String {
        let mut out = format!(
            "gpu breadcrumbs: level {} ({}), submitted {}, completed {}, acknowledged frame {}\n",
            self.level.number(),
            self.level.name(),
            self.counters.submitted,
            self.counters.completed,
            self.counters.acknowledged_frame,
        );
        let names: Vec<&str> = self.suspects.iter().take(12).map(|s| s.name.as_str()).collect();
        out.push_str(&format!(
            "  suspects ({}): {}\n",
            self.suspects.len(),
            names.join(", ")
        ));
        for f in self.frames.iter().take(3) {
            out.push_str(&format!("  frame {}: {}\n", f.frame, f.passes.join(" > ")));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------------------------

/// Applies a [`BreadcrumbConfig`] to the process writer (breadcrumbs default on in every build
/// profile, spec 54 § 10); the writer is on without this plugin, which only changes the settings.
#[derive(Clone, Copy, Debug, Default)]
pub struct BreadcrumbsPlugin {
    /// The settings.
    pub config: BreadcrumbConfig,
}

impl regia_app::Plugin for BreadcrumbsPlugin {
    fn build(&self, _app: &mut regia_app::App) {
        configure(self.config);
    }
}

/// Fault injection for tests (spec 54 § 5.1), compiled only with the `test-hooks` feature. The
/// scripted modes drive the real loss path: the loss is raised through
/// [`crate::gpu_health::mark_device_lost`], so the hooks, the bundle and the exit code are the
/// shipped code.
#[cfg(feature = "test-hooks")]
pub mod fault {
    use super::{Ordering, locked};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    /// What to provoke.
    #[derive(Clone)]
    pub enum FaultKind {
        /// A scripted device loss with this reason and message.
        DeviceLost,
        /// Submissions from this index on stop completing, then the device is lost.
        StallAtSubmission(u64),
        /// An out-of-memory pressure signal.
        Oom,
        /// `Device::destroy()` on this device (wgpu's real loss callback).
        Destroy(wgpu::Device),
    }

    /// A fault armed to fire on the first submit of `at_frame` or later.
    #[derive(Clone)]
    pub struct Fault {
        /// What to provoke.
        pub kind: FaultKind,
        /// The frame it fires on (0: the next submit).
        pub at_frame: u64,
    }

    static ARMED: AtomicBool = AtomicBool::new(false);
    static PLAN: Mutex<Option<Fault>> = Mutex::new(None);
    static STALL_FROM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Arms `fault`, replacing an earlier one.
    pub fn arm(fault: Fault) {
        *locked(&PLAN) = Some(fault);
        ARMED.store(true, Ordering::Release);
    }

    /// Forgets any armed fault and lifts a stall.
    pub fn disarm() {
        *locked(&PLAN) = None;
        ARMED.store(false, Ordering::Release);
        STALL_FROM.store(0, Ordering::Relaxed);
        *locked(&IN_PASS_PLAN) = None;
        IN_PASS_ARMED.store(false, Ordering::Release);
        *locked(&IN_PASS_HIT) = None;
    }

    static IN_PASS_ARMED: AtomicBool = AtomicBool::new(false);
    static IN_PASS_PLAN: Mutex<Option<(String, u64)>> = Mutex::new(None);
    /// The pass an in-pass fault caught, waiting for the submit that carries it.
    static IN_PASS_HIT: Mutex<Option<String>> = Mutex::new(None);

    /// Arms a scripted device loss inside the pass `pass` (ADR-2307), replacing an earlier one:
    /// when a frame at or after `at_frame` opens that pass, the submission that carries it never
    /// completes, the scripted timestamps resolve every pass before it, and the device is lost
    /// right after that submit. The report's first suspect is this pass. [`disarm`] forgets it.
    pub fn arm_in_pass(pass: impl Into<String>, at_frame: u64) {
        *locked(&IN_PASS_PLAN) = Some((pass.into(), at_frame));
        IN_PASS_ARMED.store(true, Ordering::Release);
    }

    /// A frame-path scope opened `name`: catches an armed in-pass fault for it.
    pub(crate) fn note_pass_opened(name: &str) {
        if !IN_PASS_ARMED.load(Ordering::Acquire) {
            return;
        }
        let frame = super::CURRENT_FRAME.load(Ordering::Relaxed);
        let mut plan = locked(&IN_PASS_PLAN);
        let hit = plan
            .as_ref()
            .is_some_and(|(pass, at_frame)| pass == name && frame >= *at_frame);
        if hit {
            *plan = None;
            IN_PASS_ARMED.store(false, Ordering::Release);
            *locked(&IN_PASS_HIT) = Some(name.to_owned());
        }
    }

    /// [`note_pass_opened`] for a scope opened by tag (a render-graph node): the name is looked
    /// up only while an in-pass fault is armed.
    pub(crate) fn note_tag_opened(tag: super::PassTag) {
        if IN_PASS_ARMED.load(Ordering::Acquire) {
            note_pass_opened(&super::global().name_of(tag));
        }
    }

    /// After the submit `id` of `frame`: a caught in-pass fault fires now, so the
    /// record of that submission already holds the pass.
    pub(crate) fn fire_in_pass_if_due(b: &super::Breadcrumbs, frame: u64, id: super::SubmissionId) {
        let Some(pass) = locked(&IN_PASS_HIT).take() else {
            return;
        };
        STALL_FROM.store(id.0, Ordering::Relaxed);
        // Canned timestamps: every pass of the frame before the faulting one resolved.
        let snap = b.snapshot();
        let before = snap.frames.iter().find(|f| f.frame == frame).map_or(0, |f| {
            let opens: Vec<&str> = f
                .markers
                .iter()
                .filter(|m| m.open)
                .map(|m| snap.name(m.tag))
                .collect();
            opens.iter().rposition(|n| *n == pass).unwrap_or(0)
        });
        b.note_timestamps(super::TimestampEvidence {
            frame,
            resolved_passes: u32::try_from(before).unwrap_or(u32::MAX),
        });
        let _ = crate::gpu_health::mark_device_lost(
            "unknown",
            &format!("injected device loss in pass `{pass}` (test-hooks)"),
        );
    }

    /// Whether completions of submission `id` are being held by a stall.
    pub(crate) fn stalled(id: u64) -> bool {
        let from = STALL_FROM.load(Ordering::Relaxed);
        from != 0 && id >= from
    }

    /// Fires the armed fault when due. Called by the submit path.
    pub(crate) fn fire_if_due(frame: u64, next_submission: u64) {
        if !ARMED.load(Ordering::Acquire) {
            return;
        }
        let due = {
            let mut plan = locked(&PLAN);
            match plan.as_ref() {
                Some(f) if frame >= f.at_frame => plan.take(),
                _ => None,
            }
        };
        let Some(fault) = due else {
            return;
        };
        ARMED.store(false, Ordering::Release);
        match fault.kind {
            FaultKind::DeviceLost => {
                let _ = crate::gpu_health::mark_device_lost("unknown", "injected device loss (test-hooks)");
            }
            FaultKind::StallAtSubmission(at) => {
                STALL_FROM.store(at.max(next_submission), Ordering::Relaxed);
                let _ = crate::gpu_health::mark_device_lost(
                    "unknown",
                    "injected stall then device loss (test-hooks)",
                );
            }
            FaultKind::Oom => {
                let _ = crate::gpu_health::signal_pressure(crate::gpu_health::PressureSource::OutOfMemory);
            }
            FaultKind::Destroy(device) => device.destroy(),
        }
    }
}
