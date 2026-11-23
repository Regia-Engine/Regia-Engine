// SPDX-License-Identifier: MPL-2.0
//! The two-phase occlusion result, read back without a stall for `ViewVisibility`
//! (`docs/specs/14-scene-and-framework.md` § A1, ADR-7255; per view and clusters, ADR-7286).
//!
//! The depth prepass's two-phase cull (`crate::prepass_occlusion`, ADR-1887) writes one indirect
//! command per depth instance and phase; an instance count of 0 in both phases means the GPU
//! drew it in neither: outside the frustum or behind what the frame already drew. When a
//! renderer holds an [`OcclusionFeedback`] ([`crate::FrameRenderer::set_occlusion_feedback`]),
//! each main-view frame that culls in two phases copies the command words into one of three
//! staging slots after phase 2 and maps the slot after the frame's submit. The CPU never waits:
//! [`OcclusionFeedback::collect`] (run by [`crate::update_view_visibility`] after a non-blocking
//! poll) reads only slots whose map already completed, newest recording first, and keeps the
//! object keys the GPU culled although the CPU frustum test of their bound passed (occluded, not
//! off screen). A main-view frame that does not cull in two phases clears the result, so a
//! switched-off cull never leaves stale occlusion behind.
//!
//! **Visibility-buffer clusters (ADR-7286).** The cluster cull (`vis_cull.wgsl`) keeps one
//! visibility bit per cluster: phase 2 sets it for every cluster it selects that the Hi-Z pyramid
//! does not hide and clears it for one it hides. A main-view frame that draws clusters copies
//! those bit words into a second ring of slots, with each instance's object key and bit range.
//! An instance none of whose bits is set drew no cluster this frame (hidden, or off screen; the
//! fold's own frustum test tells them apart). A bit of a cluster the frame did not select (another
//! level of detail, a back-facing cluster) keeps an older value, so a stale bit can only read as
//! drawn: the result errs toward visible. No shader or pass is added: one buffer copy per frame.
//!
//! **Lag.** The result `ViewVisibility` reads on frame N is the newest frame whose readback had
//! completed when frame N's culling ran: frame N − 1 when the GPU kept up, older under a deeper
//! queue (never newer). A mesh uncovered on frame N reads true from frame N + 1 at the earliest;
//! one that just went behind an occluder stays true for the same lag. Keys of objects neither
//! cull holds (blended draws, persistent instance sets, CPU-path frames) are never in the result,
//! so those stay frustum-only (conservative).
//!
//! **Views (ADR-7286).** Each window view the renderer culls has its own ring of slots and its
//! own result, keyed by the view's index in `ExtractedFrame::views`: the host names the view
//! before it draws it ([`crate::FrameRenderer::set_occlusion_view`]; a host that draws one view
//! never needs to, the default is view 0). A view without a cull (a texture view never culls in
//! two phases or draws clusters) has no ring, and a ring of a view that went away is dropped
//! ([`OcclusionFeedback::retain_views`]). `ViewVisibility` asks each view's result about that
//! view only: an entity is visible when any view sees it.
//!
//! A host with its own occlusion test may [`OcclusionFeedback::publish`] its result instead
//! ([`OcclusionFeedback::publish_view`] for a view other than the first).
//! Steady state allocates nothing: the rings, their slots, their key lists and the results keep
//! their capacity.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;

use crate::prepass_occlusion::CMD_STRIDE;
use crate::readback::{ST_FAILED, ST_IDLE, ST_INFLIGHT, ST_READY};

/// Staging slots per stream: one being written by the GPU, one mapped and waiting, one free to
/// record.
const SLOTS: usize = 3;

/// The forward prepass's commands (stream 0) and the visibility buffer's cluster bits (stream 1).
const FORWARD: usize = 0;
const CLUSTERS: usize = 1;

/// What the readback did since it was made ([`OcclusionFeedback::stats`], agent
/// `render.visible_entities`). Counts only, no timing. The forward prepass's and the cluster
/// cull's readbacks count together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OcclusionFeedbackStats {
    /// Culled frames whose commands (or cluster bits) were copied for readback.
    pub recorded: u64,
    /// Culled frames with no idle slot (the GPU fell three frames behind): not copied.
    pub dropped: u64,
    /// Readbacks taken into the result.
    pub read: u64,
    /// Readbacks that arrived after a newer result and were thrown away.
    pub stale: u64,
    /// Main-view frames without a two-phase cull (or without clusters), which cleared that part
    /// of the result.
    pub cleared: u64,
    /// Results a host published ([`OcclusionFeedback::publish`]).
    pub published: u64,
    /// Object keys in the current result.
    pub occluded: usize,
    /// Recording sequence number of the current result (0: none yet).
    pub result_seq: u64,
    /// Bytes of the Hi-Z pyramid the renderer keeps for this view's occlusion history (summed
    /// over the views in [`OcclusionFeedback::stats`]); current, not a count (ADR-7329).
    pub pyramid_bytes: u64,
}

/// One staging slot.
struct Slot {
    staging: Option<wgpu::Buffer>,
    /// Bytes the copy filled (phase 1, then phase 2 when it ran; or the cluster bit words).
    filled: u64,
    /// Forward: per depth instance, in cull-list order, its object key and whether its bound
    /// passed the CPU frustum test (only those can be occluded).
    keys: Vec<(u64, bool)>,
    /// Clusters: per instance, its object key, first visibility bit and bit count.
    ranges: Vec<(u64, u32, u32)>,
    /// Forward: phase 2 ran and its commands follow phase 1's.
    phase2: bool,
    /// Recording sequence number.
    seq: u64,
    state: Arc<AtomicU8>,
}

impl Slot {
    fn new() -> Self {
        Self {
            staging: None,
            filled: 0,
            keys: Vec::new(),
            ranges: Vec::new(),
            phase2: false,
            seq: 0,
            state: Arc::new(AtomicU8::new(ST_IDLE)),
        }
    }
}

/// One readback stream of one view: its slots and its newest result.
struct Stream {
    slots: Vec<Slot>,
    /// Sorted, unique object keys the newest result found not drawn.
    occluded: Vec<u64>,
    /// Sequence number of that result.
    result_seq: u64,
}

impl Stream {
    fn new() -> Self {
        Self {
            slots: (0..SLOTS).map(|_| Slot::new()).collect(),
            occluded: Vec::new(),
            result_seq: 0,
        }
    }

    /// A slot is copied into or waiting for its map.
    fn waiting(&self) -> bool {
        self.slots.iter().any(|slot| {
            let state = slot.state.load(Ordering::Acquire);
            state == ST_INFLIGHT || state == ST_READY
        })
    }
}

/// One view's streams and counters.
struct Ring {
    /// The view's index in `ExtractedFrame::views`.
    view: u32,
    /// [`FORWARD`] and [`CLUSTERS`].
    streams: [Stream; 2],
    stats: OcclusionFeedbackStats,
}

impl Ring {
    fn new(view: u32) -> Self {
        Self {
            view,
            streams: [Stream::new(), Stream::new()],
            stats: OcclusionFeedbackStats::default(),
        }
    }

    /// Keeps `stats.occluded` and `stats.result_seq` in step with the streams' results.
    fn sync_result(&mut self) {
        self.stats.occluded = self.streams.iter().map(|s| s.occluded.len()).sum();
        self.stats.result_seq = self.streams.iter().map(|s| s.result_seq).max().unwrap_or(0);
    }

    fn holds(&self, key: u64) -> bool {
        self.streams
            .iter()
            .any(|stream| stream.occluded.binary_search(&key).is_ok())
    }
}

struct Inner {
    /// One ring per view that recorded, cleared or published (ascending by view).
    rings: Vec<Ring>,
    /// The device the slots' buffers belong to (a new device drops them).
    device: Option<wgpu::Device>,
    /// Per stream: the ring and slot this frame copied into, mapped after the submit.
    pending: [Option<(usize, usize)>; 2],
    next_seq: u64,
    /// Keys of drawn instances while a result is built (an object with any drawn instance is
    /// not occluded).
    drawn: Vec<u64>,
    /// Counts of rings dropped by [`OcclusionFeedback::retain_views`], so the totals never go back.
    retired: OcclusionFeedbackStats,
    /// Per view (ascending), the bytes of the Hi-Z pyramid the renderer keeps for it (ADR-7329).
    pyramids: Vec<(u32, u64)>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            rings: Vec::new(),
            device: None,
            pending: [None, None],
            next_seq: 1,
            drawn: Vec::new(),
            retired: OcclusionFeedbackStats::default(),
            pyramids: Vec::new(),
        }
    }
}

impl Inner {
    /// The bytes of `view`'s pyramid (0 without one).
    fn pyramid_bytes(&self, view: u32) -> u64 {
        self.pyramids
            .binary_search_by_key(&view, |&(v, _)| v)
            .map_or(0, |index| self.pyramids[index].1)
    }

    /// The ring of `view`, if it has one.
    fn ring(&self, view: u32) -> Option<&Ring> {
        self.rings
            .binary_search_by_key(&view, |ring| ring.view)
            .ok()
            .map(|index| &self.rings[index])
    }

    /// The index of `view`'s ring, made on first use (kept in view order).
    fn ring_index(&mut self, view: u32) -> usize {
        match self.rings.binary_search_by_key(&view, |ring| ring.view) {
            Ok(index) => index,
            Err(index) => {
                self.rings.insert(index, Ring::new(view));
                // A ring moved: the pending slots' ring indices move with it.
                for (ring, _) in self.pending.iter_mut().flatten() {
                    if *ring >= index {
                        *ring += 1;
                    }
                }
                index
            }
        }
    }

    /// A new device drops every slot's buffer (a slot still mapping goes with its device).
    fn follow_device(&mut self, device: &wgpu::Device) {
        if self.device.as_ref() != Some(device) {
            for ring in &mut self.rings {
                for stream in &mut ring.streams {
                    stream.slots = (0..SLOTS).map(|_| Slot::new()).collect();
                }
            }
            self.device = Some(device.clone());
            self.pending = [None, None];
        }
    }

    /// An idle slot of `view`'s `stream` for this frame's copy, its buffer grown to `bytes`;
    /// `None` (counted as dropped) when the GPU holds all three.
    fn take_slot(
        &mut self,
        device: &crate::TrackedDevice,
        view: u32,
        stream: usize,
        (bytes, grow): (u64, u64),
    ) -> Option<(usize, usize)> {
        let ring_index = self.ring_index(view);
        let pending = self.pending[stream];
        let ring = &mut self.rings[ring_index];
        let Some(index) = ring.streams[stream]
            .slots
            .iter()
            .enumerate()
            .position(|(index, slot)| {
                slot.state.load(Ordering::Acquire) == ST_IDLE && Some((ring_index, index)) != pending
            })
        else {
            ring.stats.dropped += 1;
            return None;
        };
        let slot = &mut ring.streams[stream].slots[index];
        if slot.staging.as_ref().is_none_or(|b| b.size() < bytes) {
            // Grows to the next power of two of `grow`, so a steady count makes no buffer.
            slot.staging = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("occlusion feedback readback"),
                size: grow.max(bytes).next_power_of_two(),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        Some((ring_index, index))
    }

    /// Stamps the slot this frame copied into and marks it pending for the map.
    fn commit_slot(&mut self, stream: usize, (ring, index): (usize, usize), filled: u64) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let Some(ring_ref) = self.rings.get_mut(ring) else {
            return;
        };
        let slot = &mut ring_ref.streams[stream].slots[index];
        slot.filled = filled;
        slot.seq = seq;
        ring_ref.stats.recorded += 1;
        self.pending[stream] = Some((ring, index));
    }

    /// Clears `view`'s `stream` result (as the newest) after a frame without that cull. A view
    /// without a ring, or a stream already clear with nothing in flight, counts nothing.
    fn clear_stream(&mut self, view: u32, stream: usize) {
        let Ok(index) = self.rings.binary_search_by_key(&view, |ring| ring.view) else {
            return;
        };
        let pending = self.pending[stream].is_some_and(|(ring, _)| ring == index);
        let seq = self.next_seq;
        let ring = &mut self.rings[index];
        let part = &mut ring.streams[stream];
        if part.occluded.is_empty() && !pending && !part.waiting() {
            return;
        }
        self.next_seq += 1;
        part.occluded.clear();
        part.result_seq = seq;
        ring.stats.cleared += 1;
        ring.sync_result();
    }
}

/// Adds `b`'s counters into `a` (the result fields take the larger sequence, the summed keys).
fn add_stats(a: &mut OcclusionFeedbackStats, b: &OcclusionFeedbackStats) {
    a.recorded += b.recorded;
    a.dropped += b.dropped;
    a.read += b.read;
    a.stale += b.stale;
    a.cleared += b.cleared;
    a.published += b.published;
    a.occluded += b.occluded;
    a.result_seq = a.result_seq.max(b.result_seq);
    a.pyramid_bytes += b.pyramid_bytes;
}

/// One view's readback ([`OcclusionFeedback::view_stats_into`], agent `render.visible_entities`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OcclusionViewStats {
    /// The view's index in `ExtractedFrame::views`.
    pub view: u32,
    /// What this view's readback did; `occluded` is this view's result only.
    pub stats: OcclusionFeedbackStats,
}

/// Whether any bit in `[base, base + count)` of the little-endian `words` is set. A range past
/// the copied words reads as set (drawn), so a short copy never hides anything.
fn any_bit(words: &[[u8; 4]], base: u32, count: u32) -> bool {
    if count == 0 {
        return false;
    }
    let end = u64::from(base) + u64::from(count);
    let (first, last) = (u64::from(base) / 32, (end - 1) / 32);
    if last >= words.len() as u64 {
        return true;
    }
    (first..=last).any(|w| {
        let word = u32::from_le_bytes(words[w as usize]);
        let lo = if w == first { base % 32 } else { 0 };
        let hi = if w == last { ((end - 1) % 32) as u32 } else { 31 };
        let mask = (u32::MAX >> (31 - hi)) & (u32::MAX << lo);
        word & mask != 0
    })
}

/// A shared handle to the last frame's GPU occlusion result (module docs). A `Resource`: the
/// runtime hands the same handle to its renderer, and [`crate::update_view_visibility`] reads it.
/// Cloning shares the state.
#[derive(Resource, Clone, Default)]
pub struct OcclusionFeedback(Arc<Mutex<Inner>>);

impl std::fmt::Debug for OcclusionFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OcclusionFeedback").field(&self.stats()).finish()
    }
}

impl OcclusionFeedback {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `other` is a clone of this handle (same state).
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// What the readback did so far, summed over every view (`occluded`: the keys of every
    /// view's result added up; `result_seq`: the newest result of any view). Views that went
    /// away still count.
    #[must_use]
    pub fn stats(&self) -> OcclusionFeedbackStats {
        let inner = self.lock();
        let mut total = inner.retired;
        total.occluded = 0;
        total.pyramid_bytes = 0;
        for ring in &inner.rings {
            add_stats(&mut total, &ring.stats);
        }
        // Every view's pyramid, with a ring or not.
        total.pyramid_bytes = inner.pyramids.iter().map(|&(_, bytes)| bytes).sum();
        total
    }

    /// Each view's readback, ascending by view, into `out` (its capacity reused).
    pub fn view_stats_into(&self, out: &mut Vec<OcclusionViewStats>) {
        let inner = self.lock();
        out.clear();
        out.extend(inner.rings.iter().map(|ring| OcclusionViewStats {
            view: ring.view,
            stats: OcclusionFeedbackStats {
                pyramid_bytes: inner.pyramid_bytes(ring.view),
                ..ring.stats
            },
        }));
    }

    /// Records that the renderer keeps a Hi-Z pyramid of `bytes` for `view`'s occlusion history
    /// (0: none any more; ADR-7329). The renderer reports it when it changes; agent
    /// `render.visible_entities` shows it per view. Allocates only for a view not seen before.
    pub fn note_view_pyramid(&self, view: u32, bytes: u64) {
        let mut inner = self.lock();
        match inner.pyramids.binary_search_by_key(&view, |&(v, _)| v) {
            Ok(index) if bytes == 0 => {
                inner.pyramids.remove(index);
            }
            Ok(index) => inner.pyramids[index].1 = bytes,
            Err(_) if bytes == 0 => {}
            Err(index) => inner.pyramids.insert(index, (view, bytes)),
        }
    }

    /// The bytes of the Hi-Z pyramid kept for `view` ([`Self::note_view_pyramid`]; 0 without
    /// one).
    #[must_use]
    pub fn view_pyramid_bytes(&self, view: u32) -> u64 {
        self.lock().pyramid_bytes(view)
    }

    /// Whether `view` has a ring (it culled, cleared or was published at least once and has not
    /// gone away).
    #[must_use]
    pub fn has_view(&self, view: u32) -> bool {
        self.lock().ring(view).is_some()
    }

    /// Whether a copied frame is waiting for its map (a non-blocking device poll lets it land).
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.lock().rings.iter().any(|ring| {
            ring.streams.iter().any(|stream| {
                stream
                    .slots
                    .iter()
                    .any(|slot| slot.state.load(Ordering::Acquire) == ST_INFLIGHT)
            })
        })
    }

    /// Takes every slot whose map completed into its view's result, newest recording last (an
    /// older one arriving after a newer result is thrown away), and frees the slots. Never
    /// waits. Returns whether a result changed.
    pub fn collect(&self) -> bool {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut changed = false;
        for ring in &mut inner.rings {
            let mut ring_changed = false;
            for (kind, stream) in ring.streams.iter_mut().enumerate() {
                ring_changed |= collect_stream(stream, kind, &mut ring.stats, &mut inner.drawn);
            }
            if ring_changed {
                ring.sync_result();
            }
            changed |= ring_changed;
        }
        changed
    }

    /// Copies the first view's (view 0) result (sorted, unique object keys) into `out`, reusing
    /// its capacity. Empty without a result.
    pub fn occluded_into(&self, out: &mut Vec<u64>) {
        self.occluded_view_into(0, out);
    }

    /// Copies `view`'s result (sorted, unique object keys: the forward prepass's and the
    /// clusters') into `out`, reusing its capacity. Empty when the view has no result.
    pub fn occluded_view_into(&self, view: u32, out: &mut Vec<u64>) {
        let inner = self.lock();
        out.clear();
        if let Some(ring) = inner.ring(view) {
            for stream in &ring.streams {
                out.extend_from_slice(&stream.occluded);
            }
            if !ring.streams[CLUSTERS].occluded.is_empty() {
                out.sort_unstable();
                out.dedup();
            }
        }
    }

    /// Whether the object key `key` (`entity.to_bits() ^ object_key_xor`, the renderer's
    /// [`crate::FrameMesh::object_key`]) is in the first view's (view 0) result.
    #[must_use]
    pub fn is_occluded(&self, key: u64) -> bool {
        self.is_occluded_in(0, key)
    }

    /// Whether the object key `key` is in `view`'s result.
    #[must_use]
    pub fn is_occluded_in(&self, view: u32, key: u64) -> bool {
        self.lock().ring(view).is_some_and(|ring| ring.holds(key))
    }

    /// Replaces the first view's (view 0) result with `occluded` object keys from a host's own
    /// occlusion test. It counts as the newest result: a GPU readback recorded before it is
    /// thrown away.
    pub fn publish(&self, occluded: &[u64]) {
        self.publish_view(0, occluded);
    }

    /// [`Self::publish`] for `view` (its index in `ExtractedFrame::views`).
    pub fn publish_view(&self, view: u32, occluded: &[u64]) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let index = inner.ring_index(view);
        let ring = &mut inner.rings[index];
        let [forward, clusters] = &mut ring.streams;
        forward.occluded.clear();
        forward.occluded.extend_from_slice(occluded);
        forward.occluded.sort_unstable();
        forward.occluded.dedup();
        forward.result_seq = seq;
        clusters.occluded.clear();
        clusters.result_seq = seq;
        ring.stats.published += 1;
        ring.sync_result();
    }

    /// Drops the rings of views at index `views` and above (the frame has fewer views now), so
    /// a stale result never speaks for another camera. Their counts stay in [`Self::stats`].
    /// Allocates nothing.
    pub fn retain_views(&self, views: usize) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        // The renderer frees those views' pyramids too (`FrameRenderer::retain_occlusion_views`).
        inner.pyramids.retain(|&(view, _)| (view as usize) < views);
        let keep = inner
            .rings
            .iter()
            .position(|ring| ring.view as usize >= views)
            .unwrap_or(inner.rings.len());
        if keep == inner.rings.len() {
            return;
        }
        for ring in &inner.rings[keep..] {
            let mut counts = ring.stats;
            counts.occluded = 0;
            counts.pyramid_bytes = 0;
            add_stats(&mut inner.retired, &counts);
        }
        for pending in &mut inner.pending {
            if pending.is_some_and(|(ring, _)| ring >= keep) {
                *pending = None;
            }
        }
        inner.rings.truncate(keep);
    }

    /// Drops the pyramid reports of views at index `views` and above (the renderer freed them).
    pub(crate) fn retain_pyramids(&self, views: usize) {
        self.lock().pyramids.retain(|&(view, _)| (view as usize) < views);
    }

    /// A frame of `view` that did not cull its prepass in two phases: that part of its result is
    /// cleared (as the newest).
    pub(crate) fn note_no_cull(&self, view: u32) {
        self.lock().clear_stream(view, FORWARD);
    }

    /// A frame of `view` that drew no visibility-buffer clusters: that part of its result is
    /// cleared (as the newest).
    pub(crate) fn note_no_clusters(&self, view: u32) {
        self.lock().clear_stream(view, CLUSTERS);
    }

    /// Copies this frame's phase commands (`count` instances; `phase2` when it ran) of `view`
    /// into an idle slot of its ring with `keys` (per instance: object key, bound inside the
    /// frustum). Mapped by [`Self::map_after_submit`]. With no idle slot the frame is not copied
    /// (counted).
    pub(crate) fn record(
        &self,
        device: &crate::TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: u32,
        (phase1, phase2): (&wgpu::Buffer, Option<&wgpu::Buffer>),
        keys: &[(u64, bool)],
    ) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.follow_device(device);
        let count = keys.len() as u64;
        if count == 0 {
            return;
        }
        let phase_bytes = count * CMD_STRIDE;
        if phase1.size() < phase_bytes || phase2.is_some_and(|b| b.size() < phase_bytes) {
            return;
        }
        let filled = if phase2.is_some() {
            phase_bytes * 2
        } else {
            phase_bytes
        };
        let Some(at) = inner.take_slot(device, view, FORWARD, (filled, phase_bytes * 2)) else {
            return;
        };
        let slot = &mut inner.rings[at.0].streams[FORWARD].slots[at.1];
        let Some(staging) = slot.staging.as_ref() else {
            return;
        };
        encoder.copy_buffer_to_buffer(phase1, 0, staging, 0, phase_bytes);
        if let Some(phase2) = phase2 {
            encoder.copy_buffer_to_buffer(phase2, 0, staging, phase_bytes, phase_bytes);
        }
        slot.phase2 = phase2.is_some();
        slot.keys.clear();
        slot.keys.extend_from_slice(keys);
        inner.commit_slot(FORWARD, at, filled);
    }

    /// Copies this frame's cluster visibility bits (`bits`, the first `words` 32-bit words) of
    /// `view` into an idle slot of its cluster ring, with `ranges` (per instance: object key,
    /// first bit, bit count). Mapped by [`Self::map_after_submit`] (ADR-7286).
    pub(crate) fn record_clusters(
        &self,
        device: &crate::TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: u32,
        (bits, words): (&wgpu::Buffer, u64),
        ranges: &[(u64, u32, u32)],
    ) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.follow_device(device);
        let bytes = words * 4;
        if ranges.is_empty() || bytes == 0 || bits.size() < bytes {
            return;
        }
        let Some(at) = inner.take_slot(device, view, CLUSTERS, (bytes, bytes)) else {
            return;
        };
        let slot = &mut inner.rings[at.0].streams[CLUSTERS].slots[at.1];
        let Some(staging) = slot.staging.as_ref() else {
            return;
        };
        encoder.copy_buffer_to_buffer(bits, 0, staging, 0, bytes);
        slot.ranges.clear();
        slot.ranges.extend_from_slice(ranges);
        inner.commit_slot(CLUSTERS, at, bytes);
    }

    /// Maps the slots this frame copied into, after the submit that holds the copies.
    pub(crate) fn map_after_submit(&self) {
        let mut inner = self.lock();
        for stream in [FORWARD, CLUSTERS] {
            let Some((ring, index)) = inner.pending[stream].take() else {
                continue;
            };
            let Some(slot) = inner
                .rings
                .get(ring)
                .and_then(|ring| ring.streams[stream].slots.get(index))
            else {
                continue;
            };
            let Some(staging) = slot.staging.as_ref() else {
                continue;
            };
            let state = Arc::clone(&slot.state);
            state.store(ST_INFLIGHT, Ordering::Release);
            crate::frame_stats::note_map_async();
            staging
                .slice(..slot.filled)
                .map_async(wgpu::MapMode::Read, move |result| {
                    crate::readback::store_map_result(&state, result.is_ok());
                });
        }
    }
}

/// Takes `stream`'s completed slots into its result, oldest recording first so the newest wins
/// (`kind`: [`FORWARD`] or [`CLUSTERS`]; `drawn_keys` is scratch). Returns whether the result
/// changed.
fn collect_stream(
    stream: &mut Stream,
    kind: usize,
    stats: &mut OcclusionFeedbackStats,
    drawn_keys: &mut Vec<u64>,
) -> bool {
    let mut changed = false;
    loop {
        let next = stream
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                let state = slot.state.load(Ordering::Acquire);
                if state == ST_FAILED {
                    slot.state.store(ST_IDLE, Ordering::Release);
                }
                state == ST_READY
            })
            .min_by_key(|(_, slot)| slot.seq)
            .map(|(index, _)| index);
        let Some(index) = next else {
            break;
        };
        let slot = &stream.slots[index];
        if slot.seq <= stream.result_seq {
            stats.stale += 1;
        } else if let Some(staging) = slot.staging.as_ref() {
            let read = match staging.slice(..slot.filled).get_mapped_range() {
                Ok(mapped) => {
                    stream.occluded.clear();
                    drawn_keys.clear();
                    if kind == FORWARD {
                        let count = slot.keys.len();
                        let (commands, _) = mapped.as_chunks::<{ CMD_STRIDE as usize }>();
                        let drawn = |i: usize| {
                            commands
                                .get(i)
                                .is_some_and(|c| u32::from_le_bytes([c[4], c[5], c[6], c[7]]) != 0)
                        };
                        for (i, &(key, in_frustum)) in slot.keys.iter().enumerate() {
                            if drawn(i) || (slot.phase2 && drawn(count + i)) {
                                drawn_keys.push(key);
                            } else if in_frustum {
                                stream.occluded.push(key);
                            }
                        }
                    } else {
                        let (words, _) = mapped.as_chunks::<4>();
                        for &(key, base, count) in &slot.ranges {
                            if any_bit(words, base, count) {
                                drawn_keys.push(key);
                            } else {
                                stream.occluded.push(key);
                            }
                        }
                    }
                    true
                }
                Err(err) => {
                    tracing::debug!(target: "regia_render", "occlusion feedback map: {err}");
                    false
                }
            };
            if read {
                stream.occluded.sort_unstable();
                stream.occluded.dedup();
                drawn_keys.sort_unstable();
                let drawn = &*drawn_keys;
                stream.occluded.retain(|key| drawn.binary_search(key).is_err());
                stream.result_seq = slot.seq;
                stats.read += 1;
                changed = true;
            }
        }
        if let Some(staging) = stream.slots[index].staging.as_ref() {
            staging.unmap();
        }
        stream.slots[index].state.store(ST_IDLE, Ordering::Release);
    }
    changed
}

/// `Render`, before the views render: hands the world's [`OcclusionFeedback`] to the runtime
/// renderer once (and again after the renderer or the handle was replaced).
pub fn connect_occlusion_feedback(
    feedback: Option<Res<OcclusionFeedback>>,
    runtime: Option<ResMut<crate::RuntimeRenderer>>,
) {
    let (Some(feedback), Some(mut runtime)) = (feedback, runtime) else {
        return;
    };
    let connected = runtime
        .0
        .occlusion_feedback()
        .is_some_and(|held| held.same_as(&feedback));
    if !connected {
        runtime
            .bypass_change_detection()
            .0
            .set_occlusion_feedback(Some(OcclusionFeedback::clone(&feedback)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_result_is_sorted_unique_and_the_newest() {
        let feedback = OcclusionFeedback::default();
        feedback.publish(&[9, 3, 9, 1]);
        let mut out = Vec::new();
        feedback.occluded_into(&mut out);
        assert_eq!(out, [1, 3, 9]);
        assert!(feedback.is_occluded(3) && !feedback.is_occluded(4));
        let stats = feedback.stats();
        assert_eq!((stats.published, stats.occluded), (1, 3));
        // A frame without the cull clears it; a second one counts nothing more.
        feedback.note_no_cull(0);
        feedback.note_no_cull(0);
        feedback.occluded_into(&mut out);
        assert!(out.is_empty());
        assert_eq!(feedback.stats().cleared, 1);
        assert!(!feedback.collect(), "nothing mapped, nothing read");
        assert!(!feedback.in_flight());
    }

    #[test]
    fn each_view_keeps_its_own_result() {
        let feedback = OcclusionFeedback::default();
        feedback.publish_view(1, &[7, 2]);
        feedback.publish(&[4]);
        assert!(feedback.is_occluded_in(1, 7) && !feedback.is_occluded_in(0, 7));
        assert!(feedback.is_occluded(4) && !feedback.is_occluded_in(1, 4));
        assert!(
            !feedback.is_occluded_in(2, 4),
            "a view without a ring holds nothing"
        );
        let mut out = Vec::new();
        feedback.occluded_view_into(1, &mut out);
        assert_eq!(out, [2, 7]);
        let mut views = Vec::new();
        feedback.view_stats_into(&mut views);
        assert_eq!(
            views
                .iter()
                .map(|v| (v.view, v.stats.occluded))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 2)],
            "ascending by view"
        );
        let total = feedback.stats();
        assert_eq!((total.published, total.occluded), (2, 3), "summed over the views");
        // A frame of view 1 without the cull clears view 1 only.
        feedback.note_no_cull(1);
        assert!(!feedback.is_occluded_in(1, 7) && feedback.is_occluded(4));
        // A view with no ring and no cull makes none.
        feedback.note_no_cull(5);
        assert!(!feedback.has_view(5));
        // The frame has one view now: view 1's ring goes, its counts stay in the totals.
        feedback.retain_views(1);
        assert!(feedback.has_view(0) && !feedback.has_view(1));
        let after = feedback.stats();
        assert_eq!((after.published, after.cleared, after.occluded), (2, 1, 1));
        feedback.retain_views(0);
        assert!(!feedback.is_occluded(4));
        assert_eq!(feedback.stats().occluded, 0);
    }

    #[test]
    fn a_bit_range_reads_drawn_when_any_bit_is_set() {
        let words = [0u32, 1 << 31, 0, 0b100].map(u32::to_le_bytes);
        assert!(!any_bit(&words, 0, 32), "word 0 is clear");
        assert!(any_bit(&words, 63, 1), "the last bit of word 1");
        assert!(!any_bit(&words, 32, 31), "word 1 below its last bit");
        assert!(any_bit(&words, 40, 60), "spans word 1's last bit");
        assert!(!any_bit(&words, 64, 32), "word 2 is clear");
        assert!(any_bit(&words, 66, 33), "word 3 bit 2 at the range's end");
        assert!(!any_bit(&words, 96, 2), "word 3 bits 0 and 1");
        assert!(!any_bit(&words, 5, 0), "an empty range drew nothing");
        assert!(any_bit(&words, 120, 16), "past the copied words reads as drawn");
    }

    #[test]
    fn clones_share_the_state() {
        let a = OcclusionFeedback::default();
        let b = a.clone();
        assert!(a.same_as(&b));
        assert!(!a.same_as(&OcclusionFeedback::default()));
        b.publish(&[5]);
        assert!(a.is_occluded(5));
    }
}
