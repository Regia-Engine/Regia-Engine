// SPDX-License-Identifier: MPL-2.0
//! Window screenshots: requests are served at the next present of the window — the frame is copied to
//! a buffer before presentation, read back and handed to the request's callback as RGBA8.

use regia_app::prelude::*;

use crate::{RenderDevice, RenderError};

/// A captured image: tightly packed RGBA8 rows, top row first, sRGB-encoded (as displayed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// Called once with the captured image or an error.
pub type ScreenshotCallback = Box<dyn FnOnce(Result<CapturedImage, String>) + Send + Sync>;

/// What a screenshot captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScreenshotOptions {
    /// Capture before the player's display calibration ([`crate::DisplayCalibration`]): the
    /// calibration-neutral image goldens compare. The default (`false`) captures what the
    /// window shows. Either way an HDR frame is captured as its SDR rendition.
    pub neutral: bool,
}

/// Pending screenshot requests.
#[derive(Resource, Default)]
pub struct ScreenshotRequests {
    pending: Vec<(Option<Entity>, ScreenshotOptions, ScreenshotCallback)>,
}

impl ScreenshotRequests {
    /// Requests a screenshot of `window` (`None` = any window, the first one presented), as
    /// displayed (calibration applied).
    pub fn request(&mut self, window: Option<Entity>, callback: ScreenshotCallback) {
        self.request_with(window, ScreenshotOptions::default(), callback);
    }

    /// [`Self::request`] with options (for example a calibration-neutral capture).
    pub fn request_with(
        &mut self,
        window: Option<Entity>,
        options: ScreenshotOptions,
        callback: ScreenshotCallback,
    ) {
        self.pending.push((window, options, callback));
    }

    /// Removes and returns the requests that `window` should serve. Public so editors that own
    /// their own present loop (the regia-editor binary) can serve agent screenshots from it.
    /// Drops the options: a present loop without a display stage shows the neutral image.
    pub fn take_for(&mut self, window: Entity) -> Vec<ScreenshotCallback> {
        self.take_with_options(window)
            .into_iter()
            .map(|(_, cb)| cb)
            .collect()
    }

    /// [`Self::take_for`] keeping each request's options.
    pub fn take_with_options(&mut self, window: Entity) -> Vec<(ScreenshotOptions, ScreenshotCallback)> {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(w, _, _)| w.is_none_or(|w| w == window));
        self.pending = rest;
        mine.into_iter().map(|(_, options, cb)| (options, cb)).collect()
    }

    /// Puts requests [`Self::take_with_options`] took for `window` back at the front of the queue,
    /// in their order (a frame that could not serve them).
    #[cfg(feature = "frame-gen")]
    pub(crate) fn requeue(&mut self, window: Entity, requests: Vec<(ScreenshotOptions, ScreenshotCallback)>) {
        if requests.is_empty() {
            return;
        }
        let rest = std::mem::take(&mut self.pending);
        self.pending = requests
            .into_iter()
            .map(|(options, callback)| (Some(window), options, callback))
            .chain(rest)
            .collect();
    }

    /// Number of pending requests.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True if no request is pending.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The largest buffer a [`BufferPool`] keeps (64 MiB: a 4096 x 4096 RGBA8 capture). A larger
/// one is freed when it is given back, so one huge capture does not stay resident.
pub const MAX_POOLED_BUFFER_BYTES: usize = 64 << 20;

/// A small, bounded pool of byte buffers that screenshot captures reuse (ADR-8848).
///
/// Every window `seeing.screenshot` used to allocate a frame-sized RGBA copy, a PNG and a
/// base64 reply, then free them; glibc kept the freed blocks in its arenas, so each shot grew the
/// process by about 10 MB until five steps later (ADR-8782). Capture code takes its buffers from
/// a pool and gives them back when the reply is sent, so same-size shots after the first
/// allocate nothing frame-sized. A pool holds at most `max_buffers` buffers of at most
/// [`MAX_POOLED_BUFFER_BYTES`] each; [`release_screenshot_buffers`] empties every pool.
pub struct BufferPool {
    free: std::sync::Mutex<Vec<Vec<u8>>>,
    max_buffers: usize,
}

impl std::fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The sizes, not the bytes.
        f.debug_struct("BufferPool")
            .field("buffers", &self.len())
            .field("retained_bytes", &self.retained_bytes())
            .field("max_buffers", &self.max_buffers)
            .finish()
    }
}

impl BufferPool {
    /// An empty pool that keeps at most `max_buffers` buffers.
    #[must_use]
    pub const fn new(max_buffers: usize) -> Self {
        Self {
            free: std::sync::Mutex::new(Vec::new()),
            max_buffers,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Vec<u8>>> {
        self.free
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An empty buffer with room for at least `min_capacity` bytes: the smallest kept buffer
    /// that fits, else the largest kept one grown to fit, else a new one.
    #[must_use]
    pub fn take(&self, min_capacity: usize) -> Vec<u8> {
        let kept = {
            let mut free = self.lock();
            let fitting = free
                .iter()
                .enumerate()
                .filter(|(_, b)| b.capacity() >= min_capacity)
                .min_by_key(|(_, b)| b.capacity())
                .map(|(i, _)| i);
            let pick = fitting.or_else(|| {
                free.iter()
                    .enumerate()
                    .max_by_key(|(_, b)| b.capacity())
                    .map(|(i, _)| i)
            });
            pick.map(|i| free.swap_remove(i))
        };
        let mut buffer = kept.unwrap_or_default();
        buffer.clear();
        if buffer.capacity() == 0 {
            buffer.reserve_exact(min_capacity);
        } else if buffer.capacity() < min_capacity {
            // A kept buffer that grows takes an eighth more: shot sizes drift (a PNG of a moving
            // scene), and the next slightly larger one then fits without another growth.
            buffer.reserve_exact(min_capacity.saturating_add(min_capacity / 8));
        }
        buffer
    }

    /// Gives `buffer` back for a later [`Self::take`]. A buffer with no capacity or over
    /// [`MAX_POOLED_BUFFER_BYTES`] is freed; with the pool full the smallest buffer is freed.
    pub fn give(&self, mut buffer: Vec<u8>) {
        if buffer.capacity() == 0 || buffer.capacity() > MAX_POOLED_BUFFER_BYTES || self.max_buffers == 0 {
            return;
        }
        buffer.clear();
        let evicted = {
            let mut free = self.lock();
            free.push(buffer);
            if free.len() > self.max_buffers {
                let smallest = free
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, b)| b.capacity())
                    .map(|(i, _)| i);
                smallest.map(|i| free.swap_remove(i))
            } else {
                None
            }
        };
        // Freed outside the lock.
        drop(evicted);
    }

    /// Frees every kept buffer.
    pub fn clear(&self) {
        let freed = std::mem::take(&mut *self.lock());
        drop(freed);
    }

    /// Bytes of capacity the pool keeps now.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.lock().iter().map(Vec::capacity).sum()
    }

    /// Buffers the pool keeps now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True if the pool keeps no buffer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// The RGBA8 pixels of window captures ([`CapturedImage::rgba`]): a neutral and an applied
/// capture can be in flight at once. Give an image back with [`recycle`].
pub static FRAME_BUFFERS: BufferPool = BufferPool::new(2);

/// Encoded images of captures (the agent's PNG bytes).
pub static ENCODE_BUFFERS: BufferPool = BufferPool::new(1);

/// Reply text of captures: the agent's base64 string and the serialized reply it is sent in.
pub static REPLY_BUFFERS: BufferPool = BufferPool::new(4);

/// Gives a capture's pixel buffer back to [`FRAME_BUFFERS`] once its reply is built.
pub fn recycle(image: CapturedImage) {
    FRAME_BUFFERS.give(image.rgba);
}

/// Frees every buffer the screenshot pools keep ([`FRAME_BUFFERS`], [`ENCODE_BUFFERS`],
/// [`REPLY_BUFFERS`]). The renderer calls it when it drops its window surfaces (suspend, exit,
/// a lost surface); a window's own readback buffers go with its surface.
pub fn release_screenshot_buffers() {
    FRAME_BUFFERS.clear();
    ENCODE_BUFFERS.clear();
    REPLY_BUFFERS.clear();
}

/// The most mappable readback buffers a [`ReadbackBuffers`] keeps (a neutral and an applied
/// capture of one frame).
const MAX_READBACK_BUFFERS: usize = 2;

/// Mappable GPU buffers a window's screenshot copies reuse (ADR-8848): a same-size capture
/// copies into a kept buffer instead of creating one. A capture of another size frees the kept
/// buffers of the old size. Dropped with the window's surface.
#[derive(Debug, Default)]
pub struct ReadbackBuffers {
    free: Vec<wgpu::Buffer>,
    created: u64,
}

impl ReadbackBuffers {
    /// No kept buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn take(&mut self, device: &RenderDevice, size: u64) -> wgpu::Buffer {
        if let Some(i) = self.free.iter().position(|b| b.size() == size) {
            return self.free.swap_remove(i);
        }
        // A resize: the kept buffers are the old size and will not fit again soon.
        self.free.clear();
        self.created += 1;
        device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
    }

    fn give(&mut self, buffer: wgpu::Buffer) {
        if self.free.len() >= MAX_READBACK_BUFFERS {
            self.free.remove(0);
        }
        self.free.push(buffer);
    }

    /// Buffers kept now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.free.len()
    }

    /// True if no buffer is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.free.is_empty()
    }

    /// Buffers created since this cache was made (a reused buffer is not counted).
    #[must_use]
    pub fn created(&self) -> u64 {
        self.created
    }

    /// Frees the kept buffers.
    pub fn clear(&mut self) {
        self.free.clear();
    }
}

/// Copies `texture` (an RGBA8 or BGRA8 color texture with `COPY_SRC`) into a readback buffer
/// from `buffers`, waits for the GPU and returns its pixels as RGBA8 in a [`FRAME_BUFFERS`]
/// buffer. Give the image back with [`recycle`] once it is used (ADR-8848).
///
/// # Errors
/// When the device cannot be polled or the buffer cannot be mapped.
pub fn read_texture_rgba(
    device: &RenderDevice,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    buffers: &mut ReadbackBuffers,
) -> Result<CapturedImage, RenderError> {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("screenshot copy"),
        });
    let readback = encode_copy_with(device, &mut encoder, texture, format, Some(buffers));
    device.queue.submit([encoder.finish()]);
    finish_with(device, readback, Some(buffers))
}

/// A buffer the frame was copied into, waiting to be read after submission.
pub(crate) struct PendingReadback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_row: u32,
    bgra: bool,
}

/// Records a copy of `texture` (the frame) into a new buffer.
pub(crate) fn encode_copy(
    device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
) -> PendingReadback {
    encode_copy_with(device, encoder, texture, format, None)
}

/// [`encode_copy`] into a buffer from `buffers` when given (a kept one of the same size).
pub(crate) fn encode_copy_with(
    device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    buffers: Option<&mut ReadbackBuffers>,
) -> PendingReadback {
    let (width, height) = (texture.width(), texture.height());
    let padded_row =
        (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let size = u64::from(padded_row) * u64::from(height);
    let buffer = match buffers {
        Some(buffers) => buffers.take(device, size),
        None => device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        }),
    };
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let bgra = matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    PendingReadback {
        buffer,
        width,
        height,
        padded_row,
        bgra,
    }
}

/// Waits for the copy and converts it to RGBA8 (pixels in a [`FRAME_BUFFERS`] buffer).
pub(crate) fn finish(device: &RenderDevice, readback: PendingReadback) -> Result<CapturedImage, RenderError> {
    finish_with(device, readback, None)
}

/// [`finish`], giving the readback buffer back to `buffers` when given.
pub(crate) fn finish_with(
    device: &RenderDevice,
    readback: PendingReadback,
    buffers: Option<&mut ReadbackBuffers>,
) -> Result<CapturedImage, RenderError> {
    let _span = tracing::debug_span!("screenshot_finish").entered();
    let (tx, rx) = std::sync::mpsc::channel();
    readback
        .buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = readback
        .buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let row = (readback.width * 4) as usize;
    let mut rgba = FRAME_BUFFERS.take(row * readback.height as usize);
    for y in 0..readback.height as usize {
        let start = y * readback.padded_row as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    drop(mapped);
    readback.buffer.unmap();
    if let Some(buffers) = buffers {
        buffers.give(readback.buffer);
    }
    if readback.bgra {
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
    Ok(CapturedImage {
        width: readback.width,
        height: readback.height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_hands_back_the_buffer_it_was_given() {
        let pool = BufferPool::new(2);
        let mut first = pool.take(1000);
        assert!(first.capacity() >= 1000);
        first.extend_from_slice(&[7; 1000]);
        let address = first.as_ptr();
        pool.give(first);
        assert_eq!(pool.len(), 1);
        let again = pool.take(1000);
        assert_eq!(again.as_ptr(), address, "the same allocation comes back");
        assert!(again.is_empty(), "a taken buffer is empty");
        assert!(pool.is_empty());
    }

    #[test]
    fn a_pool_takes_the_smallest_fitting_buffer_and_keeps_at_most_its_bound() {
        let pool = BufferPool::new(2);
        pool.give(Vec::with_capacity(100));
        pool.give(Vec::with_capacity(5000));
        pool.give(Vec::with_capacity(2000));
        assert_eq!(pool.len(), 2, "the bound holds");
        assert_eq!(pool.retained_bytes(), 7000, "the smallest was freed");
        assert_eq!(pool.take(1500).capacity(), 2000, "smallest that fits");
        // Nothing fits: the largest kept buffer grows (with an eighth of headroom).
        let grown = pool.take(8000);
        assert!(grown.capacity() >= 8000 + 1000, "{}", grown.capacity());
        assert!(pool.is_empty());
    }

    #[test]
    fn a_pool_frees_oversized_and_empty_buffers_and_clears() {
        let pool = BufferPool::new(4);
        pool.give(Vec::new());
        pool.give(Vec::with_capacity(MAX_POOLED_BUFFER_BYTES + 1));
        assert!(pool.is_empty(), "nothing worth keeping");
        pool.give(Vec::with_capacity(64));
        pool.clear();
        assert!(pool.is_empty());
        let none = BufferPool::new(0);
        none.give(Vec::with_capacity(64));
        assert!(none.is_empty(), "a zero bound keeps nothing");
    }

    #[test]
    fn requests_are_routed_to_their_window() {
        let mut world = World::new();
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let mut requests = ScreenshotRequests::default();
        requests.request(Some(a), Box::new(|_| {}));
        requests.request(None, Box::new(|_| {}));
        requests.request(Some(b), Box::new(|_| {}));
        assert_eq!(
            requests.take_for(a).len(),
            2,
            "its own and the any-window request"
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(requests.take_for(b).len(), 1);
        assert!(requests.is_empty());
        requests.request_with(Some(a), ScreenshotOptions { neutral: true }, Box::new(|_| {}));
        let taken = requests.take_with_options(a);
        assert_eq!(taken.len(), 1);
        assert!(taken[0].0.neutral);
    }
}
