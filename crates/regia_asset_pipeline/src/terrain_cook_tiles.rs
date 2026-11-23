// SPDX-License-Identifier: MPL-2.0
//! Cooking a heightfield from multi-file source tiles (ADR-8595, `docs/formats/terrain.md` § 15).
//!
//! A `heightfield.tiles.json` beside a `*.terrain.layers.ron` replaces `heightfield.png`: the
//! cook validates the descriptor's structure, then feeds [`cook_tiled_heightfield_regions`] one
//! cooked tile region at a time from a [`TileSetReader`], which decodes only the source tiles
//! covering that region and refuses disagreeing shared samples. Nothing is published until the
//! whole grid passed: the cooked heightfield `.fckd` is written once, atomically, at the end.
//!
//! Evidence of the last successful cook (each source tile's BLAKE3, the structure, and the
//! BLAKE3 of the `.fckd` it produced) sits in `<cache>/terrain-source-tiles/`; a recook decodes
//! only the source tiles whose bytes changed and the neighbors sharing their regions, and
//! copies every other cooked tile from the previous `.fckd`, byte for byte.

use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use regia_asset::{AssetId, cooked_file_name};
use regia_terrain::source::{ImportLimits, SourceError, SourceTileRef, TileSetDescriptor, TileSetReader};
use regia_terrain::stream::TerrainHeightfieldIndex;
use serde::{Deserialize, Serialize};

use super::TerrainCookError;
use crate::terrain_data::{
    RegionFill, TiledHeightfield, cook_tiled_heightfield_regions, heightfield_asset_id,
    write_tiled_heightfield_cooked,
};

/// The tile-set descriptor the cook reads beside a terrain layer file, in place of
/// `heightfield.png`.
pub const TILE_SET_FILE: &str = "heightfield.tiles.json";

/// Folder under the cook cache holding tile-set evidence (one JSON per map).
pub const TILE_EVIDENCE_DIR: &str = "terrain-source-tiles";

const EVIDENCE_VERSION: u32 = 1;

/// Deterministic counters of one tile-set heightfield cook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SourceTileCookReport {
    /// Source tiles the descriptor names.
    pub source_tiles: usize,
    /// Source tiles whose bytes differ from the last cook's evidence (all of them without it).
    pub source_tiles_changed: usize,
    /// Source tile decodes (a tile is decoded once per cooked-tile row it spans).
    pub source_tiles_decoded: u64,
    /// Cooked heightfield tiles.
    pub cooked_tiles: usize,
    /// Cooked tiles built from source samples (the rest were copied from the previous cook).
    pub cooked_tiles_recooked: usize,
    /// Largest sum of decoded source-tile samples held at once.
    pub peak_charged_samples: usize,
    /// Shared samples compared between overlapping tiles.
    pub seam_samples_checked: u64,
}

/// One step of a tile-set cook, for progress reports and cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceTileProgress {
    /// The descriptor passed its structure check (counts: tiles, changed tiles).
    Validated {
        /// Source tiles named.
        tiles: usize,
        /// Source tiles changed since the last cook.
        changed: usize,
    },
    /// A source tile is about to be decoded.
    Decoding {
        /// Its id (`r<row>_c<col>`).
        id: String,
        /// Decodes so far.
        decoded: u64,
    },
    /// A cooked tile region is done.
    Region {
        /// Regions done.
        done: usize,
        /// Regions in total.
        total: usize,
    },
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Evidence {
    version: u32,
    tile_size: u32,
    mapping: String,
    max_overlap: u32,
    tiles: Vec<EvidenceTile>,
    cooked_blake3: String,
}

#[derive(Serialize, Deserialize, PartialEq, Clone)]
struct EvidenceTile {
    path: String,
    origin: [u32; 2],
    size: [u32; 2],
    blake3: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn source_err(e: SourceError) -> TerrainCookError {
    TerrainCookError::SourceTiles(e.0)
}

/// `<dir>/heightfield.tiles.json`, when present.
pub(super) fn find_tile_set(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(TILE_SET_FILE);
    path.is_file().then_some(path)
}

/// Reads and validates a descriptor (structure only; no tile file is opened).
///
/// # Errors
///
/// IO, or the descriptor's refusals as [`TerrainCookError::SourceTiles`].
pub fn read_tile_set(path: &Path) -> Result<TileSetDescriptor, TerrainCookError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(regia_terrain::source::TILE_SET_MAX_DESCRIPTOR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    TileSetDescriptor::from_json(&bytes, ImportLimits::default()).map_err(source_err)
}

/// The tile files a descriptor names, resolved beside it (empty when it does not parse: the
/// cook reports that).
pub(super) fn tile_set_files(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    read_tile_set(path)
        .map(|d| d.tiles().iter().map(|t| dir.join(&t.path)).collect())
        .unwrap_or_default()
}

/// BLAKE3 of a file, streamed (a tile's bytes are never held whole for hashing).
pub(super) fn file_blake3(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(*hasher.finalize().as_bytes())
}

fn read_tile_file(dir: &Path, tile: &SourceTileRef, limits: ImportLimits) -> Result<Vec<u8>, SourceError> {
    let path = dir.join(&tile.path);
    let io =
        |e: std::io::Error| SourceError(format!("height_io_read: tile {} ({}): {e}", tile.id(), tile.path));
    // A symlink must not pull a file from outside the descriptor's folder.
    let root = dir.canonicalize().map_err(io)?;
    let real = path.canonicalize().map_err(io)?;
    if !real.starts_with(&root) {
        return Err(SourceError(format!(
            "terrain_tiles_path: tile {} ({}) resolves outside the descriptor's folder",
            tile.id(),
            tile.path
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&real)
        .map_err(io)?
        .take(limits.max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    Ok(bytes)
}

fn evidence_path(cache_dir: &Path, map_id: AssetId) -> PathBuf {
    let id = heightfield_asset_id(map_id);
    cache_dir
        .join(TILE_EVIDENCE_DIR)
        .join(format!("{}.json", cooked_file_name(id)))
}

/// The previous cook this one may copy unchanged tiles from: its evidence, when the structure,
/// mapping and tile size are the same, and the `.fckd` on disk is the one it describes.
fn previous_cook(
    cache_dir: &Path,
    map_id: AssetId,
    current: &Evidence,
) -> Option<(Vec<EvidenceTile>, TiledHeightfield)> {
    let text = std::fs::read(evidence_path(cache_dir, map_id)).ok()?;
    let old: Evidence = serde_json::from_slice(&text).ok()?;
    let same_shape = old.version == EVIDENCE_VERSION
        && old.tile_size == current.tile_size
        && old.mapping == current.mapping
        && old.max_overlap == current.max_overlap
        && old.tiles.len() == current.tiles.len()
        && old
            .tiles
            .iter()
            .zip(&current.tiles)
            .all(|(a, b)| a.path == b.path && a.origin == b.origin && a.size == b.size);
    if !same_shape {
        return None;
    }
    let file = std::fs::read(cache_dir.join(cooked_file_name(heightfield_asset_id(map_id)))).ok()?;
    if hex(blake3::hash(&file).as_bytes()) != old.cooked_blake3 {
        return None;
    }
    let (_, payload) = regia_asset::decode_cooked_file(&file).ok()?;
    let previous: TiledHeightfield = postcard::from_bytes(payload).ok()?;
    Some((old.tiles, previous))
}

/// Cooks the heightfield of a tile set (`descriptor_path`) into `cache_dir` at
/// `tile_size`, returning its index entry, the global count mapping and the counters.
/// `progress` hears every step and may cancel with `ControlFlow::Break`: a cancelled or failed
/// cook writes nothing, so the previous `.fckd` and its evidence stay as they were.
///
/// # Errors
///
/// [`TerrainCookError::SourceTiles`] for every descriptor, tile, seam or cancel refusal (the
/// message starts with its stable code, e.g. `terrain_tiles_seam:`), IO and the tiled cook's own.
#[tracing::instrument(skip_all, target = "regia_asset_pipeline")]
pub fn cook_tile_set_heightfield(
    map_id: AssetId,
    descriptor_path: &Path,
    cache_dir: &Path,
    tile_size: u32,
    mut progress: impl FnMut(&SourceTileProgress) -> ControlFlow<()>,
) -> Result<
    (
        TerrainHeightfieldIndex,
        regia_terrain::source::CountMapping,
        SourceTileCookReport,
    ),
    TerrainCookError,
> {
    let dir = descriptor_path.parent().unwrap_or_else(|| Path::new("."));
    let descriptor = read_tile_set(descriptor_path)?;
    let mapping = descriptor.mapping();
    let limits = ImportLimits::default();
    let cancelled =
        || TerrainCookError::SourceTiles("height_source_cancelled: tile-set cook cancelled".into());

    let mut tiles = Vec::with_capacity(descriptor.tiles().len());
    for tile in descriptor.tiles() {
        let digest = file_blake3(&dir.join(&tile.path)).map_err(|e| {
            TerrainCookError::SourceTiles(format!("height_io_read: tile {} ({}): {e}", tile.id(), tile.path))
        })?;
        tiles.push(EvidenceTile {
            path: tile.path.clone(),
            origin: tile.origin,
            size: tile.size,
            blake3: hex(&digest),
        });
    }
    let mut evidence = Evidence {
        version: EVIDENCE_VERSION,
        tile_size,
        mapping: hex(&mapping.canonical_bytes()),
        max_overlap: descriptor.max_overlap(),
        tiles,
        cooked_blake3: String::new(),
    };
    let previous = previous_cook(cache_dir, map_id, &evidence);
    let changed: Vec<bool> = match &previous {
        Some((old, _)) => evidence
            .tiles
            .iter()
            .zip(old)
            .map(|(new, old)| new.blake3 != old.blake3)
            .collect(),
        None => vec![true; evidence.tiles.len()],
    };
    let mut report = SourceTileCookReport {
        source_tiles: descriptor.tiles().len(),
        source_tiles_changed: changed.iter().filter(|c| **c).count(),
        ..Default::default()
    };
    if progress(&SourceTileProgress::Validated {
        tiles: report.source_tiles,
        changed: report.source_tiles_changed,
    })
    .is_break()
    {
        return Err(cancelled());
    }

    let total = (mapping.cols.div_ceil(tile_size) * mapping.rows.div_ceil(tile_size)) as usize;
    let mut done = 0usize;
    let progress_cell = std::cell::RefCell::new(&mut progress);
    let mut reader = TileSetReader::new(
        &descriptor,
        limits,
        |tile: &SourceTileRef| read_tile_file(dir, tile, limits),
        |index: usize, decoded: u64| {
            let id = descriptor.tiles()[index].id();
            (progress_cell.borrow_mut())(&SourceTileProgress::Decoding { id, decoded })
        },
    );
    let tiled = cook_tiled_heightfield_regions(
        mapping.cols,
        mapping.rows,
        tile_size,
        previous.as_ref().map(|(_, p)| p),
        |region, buffer: &mut Vec<u16>| -> Result<RegionFill, TerrainCookError> {
            let rect = [region.x0, region.y0, region.x1, region.y1];
            let dirty = descriptor
                .tiles_intersecting(rect)
                .any(|i| changed.get(i).copied().unwrap_or(true));
            let fill = if dirty || previous.is_none() {
                reader.read_rect(rect, buffer).map_err(source_err)?;
                report.cooked_tiles_recooked += 1;
                RegionFill::Filled
            } else {
                RegionFill::Reuse
            };
            done += 1;
            if (progress_cell.borrow_mut())(&SourceTileProgress::Region { done, total }).is_break() {
                return Err(cancelled());
            }
            Ok(fill)
        },
    )?;
    let stats = reader.stats();
    report.source_tiles_decoded = stats.tiles_decoded;
    report.peak_charged_samples = stats.peak_charged_samples;
    report.seam_samples_checked = stats.seam_samples_checked;
    report.cooked_tiles = tiled.tiles.len();

    // Publish: the `.fckd` first, then the evidence naming its hash (a crash between the two
    // leaves evidence that no longer matches, so the next cook rebuilds everything).
    let (_, bytes) = write_tiled_heightfield_cooked(map_id, &tiled, cache_dir)?;
    let written = std::fs::read(cache_dir.join(cooked_file_name(heightfield_asset_id(map_id))))?;
    evidence.cooked_blake3 = hex(blake3::hash(&written).as_bytes());
    let evidence_file = evidence_path(cache_dir, map_id);
    if let Some(parent) = evidence_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_vec_pretty(&evidence).map_err(|e| TerrainCookError::SourceTiles(e.to_string()))?;
    crate::cook_ledger::write_atomic(&evidence_file, &text)?;

    let index = TerrainHeightfieldIndex {
        width: mapping.cols,
        height: mapping.rows,
        tile_size,
        tiles_x: mapping.cols.div_ceil(tile_size),
        tiles_y: mapping.rows.div_ceil(tile_size),
        bytes,
    };
    Ok((index, mapping, report))
}
