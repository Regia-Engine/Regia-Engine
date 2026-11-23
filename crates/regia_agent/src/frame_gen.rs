// SPDX-License-Identifier: MPL-2.0
//! FSR 3 frame generation on the agent protocol (`book/agents/frame-gen.md`, ADR-514, ADR-2067):
//! `render.frame_gen` reports the settings, whether this build and device can run frame generation
//! and what blocks it, the pacer's measurements and what the present path did;
//! `render.set_frame_gen` changes the settings. Turning frame generation on is refused with
//! `data.error = "frame_gen_unavailable"` and the blockers while any remain (a build without the
//! `frame-gen` feature, a headless app, a device without compute), and nothing changes.

use regia_app::prelude::*;
use regia_render::frame_gen::{
    self, FrameGenEnvironment, FrameGenPacing, FrameGenSettings, FrameGenStatus, HudComposition,
    PacingDecision, frame_gen_status,
};
use regia_render::{HeadlessRender, RenderDevice};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `data.error` of a refused `enabled: true`.
pub(crate) const FRAME_GEN_UNAVAILABLE: &str = "frame_gen_unavailable";
/// `data.error` of a `hud` name that is not a mode.
pub(crate) const UNKNOWN_HUD_MODE: &str = "unknown_hud_mode";
/// `data.error` of a number outside its range.
pub(crate) const OUT_OF_RANGE: &str = "out_of_range";

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no FrameGenSettings resource in this app",
        "the app runs without regia_render's RenderPlugin",
    )
}

/// Adds the snake_case `data.error` code (and more fields) to a Regia Engine error.
fn coded(mut error: RpcError, code: &str, extra: Value) -> RpcError {
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("error".into(), json!(code));
        if let Value::Object(fields) = extra {
            for (key, value) in fields {
                data.insert(key, value);
            }
        }
    }
    error
}

/// This build's constants with the app's facts.
fn environment(world: &World) -> FrameGenEnvironment {
    let headless = world.get_resource::<HeadlessRender>().is_some_and(|h| h.0);
    let desktop = world.get_resource::<RenderDevice>().map(|device| {
        frame_gen::desktop_gpu(
            device.capabilities.backend,
            device.capabilities.max_storage_textures_per_shader_stage,
            device.capabilities.max_storage_buffers_per_shader_stage,
        )
    });
    FrameGenEnvironment::current(headless, desktop)
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1.0e6
}

fn settings_json(settings: &FrameGenSettings) -> Value {
    json!({
        "enabled": settings.enabled,
        "hud": settings.hud.code(),
        "min_base_fps": settings.min_base_fps,
        "debug_tear_lines": settings.debug_tear_lines,
        "debug_view": settings.debug_view,
    })
}

fn pacing_json(pacing: &FrameGenPacing) -> Value {
    let stats = pacing.pacer.stats();
    let last = match pacing.last {
        Some(PacingDecision::Generate { interval_ns, .. }) => json!({
            "kind": "generate",
            "interval_ms": ms(interval_ns),
        }),
        Some(PacingDecision::RealOnly { reason, .. }) => json!({
            "kind": "real_only",
            "reason": reason.code(),
        }),
        None => Value::Null,
    };
    json!({
        "base_fps": pacing.pacer.base_fps(),
        "average_interval_ms": pacing.pacer.average_interval_ns().map(ms),
        "added_latency_ms": pacing.pacer.added_latency_ns().map(ms),
        "last": last,
        "present": {
            "generated_presents": pacing.present.generated_presents,
            "real_presents": pacing.present.real_presents,
            "resets": pacing.present.resets,
            "acquire_failures": pacing.present.acquire_failures,
            "fallback": pacing.present.fallback.map(|f| f.code()),
            "last_error": pacing.present.last_error,
            "gaps": {
                "count": pacing.present.gaps.count,
                "mean_ms": pacing.present.gaps.mean_ms,
                "stddev_ms": pacing.present.gaps.stddev_ms,
                "max_ms": pacing.present.gaps.max_ms,
            },
        },
        "stats": {
            "real_frames": stats.real_frames,
            "generated": stats.generated,
            "skipped_disabled": stats.skipped_disabled,
            "skipped_warmup": stats.skipped_warmup,
            "skipped_reset": stats.skipped_reset,
            "skipped_base_rate": stats.skipped_base_rate,
        },
    })
}

fn blocker_codes(status: &FrameGenStatus) -> Vec<&'static str> {
    status.blockers.iter().map(|b| b.code()).collect()
}

fn state(world: &World, env: FrameGenEnvironment) -> Result<Value, RpcError> {
    let settings = world.get_resource::<FrameGenSettings>().ok_or_else(no_renderer)?;
    let pacing = world.get_resource::<FrameGenPacing>();
    let status = frame_gen_status(settings, env, pacing.and_then(|p| p.last.as_ref()));
    Ok(json!({
        "settings": settings_json(settings),
        "compiled": status.compiled,
        "can_enable": status.can_enable(),
        "active": status.active,
        "blockers": blocker_codes(&status),
        "pacing": pacing.map_or(Value::Null, pacing_json),
        "hud_modes": HudComposition::ALL.iter().map(|m| m.code()).collect::<Vec<_>>(),
        "hud_effective": HudComposition::Redraw.code(),
        "optical_flow": {
            "block_size": frame_gen::optical_flow::BLOCK_SIZE,
            "search_radius": frame_gen::optical_flow::SEARCH_RADIUS,
            "max_levels": frame_gen::optical_flow::MAX_LEVELS,
        },
    }))
}

/// `render.frame_gen`: the settings, `compiled`, `can_enable`, `active`, `blockers`, the pacer's
/// measurements (`null` without the present observer) and the HUD modes.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let env = environment(world);
    state(world, env)
}

/// `render.set_frame_gen { enabled?, hud?, min_base_fps?, debug_tear_lines?, debug_view? }`.
/// Every parameter is checked before anything changes. Returns `{ before, after }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let env = environment(world);
    set_with(world, params, env)
}

fn optional_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// `set` against an explicit environment (the tests pass one where the interpolator exists).
fn set_with(world: &mut World, params: &Value, env: FrameGenEnvironment) -> Result<Value, RpcError> {
    let enabled = optional_bool(params, "enabled")?;
    let debug_tear_lines = optional_bool(params, "debug_tear_lines")?;
    let debug_view = optional_bool(params, "debug_view")?;
    let hud = match params.get("hud") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(HudComposition::from_code(name).ok_or_else(|| {
            coded(
                RpcError::invalid_params(format!(
                    "unknown HUD mode {name:?}: one of redraw, ui_texture, hud_less"
                )),
                UNKNOWN_HUD_MODE,
                json!({ "hud": name }),
            )
        })?),
        Some(_) => return Err(RpcError::invalid_params("`hud` must be a string")),
    };
    let min_base_fps = match params.get("min_base_fps") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let fps = n.as_f64().unwrap_or(f64::NAN) as f32;
            if !(frame_gen::MIN_BASE_FPS_FLOOR..=frame_gen::MIN_BASE_FPS_CEIL).contains(&fps) {
                return Err(coded(
                    RpcError::invalid_params(format!(
                        "min_base_fps {fps} is outside {}..={}",
                        frame_gen::MIN_BASE_FPS_FLOOR,
                        frame_gen::MIN_BASE_FPS_CEIL
                    )),
                    OUT_OF_RANGE,
                    json!({ "min_base_fps": fps }),
                ));
            }
            Some(fps)
        }
        Some(_) => return Err(RpcError::invalid_params("`min_base_fps` must be a number")),
    };

    let before = *world.get_resource::<FrameGenSettings>().ok_or_else(no_renderer)?;
    if enabled == Some(true) {
        let pacing = world.get_resource::<FrameGenPacing>();
        let status = frame_gen_status(&before, env, pacing.and_then(|p| p.last.as_ref()));
        if !status.can_enable() {
            let blockers = blocker_codes(&status);
            return Err(coded(
                RpcError::new(
                    protocol::UNSUPPORTED,
                    "F0013",
                    format!("frame generation cannot run here: {}", blockers.join(", ")),
                    "see render.frame_gen `blockers` and book/agents/frame-gen.md",
                ),
                FRAME_GEN_UNAVAILABLE,
                json!({ "blockers": blockers }),
            ));
        }
    }
    let mut after = before;
    if let Some(enabled) = enabled {
        after.enabled = enabled;
    }
    if let Some(hud) = hud {
        after.hud = hud;
    }
    if let Some(fps) = min_base_fps {
        after.min_base_fps = fps;
    }
    if let Some(on) = debug_tear_lines {
        after.debug_tear_lines = on;
    }
    if let Some(on) = debug_view {
        after.debug_view = on;
    }
    world.insert_resource(after);
    Ok(json!({ "before": settings_json(&before), "after": settings_json(&after) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(FrameGenSettings::default());
        world
    }

    fn ready() -> FrameGenEnvironment {
        FrameGenEnvironment {
            compiled: true,
            interpolator: true,
            headless: false,
            desktop_gpu: Some(true),
        }
    }

    fn error_code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("error")?.as_str()
    }

    #[test]
    fn get_reports_the_blockers_of_this_build() {
        let mut world = world();
        let state = get(&mut world).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(state["settings"]["enabled"], json!(false));
        assert_eq!(state["settings"]["hud"], json!("redraw"));
        assert_eq!(state["compiled"], json!(frame_gen::COMPILED));
        // The interpolator is ported: only the feature can block a build, and no device yet is
        // not a blocker.
        assert_eq!(state["can_enable"], json!(frame_gen::COMPILED));
        let blockers = state["blockers"].as_array().cloned().unwrap_or_default();
        assert!(!blockers.contains(&json!("interpolator_not_ported")));
        assert_eq!(blockers.contains(&json!("not_compiled")), !frame_gen::COMPILED);
        assert_eq!(state["active"], json!(false));
        assert_eq!(state["pacing"], Value::Null);
        assert_eq!(state["hud_modes"], json!(["redraw", "ui_texture", "hud_less"]));
        assert_eq!(state["hud_effective"], json!("redraw"));
        assert_eq!(state["optical_flow"]["block_size"], json!(8));
    }

    #[test]
    fn enabling_is_refused_with_the_blockers_and_changes_nothing() {
        let mut world = world();
        let blocked = FrameGenEnvironment {
            compiled: false,
            ..ready()
        };
        let err = set_with(
            &mut world,
            &json!({ "enabled": true, "hud": "hud_less", "min_base_fps": 40 }),
            blocked,
        )
        .err()
        .unwrap_or_else(|| panic!("enabling must be refused while a blocker remains"));
        assert_eq!(err.code, protocol::UNSUPPORTED);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("F0013"))
        );
        assert_eq!(error_code(&err), Some(FRAME_GEN_UNAVAILABLE));
        let blockers = err.data.as_ref().and_then(|d| d.get("blockers")).cloned();
        assert_eq!(blockers, Some(json!(["not_compiled"])));
        // The other fields of the same call did not apply either.
        assert_eq!(world.resource::<FrameGenSettings>(), &FrameGenSettings::default());
    }

    #[test]
    fn knobs_change_while_frame_generation_stays_off() {
        let mut world = world();
        let out = set(
            &mut world,
            &json!({ "hud": "ui_texture", "min_base_fps": 45.5, "debug_tear_lines": true, "enabled": false }),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out["before"]["hud"], json!("redraw"));
        assert_eq!(out["after"]["hud"], json!("ui_texture"));
        let settings = *world.resource::<FrameGenSettings>();
        assert_eq!(settings.hud, HudComposition::UiTexture);
        assert_eq!(settings.min_base_fps, 45.5);
        assert!(settings.debug_tear_lines && !settings.debug_view && !settings.enabled);
    }

    #[test]
    fn with_an_interpolator_enabling_applies() {
        let mut world = world();
        let out =
            set_with(&mut world, &json!({ "enabled": true }), ready()).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out["after"]["enabled"], json!(true));
        assert!(world.resource::<FrameGenSettings>().enabled);
        // A headless app still cannot.
        let mut world = self::world();
        let headless = FrameGenEnvironment {
            headless: true,
            ..ready()
        };
        let err = set_with(&mut world, &json!({ "enabled": true }), headless)
            .err()
            .unwrap_or_else(|| panic!("headless must be refused"));
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("blockers")),
            Some(&json!(["headless"]))
        );
    }

    #[test]
    fn bad_parameters_are_refused_with_codes_and_change_nothing() {
        let mut world = world();
        let cases = [
            (json!({ "hud": "hudless" }), Some(UNKNOWN_HUD_MODE)),
            (json!({ "hud": 3 }), None),
            (json!({ "min_base_fps": 500 }), Some(OUT_OF_RANGE)),
            (json!({ "min_base_fps": 2 }), Some(OUT_OF_RANGE)),
            (json!({ "min_base_fps": "fast" }), None),
            (json!({ "enabled": "yes" }), None),
            (json!({ "debug_view": 1, "hud": "ui_texture" }), None),
        ];
        for (params, code) in cases {
            let err = set_with(&mut world, &params, ready())
                .err()
                .unwrap_or_else(|| panic!("{params} must be refused"));
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(error_code(&err), code, "{params}");
            assert_eq!(
                world.resource::<FrameGenSettings>(),
                &FrameGenSettings::default(),
                "{params}"
            );
        }
        for code in [FRAME_GEN_UNAVAILABLE, UNKNOWN_HUD_MODE, OUT_OF_RANGE] {
            assert!(
                code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'),
                "{code}"
            );
        }
    }

    /// `1e308` is a finite JSON number that does not fit in f32. The refusal must be
    /// `out_of_range`, not a panic while building the error payload, and the settings stay.
    #[test]
    fn min_base_fps_overflow_is_an_error_not_a_panic() {
        let mut world = world();
        let err = set_with(&mut world, &json!({ "min_base_fps": 1.0e308 }), ready()).expect_err("overflow");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(error_code(&err), Some(OUT_OF_RANGE));
        assert_eq!(world.resource::<FrameGenSettings>(), &FrameGenSettings::default());
    }

    #[test]
    fn without_the_render_plugin_both_methods_say_so() {
        let mut world = World::new();
        assert!(get(&mut world).is_err());
        assert!(set(&mut world, &json!({})).is_err());
    }

    #[test]
    fn the_pacer_shows_up_once_the_observer_runs() {
        let mut world = world();
        let mut pacing = FrameGenPacing::default();
        for i in 0..4u64 {
            pacing.observe(i * 10_000_000, false, false, 55.0);
        }
        world.insert_resource(pacing);
        let state = get(&mut world).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(state["pacing"]["last"]["kind"], json!("real_only"));
        assert_eq!(state["pacing"]["last"]["reason"], json!("disabled"));
        assert_eq!(state["pacing"]["stats"]["real_frames"], json!(4));
        assert_eq!(state["pacing"]["average_interval_ms"], json!(10.0));
        assert_eq!(state["pacing"]["present"]["generated_presents"], json!(0));
        assert_eq!(state["pacing"]["present"]["fallback"], Value::Null);
        assert_eq!(state["active"], json!(false));
    }

    #[test]
    fn the_present_path_shows_up_with_its_fallback_and_error() {
        let mut world = world();
        let mut pacing = FrameGenPacing::default();
        pacing.present.generated_presents = 7;
        pacing.present.real_presents = 8;
        pacing.present.resets = 1;
        pacing.present.fallback = Some(frame_gen::FrameGenFallback::Failed);
        pacing.present.last_error = Some("interpolator: test".into());
        world.insert_resource(pacing);
        let state = get(&mut world).unwrap_or_else(|e| panic!("{e:?}"));
        let present = &state["pacing"]["present"];
        assert_eq!(present["generated_presents"], json!(7));
        assert_eq!(present["real_presents"], json!(8));
        assert_eq!(present["resets"], json!(1));
        assert_eq!(present["fallback"], json!("failed"));
        assert_eq!(present["last_error"], json!("interpolator: test"));
    }
}
