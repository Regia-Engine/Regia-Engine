// SPDX-License-Identifier: MIT-0
//! Paint and GPU cost of the `hud_draw2d` HUD, headless (P-06 measurement rows; nothing here is
//! asserted).
//!
//! ```text
//! cargo run --release -p hud_draw2d --example hud_draw2d_bench -- [width height frames] [--direct]
//! ```
//!
//! Each frame steps the HUD script, paints the list (`begin_frame`, the HUD, `end_frame`) and
//! records the layer's draw (`draw_list`: hand-over, tile hashing, uploads, the dirty-run and
//! composite passes). `paint_cpu_ms` is the HUD paint plus the layer's CPU recording: the
//! `hud_paint_cpu_ms_below_2` row (bar: under 2 ms on the Intel N150). `gpu_ms` is the layer's
//! GPU time between two timestamps (when the adapter has `TIMESTAMP_QUERY_INSIDE_ENCODERS`).
//! `--direct` turns dirty regions off for an A/B.

use std::time::Instant;

use hud_draw2d::{HudState, HudText, paint_hud};
use regia_quick::draw2d::{Draw2dLayer, Draw2dList};
use regia_ui::text::TextSystem;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let direct = args.iter().any(|a| a == "--direct");
    let nums: Vec<u32> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let width = nums.first().copied().unwrap_or(1920);
    let height = nums.get(1).copied().unwrap_or(1080);
    let frames = nums.get(2).copied().unwrap_or(900) as usize;
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hud_draw2d_bench: no GPU: {e}");
            std::process::exit(1);
        }
    };
    let info = device.adapter.get_info();
    println!(
        "hud_draw2d_bench: {width}x{height}, {frames} frames, dirty regions {}, adapter {} ({:?})",
        if direct { "off" } else { "on" },
        info.name,
        info.backend
    );
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hud bench target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut fonts = TextSystem::embedded_only();
    let mut text = HudText::default();
    let mut state = HudState::default();
    let mut list = Draw2dList::new();
    let mut layer = Draw2dLayer::new();
    layer.set_dirty_regions(!direct);
    layer.warm(&device, FORMAT);
    let gpu = GpuTimer::new(&device);
    let (mut paint_ms, mut record_ms, mut gpu_ms) = (Vec::new(), Vec::new(), Vec::new());
    let (mut dirty_frames, mut dirty_tiles, mut bytes, mut most) = (0u64, 0u64, 0u64, 0u32);
    let warmup = 60;
    for i in 0..frames + warmup {
        state.step();
        let t0 = Instant::now();
        list.begin_frame();
        paint_hud(&mut list, &mut fonts, &mut text, &state, width, height, 1.0);
        list.end_frame();
        let t1 = Instant::now();
        let mut encoder = device.device.create_command_encoder(&Default::default());
        if let Some(g) = &gpu {
            encoder.write_timestamp(&g.set, 0);
        }
        layer.draw_list(&device, &mut encoder, &view, FORMAT, width, height, &mut list);
        if let Some(g) = &gpu {
            encoder.write_timestamp(&g.set, 1);
            encoder.resolve_query_set(&g.set, 0..2, &g.resolve, 0);
            encoder.copy_buffer_to_buffer(&g.resolve, 0, &g.read, 0, 16);
        }
        let t2 = Instant::now();
        device.queue.submit([encoder.finish()]);
        let gpu_time = gpu.as_ref().and_then(|g| g.read_ms(&device));
        if i < warmup {
            continue;
        }
        let s = layer.gpu_stats();
        paint_ms.push(ms(t1 - t0) + ms(t2 - t1));
        record_ms.push(ms(t2 - t1));
        if let Some(t) = gpu_time {
            gpu_ms.push(t);
        }
        if s.tiles.dirty_tiles > 0 {
            dirty_frames += 1;
        }
        dirty_tiles += u64::from(s.tiles.dirty_tiles);
        bytes += s.bytes_uploaded;
        most = most.max(s.instances);
    }
    let n = frames.max(1) as f64;
    println!(
        "hud_paint_cpu_ms_below_2: paint_cpu_ms {} (bar: < 2 ms on the N150)",
        stats(&mut paint_ms)
    );
    println!("  of which layer record_ms {}", stats(&mut record_ms));
    if gpu_ms.is_empty() {
        println!("  gpu_ms: n/a (no TIMESTAMP_QUERY_INSIDE_ENCODERS)");
    } else {
        println!("  gpu_ms {}", stats(&mut gpu_ms));
    }
    println!(
        "  frames {frames}, instances max {most}, frames that redrew tiles {dirty_frames} ({:.1} %), \
         dirty tiles per frame {:.2}, bytes uploaded per frame {:.0}",
        dirty_frames as f64 * 100.0 / n,
        dirty_tiles as f64 / n,
        bytes as f64 / n
    );
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn stats(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "n/a".into();
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("median {:.3} p95 {:.3} max {:.3}", at(0.5), at(0.95), at(1.0))
}

/// Two timestamps around the layer's draw (needs `TIMESTAMP_QUERY_INSIDE_ENCODERS`).
struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    period_ns: f32,
}

impl GpuTimer {
    fn new(device: &regia_render::RenderDevice) -> Option<Self> {
        if !device
            .device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
        {
            return None;
        }
        let set = device.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("hud bench"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let buffer = |usage, label| {
            device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage,
                mapped_at_creation: false,
            })
        };
        Some(Self {
            set,
            resolve: buffer(
                wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                "hud bench resolve",
            ),
            read: buffer(
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                "hud bench read",
            ),
            period_ns: device.queue.get_timestamp_period(),
        })
    }

    fn read_ms(&self, device: &regia_render::RenderDevice) -> Option<f64> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let ticks = {
            let view = self.read.slice(..).get_mapped_range().ok()?;
            let word = |i: usize| -> Option<u64> {
                Some(u64::from_le_bytes(view.get(i * 8..i * 8 + 8)?.try_into().ok()?))
            };
            word(1)?.saturating_sub(word(0)?)
        };
        self.read.unmap();
        Some(ticks as f64 * f64::from(self.period_ns) / 1.0e6)
    }
}
