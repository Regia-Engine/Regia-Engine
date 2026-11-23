// SPDX-License-Identifier: MPL-2.0
//! Live viewport panes from GPU textures (ADR-1555): the GPU target holds the same bytes the CPU
//! readback returns, a panel registers/rebinds/frees its egui texture across frames, resizes and
//! closes without leaking, the CPU path stays the fallback, and the stats and agent methods
//! report the path.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::pane_texture::{EguiNative, PaneKey, PanePath, PaneTextures};
use regia_editor::render::{PaneRequest, render_pane, render_pane_gpu};
use regia_editor::viewport::{EditorCamera, ViewportPanelState, ViewportStats, viewport_panel};
use regia_editor::views::ViewKind;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Two proxy cubes either side of the origin.
fn world() -> World {
    let mut world = World::new();
    for x in [-1.5, 1.5] {
        world.spawn(Transform {
            translation: Vec3::new(x, 0.5, 0.0),
            ..Default::default()
        });
    }
    world
}

fn camera() -> EditorCamera {
    EditorCamera {
        target: Vec3::new(0.0, 0.5, 0.0),
        distance: 7.0,
        yaw: 0.6,
        pitch: 0.3,
        ..EditorCamera::default()
    }
}

fn request(pane: usize, width: u32, height: u32) -> PaneRequest {
    PaneRequest {
        pane,
        kind: ViewKind::Perspective,
        camera: camera(),
        width,
        height,
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    }
}

/// Copies an `Rgba8Unorm` target back as tightly packed RGBA8.
fn read_target(device: &regia_render::RenderDevice, target: &regia_render::OffscreenTarget) -> Vec<u8> {
    let row = target.width * 4;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pane test readback"),
        size: u64::from(padded) * u64::from(target.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pane test readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(target.height),
            },
        },
        wgpu::Extent3d {
            width: target.width,
            height: target.height,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map callback").expect("map");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    let mut pixels = Vec::with_capacity((row * target.height) as usize);
    for y in 0..target.height as usize {
        let start = y * padded as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    pixels
}

#[test]
fn the_gpu_target_holds_the_bytes_the_cpu_readback_returns() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    // Two panes, two renderers in the same fresh state: temporal history is per renderer, so
    // the second render is not the first one's successor.
    let cpu = render_pane(&device, &mut world, &request(0, 320, 200)).expect("cpu pane");
    let gpu = render_pane_gpu(&device, &mut world, &request(1, 320, 200)).expect("gpu pane");
    assert_eq!(gpu.size(), [320, 200]);
    assert_eq!(gpu.target.texture.format(), wgpu::TextureFormat::Rgba8Unorm);
    assert!(
        gpu.target
            .texture
            .usage()
            .contains(wgpu::TextureUsages::TEXTURE_BINDING),
        "egui samples the target"
    );
    let gpu_bytes = read_target(&device, &gpu.target);
    let cpu_bytes: Vec<u8> = cpu.image.pixels.iter().flat_map(|c| c.to_array()).collect();
    assert_eq!(gpu_bytes.len(), cpu_bytes.len());
    let differing = gpu_bytes.iter().zip(&cpu_bytes).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "GPU and CPU paths are pixel-identical");
}

#[test]
fn a_pane_with_a_reference_image_has_no_gpu_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    let mut with_reference = request(0, 64, 64);
    with_reference.reference = Some(regia_editor::reference::ReferenceImage::new(
        std::sync::Arc::new(egui::ColorImage::filled([2, 2], egui::Color32::WHITE)),
        regia_editor::reference::ReferenceMode::Over,
        0.5,
    ));
    assert!(
        render_pane_gpu(&device, &mut world, &with_reference).is_none(),
        "the reference is composited on the CPU, so the caller reads back"
    );
}

/// Every texture id the frame's shapes sample.
fn texture_ids(output: &egui::FullOutput) -> Vec<egui::TextureId> {
    fn walk(shape: &egui::Shape, out: &mut Vec<egui::TextureId>) {
        match shape {
            egui::Shape::Mesh(mesh) => out.push(mesh.texture_id),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut ids = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut ids);
    }
    ids
}

fn user_ids(output: &egui::FullOutput) -> Vec<egui::TextureId> {
    texture_ids(output)
        .into_iter()
        .filter(|id| matches!(id, egui::TextureId::User(_)))
        .collect()
}

/// One panel frame at `size` points; the stand-in for the shell's `run_ui`.
fn panel_frame(
    ctx: &egui::Context,
    world: &mut World,
    device: &regia_render::RenderDevice,
    state: &mut ViewportPanelState,
    size: egui::Vec2,
) -> egui::FullOutput {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };
    ctx.run_ui(input, |ui| {
        viewport_panel(ui, world, Some(device), state);
    })
}

fn egui_renderer(device: &regia_render::RenderDevice) -> egui_wgpu::Renderer {
    egui_wgpu::Renderer::new(
        &device.device,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        Default::default(),
    )
}

#[test]
fn a_panel_registers_rebinds_and_frees_its_native_texture_without_leaking() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    world.insert_resource(camera());
    let textures = PaneTextures::new(PanePath::Gpu);
    world.insert_resource(textures.clone());
    let mut renderer = egui_renderer(&device);
    let ctx = egui::Context::default();
    let mut state = ViewportPanelState::default();
    let apply = |textures: &PaneTextures, renderer: &mut egui_wgpu::Renderer| {
        textures.apply(&mut EguiNative {
            renderer,
            device: &device.device,
        });
    };

    // Frame 1: the slot is new, so the picture is not paintable yet.
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, egui::vec2(480.0, 300.0));
    out.textures_delta.clear();
    assert!(user_ids(&out).is_empty(), "no id before the first apply");
    apply(&textures, &mut renderer);
    let stats = textures.stats();
    assert_eq!((stats.live, stats.registered, stats.freed), (1, 1, 0));

    // Frame 2: painted with the registered user texture.
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, egui::vec2(480.0, 300.0));
    out.textures_delta.clear();
    let ids = user_ids(&out);
    assert!(!ids.is_empty(), "the pane paints a user texture");
    let id = ids[0];
    assert!(
        renderer.texture(&id).is_some(),
        "egui renders the registered texture"
    );
    apply(&textures, &mut renderer);
    assert_eq!(textures.stats().registered, 1, "a settled pane registers once");

    // Resize: the renderer's target changes, the id stays, nothing new is registered.
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, egui::vec2(640.0, 360.0));
    out.textures_delta.clear();
    assert_eq!(user_ids(&out).first(), Some(&id), "the id survives a resize");
    apply(&textures, &mut renderer);
    let stats = textures.stats();
    assert_eq!((stats.live, stats.registered, stats.freed), (1, 1, 0));
    assert!(
        stats.rebound >= 2,
        "every submitted frame rebinds the current target"
    );
    assert!(
        stats.last_size[0] > 480 / 2,
        "the last picture follows the new size"
    );

    // The pane closes (a frame without it): the texture is freed from egui's renderer.
    apply(&textures, &mut renderer);
    let stats = textures.stats();
    assert_eq!((stats.live, stats.registered, stats.freed), (0, 1, 1));
    assert!(renderer.texture(&id).is_none(), "the native texture was released");

    // Reopened: a fresh registration, balanced again after the next close.
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, egui::vec2(480.0, 300.0));
    out.textures_delta.clear();
    apply(&textures, &mut renderer);
    assert_eq!(textures.stats().live, 1);
    apply(&textures, &mut renderer);
    let stats = textures.stats();
    assert_eq!((stats.live, stats.registered, stats.freed), (0, 2, 2));
}

#[test]
fn the_cpu_path_is_the_fallback_and_frees_every_slot() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    world.insert_resource(camera());
    let textures = PaneTextures::new(PanePath::Gpu);
    world.insert_resource(textures.clone());
    let mut renderer = egui_renderer(&device);
    let ctx = egui::Context::default();
    let mut state = ViewportPanelState::default();
    let size = egui::vec2(400.0, 260.0);
    let apply = |renderer: &mut egui_wgpu::Renderer| {
        textures.apply(&mut EguiNative {
            renderer,
            device: &device.device,
        });
    };
    for _ in 0..2 {
        let mut out = panel_frame(&ctx, &mut world, &device, &mut state, size);
        out.textures_delta.clear();
        apply(&mut renderer);
    }
    assert_eq!(textures.stats().live, 1);

    textures.set_path(PanePath::Cpu);
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, size);
    assert!(
        user_ids(&out).is_empty(),
        "the fallback uploads a managed egui texture"
    );
    assert!(
        texture_ids(&out)
            .iter()
            .any(|id| matches!(id, egui::TextureId::Managed(_))),
        "the picture is an egui-managed texture"
    );
    out.textures_delta.clear();
    apply(&mut renderer);
    let stats = textures.stats();
    assert_eq!((stats.live, stats.registered, stats.freed), (0, 1, 1));
    let recorded = world.resource::<ViewportStats>().pane_path;
    assert_eq!(recorded, Some(PanePath::Cpu));
}

#[test]
fn a_world_without_pane_textures_reads_back() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    world.insert_resource(camera());
    let ctx = egui::Context::default();
    let mut state = ViewportPanelState::default();
    let mut out = panel_frame(&ctx, &mut world, &device, &mut state, egui::vec2(320.0, 220.0));
    out.textures_delta.clear();
    assert!(user_ids(&out).is_empty());
    assert_eq!(world.resource::<ViewportStats>().pane_path, Some(PanePath::Cpu));
}

#[test]
fn pane_stats_count_pictures_per_second_from_egui_time() {
    let mut stats = ViewportStats::default();
    assert_eq!(stats.pane_fps(), 0.0);
    for i in 0..=60 {
        stats.record_pane(f64::from(i) / 60.0, PanePath::Gpu);
    }
    assert_eq!(stats.pane_path, Some(PanePath::Gpu));
    assert_eq!(stats.pane_frames, 61);
    assert!((stats.pane_fps() - 60.0).abs() < 0.01, "{}", stats.pane_fps());
    assert!(
        stats.lines().iter().any(|l| l.starts_with("pane gpu")),
        "the overlay names the path: {:?}",
        stats.lines()
    );
}

#[test]
fn pane_keys_distinguish_instances_and_panes() {
    let a = PaneKey {
        instance: None,
        pane: 0,
    };
    let b = PaneKey {
        instance: Some(1),
        pane: 0,
    };
    let c = PaneKey {
        instance: Some(1),
        pane: 1,
    };
    assert!(a != b && b != c && a != c);
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    handler(world, &params)
}

#[test]
fn agent_reports_the_path_and_switches_it() {
    let mut world = World::new();
    // Headless: no pane textures, so the setting cannot change.
    assert!(call(&mut world, "editor.viewport_pane_path", json!({})).is_err());
    let stats = call(&mut world, "editor.viewport_stats", json!({})).expect("stats");
    assert_eq!(stats["pane"]["setting"], "none");
    assert!(stats["pane"]["path"].is_null());

    world.insert_resource(PaneTextures::new(PanePath::Gpu));
    let reported = call(&mut world, "editor.viewport_pane_path", json!({})).expect("report");
    assert_eq!(reported["path"], "gpu");
    let switched = call(&mut world, "editor.viewport_pane_path", json!({ "path": "cpu" })).expect("switch");
    assert_eq!(switched["path"], "cpu");
    assert!(!world.resource::<PaneTextures>().gpu_enabled());
    assert!(
        call(
            &mut world,
            "editor.viewport_pane_path",
            json!({ "path": "vulkan" })
        )
        .is_err()
    );

    let stats = call(&mut world, "editor.viewport_stats", json!({})).expect("stats");
    assert_eq!(stats["pane"]["setting"], "cpu");
    assert_eq!(stats["pane"]["textures"]["live"], 0);
}

#[test]
fn the_bench_times_both_paths_and_counts_only_rendered_frames() {
    use regia_editor::pane_bench::{BenchSpec, measure};
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let spec = BenchSpec {
        size: [64, 48],
        warmup: 1,
        frames: 3,
    };
    for path in [PanePath::Gpu, PanePath::Cpu] {
        let report = measure(&device, path, &spec);
        assert_eq!(report.path, path);
        assert_eq!(report.frames, 3, "{path:?}");
        assert_eq!(report.frame_ms.len(), 3, "{path:?}");
        assert!(report.frame_ms.iter().all(|ms| *ms > 0.0), "{path:?}");
    }
}
