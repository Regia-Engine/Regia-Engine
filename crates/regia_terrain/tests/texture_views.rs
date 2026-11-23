// SPDX-License-Identifier: MPL-2.0
//! Texture views over a terrain (v0.1.6, ADR-184 "Texture views keep their own view state").
//!
//! DO27 turned on three render-to-texture cameras (cockpit screen, target feed, full-screen
//! map) and its frame went from 26 to 60–87 ms of GPU: every view frame reselected the terrain
//! for its camera and overwrote the main camera's node slots, so the main camera re-uploaded
//! them; the shadow cascades and pages were replanned for each camera in turn. Here a main
//! camera on Desktop High (TAAU, paged sun shadows) over a 2 km terrain with a mesh caster
//! renders with three texture views, one of them a top-down map 2 km away, and must do exactly
//! what it does without them: the same node uploads, the same shadow work, the same pixels,
//! and no texture, bind group or buffer created per steady frame.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, FrameTarget, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
    TextureViewSettings,
};
use regia_terrain::{
    CdlodConfig, TerrainDrawStats, TerrainLayers, TerrainMap, TerrainSharedDraw, synthetic_mountains,
};

const W: u32 = 128;
const H: u32 = 72;
const EYE: [f32; 3] = [-300.0, 260.0, 500.0];
/// Texture view ids: cockpit screen, target feed, map.
const SCREEN: u64 = 1;
const FEED: u64 = 2;
const MAP: u64 = 3;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"
        TerrainLayers(
            version: 1,
            wetness_default: 0.0,
            layers: [
                LayerDesc(
                    name: "ground",
                    albedo: (0.30, 0.28, 0.22),
                    roughness: 0.9,
                    tiling: 4.0,
                    slope_min: 0.0,
                    slope_max: 1.6,
                    altitude_min: -1000.0,
                    altitude_max: 5000.0,
                    blend_sharpness: 0.15,
                    triplanar: false,
                ),
            ],
        )
        "#,
    )
    .expect("layers")
}

/// A 2 km mountain island: hundreds of selected nodes, as on a real map.
fn mountains() -> TerrainMap {
    let n = 257;
    let heights = synthetic_mountains(n, 2048.0, 220.0, 7);
    TerrainMap::from_heights(n, n, heights, 2048.0, 1.0, 0.0).expect("map")
}

/// Desktop High without the irradiance probes (they converge on their own clock; a texture
/// view never updates them either way).
fn desktop_high() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gi_probes = false;
    quality
}

/// The main scene with a box caster in front of the camera.
fn lab_scene(post: &PostSettings) -> LabScene {
    let (box_verts, box_idx) = regia_render::box_mesh([1.0, 1.0, 1.0]);
    let box_verts = regia_render::translate_mesh(&box_verts, [EYE[0] + 2.0, EYE[1] - 3.0, EYE[2] - 8.0]);
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [EYE[0] + 40.0, EYE[1] - 30.0, EYE[2] - 120.0],
        fov_y: 0.9,
        near: 0.1,
        far: 3000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.4], [1.0, 0.97, 0.92], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: box_verts,
            indices: box_idx,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// The three texture cameras: a screen looking back over the island, a feed looking down a
/// valley, and a top-down map 2 km from the main camera.
fn view_scene(lab: &LabScene, id: u64) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(desktop_high());
    scene.width = 96;
    scene.height = 96;
    match id {
        SCREEN => {
            scene.eye = [EYE[0], EYE[1] + 5.0, EYE[2]];
            scene.target = [EYE[0] - 200.0, 100.0, EYE[2] + 200.0];
        }
        FEED => {
            scene.eye = [200.0, 400.0, 200.0];
            scene.target = [0.0, 0.0, -300.0];
            scene.fov_y = 0.4;
        }
        _ => {
            scene.eye = [900.0, 2200.0, -900.0];
            scene.target = [900.01, 0.0, -900.0];
            scene.fov_y = 0.8;
            scene.far = 5000.0;
        }
    }
    scene
}

struct Rig {
    renderer: FrameRenderer,
    stats: Arc<TerrainDrawStats>,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    post: PostSettings,
    lab: LabScene,
}

impl Rig {
    fn new(device: &RenderDevice) -> Self {
        let mut draw = TerrainSharedDraw::new(device, mountains(), &layers(), CdlodConfig::default(), 6)
            .expect("terrain");
        draw.prewarm(wgpu::TextureFormat::Rgba16Float);
        let stats = draw.stats();
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_enabled(Some(false));
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture views main"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // Fixed exposure: the auto-exposure readback lands a frame or two late depending on the
        // GPU, which two renderers need not share.
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.5,
            bloom: true,
            bloom_mips: 3,
            taa: true,
            ..PostSettings::default()
        };
        let lab = lab_scene(&post);
        Self {
            renderer,
            stats,
            _texture: texture,
            view,
            post,
            lab,
        }
    }

    fn views(&mut self, device: &RenderDevice) {
        let settings = TextureViewSettings::default();
        for id in [SCREEN, FEED, MAP] {
            self.renderer
                .render_texture_view(device, id, view_scene(&self.lab, id), &settings)
                .unwrap_or_else(|err| panic!("view {id}: {err}"));
        }
    }

    /// One main frame; returns what the main camera uploaded and redrew.
    fn frame(&mut self, device: &RenderDevice) -> MainWork {
        let mut scene = self.lab.as_frame_scene_with_quality(desktop_high());
        scene.post = Some(&self.post);
        let target = FrameTarget::full(&self.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H);
        self.renderer
            .render_into(device, &scene, target)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        let report = self.renderer.shadow_cache_report();
        MainWork {
            node_upload: self.stats.last_node_upload(),
            shadow_passes: report.shadow_passes,
            dirty_pages: report.dirty_pages.len(),
            terrain_cascade_draws: self.renderer.shared_shadow_draws(),
        }
    }

    fn pixels(&mut self, device: &RenderDevice) -> Vec<u8> {
        let mut scene = self.lab.as_frame_scene_with_quality(desktop_high());
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("read: {err}"))
            .pixels
    }
}

#[derive(Debug, PartialEq)]
struct MainWork {
    node_upload: (u32, u64),
    shadow_passes: u32,
    dirty_pages: usize,
    terrain_cascade_draws: u64,
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn three_texture_views_leave_the_main_terrain_view_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let mut off = Rig::new(&device);
    let mut on = Rig::new(&device);
    for _ in 0..8 {
        off.frame(&device);
        on.views(&device);
        on.frame(&device);
    }
    let before = regia_render::frame_path_counters();
    let mut work_on = Vec::new();
    for _ in 0..4 {
        on.views(&device);
        work_on.push(on.frame(&device));
    }
    let created = regia_render::frame_path_counters().since(before);
    let work_off: Vec<MainWork> = (0..4).map(|_| off.frame(&device)).collect();
    eprintln!("steady frames with three views: {created:?}");
    eprintln!("main camera, views on: {work_on:?}");
    eprintln!("main camera, views off: {work_off:?}");
    assert_eq!(created.textures, 0, "textures created per steady frame");
    assert_eq!(created.bind_groups, 0, "bind groups created per steady frame");
    assert_eq!(created.buffers, 0, "buffers created per steady frame");
    assert_eq!(created.pipelines, 0, "pipelines created per steady frame");
    assert_eq!(
        created.inline_bakes, 0,
        "atmosphere tables rebuilt per steady frame"
    );
    assert_eq!(
        work_on, work_off,
        "texture views changed the main camera's uploads or shadow work"
    );
    assert!(
        work_on.iter().all(|w| w.node_upload == (0, 0)),
        "a held main camera uploaded terrain nodes: {work_on:?}"
    );

    on.views(&device);
    let a = off.pixels(&device);
    let b = on.pixels(&device);
    let diff = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    eprintln!("main view max channel difference, views on vs off: {diff}");
    assert!(
        diff <= 1,
        "texture views changed the main view's pixels (max {diff})"
    );

    // The map sees the terrain from 2 km up: shaded relief, not a flat colour or the sky.
    let map = on
        .renderer
        .read_texture_target(&device, MAP)
        .unwrap_or_else(|err| panic!("map: {err}"));
    let luma: Vec<f32> = map
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
        .collect();
    let mean = luma.iter().sum::<f32>() / luma.len() as f32;
    let var = luma.iter().map(|l| (l - mean).powi(2)).sum::<f32>() / luma.len() as f32;
    eprintln!("map view luma mean {mean:.1} sd {:.1}", var.sqrt());
    assert!(
        mean > 10.0 && var.sqrt() > 3.0,
        "the map shows no terrain (mean {mean}, var {var})"
    );
}

/// CPU and GPU cost of one 512² view on the reduced path, with continuous rendering (120 warm
/// frames, no readback in the loop), printed for the report. Run on purpose:
/// `cargo test -p regia_terrain --release --test texture_views -- --ignored --nocapture`.
#[test]
#[ignore = "timing run"]
fn texture_view_cost_512() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let mut rig = Rig::new(&device);
    rig.renderer.set_gpu_timing(&device, true);
    let settings = TextureViewSettings::default();
    let frames = regia_core::brand::env_var("TV_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u32);
    let mut samples: Vec<(u64, f32, f32, f32)> = Vec::new();
    for frame in 0..(120 + frames) {
        for id in [SCREEN, FEED, MAP] {
            let mut scene = view_scene(&rig.lab, id);
            scene.width = 512;
            scene.height = 512;
            let timing = rig
                .renderer
                .render_texture_view(&device, id, scene, &settings)
                .unwrap_or_else(|err| panic!("view {id}: {err}"));
            if frame >= 120 {
                let encode = rig
                    .renderer
                    .texture_view_cpu_timings(id)
                    .map_or(0.0, |cpu| cpu.encode_ms);
                samples.push((id, timing.cpu_ms, encode, timing.gpu_ms));
            }
        }
        rig.frame(&device);
    }
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    for id in [SCREEN, FEED, MAP] {
        let of = |pick: fn(&(u64, f32, f32, f32)) -> f32| {
            median(samples.iter().filter(|s| s.0 == id).map(pick).collect())
        };
        let gpu = rig.renderer.texture_view_gpu_timings(id).unwrap_or_default();
        eprintln!(
            "view {id} 512²: cpu {:.2} ms (encode {:.2}) gpu {:.3} ms | shadows {:.3} prepass {:.3} forward {:.3} post {:.3}",
            of(|s| s.1),
            of(|s| s.2),
            of(|s| s.3),
            gpu.shadows_ms,
            gpu.prepass_ms,
            gpu.forward_ms,
            gpu.post_ms
        );
    }
    let main = rig.renderer.gpu_timings();
    eprintln!("main {W}x{H} gpu {:.3} ms", main.frame_ms);
}

#[test]
fn view_count_counts_each_texture_view_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let mut draw =
        TerrainSharedDraw::new(&device, mountains(), &layers(), CdlodConfig::default(), 6).expect("terrain");
    assert_eq!(draw.view_count(), 0);
    draw.set_view(Some(SCREEN));
    assert_eq!(draw.view_count(), 1, "one view, active");
    draw.set_view(Some(MAP));
    assert_eq!(draw.view_count(), 2, "two views, one active");
    draw.set_view(None);
    assert_eq!(draw.view_count(), 2, "two views, none active");
    draw.drop_view(SCREEN);
    assert_eq!(draw.view_count(), 1);
    draw.set_view(Some(MAP));
    draw.drop_view(MAP);
    assert_eq!(draw.view_count(), 0, "dropping the active view");
}
