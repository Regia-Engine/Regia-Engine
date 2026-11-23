// SPDX-License-Identifier: MPL-2.0
//! Terrain pipelines are the device's, and the warm builds them (ADR-195 "Warm coverage",
//! DO27 INTEGRATE_005 § 3): after `terrain_warm_job` ran on a warm worker, a terrain the host
//! makes on its own thread, its hole cut and a second terrain compile nothing; a renderer
//! warmed on an uncut terrain draws the first hole cut mid-play without a compile. Its own
//! test binary: the pipeline counter is per process, so the tests take turns.

use std::sync::Mutex;
use std::time::Duration;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PipelineWarmQueue, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw, WarmTier,
};
use regia_terrain::{
    CdlodConfig, HoleEllipse, HolesMask, TerrainLayers, TerrainMap, TerrainSharedDraw, terrain_warm_job,
};

/// One test at a time (the pipeline counter is process-wide).
static SERIAL: Mutex<()> = Mutex::new(());

/// A fresh device per test: its shared pipelines start empty.
fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    }
}

fn map(n: u32, world: f32) -> TerrainMap {
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            heights.push(3.0 * (col as f32 * 0.21).sin() * (row as f32 * 0.17).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 20.0, -10.0).expect("map")
}

fn hole(x: f32) -> HolesMask {
    HolesMask::new(vec![HoleEllipse {
        center: [x, 0.0],
        radius: [6.0, 4.0],
        rotation_radians: 0.3,
    }])
    .expect("mask")
}

fn terrain(device: &RenderDevice, n: u32) -> TerrainSharedDraw {
    TerrainSharedDraw::new(
        device,
        map(n, 256.0),
        &TerrainLayers::default(),
        CdlodConfig::default(),
        2,
    )
    .expect("terrain")
}

#[test]
fn after_the_warm_job_a_host_terrain_and_its_holes_compile_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    queue.push(
        WarmTier::Map,
        "terrain pipelines",
        terrain_warm_job(vec![wgpu::TextureFormat::Rgba16Float]),
    );
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(300)),
        "warm job"
    );
    let warm = queue.progress();
    let built = warm.tiers[WarmTier::Map as usize].pipelines;
    assert!(built >= 10, "the job built the terrain sets ({built})");
    assert!(warm.last_error.is_none(), "{:?}", warm.last_error);

    // The host's terrain, made on this thread (outside the queue), like a map loaded behind
    // the menu: construction, prewarm, a hole cut.
    let outside = warm.pipelines_outside;
    let before = pipelines_created();
    let mut draw = terrain(&device, 129);
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    draw.set_holes(Some(&hole(10.0)));
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    let created = pipelines_created() - before;
    let outside_after = queue.progress().pipelines_outside - outside;
    println!(
        "terrain warm job: {built} pipelines; host terrain + holes after it: {created} creates, {outside_after} outside"
    );
    assert_eq!(created, 0, "outside sites: {:?}", queue.progress().outside_sites);
    assert_eq!(outside_after, 0);
    queue.shutdown();
}

#[test]
fn a_second_terrain_on_the_device_compiles_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let cold = pipelines_created();
    let mut first = terrain(&device, 65);
    first.set_holes(Some(&hole(-20.0)));
    first.prewarm(wgpu::TextureFormat::Rgba16Float);
    let first_created = pipelines_created() - cold;
    assert!(first_created > 0, "the first terrain compiles its sets");

    let before = pipelines_created();
    let mut second = terrain(&device, 257);
    second.prewarm(wgpu::TextureFormat::Rgba16Float);
    second.set_holes(Some(&hole(30.0)));
    let created = pipelines_created() - before;
    println!("first terrain: {first_created} creates; second: {created}");
    assert_eq!(created, 0);
}

/// The local-light switch after the warm (lane v020-terrain-local-lights, review finding 2):
/// both variants of the shared sets are the warm's, so a host terrain made with the lights off
/// compiles nothing after `terrain_warm_job`, and turning them on and off after its prewarm
/// swaps pipelines without a compile.
#[test]
fn the_local_light_switch_after_the_warm_compiles_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let queue = PipelineWarmQueue::new(1);
    queue.start(&device);
    queue.push(
        WarmTier::Map,
        "terrain pipelines",
        terrain_warm_job(vec![wgpu::TextureFormat::Rgba16Float]),
    );
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(300)),
        "warm job"
    );
    assert!(queue.progress().last_error.is_none());
    let before = pipelines_created();
    let mut off = terrain(&device, 129);
    off.set_local_lights_enabled(false);
    off.prewarm(wgpu::TextureFormat::Rgba16Float);
    let off_created = pipelines_created() - before;
    let supported = off.gpu().local_lights_supported();
    assert_eq!(off.gpu().shared_pipelines_lit(), Some(false));
    off.set_local_lights_enabled(true);
    assert_eq!(off.gpu().shared_pipelines_lit(), Some(supported));
    off.set_holes(Some(&hole(10.0)));
    off.set_local_lights_enabled(false);
    assert_eq!(off.gpu().shared_pipelines_lit(), Some(false));
    let created = pipelines_created() - before;
    println!(
        "lights off after the warm job: prewarm {off_created} creates; switching on, cutting a hole, off: {created} in all (lights supported: {supported})"
    );
    assert_eq!(created, 0, "outside sites: {:?}", queue.progress().outside_sites);
    queue.shutdown();
}

/// A shared-draw frame of the terrain at 160×90 (Desktop High, no TAA): `warm` runs the engine's
/// frame warm on it (`warm_scene`, which runs the shared draw's `warm_pipelines`) instead.
fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, warm: bool) {
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 40.0, -90.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.4], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial::default()],
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    if warm {
        renderer
            .warm_scene(device, &scene, wgpu::TextureFormat::Rgba8UnormSrgb, true)
            .expect("warm");
    } else {
        renderer.render_offscreen(device, &scene).expect("frame");
    }
}

#[test]
fn a_hole_cut_after_the_frame_warm_compiles_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let draw = terrain(&device, 129);
    let holes = draw.holes_handle();
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    // The engine's frame warm on the uncut terrain, then the first real frames.
    frame(&mut renderer, &device, true);
    frame(&mut renderer, &device, false);
    frame(&mut renderer, &device, false);
    let before = pipelines_created();
    // A crater mid-play (the agent protocol's `terrain.set_holes` path: cut on the next prepare).
    holes.request(hole(0.0));
    for _ in 0..3 {
        frame(&mut renderer, &device, false);
    }
    let created = pipelines_created() - before;
    println!("frames with the first hole cut after the warm: {created} creates");
    assert_eq!(created, 0);
}

/// Quits while `terrain_warm_job` runs on a queue that does not unwind (the shipping build's
/// path, `panic = "abort"`), once the job has created `after` pipelines. Returns the pipelines
/// created after the quit and when the job returned (counted from the quit).
fn quit_terrain_warm_after(after: u64) -> (u64, Duration) {
    let device = device();
    let queue = PipelineWarmQueue::new(1);
    queue.set_cancel_unwinds(false);
    // Six formats: 5 + 6 × 20 + 1 pipelines (each set with and without the local-light loop),
    // far more than any quit point below.
    queue.push(
        WarmTier::Map,
        "terrain pipelines",
        terrain_warm_job(vec![
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgb10a2Unorm,
        ]),
    );
    queue.start(&device);
    let t_wait = std::time::Instant::now();
    while queue.cancel_counts().0 < after && t_wait.elapsed() < Duration::from_secs(300) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let t0 = std::time::Instant::now();
    queue.shutdown();
    while queue.progress().last_error.is_none() && t0.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(2));
    }
    let ended = t0.elapsed();
    let (created, late) = queue.cancel_counts();
    let error = queue.progress().last_error;
    println!(
        "quit after {after} pipelines: {late} created after the quit, {created} by the job; returned {ended:?} after the quit ({error:?})"
    );
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("canceled at shutdown")),
        "quit after {after} pipelines: the job did not stop ({error:?})"
    );
    (late, ended)
}

/// A quit during the terrain warm stops the job at its next set boundary (review of the
/// v0.1.10 lane: it used to check only between formats, so a quit at the splash ran on
/// through the offscreen set, the hole cut and a whole format's sets, about 16 compiles).
/// At most the set in flight (5 pipelines) may finish after the quit.
#[test]
fn a_quit_stops_the_terrain_warm_between_sets_without_unwinding() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // In the offscreen set, the first plain set, its hole-cut set and a later format.
    for after in [2, 7, 12, 23] {
        let (late, ended) = quit_terrain_warm_after(after);
        assert!(
            late <= 5,
            "quit after {after} pipelines: {late} compiled after the quit"
        );
        assert!(
            ended < Duration::from_secs(30),
            "quit after {after} pipelines: the job returned {ended:?} after the quit"
        );
    }
}
