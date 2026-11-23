// SPDX-License-Identifier: MIT-0
//! `do27_bench`: DO27's frame-time bench on its real maps (`lookdev::do27_bench`).
//!
//! ```text
//! REGIA_DO27_CONTENT=<do27-content> do27_bench [--maps bore_twelve,switchbacks]
//!     [--tiers high,ultra] [--sizes 2560x1440,3840x1600,3840x2160] [--segments spawn,flyover,fight]
//!     [--frames 600] [--warmup 120] [--out <dir>] [--png]   (summary.csv, report.txt, PNGs)
//!     [--baseline <summary.csv>] [--tolerance 0.1]   (exit 1 on a regression of wall,
//!                  gpu frame, cpu render_into or host scatter p50/p95 over the path)
//!     [--no-spans] (no tracing layer: the CPU phases without its overhead)
//!     [--off ssgi,ssao,ssr,translucent-shadows,contact,reflocc,gi-probes,emissive,fog,clouds,taa,lodfade,near-tile]
//!                  (diagnosis only)
//!     [--vt [trilinear]] [--budget mid] [--aniso N] [--scale 0.67] [--upscaler tsr|fsr3]   (opt-in levers: the terrain
//!                  virtual texture in its default Surface mode, the terrain budget cuts,
//!                  a render scale and temporal upscaler; the frame is then not the default)
//!     [--scatter map|scan] [--density 1,3,5,10]   (the scatter set: the map's `scatter.ron`
//!                  with stand-ins, or DO27's shipped scan-kit set; each density factor is a
//!                  case, `<map>+<set>-x<factor>`; see `lookdev::dense_scatter`)
//!     [--detail] [--detail-px 1.0] [--shadow-min-r 0.12]   (the scatter's opt-in detail
//!                  limits, `regia_terrain::ScatterDetail`: screen-size fade, small casters)
//!     [--coarse-lod] [--clump 8]   (content levers for the scatter variants: a vertex-clustered
//!                  extra LOD, small-stone classes placed as clumps; `lookdev::dense_scatter`)
//!     [--shared-materials]   (one material per scatter mesh path: textures resident once)
//!     [--trace]    (name every frame-path create after warm-up)
//!     REGIA_TELEMETRY=verbose|normal   (drive regia_telemetry every frame as a game does,
//!                  with the frame-profiler layer and a filtered JSONL sink `<out>/telemetry.jsonl`;
//!                  prints its per-frame cost; REGIA_TELEMETRY_SPANS=0 drops the profiler layer)
//! do27_bench --compare <summary.csv> <baseline.csv> [--tolerance 0.1]
//! ```
//!
//! `cargo xtask bench-do27 -- <args>` builds it in release and runs it.

use std::process::ExitCode;

use lookdev::do27_bench::{
    BenchOptions, FRAMES_HEADER, Segment, case_text, compare, frames_csv, from_csv, parse_tier, to_csv,
    top_costs,
};
use lookdev::{LookdevError, Planet};

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn read(path: &str) -> Result<String, LookdevError> {
    std::fs::read_to_string(path).map_err(|source| LookdevError::Io {
        path: path.to_owned(),
        source,
    })
}

fn write(path: &std::path::Path, text: &str) -> Result<(), LookdevError> {
    std::fs::write(path, text).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn list<T>(
    args: &[String],
    flag: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<Vec<T>>, LookdevError> {
    let Some(v) = value(args, flag) else {
        return Ok(None);
    };
    v.split(',')
        .map(|item| parse(item).ok_or_else(|| LookdevError::Config(format!("{flag}: unknown `{item}`"))))
        .collect::<Result<Vec<T>, _>>()
        .map(Some)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match main_inner(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("do27_bench: {err}");
            ExitCode::from(2)
        }
    }
}

/// `Ok(false)`: a regression against the baseline.
fn main_inner(args: &[String]) -> Result<bool, LookdevError> {
    let tolerance = value(args, "--tolerance")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(0.1);
    if let Some(i) = args.iter().position(|a| a == "--compare") {
        let (Some(now), Some(base)) = (args.get(i + 1), args.get(i + 2)) else {
            return Err(LookdevError::Config(
                "--compare <summary.csv> <baseline.csv>".into(),
            ));
        };
        let (text, regressed) = compare(&from_csv(&read(now)?)?, &from_csv(&read(base)?)?, tolerance);
        print!("{text}");
        return Ok(!regressed);
    }
    let content = lookdev::do27::content_dir()
        .ok_or_else(|| LookdevError::Config("set REGIA_DO27_CONTENT=<do27-content dir>".into()))?;
    let mut options = BenchOptions::default();
    if let Some(maps) = list(args, "--maps", |m| match m {
        "bore_twelve" => Some(Planet::Europa),
        "switchbacks" => Some(Planet::Mars),
        _ => None,
    })? {
        options.maps = maps;
    }
    if let Some(tiers) = list(args, "--tiers", parse_tier)? {
        options.tiers = tiers;
    }
    if let Some(sizes) = list(args, "--sizes", |s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    })? {
        options.sizes = sizes;
    }
    if let Some(segments) = list(args, "--segments", Segment::parse)? {
        options.segments = segments;
    }
    if let Some(v) = value(args, "--frames").and_then(|v| v.parse().ok()) {
        options.frames = v;
    }
    if let Some(v) = value(args, "--warmup").and_then(|v| v.parse().ok()) {
        options.warmup = v;
    }
    if let Some(off) = value(args, "--off") {
        options.off = off.split(',').map(str::to_owned).collect();
    }
    if let Some(i) = args.iter().position(|a| a == "--vt") {
        let mut vt = regia_terrain::VirtualTextureSettings::default();
        // `--vt trilinear`: both LODs per pixel (no stochastic pick), for frames without TAA.
        if args.get(i + 1).is_some_and(|v| v == "trilinear") {
            vt.stochastic_lod = false;
        }
        options.vt = Some(vt);
    }
    if value(args, "--budget") == Some("mid") {
        options.budget = regia_render::TerrainBudget::MID_RANGE;
    }
    // `--aniso N`: the terrain's anisotropic filtering level (0 = off; default the tier's).
    if let Some(level) = value(args, "--aniso").and_then(|v| v.parse().ok()) {
        options.terrain_anisotropy = Some(level);
    }
    if let Some(v) = value(args, "--scale").and_then(|v| v.parse().ok()) {
        options.render_scale = Some(v);
    }
    if let Some(v) = value(args, "--scatter") {
        options.scatter = Some(
            lookdev::dense_scatter::BenchScatter::parse(v)
                .ok_or_else(|| LookdevError::Config(format!("--scatter {v}: map|scan")))?,
        );
    }
    if let Some(factors) = list(args, "--density", |f| f.parse::<f32>().ok().filter(|f| *f >= 1.0))? {
        options.density = factors;
    }
    if args.iter().any(|a| a == "--detail") {
        let mut detail = regia_terrain::ScatterDetail::default();
        if let Some(v) = value(args, "--detail-px").and_then(|v| v.parse().ok()) {
            detail.min_radius_px = v;
        }
        if let Some(v) = value(args, "--shadow-min-r").and_then(|v| v.parse().ok()) {
            detail.shadow_min_radius_m = v;
        }
        options.detail = Some(detail);
    }
    options.trace = args.iter().any(|a| a == "--trace");
    options.levers.coarse_lod = args.iter().any(|a| a == "--coarse-lod");
    options.levers.shared_materials = args.iter().any(|a| a == "--shared-materials");
    if let Some(v) = value(args, "--clump").and_then(|v| v.parse().ok()) {
        options.levers.clump = v;
    }
    match value(args, "--upscaler") {
        Some("tsr") => options.upscaler = regia_render::TemporalUpscaler::Tsr,
        Some("fsr3") => options.upscaler = regia_render::TemporalUpscaler::Fsr3,
        _ => {}
    }
    let out = value(args, "--out").map(std::path::PathBuf::from);
    if let Some(dir) = out.as_ref() {
        std::fs::create_dir_all(dir).map_err(|source| LookdevError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        if args.iter().any(|a| a == "--png") {
            options.png_dir = Some(dir.clone());
        }
    }
    // REGIA_TELEMETRY=normal|verbose: telemetry runs every frame as in a game (frame profiler
    // layer + JSONL sink), and its cost is printed per segment. Unset or `off`: as before.
    let telemetry_on =
        std::env::var("REGIA_TELEMETRY").is_ok_and(|v| regia_telemetry::TelemetryLevel::parse(&v).is_some());
    let mut _telemetry_guard = None;
    let systems = if args.iter().any(|a| a == "--no-spans") {
        None
    } else if telemetry_on {
        let jsonl = out.as_ref().map_or_else(
            || std::path::PathBuf::from("telemetry.jsonl"),
            |d| d.join("telemetry.jsonl"),
        );
        lookdev::spans::install_systems_telemetry(&jsonl).map(|(systems, profiler, guard)| {
            options.telemetry = Some(profiler);
            _telemetry_guard = Some(guard);
            systems
        })
    } else {
        lookdev::spans::install_systems()
    };
    let device = lookdev::chase::runtime_device()?;
    let info = device.adapter.get_info();
    let mut report = format!(
        "do27_bench on {} ({:?}, driver {} {}); {} measured + {} warm-up frames per segment; spans {}\n",
        info.name,
        info.backend,
        info.driver,
        info.driver_info,
        options.frames,
        options.warmup,
        if systems.is_some() { "on" } else { "off" }
    );
    print!("{report}");
    let mut rows = Vec::new();
    let mut tops = String::from("\nTop 5 costs per case (p50 over the path):\n");
    let mut frames = format!("{FRAMES_HEADER}\n");
    lookdev::do27_bench::run(&device, &content, &options, systems.as_ref(), |case| {
        let summary = case.summary();
        let text = case_text(case, &summary);
        print!("{text}");
        report.push_str(&text);
        let top: Vec<String> = top_costs(&summary)
            .iter()
            .take(5)
            .map(|(name, ms)| format!("{name} {ms:.2}"))
            .collect();
        tops.push_str(&format!("  {:<34} {}\n", case.key(), top.join(" | ")));
        rows.extend(summary);
        frames.push_str(&frames_csv(case));
    })?;
    print!("{tops}");
    report.push_str(&tops);
    let mut ok = true;
    if let Some(base) = value(args, "--baseline") {
        let (text, regressed) = compare(&rows, &from_csv(&read(base)?)?, tolerance);
        let text = format!("\nAgainst {base} (tolerance {:.0} %):\n{text}", tolerance * 100.0);
        print!("{text}");
        report.push_str(&text);
        ok = !regressed;
    }
    if let Some(dir) = out.as_ref() {
        write(&dir.join("summary.csv"), &to_csv(&rows))?;
        write(&dir.join("report.txt"), &report)?;
        write(&dir.join("frames.csv"), &frames)?;
        println!("wrote {}/summary.csv, frames.csv and report.txt", dir.display());
    }
    Ok(ok)
}
