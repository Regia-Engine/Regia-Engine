// SPDX-License-Identifier: MPL-2.0
//! `bake_sizing`: the Bore Twelve readiness dry run on a synthetic stand-in (ADR-1099,
//! `regia_pt::bake_sizing`, `book/agents/baked-gi-sizing.md`). No DO27 content is read.
//!
//! ```text
//! cargo run --release -p regia_pt --example bake_sizing -- plan  [--level bore12|small|tall] [--layout NAME] [knobs]
//! cargo run --release -p regia_pt --example bake_sizing -- sweep [--level bore12|small|tall] [--markdown]
//! cargo run --release -p regia_pt --example bake_sizing -- ron   [--level ..] [--layout ..] [knobs] --out FILE
//! cargo run --release -p regia_pt --example bake_sizing -- bake  [--level ..] [--layout ..] [knobs] [--out FILE.fgiv]
//! ```
//!
//! Layouts: `defaults`, `doc-example`, `recommended` (default). Knobs override the layout:
//! `--terrain-spacing M` (0 = no terrain layer), `--auto-spacing M`, `--interior-spacing M`,
//! `--shaft-spacing M`, `--max-probes N`, `--layers a,b` (`all` = every layer but terrain),
//! `--moments N`, `--rays N`, `--samples N`, `--batch N`.
//!
//! Every mode prints one JSON object per line on stdout (`"ok": true`), progress on stderr. A
//! failure prints `{"ok": false, "code", "message"}` and exits 1. `plan` and `sweep` need no GPU;
//! `bake` bakes on the first GPU adapter and prints the wall time per stage, the measured rays per
//! second, the peak GPU allocation, the peak resident set and, when `--rays`/`--samples` are below
//! the layout's, the projected time at the layout's rays.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use regia_pt::PathTracer;
use regia_pt::bake_sizing::{
    SIZING_LAYOUTS, SYNTHETIC_LEVELS, SizingLayout, SizingReport, SyntheticLevel, size_job, sweep_layouts,
};
use regia_pt::probe::bake_gi_volume;
use serde_json::{Value, json};

/// A failure with its agent error code.
struct Failure {
    code: &'static str,
    message: String,
}

fn fail(code: &'static str, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            println!("{}", json!({ "ok": false, "code": f.code, "message": f.message }));
            ExitCode::from(1)
        }
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn number<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>, Failure> {
    match flag(args, name) {
        Some(v) => v
            .parse()
            .map(Some)
            .map_err(|_| fail("sizing_bad_value", format!("{name}: bad value `{v}`"))),
        None => Ok(None),
    }
}

fn level(args: &[String]) -> Result<SyntheticLevel, Failure> {
    let name = flag(args, "--level").unwrap_or("bore12");
    SyntheticLevel::preset(name).ok_or_else(|| {
        fail(
            "sizing_level_unknown",
            format!(
                "unknown level `{name}`: use one of {}",
                SYNTHETIC_LEVELS.join(", ")
            ),
        )
    })
}

fn layout(args: &[String]) -> Result<(String, SizingLayout), Failure> {
    let name = flag(args, "--layout").unwrap_or("recommended");
    let mut l = SizingLayout::preset(name).ok_or_else(|| {
        fail(
            "sizing_layout_unknown",
            format!(
                "unknown layout `{name}`: use one of {}",
                SIZING_LAYOUTS.join(", ")
            ),
        )
    })?;
    let positive = |name: &str, v: f32| {
        if v > 0.0 && v.is_finite() {
            Ok(v)
        } else {
            Err(fail("sizing_bad_value", format!("{name} must be positive")))
        }
    };
    if let Some(v) = number::<f32>(args, "--terrain-spacing")? {
        l.terrain_spacing = if v == 0.0 {
            None
        } else {
            Some(positive("--terrain-spacing", v)?)
        };
    }
    if let Some(v) = number::<f32>(args, "--auto-spacing")? {
        l.auto_spacing = positive("--auto-spacing", v)?;
    }
    if let Some(v) = number::<f32>(args, "--interior-spacing")? {
        l.interior_spacing = Some(positive("--interior-spacing", v)?);
    }
    if let Some(v) = number::<f32>(args, "--shaft-spacing")? {
        l.shaft_spacing = Some(positive("--shaft-spacing", v)?);
    }
    if let Some(v) = number(args, "--max-probes")? {
        l.max_probes = v;
    }
    if let Some(v) = flag(args, "--layers") {
        l.layers = if v == "all" {
            Vec::new()
        } else {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
    }
    if let Some(v) = number(args, "--moments")? {
        l.moment_resolution = v;
    }
    if let Some(v) = number(args, "--rays")? {
        l.rays_per_probe = v;
    }
    if let Some(v) = number(args, "--samples")? {
        l.samples = v;
    }
    if let Some(v) = number(args, "--batch")? {
        l.probes_per_batch = v;
    }
    Ok((name.to_string(), l))
}

fn run(args: &[String]) -> Result<(), Failure> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("plan") => plan(rest),
        Some("sweep") => sweep(rest),
        Some("ron") => ron(rest),
        Some("bake") => bake(rest),
        _ => Err(fail(
            "sizing_usage",
            "usage: bake_sizing plan|sweep|ron|bake [--level bore12|small] [--layout defaults|doc-example|recommended] [knobs]",
        )),
    }
}

fn sized_report(
    level: &SyntheticLevel,
    name: &str,
    layout: &SizingLayout,
) -> Result<(SizingReport, f64), Failure> {
    let started = Instant::now();
    let job = level.job(layout).map_err(|e| {
        fail(
            "sizing_build_failed",
            format!("{name}: {e} (path {:?}, code {})", e.path, e.code),
        )
    })?;
    let report = size_job(&job).map_err(|e| fail("sizing_pack_failed", format!("{name}: {e}")))?;
    Ok((report, started.elapsed().as_secs_f64()))
}

fn sized(level: &SyntheticLevel, name: &str, layout: &SizingLayout) -> Result<Value, Failure> {
    let (report, seconds) = sized_report(level, name, layout)?;
    Ok(json!({
        "ok": true,
        "level": level.name,
        "layout_name": name,
        "layout": layout,
        "sizing": report,
        "plan_seconds": seconds,
    }))
}

fn plan(args: &[String]) -> Result<(), Failure> {
    let level = level(args)?;
    let (name, layout) = layout(args)?;
    println!("{}", sized(&level, &name, &layout)?);
    Ok(())
}

/// `n` with a comma every three digits.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One row of the research page's layout table (`docs/research/bake-sizing-bore-twelve.md` § 3).
fn markdown_row(name: &str, r: &SizingReport) -> String {
    let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
    let share = 100.0 * r.volume.runtime as f64 / r.portable_limit.max(1) as f64;
    let under = 100.0 * r.under_terrain as f64 / r.probes.max(1) as f64;
    format!(
        "| `{name}` | {} | {} | {} | {} | {} | {:.2} | {} ({under:.0} %) | {:.1} | {share:.0} % | {:.2} | {:.2} | {:.0} |",
        grouped(r.probes as u64),
        grouped(r.terrain_probes as u64),
        grouped(r.interior_probes as u64),
        grouped(r.cluster_probes as u64),
        r.bricks,
        r.auto_scale.unwrap_or(1.0),
        grouped(r.under_terrain as u64),
        mib(r.volume.runtime),
        r.rays.main as f64 / 1e9,
        r.rays.relocation_max as f64 / 1e9,
        mib(r.host_peak_estimate),
    )
}

fn sweep(args: &[String]) -> Result<(), Failure> {
    let level = level(args)?;
    let markdown = args.iter().any(|a| a == "--markdown");
    if markdown {
        println!(
            "| Layout | Probes | Terrain | Interiors | Clusters | Bricks | Auto scale | Under terrain (stored, not traced) | Volume MiB | Share of 128 MiB | Main rays (G) | Relocation rays, max (G) | Host estimate MiB |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    }
    for (name, layout) in sweep_layouts() {
        eprintln!("bake_sizing: {name}");
        if markdown {
            let (report, _) = sized_report(&level, &name, &layout)?;
            println!("{}", markdown_row(&name, &report));
        } else {
            println!("{}", sized(&level, &name, &layout)?);
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| fail("sizing_write_failed", format!("{}: {e}", path.display())))
}

fn ron(args: &[String]) -> Result<(), Failure> {
    let level = level(args)?;
    let (name, layout) = layout(args)?;
    let out = flag(args, "--out").ok_or_else(|| fail("sizing_usage", "ron needs --out FILE"))?;
    let text = level.bake_ron(&layout);
    write_atomic(Path::new(out), text.as_bytes())?;
    println!(
        "{}",
        json!({ "ok": true, "level": level.name, "layout_name": name, "out": out, "bytes": text.len() })
    );
    Ok(())
}

fn bake(args: &[String]) -> Result<(), Failure> {
    let level = level(args)?;
    let (name, layout) = layout(args)?;
    let full = SizingLayout::preset(&name).unwrap_or_else(SizingLayout::recommended);
    let job = level
        .job(&layout)
        .map_err(|e| fail("sizing_build_failed", format!("{e} (code {})", e.code)))?;
    let sizing = size_job(&job).map_err(|e| fail("sizing_pack_failed", e.to_string()))?;
    let device = regia_render::RenderDevice::headless().map_err(|e| fail("sizing_no_gpu", e.to_string()))?;
    let tracer = PathTracer::new(&device).map_err(|e| fail("sizing_no_gpu", e.to_string()))?;
    let info = device.adapter.get_info();
    eprintln!(
        "bake_sizing: {} probes ({} live) on {} ({:?})",
        sizing.probes, sizing.rays.live_probes, info.name, info.backend
    );
    let started = Instant::now();
    let mut mark = started;
    let mut stages: Vec<(String, f64)> = Vec::new();
    let mut gpu_peak = 0u64;
    let mut gpu_known = false;
    let bake = bake_gi_volume(
        &tracer,
        &job.scene,
        &job.layout,
        &job.settings,
        |stage, done, total| {
            let memory = regia_render::memory::memory_report(&device);
            gpu_known |= memory.available;
            gpu_peak = gpu_peak.max(memory.allocated_bytes);
            if done == total {
                let now = Instant::now();
                stages.push((stage.to_string(), (now - mark).as_secs_f64()));
                mark = now;
                eprintln!("  {stage}: {total} probes");
            }
            true
        },
    )
    .map_err(|e| fail("sizing_bake_failed", e.to_string()))?;
    let seconds = started.elapsed().as_secs_f64();
    let traced: f64 = stages.iter().map(|s| s.1).sum();
    let bytes = bake
        .volume
        .to_bytes()
        .map_err(|e| fail("sizing_write_failed", e.to_string()))?;
    if let Some(out) = flag(args, "--out") {
        write_atomic(Path::new(out), &bytes)?;
    }
    // Scale each stage to the full layout's rays: relocation traces scale with the rays per
    // probe, the main trace with rays × samples; the rest (filter, encode) does not change.
    let ray_ratio = f64::from(full.rays_per_probe) / f64::from(layout.rays_per_probe.max(1));
    let sample_ratio = f64::from(full.samples) / f64::from(layout.samples.max(1));
    let projected: f64 = stages
        .iter()
        .map(|(stage, s)| {
            if stage == "bake" {
                s * ray_ratio * sample_ratio
            } else {
                s * ray_ratio
            }
        })
        .sum::<f64>()
        + (seconds - traced);
    let main_seconds: f64 = stages.iter().filter(|s| s.0 == "bake").map(|s| s.1).sum();
    let summary = json!({
        "ok": true,
        "level": level.name,
        "layout_name": name,
        "layout": layout,
        "device": format!("{} ({:?})", info.name, info.backend),
        "seconds": seconds,
        "stage_seconds": stages.iter().map(|(s, t)| json!({ "stage": s, "seconds": t })).collect::<Vec<_>>(),
        "filter_encode_seconds": seconds - traced,
        "main_rays_per_second": if main_seconds > 0.0 { bake.report.rays_traced as f64 / main_seconds } else { 0.0 },
        "projected_seconds_at_layout_rays": projected,
        "report": bake.report,
        "file_bytes": bytes.len(),
        "gpu_peak_allocated_bytes": gpu_known.then_some(gpu_peak),
        "host_peak_rss_bytes": regia_render::memory::process_peak_rss_bytes(),
        "sizing": sizing,
    });
    println!("{summary}");
    Ok(())
}
