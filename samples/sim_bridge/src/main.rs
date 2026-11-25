// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `sim_bridge`.
//!
//! Supports `--exit-after-frames N` for automated smoke runs (same flag as `hello_window`)
//! and `--bench-frames N` for unthrottled frame-time measurement (see [`run_bench`]).
//! `--presentation-physics` adds the presentation-physics demo (kinematic hulls on the discs,
//! the terrain with a hole, debris bursts; `sim_bridge::presentation_physics`).

use std::process::ExitCode;
use std::time::Instant;

use regia::prelude::*;
use sim_bridge::{ExitAfterFrames, SimBridgeSample};

fn flag(args: &[String], name: &str) -> Result<Option<u64>, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => match args.get(i + 1).map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => Ok(Some(n)),
            _ => Err(format!("{name} needs a number")),
        },
    }
}

/// Unthrottled frame-cost measurement for the soak lane: runs the sim + the ECS render
/// path offscreen at 1280×720 (the soak window size) into a persistent target, timing
/// `app.update()` (sim + extraction) + the full pass sequence + a device wait per frame.
///
/// Why not just read the soak's numbers: a windowed soak on this rig opens its window on
/// a hidden Hyprland workspace, Wayland delivers no frame callbacks there, and the engine
/// throttles hidden surfaces to ~10 Hz by design (`specs/11` § 2) — so the soak's
/// `frame_p99_ms` measures the throttle, not the frame cost. This bench measures the cost.
fn run_bench(frames: u64) -> Result<(), String> {
    use regia_audio::prelude::*;
    use regia_framework::settings::SettingsPath;
    use regia_render::{ExtractedFrame, FrameRenderer, RenderDevice};

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const WARMUP: u32 = 30;
    /// Texture-target camera id for the bench view (any stable id works;
    /// [`FrameRenderer::render_into_texture`] keeps one persistent target per id).
    const BENCH_TARGET: u64 = 0xFFFF_FFFF;

    let device = RenderDevice::headless().map_err(|e| format!("GPU: {e}"))?;

    // Same plugin set as the test harness (headless render plugin: extraction runs,
    // no window surface is created), so the bench exercises the runtime path games run.
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        SimBridgeSample,
    ));
    // Fixed 16 ms sim steps: the sim advances deterministically while the wall clock
    // times only the work, not any real-time waits.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(16),
    ));

    let mut renderer = FrameRenderer::new();
    renderer.set_gpu_timing(&device, true);

    let render = |renderer: &mut FrameRenderer, app: &mut App| {
        let frame = app
            .world()
            .resource::<ExtractedFrame>()
            .frame_scene(0, WIDTH, HEIGHT);
        renderer.render_into_texture(&device, BENCH_TARGET, &frame)
    };

    // Warm-up: pipeline compilation, mesh/texture uploads, post-chain setup.
    for _ in 0..WARMUP {
        app.update();
        render(&mut renderer, &mut app).map_err(|e| format!("warm-up render: {e}"))?;
        device.wait_idle().map_err(|e| format!("warm-up poll: {e}"))?;
    }

    let mut times = Vec::with_capacity(frames as usize);
    for index in 0..frames {
        let start = Instant::now();
        let _extract = tracing::info_span!("extract").entered();
        app.update();
        drop(_extract);
        render(&mut renderer, &mut app).map_err(|e| format!("render: {e}"))?;
        device.wait_idle().map_err(|e| format!("poll: {e}"))?;
        let wall = start.elapsed().as_secs_f64() * 1000.0;
        let cpu = renderer.cpu_timings();
        let gpu = renderer.gpu_timings();
        println!(
            "frame {index}: wall {wall:.2} ms  cpu encode {:.2} submit {:.2} readback {:.2} present {:.2} total {:.2}  gpu scene {:.2} (gpu timestamps lag 2 frames)",
            cpu.encode_ms, cpu.submit_ms, cpu.readback_ms, cpu.present_ms, cpu.total_ms, gpu.scene_ms,
        );
        times.push(wall);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = |q: f64| times[((q * frames as f64) as usize).min(times.len() - 1)];
    println!(
        "sim_bridge 1280x720 runtime path: {frames} frames, p50 {:.2} ms ({:.0} fps), p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms, mean {:.2} ms",
        p(0.50),
        1000.0 / p(0.50),
        p(0.95),
        p(0.99),
        times[times.len() - 1],
        times.iter().sum::<f64>() / frames as f64,
    );
    println!("GPU: {}", device.capabilities.adapter_name);
    let gpu = renderer.gpu_timings();
    println!(
        "gpu passes (last frame): shadows {:.2} ms, clusters {:.2} ms, prepass {:.2} ms, cull {:.2} ms, ssao {:.2} ms, forward {:.2} ms, taau {:.2} ms, scene total {:.2} ms",
        gpu.shadows_ms,
        gpu.clusters_ms,
        gpu.prepass_ms,
        gpu.cull_ms,
        gpu.ssao_ms,
        gpu.forward_ms,
        gpu.taau_ms,
        gpu.scene_ms
    );
    let (meshes, materials, objects) = renderer.cache_sizes();
    println!("gpu cache: {meshes} meshes, {materials} materials, {objects} objects");
    let drawn = app.world().resource::<ExtractedFrame>().meshes.len();
    println!("drawn meshes per frame: {drawn}");
    Ok(())
}

/// Project directory of this sample in a dev build, resolved at runtime (ADR-10370).
///
/// Release builds pass `None` and do not call this. `cargo run` exports
/// `CARGO_MANIFEST_DIR`. The compile-time `env!("CARGO_MANIFEST_DIR")` is not remapped.
#[cfg(debug_assertions)]
fn sample_project_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("sim_bridge");
    project.is_dir().then_some(project)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (exit_after, bench_frames) = match (flag(&args, "--exit-after-frames"), flag(&args, "--bench-frames"))
    {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("sim_bridge: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(frames) = bench_frames {
        return match run_bench(frames) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("sim_bridge bench: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let presentation_physics = args.iter().any(|a| a == "--presentation-physics");
    let exit = regia::run_with(SimBridgeSample, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - sim_bridge".into(),
                ..Default::default()
            },
        });
        // Shipping builds must not embed the build machine's source path. Dev builds still point
        // the agent at this crate so a local session can see the project.
        app.insert_resource(regia::agent::AgentSettings {
            #[cfg(debug_assertions)]
            project_dir: sample_project_dir(),
            #[cfg(not(debug_assertions))]
            project_dir: None,
        });
        if let Some(n) = exit_after {
            app.insert_resource(ExitAfterFrames(n));
        }
        if presentation_physics {
            app.add_plugins(sim_bridge::presentation_physics::PresentationPhysicsDemo);
        }
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
