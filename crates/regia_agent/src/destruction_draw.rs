// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the destruction chunk draw path (`regia_framework::destruction_draw`,
//! book `agents/destruction-draw.md`): `physics.destruction_draw` reports what the draw path holds
//! and sets its options.
//!
//! Without `DestructionDrawPlugin` the method answers `NOT_FOUND` (`F0003`) naming the plugin.

use regia_app::prelude::*;
use regia_framework::destruction_draw::{DestructionDrawSettings, DestructionDrawStats, DestructionMeshes};
use regia_physics::destruction::Destruction;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn not_installed() -> RpcError {
    RpcError::not_found(
        "no DestructionDrawSettings resource in this app",
        "add regia_framework::destruction_draw::DestructionDrawPlugin (with DestructionPlugin) to draw chunks",
    )
}

/// `physics.destruction_draw { debris_lod?, hide_intact? }`: the draw path's settings and counters,
/// and the asset names with a mesh set (`book/agents/destruction-draw.md`).
pub(crate) fn destruction_draw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<DestructionDrawStats>() {
        return Err(not_installed());
    }
    // Every parameter is checked before any applies: a refused call changes nothing.
    let debris_lod = match params.get("debris_lod") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`debris_lod` must be an integer 0 to 255"))?,
        ),
    };
    let hide_intact = match params.get("hide_intact") {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`hide_intact` must be true or false"))?,
        ),
    };
    let settings = {
        let Some(mut settings) = world.get_resource_mut::<DestructionDrawSettings>() else {
            return Err(not_installed());
        };
        if let Some(v) = debris_lod {
            settings.debris_lod = v;
        }
        if let Some(v) = hide_intact {
            settings.hide_intact = v;
        }
        *settings
    };
    let changed = debris_lod.is_some() || hide_intact.is_some();
    let mesh_sets: Vec<&str> = world
        .get_resource::<DestructionMeshes>()
        .map(|m| m.names().collect())
        .unwrap_or_default();
    let Some(stats) = world.get_resource::<DestructionDrawStats>() else {
        return Err(not_installed());
    };
    Ok(json!({
        "changed": changed,
        "destruction": world.contains_resource::<Destruction>(),
        "settings": {
            "debris_lod": settings.debris_lod,
            "hide_intact": settings.hide_intact,
        },
        "mesh_sets": mesh_sets,
        "stats": {
            "instances_drawn": stats.instances_drawn,
            "attached_draws": stats.attached_draws,
            "debris_draws": stats.debris_draws,
            "intact_hidden": stats.intact_hidden,
            "missing_sets": stats.missing_sets,
            "mismatched_sets": stats.mismatched_sets,
            "missing": stats.missing,
        },
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_framework::destruction_draw::{DestructionDrawPlugin, DestructionMeshSet};
    use regia_render::CpuMaterial;

    use super::*;

    fn regia_code(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .map(str::to_string)
    }

    #[test]
    fn the_draw_method_reports_and_sets_the_options() {
        let mut app = App::new();
        let err = destruction_draw(app.world_mut(), &json!({})).expect_err("no plugin yet");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        assert_eq!(regia_code(&err).as_deref(), Some("F0003"));
        assert!(err.message.contains("DestructionDrawSettings"), "{}", err.message);

        app.add_plugins(DestructionDrawPlugin);
        let world = app.world_mut();
        let v = destruction_draw(world, &json!({})).expect("status");
        assert_eq!(v["changed"], false);
        assert_eq!(v["destruction"], false);
        assert_eq!(v["settings"]["debris_lod"], 0);
        assert_eq!(v["settings"]["hide_intact"], true);
        assert_eq!(v["stats"]["instances_drawn"], 0);
        assert_eq!(v["stats"]["missing"], json!([]));
        assert_eq!(v["mesh_sets"], json!([]));

        let v = destruction_draw(world, &json!({ "debris_lod": 1, "hide_intact": false })).expect("set");
        assert_eq!(v["changed"], true);
        assert_eq!(v["settings"]["debris_lod"], 1);
        assert_eq!(v["settings"]["hide_intact"], false);
        assert_eq!(
            *world.resource::<DestructionDrawSettings>(),
            DestructionDrawSettings {
                debris_lod: 1,
                hide_intact: false
            }
        );

        // A refused call changes nothing, whichever parameter is the bad one.
        for refused in [
            json!({ "debris_lod": 256 }),
            json!({ "debris_lod": -1 }),
            json!({ "debris_lod": 1.5 }),
            json!({ "debris_lod": "2" }),
            json!({ "hide_intact": "yes" }),
            json!({ "debris_lod": 3, "hide_intact": 1 }),
            json!({ "hide_intact": true, "debris_lod": null }),
        ] {
            let err = destruction_draw(world, &refused).expect_err("refused");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{refused}");
            let v = destruction_draw(world, &json!({})).expect("status");
            assert_eq!(v["settings"]["debris_lod"], 1, "{refused}");
            assert_eq!(v["settings"]["hide_intact"], false, "{refused}");
        }

        world.resource_mut::<DestructionMeshes>().register(
            "wall",
            DestructionMeshSet {
                chunks: Vec::new(),
                surface_material: Arc::new(CpuMaterial::default()),
                cut_material: Arc::new(CpuMaterial::default()),
            },
        );
        let v = destruction_draw(world, &json!({})).expect("status");
        assert_eq!(v["mesh_sets"], json!(["wall"]));
    }
}
