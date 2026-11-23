// SPDX-License-Identifier: MPL-2.0
//! `cloud_compare`: the raster clouds against the path tracer's cloud field at the standard skies
//! (ADR-2127). Prints one JSON object per line.
//!
//! ```text
//! cargo run --release -p regia_pt --example cloud_compare -- \
//!     [--layers default_045,cumulus] [--width 64] [--height 48] [--spp 32] [--bounces 512] \
//!     [--raster-frames 16] [--seed 3] [--no-ground] [--no-views] [--terms]
//! ```
//!
//! Each view line holds one [`regia_pt::cloud_compare::CloudCase`] with the raster's Desktop High
//! march against two traces: `physical` (albedo 1, droplet phase) and `raster_parameters` (the
//! march's albedo and first-octave phase), plus a `fine` raster transmittance check. Each ground
//! line holds the ground's light under the layer for one sun. `--terms` prints instead, per case,
//! the view's in-scattered light by source (`CloudLightTerms`), the numbers that say which term of
//! the march differs.

use std::process::ExitCode;

use regia_pt::PathTracer;
use regia_pt::cloud_compare::{
    CloudCase, CloudLightTerms, CloudTraceSize, CloudViewMeasure, GroundLightMeasure, RasterMarch,
    compare_ground, light_terms, measure_view, physical_field, raster_parameters_field, raster_view,
    standard_cases, traced_view,
};
use regia_render::RenderDevice;
use serde_json::{Value, json};

fn view_json(m: &CloudViewMeasure) -> Value {
    json!({
        "raster_radiance": m.raster_radiance,
        "pt_radiance": m.pt_radiance,
        "radiance_log2": m.radiance_log2,
        "radiance_block_rms_log2": m.radiance_block_rms_log2,
        "radiance_block_correlation": m.radiance_block_correlation,
        "raster_transmittance": m.raster_transmittance,
        "pt_transmittance": m.pt_transmittance,
        "transmittance_block_rms": m.transmittance_block_rms,
        "transmittance_block_correlation": m.transmittance_block_correlation,
        "tint_red_over_blue": m.tint_red_over_blue,
        "cloudy_blocks": m.cloudy_blocks,
    })
}

fn ground_json(m: &GroundLightMeasure) -> Value {
    json!({
        "raster_sun": m.raster_sun,
        "pt_sun_direct": m.pt_sun_direct,
        "sun_direct_block_rms": m.sun_direct_block_rms,
        "sun_direct_block_correlation": m.sun_direct_block_correlation,
        "pt_sun_all": m.pt_sun_all,
        "raster_sky": m.raster_sky,
        "pt_sky_all": m.pt_sky_all,
        "raster_total": m.raster_total,
        "pt_total": m.pt_total,
        "total_log2": m.total_log2,
        "raster_screen_shade": m.raster_screen_shade,
        "screen_log2": m.screen_log2,
    })
}

fn terms_json(case: &CloudCase, t: &CloudLightTerms) -> Value {
    json!({
        "kind": "terms",
        "layer": case.name,
        "sun_elevation_deg": case.sun_elevation_deg,
        "sun_azimuth_deg": case.sun_azimuth_deg,
        "raster_ambient": t.raster_ambient,
        "raster_sun_single": t.raster_sun_single,
        "raster_sun_full": t.raster_sun_full,
        "pt_sky_raster_phase": t.pt_sky_raster_phase,
        "pt_sky_droplet": t.pt_sky_droplet,
        "pt_sun_single_raster_phase": t.pt_sun_single_raster_phase,
        "pt_sun_all_raster_phase": t.pt_sun_all_raster_phase,
        "pt_sun_all_droplet": t.pt_sun_all_droplet,
    })
}

fn view_line(tracer: &PathTracer, case: &CloudCase, size: &CloudTraceSize) -> Result<Value, String> {
    let device = tracer.device();
    let light = case.light();
    let err = |e: regia_pt::PtError| e.to_string();
    let high = raster_view(device, case, &light, &RasterMarch::DESKTOP_HIGH, size).map_err(err)?;
    let fine = raster_view(device, case, &light, &RasterMarch::FINEST, size).map_err(err)?;
    let (phys, clear) = traced_view(
        tracer,
        case,
        &light,
        physical_field(case, &light),
        size,
        size.max_bounces,
    )
    .map_err(err)?;
    let (par, _) = traced_view(
        tracer,
        case,
        &light,
        raster_parameters_field(case, &light),
        size,
        size.max_bounces,
    )
    .map_err(err)?;
    let (single, _) = traced_view(
        tracer,
        case,
        &light,
        raster_parameters_field(case, &light),
        size,
        1,
    )
    .map_err(err)?;
    let mut single_march = RasterMarch::DESKTOP_HIGH;
    single_march.multiple_scattering = 0.0;
    let raster_single = raster_view(device, case, &light, &single_march, size).map_err(err)?;
    Ok(json!({
        "kind": "view",
        "layer": case.name,
        "sun_elevation_deg": case.sun_elevation_deg,
        "sun_azimuth_deg": case.sun_azimuth_deg,
        "view_elevation_deg": case.view_elevation_deg,
        "sun_lux": light.sun.illuminance,
        "physical": view_json(&measure_view(&high, &phys, &clear)),
        "raster_parameters": view_json(&measure_view(&high, &par, &clear)),
        "fine_march_transmittance": view_json(&measure_view(&fine, &phys, &clear)),
        // The first octave alone (multiple scattering off in the march when the look allows it)
        // against one traced vertex: single scattering on both sides.
        "single_scattering": view_json(&measure_view(&raster_single, &single, &clear)),
    }))
}

fn run() -> Result<(), String> {
    let mut size = CloudTraceSize::REPORT;
    let mut layers: Option<Vec<String>> = None;
    let mut ground = true;
    let mut terms = false;
    let mut views = true;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{a} needs a value"));
        let mut number = |target: &mut u32| -> Result<(), String> {
            *target = value()?.parse().map_err(|e| format!("{a}: {e}"))?;
            Ok(())
        };
        match a.as_str() {
            "--width" => number(&mut size.width)?,
            "--height" => number(&mut size.height)?,
            "--spp" => number(&mut size.spp)?,
            "--bounces" => number(&mut size.max_bounces)?,
            "--raster-frames" => number(&mut size.raster_frames)?,
            "--seed" => number(&mut size.seed)?,
            "--layers" => layers = Some(value()?.split(',').map(str::to_string).collect()),
            "--no-ground" => ground = false,
            "--no-views" => views = false,
            "--terms" => terms = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let device = RenderDevice::headless().map_err(|e| e.to_string())?;
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    let cases: Vec<CloudCase> = standard_cases()
        .into_iter()
        .filter(|c| layers.as_ref().is_none_or(|l| l.iter().any(|n| n == c.name)))
        .collect();
    if cases.is_empty() {
        return Err("no case matches --layers".into());
    }
    if terms {
        for case in &cases {
            let t = light_terms(&tracer, case, &size).map_err(|e| e.to_string())?;
            println!("{}", terms_json(case, &t));
        }
        return Ok(());
    }
    for case in cases.iter().filter(|_| views) {
        println!("{}", view_line(&tracer, case, &size)?);
    }
    if ground {
        let ground_size = CloudTraceSize {
            width: size.width.min(size.height),
            height: size.width.min(size.height),
            spp: (size.spp / 4).max(8),
            ..size
        };
        // One view per sun height: the ground does not depend on the view's azimuth.
        for case in cases.iter().filter(|c| c.sun_azimuth_deg == 180.0) {
            let light = case.light();
            let m = compare_ground(&tracer, case, physical_field(case, &light), &ground_size)
                .map_err(|e| e.to_string())?;
            println!(
                "{}",
                json!({
                    "kind": "ground",
                    "layer": case.name,
                    "sun_elevation_deg": case.sun_elevation_deg,
                    "measure": ground_json(&m),
                })
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cloud_compare: {e}");
            ExitCode::FAILURE
        }
    }
}
