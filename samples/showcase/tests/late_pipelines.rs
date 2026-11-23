// SPDX-License-Identifier: MIT-0
//! No pipeline after warm-up on the showcase scene (DO27 tester item pipeline-cache-cold, brief
//! item 4a; ADR-7885, review rev-d row 6, ADR-8122).
//!
//! The headless showcase on the example scene (terrain, water, scattered foliage, VFX, decals and
//! the camera track) installs its terrain, runs its track, and its runtime renderer is warmed the
//! way the engine's game-frame warm does it (`FrameRenderer::warm_extracted_at` on the extracted
//! frame at the window's size). Then 300 fixed 60 Hz steps of the track are drawn through the
//! window path's frame (`render_extracted_into`). No frame after the warm may create a pipeline; a
//! failure prints every late pipeline with its label, target format, sample count, creation site
//! and the forward-set key axes that were new.
//!
//! Counters and fixed step counts only (no wall clock); lavapipe in CI. Its own test binary: the
//! frame-path counters are per thread and the warm-target registry is per process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use regia::prelude::*;
use regia_render::{
    ExtractedFrame, FramePathEvent, FramePathKind, FrameTarget, RenderDevice, RuntimeRenderer,
    frame_path_counters, take_frame_path_events, trace_frame_path,
};
use showcase::app::{Options, ShowcaseStats};
use showcase::scene::load_scene;

const W: u32 = 320;
const H: u32 = 180;
/// The window the warm draws for.
const WINDOW: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
/// Fixed 60 Hz steps measured after the warm.
const STEPS: u32 = 300;
/// Most updates the terrain may take to install (the inline build installs it on the first).
const INSTALL_UPDATES: u32 = 30;
/// Track steps before the warm (the scene's spawns and the first effects are out).
const SPAWN_STEPS: u32 = 30;

/// The headless showcase on the example scene, or `None` without any adapter.
fn app() -> Option<App> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
    let scene = Arc::new(load_scene(&dir).expect("example scene"));
    let options = Options {
        size: (W, H),
        ..Options::default()
    };
    let mut app = match showcase::app::build_headless_app(scene, options, None) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("note: no GPU adapter ({err}); the late-pipeline test is skipped");
            return None;
        }
    };
    app.finish_plugins();
    // The track plays on the fixed 60 Hz step (the headless app starts paused for render-out jobs).
    app.world_mut().resource_mut::<Time>().paused = false;
    Some(app)
}

/// A single-sample render target of the window's format at the test size.
fn target(device: &RenderDevice) -> (wgpu::Texture, wgpu::TextureView) {
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
        format: WINDOW,
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
        "the showcase extracted a renderable frame with a window view"
    );
    frame
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

/// Item 4a: 300 fixed steps of the showcase track after the warm create no pipeline.
#[test]
fn the_showcase_creates_no_pipeline_in_300_steps_after_its_warm() {
    let Some(mut app) = app() else {
        return;
    };
    regia_render::pipeline_warm::clear_warm_targets();
    let mut installed = false;
    for _ in 0..INSTALL_UPDATES {
        app.update();
        let stats = app.world().resource::<ShowcaseStats>();
        assert!(stats.terrain_error.is_none(), "{:?}", stats.terrain_error);
        if stats.terrain_installed {
            installed = true;
            break;
        }
    }
    assert!(
        installed,
        "the terrain installed within {INSTALL_UPDATES} updates"
    );
    for _ in 0..SPAWN_STEPS {
        app.update();
    }
    let device = app.world().resource::<RenderDevice>().clone();
    let frame = extracted(&app);
    app.world_mut()
        .resource_mut::<RuntimeRenderer>()
        .0
        .warm_extracted_at(&device, &frame, WINDOW, false, Some((W, H)))
        .unwrap_or_else(|err| panic!("the game-frame warm failed: {err}"));
    let (_texture, view) = target(&device);
    let before = frame_path_counters();
    trace_frame_path(true);
    let _ = take_frame_path_events();
    for _ in 0..STEPS {
        app.update();
        let frame = extracted(&app);
        let mut runtime = app.world_mut().resource_mut::<RuntimeRenderer>();
        regia_render::render_extracted_into(
            &mut runtime.0,
            &device,
            &frame,
            FrameTarget::full(&view, WINDOW, W, H),
        )
        .unwrap_or_else(|err| panic!("showcase frame: {err}"));
    }
    let events = take_frame_path_events();
    trace_frame_path(false);
    let after = frame_path_counters();
    let (pipelines, bind_groups) = (
        after.pipelines - before.pipelines,
        after.bind_groups - before.bind_groups,
    );
    eprintln!("showcase, {STEPS} steps after the warm: {pipelines} pipelines, {bind_groups} bind groups");
    assert_eq!(
        pipelines,
        0,
        "pipelines created after warm-up on the showcase track:\n  {}",
        describe(&events, FramePathKind::Pipeline)
    );
}
