// SPDX-License-Identifier: MPL-2.0
//! End to end: the calibration screen built by its plugin, laid out by `regia_ui`, drawn by
//! the UI renderer over a stand-in scene (lavapipe), read back. Asserts the test patterns land
//! on the exact code values the calibration math assumes and that the preview strip shows the
//! scene. `REGIA_EVIDENCE_DIR=<dir>` also writes `calibration-<page>.png` there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_framework::calibration_screen::{CalibrationPage, CalibrationScreen, CalibrationScreenPlugin};
use regia_framework::settings::GameSettings;
use regia_render::calibration::{LOGO_HIDDEN_SIGNAL, LOGO_VISIBLE_SIGNAL};
use regia_render::{DisplayCalibration, OutputSettings};
use regia_ui::node::{Border, ComputedNode};
use regia_ui::render::SharedUiRenderer;
use regia_ui::{UiColor, UiPlugin};

const SCENE: [u8; 4] = [150, 82, 40, 255];

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_plugins(CalibrationScreenPlugin::default());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // 50 ms per update so theme fade-ins settle within the test's frames.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ));
    app.insert_resource(DisplayCalibration::NEUTRAL)
        .insert_resource(GameSettings::default())
        .insert_resource(OutputSettings::default());
    app
}

/// Draws the app's UI over a flat "scene" colour and reads it back (sRGB RGBA8). One device
/// for the whole test: the UI renderer keeps its atlas on it.
fn draw(device: &regia_render::RenderDevice, app: &App, width: u32, height: u32) -> Vec<u8> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("calibration screen test"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &SCENE.repeat((width * height) as usize),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let row = width * 4;
    let padded_row = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("calibration screen rb"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let shared = app.world().resource::<SharedUiRenderer>().clone();
        let mut guard = shared.lock();
        guard.draw(device, &mut encoder, &view, format, width, height);
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
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().unwrap().unwrap();
    let mapped = buffer.slice(..).get_mapped_range().unwrap();
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    pixels
}

fn save_evidence(name: &str, pixels: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("evidence dir");
    let path = std::path::Path::new(&dir).join(format!("calibration-{name}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create png"));
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(pixels))
        .expect("write png");
    println!("calibration screen evidence saved to {}", path.display());
}

fn signal_color(signal: f32) -> UiColor {
    let l = regia_render::calibration::decode_srgb(signal);
    UiColor::linear(l, l, l, 1.0)
}

#[test]
fn calibration_screen_patterns_render_exact_codes() {
    let (width, height) = (1280u32, 720u32);
    let device = regia_render::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let mut app = app();
    app.world_mut().resource_mut::<CalibrationScreen>().open();
    for _ in 0..12 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<CalibrationScreen>().page(),
        Some(CalibrationPage::Brightness)
    );
    let pixels = draw(&device, &app, width, height);
    save_evidence("brightness", &pixels, width, height);
    let px = |x: f32, y: f32| {
        let i = ((y as usize) * width as usize + x as usize) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    // The logos: find the rings by colour and sample the middle of the ring band.
    let mut rings: Vec<(UiColor, ComputedNode)> = {
        let world = app.world_mut();
        world
            .query::<(&Border, &ComputedNode)>()
            .iter(world)
            .map(|(b, c)| (b.color, *c))
            .collect()
    };
    rings.retain(|(c, _)| *c == signal_color(LOGO_VISIBLE_SIGNAL) || *c == signal_color(LOGO_HIDDEN_SIGNAL));
    assert_eq!(rings.len(), 2, "two logos");
    for (color, node) in rings {
        let want = if color == signal_color(LOGO_VISIBLE_SIGNAL) {
            (LOGO_VISIBLE_SIGNAL * 255.0).round() as u8
        } else {
            (LOGO_HIDDEN_SIGNAL * 255.0).round() as u8
        };
        let [x, y, w, _] = node.rect;
        let got = px(x + w / 2.0, y + 11.0);
        assert_eq!(got, [want; 3], "logo ring code value");
        // True black just outside the ring (the panel).
        assert_eq!(px(x + w / 2.0, y - 6.0), [0, 0, 0], "black around the logo");
    }
    // The bottom strip is transparent: the scene shows through.
    assert_eq!(
        px(640.0, 700.0),
        [SCENE[0], SCENE[1], SCENE[2]],
        "scene preview strip"
    );

    // The contrast page: the solid square sits at the 2.2 mid-grey code (186).
    app.world_mut().resource_mut::<CalibrationScreen>().close(false);
    app.update();
    app.world_mut().resource_mut::<CalibrationScreen>().open();
    app.update();
    let next = {
        let world = app.world_mut();
        let mut q = world.query::<(Entity, &regia_ui::Text)>();
        q.iter(world)
            .find(|(_, t)| {
                t.spans.iter().any(|s| match &s.content {
                    regia_ui::text::TextContent::Lit(s) => s == "Next",
                    _ => false,
                })
            })
            .map(|(e, _)| e)
    };
    let next = next.expect("Next label");
    let button = app
        .world()
        .get::<ChildOf>(next)
        .map(ChildOf::parent)
        .expect("button");
    app.world_mut()
        .write_message(regia_ui::UiEvent::new(button, regia_ui::UiEventKind::Click));
    for _ in 0..12 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<CalibrationScreen>().page(),
        Some(CalibrationPage::Contrast)
    );
    let pixels = draw(&device, &app, width, height);
    save_evidence("contrast", &pixels, width, height);
    let solid = (0.5_f32.powf(1.0 / 2.2) * 255.0).round() as u8;
    let found = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[..3] == [solid; 3])
        .count();
    assert!(
        found >= 90 * 90,
        "solid mid-gray square present ({found} px of code {solid})"
    );
}
