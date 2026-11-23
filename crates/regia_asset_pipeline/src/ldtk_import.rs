// SPDX-License-Identifier: MPL-2.0
//! LDtk import (`.ldtk` with external `.ldtkl` levels, `docs/specs/22-tier2.md` § T2.14,
//! ADR-3358): the documented JSON format (<https://ldtk.io/json/>, JSON_SCHEMA 1.5.3, cited in
//! `docs/provenance.md`) into the frozen `.tilemap.ron` v2 structure. Nothing here is derived
//! from any engine's importer; the mapping is implemented from the public schema.
//!
//! The whole world imports as **one** [`TileMapAsset`]: each level's tiles are written into the
//! merged cell grid at their world pixel, and the layer offset stays zero so the tile batch does
//! not add that position a second time. Tile layers (IntGrid with auto-layer rules, AutoLayer,
//! Tiles) become
//! tile layers with the LDtk flip bits mapped to the Tiled GID flip bits, IntGrid values become a
//! Point-object layer (`<layer>_intgrid`) with the value as a property, and entity layers become
//! object layers whose objects carry the entity fields as properties. Each level is also written
//! as a `level/<identifier>` sub-asset map (level-local coordinates) by the cook. Per-tile
//! opacity, backgrounds images, parallax, level depths other than 0 and multi-world projects are
//! skipped with a warning (listed in the book page).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use regia_2d::tilemap::{
    FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, MapObject, ObjectLayer, ObjectShape, PropertyValue,
    TILEMAP_FORMAT_VERSION, TileLayer, TileMapAsset, TilesetDef,
};
use serde::Deserialize;
use serde_json::Value;

/// Hard caps on untrusted input (book/agents/import-report.md lists them).
pub mod limits {
    /// Largest `.ldtk` / `.ldtkl` file read (bytes).
    pub const MAX_FILE_BYTES: u64 = 64 << 20;
    /// Most levels in a project.
    pub const MAX_LEVELS: usize = 1_024;
    /// Most layers (level × layer total).
    pub const MAX_LAYERS: usize = 16_384;
    /// Most tiles (grid + auto-layer, total).
    pub const MAX_TILES: usize = 1 << 24;
    /// Most entities (total).
    pub const MAX_ENTITIES: usize = 1 << 20;
    /// Most non-zero IntGrid cells (total).
    pub const MAX_INTGRID_CELLS: usize = 1 << 22;
    /// Largest merged-map edge in tiles.
    pub const MAX_MAP_EDGE: u32 = 1 << 16;
    /// Largest merged-map area in tiles (`width × height`).
    pub const MAX_MAP_TILES: u64 = 1 << 26;
    /// Largest grid size in pixels.
    pub const MAX_GRID_SIZE: u32 = 4_096;
    /// Longest identifier (bytes).
    pub const MAX_IDENTIFIER: usize = 1_024;
    /// Most tile-grid cells allocated in total: every kept tile layer (`IntGrid`, `AutoLayer`,
    /// `Tiles`) of every level costs one merged-map grid plus one level grid.
    pub const MAX_GRID_CELLS: u64 = 1 << 30;
}

/// LDtk import errors; [`LdtkError::code`] is the stable, documented code
/// (`book/agents/import-report.md`). Every message starts with its code, so a cook log greps it.
#[derive(Debug, thiserror::Error)]
pub enum LdtkError {
    /// The JSON does not parse into the expected structures.
    #[error("ldtk_json: {0}")]
    Json(String),
    /// The file's `jsonVersion` major is not 1.
    #[error("ldtk_unsupported_version: {0}")]
    UnsupportedVersion(String),
    /// An external level file is missing, leaves the source's folder, or does not parse.
    #[error("ldtk_external: {0}")]
    External(String),
    /// A hard input limit was exceeded.
    #[error("ldtk_limits: {0}")]
    Limits(&'static str),
    /// The file's structures contradict each other.
    #[error("ldtk_structure: {0}")]
    Structure(String),
}

impl LdtkError {
    /// The stable lowercase code of this error (documented in the book page).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Json(_) => "ldtk_json",
            Self::UnsupportedVersion(_) => "ldtk_unsupported_version",
            Self::External(_) => "ldtk_external",
            Self::Limits(_) => "ldtk_limits",
            Self::Structure(_) => "ldtk_structure",
        }
    }
}

// -- JSON subset (LDtk 1.5 MINIMAL_JSON_SCHEMA; unknown fields are ignored) ---------------------

#[derive(Debug, Clone, Deserialize)]
struct LdtkRoot {
    #[serde(rename = "jsonVersion")]
    json_version: Option<String>,
    #[serde(rename = "externalLevels")]
    external_levels: bool,
    #[serde(default)]
    worlds: Vec<LdtkWorld>,
    #[serde(default)]
    levels: Vec<LdtkLevel>,
    #[serde(rename = "bgColor", default)]
    bg_color: Option<String>,
    /// `Free` and `GridVania` place levels at `worldX`/`worldY`, which may be negative.
    /// `LinearHorizontal` and `LinearVertical` store `-1` and are laid out in a row.
    #[serde(rename = "worldLayout", default)]
    world_layout: Option<String>,
    #[serde(default)]
    defs: LdtkDefs,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkWorld {
    identifier: Option<String>,
    #[serde(default)]
    levels: Vec<LdtkLevel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LdtkDefs {
    #[serde(default)]
    tilesets: Vec<LdtkTileset>,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkTileset {
    uid: i64,
    identifier: Option<String>,
    #[serde(rename = "relPath")]
    rel_path: Option<String>,
    #[serde(rename = "pxWid")]
    px_wid: i64,
    #[serde(rename = "pxHei")]
    px_hei: i64,
    #[serde(rename = "tileGridSize")]
    tile_grid_size: i64,
    #[serde(rename = "__cWid")]
    c_wid: i64,
    #[serde(rename = "__cHei")]
    c_hei: i64,
    #[serde(default)]
    spacing: i64,
    #[serde(default)]
    padding: i64,
    #[serde(rename = "embedAtlas")]
    embed_atlas: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkLevel {
    identifier: Option<String>,
    uid: i64,
    #[serde(rename = "worldX", default)]
    world_x: i64,
    #[serde(rename = "worldY", default)]
    world_y: i64,
    #[serde(rename = "worldDepth", default)]
    world_depth: i64,
    #[serde(rename = "pxWid")]
    px_wid: i64,
    #[serde(rename = "pxHei")]
    px_hei: i64,
    #[serde(rename = "externalRelPath")]
    external_rel_path: Option<String>,
    #[serde(rename = "layerInstances")]
    layer_instances: Option<Vec<LdtkLayer>>,
    #[serde(rename = "fieldInstances", default)]
    field_instances: Vec<LdtkField>,
    #[serde(rename = "__bgColor", default)]
    bg_color: Option<String>,
    #[serde(rename = "bgRelPath")]
    bg_rel_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkLayer {
    #[serde(rename = "__type")]
    layer_type: String,
    #[serde(rename = "__identifier")]
    identifier: Option<String>,
    #[serde(rename = "__gridSize")]
    grid_size: i64,
    #[serde(rename = "__cWid")]
    c_wid: i64,
    #[serde(rename = "__cHei")]
    c_hei: i64,
    #[serde(rename = "__opacity", default = "default_one")]
    opacity: f64,
    #[serde(default = "default_true")]
    visible: bool,
    #[serde(rename = "__pxTotalOffsetX", default)]
    total_offset_x: i64,
    #[serde(rename = "__pxTotalOffsetY", default)]
    total_offset_y: i64,
    #[serde(rename = "__tilesetDefUid")]
    tileset_uid: Option<i64>,
    #[serde(rename = "gridTiles", default)]
    grid_tiles: Vec<LdtkTile>,
    #[serde(rename = "autoLayerTiles", default)]
    auto_layer_tiles: Vec<LdtkTile>,
    #[serde(rename = "intGridCsv", default)]
    int_grid_csv: Vec<i64>,
    #[serde(rename = "entityInstances", default)]
    entity_instances: Vec<LdtkEntity>,
}

/// One tile as stored (`t` is the local id; `f` the flip bits); `src` and per-tile opacity `a`
/// are accepted but not representable in the map (opacity warns once per layer).
#[derive(Debug, Clone, Deserialize)]
struct LdtkTile {
    /// Local tile id in its tileset.
    t: i64,
    /// Flip bits: bit 0 = X, bit 1 = Y.
    f: i64,
    /// Pixel position in the level (+Y down).
    px: [i64; 2],
    /// Opacity 0..1.
    #[serde(default = "default_one")]
    a: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkEntity {
    iid: Option<String>,
    #[serde(rename = "__identifier")]
    identifier: Option<String>,
    /// Pivot (0..1); the entity `px` is the pivot point, so the top-left is
    /// `px − pivot × size`.
    #[serde(rename = "__pivot", default)]
    pivot: [f64; 2],
    /// Pixel position of the pivot in the level (+Y down).
    px: [f64; 2],
    width: f64,
    height: f64,
    #[serde(rename = "__worldX")]
    world_x: Option<i64>,
    #[serde(rename = "__worldY")]
    world_y: Option<i64>,
    #[serde(rename = "__tile")]
    tile: Option<LdtkTilesetRect>,
    #[serde(rename = "fieldInstances", default)]
    field_instances: Vec<LdtkField>,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkTilesetRect {
    #[serde(rename = "tilesetUid")]
    tileset_uid: i64,
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct LdtkField {
    #[serde(rename = "__identifier")]
    identifier: Option<String>,
    #[serde(rename = "__type")]
    field_type: Option<String>,
    #[serde(rename = "__value")]
    value: Option<Value>,
}

fn default_true() -> bool {
    true
}
fn default_one() -> f64 {
    1.0
}

// -- Import output ------------------------------------------------------------------------------

/// One imported level (for the report and the sub-assets).
#[derive(Clone, Debug)]
pub struct LdtkLevelInfo {
    /// Level identifier.
    pub identifier: String,
    /// World position in pixels (before the merged map's origin shift).
    pub world_x: i64,
    /// World position in pixels.
    pub world_y: i64,
    /// Size in pixels.
    pub size_px: [u32; 2],
    /// Layer names in bottom-first order.
    pub layers: Vec<String>,
    /// Entity count.
    pub entities: usize,
}

/// The imported world: the merged map, the per-level maps and the report data.
#[derive(Clone, Debug)]
pub struct ImportedLdtk {
    /// The whole world as one map (levels merged; per-level positions are layer offsets).
    pub merged: TileMapAsset,
    /// One level-local map per level, in level order (the cook writes these as sub-assets).
    pub levels: Vec<TileMapAsset>,
    /// Level summaries for the report.
    pub level_infos: Vec<LdtkLevelInfo>,
    /// Tileset names with their assigned first GID (sorted by first GID).
    pub tilesets: Vec<(String, u32, u32)>,
    /// Files read besides the `.ldtk` itself (external levels); the cook keys on their bytes.
    pub side_files: Vec<std::path::PathBuf>,
    /// Skipped features and recoverable oddities, one line each.
    pub warnings: Vec<String>,
}

/// Checks an identifier length and copies it.
fn identifier_of(value: Option<&String>, what: &'static str) -> Result<String, LdtkError> {
    let id = value.cloned().unwrap_or_default();
    if id.len() > limits::MAX_IDENTIFIER {
        return Err(LdtkError::Limits(what));
    }
    Ok(id)
}

/// Converts an LDtk `"#rrggbb"` colour to the map's `"#AARRGGBB"` (opaque).
fn map_color(ldtk: &str) -> Option<String> {
    let hex = ldtk.strip_prefix('#')?;
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#FF{}", hex.to_ascii_uppercase()))
    } else {
        None
    }
}

/// A map-unique, deterministic `u32` object id for [`MapObject::id`], derived from an LDtk iid (a
/// hex string) and probed past the ids already handed out for this map (it can shift by one
/// when an earlier object in file order collides with it).
///
/// The id is the first 4 bytes of a blake3 hash of the iid. A 32-bit hash of distinct strings
/// collides on large maps (the birthday bound puts a collision at ≈ 12 % for 32 k ids), so the
/// candidate is probed against `used` — every id already handed out for this map — and bumped
/// by one until it is free. The ids stay deterministic and collision-free, and the probe also
/// guards the entity iid hashes.
fn object_id(iid: Option<&str>, index: usize, used: &mut HashSet<u32>) -> u32 {
    let candidate = if let Some(iid) = iid {
        let hash = blake3::hash(iid.as_bytes());
        let bytes: &[u8; 32] = hash.as_bytes();
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        // A file without iids (hostile or very old): keep ids distinct and deterministic.
        u32::try_from(index).unwrap_or(u32::MAX)
    };
    probe_object_id(candidate, used)
}

/// The first id at or after `candidate` that `used` does not hold yet, recorded in `used`.
fn probe_object_id(candidate: u32, used: &mut HashSet<u32>) -> u32 {
    let mut id = candidate;
    while !used.insert(id) {
        id = id.wrapping_add(1);
    }
    id
}

/// Converts one entity/level field to a [`PropertyValue`]; arrays become their compact JSON
/// text. Returns `None` for a field with no value.
fn field_property(field: &LdtkField) -> Option<(String, PropertyValue)> {
    let name = field.identifier.clone()?;
    let value = field.value.clone().unwrap_or(Value::Null);
    let typed = field.field_type.as_deref().unwrap_or("");
    let property = match value {
        Value::Bool(b) => PropertyValue::Bool(b),
        Value::Number(n) => {
            if typed.starts_with("Float") {
                PropertyValue::Float(n.as_f64().unwrap_or(f64::NAN))
            } else if let Some(i) = n.as_i64() {
                PropertyValue::Int(i)
            } else {
                PropertyValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => {
            if typed.starts_with("Color") {
                PropertyValue::Color(map_color(&s).unwrap_or_else(|| s.chars().take(16).collect()))
            } else if typed.starts_with("FilePath") {
                PropertyValue::File(s)
            } else {
                PropertyValue::String(s)
            }
        }
        Value::Object(_) | Value::Array(_) => {
            PropertyValue::String(serde_json::to_string(&value).unwrap_or_default())
        }
        Value::Null => PropertyValue::String(String::new()),
    };
    Some((name, property))
}

/// The grid size a layer must use to join the merged map: the first tile layer's grid decides.
struct WorldGrid {
    size: i64,
    set: bool,
}

impl WorldGrid {
    fn adopt(&mut self, layer_grid: i64, layer: &str, warnings: &mut Vec<String>) -> bool {
        if !(1..=i64::from(limits::MAX_GRID_SIZE)).contains(&layer_grid) {
            return false;
        }
        if !self.set {
            self.size = layer_grid;
            self.set = true;
            true
        } else if self.size == layer_grid {
            true
        } else {
            warnings.push(format!(
                "layer `{layer}` uses a {} px grid (the map uses {} px) and was skipped",
                layer_grid, self.size
            ));
            false
        }
    }
}

/// Reads an external level: a regular file only (a FIFO or device is refused before it is
/// opened, so a cook never blocks on it), and at most one byte past [`limits::MAX_FILE_BYTES`] (the
/// caller turns a longer read into `ldtk_limits` without loading the rest).
fn read_external_level(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(limits::MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)?;
    Ok(data)
}

/// A tileset with its assigned first GID.
struct TilesetGid {
    uid: i64,
    def: TilesetDef,
    grid_size: i64,
    columns: i64,
}

/// Imports an LDtk project (`bytes`) whose files live under `dir` (external `.ldtkl` levels and
/// tileset images resolve against it). Every count and size is bounded by [`limits`]; failures
/// carry a documented code ([`LdtkError::code`]).
pub fn import_ldtk(bytes: &[u8], dir: &Path) -> Result<ImportedLdtk, LdtkError> {
    if bytes.len() as u64 > limits::MAX_FILE_BYTES {
        return Err(LdtkError::Limits("file is larger than 64 MiB"));
    }
    let root: LdtkRoot = serde_json::from_slice(bytes)
        .map_err(|e| LdtkError::Json(format!("the project JSON does not parse: {e}")))?;
    if let Some(version) = &root.json_version {
        if !version.starts_with("1.") {
            return Err(LdtkError::UnsupportedVersion(version.clone()));
        }
    } else {
        return Err(LdtkError::UnsupportedVersion(String::new()));
    }
    let mut warnings: Vec<String> = Vec::new();

    // -- Levels (worlds merged; a single world is the normal case) -------------
    if root.worlds.len() > 1 {
        let names: Vec<String> = root
            .worlds
            .iter()
            .map(|w| w.identifier.clone().unwrap_or_default())
            .collect();
        warnings.push(format!(
            "the project has {} worlds ({}) and they were merged into one map",
            root.worlds.len(),
            names.join(", ")
        ));
    }
    let mut levels: Vec<LdtkLevel> = Vec::new();
    if root.worlds.is_empty() {
        levels = root.levels;
    } else {
        for world in &root.worlds {
            levels.extend(world.levels.iter().cloned());
        }
    }
    if levels.len() > limits::MAX_LEVELS {
        return Err(LdtkError::Limits("more than 1024 levels"));
    }
    if levels.is_empty() {
        return Err(LdtkError::Structure("the project has no levels".into()));
    }

    // -- External levels --------------------------------------------------------
    let mut side_files: Vec<std::path::PathBuf> = Vec::new();
    let mut layer_total = 0usize;
    for level in &mut levels {
        match (&level.external_rel_path, &level.layer_instances) {
            (Some(rel), _) => {
                // The same bound as every other content reference (ADR-4922): no absolute path,
                // and `..` or a symlink may not leave the source's folder (or its project).
                let bound = crate::cook::reference_bound(dir).ok_or_else(|| {
                    LdtkError::External(format!("cannot resolve `{rel}` against {}", dir.display()))
                })?;
                let path = crate::cook::join_bounded(dir, rel, Some(&bound)).map_err(LdtkError::External)?;
                let data = read_external_level(&path)
                    .map_err(|e| LdtkError::External(format!("cannot read `{rel}`: {e}")))?;
                if data.len() as u64 > limits::MAX_FILE_BYTES {
                    return Err(LdtkError::Limits("an external level is larger than 64 MiB"));
                }
                let external: LdtkLevel = serde_json::from_slice(&data)
                    .map_err(|e| LdtkError::External(format!("`{rel}` does not parse: {e}")))?;
                side_files.push(path);
                level.layer_instances = external.layer_instances;
            }
            (None, None) if root.external_levels => {
                return Err(LdtkError::External(format!(
                    "externalLevels is set but level `{}` has no externalRelPath",
                    identifier_of(level.identifier.as_ref(), "identifier")?
                )));
            }
            _ => {}
        }
        layer_total += level.layer_instances.as_ref().map_or(0, Vec::len);
    }
    if layer_total > limits::MAX_LAYERS {
        return Err(LdtkError::Limits("more than 16384 layers in total"));
    }

    // -- World positions ---------------------------------------------------------
    // Linear layouts report worldX/worldY as -1. Free and GridVania coordinates are real,
    // including negatives, so only an unpositioned layout is placed in a row.
    let positioned = matches!(root.world_layout.as_deref(), Some("Free" | "GridVania"));
    let mut cursor_x = 0i64;
    for level in &mut levels {
        if level.px_wid <= 0 || level.px_hei <= 0 {
            return Err(LdtkError::Limits("a level has a non-positive pixel size"));
        }
        if !positioned && (level.world_x < 0 || level.world_y < 0) {
            level.world_x = cursor_x;
            level.world_y = 0;
            cursor_x = cursor_x
                .checked_add(level.px_wid)
                .ok_or(LdtkError::Limits("level positions overflow"))?;
            warnings.push(format!(
                "level `{}` had no world position and was placed in a row",
                identifier_of(level.identifier.as_ref(), "identifier")?
            ));
        }
        if level.world_depth != 0 {
            warnings.push(format!(
                "level `{}` has worldDepth {} (not 0); depths are not imported",
                identifier_of(level.identifier.as_ref(), "identifier")?,
                level.world_depth
            ));
        }
        if level.bg_rel_path.is_some() {
            warnings.push(format!(
                "level `{}` has a background image; it was not imported",
                identifier_of(level.identifier.as_ref(), "identifier")?
            ));
        }
    }
    let mut seen_ids = BTreeSet::new();
    for level in &levels {
        let id = identifier_of(level.identifier.as_ref(), "identifier")?;
        if !seen_ids.insert(id) {
            return Err(LdtkError::Structure(
                "two levels share an identifier; each level is its own sub-asset".into(),
            ));
        }
    }

    // -- Bounding box over all levels ---------------------------------------------
    let min_x = levels.iter().map(|l| l.world_x).min().unwrap_or(0);
    let min_y = levels.iter().map(|l| l.world_y).min().unwrap_or(0);
    let extent = |pos: i64, size: i64| {
        pos.checked_add(size)
            .ok_or(LdtkError::Limits("level positions overflow"))
    };
    let max_x = levels
        .iter()
        .map(|l| extent(l.world_x, l.px_wid))
        .try_fold(i64::MIN, |acc, end| Ok(acc.max(end?)))?;
    let max_y = levels
        .iter()
        .map(|l| extent(l.world_y, l.px_hei))
        .try_fold(i64::MIN, |acc, end| Ok(acc.max(end?)))?;

    // -- Tilesets ---------------------------------------------------------------------
    let mut tilesets: Vec<TilesetGid> = Vec::new();
    for ts in &root.defs.tilesets {
        if ts.embed_atlas.is_some() {
            warnings.push(format!(
                "tileset `{}` embeds the LDtk icon atlas and was skipped",
                ts.identifier.clone().unwrap_or_default()
            ));
            continue;
        }
        let Some(rel_path) = &ts.rel_path else {
            warnings.push(format!(
                "tileset `{}` has no image path and was skipped",
                ts.identifier.clone().unwrap_or_default()
            ));
            continue;
        };
        if ts.tile_grid_size <= 0 || ts.c_wid <= 0 || ts.c_hei <= 0 {
            return Err(LdtkError::Structure(format!(
                "tileset `{}` has a non-positive grid",
                ts.identifier.clone().unwrap_or_default()
            )));
        }
        // Both factors come from the JSON: an unchecked product overflows (a panic in debug, a
        // small wrapped tile count in release).
        let Some(count) = ts
            .c_wid
            .checked_mul(ts.c_hei)
            .filter(|c| (1..=i64::from(u32::MAX)).contains(c))
        else {
            return Err(LdtkError::Limits("a tileset holds more than u32::MAX tiles"));
        };
        tilesets.push(TilesetGid {
            uid: ts.uid,
            def: TilesetDef {
                name: identifier_of(ts.identifier.as_ref(), "tileset identifier")?,
                first_gid: 0, // assigned below, in UID order
                image: rel_path.clone(),
                image_width: u32::try_from(ts.px_wid).unwrap_or(u32::MAX),
                image_height: u32::try_from(ts.px_hei).unwrap_or(u32::MAX),
                tile_width: u32::try_from(ts.tile_grid_size).unwrap_or(u32::MAX),
                tile_height: u32::try_from(ts.tile_grid_size).unwrap_or(u32::MAX),
                columns: u32::try_from(ts.c_wid).unwrap_or(u32::MAX),
                tile_count: count as u32,
                spacing: u32::try_from(ts.spacing).unwrap_or(u32::MAX),
                margin: u32::try_from(ts.padding).unwrap_or(u32::MAX),
                tiles: Vec::new(),
            },
            grid_size: ts.tile_grid_size,
            columns: ts.c_wid,
        });
    }
    tilesets.sort_by_key(|ts| ts.uid);
    let mut next_gid = 1u32;
    for ts in &mut tilesets {
        ts.def.first_gid = next_gid;
        next_gid = next_gid
            .checked_add(ts.def.tile_count)
            .ok_or(LdtkError::Limits("the GID space overflowed"))?;
        // The top three bits are the Tiled flip flags. A gid in that range stores an unflipped
        // tile as flipped.
        if next_gid > FLIP_DIAGONAL_BIT {
            return Err(LdtkError::Limits("the GID space overlaps the flip bits"));
        }
    }
    let tileset_of = |uid: i64| tilesets.iter().find(|ts| ts.uid == uid);

    // -- The grid of the merged map ------------------------------------------------------
    let mut grid = WorldGrid { size: 0, set: false };
    let mut entity_count = 0usize;
    let mut tile_count = 0usize;
    let mut intgrid_count = 0usize;
    for level in &levels {
        for layer in level.layer_instances.as_deref().unwrap_or(&[]) {
            let name = layer.identifier.clone().unwrap_or_default();
            match layer.layer_type.as_str() {
                "IntGrid" | "AutoLayer" | "Tiles" => {
                    if !grid.adopt(layer.grid_size, &name, &mut warnings) {
                        continue;
                    }
                    tile_count += layer.grid_tiles.len() + layer.auto_layer_tiles.len();
                }
                "Entities" => entity_count += layer.entity_instances.len(),
                other => {
                    warnings.push(format!("layer `{name}` has type `{other}` and was skipped"));
                    continue;
                }
            }
            intgrid_count += layer.int_grid_csv.iter().filter(|&&v| v != 0).count();
        }
    }
    if tile_count > limits::MAX_TILES {
        return Err(LdtkError::Limits("more than 16 Mi tiles in total"));
    }
    if entity_count > limits::MAX_ENTITIES {
        return Err(LdtkError::Limits("more than 1 Mi entities in total"));
    }
    if intgrid_count > limits::MAX_INTGRID_CELLS {
        return Err(LdtkError::Limits("more than 4 Mi non-zero IntGrid cells"));
    }
    let grid_size = if grid.set { grid.size } else { 16 };
    let span = |end: i64, start: i64| {
        end.checked_sub(start)
            .ok_or(LdtkError::Limits("level positions overflow"))
    };
    let map_w = ceil_div(span(max_x, min_x)?, grid_size);
    let map_h = ceil_div(span(max_y, min_y)?, grid_size);
    if !(1..=i64::from(limits::MAX_MAP_EDGE)).contains(&map_w)
        || !(1..=i64::from(limits::MAX_MAP_EDGE)).contains(&map_h)
        || u64::try_from(map_w).unwrap_or(u64::MAX) * u64::try_from(map_h).unwrap_or(0)
            > limits::MAX_MAP_TILES
    {
        return Err(LdtkError::Limits(
            "the merged map is larger than 65536 tiles an edge or 64 Mi tiles",
        ));
    }
    let multi = levels.len() > 1;
    let layer_name = |level: &LdtkLevel, layer: &str| {
        if multi {
            format!(
                "{}/{}",
                identifier_of(level.identifier.as_ref(), "identifier").unwrap_or_default(),
                layer
            )
        } else {
            layer.to_string()
        }
    };

    // Every kept tile layer allocates a merged-map grid and a level grid: bound the total
    // before any grid exists (a few hundred tiny layers on a large map asked for 100+ GiB).
    let mut grid_cells: u64 = 0;
    for level in &levels {
        let level_cells = u64::try_from(ceil_div(level.px_wid, grid_size)).unwrap_or(0)
            * u64::try_from(ceil_div(level.px_hei, grid_size)).unwrap_or(0);
        let map_cells = u64::try_from(map_w).unwrap_or(0) * u64::try_from(map_h).unwrap_or(0);
        for layer in level.layer_instances.as_deref().unwrap_or(&[]) {
            if matches!(layer.layer_type.as_str(), "IntGrid" | "AutoLayer" | "Tiles")
                && layer.grid_size == grid_size
            {
                grid_cells = grid_cells.saturating_add(map_cells.saturating_add(level_cells));
            }
        }
    }
    if grid_cells > limits::MAX_GRID_CELLS {
        return Err(LdtkError::Limits(
            "the tile layers need more than 1 Gi grid cells (merged map plus level grids)",
        ));
    }

    // -- Layers -------------------------------------------------------------------------
    let mut merged_layers: Vec<MapLayer> = Vec::new();
    let mut level_maps: Vec<TileMapAsset> = Vec::new();
    let mut level_infos: Vec<LdtkLevelInfo> = Vec::new();
    // Every object id handed out for the merged map, for the object_id collision probe:
    // `MapObject::id` is documented unique across the whole map.
    let mut object_ids = HashSet::new();
    for level in &levels {
        let level_name = identifier_of(level.identifier.as_ref(), "identifier")?;
        let origin_x = level
            .world_x
            .checked_sub(min_x)
            .ok_or(LdtkError::Limits("level positions overflow"))?;
        let origin_y = level
            .world_y
            .checked_sub(min_y)
            .ok_or(LdtkError::Limits("level positions overflow"))?;
        let level_w = ceil_div(level.px_wid, grid_size);
        let level_h = ceil_div(level.px_hei, grid_size);
        let mut local_layers: Vec<MapLayer> = Vec::new();
        let mut local_names: Vec<String> = Vec::new();
        // LDtk lists layers top-most first; the map stores bottom first.
        let instances: Vec<&LdtkLayer> = level
            .layer_instances
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .rev()
            .collect();
        let mut entity_total = 0usize;
        for layer in instances {
            let name = identifier_of(layer.identifier.as_ref(), "layer identifier")?;
            let mut values_layers: Option<(MapLayer, MapLayer)> = None;
            // Cells already include the level origin and the layer's pixel offset. The tile
            // batch adds `offset` on top of the cell (`regia_2d::batch`), so a second copy here
            // draws the level twice as far from the origin as its entities.
            let common = (layer.visible, layer.opacity as f32, [0.0_f32, 0.0]);
            let tiles: Vec<&LdtkTile> = layer
                .grid_tiles
                .iter()
                .chain(layer.auto_layer_tiles.iter())
                .collect();
            match layer.layer_type.as_str() {
                "IntGrid" | "AutoLayer" | "Tiles" => {
                    if layer.grid_size != grid_size {
                        warnings.push(format!(
                            "layer `{name}` uses a {} px grid (the map uses {} px) and was skipped",
                            layer.grid_size, grid_size
                        ));
                        continue;
                    }
                    let mut data = vec![0u32; (map_w * map_h) as usize];
                    let mut local_data = vec![0u32; (level_w * level_h) as usize];
                    let mut skipped_tiles = 0usize;
                    let mut outside_tiles = 0usize;
                    for tile in &tiles {
                        // The tileset comes from the layer definition; a tile whose id fits no
                        // tileset of this layer's tileset cannot resolve.
                        let gid = match layer.tileset_uid.and_then(tileset_of) {
                            Some(set) if tile.t >= 0 && tile.t < i64::from(set.def.tile_count) => {
                                let mut gid = set.def.first_gid + u32::try_from(tile.t).unwrap_or(u32::MAX);
                                if tile.f & 1 != 0 {
                                    gid |= FLIP_X_BIT;
                                }
                                if tile.f & 2 != 0 {
                                    gid |= FLIP_Y_BIT;
                                }
                                Some(gid)
                            }
                            _ => None,
                        };
                        let Some(gid) = gid else {
                            skipped_tiles += 1;
                            continue;
                        };
                        let merged = (
                            checked_px(&[tile.px[0], origin_x, layer.total_offset_x]),
                            checked_px(&[tile.px[1], origin_y, layer.total_offset_y]),
                        );
                        // The level-local map keeps the layer offset in `TileLayer.offset`, so the
                        // cell is the tile's own pixel. Adding the offset here as well doubles it.
                        let local = (Some(tile.px[0]), Some(tile.px[1]));
                        // An overflowing sum is outside every finite map. Counting it as a skip
                        // avoids a panic on a hostile pixel.
                        match merged {
                            (Some(mx), Some(my)) => {
                                if !put_tile(&mut data, map_w, map_h, grid_size, mx, my, gid) {
                                    outside_tiles += 1;
                                }
                            }
                            _ => outside_tiles += 1,
                        }
                        match local {
                            (Some(lx), Some(ly)) => {
                                if !put_tile(&mut local_data, level_w, level_h, grid_size, lx, ly, gid) {
                                    outside_tiles += 1;
                                }
                            }
                            _ => outside_tiles += 1,
                        }
                    }
                    if skipped_tiles > 0 {
                        warnings.push(format!(
                            "layer `{name}`: {skipped_tiles} tiles did not resolve to a tileset and were skipped"
                        ));
                    }
                    if outside_tiles > 0 {
                        warnings.push(format!(
                            "layer `{name}`: {outside_tiles} tiles landed outside the map and were skipped"
                        ));
                    }
                    if tiles.iter().any(|t| (t.a - 1.0).abs() >= f64::EPSILON) {
                        warnings.push(format!(
                            "layer `{name}`: per-tile opacity is not representable and was dropped"
                        ));
                    }
                    if layer.layer_type == "IntGrid" {
                        // The values of an IntGrid layer: one Point object per non-zero cell.
                        let mut objects = Vec::new();
                        let mut local_objects = Vec::new();
                        for (index, &value) in layer.int_grid_csv.iter().enumerate() {
                            if value == 0 {
                                continue;
                            }
                            // Keyed by level, layer and cell, so ids stay distinct across the
                            // levels and layers of the merged map (an index plus a shifted level
                            // uid collided between neighbours). The 32-bit hash can still
                            // collide on large maps, so object_id probes the map's id set.
                            let key = format!("intgrid:{}:{name}:{index}", level.uid);
                            let id = object_id(Some(&key), index, &mut object_ids);
                            let cx = (index as i64) % layer.c_wid.max(1);
                            let cy = (index as i64) / layer.c_wid.max(1);
                            if cy >= layer.c_hei {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            }
                            let half = grid_size / 2;
                            let Some(step_x) = cx.checked_mul(grid_size).and_then(|v| v.checked_add(half))
                            else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let Some(step_y) = cy.checked_mul(grid_size).and_then(|v| v.checked_add(half))
                            else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let Some(px) = checked_px(&[origin_x, layer.total_offset_x, step_x]) else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let Some(py) = checked_px(&[origin_y, layer.total_offset_y, step_y]) else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let Some(local_px) = layer.total_offset_x.checked_add(step_x) else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let Some(local_py) = layer.total_offset_y.checked_add(step_y) else {
                                warnings.push(format!(
                                    "layer `{name}`: an IntGrid value sits outside the layer grid and was skipped"
                                ));
                                continue;
                            };
                            let mut properties = BTreeMap::new();
                            properties.insert("value".to_string(), PropertyValue::Int(value));
                            objects.push(MapObject {
                                id,
                                name: String::new(),
                                class: "intgrid".to_string(),
                                position: [px as f32, py as f32],
                                size: [0.0, 0.0],
                                rotation: 0.0,
                                shape: ObjectShape::Point,
                                properties,
                            });
                            local_objects.push(MapObject {
                                id,
                                name: String::new(),
                                class: "intgrid".to_string(),
                                position: [local_px as f32, local_py as f32],
                                size: [0.0, 0.0],
                                rotation: 0.0,
                                shape: ObjectShape::Point,
                                properties: {
                                    let mut p = BTreeMap::new();
                                    p.insert("value".to_string(), PropertyValue::Int(value));
                                    p
                                },
                            });
                        }
                        // Pushed after the tile layer (the values ride on top of it).
                        values_layers = (!objects.is_empty()).then(|| {
                            let values_name = format!("{name}_intgrid");
                            (
                                MapLayer::Objects(ObjectLayer {
                                    name: layer_name(level, &values_name),
                                    visible: common.0,
                                    objects,
                                    properties: BTreeMap::new(),
                                }),
                                MapLayer::Objects(ObjectLayer {
                                    name: values_name,
                                    visible: common.0,
                                    objects: local_objects,
                                    properties: BTreeMap::new(),
                                }),
                            )
                        });
                    }
                    if !tiles.is_empty() {
                        let tile_layer = TileLayer {
                            name: layer_name(level, &name),
                            visible: common.0,
                            opacity: common.1,
                            offset: common.2,
                            data,
                            properties: BTreeMap::new(),
                        };
                        merged_layers.push(MapLayer::Tiles(tile_layer));
                        local_layers.push(MapLayer::Tiles(TileLayer {
                            name: name.clone(),
                            visible: common.0,
                            opacity: common.1,
                            offset: [layer.total_offset_x as f32, layer.total_offset_y as f32],
                            data: local_data,
                            properties: BTreeMap::new(),
                        }));
                        local_names.push(name.clone());
                    }
                    // IntGrid values are the collision layer. They are kept when the layer has
                    // no rule tiles; previously they were pushed only inside the tile branch.
                    if let Some((merged_values, local_values)) = values_layers {
                        merged_layers.push(merged_values);
                        local_layers.push(local_values);
                        local_names.push(format!("{name}_intgrid"));
                    }
                }
                "Entities" => {
                    entity_total += layer.entity_instances.len();
                    let mut objects = Vec::new();
                    let mut local_objects = Vec::new();
                    for (index, entity) in layer.entity_instances.iter().enumerate() {
                        let id_name = identifier_of(entity.identifier.as_ref(), "entity identifier")?;
                        let mut properties = BTreeMap::new();
                        for field in &entity.field_instances {
                            if let Some((key, value)) = field_property(field) {
                                properties.insert(key, value);
                            }
                        }
                        let shape = entity
                            .tile
                            .as_ref()
                            .and_then(|rect| tileset_of(rect.tileset_uid).map(|set| (rect, set)))
                            .and_then(|(rect, set)| {
                                let col = rect.x.checked_div(set.grid_size)?;
                                let row = rect.y.checked_div(set.grid_size)?;
                                let local = row.checked_mul(set.columns)?.checked_add(col)?;
                                if local < 0 || local >= i64::from(set.def.tile_count) {
                                    return None;
                                }
                                let gid = set.def.first_gid.checked_add(u32::try_from(local).ok()?)?;
                                Some(ObjectShape::Tile(gid))
                            })
                            .unwrap_or(if entity.width == 0.0 && entity.height == 0.0 {
                                ObjectShape::Point
                            } else {
                                ObjectShape::Rect
                            });
                        // LDtk positions the entity pivot; the map stores the top-left corner
                        // (+Y down, as in Tiled).
                        let top_left = (
                            entity.px[0] - entity.pivot[0] * entity.width,
                            entity.px[1] - entity.pivot[1] * entity.height,
                        );
                        let (wx, wy) = (
                            origin_x as f64 + layer.total_offset_x as f64 + top_left.0,
                            origin_y as f64 + layer.total_offset_y as f64 + top_left.1,
                        );
                        // __worldX/__worldY duplicate this for positioned layouts; a mismatch
                        // means the file was edited by hand — warn, keep the computed value.
                        let mismatched = match (entity.world_x, entity.world_y) {
                            (Some(exact_x), Some(exact_y)) => {
                                // The exact values come from the JSON: an overflowing difference
                                // is a mismatch, never a panic.
                                let off = |computed: f64, exact: i64| {
                                    (computed.round() as i64)
                                        .checked_sub(exact)
                                        .is_none_or(|d| d.unsigned_abs() > 1)
                                };
                                off(level.world_x as f64 + top_left.0, exact_x)
                                    || off(level.world_y as f64 + top_left.1, exact_y)
                            }
                            _ => false,
                        };
                        if mismatched {
                            warnings.push(format!(
                                "entity `{id_name}`: __worldX/__worldY disagree with the level position"
                            ));
                        }
                        // One probe per object; the level-local map reuses the merged id (it
                        // holds the same objects, so a merged-unique id is local-unique too).
                        let id = object_id(entity.iid.as_deref(), index, &mut object_ids);
                        objects.push(MapObject {
                            id,
                            name: id_name.clone(),
                            class: String::new(),
                            position: [wx as f32, wy as f32],
                            size: [entity.width as f32, entity.height as f32],
                            rotation: 0.0,
                            shape: shape.clone(),
                            properties: properties.clone(),
                        });
                        local_objects.push(MapObject {
                            id,
                            name: id_name,
                            class: String::new(),
                            position: [
                                (layer.total_offset_x as f64 + top_left.0) as f32,
                                (layer.total_offset_y as f64 + top_left.1) as f32,
                            ],
                            size: [entity.width as f32, entity.height as f32],
                            rotation: 0.0,
                            shape,
                            properties,
                        });
                    }
                    merged_layers.push(MapLayer::Objects(ObjectLayer {
                        name: layer_name(level, &name),
                        visible: common.0,
                        objects,
                        properties: BTreeMap::new(),
                    }));
                    local_layers.push(MapLayer::Objects(ObjectLayer {
                        name: name.clone(),
                        visible: common.0,
                        objects: local_objects,
                        properties: BTreeMap::new(),
                    }));
                    local_names.push(name.clone());
                }
                _ => {}
            }
        }
        let properties: BTreeMap<String, PropertyValue> =
            level.field_instances.iter().filter_map(field_property).collect();
        let background = level
            .bg_color
            .as_deref()
            .and_then(map_color)
            .or_else(|| root.bg_color.as_deref().and_then(map_color));
        level_maps.push(TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation: Default::default(),
            width: u32::try_from(level_w).unwrap_or(u32::MAX),
            height: u32::try_from(level_h).unwrap_or(u32::MAX),
            tile_width: u32::try_from(grid_size).unwrap_or(u32::MAX),
            tile_height: u32::try_from(grid_size).unwrap_or(u32::MAX),
            background: background.clone(),
            tilesets: tilesets.iter().map(|ts| ts.def.clone()).collect(),
            layers: local_layers,
            properties: properties.clone(),
        });
        level_infos.push(LdtkLevelInfo {
            identifier: level_name.clone(),
            world_x: level.world_x,
            world_y: level.world_y,
            size_px: [
                u32::try_from(level.px_wid).unwrap_or(u32::MAX),
                u32::try_from(level.px_hei).unwrap_or(u32::MAX),
            ],
            layers: local_names,
            entities: entity_total,
        });
    }

    let merged = TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation: Default::default(),
        width: u32::try_from(map_w).unwrap_or(u32::MAX),
        height: u32::try_from(map_h).unwrap_or(u32::MAX),
        tile_width: u32::try_from(grid_size).unwrap_or(u32::MAX),
        tile_height: u32::try_from(grid_size).unwrap_or(u32::MAX),
        background: levels
            .first()
            .and_then(|l| l.bg_color.as_deref())
            .and_then(map_color)
            .or_else(|| root.bg_color.as_deref().and_then(map_color)),
        tilesets: tilesets.iter().map(|ts| ts.def.clone()).collect(),
        layers: merged_layers,
        properties: levels
            .first()
            .map(|l| l.field_instances.iter().filter_map(field_property).collect())
            .unwrap_or_default(),
    };
    merged
        .validate()
        .map_err(|e| LdtkError::Structure(e.to_string()))?;
    for level in &level_maps {
        level
            .validate()
            .map_err(|e| LdtkError::Structure(e.to_string()))?;
    }

    Ok(ImportedLdtk {
        merged,
        levels: level_maps,
        level_infos,
        tilesets: tilesets
            .iter()
            .map(|ts| (ts.def.name.clone(), ts.def.first_gid, ts.def.tile_count))
            .collect(),
        side_files,
        warnings,
    })
}

/// Sum of `parts`, or `None` when the sum overflows `i64` (a hostile coordinate).
fn checked_px(parts: &[i64]) -> Option<i64> {
    parts.iter().try_fold(0i64, |acc, v| acc.checked_add(*v))
}

/// `a / b` rounded up, for positive `b`.
fn ceil_div(a: i64, b: i64) -> i64 {
    if b <= 0 {
        return 0;
    }
    a.div_euclid(b) + i64::from(a.rem_euclid(b) != 0)
}

/// Writes `gid` into `data` at the cell holding map pixel `(px, py)`; false when the tile lands
/// outside the map (a hostile or corrupt file can place tiles anywhere).
fn put_tile(data: &mut [u32], map_w: i64, map_h: i64, grid_size: i64, px: i64, py: i64, gid: u32) -> bool {
    let (x, y) = (px.div_euclid(grid_size), py.div_euclid(grid_size));
    if x < 0 || y < 0 || x >= map_w || y >= map_h {
        return false;
    }
    data[(y * map_w + x) as usize] = gid;
    true
}

#[cfg(test)]
mod tests {
    //! The fixtures are JSON documents composed in the tests (no third-party art or level
    //! files are checked in).

    use super::*;
    use serde_json::json;

    /// A tile layer JSON value with the fields the importer reads.
    fn layer(
        layer_type: &str,
        name: &str,
        c_wid: i64,
        c_hei: i64,
        extra: serde_json::Value,
    ) -> serde_json::Value {
        let mut value = json!({
            "__type": layer_type,
            "__identifier": name,
            "__gridSize": 8,
            "__cWid": c_wid,
            "__cHei": c_hei,
            "__opacity": 1.0,
            "visible": true,
            "__pxTotalOffsetX": 0,
            "__pxTotalOffsetY": 0,
            "autoLayerTiles": [],
            "gridTiles": [],
            "entityInstances": [],
            "intGridCsv": [],
            "iid": format!("layer-{name}"),
            "layerDefUid": 1,
            "levelId": 1,
            "pxOffsetX": 0,
            "pxOffsetY": 0,
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("extra object").clone());
        value
    }

    fn tile(t: i64, f: i64, px: [i64; 2]) -> serde_json::Value {
        json!({ "t": t, "f": f, "px": px, "src": [0, 0], "a": 1.0 })
    }

    fn field(identifier: &str, field_type: &str, value: serde_json::Value) -> serde_json::Value {
        json!({
            "__identifier": identifier,
            "__type": field_type,
            "__value": value,
            "defUid": 0,
        })
    }

    fn level(identifier: &str, world_x: i64, world_y: i64, layers: serde_json::Value) -> serde_json::Value {
        json!({
            "identifier": identifier,
            "iid": format!("iid-{identifier}"),
            "uid": identifier.len() as i64 + 10,
            "worldX": world_x,
            "worldY": world_y,
            "worldDepth": 0,
            "pxWid": 32,
            "pxHei": 16,
            "externalRelPath": null,
            "layerInstances": layers,
            "fieldInstances": [],
            "__bgColor": null,
            "bgRelPath": null,
            "__neighbours": [],
        })
    }

    /// The two-level project of the acceptance test: every layer kind, flips, fields.
    fn project_json(external: bool) -> serde_json::Value {
        let walls = layer(
            "IntGrid",
            "walls",
            4,
            2,
            json!({
                "intGridCsv": [1, 0, 0, 2, 0, 0, 0, 0],
                "autoLayerTiles": [tile(0, 1, [0, 0])],
                "__tilesetDefUid": 7,
            }),
        );
        let decor = layer(
            "Tiles",
            "decor",
            4,
            2,
            json!({
                "gridTiles": [tile(1, 2, [8, 8])],
                "__tilesetDefUid": 7,
            }),
        );
        let spawn = layer(
            "Entities",
            "spawn",
            4,
            2,
            json!({
                "entityInstances": [{
                    "iid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "__identifier": "Hero",
                    "__grid": [1, 1],
                    "__pivot": [0.5, 0.5],
                    "px": [8.0, 8.0],
                    "width": 8.0,
                    "height": 8.0,
                    "defUid": 0,
                    "__tags": [],
                    "__smartColor": "#F00",
                    "fieldInstances": [
                        field("hp", "Int", json!(3)),
                        field("name", "String", json!("cave")),
                        field("tint", "Color", json!("#ff0000")),
                    ],
                    "__tile": null,
                    "__worldX": 4,
                    "__worldY": 4,
                }],
            }),
        );
        let ground = layer(
            "Tiles",
            "ground",
            4,
            2,
            json!({
                "gridTiles": [tile(0, 0, [0, 0])],
                "__tilesetDefUid": 7,
            }),
        );
        // LDtk lists layers top-most first.
        let level_a = {
            let mut l = level("A", 0, 0, json!([spawn, walls, decor]));
            l["fieldInstances"] = json!([field("music", "String", json!("cave"))]);
            l["__bgColor"] = json!("#112233");
            if external {
                l["layerInstances"] = serde_json::Value::Null;
                l["externalRelPath"] = json!("levels/A.ldtkl");
            }
            l
        };
        let level_b = level("B", 32, 16, json!([ground]));
        json!({
            "jsonVersion": "1.5.3",
            "externalLevels": external,
            "app": "LDtk",
            "bgColor": "#000000",
            "worldGridWidth": null,
            "worldGridHeight": null,
            "worldLayout": "Free",
            "iid": "proj",
            "toc": [],
            "levels": [level_a, level_b],
            "worlds": [],
            "defs": {
                "tilesets": [{
                    "uid": 7,
                    "identifier": "terrain",
                    "relPath": "ts.png",
                    "pxWid": 16,
                    "pxHei": 8,
                    "tileGridSize": 8,
                    "__cWid": 2,
                    "__cHei": 1,
                    "spacing": 0,
                    "padding": 0,
                    "customData": [],
                    "enumTags": [],
                    "tags": [],
                }],
                "layers": [],
                "entities": [],
                "enums": [],
                "externalEnums": [],
                "levelFields": [],
            },
        })
    }

    fn import(root: &serde_json::Value) -> ImportedLdtk {
        import_ldtk(&serde_json::to_vec(root).expect("serialize"), Path::new(".")).expect("imports")
    }

    #[test]
    fn two_levels_with_every_layer_kind_merge_into_one_world() {
        let imported = import(&project_json(false));
        let map = &imported.merged;
        assert_eq!((map.width, map.height), (8, 4), "bbox 64x32 px at 8 px tiles");
        assert_eq!((map.tile_width, map.tile_height), (8, 8));
        assert_eq!(map.background.as_deref(), Some("#FF112233"));
        assert_eq!(map.tilesets.len(), 1);
        let set = &map.tilesets[0];
        assert_eq!(
            (set.name.as_str(), set.first_gid, set.tile_count),
            ("terrain", 1, 2)
        );
        assert_eq!(imported.tilesets, vec![("terrain".to_string(), 1, 2)]);
        assert_eq!(
            imported
                .level_infos
                .iter()
                .map(|l| (l.identifier.as_str(), l.world_x, l.world_y))
                .collect::<Vec<_>>(),
            vec![("A", 0, 0), ("B", 32, 16)],
        );

        // Bottom-first layers: level A's (decor, walls, walls_intgrid, spawn), then B's.
        let names: Vec<&str> = map.layers.iter().map(MapLayer::name).collect();
        assert_eq!(
            names,
            vec!["A/decor", "A/walls", "A/walls_intgrid", "A/spawn", "B/ground"]
        );

        let tiles = |name: &str| match map.layers.iter().find(|l| l.name() == name) {
            Some(MapLayer::Tiles(t)) => t,
            other => panic!("{name} is not a tile layer: {other:?}"),
        };
        // Decor: tile id 1 with a Y flip at cell (1, 1).
        let decor = tiles("A/decor");
        assert_eq!(decor.data[8 + 1], 2 | FLIP_Y_BIT);
        // Walls: the rule-placed tile, id 0 with an X flip, at the level's origin.
        let walls = tiles("A/walls");
        assert_eq!(walls.data[0], 1 | FLIP_X_BIT);

        // The IntGrid values ride along as points with the value as a property.
        let Some(MapLayer::Objects(values)) = map.layers.iter().find(|l| l.name() == "A/walls_intgrid")
        else {
            panic!("the values layer exists");
        };
        assert_eq!(values.objects.len(), 2);
        assert_eq!(values.objects[0].position, [4.0, 4.0]);
        assert_eq!(values.objects[1].position, [28.0, 4.0]);
        assert_eq!(
            values.objects[0].properties.get("value"),
            Some(&PropertyValue::Int(1))
        );

        // Entities: the pivot-centred Hero becomes a top-left Rect with fields.
        let Some(MapLayer::Objects(spawn)) = map.layers.iter().find(|l| l.name() == "A/spawn") else {
            panic!("the spawn layer exists");
        };
        assert_eq!(spawn.objects.len(), 1);
        let hero = &spawn.objects[0];
        assert_eq!(hero.name, "Hero");
        assert_eq!(hero.position, [4.0, 4.0], "pivot 0.5,0.5 of an 8x8 box at (8, 8)");
        assert_eq!(hero.size, [8.0, 8.0]);
        assert_eq!(hero.properties.get("hp"), Some(&PropertyValue::Int(3)));
        assert_eq!(
            hero.properties.get("name"),
            Some(&PropertyValue::String("cave".to_string()))
        );
        assert_eq!(
            hero.properties.get("tint"),
            Some(&PropertyValue::Color("#FFFF0000".to_string()))
        );

        // Level B's tile sits in the world cell. The offset stays 0: the batch adds it again,
        // so a copy of the world position there draws the quad at (64, -8) instead of (32, 8).
        let ground = tiles("B/ground");
        assert_eq!(ground.offset, [0.0, 0.0]);
        assert_eq!(ground.data[2 * 8 + 4], 1, "tile at world cell (4, 2)");
        let ([left, bottom], _) = map.tile_world_rect(4, 2);
        let drawn = [left as f32 + ground.offset[0], bottom as f32 - ground.offset[1]];
        assert_eq!(
            drawn,
            [32.0, 8.0],
            "one world position, not the cell plus the offset"
        );
    }

    #[test]
    fn each_level_is_also_a_level_local_sub_asset_map() {
        let imported = import(&project_json(false));
        assert_eq!(imported.levels.len(), 2);
        let a = &imported.levels[0];
        assert_eq!((a.width, a.height), (4, 2));
        assert_eq!(
            a.properties.get("music"),
            Some(&PropertyValue::String("cave".to_string()))
        );
        assert_eq!(a.background.as_deref(), Some("#FF112233"));
        let names: Vec<&str> = a.layers.iter().map(MapLayer::name).collect();
        assert_eq!(names, vec!["decor", "walls", "walls_intgrid", "spawn"]);
        let b = &imported.levels[1];
        assert_eq!((b.width, b.height), (4, 2));
        let Some(MapLayer::Objects(spawn)) = a.layers.iter().find(|l| l.name() == "spawn") else {
            panic!("spawn layer");
        };
        assert_eq!(spawn.objects[0].position, [4.0, 4.0], "level-local position");
    }

    #[test]
    fn external_levels_are_read_from_disk() {
        let dir = regia_test_utils::unique_temp_dir("ldtk-external");
        std::fs::create_dir_all(dir.join("levels")).expect("mkdir");
        // The file on disk carries the layers; the root's copy has layerInstances null.
        let root = project_json(true);
        let level_json = project_json(false)["levels"][0].clone();
        std::fs::write(
            dir.join("levels/A.ldtkl"),
            serde_json::to_vec(&level_json).expect("serialize"),
        )
        .expect("write the external level");
        let imported = import_ldtk(&serde_json::to_vec(&root).expect("serialize"), &dir)
            .expect("imports with external levels");
        assert_eq!(imported.side_files.len(), 1);
        assert_eq!(
            imported
                .merged
                .layers
                .iter()
                .map(MapLayer::name)
                .collect::<Vec<_>>(),
            vec!["A/decor", "A/walls", "A/walls_intgrid", "A/spawn", "B/ground"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Corr-low (pipeline-a #21): an external level that is not a regular file (a FIFO, a device)
    /// is refused before it is opened, rather than read until the writer closes it (a cook that
    /// blocks forever with no writer), and a regular file is read at most one byte past the limit.
    #[cfg(unix)]
    #[test]
    fn corr_low_an_external_level_that_is_not_a_regular_file_is_refused_unread() {
        let dir = regia_test_utils::unique_temp_dir("ldtk-external-fifo");
        std::fs::create_dir_all(dir.join("levels")).expect("mkdir");
        let fifo = dir.join("levels/A.ldtkl");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo failed");
        // A writer that closes at once, so an importer that does open the FIFO is not stuck.
        let writer_path = fifo.clone();
        let writer = std::thread::spawn(move || {
            drop(std::fs::OpenOptions::new().write(true).open(&writer_path));
        });
        let root = project_json(true);
        let err =
            import_ldtk(&serde_json::to_vec(&root).expect("serialize"), &dir).expect_err("a FIFO level");
        assert_eq!(err.code(), "ldtk_external", "{err}");
        assert!(
            err.to_string().contains("not a regular file"),
            "the FIFO was opened: {err}"
        );
        // Release the writer (it waits for a reader) and clean up.
        drop(std::fs::File::open(&fifo));
        writer.join().expect("writer");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_external_level_fails_with_its_code() {
        let root = project_json(true);
        let err = import_ldtk(&serde_json::to_vec(&root).expect("serialize"), Path::new("."))
            .expect_err("the external file does not exist");
        assert_eq!(err.code(), "ldtk_external");
    }

    #[test]
    fn hostile_inputs_fail_with_documented_codes() {
        let err = import_ldtk(b"not json", Path::new(".")).expect_err("not JSON");
        assert_eq!(err.code(), "ldtk_json");
        let err = import_ldtk(b"{}", Path::new(".")).expect_err("empty object: no fields");
        assert_eq!(err.code(), "ldtk_json");
        let err = import_ldtk(br#"{"externalLevels": false}"#, Path::new(".")).expect_err("no version");
        assert_eq!(err.code(), "ldtk_unsupported_version");

        let mut root = project_json(false);
        root["jsonVersion"] = json!("2.0.0");
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("version 2");
        assert_eq!(err.code(), "ldtk_unsupported_version");

        // An oversized world is refused before any layer work.
        let mut big = project_json(false);
        big["levels"][1]["pxWid"] = json!(1 << 20);
        big["levels"][1]["pxHei"] = json!(1 << 20);
        let bytes = serde_json::to_vec(&big).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("too large");
        assert_eq!(err.code(), "ldtk_limits");

        // A tile outside every tileset warns and is skipped, not fatal.
        let mut rogue = project_json(false);
        rogue["levels"][1]["layerInstances"][0]["gridTiles"] = json!([tile(9, 0, [0, 0])]);
        let imported = import(&rogue);
        assert!(
            imported.warnings.iter().any(|w| w.contains("did not resolve")),
            "{:?}",
            imported.warnings
        );
    }

    /// An external level path is a content reference: an absolute path or a `..` that leaves the
    /// level's folder is `ldtk_external`, and the file outside is not read.
    #[test]
    fn audfix_external_level_path_cannot_leave_its_folder() {
        let root = regia_test_utils::unique_temp_dir("ldtk-escape");
        let project = root.join("proj");
        std::fs::create_dir_all(project.join("levels")).expect("mkdir");
        let secret = root.join("secret.ldtkl");
        let mut leaked = project_json(false);
        leaked["levels"][0]["identifier"] = json!("LEAKED");
        std::fs::write(&secret, serde_json::to_vec(&leaked["levels"][0]).expect("level")).expect("secret");
        let mut root_json = project_json(true);
        root_json["levels"][0]["externalRelPath"] = json!("../secret.ldtkl");
        let bytes = serde_json::to_vec(&root_json).expect("serialize");
        let err = import_ldtk(&bytes, &project).expect_err("parent path");
        assert_eq!(err.code(), "ldtk_external", "{err}");
        assert!(
            !err.to_string().contains("LEAKED"),
            "the file outside the folder was parsed: {err}"
        );

        root_json["levels"][0]["externalRelPath"] = json!(secret.to_str().expect("utf-8"));
        let bytes = serde_json::to_vec(&root_json).expect("serialize");
        let err = import_ldtk(&bytes, &project).expect_err("absolute path");
        assert_eq!(err.code(), "ldtk_external", "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Level extents that overflow `i64`, or a negative size beside a valid level, are
    /// `ldtk_limits` rather than a panic or a huge allocation.
    #[test]
    fn audfix_overflowing_or_negative_level_sizes_are_limits_errors() {
        let mut root = project_json(false);
        root["levels"][0]["worldX"] = json!(i64::MAX - 5);
        root["levels"][0]["pxWid"] = json!(32);
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("world extent overflows");
        assert_eq!(err.code(), "ldtk_limits", "{err}");

        let mut root = project_json(false);
        root["levels"][1]["pxWid"] = json!(-32);
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("negative width");
        assert_eq!(err.code(), "ldtk_limits", "{err}");
    }

    /// A tile whose pixel position overflows `i64` is outside the map, not a panic.
    #[test]
    fn audfix_overflowing_tile_pixel_is_skipped() {
        let mut root = project_json(false);
        root["levels"][1]["layerInstances"][0]["gridTiles"] = json!([tile(0, 0, [i64::MAX, 0])]);
        let imported = import(&root);
        assert!(
            imported.warnings.iter().any(|w| w.contains("outside the map")),
            "{:?}",
            imported.warnings
        );
    }

    /// Tileset GIDs stay below the flip bits. A tileset whose ids would set those bits is
    /// `ldtk_limits`, so an unflipped tile is never stored as flipped.
    #[test]
    fn audfix_tileset_gids_stop_before_the_flip_bits() {
        let mut root = project_json(false);
        // 32768 * 16384 = 2^29, which is the diagonal flip bit. first gid 1 plus that count
        // would store the last tile with a flip bit set.
        root["defs"]["tilesets"][0]["__cWid"] = json!(32_768);
        root["defs"]["tilesets"][0]["__cHei"] = json!(16_384);
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("gid space");
        assert_eq!(err.code(), "ldtk_limits", "{err}");
    }

    /// Free and GridVania worlds may place a level at a negative coordinate. That is not the
    /// linear-layout sentinel (`-1`), so the level stays there instead of being stacked at y = 0.
    #[test]
    fn audfix_a_free_layout_keeps_a_negative_world_position() {
        let mut root = project_json(false);
        root["levels"][1]["worldX"] = json!(-32);
        let imported = import(&root);
        assert!(
            !imported.warnings.iter().any(|w| w.contains("no world position")),
            "{:?}",
            imported.warnings
        );
        assert_eq!(imported.level_infos[1].world_x, -32);
        let Some(MapLayer::Tiles(ground)) = imported.merged.layers.iter().find(|l| l.name() == "B/ground")
        else {
            panic!("ground layer");
        };
        assert_eq!(ground.offset, [0.0, 0.0]);
        // worldY stays 16, so the tile is on row 2 of the merged map, at the min x (cell 0).
        // Cell 0 would be level A's row, which this level does not occupy.
        assert_eq!(imported.merged.width, 8);
        assert_eq!(
            ground.data[2 * 8],
            1,
            "level B's tile is at the merged map's min x, on its world row"
        );
        let ([left, bottom], _) = imported.merged.tile_world_rect(0, 2);
        let drawn = [left as f32 + ground.offset[0], bottom as f32 - ground.offset[1]];
        assert_eq!(drawn, [0.0, 8.0], "the negative world x is the map's left edge");
    }

    /// A linear layout's `-1` coordinates are still placed in a row.
    #[test]
    fn audfix_a_linear_layout_places_unpositioned_levels_in_a_row() {
        let mut root = project_json(false);
        root["worldLayout"] = json!("LinearHorizontal");
        for level in root["levels"].as_array_mut().expect("levels") {
            level["worldX"] = json!(-1);
            level["worldY"] = json!(-1);
        }
        let imported = import(&root);
        assert_eq!(
            (imported.level_infos[0].world_x, imported.level_infos[1].world_x),
            (0, 32)
        );
        assert!(
            imported.warnings.iter().any(|w| w.contains("no world position")),
            "{:?}",
            imported.warnings
        );
    }

    /// An IntGrid that only stores values (the usual collision layer) still imports those values.
    #[test]
    fn audfix_intgrid_values_import_without_tiles() {
        let mut root = project_json(false);
        root["levels"][0]["layerInstances"][1]["autoLayerTiles"] = json!([]);
        root["levels"][0]["layerInstances"][1]["gridTiles"] = json!([]);
        let imported = import(&root);
        let names: Vec<&str> = imported.merged.layers.iter().map(MapLayer::name).collect();
        assert!(names.contains(&"A/walls_intgrid"), "{names:?}");
        assert!(!names.contains(&"A/walls"), "{names:?}");
        let Some(MapLayer::Objects(values)) = imported
            .merged
            .layers
            .iter()
            .find(|l| l.name() == "A/walls_intgrid")
        else {
            panic!("values layer");
        };
        assert_eq!(values.objects.len(), 2);
        assert_eq!(values.objects[0].position, [4.0, 4.0]);
    }

    /// Two levels with one identifier would cook as one sub-asset and drop the first level.
    #[test]
    fn audfix_duplicate_level_identifiers_are_a_structure_error() {
        let mut root = project_json(false);
        root["levels"][1]["identifier"] = json!("A");
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let err = import_ldtk(&bytes, Path::new(".")).expect_err("duplicate identifier");
        assert_eq!(err.code(), "ldtk_structure", "{err}");
    }

    /// `__worldX`/`__worldY` come from the JSON: `i64::MIN` overflowed the mismatch check's
    /// subtraction (a panic in debug and fuzz builds). It is a mismatch warning now.
    #[test]
    fn corr_extreme_world_position_warns_instead_of_overflowing() {
        for exact in [i64::MIN, i64::MAX] {
            let mut root = project_json(false);
            root["levels"][0]["layerInstances"][0]["entityInstances"][0]["__worldX"] = json!(exact);
            let imported = import(&root);
            assert!(
                imported.warnings.iter().any(|w| w.contains("disagree")),
                "{exact}: {:?}",
                imported.warnings
            );
        }
    }

    /// `__cWid × __cHei` of a tileset overflowed `i64` (a panic in debug, a wrapped tile count
    /// in release).
    #[test]
    fn corr_overflowing_tileset_grid_is_a_limits_error() {
        for (w, h) in [(1_i64 << 32, 1_i64 << 32), (3, 6_148_914_691_236_517_206)] {
            let mut root = project_json(false);
            root["defs"]["tilesets"][0]["__cWid"] = json!(w);
            root["defs"]["tilesets"][0]["__cHei"] = json!(h);
            let bytes = serde_json::to_vec(&root).expect("serialize");
            let Err(err) = import_ldtk(&bytes, Path::new(".")) else {
                panic!("overflowing tileset grid (the Ok value is not printed: it can hold gigabytes)");
            };
            assert_eq!(err.code(), "ldtk_limits", "{w}x{h}: {err}");
        }
    }

    /// Every kept tile layer allocates a merged-map grid and a level grid; the total is bounded
    /// before any grid is allocated (nine empty layers on a 64 Mi-tile level asked for 4.5 GiB).
    #[test]
    fn corr_tile_layer_grids_over_the_cell_budget_are_rejected() {
        let layers = |count: usize| {
            serde_json::Value::Array(
                (0..count)
                    .map(|i| layer("Tiles", &format!("t{i}"), 1, 1, json!({})))
                    .collect(),
            )
        };
        let mut big = level("Big", 0, 0, layers(9));
        big["pxWid"] = json!(65_536);
        big["pxHei"] = json!(65_536);
        let mut root = project_json(false);
        root["levels"] = json!([big]);
        let bytes = serde_json::to_vec(&root).expect("serialize");
        let Err(err) = import_ldtk(&bytes, Path::new(".")) else {
            panic!("9 x 128 Mi cells is over 1 Gi (the Ok value is not printed: it can hold gigabytes)");
        };
        assert_eq!(err.code(), "ldtk_limits", "{err}");

        // A smaller level with two layers fits and imports.
        let mut ok = level("Big", 0, 0, layers(2));
        ok["pxWid"] = json!(8_192);
        ok["pxHei"] = json!(8_192);
        root["levels"] = json!([ok]);
        let imported = import(&root);
        assert_eq!((imported.merged.width, imported.merged.height), (1024, 1024));
    }

    /// The 32-bit id hash collides on large maps (the birthday bound), so `object_id` probes
    /// the map's id set. Forced collision: the candidate is taken, so the probe bumps past the
    /// occupied run (rev-corr-assets, sprint-corr-asset-pipeline-a 1).
    #[test]
    fn corr_object_id_probe_skips_a_forced_collision() {
        let mut used = HashSet::new();
        assert_eq!(probe_object_id(7, &mut used), 7, "the free candidate is kept");
        assert_eq!(probe_object_id(7, &mut used), 8, "a taken candidate bumps by one");
        used.insert(9);
        used.insert(10);
        assert_eq!(
            probe_object_id(9, &mut used),
            11,
            "the probe runs past an occupied run"
        );
        assert_eq!(used.len(), 5, "two probes plus the manually occupied run");
        // The same iid asked again lands past everything handed out for the map.
        let mut map = HashSet::new();
        let first = object_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), 0, &mut map);
        let second = object_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), 0, &mut map);
        assert_ne!(first, second, "one object, one probe: repeats get a fresh id");
    }

    /// rev-corr-assets (sprint-corr-asset-pipeline-a 1): the IntGrid object id was the first 4
    /// bytes of a blake3 hash — a random 32-bit id, so the birthday bound makes collisions
    /// likely on a large map (≈ 12 % at 32 k ids, ≈ 70 % at 100 k). 16 layers of 128×128 cells
    /// give 262144 ids; `MapObject::id` is documented unique, so every one must be distinct.
    #[test]
    fn corr_intgrid_object_ids_stay_unique_on_a_large_map() {
        let layers = serde_json::Value::Array(
            (0..16)
                .map(|i| {
                    layer(
                        "IntGrid",
                        &format!("g{i}"),
                        128,
                        128,
                        json!({ "intGridCsv": vec![1; 128 * 128] }),
                    )
                })
                .collect(),
        );
        let mut a = level("A", 0, 0, layers);
        a["uid"] = json!(1);
        let mut root = project_json(false);
        root["levels"] = json!([a]);
        let imported = import(&root);
        let mut ids = Vec::new();
        for layer in &imported.merged.layers {
            if let MapLayer::Objects(objects) = layer {
                ids.extend(objects.objects.iter().map(|o| o.id));
            }
        }
        assert_eq!(ids.len(), 16 * 128 * 128, "sixteen 128×128 IntGrid layers");
        let unique: std::collections::BTreeSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "every merged object id is distinct");
    }

    /// `MapObject::id` is documented unique: IntGrid value objects of neighbouring levels (and of
    /// two layers of one level) collided when the id was the cell index plus a shifted level uid.
    #[test]
    fn corr_intgrid_object_ids_are_unique_across_levels_and_layers() {
        let grid = |name: &str| layer("IntGrid", name, 20, 20, json!({ "intGridCsv": vec![1; 400] }));
        let mut a = level("A", 0, 0, json!([grid("walls"), grid("water")]));
        a["uid"] = json!(1);
        let mut b = level("B", 32, 0, json!([grid("walls")]));
        b["uid"] = json!(2);
        let mut root = project_json(false);
        root["levels"] = json!([a, b]);
        let imported = import(&root);
        let mut ids = Vec::new();
        for layer in &imported.merged.layers {
            if let MapLayer::Objects(objects) = layer {
                ids.extend(objects.objects.iter().map(|o| o.id));
            }
        }
        assert_eq!(ids.len(), 1200, "three IntGrid layers of 400 values");
        let unique: std::collections::BTreeSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "every merged object id is distinct");
    }
}
