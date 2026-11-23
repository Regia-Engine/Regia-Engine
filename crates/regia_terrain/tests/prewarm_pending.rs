// SPDX-License-Identifier: MPL-2.0
//! A host may configure the draw through its handles before preparing it.
use regia_render::RenderDevice;
use regia_terrain::{CdlodConfig, TerrainGroundLook, TerrainLayers, TerrainMap, TerrainSharedDraw};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn prewarm_applies_the_pending_ground_look_before_compiling() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let map = TerrainMap::from_heights(17, 17, vec![0.0; 17 * 17], 32.0, 8.0, 0.0).expect("valid flat map");
    let mut draw = TerrainSharedDraw::new(&device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
        .expect("terrain construction");
    draw.ground_look_handle().set(TerrainGroundLook {
        variation: 0.5,
        ..TerrainGroundLook::default()
    });
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    assert_eq!(draw.gpu().shared_pipelines_ground_look(), Some(true));
}

#[test]
fn prewarm_applies_cost_debug_view_before_the_first_frame() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let map = TerrainMap::from_heights(17, 17, vec![0.0; 17 * 17], 32.0, 8.0, 0.0).expect("valid flat map");
    let mut draw = TerrainSharedDraw::new(&device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
        .expect("terrain construction");
    draw.set_virtual_texture(None).expect("disable VT");
    draw.set_debug_view(78);
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    let mut renderer = regia_render::FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw)));
    let queue = regia_render::PipelineWarmQueue::new(1);
    let terrain_creates = || {
        queue
            .progress()
            .outside_sites
            .iter()
            .filter(|(site, _)| site.contains("regia_terrain"))
            .map(|(_, count)| u64::from(*count))
            .sum::<u64>()
    };
    let before = terrain_creates();
    regia_render::trace_frame_path(true);
    frame(&device, &mut renderer);
    assert_eq!(
        terrain_creates(),
        before,
        "terrain compiles on first cost-view frame"
    );
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
        "prewarmed cost view creates on first frame: {late:?}"
    );
}

#[test]
fn synchronous_terrain_pipelines_emit_tracked_creation_records() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let map = TerrainMap::from_heights(17, 17, vec![0.0; 17 * 17], 32.0, 8.0, 0.0).expect("valid flat map");
    regia_render::trace_frame_path(true);
    let mut draw = TerrainSharedDraw::new(&device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
        .expect("terrain construction");
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    let pipelines: Vec<_> = regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == regia_render::FramePathKind::Pipeline)
        .collect();
    regia_render::trace_frame_path(false);
    let passes =
        4 + usize::from(device.device.limits().max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS);
    let variants = if draw.gpu().local_lights_supported() { 4 } else { 2 };
    // The other cloud-map state's colour pipelines (ADR-7886): one ground-look colour pipeline per light
    // variant and hole cut.
    let cloud_map = variants;
    let labels: Vec<_> = pipelines.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(
        pipelines.len(),
        6 + passes * variants + cloud_map,
        "every supported pipeline is tracked: {labels:?}"
    );
    assert_eq!(
        labels.iter().filter(|l| l.contains("ground look)")).count(),
        cloud_map,
        "the cloud-map state's colour pipelines: {labels:?}"
    );
    assert!(pipelines.iter().all(|e| !e.detail.warm_worker));
    assert!(pipelines.iter().all(|e| e.detail.site.is_some()));
}

fn frame(device: &RenderDevice, renderer: &mut regia_render::FrameRenderer) {
    use regia_render::{
        DirectionalLightUniform, FrameQuality, HdrTarget, LabScene, PostSettings, RenderProfile,
        RenderProfilePreset,
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
    quality.hdr_target = HdrTarget::Rgba16f;
    let mut scene = scene.as_frame_scene_with_quality(quality);
    scene.post = Some(&post);
    renderer.render_offscreen(device, &scene).expect("terrain frame");
}

#[test]
fn weather_prewarm_covers_the_virtual_texture_compute_rebuild() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe device");
    let map = TerrainMap::from_heights(17, 17, vec![0.0; 17 * 17], 32.0, 8.0, 0.0).expect("valid flat map");
    let mut draw = TerrainSharedDraw::new(&device, map, &TerrainLayers::default(), CdlodConfig::default(), 0)
        .expect("terrain construction");
    draw.set_virtual_texture(Some(regia_terrain::VirtualTextureSettings {
        atlas_slots: 2,
        page_texels: 14,
        lod0_texel_m: 0.5,
        bakes_per_frame: 1,
        rebakes_per_frame: 1,
        late_depth_fallback: true,
        ..regia_terrain::VirtualTextureSettings::default()
    }))
    .expect("valid VT settings");
    draw.gpu_mut().set_warm_surface_weather(true);
    draw.prewarm(wgpu::TextureFormat::Rgba16Float);
    assert!(draw.gpu().vt_active());
    let mut renderer = regia_render::FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw)));
    frame(&device, &mut renderer);
    let before = regia_render::pipeline_cache::pipelines_created();
    renderer.set_surface_weather(regia_render::SurfaceWeatherFrame {
        wetness: 0.5,
        ..regia_render::SurfaceWeatherFrame::DRY
    });
    frame(&device, &mut renderer);
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        before,
        "weather prewarm must build the VT compute variant too"
    );
}
