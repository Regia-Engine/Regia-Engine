// SPDX-License-Identifier: MPL-2.0
//! Frosted backing for the viewport's overlay chips (the UI backdrop blur in the editor, ADR-2129).
//!
//! The overlays are egui shapes over a texture, so nothing in egui can read what is behind them.
//! When [`crate::viewport::ViewportSettings::overlay_blur`] is above zero the pane's finished
//! picture is blurred under each chip before egui samples it: on the GPU path through the same
//! backdrop quads the in-game UI draws, on the software path with the CPU blur. Off by default;
//! with the setting at zero nothing here runs and the viewport paints as before.
//!
//! A tool overlay that draws its own chip adds its rect with [`blur_behind_chips`].

use std::sync::Mutex;

use regia_app::prelude::*;
use regia_render::{OffscreenTarget, RenderDevice};
use regia_ui::{TextureRef, UiColor, UiQuad, UiRenderer, cpu_backdrop_blur};

/// The largest blur the setting accepts, in points (matches `ui.set_backdrop_blur`).
pub const MAX_OVERLAY_BLUR: f32 = 64.0;
/// Fill alpha of a chip over a blurred backing (the plain chip is 160: the blur does part of the
/// separation from the picture).
pub const FROSTED_CHIP_ALPHA: u8 = 110;

/// The renderer of the viewport's backdrop quads, and what the last pane did with it.
#[derive(Resource, Default)]
pub struct ViewportBlurState {
    renderer: Mutex<UiRenderer>,
    /// Chips blurred in the last pane drawn, on either path.
    pub chips: u32,
    /// Pixels the blur passes shaded in the last pane drawn on the GPU path.
    pub shaded_pixels: u64,
}

/// A chip's rect in pixels of a `target`-sized picture of the pane at `pane` (x, y, w, h). The
/// picture may be smaller than the pane in points (the pane is clamped to egui's texture limit).
#[must_use]
pub fn chip_to_pixels(chip: egui::Rect, pane: egui::Rect, target: [u32; 2]) -> [f32; 4] {
    let sx = target[0] as f32 / pane.width().max(1.0);
    let sy = target[1] as f32 / pane.height().max(1.0);
    [
        (chip.min.x - pane.min.x) * sx,
        (chip.min.y - pane.min.y) * sy,
        chip.width() * sx,
        chip.height() * sy,
    ]
}

/// Blurs under `chips` (pane points) in the finished GPU picture of the pane at `pane`. `radius` is
/// the Gaussian sigma in points. The picture is rendered anew every frame, so a blur never stacks on
/// the previous frame's.
pub fn blur_behind_chips(
    world: &mut World,
    device: &RenderDevice,
    target: &OffscreenTarget,
    pane: egui::Rect,
    chips: &[egui::Rect],
    radius: f32,
) {
    if radius <= 0.0 || chips.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("viewport_overlay_blur").entered();
    let size = [target.width, target.height];
    let scale = target.width as f32 / pane.width().max(1.0);
    let quads: Vec<UiQuad> = chips
        .iter()
        .map(|chip| {
            let rect = chip_to_pixels(*chip, pane, size);
            UiQuad {
                texture: TextureRef::Backdrop,
                blur: (radius * scale).clamp(0.0, MAX_OVERLAY_BLUR),
                corner_radius: [3.0 * scale; 4],
                ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::WHITE)
            }
        })
        .collect();
    let mut state = world.get_resource_or_insert_with(ViewportBlurState::default);
    let (blurred, shaded) = {
        let mut renderer = match state.renderer.lock() {
            Ok(renderer) => renderer,
            Err(poisoned) => poisoned.into_inner(),
        };
        renderer.set_quads(quads);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewport overlay blur"),
            });
        renderer.draw_with_target(
            device,
            &mut encoder,
            &target.view,
            Some(&target.texture),
            target.texture.format(),
            target.width,
            target.height,
        );
        device.queue.submit([encoder.finish()]);
        let stats = renderer.backdrop_stats();
        (stats.blurred, stats.shaded_pixels)
    };
    state.chips = blurred;
    state.shaded_pixels = shaded;
}

/// [`blur_behind_chips`] for the software path: blurs under `chips` in the picture itself.
pub fn blur_image_behind_chips(
    image: &mut egui::ColorImage,
    pane: egui::Rect,
    chips: &[egui::Rect],
    radius: f32,
) -> u32 {
    if radius <= 0.0 {
        return 0;
    }
    let (width, height) = (image.size[0], image.size[1]);
    let size = [width as u32, height as u32];
    let scale = width as f32 / pane.width().max(1.0);
    let sigma = (radius * scale).clamp(0.0, MAX_OVERLAY_BLUR);
    let reach = (3.0 * sigma).ceil() as i64 + 1;
    let mut blurred = 0;
    for chip in chips {
        let rect = chip_to_pixels(*chip, pane, size);
        let x0 = (rect[0].floor() as i64 - reach).clamp(0, width as i64) as usize;
        let y0 = (rect[1].floor() as i64 - reach).clamp(0, height as i64) as usize;
        let x1 = ((rect[0] + rect[2]).ceil() as i64 + reach).clamp(0, width as i64) as usize;
        let y1 = ((rect[1] + rect[3]).ceil() as i64 + reach).clamp(0, height as i64) as usize;
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let mut window = Vec::with_capacity(w * h);
        for y in y0..y1 {
            for pixel in &image.pixels[y * width + x0..y * width + x1] {
                window.push(pixel.to_array().map(|c| f32::from(c) / 255.0));
            }
        }
        let local = [rect[0] - x0 as f32, rect[1] - y0 as f32, rect[2], rect[3]];
        cpu_backdrop_blur(&mut window, w as u32, h as u32, local, sigma);
        for (row, y) in (y0..y1).enumerate() {
            for (col, pixel) in image.pixels[y * width + x0..y * width + x1]
                .iter_mut()
                .enumerate()
            {
                let [r, g, b, a] = window[row * w + col].map(|c| (c * 255.0).round() as u8);
                *pixel = egui::Color32::from_rgba_premultiplied(r, g, b, a);
            }
        }
        blurred += 1;
    }
    blurred
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0))
    }

    fn checker(width: usize, height: usize) -> egui::ColorImage {
        let mut image = egui::ColorImage::filled([width, height], egui::Color32::BLACK);
        for y in 0..height {
            for x in 0..width {
                if (x / 2 + y / 2).is_multiple_of(2) {
                    image.pixels[y * width + x] = egui::Color32::WHITE;
                }
            }
        }
        image
    }

    #[test]
    fn a_chip_maps_to_picture_pixels_through_the_pane() {
        let chip = egui::Rect::from_min_size(egui::pos2(110.0, 60.0), egui::vec2(50.0, 20.0));
        assert_eq!(chip_to_pixels(chip, pane(), [200, 100]), [10.0, 10.0, 50.0, 20.0]);
        assert_eq!(
            chip_to_pixels(chip, pane(), [400, 200]),
            [20.0, 20.0, 100.0, 40.0]
        );
    }

    #[test]
    fn the_software_path_blurs_under_the_chip_and_nowhere_else() {
        let mut image = checker(200, 100);
        let before = image.clone();
        let chip = egui::Rect::from_min_size(egui::pos2(120.0, 70.0), egui::vec2(60.0, 30.0));
        assert_eq!(blur_image_behind_chips(&mut image, pane(), &[chip], 4.0), 1);
        // Under the chip the checker melts to mid gray.
        let inside = image.pixels[35 * 200 + 50].to_array();
        assert!(inside[0] > 90 && inside[0] < 170, "{inside:?}");
        for y in 0..100 {
            for x in 0..200 {
                let under = (20..80).contains(&x) && (20..50).contains(&y);
                if !under {
                    assert_eq!(image.pixels[y * 200 + x], before.pixels[y * 200 + x], "{x},{y}");
                }
            }
        }
    }

    #[test]
    fn the_software_path_with_zero_radius_or_off_picture_chips_changes_nothing() {
        let mut image = checker(200, 100);
        let before = image.clone();
        let chip = egui::Rect::from_min_size(egui::pos2(120.0, 70.0), egui::vec2(60.0, 30.0));
        assert_eq!(blur_image_behind_chips(&mut image, pane(), &[chip], 0.0), 0);
        let off = egui::Rect::from_min_size(egui::pos2(900.0, 900.0), egui::vec2(60.0, 30.0));
        assert_eq!(blur_image_behind_chips(&mut image, pane(), &[off], 4.0), 0);
        assert_eq!(image.pixels, before.pixels);
    }

    /// Tightly packed RGBA8 of `texture`.
    fn read(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<u8> {
        let (width, height) = (texture.width(), texture.height());
        let padded =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pane readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            texture.size(),
        );
        device.queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let mapped = buffer.slice(..).get_mapped_range().expect("map");
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for row in mapped.chunks(padded as usize) {
            out.extend_from_slice(&row[..(width * 4) as usize]);
        }
        out
    }

    /// The GPU path, on a target like the renderer's LDR one, through the real backdrop quads.
    #[test]
    fn the_gpu_path_blurs_under_the_chip_and_nowhere_else() {
        let device = match RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
        };
        let (width, height) = (200u32, 100u32);
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pane"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let image = checker(200, 100);
        let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
        device.queue.write_texture(
            texture.as_image_copy(),
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = OffscreenTarget {
            texture,
            view,
            width,
            height,
        };
        let mut world = World::new();
        let chip = egui::Rect::from_min_size(egui::pos2(120.0, 70.0), egui::vec2(60.0, 30.0));
        blur_behind_chips(&mut world, &device, &target, pane(), &[chip], 4.0);
        let state = world.resource::<ViewportBlurState>();
        assert_eq!(state.chips, 1);
        assert!(state.shaded_pixels > 0);
        let pixels = read(&device, &target.texture);
        let at = |x: usize, y: usize| pixels[(y * 200 + x) * 4];
        assert!((90..170).contains(&at(50, 35)), "{}", at(50, 35));
        assert_eq!(at(10, 10), bytes[(10 * 200 + 10) * 4], "left of the chip");
        assert_eq!(at(150, 80), bytes[(80 * 200 + 150) * 4], "under it");
    }
}
