// SPDX-License-Identifier: MPL-2.0
//! `procgen.wfc_solve`: run the Wave Function Collapse generator (`regia_procgen::wfc`) on an
//! inline tileset or sample and answer the solved grid (book `agents/procgen.md`, guide topic
//! `procgen`). Stateless: nothing in the world is read or written.
//!
//! Errors answer `F0002` (invalid params) with `data.reason` one of `wfc_bad_params`,
//! `wfc_invalid_model`, `wfc_contradiction` or `wfc_backtrack_limit`.

use regia_ecs::prelude::*;
use regia_procgen::wfc::{
    AdjacencyRule, CellConstraints, Direction, GridSize, OverlappingModel, SampleGrid, Symmetry, TileClass,
    TileDef, TileId, TileSpec, TiledModel, WfcError, WfcModel, WfcOptions, solve,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most output cells one `procgen.wfc_solve` call may ask for (the answer carries two arrays of
/// this length).
pub(crate) const MAX_WFC_CELLS: usize = 65_536;

/// Most tile variants one call may build: the input tiles times the transforms `symmetry` adds
/// to each socket tile (ADR-5597). Building the adjacency table costs `6 * n * n` checks for
/// `n` variants on the world thread, so an unbounded tile list could hang the engine for
/// minutes or abort it on the table allocation.
pub(crate) const MAX_WFC_TILE_VARIANTS: usize = 2_048;

/// Most patterns an overlapping sample may yield: its cells times the transforms of
/// `symmetry` (ADR-5597). Pattern adjacency compares every pair of patterns.
pub(crate) const MAX_WFC_SAMPLE_PATTERNS: usize = 4_096;

/// Largest pattern block (`n` squared in 2D, `n` cubed in 3D) an overlapping model may use.
pub(crate) const MAX_WFC_PATTERN_CELLS: usize = 64;

fn reason(why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({ "code": "F0002", "reason": why, "hint": "see book/agents/procgen.md" })),
    }
}

fn bad(message: impl Into<String>) -> RpcError {
    reason("wfc_bad_params", message)
}

fn from_wfc(error: WfcError) -> RpcError {
    let why = match &error {
        WfcError::InvalidModel(_) => "wfc_invalid_model",
        WfcError::Contradiction => "wfc_contradiction",
        WfcError::BacktrackLimit => "wfc_backtrack_limit",
        // This method runs to the end of the budget: it never passes a cancel check.
        WfcError::Cancelled => "cancelled",
    };
    reason(why, error.to_string())
}

fn index(value: &Value, what: &str) -> Result<usize, RpcError> {
    value
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| bad(format!("{what} must be a non-negative integer")))
}

fn index_list(value: Option<&Value>, what: &str) -> Result<Vec<usize>, RpcError> {
    let Some(Value::Array(items)) = value else {
        return Err(bad(format!("{what} must be an array of non-negative integers")));
    };
    items.iter().map(|v| index(v, what)).collect()
}

/// `[w, h]` or `[w, h, d]`.
fn grid_size(value: Option<&Value>, what: &str) -> Result<GridSize, RpcError> {
    let dims = index_list(value, what)?;
    match dims.as_slice() {
        [w, h] => Ok(GridSize::new_2d(*w, *h)),
        [w, h, d] => Ok(GridSize::new_3d(*w, *h, *d)),
        _ => Err(bad(format!(
            "{what} must be [width, height] or [width, height, depth]"
        ))),
    }
}

fn direction(value: Option<&Value>) -> Result<Direction, RpcError> {
    match value.and_then(Value::as_str) {
        Some("east") => Ok(Direction::East),
        Some("west") => Ok(Direction::West),
        Some("south") => Ok(Direction::South),
        Some("north") => Ok(Direction::North),
        Some("up") => Ok(Direction::Up),
        Some("down") => Ok(Direction::Down),
        _ => Err(bad(
            "rules[].dir must be one of east, west, south, north, up, down",
        )),
    }
}

/// `symmetry`: none (default), rotate, reflect or all.
fn symmetry(params: &Value) -> Result<Symmetry, RpcError> {
    match params.get("symmetry") {
        None | Some(Value::Null) => Ok(Symmetry::None),
        Some(v) => v
            .as_str()
            .and_then(Symmetry::from_name)
            .ok_or_else(|| bad("`symmetry` must be one of none, rotate, reflect, all")),
    }
}

/// Socket tiles (`tiles[].sockets: [east, south, west, north]`, optional `class` and `up`/`down`)
/// expanded by `symmetry`.
fn socket_model(tiles: &[Value], params: &Value) -> Result<TiledModel, RpcError> {
    if params.get("rules").is_some_and(|r| !r.is_null()) {
        return Err(bad(
            "socket tiles derive their adjacency: give `rules` or `sockets`, not both",
        ));
    }
    let transforms = symmetry(params)?.transforms().len();
    if tiles.len().saturating_mul(transforms) > MAX_WFC_TILE_VARIANTS {
        return Err(bad(format!(
            "{} socket tiles with {transforms} symmetry transforms each exceed the \
             {MAX_WFC_TILE_VARIANTS}-variant limit; run larger tilesets from code",
            tiles.len()
        )));
    }
    let mut defs = Vec::with_capacity(tiles.len());
    for (i, tile) in tiles.iter().enumerate() {
        // A wrong-typed name is refused, not replaced by `tile<i>` (ADR-2622).
        let name = match tile.get("name") {
            None | Some(Value::Null) => format!("tile{i}"),
            Some(n) => n
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| bad(format!("tiles[{i}].name must be a string")))?,
        };
        let weight = match tile.get("weight") {
            None | Some(Value::Null) => 1.0,
            Some(v) => v
                .as_f64()
                .ok_or_else(|| bad(format!("tiles[{i}].weight must be a number")))?,
        };
        let sockets: Option<Vec<String>> = tile
            .get("sockets")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
        let sockets = sockets
            .and_then(|v| <[String; 4]>::try_from(v).ok())
            .ok_or_else(|| {
                bad(format!(
                    "tiles[{i}].sockets must be four strings (east, south, west, north)"
                ))
            })?;
        let class = match tile.get("class") {
            None | Some(Value::Null) => None,
            Some(c) => Some(
                c.as_str()
                    .and_then(TileClass::from_name)
                    .ok_or_else(|| bad(format!("tiles[{i}].class must be one of X, I, \\, T, L, F")))?,
            ),
        };
        let text = |k: &str| {
            tile.get(k)
                .filter(|v| !v.is_null())
                .map(|v| v.as_str().map(str::to_string))
        };
        let vertical = match (text("up"), text("down")) {
            (None, None) => None,
            (Some(Some(up)), Some(Some(down))) => Some([up, down]),
            _ => {
                return Err(bad(format!(
                    "tiles[{i}].up and tiles[{i}].down are socket strings, given together"
                )));
            }
        };
        defs.push(TileSpec {
            name,
            weight,
            sockets,
            class,
            vertical,
        });
    }
    TiledModel::from_specs(&defs, symmetry(params)?).map_err(from_wfc)
}

fn tiled_model(tiles: &[Value], params: &Value) -> Result<TiledModel, RpcError> {
    if tiles.iter().any(|t| t.get("sockets").is_some()) {
        return socket_model(tiles, params);
    }
    if !matches!(symmetry(params)?, Symmetry::None) {
        return Err(bad(
            "`symmetry` needs socket tiles (`tiles[].sockets`) or a `sample`",
        ));
    }
    if tiles.len() > MAX_WFC_TILE_VARIANTS {
        return Err(bad(format!(
            "{} tiles exceed the {MAX_WFC_TILE_VARIANTS}-tile limit; run larger tilesets from code",
            tiles.len()
        )));
    }
    let mut defs = Vec::with_capacity(tiles.len());
    for (i, tile) in tiles.iter().enumerate() {
        let name = crate::params::opt_str(tile, "name")?.map_or_else(|| format!("tile{i}"), str::to_string);
        let weight = match tile.get("weight") {
            None | Some(Value::Null) => 1.0,
            Some(v) => v
                .as_f64()
                .ok_or_else(|| bad(format!("tiles[{i}].weight must be a number")))?,
        };
        defs.push(TileDef::with_weight(name, weight));
    }
    let mut rules = Vec::new();
    match params.get("rules") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for rule in items {
                let tile = index(rule.get("tile").unwrap_or(&Value::Null), "rules[].tile")?;
                let dir = direction(rule.get("dir"))?;
                let allowed: Vec<TileId> = index_list(rule.get("allowed"), "rules[].allowed")?;
                rules.push(AdjacencyRule::new(tile, dir, allowed));
            }
        }
        Some(_) => return Err(bad("`rules` must be an array")),
    }
    TiledModel::new(defs, rules).map_err(from_wfc)
}

fn overlapping_model(sample: &Value, params: &Value) -> Result<OverlappingModel, RpcError> {
    let size = grid_size(sample.get("size"), "sample.size")?;
    let Some(Value::Array(cells)) = sample.get("cells") else {
        return Err(bad("sample.cells must be an array of symbols (u32)"));
    };
    let transforms = symmetry(params)?.transforms().len();
    if cells.len().saturating_mul(transforms) > MAX_WFC_SAMPLE_PATTERNS {
        return Err(bad(format!(
            "a sample of {} cells with {transforms} symmetry transforms can yield more than \
             {MAX_WFC_SAMPLE_PATTERNS} patterns; use a smaller sample",
            cells.len()
        )));
    }
    let cells: Vec<u32> = cells
        .iter()
        .map(|v| v.as_u64().and_then(|s| u32::try_from(s).ok()))
        .collect::<Option<_>>()
        .ok_or_else(|| bad("sample.cells must hold u32 symbols"))?;
    let grid = SampleGrid::new(size, cells).map_err(from_wfc)?;
    let n = match params.get("n") {
        None | Some(Value::Null) => 3,
        Some(v) => index(v, "`n`")?,
    };
    let dimensions = u32::try_from(size.dimensions()).unwrap_or(3);
    if n.checked_pow(dimensions)
        .is_none_or(|block| block > MAX_WFC_PATTERN_CELLS)
    {
        return Err(bad(format!(
            "`n` = {n} makes a pattern block over {MAX_WFC_PATTERN_CELLS} cells"
        )));
    }
    let periodic_input = crate::params::opt_bool(params, "periodic_input")?.unwrap_or(true);
    OverlappingModel::from_sample_symmetric(&grid, n, periodic_input, symmetry(params)?).map_err(from_wfc)
}

fn tiles_have_sockets(params: &Value) -> bool {
    params
        .get("tiles")
        .and_then(Value::as_array)
        .is_some_and(|t| t.iter().any(|x| x.get("sockets").is_some()))
}

/// `procgen.wfc_solve { size, tiles?, rules?, sample?, n?, periodic_input?, constraints?, seed?,
/// periodic?, max_backtracks? }`: builds a tiled model (`tiles` + `rules`) or an overlapping model
/// (`sample` + `n`), solves it, and answers `{ model, size, tile_count, seed, tiles, values }`.
pub(crate) fn wfc_solve(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent.procgen.wfc_solve").entered();
    let size = grid_size(params.get("size"), "`size`")?;
    match size.cell_count() {
        Some(cells) if cells <= MAX_WFC_CELLS => {}
        _ => {
            return Err(bad(format!(
                "`size` asks for more than {MAX_WFC_CELLS} cells; run larger grids from code"
            )));
        }
    }
    let (model, kind) = match (params.get("tiles"), params.get("sample")) {
        (Some(Value::Array(tiles)), None | Some(Value::Null)) => {
            (WfcModel::Tiled(tiled_model(tiles, params)?), "tiled")
        }
        (None | Some(Value::Null), Some(sample @ Value::Object(_))) => (
            WfcModel::Overlapping(overlapping_model(sample, params)?),
            "overlapping",
        ),
        _ => return Err(bad("give exactly one of `tiles` (array) or `sample` (object)")),
    };
    let mut constraints = CellConstraints::new();
    match params.get("constraints") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                let cell = index_list(item.get("cell"), "constraints[].cell")?;
                let coord = match cell.as_slice() {
                    [x, y] => [*x, *y, 0],
                    [x, y, z] => [*x, *y, *z],
                    _ => return Err(bad("constraints[].cell must be [x, y] or [x, y, z]")),
                };
                let allowed = index_list(item.get("allowed"), "constraints[].allowed")?;
                constraints.force(coord, &allowed);
            }
        }
        Some(_) => return Err(bad("`constraints` must be an array")),
    }
    let seed = match params.get("seed") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_u64().ok_or_else(|| bad("`seed` must be a u64"))?,
    };
    let mut opts = WfcOptions::default();
    if crate::params::opt_bool(params, "periodic")? == Some(true) {
        opts = WfcOptions::periodic();
    }
    if let Some(v) = params.get("max_backtracks").filter(|v| !v.is_null()) {
        opts.max_backtracks = index(v, "`max_backtracks`")?;
    }
    let grid = solve(&model, size, &constraints, seed, &opts).map_err(from_wfc)?;
    let mut out = json!({
        "model": kind,
        "size": [size.width, size.height, size.depth],
        "tile_count": model.tile_count(),
        "seed": seed,
        "tiles": grid.tiles(),
        "values": grid.values(),
    });
    // Socket tiles: what each tile id is (declared tile, quarter turns, mirror).
    if let (WfcModel::Tiled(m), Value::Object(o)) = (&model, &mut out)
        && tiles_have_sockets(params)
    {
        let variants: Vec<Value> = (0..m.tile_count())
            .filter_map(|id| {
                let v = m.variant(id)?;
                Some(json!({ "name": m.tile_name(id), "base": v.base, "turns": v.rotation, "mirrored": v.mirrored }))
            })
            .collect();
        o.insert("variants".into(), Value::Array(variants));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason_of(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn world() -> World {
        World::new()
    }

    /// Grass (0) and an east-west road (1): road must continue east and west.
    fn road_params(seed: u64) -> Value {
        json!({
            "size": [12, 6],
            "tiles": [{ "name": "grass", "weight": 4.0 }, { "name": "road" }],
            "rules": [
                { "tile": 0, "dir": "east", "allowed": [0] },
                { "tile": 0, "dir": "west", "allowed": [0] },
                { "tile": 1, "dir": "east", "allowed": [1] },
                { "tile": 1, "dir": "west", "allowed": [1] },
            ],
            "constraints": [{ "cell": [0, 2], "allowed": [1] }],
            "seed": seed,
        })
    }

    #[test]
    fn tiled_solve_answers_a_valid_grid_and_honors_constraints() {
        let mut w = world();
        let r = wfc_solve(&mut w, &road_params(5)).expect("solve");
        assert_eq!(r["model"], "tiled");
        assert_eq!(r["size"], json!([12, 6, 1]));
        let tiles: Vec<u64> = r["tiles"]
            .as_array()
            .expect("tiles")
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        assert_eq!(tiles.len(), 72);
        // Row 2 is forced to start with road, so the whole row is road; every row is uniform.
        for y in 0..6 {
            let row = &tiles[y * 12..(y + 1) * 12];
            assert!(row.iter().all(|t| *t == row[0]), "row {y} mixes tiles: {row:?}");
        }
        assert!(tiles[24..36].iter().all(|t| *t == 1));
        // Same params, same answer.
        assert_eq!(wfc_solve(&mut w, &road_params(5)).expect("again"), r);
    }

    #[test]
    fn overlapping_solve_reproduces_sample_symbols() {
        let mut w = world();
        let r = wfc_solve(
            &mut w,
            &json!({
                "size": [8, 8],
                "sample": { "size": [4, 4], "cells": [0, 1, 0, 1, 1, 0, 1, 0, 0, 1, 0, 1, 1, 0, 1, 0] },
                "n": 2,
                "periodic": true,
                "seed": 3,
            }),
        )
        .expect("solve");
        assert_eq!(r["model"], "overlapping");
        let values: Vec<u64> = r["values"]
            .as_array()
            .expect("values")
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        assert_eq!(values.len(), 64);
        // A checkerboard sample only admits checkerboard output.
        for y in 0..8 {
            for x in 0..8 {
                assert_ne!(values[y * 8 + x], values[y * 8 + (x + 1) % 8]);
            }
        }
    }

    #[test]
    fn errors_carry_their_reasons() {
        let mut w = world();
        let e = wfc_solve(&mut w, &json!({ "size": [4, 4] })).expect_err("no model");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"));
        let e = wfc_solve(&mut w, &json!({ "size": [1000, 1000], "tiles": [{}] })).expect_err("too big");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"));
        let e = wfc_solve(&mut w, &json!({ "size": [4, 4], "tiles": [] })).expect_err("empty");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_invalid_model"));
        // Two tiles that must alternate cannot fill an odd periodic ring.
        let ring = |budget: u64| {
            json!({
                "size": [3, 1],
                "tiles": [{}, {}],
                "rules": [
                    { "tile": 0, "dir": "east", "allowed": [1] },
                    { "tile": 0, "dir": "west", "allowed": [1] },
                    { "tile": 1, "dir": "east", "allowed": [0] },
                    { "tile": 1, "dir": "west", "allowed": [0] },
                ],
                "periodic": true,
                "max_backtracks": budget,
            })
        };
        let e = wfc_solve(&mut w, &ring(100)).expect_err("unsatisfiable");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_contradiction"));
        let e = wfc_solve(&mut w, &ring(0)).expect_err("no budget");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_backtrack_limit"));
        let e = wfc_solve(
            &mut w,
            &json!({ "size": [4, 4], "tiles": [{}], "rules": [{ "tile": 0, "dir": "sideways", "allowed": [] }] }),
        )
        .expect_err("bad dir");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"));
    }

    /// corr-agent (ADR-5597): the tile list and the overlapping sample were unbounded, so a
    /// read-only call could make the world thread build an adjacency table of `6 * n * n`
    /// entries for any `n` (minutes of work, or an aborting allocation). Each of these calls was
    /// answered before the caps.
    #[test]
    fn oversized_tilesets_and_samples_are_refused_before_any_table_is_built() {
        let mut w = world();
        // 300 socket tiles x 8 transforms = 2 400 variants (> 2 048).
        let tiles: Vec<Value> = (0..300)
            .map(|i| json!({ "sockets": [format!("a{i}"), format!("b{i}"), format!("c{i}"), format!("d{i}")] }))
            .collect();
        let e = wfc_solve(
            &mut w,
            &json!({ "size": [2, 2], "symmetry": "all", "tiles": tiles }),
        )
        .expect_err("too many socket variants");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"), "{e:?}");
        // The same tiles without symmetry fit.
        assert!(wfc_solve(&mut w, &json!({ "size": [1, 1], "tiles": tiles })).is_ok());

        let plain: Vec<Value> = (0..=MAX_WFC_TILE_VARIANTS).map(|_| json!({})).collect();
        let e = wfc_solve(&mut w, &json!({ "size": [1, 1], "tiles": plain })).expect_err("too many tiles");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"), "{e:?}");

        // A 24 x 24 sample with 8 transforms may yield 4 608 patterns (> 4 096).
        let cells: Vec<u32> = (0..24 * 24).collect();
        let e = wfc_solve(
            &mut w,
            &json!({ "size": [2, 2], "symmetry": "all", "n": 2, "sample": { "size": [24, 24], "cells": cells } }),
        )
        .expect_err("too many patterns");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"), "{e:?}");

        // n = 9 in 2D is an 81-cell block (> 64).
        let cells: Vec<u32> = (0..81).map(|i| i % 2).collect();
        let e = wfc_solve(
            &mut w,
            &json!({ "size": [2, 2], "n": 9, "sample": { "size": [9, 9], "cells": cells } }),
        )
        .expect_err("pattern block too large");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"), "{e:?}");
    }

    #[test]
    fn socket_tiles_with_rotation_symmetry_answer_variants_and_a_matching_grid() {
        let mut w = world();
        // A straight road (sockets r on east and west) and grass: with rotations the road also runs
        // north-south, as variant `road@r1`.
        let params = json!({
            "size": [10, 10],
            "tiles": [
                { "name": "grass", "weight": 3.0, "sockets": ["g", "g", "g", "g"] },
                { "name": "road", "sockets": ["r", "g", "r", "g"] },
            ],
            "symmetry": "rotate",
            "seed": 9,
        });
        let r = wfc_solve(&mut w, &params).expect("solve");
        let names: Vec<&str> = r["variants"]
            .as_array()
            .expect("variants")
            .iter()
            .filter_map(|v| v["name"].as_str())
            .collect();
        assert_eq!(names, ["grass", "road", "road@r1"], "duplicate turns are dropped");
        assert_eq!(
            r,
            wfc_solve(&mut w, &params).expect("again"),
            "same seed, same grid"
        );
    }

    #[test]
    fn symmetry_without_sockets_or_with_rules_is_refused() {
        let mut w = world();
        let mut p = road_params(1);
        p["symmetry"] = json!("all");
        assert_eq!(
            reason_of(&wfc_solve(&mut w, &p).expect_err("no sockets")).as_deref(),
            Some("wfc_bad_params")
        );
        let p = json!({ "size": [4, 4], "tiles": [{ "sockets": ["a", "a", "a", "a"] }], "rules": [], "symmetry": "bad" });
        assert!(wfc_solve(&mut w, &p).is_err());
    }

    /// ADR-3027: classed 3D tiles answer variants by class, and the `wfc.solve` node with the same
    /// `seed` and tiles gives the same grid as this method.
    #[test]
    fn classed_3d_tiles_match_the_wfc_solve_node_for_the_same_seed() {
        use regia_procgen::{
            AttrColumn, CancelToken, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs,
        };
        let mut w = world();
        let p = json!({
            "size": [8, 8, 3],
            "tiles": [
                { "name": "air", "sockets": ["e", "e", "e", "e"], "class": "X", "up": "a", "down": "a" },
                { "name": "grass", "sockets": ["g", "g", "g", "g"], "class": "X", "up": "a", "down": "a" },
                { "name": "road", "sockets": ["g", "r", "g", "r"], "class": "I", "up": "a", "down": "a" },
                { "name": "corner", "sockets": ["r", "g", "g", "r"], "class": "L", "up": "a", "down": "a" },
            ],
            "symmetry": "all",
            "seed": 2912,
        });
        let answer = wfc_solve(&mut w, &p).expect("solve");
        // air 1, grass 1, road 2, corner 4.
        assert_eq!(answer["tile_count"], json!(8));
        let tiles: Vec<i64> = answer["tiles"]
            .as_array()
            .expect("tiles")
            .iter()
            .filter_map(Value::as_i64)
            .collect();
        let bad = json!({ "size": [4, 4], "tiles": [{ "sockets": ["r", "g", "g", "g"], "class": "T" }], "symmetry": "all" });
        assert_eq!(
            reason_of(&wfc_solve(&mut w, &bad).expect_err("class and sockets disagree")).as_deref(),
            Some("wfc_invalid_model")
        );

        let set = |e: NodeEntry, k: &str, v: &str| {
            e.with(k, regia_procgen::SettingValue::parse(v).expect("setting"))
        };
        let tiles_ron = "[(name: \"air\", sockets: (\"e\", \"e\", \"e\", \"e\"), class: \"X\", up: \"a\", down: \"a\"), \
            (name: \"grass\", sockets: (\"g\", \"g\", \"g\", \"g\"), class: \"X\", up: \"a\", down: \"a\"), \
            (name: \"road\", sockets: (\"g\", \"r\", \"g\", \"r\"), class: \"I\", up: \"a\", down: \"a\"), \
            (name: \"corner\", sockets: (\"r\", \"g\", \"g\", \"r\"), class: \"L\", up: \"a\", down: \"a\")]";
        let mut solve = NodeEntry::new("wfc.solve");
        for (k, v) in [
            ("tiles", tiles_ron),
            ("symmetry", "all"),
            ("size", "(8.0, 8.0, 3.0)"),
            ("seed", "2912"),
        ] {
            solve = set(solve, k, v);
        }
        let g = ProcGraph {
            nodes: [
                (1, solve),
                (2, set(NodeEntry::new("out.points"), "name", "\"cells\"")),
            ]
            .into(),
            edges: vec![regia_procgen::Edge::new(1, "out", 2, "in")],
            ..ProcGraph::default()
        };
        let c = g.compile(&ProcNodeRegistry::with_builtins()).expect("compile");
        let snap = std::sync::Arc::new(regia_procgen::StaticSnapshot::default());
        let r = Evaluator::new()
            .run(&c, &RunInputs::new(snap, 7), &CancelToken::new())
            .expect("run");
        let ids = match r.output.points.get("cells").and_then(|p| p.attrs.get("tile_id")) {
            Some(AttrColumn::Int(v)) => v.to_vec(),
            other => panic!("no tile_id: {other:?}"),
        };
        assert_eq!(ids, tiles, "the node and procgen.wfc_solve answer the same grid");
    }

    /// A wrong-typed socket-tile name is refused, not replaced by `tile<i>` (ADR-2622, ADR-5667).
    /// The rules model already read `name` through `opt_str`; the socket model did not.
    #[test]
    fn a_wrong_typed_socket_tile_name_is_refused() {
        let mut params = json!({
            "size": [10, 10],
            "tiles": [
                { "name": "grass", "weight": 3.0, "sockets": ["g", "g", "g", "g"] },
                { "name": "road", "sockets": ["r", "g", "r", "g"] },
            ],
            "symmetry": "rotate",
            "seed": 9,
        });
        params["tiles"][1]["name"] = json!(5);
        let e = wfc_solve(&mut world(), &params).expect_err("wrong-typed name");
        assert_eq!(reason_of(&e).as_deref(), Some("wfc_bad_params"), "{e:?}");
        assert!(
            e.message.contains("tiles[1].name must be a string"),
            "{}",
            e.message
        );
        params["tiles"][1]["name"] = Value::Null;
        wfc_solve(&mut world(), &params).expect("a null name is the default");
    }
}
