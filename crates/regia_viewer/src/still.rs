// SPDX-License-Identifier: MPL-2.0
//! Offscreen frames: the same pass into a texture, read back as RGBA8. Tests and stills use it; a browser
//! host awaits [`capture_still_async`], a native host calls [`capture_still`].

use std::sync::{Arc, Mutex};

use crate::ViewerError;
use crate::renderer::ViewerRenderer;

/// One rendered image, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Still {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8 pixels, encoded sRGB, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Still {
    /// A 64-bit FNV-1a hash of size and pixels: equal stills hash equal on any host.
    #[must_use]
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |b: u8| {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        };
        self.width.to_le_bytes().into_iter().for_each(&mut eat);
        self.height.to_le_bytes().into_iter().for_each(&mut eat);
        self.rgba.iter().copied().for_each(&mut eat);
        h
    }

    /// The still as a PNG file's bytes (feature `tools`).
    ///
    /// # Errors
    /// [`ViewerError::Still`] when encoding fails.
    #[cfg(feature = "tools")]
    pub fn encode_png(&self) -> Result<Vec<u8>, ViewerError> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ViewerError::Still(e.to_string()))?;
        writer
            .write_image_data(&self.rgba)
            .map_err(|e| ViewerError::Still(e.to_string()))?;
        writer.finish().map_err(|e| ViewerError::Still(e.to_string()))?;
        Ok(out)
    }
}

struct Capture {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_row: u32,
    swap_red_blue: bool,
}

/// The readback layout of a `width` x `height` still on a device with `limits`: the padded row and the buffer
/// size. Past a limit wgpu reports a validation error, which panics without an error scope, so every limit the
/// capture touches is checked here first: the texture side and the readback buffer (a viewer device keeps
/// WebGL2's 256 MiB buffer cap while its texture limit follows the adapter).
fn readback_layout(width: u32, height: u32, limits: &wgpu::Limits) -> Result<(u32, u64), ViewerError> {
    if width == 0 || height == 0 {
        return Err(ViewerError::Still(format!("a still of {width}x{height}")));
    }
    let largest = limits.max_texture_dimension_2d;
    if width > largest || height > largest {
        return Err(ViewerError::Still(format!(
            "a still of {width}x{height}: this device's largest texture is {largest}"
        )));
    }
    let padded_row = width
        .checked_mul(4)
        .and_then(|row| row.checked_next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT))
        .ok_or_else(|| ViewerError::Still(format!("a still of {width}x{height}: the row is too long")))?;
    let size = u64::from(padded_row) * u64::from(height);
    if size > limits.max_buffer_size {
        return Err(ViewerError::Still(format!(
            "a still of {width}x{height} needs a {size} byte readback; this device's largest buffer is {}",
            limits.max_buffer_size
        )));
    }
    Ok((padded_row, size))
}

fn begin_capture(renderer: &mut ViewerRenderer, width: u32, height: u32) -> Result<Capture, ViewerError> {
    let _span = tracing::info_span!("viewer_capture").entered();
    let (padded_row, size) = readback_layout(width, height, &renderer.device().limits())?;
    let format = renderer.target_format();
    let swap_red_blue = match format {
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => false,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => true,
        other => return Err(ViewerError::Still(format!("cannot read back {other:?}"))),
    };
    let device = renderer.device().clone();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer still"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    renderer.render_offscreen(&texture)?;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer still readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("viewer still"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        texture.size(),
    );
    renderer.queue().submit([encoder.finish()]);
    Ok(Capture {
        buffer,
        width,
        height,
        padded_row,
        swap_red_blue,
    })
}

fn finish_capture(capture: &Capture) -> Result<Still, ViewerError> {
    let mapped = capture
        .buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| ViewerError::Still(format!("map: {e}")))?;
    let row = (capture.width * 4) as usize;
    let mut rgba = Vec::with_capacity(row * capture.height as usize);
    for y in 0..capture.height as usize {
        let start = y * capture.padded_row as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    drop(mapped);
    capture.buffer.unmap();
    if capture.swap_red_blue {
        let (pixels, _) = rgba.as_chunks_mut::<4>();
        for pixel in pixels {
            pixel.swap(0, 2);
        }
    }
    Ok(Still {
        width: capture.width,
        height: capture.height,
        rgba,
    })
}

/// Shared state of the map callback and the future that waits for it.
#[derive(Default)]
struct MapState {
    result: Option<Result<(), wgpu::BufferAsyncError>>,
    waker: Option<std::task::Waker>,
}

struct MapFuture(Arc<Mutex<MapState>>);

impl std::future::Future for MapFuture {
    type Output = Result<(), wgpu::BufferAsyncError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.result.take() {
            Some(result) => std::task::Poll::Ready(result),
            None => {
                state.waker = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
}

fn map_read(buffer: &wgpu::Buffer) -> MapFuture {
    let state = Arc::new(Mutex::new(MapState::default()));
    let callback_state = Arc::clone(&state);
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let mut state = callback_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.result = Some(result);
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    });
    MapFuture(state)
}

/// Renders offscreen and reads the pixels back, awaiting the map. For the browser, where nothing may block.
///
/// # Errors
/// [`ViewerError::Still`] for a bad size or format, or when the read fails.
pub async fn capture_still_async(
    renderer: &mut ViewerRenderer,
    width: u32,
    height: u32,
) -> Result<Still, ViewerError> {
    let capture = begin_capture(renderer, width, height)?;
    map_read(&capture.buffer)
        .await
        .map_err(|e| ViewerError::Still(format!("map: {e}")))?;
    finish_capture(&capture)
}

/// Renders offscreen and reads the pixels back, blocking until the GPU is done.
///
/// # Errors
/// [`ViewerError::Still`] for a bad size or format, or when the read fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn capture_still(renderer: &mut ViewerRenderer, width: u32, height: u32) -> Result<Still, ViewerError> {
    let capture = begin_capture(renderer, width, height)?;
    let future = map_read(&capture.buffer);
    renderer
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| ViewerError::Still(format!("poll: {e}")))?;
    crate::gpu::block_on(future).map_err(|e| ViewerError::Still(format!("map: {e}")))?;
    finish_capture(&capture)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Correctness audit (corr-web-viewer): a viewer device keeps WebGL2's 256 MiB buffer cap while its texture
    /// limit follows the adapter, so a still the texture limit allows could need a readback buffer past the cap;
    /// wgpu's validation error then panicked instead of the documented `ViewerError::Still`.
    #[test]
    fn corr_a_readback_past_the_buffer_cap_is_refused() {
        let limits = wgpu::Limits {
            max_texture_dimension_2d: 16384,
            ..wgpu::Limits::downlevel_webgl2_defaults()
        };
        assert_eq!(limits.max_buffer_size, 256 << 20);
        // 8192 x 8193 RGBA8 is 268,468,224 bytes, just over 256 MiB.
        assert!(matches!(
            readback_layout(8192, 8193, &limits),
            Err(ViewerError::Still(_))
        ));
        let (row, size) = readback_layout(8192, 8192, &limits).expect("exactly the cap");
        assert_eq!((row, size), (32768, 256 << 20));
        let (row, size) = readback_layout(3, 2, &limits).expect("small");
        assert_eq!((row, size), (256, 512));
        assert!(readback_layout(0, 2, &limits).is_err());
        assert!(readback_layout(16385, 2, &limits).is_err());
    }
}
