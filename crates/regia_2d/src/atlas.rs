// SPDX-License-Identifier: MPL-2.0
//! Sprite atlases (`docs/specs/22-tier2.md` § T2.10): the `.atlas.ron` source format the cooker
//! packs, and the cooked [`SpriteAtlas`] the runtime draws from.
//!
//! An `.atlas.ron` lists source images (one sprite each, or a grid cut into numbered cells) with a
//! pivot and optional 9-slice borders. `regia_asset_pipeline` packs them into pages with
//! `texture_packer` and writes a cooked [`SpriteAtlas`]: uncompressed RGBA8 pages (pixel art must
//! stay exact) and one [`AtlasSprite`] per name, sorted by name.
//!
//! ```
//! use regia_2d::atlas::AtlasSource;
//!
//! let source = AtlasSource::from_ron(r#"(
//!     version: 1,
//!     sprites: [
//!         (name: "hero", image: "hero.png", pivot: (0.5, 0.0)),
//!         (name: "button", image: "button.png", nine_slice: Some((4, 4, 4, 4))),
//!         (name: "run", image: "run.png", grid: Some((16, 16))),
//!     ],
//! )"#).expect("valid atlas");
//! assert_eq!(source.sprites.len(), 3);
//! assert_eq!(source.padding, 1);
//! ```

use serde::{Deserialize, Serialize};

use regia_core::version::{self, VersionStep};

/// Current `.atlas.ron` version.
pub const ATLAS_FORMAT_VERSION: u32 = 1;

/// Current cooked [`SpriteAtlas`] payload version.
pub const COOKED_ATLAS_VERSION: u32 = 1;

/// Type path of the cooked atlas payload (the cooked-file type tag).
pub const ATLAS_TYPE_PATH: &str = "regia_2d::SpriteAtlas";

/// Atlas format errors.
#[derive(Debug, thiserror::Error)]
pub enum AtlasError {
    /// The file is a newer version than this build reads.
    #[error("atlas version {found} is newer than supported version {expected}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error.
    #[error("atlas parse error: {0}")]
    Parse(String),
    /// Semantic error (duplicate name, zero grid, bad pivot).
    #[error("invalid atlas: {0}")]
    Invalid(String),
    /// A migration step failed.
    #[error("atlas migration failed: {0}")]
    Migration(String),
    /// Cooked payload decode error.
    #[error("cooked atlas decode error: {0}")]
    Decode(String),
}

fn default_max_size() -> u32 {
    2048
}
fn default_padding() -> u32 {
    1
}
fn default_pivot() -> [f32; 2] {
    [0.5, 0.5]
}

/// The `.atlas.ron` source document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtlasSource {
    /// Format version ([`ATLAS_FORMAT_VERSION`]).
    pub version: u32,
    /// Largest page width/height in pixels; sprites overflow onto more pages.
    #[serde(default = "default_max_size")]
    pub max_size: u32,
    /// Transparent pixels between packed sprites.
    #[serde(default = "default_padding")]
    pub padding: u32,
    /// Pixels of each sprite's edge repeated outwards (stops filtering bleed when scaled).
    #[serde(default)]
    pub extrude: u32,
    /// Trim fully transparent borders (the original size and offset are kept for drawing).
    #[serde(default)]
    pub trim: bool,
    /// The sprites.
    pub sprites: Vec<AtlasSpriteSource>,
}

/// One source image in an `.atlas.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtlasSpriteSource {
    /// Sprite name. With `grid`, the cells are named `name_0`, `name_1`, … row by row.
    pub name: String,
    /// Image path, relative to the `.atlas.ron`.
    pub image: String,
    /// Pivot in the sprite's source rectangle, `(0, 0)` = bottom-left, `(1, 1)` = top-right.
    #[serde(default = "default_pivot")]
    pub pivot: [f32; 2],
    /// 9-slice borders in pixels: `(left, top, right, bottom)`.
    #[serde(default)]
    pub nine_slice: Option<[u32; 4]>,
    /// Cut the image into cells of `(width, height)` pixels.
    #[serde(default)]
    pub grid: Option<[u32; 2]>,
}

impl AtlasSource {
    /// Parses an `.atlas.ron`: version check, migration, validation.
    pub fn from_ron(text: &str) -> Result<Self, AtlasError> {
        let found = version::peek_ron_version(text).map_err(AtlasError::Parse)?;
        if found > ATLAS_FORMAT_VERSION {
            return Err(AtlasError::Version {
                found,
                expected: ATLAS_FORMAT_VERSION,
            });
        }
        let mut source: Self = ron::from_str(text).map_err(|e| AtlasError::Parse(e.to_string()))?;
        source.migrate_to_latest()?;
        source.validate()?;
        Ok(source)
    }

    /// Serialises to RON with a trailing newline.
    pub fn to_ron(&self) -> Result<String, AtlasError> {
        let mut s = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| AtlasError::Parse(e.to_string()))?;
        s.push('\n');
        Ok(s)
    }

    /// Rejects a newer version and runs the migration chain on an older one
    /// (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, AtlasError> {
        if self.version > ATLAS_FORMAT_VERSION {
            return Err(AtlasError::Version {
                found: self.version,
                expected: ATLAS_FORMAT_VERSION,
            });
        }
        if self.version < ATLAS_FORMAT_VERSION {
            self.version = version::migrate(self, self.version, atlas_migrations(), ATLAS_FORMAT_VERSION)
                .map_err(|e| AtlasError::Migration(e.to_string()))?;
        }
        Ok(self.version)
    }

    /// Unique non-empty names, positive grid cells, pivots in `[0, 1]`, a usable page size.
    pub fn validate(&self) -> Result<(), AtlasError> {
        let invalid = |m: String| Err(AtlasError::Invalid(m));
        if self.max_size < 16 {
            return invalid(format!("max_size {} is below 16", self.max_size));
        }
        if self.max_size > MAX_PAGE_SIZE {
            return invalid(format!("max_size {} is above {MAX_PAGE_SIZE}", self.max_size));
        }
        if self.padding > MAX_PADDING || self.extrude > MAX_PADDING {
            return invalid(format!("padding and extrude may not exceed {MAX_PADDING}"));
        }
        let mut names = std::collections::BTreeSet::new();
        for s in &self.sprites {
            if s.name.is_empty() {
                return invalid("a sprite has an empty name".into());
            }
            if !names.insert(s.name.as_str()) {
                return invalid(format!("duplicate sprite name `{}`", s.name));
            }
            if !is_relative_path(&s.image) {
                return invalid(format!(
                    "sprite `{}` image `{}` must be a relative path without `..`",
                    s.name, s.image
                ));
            }
            if let Some([w, h]) = s.grid
                && (w == 0 || h == 0)
            {
                return invalid(format!("sprite `{}` has a zero grid cell", s.name));
            }
            if s.pivot.iter().any(|p| !(0.0..=1.0).contains(p)) {
                return invalid(format!(
                    "sprite `{}` pivot {:?} is outside [0, 1]",
                    s.name, s.pivot
                ));
            }
        }
        Ok(())
    }
}

/// Largest page or image edge in pixels accepted from content (above any desktop GPU limit in use).
pub const MAX_PAGE_SIZE: u32 = 16384;

/// Largest `padding` / `extrude` in pixels.
const MAX_PADDING: u32 = 16;

/// True for a non-empty path that stays under the file it is relative to: not absolute, no drive
/// prefix, no `..` component.
fn is_relative_path(path: &str) -> bool {
    use std::path::Component;
    !path.is_empty()
        && !path.starts_with(['/', '\\'])
        && !path.contains(':')
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        && !path.split(['/', '\\']).any(|part| part == "..")
}

/// The `.atlas.ron` migration chain (v1 is the first version: no steps yet). A future v2 adds its
/// `VersionStep { from: 1, .. }` here and in `regia upgrade-project`.
fn atlas_migrations() -> &'static [VersionStep<AtlasSource>] {
    &[]
}

/// The `from` versions of the `.atlas.ron` migration steps (for `cargo xtask check-formats`).
#[must_use]
pub fn atlas_migration_sources() -> Vec<u32> {
    atlas_migrations().iter().map(|s| s.from).collect()
}

/// The serde shape fingerprint of `.atlas.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn atlas_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AtlasSource>(),
        regia_core::schema::serde_field_names::<AtlasSpriteSource>(),
    ])
}

/// 9-slice borders in source pixels: the corners keep their size, the edges stretch along one
/// axis and the centre along both. When two opposite borders together exceed the destination size
/// they shrink in proportion, so the corners get smaller and the centre is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NineSlice {
    /// Left border width.
    pub left: u32,
    /// Top border height.
    pub top: u32,
    /// Right border width.
    pub right: u32,
    /// Bottom border height.
    pub bottom: u32,
}

impl NineSlice {
    /// From the `.atlas.ron` order `(left, top, right, bottom)`.
    #[must_use]
    pub const fn from_array(b: [u32; 4]) -> Self {
        Self {
            left: b[0],
            top: b[1],
            right: b[2],
            bottom: b[3],
        }
    }
}

/// One RGBA8 image: an atlas page or a tileset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 rows, top row first (`width * height * 4` bytes).
    pub rgba8: Vec<u8>,
}

impl RgbaImage {
    /// Checks the pixel buffer matches the size.
    pub fn validate(&self) -> Result<(), String> {
        if self.width > MAX_PAGE_SIZE || self.height > MAX_PAGE_SIZE {
            return Err(format!(
                "image {}x{} exceeds the {MAX_PAGE_SIZE} pixel edge limit",
                self.width, self.height
            ));
        }
        let expected = u64::from(self.width) * u64::from(self.height) * 4;
        if self.width == 0 || self.height == 0 || self.rgba8.len() as u64 != expected {
            return Err(format!(
                "image {}x{} has {} bytes (expected {expected})",
                self.width,
                self.height,
                self.rgba8.len()
            ));
        }
        Ok(())
    }
}

/// One packed sprite.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtlasSprite {
    /// Name (grid cells: `name_N`).
    pub name: String,
    /// Page index.
    pub page: u32,
    /// Packed (trimmed) rectangle in the page: `[x, y, width, height]`, origin top-left.
    pub rect: [u32; 4],
    /// Size before trimming.
    pub source_size: [u32; 2],
    /// Top-left of `rect` inside the untrimmed source (zero when not trimmed).
    pub trim_offset: [u32; 2],
    /// Pivot in the source rectangle, `(0, 0)` = bottom-left.
    pub pivot: [f32; 2],
    /// 9-slice borders, if any.
    pub nine_slice: Option<NineSlice>,
}

/// A cooked sprite atlas: pages plus the sprites on them, sorted by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpriteAtlas {
    /// Payload version ([`COOKED_ATLAS_VERSION`]).
    pub version: u32,
    /// The pages.
    pub pages: Vec<RgbaImage>,
    /// The sprites, sorted by name.
    pub sprites: Vec<AtlasSprite>,
}

/// Most sprites one cooked atlas may hold.
const MAX_ATLAS_SPRITES: usize = 1 << 20;
/// Longest sprite name (bytes).
const MAX_SPRITE_NAME_BYTES: usize = 1024;

impl SpriteAtlas {
    /// A one-page atlas holding a single image as the sprite `name` (procedural content, tests,
    /// sheets drawn by source rectangle).
    #[must_use]
    pub fn single(name: &str, image: RgbaImage) -> Self {
        let (w, h) = (image.width, image.height);
        Self {
            version: COOKED_ATLAS_VERSION,
            pages: vec![image],
            sprites: vec![AtlasSprite {
                name: name.to_string(),
                page: 0,
                rect: [0, 0, w, h],
                source_size: [w, h],
                trim_offset: [0, 0],
                pivot: [0.5, 0.5],
                nine_slice: None,
            }],
        }
    }

    /// The sprite named `name`.
    #[must_use]
    pub fn sprite(&self, name: &str) -> Option<&AtlasSprite> {
        self.sprites
            .binary_search_by(|s| s.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.sprites[i])
    }

    /// Encodes the cooked payload (postcard).
    pub fn to_postcard(&self) -> Result<Vec<u8>, AtlasError> {
        postcard::to_allocvec(self).map_err(|e| AtlasError::Decode(e.to_string()))
    }

    /// Decodes and validates a cooked payload.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, AtlasError> {
        let atlas: Self = postcard::from_bytes(bytes).map_err(|e| AtlasError::Decode(e.to_string()))?;
        if atlas.version != COOKED_ATLAS_VERSION {
            return Err(AtlasError::Version {
                found: atlas.version,
                expected: COOKED_ATLAS_VERSION,
            });
        }
        atlas.validate()?;
        Ok(atlas)
    }

    /// Page sizes match their pixels, every sprite is on a page and inside it, names are sorted
    /// and unique.
    pub fn validate(&self) -> Result<(), AtlasError> {
        for (i, page) in self.pages.iter().enumerate() {
            page.validate()
                .map_err(|e| AtlasError::Invalid(format!("page {i}: {e}")))?;
        }
        if self.sprites.len() > MAX_ATLAS_SPRITES {
            return Err(AtlasError::Invalid(format!(
                "{} sprites exceeds the limit of {MAX_ATLAS_SPRITES}",
                self.sprites.len()
            )));
        }
        if self.sprites.iter().any(|s| s.name.len() > MAX_SPRITE_NAME_BYTES) {
            return Err(AtlasError::Invalid(format!(
                "a sprite name is longer than {MAX_SPRITE_NAME_BYTES} bytes"
            )));
        }
        for pair in self.sprites.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(AtlasError::Invalid(format!(
                    "sprites not sorted/unique at `{}`",
                    pair[1].name
                )));
            }
        }
        for s in &self.sprites {
            let Some(page) = self.pages.get(s.page as usize) else {
                return Err(AtlasError::Invalid(format!(
                    "sprite `{}` is on missing page {}",
                    s.name, s.page
                )));
            };
            let [x, y, w, h] = s.rect;
            if u64::from(x) + u64::from(w) > u64::from(page.width)
                || u64::from(y) + u64::from(h) > u64::from(page.height)
            {
                return Err(AtlasError::Invalid(format!(
                    "sprite `{}` lies outside its page",
                    s.name
                )));
            }
            let [sw, sh] = s.source_size;
            let [tx, ty] = s.trim_offset;
            if sw == 0
                || sh == 0
                || u64::from(tx) + u64::from(w) > u64::from(sw)
                || u64::from(ty) + u64::from(h) > u64::from(sh)
            {
                return Err(AtlasError::Invalid(format!(
                    "sprite `{}` has a zero source size or a trim outside its source box",
                    s.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn huge_image_edges_are_an_error_not_an_overflow() {
        let img = RgbaImage {
            width: 1 << 31,
            height: 1 << 31,
            rgba8: Vec::new(),
        };
        assert!(img.validate().is_err());
    }

    use super::*;

    #[test]
    fn rejects_newer_versions_and_bad_content() {
        assert!(matches!(
            AtlasSource::from_ron("(version: 2, sprites: [])"),
            Err(AtlasError::Version { found: 2, .. })
        ));
        let dup = r#"(version: 1, sprites: [(name: "a", image: "a.png"), (name: "a", image: "b.png")])"#;
        assert!(matches!(AtlasSource::from_ron(dup), Err(AtlasError::Invalid(_))));
        let grid = r#"(version: 1, sprites: [(name: "a", image: "a.png", grid: Some((0, 8)))])"#;
        assert!(matches!(AtlasSource::from_ron(grid), Err(AtlasError::Invalid(_))));
        let pivot = r#"(version: 1, sprites: [(name: "a", image: "a.png", pivot: (2.0, 0.0))])"#;
        assert!(matches!(
            AtlasSource::from_ron(pivot),
            Err(AtlasError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_escaping_paths_and_huge_limits() {
        for image in [
            "/etc/passwd",
            "../x.png",
            "a/../../x.png",
            "C:\\x.png",
            "\\x.png",
            "",
        ] {
            let text = format!(r#"(version: 1, sprites: [(name: "a", image: {image:?})])"#);
            assert!(
                matches!(AtlasSource::from_ron(&text), Err(AtlasError::Invalid(_))),
                "{image}"
            );
        }
        assert!(AtlasSource::from_ron(r#"(version: 1, sprites: [(name: "a", image: "sub/a.png")])"#).is_ok());
        let big = r#"(version: 1, max_size: 4294967295, sprites: [])"#;
        assert!(matches!(AtlasSource::from_ron(big), Err(AtlasError::Invalid(_))));
        let ext = r#"(version: 1, extrude: 2147483648, sprites: [])"#;
        assert!(matches!(AtlasSource::from_ron(ext), Err(AtlasError::Invalid(_))));
        let wide = RgbaImage {
            width: 100_000,
            height: 1,
            rgba8: vec![0; 400_000],
        };
        assert!(wide.validate().is_err());
    }

    #[test]
    fn source_round_trips_through_ron() {
        let text = r#"(version: 1, trim: true, sprites: [(name: "b", image: "b.png", nine_slice: Some((1, 2, 3, 4)))])"#;
        let source = AtlasSource::from_ron(text).expect("parses");
        let again = AtlasSource::from_ron(&source.to_ron().expect("serializes")).expect("re-parses");
        assert_eq!(source, again);
        assert!(source.trim);
        assert_eq!(source.max_size, 2048);
    }

    #[test]
    fn cooked_atlas_round_trips_and_validates() {
        let image = RgbaImage {
            width: 2,
            height: 1,
            rgba8: vec![255; 8],
        };
        let atlas = SpriteAtlas::single("dot", image);
        let bytes = atlas.to_postcard().expect("encodes");
        let back = SpriteAtlas::from_postcard(&bytes).expect("decodes");
        assert_eq!(back.sprite("dot").map(|s| s.rect), Some([0, 0, 2, 1]));
        assert!(back.sprite("nope").is_none());

        let mut broken = atlas.clone();
        broken.sprites[0].rect = [1, 0, 2, 1];
        assert!(broken.validate().is_err());
        let mut short = atlas;
        short.pages[0].rgba8.pop();
        assert!(short.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_source_size_and_trim_outside_source() {
        let image = RgbaImage {
            width: 8,
            height: 8,
            rgba8: vec![255; 8 * 8 * 4],
        };
        let atlas = SpriteAtlas::single("dot", image);
        assert!(atlas.validate().is_ok());
        let mut zero = atlas.clone();
        zero.sprites[0].source_size = [0, 0];
        assert!(zero.validate().is_err());
        let mut trim = atlas;
        trim.sprites[0].trim_offset = [1, 0];
        assert!(trim.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_many_sprites_and_long_names() {
        let image = RgbaImage {
            width: 1,
            height: 1,
            rgba8: vec![255; 4],
        };
        let atlas = SpriteAtlas::single("dot", image);
        let mut long = atlas.clone();
        long.sprites[0].name = "n".repeat(MAX_SPRITE_NAME_BYTES + 1);
        assert!(matches!(long.validate(), Err(AtlasError::Invalid(_))));
        let mut many = atlas.clone();
        let template = many.sprites[0].clone();
        many.sprites = (0..=MAX_ATLAS_SPRITES)
            .map(|i| AtlasSprite {
                name: format!("s{i:08}"),
                ..template.clone()
            })
            .collect();
        assert!(matches!(many.validate(), Err(AtlasError::Invalid(_))));
        assert!(atlas.validate().is_ok());
    }

    #[test]
    fn fingerprint_lists_both_structs() {
        let fp = atlas_schema_fingerprint();
        assert!(fp.contains("max_size") && fp.contains("nine_slice") && fp.contains(';'));
    }

    fn pixel() -> RgbaImage {
        RgbaImage {
            width: 1,
            height: 1,
            rgba8: vec![9, 8, 7, 6],
        }
    }

    #[test]
    fn limits_migration_and_cooked_shape_are_checked_at_the_boundary() {
        assert!(atlas_migration_sources().is_empty());
        assert!(matches!(
            AtlasSource::from_ron("(version: 0, sprites: [])"),
            Err(AtlasError::Migration(_))
        ));
        assert!(matches!(
            AtlasSource::from_ron("(sprites: [])"),
            Err(AtlasError::Parse(_))
        ));
        let source = AtlasSource::from_ron(
            r#"(version: 1, max_size: 16, padding: 16, extrude: 16, sprites: [(name: "a", image: "./a.png", pivot: (0.0, 1.0))])"#,
        )
        .expect("boundary values parse");
        assert_eq!(source.max_size, 16);
        assert_eq!(source.padding, 16);
        assert_eq!(source.extrude, 16);
        assert_eq!(source.sprites[0].image, "./a.png");
        assert_eq!(source.sprites[0].pivot, [0.0, 1.0]);
        let mut current = source;
        assert_eq!(current.migrate_to_latest().expect("already current"), 1);
        assert_eq!(current.version, 1);

        assert!(matches!(
            AtlasSource::from_ron("(version: 1, max_size: 15, sprites: [])"),
            Err(AtlasError::Invalid(_))
        ));
        assert!(matches!(
            AtlasSource::from_ron("(version: 1, max_size: 16385, sprites: [])"),
            Err(AtlasError::Invalid(_))
        ));
        let biggest = AtlasSource::from_ron("(version: 1, max_size: 16384, sprites: [])").expect("max page");
        assert_eq!(biggest.max_size, 16_384);
        assert!(matches!(
            AtlasSource::from_ron("(version: 1, padding: 17, sprites: [])"),
            Err(AtlasError::Invalid(_))
        ));
        assert!(matches!(
            AtlasSource::from_ron("(version: 1, extrude: 17, sprites: [])"),
            Err(AtlasError::Invalid(_))
        ));
        assert!(matches!(
            AtlasSource::from_ron(r#"(version: 1, sprites: [(name: "", image: "a.png")])"#),
            Err(AtlasError::Invalid(_))
        ));
        assert!(matches!(
            AtlasSource::from_ron(
                r#"(version: 1, sprites: [(name: "a", image: "a.png", pivot: (-0.1, 0.5))])"#
            ),
            Err(AtlasError::Invalid(_))
        ));

        assert_eq!(
            RgbaImage {
                width: 0,
                height: 0,
                rgba8: Vec::new()
            }
            .validate()
            .expect_err("empty"),
            "image 0x0 has 0 bytes (expected 0)"
        );
        assert_eq!(
            RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![0; 3]
            }
            .validate()
            .expect_err("short"),
            "image 1x1 has 3 bytes (expected 4)"
        );

        let nine = NineSlice::from_array([1, 2, 3, 4]);
        assert_eq!((nine.left, nine.top, nine.right, nine.bottom), (1, 2, 3, 4));

        let mut old = SpriteAtlas::single("dot", pixel());
        old.version = 0;
        assert!(matches!(
            SpriteAtlas::from_postcard(&old.to_postcard().expect("encodes")),
            Err(AtlasError::Version {
                found: 0,
                expected: COOKED_ATLAS_VERSION
            })
        ));
        old.version = 2;
        assert!(matches!(
            SpriteAtlas::from_postcard(&old.to_postcard().expect("encodes")),
            Err(AtlasError::Version {
                found: 2,
                expected: COOKED_ATLAS_VERSION
            })
        ));
        assert!(matches!(
            SpriteAtlas::from_postcard(&[]),
            Err(AtlasError::Decode(_))
        ));

        let mut missing = SpriteAtlas::single("dot", pixel());
        missing.sprites[0].page = 1;
        assert!(matches!(missing.validate(), Err(AtlasError::Invalid(_))));
        let mut unsorted = SpriteAtlas::single("dot", pixel());
        unsorted.sprites.push(AtlasSprite {
            name: "a".into(),
            ..unsorted.sprites[0].clone()
        });
        assert!(matches!(unsorted.validate(), Err(AtlasError::Invalid(_))));
    }
}

#[cfg(test)]
mod format_doc_tests {
    use super::*;

    #[test]
    fn the_published_example_parses_and_validates() {
        let text = include_str!("../../../docs/formats/atlas.example.ron");
        let source = AtlasSource::from_ron(text).expect("docs/formats/atlas.example.ron parses");
        assert_eq!(source.sprites.len(), 3);
        assert_eq!(source.sprites[2].nine_slice, Some([4, 4, 4, 4]));
    }
}
