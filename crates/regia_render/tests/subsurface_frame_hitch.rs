// SPDX-License-Identifier: MPL-2.0
//! The hitch gate for subsurface scattering (ADR-2049, `specs/27` § 2.4). One test in its own binary,
//! so the process-wide creation counters see only it.
//!
//! A warmed scattering frame creates no pipeline and no pooled target on its play frames. A profile
//! with the knob off, or a scene without the material, asks the warm for nothing.

use std::sync::Arc;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{CpuMaterial, FrameRenderer, RenderDevice, SubsurfaceFactors, SubsurfaceProfile};

#[test]
fn a_warmed_scattering_frame_creates_nothing_and_the_knob_off_asks_nothing() {
    let device = RenderDevice::headless().expect("lavapipe");
    let (w, h) = (160, 96);
    let scattering = Arc::new(CpuMaterial {
        subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
        ..CpuMaterial::default()
    });

    // What `GameFrameWarm` runs: warmed at the window size, the play frames of a scattering
    // profile create no pipeline and no pooled target, and scatter from the first one.
    let mut frame = canonical_frame();
    frame.quality.msaa_samples = 1;
    frame.quality.subsurface_scattering = true;
    frame.views[0].post.enabled = true;
    frame.meshes[0].material = scattering.clone();
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
    for _ in 0..3 {
        renderer.render_offscreen(&device, &scene).expect("play frame");
        let stats = renderer.subsurface_stats();
        assert_eq!(stats.reason, None, "{stats:?}");
        assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    }
    assert_eq!(pipelines_created() - pipelines, 0, "play pipelines");
    assert_eq!(
        renderer.graph_report().graph.transients.created,
        pool,
        "play targets"
    );
    assert!(renderer.warm_pipeline_key_for_tests(&device, "subsurface") >= 3);
    assert_eq!(pipelines_created() - pipelines, 0, "the warm finds them built");

    // The material culled for longer than the pool keeps an idle target, then back: nothing is
    // created on the frame path, and nothing that binds the frame's targets is flushed.
    let mut away = frame.frame_scene(view_index, w, h);
    away.target = [away.eye[0], away.eye[1] + 1.0, away.eye[2]];
    for _ in 0..12 {
        renderer.render_offscreen(&device, &away).expect("culled frame");
        assert_eq!(renderer.subsurface_stats().reason, Some("no_visible_draw"));
    }
    renderer.render_offscreen(&device, &scene).expect("back in view");
    assert_eq!(renderer.subsurface_stats().drawn, 1);
    assert_eq!(pipelines_created() - pipelines, 0, "back in view: pipelines");
    assert_eq!(
        renderer.graph_report().graph.transients.created,
        pool,
        "back in view: targets"
    );

    // Switched off, the warm stops asking, on the same renderer.
    let mut switched = frame.frame_scene(view_index, w, h);
    switched.quality.subsurface_scattering = false;
    renderer
        .render_offscreen(&device, &switched)
        .expect("switched off");
    assert_eq!(renderer.subsurface_stats().reason, Some("profile_off"));
    assert_eq!(renderer.warm_pipeline_key_for_tests(&device, "subsurface"), 0);

    // The knob off, the material on: the warm's key asks for nothing, and no frame builds anything.
    let mut off = canonical_frame();
    off.quality.msaa_samples = 1;
    off.views[0].post.enabled = true;
    off.meshes[0].material = scattering;
    let mut renderer = FrameRenderer::new();
    let (view_index, _) = off.window_views()[0];
    let scene = off.frame_scene(view_index, w, h);
    renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(renderer.warm_pipeline_key_for_tests(&device, "subsurface"), 0);
    assert_eq!(renderer.subsurface_stats().reason, Some("profile_off"));
    let before = pipelines_created();
    renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(pipelines_created() - before, 0, "knob off: steady frame");

    // The knob on, no material: nothing either.
    let mut plain = canonical_frame();
    plain.quality.msaa_samples = 1;
    plain.quality.subsurface_scattering = true;
    plain.views[0].post.enabled = true;
    let mut renderer = FrameRenderer::new();
    let (view_index, _) = plain.window_views()[0];
    let scene = plain.frame_scene(view_index, w, h);
    renderer.render_offscreen(&device, &scene).expect("frame");
    assert_eq!(renderer.warm_pipeline_key_for_tests(&device, "subsurface"), 0);
    assert_eq!(renderer.subsurface_stats().reason, Some("no_material"));
}
