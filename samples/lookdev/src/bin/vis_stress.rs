// SPDX-License-Identifier: MIT-0
//! `vis_stress`: the dense-scatter stress scene drawn forward and through the visibility buffer
//! (`lookdev::vis_stress`, `docs/specs/27` § 4.2 stage 3).
//!
//! ```text
//! cargo build --release -p regia_cli -p lookdev --bin regia --bin vis_stress
//! [REGIA_DO27_CONTENT=<do27-content>] target/release/vis_stress
//!     [--project DIR] [--instances 150000] [--radius 450] [--size 2560x1440] [--warmup 60]
//!     [--frames 120] [--dolly-frames 120] [--dolly-size 1280x720] [--png-dir DIR] [--no-heroes]
//!     [--threshold-px 1] [--shadow-threshold-px 1] [--forward-shadows] [--held-only]
//! ```
//!
//! The scene is written to a scratch project (`--project`, default `<tmp>/regia-vis-stress`)
//! and cooked by the `regia` CLI for this target dir (built on demand, or `REGIA_CLI`):
//! meshes and `.fmlt` meshlets come back from the pak, so this binary links no importer code.
//!
//! Prints GPU frame time (median, p90), the geometry passes, wall and CPU time for both paths,
//! held and driving, the speed-up, the SSIM of the two last frames, and the motion metrics of a
//! read-back dolly (flicker, pop count, difference band).

use std::process::ExitCode;
use std::time::Instant;

use lookdev::LookdevError;
use lookdev::vis_stress::{
    StressOptions, StressTiming, VisPath, build_scene, dolly_frames, median, motion_compare, time_ab,
    write_png,
};
use regia_render::{RenderDevice, VisBufferSettings};

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn size(args: &[String], flag: &str, default: (u32, u32)) -> (u32, u32) {
    value(args, flag)
        .and_then(|v| v.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or(default)
}

/// SSIM of two RGBA8 sRGB frames (`regia_render::compare::ssim` on the encoded values).
fn ssim_rgba8(a: &[u8], b: &[u8], size: (u32, u32)) -> f32 {
    let encode = |px: &[u8]| -> Vec<[f32; 3]> {
        px.as_chunks::<4>()
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
    };
    regia_render::compare::ssim(&encode(a), &encode(b), size.0, size.1)
}

fn percentile(values: &[f32], p: f32) -> f32 {
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    v.get(((v.len() as f32 - 1.0) * p).round().max(0.0) as usize)
        .copied()
        .unwrap_or(0.0)
}

fn report(name: &str, t: &StressTiming) {
    println!(
        "  {name:<14} gpu p50 {:>7.3} ms  p90 {:>7.3}  p99 {:>7.3}  max {:>7.3}  mean {:>7.3}  | shadows {:>6.3}  prepass {:>6.3}  cull+vis {:>6.3}  \
         forward+resolve {:>6.3}  | wall p50 {:>7.3}  cpu {:>7.3}",
        median(&t.gpu_ms),
        percentile(&t.gpu_ms, 0.9),
        percentile(&t.gpu_ms, 0.99),
        percentile(&t.gpu_ms, 1.0),
        t.gpu_ms.iter().sum::<f32>() / t.gpu_ms.len().max(1) as f32,
        t.shadows_ms,
        t.prepass_ms,
        t.cull_ms,
        t.forward_ms,
        median(&t.wall_ms),
        t.cpu_ms,
    );
    println!(
        "  {:<14} host mesh list {:.2} ms; GPU: {}",
        "", t.build_ms, t.last_report
    );
    if let Some([work, clusters, dropped, dropped_work]) = t.shadow_clusters {
        println!(
            "  {:<14} shadow jobs: {work} work items, {clusters} clusters drawn, {dropped} + {dropped_work} dropped",
            ""
        );
    }
    if t.vis.frames > 0 {
        println!(
            "  {:<14} {} instances, {} candidate clusters, {} clusters drawn, {} meshes resident ({:.1} MB), {} declined",
            "",
            t.vis.instances,
            t.vis.candidate_clusters,
            t.clusters.unwrap_or(0),
            t.vis.resident_meshes,
            t.vis.geometry_bytes as f64 / 1.0e6,
            t.vis.declined_meshes,
        );
    }
}

fn read_png(path: &std::path::Path) -> Option<Vec<u8>> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some(buf)
}

fn main_inner(args: &[String]) -> Result<(), LookdevError> {
    // `VIS_TRACE=1`: print each `regia_render` span's busy time when it closes (CPU profile).
    if std::env::var_os("VIS_TRACE").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "regia_render=info,regia_terrain=info",
            ))
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .try_init();
    }
    let mut options = StressOptions::default();
    if let Some(v) = value(args, "--instances").and_then(|v| v.parse().ok()) {
        options.instances = v;
    }
    if let Some(v) = value(args, "--radius").and_then(|v| v.parse().ok()) {
        options.radius_m = v;
    }
    let frame_size = size(args, "--size", (2560, 1440));
    let dolly_size = size(args, "--dolly-size", (1280, 720));
    let warmup = value(args, "--warmup").and_then(|v| v.parse().ok()).unwrap_or(60);
    let frames = value(args, "--frames")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let dolly_count = value(args, "--dolly-frames")
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let png_dir = value(args, "--png-dir").map(std::path::PathBuf::from);
    let threshold = value(args, "--threshold-px").and_then(|v| v.parse().ok());
    let content = if args.iter().any(|a| a == "--no-heroes") {
        None
    } else {
        lookdev::do27::content_dir()
    };
    if let Some(dir) = &png_dir {
        std::fs::create_dir_all(dir).map_err(|source| LookdevError::Io {
            path: dir.display().to_string(),
            source,
        })?;
    }

    let project = value(args, "--project").map_or_else(
        || std::env::temp_dir().join("regia-vis-stress"),
        std::path::PathBuf::from,
    );
    let shadow_threshold = value(args, "--shadow-threshold-px").and_then(|v| v.parse().ok());

    let device = RenderDevice::headless()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let started = Instant::now();
    let scene = build_scene(&options, content.as_deref(), &project)?;
    println!(
        "scene: {} rocks over r = {} m, {} hero meshes, {:.1} M triangles at full detail; {} meshes with cooked \
         meshlets, {} without (project {}, written + cooked + loaded in {:.1} s)",
        options.instances,
        options.radius_m,
        scene.hero_count(),
        scene.full_triangles as f64 / 1.0e6,
        scene.meshlet_meshes.0,
        scene.meshlet_meshes.1,
        project.display(),
        started.elapsed().as_secs_f32()
    );
    let defaults = VisBufferSettings::default();
    let vis_path = VisPath {
        settings: VisBufferSettings {
            lod_threshold_px: threshold.unwrap_or(defaults.lod_threshold_px),
            shadow_lod_threshold_px: shadow_threshold.unwrap_or(defaults.shadow_lod_threshold_px),
            cluster_shadows: !args.iter().any(|a| a == "--forward-shadows"),
            ..defaults
        },
    };
    let png = |name: &str| png_dir.as_ref().map(|d| d.join(name));
    let runs: &[(&str, bool)] = if args.iter().any(|a| a == "--held-only") {
        &[("held", false)]
    } else {
        &[("held", false), ("driving", true)]
    };
    for &(label, dolly) in runs {
        println!(
            "\n{label}, {}x{}, {frames} frames after {warmup}:",
            frame_size.0, frame_size.1
        );
        let t0 = Instant::now();
        let (fp, vp) = (
            png(&format!("stress_{label}_forward.png")),
            png(&format!("stress_{label}_vis.png")),
        );
        let (forward, vis) = time_ab(
            &device,
            &scene,
            &vis_path,
            frame_size,
            warmup,
            frames,
            dolly,
            fp.as_deref().zip(vp.as_deref()),
        )?;
        report("forward", &forward);
        report("visibility", &vis);
        println!(
            "  speed-up: GPU frame {:.2}×, geometry passes (prepass + cull/vis + forward/resolve) {:.2}×, wall {:.2}×  \
             (both runs interleaved, {:.1} s incl. cooking)",
            median(&forward.gpu_ms) / median(&vis.gpu_ms).max(1e-3),
            (forward.prepass_ms + forward.cull_ms + forward.forward_ms)
                / (vis.prepass_ms + vis.cull_ms + vis.forward_ms).max(1e-3),
            median(&forward.wall_ms) / median(&vis.wall_ms).max(1e-3),
            t0.elapsed().as_secs_f32(),
        );
        if let (Some(a), Some(b)) = (
            png(&format!("stress_{label}_forward.png")),
            png(&format!("stress_{label}_vis.png")),
        ) && let (Some(a), Some(b)) = (read_png(&a), read_png(&b))
        {
            println!(
                "  SSIM forward vs visibility (last frame): {:.4}",
                ssim_rgba8(&a, &b, frame_size)
            );
        }
    }

    for temporal in [false, true] {
        if dolly_count < 2 {
            break;
        }
        let taa = if temporal { "TAA on" } else { "TAA off" };
        println!(
            "\nmotion ({taa}): {dolly_count} read-back frames at {}x{} driving 15 m/s",
            dolly_size.0, dolly_size.1
        );
        let forward = dolly_frames(&device, &scene, None, dolly_size, dolly_count, temporal)?;
        let vis = dolly_frames(
            &device,
            &scene,
            Some(&vis_path),
            dolly_size,
            dolly_count,
            temporal,
        )?;
        let m = motion_compare(&forward, &vis, dolly_size);
        println!(
            "  flicker forward {:.5}  visibility {:.5}  | pops {} (worst jump {:.4} % of blocks)  | \
             mean |Δluma| to forward {:.2}–{:.2} levels",
            m.forward_flicker,
            m.vis_flicker,
            m.pops,
            m.worst_jump * 100.0,
            m.min_mad,
            m.max_mad
        );
        let ssim: Vec<f32> = forward
            .iter()
            .zip(&vis)
            .map(|(a, b)| ssim_rgba8(a, b, dolly_size))
            .collect();
        if !ssim.is_empty() {
            let min = ssim.iter().copied().fold(f32::MAX, f32::min);
            let mean = ssim.iter().sum::<f32>() / ssim.len() as f32;
            println!("  per-frame SSIM forward vs visibility: min {min:.4}, mean {mean:.4}");
        }
        if let Some(dir) = &png_dir {
            let tag = if temporal { "taa" } else { "notaa" };
            for i in [0, dolly_count as usize / 2, dolly_count as usize - 1] {
                if let (Some(a), Some(b)) = (forward.get(i), vis.get(i)) {
                    write_png(
                        &dir.join(format!("dolly_{tag}_{i:03}_forward.png")),
                        dolly_size,
                        a,
                    )?;
                    write_png(&dir.join(format!("dolly_{tag}_{i:03}_vis.png")), dolly_size, b)?;
                }
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match main_inner(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vis_stress: {err}");
            ExitCode::from(1)
        }
    }
}
