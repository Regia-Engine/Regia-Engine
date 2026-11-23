// SPDX-License-Identifier: MPL-2.0
//! The radiance cache behind screen-space GI on the agent protocol
//! (`book/agents/radiance-cache.md`, `specs/79` § 12, ADR-2085).
//!
//! `render.get_radiance_cache` reports whether screen-space GI reads the world-space radiance
//! cache: the switch in effect, the override, the preset's column and, with a renderer, what the
//! last frame's budget did. `render.set_radiance_cache` writes the override to
//! [`RenderSettings::radiance_cache`] for the next extracted frame and can change the cells
//! rewritten per frame.

use regia_app::prelude::*;
use regia_render::{
    GiLook, RadianceCacheSettings, RadianceCacheStatus, RadianceCacheTerm, RadianceCacheWork, RenderSettings,
    RuntimeRenderer,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RenderSettings)",
    )
}

fn status_json(s: &RadianceCacheStatus) -> Value {
    json!({
        "enabled": s.enabled,
        "active": s.active,
        "levels": s.settings.levels,
        "dims": s.settings.dims,
        "cells": s.settings.cells(),
        "cell_bytes": s.settings.cell_bytes(),
        "budget_cells": s.settings.budget_cells,
        "finest_spacing_m": s.finest_spacing_m,
        "reach_m": s.reach_m,
        "updated_cells": s.updated_cells,
        "scrolled_cells": s.scrolled_cells,
        "pending_cells": s.pending_cells,
        "idle": s.idle,
        "total_updated": s.total_updated,
        "gpu_bytes": s.gpu_bytes,
        // ADR-2605: the gather's cost by construction.
        "rays": s.settings.rays,
        "steps": s.settings.steps,
        "sky_early_out": s.settings.sky_early_out,
        "taps_per_pixel": s.settings.taps_per_pixel(),
        "gather": gather_name(&s.settings),
        "count_work": s.settings.count_work,
        "work": s.work.map(work_json),
        // ADR-4106: which term of the gather's sum the frame shows ("composite" = the shipped image).
        "attribution": s.settings.attribution.name(),
    })
}

/// A frame's counted gather work (`RadianceCacheStatus::work`).
fn work_json(w: RadianceCacheWork) -> Value {
    json!({
        "pixels": w.pixels,
        "rays": w.rays,
        "steps": w.steps,
        "cell_reads": w.cell_reads,
    })
}

/// `"economy"` when the gather's three cost tunables are [`RadianceCacheSettings::economy`]'s,
/// `"full"` when they are the defaults', else `"custom"`.
fn gather_name(s: &RadianceCacheSettings) -> &'static str {
    let tunables = |t: &RadianceCacheSettings| (t.rays, t.steps, t.sky_early_out);
    if tunables(s) == tunables(&RadianceCacheSettings::economy()) {
        "economy"
    } else if tunables(s) == tunables(&RadianceCacheSettings::default()) {
        "full"
    } else {
        "custom"
    }
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let renderer = world
        .get_resource::<RuntimeRenderer>()
        .map(|r| status_json(&r.0.radiance_cache_status()));
    let look = settings.effective_gi_look();
    Ok(json!({
        "radiance_cache": settings.effective_radiance_cache(),
        "setting": settings.radiance_cache,
        "preset": settings.profile.radiance_cache,
        "probes": look.runs_probes(),
        "screen_space_gi": settings.profile.screen_space_gi,
        "available": GiLook::depth_available(),
        "renderer": renderer,
    }))
}

/// `render.get_radiance_cache`: the switch in effect, the override, the preset's column and the
/// renderer's last frame.
pub(crate) fn get_radiance_cache(world: &mut World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_radiance_cache`: `enabled` (true, false, or null for the preset's column) and/or
/// `budget_cells` (cells rewritten per frame). Returns the state before and after.
pub(crate) fn set_radiance_cache(world: &mut World, params: &Value) -> Result<Value, RpcError> {
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
    let budget = match params.get("budget_cells") {
        None => None,
        Some(value) => match value
            .as_u64()
            .filter(|v| *v > 0)
            .and_then(|v| u32::try_from(v).ok())
        {
            Some(v) => Some(v),
            None => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "radiance_cache_bad_budget",
                    format!("`budget_cells` must be a positive integer, got {value}"),
                    "give the cells rewritten per frame (the default is 2048)",
                ));
            }
        },
    };
    let gather = match params.get("gather") {
        None => None,
        Some(Value::String(name)) if name == "full" => Some(RadianceCacheSettings::default()),
        Some(Value::String(name)) if name == "economy" => Some(RadianceCacheSettings::economy()),
        Some(other) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "radiance_cache_bad_gather",
                format!("`gather` must be \"full\" or \"economy\", got {other}"),
                "full: 8 rays of 8 steps a pixel (the default); economy: 4 rays of 6 steps taking turns, \
                 stopping at the sky (ADR-2605)",
            ));
        }
    };
    let count_work = match params.get("count_work") {
        None => None,
        Some(Value::Bool(on)) => Some(*on),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`count_work` must be true or false, got {other}"
            )));
        }
    };
    let attribution = match params.get("attribution") {
        None => None,
        Some(value) => match value.as_str().and_then(RadianceCacheTerm::from_name) {
            Some(term) => Some(term),
            None => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "radiance_cache_bad_attribution",
                    format!("`attribution` must name a term of the gather, got {value}"),
                    attribution_hint(),
                ));
            }
        },
    };
    if enabled.is_none()
        && budget.is_none()
        && gather.is_none()
        && count_work.is_none()
        && attribution.is_none()
    {
        return Err(RpcError::invalid_params(
            "give `enabled` (true, false or null for the preset's choice), `budget_cells`, \
             `gather` (\"full\" or \"economy\"), `count_work` (true or false) and/or `attribution` \
             (a term name, \"composite\" for the shipped image)",
        ));
    }
    if enabled == Some(Some(true)) && !GiLook::depth_available() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "radiance_cache_unavailable",
            "the radiance cache is fed by the GPU-traced probes, which do not run on this platform",
            "leave it off here: render.set_radiance_cache {enabled: null}",
        ));
    }
    // These three live on the frame renderer. Refuse before any write, including `enabled`
    // in the same call. `attribution` without a renderer still succeeds and stores nothing.
    if (budget.is_some() || gather.is_some() || count_work.is_some())
        && world.get_resource::<RuntimeRenderer>().is_none()
    {
        return Err(RpcError::not_found(
            "no RuntimeRenderer: budget_cells, gather and count_work are stored on the frame renderer",
            "add regia_render::RenderPlugin",
        ));
    }
    let before = state(world)?;
    if let Some(enabled) = enabled
        && let Some(mut settings) = world.get_resource_mut::<RenderSettings>()
    {
        settings.radiance_cache = enabled;
    }
    if let Some(budget) = budget
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
    {
        let mut settings = renderer.0.radiance_cache_settings();
        settings.budget_cells = budget;
        renderer.0.set_radiance_cache_settings(settings);
    }
    if let Some(gather) = gather
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
    {
        let mut settings = renderer.0.radiance_cache_settings();
        settings.rays = gather.rays;
        settings.steps = gather.steps;
        settings.sky_early_out = gather.sky_early_out;
        renderer.0.set_radiance_cache_settings(settings);
    }
    if let Some(count_work) = count_work
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
    {
        let mut settings = renderer.0.radiance_cache_settings();
        settings.count_work = count_work;
        renderer.0.set_radiance_cache_settings(settings);
    }
    if let Some(attribution) = attribution
        && let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>()
    {
        let mut settings = renderer.0.radiance_cache_settings();
        settings.attribution = attribution;
        renderer.0.set_radiance_cache_settings(settings);
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; it needs a frame that runs both the probes \
                 and screen-space GI (renderer.active), and a new budget refills the cells from \
                 empty",
    }))
}

/// The `radiance_cache_bad_attribution` hint: every term's name.
fn attribution_hint() -> String {
    let names: Vec<&str> = RadianceCacheTerm::ALL.iter().map(|t| t.name()).collect();
    format!(
        "one of {}; every term but composite and total is drawn on top of the frame it corrects \
         (ADR-4106)",
        names.join(", ")
    )
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
            .map_or_else(String::new, str::to_owned)
    }

    #[test]
    fn it_is_off_in_every_preset() {
        for preset in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
        ] {
            let mut w = world(preset);
            let s = get_radiance_cache(&mut w).expect("state");
            assert_eq!(s["radiance_cache"], false, "{preset:?}");
            assert_eq!(s["preset"], false, "{preset:?}");
            assert!(s["setting"].is_null());
            assert!(s["renderer"].is_null(), "no renderer in this world");
        }
    }

    #[test]
    fn set_writes_the_override_and_null_clears_it() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        if GiLook::depth_available() {
            let r = set_radiance_cache(&mut w, &json!({ "enabled": true })).expect("on");
            assert_eq!(r["before"]["radiance_cache"], false);
            assert_eq!(r["after"]["radiance_cache"], true);
            assert_eq!(w.resource::<RenderSettings>().radiance_cache, Some(true));
        }
        let r = set_radiance_cache(&mut w, &json!({ "enabled": false })).expect("off");
        assert_eq!(r["after"]["radiance_cache"], false);
        assert_eq!(w.resource::<RenderSettings>().radiance_cache, Some(false));
        let r = set_radiance_cache(&mut w, &json!({ "enabled": null })).expect("default");
        assert_eq!(r["after"]["setting"], Value::Null);
        assert_eq!(w.resource::<RenderSettings>().radiance_cache, None);
    }

    #[test]
    fn bad_parameters_have_their_codes_and_change_nothing() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        for bad in [json!(0), json!(-1), json!(1.5), json!("many"), json!(1u64 << 40)] {
            let err = set_radiance_cache(&mut w, &json!({ "budget_cells": bad })).expect_err("bad");
            assert_eq!(code(&err), "radiance_cache_bad_budget", "{bad}");
        }
        for bad in [json!("cheap"), json!(4), json!(null)] {
            let err = set_radiance_cache(&mut w, &json!({ "gather": bad })).expect_err("bad gather");
            assert_eq!(code(&err), "radiance_cache_bad_gather", "{bad}");
        }
        for bad in [json!("hits"), json!(3), json!(null), json!(true)] {
            let err = set_radiance_cache(&mut w, &json!({ "attribution": bad })).expect_err("bad term");
            assert_eq!(code(&err), "radiance_cache_bad_attribution", "{bad}");
        }
        set_radiance_cache(&mut w, &json!({ "enabled": "yes" })).expect_err("not a bool");
        set_radiance_cache(&mut w, &json!({})).expect_err("nothing to set");
        assert_eq!(w.resource::<RenderSettings>().radiance_cache, None);
    }

    #[test]
    fn no_render_settings_is_not_found() {
        let mut w = World::new();
        get_radiance_cache(&mut w).expect_err("no resource");
    }

    /// `budget_cells`, `gather` and `count_work` need a renderer, and a combined `enabled` is
    /// not written when that check fails. A bad budget is still its own code.
    #[test]
    fn set_radiance_cache_budget_without_renderer_is_not_found() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        w.resource_mut::<RenderSettings>().radiance_cache = Some(false);
        let err = set_radiance_cache(&mut w, &json!({ "budget_cells": 0 })).expect_err("bad budget");
        assert_eq!(code(&err), "radiance_cache_bad_budget");
        assert_eq!(w.resource::<RenderSettings>().radiance_cache, Some(false));
        for params in [
            json!({ "budget_cells": 32, "enabled": false }),
            json!({ "gather": "economy" }),
            json!({ "count_work": true }),
        ] {
            let err = set_radiance_cache(&mut w, &params).expect_err("no renderer");
            assert_eq!(code(&err), "F0003", "{params}");
            assert_eq!(
                w.resource::<RenderSettings>().radiance_cache,
                Some(false),
                "enabled is not written: {params}"
            );
        }
        set_radiance_cache(&mut w, &json!({ "attribution": "composite" }))
            .expect("attribution stores nothing");
    }
}

#[cfg(test)]
mod gather_tests {
    use super::*;

    /// ADR-2605: the gather's cost tunables have names, and `economy` makes at most 24 taps a pixel
    /// against the default's 64.
    #[test]
    fn the_gather_presets_are_named_and_economy_costs_less() {
        assert_eq!(gather_name(&RadianceCacheSettings::default()), "full");
        assert_eq!(gather_name(&RadianceCacheSettings::economy()), "economy");
        let custom = RadianceCacheSettings {
            rays: 2,
            ..RadianceCacheSettings::default()
        };
        assert_eq!(gather_name(&custom), "custom");
        assert_eq!(RadianceCacheSettings::default().taps_per_pixel(), 64);
        assert_eq!(RadianceCacheSettings::economy().taps_per_pixel(), 24);
    }

    /// ADR-4106: every attribution term is accepted by its name (no renderer here, so nothing changes),
    /// and the hint lists them all.
    #[test]
    fn every_attribution_term_is_accepted_by_name() {
        let mut w = World::new();
        w.insert_resource(RenderSettings::from_profile(
            regia_render::RenderProfilePreset::DesktopHigh,
        ));
        for term in RadianceCacheTerm::ALL {
            set_radiance_cache(&mut w, &json!({ "attribution": term.name() }))
                .unwrap_or_else(|e| panic!("{term:?}: {e:?}"));
            assert!(attribution_hint().contains(term.name()), "{term:?}");
        }
    }

    /// ADR-2605: the gather's counters reach the agent by name.
    #[test]
    fn the_work_count_reads_by_name() {
        let work = work_json(RadianceCacheWork {
            pixels: 1,
            rays: 2,
            steps: 3,
            cell_reads: 4,
        });
        assert_eq!(work["pixels"], 1);
        assert_eq!(work["rays"], 2);
        assert_eq!(work["steps"], 3);
        assert_eq!(work["cell_reads"], 4);
    }
}
