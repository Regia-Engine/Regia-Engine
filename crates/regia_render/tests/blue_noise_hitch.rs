// SPDX-License-Identifier: MPL-2.0
//! The hitch gate for spatiotemporal blue noise (ADR-2075). One test in its own binary, so the
//! process-wide creation counter sees only it.
//!
//! With the knob off a frame builds no variant and the warm asks for nothing. With it on, a warmed
//! profile's play frames create no pipeline, and the warm key builds every pass's variant, so a
//! pass that first runs mid-play (a sleeping stochastic SSR waking up) compiles nothing.

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{FrameQuality, FrameRenderer, RenderDevice, RenderProfile, RenderProfilePreset};

fn desktop_high(blue_noise: bool) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.volumetric_clouds = false;
    q.volumetric_fog = false;
    q.msaa_samples = 1;
    q.blue_noise = blue_noise;
    q
}

#[test]
fn a_warmed_blue_noise_frame_creates_nothing_and_the_knob_off_asks_nothing() {
    let device = RenderDevice::headless().expect("lavapipe");
    let (w, h) = (160, 96);

    for on in [false, true] {
        let mut frame = canonical_frame();
        frame.quality = desktop_high(on);
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
        assert_eq!(renderer.blue_noise_active(), on);
        assert_eq!(pipelines_created() - pipelines, 0, "on {on}: play pipelines");
        assert_eq!(
            renderer.graph_report().graph.transients.created,
            pool,
            "on {on}: play targets"
        );
        let asked = renderer.warm_pipeline_key_for_tests(&device, "blue_noise");
        if on {
            assert!(
                asked >= 2,
                "the warm asks for the kernel's two variants at least: {asked}"
            );
            // Everything is resident now: asking again and playing again create nothing.
            let pipelines = pipelines_created();
            assert!(renderer.warm_pipeline_key_for_tests(&device, "blue_noise") >= 2);
            renderer.render_offscreen(&device, &scene).expect("play frame");
            assert_eq!(pipelines_created() - pipelines, 0, "after the warm");
        } else {
            assert_eq!(asked, 0, "the knob off asks the warm for nothing");
        }
    }
}
