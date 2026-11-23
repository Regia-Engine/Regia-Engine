// SPDX-License-Identifier: MPL-2.0
//! ReSTIR GI on the agent protocol (ADR-3349, ADR-4001, `book/agents/restir-gi.md`).
//!
//! `render.restir_gi` reads the profile switch, device offer, settings and last-frame fallback,
//! history and work counters. `render.set_restir_gi` changes the opt-in and reuse settings.
//! Unsupported hardware is a successful `sdf_probes` fallback, never an RPC error.

use regia_app::prelude::*;
use regia_render::hwrt::last_device_report;
use regia_render::restir_gi::{RestirGiReport, RestirGiSettings, RestirGiWork};
use regia_render::{RenderSettings, RuntimeRenderer};
use serde_json::{Map, Value, json};

use crate::post_volumes::{check_fields, object};
use crate::protocol::RpcError;

const SET_FIELDS: &[&str] = &[
    "enabled",
    "temporal",
    "spatial_taps",
    "history_cap",
    "reset_history",
];

fn missing_settings() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource: this app has no render profile",
        "add regia_render::RenderPlugin (it inserts RenderSettings)",
    )
}

fn missing_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource: ReSTIR GI settings and history live in the renderer",
        "add regia_render::RenderPlugin, or set only `enabled` to change the profile for a later renderer",
    )
}

fn work_json(work: RestirGiWork) -> Value {
    json!({
        "rays": work.rays,
        "initial_rays": work.initial_rays,
        "shadow_rays": work.shadow_rays,
        "visibility_rays": work.visibility_rays,
        "temporal_reused": work.temporal_reused,
        "spatial_reused": work.spatial_reused,
        "history_rejections": work.history_rejections,
        "validation_rejections": work.validation_rejections,
        "initial_hits": work.initial_hits,
        "sky_samples": work.sky_samples,
        "sun_visible": work.sun_visible,
        "probe_lum_sum": work.probe_lum_sum,
        "motion_rejections": work.motion_rejections,
    })
}

fn settings_json(settings: &RestirGiSettings) -> Value {
    json!({
        "rays": 1,
        "temporal": settings.temporal,
        "history_cap": settings.history_cap,
        "spatial_taps": settings.spatial_taps,
        "spatial_radius_px": settings.spatial_radius_px,
        "spatial_cap": settings.spatial_cap,
        "depth_tolerance": settings.depth_tolerance,
        "normal_tolerance": settings.normal_tolerance,
        "validation_interval": settings.validation_interval,
        "denoise": {
            "alpha_color": settings.denoise.alpha_color,
            "alpha_moments": settings.denoise.alpha_moments,
            "max_history": settings.denoise.max_history,
            "atrous_iterations": settings.denoise.atrous_iterations,
            "antilag_sigma": settings.denoise.antilag_sigma,
        },
    })
}

fn frame_json(report: &RestirGiReport) -> Value {
    let fallback = report.fallback.map(|reason| {
        json!({
            "path": "sdf_probes",
            "reason": reason.as_str(),
            "message": reason.message(),
        })
    });
    json!({
        "active": report.active,
        "path": if report.active { "restir_gi" } else { "sdf_probes" },
        "fallback": fallback,
        "frames": report.frames,
        "resets": report.resets,
        "resource_builds": report.resource_builds,
        "bind_group_builds": report.bind_group_builds,
        "size": { "width": report.size.0, "height": report.size.1 },
        "history": {
            "valid": report.history_valid,
            "resets": report.resets,
        },
        "work": report.work.map(work_json),
        "total_work": work_json(report.total_work),
        "last_error": report.last_error,
    })
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world
        .get_resource::<RenderSettings>()
        .ok_or_else(missing_settings)?;
    let renderer = world.get_resource::<RuntimeRenderer>();
    let device = last_device_report().map(|report| {
        json!({
            "probed": true,
            "ray_query": report.device_ray_query,
            "vertex_return": report.probe.vertex_return,
            "reason": report.reason().code(),
            "message": report.reason().message(),
        })
    });
    Ok(json!({
        "enabled": settings.profile.restir_gi,
        "preset": settings.preset().map(|preset| preset.name()),
        "device": device,
        "settings": renderer.map_or_else(|| settings_json(&RestirGiSettings::default()), |r| settings_json(&r.0.restir_gi_settings())),
        "frame": renderer.map_or(Value::Null, |r| frame_json(&r.0.restir_gi_report())),
    }))
}

/// `render.restir_gi`: profile, device, settings and last-frame state.
pub(crate) fn report(world: &World) -> Result<Value, RpcError> {
    state(world)
}

fn boolean(map: &Map<String, Value>, key: &str) -> Result<Option<bool>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(other) => Err(RpcError::invalid_params(format!(
            "`{key}` must be true or false, got {other}"
        ))),
    }
}

/// `render.set_restir_gi`: enable and/or change temporal or spatial reuse, or reset history.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, SET_FIELDS)?;
    let enabled = boolean(map, "enabled")?;
    let temporal = boolean(map, "temporal")?;
    let reset_history = boolean(map, "reset_history")?.unwrap_or(false);
    let spatial_taps = match map.get("spatial_taps") {
        None | Some(Value::Null) => None,
        Some(value) => match value
            .as_u64()
            .filter(|v| *v <= u64::from(RestirGiSettings::MAX_SPATIAL_TAPS))
        {
            Some(value) => Some(value as u32),
            None => {
                return Err(RpcError::invalid_params(format!(
                    "`spatial_taps` must be an integer from 0 to {}, got {value}",
                    RestirGiSettings::MAX_SPATIAL_TAPS
                )));
            }
        },
    };
    let history_cap = match map.get("history_cap") {
        None | Some(Value::Null) => None,
        Some(value) => match value
            .as_f64()
            .filter(|v| v.is_finite() && (1.0..=500.0).contains(v))
        {
            Some(value) => Some(value as f32),
            None => {
                return Err(RpcError::invalid_params(format!(
                    "`history_cap` must be a number from 1 to 500, got {value}"
                )));
            }
        },
    };
    if enabled.is_none()
        && temporal.is_none()
        && spatial_taps.is_none()
        && history_cap.is_none()
        && !map.contains_key("reset_history")
    {
        return Err(RpcError::invalid_params(
            "give `enabled`, `temporal`, `spatial_taps`, `history_cap` and/or `reset_history`",
        ));
    }
    let changes_renderer =
        temporal.is_some() || spatial_taps.is_some() || history_cap.is_some() || reset_history;
    if changes_renderer && world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(missing_renderer());
    }
    if world.get_resource::<RenderSettings>().is_none() {
        return Err(missing_settings());
    }
    let before = state(world)?;
    let mut candidate = world
        .get_resource::<RuntimeRenderer>()
        .map_or_else(RestirGiSettings::default, |renderer| {
            renderer.0.restir_gi_settings()
        });
    if let Some(value) = temporal {
        candidate.temporal = value;
    }
    if let Some(value) = spatial_taps {
        candidate.spatial_taps = value;
    }
    if let Some(value) = history_cap {
        candidate.history_cap = value;
    }
    candidate
        .validate()
        .map_err(|error| RpcError::invalid_params(format!("{} ({})", error, error.code())))?;
    if let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
        && renderer.0.restir_gi_settings() != candidate
    {
        renderer
            .0
            .set_restir_gi_settings(candidate)
            .map_err(|error| RpcError::invalid_params(format!("{} ({})", error, error.code())))?;
    }
    if let Some(enabled) = enabled
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
    {
        settings.profile.restir_gi = enabled;
    }
    if reset_history && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() {
        renderer.0.reset_restir_gi_history();
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next frame; ray queries and hit-vertex return must be requested before \
                 device creation. Otherwise the frame stays on the SDF probe tier and frame.fallback says why. \
                 A preset change turns the switch off; reuse-setting changes reset history once",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::restir_gi::RestirGiFallback;
    use regia_render::{RenderProfilePreset, RuntimeRenderer};

    fn world(preset: RenderProfilePreset) -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(preset));
        world
    }

    #[test]
    fn it_is_off_in_every_preset_without_losing_preset_identity_when_enabled() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let mut w = world(preset);
            let before = report(&w).expect("report");
            assert_eq!(before["enabled"], false, "{preset:?}");
            assert!(before["frame"].is_null());
            let changed = set(&mut w, &json!({ "enabled": true })).expect("enable");
            assert_eq!(changed["after"]["enabled"], true);
            assert_eq!(changed["after"]["preset"], preset.name());
            assert_eq!(w.resource::<RenderSettings>().preset(), Some(preset));
        }
    }

    #[test]
    fn bad_calls_change_nothing() {
        let mut w = world(RenderProfilePreset::DesktopUltra);
        for params in [
            json!({}),
            json!({ "enabled": 1 }),
            json!({ "spatial_taps": 9 }),
            json!({ "history_cap": 0 }),
            json!({ "unknown": true }),
        ] {
            set(&mut w, &params).expect_err("invalid call");
            assert!(!w.resource::<RenderSettings>().profile.restir_gi, "{params}");
        }
        let err = set(&mut w, &json!({ "temporal": false })).expect_err("renderer is required");
        assert_eq!(err.code, RpcError::not_found("", "").code);
        assert!(!w.resource::<RenderSettings>().profile.restir_gi);
    }

    #[test]
    fn renderer_settings_round_trip_and_idempotent_set_does_not_reset() {
        let mut w = world(RenderProfilePreset::DesktopUltra);
        w.insert_resource(RuntimeRenderer::default());
        let changed = set(
            &mut w,
            &json!({ "enabled": true, "temporal": false, "spatial_taps": 2, "history_cap": 12 }),
        )
        .expect("settings");
        assert_eq!(changed["after"]["settings"]["temporal"], false);
        assert_eq!(changed["after"]["settings"]["spatial_taps"], 2);
        assert_eq!(changed["after"]["settings"]["history_cap"], 12.0);
        assert_eq!(changed["after"]["frame"]["resets"], 0);
        let again = set(&mut w, &json!({ "spatial_taps": 2, "history_cap": 12 })).expect("same");
        assert_eq!(again["after"]["frame"]["resets"], 0);
        let reset = set(&mut w, &json!({ "reset_history": true })).expect("reset");
        assert_eq!(reset["after"]["enabled"], true);
    }

    #[test]
    fn frame_json_names_fallback_history_and_every_work_counter() {
        let work = RestirGiWork {
            rays: 8,
            initial_rays: 1,
            shadow_rays: 2,
            visibility_rays: 5,
            temporal_reused: 3,
            spatial_reused: 4,
            history_rejections: 6,
            validation_rejections: 7,
            initial_hits: 9,
            sky_samples: 10,
            sun_visible: 11,
            probe_lum_sum: 12,
            motion_rejections: 13,
        };
        let report = RestirGiReport {
            requested: true,
            active: false,
            fallback: Some(RestirGiFallback::NoRayQuery),
            frames: 1,
            resets: 2,
            resource_builds: 3,
            bind_group_builds: 4,
            size: (80, 48),
            history_valid: false,
            work: Some(work),
            total_work: work,
            last_error: None,
        };
        let value = frame_json(&report);
        assert_eq!(value["path"], "sdf_probes");
        assert_eq!(value["fallback"]["reason"], "no_ray_query");
        assert_eq!(value["history"]["valid"], false);
        assert_eq!(value["size"]["width"], 80);
        for field in [
            "rays",
            "initial_rays",
            "shadow_rays",
            "visibility_rays",
            "temporal_reused",
            "spatial_reused",
            "history_rejections",
            "validation_rejections",
            "initial_hits",
            "sky_samples",
            "sun_visible",
            "probe_lum_sum",
            "motion_rejections",
        ] {
            assert!(value["work"].get(field).is_some(), "{field}");
            assert!(value["total_work"].get(field).is_some(), "total {field}");
        }
        assert_eq!(value["work"]["rays"], 8);
        assert_eq!(value["work"]["motion_rejections"], 13);
        assert_eq!(value["total_work"]["motion_rejections"], 13);
    }

    #[test]
    fn missing_settings_is_not_found() {
        let w = World::new();
        report(&w).expect_err("no settings");
        let mut w = World::new();
        set(&mut w, &json!({ "enabled": true })).expect_err("no settings");
    }
}
