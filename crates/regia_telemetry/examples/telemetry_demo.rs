// SPDX-License-Identifier: MPL-2.0
//! Writes a sample telemetry JSONL: `cargo run -p regia_telemetry --example telemetry_demo -- out.jsonl`.
//!
//! A real headless render device (so the session header names the real adapter, features,
//! limits and fallbacks), the real plugin and the real JSON writer, driven by a synthetic frame
//! loop: ~8 ms frames with a few injected hitches, a slow asset load, a wgpu validation error,
//! surface errors and a level-end summary. Shows the format the host's tools read.

use std::time::Duration;

use regia_app::prelude::*;
use regia_telemetry::{
    JsonLayerOptions, Telemetry, TelemetryOptions, TelemetryPlugin, json_layer, request_summary,
};
use tracing_subscriber::layer::SubscriberExt;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "telemetry-demo.jsonl".into());
    let (json, guard) = json_layer(&path, JsonLayerOptions::default())?;
    let profiler = regia_core::profiling::FrameProfiler::new();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "info,regia=debug,wgpu_core=warn,wgpu_hal=warn,naga=warn",
            ))
            .with(json)
            .with(profiler.clone()),
    )?;

    let mut app = App::new();
    app.add_plugins(TelemetryPlugin {
        options: TelemetryOptions {
            summary_interval: Duration::from_secs(5),
            gpu_error_panics: false,
            warmup_fallback_frames: 200,
            ..TelemetryOptions::default()
        },
    });
    app.insert_resource(regia_render::RuntimeRenderer::default());
    app.insert_resource(regia_telemetry::SpanProfiler(profiler.clone()));
    let device = regia_render::RenderDevice::headless()?;
    app.insert_resource(device.clone());
    app.world_mut()
        .resource_mut::<regia_telemetry::TelemetryHost>()
        .set("game", "telemetry_demo");

    for frame in 0..1300u32 {
        {
            let _frame = tracing::info_span!("frame").entered();
            let work = match frame {
                400 => 40,
                700 => 120,
                1000 => 60,
                _ => 8,
            };
            let _update = tracing::info_span!("game_update").entered();
            std::thread::sleep(Duration::from_millis(work));
        }
        if frame == 500 {
            regia_core::telemetry::note(
                regia_core::telemetry::NoteKind::SlowAssetLoad,
                "texture 7f3a9c0e-demo",
                184.0,
            );
            regia_core::telemetry::add(regia_core::telemetry::Counter::SurfaceOutdated, 2);
        }
        if frame == 800 {
            let _pass = tracing::info_span!("forward_pass").entered();
            let _ = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("demo invalid buffer"),
                size: 256,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            });
        }
        if frame == 1290 {
            request_summary(app.world_mut(), "level_end");
        }
        // The profiler closes the frame's spans before telemetry reads them (as DefaultPlugins orders it).
        profiler.end_frame();
        app.update();
    }
    let counts = app.world().resource::<Telemetry>().counts();
    drop(app);
    let stats = guard.stats();
    drop(guard);
    println!("{counts:?}\n{stats:?}\nwrote {path}");
    Ok(())
}
