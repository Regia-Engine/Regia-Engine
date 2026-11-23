// SPDX-License-Identifier: MPL-2.0
//! Auto-tiling rules (`docs/specs/86-2d-toolkit.md`, ADR-1889): the frozen `.autotile.ron`
//! format ([`AutoTileSet`]; v2 since ADR-2597, v1 files load through the migration) and the
//! painter that turns "this cell is grass" into the right edge, corner or inner tile.
//!
//! A rule set is bound to one tileset of a map by name ([`AutoTileSet::bind`]). Each [`Terrain`]
//! owns a set of member tiles (every tile its layout can output, its variants and an optional
//! `members` list); a cell belongs to a terrain when its tile is one of the members. Resolving a
//! cell looks at which of its 8 neighbours belong to the same terrain (row 0 is the **top** row,
//! so north is `y − 1`) and picks the tile from the [`Layout`]:
//!
//! - [`Layout::Edge16`]: 4-bit mask, N = 1, E = 2, S = 4, W = 8; the index is the mask
//!   ([`edge16_mask`]).
//! - [`Layout::Blob47`]: 8-bit mask clockwise from north (N = 1, NE = 2, E = 4, SE = 8, S = 16,
//!   SW = 32, W = 64, NW = 128) with a corner kept only when both edges next to it are set; the
//!   index is the reduced mask's rank among the 47 reduced masks ([`blob47_index`]).
//! - [`Layout::WangCorner`]: 4-bit mask of which cell **corners** lie inside the terrain (TL = 1,
//!   TR = 2, BR = 4, BL = 8; a corner is inside when the cell and the three others touching it
//!   all belong to the terrain); the index is the mask.
//! - [`Layout::Rules`]: first matching 3×3 [`PatternRule`].
//!
//! [`BoundAutoTiles::paint`] writes a member (or `0` for erase) into the painted cells and
//! re-resolves them and their neighbours. Resolution reads only membership and every output is a
//! member of the same terrain, so the result is independent of cell and stroke order. Variant picks
//! are a hash of `(seed, x, y, tile)`: integer math only, identical on every platform.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use regia_app::prelude::*;
use regia_core::version::{self, VersionStep};

use crate::tilemap::{
    FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, Orientation, TileMapAsset, split_gid,
};

/// Current `.autotile.ron` version (v2 adds [`Layout::WangCorner`], ADR-2597).
pub const AUTOTILE_FORMAT_VERSION: u32 = 2;

/// Most terrains one rule set may hold.
pub const MAX_TERRAINS: usize = 256;
/// Most pattern rules one terrain may hold.
pub const MAX_RULES: usize = 1024;
/// Most alternatives one variant entry may hold.
pub const MAX_VARIANTS: usize = 64;
/// Largest variant weight.
pub const MAX_WEIGHT: u32 = 65_535;
/// Longest terrain or tileset name (bytes).
const MAX_NAME: usize = 256;
/// Longest pattern string a file may hold (bytes, including spacing). A file-level bound only:
/// `compile_pattern` itself counts cells, not spacing.
const MAX_PATTERN: usize = 64;
/// Most extra `members` or variant keys one terrain may list.
const MAX_MEMBER_LIST: usize = 65_536;

/// Auto-tiling errors. [`AutoTileError::code`] gives the stable agent error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutoTileError {
    /// The file is a newer version than this build reads.
    #[error("autotile version {found} is newer than supported version {expected}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("autotile parse error: {0}")]
    Parse(String),
    /// Semantic error in the rule set.
    #[error("invalid autotile rules: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("autotile migration failed: {0}")]
    Migration(String),
    /// The rule set does not fit the map: the tileset is missing, a tile id is out of range, or a
    /// tile layer's data is not `width × height` cells long (all share the code `tile_bind_failed`).
    #[error("autotile rules do not fit the map: {0}")]
    Bind(String),
    /// The layer index is not a tile layer.
    #[error("layer {0} is not a tile layer")]
    NotTileLayer(usize),
    /// A cell lies outside the map.
    #[error("cell ({x}, {y}) is outside the map")]
    OutsideMap {
        /// Column.
        x: u32,
        /// Row (0 = top).
        y: u32,
    },
    /// The terrain index does not exist in the rule set.
    #[error("terrain {0} does not exist")]
    UnknownTerrain(usize),
}

impl AutoTileError {
    /// The stable error code (`data.code` on the agent protocol, `book/agents/2d.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Version { .. } | Self::Parse(_) | Self::Invalid(_) | Self::Migration(_) => "tile_bad_rules",
            Self::Bind(_) => "tile_bind_failed",
            Self::NotTileLayer(_) => "tile_not_tile_layer",
            Self::OutsideMap { .. } => "tile_outside_map",
            Self::UnknownTerrain(_) => "tile_unknown_terrain",
        }
    }
}

/// How cells outside the map count when resolving a cell on the map's edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Border {
    /// As the same terrain: a terrain runs off the edge without edge tiles.
    #[default]
    Same,
    /// As empty: the map boundary gets edge tiles.
    Empty,
}

/// One 3×3 pattern rule of a [`Layout::Rules`] terrain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternRule {
    /// Nine cells, row by row, top row first; whitespace and `/` are ignored (any amount). `#` = the neighbour
    /// belongs to this terrain, `.` = it does not, `?` = either. The centre is `#` or `?`.
    pub pattern: String,
    /// Local tile id written when the rule matches.
    pub tile: u32,
    /// Mirror horizontally.
    #[serde(default)]
    pub flip_x: bool,
    /// Mirror vertically.
    #[serde(default)]
    pub flip_y: bool,
    /// Mirror along the diagonal (applied first, as in `.tilemap.ron`).
    #[serde(default)]
    pub diagonal: bool,
}

/// A weighted alternative of a variant entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeightedTile {
    /// Local tile id.
    pub tile: u32,
    /// Relative weight, 1 ..= [`MAX_WEIGHT`].
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Mirror horizontally.
    #[serde(default)]
    pub flip_x: bool,
    /// Mirror vertically.
    #[serde(default)]
    pub flip_y: bool,
    /// Mirror along the diagonal.
    #[serde(default)]
    pub diagonal: bool,
}

fn default_weight() -> u32 {
    1
}

/// How a terrain picks a tile from its neighbourhood.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    /// 16 local ids indexed by the 4-bit edge mask (N = 1, E = 2, S = 4, W = 8).
    Edge16(Vec<u32>),
    /// 47 local ids indexed by [`blob47_index`] of the 8-bit mask.
    Blob47(Vec<u32>),
    /// First matching rule wins.
    Rules(Vec<PatternRule>),
    /// 16 local ids indexed by the 4-bit corner mask (top-left = 1, top-right = 2, bottom-right =
    /// 4, bottom-left = 8): a corner counts when the cell's two edge neighbours and the diagonal
    /// neighbour at that corner are all the same terrain.
    WangCorner(Vec<u32>),
}

impl Layout {
    /// Stable layout name (`edge16`, `blob47`, `rules`, `wang_corner`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Edge16(_) => "edge16",
            Self::Blob47(_) => "blob47",
            Self::Rules(_) => "rules",
            Self::WangCorner(_) => "wang_corner",
        }
    }
}

/// One terrain of a rule set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terrain {
    /// Name (unique in the rule set).
    pub name: String,
    /// How the tile is chosen.
    pub layout: Layout,
    /// [`Layout::Rules`] only: the tile when no rule matches (default: the first rule's tile).
    #[serde(default)]
    pub fallback: Option<u32>,
    /// Extra local ids that count as this terrain (hand-placed decorations).
    #[serde(default)]
    pub members: Vec<u32>,
    /// Weighted replacements of a resolved tile, keyed by its local id. A variant replaces the
    /// tile and its flips.
    #[serde(default)]
    pub variants: BTreeMap<u32, Vec<WeightedTile>>,
}

impl Terrain {
    /// Every member local id, sorted and deduplicated.
    #[must_use]
    pub fn member_tiles(&self) -> Vec<u32> {
        let mut out: BTreeSet<u32> = self.members.iter().copied().collect();
        match &self.layout {
            Layout::Edge16(tiles) | Layout::Blob47(tiles) | Layout::WangCorner(tiles) => {
                out.extend(tiles.iter().copied());
            }
            Layout::Rules(rules) => out.extend(rules.iter().map(|r| r.tile)),
        }
        out.extend(self.fallback);
        for (key, alts) in &self.variants {
            out.insert(*key);
            out.extend(alts.iter().map(|a| a.tile));
        }
        out.into_iter().collect()
    }

    /// The tile a freshly painted cell holds before it is resolved (the layout's first output).
    fn base_tile(&self) -> Option<u32> {
        match &self.layout {
            Layout::Edge16(tiles) | Layout::Blob47(tiles) | Layout::WangCorner(tiles) => {
                tiles.first().copied()
            }
            Layout::Rules(rules) => rules.first().map(|r| r.tile).or(self.fallback),
        }
    }
}

/// The `.autotile.ron` document.
///
/// ```
/// use regia_2d::autotile::AutoTileSet;
///
/// let set = AutoTileSet::from_ron(r#"(
///     version: 1,
///     tileset: "terrain",
///     terrains: [(name: "wall", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))],
/// )"#).expect("valid");
/// assert_eq!(set.terrain_index("wall"), Some(0));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoTileSet {
    /// Format version ([`AUTOTILE_FORMAT_VERSION`]).
    pub version: u32,
    /// Name of the tileset (in the map) the local ids refer to.
    pub tileset: String,
    /// Seed of the variant picks.
    #[serde(default)]
    pub seed: u32,
    /// How cells outside the map count.
    #[serde(default)]
    pub border: Border,
    /// The terrains.
    pub terrains: Vec<Terrain>,
}

/// A neighbour condition of a compiled pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cond {
    Same,
    Other,
    Any,
}

/// Parses a pattern string into its nine conditions.
fn compile_pattern(pattern: &str) -> Result<[Cond; 9], String> {
    let mut out = [Cond::Any; 9];
    let mut n = 0;
    for c in pattern.chars().filter(|c| !c.is_whitespace() && *c != '/') {
        let cond = match c {
            '#' => Cond::Same,
            '.' => Cond::Other,
            '?' => Cond::Any,
            other => return Err(format!("pattern `{pattern}` has `{other}` (use #, . or ?)")),
        };
        if n == 9 {
            return Err(format!("pattern `{pattern}` has more than 9 cells"));
        }
        out[n] = cond;
        n += 1;
    }
    if n != 9 {
        return Err(format!("pattern `{pattern}` has {n} cells, expected 9"));
    }
    if out[4] == Cond::Other {
        return Err(format!("pattern `{pattern}`: the centre cell must be # or ?"));
    }
    Ok(out)
}

const fn reduce_blob(mask: u8) -> u8 {
    let n = mask & 1 != 0;
    let e = mask & 4 != 0;
    let s = mask & 16 != 0;
    let w = mask & 64 != 0;
    let mut out = mask & (1 | 4 | 16 | 64);
    if n && e {
        out |= mask & 2;
    }
    if s && e {
        out |= mask & 8;
    }
    if s && w {
        out |= mask & 32;
    }
    if n && w {
        out |= mask & 128;
    }
    out
}

/// The 47 reduced blob masks in ascending order (a compile error if there were not 47).
const BLOB47_MASKS: [u8; 47] = {
    let mut out = [0u8; 47];
    let mut n = 0;
    let mut m = 0u32;
    while m < 256 {
        if reduce_blob(m as u8) == m as u8 {
            out[n] = m as u8;
            n += 1;
        }
        m += 1;
    }
    assert!(n == 47);
    out
};

/// Index into [`BLOB47_MASKS`] of every reduced mask.
const BLOB47_INDEX: [u8; 256] = {
    let mut out = [0u8; 256];
    let mut i = 0;
    while i < 47 {
        out[BLOB47_MASKS[i] as usize] = i as u8;
        i += 1;
    }
    out
};

/// Drops every corner bit whose two neighbouring edge bits are not both set.
#[must_use]
pub const fn reduce_blob_mask(mask: u8) -> u8 {
    reduce_blob(mask)
}

/// The 47 reduced 8-bit masks in ascending order: entry `i` is the neighbourhood
/// [`Layout::Blob47`]'s tile `i` is drawn for.
#[must_use]
pub const fn blob47_masks() -> &'static [u8; 47] {
    &BLOB47_MASKS
}

/// The [`Layout::Edge16`] index for which sides have a same-terrain neighbour: north = bit 0,
/// east = bit 1, south = bit 2, west = bit 3 (north is `y − 1`).
#[must_use]
pub const fn edge16_mask(north: bool, east: bool, south: bool, west: bool) -> usize {
    (north as usize) | ((east as usize) << 1) | ((south as usize) << 2) | ((west as usize) << 3)
}

/// The [`Layout::Blob47`] index of an 8-bit neighbour mask (reduced first).
#[must_use]
pub const fn blob47_index(mask: u8) -> usize {
    BLOB47_INDEX[reduce_blob(mask) as usize] as usize
}

/// SplitMix64's output function (Steele, Lea and Flood, OOPSLA 2014).
const fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The variant hash of a cell: a pure function of the seed, the cell and the base tile.
#[must_use]
pub const fn variant_hash(seed: u32, x: u32, y: u32, tile: u32) -> u64 {
    mix64(mix64(((seed as u64) << 32) | tile as u64) ^ (((x as u64) << 32) | y as u64))
}

fn flag_bits(flip_x: bool, flip_y: bool, diagonal: bool) -> u32 {
    (if flip_x { FLIP_X_BIT } else { 0 })
        | (if flip_y { FLIP_Y_BIT } else { 0 })
        | (if diagonal { FLIP_DIAGONAL_BIT } else { 0 })
}

impl AutoTileSet {
    /// Parses an `.autotile.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, AutoTileError> {
        let found = version::peek_ron_version(text).map_err(AutoTileError::Parse)?;
        if found > AUTOTILE_FORMAT_VERSION {
            return Err(AutoTileError::Version {
                found,
                expected: AUTOTILE_FORMAT_VERSION,
            });
        }
        let mut set: Self = ron::from_str(text).map_err(|e| AutoTileError::Parse(e.to_string()))?;
        set.migrate_to_latest()?;
        set.validate()?;
        Ok(set)
    }

    /// Serialises to RON with a trailing newline.
    pub fn to_ron(&self) -> Result<String, AutoTileError> {
        let mut s = ron::ser::to_string_pretty(self, regia_core::ron_pretty().compact_arrays(true))
            .map_err(|e| AutoTileError::Parse(e.to_string()))?;
        s.push('\n');
        Ok(s)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, AutoTileError> {
        if self.version > AUTOTILE_FORMAT_VERSION {
            return Err(AutoTileError::Version {
                found: self.version,
                expected: AUTOTILE_FORMAT_VERSION,
            });
        }
        if self.version < AUTOTILE_FORMAT_VERSION {
            self.version =
                version::migrate(self, self.version, autotile_migrations(), AUTOTILE_FORMAT_VERSION)
                    .map_err(|e| AutoTileError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// Names, sizes, patterns, weights and limits; member sets of different terrains disjoint.
    pub fn validate(&self) -> Result<(), AutoTileError> {
        let invalid = |m: String| Err(AutoTileError::Invalid(m));
        if self.tileset.is_empty() || self.tileset.len() > MAX_NAME {
            return invalid(format!("the tileset name must be 1..={MAX_NAME} bytes"));
        }
        if self.terrains.is_empty() || self.terrains.len() > MAX_TERRAINS {
            return invalid(format!("a rule set holds 1..={MAX_TERRAINS} terrains"));
        }
        let mut names = BTreeSet::new();
        let mut owner: BTreeMap<u32, &str> = BTreeMap::new();
        for terrain in &self.terrains {
            let name = terrain.name.as_str();
            if name.is_empty() || name.len() > MAX_NAME {
                return invalid(format!("terrain names must be 1..={MAX_NAME} bytes"));
            }
            if !names.insert(name) {
                return invalid(format!("terrain `{name}` is defined twice"));
            }
            match &terrain.layout {
                Layout::Edge16(tiles) if tiles.len() != 16 => {
                    return invalid(format!(
                        "terrain `{name}`: Edge16 needs 16 tiles, has {}",
                        tiles.len()
                    ));
                }
                Layout::WangCorner(tiles) if tiles.len() != 16 => {
                    return invalid(format!(
                        "terrain `{name}`: WangCorner needs 16 tiles, has {}",
                        tiles.len()
                    ));
                }
                Layout::Blob47(tiles) if tiles.len() != 47 => {
                    return invalid(format!(
                        "terrain `{name}`: Blob47 needs 47 tiles, has {}",
                        tiles.len()
                    ));
                }
                Layout::Rules(rules) => {
                    if rules.is_empty() || rules.len() > MAX_RULES {
                        return invalid(format!("terrain `{name}`: Rules needs 1..={MAX_RULES} rules"));
                    }
                    for rule in rules {
                        if rule.pattern.len() > MAX_PATTERN {
                            return invalid(format!(
                                "terrain `{name}`: pattern is longer than {MAX_PATTERN} bytes"
                            ));
                        }
                        compile_pattern(&rule.pattern)
                            .map_err(|e| AutoTileError::Invalid(format!("terrain `{name}`: {e}")))?;
                    }
                }
                Layout::Edge16(_) | Layout::Blob47(_) | Layout::WangCorner(_) => {}
            }
            if terrain.fallback.is_some() && !matches!(terrain.layout, Layout::Rules(_)) {
                return invalid(format!("terrain `{name}`: `fallback` is for Rules layouts only"));
            }
            if terrain.members.len() > MAX_MEMBER_LIST || terrain.variants.len() > MAX_MEMBER_LIST {
                return invalid(format!(
                    "terrain `{name}` lists more than {MAX_MEMBER_LIST} members or variants"
                ));
            }
            for (key, alts) in &terrain.variants {
                if alts.is_empty() || alts.len() > MAX_VARIANTS {
                    return invalid(format!(
                        "terrain `{name}`: variants of tile {key} need 1..={MAX_VARIANTS} entries"
                    ));
                }
                if let Some(bad) = alts.iter().find(|a| a.weight == 0 || a.weight > MAX_WEIGHT) {
                    return invalid(format!(
                        "terrain `{name}`: variant tile {} has weight {} (1..={MAX_WEIGHT})",
                        bad.tile, bad.weight
                    ));
                }
            }
            for tile in terrain.member_tiles() {
                if let Some(other) = owner.insert(tile, name) {
                    return invalid(format!("tile {tile} belongs to both `{other}` and `{name}`"));
                }
            }
        }
        Ok(())
    }

    /// Index of the terrain named `name`.
    #[must_use]
    pub fn terrain_index(&self, name: &str) -> Option<usize> {
        self.terrains.iter().position(|t| t.name == name)
    }

    /// Binds the rules to `map`: finds the tileset and checks every tile id is in it.
    pub fn bind(&self, map: &TileMapAsset) -> Result<BoundAutoTiles<'_>, AutoTileError> {
        let bind = |m: String| AutoTileError::Bind(m);
        // Layout lengths and variant lists are indexed by `resolve_in`; a set that skipped
        // `validate` must not reach it.
        self.validate()?;
        let tileset = map
            .tilesets
            .iter()
            .position(|t| t.name == self.tileset)
            .ok_or_else(|| bind(format!("the map has no tileset named `{}`", self.tileset)))?;
        let set = &map.tilesets[tileset];
        let mut member_of = HashMap::new();
        let mut terrains = Vec::with_capacity(self.terrains.len());
        for (index, terrain) in self.terrains.iter().enumerate() {
            for tile in terrain.member_tiles() {
                // A GID past the mask would read as flip flags.
                let gid = set.first_gid.checked_add(tile);
                if tile >= set.tile_count || gid.is_none_or(|g| g > crate::tilemap::GID_MASK) {
                    return Err(bind(format!(
                        "terrain `{}` uses tile {tile}; tileset `{}` has {} tiles",
                        terrain.name, set.name, set.tile_count
                    )));
                }
                member_of.insert(tile, index);
            }
            let rules = match &terrain.layout {
                Layout::Rules(rules) => rules
                    .iter()
                    .map(|r| {
                        compile_pattern(&r.pattern)
                            .map(|p| (p, r.tile | flag_bits(r.flip_x, r.flip_y, r.diagonal)))
                            .map_err(AutoTileError::Invalid)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Layout::Edge16(_) | Layout::Blob47(_) | Layout::WangCorner(_) => Vec::new(),
            };
            let base = terrain
                .base_tile()
                .ok_or_else(|| AutoTileError::Invalid(format!("terrain `{}` has no tiles", terrain.name)))?;
            terrains.push(BoundTerrain { rules, base });
        }
        Ok(BoundAutoTiles {
            set: self,
            first_gid: set.first_gid,
            tile_count: set.tile_count,
            member_of,
            terrains,
        })
    }
}

/// The `.autotile.ron` migration chain. v1 → v2 only adds the `WangCorner` layout, so a v1 file
/// already reads as v2 and the step changes only the version (`regia upgrade-project` rewrites the
/// number).
fn autotile_migrations() -> &'static [VersionStep<AutoTileSet>] {
    &[VersionStep {
        from: 1,
        to: 2,
        migrate: |_set| Ok(()),
    }]
}

/// The `from` versions of the `.autotile.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn autotile_migration_sources() -> Vec<u32> {
    autotile_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.autotile.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn autotile_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<AutoTileSet>(),
        serde_field_names::<Terrain>(),
        serde_field_names::<PatternRule>(),
        serde_field_names::<WeightedTile>(),
        serde_variant_names::<Layout>(),
        serde_variant_names::<Border>(),
    ])
}

/// A compiled terrain.
#[derive(Debug)]
struct BoundTerrain {
    /// Rules layout: compiled patterns with their raw output (local id | flag bits).
    rules: Vec<([Cond; 9], u32)>,
    /// Placeholder local id written by a paint before resolving.
    base: u32,
}

/// What a paint writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerrainBrush {
    /// Terrain at this index of [`AutoTileSet::terrains`].
    Terrain(usize),
    /// Empty cells (GID 0).
    Erase,
}

/// One changed cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileChange {
    /// Column.
    pub x: u32,
    /// Row (0 = top).
    pub y: u32,
    /// Raw GID (with flags) before.
    pub before: u32,
    /// Raw GID (with flags) after.
    pub after: u32,
}

/// Restores the cells of `changes` to their `before` values (an undo of a paint).
pub fn revert_changes(
    map: &mut TileMapAsset,
    layer: usize,
    changes: &[TileChange],
) -> Result<(), AutoTileError> {
    write_changes(map, layer, changes, |c| c.before)
}

/// Writes the `after` values of `changes` (a redo of a paint).
pub fn apply_changes(
    map: &mut TileMapAsset,
    layer: usize,
    changes: &[TileChange],
) -> Result<(), AutoTileError> {
    write_changes(map, layer, changes, |c| c.after)
}

fn write_changes(
    map: &mut TileMapAsset,
    layer: usize,
    changes: &[TileChange],
    value: impl Fn(&TileChange) -> u32,
) -> Result<(), AutoTileError> {
    let (width, height) = (map.width, map.height);
    if let Some(c) = changes.iter().find(|c| c.x >= width || c.y >= height) {
        return Err(AutoTileError::OutsideMap { x: c.x, y: c.y });
    }
    let data = layer_data_mut(map, layer)?;
    // Checked before the first write so a short layer is not half-edited.
    if data.len() as u64 != u64::from(width) * u64::from(height) {
        return Err(AutoTileError::Bind(format!(
            "layer {layer} does not hold {width}x{height} cells"
        )));
    }
    for c in changes.iter().rev() {
        if let Some(cell) = data.get_mut((c.y as usize) * (width as usize) + c.x as usize) {
            *cell = value(c);
        }
    }
    Ok(())
}

fn layer_data(map: &TileMapAsset, layer: usize) -> Result<&[u32], AutoTileError> {
    match map.layers.get(layer) {
        Some(MapLayer::Tiles(t)) => Ok(&t.data),
        _ => Err(AutoTileError::NotTileLayer(layer)),
    }
}

fn layer_data_mut(map: &mut TileMapAsset, layer: usize) -> Result<&mut Vec<u32>, AutoTileError> {
    match map.layers.get_mut(layer) {
        Some(MapLayer::Tiles(t)) => Ok(&mut t.data),
        _ => Err(AutoTileError::NotTileLayer(layer)),
    }
}

/// A rule set bound to one map's tileset ([`AutoTileSet::bind`]).
#[derive(Debug)]
pub struct BoundAutoTiles<'a> {
    set: &'a AutoTileSet,
    first_gid: u32,
    tile_count: u32,
    member_of: HashMap<u32, usize>,
    terrains: Vec<BoundTerrain>,
}

impl BoundAutoTiles<'_> {
    /// The rule set.
    #[must_use]
    pub fn rules(&self) -> &AutoTileSet {
        self.set
    }

    /// The terrain a raw GID belongs to.
    #[must_use]
    pub fn terrain_of(&self, raw_gid: u32) -> Option<usize> {
        let (gid, _) = split_gid(raw_gid);
        let local = gid.checked_sub(self.first_gid)?;
        if gid == 0 || local >= self.tile_count {
            return None;
        }
        self.member_of.get(&local).copied()
    }

    /// The terrain of cell `(x, y)` (row 0 = top) of `layer`.
    #[must_use]
    pub fn terrain_at(&self, map: &TileMapAsset, layer: usize, x: u32, y: u32) -> Option<usize> {
        if x >= map.width || y >= map.height {
            return None;
        }
        let data = layer_data(map, layer).ok()?;
        self.terrain_of(*data.get((y as usize) * (map.width as usize) + x as usize)?)
    }

    /// The raw GID cell `(x, y)` should show, or `None` when it belongs to no terrain.
    #[must_use]
    pub fn resolve(&self, map: &TileMapAsset, layer: usize, x: u32, y: u32) -> Option<u32> {
        let data = layer_data(map, layer).ok()?;
        if x >= map.width || y >= map.height {
            return None;
        }
        self.resolve_in(map.orientation, data, map.width, map.height, x, y)
    }

    /// Whether the neighbour at `(x + dx, y + dy)` belongs to `terrain`.
    fn same(&self, data: &[u32], width: u32, height: u32, terrain: usize, x: i64, y: i64) -> bool {
        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
            return self.set.border == Border::Same;
        }
        data.get((y as usize) * (width as usize) + x as usize)
            .and_then(|&raw| self.terrain_of(raw))
            == Some(terrain)
    }

    fn resolve_in(
        &self,
        orientation: Orientation,
        data: &[u32],
        width: u32,
        height: u32,
        x: u32,
        y: u32,
    ) -> Option<u32> {
        let raw = *data.get((y as usize) * (width as usize) + x as usize)?;
        let terrain = self.terrain_of(raw)?;
        let def = &self.set.terrains[terrain];
        // The pattern slots follow the cell geometry; a slot with no neighbor never matches.
        let cells = orientation.neighbor_cells(x, y);
        let at = |i: usize| match cells[i] {
            Some((nx, ny)) => self.same(data, width, height, terrain, nx, ny),
            None => false,
        };
        let out = match &def.layout {
            Layout::Edge16(tiles) => tiles[edge16_mask(at(1), at(5), at(7), at(3))],
            Layout::WangCorner(tiles) => {
                // Pattern cells: NW 0, N 1, NE 2, W 3, E 5, SW 6, S 7, SE 8.
                let corner = |a: usize, b: usize, diagonal: usize| at(a) && at(b) && at(diagonal);
                let mask = usize::from(corner(1, 3, 0))
                    | (usize::from(corner(1, 5, 2)) << 1)
                    | (usize::from(corner(7, 5, 8)) << 2)
                    | (usize::from(corner(7, 3, 6)) << 3);
                tiles[mask]
            }
            Layout::Blob47(tiles) => {
                // Clockwise from north: N, NE, E, SE, S, SW, W, NW as pattern cells.
                let order = [1, 2, 5, 8, 7, 6, 3, 0];
                let mask = order
                    .iter()
                    .enumerate()
                    .fold(0u8, |m, (bit, &cell)| m | (u8::from(at(cell)) << bit));
                tiles[blob47_index(mask)]
            }
            Layout::Rules(_) => {
                let rules = &self.terrains[terrain].rules;
                let hit = rules.iter().find(|(pattern, _)| {
                    pattern.iter().enumerate().all(|(i, cond)| match cond {
                        Cond::Any => true,
                        Cond::Same => at(i),
                        Cond::Other => !at(i),
                    })
                });
                match hit {
                    Some((_, out)) => *out,
                    None => def.fallback.unwrap_or(self.terrains[terrain].base),
                }
            }
        };
        let (local, flags) = (out & crate::tilemap::GID_MASK, out & !crate::tilemap::GID_MASK);
        let out = match def.variants.get(&local) {
            Some(alts) => {
                let total: u64 = alts.iter().map(|a| u64::from(a.weight)).sum();
                let mut pick = variant_hash(self.set.seed, x, y, local) % total.max(1);
                let mut chosen = &alts[0];
                for alt in alts {
                    if pick < u64::from(alt.weight) {
                        chosen = alt;
                        break;
                    }
                    pick -= u64::from(alt.weight);
                }
                chosen.tile | flag_bits(chosen.flip_x, chosen.flip_y, chosen.diagonal)
            }
            None => local | flags,
        };
        Some(((out & crate::tilemap::GID_MASK) + self.first_gid) | (out & !crate::tilemap::GID_MASK))
    }

    /// Paints `brush` over `cells` of `layer` and re-resolves them and their neighbors on the map's cell geometry (`Orientation::neighbor_cells`).
    /// Every cell is checked before anything is written. Returns the changed cells sorted by
    /// `(y, x)`.
    pub fn paint(
        &self,
        map: &mut TileMapAsset,
        layer: usize,
        brush: TerrainBrush,
        cells: &[(u32, u32)],
    ) -> Result<Vec<TileChange>, AutoTileError> {
        let _span = tracing::info_span!("autotile.paint", cells = cells.len()).entered();
        let (width, height, orientation) = (map.width, map.height, map.orientation);
        // A map built in code may have skipped `TileMapAsset::validate`.
        let expected = u64::from(width) * u64::from(height);
        if layer_data(map, layer)?.len() as u64 != expected {
            return Err(AutoTileError::Bind(format!(
                "layer {layer} does not hold {width}x{height} cells"
            )));
        }
        if let Some(&(x, y)) = cells.iter().find(|(x, y)| *x >= width || *y >= height) {
            return Err(AutoTileError::OutsideMap { x, y });
        }
        let placeholder = match brush {
            TerrainBrush::Terrain(t) => {
                let bound = self.terrains.get(t).ok_or(AutoTileError::UnknownTerrain(t))?;
                bound.base + self.first_gid
            }
            TerrainBrush::Erase => 0,
        };
        let mut dirty = BTreeSet::new();
        for &(x, y) in cells {
            for (nx, ny) in orientation.neighbor_cells(x, y).into_iter().flatten() {
                if nx >= 0 && ny >= 0 && nx < i64::from(width) && ny < i64::from(height) {
                    dirty.insert((ny as u32, nx as u32));
                }
            }
        }
        let data = layer_data_mut(map, layer)?;
        let index = |x: u32, y: u32| (y as usize) * (width as usize) + x as usize;
        let before: Vec<u32> = dirty.iter().map(|&(y, x)| data[index(x, y)]).collect();
        for &(x, y) in cells {
            data[index(x, y)] = placeholder;
        }
        // Membership no longer changes: every resolved output is a member of the same terrain.
        let resolved: Vec<Option<u32>> = dirty
            .iter()
            .map(|&(y, x)| self.resolve_in(orientation, data, width, height, x, y))
            .collect();
        let mut changes = Vec::new();
        for ((&(y, x), before), resolved) in dirty.iter().zip(before).zip(resolved) {
            let cell = &mut data[index(x, y)];
            if let Some(value) = resolved {
                *cell = value;
            }
            if *cell != before {
                changes.push(TileChange {
                    x,
                    y,
                    before,
                    after: *cell,
                });
            }
        }
        Ok(changes)
    }

    /// Re-resolves every terrain cell of `layer` (after hand edits or an import). Returns the
    /// changed cells sorted by `(y, x)`.
    pub fn apply_layer(
        &self,
        map: &mut TileMapAsset,
        layer: usize,
    ) -> Result<Vec<TileChange>, AutoTileError> {
        let _span = tracing::info_span!("autotile.apply_layer", layer).entered();
        let (width, height, orientation) = (map.width, map.height, map.orientation);
        let data = layer_data_mut(map, layer)?;
        if data.len() as u64 != u64::from(width) * u64::from(height) {
            return Err(AutoTileError::Bind(format!(
                "layer {layer} does not hold {width}x{height} cells"
            )));
        }
        let mut changes = Vec::new();
        for y in 0..height {
            for x in 0..width {
                if let Some(value) = self.resolve_in(orientation, data, width, height, x, y) {
                    let cell = &mut data[(y as usize) * (width as usize) + x as usize];
                    if *cell != value {
                        changes.push(TileChange {
                            x,
                            y,
                            before: *cell,
                            after: value,
                        });
                        *cell = value;
                    }
                }
            }
        }
        Ok(changes)
    }
}

/// Current cooked [`CookedAutoTileSet`] payload version.
pub const COOKED_AUTOTILE_VERSION: u32 = 1;

/// Type path of the cooked autotile payload (the cooked-file type tag).
pub const AUTOTILE_TYPE_PATH: &str = "regia_2d::CookedAutoTileSet";

/// A cooked rule set: the validated [`AutoTileSet`] as a binary payload, so a shipped game reads
/// its rules from the pak with the rest of the tilemap assets instead of parsing RON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookedAutoTileSet {
    /// Payload version ([`COOKED_AUTOTILE_VERSION`]).
    pub version: u32,
    /// The rules.
    pub set: AutoTileSet,
}

impl CookedAutoTileSet {
    /// Wraps a rule set (validated).
    pub fn new(set: AutoTileSet) -> Result<Self, AutoTileError> {
        set.validate()?;
        Ok(Self {
            version: COOKED_AUTOTILE_VERSION,
            set,
        })
    }

    /// Encodes the cooked payload (postcard).
    pub fn to_postcard(&self) -> Result<Vec<u8>, AutoTileError> {
        postcard::to_allocvec(self).map_err(|e| AutoTileError::Parse(e.to_string()))
    }

    /// Decodes and validates a cooked payload.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, AutoTileError> {
        let cooked: Self = postcard::from_bytes(bytes).map_err(|e| AutoTileError::Parse(e.to_string()))?;
        if cooked.version != COOKED_AUTOTILE_VERSION {
            return Err(AutoTileError::Version {
                found: cooked.version,
                expected: COOKED_AUTOTILE_VERSION,
            });
        }
        cooked.set.validate()?;
        Ok(cooked)
    }
}

/// Rule sets available at run time, by name (paths or registered names). The agent's
/// `tilemap.paint` and `tilemap.autotile` look rules up here.
#[derive(Resource, Default, Debug)]
pub struct AutoTiles {
    /// Rule sets by name, sorted (listing order is stable).
    pub sets: BTreeMap<String, Arc<AutoTileSet>>,
}

impl AutoTiles {
    /// Registers a validated rule set under `name`, replacing any previous one.
    pub fn insert(&mut self, name: impl Into<String>, set: AutoTileSet) -> Result<(), AutoTileError> {
        set.validate()?;
        self.sets.insert(name.into(), Arc::new(set));
        Ok(())
    }

    /// Registers the rule set of a cooked file (`.autotile.ron` cooked by the asset pipeline)
    /// under `name`. `bytes` is the cooked file as `AssetServer::read_cooked` returns it.
    pub fn insert_cooked(&mut self, name: impl Into<String>, bytes: &[u8]) -> Result<(), AutoTileError> {
        let (header, payload) =
            regia_asset::decode_cooked_file(bytes).map_err(|e| AutoTileError::Parse(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(AUTOTILE_TYPE_PATH) {
            return Err(AutoTileError::Parse("not a cooked autotile set".into()));
        }
        let cooked = CookedAutoTileSet::from_postcard(payload)?;
        self.insert(name, cooked.set)
    }

    /// Loads the cooked rule set at asset `path` (a `.autotile.ron` in the cooked manifest, read
    /// from the pak or the dev cache) and registers it under `name`.
    pub fn load_from_server(
        &mut self,
        name: impl Into<String>,
        path: &str,
        server: &mut regia_asset::AssetServer,
    ) -> Result<(), AutoTileError> {
        let id = server
            .id_for_path(path)
            .ok_or_else(|| AutoTileError::Parse(format!("`{path}` is not in the cooked manifest")))?;
        let (bytes, _) = server
            .read_cooked(id)
            .map_err(|e| AutoTileError::Parse(e.to_string()))?;
        self.insert_cooked(name, &bytes)
    }

    /// The rule set named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<AutoTileSet>> {
        self.sets.get(name)
    }
}

#[cfg(test)]
mod tests;
