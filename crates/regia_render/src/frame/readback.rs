// SPDX-License-Identifier: MPL-2.0
//! CPU readbacks: RGBA8 colour, depth, the motion field and HDR captures (moved out of
//! `frame.rs` unchanged, ADR-1552).

use super::*;

/// Renders `texture` into tightly packed RGBA8 (used by offscreen frames and screenshots).
#[tracing::instrument(skip_all)]
pub fn read_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, RenderError> {
    let row = width * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame readback"),
    });
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
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        extent3d(width, height),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::device::TrackedDevice::observe(device)
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height as usize {
        let start = y * padded as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
}

/// Object motion of one frame, read back from the velocity target.
///
/// `xy` is `previous uv − current uv`. `(0, 0)` is a pixel no surface wrote (the clear).
#[derive(Clone, Debug, PartialEq)]
pub struct MotionField {
    /// Width of the velocity target (the internal render size).
    pub width: u32,
    /// Height of the velocity target.
    pub height: u32,
    /// Row-major, top row first.
    pub motion: Vec<[f32; 2]>,
}

/// Copies a single-sample `Depth32Float` texture into row-major `f32`s.
pub(super) fn read_depth32(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<f32>, RenderError> {
    let row = width * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("depth readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("depth readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::DepthOnly,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        extent3d(width, height),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::device::TrackedDevice::observe(device)
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mut depth = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let start = y * padded as usize;
        depth.extend_from_slice(bytemuck::cast_slice(&mapped[start..start + row as usize]));
    }
    drop(mapped);
    buffer.unmap();
    Ok(depth)
}

/// Test hook state ([`FrameRenderer::set_prepass_depth_capture`]): the buffer a frame copies its
/// scene depth into right after the depth passes, with the size it holds.
#[cfg(feature = "test-hooks")]
#[derive(Default)]
pub(super) struct PrepassDepthCapture {
    buffer: Option<(wgpu::Buffer, u32, u32)>,
}

#[cfg(feature = "test-hooks")]
impl FrameRenderer {
    /// Test hook: while on, every single-sample frame copies its scene depth as the depth passes
    /// left it (the prepass and its culls, before the forward pass draws and writes depth again)
    /// into a buffer [`Self::read_prepass_depth`] reads. Off drops the buffer.
    #[doc(hidden)]
    pub fn set_prepass_depth_capture(&mut self, on: bool) {
        self.prepass_depth_capture = on.then(PrepassDepthCapture::default);
    }

    /// Records the copy of `depth` (`width` x `height`, `Depth32Float`); the frame calls it right
    /// after the depth passes.
    pub(in crate::frame) fn capture_prepass_depth(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        let Some(capture) = self.prepass_depth_capture.as_mut() else {
            return;
        };
        let padded =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        if capture
            .buffer
            .as_ref()
            .is_none_or(|(_, w, h)| (*w, *h) != (width, height))
        {
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("prepass depth capture"),
                size: u64::from(padded) * u64::from(height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            capture.buffer = Some((buffer, width, height));
        }
        let Some((buffer, _, _)) = capture.buffer.as_ref() else {
            return;
        };
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: depth.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::DepthOnly,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            extent3d(width, height),
        );
    }

    /// The depth the last frame captured, row-major, top row first (`None` before a captured
    /// frame). Blocks on the GPU: tests only.
    ///
    /// # Errors
    ///
    /// The readback failed.
    #[doc(hidden)]
    pub fn read_prepass_depth(&self, device: &RenderDevice) -> Result<Option<Vec<f32>>, RenderError> {
        let Some((buffer, width, height)) = self
            .prepass_depth_capture
            .as_ref()
            .and_then(|capture| capture.buffer.as_ref())
        else {
            return Ok(None);
        };
        let (width, height) = (*width, *height);
        let row = width * 4;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut depth = Vec::with_capacity((width * height) as usize);
        {
            let mapped = buffer
                .slice(..)
                .get_mapped_range()
                .map_err(|e| RenderError::Readback(e.to_string()))?;
            for y in 0..height as usize {
                let start = y * padded as usize;
                depth.extend_from_slice(bytemuck::cast_slice(&mapped[start..start + row as usize]));
            }
        }
        buffer.unmap();
        Ok(Some(depth))
    }
}

/// Linear HDR scene colour read back by [`FrameRenderer::take_hdr_capture`].
#[derive(Clone, Debug, PartialEq)]
pub struct HdrCapture {
    /// Width (the post stack's input size).
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Row-major linear RGB, top row first, in the frame's photometric units (cd/m²).
    pub rgb: Vec<[f32; 3]>,
}

pub(super) fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let mant = f32::from(h & 0x3ff);
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 => {
            if mant == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => sign * (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Unsigned float with `mant_bits` mantissa bits and a 5-bit exponent (the R11G11B10 channels).
pub(super) fn small_float(bits: u32, mant_bits: u32) -> f32 {
    let exp = ((bits >> mant_bits) & 0x1f) as i32;
    let mant = (bits & ((1 << mant_bits) - 1)) as f32;
    let scale = (1u32 << mant_bits) as f32;
    match exp {
        0 => mant / scale * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + mant / scale) * 2f32.powi(e - 15),
    }
}

pub(super) fn decode_hdr_texel(row: &[u8], x: usize, format: wgpu::TextureFormat) -> [f32; 3] {
    match format {
        wgpu::TextureFormat::Rg11b10Ufloat => {
            let o = x * 4;
            let v = u32::from_le_bytes([row[o], row[o + 1], row[o + 2], row[o + 3]]);
            [
                small_float(v & 0x7ff, 6),
                small_float((v >> 11) & 0x7ff, 6),
                small_float(v >> 22, 5),
            ]
        }
        _ => {
            let o = x * 8;
            let h = |k: usize| f16_to_f32(u16::from_le_bytes([row[o + 2 * k], row[o + 2 * k + 1]]));
            [h(0), h(1), h(2)]
        }
    }
}

/// A recorded HDR copy: buffer, width, height, padded row bytes, format.
pub(super) type HdrCopy = (wgpu::Buffer, u32, u32, u32, wgpu::TextureFormat);

/// Records a copy of the HDR target into a mappable buffer on the frame's encoder.
pub(super) fn record_hdr_copy(
    d: &crate::device::TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Option<HdrCopy> {
    let format = texture.format();
    let bpp = match format {
        wgpu::TextureFormat::Rgba16Float => 8,
        wgpu::TextureFormat::Rg11b10Ufloat => 4,
        other => {
            tracing::warn!(target: "regia_render", ?other, "HDR capture skipped: unsupported format");
            return None;
        }
    };
    if !texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        tracing::warn!(target: "regia_render", "HDR capture skipped: the HDR target is not copyable");
        return None;
    }
    let width = width.min(texture.width());
    let height = height.min(texture.height());
    let padded =
        (width * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hdr capture"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
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
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        extent3d(width, height),
    );
    Some((buffer, width, height, padded, format))
}
