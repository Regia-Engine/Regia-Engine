// SPDX-License-Identifier: MPL-2.0
//! GPU mesh arenas and object buffers (`specs/13` § 8).
//!
//! The renderer's mesh cache suballocates vertex and index data from two [`GpuArena`]s (one
//! for vertices, one for `u32` indices): a mesh is a range in a shared chunk buffer, drawn with
//! `base_vertex` / `first_index`, so a new mesh after an eviction takes a freed range and
//! creates no buffer. Ranges are owned by an [`ArenaLease`]; dropping the last clone of it
//! (the cache entry, or a LOD cross-fade's twin entry) returns the range to the free list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Suballocation in the shared vertex arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshSlice {
    /// First vertex in the arena.
    pub base_vertex: u32,
    /// First index in the index arena.
    pub first_index: u32,
    /// Number of indices to draw.
    pub index_count: u32,
    /// Number of vertices the slice spans.
    pub vertex_count: u32,
}

/// Stable per-object data in the object storage buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ObjectData {
    /// Model matrix (column-major).
    pub model: [[f32; 4]; 4],
    /// Previous-frame model matrix for motion vectors.
    pub prev_model: [[f32; 4]; 4],
    /// Material index into the material buffer.
    pub material_index: u32,
    /// Mesh slice index.
    pub mesh_index: u32,
    /// Skin palette index (`0xffff_ffff` = none).
    pub skin_index: u32,
    /// LOD bias.
    pub lod_bias: f32,
}

/// CPU-side mesh arena bookkeeping: the slices appended so far and the vertex and index
/// totals they span.
#[derive(Debug, Default)]
pub struct MeshArena {
    /// Total vertices allocated.
    pub vertex_count: u32,
    /// Total indices allocated.
    pub index_count: u32,
    /// Registered mesh slices.
    pub slices: Vec<MeshSlice>,
}

impl MeshArena {
    /// Appends a mesh slice and returns its index.
    pub fn push_slice(&mut self, slice: MeshSlice) -> u32 {
        let id = self.slices.len() as u32;
        self.slices.push(slice);
        self.vertex_count += slice.vertex_count;
        self.index_count += slice.index_count;
        id
    }
}

/// Best-fit free list over `0..capacity` units, coalescing on free.
#[derive(Debug)]
pub(crate) struct RangeFreeList {
    /// Free ranges by start: `start -> len`. Never adjacent (frees coalesce).
    free: BTreeMap<u32, u32>,
    capacity: u32,
    free_units: u32,
}

impl RangeFreeList {
    /// A list with the whole of `0..capacity` free.
    pub(crate) fn new(capacity: u32) -> Self {
        let mut free = BTreeMap::new();
        if capacity > 0 {
            free.insert(0, capacity);
        }
        Self {
            free,
            capacity,
            free_units: capacity,
        }
    }

    /// Takes `len` units from the smallest free range that holds them (its start). `None`
    /// when no free range is that long; `len` 0 takes nothing and returns `Some(0)`.
    pub(crate) fn alloc(&mut self, len: u32) -> Option<u32> {
        if len == 0 {
            return Some(0);
        }
        let (&start, &have) = self
            .free
            .iter()
            .filter(|&(_, &have)| have >= len)
            .min_by_key(|&(&start, &have)| (have, start))?;
        self.free.remove(&start);
        if have > len {
            self.free.insert(start + len, have - len);
        }
        self.free_units -= len;
        Some(start)
    }

    /// Returns `start..start + len` to the list, merging it with free neighbours.
    pub(crate) fn free(&mut self, start: u32, len: u32) {
        if len == 0 {
            return;
        }
        debug_assert!(start.checked_add(len).is_some_and(|end| end <= self.capacity));
        self.free_units += len;
        let mut start = start;
        let mut len = len;
        if let Some((&prev, &prev_len)) = self.free.range(..start).next_back()
            && prev + prev_len == start
        {
            self.free.remove(&prev);
            start = prev;
            len += prev_len;
        }
        if let Some(&next_len) = self.free.get(&(start + len)) {
            self.free.remove(&(start + len));
            len += next_len;
        }
        self.free.insert(start, len);
    }

    /// Units free in total.
    pub(crate) fn free_units(&self) -> u32 {
        self.free_units
    }

    /// Whether nothing is allocated.
    pub(crate) fn is_empty(&self) -> bool {
        self.free_units == self.capacity
    }

    /// Number of free ranges (fragmentation).
    pub(crate) fn free_ranges(&self) -> usize {
        self.free.len()
    }
}

/// A range of one arena chunk, in units (vertices or indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArenaRange {
    /// Chunk slot in the arena.
    pub chunk: u32,
    /// First unit.
    pub start: u32,
    /// Unit count.
    pub len: u32,
}

/// The extra usage of mesh buffers on a device with `features`: `BLAS_INPUT` where the device has
/// ray queries, so acceleration structures can be built from the arena (ADR-2035). wgpu refuses
/// the usage on any other device, and a default device carries none.
pub(crate) fn blas_input_usage(features: wgpu::Features) -> wgpu::BufferUsages {
    if features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY) {
        wgpu::BufferUsages::BLAS_INPUT
    } else {
        wgpu::BufferUsages::empty()
    }
}

/// Ranges whose lease was dropped, waiting for [`GpuArena::reclaim`].
type Released = Arc<Mutex<Vec<ArenaRange>>>;

/// Ownership of an [`ArenaRange`]: the range returns to its arena's free list when the lease
/// drops (at the arena's next [`GpuArena::alloc`] or [`GpuArena::reclaim`]).
#[derive(Debug)]
pub(crate) struct ArenaLease {
    range: ArenaRange,
    released: Released,
}

impl Drop for ArenaLease {
    fn drop(&mut self) {
        self.released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(self.range);
    }
}

struct ArenaChunk {
    buffer: wgpu::Buffer,
    ranges: RangeFreeList,
}

/// Chunk sizes of one [`GpuArena`], in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArenaSizes {
    /// The first chunk; each new chunk doubles the previous one up to [`Self::max_chunk`].
    pub first_chunk: u64,
    /// Largest chunk. An allocation over half of it gets its own buffer instead.
    pub max_chunk: u64,
}

impl ArenaSizes {
    /// Desktop: 2 MiB growing to 64 MiB chunks. Android: 1 MiB growing to 16 MiB.
    pub(crate) const fn for_platform(android: bool) -> Self {
        if android {
            Self {
                first_chunk: 1 << 20,
                max_chunk: 16 << 20,
            }
        } else {
            Self {
                first_chunk: 2 << 20,
                max_chunk: 64 << 20,
            }
        }
    }

    /// Clamped to the device's largest buffer.
    pub(crate) fn clamped(self, max_buffer_size: u64) -> Self {
        let max_chunk = self.max_chunk.min(max_buffer_size).max(4);
        Self {
            first_chunk: self.first_chunk.min(max_chunk),
            max_chunk,
        }
    }
}

/// Where an allocation landed.
pub(crate) struct ArenaAlloc {
    /// The chunk buffer (bind the whole buffer; offset with `base_vertex` / `first_index`).
    pub buffer: wgpu::Buffer,
    /// First unit in `buffer`.
    pub first: u32,
    /// The range's owner (`None`: `buffer` is a dedicated buffer the caller owns alone).
    pub lease: Option<Arc<ArenaLease>>,
    /// Buffers created for this allocation (a new chunk or a dedicated buffer): 0 or 1.
    pub created: u64,
}

/// One growable GPU arena of fixed-size units (`stride` bytes each), in chunks that never
/// move: a chunk's buffer handle stays valid for every range in it.
pub(crate) struct GpuArena {
    label: &'static str,
    usage: wgpu::BufferUsages,
    stride: u64,
    sizes: ArenaSizes,
    /// Chunk slots; `None` after [`Self::release_empty`] (slot indices stay stable).
    chunks: Vec<Option<ArenaChunk>>,
    /// Bytes of the next chunk.
    next_chunk: u64,
    released: Released,
}

impl GpuArena {
    /// An empty arena; the first chunk is created by the first [`Self::alloc`].
    pub(crate) fn new(
        label: &'static str,
        usage: wgpu::BufferUsages,
        stride: u64,
        sizes: ArenaSizes,
    ) -> Self {
        Self {
            label,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            stride: stride.max(1),
            sizes,
            chunks: Vec::new(),
            next_chunk: sizes.first_chunk,
            released: Arc::default(),
        }
    }

    /// Bytes per unit.
    pub(crate) fn stride(&self) -> u64 {
        self.stride
    }

    /// Returns dropped leases' ranges to their chunks' free lists.
    pub(crate) fn reclaim(&mut self) {
        let released = std::mem::take(&mut *self.released.lock().unwrap_or_else(PoisonError::into_inner));
        for range in released {
            if let Some(Some(chunk)) = self.chunks.get_mut(range.chunk as usize) {
                chunk.ranges.free(range.start, range.len);
            }
        }
    }

    /// `units` units: a range of an existing chunk when one has room, else a new chunk; an
    /// allocation over half of [`ArenaSizes::max_chunk`] gets a dedicated buffer.
    pub(crate) fn alloc(&mut self, device: &wgpu::Device, units: u32) -> ArenaAlloc {
        self.reclaim();
        let units = units.max(1);
        let bytes = u64::from(units) * self.stride;
        if bytes > self.sizes.max_chunk / 2 {
            let buffer = self.create(device, bytes);
            return ArenaAlloc {
                buffer,
                first: 0,
                lease: None,
                created: 1,
            };
        }
        let found = self.chunks.iter_mut().enumerate().find_map(|(slot, chunk)| {
            let chunk = chunk.as_mut()?;
            let first = chunk.ranges.alloc(units)?;
            Some((slot, first, chunk.buffer.clone()))
        });
        if let Some((slot, first, buffer)) = found {
            return ArenaAlloc {
                buffer,
                first,
                lease: Some(self.lease(slot, first, units)),
                created: 0,
            };
        }
        let chunk_bytes = self.next_chunk.max(bytes).min(self.sizes.max_chunk);
        self.next_chunk = (self.next_chunk.saturating_mul(2)).min(self.sizes.max_chunk);
        let capacity = u32::try_from(chunk_bytes / self.stride).unwrap_or(u32::MAX);
        let buffer = self.create(device, u64::from(capacity) * self.stride);
        let mut ranges = RangeFreeList::new(capacity);
        let first = ranges.alloc(units).unwrap_or(0);
        let chunk = ArenaChunk {
            buffer: buffer.clone(),
            ranges,
        };
        let slot = match self.chunks.iter().position(Option::is_none) {
            Some(slot) => {
                self.chunks[slot] = Some(chunk);
                slot
            }
            None => {
                self.chunks.push(Some(chunk));
                self.chunks.len() - 1
            }
        };
        ArenaAlloc {
            buffer,
            first,
            lease: Some(self.lease(slot, first, units)),
            created: 1,
        }
    }

    fn lease(&self, slot: usize, start: u32, len: u32) -> Arc<ArenaLease> {
        Arc::new(ArenaLease {
            range: ArenaRange {
                chunk: slot as u32,
                start,
                len,
            },
            released: Arc::clone(&self.released),
        })
    }

    fn create(&self, device: &wgpu::Device, bytes: u64) -> wgpu::Buffer {
        // `copy_buffer_to_buffer` / `write_buffer` sizes are multiples of 4.
        let size = bytes.max(4).next_multiple_of(4);
        crate::frame_stats::note_buffer(Some(self.label), size);
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size,
            usage: self.usage,
            mapped_at_creation: false,
        })
    }

    /// Drops chunks with nothing allocated (memory pressure). Returns the bytes freed.
    pub(crate) fn release_empty(&mut self) -> u64 {
        self.reclaim();
        let mut freed = 0;
        for slot in &mut self.chunks {
            if slot.as_ref().is_some_and(|chunk| chunk.ranges.is_empty()) {
                freed += slot.take().map_or(0, |chunk| chunk.buffer.size());
            }
        }
        if freed > 0 {
            self.next_chunk = self.sizes.first_chunk;
        }
        freed
    }

    /// Chunk count, bytes and free space.
    pub(crate) fn stats(&self) -> ArenaStats {
        let mut stats = ArenaStats::default();
        for chunk in self.chunks.iter().flatten() {
            stats.chunks += 1;
            stats.bytes += chunk.buffer.size();
            stats.free_bytes += u64::from(chunk.ranges.free_units()) * self.stride;
            stats.free_ranges += chunk.ranges.free_ranges() as u64;
        }
        stats
    }
}

/// Occupancy of one mesh arena ([`crate::FrameRenderer::mesh_arena_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaStats {
    /// Chunk buffers resident.
    pub chunks: u64,
    /// Bytes of those chunks.
    pub bytes: u64,
    /// Bytes of them not allocated to a mesh.
    pub free_bytes: u64,
    /// Free ranges (fragmentation: 1 per chunk is none).
    pub free_ranges: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `BLAS_INPUT` follows the device's ray queries and nothing else: wgpu refuses the usage on
    /// any other device.
    #[test]
    fn blas_input_only_on_a_ray_query_device() {
        assert_eq!(
            blas_input_usage(wgpu::Features::empty()),
            wgpu::BufferUsages::empty()
        );
        assert_eq!(
            blas_input_usage(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
            wgpu::BufferUsages::BLAS_INPUT
        );
    }

    #[test]
    fn push_slice_counts_vertices_and_indices_apart() {
        let mut arena = MeshArena::default();
        let a = arena.push_slice(MeshSlice {
            base_vertex: 0,
            first_index: 0,
            index_count: 36,
            vertex_count: 24,
        });
        let b = arena.push_slice(MeshSlice {
            base_vertex: 24,
            first_index: 36,
            index_count: 6,
            vertex_count: 4,
        });
        assert_eq!((a, b), (0, 1));
        assert_eq!(arena.vertex_count, 28);
        assert_eq!(arena.index_count, 42);
    }

    #[test]
    fn free_list_best_fit_and_coalesce() {
        let mut list = RangeFreeList::new(100);
        let a = list.alloc(10).expect("a");
        let b = list.alloc(20).expect("b");
        let c = list.alloc(30).expect("c");
        assert_eq!((a, b, c), (0, 10, 30));
        assert_eq!(list.free_units(), 40);
        list.free(b, 20);
        // Best fit: the 20-unit hole, not the 40-unit tail.
        assert_eq!(list.alloc(15), Some(10));
        list.free(10, 15);
        list.free(a, 10);
        // 0..30 is one range again (a + b coalesced), then c, then the tail.
        assert_eq!(list.free_ranges(), 2);
        list.free(c, 30);
        assert_eq!(list.free_ranges(), 1);
        assert!(list.is_empty());
        assert_eq!(list.alloc(100), Some(0));
        assert_eq!(list.alloc(1), None);
    }

    #[test]
    fn free_list_zero_and_full() {
        let mut list = RangeFreeList::new(8);
        assert_eq!(list.alloc(0), Some(0));
        assert_eq!(list.alloc(9), None);
        assert_eq!(list.alloc(8), Some(0));
        assert_eq!(list.free_units(), 0);
        list.free(0, 8);
        assert_eq!(list.free_units(), 8);
    }

    #[test]
    fn free_list_churn_keeps_totals() {
        // Allocate and free in a scrambled order; the list must end whole.
        let mut list = RangeFreeList::new(1000);
        let mut held = Vec::new();
        for i in 0..40u32 {
            let len = 1 + (i * 7) % 23;
            if let Some(start) = list.alloc(len) {
                held.push((start, len));
            }
            if i % 3 == 2 {
                let (start, len) = held.remove((i as usize * 5) % held.len());
                list.free(start, len);
            }
        }
        let used: u32 = held.iter().map(|&(_, len)| len).sum();
        assert_eq!(list.free_units() + used, 1000);
        // No two held ranges overlap.
        let mut sorted = held.clone();
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
        for (start, len) in held {
            list.free(start, len);
        }
        assert!(list.is_empty());
        assert_eq!(list.free_ranges(), 1);
    }

    #[test]
    fn sizes_clamp_to_the_device() {
        let sizes = ArenaSizes::for_platform(false).clamped(8 << 20);
        assert_eq!(sizes.max_chunk, 8 << 20);
        assert_eq!(sizes.first_chunk, 2 << 20);
        let android = ArenaSizes::for_platform(true);
        assert!(android.max_chunk < ArenaSizes::for_platform(false).max_chunk);
    }

    /// Spec 13 § 8: chunks start at 2 MiB and double to 64 MiB on desktop, 1 MiB to 16 MiB on
    /// Android, and a device with larger buffers leaves them as they are.
    #[test]
    fn chunk_sizes_match_spec_13_on_desktop_and_android() {
        let desktop = ArenaSizes::for_platform(false);
        assert_eq!((desktop.first_chunk, desktop.max_chunk), (2 << 20, 64 << 20));
        let android = ArenaSizes::for_platform(true);
        assert_eq!((android.first_chunk, android.max_chunk), (1 << 20, 16 << 20));
        assert_eq!(desktop.clamped(1 << 30), desktop);
        assert_eq!(android.clamped(1 << 30), android);
    }
}
