// SPDX-License-Identifier: MPL-2.0
//! Tilemaps (`docs/specs/22-tier2.md` § T2.10): the frozen `.tilemap.ron` format, its cooked form
//! and tile queries.
//!
//! `regia_asset_pipeline` imports Tiled maps (`.tmx` with `.tsx` tilesets, and the JSON
//! `.tmj`/`.json` export) into a [`TileMapAsset`] — the same structure a `.tilemap.ron` holds — so
//! a generator can write `.tilemap.ron` directly and the editor can save one. Tile layers keep
//! Tiled's global tile ids (GIDs) including the three flip bits ([`TileFlags`]); row 0 is the
//! **top** row, as in Tiled.
//!
//! In the world the map's origin (its entity's `GlobalTransform`, which equals `Transform` without a parent) is the **bottom-left** corner and
//! +Y is up. On an orthogonal map, tile `(x, y)` covers
//! `[x·tw, (x+1)·tw] × [(h−1−y)·th, (h−y)·th]` in map pixels, and a `y` past the last row is
//! clamped to that row (`x` is not). Isometric, staggered and hexagonal maps take
//! [`Orientation::cell_origin`] (map pixels, +Y down) and flip Y into the same bottom-left frame
//! ([`TileMapAsset::tile_world_rect`]).
//!
//! Collision comes from the tileset: a tile is [`TileCollision::Solid`] or
//! [`TileCollision::OneWay`] (the platformer's jump-through) when its Tiled tile has a `collision`
//! string property `"solid"` / `"one_way"`, a bool property `solid` / `one_way`, or that class.

use std::cell::Cell;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use regia_core::version::{self, VersionStep};

use crate::atlas::RgbaImage;

/// Current `.tilemap.ron` version (v2 adds the isometric, staggered and hexagonal orientations,
/// ADR-2597).
pub const TILEMAP_FORMAT_VERSION: u32 = 2;

/// Current cooked [`CookedTileMap`] payload version.
pub const COOKED_TILEMAP_VERSION: u32 = 1;

/// Type path of the cooked tilemap payload (the cooked-file type tag).
pub const TILEMAP_TYPE_PATH: &str = "regia_2d::CookedTileMap";

/// Tilemap format errors.
#[derive(Debug, thiserror::Error)]
pub enum TileMapError {
    /// The file is a newer version than this build reads.
    #[error("tilemap version {found} is newer than supported version {expected}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("tilemap parse error: {0}")]
    Parse(String),
    /// Semantic error.
    #[error("invalid tilemap: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("tilemap migration failed: {0}")]
    Migration(String),
    /// Cooked payload decode error.
    #[error("cooked tilemap decode error: {0}")]
    Decode(String),
}

/// Which axis of a staggered or hexagonal map is offset every other row or column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StaggerAxis {
    /// Every other **column** is shifted down (flat-topped hexagons).
    X,
    /// Every other **row** is shifted right (pointy-topped hexagons).
    #[default]
    Y,
}

/// Which rows or columns are shifted (indices count from 0, so `Odd` shifts 1, 3, 5 ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StaggerIndex {
    /// The even rows or columns are shifted.
    Even,
    /// The odd rows or columns are shifted.
    #[default]
    Odd,
}

/// Map orientation (the geometry is in `grid.rs`: [`Orientation::pixel_size`],
/// [`Orientation::cell_origin`], [`Orientation::cell_at`] and [`Orientation::draw_order`]).
///
/// Cell `(x, y)` is always the column and row of the layer data (row 0 first); the orientation
/// only decides where that cell sits in the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Orientation {
    /// Square grid.
    #[default]
    Orthogonal,
    /// Diamond isometric: `x` runs down-right, `y` down-left; `tile_width` x `tile_height` is the
    /// diamond's bounding box (typically 2:1).
    Isometric,
    /// Staggered isometric: diamonds in offset rows or columns.
    Staggered {
        /// Offset axis (default `Y`).
        #[serde(default)]
        axis: StaggerAxis,
        /// Which rows or columns are shifted (default `Odd`).
        #[serde(default)]
        index: StaggerIndex,
    },
    /// Hexagonal: `tile_width` x `tile_height` is the hexagon's bounding box and `side_length` the
    /// length of its straight edge (the vertical one when pointy, the horizontal one when flat).
    Hexagonal {
        /// Offset axis (`Y` = pointy-topped, `X` = flat-topped; default `Y`).
        #[serde(default)]
        axis: StaggerAxis,
        /// Which rows or columns are shifted (default `Odd`).
        #[serde(default)]
        index: StaggerIndex,
        /// Straight edge length in pixels (0 degenerates to a staggered isometric diamond).
        #[serde(default)]
        side_length: u32,
    },
}

/// A custom property value (Tiled's property types).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PropertyValue {
    /// `bool`.
    Bool(bool),
    /// `int`.
    Int(i64),
    /// `float`.
    Float(f64),
    /// `string` (also Tiled's `class`-less enums).
    String(String),
    /// `color` as `#AARRGGBB` / `#RRGGBB`.
    Color(String),
    /// `file`, relative to the map.
    File(String),
    /// `object` (an object id).
    Object(u32),
}

/// What a tile is for the platformer and other tile-collision users.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TileCollision {
    /// No collision.
    #[default]
    Empty,
    /// Blocks from every side.
    Solid,
    /// Jump-through: blocks only something falling onto it from above.
    OneWay,
}

/// One frame of an animated tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileFrame {
    /// Local tile id shown.
    pub tile: u32,
    /// Duration in milliseconds.
    pub duration_ms: u32,
}

/// Per-tile data of a tileset (only tiles that have any).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct TileDef {
    /// Local tile id.
    pub id: u32,
    /// Tiled class / type string.
    #[serde(default)]
    pub class: String,
    /// Collision kind.
    #[serde(default)]
    pub collision: TileCollision,
    /// Animation frames (empty = static).
    #[serde(default)]
    pub animation: Vec<TileFrame>,
    /// Custom properties.
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
}

/// A tileset: one image cut into a grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TilesetDef {
    /// Name.
    pub name: String,
    /// First GID of the tileset in this map.
    pub first_gid: u32,
    /// Image path, relative to the map file.
    pub image: String,
    /// Image width in pixels.
    pub image_width: u32,
    /// Image height in pixels.
    pub image_height: u32,
    /// Tile width in pixels.
    pub tile_width: u32,
    /// Tile height in pixels.
    pub tile_height: u32,
    /// Tiles per row.
    pub columns: u32,
    /// Total tiles.
    pub tile_count: u32,
    /// Pixels between tiles.
    #[serde(default)]
    pub spacing: u32,
    /// Pixels around the grid.
    #[serde(default)]
    pub margin: u32,
    /// Tiles with data, sorted by id.
    #[serde(default)]
    pub tiles: Vec<TileDef>,
}

impl TilesetDef {
    /// Source rectangle `[x, y, w, h]` (image pixels, origin top-left) of local tile `id`.
    #[must_use]
    pub fn tile_rect(&self, id: u32) -> Option<[u32; 4]> {
        if id >= self.tile_count || self.columns == 0 {
            return None;
        }
        let (col, row) = (id % self.columns, id / self.columns);
        // Hostile content can make these products exceed u32: report no rectangle, not a wrap.
        let axis = |index: u32, size: u32| {
            index
                .checked_mul(size.checked_add(self.spacing)?)?
                .checked_add(self.margin)
        };
        Some([
            axis(col, self.tile_width)?,
            axis(row, self.tile_height)?,
            self.tile_width,
            self.tile_height,
        ])
    }

    /// The data of local tile `id`, if it has any.
    #[must_use]
    pub fn tile(&self, id: u32) -> Option<&TileDef> {
        self.tiles
            .binary_search_by_key(&id, |t| t.id)
            .ok()
            .map(|i| &self.tiles[i])
    }

    /// Prefix sums of every animated tile, sorted by local id. Empty animations are omitted.
    ///
    /// Each frame duration is read once here. [`TileAnimPrefix::at`] then does not walk the list.
    pub(crate) fn anim_prefixes(&self) -> Vec<TileAnimPrefix> {
        let mut out = Vec::new();
        for def in &self.tiles {
            if def.animation.is_empty() {
                continue;
            }
            note_prefix_frame_reads(def.animation.len() as u64);
            let mut ends_ms = Vec::with_capacity(def.animation.len());
            let mut tiles = Vec::with_capacity(def.animation.len());
            let mut sum = 0u64;
            for frame in &def.animation {
                sum += u64::from(frame.duration_ms);
                ends_ms.push(sum);
                tiles.push(frame.tile);
            }
            out.push(TileAnimPrefix {
                id: def.id,
                total_ms: sum,
                ends_ms,
                tiles,
            });
        }
        out.sort_by_key(|prefix| prefix.id);
        out
    }

    /// A fingerprint of every animated frame. A change in id, length, tile or duration misses the
    /// cache built by [`Self::anim_prefixes`].
    pub(crate) fn anim_fingerprint(&self) -> u64 {
        // FNV-1a 64. The input is the tileset's own animation tables, not a hash of pixels.
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for def in &self.tiles {
            hash = fnv1a(hash, u64::from(def.id));
            hash = fnv1a(hash, def.animation.len() as u64);
            for frame in &def.animation {
                hash = fnv1a(hash, u64::from(frame.tile));
                hash = fnv1a(hash, u64::from(frame.duration_ms));
            }
        }
        hash
    }
}

fn fnv1a(hash: u64, value: u64) -> u64 {
    // FNV-1a: xor the word, then multiply by the FNV prime.
    (hash ^ value).wrapping_mul(0x0000_0100_0000_01B3)
}

thread_local! {
    /// How many animation-frame durations [`TilesetDef::anim_prefixes`] has read on this thread.
    static PREFIX_FRAME_READS: Cell<u64> = const { Cell::new(0) };
}

/// How many animation-frame durations [`TilesetDef::anim_prefixes`] has read on this thread.
///
/// Lookups through `TileAnimPrefix::at` do not increment this.
#[cfg(test)]
#[must_use]
pub(crate) fn prefix_frame_reads() -> u64 {
    PREFIX_FRAME_READS.with(Cell::get)
}

/// Zeroes [`prefix_frame_reads`] on this thread.
#[cfg(test)]
pub(crate) fn reset_prefix_frame_reads() {
    PREFIX_FRAME_READS.with(|cell| cell.set(0));
}

fn note_prefix_frame_reads(frames: u64) {
    PREFIX_FRAME_READS.with(|cell| cell.set(cell.get().saturating_add(frames)));
}

/// A layer of tiles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TileLayer {
    /// Name (group layers flatten to `group/name`).
    pub name: String,
    /// Drawn when true.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Opacity 0..1.
    #[serde(default = "default_one")]
    pub opacity: f32,
    /// Pixel offset (Tiled's `offsetx`, `offsety`; +Y down as in Tiled).
    #[serde(default)]
    pub offset: [f32; 2],
    /// GIDs row by row, top row first (`width * height`; 0 = empty).
    pub data: Vec<u32>,
    /// Custom properties.
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
}

/// Shape of a map object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ObjectShape {
    /// Rectangle (`width`×`height`).
    Rect,
    /// Ellipse in the `width`×`height` box.
    Ellipse,
    /// A point.
    Point,
    /// Closed polygon, points relative to the object position.
    Polygon(Vec<[f32; 2]>),
    /// Open polyline, points relative to the object position.
    Polyline(Vec<[f32; 2]>),
    /// A tile object showing `gid`.
    Tile(u32),
}

/// One object of an object layer (Tiled coordinates: map pixels, +Y down).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapObject {
    /// Unique object id.
    pub id: u32,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Class / type.
    #[serde(default)]
    pub class: String,
    /// Position (top-left; bottom-left for tile objects, as in Tiled).
    pub position: [f32; 2],
    /// Size.
    #[serde(default)]
    pub size: [f32; 2],
    /// Rotation in degrees, clockwise.
    #[serde(default)]
    pub rotation: f32,
    /// Shape.
    pub shape: ObjectShape,
    /// Custom properties.
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
}

/// A layer of objects (spawn points, triggers, collision shapes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectLayer {
    /// Name.
    pub name: String,
    /// Reserved for tile objects; object layers are not drawn yet (spawn points and shapes are data).
    #[serde(default = "default_true")]
    pub visible: bool,
    /// The objects.
    pub objects: Vec<MapObject>,
    /// Custom properties.
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
}

/// A map layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MapLayer {
    /// Tiles.
    Tiles(TileLayer),
    /// Objects.
    Objects(ObjectLayer),
}

impl MapLayer {
    /// The layer name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Tiles(l) => &l.name,
            Self::Objects(l) => &l.name,
        }
    }
}

/// Most points one polygon or polyline object may hold.
const MAX_SHAPE_POINTS: usize = 65_536;
/// Most animation frames one tile may hold.
const MAX_ANIMATION_FRAMES: usize = 4_096;
/// Most per-tile definitions one tileset may hold.
const MAX_TILE_DEFS: usize = 1 << 20;
/// Longest custom-property key or string value (bytes).
const MAX_PROPERTY_STRING: usize = 64 * 1024;

/// Rejects a custom-property key or string value longer than `MAX_PROPERTY_STRING` bytes.
fn check_properties(props: &BTreeMap<String, PropertyValue>) -> Result<(), TileMapError> {
    for (key, value) in props {
        let len = match value {
            PropertyValue::String(s) | PropertyValue::Color(s) | PropertyValue::File(s) => s.len(),
            _ => 0,
        };
        if key.len() > MAX_PROPERTY_STRING || len > MAX_PROPERTY_STRING {
            return Err(TileMapError::Invalid(format!(
                "property `{}` is longer than {MAX_PROPERTY_STRING} bytes",
                key.chars().take(32).collect::<String>()
            )));
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}
fn default_one() -> f32 {
    1.0
}

/// The `.tilemap.ron` document.
///
/// ```
/// use regia_2d::tilemap::TileMapAsset;
///
/// let map = TileMapAsset::from_ron(r#"(
///     version: 2,
///     width: 2, height: 1, tile_width: 8, tile_height: 8,
///     tilesets: [(name: "t", first_gid: 1, image: "t.png", image_width: 16, image_height: 8,
///                 tile_width: 8, tile_height: 8, columns: 2, tile_count: 2)],
///     layers: [Tiles((name: "ground", data: [1, 2]))],
/// )"#).expect("valid");
/// assert_eq!(map.tile_at(0, 1, 0).map(|t| t.local_id), Some(1));
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TileMapAsset {
    /// Format version ([`TILEMAP_FORMAT_VERSION`]).
    pub version: u32,
    /// Orientation.
    #[serde(default)]
    pub orientation: Orientation,
    /// Width in tiles.
    pub width: u32,
    /// Height in tiles.
    pub height: u32,
    /// Tile width in pixels.
    pub tile_width: u32,
    /// Tile height in pixels.
    pub tile_height: u32,
    /// Background colour `#RRGGBB` / `#AARRGGBB`, if any.
    #[serde(default)]
    pub background: Option<String>,
    /// Tilesets, sorted by `first_gid`.
    pub tilesets: Vec<TilesetDef>,
    /// Layers, bottom first.
    pub layers: Vec<MapLayer>,
    /// Custom properties.
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
}

/// The flip bits of a GID (Tiled's top three bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TileFlags {
    /// Mirrored horizontally.
    pub flip_x: bool,
    /// Mirrored vertically.
    pub flip_y: bool,
    /// Mirrored along the top-left → bottom-right diagonal (applied first).
    pub diagonal: bool,
}

/// GID flag bits.
pub const FLIP_X_BIT: u32 = 0x8000_0000;
/// GID flag bit: vertical flip.
pub const FLIP_Y_BIT: u32 = 0x4000_0000;
/// GID flag bit: diagonal flip.
pub const FLIP_DIAGONAL_BIT: u32 = 0x2000_0000;
/// GID flag bit: hexagonal 120° rotation (unused for orthogonal maps; masked off).
pub const ROTATE_HEX_BIT: u32 = 0x1000_0000;
/// Mask of the id part of a GID.
pub const GID_MASK: u32 = !(FLIP_X_BIT | FLIP_Y_BIT | FLIP_DIAGONAL_BIT | ROTATE_HEX_BIT);

/// Splits a raw GID into its id and flip flags.
#[must_use]
pub const fn split_gid(raw: u32) -> (u32, TileFlags) {
    (
        raw & GID_MASK,
        TileFlags {
            flip_x: raw & FLIP_X_BIT != 0,
            flip_y: raw & FLIP_Y_BIT != 0,
            diagonal: raw & FLIP_DIAGONAL_BIT != 0,
        },
    )
}

/// A resolved tile of a layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileRef<'a> {
    /// GID without flags.
    pub gid: u32,
    /// Flip flags.
    pub flags: TileFlags,
    /// Index of the tileset.
    pub tileset: usize,
    /// Tile id inside the tileset.
    pub local_id: u32,
    /// The tileset.
    pub set: &'a TilesetDef,
}

impl TileRef<'_> {
    /// The tile's data, if it has any.
    #[must_use]
    pub fn def(&self) -> Option<&TileDef> {
        self.set.tile(self.local_id)
    }

    /// The tile's collision kind.
    #[must_use]
    pub fn collision(&self) -> TileCollision {
        self.def().map_or(TileCollision::Empty, |d| d.collision)
    }
}

/// A tile outline in map-local world space (+Y up). `len` is 4 or 6; only that many `points` are live.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldOutline {
    /// Clockwise vertices.
    pub points: [[f32; 2]; 6],
    /// 4 or 6.
    pub len: usize,
}

/// Prefix sums of one animated tile, built when the tileset is loaded.
///
/// Not part of the cooked map. `ends_ms[i]` is the loop time at which frame `i` ends, so a lookup
/// is one modulo and a binary search instead of a walk of the frame list.
#[derive(Clone, Debug)]
pub(crate) struct TileAnimPrefix {
    /// Local tile id this animation belongs to.
    pub id: u32,
    /// Sum of the frame durations. Zero means the animation never advances.
    total_ms: u64,
    /// Exclusive ends, one per frame, in order.
    ends_ms: Vec<u64>,
    /// Local tile id shown for each frame.
    tiles: Vec<u32>,
}

impl TileAnimPrefix {
    /// The frame shown at `time_ms`, or `None` when the loop length is zero.
    pub(crate) fn at(&self, time_ms: u64) -> Option<u32> {
        if self.total_ms == 0 || self.tiles.is_empty() {
            return None;
        }
        let t = time_ms % self.total_ms;
        let index = self.ends_ms.partition_point(|&end| end <= t);
        self.tiles.get(index).copied()
    }
}

impl TileMapAsset {
    /// Parses a `.tilemap.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, TileMapError> {
        let found = version::peek_ron_version(text).map_err(TileMapError::Parse)?;
        if found > TILEMAP_FORMAT_VERSION {
            return Err(TileMapError::Version {
                found,
                expected: TILEMAP_FORMAT_VERSION,
            });
        }
        let mut map: Self = ron::from_str(text).map_err(|e| TileMapError::Parse(e.to_string()))?;
        map.migrate_to_latest()?;
        map.validate()?;
        Ok(map)
    }

    /// Serialises to RON with a trailing newline.
    pub fn to_ron(&self) -> Result<String, TileMapError> {
        let mut s = ron::ser::to_string_pretty(self, regia_core::ron_pretty().compact_arrays(true))
            .map_err(|e| TileMapError::Parse(e.to_string()))?;
        s.push('\n');
        Ok(s)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, TileMapError> {
        if self.version > TILEMAP_FORMAT_VERSION {
            return Err(TileMapError::Version {
                found: self.version,
                expected: TILEMAP_FORMAT_VERSION,
            });
        }
        if self.version < TILEMAP_FORMAT_VERSION {
            self.version = version::migrate(self, self.version, tilemap_migrations(), TILEMAP_FORMAT_VERSION)
                .map_err(|e| TileMapError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// Positive sizes, tilesets sorted and non-overlapping, layer data the right length, every
    /// GID resolvable.
    pub fn validate(&self) -> Result<(), TileMapError> {
        let invalid = |m: String| Err(TileMapError::Invalid(m));
        if self.width == 0 || self.height == 0 || self.tile_width == 0 || self.tile_height == 0 {
            return invalid("map and tile sizes must be positive".into());
        }
        let cells = u64::from(self.width) * u64::from(self.height);
        if cells > MAX_CELLS {
            return invalid(format!("{}x{} tiles is too large", self.width, self.height));
        }
        if self.layers.len() > MAX_LAYERS {
            return invalid(format!(
                "{} layers exceed the limit of {MAX_LAYERS}",
                self.layers.len()
            ));
        }
        // The isometric box is (w + h) cells wide and high; the others at most w x h plus a half cell.
        let (across, down) = if self.orientation == Orientation::Isometric {
            let span = u64::from(self.width) + u64::from(self.height);
            (span, span)
        } else {
            (u64::from(self.width), u64::from(self.height))
        };
        // Half-cell overhangs and the doubled stagger steps need headroom; orthogonal keeps its limit.
        let limit = if self.orientation.is_orthogonal() {
            u64::from(u32::MAX)
        } else {
            u64::from(u32::MAX) / 2
        };
        if across * u64::from(self.tile_width) > limit || down * u64::from(self.tile_height) > limit {
            return invalid("the map's pixel size overflows 32 bits".into());
        }
        if let Orientation::Hexagonal {
            axis, side_length, ..
        } = self.orientation
        {
            let limit = match axis {
                StaggerAxis::Y => self.tile_height,
                StaggerAxis::X => self.tile_width,
            };
            if side_length > limit {
                return invalid(format!(
                    "hexagon side length {side_length} exceeds the tile size {limit} along its axis"
                ));
            }
        }
        let mut next_gid = 1u32;
        for set in &self.tilesets {
            if set.first_gid < next_gid {
                return invalid(format!("tileset `{}` overlaps the previous one", set.name));
            }
            if set.tile_width == 0 || set.tile_height == 0 || set.columns == 0 {
                return invalid(format!(
                    "tileset `{}` has a zero tile size or column count",
                    set.name
                ));
            }
            // The last tile's source rectangle must fit 32 bits (see `TilesetDef::tile_rect`).
            let rows = u64::from(set.tile_count.max(1) - 1) / u64::from(set.columns) + 1;
            let cols = u64::from(set.columns.min(set.tile_count.max(1)));
            let step = |size: u32| u64::from(size) + u64::from(set.spacing);
            if u64::from(set.margin) + rows * step(set.tile_height) > u64::from(u32::MAX)
                || u64::from(set.margin) + cols * step(set.tile_width) > u64::from(u32::MAX)
            {
                return invalid(format!(
                    "tileset `{}` is too large for 32-bit pixel math",
                    set.name
                ));
            }
            if set.tiles.windows(2).any(|p| p[0].id >= p[1].id) {
                return invalid(format!("tileset `{}` tile data is not sorted by id", set.name));
            }
            if set.tiles.len() > MAX_TILE_DEFS {
                return invalid(format!("tileset `{}` has too many tile definitions", set.name));
            }
            for def in &set.tiles {
                if def.animation.len() > MAX_ANIMATION_FRAMES {
                    return invalid(format!(
                        "tileset `{}` tile {} has too many animation frames",
                        set.name, def.id
                    ));
                }
                if def.id >= set.tile_count {
                    return invalid(format!(
                        "tileset `{}` defines tile {} beyond its {} tiles",
                        set.name, def.id, set.tile_count
                    ));
                }
                for frame in &def.animation {
                    if frame.tile >= set.tile_count || frame.duration_ms == 0 {
                        return invalid(format!(
                            "tileset `{}` tile {} has an animation frame with a missing tile or zero duration",
                            set.name, def.id
                        ));
                    }
                }
                check_properties(&def.properties)?;
            }
            next_gid = set.first_gid.saturating_add(set.tile_count);
        }
        check_properties(&self.properties)?;
        for layer in &self.layers {
            let tiles = match layer {
                MapLayer::Tiles(tiles) => tiles,
                MapLayer::Objects(objects) => {
                    check_properties(&objects.properties)?;
                    for o in &objects.objects {
                        if let ObjectShape::Polygon(p) | ObjectShape::Polyline(p) = &o.shape
                            && p.len() > MAX_SHAPE_POINTS
                        {
                            return invalid(format!(
                                "object {} in layer `{}` has too many shape points",
                                o.id, objects.name
                            ));
                        }
                        check_properties(&o.properties)?;
                    }
                    continue;
                }
            };
            check_properties(&tiles.properties)?;
            if tiles.data.len() as u64 != cells {
                return invalid(format!(
                    "layer `{}` has {} tiles, expected {cells}",
                    tiles.name,
                    tiles.data.len()
                ));
            }
            for &raw in &tiles.data {
                let (gid, _) = split_gid(raw);
                if gid != 0 && self.tileset_for(gid).is_none() {
                    return invalid(format!(
                        "layer `{}` uses GID {gid} that no tileset holds",
                        tiles.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// The tileset index and local id of a GID (flags already removed).
    #[must_use]
    pub fn tileset_for(&self, gid: u32) -> Option<(usize, u32)> {
        if gid == 0 {
            return None;
        }
        let index = self
            .tilesets
            .partition_point(|s| s.first_gid <= gid)
            .checked_sub(1)?;
        let set = &self.tilesets[index];
        let local = gid - set.first_gid;
        (local < set.tile_count).then_some((index, local))
    }

    /// The tile layer at `layer` (index into [`Self::layers`]).
    #[must_use]
    pub fn tile_layer(&self, layer: usize) -> Option<&TileLayer> {
        match self.layers.get(layer)? {
            MapLayer::Tiles(t) => Some(t),
            MapLayer::Objects(_) => None,
        }
    }

    /// Index of the layer named `name`.
    #[must_use]
    pub fn layer_index(&self, name: &str) -> Option<usize> {
        self.layers.iter().position(|l| l.name() == name)
    }

    /// The tile at column `x`, row `y` (row 0 = top) of `layer`, or `None` when empty/outside.
    #[must_use]
    pub fn tile_at(&self, layer: usize, x: u32, y: u32) -> Option<TileRef<'_>> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = u64::from(y) * u64::from(self.width) + u64::from(x);
        let raw = *self.tile_layer(layer)?.data.get(usize::try_from(index).ok()?)?;
        let (gid, flags) = split_gid(raw);
        let (tileset, local_id) = self.tileset_for(gid)?;
        Some(TileRef {
            gid,
            flags,
            tileset,
            local_id,
            set: &self.tilesets[tileset],
        })
    }

    /// Writes a raw GID (with flags) at column `x`, row `y` of tile layer `layer`. Returns false
    /// when the cell or layer does not exist or no tileset holds the GID.
    pub fn set_tile(&mut self, layer: usize, x: u32, y: u32, raw_gid: u32) -> bool {
        let (gid, _) = split_gid(raw_gid);
        if (gid != 0 && self.tileset_for(gid).is_none()) || x >= self.width || y >= self.height {
            return false;
        }
        // u64 like `tile_at`: on an unvalidated (code-built) map `y * width` can pass u32.
        let Ok(index) = usize::try_from(u64::from(y) * u64::from(self.width) + u64::from(x)) else {
            return false;
        };
        match self.layers.get_mut(layer) {
            Some(MapLayer::Tiles(t)) => match t.data.get_mut(index) {
                Some(cell) => {
                    *cell = raw_gid;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The size parameters of the map's grid ([`crate::grid::GridSize`]).
    #[must_use]
    pub fn grid_size(&self) -> crate::grid::GridSize {
        crate::grid::GridSize {
            width: self.width,
            height: self.height,
            tile_width: self.tile_width,
            tile_height: self.tile_height,
        }
    }

    /// Map size in pixels (the bounding box of the whole grid in the map's orientation).
    /// Fractional extents round upward so the integer box contains every cell.
    #[must_use]
    pub fn pixel_size(&self) -> [u32; 2] {
        let [w, h] = self.orientation.pixel_size(self.grid_size());
        // Saturating, like the orthogonal arithmetic always was (NaN-free: sizes are finite).
        [
            w.ceil().min(f64::from(u32::MAX)) as u32,
            h.ceil().min(f64::from(u32::MAX)) as u32,
        ]
    }

    /// World rectangle `(min, max)` of the **bounding box** of tile `(x, y)` in map pixels
    /// relative to the map origin (bottom-left, +Y up). Tile images draw from its bottom-left.
    /// Corners use double precision so neighboring cells remain distinct on legal wide maps.
    /// Convert to GPU coordinates after applying the map's transform (including any rebasing).
    #[must_use]
    pub fn tile_world_rect(&self, x: u32, y: u32) -> ([f64; 2], [f64; 2]) {
        let (tw, th) = (f64::from(self.tile_width), f64::from(self.tile_height));
        if self.orientation.is_orthogonal() {
            let last_row = self.height.saturating_sub(1);
            let bottom = f64::from(last_row - y.min(last_row)) * th;
            let left = f64::from(x) * tw;
            return ([left, bottom], [left + tw, bottom + th]);
        }
        let g = self.grid_size();
        let [_, map_h] = self.orientation.pixel_size(g);
        let [ox, oy] = self.orientation.cell_origin(g, x, y);
        let bottom = map_h - oy - th;
        ([ox, bottom], [ox + tw, bottom + th])
    }

    /// The tile cell `(x, y)` (row 0 = top) under a map-local world point (bottom-left origin,
    /// +Y up), or `None` outside the map. For the isometric, staggered and hexagonal orientations
    /// this is the cell whose **shape** (diamond or hexagon) holds the point.
    #[must_use]
    pub fn cell_at_world(&self, point: [f32; 2]) -> Option<(u32, u32)> {
        if !self.orientation.is_orthogonal() {
            let g = self.grid_size();
            let [_, map_h] = self.orientation.pixel_size(g);
            return self
                .orientation
                .cell_at(g, [f64::from(point[0]), map_h - f64::from(point[1])]);
        }
        // f64 keeps every column of a legal wide map (up to 64M cells) exact; f32 stops at 2^24.
        let cx = (f64::from(point[0]) / f64::from(self.tile_width)).floor();
        let cy_up = (f64::from(point[1]) / f64::from(self.tile_height)).floor();
        // `!(..)` also rejects NaN.
        if !(cx >= 0.0 && cy_up >= 0.0 && cx < f64::from(self.width) && cy_up < f64::from(self.height)) {
            return None;
        }
        Some((cx as u32, self.height - 1 - cy_up as u32))
    }

    /// The outline of tile `(x, y)` in map-local world space (+Y up). `map_h` is the map box
    /// height in pixels ([`Orientation::pixel_size`]'s second component), computed once by the
    /// caller for a whole overlay. 4 points (rectangle or diamond) or 6 (hexagon) live on the stack.
    #[must_use]
    pub fn tile_world_outline_points(&self, map_h: f64, x: u32, y: u32) -> WorldOutline {
        let outline = self.orientation.cell_outline_points(self.grid_size(), x, y);
        let mut points = [[0.0; 2]; 6];
        for (i, [px, py]) in outline.points.iter().take(outline.len).enumerate() {
            points[i] = [*px as f32, (map_h - py) as f32];
        }
        WorldOutline {
            points,
            len: outline.len,
        }
    }

    /// The outline of tile `(x, y)` in map-local world space (+Y up), for drawing grids and hover
    /// cells: 4 points (rectangle or diamond) or 6 (hexagon).
    ///
    /// Prefer [`Self::tile_world_outline_points`] when outlining more than one cell: this allocates
    /// a `Vec` and recomputes the map height.
    #[must_use]
    pub fn tile_world_outline(&self, x: u32, y: u32) -> Vec<[f32; 2]> {
        let map_h = self.orientation.pixel_size(self.grid_size())[1];
        let outline = self.tile_world_outline_points(map_h, x, y);
        outline.points[..outline.len].to_vec()
    }

    /// The center of tile `(x, y)` in map-local world space (+Y up).
    #[must_use]
    pub fn tile_world_center(&self, x: u32, y: u32) -> [f32; 2] {
        let g = self.grid_size();
        let [_, map_h] = self.orientation.pixel_size(g);
        let [cx, cy] = self.orientation.cell_center(g, x, y);
        [cx as f32, (map_h - cy) as f32]
    }

    /// Where a Tiled object sits in map-local world space (+Y up, origin at the map box's
    /// bottom-left): the object's `position` converted from Tiled's object space for this map's
    /// orientation ([`Orientation::object_to_map_pixels`]). Tile objects keep Tiled's anchor, the
    /// bottom-left of the tile image.
    #[must_use]
    pub fn object_world_position(&self, object: &MapObject) -> [f32; 2] {
        let g = self.grid_size();
        let [_, map_h] = self.orientation.pixel_size(g);
        let [x, y] = self
            .orientation
            .object_to_map_pixels(g, [f64::from(object.position[0]), f64::from(object.position[1])]);
        [x as f32, (map_h - y) as f32]
    }

    /// An all-empty collision grid, or `None` when the cell count is past the map cap (a map that
    /// skipped `validate`).
    fn empty_collision_grid(&self) -> Option<Vec<TileCollision>> {
        let cells = u64::from(self.width).checked_mul(u64::from(self.height))?;
        if cells > MAX_CELLS {
            return None;
        }
        Some(vec![TileCollision::Empty; usize::try_from(cells).ok()?])
    }

    /// The collision grid of one layer, stored **bottom row first**: data row `y` is written at
    /// index row `height - 1 - y`. On an orthogonal map that row is world y 0. On an isometric,
    /// staggered or hexagonal map it is only that index flip: two cells in the same data row can
    /// sit at different world bottoms, so a platformer samples [`Self::tile_world_rect`] instead.
    /// A map whose `width * height` is past the cell cap (64 × 1024 × 1024; one that skipped
    /// [`Self::validate`]) returns an empty vector. The OR over several layers is
    /// [`Self::collision_grid_all`].
    #[must_use]
    pub fn collision_grid(&self, layer: usize) -> Vec<TileCollision> {
        let Some(mut out) = self.empty_collision_grid() else {
            return Vec::new();
        };
        if self.tile_layer(layer).is_none() {
            return out;
        }
        for y in 0..self.height {
            for x in 0..self.width {
                if let Some(tile) = self.tile_at(layer, x, y) {
                    let up_row = self.height - 1 - y;
                    let index = u64::from(up_row) * u64::from(self.width) + u64::from(x);
                    if let Some(cell) = usize::try_from(index).ok().and_then(|i| out.get_mut(i)) {
                        *cell = tile.collision();
                    }
                }
            }
        }
        out
    }

    /// The collision grid of every tile layer combined (Solid wins over OneWay), with the same
    /// bottom-row-first index as [`Self::collision_grid`]. An oversized map returns an empty vector.
    #[must_use]
    pub fn collision_grid_all(&self) -> Vec<TileCollision> {
        let Some(mut out) = self.empty_collision_grid() else {
            return Vec::new();
        };
        for layer in (0..self.layers.len()).filter(|&l| self.tile_layer(l).is_some()) {
            for (cell, c) in out.iter_mut().zip(self.collision_grid(layer)) {
                *cell = match (*cell, c) {
                    (TileCollision::Solid, _) | (_, TileCollision::Solid) => TileCollision::Solid,
                    (TileCollision::OneWay, _) | (_, TileCollision::OneWay) => TileCollision::OneWay,
                    _ => TileCollision::Empty,
                };
            }
        }
        out
    }
}

/// Most cells (`width * height`) a map may have.
const MAX_CELLS: u64 = 64 * 1024 * 1024;

/// Most layers a map may have.
const MAX_LAYERS: usize = 256;

/// The `.tilemap.ron` migration chain. v1 → v2 only adds `Orientation` variants, so every v1
/// file already reads as a v2 map and the step changes nothing but the version
/// (`regia upgrade-project` rewrites the number).
fn tilemap_migrations() -> &'static [VersionStep<TileMapAsset>] {
    &[VersionStep {
        from: 1,
        to: 2,
        migrate: |_map| Ok(()),
    }]
}

/// The `from` versions of the `.tilemap.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn tilemap_migration_sources() -> Vec<u32> {
    tilemap_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.tilemap.ron` (hashed by `cargo xtask check-formats`): every
/// struct's field list and every enum's variant names.
#[must_use]
pub fn tilemap_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<TileMapAsset>(),
        serde_field_names::<TilesetDef>(),
        serde_field_names::<TileDef>(),
        serde_field_names::<TileFrame>(),
        serde_field_names::<TileLayer>(),
        serde_field_names::<ObjectLayer>(),
        serde_field_names::<MapObject>(),
        serde_variant_names::<Orientation>(),
        serde_variant_names::<StaggerAxis>(),
        serde_variant_names::<StaggerIndex>(),
        serde_variant_names::<PropertyValue>(),
        serde_variant_names::<TileCollision>(),
        serde_variant_names::<ObjectShape>(),
        serde_variant_names::<MapLayer>(),
    ])
}

/// A cooked tilemap: the map plus its tileset images (RGBA8, one per tileset, same order).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedTileMap {
    /// Payload version ([`COOKED_TILEMAP_VERSION`]).
    pub version: u32,
    /// The map.
    pub map: TileMapAsset,
    /// One image per tileset.
    pub images: Vec<RgbaImage>,
}

impl CookedTileMap {
    /// Encodes the cooked payload (postcard).
    pub fn to_postcard(&self) -> Result<Vec<u8>, TileMapError> {
        postcard::to_allocvec(self).map_err(|e| TileMapError::Decode(e.to_string()))
    }

    /// Decodes and validates a cooked payload.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, TileMapError> {
        let cooked: Self = postcard::from_bytes(bytes).map_err(|e| TileMapError::Decode(e.to_string()))?;
        if cooked.version != COOKED_TILEMAP_VERSION {
            return Err(TileMapError::Version {
                found: cooked.version,
                expected: COOKED_TILEMAP_VERSION,
            });
        }
        cooked.validate()?;
        Ok(cooked)
    }

    /// The map validates and there is one well-formed image per tileset.
    pub fn validate(&self) -> Result<(), TileMapError> {
        self.map.validate()?;
        if self.images.len() != self.map.tilesets.len() {
            return Err(TileMapError::Invalid(format!(
                "{} images for {} tilesets",
                self.images.len(),
                self.map.tilesets.len()
            )));
        }
        for (image, set) in self.images.iter().zip(&self.map.tilesets) {
            image
                .validate()
                .map_err(|e| TileMapError::Invalid(format!("tileset `{}`: {e}", set.name)))?;
            // The tiles must lie inside the image: the quads divide by its size and the sampler
            // clamps, so a tile past the edge would smear the last texel.
            if set.tile_count > 0 {
                let fits = |idx: u32| {
                    set.tile_rect(idx).is_some_and(|[x, y, w, h]| {
                        u64::from(x) + u64::from(w) <= u64::from(image.width)
                            && u64::from(y) + u64::from(h) <= u64::from(image.height)
                    })
                };
                let last_in_row = set.columns.min(set.tile_count) - 1;
                if !fits(set.tile_count - 1) || !fits(last_in_row) {
                    return Err(TileMapError::Invalid(format!(
                        "tileset `{}` has tiles outside its {}x{} image",
                        set.name, image.width, image.height
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unvalidated_huge_map_does_not_panic() {
        let mut m = map();
        m.width = 65_536;
        m.height = 65_536;
        m.layers = vec![MapLayer::Tiles(TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![1],
            properties: BTreeMap::new(),
        })];
        assert!(m.collision_grid(0).is_empty());
        assert!(m.collision_grid_all().is_empty());
    }

    #[test]
    fn tile_at_index_does_not_wrap() {
        let mut m = map();
        m.width = 3;
        m.height = 0x5555_5556;
        m.layers = vec![MapLayer::Tiles(TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![1],
            properties: BTreeMap::new(),
        })];
        assert!(m.tile_at(0, 1, m.height - 1).is_none());
    }

    /// `set_tile` on an unvalidated (code-built) map must index in u64 like `tile_at`: in u32
    /// the row offset wraps (concurrency audit 11-03 M2) — a debug panic, or a write to cell 0 in
    /// release.
    #[test]
    fn set_tile_index_does_not_wrap() {
        let mut m = map();
        m.width = 3;
        m.height = 0x5555_5556;
        m.layers = vec![MapLayer::Tiles(TileLayer {
            name: "g".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0],
            properties: BTreeMap::new(),
        })];
        // 3 * 0x5555_5555 + 1 = 2^32: wraps to cell 0 in u32.
        assert!(!m.set_tile(0, 1, m.height - 1, 1), "the far cell does not exist");
        assert_eq!(
            m.tile_layer(0).map(|l| l.data.clone()),
            Some(vec![0]),
            "cell 0 was overwritten"
        );
    }

    #[test]
    fn tile_world_rect_on_empty_map() {
        let mut m = map();
        m.height = 0;
        assert_eq!(m.tile_world_rect(0, 0).0[1], 0.0);
    }

    fn map() -> TileMapAsset {
        TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation: Orientation::Orthogonal,
            width: 3,
            height: 2,
            tile_width: 8,
            tile_height: 8,
            background: None,
            tilesets: vec![
                TilesetDef {
                    name: "a".into(),
                    first_gid: 1,
                    image: "a.png".into(),
                    image_width: 18,
                    image_height: 8,
                    tile_width: 8,
                    tile_height: 8,
                    columns: 2,
                    tile_count: 2,
                    spacing: 2,
                    margin: 0,
                    tiles: vec![TileDef {
                        id: 1,
                        collision: TileCollision::Solid,
                        ..TileDef::default()
                    }],
                },
                TilesetDef {
                    name: "b".into(),
                    first_gid: 10,
                    image: "b.png".into(),
                    image_width: 8,
                    image_height: 8,
                    tile_width: 8,
                    tile_height: 8,
                    columns: 1,
                    tile_count: 1,
                    spacing: 0,
                    margin: 0,
                    tiles: vec![TileDef {
                        id: 0,
                        collision: TileCollision::OneWay,
                        ..TileDef::default()
                    }],
                },
            ],
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0, 10, 0, 1, 2 | FLIP_X_BIT, 2],
                properties: BTreeMap::new(),
            })],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn tile_rect_overflow_gives_none() {
        let mut set = map().tilesets[0].clone();
        set.tile_count = u32::MAX;
        set.columns = 1;
        set.tile_height = u32::MAX;
        set.spacing = 0;
        assert!(set.tile_rect(0).is_some());
        assert_eq!(set.tile_rect(2), None);
        set.tile_width = u32::MAX;
        set.spacing = 1;
        set.columns = 4;
        assert_eq!(set.tile_rect(0), None);
    }

    #[test]
    fn cell_at_world_is_exact_on_maps_wider_than_f32_integers() {
        let mut m = map();
        m.width = 16_777_219;
        m.height = 1;
        m.tile_width = 1;
        m.tile_height = 1;
        m.layers.clear();
        assert_eq!(m.cell_at_world([16_777_218.5, 0.5]), Some((16_777_218, 0)));
        assert_eq!(m.cell_at_world([16_777_219.0, 0.5]), None);
        assert_eq!(m.cell_at_world([f32::NAN, 0.5]), None);
    }

    #[test]
    fn tile_world_rect_preserves_adjacent_cells_past_f32_precision() {
        let mut m = map();
        m.width = 20_000_000;
        m.height = 1;
        m.tile_width = 16;
        m.tile_height = 16;
        m.layers.clear();
        assert!(m.validate().is_ok());
        let (min, max) = m.tile_world_rect(16_777_217, 0);
        assert_eq!(min[0] - 268_435_456.0, 16.0);
        assert_eq!(max[0] - min[0], 16.0);
        assert_eq!(min[0] - m.tile_world_rect(16_777_216, 0).0[0], 16.0);

        m.orientation = Orientation::Isometric;
        assert!(m.validate().is_ok());
        let (min, max) = m.tile_world_rect(16_777_217, 0);
        assert_eq!(min[0] - 134_217_728.0, 8.0);
        assert_eq!(max[0] - min[0], 16.0);
    }

    #[test]
    fn gids_resolve_to_tilesets_and_flags() {
        let m = map();
        assert!(m.validate().is_ok());
        assert_eq!(m.tileset_for(1), Some((0, 0)));
        assert_eq!(m.tileset_for(10), Some((1, 0)));
        assert_eq!(m.tileset_for(3), None);
        let t = m.tile_at(0, 1, 1).expect("tile");
        assert_eq!((t.gid, t.local_id, t.flags.flip_x), (2, 1, true));
        assert_eq!(t.collision(), TileCollision::Solid);
        assert_eq!(m.tilesets[0].tile_rect(1), Some([10, 0, 8, 8]));
        assert!(m.tile_at(0, 0, 0).is_none());
    }

    #[test]
    fn object_layers_do_not_cost_a_grid_each() {
        let mut m = map();
        for i in 0..4 {
            m.layers.push(MapLayer::Objects(ObjectLayer {
                name: format!("o{i}"),
                visible: true,
                objects: Vec::new(),
                properties: BTreeMap::new(),
            }));
        }
        assert_eq!(m.collision_grid_all(), {
            let mut only = map();
            only.layers.truncate(1);
            only.collision_grid_all()
        });
    }

    #[test]
    fn layer_count_and_pixel_size_are_bounded() {
        let mut m = map();
        for i in 0..=MAX_LAYERS {
            m.layers.push(MapLayer::Objects(ObjectLayer {
                name: format!("o{i}"),
                visible: true,
                objects: Vec::new(),
                properties: BTreeMap::new(),
            }));
        }
        assert!(matches!(m.validate(), Err(TileMapError::Invalid(_))));
        let mut wide = map();
        wide.tile_width = u32::MAX;
        assert!(matches!(wide.validate(), Err(TileMapError::Invalid(_))));
        assert_eq!(wide.pixel_size()[0], u32::MAX);
    }

    #[test]
    fn collision_grid_is_bottom_row_first() {
        let grid = map().collision_grid(0);
        use TileCollision::*;
        assert_eq!(grid, vec![Empty, Solid, Solid, Empty, OneWay, Empty]);
    }

    #[test]
    fn isometric_world_rect_is_not_the_orthogonal_product() {
        let mut m = map();
        m.orientation = Orientation::Isometric;
        m.width = 2;
        m.height = 2;
        m.tile_width = 64;
        m.tile_height = 32;
        assert_eq!(m.pixel_size(), [128, 64]);
        // Cell (0, 1): map-pixel origin (0, 16), so the +Y-up box is not [0, 64] × [0, 32].
        assert_eq!(m.tile_world_rect(0, 1), ([0.0, 16.0], [64.0, 48.0]));
    }

    #[test]
    fn collision_grid_index_is_the_data_row_flip() {
        let mut m = map();
        m.orientation = Orientation::Isometric;
        m.width = 2;
        m.height = 2;
        m.tile_width = 64;
        m.tile_height = 32;
        if let MapLayer::Tiles(layer) = &mut m.layers[0] {
            // Data row y = 1, column 0 holds GID 2. In `map()` that is local id 1, the only
            // solid tile (GID 1 is local id 0 and has no collision def). Grid row 0 stores
            // that data row.
            layer.data = vec![0, 0, 2, 0];
        }
        let grid = m.collision_grid(0);
        assert_eq!(grid.len(), 4);
        assert_eq!(grid[0], TileCollision::Solid);
        assert_eq!(grid[1], TileCollision::Empty);
        assert_eq!(
            grid[2],
            TileCollision::Empty,
            "the solid cell is not left at data index 2"
        );
        let (min_a, _) = m.tile_world_rect(0, 1);
        let (min_b, _) = m.tile_world_rect(1, 1);
        assert_ne!(min_a[1], min_b[1], "the same data row is not one world-y row");
        let mut huge = map();
        huge.width = u32::MAX;
        huge.height = 2;
        assert!(huge.collision_grid(0).is_empty());
        assert!(huge.collision_grid_all().is_empty());
    }

    #[test]
    fn world_cells_and_rects_agree() {
        let m = map();
        assert_eq!(m.tile_world_rect(1, 0), ([8.0, 8.0], [16.0, 16.0]));
        assert_eq!(m.cell_at_world([9.0, 9.0]), Some((1, 0)));
        assert_eq!(m.cell_at_world([23.9, 0.5]), Some((2, 1)));
        assert_eq!(m.cell_at_world([-0.1, 0.5]), None);
    }

    #[test]
    fn validation_catches_bad_data() {
        let mut m = map();
        if let MapLayer::Tiles(t) = &mut m.layers[0] {
            t.data.pop();
        }
        assert!(m.validate().is_err());
        let mut m = map();
        if let MapLayer::Tiles(t) = &mut m.layers[0] {
            t.data[0] = 5;
        }
        assert!(m.validate().is_err());
        assert!(!map().set_tile(0, 0, 0, 7));
        let mut m = map();
        assert!(m.set_tile(0, 0, 0, 1 | FLIP_Y_BIT));
        assert!(m.tile_at(0, 0, 0).is_some_and(|t| t.flags.flip_y));
    }

    #[test]
    fn ron_round_trip_and_version_gate() {
        let m = map();
        let text = m.to_ron().expect("serializes");
        assert_eq!(TileMapAsset::from_ron(&text).expect("parses"), m);
        let newer = text.replacen(&format!("version: {TILEMAP_FORMAT_VERSION}"), "version: 9", 1);
        assert!(matches!(
            TileMapAsset::from_ron(&newer),
            Err(TileMapError::Version { found: 9, .. })
        ));
        let cooked = CookedTileMap {
            version: COOKED_TILEMAP_VERSION,
            map: m.clone(),
            images: vec![
                RgbaImage {
                    width: 18,
                    height: 8,
                    rgba8: vec![0; 18 * 8 * 4],
                },
                RgbaImage {
                    width: 8,
                    height: 8,
                    rgba8: vec![0; 8 * 8 * 4],
                },
            ],
        };
        let back = CookedTileMap::from_postcard(&cooked.to_postcard().expect("encodes")).expect("decodes");
        assert_eq!(back, cooked);
    }

    #[test]
    fn validate_rejects_tiles_outside_the_image_and_bad_animation_frames() {
        let img = |w: u32, h: u32| RgbaImage {
            width: w,
            height: h,
            rgba8: vec![0; (w * h * 4) as usize],
        };
        let cooked = |images: Vec<RgbaImage>, m: TileMapAsset| CookedTileMap {
            version: COOKED_TILEMAP_VERSION,
            map: m,
            images,
        };
        assert!(cooked(vec![img(18, 8), img(8, 8)], map()).validate().is_ok());
        // Tileset `a` needs 18x8; a 17-wide or 7-high image cuts a tile off.
        assert!(cooked(vec![img(17, 8), img(8, 8)], map()).validate().is_err());
        assert!(cooked(vec![img(18, 7), img(8, 8)], map()).validate().is_err());
        assert!(cooked(vec![img(18, 8), img(1, 1)], map()).validate().is_err());

        let mut missing_id = map();
        missing_id.tilesets[0].tiles[0].id = 2;
        assert!(matches!(missing_id.validate(), Err(TileMapError::Invalid(_))));
        let mut missing_frame = map();
        missing_frame.tilesets[1].tiles[0].animation = vec![TileFrame {
            tile: 5,
            duration_ms: 100,
        }];
        assert!(matches!(missing_frame.validate(), Err(TileMapError::Invalid(_))));
        let mut zero_ms = map();
        zero_ms.tilesets[1].tiles[0].animation = vec![TileFrame {
            tile: 0,
            duration_ms: 0,
        }];
        assert!(matches!(zero_ms.validate(), Err(TileMapError::Invalid(_))));
        let mut fine = map();
        fine.tilesets[1].tiles[0].animation = vec![TileFrame {
            tile: 0,
            duration_ms: 100,
        }];
        assert!(fine.validate().is_ok());
    }

    #[test]
    fn fingerprint_covers_nested_types() {
        let fp = tilemap_schema_fingerprint();
        for needle in [
            "tile_width",
            "first_gid",
            "duration_ms",
            "OneWay",
            "Polyline",
            "Objects",
        ] {
            assert!(fp.contains(needle), "{needle} missing from {fp}");
        }
    }
    #[test]
    fn pixel_math_overflow_is_rejected_or_saturates() {
        let mut huge = map();
        huge.tile_width = u32::MAX;
        huge.width = 2;
        assert!(huge.validate().is_err());
        assert_eq!(huge.pixel_size()[0], u32::MAX);
        let set = TilesetDef {
            name: "s".into(),
            first_gid: 1,
            image: "s.png".into(),
            image_width: 1,
            image_height: 1,
            tile_width: 1,
            tile_height: 100_000,
            columns: 1,
            tile_count: 100_000,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        };
        assert_eq!(set.tile_rect(50_000), None);
        let mut m = map();
        m.tilesets = vec![set];
        if let Some(MapLayer::Tiles(layer)) = m.layers.first_mut() {
            layer.data = vec![0; 6];
        }
        let err = m.validate().expect_err("oversized tileset").to_string();
        assert!(err.contains("32-bit"), "{err}");
    }

    #[test]
    fn cell_at_world_rejects_non_finite_points_and_wide_maps_stay_exact() {
        let m = map();
        assert_eq!(m.cell_at_world([f32::NAN, 0.0]), None);
        assert_eq!(m.cell_at_world([0.0, f32::INFINITY]), None);
        let mut wide = map();
        wide.width = 1;
        wide.height = 1;
        assert_eq!(wide.cell_at_world([f32::MAX, 0.0]), None);
    }

    #[test]
    fn validate_rejects_oversized_shapes_animations_and_strings() {
        let mut m = map();
        m.layers.push(MapLayer::Objects(ObjectLayer {
            name: "o".into(),
            visible: true,
            objects: vec![MapObject {
                id: 1,
                name: String::new(),
                class: String::new(),
                position: [0.0; 2],
                size: [0.0; 2],
                rotation: 0.0,
                shape: ObjectShape::Polygon(vec![[0.0; 2]; MAX_SHAPE_POINTS + 1]),
                properties: BTreeMap::new(),
            }],
            properties: BTreeMap::new(),
        }));
        assert!(matches!(m.validate(), Err(TileMapError::Invalid(_))));

        let mut m = map();
        m.tilesets[0].tiles[0].animation = vec![
            TileFrame {
                tile: 0,
                duration_ms: 1
            };
            MAX_ANIMATION_FRAMES + 1
        ];
        assert!(matches!(m.validate(), Err(TileMapError::Invalid(_))));

        let mut m = map();
        m.properties.insert(
            "k".into(),
            PropertyValue::String("x".repeat(MAX_PROPERTY_STRING + 1)),
        );
        assert!(matches!(m.validate(), Err(TileMapError::Invalid(_))));

        let mut ok = map();
        ok.properties
            .insert("k".into(), PropertyValue::String("x".repeat(MAX_PROPERTY_STRING)));
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn solid_beats_one_way_and_bad_writes_and_cooked_versions_are_rejected() {
        use TileCollision::{Empty, OneWay, Solid};
        let mut m = map();
        assert_eq!(m.migrate_to_latest().expect("current"), TILEMAP_FORMAT_VERSION);
        m.layers.push(MapLayer::Tiles(TileLayer {
            name: "overlay".into(),
            visible: true,
            opacity: 1.0,
            offset: [0.0, 0.0],
            data: vec![0, 2, 0, 10, 10, 0],
            properties: BTreeMap::new(),
        }));
        m.layers.push(MapLayer::Objects(ObjectLayer {
            name: "spawns".into(),
            visible: true,
            objects: Vec::new(),
            properties: BTreeMap::new(),
        }));
        assert!(m.validate().is_ok());
        assert_eq!(
            m.collision_grid_all(),
            vec![OneWay, Solid, Solid, Empty, Solid, Empty]
        );
        assert_eq!(m.layer_index("ground"), Some(0));
        assert_eq!(m.layer_index("overlay"), Some(1));
        assert_eq!(m.layer_index("spawns"), Some(2));
        assert_eq!(m.layer_index("Ground"), None);
        assert!(m.tile_layer(2).is_none());
        assert!(m.tile_layer(9).is_none());
        assert_eq!(m.collision_grid(2), vec![Empty; 6]);
        assert_eq!(m.collision_grid(9), vec![Empty; 6]);
        assert_eq!(m.layers[2].name(), "spawns");

        assert_eq!(m.tile_world_rect(1, 99), m.tile_world_rect(1, 1));
        assert_eq!(m.tile_world_rect(1, 1), ([8.0, 0.0], [16.0, 8.0]));
        assert_eq!(m.cell_at_world([0.0, 0.0]), Some((0, 1)));
        assert_eq!(m.cell_at_world([0.0, 8.0]), Some((0, 0)));
        assert_eq!(m.cell_at_world([8.0, 8.0]), Some((1, 0)));
        assert_eq!(m.cell_at_world([24.0, 0.0]), None);
        assert_eq!(m.cell_at_world([0.0, 16.0]), None);

        assert!(!m.set_tile(2, 0, 0, 1));
        assert!(!m.set_tile(9, 0, 0, 1));
        assert!(!m.set_tile(0, 3, 0, 1));
        assert!(!m.set_tile(0, 0, 2, 1));
        assert!(m.set_tile(0, 0, 0, FLIP_X_BIT));
        assert!(m.tile_at(0, 0, 0).is_none());
        assert_eq!(m.tile_layer(0).expect("ground").data[0], FLIP_X_BIT);
        assert!(m.set_tile(0, 1, 1, 0));
        assert!(m.tile_at(0, 1, 1).is_none());
        assert!(m.set_tile(0, 1, 0, 10 | FLIP_DIAGONAL_BIT | FLIP_Y_BIT | ROTATE_HEX_BIT));
        let tile = m.tile_at(0, 1, 0).expect("flagged");
        assert_eq!(tile.gid, 10);
        assert!(!tile.flags.flip_x);
        assert!(tile.flags.flip_y);
        assert!(tile.flags.diagonal);
        assert_eq!(tile.def().map(|d| d.collision), Some(OneWay));

        let mut short = map();
        if let MapLayer::Tiles(layer) = &mut short.layers[0] {
            layer.data.pop();
        }
        assert!(!short.set_tile(0, 2, 1, 1));
        assert!(short.set_tile(0, 0, 0, 1));

        let mut overlap = map();
        overlap.tilesets[1].first_gid = 2;
        assert!(matches!(overlap.validate(), Err(TileMapError::Invalid(_))));
        let mut unsorted = map();
        unsorted.tilesets[0].tiles.push(TileDef {
            id: 0,
            ..TileDef::default()
        });
        assert!(matches!(unsorted.validate(), Err(TileMapError::Invalid(_))));
        let mut zero_cols = map();
        zero_cols.tilesets[0].columns = 0;
        assert!(matches!(zero_cols.validate(), Err(TileMapError::Invalid(_))));
        let mut long_key = map();
        long_key
            .properties
            .insert("k".repeat(MAX_PROPERTY_STRING + 1), PropertyValue::Bool(true));
        assert!(matches!(long_key.validate(), Err(TileMapError::Invalid(_))));

        let mut old = map();
        old.version = 0;
        assert!(matches!(old.migrate_to_latest(), Err(TileMapError::Migration(_))));
        assert_eq!(
            tilemap_migration_sources(),
            vec![1],
            "v1 -> v2 (orientations) is the only step"
        );

        let mut cooked = CookedTileMap {
            version: 0,
            map: map(),
            images: vec![
                RgbaImage {
                    width: 18,
                    height: 8,
                    rgba8: vec![0; 18 * 8 * 4],
                },
                RgbaImage {
                    width: 8,
                    height: 8,
                    rgba8: vec![0; 8 * 8 * 4],
                },
            ],
        };
        assert!(matches!(
            CookedTileMap::from_postcard(&cooked.to_postcard().expect("encodes")),
            Err(TileMapError::Version {
                found: 0,
                expected: COOKED_TILEMAP_VERSION
            })
        ));
        cooked.version = 2;
        assert!(matches!(
            CookedTileMap::from_postcard(&cooked.to_postcard().expect("encodes")),
            Err(TileMapError::Version {
                found: 2,
                expected: COOKED_TILEMAP_VERSION
            })
        ));
        cooked.version = COOKED_TILEMAP_VERSION;
        cooked.images.pop();
        assert!(matches!(cooked.validate(), Err(TileMapError::Invalid(_))));
        assert!(matches!(
            CookedTileMap::from_postcard(&[0]),
            Err(TileMapError::Decode(_))
        ));
    }
}

#[cfg(test)]
mod format_doc_tests {
    use super::*;

    #[test]
    fn the_published_example_parses_and_validates() {
        let text = include_str!("../../../docs/formats/tilemap.example.ron");
        let map = TileMapAsset::from_ron(text).expect("docs/formats/tilemap.example.ron parses");
        assert_eq!(
            map.tile_at(0, 5, 2).map(|t| (t.local_id, t.flags.flip_x)),
            Some((1, true))
        );
        assert_eq!(
            map.tile_at(0, 1, 1).map(|t| t.collision()),
            Some(TileCollision::OneWay)
        );
    }
}

#[cfg(test)]
mod orientation_tests {
    use super::*;

    fn base(orientation: Orientation) -> TileMapAsset {
        TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation,
            width: 3,
            height: 2,
            tile_width: 64,
            tile_height: 32,
            background: None,
            tilesets: Vec::new(),
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "g".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; 6],
                properties: BTreeMap::new(),
            })],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn every_orientation_round_trips_through_ron_and_validates() {
        for o in [
            Orientation::Orthogonal,
            Orientation::Isometric,
            Orientation::Staggered {
                axis: StaggerAxis::X,
                index: StaggerIndex::Even,
            },
            Orientation::Hexagonal {
                axis: StaggerAxis::Y,
                index: StaggerIndex::Odd,
                side_length: 16,
            },
        ] {
            let map = base(o);
            let text = map.to_ron().expect("serializes");
            assert_eq!(TileMapAsset::from_ron(&text).expect("parses"), map, "{o:?}");
        }
    }

    #[test]
    fn a_v1_file_loads_as_v2() {
        let text = "(version: 1, width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [Tiles((name: \"g\", data: [0]))])";
        let map = TileMapAsset::from_ron(text).expect("v1 reads");
        assert_eq!(map.version, TILEMAP_FORMAT_VERSION);
        assert_eq!(map.orientation, Orientation::Orthogonal);
        assert_eq!(tilemap_migration_sources(), vec![1]);
        // A v2 file with an orientation the v1 format never had.
        let iso = "(version: 2, orientation: Isometric, width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [Tiles((name: \"g\", data: [0]))])";
        assert_eq!(
            TileMapAsset::from_ron(iso).expect("v2").orientation,
            Orientation::Isometric
        );
        let hex = "(version: 2, orientation: Hexagonal(axis: X, index: Even, side_length: 4), width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [Tiles((name: \"g\", data: [0]))])";
        assert_eq!(
            TileMapAsset::from_ron(hex).expect("hex").orientation,
            Orientation::Hexagonal {
                axis: StaggerAxis::X,
                index: StaggerIndex::Even,
                side_length: 4
            }
        );
    }

    #[test]
    fn the_hexagon_side_length_must_fit_its_axis() {
        let ok = base(Orientation::Hexagonal {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Odd,
            side_length: 32,
        });
        assert!(ok.validate().is_ok());
        let too_long = base(Orientation::Hexagonal {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Odd,
            side_length: 33,
        });
        assert!(too_long.validate().is_err());
        // Along X the limit is the tile width.
        let flat = base(Orientation::Hexagonal {
            axis: StaggerAxis::X,
            index: StaggerIndex::Odd,
            side_length: 64,
        });
        assert!(flat.validate().is_ok());
    }

    #[test]
    fn pixel_size_rounds_outward_to_cover_fractional_cell_boxes() {
        for (orientation, expected) in [
            (Orientation::Isometric, [23, 23]),
            (
                Orientation::Staggered {
                    axis: StaggerAxis::X,
                    index: StaggerIndex::Even,
                },
                [23, 23],
            ),
            (
                Orientation::Hexagonal {
                    axis: StaggerAxis::X,
                    index: StaggerIndex::Even,
                    side_length: 6,
                },
                [26, 23],
            ),
        ] {
            let mut map = base(orientation);
            map.width = 2;
            map.height = 1;
            map.tile_width = 15;
            map.tile_height = 15;
            map.layers.clear();
            assert!(map.validate().is_ok());
            assert_eq!(map.pixel_size(), expected, "{orientation:?}");
        }
    }

    #[test]
    fn world_conversions_flip_y_and_agree_with_the_grid() {
        // Isometric 3 x 2 of 64 x 32: the box is 160 x 80; cell (0, 0) is the top diamond.
        let map = base(Orientation::Isometric);
        assert_eq!(map.pixel_size(), [160, 80]);
        let (min, max) = map.tile_world_rect(0, 0);
        assert_eq!(min, [32.0, 48.0]);
        assert_eq!(max, [96.0, 80.0]);
        assert_eq!(map.tile_world_center(0, 0), [64.0, 64.0]);
        assert_eq!(map.cell_at_world([64.0, 64.0]), Some((0, 0)));
        // The bottom diamond of the 3 x 2 grid is cell (2, 1); its center is at pixel (96, 64) down.
        let centre = map.tile_world_center(2, 1);
        assert_eq!(centre, [96.0, 80.0 - 64.0]);
        assert_eq!(map.cell_at_world(centre), Some((2, 1)));
        // Outside the box, and in the empty corner above the left diamond.
        assert_eq!(map.cell_at_world([-1.0, 40.0]), None);
        assert_eq!(map.cell_at_world([2.0, 78.0]), None);
        // Four outline points per diamond, six per hexagon.
        assert_eq!(map.tile_world_outline(1, 1).len(), 4);
        let map_h = map.orientation.pixel_size(map.grid_size())[1];
        let stacked = map.tile_world_outline_points(map_h, 1, 1);
        assert_eq!(stacked.len, 4);
        assert_eq!(
            &stacked.points[..stacked.len],
            map.tile_world_outline(1, 1).as_slice()
        );
        let hex = base(Orientation::Hexagonal {
            axis: StaggerAxis::Y,
            index: StaggerIndex::Odd,
            side_length: 16,
        });
        assert_eq!(hex.tile_world_outline(1, 1).len(), 6);
        let hex_h = hex.orientation.pixel_size(hex.grid_size())[1];
        let hex_stacked = hex.tile_world_outline_points(hex_h, 1, 1);
        assert_eq!(hex_stacked.len, 6);
        assert_eq!(
            &hex_stacked.points[..hex_stacked.len],
            hex.tile_world_outline(1, 1).as_slice()
        );
        // The orthogonal path is unchanged: row 0 at the top.
        let ortho = base(Orientation::Orthogonal);
        assert_eq!(ortho.tile_world_rect(0, 0), ([0.0, 32.0], [64.0, 64.0]));
        assert_eq!(ortho.cell_at_world([10.0, 40.0]), Some((0, 0)));
    }
}

// ---- Layer and object-layer edits (the editor's 2D layer tree, ADR-7853) -------------------

/// Why a layer or object edit was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayerEditError {
    /// No layer at that index.
    #[error("no layer {0}")]
    NoLayer(usize),
    /// The layer is a tile layer, not an object layer.
    #[error("layer {0} is not an object layer")]
    NotObjectLayer(usize),
    /// No object with that id on the layer.
    #[error("layer {layer} has no object {id}")]
    NoObject {
        /// The layer.
        layer: usize,
        /// The object id.
        id: u32,
    },
    /// A position, size or rotation is not finite.
    #[error("object values must be finite")]
    NotFinite,
}

impl TileMapAsset {
    /// The object layer at `layer`.
    #[must_use]
    pub fn object_layer(&self, layer: usize) -> Option<&ObjectLayer> {
        match self.layers.get(layer)? {
            MapLayer::Objects(o) => Some(o),
            MapLayer::Tiles(_) => None,
        }
    }

    fn object_layer_mut(&mut self, layer: usize) -> Result<&mut ObjectLayer, LayerEditError> {
        match self.layers.get_mut(layer) {
            Some(MapLayer::Objects(o)) => Ok(o),
            Some(MapLayer::Tiles(_)) => Err(LayerEditError::NotObjectLayer(layer)),
            None => Err(LayerEditError::NoLayer(layer)),
        }
    }

    /// One more than the largest object id on any layer (ids are unique per map).
    #[must_use]
    pub fn next_object_id(&self) -> u32 {
        self.layers
            .iter()
            .filter_map(|l| match l {
                MapLayer::Objects(o) => o.objects.iter().map(|ob| ob.id).max(),
                MapLayer::Tiles(_) => None,
            })
            .max()
            .map_or(1, |id| id.saturating_add(1))
    }

    /// Adds `object` to object layer `layer` with a fresh id (its own `id` is replaced). Returns
    /// the id.
    pub fn add_object(&mut self, layer: usize, mut object: MapObject) -> Result<u32, LayerEditError> {
        check_object(&object)?;
        let id = self.next_object_id();
        object.id = id;
        self.object_layer_mut(layer)?.objects.push(object);
        Ok(id)
    }

    /// Replaces object `id` of layer `layer` with `object` (the id is kept). Returns the old one.
    pub fn replace_object(
        &mut self,
        layer: usize,
        id: u32,
        mut object: MapObject,
    ) -> Result<MapObject, LayerEditError> {
        check_object(&object)?;
        object.id = id;
        let objects = &mut self.object_layer_mut(layer)?.objects;
        let slot = objects
            .iter_mut()
            .find(|o| o.id == id)
            .ok_or(LayerEditError::NoObject { layer, id })?;
        Ok(std::mem::replace(slot, object))
    }

    /// Removes object `id` from layer `layer`; returns its list index and the object.
    pub fn remove_object(&mut self, layer: usize, id: u32) -> Result<(usize, MapObject), LayerEditError> {
        let objects = &mut self.object_layer_mut(layer)?.objects;
        let index = objects
            .iter()
            .position(|o| o.id == id)
            .ok_or(LayerEditError::NoObject { layer, id })?;
        Ok((index, objects.remove(index)))
    }

    /// Moves the layer at `from` to depth `to` (the layer list is bottom first, so the index is
    /// the draw depth). Returns false when either index is out of range.
    pub fn move_layer(&mut self, from: usize, to: usize) -> bool {
        if from >= self.layers.len() || to >= self.layers.len() {
            return false;
        }
        let layer = self.layers.remove(from);
        self.layers.insert(to, layer);
        true
    }
}

fn check_object(object: &MapObject) -> Result<(), LayerEditError> {
    let finite =
        object.position.iter().chain(&object.size).all(|v| v.is_finite()) && object.rotation.is_finite();
    if finite {
        Ok(())
    } else {
        Err(LayerEditError::NotFinite)
    }
}

/// The index a layer at `index` has after [`TileMapAsset::move_layer`]`(from, to)`.
#[must_use]
pub fn layer_index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

#[cfg(test)]
mod layer_edit_tests {
    use super::*;

    fn object(name: &str) -> MapObject {
        MapObject {
            id: 0,
            name: name.into(),
            class: "spawn".into(),
            position: [8.0, 16.0],
            size: [16.0, 16.0],
            rotation: 0.0,
            shape: ObjectShape::Rect,
            properties: BTreeMap::new(),
        }
    }

    fn map() -> TileMapAsset {
        let tiles = |name: &str| {
            MapLayer::Tiles(TileLayer {
                name: name.into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; 4],
                properties: BTreeMap::new(),
            })
        };
        TileMapAsset {
            version: TILEMAP_FORMAT_VERSION,
            orientation: Orientation::Orthogonal,
            width: 2,
            height: 2,
            tile_width: 16,
            tile_height: 16,
            background: None,
            tilesets: Vec::new(),
            layers: vec![
                tiles("ground"),
                MapLayer::Objects(ObjectLayer {
                    name: "spawns".into(),
                    visible: true,
                    objects: Vec::new(),
                    properties: BTreeMap::new(),
                }),
                tiles("top"),
            ],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn objects_get_fresh_ids_and_edit_and_remove_by_id() {
        let mut map = map();
        assert_eq!(map.next_object_id(), 1);
        assert_eq!(map.add_object(1, object("a")), Ok(1));
        assert_eq!(map.add_object(1, object("b")), Ok(2));
        assert_eq!(
            map.add_object(0, object("c")),
            Err(LayerEditError::NotObjectLayer(0))
        );
        assert_eq!(map.add_object(9, object("c")), Err(LayerEditError::NoLayer(9)));
        let mut bad = object("nan");
        bad.position[0] = f32::NAN;
        assert_eq!(map.add_object(1, bad), Err(LayerEditError::NotFinite));
        let mut moved = object("a2");
        moved.position = [32.0, 0.0];
        let old = map.replace_object(1, 1, moved).expect("replaced");
        assert_eq!(old.name, "a");
        assert_eq!(
            map.object_layer(1).map(|l| l.objects[0].position),
            Some([32.0, 0.0])
        );
        assert_eq!(
            map.object_layer(1).map(|l| l.objects[0].id),
            Some(1),
            "the id stays"
        );
        assert_eq!(
            map.remove_object(1, 1).map(|(i, o)| (i, o.name)),
            Ok((0, "a2".into()))
        );
        assert_eq!(
            map.remove_object(1, 1),
            Err(LayerEditError::NoObject { layer: 1, id: 1 })
        );
        assert_eq!(map.next_object_id(), 3, "ids grow past the largest one left");
        map.validate().expect("still a valid map");
    }

    #[test]
    fn moving_a_layer_changes_its_depth_and_shifts_the_others() {
        let mut map = map();
        assert!(map.move_layer(0, 2));
        let names: Vec<&str> = map.layers.iter().map(MapLayer::name).collect();
        assert_eq!(names, ["spawns", "top", "ground"]);
        assert!(!map.move_layer(0, 3));
        for (index, after) in [(0, 2), (1, 0), (2, 1)] {
            assert_eq!(layer_index_after_move(index, 0, 2), after);
        }
        for (index, after) in [(0, 1), (1, 2), (2, 0)] {
            assert_eq!(layer_index_after_move(index, 2, 0), after);
        }
        assert_eq!(layer_index_after_move(1, 1, 1), 1);
    }
}
