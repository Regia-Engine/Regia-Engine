// SPDX-License-Identifier: MIT-0
//! `look_ci`: the look-regression check at every green tag (`lookdev::look_ci`,
//! `book/user-guide/look-ci.md`).
//!
//! ```text
//! look_ci render  --out <dir> [--ue <ue plates dir>] [--width 960] [--height 540] [--frames 14]
//!                 (the twelve parity plates at the LOOK_R2_TIER tier, plus metrics.json;
//!                  REGIA_DO27_CONTENT=<do27-content> renders DO27's maps and plate cameras)
//! look_ci metrics --dir <dir> [--ue <ue plates dir>]          (metrics.json for an existing set)
//! look_ci compare --this <set> [--prev <set>] [--ue <dir>] [--noise <noise.json>]
//!                 [--this-label <tag>] [--prev-label <tag>] [--location <text>] [--md <path>]
//!                 [--content <DO27 content fingerprint both sets were rendered on>]
//!                 (sheets, heat maps, compare.json and the markdown summary into <set>)
//! look_ci noise   --a <set> --b <set> --out <noise.json> [--label <tag>]
//!                 (FLIP and GPU p50 spread between two renders of the same tree)
//! ```
//!
//! A render *set* holds one directory per tier (`desktop-high/`, `deck/`) of `<shot>.png` and the
//! `do27_chase --plates --csv` run as `timing.csv`. UE plates are `<shot>_ue.png` (or
//! `<shot>.png`) at any multiple of the render size.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::default_look::{read_png, write_png};
use lookdev::look_ci::{
    CompareSummary, MetricsFile, NoiseFloor, PlateComparison, PlateRecord, TIERS, TIMING_CSV, compare_gpu,
    gpu_p50_by_plate, hstack, plate_names, plate_stats, ue_gap, vstack,
};
use lookdev::look_r2::{CameraOverride, R2Options, R2Planets, R2Shot, render_shot};
use lookdev::measure::downscale_to_width;
use regia_render::compare::{DEFAULT_PPD, compare_srgb8, flip_heatmap_rgba8};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("render") => render(&args[1..]),
        Some("metrics") => metrics(&args[1..]),
        Some("compare") => compare(&args[1..]),
        Some("noise") => noise(&args[1..]),
        _ => Err(LookdevError::Config(
            "usage: look_ci render|metrics|compare|noise ... (see the bin's doc comment)".into(),
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("look_ci: {err}");
            ExitCode::from(1)
        }
    }
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn required<'a>(args: &'a [String], flag: &str) -> Result<&'a str, LookdevError> {
    value(args, flag).ok_or_else(|| LookdevError::Config(format!("{flag} <value> is required")))
}

fn io_err(path: &Path, source: std::io::Error) -> LookdevError {
    LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), LookdevError> {
    std::fs::write(path, text).map_err(|e| io_err(path, e))
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, LookdevError> {
    serde_json::to_string_pretty(value).map_err(|e| LookdevError::Json(e.to_string()))
}

/// A PNG if it exists and decodes.
fn try_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    path.is_file().then(|| read_png(path).ok()).flatten()
}

/// The UE plate of `shot` box-filtered to `width × height`, if one is there at a multiple of
/// that size.
fn ue_plate(ue_dir: Option<&Path>, shot: &str, width: u32, height: u32) -> Option<Vec<u8>> {
    let dir = ue_dir?;
    let (w, h, px) =
        try_png(&dir.join(format!("{shot}_ue.png"))).or_else(|| try_png(&dir.join(format!("{shot}.png"))))?;
    let (px, w2, h2) = downscale_to_width(&px, w, h, width);
    ((w2, h2) == (width, height)).then_some(px)
}

fn record(shot: &str, pixels: &[u8], width: u32, height: u32, ue_dir: Option<&Path>) -> PlateRecord {
    let stats = plate_stats(pixels, width, height);
    let ue = ue_plate(ue_dir, shot, width, height).map(|px| plate_stats(&px, width, height));
    PlateRecord {
        shot: shot.to_string(),
        stats,
        ue,
        ue_gap: ue.map(|u| ue_gap(&stats, &u)),
    }
}

fn tier_name() -> String {
    std::env::var("LOOK_R2_TIER").unwrap_or_else(|_| "desktop-high".into())
}

fn render(args: &[String]) -> Result<(), LookdevError> {
    let out = PathBuf::from(required(args, "--out")?);
    std::fs::create_dir_all(&out).map_err(|e| io_err(&out, e))?;
    let ue_dir = value(args, "--ue").map(PathBuf::from);
    let mut options = R2Options::evidence();
    let parse = |flag: &str, default: u32| value(args, flag).and_then(|v| v.parse().ok()).unwrap_or(default);
    options.width = parse("--width", options.width);
    options.height = parse("--height", options.height);
    options.frames = parse("--frames", options.frames);
    // DO27's cooked terrain is BC7/BC5/BC4: ask for block compression when rendering it.
    let device = if lookdev::do27::content_dir().is_some() {
        regia_terrain::gpu::headless_bcn_device()?
    } else {
        regia_render::RenderDevice::headless()?
    };
    println!(
        "adapter: {}, tier {}",
        device.adapter.get_info().name,
        tier_name()
    );
    let mut planets = R2Planets::default();
    let mut plates = Vec::new();
    for shot in R2Shot::PARITY {
        let started = Instant::now();
        let (pixels, _) = render_shot(&device, &mut planets, shot, options, CameraOverride::default())?;
        let path = out.join(format!("{}.png", shot.name()));
        write_png(&path, options.width, options.height, &pixels)?;
        println!(
            "{} -> {} ({:.1} s)",
            shot.name(),
            path.display(),
            started.elapsed().as_secs_f32()
        );
        plates.push(record(
            shot.name(),
            &pixels,
            options.width,
            options.height,
            ue_dir.as_deref(),
        ));
    }
    let metrics = MetricsFile {
        tier: tier_name(),
        plates,
    };
    write_text(&out.join("metrics.json"), &to_json(&metrics)?)
}

fn metrics(args: &[String]) -> Result<(), LookdevError> {
    let dir = PathBuf::from(required(args, "--dir")?);
    let ue_dir = value(args, "--ue").map(PathBuf::from);
    let mut plates = Vec::new();
    for shot in plate_names() {
        if let Some((w, h, px)) = try_png(&dir.join(format!("{shot}.png"))) {
            plates.push(record(shot, &px, w, h, ue_dir.as_deref()));
        }
    }
    let tier = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = dir.join("metrics.json");
    write_text(&path, &to_json(&MetricsFile { tier, plates })?)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// One plate's comparison plus its sheet row (`UE | prev | this | FLIP`, full size).
struct PlateOutput {
    comparison: PlateComparison,
    sheet: (u32, u32, Vec<u8>),
}

fn compare_plate(
    set: &Path,
    prev_set: Option<&Path>,
    ue_dir: Option<&Path>,
    noise: Option<&NoiseFloor>,
    tier: &str,
    shot: &str,
) -> Option<PlateOutput> {
    let (w, h, this) = try_png(&set.join(tier).join(format!("{shot}.png")))?;
    let prev = prev_set
        .and_then(|p| try_png(&p.join(tier).join(format!("{shot}.png"))))
        .filter(|(pw, ph, _)| (*pw, *ph) == (w, h))
        .map(|(_, _, px)| px);
    let ue = ue_plate(ue_dir, shot, w, h);
    let grey = vec![64u8; (w * h * 4) as usize];
    let (diff, heat) = match &prev {
        Some(prev) => {
            let (report, flip) = compare_srgb8(prev, &this, w, h, DEFAULT_PPD);
            (
                Some((report.flip_mean, report.flip_p95, report.ssim)),
                flip_heatmap_rgba8(&flip.map),
            )
        }
        None => (None, grey.clone()),
    };
    let comparison = PlateComparison::new(
        tier,
        shot,
        plate_stats(&this, w, h),
        prev.as_ref().map(|p| plate_stats(p, w, h)),
        ue.as_ref().map(|p| plate_stats(p, w, h)),
        diff,
        noise,
    );
    let (sw, px) = hstack(
        &[
            ue.as_deref().unwrap_or(&grey),
            prev.as_deref().unwrap_or(&grey),
            &this,
            &heat,
        ],
        w,
        h,
        4,
    );
    Some(PlateOutput {
        comparison,
        sheet: (sw, h, px),
    })
}

fn read_noise(path: Option<&str>) -> Result<Option<NoiseFloor>, LookdevError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|e| io_err(Path::new(path), e))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| LookdevError::Json(format!("{path}: {e}")))
}

fn timing(set: &Path) -> std::collections::BTreeMap<String, f32> {
    std::fs::read_to_string(set.join(TIMING_CSV))
        .map(|csv| gpu_p50_by_plate(&csv))
        .unwrap_or_default()
}

fn compare(args: &[String]) -> Result<(), LookdevError> {
    let set = PathBuf::from(required(args, "--this")?);
    let prev_set = value(args, "--prev").map(PathBuf::from);
    let ue_dir = value(args, "--ue").map(PathBuf::from);
    let noise = read_noise(value(args, "--noise"))?;
    let label = |flag: &str, path: &Path| {
        value(args, flag).map_or_else(
            || {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            str::to_string,
        )
    };
    let started = Instant::now();
    let mut plates = Vec::new();
    for tier in TIERS {
        if !set.join(tier).is_dir() {
            continue;
        }
        let out_dir = set.join("compare").join(tier);
        std::fs::create_dir_all(&out_dir).map_err(|e| io_err(&out_dir, e))?;
        // FLIP at 960×540 is a few hundred ms per plate: one thread per plate.
        let outputs: Vec<Option<PlateOutput>> = std::thread::scope(|scope| {
            let handles: Vec<_> = plate_names()
                .into_iter()
                .map(|shot| {
                    let (set, prev, ue, noise) =
                        (&set, prev_set.as_deref(), ue_dir.as_deref(), noise.as_ref());
                    scope.spawn(move || compare_plate(set, prev, ue, noise, tier, shot))
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        let mut rows = Vec::new();
        let mut row_size = None;
        for output in outputs.into_iter().flatten() {
            let (sw, sh, px) = output.sheet;
            write_png(
                &out_dir.join(format!("{}.png", output.comparison.shot)),
                sw,
                sh,
                &px,
            )?;
            // Overview rows at half size.
            let (half, hw, hh) = downscale_to_width(&px, sw, sh, sw / 2);
            if row_size.is_none_or(|s| s == (hw, hh)) {
                row_size = Some((hw, hh));
                rows.push(half);
            }
            plates.push(output.comparison);
        }
        if let Some((hw, hh)) = row_size {
            let (oh, px) = vstack(&rows, hw, hh, 6);
            write_png(&set.join(format!("overview_{tier}.png")), hw, oh, &px)?;
        }
    }
    let gpu = compare_gpu(
        &prev_set.as_deref().map(timing).unwrap_or_default(),
        &timing(&set),
    );
    let summary = CompareSummary {
        this: label("--this-label", &set),
        prev: prev_set.as_deref().map(|p| label("--prev-label", p)),
        location: value(args, "--location").map_or_else(|| set.display().to_string(), str::to_string),
        content: value(args, "--content").map(str::to_string),
        plates,
        gpu,
    };
    write_text(&set.join("compare.json"), &to_json(&summary)?)?;
    let md_path = value(args, "--md").map_or_else(|| set.join("summary.md"), PathBuf::from);
    write_text(&md_path, &summary.markdown())?;
    println!(
        "compared {} plates in {:.1} s: {} flags; wrote {}",
        summary.plates.len(),
        started.elapsed().as_secs_f32(),
        summary.flag_count(),
        md_path.display()
    );
    Ok(())
}

fn noise(args: &[String]) -> Result<(), LookdevError> {
    let a = PathBuf::from(required(args, "--a")?);
    let b = PathBuf::from(required(args, "--b")?);
    let out = PathBuf::from(required(args, "--out")?);
    let mut floor = NoiseFloor {
        source: value(args, "--label").unwrap_or_default().to_string(),
        ..NoiseFloor::default()
    };
    for tier in TIERS {
        let pairs: Vec<(String, Option<(f32, f32)>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = plate_names()
                .into_iter()
                .map(|shot| {
                    let (a, b) = (&a, &b);
                    scope.spawn(move || {
                        let pa = try_png(&a.join(tier).join(format!("{shot}.png")));
                        let pb = try_png(&b.join(tier).join(format!("{shot}.png")));
                        let score = match (pa, pb) {
                            (Some((w, h, x)), Some((w2, h2, y))) if (w, h) == (w2, h2) => {
                                let (r, _) = compare_srgb8(&x, &y, w, h, DEFAULT_PPD);
                                Some((r.flip_mean, r.flip_p95))
                            }
                            _ => None,
                        };
                        (format!("{tier}/{shot}"), score)
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (key, score) in pairs {
            if let Some(score) = score {
                println!("{key}: FLIP {:.5} p95 {:.4}", score.0, score.1);
                floor.flip.insert(key, score);
            }
        }
    }
    let (ta, tb) = (timing(&a), timing(&b));
    for (plate, ms_a) in &ta {
        if let Some(ms_b) = tb.get(plate) {
            let spread = (ms_b - ms_a).abs() / ms_a.max(1e-3);
            println!(
                "{plate}: GPU p50 {ms_a:.3} / {ms_b:.3} ms ({:.1} %)",
                spread * 100.0
            );
            floor.gpu.insert(plate.clone(), spread);
        }
    }
    write_text(&out, &to_json(&floor)?)?;
    println!("wrote {}", out.display());
    Ok(())
}
