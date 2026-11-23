// SPDX-License-Identifier: MPL-2.0
//! Per window view LOD hysteresis history and the retirement of a view's history (ADR-8620).
//!
//! ADR-7329 gave each split-screen view its own Hi-Z pyramid and cluster visibility bits; the
//! cluster LOD hysteresis history (ADR-4631) stayed shared, so a view change read as a camera
//! jump (the views stand apart) or handed one view's held level to the other. Here each window
//! view keeps its own history: the CPU mirror (`lod_history`, `lod_previous`, the keys, the eye,
//! the pending discard) swaps with the view's parked copy, without allocating once both exist,
//! and the GPU boundary words (`lod_words` from `cap + 1`) are copied out and back like the
//! cluster bits. One view never swaps and never copies.

use super::{GpuInstance, TrackedDevice, VisBufferPass, make_buffer};

/// The CPU half of a view's LOD history, parked while another view draws.
#[derive(Default)]
pub(super) struct LodCpu {
    history: Vec<u32>,
    previous: Vec<GpuInstance>,
    previous_keys: Vec<Option<u64>>,
    sets_generation: u64,
    eye: Option<[f32; 3]>,
    clear: bool,
}

/// A window view's LOD history while another view draws.
#[derive(Default)]
pub(super) struct LodViewSlot {
    /// The parked CPU mirror (its vectors keep their capacity while the slot is empty).
    cpu: LodCpu,
    /// `cpu` holds this view's history (false: never drawn, or retired).
    held: bool,
    /// The view's boundary words and how many bytes of them are its history.
    gpu: Option<(wgpu::Buffer, u64)>,
}

impl VisBufferPass {
    /// Swaps the live CPU history with `cpu`.
    fn swap_lod_cpu(&mut self, cpu: &mut LodCpu) {
        std::mem::swap(&mut self.lod_history, &mut cpu.history);
        std::mem::swap(&mut self.lod_previous, &mut cpu.previous);
        std::mem::swap(&mut self.lod_previous_keys, &mut cpu.previous_keys);
        std::mem::swap(&mut self.lod_sets_generation, &mut cpu.sets_generation);
        std::mem::swap(&mut self.lod_eye, &mut cpu.eye);
        std::mem::swap(&mut self.lod_clear, &mut cpu.clear);
    }

    /// Makes the live history empty (a view that has none): the next frame starts from the hard
    /// cut, which is not a discard (no eye is stored). Keeps the vectors' capacity.
    fn reset_live_lod(&mut self) {
        self.lod_history.clear();
        self.lod_previous.clear();
        self.lod_previous_keys.clear();
        self.lod_eye = None;
        self.lod_clear = true;
    }

    /// The live boundary words: their byte offset in the history buffer.
    fn lod_words_offset(&self) -> u64 {
        (u64::from(self.lod_slots) + 1) * 4
    }

    /// Parks the outgoing view's LOD history (`out`, `None` when no view's history is live) and
    /// brings `view`'s in (ADR-8620). Called from `switch_bits_view` when the view changes.
    pub(super) fn switch_lod_view(
        &mut self,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        out: Option<u32>,
        view: u32,
    ) {
        let Some((live, live_size)) = self.buffers.lod.as_ref().map(|(b, _)| (b.clone(), b.size())) else {
            return;
        };
        let offset = self.lod_words_offset().min(live_size);
        let index = view as usize;
        let needed = index.max(out.map_or(0, |o| o as usize)) + 1;
        if self.lod_views.len() < needed {
            self.lod_views.resize_with(needed, LodViewSlot::default);
        }
        if let Some(out) = out.map(|o| o as usize) {
            let _span = tracing::info_span!("visbuffer_lod_view").entered();
            let used = (self.lod_history.len() as u64 * 4).min(live_size - offset);
            let mut slot = std::mem::take(&mut self.lod_views[out]);
            self.swap_lod_cpu(&mut slot.cpu);
            slot.held = true;
            if slot.gpu.as_ref().is_none_or(|(saved, _)| saved.size() < used) {
                slot.gpu = Some((
                    make_buffer(
                        d,
                        "vis lod history view",
                        used.max(4),
                        wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                    ),
                    0,
                ));
            }
            if let Some((saved, saved_used)) = slot.gpu.as_mut() {
                if used > 0 {
                    encoder.copy_buffer_to_buffer(&live, offset, saved, 0, used);
                    self.lod_view_copies += 1;
                }
                *saved_used = used;
            }
            self.lod_views[out] = slot;
        }
        let slot = &mut self.lod_views[index];
        if !slot.held {
            self.reset_live_lod();
            return;
        }
        let mut cpu = std::mem::take(&mut slot.cpu);
        slot.held = false;
        let restore = slot.gpu.as_ref().map(|(saved, used)| (saved.clone(), *used));
        self.swap_lod_cpu(&mut cpu);
        // `cpu` now holds the outgoing (already parked) or empty vectors: keep their capacity in
        // the slot for the view's next parking.
        self.lod_views[index].cpu = cpu;
        if let Some((saved, used)) = restore {
            let n = used.min(live_size - offset);
            if n > 0 {
                encoder.copy_buffer_to_buffer(&saved, 0, &live, offset, n);
                self.lod_view_copies += 1;
            }
            if n < used {
                // The live buffer cannot hold the view's words: the next frame starts over.
                self.lod_clear = true;
            }
        }
    }

    /// Discards `view`'s LOD history at its next frame: that view's camera cut (ADR-8620). The
    /// other views keep theirs. A view index the views do not track (64 or more) shares the
    /// live history.
    pub(crate) fn clear_lod_history_of(&mut self, view: u32) {
        let live = self.bits_view.is_none_or(|v| v == view)
            || view as usize >= crate::prepass_occlusion::MAX_OCCLUSION_VIEWS;
        if live {
            self.lod_clear = true;
        } else if let Some(slot) = self.lod_views.get_mut(view as usize)
            && slot.held
        {
            slot.cpu.clear = true;
        }
    }

    /// Marks every parked view's LOD history for a discard at its next frame (resident geometry
    /// went away: their boundary words may name clusters that moved).
    pub(super) fn clear_parked_lod_views(&mut self) {
        for slot in &mut self.lod_views {
            if slot.held {
                slot.cpu.clear = true;
            }
        }
    }

    /// Frees everything `view` keeps for its occlusion history in the visibility buffer: its
    /// parked cluster bits and LOD history (ADR-8620's retirement). When `view` is the live
    /// one, the live state forgets it: the next view to draw starts from cleared bits and an
    /// empty LOD history.
    pub(crate) fn retire_view(&mut self, view: u32) {
        let index = view as usize;
        if let Some(bits) = self.view_bits.get_mut(index) {
            *bits = None;
        }
        if let Some(slot) = self.lod_views.get_mut(index) {
            *slot = LodViewSlot::default();
        }
        if self.bits_view == Some(view) {
            self.bits_view = None;
            self.bits_stale = true;
            self.reset_live_lod();
        }
        self.drop_lone_view_buffers();
    }

    /// Frees the LOD histories kept for window views at index `views` and above.
    pub(super) fn retain_lod_views(&mut self, views: usize) {
        self.lod_views.truncate(views);
    }

    /// When no other view keeps anything aside, the live view needs no buffers to park its bits
    /// and LOD history in: one view again holds nothing besides the live state (ADR-8620).
    pub(super) fn drop_lone_view_buffers(&mut self) {
        let live = self.bits_view.map(|view| view as usize);
        let slots = self.view_bits.len().max(self.lod_views.len());
        let others = (0..slots).filter(|&index| Some(index) != live).any(|index| {
            self.view_bits.get(index).is_some_and(Option::is_some)
                || self
                    .lod_views
                    .get(index)
                    .is_some_and(|slot| slot.held || slot.gpu.is_some())
        });
        if others {
            return;
        }
        if let Some(index) = live {
            if let Some(bits) = self.view_bits.get_mut(index) {
                *bits = None;
            }
            if let Some(slot) = self.lod_views.get_mut(index) {
                *slot = LodViewSlot::default();
            }
        }
    }

    /// Window views whose cluster bits or LOD history are kept aside, and the GPU bytes they
    /// hold (ADR-8620; `VisBufferStats::view_histories`, `view_history_bytes`).
    pub(super) fn view_history_report(&self) -> (u32, u64) {
        let slots = self.view_bits.len().max(self.lod_views.len());
        let mut views = 0u32;
        let mut bytes = 0u64;
        for index in 0..slots {
            let bits = self.view_bits.get(index).and_then(Option::as_ref);
            let lod = self
                .lod_views
                .get(index)
                .filter(|slot| slot.held || slot.gpu.is_some());
            if bits.is_none() && lod.is_none() {
                continue;
            }
            views += 1;
            bytes += bits.map_or(0, |(buffer, _)| buffer.size());
            bytes += lod
                .and_then(|slot| slot.gpu.as_ref())
                .map_or(0, |(buffer, _)| buffer.size());
        }
        (views, bytes)
    }
}
