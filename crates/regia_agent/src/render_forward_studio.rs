// SPDX-License-Identifier: MPL-2.0
//! The forward shader's frame-level studio axis on the agent protocol
//! (`book/agents/forward-studio-axis.md`, ADR-2308): `render.forward_studio` reads the runtime
//! renderer's mode, the studio features on, the module the next frame composes and the forward
//! pipelines built for each module; `render.set_forward_studio` sets the mode (`auto`: the full
//! module only while a studio feature is on; `full`: the full module on every frame, an A/B
//! baseline).

use regia_app::prelude::*;
use regia_render::{ForwardStudioMode, ForwardStudioReport, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
         FrameRenderer::forward_studio_report and set_forward_studio_mode)",
    )
}

fn state(report: &ForwardStudioReport) -> Value {
    json!({
        "mode": report.mode.name(),
        "env_full": report.env_full,
        "adreno_workaround": report.adreno_workaround,
        "features": {
            "ground_plane": report.features.ground_plane,
            "captures": report.features.captures,
            "image_light_view": report.features.image_light_view,
            "any": report.features.any(),
        },
        "studio_off": report.studio_off,
        "pipelines": {
            "sets_studio_off": report.sets_studio_off,
            "sets_full": report.sets_full,
            "gpu_color_studio_off": report.gpu_color_studio_off,
            "gpu_color_full": report.gpu_color_full,
        },
    })
}

/// `render.forward_studio`: `{ mode, env_full, adreno_workaround, features: { ground_plane,
/// captures, image_light_view, any }, studio_off, pipelines: { sets_studio_off, sets_full,
/// gpu_color_studio_off, gpu_color_full } }`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    Ok(state(&renderer.0.forward_studio_report()))
}

/// `render.set_forward_studio { mode }`: sets the mode and answers the new state.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = crate::params::opt_str(params, "mode")?.unwrap_or_default();
    let mode = ForwardStudioMode::from_name(name).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "forward_studio_bad_mode",
            format!("unknown forward studio mode `{name}`"),
            "use \"auto\" (the full module only while a studio feature is on) or \"full\" (every frame)",
        )
    })?;
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?;
    renderer.0.set_forward_studio_mode(mode);
    Ok(state(&renderer.0.forward_studio_report()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{DebugViewMode, FrameRenderer};

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        world
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn reads_the_axis_and_follows_the_mode_and_the_debug_view() {
        let mut world = world();
        let got = get(&mut world).expect("read");
        assert_eq!(got["mode"], "auto");
        assert_eq!(got["features"]["any"], false);
        assert_eq!(got["pipelines"]["sets_full"], 0);
        assert_eq!(got["pipelines"]["sets_studio_off"], 0, "no frame drawn yet");
        let fixed = got["env_full"] == true || got["adreno_workaround"] == true;
        if !fixed {
            assert_eq!(
                got["studio_off"], true,
                "no studio feature: the studio-off module"
            );
        }
        let got = set(&mut world, &json!({ "mode": "full" })).expect("full");
        assert_eq!(got["mode"], "full");
        if !fixed {
            assert_eq!(got["studio_off"], false);
        }
        let got = set(&mut world, &json!({ "mode": "auto" })).expect("auto");
        assert_eq!(got["mode"], "auto");
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_image_light_view(DebugViewMode::ImageLight);
        let got = get(&mut world).expect("read");
        assert_eq!(got["features"]["image_light_view"], 1);
        assert_eq!(got["features"]["any"], true);
        assert_eq!(
            got["studio_off"], got["adreno_workaround"],
            "a studio feature takes the full module"
        );
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut world = world();
        for params in [json!({ "mode": "studio" }), json!({})] {
            let err = set(&mut world, &params).expect_err("bad mode");
            assert_eq!(code(&err), Some("forward_studio_bad_mode"), "{params}");
        }
        let mut empty = World::new();
        assert!(get(&mut empty).is_err());
        assert!(set(&mut empty, &json!({ "mode": "auto" })).is_err());
    }
}
