// SPDX-License-Identifier: MPL-2.0
//! `vfx.crash_fireball` (ADR-1498, `book/agents/crash-fireball.md`): the crash fireball as a
//! level camera sees it, from the CPU footprint model (`regia_vfx::crash::fireball_footprint`):
//! its flame and soot areas, equivalent diameter, extent, height and the soot toroid over time,
//! and the seen peak against the yield's `3.5 W^(1/3)`. No GPU and no scene needed.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::crash::{
    CrashScale, CrashTier, FIREBALL_SPAWN_HEIGHT, FireballFootprint, crash_fireball, fireball_effect,
    fireball_footprint,
};
use regia_vfx::{VfxMedium, VfxScene, VfxSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most times one call takes.
const MAX_TIMES: usize = 32;
/// Latest time, seconds (the fireball is soot well before this at any yield).
const MAX_SECONDS: f64 = 30.0;
/// Steps of the peak scan per buoyancy time `τ` (it covers `0..1.6 τ`).
const PEAK_STEPS_PER_TAU: f32 = 30.0;

fn number(params: &Value, field: &str, default: f32, min: f32, max: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite() && (min..=max).contains(f))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number in {min}..={max}"))),
    }
}

fn wind(params: &Value) -> Result<Option<Vec3>, RpcError> {
    let Some(value) = params.get("wind").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params("`wind` must be [x, y, z]"))?;
    let mut out = [0.0f32; 3];
    for (o, v) in out.iter_mut().zip(items) {
        *o = v
            .as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| RpcError::invalid_params("`wind` must hold finite numbers"))? as f32;
    }
    Ok(Some(Vec3::from_array(out)))
}

fn times(params: &Value) -> Result<Vec<f32>, RpcError> {
    let Some(value) = params.get("times").filter(|v| !v.is_null()) else {
        return Ok(vec![0.1, 0.25, 0.5, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 5.0]);
    };
    let items = value
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= MAX_TIMES)
        .ok_or_else(|| RpcError::invalid_params(format!("`times` must hold 1..={MAX_TIMES} seconds")))?;
    items
        .iter()
        .map(|t| {
            t.as_f64()
                .filter(|t| t.is_finite() && (0.0..=MAX_SECONDS).contains(t))
                .map(|t| t as f32)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`times` entries must be 0..={MAX_SECONDS} s"))
                })
        })
        .collect()
}

fn frame_json(f: &FireballFootprint) -> Value {
    json!({
        "time": f.time,
        "flame_area": f.flame_area,
        "flame_diameter": f.flame_diameter,
        "soot_area": f.soot_area,
        "width": f.width,
        "height": f.height,
        "top": f.top,
        "centre_height": f.centre_height,
        "soot_ring_radius": f.soot_ring_radius,
        "soot_hole": f.soot_hole,
        "soot_aspect": f.soot_aspect,
    })
}

/// `vfx.crash_fireball`: the fireball of a crash of `yield_t` as a level camera sees it (CPU
/// footprint model): the frames at `times`, the seen peak (scanned over `0..1.6 τ`), when the
/// flame is under a tenth of its peak area, and the yield's target diameter.
pub fn crash_fireball_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scale = CrashScale::from_tonnes(number(
        params,
        "yield_t",
        CrashScale::VEHICLE.tonnes(),
        CrashScale::MIN_TONNES,
        CrashScale::MAX_TONNES,
    )?);
    let tier = match crate::params::opt_str(params, "tier")? {
        Some(name) => CrashTier::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`tier` {name}: full or lite")))?,
        None => world
            .get_resource::<VfxSettings>()
            .map_or(CrashTier::Full, |s| CrashTier::for_profile(&s.profile)),
    };
    let mut medium = world
        .get_resource::<VfxScene>()
        .map_or_else(VfxMedium::earth, VfxScene::medium);
    if let Some(w) = wind(params)? {
        medium.wind = w;
    }
    let times = times(params)?;
    let def = crash_fireball(scale, tier, &medium);
    let frames: Vec<Value> = fireball_footprint(&def, &times).iter().map(frame_json).collect();
    let tau = scale.time_scale();
    let step = tau / PEAK_STEPS_PER_TAU;
    let scan_times: Vec<f32> = (1..=(1.6 * PEAK_STEPS_PER_TAU) as u32)
        .map(|i| i as f32 * step)
        .collect();
    let scan = fireball_footprint(&def, &scan_times);
    let peak = scan
        .iter()
        .copied()
        .fold(FireballFootprint::default(), |best, f| {
            if f.flame_diameter > best.flame_diameter {
                f
            } else {
                best
            }
        });
    let black_at = scan
        .iter()
        .find(|f| f.time > peak.time && f.flame_area <= 0.1 * peak.flame_area)
        .map(|f| f.time);
    Ok(json!({
        "effect": fireball_effect(scale),
        "tier": tier.name(),
        "yield_t": scale.tonnes(),
        "target_diameter": 2.0 * scale.fireball_radius(),
        "spawn_height": FIREBALL_SPAWN_HEIGHT * scale.fireball_radius(),
        "time_scale": tau,
        "peak": { "time": peak.time, "flame_diameter": peak.flame_diameter, "width": peak.width },
        "black_at": black_at,
        "frames": frames,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;

    /// At the showcase yield the seen peak is 150–250 m at 1–1.5 s, and the flame is under a tenth
    /// of its peak area by 5 s; bad parameters are invalid params.
    #[test]
    fn the_view_reports_the_seen_fireball() {
        let mut world = World::new();
        let out = crash_fireball_view(
            &mut world,
            &json!({ "yield_t": 250.0, "tier": "lite", "times": [1.0, 4.0] }),
        )
        .expect("view");
        assert_eq!(out["effect"], "crash_fireball@250t");
        let d = out["peak"]["flame_diameter"].as_f64().unwrap_or_default();
        assert!((150.0..=250.0).contains(&d), "{d}");
        let t = out["peak"]["time"].as_f64().unwrap_or_default();
        assert!((1.0..=1.5).contains(&t), "{t}");
        let black = out["black_at"].as_f64().unwrap_or(f64::MAX);
        assert!(black <= 5.0, "{black}");
        assert_eq!(out["frames"].as_array().map(Vec::len), Some(2));
        let target = out["target_diameter"].as_f64().unwrap_or_default();
        assert!((219.0..=222.0).contains(&target), "{target}");
        for bad in [
            json!({ "yield_t": 0.0 }),
            json!({ "yield_t": 2000.0 }),
            json!({ "tier": "ultra" }),
            json!({ "times": [] }),
            json!({ "times": [99.0] }),
            json!({ "wind": [1.0, 2.0] }),
        ] {
            let err = crash_fireball_view(&mut world, &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }
}
