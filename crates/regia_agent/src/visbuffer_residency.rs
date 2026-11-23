// SPDX-License-Identifier: MPL-2.0
//! `render.get_visbuffer_residency` and `render.set_visbuffer_residency`: the visibility buffer's
//! cluster residency (ADR-3118, `book/agents/visbuffer-residency.md`). The getter reports the
//! budget, the bytes the managed meshes hold, per-band counts, evictions, upload bytes and the
//! refused and degraded meshes; the setter sets the runtime override
//! (`VisBufferSettings::residency`) or hands the choice back to the profile.

use regia_app::prelude::*;
use regia_render::visbuffer::{
    ClusterResidency, RESIDENCY_BANDS, VisBufferResidencyStats, VisBufferSettings,
};
use regia_render::{RenderProfilePreset, RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The parameters `render.set_visbuffer_residency` accepts.
const PARAMS: [&str; 7] = [
    "enabled",
    "budget_bytes",
    "upload_bytes_per_frame",
    "ram_budget_bytes",
    "io_bytes_per_frame",
    "tier",
    "follow_profile",
];

fn config_json(c: &ClusterResidency) -> Value {
    json!({
        "enabled": c.enabled,
        "budget_bytes": c.budget_bytes,
        "upload_bytes_per_frame": c.upload_bytes_per_frame,
        "ram_budget_bytes": c.ram_budget_bytes,
        "io_bytes_per_frame": c.io_bytes_per_frame,
    })
}

fn stats_json(s: &VisBufferResidencyStats) -> Value {
    let bands: Vec<Value> = (0..RESIDENCY_BANDS)
        .filter(|&b| s.bands_total[b] > 0)
        .map(|b| json!({ "band": b, "resident": s.bands_resident[b], "total": s.bands_total[b] }))
        .collect();
    json!({
        "meshes": s.meshes,
        "unmanaged_meshes": s.unmanaged_meshes,
        "resident_bytes": s.resident_bytes,
        "peak_resident_bytes": s.peak_resident_bytes,
        "full_bytes": s.full_bytes,
        "bands": bands,
        "pending_bands": s.pending_bands,
        "refinements": s.refinements,
        "evictions": s.evictions,
        "evicted_bytes": s.evicted_bytes,
        "upload_bytes": s.upload_bytes,
        "peak_upload_bytes": s.peak_upload_bytes,
        "upload_bytes_total": s.upload_bytes_total,
        "bound_overruns": s.bound_overruns,
        "base_upload_bytes": s.base_upload_bytes,
        "refused_meshes": s.refused_meshes,
        "degraded_meshes": s.degraded_meshes,
        "fallbacks": s.fallbacks,
        "io_bytes": s.io_bytes,
        "io_bytes_total": s.io_bytes_total,
        "read_queue_depth": s.read_queue_depth,
        "bands_on_disk": s.bands_on_disk,
        "bands_loading": s.bands_loading,
        "bands_in_ram": s.bands_in_ram,
        "ram_bytes": s.ram_bytes,
        "peak_ram_bytes": s.peak_ram_bytes,
        "ram_evictions": s.ram_evictions,
        "read_errors": s.read_errors,
        "last_read_error_code": s.last_read_error_code,
        "frames": s.frames,
    })
}

/// The state both methods report.
fn state(world: &World) -> Value {
    let profile = world
        .get_resource::<RenderSettings>()
        .map(|s| config_json(&s.profile.cluster_residency));
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| &r.0);
    let (override_config, stats) = match renderer {
        Some(r) => {
            let stats = r.visbuffer_arena_stats().residency;
            (
                Some(r.visbuffer_settings().residency.map(|c| config_json(&c))),
                Some(stats),
            )
        }
        None => (None, None),
    };
    let follows_profile = override_config.as_ref().is_some_and(Option::is_none);
    json!({
        "renderer": renderer.is_some(),
        "profile": profile,
        "override": override_config.flatten(),
        "follows_profile": follows_profile,
        "config": stats.as_ref().map(|s| config_json(&s.config)),
        "stats": stats.as_ref().map(stats_json),
    })
}

/// `render.get_visbuffer_residency`: read-only, answers in any world (nulls for what is absent).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(state(world))
}

fn u64_param(params: &Value, name: &str, min: u64) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| *n >= min).map(Some).ok_or_else(|| {
            RpcError::invalid_params(format!("`{name}` must be an integer of at least {min}"))
        }),
    }
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    }
}

/// `render.set_visbuffer_residency`: sets the runtime override of the cluster residency knobs, or
/// (`follow_profile: true`) clears it. `tier` takes a preset name and its numbers
/// (`ClusterResidency::for_preset`); `enabled`, `budget_bytes`, `upload_bytes_per_frame`,
/// `ram_budget_bytes` and `io_bytes_per_frame` then replace single fields. Nothing is applied
/// unless every parameter is valid.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(object) = params.as_object()
        && let Some(unknown) = object.keys().find(|k| !PARAMS.contains(&k.as_str()))
    {
        return Err(RpcError::invalid_params(format!(
            "unknown parameter `{unknown}`: pass {}",
            PARAMS.join(", ")
        )));
    }
    let enabled = bool_param(params, "enabled")?;
    let budget = u64_param(params, "budget_bytes", 0)?;
    let upload = u64_param(params, "upload_bytes_per_frame", 4)?;
    let ram = u64_param(params, "ram_budget_bytes", 0)?;
    let io = u64_param(params, "io_bytes_per_frame", 0)?;
    let follow = bool_param(params, "follow_profile")?;
    let tier = match params.get("tier") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(RenderProfilePreset::from_name)
                .map(ClusterResidency::for_preset)
                .ok_or_else(|| {
                    RpcError::invalid_params(
                        "`tier` must be a preset name: mobile_low, mobile_high, deck, desktop_low, \
                         desktop_high or desktop_ultra",
                    )
                })?,
        ),
    };
    let edits = enabled.is_some()
        || budget.is_some()
        || upload.is_some()
        || ram.is_some()
        || io.is_some()
        || tier.is_some();
    if follow == Some(true) && edits {
        return Err(RpcError::invalid_params(
            "`follow_profile: true` clears the override; pass it alone",
        ));
    }
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Err(RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             VisBufferSettings::residency through FrameRenderer::set_visbuffer_settings)",
        ));
    };
    let current: VisBufferSettings = renderer.0.visbuffer_settings();
    let before_config = renderer.0.visbuffer_arena_stats().residency.config;
    let residency = if follow == Some(true) {
        None
    } else if edits {
        let base = tier.unwrap_or(current.residency.unwrap_or(before_config));
        Some(ClusterResidency {
            enabled: enabled.unwrap_or(if tier.is_some() { true } else { base.enabled }),
            budget_bytes: budget.unwrap_or(base.budget_bytes),
            upload_bytes_per_frame: upload.unwrap_or(base.upload_bytes_per_frame),
            ram_budget_bytes: ram.unwrap_or(base.ram_budget_bytes),
            io_bytes_per_frame: io.unwrap_or(base.io_bytes_per_frame),
        })
    } else {
        current.residency
    };
    renderer
        .0
        .set_visbuffer_settings(VisBufferSettings { residency, ..current });
    let after = state(world);
    Ok(json!({
        "before": config_json(&before_config),
        "after": after,
        "note": "applies from the next frame. Meshes made resident while residency was off stay whole until \
                 their meshlets are inserted again or released; turned off, managed meshes load every band at \
                 once. The budget covers the managed meshes' records and geometry.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_renderer() -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        world.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        world
    }

    #[test]
    fn get_answers_in_a_bare_world_and_with_a_renderer() {
        let mut bare = World::new();
        let got = get(&mut bare).expect("bare");
        assert_eq!(got["renderer"], false);
        assert!(got["stats"].is_null());
        let mut world = world_with_renderer();
        let got = get(&mut world).expect("renderer");
        assert_eq!(got["renderer"], true);
        assert_eq!(got["follows_profile"], true);
        assert_eq!(got["config"]["enabled"], false, "off by default");
        assert_eq!(got["stats"]["meshes"], 0);
        assert_eq!(got["profile"]["enabled"], false);
    }

    #[test]
    fn stats_json_reports_ram_evictions() {
        let stats = VisBufferResidencyStats {
            ram_evictions: 4,
            ..Default::default()
        };
        let got = stats_json(&stats);
        assert_eq!(got["ram_evictions"], json!(4));
    }

    #[test]
    fn set_takes_a_tier_then_fields_then_follows_the_profile_again() {
        let mut world = world_with_renderer();
        let got = set(&mut world, &json!({ "tier": "deck" })).expect("tier");
        let deck = ClusterResidency::for_preset(RenderProfilePreset::Deck);
        assert_eq!(got["after"]["config"]["enabled"], true);
        assert_eq!(got["after"]["config"]["budget_bytes"], deck.budget_bytes);
        assert_eq!(got["after"]["follows_profile"], false);
        let got = set(&mut world, &json!({ "budget_bytes": 1_000_000 })).expect("field");
        assert_eq!(got["after"]["config"]["budget_bytes"], 1_000_000);
        assert_eq!(
            got["after"]["config"]["upload_bytes_per_frame"],
            deck.upload_bytes_per_frame
        );
        let got = set(&mut world, &json!({ "follow_profile": true })).expect("follow");
        assert_eq!(got["after"]["follows_profile"], true);
        assert_eq!(got["after"]["config"]["enabled"], false);
    }

    #[test]
    fn bad_parameters_apply_nothing() {
        let mut world = world_with_renderer();
        for bad in [
            json!({ "tier": "phone" }),
            json!({ "budget_bytes": -1 }),
            json!({ "upload_bytes_per_frame": 1 }),
            json!({ "enabled": "yes" }),
            json!({ "follow_profile": true, "enabled": true }),
            json!({ "budget": 5 }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.resource::<RuntimeRenderer>().0.visbuffer_settings(),
            VisBufferSettings::default()
        );
        let mut bare = World::new();
        let err = set(&mut bare, &json!({ "enabled": true })).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
    }
}
