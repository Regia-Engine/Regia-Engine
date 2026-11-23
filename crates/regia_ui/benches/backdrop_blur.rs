// SPDX-License-Identifier: MPL-2.0
//! What the UI backdrop blur costs (ADR-1836, ADR-2129): wall time of a blurred panel on a 1920x1080
//! target against the same draw with the blur left out, per panel size and sigma, on the adapter
//! this run gets. Each frame is recorded, submitted and waited for, so the numbers are GPU work plus
//! the CPU that records it. Median per case. Prints a table; there is no budget to fail.
//!
//! `cargo bench -p regia_ui --bench backdrop_blur`. `REGIA_BLUR_BENCH_FRAMES` sets the frames per
//! case (default 60; 12 is enough on a software adapter). On a real card pin it with
//! `REGIA_GPU_UUID` and `REGIA_GPU_STRICT=1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use regia_render::RenderDevice;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

fn tint(pos: [f32; 2], size: [f32; 2]) -> UiQuad {
    UiQuad::fill(pos, size, UiColor([0.05, 0.07, 0.1, 0.45]))
}

fn frames(device: &RenderDevice, target: &OffscreenTarget, renderer: &mut UiRenderer, count: u32) -> f64 {
    // One frame to create pipelines and scratch textures: their cost is not per-frame cost.
    target.draw_ui(device, renderer);
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    // The median: a shared machine stalls some frames for reasons that are not this work.
    let mut times: Vec<f64> = (0..count)
        .map(|_| {
            let start = Instant::now();
            target.draw_ui(device, renderer);
            device
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no GPU adapter: {err}");
            return;
        }
    };
    let count = std::env::var("REGIA_BLUR_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u32);
    let info = device.adapter.get_info();
    println!(
        "adapter: {} ({:?}), {WIDTH}x{HEIGHT}, {count} frames per case",
        info.name, info.backend
    );
    let target = OffscreenTarget::new(&device, WIDTH, HEIGHT);
    let mut seed = 0x2545_f491u32;
    let frame: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|i| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let (x, y) = (i % WIDTH, i / WIDTH);
            [(x / 8) as u8, (y / 5) as u8, (seed >> 24) as u8, 255]
        })
        .collect();
    assert!(target.upload(&device, &frame));
    println!("panel       sigma  factor  passes  shaded_px  samples      base_ms  blur_ms  cost_ms");
    for (w, h) in [(320.0, 180.0), (640.0, 360.0), (1280.0, 720.0), (1920.0, 1080.0)] {
        let pos = [(WIDTH as f32 - w) * 0.5, (HEIGHT as f32 - h) * 0.5];
        let mut plain = UiRenderer::default();
        plain.set_quads(vec![tint(pos, [w, h])]);
        let base = frames(&device, &target, &mut plain, count);
        for sigma in [2.0f32, 4.0, 8.0, 16.0, 32.0, 64.0] {
            let mut renderer = UiRenderer::default();
            renderer.set_quads(vec![
                UiQuad {
                    texture: TextureRef::Backdrop,
                    blur: sigma,
                    ..UiQuad::fill(pos, [w, h], UiColor::WHITE)
                },
                tint(pos, [w, h]),
            ]);
            let blur = frames(&device, &target, &mut renderer, count);
            let stats = renderer.backdrop_stats();
            let factor = if sigma <= 4.0 {
                1
            } else if sigma <= 16.0 {
                2
            } else {
                4
            };
            println!(
                "{:>4}x{:<5}  {sigma:>5}  {factor:>6}  {:>6}  {:>9}  {:>11}  {base:>7.3}  {blur:>7.3}  {:>7.3}",
                w as u32,
                h as u32,
                stats.passes,
                stats.shaded_pixels,
                stats.samples,
                blur - base
            );
        }
    }
}
