// SPDX-License-Identifier: MPL-2.0
//! GPU residency feedback for the material texture table (bindless step 5, ADR-1110).
//!
//! Streaming (`texture_stream`) estimates each material's texel need on the CPU from the mesh's
//! UV span, its distance and the view's pixel density. On the texture table path the GPU can
//! also report what it really sampled: the shaded pixel's own UV gradients (after culling,
//! occlusion, parallax, detail tiling and the upscaler's mip bias), per texture.
//!
//! - **Switch.** [`FEEDBACK_ENV`] (`off`, `observe` or `drive`; read once per process, because the
//!   shaders and the table layout change) picks the [`FeedbackMode`]. `off` (the default) changes
//!   nothing: no buffer, no binding, no define. A device gets feedback only on the table path
//!   with storage buffers to spare ([`feedback_mode_for`]).
//! - **Shader** (`RESIDENCY_FEEDBACK` in `lab_forward.wgsl`): the table group gains binding
//!   [`FEEDBACK_BINDING`], one `atomic<u32>` per table index. In each 4 × 4 pixel cell one pixel a
//!   frame (the cell position is the phase the renderer writes into word 0, [`phase_for`]) takes
//!   `atomicMax` of [`encode_need`] of its texel need into the indices of its material's four
//!   streamed slots. Need is `1 / max(|∂uv/∂x|, |∂uv/∂y|)`: texels along the texture's edge for one
//!   texel per pixel (Williams 1983's LOD before the log), which does not depend on what is
//!   resident. Pixels never change: the pass only adds stores.
//! - **Readback.** Each main-view frame, before streaming, the renderer copies the last frame's
//!   words into one of [`FEEDBACK_RING`] mappable buffers, clears the words from index
//!   [`crate::texture_table::RESERVED_INDICES`] on, writes the next phase, and maps the copy
//!   (`FeedbackRing`). It never waits: a copy that is not mapped yet is read on a later frame,
//!   a full ring skips the copy (counted), and only the newest mapped copy is used.
//! - **Stale indices.** An index freed and handed to another texture after the frame that wrote
//!   it is dropped (the table records the refresh each index was assigned in).
//! - **Use.** `observe` reports GPU need beside CPU need per streamed material
//!   (`render.texture_residency`). `drive` also feeds the GPU need to the stream as one more
//!   draw's need (the larger wins), so the stream grows where the GPU sampled finer than the CPU
//!   estimate, and the material counts as seen.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// The shader define that adds the feedback stores (only composed with
/// [`crate::texture_table::TEXTURE_TABLE_DEF`]).
pub const RESIDENCY_FEEDBACK_DEF: &str = "RESIDENCY_FEEDBACK";

/// Environment variable read once per process: `off` (default), `observe` or `drive`.
pub const FEEDBACK_ENV: &str = "REGIA_RESIDENCY_FEEDBACK";

/// Binding of the feedback words in the table group ([`crate::texture_table::TEXTURE_TABLE_GROUP`]).
pub const FEEDBACK_BINDING: u32 = 2;

/// Mappable copies in flight; with one copy a frame, a readout is usually two or three frames old.
pub const FEEDBACK_RING: usize = 3;

/// Steps per doubling of need in [`encode_need`].
pub const NEED_STEPS_PER_OCTAVE: f32 = 256.0;

/// Largest need [`encode_need`] represents, as a power of two (65 536 texels).
pub const NEED_MAX_LOG2: f32 = 16.0;

/// Pixels per cell that take turns writing (4 × 4).
pub const PHASE_CELLS: u32 = 16;

/// Storage buffers per stage a device needs for feedback: the material path's plus the
/// feedback words.
pub const FEEDBACK_MIN_STORAGE_BUFFERS: u32 = crate::bindless::MATERIAL_SSBO_MIN_STORAGE_BUFFERS + 1;

/// Most materials kept in [`ResidencyFeedbackStats`]'s report (largest mismatch first).
pub const REPORT_MATERIALS: usize = 64;

/// What the renderer does with residency feedback ([`FEEDBACK_ENV`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FeedbackMode {
    /// No feedback: no buffer, binding or define (the default).
    #[default]
    Off,
    /// Read the GPU's need and report it beside the CPU estimate; streaming ignores it.
    Observe,
    /// Also feed the GPU's need to streaming (the larger of the two wins).
    Drive,
}

impl FeedbackMode {
    /// Stable lowercase name: `off`, `observe`, `drive`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Drive => "drive",
        }
    }

    /// The mode named by `value` (the [`FEEDBACK_ENV`] text, any case, trimmed); `Off` when unset,
    /// empty or unknown.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("observe") => Self::Observe,
            Some("drive") => Self::Drive,
            _ => Self::Off,
        }
    }
}

impl std::fmt::Display for FeedbackMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`FeedbackMode::parse`] of the process environment, read once: every pipeline of the process
/// agrees on the table layout.
fn configured_mode() -> FeedbackMode {
    static MODE: std::sync::OnceLock<FeedbackMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let value = regia_core::brand::env_var(FEEDBACK_ENV).ok();
        let mode = FeedbackMode::parse(value.as_deref());
        if let Some(text) = value.as_deref()
            && !text.trim().is_empty()
            && mode == FeedbackMode::Off
            && !text.trim().eq_ignore_ascii_case("off")
        {
            tracing::warn!(
                target: "regia_render",
                "{FEEDBACK_ENV}={text:?} is not off, observe or drive; feedback stays off"
            );
        }
        mode
    })
}

/// The mode a device runs: `asked` when the device draws through the table (`table`) and has
/// [`FEEDBACK_MIN_STORAGE_BUFFERS`] per stage, else `Off`. Pure.
#[must_use]
pub fn feedback_mode_for(asked: FeedbackMode, table: bool, limits: &wgpu::Limits) -> FeedbackMode {
    if table && limits.max_storage_buffers_per_shader_stage >= FEEDBACK_MIN_STORAGE_BUFFERS {
        asked
    } else {
        FeedbackMode::Off
    }
}

/// [`feedback_mode_for`] of `device` with the process's [`FEEDBACK_ENV`].
#[must_use]
pub fn device_feedback_mode(device: &wgpu::Device) -> FeedbackMode {
    let table = crate::texture_table::device_support(device).capacity().is_some();
    #[cfg(test)]
    if let Some(mode) = test_override::get(device) {
        return feedback_mode_for(mode, table, &device.limits());
    }
    feedback_mode_for(configured_mode(), table, &device.limits())
}

/// Per-device modes for tests: the process setting is read once, so a test that needs feedback
/// on its own device names the mode here before it builds anything on that device. Every
/// other device keeps the process setting.
#[cfg(test)]
pub(crate) mod test_override {
    use super::FeedbackMode;

    static MODES: std::sync::Mutex<Vec<(wgpu::Device, FeedbackMode)>> = std::sync::Mutex::new(Vec::new());

    /// Runs `device` in `mode` (call before the first pipeline or table on it).
    pub(crate) fn set(device: &wgpu::Device, mode: FeedbackMode) {
        if let Ok(mut modes) = MODES.lock() {
            modes.retain(|(known, _)| known != device);
            modes.push((device.clone(), mode));
        }
    }

    /// The mode set for `device`, if any.
    pub(crate) fn get(device: &wgpu::Device) -> Option<FeedbackMode> {
        let modes = MODES.lock().ok()?;
        modes
            .iter()
            .find(|(known, _)| known == device)
            .map(|(_, mode)| *mode)
    }
}

/// The word the shader stores for a need of `px` texels along an edge:
/// `1 + 256 · log2(px)` with `log2(px)` clamped to `0..`[`NEED_MAX_LOG2`] (`0` = not sampled).
/// The shader computes the same in `f32` and truncates.
#[must_use]
pub fn encode_need(px: f32) -> u32 {
    let octaves = if px > 1.0 { px.log2() } else { 0.0 };
    let max = NEED_MAX_LOG2 - 1.0 / NEED_STEPS_PER_OCTAVE;
    1 + (octaves.clamp(0.0, max) * NEED_STEPS_PER_OCTAVE) as u32
}

/// The need a word stands for (the low edge of its step), or `None` for an untouched word.
#[must_use]
pub fn decode_need(code: u32) -> Option<f32> {
    let step = code.checked_sub(1)?;
    Some(((step as f32) / NEED_STEPS_PER_OCTAVE).exp2())
}

/// The phase written for the frame tagged `tag`: which pixel of each 4 × 4 cell writes.
#[must_use]
pub fn phase_for(tag: u64) -> u32 {
    (tag % u64::from(PHASE_CELLS)) as u32
}

/// Whether pixel `(x, y)` writes under `phase` (the shader's test).
#[must_use]
pub fn pixel_writes(x: u32, y: u32, phase: u32) -> bool {
    ((x & 3) | ((y & 3) << 2)) == phase % PHASE_CELLS
}

/// One mappable copy's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotState {
    /// Unused: the next copy may go here.
    Free,
    /// Copied (frame `tag`'s words) and asked to map; not readable yet.
    Mapping {
        /// The table refresh whose frame wrote the words.
        tag: u64,
    },
    /// Mapped: readable.
    Ready {
        /// The table refresh whose frame wrote the words.
        tag: u64,
    },
}

/// The readback ring's bookkeeping (no GPU state, so it is tested on its own).
#[derive(Debug)]
pub(crate) struct FeedbackRing {
    slots: [SlotState; FEEDBACK_RING],
    next: usize,
}

impl Default for FeedbackRing {
    fn default() -> Self {
        Self {
            slots: [SlotState::Free; FEEDBACK_RING],
            next: 0,
        }
    }
}

impl FeedbackRing {
    /// A slot for frame `tag`'s copy (now `Mapping`), in turn, or `None` when that slot is still
    /// in use (the copy is skipped; nothing changes).
    pub(crate) fn claim(&mut self, tag: u64) -> Option<usize> {
        let slot = self.next;
        if self.slots[slot] != SlotState::Free {
            return None;
        }
        self.slots[slot] = SlotState::Mapping { tag };
        self.next = (slot + 1) % FEEDBACK_RING;
        Some(slot)
    }

    /// The map of `slot` completed: `Ready` when it succeeded, `Free` when it failed. Other
    /// states change nothing.
    pub(crate) fn mapped(&mut self, slot: usize, ok: bool) {
        if let Some(SlotState::Mapping { tag }) = self.slots.get(slot).copied() {
            self.slots[slot] = if ok {
                SlotState::Ready { tag }
            } else {
                SlotState::Free
            };
        }
    }

    /// Every `Ready` slot as `(slot, tag)`, newest first. The caller reads the first and frees
    /// them all ([`Self::free`]).
    pub(crate) fn ready(&self) -> Vec<(usize, u64)> {
        let mut ready: Vec<(usize, u64)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, state)| match state {
                SlotState::Ready { tag } => Some((slot, *tag)),
                _ => None,
            })
            .collect();
        ready.sort_by_key(|&(_, tag)| std::cmp::Reverse(tag));
        ready
    }

    /// `slot` may take a copy again.
    pub(crate) fn free(&mut self, slot: usize) {
        if let Some(state) = self.slots.get_mut(slot) {
            *state = SlotState::Free;
        }
    }

    /// Slots that wait for their map.
    #[must_use]
    pub(crate) fn mapping(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(slot, state)| matches!(state, SlotState::Mapping { .. }).then_some(slot))
            .collect()
    }
}

/// One frame's feedback, read back: the word of every table index (`0` = not sampled, or
/// dropped as stale).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackReadout {
    /// The table refresh whose frame wrote the words.
    pub(crate) tag: u64,
    /// One word per table index.
    pub(crate) codes: Vec<u32>,
}

impl FeedbackReadout {
    /// The need recorded for table `index` ([`decode_need`]); `None` for the neutral indices, an
    /// index nothing sampled, or one past the table.
    #[must_use]
    pub(crate) fn need_px(&self, index: u32) -> Option<f32> {
        if index < crate::texture_table::RESERVED_INDICES {
            return None;
        }
        decode_need(*self.codes.get(index as usize)?)
    }

    /// Zeroes every word whose index was handed to its current texture at or after the frame
    /// that wrote the readout (`assigned[i]` is the refresh count when index `i` was last
    /// assigned): those words were written for the index's previous texture. Returns how many
    /// non-zero words were dropped.
    pub(crate) fn drop_stale(&mut self, assigned: &[u64]) -> u64 {
        let mut dropped = 0;
        for (code, &at) in self.codes.iter_mut().zip(assigned) {
            if *code != 0 && at >= self.tag {
                *code = 0;
                dropped += 1;
            }
        }
        dropped
    }

    /// Indices with a word (the neutral ones excluded).
    #[must_use]
    pub(crate) fn reported(&self) -> u64 {
        self.codes
            .iter()
            .skip(crate::texture_table::RESERVED_INDICES as usize)
            .filter(|&&code| code != 0)
            .count() as u64
    }
}

/// One streamed material's need from both sides (`render.texture_residency`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct MaterialResidency {
    /// The material's cache key (`FrameMesh::material_key`).
    pub material_key: u64,
    /// The largest need the GPU recorded among the material's streamed slots, texels along an edge.
    pub gpu_need_px: f32,
    /// The CPU estimate this frame (`0` when no draw measured it this frame).
    pub cpu_need_px: f32,
    /// The resident set's largest edge, texels.
    pub resident_px: u32,
}

impl MaterialResidency {
    /// `|log2(max(gpu, 1) / max(cpu, 1))|`: how many mip levels the two estimates disagree by.
    /// A CPU need of 0 is treated as 1 texel (the GPU need alone when the CPU measured nothing).
    #[must_use]
    pub fn mismatch_levels(&self) -> f32 {
        let gpu = self.gpu_need_px.max(1.0);
        let cpu = self.cpu_need_px.max(1.0);
        (gpu / cpu).log2().abs()
    }
}

/// What residency feedback did ([`crate::FrameRenderer::residency_feedback_stats`]).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ResidencyFeedbackStats {
    /// The device's mode.
    pub mode: FeedbackMode,
    /// Copies made (one per main-view frame while the ring has room).
    pub copies: u64,
    /// Frames whose copy was skipped because the ring was full (the GPU or the map is behind).
    pub skipped_copies: u64,
    /// Readouts used.
    pub readouts: u64,
    /// Copies read late and dropped because a newer one was ready.
    pub superseded: u64,
    /// Maps that failed (the copy is dropped).
    pub map_failures: u64,
    /// Words dropped because their index changed hands after the frame wrote them.
    pub stale_dropped: u64,
    /// Table indices with a need in the last readout.
    pub textures_reported: u64,
    /// Streamed materials with a GPU need in the last readout.
    pub materials_reported: u64,
    /// Of those, the materials whose stream took the need (mode `drive`).
    pub materials_driven: u64,
    /// Refreshes between the frame that wrote the last readout and its use.
    pub last_latency: u64,
    /// The last readout's streamed materials, largest mismatch first (at most
    /// [`REPORT_MATERIALS`]).
    pub materials: Vec<MaterialResidency>,
}

/// `materials` with the largest mismatch first, at most [`REPORT_MATERIALS`].
fn sorted_report(mut materials: Vec<MaterialResidency>) -> Vec<MaterialResidency> {
    materials.sort_by(|a, b| b.mismatch_levels().total_cmp(&a.mismatch_levels()));
    materials.truncate(REPORT_MATERIALS);
    materials
}

/// Copy and clear of the feedback words, recorded on the frame encoder rather than submitted
/// on their own.
///
/// `copy_to` is `None` when the ring had no free slot. The clear still runs. The phase word
/// is a queue write done before this copy, so the copy carries the new phase in word 0.
pub(crate) struct FeedbackCopy {
    /// The shader storage buffer.
    pub(crate) words: wgpu::Buffer,
    /// Ring slot that receives the copy. `None` when the claim failed.
    pub(crate) copy_to: Option<wgpu::Buffer>,
    /// Byte length of `words`.
    pub(crate) bytes: u64,
    /// Where the clear starts (past the phase word).
    pub(crate) clear_offset: u64,
}

/// The GPU side: the feedback words the shaders write, and the ring of mappable copies.
pub(crate) struct ResidencyFeedback {
    words: wgpu::Buffer,
    copies: [wgpu::Buffer; FEEDBACK_RING],
    /// Map results per ring slot: 0 pending, 1 mapped, 2 failed.
    flags: [Arc<AtomicU8>; FEEDBACK_RING],
    ring: FeedbackRing,
    bytes: u64,
    stats: ResidencyFeedbackStats,
    /// Readout words, reused across frames. [`Self::recycle`] parks a returned buffer here.
    codes: Vec<u32>,
    /// Copy and clear waiting for the frame encoder ([`Self::take_gpu_op`]).
    queued: Option<FeedbackCopy>,
    /// Ring slot claimed for a copy that [`Self::map_after_submit`] has not mapped.
    ///
    /// The next [`Self::frame`] frees it when the previous frame never submitted, so a
    /// dropped encoder does not pin the ring.
    awaiting_submit: Option<usize>,
    /// Times `codes` had to grow (tests).
    #[cfg(test)]
    codes_allocs: u64,
}

impl ResidencyFeedback {
    /// Feedback words for a table of `capacity` views on `device`, in `mode` (not `Off`).
    pub(crate) fn new(device: &wgpu::Device, capacity: u32, mode: FeedbackMode) -> Self {
        let bytes = u64::from(capacity.max(crate::texture_table::RESERVED_INDICES)) * 4;
        let words = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("texture table residency feedback"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let copies = std::array::from_fn(|_| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("texture table residency feedback readback"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        Self {
            words,
            copies,
            flags: std::array::from_fn(|_| Arc::new(AtomicU8::new(0))),
            ring: FeedbackRing::default(),
            bytes,
            stats: ResidencyFeedbackStats {
                mode,
                ..ResidencyFeedbackStats::default()
            },
            codes: Vec::new(),
            queued: None,
            awaiting_submit: None,
            #[cfg(test)]
            codes_allocs: 0,
        }
    }

    /// The layout entry of the feedback words in the table group.
    #[must_use]
    pub(crate) fn layout_entry() -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry {
            binding: FEEDBACK_BINDING,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }
    }

    /// The feedback words, for the table group's [`FEEDBACK_BINDING`] entry.
    #[must_use]
    pub(crate) fn binding(&self) -> wgpu::BindingResource<'_> {
        self.words.as_entire_binding()
    }

    /// The device's mode.
    #[must_use]
    pub(crate) fn mode(&self) -> FeedbackMode {
        self.stats.mode
    }

    /// One main-view frame, before streaming: reads the newest mapped copy (if any) into a
    /// reused buffer, writes the next phase, and queues the copy and clear for the frame
    /// encoder ([`Self::take_gpu_op`]). Does not submit and does not map.
    ///
    /// The caller records the copy on the frame encoder and calls [`Self::map_after_submit`]
    /// after that submit. The copy used to be its own command buffer, so the readout could
    /// finish before the rest of the frame. It now waits for the frame submit. The ring of
    /// [`FEEDBACK_RING`] copies absorbs that extra frame of latency.
    pub(crate) fn frame(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        tag: u64,
    ) -> Option<FeedbackReadout> {
        let _span = tracing::info_span!("residency_feedback").entered();
        // A claim whose copy never reached a submit must not stay `Mapping`.
        if let Some(slot) = self.awaiting_submit.take() {
            self.ring.free(slot);
            self.flags[slot].store(0, Ordering::Release);
        }
        // Map callbacks run inside a device poll (or a submit); poll without waiting.
        let _ = d.poll(wgpu::PollType::Poll);
        for slot in self.ring.mapping() {
            match self.flags[slot].load(Ordering::Acquire) {
                1 => self.ring.mapped(slot, true),
                2 => {
                    self.ring.mapped(slot, false);
                    self.stats.map_failures += 1;
                }
                _ => {}
            }
        }
        let ready = self.ring.ready();
        let mut readout = None;
        for (position, &(slot, ready_tag)) in ready.iter().enumerate() {
            if position == 0 {
                match self.copies[slot].slice(..).get_mapped_range() {
                    Ok(view) => {
                        let bytes: &[u8] = &view;
                        let words = bytes.as_chunks::<4>().0;
                        let mut codes = std::mem::take(&mut self.codes);
                        codes.clear();
                        if codes.capacity() < words.len() {
                            // `len` is 0, so this asks for `words.len()` elements in all.
                            codes.reserve(words.len());
                            #[cfg(test)]
                            {
                                self.codes_allocs += 1;
                            }
                        }
                        codes.extend(words.iter().map(|word| u32::from_le_bytes(*word)));
                        drop(view);
                        readout = Some(FeedbackReadout {
                            tag: ready_tag,
                            codes,
                        });
                        self.stats.readouts += 1;
                        self.stats.last_latency = tag.saturating_sub(ready_tag);
                    }
                    // A mapped range the device will not hand out counts as a failed map.
                    Err(_) => self.stats.map_failures += 1,
                }
            } else {
                self.stats.superseded += 1;
            }
            self.copies[slot].unmap();
            self.flags[slot].store(0, Ordering::Release);
            self.ring.free(slot);
        }
        let claimed = self.ring.claim(tag);
        let copy_to = match claimed {
            Some(slot) => {
                self.stats.copies += 1;
                self.awaiting_submit = Some(slot);
                Some(self.copies[slot].clone())
            }
            None => {
                self.stats.skipped_copies += 1;
                None
            }
        };
        let reserved = u64::from(crate::texture_table::RESERVED_INDICES) * 4;
        // Word 0 is the next frame's phase. A queue write lands before later submits, so the
        // frame encoder's copy carries the new phase in word 0 (never read as a need) and the
        // clear starts past it.
        q.write_buffer(&self.words, 0, bytemuck::bytes_of(&phase_for(tag + 1)));
        self.queued = Some(FeedbackCopy {
            words: self.words.clone(),
            copy_to,
            bytes: self.bytes,
            clear_offset: reserved,
        });
        readout
    }

    /// The copy and clear queued by the last [`Self::frame`], if that call ran.
    pub(crate) fn take_gpu_op(&mut self) -> Option<FeedbackCopy> {
        self.queued.take()
    }

    /// Maps the copy the last submitted frame encoder recorded. No-op when [`Self::frame`]
    /// claimed nothing, or when that claim was never submitted (the next `frame` frees it).
    pub(crate) fn map_after_submit(&mut self) {
        let Some(slot) = self.awaiting_submit.take() else {
            return;
        };
        let flag = Arc::clone(&self.flags[slot]);
        self.copies[slot]
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                flag.store(if result.is_ok() { 1 } else { 2 }, Ordering::Release);
            });
    }

    /// Parks `readout`'s word buffer for the next frame, when it is at least as large as the
    /// one already parked.
    pub(crate) fn recycle(&mut self, readout: FeedbackReadout) {
        if readout.codes.capacity() >= self.codes.capacity() {
            self.codes = readout.codes;
        }
    }

    /// Counts a readout's use, and keeps its per-material report (largest mismatch first).
    pub(crate) fn note_applied(
        &mut self,
        stale: u64,
        textures: u64,
        driven: u64,
        materials: Vec<MaterialResidency>,
    ) {
        self.stats.stale_dropped += stale;
        self.stats.textures_reported = textures;
        self.stats.materials_reported = materials.len() as u64;
        self.stats.materials_driven = driven;
        self.stats.materials = sorted_report(materials);
    }

    /// Copies, readouts and the last report.
    #[must_use]
    pub(crate) fn stats(&self) -> &ResidencyFeedbackStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_parse_and_are_stable_protocol_strings() {
        assert_eq!(FeedbackMode::parse(None), FeedbackMode::Off);
        assert_eq!(FeedbackMode::parse(Some("")), FeedbackMode::Off);
        assert_eq!(FeedbackMode::parse(Some("yes")), FeedbackMode::Off);
        assert_eq!(FeedbackMode::parse(Some(" Observe ")), FeedbackMode::Observe);
        assert_eq!(FeedbackMode::parse(Some("DRIVE")), FeedbackMode::Drive);
        for mode in [FeedbackMode::Off, FeedbackMode::Observe, FeedbackMode::Drive] {
            assert_eq!(FeedbackMode::parse(Some(mode.as_str())), mode);
            assert_eq!(mode.to_string(), mode.as_str());
            assert!(mode.as_str().bytes().all(|b| b.is_ascii_lowercase()));
        }
        assert_eq!(FeedbackMode::default(), FeedbackMode::Off);
    }

    #[test]
    fn feedback_needs_the_table_and_a_spare_storage_buffer() {
        let roomy = wgpu::Limits {
            max_storage_buffers_per_shader_stage: FEEDBACK_MIN_STORAGE_BUFFERS,
            ..wgpu::Limits::default()
        };
        let tight = wgpu::Limits {
            max_storage_buffers_per_shader_stage: FEEDBACK_MIN_STORAGE_BUFFERS - 1,
            ..wgpu::Limits::default()
        };
        assert_eq!(
            feedback_mode_for(FeedbackMode::Drive, true, &roomy),
            FeedbackMode::Drive
        );
        assert_eq!(
            feedback_mode_for(FeedbackMode::Observe, true, &roomy),
            FeedbackMode::Observe
        );
        assert_eq!(
            feedback_mode_for(FeedbackMode::Drive, false, &roomy),
            FeedbackMode::Off
        );
        assert_eq!(
            feedback_mode_for(FeedbackMode::Drive, true, &tight),
            FeedbackMode::Off
        );
        assert_eq!(
            feedback_mode_for(FeedbackMode::Off, true, &roomy),
            FeedbackMode::Off
        );
    }

    /// The word keeps the octave to 1/256 and orders like the need, so `atomicMax` keeps the
    /// largest need.
    #[test]
    fn need_words_round_trip_within_a_step_and_keep_order() {
        assert_eq!(encode_need(0.0), 1, "below one texel is one texel");
        assert_eq!(encode_need(1.0), 1);
        assert_eq!(decode_need(0), None, "untouched");
        assert_eq!(decode_need(1), Some(1.0));
        let step = (1.0f32 / NEED_STEPS_PER_OCTAVE).exp2();
        let mut last = 0;
        for px in [1.5f32, 2.0, 3.0, 17.0, 256.0, 1000.0, 2048.0, 4096.0, 40_000.0] {
            let code = encode_need(px);
            assert!(code > last, "{px}: {code} after {last}");
            last = code;
            let back = decode_need(code).expect("a sampled word");
            assert!(
                back <= px * 1.0001 && back * step >= px * 0.9999,
                "{px} → {code} → {back}"
            );
        }
        let top = encode_need(f32::MAX);
        assert_eq!(top, encode_need(1.0e30));
        assert!(decode_need(top).expect("word") < 65_536.0);
        assert!(top <= (NEED_MAX_LOG2 * NEED_STEPS_PER_OCTAVE) as u32);
    }

    /// Every pixel of a 4 × 4 cell writes once in 16 frames, and one pixel per cell per frame.
    #[test]
    fn the_phase_visits_every_pixel_of_a_cell() {
        for (x0, y0) in [(0u32, 0u32), (4, 8), (1023, 517)] {
            let cell_x = x0 & !3;
            let cell_y = y0 & !3;
            let mut writes = [[0u32; 4]; 4];
            for tag in 0..u64::from(PHASE_CELLS) {
                let phase = phase_for(tag);
                let mut this_frame = 0;
                for dy in 0..4 {
                    for dx in 0..4 {
                        if pixel_writes(cell_x + dx, cell_y + dy, phase) {
                            writes[dy as usize][dx as usize] += 1;
                            this_frame += 1;
                        }
                    }
                }
                assert_eq!(this_frame, 1, "one pixel per cell per frame");
            }
            assert!(writes.iter().flatten().all(|&n| n == 1), "{writes:?}");
        }
        assert_eq!(phase_for(16), phase_for(0));
    }

    #[test]
    fn the_ring_claims_in_turn_and_skips_when_behind() {
        let mut ring = FeedbackRing::default();
        assert_eq!(ring.claim(10), Some(0));
        assert_eq!(ring.claim(11), Some(1));
        assert_eq!(ring.claim(12), Some(2));
        assert_eq!(ring.claim(13), None, "every slot waits for its map");
        assert_eq!(ring.mapping(), vec![0, 1, 2]);
        ring.mapped(1, true);
        ring.mapped(0, true);
        assert_eq!(ring.claim(13), None, "slot 0 is next and still held");
        assert_eq!(ring.ready(), vec![(1, 11), (0, 10)], "newest first");
        ring.free(0);
        ring.free(1);
        assert_eq!(ring.claim(14), Some(0));
        assert_eq!(ring.claim(15), Some(1));
        ring.mapped(2, false);
        assert!(ring.ready().is_empty(), "a failed map is dropped");
        assert_eq!(ring.claim(16), Some(2));
        // Completing a slot that is not mapping changes nothing.
        ring.mapped(2, true);
        ring.mapped(2, true);
        assert_eq!(ring.ready(), vec![(2, 16)]);
        ring.free(7);
    }

    /// A deterministic run with a map that completes two frames after its copy: after warm-up
    /// every frame reads exactly one readout, two frames old, and no copy is skipped.
    #[test]
    fn a_steady_two_frame_map_reads_one_readout_a_frame() {
        let mut ring = FeedbackRing::default();
        let mut claimed_at: Vec<(usize, u64)> = Vec::new();
        let mut skipped = 0;
        let mut read = Vec::new();
        for frame in 0..40u64 {
            for &(slot, at) in &claimed_at {
                if frame == at + 2 {
                    ring.mapped(slot, true);
                }
            }
            let ready = ring.ready();
            if let Some(&(_, tag)) = ready.first() {
                read.push((frame, tag));
            }
            for (slot, _) in ready {
                ring.free(slot);
            }
            match ring.claim(frame) {
                Some(slot) => claimed_at.push((slot, frame)),
                None => skipped += 1,
            }
        }
        assert_eq!(skipped, 0);
        assert_eq!(read.len(), 38);
        assert!(read.iter().all(|&(frame, tag)| frame - tag == 2), "{read:?}");
    }

    #[test]
    fn stale_words_are_dropped_and_neutral_indices_never_read() {
        let mut readout = FeedbackReadout {
            tag: 10,
            codes: vec![
                7,
                9,
                9,
                9,
                encode_need(64.0),
                encode_need(512.0),
                0,
                encode_need(8.0),
            ],
        };
        // Index 5 changed hands at refresh 10 (the frame that wrote it), index 7 at 3.
        let assigned = [0, 0, 0, 0, 2, 10, 0, 3];
        assert_eq!(readout.drop_stale(&assigned), 1);
        assert_eq!(readout.need_px(0), None, "word 0 is the phase");
        assert_eq!(readout.need_px(3), None, "neutral views");
        assert!(readout.need_px(4).is_some_and(|px| (px - 64.0).abs() < 0.5));
        assert_eq!(readout.need_px(5), None, "stale");
        assert_eq!(readout.need_px(6), None, "not sampled");
        assert!(readout.need_px(7).is_some());
        assert_eq!(readout.need_px(99), None, "past the table");
        assert_eq!(readout.reported(), 2);
    }

    /// Observe mode queues the copy for the caller and reuses one codes buffer across two
    /// ready maps. Skips when no headless adapter exists. Not a golden.
    #[test]
    fn gpu_feedback_frame_reuses_codes_without_submitting() {
        let device = match crate::RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("gpu_feedback skipped: {err}");
                return;
            }
        };
        let mut feedback = ResidencyFeedback::new(&device.device, 8, FeedbackMode::Observe);
        let queue = device.tracked_queue();
        let submit_copy = |feedback: &mut ResidencyFeedback| {
            let op = feedback.take_gpu_op().expect("frame queued a copy");
            let mut belt = crate::staging::StagingBelt::default();
            belt.defer_feedback(op);
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("feedback test copy"),
                });
            belt.flush(&mut encoder);
            let _ = queue.submit([encoder.finish()]);
            feedback.map_after_submit();
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
        };

        let before = crate::frame_stats::submits();
        assert!(feedback.frame(&device.device, &queue, 1).is_none());
        assert_eq!(
            crate::frame_stats::submits(),
            before,
            "frame() submitted its own command buffer"
        );
        submit_copy(&mut feedback);

        let before = crate::frame_stats::submits();
        let second = feedback.frame(&device.device, &queue, 2);
        assert_eq!(crate::frame_stats::submits(), before);
        let second = second.expect("the first copy mapped");
        let ptr = second.codes.as_ptr();
        let cap = second.codes.capacity();
        let allocs = feedback.codes_allocs;
        assert!(cap > 0, "the readout borrowed no words");
        feedback.recycle(second);
        submit_copy(&mut feedback);

        let third = feedback
            .frame(&device.device, &queue, 3)
            .expect("the second copy mapped");
        assert_eq!(third.codes.as_ptr(), ptr, "the codes buffer was allocated again");
        assert_eq!(third.codes.capacity(), cap);
        assert_eq!(feedback.codes_allocs, allocs, "codes grew on the second readout");
        let _ = feedback.take_gpu_op();
    }

    #[test]
    fn reports_put_the_largest_disagreement_first() {
        let m = |key: u64, gpu: f32, cpu: f32| MaterialResidency {
            material_key: key,
            gpu_need_px: gpu,
            cpu_need_px: cpu,
            resident_px: 256,
        };
        assert!((m(1, 1024.0, 256.0).mismatch_levels() - 2.0).abs() < 1e-5);
        assert!((m(1, 256.0, 1024.0).mismatch_levels() - 2.0).abs() < 1e-5);
        assert!(
            (m(1, 64.0, 0.0).mismatch_levels() - 6.0).abs() < 1e-5,
            "no CPU estimate"
        );
        let mut materials: Vec<MaterialResidency> =
            (0..100).map(|k| m(k, 2.0f32.powi((k % 9) as i32), 1.0)).collect();
        materials.push(m(500, 4096.0, 1.0));
        let report = sorted_report(materials);
        assert_eq!(report.len(), REPORT_MATERIALS);
        assert_eq!(report[0].material_key, 500);
        assert!(
            report
                .windows(2)
                .all(|w| w[0].mismatch_levels() >= w[1].mismatch_levels())
        );
    }
}

/// The feedback variants compose and validate (naga, no GPU) and declare exactly one more
/// binding in the table group: a read-write storage array of atomics at [`FEEDBACK_BINDING`].
#[cfg(test)]
mod shader_tests {
    use std::collections::HashMap;

    use naga_oil::compose::ShaderDefValue;

    use super::*;
    use crate::texture_table::{TEXTURE_TABLE_DEF, TEXTURE_TABLE_GROUP, TEXTURE_TABLE_SIZE_DEF};

    const SIZE: u32 = 4032;

    fn compose(source: &str, file: &str, extra: &[&str], feedback: bool) -> naga::Module {
        let mut defs: HashMap<String, ShaderDefValue> = extra
            .iter()
            .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
            .collect();
        defs.insert(
            crate::bindless::MATERIAL_SSBO_DEF.to_string(),
            ShaderDefValue::Bool(true),
        );
        defs.insert(TEXTURE_TABLE_DEF.to_string(), ShaderDefValue::Bool(true));
        defs.insert(TEXTURE_TABLE_SIZE_DEF.to_string(), ShaderDefValue::UInt(SIZE));
        if feedback {
            defs.insert(RESIDENCY_FEEDBACK_DEF.to_string(), ShaderDefValue::Bool(true));
        }
        crate::shader_lib::compose_source(source, file, defs)
            .unwrap_or_else(|err| panic!("{file} {extra:?} feedback {feedback}: {err}"))
    }

    fn validate(module: &naga::Module, what: &str) {
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        .unwrap_or_else(|err| panic!("{what}: {err:?}"));
    }

    fn table_group(module: &naga::Module) -> Vec<(u32, naga::AddressSpace, naga::TypeInner)> {
        let mut found: Vec<_> = module
            .global_variables
            .iter()
            .filter_map(|(_, var)| {
                let binding = var.binding.as_ref().filter(|b| b.group == TEXTURE_TABLE_GROUP)?;
                Some((binding.binding, var.space, module.types[var.ty].inner.clone()))
            })
            .collect();
        found.sort_by_key(|(binding, _, _)| *binding);
        found
    }

    /// The standard-material colour variants: CPU, GPU-driven, instanced, glass, mask decals,
    /// and the visibility resolve.
    fn cases() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let resolve = crate::visbuffer::gpu::resolve_source();
        vec![
            (forward, "lab_forward.wgsl", &["ALPHA_TEST"]),
            (
                forward,
                "lab_forward.wgsl",
                &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
            ),
            (forward, "lab_forward.wgsl", &["GPU_DRIVEN", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["GLASS", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["MASK_DECALS", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["THIN_FILM", "ALPHA_TEST"]),
            (resolve, "vis_resolve.wgsl", &["VISBUFFER"]),
        ]
    }

    #[test]
    fn feedback_variants_validate_and_add_one_storage_binding() {
        for (source, file, extra) in cases() {
            let what = format!("{file} {extra:?}");
            let module = compose(source, file, extra, true);
            validate(&module, &what);
            let group = table_group(&module);
            assert_eq!(
                group.iter().map(|(binding, _, _)| *binding).collect::<Vec<_>>(),
                vec![0, 1, FEEDBACK_BINDING, 3, 4],
                "{what}: the table, its sampler, the feedback words and the rest of the sampler set"
            );
            let (_, space, inner) = &group[2];
            assert!(
                matches!(space, naga::AddressSpace::Storage { access } if access.contains(naga::StorageAccess::STORE)),
                "{what}: the feedback words are read-write storage ({space:?})"
            );
            match inner {
                naga::TypeInner::Array {
                    base,
                    size: naga::ArraySize::Dynamic,
                    ..
                } => assert!(
                    matches!(module.types[*base].inner, naga::TypeInner::Atomic(_)),
                    "{what}: one atomic word per index"
                ),
                other => panic!("{what}: the feedback binding is {other:?}"),
            }
            let without = compose(source, file, extra, false);
            assert_eq!(
                table_group(&without)
                    .iter()
                    .map(|(binding, _, _)| *binding)
                    .collect::<Vec<_>>(),
                vec![0, 1, 3, 4],
                "{what}: without the define the table group is the array and its sampler set"
            );
        }
    }

    /// The translucent casters compose the table defines too; the feedback define adds nothing
    /// to them (they shade no colour the stream reads).
    #[test]
    fn casters_ignore_the_feedback_define() {
        let caster = include_str!("../shaders/translucent_shadow.wgsl");
        for extra in [&[][..], &["GLASS"][..]] {
            let module = compose(caster, "translucent_shadow.wgsl", extra, true);
            validate(&module, "translucent_shadow.wgsl");
            assert_eq!(
                table_group(&module)
                    .iter()
                    .map(|(binding, _, _)| *binding)
                    .collect::<Vec<_>>(),
                vec![0, 1, 3, 4],
                "the array and its sampler set, no feedback words"
            );
        }
    }

    /// The fragment entry points that shade standard materials store feedback; the depth
    /// prepass does not (its fragment stage only tests alpha).
    #[test]
    fn only_colour_entry_points_store_feedback() {
        let module = compose(
            include_str!("../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            &["ALPHA_TEST"],
            true,
        );
        let Some((feedback, _)) = module.global_variables.iter().find(|(_, var)| {
            var.binding
                .as_ref()
                .is_some_and(|b| b.group == TEXTURE_TABLE_GROUP && b.binding == FEEDBACK_BINDING)
        }) else {
            panic!("no feedback binding");
        };
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|err| panic!("{err:?}"));
        let uses = |name: &str| {
            module
                .entry_points
                .iter()
                .position(|ep| ep.name == name)
                .map(|index| !info.get_entry_point(index)[feedback].is_empty())
        };
        assert_eq!(uses("fs_main"), Some(true), "the forward colour pass stores");
        assert_eq!(uses("fs_depth"), Some(false), "the depth prepass does not");
    }
}
