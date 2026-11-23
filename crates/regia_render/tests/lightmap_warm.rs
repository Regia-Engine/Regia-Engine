// SPDX-License-Identifier: MPL-2.0
//! The `LIGHTMAP` variants are warm (ADR-2107): a level warmed with a lightmap installed draws its
//! receivers from the first frame with no pipeline or table created on the frame path, on the CPU, GPU-driven
//! and lite paths, and a frame mid-play without the warm keeps the probe light while the variant
//! builds in the background rather than compiling on the frame thread.

use std::sync::Arc;
use std::time::Duration;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::pipeline_warm::{blocking_scope, deferred_pending};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FramePathKind, FrameQuality, FrameRenderer, FrameScene,
    LabMaterial, LabVertex, PipelineWarmQueue, RenderDevice, ShadowCast, WarmTier,
};

const OBJECT: u64 = 0x0B1E_C801;

struct Level {
    vertices: Vec<LabVertex>,
    indices: Vec<u32>,
    uv2: Vec<[f32; 2]>,
    material: LabMaterial,
    map: Arc<Lightmap>,
}

fn level() -> Arc<Level> {
    let n = [0.0, 0.0, 1.0];
    let vertices = vec![
        LabVertex::from_pos_normal_uv([-0.5, -0.5, 0.0], n, [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([0.5, -0.5, 0.0], n, [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([0.5, 0.5, 0.0], n, [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([-0.5, 0.5, 0.0], n, [0.0, 0.0]),
    ];
    let indices = vec![0, 1, 2, 0, 2, 3];
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 4,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "receiver".into(),
                source_vertices: 4,
                rect: [0, 0, 4, 2],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &[[9_000.0; 3]; 8],
        vec![Coverage::BAKED; 8],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap");
    let built = receiver_mesh(&map, 0, &vertices, &indices).expect("the baked mesh");
    Arc::new(Level {
        vertices: built.vertices,
        indices: built.indices,
        uv2: built.uv2,
        material: LabMaterial::default(),
        map: Arc::new(map),
    })
}

fn scene(level: &Level, gpu_driven: bool) -> FrameScene<'_> {
    let identity = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    FrameScene {
        width: 96,
        height: 48,
        eye: [0.0, 0.0, 2.5],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 1_000.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 0.0),
        local_lights: &[],
        quality: FrameQuality {
            gpu_driven,
            ..FrameQuality::default()
        },
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 0x0A11_0201 << 3,
            material_key: 0x0A11_0202 << 3,
            object_key: OBJECT,
            vertices: &level.vertices,
            indices: &level.indices,
            material: &level.material,
            model: identity,
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &level.uv2,
            lod_dither: 0.0,
        }],
    }
}

fn settings() -> LightmapSettings {
    LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    }
}

/// What the frames since the last call created: every pipeline, and anything else of the lightmap's
/// (its tables and their bind groups).
fn created_labels() -> Vec<String> {
    regia_render::take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == FramePathKind::Pipeline || e.label.contains("lightmap"))
        .map(|e| e.label)
        .collect()
}

fn warmed(
    queue: &PipelineWarmQueue,
    level: &Arc<Level>,
    mut renderer: FrameRenderer,
    gpu_driven: bool,
) -> FrameRenderer {
    renderer.set_lightmap(Some(level.map.clone()), settings());
    renderer.set_lightmap_receivers(&[(OBJECT, 0)]);
    let warm_level = level.clone();
    let handle = queue.warm_renderer(WarmTier::Map, "level", renderer, move |r, device| {
        r.warm_scene(
            device,
            &scene(&warm_level, gpu_driven),
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

/// Warmed on each path: the first frame draws the receiver and no frame creates a pipeline.
#[test]
fn a_warmed_level_draws_receivers_at_once_and_compiles_nothing() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let level = level();
    for gpu_driven in [false, true] {
        for lite in [None, Some(ForwardLite::Low), Some(ForwardLite::High)] {
            let mut renderer = FrameRenderer::new();
            renderer.set_forward_lite(lite);
            let mut renderer = warmed(&queue, &level, renderer, gpu_driven);
            regia_render::trace_frame_path(true);
            let _ = created_labels();
            for frame in 0..3 {
                renderer
                    .render_offscreen(&device, &scene(&level, gpu_driven))
                    .expect("frame");
                let stats = renderer.lightmap_forward_stats();
                assert_eq!(
                    (stats.drawn, stats.fallback),
                    (1, 0),
                    "gpu_driven {gpu_driven}, lite {lite:?}, frame {frame}: {stats:?}"
                );
            }
            let created = created_labels();
            regia_render::trace_frame_path(false);
            assert!(
                created.is_empty(),
                "gpu_driven {gpu_driven}, lite {lite:?}: frames after the warm created {created:?}"
            );
        }
    }
    queue.shutdown();
}

/// A warm with no lightmap warms nothing of it (a project without lightmaps), unless the host asked
/// with `set_warm_lightmaps`: then a lightmap installed after the warm draws at once.
#[test]
fn set_warm_lightmaps_builds_the_variants_before_a_lightmap_exists() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let level = level();
    let warm = |asked: bool| {
        let mut renderer = FrameRenderer::new();
        renderer.set_warm_lightmaps(asked);
        let warm_level = level.clone();
        let handle = queue.warm_renderer(WarmTier::Map, "level", renderer, move |r, device| {
            r.warm_scene(
                device,
                &scene(&warm_level, true),
                wgpu::TextureFormat::Rgba8Unorm,
                true,
            )
            .map_err(|e| e.to_string())
        });
        assert!(queue.wait_tier(WarmTier::Map, Duration::from_secs(300)));
        let warmed = handle.take().expect("renderer handed back");
        assert_eq!(warmed.error, None);
        warmed.renderer.expect("renderer")
    };
    let mut renderer = warm(true);
    renderer.set_lightmap(Some(level.map.clone()), settings());
    renderer.set_lightmap_receivers(&[(OBJECT, 0)]);
    renderer
        .render_offscreen(&device, &scene(&level, true))
        .expect("frame");
    let stats = renderer.lightmap_forward_stats();
    assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");

    // Without it the same frame mid-play keeps the probe light while the variant builds.
    let mut renderer = warm(false);
    renderer.set_lightmap(Some(level.map.clone()), settings());
    renderer.set_lightmap_receivers(&[(OBJECT, 0)]);
    renderer
        .render_offscreen(&device, &scene(&level, true))
        .expect("frame");
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(stats.drawn + stats.fallback, 1, "{stats:?}");
    // It lands: waiting frames (a loading screen's scope builds what is left).
    {
        let _blocking = blocking_scope();
        renderer
            .render_offscreen(&device, &scene(&level, true))
            .expect("frame");
    }
    let stats = renderer.lightmap_forward_stats();
    assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    let _ = deferred_pending();
    queue.shutdown();
}
