// SPDX-License-Identifier: MPL-2.0
//! Terrain inside `FrameRenderer` on Desktop High, the frame-time contract.
//!
//! One graphics submit per steady frame with terrain installed (no second
//! encoder, no external composite). A held camera uploads no node bytes and
//! does not redraw the terrain into the sun cascades; a moving camera writes a
//! few contiguous node ranges, not one write per node.

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, TerrainDrawStats, TerrainLayers, TerrainMap, TerrainSharedDraw, synthetic_mountains,
};

const W: u32 = 128;
const H: u32 = 72;

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

/// A 2 km mountain island, so the selection has the hundreds of nodes a real map has.
fn mountains() -> TerrainMap {
    let n = 257;
    let heights = synthetic_mountains(n, 2048.0, 220.0, 7);
    TerrainMap::from_heights(n, n, heights, 2048.0, 1.0, 0.0).expect("map")
}

fn desktop_high() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
}

/// The scene from `eye`, with a box (a mesh caster) in front of the camera.
fn lab_scene(eye: [f32; 3], post: &PostSettings) -> LabScene {
    let (box_verts, box_idx) = regia_render::box_mesh([1.0, 1.0, 1.0]);
    let box_verts = regia_render::translate_mesh(&box_verts, [eye[0] + 2.0, eye[1] - 3.0, eye[2] - 8.0]);
    LabScene {
        width: W,
        height: H,
        eye,
        target: [eye[0] + 40.0, eye[1] - 30.0, eye[2] - 120.0],
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

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    stats: Arc<TerrainDrawStats>,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    post: PostSettings,
    /// Built once: a mesh's object key is its address, so a rebuilt scene
    /// would look like a new caster every frame.
    lab: LabScene,
}

impl Rig {
    fn new() -> Self {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU: {err}"),
        };
        let mut draw = TerrainSharedDraw::new(&device, mountains(), &layers(), CdlodConfig::default(), 6)
            .expect("terrain");
        draw.prewarm(wgpu::TextureFormat::Rgba16Float);
        let stats = draw.stats();
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain budget"),
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
        let post = PostSettings {
            auto_exposure: true,
            bloom: true,
            bloom_mips: 3,
            taa: true,
            ..PostSettings::default()
        };
        let lab = lab_scene(EYE, &post);
        Self {
            device,
            renderer,
            stats,
            _texture: texture,
            view,
            post,
            lab,
        }
    }

    /// One Desktop High frame from `eye`; the box caster stays where it was built.
    fn frame(&mut self, eye: [f32; 3]) {
        self.lab.eye = eye;
        self.lab.target = [eye[0] + 40.0, eye[1] - 30.0, eye[2] - 120.0];
        let lab = &self.lab;
        let mut scene = lab.as_frame_scene_with_quality(desktop_high());
        scene.post = Some(&self.post);
        scene.ibl = true;
        let target = FrameTarget::full(&self.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H);
        self.renderer
            .render_into(&self.device, &scene, target)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }

    /// Same frame through `render_offscreen`, which waits, so GPU timestamps
    /// belong to this frame.
    fn timed_frame(&mut self, eye: [f32; 3]) {
        self.lab.eye = eye;
        self.lab.target = [eye[0] + 40.0, eye[1] - 30.0, eye[2] - 120.0];
        let lab = &self.lab;
        let mut scene = lab.as_frame_scene_with_quality(desktop_high());
        scene.post = Some(&self.post);
        scene.ibl = true;
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
    }
}

const EYE: [f32; 3] = [-300.0, 260.0, 500.0];

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn terrain_frame_submits_once_and_uploads_only_dirty_nodes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    rig.frame(EYE);
    let selected = rig.stats.selected_nodes();
    let (first_writes, first_bytes) = rig.stats.last_node_upload();
    eprintln!("frame 1: {selected} nodes, {first_writes} node writes, {first_bytes} bytes");
    assert!(
        selected > 40,
        "the selection is too small to measure ({selected} nodes)"
    );
    assert!(first_bytes > 0, "frame 1 must upload the selection");
    assert!(
        first_writes <= 2,
        "frame 1 must upload the selection as one contiguous range, not per node ({first_writes} writes)"
    );
    for frame in 2..=6 {
        rig.frame(EYE);
        let timings = rig.renderer.cpu_timings();
        assert_eq!(
            timings.submits, 1,
            "frame {frame} with terrain submitted {} times (one encoder, one submit)",
            timings.submits
        );
        assert_eq!(
            rig.stats.last_node_upload(),
            (0, 0),
            "a held camera uploaded node bytes on frame {frame}"
        );
    }

    // A moving camera: the selection changes at a few nodes per frame.
    let mut eye = EYE;
    let mut max_writes = 0;
    let mut moved_bytes = 0u64;
    for _ in 0..8 {
        eye[0] += 6.0;
        eye[2] -= 6.0;
        rig.frame(eye);
        let (writes, bytes) = rig.stats.last_node_upload();
        max_writes = max_writes.max(writes);
        moved_bytes += bytes;
        assert_eq!(
            rig.renderer.cpu_timings().submits,
            1,
            "a moving terrain frame submitted twice"
        );
    }
    eprintln!(
        "moving: max {max_writes} node writes per frame, {moved_bytes} bytes over 8 frames, {} nodes, prepare {:.3} ms",
        rig.stats.selected_nodes(),
        rig.stats.last_prepare_ms()
    );
    assert!(
        moved_bytes > 0,
        "the selection never changed; the camera move is too small"
    );
    assert!(
        (max_writes as usize) * 4 < rig.stats.selected_nodes() as usize,
        "a moving camera wrote {max_writes} ranges for {} nodes: not batched",
        rig.stats.selected_nodes()
    );
}

#[test]
fn held_camera_renders_the_terrain_cascade_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    rig.frame(EYE);
    let after_first = rig.renderer.shared_shadow_draws();
    eprintln!("terrain cascade draws after frame 1: {after_first}");
    assert!(after_first > 0, "terrain never drew into the sun cascades");
    for _ in 0..5 {
        rig.frame(EYE);
    }
    assert_eq!(
        rig.renderer.shared_shadow_draws(),
        after_first,
        "a held camera re-rendered the terrain cascade"
    );
    // A large move changes the selection and the snapped cascades: redraw.
    rig.frame([EYE[0] + 200.0, EYE[1], EYE[2] - 200.0]);
    assert!(
        rig.renderer.shared_shadow_draws() > after_first,
        "a moved camera kept a stale terrain cascade"
    );
}

/// Per-pass cost on a terrain scene with a moving camera (lavapipe). A held
/// frame skips the terrain and mesh cascades; a moving frame redraws them.
/// The skip is read from the shadow cache report (render passes recorded, terrain
/// draws per cascade), not from the shadow stamps: on a shared GPU a held frame's
/// empty shadow interval (0.008–0.031 ms) read above a moving redraw (0.027 ms).
/// The numbers are copied into `docs/evidence/terrain-runtime/timings.md`.
#[test]
fn terrain_desktop_high_moving_pass_times() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = Rig::new();
    rig.post.auto_exposure = false;
    rig.post.exposure_ev = 14.5;
    rig.renderer.set_gpu_timing(&rig.device, true);
    let mut held_shadows = Vec::new();
    let mut moving_shadows = Vec::new();
    let mut held_passes = Vec::new();
    let mut moving_passes = Vec::new();
    let mut eye = EYE;
    for i in 0..10 {
        if i >= 4 {
            eye[0] += 6.0;
            eye[2] -= 6.0;
        }
        rig.timed_frame(eye);
        let cpu = rig.renderer.cpu_timings();
        let gpu = rig.renderer.gpu_timings();
        let (writes, bytes) = rig.stats.last_node_upload();
        let report = rig.renderer.shadow_cache_report();
        let passes = (
            report.shadow_passes,
            report.shared_per_cascade,
            report.jobs_per_kind,
        );
        eprintln!(
            "frame {i} shadow passes {} terrain per cascade {:?} mesh jobs per kind {:?}",
            passes.0, passes.1, passes.2
        );
        eprintln!(
            "frame {i} {} cpu encode {:.2} total {:.2} terrain prepare {:.3} node writes {writes} bytes {bytes} cascade draws {} | gpu shadows {:.3} prepass {:.3} forward {:.3} ssao {:.3} ssgi {:.3} ssr {:.3} atmos {:.3} taau {:.3} scene {:.3}",
            if i >= 4 { "moving" } else { "held" },
            cpu.encode_ms,
            cpu.total_ms,
            rig.stats.last_prepare_ms(),
            rig.renderer.shared_shadow_draws(),
            gpu.shadows_ms,
            gpu.prepass_ms,
            gpu.forward_ms,
            gpu.ssao_ms,
            gpu.ssgi_ms,
            gpu.ssr_ms,
            gpu.atmosphere_ms,
            gpu.taau_ms,
            gpu.scene_ms,
        );
        if (2..4).contains(&i) {
            held_shadows.push(gpu.shadows_ms);
            held_passes.push((i, passes));
        }
        if i >= 5 {
            moving_shadows.push(gpu.shadows_ms);
            moving_passes.push((i, passes));
        }
    }
    let held = held_shadows.iter().copied().fold(0.0f32, f32::max);
    let moving = moving_shadows.iter().sum::<f32>() / moving_shadows.len() as f32;
    eprintln!("shadows held max {held:.3} ms, moving mean {moving:.3} ms (printed, not asserted)");
    for (i, (shadow_passes, terrain, jobs)) in &held_passes {
        assert!(
            *shadow_passes == 0 && terrain.iter().all(|&t| t == 0) && jobs.iter().all(|&j| j == 0),
            "held frame {i} shadow passes not skipped: {shadow_passes} passes, terrain per cascade {terrain:?}, mesh jobs {jobs:?}"
        );
    }
    for (i, (shadow_passes, terrain, _)) in &moving_passes {
        assert!(
            *shadow_passes > 0 && terrain.iter().any(|&t| t != 0),
            "moving frame {i} did not redraw the cascades: {shadow_passes} passes, terrain per cascade {terrain:?}"
        );
    }
}
