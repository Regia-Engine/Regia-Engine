// SPDX-License-Identifier: MPL-2.0
//! The boot tier's warmed renderer hands its pipelines to the runtime renderer
//! (`FrameRenderer::adopt_warm_pipelines`, `docs/specs/13-renderer.md` § 2c): the adopting
//! renderer's first frame creates fewer pipelines than a cold renderer's and draws the same
//! pixels. One test in its own binary, so the process-wide pipeline counter sees only it.

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::{canonical_frame, frame_job_handoff};
use regia_render::{FrameRenderer, PipelineWarmQueue, RenderDevice};

#[test]
fn adopted_pipelines_skip_first_frame_creates_and_keep_pixels() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let frame = canonical_frame();
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, 160, 96);
    let format = wgpu::TextureFormat::Rgba8Unorm;

    // Cold: the first frame builds everything.
    let mut cold = FrameRenderer::new();
    let before = pipelines_created();
    let cold_pixels = cold.render_offscreen(&device, &scene).expect("cold frame").pixels;
    let cold_creates = pipelines_created() - before;

    // Warm on the queue's worker, then adopt.
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
    assert!(queue.take_warmed_renderer().is_none(), "handed over once");
    queue.shutdown();

    let mut runtime = FrameRenderer::new();
    assert!(
        runtime.adopt_warm_pipelines(&mut warmed),
        "a fresh renderer adopts"
    );
    assert!(
        !runtime.adopt_warm_pipelines(&mut FrameRenderer::new()),
        "a renderer that has pipelines never adopts again"
    );
    let before = pipelines_created();
    let adopted_pixels = runtime
        .render_offscreen(&device, &scene)
        .expect("adopted frame")
        .pixels;
    let adopted_creates = pipelines_created() - before;

    println!("first frame pipeline creates: cold {cold_creates}, adopted {adopted_creates}");
    assert!(cold_creates > 0, "the cold frame builds pipelines");
    assert!(
        adopted_creates * 2 < cold_creates,
        "adoption should remove most first-frame creates (cold {cold_creates}, adopted {adopted_creates})"
    );
    assert_eq!(
        cold_pixels, adopted_pixels,
        "adopted pipelines draw the same frame"
    );
}
