// SPDX-License-Identifier: MPL-2.0
//! `regia_quick::draw2d` on the GPU (headless; lavapipe in CI): a fixed HUD tile drawn by the
//! instanced [`Draw2dLayer`] matches the CPU reference rasterization of the same list
//! ([`regia_quick::draw2d::reference`]), segment edges are anti-aliased, and once `warm()` ran the
//! first frame creates no GPU object.
//!
//! Tolerance of the tile compare (stated, not tuned per run): every channel of at least 99.5 % of
//! the pixels is within 3/255 of the reference (8-bit storage rounds once per blend on both sides),
//! and no pixel is off by more than 40/255 (a rasterizer may place an edge pixel's sample on the
//! other side of a quad edge). TODO(lead): bless `goldens/draw2d_hud_tile.png` from this render on
//! the build server lavapipe reference and add the PNG compare; lanes cannot bless goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_core::math::{Color, Vec2};
use regia_quick::draw2d::{Draw2dImage, Draw2dLayer, Draw2dList, PxRect, reference};
use regia_render::{ImmediateImage, ImmediateImageFormat, RenderDevice};
use regia_ui::Localization;
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};

const W: u32 = 256;
const H: u32 = 128;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (lavapipe)")
}

fn target(device: &RenderDevice, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("draw2d test target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Clears `texture` to `clear` (RGBA8 as stored), draws `list` with `layer`, and reads it back.
fn render(
    device: &RenderDevice,
    layer: &mut Draw2dLayer,
    list: &mut Draw2dList,
    format: wgpu::TextureFormat,
    clear: [u8; 4],
) -> Vec<u8> {
    let texture = target(device, format);
    let view = texture.create_view(&Default::default());
    let mut encoder = device.device.create_command_encoder(&Default::default());
    let decode = |v: u8| {
        let v = f64::from(v) / 255.0;
        if format.is_srgb() {
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        } else {
            v
        }
    };
    {
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("draw2d test clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: decode(clear[0]),
                        g: decode(clear[1]),
                        b: decode(clear[2]),
                        a: f64::from(clear[3]) / 255.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    layer.draw_list(device, &mut encoder, &view, format, W, H, list);
    readback(device, encoder, &texture)
}

fn readback(device: &RenderDevice, mut encoder: wgpu::CommandEncoder, texture: &wgpu::Texture) -> Vec<u8> {
    let row = W * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("draw2d readback"),
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

/// A 16 × 16 icon: a premultiplied amber diamond on transparent texels.
fn icon() -> ImmediateImage {
    let mut data = vec![0u8; 16 * 16 * 4];
    for y in 0..16i32 {
        for x in 0..16i32 {
            if (x - 8).abs() + (y - 8).abs() <= 6 {
                let i = ((y * 16 + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[255, 176, 32, 255]);
            }
        }
    }
    ImmediateImage {
        width: 16,
        height: 16,
        format: ImmediateImageFormat::Rgba8SrgbPremultiplied,
        data: data.into(),
    }
}

const ICON: Draw2dImage = Draw2dImage(0);

/// The fixed HUD tile: a translucent rounded panel, a health bar with its frame, a crosshair and a
/// diagonal pointer (segments), a ring, a halo, a gradient strip, an icon and a line of text.
fn hud_tile(list: &mut Draw2dList, fonts: &mut TextSystem) {
    let loc = Localization::default();
    let text = Text {
        spans: vec![TextSpan::lit("SCORE 1280").with_style(TextStyle {
            font_px: 14.0,
            ..Default::default()
        })],
        ..Default::default()
    };
    let shaped = fonts.shape(&text, 1.0, None, &loc);
    list.begin_frame();
    list.push_box(
        PxRect::new(4.0, 4.0, 248.0, 120.0),
        Color::from_srgb_u8(16, 20, 28, 200),
        10.0,
    );
    list.push_outline(
        PxRect::new(12.0, 84.0, 140.0, 16.0),
        Color::from_srgb_u8(220, 220, 220, 255),
        3.0,
        1.5,
    );
    list.push_box(
        PxRect::new(14.0, 86.0, 96.5, 12.0),
        Color::from_srgb_u8(64, 210, 96, 255),
        2.0,
    );
    list.push_segment(
        Vec2::new(200.0, 40.0),
        Vec2::new(232.0, 40.0),
        2.0,
        Color::from_srgb_u8(255, 255, 255, 230),
    );
    list.push_segment(
        Vec2::new(216.0, 24.0),
        Vec2::new(216.0, 56.0),
        2.0,
        Color::from_srgb_u8(255, 255, 255, 230),
    );
    list.push_round_segment(
        Vec2::new(170.0, 110.0),
        Vec2::new(236.0, 72.3),
        3.0,
        Color::from_srgb_u8(255, 80, 64, 255),
    );
    list.push_outline(
        PxRect::new(196.0, 20.0, 40.0, 40.0),
        Color::from_srgb_u8(120, 200, 255, 160),
        20.0,
        2.0,
    );
    list.push_image(
        ICON,
        PxRect::new(14.0, 14.0, 16.0, 16.0),
        [0.0, 0.0, 1.0, 1.0],
        Color::WHITE,
    );
    list.push_text(
        fonts,
        &shaped,
        Vec2::new(36.0, 14.0),
        Color::from_srgb_u8(240, 240, 240, 255),
    );
    // A halo (a soft disc) and a gradient strip.
    list.push_soft_box(
        PxRect::new(150.0, 30.0, 30.0, 30.0),
        Color::from_srgb_u8(80, 160, 255, 200),
        15.0,
        10.0,
    );
    list.push_gradient_box(
        PxRect::new(120.0, 104.0, 120.0, 8.0),
        Color::from_srgb_u8(255, 64, 32, 255),
        Color::from_srgb_u8(32, 64, 255, 64),
        2.0,
        true,
    );
    list.set_clip(Some(PxRect::new(12.0, 50.0, 60.0, 20.0)));
    list.push_box(
        PxRect::new(0.0, 40.0, 200.0, 40.0),
        Color::from_srgb_u8(255, 255, 0, 120),
        0.0,
    );
    list.set_clip(None);
    list.end_frame();
}

#[test]
fn draw2d_matches_the_golden_hud_tile() {
    let device = device();
    let mut fonts = TextSystem::embedded_only();
    let mut list = Draw2dList::new();
    let image = icon();
    list.set_image(ICON, image.clone());
    hud_tile(&mut list, &mut fonts);
    assert!(
        list.stats().glyphs > 5,
        "the text shaped glyphs: {:?}",
        list.stats()
    );

    let clear = [10, 12, 18, 255];
    let mut expected = vec![0u8; (W * H * 4) as usize];
    for p in expected.as_chunks_mut::<4>().0 {
        *p = clear;
    }
    reference::rasterize(&list, &[Some(&image)], &mut expected, W, H, FORMAT.is_srgb());

    let mut layer = Draw2dLayer::new();
    layer.warm(&device, FORMAT);
    let got = render(&device, &mut layer, &mut list, FORMAT, clear);

    let mut off = 0usize;
    let mut worst = 0u8;
    for (g, e) in got.as_chunks::<4>().0.iter().zip(expected.as_chunks::<4>().0) {
        let d = (0..4).map(|c| g[c].abs_diff(e[c])).max().unwrap_or(0);
        worst = worst.max(d);
        if d > 3 {
            off += 1;
        }
    }
    let total = (W * H) as usize;
    if regia_core::brand::env_var_os("REGIA_QUICK_OUT").is_some() {
        let dir = std::path::PathBuf::from(regia_core::brand::env_var_os("REGIA_QUICK_OUT").unwrap());
        std::fs::create_dir_all(&dir).expect("review dir");
        image::RgbaImage::from_raw(W, H, got.clone())
            .unwrap()
            .save(dir.join("draw2d_hud_tile.png"))
            .unwrap();
        image::RgbaImage::from_raw(W, H, expected.clone())
            .unwrap()
            .save(dir.join("draw2d_hud_tile_reference.png"))
            .unwrap();
    }
    assert!(
        off * 200 <= total,
        "{off} of {total} pixels differ by more than 3/255 (worst {worst})"
    );
    assert!(worst <= 40, "worst channel difference {worst}/255");
    // Not a blank frame: the tile holds the panel, the bar, the text and the icon.
    let colours: std::collections::HashSet<[u8; 4]> = got.as_chunks::<4>().0.iter().copied().collect();
    assert!(colours.len() > 40, "only {} distinct colours", colours.len());
    let at = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [got[i], got[i + 1], got[i + 2]]
    };
    let bar = at(50, 92);
    assert!(bar[1] > 150 && bar[0] < 120, "the health bar is green: {bar:?}");
    let icon_px = at(22, 22);
    assert!(
        icon_px[0] > 200 && icon_px[2] < 100,
        "the icon is amber: {icon_px:?}"
    );
    let clipped = at(100, 60);
    let inside = at(40, 60);
    assert!(
        inside[0] > clipped[0] + 40,
        "the clip keeps the yellow box inside it: {inside:?} vs {clipped:?}"
    );
}

#[test]
fn segment_edges_are_antialiased() {
    let device = device();
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut list = Draw2dList::new();
    list.begin_frame();
    // A shallow diagonal: its long edges cross pixel rows at every fraction.
    list.push_segment(Vec2::new(20.0, 30.0), Vec2::new(230.0, 90.0), 4.0, Color::WHITE);
    list.end_frame();
    let mut layer = Draw2dLayer::new();
    layer.warm(&device, format);
    let got = render(&device, &mut layer, &mut list, format, [0, 0, 0, 0]);
    let alpha: Vec<u8> = got.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    let partial = alpha.iter().filter(|a| **a > 0 && **a < 255).count();
    let full = alpha.iter().filter(|a| **a == 255).count();
    assert!(full > 400, "the segment's core is opaque: {full} pixels");
    // Every column the segment crosses has partly covered pixels on both long edges.
    assert!(
        partial > 2 * 180,
        "{partial} edge pixels with coverage strictly between 0 and 1"
    );
    for x in 40..210u32 {
        let column: Vec<u8> = (0..H).map(|y| alpha[(y * W + x) as usize]).collect();
        let first = column
            .iter()
            .position(|a| *a > 0)
            .expect("the segment crosses the column");
        assert!(
            column[first] < 255,
            "column {x}: the top edge pixel is fully covered (no anti-aliasing)"
        );
        let last = column
            .iter()
            .rposition(|a| *a > 0)
            .expect("the segment crosses the column");
        assert!(
            column[last] < 255,
            "column {x}: the bottom edge pixel is fully covered"
        );
    }
    // The CPU reference agrees on the coverage.
    let mut expected = vec![0u8; (W * H * 4) as usize];
    reference::rasterize(&list, &[], &mut expected, W, H, false);
    let worst = got
        .iter()
        .zip(&expected)
        .map(|(g, e)| g.abs_diff(*e))
        .max()
        .unwrap_or(0);
    assert!(worst <= 3, "GPU and reference coverage differ by {worst}/255");
}

#[test]
fn the_first_frame_after_warm_creates_nothing() {
    let device = device();
    let mut fonts = TextSystem::embedded_only();
    let mut list = Draw2dList::new();
    let mut layer = Draw2dLayer::new();
    layer.warm(&device, FORMAT);
    let warmed = layer.gpu_stats();
    assert!(warmed.pipelines >= 1 && warmed.textures >= 1, "{warmed:?}");
    hud_tile(&mut list, &mut fonts);
    let _ = render(&device, &mut layer, &mut list, FORMAT, [0, 0, 0, 255]);
    let first = layer.gpu_stats();
    assert_eq!(
        first.pipelines, warmed.pipelines,
        "a pipeline was built on the first frame"
    );
    assert_eq!(
        first.buffers, warmed.buffers,
        "a buffer was created on the first frame"
    );
    assert_eq!(
        first.textures, warmed.textures,
        "a texture was created on the first frame"
    );
    assert!(first.bytes_uploaded > 0, "the first frame uploaded its instances");
    // Steady frames upload the instances (and nothing else) and create nothing.
    for _ in 0..3 {
        hud_tile(&mut list, &mut fonts);
        let _ = render(&device, &mut layer, &mut list, FORMAT, [0, 0, 0, 255]);
    }
    let steady = layer.gpu_stats();
    assert_eq!(
        (steady.pipelines, steady.buffers, steady.textures),
        (first.pipelines, first.buffers, first.textures)
    );
    assert_eq!(
        steady.bytes_uploaded,
        u64::from(list.stats().instances) * Draw2dList::INSTANCE_BYTES,
        "a steady frame uploads only its instances: {steady:?}"
    );
}
