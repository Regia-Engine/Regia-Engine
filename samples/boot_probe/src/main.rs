// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `boot_probe`.
//!
//! `boot_probe --exit` prints `BOOT_PROBE {json}` (the boot timeline and the warm-up report) once
//! the splash is gone and the scene has drawn, and exits; `--no-splash` measures the path without a splash;
//! `--timeout-s N` gives up after N seconds (default 60); `--game` draws the DO27-like frame
//! configuration (Desktop Ultra, sky, clouds, volumetric fog); `--profile high|low|deck` picks
//! another render profile for it; `--quality-probe` also runs the first-launch quality probe
//! behind the scene and reports it (ADR-173); `--fullscreen` opens the window borderless
//! fullscreen (the quit then leaves fullscreen first, ADR-188); `--late-scene-s S [--map-mb MB]` is
//! DO27's order (splash finished with no scene, a UI-only window, a `MB`-sized map spawned at `S`
//! seconds; default 1024 MB) and reports the main-loop intervals after the spawn (lane v018-stalls);
//! `--warm-threads N` runs the shader warm queue with N workers (default: the Boot budget) until
//! the first game frame, for A/B timing.

use std::process::ExitCode;
use std::time::Duration;

use boot_probe::BootProbe;
use regia::prelude::*;

fn main() -> ExitCode {
    // The first boot mark: process age is read from the OS, so this only pins the clock early.
    regia_render::boot::mark_named("main");
    let args: Vec<String> = std::env::args().collect();
    let timeout = args
        .iter()
        .position(|a| a == "--timeout-s")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let probe = BootProbe {
        exit: args.iter().any(|a| a == "--exit"),
        splash: !args.iter().any(|a| a == "--no-splash"),
        timeout: Duration::from_secs(timeout),
        game: args.iter().any(|a| a == "--game"),
        quality_probe: args.iter().any(|a| a == "--quality-probe"),
        late_scene: args
            .iter()
            .position(|a| a == "--late-scene-s")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f32>().ok())
            .map(Duration::from_secs_f32),
        map_mb: args
            .iter()
            .position(|a| a == "--map-mb")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1024),
        profile: match args
            .iter()
            .position(|a| a == "--profile")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
        {
            Some("high") => regia_render::RenderProfilePreset::DesktopHigh,
            Some("low") => regia_render::RenderProfilePreset::DesktopLow,
            Some("deck") => regia_render::RenderProfilePreset::Deck,
            _ => regia_render::RenderProfilePreset::DesktopUltra,
        },
    };
    let fullscreen = args.iter().any(|a| a == "--fullscreen");
    let warm_threads = args
        .iter()
        .position(|a| a == "--warm-threads")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok());
    let exit = regia::run_with(probe, |app| {
        if let Some(n) = warm_threads {
            app.insert_resource(regia_render::PipelineWarmQueue::new(n));
        }
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - boot_probe".into(),
                mode: if fullscreen {
                    WindowMode::BorderlessFullscreen
                } else {
                    WindowMode::Windowed
                },
                ..Default::default()
            },
        });
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
