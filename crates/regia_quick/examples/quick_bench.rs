// SPDX-License-Identifier: MPL-2.0
//! Quick-draw throughput: 10 000 mixed shapes (rectangles, rotated rectangles, circles, ellipse
//! outlines, lines, triangles) per frame at 1920×1080, timed on the GPU with timestamp queries
//! around the quick pass (MSAA layer + composite, and the unsmoothed direct path), plus the CPU
//! cost of building the batch.
//!
//! `cargo run --release -p regia_quick --example quick_bench [-- shapes frames]`
//! Acceptance (`quick-draw` lane): median GPU time ≤ 0.5 ms for 10 000 shapes.

#![allow(clippy::unwrap_used, clippy::expect_used)] // a bench binary: failures abort the run

use std::time::Instant;

use regia_quick::color::hsv;
use regia_quick::frame::{FrameParams, QuickRenderer};
use regia_quick::prelude::*;
use regia_render::{ImmediateBatch, ImmediateRenderer, ImmediateTarget, RenderDevice};

const W: u32 = 1920;
const H: u32 = 1080;

fn scene(draw: &Draw, shapes: usize, frame: usize) {
    let t = frame as f32 * 0.016;
    for i in 0..shapes {
        let f = i as f32;
        let x = ((f * 37.0) % 1800.0) - 900.0;
        let y = ((f * 53.0) % 1000.0) - 500.0;
        let color = hsv((f * 7.0) % 360.0, 0.6, 0.95);
        match i % 6 {
            0 => drop(draw.rect().x_y(x, y).w_h(12.0, 8.0).color(color)),
            1 => drop(draw.rect().x_y(x, y).w_h(14.0, 6.0).rotate(f + t).color(color)),
            2 => drop(draw.ellipse().x_y(x, y).radius(6.0).color(color)),
            3 => drop(
                draw.ellipse()
                    .x_y(x, y)
                    .radius(7.0)
                    .no_fill()
                    .stroke(color)
                    .stroke_weight(1.5),
            ),
            4 => drop(
                draw.line()
                    .start(Vec2::new(x, y))
                    .end(Vec2::new(x + 14.0 * (f + t).cos(), y + 14.0 * (f + t).sin()))
                    .weight(2.0)
                    .color(color),
            ),
            _ => drop(
                draw.tri()
                    .x_y(x, y)
                    .points(Vec2::new(-6.0, -5.0), Vec2::new(6.0, -5.0), Vec2::new(0.0, 7.0))
                    .color(color),
            ),
        }
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let shapes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10_000);
    let frames: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let device = RenderDevice::headless().expect("GPU");
    let info = device.adapter.get_info();
    let timed = device
        .device
        .features()
        .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
    let period = f64::from(device.queue.get_timestamp_period());
    let format = wgpu::TextureFormat::Bgra8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quick bench"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let queries = device.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("quick bench"),
        ty: wgpu::QueryType::Timestamp,
        count: 2,
    });
    let resolve = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quick bench resolve"),
        size: 16,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quick bench readback"),
        size: 16,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let params = FrameParams {
        size: Vec2::new(W as f32, H as f32),
        scale_factor: 1.0,
    };
    println!(
        "adapter: {} ({:?}); {shapes} shapes, {frames} frames, {W}x{H}; timestamps: {timed}",
        info.name, info.device_type
    );
    for samples in [4u32, 1] {
        let draw = Draw::new();
        let mut builder = QuickRenderer::new();
        let mut batch = ImmediateBatch::new();
        let mut renderer = ImmediateRenderer::new();
        let (mut gpu_ms, mut cpu_ms, mut record_ms) = (Vec::new(), Vec::new(), Vec::new());
        let mut last = None;
        for frame in 0..frames + 5 {
            let started = Instant::now();
            scene(&draw, shapes, frame);
            let recorded = started.elapsed().as_secs_f64() * 1000.0;
            let stats = builder.build(&draw, params, None, &mut batch);
            let cpu = started.elapsed().as_secs_f64() * 1000.0;
            if frame >= 5 {
                record_ms.push(recorded);
            }
            let mut encoder = device.device.create_command_encoder(&Default::default());
            if timed {
                encoder.write_timestamp(&queries, 0);
            }
            renderer
                .record(
                    &device,
                    &mut encoder,
                    &ImmediateTarget {
                        view: &view,
                        format,
                        width: W,
                        height: H,
                    },
                    &batch,
                    params.clip_from_local(),
                    samples,
                )
                .expect("record");
            if timed {
                encoder.write_timestamp(&queries, 1);
                encoder.resolve_query_set(&queries, 0..2, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, 16);
            }
            device.queue.submit([encoder.finish()]);
            if timed {
                let (tx, rx) = std::sync::mpsc::channel();
                readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx.send(r);
                });
                device
                    .device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
                rx.recv().expect("map").expect("map ok");
                let ticks: Vec<u64> = readback
                    .slice(..)
                    .get_mapped_range()
                    .expect("mapped")
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| u64::from_le_bytes(*b))
                    .collect();
                readback.unmap();
                if frame >= 5 {
                    gpu_ms.push(ticks[1].saturating_sub(ticks[0]) as f64 * period / 1.0e6);
                }
            } else {
                device
                    .device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
            }
            if frame >= 5 {
                cpu_ms.push(cpu);
            }
            last = Some(stats);
        }
        let stats = last.expect("ran frames");
        let gpu = if timed { median(gpu_ms) } else { f64::NAN };
        let cpu = median(cpu_ms);
        let rec = median(record_ms);
        println!(
            "samples {samples}: GPU median {gpu:.3} ms | CPU median {cpu:.3} ms (drawing calls {rec:.3} ms, tessellation + batch {:.3} ms) | {} vertices, {} triangles, {} draw calls",
            cpu - rec,
            stats.vertices,
            stats.triangles,
            stats.draw_calls
        );
        println!(
            "csv,{},{shapes},{samples},{gpu:.4},{cpu:.4},{},{}",
            info.name, stats.triangles, stats.draw_calls
        );
    }
}
