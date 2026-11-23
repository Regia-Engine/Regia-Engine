// SPDX-License-Identifier: MPL-2.0
//! `pt_trace`: trace a validation scene to EXR + AgX preview, or diff two traces.
//!
//! ```text
//! cargo run --release -p regia_pt --example pt_trace -- trace --scene cornell --size 512x512 \
//!     --spp 4096 --seed 1 --out cornell.exr [--png cornell.png] [--ev100 3]
//! cargo run --release -p regia_pt --example pt_trace -- diff a.exr b.exr [--aov beauty]
//! ```
//!
//! `diff` prints the largest and mean per-pixel relative difference (spec 27 § 1.5 item 6:
//! lavapipe vs RADV vs NVIDIA within 1e-3 relative at 256 spp).

use std::path::PathBuf;
use std::process::ExitCode;

use regia_pt::{Aov, PathTracer, PtMaterial, PtSettings, scenes};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pt_trace: {e}");
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

fn parse<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    match flag(args, name) {
        Some(v) => v.parse().map_err(|_| format!("{name}: bad value {v}")),
        None => Ok(default),
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("trace") => trace(&args[1..]),
        Some("diff") => diff(&args[1..]),
        _ => Err(
            "usage: pt_trace trace --scene <cornell|cornell_room|furnace> ... | pt_trace diff a.exr b.exr"
                .into(),
        ),
    }
}

fn trace(args: &[String]) -> Result<(), String> {
    let (w, h) = match flag(args, "--size").and_then(|s| s.split_once('x')) {
        Some((a, b)) => (
            a.parse().map_err(|_| "bad --size")?,
            b.parse().map_err(|_| "bad --size")?,
        ),
        None => (256u32, 256u32),
    };
    let scene = match flag(args, "--scene").unwrap_or("cornell") {
        "cornell" => scenes::cornell_box(w, h, true),
        "cornell_room" => scenes::cornell_box(w, h, false),
        "furnace" => scenes::furnace_sphere(PtMaterial::lambert([1.0; 3]), w),
        other => return Err(format!("unknown scene {other}")),
    };
    let settings = PtSettings {
        spp: parse(args, "--spp", 256)?,
        max_bounces: parse(args, "--bounces", 12)?,
        seed: parse(args, "--seed", 1)?,
        samples_per_submit: 16,
        ..PtSettings::default()
    };
    let out = PathBuf::from(flag(args, "--out").ok_or("--out <file.exr> is required")?);
    let device = regia_render::RenderDevice::headless().map_err(|e| e.to_string())?;
    let info = device.adapter.get_info();
    println!("device {} ({:?})", info.name, info.backend);
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    let img = tracer.render(&scene, &settings).map_err(|e| e.to_string())?;
    img.write_exr(&out, &[]).map_err(|e| e.to_string())?;
    img.write_metadata_json(&out.with_extension("json"))
        .map_err(|e| e.to_string())?;
    if let Some(png) = flag(args, "--png") {
        let ev: f32 = parse(args, "--ev100", 3.0)?;
        img.write_preview_png(&PathBuf::from(png), Aov::Beauty, ev)
            .map_err(|e| e.to_string())?;
    }
    println!(
        "{} spp at {w}x{h} in {:.1} s → {} (rejected samples {})",
        img.spp,
        started.elapsed().as_secs_f32(),
        out.display(),
        img.metadata.rejected_samples
    );
    Ok(())
}

fn diff(args: &[String]) -> Result<(), String> {
    let (a, b) = match args {
        [a, b, ..] => (PathBuf::from(a), PathBuf::from(b)),
        _ => return Err("diff needs two EXR paths".into()),
    };
    let aov = flag(args, "--aov").unwrap_or("beauty");
    let ia = regia_pt::compare::load(&a, aov).map_err(|e| e.to_string())?;
    let ib = regia_pt::compare::load(&b, aov).map_err(|e| e.to_string())?;
    if (ia.width, ia.height) != (ib.width, ib.height) {
        return Err("sizes differ".into());
    }
    let (mut worst, mut sum, mut over) = (0.0f64, 0.0f64, 0usize);
    for (p, q) in ia.rgb.iter().zip(&ib.rgb) {
        for k in 0..3 {
            let (x, y) = (f64::from(p[k]), f64::from(q[k]));
            let rel = (x - y).abs() / x.abs().max(y.abs()).max(1e-6);
            worst = worst.max(rel);
            sum += rel;
            if rel > 1e-3 {
                over += 1;
            }
        }
    }
    let n = (ia.rgb.len() * 3) as f64;
    println!(
        "{aov}: max relative {worst:.3e}, mean relative {:.3e}, channels over 1e-3: {over} of {}",
        sum / n,
        ia.rgb.len() * 3
    );
    Ok(())
}
