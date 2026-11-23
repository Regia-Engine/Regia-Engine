// SPDX-License-Identifier: MPL-2.0
//! One staging belt for a frame's buffer and texture uploads.
//!
//! `queue.write_buffer` / `queue.write_texture` each copy into wgpu's internal
//! staging belt. A steady frame that issues dozens of those calls makes
//! `queue.submit` record dozens of copies. This belt packs the bytes into one
//! GPU buffer per chunk and records `copy_buffer_to_buffer` / `copy_buffer_to_texture`
//! in the frame's command encoder. See the staging-belt ADR.
//!
//! **Mapped upload ring.** Each chunk is a `MAP_WRITE | COPY_SRC` buffer that stays mapped
//! while it is free: a write lands in the mapping directly (one copy, CPU to upload memory),
//! [`StagingBelt::upload`] unmaps the chunks the frame used before the submit, and the next
//! [`StagingBelt::begin_frame`] asks for them to be mapped again (`map_async`), which completes
//! once the GPU has run that submit. Until then the frame writes into another chunk. The ring
//! keeps [`RING_CHUNKS`] chunks (plus what [`StagingBelt::reserve`] asked for, and a chunk for a write
//! larger than all of them); a chunk needed beyond that, while the others are in flight (a GPU
//! backlog, a burst of texture-view frames), is transient: dropped after its submit, as
//! `queue.write_buffer`'s own staging is, so a burst keeps no upload memory (the BAR heap on some
//! GPUs). Before, every byte went into a CPU `Vec`
//! and then through `queue.write_buffer`'s staging copy. `REGIA_UPLOAD_RING=0` restores the
//! `Vec` + `write_buffer` chunks.
//!
//! **Spill chunk.** Once the ring holds [`RING_CHUNKS`] chunks, the belt also keeps one
//! [`MIN_CHUNK`] CPU chunk uploaded with `queue.write_buffer` (the `REGIA_UPLOAD_RING=0` path).
//! A write that finds every ring chunk in flight goes there before it would create a transient
//! chunk, so a GPU a frame or two behind (a GPU-bound or shared GPU, back-to-back texture-view
//! submits) creates no buffer per frame; only a burst past the spill's room is transient
//! (ADR-358). The spill is never mapped (no BAR heap) and is not counted by [`StagingBelt::reserve`],
//! [`StagingBelt::has_room`] or [`StagingBelt::chunk_counts`].
//!
//! **Warm-up.** A renderer that reserved instances makes the ring and the spill with
//! [`StagingBelt::warm_ring`] on its warm-up, whether or not a warm-up frame found a chunk in flight
//! (ADR-3189).

use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Byte alignment of `copy_buffer_to_buffer` (wgpu `COPY_BUFFER_ALIGNMENT`).
const BUFFER_ALIGN: u64 = 4;
/// Row and offset alignment of `copy_buffer_to_texture`.
const ROW_ALIGN: u32 = 256;
/// Smallest GPU chunk. A larger write gets its own power-of-two chunk.
const MIN_CHUNK: u64 = 1024 * 1024;

/// Chunks the mapped ring keeps (one the GPU reads, one the frame writes); more are transient.
pub(crate) const RING_CHUNKS: usize = 2;

/// Environment variable: `0`/`off` makes the belt's chunks CPU `Vec`s uploaded with
/// `queue.write_buffer` (the belt before the mapped ring).
pub(crate) const UPLOAD_RING_ENV: &str = "REGIA_UPLOAD_RING";

/// Map states of a ring chunk, kept in [`Backing::Mapped::remap`] by the `map_async` callback.
const REMAP_PENDING: u8 = 0;
const REMAP_DONE: u8 = 1;
const REMAP_FAILED: u8 = 2;

fn upload_ring_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !regia_core::brand::env_var(UPLOAD_RING_ENV).is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false"))
    })
}

/// Where a chunk's bytes are written.
enum Backing {
    /// The mapped ring: `view` is the whole chunk while it is mapped and free to write.
    Mapped {
        view: Option<wgpu::BufferViewMut>,
        /// `true` from [`StagingBelt::upload`] (unmapped, the GPU reads it) until
        /// [`StagingBelt::begin_frame`] requests the remap.
        in_flight: bool,
        /// Set by the remap's callback; `None` when no remap is outstanding.
        remap: Option<Arc<AtomicU8>>,
    },
    /// CPU bytes uploaded with one `queue.write_buffer` (`REGIA_UPLOAD_RING=0`).
    Cpu(Vec<u8>),
}

struct Chunk {
    buffer: wgpu::Buffer,
    capacity: u64,
    /// Bytes occupied this frame. Reset by [`StagingBelt::begin_frame`].
    used: u64,
    backing: Backing,
    /// Dropped after its submit instead of mapped again (beyond [`RING_CHUNKS`]).
    transient: bool,
    /// Dropped at the next [`StagingBelt::begin_frame`] (its remap failed: device lost).
    dead: bool,
    /// The belt's spill chunk: CPU bytes, taken only when no ring chunk has room.
    spill: bool,
}

impl Chunk {
    fn new(device: &wgpu::Device, capacity: u64) -> Self {
        crate::frame_stats::note_buffer(Some("staging belt"), capacity);
        if upload_ring_enabled() {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("staging belt"),
                size: capacity,
                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            });
            match buffer.get_mapped_range_mut(..) {
                Ok(view) => {
                    return Self {
                        buffer,
                        capacity,
                        used: 0,
                        backing: Backing::Mapped {
                            view: Some(view),
                            in_flight: false,
                            remap: None,
                        },
                        transient: false,
                        dead: false,
                        spill: false,
                    };
                }
                Err(e) => {
                    tracing::warn!(target: "regia_render", "staging belt chunk did not map ({e}); using write_buffer");
                }
            }
        }
        Self::cpu(device, capacity, "staging belt")
    }

    /// A chunk of CPU bytes uploaded with one `queue.write_buffer` per frame. The caller counts
    /// the buffer.
    fn cpu(device: &wgpu::Device, capacity: u64, label: &str) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            capacity,
            used: 0,
            backing: Backing::Cpu(Vec::with_capacity(capacity.min(8 * 1024 * 1024) as usize)),
            transient: false,
            dead: false,
            spill: false,
        }
    }

    /// The spill chunk ([`MIN_CHUNK`] of GPU buffer; see the module docs). Its CPU bytes start
    /// empty and grow on the first write that spills (ADR-3351): most apps never spill, and the
    /// warm ring makes the spill up front, so a preallocated `Vec` was a megabyte of heap held for
    /// nothing. Once a frame spilled, the capacity stays for the next one.
    fn spill(device: &wgpu::Device) -> Self {
        crate::frame_stats::note_buffer(Some("staging belt spill"), MIN_CHUNK);
        let mut chunk = Self::cpu(device, MIN_CHUNK, "staging belt spill");
        chunk.backing = Backing::Cpu(Vec::new());
        chunk.spill = true;
        chunk
    }

    /// A ring chunk: neither transient nor the spill.
    fn in_ring(&self) -> bool {
        !self.transient && !self.spill
    }

    /// Whether bytes can be written now (a mapped chunk only while its view is held).
    fn writable(&self) -> bool {
        match &self.backing {
            Backing::Mapped { view, .. } => view.is_some(),
            Backing::Cpu(_) => true,
        }
    }

    /// Requests the remap of a chunk the last upload unmapped (its submit has been made).
    fn request_remap(&mut self) {
        let Backing::Mapped {
            view: None,
            in_flight,
            remap,
        } = &mut self.backing
        else {
            return;
        };
        if !*in_flight {
            return;
        }
        *in_flight = false;
        if self.transient {
            // wgpu keeps the buffer alive until the GPU ran the submit that reads it.
            self.dead = true;
            return;
        }
        let flag = Arc::new(AtomicU8::new(REMAP_PENDING));
        let done = Arc::clone(&flag);
        self.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Write, move |result| {
                done.store(
                    if result.is_ok() { REMAP_DONE } else { REMAP_FAILED },
                    Ordering::Release,
                );
            });
        *remap = Some(flag);
    }

    /// Picks up a finished remap: the chunk is free to write again.
    fn collect_remap(&mut self) {
        let Backing::Mapped { view, remap, .. } = &mut self.backing else {
            return;
        };
        let state = remap.as_ref().map(|r| r.load(Ordering::Acquire));
        match state {
            Some(REMAP_DONE) => match self.buffer.get_mapped_range_mut(..) {
                Ok(mapped) => {
                    *view = Some(mapped);
                    *remap = None;
                    self.used = 0;
                }
                Err(_) => self.dead = true,
            },
            Some(REMAP_FAILED) => self.dead = true,
            _ => {}
        }
    }
}

enum Pending {
    Buffer {
        chunk: usize,
        src_offset: u64,
        size: u64,
        dst: wgpu::Buffer,
        dst_offset: u64,
    },
    #[cfg_attr(not(test), allow(dead_code))]
    Texture {
        chunk: usize,
        src_offset: u64,
        bytes_per_row: u32,
        texture: wgpu::Texture,
        mip: u32,
        origin: wgpu::Origin3d,
        width: u32,
        height: u32,
    },
    /// Residency feedback: copy `words` into `copy_to` (when the ring had a slot), then clear
    /// `words` from `clear_offset`. No staging chunk; the buffers already exist.
    Feedback {
        words: wgpu::Buffer,
        copy_to: Option<wgpu::Buffer>,
        bytes: u64,
        clear_offset: u64,
    },
}

/// Packs a frame's uploads. Create one per renderer and call [`Self::begin_frame`]
/// at the start of each frame.
#[derive(Default)]
pub(crate) struct StagingBelt {
    chunks: Vec<Chunk>,
    pending: Vec<Pending>,
}

impl StagingBelt {
    /// Makes one ring chunk of at least `bytes` resident (level load), so a frame that uploads that
    /// much creates no chunk on the frame path. One chunk, because the frame's largest write is most
    /// of its bytes (one instance list, up to 400 B per instance): smaller chunks that only add up
    /// to `bytes` could not hold it. Chunks already in the ring stay, so raising a reservation in
    /// steps keeps at most about twice the last one.
    pub(crate) fn reserve(&mut self, device: &wgpu::Device, bytes: u64) {
        let have = self
            .chunks
            .iter()
            .filter(|c| c.in_ring())
            .map(|c| c.capacity)
            .max()
            .unwrap_or(0);
        if have >= bytes {
            return;
        }
        self.chunks
            .push(Chunk::new(device, bytes.next_power_of_two().max(MIN_CHUNK)));
        self.spill_once_full(device);
    }

    /// [`Self::reserve`], plus the rest of the ring and the spill: the belt's warm-up state, made
    /// off the frame path (`specs/27` § 2.4). Otherwise the ring only fills on the first frame that
    /// finds a chunk in flight, which depends on how far behind the GPU happens to be: a warm-up on
    /// a GPU that keeps up (a fast card, a loaded CPU) leaves one chunk, and the next frame that
    /// lags or overflows it creates the second chunk and the spill in the middle of play.
    pub(crate) fn warm_ring(&mut self, device: &wgpu::Device, bytes: u64) {
        self.reserve(device, bytes);
        if !upload_ring_enabled() {
            // Every chunk is CPU bytes, always free to write: nothing is ever in flight.
            return;
        }
        while self.chunks.iter().filter(|c| c.in_ring()).count() < RING_CHUNKS {
            self.chunks.push(Chunk::new(device, MIN_CHUNK));
        }
        self.spill_once_full(device);
    }

    /// Whether `bytes` more fit in the chunks free to write now (a write of that size would not
    /// create a chunk). A ring chunk the GPU still reads (unmapped, remap pending) has no room:
    /// a write would land in a new chunk instead. The spill chunk does not count: it is kept for
    /// the frame's small writes, and a mesh that finds no ring room uses `write_buffer` anyway.
    pub(crate) fn has_room(&self, bytes: u64) -> bool {
        let size = align_up(bytes, BUFFER_ALIGN);
        self.chunks.iter().any(|chunk| {
            !chunk.spill && chunk.writable() && align_up(chunk.used, BUFFER_ALIGN) + size <= chunk.capacity
        })
    }

    /// Starts a frame: chunks the last frame uploaded are asked to map again (they are free
    /// once the GPU ran that submit), free ones are reset, transient chunks are dropped.
    ///
    /// Call it after the previous frame's submit (a remap requested before the submit that
    /// reads the chunk would fail that submit's validation).
    pub(crate) fn begin_frame(&mut self) {
        self.pending.clear();
        for chunk in &mut self.chunks {
            match &mut chunk.backing {
                Backing::Cpu(cpu) => {
                    cpu.clear();
                    chunk.used = 0;
                }
                Backing::Mapped { view: Some(_), .. } => chunk.used = 0,
                Backing::Mapped { view: None, .. } => {
                    chunk.request_remap();
                    chunk.collect_remap();
                }
            }
        }
        // A transient chunk this frame did not upload is free: drop it too.
        self.chunks.retain(|c| !c.dead && !(c.transient && c.writable()));
    }

    /// Requests the remap of the chunks [`Self::upload`] unmapped. Call it right after the
    /// submit that reads them, so the chunks are free again as soon as the GPU ran it (the
    /// remap completes on a later device poll). [`Self::begin_frame`] requests any left.
    pub(crate) fn after_submit(&mut self) {
        for chunk in &mut self.chunks {
            chunk.request_remap();
        }
    }

    /// Chunks in the ring (with transient ones, without the spill), and how many are free to
    /// write now (tests, diagnostics).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn chunk_counts(&self) -> (usize, usize) {
        let chunks = || self.chunks.iter().filter(|c| !c.spill);
        (chunks().count(), chunks().filter(|c| c.writable()).count())
    }

    /// Bytes the frame wrote, over every chunk (tests). Call it before the next frame's
    /// [`Self::begin_frame`], on a frame that found no chunk in flight (a chunk still in flight
    /// keeps the count of the frame before).
    #[cfg(test)]
    pub(crate) fn frame_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.used).sum()
    }

    /// Ring chunks whose remap was requested and whose callback has not run yet (tests).
    #[cfg(test)]
    fn remaps_pending(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| {
                matches!(&c.backing, Backing::Mapped { remap: Some(flag), .. }
                    if flag.load(Ordering::Acquire) == REMAP_PENDING)
            })
            .count()
    }

    /// Bytes in the spill chunk this frame; `None` while the belt has no spill (tests).
    #[cfg(test)]
    fn spill_used(&self) -> Option<u64> {
        self.chunks.iter().find(|c| c.spill).map(|c| c.used)
    }

    /// CPU bytes the spill chunk holds (its `Vec`'s capacity); `None` while the belt has no
    /// spill (tests).
    #[cfg(test)]
    fn spill_heap_bytes(&self) -> Option<usize> {
        self.chunks.iter().find(|c| c.spill).map(|c| match &c.backing {
            Backing::Cpu(cpu) => cpu.capacity(),
            Backing::Mapped { .. } => 0,
        })
    }

    /// `true` when every deferred copy has been recorded into an encoder.
    pub(crate) fn pending_is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Remembers a buffer write. [`Self::flush`] records the copy.
    ///
    /// `data.len()` must be a multiple of 4. The destination buffer must be at
    /// least that long and have `COPY_DST`.
    pub(crate) fn defer(&mut self, device: &wgpu::Device, dst: &wgpu::Buffer, dst_offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let (chunk, src_offset, size) = self.alloc(device, data, BUFFER_ALIGN);
        self.pending.push(Pending::Buffer {
            chunk,
            src_offset,
            size,
            dst: dst.clone(),
            dst_offset,
        });
    }

    /// Remembers a 2D RGBA8 upload. Rows are padded to 256 bytes.
    ///
    /// # Errors
    ///
    /// The padded staging size does not fit the belt's `u32`-sized uploads.
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn defer_texture(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::Texture,
        mip: u32,
        origin: wgpu::Origin3d,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), crate::RenderError> {
        let (chunk, src_offset, bytes_per_row) = self.alloc_rgba(device, rgba, width, height)?;
        self.pending.push(Pending::Texture {
            chunk,
            src_offset,
            bytes_per_row,
            texture: texture.clone(),
            mip,
            origin,
            width,
            height,
        });
        Ok(())
    }

    /// Queues a residency-feedback copy and clear for [`Self::flush`].
    ///
    /// The frame calls this before the encoder exists. `flush` records the copy, then the
    /// clear, ahead of any pass that writes the feedback words.
    pub(crate) fn defer_feedback(&mut self, op: crate::residency_feedback::FeedbackCopy) {
        self.pending.push(Pending::Feedback {
            words: op.words,
            copy_to: op.copy_to,
            bytes: op.bytes,
            clear_offset: op.clear_offset,
        });
    }

    /// Records every deferred copy into `encoder`. Call this once the encoder
    /// exists and before any pass that reads the destinations.
    pub(crate) fn flush(&mut self, encoder: &mut wgpu::CommandEncoder) {
        // Drained in place: the list keeps its room for the next frame's writes, and the chunk
        // buffers are borrowed, not cloned, per copy (ADR-385).
        let chunks = &self.chunks;
        for op in self.pending.drain(..) {
            match op {
                Pending::Buffer {
                    chunk,
                    src_offset,
                    size,
                    dst,
                    dst_offset,
                } => {
                    let src = &chunks[chunk].buffer;
                    encoder.copy_buffer_to_buffer(src, src_offset, &dst, dst_offset, size);
                }
                Pending::Texture {
                    chunk,
                    src_offset,
                    bytes_per_row,
                    texture,
                    mip,
                    origin,
                    width,
                    height,
                } => {
                    let src = &chunks[chunk].buffer;
                    copy_rgba(
                        encoder,
                        src,
                        src_offset,
                        bytes_per_row,
                        &texture,
                        mip,
                        origin,
                        width,
                        height,
                    );
                }
                Pending::Feedback {
                    words,
                    copy_to,
                    bytes,
                    clear_offset,
                } => {
                    if let Some(dst) = &copy_to {
                        encoder.copy_buffer_to_buffer(&words, 0, dst, 0, bytes);
                    }
                    encoder.clear_buffer(&words, clear_offset, None);
                }
            }
        }
    }

    /// Records a buffer copy immediately. Use this after [`Self::flush`], while
    /// the encoder is open and before the pass that reads `dst`.
    pub(crate) fn write(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        dst: &wgpu::Buffer,
        dst_offset: u64,
        data: &[u8],
    ) {
        if data.is_empty() {
            return;
        }
        let (chunk, src_offset, size) = self.alloc(device, data, BUFFER_ALIGN);
        let src = self.chunks[chunk].buffer.clone();
        encoder.copy_buffer_to_buffer(&src, src_offset, dst, dst_offset, size);
    }

    /// Records an RGBA8 texture copy immediately.
    ///
    /// # Errors
    ///
    /// The padded staging size does not fit the belt's `u32`-sized uploads.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_texture(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        mip: u32,
        origin: wgpu::Origin3d,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), crate::RenderError> {
        let (chunk, src_offset, bytes_per_row) = self.alloc_rgba(device, rgba, width, height)?;
        let src = self.chunks[chunk].buffer.clone();
        copy_rgba(
            encoder,
            &src,
            src_offset,
            bytes_per_row,
            texture,
            mip,
            origin,
            width,
            height,
        );
        Ok(())
    }

    /// Writes `bytes` through [`Self::write`] when they differ from `cache`.
    pub(crate) fn write_if_changed(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        buffer: &wgpu::Buffer,
        bytes: &[u8],
        cache: &mut Vec<u8>,
    ) -> bool {
        if cache.as_slice() == bytes {
            return false;
        }
        self.write(device, encoder, buffer, 0, bytes);
        cache.clear();
        cache.extend_from_slice(bytes);
        true
    }

    /// Makes this frame's bytes visible to the GPU before the submit: unmaps every ring chunk
    /// the frame used (one `queue.write_buffer` per used chunk with `REGIA_UPLOAD_RING=0`).
    ///
    /// Not a submit. The copies recorded from these chunks must be in the next
    /// [`wgpu::Queue::submit`].
    pub(crate) fn upload(&mut self, queue: &crate::device::TrackedQueue) {
        for chunk in &mut self.chunks {
            if chunk.used == 0 {
                continue;
            }
            let end = chunk.used as usize;
            crate::frame_stats::note_belt_upload(end as u64);
            match &mut chunk.backing {
                Backing::Cpu(cpu) => queue.write_buffer(&chunk.buffer, 0, &cpu[..end]),
                Backing::Mapped { view, in_flight, .. } => {
                    if view.take().is_some() {
                        chunk.buffer.unmap();
                        *in_flight = true;
                    }
                }
            }
        }
    }

    fn alloc(&mut self, device: &wgpu::Device, data: &[u8], align: u64) -> (usize, u64, u64) {
        let size = align_up(data.len() as u64, BUFFER_ALIGN);
        let align = align.max(BUFFER_ALIGN);
        // Ring (and transient) chunks first, the spill only when none of them has room.
        for spill in [false, true] {
            for (index, chunk) in self.chunks.iter_mut().enumerate() {
                if chunk.spill != spill {
                    continue;
                }
                chunk.collect_remap();
                if !chunk.writable() {
                    continue;
                }
                let offset = align_up(chunk.used, align);
                if offset + size <= chunk.capacity {
                    place(chunk, offset, data, size);
                    return (index, offset, size);
                }
            }
        }
        let cap = (size + align).next_power_of_two().max(MIN_CHUNK);
        // A write no ring chunk could hold even when free is not a backlog: the ring is too small for
        // what the frame uploads, and a transient chunk would be made for it on every frame. It joins
        // the ring instead. Each one is larger than all before it, so the ring stays small.
        let outgrown = !self.chunks.iter().any(|c| c.in_ring() && c.capacity >= size);
        let mut chunk = Chunk::new(device, cap);
        chunk.transient = matches!(chunk.backing, Backing::Mapped { .. })
            && !outgrown
            && self.chunks.iter().filter(|c| c.in_ring()).count() >= RING_CHUNKS;
        place(&mut chunk, 0, data, size);
        self.chunks.push(chunk);
        let index = self.chunks.len() - 1;
        self.spill_once_full(device);
        (index, 0, size)
    }

    /// Makes the spill chunk once the belt holds [`RING_CHUNKS`] mapped chunks: the GPU was a
    /// frame behind, so a later frame may find every ring chunk in flight. Made with the ring
    /// (warm-up), not on the frame that first needs it.
    fn spill_once_full(&mut self, device: &wgpu::Device) {
        let mapped = self
            .chunks
            .iter()
            .filter(|c| !c.spill && matches!(c.backing, Backing::Mapped { .. }))
            .count();
        if mapped >= RING_CHUNKS && !self.chunks.iter().any(|c| c.spill) {
            self.chunks.push(Chunk::spill(device));
        }
    }

    /// Returns `(chunk, offset, bytes_per_row)` after padding rows to 256.
    ///
    /// # Errors
    ///
    /// The padded byte count does not fit the belt's `u32`-sized uploads.
    fn alloc_rgba(
        &mut self,
        device: &wgpu::Device,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(usize, u64, u32), crate::RenderError> {
        let width = width.max(1);
        let height = height.max(1);
        // The padded size used to be computed in u32 and wrapped to 0 at 4 GiB (a
        // 32768 × 32768 RGBA8 upload), and the slice copies then panicked.
        let row = u64::from(width) * 4;
        let aligned = row.div_ceil(u64::from(ROW_ALIGN)) * u64::from(ROW_ALIGN);
        let padded_bytes = aligned * u64::from(height);
        if padded_bytes > u64::from(u32::MAX) {
            return Err(crate::RenderError::UploadTooLarge(format!(
                "{width}x{height} RGBA8 upload pads to {padded_bytes} bytes"
            )));
        }
        let row = row as usize;
        let aligned = aligned as u32;
        let mut padded = vec![0u8; padded_bytes as usize];
        let tight = row * (height as usize);
        if rgba.len() >= tight && u64::from(aligned) == row as u64 {
            padded[..tight].copy_from_slice(&rgba[..tight]);
        } else {
            for y in 0..height as usize {
                let src = y * row;
                let dst = y * aligned as usize;
                let n = row;
                if src + n <= rgba.len() {
                    padded[dst..dst + n].copy_from_slice(&rgba[src..src + n]);
                }
            }
        }
        let (chunk, offset, _) = self.alloc(device, &padded, u64::from(ROW_ALIGN));
        Ok((chunk, offset, aligned))
    }
}

/// Something [`crate::gpu_driven::GpuCuller::upload`] can write uniforms into.
///
/// `&wgpu::Queue` writes immediately (tests, one-shot readback). [`LiveBelt`]
/// records a copy in the frame encoder so the steady path uploads once.
pub(crate) trait BufferSink {
    fn write_buffer(&mut self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]);
}

impl BufferSink for &wgpu::Queue {
    fn write_buffer(&mut self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        wgpu::Queue::write_buffer(self, buffer, offset, data);
    }
}

/// Belt writes that record into the open frame encoder.
pub(crate) struct LiveBelt<'a> {
    /// Destination belt.
    pub belt: &'a mut StagingBelt,
    /// Device that owns the belt's chunks.
    pub device: &'a wgpu::Device,
    /// Encoder the copies are recorded into.
    pub encoder: &'a mut wgpu::CommandEncoder,
}

impl BufferSink for LiveBelt<'_> {
    fn write_buffer(&mut self, buffer: &wgpu::Buffer, offset: u64, data: &[u8]) {
        self.belt.write(self.device, self.encoder, buffer, offset, data);
    }
}

/// Writes `data` at `offset` of a writable chunk, zero-padded to `size`.
fn place(chunk: &mut Chunk, offset: u64, data: &[u8], size: u64) {
    let (start, end) = (offset as usize, (offset + size) as usize);
    match &mut chunk.backing {
        Backing::Cpu(cpu) => {
            cpu.resize(start, 0);
            cpu.extend_from_slice(data);
            cpu.resize(end, 0);
        }
        Backing::Mapped { view: Some(view), .. } => {
            view.slice(start..start + data.len()).copy_from_slice(data);
            if start + data.len() < end {
                view.slice(start + data.len()..end).fill(0);
            }
        }
        // `alloc` only places into writable chunks.
        Backing::Mapped { view: None, .. } => return,
    }
    chunk.used = offset + size;
}

fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

#[allow(clippy::too_many_arguments)]
fn copy_rgba(
    encoder: &mut wgpu::CommandEncoder,
    src: &wgpu::Buffer,
    src_offset: u64,
    bytes_per_row: u32,
    texture: &wgpu::Texture,
    mip: u32,
    origin: wgpu::Origin3d,
    width: u32,
    height: u32,
) {
    encoder.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: src,
            layout: wgpu::TexelCopyBufferLayout {
                offset: src_offset,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: mip,
            origin,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Skip a write when `cache` already holds `bytes`, then update `cache`.
pub(crate) fn defer_if_changed(
    belt: &mut StagingBelt,
    device: &wgpu::Device,
    cache: &mut Vec<u8>,
    buffer: &wgpu::Buffer,
    bytes: &[u8],
) -> bool {
    if cache.as_slice() == bytes {
        return false;
    }
    belt.defer(device, buffer, 0, bytes);
    cache.clear();
    cache.extend_from_slice(bytes);
    true
}

/// The 4-byte-aligned byte range where `bytes` differs from `cache`: `None` when they
/// match, all of `bytes` when the lengths differ. A buffer holding `cache` that receives
/// this range of `bytes` then holds `bytes`.
pub(crate) fn changed_range(cache: &[u8], bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    if cache.len() != bytes.len() {
        return (!bytes.is_empty()).then_some(0..bytes.len());
    }
    if cache == bytes {
        return None;
    }
    // Block-wise memcmp from both ends, then bytes inside the first differing block.
    const BLOCK: usize = 256;
    let len = bytes.len();
    let mut first = 0;
    while first + BLOCK <= len && cache[first..first + BLOCK] == bytes[first..first + BLOCK] {
        first += BLOCK;
    }
    while first < len && cache[first] == bytes[first] {
        first += 1;
    }
    let mut end = len;
    while end >= first + BLOCK && cache[end - BLOCK..end] == bytes[end - BLOCK..end] {
        end -= BLOCK;
    }
    while end > first && cache[end - 1] == bytes[end - 1] {
        end -= 1;
    }
    let align = BUFFER_ALIGN as usize;
    let start = first / align * align;
    let end = (end.div_ceil(align) * align).min(len);
    Some(start..end)
}

/// Writes through [`StagingBelt::write`] only the bytes of `bytes` that differ from
/// `cache` (the buffer's current contents), then updates `cache`. Returns the bytes
/// written.
pub(crate) fn write_changed(
    belt: &mut StagingBelt,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    buffer: &wgpu::Buffer,
    bytes: &[u8],
    cache: &mut Vec<u8>,
) -> usize {
    let Some(range) = changed_range(cache, bytes) else {
        return 0;
    };
    belt.write(device, encoder, buffer, range.start as u64, &bytes[range.clone()]);
    if cache.len() == bytes.len() {
        cache[range.clone()].copy_from_slice(&bytes[range.clone()]);
    } else {
        cache.clear();
        cache.extend_from_slice(bytes);
    }
    range.len()
}

/// [`write_changed`] through any [`BufferSink`].
pub(crate) fn sink_write_changed(
    sink: &mut dyn BufferSink,
    buffer: &wgpu::Buffer,
    bytes: &[u8],
    cache: &mut Vec<u8>,
) -> usize {
    let Some(range) = changed_range(cache, bytes) else {
        return 0;
    };
    sink.write_buffer(buffer, range.start as u64, &bytes[range.clone()]);
    if cache.len() == bytes.len() {
        cache[range.clone()].copy_from_slice(&bytes[range.clone()]);
    } else {
        cache.clear();
        cache.extend_from_slice(bytes);
    }
    range.len()
}

/// [`write_changed`] through [`StagingBelt::defer`] (before the frame encoder exists).
pub(crate) fn defer_changed(
    belt: &mut StagingBelt,
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    bytes: &[u8],
    cache: &mut Vec<u8>,
) -> usize {
    let Some(range) = changed_range(cache, bytes) else {
        return 0;
    };
    belt.defer(device, buffer, range.start as u64, &bytes[range.clone()]);
    if cache.len() == bytes.len() {
        cache[range.clone()].copy_from_slice(&bytes[range.clone()]);
    } else {
        cache.clear();
        cache.extend_from_slice(bytes);
    }
    range.len()
}

/// `size_of` helper kept so a padding change fails a unit test rather than a GPU one.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn buffer_align() -> u64 {
    let _ = size_of::<u32>();
    BUFFER_ALIGN
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upper bound on the device polls [`settle`] makes while a remap callback is still to run.
    const SETTLE_POLLS: u32 = 100_000;

    /// Waits until the GPU ran every submit and every remap callback `belt` asked for has run
    /// (ADR-9604). `wait_indefinitely` returns once the submits are done, but wgpu runs a map
    /// callback in the thread whose poll collected it, after that poll lets go of the device: on
    /// the process's shared test device another test's poll can collect this belt's remap and run
    /// it a moment after this poll returned (`chunks_beyond_the_ring_are_transient` read (2, 1)
    /// once on an RTX 5090 in the pre-gate's batch run). Bounded by a count, not a clock; the
    /// test's own assertion still fails when a remap never runs.
    fn settle(device: &crate::RenderDevice, belt: &StagingBelt) {
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        for _ in 0..SETTLE_POLLS {
            if belt.remaps_pending() == 0 {
                return;
            }
            std::thread::yield_now();
            let _ = device.tracked().poll(wgpu::PollType::Poll);
        }
    }

    /// The device of the staging tests whose maps must complete only on their own submits and
    /// polls, and the lock that gives one of them the device at a time.
    ///
    /// A poll on the shared test device by another test's thread could finish a remap between a
    /// test's write and its check, so these tests do not use `crate::test_device`. They used to
    /// open a device each, but on an NVIDIA adapter every device a unit test opens is kept until
    /// exit (ADR-1584), and the lib binary's devices ran past what the RTX 5090 creates in one
    /// process (`request_device` failed with "Parent device is lost" for the last ones). One
    /// device serves them in turn: no other test polls it, and each test waits for its GPU work
    /// before it hands the device on.
    fn isolated_device() -> (std::sync::MutexGuard<'static, ()>, crate::RenderDevice) {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static DEVICE: std::sync::OnceLock<crate::RenderDevice> = std::sync::OnceLock::new();
        // A test that panicked while it held the turn leaves the device as usable as any other.
        let turn = TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let device = DEVICE
            .get_or_init(|| crate::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)"))
            .clone();
        (turn, device)
    }

    #[test]
    fn changed_range_covers_exactly_the_differing_words() {
        let a: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(changed_range(&a, &a), None);
        let mut b = a.clone();
        b[1001] ^= 1;
        b[1302] ^= 1;
        assert_eq!(changed_range(&a, &b), Some(1000..1304));
        b[0] ^= 1;
        b[1999] ^= 1;
        assert_eq!(changed_range(&a, &b), Some(0..2000));
        assert_eq!(changed_range(&a[..8], &b[..12]), Some(0..12));
        assert_eq!(changed_range(&a[..8], &[]), None);
        // Applying the range to a copy of `a` reproduces `b`.
        let mut c = a.clone();
        b[7] ^= 2;
        let range = changed_range(&a, &b).expect("differs");
        c[range.clone()].copy_from_slice(&b[range]);
        assert_eq!(c, b);
    }

    #[test]
    fn a_reserved_belt_takes_a_frame_of_that_size_without_a_new_chunk() {
        let device = crate::test_device();
        let mut belt = StagingBelt::default();
        belt.reserve(&device.device, 3 * 1024 * 1024);
        assert_eq!(belt.chunks.len(), 1);
        let have = belt.chunks[0].capacity;
        assert!(have >= 3 * 1024 * 1024);
        // Reserving less than what is resident adds nothing.
        belt.reserve(&device.device, 1024);
        assert_eq!(belt.chunks.len(), 1);
        belt.begin_frame();
        let bytes = vec![7u8; 1024 * 1024];
        for _ in 0..3 {
            let _ = belt.alloc(&device.device, &bytes, 4);
        }
        assert_eq!(belt.chunks.len(), 1, "a reserved frame grew the belt");
    }

    /// The warm ring is the whole ring and the spill, once; a reservation past its room still adds
    /// its chunk, and a later call with nothing missing adds nothing.
    #[test]
    fn warm_ring_makes_the_ring_and_the_spill_once() {
        if !upload_ring_enabled() {
            return;
        }
        let device = crate::test_device();
        let mut belt = StagingBelt::default();
        belt.warm_ring(&device.device, 1024);
        assert_eq!(belt.chunk_counts(), (RING_CHUNKS, RING_CHUNKS));
        assert_eq!(belt.spill_used(), Some(0), "the spill is made with the ring");
        assert_eq!(
            belt.spill_heap_bytes(),
            Some(0),
            "the spill holds no CPU bytes until a write spills (ADR-3351)"
        );
        let chunks = belt.chunks.len();
        belt.warm_ring(&device.device, 1024);
        assert_eq!(belt.chunks.len(), chunks, "nothing was missing");
        belt.warm_ring(&device.device, 8 * 1024 * 1024);
        assert_eq!(belt.chunk_counts(), (RING_CHUNKS + 1, RING_CHUNKS + 1));
        assert_eq!(
            belt.chunks.iter().filter(|c| c.spill).count(),
            1,
            "still one spill"
        );
    }

    /// A warmed ring takes frames the GPU is behind on without a new chunk: however many of the
    /// earlier chunks are still in flight, a small frame lands in another ring chunk or the spill.
    #[test]
    fn a_warmed_ring_creates_nothing_for_frames_behind() {
        if !upload_ring_enabled() {
            return;
        }
        // The isolated device, like `a_chunk_in_flight_is_not_overwritten`: no other test's poll
        // finishes a remap here, so the chunks stay in flight as long as this GPU needs.
        let (_turn, device) = isolated_device();
        let dst = target(&device);
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.warm_ring(&device.device, MIN_CHUNK);
        let chunks = belt.chunks.len();
        for frame in 0..6u8 {
            belt.begin_frame();
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            belt.write(&device.device, &mut encoder, &dst, 0, &[frame + 1; 64]);
            belt.upload(&queue);
            device.queue.submit([encoder.finish()]);
            belt.after_submit();
            assert_eq!(belt.chunks.len(), chunks, "frame {frame} created a chunk");
        }
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        assert_eq!(
            read_back(&device, &dst, 64),
            vec![6u8; 64],
            "the last frame's bytes"
        );
    }

    /// A level raises what the renderer's first frames reserved: the room must be one chunk, because
    /// the frame's instance list is one write. Two 1 MiB chunks add up to a 1.92 MB reservation and
    /// hold none of a 1.2 MB list, which then made a transient chunk on every frame.
    #[test]
    fn a_raised_reservation_makes_one_chunk_that_holds_it() {
        if !upload_ring_enabled() {
            return;
        }
        let device = crate::test_device();
        let mut belt = StagingBelt::default();
        belt.warm_ring(&device.device, 655_360);
        assert_eq!(belt.chunk_counts().0, RING_CHUNKS);
        belt.warm_ring(&device.device, 1_920_000);
        assert_eq!(belt.chunk_counts().0, RING_CHUNKS + 1, "the raise made a chunk");
        assert!(belt.has_room(1_920_000), "one chunk holds the whole reservation");
        let chunks = belt.chunks.len();
        belt.begin_frame();
        let _ = belt.alloc(&device.device, &vec![1u8; 1_200_000], BUFFER_ALIGN);
        assert_eq!(belt.chunks.len(), chunks, "the frame's list made a chunk");
        // Asking again for what a chunk holds adds nothing.
        belt.warm_ring(&device.device, 1_920_000);
        assert_eq!(belt.chunks.len(), chunks);
    }

    /// A write larger than every ring chunk, on a ring that is already full, is not a backlog. It
    /// joins the ring, so the frames after it create nothing; a transient chunk would be remade for
    /// it on every frame. Writes a ring chunk could hold stay transient
    /// (`chunks_beyond_the_ring_are_transient`).
    #[test]
    fn a_write_no_ring_chunk_could_hold_joins_the_ring() {
        if !upload_ring_enabled() {
            return;
        }
        let device = crate::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
        let size = 1_500_000u64;
        let dst = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dst"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.warm_ring(&device.device, 1024);
        let mut chunks = 0;
        for frame in 0..4u8 {
            belt.begin_frame();
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            belt.write(
                &device.device,
                &mut encoder,
                &dst,
                0,
                &vec![frame + 1; size as usize],
            );
            if frame == 0 {
                chunks = belt.chunks.len();
                assert_eq!(chunks, RING_CHUNKS + 2, "the ring, the spill and the new chunk");
            }
            assert_eq!(belt.chunks.len(), chunks, "frame {frame} created a chunk");
            belt.upload(&queue);
            device.queue.submit([encoder.finish()]);
            belt.after_submit();
            settle(&device, &belt);
        }
        belt.begin_frame();
        assert_eq!(
            belt.chunk_counts(),
            (RING_CHUNKS + 1, RING_CHUNKS + 1),
            "it stayed"
        );
        assert!(
            read_back(&device, &dst, size).iter().all(|&b| b == 4),
            "the last frame's bytes"
        );
    }

    /// A dense frame with the frame before it still in flight: the second ring chunk takes its
    /// instance list and the spill the rest, and nothing is created. The sizes are a two-phase dense
    /// view's (2112 instances): a list of 400 B each, then the frame's other uploads. No poll, on its
    /// own device, so the first chunk stays in flight whatever the GPU's speed.
    #[test]
    fn a_dense_frame_behind_the_gpu_creates_nothing() {
        if !upload_ring_enabled() {
            return;
        }
        let device = crate::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
        let (list, rest) = (844_800u64, 271_532u64);
        let make = |size| {
            device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dst"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let (list_dst, rest_dst) = (make(list), make(rest));
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.warm_ring(&device.device, 2112 * 640);
        let chunks = belt.chunks.len();
        for frame in 1..=2u8 {
            belt.begin_frame();
            if frame == 2 {
                assert_eq!(
                    belt.chunk_counts(),
                    (RING_CHUNKS, RING_CHUNKS - 1),
                    "the first frame's chunk is in flight"
                );
            }
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            belt.write(
                &device.device,
                &mut encoder,
                &list_dst,
                0,
                &vec![frame; list as usize],
            );
            belt.write(
                &device.device,
                &mut encoder,
                &rest_dst,
                0,
                &vec![frame; rest as usize],
            );
            assert_eq!(belt.chunks.len(), chunks, "frame {frame} created a chunk");
            if frame == 2 {
                assert_eq!(belt.spill_used(), Some(rest), "the rest went to the spill");
            }
            belt.upload(&queue);
            device.queue.submit([encoder.finish()]);
            belt.after_submit();
        }
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        assert!(
            read_back(&device, &list_dst, list).iter().all(|&b| b == 2),
            "the list"
        );
        assert!(
            read_back(&device, &rest_dst, rest).iter().all(|&b| b == 2),
            "the rest"
        );
    }

    fn read_back(device: &crate::RenderDevice, src: &wgpu::Buffer, len: u64) -> Vec<u8> {
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("read"),
            size: len,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("read") });
        encoder.copy_buffer_to_buffer(src, 0, &staging, 0, len);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("callback").expect("map");
        let out = staging.slice(..).get_mapped_range().expect("range").to_vec();
        staging.unmap();
        out
    }

    fn target(device: &crate::RenderDevice) -> wgpu::Buffer {
        device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dst"),
            size: 64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    }

    /// The mapped ring: a chunk the GPU finished with is mapped again and reused, so frames that
    /// wait for the GPU run on one chunk, and every frame's bytes arrive.
    #[test]
    fn ring_chunks_are_reused_once_the_gpu_ran_their_submit() {
        let device = crate::test_device();
        let dst = target(&device);
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        for frame in 0..4u8 {
            belt.begin_frame();
            let bytes = [frame + 1; 64];
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            belt.write(&device.device, &mut encoder, &dst, 0, &bytes);
            belt.upload(&queue);
            device.queue.submit([encoder.finish()]);
            belt.after_submit();
            settle(&device, &belt);
            assert_eq!(read_back(&device, &dst, 64), bytes, "frame {frame}");
        }
        assert_eq!(belt.chunk_counts().0, 1, "one chunk served every frame");
    }

    /// A chunk the GPU may still read is never written: the next frame takes another chunk, and
    /// both frames' bytes arrive intact.
    #[test]
    fn a_chunk_in_flight_is_not_overwritten() {
        // The isolated device, like `a_backlogged_ring_spills_without_new_chunks`: a poll on the
        // shared test device by another test's thread could finish the first chunk's remap before
        // the second write (seen once in 24 full-suite runs on an RTX 5090, ADR-1584).
        let (_turn, device) = isolated_device();
        let (a, b) = (target(&device), target(&device));
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.begin_frame();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("one") });
        belt.write(&device.device, &mut encoder, &a, 0, &[1u8; 64]);
        belt.upload(&queue);
        if upload_ring_enabled() {
            assert_eq!(belt.chunk_counts(), (1, 0), "unmapped for the submit");
        }
        device.queue.submit([encoder.finish()]);
        belt.after_submit();
        // No poll: the remap requested has not completed when the next frame writes.
        belt.begin_frame();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("two") });
        belt.write(&device.device, &mut encoder, &b, 0, &[2u8; 64]);
        if upload_ring_enabled() {
            assert_eq!(belt.chunk_counts().0, 2, "the in-flight chunk was skipped");
        }
        belt.upload(&queue);
        device.queue.submit([encoder.finish()]);
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        assert_eq!(read_back(&device, &a, 64), vec![1u8; 64]);
        assert_eq!(read_back(&device, &b, 64), vec![2u8; 64]);
        // Both chunks come back once mapped (the second's remap is requested here).
        belt.begin_frame();
        settle(&device, &belt);
        belt.begin_frame();
        assert_eq!(belt.chunk_counts(), (2, 2));
    }

    /// `has_room` (mesh uploads through the belt, ADR-218) sees only chunks free to write: a ring
    /// chunk the GPU may still read has no room, so a mesh is never deferred into a new chunk on
    /// the frame path; once its remap completed the room is back.
    #[test]
    fn a_chunk_in_flight_has_no_room_for_a_mesh() {
        let device = crate::test_device();
        let dst = target(&device);
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.reserve(&device.device, 2 * 1024 * 1024);
        belt.begin_frame();
        assert!(belt.has_room(1024 * 1024), "a reserved, mapped chunk has room");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        belt.write(&device.device, &mut encoder, &dst, 0, &[3u8; 64]);
        belt.upload(&queue);
        device.queue.submit([encoder.finish()]);
        belt.after_submit();
        // No poll and no new frame yet: the chunk is unmapped for (or still read by) that submit.
        assert_eq!(
            belt.has_room(1024),
            !upload_ring_enabled(),
            "an in-flight ring chunk has no room"
        );
        settle(&device, &belt);
        belt.begin_frame();
        assert!(belt.has_room(1024 * 1024), "the remapped chunk has its room back");
        assert_eq!(belt.chunk_counts().0, 1, "no chunk was added");
        assert_eq!(read_back(&device, &dst, 64), vec![3u8; 64]);
    }

    /// A frame that needs more chunks than the ring and the spill hold gets a transient one: its
    /// bytes arrive, and it is gone once its submit was made (the ring stays at its size).
    #[test]
    fn chunks_beyond_the_ring_are_transient() {
        if !upload_ring_enabled() {
            return;
        }
        let device = crate::test_device();
        let targets: Vec<wgpu::Buffer> = (0..4)
            .map(|_| {
                device.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("dst"),
                    size: 600 * 1024,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                })
            })
            .collect();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let mut belt = StagingBelt::default();
        belt.begin_frame();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("big") });
        // Four 600 KiB writes: a 1 MiB chunk holds one each. The first two fill the ring (which
        // makes the spill), the third goes to the spill, the fourth is past all three.
        for (i, dst) in targets.iter().enumerate() {
            belt.write(
                &device.device,
                &mut encoder,
                dst,
                0,
                &vec![i as u8 + 1; 600 * 1024],
            );
        }
        assert_eq!(belt.chunk_counts().0, RING_CHUNKS + 1);
        assert_eq!(
            belt.spill_used(),
            Some(600 * 1024),
            "the third write took the spill"
        );
        assert!(
            belt.spill_heap_bytes().is_some_and(|b| b >= 600 * 1024),
            "a write that spills grows the spill's CPU bytes"
        );
        belt.upload(&queue);
        device.queue.submit([encoder.finish()]);
        belt.after_submit();
        settle(&device, &belt);
        belt.begin_frame();
        assert_eq!(
            belt.chunk_counts(),
            (RING_CHUNKS, RING_CHUNKS),
            "the transient chunk is gone"
        );
        for (i, dst) in targets.iter().enumerate() {
            let got = read_back(&device, dst, 600 * 1024);
            assert!(got.iter().all(|&b| b == i as u8 + 1), "write {i}");
        }
    }

    /// A GPU behind by frames (ADR-358): once the ring is full, a write that finds every ring
    /// chunk in flight or full lands in the spill, frame after frame, without a new chunk; the
    /// spill is reused while earlier submits that read it may still run, and every frame's
    /// bytes arrive.
    #[test]
    fn a_backlogged_ring_spills_without_new_chunks() {
        if !upload_ring_enabled() {
            return;
        }
        // The isolated device: a poll on the shared test device (another test's thread) could
        // finish a remap between the fill below and the frame's write. Here maps complete only on
        // this test's submits and its final poll.
        let (_turn, device) = isolated_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let filler = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("filler"),
            size: MIN_CHUNK,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let targets: Vec<wgpu::Buffer> = (0..6).map(|_| target(&device)).collect();
        let mut belt = StagingBelt::default();
        for (frame, dst) in targets.iter().enumerate() {
            belt.begin_frame();
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            if frame >= 2 {
                // Frames 0 and 1 (no poll between them) filled the ring and made the spill.
                assert_eq!(belt.spill_used(), Some(0), "frame {frame}: the spill is kept");
                // Fill whatever ring room the GPU gave back, so the frame's write finds none.
                while let Some(room) = belt
                    .chunks
                    .iter()
                    .filter(|c| c.in_ring() && c.writable())
                    .map(|c| c.capacity - align_up(c.used, BUFFER_ALIGN))
                    .find(|&room| room >= BUFFER_ALIGN)
                {
                    belt.write(
                        &device.device,
                        &mut encoder,
                        &filler,
                        0,
                        &vec![0u8; room as usize],
                    );
                }
                assert!(!belt.has_room(BUFFER_ALIGN), "the spill is not ring room");
            }
            belt.write(&device.device, &mut encoder, dst, 0, &[frame as u8 + 1; 64]);
            if frame >= 2 {
                assert_eq!(
                    belt.spill_used(),
                    Some(64),
                    "frame {frame}: the write took the spill"
                );
            }
            if frame >= 1 {
                assert_eq!(
                    belt.chunk_counts().0,
                    RING_CHUNKS,
                    "frame {frame}: no chunk was added"
                );
            }
            belt.upload(&queue);
            device.queue.submit([encoder.finish()]);
            belt.after_submit();
        }
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        for (frame, dst) in targets.iter().enumerate() {
            assert_eq!(
                read_back(&device, dst, 64),
                vec![frame as u8 + 1; 64],
                "frame {frame}"
            );
        }
    }

    #[test]
    fn align_up_rounds_to_the_boundary() {
        assert_eq!(align_up(0, 256), 0);
        assert_eq!(align_up(4, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        assert_eq!(buffer_align(), 4);
    }

    #[test]
    fn two_buffer_writes_round_trip_through_one_upload() {
        let device = crate::test_device();
        let mk = |label: &str| {
            device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let a = mk("a");
        let b = mk("b");
        let mut belt = StagingBelt::default();
        belt.begin_frame();
        belt.defer(&device.device, &a, 0, &1.0f32.to_le_bytes());
        // 4 bytes is the minimum copy. Pad the logical write to 16 so the
        // destination's whole uniform is defined.
        let mut bytes_a = vec![0u8; 16];
        bytes_a[..4].copy_from_slice(&1.0f32.to_le_bytes());
        let mut bytes_b = vec![0u8; 16];
        bytes_b[..4].copy_from_slice(&2.0f32.to_le_bytes());
        belt.begin_frame();
        belt.defer(&device.device, &a, 0, &bytes_a);
        belt.defer(&device.device, &b, 0, &bytes_b);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("belt") });
        belt.flush(&mut encoder);
        assert!(belt.pending_is_empty());
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        belt.upload(&queue);
        let read = |src: &wgpu::Buffer| -> [u8; 16] {
            let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("read"),
                size: 16,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("read") });
            encoder.copy_buffer_to_buffer(src, 0, &staging, 0, 16);
            device.queue.submit([encoder.finish()]);
            let (tx, rx) = std::sync::mpsc::channel();
            staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            device
                .tracked()
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            rx.recv().expect("callback").expect("map");
            let mapped = staging.slice(..).get_mapped_range().expect("range");
            let mut out = [0u8; 16];
            out.copy_from_slice(&mapped[..16]);
            drop(mapped);
            staging.unmap();
            out
        };
        // The copies are in `encoder`, which has not been submitted yet.
        device.queue.submit([encoder.finish()]);
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let got_a = read(&a);
        let got_b = read(&b);
        assert_eq!(&got_a[..4], &1.0f32.to_le_bytes());
        assert_eq!(&got_b[..4], &2.0f32.to_le_bytes());
    }

    #[test]
    fn unaligned_rgba_row_survives_the_belt() {
        let device = crate::test_device();
        // 3 pixels: 12-byte row, which `copy_buffer_to_texture` cannot express
        // without a 256-byte stride.
        let width = 3u32;
        let height = 1u32;
        let rgba = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("row"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut belt = StagingBelt::default();
        belt.begin_frame();
        belt.defer_texture(
            &device.device,
            &texture,
            0,
            wgpu::Origin3d::ZERO,
            &rgba,
            width,
            height,
        )
        .expect("the 12-byte upload fits");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("tex") });
        belt.flush(&mut encoder);
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        belt.upload(&queue);
        let padded = 256u32;
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tex read"),
            size: u64::from(padded),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("callback").expect("map");
        let mapped = staging.slice(..).get_mapped_range().expect("range");
        assert_eq!(&mapped[..12], &rgba);
        drop(mapped);
        staging.unmap();
    }

    /// A 32768 × 32768 RGBA8 upload is 4 GiB padded: the byte count used to be computed in
    /// `u32`, wrap to 0, and panic on the slice copies. It must return an error instead.
    #[test]
    fn rgba_byte_size_does_not_wrap_at_four_gib() {
        let (_turn, device) = isolated_device();
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("big upload"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut belt = StagingBelt::default();
        belt.begin_frame();
        // One row of data; the size that matters is width × height.
        let row = vec![0u8; 32768 * 4];
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("big") });
        let err = belt
            .write_texture(
                &device.device,
                &mut encoder,
                &texture,
                0,
                wgpu::Origin3d::ZERO,
                &row,
                32768,
                32768,
            )
            .expect_err("a 4 GiB upload must not be staged");
        assert!(err.to_string().contains("too large"), "unexpected error: {err}");
    }
}
