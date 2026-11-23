// SPDX-License-Identifier: MPL-2.0
//! Deferred forward variants (`pipeline_warm::DeferredPipeline`) never compile on the frame path
//! once warm workers run:
//!
//! 1. A warm builds every blend / glass variant a material of its frame can draw, visible or
//!    not, so a level whose glass is off-screen during the warm still draws it later without a
//!    pipeline create, with the same pixels as a renderer that built everything inline.
//! 2. A blended or glass material that appears mid-play, after a warm that never saw it, never
//!    compiles its variant on this thread: either the background already built it (released
//!    after the first warm) or the draw is skipped until it has; the frame then matches the
//!    inline renderer.
//!
//! Its own test binary: the warm queue and the deferred registry are process-wide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_core::glam::{Mat4, Vec3};
use regia_render::pipeline_warm::{blocking_scope, deferred_pending};
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabVertex, PipelineWarmQueue, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    WarmTier,
};

struct Level {
    floor: (Vec<LabVertex>, Vec<u32>),
    pane: (Vec<LabVertex>, Vec<u32>),
    floor_mat: LabMaterial,
    blended: LabMaterial,
    tinted: LabMaterial,
    quality: FrameQuality,
}

fn square(h: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]];
    let vertices = corners
        .into_iter()
        .map(|position| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

fn level(double_sided_glass: bool) -> Arc<Level> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.csm_resolution = 256;
    quality.local_shadow_atlas_size = 1024;
    Arc::new(Level {
        floor: square(6.0, 0.0),
        pane: square(1.0, 1.0),
        floor_mat: LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            double_sided: double_sided_glass,
            ..LabMaterial::default()
        },
        blended: LabMaterial {
            base_color: [0.2, 0.3, 0.4, 0.7],
            alpha_mode: ALPHA_BLEND,
            ..LabMaterial::default()
        },
        tinted: LabMaterial {
            base_color: [0.9, 0.7, 0.5, 1.0],
            transmission: 1.0,
            double_sided: double_sided_glass,
            ..LabMaterial::default()
        },
        quality,
    })
}

/// The level with its two panes at `pane_x` (in view at 0, behind the camera far away), or
/// without them.
fn scene(level: &Level, panes: bool, pane_x: f32) -> FrameScene<'_> {
    let mut meshes = vec![FrameMesh {
        is_static: false,
        mesh_key: 1,
        material_key: 101,
        object_key: 201,
        vertices: &level.floor.0,
        indices: &level.floor.1,
        material: &level.floor_mat,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }];
    if panes {
        for (i, material) in [&level.blended, &level.tinted].into_iter().enumerate() {
            meshes.push(FrameMesh {
                is_static: false,
                mesh_key: 2,
                material_key: 102 + i as u64,
                object_key: 202 + i as u64,
                vertices: &level.pane.0,
                indices: &level.pane.1,
                material,
                model: Mat4::from_translation(Vec3::new(pane_x + i as f32 * 0.5, i as f32 * 0.3, 0.0))
                    .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
    }
    FrameScene {
        width: 64,
        height: 48,
        eye: [0.0, 2.0, 5.0],
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
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0; 3], 50_000.0),
        local_lights: &[],
        quality: level.quality,
        meshes,
    }
}

/// Warms a fresh renderer on `queue`'s worker on `level` (panes loaded or not, off-screen).
fn warmed(queue: &PipelineWarmQueue, level: &Arc<Level>, panes: bool) -> FrameRenderer {
    let warm_level = level.clone();
    let handle = queue.warm_renderer(WarmTier::Map, "level", FrameRenderer::new(), move |r, device| {
        r.warm_scene(
            device,
            &scene(&warm_level, panes, 60.0),
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

fn pipeline_labels() -> Vec<String> {
    regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == FramePathKind::Pipeline)
        .map(|e| e.label)
        .collect()
}

#[test]
fn deferred_variants_never_compile_on_the_frame_path() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);

    // Reference: everything built inline (a loading screen may wait).
    let reference = |level: &Level| {
        let _blocking = blocking_scope();
        let mut inline = FrameRenderer::new();
        inline
            .render_offscreen(&device, &scene(level, true, 0.0))
            .expect("reference")
            .pixels
    };

    // 1. Glass and a blended pane loaded but off-screen during the warm (double-sided).
    let level_a = level(true);
    let expected_a = reference(&level_a);
    let mut renderer = warmed(&queue, &level_a, true);
    regia_render::trace_frame_path(true);
    let _ = pipeline_labels();
    for frame in 0..3 {
        let pixels = renderer
            .render_offscreen(&device, &scene(&level_a, true, 0.0))
            .expect("frame")
            .pixels;
        if frame == 0 {
            // Both are a renderer's first frame of this view (the warm ends with a camera cut).
            assert_eq!(pixels, expected_a, "a warmed level draws its glass at once");
        }
    }
    let created = pipeline_labels();
    assert!(
        created.is_empty(),
        "frames after the warm created pipelines: {created:?}"
    );

    // 2. Panes the warm never saw appear mid-play (single-sided: the frame's own set).
    let level_b = level(false);
    let expected_b = reference(&level_b);
    let mut renderer = warmed(&queue, &level_b, false);
    let _ = pipeline_labels();
    let first = renderer
        .render_offscreen(&device, &scene(&level_b, true, 0.0))
        .expect("frame")
        .pixels;
    let created = pipeline_labels();
    let variants: Vec<&String> = created
        .iter()
        .filter(|l| {
            ["lab blend", "lab premul", "lab additive", "lab transmit"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .collect();
    assert!(
        variants.is_empty(),
        "a deferred variant compiled on the frame path: {variants:?}"
    );
    let start = Instant::now();
    while deferred_pending() > 0 && start.elapsed() < Duration::from_secs(300) {
        std::thread::sleep(Duration::from_millis(5));
    }
    // A fresh history, so the frame compares with the inline renderer's first frame.
    renderer.camera_cut();
    let later = renderer
        .render_offscreen(&device, &scene(&level_b, true, 0.0))
        .expect("frame")
        .pixels;
    assert_eq!(
        later, expected_b,
        "once built in the background the panes draw normally"
    );
    println!(
        "mid-play panes: first frame {}; after the background build identical; frame-path pipelines on the first frame: {created:?}",
        if first == expected_b {
            "already built in the background"
        } else {
            "skipped"
        }
    );
    regia_render::trace_frame_path(false);
    queue.shutdown();
}
