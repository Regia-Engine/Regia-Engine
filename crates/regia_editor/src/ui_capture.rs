// SPDX-License-Identifier: MPL-2.0
//! Offscreen rendering of editor UI (no window): runs an egui pass, paints it with `egui-wgpu`
//! into a texture and reads the pixels back. Used by tests and evidence captures of panels such
//! as the Inspector; the editor binary's own present path is in `main.rs`.

use regia_render::RenderDevice;

/// Renders `run` as the whole UI of a `size` (pixels) window at `pixels_per_point` and returns the
/// pixels (sRGB, opaque dark background like the editor's clear colour).
///
/// The UI runs `passes` times before painting so grids and collapsing headers settle their sizes
/// (egui lays some widgets out from the previous pass); the last pass is painted.
///
/// # Errors
/// When the GPU readback fails.
pub fn render_ui_offscreen(
    device: &RenderDevice,
    size: [u32; 2],
    pixels_per_point: f32,
    passes: usize,
    mut run: impl FnMut(&mut egui::Ui),
) -> Result<egui::ColorImage, String> {
    let [width, height] = size;
    let ctx = egui::Context::default();
    ctx.set_visuals(egui::Visuals::dark());
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width as f32 / pixels_per_point, height as f32 / pixels_per_point),
        )),
        ..Default::default()
    };
    ctx.set_pixels_per_point(pixels_per_point);
    let mut renderer = egui_wgpu::Renderer::new(
        &device.device,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        Default::default(),
    );
    let mut output = ctx.run_ui(input(), &mut run);
    for _ in 1..passes.max(1) {
        // A capture waits for the previews earlier passes queued on the preview thread
        // (ADR-8584), so the picture holds them.
        crate::preview_worker::wait_idle();
        // Earlier passes' textures (the font atlas) must still be uploaded.
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(&device.device, &device.queue, *id, delta);
            }
        }
        output.textures_delta.set.clear();
        output.textures_delta.free.clear();
        output = ctx.run_ui(input(), &mut run);
    }
    for (id, deltas) in &output.textures_delta.set {
        for delta in deltas {
            renderer.update_texture(&device.device, &device.queue, *id, delta);
        }
    }
    let paint_jobs = ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: output.pixels_per_point,
    };

    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui capture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let padded_row =
        (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ui capture readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui capture"),
        });
    renderer.update_buffers(&device.device, &device.queue, &mut encoder, &paint_jobs, &screen);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui capture pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.025,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        renderer.render(&mut pass, &paint_jobs, &screen);
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
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    for id in &output.textures_delta.free {
        renderer.free_texture(id);
    }
    output.textures_delta.clear();

    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed: {e}"))?;
    rx.recv()
        .map_err(|_| "map callback dropped".to_string())?
        .map_err(|e| format!("readback failed: {e}"))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| format!("map failed: {e:?}"))?;
    let row = (width * 4) as usize;
    let mut rgba = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ))
}

/// Encodes an image as PNG bytes.
///
/// # Errors
/// When encoding fails.
pub fn png_bytes(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width() as u32, image.height() as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
        writer.write_image_data(&bytes).map_err(|e| e.to_string())?;
    }
    Ok(out)
}
