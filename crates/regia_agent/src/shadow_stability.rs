// SPDX-License-Identifier: MPL-2.0
//! `render.shadow_stability` (`book/agents/shadow-stability.md`, ADR-1236): how still the
//! active profile's sun cascades stay while the camera turns or moves. The cascades are
//! placed on the CPU exactly as the renderer places them
//! (`regia_render::sun_cascades`), so no frame needs to run.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::RenderSettings;
use regia_render::sun_cascades::{
    CameraSweep, STILL_DRIFT_TEXELS, SunCascadeView, cascade_snap_texels, sweep_stability,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Sun travel direction used when the call names none (about 46° elevation).
const DEFAULT_SUN: [f32; 3] = [-0.45, -0.72, -0.53];
/// Most frames one call may place (each places every cascade once).
const MAX_FRAMES: u64 = 1000;

fn vec3(params: &Value, field: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(v) = params.get(field) else {
        return Ok(None);
    };
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x
            .as_f64()
            .filter(|x| x.is_finite())
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three finite f32 numbers")))?;
    }
    Ok(Some(Vec3::from_array(out)))
}

/// An optional finite number in `[lo, hi]`.
fn number(params: &Value, field: &str, lo: f64, hi: f64) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && (lo..=hi).contains(v))
            .map(|v| Some(v as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number in {lo}–{hi}"))),
    }
}

fn sweep_param(params: &Value) -> Result<(CameraSweep, &'static str, f32), RpcError> {
    let kind = match params.get("sweep") {
        None => "yaw",
        Some(v) => match v.as_str() {
            Some("yaw") => "yaw",
            Some("pitch") => "pitch",
            Some("strafe") => "strafe",
            Some("zoom") => "zoom",
            _ => {
                return Err(RpcError::invalid_params(
                    "`sweep` must be \"yaw\", \"pitch\", \"strafe\" or \"zoom\"",
                ));
            }
        },
    };
    let (default, limit) = match kind {
        "strafe" => (5.0, 10_000.0),
        "zoom" => (-5.0, 170.0),
        _ => (30.0, 360.0),
    };
    let amount = number(params, "amount", -limit, limit)?.unwrap_or(default);
    let sweep = match kind {
        "pitch" => CameraSweep::Pitch { degrees: amount },
        "strafe" => CameraSweep::Strafe { meters: amount },
        "zoom" => CameraSweep::Zoom { degrees: amount },
        _ => CameraSweep::Yaw { degrees: amount },
    };
    Ok((sweep, kind, amount))
}

/// `render.shadow_stability {eye?, target?, sun?, sweep?, amount?, frames?, fov_y_deg?, aspect?,
/// near?, far?, stable?}`: places the active profile's sun cascades for every frame of a camera
/// sweep and reports, per cascade, the texel size, the largest and mean lattice drift between
/// frames (texels; 0 = static shadows do not shimmer) and how many frames moved the cascade.
/// `stable` overrides `FrameQuality::stable_sun_cascades` for the report only (a what-if; the
/// renderer is not changed).
pub fn shadow_stability(world: &World, params: &Value) -> Result<Value, RpcError> {
    let eye = vec3(params, "eye")?.unwrap_or(Vec3::new(0.0, 1.7, 0.0));
    let target = vec3(params, "target")?.unwrap_or(eye + Vec3::new(0.0, -0.25, -1.0));
    let forward = target - eye;
    let forward_length_squared = forward.length_squared();
    if !forward_length_squared.is_finite() || forward_length_squared < 1.0e-8 {
        return Err(RpcError::invalid_params(
            "`target` must differ from `eye` by a finite direction",
        ));
    }
    let sun = vec3(params, "sun")?.unwrap_or(Vec3::from_array(DEFAULT_SUN));
    let sun_length_squared = sun.length_squared();
    if !sun_length_squared.is_finite() || sun_length_squared < 1.0e-8 {
        return Err(RpcError::invalid_params(
            "`sun` must be a finite, non-zero direction",
        ));
    }
    let (sweep, kind, amount) = sweep_param(params)?;
    let frames = match params.get("frames") {
        None => 60,
        Some(v) => v
            .as_u64()
            .filter(|n| (2..=MAX_FRAMES).contains(n))
            .ok_or_else(|| RpcError::invalid_params(format!("`frames` must be an integer 2–{MAX_FRAMES}")))?
            as u32,
    };
    let fov_y = number(params, "fov_y_deg", 1.0, 179.0)?
        .unwrap_or(60.0)
        .to_radians();
    let aspect = number(params, "aspect", 0.05, 20.0)?.unwrap_or(16.0 / 9.0);
    let near = number(params, "near", 0.001, 1000.0)?.unwrap_or(0.1);
    let far = number(params, "far", 0.01, 1.0e7)?.unwrap_or(2000.0);
    if far <= near {
        return Err(RpcError::invalid_params("`far` must be greater than `near`"));
    }
    let stable = match params.get("stable") {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`stable` must be a boolean"))?,
        ),
    };
    let settings = world.get_resource::<RenderSettings>().ok_or_else(|| {
        RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render's RenderPlugin (it inserts RenderSettings)",
        )
    })?;
    let preset = settings.preset().map(|p| format!("{p:?}"));
    let mut quality = settings.quality();
    if let Some(stable) = stable {
        quality.stable_sun_cascades = stable;
    }
    let view = SunCascadeView {
        eye: eye.to_array(),
        forward: forward.normalize().to_array(),
        fov_y,
        aspect,
        near,
        far,
    };
    let report = sweep_stability(&view, sun.normalize().to_array(), &quality, sweep, frames);
    let still = report.iter().all(|c| c.max_drift_texels < STILL_DRIFT_TEXELS);
    let cascades: Vec<Value> = report
        .iter()
        .map(|c| {
            json!({
                "cascade": c.cascade,
                "texel_m": c.texel_m,
                "max_drift_texels": c.max_drift_texels,
                "mean_drift_texels": c.mean_drift_texels,
                "moves": c.moves,
                "rescales": c.rescales,
                "pairs": c.pairs,
            })
        })
        .collect();
    Ok(json!({
        "preset": preset,
        "stable": quality.stable_sun_cascades,
        "paged": quality.paged_sun_shadows,
        "snap_texels": cascade_snap_texels(&quality),
        "resolution": quality.csm_resolution,
        "sweep": kind,
        "amount": amount,
        "frames": frames,
        "still": still,
        "still_threshold_texels": STILL_DRIFT_TEXELS,
        "cascades": cascades,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    #[test]
    fn oversized_eye_is_rejected() {
        let world = world(RenderProfilePreset::DesktopHigh);
        for params in [
            json!({ "eye": [1.0e40, 0, 0] }),
            json!({ "target": [0, -1.0e40, 0] }),
            json!({ "sun": [0, 0, 1.0e40] }),
            json!({ "eye": [3.0e38, 0, 0], "target": [-3.0e38, 0, 0] }),
            json!({ "eye": [1.0e20, 0, 0], "target": [0, 0, 0] }),
            json!({ "sun": [1.0e20, 0, 0] }),
        ] {
            let err = shadow_stability(&world, &params).expect_err("non-finite camera direction");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }

    fn world(preset: RenderProfilePreset) -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(preset));
        world
    }

    #[test]
    fn every_preset_reports_still_cascades_under_a_turn() {
        for preset in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
        ] {
            let w = world(preset);
            let out = shadow_stability(&w, &json!({ "frames": 24 })).expect("report");
            assert_eq!(out["still"], json!(true), "{preset:?}: {out}");
            assert_eq!(out["stable"], json!(true));
            assert_eq!(out["sweep"], json!("yaw"));
            let cascades = out["cascades"].as_array().expect("cascades");
            assert!(!cascades.is_empty());
            for c in cascades {
                assert_eq!(c["pairs"], json!(23), "{preset:?}: {c}");
            }
        }
    }

    #[test]
    fn the_what_if_override_shows_the_sliding_fit() {
        let w = world(RenderProfilePreset::MobileHigh);
        let out = shadow_stability(
            &w,
            &json!({ "stable": false, "sweep": "yaw", "amount": 12.0, "frames": 48 }),
        )
        .expect("report");
        assert_eq!(out["stable"], json!(false));
        assert_eq!(out["snap_texels"], json!(0));
        assert_eq!(out["still"], json!(false), "{out}");
        // The renderer's settings are untouched.
        let quality = w.resource::<RenderSettings>().quality();
        assert!(quality.stable_sun_cascades);
    }

    #[test]
    fn strafe_and_pitch_sweeps_are_still() {
        let w = world(RenderProfilePreset::Deck);
        for params in [
            json!({ "sweep": "strafe", "amount": 4.0, "eye": [10.0, 2.0, -3.0], "target": [11.0, 1.0, -9.0] }),
            json!({ "sweep": "pitch", "amount": -20.0, "sun": [0.2, -0.9, 0.1] }),
        ] {
            let out = shadow_stability(&w, &params).expect("report");
            assert_eq!(out["still"], json!(true), "{params}: {out}");
        }
    }

    #[test]
    fn a_zoom_sweep_reports_few_rescales() {
        let w = world(RenderProfilePreset::DesktopHigh);
        let out = shadow_stability(&w, &json!({ "sweep": "zoom", "amount": -5.0, "fov_y_deg": 65.0 }))
            .expect("report");
        assert_eq!(out["still"], json!(true), "{out}");
        for c in out["cascades"].as_array().expect("cascades") {
            assert!(c["rescales"].as_u64().expect("rescales") <= 8, "{c}");
        }
    }

    #[test]
    fn bad_params_are_rejected() {
        let w = world(RenderProfilePreset::DesktopHigh);
        for params in [
            json!({ "sweep": "roll" }),
            json!({ "frames": 1 }),
            json!({ "frames": 5000 }),
            json!({ "eye": [0.0, 1.0] }),
            json!({ "eye": [0.0, 1.0, 0.0], "target": [0.0, 1.0, 0.0] }),
            json!({ "sun": [0.0, 0.0, 0.0] }),
            json!({ "amount": 400.0 }),
            json!({ "near": 10.0, "far": 5.0 }),
            json!({ "stable": "yes" }),
        ] {
            assert!(shadow_stability(&w, &params).is_err(), "{params}");
        }
        assert!(shadow_stability(&World::new(), &json!({})).is_err());
    }
}
