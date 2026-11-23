// SPDX-License-Identifier: MPL-2.0
//! What isolated group opacity and rounded clips cost (ADR-2866): wall time of a panel of 24
//! overlapping children drawn through a group layer (opacity 0.5), through a rounded-clip layer
//! (mask pass), and non-isolated (per-primitive opacity, no layer), per panel size, on a 1920x1080
//! target. Each frame is recorded, submitted and waited for, so the numbers are GPU work plus the
//! CPU that records it. Median per case. Prints a table; there is no budget to fail.
//!
//! `cargo bench -p regia_ui --bench group_layers`. `REGIA_LAYER_BENCH_FRAMES` sets the frames per
//! case (default 60; 12 is enough on a software adapter). On a real card pin it with
//! `REGIA_GPU_UUID` and `REGIA_GPU_STRICT=1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use regia_render::RenderDevice;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

fn frames(device: &RenderDevice, target: &OffscreenTarget, renderer: &mut UiRenderer, count: u32) -> f64 {
    // One frame to create pipelines and pooled layers: their cost is not per-frame cost.
    target.draw_ui(device, renderer);
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
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

/// 24 children in a 6 x 4 grid, each overlapping its neighbours, at `origin`.
fn children(origin: [f32; 2], size: [f32; 2], alpha: f32) -> Vec<UiQuad> {
    let (cw, ch) = (size[0] / 5.0, size[1] / 3.0);
    (0..24)
        .map(|i| {
            let (col, row) = ((i % 6) as f32, (i / 6) as f32);
            UiQuad {
                alpha,
                corner_radius: [6.0; 4],
                ..UiQuad::fill(
                    [origin[0] + col * cw * 0.8, origin[1] + row * ch * 0.66],
                    [cw, ch],
                    UiColor([0.2, 0.5, 0.9, 1.0]),
                )
            }
        })
        .collect()
}

/// The quads of one layer at `pos` (screen) of `size`: begin, the children in layer space, an
/// optional rounded mask, and the composite at `opacity`.
fn layer(pos: [f32; 2], size: [f32; 2], opacity: f32, rounded: bool) -> Vec<UiQuad> {
    let mut out = vec![UiQuad {
        texture: TextureRef::LayerBegin,
        ..UiQuad::fill(pos, size, UiColor::TRANSPARENT)
    }];
    out.extend(children([0.0, 0.0], size, 1.0));
    if rounded {
        out.push(UiQuad {
            texture: TextureRef::LayerMask {
                image: None,
                tile: false,
            },
            corner_radius: [24.0; 4],
            border_width: [0.0, 0.0, size[0], size[1]],
            blur: 1.0,
            ..UiQuad::fill([0.0, 0.0], size, UiColor::WHITE)
        });
    }
    out.push(UiQuad {
        texture: TextureRef::LayerComposite,
        alpha: opacity,
        clip: [pos[0], pos[1], size[0], size[1]],
        ..UiQuad::fill(pos, size, UiColor::WHITE)
    });
    out
}

fn main() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no GPU adapter: {err}");
            return;
        }
    };
    let count = std::env::var("REGIA_LAYER_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60u32);
    let info = device.adapter.get_info();
    println!(
        "adapter: {} ({:?}), {WIDTH}x{HEIGHT}, {count} frames per case",
        info.name, info.backend
    );
    let target = OffscreenTarget::new(&device, WIDTH, HEIGHT);
    target.clear(&device, [0.1, 0.1, 0.1, 1.0]);
    println!("panel        plain_ms  group_ms  rounded_ms  group_cost  rounded_cost  pool_bytes");
    for (w, h) in [(320.0, 180.0), (640.0, 360.0), (1280.0, 720.0), (1920.0, 1080.0)] {
        let pos = [
            ((WIDTH as f32 - w) * 0.5).floor(),
            ((HEIGHT as f32 - h) * 0.5).floor(),
        ];
        let mut plain = UiRenderer::default();
        plain.set_quads(children(pos, [w, h], 0.5));
        let base = frames(&device, &target, &mut plain, count);
        let mut group = UiRenderer::default();
        group.set_quads(layer(pos, [w, h], 0.5, false));
        let grouped = frames(&device, &target, &mut group, count);
        let stats = group.layer_stats();
        assert_eq!(
            (stats.drawn, stats.created_total),
            (1, 1),
            "one pooled layer: {stats:?}"
        );
        let mut clip = UiRenderer::default();
        clip.set_quads(layer(pos, [w, h], 1.0, true));
        let clipped = frames(&device, &target, &mut clip, count);
        println!(
            "{:>4}x{:<5}  {base:>8.3}  {grouped:>8.3}  {clipped:>10.3}  {:>10.3}  {:>12.3}  {:>10}",
            w as u32,
            h as u32,
            grouped - base,
            clipped - base,
            stats.pool_bytes
        );
    }
}
