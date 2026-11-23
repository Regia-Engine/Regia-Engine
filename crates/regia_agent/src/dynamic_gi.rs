// SPDX-License-Identifier: MPL-2.0
//! Dynamic GI tiers on the agent protocol (`book/agents/dynamic-gi.md`, `specs/79`, ADR-1679).
//!
//! `render.get_dynamic_gi` reports the SDF-traced probe field: the tier in effect, the player's
//! setting, the preset's tier, every tier's budget and, with a renderer, whether the probes ran,
//! their settling and their last measured GPU cost. `render.set_dynamic_gi` writes the tier to
//! [`RenderSettings::dynamic_gi_tier`] for the next extracted frame and can switch the probes on
//! or off (`enabled`, the lighting option `render.set_gi_mode` sets: `depth` or `balanced`).
//! With [`GameSettings`] the tier is also written to `graphics.gi_quality`, the options-menu value
//! that persists with the other graphics settings.
//!
//! `young_budget` (ADR-7870) caps the young probes the renderer's probe update traces in one frame (0 = off, every
//! tier); `render.set_dynamic_gi` sets it on the runtime renderer's probe settings, and every budget reports it
//! with `young_settle_frames`.
//!
//! `normal_bias` (ADR-3947) reports the existing normal offset in units of probe spacing,
//! for each tier and for the live renderer. `thin_wall_leak_fraction` is the reference lavapipe
//! two-room fixture's dark/lit floor ratio on a 0.4 m grid, not a measurement of the live scene.

use regia_app::prelude::*;
use regia_framework::settings::{GameSettings, GiQuality};
use regia_render::{DynamicGiBudget, DynamicGiTier, GiLook, RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

const TIERS: &str = "\"low\" (Deck, Desktop Low), \"medium\", \"high\" (Desktop High), \"ultra\" (Desktop \
                     Ultra) or \"default\" (the preset's tier)";

/// Reference dark/lit floor ratio for the 0.1 m shared wall on a 0.4 m grid with the default
/// tracer (ADR-3947, `regia_render` test `ddgi_thin_wall`, lavapipe). This is fixture reference
/// data, not the live scene's leakage; spec 27 § 2.2 bound is 0.005.
const THIN_WALL_LEAK_FRACTION: f32 = 0.0;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RenderSettings)",
    )
}

fn budget_json(b: &DynamicGiBudget) -> Value {
    json!({
        "probes": b.probes,
        "rays_per_frame": b.rays_per_frame,
        "probe_bytes": b.probe_bytes,
        "sdf_pool_bytes": b.sdf_pool_bytes,
        "reach_m": b.reach_m,
        "finest_above_m": b.finest_above_m,
        "finest_below_m": b.finest_below_m,
        "room_rule": b.room_rule,
        "screen_bytes": b.screen_bytes,
        "young_budget": b.young_budget,
        "young_settle_frames": b.young_settle_frames,
    })
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let tiers: Vec<Value> = DynamicGiTier::ALL
        .into_iter()
        .map(|t| {
            let mut v = budget_json(&t.budget());
            let ts = t.settings();
            v["name"] = json!(t.name());
            // ADR-2605: what the tier does when a probe starts over or a coarse cascade is blind.
            v["seed_updates"] = json!(ts.seed_updates);
            v["visibility_blend"] = json!(ts.visibility_blend);
            v["screen_max"] = json!(ts.screen_max);
            v["recent_weight"] = json!(ts.recent_weight);
            v["normal_bias"] = json!(ts.normal_bias);
            // ADR-5566, ADR-5716: whether the tier weighs a probe by which side of the point's surface it
            // stands on, and whether a probe past that surface's edge has to see the point without doubt.
            v["front_weights"] = json!(ts.front_weights);
            v["edge_visibility"] = json!(ts.edge_visibility);
            v
        })
        .collect();
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| {
        let s = r.0.dynamic_gi_status();
        let [trace, relocate, irradiance, distance, lookup] = s.gpu_stages_ms;
        json!({
            "tier": s.tier.name(),
            "custom": s.custom,
            "active": s.active,
            "enabled_override": s.enabled_override,
            "budget": budget_json(&s.budget),
            "settled_frames": (s.settled_frames != u32::MAX).then_some(s.settled_frames),
            "pending_bakes": s.pending_bakes,
            "emitters": s.emitters,
            "normal_bias": r.0.ddgi_settings().normal_bias,
            "young_budget": r.0.ddgi_settings().young_budget,
            "gpu_ms": s.gpu_ms,
            "gpu_stages_ms": {
                "trace": trace,
                "relocate": relocate,
                "irradiance": irradiance,
                "distance": distance,
                "lookup": lookup,
            },
            // ADR-7659: the heightfield max pyramid's rebuilds (a runtime height edit re-reduces only
            // the texels its samples reach).
            "pyramid": {
                "full_texels": s.pyramid.full_texels,
                "full_rebuilds": s.pyramid.full_rebuilds,
                "regional_rebuilds": s.pyramid.regional_rebuilds,
                "texels_reduced": s.pyramid.texels_reduced,
                "last_texels": s.pyramid.last_texels,
            },
        })
    });
    let look = settings.effective_gi_look();
    Ok(json!({
        "tier": settings.effective_dynamic_gi_tier().name(),
        "setting": settings.dynamic_gi_tier.map_or("default", DynamicGiTier::name),
        "preset_tier": settings.profile.gi_tier.name(),
        "lighting": look.name(),
        "probes": look.runs_probes(),
        "available": GiLook::depth_available(),
        "normal_bias": settings.effective_dynamic_gi_tier().settings().normal_bias,
        "thin_wall_leak_fraction": THIN_WALL_LEAK_FRACTION,
        "tiers": tiers,
        "renderer": renderer,
        "persisted": world
            .get_resource::<GameSettings>()
            .map(|s| s.graphics.gi_quality.map_or("default", |q| q.tier().name())),
    }))
}

/// `render.get_dynamic_gi`: the dynamic GI tier in effect, the setting, the preset's tier,
/// every tier's budget and the renderer's last frame.
pub(crate) fn get_dynamic_gi(world: &mut World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_dynamic_gi`: `tier` (a name or `"default"`), `enabled` (true = the probes'
/// lighting option `depth`, false = `balanced`, null = the preset's choice) and/or `young_budget` (young probes
/// traced a frame on the runtime renderer, 0 or null = off). Returns the state before and after.
pub(crate) fn set_dynamic_gi(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tier = match params.get("tier") {
        None => None,
        Some(Value::String(name)) if name == "default" => Some(None),
        Some(Value::String(name)) => match DynamicGiTier::parse(name) {
            Some(t) => Some(Some(t)),
            None => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "unknown_gi_tier",
                    format!("unknown dynamic GI tier {name:?}"),
                    format!("use {TIERS}"),
                ));
            }
        },
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`tier` must be a string ({TIERS}), got {other}"
            )));
        }
    };
    let enabled = match params.get("enabled") {
        None => None,
        Some(Value::Bool(on)) => Some(Some(*on)),
        Some(Value::Null) => Some(None),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`enabled` must be true, false or null, got {other}"
            )));
        }
    };
    let young_budget = match params.get("young_budget") {
        None => None,
        Some(Value::Null) => Some(0),
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) => Some(n),
            None => {
                return Err(RpcError::invalid_params(format!(
                    "`young_budget` must be a non-negative integer (young probes traced a frame; 0 or null = off), \
                     got {v}"
                )));
            }
        },
    };
    if tier.is_none() && enabled.is_none() && young_budget.is_none() {
        return Err(RpcError::invalid_params(format!(
            "give `tier` ({TIERS}), `enabled` (true, false or null) and/or `young_budget` (an integer)"
        )));
    }
    if young_budget.is_some() && world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "dynamic_gi_no_renderer",
            "`young_budget` is a setting of the runtime renderer's probes, and this app has none",
            "set it on an app with regia_render's RenderPlugin, or call FrameRenderer::set_ddgi_settings",
        ));
    }
    if enabled == Some(Some(true)) && !GiLook::depth_available() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "dynamic_gi_unavailable",
            "the GPU-traced probes do not run on this platform",
            "use the balanced lighting option (render.set_gi_mode {mode: \"balanced\"})",
        ));
    }
    let before = state(world)?;
    if let Some(tier) = tier
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
    {
        settings.dynamic_gi_tier = tier;
    }
    // The options-menu value persists with the other graphics settings.
    if let Some(tier) = tier
        && let Some(mut game) = world.get_resource_mut::<GameSettings>()
    {
        let quality = tier.map(GiQuality::of_tier);
        if game.graphics.gi_quality != quality {
            game.graphics.gi_quality = quality;
        }
    }
    // ADR-7870: the budget is the host's probe setting (no tier writes it), so it stays through a tier change.
    if let Some(budget) = young_budget
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
        && renderer.0.ddgi_settings().young_budget != budget
    {
        let mut settings = renderer.0.ddgi_settings().clone();
        settings.young_budget = budget;
        renderer.0.set_ddgi_settings(settings);
    }
    if let Some(enabled) = enabled {
        let mode = match enabled {
            Some(true) => "depth",
            Some(false) => "balanced",
            None => "default",
        };
        crate::gi_mode::set_gi_mode(world, &json!({ "mode": mode }))?;
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; a new tier recreates the probes, which \
                 settle again in about 8 frames (30 after a camera cut)",
    }))
}

fn gather_state() -> Value {
    let on = regia_render::ssgi_quarter_gather();
    json!({
        "quarter": on,
        "step": if on { json!(regia_render::SSGI_GATHER_QUARTER) } else { Value::Null },
    })
}

/// `render.get_ssgi_gather`: whether the screen-space gather runs at quarter resolution.
///
/// `quarter` is the override. `step` is 4 when it is on and null when the preset chooses
/// 1 or 2 (ADR-3964). The setting is process-wide; the call still needs `RenderSettings`.
pub(crate) fn get_ssgi_gather(world: &mut World) -> Result<Value, RpcError> {
    world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    Ok(gather_state())
}

/// `render.set_ssgi_gather`: `step` 4 turns the quarter gather on; 1 and 2 clear it and leave
/// the preset (or `FrameRenderer::set_ssgi_half_res`) to choose 1 or 2. Any other non-negative integer is
/// `ssgi_gather_bad_step`.
pub(crate) fn set_ssgi_gather(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let step = match crate::params::opt_u64(params, "step")? {
        Some(step) => step,
        None => {
            return Err(RpcError::invalid_params(
                "`step` is required (1 native, 2 half, or 4 quarter)",
            ));
        }
    };
    let quarter = match step {
        1 | 2 => false,
        4 => true,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "ssgi_gather_bad_step",
                format!("screen-space GI gather step {step} is not 1, 2 or 4"),
                "pass 4 to gather at quarter resolution, or 1 or 2 to leave the preset's step",
            ));
        }
    };
    let before = gather_state();
    regia_render::set_ssgi_settings(regia_render::ScreenSpaceGiSettings {
        quarter_gather: quarter,
    });
    Ok(json!({
        "before": before,
        "after": gather_state(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    fn world(preset: RenderProfilePreset) -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(preset));
        world
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn get_reports_the_preset_tier_and_every_budget() {
        let mut w = world(RenderProfilePreset::Deck);
        let s = get_dynamic_gi(&mut w).expect("state");
        assert_eq!(s["tier"], "low");
        assert_eq!(s["setting"], "default");
        assert_eq!(s["preset_tier"], "low");
        assert_eq!(s["tiers"].as_array().map(Vec::len), Some(4));
        assert_eq!(s["tiers"][0]["name"], "low");
        assert_eq!(s["tiers"][0]["probes"], 4_608);
        assert_eq!(s["tiers"][2]["probes"], 19_200);
        // The finest cascade's full weight around the eye, every tier (specs/79 § 7).
        assert_eq!(s["tiers"][0]["finest_above_m"], 4.0);
        assert_eq!(s["tiers"][0]["finest_below_m"], 3.0);
        // ADR-5290: High's 1 m finest cascade in 12 layers is placed for 3.5 m over and 3 m under the eye;
        // ADR-5909: the lookup holds half a cell more at each end (Low is placed for 3 m over and 2 m under).
        assert_eq!(s["tiers"][2]["finest_above_m"], 4.0);
        assert_eq!(s["tiers"][2]["finest_below_m"], 3.5);
        assert_eq!(s["tiers"][0]["room_rule"], true);
        // ADR-2605: Low seeds probes and blends by visibility on a smaller lookup region; High does too
        // since ADR-5001, on the default region.
        assert_eq!(s["tiers"][0]["seed_updates"], 4);
        assert_eq!(s["tiers"][0]["visibility_blend"], true);
        assert_eq!(s["tiers"][0]["screen_max"], json!([960, 540]));
        assert_eq!(s["tiers"][0]["screen_bytes"], 960 * 540 * 32);
        assert_eq!(s["tiers"][2]["seed_updates"], 4);
        assert_eq!(s["tiers"][2]["visibility_blend"], true);
        assert_eq!(s["tiers"][2]["screen_bytes"], 1920 * 1080 * 32);
        assert_eq!(s["tiers"][0]["recent_weight"], 3);
        // ADR-5716: every tier takes the front weights, the 2 m tiers the edge visibility.
        assert_eq!(s["tiers"][0]["front_weights"], true);
        assert_eq!(s["tiers"][1]["front_weights"], true);
        assert_eq!(s["tiers"][2]["front_weights"], true);
        assert_eq!(s["tiers"][0]["edge_visibility"], true);
        assert_eq!(s["tiers"][1]["edge_visibility"], true);
        assert_eq!(s["tiers"][2]["edge_visibility"], false);
        assert_eq!(s["tiers"][3]["edge_visibility"], false);
        assert_eq!(s["tiers"][2]["recent_weight"], 3);
        // ADR-3947: report the existing bias and the fixture reference separately.
        assert_eq!(s["tiers"][0]["normal_bias"], json!(0.1f32));
        assert_eq!(s["tiers"][2]["normal_bias"], json!(0.1f32));
        assert_eq!(s["normal_bias"], json!(0.1f32));
        assert_eq!(s["thin_wall_leak_fraction"], 0.0);
        assert!(s["renderer"].is_null(), "no renderer in this world");
        let mut w = world(RenderProfilePreset::DesktopUltra);
        assert_eq!(get_dynamic_gi(&mut w).expect("state")["tier"], "ultra");
    }

    #[test]
    fn normal_bias_reports_the_live_renderer_override_separately_from_the_tier() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let mut renderer = regia_render::FrameRenderer::new();
        renderer.set_ddgi_settings(regia_render::DdgiSettings {
            normal_bias: 0.25,
            ..regia_render::DdgiSettings::default()
        });
        w.insert_resource(RuntimeRenderer(renderer));
        let state = get_dynamic_gi(&mut w).expect("state");
        assert_eq!(state["normal_bias"], json!(0.1f32));
        assert_eq!(state["renderer"]["normal_bias"], 0.25);
        assert_eq!(state["thin_wall_leak_fraction"], 0.0);
        // ADR-7659: no frame yet, so no max pyramid was built.
        let pyramid = &state["renderer"]["pyramid"];
        for key in [
            "full_texels",
            "full_rebuilds",
            "regional_rebuilds",
            "texels_reduced",
            "last_texels",
        ] {
            assert_eq!(pyramid[key], 0, "{key}");
        }
    }

    #[test]
    fn set_writes_the_tier_and_default_clears_it() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let r = set_dynamic_gi(&mut w, &json!({ "tier": "low" })).expect("set");
        assert_eq!(r["before"]["tier"], "high");
        assert_eq!(r["after"]["tier"], "low");
        assert_eq!(r["after"]["setting"], "low");
        assert_eq!(
            w.resource::<RenderSettings>().dynamic_gi_tier,
            Some(DynamicGiTier::Low)
        );
        let r = set_dynamic_gi(&mut w, &json!({ "tier": "default" })).expect("set");
        assert_eq!(r["after"]["tier"], "high");
        assert_eq!(w.resource::<RenderSettings>().dynamic_gi_tier, None);
        assert!(r["after"]["persisted"].is_null(), "no GameSettings in this world");
    }

    #[test]
    fn set_persists_the_menu_value_with_game_settings() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        w.insert_resource(GameSettings::default());
        let r = set_dynamic_gi(&mut w, &json!({ "tier": "medium" })).expect("set");
        assert_eq!(r["before"]["persisted"], "default");
        assert_eq!(r["after"]["persisted"], "medium");
        assert_eq!(
            w.resource::<GameSettings>().graphics.gi_quality,
            Some(GiQuality::Medium)
        );
        set_dynamic_gi(&mut w, &json!({ "tier": "default" })).expect("set");
        assert_eq!(w.resource::<GameSettings>().graphics.gi_quality, None);
    }

    #[test]
    fn enabled_switches_the_lighting_option() {
        let mut w = world(RenderProfilePreset::Deck);
        let r = set_dynamic_gi(&mut w, &json!({ "enabled": false })).expect("off");
        assert_eq!(r["after"]["lighting"], "balanced");
        assert_eq!(r["after"]["probes"], false);
        if GiLook::depth_available() {
            let r = set_dynamic_gi(&mut w, &json!({ "enabled": true, "tier": "medium" })).expect("on");
            assert_eq!(r["after"]["lighting"], "depth");
            assert_eq!(r["after"]["probes"], true);
            assert_eq!(r["after"]["tier"], "medium");
        } else {
            let err = set_dynamic_gi(&mut w, &json!({ "enabled": true })).expect_err("unavailable");
            assert_eq!(code(&err), "dynamic_gi_unavailable");
        }
        let r = set_dynamic_gi(&mut w, &json!({ "enabled": null })).expect("default");
        assert_eq!(
            r["after"]["lighting"],
            GiLook::of_profile(&w.resource::<RenderSettings>().profile).name()
        );
    }

    #[test]
    fn young_budget_sets_the_renderers_probes_and_survives_a_tier() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        // Without a runtime renderer there is nothing to set, and nothing changes.
        let err =
            set_dynamic_gi(&mut w, &json!({ "young_budget": 4096, "tier": "low" })).expect_err("no renderer");
        assert_eq!(code(&err), "dynamic_gi_no_renderer");
        assert_eq!(w.resource::<RenderSettings>().dynamic_gi_tier, None);
        w.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let s = get_dynamic_gi(&mut w).expect("state");
        assert_eq!(s["renderer"]["young_budget"], 0, "off by default");
        assert_eq!(s["tiers"][0]["young_budget"], 0);
        assert_eq!(s["tiers"][0]["young_settle_frames"], 16);
        let out = set_dynamic_gi(&mut w, &json!({ "young_budget": 4096 })).expect("set");
        assert_eq!(out["before"]["renderer"]["young_budget"], 0);
        assert_eq!(out["after"]["renderer"]["young_budget"], 4096);
        // High's 19 200 probes at 4 096 a frame: 75 frames, then at most 16 more.
        assert_eq!(
            out["after"]["renderer"]["budget"]["young_settle_frames"],
            16 * 19_200 / 4096 + 16
        );
        assert_eq!(
            out["after"]["renderer"]["custom"], false,
            "the budget is no tier field"
        );
        let renderer = &mut w.resource_mut::<RuntimeRenderer>().0;
        renderer.set_dynamic_gi_tier(regia_render::DynamicGiTier::Low);
        assert_eq!(
            renderer.ddgi_settings().young_budget,
            4096,
            "a tier change keeps the budget"
        );
        let out = set_dynamic_gi(&mut w, &json!({ "young_budget": null })).expect("off");
        assert_eq!(out["after"]["renderer"]["young_budget"], 0);
        for bad in [json!(-1), json!(1.5), json!("many"), json!(5_000_000_000u64)] {
            let err = set_dynamic_gi(&mut w, &json!({ "young_budget": bad })).expect_err("bad budget");
            assert_eq!(code(&err), "F0002", "{bad}");
        }
    }

    #[test]
    fn bad_params_are_refused_with_their_codes() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let err = set_dynamic_gi(&mut w, &json!({ "tier": "epic" })).expect_err("unknown");
        assert_eq!(code(&err), "unknown_gi_tier");
        let err = set_dynamic_gi(&mut w, &json!({ "tier": 3 })).expect_err("not a string");
        assert_eq!(code(&err), "F0002");
        let err = set_dynamic_gi(&mut w, &json!({ "enabled": "yes" })).expect_err("not a bool");
        assert_eq!(code(&err), "F0002");
        let err = set_dynamic_gi(&mut w, &json!({})).expect_err("nothing to set");
        assert_eq!(code(&err), "F0002");
        let mut empty = World::new();
        let err = get_dynamic_gi(&mut empty).expect_err("no settings");
        assert_eq!(code(&err), "F0003");
        // A refused call changes nothing.
        assert_eq!(w.resource::<RenderSettings>().dynamic_gi_tier, None);
    }

    #[test]
    fn ssgi_gather_step_turns_quarter_on_and_1_or_2_clear_it() {
        // Unrelated unit tests render frames in parallel. Keep this process-wide switch's
        // mutations in a child running only this test, so those frames keep their defaults.
        const CHILD: &str = "REGIA_SSGI_GATHER_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("unit test executable"))
                .args([
                    "--exact",
                    "dynamic_gi::tests::ssgi_gather_step_turns_quarter_on_and_1_or_2_clear_it",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .expect("isolated gather settings test");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "isolated gather test: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let off = get_ssgi_gather(&mut w).expect("get");
        assert_eq!(off["quarter"], false);
        assert!(off["step"].is_null());
        let _restore = regia_render::SsgiGatherGuard::quarter(false);
        let on = set_ssgi_gather(&mut w, &json!({ "step": 4 })).expect("quarter");
        assert_eq!(on["before"]["quarter"], false);
        assert_eq!(on["after"]["quarter"], true);
        assert_eq!(on["after"]["step"], 4);
        assert!(regia_render::ssgi_quarter_gather());
        for step in [0, 3, u64::MAX] {
            let err = set_ssgi_gather(&mut w, &json!({ "step": step })).expect_err("unsupported step");
            assert_eq!(code(&err), "ssgi_gather_bad_step");
            assert!(
                regia_render::ssgi_quarter_gather(),
                "a refused step preserves quarter"
            );
        }
        for step in [json!(-1), json!(4.5), json!(true), Value::Null, json!("4")] {
            let err = set_ssgi_gather(&mut w, &json!({ "step": step })).expect_err("bad parameter type");
            assert_eq!(code(&err), "F0002");
            assert!(regia_render::ssgi_quarter_gather(), "bad params preserve quarter");
        }
        let cleared = set_ssgi_gather(&mut w, &json!({ "step": 2 })).expect("clear");
        assert_eq!(cleared["after"]["quarter"], false);
        assert!(cleared["after"]["step"].is_null());
        set_ssgi_gather(&mut w, &json!({ "step": 1 })).expect("native clears too");
        assert!(!regia_render::ssgi_quarter_gather());
        let err = set_ssgi_gather(&mut w, &json!({ "step": 3 })).expect_err("bad step");
        assert_eq!(code(&err), "ssgi_gather_bad_step");
        assert!(
            !regia_render::ssgi_quarter_gather(),
            "a refused step leaves the override off"
        );
        let err = set_ssgi_gather(&mut w, &json!({})).expect_err("missing step");
        assert_eq!(code(&err), "F0002");
        let err = set_ssgi_gather(&mut w, &json!({ "step": "4" })).expect_err("not an integer");
        assert_eq!(code(&err), "F0002");
        let mut empty = World::new();
        let err = get_ssgi_gather(&mut empty).expect_err("no settings");
        assert_eq!(code(&err), "F0003");
        let err = set_ssgi_gather(&mut empty, &json!({ "step": 4 })).expect_err("no settings");
        assert_eq!(code(&err), "F0003");
        assert!(!regia_render::ssgi_quarter_gather());
    }
}
