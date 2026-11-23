// SPDX-License-Identifier: MIT-0
//! `match_levers`: the match bench (`do27_chase --match`, `lookdev::match_scene`) under the
//! forward-pass levers of DO27 tester item gpu-cost-midhigh, interleaved on one device, with a
//! plate per run and its PSNR against the first base run.
//!
//! ```text
//! cargo run --release -p lookdev --example match_levers -- --content <do27-content>
//!     [--size 2560x1440] [--scale 0.87] [--upscaler tsr|taau] [--frames 300] [--warmup 120]
//!     [--rounds 2] [--levers base,vis,noprepass,nolights,noshadowlights,flat,novfx] [--png-dir DIR]
//! ```
//!
//! Levers (each a whole chase run of the same seeded match on Switchbacks):
//!
//! - `base`: the shell's Desktop High frame with the match;
//! - `vis`: rigid opaque meshes through the visibility buffer (`quality.visibility_buffer`, off in
//!   every preset; clusters from `regia_asset_pipeline::build_meshlet_mesh_with_attributes`);
//! - `noprepass`: no depth prepass (the overdraw the prepass removes lands in forward);
//! - `nolights`: the match's 24 local lights off (the clustered light loop's share; their atlas
//!   shadows go too);
//! - `noshadowlights`: the 24 lights kept, none shadowed (the atlas share alone);
//! - `flat`: the match's materials without textures (`match_scene::flat_material`);
//! - `novfx`: no VFX emitters and no decal grid (their share of the translucent and post passes).
//!
//! `base` runs once per round, so base-vs-base PSNR is the run-to-run floor a lever's PSNR is
//! read against. Only `vis` is a candidate default; the others change the image on purpose and
//! price a part of the forward pass.

use std::process::ExitCode;
use std::sync::Arc;

use lookdev::chase::{ChaseOptions, ChaseRun, MeshletSource, run};
use lookdev::look_r2::R2Planets;
use lookdev::{LookdevError, Planet};
use regia_asset_pipeline::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::{LabVertex, MeshletBuilder};

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn cooker() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// PSNR (dB) of two RGBA8 images over RGB; `f64::INFINITY` when identical.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0f64, 0u64);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for k in 0..3 {
            let d = f64::from(pa[k]) - f64::from(pb[k]);
            sum += d * d;
            n += 1;
        }
    }
    if sum == 0.0 || n == 0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / (sum / n as f64)).log10()
}

fn median(run: &ChaseRun, f: &dyn Fn(&lookdev::chase::ChaseSample) -> f32) -> f32 {
    let v: Vec<f32> = run.samples.iter().map(f).collect();
    lookdev::chase::percentile(&v, 0.5)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match main_inner(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("match_levers: {err}");
            ExitCode::from(1)
        }
    }
}

fn main_inner(args: &[String]) -> Result<(), LookdevError> {
    let content = value(args, "--content")
        .map(std::path::PathBuf::from)
        .or_else(lookdev::do27::content_dir)
        .ok_or_else(|| LookdevError::Config("pass --content <dir> or set REGIA_DO27_CONTENT".into()))?;
    let mut base = ChaseOptions {
        frames: value(args, "--frames")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
        warmup: value(args, "--warmup")
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
        match_scene: Some(lookdev::match_scene::MatchOptions::default()),
        ..ChaseOptions::default()
    };
    if let Some((w, h)) = value(args, "--size").and_then(|v| v.split_once('x')) {
        base.width = w.parse().unwrap_or(base.width);
        base.height = h.parse().unwrap_or(base.height);
    }
    if let Some(scale) = value(args, "--scale").and_then(|v| v.parse::<f32>().ok()) {
        base.render_scale = Some(scale);
    }
    base.upscaler = match value(args, "--upscaler") {
        Some("taau") => regia_render::TemporalUpscaler::Taau,
        Some("fsr3") => regia_render::TemporalUpscaler::Fsr3,
        _ if base.render_scale.is_some() => regia_render::TemporalUpscaler::Tsr,
        _ => regia_render::TemporalUpscaler::Taau,
    };
    let rounds: u32 = value(args, "--rounds").and_then(|v| v.parse().ok()).unwrap_or(2);
    let levers: Vec<String> = value(args, "--levers")
        .unwrap_or("base,vis,noprepass,nolights,noshadowlights,flat,novfx")
        .split(',')
        .map(str::to_owned)
        .collect();
    let png_dir = std::path::PathBuf::from(value(args, "--png-dir").unwrap_or("match_levers"));
    std::fs::create_dir_all(&png_dir).map_err(|source| LookdevError::Io {
        path: png_dir.display().to_string(),
        source,
    })?;
    let device = lookdev::chase::runtime_device()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let mut planets = R2Planets::default();
    println!(
        "{:<15} {:>5} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>4} {:>8}",
        "lever",
        "round",
        "frame",
        "shadows",
        "atlas",
        "clusters",
        "prepass",
        "forward",
        "terrain",
        "glass",
        "meshes",
        "post",
        "drawn",
        "inst",
        "cast",
        "lts",
        "psnr"
    );
    let mut reference: Option<Vec<u8>> = None;
    for round in 1..=rounds {
        for lever in &levers {
            let mut options = base.clone();
            let mut m = options.match_scene.unwrap_or_default();
            match lever.as_str() {
                "base" => {}
                "vis" => {
                    options.meshlets = Some(MeshletSource {
                        builder: cooker(),
                        settings: regia_render::VisBufferSettings::default(),
                    });
                }
                "noprepass" => options.off.push("prepass".into()),
                "nolights" => {
                    m.plan.lights = 0;
                    m.plan.shadowed_lights = 0;
                }
                "noshadowlights" => m.plan.shadowed_lights = 0,
                "flat" => m.flat = true,
                "novfx" => {
                    m.plan.emitters = 0;
                    m.plan.decal_grid = 0;
                }
                other => {
                    return Err(LookdevError::Config(format!(
                        "--levers {other}: base|vis|noprepass|nolights|noshadowlights|flat|novfx"
                    )));
                }
            }
            options.match_scene = Some(m);
            let png = png_dir.join(format!("{lever}_r{round}.png"));
            options.png = Some(png.clone());
            let result = run(&device, &mut planets, &content, Planet::Mars, &options)?;
            let image = lookdev::default_look::read_png(&png)?.2;
            let db = match reference.as_ref() {
                Some(r) => psnr(r, &image),
                None => f64::INFINITY,
            };
            if reference.is_none() && lever == "base" {
                reference = Some(image);
            }
            let g = |f: &dyn Fn(&regia_render::GpuTimings) -> f32| median(&result, &|s| f(&s.gpu));
            println!(
                "{lever:<15} {round:>5} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>6.0} {:>6.0} {:>6.0} {:>4.0} {:>8.2}",
                g(&|t| t.frame_ms),
                g(&|t| t.shadows_ms),
                g(&|t| t.shadow_kind_ms[4]),
                g(&|t| t.clusters_ms),
                g(&|t| t.prepass_ms),
                g(&|t| t.forward_ms),
                g(&|t| t.shared_forward_ms),
                g(&|t| t.forward_glass_ms),
                g(&|t| t.forward_ms - t.shared_forward_ms - t.forward_glass_ms),
                g(&|t| t.post_ms),
                median(&result, &|s| s.drawn as f32),
                median(&result, &|s| s.instances as f32),
                median(&result, &|s| s.casters as f32),
                median(&result, &|s| s.local_lights as f32),
                db,
            );
            if lever == "vis" {
                println!(
                    "{:<15} {round:>5} vis: {} instances, {} meshes resident, {} declined",
                    "",
                    result.visbuffer.instances,
                    result.visbuffer.resident_meshes,
                    result.visbuffer.declined_meshes
                );
            }
            if let Some(report) = result.match_report.as_ref()
                && round == 1
                && lever == "base"
            {
                println!("{}", report.line());
            }
        }
    }
    println!(
        "psnr: dB against base round 1's plate (the last pose, read back); base rounds 2+ are the run-to-run floor"
    );
    Ok(())
}
