// SPDX-License-Identifier: MPL-2.0
//! The visibility buffer's cluster arena (ADR-353): suballocated, recycled storage for the
//! cluster records and the geometry words (vertices, cluster vertex lists, triangles) of every
//! resident [`super::MeshletMesh`].
//!
//! The visibility shaders address one flat storage buffer per stream (a cluster's
//! `vertex_offset` and `triangle_offset` are word offsets into the geometry buffer, and the
//! instance records name absolute cluster indices), so the multi-chunk `GpuArena` of the mesh
//! cache (ADR-218, whose chunks are separate buffers) cannot back them. Each stream is instead one
//! buffer whose ranges come from ADR-218's best-fit coalescing free list (`RangeFreeList`): a
//! removed mesh's ranges return to the list and the next mesh takes them, so churn creates no
//! buffer. A stream that runs out of room doubles (one new buffer, which gets a GPU copy of the old
//! one and then the ranges written since, ADR-7462) up to a cap, which is logged once when hit; an
//! allocation past the cap keeps that mesh on the forward path.
//!
//! A geometry range may be GPU-owned ([`ClusterArena::own_on_gpu`], ADR-7462): a counted mesh's
//! vertices, written by the compute repack of its upload. The CPU mirror never writes such a range
//! (a write that overlaps it is refused, so no upload covers it) and a growth carries it over with
//! the GPU copy.
//!
//! Every resident mesh is named by a [`ClusterKey`] (slot + generation): removing a mesh bumps
//! its slot's generation, so a stale key is refused and never frees another mesh's ranges.

use std::collections::BTreeMap;

use super::meshlet::Meshlet;
use crate::device::{TrackedDevice, TrackedQueue};
use crate::mesh_storage::RangeFreeList;

/// Geometry words a fresh arena starts with (256 KiB).
const GEO_MIN_WORDS: u32 = 1 << 16;
/// Largest geometry stream, words (1 GiB; the device's storage binding limit may lower it).
const GEO_MAX_WORDS: u32 = 1 << 28;
/// Cluster records a fresh arena starts with (384 KiB).
const CLUSTER_MIN: u32 = 1 << 12;
/// Largest cluster stream, records (768 MiB; the device's storage binding limit may lower it).
const CLUSTER_MAX: u32 = 1 << 23;

/// One resident mesh in the [`ClusterArena`]. Stale once the mesh is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClusterKey {
    slot: u32,
    generation: u32,
}

/// Where a resident mesh's data lives: (first unit, units) in each stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClusterRanges {
    /// Geometry words: the mesh's vertices, then its cluster vertex lists, then its triangles.
    pub geo: (u32, u32),
    /// Cluster records.
    pub clusters: (u32, u32),
}

/// One flat, growable stream: a CPU mirror the size of the GPU buffer, a free list over it and
/// the ranges written since the last upload.
struct Stream<T: bytemuck::Pod> {
    label: &'static str,
    data: Vec<T>,
    free: RangeFreeList,
    /// Live ranges, `start -> len` (rebuilds the free list when the stream grows).
    live: BTreeMap<u32, u32>,
    min_units: u32,
    max_units: u32,
    /// Ranges written since the last upload, `(start, len)`.
    dirty: Vec<(u32, u32)>,
    /// GPU-owned ranges, `start -> len` (ADR-7462): written on the GPU, never from the mirror.
    gpu_owned: BTreeMap<u32, u32>,
    /// The last [`Self::plan`]'s writes, `(start, end)` (kept to reuse its storage).
    writes: Vec<(u32, u32)>,
    /// Capacity doublings since start.
    growths: u64,
    /// GPU buffers created for this stream since start.
    buffer_creates: u64,
    /// The cap was hit (logged once).
    capped: bool,
}

impl<T: bytemuck::Pod> Stream<T> {
    fn new(label: &'static str, min_units: u32, max_units: u32) -> Self {
        Self {
            label,
            data: Vec::new(),
            free: RangeFreeList::new(0),
            live: BTreeMap::new(),
            min_units,
            max_units,
            dirty: Vec::new(),
            gpu_owned: BTreeMap::new(),
            writes: Vec::new(),
            growths: 0,
            buffer_creates: 0,
            capped: false,
        }
    }

    fn capacity(&self) -> u32 {
        self.data.len() as u32
    }

    fn used(&self) -> u32 {
        self.capacity() - self.free.free_units()
    }

    /// Takes `len` units (best fit), doubling the stream until they fit or the cap is reached.
    fn alloc(&mut self, len: u32) -> Option<u32> {
        let len = len.max(1);
        loop {
            if let Some(start) = self.free.alloc(len) {
                self.live.insert(start, len);
                return Some(start);
            }
            if !self.grow() {
                return None;
            }
        }
    }

    fn free(&mut self, start: u32) {
        if let Some(len) = self.live.remove(&start) {
            self.free.free(start, len);
            // GPU-owned ranges go with the allocation they lie in, and so do its writes not yet
            // uploaded: nothing reads them, and a counted mesh admitted into the hole the same
            // frame must be able to own its vertex words (rev-claude-60 finding 5).
            let end = start + len;
            if !self.gpu_owned.is_empty() {
                self.gpu_owned.retain(|&owned, _| owned < start || owned >= end);
            }
            self.dirty.retain(|&(s, l)| s < start || s + l > end);
        }
    }

    /// Whether `start..start + len` overlaps a GPU-owned range.
    fn overlaps_gpu(&self, start: u32, len: u32) -> bool {
        let end = start.saturating_add(len);
        self.gpu_owned
            .range(..end)
            .next_back()
            .is_some_and(|(&owned, &owned_len)| owned + owned_len > start)
    }

    /// Doubles the capacity (the first growth takes `min_units`). False at the cap.
    fn grow(&mut self) -> bool {
        let capacity = self.capacity();
        if capacity >= self.max_units {
            if !self.capped {
                self.capped = true;
                tracing::warn!(
                    target: "regia_render",
                    stream = self.label,
                    cap = self.max_units,
                    "visibility-buffer cluster arena is at its cap; new meshes draw forward"
                );
            }
            return false;
        }
        let grown = if capacity == 0 {
            self.min_units
        } else {
            capacity.saturating_mul(2)
        }
        .min(self.max_units);
        // A fresh list over the new size with every live range taken out again: the free
        // ranges keep their places and the new tail joins the old one.
        let mut free = RangeFreeList::new(grown);
        if free.alloc(grown).is_some() {
            let mut at = 0u32;
            for (&start, &len) in &self.live {
                if start > at {
                    free.free(at, start - at);
                }
                at = start + len;
            }
            if grown > at {
                free.free(at, grown - at);
            }
        }
        self.free = free;
        self.data
            .resize(grown as usize, <T as bytemuck::Zeroable>::zeroed());
        self.growths += 1;
        tracing::info!(
            target: "regia_render",
            stream = self.label,
            capacity = grown,
            cap = self.max_units,
            "visibility-buffer cluster arena grew"
        );
        true
    }

    /// Writes `values` at `start` into the mirror and marks them for the next upload. Refused
    /// (nothing written) when the range overlaps a GPU-owned one: the mirror never writes those.
    fn write(&mut self, start: u32, values: &[T]) {
        let len = values.len() as u32;
        if self.overlaps_gpu(start, len) {
            debug_assert!(false, "a CPU write into a GPU-owned range");
            tracing::error!(
                target: "regia_render",
                stream = self.label,
                start,
                len,
                "a CPU write into a GPU-owned cluster-arena range was refused"
            );
            return;
        }
        let begin = start as usize;
        if let Some(target) = self.data.get_mut(begin..begin + values.len()) {
            target.copy_from_slice(values);
            self.dirty.push((start, len));
        }
    }

    /// Makes `start..start + len` (inside a live allocation) GPU-owned. False when it is not
    /// inside one, is empty, is GPU-owned already, or overlaps a range written since the last
    /// upload.
    fn own_on_gpu(&mut self, start: u32, len: u32) -> bool {
        let inside = self
            .live
            .range(..=start)
            .next_back()
            .is_some_and(|(&alloc, &alloc_len)| start + len <= alloc + alloc_len);
        let written = self.dirty.iter().any(|&(s, l)| s < start + len && start < s + l);
        if !inside || written || len == 0 || self.overlaps_gpu(start, len) {
            return false;
        }
        self.gpu_owned.insert(start, len);
        true
    }

    /// What the next upload does, given the buffer there now (`current`: its size in bytes):
    /// `Some(old_bytes)` copy of the old buffer when the stream needs a new one (growth, or the
    /// first buffer with `0`), and the ranges written since the last upload in
    /// [`Self::writes`] (sorted; overlapping or touching ones merged; never a GPU-owned word).
    /// `None` when nothing changes. The mirror is never uploaded whole (ADR-7462).
    fn plan(&mut self, current: Option<u64>) -> Option<Option<u64>> {
        let bytes = (self.data.len() * std::mem::size_of::<T>()) as u64;
        if bytes == 0 {
            return None;
        }
        let recreate = current.is_none_or(|size| size != bytes);
        self.writes.clear();
        if self.dirty.is_empty() && !recreate {
            return None;
        }
        self.dirty.sort_unstable();
        let mut pending: Option<(u32, u32)> = None;
        for &(start, len) in &self.dirty {
            pending = match pending {
                Some((s, e)) if start <= e => Some((s, e.max(start + len))),
                Some(done) => {
                    self.writes.push(done);
                    Some((start, start + len))
                }
                None => Some((start, start + len)),
            };
        }
        if let Some(done) = pending {
            self.writes.push(done);
        }
        self.dirty.clear();
        debug_assert!(
            self.writes.iter().all(|&(s, e)| !self.overlaps_gpu(s, e - s)),
            "an upload would cover a GPU-owned range"
        );
        Some(recreate.then_some(current.unwrap_or(0)))
    }

    /// Lowers the cap to `max_bytes` (the device's storage binding limit); never below the
    /// current capacity.
    fn clamp_bytes(&mut self, max_bytes: u64) {
        let units = (max_bytes / std::mem::size_of::<T>() as u64).min(u64::from(u32::MAX)) as u32;
        self.max_units = self.max_units.min(units).max(self.capacity());
    }

    /// Makes `slot` hold this stream ([`Self::plan`]): a new buffer the size of the capacity when
    /// the stream grew (the old buffer copied into it on the GPU, in a submission of its own so the
    /// writes after it land on top), then the ranges written since the last call. True when the
    /// buffer was replaced (bind groups are rebuilt).
    fn sync(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        slot: &mut Option<(wgpu::Buffer, u64)>,
        usage: wgpu::BufferUsages,
    ) -> bool {
        let bytes = (self.data.len() * std::mem::size_of::<T>()) as u64;
        let Some(recreate) = self.plan(slot.as_ref().map(|(_, size)| *size)) else {
            return false;
        };
        let _span = tracing::info_span!("visbuffer_arena_upload", stream = self.label).entered();
        if let Some(old_bytes) = recreate {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: bytes.next_multiple_of(16),
                usage,
                mapped_at_creation: false,
            });
            if let Some((old, _)) = slot.as_ref()
                && old_bytes > 0
            {
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("vis arena growth copy"),
                });
                encoder.copy_buffer_to_buffer(old, 0, &buffer, 0, old_bytes.min(bytes));
                queue.submit([encoder.finish()]);
            }
            *slot = Some((buffer, bytes));
            self.buffer_creates += 1;
        }
        let Some((buffer, _)) = slot.as_ref() else {
            return false;
        };
        let stride = std::mem::size_of::<T>() as u64;
        for &(s, e) in &self.writes {
            queue.write_buffer(
                buffer,
                u64::from(s) * stride,
                bytemuck::cast_slice(&self.data[s as usize..e as usize]),
            );
        }
        recreate.is_some()
    }
}

struct Slot {
    generation: u32,
    ranges: Option<ClusterRanges>,
}

/// The suballocated cluster and geometry storage of a [`super::gpu::VisBufferPass`].
pub(crate) struct ClusterArena {
    geo: Stream<u32>,
    clusters: Stream<Meshlet>,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
}

impl Default for ClusterArena {
    fn default() -> Self {
        Self::with_limits(GEO_MIN_WORDS, GEO_MAX_WORDS, CLUSTER_MIN, CLUSTER_MAX)
    }
}

impl ClusterArena {
    /// An empty arena with the given stream sizes (tests use small ones).
    pub(crate) fn with_limits(geo_min: u32, geo_max: u32, cluster_min: u32, cluster_max: u32) -> Self {
        Self {
            geo: Stream::new("vis geometry", geo_min, geo_max),
            clusters: Stream::new("vis meshlets", cluster_min, cluster_max),
            slots: Vec::new(),
            free_slots: Vec::new(),
        }
    }

    /// Ranges for a mesh of `geo_words` geometry words and `clusters` clusters. `None` (nothing
    /// taken) when either stream is at its cap.
    pub(crate) fn alloc(&mut self, geo_words: u32, clusters: u32) -> Option<(ClusterKey, ClusterRanges)> {
        let geo_start = self.geo.alloc(geo_words)?;
        let Some(cluster_start) = self.clusters.alloc(clusters) else {
            self.geo.free(geo_start);
            return None;
        };
        let ranges = ClusterRanges {
            geo: (geo_start, geo_words.max(1)),
            clusters: (cluster_start, clusters.max(1)),
        };
        let slot = match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    ranges: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let entry = &mut self.slots[slot as usize];
        entry.ranges = Some(ranges);
        Some((
            ClusterKey {
                slot,
                generation: entry.generation,
            },
            ranges,
        ))
    }

    /// A geometry range of `geo_words` words with no cluster records (a residency band's block,
    /// ADR-3118). `None` (nothing taken) when the geometry stream is at its cap.
    pub(crate) fn alloc_geo(&mut self, geo_words: u32) -> Option<(ClusterKey, u32)> {
        let geo_start = self.geo.alloc(geo_words)?;
        let ranges = ClusterRanges {
            geo: (geo_start, geo_words.max(1)),
            clusters: (u32::MAX, 0),
        };
        let slot = match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    ranges: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let entry = &mut self.slots[slot as usize];
        entry.ranges = Some(ranges);
        Some((
            ClusterKey {
                slot,
                generation: entry.generation,
            },
            geo_start,
        ))
    }

    /// Returns `key`'s ranges to the free lists. False (nothing freed) for a stale key.
    pub(crate) fn free(&mut self, key: ClusterKey) -> bool {
        let Some(entry) = self.slots.get_mut(key.slot as usize) else {
            return false;
        };
        if entry.generation != key.generation {
            return false;
        }
        let Some(ranges) = entry.ranges.take() else {
            return false;
        };
        entry.generation = entry.generation.wrapping_add(1);
        self.free_slots.push(key.slot);
        self.geo.free(ranges.geo.0);
        if ranges.clusters.1 > 0 {
            self.clusters.free(ranges.clusters.0);
        }
        true
    }

    /// `key`'s ranges, `None` for a stale key.
    #[cfg(test)]
    pub(crate) fn get(&self, key: ClusterKey) -> Option<ClusterRanges> {
        self.slots
            .get(key.slot as usize)
            .filter(|entry| entry.generation == key.generation)
            .and_then(|entry| entry.ranges)
    }

    /// Makes the geometry words `start..start + len` (inside a range from [`Self::alloc`], not
    /// written since the last upload) GPU-owned (ADR-7462): written on the GPU (a counted mesh's vertex repack),
    /// never by the CPU mirror, carried over by a growth's GPU copy, released with the range.
    /// False (nothing changes) when the words are not inside a live range or were written.
    pub(crate) fn own_on_gpu(&mut self, start: u32, len: u32) -> bool {
        self.geo.own_on_gpu(start, len)
    }

    /// Whether `key` names a resident range (not freed since).
    pub(crate) fn is_live(&self, key: ClusterKey) -> bool {
        self.slots
            .get(key.slot as usize)
            .is_some_and(|entry| entry.generation == key.generation && entry.ranges.is_some())
    }

    /// Writes geometry words at `start` (inside a range from [`Self::alloc`]; refused over a
    /// GPU-owned range).
    pub(crate) fn write_geo(&mut self, start: u32, words: &[u32]) {
        self.geo.write(start, words);
    }

    /// Test hook: the geometry words at `start..start + len` of the CPU mirror, `None` past
    /// the stream.
    #[cfg(test)]
    pub(crate) fn geo_words(&self, start: u32, len: usize) -> Option<&[u32]> {
        self.geo.data.get(start as usize..start as usize + len)
    }

    /// Writes cluster records at `start` (inside a range from [`Self::alloc`]).
    pub(crate) fn write_clusters(&mut self, start: u32, clusters: &[Meshlet]) {
        self.clusters.write(start, clusters);
    }

    /// Cluster records at `start` (the GPU meshlet index), or empty when the range is not resident.
    pub(crate) fn meshlets(&self, start: u32, count: u32) -> &[Meshlet] {
        let start = start as usize;
        let end = start.saturating_add(count as usize);
        self.clusters.data.get(start..end).unwrap_or(&[])
    }

    /// Lowers both caps to the device's largest storage binding.
    pub(crate) fn clamp_to(&mut self, max_binding_bytes: u64) {
        self.geo.clamp_bytes(max_binding_bytes);
        self.clusters.clamp_bytes(max_binding_bytes);
    }

    /// Uploads both streams into `geo` and `clusters` (see `Stream::sync`). True when a
    /// buffer was replaced.
    pub(crate) fn sync(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        geo: &mut Option<(wgpu::Buffer, u64)>,
        clusters: &mut Option<(wgpu::Buffer, u64)>,
        usage: wgpu::BufferUsages,
    ) -> bool {
        let a = self.geo.sync(device, queue, geo, usage);
        let b = self.clusters.sync(device, queue, clusters, usage);
        a | b
    }

    /// Bytes of resident geometry and cluster records.
    pub(crate) fn used_bytes(&self) -> u64 {
        u64::from(self.geo.used()) * 4
            + u64::from(self.clusters.used()) * std::mem::size_of::<Meshlet>() as u64
    }

    /// Occupancy and counters.
    pub(crate) fn stats(&self) -> super::VisBufferArenaStats {
        let cluster_bytes = std::mem::size_of::<Meshlet>() as u64;
        super::VisBufferArenaStats {
            meshes: self
                .slots
                .iter()
                .filter(|s| s.ranges.is_some_and(|r| r.clusters.1 > 0))
                .count() as u32,
            clusters: u64::from(self.clusters.used()),
            cluster_capacity: u64::from(self.clusters.capacity()),
            geometry_words: u64::from(self.geo.used()),
            geometry_capacity_words: u64::from(self.geo.capacity()),
            used_bytes: self.used_bytes(),
            free_bytes: u64::from(self.geo.free.free_units()) * 4
                + u64::from(self.clusters.free.free_units()) * cluster_bytes,
            capacity_bytes: u64::from(self.geo.capacity()) * 4
                + u64::from(self.clusters.capacity()) * cluster_bytes,
            free_ranges: (self.geo.free.free_ranges() + self.clusters.free.free_ranges()) as u64,
            growths: self.geo.growths + self.clusters.growths,
            buffer_creates: self.geo.buffer_creates + self.clusters.buffer_creates,
            capped: self.geo.capped || self.clusters.capped,
            ..super::VisBufferArenaStats::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(i: u32) -> Meshlet {
        Meshlet {
            bounds: [i as f32, 0.0, 0.0, 1.0],
            cone: [0.0, 0.0, 1.0, 1.0],
            lod: [0.0; 4],
            parent: [0.0; 4],
            lod_error: 0.0,
            parent_error: super::super::ROOT_ERROR,
            vertex_offset: 0,
            triangle_offset: 0,
            vertex_count: 3,
            triangle_count: 1,
            level: 0,
            pad: 0,
        }
    }

    /// Mixed sizes from a fixed sequence: small props to dense rocks (1 to 61 clusters).
    fn size_of_mesh(i: u32) -> (u32, u32) {
        let clusters = 1 + (i * 37) % 61;
        let words = clusters * 300 + (i * 13) % 97;
        (words, clusters)
    }

    /// Inserting and removing 2 000 meshes of mixed sizes in five waves (each wave the same
    /// sizes in another order, removed in a scrambled order) grows the arena only during the
    /// first wave: after it, no stream grows (each growth is exactly one buffer create at the
    /// next upload), every key frees once, a stale key is refused, and once everything is
    /// removed each stream is one free range again.
    #[test]
    fn churn_recycles_ranges_with_no_growth_after_warm_up() {
        let mut arena = ClusterArena::default();
        const WAVE: u32 = 400;
        let mut held: Vec<(u32, ClusterKey)> = Vec::new();
        let mut stale: Option<ClusterKey> = None;
        let mut warm_growths = None;
        for wave in 0..5u32 {
            for n in 0..WAVE {
                // A permutation of 0..WAVE that differs per wave (7 is coprime to 400).
                let i = (n * 7 + wave * 31) % WAVE;
                let id = wave * WAVE + i;
                let (words, clusters) = size_of_mesh(i);
                let (key, ranges) = arena.alloc(words, clusters).expect("within the cap");
                arena.write_clusters(ranges.clusters.0, &vec![cluster(id); clusters as usize]);
                arena.write_geo(ranges.geo.0, &vec![id; words as usize]);
                held.push((id, key));
            }
            if wave == 0 {
                warm_growths = Some(arena.stats().growths);
            } else {
                assert_eq!(
                    Some(arena.stats().growths),
                    warm_growths,
                    "wave {wave} grew the arena after warm-up"
                );
            }
            // Remove every held mesh in a scrambled order: the next wave reuses the ranges.
            let mut order: Vec<usize> = (0..held.len()).collect();
            order.sort_by_key(|&k| (k * 7919) % held.len());
            for k in order {
                let (_, key) = held[k];
                assert!(arena.free(key), "a live key frees");
                stale = Some(key);
            }
            held.clear();
            assert_eq!(arena.stats().meshes, 0);
            assert_eq!(arena.stats().used_bytes, 0);
            assert_eq!(arena.geo.free.free_ranges(), 1, "geometry coalesced");
            assert_eq!(arena.clusters.free.free_ranges(), 1, "clusters coalesced");
        }
        let stale = stale.expect("keys were freed");
        assert!(!arena.free(stale), "a stale key is refused");
        assert_eq!(arena.get(stale), None);
        // A new mesh in the stale key's slot is not reachable through the stale key.
        let (fresh, _) = arena.alloc(64, 1).expect("alloc");
        assert_ne!(fresh, stale);
        assert!(!arena.free(stale));
        assert!(arena.get(fresh).is_some());
        assert!(arena.free(fresh));
    }

    /// Live meshes keep their ranges and data across a growth; the new tail joins the free
    /// list.
    #[test]
    fn growth_keeps_live_ranges_and_their_data() {
        let mut arena = ClusterArena::with_limits(64, 1 << 12, 4, 1 << 8);
        let (a, ra) = arena.alloc(40, 3).expect("a");
        arena.write_geo(ra.geo.0, &[7; 40]);
        arena.write_clusters(ra.clusters.0, &[cluster(1); 3]);
        let (b, rb) = arena.alloc(20, 2).expect("b");
        assert!(arena.free(a));
        // Larger than everything there: the streams double.
        let (_, rc) = arena.alloc(500, 9).expect("c");
        assert_eq!(arena.get(b), Some(rb), "b kept its ranges");
        assert!(arena.stats().growths >= 2);
        assert!(arena.stats().capacity_bytes > 0);
        // a's freed range (before b) is still free and takes a small mesh.
        let (_, rd) = arena.alloc(40, 3).expect("d");
        assert_eq!(rd.geo.0, ra.geo.0, "best fit reuses a's hole");
        assert_ne!(rc.geo.0, rd.geo.0);
        assert_eq!(&arena.geo.data[ra.geo.0 as usize..][..40], &[7; 40]);
    }

    /// Past the cap an allocation fails cleanly (nothing taken) and the cap is reported.
    #[test]
    fn the_cap_refuses_without_leaking() {
        let mut arena = ClusterArena::with_limits(16, 64, 2, 4);
        let (_, _) = arena.alloc(60, 2).expect("fits");
        let before = arena.stats();
        assert!(arena.alloc(10, 1).is_none(), "geometry is full");
        assert!(arena.alloc(2, 3).is_none(), "clusters are full");
        let after = arena.stats();
        assert!(after.capped);
        assert_eq!(
            after.used_bytes, before.used_bytes,
            "a failed alloc takes nothing"
        );
        assert_eq!(after.meshes, 1);
    }

    /// A geometry-only range (a residency band) takes no cluster record, frees once and does not
    /// count as a mesh.
    #[test]
    fn geometry_only_ranges_take_no_records() {
        let mut arena = ClusterArena::default();
        let (mesh, _) = arena.alloc(100, 4).expect("mesh");
        let used = arena.stats().clusters;
        let (band, start) = arena.alloc_geo(300).expect("band");
        assert_eq!(arena.stats().clusters, used, "no cluster record taken");
        assert_eq!(arena.stats().meshes, 1, "a band is not a mesh");
        arena.write_geo(start, &[9; 300]);
        assert!(arena.free(band));
        assert!(!arena.free(band), "stale");
        assert_eq!(arena.stats().clusters, used);
        assert!(arena.free(mesh));
        assert_eq!(arena.stats().used_bytes, 0);
    }

    #[test]
    fn device_limit_lowers_the_cap_but_not_below_the_capacity() {
        let mut arena = ClusterArena::default();
        let _ = arena.alloc(10, 1).expect("alloc");
        arena.clamp_to(1024);
        assert_eq!(arena.geo.max_units, GEO_MIN_WORDS, "the current capacity stays");
        assert_eq!(arena.clusters.max_units, CLUSTER_MIN);
    }

    /// The ranges the next upload writes, as `(start, end)`, and whether it replaces the buffer
    /// (with the old size copied over on the GPU).
    fn upload(stream: &mut Stream<u32>, current: Option<u64>) -> (Option<Option<u64>>, Vec<(u32, u32)>) {
        let plan = stream.plan(current);
        (plan, stream.writes.clone())
    }

    fn intersects(writes: &[(u32, u32)], (start, len): (u32, u32)) -> bool {
        writes.iter().any(|&(s, e)| s < start + len && start < e)
    }

    /// ADR-7462: a GPU-owned geometry range survives the stream's growth and the uploads of the
    /// CPU-written ranges around it. The mirror never writes it: a write into it is refused, no
    /// upload (first buffer, merged neighbours, growth) covers it, the growth copies the old buffer
    /// on the GPU instead of uploading the mirror, and it goes with its allocation.
    #[test]
    fn a_gpu_owned_range_survives_growth_and_its_neighbours_uploads() {
        let mut arena = ClusterArena::with_limits(256, 1 << 12, 4, 1 << 8);
        // A counted mesh: 96 words, the first 64 (its vertices) GPU-owned, the rest (lists and
        // triangles) written through the mirror; then a CPU mesh right after it.
        let (a, ra) = arena.alloc(96, 2).expect("a");
        let owned = (ra.geo.0, 64);
        assert!(arena.own_on_gpu(owned.0, owned.1), "inside a fresh range");
        assert!(!arena.own_on_gpu(owned.0, owned.1), "already GPU-owned");
        arena.write_geo(owned.0 + 64, &[5; 32]);
        let (_, rb) = arena.alloc(100, 1).expect("b");
        assert_eq!(rb.geo.0, ra.geo.0 + 96, "b sits right after a");
        arena.write_geo(rb.geo.0, &[6; 100]);
        let ranges = vec![(owned.0 + 64, rb.geo.0 + 100)];

        // The first buffer: only the written words, merged across a and b; none of a's vertices.
        let (plan, writes) = upload(&mut arena.geo, None);
        assert_eq!(plan, Some(Some(0)), "a new buffer, nothing to copy");
        assert_eq!(writes, ranges);
        assert!(!intersects(&writes, owned));
        let size = Some(u64::from(arena.geo.capacity()) * 4);
        // Nothing written since: no upload.
        assert_eq!(upload(&mut arena.geo, size).0, None);

        // A CPU write into the GPU-owned words is refused: the mirror keeps its zeros there and
        // nothing is marked for upload.
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            arena.write_geo(owned.0 + 10, &[9; 4]);
        }));
        assert_eq!(refused.is_err(), cfg!(debug_assertions), "debug builds assert");
        assert_eq!(arena.geo_words(owned.0, 64), Some(&[0u32; 64][..]));
        assert_eq!(upload(&mut arena.geo, size).0, None, "nothing written");

        // The neighbours on both sides are written again: their writes merge around the range,
        // never across it.
        arena.write_geo(owned.0 + 64, &[7; 32]);
        arena.write_geo(rb.geo.0, &[8; 10]);
        let (plan, writes) = upload(&mut arena.geo, size);
        assert_eq!(plan, Some(None), "the same buffer");
        assert_eq!(writes, vec![(owned.0 + 64, rb.geo.0 + 10)]);
        assert!(!intersects(&writes, owned));

        // A mesh larger than the stream: it grows. The new buffer gets the old one copied on the
        // GPU (a's vertices included) and then only the new mesh's words, not the mirror.
        let old_bytes = u64::from(arena.geo.capacity()) * 4;
        let (_, rc) = arena.alloc(600, 1).expect("c");
        assert!(arena.stats().growths >= 2, "the stream grew");
        arena.write_geo(rc.geo.0, &[3; 600]);
        let (plan, writes) = upload(&mut arena.geo, size);
        assert_eq!(
            plan,
            Some(Some(old_bytes)),
            "a new buffer with the old one copied over"
        );
        assert_eq!(writes, vec![(rc.geo.0, rc.geo.0 + 600)]);
        assert!(!intersects(&writes, owned));
        assert_eq!(arena.geo_words(owned.0, 64), Some(&[0u32; 64][..]));

        // The range goes with its allocation: the hole takes a CPU mesh again.
        assert!(arena.free(a));
        assert!(arena.geo.gpu_owned.is_empty());
        let (_, rd) = arena.alloc(96, 2).expect("d");
        assert_eq!(rd.geo.0, ra.geo.0, "best fit reuses a's hole");
        arena.write_geo(rd.geo.0, &[4; 96]);
        let size = Some(u64::from(arena.geo.capacity()) * 4);
        let (_, writes) = upload(&mut arena.geo, size);
        assert_eq!(writes, vec![(rd.geo.0, rd.geo.0 + 96)]);
    }

    /// rev-claude-60 finding 5: a range freed in the same frame it was written (its writes not
    /// uploaded yet) takes GPU ownership when a counted mesh is admitted into it: the freed
    /// allocation's pending writes go with it, so they neither block the ownership nor are uploaded
    /// over the GPU-written words later.
    #[test]
    fn a_range_freed_before_its_upload_can_be_gpu_owned_again() {
        let mut arena = ClusterArena::with_limits(256, 1 << 12, 4, 1 << 8);
        let (keep, rk) = arena.alloc(32, 1).expect("keep");
        arena.write_geo(rk.geo.0, &[1; 32]);
        let (gone, rg) = arena.alloc(96, 2).expect("gone");
        arena.write_geo(rg.geo.0, &[2; 96]);
        // Same frame: the mesh goes before the upload, and a counted mesh takes its hole.
        assert!(arena.free(gone));
        let (_, rc) = arena.alloc(96, 2).expect("counted");
        assert_eq!(rc.geo.0, rg.geo.0, "best fit reuses the hole");
        assert!(
            arena.own_on_gpu(rc.geo.0, 64),
            "the freed allocation's pending writes went with it"
        );
        let (_, writes) = upload(&mut arena.geo, None);
        assert_eq!(
            writes,
            vec![(rk.geo.0, rk.geo.0 + 32)],
            "only the live mesh's words"
        );
        assert!(!intersects(&writes, (rc.geo.0, 64)));
        assert!(arena.is_live(keep));
    }

    /// Only words inside a live allocation become GPU-owned.
    #[test]
    fn gpu_ownership_needs_a_live_unwritten_range() {
        let mut arena = ClusterArena::with_limits(64, 1 << 10, 4, 1 << 6);
        let (key, r) = arena.alloc(32, 1).expect("alloc");
        assert!(!arena.own_on_gpu(r.geo.0, 33), "past the range");
        assert!(!arena.own_on_gpu(r.geo.0 + 40, 4), "outside every range");
        assert!(!arena.own_on_gpu(r.geo.0, 0), "empty");
        assert!(arena.own_on_gpu(r.geo.0 + 8, 8));
        assert!(!arena.own_on_gpu(r.geo.0 + 12, 8), "overlaps the owned words");
        assert!(arena.is_live(key));
        assert!(arena.free(key));
        assert!(!arena.is_live(key), "a freed key is stale");
        assert!(!arena.own_on_gpu(r.geo.0 + 8, 8), "freed");
    }
}
