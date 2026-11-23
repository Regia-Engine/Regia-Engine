// SPDX-License-Identifier: MIT-0
//! `mobile_aa`: the SMAA / SGSR chart and the plate SSIM comparison (`lookdev::mobile_aa`).
//!
//! ```text
//! mobile_aa --chart <out_dir>
//!     Renders the lattice-tower chart for every configuration, writes <label>.png and
//!     reference.png, and prints bars resolved, mean contrast and SSIM per configuration.
//! mobile_aa --ssim <reference_dir> <dir>...
//!     SSIM of every PNG in each <dir> against the PNG of the same name in <reference_dir>.
//!     A reference an integer factor larger is box-filtered down in linear light first
//!     (a supersampled reference).
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::{read_png, write_png};
use lookdev::mobile_aa::{CHART_SIZE, measure_all};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mobile_aa: {err}");
            ExitCode::from(1)
        }
    }
}

fn io_err(path: &Path, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    match args.first().map(String::as_str) {
        Some("--chart") => {
            let out = PathBuf::from(
                args.get(1)
                    .ok_or_else(|| LookdevError::Config("--chart <out_dir>".into()))?,
            );
            std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
            let device = regia_render::RenderDevice::headless()?;
            println!("adapter: {}", device.adapter.get_info().name);
            let (results, reference) = measure_all(&device)?;
            let (w, h) = CHART_SIZE;
            write_png(&out.join("reference.png"), w, h, &reference)?;
            println!(
                "{:<20} {:>14} {:>14} {:>8}",
                "config", "bars_resolved", "mean_contrast", "ssim"
            );
            for r in &results {
                println!(
                    "{:<20} {:>13.1}% {:>14.3} {:>8.4}",
                    r.config.label,
                    100.0 * r.bars_resolved,
                    r.mean_contrast,
                    r.ssim
                );
                write_png(
                    &out.join(format!("{}.png", r.config.label)),
                    r.image.width,
                    r.image.height,
                    &r.image.pixels,
                )?;
            }
            Ok(())
        }
        Some("--ssim") => {
            let [_, reference, dirs @ ..] = args else {
                return Err(LookdevError::Config("--ssim <reference_dir> <dir>...".into()));
            };
            ssim_dirs(Path::new(reference), dirs)
        }
        _ => Err(LookdevError::Config(
            "usage: mobile_aa --chart <out_dir> | --ssim <reference_dir> <dir>...".into(),
        )),
    }
}

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// RGB 0–1 of `rgba` (`w × h`), box-filtered down by `factor` in linear light.
fn downsample(rgba: &[u8], w: u32, h: u32, factor: u32) -> Vec<[f32; 3]> {
    let (ow, oh) = (w / factor, h / factor);
    let mut out = Vec::with_capacity((ow * oh) as usize);
    for y in 0..oh {
        for x in 0..ow {
            let mut acc = [0.0f32; 3];
            for sy in 0..factor {
                for sx in 0..factor {
                    let i = (((y * factor + sy) * w + x * factor + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += srgb_to_linear(f32::from(rgba[i + c]) / 255.0);
                    }
                }
            }
            let n = (factor * factor) as f32;
            out.push(acc.map(|a| linear_to_srgb(a / n)));
        }
    }
    out
}

fn ssim_dirs(reference: &Path, dirs: &[String]) -> Result<(), LookdevError> {
    let mut names: Vec<String> = std::fs::read_dir(reference)
        .map_err(|e| io_err(reference, e))?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".png"))
        .collect();
    names.sort();
    print!("{:<28}", "plate");
    for dir in dirs {
        let label = Path::new(dir)
            .file_name()
            .map_or(dir.clone(), |n| n.to_string_lossy().into_owned());
        print!(" {label:>22}");
    }
    println!();
    let mut sums = vec![0.0f64; dirs.len()];
    let mut counts = vec![0usize; dirs.len()];
    for name in &names {
        let (rw, rh, rpx) = read_png(&reference.join(name))?;
        print!("{name:<28}");
        for (i, dir) in dirs.iter().enumerate() {
            let Ok((w, h, px)) = read_png(&Path::new(dir).join(name)) else {
                print!(" {:>22}", "-");
                continue;
            };
            if w == 0 || h == 0 || rw % w != 0 || rh % h != 0 || rw / w != rh / h {
                return Err(LookdevError::Config(format!(
                    "{name}: reference {rw}x{rh} is not an integer multiple of {w}x{h}"
                )));
            }
            let reference = downsample(&rpx, rw, rh, rw / w);
            let image = downsample(&px, w, h, 1);
            let s = regia_render::compare::ssim(&image, &reference, w, h);
            sums[i] += f64::from(s);
            counts[i] += 1;
            print!(" {s:>22.4}");
        }
        println!();
    }
    print!("{:<28}", "mean");
    for (sum, n) in sums.iter().zip(&counts) {
        print!(" {:>22.4}", sum / (*n).max(1) as f64);
    }
    println!();
    Ok(())
}
