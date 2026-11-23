// SPDX-License-Identifier: MPL-2.0
//! Rendering into a texture and reading it back (tests, screenshots).

use crate::overlay::WindowOverlay;
use crate::{RenderDevice, RenderError, surface::encode_clear};

/// Clears a `width`×`height` `Rgba8Unorm` texture to `color` and returns its pixels, tightly packed
/// RGBA8, row by row.
pub fn clear_to_pixels(
    device: &RenderDevice,
    color: [f64; 4],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, RenderError> {
    clear_to_pixels_format(
        device,
        color,
        width,
        height,
        wgpu::TextureFormat::Rgba8Unorm,
        None,
    )
}

/// Like [`clear_to_pixels`] with a chosen RGBA8 format (`Rgba8UnormSrgb` gives what an sRGB window
/// shows). When `overlay` is set, the window overlay is drawn after the clear (so occluded /
/// `SurfacesHidden` screenshots still include the runtime UI).
pub fn clear_to_pixels_format(
    device: &RenderDevice,
    color: [f64; 4],
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    overlay: Option<&WindowOverlay>,
) -> Result<Vec<u8>, RenderError> {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let row = width * 4;
    let padded_row = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen"),
        });
    encode_clear(&mut encoder, &view, color);
    if let Some(overlay) = overlay {
        crate::overlay::record_overlay_on(
            overlay,
            device,
            &mut encoder,
            Some(&texture),
            &view,
            format,
            width,
            height,
        );
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
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
        size,
    );
    device.queue.submit([encoder.finish()]);
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
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
}
