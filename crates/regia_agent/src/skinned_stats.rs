// SPDX-License-Identifier: MPL-2.0
//! The runtime renderer's draw counters and skinned instancing on the agent protocol
//! (`book/agents/skinned-instancing.md`, ADR-1368): `render.stats` reads the last frame's
//! [`regia_render::DrawStats`] (the skinned crowd counters included) and
//! `render.set_skinned_instancing` turns skinned instancing on or off
//! ([`regia_render::FrameRenderer::set_skinned_instancing`]).

use regia_app::prelude::*;
use regia_render::{FrameRenderer, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::draw_stats)",
    )
}

fn stats_json(
    renderer: &FrameRenderer,
    counted: Option<&regia_render::CountedAssetUploads>,
    cells: Option<&regia_render::VisCellStats>,
) -> Value {
    let vis = renderer.counted_vis_stats();
    let shadow = renderer.shadow_cache_report();
    let d = renderer.draw_stats();
    let cache = renderer.cache_stats();
    let budgets = renderer.cache_budgets();
    json!({
        "draws": {
            "drawn": d.drawn,
            "culled": d.culled,
            "instanced": d.instanced,
            "per_object": d.per_object,
            "double_sided_per_object": d.double_sided_per_object,
            "graph_instanced": d.graph_instanced,
            "graph_per_object": d.graph_per_object,
            "rigid_skin_folded": d.rigid_skin_folded,
            "zero_key_moving_meshes": d.zero_key_moving_meshes,
        },
        "skinned": {
            "instancing": renderer.skinned_instancing(),
            "bounds": renderer.skinned_bounds(),
            "draws": d.skinned_draws,
            "instanced": d.skinned_instanced,
            "per_object": d.skinned_draws.saturating_sub(d.skinned_instanced),
            "batches": d.skinned_batches,
            "batches_possible": d.skinned_batches_possible,
        },
        // The mesh cache as of the last eviction sweep (ADR-7330): its own entries against the
        // mesh budget, and the counted meshes it adopted from the asset server apart.
        "mesh_cache": {
            "meshes": cache.meshes,
            "mesh_bytes": budgets.mesh_bytes,
            "mesh_budget_bytes": budgets.mesh_budget_bytes,
            "evicted": cache.meshes_evicted,
            "adopted_meshes": budgets.adopted_meshes,
            "adopted_mesh_bytes": budgets.adopted_mesh_bytes,
            "adopted_skinned_meshes": renderer.adopted_skinned_asset_mesh_count(),
        },
        // Sun caster counts (ADR-8414): mesh casters per sun cascade in the last frame, and the
        // draws the cell cull took out of every cascade because they lie in a sun-sealed cell.
        "shadows": {
            "sun_casters_per_cascade": shadow.sun_casters_per_cascade,
            "sun_sealed_cells": cells.map_or(0, |c| c.sun_sealed_cells),
            "sun_sealed_draws": cells.map_or(0, |c| c.sun_sealed_draws),
        },
        // Counted textures (ADR-8610): material entries rebuilt for a new upload serial, and the
        // App's counted texture table (`null` without `CountedMeshPlugin`'s uploaders).
        "counted": {
            "texture_rebinds": renderer.counted_material_rebinds(),
            // LOD index lists uploaded for counted meshes' chain levels (ADR-7428).
            "lod_index_uploads": renderer.counted_lod_index_uploads(),
            // Counted meshes on the visibility buffer's cluster path (ADR-7462).
            "vis": {
                "meshes": vis.meshes,
                "clusters": vis.clusters,
                "repacks": vis.repacks,
                "forward_fallbacks": vis.forward_fallbacks,
                // The cooked clusters kept on the CPU for a hot reload (ADR-7542).
                "kept_meshlet_bytes": vis.kept_meshlet_bytes,
            },
            "textures": counted.map(|uploads| {
                let textures = &uploads.textures;
                let (uploads, frees) = textures.counters();
                json!({
                    "resident": textures.resident_count(),
                    "resident_bytes": textures.resident_bytes(),
                    "sibling_views": textures.sibling_view_count(),
                    "uploads": uploads,
                    "frees": frees,
                })
            }),
        },
    })
}

/// `render.stats`: `{ draws: { drawn, culled, instanced, per_object, double_sided_per_object,
/// graph_instanced, graph_per_object, rigid_skin_folded, zero_key_moving_meshes }, skinned:
/// { instancing, bounds, draws, instanced, per_object, batches, batches_possible }, mesh_cache:
/// { meshes, mesh_bytes, mesh_budget_bytes, evicted, adopted_meshes, adopted_mesh_bytes,
/// adopted_skinned_meshes }, shadows: { sun_casters_per_cascade, sun_sealed_cells,
/// sun_sealed_draws }, counted: { texture_rebinds, lod_index_uploads, vis: { meshes, clusters, repacks, forward_fallbacks, kept_meshlet_bytes }, textures: { resident, resident_bytes,
/// sibling_views, uploads, frees } | null }, interior: { restricted, frusta, mouths, views: [{ view,
/// restricted, frusta }], terrain: { restricted, nodes_drawn, nodes_culled, grass_patches_drawn,
/// grass_patches_culled, impostors_kept, impostors_culled } | null, scatter: { emitted,
/// portal_culled } | null, water: { restricted, bodies_drawn, bodies_culled, ring_chunks_culled } |
/// null } }`
/// of the runtime renderer's last main-view frame (the mesh cache as of its last eviction sweep).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let counted = world.get_resource::<regia_render::CountedAssetUploads>();
    let cells = world.get_resource::<regia_render::VisCellStats>();
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let mut out = stats_json(&renderer.0, counted, cells);
    out["interior"] = interior_json(world, &renderer.0);
    Ok(out)
}

/// What the views culled from inside a cell (plan row W-22, ADR-8413, ADR-8843,
/// `book/agents/viscells.md` "Terrain and scatter culling"): the extracted frame's outdoor
/// frusta (the union, and one line per window view that has a list), what the terrain draw left
/// out of its camera passes (nodes, grass patches, impostors) on the last view it drew, what the
/// runtime scatter left out, and what the water left out on the last view it drew. `terrain` is
/// `null` without a shared draw that culls, `scatter` without a `RuntimeScatter`, `water` without
/// a `WaterState`.
fn interior_json(world: &World, renderer: &FrameRenderer) -> Value {
    let frame = world.get_resource::<regia_render::ExtractedFrame>();
    let portals = frame.map(|frame| &frame.outdoor_portals);
    let views: Vec<Value> = frame
        .map(|frame| {
            frame
                .outdoor_portal_views
                .iter()
                .map(|(view, list)| {
                    json!({
                        "view": view,
                        "restricted": list.is_restricted(),
                        "frusta": list.frusta().len(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let terrain = renderer.outdoor_cull_counts().map(|c| {
        json!({
            "restricted": c.restricted,
            "nodes_drawn": c.nodes_drawn,
            "nodes_culled": c.nodes_culled,
            "grass_patches_drawn": c.grass_patches_drawn,
            "grass_patches_culled": c.grass_patches_culled,
            "impostors_kept": c.impostors_kept,
            "impostors_culled": c.impostors_culled,
        })
    });
    let water = world.get_resource::<regia_terrain::WaterState>().map(|w| {
        let s = w.renderer.stats();
        json!({
            "restricted": s.portal_restricted,
            "bodies_drawn": s.bodies_drawn,
            "bodies_culled": s.portal_culled_bodies,
            "ring_chunks_culled": s.portal_culled_ring_chunks,
        })
    });
    let scatter = world.get_resource::<regia_terrain::RuntimeScatter>().map(|s| {
        json!({
            "emitted": s.last_frame.emitted,
            "portal_culled": s.last_frame.portal_culled,
        })
    });
    json!({
        "restricted": portals.is_some_and(|p| p.is_restricted()),
        "frusta": portals.map_or(0, |p| p.frusta().len()),
        "mouths": portals.map_or(0, |p| p.mouths().len()),
        "views": views,
        "terrain": terrain,
        "scatter": scatter,
        "water": water,
    })
}

/// `render.set_skinned_instancing {enabled}`: skinned meshes with a joint-palette bound draw in
/// the GPU-driven instanced batches (`true`, the default) or one by one (`false`). Returns the
/// stats before and after the switch (`after` counts the next frame only once it ran).
pub(crate) fn set_skinned_instancing(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .ok_or_else(|| RpcError::invalid_params("pass `enabled` (boolean)"))?
        .as_bool()
        .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
    let counted = world.get_resource::<regia_render::CountedAssetUploads>().cloned();
    let cells = world.get_resource::<regia_render::VisCellStats>().cloned();
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)?;
    let before = stats_json(&renderer.0, counted.as_ref(), cells.as_ref());
    renderer.0.set_skinned_instancing(enabled);
    let after = stats_json(&renderer.0, counted.as_ref(), cells.as_ref());
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_counters_and_switches_skinned_instancing() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get(&mut world).expect("read");
        assert_eq!(got["draws"]["drawn"], 0);
        assert_eq!(got["skinned"]["instancing"], true);
        assert_eq!(got["skinned"]["draws"], 0);
        assert_eq!(got["skinned"]["batches_possible"], 0);
        assert_eq!(got["mesh_cache"]["adopted_meshes"], 0);
        assert_eq!(got["mesh_cache"]["adopted_mesh_bytes"], 0);
        assert_eq!(got["mesh_cache"]["evicted"], 0);
        assert_eq!(got["interior"]["restricted"], false);
        assert!(got["interior"]["terrain"].is_null() && got["interior"]["scatter"].is_null());
        assert!(got["interior"]["water"].is_null());
        assert_eq!(got["interior"]["views"], json!([]));
        assert_eq!(got["shadows"]["sun_casters_per_cascade"], json!([0, 0, 0, 0]));
        assert_eq!(got["shadows"]["sun_sealed_draws"], 0);
        let set = set_skinned_instancing(&mut world, &json!({ "enabled": false })).expect("set");
        assert_eq!(set["before"]["skinned"]["instancing"], true);
        assert_eq!(set["after"]["skinned"]["instancing"], false);
        assert!(set_skinned_instancing(&mut world, &json!({})).is_err());
        assert!(set_skinned_instancing(&mut world, &json!({ "enabled": "yes" })).is_err());
        assert!(!world.resource::<RuntimeRenderer>().0.skinned_instancing());
    }

    /// ADR-7330: `render.stats` through the dispatcher reports the mesh cache with the adopted
    /// counted meshes apart (none on a fresh renderer; the sweep's numbers once a frame ran).
    #[test]
    fn render_stats_reports_the_mesh_cache_with_adopted_counted_meshes_apart() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let v = crate::dispatch::call_in_process(&mut world, "render.stats", json!({}))
            .expect("synchronous method")
            .expect("stats");
        let cache = &v["mesh_cache"];
        for field in [
            "meshes",
            "mesh_bytes",
            "mesh_budget_bytes",
            "evicted",
            "adopted_meshes",
            "adopted_mesh_bytes",
        ] {
            assert!(cache[field].is_u64(), "mesh_cache.{field} missing: {v}");
        }
        assert_eq!(cache["adopted_meshes"], 0, "{v}");
        assert_eq!(cache["adopted_mesh_bytes"], 0, "{v}");
    }

    /// ADR-8610: `render.stats` through the dispatcher reports the skinned counted meshes the mesh
    /// cache adopted, the material entries rebuilt for a counted texture's new upload, and the
    /// App's counted texture table (`null` until `CountedMeshPlugin` made one).
    #[test]
    fn render_stats_reports_skinned_counted_meshes_and_the_counted_texture_table() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let call = |world: &mut World| {
            crate::dispatch::call_in_process(world, "render.stats", json!({}))
                .expect("synchronous method")
                .expect("stats")
        };
        let v = call(&mut world);
        assert_eq!(v["mesh_cache"]["adopted_skinned_meshes"], 0, "{v}");
        assert_eq!(v["counted"]["texture_rebinds"], 0, "{v}");
        assert!(v["counted"]["textures"].is_null(), "no uploaders yet: {v}");
        let Ok(device) = regia_render::RenderDevice::shared_headless() else {
            eprintln!("SKIP: no adapter (the table half needs a device)");
            return;
        };
        world.insert_resource(regia_render::CountedAssetUploads::new(&device));
        let v = call(&mut world);
        let textures = &v["counted"]["textures"];
        for field in ["resident", "resident_bytes", "sibling_views", "uploads", "frees"] {
            assert_eq!(textures[field], 0, "counted.textures.{field}: {v}");
        }
    }

    /// ADR-7428: `render.stats` through the dispatcher reports the LOD index lists uploaded for
    /// counted meshes' chain levels (none on a fresh renderer).
    #[test]
    fn render_stats_reports_counted_lod_index_uploads() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let v = crate::dispatch::call_in_process(&mut world, "render.stats", json!({}))
            .expect("synchronous method")
            .expect("stats");
        assert!(v["counted"]["lod_index_uploads"].is_u64(), "{v}");
        assert_eq!(v["counted"]["lod_index_uploads"], 0, "{v}");
    }

    /// ADR-7462: `render.stats` through the dispatcher reports the counted meshes on the visibility
    /// buffer's cluster path (none on a fresh renderer).
    #[test]
    fn render_stats_reports_counted_meshes_on_the_cluster_path() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let v = crate::dispatch::call_in_process(&mut world, "render.stats", json!({}))
            .expect("synchronous method")
            .expect("stats");
        let vis = &v["counted"]["vis"];
        for field in [
            "meshes",
            "clusters",
            "repacks",
            "forward_fallbacks",
            "kept_meshlet_bytes",
        ] {
            assert!(vis[field].is_u64(), "counted.vis.{field}: {v}");
            assert_eq!(vis[field], 0, "counted.vis.{field}: {v}");
        }
    }

    #[test]
    fn without_a_renderer_it_says_what_to_add() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
        let err = set_skinned_instancing(&mut world, &json!({ "enabled": true })).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RenderPlugin"));
    }
}
