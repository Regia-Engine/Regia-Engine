// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the world streaming and HLOD data model (`regia_scene::stream_grid`,
//! `stream_plan`, `hlod`, `floating_origin`): `scene.stream_plan`, `scene.hlod_select` and
//! `scene.large_position`, and the cooked HLOD proxies' atlases: `world.hlod_proxy` (ADR-2483).
//!
//! All three are stateless and read-only. They build the model from the call's parameters, run
//! the same pure functions a game calls, and return the result, so an agent can check a cell
//! layout, a budget or a coordinate conversion without a running level. See
//! `book/agents/world-streaming.md`.

use regia_scene::floating_origin::{LargePosition, LargeSpace};
use regia_scene::hlod::{CellProxy, HlodConfig, HlodTree};
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use regia_scene::stream_plan::{CellInfo, StreamConfig, StreamPlanner};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

fn fields(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))
}

fn number(map: &Map<String, Value>, key: &str) -> Result<Option<f64>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(n)),
            _ => Err(RpcError::invalid_params(format!(
                "`{key}` must be a finite number"
            ))),
        },
    }
}

fn required_number(map: &Map<String, Value>, key: &str) -> Result<f64, RpcError> {
    number(map, key)?.ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

fn unsigned(map: &Map<String, Value>, key: &str) -> Result<Option<u64>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn signed(map: &Map<String, Value>, key: &str) -> Result<Option<i64>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an integer"))),
    }
}

fn small(map: &Map<String, Value>, key: &str) -> Result<Option<u32>, RpcError> {
    match unsigned(map, key)? {
        None => Ok(None),
        Some(v) => u32::try_from(v)
            .map(Some)
            .map_err(|_| RpcError::invalid_params(format!("`{key}` is too large"))),
    }
}

fn array<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], RpcError> {
    crate::params::opt_array(map, key)?
        .map(Vec::as_slice)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required and must be an array")))
}

/// A JSON array of exactly `N` finite numbers.
fn vector<const N: usize>(value: &Value, what: &str) -> Result<[f64; N], RpcError> {
    let bad = || RpcError::invalid_params(format!("{what} must be an array of {N} finite numbers"));
    let items = value.as_array().ok_or_else(bad)?;
    if items.len() != N {
        return Err(bad());
    }
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
    }
    Ok(out)
}

fn cell_coord(map: &Map<String, Value>) -> Result<CellCoord, RpcError> {
    let x = signed(map, "x")?.ok_or_else(|| RpcError::invalid_params("each cell needs an integer `x`"))?;
    let z = signed(map, "z")?.ok_or_else(|| RpcError::invalid_params("each cell needs an integer `z`"))?;
    Ok(CellCoord::new(x, z))
}

fn cell_objects(map: &Map<String, Value>) -> Result<Vec<&Map<String, Value>>, RpcError> {
    array(map, "cells")?
        .iter()
        .map(|v| {
            v.as_object()
                .ok_or_else(|| RpcError::invalid_params("each cell must be an object"))
        })
        .collect()
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn bad<E: std::fmt::Display>(error: E) -> RpcError {
    RpcError::invalid_params(error.to_string())
}

/// `scene.stream_plan`: the loads and unloads the streaming planner orders for the given cells,
/// viewers, radii and budget.
pub fn stream_plan(params: &Value) -> Result<Value, RpcError> {
    let map = fields(params)?;
    let grid = StreamGrid::new(required_number(map, "cell_size")?).map_err(bad)?;
    let load_radius = required_number(map, "load_radius")?;
    let config = StreamConfig {
        load_radius,
        unload_radius: number(map, "unload_radius")?.unwrap_or(load_radius * 1.25),
        memory_budget_bytes: unsigned(map, "memory_budget_bytes")?.unwrap_or(u64::MAX / 4),
        max_loads_per_step: small(map, "max_loads_per_step")?.unwrap_or(0),
    };
    let mut planner = StreamPlanner::new(grid, config).map_err(bad)?;
    let mut loaded = Vec::new();
    for cell in cell_objects(map)? {
        let coord = cell_coord(cell)?;
        let info = CellInfo {
            bytes: unsigned(cell, "bytes")?.unwrap_or(0),
            priority: i32::try_from(signed(cell, "priority")?.unwrap_or(0))
                .map_err(|_| RpcError::invalid_params("`priority` does not fit 32 bits"))?,
        };
        planner.register_cell(coord, info).map_err(bad)?;
        if crate::params::opt_bool(cell, "loaded")? == Some(true) {
            loaded.push(coord);
        }
    }
    for coord in &loaded {
        planner.set_loaded(*coord, true).map_err(bad)?;
    }
    let viewers: Vec<[f64; 2]> = array(map, "viewers")?
        .iter()
        .map(|v| vector::<2>(v, "each viewer"))
        .collect::<Result<_, _>>()?;
    let before = planner.loaded_bytes();
    let plan = planner.plan(&viewers).map_err(bad)?;
    Ok(json!({
        "loads": to_value(&plan.loads),
        "unloads": to_value(&plan.unloads),
        "deferred": to_value(&plan.deferred),
        "resident_bytes_before": before,
        "resident_bytes_after": plan.resident_bytes_after,
        "memory_budget_bytes": config.memory_budget_bytes,
    }))
}

/// `scene.hlod_select`: build an HLOD tree from cell proxies and choose what to draw for the
/// given viewers.
pub fn hlod_select(params: &Value) -> Result<Value, RpcError> {
    let map = fields(params)?;
    let grid = StreamGrid::new(required_number(map, "cell_size")?).map_err(bad)?;
    let defaults = HlodConfig::default();
    let config = HlodConfig {
        fov_y_rad: number(map, "fov_deg")?.map_or(defaults.fov_y_rad, f64::to_radians),
        screen_size_threshold: number(map, "screen_size_threshold")?
            .unwrap_or(defaults.screen_size_threshold),
        merge_factor: small(map, "merge_factor")?.unwrap_or(defaults.merge_factor),
        max_levels: small(map, "max_levels")?.unwrap_or(defaults.max_levels),
        proxy_byte_ratio: number(map, "proxy_byte_ratio")?.unwrap_or(defaults.proxy_byte_ratio),
    };
    let mut proxies = Vec::new();
    for cell in cell_objects(map)? {
        proxies.push(CellProxy {
            cell: cell_coord(cell)?,
            min_y: number(cell, "min_y")?.unwrap_or(0.0),
            max_y: number(cell, "max_y")?.unwrap_or(100.0),
            bytes: unsigned(cell, "bytes")?.unwrap_or(0),
        });
    }
    let viewers: Vec<[f64; 3]> = array(map, "viewers")?
        .iter()
        .map(|v| vector::<3>(v, "each viewer"))
        .collect::<Result<_, _>>()?;
    let tree = HlodTree::build(&grid, config, &proxies).map_err(bad)?;
    let draws = tree.select(&viewers).map_err(bad)?;
    let entries: Vec<Value> = draws
        .iter()
        .filter_map(|d| {
            let node = tree.node(d.node)?;
            Some(json!({
                "node": d.node,
                "kind": to_value(&d.kind),
                "level": node.level,
                "cell": node.cell.map(|c| c.label()),
                "switch_distance": node.switch_distance,
                "proxy_bytes": node.proxy_bytes,
            }))
        })
        .collect();
    Ok(json!({
        "node_count": tree.nodes().len(),
        "leaf_count": tree.leaf_count(),
        "max_level": tree.max_level(),
        "roots": tree.roots(),
        "draws": entries,
        "proxy_bytes": tree.proxy_bytes_of(&draws),
    }))
}

/// `scene.large_position`: convert a world position to a cell plus `f32` offset, and optionally
/// the `f32` vector from an origin.
pub fn large_position(params: &Value) -> Result<Value, RpcError> {
    let map = fields(params)?;
    let space = LargeSpace::new(required_number(map, "cell_size")?).map_err(bad)?;
    let position = map
        .get("position")
        .ok_or_else(|| RpcError::invalid_params("`position` is required"))
        .and_then(|v| vector::<3>(v, "`position`"))?;
    let pos: LargePosition = space.from_world(position).map_err(bad)?;
    let mut out = json!({
        "cell": pos.cell,
        "local": pos.local,
        "world": space.to_world(&pos),
    });
    if let Some(origin) = map.get("origin").filter(|v| !v.is_null()) {
        let origin = space.from_world(vector::<3>(origin, "`origin`")?).map_err(bad)?;
        out["relative_to_origin"] = json!(space.relative(&pos, &origin));
    }
    Ok(out)
}

fn hlod_error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        code,
        message,
        "name a cooked world index: `path` (the .fckd file) or `level` (the level guid, read from this \
         project's cooked cache for `platform`); cook the level first",
    )
}

/// `world.hlod_proxy`: the HLOD proxies of a cooked streamed level and their baked atlases
/// (ADR-2483): per node the proxy mesh, the atlas size, the texel density reached and asked, whether
/// the cap truncated it, the bake error and the miss ratio. `node` narrows the reply to one node.
pub fn hlod_proxy(world: &regia_ecs::prelude::World, params: &Value) -> Result<Value, RpcError> {
    let map = fields(params)?;
    let path = match (crate::params::opt_str(map, "path")?, map.get("level")) {
        (Some(path), _) => crate::render_out::confine_path(world, "path", std::path::Path::new(path))?,
        (None, Some(level)) => {
            let level = level
                .as_str()
                .and_then(|text| text.parse::<regia_core::id::Guid>().ok())
                .ok_or_else(|| RpcError::invalid_params("`level` must be a guid string"))?;
            // A directory name under the project's cooked cache, never a path out of it.
            let platform = match map.get("platform") {
                None => std::env::consts::OS.to_owned(),
                Some(value) => value
                    .as_str()
                    .filter(|tag| {
                        !tag.is_empty()
                            && tag.bytes().all(|b| {
                                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                            })
                    })
                    .ok_or_else(|| {
                        RpcError::invalid_params(
                            "`platform` must be a platform tag such as `linux` or `android`",
                        )
                    })?
                    .to_owned(),
            };
            let root = crate::req_path::project_root(world);
            regia_asset::cooked_cache_dir(&root, &platform).join(regia_asset::cooked_file_name(
                regia_world::format::world_index_id(regia_scene::level::LevelId(level)),
            ))
        }
        (None, None) => return Err(RpcError::invalid_params("`path` or `level` is required")),
    };
    let node = small(map, "node")?;
    // Capped, regular files only (ADR-5597): a FIFO at the index or sidecar name blocked the
    // world thread, and the file was read whole whatever its size.
    let bytes = crate::req_path::read_file_capped(&path, crate::req_path::MAX_READ_BYTES)
        .map_err(|e| hlod_error("hlod_index_unreadable", format!("{}: {e}", path.display())))?;
    let index = regia_world::format::WorldIndex::decode_any(&bytes)
        .map_err(|e| hlod_error("hlod_index_unreadable", format!("{}: {e}", path.display())))?;
    // The scatter's voxel proxies (ADR-3253), a sidecar beside the index; absent without scatter.
    let voxels = path
        .parent()
        .map(|dir| {
            dir.join(regia_asset::cooked_file_name(
                regia_world::hlod_voxels::hlod_voxels_id(regia_scene::level::LevelId(index.level)),
            ))
        })
        .and_then(|file| crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES).ok())
        .map(|bytes| {
            regia_world::hlod_voxels::HlodVoxels::from_bytes(&bytes, regia_scene::level::LevelId(index.level))
                .map_err(|e| hlod_error("hlod_voxels_unreadable", format!("voxel table: {e}")))
        })
        .transpose()?;
    // Whether proxies cast shadows here: the live switch and the active tier (ADR-3253).
    let shadows = world
        .get_resource::<regia_render::HlodProxyShadows>()
        .copied()
        .unwrap_or_default();
    let preset = world
        .get_resource::<regia_render::RenderSettings>()
        .and_then(|s| s.preset());
    let mut report = hlod_proxy_report_with(&index, voxels.as_ref(), node, shadows.cascades(preset))?;
    if let Some(map) = report.as_object_mut() {
        map.insert(
            "shadows".into(),
            json!({
                "enabled": shadows.enabled,
                "tier": preset.map(|p| format!("{p:?}")),
                "cascades": shadows.cascades(preset),
            }),
        );
    }
    // Whether each atlas material cuts holes (ADR-2713): read from the cooked material beside the
    // index; `null` when that file does not read.
    if let (Some(dir), Some(rows)) = (
        path.parent(),
        report.get_mut("nodes").and_then(Value::as_array_mut),
    ) {
        for row in rows {
            let Some(atlas) = row.get_mut("atlas").and_then(Value::as_object_mut) else {
                continue;
            };
            let masked = atlas
                .get("material")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<regia_core::id::Guid>().ok())
                .and_then(|id| {
                    std::fs::read(dir.join(regia_asset::cooked_file_name(regia_asset::AssetId(id)))).ok()
                })
                .and_then(|bytes| regia_asset::load_material_bytes(&bytes).ok())
                .map(|(_, m)| m.authored.render_state.alpha_mode == regia_asset::AlphaMode::Mask);
            atlas.insert("alpha_masked".into(), json!(masked));
        }
    }
    Ok(report)
}

/// The `world.hlod_proxy` reply for a decoded index (no voxel table, proxy shadows off), as the tests
/// read it.
#[cfg(test)]
fn hlod_proxy_report(index: &regia_world::format::WorldIndex, node: Option<u32>) -> Result<Value, RpcError> {
    hlod_proxy_report_with(index, None, node, 0)
}

/// The `world.hlod_proxy` reply for a decoded index, its voxel table if the cook wrote one, and the
/// sun cascades a proxy casts into here (0: proxy shadows off, ADR-3253). Each node names its proxy
/// kinds (`mesh`, `atlas`, `voxel`) and whether it casts a shadow.
pub fn hlod_proxy_report_with(
    index: &regia_world::format::WorldIndex,
    voxels: Option<&regia_world::hlod_voxels::HlodVoxels>,
    node: Option<u32>,
    shadow_cascades: u8,
) -> Result<Value, RpcError> {
    let Some(hlod) = &index.hlod else {
        return Err(hlod_error(
            "hlod_no_proxies",
            "the level cooked no HLOD proxies (`hlod: None`)",
        ));
    };
    let grid = index.settings.grid().map_err(bad)?;
    let tree = hlod.tree(&grid).map_err(bad)?;
    let row = |i: usize| -> Value {
        let tree_node = tree.node(i as u32);
        let atlas = hlod.atlas(i).map(|a| {
            json!({
                "material": a.material.0.to_string(),
                "textures": a.textures.iter().map(|t| t.0.to_string()).collect::<Vec<_>>(),
                "size": a.size,
                "texels_per_m": a.texels_per_m,
                "asked_texels_per_m": a.asked_texels_per_m,
                "capped": a.capped,
                "bake_error_m": a.bake_error_m,
                "miss_ratio": a.miss_ratio,
            })
        });
        let mesh = hlod.node_proxies.get(i).copied().flatten();
        let voxel = voxels.and_then(|t| t.node(i));
        let mut kinds = Vec::new();
        if mesh.is_some() {
            kinds.push("mesh");
        }
        if atlas.is_some() {
            kinds.push("atlas");
        }
        if voxel.is_some() {
            kinds.push("voxel");
        }
        json!({
            "node": i,
            "level": tree_node.map(|n| n.level),
            "cell": tree_node.and_then(|n| n.cell).map(|c| c.label()),
            "mesh": mesh.map(|m| m.0.to_string()),
            "color": hlod.node_colors.get(i).copied().flatten(),
            "proxy_bytes": tree_node.map(|n| n.proxy_bytes),
            "atlas": atlas,
            "kinds": kinds,
            "voxel": voxel.map(|v| json!({
                "proxy": v.proxy.0.to_string(),
                "instances": v.instances,
                "voxels": v.voxels,
                "voxel_size_m": v.voxel_size_m,
                "bytes": v.bytes,
            })),
            "casts_shadow": shadow_cascades > 0 && !kinds.is_empty(),
        })
    };
    let nodes = tree.nodes().len();
    let rows: Vec<Value> = match node {
        Some(n) if (n as usize) < nodes => vec![row(n as usize)],
        Some(n) => {
            return Err(hlod_error(
                "hlod_node_unknown",
                format!("node {n} is not in the tree ({nodes} nodes)"),
            ));
        }
        None => (0..nodes).map(row).collect(),
    };
    Ok(json!({
        "level": index.level.to_string(),
        "format_version": index.format_version,
        "hlod_atlas_px": index.settings.hlod_atlas_px,
        "hlod_atlas_max_px": index.settings.hlod_atlas_max_px,
        "atlases": hlod.node_atlases.iter().flatten().count(),
        "voxel_proxies": voxels.map_or(0, regia_world::hlod_voxels::HlodVoxels::count),
        "shadow_cascades": shadow_cascades,
        "nodes": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn row_cells(n: i64, bytes: u64) -> Value {
        Value::Array(
            (0..n)
                .map(|x| json!({ "x": x, "z": 0, "bytes": bytes }))
                .collect(),
        )
    }

    #[test]
    fn stream_plan_orders_loads_nearest_first_within_the_budget() {
        let reply = stream_plan(&json!({
            "cell_size": 100.0, "load_radius": 450.0, "unload_radius": 500.0,
            "memory_budget_bytes": 35,
            "viewers": [[50.0, 50.0]],
            "cells": row_cells(8, 10),
        }))
        .expect("plan");
        let loads: Vec<i64> = reply["loads"]
            .as_array()
            .expect("loads")
            .iter()
            .filter_map(|l| l["cell"]["x"].as_i64())
            .collect();
        assert_eq!(loads, vec![0, 1, 2]);
        assert_eq!(reply["resident_bytes_after"], 30);
        assert_eq!(
            reply["deferred"].as_array().map(Vec::len),
            Some(3),
            "cells 3, 4 and 5 are in range but do not fit"
        );
    }

    #[test]
    fn stream_plan_unloads_loaded_cells_that_left_the_unload_radius() {
        let reply = stream_plan(&json!({
            "cell_size": 100.0, "load_radius": 100.0, "unload_radius": 200.0,
            "viewers": [[1050.0, 50.0]],
            "cells": [{ "x": 0, "z": 0, "bytes": 5, "loaded": true }, { "x": 10, "z": 0, "bytes": 5 }],
        }))
        .expect("plan");
        assert_eq!(reply["unloads"][0]["cell"]["x"], 0);
        assert_eq!(reply["unloads"][0]["reason"], "Distance");
        assert_eq!(reply["loads"][0]["cell"]["x"], 10);
        assert_eq!(reply["resident_bytes_before"], 5);
    }

    #[test]
    fn stream_plan_reports_bad_parameters_as_invalid_params() {
        let base = json!({ "cell_size": 100.0, "load_radius": 100.0, "viewers": [[0.0, 0.0]], "cells": [] });
        assert!(stream_plan(&base).is_ok());
        let cases = [
            json!({ "load_radius": 1.0, "viewers": [], "cells": [] }),
            json!({ "cell_size": 0.0, "load_radius": 1.0, "viewers": [[0, 0]], "cells": [] }),
            json!({ "cell_size": 100.0, "load_radius": 100.0, "unload_radius": 50.0, "viewers": [[0, 0]], "cells": [] }),
            json!({ "cell_size": 100.0, "load_radius": 1.0, "viewers": [], "cells": [] }),
            json!({ "cell_size": 100.0, "load_radius": 1.0, "viewers": [[0, 0, 0]], "cells": [] }),
            json!({ "cell_size": 100.0, "load_radius": 1.0, "viewers": [[0, 0]], "cells": [{ "x": 0 }] }),
            json!({ "cell_size": 100.0, "load_radius": 1.0, "viewers": [[0, 0]], "cells": [{ "x": 1, "z": 1 }, { "x": 1, "z": 1 }] }),
            json!({ "cell_size": 100.0, "load_radius": 1.0, "viewers": [[0, 0]], "cells": [{ "x": 1.5, "z": 1 }] }),
            json!(["not an object"]),
        ];
        for params in cases {
            let error = stream_plan(&params).expect_err("invalid");
            assert_eq!(code(&error), "F0002", "{params}");
        }
    }

    #[test]
    fn hlod_select_draws_a_single_root_proxy_from_far_away() {
        let cells: Vec<Value> = (0..4)
            .flat_map(|x| {
                (0..4).map(move |z| json!({ "x": x, "z": z, "min_y": 0.0, "max_y": 40.0, "bytes": 1000 }))
            })
            .collect();
        let params = |viewer: [f64; 3]| {
            json!({ "cell_size": 100.0, "fov_deg": 90.0, "screen_size_threshold": 0.1,
                    "cells": cells, "viewers": [viewer] })
        };
        let far = hlod_select(&params([200.0, 0.0, 1.0e6])).expect("far");
        assert_eq!(far["draws"].as_array().map(Vec::len), Some(1));
        assert_eq!(far["draws"][0]["kind"], "Proxy");
        assert_eq!(far["leaf_count"], 16);
        assert_eq!(far["max_level"], 2);
        let near = hlod_select(&params([50.0, 10.0, 50.0])).expect("near");
        let draws = near["draws"].as_array().expect("draws");
        assert!(draws.len() > 1);
        assert!(draws.iter().any(|d| d["kind"] == "Full" && d["cell"] == "0_0"));
    }

    #[test]
    fn hlod_select_reports_bad_parameters() {
        let ok = json!({ "cell_size": 100.0, "cells": [{ "x": 0, "z": 0 }], "viewers": [[0, 0, 0]] });
        assert!(hlod_select(&ok).is_ok());
        for patch in [
            json!({ "fov_deg": 200.0 }),
            json!({ "merge_factor": 1 }),
            json!({ "screen_size_threshold": 0.0 }),
            json!({ "max_levels": 99 }),
            json!({ "viewers": [] }),
            json!({ "cells": [{ "x": 0, "z": 0 }, { "x": 0, "z": 0 }] }),
            json!({ "cells": [{ "x": 0, "z": 0, "min_y": 5.0, "max_y": 1.0 }] }),
        ] {
            let mut params = ok.clone();
            if let (Some(target), Some(extra)) = (params.as_object_mut(), patch.as_object()) {
                target.extend(extra.clone());
            }
            let error = hlod_select(&params).expect_err("invalid");
            assert_eq!(code(&error), "F0002", "{patch}");
        }
    }

    #[test]
    fn large_position_splits_and_measures_relative_to_an_origin() {
        let reply = large_position(&json!({
            "cell_size": 1024.0,
            "position": [5.0e9 + 1.002, 12.5, -3000.0],
            "origin": [5.0e9, 0.0, -3000.0],
        }))
        .expect("ok");
        let relative = reply["relative_to_origin"].as_array().expect("relative");
        assert!((relative[0].as_f64().unwrap_or(0.0) - 1.002).abs() < 1.0e-4);
        assert_eq!(relative[1].as_f64(), Some(12.5));
        let local = reply["local"].as_array().expect("local");
        assert!(
            local
                .iter()
                .all(|l| l.as_f64().is_some_and(|v| (-512.0..512.0).contains(&v)))
        );
        let no_origin =
            large_position(&json!({ "cell_size": 1024.0, "position": [0.0, 0.0, 0.0] })).expect("ok");
        assert!(no_origin.get("relative_to_origin").is_none());
        assert_eq!(no_origin["cell"], json!([0, 0, 0]));
    }

    #[test]
    fn large_position_reports_bad_parameters() {
        for params in [
            json!({ "position": [0, 0, 0] }),
            json!({ "cell_size": 1024.0 }),
            json!({ "cell_size": -1.0, "position": [0, 0, 0] }),
            json!({ "cell_size": 1024.0, "position": [0, 0] }),
            json!({ "cell_size": 1024.0, "position": [1.0e30, 0, 0] }),
            json!({ "cell_size": 1024.0, "position": [0, 0, 0], "origin": "here" }),
        ] {
            let error = large_position(&params).expect_err("invalid");
            assert_eq!(code(&error), "F0002", "{params}");
        }
    }

    fn cooked_index(atlas: bool) -> regia_world::format::WorldIndex {
        use regia_scene::hlod::CellProxy;
        use regia_scene::stream_grid::CellCoord;
        use regia_world::format::{HlodAtlas, HlodIndex, WORLD_INDEX_FORMAT_VERSION, WorldIndex};
        let settings = regia_scene::streaming::LevelStreaming {
            enabled: true,
            hlod_atlas_px: u32::from(atlas) * 8,
            ..Default::default()
        };
        let proxies: Vec<CellProxy> = (0..2)
            .map(|x| CellProxy {
                cell: CellCoord::new(x, 0),
                min_y: 0.0,
                max_y: 1.0,
                bytes: 10,
            })
            .collect();
        let mut hlod = HlodIndex {
            config: regia_scene::hlod::HlodConfig::default(),
            proxies,
            node_proxies: vec![],
            node_anchors: vec![],
            node_colors: vec![],
            node_atlases: vec![],
        };
        let nodes = hlod
            .tree(&settings.grid().expect("grid"))
            .expect("tree")
            .nodes()
            .len();
        let id = |n: u128| regia_asset::AssetId(regia_core::id::Guid::from_u128(n));
        hlod.node_proxies = (0..nodes).map(|n| Some(id(100 + n as u128))).collect();
        hlod.node_anchors = vec![[0; 3]; nodes];
        hlod.node_colors = vec![Some([0.5, 0.5, 0.5, 1.0]); nodes];
        if atlas {
            hlod.node_atlases = vec![Some(HlodAtlas {
                material: id(7),
                textures: [id(1), id(2), id(3)],
                size: [128, 64],
                texels_per_m: 6.0,
                asked_texels_per_m: 8.0,
                capped: true,
                bake_error_m: 0.02,
                miss_ratio: 0.01,
            })];
        }
        WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: regia_core::id::Guid::from_u128(55),
            content_hash: [0; 32],
            settings,
            cells: vec![],
            bounds: [CellCoord::new(0, 0), CellCoord::new(1, 0)],
            hlod: Some(hlod),
            replicated: vec![],
            nav: None,
        }
    }

    #[test]
    fn hlod_proxy_names_each_nodes_proxy_kinds_and_shadow_casting() {
        use regia_world::hlod_voxels::{HLOD_VOXELS_FORMAT_VERSION, HlodVoxelNode, HlodVoxels};
        let index = cooked_index(true);
        let nodes = index.hlod.as_ref().map_or(0, |h| h.node_proxies.len());
        let mut table = HlodVoxels {
            format_version: HLOD_VOXELS_FORMAT_VERSION,
            level: index.level,
            resolution: 32,
            nodes: vec![None; nodes],
        };
        table.nodes[1] = Some(HlodVoxelNode {
            proxy: regia_asset::AssetId(regia_core::id::Guid::from_u128(900)),
            anchor: [0; 3],
            instances: 120,
            voxels: 2048,
            voxel_size_m: 4.0,
            bytes: 30_000,
        });
        // ADR-3253: mesh, atlas and voxel kinds per node; shadows follow the tier's cascades.
        let off = hlod_proxy_report_with(&index, Some(&table), None, 0).expect("report");
        assert_eq!(off["voxel_proxies"], 1);
        assert_eq!(off["shadow_cascades"], 0);
        assert_eq!(off["nodes"][0]["kinds"], json!(["mesh", "atlas"]));
        assert_eq!(off["nodes"][1]["kinds"], json!(["mesh", "voxel"]));
        assert_eq!(off["nodes"][1]["voxel"]["voxels"], 2048);
        assert_eq!(off["nodes"][1]["voxel"]["instances"], 120);
        assert_eq!(off["nodes"][0]["voxel"], Value::Null);
        assert_eq!(off["nodes"][1]["casts_shadow"], false);
        let on = hlod_proxy_report_with(&index, Some(&table), Some(1), 2).expect("report");
        assert_eq!(on["nodes"][0]["casts_shadow"], true);
        assert_eq!(on["shadow_cascades"], 2);
        // Without a table no node is a voxel proxy (the old reply plus empty kinds lists).
        let plain = hlod_proxy_report(&index, None).expect("report");
        assert_eq!(plain["voxel_proxies"], 0);
        assert!(
            plain["nodes"]
                .as_array()
                .is_some_and(|rows| rows.iter().all(|r| r["voxel"].is_null()))
        );
    }

    #[test]
    fn hlod_proxy_names_each_atlas_its_size_density_and_bake_error() {
        let reply = hlod_proxy_report(&cooked_index(true), None).expect("report");
        assert_eq!(reply["hlod_atlas_px"], 8);
        assert_eq!(reply["atlases"], 1);
        let nodes = reply["nodes"].as_array().expect("nodes");
        assert!(nodes.len() >= 2);
        let atlas = &nodes[0]["atlas"];
        assert_eq!(atlas["size"], json!([128, 64]));
        assert_eq!(atlas["texels_per_m"], 6.0);
        assert_eq!(atlas["asked_texels_per_m"], 8.0);
        assert_eq!(atlas["capped"], true);
        assert!((atlas["bake_error_m"].as_f64().expect("error") - 0.02).abs() < 1e-6);
        assert!(
            nodes[1]["atlas"].is_null(),
            "a node past the atlas list draws flat"
        );
        let one = hlod_proxy_report(&cooked_index(true), Some(1)).expect("one");
        assert_eq!(one["nodes"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["nodes"][0]["node"], 1);
        // A flat cook says so.
        let flat = hlod_proxy_report(&cooked_index(false), None).expect("flat");
        assert_eq!(flat["atlases"], 0);
        assert_eq!(flat["hlod_atlas_px"], 0);
    }

    #[test]
    fn hlod_proxy_reports_bad_requests_with_codes() {
        let err = hlod_proxy_report(&cooked_index(true), Some(999)).expect_err("unknown node");
        assert_eq!(code(&err), "hlod_node_unknown");
        let mut none = cooked_index(true);
        none.hlod = None;
        assert_eq!(
            code(&hlod_proxy_report(&none, None).expect_err("no hlod")),
            "hlod_no_proxies"
        );
        let mut world = regia_ecs::prelude::World::new();
        let dir = regia_test_utils::unique_temp_dir("agent_hlod_proxy");
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        let file = dir.join("index.fckd");
        std::fs::write(&file, b"not a cooked file").expect("write");
        let err = hlod_proxy(&world, &json!({ "path": file.to_string_lossy() })).expect_err("garbage");
        assert_eq!(code(&err), "hlod_index_unreadable");
        let bytes = cooked_index(true).to_bytes().expect("encode");
        std::fs::write(&file, bytes).expect("write");
        let reply = hlod_proxy(&world, &json!({ "path": file.to_string_lossy(), "node": 0 })).expect("reads");
        assert_eq!(reply["nodes"][0]["atlas"]["size"], json!([128, 64]));
        let err = hlod_proxy(&world, &json!({})).expect_err("no target");
        assert_eq!(code(&err), "F0002");
        // `platform` names a directory of the cooked cache: it cannot walk out of the project.
        let guid = "00000000-0000-0000-0000-000000000abc";
        for platform in [json!("../../../x"), json!("a/b"), json!(""), json!(3)] {
            let err = hlod_proxy(&world, &json!({ "level": guid, "platform": platform }))
                .expect_err("bad platform");
            assert_eq!(code(&err), "F0002", "{platform}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-agent (ADR-5597): the index and its voxel sidecar were read with `std::fs::read`, so a
    /// FIFO at either name blocked the world thread (this test hung before the fix). A pipe index
    /// is unreadable; a pipe sidecar reads as no sidecar.
    #[cfg(unix)]
    #[test]
    fn hlod_proxy_refuses_a_pipe_index_and_skips_a_pipe_sidecar() {
        let mkfifo = |path: &std::path::Path| {
            let made = std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .map(|s| s.success());
            assert_eq!(made.ok(), Some(true), "mkfifo {}", path.display());
        };
        let mut world = regia_ecs::prelude::World::new();
        let dir = regia_test_utils::unique_temp_dir("agent_hlod_proxy_fifo");
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        let pipe = dir.join("pipe.fckd");
        mkfifo(&pipe);
        let err = hlod_proxy(&world, &json!({ "path": pipe.to_string_lossy() })).expect_err("a pipe index");
        assert_eq!(code(&err), "hlod_index_unreadable");
        let index = cooked_index(true);
        let file = dir.join("index.fckd");
        std::fs::write(&file, index.to_bytes().expect("encode")).expect("write");
        mkfifo(&dir.join(regia_asset::cooked_file_name(
            regia_world::hlod_voxels::hlod_voxels_id(regia_scene::level::LevelId(index.level)),
        )));
        let reply = hlod_proxy(&world, &json!({ "path": file.to_string_lossy() })).expect("reads");
        assert!(reply["nodes"].is_array(), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2713: each row names the node's residency bytes, and an atlas says whether its cooked
    /// material is alpha-masked (`null` when the material file is not beside the index).
    #[test]
    fn hlod_proxy_says_which_atlases_cut_holes_and_what_each_node_weighs() {
        let mut world = regia_ecs::prelude::World::new();
        let dir = regia_test_utils::unique_temp_dir("agent_hlod_proxy_mask");
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        let file = dir.join("index.fckd");
        std::fs::write(&file, cooked_index(true).to_bytes().expect("encode")).expect("write");
        let reply = hlod_proxy(&world, &json!({ "path": file.to_string_lossy(), "node": 0 })).expect("reads");
        assert!(
            reply["nodes"][0]["atlas"]["alpha_masked"].is_null(),
            "no material file"
        );
        assert_eq!(
            reply["nodes"][0]["proxy_bytes"], 10,
            "a leaf weighs its recorded bytes"
        );

        let material_id = regia_asset::AssetId(regia_core::id::Guid::from_u128(7));
        let mut authored = regia_asset::MaterialAssetRon::default_standard();
        authored.render_state.alpha_mode = regia_asset::AlphaMode::Mask;
        let payload = regia_asset::CookedStandardMaterial::new(authored)
            .to_postcard()
            .expect("payload");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::standard_material().0,
            asset_id: material_id,
            importer_version: regia_asset::pack_importer_version(1, 0, 0),
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        std::fs::write(
            dir.join(regia_asset::cooked_file_name(material_id)),
            regia_asset::encode_cooked_file(&header, &payload).expect("file"),
        )
        .expect("write");
        let reply = hlod_proxy(&world, &json!({ "path": file.to_string_lossy() })).expect("reads");
        assert_eq!(reply["nodes"][0]["atlas"]["alpha_masked"], true);
        assert!(reply["nodes"][1]["atlas"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every key of a full reply (an atlas with `alpha_masked`, a leaf with `proxy_bytes`) is named
    /// by the method's schema `result` string, so `agent.schema` and the snapshot describe what
    /// the call returns.
    #[test]
    fn hlod_proxy_schema_result_names_every_reply_key() {
        fn keys(value: &Value, out: &mut std::collections::BTreeSet<String>) {
            match value {
                Value::Object(map) => {
                    for (key, inner) in map {
                        out.insert(key.clone());
                        keys(inner, out);
                    }
                }
                Value::Array(items) => items.iter().for_each(|inner| keys(inner, out)),
                _ => {}
            }
        }
        let mut world = regia_ecs::prelude::World::new();
        let dir = regia_test_utils::unique_temp_dir("agent_hlod_proxy_schema");
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        let file = dir.join("index.fckd");
        std::fs::write(&file, cooked_index(true).to_bytes().expect("encode")).expect("write");
        let reply = hlod_proxy(&world, &json!({ "path": file.to_string_lossy() })).expect("reads");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            reply["nodes"][0]["atlas"].is_object(),
            "the fixture has an atlas node"
        );
        let result = crate::schema::METHODS
            .iter()
            .find(|m| m.name == "world.hlod_proxy")
            .expect("schema row")
            .result;
        let mut found = std::collections::BTreeSet::new();
        keys(&reply, &mut found);
        let missing: Vec<&String> = found
            .iter()
            .filter(|key| !crate::mentions_token(result, key))
            .collect();
        assert!(
            missing.is_empty(),
            "reply keys missing from the schema result: {missing:?}"
        );
    }
}
