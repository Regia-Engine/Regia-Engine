// SPDX-License-Identifier: MPL-2.0
//! `cloud_validation`: the path-traced cloud layer against the real-time cloud model (ADR-320,
//! ADR-321, ADR-1834). Prints one JSON object per line.
//!
//! ```text
//! cargo run --release -p regia_pt --example cloud_validation -- \
//!     [--tau 0.5,2,8,30] [--sun-y 0.3,0.6,0.9] [--spp 64] [--fill 0.05]
//! ```
//!
//! Per (optical depth, sun height): the traced ground-light ratio under the slab (`pt_ratio`),
//! the real-time renderer's (`renderer_ratio`), their difference, the unscattered beam's
//! transmittance and the two-stream diffuse transmittance. With `--fill F` one extra `settings`
//! line reports the optical depth the default cloud settings imply at that column fill.

use std::process::ExitCode;

use regia_pt::PathTracer;
use regia_pt::cloud::{
    CloudLight, PtCloudLayer, VALIDATION_SKY_NIT, VALIDATION_SUN_LUX, measure_ground_ratio,
    reference_ground_ratio, renderer_ground_ratio, renderer_log2_gap, renderer_sky_ratio, renderer_sun_ratio,
    two_stream_diffuse_transmittance,
};
use regia_render::{CloudSettings, RenderDevice};
use serde_json::json;

fn list(arg: &str) -> Result<Vec<f32>, String> {
    arg.split(',')
        .map(|v| v.trim().parse::<f32>().map_err(|e| format!("`{v}`: {e}")))
        .collect()
}

fn run() -> Result<(), String> {
    let mut taus = vec![0.5, 2.0, 8.0, 30.0];
    let mut suns = vec![0.3, 0.6, 0.9];
    let mut spp = 64u32;
    let mut fill = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--tau" => taus = list(&value()?)?,
            "--sun-y" => suns = list(&value()?)?,
            "--spp" => spp = value()?.parse().map_err(|e| format!("--spp: {e}"))?,
            "--fill" => fill = Some(value()?.parse::<f32>().map_err(|e| format!("--fill: {e}"))?),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if let Some(fill) = fill {
        let s = CloudSettings::default();
        let layer = PtCloudLayer::from_settings(&s, fill);
        println!(
            "{}",
            json!({"kind": "settings", "fill": fill, "optical_depth": layer.optical_depth(),
                   "base_m": layer.base_m, "top_m": layer.top_m})
        );
    }
    let device = RenderDevice::headless().map_err(|e| format!("device: {e}"))?;
    let tracer = PathTracer::new(&device).map_err(|e| e.to_string())?;
    for &tau in &taus {
        let layer = PtCloudLayer::overcast(1500.0, 4000.0, tau);
        for &sun_y in &suns {
            let both = measure_ground_ratio(&tracer, &layer, sun_y, CloudLight::Both, spp, 1)
                .map_err(|e| e.to_string())?;
            let sun = measure_ground_ratio(&tracer, &layer, sun_y, CloudLight::SunOnly, spp, 2)
                .map_err(|e| e.to_string())?;
            let sky = measure_ground_ratio(&tracer, &layer, sun_y, CloudLight::SkyOnly, spp, 3)
                .map_err(|e| e.to_string())?;
            let sky_lux = std::f32::consts::PI * VALIDATION_SKY_NIT;
            let renderer = renderer_ground_ratio(&layer, sun_y, VALIDATION_SUN_LUX, sky_lux);
            println!(
                "{}",
                json!({
                    "kind": "ground", "tau": tau, "sun_y": sun_y, "spp": spp,
                    "pt_ratio": both.ratio(), "renderer_ratio": renderer,
                    "reference_ratio": reference_ground_ratio(&layer, sun_y, VALIDATION_SUN_LUX, sky_lux),
                    "renderer_log2_gap": renderer_log2_gap(&layer, sun_y),
                    "pt_sun_ratio": sun.ratio(), "renderer_sun_ratio": renderer_sun_ratio(&layer, sun_y),
                    "pt_sky_ratio": sky.ratio(), "renderer_sky_ratio": renderer_sky_ratio(&layer),
                    "beam_transmittance": layer.beam_transmittance(sun_y),
                    "two_stream_diffuse": two_stream_diffuse_transmittance(tau, layer.asymmetry),
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
            eprintln!("cloud_validation: {e}");
            ExitCode::FAILURE
        }
    }
}
