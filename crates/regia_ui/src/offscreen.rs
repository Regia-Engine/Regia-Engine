// SPDX-License-Identifier: MPL-2.0
//! Offscreen UI target for tests, captures and benches: an `Rgba8UnormSrgb` texture that can
//! hold a backdrop (or a scene frame rendered into [`OffscreenTarget::view`]), receive the UI
//! overlay exactly as a window frame does, and be read back as sRGB RGBA8.

use regia_render::RenderDevice;

use crate::render::UiRenderer;

/// An sRGB colour target plus its readback path.
pub struct OffscreenTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl OffscreenTarget {
    /// Texture format of the target (what an sRGB window surface uses).
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    /// A `width`×`height` target.
    pub fn new(device: &RenderDevice, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// The colour view (render a scene into it before [`Self::draw_ui`]).
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The colour texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Clears to a linear colour.
    pub fn clear(&self, device: &RenderDevice, linear: [f64; 4]) {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ui offscreen clear"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui offscreen clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: linear[0],
                            g: linear[1],
                            b: linear[2],
                            a: linear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        device.queue.submit([encoder.finish()]);
    }

    /// Uploads a backdrop (tightly packed sRGB RGBA8 of the target's size). Returns false when
    /// the size does not match.
    pub fn upload(&self, device: &RenderDevice, rgba: &[u8]) -> bool {
        if rgba.len() != (self.width as usize) * (self.height as usize) * 4 {
            return false;
        }
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        true
    }

    /// Draws the renderer's current quad list over the target and submits.
    pub fn draw_ui(&self, device: &RenderDevice, renderer: &mut UiRenderer) {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ui offscreen draw"),
            });
        renderer.draw_with_target(
            device,
            &mut encoder,
            &self.view,
            Some(&self.texture),
            Self::FORMAT,
            self.width,
            self.height,
        );
        device.queue.submit([encoder.finish()]);
    }

    /// Reads the target back as tightly packed sRGB RGBA8.
    pub fn read(&self, device: &RenderDevice) -> Result<Vec<u8>, String> {
        let row = self.width * 4;
        let padded_row =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui offscreen readback"),
            size: u64::from(padded_row) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ui offscreen readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("poll: {e}"))?;
        rx.recv()
            .map_err(|e| format!("map channel: {e}"))?
            .map_err(|e| format!("map: {e}"))?;
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|e| format!("mapped range: {e}"))?;
        let mut pixels = Vec::with_capacity((row * self.height) as usize);
        for y in 0..self.height as usize {
            let start = y * padded_row as usize;
            pixels.extend_from_slice(&mapped[start..start + row as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }
}
