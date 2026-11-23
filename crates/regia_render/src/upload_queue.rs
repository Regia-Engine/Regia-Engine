// SPDX-License-Identifier: MPL-2.0
//! Budgeted background uploads (`docs/specs/12-assets.md` § 5b): streamed asset bytes reach the
//! GPU a slice per frame, never a whole 4 K level in one call.
//!
//! Producers (a [`regia_asset::PakStreamer`] consumer, a level loader) push buffer writes and
//! texture levels from the main thread; [`UploadQueue::pump`] runs once per frame (the render
//! plugin's `pump_uploads` system) and writes as many bytes as fit in the frame's time budget at
//! the measured upload rate. A texture level is split into bands of whole block rows, so no single
//! `write_texture` exceeds [`UploadQueue::max_call_bytes`] — mem-stream measured single 4 K
//! writes of ~4 ms as the remaining streaming hitch. When an upload's last band is written its
//! completion callback runs (typically [`regia_asset::LoadSets::mark_resident`]).
//!
//! The budget follows texture streaming's (`texture_stream`): time × an exponential moving
//! average of the measured bytes per microsecond, starting from a conservative rate.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use regia_app::prelude::*;

/// Where an upload goes.
pub enum UploadTarget {
    /// `bytes` at `offset` of `buffer` (`COPY_DST`; length a multiple of 4).
    Buffer {
        /// Destination.
        buffer: wgpu::Buffer,
        /// Byte offset.
        offset: u64,
    },
    /// One mip level (one array layer) of `texture`, tightly packed rows of blocks.
    TextureLevel {
        /// Destination (`COPY_DST`).
        texture: wgpu::Texture,
        /// Mip level.
        mip: u32,
        /// Array layer.
        layer: u32,
    },
}

/// Bytes of one upload: a range of a shared buffer, so a streamed asset holding every mip level
/// is split into levels without copying.
///
/// The buffer is any shared byte owner: a `Vec<u8>`, or a streamed
/// [`regia_asset::PakPayload`] whose bytes are still the mapped pak's (no heap copy between the
/// file and `write_texture`).
#[derive(Clone)]
pub struct UploadBytes {
    data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    range: std::ops::Range<usize>,
}

impl std::fmt::Debug for UploadBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadBytes")
            .field("buffer_len", &(*self.data).as_ref().len())
            .field("range", &self.range)
            .finish()
    }
}

impl UploadBytes {
    /// `range` of `data` (clamped to its length).
    #[must_use]
    pub fn slice<T: AsRef<[u8]> + Send + Sync + 'static>(
        data: Arc<T>,
        range: std::ops::Range<usize>,
    ) -> Self {
        let data: Arc<dyn AsRef<[u8]> + Send + Sync> = data;
        let end = range.end.min((*data).as_ref().len());
        let start = range.start.min(end);
        Self {
            data,
            range: start..end,
        }
    }

    /// The bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &(*self.data).as_ref()[self.range.clone()]
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    /// Whether there are no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl From<Vec<u8>> for UploadBytes {
    fn from(data: Vec<u8>) -> Self {
        let len = data.len();
        Self {
            data: Arc::new(data),
            range: 0..len,
        }
    }
}

impl From<Arc<Vec<u8>>> for UploadBytes {
    fn from(data: Arc<Vec<u8>>) -> Self {
        let len = data.len();
        Self { data, range: 0..len }
    }
}

impl From<regia_asset::PakPayload> for UploadBytes {
    fn from(payload: regia_asset::PakPayload) -> Self {
        let len = payload.len();
        Self {
            data: Arc::new(payload),
            range: 0..len,
        }
    }
}

/// Called on the main thread when an upload is fully written.
pub type UploadDone = Box<dyn FnOnce() + Send + Sync>;

struct Job {
    label: String,
    bytes: UploadBytes,
    target: UploadTarget,
    /// Bytes (buffer) or block rows (texture) already written.
    cursor: u64,
    /// A texture level whose bytes do not match the level: dropped without a write, its
    /// callback and the completed counter (it was never uploaded).
    failed: bool,
    done: Option<UploadDone>,
}

/// Counters of the upload queue.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UploadStats {
    /// Uploads waiting or in progress.
    pub pending: usize,
    /// Bytes not written yet.
    pub pending_bytes: u64,
    /// Bytes written by the last pump.
    pub last_frame_bytes: u64,
    /// Time the last pump took, microseconds.
    pub last_frame_us: f32,
    /// Measured rate, bytes per microsecond (moving average).
    pub rate_bytes_per_us: f32,
    /// Uploads completed so far.
    pub completed: u64,
    /// Uploads dropped so far (a wrong-sized level: nothing was written, no callback ran).
    pub failed: u64,
    /// Bytes written so far.
    pub total_bytes: u64,
    /// Largest single write so far.
    pub max_call_bytes: u64,
}

/// The per-frame upload queue (a render-plugin resource).
#[derive(Resource)]
pub struct UploadQueue {
    jobs: VecDeque<Job>,
    /// Time budget per frame, microseconds.
    pub budget_us: u32,
    /// Largest single write, bytes (a texture band is whole block rows up to this).
    pub max_call_bytes: u64,
    /// Smallest per-frame allowance, bytes (progress even when the rate estimate is low).
    pub min_frame_bytes: u64,
    rate: f32,
    stats: UploadStats,
}

impl Default for UploadQueue {
    fn default() -> Self {
        Self {
            jobs: VecDeque::new(),
            budget_us: 1500,
            max_call_bytes: 1024 * 1024,
            min_frame_bytes: 256 * 1024,
            // Conservative start (0.5 GB/s); the farm's RTX 5090 measures 1–3 GB/s.
            rate: 500.0,
            stats: UploadStats::default(),
        }
    }
}

impl UploadQueue {
    /// Queues a buffer write.
    pub fn push_buffer(
        &mut self,
        label: impl Into<String>,
        buffer: &wgpu::Buffer,
        offset: u64,
        bytes: impl Into<UploadBytes>,
        done: Option<UploadDone>,
    ) {
        self.jobs.push_back(Job {
            label: label.into(),
            bytes: bytes.into(),
            target: UploadTarget::Buffer {
                buffer: buffer.clone(),
                offset,
            },
            cursor: 0,
            failed: false,
            done,
        });
    }

    /// Queues one texture level: `bytes` are the level's tightly packed block rows (for BCn, one
    /// row of 4×4 blocks per `bytes_per_block_row`).
    pub fn push_texture_level(
        &mut self,
        label: impl Into<String>,
        texture: &wgpu::Texture,
        mip: u32,
        layer: u32,
        bytes: impl Into<UploadBytes>,
        done: Option<UploadDone>,
    ) {
        self.jobs.push_back(Job {
            label: label.into(),
            bytes: bytes.into(),
            target: UploadTarget::TextureLevel {
                texture: texture.clone(),
                mip,
                layer,
            },
            cursor: 0,
            failed: false,
            done,
        });
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> UploadStats {
        let mut s = self.stats;
        s.pending = self.jobs.len();
        s.pending_bytes = self.jobs.iter().map(remaining_bytes).sum();
        s.rate_bytes_per_us = self.rate;
        s
    }

    /// This frame's byte allowance.
    #[must_use]
    pub fn frame_allowance(&self) -> u64 {
        ((self.rate * self.budget_us as f32) as u64).max(self.min_frame_bytes)
    }

    /// Writes up to this frame's allowance and runs the callbacks of finished uploads.
    ///
    /// The allowance is predicted from the measured rate; the clock is checked after every write
    /// too, so a frame whose writes run slower than predicted (driver staging, a busy GPU or
    /// host) stops at the time budget instead of finishing the allowance. It overshoots by at
    /// most one write ([`Self::max_call_bytes`]) and always writes [`Self::min_frame_bytes`].
    pub fn pump(&mut self, queue: &wgpu::Queue) -> UploadStats {
        let _span = tracing::info_span!("upload_queue_pump").entered();
        let allowance = self.frame_allowance();
        let start = Instant::now();
        let time_budget = std::time::Duration::from_micros(u64::from(self.budget_us));
        let mut written = 0u64;
        let mut finished: Vec<UploadDone> = Vec::new();
        while written < allowance {
            let Some(job) = self.jobs.front_mut() else {
                break;
            };
            let budget = (allowance - written).min(self.max_call_bytes);
            let n = write_slice(queue, job, budget);
            written += n;
            self.stats.max_call_bytes = self.stats.max_call_bytes.max(n);
            if job.failed {
                // A bad level is dropped, not completed: no write happened and its callback
                // (typically `mark_resident`) must not run. Later uploads are unaffected.
                if let Some(job) = self.jobs.pop_front() {
                    self.stats.failed += 1;
                    tracing::warn!(target: "regia_render", "upload {} failed", job.label);
                }
                continue;
            }
            if remaining_bytes(job) == 0 {
                if let Some(job) = self.jobs.pop_front() {
                    self.stats.completed += 1;
                    if let Some(done) = job.done {
                        finished.push(done);
                    }
                    tracing::trace!(target: "regia_render", "upload {} done", job.label);
                }
            } else if n == 0 {
                // The next band does not fit in what is left of this frame.
                break;
            }
            if written >= self.min_frame_bytes && start.elapsed() >= time_budget {
                break;
            }
        }
        let elapsed_us = start.elapsed().as_secs_f32() * 1e6;
        if written >= 64 * 1024 && elapsed_us > 1.0 {
            // Moving average of the measured rate; slow frames pull it down quickly.
            let measured = written as f32 / elapsed_us;
            self.rate = if measured < self.rate {
                0.5 * self.rate + 0.5 * measured
            } else {
                0.9 * self.rate + 0.1 * measured
            };
        }
        self.stats.last_frame_bytes = written;
        self.stats.last_frame_us = elapsed_us;
        self.stats.total_bytes += written;
        for done in finished {
            done();
        }
        self.stats()
    }
}

/// `(bytes per block row, block rows, block height, width, height)` of a level.
fn level_layout(texture: &wgpu::Texture, mip: u32) -> (u32, u32, u32, u32, u32) {
    layout_of(texture.format(), texture.width(), texture.height(), mip)
}

/// `(bytes per block row, block rows, block height, copy width, copy height)` of one level.
/// Copy sizes are the level's physical size (whole blocks): a compressed level smaller than a
/// block (a 4096² BC7 texture's 2×2 and 1×1 mips) is still written as one 4×4 block.
fn layout_of(format: wgpu::TextureFormat, width: u32, height: u32, mip: u32) -> (u32, u32, u32, u32, u32) {
    let (bw, bh) = format.block_dimensions();
    let block_bytes = format.block_copy_size(None).unwrap_or(4);
    let blocks_wide = (width >> mip).max(1).div_ceil(bw);
    let block_rows = (height >> mip).max(1).div_ceil(bh);
    (
        blocks_wide * block_bytes,
        block_rows,
        bh,
        blocks_wide * bw,
        block_rows * bh,
    )
}

fn remaining_bytes(job: &Job) -> u64 {
    match &job.target {
        UploadTarget::Buffer { .. } => job.bytes.len() as u64 - job.cursor.min(job.bytes.len() as u64),
        UploadTarget::TextureLevel { texture, mip, .. } => {
            let (row_bytes, rows, ..) = level_layout(texture, *mip);
            u64::from(rows).saturating_sub(job.cursor) * u64::from(row_bytes)
        }
    }
}

/// Writes the next slice of `job` of at most `budget` bytes (at least one band when the budget
/// allows nothing smaller). Returns the bytes written.
fn write_slice(queue: &wgpu::Queue, job: &mut Job, budget: u64) -> u64 {
    match &job.target {
        UploadTarget::Buffer { buffer, offset } => {
            let len = job.bytes.len() as u64;
            let start = job.cursor;
            // Keep slices 4-byte aligned (COPY_BUFFER_ALIGNMENT).
            let mut n = (len - start).min(budget.max(4));
            if start + n < len {
                n &= !3;
            }
            if n == 0 {
                return 0;
            }
            queue.write_buffer(
                buffer,
                offset + start,
                &job.bytes.as_slice()[start as usize..(start + n) as usize],
            );
            job.cursor += n;
            n
        }
        UploadTarget::TextureLevel { texture, mip, layer } => {
            let (row_bytes, rows, bh, width, height) = level_layout(texture, *mip);
            let expected = u64::from(row_bytes) * u64::from(rows);
            if job.bytes.len() as u64 != expected {
                tracing::warn!(
                    target: "regia_render",
                    "upload {}: {} bytes for a level of {expected}; skipped",
                    job.label,
                    job.bytes.len()
                );
                job.failed = true;
                return 0;
            }
            let first = job.cursor as u32;
            let band = ((budget / u64::from(row_bytes.max(1))) as u32).clamp(1, rows - first);
            let start = u64::from(first) * u64::from(row_bytes);
            let end = start + u64::from(band) * u64::from(row_bytes);
            let y = first * bh;
            let h = (band * bh).min(height - y);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: *mip,
                    origin: wgpu::Origin3d { x: 0, y, z: *layer },
                    aspect: wgpu::TextureAspect::All,
                },
                &job.bytes.as_slice()[start as usize..end as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(band),
                },
                wgpu::Extent3d {
                    width,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            job.cursor += u64::from(band);
            end - start
        }
    }
}

/// `Render`: pumps the upload queue once per frame (before the windows draw).
pub(crate) fn pump_uploads(device: Option<Res<crate::RenderDevice>>, queue: Option<ResMut<UploadQueue>>) {
    let _span = tracing::debug_span!("pump_uploads").entered();
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::UPLOADS);
    let (Some(device), Some(mut queue)) = (device, queue) else {
        return;
    };
    if queue.is_idle() {
        return;
    }
    queue.pump(&device.queue);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn levels_upload_in_bands_within_the_allowance() {
        let device = crate::device::test_device();
        let size = 256u32;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("upload queue test"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 2,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let level0: Vec<u8> = (0..size * size)
            .flat_map(|i| [(i % 251) as u8, 7, 9, 255])
            .collect();
        let level1 = vec![3u8; (size / 2 * size / 2 * 4) as usize];
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upload queue test buffer"),
            size: 4096,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let done = Arc::new(AtomicU32::new(0));
        let mut q = UploadQueue {
            budget_us: 0,
            min_frame_bytes: 64 * 1024,
            max_call_bytes: 16 * 1024,
            ..UploadQueue::default()
        };
        for (mip, bytes) in [(0, level0.clone()), (1, level1)] {
            let done = done.clone();
            q.push_texture_level(
                "level",
                &texture,
                mip,
                0,
                bytes,
                Some(Box::new(move || {
                    done.fetch_add(1, Ordering::SeqCst);
                })),
            );
        }
        q.push_buffer("buf", &buffer, 0, vec![1u8; 4096], None);
        let total = q.stats().pending_bytes;
        assert_eq!(total, 256 * 256 * 4 + 128 * 128 * 4 + 4096);
        let mut frames = 0;
        while !q.is_idle() {
            let s = q.pump(&device.queue);
            assert!(s.last_frame_bytes <= 64 * 1024, "{s:?}");
            assert!(s.max_call_bytes <= 16 * 1024, "{s:?}");
            frames += 1;
            assert!(frames < 100);
        }
        assert_eq!(done.load(Ordering::SeqCst), 2);
        assert_eq!(q.stats().total_bytes, total);
        assert_eq!(q.stats().completed, 3);
        // The bands reassemble the level exactly.
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let readback =
            crate::screenshot::encode_copy(&device, &mut encoder, &texture, wgpu::TextureFormat::Rgba8Unorm);
        device.queue.submit([encoder.finish()]);
        let image = crate::screenshot::finish(&device, readback).expect("readback");
        assert_eq!(image.rgba, level0);
    }

    #[test]
    fn a_slow_frame_stops_at_the_time_budget() {
        let device = crate::device::test_device();
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("upload queue time budget"),
            size: 256 * 1024,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // A rate estimate far above what the device does: the predicted allowance covers the
        // whole buffer, but a 1 µs time budget (every write takes longer) ends the frame after
        // the guaranteed minimum.
        let mut q = UploadQueue {
            budget_us: 1,
            min_frame_bytes: 16 * 1024,
            max_call_bytes: 16 * 1024,
            rate: 1.0e9,
            ..UploadQueue::default()
        };
        assert!(q.frame_allowance() >= 256 * 1024);
        q.push_buffer("buf", &buffer, 0, vec![5u8; 256 * 1024], None);
        let mut frames = 0;
        while !q.is_idle() {
            let s = q.pump(&device.queue);
            assert_eq!(s.last_frame_bytes, 16 * 1024, "{s:?}");
            frames += 1;
            assert!(frames <= 16);
        }
        assert_eq!(frames, 16);
    }

    #[test]
    fn a_wrong_sized_level_is_skipped_not_fatal() {
        let device = crate::device::test_device();
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("upload queue bad"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut q = UploadQueue::default();
        let done = Arc::new(AtomicU32::new(0));
        let callback = {
            let done = done.clone();
            Some(Box::new(move || {
                done.fetch_add(1, Ordering::SeqCst);
            }) as UploadDone)
        };
        q.push_texture_level("bad", &texture, 0, 0, vec![0u8; 10], callback);
        q.pump(&device.queue);
        assert!(q.is_idle());
        let stats = q.stats();
        assert_eq!(stats.completed, 0, "a skipped level is not a completed upload");
        assert_eq!(stats.failed, 1);
        assert_eq!(
            done.load(Ordering::SeqCst),
            0,
            "no write: the callback must not run"
        );
        // A bad level must not stall the uploads behind it.
        q.push_texture_level("bad2", &texture, 0, 0, vec![0u8; 10], None);
        q.push_texture_level("good", &texture, 0, 0, vec![7u8; 8 * 8 * 4], None);
        q.pump(&device.queue);
        assert!(q.is_idle());
        assert_eq!(q.stats().completed, 1, "the well-formed level still uploads");
        assert_eq!(q.stats().failed, 2);
    }

    #[test]
    fn block_compressed_levels_band_by_block_rows() {
        // A BC7 4096² level: 1024 blocks of 16 bytes per row, 1024 block rows of 4 pixels.
        let (row, rows, bh, w, h) = layout_of(wgpu::TextureFormat::Bc7RgbaUnormSrgb, 4096, 4096, 0);
        assert_eq!((row, rows, bh, w, h), (16 * 1024, 1024, 4, 4096, 4096));
        // Mips 11 and 12 are 2×2 and 1×1: still one block, copied as a whole 4×4 block
        // (wgpu rejects a compressed copy that is not a multiple of the block size).
        for mip in [11, 12] {
            assert_eq!(
                layout_of(wgpu::TextureFormat::Bc7RgbaUnormSrgb, 4096, 4096, mip),
                (16, 1, 4, 4, 4)
            );
        }
        assert_eq!(layout_of(wgpu::TextureFormat::Rgba8Unorm, 8, 8, 0).0, 32);
    }
}
