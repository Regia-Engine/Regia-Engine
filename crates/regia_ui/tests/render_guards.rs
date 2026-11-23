// SPDX-License-Identifier: MPL-2.0
//! Guard tests that catch the Round-1 renderer bugs: half-drawn quads, garbled glyphs, and
//! baseline clipping (`specs/18` § 3 / § 7). The tests share one device
//! ([`RenderDevice::shared_headless`]): a device per parallel test hung the NVIDIA driver under load.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::atlas::PAGE_SIZE;
use regia_ui::draw::{TextureRef, UiQuad};
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};
use regia_ui::{Localization, UiColor, UiRenderer};

fn readback(device: &RenderDevice, renderer: &mut UiRenderer, width: u32, height: u32) -> Vec<u8> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui guard"),
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
        label: Some("ui guard readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ui guard"),
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
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
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

fn luma(px: &[u8]) -> u8 {
    // Rec. 709-ish; enough to tell ink from background.
    ((u16::from(px[0]) * 3 + u16::from(px[1]) * 6 + u16::from(px[2])) / 10) as u8
}

#[test]
fn filled_rect_covers_at_least_99_percent() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let (x0, y0, w, h) = (40u32, 30u32, 120u32, 80u32);
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![UiQuad::fill(
        [x0 as f32, y0 as f32],
        [w as f32, h as f32],
        UiColor([1.0, 0.0, 0.0, 1.0]),
    )]);
    let pixels = readback(&device, &mut renderer, 256, 160);
    let mut covered = 0u32;
    for py in y0..y0 + h {
        for px in x0..x0 + w {
            let i = ((py * 256 + px) * 4) as usize;
            // Red fill against black clear: R channel must dominate.
            if pixels[i] > 200 && pixels[i + 1] < 40 && pixels[i + 2] < 40 {
                covered += 1;
            }
        }
    }
    let area = w * h;
    let ratio = f64::from(covered) / f64::from(area);
    assert!(
        ratio >= 0.99,
        "filled rect coverage {ratio:.4} (< 0.99); half-quad / topology bug?"
    );
}

#[test]
fn letter_h_is_left_right_symmetric() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut sys = TextSystem::embedded_only();
    let loc = Localization::default();
    let text = Text {
        spans: vec![TextSpan::lit("H").with_style(TextStyle {
            font_px: 64.0,
            color: UiColor::WHITE,
            ..Default::default()
        })],
        ..Default::default()
    };
    let shaped = sys.shape(&text, 1.0, Some(200.0), &loc);
    assert!(!shaped.glyphs.is_empty(), "expected an H glyph");

    let mut renderer = UiRenderer::default();
    let mut quads = vec![UiQuad::fill(
        [0.0, 0.0],
        [128.0, 128.0],
        UiColor([0.0, 0.0, 0.0, 1.0]),
    )];
    let x0 = 32.0f32;
    let y0 = 16.0f32;
    let mut ink_box = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
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
        let gw = place.rect[2] as i32;
        let gh = place.rect[3] as i32;
        ink_box[0] = ink_box[0].min(gx as i32);
        ink_box[1] = ink_box[1].min(gy as i32);
        ink_box[2] = ink_box[2].max(gx as i32 + gw);
        ink_box[3] = ink_box[3].max(gy as i32 + gh);
        quads.push(UiQuad {
            pos: [gx, gy],
            size: [place.rect[2] as f32, place.rect[3] as f32],
            uv,
            color: glyph.color,
            texture: TextureRef::MonoAtlas(place.page),
            alpha: 1.0,
            z: 1.0,
            ..UiQuad::fill([gx, gy], [1.0, 1.0], glyph.color)
        });
    }
    renderer.set_quads(quads);
    let pixels = readback(&device, &mut renderer, 128, 128);

    assert!(
        ink_box[0] < ink_box[2] && ink_box[1] < ink_box[3],
        "no ink bounds"
    );
    let (x0b, y0b, x1b, y1b) = (ink_box[0], ink_box[1], ink_box[2], ink_box[3]);
    let bw = x1b - x0b;
    let bh = y1b - y0b;
    assert!(bw >= 8 && bh >= 8, "H ink too small: {bw}x{bh}");

    let mut mismatch = 0u32;
    let mut compared = 0u32;
    for row in 0..bh {
        for col in 0..bw / 2 {
            let lx = x0b + col;
            let rx = x1b - 1 - col;
            let ly = y0b + row;
            if !(0..128).contains(&lx) || !(0..128).contains(&rx) || !(0..128).contains(&ly) {
                continue;
            }
            let li = ((ly * 128 + lx) * 4) as usize;
            let ri = ((ly * 128 + rx) * 4) as usize;
            let left = luma(&pixels[li..li + 4]);
            let right = luma(&pixels[ri..ri + 4]);
            compared += 1;
            if left.abs_diff(right) > 48 {
                mismatch += 1;
            }
        }
    }
    assert!(compared > 50, "too few pixels compared ({compared})");
    let frac = f64::from(mismatch) / f64::from(compared);
    assert!(
        frac <= 0.12,
        "H left/right asymmetry {frac:.3} (> 0.12); atlas UV / packing bug? compared={compared}"
    );
}

#[test]
fn bordered_quad_draws_ring_not_interior_wash() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    // Green panel with a 2px white border ring, over a dark clear.
    let mut border = UiQuad::fill([32.0, 32.0], [64.0, 48.0], UiColor::TRANSPARENT);
    border.color2 = UiColor([1.0, 1.0, 1.0, 1.0]);
    border.border_width = [2.0; 4];
    let panel = UiQuad::fill([32.0, 32.0], [64.0, 48.0], UiColor([0.0, 0.6, 0.2, 1.0]));
    let mut renderer = UiRenderer::default();
    renderer.set_quads(vec![panel, border]);
    let pixels = readback(&device, &mut renderer, 128, 128);

    let px = |x: usize, y: usize| {
        let i = (y * 128 + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    // Interior (well inside the 2px ring): pure panel green, no white wash. The wash would add
    // red (white) to the interior; the panel is pure green/blue.
    let mid = px(64, 56);
    assert!(
        mid[0] < 40 && mid[1] > 150,
        "interior should be the panel color, got {mid:?} (border rendered as gradient wash?)"
    );
    // Top border ring: white.
    let ring = px(64, 33);
    assert!(
        ring[0] > 200 && ring[1] > 200 && ring[2] > 200,
        "border ring should be white, got {ring:?}"
    );
    // Just inside the ring: panel green again.
    let inner = px(64, 36);
    assert!(
        inner[0] < 40 && inner[1] > 150,
        "inside the ring should be panel color, got {inner:?}"
    );
}

#[test]
fn text_ink_lies_inside_layout_box() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut sys = TextSystem::embedded_only();
    let loc = Localization::default();
    let text = Text {
        spans: vec![TextSpan::lit("Ag").with_style(TextStyle {
            font_px: 36.0,
            color: UiColor::WHITE,
            ..Default::default()
        })],
        ..Default::default()
    };
    // Layout box: content area [8,8]..[8+200,8+48] inside a 256×96 surface.
    let box_pos = [8.0f32, 8.0];
    let box_size = [200.0f32, 48.0];
    let shaped = sys.shape(&text, 1.0, Some(box_size[0]), &loc);
    assert!(!shaped.glyphs.is_empty());

    let mut renderer = UiRenderer::default();
    let mut quads = vec![
        UiQuad::fill([0.0, 0.0], [256.0, 96.0], UiColor([0.05, 0.05, 0.05, 1.0])),
        // Dim layout box so we can see it in debug dumps; not used for the assertion.
        UiQuad::fill(box_pos, box_size, UiColor([0.15, 0.15, 0.2, 1.0])),
    ];
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
        let gx = box_pos[0] + (glyph.pen[0] + offset[0]) as f32;
        let gy = box_pos[1] + (glyph.pen[1] + offset[1]) as f32;
        quads.push(UiQuad {
            pos: [gx, gy],
            size: [place.rect[2] as f32, place.rect[3] as f32],
            uv,
            color: glyph.color,
            texture: TextureRef::MonoAtlas(place.page),
            alpha: 1.0,
            z: 1.0,
            ..UiQuad::fill([gx, gy], [1.0, 1.0], glyph.color)
        });
    }
    renderer.set_quads(quads);
    let pixels = readback(&device, &mut renderer, 256, 96);

    let (bx0, by0) = (box_pos[0] as i32, box_pos[1] as i32);
    let (bx1, by1) = (
        (box_pos[0] + box_size[0]) as i32,
        (box_pos[1] + box_size[1]) as i32,
    );
    let mut ink_outside = 0u32;
    let mut ink_total = 0u32;
    for y in 0..96i32 {
        for x in 0..256i32 {
            let i = ((y * 256 + x) * 4) as usize;
            // Bright ink on the dim box / dark clear.
            if luma(&pixels[i..i + 4]) < 180 {
                continue;
            }
            ink_total += 1;
            if x < bx0 || x >= bx1 || y < by0 || y >= by1 {
                ink_outside += 1;
            }
        }
    }
    assert!(ink_total > 30, "expected visible text ink, got {ink_total}");
    let frac = f64::from(ink_outside) / f64::from(ink_total);
    assert!(
        frac <= 0.02,
        "{frac:.3} of text ink outside layout box (baseline/ascent bug?); outside={ink_outside}/{ink_total}"
    );
}

/// UI that appears after an empty boot (a HUD toggled on, the performance overlay) creates no
/// bind group and no pipeline: the first draw, with nothing to draw, already made the window's
/// screen uniforms and the format's pipeline (the integ-v036 regression: the window's uniforms
/// were made on a placeholder batch and dropped, so the first shown frame made them again).
#[test]
fn ui_shown_after_an_empty_boot_creates_nothing() {
    let device = match RenderDevice::shared_headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut renderer = UiRenderer::default();
    // Boot: two empty frames.
    readback(&device, &mut renderer, 64, 48);
    readback(&device, &mut renderer, 64, 48);
    let before = regia_render::frame_path_counters();
    renderer.set_quads(vec![UiQuad::fill(
        [8.0, 8.0],
        [32.0, 24.0],
        UiColor([1.0, 0.0, 0.0, 1.0]),
    )]);
    let pixels = readback(&device, &mut renderer, 64, 48);
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!(delta.bind_groups, 0, "the first shown frame created a bind group");
    assert_eq!(delta.pipelines, 0, "the first shown frame compiled a pipeline");
    let i = ((20 * 64 + 20) * 4) as usize;
    assert!(
        pixels[i] > 200 && pixels[i + 1] < 40,
        "the quad was not drawn: {:?}",
        &pixels[i..i + 4]
    );
}
