// SPDX-License-Identifier: MPL-2.0
//! The terrain a level names (scene format v12, `regia_scene::level::LevelTerrain`, ADR-2411).
//!
//! Before v12 a level could not say which terrain it stood on: the host built the [`Terrain`],
//! and the side files were found by the level's own name (`<level>.holes.ron`,
//! `<level>.weights.bin`, `<level>.scatterpaint.bin`). A v12 level names its terrain:
//!
//! - `layers`: the `.terrain.layers.ron` source; its `.meta` id is the *map id*, and the cooked
//!   tiled heightfield is `terrain/heightfield` under it (`docs/formats/terrain.md` § 9);
//! - `map`: the name the side files take (empty: the level's own path, the pre-v12 rule);
//! - the world mapping (width, height scale and offset).
//!
//! [`LevelTerrainFiles::resolve`] turns that into paths, [`load_level_terrain`] reads them, and
//! [`spawn_level_terrain`] puts the result in a world (one entity tagged [`LevelTerrainSource`],
//! replaced when the level changes). [`LevelTerrainPlugin`] does it on every `LevelSettings`
//! change for hosts that set [`LevelTerrainRoots`]. A migrated v11 level names no terrain, so it
//! resolves to the same side files as before and the host's terrain stays: the same terrain.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_scene::level::{LevelSettings, LevelTerrain};

use crate::grass_density::{GrassDensityPaint, fnv, fnv_start, grass_density_path};
use crate::holes::HolesMask;
use crate::layer_paint::{LayerWeightMap, layer_weights_path};
use crate::layers::TerrainLayers;
use crate::scatter::painted::scatter_paint_path;
use crate::source::CountMapping;
use crate::{CdlodConfig, Terrain, TerrainEdits, TerrainPaint};

/// Largest side file read, bytes (2 GiB, the painted-scatter limit).
const MAX_SIDE_FILE: u64 = 2 << 30;

/// Errors resolving or loading a level's terrain. The `code` names the agent error code.
#[derive(Debug, thiserror::Error)]
pub enum LevelTerrainError {
    /// The level's `terrain` block is invalid (a path leaves `assets/`, a bad number).
    #[error("level terrain: {0}")]
    Invalid(String),
    /// A named file is missing or cannot be read.
    #[error("{path}: {reason}")]
    Read {
        /// The file.
        path: PathBuf,
        /// What failed.
        reason: String,
    },
    /// The cooked count mapping cannot be loaded exactly into the runtime map, or the level
    /// block contradicts it (ADR-8471). Nothing is resampled.
    #[error("level terrain mapping: {0}")]
    Mapping(String),
    /// A named file does not parse.
    #[error("{path}: {reason}")]
    Decode {
        /// The file.
        path: PathBuf,
        /// What failed.
        reason: String,
    },
}

impl LevelTerrainError {
    /// The agent error code (`book/agents/level-terrain.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "level_terrain_invalid",
            Self::Read { .. } => "level_terrain_read",
            Self::Decode { .. } => "level_terrain_decode",
            Self::Mapping(_) => "level_terrain_mapping",
        }
    }
}

/// The files of a level's terrain, absolute (under the assets root).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelTerrainFiles {
    /// The `.terrain.layers.ron` (`None`: the level names no heightfield).
    pub layers: Option<PathBuf>,
    /// The map the side files are named after.
    pub map: PathBuf,
    /// `<map>.holes.ron`.
    pub holes: PathBuf,
    /// `<map>.weights.bin`.
    pub weights: PathBuf,
    /// `<map>.scatterpaint.bin`.
    pub scatter_paint: PathBuf,
    /// `<map>.grassdensity.bin`.
    pub grass_density: PathBuf,
    /// `<map>.meshlayers`: the cooked mesh layers (spec 26 § 2a.10). Missing: no layers.
    pub meshlayers: PathBuf,
}

fn check_relative(path: &str, what: &str) -> Result<(), LevelTerrainError> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.split('/').any(|p| p == ".." || p == "." || p.is_empty());
    if bad {
        return Err(LevelTerrainError::Invalid(format!(
            "{what} `{path}` must be relative to assets/ without `..`"
        )));
    }
    Ok(())
}

/// A symlink under `assets/` may point outside it: the deepest part of `path` that exists must
/// resolve under the resolved `assets/` (when `assets/` exists at all).
fn check_inside(assets: &Path, path: &Path, what: &str) -> Result<(), LevelTerrainError> {
    if let Ok(root) = assets.canonicalize()
        && let Some(real) = path.ancestors().find_map(|p| p.canonicalize().ok())
        && !real.starts_with(&root)
    {
        return Err(LevelTerrainError::Invalid(format!(
            "{what} `{}` resolves outside assets/",
            path.display()
        )));
    }
    Ok(())
}

impl LevelTerrainFiles {
    /// Resolves the files of `terrain` for the level at `level` (relative to `assets`,
    /// `maps/arena.level.ron`). `None` (a level that names no terrain, every migrated v11
    /// level) gives the side files under the level's own name and no layer file.
    ///
    /// # Errors
    ///
    /// [`LevelTerrainError::Invalid`] when the level path or the block is invalid.
    pub fn resolve(
        assets: &Path,
        level: &str,
        terrain: Option<&LevelTerrain>,
    ) -> Result<Self, LevelTerrainError> {
        check_relative(level, "level")?;
        if let Some(t) = terrain {
            t.validate().map_err(LevelTerrainError::Invalid)?;
        }
        let map_rel = terrain.map_or_else(|| level.to_owned(), |t| t.map_for(level));
        let map = assets.join(&map_rel);
        let layers = terrain
            .filter(|t| !t.layers.is_empty())
            .map(|t| assets.join(&t.layers));
        check_inside(assets, &map, "map")?;
        if let Some(l) = &layers {
            check_inside(assets, l, "layers")?;
        }
        Ok(Self {
            layers,
            holes: crate::holes_ron_path(&map),
            weights: layer_weights_path(&map),
            scatter_paint: scatter_paint_path(&map),
            grass_density: grass_density_path(&map),
            meshlayers: meshlayers_cooked_path(&map),
            map,
        })
    }
}

/// What [`load_level_terrain`] read.
#[derive(Debug, Clone)]
pub struct LevelTerrainLoad {
    /// The files.
    pub files: LevelTerrainFiles,
    /// The terrain built from the cooked heightfield (`None` without a layer file).
    pub terrain: Option<Terrain>,
    /// The layer file's map id (`None` without a layer file).
    pub map_id: Option<regia_asset::AssetId>,
    /// The cooked heightfield file read.
    pub cooked_heightfield: Option<PathBuf>,
    /// The holes file's mask, when the file exists.
    pub holes: Option<HolesMask>,
    /// The painted layer weights, when the file exists.
    pub paint: Option<LayerWeightMap>,
    /// The painted grass density, when the file exists.
    pub grass_density: Option<GrassDensityPaint>,
    /// The cooked count mapping the terrain was built from (`None`: the level's scale, offset
    /// and width, ADR-8471).
    pub source_mapping: Option<CountMapping>,
    /// The cooked mesh layers, when `<map>.meshlayers` exists and loads. A file that does not load
    /// is logged and gives no layers (the map draws as without the file).
    pub mesh_layers: Option<crate::mesh_layer::TerrainMeshLayers>,
}

impl LevelTerrainLoad {
    /// FNV-1a 64 over everything loaded: the heightfield samples and mapping, the holes (as their
    /// canonical RON), the painted weights and the grass density. Two loads of the same terrain
    /// give the same digest on every machine.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut h = fnv_start();
        match &self.terrain {
            None => h = fnv(h, b"no-terrain"),
            Some(t) => {
                let hf = t.map.to_heightfield();
                h = fnv(h, &hf.width.to_le_bytes());
                h = fnv(h, &hf.height.to_le_bytes());
                for s in &hf.samples {
                    h = fnv(h, &s.to_le_bytes());
                }
                for v in [t.map.world_width(), t.map.height_scale(), t.map.height_offset()] {
                    h = fnv(h, &v.to_bits().to_le_bytes());
                }
                h = fnv(h, &t.layers.count().to_le_bytes());
                // A count source's mapping and grid origin (ADR-8471); a legacy load hashes as before.
                if let Some(m) = &self.source_mapping {
                    h = fnv(h, b"count-mapping");
                    h = fnv(h, &m.canonical_bytes());
                    for v in t.map.grid_origin() {
                        h = fnv(h, &v.to_bits().to_le_bytes());
                    }
                }
            }
        }
        let holes = self.holes.as_ref().and_then(|m| m.to_holes_ron().ok());
        h = fnv(h, holes.as_deref().unwrap_or("no-holes").as_bytes());
        let paint = self.paint.as_ref().and_then(|p| p.to_bytes().ok());
        h = fnv(h, paint.as_deref().unwrap_or(b"no-paint"));
        match &self.grass_density {
            None => fnv(h, b"no-density"),
            Some(d) => fnv(h, &d.digest().to_le_bytes()),
        }
    }
}

fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, LevelTerrainError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(LevelTerrainError::Read {
                path: path.to_path_buf(),
                reason: e.to_string(),
            });
        }
    };
    if meta.len() > MAX_SIDE_FILE {
        return Err(LevelTerrainError::Decode {
            path: path.to_path_buf(),
            reason: format!("{} bytes is over the {MAX_SIDE_FILE} byte limit", meta.len()),
        });
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|e| LevelTerrainError::Read {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
}

fn required(path: &Path) -> Result<Vec<u8>, LevelTerrainError> {
    read_capped(path)?.ok_or_else(|| LevelTerrainError::Read {
        path: path.to_path_buf(),
        reason: "not found".into(),
    })
}

fn decode_err(path: &Path, e: impl std::fmt::Display) -> LevelTerrainError {
    LevelTerrainError::Decode {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

/// The quadtree depth that draws every sample at the finest level (the shared draw's rule): the
/// root a level's terrain is built with, and the root the mesh-layer cook cuts chunks on.
#[must_use]
pub fn root_level_for(width: u32, height: u32, config: &CdlodConfig) -> u32 {
    let cells = (width.max(height).saturating_sub(1)) as f32 / config.grid_size.max(1) as f32;
    let mut root = 0u32;
    while root < 12 && ((1u32 << root) as f32) < cells {
        root += 1;
    }
    root
}

/// Builds the terrain of `terrain` from its cooked heightfield in `cooked` and reads the side
/// files (missing side files are `None`; a side file that exists and does not parse is an error,
/// so a save never replaces it). See the module docs.
///
/// # Errors
///
/// [`LevelTerrainError`]: an invalid block, a missing layer file, `.meta` or cooked heightfield,
/// or a file that does not parse.
pub fn load_level_terrain(
    assets: &Path,
    cooked: &Path,
    level: &str,
    terrain: Option<&LevelTerrain>,
) -> Result<LevelTerrainLoad, LevelTerrainError> {
    let _span = tracing::info_span!(target: "regia_terrain", "level_terrain.load", level).entered();
    let files = LevelTerrainFiles::resolve(assets, level, terrain)?;
    let ground = load_level_ground_of(&files, cooked, terrain)?;
    let mut load = LevelTerrainLoad {
        terrain: None,
        map_id: None,
        cooked_heightfield: None,
        holes: None,
        paint: None,
        grass_density: None,
        source_mapping: None,
        mesh_layers: None,
        files,
    };
    if let (Some(ground), Some(block)) = (ground, terrain) {
        let config = CdlodConfig::default();
        let hf = &ground.heightfield;
        let root = root_level_for(hf.width, hf.height, &config);
        let cooked_path = ground.cooked_heightfield;
        let mapping = ground.source_mapping;
        let mut built = match mapping {
            // A count source (ADR-8471): the cooked mapping decides heights and placement.
            Some(mapping) => {
                let map = mapping_map(&mapping, hf, block, &cooked_path)?;
                let minmax =
                    crate::MinMaxPyramid::build(&map, root).map_err(|e| decode_err(&cooked_path, e))?;
                Terrain {
                    map,
                    cdlod: config,
                    root_level: root,
                    minmax,
                    layers: ground.layers,
                    holes: None,
                    water: None,
                }
            }
            None => {
                // ADR-10079: the block's layout decides where the samples sit in its width.
                let map = crate::TerrainMap::new(
                    hf,
                    ground.world_width_m,
                    ground.height_scale_m,
                    ground.height_offset_m,
                )
                .map_err(|e| decode_err(&cooked_path, crate::TerrainError::from(e)))?
                .with_sample_layout(ground.sample_layout);
                config
                    .validate()
                    .map_err(|e| decode_err(&cooked_path, crate::TerrainError::from(e)))?;
                let minmax = crate::MinMaxPyramid::build(&map, root)
                    .map_err(|e| decode_err(&cooked_path, crate::TerrainError::from(e)))?;
                Terrain {
                    map,
                    cdlod: config,
                    root_level: root,
                    minmax,
                    layers: ground.layers,
                    holes: None,
                    water: None,
                }
            }
        };
        load.source_mapping = mapping;
        let map_id = ground.map_id;
        if let Some(mask) =
            crate::load_holes_ron(&load.files.holes).map_err(|e| decode_err(&load.files.holes, e))?
        {
            built.holes = Some(mask);
        }
        load.terrain = Some(built);
        load.map_id = Some(map_id);
        load.cooked_heightfield = Some(cooked_path);
    }
    if load.holes.is_none() {
        load.holes = match &load.terrain {
            Some(t) => t.holes.clone(),
            None => crate::load_holes_ron(&load.files.holes).map_err(|e| decode_err(&load.files.holes, e))?,
        };
    }
    if let Some(bytes) = read_capped(&load.files.weights)? {
        let paint = LayerWeightMap::from_bytes(&bytes).map_err(|e| decode_err(&load.files.weights, e))?;
        if let Some(t) = &load.terrain
            && (paint.width() != t.map.width() || paint.height() != t.map.height())
        {
            return Err(decode_err(
                &load.files.weights,
                format!(
                    "{}x{} weights for a {}x{} heightfield",
                    paint.width(),
                    paint.height(),
                    t.map.width(),
                    t.map.height()
                ),
            ));
        }
        load.paint = Some(paint);
    }
    load.mesh_layers = load_mesh_layers(&load.files.meshlayers, cooked);
    if let Some(bytes) = read_capped(&load.files.grass_density)? {
        load.grass_density = Some(
            GrassDensityPaint::from_bytes(&bytes).map_err(|e| decode_err(&load.files.grass_density, e))?,
        );
    }
    Ok(load)
}

/// The heightfield and height mapping a level's terrain block names, read from the cooked cache:
/// what [`load_level_terrain`] builds the run-time [`Terrain`] from, and what the mesh-layer cook
/// of `regia cook` builds its sim ground from (ADR-8451), so the two cannot disagree.
#[derive(Clone, Debug)]
pub struct LevelGround {
    /// The map id: the layer file's `.meta` id.
    pub map_id: regia_asset::AssetId,
    /// The layer stack of the layer file.
    pub layers: TerrainLayers,
    /// The stitched cooked heightfield (16-bit samples).
    pub heightfield: regia_asset::Heightfield,
    /// The cooked heightfield file read.
    pub cooked_heightfield: PathBuf,
    /// World width, metres: the block's, else the cooked index's `size_m`, else one metre a sample.
    /// For a cell-centered block (ADR-10079) it is the cells' extent (the index's `size_m` widened
    /// by `n / (n − 1)`, else one metre a cell); [`Self::sample_span_m`] is the span between the
    /// outer samples.
    pub world_width_m: f32,
    /// Where the samples sit in that width (the block's `sample_layout`, ADR-10079).
    pub sample_layout: regia_scene::level::TerrainSampleLayout,
    /// Metres from sample 0 to sample 65535 (the block's `height_scale_m`).
    pub height_scale_m: f32,
    /// Height of sample 0, metres (the block's `height_offset_m`).
    pub height_offset_m: f32,
    /// The cooked index's count mapping (ADR-8471): when present it, not the width, scale and offset
    /// above, decides the run-time heights ([`load_level_terrain`] builds from it).
    pub source_mapping: Option<CountMapping>,
}

impl LevelGround {
    /// The run-time map of a count-mapped ground, built exactly as [`load_level_terrain`] builds it
    /// (the same refusals, `level_terrain_mapping`, for a `block` whose `world_width_m`
    /// contradicts the mapping or a grid the map cannot hold); `None` without a mapping. The
    /// mesh-layer cook builds its sim ground from it (ADR-8842), so the sim ground and the drawn
    /// terrain take one width, one height formula and one placement.
    ///
    /// # Errors
    ///
    /// [`LevelTerrainError::Mapping`], or [`LevelTerrainError::Decode`] when the heightfield is
    /// not the mapping's grid.
    pub fn mapped_map(&self, block: &LevelTerrain) -> Result<Option<crate::TerrainMap>, LevelTerrainError> {
        self.source_mapping
            .as_ref()
            .map(|m| mapping_map(m, &self.heightfield, block, &self.cooked_heightfield))
            .transpose()
    }
}

impl LevelGround {
    /// The distance from the first to the last sample along X, metres: [`Self::world_width_m`] for
    /// a vertex-centered block, `world_width_m · (n − 1) / n` for a cell-centered one (ADR-10079).
    /// What a vertex-centered consumer (the mesh-layer cook's sim ground) takes as its width, so
    /// it places every sample where the run-time map does.
    #[must_use]
    pub fn sample_span_m(&self) -> f32 {
        self.sample_layout
            .sample_span(self.heightfield.width, self.world_width_m)
    }
}

/// Reads the [`LevelGround`] of `terrain` for the level at `level`. `None` when the level names no
/// heightfield (no block, or an empty `layers`).
///
/// # Errors
///
/// [`LevelTerrainError`]: an invalid block, or a missing or bad layer file, `.meta` or cooked
/// heightfield.
pub fn load_level_ground(
    assets: &Path,
    cooked: &Path,
    level: &str,
    terrain: Option<&LevelTerrain>,
) -> Result<Option<LevelGround>, LevelTerrainError> {
    let files = LevelTerrainFiles::resolve(assets, level, terrain)?;
    load_level_ground_of(&files, cooked, terrain)
}

fn load_level_ground_of(
    files: &LevelTerrainFiles,
    cooked: &Path,
    terrain: Option<&LevelTerrain>,
) -> Result<Option<LevelGround>, LevelTerrainError> {
    let (Some(layer_path), Some(block)) = (files.layers.as_ref(), terrain) else {
        return Ok(None);
    };
    let text = required(layer_path)?;
    let text = String::from_utf8(text).map_err(|e| decode_err(layer_path, e))?;
    let layers = TerrainLayers::from_ron(&text).map_err(|e| decode_err(layer_path, e))?;
    let meta_path = regia_asset::meta_path_for(layer_path);
    let meta =
        regia_asset::MetaSidecar::from_ron(&required(&meta_path)?).map_err(|e| decode_err(&meta_path, e))?;
    let map_id = meta.id;
    let id = crate::stream::heightfield_asset_id(map_id);
    let cooked_path = cooked.join(regia_asset::cooked_file_name(id));
    let bytes = required(&cooked_path)?;
    let tiled =
        crate::stream::decode_cooked_tiled_heightfield(&bytes).map_err(|e| decode_err(&cooked_path, e))?;
    let hf = crate::stream::stitch_tiled_heightfield(&tiled).map_err(|e| decode_err(&cooked_path, e))?;
    let index = read_index(cooked, map_id)?;
    // A version-3 index can hold a Z spacing other than X and a valid-data mask (ADR-8871); the
    // run-time map is built from `source_mapping` alone, so such an index is refused here rather
    // than read with X's spacing on Z or a missing sample's count as a height.
    if let Some(why) = index
        .as_ref()
        .and_then(crate::stream::TerrainIndex::source_grid_unsupported)
    {
        return Err(LevelTerrainError::Mapping(why));
    }
    // ADR-10079: a cell-centered block's width is its cells' extent. The cooked index's `size_m`
    // is the span between the outer samples (what every cook writes), so it is widened by
    // `n / (n − 1)`; with neither, a cell-centered map is one metre a cell.
    let world_width_m = match (block.world_width_m, block.sample_layout) {
        (Some(w), _) => w,
        (None, regia_scene::level::TerrainSampleLayout::VertexCentered) => {
            index_world_width(index.as_ref()).unwrap_or((hf.width.max(hf.height) - 1) as f32)
        }
        (None, regia_scene::level::TerrainSampleLayout::CellCentered) => {
            let n = f64::from(hf.width.max(2));
            index_world_width(index.as_ref()).map_or(hf.width.max(hf.height) as f32, |span| {
                (f64::from(span) * n / (n - 1.0)) as f32
            })
        }
    };
    let source_mapping = index.as_ref().and_then(|i| i.source_mapping);
    Ok(Some(LevelGround {
        map_id,
        layers,
        heightfield: hf,
        cooked_heightfield: cooked_path,
        world_width_m,
        sample_layout: block.sample_layout,
        height_scale_m: block.height_scale_m,
        height_offset_m: block.height_offset_m,
        source_mapping,
    }))
}

/// The mesh-layer source of `map`: `<stem>.meshlayers.ron` next to it, the stem as
/// [`crate::holes_ron_path`] takes it (spec 26 § 2a).
#[must_use]
pub fn meshlayers_source_path(map: &Path) -> PathBuf {
    let mut name = meshlayers_cooked_path(map).into_os_string();
    name.push(".ron");
    PathBuf::from(name)
}

/// The cooked mesh layers of `map`: `<stem>.meshlayers` next to it, the stem as
/// [`crate::holes_ron_path`] takes it (spec 26 § 2a.10).
#[must_use]
pub fn meshlayers_cooked_path(map: &Path) -> PathBuf {
    let holes = crate::holes_ron_path(map);
    let name = holes.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stem = name.strip_suffix(".holes.ron").unwrap_or(name);
    map.with_file_name(format!("{stem}.meshlayers"))
}

/// `WorldGeom::from_cooked` with the count-body check (cooked format v2, ADR-8816) on a scoped
/// worker beside the load instead of inside it: the same geometry and the same refusals, in the
/// time of the longer of the two (on the Android test device the check is 20 ms of a 50 ms load of
/// a 2.56 km map). The check is a pure function of the bytes, so the result does not depend on the
/// split. When no worker can be spawned the load checks inline.
fn load_geom_checked(bytes: &[u8]) -> Result<regia_world_geom::WorldGeom, regia_world_geom::GeomLoadError> {
    use regia_world_geom::cooked::{CountBodyCheck, check_count_body, split_file_ref};
    let canonical = split_file_ref(bytes)?.canonical;
    std::thread::scope(|s| {
        let worker = std::thread::Builder::new()
            .name("regia-meshlayers-check".into())
            .spawn_scoped(s, || check_count_body(canonical));
        let Ok(worker) = worker else {
            return regia_world_geom::WorldGeom::from_cooked(bytes);
        };
        let geom = regia_world_geom::WorldGeom::from_cooked_with(bytes, CountBodyCheck::Deferred);
        let checked = worker.join().unwrap_or_else(|_| {
            Err(regia_world_geom::GeomLoadError::Corrupt(
                "the count-body check stopped".into(),
            ))
        });
        // The load's own refusal first (it comes first in the checked load too), then the body's.
        let geom = geom?;
        checked?;
        Ok(geom)
    })
}

/// Reads `<map>.meshlayers` (spec 26 § 2a.5): the canonical section into a [`regia_world_geom::WorldGeom`]
/// and the presentation section into the layer chunks, whose LOD meshes stream from `cooked`.
/// A missing file is `None` with no log. A file that does not load is `None` and an error log
/// with its code (`geom_*` or `meshlayers_presentation_*`): the map draws as if it had none.
#[must_use]
pub fn load_mesh_layers(path: &Path, cooked: &Path) -> Option<crate::mesh_layer::TerrainMeshLayers> {
    let bytes = match read_capped(path) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(target: "regia_terrain", path = %path.display(), code = e.code(), error = %e, "mesh layers not loaded");
            return None;
        }
    };
    let geom = match load_geom_checked(&bytes) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(target: "regia_terrain", path = %path.display(), code = e.code(), error = %e, "mesh layers not loaded");
            return None;
        }
    };
    // Borrowed: the canonical section (tens of MB on a 2.56 km map) is not copied to reach the chunks (ADR-8816).
    let presentation = regia_world_geom::cooked::split_file_ref(&bytes)
        .map_err(|e| e.code())
        .and_then(|parts| {
            crate::mesh_layer::TerrainMeshLayersCooked::decode(parts.chunks).map_err(|e| e.code())
        });
    let layers = crate::mesh_layer::TerrainMeshLayers::with_geom(std::sync::Arc::new(geom));
    match presentation {
        Ok(Some(p)) => Some(layers.with_presentation(p, Some(cooked.to_path_buf()))),
        Ok(None) => Some(layers),
        Err(code) => {
            tracing::error!(target: "regia_terrain", path = %path.display(), code, "mesh layers not loaded: the chunk section does not decode (cook again)");
            None
        }
    }
}

/// The world width the cooked terrain index records (`heightfield.json` `size_m`), if any.
fn index_world_width(index: Option<&crate::stream::TerrainIndex>) -> Option<f32> {
    index?.world_size_m.filter(|w| w.is_finite() && *w > 0.0)
}

/// The cooked terrain index of `map_id`. A missing or unreadable index is `None` (the level's own
/// numbers apply, as before); an index that does not decode is `None` too unless its bytes name a
/// version this build cannot read, which is an error: a newer index may carry a count mapping, and
/// loading its counts under the level's scale would give other heights (ADR-8471).
fn read_index(
    cooked: &Path,
    map_id: regia_asset::AssetId,
) -> Result<Option<crate::stream::TerrainIndex>, LevelTerrainError> {
    let id = crate::stream::terrain_index_asset_id(map_id);
    let path = cooked.join(regia_asset::cooked_file_name(id));
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    match crate::stream::load_terrain_index_bytes(&bytes) {
        Ok(index) => Ok(Some(index)),
        Err(e @ crate::stream::TerrainIndexError::BadVersion { .. }) => Err(decode_err(&path, e)),
        Err(_) => Ok(None),
    }
}

/// The runtime map of a count-mapped terrain ([`CountMapping::terrain_map`]). A level block that
/// names its own world width must name the mapping's (`(cols - 1) * cell_m`); the level's height
/// scale and offset do not apply to a count source.
fn mapping_map(
    mapping: &CountMapping,
    hf: &regia_asset::Heightfield,
    block: &LevelTerrain,
    cooked_path: &Path,
) -> Result<crate::TerrainMap, LevelTerrainError> {
    if block.sample_layout != regia_scene::level::TerrainSampleLayout::VertexCentered {
        return Err(LevelTerrainError::Mapping(format!(
            "the level's sample_layout {} cannot apply to a cooked count mapping: the mapping places \
             the samples; remove sample_layout from the level",
            block.sample_layout.as_str()
        )));
    }
    if let Some(w) = block.world_width_m {
        let derived = (f64::from(mapping.cols.max(mapping.rows) - 1) * f64::from(mapping.cell_m)) as f32;
        if w.to_bits() != derived.to_bits() {
            return Err(LevelTerrainError::Mapping(format!(
                "the level's world_width_m {w} differs from the cooked count mapping's {derived}; \
                 remove world_width_m or set it to the mapping's"
            )));
        }
    }
    mapping.terrain_map(hf).map_err(|e| {
        if e.0.starts_with("terrain_mapping_unrepresentable") {
            LevelTerrainError::Mapping(e.0)
        } else {
            decode_err(cooked_path, e)
        }
    })
}

/// Tags the terrain entity [`spawn_level_terrain`] made, with what it was made from.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct LevelTerrainSource {
    /// The level path (relative to `assets/`).
    pub level: String,
    /// The block it was built from (`None`: the level names no terrain).
    pub terrain: Option<LevelTerrain>,
    /// The files.
    pub files: LevelTerrainFiles,
    /// The map id of the layer file.
    pub map_id: Option<regia_asset::AssetId>,
    /// The cooked heightfield file.
    pub cooked_heightfield: Option<PathBuf>,
    /// [`LevelTerrainLoad::digest`] at load.
    pub digest: u64,
    /// The cooked count mapping the terrain was built from ([`LevelTerrainLoad::source_mapping`]).
    pub source_mapping: Option<CountMapping>,
}

/// Where [`LevelTerrainPlugin`] finds a level's files: the project's `assets/`, the cooked cache
/// and the open level's path relative to `assets/`.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct LevelTerrainRoots {
    /// The project's `assets/` directory.
    pub assets: PathBuf,
    /// The cooked cache (`.regia/cooked` in a project, the package's cooked directory at run time).
    pub cooked: PathBuf,
    /// The open level, relative to `assets/` (`maps/arena.level.ron`).
    pub level: String,
}

/// What [`spawn_level_terrain`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelTerrainSpawn {
    /// The terrain entity (`None`: the level names no heightfield).
    pub entity: Option<Entity>,
    /// Entities of an earlier level's terrain it removed.
    pub replaced: usize,
    /// [`LevelTerrainLoad::digest`].
    pub digest: u64,
}

/// Loads the terrain `settings` names for `level` and spawns it, replacing every earlier
/// [`LevelTerrainSource`] entity. The entity carries the [`Terrain`] (holes applied),
/// [`TerrainEdits`], [`TerrainPaint`] when a weights file exists and [`GrassDensityPaint`] when a
/// density file exists. A level that names no heightfield spawns nothing (the host's terrain
/// stays) and removes nothing.
///
/// # Errors
///
/// [`LevelTerrainError`] from [`load_level_terrain`]; the world is unchanged then.
pub fn spawn_level_terrain(
    world: &mut World,
    roots: &LevelTerrainRoots,
    settings: &LevelSettings,
) -> Result<LevelTerrainSpawn, LevelTerrainError> {
    let load = load_level_terrain(
        &roots.assets,
        &roots.cooked,
        &roots.level,
        settings.terrain.as_ref(),
    )?;
    let digest = load.digest();
    let Some(terrain) = load.terrain else {
        // Nothing to build: only an earlier level's terrain goes (it is not this level's ground,
        // and its side files must not stand in for this level's).
        let replaced = despawn_other_levels(world, &roots.level);
        return Ok(LevelTerrainSpawn {
            entity: None,
            replaced,
            digest,
        });
    };
    let old: Vec<Entity> = world
        .query_filtered::<Entity, With<LevelTerrainSource>>()
        .iter(world)
        .collect();
    for e in &old {
        world.despawn(*e);
    }
    let source = LevelTerrainSource {
        level: roots.level.clone(),
        terrain: settings.terrain.clone(),
        files: load.files,
        map_id: load.map_id,
        cooked_heightfield: load.cooked_heightfield,
        digest,
        source_mapping: load.source_mapping,
    };
    let mut entity = world.spawn((terrain, TerrainEdits::default(), source));
    if let Some(paint) = load.paint {
        entity.insert(TerrainPaint(paint));
    }
    if let Some(density) = load.grass_density {
        entity.insert(density);
    }
    if let Some(layers) = load.mesh_layers {
        entity.insert(layers);
    }
    Ok(LevelTerrainSpawn {
        entity: Some(entity.id()),
        replaced: old.len(),
        digest,
    })
}

/// Despawns the [`LevelTerrainSource`] entities built for another level than `level`; returns how
/// many. Hosts call it when they open a level that names no heightfield.
pub fn despawn_other_levels(world: &mut World, level: &str) -> usize {
    let old: Vec<Entity> = world
        .query::<(Entity, &LevelTerrainSource)>()
        .iter(world)
        .filter(|(_, s)| s.level != level)
        .map(|(e, _)| e)
        .collect();
    for e in &old {
        world.despawn(*e);
    }
    old.len()
}

/// The last load problem of [`LevelTerrainPlugin`] (`None` after a good load).
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelTerrainStatus {
    /// The level the status is for.
    pub level: String,
    /// The error code and message of the last failed load.
    pub error: Option<(String, String)>,
    /// The digest of the last good load.
    pub digest: Option<u64>,
}

/// Loads the open level's terrain when `LevelSettings` or [`LevelTerrainRoots`] change (an
/// `Update` system; nothing runs while both are unchanged, and nothing at all without the roots).
/// Opt-in: hosts that build their own terrain do not add it.
pub struct LevelTerrainPlugin;

impl Plugin for LevelTerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LevelTerrainStatus>()
            .add_systems(Update, load_level_terrain_on_change);
    }
}

fn load_level_terrain_on_change(world: &mut World) {
    let changed =
        world.is_resource_changed::<LevelSettings>() || world.is_resource_changed::<LevelTerrainRoots>();
    if !changed {
        return;
    }
    let (Some(roots), Some(settings)) = (
        world.get_resource::<LevelTerrainRoots>().cloned(),
        world.get_resource::<LevelSettings>().cloned(),
    ) else {
        return;
    };
    // The same level naming the same block: nothing to do (a settings edit elsewhere).
    let same = world
        .query::<&LevelTerrainSource>()
        .iter(world)
        .any(|s| s.level == roots.level && s.terrain == settings.terrain);
    if same {
        return;
    }
    let status = match spawn_level_terrain(world, &roots, &settings) {
        Ok(spawn) => LevelTerrainStatus {
            level: roots.level.clone(),
            error: None,
            digest: Some(spawn.digest),
        },
        Err(err) => {
            tracing::warn!(target: "regia_terrain", "level {}: {err}", roots.level);
            LevelTerrainStatus {
                level: roots.level.clone(),
                error: Some((err.code().to_owned(), err.to_string())),
                digest: None,
            }
        }
    };
    world.insert_resource(status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerrainMap;

    /// Spec 26 § 1 (ADR-044): the quadtree is deep enough that a leaf cell is at most one
    /// heightfield texel: `2^root * grid >= samples - 1` and one level shallower would not be
    /// (below the 12-level cap). Up to the 4097² tile size of the spec.
    #[test]
    fn the_root_level_makes_a_leaf_cell_at_most_one_texel() {
        for samples in [2u32, 3, 17, 65, 129, 257, 513, 1025, 2049, 4097] {
            for grid in [8u32, 16, 32, 64] {
                let config = CdlodConfig::with_grid_size(grid);
                let root = root_level_for(samples, samples, &config);
                let spans = u64::from(samples - 1);
                let leaf_cells = (1u64 << root) * u64::from(grid);
                assert!(
                    leaf_cells >= spans,
                    "{samples}² grid {grid}: root {root} leaves cells over one texel"
                );
                if root > 0 {
                    assert!(
                        (1u64 << (root - 1)) * u64::from(grid) < spans,
                        "{samples}² grid {grid}: root {root} is deeper than needed"
                    );
                }
                // Non-square maps follow the longer side.
                assert_eq!(root_level_for(samples, 2, &config), root);
                assert_eq!(root_level_for(2, samples, &config), root);
            }
        }
    }

    /// The level loader and the shared draw use one rule (`root_level_for` is a copy of
    /// `gpu::full_detail_root_level` without a map).
    #[test]
    fn the_level_loader_and_the_shared_draw_pick_the_same_root() {
        for samples in [2u32, 3, 33, 65, 129, 257, 513] {
            let map = TerrainMap::from_heights(
                samples,
                samples,
                vec![0.0; (samples * samples) as usize],
                256.0,
                1.0,
                0.0,
            )
            .expect("map");
            for grid in [8u32, 16, 32, 64] {
                let config = CdlodConfig::with_grid_size(grid);
                assert_eq!(
                    root_level_for(samples, samples, &config),
                    crate::gpu::full_detail_root_level(&map, &config),
                    "{samples}² grid {grid}"
                );
            }
        }
    }
}
