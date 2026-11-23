// SPDX-License-Identifier: MIT-0
//! `default_look`: renders the DO27-like default-look shot set (`lookdev::default_look`).
//!
//! ```text
//! default_look --out <dir> [--width 1280] [--height 720] [--fixed-ev <ev100>] [--frames 12]
//! default_look --side-by-side <before_dir> <after_dir> <out_dir>
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::default_look::{
    LookExposure, LookOptions, LookShot, read_png, render_shot, side_by_side, write_png,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("default_look: {err}");
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

fn io_err(path: &std::path::Path, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    if args.first().map(String::as_str) == Some("--side-by-side") {
        let [_, before, after, out] = args else {
            return Err(LookdevError::Config(
                "--side-by-side <before> <after> <out>".into(),
            ));
        };
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
        for shot in LookShot::ALL {
            let name = format!("{}.png", shot.name());
            let (w, h, left) = read_png(&PathBuf::from(before).join(&name))?;
            let (w2, h2, right) = read_png(&PathBuf::from(after).join(&name))?;
            if (w, h) != (w2, h2) {
                return Err(LookdevError::Config(format!("{name}: sizes differ")));
            }
            let (ow, pixels) = side_by_side(w, h, &left, &right);
            let path = out.join(&name);
            write_png(&path, ow, h, &pixels)?;
            println!("wrote {}", path.display());
        }
        return Ok(());
    }
    let mut options = LookOptions::evidence();
    let mut out: Option<PathBuf> = None;
    let mut only: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out")?)),
            "--width" => options.width = number(args, i, "--width")?,
            "--height" => options.height = number(args, i, "--height")?,
            "--frames" => options.frames = number(args, i, "--frames")?,
            "--fixed-ev" => options.exposure = LookExposure::Fixed(number(args, i, "--fixed-ev")?),
            "--shot" => only = Some(value(args, i, "--shot")?.to_string()),
            other => return Err(LookdevError::Config(format!("unknown flag {other}"))),
        }
        i += 2;
    }
    let out = out.ok_or_else(|| LookdevError::Config("--out <dir> is required".into()))?;
    std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
    let device = regia_render::RenderDevice::headless()?;
    for shot in LookShot::ALL {
        if only.as_deref().is_some_and(|name| name != shot.name()) {
            continue;
        }
        let mut renderer = regia_render::FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let started = Instant::now();
        let pixels = render_shot(&device, &mut renderer, shot, options)?;
        let t = renderer.gpu_timings();
        let wall = started.elapsed().as_secs_f32() * 1000.0 / options.frames.max(1) as f32;
        let path = out.join(format!("{}.png", shot.name()));
        write_png(&path, options.width, options.height, &pixels)?;
        let mean: f64 = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
            .sum::<f64>()
            / f64::from(options.width * options.height);
        println!(
            "{} -> {} mean_luma {:.1} wall {:.0} ms/frame",
            shot.name(),
            path.display(),
            mean,
            wall
        );
        println!(
            "  gpu ms: shadows {:.2} prepass {:.2} ssao {:.2} forward {:.2} ssgi {:.2} ssr {:.2} taa {:.2} scene {:.2}",
            t.shadows_ms, t.prepass_ms, t.ssao_ms, t.forward_ms, t.ssgi_ms, t.ssr_ms, t.taau_ms, t.scene_ms
        );
    }
    Ok(())
}
