// SPDX-License-Identifier: MIT-0
//! Warm coverage (lane v0110-warm-coverage, ADR-195; DO27 INTEGRATE_005 § 3): after the game
//! frame warm, a DO27-like session creates no pipeline outside the warm queue.
//!
//! The DO27 smoke logged pipelines "created outside the warm queue" for minutes after START:
//! terrain sets (a terrain built by the host, its hole cut), material graphs met after the
//! warm, the forward sets of double-sided meshes, the translucent shadow and the effects
//! context. This test plays the same session on synthetic content, in the runtime's order:
//!
//! 1. boot: the engine's own warm jobs (what `TerrainPlugin` and `VfxRuntimePlugin` queue by
//!    themselves: `terrain_warm_job`, the VFX context built and warmed on a worker);
//! 2. level load on the host thread: the map's terrain, the effects layer, the level's material
//!    graphs (`set_level_material_graphs`);
//! 3. the game-frame warm on a worker (`warm_renderer` + `warm_extracted_at`, what
//!    `GameFrameWarm` runs) with a terrain, a graph material and a translucent shadow caster;
//! 4. the first game frame, then 120 frames of play: a second graph material, double-sided
//!    meshes, a hole cut mid-play, effects and decals, and the next map's terrain built on the
//!    host thread.
//!
//! Asserted: 0 pipelines created outside the warm queue from the first game frame on (the
//! counter `render.pipeline_warm` reports as `pipelines_outside`). Needs a GPU (lavapipe is
//! enough); its own test binary, as the counters are per process (the tests take turns).
//!
//! v0.1.11 (DO27's v0.1.10 smoke, ADR-210): the real session still logged ~14 pipelines "outside"
//! at the start of play on both maps. They came from what the shell does and this session did
//! not: a reflection-probe capture (`do27.apply_lighting`, six faces in the readback target
//! with no post stack, the terrain's hole-cut set included) and a glass canopy (a transmissive
//! mesh with a glass instance) that spawned after the warm and compiled the glass sets on the
//! frame path. The session now captures a probe at level load (the shell's order: before the game-frame
//! warm), adds a canopy mid-play and captures again after the hole cut; the measurement
//! includes the first game frame itself. A second test plays the order that makes the load-time
//! capture free as well: `set_warm_probe_capture` at load, the capture after the warm.
//!
//! v0.1.14 (DO27's rehearsal B on v0.1.11, ADR-220): still "outside" after the first game frame
//! were the renderer's own device-level pipelines (`frame.rs` cluster pipeline and page clear,
//! the translucent casters) built by a renderer other than the warmed one, TSR and the depth
//! match 5–15 s into play, and the outdoor passes on a second map's first frame. The session
//! now switches to TSR at a lower render scale mid-play, goes fullscreen (a new output size),
//! prewarms a graph on a stand-in renderer (what `prewarm_graph_pipelines` does while a map's
//! game-frame warm has the runtime renderer) and loads a next map with clouds and froxel fog. A
//! third test plays a TSR session over two maps with a warm per map.
//!
//! v0.1.17 (ADR-224): the canopy is viewer-inside glass, so it draws with the cheap own-canopy
//! pipelines by default; the session switches it to the full shader and back mid-play.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lookdev::soak::SoakVfx;
use regia_core::math::{Mat4, Vec3};
use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::runtime::ExtractedMesh;
use regia_render::{
    CanopyGlass, CanopyShading, ExtractedFrame, FrameQuality, FrameRenderer, GlassInstance, GraphTarget,
    LabMaterial, PipelineWarmQueue, RenderDevice, RenderProfile, RenderProfilePreset, RuntimeRenderer,
    SharedPassDraw, SurfaceGraph, TemporalUpscaler, WarmTier,
};
use regia_terrain::{
    CdlodConfig, HoleEllipse, HolesMask, TerrainLayers, TerrainMap, TerrainSharedDraw, terrain_warm_job,
};
use regia_vfx::{VfxGpu, VfxProfile};

const W: u32 = 320;
const H: u32 = 180;
/// The window's size after it goes fullscreen mid-play.
const FULL: (u32, u32) = (384, 216);
/// The offscreen frames' target (`render_offscreen`).
const TARGET: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Desktop High's HDR target.
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The canopy that drives in mid-play: a transmissive mesh with a glass instance
/// (`GlassInstance::object_key`; glass instances apply to transmissive standard materials).
const CANOPY: u32 = 13;

/// One test at a time: the warm counters are per process.
static SERIAL: Mutex<()> = Mutex::new(());

/// A graph material (`regia::pbr::surface`), its albedo `grey`.
fn graph(grey: f32) -> Arc<SurfaceGraph> {
    Arc::new(SurfaceGraph::new(format!(
        r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{
    return regia::pbr::surface::Surface(
        vec3<f32>({grey:.3}, {grey:.3}, {grey:.3}),
        0.6,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}}
"
    )))
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

/// Lets the GPU finish before a test's device goes (ADR-287; ADR-269 left this open). Declared
/// right after the device, so it drops after the renderers, the warm queue and everything else
/// that holds the device, and just before the device itself: the warm workers a shutdown left
/// in a compile are joined, then `poll(Wait)` drains the queue, so the last device clone is
/// never dropped with work in flight or a worker still in the driver. Changes no assertion.
struct SettleBeforeDeviceDrop(RenderDevice);

impl Drop for SettleBeforeDeviceDrop {
    fn drop(&mut self) {
        let left = regia_render::pipeline_warm::join_leftover_workers(Duration::from_secs(30));
        if left > 0 {
            eprintln!("warm coverage: {left} warm worker(s) still compiling when the test ended");
        }
        if let Err(err) = self.0.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(30)),
        }) {
            eprintln!("warm coverage: the GPU did not go idle before the device drop: {err}");
        }
    }
}

/// Rolling ground, `world` metres across, `n`² samples.
fn map(n: u32, world: f32) -> TerrainMap {
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            heights.push(2.0 * (col as f32 * 0.13).sin() * (row as f32 * 0.11).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 20.0, -10.0).expect("map")
}

fn terrain(device: &RenderDevice, n: u32, world: f32) -> TerrainSharedDraw {
    let mut draw = TerrainSharedDraw::new(
        device,
        map(n, world),
        &TerrainLayers::default(),
        CdlodConfig::default(),
        2,
    )
    .expect("terrain");
    draw.prewarm(HDR);
    draw
}

/// A copy of the canonical quad as entity `id` with `material`, moved to `at`, scaled by `scale`.
fn mesh(base: &ExtractedMesh, id: u32, material: LabMaterial, at: [f32; 3], scale: f32) -> ExtractedMesh {
    ExtractedMesh {
        entity: regia_ecs::prelude::Entity::from_raw_u32(id).expect("entity index"),
        material: Arc::new(material),
        model: (Mat4::from_translation(Vec3::from_array(at)) * Mat4::from_scale(Vec3::splat(scale)))
            .to_cols_array_2d(),
        ..base.clone()
    }
}

/// The map's first frame: Desktop High, the ground quad, a graph-material slab and a
/// half-transparent pane casting a translucent shadow.
fn first_frame(graph_a: &Arc<SurfaceGraph>) -> ExtractedFrame {
    let mut frame = canonical_frame();
    frame.quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    frame.depth_prepass = true;
    let view = &mut frame.views[0];
    view.camera.eye = [0.0, 12.0, -30.0];
    view.camera.target = [0.0, 0.0, 0.0];
    view.camera.far = 2000.0;
    let base = frame.meshes[0].clone();
    frame.meshes[0].model = Mat4::from_scale(Vec3::splat(6.0)).to_cols_array_2d();
    frame.meshes.push(mesh(
        &base,
        11,
        LabMaterial {
            surface_graph: Some(Arc::clone(graph_a)),
            ..LabMaterial::default()
        },
        [3.0, 1.0, 0.0],
        1.5,
    ));
    frame.meshes.push(mesh(
        &base,
        12,
        LabMaterial {
            base_color: [0.2, 0.6, 0.9, 0.5],
            alpha_mode: regia_render::ALPHA_BLEND,
            ..LabMaterial::default()
        },
        [-3.0, 2.0, 0.0],
        1.5,
    ));
    frame
}

/// The canopy mesh [`CANOPY`] (not in the warm frame: its vehicle spawns mid-play).
fn canopy_mesh(base: &ExtractedMesh) -> ExtractedMesh {
    mesh(
        base,
        CANOPY,
        LabMaterial {
            base_color: [0.8, 0.9, 1.0, 0.2],
            transmission: 1.0,
            roughness: 0.05,
            ..LabMaterial::default()
        },
        [0.0, 1.5, -3.0],
        1.0,
    )
}

/// The glass instance of the player's own canopy on mesh [`CANOPY`].
fn canopy() -> GlassInstance {
    GlassInstance {
        object_key: regia_ecs::prelude::Entity::from_raw_u32(CANOPY)
            .expect("entity index")
            .to_bits(),
        inside: true,
        specular_scale: 0.15,
        damage: None,
    }
}

/// DO27's `apply_lighting`: a reflection probe captured from the map's view on the calling
/// thread (six faces, `capture_reflection_probe`) and installed.
fn capture_probe(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &ExtractedFrame, id: u64) {
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, W, H);
    let probe = renderer
        .capture_reflection_probe(device, &scene, id, [0.0, 4.0, 0.0], [40.0, 10.0, 40.0])
        .expect("probe capture");
    renderer.set_reflection_probes(vec![probe]);
}

fn draw(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &ExtractedFrame) {
    draw_at(device, renderer, frame, (W, H));
}

fn draw_at(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &ExtractedFrame, (w, h): (u32, u32)) {
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, w, h);
    renderer.render_offscreen(device, &scene).expect("frame");
}

/// The next map's sky: a cloud layer and height fog (the first map has neither).
fn cloudy_foggy(frame: &mut ExtractedFrame) {
    frame.outdoor.clouds.coverage = 0.6;
    frame.outdoor.fog.density = 0.02;
}

/// What `prewarm_graph_pipelines` does with a graph material that resolves while a map's
/// game-frame warm has the runtime renderer on a worker: it queues the graph's sets on the
/// stand-in `GameFrameWarm` left (`RuntimeRenderer::default()`), whose renderer has built
/// nothing yet. Before v0.1.14 that built the stand-in's pipes on this thread.
fn prewarm_on_a_stand_in(device: &RenderDevice, graph: &SurfaceGraph) {
    let mut stand_in = RuntimeRenderer::default().0;
    stand_in
        .prewarm_material_graphs(device, &[graph], &[GraphTarget::HDR])
        .expect("stand-in graph prewarm");
}

/// Runs the game-frame warm of `frame` on a worker (what `GameFrameWarm` runs) and returns the
/// renderer with the warm's pipeline count and time.
fn game_frame_warm(
    queue: &PipelineWarmQueue,
    renderer: FrameRenderer,
    frame: &ExtractedFrame,
) -> (FrameRenderer, u64, f32) {
    let warm_frame = frame.clone();
    let warm = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, d| {
        r.warm_extracted_at(d, &warm_frame, TARGET, true, Some((W, H)))
            .map_err(|e| e.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(600);
    while !warm.is_done() {
        assert!(Instant::now() < deadline, "the game-frame warm came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    let warmed = warm.take().expect("warmed");
    assert!(warmed.error.is_none(), "{:?}", warmed.error);
    (
        warmed.renderer.expect("renderer back"),
        warmed.pipelines,
        warmed.ms,
    )
}

#[test]
#[allow(clippy::too_many_lines)] // one session, in order
fn a_do27_like_session_creates_no_pipeline_outside_the_warm_after_the_first_game_frame() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let device = device();
    let _settle = SettleBeforeDeviceDrop(device.clone());
    let queue = PipelineWarmQueue::new(4);
    queue.start(&device);
    let profile = VfxProfile::desktop_high();

    // 1. Boot: the engine's own warm jobs.
    queue.push(WarmTier::Map, "terrain pipelines", terrain_warm_job(vec![HDR]));
    let vfx_slot: Arc<Mutex<Option<VfxGpu>>> = Arc::default();
    let slot = Arc::clone(&vfx_slot);
    queue.push(WarmTier::Boot, "vfx context", move |device: &RenderDevice| {
        let gpu = VfxGpu::new_warmed(device, profile, &VfxGpu::warm_formats(TARGET))
            .map_err(|e| e.to_string())?
            .ok_or("canceled at shutdown")?;
        *slot.lock().map_err(|_| "slot")? = Some(gpu);
        Ok(())
    });
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(600)),
        "boot jobs"
    );
    let boot = queue.progress();
    assert!(boot.last_error.is_none(), "{:?}", boot.last_error);

    // 2. Level load on this (the host's) thread.
    let graph_a = graph(0.7);
    let graph_b = graph(0.3);
    // The next map's graph (not in this level's list): it resolves while that map's warm runs.
    let graph_next = graph(0.5);
    let terrain_map = map(257, 512.0);
    let first = terrain(&device, 257, 512.0);
    let holes = first.holes_handle();
    let mut renderer = RuntimeRenderer::default().0;
    renderer.set_shared_draw(Some(Box::new(first) as Box<dyn SharedPassDraw + Send + Sync>));
    let gpu = vfx_slot.lock().expect("slot").take().expect("vfx context built");
    let mut vfx = SoakVfx::with_gpu(gpu, profile).expect("vfx");
    renderer.set_translucent_layers(vec![vfx.layer()]);
    renderer.set_level_material_graphs(vec![Arc::clone(&graph_a), Arc::clone(&graph_b)]);
    let mut frame = first_frame(&graph_a);
    // The shell's lighting step captures the map's probe here, before the game-frame warm.
    capture_probe(&device, &mut renderer, &frame, 1);
    let load_outside = queue.progress().pipelines_outside - boot.pipelines_outside;

    // 3. The game-frame warm on a worker.
    let warm_frame = frame.clone();
    let warm = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, d| {
        r.warm_extracted_at(d, &warm_frame, TARGET, true, Some((W, H)))
            .map_err(|e| e.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(600);
    while !warm.is_done() {
        assert!(Instant::now() < deadline, "the game-frame warm came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    let warmed = warm.take().expect("warmed");
    assert!(warmed.error.is_none(), "{:?}", warmed.error);
    let mut renderer = warmed.renderer.expect("renderer back");

    // 4. The first game frame (measured: DO27's first frame compiled the glass sets), then play.
    let outside_at_first = queue.progress().pipelines_outside;
    let created_at_first = pipelines_created();
    // This thread is the frame thread: its own creates (`frame_path_counters` is per thread).
    let frame_thread_at_first = regia_render::frame_path_counters();
    draw(&device, &mut renderer, &frame);
    let base = canonical_frame().meshes[0].clone();
    let mut next_id = 100;
    let mut fallback_at_spawn = None;
    let mut size = (W, H);
    for step in 0..120u32 {
        match step {
            // A unit with the level's second graph material drives in. The warm started its
            // set (`set_level_material_graphs`), so play goes on until the graph workers are
            // done, and from its first draw it uses its own set: no standard-surface fallback.
            // (Graph-worker builds are background builds, not "outside"; this is what shows
            // that the warm, not the draw, started them.)
            10 => {
                let wait = Instant::now() + Duration::from_secs(300);
                while renderer.material_graph_stats().pending > 0 {
                    assert!(Instant::now() < wait, "the level's graph sets were built");
                    draw(&device, &mut renderer, &frame);
                }
                let stats = renderer.material_graph_stats();
                assert_eq!(stats.failed, 0, "{stats:?}");
                fallback_at_spawn = Some(stats.fallback_draws);
                frame.meshes.push(mesh(
                    &base,
                    next_id,
                    LabMaterial {
                        surface_graph: Some(Arc::clone(&graph_b)),
                        ..LabMaterial::default()
                    },
                    [0.0, 1.0, 4.0],
                    1.0,
                ));
                next_id += 1;
            }
            // A wreck: double-sided, opaque and blended.
            20 => {
                for (i, alpha_mode) in [regia_render::ALPHA_OPAQUE, regia_render::ALPHA_BLEND]
                    .into_iter()
                    .enumerate()
                {
                    frame.meshes.push(mesh(
                        &base,
                        next_id,
                        LabMaterial {
                            base_color: [0.5, 0.4, 0.3, if i == 0 { 1.0 } else { 0.6 }],
                            alpha_mode,
                            double_sided: true,
                            ..LabMaterial::default()
                        },
                        [-4.0 + 2.0 * i as f32, 0.8, -2.0],
                        1.0,
                    ));
                    next_id += 1;
                }
            }
            // A crater cuts the terrain.
            30 => holes.request(
                HolesMask::new(vec![HoleEllipse {
                    center: [0.0, 8.0],
                    radius: [5.0, 3.0],
                    rotation_radians: 0.2,
                }])
                .expect("mask"),
            ),
            // A vehicle with a glass canopy spawns, and the player climbs in.
            50 => {
                frame.meshes.push(canopy_mesh(&base));
                renderer.set_glass(vec![canopy()]);
            }
            // A tester restores the full canopy shader (`REGIA_CANOPY_FULL`'s setting, v0.1.17),
            // then the default cheap one again: both own-canopy variants come from the warm.
            55 => renderer.set_canopy_glass(Some(CanopyGlass {
                shading: CanopyShading::Full,
                ..CanopyGlass::default()
            })),
            // The lighting step captures the probe again, now with the crater and the canopy.
            60 => capture_probe(&device, &mut renderer, &frame, 2),
            65 => renderer.set_canopy_glass(None),
            // The shell applies its 4K lever, 0.67 + TSR (host checklist "FSR 3, TSR, SGSR and 4K
            // render scale"), after the first-launch preset re-seeded the profile native + TAAU
            // (DO27's rehearsal B, a fresh HOME: `tsr/mod.rs` and the depth match 5–15 s into
            // play). The window path hands the profile's upscaler over every frame.
            70 => {
                frame.temporal_upscaler = TemporalUpscaler::Tsr;
                frame.quality.render_scale_permille = 670;
                renderer.set_temporal_upscaler(frame.temporal_upscaler);
            }
            // The next map's warm leaves a stand-in, and that map's graphs resolve meanwhile.
            75 => prewarm_on_a_stand_in(&device, &graph_next),
            // The window goes fullscreen: a new output size.
            80 => size = FULL,
            // The next map, with no warm of its own (DO27's shell warms once per session): its
            // terrain, built on the host thread, and its sky with clouds and froxel fog.
            90 => {
                let next = terrain(&device, 129, 256.0);
                renderer.set_shared_draw(Some(Box::new(next) as Box<dyn SharedPassDraw + Send + Sync>));
                cloudy_foggy(&mut frame);
                renderer.set_outdoor(frame.outdoor.clone());
            }
            _ => {}
        }
        // Effects and decals from step 40.
        if step >= 40 && step.is_multiple_of(8) {
            vfx.spawn(&terrain_map, [0.0, -6.0], [0.0, 1.0], step);
        }
        vfx.update(frame.views[0].camera.eye);
        draw_at(&device, &mut renderer, &frame, size);
    }
    let graphs = renderer.material_graph_stats();
    assert_eq!(
        Some(graphs.fallback_draws),
        fallback_at_spawn,
        "the second graph drew the standard surface while its set compiled: {graphs:?}"
    );
    assert_eq!(
        graphs.pending, 0,
        "a graph set started after the warm: {graphs:?}"
    );
    let progress = queue.progress();
    let outside = progress.pipelines_outside - outside_at_first;
    let created = pipelines_created() - created_at_first;
    let frame_thread = regia_render::frame_path_counters()
        .since(frame_thread_at_first)
        .pipelines;
    println!(
        "warm coverage: boot jobs {} pipelines; level load (probe capture included) {load_outside} outside \
         (before the first game frame); \
         game-frame warm {} pipelines in {:.0} ms; from the first game frame on: {outside} outside the warm queue, \
         {frame_thread} on the frame thread, {created} created in all (background builds included); \
         outside sites: {:?}",
        boot.tiers.iter().map(|t| t.pipelines).sum::<u64>(),
        warmed.pipelines,
        warmed.ms,
        progress.outside_sites,
    );
    queue.shutdown();
    assert_eq!(
        outside, 0,
        "pipelines created outside the warm queue from the first game frame on; sites: {:?}",
        progress.outside_sites
    );
    assert_eq!(
        frame_thread, 0,
        "pipelines created on the frame thread from the first game frame on"
    );
}

/// The order that keeps the shell's first capture off the host thread as well: the level asks
/// for probe-capture coverage at load (`set_warm_probe_capture`), the game-frame warm renders a
/// probe face, and the probe is captured after the warm: the capture and the first game frame
/// compile nothing, the hole-cut terrain set in the capture's format included.
#[test]
fn a_probe_capture_after_the_game_frame_warm_compiles_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let device = device();
    let _settle = SettleBeforeDeviceDrop(device.clone());
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);

    // Level load on the host thread: the map's terrain, holes cut (a borehole, a tunnel mouth).
    let first = terrain(&device, 129, 256.0);
    first.holes_handle().request(
        HolesMask::new(vec![HoleEllipse {
            center: [0.0, 8.0],
            radius: [5.0, 3.0],
            rotation_radians: 0.2,
        }])
        .expect("mask"),
    );
    let mut renderer = RuntimeRenderer::default().0;
    renderer.set_shared_draw(Some(Box::new(first) as Box<dyn SharedPassDraw + Send + Sync>));
    renderer.set_warm_probe_capture(true);
    let graph_a = graph(0.7);
    renderer.set_level_material_graphs(vec![Arc::clone(&graph_a)]);
    let frame = first_frame(&graph_a);

    let warm_frame = frame.clone();
    let warm = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, d| {
        r.warm_extracted_at(d, &warm_frame, TARGET, true, Some((W, H)))
            .map_err(|e| e.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(600);
    while !warm.is_done() {
        assert!(Instant::now() < deadline, "the game-frame warm came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    let warmed = warm.take().expect("warmed");
    assert!(warmed.error.is_none(), "{:?}", warmed.error);
    let mut renderer = warmed.renderer.expect("renderer back");

    // The lighting step after the warm, then the first game frames, on this (the host's) thread.
    let outside_before = queue.progress().pipelines_outside;
    let thread_before = regia_render::frame_path_counters();
    capture_probe(&device, &mut renderer, &frame, 1);
    let capture = regia_render::frame_path_counters().since(thread_before).pipelines;
    for _ in 0..3 {
        draw(&device, &mut renderer, &frame);
    }
    let progress = queue.progress();
    let outside = progress.pipelines_outside - outside_before;
    let thread = regia_render::frame_path_counters().since(thread_before).pipelines;
    println!(
        "probe capture after the warm: game-frame warm {} pipelines in {:.0} ms; capture {capture} created \
         on the host thread, capture + 3 frames {thread}; {outside} outside the warm queue; sites: {:?}",
        warmed.pipelines, warmed.ms, progress.outside_sites,
    );
    queue.shutdown();
    assert_eq!(
        outside, 0,
        "the capture or the first frames compiled outside the warm queue; sites: {:?}",
        progress.outside_sites
    );
    assert_eq!(
        thread, 0,
        "the capture or the first frames compiled on the host thread"
    );
}

/// A TSR session over two maps, each with its own game-frame warm (the order DO27's shell
/// moves to, WARM_PER_MAP): the profile draws with TSR at 0.67 from the start, the first map
/// plays, then the next map loads. While its warm has the runtime renderer, the next map's
/// graph materials resolve on the stand-in (`prewarm_graph_pipelines`), and the next map has
/// clouds and froxel fog. Nothing compiles outside the warm queue from the first game frame
/// on, the stand-in and the second map included.
#[test]
fn a_tsr_session_over_two_maps_creates_no_pipeline_outside_the_warm() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let device = device();
    let _settle = SettleBeforeDeviceDrop(device.clone());
    let queue = PipelineWarmQueue::new(4);
    queue.start(&device);

    // Map 1 at level load, on this (the host's) thread: TSR at 0.67.
    let graph_a = graph(0.7);
    let graph_next = graph(0.4);
    let mut renderer = RuntimeRenderer::default().0;
    renderer.set_shared_draw(Some(
        Box::new(terrain(&device, 129, 256.0)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    renderer.set_level_material_graphs(vec![Arc::clone(&graph_a)]);
    let mut frame = first_frame(&graph_a);
    frame.temporal_upscaler = TemporalUpscaler::Tsr;
    frame.quality.render_scale_permille = 670;
    let (mut renderer, first_warm, first_ms) = game_frame_warm(&queue, renderer, &frame);
    renderer.set_temporal_upscaler(frame.temporal_upscaler);

    let outside_at_first = queue.progress().pipelines_outside;
    let thread_at_first = regia_render::frame_path_counters();
    for _ in 0..20 {
        draw(&device, &mut renderer, &frame);
    }

    // Map 2: its warm takes the renderer; its graph resolves on the stand-in meanwhile.
    renderer.set_shared_draw(Some(
        Box::new(terrain(&device, 129, 384.0)) as Box<dyn SharedPassDraw + Send + Sync>
    ));
    renderer.set_level_material_graphs(vec![Arc::clone(&graph_next)]);
    cloudy_foggy(&mut frame);
    let base = canonical_frame().meshes[0].clone();
    frame.meshes[1] = mesh(
        &base,
        11,
        LabMaterial {
            surface_graph: Some(Arc::clone(&graph_next)),
            ..LabMaterial::default()
        },
        [3.0, 1.0, 0.0],
        1.5,
    );
    let warm_frame = frame.clone();
    let warm = queue.warm_renderer(WarmTier::Map, "game frame", renderer, move |r, d| {
        r.warm_extracted_at(d, &warm_frame, TARGET, true, Some((W, H)))
            .map_err(|e| e.to_string())
    });
    prewarm_on_a_stand_in(&device, &graph_next);
    let deadline = Instant::now() + Duration::from_secs(600);
    while !warm.is_done() {
        assert!(Instant::now() < deadline, "the second map's warm came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    let warmed = warm.take().expect("warmed");
    assert!(warmed.error.is_none(), "{:?}", warmed.error);
    let mut renderer = warmed.renderer.expect("renderer back");
    renderer.set_outdoor(frame.outdoor.clone());
    for _ in 0..20 {
        draw(&device, &mut renderer, &frame);
    }

    let progress = queue.progress();
    let outside = progress.pipelines_outside - outside_at_first;
    let thread = regia_render::frame_path_counters()
        .since(thread_at_first)
        .pipelines;
    println!(
        "TSR session over two maps: warms {first_warm} pipelines in {first_ms:.0} ms and {} in {:.0} ms; \
         from the first game frame on: {outside} outside the warm queue, {thread} on this thread; sites: {:?}",
        warmed.pipelines, warmed.ms, progress.outside_sites,
    );
    queue.shutdown();
    assert_eq!(
        outside, 0,
        "pipelines created outside the warm queue in a TSR session over two maps; sites: {:?}",
        progress.outside_sites
    );
    assert_eq!(
        thread, 0,
        "pipelines created on the host thread in a TSR session over two maps"
    );
}
