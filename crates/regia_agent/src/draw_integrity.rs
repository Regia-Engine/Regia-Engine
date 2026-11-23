// SPDX-License-Identifier: MPL-2.0
//! Render-world entry integrity and skinned bounds on the agent protocol
//! (`book/agents/draw-integrity.md`): `render.draw_integrity` reads the runtime renderer's
//! counters ([`regia_render::FrameRenderer::draw_integrity`]) and `render.set_skinned_bounds`
//! turns the joint-palette culling of skinned meshes on or off
//! ([`regia_render::FrameRenderer::set_skinned_bounds`]).

use regia_app::prelude::*;
use regia_render::{DrawIntegrity, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::draw_integrity)",
    )
}

fn integrity_json(i: &DrawIntegrity) -> Value {
    json!({
        "skipped_draws": i.skipped_draws,
        "skipped_draws_total": i.skipped_draws_total,
        "objects_rebuilt": i.objects_rebuilt,
        "objects_rebuilt_total": i.objects_rebuilt_total,
        "cull_missed_draws": i.cull_missed_draws,
        "cull_missed_draws_total": i.cull_missed_draws_total,
        "deferred_removals": i.deferred_removals,
        "deferred_removals_total": i.deferred_removals_total,
        "skinned": {
            "bounds": i.skinned_bounds,
            "bounded": i.skinned_bounded,
            "unbounded": i.skinned_unbounded,
            "culled": i.skinned_culled,
            "refused": i.skinned_refused,
        },
        // A healthy frame skipped nothing; a rebuilt object means a removal slipped past the
        // lifecycle (worth a bug report with the log's `object entry rebuilt` lines), and a
        // cull-missed draw means a mesh lost its cull bound after collection (the log's
        // `opaque draw missed by the GPU cull` lines).
        "healthy": i.skipped_draws_total == 0
            && i.objects_rebuilt_total == 0
            && i.cull_missed_draws_total == 0,
    })
}

/// `render.draw_integrity`: `{ skipped_draws, skipped_draws_total, objects_rebuilt,
/// objects_rebuilt_total, cull_missed_draws, cull_missed_draws_total, deferred_removals,
/// deferred_removals_total, skinned: { bounds, bounded, unbounded, culled, refused }, healthy }`
/// of the runtime renderer's last frame and since it was created. `refused` counts skinned draws
/// that weight a joint past their palette or the joint cap (ADR-5418): no pass drew them.
/// `cull_missed_draws` counts opaque draws the GPU cull did not take after collection sent them to
/// the GPU-driven batches; they were drawn per object (ADR-8028).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    Ok(integrity_json(&renderer.0.draw_integrity()))
}

/// `render.set_skinned_bounds {enabled}`: culls skinned meshes by their joint-palette bound
/// (`true`, the default) or draws every skinned mesh whatever the view (`false`). Returns the
/// counters before and after the switch (`after` counts the next frame only once it ran).
pub(crate) fn set_skinned_bounds(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .ok_or_else(|| RpcError::invalid_params("pass `enabled` (boolean)"))?
        .as_bool()
        .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?;
    let before = integrity_json(&renderer.0.draw_integrity());
    renderer.0.set_skinned_bounds(enabled);
    let after = integrity_json(&renderer.0.draw_integrity());
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FrameRenderer;

    #[test]
    fn reads_and_switches_the_runtime_renderers_counters() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get(&mut world).expect("read");
        assert_eq!(got["skipped_draws_total"], 0);
        assert_eq!(got["skinned"]["bounds"], true);
        assert_eq!(got["skinned"]["refused"], 0);
        assert_eq!(got["cull_missed_draws"], 0);
        assert_eq!(got["cull_missed_draws_total"], 0);
        assert_eq!(got["healthy"], true);
        let set = set_skinned_bounds(&mut world, &json!({ "enabled": false })).expect("set");
        assert_eq!(set["before"]["skinned"]["bounds"], true);
        assert_eq!(set["after"]["skinned"]["bounds"], false);
        assert!(set_skinned_bounds(&mut world, &json!({})).is_err());
        assert!(set_skinned_bounds(&mut world, &json!({ "enabled": 1 })).is_err());
        assert!(!world.resource::<RuntimeRenderer>().0.skinned_bounds());
    }

    #[test]
    fn without_a_renderer_it_says_what_to_add() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
    }
}
