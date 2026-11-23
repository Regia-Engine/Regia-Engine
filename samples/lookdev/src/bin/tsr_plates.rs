// SPDX-License-Identifier: MIT-0
//! `tsr_plates`: scores upscaled DO27 plate renders against the native ones and builds
//! side-by-side sheets for judging by eye.
//!
//! ```text
//! tsr_plates --native <dir> --test <label>=<dir> [--test <label>=<dir> ...] [--ue <dir>]
//!     --out <dir> [--crop x,y,w,h]   (crop in fractions of the frame; default the centre third)
//! ```
//!
//! For every `<plate>.png` in the native directory (as `do27_chase --plates --png-dir`
//! writes them) it prints the luma SSIM (`regia_render::compare::ssim`) of each test
//! render against native, writes `<plate>_sheet.png` (UE | native | tests, half size)
//! and `<plate>_crop.png` (the same order, 1:1 crop of the region), and `ssim.json`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::default_look::{read_png, write_png};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tsr_plates: {err}");
            ExitCode::from(1)
        }
    }
}

struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn load(path: &Path) -> Result<Image, LookdevError> {
    let (width, height, rgba) = read_png(path)?;
    Ok(Image { width, height, rgba })
}

/// Nearest-neighbour resample to `width`×`height` (UE plates may differ in size).
fn resize(image: &Image, width: u32, height: u32) -> Image {
    if image.width == width && image.height == height {
        return Image {
            width,
            height,
            rgba: image.rgba.clone(),
        };
    }
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let sy = (u64::from(y) * u64::from(image.height) / u64::from(height)) as u32;
        for x in 0..width {
            let sx = (u64::from(x) * u64::from(image.width) / u64::from(width)) as u32;
            let i = ((sy * image.width + sx) * 4) as usize;
            rgba.extend_from_slice(&image.rgba[i..i + 4]);
        }
    }
    Image { width, height, rgba }
}

/// 2×2 box downscale.
fn half(image: &Image) -> Image {
    let (w, h) = (image.width / 2, image.height / 2);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..4 {
                let mut sum = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    sum +=
                        u32::from(image.rgba[(((2 * y + dy) * image.width + 2 * x + dx) * 4 + c) as usize]);
                }
                rgba.push(((sum + 2) / 4) as u8);
            }
        }
    }
    Image {
        width: w,
        height: h,
        rgba,
    }
}

fn crop(image: &Image, frac: [f32; 4]) -> Image {
    let x0 = ((frac[0] * image.width as f32) as u32).min(image.width - 1);
    let y0 = ((frac[1] * image.height as f32) as u32).min(image.height - 1);
    let w = ((frac[2] * image.width as f32) as u32).clamp(1, image.width - x0);
    let h = ((frac[3] * image.height as f32) as u32).clamp(1, image.height - y0);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in y0..y0 + h {
        let i = ((y * image.width + x0) * 4) as usize;
        rgba.extend_from_slice(&image.rgba[i..i + (w * 4) as usize]);
    }
    Image {
        width: w,
        height: h,
        rgba,
    }
}

/// Panels left to right with a 4 px gap.
fn row(panels: &[Image]) -> Image {
    let height = panels.iter().map(|p| p.height).max().unwrap_or(1);
    let gap = 4;
    let width = panels.iter().map(|p| p.width).sum::<u32>() + gap * panels.len().saturating_sub(1) as u32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let mut x0 = 0;
    for p in panels {
        for y in 0..p.height {
            let src = (y * p.width * 4) as usize;
            let dst = ((y * width + x0) * 4) as usize;
            rgba[dst..dst + (p.width * 4) as usize]
                .copy_from_slice(&p.rgba[src..src + (p.width * 4) as usize]);
        }
        x0 += p.width + gap;
    }
    Image { width, height, rgba }
}

fn to_f32(image: &Image) -> Vec<[f32; 3]> {
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

fn arg_err(flag: &str) -> LookdevError {
    LookdevError::Config(format!("{flag} needs a value"))
}

fn run() -> Result<(), LookdevError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut native: Option<PathBuf> = None;
    let mut ue: Option<PathBuf> = None;
    let mut out = PathBuf::from("target/tsr_plates");
    let mut tests: Vec<(String, PathBuf)> = Vec::new();
    let mut crop_frac = [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0];
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let v = args.get(i + 1).ok_or_else(|| arg_err(flag))?;
        match flag {
            "--native" => native = Some(PathBuf::from(v)),
            "--ue" => ue = Some(PathBuf::from(v)),
            "--out" => out = PathBuf::from(v),
            "--test" => {
                let (label, dir) = v
                    .split_once('=')
                    .ok_or_else(|| LookdevError::Config("--test <label>=<dir>".into()))?;
                tests.push((label.to_string(), PathBuf::from(dir)));
            }
            "--crop" => {
                let parts: Vec<f32> = v.split(',').filter_map(|p| p.parse().ok()).collect();
                if parts.len() != 4 {
                    return Err(LookdevError::Config("--crop x,y,w,h (fractions)".into()));
                }
                crop_frac = [parts[0], parts[1], parts[2], parts[3]];
            }
            other => return Err(LookdevError::Config(format!("unknown flag {other}"))),
        }
        i += 2;
    }
    let native = native.ok_or_else(|| LookdevError::Config("--native <dir>".into()))?;
    std::fs::create_dir_all(&out).map_err(|source| LookdevError::Io {
        path: out.display().to_string(),
        source,
    })?;
    let mut names: Vec<String> = std::fs::read_dir(&native)
        .map_err(|source| LookdevError::Io {
            path: native.display().to_string(),
            source,
        })?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png"))
        .collect();
    names.sort();
    let mut json = serde_json::Map::new();
    for name in names {
        let plate = name.trim_end_matches(".png").to_string();
        let reference = load(&native.join(&name))?;
        let (w, h) = (reference.width, reference.height);
        let ref_f = to_f32(&reference);
        let mut panels = Vec::new();
        if let Some(dir) = &ue {
            let path = dir.join(format!("{plate}_ue.png"));
            if path.exists() {
                panels.push(resize(&load(&path)?, w, h));
            }
        }
        panels.push(Image {
            width: w,
            height: h,
            rgba: reference.rgba.clone(),
        });
        let mut scores = serde_json::Map::new();
        let mut line = format!("{plate:<20}");
        for (label, dir) in &tests {
            let path = dir.join(&name);
            if !path.exists() {
                continue;
            }
            let test = resize(&load(&path)?, w, h);
            let s = regia_render::compare::ssim(&ref_f, &to_f32(&test), w, h);
            line.push_str(&format!("  {label} {s:.4}"));
            scores.insert(label.clone(), serde_json::json!(s));
            panels.push(test);
        }
        println!("{line}");
        json.insert(plate.clone(), serde_json::Value::Object(scores));
        let sheet = row(&panels.iter().map(half).collect::<Vec<_>>());
        write_png(
            &out.join(format!("{plate}_sheet.png")),
            sheet.width,
            sheet.height,
            &sheet.rgba,
        )?;
        let crops = row(&panels.iter().map(|p| crop(p, crop_frac)).collect::<Vec<_>>());
        write_png(
            &out.join(format!("{plate}_crop.png")),
            crops.width,
            crops.height,
            &crops.rgba,
        )?;
    }
    let path = out.join("ssim.json");
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(json))
        .map_err(|e| LookdevError::Config(e.to_string()))?;
    std::fs::write(&path, text).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    println!("wrote {}", out.display());
    Ok(())
}
