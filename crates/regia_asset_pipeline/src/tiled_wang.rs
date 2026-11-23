// SPDX-License-Identifier: MPL-2.0
//! Tiled terrain and Wang set import (`docs/specs/86-2d-toolkit.md` § 12, ADR-2597): the Wang
//! sets of a `.tsx` / `.tsj` tileset (or of the tilesets a `.tmx` / `.tmj` map uses) become
//! `.autotile.ron` terrains, one terrain per Wang color.
//!
//! A Wang tile carries eight color slots (Tiled's `wangid`: top, top-right, right, bottom-right,
//! bottom, bottom-left, left, top-left; 0 = no color). For the terrain of color `c` a tile is
//! usable when every slot its set type reads is `c` or 0 and at least one is `c`; the slots equal
//! to `c` form a mask, and the terrain's layout is indexed by that mask:
//!
//! - `corner` sets read the four corner slots → `WangCorner` (TL = 1, TR = 2, BR = 4, BL = 8);
//! - `edge` sets read the four edge slots → `Edge16` (N = 1, E = 2, S = 4, W = 8);
//! - `mixed` sets read all eight → `Blob47` (reduced masks only).
//!
//! A mask no tile covers takes the tile of the nearest covered mask (fewest differing bits, lowest
//! mask on a tie) and the import warns. Tiles that mix two non-zero colors (transitions between
//! two terrains) and all-zero tiles belong to no single terrain and are skipped. Tiled's older
//! `terrains` (JSON tilesets: `terrains` plus a per-tile `terrain` corner list, and the XML
//! `<terraintypes>` of `.tsx` / `.tmx`, which the `tiled` crate drops and
//! [`crate::tiled_import`] scans for) are read as corner sets: terrain `i` is Wang color `i + 1`
//! and a tile's four corner indices fill the corner slots, so an old file gives the same tiles as
//! the equivalent `corner` Wang set (ADR-2714). The slot layout is from the public Tiled map format documentation.

use std::path::Path;

use regia_2d::autotile::{AUTOTILE_FORMAT_VERSION, AutoTileSet, Border, Layout, Terrain, blob47_masks};
use serde::Deserialize;

use crate::tiled_import::{LegacyTerrains, TiledImportError};

/// One tileset's imported rules.
#[derive(Clone, Debug)]
pub struct ImportedAutoTile {
    /// The rules, bound by name to the tileset they came from.
    pub set: AutoTileSet,
    /// Skipped or approximated features, one line each.
    pub warnings: Vec<String>,
}

/// The kind of a Wang set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WangKind {
    /// Corner colors only.
    Corner,
    /// Edge colors only.
    Edge,
    /// Corners and edges.
    Mixed,
}

/// A Wang set in neutral form (from XML or JSON).
#[derive(Clone, Debug)]
pub struct WangData {
    /// Set name.
    pub name: String,
    /// Kind.
    pub kind: WangKind,
    /// Color names (index 0 is Tiled's color 1).
    pub colors: Vec<String>,
    /// `(local tile id, wangid)` pairs.
    pub tiles: Vec<(u32, [u8; 8])>,
}

/// Most Wang sets, colors or tiles one tileset may contribute (a hostile file must not blow up).
const MAX_WANG_ITEMS: usize = 65_536;

/// Slot indices of the four corners in a `wangid`: top-left, top-right, bottom-right,
/// bottom-left (the bit order of `WangCorner`).
const CORNER_SLOTS: [usize; 4] = [7, 1, 3, 5];
/// Slot indices of the four edges: top, right, bottom, left (the bit order of `Edge16`).
const EDGE_SLOTS: [usize; 4] = [0, 2, 4, 6];
/// All eight slots clockwise from the top (the bit order of `Blob47`).
const ALL_SLOTS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// The mask of color `color` among `slots` of `wangid`, or `None` when the tile is unusable for
/// that terrain (another non-zero color in a read slot, or no slot of `color`).
fn mask_for(wangid: &[u8; 8], slots: &[usize], color: u8) -> Option<u32> {
    let mut mask = 0u32;
    for (bit, &slot) in slots.iter().enumerate() {
        match wangid[slot] {
            0 => {}
            c if c == color => mask |= 1 << bit,
            _ => return None,
        }
    }
    (mask != 0).then_some(mask)
}

/// The tile for `mask` among `table` (`mask -> tile`), else the nearest covered mask's tile.
fn nearest(table: &[Option<u32>], mask: usize) -> Option<(u32, bool)> {
    if let Some(Some(tile)) = table.get(mask) {
        return Some((*tile, false));
    }
    table
        .iter()
        .enumerate()
        .filter_map(|(m, t)| t.map(|t| ((m ^ mask).count_ones(), m, t)))
        .min()
        .map(|(_, _, tile)| (tile, true))
}

/// Builds the terrains of one Wang set.
fn terrains_of(data: &WangData, warnings: &mut Vec<String>) -> Vec<Terrain> {
    let mut out = Vec::new();
    for (color_index, color_name) in data.colors.iter().enumerate() {
        let color = u8::try_from(color_index + 1).unwrap_or(u8::MAX);
        let (slots, size): (&[usize], usize) = match data.kind {
            WangKind::Corner => (&CORNER_SLOTS[..], 16),
            WangKind::Edge => (&EDGE_SLOTS[..], 16),
            WangKind::Mixed => (&ALL_SLOTS[..], 256),
        };
        let mut table: Vec<Option<u32>> = vec![None; size];
        for (tile, wangid) in &data.tiles {
            if let Some(mask) = mask_for(wangid, slots, color) {
                let slot = &mut table[mask as usize];
                // The lowest tile id wins (the tiles arrive sorted).
                if slot.is_none() {
                    *slot = Some(*tile);
                }
            }
        }
        let label = format!("wang set `{}` color `{color_name}`", data.name);
        let masks: Vec<usize> = match data.kind {
            WangKind::Mixed => blob47_masks().iter().map(|&m| usize::from(m)).collect(),
            WangKind::Corner | WangKind::Edge => (0..16).collect(),
        };
        let mut tiles = Vec::with_capacity(masks.len());
        let mut approximated = 0usize;
        for &mask in &masks {
            match nearest(&table, mask) {
                Some((tile, was_approx)) => {
                    approximated += usize::from(was_approx);
                    tiles.push(tile);
                }
                None => break,
            }
        }
        if tiles.len() != masks.len() {
            warnings.push(format!("{label}: no tile uses only this color; skipped"));
            continue;
        }
        if approximated > 0 {
            warnings.push(format!(
                "{label}: {approximated} of {} masks have no tile; the nearest mask's tile is used",
                masks.len()
            ));
        }
        let layout = match data.kind {
            WangKind::Corner => Layout::WangCorner(tiles),
            WangKind::Edge => Layout::Edge16(tiles),
            WangKind::Mixed => Layout::Blob47(tiles),
        };
        out.push(Terrain {
            name: color_name.clone(),
            layout,
            fallback: None,
            members: Vec::new(),
            variants: std::collections::BTreeMap::new(),
        });
    }
    out
}

/// Turns the Wang sets of the tileset `tileset` into rules. Returns `None` when no terrain
/// survives. Terrain names are the color names, prefixed by the set name when two collide, and
/// numbered when a color has no name.
pub fn autotile_from_wang_sets(
    tileset: &str,
    sets: &[WangData],
    mut warnings: Vec<String>,
) -> Option<ImportedAutoTile> {
    let mut terrains: Vec<Terrain> = Vec::new();
    for data in sets.iter().take(MAX_WANG_ITEMS) {
        let mut named = data.clone();
        for (i, name) in named.colors.iter_mut().enumerate() {
            if name.is_empty() {
                *name = format!("{}_{}", data.name, i + 1);
            }
        }
        for mut terrain in terrains_of(&named, &mut warnings) {
            if terrains.iter().any(|t| t.name == terrain.name) {
                terrain.name = format!("{}/{}", data.name, terrain.name);
            }
            // Later duplicates after the prefix (same set and color name twice) get a number.
            let base = terrain.name.clone();
            let mut n = 2;
            while terrains.iter().any(|t| t.name == terrain.name) {
                terrain.name = format!("{base}_{n}");
                n += 1;
            }
            terrains.push(terrain);
        }
    }
    if terrains.is_empty() {
        return None;
    }
    // Member sets must be disjoint: a tile reused by two terrains (a nearest-mask fill cannot
    // cause it, but two colors can share a fallback only through mixed tiles, which are skipped).
    let set = AutoTileSet {
        version: AUTOTILE_FORMAT_VERSION,
        tileset: tileset.to_string(),
        seed: 0,
        border: Border::Empty,
        terrains,
    };
    match set.validate() {
        Ok(()) => Some(ImportedAutoTile { set, warnings }),
        Err(e) => {
            warnings.push(format!(
                "tileset `{tileset}`: the Wang sets do not form valid rules: {e}"
            ));
            None
        }
    }
}

// --- XML (.tsx, .tmx) through the `tiled` crate ---------------------------------------------

fn kind_of_tmx(kind: tiled::WangSetType) -> WangKind {
    match kind {
        tiled::WangSetType::Corner => WangKind::Corner,
        tiled::WangSetType::Edge => WangKind::Edge,
        tiled::WangSetType::Mixed => WangKind::Mixed,
    }
}

/// The Wang data of a `tiled` crate tileset.
fn wang_data_of_tileset(set: &tiled::Tileset) -> Vec<WangData> {
    set.wang_sets
        .iter()
        .take(MAX_WANG_ITEMS)
        .map(|w| {
            let mut tiles: Vec<(u32, [u8; 8])> = w
                .wang_tiles
                .iter()
                .take(MAX_WANG_ITEMS)
                .map(|(id, t)| (*id, t.wang_id.0))
                .collect();
            tiles.sort_unstable();
            WangData {
                name: w.name.clone(),
                kind: kind_of_tmx(w.wang_set_type),
                colors: w.wang_colors.iter().map(|c| c.name.clone()).collect(),
                tiles,
            }
        })
        .collect()
}

// --- JSON (.tsj, .tmj) ----------------------------------------------------------------------

#[derive(Deserialize)]
struct JWangColor {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct JWangTile {
    tileid: u32,
    wangid: Vec<i64>,
}

#[derive(Deserialize)]
struct JWangSet {
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    colors: Vec<JWangColor>,
    #[serde(default)]
    wangtiles: Vec<JWangTile>,
}

#[derive(Deserialize)]
struct JLegacyTerrain {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct JTerrainTile {
    id: u32,
    /// Corner terrain indices: top-left, top-right, bottom-left, bottom-right (-1 = none).
    #[serde(default)]
    terrain: Vec<i64>,
}

#[derive(Deserialize)]
struct JWangTileset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    wangsets: Vec<JWangSet>,
    #[serde(default)]
    terrains: Vec<JLegacyTerrain>,
    #[serde(default)]
    tiles: Vec<JTerrainTile>,
}

/// The corner Wang set equal to Tiled 1.0 terrains (JSON `terrains` or XML `<terraintypes>`): terrain
/// `i` is Wang color `i + 1`, and a tile's `[top-left, top-right, bottom-left, bottom-right]`
/// indices are the four corner slots of its Wang id.
fn legacy_terrains_to_wang(names: Vec<String>, tiles: &[(u32, [i64; 4])]) -> WangData {
    // A corner past any terrain (`-1`, garbage, `i64::MAX`) is no color; `checked_add` because
    // the scanners parse corners as `i64` and `i64::MAX + 1` overflows (a panic with overflow checks).
    let color = |i: i64| i.checked_add(1).and_then(|c| u8::try_from(c).ok()).unwrap_or(0);
    let mut out: Vec<(u32, [u8; 8])> = tiles
        .iter()
        .take(MAX_WANG_ITEMS)
        .map(|&(id, [tl, tr, bl, br])| {
            // Slots: top, TR, right, BR, bottom, BL, left, TL.
            (id, [0, color(tr), 0, color(br), 0, color(bl), 0, color(tl)])
        })
        .collect();
    out.sort_unstable();
    WangData {
        name: "terrains".to_string(),
        kind: WangKind::Corner,
        colors: names,
        tiles: out,
    }
}

fn wang_data_of_json(set: &JWangTileset, warnings: &mut Vec<String>) -> Vec<WangData> {
    let mut out = Vec::new();
    for w in set.wangsets.iter().take(MAX_WANG_ITEMS) {
        let kind = match w.kind.as_str() {
            "edge" => WangKind::Edge,
            "mixed" => WangKind::Mixed,
            _ => WangKind::Corner,
        };
        let mut tiles = Vec::new();
        for t in w.wangtiles.iter().take(MAX_WANG_ITEMS) {
            if t.wangid.len() != 8 {
                warnings.push(format!(
                    "wang set `{}` tile {}: wangid has {} values, expected 8; skipped",
                    w.name,
                    t.tileid,
                    t.wangid.len()
                ));
                continue;
            }
            let mut id = [0u8; 8];
            for (slot, v) in id.iter_mut().zip(&t.wangid) {
                *slot = u8::try_from((*v).clamp(0, 255)).unwrap_or(0);
            }
            tiles.push((t.tileid, id));
        }
        tiles.sort_unstable();
        out.push(WangData {
            name: w.name.clone(),
            kind,
            colors: w.colors.iter().map(|c| c.name.clone()).collect(),
            tiles,
        });
    }
    // Tiled 1.0's terrains: per tile [top-left, top-right, bottom-left, bottom-right].
    if !set.terrains.is_empty() {
        let names: Vec<String> = set.terrains.iter().map(|t| t.name.clone()).collect();
        let tiles: Vec<(u32, [i64; 4])> = set
            .tiles
            .iter()
            .filter(|t| t.terrain.len() == 4)
            .map(|t| (t.id, [t.terrain[0], t.terrain[1], t.terrain[2], t.terrain[3]]))
            .collect();
        out.push(legacy_terrains_to_wang(names, &tiles));
    }
    out
}

fn json_err(path: &Path, reason: impl ToString) -> TiledImportError {
    TiledImportError::Json {
        path: path.display().to_string(),
        reason: reason.to_string(),
    }
}

fn read_limited(path: &Path) -> Result<String, std::io::Error> {
    let len = std::fs::metadata(path)?.len();
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{len} bytes is over the 64 MiB limit"),
        ));
    }
    std::fs::read_to_string(path)
}

/// Imports the Wang sets and terrains of a Tiled tileset (`.tsx`, `.tsj`, `.json`) or of every
/// tileset a map (`.tmx`, `.tmj`) uses into `.autotile.ron` rules: one [`ImportedAutoTile`] per
/// tileset that has usable terrains. External tilesets of a `.tmj` resolve relative to the map.
pub fn import_wang_autotiles(path: &Path) -> Result<Vec<ImportedAutoTile>, TiledImportError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut out = Vec::new();
    match ext.as_str() {
        "tsx" => {
            let set = crate::tiled_import::load_tsx(path)?;
            collect_tmx(&set, xml_legacy_of_file(path)?, &mut out);
        }
        "tmx" => {
            let map = crate::tiled_import::load_tmx(path)?;
            let dir = path.parent().unwrap_or(Path::new(""));
            let elements =
                crate::tiled_import::scan_xml_tilesets(&read_limited(path).map_err(|e| json_err(path, e))?);
            // The `tiled` crate lists tilesets in document order, as the scan does.
            let aligned = elements.len() == map.tilesets().len();
            for (i, set) in map.tilesets().iter().enumerate() {
                let legacy = match elements.get(i).filter(|_| aligned) {
                    Some(e) => match (&e.terrains, &e.source) {
                        (Some(t), _) => Some(t.clone()),
                        (None, Some(source)) => {
                            let resolved = crate::cook::join_portable(dir, source)
                                .map_err(|reason| json_err(path, reason))?;
                            xml_legacy_of_file(&resolved)?
                        }
                        (None, None) => None,
                    },
                    None => None,
                };
                collect_tmx(set, legacy, &mut out);
            }
        }
        "tsj" | "json" => {
            let text = read_limited(path).map_err(|e| json_err(path, e))?;
            let set: JWangTileset = serde_json::from_str(&text).map_err(|e| json_err(path, e))?;
            collect_json(&set, &set.name, &mut out);
        }
        "tmj" => {
            let text = read_limited(path).map_err(|e| json_err(path, e))?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| json_err(path, e))?;
            let dir = path.parent().unwrap_or(Path::new(""));
            let entries = value
                .get("tilesets")
                .and_then(serde_json::Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for entry in entries.iter().take(MAX_WANG_ITEMS) {
                match entry.get("source").and_then(serde_json::Value::as_str) {
                    Some(source) => {
                        let ext = Path::new(source)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let resolved = crate::cook::join_portable(dir, source)
                            .map_err(|reason| json_err(path, reason))?;
                        if ext == "tsx" {
                            let set = crate::tiled_import::load_tsx(&resolved)?;
                            collect_tmx(&set, xml_legacy_of_file(&resolved)?, &mut out);
                        } else {
                            let text = read_limited(&resolved).map_err(|e| json_err(&resolved, e))?;
                            let set: JWangTileset =
                                serde_json::from_str(&text).map_err(|e| json_err(&resolved, e))?;
                            collect_json(&set, &set.name, &mut out);
                        }
                    }
                    None => {
                        let set: JWangTileset =
                            serde_json::from_value(entry.clone()).map_err(|e| json_err(path, e))?;
                        collect_json(&set, &set.name, &mut out);
                    }
                }
            }
        }
        other => {
            return Err(TiledImportError::Unsupported {
                path: path.display().to_string(),
                what: format!("`.{other}` (use .tsx, .tsj, .tmx or .tmj)"),
            });
        }
    }
    Ok(out)
}

/// The Tiled 1.0 `<terraintypes>` of a `.tsx` file, if it has any.
fn xml_legacy_of_file(path: &Path) -> Result<Option<LegacyTerrains>, TiledImportError> {
    let text = read_limited(path).map_err(|e| json_err(path, e))?;
    Ok(crate::tiled_import::scan_xml_tilesets(&text)
        .into_iter()
        .find_map(|t| t.terrains))
}

fn collect_tmx(set: &tiled::Tileset, legacy: Option<LegacyTerrains>, out: &mut Vec<ImportedAutoTile>) {
    let mut data = wang_data_of_tileset(set);
    if let Some(legacy) = legacy.filter(|l| !l.names.is_empty() && !l.tiles.is_empty()) {
        data.push(legacy_terrains_to_wang(legacy.names, &legacy.tiles));
    }
    if data.is_empty() {
        return;
    }
    if let Some(imported) = autotile_from_wang_sets(&set.name, &data, Vec::new()) {
        out.push(imported);
    }
}

fn collect_json(set: &JWangTileset, name: &str, out: &mut Vec<ImportedAutoTile>) {
    let mut warnings = Vec::new();
    let data = wang_data_of_json(set, &mut warnings);
    if data.is_empty() {
        return;
    }
    if let Some(imported) = autotile_from_wang_sets(name, &data, warnings) {
        out.push(imported);
    }
}

/// `.autotile.ron` text for each tileset with usable Wang sets in `path` (see
/// [`import_wang_autotiles`]), with the tileset name and the import warnings.
pub fn wang_to_autotile_ron(path: &Path) -> Result<Vec<(String, String, Vec<String>)>, TiledImportError> {
    import_wang_autotiles(path)?
        .into_iter()
        .map(|i| {
            let text = i.set.to_ron().map_err(|e| TiledImportError::Unsupported {
                path: path.display().to_string(),
                what: e.to_string(),
            })?;
            Ok((i.set.tileset.clone(), text, i.warnings))
        })
        .collect()
}

#[cfg(test)]
mod tests;
