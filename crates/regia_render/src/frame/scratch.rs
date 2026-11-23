// SPDX-License-Identifier: MPL-2.0
//! Per-frame scratch buffers of [`super::FrameRenderer::render_into`] (ADR-385).
//!
//! The frame builds its light, shadow-caster, culling and object lists from scratch every
//! frame. They used to be fresh `Vec`s and hash maps, so a steady frame paid one heap
//! allocation per list (and, for the culler's object upload, three per frame of the size of
//! the instance count). Each list now lives in a [`Slot`] between frames: the frame takes it
//! out emptied, fills it, and gives it back at the end. A list that needs more room than it
//! held reallocates once; the slot counts that as a growth. Once the scene and the view
//! settle, every list fits and the growth count stops rising, which a test or a telemetry
//! window can assert (`FrameRenderer::frame_scratch_stats`).
//!
//! A frame that returns early (a cancelled warm-up) drops what it took; the next frame starts
//! that list empty and counts its growth again. That is correct: it allocated.

use crate::key_hash::KeyMap;

/// A caster's light-space footprint (shadow-map UV min and max) in one cascade.
pub(super) type Footprint = Option<([f32; 2], [f32; 2])>;

/// Growths of the frame renderer's reusable per-frame buffers (ADR-385).
///
/// [`super::FrameRenderer::render_into`] keeps its light, shadow-caster, culling and object
/// lists between frames. A list that needs more room than it held reallocates: that is one
/// growth. Once the scene and the view settle, every list fits and [`Self::growths`] stops
/// rising. The draw list is one of the counted slots. wgpu's command recording is not
/// (the book page `agents/frame-scratch.md` lists both).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameScratchStats {
    /// Growths since the renderer was created (main view and texture views together).
    pub growths: u64,
    /// Growths during the last frame that reached its end.
    pub last_frame_growths: u64,
    /// Frames that reached their end and returned their buffers.
    pub frames: u64,
}

/// A buffer a [`Slot`] can keep between frames: emptied without giving its room back.
pub(crate) trait Reuse: Default {
    /// Removes every element and keeps the allocation.
    fn reset(&mut self);
    /// Elements the buffer holds without reallocating.
    fn room(&self) -> usize;
}

impl<T> Reuse for Vec<T> {
    fn reset(&mut self) {
        self.clear();
    }

    fn room(&self) -> usize {
        self.capacity()
    }
}

impl<K, V, S: Default> Reuse for std::collections::HashMap<K, V, S> {
    fn reset(&mut self) {
        self.clear();
    }

    fn room(&self) -> usize {
        self.capacity()
    }
}

/// One reusable list and the times it had to grow.
#[derive(Default)]
pub(crate) struct Slot<B> {
    buf: B,
    /// Room of the buffer when it was taken out.
    room: usize,
    growths: u64,
}

impl<B: Reuse> Slot<B> {
    /// The buffer, emptied, with the room it had. Hand it back with [`Self::give`].
    pub(crate) fn take(&mut self) -> B {
        let mut buf = std::mem::take(&mut self.buf);
        buf.reset();
        self.room = buf.room();
        buf
    }

    /// Keeps `buf` for the next frame. Counts a growth when it holds more room than it was
    /// taken out with (it reallocated, or a dropped buffer was replaced by a new one).
    pub(crate) fn give(&mut self, buf: B) {
        if buf.room() > self.room {
            self.growths += 1;
        }
        self.room = buf.room();
        self.buf = buf;
    }

    /// Growths since the slot was created.
    pub(crate) fn growths(&self) -> u64 {
        self.growths
    }
}

impl<T> Slot<Vec<T>> {
    /// Allocation of the stored buffer. An empty buffer's pointer is dangling.
    #[cfg(test)]
    pub(crate) fn vec_ptr(&self) -> *const T {
        self.buf.as_ptr()
    }

    /// Capacity of the stored buffer.
    #[cfg(test)]
    pub(crate) fn vec_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl<K, V, S> Slot<std::collections::HashMap<K, V, S>> {
    /// Capacity of the stored map. An empty map's capacity is 0 until the first insert.
    #[cfg(test)]
    pub(crate) fn map_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

/// One list per cascade whose inner buffers survive a reset.
///
/// `Default` does not require `T: Default`: the lists start empty, and an inner buffer is
/// cleared in place rather than rebuilt.
pub(crate) struct ListOfLists<T> {
    lists: Vec<Vec<T>>,
    len: usize,
}

impl<T> Default for ListOfLists<T> {
    fn default() -> Self {
        Self {
            lists: Vec::new(),
            len: 0,
        }
    }
}

impl<T> ListOfLists<T> {
    /// Appends an empty list (reusing an earlier frame's buffer) and returns it.
    pub(crate) fn push_list(&mut self) -> &mut Vec<T> {
        if self.len == self.lists.len() {
            self.lists.push(Vec::new());
        }
        let list = &mut self.lists[self.len];
        list.clear();
        self.len += 1;
        list
    }

    /// Appends `count` empty lists at once and returns them, to be filled in parallel.
    pub(crate) fn push_lists(&mut self, count: usize) -> &mut [Vec<T>] {
        let first = self.len;
        for _ in 0..count {
            self.push_list();
        }
        &mut self.lists[first..self.len]
    }

    /// The lists pushed since the last reset.
    pub(crate) fn lists(&self) -> &[Vec<T>] {
        &self.lists[..self.len]
    }
}

impl<T> Reuse for ListOfLists<T> {
    fn reset(&mut self) {
        self.len = 0;
    }

    fn room(&self) -> usize {
        self.lists.capacity() + self.lists.iter().map(Vec::capacity).sum::<usize>()
    }
}

/// `(joint base, model, folded)` of one skinned mesh's joint span.
pub(super) type SkinSpan = (u32, [[f32; 4]; 4], bool);

/// A velocity-pass sort key: `(mesh, skinned, cull none, order, draw)`.
pub(super) type VelocityKey = (u64, bool, bool, u32, usize);

/// The frame's reusable lists (one [`Slot`] each), in the order `render_into` builds them.
#[derive(Default)]
pub(super) struct FrameScratch {
    /// The local lights uploaded this frame (`uploaded`).
    pub(super) lights: Slot<Vec<crate::LabLocalLight>>,
    /// The light buffer's records (`MAX_UPLOADED_LIGHTS` entries).
    pub(super) gpu_lights: Slot<Vec<super::GpuLight>>,
    /// The shadow tile records (`MAX_SHADOW_TILES` entries).
    pub(super) shadow_tiles: Slot<Vec<super::ShadowTileGpu>>,
    /// The GI probes in shader order, padded to `MAX_PROBES`.
    pub(super) gi_probes: Slot<Vec<crate::gi::GpuProbe>>,
    /// The frame's joint palette. It becomes the previous palette at the end of the frame;
    /// the palette it replaces comes back here.
    pub(super) joints: Slot<Vec<crate::skin::JointMatrix>>,
    /// `(joint base, model, folded)` per non-empty mesh.
    pub(super) spans: Slot<Vec<SkinSpan>>,
    /// Glass instance index by object key.
    pub(super) glass_index: Slot<KeyMap<u64, usize>>,
    /// Object uniforms that changed this frame.
    pub(super) object_uploads: Slot<Vec<(wgpu::Buffer, super::ObjectData)>>,
    /// Draw indices: visible opaque, visible blended, shadow casters, translucent casters.
    pub(super) opaque_ix: Slot<Vec<usize>>,
    pub(super) blend_ix: Slot<Vec<usize>>,
    pub(super) shadow_ix: Slot<Vec<usize>>,
    pub(super) trans_ix: Slot<Vec<usize>>,
    /// Sun cascade split distances.
    pub(super) csm_splits: Slot<Vec<f32>>,
    /// `(world aabb, cascade mask)` per opaque and per translucent caster.
    pub(super) caster_info: Slot<Vec<(Option<crate::paged_shadow::Aabb3>, u8)>>,
    pub(super) trans_info: Slot<Vec<(Option<crate::paged_shadow::Aabb3>, u8)>>,
    /// Each caster's footprint per cascade.
    pub(super) cascade_uv: Slot<ListOfLists<Footprint>>,
    /// Bounded casters for the page and atlas caches, and their `caster_info` index.
    pub(super) casters: Slot<Vec<crate::paged_shadow::ShadowCaster>>,
    pub(super) caster_slot: Slot<Vec<usize>>,
    /// The frame's shadow jobs.
    pub(super) shadow_jobs: Slot<Vec<super::ShadowJob>>,
    /// Rigid opaque draws submitted to the GPU cull.
    pub(super) cull_meshes: Slot<Vec<crate::gpu_driven::CullMesh>>,
    /// Indirect command index by object key, after the cull upload.
    pub(super) cull_slots: Slot<KeyMap<u64, u32>>,
    /// First draw index by object key, for the storage-object upload.
    pub(super) object_draw: Slot<KeyMap<u64, usize>>,
    /// The storage objects' bytes (`GPU_OBJECT_STRIDE` per culled draw).
    pub(super) object_bytes: Slot<Vec<u8>>,
    /// The frame's draws. Taken by `build_draw_list` and given back from `finish_frame`.
    pub(super) draws: Slot<Vec<super::Draw>>,
    /// Stream-pass order: `(material key, mesh index, priority)`. The mesh index names
    /// `scene.meshes`, so the slot does not borrow a material across the frame.
    pub(super) stream_order: Slot<Vec<(u64, u32, f32)>>,
    /// First mesh index of each material key in this frame's stream pass.
    pub(super) stream_in_scene: Slot<KeyMap<u64, u32>>,
    /// The velocity pass: `(mesh, skinned, cull none, order, draw)` sort keys, instances, the
    /// skinned palettes (current and previous) and the batched draws (emptied before they are
    /// kept, so no buffer handle outlives the frame).
    pub(super) velocity_keyed: Slot<Vec<VelocityKey>>,
    pub(super) velocity_instances: Slot<Vec<crate::taau::VelocityInstance>>,
    pub(super) velocity_joints: Slot<Vec<crate::skin::JointMatrix>>,
    pub(super) velocity_draws: Slot<Vec<crate::taau::VelocityDraw>>,
    /// Growth total at the start of the current frame.
    at_frame_start: u64,
    last_frame_growths: u64,
    frames: u64,
}

impl FrameScratch {
    /// Allocation retained by the draw-list slot, without counting GPU buffers its handles reference.
    pub(super) fn draw_buffer_allocation(&self) -> (u64, u64) {
        (
            (self.draws.buf.capacity() * size_of::<super::Draw>()) as u64,
            self.draws.growths(),
        )
    }

    /// Growths of every slot since the renderer was created.
    fn growths(&self) -> u64 {
        [
            self.lights.growths(),
            self.gpu_lights.growths(),
            self.shadow_tiles.growths(),
            self.gi_probes.growths(),
            self.joints.growths(),
            self.spans.growths(),
            self.glass_index.growths(),
            self.object_uploads.growths(),
            self.opaque_ix.growths(),
            self.blend_ix.growths(),
            self.shadow_ix.growths(),
            self.trans_ix.growths(),
            self.csm_splits.growths(),
            self.caster_info.growths(),
            self.trans_info.growths(),
            self.cascade_uv.growths(),
            self.casters.growths(),
            self.caster_slot.growths(),
            self.shadow_jobs.growths(),
            self.cull_meshes.growths(),
            self.cull_slots.growths(),
            self.object_draw.growths(),
            self.object_bytes.growths(),
            self.velocity_keyed.growths(),
            self.velocity_instances.growths(),
            self.velocity_joints.growths(),
            self.velocity_draws.growths(),
            self.draws.growths(),
            self.stream_order.growths(),
            self.stream_in_scene.growths(),
        ]
        .into_iter()
        .sum()
    }

    /// Marks the start of a frame. `extra` is the growth total of buffers kept elsewhere (the
    /// GPU culler's).
    pub(super) fn begin_frame(&mut self, extra: u64) {
        self.at_frame_start = self.growths() + extra;
    }

    /// Allocation, capacity and growths of the pooled draw list (tests).
    #[cfg(test)]
    pub(super) fn draw_list_probe(&self) -> (*const super::Draw, usize, u64) {
        (
            self.draws.vec_ptr(),
            self.draws.vec_capacity(),
            self.draws.growths(),
        )
    }

    /// Capacities and growths of the stream-pass order list and the in-scene map (tests).
    #[cfg(test)]
    pub(super) fn stream_pass_probe(&self) -> (usize, u64, usize, u64) {
        (
            self.stream_order.vec_capacity(),
            self.stream_order.growths(),
            self.stream_in_scene.map_capacity(),
            self.stream_in_scene.growths(),
        )
    }

    /// Marks the end of a frame that gave its buffers back.
    pub(super) fn end_frame(&mut self, extra: u64) {
        self.last_frame_growths = (self.growths() + extra).saturating_sub(self.at_frame_start);
        self.frames += 1;
    }

    /// The counts, with `extra` growths kept elsewhere (the GPU culler's).
    pub(super) fn stats(&self, extra: u64) -> FrameScratchStats {
        FrameScratchStats {
            growths: self.growths() + extra,
            last_frame_growths: self.last_frame_growths,
            frames: self.frames,
        }
    }
}

/// Identity of frame draw `index` in the GPU object table and the indirect-command map.
///
/// One id per draw, not per [`crate::FrameMesh::object_key`]. Two draws that share an object
/// key (a LOD cross-fade, or two meshes of one object) keep their own `lod_dither` and their
/// own indirect command. A repeated id still shares the first draw's record.
#[inline]
pub(super) fn gpu_record_id(index: usize) -> u64 {
    index as u64
}

/// Cache key of one per-object uniform: the object key and the side of
/// [`crate::FrameMesh::lod_dither`] (`0` whole, `1` incoming, `2` outgoing).
///
/// Dither `0` is the whole draw. The incoming (`c > 0`) and outgoing (`c < 0`) copies of a
/// cross-fade get one uniform each, so they do not share `skin.y`. The magnitude is not part
/// of the key: it changes on every fade frame, and the draw list rewrites the uniform when its
/// contents change, so a fade reuses its two uniforms instead of creating a buffer and a bind
/// group per frame (each held until the 600-frame eviction).
#[inline]
pub(super) fn object_uniform_key(object: u64, lod_dither_bits: u32) -> (u64, u32) {
    let dither = f32::from_bits(lod_dither_bits);
    let side = if dither > 0.0 {
        1
    } else if dither < 0.0 {
        2
    } else {
        0
    };
    (object, side)
}

/// Packs one object record per GPU-culled draw at its object slot (`GPU_OBJECT_STRIDE` each)
/// into `bytes`, for `GpuCuller::write_objects`.
///
/// `draw_objects` are the record ids of the frame's draws in draw order ([`gpu_record_id`]).
/// A GPU draw takes the record of the first draw with its id (`object_at(draw index)`).
/// `first_draw` and `bytes` are scratch: both are cleared here and keep their room for the
/// next frame.
pub(super) fn pack_gpu_objects(
    bytes: &mut Vec<u8>,
    first_draw: &mut KeyMap<u64, usize>,
    draw_objects: impl Iterator<Item = u64>,
    gpu_draws: &[crate::gpu_driven::GpuDraw],
    object_at: impl Fn(usize) -> super::ObjectData,
) {
    first_draw.clear();
    for (index, object) in draw_objects.enumerate() {
        first_draw.entry(object).or_insert(index);
    }
    let stride = size_of::<super::ObjectData>();
    bytes.clear();
    bytes.resize(gpu_draws.len() * stride, 0);
    for gpu in gpu_draws {
        let Some(&index) = first_draw.get(&gpu.object) else {
            continue;
        };
        let start = gpu.object_slot as usize * stride;
        let end = start + stride;
        if end <= bytes.len() {
            bytes[start..end].copy_from_slice(bytemuck::bytes_of(&object_at(index)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_driven::GpuDraw;

    fn object(x: f32, joint_base: u32) -> super::super::ObjectData {
        let mut model = [[0.0; 4]; 4];
        model[3][0] = x;
        super::super::ObjectData {
            model,
            skin: [joint_base, 0, 0, 0],
        }
    }

    #[test]
    fn a_slot_counts_a_growth_only_when_the_list_outgrows_its_room() {
        let mut slot: Slot<Vec<u32>> = Slot::default();
        // Frame 1: the first fill allocates.
        let mut list = slot.take();
        list.extend(0..100);
        slot.give(list);
        assert_eq!(slot.growths(), 1);
        // Frames 2..50: the same or fewer elements fit; the buffer is the same allocation.
        let mut address = None;
        for frame in 0..50u32 {
            let mut list = slot.take();
            assert!(list.is_empty(), "a taken list starts empty");
            list.extend(0..100 - frame);
            let here = list.as_ptr();
            assert!(
                address.is_none_or(|a| a == here),
                "steady frames reuse one allocation"
            );
            address = Some(here);
            slot.give(list);
        }
        assert_eq!(slot.growths(), 1, "steady frames do not grow");
        // A frame with more elements grows once, then settles again.
        let mut list = slot.take();
        list.extend(0..1000);
        slot.give(list);
        assert_eq!(slot.growths(), 2);
        let mut list = slot.take();
        list.extend(0..1000);
        slot.give(list);
        assert_eq!(slot.growths(), 2);
    }

    #[test]
    fn a_list_dropped_by_an_early_return_counts_again() {
        let mut slot: Slot<Vec<u32>> = Slot::default();
        let mut list = slot.take();
        list.extend(0..10);
        slot.give(list);
        // A frame takes the list and returns early: the buffer is gone.
        let taken = slot.take();
        drop(taken);
        let mut list = slot.take();
        list.extend(0..10);
        slot.give(list);
        assert_eq!(slot.growths(), 2, "the replacement allocation is counted");
    }

    #[test]
    fn a_map_slot_keeps_its_room() {
        let mut slot: Slot<KeyMap<u64, usize>> = Slot::default();
        for _ in 0..20 {
            let mut map = slot.take();
            assert!(map.is_empty());
            map.extend((0..500u64).map(|key| (key * 7919, key as usize)));
            slot.give(map);
        }
        assert_eq!(slot.growths(), 1);
    }

    #[test]
    fn list_of_lists_reuses_its_inner_buffers() {
        let mut slot: Slot<ListOfLists<Footprint>> = Slot::default();
        for frame in 0..10 {
            let mut lists = slot.take();
            for cascade in 0..4 {
                let list = lists.push_list();
                assert!(list.is_empty(), "a pushed list starts empty");
                list.extend((0..64 + cascade).map(|_| Some(([0.0, 0.0], [1.0, 1.0]))));
            }
            assert_eq!(lists.lists().len(), 4);
            assert_eq!(lists.lists()[3].len(), 67);
            slot.give(lists);
            if frame == 0 {
                assert_eq!(slot.growths(), 1);
            }
        }
        assert_eq!(
            slot.growths(),
            1,
            "four cascades of the same casters never grow again"
        );
        // Fewer cascades: no growth, and only the pushed lists are visible.
        let mut lists = slot.take();
        lists.push_list().push(None);
        assert_eq!(lists.lists().len(), 1);
        assert_eq!(lists.lists()[0], vec![None]);
        slot.give(lists);
        assert_eq!(slot.growths(), 1);
    }

    #[test]
    fn frame_counts_report_the_last_frame_growths() {
        let mut scratch = FrameScratch::default();
        scratch.begin_frame(0);
        let mut ix = scratch.opaque_ix.take();
        ix.extend(0..8);
        scratch.opaque_ix.give(ix);
        let mut splits = scratch.csm_splits.take();
        splits.extend([1.0, 2.0]);
        scratch.csm_splits.give(splits);
        scratch.end_frame(3);
        let stats = scratch.stats(3);
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.growths, 5, "two slots plus three growths kept elsewhere");
        assert_eq!(stats.last_frame_growths, 5);
        scratch.begin_frame(3);
        let mut ix = scratch.opaque_ix.take();
        ix.extend(0..8);
        scratch.opaque_ix.give(ix);
        scratch.end_frame(3);
        let stats = scratch.stats(3);
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.growths, 5);
        assert_eq!(stats.last_frame_growths, 0, "a steady frame grows nothing");
    }

    #[test]
    fn gpu_objects_land_at_their_slots_and_the_first_draw_of_a_key_wins() {
        let mut bytes = Vec::new();
        let mut first = KeyMap::default();
        // Draw order: objects 10, 20, 10 (a duplicate key takes the first draw), 30 (not culled).
        let keys = [10u64, 20, 10, 30];
        let objects = [object(1.0, 1), object(2.0, 2), object(3.0, 3), object(4.0, 4)];
        let gpu = [
            GpuDraw {
                object: 20,
                index: 0,
                object_slot: 0,
            },
            GpuDraw {
                object: 10,
                index: 1,
                object_slot: 1,
            },
            GpuDraw {
                object: 99,
                index: 2,
                object_slot: 2,
            },
        ];
        pack_gpu_objects(&mut bytes, &mut first, keys.iter().copied(), &gpu, |i| objects[i]);
        let stride = size_of::<super::super::ObjectData>();
        assert_eq!(bytes.len(), 3 * stride);
        assert_eq!(&bytes[..stride], bytemuck::bytes_of(&objects[1]));
        assert_eq!(&bytes[stride..2 * stride], bytemuck::bytes_of(&objects[0]));
        assert!(
            bytes[2 * stride..].iter().all(|&b| b == 0),
            "an unknown object stays zero"
        );
        // A second frame of the same draws reuses both buffers.
        let (bytes_at, room) = (bytes.as_ptr(), first.capacity());
        pack_gpu_objects(&mut bytes, &mut first, keys.iter().copied(), &gpu, |i| objects[i]);
        assert_eq!(bytes.as_ptr(), bytes_at);
        assert_eq!(first.capacity(), room);
        assert_eq!(&bytes[stride..2 * stride], bytemuck::bytes_of(&objects[0]));
    }

    /// Two draws of one object key with opposite LOD dither land in two slots.
    ///
    /// The record id is the draw index ([`gpu_record_id`]), so the pair does not collapse
    /// onto the first draw's `skin.y`.
    #[test]
    fn a_cross_fade_pair_that_shares_an_object_key_keeps_both_dithers() {
        let mut bytes = Vec::new();
        let mut first = KeyMap::default();
        let plus = 0.4f32.to_bits();
        let minus = (-0.4f32).to_bits();
        let objects = [object_y(plus), object_y(minus)];
        let object_key = 7u64;
        let ids = [gpu_record_id(0), gpu_record_id(1)];
        assert_ne!(ids[0], ids[1], "object key {object_key} is not the record id");
        assert_ne!(ids[0], object_key);
        assert_ne!(ids[1], object_key);
        let gpu = [
            GpuDraw {
                object: ids[0],
                index: 3,
                object_slot: 0,
            },
            GpuDraw {
                object: ids[1],
                index: 8,
                object_slot: 1,
            },
        ];
        pack_gpu_objects(&mut bytes, &mut first, ids.into_iter(), &gpu, |i| objects[i]);
        let stride = size_of::<super::super::ObjectData>();
        assert_eq!(bytes.len(), 2 * stride);
        let skin_y = |slot: usize| {
            let start = slot * stride;
            let data: super::super::ObjectData = bytemuck::pod_read_unaligned(&bytes[start..start + stride]);
            data.skin[1]
        };
        assert_eq!(skin_y(0), plus, "the incoming copy keeps +0.4");
        assert_eq!(skin_y(1), minus, "the outgoing copy keeps −0.4");
    }

    #[test]
    fn opposite_dithers_of_one_object_do_not_share_a_uniform_key() {
        let plus = object_uniform_key(7, 0.4f32.to_bits());
        let minus = object_uniform_key(7, (-0.4f32).to_bits());
        assert_eq!(plus.0, 7);
        assert_eq!(minus.0, 7);
        assert_ne!(plus, minus, "the cross-fade copies do not share skin.y");
        assert_eq!(object_uniform_key(7, 0.0f32.to_bits()), (7, 0));
    }

    #[test]
    fn a_fade_keeps_one_uniform_key_per_side_across_its_frames() {
        // The dither magnitude moves every fade frame (mesh_lod: 8 frames); a key per value made
        // a new uniform buffer and bind group per fading draw per frame.
        let incoming: Vec<_> = (1..8)
            .map(|i| object_uniform_key(7, (i as f32 / 8.0).to_bits()))
            .collect();
        let outgoing: Vec<_> = (1..8)
            .map(|i| object_uniform_key(7, (-(i as f32) / 8.0).to_bits()))
            .collect();
        assert!(incoming.iter().all(|k| *k == incoming[0]), "{incoming:?}");
        assert!(outgoing.iter().all(|k| *k == outgoing[0]), "{outgoing:?}");
        assert_ne!(incoming[0], outgoing[0]);
        assert_ne!(incoming[0], object_uniform_key(7, 0.0f32.to_bits()));
        assert_ne!(outgoing[0], object_uniform_key(7, 0.0f32.to_bits()));
        assert_eq!(
            object_uniform_key(7, (-0.0f32).to_bits()),
            (7, 0),
            "-0 is drawn whole"
        );
    }

    /// A fade moves the dither every frame: one side of it keeps one uniform for the whole fade
    /// (the draw list rewrites `skin.y`), instead of a new uniform buffer and bind group per frame
    /// that the idle eviction only drops 600 frames later.
    #[test]
    fn a_fade_in_progress_keeps_one_uniform_per_side() {
        let side = |c: f32| object_uniform_key(7, c.to_bits());
        let incoming: Vec<(u64, u32)> = (1..30).map(|f| side(f as f32 / 30.0)).collect();
        assert!(incoming.windows(2).all(|w| w[0] == w[1]), "{incoming:?}");
        let outgoing: Vec<(u64, u32)> = (1..30).map(|f| side(-(f as f32) / 30.0)).collect();
        assert!(outgoing.windows(2).all(|w| w[0] == w[1]), "{outgoing:?}");
        assert_ne!(incoming[0], outgoing[0]);
        assert_ne!(incoming[0], side(0.0));
        assert_ne!(outgoing[0], side(0.0));
        assert_eq!(side(-0.0), side(0.0), "negative zero is the whole draw");
        assert_ne!(side(0.3), object_uniform_key(8, 0.3f32.to_bits()));
    }

    fn object_y(dither_bits: u32) -> super::super::ObjectData {
        super::super::ObjectData {
            model: [[0.0; 4]; 4],
            skin: [0, dither_bits, 0, 0],
        }
    }
}
