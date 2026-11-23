// SPDX-License-Identifier: MPL-2.0
//! A renderer that adopted the boot tier's warmed pipelines (`FrameRenderer::adopt_warm_pipelines`)
//! then draws the mobile tiers: SMAA plus the SGSR upscale (Mobile High), and SGSR alone
//! (Mobile Low). The adopted post chain once lacked the SGSR shader module and panicked on the
//! first scaled frame, which took down the Pixel 6 the moment a game ran Android's default
//! profile after the warm handoff.

use regia_render::pipeline_warm::{canonical_frame, frame_job_handoff};
use regia_render::{
    FrameQuality, FrameRenderer, PipelineWarmQueue, RenderDevice, RenderProfile, RenderProfilePreset,
};

#[test]
fn adopted_renderer_draws_the_mobile_tiers() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let frame = canonical_frame();
    let (view_index, _) = frame.window_views()[0];
    let format = wgpu::TextureFormat::Rgba8Unorm;

    let queue = PipelineWarmQueue::new(1);
    queue.push(
        regia_render::WarmTier::Boot,
        "canonical frame",
        frame_job_handoff(frame.clone(), format, queue.clone()),
    );
    queue.start(&device);
    assert!(
        queue.wait_tier(regia_render::WarmTier::Boot, std::time::Duration::from_secs(300)),
        "boot tier did not finish"
    );
    let mut warmed = queue.take_warmed_renderer().expect("handoff renderer");
    queue.shutdown();
    let mut runtime = FrameRenderer::new();
    assert!(
        runtime.adopt_warm_pipelines(&mut warmed),
        "a fresh renderer adopts"
    );

    for preset in [RenderProfilePreset::MobileHigh, RenderProfilePreset::MobileLow] {
        let mut scene = frame.frame_scene(view_index, 160, 96);
        scene.quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
        let image = runtime
            .render_offscreen(&device, &scene)
            .unwrap_or_else(|err| panic!("{preset:?} frame after adoption: {err}"));
        assert_eq!(image.pixels.len(), 160 * 96 * 4, "{preset:?}");
        assert!(
            image.pixels.chunks(4).any(|p| p[..3] != [0, 0, 0]),
            "{preset:?} frame is black"
        );
    }
}
