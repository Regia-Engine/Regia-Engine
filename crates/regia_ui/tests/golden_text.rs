// SPDX-License-Identifier: MPL-2.0
//! Golden images for Latin / Arabic / CJK text and a minimal gallery strip (`specs/18` § 7).
//!
//! CI runs this on Mesa lavapipe. Bless with:
//! `REGIA_BLESS=1 cargo test -p regia_ui --test golden_text`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use image::RgbaImage;
use regia_render::RenderDevice;
use regia_ui::atlas::PAGE_SIZE;
use regia_ui::draw::{TextureRef, UiQuad};
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};
use regia_ui::{Localization, UiColor, UiRenderer};

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens")
}

fn bless() -> bool {
    regia_core::brand::env_var_os("REGIA_BLESS").is_some()
}

fn assert_golden(name: &str, width: u32, height: u32, pixels: &[u8]) {
    let path = goldens_dir().join(format!("{name}.png"));
    if bless() {
        std::fs::create_dir_all(goldens_dir()).expect("goldens dir");
        let img = RgbaImage::from_raw(width, height, pixels.to_vec()).expect("rgba");
        img.save(&path).expect("save golden");
        return;
    }
    let Ok(expected) = image::open(&path) else {
        panic!("missing golden {path:?}; bless with REGIA_BLESS=1 cargo test -p regia_ui --test golden_text");
    };
    let expected = expected.to_rgba8();
    assert_eq!(expected.dimensions(), (width, height), "{name} size");
    let actual = RgbaImage::from_raw(width, height, pixels.to_vec()).expect("rgba");
    let score = image_compare::rgba_hybrid_compare(&expected, &actual)
        .expect("compare")
        .score;
    assert!(score >= 0.98, "{name}: hybrid SSIM {score} below 0.98");
}

fn readback_ui(device: &RenderDevice, renderer: &mut UiRenderer, width: u32, height: u32) -> Vec<u8> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui golden"),
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
        label: Some("ui golden readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui golden"),
        });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.06,
                        g: 0.08,
                        b: 0.1,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    renderer.draw(device, &mut encoder, &view, format, width, height);
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
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map").expect("map ok");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    pixels
}

fn render_shaped_text(
    device: &RenderDevice,
    sys: &mut TextSystem,
    loc: &Localization,
    text: &Text,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let shaped = sys.shape(text, 1.0, Some(width as f32 - 16.0), loc);
    assert!(!shaped.glyphs.is_empty(), "expected glyphs");
    let mut renderer = UiRenderer::default();
    let mut quads = vec![UiQuad::fill(
        [0.0, 0.0],
        [width as f32, height as f32],
        UiColor::from_hex("#10141a").unwrap_or(UiColor([0.06, 0.08, 0.1, 1.0])),
    )];
    let x0 = 8.0f32;
    let y0 = 16.0f32;
    for glyph in &shaped.glyphs {
        let key = glyph.cache_key;
        let Some((place, offset)) = renderer.atlas_mut().get_or_insert(key, || sys.glyph_image(key)) else {
            continue;
        };
        let uv = [
            place.rect[0] as f32 / PAGE_SIZE as f32,
            place.rect[1] as f32 / PAGE_SIZE as f32,
            (place.rect[0] + place.rect[2]) as f32 / PAGE_SIZE as f32,
            (place.rect[1] + place.rect[3]) as f32 / PAGE_SIZE as f32,
        ];
        let gx = x0 + (glyph.pen[0] + offset[0]) as f32;
        let gy = y0 + (glyph.pen[1] + offset[1]) as f32;
        quads.push(UiQuad {
            pos: [gx, gy],
            size: [place.rect[2] as f32, place.rect[3] as f32],
            uv,
            color: glyph.color,
            texture: match place.kind {
                1 => TextureRef::ColorAtlas(place.page),
                _ => TextureRef::MonoAtlas(place.page),
            },
            alpha: 1.0,
            z: 1.0,
            ..UiQuad::fill([gx, gy], [1.0, 1.0], glyph.color)
        });
    }
    renderer.set_quads(quads);
    readback_ui(device, &mut renderer, width, height)
}

#[test]
fn golden_latin_arabic_cjk_and_gallery_strip() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    // The engine's default embedded set now covers Latin, Arabic and CJK by itself
    // (regia_ui::text bundles the Noto subsets); no gallery fonts are loaded here, so
    // the goldens exercise the same default stack every platform ships.
    let mut sys = TextSystem::embedded_only();
    let loc = Localization::default();
    let samples = [
        ("latin", "The quick brown fox", None),
        ("arabic", "مرحبا بالعالم", None),
        // 漢字 are the characters that shipped as tofu in round 4 (the old SC subset
        // predated them); keeping them in the golden pins the coverage visually.
        ("cjk", "你好世界 漢字", None),
    ];
    for (name, sample, family) in samples {
        let text = Text {
            spans: vec![TextSpan::lit(sample).with_style(TextStyle {
                font_px: 28.0,
                color: UiColor::WHITE,
                family: family.map(str::to_owned),
                ..Default::default()
            })],
            ..Default::default()
        };
        let pixels = render_shaped_text(&device, &mut sys, &loc, &text, 320, 64);
        assert!(pixels.iter().any(|&b| b > 20), "{name}: blank image");
        assert_golden(name, 320, 64, &pixels);
    }

    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![
        UiQuad::fill([0.0, 0.0], [320.0, 96.0], UiColor::from_hex("#10141a").unwrap()),
        UiQuad::fill([16.0, 24.0], [80.0, 40.0], UiColor::from_hex("#3d9df0").unwrap()),
        UiQuad::fill([112.0, 24.0], [80.0, 40.0], UiColor::from_hex("#181e27").unwrap()),
        UiQuad::fill([208.0, 24.0], [80.0, 40.0], UiColor::from_hex("#e2544c").unwrap()),
    ]);
    let pixels = readback_ui(&device, &mut renderer, 320, 96);
    assert_golden("gallery_strip", 320, 96, &pixels);
}
