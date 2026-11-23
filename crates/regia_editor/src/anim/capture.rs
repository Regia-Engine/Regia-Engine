// SPDX-License-Identifier: MPL-2.0
//! Offscreen captures of editor panels: runs an egui frame into a wgpu texture and reads it
//! back, so tests, CI evidence and the agent protocol (`editor.capture_panel`) can see a panel
//! exactly as it draws, without a window.

use regia_ecs::prelude::World;
use regia_render::RenderDevice;

/// Which animation panel to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimPanel {
    /// The anim graph editor.
    Graph,
    /// The clip viewer.
    Clip,
    /// The skeleton viewer.
    Skeleton,
    /// The locomotion test bed (ADR-8806).
    Locomotion,
}

impl AnimPanel {
    /// Parses a panel name (`graph`, `clip`, `skeleton`, `locomotion`; `anim_` prefixes accepted).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim_start_matches("anim_").trim_start_matches("anim-") {
            "graph" | "state_machine" => Some(Self::Graph),
            "clip" | "timeline" | "curves" => Some(Self::Clip),
            "skeleton" => Some(Self::Skeleton),
            "locomotion" | "locomotion_test_bed" => Some(Self::Locomotion),
            _ => None,
        }
    }

    /// Draws the panel against `world`.
    pub fn draw(self, ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
        match self {
            Self::Graph => super::graph_panel::anim_graph_panel(ui, world, device),
            Self::Clip => super::clip_panel::anim_clip_panel(ui, world, device),
            Self::Skeleton => super::skeleton_panel::anim_skeleton_panel(ui, world, device),
            Self::Locomotion => super::locomotion_panel::anim_locomotion_panel(ui, world),
        }
    }
}

/// Runs `frames` egui frames of `draw` at `size` points (1 point = 1 pixel) and renders the
/// last one. Earlier frames let egui settle layout (widths measured on the first frame).
///
/// # Errors
///
/// GPU readback failures.
#[tracing::instrument(target = "regia_editor", skip_all, name = "capture_ui")]
pub fn render_ui_offscreen(
    device: &RenderDevice,
    size: [u32; 2],
    frames: usize,
    mut draw: impl FnMut(&mut egui::Ui),
) -> Result<egui::ColorImage, String> {
    let (width, height) = (size[0].clamp(16, 4096), size[1].clamp(16, 4096));
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    let mut renderer = egui_wgpu::Renderer::new(&device.device, format, Default::default());
    let mut last = None;
    let mut to_free: Vec<egui::TextureId> = Vec::new();
    for frame in 0..frames.max(1) {
        // A capture is not a UI frame: it waits for the previews the earlier frames queued on
        // the preview thread (ADR-8584), so the picture holds them as before.
        if frame > 0 {
            crate::preview_worker::wait_idle();
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32, height as f32),
            )),
            time: Some(frame as f64 / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| draw(ui));
        });
        let mut textures = std::mem::take(&mut output.textures_delta);
        for (id, deltas) in &textures.set {
            for delta in deltas {
                renderer.update_texture(&device.device, &device.queue, *id, delta);
            }
        }
        // The last frame's frees wait until its draw is submitted.
        if frame + 1 < frames.max(1) {
            for id in &textures.free {
                renderer.free_texture(id);
            }
        } else {
            to_free = textures.free.drain().collect();
        }
        // Every delta is handled; `TexturesDelta` asserts on drop otherwise.
        textures.clear();
        last = Some(output);
    }
    let Some(output) = last else {
        return Err("no frame ran".into());
    };
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: 1.0,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("panel capture"),
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("panel capture"),
        });
    renderer.update_buffers(&device.device, &device.queue, &mut encoder, &jobs, &screen);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("panel capture"),
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
        renderer.render(&mut pass, &jobs, &screen);
    }
    let padded =
        (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("panel capture readback"),
        size: u64::from(padded) * u64::from(height),
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
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
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
    for id in &to_free {
        renderer.free_texture(id);
    }
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
        let start = y * padded as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ))
}

/// Captures one animation panel drawn against `world`.
///
/// # Errors
///
/// GPU readback failures.
pub fn capture_anim_panel(
    device: &RenderDevice,
    world: &mut World,
    panel: AnimPanel,
    size: [u32; 2],
) -> Result<egui::ColorImage, String> {
    // Three frames: egui measures on the first, the preview texture appears on the second.
    render_ui_offscreen(device, size, 3, |ui| panel.draw(ui, world, Some(device)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_names_parse() {
        assert_eq!(AnimPanel::parse("anim_graph"), Some(AnimPanel::Graph));
        assert_eq!(AnimPanel::parse("curves"), Some(AnimPanel::Clip));
        assert_eq!(AnimPanel::parse("skeleton"), Some(AnimPanel::Skeleton));
        assert_eq!(AnimPanel::parse("anim_locomotion"), Some(AnimPanel::Locomotion));
        assert_eq!(AnimPanel::parse("nope"), None);
    }
}
