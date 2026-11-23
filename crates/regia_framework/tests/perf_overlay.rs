// SPDX-License-Identifier: MPL-2.0
//! End to end: the performance overlay built by its plugin, laid out by `regia_ui`, drawn by
//! the UI renderer into an offscreen target, read back.
//!
//! - toggling it (Off → Compact → Detailed → Off → Compact) creates no pipelines and no bind
//!   groups (the UI pipeline is warm from the first frame);
//! - it draws in the top-left corner and nowhere else;
//! - its values are the engine sources' values;
//! - its cost: CPU per frame (release: ≤ 0.1 ms over the hidden state) and an upper bound on
//!   its GPU draw (real GPUs: ≤ 0.1 ms).
//!
//! `REGIA_EVIDENCE_DIR=<dir>` writes `perf-overlay-<mode>.png` and `perf-overlay-cost.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_framework::perf_overlay::{
    PerfOverlayPlugin, PerfOverlayState, PerformanceOverlay, PerformanceOverlayMode, set_override, snapshot,
};
use regia_framework::settings::GameSettings;
use regia_render::{RenderSettings, RuntimeRenderer};
use regia_ui::render::SharedUiRenderer;
use regia_ui::{OffscreenTarget, UiPlugin};

const W: u32 = 1280;
const H: u32 = 720;
/// The backdrop the overlay is drawn over (linear), read back as sRGB.
const BACKDROP: [f64; 4] = [0.30, 0.35, 0.40, 1.0];

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_plugins(regia_app::DiagnosticsPlugin::default())
        .add_plugins(PerfOverlayPlugin);
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(GameSettings::default())
        .insert_resource(RenderSettings::default())
        .insert_resource(RuntimeRenderer::default());
    app
}

fn draw(device: &regia_render::RenderDevice, app: &App, target: &OffscreenTarget) {
    target.clear(device, BACKDROP);
    let shared = app.world().resource::<SharedUiRenderer>().clone();
    let mut guard = shared.lock();
    target.draw_ui(device, &mut guard);
}

fn frames(device: &regia_render::RenderDevice, app: &mut App, target: &OffscreenTarget, n: usize) {
    for _ in 0..n {
        app.update();
        draw(device, app, target);
    }
}

fn save(name: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("evidence dir");
    let path = std::path::Path::new(&dir).join(format!("perf-overlay-{name}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create png"));
    let mut encoder = png::Encoder::new(file, W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("png header")
        .write_image_data(pixels)
        .expect("png data");
}

/// Pixels in `rect` (x0, y0, x1, y1) that differ from the backdrop.
fn changed(pixels: &[u8], backdrop: [u8; 4], rect: [u32; 4]) -> usize {
    let mut n = 0;
    for y in rect[1]..rect[3] {
        for x in rect[0]..rect[2] {
            let i = ((y * W + x) * 4) as usize;
            let d = (0..3)
                .map(|c| pixels[i + c].abs_diff(backdrop[c]))
                .max()
                .unwrap_or(0);
            if d > 2 {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn toggling_creates_no_pipelines_and_draws_in_the_corner() {
    let device = regia_render::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let target = OffscreenTarget::new(&device, W, H);
    let mut app = app();
    // Boot with the overlay hidden: the first draws warm the UI pipeline for the target format.
    frames(&device, &mut app, &target, 5);
    target.clear(&device, BACKDROP);
    let backdrop = target.read(&device).unwrap();
    let backdrop_px = [backdrop[0], backdrop[1], backdrop[2], backdrop[3]];
    assert_eq!(changed(&backdrop, backdrop_px, [0, 0, W, H]), 0);

    let before = regia_render::frame_path_counters();
    let mut shots = Vec::new();
    for mode in [
        PerformanceOverlayMode::Compact,
        PerformanceOverlayMode::Detailed,
        PerformanceOverlayMode::Off,
        PerformanceOverlayMode::Compact,
    ] {
        set_override(app.world_mut(), Some(mode));
        frames(&device, &mut app, &target, 30);
        assert_eq!(app.world().resource::<PerfOverlayState>().shown(), mode);
        let pixels = target.read(&device).unwrap();
        shots.push((mode, pixels));
    }
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!(delta.pipelines, 0, "showing the overlay compiled a pipeline");
    assert_eq!(delta.bind_groups, 0, "showing the overlay created a bind group");

    for (i, (mode, pixels)) in shots.iter().enumerate() {
        save(&format!("{}-{i}", mode.name().to_lowercase()), pixels);
        // Top-left corner (the panel, ≥ 360 × 150 dp at scale 1) vs the rest of the screen.
        let corner = changed(pixels, backdrop_px, [0, 0, 380, 300]);
        let right = changed(pixels, backdrop_px, [400, 0, W, H]);
        let below = changed(pixels, backdrop_px, [0, 600, 400, H]);
        if *mode == PerformanceOverlayMode::Off {
            assert_eq!(corner + right + below, 0, "hidden overlay drew something");
        } else {
            assert!(corner > 380 * 120, "{mode:?}: corner barely drawn ({corner} px)");
            assert_eq!(right, 0, "{mode:?}: drew outside the corner");
            if *mode == PerformanceOverlayMode::Compact {
                assert_eq!(below, 0, "{mode:?}: drew below the panel");
            }
        }
    }
    // Detailed is taller (the per-pass table).
    let rows = |pixels: &[u8]| {
        (0..H)
            .filter(|y| changed(pixels, backdrop_px, [20, *y, 30, y + 1]) > 0)
            .count()
    };
    assert!(
        rows(&shots[1].1) > rows(&shots[0].1) + 100,
        "Detailed adds the pass table"
    );
}

#[test]
fn values_match_the_engine_sources() {
    let mut app = app();
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Detailed));
    for _ in 0..40 {
        app.update();
    }
    let world = app.world();
    let stats = snapshot(world);
    let diagnostics = world.resource::<regia_app::Diagnostics>();
    assert_eq!(stats.fps, diagnostics.fps);
    assert_eq!(
        stats.frame_avg_ms,
        diagnostics.frame_time_avg.as_secs_f64() * 1000.0
    );
    let real = world.resource::<RealTime>();
    assert_eq!(stats.frame_ms, real.delta().as_secs_f32() * 1000.0);
    let renderer = &world.resource::<RuntimeRenderer>().0;
    let gpu = renderer.gpu_timings();
    assert_eq!(stats.cpu_ms, renderer.cpu_timings().total_ms);
    assert_eq!(stats.gpu_ms, gpu.frame_ms);
    let passes: Vec<(String, f32)> = gpu.passes().iter().map(|(n, v)| ((*n).to_owned(), *v)).collect();
    assert_eq!(stats.gpu_passes, passes);
    let tex = renderer.texture_memory_summary();
    assert_eq!(stats.texture_bytes, tex.resident_bytes);
    assert_eq!(stats.texture_budget_bytes, tex.checked_budget);
    let settings = world.resource::<RenderSettings>();
    let quality = settings.quality();
    assert_eq!(stats.preset, settings.preset().map_or("Custom", |p| p.name()));
    assert_eq!(stats.gi_look, format!("{:?}", settings.effective_gi_look()));
    assert_eq!(stats.render_scale, quality.render_scale());
    assert_eq!(
        stats.internal,
        quality.scaled_extent(stats.output.0, stats.output.1)
    );
    // The window holds the frames since the overlay appeared (16 ms each, manual time).
    assert!(stats.window_frames >= 30, "{}", stats.window_frames);
    assert!((stats.p50_ms - 16.0).abs() < 0.5, "{}", stats.p50_ms);
    assert_eq!(stats.stutters_window, 0);
}

#[test]
fn host_default_shows_it_and_the_player_choice_wins() {
    let mut app = app();
    app.insert_resource(PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact));
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<PerfOverlayState>().shown(),
        PerformanceOverlayMode::Compact
    );
    app.world_mut().resource_mut::<GameSettings>().performance_overlay = Some(PerformanceOverlayMode::Off);
    app.update();
    assert_eq!(
        app.world().resource::<PerfOverlayState>().shown(),
        PerformanceOverlayMode::Off
    );
}

#[test]
fn settings_entry_dropdown_writes_the_player_choice() {
    let mut app = app();
    app.update();
    let row = regia_framework::perf_overlay::spawn_settings_entry(app.world_mut(), None);
    app.update();
    let dropdown = {
        let world = app.world_mut();
        let mut q =
            world.query_filtered::<Entity, With<regia_framework::perf_overlay::PerformanceOverlayOption>>();
        q.iter(world).next().expect("dropdown spawned")
    };
    assert!(app.world().get_entity(row).is_ok());
    app.world_mut()
        .get_mut::<regia_ui::Dropdown>(dropdown)
        .unwrap()
        .selected = Some(PerformanceOverlayMode::Detailed.index());
    app.update();
    assert_eq!(
        app.world().resource::<GameSettings>().performance_overlay,
        Some(PerformanceOverlayMode::Detailed)
    );
    app.update();
    assert_eq!(
        app.world().resource::<PerfOverlayState>().shown(),
        PerformanceOverlayMode::Detailed
    );
}

/// Mean `app.update()` time over `n` frames.
fn mean_update(app: &mut App, n: usize) -> f64 {
    let t = Instant::now();
    for _ in 0..n {
        app.update();
    }
    t.elapsed().as_secs_f64() * 1000.0 / n as f64
}

#[test]
fn cost_stays_within_budget() {
    let mut app = app();
    for _ in 0..60 {
        app.update();
    }
    // Interleave hidden and visible blocks so drift (thermal, other jobs) hits both.
    let (mut off, mut on) = (Vec::new(), Vec::new());
    for _ in 0..6 {
        set_override(app.world_mut(), Some(PerformanceOverlayMode::Off));
        mean_update(&mut app, 20);
        off.push(mean_update(&mut app, 200));
        set_override(app.world_mut(), Some(PerformanceOverlayMode::Detailed));
        mean_update(&mut app, 20);
        on.push(mean_update(&mut app, 200));
    }
    off.sort_by(f64::total_cmp);
    on.sort_by(f64::total_cmp);
    let cpu_ms = on[on.len() / 2] - off[off.len() / 2];
    let own_us = app.world().resource::<PerfOverlayState>().stats().overlay_cpu_us;

    // GPU: K overlay draws in one submit, against K draws with the overlay off (an upper bound: it
    // includes the CPU encode and the pass begin/end, since the UI renderer opens no pass when it
    // has nothing to draw). K empty passes over the same target split that into the pass's own
    // load and store and the overlay's draws, printed with the bound (ADR-3373: on the M5 the pass
    // is 0.04 ms and the draws 0.12 ms).
    let mut gpu_ms = None;
    if let Ok(device) = regia_render::RenderDevice::headless() {
        let target = OffscreenTarget::new(&device, 1920, 1080);
        let shared = app.world().resource::<SharedUiRenderer>().clone();
        let submit = |encoder: wgpu::CommandEncoder| {
            let t = Instant::now();
            device.queue.submit([encoder.finish()]);
            device
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            t.elapsed().as_secs_f64() * 1000.0
        };
        let time_draws = |k: usize| {
            let mut guard = shared.lock();
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            for _ in 0..k {
                guard.draw(
                    &device,
                    &mut encoder,
                    target.view(),
                    OffscreenTarget::FORMAT,
                    1920,
                    1080,
                );
            }
            drop(guard);
            submit(encoder)
        };
        let time_empty_passes = |k: usize| {
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            for _ in 0..k {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("perf overlay empty pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view(),
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            submit(encoder)
        };
        let k = 50;
        time_draws(1);
        let with = (0..5).map(|_| time_draws(k)).fold(f64::MAX, f64::min);
        time_empty_passes(1);
        let empty = (0..5).map(|_| time_empty_passes(k)).fold(f64::MAX, f64::min);
        set_override(app.world_mut(), Some(PerformanceOverlayMode::Off));
        for _ in 0..3 {
            app.update();
        }
        time_draws(1);
        let none = (0..5).map(|_| time_draws(k)).fold(f64::MAX, f64::min);
        let per_draw = ((with - none) / k as f64).max(0.0);
        let per_pass = ((empty - none) / k as f64).max(0.0);
        let cpu_adapter = device.adapter.get_info().device_type == wgpu::DeviceType::Cpu;
        gpu_ms = Some((per_draw, per_pass, cpu_adapter));
    }
    let line = format!(
        "perf overlay cost ({} build): CPU {:.4} ms/frame over hidden (hidden {:.4}, Detailed {:.4}); \
         overlay systems {:.1} us; GPU draw upper bound {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        cpu_ms,
        off[off.len() / 2],
        on[on.len() / 2],
        own_us,
        gpu_ms.map_or("n/a".to_owned(), |(ms, pass, cpu)| format!(
            "{ms:.4} ms, of which the pass's load and store {pass:.4} ms ({})",
            if cpu { "CPU rasterizer" } else { "GPU" }
        )),
    );
    eprintln!("{line}");
    if let Ok(dir) = regia_core::brand::env_var("REGIA_EVIDENCE_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(std::path::Path::new(&dir).join("perf-overlay-cost.txt"), &line);
    }
    // Debug builds run text shaping and layout unoptimised: the bound is the release number.
    let cpu_bound = if cfg!(debug_assertions) { 2.0 } else { 0.1 };
    assert!(cpu_ms <= cpu_bound, "{line}");
    if let Some((ms, _, false)) = gpu_ms {
        assert!(ms <= 0.1, "{line}");
    }
}

/// The window path's boot tier runs `OverlayDraw::warm` for every overlay layer on a warm worker
/// (job "overlay layers"): afterwards the UI pipeline exists and the main thread's first overlay
/// frame compiles nothing.
#[test]
fn boot_warm_job_builds_the_ui_pipeline_off_the_main_thread() {
    let device = regia_render::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let mut app = app();
    app.init_resource::<regia_render::WindowOverlay>();
    app.update();
    let layers = app
        .world()
        .resource::<regia_render::WindowOverlay>()
        .shared_layers();
    assert!(!layers.is_empty(), "UiPlugin installs its overlay layer");
    let shared = app.world().resource::<SharedUiRenderer>().clone();
    assert!(!shared.lock().is_warm(OffscreenTarget::FORMAT));
    let worker_device = device.clone();
    std::thread::spawn(move || {
        for layer in &layers {
            layer
                .lock()
                .unwrap()
                .warm(&worker_device, OffscreenTarget::FORMAT);
        }
    })
    .join()
    .unwrap();
    assert!(shared.lock().is_warm(OffscreenTarget::FORMAT));
    let target = OffscreenTarget::new(&device, W, H);
    let before = regia_render::frame_path_counters();
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Compact));
    frames(&device, &mut app, &target, 10);
    let delta = regia_render::frame_path_counters().since(before);
    assert_eq!(
        delta.pipelines, 0,
        "the first overlay frames compiled a pipeline after the boot warm"
    );
    assert_eq!(delta.bind_groups, 0);
}

/// The first-launch quality probe restores GPU timing to "off" when it ends, which can be while
/// the overlay is visible (DO27 ships Compact): the overlay takes the timestamps back on its next
/// update instead of showing a blank GPU column until it is toggled.
#[test]
fn visible_overlay_takes_gpu_timing_back_when_someone_turns_it_off() {
    let device = regia_render::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    if !device.capabilities.timestamp_query {
        eprintln!("skipped: this adapter has no TIMESTAMP_QUERY");
        return;
    }
    let mut app = app();
    app.insert_resource(device.clone());
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Compact));
    for _ in 0..3 {
        app.update();
    }
    assert!(app.world().resource::<RuntimeRenderer>().0.gpu_timing_enabled());
    // The probe's restore.
    app.world_mut()
        .resource_mut::<RuntimeRenderer>()
        .0
        .set_gpu_timing(&device, false);
    app.update();
    assert!(
        app.world().resource::<RuntimeRenderer>().0.gpu_timing_enabled(),
        "the visible overlay re-enables GPU timing"
    );
    // Hiding gives them up again: the overlay owns them now.
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Off));
    app.update();
    assert!(!app.world().resource::<RuntimeRenderer>().0.gpu_timing_enabled());
}
