// SPDX-License-Identifier: MPL-2.0
//! `render.viscells` and `render.set_viscells`: indoor cells and portals (spec 25 § 7.7, ADR-7586,
//! `book/agents/viscells.md`). The read returns the loaded cells and portals, the switches and
//! the last frame's walk per view (eye cell, reached cells, visits) and its cull counters. The
//! write installs a `<map>.viscells.ron` text (validated, refused with its error code), clears the
//! cells, turns culling on or off, and turns the debug draw (gizmo group `viscells`) on or off.
//! Stage 3 (ADR-8414): the read reports the sun walk and the sun-sealed draws; the write installs
//! or clears the host's [`SunCover`] (`sun_cover`, `sun_direction`).
//! Shadow views per cell (ADR-8874): the read reports the shadow-view cull (`frame.shadow_view_*`)
//! and the terrain's cover provider (`terrain_sun_cover`); the write turns either on or off
//! (`shadow_views`, `terrain_sun_cover`).

use regia_app::prelude::*;
use regia_render::gizmos::{GizmoConfigGroup, GizmoConfigStore};
use regia_render::viscells::{
    SunCover, VisCellCulled, VisCellGizmos, VisCellSettings, VisCellShadowViews, VisCellStats, VisCells,
    VisCellsError,
};
use regia_terrain::{TerrainSunCover, TerrainSunCoverStats};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The longest `text` `render.set_viscells` takes, bytes.
pub(crate) const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

fn opt_bool(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::wrong_type(field, "a boolean")),
    }
}

/// The refusal of a viscells text as a protocol error carrying its code.
pub(crate) fn refusal(error: &VisCellsError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "fix the viscells.ron text; book/agents/viscells.md lists every rule and code",
    )
}

fn debug_draw(world: &World) -> bool {
    world
        .get_resource::<GizmoConfigStore>()
        .is_some_and(|store| store.is_enabled::<VisCellGizmos>())
}

fn v3(v: regia_core::math::Vec3) -> [f32; 3] {
    v.to_array()
}

/// `render.viscells { geometry? }` (read only).
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let geometry = opt_bool(params, "geometry")?.unwrap_or(true);
    let cull = world
        .get_resource::<VisCellSettings>()
        .copied()
        .unwrap_or_default()
        .cull;
    let stats = world.get_resource::<VisCellStats>().cloned().unwrap_or_default();
    let culled_glass = world.get_resource::<VisCellCulled>().map_or(0, |c| c.glass);
    let cells = world.get_resource::<VisCells>();
    let (cell_list, portal_list) = match (cells, geometry) {
        (Some(cells), true) => (
            cells
                .cells
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "name": c.name,
                        "boxes": c.bounds.iter().map(|b| json!({
                            "center": b.center,
                            "half_extents": b.half_extents,
                            "rotation_radians": b.rotation_radians,
                        })).collect::<Vec<_>>(),
                        "portals": c.portals,
                    })
                })
                .collect::<Vec<_>>(),
            cells
                .portals
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    json!({
                        "index": i,
                        "a": cells.cells[p.a as usize].id,
                        "b": cells.cells[p.b as usize].id,
                        "quad": p.quad.map(v3),
                        "normal": v3(p.normal),
                        "doorway_depth": p.doorway_depth,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    let views: Vec<Value> = stats
        .views
        .iter()
        .map(|v| {
            json!({
                "view": v.view,
                "eye": v.eye,
                "eye_cell": v.eye_cell,
                "eye_cells": v.eye_cells,
                "visits": v.visits,
                "portals_passed": v.portals_passed,
                "capped": v.capped,
                "depth_capped": v.depth_capped,
                "reachable": v.reachable,
            })
        })
        .collect();
    let mut segments = Vec::new();
    if let Some(cells) = cells {
        regia_render::viscells::debug_segments(cells, Some(&stats), &mut segments);
    }
    Ok(json!({
        "loaded": cells.is_some(),
        "version": cells.map(|c| c.document().version),
        "cell_count": cells.map_or(0, |c| c.cells.len()),
        "portal_count": cells.map_or(0, |c| c.portals.len()),
        "cull": cull,
        "debug_draw": debug_draw(world),
        "debug_segments": segments.len(),
        "cells": cell_list,
        "portals": portal_list,
        "frame": {
            "runs": stats.runs,
            "active": stats.active,
            "views": stats.views.len(),
            "cell_draws": stats.cell_draws,
            "culled_draws": stats.culled_draws,
            "shadow_only_draws": stats.shadow_only_draws,
            "unknown_cell_draws": stats.unknown_cell_draws,
            "visits": stats.visits,
            "cell_lights": stats.cell_lights,
            "culled_lights": stats.culled_lights,
            "culled_glass": culled_glass,
            "sun_reached": stats.sun_reached,
            "sun_walk_visits": stats.sun_walk_visits,
            "sun_sealed_cells": stats.sun_sealed_cells,
            "sun_sealed_draws": stats.sun_sealed_draws,
            "sun_dropped_draws": stats.sun_dropped_draws,
            "shadow_view_active": stats.shadow_view_active,
            "shadow_view_tested": stats.shadow_view_tested,
            "shadow_view_culled_draws": stats.shadow_view_culled_draws,
            "shadow_view_dropped_draws": stats.shadow_view_dropped_draws,
        },
        "shadow_views": shadow_views(world),
        "sun_cover": sun_cover_json(world),
        "terrain_sun_cover": terrain_sun_cover_json(world),
        "reachable": stats.reachable,
        "views": views,
    }))
}

/// Whether the shadow-view cull is on ([`VisCellShadowViews`], on without the resource).
fn shadow_views(world: &World) -> bool {
    world
        .get_resource::<VisCellShadowViews>()
        .copied()
        .unwrap_or_default()
        .cull
}

/// The terrain's cover provider ([`TerrainSunCover`]) and its last fill, or `null` when off.
fn terrain_sun_cover_json(world: &World) -> Value {
    let Some(settings) = world.get_resource::<TerrainSunCover>() else {
        return Value::Null;
    };
    let stats = world
        .get_resource::<TerrainSunCoverStats>()
        .cloned()
        .unwrap_or_default();
    json!({
        "grow_m": settings.grow_m,
        "fills": stats.fills,
        "cells_tested": stats.cells_tested,
        "cells_covered": stats.cells_covered,
        "last_grow_m": stats.grow_m,
    })
}

/// The installed [`SunCover`]: its cells and the sun it was computed for, or `null`.
fn sun_cover_json(world: &World) -> Value {
    world.get_resource::<SunCover>().map_or(
        Value::Null,
        |cover| json!({ "cells": cover.cells(), "sun_direction": cover.direction() }),
    )
}

/// `sun_cover` (`null` clears; an array of cell ids installs) and `sun_direction` (three finite
/// numbers, not all 0; required with a non-empty `sun_cover`). `None`: leave the cover alone;
/// `Some(None)`: clear it.
fn sun_cover_param(params: &Value) -> Result<Option<Option<SunCover>>, RpcError> {
    // Type-checked whenever it is present, even without a `sun_cover` that uses it (ADR-2622).
    let direction = match params.get("sun_direction") {
        None | Some(Value::Null) => None,
        Some(Value::Array(v)) if v.len() == 3 => {
            let mut d = [0.0f32; 3];
            for (out, value) in d.iter_mut().zip(v) {
                *out = value
                    .as_f64()
                    .ok_or_else(|| RpcError::wrong_type("sun_direction", "three numbers"))?
                    as f32;
            }
            Some(d)
        }
        Some(_) => return Err(RpcError::wrong_type("sun_direction", "three numbers")),
    };
    let ids = match params.get("sun_cover") {
        None => return Ok(None),
        Some(Value::Null) => return Ok(Some(None)),
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|id| {
                id.as_u64()
                    .and_then(|id| u16::try_from(id).ok())
                    .ok_or_else(|| RpcError::wrong_type("sun_cover", "an array of cell ids (0 to 65535)"))
            })
            .collect::<Result<Vec<u16>, _>>()?,
        Some(_) => return Err(RpcError::wrong_type("sun_cover", "an array of cell ids or null")),
    };
    if ids.is_empty() {
        return Ok(Some(None));
    }
    let Some(direction) = direction else {
        return Err(RpcError::invalid_params(
            "sun_cover needs sun_direction: the sun's travel the cover was computed for",
        ));
    };
    if !(direction.iter().all(|v| v.is_finite()) && direction.iter().any(|v| *v != 0.0)) {
        return Err(RpcError::invalid_params(
            "sun_direction must be finite and not zero",
        ));
    }
    Ok(Some(Some(SunCover::new(ids, direction))))
}

/// `render.set_viscells { text?, clear?, cull?, debug_draw?, sun_cover?, sun_direction?, shadow_views?,
/// terrain_sun_cover? }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = match params.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= MAX_TEXT_BYTES => Some(s.as_str()),
        Some(Value::String(_)) => {
            return Err(RpcError::invalid_params(format!(
                "text is longer than {MAX_TEXT_BYTES} bytes"
            )));
        }
        Some(_) => return Err(RpcError::wrong_type("text", "a string")),
    };
    let clear = opt_bool(params, "clear")?.unwrap_or(false);
    let cull = opt_bool(params, "cull")?;
    let draw = opt_bool(params, "debug_draw")?;
    let sun_cover = sun_cover_param(params)?;
    let shadow_view_cull = opt_bool(params, "shadow_views")?;
    let terrain_cover = opt_bool(params, "terrain_sun_cover")?;
    if clear && text.is_some() {
        return Err(RpcError::invalid_params("pass text or clear, not both"));
    }
    if terrain_cover == Some(true) && matches!(sun_cover, Some(Some(_))) {
        return Err(RpcError::invalid_params(
            "pass sun_cover or terrain_sun_cover true, not both: the terrain fills the cover itself",
        ));
    }
    // Validate before changing anything: a refused text leaves the world as it was.
    let loaded = text
        .map(VisCells::from_ron)
        .transpose()
        .map_err(|e| refusal(&e))?;
    if draw.is_some() && !world.contains_resource::<GizmoConfigStore>() {
        return Err(RpcError::not_found(
            "no gizmo store: the renderer is not running",
            "debug_draw needs the RenderPlugin (its GizmoPlugin)",
        ));
    }
    if let Some(cells) = loaded {
        world.insert_resource(cells);
    }
    if clear {
        world.remove_resource::<VisCells>();
    }
    if let Some(cull) = cull {
        world.get_resource_or_init::<VisCellSettings>().cull = cull;
    }
    if let Some(on) = shadow_view_cull {
        world.insert_resource(VisCellShadowViews { cull: on });
    }
    match sun_cover {
        Some(Some(cover)) => {
            // The host's cover replaces the terrain's provider.
            world.remove_resource::<TerrainSunCover>();
            world.insert_resource(cover);
        }
        Some(None) => {
            world.remove_resource::<SunCover>();
        }
        None => {}
    }
    match terrain_cover {
        Some(true) => {
            if !world.contains_resource::<TerrainSunCover>() {
                world.insert_resource(TerrainSunCover::default());
            }
        }
        Some(false) => {
            // The cover it filled goes with it (a stale cover would still seal for this sun).
            let filled = world.remove_resource::<TerrainSunCover>().is_some();
            if filled {
                world.remove_resource::<SunCover>();
            }
        }
        None => {}
    }
    if let Some(on) = draw
        && let Some(mut store) = world.get_resource_mut::<GizmoConfigStore>()
    {
        store.set_enabled(VisCellGizmos::NAME, on);
    }
    let cells = world.get_resource::<VisCells>();
    Ok(json!({
        "loaded": cells.is_some(),
        "cell_count": cells.map_or(0, |c| c.cells.len()),
        "portal_count": cells.map_or(0, |c| c.portals.len()),
        "cull": world.get_resource::<VisCellSettings>().copied().unwrap_or_default().cull,
        "debug_draw": debug_draw(world),
        "shadow_views": shadow_views(world),
        "sun_cover": sun_cover_json(world),
        "terrain_sun_cover": terrain_sun_cover_json(world),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOMS: &str = "(cells: [
        (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
        (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
    ], portals: [(a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)])])";

    fn code(error: &RpcError) -> String {
        error.data.as_ref().expect("data")["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn set_switches_shadow_views_and_the_terrain_cover() {
        let mut world = World::new();
        let read = get(&mut world, &json!({})).expect("reads");
        assert_eq!(read["shadow_views"], json!(true), "on by default");
        assert_eq!(read["terrain_sun_cover"], Value::Null);
        assert_eq!(read["frame"]["shadow_view_culled_draws"], json!(0));
        let reply = set(&mut world, &json!({ "shadow_views": false })).expect("switches");
        assert_eq!(reply["shadow_views"], json!(false));
        assert!(!world.resource::<VisCellShadowViews>().cull);
        let reply = set(&mut world, &json!({ "terrain_sun_cover": true })).expect("installs");
        assert_eq!(reply["terrain_sun_cover"]["fills"], json!(0));
        assert_eq!(reply["terrain_sun_cover"]["grow_m"], Value::Null);
        assert!(world.contains_resource::<TerrainSunCover>());
        // Both at once is refused, and changes nothing.
        let err = set(
            &mut world,
            &json!({ "terrain_sun_cover": true, "sun_cover": [1], "sun_direction": [0.0, -1.0, 0.0] }),
        )
        .expect_err("both");
        assert_eq!(code(&err), "F0002");
        assert!(!world.contains_resource::<SunCover>());
        for bad in [
            json!({ "shadow_views": "yes" }),
            json!({ "terrain_sun_cover": 1 }),
        ] {
            assert_eq!(
                code(&set(&mut world, &bad).expect_err("wrong type")),
                "F0002",
                "{bad}"
            );
        }
        // A host cover replaces the provider; turning the provider off clears the cover it filled.
        set(
            &mut world,
            &json!({ "sun_cover": [1], "sun_direction": [0.0, -1.0, 0.0] }),
        )
        .expect("host cover");
        assert!(!world.contains_resource::<TerrainSunCover>());
        assert!(world.contains_resource::<SunCover>());
        set(&mut world, &json!({ "terrain_sun_cover": false })).expect("off");
        assert!(world.contains_resource::<SunCover>(), "the host's cover stays");
        set(&mut world, &json!({ "terrain_sun_cover": true })).expect("on");
        world.insert_resource(SunCover::new([1], [0.0, -1.0, 0.0]));
        set(&mut world, &json!({ "terrain_sun_cover": false })).expect("off");
        assert!(
            !world.contains_resource::<SunCover>(),
            "the terrain's cover goes with it"
        );
    }

    #[test]
    fn set_installs_and_clears_the_sun_cover() {
        let mut world = World::new();
        let reply = set(
            &mut world,
            &json!({ "text": ROOMS, "sun_cover": [2, 1, 2], "sun_direction": [0.0, -1.0, 0.0] }),
        )
        .expect("installs");
        assert_eq!(reply["sun_cover"]["cells"], json!([1, 2]));
        assert_eq!(reply["sun_cover"]["sun_direction"], json!([0.0, -1.0, 0.0]));
        let read = get(&mut world, &json!({ "geometry": false })).expect("reads");
        assert_eq!(read["sun_cover"]["cells"], json!([1, 2]));
        assert_eq!(read["frame"]["sun_sealed_draws"], 0);
        for bad in [
            json!({ "sun_cover": [1] }),
            json!({ "sun_cover": [1], "sun_direction": [0.0, 0.0, 0.0] }),
            json!({ "sun_cover": [70000], "sun_direction": [0.0, -1.0, 0.0] }),
            json!({ "sun_cover": "1", "sun_direction": [0.0, -1.0, 0.0] }),
            // A wrong-typed `sun_direction` is refused even without a `sun_cover` to use it.
            json!({ "sun_direction": "down" }),
            json!({ "sun_direction": [0.0, "-1", 0.0] }),
            json!({ "cull": true, "sun_direction": 1.0 }),
        ] {
            let err = set(&mut world, &bad).expect_err("refused");
            assert_eq!(code(&err), "F0002", "{bad}");
        }
        assert_eq!(
            get(&mut world, &json!({})).expect("reads")["sun_cover"]["cells"],
            json!([1, 2]),
            "a refused call changes nothing"
        );
        let reply = set(&mut world, &json!({ "sun_cover": null })).expect("clears");
        assert_eq!(reply["sun_cover"], Value::Null);
    }

    #[test]
    fn set_installs_reads_back_and_clears() {
        let mut world = World::new();
        let reply = set(&mut world, &json!({ "text": ROOMS, "cull": false })).expect("installs");
        assert_eq!(reply["loaded"], true);
        assert_eq!(reply["cell_count"], 3, "cells 0, 1 and 2");
        assert_eq!(reply["cull"], false);
        let read = get(&mut world, &json!({})).expect("reads");
        assert_eq!(read["portals"][0]["a"], 1);
        assert_eq!(read["portals"][0]["b"], 2);
        assert_eq!(read["cells"][1]["boxes"][0]["half_extents"][0], 4.0);
        assert_eq!(read["debug_segments"], 12 * 2 + 6);
        let read = get(&mut world, &json!({ "geometry": false })).expect("reads");
        assert_eq!(read["cells"].as_array().map(Vec::len), Some(0));
        let reply = set(&mut world, &json!({ "clear": true })).expect("clears");
        assert_eq!(reply["loaded"], false);
    }

    #[test]
    fn a_refused_text_keeps_the_old_cells_and_names_its_code() {
        let mut world = World::new();
        set(&mut world, &json!({ "text": ROOMS })).expect("installs");
        let bad = ROOMS.replace("(a: 1, b: 2", "(a: 1, b: 1");
        let err = set(&mut world, &json!({ "text": bad })).expect_err("refused");
        assert_eq!(code(&err), "viscells_self_portal");
        let err = set(&mut world, &json!({ "text": "(version: 2)" })).expect_err("refused");
        assert_eq!(code(&err), "viscells_version_newer");
        let err = set(&mut world, &json!({ "text": ROOMS, "clear": true })).expect_err("both");
        assert_eq!(code(&err), "F0002");
        assert_eq!(get(&mut world, &json!({})).expect("reads")["cell_count"], 3);
        let err = set(&mut world, &json!({ "debug_draw": true })).expect_err("no store");
        assert_eq!(code(&err), "F0003");
    }
}
