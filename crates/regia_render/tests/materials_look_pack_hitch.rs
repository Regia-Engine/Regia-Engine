// SPDX-License-Identifier: MPL-2.0
//! The hitch gate for the materials look pack (`tests/materials_look_pack.rs`): the warm that
//! `GameFrameWarm` runs, over one frame that holds a thin film, a scattering skin, rough glass and
//! a blended material at once with scattering, order-independent transparency and refraction on,
//! leaves nothing to build on the play frames. Each feature has its own hitch test; this one holds
//! them together. One test in its own binary, so the process-wide creation counters see only it.

use std::sync::Arc;

use regia_core::glam::{Mat4, Vec3};
use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, CpuMaterial, FrameRenderer, RenderDevice, SubsurfaceFactors,
    SubsurfaceProfile, ThinFilmFactors,
};

#[test]
fn a_warmed_pack_frame_builds_nothing_on_its_play_frames() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let (w, h) = (160, 96);
    let materials = [
        CpuMaterial {
            base_color: [0.02, 0.02, 0.03, 1.0],
            roughness: 0.22,
            thin_film: ThinFilmFactors {
                factor: 1.0,
                ..ThinFilmFactors::OFF
            },
            ..CpuMaterial::default()
        },
        CpuMaterial {
            base_color: [0.8, 0.55, 0.45, 1.0],
            subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
            ..CpuMaterial::default()
        },
        CpuMaterial {
            alpha_mode: ALPHA_PREMULTIPLIED,
            transmission: 1.0,
            transmission_thickness: 0.5,
            transmission_roughness: 0.45,
            roughness: 0.05,
            ..CpuMaterial::default()
        },
        CpuMaterial {
            base_color: [0.15, 0.45, 0.9, 0.45],
            alpha_mode: ALPHA_BLEND,
            ..CpuMaterial::default()
        },
    ];

    let mut frame = canonical_frame();
    frame.quality.msaa_samples = 1;
    frame.quality.subsurface_scattering = true;
    frame.quality.order_independent_transparency = true;
    frame.quality.screen_space_refraction = true;
    frame.views[0].post.enabled = true;
    let template = frame.meshes[0].clone();
    frame.meshes.clear();
    for (i, material) in materials.into_iter().enumerate() {
        let mut mesh = template.clone();
        mesh.material = Arc::new(material);
        mesh.model = (Mat4::from_translation(Vec3::new(-1.5 + i as f32, 0.0, 0.0))
            * Mat4::from_scale(Vec3::splat(0.45)))
        .to_cols_array_2d();
        mesh.object_key_xor = i as u64 + 1;
        frame.meshes.push(mesh);
    }

    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted_at(
            &device,
            &frame,
            wgpu::TextureFormat::Rgba8Unorm,
            true,
            Some((w, h)),
        )
        .expect("warm");
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, w, h);
    let pipelines = pipelines_created();
    let pool = renderer.graph_report().graph.transients.created;
    for n in 0..3 {
        renderer.render_offscreen(&device, &scene).expect("play frame");
        let film = renderer.thin_film_stats();
        assert_eq!((film.drawn, film.fallback), (1, 0), "frame {n}: {film:?}");
        let scatter = renderer.subsurface_stats();
        assert_eq!(
            (scatter.reason, scatter.drawn, scatter.fallback),
            (None, 1, 0),
            "frame {n}: {scatter:?}"
        );
        let oit = renderer.oit_stats();
        assert_eq!(
            (oit.reason, oit.drawn, oit.fallback),
            (None, 1, 0),
            "frame {n}: {oit:?}"
        );
    }
    assert_eq!(pipelines_created() - pipelines, 0, "play pipelines");
    assert_eq!(
        renderer.graph_report().graph.transients.created,
        pool,
        "play targets"
    );
}
