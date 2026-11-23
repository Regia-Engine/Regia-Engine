// SPDX-License-Identifier: MPL-2.0
//! Warm coverage of the `THIN_FILM` forward variants (ADR-1977):
//!
//! 1. A warm whose frame draws a filmed material builds its variants on the warm worker: the
//!    frames after it create no pipeline and match a renderer that built everything inline.
//! 2. A filmed material that appears mid-play, after a warm that never saw it, never compiles a
//!    variant on the frame path: it keeps the base Fresnel until a warm worker has built the
//!    variant, then matches the inline renderer.
//! 3. [`FrameRenderer::prewarm`] covers a level's filmed materials: the first frame shows the film.
//!
//! Its own test binary: the warm queue and the deferred registry are process-wide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_core::glam::{Mat4, Vec3};
use regia_render::pipeline_warm::{blocking_scope, deferred_pending};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PipelineWarmQueue, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    ThinFilmFactors, WarmTier, uv_sphere,
};

struct Level {
    sphere: (Vec<LabVertex>, Vec<u32>),
    plain: LabMaterial,
    filmed: LabMaterial,
    quality: FrameQuality,
}

fn level() -> Arc<Level> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    let plain = LabMaterial {
        base_color: [0.03, 0.03, 0.05, 1.0],
        roughness: 0.3,
        ..LabMaterial::default()
    };
    Arc::new(Level {
        sphere: uv_sphere(0.45, 24, 12),
        filmed: LabMaterial {
            thin_film: ThinFilmFactors {
                factor: 1.0,
                ..ThinFilmFactors::OFF
            },
            ..plain.clone()
        },
        plain,
        quality,
    })
}

fn scene(level: &Level, film: bool) -> FrameScene<'_> {
    let first = if film { &level.filmed } else { &level.plain };
    let meshes = [(first, -0.6f32), (&level.plain, 0.6)]
        .into_iter()
        .enumerate()
        .map(|(i, (material, x))| FrameMesh {
            is_static: false,
            mesh_key: 1,
            // The filmed material is another material: a new key, like a new `Arc` on the ECS path.
            material_key: if film && i == 0 { 301 } else { 101 + i as u64 },
            object_key: 201 + i as u64,
            vertices: &level.sphere.0,
            indices: &level.sphere.1,
            material,
            model: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        })
        .collect();
    FrameScene {
        width: 96,
        height: 48,
        eye: [0.0, 0.0, 2.5],
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
        meshes,
    }
}

fn pipeline_labels() -> Vec<String> {
    regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == FramePathKind::Pipeline)
        .map(|e| e.label)
        .collect()
}

/// Warms `renderer` on `queue`'s worker on the level.
fn warmed(
    queue: &PipelineWarmQueue,
    level: &Arc<Level>,
    renderer: FrameRenderer,
    film: bool,
) -> FrameRenderer {
    let warm_level = level.clone();
    let handle = queue.warm_renderer(WarmTier::Map, "level", renderer, move |r, device| {
        r.warm_scene(
            device,
            &scene(&warm_level, film),
            wgpu::TextureFormat::Rgba8Unorm,
            true,
        )
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
fn thin_film_variants_never_compile_on_the_frame_path() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let level = level();

    // Reference: everything built inline (a loading screen may wait).
    let expected = {
        let _blocking = blocking_scope();
        let mut inline = FrameRenderer::new();
        let pixels = inline
            .render_offscreen(&device, &scene(&level, true))
            .expect("frame")
            .pixels;
        assert_eq!(inline.thin_film_stats().drawn, 1);
        pixels
    };

    // 1. The warm sees the film.
    let mut renderer = warmed(&queue, &level, FrameRenderer::new(), true);
    regia_render::trace_frame_path(true);
    let _ = pipeline_labels();
    for frame in 0..3 {
        let pixels = renderer
            .render_offscreen(&device, &scene(&level, true))
            .expect("frame")
            .pixels;
        if frame == 0 {
            assert_eq!(pixels, expected, "a warmed level draws its film at once");
        }
        let stats = renderer.thin_film_stats();
        assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    }
    let created = pipeline_labels();
    assert!(
        created.is_empty(),
        "frames after the warm created pipelines: {created:?}"
    );

    // 2. The film appears mid-play, after a warm without it.
    let mut renderer = warmed(&queue, &level, FrameRenderer::new(), false);
    let _ = pipeline_labels();
    let first = renderer
        .render_offscreen(&device, &scene(&level, true))
        .expect("frame")
        .pixels;
    let first_stats = renderer.thin_film_stats();
    let created = pipeline_labels();
    let variants: Vec<&String> = created
        .iter()
        .filter(|l| l.starts_with("lab thin film"))
        .collect();
    assert!(
        variants.is_empty(),
        "a THIN_FILM variant compiled on the frame path: {variants:?}"
    );
    assert_eq!(first_stats.drawn + first_stats.fallback, 1, "{first_stats:?}");
    let start = Instant::now();
    while deferred_pending() > 0 && start.elapsed() < Duration::from_secs(300) {
        std::thread::sleep(Duration::from_millis(5));
    }
    renderer.camera_cut();
    let later = renderer
        .render_offscreen(&device, &scene(&level, true))
        .expect("frame")
        .pixels;
    assert_eq!(renderer.thin_film_stats().drawn, 1);
    assert_eq!(later, expected, "once built in the background the film draws");
    let created = pipeline_labels();
    assert!(
        !created.iter().any(|l| l.starts_with("lab thin film")),
        "frame-path pipelines after the background build: {created:?}"
    );
    println!(
        "mid-play film: first frame {} (drawn {}, fallback {})",
        if first == expected {
            "already filmed"
        } else {
            "base Fresnel"
        },
        first_stats.drawn,
        first_stats.fallback
    );
    // 3. `prewarm` covers a level's filmed materials while warm workers run (a variant it left out
    // would fall back here).
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    };
    let mut frame = scene(&level, true);
    frame.post = Some(&post);
    frame.quality = FrameQuality::LAB;
    let mut renderer = FrameRenderer::new();
    renderer.prewarm(&device, &frame.meshes, 4).expect("prewarm");
    let _ = pipeline_labels();
    renderer.render_offscreen(&device, &frame).expect("frame");
    let stats = renderer.thin_film_stats();
    assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    let created = pipeline_labels();
    assert!(
        !created.iter().any(|l| l.starts_with("lab thin film")),
        "a prewarmed film compiled on the frame path: {created:?}"
    );
    regia_render::trace_frame_path(false);
    queue.shutdown();
}
