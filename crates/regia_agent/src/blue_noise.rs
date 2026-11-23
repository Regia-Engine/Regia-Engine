// SPDX-License-Identifier: MPL-2.0
//! `render.blue_noise` and `render.set_blue_noise` (`book/agents/blue-noise.md`, ADR-2075): the
//! spatiotemporal blue-noise table and the active profile's knob.

use regia_ecs::prelude::*;
use regia_render::blue_noise::{BYTES, REVISION, SIZE, SLICES, table_hash};
use regia_render::{RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `render.blue_noise`: the knob, whether the last frame used the table, and the table's identity.
pub(crate) fn render_blue_noise(world: &World, _params: &Value) -> Result<Value, RpcError> {
    let requested = world
        .get_resource::<RenderSettings>()
        .is_some_and(|s| s.profile.blue_noise);
    let frame = world
        .get_resource::<RuntimeRenderer>()
        .map_or(Value::Null, |r| json!(r.0.blue_noise_active()));
    // The forward pass's sites (ADR-2652) need the view layout's binding 38 (desktop devices).
    let forward = world
        .get_resource::<RuntimeRenderer>()
        .and_then(|r| r.0.forward_blue_noise_binding())
        .map_or(Value::Null, |b| json!(b));
    Ok(json!({
        "requested": requested,
        "frame": frame,
        "sites": [
            "sssr_rays",
            "ssgi_rotation",
            "ssao_rotation",
            "forward_shadow_rotation",
            "forward_hashed_alpha",
            "pane_ssr_offset"
        ],
        "forward_binding": forward,
        "table": {
            "size": SIZE,
            "slices": SLICES,
            "bytes": BYTES,
            "revision": REVISION,
            "hash": format!("{:016x}", table_hash()),
        },
    }))
}

/// `render.set_blue_noise { enabled }`: switches the active profile's knob. Returns it before and
/// after.
pub(crate) fn set_blue_noise(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(enabled) = crate::params::opt_bool(params, "enabled")? else {
        return Err(RpcError::invalid_params("`enabled` must be a boolean"));
    };
    let Some(mut settings) = world.get_resource_mut::<RenderSettings>() else {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::blue_noise)",
        ));
    };
    let before = settings.profile.blue_noise;
    if before != enabled {
        settings.profile.blue_noise = enabled;
    }
    Ok(json!({
        "before": before,
        "after": enabled,
        "note": "applies from the next frame; a preset change (render.set_profile) replaces the \
                 profile and turns it off",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_works_without_a_renderer_and_the_switch_needs_settings() {
        let mut world = World::new();
        let read = render_blue_noise(&world, &json!({})).expect("read");
        assert_eq!(read["requested"], false);
        assert!(read["frame"].is_null());
        assert_eq!(read["table"]["bytes"], 65536);
        assert!(set_blue_noise(&mut world, &json!({ "enabled": true })).is_err());
        world.insert_resource(RenderSettings::default());
        assert!(set_blue_noise(&mut world, &json!({ "enabled": 1 })).is_err());
        let out = set_blue_noise(&mut world, &json!({ "enabled": true })).expect("switch");
        assert_eq!(
            (out["before"].as_bool(), out["after"].as_bool()),
            (Some(false), Some(true))
        );
        assert!(world.resource::<RenderSettings>().profile.blue_noise);
        assert_eq!(
            render_blue_noise(&world, &json!({})).expect("read")["requested"],
            true
        );
    }
}
