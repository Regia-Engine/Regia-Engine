// SPDX-License-Identifier: MPL-2.0
//! Agent methods for order-independent transparency (`book/agents/oit.md`, ADR-2065): `render.oit`
//! reads what the last frame did, `render.set_oit` switches the profile's knob.

use regia_app::prelude::*;
use regia_render::{OitStats, RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn stats_json(stats: OitStats) -> Value {
    json!({
        "requested": stats.requested,
        "drawn": stats.drawn,
        "fallback": stats.fallback,
        "sorted": stats.sorted,
        "passes": stats.passes,
        "covered_permille": stats.covered_permille,
        "reactive_mask": stats.reactive_mask,
        "reason": stats.reason,
    })
}

/// `render.oit`: the active profile's knob and what the last frame did with it.
pub(crate) fn report(world: &World) -> Value {
    let requested = world
        .get_resource::<RenderSettings>()
        .is_some_and(|settings| settings.profile.order_independent_transparency);
    json!({
        "enabled": requested,
        "frame": world
            .get_resource::<RuntimeRenderer>()
            .map_or(Value::Null, |r| stats_json(r.0.oit_stats())),
    })
}

/// `render.set_oit { enabled }`: switches the active profile's knob. Returns the value before and
/// after.
pub(crate) fn set_enabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(enabled) = crate::params::opt_bool(params, "enabled")? else {
        return Err(RpcError::invalid_params("`enabled` must be true or false"));
    };
    let Some(mut settings) = world.get_resource_mut::<RenderSettings>() else {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::order_independent_transparency)",
        ));
    };
    let before = settings.profile.order_independent_transparency;
    if before != enabled {
        settings.profile.order_independent_transparency = enabled;
    }
    Ok(json!({
        "before": before,
        "after": enabled,
        "note": "applies from the next frame; read render.oit. Only alpha-blended and premultiplied meshes \
                 take it; glass and additive materials stay in the sorted run. A preset change \
                 (render.set_profile) replaces the profile and turns it off",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_world_without_settings_reports_off_and_refuses_the_switch() {
        let mut world = World::new();
        let got = report(&world);
        assert_eq!(got["enabled"], false);
        assert_eq!(got["frame"], Value::Null);
        let err = set_enabled(&mut world, &json!({ "enabled": true })).expect_err("no settings");
        assert_eq!(err.code, RpcError::not_found("", "").code);
    }

    #[test]
    fn the_switch_needs_a_boolean_and_reads_back() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        for bad in [json!({}), json!({ "enabled": 1 }), json!({ "enabled": "yes" })] {
            let err = set_enabled(&mut world, &bad).expect_err("not a boolean");
            assert_eq!(err.code, RpcError::invalid_params("").code);
        }
        let out = set_enabled(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["before"], false);
        assert_eq!(out["after"], true);
        assert_eq!(report(&world)["enabled"], true);
        let out = set_enabled(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(out["before"], true);
        assert_eq!(report(&world)["enabled"], false);
    }

    #[test]
    fn the_frame_block_holds_every_field_the_schema_lists() {
        let value = stats_json(OitStats {
            requested: true,
            drawn: 2,
            fallback: 1,
            sorted: 3,
            passes: 9,
            covered_permille: 120,
            reactive_mask: true,
            reason: None,
        });
        for field in [
            "requested",
            "drawn",
            "fallback",
            "sorted",
            "passes",
            "covered_permille",
            "reactive_mask",
            "reason",
        ] {
            assert!(value.get(field).is_some(), "{field}");
        }
        assert_eq!(value["sorted"], 3);
        assert_eq!(value["reason"], Value::Null);
        assert!(value.get("hair").is_none());
    }
}
