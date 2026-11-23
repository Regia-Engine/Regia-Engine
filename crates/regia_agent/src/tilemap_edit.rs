// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol writes for tilemaps and auto-tiling (`docs/specs/86-2d-toolkit.md` § 5,
//! ADR-1889, `book/agents/2d.md`): `tilemap.set_tile` (one raw tile), `tilemap.autotile_register`
//! / `tilemap.autotile_list` (rule sets in [`regia_2d::AutoTiles`]), `tilemap.paint` (paint or
//! erase a terrain over cells; neighbours re-resolve) and `tilemap.autotile` (re-resolve a layer).
//!
//! Edits change the loaded map in [`SpriteAssets`] (copy-on-write; the sprite pass draws them the
//! next frame). They are runtime edits: a hot reload of the map's source replaces them. Errors carry
//! `data.code` = one of the `tile_*` codes below (also in `errors.explain`).

use std::sync::Arc;

use regia_2d::autotile::{AutoTileError, AutoTileSet, AutoTiles, Border, Layout, TerrainBrush, TileChange};
use regia_2d::stamp::{Stamp, StampOp};
use regia_2d::tilemap::{FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, GID_MASK, MapLayer, TileMapAsset};
use regia_2d::{SpriteAssets, TileMap};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most cells one `tilemap.paint` call may paint.
pub const MAX_PAINT_CELLS: u64 = 65_536;
/// Most rotate/flip ops one `tilemap.stamp_paste` call may list.
const MAX_STAMP_OPS: usize = 16;
/// Most changed cells listed in a `tilemap.paint` answer (`count` has the total).
const MAX_LISTED_CHANGES: usize = 1_000;
/// Largest inline `.autotile.ron` text `tilemap.autotile_register` accepts (bytes).
pub const MAX_RULES_BYTES: usize = 1 << 20;

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn from_autotile(error: &AutoTileError) -> RpcError {
    let hint = match error {
        AutoTileError::Bind(_) => {
            "the rule set's `tileset` must name a tileset of this map holding every tile id, and the layer must hold width × height cells"
        }
        AutoTileError::NotTileLayer(_) => "pick a tile layer (tilemap.list shows each layer's kind)",
        AutoTileError::OutsideMap { .. } => {
            "cells are 0..width × 0..height, row 0 = top (tilemap.list `size`)"
        }
        AutoTileError::UnknownTerrain(_) => "tilemap.autotile_list lists each rule set's terrains",
        _ => "see docs/formats/autotile-ron.md",
    };
    bad(error.code(), error.to_string(), hint)
}

/// The tilemap entity (`entity`, else the first one) and the asset path it shows.
fn target_map(world: &mut World, params: &Value) -> Result<String, RpcError> {
    let entity = match params.get("entity") {
        Some(v) => {
            let bits = v.as_u64().ok_or_else(|| {
                bad(
                    "tile_bad_params",
                    "`entity` must be an entity id",
                    "take ids from tilemap.list",
                )
            })?;
            Entity::try_from_bits(bits).ok_or_else(|| {
                bad(
                    "tile_bad_params",
                    format!("{bits} is not an entity id"),
                    "take ids from tilemap.list",
                )
            })?
        }
        None => {
            let mut query = world.query_filtered::<Entity, With<TileMap>>();
            let mut all: Vec<Entity> = query.iter(world).collect();
            all.sort();
            *all.first().ok_or_else(|| {
                RpcError::not_found("no TileMap in this world", "spawn a regia_2d::TileMap entity")
            })?
        }
    };
    let component = world.get::<TileMap>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no TileMap", entity.to_bits()),
            "list maps with tilemap.list",
        )
    })?;
    let path = component.map.clone();
    let loaded = world
        .get_resource::<SpriteAssets>()
        .is_some_and(|a| a.tilemap(&path).is_some());
    if !loaded {
        return Err(RpcError::not_found(
            format!("tilemap `{path}` is not loaded"),
            "check the path is cooked (assets.*) and Sprite2dPlugin is added",
        ));
    }
    Ok(path)
}

/// Runs `read` on the loaded map at `path`.
fn with_map<R>(world: &World, path: &str, read: impl FnOnce(&TileMapAsset) -> R) -> Result<R, RpcError> {
    world
        .get_resource::<SpriteAssets>()
        .and_then(|a| a.tilemap(path))
        .map(|m| read(&m.map))
        .ok_or_else(|| {
            RpcError::not_found(
                format!("tilemap `{path}` is not loaded"),
                "list maps with tilemap.list",
            )
        })
}

/// The tile layer `layer` (name or index, required) of `map`.
fn tile_layer(map: &TileMapAsset, params: &Value) -> Result<usize, RpcError> {
    let index = match params.get("layer") {
        Some(Value::String(name)) => map.layer_index(name).ok_or_else(|| {
            bad(
                "tile_not_tile_layer",
                format!("no layer named `{name}`"),
                "tilemap.list lists the layers",
            )
        })?,
        Some(v) => v
            .as_u64()
            .map_or(usize::MAX, |i| usize::try_from(i).unwrap_or(usize::MAX)),
        None => {
            return Err(bad(
                "tile_bad_params",
                "`layer` (a tile layer name or index) is required",
                "tilemap.list lists the layers",
            ));
        }
    };
    match map.layers.get(index) {
        Some(MapLayer::Tiles(_)) => Ok(index),
        _ => Err(bad(
            "tile_not_tile_layer",
            format!("layer {} is not a tile layer", params["layer"]),
            "pick a tile layer (tilemap.list shows each layer's kind)",
        )),
    }
}

fn cell_coord(value: &Value) -> Option<u32> {
    value.as_u64().map(|v| u32::try_from(v).unwrap_or(u32::MAX))
}

fn change_json(c: &TileChange) -> Value {
    json!({ "cell": [c.x, c.y], "before": c.before, "after": c.after })
}

fn rules_set(world: &World, params: &Value) -> Result<Arc<AutoTileSet>, RpcError> {
    let name = params.get("rules").and_then(Value::as_str).ok_or_else(|| {
        bad(
            "tile_bad_params",
            "`rules` (a rule set name) is required",
            "tilemap.autotile_list",
        )
    })?;
    world
        .get_resource::<AutoTiles>()
        .and_then(|a| a.get(name).cloned())
        .ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "tile_unknown_rules",
                format!("no rule set named `{name}`"),
                "register one with tilemap.autotile_register (or AutoTiles::insert in code)",
            )
        })
}

/// `tilemap.set_tile`: writes one tile (`gid`, 0 = erase, plus optional `flip`).
pub fn set_tile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = target_map(world, params)?;
    let (Some(x), Some(y)) = (
        params.get("x").and_then(cell_coord),
        params.get("y").and_then(cell_coord),
    ) else {
        return Err(bad(
            "tile_bad_params",
            "`x` and `y` (a cell, row 0 = top) are required",
            "see book/agents/2d.md",
        ));
    };
    let gid = crate::params::opt_u64(params, "gid")?
        .and_then(|g| u32::try_from(g).ok())
        .filter(|g| g & !GID_MASK == 0)
        .ok_or_else(|| {
            bad(
                "tile_bad_gid",
                "`gid` must be a tile id without flag bits (0 = erase); flips go in `flip`",
                "tilemap.get_tile shows a cell's gid",
            )
        })?;
    let flip = params.get("flip");
    let flag = |key: &str, bit: u32| {
        if flip
            .and_then(|f| f.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bit
        } else {
            0
        }
    };
    // Spec 86: gid 0 erases. Flip bits on an empty gid would store a non-zero word that
    // `tile_at` still reads as empty while `tilemap.list` counts the cell as occupied.
    let raw = if gid == 0 {
        0
    } else {
        gid | flag("x", FLIP_X_BIT) | flag("y", FLIP_Y_BIT) | flag("diagonal", FLIP_DIAGONAL_BIT)
    };
    let (layer, before) = with_map(world, &path, |map| {
        let layer = tile_layer(map, params)?;
        if x >= map.width || y >= map.height {
            return Err(bad(
                "tile_outside_map",
                format!("cell ({x}, {y}) is outside the {}x{} map", map.width, map.height),
                "row 0 is the top row",
            ));
        }
        if gid != 0 && map.tileset_for(gid).is_none() {
            return Err(bad(
                "tile_bad_gid",
                format!("no tileset holds gid {gid}"),
                "tilemap.list lists the tilesets' first_gid and tile_count",
            ));
        }
        let before = map
            .tile_layer(layer)
            .and_then(|t| {
                t.data
                    .get((y as usize) * (map.width as usize) + x as usize)
                    .copied()
            })
            .unwrap_or(0);
        Ok((layer, before))
    })??;
    let mut assets = world
        .get_resource_mut::<SpriteAssets>()
        .ok_or_else(|| RpcError::internal("SpriteAssets vanished"))?;
    let map = assets
        .tilemap_mut(&path)
        .ok_or_else(|| RpcError::internal("the tilemap vanished"))?;
    if !map.set_tile(layer, x, y, raw) {
        return Err(RpcError::internal("set_tile refused a checked write"));
    }
    Ok(json!({ "cell": [x, y], "layer": layer, "before": before, "after": raw }))
}

/// `tilemap.autotile_register`: registers a rule set as `name` from inline `.autotile.ron` text
/// (`ron`), or from a cooked `.autotile.ron` of the asset manifest (`path`, the pak or dev cache).
pub fn autotile_register(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = crate::params::opt_str(params, "name")?
        .filter(|n| !n.is_empty())
        .ok_or_else(|| bad("tile_bad_params", "`name` is required", "any non-empty name"))?;
    if let Some(path) = crate::params::opt_str(params, "path")? {
        if params.get("ron").is_some() {
            return Err(bad(
                "tile_bad_params",
                "give `ron` or `path`, not both",
                "see book/agents/2d.md",
            ));
        }
        let Some(mut tiles) = world.remove_resource::<AutoTiles>() else {
            return Err(RpcError::not_found(
                "no AutoTiles resource in this app",
                "add regia_2d::Sprite2dPlugin",
            ));
        };
        let replaced = tiles.get(name).is_some();
        let result = match world.get_resource_mut::<regia_asset::AssetServer>() {
            Some(mut server) => tiles.load_from_server(name, path, &mut server),
            None => Err(AutoTileError::Parse("no AssetServer in this app".into())),
        };
        let terrains = tiles.get(name).map_or(0, |s| s.terrains.len());
        world.insert_resource(tiles);
        result.map_err(|e| from_autotile(&e))?;
        return Ok(json!({ "name": name, "terrains": terrains, "replaced": replaced, "path": path }));
    }
    let text = params.get("ron").and_then(Value::as_str).ok_or_else(|| {
        bad(
            "tile_bad_params",
            "`ron` (the .autotile.ron text) or `path` (a cooked .autotile.ron) is required",
            "docs/formats/autotile-ron.md",
        )
    })?;
    let set = AutoTileSet::from_ron(text).map_err(|e| from_autotile(&e))?;
    let terrains = set.terrains.len();
    let mut tiles = world.get_resource_mut::<AutoTiles>().ok_or_else(|| {
        RpcError::not_found(
            "no AutoTiles resource in this app",
            "add regia_2d::Sprite2dPlugin",
        )
    })?;
    let replaced = tiles.get(name).is_some();
    tiles.insert(name, set).map_err(|e| from_autotile(&e))?;
    Ok(json!({ "name": name, "terrains": terrains, "replaced": replaced }))
}

/// `tilemap.autotile_list`: every registered rule set with its terrains.
pub fn autotile_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(tiles) = world.get_resource::<AutoTiles>() else {
        return Ok(json!({ "rule_sets": [] }));
    };
    let sets: Vec<Value> = tiles
        .sets
        .iter()
        .map(|(name, set)| {
            json!({
                "name": name,
                "tileset": set.tileset,
                "seed": set.seed,
                "border": match set.border { Border::Same => "same", Border::Empty => "empty" },
                "terrains": set.terrains.iter().map(|t| json!({
                    "name": t.name,
                    "layout": t.layout.name(),
                    "rules": match &t.layout { Layout::Rules(r) => r.len(), _ => 0 },
                    "members": t.member_tiles(),
                    "variants": t.variants.len(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "rule_sets": sets }))
}

/// The painted cells: `cells: [[x, y], …]` or an inclusive `rect: [x0, y0, x1, y1]`.
fn paint_cells(params: &Value) -> Result<Vec<(u32, u32)>, RpcError> {
    let shape_hint = "give `cells: [[x, y], …]` or `rect: [x0, y0, x1, y1]` (inclusive, row 0 = top)";
    match (params.get("cells"), params.get("rect")) {
        (Some(Value::Array(cells)), None) => {
            if cells.len() as u64 > MAX_PAINT_CELLS {
                return Err(bad(
                    "tile_too_many_cells",
                    format!("{} cells exceed {MAX_PAINT_CELLS}", cells.len()),
                    "split the stroke",
                ));
            }
            cells
                .iter()
                .map(|c| match c.as_array().map(Vec::as_slice) {
                    Some([x, y]) => cell_coord(x).zip(cell_coord(y)),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| bad("tile_bad_params", "every cell is [x, y]", shape_hint))
        }
        (None, Some(Value::Array(rect))) => {
            let coords: Option<Vec<u32>> = rect.iter().map(cell_coord).collect();
            let Some([x0, y0, x1, y1]) = coords.as_deref().and_then(|c| <[u32; 4]>::try_from(c).ok()) else {
                return Err(bad("tile_bad_params", "`rect` is [x0, y0, x1, y1]", shape_hint));
            };
            let (x0, x1) = (x0.min(x1), x0.max(x1));
            let (y0, y1) = (y0.min(y1), y0.max(y1));
            // u128: the rect of every cell is 2^64.
            let count = (u128::from(x1 - x0) + 1) * (u128::from(y1 - y0) + 1);
            if count > u128::from(MAX_PAINT_CELLS) {
                return Err(bad(
                    "tile_too_many_cells",
                    format!("the rect holds {count} cells, more than {MAX_PAINT_CELLS}"),
                    "split the stroke",
                ));
            }
            Ok((y0..=y1).flat_map(|y| (x0..=x1).map(move |x| (x, y))).collect())
        }
        _ => Err(bad(
            "tile_bad_params",
            "exactly one of `cells` or `rect` is required",
            shape_hint,
        )),
    }
}

/// `tilemap.paint`: paints a terrain (or erases) over cells; the cells and their neighbours
/// re-resolve through the rule set.
pub fn paint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = target_map(world, params)?;
    let set = rules_set(world, params)?;
    let brush = match (params.get("terrain"), params.get("erase")) {
        (Some(Value::String(name)), None | Some(Value::Bool(false))) => {
            TerrainBrush::Terrain(set.terrain_index(name).ok_or_else(|| {
                bad(
                    "tile_unknown_terrain",
                    format!("rule set has no terrain `{name}`"),
                    "tilemap.autotile_list lists each rule set's terrains",
                )
            })?)
        }
        (None, Some(Value::Bool(true))) => TerrainBrush::Erase,
        _ => {
            return Err(bad(
                "tile_bad_params",
                "give exactly one of `terrain` (a name) or `erase: true`",
                "see book/agents/2d.md",
            ));
        }
    };
    let cells = paint_cells(params)?;
    // Every check before the copy-on-write: a refused call leaves the map and its revision alone.
    let layer = with_map(world, &path, |map| {
        let layer = tile_layer(map, params)?;
        set.bind(map).map_err(|e| from_autotile(&e))?;
        if let Some(&(x, y)) = cells.iter().find(|(x, y)| *x >= map.width || *y >= map.height) {
            return Err(from_autotile(&AutoTileError::OutsideMap { x, y }));
        }
        Ok(layer)
    })??;
    let mut assets = world
        .get_resource_mut::<SpriteAssets>()
        .ok_or_else(|| RpcError::internal("SpriteAssets vanished"))?;
    let map = assets
        .tilemap_mut(&path)
        .ok_or_else(|| RpcError::internal("the tilemap vanished"))?;
    let bound = set.bind(map).map_err(|e| from_autotile(&e))?;
    let changes = bound
        .paint(map, layer, brush, &cells)
        .map_err(|e| from_autotile(&e))?;
    Ok(json!({
        "count": changes.len(),
        "truncated": changes.len() > MAX_LISTED_CHANGES,
        "changed": changes.iter().take(MAX_LISTED_CHANGES).map(change_json).collect::<Vec<_>>(),
    }))
}

/// `tilemap.autotile`: re-resolves every terrain cell of a layer.
pub fn autotile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = target_map(world, params)?;
    let set = rules_set(world, params)?;
    let layer = with_map(world, &path, |map| {
        let layer = tile_layer(map, params)?;
        set.bind(map).map_err(|e| from_autotile(&e))?;
        Ok(layer)
    })??;
    let mut assets = world
        .get_resource_mut::<SpriteAssets>()
        .ok_or_else(|| RpcError::internal("SpriteAssets vanished"))?;
    let map = assets
        .tilemap_mut(&path)
        .ok_or_else(|| RpcError::internal("the tilemap vanished"))?;
    let bound = set.bind(map).map_err(|e| from_autotile(&e))?;
    let changes = bound.apply_layer(map, layer).map_err(|e| from_autotile(&e))?;
    Ok(json!({ "changed": changes.len() }))
}

fn from_stamp(error: &regia_2d::stamp::StampError) -> RpcError {
    bad(
        error.code(),
        error.to_string(),
        "tilemap.stamp_copy returns a stamp; layers are listed by tilemap.list",
    )
}

/// A stamp parameter `{ width, height, cells }`.
fn stamp_param(value: &Value) -> Result<Stamp, RpcError> {
    let stamp: Stamp = serde_json::from_value(value.clone()).map_err(|e| {
        bad(
            "tile_bad_stamp",
            format!("`stamp` is {{ width, height, cells }}: {e}"),
            "tilemap.stamp_copy returns one",
        )
    })?;
    stamp.validate().map_err(|e| from_stamp(&e))?;
    if stamp.cells.len() as u64 > MAX_PAINT_CELLS {
        return Err(bad(
            "tile_too_many_cells",
            format!("a stamp of {} cells exceeds {MAX_PAINT_CELLS}", stamp.cells.len()),
            "paste several smaller stamps",
        ));
    }
    Ok(stamp)
}

fn stamp_json(stamp: &Stamp) -> Value {
    json!({ "width": stamp.width, "height": stamp.height, "cells": stamp.cells })
}

/// `tilemap.stamp_copy`: the cells `rect` `[x0, y0, x1, y1]` (inclusive, row 0 = top) of a tile
/// layer as a stamp.
pub fn stamp_copy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = target_map(world, params)?;
    let bad_rect = || {
        bad(
            "tile_bad_params",
            "`rect` is [x0, y0, x1, y1] (cells, inclusive, row 0 = top)",
            "see book/agents/2d.md",
        )
    };
    let rect: Vec<u32> = params
        .get("rect")
        .and_then(Value::as_array)
        .ok_or_else(bad_rect)?
        .iter()
        .map(cell_coord)
        .collect::<Option<Vec<u32>>>()
        .filter(|v| v.len() == 4)
        .ok_or_else(bad_rect)?;
    let stamp = with_map(world, &path, |map| {
        let layer = tile_layer(map, params)?;
        Stamp::copy(map, layer, (rect[0], rect[1]), (rect[2], rect[3])).map_err(|e| from_stamp(&e))
    })??;
    Ok(stamp_json(&stamp))
}

/// `tilemap.stamp_paste`: pastes a stamp with its top-left cell at `x`, `y` (signed; the part
/// off the map is clipped). `op` / `ops` rotate or mirror it first.
/// The transformed stamp's last cell must fit signed 64-bit coordinates.
pub fn stamp_paste(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = target_map(world, params)?;
    let mut stamp = stamp_param(params.get("stamp").unwrap_or(&Value::Null))?;
    let ops: Vec<&str> = match (params.get("op"), params.get("ops")) {
        (Some(Value::String(op)), None) => vec![op.as_str()],
        (None, Some(Value::Array(list))) => {
            if list.len() > MAX_STAMP_OPS {
                return Err(bad(
                    "tile_bad_params",
                    format!("{} ops exceed {MAX_STAMP_OPS}", list.len()),
                    "four quarter turns are the identity",
                ));
            }
            list.iter()
                .map(|v| {
                    v.as_str().ok_or_else(|| {
                        bad(
                            "tile_bad_params",
                            "`ops` is a list of op names",
                            "rotate_cw, rotate_ccw, rotate_180, flip_x, flip_y",
                        )
                    })
                })
                .collect::<Result<Vec<&str>, RpcError>>()?
        }
        (None, None) => Vec::new(),
        _ => {
            return Err(bad(
                "tile_bad_params",
                "give `op` (one name) or `ops` (a list of names), not both",
                "rotate_cw, rotate_ccw, rotate_180, flip_x, flip_y",
            ));
        }
    };
    for name in &ops {
        let op = StampOp::parse(name).ok_or_else(|| {
            bad(
                "tile_bad_params",
                format!("unknown stamp op `{name}`"),
                "rotate_cw, rotate_ccw, rotate_180, flip_x, flip_y",
            )
        })?;
        stamp = stamp.transformed(op);
    }
    let at = |key: &str| crate::params::opt_i64(params, key);
    let (Some(x), Some(y)) = (at("x")?, at("y")?) else {
        return Err(bad(
            "tile_bad_params",
            "`x` and `y` (the stamp's top-left cell) are required",
            "see book/agents/2d.md",
        ));
    };
    if x.checked_add(i64::from(stamp.width - 1)).is_none()
        || y.checked_add(i64::from(stamp.height - 1)).is_none()
    {
        return Err(bad(
            "tile_bad_params",
            "`x` and `y` plus the transformed stamp extent must fit signed 64-bit coordinates",
            "choose a representable stamp origin; off-map cells are clipped",
        ));
    }
    let skip_empty = crate::params::opt_bool(params, "skip_empty")?.unwrap_or(true);
    // Every check before the copy-on-write: a refused call leaves the map and its revision alone.
    let layer = with_map(world, &path, |map| {
        let layer = tile_layer(map, params)?;
        stamp.check(map, layer).map_err(|e| from_stamp(&e))?;
        Ok(layer)
    })??;
    let mut assets = world
        .get_resource_mut::<SpriteAssets>()
        .ok_or_else(|| RpcError::internal("SpriteAssets vanished"))?;
    let map = assets
        .tilemap_mut(&path)
        .ok_or_else(|| RpcError::internal("the tilemap vanished"))?;
    let changes = stamp
        .paste(map, layer, (x, y), skip_empty)
        .map_err(|e| from_stamp(&e))?;
    Ok(json!({
        "stamp": { "width": stamp.width, "height": stamp.height },
        "count": changes.len(),
        "truncated": changes.len() > MAX_LISTED_CHANGES,
        "changed": changes.iter().take(MAX_LISTED_CHANGES).map(change_json).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests;
