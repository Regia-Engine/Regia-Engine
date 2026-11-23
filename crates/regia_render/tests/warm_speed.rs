// SPDX-License-Identifier: MPL-2.0
//! A game-frame warm on a `PipelineWarmQueue` worker costs about what the same warm costs
//! inline, while the main thread keeps submitting frames (lane warm-speed, 2026-10-10: DO27
//! saw a worker warm take ~25x its frame-path time). Generous bound for CI: the worker warm
//! (which runs second, so any driver cache only helps it) takes at most 1.5x the inline warm
//! plus 2 s. One test in its own binary so no other test's GPU work lands in its timings.

use std::time::{Duration, Instant};

use regia_render::pipeline_warm::canonical_frame;
use regia_render::{FrameRenderer, PipelineWarmQueue, RenderDevice, RuntimeRenderer, WarmTier};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// One window-sized clear, submitted (the splash / loading screen the window presents).
fn present_clear(device: &RenderDevice, view: &wgpu::TextureView) {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("busy clear"),
        });
    regia_render::encode_clear(&mut encoder, view, [0.0, 0.0, 0.0, 1.0]);
    device.queue.submit([encoder.finish()]);
    let _ = device.device.poll(wgpu::PollType::Poll);
}

#[test]
fn worker_warm_with_a_busy_main_thread_costs_about_the_inline_warm() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU adapter: {err}"));
    let frame = canonical_frame();

    // Inline: the runtime renderer's configuration (async graphs and sky), on this thread.
    let mut inline = RuntimeRenderer::default().0;
    let start = Instant::now();
    inline
        .warm_extracted(&device, &frame, FORMAT, true)
        .expect("inline warm");
    let inline_ms = start.elapsed().as_secs_f64() * 1e3;

    // Worker: a fresh runtime renderer on the queue while this thread presents every 16 ms.
    let queue = PipelineWarmQueue::new(1);
    let warm_frame = frame.clone();
    let handle = queue.warm_renderer(
        WarmTier::Map,
        "game frame",
        RuntimeRenderer::default().0,
        move |renderer: &mut FrameRenderer, device| {
            renderer
                .warm_extracted(device, &warm_frame, FORMAT, true)
                .map_err(|e| e.to_string())
        },
    );
    let screen = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("busy main thread target"),
        size: wgpu::Extent3d {
            width: 1280,
            height: 720,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = screen.create_view(&wgpu::TextureViewDescriptor::default());
    let start = Instant::now();
    queue.start(&device);
    let deadline = start + Duration::from_secs(300);
    let mut ticks = 0u32;
    while !handle.is_done() {
        assert!(Instant::now() < deadline, "worker warm did not finish in 300 s");
        let tick = Instant::now();
        present_clear(&device, &view);
        ticks += 1;
        std::thread::sleep(Duration::from_millis(16).saturating_sub(tick.elapsed()));
    }
    let worker_wall_ms = start.elapsed().as_secs_f64() * 1e3;
    let warmed = handle.take().expect("the job hands the renderer back");
    queue.shutdown();
    assert_eq!(warmed.error, None);
    assert!(warmed.renderer.is_some(), "renderer returned");
    println!(
        "warm: inline {inline_ms:.1} ms, worker {:.1} ms ({worker_wall_ms:.1} ms wall, {ticks} main-thread frames meanwhile)",
        warmed.ms
    );
    assert!(
        f64::from(warmed.ms) <= inline_ms * 1.5 + 2000.0,
        "worker warm {:.1} ms against {inline_ms:.1} ms inline: the worker stalls",
        warmed.ms
    );
}
