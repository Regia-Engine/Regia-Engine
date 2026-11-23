// SPDX-License-Identifier: MPL-2.0
//! Terrain cook (`regia cook`): the tiled path for `*.terrain.layers.ron` sources.
//!
//! A `terrain.layers.ron` source (the frozen v1 layer format, `docs/formats/terrain.md` § 1) is
//! cooked as one *terrain map*: its heightfield, macro maps, splat masks and per-layer textures are
//! each cooked through the tiled virtual-texture path ([`crate::terrain_data`]) at deterministic
//! asset ids, and a [`TerrainIndex`] (`regia_terrain::stream`) is emitted so the runtime
//! streaming cache knows which slots to register and how to address every tile.
//!
//! The per-slot discovery follows the cook convention documented in
//! `docs/formats/terrain.md` § 9 (the lead's decision, matching DO27's real exports). Missing
//! optional slots are warnings, never errors; layer ORM/height missing their texture cook to a
//! neutral default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, META_FORMAT_VERSION, ManifestEntry, cooked_file_name,
    encode_cooked_file,
};
use regia_terrain::layers::TerrainLayers;
use regia_terrain::source::{CountMapping, ImportLimits, RasterDescriptor, TerrainSource as CountSource};
use regia_terrain::stream::{
    TERRAIN_INDEX_FORMAT_VERSION, TERRAIN_INDEX_TYPE_PATH, TerrainHeightfieldIndex, TerrainIndex,
    TerrainSlotIndex, TileFormat as StreamTileFormat, terrain_index_asset_id,
};
use thiserror::Error;
use tracing::{info, warn};

use crate::cook::CookError;
use crate::db::AssetDatabase;
use crate::terrain_data::{
    DEFAULT_TILE_SIZE, TERRAIN_DATA_IMPORTER_VERSION, TILED_HEIGHTFIELD_TYPE_PATH, TileFormat,
    cook_tiled_heightfield, cook_tiled_texture, tail_asset_id, tail_asset_path, tile_asset_id,
    tile_asset_path, write_tiled_heightfield_cooked, write_tiled_texture_cooked,
};
use crate::texture_compress::TextureCookQuality;
use crate::{import_png_height, import_png_rgba8};

#[path = "terrain_cook_tiles.rs"]
mod tiles;
pub use tiles::{
    SourceTileCookReport, SourceTileProgress, TILE_EVIDENCE_DIR, TILE_SET_FILE, cook_tile_set_heightfield,
    read_tile_set,
};

/// The importer name stamped into the asset database for a terrain source.
pub const TERRAIN_IMPORTER: &str = "terrain";

/// The macro slots and their source paths relative to the layer file's directory, in cook order.
///
/// `macro_albedo` lives at the layer file's directory; the rest live in its `macro/` sub-directory
/// (the lead's convention, matching DO27's real exports).
const MACRO_SLOTS: &[(&str, &str, TileFormat)] = &[
    ("macro_albedo", "macro_albedo.png", TileFormat::Bc7Srgb),
    ("macro_normal", "macro/normal.png", TileFormat::Bc5),
    ("macro_ao", "macro/ao.png", TileFormat::Bc4),
    ("macro_pack0", "macro/pack0.png", TileFormat::Bc7Linear),
    ("macro_pack1", "macro/pack1.png", TileFormat::Bc7Linear),
];

/// The per-layer texture kinds and their file names inside `detail/<name>/`, in cook order.
const LAYER_KINDS: &[(&str, &str, TileFormat)] = &[
    ("albedo", "albedo.png", TileFormat::Bc7Srgb),
    ("normal", "normal.png", TileFormat::Bc5),
    ("orm", "orm.png", TileFormat::Bc7Linear),
    ("height", "height.png", TileFormat::Bc4),
];

/// Errors the terrain cooker produces (folded into [`CookError::Terrain`]).
#[derive(Debug, Error)]
pub enum TerrainCookError {
    /// The `.terrain.layers.ron` source could not be parsed.
    #[error("terrain layers error: {0}")]
    Layers(String),

    /// A source image could not be imported.
    #[error("image import error: {0}")]
    Import(#[from] crate::ImportError),

    /// The tiled cook failed.
    #[error("tiled cook error: {0}")]
    Tiled(#[from] crate::terrain_data::TerrainDataError),

    /// The sibling `heightfield.json` world-extents file could not be parsed.
    #[error("heightfield.json error: {0}")]
    Extents(String),

    /// The `heightfield.json` count mapping is invalid or is not the heightfield's grid (ADR-8471).
    #[error("heightfield.json count mapping: {0}")]
    Mapping(String),

    /// Serialising the terrain index failed.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// Building the terrain-stats report failed.
    #[error("terrain stats error: {0}")]
    Stats(String),

    /// A `heightfield.tiles.json` source tile set was refused or cancelled (ADR-8595). The
    /// message starts with its stable code (`terrain_tiles_seam:`, `height_source_cancelled:`, ...).
    #[error("source tiles: {0}")]
    SourceTiles(String),

    /// IO while cooking.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// One terrain source (a `*.terrain.layers.ron` file) discovered in `assets/`.
#[derive(Clone, Debug)]
pub struct TerrainSource {
    /// The layer file path.
    pub source: PathBuf,
}

/// The result of cooking one terrain source.
#[derive(Clone, Debug)]
pub struct TerrainCookOutcome {
    /// The terrain-index asset id (the primary cooked asset).
    pub primary_id: AssetId,
    /// Reflect path of the primary type.
    pub type_path: String,
    /// BLAKE3 of the layer file bytes.
    pub source_hash: [u8; 32],
    /// BLAKE3 over the discovered inputs (paths + contents).
    pub settings_hash: [u8; 32],
    /// Importer name.
    pub importer: String,
    /// Packed importer version.
    pub importer_version: u32,
    /// Cache key for incremental skip.
    pub cache_key: [u8; 32],
    /// Path to the index cooked file relative to the cache dir.
    pub cooked_rel: String,
    /// True when the cook was a cache hit.
    pub skipped: bool,
    /// True when the layer file resolved to no heightfield or layer texture and was not cooked
    /// (no terrain index emitted).
    pub empty: bool,
    /// Manifest entries for the index, every tile/tail, and the heightfield.
    pub sub_manifest: Vec<ManifestEntry>,
    /// Warnings for missing optional slots.
    pub warnings: Vec<String>,
    /// Counters of a heightfield cooked from source tiles (`heightfield.tiles.json`, ADR-8595);
    /// `None` for a `heightfield.png` terrain or a skipped cook.
    pub source_tiles: Option<SourceTileCookReport>,
}

/// Discover every terrain layer file among the discovered sources.
pub fn discover_terrains(sources: &[PathBuf]) -> Vec<TerrainSource> {
    let mut out: Vec<TerrainSource> = sources
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| crate::cook::name_ends_with(n, "terrain.layers.ron"))
        })
        .map(|p| TerrainSource { source: p.clone() })
        .collect();
    out.sort_by(|a, b| a.source.cmp(&b.source));
    out
}

/// The candidate input files a terrain layer file *may* consume (heightfield, macro, per-layer
/// textures). Used to exclude those files from the generic cook. Layer names come from the file.
pub fn terrain_input_files(layer_file: &Path) -> Result<BTreeSet<PathBuf>, TerrainCookError> {
    let dir = layer_file.parent().unwrap_or_else(|| Path::new("."));
    let mut out = BTreeSet::new();

    // Heightfield: both candidate locations are consumed by whichever exists.
    out.insert(dir.join("heightfield.png"));
    // World extents sit beside the heightfield (either location) — consumed by the cooker, not
    // cooked generically.
    out.insert(dir.join("heightfield.json"));
    // An engine raster descriptor beside the heightfield (ADR-8542); absent = hashes as before.
    out.insert(dir.join(RASTER_DESCRIPTOR_FILE));
    // A source tile set (ADR-8595) and the tile files it names.
    let tile_set = dir.join(TILE_SET_FILE);
    if tile_set.is_file() {
        out.extend(tiles::tile_set_files(&tile_set));
    }
    out.insert(tile_set);
    if let Some(parent) = dir.parent() {
        out.insert(parent.join("visual").join("heightfield.png"));
        out.insert(parent.join("visual").join("heightfield.json"));
        out.insert(parent.join("visual").join(RASTER_DESCRIPTOR_FILE));
    }
    // Macro slots.
    for &(_, rel, _) in MACRO_SLOTS {
        out.insert(dir.join(rel));
    }
    // Layer slots (names from the file).
    let names = layer_names(layer_file)?;
    for name in &names {
        for &(_, file, _) in LAYER_KINDS {
            out.insert(dir.join("detail").join(name).join(file));
        }
    }
    Ok(out)
}

/// Domain tag of [`terrain_inputs_hash`] (version 2: portable tags and canonical extents, ADR-2440).
pub const TERRAIN_INPUTS_HASH_DOMAIN: &[u8] = b"regia-terrain-inputs-v2\0";

/// The portable tag of an input file: its path relative to the layer file's folder, with `/`
/// separators whatever the host writes (`\` on Windows), `.` segments dropped, and `../` for a
/// file in the parent folder (`../visual/heightfield.png`). Both paths are taken as text, so the
/// tag of a Windows path computed on Linux equals the tag of the Unix path.
#[must_use]
pub fn terrain_input_tag(layer_dir: &str, path: &str) -> String {
    fn segments(text: &str) -> Vec<&str> {
        text.split(['/', '\\'])
            .filter(|s| !s.is_empty() && *s != ".")
            .collect()
    }
    let base = segments(layer_dir);
    let file = segments(path);
    if file.starts_with(&base) {
        return file[base.len()..].join("/");
    }
    if let Some((_, parent)) = base.split_last()
        && file.starts_with(parent)
    {
        return format!("../{}", file[parent.len()..].join("/"));
    }
    file.last().copied().unwrap_or_default().to_owned()
}

/// The bytes a `heightfield.json` contributes to the hash: the parsed extents with an explicit
/// layout, so `1024`, `1024.0` and `1.024e3` hash alike: `has_size u8 | size_m f32 LE |
/// has_origin u8 | origin [f32 LE; 3]` (absent fields are 0 and zeros). A schema-2 count mapping
/// (ADR-8471) appends `b'M'` and its resolved [`CountMapping::canonical_bytes`] (datum, scale,
/// cell, registration and grid size), so changing any of them changes the hash; a file without
/// `schema` hashes exactly as before. A file that does not parse, or whose mapping is invalid,
/// contributes `0xFF` and its raw bytes (the cook refuses it anyway).
#[must_use]
pub fn canonical_extents_bytes(json: &[u8]) -> Vec<u8> {
    let raw = || {
        let mut out = vec![0xFF];
        out.extend_from_slice(json);
        out
    };
    let Ok(extents) = serde_json::from_slice::<HeightfieldExtentsJson>(json) else {
        return raw();
    };
    let Ok(mapping) = CountMapping::from_heightfield_json(json) else {
        return raw();
    };
    let mut out = Vec::with_capacity(18 + 1 + regia_terrain::source::COUNT_MAPPING_CANONICAL_LEN);
    out.push(u8::from(extents.size_m.is_some()));
    out.extend_from_slice(&extents.size_m.unwrap_or(0.0).to_bits().to_le_bytes());
    out.push(u8::from(extents.origin.is_some()));
    for v in extents.origin.unwrap_or([0.0; 3]) {
        out.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    if let Some(mapping) = mapping {
        out.push(b'M');
        out.extend_from_slice(&mapping.canonical_bytes());
    }
    out
}

/// Feeds one framed input: `tag length u64 LE | tag | content length u64 LE | content`. Explicit
/// little-endian lengths: the same bytes on every host.
fn frame_input(hasher: &mut blake3::Hasher, tag: &str, content: &[u8]) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update(&(content.len() as u64).to_le_bytes());
    hasher.update(content);
}

/// The platform-neutral BLAKE3 of `(tag, content)` inputs (ADR-2440): sorted by tag, each framed
/// by `frame_input`'s layout after [`TERRAIN_INPUTS_HASH_DOMAIN`]; a `heightfield.json` counts
/// by its [`canonical_extents_bytes`]. No host path, path separator, float spelling or byte order
/// reaches the hash, so an Android and a desktop cook of the same content agree.
#[must_use]
pub fn terrain_inputs_hash(inputs: &[(String, Vec<u8>)]) -> [u8; 32] {
    let mut sorted: Vec<&(String, Vec<u8>)> = inputs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    hasher.update(TERRAIN_INPUTS_HASH_DOMAIN);
    for (tag, content) in sorted {
        if tag.ends_with("heightfield.json") {
            frame_input(&mut hasher, tag, &canonical_extents_bytes(content));
        } else {
            frame_input(&mut hasher, tag, content);
        }
    }
    *hasher.finalize().as_bytes()
}

/// Reads the existing files of `paths` as `(portable tag, bytes)` inputs.
fn read_inputs(
    layer_file: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<(String, Vec<u8>)>, TerrainCookError> {
    let dir = layer_file.parent().unwrap_or_else(|| Path::new("."));
    let dir_text = dir.to_string_lossy();
    // Source tile files (ADR-8595) count by their streamed BLAKE3, never read whole: a tile set's
    // total bytes can be far larger than one tile. Terrains without a tile set hash as before.
    let tile_files: BTreeSet<PathBuf> = tiles::find_tile_set(dir)
        .map(|p| tiles::tile_set_files(&p).into_iter().collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for path in paths {
        if path.is_file() {
            let tag = terrain_input_tag(&dir_text, &path.to_string_lossy());
            if tile_files.contains(&path) {
                let mut content = b"regia-source-tile-blake3\0".to_vec();
                content.extend_from_slice(&tiles::file_blake3(&path)?);
                out.push((tag, content));
            } else {
                out.push((tag, std::fs::read(&path)?));
            }
        }
    }
    Ok(out)
}

/// The platform-neutral hash of a terrain's discovered input files (their portable tags and
/// contents, [`terrain_inputs_hash`]) — the `settings_hash` recorded in the asset database for a
/// terrain layer source. A change to any input (the heightfield, its `heightfield.json` world
/// extents, a macro map, or a per-layer texture) changes this hash, so `regia_cli`'s pak-freshness
/// check can re-derive it and compare against the database without reading the tiled output.
/// Exported for that check; do not fork it.
pub fn terrain_settings_hash(layer_file: &Path) -> Result<[u8; 32], TerrainCookError> {
    let inputs = read_inputs(layer_file, terrain_input_files(layer_file)?)?;
    Ok(terrain_inputs_hash(&inputs))
}

/// The platform-neutral hash of the ground a server simulates (ADR-2440): the heightfield
/// `find_heightfield` actually reads and the `heightfield.json` beside that file
/// (`docs/formats/terrain.md` § 9), not the presentation textures and not the other candidate
/// location. An unused sibling does not change this hash. The net content manifest uses it, so an
/// Android client and a desktop server of the same content agree (`content_mismatch` otherwise).
pub fn terrain_ground_hash(layer_file: &Path) -> Result<[u8; 32], TerrainCookError> {
    let inputs = read_inputs(layer_file, terrain_ground_hashed_files(layer_file))?;
    Ok(terrain_inputs_hash(&inputs))
}

/// The heightfield the cook reads and the extents file beside it. A missing file is omitted by
/// [`read_inputs`]. No heightfield means nothing: a sibling json the cook does not open is not ground.
fn terrain_ground_hashed_files(layer_file: &Path) -> Vec<PathBuf> {
    let dir = layer_file.parent().unwrap_or_else(|| Path::new("."));
    let Some(png) = find_heightfield(dir) else {
        // A source tile set (ADR-8595): its descriptor and every tile file are the ground.
        return match tiles::find_tile_set(dir) {
            Some(tile_set) => {
                let mut files = tiles::tile_set_files(&tile_set);
                files.push(tile_set);
                files
            }
            None => Vec::new(),
        };
    };
    let json = png.with_file_name("heightfield.json");
    let raster = png.with_file_name(RASTER_DESCRIPTOR_FILE);
    vec![png, json, raster]
}

/// True when a `*.terrain.layers.ron` source resolves to a cookable terrain (a heightfield or at
/// least one layer texture), matching the emptiness test in [`cook_terrain`]. An empty terrain is
/// left uncooked with no database row; the pak-freshness check uses this to skip it instead of
/// recooking forever. Exported for that check; do not fork it.
pub fn terrain_is_cookable(layer_file: &Path) -> Result<bool, TerrainCookError> {
    let dir = layer_file.parent().unwrap_or_else(|| Path::new("."));
    let bytes = std::fs::read(layer_file)?;
    let text = std::str::from_utf8(&bytes).map_err(|e| TerrainCookError::Layers(e.to_string()))?;
    let layers = TerrainLayers::from_ron(text).map_err(|e| TerrainCookError::Layers(e.to_string()))?;
    let heightfield = find_heightfield(dir).or_else(|| tiles::find_tile_set(dir));
    heightfield_or_layer_texture(heightfield.as_deref(), dir, &layers)
}

/// Reads the layer names (bottom-first) from a `*.terrain.layers.ron` file.
fn layer_names(layer_file: &Path) -> Result<Vec<String>, TerrainCookError> {
    let bytes = std::fs::read(layer_file)?;
    let text = std::str::from_utf8(&bytes).map_err(|e| TerrainCookError::Layers(e.to_string()))?;
    let layers = TerrainLayers::from_ron(text).map_err(|e| TerrainCookError::Layers(e.to_string()))?;
    layers
        .layers
        .iter()
        .map(|l| checked_layer_name(&l.name).map(str::to_owned))
        .collect()
}

/// A layer name is a folder under `detail/`, read straight from `.terrain.layers.ron`: one segment,
/// never `..` or a path that would pull another folder's files into the terrain.
fn checked_layer_name(name: &str) -> Result<&str, TerrainCookError> {
    if name.contains(['/', '\\']) || regia_core::project_path::check_relative(name).is_err() {
        return Err(TerrainCookError::Layers(format!(
            "layer name {name:?} must be one folder name"
        )));
    }
    Ok(name)
}

/// The sibling `heightfield.json` world extents, when present (`size_m`, `origin`; unknown fields
/// ignored). Documented in `docs/formats/terrain.md` § 9.
#[derive(serde::Deserialize)]
struct HeightfieldExtentsJson {
    #[serde(default)]
    size_m: Option<f32>,
    #[serde(default)]
    origin: Option<[f32; 3]>,
}

/// What a `heightfield.json` gives the cook: legacy extents, or a schema-2 count mapping, and the
/// raster descriptor beside it (ADR-8542).
type HeightfieldJsonParts = (
    Option<f32>,
    Option<[f32; 3]>,
    Option<CountMapping>,
    Option<RasterDescriptor>,
);

/// The engine raster descriptor the cook reads beside `heightfield.png` (ADR-8542,
/// `docs/formats/terrain.md` § 14).
pub const RASTER_DESCRIPTOR_FILE: &str = "heightfield.raster.json";

/// Largest `heightfield.json` the cook reads, bytes (it holds a handful of numbers).
const MAX_HEIGHTFIELD_JSON_BYTES: u64 = 1 << 20;

/// Reads the world extents, or the schema-2 count mapping (ADR-8471), from the `heightfield.json`
/// beside the matched heightfield (`<dir>/heightfield.json` for a local heightfield,
/// `<dir>/../visual/heightfield.json` for the visual sibling). Absent heightfield/json → defaults
/// (`None`). Documented in `docs/formats/terrain.md` § 9.
fn read_extents(heightfield_path: Option<&Path>) -> Result<HeightfieldJsonParts, TerrainCookError> {
    let Some(hf) = heightfield_path else {
        return Ok((None, None, None, None));
    };
    let dir = hf.parent().unwrap_or_else(|| Path::new("."));
    let raster = read_raster_descriptor(&dir.join(RASTER_DESCRIPTOR_FILE))?;
    let json_path = dir.join("heightfield.json");
    if !json_path.is_file() {
        if raster.is_some() {
            return Err(TerrainCookError::Mapping(format!(
                "{RASTER_DESCRIPTOR_FILE} needs a schema-2 heightfield.json beside it for the datum and scale"
            )));
        }
        return Ok((None, None, None, None));
    }
    if std::fs::metadata(&json_path)?.len() > MAX_HEIGHTFIELD_JSON_BYTES {
        return Err(TerrainCookError::Extents(format!(
            "{} is over {MAX_HEIGHTFIELD_JSON_BYTES} bytes",
            json_path.display()
        )));
    }
    let bytes = std::fs::read(&json_path)?;
    let extents: HeightfieldExtentsJson =
        serde_json::from_slice(&bytes).map_err(|e| TerrainCookError::Extents(e.to_string()))?;
    let mapping = CountMapping::from_heightfield_json(&bytes).map_err(|e| TerrainCookError::Mapping(e.0))?;
    if raster.is_some() && mapping.is_none() {
        return Err(TerrainCookError::Mapping(format!(
            "{RASTER_DESCRIPTOR_FILE} needs a schema-2 heightfield.json beside it for the datum and scale"
        )));
    }
    Ok((extents.size_m, extents.origin, mapping, raster))
}

/// Reads the optional raster descriptor (at most [`MAX_HEIGHTFIELD_JSON_BYTES`]).
fn read_raster_descriptor(path: &Path) -> Result<Option<RasterDescriptor>, TerrainCookError> {
    if !path.is_file() {
        return Ok(None);
    }
    if std::fs::metadata(path)?.len() > MAX_HEIGHTFIELD_JSON_BYTES {
        return Err(TerrainCookError::Mapping(format!(
            "{} is over {MAX_HEIGHTFIELD_JSON_BYTES} bytes",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)?;
    RasterDescriptor::from_json(&bytes, ImportLimits::default())
        .map(Some)
        .map_err(|e| TerrainCookError::Mapping(format!("{RASTER_DESCRIPTOR_FILE}: {}", e.0)))
}

/// Applies a raster descriptor to the decoded heightfield (ADR-8542): the counts are permuted
/// exactly into canonical south/west order and the resolved mapping replaces `mapping`. What the
/// terrain index (v2) cannot hold exactly is refused: unequal X/Z spacing
/// (`terrain_mapping_unrepresentable`) and samples without data (`terrain_raster_missing_data`:
/// the cook has no hole policy, nothing is filled).
fn apply_raster(
    hf: &mut regia_asset::Heightfield,
    raster: &RasterDescriptor,
    mapping: &CountMapping,
) -> Result<CountMapping, TerrainCookError> {
    let refuse = |e: regia_terrain::source::SourceError| {
        TerrainCookError::Mapping(format!("{RASTER_DESCRIPTOR_FILE}: {}", e.0))
    };
    if [hf.width, hf.height] != [mapping.cols, mapping.rows] {
        return Err(TerrainCookError::Mapping(format!(
            "the mapping is {}x{} but the heightfield is {}x{}",
            mapping.cols, mapping.rows, hf.width, hf.height
        )));
    }
    let source = CountSource::from_raster(
        std::mem::take(&mut hf.samples),
        [hf.width, hf.height],
        mapping.meters_per_count,
        mapping.zero_counts,
        raster,
        ImportLimits::default(),
    )
    .map_err(refuse)?;
    let resolved = source.count_mapping().map_err(refuse)?;
    hf.samples = source.counts().to_vec();
    Ok(resolved)
}

/// Maps the pipeline's block format to the runtime stream's.
fn stream_format(format: TileFormat) -> StreamTileFormat {
    match format {
        TileFormat::Bc7Srgb => StreamTileFormat::Bc7Srgb,
        TileFormat::Bc5 => StreamTileFormat::Bc5,
        TileFormat::Bc4 => StreamTileFormat::Bc4,
        TileFormat::Bc7Linear => StreamTileFormat::Bc7Linear,
    }
}

/// The stable slot name of a layer texture: `layer{index}_{kind}`.
fn layer_slot_name(index: usize, kind: &str) -> String {
    format!("layer{index}_{kind}")
}

/// Cooks one terrain source through the tiled path: BCn tiles on desktop, ASTC 4×4 tiles on
/// `android` (ADR-5294, which supersedes the heightfield-only Android index of ADR-4914).
///
/// # Errors
///
/// Returns [`CookError`] when the layer file, a required input, or the tiled cook itself fails.
/// Missing optional slots produce warnings on the returned outcome, never an error.
pub fn cook_terrain(
    source: &Path,
    rel_path: &str,
    cache_dir: &Path,
    platform: &str,
    db: &AssetDatabase,
    incremental: bool,
) -> Result<TerrainCookOutcome, CookError> {
    // Without a project root the cache's ledger, or failing that the database rows, decide the
    // incremental skip (`cook_project` passes its own ledger, ADR-875).
    let ledger = crate::cook_ledger::CookLedger::open(cache_dir, None, db);
    cook_terrain_with_ledger(source, rel_path, cache_dir, platform, &ledger, incremental)
}

/// [`cook_terrain`] with the platform's cook key ledger deciding the incremental skip.
pub(crate) fn cook_terrain_with_ledger(
    source: &Path,
    rel_path: &str,
    cache_dir: &Path,
    platform: &str,
    ledger: &crate::cook_ledger::CookLedger,
    incremental: bool,
) -> Result<TerrainCookOutcome, CookError> {
    // Stable map id from the `.meta` sidecar.
    let meta = AssetDatabase::load_or_create_meta(source, TERRAIN_IMPORTER).map_err(CookError::Db)?;
    if meta.format_version != META_FORMAT_VERSION {
        return Err(CookError::Meta(regia_asset::MetaError::UnsupportedVersion {
            found: meta.format_version,
            supported: META_FORMAT_VERSION,
        }));
    }
    let map_id = meta.id;

    let layer_bytes = std::fs::read(source)?;
    let layer_text = std::str::from_utf8(&layer_bytes)
        .map_err(|e| CookError::Terrain(TerrainCookError::Layers(e.to_string())))?;
    let layers = TerrainLayers::from_ron(layer_text)
        .map_err(|e| CookError::Terrain(TerrainCookError::Layers(e.to_string())))?;

    let dir = source.parent().unwrap_or_else(|| Path::new("."));

    let source_hash = *blake3::hash(&layer_bytes).as_bytes();

    // A layer file with no resolvable heightfield or layer texture is not a cookable terrain:
    // leave it uncooked (no terrain index) rather than emitting an empty map.
    let heightfield_path = find_heightfield(dir);
    let tile_set_path = tiles::find_tile_set(dir);
    if !heightfield_or_layer_texture(
        heightfield_path.as_deref().or(tile_set_path.as_deref()),
        dir,
        &layers,
    )
    .map_err(CookError::Terrain)?
    {
        return Ok(empty_terrain_outcome(map_id, source_hash));
    }

    // Discovery pass: hash every input (paths + contents) for the settings hash, so a change to any
    // input invalidates the cache. The world-extents `heightfield.json` (beside either heightfield
    // location) is one of those inputs and is hashed here.
    let settings_hash = terrain_settings_hash(source).map_err(CookError::Terrain)?;

    let importer_version = TERRAIN_DATA_IMPORTER_VERSION;
    let mut key_hasher = blake3::Hasher::new();
    key_hasher.update(&source_hash);
    key_hasher.update(&settings_hash);
    key_hasher.update(TERRAIN_IMPORTER.as_bytes());
    key_hasher.update(&importer_version.to_le_bytes());
    if platform == "android" {
        // Desktop keys are the bytes above, so an existing desktop cache entry still matches.
        // Android tiles are ASTC 4×4 and must not skip from a BCn cache key.
        key_hasher.update(b"\0astc-4x4");
    }
    let cache_key = *key_hasher.finalize().as_bytes();

    let index_id = terrain_index_asset_id(map_id);
    let cooked_rel = cooked_file_name(index_id);

    // Incremental skip: unchanged inputs and a present index `.fckd` mean the tiles are all there.
    if incremental && ledger.is_current(rel_path, &cache_key) && cache_dir.join(&cooked_rel).is_file() {
        return Ok(TerrainCookOutcome {
            primary_id: index_id,
            type_path: TERRAIN_INDEX_TYPE_PATH.to_string(),
            source_hash,
            settings_hash,
            importer: TERRAIN_IMPORTER.to_string(),
            importer_version,
            cache_key,
            cooked_rel,
            skipped: true,
            empty: false,
            sub_manifest: Vec::new(),
            warnings: Vec::new(),
            source_tiles: None,
        });
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut sub_manifest: Vec<ManifestEntry> = Vec::new();
    let mut slots: Vec<TerrainSlotIndex> = Vec::new();

    // Heightfield from source tiles (ADR-8595): the descriptor carries the mapping; the tiles
    // stream into the tiled cook. It replaces `heightfield.png` and its `heightfield.json`.
    let mut source_tiles = None;
    let (world_size_m, world_origin, heightfield, source_mapping) = if let Some(tile_set) = &tile_set_path {
        // A raster descriptor beside the tile set would be a second description of the same
        // heightfield (its row order and spacing are never applied to tiles), so it is refused
        // like `heightfield.json` (ADR-8639).
        if heightfield_path.is_some()
            || dir.join("heightfield.json").is_file()
            || dir.join(RASTER_DESCRIPTOR_FILE).is_file()
        {
            return Err(CookError::Terrain(TerrainCookError::SourceTiles(format!(
                "terrain_tiles_ambiguous: {TILE_SET_FILE} carries the heightfield and its mapping; remove heightfield.png / heightfield.json / {RASTER_DESCRIPTOR_FILE} beside it"
            ))));
        }
        let (index, mapping, report) =
            cook_tile_set_heightfield(map_id, tile_set, cache_dir, DEFAULT_TILE_SIZE, |_| {
                std::ops::ControlFlow::Continue(())
            })
            .map_err(CookError::Terrain)?;
        sub_manifest.push(ManifestEntry {
            path: format!("{rel_path}#terrain/heightfield"),
            id: crate::terrain_data::heightfield_asset_id(map_id),
            type_path: TILED_HEIGHTFIELD_TYPE_PATH.to_string(),
        });
        source_tiles = Some(report);
        (None, None, Some(index), Some(mapping))
    } else {
        // Heightfield. The extents, mapping and raster descriptor are read first: a descriptor
        // reorders the counts before they are tiled (ADR-8542).
        let (world_size_m, world_origin, source_mapping, raster) =
            read_extents(heightfield_path.as_deref()).map_err(CookError::Terrain)?;
        let raster = raster.zip(source_mapping);
        let (heightfield, source_mapping) = match cook_heightfield(
            map_id,
            heightfield_path.as_deref(),
            raster.as_ref().map(|(r, m)| (r, m)),
            cache_dir,
            rel_path,
            &mut sub_manifest,
            &mut warnings,
        )? {
            (heightfield, Some(resolved)) => (heightfield, Some(resolved)),
            (heightfield, None) => (heightfield, source_mapping),
        };
        (world_size_m, world_origin, heightfield, source_mapping)
    };
    if let Some(mapping) = &source_mapping {
        // The tiled counts are the source's verbatim; the mapping must describe exactly that grid.
        let grid = heightfield.as_ref().map(|h| [h.width, h.height]);
        if grid != Some([mapping.cols, mapping.rows]) {
            return Err(CookError::Terrain(TerrainCookError::Mapping(format!(
                "the mapping is {}x{} but the heightfield is {grid:?}",
                mapping.cols, mapping.rows
            ))));
        }
    }

    // Macro slots.
    for &(slot, rel, format) in MACRO_SLOTS {
        let path = dir.join(rel);
        if !path.is_file() {
            warn!(target: "regia_asset_pipeline", slot, path = %path.display(), "terrain macro slot missing");
            warnings.push(format!("missing macro slot `{slot}` ({})", path.display()));
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let img = import_png_rgba8(&bytes).map_err(|e| CookError::Terrain(TerrainCookError::Import(e)))?;
        let slot_index = cook_texture_slot(
            map_id,
            slot,
            img.width,
            img.height,
            &img.rgba8,
            format,
            platform,
            cache_dir,
            rel_path,
            &mut sub_manifest,
        )?;
        slots.push(slot_index);
    }

    // Layer slots (bottom-first, in file order).
    for (i, layer) in layers.layers.iter().enumerate() {
        let name = checked_layer_name(&layer.name).map_err(CookError::Terrain)?;
        let base = dir.join("detail").join(name);
        // Reference size for neutral orm/height defaults: the first present texture of the layer.
        let mut ref_size: Option<(u32, u32)> = None;
        for &(_, file, _) in LAYER_KINDS {
            if let Some((w, h)) = png_size(&base.join(file)) {
                ref_size = Some((w, h));
                break;
            }
        }
        let Some((ref_w, ref_h)) = ref_size else {
            warn!(target: "regia_asset_pipeline", layer = %layer.name, "terrain layer has no textures; skipping");
            warnings.push(format!("layer `{}` has no textures; skipped", layer.name));
            continue;
        };

        for &(kind, file, format) in LAYER_KINDS {
            let slot = layer_slot_name(i, kind);
            let path = base.join(file);
            if path.is_file() {
                let bytes = std::fs::read(&path)?;
                let img =
                    import_png_rgba8(&bytes).map_err(|e| CookError::Terrain(TerrainCookError::Import(e)))?;
                let slot_index = cook_texture_slot(
                    map_id,
                    &slot,
                    img.width,
                    img.height,
                    &img.rgba8,
                    format,
                    platform,
                    cache_dir,
                    rel_path,
                    &mut sub_manifest,
                )?;
                slots.push(slot_index);
            } else if kind == "orm" || kind == "height" {
                // Neutral default (AO 1, roughness 0.8, metal 0; flat height), never an error.
                warn!(target: "regia_asset_pipeline", slot, "terrain layer slot missing; cooking neutral default");
                warnings.push(format!(
                    "layer `{}` missing `{file}`; cooking neutral default",
                    layer.name
                ));
                let neutral = neutral_default_rgba(kind, ref_w, ref_h);
                let slot_index = cook_texture_slot(
                    map_id,
                    &slot,
                    ref_w,
                    ref_h,
                    &neutral,
                    format,
                    platform,
                    cache_dir,
                    rel_path,
                    &mut sub_manifest,
                )?;
                slots.push(slot_index);
            } else {
                warn!(target: "regia_asset_pipeline", slot, "terrain layer slot missing");
                warnings.push(format!("layer `{}` missing `{file}`", layer.name));
            }
        }
    }

    // Terrain index.
    let index = TerrainIndex {
        version: TERRAIN_INDEX_FORMAT_VERSION,
        map_id,
        slots,
        heightfield,
        layers: layers.layers.iter().map(|l| l.name.clone()).collect(),
        world_size_m,
        world_origin,
        source_mapping,
        source_spacing_z_m: None,
        source_valid_mask: None,
        heights: None,
        holes: None,
        material: None,
    };
    let index_payload = index
        .to_postcard()
        .map_err(|e| CookError::Terrain(TerrainCookError::Postcard(e)))?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
        asset_id: index_id,
        importer_version,
        dependencies: Vec::new(),
        payload_len: index_payload.len() as u64,
    };
    let index_file = encode_cooked_file(&header, &index_payload).map_err(CookError::Cooked)?;
    crate::cook_ledger::write_atomic(&cache_dir.join(&cooked_rel), &index_file)?;

    info!(
        target: "regia_asset_pipeline",
        path = %rel_path,
        slots = index.slots.len(),
        bytes = index.total_bytes(),
        "cooked terrain map"
    );

    Ok(TerrainCookOutcome {
        primary_id: index_id,
        type_path: TERRAIN_INDEX_TYPE_PATH.to_string(),
        source_hash,
        settings_hash,
        importer: TERRAIN_IMPORTER.to_string(),
        importer_version,
        cache_key,
        cooked_rel,
        skipped: false,
        empty: false,
        sub_manifest,
        warnings,
        source_tiles,
    })
}

/// The outcome for a layer file that resolves to no heightfield or layer texture (not a cookable
/// terrain): `empty` is set and no terrain index is emitted.
fn empty_terrain_outcome(map_id: AssetId, source_hash: [u8; 32]) -> TerrainCookOutcome {
    let index_id = terrain_index_asset_id(map_id);
    TerrainCookOutcome {
        primary_id: index_id,
        type_path: TERRAIN_INDEX_TYPE_PATH.to_string(),
        source_hash,
        settings_hash: [0u8; 32],
        importer: TERRAIN_IMPORTER.to_string(),
        importer_version: TERRAIN_DATA_IMPORTER_VERSION,
        cache_key: [0u8; 32],
        cooked_rel: cooked_file_name(index_id),
        skipped: true,
        empty: true,
        sub_manifest: Vec::new(),
        warnings: Vec::new(),
        source_tiles: None,
    }
}

/// True when `dir` has a resolvable heightfield or any layer of `layers` has at least one texture —
/// the shared "is this a cookable terrain" test used by both [`cook_terrain`] and
/// [`terrain_is_cookable`]. Every layer name is checked first (as the cook and
/// [`terrain_input_files`] check it), so a name that is not one folder is refused here too instead
/// of being probed outside `detail/` or passing as an empty terrain.
fn heightfield_or_layer_texture(
    heightfield: Option<&Path>,
    dir: &Path,
    layers: &TerrainLayers,
) -> Result<bool, TerrainCookError> {
    for layer in &layers.layers {
        checked_layer_name(&layer.name)?;
    }
    Ok(heightfield.is_some() || terrain_has_layer_texture(dir, layers))
}

/// True when any layer of `layers` has at least one texture in `dir/detail/<name>/`.
fn terrain_has_layer_texture(dir: &Path, layers: &TerrainLayers) -> bool {
    layers.layers.iter().any(|layer| {
        let base = dir.join("detail").join(&layer.name);
        LAYER_KINDS.iter().any(|(_, file, _)| base.join(file).is_file())
    })
}

/// The dimensions of a PNG, or `None` when the file is absent or undecodable.
fn png_size(path: &Path) -> Option<(u32, u32)> {
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    import_png_rgba8(&bytes).ok().map(|img| (img.width, img.height))
}

/// The neutral default RGBA8 for a missing ORM/height slot: ORM = (AO 1, roughness 0.8, metal 0),
/// height = flat 0.
fn neutral_default_rgba(kind: &str, width: u32, height: u32) -> Vec<u8> {
    let px = if kind == "orm" {
        [255u8, 204, 0, 255] // AO 1.0, roughness 0.8, metal 0.0
    } else {
        [0u8, 0, 0, 255] // flat height
    };
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        out.extend_from_slice(&px);
    }
    out
}

/// Cooks a 16-bit heightfield through the tiled path (if present) and appends its manifest entry.
///
/// Returns the heightfield index (or `None` when no heightfield exists).
#[allow(clippy::too_many_arguments)] // a self-contained sub-step of the terrain cook
/// Cooks the heightfield; with a raster descriptor, also returns the resolved mapping
/// ([`apply_raster`]).
fn cook_heightfield(
    map_id: AssetId,
    hf_path: Option<&Path>,
    raster: Option<(&RasterDescriptor, &CountMapping)>,
    cache_dir: &Path,
    rel_path: &str,
    sub_manifest: &mut Vec<ManifestEntry>,
    warnings: &mut Vec<String>,
) -> Result<(Option<TerrainHeightfieldIndex>, Option<CountMapping>), CookError> {
    let Some(path) = hf_path else {
        warn!(target: "regia_asset_pipeline", "terrain heightfield missing");
        warnings.push("missing heightfield (heightfield.png)".to_string());
        return Ok((None, None));
    };
    let bytes = std::fs::read(path)?;
    let mut hf = import_png_height(&bytes).map_err(|e| CookError::Terrain(TerrainCookError::Import(e)))?;
    let resolved = match raster {
        Some((raster, mapping)) => Some(apply_raster(&mut hf, raster, mapping).map_err(CookError::Terrain)?),
        None => None,
    };
    let tiled = cook_tiled_heightfield(&hf, DEFAULT_TILE_SIZE)
        .map_err(|e| CookError::Terrain(TerrainCookError::Tiled(e)))?;
    let tiles_x = hf.width.div_ceil(DEFAULT_TILE_SIZE);
    let tiles_y = hf.height.div_ceil(DEFAULT_TILE_SIZE);
    let (id, bytes) = write_tiled_heightfield_cooked(map_id, &tiled, cache_dir)
        .map_err(|e| CookError::Terrain(TerrainCookError::Tiled(e)))?;
    sub_manifest.push(ManifestEntry {
        path: format!("{rel_path}#terrain/heightfield"),
        id,
        type_path: TILED_HEIGHTFIELD_TYPE_PATH.to_string(),
    });
    Ok((
        Some(TerrainHeightfieldIndex {
            width: hf.width,
            height: hf.height,
            tile_size: DEFAULT_TILE_SIZE,
            tiles_x,
            tiles_y,
            bytes,
        }),
        resolved,
    ))
}

/// First match of `<dir>/heightfield.png`, then `<dir>/../visual/heightfield.png`.
fn find_heightfield(dir: &Path) -> Option<PathBuf> {
    let local = dir.join("heightfield.png");
    if local.is_file() {
        return Some(local);
    }
    let visual = dir.parent()?.join("visual").join("heightfield.png");
    visual.is_file().then_some(visual)
}

/// Cooks one square RGBA8 slot through the tiled path, appending its tile/tail manifest entries.
#[allow(clippy::too_many_arguments)] // a self-contained sub-step of the terrain cook
fn cook_texture_slot(
    map_id: AssetId,
    slot: &str,
    width: u32,
    height: u32,
    rgba: &[u8],
    format: TileFormat,
    platform: &str,
    cache_dir: &Path,
    rel_path: &str,
    sub_manifest: &mut Vec<ManifestEntry>,
) -> Result<TerrainSlotIndex, CookError> {
    let cooked = cook_tiled_texture(
        slot,
        width,
        height,
        rgba,
        DEFAULT_TILE_SIZE,
        format,
        platform,
        TextureCookQuality::High,
    )
    .map_err(|e| CookError::Terrain(TerrainCookError::Tiled(e)))?;
    let written = write_tiled_texture_cooked(map_id, &cooked, cache_dir)
        .map_err(|e| CookError::Terrain(TerrainCookError::Tiled(e)))?;
    let bytes: u64 = written.iter().map(|(_, b)| *b).sum();

    // Manifest entries for the tail and every tile (deterministic ids match the streamer).
    sub_manifest.push(ManifestEntry {
        path: format!("{rel_path}#{}", tail_asset_path(slot)),
        id: tail_asset_id(map_id, slot),
        type_path: "regia_asset::CookedTexture".to_string(),
    });
    for tile in &cooked.tiles {
        sub_manifest.push(ManifestEntry {
            path: format!("{rel_path}#{}", tile_asset_path(slot, tile.mip, tile.x, tile.y)),
            id: tile_asset_id(map_id, slot, tile.mip, tile.x, tile.y),
            type_path: "regia_asset::CookedTexture".to_string(),
        });
    }

    Ok(TerrainSlotIndex {
        name: slot.to_string(),
        width: cooked.width,
        height: cooked.height,
        tile_size: cooked.tile_size,
        format: stream_format(cooked.format),
        mip_count: cooked.mip_count,
        tail_mips: cooked.tail_mips,
        tile_count: cooked.tiles.len() as u32,
        bytes,
    })
}

/// Reads every cooked terrain index from a cooked cache directory and formats a per-slot, per-map
/// size report for `regia cook --stats`. Returns `None` when the cache holds no terrain index.
///
/// # Errors
///
/// Returns [`TerrainCookError`] when the manifest or a terrain index cannot be read or decoded.
pub fn format_terrain_stats(cache_dir: &Path) -> Result<Option<String>, TerrainCookError> {
    let Some(manifest) = regia_asset::AssetManifest::read_from_cache(cache_dir)
        .map_err(|e| TerrainCookError::Stats(e.to_string()))?
    else {
        return Ok(None);
    };

    let mut maps: Vec<(String, TerrainIndex)> = Vec::new();
    for entry in &manifest.entries {
        if entry.type_path != TERRAIN_INDEX_TYPE_PATH {
            continue;
        }
        let path = cache_dir.join(format!("{}.fckd", entry.id));
        let bytes = std::fs::read(&path)?;
        let index = regia_terrain::stream::load_terrain_index_bytes(&bytes)
            .map_err(|e| TerrainCookError::Stats(e.to_string()))?;
        maps.push((entry.path.clone(), index));
    }
    if maps.is_empty() {
        return Ok(None);
    }
    maps.sort_by(|a, b| a.0.cmp(&b.0));

    let kib = |b: u64| b as f64 / 1024.0;
    let mut out = String::new();
    out.push_str("cooked terrain maps:\n");
    for (path, index) in &maps {
        out.push_str(&format!("  {path}\n"));
        if let Some(hf) = &index.heightfield {
            out.push_str(&format!(
                "    heightfield         {}x{} @{}   {:>3} tiles   {:>10.1} KiB\n",
                hf.width,
                hf.height,
                hf.tile_size,
                hf.tiles_x * hf.tiles_y,
                kib(hf.bytes)
            ));
        }
        for slot in &index.slots {
            out.push_str(&format!(
                "    {:<18} {:<9} {}x{} @{}   {:>3} tiles   {:>10.1} KiB\n",
                slot.name,
                format_name(slot.format),
                slot.width,
                slot.height,
                slot.tile_size,
                slot.tile_count,
                kib(slot.bytes)
            ));
        }
        if let Some(heights) = &index.heights {
            let encoding = match heights.encoding {
                regia_terrain::TerrainHeightsEncoding::Residual => "residual",
                regia_terrain::TerrainHeightsEncoding::Raw => "raw",
            };
            out.push_str(&format!(
                "    exact heights       {}x{} {encoding:<8}               {:>10.1} KiB\n",
                heights.width,
                heights.height,
                kib(heights.bytes)
            ));
        }
        if let Some(holes) = &index.holes {
            out.push_str(&format!(
                "    holes                                          {:>10.1} KiB\n",
                kib(holes.bytes)
            ));
        }
        if let Some(material) = &index.material {
            out.push_str(&format!(
                "    cooked material     {} layers, macro {}x{}            {:>10.1} KiB\n",
                material.layer_albedo,
                material.macro_tiles,
                material.macro_tiles,
                kib(material.bytes)
            ));
        }
        out.push_str(&format!(
            "    total ({} layers)                      {:>10.1} KiB\n",
            index.layers.len(),
            kib(index.total_bytes())
        ));
    }
    Ok(Some(out))
}

/// A short human-readable name for a block format in the stats report.
fn format_name(format: StreamTileFormat) -> &'static str {
    match format {
        StreamTileFormat::Bc7Srgb => "BC7 sRGB",
        StreamTileFormat::Bc5 => "BC5",
        StreamTileFormat::Bc4 => "BC4",
        StreamTileFormat::Bc7Linear => "BC7 lin",
    }
}

#[cfg(test)]
mod portable_hash_tests {
    use super::*;

    fn inputs(dir: &str, sep: char) -> Vec<(String, Vec<u8>)> {
        let p = |rel: &str| format!("{dir}{sep}{}", rel.replace('/', &sep.to_string()));
        let parent = dir.rsplit_once(sep).map_or("", |(a, _)| a);
        [
            (p("heightfield.png"), b"PNGDATA".to_vec()),
            (
                p("heightfield.json"),
                br#"{"size_m": 1024.0, "origin": [0.0, -5.0, 0.0]}"#.to_vec(),
            ),
            (p("detail/rock/albedo.png"), b"ROCK".to_vec()),
            (
                format!("{parent}{sep}visual{sep}heightfield.png"),
                b"VISUAL".to_vec(),
            ),
        ]
        .into_iter()
        .map(|(path, bytes)| (terrain_input_tag(dir, &path), bytes))
        .collect()
    }

    /// ADR-2440: Windows-style and Unix-style paths of the same content hash alike.
    #[test]
    fn windows_and_unix_paths_hash_alike() {
        let unix = inputs("/home/dev/game/assets/maps/island", '/');
        let windows = inputs(r"C:\Users\dev\game\assets\maps\island", '\\');
        let tags: Vec<&str> = unix.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            tags,
            [
                "heightfield.png",
                "heightfield.json",
                "detail/rock/albedo.png",
                "../visual/heightfield.png"
            ]
        );
        assert_eq!(
            unix.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            windows.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>()
        );
        assert_eq!(terrain_inputs_hash(&unix), terrain_inputs_hash(&windows));
        // A mixed path and a `.` segment are the same tag too.
        assert_eq!(terrain_input_tag("a/b", r"a\b\./detail\x.png"), "detail/x.png");
        // Input order does not matter; content does.
        let mut reversed = unix.clone();
        reversed.reverse();
        assert_eq!(terrain_inputs_hash(&reversed), terrain_inputs_hash(&unix));
        let mut changed = unix.clone();
        changed[0].1 = b"OTHER".to_vec();
        assert_ne!(terrain_inputs_hash(&changed), terrain_inputs_hash(&unix));
    }

    /// The byte order is explicit: lengths are u64 little-endian, extents f32 bits little-endian,
    /// whatever the host's order.
    #[test]
    fn the_framing_and_the_extents_are_little_endian() {
        // 1024.0f32 = 0x4480_0000; -5.0f32 = 0xC0A0_0000.
        let canonical = canonical_extents_bytes(br#"{"size_m": 1024, "origin": [0, -5.0, 0]}"#);
        assert_eq!(
            canonical,
            [
                1, 0x00, 0x00, 0x80, 0x44, // size_m
                1, 0, 0, 0, 0, 0x00, 0x00, 0xA0, 0xC0, 0, 0, 0, 0, // origin
            ]
        );
        // The float spelling does not matter.
        assert_eq!(
            canonical,
            canonical_extents_bytes(br#"{"origin": [0.0, -5e0, 0.0], "size_m": 1.024e3}"#)
        );
        assert_eq!(canonical_extents_bytes(b"{}"), [0u8; 18]);
        assert_eq!(canonical_extents_bytes(b"nope")[0], 0xFF);

        // One input, framed by hand with explicit little-endian lengths.
        let mut expected = blake3::Hasher::new();
        expected.update(TERRAIN_INPUTS_HASH_DOMAIN);
        expected.update(&[15, 0, 0, 0, 0, 0, 0, 0]);
        expected.update(b"heightfield.png");
        expected.update(&[3, 0, 0, 0, 0, 0, 0, 0]);
        expected.update(b"abc");
        assert_eq!(
            terrain_inputs_hash(&[("heightfield.png".to_owned(), b"abc".to_vec())]),
            *expected.finalize().as_bytes()
        );
    }

    /// The hash of files on disk: their place, not the folder they sit in.
    #[test]
    fn two_copies_of_a_terrain_in_different_folders_hash_alike() {
        let hashes: Vec<([u8; 32], [u8; 32])> = ["copy_a", "copy_b"]
            .into_iter()
            .map(|name| {
                let root = std::env::temp_dir().join(format!(
                    "regia_terrain_hash_{name}_{}_{}",
                    std::process::id(),
                    line!()
                ));
                let dir = root.join("maps").join("island");
                std::fs::create_dir_all(&dir).expect("dir");
                std::fs::write(dir.join("heightfield.png"), b"samples").expect("write");
                std::fs::write(dir.join("heightfield.json"), br#"{"size_m": 512}"#).expect("write");
                std::fs::create_dir_all(dir.join("detail").join("rock")).expect("dir");
                std::fs::write(dir.join("detail/rock/albedo.png"), b"rock").expect("write");
                let layer = dir.join("island.terrain.layers.ron");
                // The reader `terrain_settings_hash` uses, on its kind of path list.
                let files = vec![
                    dir.join("heightfield.png"),
                    dir.join("heightfield.json"),
                    dir.join("detail").join("rock").join("albedo.png"),
                    dir.join("missing.png"),
                ];
                let inputs = read_inputs(&layer, files).expect("read");
                assert_eq!(inputs.len(), 3, "a missing file is skipped");
                let out = (
                    terrain_inputs_hash(&inputs),
                    terrain_ground_hash(&layer).expect("ground"),
                );
                let _ = std::fs::remove_dir_all(&root);
                out
            })
            .collect();
        assert_eq!(hashes[0], hashes[1]);
    }

    /// The ground hash is the heightfield the cook reads and the json beside that file. The other
    /// location (`docs/formats/terrain.md` § 9) does not change it.
    #[test]
    fn ground_hash_ignores_the_unused_heightfield_sibling() {
        let root = regia_test_utils::unique_temp_dir("terrain_ground_sibling");
        let dir = root.join("maps").join("island");
        let visual = root.join("maps").join("visual");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::create_dir_all(&visual).expect("visual");
        std::fs::write(dir.join("heightfield.png"), b"local-a").expect("png");
        std::fs::write(dir.join("heightfield.json"), br#"{"size_m": 64}"#).expect("json");
        std::fs::write(visual.join("heightfield.png"), b"visual-b").expect("png");
        std::fs::write(visual.join("heightfield.json"), br#"{"size_m": 999}"#).expect("json");
        let layer = dir.join("island.terrain.layers.ron");
        let before = terrain_ground_hash(&layer).expect("hash");
        std::fs::write(visual.join("heightfield.png"), b"visual-changed").expect("png");
        std::fs::write(visual.join("heightfield.json"), br#"{"size_m": 1}"#).expect("json");
        assert_eq!(
            terrain_ground_hash(&layer).expect("unused sibling"),
            before,
            "the visual sibling is not the heightfield the cook reads"
        );
        std::fs::write(dir.join("heightfield.png"), b"local-changed").expect("png");
        assert_ne!(
            terrain_ground_hash(&layer).expect("used png"),
            before,
            "the local heightfield is the one the cook reads"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
