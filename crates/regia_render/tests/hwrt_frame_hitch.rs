// SPDX-License-Identifier: MPL-2.0
//! The hitch gate for ray-traced sun shadows (ADR-2035, `specs/27` § 2.4). One test in its own
//! binary, so the process-wide creation counters see only it.
//!
//! A frame of a traced profile builds what it draws. The frames after it create nothing, whatever
//! variant the adapter runs, and the game-frame warm (`prewarm_mid_play`) takes the trace's
//! pipelines and the other forward module, so the trace giving way to the shadow map mid-play
//! compiles nothing on the frame path. A profile with the knob off asks the warm for nothing.

use regia_render::hwrt::{RtShadowRequest, SunShadowVariant};
use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    RenderDevice, ThinFilmFactors, box_mesh, translate_mesh,
};

fn scene() -> LabScene {
    let (gv, gi) = box_mesh([20.0, 0.05, 20.0]);
    let (bv, bi) = box_mesh([1.0, 1.0, 1.0]);
    LabScene {
        width: 96,
        height: 72,
        eye: [0.0, 12.0, 8.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.5, -1.0, 0.25], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial::default(),
            LabMaterial {
                base_color: [0.3, 0.6, 0.9, 0.5],
                alpha_mode: ALPHA_BLEND,
                ..LabMaterial::default()
            },
            LabMaterial {
                thin_film: ThinFilmFactors {
                    factor: 1.0,
                    ..ThinFilmFactors::default()
                },
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            LabMesh {
                vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
                indices: gi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&bv, [0.0, 3.0, 0.0]),
                indices: bi.clone(),
                material: 0,
                ..LabMesh::unskinned()
            },
            // The forward families a flip of the module must find warm: a deferred blend variant
            // and a thin-film set.
            LabMesh {
                vertices: translate_mesh(&bv, [4.0, 1.0, 0.0]),
                indices: bi.clone(),
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&bv, [-4.0, 1.0, 0.0]),
                indices: bi,
                material: 2,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn frame(device: &RenderDevice, renderer: &mut FrameRenderer, lab: &LabScene, request: RtShadowRequest) {
    let mut quality = FrameQuality::LAB;
    quality.sun_shadow_trace = request;
    let scene = lab.as_frame_scene_with_quality(quality);
    renderer.render_offscreen(device, &scene).expect("frame");
}

#[test]
fn a_warmed_traced_frame_creates_nothing_and_the_knob_off_asks_nothing() {
    let device = RenderDevice::headless().expect("lavapipe");

    // What `GameFrameWarm` runs: warmed at the window size, the play frames of a traced profile
    // create no pipeline and no pooled target.
    let (w, h) = (160, 96);
    for request in [RtShadowRequest::Compute, RtShadowRequest::Screen] {
        let mut frame = canonical_frame();
        frame.quality.msaa_samples = 1;
        frame.quality.sun_shadow_trace = request;
        // The canonical frame draws no prepass; the trace starts from one.
        frame.depth_prepass = true;
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
        }
        let report = renderer.sun_trace_report();
        assert_ne!(
            report.selection.variant,
            SunShadowVariant::ShadowMap,
            "{request:?}: {:?}",
            report.selection.reason
        );
        assert_eq!(pipelines_created() - pipelines, 0, "{request:?}: play pipelines");
        assert_eq!(
            renderer.graph_report().graph.transients.created,
            pool,
            "{request:?}: play targets"
        );
    }

    let lab = scene();

    // The knob off: the warm's new keys ask for nothing and create nothing.
    let mut off = FrameRenderer::new();
    frame(&device, &mut off, &lab, RtShadowRequest::Off);
    assert_eq!(off.warm_pipeline_key_for_tests(&device, "sun_shadow_trace"), 0);
    assert_eq!(off.warm_pipeline_key_for_tests(&device, "forward_sun_rt"), 0);

    // Hardware ray queries too, where the adapter offers them: their own device, because only a
    // device that asked has them. The shared run-time gate (regia_test_utils::hwrt) decides from
    // the device's own features. Lavapipe does on Mesa 24.1 and later.
    let rq_device = regia_test_utils::hwrt::ray_query_device();
    let mut requests = vec![
        (&device, RtShadowRequest::Compute),
        (&device, RtShadowRequest::Screen),
    ];
    if let Some(rq_device) = &rq_device {
        requests.push((rq_device, RtShadowRequest::On));
    }
    for (device, request) in requests {
        let mut renderer = FrameRenderer::new();
        frame(device, &mut renderer, &lab, request);
        let variant = renderer.sun_trace_report().selection.variant;
        assert_ne!(variant, SunShadowVariant::ShadowMap, "{request:?}");

        // The frames after the first create no pipeline.
        let before = pipelines_created();
        for _ in 0..3 {
            frame(device, &mut renderer, &lab, request);
        }
        assert_eq!(pipelines_created() - before, 0, "{request:?}: steady frames");
        assert_eq!(renderer.sun_trace_report().scene_builds, 1, "{request:?}");

        // The warm takes the trace (with the way out) and the other forward module.
        assert!(
            renderer.warm_pipeline_key_for_tests(device, "sun_shadow_trace") >= 1,
            "{request:?}"
        );
        if variant.forward_key_bit() {
            assert!(
                renderer.warm_pipeline_key_for_tests(device, "forward_sun_rt") >= 1,
                "{request:?}"
            );
            // After the warm the shadow-map module is resident: the trace giving way compiles no
            // pipeline on the frame path, and neither does coming back.
            let before = pipelines_created();
            frame(device, &mut renderer, &lab, RtShadowRequest::Off);
            assert_eq!(pipelines_created() - before, 0, "{request:?}: to the shadow map");
            frame(device, &mut renderer, &lab, request);
            assert_eq!(pipelines_created() - before, 0, "{request:?}: and back");
        }
    }
}
