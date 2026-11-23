// SPDX-License-Identifier: MIT-0
//! `perf_overlay_shot`: the in-game performance overlay over a real DO27 map. DO27's chase
//! (`lookdev::chase`) runs through the full runtime frame with GPU timestamps on. After every
//! frame the overlay reads that renderer ([`PerfOverlayRendererSample`]) and updates through the
//! runtime UI. At the end the overlay is drawn over the chase's last frame, once Compact and once
//! Detailed.
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> cargo run --release -p lookdev --features overlay \
//!     --bin perf_overlay_shot -- [--map switchbacks|bore_twelve] [--frames 600] [--size 2560x1440] \
//!     [--out <dir>]
//! ```
//!
//! Writes `perf-overlay-real-compact.png`, `perf-overlay-real-detailed.png` and
//! `perf-overlay-real.json` (the overlay's `perf_overlay.get` stats) to `--out` (default
//! `target/perf-overlay`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use lookdev::chase::{ChaseOptions, run_with};
use lookdev::look_r2::R2Planets;
use lookdev::{LookdevError, Planet};
use regia_app::prelude::*;
use regia_framework::perf_overlay::{
    PerfOverlayPlugin, PerfOverlayRendererSample, PerfOverlayState, PerformanceOverlayMode, RendererSample,
    set_override, snapshot,
};
use regia_render::{RenderProfilePreset, RenderSettings};
use regia_ui::render::SharedUiRenderer;
use regia_ui::{OffscreenTarget, UiPlugin};

fn value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn overlay_app(device: &regia_render::RenderDevice) -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_plugins(regia_app::DiagnosticsPlugin::default())
        .add_plugins(PerfOverlayPlugin);
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // The chase renders Desktop High (its default preset).
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh))
        .insert_resource(device.clone());
    set_override(app.world_mut(), Some(PerformanceOverlayMode::Compact));
    // What the window's boot tier does for its formats (job "overlay layers"): this harness draws
    // the UI into an offscreen target, so warm that format before the overlay's warm-up ends.
    app.world()
        .resource::<SharedUiRenderer>()
        .clone()
        .lock()
        .warm(device, OffscreenTarget::FORMAT);
    app
}

fn draw(
    device: &regia_render::RenderDevice,
    app: &App,
    backdrop: &[u8],
    w: u32,
    h: u32,
) -> Result<Vec<u8>, String> {
    let target = OffscreenTarget::new(device, w, h);
    if !target.upload(device, backdrop) {
        return Err("backdrop upload failed".into());
    }
    let shared = app.world().resource::<SharedUiRenderer>().clone();
    {
        let mut guard = shared.lock();
        target.draw_ui(device, &mut guard);
    }
    target.read(device)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("perf_overlay_shot: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), LookdevError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let planet = match value(&args, "--map") {
        Some("bore_twelve") => Planet::Europa,
        _ => Planet::Mars,
    };
    let out = PathBuf::from(value(&args, "--out").unwrap_or("target/perf-overlay"));
    std::fs::create_dir_all(&out).map_err(|e| LookdevError::Config(e.to_string()))?;
    let (width, height) = value(&args, "--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((2560u32, 1440u32));
    let last_png = out.join("chase-last.png");
    let options = ChaseOptions {
        width,
        height,
        frames: value(&args, "--frames")
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
        png: Some(last_png.clone()),
        ..ChaseOptions::default()
    };
    let device = lookdev::chase::runtime_device()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let mut planets = R2Planets::default();
    let mut app = overlay_app(&device);
    // No window here: tell the UI the frame's size, as a window surface would.
    app.update();
    app.insert_resource(regia_ui::UiScales {
        window_size: [width as f32, height as f32],
        scale_factor: 1.0,
    });
    let mut median = Vec::new();
    run_with(
        &device,
        &mut planets,
        &content,
        planet,
        &options,
        &mut |_, renderer| {
            app.insert_resource(PerfOverlayRendererSample(RendererSample::of(renderer)));
            app.update();
            if let Some(t) = app.world().get_resource::<RealTime>() {
                median.push(t.delta());
            }
        },
    )?;
    // The extra updates below draw nothing new: hold the clock at the run's median frame so the
    // window and the graph keep the chase's frames.
    median.sort_unstable();
    let step = median
        .get(median.len() / 2)
        .copied()
        .unwrap_or(Duration::from_millis(16));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    let (w, h, backdrop) = lookdev::default_look::read_png(&last_png)?;
    for (mode, name) in [
        (PerformanceOverlayMode::Compact, "compact"),
        (PerformanceOverlayMode::Detailed, "detailed"),
    ] {
        set_override(app.world_mut(), Some(mode));
        for _ in 0..3 {
            app.update();
        }
        if app.world().resource::<PerfOverlayState>().shown() != mode {
            return Err(LookdevError::Config(format!(
                "overlay did not switch to {mode:?}"
            )));
        }
        let pixels = draw(&device, &app, &backdrop, w, h).map_err(LookdevError::Config)?;
        let path = out.join(format!("perf-overlay-real-{name}.png"));
        lookdev::default_look::write_png(&path, w, h, &pixels)?;
        println!("wrote {}", path.display());
    }
    let stats = snapshot(app.world());
    let json = serde_json::to_string_pretty(&stats).map_err(|e| LookdevError::Config(e.to_string()))?;
    std::fs::write(out.join("perf-overlay-real.json"), &json)
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    println!("{json}");
    Ok(())
}
