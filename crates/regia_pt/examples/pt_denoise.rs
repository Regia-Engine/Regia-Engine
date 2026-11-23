// SPDX-License-Identifier: MPL-2.0
//! `pt_denoise`: an OIDN preview of a low-spp trace, optionally scored against a converged
//! reference (oss-survey3 item 10 acceptance: 64 spp + OIDN within FLIP 0.1 of 1024 spp in under
//! 10 % of its time). Needs the `denoise` feature and the native OIDN library.
//!
//! ```text
//! OIDN_DIR=/path/to/oidn LD_LIBRARY_PATH=/path/to/oidn/lib \
//! cargo run --release -p regia_pt --features denoise --example pt_denoise -- \
//!     --input shot_64spp_pt.exr --out shot_preview.exr [--png shot_preview.png] \
//!     [--reference shot_1024spp_pt.exr] [--ev100 <auto>] [--no-aux] \
//!     [--map flip.png] [--json scores.json]
//! ```
//!
//! `--ev100` defaults to the reference's (else the input's) log-average luminance, the meter
//! `lookdev`'s `pt_plates` scores the parity plates with.

use std::path::PathBuf;
use std::process::ExitCode;

use regia_pt::denoise::{DenoiseOptions, denoise};
use regia_pt::{Aov, PtImage};
use regia_render::compare;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pt_denoise: {e}");
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

fn metered_ev100(beauty: &[[f32; 3]]) -> f32 {
    let mut s = 0.0f64;
    for c in beauty {
        let y = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        s += f64::from(y.max(1e-4)).ln();
    }
    regia_render::ev100_from_luminance((s / beauty.len().max(1) as f64).exp() as f32)
}

fn run(args: &[String]) -> Result<(), String> {
    let input = PathBuf::from(flag(args, "--input").ok_or("--input <trace.exr> is required")?);
    let out = PathBuf::from(flag(args, "--out").ok_or("--out <preview.exr> is required")?);
    let options = DenoiseOptions {
        use_aux: !args.iter().any(|a| a == "--no-aux"),
        ..DenoiseOptions::default()
    };
    let noisy = PtImage::read_exr(&input).map_err(|e| e.to_string())?;
    let preview = denoise(&noisy, &options).map_err(|e| e.to_string())?;
    preview.write_exr(&out, &[]).map_err(|e| e.to_string())?;
    preview
        .write_metadata_json(&out.with_extension("json"))
        .map_err(|e| e.to_string())?;
    let reference = match flag(args, "--reference") {
        Some(r) => Some((
            PathBuf::from(r),
            PtImage::read_exr(&PathBuf::from(r)).map_err(|e| e.to_string())?,
        )),
        None => None,
    };
    let ev100 = match flag(args, "--ev100") {
        Some(v) => v.parse().map_err(|_| format!("--ev100: bad value {v}"))?,
        None => metered_ev100(reference.as_ref().map_or(&noisy.beauty, |(_, r)| &r.beauty)),
    };
    if let Some(png) = flag(args, "--png") {
        preview
            .write_preview_png(&PathBuf::from(png), Aov::Beauty, ev100)
            .map_err(|e| e.to_string())?;
    }
    let preview_seconds = noisy.metadata.wall_seconds + preview.metadata.denoise_seconds;
    println!(
        "{}: {} spp traced in {:.2} s + {} in {:.2} s = {:.2} s",
        input.display(),
        noisy.spp,
        noisy.metadata.wall_seconds,
        preview.metadata.denoiser,
        preview.metadata.denoise_seconds,
        preview_seconds
    );
    let Some((ref_path, reference)) = reference else {
        return Ok(());
    };
    // The § 2 tool on files, so the denoiser mark round-trips exactly as an agent would see it.
    let loaded_ref = regia_pt::compare::load(&ref_path, "beauty").map_err(|e| e.to_string())?;
    let loaded_noisy = regia_pt::compare::load(&input, "beauty").map_err(|e| e.to_string())?;
    let loaded_preview = regia_pt::compare::load(&out, "beauty").map_err(|e| e.to_string())?;
    let (raw, _) = regia_pt::compare::compare_images(&loaded_ref, &loaded_noisy, ev100, compare::DEFAULT_PPD)
        .map_err(|e| e.to_string())?;
    let (den, flip) =
        regia_pt::compare::compare_images(&loaded_ref, &loaded_preview, ev100, compare::DEFAULT_PPD)
            .map_err(|e| e.to_string())?;
    let refused =
        regia_pt::compare::compare_images(&loaded_preview, &loaded_ref, ev100, compare::DEFAULT_PPD);
    if let Some(map) = flag(args, "--map") {
        regia_pt::output::write_png(
            &PathBuf::from(map),
            den.width,
            den.height,
            &compare::flip_heatmap_rgba8(&flip.map),
        )
        .map_err(|e| e.to_string())?;
    }
    let ratio = preview_seconds / reference.metadata.wall_seconds.max(1e-9);
    println!(
        "reference {} spp in {:.2} s | ev100 {ev100:.2}\n  raw {} spp: FLIP {:.4} (p95 {:.4}) relRMSE {:.4} SSIM {:.4}\n  \
         OIDN preview: FLIP {:.4} (p95 {:.4}) relRMSE {:.4} SSIM {:.4} | time {:.2} % of the reference\n  \
         preview as the reference: {}",
        reference.spp,
        reference.metadata.wall_seconds,
        noisy.spp,
        raw.flip_mean,
        raw.flip_p95,
        raw.rel_rmse,
        raw.ssim,
        den.flip_mean,
        den.flip_p95,
        den.rel_rmse,
        den.ssim,
        ratio * 100.0,
        match &refused {
            Err(e) => format!("refused ({e})"),
            Ok(_) => "ACCEPTED (bug)".into(),
        }
    );
    if let Some(json) = flag(args, "--json") {
        let text = format!(
            "{{\"input\":\"{}\",\"reference\":\"{}\",\"ev100\":{ev100:.3},\"spp\":{},\"reference_spp\":{},\
             \"trace_seconds\":{:.3},\"denoise_seconds\":{:.3},\"reference_seconds\":{:.3},\"time_ratio\":{ratio:.4},\
             \"denoiser\":\"{}\",\"raw\":{},\"denoised\":{},\"refused_as_reference\":{}}}",
            input.display(),
            ref_path.display(),
            noisy.spp,
            reference.spp,
            noisy.metadata.wall_seconds,
            preview.metadata.denoise_seconds,
            reference.metadata.wall_seconds,
            preview.metadata.denoiser,
            raw.to_json(),
            den.to_json(),
            refused.is_err()
        );
        std::fs::write(json, text).map_err(|e| e.to_string())?;
    }
    if refused.is_ok() {
        return Err("the compare tool accepted a denoised reference".into());
    }
    Ok(())
}
