// SPDX-License-Identifier: MIT-0
//! UI cost bench for the dense DO27-style HUD (lane ui-look budget: UI CPU + GPU ≤ 0.3 ms).
//!
//! ```text
//! cargo run --release -p ui_gallery --example hud_bench -- [width height frames]
//! ```
//!
//! Scenarios: `empty` (UiPlugin, no tree: the per-frame engine floor), `static` (nothing changes; the steady state of a HUD between events) and `live`
//! (a resource bar's width animates every frame and the timer text changes every second, so
//! layout, shaping and extract all re-run). Reports medians and p95 of: the whole app update
//! (every UI system: layout, text, interaction, extract), the overlay's CPU draw recording
//! (batching + upload), and the overlay's GPU time (timestamp queries around the UI pass).

use std::time::Instant;

use regia_ui::render::SharedUiRenderer;
use regia_ui::{OffscreenTarget, UiPlugin};
use ui_gallery::hud::{HudClock, HudSample};
use ui_gallery::shots::{headless_app, resize};

fn main() {
    // `HUD_BENCH_SPANS=1`: also report where UI CPU time goes, per tracing span (adds overhead;
    // keep it off for the headline numbers).
    let spans = std::env::var_os("HUD_BENCH_SPANS").is_some();
    if spans {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(SpanTimes);
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let width = args.first().and_then(|a| a.parse().ok()).unwrap_or(1920u32);
    let height = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(1080u32);
    let frames = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(600usize);
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hud_bench: no GPU: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "adapter: {} ({:?}), {width}x{height}, {frames} frames",
        device.adapter.get_info().name,
        device.adapter.get_info().backend
    );
    // `empty`: the same app with no UI tree — the schedule/engine floor every frame pays, so
    // the HUD's own cost is the difference.
    for scenario in ["empty", "static", "live"] {
        let live = scenario == "live";
        let mut app = if scenario == "empty" {
            headless_app(UiPlugin::code_only())
        } else {
            headless_app(HudSample)
        };
        assert!(app.has_plugin(std::any::type_name::<UiPlugin>()));
        if let Some(mut clock) = app.world_mut().get_resource_mut::<HudClock>() {
            clock.live = live;
        }
        resize(&mut app, width, height, None);
        let target = OffscreenTarget::new(&device, width, height);
        let shared = app.world().resource::<SharedUiRenderer>().clone();
        let gpu = GpuTimer::new(&device);
        let (mut update_ms, mut draw_ms, mut gpu_ms) = (Vec::new(), Vec::new(), Vec::new());
        let (mut quads, mut batches) = (0, 0);
        for i in 0..frames + 60 {
            let t0 = Instant::now();
            app.update();
            let t1 = Instant::now();
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("bench") });
            if let Some(g) = &gpu {
                encoder.write_timestamp(&g.set, 0);
            }
            {
                let mut r = shared.lock();
                quads = r.quads().len();
                r.draw(
                    &device,
                    &mut encoder,
                    target.view(),
                    OffscreenTarget::FORMAT,
                    width,
                    height,
                );
                batches = r.batch_count();
            }
            if let Some(g) = &gpu {
                encoder.write_timestamp(&g.set, 1);
                encoder.resolve_query_set(&g.set, 0..2, &g.resolve, 0);
                encoder.copy_buffer_to_buffer(&g.resolve, 0, &g.read, 0, 16);
            }
            let t2 = Instant::now();
            device.queue.submit([encoder.finish()]);
            let gpu_time = gpu.as_ref().and_then(|g| g.read_ms(&device));
            if i >= 60 {
                update_ms.push(ms(t1 - t0));
                draw_ms.push(ms(t2 - t1));
                if let Some(t) = gpu_time {
                    gpu_ms.push(t);
                }
            }
        }
        let name = scenario;
        if spans {
            let mut totals: Vec<(&'static str, f64)> = span_totals().drain().collect();
            totals.sort_by(|a, b| b.1.total_cmp(&a.1));
            let per_frame: Vec<String> = totals
                .iter()
                .map(|(n, t)| format!("{n} {:.3}", t / (frames + 60) as f64))
                .collect();
            println!("{name:<6} ms/frame by span: {}", per_frame.join(", "));
        }
        println!(
            "{name:<6} quads {quads:>5} draws {batches:>3} | update {} | draw-record {} | gpu {} | cpu+gpu median {:.3} ms",
            stats(&mut update_ms),
            stats(&mut draw_ms),
            if gpu_ms.is_empty() {
                "n/a".to_owned()
            } else {
                stats(&mut gpu_ms)
            },
            median(&mut update_ms)
                + median(&mut draw_ms)
                + if gpu_ms.is_empty() {
                    0.0
                } else {
                    median(&mut gpu_ms)
                },
        );
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

fn stats(v: &mut [f64]) -> String {
    v.sort_by(f64::total_cmp);
    let p = |q: f64| {
        v.get(((v.len() as f64 - 1.0) * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    format!("med {:.3} p95 {:.3} ms", p(0.5), p(0.95))
}

/// Two timestamps around the UI pass (needs `TIMESTAMP_QUERY_INSIDE_ENCODERS`).
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
            label: Some("ui bench"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let resolve = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui bench resolve"),
            size: 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui bench read"),
            size: 16,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Some(Self {
            set,
            resolve,
            read,
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

fn span_totals() -> std::sync::MutexGuard<'static, std::collections::HashMap<&'static str, f64>> {
    static TOTALS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, f64>>> =
        std::sync::OnceLock::new();
    let m = TOTALS.get_or_init(Default::default);
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Accumulates wall time per span name (UI spans are entered once per call).
struct SpanTimes;

struct Entered(Instant);

impl<S> tracing_subscriber::Layer<S> for SpanTimes
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Entered(Instant::now()));
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(Entered(t)) = span.extensions_mut().remove::<Entered>()
        {
            *span_totals().entry(span.name()).or_insert(0.0) += ms(t.elapsed());
        }
    }
}
