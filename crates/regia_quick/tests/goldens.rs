// SPDX-License-Identifier: MPL-2.0
//! Quick-draw goldens, blessed on Mesa lavapipe (`docs/06` § 2): shapes, strokes, text,
//! transforms, and 3D primitives through the forward path with a 2D HUD over them.
//!
//! Bless with `REGIA_BLESS=1 cargo test -p regia_quick --test goldens` on lavapipe only.
//! `REGIA_QUICK_OUT=<dir>` also writes every rendered frame there for review by eye.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::f32::consts::{PI, TAU};
use std::path::PathBuf;

use regia_quick::color::{hsv, srgb, srgba};
use regia_quick::frame::{FrameParams, QuickRenderer};
use regia_quick::prelude::*;
use regia_quick::{QuickOverlay, QuickSettings};
use regia_render::{
    ExtractedFrame, FrameRenderer, ImmediateBatch, ImmediateRenderer, ImmediateTarget, RenderDevice,
    RenderPlugin, encode_clear, spawn_lab_scene,
};
use regia_test_utils::golden::compare_rgba8_to_png_on;
use regia_ui::Localization;
use regia_ui::text::TextSystem;

const W: u32 = 640;
const H: u32 = 360;
const SSIM: f64 = 0.98;
const PARAMS: FrameParams = FrameParams {
    size: Vec2::new(W as f32, H as f32),
    scale_factor: 1.0,
};

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (lavapipe)")
}

fn readback(device: &RenderDevice, encoder: wgpu::CommandEncoder, texture: &wgpu::Texture) -> Vec<u8> {
    let mut encoder = encoder;
    let row = W * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quick golden readback"),
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

/// Draws `draw` over a dark grey frame (what a window shows after tonemapping) and reads it back.
fn render_2d(device: &RenderDevice, draw: &Draw) -> Vec<u8> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quick golden"),
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
    });
    let view = texture.create_view(&Default::default());
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut builder = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    builder.build(draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    let mut renderer = ImmediateRenderer::new();
    for (id, image) in builder.take_images() {
        renderer.set_image(id, image);
    }
    let mut encoder = device.device.create_command_encoder(&Default::default());
    encode_clear(&mut encoder, &view, [0.018, 0.02, 0.026, 1.0]);
    let target = ImmediateTarget {
        view: &view,
        format,
        width: W,
        height: H,
    };
    renderer
        .record(device, &mut encoder, &target, &batch, PARAMS.clip_from_local(), 4)
        .expect("record");
    readback(device, encoder, &texture)
}

/// Saves a review copy and compares with (or blesses) the golden.
fn check(name: &str, pixels: &[u8]) {
    if let Some(dir) = regia_core::brand::env_var_os("REGIA_QUICK_OUT") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("review dir");
        image::RgbaImage::from_raw(W, H, pixels.to_vec())
            .expect("rgba")
            .save(dir.join(format!("{name}.png")))
            .expect("write review png");
    }
    // A blank frame must never be blessed: the drawing covers a good share of the frame, and the
    // frame holds many distinct colours (a clear or flat background alone has one).
    let drawn = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 40 || p[1] > 40 || p[2] > 40)
        .count();
    assert!(drawn > (W * H / 50) as usize, "{name}: only {drawn} pixels drawn");
    let colours: std::collections::HashSet<[u8; 4]> = pixels.as_chunks::<4>().0.iter().copied().collect();
    assert!(
        colours.len() > 8,
        "{name}: only {} distinct colours (blank frame?)",
        colours.len()
    );
    let info = device().adapter.get_info();
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        assert_eq!(
            info.device_type,
            wgpu::DeviceType::Cpu,
            "{name}: goldens are blessed on lavapipe only, not on {}",
            info.name
        );
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{name}.png"));
    // Hardware adapters are held to the SSIM sanity floor instead of skipping the compare.
    compare_rgba8_to_png_on(&info, pixels, W, H, &path, SSIM).unwrap_or_else(|e| panic!("{name}: {e}"));
}

#[test]
fn shapes() {
    let draw = Draw::new();
    draw.background().color(srgb(0.06, 0.07, 0.09));
    // A row of filled rectangles, rotated rectangles, ellipses and triangles in hue steps.
    for i in 0..8 {
        let x = -280.0 + i as f32 * 80.0;
        let hue = i as f32 * 45.0;
        draw.rect()
            .x_y(x, 120.0)
            .w_h(56.0, 56.0)
            .color(hsv(hue, 0.65, 0.95));
        draw.rect()
            .x_y(x, 40.0)
            .w_h(48.0, 30.0)
            .rotate(i as f32 * PI / 16.0)
            .color(hsv(hue + 20.0, 0.5, 0.85));
        draw.ellipse()
            .x_y(x, -40.0)
            .w_h(30.0 + i as f32 * 4.0, 54.0 - i as f32 * 3.0)
            .color(hsv(hue + 40.0, 0.75, 0.9));
        draw.tri()
            .x_y(x, -120.0)
            .points(
                Vec2::new(-26.0, -22.0),
                Vec2::new(26.0, -22.0),
                Vec2::new(0.0, 26.0),
            )
            .color(hsv(hue + 60.0, 0.6, 1.0));
    }
    // A translucent polygon over everything and a quad.
    let star: Vec<Vec2> = (0..10)
        .map(|i| {
            let a = i as f32 * TAU / 10.0 + PI / 2.0;
            let r = if i % 2 == 0 { 70.0 } else { 30.0 };
            Vec2::new(a.cos() * r, a.sin() * r)
        })
        .collect();
    draw.polygon().points(star).color(srgba(1.0, 0.85, 0.2, 0.75));
    draw.quad()
        .points(
            Vec2::new(200.0, -170.0),
            Vec2::new(300.0, -150.0),
            Vec2::new(280.0, -80.0),
            Vec2::new(190.0, -100.0),
        )
        .color(srgba(0.3, 0.8, 1.0, 0.6));
    check("quick_shapes", &render_2d(&device(), &draw));
}

#[test]
fn strokes() {
    let draw = Draw::new();
    draw.background().color(srgb(0.08, 0.08, 0.1));
    // Polyline joins.
    let zig = |y: f32| {
        (0..7).map(move |i| Vec2::new(-280.0 + i as f32 * 40.0, y + if i % 2 == 0 { 0.0 } else { 40.0 }))
    };
    draw.polyline()
        .weight(10.0)
        .join_miter()
        .color(srgb(0.95, 0.4, 0.3))
        .points(zig(110.0));
    draw.polyline()
        .weight(10.0)
        .join_round()
        .color(srgb(0.4, 0.9, 0.5))
        .points(zig(30.0));
    draw.polyline()
        .weight(10.0)
        .join_bevel()
        .color(srgb(0.4, 0.6, 1.0))
        .points(zig(-50.0));
    // Line caps.
    for (i, cap) in ["butt", "square", "round"].iter().enumerate() {
        let y = -110.0 - i as f32 * 25.0;
        let line = draw
            .line()
            .start(Vec2::new(-260.0, y))
            .end(Vec2::new(-60.0, y))
            .weight(14.0)
            .color(srgb(0.9, 0.9, 0.85));
        match *cap {
            "square" => drop(line.caps_square()),
            "round" => drop(line.caps_round()),
            _ => drop(line),
        }
    }
    // Outlines: ellipse ring, outlined rectangle, an outlined polygon, a sine path, arrows.
    draw.ellipse()
        .x_y(120.0, 90.0)
        .radius(60.0)
        .no_fill()
        .stroke(srgb(1.0, 0.75, 0.2))
        .stroke_weight(6.0);
    draw.rect()
        .x_y(120.0, 90.0)
        .w_h(70.0, 70.0)
        .rotate(PI / 4.0)
        .color(srgb(0.2, 0.25, 0.35))
        .stroke(srgb(0.6, 0.85, 1.0))
        .stroke_weight(3.0);
    let wave = (0..=120).map(|i| {
        let t = i as f32 / 120.0;
        Vec2::new(20.0 + t * 260.0, -60.0 + (t * TAU * 2.0).sin() * 30.0)
    });
    draw.polyline()
        .weight(4.0)
        .caps_round()
        .color(srgb(0.85, 0.5, 1.0))
        .points(wave);
    for i in 0..4 {
        let a = i as f32 * PI / 6.0;
        draw.arrow()
            .start(Vec2::new(40.0, -150.0))
            .end(Vec2::new(40.0 + a.cos() * 110.0, -150.0 + a.sin() * 40.0))
            .weight(3.0)
            .color(hsv(30.0 * i as f32, 0.5, 1.0));
    }
    check("quick_strokes", &render_2d(&device(), &draw));
}

#[test]
fn text() {
    let draw = Draw::new();
    draw.background().color(srgb(0.1, 0.1, 0.13));
    draw.text("Regia quick draw")
        .font_size(40)
        .y(130.0)
        .color(srgb(1.0, 0.95, 0.85));
    draw.text("Left justified, wrapped by word inside a 260 unit wide box that is outlined in gray.")
        .font_size(16)
        .x_y(-170.0, 30.0)
        .w_h(260.0, 120.0)
        .left_justify()
        .align_text_top()
        .color(srgb(0.85, 0.9, 1.0));
    draw.rect()
        .x_y(-170.0, 30.0)
        .w_h(260.0, 120.0)
        .no_fill()
        .stroke(srgb(0.4, 0.4, 0.45))
        .stroke_weight(1.0);
    draw.text("Right\njustified\nlines")
        .font_size(20)
        .x_y(170.0, 30.0)
        .w_h(220.0, 120.0)
        .right_justify()
        .color(srgb(0.6, 1.0, 0.7));
    draw.text("rotated")
        .font_size(28)
        .x_y(-200.0, -120.0)
        .rotate(PI / 8.0)
        .color(srgb(1.0, 0.6, 0.4));
    draw.text("RAINBOW")
        .font_size(36)
        .x_y(120.0, -120.0)
        .glyph_colors((0..7).map(|i| hsv(i as f32 * 50.0, 0.7, 1.0)));
    check("quick_text", &render_2d(&device(), &draw));
}

#[test]
fn transforms() {
    let draw = Draw::new();
    draw.background().color(srgb(0.03, 0.03, 0.05));
    // A spiral of rects from nested derived draws.
    let mut d = draw.x(-150.0);
    for i in 0..36 {
        d.rect()
            .w_h(70.0, 8.0)
            .x(40.0)
            .color(hsv(i as f32 * 10.0, 0.6, 1.0));
        d = d.rotate(TAU / 36.0).scale(0.97);
    }
    // Additive glow discs.
    let glow = draw.x(150.0).blend(BlendMode::Additive);
    for (i, c) in [srgb(0.7, 0.1, 0.1), srgb(0.1, 0.6, 0.1), srgb(0.1, 0.2, 0.8)]
        .into_iter()
        .enumerate()
    {
        let a = i as f32 * TAU / 3.0;
        glow.ellipse()
            .x_y(a.cos() * 40.0, a.sin() * 40.0)
            .radius(70.0)
            .color(c);
    }
    // A clipped grid: only the scissored square shows.
    let clipped = draw.scissor(geom::Rect::from_x_y_w_h(150.0, -120.0, 100.0, 80.0));
    for i in 0..12 {
        clipped
            .line()
            .start(Vec2::new(60.0 + i as f32 * 16.0, -200.0))
            .end(Vec2::new(100.0 + i as f32 * 16.0, -40.0))
            .weight(4.0)
            .color(srgb(1.0, 1.0, 1.0));
    }
    check("quick_transforms", &render_2d(&device(), &draw));
}

fn scene_3d(draw3d: Res<Draw3d>, draw: Res<Draw>) {
    draw3d
        .plane(Vec3::ZERO, Vec2::new(4.0, 3.0))
        .color(srgb(0.5, 0.5, 0.52))
        .roughness(0.9);
    draw3d
        .cube(Vec3::new(-1.6, 0.5, 0.0), Vec3::ONE)
        .color(srgb(0.8, 0.25, 0.2))
        .rotation(Quat::from_rotation_y(0.5));
    draw3d
        .sphere(Vec3::new(0.0, 0.6, 0.0), 0.6)
        .color(srgb(0.85, 0.85, 0.9))
        .metallic(1.0)
        .roughness(0.25);
    draw3d
        .cylinder(Vec3::new(1.6, 0.0, 0.2), 0.45, 0.45, 1.2)
        .color(srgb(0.2, 0.55, 0.85));
    draw3d
        .cone(Vec3::new(0.9, 0.0, 1.5), 0.4, 0.9)
        .color(srgb(0.95, 0.75, 0.2));
    draw3d
        .sphere(Vec3::new(-0.6, 0.25, 1.6), 0.25)
        .color(srgb(0.3, 1.0, 0.5))
        .emissive(4.0);
    draw3d.cube_wires(Vec3::new(-1.6, 0.5, 0.0), Vec3::splat(1.3), srgb(1.0, 1.0, 1.0));
    draw3d.grid(
        Vec3::new(0.0, 0.01, 0.0),
        8,
        0.5,
        srgb(1.0, 0.3, 0.2),
        srgb(0.6, 0.6, 0.6),
    );
    // The HUD over the 3D frame.
    draw.rect()
        .x_y(0.0, 155.0)
        .w_h(640.0, 50.0)
        .color(srgba(0.0, 0.0, 0.0, 0.55));
    draw.text("Draw3d: lit primitives + HUD")
        .font_size(22)
        .y(155.0)
        .color(srgb(1.0, 1.0, 1.0));
}

#[test]
fn primitives_3d_with_hud() {
    let mut app = App::new();
    app.add_plugins(RenderPlugin { headless: true })
        .add_plugins(QuickPlugin);
    app.insert_resource(QuickSettings {
        fallback: PARAMS,
        ..Default::default()
    });
    app.finish_plugins();
    let mut lab = regia_test_utils::scenes::material_sphere_grid_scene();
    lab.meshes.clear();
    lab.materials.clear();
    lab.width = W;
    lab.height = H;
    lab.eye = [2.6, 3.0, 5.4];
    lab.target = [0.0, 0.4, 0.0];
    lab.directional_shadows = true;
    spawn_lab_scene(app.world_mut(), &lab);
    app.add_systems(Update, scene_3d);
    app.update();

    let device = device();
    let extracted: ExtractedFrame = app.world().resource::<ExtractedFrame>().clone();
    assert_eq!(
        extracted.meshes.len(),
        6,
        "every Draw3d primitive is a mesh instance"
    );
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(extracted.outdoor.clone());
    renderer.set_gizmos(Some(extracted.gizmos.clone()));
    let frame = extracted.frame_scene(0, W, H);
    let target = renderer
        .begin_offscreen(&device, &frame, wgpu::TextureFormat::Rgba8Unorm)
        .expect("render");
    // The quick overlay layer, exactly as the window path records it.
    let overlay = app.world().resource::<QuickOverlay>().clone();
    {
        let mut state = overlay.lock();
        let state = &mut *state;
        let mut encoder = device.device.create_command_encoder(&Default::default());
        state
            .renderer
            .record(
                &device,
                &mut encoder,
                &ImmediateTarget {
                    view: &target.view,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    width: W,
                    height: H,
                },
                &state.batch,
                PARAMS.clip_from_local(),
                state.samples,
            )
            .expect("overlay");
        device.queue.submit([encoder.finish()]);
    }
    let pixels = renderer.read_offscreen(&device, target).expect("readback").pixels;
    check("quick_3d", &pixels);
}
