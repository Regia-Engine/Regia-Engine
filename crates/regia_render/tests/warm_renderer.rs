// SPDX-License-Identifier: MPL-2.0
//! A renderer moved onto a warm worker (`PipelineWarmQueue::warm_renderer` +
//! `FrameRenderer::warm_extracted`, `docs/specs/13-rendering.md` § 2c) comes back with every
//! pipeline its frame uses: its first full-size frame on the calling thread creates none and
//! draws the same pixels as a cold renderer's first frame. One test in its own binary, so the
//! process-wide pipeline counter sees only it.

use std::time::Duration;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{FrameRenderer, PipelineWarmQueue, RenderDevice, WarmTier};

#[test]
fn warmed_renderer_first_frame_creates_no_pipeline() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let frame = canonical_frame();
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, 160, 96);

    // Cold: the first frame builds everything.
    let mut cold = FrameRenderer::new();
    let before = pipelines_created();
    let cold_pixels = cold.render_offscreen(&device, &scene).expect("cold frame").pixels;
    let cold_creates = pipelines_created() - before;

    // Warm a fresh renderer on the queue's worker, in the offscreen target's format.
    let queue = PipelineWarmQueue::new(1);
    let warm_frame = frame.clone();
    let handle = queue.warm_renderer(
        WarmTier::Map,
        "game frame",
        FrameRenderer::new(),
        move |renderer, device| {
            renderer
                .warm_extracted(device, &warm_frame, wgpu::TextureFormat::Rgba8Unorm, true)
                .map_err(|e| e.to_string())
        },
    );
    assert!(!handle.is_done(), "not run before the queue starts");
    queue.start(&device);
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(300)),
        "map tier did not finish"
    );
    let warmed = handle.take().expect("the job hands the renderer back");
    assert!(handle.take().is_none(), "handed back once");
    assert_eq!(warmed.error, None);
    assert!(warmed.pipelines > 0, "the warm built the frame's pipelines");
    let mut runtime = warmed.renderer.expect("renderer returned");
    queue.shutdown();

    let before = pipelines_created();
    let warm_pixels = runtime
        .render_offscreen(&device, &scene)
        .expect("warmed frame")
        .pixels;
    let warm_creates = pipelines_created() - before;
    println!(
        "first frame pipeline creates: cold {cold_creates}, after warm {warm_creates} (warm built {})",
        warmed.pipelines
    );
    assert!(cold_creates > 0, "the cold frame builds pipelines");
    assert_eq!(
        warm_creates, 0,
        "the warmed renderer's first frame compiles nothing"
    );
    assert_eq!(
        cold_pixels, warm_pixels,
        "the warm leaves the first frame unchanged"
    );
}
