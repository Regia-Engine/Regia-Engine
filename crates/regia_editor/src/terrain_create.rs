// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ New terrain (ADR-7727): create a terrain from nothing (a flat 16-bit heightfield) or
//! from a 16-bit PNG/RAW heightmap, cook it, name it in the open level and load it.
//!
//! What a create writes follows the cook convention of `docs/formats/terrain.md` § 9 exactly (no
//! format change): for a terrain `hills` in the folder `assets/maps` it makes the folder
//! `assets/maps/hills/` holding
//!
//! - `hills.terrain.layers.ron` (the layer stack, format v5) and its `.meta` (the map id);
//! - `heightfield.png`, the 16-bit grayscale heightfield (§ 11: sample `s` is the height
//!   `min + s · (max − min) / 65535`, row 0 at −Z);
//! - `heightfield.json`, the world extents (`size_m`);
//! - `detail/<layer>/{albedo,normal,orm,height}.png` copied from the project's starter content
//!   (`assets/starter/terrain/<layer>/`) when it has them.
//!
//! The work runs as the cancellable editor job `terrain_create:<n>` ([`crate::editor_jobs`],
//! ADR-7664): the files are written into a staging folder under the project's state directory,
//! cooked from there into the cooked cache, and only then moved into `assets/` in one rename. A
//! cancel before that rename (at any of [`Phase`]'s checks) removes the staging folder and every
//! cooked file of the new map id, so a cancelled create leaves no partial output; a cancel that
//! arrives after the rename is too late and the job ends `done`.
//!
//! When the job ends, the open level's `LevelSettings::terrain` names the new terrain as one undo
//! step ([`TerrainCreateCommand`]) and the terrain is built at once
//! ([`crate::terrain_panel::load_open_level_terrain`]). Undo removes the level's reference and the
//! built terrain; the files stay (delete them in the Content Browser).
//!
//! The agent route is `editor.terrain_create` (an editor method, not `terrain.*`: the cook lives
//! in the editor-only asset pipeline); `editor.cancel_job { id }` cancels it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_asset::{AssetId, Heightfield};
use regia_scene::level::{LevelTerrain, TerrainSampleLayout};
use regia_terrain::layers::{LayerDesc, MAX_LAYERS, TERRAIN_LAYERS_FORMAT_VERSION, TerrainLayers};
use regia_terrain::{LevelTerrainSource, RawEndian};
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::terrain_panel::{HeightmapFormat, MAX_HEIGHTMAP_FILE};

/// Samples per side a new terrain may have: `2^n + 1` from 129 (128 cells) to 4097 (4096 cells,
/// the largest tile `docs/specs/26-terrain-vfx-highend.md` allows), so every CDLOD level and
/// height mip keeps the last sample.
pub const ALLOWED_SIZES: [u32; 6] = [129, 257, 513, 1025, 2049, 4097];

/// Metres per cell a new terrain may have.
pub const ALLOWED_CELL_SIZES: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];

/// The layers a new terrain gets when the project has the starter content's terrain layers
/// (`assets/starter/terrain/<name>/`), bottom first.
pub const STARTER_LAYERS: [&str; 3] = ["grass", "rock", "dirt"];

/// The layer a new terrain gets without starter content.
pub const DEFAULT_LAYER: &str = "ground";

/// Longest terrain or layer name.
pub const MAX_NAME_LEN: usize = 64;

/// The per-layer texture files the cook convention reads under `detail/<layer>/`.
const DETAIL_FILES: [&str; 4] = ["albedo.png", "normal.png", "orm.png", "height.png"];

/// The job kind in the agent job registry (`terrain_create:<n>`).
pub const JOB_KIND: &str = "terrain_create";

/// The background-task row id.
const TASK: &str = "terrain_create";

/// Where a new terrain's heights come from.
#[derive(Debug, Clone, PartialEq)]
pub enum HeightSource {
    /// A flat heightfield at the height nearest 0 m inside the height range.
    Flat,
    /// A 16-bit heightmap file (project-relative `.png`, `.r16` or `.raw`), its samples kept as
    /// they are ("Import as new"). A RAW file is a square from its byte count.
    Heightmap {
        /// Project-relative path.
        path: String,
        /// Byte order of a RAW file (a PNG is always big-endian).
        endian: RawEndian,
    },
}

/// What a new terrain should be: the dialog's fields and `editor.terrain_create`'s parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainCreateSpec {
    /// The terrain's name: its folder and file stem (ASCII letters, digits, `_` and `-`).
    pub name: String,
    /// The project-relative folder the terrain's folder goes in (`assets/maps`; a leading
    /// `assets/` is optional).
    pub folder: String,
    /// Samples per side ([`ALLOWED_SIZES`]); `None` with a heightmap takes the file's size.
    pub size: Option<u32>,
    /// Metres per cell ([`ALLOWED_CELL_SIZES`]).
    pub cell_size: f32,
    /// The heights of sample 0 and sample 65535, metres (`min < max`).
    pub height_range: [f32; 2],
    /// Layer names, bottom first (`None`: [`default_layers`]).
    pub layers: Option<Vec<String>>,
    /// Where the heights come from.
    pub source: HeightSource,
    /// Where the samples sit (ADR-10079): `VertexCentered` (the default) spans `(size − 1) ·
    /// cell_size` edge to edge; `CellCentered` makes `size` cells of `cell_size` and puts each
    /// sample at its cell's center, for a heightmap whose pixels are areas. The level's block
    /// gets the layout and the cells' extent `size · cell_size` as its width.
    pub sample_layout: TerrainSampleLayout,
}

impl Default for TerrainCreateSpec {
    fn default() -> Self {
        Self {
            name: "terrain".into(),
            folder: "assets/maps".into(),
            size: Some(513),
            cell_size: 1.0,
            height_range: [0.0, 256.0],
            layers: None,
            source: HeightSource::Flat,
            sample_layout: TerrainSampleLayout::VertexCentered,
        }
    }
}

/// A refused or failed create: an agent error code (`book/agents/terrain-editing.md`) and a
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainCreateError {
    /// `terrain_create_*`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl TerrainCreateError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let code = match self.code {
            "terrain_create_no_project" => NOT_FOUND,
            "terrain_create_failed" => regia_agent::protocol::INTERNAL_ERROR,
            _ => INVALID_PARAMS,
        };
        RpcError::new(
            code,
            self.code,
            self.message.clone(),
            "see book/agents/terrain-editing.md (New terrain)",
        )
    }
}

impl std::fmt::Display for TerrainCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The layers a new terrain gets by default: [`STARTER_LAYERS`] when the project has every one of
/// them under `assets/starter/terrain/`, else one [`DEFAULT_LAYER`].
#[must_use]
pub fn default_layers(root: &Path) -> Vec<String> {
    let starter = root.join("assets/starter/terrain");
    if STARTER_LAYERS.iter().all(|name| starter.join(name).is_dir()) {
        STARTER_LAYERS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        vec![DEFAULT_LAYER.to_owned()]
    }
}

/// The layer description a new terrain's layer `name` gets: the grass, rock and dirt presets
/// (slope bands: grass flat, dirt between, rock on steep faces, triplanar), else
/// `LayerDesc::ground` under that name. The altitude band covers the height range.
#[must_use]
pub fn layer_desc(name: &str, height_range: [f32; 2]) -> LayerDesc {
    let mut layer = LayerDesc::ground();
    layer.name = name.to_owned();
    layer.altitude_min = layer.altitude_min.min(height_range[0] - 1.0);
    layer.altitude_max = layer.altitude_max.max(height_range[1] + 1.0);
    match name {
        "grass" => {
            layer.albedo = [0.18, 0.28, 0.10];
            layer.slope_max = 0.5;
        }
        "dirt" => {
            layer.albedo = [0.28, 0.20, 0.13];
            layer.roughness = 0.9;
            layer.slope_min = 0.35;
            layer.slope_max = 0.9;
        }
        "rock" => {
            layer.albedo = [0.32, 0.30, 0.28];
            layer.roughness = 0.85;
            layer.tiling = 4.0;
            layer.slope_min = 0.7;
            layer.slope_max = 1.6;
            layer.triplanar = true;
        }
        _ => {}
    }
    layer
}

fn check_name(name: &str, what: &str) -> Result<(), TerrainCreateError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && regia_core::project_path::check_relative(name).is_ok();
    if ok {
        Ok(())
    } else {
        Err(TerrainCreateError::new(
            "terrain_create_bad_name",
            format!("{what} {name:?} must be 1..={MAX_NAME_LEN} ASCII letters, digits, `_` or `-`"),
        ))
    }
}

/// The folder under `assets/` (`/`-separated, no `..`, no hidden segment), from a project-relative
/// folder with or without the leading `assets/`.
fn assets_folder(folder: &str) -> Result<String, TerrainCreateError> {
    let bad = |why: String| TerrainCreateError::new("terrain_create_bad_path", why);
    if folder.contains('\\') || folder.starts_with('/') || Path::new(folder).is_absolute() {
        return Err(bad(format!(
            "folder `{folder}` must be relative to the project, `/`-separated"
        )));
    }
    let trimmed = folder.trim_matches('/');
    let rel = if trimmed == "assets" {
        ""
    } else if let Some(rest) = trimmed.strip_prefix("assets/") {
        rest
    } else {
        return Err(bad(format!(
            "folder `{folder}` must be under the project's assets/"
        )));
    };
    if rel.is_empty() {
        return Ok(String::new());
    }
    if rel
        .split('/')
        .any(|s| s.is_empty() || s.starts_with('.') || s == "..")
        || regia_core::project_path::check_relative(rel).is_err()
    {
        return Err(bad(format!(
            "folder `{folder}` must stay inside assets/ without `..`"
        )));
    }
    Ok(rel.to_owned())
}

/// A symlink under the project may lead out of it: the deepest part of `path` that exists must
/// resolve under the resolved `inside`.
fn check_inside(inside: &Path, path: &Path) -> Result<(), TerrainCreateError> {
    let root = inside.canonicalize().map_err(|e| {
        TerrainCreateError::new("terrain_create_bad_path", format!("{}: {e}", inside.display()))
    })?;
    if let Some(real) = path.ancestors().find_map(|p| p.canonicalize().ok())
        && !real.starts_with(&root)
    {
        return Err(TerrainCreateError::new(
            "terrain_create_bad_path",
            format!("`{}` resolves outside {}", path.display(), inside.display()),
        ));
    }
    Ok(())
}

/// A checked create: every path resolved and every value validated, ready for the worker.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainCreatePlan {
    /// The project root.
    pub root: PathBuf,
    /// The terrain's name.
    pub name: String,
    /// The terrain's folder under `assets/` (`maps/hills`).
    pub dir: String,
    /// Samples per side.
    pub size: u32,
    /// Metres per cell.
    pub cell_size: f32,
    /// Heights of sample 0 and 65535.
    pub height_range: [f32; 2],
    /// The layer stack.
    pub layers: TerrainLayers,
    /// The heightmap to import (absolute path and format), or `None` for a flat heightfield.
    pub heightmap: Option<(PathBuf, HeightmapFormat)>,
    /// Starter textures to copy: source file and destination relative to the terrain folder.
    pub detail: Vec<(PathBuf, String)>,
    /// The cooked cache the cook writes into (the editor's platform).
    pub cache_dir: PathBuf,
    /// The cook platform.
    pub platform: String,
    /// The block the level will name.
    pub block: LevelTerrain,
    /// The staging folder the worker writes and cooks in before the move into `assets/`.
    pub staging: PathBuf,
}

impl TerrainCreatePlan {
    /// The terrain's folder, absolute.
    #[must_use]
    pub fn target(&self) -> PathBuf {
        self.root.join("assets").join(&self.dir)
    }

    /// The layer file's name (`hills.terrain.layers.ron`).
    #[must_use]
    pub fn layer_file_name(&self) -> String {
        format!("{}.terrain.layers.ron", self.name)
    }

    /// The span between the outer samples, metres: `(size − 1) · cell_size` (the
    /// `heightfield.json` `size_m`). A cell-centered block's own width is half a cell wider on
    /// each side (ADR-10079).
    #[must_use]
    pub fn world_width(&self) -> f32 {
        (self.size - 1) as f32 * self.cell_size
    }
}

/// The side of a heightmap file from its header alone (a PNG's IHDR, a RAW's byte count), so a
/// size problem is refused before any work starts.
fn heightmap_size(path: &Path, format: HeightmapFormat) -> Result<(u32, u32), TerrainCreateError> {
    let bad = |m: String| TerrainCreateError::new("terrain_create_bad_heightmap", m);
    let len = std::fs::metadata(path)
        .map_err(|e| bad(format!("{}: {e}", path.display())))?
        .len();
    if len > MAX_HEIGHTMAP_FILE {
        return Err(bad(format!(
            "{}: {len} bytes is over the {MAX_HEIGHTMAP_FILE} byte limit",
            path.display()
        )));
    }
    match format {
        HeightmapFormat::Png => {
            let file = std::fs::File::open(path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
            let decoder = png::Decoder::new(std::io::BufReader::new(file));
            let reader = decoder
                .read_info()
                .map_err(|e| bad(format!("{}: {e}", path.display())))?;
            Ok((reader.info().width, reader.info().height))
        }
        HeightmapFormat::R16(_) => {
            let side = usize::try_from(len)
                .ok()
                .and_then(regia_terrain::square_side)
                .ok_or_else(|| {
                    bad(format!(
                        "{}: {len} bytes is not a square 16-bit grid",
                        path.display()
                    ))
                })?;
            Ok((side, side))
        }
    }
}

/// Checks `spec` against the project at `root` and resolves it into a plan. Reads only the header
/// of a heightmap.
///
/// # Errors
///
/// `terrain_create_bad_name` (the name or a layer name), `terrain_create_bad_path` (the folder or
/// the heightmap leaves the project), `terrain_create_exists` (the terrain's folder exists),
/// `terrain_create_bad_size` (a size or cell size not allowed, or a heightmap of another size),
/// `terrain_create_bad_params` (the height range or the layer list),
/// `terrain_create_bad_heightmap` (a missing, oversize or unreadable heightmap).
pub fn plan(root: &Path, spec: &TerrainCreateSpec) -> Result<TerrainCreatePlan, TerrainCreateError> {
    check_name(&spec.name, "the terrain name")?;
    let folder = assets_folder(&spec.folder)?;
    let dir = if folder.is_empty() {
        spec.name.clone()
    } else {
        format!("{folder}/{}", spec.name)
    };
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).map_err(|e| {
        TerrainCreateError::new("terrain_create_failed", format!("{}: {e}", assets.display()))
    })?;
    let target = assets.join(&dir);
    check_inside(&assets, &target)?;
    if target.exists() {
        return Err(TerrainCreateError::new(
            "terrain_create_exists",
            format!("assets/{dir} already exists"),
        ));
    }
    let bad_size = |m: String| TerrainCreateError::new("terrain_create_bad_size", m);
    if !ALLOWED_CELL_SIZES.contains(&spec.cell_size) {
        return Err(bad_size(format!(
            "cell size {} m is not one of {ALLOWED_CELL_SIZES:?}",
            spec.cell_size
        )));
    }
    let [min, max] = spec.height_range;
    if !(min.is_finite() && max.is_finite() && max > min && max - min <= 1.0e6) {
        return Err(TerrainCreateError::new(
            "terrain_create_bad_params",
            format!("height range [{min}, {max}] must be finite with min < max and max − min ≤ 1e6 m"),
        ));
    }
    let heightmap = match &spec.source {
        HeightSource::Flat => None,
        HeightSource::Heightmap { path, endian } => {
            let rel = path.replace('\\', "/");
            let checked = regia_core::project_path::check_relative(&rel).map_err(|e| {
                TerrainCreateError::new("terrain_create_bad_path", format!("heightmap `{path}`: {e}"))
            })?;
            let full = root.join(checked);
            check_inside(root, &full)?;
            let format = match HeightmapFormat::of_path(&full) {
                Some(HeightmapFormat::R16(_)) => HeightmapFormat::R16(*endian),
                Some(f) => f,
                None => {
                    return Err(TerrainCreateError::new(
                        "terrain_create_bad_heightmap",
                        format!("heightmap `{path}`: use a .png, .r16 or .raw file"),
                    ));
                }
            };
            Some((full, format))
        }
    };
    let size = match (&heightmap, spec.size) {
        (Some((full, format)), size) => {
            let (w, h) = heightmap_size(full, *format)?;
            if w != h || !ALLOWED_SIZES.contains(&w) {
                return Err(bad_size(format!(
                    "the heightmap is {w}x{h}; a new terrain is square, one of {ALLOWED_SIZES:?} samples a side"
                )));
            }
            if size.is_some_and(|s| s != w) {
                return Err(bad_size(format!(
                    "size {} does not match the heightmap's {w}",
                    size.unwrap_or_default()
                )));
            }
            w
        }
        (None, Some(size)) if ALLOWED_SIZES.contains(&size) => size,
        (None, size) => {
            return Err(bad_size(format!(
                "size {} is not one of {ALLOWED_SIZES:?} samples a side",
                size.map_or_else(|| "(none)".to_owned(), |s| s.to_string())
            )));
        }
    };
    let names = spec.layers.clone().unwrap_or_else(|| default_layers(root));
    if names.is_empty() || names.len() > MAX_LAYERS {
        return Err(TerrainCreateError::new(
            "terrain_create_bad_params",
            format!("a terrain has 1..={MAX_LAYERS} layers, not {}", names.len()),
        ));
    }
    let mut detail = Vec::new();
    for (i, name) in names.iter().enumerate() {
        check_name(name, "the layer name")?;
        if names[..i].contains(name) {
            return Err(TerrainCreateError::new(
                "terrain_create_bad_params",
                format!("layer {name:?} is named twice"),
            ));
        }
        let starter = root.join("assets/starter/terrain").join(name);
        for file in DETAIL_FILES {
            let src = starter.join(file);
            if src.is_file() {
                detail.push((src, format!("detail/{name}/{file}")));
            }
        }
    }
    let layers = TerrainLayers {
        version: TERRAIN_LAYERS_FORMAT_VERSION,
        wetness_default: 0.0,
        layers: names.iter().map(|n| layer_desc(n, spec.height_range)).collect(),
        puddles: true,
    };
    layers
        .validate()
        .map_err(|e| TerrainCreateError::new("terrain_create_bad_params", format!("layers: {e}")))?;
    let platform = regia_asset::current_platform();
    let world_width = match spec.sample_layout {
        TerrainSampleLayout::VertexCentered => (size - 1) as f32 * spec.cell_size,
        TerrainSampleLayout::CellCentered => size as f32 * spec.cell_size,
    };
    let block = LevelTerrain {
        layers: format!("{dir}/{}.terrain.layers.ron", spec.name),
        map: format!("{dir}/{}", spec.name),
        world_width_m: Some(world_width),
        height_scale_m: max - min,
        height_offset_m: min,
        sample_layout: spec.sample_layout,
    };
    block
        .validate()
        .map_err(|e| TerrainCreateError::new("terrain_create_bad_params", e))?;
    Ok(TerrainCreatePlan {
        root: root.to_path_buf(),
        name: spec.name.clone(),
        dir,
        size,
        cell_size: spec.cell_size,
        height_range: spec.height_range,
        layers,
        heightmap,
        detail,
        cache_dir: regia_asset::cooked_cache_dir(root, &platform),
        platform,
        block,
        staging: staging_dir(root),
    })
}

/// The 16-bit sample of a flat terrain: the height nearest 0 m inside the range.
#[must_use]
pub fn flat_sample(height_range: [f32; 2]) -> u16 {
    let [min, max] = height_range;
    let h = 0.0f32.clamp(min, max);
    let s = ((f64::from(h) - f64::from(min)) * 65535.0 / (f64::from(max) - f64::from(min))).round();
    s.clamp(0.0, 65535.0) as u16
}

/// The source files of a plan, as bytes: the layer file, the heightfield PNG and its json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainSources {
    /// `<name>.terrain.layers.ron`.
    pub layers_ron: String,
    /// `heightfield.png`.
    pub heightfield_png: Vec<u8>,
    /// `heightfield.json`.
    pub heightfield_json: Vec<u8>,
}

/// Builds the plan's source files (decodes the heightmap, or makes the flat heightfield).
///
/// # Errors
///
/// `terrain_create_bad_heightmap` when the heightmap does not decode or changed size since the
/// plan; `terrain_create_failed` when an encoder fails.
pub fn build_sources(plan: &TerrainCreatePlan) -> Result<TerrainSources, TerrainCreateError> {
    let _span =
        tracing::info_span!(target: "regia_editor", "terrain_create.sources", size = plan.size).entered();
    let bad = |m: String| TerrainCreateError::new("terrain_create_bad_heightmap", m);
    let hf = match &plan.heightmap {
        None => Heightfield {
            width: plan.size,
            height: plan.size,
            samples: vec![flat_sample(plan.height_range); (plan.size as usize) * (plan.size as usize)],
        },
        Some((path, format)) => {
            let bytes = std::fs::read(path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
            let hf = match format {
                HeightmapFormat::Png => crate::terrain_panel::decode_png16(&bytes),
                HeightmapFormat::R16(endian) => {
                    regia_terrain::decode_r16(&bytes, plan.size, plan.size, *endian)
                        .map_err(|e| e.to_string())
                }
            }
            .map_err(|e| bad(format!("{}: {e}", path.display())))?;
            if hf.width != plan.size || hf.height != plan.size {
                return Err(bad(format!(
                    "{}: {}x{} now, {} when the create started",
                    path.display(),
                    hf.width,
                    hf.height,
                    plan.size
                )));
            }
            hf
        }
    };
    let failed = |m: String| TerrainCreateError::new("terrain_create_failed", m);
    let heightfield_png = crate::terrain_panel::encode_png16(&hf).map_err(failed)?;
    let layers_ron = plan.layers.to_ron().map_err(|e| failed(e.to_string()))?;
    let mut heightfield_json = serde_json::to_vec_pretty(&json!({ "size_m": plan.world_width() }))
        .map_err(|e| failed(e.to_string()))?;
    heightfield_json.push(b'\n');
    Ok(TerrainSources {
        layers_ron,
        heightfield_png,
        heightfield_json,
    })
}

/// The checks a create makes for a cancel, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before anything is written.
    Write,
    /// After the staging folder is written, before the cook.
    Cook,
    /// After the cook, before the folder moves into `assets/` (the last chance).
    Commit,
}

/// How [`run_create`] ended without creating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// Cancelled at a check: nothing is left.
    Cancelled(Phase),
    /// Failed: nothing is left.
    Failed(TerrainCreateError),
}

/// What a finished create wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedFiles {
    /// The map id (the layer file's `.meta` id).
    pub map_id: AssetId,
    /// Project-relative files under the terrain's folder (`assets/maps/hills/...`), sorted.
    pub files: Vec<String>,
    /// Cooked files written (the terrain index and the tiled heightfield, plus texture tiles).
    pub cooked: usize,
    /// Cook warnings (missing optional slots).
    pub warnings: Vec<String>,
    /// The block the level names.
    pub block: LevelTerrain,
}

static STAGING: AtomicU64 = AtomicU64::new(0);

/// The staging folder of one create: under the project's state directory, so the move into
/// `assets/` is a rename on the same disk and a live asset watcher never sees a half-written
/// terrain.
fn staging_dir(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root)
        .join("staging")
        .join(format!(
            "terrain-{}-{}",
            std::process::id(),
            STAGING.fetch_add(1, Ordering::Relaxed)
        ))
}

fn remove_cooked(cache_dir: &Path, ids: &[AssetId]) {
    for id in ids {
        let file = cache_dir.join(regia_asset::cooked_file_name(*id));
        if let Err(e) = std::fs::remove_file(&file)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "regia_editor", "{}: {e}", file.display());
        }
    }
}

fn remove_tree(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(target: "regia_editor", "{}: {e}", dir.display());
    }
}

fn list_files(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            list_files(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Writes, cooks and commits a plan (the worker's body). `cancelled` is asked at each [`Phase`];
/// a `true` (or a failure) removes the staging folder and the cooked files of the new map id, so
/// nothing is left. `progress` gets a fraction and what the step does.
///
/// # Errors
///
/// [`RunEnd::Cancelled`] or [`RunEnd::Failed`] (`terrain_create_exists` when the folder appeared
/// meanwhile, `terrain_create_bad_heightmap`, `terrain_create_failed`).
pub fn run_create(
    plan: &TerrainCreatePlan,
    cancelled: &dyn Fn(Phase) -> bool,
    progress: &dyn Fn(f64, &str),
) -> Result<CreatedFiles, RunEnd> {
    let _span =
        tracing::info_span!(target: "regia_editor", "terrain_create.run", name = %plan.name).entered();
    if cancelled(Phase::Write) {
        return Err(RunEnd::Cancelled(Phase::Write));
    }
    progress(0.1, "writing the heightfield and layers");
    let staging = &plan.staging;
    let staged = staging.join(&plan.name);
    let failed = |m: String| RunEnd::Failed(TerrainCreateError::new("terrain_create_failed", m));
    let write = || -> Result<(PathBuf, AssetId), RunEnd> {
        let sources = build_sources(plan).map_err(RunEnd::Failed)?;
        let io = |p: &Path, e: std::io::Error| failed(format!("{}: {e}", p.display()));
        std::fs::create_dir_all(&staged).map_err(|e| io(&staged, e))?;
        let layer_path = staged.join(plan.layer_file_name());
        std::fs::write(&layer_path, sources.layers_ron.as_bytes()).map_err(|e| io(&layer_path, e))?;
        let map_id = AssetId::new_random();
        let meta =
            regia_asset::MetaSidecar::new(regia_asset_pipeline::terrain_cook::TERRAIN_IMPORTER, map_id)
                .to_ron_string()
                .map_err(|e| failed(e.to_string()))?;
        let meta_path = regia_asset::meta_path_for(&layer_path);
        std::fs::write(&meta_path, meta.as_bytes()).map_err(|e| io(&meta_path, e))?;
        for (name, bytes) in [
            ("heightfield.png", &sources.heightfield_png),
            ("heightfield.json", &sources.heightfield_json),
        ] {
            let path = staged.join(name);
            std::fs::write(&path, bytes).map_err(|e| io(&path, e))?;
        }
        for (src, rel) in &plan.detail {
            let dst = staged.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
            }
            std::fs::copy(src, &dst).map_err(|e| io(src, e))?;
        }
        Ok((layer_path, map_id))
    };
    let (layer_path, map_id) = match write() {
        Ok(v) => v,
        Err(end) => {
            remove_tree(staging);
            return Err(end);
        }
    };
    // Every file a cook of this map id can leave when it stops part way: the heightfield and the
    // index (texture tiles are listed by the outcome).
    let base_ids = [
        regia_terrain::stream::heightfield_asset_id(map_id),
        regia_terrain::stream::terrain_index_asset_id(map_id),
    ];
    if cancelled(Phase::Cook) {
        remove_tree(staging);
        return Err(RunEnd::Cancelled(Phase::Cook));
    }
    progress(0.3, "cooking the terrain");
    let rel_path = format!("assets/{}/{}", plan.dir, plan.layer_file_name());
    let db = regia_asset_pipeline::AssetDatabase::open(&plan.root).unwrap_or_default();
    let outcome = std::fs::create_dir_all(&plan.cache_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            regia_asset_pipeline::cook_terrain(
                &layer_path,
                &rel_path,
                &plan.cache_dir,
                &plan.platform,
                &db,
                false,
            )
            .map_err(|e| e.to_string())
        });
    let outcome = match outcome {
        Ok(o) if !o.empty => o,
        Ok(_) => {
            remove_tree(staging);
            return Err(failed("the cook found no heightfield".into()));
        }
        Err(e) => {
            remove_cooked(&plan.cache_dir, &base_ids);
            remove_tree(staging);
            return Err(failed(format!("cook: {e}")));
        }
    };
    let mut cooked_ids: Vec<AssetId> = vec![outcome.primary_id];
    cooked_ids.extend(outcome.sub_manifest.iter().map(|m| m.id));
    cooked_ids.extend(base_ids);
    cooked_ids.sort_unstable();
    cooked_ids.dedup();
    if cancelled(Phase::Commit) {
        remove_cooked(&plan.cache_dir, &cooked_ids);
        remove_tree(staging);
        return Err(RunEnd::Cancelled(Phase::Commit));
    }
    progress(0.9, "moving the terrain into assets/");
    let target = plan.target();
    let commit = || -> Result<(), String> {
        if target.exists() {
            return Err(format!("assets/{} appeared meanwhile", plan.dir));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::rename(&staged, &target).map_err(|e| format!("{}: {e}", target.display()))
    };
    if let Err(message) = commit() {
        remove_cooked(&plan.cache_dir, &cooked_ids);
        remove_tree(staging);
        let code = if target.exists() {
            "terrain_create_exists"
        } else {
            "terrain_create_failed"
        };
        return Err(RunEnd::Failed(TerrainCreateError::new(code, message)));
    }
    remove_tree(staging);
    let mut files = Vec::new();
    list_files(&plan.root, &target, &mut files);
    files.sort();
    Ok(CreatedFiles {
        map_id,
        files,
        cooked: cooked_ids
            .iter()
            .filter(|id| plan.cache_dir.join(regia_asset::cooked_file_name(**id)).is_file())
            .count(),
        warnings: outcome.warnings,
        block: plan.block.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// The level reference: one undo step
// ---------------------------------------------------------------------------------------------

/// Names a created terrain in the open level (`LevelSettings::terrain`): one undo step. Undo puts
/// the level's earlier block back and removes the terrain built for the new one; the files stay.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainCreateCommand {
    /// The level's terrain block before.
    pub before: Option<LevelTerrain>,
    /// The block naming the new terrain.
    pub after: Option<LevelTerrain>,
}

fn set_block(world: &mut World, block: Option<LevelTerrain>) {
    let mut settings = regia_scene::level::current_settings(world);
    if settings.terrain != block {
        settings.terrain = block;
        world.insert_resource(settings);
    }
}

impl EditorCommand for TerrainCreateCommand {
    fn apply(&mut self, world: &mut World) {
        set_block(world, self.after.clone());
    }

    fn revert(&mut self, world: &mut World) {
        set_block(world, self.before.clone());
        if self.before != self.after {
            let built: Vec<Entity> = world
                .query::<(Entity, &LevelTerrainSource)>()
                .iter(world)
                .filter(|(_, s)| s.terrain == self.after)
                .map(|(e, _)| e)
                .collect();
            for e in built {
                world.despawn(e);
            }
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        match &self.after {
            Some(t) => format!("New terrain: {}", t.layers),
            None => "Clear the level's terrain".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

type CreateOutcome = Result<CreatedFiles, RunEnd>;

/// The running create (one at a time).
#[derive(Resource, Debug)]
pub struct TerrainCreateJob {
    job: WorkerJob<CreateOutcome>,
    plan: TerrainCreatePlan,
}

impl TerrainCreateJob {
    /// The job id (`terrain_create:<n>`).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.job.id
    }

    /// Asks the worker to stop (it removes what it wrote at its next check).
    pub fn cancel(&self) {
        self.job.cancel();
    }
}

/// How a create ended.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateEnd {
    /// The terrain is in `assets/`, cooked, named by the level and (for a saved level) built.
    Created(CreatedTerrain),
    /// Cancelled: nothing was left.
    Cancelled,
    /// Failed: nothing was left.
    Failed(TerrainCreateError),
}

/// A created terrain, as the dialog and `editor.terrain_create` report it.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedTerrain {
    /// The job id.
    pub job: String,
    /// The files.
    pub files: CreatedFiles,
    /// Samples per side.
    pub size: u32,
    /// Metres per cell.
    pub cell_size: f32,
    /// The terrain entity built for the open level (`None` while the level is unsaved: it is
    /// built when the level has a path).
    pub entity: Option<Entity>,
}

impl CreatedTerrain {
    /// The reply of `editor.terrain_create`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let b = &self.files.block;
        json!({
            "job": self.job,
            "state": "done",
            "layers": format!("assets/{}", b.layers),
            "folder": format!("assets/{}", b.layers.rsplit_once('/').map_or("", |(d, _)| d)),
            "files": self.files.files,
            "map_id": self.files.map_id.to_string(),
            "cooked": self.files.cooked,
            "warnings": self.files.warnings,
            "size": self.size,
            "cell_size": self.cell_size,
            "world_width_m": b.world_width_m,
            "height_range": [b.height_offset_m, b.height_offset_m + b.height_scale_m],
            "terrain": {
                "layers": b.layers,
                "map": b.map,
                "world_width_m": b.world_width_m,
                "height_scale_m": b.height_scale_m,
                "height_offset_m": b.height_offset_m,
                "sample_layout": b.sample_layout.as_str(),
            },
            "entity": self.entity.map(Entity::to_bits),
            "loaded": self.entity.is_some(),
        })
    }
}

/// The last ended create, for the dialog's message.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainCreateLast(pub CreateEnd);

fn project_root(world: &World) -> Result<PathBuf, TerrainCreateError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| TerrainCreateError::new("terrain_create_no_project", "open a project first"))
}

/// Checks `spec` and starts the create as job `terrain_create:<n>`; [`tick_job`] (every frame,
/// [`crate::editor_jobs::tick`]) applies its end. Returns the job id.
///
/// # Errors
///
/// `terrain_create_no_project`, `terrain_create_busy` (a create runs), [`plan`]'s errors, or
/// `terrain_create_failed` when the worker cannot start.
pub fn start(world: &mut World, spec: &TerrainCreateSpec) -> Result<String, TerrainCreateError> {
    if let Some(job) = world.get_resource::<TerrainCreateJob>() {
        return Err(TerrainCreateError::new(
            "terrain_create_busy",
            format!("{} is running; cancel it or wait", job.job.id),
        ));
    }
    let root = project_root(world)?;
    let plan = plan(&root, spec)?;
    let worker_plan = plan.clone();
    let job = WorkerJob::spawn(
        world,
        JOB_KIND,
        TASK,
        format!("creating terrain {} ({} samples a side)", plan.name, plan.size),
        "regia-terrain-create",
        move |cx: WorkerContext| {
            run_create(&worker_plan, &|_| cx.cancelled(), &|f, m| {
                cx.progress(Some(f), m);
            })
        },
    )
    .map_err(|e| TerrainCreateError::new("terrain_create_failed", e))?;
    let id = job.id.clone();
    world.insert_resource(TerrainCreateJob { job, plan });
    Ok(id)
}

/// Applies an ended create: names the terrain in the level (one undo step) and builds it.
fn finish(world: &mut World, job: &TerrainCreateJob, outcome: Result<CreateOutcome, String>) -> CreateEnd {
    let end = match outcome {
        Ok(Ok(files)) => {
            let before = regia_scene::level::current_settings(world).terrain;
            let mut command = TerrainCreateCommand {
                before,
                after: Some(files.block.clone()),
            };
            command.apply(world);
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(Box::new(command));
            crate::content_index::invalidate(world);
            crate::terrain_panel::load_open_level_terrain(world);
            let entity = world
                .query::<(Entity, &LevelTerrainSource)>()
                .iter(world)
                .find(|(_, s)| s.terrain.as_ref() == Some(&files.block))
                .map(|(e, _)| e);
            CreateEnd::Created(CreatedTerrain {
                job: job.job.id.clone(),
                files,
                size: job.plan.size,
                cell_size: job.plan.cell_size,
                entity,
            })
        }
        Ok(Err(RunEnd::Cancelled(_))) => CreateEnd::Cancelled,
        Ok(Err(RunEnd::Failed(e))) => CreateEnd::Failed(e),
        Err(panic) => {
            // The worker stopped without its cleanup: its staging folder goes (nothing reached
            // `assets/`, which only the last step's rename writes).
            remove_tree(&job.plan.staging);
            CreateEnd::Failed(TerrainCreateError::new("terrain_create_failed", panic))
        }
    };
    match &end {
        CreateEnd::Created(created) => {
            tracing::info!(target: "regia_editor", layers = %created.files.block.layers, "terrain created");
            job.job.end(world, Ok((created.to_json(), false)));
        }
        CreateEnd::Cancelled => {
            job.job.end(
                world,
                Ok((
                    json!({ "cancelled": true, "reason": "terrain_create_cancelled" }),
                    true,
                )),
            );
        }
        CreateEnd::Failed(e) => {
            tracing::warn!(target: "regia_editor", code = e.code, "terrain create: {}", e.message);
            job.job.end(world, Err((e.code, e.message.clone())));
        }
    }
    world.insert_resource(TerrainCreateLast(end.clone()));
    end
}

/// Polls the running create (every frame): a returned worker's end is applied. A cancel waits for
/// the worker's next check, which removes what it wrote. Never waits.
pub fn tick_job(world: &mut World) {
    let Some(mut job) = world.remove_resource::<TerrainCreateJob>() else {
        return;
    };
    match job.job.take() {
        Some(outcome) => {
            finish(world, &job, outcome);
        }
        None => {
            let message = job.job.context().message();
            job.job.sync_row(world, &message, job.job.context().fraction());
            world.insert_resource(job);
        }
    }
}

/// Waits for the running create and applies its end (the blocking `editor.terrain_create`).
/// `None` without a running create.
pub fn finish_now(world: &mut World) -> Option<CreateEnd> {
    let mut job = world.remove_resource::<TerrainCreateJob>()?;
    let outcome = job.job.join()?;
    Some(finish(world, &job, outcome))
}

// ---------------------------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------------------------

/// The New terrain dialog's fields.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainCreateDialog {
    /// Shown.
    pub open: bool,
    /// The terrain's name.
    pub name: String,
    /// Project-relative folder.
    pub folder: String,
    /// Samples per side (one of [`ALLOWED_SIZES`]).
    pub size: u32,
    /// Metres per cell (one of [`ALLOWED_CELL_SIZES`]).
    pub cell_size: f32,
    /// Height of sample 0, metres.
    pub min_height: f32,
    /// Height of sample 65535, metres.
    pub max_height: f32,
    /// Layer names, comma-separated, bottom first.
    pub layers: String,
    /// Start from a heightmap file ("Import as new").
    pub from_heightmap: bool,
    /// The heightmap, project-relative.
    pub heightmap: String,
    /// A RAW heightmap is big-endian.
    pub big_endian: bool,
    /// The samples sit at cell centers ([`TerrainSampleLayout::CellCentered`], ADR-10079).
    pub cell_centered: bool,
    /// The last result or error.
    pub message: String,
}

impl Default for TerrainCreateDialog {
    fn default() -> Self {
        let spec = TerrainCreateSpec::default();
        Self {
            open: false,
            name: spec.name,
            folder: spec.folder,
            size: spec.size.unwrap_or(513),
            cell_size: spec.cell_size,
            min_height: spec.height_range[0],
            max_height: spec.height_range[1],
            layers: DEFAULT_LAYER.to_owned(),
            from_heightmap: false,
            heightmap: "assets/maps/heights.png".into(),
            big_endian: false,
            cell_centered: false,
            message: String::new(),
        }
    }
}

impl TerrainCreateDialog {
    /// The spec the fields describe.
    #[must_use]
    pub fn spec(&self) -> TerrainCreateSpec {
        let layers: Vec<String> = self
            .layers
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        TerrainCreateSpec {
            name: self.name.trim().to_owned(),
            folder: self.folder.trim().to_owned(),
            size: (!self.from_heightmap).then_some(self.size),
            cell_size: self.cell_size,
            height_range: [self.min_height, self.max_height],
            layers: Some(layers),
            source: if self.from_heightmap {
                HeightSource::Heightmap {
                    path: self.heightmap.trim().to_owned(),
                    endian: if self.big_endian {
                        RawEndian::Big
                    } else {
                        RawEndian::Little
                    },
                }
            } else {
                HeightSource::Flat
            },
            sample_layout: if self.cell_centered {
                TerrainSampleLayout::CellCentered
            } else {
                TerrainSampleLayout::VertexCentered
            },
        }
    }
}

/// Opens the dialog with the project's default layers and the first free name (Terrain panel ▸
/// New terrain…, Content ▸ New ▸ Terrain…). `folder` sets the folder when given.
pub fn open_dialog(world: &mut World, folder: Option<&str>) {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let mut dialog = world
        .get_resource::<TerrainCreateDialog>()
        .cloned()
        .unwrap_or_default();
    if let Some(folder) = folder {
        dialog.folder = if folder.is_empty() || folder == "assets" {
            "assets".to_owned()
        } else if folder.starts_with("assets/") {
            folder.to_owned()
        } else {
            format!("assets/{folder}")
        };
    }
    if let Some(root) = &root {
        dialog.layers = default_layers(root).join(", ");
        let base = assets_folder(&dialog.folder).unwrap_or_default();
        let taken = |name: &str| root.join("assets").join(&base).join(name).exists();
        if taken(&dialog.name) {
            let mut n = 1;
            while taken(&format!("terrain-{n}")) {
                n += 1;
            }
            dialog.name = format!("terrain-{n}");
        }
    }
    dialog.open = true;
    dialog.message.clear();
    world.insert_resource(dialog);
}

/// Draws the New terrain dialog when it is open (a window over the editor). The Terrain panel
/// calls it every frame.
pub fn dialog_window(ctx: &egui::Context, world: &mut World) {
    let Some(mut dialog) = world.get_resource::<TerrainCreateDialog>().cloned() else {
        return;
    };
    if !dialog.open {
        return;
    }
    if let Some(TerrainCreateLast(end)) = world.remove_resource::<TerrainCreateLast>() {
        dialog.message = match end {
            CreateEnd::Created(c) => {
                dialog.open = false;
                format!(
                    "created {}{}",
                    c.files.block.layers,
                    if c.entity.is_some() {
                        ""
                    } else {
                        " (save the level to build it)"
                    }
                )
            }
            CreateEnd::Cancelled => "cancelled: nothing was written".to_owned(),
            CreateEnd::Failed(e) => e.to_string(),
        };
    }
    let running = world
        .get_resource::<TerrainCreateJob>()
        .map(|job| (job.job.id.clone(), job.job.context().fraction()));
    let mut open = dialog.open;
    egui::Window::new("New terrain")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("terrain_create_fields")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut dialog.name);
                    ui.end_row();
                    ui.label("Folder");
                    ui.text_edit_singleline(&mut dialog.folder);
                    ui.end_row();
                    ui.label("Size");
                    ui.add_enabled_ui(!dialog.from_heightmap, |ui| {
                        egui::ComboBox::from_id_salt("terrain_create_size")
                            .selected_text(format!("{0} × {0}", dialog.size))
                            .show_ui(ui, |ui| {
                                for s in ALLOWED_SIZES {
                                    ui.selectable_value(
                                        &mut dialog.size,
                                        s,
                                        format!("{s} × {s} samples ({} cells)", s - 1),
                                    );
                                }
                            });
                    });
                    ui.end_row();
                    ui.label("Cell size");
                    egui::ComboBox::from_id_salt("terrain_create_cell")
                        .selected_text(format!("{} m", dialog.cell_size))
                        .show_ui(ui, |ui| {
                            for c in ALLOWED_CELL_SIZES {
                                ui.selectable_value(&mut dialog.cell_size, c, format!("{c} m"));
                            }
                        });
                    ui.end_row();
                    ui.label("Height range (m)");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut dialog.min_height).speed(1.0));
                        ui.label("to");
                        ui.add(egui::DragValue::new(&mut dialog.max_height).speed(1.0));
                    });
                    ui.end_row();
                    ui.label("Layers");
                    ui.text_edit_singleline(&mut dialog.layers);
                    ui.end_row();
                });
            ui.checkbox(&mut dialog.cell_centered, "Samples at cell centers")
                .on_hover_text(
                    "Each sample is the center of a cell (a heightmap whose pixels are areas): \
                     the terrain is size × cell size wide",
                );
            ui.checkbox(&mut dialog.from_heightmap, "Start from a 16-bit heightmap");
            if dialog.from_heightmap {
                ui.horizontal(|ui| {
                    ui.label("Heightmap");
                    ui.text_edit_singleline(&mut dialog.heightmap);
                });
                ui.checkbox(&mut dialog.big_endian, "RAW big-endian");
                ui.weak("The file's size sets the terrain's size; its samples are kept as they are.");
            } else {
                let cells = if dialog.cell_centered {
                    dialog.size
                } else {
                    dialog.size - 1
                };
                ui.weak(format!(
                    "{:.0} m × {:.0} m, flat",
                    cells as f32 * dialog.cell_size,
                    cells as f32 * dialog.cell_size
                ));
            }
            ui.separator();
            match &running {
                Some((id, fraction)) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("{id}: {:.0} %", fraction.unwrap_or(0.0) * 100.0));
                        if ui.button("Cancel").clicked()
                            && let Some(job) = world.get_resource::<TerrainCreateJob>()
                        {
                            job.cancel();
                        }
                    });
                }
                None => {
                    if ui.button("Create").clicked() {
                        let spec = dialog.spec();
                        dialog.message = match start(world, &spec) {
                            Ok(id) => format!("{id} started"),
                            Err(e) => e.to_string(),
                        };
                    }
                }
            }
            if !dialog.message.is_empty() {
                ui.label(&dialog.message);
            }
        });
    dialog.open = dialog.open && open;
    world.insert_resource(dialog);
}

// ---------------------------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------------------------

fn spec_from_params(params: &Value) -> Result<(TerrainCreateSpec, bool), TerrainCreateError> {
    let bad = |m: String| TerrainCreateError::new("terrain_create_bad_params", m);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`name` (a string) is required".into()))?;
    let folder = match params.get("folder") {
        None | Some(Value::Null) => "assets/maps".to_owned(),
        Some(v) => v
            .as_str()
            .ok_or_else(|| bad("`folder` must be a string".into()))?
            .to_owned(),
    };
    let size = match params.get("size") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|s| u32::try_from(s).ok())
                .ok_or_else(|| bad("`size` must be a whole number".into()))?,
        ),
    };
    let cell_size = match params.get("cell_size") {
        None | Some(Value::Null) => 1.0,
        Some(v) => v
            .as_f64()
            .ok_or_else(|| bad("`cell_size` must be a number".into()))? as f32,
    };
    let height_range = match params.get("height_range") {
        None | Some(Value::Null) => TerrainCreateSpec::default().height_range,
        Some(v) => v
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
            .ok_or_else(|| bad("`height_range` is [min, max] in metres".into()))?,
    };
    let layers = match params.get("layers") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_array()
                .and_then(|a| {
                    a.iter()
                        .map(|n| n.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| bad("`layers` is a list of names".into()))?,
        ),
    };
    let endian = match params.get("endian").and_then(Value::as_str) {
        None | Some("little") => RawEndian::Little,
        Some("big") => RawEndian::Big,
        Some(other) => return Err(bad(format!("`endian` is little or big, not {other:?}"))),
    };
    let source = match params.get("heightmap") {
        None | Some(Value::Null) => HeightSource::Flat,
        Some(v) => HeightSource::Heightmap {
            path: v
                .as_str()
                .ok_or_else(|| bad("`heightmap` must be a project-relative path".into()))?
                .to_owned(),
            endian,
        },
    };
    let sample_layout = match params.get("sample_layout") {
        None | Some(Value::Null) => TerrainSampleLayout::VertexCentered,
        Some(Value::String(s)) if s == "VertexCentered" => TerrainSampleLayout::VertexCentered,
        Some(Value::String(s)) if s == "CellCentered" => TerrainSampleLayout::CellCentered,
        Some(other) => {
            return Err(bad(format!(
                "`sample_layout` is VertexCentered or CellCentered, not {other}"
            )));
        }
    };
    let asynchronous = params.get("async").and_then(Value::as_bool).unwrap_or(false);
    Ok((
        TerrainCreateSpec {
            name: name.to_owned(),
            folder,
            size,
            cell_size,
            height_range,
            layers,
            source,
            sample_layout,
        },
        asynchronous,
    ))
}

/// `editor.terrain_create`: what the New terrain dialog's Create does. Blocking by default (the
/// reply is the created terrain); `async: true` answers `{ job }` at once (poll `agent.job_status`,
/// cancel with `editor.cancel_job`).
///
/// # Errors
///
/// The `terrain_create_*` codes of [`plan`] and [`start`]; `terrain_create_cancelled` when the job
/// was cancelled.
pub fn terrain_create_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (spec, asynchronous) = spec_from_params(params).map_err(|e| e.rpc())?;
    let id = start(world, &spec).map_err(|e| e.rpc())?;
    if asynchronous {
        return Ok(json!({ "job": id, "state": "running" }));
    }
    match finish_now(world) {
        Some(CreateEnd::Created(created)) => Ok(created.to_json()),
        Some(CreateEnd::Cancelled) => Err(TerrainCreateError::new(
            "terrain_create_cancelled",
            format!("{id} was cancelled; nothing was written"),
        )
        .rpc()),
        Some(CreateEnd::Failed(e)) => Err(e.rpc()),
        None => Err(TerrainCreateError::new("terrain_create_failed", format!("{id} vanished")).rpc()),
    }
}

/// An editor agent method's handler.
type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// `editor.terrain_create`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.terrain_create",
            tool: "terrain_create",
            description: "Create a terrain as Terrain ▸ New terrain does (ADR-7727): a flat 16-bit heightfield \
                (or the samples of a 16-bit PNG/RAW `heightmap`, Import as new) and a layer file under \
                assets/<folder>/<name>/ by the cook convention, cooked as job terrain_create:<n>, then named \
                in the open level's LevelSettings::terrain (one undo step; undo keeps the files) and built. \
                size: 129, 257, 513, 1025, 2049 or 4097 samples a side; cell_size: 0.25, 0.5, 1, 2 or 4 m. \
                Cancel with editor.cancel_job (nothing is left). Errors terrain_create_bad_params, \
                terrain_create_bad_name, terrain_create_bad_path, terrain_create_exists, \
                terrain_create_bad_size, terrain_create_bad_heightmap, terrain_create_no_project, \
                terrain_create_busy, terrain_create_failed, terrain_create_cancelled.",
            params: &[
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: true,
                    description: "Terrain name: its folder and file stem (letters, digits, _ and -).",
                },
                ParamSchema {
                    name: "folder",
                    kind: "string",
                    required: false,
                    description: "Project-relative folder under assets/ (default assets/maps).",
                },
                ParamSchema {
                    name: "size",
                    kind: "integer",
                    required: false,
                    description: "Samples a side: 129, 257, 513, 1025, 2049 or 4097 (with heightmap: the file's).",
                },
                ParamSchema {
                    name: "cell_size",
                    kind: "number",
                    required: false,
                    description: "Metres per cell: 0.25, 0.5, 1 (default), 2 or 4.",
                },
                ParamSchema {
                    name: "height_range",
                    kind: "array",
                    required: false,
                    description: "[min, max] metres of samples 0 and 65535 (default [0, 256]).",
                },
                ParamSchema {
                    name: "layers",
                    kind: "array",
                    required: false,
                    description: "Layer names, bottom first (default grass, rock, dirt with starter content, else ground).",
                },
                ParamSchema {
                    name: "heightmap",
                    kind: "string",
                    required: false,
                    description: "Project-relative 16-bit .png, .r16 or .raw to start from (Import as new).",
                },
                ParamSchema {
                    name: "endian",
                    kind: "string",
                    required: false,
                    description: "RAW byte order: little (default) or big.",
                },
                ParamSchema {
                    name: "sample_layout",
                    kind: "string",
                    required: false,
                    description: "VertexCentered (default: samples on the edges, (size - 1) * cell_size wide) or CellCentered (each sample at a cell's center, size * cell_size wide; ADR-10079).",
                },
                ParamSchema {
                    name: "async",
                    kind: "boolean",
                    required: false,
                    description: "Answer { job } at once instead of waiting for the cook.",
                },
            ],
            result: "{ job, state, layers, folder, files, map_id, cooked, warnings, size, cell_size, world_width_m, \
                     height_range, terrain, entity, loaded }",
            mutating: true,
            permission: Permission::Assets,
        },
        terrain_create_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::level::LevelSettings;
    use std::cell::Cell;

    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/maps")).expect("assets");
        root
    }

    fn editor_world(root: &Path) -> (World, PathBuf) {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        let level = root.join("assets/maps/arena.level.ron");
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::level::LevelId::default(),
            path: Some(level.clone()),
        });
        world.insert_resource(LevelSettings::default());
        world.insert_resource(UndoStack::new());
        (world, level)
    }

    fn spec(name: &str) -> TerrainCreateSpec {
        TerrainCreateSpec {
            name: name.into(),
            size: Some(129),
            height_range: [-10.0, 90.0],
            ..TerrainCreateSpec::default()
        }
    }

    fn created(world: &mut World, spec: &TerrainCreateSpec) -> CreatedTerrain {
        start(world, spec).expect("start");
        match finish_now(world).expect("ended") {
            CreateEnd::Created(c) => c,
            other => panic!("not created: {other:?}"),
        }
    }

    fn cooked_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        list_files(
            root,
            &regia_core::brand::project_state_dir(root).join("cooked"),
            &mut out,
        );
        out.retain(|f| f.ends_with(".fckd"));
        out.sort();
        out
    }

    fn samples(world: &World, e: Entity) -> Vec<u16> {
        world
            .get::<regia_terrain::Terrain>(e)
            .expect("terrain")
            .map
            .to_heightfield()
            .samples
    }

    /// The acceptance of item 1: a 129² terrain in a temp project is written by the cook
    /// convention, loads through the runtime loader, `terrain.level_terrain` reports it, a sculpt
    /// stroke works on it, and Undo removes the level's reference but keeps the files.
    #[test]
    fn a_new_129_terrain_cooks_loads_sculpts_and_undoes_to_no_reference() {
        let root = project("terrain_create_129");
        let (mut world, _) = editor_world(&root);
        let made = created(&mut world, &spec("hills"));
        let dir = root.join("assets/maps/hills");
        for f in [
            "hills.terrain.layers.ron",
            "hills.terrain.layers.ron.meta",
            "heightfield.png",
            "heightfield.json",
        ] {
            assert!(dir.join(f).is_file(), "{f}: {:?}", made.files.files);
        }
        let block = made.files.block.clone();
        assert_eq!(block.layers, "maps/hills/hills.terrain.layers.ron");
        assert_eq!(block.map, "maps/hills/hills");
        assert_eq!(block.world_width_m, Some(128.0));
        assert_eq!((block.height_offset_m, block.height_scale_m), (-10.0, 100.0));

        // The runtime loader builds it from the cooked heightfield alone.
        let cooked = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
        let load = regia_terrain::load_level_terrain(
            &root.join("assets"),
            &cooked,
            "maps/arena.level.ron",
            Some(&block),
        )
        .expect("runtime load");
        let terrain = load.terrain.expect("terrain");
        assert_eq!((terrain.map.width(), terrain.map.height()), (129, 129));
        let flat = flat_sample([-10.0, 90.0]);
        assert!(terrain.map.to_heightfield().samples.iter().all(|s| *s == flat));
        assert!(terrain.map.height_at_sample(64, 64).abs() < 0.01, "flat at 0 m");

        // The level names it and the editor built it.
        assert_eq!(world.resource::<LevelSettings>().terrain.as_ref(), Some(&block));
        let entity = made.entity.expect("built for the saved level");
        assert!(
            world
                .get::<crate::terrain_tool::TerrainCookTarget>(entity)
                .is_some()
        );
        let source = world.get::<LevelTerrainSource>(entity).expect("level terrain");
        assert_eq!(source.terrain.as_ref(), Some(&block));
        assert_eq!(source.level, "maps/arena.level.ron");
        assert_eq!(
            world
                .resource::<crate::terrain_panel::EditorLevelTerrain>()
                .entity,
            Some(entity)
        );

        // A sculpt stroke works on it.
        let before = samples(&world, entity);
        assert!(crate::terrain_tool::begin_stroke(
            &mut world,
            entity,
            regia_core::math::Vec2::ZERO,
            false
        ));
        crate::terrain_tool::end_stroke(&mut world);
        crate::terrain_tool::pump(&mut world, true);
        assert_ne!(samples(&world, entity), before, "the stroke changed heights");

        // Undo the stroke, then the create: the reference and the built terrain go, files stay.
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(samples(&world, entity), before);
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(world.resource::<LevelSettings>().terrain, None);
        assert!(world.get_entity(entity).is_err(), "the built terrain is gone");
        crate::terrain_panel::load_open_level_terrain(&mut world);
        assert_eq!(world.query::<&LevelTerrainSource>().iter(&world).count(), 0);
        assert!(dir.join("heightfield.png").is_file(), "files stay");
        // Redo names it again; the next frame builds it.
        assert!(crate::command::redo_world(&mut world));
        crate::terrain_panel::load_open_level_terrain(&mut world);
        assert_eq!(world.query::<&LevelTerrainSource>().iter(&world).count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cancel at any check leaves nothing: no terrain folder, no staging folder, no cooked file.
    #[test]
    fn a_cancel_at_each_check_leaves_no_partial_output() {
        for phase in [Phase::Write, Phase::Cook, Phase::Commit] {
            let root = project("terrain_create_cancel");
            let before = cooked_files(&root);
            let plan = plan(&root, &spec("hills")).expect("plan");
            let checks = Cell::new(0);
            let end = run_create(
                &plan,
                &|p| {
                    checks.set(checks.get() + 1);
                    p == phase
                },
                &|_, _| {},
            );
            assert_eq!(end, Err(RunEnd::Cancelled(phase)));
            assert!(!root.join("assets/maps/hills").exists(), "{phase:?}");
            let staging = regia_core::brand::project_state_dir(&root).join("staging");
            let mut left = Vec::new();
            list_files(&root, &staging, &mut left);
            assert!(left.is_empty(), "{phase:?}: staging {left:?}");
            assert_eq!(cooked_files(&root), before, "{phase:?}: cooked files");
            if phase == Phase::Commit {
                assert_eq!(checks.get(), 3, "the commit check runs after the cook");
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// The job route: a cancel requested before the worker's first check ends the job
    /// `cancelled` and leaves no output, and the level names nothing.
    #[test]
    fn a_cancelled_job_ends_cancelled_and_names_nothing() {
        let root = project("terrain_create_cancel_job");
        let (mut world, _) = editor_world(&root);
        let plan = plan(&root, &spec("hills")).expect("plan");
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let worker_plan = plan.clone();
        let job = WorkerJob::spawn(
            &mut world,
            JOB_KIND,
            TASK,
            "t".into(),
            "t",
            move |cx: WorkerContext| {
                let _ = gate.recv();
                run_create(&worker_plan, &|_| cx.cancelled(), &|_, _| {})
            },
        )
        .expect("spawn");
        let id = job.id.clone();
        let handle = job.context().handle.clone();
        world.insert_resource(TerrainCreateJob { job, plan });
        crate::editor_jobs::cancel_job(&mut world, &json!({ "id": id })).expect("cancel");
        release.send(()).expect("release");
        assert_eq!(finish_now(&mut world), Some(CreateEnd::Cancelled));
        assert!(!root.join("assets/maps/hills").exists());
        assert_eq!(world.resource::<LevelSettings>().terrain, None);
        assert_eq!(handle.state(), regia_agent::jobs::JobState::Cancelled);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Item 2: `editor.terrain_create` writes the same bytes as the dialog's Create.
    #[test]
    fn the_agent_writes_the_same_bytes_as_the_dialog() {
        let a = project("terrain_create_dialog");
        let b = project("terrain_create_agent");
        let (mut wa, _) = editor_world(&a);
        let dialog = TerrainCreateDialog {
            name: "hills".into(),
            size: 257,
            cell_size: 2.0,
            min_height: -20.0,
            max_height: 300.0,
            layers: "grass, rock, dirt".into(),
            ..TerrainCreateDialog::default()
        };
        created(&mut wa, &dialog.spec());
        let (mut wb, _) = editor_world(&b);
        let reply = terrain_create_method(
            &mut wb,
            &json!({ "name": "hills", "size": 257, "cell_size": 2.0, "height_range": [-20.0, 300.0],
                      "layers": ["grass", "rock", "dirt"] }),
        )
        .expect("agent create");
        assert_eq!(reply["loaded"], true, "{reply}");
        assert_eq!(reply["world_width_m"], 512.0, "{reply}");
        for f in ["hills.terrain.layers.ron", "heightfield.png", "heightfield.json"] {
            let read = |root: &Path| std::fs::read(root.join("assets/maps/hills").join(f)).expect("read");
            assert_eq!(read(&a), read(&b), "{f}");
        }
        assert_eq!(
            wa.resource::<LevelSettings>().terrain,
            wb.resource::<LevelSettings>().terrain
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// ADR-10079: `sample_layout: CellCentered` makes `size` cells of `cell_size`: the level's block
    /// names the layout and the cells' extent, and the loaded map puts its samples at the cells'
    /// centers (spacing `cell_size`, sample 0 half a cell in from the edge). A bad value is refused.
    #[test]
    fn a_cell_centered_create_names_its_layout_and_loads_it() {
        let root = project("terrain_create_cell_centered");
        let (mut world, _) = editor_world(&root);
        let reply = terrain_create_method(
            &mut world,
            &json!({ "name": "cells", "size": 129, "cell_size": 2.0, "height_range": [0.0, 100.0],
                      "sample_layout": "CellCentered" }),
        )
        .expect("agent create");
        assert_eq!(reply["loaded"], true, "{reply}");
        assert_eq!(reply["world_width_m"], 258.0, "{reply}");
        assert_eq!(reply["terrain"]["sample_layout"], "CellCentered", "{reply}");
        let block = world.resource::<LevelSettings>().terrain.clone().expect("block");
        assert_eq!(block.sample_layout, TerrainSampleLayout::CellCentered);
        assert_eq!(block.world_width_m, Some(258.0));
        let entity = Entity::from_bits(reply["entity"].as_u64().expect("entity"));
        let map = &world.get::<regia_terrain::Terrain>(entity).expect("terrain").map;
        assert_eq!(map.sample_layout(), TerrainSampleLayout::CellCentered);
        assert_eq!(map.sample_spacing(), 2.0);
        assert_eq!(map.sample_to_world(0, 0), [-128.0, -128.0]);
        assert_eq!(map.cell_extent(), [258.0, 258.0]);
        assert_eq!(
            code(
                &mut world,
                json!({ "name": "y", "size": 129, "sample_layout": "Centered" })
            ),
            "terrain_create_bad_params"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn code(world: &mut World, params: Value) -> String {
        let err = terrain_create_method(world, &params).expect_err("refused");
        err.data.expect("data")["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The documented refusals, each before anything is written.
    #[test]
    fn refusals_have_their_codes() {
        let root = project("terrain_create_refusals");
        let (mut world, _) = editor_world(&root);
        std::fs::create_dir_all(root.join("assets/maps/taken")).expect("dir");
        assert_eq!(
            code(&mut world, json!({ "name": "taken", "size": 129 })),
            "terrain_create_exists"
        );
        assert_eq!(
            code(
                &mut world,
                json!({ "name": "x", "size": 129, "folder": "assets/../outside" })
            ),
            "terrain_create_bad_path"
        );
        assert_eq!(
            code(&mut world, json!({ "name": "x", "size": 129, "folder": "/tmp" })),
            "terrain_create_bad_path"
        );
        assert_eq!(
            code(&mut world, json!({ "name": "x", "size": 100 })),
            "terrain_create_bad_size"
        );
        assert_eq!(
            code(&mut world, json!({ "name": "x", "size": 129, "cell_size": 3.0 })),
            "terrain_create_bad_size"
        );
        assert_eq!(
            code(&mut world, json!({ "name": "a.b", "size": 129 })),
            "terrain_create_bad_name"
        );
        assert_eq!(
            code(
                &mut world,
                json!({ "name": "x", "size": 129, "height_range": [5.0, 5.0] })
            ),
            "terrain_create_bad_params"
        );
        assert_eq!(
            code(
                &mut world,
                json!({ "name": "x", "size": 129, "layers": ["a", "a"] })
            ),
            "terrain_create_bad_params"
        );
        assert_eq!(
            code(&mut world, json!({ "name": "x", "heightmap": "assets/none.png" })),
            "terrain_create_bad_heightmap"
        );
        assert_eq!(
            code(&mut World::new(), json!({ "name": "x", "size": 129 })),
            "terrain_create_no_project"
        );
        // A symlink inside assets/ that leads out of the project.
        #[cfg(unix)]
        {
            let outside = regia_test_utils::unique_temp_dir("terrain_create_outside");
            std::fs::create_dir_all(&outside).expect("outside");
            std::os::unix::fs::symlink(&outside, root.join("assets/escape")).expect("symlink");
            assert_eq!(
                code(
                    &mut world,
                    json!({ "name": "x", "size": 129, "folder": "assets/escape" })
                ),
                "terrain_create_bad_path"
            );
            assert!(!outside.join("x").exists());
            let _ = std::fs::remove_dir_all(&outside);
        }
        assert!(!root.join("assets/maps/x").exists());
        assert_eq!(world.resource::<LevelSettings>().terrain, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Item 3: importing the exported heightmap of terrain A as new terrain B gives B A's samples,
    /// through PNG and through RAW (both byte orders).
    #[test]
    fn import_as_new_keeps_the_exported_samples() {
        let root = project("terrain_create_import");
        let (mut world, _) = editor_world(&root);
        let a = created(&mut world, &spec("a")).entity.expect("a");
        // Make A uneven: one raise stroke off-centre.
        assert!(crate::terrain_tool::begin_stroke(
            &mut world,
            a,
            regia_core::math::Vec2::new(20.0, -10.0),
            false
        ));
        crate::terrain_tool::end_stroke(&mut world);
        crate::terrain_tool::pump(&mut world, true);
        let wanted = samples(&world, a);
        assert!(wanted.iter().any(|s| *s != wanted[0]), "A is not flat");
        let files = [
            ("assets/maps/a.png", HeightmapFormat::Png, "little", "b_png"),
            (
                "assets/maps/a.r16",
                HeightmapFormat::R16(RawEndian::Little),
                "little",
                "b_r16",
            ),
            (
                "assets/maps/a_be.raw",
                HeightmapFormat::R16(RawEndian::Big),
                "big",
                "b_raw_be",
            ),
        ];
        // Export every file first: a create names the new terrain in the level, and the level
        // then builds that one (A, with an unsaved sculpt, is not replaced, but the test should
        // not depend on that).
        for (file, format, _, _) in files {
            crate::terrain_panel::export_heightmap(&world, a, &root.join(file), format).expect("export");
        }
        for (file, _, endian, name) in files {
            let reply = terrain_create_method(
                &mut world,
                &json!({ "name": name, "heightmap": file, "endian": endian, "height_range": [-10.0, 90.0] }),
            )
            .expect("import as new");
            assert_eq!(reply["size"], 129, "{reply}");
            let block: LevelTerrain = serde_json::from_value(reply["terrain"].clone()).expect("block");
            let cooked = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
            let load = regia_terrain::load_level_terrain(
                &root.join("assets"),
                &cooked,
                "maps/arena.level.ron",
                Some(&block),
            )
            .expect("load");
            assert_eq!(
                load.terrain.expect("b").map.to_heightfield().samples,
                wanted,
                "{file}"
            );
        }
        // A size the heightmap does not have is refused.
        assert_eq!(
            code(
                &mut world,
                json!({ "name": "c", "heightmap": "assets/maps/a.png", "size": 257 })
            ),
            "terrain_create_bad_size"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Starter content gives the grass, rock and dirt layers, and their textures go where the cook
    /// convention reads them.
    #[test]
    fn starter_layers_are_the_default_and_their_textures_are_copied() {
        let root = project("terrain_create_starter");
        assert_eq!(default_layers(&root), vec!["ground".to_owned()]);
        for name in STARTER_LAYERS {
            std::fs::create_dir_all(root.join("assets/starter/terrain").join(name)).expect("dir");
        }
        let albedo = root.join("assets/starter/terrain/rock/albedo.png");
        let image = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0; 4],
        };
        std::fs::write(&albedo, crate::terrain_panel::encode_png16(&image).expect("png")).expect("write");
        assert_eq!(default_layers(&root), vec!["grass", "rock", "dirt"]);
        let plan = plan(
            &root,
            &TerrainCreateSpec {
                layers: None,
                ..spec("hills")
            },
        )
        .expect("plan");
        let names: Vec<&str> = plan.layers.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["grass", "rock", "dirt"]);
        assert_eq!(plan.detail, vec![(albedo, "detail/rock/albedo.png".to_owned())]);
        assert!(plan.layers.layers[1].triplanar, "rock is triplanar");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flat_sample_is_the_height_nearest_zero() {
        assert_eq!(flat_sample([0.0, 256.0]), 0);
        assert_eq!(flat_sample([-100.0, 100.0]), 32768);
        assert_eq!(flat_sample([10.0, 20.0]), 0);
        assert_eq!(flat_sample([-20.0, -10.0]), 65535);
    }
}
