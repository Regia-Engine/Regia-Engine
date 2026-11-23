// SPDX-License-Identifier: MPL-2.0
//! Warm coverage of the `MASK_DECALS` forward variants (material mask recolor and decal slots):
//!
//! 1. A warm whose frame has masked instances builds their opaque and blend variants on the warm
//!    worker: the frames after it create no pipeline and match a renderer that built everything
//!    inline.
//! 2. Masked instances that appear mid-play, after a warm that never saw them, never compile a
//!    variant on the frame path: the draws keep the standard surface until a warm worker has built
//!    it, then match the inline renderer.
//!
//! Its own test binary: the warm queue and the deferred registry are process-wide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_core::glam::{Mat4, Vec3};
use regia_render::pipeline_warm::{blocking_scope, deferred_pending};
use regia_render::{
    ALPHA_BLEND, DecalSlot, DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer,
    FrameScene, LabMaterial, LabVertex, MaskDecalInstance, MaskDecalTextures, MaskDecals, MaskPalette,
    PipelineWarmQueue, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast, SharedImage, WarmTier,
};

struct Level {
    quad: (Vec<LabVertex>, Vec<u32>),
    opaque: LabMaterial,
    blended: LabMaterial,
    textures: Arc<MaskDecalTextures>,
    quality: FrameQuality,
}

fn level() -> Arc<Level> {
    let n = [0.0, 0.0, 1.0];
    let quad = (
        vec![
            LabVertex::from_pos_normal_uv([-0.5, -0.5, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.5, -0.5, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([0.5, 0.5, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-0.5, 0.5, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    );
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    let opaque = LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        ..LabMaterial::default()
    };
    let mask = SharedImage::rgba(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 0], false).expect("mask");
    let atlas = SharedImage::rgba(2, 2, vec![255; 16], true).expect("atlas");
    Arc::new(Level {
        quad,
        blended: LabMaterial {
            base_color: [0.8, 0.8, 0.8, 0.7],
            alpha_mode: ALPHA_BLEND,
            ..LabMaterial::default()
        },
        opaque,
        textures: Arc::new(MaskDecalTextures {
            mask: Some(mask),
            atlas: Some(atlas),
            atlas_grid: [1, 1],
        }),
        quality,
    })
}

fn scene(level: &Level) -> FrameScene<'_> {
    let meshes = [(&level.opaque, -0.6f32), (&level.blended, 0.6)]
        .into_iter()
        .enumerate()
        .map(|(i, (material, x))| FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 101 + i as u64,
            object_key: 201 + i as u64,
            vertices: &level.quad.0,
            indices: &level.quad.1,
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

fn instances(level: &Level) -> Vec<MaskDecalInstance> {
    let palette = MaskPalette::from_srgb8([200, 16, 46], [20, 40, 160], [230, 200, 40])
        .with_emissive_srgb8([255, 140, 0], 2.0);
    (0..2)
        .map(|i| MaskDecalInstance {
            object_key: 201 + i,
            decals: MaskDecals::new(level.textures.clone(), palette)
                .with_decal(DecalSlot::atlas(0, [0.1, 0.1, 0.4, 0.4]).with_color([1.0, 1.0, 1.0, 1.0])),
        })
        .collect()
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
fn mask_decal_variants_never_compile_on_the_frame_path() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let level = level();

    // Reference: everything built inline (a loading screen may wait).
    let expected = {
        let _blocking = blocking_scope();
        let mut inline = FrameRenderer::new();
        inline.set_mask_decals(&instances(&level));
        let pixels = inline
            .render_offscreen(&device, &scene(&level))
            .expect("reference")
            .pixels;
        assert_eq!(inline.mask_decal_stats().masked_draws, 2);
        pixels
    };

    // 1. The warm sees the masked instances.
    let mut prepared = FrameRenderer::new();
    prepared.set_mask_decals(&instances(&level));
    let mut renderer = warmed(&queue, &level, prepared);
    regia_render::trace_frame_path(true);
    let _ = pipeline_labels();
    for frame in 0..3 {
        let pixels = renderer
            .render_offscreen(&device, &scene(&level))
            .expect("frame")
            .pixels;
        if frame == 0 {
            assert_eq!(
                pixels, expected,
                "a warmed level draws its masked instances at once"
            );
        }
        let stats = renderer.mask_decal_stats();
        assert_eq!((stats.masked_draws, stats.fallback_draws), (2, 0), "{stats:?}");
    }
    let created = pipeline_labels();
    assert!(
        created.is_empty(),
        "frames after the warm created pipelines: {created:?}"
    );

    // 2. Masked instances appear mid-play, after a warm without them.
    let mut renderer = warmed(&queue, &level, FrameRenderer::new());
    let _ = pipeline_labels();
    renderer.set_mask_decals(&instances(&level));
    let first = renderer
        .render_offscreen(&device, &scene(&level))
        .expect("frame")
        .pixels;
    let first_stats = renderer.mask_decal_stats();
    let created = pipeline_labels();
    let variants: Vec<&String> = created
        .iter()
        .filter(|l| l.starts_with("lab mask decals"))
        .collect();
    assert!(
        variants.is_empty(),
        "a MASK_DECALS variant compiled on the frame path: {variants:?}"
    );
    assert_eq!(
        first_stats.masked_draws + first_stats.fallback_draws,
        2,
        "{first_stats:?}"
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
    assert_eq!(renderer.mask_decal_stats().masked_draws, 2);
    assert_eq!(
        later, expected,
        "once built in the background the instances draw masked"
    );
    let created = pipeline_labels();
    assert!(
        !created.iter().any(|l| l.starts_with("lab mask decals")),
        "frame-path pipelines after the background build: {created:?}"
    );
    println!(
        "mid-play masked instances: first frame {} (masked {}, fallback {})",
        if first == expected {
            "already masked"
        } else {
            "standard surface"
        },
        first_stats.masked_draws,
        first_stats.fallback_draws
    );
    regia_render::trace_frame_path(false);
    queue.shutdown();
}
