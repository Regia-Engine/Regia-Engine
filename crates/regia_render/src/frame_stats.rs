// SPDX-License-Identifier: MPL-2.0
//! Frame-path counters.
//!
//! Pipeline creation, bind-group creation, blocking `Device::poll(Wait)`,
//! `Queue::submit`, `Buffer::map_async`, buffer and texture allocation and upload bytes
//! increment these; [`trace`] also logs each create with its label (the hitch hunt). A frame-path test
//! renders past warm-up and asserts pipelines and bind groups stay put, and a
//! steady frame submits at most twice. Auto exposure maps one histogram slot
//! after that submit. Timestamp slots map only when GPU timing is on.

// The thread-local initializers below are already `const`; clippy's `missing_const_for_thread_local`
// still fires on aarch64-linux-android (no native TLS there), so the module allows it on Android only.
#![cfg_attr(target_os = "android", allow(clippy::missing_const_for_thread_local))]

use crate::frame_work::counted::*;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

// Thread-local so a test's delta ignores pipeline creates on other test threads.
thread_local! {
    static PIPELINE_CREATES: Cell<u64> = const { Cell::new(0) };
    static BLOCKING_POLLS: Cell<u64> = const { Cell::new(0) };
    static BIND_GROUP_CREATES: Cell<u64> = const { Cell::new(0) };
    static SUBMITS: Cell<u64> = const { Cell::new(0) };
    static MAP_ASYNCS: Cell<u64> = const { Cell::new(0) };
    static BUFFER_CREATES: Cell<u64> = const { Cell::new(0) };
    static BUFFER_BYTES: Cell<u64> = const { Cell::new(0) };
    static TEXTURE_CREATES: Cell<u64> = const { Cell::new(0) };
    static TEXTURE_BYTES: Cell<u64> = const { Cell::new(0) };
    static UPLOAD_BYTES: Cell<u64> = const { Cell::new(0) };
    static INLINE_BAKES: Cell<u64> = const { Cell::new(0) };
    // `Some` while a hitch hunt traces the frame path ([`trace`]): every counted create, by label.
    static TRACE: RefCell<Option<Vec<FramePathEvent>>> = const { RefCell::new(None) };
}

// Process-wide mirrors of the thread-local counters, in `FramePathCounters` field order: the
// runtime renders on whichever thread the ECS executor picks, so `play.hitch_report` reads these.
static GLOBAL: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>, slot: usize, by: u64) {
    counter.with(|count| count.set(count.get().saturating_add(by)));
    GLOBAL[slot].fetch_add(by, Ordering::Relaxed);
}

/// Frame-path device calls of every thread since start (the runtime's view; tests use the
/// thread-local [`counters`]).
#[must_use]
pub fn process_counters() -> FramePathCounters {
    let at = |slot: usize| GLOBAL[slot].load(Ordering::Relaxed);
    FramePathCounters {
        pipelines: at(0),
        bind_groups: at(1),
        blocking_polls: at(2),
        submits: at(3),
        map_asyncs: at(4),
        buffers: at(5),
        buffer_bytes: at(6),
        textures: at(7),
        texture_bytes: at(8),
        upload_bytes: at(9),
        inline_bakes: at(10),
    }
}

fn record(kind: FramePathKind, label: Option<&str>, bytes: u64) {
    record_detailed(kind, label, bytes, CreateDetail::default());
}

fn record_detailed(kind: FramePathKind, label: Option<&str>, bytes: u64, detail: CreateDetail) {
    if kind != FramePathKind::BlockingPoll {
        remember_create(kind, label, bytes, detail);
    }
    TRACE.with(|trace| {
        if let Some(events) = trace.borrow_mut().as_mut() {
            events.push(FramePathEvent {
                kind,
                label: label.unwrap_or("<unlabeled>").to_owned(),
                bytes,
                detail,
            });
        }
    });
}

/// What a pipeline creation was for (ADR-7885): the after-warm-up report names the target a
/// renderer drew into, the thread that compiled it and the creation site, not only the label,
/// so a host log says which target or renderer the warm never saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateDetail {
    /// Format of the first colour target (render pipelines that have one).
    pub format: Option<wgpu::TextureFormat>,
    /// Depth/stencil format (render pipelines that have one).
    pub depth_format: Option<wgpu::TextureFormat>,
    /// MSAA sample count (render pipelines; 0 for compute pipelines and other creations).
    pub samples: u32,
    /// Created on a pipeline-warm worker or a background builder (a warm job, a deferred variant,
    /// a material-graph set), not on a thread that records frames.
    pub warm_worker: bool,
    /// Source location of the `TrackedDevice` call that created it.
    pub site: Option<&'static std::panic::Location<'static>>,
    /// The forward pipeline-set key axes this creation was the first of on its renderer
    /// ([`KeyAxes`]); empty outside a forward set.
    pub new_axes: KeyAxes,
}

/// Axes of the renderer's forward pipeline-set key ([`CreateDetail::new_axes`]): which part of
/// the key no set of that renderer had yet when a set was built (a new colour format, MSAA, cull
/// mode, ...). Empty for creations outside a forward set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyAxes(u8);

impl KeyAxes {
    /// No axis: not a forward set, or a set whose every axis value was known.
    pub const NONE: Self = Self(0);
    /// The renderer had no set at all (its first one).
    pub const FIRST: Self = Self(1);
    /// A colour format no set of the renderer had.
    pub const FORMAT: Self = Self(1 << 1);
    /// An MSAA sample count no set of the renderer had.
    pub const SAMPLES: Self = Self(1 << 2);
    /// The shader sRGB encode (a non-sRGB LDR target).
    pub const ENCODE: Self = Self(1 << 3);
    /// The cull-none (double-sided) twin.
    pub const DOUBLE_SIDED: Self = Self(1 << 4);
    /// The ray-traced sun's forward module.
    pub const SUN_RT: Self = Self(1 << 5);
    /// The studio-off forward module.
    pub const STUDIO_OFF: Self = Self(1 << 6);
    /// The many-light (ReSTIR DI) opaque colour pipeline.
    pub const RESTIR_DI: Self = Self(1 << 7);

    const NAMED: [(Self, &'static str); 8] = [
        (Self::FIRST, "first_set"),
        (Self::FORMAT, "format"),
        (Self::SAMPLES, "samples"),
        (Self::ENCODE, "encode"),
        (Self::DOUBLE_SIDED, "double_sided"),
        (Self::SUN_RT, "sun_rt"),
        (Self::STUDIO_OFF, "studio_off"),
        (Self::RESTIR_DI, "restir_di"),
    ];

    /// Both sets of axes.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every axis of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no axis is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The axes' names (`first_set`, `format`, `samples`, `encode`, `double_sided`, `sun_rt`,
    /// `studio_off`, `restir_di`), in that order.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMED
            .into_iter()
            .filter(move |(axis, _)| !axis.is_empty() && self.contains(*axis))
            .map(|(_, name)| name)
    }
}

thread_local! {
    /// The forward-set key axes of the set being built on this thread ([`KeyAxes`]).
    static SET_AXES: Cell<KeyAxes> = const { Cell::new(KeyAxes::NONE) };
}

/// Makes `axes` the new-key axes of every pipeline this thread creates until the returned guard
/// drops (one forward set's build). Helper threads of `pipeline_warm::join` inherit it.
pub(crate) fn key_axes_scope(axes: KeyAxes) -> KeyAxesScope {
    KeyAxesScope(SET_AXES.with(|cell| cell.replace(axes)))
}

/// The new-key axes this thread charges its pipelines to ([`key_axes_scope`]).
pub(crate) fn current_key_axes() -> KeyAxes {
    SET_AXES.with(Cell::get)
}

/// Restores the previous new-key axes when dropped ([`key_axes_scope`]).
pub(crate) struct KeyAxesScope(KeyAxes);

impl Drop for KeyAxesScope {
    fn drop(&mut self) {
        SET_AXES.with(|cell| cell.set(self.0));
    }
}

/// Bytes of a [`CreateRecord`]'s label; longer labels are cut at a character boundary.
pub const CREATE_LABEL_BYTES: usize = 64;
/// Creations [`recent_creates`] remembers (process-wide, newest kept).
pub const RECENT_CREATES: usize = 32;

/// One pipeline, bind group, buffer or texture creation, remembered without allocating so a
/// hitch report can name what a slow frame created ([`recent_creates`]).
#[derive(Clone, Copy, Debug)]
pub struct CreateRecord {
    /// Process-wide sequence number (1-based; [`create_seq`] is the newest).
    pub seq: u64,
    /// The call.
    pub kind: FramePathKind,
    /// Bytes allocated (buffers, textures), 0 otherwise.
    pub bytes: u64,
    /// Target, thread and site of a pipeline creation (ADR-7885); default for other kinds.
    pub detail: CreateDetail,
    label: [u8; CREATE_LABEL_BYTES],
    label_len: u8,
}

impl CreateRecord {
    const EMPTY: Self = Self {
        seq: 0,
        kind: FramePathKind::Pipeline,
        bytes: 0,
        detail: CreateDetail {
            format: None,
            depth_format: None,
            samples: 0,
            warm_worker: false,
            site: None,
            new_axes: KeyAxes::NONE,
        },
        label: [0; CREATE_LABEL_BYTES],
        label_len: 0,
    };

    /// The descriptor's label (`<unlabelled>` without one), possibly cut short.
    #[must_use]
    pub fn label(&self) -> &str {
        std::str::from_utf8(&self.label[..usize::from(self.label_len)]).unwrap_or("")
    }
}

struct RecentCreates {
    slots: [CreateRecord; RECENT_CREATES],
    seq: u64,
}

static RECENT: std::sync::Mutex<RecentCreates> = std::sync::Mutex::new(RecentCreates {
    slots: [CreateRecord::EMPTY; RECENT_CREATES],
    seq: 0,
});

/// The opt-in log of every pipeline creation of the process ([`log_pipeline_creates`]).
static PIPELINE_LOG: std::sync::Mutex<Option<Vec<(String, CreateDetail)>>> = std::sync::Mutex::new(None);

/// Starts (an empty log) or stops (drops it) the process-wide log of pipeline creations, on
/// every thread: warm workers, background builders and frame threads (ADR-8453). Off by
/// default. A test compares what one run created with another's ([`take_pipeline_creates`]).
pub fn log_pipeline_creates(enabled: bool) {
    let mut log = PIPELINE_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *log = enabled.then(Vec::new);
}

/// The pipeline creations logged since the log started or was last taken: the full label and
/// [`CreateDetail`] of each, in creation order. Empty when the log is off.
#[must_use]
pub fn take_pipeline_creates() -> Vec<(String, CreateDetail)> {
    PIPELINE_LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .map(std::mem::take)
        .unwrap_or_default()
}

fn remember_create(kind: FramePathKind, label: Option<&str>, bytes: u64, detail: CreateDetail) {
    let label = label.unwrap_or("<unlabeled>");
    if kind == FramePathKind::Pipeline
        && let Some(log) = PIPELINE_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
    {
        log.push((label.to_owned(), detail));
    }
    let mut cut = label.len().min(CREATE_LABEL_BYTES);
    while !label.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut ring = RECENT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    ring.seq += 1;
    let seq = ring.seq;
    let slot = &mut ring.slots[(seq as usize) % RECENT_CREATES];
    slot.seq = seq;
    slot.kind = kind;
    slot.bytes = bytes;
    slot.detail = detail;
    slot.label[..cut].copy_from_slice(&label.as_bytes()[..cut]);
    slot.label_len = cut as u8;
}

/// Sequence number of the newest creation (0 before any).
#[must_use]
pub fn create_seq() -> u64 {
    RECENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .seq
}

/// Copies the remembered creations newer than `after_seq` into `out`, oldest first (at most
/// [`RECENT_CREATES`]; older ones were overwritten), and returns how many it copied. Never allocates.
pub fn recent_creates(after_seq: u64, out: &mut [CreateRecord; RECENT_CREATES]) -> usize {
    let ring = RECENT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = after_seq
        .saturating_add(1)
        .max(ring.seq.saturating_sub(RECENT_CREATES as u64 - 1));
    let mut n = 0;
    for seq in first..=ring.seq {
        out[n] = ring.slots[(seq as usize) % RECENT_CREATES];
        n += 1;
    }
    n
}

/// A blank buffer for [`recent_creates`].
#[must_use]
pub fn create_buffer() -> [CreateRecord; RECENT_CREATES] {
    [CreateRecord::EMPTY; RECENT_CREATES]
}

/// What a traced frame-path event was ([`FramePathEvent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FramePathKind {
    /// A render or compute pipeline was created.
    Pipeline,
    /// A bind group was created.
    BindGroup,
    /// A GPU buffer was allocated.
    Buffer,
    /// A texture was allocated.
    Texture,
    /// `Device::poll` waited for the GPU.
    BlockingPoll,
}

/// One device call the renderer made while tracing was on ([`trace`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramePathEvent {
    /// The call.
    pub kind: FramePathKind,
    /// The descriptor's label (`<unlabelled>` without one).
    pub label: String,
    /// Bytes allocated (buffers, textures), 0 otherwise.
    pub bytes: u64,
    /// Target, thread and site of a pipeline creation (ADR-7885); default for other kinds.
    pub detail: CreateDetail,
}

/// Turns the frame-path event trace on (an empty log) or off (drops the log) on this thread.
///
/// A soak or `play.hitch_report` traces so a frame that created a pipeline, bind group,
/// buffer or texture names it; off by default (no allocation per counted call).
pub fn trace(enabled: bool) {
    TRACE.with(|trace| *trace.borrow_mut() = enabled.then(Vec::new));
}

/// The events traced on this thread since the last call (empty when tracing is off).
#[must_use]
pub fn take_events() -> Vec<FramePathEvent> {
    TRACE.with(|trace| {
        trace
            .borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

#[cfg(test)]
thread_local! {
    // Labels of every bind group created on this thread, so a warm-frame test can name the culprit.
    static BIND_GROUP_LABELS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// One `create_render_pipeline` or `create_compute_pipeline` on this thread.
pub(crate) fn note_pipeline_create(label: Option<&str>) {
    note_pipeline_shape(label, None, None, 0, None);
}

/// [`note_pipeline_create`] with what the pipeline draws into and where it was created
/// ([`CreateDetail`]); the thread and the forward-set key axes are this thread's.
pub(crate) fn note_pipeline_shape(
    label: Option<&str>,
    format: Option<wgpu::TextureFormat>,
    depth_format: Option<wgpu::TextureFormat>,
    samples: u32,
    site: Option<&'static std::panic::Location<'static>>,
) {
    bump(&PIPELINE_CREATES, 0, 1);
    let detail = CreateDetail {
        format,
        depth_format,
        samples,
        warm_worker: crate::pipeline_warm::on_warm_thread(),
        site,
        new_axes: current_key_axes(),
    };
    record_detailed(FramePathKind::Pipeline, label, 0, detail);
}

/// One `Device::poll` that waits for the GPU (indefinite or timed) on this thread, called at
/// `site` (ADR-4050: the hitch report names where the waits come from).
pub(crate) fn note_blocking_poll(site: &'static std::panic::Location<'static>) {
    bump(&BLOCKING_POLLS, 2, 1);
    note_poll_site(site);
    record(FramePathKind::BlockingPoll, None, 0);
}

/// Distinct call sites [`blocking_poll_sites`] keeps (process-wide, first come). Waits from
/// sites past this many are counted in [`BlockingPollSites::unnamed`].
pub const BLOCKING_POLL_SITES: usize = 32;

/// One source location that called a blocking `TrackedDevice::poll`, and how often.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockingPollSite {
    /// Source file of the call, as the compiler names it (`crates/regia_render/src/...`).
    pub file: &'static str,
    /// Line of the call.
    pub line: u32,
    /// Blocking polls from this site since the process started.
    pub count: u64,
}

/// Every blocking-poll call site seen since the process started ([`blocking_poll_sites`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockingPollSites {
    /// Sites in order of their first wait.
    pub sites: Vec<BlockingPollSite>,
    /// Waits from sites that came after the table was full ([`BLOCKING_POLL_SITES`]).
    pub unnamed: u64,
}

impl BlockingPollSites {
    /// The waits since `earlier` (an older snapshot of the same process): sites with none are
    /// left out.
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        let sites = self
            .sites
            .iter()
            .filter_map(|site| {
                let before = earlier
                    .sites
                    .iter()
                    .find(|old| old.line == site.line && old.file == site.file)
                    .map_or(0, |old| old.count);
                let count = site.count.saturating_sub(before);
                (count > 0).then_some(BlockingPollSite { count, ..*site })
            })
            .collect();
        Self {
            sites,
            unnamed: self.unnamed.saturating_sub(earlier.unnamed),
        }
    }
}

struct PollSiteTable {
    slots: [Option<BlockingPollSite>; BLOCKING_POLL_SITES],
    unnamed: u64,
}

impl PollSiteTable {
    const EMPTY: Self = Self {
        slots: [None; BLOCKING_POLL_SITES],
        unnamed: 0,
    };

    /// Counts one wait from `file:line`. Never allocates: a fixed table scanned in order.
    fn note(&mut self, file: &'static str, line: u32) {
        for slot in &mut self.slots {
            match slot {
                Some(known) if known.line == line && known.file == file => {
                    known.count = known.count.saturating_add(1);
                    return;
                }
                Some(_) => {}
                None => {
                    *slot = Some(BlockingPollSite { file, line, count: 1 });
                    return;
                }
            }
        }
        self.unnamed = self.unnamed.saturating_add(1);
    }

    fn snapshot(&self) -> BlockingPollSites {
        BlockingPollSites {
            sites: self.slots.iter().flatten().copied().collect(),
            unnamed: self.unnamed,
        }
    }
}

static POLL_SITES: std::sync::Mutex<PollSiteTable> = std::sync::Mutex::new(PollSiteTable::EMPTY);

fn note_poll_site(site: &'static std::panic::Location<'static>) {
    POLL_SITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .note(site.file(), site.line());
}

/// Every source location that called a blocking `TrackedDevice::poll` since the process
/// started, with its count (all threads). Take one snapshot before and one after a window and
/// [`BlockingPollSites::since`] names the waits in between.
#[must_use]
pub fn blocking_poll_sites() -> BlockingPollSites {
    POLL_SITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .snapshot()
}

/// One `create_bind_group` on this thread.
pub(crate) fn note_bind_group(label: Option<&str>) {
    bump(&BIND_GROUP_CREATES, 1, 1);
    record(FramePathKind::BindGroup, label, 0);
    #[cfg(test)]
    BIND_GROUP_LABELS.with(|labels| {
        labels
            .borrow_mut()
            .push(label.unwrap_or("<unlabeled>").to_owned())
    });
}

/// One GPU buffer of `bytes` allocated on the frame path of this thread.
pub(crate) fn note_buffer(label: Option<&str>, bytes: u64) {
    bump(&BUFFER_CREATES, 5, 1);
    bump(&BUFFER_BYTES, 6, bytes);
    record(FramePathKind::Buffer, label, bytes);
}

/// One texture of about `bytes` allocated on the frame path of this thread.
pub(crate) fn note_texture(label: Option<&str>, bytes: u64) {
    bump(&TEXTURE_CREATES, 7, 1);
    bump(&TEXTURE_BYTES, 8, bytes);
    record(FramePathKind::Texture, label, bytes);
}

/// One CPU bake (atmosphere tables, sky IBL) run inside a frame on this thread after the
/// first: the work [`crate::FrameRenderer::set_async_sky`] moves to a worker.
pub(crate) fn note_inline_bake() {
    bump(&INLINE_BAKES, 10, 1);
}

/// `bytes` handed to `Queue::write_buffer` / `write_texture` on this thread.
pub(crate) fn note_upload(bytes: u64) {
    bump(&UPLOAD_BYTES, 9, bytes);
}

/// Bytes a texture of `desc` occupies (all mips and layers; block formats by block size).
#[must_use]
pub(crate) fn texture_bytes(desc: &wgpu::TextureDescriptor<'_>) -> u64 {
    let (bw, bh) = desc.format.block_dimensions();
    let block = u64::from(desc.format.block_copy_size(None).unwrap_or(4));
    let layers = u64::from(desc.size.depth_or_array_layers.max(1));
    let samples = u64::from(desc.sample_count.max(1));
    (0..desc.mip_level_count.max(1))
        .map(|mip| {
            let w = (desc.size.width >> mip).max(1).div_ceil(bw);
            let h = (desc.size.height >> mip).max(1).div_ceil(bh);
            u64::from(w) * u64::from(h) * block
        })
        .sum::<u64>()
        * layers
        * samples
}

/// Labels of the bind groups created on this thread, from the `skip`-th one on.
#[cfg(test)]
#[must_use]
pub(crate) fn bind_group_labels_from(skip: u64) -> Vec<String> {
    BIND_GROUP_LABELS.with(|labels| labels.borrow().iter().skip(skip as usize).cloned().collect())
}

/// One [`crate::device::TrackedQueue::submit`] on this thread.
pub(crate) fn note_submit() {
    bump(&SUBMITS, 3, 1);
}

/// One frame-path `map_async` on this thread.
pub(crate) fn note_map_async() {
    bump(&MAP_ASYNCS, 4, 1);
}

/// Pipelines created on this thread since start.
#[cfg(test)]
#[must_use]
pub(crate) fn pipeline_creates() -> u64 {
    PIPELINE_CREATES.with(Cell::get)
}

/// Pipelines created on this thread since start (any build).
#[must_use]
pub(crate) fn thread_pipeline_creates() -> u64 {
    PIPELINE_CREATES.with(Cell::get)
}

/// Charges `n` pipelines a helper thread created for this thread's work to this thread's
/// counter (the process-wide mirror already counted them), so a frame that builds pipelines in
/// parallel ([`crate::pipeline_warm::join`]) still shows them in its thread-local delta.
pub(crate) fn adopt_pipeline_creates(n: u64) {
    PIPELINE_CREATES.with(|count| count.set(count.get().saturating_add(n)));
}

/// Blocking polls on this thread since start.
#[cfg(test)]
#[must_use]
pub(crate) fn blocking_polls() -> u64 {
    BLOCKING_POLLS.with(Cell::get)
}

/// Bind groups created on this thread since start.
#[cfg(test)]
#[must_use]
pub(crate) fn bind_group_creates() -> u64 {
    BIND_GROUP_CREATES.with(Cell::get)
}

/// Submits on this thread since start.
///
/// Not test-only: [`crate::frame::FrameCpuTimings::submits`] is this counter's
/// delta for the frame, which the lab prints.
#[must_use]
pub(crate) fn submits() -> u64 {
    SUBMITS.with(Cell::get)
}

/// Frame-path `map_async` calls on this thread since start.
#[cfg(test)]
#[must_use]
pub(crate) fn map_asyncs() -> u64 {
    MAP_ASYNCS.with(Cell::get)
}

/// Frame-path device calls on this thread since start ([`crate::FramePathCounters`]).
#[must_use]
pub fn counters() -> FramePathCounters {
    FramePathCounters {
        pipelines: PIPELINE_CREATES.with(Cell::get),
        bind_groups: BIND_GROUP_CREATES.with(Cell::get),
        blocking_polls: BLOCKING_POLLS.with(Cell::get),
        submits: SUBMITS.with(Cell::get),
        map_asyncs: MAP_ASYNCS.with(Cell::get),
        buffers: BUFFER_CREATES.with(Cell::get),
        buffer_bytes: BUFFER_BYTES.with(Cell::get),
        textures: TEXTURE_CREATES.with(Cell::get),
        texture_bytes: TEXTURE_BYTES.with(Cell::get),
        upload_bytes: UPLOAD_BYTES.with(Cell::get),
        inline_bakes: INLINE_BAKES.with(Cell::get),
    }
}

/// Frame-path device calls the renderer made on one thread (ADR-035): a bench takes the
/// difference across its measured frames; pipelines and bind groups must not grow after
/// warm-up. Calls a shared draw makes on its own `wgpu::Device` are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramePathCounters {
    /// `create_render_pipeline` / `create_compute_pipeline`.
    pub pipelines: u64,
    /// `create_bind_group`.
    pub bind_groups: u64,
    /// `Device::poll` that waits.
    pub blocking_polls: u64,
    /// `TrackedQueue::submit`.
    pub submits: u64,
    /// Frame-path `map_async`.
    pub map_asyncs: u64,
    /// GPU buffers allocated (first-use uploads, reallocations on growth).
    pub buffers: u64,
    /// Bytes of those buffers.
    pub buffer_bytes: u64,
    /// Textures allocated (material uploads, target reallocations).
    pub textures: u64,
    /// Bytes of those textures.
    pub texture_bytes: u64,
    /// Bytes written through `write_buffer` / `write_texture` (staging-belt chunks included).
    pub upload_bytes: u64,
    /// CPU bakes (atmosphere tables, sky IBL; ~20–60 ms each) run inside a frame, not counting
    /// a planet's first tables.
    pub inline_bakes: u64,
}

impl FramePathCounters {
    /// `self − earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            pipelines: self.pipelines.saturating_sub(earlier.pipelines),
            bind_groups: self.bind_groups.saturating_sub(earlier.bind_groups),
            blocking_polls: self.blocking_polls.saturating_sub(earlier.blocking_polls),
            submits: self.submits.saturating_sub(earlier.submits),
            map_asyncs: self.map_asyncs.saturating_sub(earlier.map_asyncs),
            buffers: self.buffers.saturating_sub(earlier.buffers),
            buffer_bytes: self.buffer_bytes.saturating_sub(earlier.buffer_bytes),
            textures: self.textures.saturating_sub(earlier.textures),
            texture_bytes: self.texture_bytes.saturating_sub(earlier.texture_bytes),
            upload_bytes: self.upload_bytes.saturating_sub(earlier.upload_bytes),
            inline_bakes: self.inline_bakes.saturating_sub(earlier.inline_bakes),
        }
    }
}

/// Main-thread split of the last window frame (`render_frame` in `surface.rs`): what the
/// `Render` schedule spent waiting on the swapchain, recording, and presenting. Telemetry reads
/// it once per frame ([`window_phases`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowPhases {
    /// `Surface::get_current_texture` (blocks while every swapchain image is in flight, and on
    /// vsync with a full queue).
    pub acquire_ms: f32,
    /// Recording and submitting every window view (`FrameRenderer::render_into` and the frame
    /// clear).
    pub render_into_ms: f32,
    /// Overlay, display stage, splash, the tail submit and screenshot copies.
    pub tail_ms: f32,
    /// `Queue::present`.
    pub present_ms: f32,
    /// Publication count (1-based); unchanged when no window frame ran since the last read.
    pub seq: u64,
}

/// Where the main thread waited on the GPU in the last window frame (v0.1.14, the frame timeline's
/// step S0): the swapchain acquire, the present, and screenshot readbacks. Published with
/// [`WindowPhases`] (same `seq`); read it with [`frame_waits`]. Measurement only: nothing about
/// how a frame runs changes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct FrameWaits {
    /// `Surface::get_current_texture`, ms (the same number as [`WindowPhases::acquire_ms`]).
    pub acquire_ms: f32,
    /// `Queue::present`, ms (the same number as [`WindowPhases::present_ms`]).
    pub present_ms: f32,
    /// Blocking screenshot readbacks after the present (`screenshot::finish` waits for the GPU),
    /// ms; 0 in frames without a screenshot request.
    pub readback_ms: f32,
    /// [`WindowPhases::seq`] of the frame this describes (0 before the first window frame).
    pub seq: u64,
}

impl FrameWaits {
    /// Acquire, present and readback together: the frame's CPU wait on the GPU, ms.
    #[must_use]
    pub fn total_ms(&self) -> f32 {
        self.acquire_ms + self.present_ms + self.readback_ms
    }
}

/// The newest [`WindowPhases`] and its frame's screenshot readback wait, ms.
static WINDOW_PHASES: std::sync::Mutex<(WindowPhases, f32)> = std::sync::Mutex::new((
    WindowPhases {
        acquire_ms: 0.0,
        render_into_ms: 0.0,
        tail_ms: 0.0,
        present_ms: 0.0,
        seq: 0,
    },
    0.0,
));

pub(crate) fn set_window_phases(phases: WindowPhases, readback_ms: f32) {
    let mut slot = WINDOW_PHASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seq = slot.0.seq + 1;
    *slot = (WindowPhases { seq, ..phases }, readback_ms);
    drop(slot);
    // `render.frame_work` (ADR-2711): one relaxed load unless a reader asked.
    crate::frame_work::note_window_frame();
}

/// The newest window frame's [`WindowPhases`] (all zero with `seq` 0 before the first).
#[must_use]
pub fn window_phases() -> WindowPhases {
    WINDOW_PHASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0
}

/// The newest window frame's [`FrameWaits`] (all zero with `seq` 0 before the first).
#[must_use]
pub fn frame_waits() -> FrameWaits {
    let (phases, readback_ms) = *WINDOW_PHASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    FrameWaits {
        acquire_ms: phases.acquire_ms,
        present_ms: phases.present_ms,
        readback_ms,
        seq: phases.seq,
    }
}

/// Render-to-texture views of the newest `Render` schedule (`runtime::render_texture_views`),
/// for telemetry's `frame_summary.texture_views`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextureViewStats {
    /// Texture cameras extracted that frame.
    pub views: u32,
    /// Views rendered that frame (the others were paused or between refreshes).
    pub rendered: u32,
    /// CPU time of the rendered views, summed, ms.
    pub cpu_ms: f32,
    /// GPU time of the rendered views' newest timed frames, summed, ms (0 without GPU timing).
    pub gpu_ms: f32,
    /// Per rendered view: `(asset id, CPU ms, GPU ms)`.
    pub per_view: Vec<(u64, f32, f32)>,
    /// Publication count (1-based); unchanged when no texture-view pass ran since the last read.
    pub seq: u64,
}

static TEXTURE_VIEWS: std::sync::Mutex<TextureViewStats> = std::sync::Mutex::new(TextureViewStats {
    views: 0,
    rendered: 0,
    cpu_ms: 0.0,
    gpu_ms: 0.0,
    per_view: Vec::new(),
    seq: 0,
});

pub(crate) fn set_texture_view_stats(stats: TextureViewStats) {
    let mut slot = TEXTURE_VIEWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seq = slot.seq + 1;
    *slot = TextureViewStats { seq, ..stats };
}

/// Runs `f` on the newest frame's [`TextureViewStats`] without copying them (telemetry reads
/// it every frame).
pub fn with_texture_view_stats<R>(f: impl FnOnce(&TextureViewStats) -> R) -> R {
    f(&TEXTURE_VIEWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// The newest frame's [`TextureViewStats`] (all zero with `seq` 0 before the first).
#[must_use]
pub fn texture_view_stats() -> TextureViewStats {
    TEXTURE_VIEWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// Process-wide upload split for telemetry: belt bytes, texture bytes, mesh bytes, mesh buffers.
static UPLOAD_SPLIT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Bytes the staging belt wrote this frame (per-frame instance, uniform and draw data).
pub(crate) fn note_belt_upload(bytes: u64) {
    UPLOAD_SPLIT[0].fetch_add(bytes, Ordering::Relaxed);
}

/// Bytes written to textures (streaming levels, bakes).
pub(crate) fn note_texture_upload(bytes: u64) {
    UPLOAD_SPLIT[1].fetch_add(bytes, Ordering::Relaxed);
}

/// A mesh uploaded to the renderer's cache: its vertex + index bytes and the buffers created.
pub(crate) fn note_mesh_upload(bytes: u64, buffers: u64) {
    UPLOAD_SPLIT[2].fetch_add(bytes, Ordering::Relaxed);
    UPLOAD_SPLIT[3].fetch_add(buffers, Ordering::Relaxed);
}

/// Uploads by source since start (process-wide; telemetry reports per-window deltas).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadSplit {
    /// Every counted `write_buffer`/`write_texture` byte ([`FramePathCounters::upload_bytes`]).
    pub total_bytes: u64,
    /// Of those, the staging belt's per-frame data.
    pub belt_bytes: u64,
    /// Of those, texture writes.
    pub texture_bytes: u64,
    /// Of those, meshes uploaded to the renderer's mesh cache (new keys or changed sizes).
    pub mesh_bytes: u64,
    /// Vertex and index buffers created for those meshes.
    pub mesh_buffers: u64,
}

/// The process-wide [`UploadSplit`] since start.
#[must_use]
pub fn upload_split() -> UploadSplit {
    let at = |i: usize| UPLOAD_SPLIT[i].load(Ordering::Relaxed);
    UploadSplit {
        total_bytes: GLOBAL[9].load(Ordering::Relaxed),
        belt_bytes: at(0),
        texture_bytes: at(1),
        mesh_bytes: at(2),
        mesh_buffers: at(3),
    }
}

/// Bind group slots the draw counters track (0 view, 1 object, 2 material, 3 the pass extra).
pub const COUNTED_BIND_GROUPS: usize = 4;

/// The bind group slot of the material group: [`PassDrawCounts::material_batches`] counts runs
/// of draws by what is bound here.
const MATERIAL_GROUP: usize = 2;

/// Encoder calls the mesh draw loops of one pass made in one frame (bindless step 1, ADR-316):
/// what binding cost the frame paid, counted where the calls are recorded. Deterministic for a
/// given scene and view (no timing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassDrawCounts {
    /// Draw calls (`draw`, `draw_indexed`, one per `draw_indexed_indirect`).
    pub draws: u64,
    /// `set_pipeline` calls that bound a different pipeline than the one already bound.
    pub pipeline_switches: u64,
    /// `set_bind_group` calls that bound a different group (or offsets) than the one already
    /// bound, per group slot ([`COUNTED_BIND_GROUPS`]).
    pub bind_group_switches: [u64; COUNTED_BIND_GROUPS],
    /// Runs of consecutive draws that share one material bind group (slot 2): what a
    /// bindless renderer would merge. A draw with no material group bound starts no run.
    pub material_batches: u64,
}

impl PassDrawCounts {
    /// Bind group switches over every slot.
    #[must_use]
    pub fn bind_group_switches_total(&self) -> u64 {
        self.bind_group_switches.iter().sum()
    }

    /// `self + other`, per counter.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        let mut switches = self.bind_group_switches;
        for (sum, add) in switches.iter_mut().zip(other.bind_group_switches) {
            *sum += add;
        }
        Self {
            draws: self.draws + other.draws,
            pipeline_switches: self.pipeline_switches + other.pipeline_switches,
            bind_group_switches: switches,
            material_batches: self.material_batches + other.material_batches,
        }
    }
}

/// [`PassDrawCounts`] of every counted pass of the last main-view frame
/// ([`crate::FrameRenderer::draw_counts`], `render.get_frame_stats`).
///
/// Counted: the depth prepass, the opaque colour draws, the translucent draws (the glass front
/// layer included), every shadow job (cascades, atlas tiles, page clears) and the
/// visibility-buffer material resolve. Not counted: the shared terrain draw, the sky, and the
/// draws other modules record inside those passes (visibility-buffer rasters, translucent shadow
/// casters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameDrawCounts {
    /// The depth prepass.
    pub depth_prepass: PassDrawCounts,
    /// Opaque and masked colour draws of the forward pass.
    pub opaque: PassDrawCounts,
    /// Blended, transmissive and glass draws.
    pub translucent: PassDrawCounts,
    /// Every shadow pass of the frame (summed over cascades and atlas tiles).
    pub shadow: PassDrawCounts,
    /// The visibility-buffer material resolve.
    pub visbuffer_resolve: PassDrawCounts,
}

impl FrameDrawCounts {
    /// Every counted pass with its protocol name, in frame order.
    #[must_use]
    pub fn passes(&self) -> [(&'static str, PassDrawCounts); 5] {
        [
            ("shadow", self.shadow),
            ("depth_prepass", self.depth_prepass),
            ("visbuffer_resolve", self.visbuffer_resolve),
            ("opaque", self.opaque),
            ("translucent", self.translucent),
        ]
    }

    /// The sum over every counted pass.
    #[must_use]
    pub fn total(&self) -> PassDrawCounts {
        self.passes()
            .iter()
            .fold(PassDrawCounts::default(), |sum, (_, pass)| sum.plus(*pass))
    }
}

/// One draw call a frame-debugger capture recorded (ADR-8224): the pass it belongs to, what the
/// draw loop bound for it, and its counts. Recorded only on the main-view frame an armed pass
/// capture copies ([`crate::frame_capture::PassCaptureRequest`]) and only by the draw loops that
/// name their pass ([`DRAW_RECORD_PASSES`]); every other frame records nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrawRecord {
    /// The frame graph node the draw belongs to (`forward`, `depth_prepass`, `shadows`).
    pub pass: &'static str,
    /// The [`FrameDrawCounts`] pass that counted it (`opaque`, `translucent`, `depth_prepass`,
    /// `shadow`).
    pub counted: &'static str,
    /// What the draw loop bound as its pipeline, named by the loop (`standard`, `mask`,
    /// `material_graph`, `lightmap`, ...); `unnamed` when the loop bound one without a name.
    pub pipeline: &'static str,
    /// The mesh cache key (`FrameMesh::mesh_key`), when the loop knows it.
    pub mesh: Option<u64>,
    /// The material cache key (`FrameMesh::material_key`), when the loop knows it.
    pub material: Option<u64>,
    /// Object keys (`FrameMesh::object_key`) the draw drew: one for a direct draw, every member
    /// of a GPU-driven batch for an instanced indirect draw. For the runtime and editor
    /// extraction a mesh entity's object key is its `Entity::to_bits`.
    pub objects: Vec<u64>,
    /// Vertices of a non-indexed draw.
    pub vertex_count: Option<u32>,
    /// Indices of an indexed draw (`None` for a non-indexed or indirect draw).
    pub index_count: Option<u32>,
    /// Instances: the encoder's instance range, or for an indirect draw the CPU upper bound
    /// before GPU culling.
    pub instance_count: Option<u32>,
    /// The counts come from a GPU buffer (`draw_indexed_indirect`, multi-draw).
    pub indirect: bool,
    /// The color (or, for a depth-only pass, depth) target the pass draws into (`hdr_color`,
    /// `msaa_color`, `output`, `depth`, `shadow_atlas`).
    pub target: &'static str,
}

/// The draw loops that record [`DrawRecord`]s while a capture is armed, as
/// `(graph pass, counted pass)`. Draws of other loops are counted ([`FrameDrawCounts`]) but
/// not recorded one by one.
pub const DRAW_RECORD_PASSES: &[(&str, &str)] = &[
    ("depth_prepass", "depth_prepass"),
    ("forward", "opaque"),
    ("forward", "translucent"),
];

/// What a draw loop knows about the draw it is about to record ([`DrawTally::identify`]).
#[derive(Default)]
pub(crate) struct DrawIdentity {
    pub(crate) mesh: Option<u64>,
    pub(crate) material: Option<u64>,
    pub(crate) objects: Vec<u64>,
    /// Instances of an indirect draw (the CPU upper bound).
    pub(crate) instances: Option<u32>,
}

thread_local! {
    // The frame-debugger draw log (ADR-8224): on only between a capture frame's open and its end
    // on the thread that records it. Off, a draw costs one `bool` test and allocates nothing.
    static DRAW_LOG_ON: Cell<bool> = const { Cell::new(false) };
    static DRAW_LOG: RefCell<Vec<DrawRecord>> = const { RefCell::new(Vec::new()) };
    // This thread's draw-log work: records pushed and frames the log was armed for.
    static DRAW_LOG_RECORDS: Cell<u64> = const { Cell::new(0) };
    static DRAW_LOG_FRAMES: Cell<u64> = const { Cell::new(0) };
}

/// Counts of the frame-debugger draw log's work on one thread (ADR-8224). A frame no capture
/// armed adds nothing to either: the way a test proves the off path records nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawLogCounters {
    /// [`DrawRecord`]s pushed.
    pub records: u64,
    /// Frames the log was armed for.
    pub frames: u64,
}

impl DrawLogCounters {
    /// `self - earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            records: self.records.saturating_sub(earlier.records),
            frames: self.frames.saturating_sub(earlier.frames),
        }
    }
}

/// The draw log's counters so far on this thread (frames record on the thread that calls
/// `render_into`).
#[must_use]
pub fn draw_log_counters() -> DrawLogCounters {
    DrawLogCounters {
        records: DRAW_LOG_RECORDS.with(Cell::get),
        frames: DRAW_LOG_FRAMES.with(Cell::get),
    }
}

/// Opens a frame's draw log on this thread: `on` for the frame a capture armed, off for every
/// other frame (which also drops a log a failed capture frame left behind). Off allocates
/// nothing.
pub(crate) fn open_draw_log(on: bool) {
    DRAW_LOG_ON.with(|flag| flag.set(on));
    if on {
        DRAW_LOG_FRAMES.with(|count| count.set(count.get().saturating_add(1)));
    }
    DRAW_LOG.with(|log| {
        if let Ok(mut log) = log.try_borrow_mut()
            && !log.is_empty()
        {
            log.clear();
        }
    });
}

/// Closes this thread's draw log and returns what it recorded, or `None` (nothing to do) when it
/// was off.
pub(crate) fn close_draw_log() -> Option<Vec<DrawRecord>> {
    if !DRAW_LOG_ON.with(|flag| flag.replace(false)) {
        return None;
    }
    Some(DRAW_LOG.with(|log| {
        log.try_borrow_mut()
            .map(|mut log| std::mem::take(&mut *log))
            .unwrap_or_default()
    }))
}

fn push_draw_record(record: DrawRecord) {
    DRAW_LOG.with(|log| {
        if let Ok(mut log) = log.try_borrow_mut() {
            log.push(record);
            DRAW_LOG_RECORDS.with(|count| count.set(count.get().saturating_add(1)));
        }
    });
}

/// Counts one render pass's encoder calls as they are recorded: the draw loops call these
/// instead of the pass methods (one helper, no counter bumps at the call sites). A new pass
/// starts a new tally (pass state does not carry over); [`Self::finish`] returns its counts.
///
/// A loop that names its pass ([`Self::recording`]) also pushes one [`DrawRecord`] per draw
/// while a frame-debugger capture armed the draw log (ADR-8224).
#[derive(Default)]
pub(crate) struct DrawTally {
    counts: PassDrawCounts,
    pipeline: Option<wgpu::RenderPipeline>,
    groups: [Option<(wgpu::BindGroup, Option<u32>)>; COUNTED_BIND_GROUPS],
    /// The material group of the previous draw.
    batch: Option<wgpu::BindGroup>,
    /// The material texture table group (`texture_table`, ADR-863), bound at
    /// [`crate::texture_table::TEXTURE_TABLE_GROUP`] with the pass's first material group.
    table: Option<wgpu::BindGroup>,
    /// Whether `table` is bound in this pass.
    table_bound: bool,
    /// The draw log is armed and this loop named its pass: every draw is recorded.
    log: bool,
    /// `(graph pass, counted pass, target)` of a recording loop.
    names: (&'static str, &'static str, &'static str),
    /// The named pipeline bound now (recording loops only).
    pipeline_name: &'static str,
    /// The draw the loop is about to record (recording loops only).
    next: DrawIdentity,
}

impl DrawTally {
    /// A tally whose pass binds `table` (the texture table group, on the table path) the first
    /// time a material group is bound. `None` is [`Self::default`]: nothing extra is bound.
    pub(crate) fn with_texture_table(table: Option<&wgpu::BindGroup>) -> Self {
        Self {
            table: table.cloned(),
            ..Self::default()
        }
    }

    /// Names this loop's pass (a [`DRAW_RECORD_PASSES`] row) and `target`, so its draws are
    /// recorded while a capture armed the draw log. One thread-local read; nothing else when off.
    pub(crate) fn recording(
        mut self,
        pass: &'static str,
        counted: &'static str,
        target: &'static str,
    ) -> Self {
        debug_assert!(DRAW_RECORD_PASSES.contains(&(pass, counted)));
        self.log = DRAW_LOG_ON.with(Cell::get);
        self.names = (pass, counted, target);
        self.pipeline_name = "unnamed";
        self
    }

    /// Whether this loop records draws now (a capture frame; the loop named its pass).
    pub(crate) fn logging(&self) -> bool {
        self.log
    }

    /// Tells the tally what the next draws draw. `identity` runs only while recording, so a
    /// frame without a capture builds nothing.
    pub(crate) fn identify(&mut self, identity: impl FnOnce() -> DrawIdentity) {
        if self.logging() {
            self.next = identity();
        }
    }

    /// [`Self::set_pipeline`], naming the pipeline for the draw records.
    pub(crate) fn set_pipeline_named(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        pipeline: &wgpu::RenderPipeline,
        name: &'static str,
    ) {
        self.set_pipeline(pass, pipeline);
        self.pipeline_name = name;
    }

    fn record(
        &self,
        vertex_count: Option<u32>,
        index_count: Option<u32>,
        instances: Option<u32>,
        indirect: bool,
    ) {
        let (pass, counted, target) = self.names;
        push_draw_record(DrawRecord {
            pass,
            counted,
            pipeline: self.pipeline_name,
            mesh: self.next.mesh,
            material: self.next.material,
            objects: self.next.objects.clone(),
            vertex_count,
            index_count,
            instance_count: if indirect { self.next.instances } else { instances },
            indirect,
            target,
        });
    }

    /// `pass.set_pipeline`, counted when the pipeline changes.
    pub(crate) fn set_pipeline(&mut self, pass: &mut wgpu::RenderPass<'_>, pipeline: &wgpu::RenderPipeline) {
        if self.pipeline.as_ref() != Some(pipeline) {
            self.counts.pipeline_switches += 1;
            self.pipeline = Some(pipeline.clone());
        }
        self.pipeline_name = "unnamed";
        pass.counted_set_pipeline(pipeline);
    }

    /// `pass.set_bind_group`, counted when the group or its first dynamic offset changes.
    pub(crate) fn set_bind_group(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        index: u32,
        group: &wgpu::BindGroup,
        offsets: &[u32],
    ) {
        self.note_bind_group(index, group, offsets.first().copied());
        pass.counted_set_bind_group(index, group, offsets);
        if index as usize == MATERIAL_GROUP
            && !self.table_bound
            && let Some(table) = self.table.as_ref()
        {
            // Not a counted group: the table is bound once per pass, whatever the materials.
            pass.counted_set_bind_group(crate::texture_table::TEXTURE_TABLE_GROUP, table, &[]);
            self.table_bound = true;
        }
    }

    /// `pass.draw`, counted.
    pub(crate) fn draw(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        vertices: std::ops::Range<u32>,
        instances: std::ops::Range<u32>,
    ) {
        self.note_draw();
        if self.logging() {
            self.record(
                Some(vertices.len() as u32),
                None,
                Some(instances.len() as u32),
                false,
            );
        }
        pass.counted_draw(vertices, instances);
    }

    /// `pass.draw_indexed`, counted.
    pub(crate) fn draw_indexed(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        indices: std::ops::Range<u32>,
        base_vertex: i32,
        instances: std::ops::Range<u32>,
    ) {
        self.note_draw();
        if self.logging() {
            self.record(
                None,
                Some(indices.len() as u32),
                Some(instances.len() as u32),
                false,
            );
        }
        pass.counted_draw_indexed(indices, base_vertex, instances);
    }

    /// `pass.draw_indexed_indirect`, counted as one draw.
    pub(crate) fn draw_indexed_indirect(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        buffer: &wgpu::Buffer,
        offset: u64,
    ) {
        self.note_draw();
        if self.logging() {
            self.record(None, None, None, true);
        }
        pass.counted_draw_indexed_indirect(buffer, offset);
    }

    /// `pass.multi_draw_indexed_indirect`, counted as one draw (one encoder call; the
    /// two-phase prepass, `crate::prepass_occlusion`).
    pub(crate) fn multi_draw_indexed_indirect(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        buffer: &wgpu::Buffer,
        offset: u64,
        count: u32,
    ) {
        self.note_draw();
        if self.logging() {
            self.record(None, None, None, true);
        }
        pass.counted_multi_draw_indexed_indirect(buffer, offset, count);
    }

    /// Another module recorded into the pass (the visibility-buffer rasters, the shared terrain
    /// draw): what is bound is unknown, so the next bind of each slot counts.
    pub(crate) fn forget_state(&mut self) {
        self.pipeline = None;
        self.groups = Default::default();
        self.batch = None;
        self.table_bound = false;
    }

    /// This pass's counts.
    #[must_use]
    pub(crate) fn finish(self) -> PassDrawCounts {
        self.counts
    }

    fn note_bind_group(&mut self, index: u32, group: &wgpu::BindGroup, offset: Option<u32>) {
        let Some(bound) = self.groups.get_mut(index as usize) else {
            return;
        };
        if bound.as_ref().is_none_or(|(g, o)| g != group || *o != offset) {
            self.counts.bind_group_switches[index as usize] += 1;
            *bound = Some((group.clone(), offset));
        }
    }

    fn note_draw(&mut self) {
        self.counts.draws += 1;
        let Some((material, _)) = self.groups[MATERIAL_GROUP].as_ref() else {
            return;
        };
        if self.batch.as_ref() != Some(material) {
            self.counts.material_batches += 1;
            self.batch = Some(material.clone());
        }
    }
}

#[cfg(test)]
mod draw_tally_tests {
    use super::*;

    #[test]
    fn pass_counts_add_and_total() {
        let a = PassDrawCounts {
            draws: 3,
            pipeline_switches: 1,
            bind_group_switches: [1, 2, 3, 0],
            material_batches: 2,
        };
        let b = PassDrawCounts {
            draws: 4,
            pipeline_switches: 2,
            bind_group_switches: [0, 1, 1, 1],
            material_batches: 1,
        };
        let sum = a.plus(b);
        assert_eq!(sum.draws, 7);
        assert_eq!(sum.pipeline_switches, 3);
        assert_eq!(sum.bind_group_switches, [1, 3, 4, 1]);
        assert_eq!(sum.material_batches, 3);
        assert_eq!(sum.bind_group_switches_total(), 9);
        let frame = FrameDrawCounts {
            opaque: a,
            shadow: b,
            ..FrameDrawCounts::default()
        };
        assert_eq!(frame.total(), sum);
        let names: Vec<&str> = frame.passes().iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "shadow",
                "depth_prepass",
                "visbuffer_resolve",
                "opaque",
                "translucent"
            ]
        );
    }

    /// The tally on a real pass: redundant binds are not switches, a material run spans the
    /// draws that keep one group, and offsets distinguish a rebind.
    #[test]
    fn tally_counts_switches_and_material_runs() {
        let device = crate::device::test_device();
        let d = &device.device;
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tally test"),
            entries: &[],
        });
        let group = |label| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &layout,
                entries: &[],
            })
        };
        let (view, m1, m2) = (group("view"), group("m1"), group("m2"));
        let target = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("tally target"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let mut tally = DrawTally::default();
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tally"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            // Bind calls only (no pipeline, no draw is submitted): the encoder records them,
            // and nothing is submitted, so no validation of a draw runs.
            tally.set_bind_group(&mut pass, 0, &view, &[]);
            tally.set_bind_group(&mut pass, 0, &view, &[]);
            tally.set_bind_group(&mut pass, 2, &m1, &[]);
            tally.note_draw();
            tally.set_bind_group(&mut pass, 2, &m1, &[]);
            tally.note_draw();
            tally.set_bind_group(&mut pass, 2, &m2, &[]);
            tally.note_draw();
            tally.set_bind_group(&mut pass, 2, &m1, &[]);
            tally.note_draw();
            tally.note_draw();
        }
        drop(encoder);
        let counts = tally.finish();
        assert_eq!(counts.draws, 5);
        assert_eq!(
            counts.bind_group_switches,
            [1, 0, 3, 0],
            "redundant binds are not switches"
        );
        assert_eq!(counts.material_batches, 3, "m1 m1 | m2 | m1 m1");
        assert_eq!(counts.pipeline_switches, 0);
        let mut offsets = DrawTally::default();
        offsets.note_bind_group(2, &m1, Some(0));
        offsets.note_bind_group(2, &m1, Some(512));
        offsets.note_bind_group(2, &m1, Some(512));
        offsets.note_bind_group(7, &m1, None);
        assert_eq!(
            offsets.counts.bind_group_switches,
            [0, 0, 2, 0],
            "an offset change rebinds; slot 7 is not counted"
        );
    }
}

#[cfg(test)]
mod poll_site_tests {
    use super::*;

    /// ADR-4050: each call site gets one row with its own count, in order of the first wait;
    /// sites past the table's room are counted, not named.
    #[test]
    fn the_poll_site_table_counts_each_site_once_and_overflows_into_unnamed() {
        let mut table = PollSiteTable::EMPTY;
        table.note("a.rs", 10);
        table.note("b.rs", 10);
        table.note("a.rs", 10);
        table.note("a.rs", 11);
        let got = table.snapshot();
        assert_eq!(
            got.sites,
            vec![
                BlockingPollSite {
                    file: "a.rs",
                    line: 10,
                    count: 2
                },
                BlockingPollSite {
                    file: "b.rs",
                    line: 10,
                    count: 1
                },
                BlockingPollSite {
                    file: "a.rs",
                    line: 11,
                    count: 1
                },
            ]
        );
        assert_eq!(got.unnamed, 0);
        for line in 0..(BLOCKING_POLL_SITES as u32) {
            table.note("c.rs", line);
        }
        let full = table.snapshot();
        assert_eq!(full.sites.len(), BLOCKING_POLL_SITES);
        assert_eq!(full.unnamed, 3, "c.rs lines past the table's room");
        table.note("a.rs", 10);
        assert_eq!(
            table.snapshot().sites[0].count,
            3,
            "a known site still counts when full"
        );
    }

    #[test]
    fn since_keeps_only_the_sites_that_waited_in_between() {
        let mut table = PollSiteTable::EMPTY;
        table.note("a.rs", 1);
        table.note("b.rs", 2);
        let before = table.snapshot();
        table.note("b.rs", 2);
        table.note("b.rs", 2);
        table.note("c.rs", 3);
        let window = table.snapshot().since(&before);
        assert_eq!(
            window.sites,
            vec![
                BlockingPollSite {
                    file: "b.rs",
                    line: 2,
                    count: 2
                },
                BlockingPollSite {
                    file: "c.rs",
                    line: 3,
                    count: 1
                },
            ]
        );
        assert_eq!(window.unnamed, 0);
    }

    /// The process-wide table records the caller of the wait, not this module.
    #[test]
    fn a_wait_is_recorded_at_its_caller() {
        #[track_caller]
        fn wait() {
            note_blocking_poll(std::panic::Location::caller());
        }
        let before = blocking_poll_sites();
        let line = line!() + 1;
        wait();
        let window = blocking_poll_sites().since(&before);
        let named = window
            .sites
            .iter()
            .any(|s| s.file == file!() && s.line == line && s.count == 1);
        // Parallel tests share the table; once it is full a new site is counted as unnamed.
        assert!(named || window.unnamed >= 1, "{window:?}");
    }
}

#[cfg(test)]
mod draw_record_tests {
    use super::*;
    use crate::frame_capture::{CaptureSession, PassCaptureRequest};
    use crate::{DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex};

    /// A tally whose loop named its pass records nothing while the log is off, and never runs
    /// the identity closure; with the log on it records what the loop identified.
    #[test]
    fn frame_stats_draw_tally_records_only_while_the_log_is_on() {
        open_draw_log(false);
        let before = draw_log_counters();
        let mut off = DrawTally::default().recording("forward", "opaque", "hdr_color");
        assert!(!off.logging());
        off.identify(|| unreachable!("the identity is built only while recording"));
        off.note_draw();
        assert_eq!(close_draw_log(), None, "an off log returns nothing");
        assert_eq!(draw_log_counters().since(before), DrawLogCounters::default());

        open_draw_log(true);
        let mut on = DrawTally::default().recording("forward", "opaque", "hdr_color");
        // An unnamed loop records nothing even while the log is on.
        let unnamed = DrawTally::default();
        assert!(on.logging() && !unnamed.logging());
        on.pipeline_name = "standard";
        on.identify(|| DrawIdentity {
            mesh: Some(7),
            material: Some(9),
            objects: vec![11],
            instances: None,
        });
        on.record(None, Some(36), Some(1), false);
        let records = close_draw_log().expect("the log was on");
        assert_eq!(
            records,
            vec![DrawRecord {
                pass: "forward",
                counted: "opaque",
                pipeline: "standard",
                mesh: Some(7),
                material: Some(9),
                objects: vec![11],
                vertex_count: None,
                index_count: Some(36),
                instance_count: Some(1),
                indirect: false,
                target: "hdr_color",
            }]
        );
        assert_eq!(
            draw_log_counters().since(before),
            DrawLogCounters {
                records: 1,
                frames: 1
            }
        );
        // A frame that opens off drops whatever a failed capture frame left behind.
        open_draw_log(true);
        on.record(None, Some(3), Some(1), false);
        open_draw_log(false);
        assert_eq!(close_draw_log(), None);
        open_draw_log(true);
        assert_eq!(close_draw_log(), Some(Vec::new()));
    }

    /// Three meshes (one material each), no post, depth prepass on.
    fn three_meshes() -> LabScene {
        let normal = [0.0, 0.0, 1.0];
        let tri = |x: f32, material: usize| LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x - 0.4, -0.4, 0.0], normal, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([x + 0.4, -0.4, 0.0], normal, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x, 0.4, 0.0], normal, [0.5, 1.0]),
            ],
            indices: vec![0, 1, 2],
            material,
            ..LabMesh::unskinned()
        };
        let material = |r: f32| LabMaterial {
            base_color: [r, 0.5, 0.2, 1.0],
            ..Default::default()
        };
        LabScene {
            width: 64,
            height: 48,
            eye: [0.0, 0.0, 4.0],
            target: [0.0; 3],
            fov_y: 0.9,
            near: 0.1,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.5,
            ambient_lux: 10_000.0,
            double_sided: true,
            post: None,
            directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0; 3], 100_000.0),
            local_lights: vec![],
            materials: vec![material(0.2), material(0.5), material(0.8)],
            meshes: vec![tri(-1.0, 0), tri(0.0, 1), tri(1.0, 2)],
        }
    }

    /// The acceptance scene: a capture of three meshes lists three draws in the opaque pass, one
    /// per mesh, each naming its own object, mesh and material key, with its index count and the
    /// target it drew into; the depth prepass lists the same three objects.
    #[test]
    fn frame_stats_a_capture_of_three_meshes_records_three_opaque_draws_with_their_objects() {
        let device = crate::device::test_device();
        let lab = three_meshes();
        let scene = lab.as_frame_scene();
        let mut renderer = FrameRenderer::new();
        for _ in 0..2 {
            renderer.render_offscreen(&device, &scene).expect("warm");
        }
        let session =
            CaptureSession::arm_with_passes(&mut renderer, &PassCaptureRequest::all()).expect("arm");
        renderer
            .render_offscreen(&device, &scene)
            .expect("captured frame");
        let capture = session.finish(&mut renderer, &device).expect("finish");
        let mut expected: Vec<(u64, u64, u64)> = scene
            .meshes
            .iter()
            .map(|m| (m.object_key, m.mesh_key, m.material_key))
            .collect();
        expected.sort_unstable();
        let opaque: Vec<_> = capture.draws.iter().filter(|d| d.counted == "opaque").collect();
        assert_eq!(opaque.len(), 3, "{:#?}", capture.draws);
        assert_eq!(
            opaque.len() as u64,
            capture.counts.opaque.draws,
            "one record per counted opaque draw"
        );
        let mut got: Vec<(u64, u64, u64)> = opaque
            .iter()
            .map(|d| {
                assert_eq!(d.pass, "forward");
                assert_ne!(d.pipeline, "unnamed", "{d:?}");
                assert!(!d.target.is_empty());
                assert_eq!(d.objects.len(), 1, "{d:?}");
                if !d.indirect {
                    assert_eq!(d.index_count, Some(3), "{d:?}");
                    assert_eq!(d.instance_count, Some(1), "{d:?}");
                }
                (
                    d.objects[0],
                    d.mesh.expect("mesh key"),
                    d.material.expect("material key"),
                )
            })
            .collect();
        got.sort_unstable();
        assert_eq!(got, expected);
        let prepass: Vec<_> = capture
            .draws
            .iter()
            .filter(|d| d.counted == "depth_prepass")
            .collect();
        assert_eq!(prepass.len() as u64, capture.counts.depth_prepass.draws);
        // The draws of the capture's frame only: the next frame records nothing.
        let before = draw_log_counters();
        renderer.render_offscreen(&device, &scene).expect("next frame");
        assert_eq!(draw_log_counters().since(before), DrawLogCounters::default());
    }

    /// The off path (the lead's counter rule, ADR-8224): with no capture armed, or with one armed
    /// and cancelled before any frame, frames push no draw record, never open the log, and make
    /// the same counted device calls and allocations (buffers, textures, bind groups, maps) as a
    /// never-armed renderer at the same frame numbers.
    #[test]
    fn frame_stats_unarmed_and_disarmed_frames_record_and_allocate_nothing_more() {
        let device = crate::device::test_device();
        let lab = three_meshes();
        let scene = lab.as_frame_scene();
        let mut control = FrameRenderer::new();
        let mut renderer = FrameRenderer::new();
        for _ in 0..3 {
            control.render_offscreen(&device, &scene).expect("warm control");
            renderer.render_offscreen(&device, &scene).expect("warm renderer");
        }
        let log_before = draw_log_counters();
        for round in 0..2 {
            if round == 1 {
                renderer
                    .request_pass_capture(&PassCaptureRequest::all())
                    .expect("arm");
                assert!(renderer.cancel_pass_capture(), "disarmed before a frame");
            }
            for frame in 0..3 {
                let before = counters();
                control.render_offscreen(&device, &scene).expect("control");
                let expected = counters().since(before);
                let before = counters();
                renderer.render_offscreen(&device, &scene).expect("frame");
                let got = counters().since(before);
                assert_eq!(got, expected, "round {round} frame {frame}");
            }
        }
        assert_eq!(
            draw_log_counters().since(log_before),
            DrawLogCounters::default(),
            "an unarmed frame opened the draw log or pushed a record"
        );
        DRAW_LOG.with(|log| assert_eq!(log.borrow().capacity(), 0, "the log never allocated"));
    }
}
