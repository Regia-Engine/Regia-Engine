// SPDX-License-Identifier: MPL-2.0
//! Tiled map import (`docs/specs/22-tier2.md` § T2.10): `.tmx` (with `.tsx` tilesets) through the
//! `tiled` crate, and Tiled's JSON export (`.tmj` with `.tsj`/`.tsx` tilesets) through serde, both
//! into a [`TileMapAsset`] — the frozen `.tilemap.ron` structure.
//!
//! Supported: finite maps in every Tiled orientation (orthogonal, isometric, staggered and
//! hexagonal with either stagger axis and index, ADR-2597; object layers keep Tiled's own object
//! coordinates, which `TileMapAsset::object_world_position` converts per orientation, ADR-2714); tile layers (CSV, XML, base64 raw/zlib/gzip; zstd is not
//! built in), object layers, group layers (flattened to `group/name`, offsets added, opacity
//! multiplied, visibility and-ed), single-image tilesets with spacing and margin, flips, animated
//! tiles, tile classes and custom properties. Image layers and list/class properties are skipped
//! with a warning. An image-collection tileset (no single tileset image) is an error and fails the
//! import. A text object is stored as a rectangle of its width and height, with a warning. Infinite
//! maps are errors.
//!
//! Collision ([`TileCollision`]) is derived per tile: a `collision` string property `solid` /
//! `one_way` (also `oneway`, `jump_through`), a bool property `solid` / `one_way`, the tile's
//! class with one of those names, or — for `Solid` — any collision shape drawn in Tiled's
//! collision editor.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use regia_2d::atlas::RgbaImage;
use regia_2d::tilemap::{
    COOKED_TILEMAP_VERSION, CookedTileMap, FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, MapObject,
    ObjectLayer, ObjectShape, Orientation, PropertyValue, StaggerAxis, StaggerIndex, TILEMAP_FORMAT_VERSION,
    TileCollision, TileDef, TileFrame, TileLayer, TileMapAsset, TilesetDef,
};
use serde::Deserialize;

/// Tiled import errors.
#[derive(Debug, thiserror::Error)]
pub enum TiledImportError {
    /// The `tiled` crate rejected the TMX/TSX.
    #[error("{path}: {reason}")]
    Tmx {
        /// File.
        path: String,
        /// Parser message.
        reason: String,
    },
    /// The JSON map or tileset does not parse.
    #[error("{path}: {reason}")]
    Json {
        /// File.
        path: String,
        /// Parser message.
        reason: String,
    },
    /// A feature Regia Engine does not import.
    #[error("{path}: unsupported: {what}")]
    Unsupported {
        /// File.
        path: String,
        /// What.
        what: String,
    },
    /// The resulting map does not validate.
    #[error("{0}")]
    Map(#[from] regia_2d::tilemap::TileMapError),
    /// A tileset image is missing or unreadable.
    #[error("tileset `{tileset}`: image `{path}`: {reason}")]
    Image {
        /// Tileset name.
        tileset: String,
        /// Image path.
        path: String,
        /// What went wrong.
        reason: String,
    },
}

/// The imported map plus what was skipped.
#[derive(Clone, Debug)]
pub struct ImportedTileMap {
    /// The map (`.tilemap.ron` structure).
    pub map: TileMapAsset,
    /// Skipped features, one line each.
    pub warnings: Vec<String>,
}

/// True for the source extensions this importer reads as maps (`tmx`, `tmj`).
#[must_use]
pub fn is_tiled_map(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tmx") || e.eq_ignore_ascii_case("tmj"))
}

/// True for Tiled tileset files (`tsx`, `tsj`): they are read through the maps that use them.
#[must_use]
pub fn is_tiled_tileset(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tsx") || e.eq_ignore_ascii_case("tsj"))
}

/// Imports a Tiled map by extension: `.tmx` (XML) or `.tmj`/`.json` (JSON).
pub fn import_tiled(path: &Path) -> Result<ImportedTileMap, TiledImportError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let imported = if ext == "tmx" {
        import_tmx(path)?
    } else {
        import_tmj(path)?
    };
    imported.map.validate()?;
    Ok(imported)
}

/// A Tiled `orientation` string and its stagger settings as an [`Orientation`]. Tiled defaults to
/// stagger axis `y` and index `odd`; `axis_x` and `index_even` are true for `x` and `even`.
fn orientation_of(
    name: &str,
    axis_x: bool,
    index_even: bool,
    side_length: u32,
) -> Result<Orientation, String> {
    let axis = if axis_x { StaggerAxis::X } else { StaggerAxis::Y };
    let index = if index_even {
        StaggerIndex::Even
    } else {
        StaggerIndex::Odd
    };
    match name {
        "orthogonal" => Ok(Orientation::Orthogonal),
        "isometric" => Ok(Orientation::Isometric),
        "staggered" => Ok(Orientation::Staggered { axis, index }),
        "hexagonal" => Ok(Orientation::Hexagonal {
            axis,
            index,
            side_length,
        }),
        other => Err(format!("`{other}` orientation")),
    }
}

/// Loads a `.tsx` tileset through the `tiled` crate with the importer's input limits.
pub(crate) fn load_tsx(path: &Path) -> Result<tiled::Tileset, TiledImportError> {
    tiled_loader(path)
        .load_tsx_tileset(path)
        .map_err(|e| TiledImportError::Tmx {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
}

/// Loads a `.tmx` map through the `tiled` crate with the importer's input limits.
pub(crate) fn load_tmx(path: &Path) -> Result<tiled::Map, TiledImportError> {
    tiled_loader(path)
        .load_tmx_map(path)
        .map_err(|e| TiledImportError::Tmx {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
}

/// Converts a Tiled map to `.tilemap.ron` text.
pub fn tiled_to_tilemap_ron(path: &Path) -> Result<(String, Vec<String>), TiledImportError> {
    let imported = import_tiled(path)?;
    Ok((imported.map.to_ron()?, imported.warnings))
}

/// Loads the tileset images of `map` (paths relative to `map_dir`) and builds the cooked form.
pub fn cook_tilemap(map: TileMapAsset, map_dir: &Path) -> Result<CookedTileMap, TiledImportError> {
    let mut images = Vec::with_capacity(map.tilesets.len());
    for set in &map.tilesets {
        let err = |reason: String| TiledImportError::Image {
            tileset: set.name.clone(),
            path: set.image.clone(),
            reason,
        };
        let path = crate::cook::join_portable(map_dir, &set.image).map_err(err)?;
        let bytes = std::fs::read(path).map_err(|e| err(e.to_string()))?;
        let image = image::load_from_memory(&bytes)
            .map_err(|e| err(e.to_string()))?
            .to_rgba8();
        // The sampler clamps to the edge, so a tile past the image would draw the last row.
        let (width, height) = (u64::from(image.width()), u64::from(image.height()));
        let fits = |id: u32| {
            set.tile_rect(id).is_some_and(|[x, y, w, h]| {
                u64::from(x) + u64::from(w) <= width && u64::from(y) + u64::from(h) <= height
            })
        };
        if set.tile_count > 0 && !(fits(set.tile_count.min(set.columns) - 1) && fits(set.tile_count - 1)) {
            return Err(err(format!(
                "{width}x{height} pixels do not hold {} tiles in {} columns",
                set.tile_count, set.columns
            )));
        }
        images.push(RgbaImage {
            width: image.width(),
            height: image.height(),
            rgba8: image.into_raw(),
        });
    }
    let cooked = CookedTileMap {
        version: COOKED_TILEMAP_VERSION,
        map,
        images,
    };
    cooked.validate()?;
    Ok(cooked)
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Collision kind from a tile's class and properties.
fn collision_of(
    class: &str,
    properties: &BTreeMap<String, PropertyValue>,
    has_shapes: bool,
) -> TileCollision {
    let parse = |s: &str| match s.to_ascii_lowercase().as_str() {
        "solid" => Some(TileCollision::Solid),
        "one_way" | "oneway" | "one-way" | "jump_through" | "jumpthrough" | "platform" => {
            Some(TileCollision::OneWay)
        }
        "none" | "empty" => Some(TileCollision::Empty),
        _ => None,
    };
    if let Some(PropertyValue::String(s)) = properties.get("collision")
        && let Some(c) = parse(s)
    {
        return c;
    }
    if matches!(properties.get("one_way"), Some(PropertyValue::Bool(true)))
        || matches!(properties.get("jump_through"), Some(PropertyValue::Bool(true)))
    {
        return TileCollision::OneWay;
    }
    if matches!(properties.get("solid"), Some(PropertyValue::Bool(true))) {
        return TileCollision::Solid;
    }
    if let Some(c) = parse(class) {
        return c;
    }
    if has_shapes {
        TileCollision::Solid
    } else {
        TileCollision::Empty
    }
}

/// `path` relative to `base` when it is inside it, with `/` separators.
fn relative_to(path: &Path, base: &Path) -> String {
    let rel = path.strip_prefix(base).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Joins a relative path to a base directory as written, `/`-separated, keeping `..`.
fn join_rel(base: &str, rel: &str) -> String {
    if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Flattened group state while walking layers.
#[derive(Clone)]
struct GroupCtx {
    prefix: String,
    offset: [f32; 2],
    opacity: f32,
    visible: bool,
}

impl GroupCtx {
    fn root() -> Self {
        Self {
            prefix: String::new(),
            offset: [0.0, 0.0],
            opacity: 1.0,
            visible: true,
        }
    }
    fn name(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.prefix)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TMX / TSX (tiled crate)
// ---------------------------------------------------------------------------------------------

fn tmx_color(c: tiled::Color) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.alpha, c.red, c.green, c.blue)
}

fn tmx_properties(
    props: &tiled::Properties,
    what: &str,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, PropertyValue> {
    let mut out = BTreeMap::new();
    for (name, value) in props {
        let v = match value {
            tiled::PropertyValue::BoolValue(b) => PropertyValue::Bool(*b),
            tiled::PropertyValue::FloatValue(f) => PropertyValue::Float(f64::from(*f)),
            tiled::PropertyValue::IntValue(i) => PropertyValue::Int(i64::from(*i)),
            tiled::PropertyValue::ColorValue(c) => PropertyValue::Color(tmx_color(*c)),
            tiled::PropertyValue::StringValue(s) => PropertyValue::String(s.clone()),
            tiled::PropertyValue::FileValue(s) => PropertyValue::File(s.clone()),
            tiled::PropertyValue::ObjectValue(id) => PropertyValue::Object(*id),
            tiled::PropertyValue::ListValue(_) | tiled::PropertyValue::ClassValue { .. } => {
                warnings.push(format!("{what}: list/class property `{name}` skipped"));
                continue;
            }
        };
        out.insert(name.clone(), v);
    }
    out
}

/// Largest Tiled XML file the importer reads.
pub const MAX_TILED_XML_BYTES: u64 = 32 * 1024 * 1024;
/// Most attributes one XML tag may carry (Tiled writes at most a dozen).
pub const MAX_TILED_XML_ATTRIBUTES: usize = 256;

/// Rejects XML that could make `quick-xml` 0.31 (pinned by `tiled`) spend quadratic time on
/// duplicate-attribute checks (RUSTSEC-2026-0194): more than [`MAX_TILED_XML_ATTRIBUTES`]
/// attributes in one tag. Counts `=` outside quoted values inside each `<…>` (ADR-110).
fn check_xml_shape(bytes: &[u8]) -> Result<(), String> {
    let (mut in_tag, mut quote, mut attributes) = (false, 0u8, 0usize);
    for &b in bytes {
        match (in_tag, quote, b) {
            (false, _, b'<') => {
                in_tag = true;
                attributes = 0;
            }
            (true, 0, b'"' | b'\'') => quote = b,
            (true, q, c) if q != 0 && c == q => quote = 0,
            (true, 0, b'=') => {
                attributes += 1;
                if attributes > MAX_TILED_XML_ATTRIBUTES {
                    return Err(format!(
                        "a tag has more than {MAX_TILED_XML_ATTRIBUTES} attributes"
                    ));
                }
            }
            (true, 0, b'>') => in_tag = false,
            _ => {}
        }
    }
    Ok(())
}

/// Most tiles one map or tile layer may hold, `.tmx` and `.tmj` alike (an 8192 x 8192 map): bounds
/// the memory a layer's data may decode or inflate into (`cells * 4` bytes) and the importer's
/// per-layer output (threat-model finding 20, ADR-4922).
pub const MAX_TILED_CELLS: usize = 1 << 26;

/// `width * height` of a map or layer, refused past [`MAX_TILED_CELLS`] (checked: a `u32` product
/// overflows).
fn checked_cells(width: u64, height: u64) -> Result<usize, String> {
    width
        .checked_mul(height)
        .and_then(|c| usize::try_from(c).ok())
        .filter(|c| *c <= MAX_TILED_CELLS)
        .ok_or_else(|| format!("a {width} x {height} map (more than {MAX_TILED_CELLS} tiles per layer)"))
}

/// The `name="value"` attributes of one XML start tag (`tag` from `<` to `>`).
fn xml_attrs(tag: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = tag;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq]
            .rsplit(|c: char| c.is_whitespace() || c == '<')
            .next()
            .unwrap_or("");
        let after = rest[eq + 1..].trim_start();
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let Some(close) = after[1..].find(quote) else {
            break;
        };
        out.push((name.trim(), &after[1..=close]));
        rest = &after[close + 2..];
    }
    out
}

/// The byte index of the `>` that ends the start tag at the beginning of `text` (quotes respected).
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// The next start tag named `name` in `text`: (its offset, the tag text through `>`).
fn next_tag<'a>(text: &'a str, name: &str) -> Option<(usize, &'a str)> {
    let mut from = 0;
    let open = format!("<{name}");
    while let Some(i) = text[from..].find(&open) {
        let at = from + i;
        let after = &text[at + open.len()..];
        if after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            let end = tag_end(&text[at..])?;
            return Some((at, &text[at..=at + end]));
        }
        from = at + open.len();
    }
    None
}

/// Checks every finite tile layer of a Tiled XML file before the `tiled` crate decodes it: the
/// layer's `width * height` is within [`MAX_TILED_CELLS`] and its data holds exactly that many tiles.
/// The crate inflates zlib and gzip data with no limit, and indexes a layer's tiles by its declared
/// size, so a short layer panicked the importer; compressed data is inflated here at most one tile
/// past the declared size (ADR-4922).
fn check_tile_layers(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Tiled XML must be UTF-8".to_string())?;
    let mut rest = text;
    while let Some((at, tag)) = next_tag(rest, "layer") {
        let attrs = xml_attrs(tag);
        let get = |key: &str| attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let name = get("name").unwrap_or("");
        let dim = |key: &str| get(key).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        let cells = checked_cells(dim("width"), dim("height")).map_err(|e| format!("layer `{name}`: {e}"))?;
        let body_start = at + tag.len();
        if tag.ends_with("/>") {
            rest = &rest[body_start..];
            continue;
        }
        let body_end = rest[body_start..]
            .find("</layer>")
            .map_or(rest.len(), |i| body_start + i);
        let body = &rest[body_start..body_end];
        rest = &rest[body_end..];
        let Some((data_at, data_tag)) = next_tag(body, "data") else {
            continue;
        };
        let data_attrs = xml_attrs(data_tag);
        let data_get = |key: &str| data_attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        if data_tag.ends_with("/>") || body.contains("<chunk") {
            // Empty data is the crate's error; chunks belong to infinite maps, refused after load.
            continue;
        }
        let payload_start = data_at + data_tag.len();
        let payload = &body[payload_start..];
        let payload = &payload[..payload.find("</data>").unwrap_or(payload.len())];
        let payload = payload.trim();
        let payload = payload
            .strip_prefix("<![CDATA[")
            .and_then(|p| p.strip_suffix("]]>"))
            .unwrap_or(payload);
        if payload.contains('&') {
            return Err(format!("layer `{name}`: entities in tile data"));
        }
        let bad = |what: String| format!("layer `{name}`: {what}");
        let found = match (data_get("encoding"), data_get("compression")) {
            (Some("csv"), None) => payload.split(',').count(),
            (Some("base64"), compression) => {
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(payload.trim())
                    .map_err(|e| bad(format!("base64: {e}")))?;
                let legal = cells.saturating_mul(4);
                let inflated = |reader: &mut dyn Read, codec: &str| -> Result<usize, String> {
                    let mut out = Vec::new();
                    reader
                        .take(legal as u64 + 1)
                        .read_to_end(&mut out)
                        .map_err(|e| bad(format!("{codec}: {e}")))?;
                    if out.len() > legal {
                        return Err(bad(format!(
                            "{codec} data inflates past the {legal} bytes of a {cells}-tile layer"
                        )));
                    }
                    Ok(out.len())
                };
                let len = match compression {
                    None => raw.len(),
                    Some("zlib") => inflated(&mut flate2::read::ZlibDecoder::new(raw.as_slice()), "zlib")?,
                    Some("gzip") => inflated(&mut flate2::read::GzDecoder::new(raw.as_slice()), "gzip")?,
                    // zstd is not built in: the crate refuses it.
                    Some(_) => continue,
                };
                len / 4
            }
            // XML-encoded data and unknown encodings are the crate's error.
            _ => continue,
        };
        if found != cells {
            return Err(bad(format!("{found} tiles, expected {cells}")));
        }
    }
    Ok(())
}

/// The `tiled` crate's file reader with Regia Engine's input limits: every `.tmx`, `.tsx` and template
/// it opens is size-capped and shape-checked first (see [`check_xml_shape`] and
/// [`check_tile_layers`]), and every file other than the one asked for (an external tileset, a
/// template) is a path the entry names, held to `cook::join_portable`'s rule: no absolute or drive
/// path, the shared segment rule, and inside a project no `..` or symlink out of it (ADR-4922).
#[derive(Clone, Debug)]
struct GuardedReader {
    /// The file the loader was asked to open.
    entry: PathBuf,
    /// Its folder: what the files it names are relative to.
    dir: PathBuf,
}

impl GuardedReader {
    fn confine(&self, path: &Path) -> Result<(), String> {
        if path == self.entry {
            return Ok(());
        }
        // The crate joins the entry's folder with the name as written: an absolute name replaced it.
        let rel = path
            .strip_prefix(&self.dir)
            .map_err(|_| format!("{} is not relative to the map", path.display()))?;
        let rel = rel
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
        crate::cook::join_portable(&self.dir, rel).map(|_| ())
    }
}

impl tiled::ResourceReader for GuardedReader {
    type Resource = std::io::Cursor<Vec<u8>>;
    type Error = std::io::Error;

    fn read_from(&mut self, path: &Path) -> Result<Self::Resource, Self::Error> {
        let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        self.confine(path).map_err(invalid)?;
        let len = std::fs::metadata(path)?.len();
        if len > MAX_TILED_XML_BYTES {
            return Err(invalid(format!(
                "{len} bytes is over the {MAX_TILED_XML_BYTES}-byte limit"
            )));
        }
        let bytes = std::fs::read(path)?;
        check_xml_shape(&bytes).map_err(invalid)?;
        check_tile_layers(&bytes).map_err(invalid)?;
        Ok(std::io::Cursor::new(bytes))
    }
}

/// A `tiled` loader that opens `entry` and the files it names through [`GuardedReader`].
fn tiled_loader(entry: &Path) -> tiled::Loader<tiled::DefaultResourceCache, GuardedReader> {
    let reader = GuardedReader {
        entry: entry.to_path_buf(),
        dir: entry.parent().unwrap_or(Path::new("")).to_path_buf(),
    };
    tiled::Loader::with_cache_and_reader(tiled::DefaultResourceCache::new(), reader)
}

fn import_tmx(path: &Path) -> Result<ImportedTileMap, TiledImportError> {
    let shown = path.display().to_string();
    let mut loader = tiled_loader(path);
    let map = loader.load_tmx_map(path).map_err(|e| TiledImportError::Tmx {
        path: shown.clone(),
        reason: e.to_string(),
    })?;
    let unsupported = |what: &str| TiledImportError::Unsupported {
        path: shown.clone(),
        what: what.to_string(),
    };
    let orientation = orientation_of(
        match map.orientation {
            tiled::Orientation::Orthogonal => "orthogonal",
            tiled::Orientation::Isometric => "isometric",
            tiled::Orientation::Staggered => "staggered",
            tiled::Orientation::Hexagonal => "hexagonal",
            tiled::Orientation::Oblique => "oblique",
        },
        matches!(map.stagger_axis, tiled::StaggerAxis::X),
        matches!(map.stagger_index, tiled::StaggerIndex::Even),
        map.hex_side_length.map_or(0, |v| u32::try_from(v).unwrap_or(0)),
    )
    .map_err(|what| unsupported(&what))?;
    if map.infinite() {
        return Err(unsupported("infinite map (resize it to a fixed size in Tiled)"));
    }
    // Finding 20: the `.tmj` reader's cap, checked (a `u32` product wraps), before any layer.
    checked_cells(u64::from(map.width), u64::from(map.height)).map_err(|what| unsupported(&what))?;
    let map_dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut warnings = Vec::new();

    // GIDs: tilesets numbered consecutively in map order.
    let mut first_gids = Vec::new();
    let mut tilesets = Vec::new();
    let mut next = 1u32;
    for set in map.tilesets() {
        let Some(image) = &set.image else {
            return Err(unsupported(&format!(
                "image-collection tileset `{}` (pack it into one image or an .atlas.ron)",
                set.name
            )));
        };
        let mut tiles = Vec::new();
        for (id, tile) in set.tiles() {
            let what = format!("tileset `{}` tile {id}", set.name);
            let properties = tmx_properties(&tile.properties, &what, &mut warnings);
            let has_shapes = tile
                .collision
                .as_ref()
                .is_some_and(|c| !c.object_data().is_empty());
            let def = TileDef {
                id,
                class: tile.user_type.clone(),
                collision: collision_of(&tile.user_type, &properties, has_shapes),
                animation: tile
                    .animation
                    .as_ref()
                    .map(|frames| {
                        frames
                            .iter()
                            .map(|f| TileFrame {
                                tile: f.tile_id,
                                duration_ms: f.duration,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                properties,
            };
            if !def.class.is_empty()
                || def.collision != TileCollision::Empty
                || !def.animation.is_empty()
                || !def.properties.is_empty()
            {
                tiles.push(def);
            }
        }
        tiles.sort_by_key(|t| t.id);
        first_gids.push(next);
        tilesets.push(TilesetDef {
            name: set.name.clone(),
            first_gid: next,
            image: relative_to(&image.source, &map_dir),
            image_width: image.width.max(0) as u32,
            image_height: image.height.max(0) as u32,
            tile_width: set.tile_width,
            tile_height: set.tile_height,
            columns: set.columns,
            tile_count: set.tilecount,
            spacing: set.spacing,
            margin: set.margin,
            tiles,
        });
        next = next.saturating_add(set.tilecount.max(1));
    }

    let mut layers = Vec::new();
    for layer in map.layers() {
        tmx_layer(
            &map,
            layer,
            &GroupCtx::root(),
            &first_gids,
            &mut layers,
            &mut warnings,
        )?;
    }

    let out = TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation,
        width: map.width,
        height: map.height,
        tile_width: map.tile_width,
        tile_height: map.tile_height,
        background: map.background_color.map(tmx_color),
        tilesets,
        layers,
        properties: tmx_properties(&map.properties, "map", &mut warnings),
    };
    Ok(ImportedTileMap { map: out, warnings })
}

fn tmx_layer(
    map: &tiled::Map,
    layer: tiled::Layer<'_>,
    group: &GroupCtx,
    first_gids: &[u32],
    out: &mut Vec<MapLayer>,
    warnings: &mut Vec<String>,
) -> Result<(), TiledImportError> {
    let name = group.name(&layer.name);
    let offset = [group.offset[0] + layer.offset_x, group.offset[1] + layer.offset_y];
    let opacity = group.opacity * layer.opacity;
    let visible = group.visible && layer.visible;
    let properties = tmx_properties(&layer.properties, &format!("layer `{name}`"), warnings);
    match layer.layer_type() {
        tiled::LayerType::Tiles(tiles) => {
            let tiled::TileLayer::Finite(finite) = tiles else {
                return Err(TiledImportError::Unsupported {
                    path: map.source.display().to_string(),
                    what: format!("infinite tile layer `{name}`"),
                });
            };
            // `import_tmx` checked `width * height` against `MAX_TILED_CELLS`.
            let mut data = Vec::with_capacity((map.width as usize).saturating_mul(map.height as usize));
            for y in 0..map.height as i32 {
                for x in 0..map.width as i32 {
                    let raw = finite.get_tile_data(x, y).map_or(0, |t| {
                        let first = first_gids.get(t.tileset_index()).copied().unwrap_or(0);
                        let mut gid = first + t.id();
                        if t.flip_h {
                            gid |= FLIP_X_BIT;
                        }
                        if t.flip_v {
                            gid |= FLIP_Y_BIT;
                        }
                        if t.flip_d {
                            gid |= FLIP_DIAGONAL_BIT;
                        }
                        gid
                    });
                    data.push(raw);
                }
            }
            out.push(MapLayer::Tiles(TileLayer {
                name,
                visible,
                opacity,
                offset,
                data,
                properties,
            }));
        }
        tiled::LayerType::Objects(objects) => {
            let mut list = Vec::new();
            for object in objects.objects() {
                let what = format!("object {} in `{name}`", object.id());
                let props = tmx_properties(&object.properties, &what, warnings);
                let (shape, size) = match &object.shape {
                    tiled::ObjectShape::Rect { width, height } => (ObjectShape::Rect, [*width, *height]),
                    tiled::ObjectShape::Ellipse { width, height } => {
                        (ObjectShape::Ellipse, [*width, *height])
                    }
                    tiled::ObjectShape::Capsule { width, height } => {
                        warnings.push(format!("{what}: capsule imported as an ellipse"));
                        (ObjectShape::Ellipse, [*width, *height])
                    }
                    tiled::ObjectShape::Polyline { points } => (
                        ObjectShape::Polyline(points.iter().map(|&(x, y)| [x, y]).collect()),
                        [0.0, 0.0],
                    ),
                    tiled::ObjectShape::Polygon { points } => (
                        ObjectShape::Polygon(points.iter().map(|&(x, y)| [x, y]).collect()),
                        [0.0, 0.0],
                    ),
                    tiled::ObjectShape::Point(..) => (ObjectShape::Point, [0.0, 0.0]),
                    tiled::ObjectShape::Text { width, height, .. } => {
                        warnings.push(format!("{what}: text object imported as a rectangle"));
                        (ObjectShape::Rect, [*width, *height])
                    }
                };
                let shape = match object.tile_data() {
                    Some(tile) => match tile.tileset_location() {
                        tiled::TilesetLocation::Map(index) => {
                            let first = first_gids.get(*index).copied().unwrap_or(0);
                            ObjectShape::Tile(first + tile.id())
                        }
                        tiled::TilesetLocation::Template(_) => {
                            warnings.push(format!("{what}: template tile reference imported as its shape"));
                            shape
                        }
                    },
                    None => shape,
                };
                list.push(MapObject {
                    id: object.id(),
                    name: object.name.clone(),
                    class: object.user_type.clone(),
                    position: [object.x, object.y],
                    size,
                    rotation: object.rotation,
                    shape,
                    properties: props,
                });
            }
            out.push(MapLayer::Objects(ObjectLayer {
                name,
                visible,
                objects: list,
                properties,
            }));
        }
        tiled::LayerType::Image(_) => warnings.push(format!("image layer `{name}` skipped")),
        tiled::LayerType::Group(g) => {
            let ctx = GroupCtx {
                prefix: name,
                offset,
                opacity,
                visible,
            };
            for child in g.layers() {
                tmx_layer(map, child, &ctx, first_gids, out, warnings)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// TMJ / TSJ (serde)
// ---------------------------------------------------------------------------------------------

fn yes() -> bool {
    true
}
fn one() -> f32 {
    1.0
}

#[derive(Deserialize)]
struct JProperty {
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    value: serde_json::Value,
}

#[derive(Deserialize)]
struct JMap {
    width: u32,
    height: u32,
    tilewidth: u32,
    tileheight: u32,
    #[serde(default)]
    orientation: String,
    #[serde(default)]
    infinite: bool,
    #[serde(default)]
    staggeraxis: String,
    #[serde(default)]
    staggerindex: String,
    #[serde(default)]
    hexsidelength: i64,
    #[serde(default)]
    backgroundcolor: Option<String>,
    #[serde(default)]
    layers: Vec<JLayer>,
    #[serde(default)]
    tilesets: Vec<serde_json::Value>,
    #[serde(default)]
    properties: Vec<JProperty>,
}

#[derive(Deserialize)]
struct JLayer {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default = "yes")]
    visible: bool,
    #[serde(default = "one")]
    opacity: f32,
    #[serde(default)]
    offsetx: f32,
    #[serde(default)]
    offsety: f32,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    compression: Option<String>,
    #[serde(default)]
    objects: Vec<JObject>,
    #[serde(default)]
    layers: Vec<JLayer>,
    #[serde(default)]
    properties: Vec<JProperty>,
}

#[derive(Deserialize)]
struct JPoint {
    x: f32,
    y: f32,
}

#[derive(Deserialize)]
struct JObject {
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(default, alias = "type")]
    class: String,
    x: f32,
    y: f32,
    #[serde(default)]
    width: f32,
    #[serde(default)]
    height: f32,
    #[serde(default)]
    rotation: f32,
    #[serde(default)]
    gid: Option<u32>,
    #[serde(default)]
    point: bool,
    #[serde(default)]
    ellipse: bool,
    #[serde(default)]
    polygon: Option<Vec<JPoint>>,
    #[serde(default)]
    polyline: Option<Vec<JPoint>>,
    #[serde(default)]
    text: Option<serde_json::Value>,
    #[serde(default)]
    properties: Vec<JProperty>,
}

#[derive(Deserialize)]
struct JFrame {
    tileid: u32,
    duration: u32,
}

#[derive(Deserialize)]
struct JTile {
    id: u32,
    #[serde(default, alias = "type")]
    class: String,
    #[serde(default)]
    properties: Vec<JProperty>,
    #[serde(default)]
    animation: Vec<JFrame>,
    #[serde(default)]
    objectgroup: Option<serde_json::Value>,
    #[serde(default)]
    image: Option<String>,
}

#[derive(Deserialize)]
struct JTileset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    imagewidth: u32,
    #[serde(default)]
    imageheight: u32,
    tilewidth: u32,
    tileheight: u32,
    #[serde(default)]
    tilecount: u32,
    #[serde(default)]
    columns: u32,
    #[serde(default)]
    spacing: u32,
    #[serde(default)]
    margin: u32,
    #[serde(default)]
    tiles: Vec<JTile>,
}

fn json_properties(
    props: &[JProperty],
    what: &str,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, PropertyValue> {
    let mut out = BTreeMap::new();
    for p in props {
        let v = match (p.kind.as_str(), &p.value) {
            ("bool", serde_json::Value::Bool(b)) => PropertyValue::Bool(*b),
            ("int", v) if v.as_i64().is_some() => PropertyValue::Int(v.as_i64().unwrap_or(0)),
            ("float", v) if v.as_f64().is_some() => PropertyValue::Float(v.as_f64().unwrap_or(0.0)),
            ("color", serde_json::Value::String(s)) => PropertyValue::Color(s.clone()),
            ("file", serde_json::Value::String(s)) => PropertyValue::File(s.clone()),
            ("object", v) if v.as_u64().is_some() => PropertyValue::Object(v.as_u64().unwrap_or(0) as u32),
            ("string" | "", serde_json::Value::String(s)) => PropertyValue::String(s.clone()),
            (kind, _) => {
                warnings.push(format!("{what}: {kind} property `{}` skipped", p.name));
                continue;
            }
        };
        out.insert(p.name.clone(), v);
    }
    out
}

/// Decodes a JSON tile layer's `data`: an array of GIDs, or base64 (raw, zlib or gzip).
fn json_layer_data(layer: &JLayer, cells: usize, shown: &str) -> Result<Vec<u32>, TiledImportError> {
    let bad = |reason: String| TiledImportError::Json {
        path: shown.to_string(),
        reason: format!("layer `{}`: {reason}", layer.name),
    };
    let data = match &layer.data {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_u64()
                    .map(|g| g as u32)
                    .ok_or_else(|| bad("non-integer tile".into()))
            })
            .collect::<Result<Vec<u32>, _>>()?,
        Some(serde_json::Value::String(text)) if layer.encoding.as_deref() == Some("base64") => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(text.trim())
                .map_err(|e| bad(format!("base64: {e}")))?;
            // The legal output is exactly `cells * 4` bytes: inflate at most one byte past it, so
            // a small compressed layer cannot expand into gigabytes before the length check.
            let legal = cells.saturating_mul(4);
            let inflate = |reader: &mut dyn Read, codec: &str| -> Result<Vec<u8>, TiledImportError> {
                let mut out = Vec::new();
                reader
                    .take(legal as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| bad(format!("{codec}: {e}")))?;
                if out.len() > legal {
                    return Err(bad(format!(
                        "{codec} data inflates past the {legal} bytes of a {cells}-tile layer"
                    )));
                }
                Ok(out)
            };
            let bytes = match layer.compression.as_deref().unwrap_or("") {
                "" => raw,
                "zlib" => inflate(&mut flate2::read::ZlibDecoder::new(raw.as_slice()), "zlib")?,
                "gzip" => inflate(&mut flate2::read::GzDecoder::new(raw.as_slice()), "gzip")?,
                other => return Err(bad(format!("unsupported compression `{other}`"))),
            };
            let (chunks, rest) = bytes.as_chunks::<4>();
            if !rest.is_empty() {
                return Err(bad("tile data is not a multiple of 4 bytes".into()));
            }
            chunks.iter().map(|c| u32::from_le_bytes(*c)).collect()
        }
        _ => return Err(bad("missing or unsupported tile data".into())),
    };
    if data.len() != cells {
        return Err(bad(format!("{} tiles, expected {cells}", data.len())));
    }
    Ok(data)
}

fn json_tileset(value: &serde_json::Value, path: &str) -> Result<JTileset, TiledImportError> {
    serde_json::from_value(value.clone()).map_err(|e| TiledImportError::Json {
        path: path.to_string(),
        reason: format!("tileset: {e}"),
    })
}

/// Reads an external tileset referenced from a JSON map: `.tsj`/`.json` (JSON) or `.tsx` (XML),
/// returning it as a [`JTileset`] plus its directory relative to the map.
fn external_tileset(
    map_dir: &Path,
    source: &str,
    warnings: &mut Vec<String>,
) -> Result<(JTileset, String), TiledImportError> {
    let path: PathBuf =
        crate::cook::join_portable(map_dir, source).map_err(|reason| TiledImportError::Tmx {
            path: source.to_string(),
            reason,
        })?;
    let shown = path.display().to_string();
    let rel_dir = Path::new(source)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tsx"))
    {
        let set = tiled_loader(&path)
            .load_tsx_tileset(&path)
            .map_err(|e| TiledImportError::Tmx {
                path: shown.clone(),
                reason: e.to_string(),
            })?;
        let image = set
            .image
            .as_ref()
            .map(|i| relative_to(&i.source, path.parent().unwrap_or(Path::new(""))));
        let mut tiles = Vec::new();
        for (id, tile) in set.tiles() {
            let what = format!("tileset `{}` tile {id}", set.name);
            let mut props = Vec::new();
            for (name, value) in tmx_properties(&tile.properties, &what, warnings) {
                let (kind, value) = match value {
                    PropertyValue::Bool(b) => ("bool", serde_json::json!(b)),
                    PropertyValue::Int(i) => ("int", serde_json::json!(i)),
                    PropertyValue::Float(f) => ("float", serde_json::json!(f)),
                    PropertyValue::String(s) => ("string", serde_json::json!(s)),
                    PropertyValue::Color(s) => ("color", serde_json::json!(s)),
                    PropertyValue::File(s) => ("file", serde_json::json!(s)),
                    PropertyValue::Object(o) => ("object", serde_json::json!(o)),
                };
                props.push(JProperty {
                    name,
                    kind: kind.into(),
                    value,
                });
            }
            tiles.push(JTile {
                id,
                class: tile.user_type.clone(),
                properties: props,
                animation: tile
                    .animation
                    .as_ref()
                    .map(|f| {
                        f.iter()
                            .map(|f| JFrame {
                                tileid: f.tile_id,
                                duration: f.duration,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                objectgroup: tile
                    .collision
                    .as_ref()
                    .filter(|c| !c.object_data().is_empty())
                    .map(|_| serde_json::json!({"objects": [{}]})),
                image: None,
            });
        }
        let (iw, ih) = set
            .image
            .as_ref()
            .map_or((0, 0), |i| (i.width.max(0) as u32, i.height.max(0) as u32));
        return Ok((
            JTileset {
                name: set.name.clone(),
                image,
                imagewidth: iw,
                imageheight: ih,
                tilewidth: set.tile_width,
                tileheight: set.tile_height,
                tilecount: set.tilecount,
                columns: set.columns,
                spacing: set.spacing,
                margin: set.margin,
                tiles,
            },
            rel_dir,
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| TiledImportError::Json {
        path: shown.clone(),
        reason: e.to_string(),
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| TiledImportError::Json {
        path: shown.clone(),
        reason: e.to_string(),
    })?;
    Ok((json_tileset(&value, &shown)?, rel_dir))
}

fn import_tmj(path: &Path) -> Result<ImportedTileMap, TiledImportError> {
    let shown = path.display().to_string();
    let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_TILED_XML_BYTES);
    if too_big {
        return Err(TiledImportError::Json {
            path: shown,
            reason: format!("over the {MAX_TILED_XML_BYTES}-byte limit"),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| TiledImportError::Json {
        path: shown.clone(),
        reason: e.to_string(),
    })?;
    let map: JMap = serde_json::from_str(&text).map_err(|e| TiledImportError::Json {
        path: shown.clone(),
        reason: e.to_string(),
    })?;
    let unsupported = |what: String| TiledImportError::Unsupported {
        path: shown.clone(),
        what,
    };
    let orientation = orientation_of(
        if map.orientation.is_empty() {
            "orthogonal"
        } else {
            &map.orientation
        },
        map.staggeraxis == "x",
        map.staggerindex == "even",
        u32::try_from(map.hexsidelength).unwrap_or(0),
    )
    .map_err(unsupported)?;
    if map.infinite {
        return Err(unsupported(
            "infinite map (resize it to a fixed size in Tiled)".into(),
        ));
    }
    let map_dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut warnings = Vec::new();

    let mut tilesets = Vec::new();
    for entry in &map.tilesets {
        let first_gid = entry
            .get("firstgid")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TiledImportError::Json {
                path: shown.clone(),
                reason: "tileset without firstgid".into(),
            })? as u32;
        let (set, dir) = match entry.get("source").and_then(serde_json::Value::as_str) {
            Some(source) => external_tileset(&map_dir, source, &mut warnings)?,
            None => (json_tileset(entry, &shown)?, String::new()),
        };
        let Some(image) = set.image.clone() else {
            return Err(unsupported(format!(
                "image-collection tileset `{}` (pack it into one image or an .atlas.ron)",
                set.name
            )));
        };
        if set.tiles.iter().any(|t| t.image.is_some()) {
            return Err(unsupported(format!("per-tile images in tileset `{}`", set.name)));
        }
        let columns = if set.columns > 0 {
            set.columns
        } else {
            (set.imagewidth.saturating_sub(2 * set.margin) + set.spacing)
                / (set.tilewidth + set.spacing).max(1)
        };
        let mut tiles = Vec::new();
        for t in &set.tiles {
            let what = format!("tileset `{}` tile {}", set.name, t.id);
            let properties = json_properties(&t.properties, &what, &mut warnings);
            let has_shapes = t
                .objectgroup
                .as_ref()
                .and_then(|g| g.get("objects"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|o| !o.is_empty());
            let def = TileDef {
                id: t.id,
                class: t.class.clone(),
                collision: collision_of(&t.class, &properties, has_shapes),
                animation: t
                    .animation
                    .iter()
                    .map(|f| TileFrame {
                        tile: f.tileid,
                        duration_ms: f.duration,
                    })
                    .collect(),
                properties,
            };
            if !def.class.is_empty()
                || def.collision != TileCollision::Empty
                || !def.animation.is_empty()
                || !def.properties.is_empty()
            {
                tiles.push(def);
            }
        }
        tiles.sort_by_key(|t| t.id);
        tilesets.push(TilesetDef {
            name: set.name.clone(),
            first_gid,
            image: join_rel(&dir, &image),
            image_width: set.imagewidth,
            image_height: set.imageheight,
            tile_width: set.tilewidth,
            tile_height: set.tileheight,
            columns,
            tile_count: set.tilecount,
            spacing: set.spacing,
            margin: set.margin,
            tiles,
        });
    }
    tilesets.sort_by_key(|s| s.first_gid);

    let cells = checked_cells(u64::from(map.width), u64::from(map.height)).map_err(unsupported)?;
    let mut layers = Vec::new();
    for layer in &map.layers {
        json_layer(
            layer,
            &GroupCtx::root(),
            cells,
            &shown,
            &mut layers,
            &mut warnings,
        )?;
    }
    let out = TileMapAsset {
        version: TILEMAP_FORMAT_VERSION,
        orientation,
        width: map.width,
        height: map.height,
        tile_width: map.tilewidth,
        tile_height: map.tileheight,
        background: map.backgroundcolor.clone(),
        tilesets,
        layers,
        properties: json_properties(&map.properties, "map", &mut warnings),
    };
    Ok(ImportedTileMap { map: out, warnings })
}

fn json_layer(
    layer: &JLayer,
    group: &GroupCtx,
    cells: usize,
    shown: &str,
    out: &mut Vec<MapLayer>,
    warnings: &mut Vec<String>,
) -> Result<(), TiledImportError> {
    let name = group.name(&layer.name);
    let offset = [group.offset[0] + layer.offsetx, group.offset[1] + layer.offsety];
    let opacity = group.opacity * layer.opacity;
    let visible = group.visible && layer.visible;
    let properties = json_properties(&layer.properties, &format!("layer `{name}`"), warnings);
    match layer.kind.as_str() {
        "tilelayer" => out.push(MapLayer::Tiles(TileLayer {
            data: json_layer_data(layer, cells, shown)?,
            name,
            visible,
            opacity,
            offset,
            properties,
        })),
        "objectgroup" => {
            let mut objects = Vec::new();
            for o in &layer.objects {
                let what = format!("object {} in `{name}`", o.id);
                let shape = if let Some(gid) = o.gid {
                    ObjectShape::Tile(gid)
                } else if o.point {
                    ObjectShape::Point
                } else if o.ellipse {
                    ObjectShape::Ellipse
                } else if let Some(points) = &o.polygon {
                    ObjectShape::Polygon(points.iter().map(|p| [p.x, p.y]).collect())
                } else if let Some(points) = &o.polyline {
                    ObjectShape::Polyline(points.iter().map(|p| [p.x, p.y]).collect())
                } else {
                    if o.text.is_some() {
                        warnings.push(format!("{what}: text object imported as a rectangle"));
                    }
                    ObjectShape::Rect
                };
                objects.push(MapObject {
                    id: o.id,
                    name: o.name.clone(),
                    class: o.class.clone(),
                    position: [o.x, o.y],
                    size: [o.width, o.height],
                    rotation: o.rotation,
                    shape,
                    properties: json_properties(&o.properties, &what, warnings),
                });
            }
            out.push(MapLayer::Objects(ObjectLayer {
                name,
                visible,
                objects,
                properties,
            }));
        }
        "group" => {
            let ctx = GroupCtx {
                prefix: name,
                offset,
                opacity,
                visible,
            };
            for child in &layer.layers {
                json_layer(child, &ctx, cells, shown, out, warnings)?;
            }
        }
        other => warnings.push(format!("{other} layer `{name}` skipped")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_shape_check_counts_attributes_outside_quotes() {
        assert!(check_xml_shape(br#"<map a="1" b='x=y' c="=="><layer d="2"/></map>"#).is_ok());
        let many: String = (0..300).map(|i| format!(" a{i}=\"v\"")).collect();
        let hostile = format!("<map{many}/>");
        assert!(check_xml_shape(hostile.as_bytes()).is_err());
        // Text content with '=' outside tags does not count.
        let text = format!("<data>{}</data>", "=".repeat(1000));
        assert!(check_xml_shape(text.as_bytes()).is_ok());
    }

    /// A one-cell map whose only tileset names `image`, cut as `columns` by `tile_count`.
    fn tileset_map(image: &str, columns: u32, tile_count: u32) -> TileMapAsset {
        use regia_2d::tilemap::{MapLayer, Orientation, TileLayer, TilesetDef};
        TileMapAsset {
            version: 1,
            orientation: Orientation::Orthogonal,
            width: 1,
            height: 1,
            tile_width: 8,
            tile_height: 8,
            background: None,
            tilesets: vec![TilesetDef {
                name: "t".into(),
                first_gid: 1,
                image: image.into(),
                image_width: 8,
                image_height: 8,
                tile_width: 8,
                tile_height: 8,
                columns,
                tile_count,
                spacing: 0,
                margin: 0,
                tiles: Vec::new(),
            }],
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "l".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0],
                properties: BTreeMap::new(),
            })],
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn a_tileset_image_outside_the_project_is_refused_before_it_is_read() {
        let top = regia_test_utils::unique_temp_dir("tiled_image_bound");
        let maps = top.join("project/assets/maps");
        assert!(std::fs::create_dir_all(&maps).is_ok());
        let manifest = top.join("project").join(regia_core::brand::MANIFEST_FILE);
        assert!(std::fs::write(manifest, "").is_ok());
        let png = top.join("secret.png");
        let pixels = image::RgbaImage::new(8, 8);
        assert!(pixels.save(&png).is_ok());
        assert!(pixels.save(top.join("project/assets/maps/own.png")).is_ok());
        for image in [
            "/etc/passwd",
            "../../../secret.png",
            r"..\..\..\secret.png",
            "C:/x.png",
        ] {
            let err = cook_tilemap(tileset_map(image, 1, 1), &maps).expect_err(image);
            assert!(matches!(err, TiledImportError::Image { .. }), "{image}: {err}");
        }
        assert!(cook_tilemap(tileset_map("own.png", 1, 1), &maps).is_ok());
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn a_tile_grid_past_the_image_is_a_cook_error() {
        let top = regia_test_utils::unique_temp_dir("tiled_image_grid");
        assert!(std::fs::create_dir_all(&top).is_ok());
        assert!(image::RgbaImage::new(8, 8).save(top.join("set.png")).is_ok());
        assert!(cook_tilemap(tileset_map("set.png", 1, 1), &top).is_ok());
        // Two columns in an image one tile wide, and a second row that is not there.
        assert!(cook_tilemap(tileset_map("set.png", 2, 2), &top).is_err());
        assert!(cook_tilemap(tileset_map("set.png", 1, 2), &top).is_err());
        let _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn hostile_tmx_is_rejected_before_parsing() {
        let dir = std::env::temp_dir().join(format!("regia-tiled-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(std::fs::create_dir_all(&dir).is_ok());
        let many: String = (0..1000).map(|i| format!(" a{i}=\"v\"")).collect();
        let path = dir.join("hostile.tmx");
        assert!(std::fs::write(&path, format!("<map{many}/>")).is_ok());
        let err = import_tiled(&path).map(|_| ()).expect_err("rejected");
        assert!(err.to_string().contains("attributes"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// --- Tiled 1.0 XML `<terraintypes>` -------------------------------------------------------------

/// The old terrain data of one XML tileset: the `<terraintypes>` names and each tile's
/// `terrain="topleft,topright,bottomleft,bottomright"` corner list (`-1` = none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LegacyTerrains {
    /// Terrain names, in index order.
    pub names: Vec<String>,
    /// `(local tile id, corner terrain indices)`.
    pub tiles: Vec<(u32, [i64; 4])>,
}

/// One `<tileset>` element of an XML file, in document order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct XmlTileset {
    /// The `source` of an external tileset reference (`.tsx` path), if it is one.
    pub source: Option<String>,
    /// Old terrains, when the tileset has `<terraintypes>`.
    pub terrains: Option<LegacyTerrains>,
}

/// Most tags the legacy-terrain scanner reads (a hostile file must not blow up).
const MAX_SCAN_TAGS: usize = 4_000_000;
/// Most terrains and terrain tiles read per tileset.
const MAX_LEGACY_ITEMS: usize = 65_536;

fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Splits the inside of a start tag into its name and attributes.
fn parse_tag(inner: &str) -> (&str, Vec<(&str, String)>) {
    let inner = inner.trim().trim_end_matches('/').trim_end();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let (name, mut rest) = inner.split_at(name_end);
    let mut attrs = Vec::new();
    loop {
        rest = rest.trim_start();
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let body = &after[1..];
        let Some(end) = body.find(quote) else { break };
        attrs.push((key, unescape_xml(&body[..end])));
        rest = &body[end + 1..];
    }
    (name, attrs)
}

/// Reads the `<tileset>` elements of a `.tmx` or `.tsx` text for Tiled 1.0 terrains
/// (`<terraintypes><terrain name tile/></terraintypes>` plus `<tile id terrain="a,b,c,d"/>`).
/// This is a tag scanner, not an XML parser: the `tiled` crate (which does the real parsing) drops
/// these elements, so only the few tags and attributes the old terrains use are read.
pub(crate) fn scan_xml_tilesets(text: &str) -> Vec<XmlTileset> {
    let mut out: Vec<XmlTileset> = Vec::new();
    let mut open = false;
    let mut in_types = false;
    let mut rest = text;
    let mut tags = 0usize;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        if let Some(after) = rest.strip_prefix("!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let inner = &rest[..end];
        rest = &rest[end + 1..];
        tags += 1;
        if tags > MAX_SCAN_TAGS {
            break;
        }
        if inner.starts_with('?') || inner.starts_with('!') {
            continue;
        }
        if let Some(closing) = inner.strip_prefix('/') {
            match closing.trim() {
                "tileset" => (open, in_types) = (false, false),
                "terraintypes" => in_types = false,
                _ => {}
            }
            continue;
        }
        let self_closed = inner.trim_end().ends_with('/');
        let (name, attrs) = parse_tag(inner);
        let attr = |key: &str| attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str());
        match name {
            "tileset" => {
                out.push(XmlTileset {
                    source: attr("source").map(str::to_string),
                    terrains: None,
                });
                open = !self_closed;
            }
            "terraintypes" if open => {
                if let Some(set) = out.last_mut() {
                    set.terrains.get_or_insert_with(LegacyTerrains::default);
                }
                in_types = !self_closed;
            }
            "terrain" if open && in_types => {
                if let Some(t) = out
                    .last_mut()
                    .and_then(|s| s.terrains.as_mut())
                    .filter(|t| t.names.len() < MAX_LEGACY_ITEMS)
                {
                    t.names.push(attr("name").unwrap_or_default().to_string());
                }
            }
            "tile" if open && !in_types => {
                let (Some(id), Some(corners)) = (attr("id"), attr("terrain")) else {
                    continue;
                };
                let Ok(id) = id.trim().parse::<u32>() else {
                    continue;
                };
                let parts: Vec<&str> = corners.split(',').collect();
                if parts.len() != 4 {
                    continue;
                }
                let mut ids = [-1i64; 4];
                for (slot, part) in ids.iter_mut().zip(&parts) {
                    *slot = part.trim().parse::<i64>().unwrap_or(-1);
                }
                if let Some(t) = out.last_mut() {
                    let t = t.terrains.get_or_insert_with(LegacyTerrains::default);
                    if t.tiles.len() < MAX_LEGACY_ITEMS {
                        t.tiles.push((id, ids));
                    }
                }
            }
            _ => {}
        }
    }
    out
}
