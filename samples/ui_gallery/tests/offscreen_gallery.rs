// SPDX-License-Identifier: MIT-0
//! End-to-end gallery render test: builds the real `UiGallery` app headless, extracts the UI
//! draw list, renders it offscreen (lavapipe) and asserts the Round-4 fixes held:
//! flat panel fills (no border-as-gradient wash), borders that survive clipping, and the
//! RTL Arabic sample starting at the right edge of its line box.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia::prelude::*;
use regia_ui::draw::UiQuad;
use regia_ui::interaction::{UiEvent, UiEventKind};
use regia_ui::render::SharedUiRenderer;
use regia_ui::{ComputedNode, ShapedText, Text};
use ui_gallery::gallery::UiGallery;

fn main() {}

/// Builds the gallery app headless with the platform messages its systems read.
fn gallery_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiGallery);
    app.add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<CursorMoved>()
        .add_message::<MouseButtonInput>()
        .add_message::<TouchInput>()
        .add_message::<MouseWheel>()
        .add_message::<KeyboardInput>()
        .add_message::<ImeEvent>();
    // Deterministic clock: advance 50 ms per update so theme transitions settle in-test.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ));
    app
}

/// The Arabic widgets-page sample ("مرحبا بالعالم — النص العربي").
const ARABIC_SAMPLE: &str = "مرحبا بالعالم — النص العربي";

#[test]
fn gallery_renders_offscreen_with_round4_fixes() {
    let mut app = gallery_app();
    for _ in 0..10 {
        app.update();
    }

    // 1. RTL alignment at the data level: the Arabic line's first (visually rightmost) glyph
    //    starts in the right half of its layout box, and the ink's left edge is inside the box.
    let mut arabic: Option<(Entity, ComputedNode, ShapedText)> = None;
    {
        let world = app.world_mut();
        for (entity, text, computed, shaped) in world
            .query::<(Entity, &Text, &ComputedNode, &ShapedText)>()
            .iter(world)
        {
            // The widgets-page Arabic sample: any span whose literal matches the RTL string.
            if text.spans.iter().any(|s| match &s.content {
                regia_ui::text::TextContent::Lit(s) => s == ARABIC_SAMPLE,
                _ => false,
            }) {
                arabic = Some((entity, *computed, shaped.clone()));
                break;
            }
        }
    }
    let Some((_arabic_entity, computed, shaped)) = arabic else {
        panic!("Arabic sample text was not shaped");
    };
    assert!(!shaped.glyphs.is_empty(), "Arabic sample has no glyphs");
    let node_w = computed.rect[2];
    let first_x = f64::from(shaped.glyphs[0].pen[0]);
    assert!(
        first_x > f64::from(node_w) * 0.5,
        "RTL line should start in the right half of its {node_w}px box, got pen.x={first_x}"
    );
    let min_x = shaped
        .glyphs
        .iter()
        .map(|g| f64::from(g.pen[0]))
        .fold(f64::MAX, f64::min);
    assert!(min_x >= -2.0, "RTL ink escapes the left edge ({min_x})");

    // 1b. No glyph in the whole gallery resolves to .notdef (the round-5 tofu guard at
    //     app level): the engine's default font set must cover every rendered string.
    {
        let world = app.world_mut();
        let loc = regia_ui::Localization::default();
        let mut shaped_count = 0usize;
        let mut tofu: Vec<String> = Vec::new();
        for (text, shaped) in world.query::<(&Text, &ShapedText)>().iter(world) {
            shaped_count += 1;
            if shaped.glyphs.iter().any(|g| g.cache_key.glyph_id == 0) {
                let lit: Vec<String> = text.spans.iter().map(|s| s.content.resolve(&loc)).collect();
                tofu.push(lit.join(" | "));
            }
        }
        assert!(
            shaped_count > 0,
            "no text was shaped; the gallery must shape text"
        );
        assert!(
            tofu.is_empty(),
            "shaped gallery strings still contain .notdef glyphs: {tofu:?}"
        );
    }

    // 2. Border decoration survives the unbounded root clip: at least one rect quad keeps a
    //    non-zero border width (Round-4 bug: every border was silently zeroed in `clipped`).
    let world = app.world();
    let shared = world.get_resource::<SharedUiRenderer>().expect("renderer");
    let lock = shared.lock();
    let quads: Vec<UiQuad> = lock.quads().to_vec();
    drop(lock);
    assert!(
        quads
            .iter()
            .any(|q| q.texture == regia_ui::TextureRef::Rect && q.border_width.iter().any(|w| *w > 0.5)),
        "no bordered quad survived clipping; borders would be invisible"
    );

    // 3. Offscreen render: panel interior must be flat — the border-as-gradient wash bug tinted
    //    the page brighter near its top edge. The app's own `SharedUiRenderer` is drawn (not a
    //    fresh `UiRenderer`, whose empty glyph atlas would drop every text quad); with the
    //    atlas in place the capture is the same image a windowed screenshot would show.
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let (width, height) = (1280u32, 720u32);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gallery test"),
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
        label: Some("gallery test rb"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    // Draw through the app's shared renderer: it holds the glyph atlas the text quads
    // sample. `quads` (cloned above) is exactly what the shared renderer already has. The
    // lock is held only for the draw call — later `app.update()` runs extract, which locks
    // the same renderer, so holding it across updates would deadlock.
    let shared = app.world().resource::<SharedUiRenderer>().clone();
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gallery test enc"),
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
                        g: 0.07,
                        b: 0.09,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    {
        let mut guard = shared.lock();
        guard.draw(&device, &mut encoder, &view, format, width, height);
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
    drop(mapped);
    buffer.unmap();

    // Evidence capture (M7): `REGIA_EVIDENCE_DIR=<dir> cargo test` writes the rendered
    // gallery as <dir>/ui-gallery.png for review (`docs/evidence/m7`).
    if let Ok(dir) = std::env::var("REGIA_EVIDENCE_DIR") {
        let img = image::RgbaImage::from_raw(width, height, pixels.clone()).expect("gallery pixels");
        let path = std::path::Path::new(&dir).join("ui-gallery.png");
        std::fs::create_dir_all(&dir).expect("evidence dir");
        img.save(&path).expect("save gallery evidence");
        println!("gallery evidence saved to {}", path.display());
    }

    let px = |x: usize, y: usize| {
        let i = ((y * width as usize) + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    // Two interior points of the widgets page (clear of every widget row).
    let a = px(1000, 200);
    let b = px(1000, 480);
    let c = px(300, 700);
    assert!(
        a == b && b == c,
        "page interior must be flat, got {a:?} / {b:?} / {c:?} (gradient wash over panels?)"
    );

    // 4. The click pipeline still works after the layout fixes: press+release over the
    //    "Text" toolbar button emits a Click UiEvent whose target contains the pointer.
    // No window exists headless; the UI pointer system only reads the position.
    let window = Entity::PLACEHOLDER;
    let (cx, cy) = (430.0f64, 82.0f64);
    {
        let world = app.world_mut();
        world.write_message(CursorMoved {
            window,
            position: (cx, cy),
            timestamp: regia::platform::InputTimestamp::ZERO,
        });
        world.write_message(MouseButtonInput {
            window,
            button: MouseButton::Left,
            state: ButtonState::Pressed,
            timestamp: regia::platform::InputTimestamp::ZERO,
        });
    }
    app.update();
    {
        let world = app.world_mut();
        world.write_message(CursorMoved {
            window,
            position: (cx, cy),
            timestamp: regia::platform::InputTimestamp::ZERO,
        });
        world.write_message(MouseButtonInput {
            window,
            button: MouseButton::Left,
            state: ButtonState::Released,
            timestamp: regia::platform::InputTimestamp::ZERO,
        });
    }
    app.update();
    let world = app.world();
    let clicked: Vec<[f32; 4]> = world
        .resource::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|e| e.kind == UiEventKind::Click)
        .filter_map(|e| world.get::<ComputedNode>(e.target).map(|c| c.rect))
        .collect();
    assert!(
        clicked
            .iter()
            .any(|r| r[0] <= 430.0 && r[0] + r[2] >= 430.0 && r[1] <= 82.0 && r[1] + r[3] >= 82.0),
        "press+release over the Text button must Click the node under the pointer; got {clicked:?}"
    );
}
