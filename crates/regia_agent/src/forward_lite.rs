// SPDX-License-Identifier: MPL-2.0
//! `render.get_forward_lite` / `render.set_forward_lite` (ADR-1874): which forward fragment shader
//! the frame's opaque and masked materials draw with, and an A/B override.

use regia_app::prelude::*;
use regia_render::forward_lite::ForwardLite;
use regia_render::{RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no runtime renderer: this app has no RenderSettings or RuntimeRenderer",
        "add regia_render::RenderPlugin",
    )
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?;
    let forced = renderer
        .0
        .forward_lite_override()
        .or_else(regia_render::forward_lite::recorded_env);
    let profile = settings.profile.forward_lite;
    Ok(json!({
        "tier": renderer.0.frame_forward_tier(profile).name(),
        "profile": profile.name(),
        "override": forced.map(ForwardLite::name),
    }))
}

/// `render.get_forward_lite`.
pub(crate) fn get_forward_lite(world: &World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_forward_lite`.
pub(crate) fn set_forward_lite(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("tier").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`tier` is required: \"auto\", \"off\", \"low\" or \"high\"")
    })?;
    let forced = if name == "auto" {
        None
    } else {
        Some(ForwardLite::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown tier {name:?}: use \"auto\", \"off\", \"low\" or \"high\""
            ))
        })?)
    };
    let before = state(world)?;
    world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(missing)?
        .0
        .set_forward_lite(forced);
    let after = state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{FrameRenderer, RenderProfilePreset};

    /// Restores the process-global `REGIA_FORWARD_LITE` record.
    struct RestoreEnv(Option<ForwardLite>);

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            let _ = regia_render::forward_lite::set_recorded_env(self.0);
        }
    }

    #[test]
    fn env_forward_lite_shows_up_in_override() {
        let previous = regia_render::forward_lite::set_recorded_env(Some(ForwardLite::Low));
        let _restore = RestoreEnv(previous);
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get_forward_lite(&world).expect("state");
        assert_eq!(got["override"], "low");
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_forward_lite(Some(ForwardLite::Off));
        let got = get_forward_lite(&world).expect("renderer wins");
        assert_eq!(got["override"], "off");
    }
}
