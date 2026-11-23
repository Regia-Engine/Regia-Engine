// SPDX-License-Identifier: MPL-2.0
//! `wfc.solve`: Wave Function Collapse as a graph node (spec 63 addendum, ADR-2724).
//!
//! Solves a tiled model (socket tiles, `crate::wfc::TiledModel::from_sockets`) or an overlapping
//! model (a sample of rows, one character per symbol) on a grid, seeded from the node seed, and emits
//! one point per cell: at the cell center, turned by the variant's quarter turns (mirrored variants
//! get a negative X scale) and carrying the chosen tile as attributes.

use crate::data::{AttrColumn, Bundle, DataItem, Point, PointSet};
use crate::exec::NodeCtx;
use crate::nodes::common::euler_deg;
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, num, pin_in, pin_out, set,
};
use crate::value::SettingValue;
use crate::wfc::{
    Boundary, CellConstraints, GridSize, OverlappingModel, SampleGrid, Symmetry, TileClass, TileSpec,
    TiledModel, WfcError, WfcModel, WfcOptions,
};

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[BuiltinNode::new(
    "wfc.solve",
    "wfc",
    "Wave Function Collapse on a grid (socket tiles or an overlapping sample, with rotation and reflection symmetry); one point per cell",
    &[
        set("mode", T::Enum(&["tiled", "overlapping"]), "tiled", "Model kind"),
        set(
            "tiles",
            T::Any,
            "[]",
            "tiled (or a table on the `tileset` input, columns name, east, south, west, north, weight?, class?, up?, down?): [(name: \"road\", weight: 1.0, sockets: (\"a\", \"b\", \"a\", \"b\"), class: \"I\", up: \"u\", down: \"u\")], sockets east, south, west, north read clockwise; class X, I, \\\\, T, L or F (optional); up/down sockets for 3D grids (optional)",
        ),
        set(
            "sample",
            T::NameList,
            "[]",
            "overlapping: sample rows, one character per symbol, all rows the same length",
        ),
        num("n", T::Int, "2", 1.0, 8.0, "overlapping: pattern size"),
        set("periodic_input", T::Bool, "true", "overlapping: patterns wrap around the sample edges"),
        set(
            "symmetry",
            T::Enum(&["none", "rotate", "reflect", "all"]),
            "none",
            "Tile or pattern variants: quarter turns, the x mirror, or all eight",
        ),
        num("size", T::Vec3, "(8.0, 8.0, 1.0)", 1.0, 4096.0, "Cells along x, rows along z, layers along y (integers)"),
        num("cell_m", T::Float, "1.0", 1.0e-3, 1.0e4, "Cell size, meters"),
        set("origin", T::Vec3, "(0.0, 0.0, 0.0)", "World position of the grid's min corner"),
        set("periodic", T::Bool, "false", "The output wraps around its edges"),
        num("max_backtracks", T::Int, "10000", 0.0, 1.0e7, "Backtrack budget"),
        set(
            "border",
            T::Name,
            "\"\"",
            "tiled, 2D: tile name forced on every border cell (any of its variants; empty = none)",
        ),
        num(
            "seed",
            T::Int,
            "-1",
            -1.0,
            9.0e15,
            "Solver seed; -1 = from the generator seed (the same seed as procgen.wfc_solve gives the same grid)",
        ),
    ],
    |_| {
        PinSpec::new(
            vec![
                pin_in("bounds", crate::nodes::SPATIAL),
                pin_in("tileset", crate::data::KindSet::TABLE),
            ],
            vec![pin_out("out", POINTS)],
        )
    },
    eval_solve,
)
.checked(check_solve)];

/// A built model plus, per tile id, `(name, base name, quarter turns, mirrored)`.
type Built = (WfcModel, Vec<(String, String, u8, bool)>);

fn symmetry(s: &Settings) -> Symmetry {
    Symmetry::from_name(s.text("symmetry")).unwrap_or_default()
}

fn grid_size(s: &Settings) -> Result<GridSize, String> {
    let v = s.v3("size");
    if v.iter().any(|x| *x < 1.0 || x.fract() != 0.0) {
        return Err("`size` needs whole numbers of at least 1".to_string());
    }
    let size = GridSize::new_3d(v[0] as usize, v[1] as usize, v[2] as usize);
    match size.cell_count() {
        Some(c) if c <= crate::wfc::MAX_CELLS => Ok(size),
        _ => Err(format!(
            "`size` asks for more than {} cells",
            crate::wfc::MAX_CELLS
        )),
    }
}

fn socket_tiles(v: Option<&SettingValue>) -> Result<Vec<TileSpec>, String> {
    let items = v
        .and_then(SettingValue::as_items)
        .ok_or("`tiles` must be a list")?;
    items
        .iter()
        .map(|t| {
            let name = t
                .field("name")
                .and_then(SettingValue::as_text)
                .ok_or("each tile needs a `name`")?
                .to_string();
            let weight = t.field("weight").and_then(SettingValue::as_f64).unwrap_or(1.0);
            let sockets: Vec<String> = t
                .field("sockets")
                .and_then(SettingValue::as_items)
                .map(|v| v.iter().filter_map(|x| x.as_text().map(str::to_string)).collect())
                .unwrap_or_default();
            let Ok(sockets) = <[String; 4]>::try_from(sockets) else {
                return Err(format!(
                    "tile `{name}` needs four sockets (east, south, west, north)"
                ));
            };
            let class = match t.field("class") {
                None => None,
                Some(c) => Some(
                    c.as_text()
                        .and_then(TileClass::from_name)
                        .ok_or_else(|| format!("tile `{name}`: `class` is one of X, I, \\, T, L, F"))?,
                ),
            };
            let text = |k: &str| t.field(k).map(|v| v.as_text().map(str::to_string));
            let vertical = match (text("up"), text("down")) {
                (None, None) => None,
                (Some(Some(up)), Some(Some(down))) => Some([up, down]),
                _ => {
                    return Err(format!(
                        "tile `{name}`: `up` and `down` are socket names, given together"
                    ));
                }
            };
            Ok(TileSpec {
                name,
                weight,
                sockets,
                class,
                vertical,
            })
        })
        .collect()
}

/// The sample rows as a symbol grid (each distinct character one symbol, its code point the value).
fn sample_grid(rows: &[String]) -> Result<SampleGrid, String> {
    let width = rows.first().map_or(0, |r| r.chars().count());
    if width == 0 || rows.iter().any(|r| r.chars().count() != width) {
        return Err("`sample` needs non-empty rows of equal length".to_string());
    }
    let cells: Vec<u32> = rows.iter().flat_map(|r| r.chars().map(u32::from)).collect();
    SampleGrid::new_2d(width, rows.len(), cells).map_err(|e| e.to_string())
}

/// Tiles from a data table (`in.table` of a tile-set asset): columns `name`, `east`, `south`, `west`,
/// `north` (names), optional `weight` (number), `class`, `up` and `down` (names; an empty `class` is
/// none, empty `up` and `down` are no vertical sockets).
fn table_tiles(t: &crate::data::Table) -> Result<Vec<TileSpec>, String> {
    use crate::data::AttrValue;
    let text = |col: &str, r: usize| -> Option<String> {
        match t.column(col)?.get(r)? {
            AttrValue::Name(v) | AttrValue::Asset(v) => Some(v),
            _ => None,
        }
    };
    for col in ["name", "east", "south", "west", "north"] {
        if t.column(col).is_none() {
            return Err(format!("the tile-set table has no `{col}` column"));
        }
    }
    (0..t.rows)
        .map(|r| {
            let name = text("name", r).ok_or_else(|| format!("tile-set row {r}: `name` is not a name"))?;
            let side = |c: &str| text(c, r).ok_or_else(|| format!("tile `{name}`: `{c}` is not a name"));
            let sockets = [side("east")?, side("south")?, side("west")?, side("north")?];
            let weight = t
                .column("weight")
                .and_then(|c| c.components(r))
                .and_then(|v| v.first().copied())
                .unwrap_or(1.0);
            let class = match text("class", r).filter(|c| !c.is_empty()) {
                None => None,
                Some(c) => Some(
                    TileClass::from_name(&c)
                        .ok_or_else(|| format!("tile `{name}`: `class` is one of X, I, \\, T, L, F"))?,
                ),
            };
            let vertical = match (
                text("up", r).filter(|v| !v.is_empty()),
                text("down", r).filter(|v| !v.is_empty()),
            ) {
                (None, None) => None,
                (Some(up), Some(down)) => Some([up, down]),
                _ => return Err(format!("tile `{name}`: `up` and `down` are given together")),
            };
            Ok(TileSpec {
                name,
                weight,
                sockets,
                class,
                vertical,
            })
        })
        .collect()
}

fn build(s: &Settings) -> Result<Built, (&'static str, String)> {
    build_with(s, None)
}

/// [`build`] with the tiles of a connected `tileset` table instead of the `tiles` setting.
fn build_with(s: &Settings, table: Option<&crate::data::Table>) -> Result<Built, (&'static str, String)> {
    let bad = |m: String| ("setting_type", m);
    let sym = symmetry(s);
    if s.text("mode") == "overlapping" {
        let grid = sample_grid(&s.names("sample")).map_err(bad)?;
        let n = s.i("n").clamp(1, 8) as usize;
        let model = OverlappingModel::from_sample_symmetric(&grid, n, s.b("periodic_input"), sym)
            .map_err(|e| ("wfc_invalid_model", e.to_string()))?;
        let names = model
            .patterns()
            .iter()
            .map(|p| {
                let symbol = char::from_u32(p.origin()).map_or_else(|| p.origin().to_string(), String::from);
                (symbol.clone(), symbol, 0, false)
            })
            .collect();
        return Ok((WfcModel::Overlapping(model), names));
    }
    let tiles = match table {
        Some(t) => table_tiles(t).map_err(bad)?,
        None => socket_tiles(s.get("tiles")).map_err(bad)?,
    };
    let model = TiledModel::from_specs(&tiles, sym).map_err(|e| ("wfc_invalid_model", e.to_string()))?;
    let names = model
        .tiles()
        .iter()
        .enumerate()
        .map(|(id, t)| {
            let v = model.variant(id).unwrap_or_default();
            let base = tiles.get(v.base).map_or_else(String::new, |b| b.name.clone());
            (t.name.clone(), base, v.rotation, v.mirrored)
        })
        .collect();
    Ok((WfcModel::Tiled(model), names))
}

fn check_solve(s: &Settings) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if let Err(m) = grid_size(s) {
        e.push(("setting_out_of_range".to_string(), m));
    }
    if Symmetry::from_name(s.text("symmetry")).is_none() {
        e.push(("setting_type".to_string(), "unknown `symmetry`".to_string()));
    }
    // An empty tile list or sample (the defaults) fails at run time with `wfc_invalid_model`, so a node
    // dropped from the palette validates before it is filled in.
    let declared = if s.text("mode") == "overlapping" {
        !s.names("sample").is_empty()
    } else {
        s.get("tiles")
            .and_then(SettingValue::as_items)
            .is_some_and(|v| !v.is_empty())
    };
    if declared && let Err((code, m)) = build(s) {
        e.push((code.to_string(), m));
    }
    e
}

fn wfc_error(e: WfcError) -> NodeError {
    let code = match e {
        WfcError::InvalidModel(_) => "wfc_invalid_model",
        WfcError::Contradiction => "wfc_contradiction",
        WfcError::BacktrackLimit => "wfc_backtrack_limit",
        WfcError::Cancelled => "cancelled",
    };
    NodeError::new(code, e.to_string())
}

/// The solver seed: the `seed` setting when it is not negative, else the generator-derived seed.
fn solver_seed(ctx: &NodeCtx<'_>, s: &Settings) -> u64 {
    match u64::try_from(s.i("seed")) {
        Ok(seed) => seed,
        Err(_) => (u64::from(ctx.node_seed) << 32) | u64::from(ctx.tier_seed),
    }
}

/// A grid origin and its `x`/`z` cell counts.
type GridCover = ([f64; 3], [usize; 2]);

/// With `bounds` connected: the grid origin and `x`/`z` cell counts that cover the world box of
/// every input item (points, shapes, ground); the layer count stays `size.z`.
fn bounds_grid(inputs: &[Bundle], s: &Settings) -> Result<Option<GridCover>, NodeError> {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for item in crate::nodes::input_items(inputs, 0) {
        if let Some((a, b)) = crate::nodes::common::world_bounds(item) {
            lo = std::array::from_fn(|k| lo[k].min(f64::from(a[k])));
            hi = std::array::from_fn(|k| hi[k].max(f64::from(b[k])));
        }
    }
    if lo.iter().chain(&hi).any(|v| !v.is_finite()) {
        return Ok(None);
    }
    let cell = s.f("cell_m");
    let count = |k: usize| -> Result<usize, NodeError> {
        let n = ((hi[k] - lo[k]) / cell).ceil().max(1.0);
        if n > 4096.0 {
            return Err(NodeError::new(
                "setting_out_of_range",
                format!("`bounds` spans {n} cells along an axis; at most 4096"),
            ));
        }
        Ok(n as usize)
    };
    Ok(Some((lo, [count(0)?, count(2)?])))
}

fn eval_solve(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let mut size = grid_size(s).map_err(|m| NodeError::new("setting_out_of_range", m))?;
    let mut origin = s.v3("origin");
    if let Some((min, [nx, nz])) = bounds_grid(inputs, s)? {
        origin = min;
        size = GridSize::new_3d(nx, nz, size.depth);
        if size.cell_count().is_none_or(|c| c > crate::wfc::MAX_CELLS) {
            return Err(NodeError::new(
                "setting_out_of_range",
                format!("`bounds` asks for more than {} cells", crate::wfc::MAX_CELLS),
            ));
        }
    }
    let cells = size.cell_count().unwrap_or(usize::MAX);
    ctx.check_points(cells)?;
    let table = crate::nodes::input_items(inputs, 1)
        .iter()
        .find_map(|i| match &i.payload {
            crate::data::Payload::Table(t) => Some(t),
            _ => None,
        });
    let (model, names) = build_with(s, table).map_err(|(code, m)| NodeError::new(code, m))?;
    let mut constraints = CellConstraints::new();
    let border = s.text("border");
    if !border.is_empty() && size.depth == 1 && matches!(model, WfcModel::Tiled(_)) {
        let allowed: Vec<usize> = (0..names.len()).filter(|&k| names[k].1 == border).collect();
        if allowed.is_empty() {
            return Err(NodeError::new(
                "wfc_invalid_model",
                format!("`border` names no tile `{border}`"),
            ));
        }
        constraints = CellConstraints::with_border_2d(size, &allowed);
    }
    let boundary = if s.b("periodic") {
        Boundary::Periodic
    } else {
        Boundary::Fixed
    };
    let opts = WfcOptions::default()
        .with_boundary(boundary)
        .with_max_backtracks(s.i("max_backtracks").max(0) as usize);
    let seed = solver_seed(ctx, s);
    let grid = crate::wfc::solve_cancellable(&model, size, &constraints, seed, &opts, &|| {
        ctx.check_cancel().is_err()
    })
    .map_err(wfc_error)?;

    let cell = s.f("cell_m");
    let half = (cell * 0.5) as f32;
    let mut p = PointSet::new();
    let (mut tile, mut base, mut ids, mut turns, mut mirrored) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (index, &id) in grid.tiles().iter().enumerate() {
        let Some([x, y, z]) = size.coords(index) else {
            continue;
        };
        let (name, base_name, rotation, mirror) = names.get(id).cloned().unwrap_or_default();
        // Grid x is world +x, rows (grid y, south) world +z, layers (grid z, up) world +y.
        let pos = [
            (origin[0] + (x as f64 + 0.5) * cell) as f32,
            (origin[1] + z as f64 * cell) as f32,
            (origin[2] + (y as f64 + 0.5) * cell) as f32,
        ];
        // A clockwise quarter turn seen from above takes +x to +z: a yaw of -90 degrees.
        let rot = euler_deg([0.0, -90.0 * f64::from(rotation), 0.0]).normalize();
        p.push(Point {
            position: pos,
            rotation: rot.to_array(),
            scale: [if mirror { -1.0 } else { 1.0 }, 1.0, 1.0],
            seed: ctx.point_seed(pos),
            bounds_min: [-half, 0.0, -half],
            bounds_max: [half, cell as f32, half],
            ..Point::default()
        });
        tile.push(name);
        base.push(base_name);
        ids.push(id as i64);
        turns.push(i64::from(rotation));
        mirrored.push(mirror);
    }
    p.attrs.insert("tile".to_string(), AttrColumn::Name(tile.into()));
    p.attrs
        .insert("tile_base".to_string(), AttrColumn::Name(base.into()));
    p.attrs.insert("tile_id".to_string(), AttrColumn::Int(ids.into()));
    p.attrs
        .insert("tile_turns".to_string(), AttrColumn::Int(turns.into()));
    p.attrs
        .insert("tile_mirrored".to_string(), AttrColumn::Bool(mirrored.into()));
    ctx.count("cells", cells as u64);
    Ok(vec![Bundle::one(DataItem::points(p))])
}
