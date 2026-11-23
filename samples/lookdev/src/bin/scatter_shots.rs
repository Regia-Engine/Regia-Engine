// SPDX-License-Identifier: MIT-0
//! `scatter_shots`: rock-scatter evidence on the runtime frame path (`lookdev::scatter_shots`).
//!
//! ```text
//! scatter_shots --out <dir> [--width 960] [--height 540] [--frames 12] [--res 2049]
//! scatter_shots --bench [--width 2560] [--height 1440] [--res 2049] [--density 1.0]
//! ```
//!
//! Shots write `<shot>_on.png` (scatter + contact), `<shot>_off.png` (no scatter) and
//! `<shot>_no_contact.png` (scatter, no contact pool) plus `scatter_shots.json`. `--bench`
//! renders the wide Mars view with GPU timestamps, scatter on and off, and prints the scene
//! GPU time difference per frame.

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::default_look::write_png;
use lookdev::scatter_shots::{ScatterOptions, ScatterShot, ScatterStage, render_rock_lineup};
use lookdev::{LookdevConfig, LookdevError, Planet};

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("lookdev=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("scatter_shots: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, LookdevError> {
    args.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| LookdevError::Config(format!("{flag} needs a value")))
}

fn number<T: std::str::FromStr>(args: &[String], i: usize, flag: &str) -> Result<T, LookdevError> {
    value(args, i, flag)?
        .parse()
        .map_err(|_| LookdevError::Config(format!("{flag}: not a number")))
}

fn config(res: u32) -> LookdevConfig {
    LookdevConfig {
        height_resolution: res,
        macro_resolution: 64,
        layer_height_resolution: 64,
        ..LookdevConfig::baseline()
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let mut out: Option<PathBuf> = None;
    let mut bench = false;
    let mut options = ScatterOptions::evidence();
    let mut res = 2049u32;
    let mut density = 1.0f32;
    let mut bench_frames = 24u32;
    let mut rocks_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out")?)),
            "--width" => options.width = number(args, i, "--width")?,
            "--height" => options.height = number(args, i, "--height")?,
            "--frames" => options.frames = number(args, i, "--frames")?,
            "--res" => res = number(args, i, "--res")?,
            "--density" => density = number(args, i, "--density")?,
            "--bench-frames" => bench_frames = number(args, i, "--bench-frames")?,
            "--rocks-only" => {
                rocks_only = true;
                i += 1;
                continue;
            }
            "--bench" => {
                bench = true;
                i += 1;
                continue;
            }
            other => return Err(LookdevError::Config(format!("unknown argument {other}"))),
        }
        i += 2;
    }
    let device = regia_render::RenderDevice::headless()?;
    println!("adapter: {}", device.adapter.get_info().name);
    if bench {
        return run_bench(&device, &config(res), options, density, bench_frames);
    }
    let out = out.ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?;
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let mut rows = Vec::new();
    for planet in [Planet::Europa, Planet::Mars] {
        let lineup = render_rock_lineup(&device, planet, options.width, options.height)?;
        let path = out.join(format!("rocks_{}.png", planet.name()));
        write_png(&path, options.width, options.height, &lineup)?;
        println!("rock line-up -> {}", path.display());
        if rocks_only {
            continue;
        }
        let started = std::time::Instant::now();
        let mut stage = ScatterStage::with_density(&device, planet, &config(res), density)?;
        println!(
            "{}: generated in {:.1} s ({} instances resident before the first view)",
            planet.name(),
            started.elapsed().as_secs_f32(),
            stage.field.len()
        );
        for shot in ScatterShot::ALL.into_iter().filter(|s| s.planet() == planet) {
            for (suffix, scatter, contact) in [
                ("on", true, true),
                ("off", false, false),
                ("no_contact", true, false),
            ] {
                let capture = stage.render(
                    &device,
                    shot,
                    ScatterOptions {
                        scatter,
                        contact,
                        ..options
                    },
                )?;
                let path = out.join(format!("{}_{suffix}.png", shot.name()));
                write_png(&path, options.width, options.height, &capture.pixels)?;
                println!(
                    "{} {suffix}: resident {}, emitted {} (lod {:?}, fading {}), in frustum {} -> {}",
                    shot.name(),
                    capture.stats.resident,
                    capture.stats.emitted,
                    capture.stats.per_lod,
                    capture.stats.fading,
                    capture.drawn,
                    path.display()
                );
                rows.push(format!(
                    "{{\"shot\":\"{}\",\"variant\":\"{suffix}\",\"emitted\":{},\"in_frustum\":{},\"resident\":{}}}",
                    shot.name(),
                    capture.stats.emitted,
                    capture.drawn,
                    capture.stats.resident
                ));
            }
        }
    }
    let json = format!("[\n  {}\n]\n", rows.join(",\n  "));
    let path = out.join("scatter_shots.json");
    std::fs::write(&path, json).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    println!("wrote {}", path.display());
    Ok(())
}

fn run_bench(
    device: &regia_render::RenderDevice,
    config: &LookdevConfig,
    mut options: ScatterOptions,
    density: f32,
    frames: u32,
) -> Result<(), LookdevError> {
    if options.width == 960 {
        options.width = 2560;
        options.height = 1440;
    }
    options.frames = 1;
    options.gpu_timing = true;
    let mut stage = ScatterStage::with_density(device, Planet::Mars, config, density)?;
    println!("density x{density}");
    let shot = ScatterShot::SwitchbacksWide;
    let (eye, target, fov) = stage.camera(shot);
    type Timing = regia_render::GpuTimings;
    type Column = (&'static str, fn(&Timing) -> f32);
    let fields: [Column; 6] = [
        ("scene", |t| t.scene_ms),
        ("shadows", |t| t.shadows_ms),
        ("prepass", |t| t.prepass_ms),
        ("cull", |t| t.cull_ms),
        ("forward", |t| t.forward_ms),
        ("ssao", |t| t.ssao_ms),
    ];
    let median = |v: &[Timing], f: fn(&Timing) -> f32| {
        let mut xs: Vec<f32> = v.iter().map(f).collect();
        xs.sort_by(f32::total_cmp);
        xs.get(xs.len() / 2).copied().unwrap_or(0.0)
    };
    let print = |title: &str, on: &[Timing], off: &[Timing]| {
        println!("{title}");
        for (name, f) in fields {
            let (a, b) = (median(on, f), median(off, f));
            println!(
                "  {name:8} on {a:7.3} ms  off {b:7.3} ms  scatter cost {:7.3} ms",
                a - b
            );
        }
    };
    // Steady state: the camera strafes 0.25 m per frame across the view (a pan keeps the rock
    // count steady; paged sun shadows re-render what the move dirtied), scatter on for the
    // whole path, then off for the same path.
    let dir = [target[0] - eye[0], 0.0, target[2] - eye[2]];
    let len = (dir[0] * dir[0] + dir[2] * dir[2]).sqrt().max(1e-3);
    let step = [-dir[2] / len * 0.25, 0.0, dir[0] / len * 0.25];
    let mut steady = [Vec::new(), Vec::new()];
    let mut in_view: Vec<usize> = Vec::new();
    let mut cpu_ms: Vec<f32> = Vec::new();
    let mut seat_cm = 0u32;
    let mut last = None;
    for (slot, scatter) in [(0usize, true), (1usize, false)] {
        for i in 0..(frames + 8) {
            let k = i as f32;
            let e = [eye[0] + step[0] * k, eye[1], eye[2] + step[2] * k];
            let t = [target[0] + step[0] * k, target[1], target[2] + step[2] * k];
            let capture = stage.render_view(
                device,
                e,
                t,
                fov,
                ScatterOptions {
                    scatter,
                    contact: scatter,
                    ..options
                },
            )?;
            if i >= 8 {
                if scatter {
                    cpu_ms.push(capture.scatter_cpu_ms);
                    seat_cm = seat_cm.max(capture.stats.seat_max_cm);
                }
                steady[slot].push(capture.gpu);
                if scatter {
                    in_view.push(capture.drawn);
                }
            }
            if scatter && i == frames + 7 {
                last = Some((capture.stats, capture.drawn));
            }
        }
    }
    let (stats, drawn) = last.unwrap_or_default();
    println!(
        "{}x{}: scatter emitted {} instances, {} in frustum, per LOD {:?}",
        options.width, options.height, stats.emitted, drawn, stats.per_lod
    );
    in_view.sort_unstable();
    println!(
        "steady path: rocks in frustum min {} median {} max {}",
        in_view.first().copied().unwrap_or(0),
        in_view.get(in_view.len() / 2).copied().unwrap_or(0),
        in_view.last().copied().unwrap_or(0)
    );
    cpu_ms.sort_by(f32::total_cmp);
    println!(
        "steady path: scatter CPU (update + collect + seating) median {:.2} ms, max {:.2} ms; largest seat {seat_cm} cm",
        cpu_ms.get(cpu_ms.len() / 2).copied().unwrap_or(0.0),
        cpu_ms.last().copied().unwrap_or(0.0)
    );
    print(
        "steady moving camera (scatter on vs off over the same path):",
        &steady[0],
        &steady[1],
    );
    // Worst case: scatter toggled every frame, so every rock is redrawn into every cascade.
    let mut toggled = [Vec::new(), Vec::new()];
    let mut first_drawn = 0;
    for round in 0..(frames + 4) {
        for (slot, scatter) in [(0usize, true), (1usize, false)] {
            let capture = stage.render(
                device,
                shot,
                ScatterOptions {
                    scatter,
                    contact: scatter,
                    ..options
                },
            )?;
            if round >= 4 {
                toggled[slot].push(capture.gpu);
            }
            if scatter {
                first_drawn = capture.drawn;
            }
        }
    }
    println!(
        "toggled view: {} rocks in frustum (first frame of the path)",
        first_drawn
    );
    print(
        "worst case (scatter toggled every frame: every rock's shadow pages redrawn every frame):",
        &toggled[0],
        &toggled[1],
    );
    Ok(())
}
