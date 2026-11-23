// SPDX-License-Identifier: MPL-2.0
//! Warm coverage of the morph-target pre-pass (Tincal engine item B1):
//!
//! 1. A warm whose frame has morphed instances builds the morph pipeline on the warm worker: the
//!    frames after it create no pipeline and match a renderer that built everything inline.
//! 2. Morphed instances that appear mid-play, after a warm that never saw them, never compile the
//!    pipeline on the frame path: the mesh draws undeformed until a warm worker has built it,
//!    then matches the inline renderer.
//!
//! Its own test binary: the warm queue and the deferred registry are process-wide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_render::pipeline_warm::{blocking_scope, deferred_pending};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, MorphInstance, MorphMode, MorphTargets, PipelineWarmQueue, RenderDevice, RenderProfile,
    RenderProfilePreset, ShadowCast, WarmTier,
};

const OBJECT: u64 = 0x0B1D_00AA;

struct Level {
    panel: (Vec<LabVertex>, Vec<u32>),
    targets: Arc<MorphTargets>,
    material: LabMaterial,
    quality: FrameQuality,
}

fn level() -> Arc<Level> {
    const N: usize = 5;
    let mut vertices = Vec::new();
    for j in 0..N {
        for i in 0..N {
            let (u, v) = (i as f32 / (N - 1) as f32, j as f32 / (N - 1) as f32);
            vertices.push(LabVertex::from_pos_normal_uv(
                [u - 0.5, v - 0.5, 0.0],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            ));
        }
    }
    let mut indices = Vec::new();
    for j in 0..N - 1 {
        for i in 0..N - 1 {
            let a = (j * N + i) as u32;
            indices.extend([a, a + 1, a + N as u32 + 1, a, a + N as u32 + 1, a + N as u32]);
        }
    }
    let pull = regia_asset::CpuMorphTarget {
        name: "pull".into(),
        vertex_indices: vec![6, 7, 8, 11, 12, 13],
        position_deltas: vec![[0.0, 0.25, 0.3]; 6],
        normal_deltas: vec![[0.0, -0.4, 0.0]; 6],
        tangent_deltas: Vec::new(),
    };
    let targets = Arc::new(MorphTargets::new(&vertices, &[pull]).expect("targets"));
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    Arc::new(Level {
        panel: (vertices, indices),
        targets,
        material: LabMaterial {
            base_color: [0.8, 0.7, 0.6, 1.0],
            ..LabMaterial::default()
        },
        quality,
    })
}

fn scene(level: &Level) -> FrameScene<'_> {
    FrameScene {
        width: 96,
        height: 64,
        eye: [0.0, 0.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 1_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 50_000.0),
        local_lights: &[],
        quality: level.quality,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 0x0B1D_0100,
            material_key: 0x0B1D_0200,
            object_key: OBJECT,
            vertices: &level.panel.0,
            indices: &level.panel.1,
            material: &level.material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }],
    }
}

fn instances(level: &Level) -> Vec<MorphInstance> {
    vec![MorphInstance {
        object_key: OBJECT,
        targets: Arc::clone(&level.targets),
        weights: vec![0.9],
        mode: MorphMode::Game,
    }]
}

fn pipeline_labels() -> Vec<String> {
    regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == FramePathKind::Pipeline)
        .map(|e| e.label)
        .collect()
}

/// Warms `renderer` on `queue`'s worker on the level.
fn warmed(queue: &PipelineWarmQueue, level: &Arc<Level>, renderer: FrameRenderer) -> FrameRenderer {
    let warm_level = level.clone();
    let handle = queue.warm_renderer(WarmTier::Map, "level", renderer, move |r, device| {
        r.warm_scene(device, &scene(&warm_level), wgpu::TextureFormat::Rgba8Unorm, true)
            .map_err(|e| e.to_string())
    });
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(300)),
        "warm did not finish"
    );
    let warmed = handle.take().expect("renderer handed back");
    assert_eq!(warmed.error, None);
    warmed.renderer.expect("renderer")
}

#[test]
fn the_morph_pipeline_never_compiles_on_the_frame_path() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let level = level();

    // References: everything built inline (a loading screen may wait), each a first frame.
    let (expected, plain) = {
        let _blocking = blocking_scope();
        let plain = FrameRenderer::new()
            .render_offscreen(&device, &scene(&level))
            .expect("plain")
            .pixels;
        let mut inline = FrameRenderer::new();
        inline.set_morph_instances(&instances(&level));
        let pixels = inline
            .render_offscreen(&device, &scene(&level))
            .expect("reference")
            .pixels;
        assert_eq!(inline.morph_stats().dispatched, 1);
        assert_ne!(pixels, plain, "the morph shows");
        (pixels, plain)
    };

    // 1. The warm sees the morphed instance.
    let mut prepared = FrameRenderer::new();
    prepared.set_morph_instances(&instances(&level));
    let mut renderer = warmed(&queue, &level, prepared);
    regia_render::trace_frame_path(true);
    let _ = pipeline_labels();
    for frame in 0..3 {
        // A fresh weight each later frame: the pass runs again.
        let mut set = instances(&level);
        set[0].weights[0] = 0.9 - 0.1 * frame as f32;
        renderer.set_morph_instances(&set);
        let pixels = renderer
            .render_offscreen(&device, &scene(&level))
            .expect("frame")
            .pixels;
        let stats = renderer.morph_stats();
        assert_eq!((stats.drawn, stats.waiting_pipeline), (1, 0), "{stats:?}");
        if frame == 0 {
            assert_eq!(pixels, expected, "a warmed level draws its morphs at once");
        }
    }
    let created = pipeline_labels();
    assert!(
        created.is_empty(),
        "frames after the warm created pipelines: {created:?}"
    );

    // 2. Morphs appear mid-play, after a warm without them.
    let mut renderer = warmed(&queue, &level, FrameRenderer::new());
    let _ = pipeline_labels();
    renderer.set_morph_instances(&instances(&level));
    let first = renderer
        .render_offscreen(&device, &scene(&level))
        .expect("frame")
        .pixels;
    let first_stats = renderer.morph_stats();
    let created = pipeline_labels();
    assert!(
        !created.iter().any(|l| l == "morph targets"),
        "the morph pipeline compiled on the frame path: {created:?}"
    );
    let start = Instant::now();
    while deferred_pending() > 0 && start.elapsed() < Duration::from_secs(300) {
        std::thread::sleep(Duration::from_millis(5));
    }
    renderer.camera_cut();
    let later = renderer
        .render_offscreen(&device, &scene(&level))
        .expect("frame")
        .pixels;
    assert_eq!(renderer.morph_stats().waiting_pipeline, 0);
    assert_eq!(
        later, expected,
        "once built in the background the instance draws morphed"
    );
    let created = pipeline_labels();
    assert!(
        !created.iter().any(|l| l == "morph targets"),
        "frame-path pipelines after the background build: {created:?}"
    );
    println!(
        "mid-play morphs: first frame {} ({first_stats:?})",
        if first == expected {
            "already morphed"
        } else if first == plain {
            "undeformed"
        } else {
            "other"
        }
    );
    regia_render::trace_frame_path(false);
    queue.shutdown();
}
