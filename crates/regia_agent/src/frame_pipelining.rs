// SPDX-License-Identifier: MPL-2.0
//! `render.frame_pipelining`, `render.set_frame_pipelining`: the frame's CPU pipelining switch, what
//! the last frame applied and where its CPU time went (`book/agents/frame-pipelining.md`, ADR-2074).

use regia_ecs::prelude::World;
use regia_render::{RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Why the engine's own frame loop stays on one thread.
const RENDER_THREAD_NOTE: &str = "render_windows runs inline: engine systems take the renderer during the \
     simulation. regia_render::with_render_thread serves a host that drives FrameRenderer itself";

/// `render.frame_pipelining`: the profile's switch, the last frame's, and the last frame's CPU split.
pub(crate) fn frame_pipelining(world: &mut World) -> Result<Value, RpcError> {
    let profile = world
        .get_resource::<RenderSettings>()
        .map(|settings| settings.profile.frame_pipelining);
    let Some(renderer) = world.get_resource::<RuntimeRenderer>() else {
        return Err(RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin",
        ));
    };
    let applied = renderer.0.frame_pipelining();
    let cpu = renderer.0.cpu_timings();
    let shadow = renderer.0.shadow_cpu_timings();
    Ok(json!({
        "profile": profile.map(|p| json!({ "parallel_shadow_plan": p.parallel_shadow_plan })),
        "applied": {
            "parallel_shadow_plan": applied.parallel_shadow_plan,
            "on_tasks": renderer.0.shadow_plan_on_tasks(),
        },
        "encode_threads": renderer.0.encode_threads(),
        "render_thread": { "in_frame_loop": false, "note": RENDER_THREAD_NOTE },
        "last_frame_ms": {
            "total": cpu.total_ms,
            "encode": cpu.encode_ms,
            "submit": cpu.submit_ms,
            "readback": cpu.readback_ms,
            "shadow_plan": shadow.plan_ms,
        },
    }))
}

/// `render.set_frame_pipelining`: switches the active profile's `frame_pipelining`. Returns the
/// switch before and after.
pub(crate) fn set_frame_pipelining(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(parallel) = crate::params::opt_bool(params, "parallel_shadow_plan")? else {
        return Err(RpcError::invalid_params(
            "`parallel_shadow_plan` must be a boolean",
        ));
    };
    let Some(mut settings) = world.get_resource_mut::<RenderSettings>() else {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::frame_pipelining)",
        ));
    };
    let before = settings.profile.frame_pipelining.parallel_shadow_plan;
    if before != parallel {
        settings.profile.frame_pipelining.parallel_shadow_plan = parallel;
    }
    Ok(json!({
        "before": { "parallel_shadow_plan": before },
        "after": { "parallel_shadow_plan": parallel },
        "note": "applies from the next frame; the frames are the same either way. A preset change \
                 (render.set_profile) replaces the profile and turns it off",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_switches_the_profile_and_refuses_a_bad_value() {
        let mut world = World::new();
        assert!(
            set_frame_pipelining(&mut world, &json!({ "parallel_shadow_plan": true })).is_err(),
            "no RenderSettings"
        );
        world.insert_resource(RenderSettings::default());
        let reply =
            set_frame_pipelining(&mut world, &json!({ "parallel_shadow_plan": true })).expect("switch");
        assert_eq!(reply["before"]["parallel_shadow_plan"], false);
        assert_eq!(reply["after"]["parallel_shadow_plan"], true);
        assert!(
            world
                .resource::<RenderSettings>()
                .profile
                .frame_pipelining
                .parallel_shadow_plan
        );
        for bad in [json!({}), json!({ "parallel_shadow_plan": "yes" })] {
            assert!(set_frame_pipelining(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_report_needs_a_renderer() {
        let mut world = World::new();
        assert!(frame_pipelining(&mut world).is_err());
    }
}
