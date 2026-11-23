// SPDX-License-Identifier: MPL-2.0
//! The bindless step-2 census on the agent protocol (`book/agents/texture-sets.md`, ADR-356):
//! `render.texture_sets` reads the runtime renderer's census
//! ([`regia_render::FrameRenderer::texture_set_stats`]) and `render.set_texture_set_census`
//! turns its per-frame pass count on or off
//! ([`regia_render::FrameRenderer::set_texture_set_census`]).

use regia_app::prelude::*;
use regia_render::{RuntimeRenderer, TextureSetStats};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::texture_set_stats)",
    )
}

/// `material_runs / texture_set_runs`: how many times fewer material-group binds step 2 would
/// make in the observed order (1.0 when nothing would be saved or nothing was observed).
fn bind_ratio(s: &TextureSetStats) -> f64 {
    if s.texture_set_runs == 0 {
        1.0
    } else {
        s.material_runs as f64 / s.texture_set_runs as f64
    }
}

fn stats_json(s: &TextureSetStats) -> Value {
    json!({
        "census": s.census,
        "resident": {
            "standard_materials": s.standard_materials,
            "graph_materials": s.graph_materials,
            "texture_sets": s.texture_sets,
            "shared_materials": s.shared_materials,
            "largest_set": s.largest_set,
        },
        "pass": {
            "frames": s.frames,
            "opaque_draws": s.opaque_draws,
            "material_runs": s.material_runs,
            "texture_set_runs": s.texture_set_runs,
            "materials": s.pass_materials,
            "texture_sets": s.pass_texture_sets,
            "bind_ratio": bind_ratio(s),
        },
    })
}

/// `render.texture_sets`: `{ census, resident: { standard_materials, graph_materials,
/// texture_sets, shared_materials, largest_set }, pass: { frames, opaque_draws, material_runs,
/// texture_set_runs, materials, texture_sets, bind_ratio } }`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    Ok(stats_json(&renderer.0.texture_set_stats()))
}

/// `render.set_texture_set_census {enabled}`: turns the per-frame pass census on (`true`) or
/// off (`false`, the default). Returns the readings before and after the switch; a change
/// restarts the pass counts, which fill in with the next frame.
pub(crate) fn set_census(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .ok_or_else(|| RpcError::invalid_params("pass `enabled` (boolean)"))?
        .as_bool()
        .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?;
    let before = stats_json(&renderer.0.texture_set_stats());
    renderer.0.set_texture_set_census(enabled);
    let after = stats_json(&renderer.0.texture_set_stats());
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FrameRenderer;

    #[test]
    fn reads_and_switches_the_runtime_renderers_census() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get(&mut world).expect("read");
        assert_eq!(got["census"], false);
        assert_eq!(got["resident"]["standard_materials"], 0);
        assert_eq!(got["resident"]["texture_sets"], 0);
        assert_eq!(got["pass"]["frames"], 0);
        assert_eq!(got["pass"]["bind_ratio"], 1.0);
        let set = set_census(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(set["before"]["census"], false);
        assert_eq!(set["after"]["census"], true);
        assert!(world.resource::<RuntimeRenderer>().0.texture_set_census());
        assert!(set_census(&mut world, &json!({})).is_err());
        assert!(set_census(&mut world, &json!({ "enabled": "yes" })).is_err());
        let off = set_census(&mut world, &json!({ "enabled": false })).expect("set");
        assert_eq!(off["after"]["census"], false);
    }

    #[test]
    fn the_bind_ratio_is_runs_over_set_runs() {
        let mut s = TextureSetStats::default();
        assert!((bind_ratio(&s) - 1.0).abs() < f64::EPSILON);
        s.material_runs = 12;
        s.texture_set_runs = 4;
        assert!((bind_ratio(&s) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn without_a_renderer_it_says_what_to_add() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
        let err = set_census(&mut world, &json!({ "enabled": true })).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
    }
}
