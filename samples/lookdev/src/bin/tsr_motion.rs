// SPDX-License-Identifier: MIT-0
//! `tsr_motion`: the `specs/27` § 2.3 motion chart for each temporal upscaler.
//!
//! ```text
//! tsr_motion --out <dir> [--frames 120]
//!     [--kernel K] [--sharpness S] [--max-frames N] [--ridge R] [--lock-decay D]
//!     [--motion-sharpen M]   (TSR tuning sweep)
//!     [--debug | --debug-only] [--debug-pan PX] [--debug-scale S] [--probe] [--gpu-driven]
//! ```
//!
//! `--debug` also writes TSR debug views 0–4 (`TsrSettings::debug_view`) after the pan;
//! `--probe` prints one chart row per debug view after 16 and 48 frames.
//!
//! Renders the native still, native TAA, TAAU 0.67, TSR at 0.67 and 0.5, and FSR 3.1 at
//! 0.67 and 0.5 while the camera pans 2 px/frame; writes each last frame, a 3× stacked crop
//! (`pan_strip.png`, rows in that order), and `motion.json` with the line-pair MTF50,
//! lattice bars resolved, and ghost residual per mode.

use std::path::PathBuf;
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::write_png;
use lookdev::tsr_motion::{ChartMode, ghost_energy, measure_mode, measure_mtf, render_pan, render_pan_with};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tsr_motion: {err}");
            ExitCode::from(1)
        }
    }
}

fn apply(settings: &mut regia_render::TsrSettings, flag: &str, v: f32) {
    match flag {
        "--ridge" => settings.ridge_threshold = v,
        "--sharpness" => settings.sharpness = v,
        "--max-frames" => settings.max_frames = v,
        "--kernel" => settings.kernel_sharpness = v,
        "--motion-sharpen" => settings.motion_sharpness = v,
        _ => settings.lock_decay = v,
    }
}

fn run() -> Result<(), LookdevError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = PathBuf::from("target/tsr_motion");
    let mut frames = 120u32;
    let mut debug = false;
    let mut debug_only = false;
    let mut debug_pan = 2.0f32;
    let mut debug_scale = 0.67f32;
    let mut tune = regia_render::TsrSettings::for_scale(0.67);
    let mut overrides: Vec<(String, f32)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out = PathBuf::from(
                    args.get(i + 1)
                        .ok_or_else(|| LookdevError::Config("--out".into()))?,
                );
                i += 2;
            }
            "--frames" => {
                frames = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| LookdevError::Config("--frames N".into()))?;
                i += 2;
            }
            "--debug" => {
                debug = true;
                i += 1;
            }
            "--debug-only" => {
                debug = true;
                debug_only = true;
                i += 1;
            }
            "--ridge" | "--sharpness" | "--max-frames" | "--lock-decay" | "--kernel" | "--motion-sharpen" => {
                let v: f32 = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| LookdevError::Config(format!("{} needs a number", args[i])))?;
                apply(&mut tune, &args[i], v);
                overrides.push((args[i].clone(), v));
                i += 2;
            }
            "--debug-scale" => {
                debug_scale = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| LookdevError::Config("--debug-scale S".into()))?;
                i += 2;
            }
            "--debug-pan" => {
                debug_pan = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| LookdevError::Config("--debug-pan PX".into()))?;
                i += 2;
            }
            "--probe" => i += 1,
            "--gpu-driven" => {
                lookdev::tsr_motion::CHART_GPU_DRIVEN.store(true, std::sync::atomic::Ordering::Relaxed);
                i += 1;
            }
            other => return Err(LookdevError::Config(format!("unknown argument {other}"))),
        }
    }
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let device = regia_render::RenderDevice::headless()?;
    if debug {
        // TSR 0.67 debug views after the pan: frames, lock, disocclusion.
        for view in 0..=4u32 {
            let settings = regia_render::TsrSettings {
                debug_view: view,
                ..tune
            };
            let (image, _) = render_pan_with(
                &device,
                ChartMode::Tsr(debug_scale),
                frames,
                debug_pan,
                Some(settings),
            )?;
            write_png(
                &out.join(format!("debug_{view}.png")),
                image.width,
                image.height,
                &image.pixels,
            )?;
        }
        if debug_only {
            if std::env::args().any(|a| a == "--probe") {
                for (n, view) in [
                    (16u32, 1u32),
                    (16, 2),
                    (16, 3),
                    (16, 0),
                    (48, 1),
                    (48, 2),
                    (48, 3),
                    (48, 0),
                ] {
                    let settings = regia_render::TsrSettings {
                        debug_view: view,
                        ..tune
                    };
                    let (image, _) =
                        render_pan_with(&device, ChartMode::Tsr(debug_scale), n, 0.0, Some(settings))?;
                    let row: Vec<u8> = (12..28)
                        .map(|x| image.pixels[((122 * image.width + x) * 4) as usize])
                        .collect();
                    println!("probe frame {n} view {view}: {row:?}");
                }
            }
            return Ok(());
        }
    }
    let native = render_pan(&device, ChartMode::NativeStill, 1, 0.0)?;
    write_png(
        &out.join("native_still.png"),
        native.0.width,
        native.0.height,
        &native.0.pixels,
    )?;
    let native_mtf = measure_mtf(&native.0);
    let (native_ghost, native_ghost_frame) = ghost_energy(&device, ChartMode::NativeStill, 64, None)?;
    println!(
        "native_still ghost residual {:.2} % bias {:.2} % (metric floor)",
        100.0 * native_ghost.residual,
        100.0 * native_ghost.bias
    );
    if let Some(frame) = native_ghost_frame {
        write_png(
            &out.join("ghost_native_still.png"),
            frame.width,
            frame.height,
            &frame.pixels,
        )?;
    }
    println!(
        "native_still mtf50 {:.3} contrast {:?}",
        native_mtf.mtf50, native_mtf.contrast
    );
    let modes: Vec<ChartMode> = if overrides.is_empty() {
        vec![
            ChartMode::NativeTaa,
            ChartMode::Taau(0.67),
            ChartMode::Tsr(0.67),
            ChartMode::Tsr(0.5),
            ChartMode::Fsr3(0.67),
            ChartMode::Fsr3(0.5),
        ]
    } else {
        // A tuning sweep: only the TSR modes.
        vec![ChartMode::Tsr(0.67), ChartMode::Tsr(0.5)]
    };
    let mut reports = Vec::new();
    let mut images = vec![native.0.clone()];
    for mode in modes {
        let tsr = match mode {
            ChartMode::Tsr(scale) => {
                let mut settings = regia_render::TsrSettings::for_scale(scale);
                for (flag, v) in &overrides {
                    apply(&mut settings, flag, *v);
                }
                Some(settings)
            }
            _ => None,
        };
        let (report, image) = measure_mode(&device, mode, frames, &native, tsr)?;
        println!(
            "{:<12} mtf50 {:.3} ({:.0} % of native) lattice {:.0} % ghost bias {:.2} % residual {:.2} % modulation {:?}",
            report.mode,
            report.pan.mtf50,
            100.0 * report.pan.mtf50 / native_mtf.mtf50.max(1.0e-6),
            100.0 * report.lattice,
            100.0 * report.ghost.bias,
            100.0 * report.ghost.residual,
            report.pan.modulation,
        );
        write_png(
            &out.join(format!("{}.png", report.mode)),
            image.width,
            image.height,
            &image.pixels,
        )?;
        if let Some(frame) = &report.ghost_frame {
            write_png(
                &out.join(format!("ghost_{}.png", report.mode)),
                frame.width,
                frame.height,
                &frame.pixels,
            )?;
        }
        images.push(image);
        reports.push(report);
    }
    // Stacked crop, 3× nearest: the left 160 columns of every mode.
    let (crop_w, scale) = (160u32, 3u32);
    let h = images[0].height;
    let strip_w = crop_w * scale;
    let strip_h = h * scale * images.len() as u32;
    let mut strip = vec![0u8; (strip_w * strip_h * 4) as usize];
    for (row, image) in images.iter().enumerate() {
        for y in 0..h * scale {
            for x in 0..strip_w {
                let src = (((y / scale) * image.width + x / scale) * 4) as usize;
                let dy = row as u32 * h * scale + y;
                let dst = ((dy * strip_w + x) * 4) as usize;
                strip[dst..dst + 4].copy_from_slice(&image.pixels[src..src + 4]);
            }
        }
    }
    write_png(&out.join("pan_strip.png"), strip_w, strip_h, &strip)?;
    let json = serde_json::json!({ "native_mtf50": native_mtf.mtf50, "frames": frames, "modes": reports });
    let path = out.join("motion.json");
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap_or_default()).map_err(|source| {
        LookdevError::Io {
            path: path.display().to_string(),
            source,
        }
    })?;
    Ok(())
}
