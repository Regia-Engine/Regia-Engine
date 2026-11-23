// SPDX-License-Identifier: MPL-2.0
//! `seeing.temporal_chart`: the spec 27 § 2.3 chart through a temporal upscaler on the GPU
//! (book `agents/temporal-chart.md`, guide topic `temporal_chart`, ADR-2638).
//!
//! Renders `regia_render::temporal_chart`'s deterministic chart scene through the real frame
//! (a private renderer on the app's render device; an app without one answers
//! `temporal_chart_no_device`) and returns the five § 2.3 numbers with their targets. The call
//! blocks for the whole run (about 400 small frames at the default 120), so it is a heavy method.

use regia_app::prelude::*;
use regia_render::temporal_chart::{
    ChartMotion, ChartReport, ChartRequest, GHOST_FRAMES, MAX_FRAMES, STABILITY_FRAMES, TARGETS, run,
};
use regia_render::upscaler::UpscalerKind;
use regia_render::{RenderDevice, TemporalUpscaler};
use serde_json::{Map, Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Largest pan or orbit step per frame a call accepts.
const MAX_STEP: f64 = 32.0;

fn number(
    map: &Map<String, Value>,
    name: &str,
    range: std::ops::RangeInclusive<f64>,
) -> Result<Option<f64>, RpcError> {
    match map.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && range.contains(f))
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`{name}` must be a number from {} to {}",
                    range.start(),
                    range.end()
                ))
            }),
    }
}

/// The request `params` describe.
fn parse(params: &Value) -> Result<ChartRequest, RpcError> {
    let empty = Map::new();
    let map = match params {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => return Err(RpcError::invalid_params("params must be an object")),
    };
    let upscaler = match map.get("upscaler") {
        None | Some(Value::Null) => TemporalUpscaler::Tsr,
        Some(Value::String(name)) => match UpscalerKind::from_name(name) {
            Some(UpscalerKind::Builtin(builtin)) => builtin,
            Some(_) => {
                return Err(RpcError::new(
                    UNSUPPORTED,
                    "temporal_chart_vendor_upscaler",
                    format!("the chart runs the built-in upscalers only, not {name:?}"),
                    "use tsr, taau or fsr3",
                ));
            }
            None => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "temporal_chart_unknown_upscaler",
                    format!("no temporal upscaler {name:?}"),
                    "use tsr (default), taau or fsr3",
                ));
            }
        },
        Some(_) => return Err(RpcError::invalid_params("`upscaler` must be a string")),
    };
    let scale = number(map, "scale", 0.25..=1.0)?.unwrap_or(0.67) as f32;
    let pan = number(map, "pan_px", -MAX_STEP..=MAX_STEP)?;
    let orbit = number(map, "orbit_deg", -MAX_STEP..=MAX_STEP)?;
    let motion = match (pan, orbit) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params("give `pan_px` or `orbit_deg`, not both"));
        }
        (_, Some(deg)) => ChartMotion::Orbit {
            deg_per_frame: deg as f32,
        },
        (pan, None) => ChartMotion::Pan {
            px_per_frame: pan.unwrap_or(2.0) as f32,
        },
    };
    let frames = match map.get("frames") {
        None | Some(Value::Null) => 120,
        Some(v) => v
            .as_u64()
            .filter(|f| (2..=u64::from(MAX_FRAMES)).contains(f))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`frames` must be an integer from 2 to {MAX_FRAMES}"))
            })? as u32,
    };
    Ok(ChartRequest {
        motion,
        frames,
        scale,
        upscaler,
        tsr: None,
    })
}

fn upscaler_name(upscaler: TemporalUpscaler) -> &'static str {
    UpscalerKind::Builtin(upscaler).name()
}

/// The JSON result for one report.
fn report_json(report: &ChartReport) -> Value {
    let r = &report.request;
    let motion = match r.motion {
        ChartMotion::Pan { px_per_frame } => json!({ "kind": "pan", "px_per_frame": px_per_frame }),
        ChartMotion::Orbit { deg_per_frame } => json!({ "kind": "orbit", "deg_per_frame": deg_per_frame }),
    };
    let checks: Map<String, Value> = report
        .checks()
        .iter()
        .map(|(name, value, target, met)| {
            (
                (*name).to_string(),
                json!({ "value": value, "target": target, "met": met }),
            )
        })
        .collect();
    let failing: Vec<&str> = report.checks().iter().filter(|c| !c.3).map(|c| c.0).collect();
    json!({
        "upscaler": upscaler_name(r.upscaler),
        "scale": r.scale,
        "frames": r.frames,
        "motion": motion,
        "motion_clarity": report.motion.motion_clarity,
        "thin_geometry": report.motion.thin_geometry,
        "ghosting": report.ghost.ghosting,
        "disocclusion_px": report.ghost.disocclusion_px,
        "stability": report.stability.flip,
        "meets_targets": report.meets_targets(),
        "failing": failing,
        "checks": checks,
        "detail": {
            "mtf50": report.motion.moving.mtf50,
            "native_mtf50": report.motion.native.mtf50,
            "modulation": report.motion.moving.modulation,
            "ghost_residual": report.ghost.ghost_residual,
            "stability_luma_delta": report.stability.luma_delta,
            "ghost_frames": GHOST_FRAMES,
            "stability_frames": STABILITY_FRAMES,
        },
        "frames_rendered": report.frames_rendered,
    })
}

/// `seeing.temporal_chart { pan_px?, orbit_deg?, frames?, scale?, upscaler? }` → the five
/// § 2.3 numbers (`motion_clarity`, `thin_geometry`, `ghosting`, `disocclusion_px`,
/// `stability`), `checks` against the targets and `failing`.
pub(crate) fn temporal_chart(world: &World, params: &Value) -> Result<Value, RpcError> {
    let request = parse(params)?;
    let Some(device) = world.get_resource::<RenderDevice>() else {
        return Err(RpcError::new(
            NOT_FOUND,
            "temporal_chart_no_device",
            "this app has no render device",
            "run it in an app with regia_render::RenderPlugin, or call seeing.screenshot \
             {\"target\": \"offscreen\"} first (it creates a headless device)",
        ));
    };
    let report = run(device, &request).map_err(|e| {
        RpcError::new(
            INTERNAL_ERROR,
            "temporal_chart_render_failed",
            format!("a chart frame failed: {e}"),
            "check the log for the GPU error; lower `frames` to retry quickly",
        )
    })?;
    let mut out = report_json(&report);
    if let Value::Object(map) = &mut out {
        map.insert(
            "targets".into(),
            json!({
                "motion_clarity": TARGETS.motion_clarity,
                "thin_geometry": TARGETS.thin_geometry,
                "ghosting": TARGETS.ghosting,
                "disocclusion_px": TARGETS.disocclusion_px,
                "stability": TARGETS.stability,
            }),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: &RpcError) -> String {
        e.data.clone().unwrap_or_default()["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn defaults_are_the_spec_pan_through_tsr_at_067() {
        let r = parse(&json!({})).expect("defaults");
        assert_eq!(r.upscaler, TemporalUpscaler::Tsr);
        assert_eq!(r.frames, 120);
        assert!((r.scale - 0.67).abs() < 1e-6);
        assert_eq!(r.motion, ChartMotion::Pan { px_per_frame: 2.0 });
        let r = parse(&json!({ "orbit_deg": 1, "upscaler": "FSR 3", "scale": 0.5, "frames": 30 }))
            .expect("orbit");
        assert_eq!(r.upscaler, TemporalUpscaler::Fsr3);
        assert_eq!(r.motion, ChartMotion::Orbit { deg_per_frame: 1.0 });
        assert_eq!(r.frames, 30);
    }

    #[test]
    fn bad_params_are_refused_before_any_frame() {
        let unknown = parse(&json!({ "upscaler": "magic" })).expect_err("unknown");
        assert_eq!(code(&unknown), "temporal_chart_unknown_upscaler");
        let vendor = parse(&json!({ "upscaler": "dlss" })).expect_err("vendor");
        assert_eq!(code(&vendor), "temporal_chart_vendor_upscaler");
        assert!(parse(&json!({ "frames": 1 })).is_err());
        assert!(parse(&json!({ "frames": 100_000 })).is_err());
        assert!(parse(&json!({ "scale": 0.1 })).is_err());
        assert!(parse(&json!({ "pan_px": 2, "orbit_deg": 1 })).is_err());
        assert!(parse(&json!([1])).is_err());
    }

    /// One short run on lavapipe: every number is present and finite.
    #[test]
    fn a_short_run_returns_the_five_numbers() {
        let mut world = World::new();
        let no_device = temporal_chart(&world, &json!({})).expect_err("no device");
        assert_eq!(code(&no_device), "temporal_chart_no_device");
        match RenderDevice::shared_headless() {
            Ok(device) => world.insert_resource(device),
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
        }
        let out = temporal_chart(&world, &json!({ "frames": 8, "scale": 0.5 })).expect("chart");
        for key in [
            "motion_clarity",
            "thin_geometry",
            "ghosting",
            "disocclusion_px",
            "stability",
        ] {
            let v = out[key].as_f64().unwrap_or(f64::NAN);
            assert!(v.is_finite(), "{key}: {out}");
            assert!(out["checks"][key]["met"].is_boolean(), "{key}: {out}");
        }
        assert_eq!(out["upscaler"], "tsr");
        assert_eq!(out["frames"], 8);
    }
}
