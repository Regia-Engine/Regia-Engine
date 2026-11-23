// SPDX-License-Identifier: MPL-2.0
//! Count-based source inspection and overview baking (ADR-8063); registered overviews with a
//! descriptor, cache and cancel (ADR-8596).

use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_terrain::overview::{
    MARKER_RGB, OverviewFit, OverviewMarker, OverviewProgress, OverviewSettings, PlacedMarker,
    RegisteredOverview, bake_overview_registered, draw_markers,
};
use regia_terrain::overview_artifact::{OverviewJob, descriptor_path, publish_overview};
use regia_terrain::source::{
    ImportLimits, MissingDataPolicy, MissingDatum, PngImportSettings, RasterDescriptor, SourceError,
    SourceTileRef, TerrainSource, TileSetDescriptor, TileSetReader,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Project/session ceiling for terrain source imports. Hosts insert this resource
/// after loading their project policy; an absent resource uses engine defaults.
/// Requests may lower these limits but cannot raise them.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct TerrainSourcePolicy {
    /// Host-selected ceilings, themselves clamped to `ImportLimits::HARD_MAX`.
    pub limits: ImportLimits,
}

const HINT: &str = "see book/agents/terrain-source.md";

fn coded(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, reason, message, HINT)
}

fn required<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    crate::params::opt_str(params, key)?.ok_or_else(|| RpcError::invalid_params(format!("{key} is required")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PngParams {
    meters_per_count: f32,
    cell_m: f32,
    sample0: Option<[f32; 2]>,
    datum_keyword: Option<String>,
    zero_counts: Option<u16>,
    #[serde(default)]
    center_zero: bool,
}

/// A count source read under a project root (ADR-8541): what `terrain.inspect_source`, the editor's
/// Terrain ▸ Import source workflow and `regia terrain source-inspect` all read, so the three
/// previews agree on bounds and datum by construction.
#[derive(Debug)]
pub struct ProjectSource {
    /// The decoded source (counts, datum, units and mapping).
    pub source: TerrainSource,
    /// The limits the read ran under (the request's, lowered by the policy).
    pub limits: ImportLimits,
    /// BLAKE3 of the input bytes (the source, then the mapping file when one is named), hex: the
    /// content generation a later apply compares against ([`source_inputs_digest`]).
    pub digest: String,
}

/// Resolves a project-relative `path` under `root`: no absolute path, no `..`, and no symlink that
/// leaves the root.
///
/// # Errors
///
/// `invalid_params` for a path outside the root.
pub fn resolve_project_path(root: &Path, path: &str) -> Result<PathBuf, RpcError> {
    let Ok(relative) = regia_core::project_path::check_relative(path) else {
        return Err(RpcError::invalid_params(
            "`path` must be relative to the project root, without `..`",
        ));
    };
    let file = root.join(relative);
    if let Ok(root) = root.canonicalize()
        && let Some(real) = file.ancestors().find_map(|p| p.canonicalize().ok())
        && !real.starts_with(&root)
    {
        return Err(RpcError::invalid_params(format!(
            "{path} resolves outside the project root"
        )));
    }
    Ok(file)
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .ok_or_else(|| {
            RpcError::not_found(
                "no AssetServer resource: no project root to read or write under",
                "run the method in an app with a project (the editor or a game)",
            )
        })
}

fn read_capped(file: &Path, cap: u64) -> Result<Vec<u8>, RpcError> {
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(cap).read_to_end(&mut bytes))
        .map_err(|e| coded("height_io_read", e.to_string()))?;
    Ok(bytes)
}

/// BLAKE3 over the inputs a source request names (the source bytes, then the mapping file's
/// when `mapping` is given, then the raster descriptor's when `raster` is given), hex, without decoding them. An apply compares it with
/// [`ProjectSource::digest`] to refuse a preview whose source changed since (ADR-8541).
///
/// # Errors
///
/// A path outside the root or a read failure (`height_io_read`).
pub fn source_inputs_digest(root: &Path, policy: ImportLimits, params: &Value) -> Result<String, RpcError> {
    let limits = requested_limits(params)?.restricted_by(policy);
    let path = required(params, "path")?;
    if is_tile_set_path(path) {
        let (descriptor, bytes) = read_tile_set_descriptor(root, path, limits)?;
        return tile_set_digest(root, path, &bytes, &descriptor);
    }
    let file = resolve_project_path(root, path)?;
    let source = read_capped(&file, limits.max_bytes as u64 + 1)?;
    let mapping = match crate::params::opt_str(params, "mapping")? {
        None => None,
        Some(path) => Some(read_capped(
            &resolve_project_path(root, path)?,
            MAX_MAPPING_BYTES + 1,
        )?),
    };
    let raster = raster_bytes(root, params)?;
    Ok(inputs_digest(&source, mapping.as_deref(), raster.as_deref()))
}

fn inputs_digest(source: &[u8], mapping: Option<&[u8]>, raster: Option<&[u8]>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(source.len() as u64).to_le_bytes());
    hasher.update(source);
    if let Some(mapping) = mapping {
        hasher.update(b"M");
        hasher.update(mapping);
    }
    // A raster descriptor (ADR-8542) changes the decoded source, so it is part of its identity.
    if let Some(raster) = raster {
        hasher.update(b"R");
        hasher.update(&(raster.len() as u64).to_le_bytes());
        hasher.update(raster);
    }
    hasher.finalize().to_hex().to_string()
}

fn requested_limits(params: &Value) -> Result<ImportLimits, RpcError> {
    let requested = params.get("limits").cloned().unwrap_or_else(|| json!({}));
    serde_json::from_value(requested).map_err(|e| RpcError::invalid_params(format!("limits: {e}")))
}

fn read_source(world: &World, params: &Value) -> Result<(TerrainSource, ImportLimits), RpcError> {
    let policy = world
        .get_resource::<TerrainSourcePolicy>()
        .copied()
        .unwrap_or_default();
    // The path is checked before the root: a bad path is `invalid_params` in any app.
    required(params, "path")?;
    let root = project_root(world)?;
    read_project_source(&root, policy.limits, params).map(|read| (read.source, read.limits))
}

/// Reads and validates the count source `params` names under `root` (the parameters of
/// `terrain.inspect_source`: `path`, optional `png`, `mapping` and `limits`), with the request's
/// limits lowered by `policy`. Writes nothing. A `*.tiles.json` path is a source tile set
/// (ADR-8595): its tiles are validated and assembled into one source within `limits`.
///
/// # Errors
///
/// The documented `terrain.inspect_source` refusals (`height_io_read`, `height_io_bad_format`,
/// `height_io_decode`, `height_source_limit_*`, `terrain_tiles_*`), each with `effective_limits`
/// in its data.
pub fn read_project_source(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
) -> Result<ProjectSource, RpcError> {
    read_project_source_observed(root, policy, params, &mut |_, _| {
        std::ops::ControlFlow::Continue(())
    })
}

/// [`read_project_source`] reporting tile progress for a source tile set: `observer` hears
/// `(tiles decoded so far, tiles in the set)` before each tile decode and may cancel with
/// `ControlFlow::Break` (`height_source_cancelled`). Other sources never call it.
///
/// # Errors
///
/// As [`read_project_source`].
pub fn read_project_source_observed(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
    observer: &mut dyn FnMut(u64, usize) -> std::ops::ControlFlow<()>,
) -> Result<ProjectSource, RpcError> {
    let path = required(params, "path")?;
    if is_tile_set_path(path) {
        return read_tile_set_source(root, policy, params, path, observer);
    }
    let file = resolve_project_path(root, path)?;
    let limits = requested_limits(params)?.restricted_by(policy);
    let bytes = read_capped(&file, limits.max_bytes as u64 + 1)?;
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let result = match ext.as_str() {
        "json" => {
            if params.get("png").is_some() {
                return Err(RpcError::invalid_params("png settings only apply to PNG sources"));
            }
            TerrainSource::from_json(&bytes, limits)
        }
        "png" => {
            let args: PngParams = serde_json::from_value(params.get("png").cloned().unwrap_or(Value::Null))
                .map_err(|e| RpcError::invalid_params(format!("png settings: {e}")))?;
            if args.center_zero && args.zero_counts.is_some() {
                return Err(RpcError::invalid_params(
                    "choose center_zero or zero_counts, not both",
                ));
            }
            let missing_datum = match (args.zero_counts, args.center_zero) {
                (Some(v), _) => MissingDatum::Counts(v),
                (_, true) => MissingDatum::CenterSample,
                _ => MissingDatum::Reject,
            };
            let mut settings = PngImportSettings {
                meters_per_count: args.meters_per_count,
                cell_m: args.cell_m,
                sample0: args.sample0,
                missing_datum,
                ..Default::default()
            };
            if let Some(keyword) = args.datum_keyword {
                settings.datum_keyword = keyword;
            }
            TerrainSource::from_png(&bytes, &settings, limits)
        }
        _ => {
            return Err(coded(
                "height_io_bad_format",
                "source must have .json or .png extension",
            ));
        }
    };
    // A cook mapping (`heightfield.json` schema 2, ADR-8471) reinterprets the decoded counts
    // exactly as `regia cook` will: its datum, scale, cell and registration replace the request's.
    let mut mapping_bytes = None;
    let result = match crate::params::opt_str(params, "mapping")? {
        None => result,
        Some(mapping_path) => {
            let mapping_file = resolve_project_path(root, mapping_path)?;
            let text = read_capped(&mapping_file, MAX_MAPPING_BYTES + 1)?;
            if text.len() as u64 > MAX_MAPPING_BYTES {
                return Err(coded("height_io_decode", "the mapping file is over 1 MiB"));
            }
            let mapping = regia_terrain::source::CountMapping::from_heightfield_json(&text)
                .map_err(|e| coded("height_io_decode", e.to_string()))?
                .ok_or_else(|| {
                    coded(
                        "height_io_decode",
                        "the mapping file has no \"schema\": 2 count mapping",
                    )
                })?;
            mapping_bytes = Some(text);
            result.and_then(|source| TerrainSource::from_counts(source.counts().to_vec(), mapping, limits))
        }
    };
    // A raster descriptor (ADR-8542) reinterprets the decoded counts' layout: X/Z spacing, row and
    // column order (an exact permutation), registration and missing data.
    let (result, raster_bytes) = match read_raster(root, params, limits)? {
        None => (result, None),
        Some((descriptor, raster_bytes)) => (
            result.and_then(|source| source.with_raster(&descriptor, limits)),
            Some(raster_bytes),
        ),
    };
    let digest = inputs_digest(&bytes, mapping_bytes.as_deref(), raster_bytes.as_deref());
    result
        .map(|source| ProjectSource {
            source,
            limits,
            digest,
        })
        .map_err(|e| {
            let mut error = source_error(&e, limits);
            // A monolithic source over a budget: point at the tiled form (ADR-8595).
            if matches!(
                e.code(),
                "height_source_limit_bytes" | "height_source_limit_samples"
            ) && let Some(data) = error.data.as_mut().and_then(Value::as_object_mut)
            {
                data.insert("hint".into(), json!(SPLIT_HINT));
            }
            error
        })
}

/// The hint a monolithic source over a budget carries.
const SPLIT_HINT: &str = "split the source into tiles under a heightfield.tiles.json descriptor (book/agents/terrain-source.md, Source tiles); each tile stays within the limits and the cook streams them";

fn source_error(e: &SourceError, limits: ImportLimits) -> RpcError {
    let mut error = coded(e.code(), e.to_string());
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("effective_limits".into(), json!(limits));
    }
    error
}

/// Square read band of tile-set reads (samples): bounds the rectangle buffer.
const TILE_READ_BAND: u32 = 256;

/// True for a source tile-set descriptor path (`*.tiles.json`, ADR-8595).
fn is_tile_set_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".tiles.json")
}

/// The descriptor's folder, project-relative (`""` at the root).
fn tile_set_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn tile_rel_path(descriptor: &str, tile: &SourceTileRef) -> String {
    match tile_set_dir(descriptor) {
        "" => tile.path.clone(),
        dir => format!("{dir}/{}", tile.path),
    }
}

fn read_tile_set_descriptor(
    root: &Path,
    path: &str,
    limits: ImportLimits,
) -> Result<(TileSetDescriptor, Vec<u8>), RpcError> {
    let file = resolve_project_path(root, path)?;
    let bytes = read_capped(
        &file,
        regia_terrain::source::TILE_SET_MAX_DESCRIPTOR_BYTES as u64 + 1,
    )?;
    let descriptor = TileSetDescriptor::from_json(&bytes, limits).map_err(|e| source_error(&e, limits))?;
    Ok((descriptor, bytes))
}

/// BLAKE3 of the descriptor and, in its (sorted) order, each tile's path and streamed BLAKE3.
fn tile_set_digest(
    root: &Path,
    path: &str,
    descriptor_bytes: &[u8],
    descriptor: &TileSetDescriptor,
) -> Result<String, RpcError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"regia-source-tiles\0");
    hasher.update(&(descriptor_bytes.len() as u64).to_le_bytes());
    hasher.update(descriptor_bytes);
    for tile in descriptor.tiles() {
        let file = resolve_project_path(root, &tile_rel_path(path, tile))?;
        let mut tile_hasher = blake3::Hasher::new();
        std::fs::File::open(&file)
            .and_then(|f| tile_hasher.update_reader(f).map(|_| ()))
            .map_err(|e| {
                coded(
                    "height_io_read",
                    format!("tile {} ({}): {e}", tile.id(), tile.path),
                )
            })?;
        hasher.update(&(tile.path.len() as u64).to_le_bytes());
        hasher.update(tile.path.as_bytes());
        hasher.update(tile_hasher.finalize().as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Reads one tile's bytes (project-contained, at most `limits.max_bytes`).
fn read_tile_bytes(
    root: &Path,
    descriptor: &str,
    tile: &SourceTileRef,
    limits: ImportLimits,
) -> Result<Vec<u8>, SourceError> {
    let file = resolve_project_path(root, &tile_rel_path(descriptor, tile)).map_err(|e| {
        SourceError(format!(
            "height_io_read: tile {} ({}): {}",
            tile.id(),
            tile.path,
            e.message
        ))
    })?;
    let bytes = read_capped(&file, limits.max_bytes as u64 + 1).map_err(|e| {
        SourceError(format!(
            "height_io_read: tile {} ({}): {}",
            tile.id(),
            tile.path,
            e.message
        ))
    })?;
    Ok(bytes)
}

fn reject_tile_set_options(params: &Value) -> Result<(), RpcError> {
    for key in ["png", "mapping", "raster"] {
        if params.get(key).is_some_and(|v| !v.is_null()) {
            return Err(RpcError::invalid_params(format!(
                "`{key}` does not apply to a source tile set: its descriptor carries the mapping"
            )));
        }
    }
    Ok(())
}

fn read_tile_set_source(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
    path: &str,
    observer: &mut dyn FnMut(u64, usize) -> std::ops::ControlFlow<()>,
) -> Result<ProjectSource, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "terrain_source.read_tiles").entered();
    reject_tile_set_options(params)?;
    let limits = requested_limits(params)?.restricted_by(policy);
    let (descriptor, bytes) = read_tile_set_descriptor(root, path, limits)?;
    let digest = tile_set_digest(root, path, &bytes, &descriptor)?;
    let total = descriptor.tiles().len();
    let mut reader = TileSetReader::new(
        &descriptor,
        limits,
        |tile: &SourceTileRef| read_tile_bytes(root, path, tile, limits),
        |_: usize, decoded: u64| observer(decoded, total),
    );
    let source = reader
        .assemble(TILE_READ_BAND)
        .map_err(|e| source_error(&e, limits))?;
    Ok(ProjectSource {
        source,
        limits,
        digest,
    })
}

/// `terrain.inspect_source` on a tile set: validates every tile and seam in bounded reads and
/// reports coverage and counters (`tile_set`); a set within `limits` also answers the full
/// source summary.
fn inspect_tile_set(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
    path: &str,
) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "terrain_source.inspect_tiles").entered();
    reject_tile_set_options(params)?;
    let root = root.to_path_buf();
    let limits = requested_limits(params)?.restricted_by(policy);
    let (descriptor, _) = read_tile_set_descriptor(&root, path, limits)?;
    let coverage = descriptor.coverage().map_err(|e| source_error(&e, limits))?;
    let m = descriptor.mapping();
    let fits = coverage.grid_samples <= limits.max_samples as u64;
    let mut reader = TileSetReader::new(
        &descriptor,
        limits,
        |tile: &SourceTileRef| read_tile_bytes(&root, path, tile, limits),
        |_: usize, _: u64| std::ops::ControlFlow::Continue(()),
    );
    let mut out = if fits {
        let source = reader
            .assemble(TILE_READ_BAND)
            .map_err(|e| source_error(&e, limits))?;
        summary(&source, limits)
    } else {
        let (mut low, mut high) = (u16::MAX, 0u16);
        reader
            .for_each_rect(TILE_READ_BAND, |_, samples| {
                for &v in samples {
                    low = low.min(v);
                    high = high.max(v);
                }
                Ok(())
            })
            .map_err(|e| source_error(&e, limits))?;
        let height = |v| (f32::from(v) - f32::from(m.zero_counts)) * m.meters_per_count;
        json!({
            "width": m.cols, "height": m.rows, "samples": coverage.grid_samples,
            "meters_per_count": m.meters_per_count, "zero_counts": m.zero_counts,
            "cell_m": m.cell_m, "sample0": m.sample0_m,
            "min_m": height(low), "max_m": height(high),
            "row0": "south", "col0": "west", "effective_limits": limits,
            "world_min": m.world_min(), "world_max": m.world_max(),
            "mapping": mapping_json(&m),
            "spacing_m": [m.cell_m, m.cell_m], "missing_samples": 0,
        })
    };
    let stats = reader.stats();
    let listed: Vec<Value> = descriptor
        .tiles()
        .iter()
        .take(64)
        .map(|t| json!({ "id": t.id(), "path": t.path, "origin": t.origin, "size": t.size }))
        .collect();
    if let Some(object) = out.as_object_mut() {
        object.insert(
            "tile_set".into(),
            json!({
                "version": regia_terrain::source::TILE_SET_VERSION,
                "tiles": coverage.tiles,
                "tiles_listed": listed,
                "tiles_truncated": coverage.tiles > 64,
                "grid_samples": coverage.grid_samples,
                "tile_samples": coverage.tile_samples,
                "overlapping_pairs": coverage.overlapping_pairs,
                "max_overlap": descriptor.max_overlap(),
                "max_tile_samples": coverage.max_tile_samples,
                "covered": true,
                "assembled": fits,
                "tiles_decoded": stats.tiles_decoded,
                "seam_samples_checked": stats.seam_samples_checked,
                "peak_charged_samples": stats.peak_charged_samples,
            }),
        );
    }
    Ok(out)
}

/// The `raster` parameter: a project-relative descriptor path or an inline descriptor object.
/// Returns the descriptor and the bytes it was read from (they join the inputs digest).
fn read_raster(
    root: &Path,
    params: &Value,
    limits: ImportLimits,
) -> Result<Option<(RasterDescriptor, Vec<u8>)>, RpcError> {
    let bytes = raster_bytes(root, params)?;
    let Some(bytes) = bytes else { return Ok(None) };
    RasterDescriptor::from_json(&bytes, limits)
        .map(|descriptor| Some((descriptor, bytes)))
        .map_err(|e| coded(e.code(), e.to_string()))
}

/// The raw bytes of the `raster` parameter (file contents or the inline object's JSON), if any.
fn raster_bytes(root: &Path, params: &Value) -> Result<Option<Vec<u8>>, RpcError> {
    match params.get("raster") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) => {
            let file = resolve_project_path(root, path)?;
            let text = read_capped(&file, MAX_MAPPING_BYTES + 1)?;
            if text.len() as u64 > MAX_MAPPING_BYTES {
                return Err(coded("height_io_decode", "the raster descriptor is over 1 MiB"));
            }
            Ok(Some(text))
        }
        Some(inline @ Value::Object(_)) => serde_json::to_vec(inline)
            .map(Some)
            .map_err(|e| RpcError::invalid_params(format!("raster: {e}"))),
        Some(_) => Err(RpcError::invalid_params(
            "raster must be a project-relative path or a descriptor object",
        )),
    }
}

/// Read and validate a source without mutating a scene or writing an asset.
#[tracing::instrument(skip_all, target = "regia_agent")]
pub(crate) fn inspect(world: &World, params: &Value) -> Result<Value, RpcError> {
    let path = required(params, "path")?;
    if is_tile_set_path(path) {
        let policy = world
            .get_resource::<TerrainSourcePolicy>()
            .copied()
            .unwrap_or_default();
        return inspect_tile_set(&project_root(world)?, policy.limits, params, path);
    }
    let (source, limits) = read_source(world, params)?;
    Ok(summary(&source, limits))
}

/// What `terrain.inspect_source` answers for `params` under `root` (the CLI's
/// `regia terrain source-inspect`): [`source_summary`] of [`read_project_source`], or for a
/// source tile set (`*.tiles.json`, ADR-8595) the bounded validation read with its `tile_set`
/// report.
///
/// # Errors
///
/// As [`read_project_source`].
pub fn inspect_project_source(root: &Path, policy: ImportLimits, params: &Value) -> Result<Value, RpcError> {
    let path = required(params, "path")?;
    if is_tile_set_path(path) {
        return inspect_tile_set(root, policy, params, path);
    }
    let read = read_project_source(root, policy, params)?;
    Ok(summary(&read.source, read.limits))
}

/// The preview of a read source, as `terrain.inspect_source` answers it: size, datum, units,
/// bounds (`world_min` / `world_max`), corner heights, the limits it was read under, the cook
/// mapping and whether a level loads it exactly (`runtime_map`).
#[must_use]
pub fn source_summary(source: &TerrainSource, limits: ImportLimits) -> Value {
    summary(source, limits)
}

fn summary(source: &TerrainSource, limits: ImportLimits) -> Value {
    let [w, h] = source.dimensions();
    // Samples without data never count toward the height range (ADR-8542).
    let (low, high) = source
        .counts()
        .iter()
        .enumerate()
        .filter(|(i, _)| source.is_valid((*i % w as usize) as u32, (*i / w as usize) as u32))
        .fold((u16::MAX, 0u16), |(low, high), (_, &v)| (low.min(v), high.max(v)));
    let height = |v| (f32::from(v) - f32::from(source.zero_counts())) * source.meters_per_count();
    json!({
        "width": w, "height": h, "samples": source.counts().len(),
        "meters_per_count": source.meters_per_count(), "zero_counts": source.zero_counts(),
        "cell_m": source.cell_m(), "sample0": source.sample0(),
        "min_m": height(low), "max_m": height(high),
        "corners_m": [source.height_m(0, 0), source.height_m(w - 1, 0), source.height_m(0, h - 1), source.height_m(w - 1, h - 1)],
        "row0": "south", "col0": "west", "effective_limits": limits,
        "world_min": source.sample_world(0, 0), "world_max": source.sample_world(w - 1, h - 1),
        "mapping": source.count_mapping().ok().map(|m| mapping_json(&m)),
        "runtime_map": runtime_map_json(source),
        "spacing_m": source.spacing_m(),
        "missing_samples": source.missing_samples(),
        "raster": raster_json(source),
    })
}

/// The raster layout as declared and as canonicalized (ADR-8542): the mapping axes, the
/// registration and the missing samples (canonical `[col, row]`, at most 64 listed).
fn raster_json(source: &TerrainSource) -> Value {
    let layout = source.layout();
    let [w, h] = source.dimensions();
    let mut missing = Vec::new();
    if let Some(mask) = source.valid_mask() {
        'rows: for row in 0..h {
            for col in 0..w {
                if !mask.is_valid(col, row) {
                    if missing.len() == MAX_LISTED_MISSING {
                        break 'rows;
                    }
                    missing.push([col, row]);
                }
            }
        }
    }
    json!({
        "descriptor_version": layout.version,
        "declared": { "row0": layout.row0, "col0": layout.col0 },
        "canonical": { "row0": "south", "col0": "west", "x_axis": "+X by column", "z_axis": "+Z by row" },
        "registration": layout.registration,
        "spacing_m": source.spacing_m(),
        "sample0_m": source.sample0(),
        "missing_samples": source.missing_samples(),
        "missing_cells": missing,
        "missing_cells_truncated": source.missing_samples() > missing.len(),
    })
}

/// Missing samples listed by [`raster_json`]; the count is always exact.
const MAX_LISTED_MISSING: usize = 64;

/// Largest `heightfield.json` a `mapping` parameter reads, bytes.
const MAX_MAPPING_BYTES: u64 = 1 << 20;

/// A count mapping as the agent reports it (ADR-8471): every value the cook persists.
pub(crate) fn mapping_json(m: &regia_terrain::source::CountMapping) -> Value {
    json!({
        "cols": m.cols, "rows": m.rows,
        "meters_per_count": m.meters_per_count, "zero_counts": m.zero_counts,
        "cell_m": m.cell_m, "sample0_m": m.sample0_m,
        "world_min": m.world_min(), "world_max": m.world_max(),
    })
}

/// Whether a level naming this source after a cook loads it exactly (`loadable`), and why not.
fn runtime_map_json(source: &TerrainSource) -> Value {
    let report = |policy| match source.terrain_map(policy) {
        Ok((map, hole)) => json!({
            "loadable": true, "grid_origin": map.grid_origin(), "world_width": map.world_width(),
            "hole": hole.is_some(),
        }),
        Err(e) => json!({ "loadable": false, "reason": e.0 }),
    };
    let mut value = report(MissingDataPolicy::Refuse);
    if source.missing_samples() > 0
        && let Some(object) = value.as_object_mut()
    {
        object.insert("with_holes".into(), report(MissingDataPolicy::Holes));
    }
    value
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BakeParams {
    size: Option<[u32; 2]>,
    relief: Option<f32>,
    contour_step_m: Option<f32>,
    blur_passes: Option<u8>,
    light: Option<[f32; 3]>,
    ambient: Option<f32>,
    contrast: Option<f32>,
    frame_pixels: Option<u32>,
    fit: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerParams {
    label: Option<String>,
    /// World X/Z, meters.
    xz: Option<[f64; 2]>,
    /// A scene entity (an id, `{ "name": glob }` or `{ "component": type }`): its world X/Z.
    entity: Option<Value>,
    rgb: Option<[u8; 3]>,
}

/// The overview settings of a `terrain.bake_overview` request (`overview` object, ADR-8063; `fit`,
/// ADR-8596), the same for the agent, `regia terrain overview` and the editor's overview job.
///
/// # Errors
/// `invalid_params` for an unknown field or fit name.
pub fn overview_settings(params: &Value) -> Result<OverviewSettings, RpcError> {
    let args: BakeParams =
        serde_json::from_value(params.get("overview").cloned().unwrap_or_else(|| json!({})))
            .map_err(|e| RpcError::invalid_params(format!("overview settings: {e}")))?;
    let mut settings = OverviewSettings::default();
    if let Some(v) = args.size {
        settings.size = v;
    }
    if let Some(v) = args.relief {
        settings.relief = v;
    }
    settings.contour_step_m = args.contour_step_m;
    if let Some(v) = args.blur_passes {
        settings.blur_passes = v;
    }
    if let Some(v) = args.light {
        settings.light = v;
    }
    if let Some(v) = args.ambient {
        settings.ambient = v;
    }
    if let Some(v) = args.contrast {
        settings.contrast = v;
    }
    if let Some(v) = args.frame_pixels {
        settings.frame_pixels = v;
    }
    if let Some(name) = args.fit {
        settings.fit = OverviewFit::parse(&name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "overview settings: fit must be \"stretch\" or \"preserve_aspect\", not {name:?}"
            ))
        })?;
    }
    Ok(settings)
}

/// Largest number of markers one overview draws.
pub const MAX_OVERVIEW_MARKERS: usize = 256;

/// The request's `markers` (ADR-8596): each `{ label?, xz: [x, z], rgb? }`, or with a `world`,
/// `{ label?, entity, rgb? }` resolved to the entity's world X/Z (its name is the default label).
///
/// # Errors
/// `invalid_params` for a malformed marker, more than [`MAX_OVERVIEW_MARKERS`], an entity marker
/// without a world or an entity that does not resolve.
pub fn overview_markers(world: Option<&World>, params: &Value) -> Result<Vec<OverviewMarker>, RpcError> {
    let list: Vec<MarkerParams> = match params.get("markers") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::invalid_params(format!("markers: {e}")))?,
    };
    if list.len() > MAX_OVERVIEW_MARKERS {
        return Err(RpcError::invalid_params(format!(
            "markers: at most {MAX_OVERVIEW_MARKERS}"
        )));
    }
    list.into_iter()
        .enumerate()
        .map(|(i, m)| {
            let (world_xz, default_label) = match (m.xz, &m.entity) {
                (Some(xz), None) if xz.iter().all(|v| v.is_finite()) => (xz, format!("marker {i}")),
                (None, Some(reference)) => {
                    let world = world.ok_or_else(|| {
                        RpcError::invalid_params(format!("markers[{i}]: entity markers need an open scene"))
                    })?;
                    let entity = crate::scene::resolve_entity(world, reference)
                        .map_err(|e| RpcError::invalid_params(format!("markers[{i}]: {e}")))?;
                    let entity_ref = world
                        .get_entity(entity)
                        .map_err(|e| RpcError::invalid_params(format!("markers[{i}]: {e}")))?;
                    let p = entity_ref
                        .get::<regia_scene::GlobalTransform>()
                        .map(|g| g.0.to_scale_rotation_translation().2)
                        .filter(|p| *p != regia_core::math::Vec3::ZERO)
                        .or_else(|| entity_ref.get::<regia_scene::Transform>().map(|t| t.translation))
                        .unwrap_or(regia_core::math::Vec3::ZERO);
                    let name = crate::scene::entity_name(world, entity)
                        .unwrap_or_else(|| format!("entity {}", entity.to_bits()));
                    ([f64::from(p.x), f64::from(p.z)], name)
                }
                _ => {
                    return Err(RpcError::invalid_params(format!(
                        "markers[{i}]: give finite xz [x, z] or entity, not both"
                    )));
                }
            };
            Ok(OverviewMarker {
                label: m.label.unwrap_or(default_label),
                world_xz,
                rgb: m.rgb.unwrap_or(MARKER_RGB),
            })
        })
        .collect()
}

fn overview_error(e: &regia_terrain::source::SourceError) -> RpcError {
    if e.0.starts_with("height_io_write:") {
        coded("height_io_write", e.to_string())
    } else {
        coded(e.code(), e.to_string())
    }
}

/// The mapping, legend and markers of a bake as the agent reports them (the descriptor's fields).
fn overview_layout_json(
    baked: &RegisteredOverview,
    placed: &[PlacedMarker],
    markers: &[OverviewMarker],
) -> Value {
    let m = &baked.mapping;
    json!({
        "mapping": {
            "image_size": m.image_size, "content_px": m.content_px,
            "world_min": m.world_min, "world_max": m.world_max,
            "meters_per_pixel": m.meters_per_pixel(),
        },
        "legend": {
            "elevation_range_m": baked.legend.elevation_range_m,
            "scale_bar_m": baked.legend.scale_bar_m, "scale_bar_px": baked.legend.scale_bar_px,
            "missing_rgb": regia_terrain::overview::MISSING_RGB,
            "letterbox_rgb": regia_terrain::overview::LETTERBOX_RGB,
        },
        "markers": markers.iter().zip(placed).map(|(m, p)| json!({
            "label": m.label, "world_xz": m.world_xz, "rgb": m.rgb, "pixel": p.pixel,
        })).collect::<Vec<_>>(),
    })
}

/// Checks a request's `output`: a `.png` under `root` that replaces neither the source nor, for a
/// registered bake, has a descriptor path equal to the source.
fn overview_output(root: &Path, params: &Value) -> Result<(PathBuf, PathBuf), RpcError> {
    let output = required(params, "output")?;
    let file = resolve_project_path(root, output)?;
    if file
        .extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| !e.eq_ignore_ascii_case("png"))
    {
        return Err(coded("height_io_bad_format", "overview output must end in .png"));
    }
    let input = resolve_project_path(root, required(params, "path")?)?;
    let same = |a: &Path| a == input || (a.exists() && a.canonicalize().ok() == input.canonicalize().ok());
    if same(&file) || same(&descriptor_path(&file)) {
        return Err(RpcError::invalid_params(
            "overview output must not replace its source",
        ));
    }
    Ok((file, input))
}

/// Bakes the overview a `terrain.bake_overview` request names under `root` and publishes it as a
/// registered pair (ADR-8596): the PNG at `output` and its descriptor `<name>.overview.json`,
/// cached by source and settings (a current pair is not baked again: `cached: true`). `observer`
/// sees the bake's steps and the commit step; [`ControlFlow::Break`] cancels before publication
/// (`terrain_overview_cancelled`). What `terrain.bake_overview { register: true }`,
/// `regia terrain source-overview` and the editor's `terrain_overview:<n>` job run.
///
/// # Errors
/// The source reader's refusals, `height_io_bad_format`, `height_io_write`,
/// `terrain_overview_cancelled` and invalid settings.
pub fn publish_project_overview(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
    markers: Vec<OverviewMarker>,
    observer: impl FnMut(OverviewProgress) -> ControlFlow<()>,
) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "terrain_source.publish_overview").entered();
    let output = required(params, "output")?;
    let settings = overview_settings(params)?;
    let (file, _) = overview_output(root, params)?;
    let read = read_project_source(root, policy, params)?;
    let job = OverviewJob {
        source: &read.source,
        source_path: required(params, "path")?.replace('\\', "/"),
        source_digest: read.digest.clone(),
        settings,
        markers,
    };
    let published = publish_overview(&job, &file, observer).map_err(|e| overview_error(&e))?;
    let d = &published.descriptor;
    let descriptor_rel = Path::new(output)
        .with_file_name(
            published
                .descriptor_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
        .to_string_lossy()
        .replace('\\', "/");
    Ok(json!({
        "source": summary(&read.source, read.limits), "output": output,
        "width": d["image"]["width"], "height": d["image"]["height"], "bytes": d["image"]["bytes"],
        "north_up": true, "missing_pixels": d["image"]["missing_pixels"],
        "missing_rgb": regia_terrain::overview::MISSING_RGB,
        "fit": d["settings"]["fit"], "mapping": d["mapping"], "legend": d["legend"], "markers": d["markers"],
        "registered": true, "descriptor": descriptor_rel, "cached": published.cached,
        "cache_key": d["cache_key"],
    }))
}

/// Read a source and atomically write a north-up shaded RGB PNG under the project root; with
/// `register: true` also its descriptor, through the cache ([`publish_project_overview`]).
#[tracing::instrument(skip_all, target = "regia_agent")]
pub(crate) fn bake(world: &World, params: &Value) -> Result<Value, RpcError> {
    let output = required(params, "output")?;
    // Checked before the root: a bad path is `invalid_params` in any app.
    for path in [output, required(params, "path")?] {
        if regia_core::project_path::check_relative(path).is_err() {
            return Err(RpcError::invalid_params(
                "`path` must be relative to the project root, without `..`",
            ));
        }
    }
    let register = crate::params::opt_bool(params, "register")?.unwrap_or(false);
    let markers = overview_markers(Some(world), params)?;
    let root = project_root(world)?;
    if register {
        let policy = world
            .get_resource::<TerrainSourcePolicy>()
            .copied()
            .unwrap_or_default();
        return publish_project_overview(&root, policy.limits, params, markers, |_| {
            ControlFlow::Continue(())
        });
    }
    let (file, _) = overview_output(&root, params)?;
    let settings = overview_settings(params)?;
    let (source, limits) = read_source(world, params)?;
    let mut baked = bake_overview_registered(&source, &settings, |_| ControlFlow::Continue(()))
        .map_err(|e| coded("height_io_decode", e.to_string()))?;
    let placed = draw_markers(&mut baked.image, &baked.mapping, &markers);
    let png = baked
        .image
        .encode_png()
        .map_err(|e| coded("height_io_write", e.to_string()))?;
    crate::bake_export::write_atomic(&file, &png).map_err(|e| coded("height_io_write", e.to_string()))?;
    let mut reply = json!({ "source": summary(&source, limits), "output": output, "width": settings.size[0],
        "height": settings.size[1], "bytes": png.len(), "north_up": true,
        "missing_pixels": baked.image.missing_pixels(), "missing_rgb": regia_terrain::overview::MISSING_RGB,
        "fit": settings.fit.as_str(), "registered": false });
    if let (Some(reply), Value::Object(layout)) = (
        reply.as_object_mut(),
        overview_layout_json(&baked, &placed, &markers),
    ) {
        reply.extend(layout);
    }
    Ok(reply)
}

/// The `heightfield.json` a published source carries: the schema-2 count mapping
/// (`docs/formats/terrain.md` § 9a) of `m`, with every value as the cook reads it back.
#[must_use]
pub fn mapping_document(m: &regia_terrain::source::CountMapping) -> Value {
    json!({
        "schema": 2,
        "grid": {
            "cols": m.cols, "rows": m.rows, "cell_m": m.cell_m,
            "sample0_x_m": m.sample0_m[0], "sample0_z_m": m.sample0_m[1],
            "row0": "south", "col0": "west",
        },
        "units": { "kind": "counts", "meters_per_count": m.meters_per_count, "zero_counts": m.zero_counts },
    })
}

fn encode_counts_png(cols: u32, rows: u32, counts: &[u16]) -> Result<Vec<u8>, RpcError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, cols, rows);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder
            .write_header()
            .map_err(|e| coded("height_io_write", e.to_string()))?;
        // PNG stores 16-bit samples big-endian, row 0 first (the cook's row 0 is the south edge).
        let data: Vec<u8> = counts.iter().flat_map(|c| c.to_be_bytes()).collect();
        writer
            .write_image_data(&data)
            .map_err(|e| coded("height_io_write", e.to_string()))?;
        writer
            .finish()
            .map_err(|e| coded("height_io_write", e.to_string()))?;
    }
    Ok(bytes)
}

fn decode_counts_png(bytes: &[u8]) -> Option<([u32; 2], Vec<u16>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    if info.color_type != png::ColorType::Grayscale || info.bit_depth != png::BitDepth::Sixteen {
        return None;
    }
    let dims = [info.width, info.height];
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let (pairs, _) = buf.get(..frame.buffer_size())?.as_chunks::<2>();
    let counts = pairs.iter().map(|b| u16::from_be_bytes(*b)).collect();
    Some((dims, counts))
}

/// Publishes the count source `params` names as a terrain's source asset (ADR-8541; `regia terrain
/// source-import`): `dest` is the project-relative `heightfield.png`, written with the source's
/// counts verbatim, and its sibling `heightfield.json` gets the source's schema-2 count mapping, so
/// the cook keeps counts, datum, scale and registration exactly (ADR-8471). Each file is written
/// atomically (temporary file and rename), the PNG first and the mapping last.
///
/// The destination's existing mapping is compared first: a different grid is refused
/// (`terrain_source_grid_mismatch`, never resampled); a different mapping, or a legacy extents
/// file the mapping would replace, is refused (`terrain_source_mapping_mismatch`) unless
/// `adopt_source` is set. `dry_run` answers the preview and writes nothing.
///
/// # Errors
///
/// The reader's refusals, the two above, `height_io_write`.
pub fn publish_source_asset(
    root: &Path,
    policy: ImportLimits,
    params: &Value,
    dest: &str,
    adopt_source: bool,
    dry_run: bool,
) -> Result<Value, RpcError> {
    let _span = tracing::info_span!(target: "regia_agent", "terrain_source.publish").entered();
    let read = read_project_source(root, policy, params)?;
    let png_path = resolve_project_path(root, dest)?;
    if png_path
        .extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| !e.eq_ignore_ascii_case("png"))
    {
        return Err(coded(
            "height_io_bad_format",
            "the destination must be a heightfield .png",
        ));
    }
    let json_path = png_path.with_file_name("heightfield.json");
    // A raster source with unequal spacing or missing samples has no count mapping (ADR-8542):
    // publishing it would write a mapping that does not describe it.
    let mapping = read
        .source
        .count_mapping()
        .map_err(|e| coded(e.code(), e.to_string()))?;
    let [cols, rows] = read.source.dimensions();
    let existing = match std::fs::read(&png_path) {
        Ok(bytes) => Some(decode_counts_png(&bytes).ok_or_else(|| {
            coded(
                "height_io_decode",
                "the destination is not a 16-bit grayscale PNG",
            )
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(coded("height_io_read", e.to_string())),
    };
    if let Some((dims, _)) = &existing
        && *dims != [cols, rows]
    {
        return Err(coded(
            "terrain_source_grid_mismatch",
            format!(
                "the source is {cols}x{rows} samples and the destination {}x{}; an import never resamples",
                dims[0], dims[1]
            ),
        ));
    }
    let dest_mapping = match std::fs::read(&json_path) {
        Ok(bytes) => Some(
            regia_terrain::source::CountMapping::from_heightfield_json(&bytes)
                .map_err(|e| coded("height_io_decode", e.to_string()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(coded("height_io_read", e.to_string())),
    };
    let mapping_match = match &dest_mapping {
        None => true,
        Some(Some(m)) => m.canonical_bytes() == mapping.canonical_bytes(),
        Some(None) => false,
    };
    let changed = existing.as_ref().map_or(read.source.counts().len(), |(_, old)| {
        old.iter()
            .zip(read.source.counts())
            .filter(|(a, b)| a != b)
            .count()
    });
    let preview = json!({
        "source": summary(&read.source, read.limits),
        "digest": read.digest,
        "destination": {
            "heightfield": dest,
            "exists": existing.is_some(),
            "mapping": match &dest_mapping {
                None => Value::Null,
                Some(None) => json!("legacy"),
                Some(Some(m)) => mapping_json(m),
            },
        },
        "mapping_match": mapping_match,
        "changed": changed,
        "total": read.source.counts().len(),
    });
    if !mapping_match && !adopt_source {
        let mut error = coded(
            "terrain_source_mapping_mismatch",
            "the destination's heightfield.json maps counts differently; choose adopt-source to replace it",
        );
        if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
            data.insert("preview".into(), preview);
        }
        return Err(error);
    }
    if dry_run {
        return Ok(json!({ "dry_run": true, "written": [], "preview": preview }));
    }
    let png = encode_counts_png(cols, rows, read.source.counts())?;
    let doc = serde_json::to_vec_pretty(&mapping_document(&mapping))
        .map_err(|e| coded("height_io_write", e.to_string()))?;
    if let Some(dir) = png_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| coded("height_io_write", e.to_string()))?;
    }
    crate::bake_export::write_atomic(&png_path, &png).map_err(|e| coded("height_io_write", e.to_string()))?;
    crate::bake_export::write_atomic(&json_path, &doc)
        .map_err(|e| coded("height_io_write", e.to_string()))?;
    let json_rel = Path::new(dest)
        .with_file_name("heightfield.json")
        .to_string_lossy()
        .replace('\\', "/");
    Ok(json!({ "dry_run": false, "written": [dest, json_rel], "preview": preview }))
}
