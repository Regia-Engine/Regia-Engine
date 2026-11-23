// SPDX-License-Identifier: MIT-0
//! `pt_plates`: reference path-traced ground truth for parity plates and the real-time frame's
//! error against it (`docs/specs/27-renderer-excellence.md` § 2, `lookdev::pt_plates`).
//!
//! ```text
//! pt_plates --out <dir> [--shots switchbacks_wide,…|parity] [--width 1280] [--height 720]
//!           [--spp 2048] [--bounces 8] [--frames 6] [--albedo-m 2] [--seed 1]
//!           [--resume]
//! pt_plates --evidence <root> [same flags]   # all twelve plates into <root>/<date>/ + README
//! ```
//!
//! With `REGIA_DO27_CONTENT` the shots are DO27's maps and plate cameras. The tracer carries
//! the frame's media (air, height fog), sky bodies and the terrain shader's albedo, so the real-
//! time frame keeps its fog and aerial perspective. `--resume` skips shots whose
//! `<shot>_scores.json` already exists (a nightly run picks up where a killed one stopped).
//! `--evidence` is the nightly job: the twelve parity plates at the given settings into a dated
//! folder with `scores.json` and a `README.md` score table.
//!
//! Diagnostics: `PT_PLATES_RT_ONLY=1` writes only the real-time `<shot>_rt.exr` (no trace), to
//! score a raster change against plates traced earlier; `PT_PLATES_MEDIA=air|haze` traces only
//! the planet's air or only the level's height fog (`PT_PLATES_NO_MEDIA=1`: neither). Each shot
//! prints a `CAM` line (eye, target, vertical FOV, near/far, sun, fog) for offline analysis.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::look_r2::{CameraOverride, R2Options, R2Planets, R2Shot, ShotHook, render_shot_with};
use lookdev::pt_plates::{PlateScores, PlateTrace, trace_and_score};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("pt_plates: {err}");
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

/// Civil date (UTC) of a Unix day number (Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (y, m, d) = civil(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LookdevError + '_ {
    move |source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn run(args: &[String]) -> Result<(), LookdevError> {
    let mut out: Option<PathBuf> = None;
    let mut evidence: Option<PathBuf> = None;
    let mut shots = vec![
        R2Shot::SwitchbacksWide,
        R2Shot::SwitchbacksClose,
        R2Shot::BoreTwelveMid,
    ];
    let mut options = R2Options {
        width: 1280,
        height: 720,
        frames: 6,
        terrain_resolution: 1025,
        timing: false,
    };
    let mut trace = PlateTrace {
        spp: 2048,
        max_bounces: 8,
        seed: 1,
    };
    let mut albedo = regia_pt::terrain_bake::AlbedoBake::default();
    let mut resume = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out")?)),
            "--evidence" => evidence = Some(PathBuf::from(value(args, i, "--evidence")?)),
            "--shots" => {
                let list = value(args, i, "--shots")?;
                shots = if list == "parity" {
                    R2Shot::PARITY.to_vec()
                } else {
                    list.split(',')
                        .map(|n| {
                            R2Shot::from_name(n.trim())
                                .ok_or_else(|| LookdevError::Config(format!("unknown shot {n}")))
                        })
                        .collect::<Result<_, _>>()?
                };
            }
            "--width" => options.width = number(args, i, "--width")?,
            "--height" => options.height = number(args, i, "--height")?,
            "--frames" => options.frames = number(args, i, "--frames")?,
            "--spp" => trace.spp = number(args, i, "--spp")?,
            "--bounces" => trace.max_bounces = number(args, i, "--bounces")?,
            "--seed" => trace.seed = number(args, i, "--seed")?,
            "--albedo-m" => albedo.texel_m = number(args, i, "--albedo-m")?,
            "--resume" => {
                resume = true;
                i += 1;
                continue;
            }
            other => return Err(LookdevError::Config(format!("unknown flag {other}"))),
        }
        i += 2;
    }
    let date = today();
    if let Some(root) = &evidence {
        if !args.iter().any(|a| a == "--shots") {
            shots = R2Shot::PARITY.to_vec();
        }
        out = Some(root.join(&date));
    }
    let out =
        out.ok_or_else(|| LookdevError::Config("--out <dir> or --evidence <root> is required".into()))?;
    std::fs::create_dir_all(&out).map_err(io(&out))?;
    let content = lookdev::do27::content_dir().is_some();
    let device = if content {
        regia_terrain::gpu::headless_bcn_device()?
    } else {
        regia_render::RenderDevice::headless()?
    };
    let info = device.adapter.get_info();
    println!(
        "pt_plates: device {} ({:?}), DO27 content: {content}, {} shots → {}",
        info.name,
        info.backend,
        shots.len(),
        out.display()
    );
    let tracer = regia_pt::PathTracer::new(&device).map_err(|e| LookdevError::Config(e.to_string()))?;
    let mut planets = R2Planets::default();
    let mut all: Vec<String> = Vec::new();
    let mut rows: Vec<PlateScores> = Vec::new();
    for shot in shots {
        let scores_path = out.join(format!("{}_scores.json", shot.name()));
        if resume && scores_path.exists() {
            println!("{}: done earlier, skipped (--resume)", shot.name());
            all.push(std::fs::read_to_string(&scores_path).map_err(io(&scores_path))?);
            continue;
        }
        let started = std::time::Instant::now();
        let mut result: Option<PlateScores> = None;
        // Diagnostic: the real-time frame only, against plates traced earlier.
        let rt_only = std::env::var_os("PT_PLATES_RT_ONLY").is_some();
        let mut run_hook = |capture: lookdev::look_r2::ShotCapture<'_>| -> Result<(), LookdevError> {
            let f = capture.frame;
            println!(
                "CAM {} eye {:?} target {:?} fov_y {} near {} far {} sun {:?} fog {:?}",
                shot.name(),
                f.eye,
                f.target,
                f.fov_y,
                f.near,
                f.far,
                capture.toward_sun,
                capture.outdoor.fog
            );
            if rt_only {
                lookdev::pt_plates::write_linear_exr(
                    &out.join(format!("{}_rt.exr", shot.name())),
                    f.width,
                    f.height,
                    &[
                        ("full", &capture.hdr_full.rgb),
                        ("direct", &capture.hdr_direct.rgb),
                    ],
                )?;
                return Err(LookdevError::Config("rt-only".into()));
            }
            result = Some(trace_and_score(
                &tracer,
                &capture,
                shot.name(),
                trace,
                &out,
                content,
            )?);
            Ok(())
        };
        let rendered = render_shot_with(
            &device,
            &mut planets,
            shot,
            options,
            CameraOverride::default(),
            None,
            Some(ShotHook {
                albedo,
                run: &mut run_hook,
            }),
        );
        if rt_only {
            // The hook stops the render after the capture; any other error is real.
            match rendered {
                Err(LookdevError::Config(m)) if m == "rt-only" => {}
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            println!(
                "{}: rt only, {:.0} s",
                shot.name(),
                started.elapsed().as_secs_f32()
            );
            continue;
        }
        let (pixels, _) = rendered?;
        // `render_shot_with` already reflects DO27 content once (the sim frame is
        // left-handed), so the LDR it returns is the same orientation as the scored HDR and
        // the path tracer. Mirroring again here would flip it back.
        let ldr = pixels;
        regia_pt::output::write_png(
            &out.join(format!("{}_rt_ldr.png", shot.name())),
            options.width,
            options.height,
            &ldr,
        )
        .map_err(|e| LookdevError::Config(e.to_string()))?;
        let scores = result.ok_or_else(|| LookdevError::Config("the capture hook did not run".into()))?;
        println!(
            "{}: beauty FLIP {:.3} (p95 {:.3}) relRMSE {:.3} | direct FLIP {:.3} relRMSE {:.3} | GI FLIP {:.3} (p95 {:.3}) relRMSE {:.3}, mean GI rt/pt {:.3} | shadow GI {} px rt/pt {:.3} relRMSE {:.3} FLIP {:.3} (p95 {:.3}) | {:.0} s",
            shot.name(),
            scores.beauty.flip_mean,
            scores.beauty.flip_p95,
            scores.beauty.rel_rmse,
            scores.direct.flip_mean,
            scores.direct.rel_rmse,
            scores.gi.flip_mean,
            scores.gi.flip_p95,
            scores.gi.rel_rmse,
            scores.gi.mean_test / scores.gi.mean_reference.max(1e-9),
            scores.shadow.pixels,
            scores.shadow.mean_rt / scores.shadow.mean_pt.max(1e-9),
            scores.shadow.rel_rmse,
            scores.shadow.flip_mean,
            scores.shadow.flip_p95,
            started.elapsed().as_secs_f32()
        );
        if let Some(p) = &scores.probes {
            println!(
                "{}: probe vs PT irradiance/pi: shadow {:.4} vs {:.4} (x{:.2}), lit {:.4} vs {:.4} (x{:.2}); shadow albedo rt {:.3} pt {:.3}; shadow r/g probe {:.2} pt {:.2}, GI rt {:?} pt {:?} (rt/pt {:.2}, r/g rt {:.2} pt {:.2}); point {:?}",
                shot.name(),
                p.shadow_probe,
                p.shadow_pt,
                p.shadow_probe / p.shadow_pt.max(1e-9),
                p.lit_probe,
                p.lit_pt,
                p.lit_probe / p.lit_pt.max(1e-9),
                p.shadow_albedo_rt,
                p.shadow_albedo_pt,
                p.shadow_probe_rgb[0] / p.shadow_probe_rgb[1].max(1e-9),
                p.shadow_pt_rgb[0] / p.shadow_pt_rgb[1].max(1e-9),
                p.shadow_rt_gi_rgb,
                p.shadow_pt_gi_rgb,
                p.shadow_rt_gi_rgb[1] / p.shadow_pt_gi_rgb[1].max(1e-9),
                p.shadow_rt_gi_rgb[0] / p.shadow_rt_gi_rgb[1].max(1e-9),
                p.shadow_pt_gi_rgb[0] / p.shadow_pt_gi_rgb[1].max(1e-9),
                p.point
            );
        }
        all.push(scores.to_json());
        rows.push(scores);
    }
    let path = out.join("scores.json");
    std::fs::write(&path, format!("[{}]", all.join(","))).map_err(io(&path))?;
    println!("wrote {}", path.display());
    if evidence.is_some() {
        // The raw outputs (EXRs, full-size previews) stay out of git; the README, the scores
        // and the contact sheets (converted to JPEG) are the evidence.
        let ignore = out.join(".gitignore");
        std::fs::write(
            &ignore,
            "*\n!.gitignore\n!README.md\n!scores.json\n!*_sheet.jpg\n",
        )
        .map_err(io(&ignore))?;
        let readme = out.join("README.md");
        std::fs::write(&readme, evidence_readme(&date, options, trace, albedo, &rows))
            .map_err(io(&readme))?;
        println!("wrote {}", readme.display());
    }
    Ok(())
}

/// The dated evidence folder's README: settings and the score table (spec 27 § 2 columns).
fn evidence_readme(
    date: &str,
    options: R2Options,
    trace: PlateTrace,
    albedo: regia_pt::terrain_bake::AlbedoBake,
    rows: &[PlateScores],
) -> String {
    let mut s = format!(
        "# Reference path tracer vs real-time, DO27 parity plates ({date})\n\n\
         `pt_plates --evidence` (spec 27 § 2): {}×{}, {} spp, {} bounces, terrain albedo baked at \
         {} m/texel through the terrain shader, media (air + height fog), sky bodies and fill. \
         Per plate: `<shot>_pt.exr` (all AOVs), `<shot>_rt.exr` (real-time full / direct / gi), \
         previews `<shot>_{{pt,rt}}_{{beauty,direct,gi}}.png`, FLIP maps, `<shot>_scores.json`.\n\n\
         | plate | beauty FLIP mean / p95 | beauty rel RMSE | direct FLIP mean | GI in sun shadow rt/pt (px) | GI shadow FLIP mean | spp | s |\n\
         |---|---|---|---|---|---|---|---|\n",
        options.width, options.height, trace.spp, trace.max_bounces, albedo.texel_m
    );
    for r in rows {
        let ratio = if r.shadow.pixels > 0 {
            format!(
                "{:.2}× ({})",
                r.shadow.mean_rt / r.shadow.mean_pt.max(1e-9),
                r.shadow.pixels
            )
        } else {
            "—".into()
        };
        s.push_str(&format!(
            "| {} | {:.3} / {:.3} | {:.3} | {:.3} | {} | {:.3} | {} | {:.0} |\n",
            r.shot,
            r.beauty.flip_mean,
            r.beauty.flip_p95,
            r.beauty.rel_rmse,
            r.direct.flip_mean,
            ratio,
            r.shadow.flip_mean,
            r.pt.spp,
            r.pt.wall_seconds
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::civil;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_733), (2026, 10, 7));
        assert_eq!(civil(-1), (1969, 12, 31));
    }
}
