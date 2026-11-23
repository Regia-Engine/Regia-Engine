// SPDX-License-Identifier: MPL-2.0
//! Host terrain preparation must finish all compiles before publishing the draw.
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::{FrameRenderer, PipelineWarmQueue, RenderDevice, WarmTier};
use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap, TerrainSharedDraw, prepare_terrain};

static SERIAL: Mutex<()> = Mutex::new(());
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

fn draw(device: &RenderDevice) -> Result<TerrainSharedDraw, String> {
    let map =
        TerrainMap::from_heights(17, 17, vec![0.0; 17 * 17], 32.0, 8.0, 0.0).map_err(|e| e.to_string())?;
    TerrainSharedDraw::new(device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
        .map_err(|e| e.to_string())
}

#[test]
fn preparation_publishes_only_after_all_pipelines_and_install_creates_none() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let queue = PipelineWarmQueue::new(1);
    queue.set_engine_jobs(false);
    let before = pipelines_created();
    let outside = queue.progress().pipelines_outside;
    let tier_before = queue.progress().tiers[WarmTier::Map as usize].pipelines;
    let (constructed_tx, constructed_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let host_thread = std::thread::current().id();
    let prepared = prepare_terrain(&queue, FORMAT, move |device| {
        assert_ne!(std::thread::current().id(), host_thread);
        let draw = draw(device)?;
        constructed_tx.send(()).expect("constructed receiver");
        continue_rx.recv().expect("continue signal");
        Ok(draw)
    });
    assert!(!prepared.is_finished());
    assert!(!prepared.is_ready());
    assert!(prepared.take().is_none());
    queue.start(&device);
    constructed_rx.recv().expect("constructed signal");
    // The constructor is finished, but the shared draw's first-frame pipelines do not exist.
    assert!(pipelines_created() > before);
    assert!(!prepared.is_ready());
    assert!(prepared.take().is_none());
    continue_tx.send(()).expect("continue receiver");
    assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(300)));
    assert!(prepared.is_finished());
    assert!(prepared.is_ready());
    let mut terrain = prepared.take().expect("completed").expect("prepared terrain");
    assert!(prepared.take().is_none(), "single ownership handoff");
    let built = pipelines_created() - before;
    let passes =
        4 + u64::from(device.device.limits().max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS);
    let variants = if terrain.gpu().local_lights_supported() {
        4
    } else {
        2
    };
    // Plus the other cloud-map state's colour pipelines (ADR-7886): one per light variant and hole cut.
    assert_eq!(
        built,
        6 + passes * variants + variants,
        "all supported core pipeline sets"
    );
    let progress = queue.progress();
    assert_eq!(
        progress.tiers[WarmTier::Map as usize].pipelines - tier_before,
        built
    );
    assert_eq!(progress.pipelines_outside, outside);
    let install_before = pipelines_created();
    terrain.prewarm(FORMAT);
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(terrain));
    assert_eq!(pipelines_created(), install_before);
    assert_eq!(queue.progress().pipelines_outside, outside);
    queue.shutdown();
}

#[test]
fn failed_preparation_is_finished_but_never_ready() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let queue = PipelineWarmQueue::new(1);
    let prepared = prepare_terrain(&queue, FORMAT, |_| Err("bad terrain input".into()));
    assert_eq!(queue.run_inline(&device, WarmTier::Map), 1);
    assert!(prepared.is_finished());
    assert!(!prepared.is_ready());
    assert_eq!(
        prepared.take().expect("completed preparation").err().as_deref(),
        Some("bad terrain input")
    );
    assert_eq!(queue.progress().tiers[WarmTier::Map as usize].jobs_failed, 1);
}

#[test]
fn shutdown_before_start_finishes_the_handle_without_a_draw() {
    let queue = PipelineWarmQueue::new(1);
    let prepared = prepare_terrain(&queue, FORMAT, |_| panic!("must not run"));
    queue.shutdown();
    assert!(prepared.is_finished());
    assert!(!prepared.is_ready());
    assert!(prepared.take().expect("completed preparation").is_err());
}

#[test]
fn preparation_after_shutdown_and_panicking_builder_resolve_as_errors() {
    let queue = PipelineWarmQueue::new(1);
    queue.shutdown();
    let prepared = prepare_terrain(&queue, FORMAT, |_| panic!("closed queue must not run"));
    assert!(prepared.is_finished());
    assert!(!prepared.is_ready());
    assert!(prepared.take().expect("completed preparation").is_err());

    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let queue = PipelineWarmQueue::new(1);
    let prepared = prepare_terrain(&queue, FORMAT, |_| panic!("test builder panic"));
    assert_eq!(queue.run_inline(&device, WarmTier::Map), 1);
    assert!(prepared.is_finished());
    assert!(!prepared.is_ready());
    assert!(prepared.take().expect("completed preparation").is_err());
    assert_eq!(queue.progress().tiers[WarmTier::Map as usize].jobs_failed, 1);
}

#[test]
fn preparation_rejects_a_draw_from_another_device() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = RenderDevice::headless().expect("first device");
    let second = RenderDevice::headless().expect("second device");
    let queue = PipelineWarmQueue::new(1);
    let prepared = prepare_terrain(&queue, FORMAT, move |_| draw(&first));
    queue.run_inline(&second, WarmTier::Map);
    assert!(!prepared.is_ready());
    assert_eq!(
        prepared.take().expect("completed preparation").err().as_deref(),
        Some("terrain was constructed on a different render device")
    );
}

fn optional_draw(device: &RenderDevice, vt: bool) -> Result<TerrainSharedDraw, String> {
    use regia_terrain::{
        GrassLayer, GrassMask, GrassMaskInputs, GrassSet, HoleEllipse, HolesMask, ImpostorAtlas,
        ImpostorLayer, ImpostorSettings, TerrainGroundLook, VirtualTextureSettings,
    };
    let mut terrain = draw(device)?;
    let holes = HolesMask::new(vec![HoleEllipse {
        center: [0.0, 0.0],
        radius: [2.0, 2.0],
        rotation_radians: 0.0,
    }])
    .map_err(|e| e.to_string())?;
    terrain.set_holes(Some(&holes));
    terrain.ground_look_handle().set(TerrainGroundLook {
        variation: 0.5,
        ..TerrainGroundLook::default()
    });
    terrain.gpu_mut().set_warm_surface_weather(true);
    terrain
        .set_virtual_texture(vt.then_some(VirtualTextureSettings {
            atlas_slots: 2,
            page_texels: 14,
            lod0_texel_m: 0.5,
            bakes_per_frame: 1,
            rebakes_per_frame: 1,
            late_depth_fallback: true,
            ..VirtualTextureSettings::default()
        }))
        .map_err(|e| e.to_string())?;
    let set = GrassSet::winter();
    let mask = GrassMask::build(&set, &GrassMaskInputs::new(terrain.map())).map_err(|e| e.to_string())?;
    let grass = GrassLayer::new(&device.device, &device.queue, set, mask).map_err(|e| e.to_string())?;
    terrain.set_grass(Some(grass));
    let atlas = ImpostorAtlas {
        settings: ImpostorSettings {
            grid: 2,
            cell: 8,
            ..ImpostorSettings::default()
        },
        cards: Vec::new(),
        protos: Vec::new(),
    };
    let impostors = ImpostorLayer::new(&device.device, &device.queue, &atlas);
    terrain.set_impostors(Some(&impostors));
    Ok(terrain)
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice) {
    use regia_render::{
        DirectionalLightUniform, FrameQuality, LabScene, PostSettings, RenderProfile, RenderProfilePreset,
    };
    let post = PostSettings::golden_deterministic();
    let scene = LabScene {
        width: 64,
        height: 48,
        eye: [0.0, 12.0, -20.0],
        target: [0.0; 3],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 100.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.2, -0.7, 0.6], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileLow));
    quality.hdr_target = regia_render::HdrTarget::Rgba16f;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    renderer.render_offscreen(device, &scene).expect("terrain frame");
}

#[test]
fn configured_optional_state_is_kept_and_first_draw_creates_no_terrain_pipelines() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for vt in [false, true] {
        let device = RenderDevice::headless().expect("lavapipe device");
        let queue = PipelineWarmQueue::new(1);
        let outside = queue.progress().pipelines_outside;
        let prepared = prepare_terrain(&queue, FORMAT, move |device| optional_draw(device, vt));
        let before_pipelines = pipelines_created();
        regia_render::trace_frame_path(true);
        queue.run_inline(&device, WarmTier::Map);
        let warm_events = regia_render::take_frame_path_events();
        regia_render::trace_frame_path(false);
        let pipeline_events: Vec<_> = warm_events
            .iter()
            .filter(|e| e.kind == regia_render::FramePathKind::Pipeline)
            .collect();
        assert!(!pipeline_events.is_empty());
        assert_eq!(
            pipeline_events.len() as u64,
            pipelines_created() - before_pipelines
        );
        assert!(pipeline_events.iter().all(|e| e.detail.warm_worker));
        assert!(prepared.is_ready(), "{:?}", queue.progress().last_error);
        let mut draw = prepared
            .take()
            .expect("completed preparation")
            .expect("prepared draw");
        assert_eq!(draw.gpu().shared_pipelines_ground_look(), Some(true));
        assert_eq!(draw.gpu().vt_active(), vt);
        assert!(draw.grass_handle().is_some());
        assert_eq!(queue.progress().pipelines_outside, outside);
        let before = pipelines_created();
        draw.prewarm(FORMAT);
        assert_eq!(pipelines_created(), before, "all configured pipelines exist");
        let stats = draw.stats();
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(draw));
        assert_eq!(pipelines_created(), before, "install only transfers ownership");
        regia_render::trace_frame_path(true);
        render(&mut renderer, &device);
        assert!(stats.camera_nodes() > 0, "the first frame must draw terrain");
        let late: Vec<_> = regia_render::take_frame_path_events()
            .into_iter()
            .filter(|e| {
                e.kind == regia_render::FramePathKind::Pipeline
                    && e.detail
                        .site
                        .is_some_and(|site| site.file().contains("regia_terrain"))
            })
            .collect();
        regia_render::trace_frame_path(false);
        assert!(
            late.is_empty(),
            "first draw late terrain pipelines (VT {vt}): {late:?}"
        );
        let before_wet = pipelines_created();
        renderer.set_surface_weather(regia_render::SurfaceWeatherFrame {
            wetness: 0.5,
            ..regia_render::SurfaceWeatherFrame::DRY
        });
        render(&mut renderer, &device);
        assert_eq!(
            pipelines_created(),
            before_wet,
            "weather prewarm covers VT {vt} compute too"
        );
    }
}

#[test]
fn readiness_belongs_to_this_terrain_not_the_entire_map_tier() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let queue = PipelineWarmQueue::new(1);
    queue.set_engine_jobs(false);
    let prepared = prepare_terrain(&queue, FORMAT, draw);
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    queue.push(WarmTier::Map, "unrelated loading work", move |_| {
        started_tx.send(()).expect("started receiver");
        finish_rx.recv().expect("finish signal");
        Ok(())
    });
    queue.start(&device);
    started_rx.recv().expect("started signal");
    assert!(prepared.is_ready());
    assert!(!queue.is_tier_ready(WarmTier::Map));
    let before = pipelines_created();
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(
        prepared
            .take()
            .expect("completed preparation")
            .expect("prepared draw"),
    ));
    assert_eq!(pipelines_created(), before);
    finish_tx.send(()).expect("finish receiver");
    assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(300)));
    queue.shutdown();
}

#[test]
fn unsupported_vt_allocation_does_not_publish_a_successful_preparation() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    assert!(device.device.limits().max_texture_dimension_2d < 65_536);
    let queue = PipelineWarmQueue::new(1);
    let prepared = prepare_terrain(&queue, FORMAT, |device| {
        let mut terrain = draw(device)?;
        terrain
            .set_virtual_texture(Some(regia_terrain::VirtualTextureSettings {
                page_texels: 1022,
                atlas_slots: 64,
                late_depth_fallback: true,
                ..regia_terrain::VirtualTextureSettings::default()
            }))
            .map_err(|e| e.to_string())?;
        assert!(terrain.gpu().vt_supported(), "test needs the lavapipe VT path");
        Ok(terrain)
    });
    queue.run_inline(&device, WarmTier::Map);
    assert!(prepared.is_finished());
    assert!(
        !prepared.is_ready(),
        "missing configured pipelines must not report success"
    );
    assert!(prepared.take().expect("completed preparation").is_err());
}
