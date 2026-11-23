// SPDX-License-Identifier: MPL-2.0
//! `.tilepatterns.ron` v1: named tile patterns saved with a tileset (ADR-7853, the editor's 2D
//! pattern palette). The file sits next to the tileset's image (`grass.png` →
//! `grass.tilepatterns.ron`), so every map that uses the tileset sees the same patterns. A pattern
//! is a rectangle of the tileset's own tiles: cells hold `local id + 1` (0 is an empty cell) with
//! the flip bits on top, so a map that gives the tileset another `first_gid` still reads it right.
//!
//! ```ron
//! (
//!     version: 1,
//!     tileset: "grass",
//!     patterns: [
//!         (name: "bush", width: 2, height: 1, cells: [5, 6]),
//!     ],
//! )
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::stamp::{MAX_STAMP_CELLS, Stamp};
use crate::tilemap::{GID_MASK, TileMapAsset, TilesetDef};

/// Current `.tilepatterns.ron` version.
pub const TILEPATTERNS_FORMAT_VERSION: u32 = 1;
/// The file suffix.
pub const TILEPATTERNS_SUFFIX: &str = ".tilepatterns.ron";
/// Most patterns one file holds.
pub const MAX_PATTERNS: usize = 1024;
/// Longest pattern or tileset name, bytes.
pub const MAX_PATTERN_NAME: usize = 128;

/// Why a pattern file or edit was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TilePatternError {
    /// Not RON of this shape.
    #[error("cannot parse the patterns: {0}")]
    Parse(String),
    /// Written by a newer version.
    #[error("tile patterns version {found} is newer than {expected}")]
    Version {
        /// The file's version.
        found: u32,
        /// The newest this build reads.
        expected: u32,
    },
    /// The content breaks a rule of the format.
    #[error("{0}")]
    Invalid(String),
}

impl TilePatternError {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "tilepatterns_parse",
            Self::Version { .. } => "tilepatterns_version",
            Self::Invalid(_) => "tilepatterns_invalid",
        }
    }
}

/// One named pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilePattern {
    /// Unique name in the file.
    pub name: String,
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height` cells row by row, top row first: `local id + 1` with flip bits, 0 empty.
    pub cells: Vec<u32>,
}

/// The `.tilepatterns.ron` document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilePatternLibrary {
    /// Format version ([`TILEPATTERNS_FORMAT_VERSION`]).
    pub version: u32,
    /// The tileset's name (as in the map's `TilesetDef::name`).
    pub tileset: String,
    /// The patterns, in palette order.
    pub patterns: Vec<TilePattern>,
}

impl TilePatternLibrary {
    /// An empty library for `tileset`.
    #[must_use]
    pub fn new(tileset: &str) -> Self {
        Self {
            version: TILEPATTERNS_FORMAT_VERSION,
            tileset: tileset.into(),
            patterns: Vec::new(),
        }
    }

    /// Parses a `.tilepatterns.ron`: version check and validation.
    pub fn from_ron(text: &str) -> Result<Self, TilePatternError> {
        let found = regia_core::version::peek_ron_version(text).map_err(TilePatternError::Parse)?;
        if found > TILEPATTERNS_FORMAT_VERSION {
            return Err(TilePatternError::Version {
                found,
                expected: TILEPATTERNS_FORMAT_VERSION,
            });
        }
        let mut lib: Self = ron::from_str(text).map_err(|e| TilePatternError::Parse(e.to_string()))?;
        lib.version = TILEPATTERNS_FORMAT_VERSION;
        lib.validate()?;
        Ok(lib)
    }

    /// Serializes to RON with a trailing newline.
    pub fn to_ron(&self) -> Result<String, TilePatternError> {
        let mut s = ron::ser::to_string_pretty(self, regia_core::ron_pretty().compact_arrays(true))
            .map_err(|e| TilePatternError::Parse(e.to_string()))?;
        s.push('\n');
        Ok(s)
    }

    /// Names (non-empty, unique, at most [`MAX_PATTERN_NAME`] bytes), sizes (non-zero, at most
    /// `MAX_STAMP_CELLS` cells, `width * height` cells), at most [`MAX_PATTERNS`] patterns.
    pub fn validate(&self) -> Result<(), TilePatternError> {
        let invalid = |m: String| Err(TilePatternError::Invalid(m));
        if self.tileset.is_empty() || self.tileset.len() > MAX_PATTERN_NAME {
            return invalid(format!("the tileset name is 1..={MAX_PATTERN_NAME} bytes"));
        }
        if self.patterns.len() > MAX_PATTERNS {
            return invalid(format!("at most {MAX_PATTERNS} patterns"));
        }
        for (i, p) in self.patterns.iter().enumerate() {
            if p.name.trim().is_empty() || p.name.len() > MAX_PATTERN_NAME {
                return invalid(format!("pattern {i}: the name is 1..={MAX_PATTERN_NAME} bytes"));
            }
            if self.patterns[..i].iter().any(|q| q.name == p.name) {
                return invalid(format!("two patterns are named `{}`", p.name));
            }
            let cells = u64::from(p.width) * u64::from(p.height);
            if cells == 0 || cells > MAX_STAMP_CELLS {
                return invalid(format!("pattern `{}`: size {}x{}", p.name, p.width, p.height));
            }
            if p.cells.len() as u64 != cells {
                return invalid(format!(
                    "pattern `{}`: {} cells for {}x{}",
                    p.name,
                    p.cells.len(),
                    p.width,
                    p.height
                ));
            }
        }
        Ok(())
    }

    /// The pattern named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&TilePattern> {
        self.patterns.iter().find(|p| p.name == name)
    }

    /// Adds `pattern`, or replaces the one with its name (keeping its place). Checked.
    pub fn put(&mut self, pattern: TilePattern) -> Result<(), TilePatternError> {
        let mut next = self.clone();
        match next.patterns.iter_mut().find(|p| p.name == pattern.name) {
            Some(slot) => *slot = pattern,
            None => next.patterns.push(pattern),
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Removes the pattern named `name`; false when there is none.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.patterns.len();
        self.patterns.retain(|p| p.name != name);
        self.patterns.len() != before
    }
}

impl TilePattern {
    /// A pattern from a stamp whose tiles all belong to `set` (empty stamp cells stay empty).
    pub fn from_stamp(name: &str, stamp: &Stamp, set: &TilesetDef) -> Result<Self, TilePatternError> {
        let cells = stamp
            .cells
            .iter()
            .map(|&raw| {
                let gid = raw & GID_MASK;
                if gid == 0 {
                    return Ok(0);
                }
                let local = gid
                    .checked_sub(set.first_gid)
                    .filter(|&l| l < set.tile_count)
                    .ok_or_else(|| {
                        TilePatternError::Invalid(format!("tile {gid} is not in tileset `{}`", set.name))
                    })?;
                Ok((local + 1) | (raw & !GID_MASK))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: name.into(),
            width: stamp.width,
            height: stamp.height,
            cells,
        })
    }

    /// The stamp of this pattern for `set` (its `first_gid`); a local id past the tileset is refused.
    pub fn to_stamp(&self, set: &TilesetDef) -> Result<Stamp, TilePatternError> {
        let cells = self
            .cells
            .iter()
            .map(|&cell| {
                let id = cell & GID_MASK;
                if id == 0 {
                    return Ok(0);
                }
                if id > set.tile_count {
                    return Err(TilePatternError::Invalid(format!(
                        "pattern `{}` uses tile {} of a {}-tile tileset",
                        self.name,
                        id - 1,
                        set.tile_count
                    )));
                }
                Ok((set.first_gid + id - 1) | (cell & !GID_MASK))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Stamp::new(self.width, self.height, cells).map_err(|e| TilePatternError::Invalid(e.to_string()))
    }
}

/// The sidecar of tileset `set` of the map at `map_path`: next to the tileset's image (relative to
/// the map's folder), with the image's extension replaced by `.tilepatterns.ron`.
#[must_use]
pub fn tilepatterns_path(map_path: &Path, set: &TilesetDef) -> PathBuf {
    let image = map_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(&set.image);
    let stem = image
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(set.name.as_str())
        .to_string();
    image.with_file_name(format!("{stem}{TILEPATTERNS_SUFFIX}"))
}

/// The tileset of `map` named `name`, with its index.
#[must_use]
pub fn tileset_named<'a>(map: &'a TileMapAsset, name: &str) -> Option<(usize, &'a TilesetDef)> {
    map.tilesets.iter().enumerate().find(|(_, s)| s.name == name)
}

/// The `from` versions of the `.tilepatterns.ron` migration steps (none yet).
#[must_use]
pub fn tilepatterns_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.tilepatterns.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn tilepatterns_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<TilePatternLibrary>(),
        serde_field_names::<TilePattern>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(first_gid: u32) -> TilesetDef {
        TilesetDef {
            name: "grass".into(),
            first_gid,
            image: "tiles/grass.png".into(),
            image_width: 64,
            image_height: 16,
            tile_width: 16,
            tile_height: 16,
            columns: 4,
            tile_count: 4,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }
    }

    #[test]
    fn a_pattern_round_trips_through_the_file_and_moves_with_first_gid() {
        let stamp = Stamp::new(2, 1, vec![2 | crate::tilemap::FLIP_X_BIT, 0]).expect("stamp");
        let pattern = TilePattern::from_stamp("bush", &stamp, &set(1)).expect("pattern");
        assert_eq!(
            pattern.cells,
            vec![2 | crate::tilemap::FLIP_X_BIT, 0],
            "local 1 is stored as 2"
        );
        let mut lib = TilePatternLibrary::new("grass");
        lib.put(pattern.clone()).expect("put");
        let text = lib.to_ron().expect("ron");
        let back = TilePatternLibrary::from_ron(&text).expect("parses");
        assert_eq!(back, lib);
        // In a map where the tileset starts at gid 11, local 1 is gid 12.
        let moved = back.get("bush").expect("bush").to_stamp(&set(11)).expect("stamp");
        assert_eq!(moved.cells, vec![12 | crate::tilemap::FLIP_X_BIT, 0]);
        // A tile of another tileset is refused.
        let foreign = Stamp::new(1, 1, vec![9]).expect("stamp");
        assert!(TilePattern::from_stamp("x", &foreign, &set(1)).is_err());
        // A local id past a smaller tileset is refused when used.
        let mut small = set(1);
        small.tile_count = 1;
        assert!(pattern.to_stamp(&small).is_err());
    }

    #[test]
    fn the_file_checks_names_sizes_and_versions() {
        let mut lib = TilePatternLibrary::new("grass");
        let p = |name: &str, w: u32, h: u32, n: usize| TilePattern {
            name: name.into(),
            width: w,
            height: h,
            cells: vec![1; n],
        };
        lib.put(p("a", 1, 1, 1)).expect("ok");
        lib.put(p("a", 2, 1, 2)).expect("replaces in place");
        assert_eq!(lib.patterns.len(), 1);
        assert_eq!(lib.get("a").map(|p| p.width), Some(2));
        for bad in [p("", 1, 1, 1), p("b", 0, 1, 0), p("b", 2, 2, 3)] {
            assert!(lib.put(bad).is_err());
        }
        assert_eq!(lib.patterns.len(), 1, "a refused put changes nothing");
        assert!(lib.remove("a"));
        assert!(!lib.remove("a"));
        let newer = "(version: 2, tileset: \"grass\", patterns: [])";
        assert_eq!(
            TilePatternLibrary::from_ron(newer).map_err(|e| e.code()),
            Err("tilepatterns_version")
        );
        let dup = "(version: 1, tileset: \"g\", patterns: [(name: \"a\", width: 1, height: 1, cells: [1]), (name: \"a\", width: 1, height: 1, cells: [1])])";
        assert_eq!(
            TilePatternLibrary::from_ron(dup).map_err(|e| e.code()),
            Err("tilepatterns_invalid")
        );
        assert_eq!(
            TilePatternLibrary::from_ron("nope").map_err(|e| e.code()),
            Err("tilepatterns_parse")
        );
    }

    #[test]
    fn the_sidecar_sits_next_to_the_tileset_image() {
        let path = tilepatterns_path(Path::new("assets/maps/farm.tilemap.ron"), &set(1));
        assert_eq!(path, Path::new("assets/maps/tiles/grass.tilepatterns.ron"));
    }
}
