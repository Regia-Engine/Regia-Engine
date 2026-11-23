// SPDX-License-Identifier: MPL-2.0
//! Regression through the real renderer, shared feedback sampler and telemetry histograms.
use super::*;

#[test]
fn render_once_then_update_only_ticks_aggregate_once() {
    let _serial = super::tests::SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let scene = empty_scene();
    renderer
        .render_offscreen(&device, &scene)
        .expect("one offscreen render");
    assert!(renderer.cpu_timings().total_ms > 0.0, "a real CPU publication");
    assert_eq!(renderer.cpu_timings().frame_id, Some(renderer.frame_index()));
    assert_ne!(renderer.cpu_timings().sample_id, 0);
    if renderer.gpu_timings().frame_ms > 0.0 {
        assert_eq!(renderer.gpu_timings().frame_id, Some(renderer.frame_index()));
        assert_ne!(renderer.gpu_timings().sample_id, 0);
        assert_ne!(renderer.gpu_timings().sample_id, renderer.cpu_timings().sample_id);
    }
    let has_gpu_sample = renderer.gpu_timings().frame_ms > 0.0;
    if device.capabilities.timestamp_query {
        assert!(has_gpu_sample, "a supported timed capture must publish GPU data");
    }
    let mut t = Telemetry::new(TelemetryOptions::default());
    t.observe_frame(110.0, 0.11, Some(&renderer), None);
    assert_eq!(t.cpu_hist.count(), 1);
    for tick in 2..=9 {
        t.observe_frame(110.0, f64::from(tick) * 0.11, Some(&renderer), None);
    }
    assert_eq!(
        t.cpu_hist.count(),
        1,
        "eight update-only ticks must not repeat the renderer's CPU sample"
    );
    assert_eq!(
        t.gpu_hist.count(),
        u32::from(has_gpu_sample),
        "a GPU publication counts once too"
    );
    assert_eq!(
        t.live().bound,
        FrameBound::Unknown,
        "a cached sample cannot imply CPU saturation"
    );
    assert_eq!(t.live().cpu_ms, 0.0);
    assert_eq!(t.live().gpu_ms, 0.0);
    assert_eq!(t.pass_frames, u64::from(has_gpu_sample));
    assert_eq!(t.draw_frames, 1);
    t.request_summary("reset_stale_window");
    t.observe_frame(110.0, 1.1, Some(&renderer), None);
    t.observe_frame(110.0, 1.21, Some(&renderer), None);
    assert_eq!(
        t.cpu_hist.count(),
        0,
        "summary reset does not renew cached timings"
    );
    assert_eq!(t.pass_frames, 0);
    let old_cpu_id = renderer.cpu_timings().sample_id;
    renderer
        .render_offscreen(&device, &scene)
        .expect("resumed render");
    assert!(renderer.cpu_timings().sample_id > old_cpu_id);
    t.observe_frame(110.0, 1.32, Some(&renderer), None);
    assert_eq!(
        t.cpu_hist.count(),
        1,
        "a resumed render contributes its new sample"
    );
    assert_eq!(
        t.live().timing.expect("timing").cpu_frame_id,
        Some(renderer.frame_index())
    );
}

fn empty_scene() -> regia_render::FrameScene<'static> {
    regia_render::FrameScene {
        width: 8,
        height: 8,
        eye: [0.0, 0.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 1.0,
        double_sided: false,
        post: None,
        fog: Default::default(),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: &[],
        quality: Default::default(),
        meshes: Vec::new(),
    }
}

#[test]
fn asynchronous_gpu_publication_retains_the_submitted_frame() {
    let _serial = super::tests::SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
    if !device.capabilities.timestamp_query {
        eprintln!("GPU source-frame test unavailable: adapter lacks timestamp queries");
        return;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);
    let scene = empty_scene();
    for frame in 1..=3 {
        let target = renderer
            .begin_offscreen_live(&device, &scene, wgpu::TextureFormat::Rgba8Unorm)
            .expect("asynchronous frame");
        device
            .wait_idle()
            .expect("complete callbacks without a timing sleep");
        assert_eq!(renderer.cpu_timings().frame_id, Some(frame));
        if frame < 3 {
            assert_eq!(
                renderer.gpu_timings().frame_id,
                None,
                "two-render-frame latency remains intact"
            );
        }
        drop(target);
    }
    assert_eq!(
        renderer.gpu_timings().frame_id,
        Some(1),
        "readback at render 3 must still name render 1"
    );
    let mut sampler = FrameSampler::default();
    let first = sampler.sample_at(0.1, 100.0, Some(&renderer));
    let ids = first.timing.expect("timing identities");
    assert_eq!(ids.cpu_frame_id, Some(3));
    assert_eq!(ids.gpu_frame_id, Some(1));
    assert!(ids.cpu_fresh && ids.gpu_fresh);
    let next = sampler.sample_at(0.2, 100.0, Some(&renderer));
    assert_eq!(next.cpu_ms, 0.0);
    assert_eq!(next.gpu_ms, 0.0);
    assert!(!next.timing.expect("timing").gpu_fresh);
}
