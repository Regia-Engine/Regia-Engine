// SPDX-License-Identifier: MPL-2.0
//! Device-independent work of a frame (ADR-2711): what the renderer asked the GPU to do, counted
//! where the calls are recorded, identical on every adapter for a given scene, preset and size.
//!
//! A frame's GPU time needs a card to measure; most of what makes it cost is countable on any
//! device, lavapipe included: render and compute passes, dispatches and their workgroups, draw
//! calls and indirect draws, triangles, pipeline and bind-group binds, the bytes of the textures
//! and buffers the renderer allocates, the bytes it uploads and the heap allocations it makes.
//! [`FrameWorkProbe`] takes the difference over a frame (or several) and returns a
//! [`FrameWork`]; `crates/regia_render/tests/frame_work_ratchet.rs` compares those counts against
//! a committed table per shipped preset so a default-path cost regression fails on lavapipe instead
//! of at the next card sweep ([`ratchet`]).
//!
//! # Where the counts come from
//!
//! - Encoder calls go through the wrappers in [`counted`] (`counted_render_pass`,
//!   `counted_dispatch`, `counted_draw_indexed`, ...): the renderer's modules call those instead of
//!   the raw `wgpu` methods. Each is the `wgpu` call plus a plain load and store on the calling
//!   thread's own counter slot (no lock, no read-modify-write instruction); a reader sums the
//!   slots of every thread, so the frame's encode workers count too. Calls another crate records
//!   on its own encoder, and raw `wgpu` calls a module still makes, are not counted.
//! - Creations, uploads and submits are [`crate::FramePathCounters`] (the `TrackedDevice` and
//!   tracked queue counters).
//! - Heap allocations are counted only when a `regia_core::memory::TaggingAllocator` is the
//!   process's global allocator; [`FrameWork::heap_allocations`] is `None` otherwise.
//!
//! Triangles are counted for direct draws as triangle-list equivalents (`vertices / 3 ×
//! instances`); an indirect draw's count lives on the GPU and adds to
//! [`FrameWork::indirect_draws`] only.
//!
//! Nothing here allocates or locks on the counting path; the totals are read only when a reader
//! asks ([`work_counters`], [`FrameWorkProbe`], `render.frame_work`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

/// Counter slots, in [`WorkCounters`] field order.
#[derive(Clone, Copy)]
enum Slot {
    RenderPasses,
    ComputePasses,
    Dispatches,
    Workgroups,
    IndirectDispatches,
    Draws,
    IndirectDraws,
    Triangles,
    Instances,
    PipelineSets,
    BindGroupSets,
}

const SLOTS: usize = 11;

/// One thread's counters. Only the owning thread writes them (load + store, no RMW).
struct ThreadSlots([AtomicU64; SLOTS]);

impl ThreadSlots {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; SLOTS])
    }
}

/// Every thread's slots, and the totals of threads that have exited.
struct Registry {
    threads: Vec<Arc<ThreadSlots>>,
    retired: [u64; SLOTS],
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    threads: Vec::new(),
    retired: [0; SLOTS],
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    // A poisoned lock only means a thread panicked while holding it; the counts are still valid.
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    static MINE: Arc<ThreadSlots> = {
        let slots = Arc::new(ThreadSlots::new());
        registry().threads.push(Arc::clone(&slots));
        slots
    };
}

#[inline]
fn add(slot: Slot, by: u64) {
    // `try_with`: a call from a thread-local destructor at thread exit is not counted.
    let _ = MINE.try_with(|slots| {
        let counter = &slots.0[slot as usize];
        counter.store(
            counter.load(Ordering::Relaxed).wrapping_add(by),
            Ordering::Relaxed,
        );
    });
}

/// Encoder calls counted since start, every thread (monotonic; take differences).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCounters {
    /// `begin_render_pass` through [`counted::CountedEncoder`].
    pub render_passes: u64,
    /// `begin_compute_pass` through [`counted::CountedEncoder`].
    pub compute_passes: u64,
    /// Direct `dispatch_workgroups` calls.
    pub dispatches: u64,
    /// Workgroups of the direct dispatches (`x × y × z`, summed).
    pub workgroups: u64,
    /// `dispatch_workgroups_indirect` calls (their group counts live on the GPU).
    pub indirect_dispatches: u64,
    /// Direct `draw` / `draw_indexed` calls.
    pub draws: u64,
    /// `draw_indirect`, `draw_indexed_indirect` and `multi_draw_indexed_indirect` calls (one per
    /// call, whatever its count).
    pub indirect_draws: u64,
    /// Triangle-list equivalents of the direct draws (`vertices / 3 × instances`).
    pub triangles: u64,
    /// Instances of the direct draws.
    pub instances: u64,
    /// `set_pipeline` calls, render and compute.
    pub pipeline_sets: u64,
    /// `set_bind_group` calls, render and compute.
    pub bind_group_sets: u64,
}

impl WorkCounters {
    fn from_slots(s: [u64; SLOTS]) -> Self {
        Self {
            render_passes: s[Slot::RenderPasses as usize],
            compute_passes: s[Slot::ComputePasses as usize],
            dispatches: s[Slot::Dispatches as usize],
            workgroups: s[Slot::Workgroups as usize],
            indirect_dispatches: s[Slot::IndirectDispatches as usize],
            draws: s[Slot::Draws as usize],
            indirect_draws: s[Slot::IndirectDraws as usize],
            triangles: s[Slot::Triangles as usize],
            instances: s[Slot::Instances as usize],
            pipeline_sets: s[Slot::PipelineSets as usize],
            bind_group_sets: s[Slot::BindGroupSets as usize],
        }
    }

    /// `self − earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            render_passes: self.render_passes.saturating_sub(earlier.render_passes),
            compute_passes: self.compute_passes.saturating_sub(earlier.compute_passes),
            dispatches: self.dispatches.saturating_sub(earlier.dispatches),
            workgroups: self.workgroups.saturating_sub(earlier.workgroups),
            indirect_dispatches: self
                .indirect_dispatches
                .saturating_sub(earlier.indirect_dispatches),
            draws: self.draws.saturating_sub(earlier.draws),
            indirect_draws: self.indirect_draws.saturating_sub(earlier.indirect_draws),
            triangles: self.triangles.saturating_sub(earlier.triangles),
            instances: self.instances.saturating_sub(earlier.instances),
            pipeline_sets: self.pipeline_sets.saturating_sub(earlier.pipeline_sets),
            bind_group_sets: self.bind_group_sets.saturating_sub(earlier.bind_group_sets),
        }
    }
}

/// The counted encoder calls of every thread since start. One lock and a pass over the
/// registered threads; exited threads are folded into a running total here.
#[must_use]
pub fn work_counters() -> WorkCounters {
    let mut reg = registry();
    let Registry { threads, retired } = &mut *reg;
    // A thread that exited dropped its handle: its slots are final, fold them into `retired`.
    threads.retain(|slots| {
        if Arc::strong_count(slots) > 1 {
            return true;
        }
        for (total, counter) in retired.iter_mut().zip(&slots.0) {
            *total = total.wrapping_add(counter.load(Ordering::Relaxed));
        }
        false
    });
    let mut sum = *retired;
    for slots in threads.iter() {
        for (total, counter) in sum.iter_mut().zip(&slots.0) {
            *total = total.wrapping_add(counter.load(Ordering::Relaxed));
        }
    }
    WorkCounters::from_slots(sum)
}

/// The wrappers the renderer's modules record through (`use crate::frame_work::counted::*;`):
/// the raw `wgpu` call plus one counter bump.
pub mod counted {
    use std::ops::Range;

    use super::{Slot, add};

    /// Counted pass creation on a [`wgpu::CommandEncoder`].
    pub trait CountedEncoder {
        /// `begin_render_pass`, counted.
        fn counted_render_pass(&mut self, desc: &wgpu::RenderPassDescriptor<'_>) -> wgpu::RenderPass<'_>;
        /// `begin_compute_pass`, counted.
        fn counted_compute_pass(&mut self, desc: &wgpu::ComputePassDescriptor<'_>) -> wgpu::ComputePass<'_>;
    }

    impl CountedEncoder for wgpu::CommandEncoder {
        #[inline]
        fn counted_render_pass(&mut self, desc: &wgpu::RenderPassDescriptor<'_>) -> wgpu::RenderPass<'_> {
            crate::parallel_finish::pass_cuts::before_pass(self);
            add(Slot::RenderPasses, 1);
            self.begin_render_pass(desc)
        }

        #[inline]
        fn counted_compute_pass(&mut self, desc: &wgpu::ComputePassDescriptor<'_>) -> wgpu::ComputePass<'_> {
            crate::parallel_finish::pass_cuts::before_pass(self);
            add(Slot::ComputePasses, 1);
            self.begin_compute_pass(desc)
        }
    }

    /// Counted calls on a [`wgpu::ComputePass`].
    pub trait CountedCompute {
        /// `dispatch_workgroups`, counted with its workgroups.
        fn counted_dispatch(&mut self, x: u32, y: u32, z: u32);
        /// `dispatch_workgroups_indirect`, counted.
        fn counted_dispatch_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress);
        /// `set_pipeline`, counted.
        fn counted_set_pipeline(&mut self, pipeline: &wgpu::ComputePipeline);
        /// `set_bind_group`, counted.
        fn counted_set_bind_group<'a, BG>(
            &mut self,
            index: u32,
            bind_group: BG,
            offsets: &[wgpu::DynamicOffset],
        ) where
            Option<&'a wgpu::BindGroup>: From<BG>;
    }

    impl CountedCompute for wgpu::ComputePass<'_> {
        #[inline]
        fn counted_dispatch(&mut self, x: u32, y: u32, z: u32) {
            add(Slot::Dispatches, 1);
            add(
                Slot::Workgroups,
                u64::from(x)
                    .saturating_mul(u64::from(y))
                    .saturating_mul(u64::from(z)),
            );
            self.dispatch_workgroups(x, y, z);
        }

        #[inline]
        fn counted_dispatch_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress) {
            add(Slot::IndirectDispatches, 1);
            self.dispatch_workgroups_indirect(buffer, offset);
        }

        #[inline]
        fn counted_set_pipeline(&mut self, pipeline: &wgpu::ComputePipeline) {
            add(Slot::PipelineSets, 1);
            self.set_pipeline(pipeline);
        }

        #[inline]
        fn counted_set_bind_group<'a, BG>(
            &mut self,
            index: u32,
            bind_group: BG,
            offsets: &[wgpu::DynamicOffset],
        ) where
            Option<&'a wgpu::BindGroup>: From<BG>,
        {
            add(Slot::BindGroupSets, 1);
            self.set_bind_group(index, bind_group, offsets);
        }
    }

    /// Counted calls on a [`wgpu::RenderPass`].
    pub trait CountedRender {
        /// `draw`, counted with its triangles and instances.
        fn counted_draw(&mut self, vertices: Range<u32>, instances: Range<u32>);
        /// `draw_indexed`, counted with its triangles and instances.
        fn counted_draw_indexed(&mut self, indices: Range<u32>, base_vertex: i32, instances: Range<u32>);
        /// `draw_indirect`, counted.
        fn counted_draw_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress);
        /// `draw_indexed_indirect`, counted.
        fn counted_draw_indexed_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress);
        /// `multi_draw_indexed_indirect`, counted as one indirect draw.
        fn counted_multi_draw_indexed_indirect(
            &mut self,
            buffer: &wgpu::Buffer,
            offset: wgpu::BufferAddress,
            count: u32,
        );
        /// `set_pipeline`, counted.
        fn counted_set_pipeline(&mut self, pipeline: &wgpu::RenderPipeline);
        /// `set_bind_group`, counted.
        fn counted_set_bind_group<'a, BG>(
            &mut self,
            index: u32,
            bind_group: BG,
            offsets: &[wgpu::DynamicOffset],
        ) where
            Option<&'a wgpu::BindGroup>: From<BG>;
    }

    #[inline]
    fn note_direct(elements: &Range<u32>, instances: &Range<u32>) {
        let n = u64::from(instances.end.saturating_sub(instances.start));
        add(Slot::Draws, 1);
        add(Slot::Instances, n);
        add(
            Slot::Triangles,
            u64::from(elements.end.saturating_sub(elements.start) / 3).saturating_mul(n),
        );
    }

    impl CountedRender for wgpu::RenderPass<'_> {
        #[inline]
        fn counted_draw(&mut self, vertices: Range<u32>, instances: Range<u32>) {
            note_direct(&vertices, &instances);
            self.draw(vertices, instances);
        }

        #[inline]
        fn counted_draw_indexed(&mut self, indices: Range<u32>, base_vertex: i32, instances: Range<u32>) {
            note_direct(&indices, &instances);
            self.draw_indexed(indices, base_vertex, instances);
        }

        #[inline]
        fn counted_draw_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress) {
            add(Slot::IndirectDraws, 1);
            self.draw_indirect(buffer, offset);
        }

        #[inline]
        fn counted_draw_indexed_indirect(&mut self, buffer: &wgpu::Buffer, offset: wgpu::BufferAddress) {
            add(Slot::IndirectDraws, 1);
            self.draw_indexed_indirect(buffer, offset);
        }

        #[inline]
        fn counted_multi_draw_indexed_indirect(
            &mut self,
            buffer: &wgpu::Buffer,
            offset: wgpu::BufferAddress,
            count: u32,
        ) {
            add(Slot::IndirectDraws, 1);
            self.multi_draw_indexed_indirect(buffer, offset, count);
        }

        #[inline]
        fn counted_set_pipeline(&mut self, pipeline: &wgpu::RenderPipeline) {
            add(Slot::PipelineSets, 1);
            self.set_pipeline(pipeline);
        }

        #[inline]
        fn counted_set_bind_group<'a, BG>(
            &mut self,
            index: u32,
            bind_group: BG,
            offsets: &[wgpu::DynamicOffset],
        ) where
            Option<&'a wgpu::BindGroup>: From<BG>,
        {
            add(Slot::BindGroupSets, 1);
            self.set_bind_group(index, bind_group, offsets);
        }
    }
}

/// Format version of [`FrameWork`] as the agent protocol and the ratchet table write it.
pub const FRAME_WORK_FORMAT_VERSION: u32 = 1;

/// The device-independent work of one frame (or the sum over several frames, see
/// [`FrameWork::frames`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameWork {
    /// Frames the counts cover (1 for a single frame).
    pub frames: u64,
    /// Render passes.
    pub render_passes: u64,
    /// Compute passes.
    pub compute_passes: u64,
    /// Direct dispatches.
    pub dispatches: u64,
    /// Workgroups of the direct dispatches.
    pub workgroups: u64,
    /// Indirect dispatches.
    pub indirect_dispatches: u64,
    /// Direct draw calls.
    pub draws: u64,
    /// Indirect draw calls.
    pub indirect_draws: u64,
    /// Triangle-list equivalents of the direct draws.
    pub triangles: u64,
    /// Instances of the direct draws.
    pub instances: u64,
    /// `set_pipeline` calls (pipeline switches as recorded; redundant sets included).
    pub pipeline_sets: u64,
    /// `set_bind_group` calls.
    pub bind_group_sets: u64,
    /// `Queue::submit` calls (staging flushes included).
    pub submits: u64,
    /// Pipelines created.
    pub pipelines_created: u64,
    /// Bind groups created.
    pub bind_groups_created: u64,
    /// Buffers allocated through the tracked device.
    pub buffers_created: u64,
    /// Bytes of those buffers.
    pub buffer_bytes: u64,
    /// Textures allocated through the tracked device (render targets, LUTs, uploads).
    pub textures_created: u64,
    /// Bytes of those textures (all mips, layers and samples; at the frame's resolution for its
    /// targets).
    pub texture_bytes: u64,
    /// Bytes written through `write_buffer` / `write_texture`.
    pub upload_bytes: u64,
    /// Heap allocations (and reallocations), when a tagging allocator is installed.
    pub heap_allocations: Option<u64>,
}

impl FrameWork {
    /// Every count with its stable name, in field order (`frames` and the optional heap count
    /// excluded: the heap count is listed by [`Self::counts`] when present).
    #[must_use]
    pub fn counts(&self) -> Vec<(&'static str, u64)> {
        let mut out = vec![
            ("render_passes", self.render_passes),
            ("compute_passes", self.compute_passes),
            ("dispatches", self.dispatches),
            ("workgroups", self.workgroups),
            ("indirect_dispatches", self.indirect_dispatches),
            ("draws", self.draws),
            ("indirect_draws", self.indirect_draws),
            ("triangles", self.triangles),
            ("instances", self.instances),
            ("pipeline_sets", self.pipeline_sets),
            ("bind_group_sets", self.bind_group_sets),
            ("submits", self.submits),
            ("pipelines_created", self.pipelines_created),
            ("bind_groups_created", self.bind_groups_created),
            ("buffers_created", self.buffers_created),
            ("buffer_bytes", self.buffer_bytes),
            ("textures_created", self.textures_created),
            ("texture_bytes", self.texture_bytes),
            ("upload_bytes", self.upload_bytes),
        ];
        if let Some(heap) = self.heap_allocations {
            out.push(("heap_allocations", heap));
        }
        out
    }

    /// Tracked GPU bytes allocated: buffers plus textures.
    #[must_use]
    pub fn tracked_bytes(&self) -> u64 {
        self.buffer_bytes.saturating_add(self.texture_bytes)
    }
}

fn heap_allocations_now() -> Option<u64> {
    let snapshot = regia_core::memory::snapshot();
    snapshot.tracking.then(|| {
        snapshot
            .subsystems
            .iter()
            .fold(0u64, |sum, (_, s)| sum.saturating_add(s.total_allocations))
    })
}

/// A start point; [`Self::finish`] returns the work done since.
///
/// The counts are process-wide (every thread), so a reader measures a frame while no other
/// renderer records (the ratchet test runs its scenes one after the other).
#[derive(Clone, Copy, Debug)]
pub struct FrameWorkProbe {
    work: WorkCounters,
    path: crate::FramePathCounters,
    heap: Option<u64>,
}

impl FrameWorkProbe {
    /// Reads every counter now.
    #[must_use]
    pub fn start() -> Self {
        Self {
            work: work_counters(),
            path: crate::frame_stats::process_counters(),
            heap: heap_allocations_now(),
        }
    }

    /// The work since [`Self::start`], over `frames` frames.
    #[must_use]
    pub fn finish(&self, frames: u64) -> FrameWork {
        let w = work_counters().since(self.work);
        let p = crate::frame_stats::process_counters().since(self.path);
        let heap = match (self.heap, heap_allocations_now()) {
            (Some(before), Some(after)) => Some(after.saturating_sub(before)),
            _ => None,
        };
        FrameWork {
            frames,
            render_passes: w.render_passes,
            compute_passes: w.compute_passes,
            dispatches: w.dispatches,
            workgroups: w.workgroups,
            indirect_dispatches: w.indirect_dispatches,
            draws: w.draws,
            indirect_draws: w.indirect_draws,
            triangles: w.triangles,
            instances: w.instances,
            pipeline_sets: w.pipeline_sets,
            bind_group_sets: w.bind_group_sets,
            submits: p.submits,
            pipelines_created: p.pipelines,
            bind_groups_created: p.bind_groups,
            buffers_created: p.buffers,
            buffer_bytes: p.buffer_bytes,
            textures_created: p.textures,
            texture_bytes: p.texture_bytes,
            upload_bytes: p.upload_bytes,
            heap_allocations: heap,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The runtime's per-frame report (`render.frame_work`)
// ---------------------------------------------------------------------------------------------

/// Set by the first [`request_window_reports`]: from then on each window frame publishes its work.
static WANTED: AtomicBool = AtomicBool::new(false);

struct Published {
    probe: Option<FrameWorkProbe>,
    last: Option<FrameWork>,
    seq: u64,
}

static PUBLISHED: Mutex<Published> = Mutex::new(Published {
    probe: None,
    last: None,
    seq: 0,
});

/// Turns the per-window-frame report on (it stays on). Until a reader asks, a window frame does
/// not read any counter.
pub fn request_window_reports() {
    WANTED.store(true, Ordering::Relaxed);
}

/// Whether [`request_window_reports`] has been called.
#[must_use]
pub fn window_reports_on() -> bool {
    WANTED.load(Ordering::Relaxed)
}

/// Called once per window frame (with the window phases): publishes the work since the previous
/// window frame. One relaxed load when no reader asked.
pub(crate) fn note_window_frame() {
    if !WANTED.load(Ordering::Relaxed) {
        return;
    }
    let mut published = PUBLISHED.lock().unwrap_or_else(PoisonError::into_inner);
    let next = FrameWorkProbe::start();
    if let Some(probe) = published.probe {
        published.last = Some(probe.finish(1));
        published.seq += 1;
    }
    published.probe = Some(next);
}

/// The newest window frame's work and its publication count (1-based), or `None` before the
/// first full frame after [`request_window_reports`].
#[must_use]
pub fn last_window_frame_work() -> Option<(FrameWork, u64)> {
    let published = PUBLISHED.lock().unwrap_or_else(PoisonError::into_inner);
    published.last.map(|work| (work, published.seq))
}

// ---------------------------------------------------------------------------------------------
// The ratchet table and VRAM rows
// ---------------------------------------------------------------------------------------------

/// The committed per-preset work table and its comparison (`tests/data/frame_work.ron`).
pub mod ratchet {
    use serde::{Deserialize, Serialize};

    use super::FrameWork;
    use crate::memory_budget::{BudgetTier, MemoryBudget};

    /// Format version of [`RatchetTable`] (`frame_work.ron`).
    pub const TABLE_FORMAT_VERSION: u32 = 1;

    /// How far a count may rise over its row before the ratchet fails, percent.
    pub const OVER_PERCENT: u64 = 10;

    /// How far a count may fall under its row before the ratchet prints a tighten note, percent.
    pub const UNDER_PERCENT: u64 = 10;

    /// The environment variable that re-records the table.
    pub const RECORD_ENV: &str = "REGIA_FRAME_WORK_RECORD";

    /// The command that re-records the table after a justified change.
    pub const RECORD_COMMAND: &str =
        "REGIA_FRAME_WORK_RECORD=1 cargo test -p regia_render --test frame_work_ratchet -- --nocapture";

    /// One scene at one preset and size.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RatchetRow {
        /// Scene name (`forward_box`, `instanced_field`, `terrain_lab`, `gi_room`).
        pub scene: String,
        /// Shipped preset name (`desktop_high`, ...).
        pub preset: String,
        /// Target width.
        pub width: u32,
        /// Target height.
        pub height: u32,
        /// Allocations of the warm-up frames (textures, buffers, pipelines and bind groups the
        /// renderer created to draw the scene at this size).
        pub warm: FrameWork,
        /// One steady frame after warm-up.
        pub steady: FrameWork,
    }

    impl RatchetRow {
        /// `scene/preset/WxH`, the name a failure prints.
        #[must_use]
        pub fn key(&self) -> String {
            format!("{}/{}/{}x{}", self.scene, self.preset, self.width, self.height)
        }
    }

    /// The table.
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RatchetTable {
        /// [`TABLE_FORMAT_VERSION`].
        pub format_version: u32,
        /// The adapter the rows were recorded on (the ratchet compares on that adapter class only).
        pub adapter: String,
        /// Every row.
        pub rows: Vec<RatchetRow>,
    }

    impl RatchetTable {
        /// Parses `frame_work.ron`.
        ///
        /// # Errors
        /// Malformed RON, or a format version this build does not read.
        pub fn parse(text: &str) -> Result<Self, String> {
            let table: Self = ron::from_str(text).map_err(|err| format!("frame_work.ron: {err}"))?;
            if table.format_version != TABLE_FORMAT_VERSION {
                return Err(format!(
                    "frame_work.ron: format_version {} (this build reads {TABLE_FORMAT_VERSION})",
                    table.format_version
                ));
            }
            Ok(table)
        }

        /// The table as `frame_work.ron` text.
        ///
        /// # Errors
        /// Serialization failed (it does not for this type).
        pub fn to_ron(&self) -> Result<String, String> {
            let config = regia_core::ron_pretty().depth_limit(3).struct_names(false);
            ron::ser::to_string_pretty(self, config).map_err(|err| err.to_string())
        }

        /// The row with `key` ([`RatchetRow::key`]).
        #[must_use]
        pub fn row(&self, key: &str) -> Option<&RatchetRow> {
            self.rows.iter().find(|row| row.key() == key)
        }
    }

    /// One count that moved past its slack.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Moved {
        /// [`RatchetRow::key`] plus `warm` or `steady`.
        pub row: String,
        /// The count's name ([`FrameWork::counts`]).
        pub count: &'static str,
        /// The table's value.
        pub recorded: u64,
        /// This run's value.
        pub measured: u64,
    }

    impl std::fmt::Display for Moved {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{} {}: {} (table {})",
                self.row, self.count, self.measured, self.recorded
            )
        }
    }

    /// What a comparison found.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Comparison {
        /// Counts more than [`OVER_PERCENT`] above their row: the ratchet fails.
        pub over: Vec<Moved>,
        /// Counts more than [`UNDER_PERCENT`] under their row: a note (re-record to tighten).
        pub under: Vec<Moved>,
    }

    impl Comparison {
        /// Whether nothing grew past its slack.
        #[must_use]
        pub fn passes(&self) -> bool {
            self.over.is_empty()
        }

        /// The failure message: every grown count, then the re-record command.
        #[must_use]
        pub fn failure_message(&self) -> String {
            let lines: Vec<String> = self.over.iter().map(ToString::to_string).collect();
            format!(
                "frame work grew more than {OVER_PERCENT} % over crates/regia_render/tests/data/frame_work.ron:\n  {}\n\
                 If the growth is intended (a justified default-path change), re-record with\n  {RECORD_COMMAND}\n\
                 and give the reason in the commit message.",
                lines.join("\n  ")
            )
        }
    }

    /// `measured > recorded` by more than `percent`: `measured × 100 > recorded × (100 + percent)`.
    /// A count recorded as 0 fails at the first unit (a new pass where there was none).
    #[must_use]
    pub fn over(recorded: u64, measured: u64, percent: u64) -> bool {
        u128::from(measured) * 100 > u128::from(recorded) * u128::from(100 + percent)
    }

    /// `measured < recorded` by more than `percent`.
    #[must_use]
    pub fn under(recorded: u64, measured: u64, percent: u64) -> bool {
        u128::from(measured) * 100 < u128::from(recorded) * u128::from(100 - percent.min(100))
    }

    fn compare_work(row: &str, recorded: &FrameWork, measured: &FrameWork, out: &mut Comparison) {
        let table = recorded.counts();
        for (name, value) in measured.counts() {
            // A count the table does not have (heap allocations recorded without the counting
            // allocator) is not compared.
            let Some((_, was)) = table.iter().find(|(n, _)| *n == name) else {
                continue;
            };
            let moved = |out: &mut Vec<Moved>| {
                out.push(Moved {
                    row: row.to_owned(),
                    count: name,
                    recorded: *was,
                    measured: value,
                });
            };
            if over(*was, value, OVER_PERCENT) {
                moved(&mut out.over);
            } else if under(*was, value, UNDER_PERCENT) {
                moved(&mut out.under);
            }
        }
    }

    /// Compares `measured` against `table`. A measured row the table lacks is an `over` entry
    /// for every count above 0 (a new row must be recorded on purpose).
    #[must_use]
    pub fn compare(table: &RatchetTable, measured: &[RatchetRow]) -> Comparison {
        let mut out = Comparison::default();
        for row in measured {
            let key = row.key();
            match table.row(&key) {
                Some(recorded) => {
                    compare_work(&format!("{key} warm"), &recorded.warm, &row.warm, &mut out);
                    compare_work(&format!("{key} steady"), &recorded.steady, &row.steady, &mut out);
                }
                None => {
                    compare_work(
                        &format!("{key} warm (no row)"),
                        &FrameWork::default(),
                        &row.warm,
                        &mut out,
                    );
                    compare_work(
                        &format!("{key} steady (no row)"),
                        &FrameWork::default(),
                        &row.steady,
                        &mut out,
                    );
                }
            }
        }
        out
    }

    /// The resolution the VRAM rows of `tier` are counted at: the tier's target output
    /// (master plan § 5.1a; Minimum is the 1080p floor, Deck its 1280 × 800 panel, Desktop High
    /// the 1440p mid tier, Integrated Low the 720p integrated-GPU floor, ADR-8878).
    #[must_use]
    pub fn tier_resolution(tier: BudgetTier) -> (u32, u32) {
        match tier {
            BudgetTier::Minimum => (1920, 1080),
            BudgetTier::Deck => (1280, 800),
            BudgetTier::DesktopHigh => (2560, 1440),
            BudgetTier::IntegratedLow => (1280, 720),
        }
    }

    /// The shipped preset a tier's VRAM row is counted with. Integrated Low's tier is Very Low,
    /// which draws Desktop Low's targets at a third of the output on each axis; Desktop Low at the
    /// full 720p is an upper bound on its counted bytes (ADR-8878).
    #[must_use]
    pub fn tier_preset(tier: BudgetTier) -> crate::RenderProfilePreset {
        match tier {
            BudgetTier::Minimum => crate::RenderProfilePreset::DesktopHigh,
            BudgetTier::Deck => crate::RenderProfilePreset::Deck,
            BudgetTier::DesktopHigh => crate::RenderProfilePreset::DesktopUltra,
            BudgetTier::IntegratedLow => crate::RenderProfilePreset::DesktopLow,
        }
    }

    /// A tier's counted VRAM row.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct VramRow {
        /// The tier.
        pub tier: BudgetTier,
        /// Its resolution ([`tier_resolution`]).
        pub resolution: (u32, u32),
        /// Tracked texture and buffer bytes the renderer allocated to draw the scene.
        pub tracked_bytes: u64,
        /// The tier's VRAM budget ([`MemoryBudget::for_tier`]).
        pub budget_bytes: u64,
    }

    impl VramRow {
        /// A row for `tier` with `tracked_bytes` counted.
        #[must_use]
        pub fn new(tier: BudgetTier, tracked_bytes: u64) -> Self {
            Self {
                tier,
                resolution: tier_resolution(tier),
                tracked_bytes,
                budget_bytes: MemoryBudget::for_tier(tier).vram,
            }
        }

        /// Whether the counted bytes fit the budget.
        #[must_use]
        pub fn fits(&self) -> bool {
            self.tracked_bytes <= self.budget_bytes
        }

        /// Counted bytes as a share of the budget, percent.
        #[must_use]
        pub fn percent_of_budget(&self) -> f64 {
            if self.budget_bytes == 0 {
                return 0.0;
            }
            self.tracked_bytes as f64 * 100.0 / self.budget_bytes as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ratchet::*;
    use super::*;

    fn work(render_passes: u64, draws: u64) -> FrameWork {
        FrameWork {
            frames: 1,
            render_passes,
            draws,
            ..FrameWork::default()
        }
    }

    fn row(steady: FrameWork) -> RatchetRow {
        RatchetRow {
            scene: "forward_box".into(),
            preset: "desktop_high".into(),
            width: 64,
            height: 36,
            warm: FrameWork::default(),
            steady,
        }
    }

    #[test]
    fn the_slack_is_ten_percent_each_way() {
        assert!(!over(100, 110, OVER_PERCENT));
        assert!(over(100, 111, OVER_PERCENT));
        assert!(over(0, 1, OVER_PERCENT), "a count from nothing fails");
        assert!(!over(0, 0, OVER_PERCENT));
        assert!(!under(100, 90, UNDER_PERCENT));
        assert!(under(100, 89, UNDER_PERCENT));
        assert!(!over(u64::MAX, u64::MAX, OVER_PERCENT), "no overflow");
    }

    /// An extra pass in the measured frame fails the comparison, naming the row and the count,
    /// and the message carries the re-record command.
    #[test]
    fn an_extra_pass_fails_and_names_its_row() {
        let table = RatchetTable {
            format_version: TABLE_FORMAT_VERSION,
            adapter: "test".into(),
            rows: vec![row(work(5, 40))],
        };
        assert!(compare(&table, &[row(work(5, 40))]).passes());
        let grown = compare(&table, &[row(work(6, 40))]);
        assert!(!grown.passes());
        assert_eq!(grown.over.len(), 1);
        assert_eq!(grown.over[0].row, "forward_box/desktop_high/64x36 steady");
        assert_eq!(grown.over[0].count, "render_passes");
        let message = grown.failure_message();
        assert!(message.contains("forward_box/desktop_high/64x36 steady render_passes: 6 (table 5)"));
        assert!(message.contains(RECORD_COMMAND));
        let shrunk = compare(&table, &[row(work(5, 30))]);
        assert!(shrunk.passes(), "fewer draws only note");
        assert_eq!(shrunk.under.len(), 1);
    }

    #[test]
    fn a_row_the_table_lacks_fails() {
        let table = RatchetTable {
            format_version: TABLE_FORMAT_VERSION,
            ..RatchetTable::default()
        };
        assert!(!compare(&table, &[row(work(1, 1))]).passes());
    }

    #[test]
    fn the_table_round_trips_and_checks_its_version() {
        let table = RatchetTable {
            format_version: TABLE_FORMAT_VERSION,
            adapter: "llvmpipe".into(),
            rows: vec![row(FrameWork {
                heap_allocations: Some(12),
                ..work(3, 4)
            })],
        };
        let text = table.to_ron().expect("ron");
        assert_eq!(RatchetTable::parse(&text).expect("parse"), table);
        let future = text.replacen("format_version: 1", "format_version: 99", 1);
        assert!(RatchetTable::parse(&future).is_err());
    }

    #[test]
    fn heap_counts_compare_only_when_recorded() {
        let mut recorded = row(work(1, 1));
        recorded.steady.heap_allocations = None;
        let table = RatchetTable {
            format_version: TABLE_FORMAT_VERSION,
            adapter: String::new(),
            rows: vec![recorded],
        };
        let mut measured = row(work(1, 1));
        measured.steady.heap_allocations = Some(1000);
        assert!(compare(&table, &[measured]).passes());
    }

    /// The wrappers count on the calling thread and a reader on another thread sees them.
    #[test]
    fn counted_calls_are_seen_from_every_thread() {
        let before = work_counters();
        std::thread::spawn(|| {
            super::add(Slot::Dispatches, 2);
            super::add(Slot::Workgroups, 64);
        })
        .join()
        .expect("thread");
        let delta = work_counters().since(before);
        assert!(
            delta.dispatches >= 2,
            "an exited thread's counts are kept: {delta:?}"
        );
        assert!(delta.workgroups >= 64);
        // Reading twice after the thread exited does not count its slots twice.
        let again = work_counters().since(before);
        assert_eq!(again.dispatches, delta.dispatches);
    }

    /// The wrappers on a real encoder: a render pass and a bind-group set are counted, and a
    /// probe on this thread sees them (nothing is submitted, so no draw needs a pipeline).
    #[test]
    fn the_wrappers_count_a_real_pass() {
        use super::counted::{CountedEncoder, CountedRender};
        let device = crate::device::test_device();
        let d = &device.device;
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame work test"),
            entries: &[],
        });
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame work test"),
            layout: &layout,
            entries: &[],
        });
        let target = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame work test target"),
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
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let before = work_counters();
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame work test"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            pass.counted_set_bind_group(0, &group, &[]);
        }
        drop(encoder);
        let delta = work_counters().since(before);
        assert!(delta.render_passes >= 1, "{delta:?}");
        assert!(delta.bind_group_sets >= 1, "{delta:?}");
    }

    #[test]
    fn vram_rows_use_the_tier_budget() {
        for tier in crate::memory_budget::BudgetTier::ALL {
            let row = VramRow::new(tier, 1);
            assert!(row.fits());
            assert_eq!(
                row.budget_bytes,
                crate::memory_budget::MemoryBudget::for_tier(tier).vram
            );
            assert!(row.resolution.0 >= 1280);
        }
        assert!(!VramRow::new(crate::memory_budget::BudgetTier::Deck, u64::MAX).fits());
    }
}
