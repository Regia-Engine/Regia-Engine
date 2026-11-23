// SPDX-License-Identifier: MIT-0
//! No pipeline after warm-up on a shipped scene (DO27 tester item pipeline-cache-cold, brief item
//! 4a; ADR-7885, review rev-d row 6, ADR-8122).
//!
//! The headless arena (bots running their match, the local pawn's first-person camera) is warmed
//! the way the engine's game-frame warm does it (`FrameRenderer::warm_extracted_at` on the
//! extracted frame at the window's size), then runs 300 fixed 60 Hz steps, each drawn through the
//! window path's frame (`render_extracted_into`). No frame after the warm may create a pipeline;
//! a failure prints every late pipeline with its label, target format, sample count, creation site
//! and the forward-set key axes that were new. The second test adds an offscreen renderer of
//! another format with 4x MSAA, created after warm-up and registered with the warm
//! (`register_warm_target`): its frames create none either.
//!
//! Counters and fixed step counts only (no wall clock); lavapipe in CI. The frame-path counters
//! are per thread, and the warm-target registry is per process: the tests take turns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::{App, MinimalPlugins};
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use regia_render::{
    ExtractedFrame, FramePathEvent, FramePathKind, FrameRenderer, FrameTarget, RenderDevice,
    frame_path_counters, take_frame_path_events, trace_frame_path,
};

use arena::{ArenaConfig, ArenaSample, MatchRng};

const W: u32 = 320;
const H: u32 = 180;
/// The window the warm draws for.
const WINDOW: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
/// Fixed 60 Hz steps measured after the warm.
const STEPS: u32 = 300;
/// Fixed steps that spawn the match before the warm (the map, the bots, the local pawn).
const SPAWN_STEPS: u32 = 30;

/// One GPU test at a time (per-thread counters, process-wide warm targets).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The headless arena: four steering bots and the local pawn, a fixed 60 Hz step.
fn arena_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ArenaSample,
    ));
    // The HUD and input systems read these platform messages even headless (no window plugin).
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: true,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 3600.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(0x4A_0122));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

/// A single-sample render target of `format` at the test size.
fn target(device: &RenderDevice, format: wgpu::TextureFormat) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("late pipelines target"),
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// The extracted frame of the last update, with its window view.
fn extracted(app: &App) -> ExtractedFrame {
    let frame = app.world().resource::<ExtractedFrame>().clone();
    assert!(
        frame.is_renderable() && !frame.window_views().is_empty(),
        "the arena extracted a renderable frame with a window view"
    );
    frame
}

/// Pipelines and bind groups created on this thread by `f`, with their traced events.
fn late(f: impl FnOnce()) -> (u64, u64, Vec<FramePathEvent>) {
    let before = frame_path_counters();
    trace_frame_path(true);
    let _ = take_frame_path_events();
    f();
    let events = take_frame_path_events();
    trace_frame_path(false);
    let after = frame_path_counters();
    (
        after.pipelines - before.pipelines,
        after.bind_groups - before.bind_groups,
        events,
    )
}

/// The late creations of `kind`: label, target, samples, site and new key axes.
fn describe(events: &[FramePathEvent], kind: FramePathKind) -> String {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| {
            let d = &e.detail;
            let axes: Vec<&str> = d.new_axes.names().collect();
            format!(
                "`{}` format {:?} samples {} site {} new axes [{}]",
                e.label,
                d.format,
                d.samples,
                d.site
                    .map_or_else(|| "?".to_owned(), |s| format!("{}:{}", s.file(), s.line())),
                axes.join(","),
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// The spawned match, warmed like the engine's game-frame warm: `(app, renderer, device)`.
fn warmed_arena() -> (App, FrameRenderer, RenderDevice) {
    let device = device();
    let mut app = arena_app();
    for _ in 0..SPAWN_STEPS {
        app.update();
    }
    let frame = extracted(&app);
    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted_at(&device, &frame, WINDOW, false, Some((W, H)))
        .unwrap_or_else(|err| panic!("the game-frame warm failed: {err}"));
    (app, renderer, device)
}

/// Item 4a: 300 fixed steps of the running match after the warm create no pipeline.
#[test]
fn the_arena_creates_no_pipeline_in_300_steps_after_its_warm() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    regia_render::pipeline_warm::clear_warm_targets();
    let (mut app, mut renderer, device) = warmed_arena();
    let (_texture, view) = target(&device, WINDOW);
    let (pipelines, bind_groups, events) = late(|| {
        for _ in 0..STEPS {
            app.update();
            let frame = extracted(&app);
            regia_render::render_extracted_into(
                &mut renderer,
                &device,
                &frame,
                FrameTarget::full(&view, WINDOW, W, H),
            )
            .unwrap_or_else(|err| panic!("arena frame: {err}"));
        }
    });
    eprintln!("arena, {STEPS} steps after the warm: {pipelines} pipelines, {bind_groups} bind groups");
    assert_eq!(
        pipelines,
        0,
        "pipelines created after warm-up in the arena:\n  {}",
        describe(&events, FramePathKind::Pipeline)
    );
}

/// Item 4a, second renderer: an offscreen renderer of another format with 4x MSAA
/// (Rgba8UnormSrgb), created after warm-up and registered with the warm before it is warmed,
/// draws 60 steps of the match without creating a pipeline.
#[test]
fn a_second_offscreen_renderer_registered_after_warmup_creates_no_pipeline() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    regia_render::pipeline_warm::clear_warm_targets();
    let (mut app, mut renderer, device) = warmed_arena();
    let (_window_texture, window_view) = target(&device, WINDOW);
    for _ in 0..10 {
        app.update();
        let frame = extracted(&app);
        regia_render::render_extracted_into(
            &mut renderer,
            &device,
            &frame,
            FrameTarget::full(&window_view, WINDOW, W, H),
        )
        .unwrap_or_else(|err| panic!("arena frame: {err}"));
    }
    // After warm-up: the host creates a render-to-texture renderer of another format and MSAA,
    // registers its target and warms it (off the frame path, as `PipelineWarmQueue::warm_target`
    // documents for targets created after the game-frame warm).
    let offscreen = wgpu::TextureFormat::Rgba8UnormSrgb;
    assert!(
        regia_render::pipeline_warm::register_warm_target(offscreen, 4),
        "a new target registers"
    );
    let mut second = FrameRenderer::new();
    second
        .warm_extracted_at(&device, &extracted(&app), offscreen, false, Some((W, H)))
        .unwrap_or_else(|err| panic!("the second renderer's warm failed: {err}"));
    let (_texture, view) = target(&device, offscreen);
    let (pipelines, bind_groups, events) = late(|| {
        for _ in 0..60 {
            app.update();
            let frame = extracted(&app);
            let mut scene = frame.frame_scene(0, W, H);
            scene.quality.msaa_samples = 4;
            second
                .render_into(&device, &scene, FrameTarget::full(&view, offscreen, W, H))
                .unwrap_or_else(|err| panic!("offscreen frame: {err}"));
        }
    });
    regia_render::pipeline_warm::clear_warm_targets();
    eprintln!(
        "second renderer {offscreen:?} x4, 60 steps after its warm: {pipelines} pipelines, {bind_groups} bind groups"
    );
    assert_eq!(
        pipelines,
        0,
        "pipelines created after warm-up by the registered offscreen renderer:\n  {}",
        describe(&events, FramePathKind::Pipeline)
    );
}
