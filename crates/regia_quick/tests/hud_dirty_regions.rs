// SPDX-License-Identifier: MPL-2.0
//! Dirty regions of the 2D draw list on the GPU (P-06; headless, lavapipe in CI): with dirty
//! regions on, the layer keeps a retained UI target, hashes the list per 256 × 64 tile, and
//! redraws (and uploads) only the tiles whose instances changed.
//!
//! - `hud_unchanged_frame_redraws_no_tile`: the same HUD two frames in a row redraws no tile.
//! - `hud_one_changed_value_redraws_one_tile_row`: a changed ammo count redraws one run of tiles
//!   in one tile row.
//! - `hud_steady_frame_uploads_nothing`: the steady frame writes no byte to the GPU.
//! - `hud_partial_redraw_matches_a_full_redraw`: after the partial redraw the frame equals a fresh
//!   layer's full draw of the same list (bit for bit: both write the same instances into the same
//!   pixels of the same retained format).
//! - `hud_retained_matches_direct`: the retained path's composite matches the direct path within
//!   3/255 per channel (the retained target rounds once more to 8 bits).
//!
//! The wall-clock row of P-06 (`hud_paint_cpu_ms_below_2`: the HUD's CPU paint under 2 ms on the
//! Intel N150) is a measurement recorded by `samples/hud_draw2d`'s bench, never asserted here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_core::math::{Color, Vec2};
use regia_quick::draw2d::{Draw2dLayer, Draw2dList, PxRect};
use regia_render::RenderDevice;
use regia_ui::Localization;
use regia_ui::text::{ShapedText, Text, TextSpan, TextStyle, TextSystem};

const W: u32 = 1280;
const H: u32 = 720;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (lavapipe)")
}

fn shaped(fonts: &mut TextSystem, s: &str, px: f32) -> ShapedText {
    let text = Text {
        spans: vec![TextSpan::lit(s).with_style(TextStyle {
            font_px: px,
            ..Default::default()
        })],
        ..Default::default()
    };
    fonts.shape(&text, 1.0, None, &Localization::default())
}

/// A generic HUD: health bar bottom-left, ammo count bottom-right (inside tile row 10, y 640..704),
/// crosshair in the centre, minimap frame top-right, two kill-feed lines top-left.
fn hud(list: &mut Draw2dList, fonts: &mut TextSystem, ammo: &ShapedText, health: f32) {
    let feed = [
        shaped(fonts, "Red took the flag", 16.0),
        shaped(fonts, "Blue scored", 16.0),
    ];
    list.begin_frame();
    // Kill feed.
    for (i, line) in feed.iter().enumerate() {
        let y = 16.0 + i as f32 * 24.0;
        list.push_soft_box(
            PxRect::new(12.0, y - 2.0, 220.0, 22.0),
            Color::from_srgb_u8(0, 0, 0, 120),
            4.0,
            6.0,
        );
        list.push_text(fonts, line, Vec2::new(20.0, y), Color::WHITE);
    }
    // Minimap frame.
    list.push_box(
        PxRect::new(1080.0, 16.0, 184.0, 184.0),
        Color::from_srgb_u8(10, 14, 20, 160),
        8.0,
    );
    list.push_outline(
        PxRect::new(1080.0, 16.0, 184.0, 184.0),
        Color::from_srgb_u8(200, 220, 255, 220),
        8.0,
        2.0,
    );
    // Crosshair.
    let c = Vec2::new(640.0, 360.0);
    for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
        let d = Vec2::new(dx, dy);
        list.push_segment(c + d * 6.0, c + d * 16.0, 2.0, Color::WHITE);
    }
    // Health bar.
    list.push_box(
        PxRect::new(24.0, 660.0, 260.0, 20.0),
        Color::from_srgb_u8(0, 0, 0, 140),
        4.0,
    );
    list.push_box(
        PxRect::new(26.0, 662.0, 256.0 * health, 16.0),
        Color::from_srgb_u8(64, 210, 96, 255),
        3.0,
    );
    // Ammo count.
    list.push_text(fonts, ammo, Vec2::new(1180.0, 654.0), Color::WHITE);
    list.end_frame();
}

fn target(device: &RenderDevice) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hud dirty target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// One frame: clears a target to a fixed scene colour (as a game's scene pass would), draws the
/// list with `layer` and reads the pixels back.
fn frame(device: &RenderDevice, layer: &mut Draw2dLayer, list: &mut Draw2dList) -> Vec<u8> {
    let texture = target(device);
    let view = texture.create_view(&Default::default());
    let mut encoder = device.device.create_command_encoder(&Default::default());
    {
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hud dirty clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.05,
                        g: 0.2,
                        b: 0.4,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    layer.draw_list(device, &mut encoder, &view, FORMAT, W, H, list);
    readback(device, encoder, &texture)
}

fn readback(device: &RenderDevice, mut encoder: wgpu::CommandEncoder, texture: &wgpu::Texture) -> Vec<u8> {
    let row = W * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hud dirty readback"),
        size: u64::from(padded) * u64::from(H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
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
        .expect("poll");
    rx.recv().expect("map").expect("map ok");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut pixels = Vec::with_capacity((row * H) as usize);
    for y in 0..H as usize {
        let start = y * padded as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    pixels
}

fn retained_layer(device: &RenderDevice) -> Draw2dLayer {
    let mut layer = Draw2dLayer::new();
    layer.set_dirty_regions(true);
    layer.warm(device, FORMAT);
    layer
}

/// Fonts with every digit already in the atlas, so a changed count adds no glyph.
fn fonts_and_digits(list: &mut Draw2dList) -> TextSystem {
    let mut fonts = TextSystem::embedded_only();
    let digits = shaped(&mut fonts, "0123456789", 28.0);
    list.begin_frame();
    list.push_text(&mut fonts, &digits, Vec2::ZERO, Color::WHITE);
    list.end_frame();
    fonts
}

#[test]
fn hud_unchanged_frame_redraws_no_tile() {
    let device = device();
    let mut layer = retained_layer(&device);
    let mut list = Draw2dList::new();
    let mut fonts = fonts_and_digits(&mut list);
    let ammo = shaped(&mut fonts, "30", 28.0);
    hud(&mut list, &mut fonts, &ammo, 0.8);
    frame(&device, &mut layer, &mut list);
    let first = layer.gpu_stats().tiles;
    assert_eq!(first.tiles, 5 * 12, "1280 x 720 in 256 x 64 tiles: {first:?}");
    assert_eq!(
        first.dirty_tiles, first.tiles,
        "the first frame draws everything: {first:?}"
    );
    hud(&mut list, &mut fonts, &ammo, 0.8);
    frame(&device, &mut layer, &mut list);
    let second = layer.gpu_stats().tiles;
    assert_eq!(
        second.dirty_tiles, 0,
        "an unchanged HUD redraws no tile: {second:?}"
    );
    assert_eq!(second.dirty_runs, 0, "{second:?}");
    assert_eq!(second.redrawn_instances, 0, "{second:?}");
    assert_eq!(second.clean_frames, 1, "{second:?}");
    assert!(second.painted_tiles > 4, "{second:?}");
}

#[test]
fn hud_one_changed_value_redraws_one_tile_row() {
    let device = device();
    let mut layer = retained_layer(&device);
    let mut list = Draw2dList::new();
    let mut fonts = fonts_and_digits(&mut list);
    let thirty = shaped(&mut fonts, "30", 28.0);
    let twenty_nine = shaped(&mut fonts, "29", 28.0);
    hud(&mut list, &mut fonts, &thirty, 0.8);
    frame(&device, &mut layer, &mut list);
    hud(&mut list, &mut fonts, &twenty_nine, 0.8);
    frame(&device, &mut layer, &mut list);
    let t = layer.gpu_stats().tiles;
    assert_eq!(t.dirty_runs, 1, "one changed value, one run: {t:?}");
    assert!((1..=2).contains(&t.dirty_tiles), "{t:?}");
    let glyphs = twenty_nine.glyphs.len() as u32;
    assert_eq!(
        t.redrawn_instances, glyphs,
        "only the count's glyphs touch its tile: {t:?}"
    );
}

#[test]
fn hud_steady_frame_uploads_nothing() {
    let device = device();
    let mut layer = retained_layer(&device);
    let mut list = Draw2dList::new();
    let mut fonts = fonts_and_digits(&mut list);
    let ammo = shaped(&mut fonts, "30", 28.0);
    hud(&mut list, &mut fonts, &ammo, 0.8);
    frame(&device, &mut layer, &mut list);
    assert!(layer.gpu_stats().bytes_uploaded > 0);
    let created = layer.gpu_stats();
    for n in 0..3 {
        hud(&mut list, &mut fonts, &ammo, 0.8);
        frame(&device, &mut layer, &mut list);
        let s = layer.gpu_stats();
        assert_eq!(s.bytes_uploaded, 0, "steady frame {n} uploads nothing: {s:?}");
        assert_eq!(
            (s.buffers, s.textures, s.pipelines),
            (created.buffers, created.textures, created.pipelines),
            "and creates nothing"
        );
    }
    // A changed health value uploads only the instances of its tiles.
    hud(&mut list, &mut fonts, &ammo, 0.5);
    frame(&device, &mut layer, &mut list);
    let s = layer.gpu_stats();
    assert_eq!(
        s.bytes_uploaded,
        u64::from(s.tiles.redrawn_instances) * Draw2dList::INSTANCE_BYTES,
        "{s:?}"
    );
    assert!(s.tiles.redrawn_instances < list.instances().len() as u32, "{s:?}");
}

#[test]
fn hud_partial_redraw_matches_a_full_redraw() {
    let device = device();
    let mut layer = retained_layer(&device);
    let mut list = Draw2dList::new();
    let mut fonts = fonts_and_digits(&mut list);
    let thirty = shaped(&mut fonts, "30", 28.0);
    let seven = shaped(&mut fonts, "7", 28.0);
    hud(&mut list, &mut fonts, &thirty, 0.8);
    frame(&device, &mut layer, &mut list);
    hud(&mut list, &mut fonts, &seven, 0.35);
    let partial = frame(&device, &mut layer, &mut list);
    assert!(layer.gpu_stats().tiles.dirty_tiles < layer.gpu_stats().tiles.tiles);

    let mut fresh = retained_layer(&device);
    let mut fresh_list = Draw2dList::new();
    let mut fresh_fonts = fonts_and_digits(&mut fresh_list);
    let seven = shaped(&mut fresh_fonts, "7", 28.0);
    hud(&mut fresh_list, &mut fresh_fonts, &seven, 0.35);
    let full = frame(&device, &mut fresh, &mut fresh_list);
    let differing = partial.iter().zip(&full).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "the partial redraw left stale or missing pixels");
}

#[test]
fn hud_retained_matches_direct() {
    let device = device();
    let mut list = Draw2dList::new();
    let mut fonts = fonts_and_digits(&mut list);
    let ammo = shaped(&mut fonts, "30", 28.0);
    let mut retained = retained_layer(&device);
    hud(&mut list, &mut fonts, &ammo, 0.8);
    let a = frame(&device, &mut retained, &mut list);
    // A list of its own: a list hands its atlas pages over once, to the layer it drew with.
    let mut direct = Draw2dLayer::new();
    direct.warm(&device, FORMAT);
    let mut direct_list = Draw2dList::new();
    let mut direct_fonts = fonts_and_digits(&mut direct_list);
    let direct_ammo = shaped(&mut direct_fonts, "30", 28.0);
    hud(&mut direct_list, &mut direct_fonts, &direct_ammo, 0.8);
    let b = frame(&device, &mut direct, &mut direct_list);
    let worst = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    assert!(worst <= 3, "retained vs direct differ by {worst}/255");
    assert_eq!(
        direct.gpu_stats().tiles.tiles,
        0,
        "the direct path tracks no tiles"
    );
}
