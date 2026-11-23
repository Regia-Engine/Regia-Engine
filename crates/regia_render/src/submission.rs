// SPDX-License-Identifier: MPL-2.0
//! Render-submission trims (ADR-508): binds a pass already holds, culled dead work, and the
//! per-frame counters that show both.
//!
//! wgpu 30 drops a `set_pipeline` or `set_bind_group` that repeats the bound object while a pass
//! is recorded, but it records every `set_vertex_buffer` and `set_index_buffer`. Each one looks
//! its buffer up when it is recorded, and `CommandEncoder::finish` replays it: a usage merge into
//! the pass scope, an initialization check and the backend's bind call. Cached meshes are ranges
//! in shared arena chunks (`mesh_storage`), drawn with `base_vertex` and `first_index`, so
//! consecutive draws of different meshes mostly bind the same two buffers. `MeshBinds`
//! remembers what the pass holds and records a bind only when it changes. The draws, their
//! order and every other command are unchanged, so the GPU does the same work.
//!
//! The frame also culls translucent-tail work nothing consumes: the display-referred
//! translucent layers pass when every layer already drew into the HDR target, and the decal
//! receiver's per-draw stamps when no layer has a decal.
//!
//! [`SubmissionStats`] counts the binds recorded and skipped, the passes culled and the stamps
//! built in the last main-view frame. [`crate::FrameRenderer::set_submission_trims`] with
//! `false` (or `REGIA_SUBMIT_TRIMS=0` at start) records every bind and keeps the dead work, for
//! an A/B measurement of the same build.

/// Vertex buffer slots a pass tracks (0: positions and attributes, 1: the second UV stream or
/// a compute-skinned twin's previous pose). A bind to a higher slot is always recorded.
const VERTEX_SLOTS: usize = 2;

/// Counters of the last main-view frame's render submission ([`crate::FrameRenderer::submission_stats`]).
///
/// With the trims on, `vertex_binds + vertex_binds_skipped` is what the frame recorded before
/// ADR-508 (likewise for index binds). With them off, nothing is skipped or culled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmissionStats {
    /// The trims were on for the frame ([`crate::FrameRenderer::set_submission_trims`]).
    pub trims: bool,
    /// `set_vertex_buffer` calls the frame's mesh passes recorded.
    pub vertex_binds: u32,
    /// Vertex buffer binds skipped because the pass already held the buffer.
    pub vertex_binds_skipped: u32,
    /// `set_index_buffer` calls the frame's mesh passes recorded.
    pub index_binds: u32,
    /// Index buffer binds skipped because the pass already held the buffer.
    pub index_binds_skipped: u32,
    /// Render passes the frame did not begin because they would have drawn nothing.
    pub passes_culled: u32,
    /// Decal receiver stamps built (one per visible opaque draw when a layer has decals).
    pub receiver_stamps: u32,
    /// Visible opaque draws not stamped because no translucent layer had a decal.
    pub receiver_stamps_skipped: u32,
    /// Main-view frames counted since the renderer was created.
    pub frames: u64,
}

/// Bind counts of one pass or one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BindCounts {
    pub(crate) vertex: u32,
    pub(crate) vertex_skipped: u32,
    pub(crate) index: u32,
    pub(crate) index_skipped: u32,
}

impl BindCounts {
    /// Adds `other` (another pass of the frame).
    pub(crate) fn add(&mut self, other: Self) {
        self.vertex = self.vertex.saturating_add(other.vertex);
        self.vertex_skipped = self.vertex_skipped.saturating_add(other.vertex_skipped);
        self.index = self.index.saturating_add(other.index);
        self.index_skipped = self.index_skipped.saturating_add(other.index_skipped);
    }
}

/// One binding point of a pass: the object bound last (`None` before the first bind).
#[derive(Debug)]
struct Slot<K> {
    bound: Option<K>,
}

impl<K> Default for Slot<K> {
    fn default() -> Self {
        Self { bound: None }
    }
}

impl<K: PartialEq + Clone> Slot<K> {
    /// Whether `key` must be recorded: it is not the bound object, or `trims` is off. Either
    /// way `key` is the bound object afterwards.
    fn want(&mut self, key: &K, trims: bool) -> bool {
        let same = self.bound.as_ref() == Some(key);
        if !same {
            self.bound = Some(key.clone());
        }
        !(same && trims)
    }
}

/// The vertex and index buffers one render pass holds, so a draw rebinds only what changed.
///
/// Make one per pass (bindings do not carry across passes) and call [`Self::forget`] after
/// code outside the tracker bound buffers in the same pass (the shared draw, the visibility
/// buffer). Pipeline changes keep the bindings (WebGPU vertex and index buffer state belongs
/// to the pass, not the pipeline).
#[derive(Debug)]
pub(crate) struct MeshBinds<K = wgpu::Buffer> {
    trims: bool,
    vertex: [Slot<K>; VERTEX_SLOTS],
    index: Slot<K>,
    counts: BindCounts,
}

impl<K: PartialEq + Clone> MeshBinds<K> {
    /// An empty pass. `trims` false records every bind (the A/B baseline) but still counts.
    pub(crate) fn new(trims: bool) -> Self {
        Self {
            trims,
            vertex: std::array::from_fn(|_| Slot::default()),
            index: Slot::default(),
            counts: BindCounts::default(),
        }
    }

    /// Whether binding `key` to vertex slot `slot` must be recorded (counted either way).
    pub(crate) fn want_vertex(&mut self, slot: u32, key: &K) -> bool {
        let want = match self.vertex.get_mut(slot as usize) {
            Some(tracked) => tracked.want(key, self.trims),
            None => true,
        };
        if want {
            self.counts.vertex = self.counts.vertex.saturating_add(1);
        } else {
            self.counts.vertex_skipped = self.counts.vertex_skipped.saturating_add(1);
        }
        want
    }

    /// Whether binding index buffer `key` must be recorded (counted either way).
    pub(crate) fn want_index(&mut self, key: &K) -> bool {
        let want = self.index.want(key, self.trims);
        if want {
            self.counts.index = self.counts.index.saturating_add(1);
        } else {
            self.counts.index_skipped = self.counts.index_skipped.saturating_add(1);
        }
        want
    }

    /// Something else bound buffers in this pass: the next bind of every slot is recorded.
    pub(crate) fn forget(&mut self) {
        for slot in &mut self.vertex {
            slot.bound = None;
        }
        self.index.bound = None;
    }

    /// Binds recorded and skipped so far.
    pub(crate) fn counts(&self) -> BindCounts {
        self.counts
    }
}

/// The tracker a frame's render pass uses (buffers compared by identity).
pub(crate) type PassBinds = MeshBinds<wgpu::Buffer>;

impl MeshBinds<wgpu::Buffer> {
    /// Binds all of `buffer` to vertex slot `slot` unless the pass already holds it there.
    pub(crate) fn vertex(&mut self, pass: &mut wgpu::RenderPass<'_>, slot: u32, buffer: &wgpu::Buffer) {
        if self.want_vertex(slot, buffer) {
            pass.set_vertex_buffer(slot, buffer.slice(..));
        }
    }

    /// Binds all of `buffer` as the `u32` index buffer unless the pass already holds it.
    pub(crate) fn index(&mut self, pass: &mut wgpu::RenderPass<'_>, buffer: &wgpu::Buffer) {
        if self.want_index(buffer) {
            pass.set_index_buffer(buffer.slice(..), wgpu::IndexFormat::Uint32);
        }
    }

    /// A cached mesh's vertex (slot 0) and index buffers.
    pub(crate) fn mesh(&mut self, pass: &mut wgpu::RenderPass<'_>, vb: &wgpu::Buffer, ib: &wgpu::Buffer) {
        self.vertex(pass, 0, vb);
        self.index(pass, ib);
    }
}

/// The renderer's trims switch and the last main-view frame's counters.
#[derive(Debug, Default)]
pub(crate) struct SubmissionState {
    /// `None` until the first frame reads `REGIA_SUBMIT_TRIMS` (default on).
    enabled: Option<bool>,
    last: SubmissionStats,
    frames: u64,
}

impl SubmissionState {
    /// Whether the trims are on (resolved from the environment on first use).
    pub(crate) fn enabled(&mut self) -> bool {
        *self.enabled.get_or_insert_with(env_default)
    }

    /// Turns the trims on or off from the next frame on.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = Some(enabled);
    }

    /// The switch as last set, or what the environment asks for before the first frame.
    pub(crate) fn current(&self) -> bool {
        self.enabled.unwrap_or_else(env_default)
    }

    /// Keeps `other`'s switch (a renderer rebuilt for a shader hot swap).
    pub(crate) fn carry(&mut self, other: &Self) {
        self.enabled = other.enabled;
        self.frames = other.frames;
        self.last = other.last;
    }

    /// Publishes one main-view frame's counters.
    pub(crate) fn publish(&mut self, mut frame: SubmissionStats) {
        self.frames = self.frames.saturating_add(1);
        frame.frames = self.frames;
        self.last = frame;
    }

    /// The last published frame's counters.
    pub(crate) fn last(&self) -> SubmissionStats {
        self.last
    }
}

/// The switch before any `set_enabled`: `REGIA_SUBMIT_TRIMS`, on when unset.
fn env_default() -> bool {
    regia_core::brand::env_var("REGIA_SUBMIT_TRIMS")
        .ok()
        .is_none_or(|value| trims_from_env(&value))
}

/// `REGIA_SUBMIT_TRIMS`: `0`, `false`, `off` or `no` turn the trims off; anything else keeps
/// them on.
fn trims_from_env(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_buffer_is_bound_once() {
        let mut binds: MeshBinds<u32> = MeshBinds::new(true);
        // Three meshes in arena chunk 7, then one in chunk 9, then chunk 7 again.
        let chunks = [7, 7, 7, 9, 7];
        let recorded: Vec<bool> = chunks.iter().map(|chunk| binds.want_vertex(0, chunk)).collect();
        assert_eq!(recorded, [true, false, false, true, true]);
        let counts = binds.counts();
        assert_eq!((counts.vertex, counts.vertex_skipped), (3, 2));
        assert_eq!(counts.index + counts.index_skipped, 0);
    }

    #[test]
    fn slots_and_the_index_buffer_are_tracked_apart() {
        let mut binds: MeshBinds<u32> = MeshBinds::new(true);
        assert!(binds.want_vertex(0, &1));
        // The same buffer in another slot is another binding.
        assert!(binds.want_vertex(1, &1));
        assert!(!binds.want_vertex(1, &1));
        assert!(binds.want_index(&1));
        assert!(!binds.want_index(&1));
        assert!(!binds.want_vertex(0, &1));
        let counts = binds.counts();
        assert_eq!(
            counts,
            BindCounts {
                vertex: 2,
                vertex_skipped: 2,
                index: 1,
                index_skipped: 1,
            }
        );
    }

    #[test]
    fn slots_past_the_tracked_ones_always_bind() {
        let mut binds: MeshBinds<u32> = MeshBinds::new(true);
        let slot = VERTEX_SLOTS as u32;
        assert!(binds.want_vertex(slot, &3));
        assert!(binds.want_vertex(slot, &3));
        assert_eq!(binds.counts().vertex, 2);
    }

    #[test]
    fn forget_rebinds_every_slot() {
        let mut binds: MeshBinds<u32> = MeshBinds::new(true);
        assert!(binds.want_vertex(0, &4));
        assert!(binds.want_vertex(1, &5));
        assert!(binds.want_index(&6));
        // A shared draw bound its own buffers in between.
        binds.forget();
        assert!(binds.want_vertex(0, &4));
        assert!(binds.want_vertex(1, &5));
        assert!(binds.want_index(&6));
        assert_eq!(binds.counts().vertex_skipped + binds.counts().index_skipped, 0);
    }

    #[test]
    fn trims_off_records_every_bind_and_counts_the_same_total() {
        let keys = [1u32, 1, 2, 2, 2, 1];
        let mut on: MeshBinds<u32> = MeshBinds::new(true);
        let mut off: MeshBinds<u32> = MeshBinds::new(false);
        for key in keys {
            on.want_vertex(0, &key);
            on.want_index(&key);
            assert!(off.want_vertex(0, &key));
            assert!(off.want_index(&key));
        }
        let (on, off) = (on.counts(), off.counts());
        assert_eq!(off.vertex, keys.len() as u32);
        assert_eq!(off.vertex_skipped, 0);
        assert_eq!(on.vertex, 3);
        assert_eq!(
            on.vertex + on.vertex_skipped,
            off.vertex,
            "the baseline is the sum"
        );
        assert_eq!(on.index + on.index_skipped, off.index);
    }

    #[test]
    fn frame_counts_add_passes() {
        let mut frame = BindCounts::default();
        frame.add(BindCounts {
            vertex: 2,
            vertex_skipped: 5,
            index: 1,
            index_skipped: 6,
        });
        frame.add(BindCounts {
            vertex: 1,
            vertex_skipped: 0,
            index: 1,
            index_skipped: 0,
        });
        assert_eq!(
            frame,
            BindCounts {
                vertex: 3,
                vertex_skipped: 5,
                index: 2,
                index_skipped: 6,
            }
        );
    }

    #[test]
    fn the_state_numbers_frames_and_keeps_its_switch() {
        let mut state = SubmissionState::default();
        state.set_enabled(false);
        assert!(!state.enabled());
        state.publish(SubmissionStats {
            vertex_binds: 4,
            ..SubmissionStats::default()
        });
        state.publish(SubmissionStats {
            vertex_binds: 9,
            ..SubmissionStats::default()
        });
        assert_eq!(state.last().vertex_binds, 9);
        assert_eq!(state.last().frames, 2);
        let mut fresh = SubmissionState::default();
        fresh.carry(&state);
        assert!(!fresh.current());
        assert_eq!(fresh.last().frames, 2);
    }

    #[test]
    fn the_environment_switch_reads_common_spellings() {
        for off in ["0", "false", "OFF", " no "] {
            assert!(!trims_from_env(off), "{off:?} turns the trims off");
        }
        for on in ["1", "true", "on", ""] {
            assert!(trims_from_env(on), "{on:?} keeps the trims on");
        }
    }
}
