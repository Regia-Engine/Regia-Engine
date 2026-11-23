// SPDX-License-Identifier: MPL-2.0
//! The `regia cook` pass that cooks every level's mesh layers (ADR-8451, spec 26 § 2a.10).
//!
//! A level's `terrain` block (scene format v12) names the heightfield and its height mapping; the
//! map's `<map stem>.meshlayers.ron` sits next to the map. For each level that names a heightfield
//! and whose map has a layers file, the pass builds the sim ground from the cooked heightfield and
//! the block's mapping (`regia_terrain::load_level_ground`, the run-time terrain load's own
//! reader), resolves each layer's `material` against the cook's manifest, and calls
//! [`crate::terrain_mesh_layers::cook_mesh_layers`]. The cooked `<map stem>.meshlayers` goes next to the map
//! (`LevelTerrainFiles::meshlayers`), the chunk LOD meshes into the platform's cooked cache, with
//! one manifest row each so paks carry them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::{AssetId, ManifestEntry};
use regia_scene::level::{LEVEL_SETTINGS_PATH, LevelSettings, LevelTerrain};
use regia_terrain::{
    CdlodConfig, LevelTerrainFiles, chunk_lod_asset_path, load_level_ground, meshlayers_source_path,
};
use regia_world_geom::cook::GroundInput;

use crate::terrain_mesh_layers::{
    MeshLayersCookOutcome, MeshLayersCookRequest, cook_mesh_layers, material_resolver, stage_a_sampler,
};

/// The manifest type path of a chunk LOD mesh.
pub const CHUNK_TYPE_PATH: &str = "regia_asset::CookedMesh";

/// Whether `path` is a mesh-layer file the generic cook must not offer to an importer: a
/// `.meshlayers.ron` source, a `.passages.ron` it names, or a cooked `.meshlayers` (ADR-8451).
#[must_use]
pub fn is_mesh_layer_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".meshlayers.ron") || name.ends_with(".passages.ron") || name.ends_with(".meshlayers")
}

/// Why one level's mesh layers did not cook. Codes are part of the agent protocol.
#[derive(Debug, thiserror::Error)]
pub enum LevelMeshLayersError {
    /// (`meshlayers_level_read`) the level or its settings do not read.
    #[error("{0} (meshlayers_level_read)")]
    LevelRead(String),
    /// (`meshlayers_no_heightfield`) the map has a `.meshlayers.ron` but the level's terrain block
    /// names no heightfield (`layers` is empty).
    #[error(
        "{source_file} needs a heightfield: the level's terrain block names no `layers` file (meshlayers_no_heightfield)"
    )]
    NoHeightfield {
        /// The `.meshlayers.ron`.
        source_file: String,
    },
    /// (`meshlayers_ground_not_square`) the sim ground lattice is square.
    #[error(
        "the heightfield is {width}x{height}; the mesh-layer ground lattice is square (meshlayers_ground_not_square)"
    )]
    GroundNotSquare {
        /// Samples along x.
        width: u32,
        /// Samples along z.
        height: u32,
    },
    /// (`meshlayers_level_conflict`) two levels name one map with different ground.
    #[error(
        "{source_file}: levels `{first}` and `{second}` give it different ground (map id or height mapping) (meshlayers_level_conflict)"
    )]
    Conflict {
        /// The `.meshlayers.ron`.
        source_file: String,
        /// The level that came first (path order).
        first: String,
        /// The level that disagrees.
        second: String,
    },
    /// (`meshlayers_ground_offset`) the cooked count mapping (ADR-8471) centers the grid away from
    /// the origin; the run-time terrain can sit there, but the mesh layers' canonical ground
    /// lattice is centered on the origin, so the cook refuses rather than build a sim ground that
    /// is offset from the drawn terrain (ADR-8842).
    #[error(
        "the count mapping centers the ground at {center:?} (first sample {sample0:?}); mesh layers need a ground centered on the origin: set sample0_x_m and sample0_z_m to {centered_sample0} (meshlayers_ground_offset)"
    )]
    GroundOffset {
        /// The run-time grid center, X and Z.
        center: [f32; 2],
        /// The mapping's first sample, X and Z.
        sample0: [f32; 2],
        /// The first sample that centers the grid on the origin.
        centered_sample0: f32,
    },
    /// A level-terrain error (`level_terrain_*`).
    #[error("{0}")]
    Terrain(#[from] regia_terrain::LevelTerrainError),
    /// The mesh-layer cook failed (its own codes).
    #[error("{error}")]
    Cook {
        /// The cook error.
        error: crate::terrain_mesh_layers::MeshLayersCookError,
    },
}

impl LevelMeshLayersError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LevelRead(_) => "meshlayers_level_read",
            Self::NoHeightfield { .. } => "meshlayers_no_heightfield",
            Self::GroundNotSquare { .. } => "meshlayers_ground_not_square",
            Self::Conflict { .. } => "meshlayers_level_conflict",
            Self::GroundOffset { .. } => "meshlayers_ground_offset",
            Self::Terrain(e) => e.code(),
            Self::Cook { error } => error.code(),
        }
    }

    /// The report line: the message, with the code appended when the message does not name it.
    #[must_use]
    pub fn report_line(&self) -> String {
        let text = self.to_string();
        if text.contains(self.code()) {
            text
        } else {
            format!("{text} ({})", self.code())
        }
    }
}

/// One map whose mesh layers a level asks for.
#[derive(Clone, Debug)]
pub struct LevelMeshLayersJob {
    /// The level (relative to `assets/`, `/` separators) that named the map first.
    pub level: String,
    /// The `<map stem>.meshlayers.ron`.
    pub source: PathBuf,
    /// The cooked `<map stem>.meshlayers` to write.
    pub out_file: PathBuf,
    /// The map id (the layer file's `.meta` id).
    pub map_id: AssetId,
    /// The sim ground from the cooked heightfield and the level's height mapping.
    pub ground: GroundInput,
}

/// The level sources under `assets_root`: `*.level.ron` files and `*.level.d` directories, sorted
/// (staging directories of `regia upgrade-project --write` skipped). Each is `(path, rel)`.
fn discover_levels(assets_root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(assets_root)
        .into_iter()
        .filter_entry(|e| !crate::stream_cook::is_upgrade_stage_dir(e))
        .filter_map(Result::ok)
    {
        let name = entry.file_name().to_str().unwrap_or("");
        let is_level = (entry.file_type().is_file() && name.ends_with(".level.ron"))
            || (entry.file_type().is_dir() && name.ends_with(".level.d"));
        if !is_level {
            continue;
        }
        let path = entry.path().to_path_buf();
        let rel = path
            .strip_prefix(assets_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((path, rel));
    }
    out.sort();
    out
}

/// The level's terrain block, `None` when it names none. Reads the settings text only when it
/// mentions a terrain (most levels do not).
fn level_terrain(path: &Path) -> Result<Option<LevelTerrain>, LevelMeshLayersError> {
    let file = if path.is_dir() {
        path.join("level.ron")
    } else {
        path.to_path_buf()
    };
    let text = std::fs::read_to_string(&file)
        .map_err(|e| LevelMeshLayersError::LevelRead(format!("{}: {e}", file.display())))?;
    if !text.contains("terrain") {
        return Ok(None);
    }
    let scene = regia_scene::level_dir::read_level_source(path)
        .map_err(|e| LevelMeshLayersError::LevelRead(format!("{}: {e}", path.display())))?;
    let Some(raw) = scene.resources.get(LEVEL_SETTINGS_PATH) else {
        return Ok(None);
    };
    let settings = LevelSettings::from_ron(raw.get_ron())
        .map_err(|e| LevelMeshLayersError::LevelRead(format!("{}: settings: {e}", path.display())))?;
    Ok(settings.terrain)
}

/// The job of one level, `None` when it names no terrain or its map has no `.meshlayers.ron`.
///
/// # Errors
///
/// [`LevelMeshLayersError`] for a level whose map has a layers file but whose ground does not read.
pub fn level_job(
    assets_root: &Path,
    cache_dir: &Path,
    level_path: &Path,
    level: &str,
) -> Result<Option<LevelMeshLayersJob>, LevelMeshLayersError> {
    let Some(block) = level_terrain(level_path)? else {
        return Ok(None);
    };
    let files = LevelTerrainFiles::resolve(assets_root, level, Some(&block))?;
    let source = meshlayers_source_path(&files.map);
    if !source.is_file() {
        return Ok(None);
    }
    let Some(ground) = load_level_ground(assets_root, cache_dir, level, Some(&block))? else {
        return Err(LevelMeshLayersError::NoHeightfield {
            source_file: source.display().to_string(),
        });
    };
    let hf = &ground.heightfield;
    if hf.width != hf.height {
        return Err(LevelMeshLayersError::GroundNotSquare {
            width: hf.width,
            height: hf.height,
        });
    }
    let (width_m, offset_m, step_m) = sim_ground_mapping(&ground, &block)?;
    Ok(Some(LevelMeshLayersJob {
        level: level.to_owned(),
        out_file: files.meshlayers,
        source,
        map_id: ground.map_id,
        ground: GroundInput {
            lattice_n: hf.width,
            width_m,
            offset_m,
            step_m,
            counts: hf.samples.clone(),
        },
    }))
}

/// The sim ground's `(width_m, offset_m, step_m)` (ADR-8842). Without a count mapping: the level's
/// width, offset and `height_scale_m / 65535`, as before. With one (ADR-8471): the run-time map's
/// width (`(n - 1) * cell_m`), `-zero_counts * meters_per_count` and `meters_per_count`, after the
/// run-time load's own checks ([`regia_terrain::LevelGround::mapped_map`]); a grid whose center is
/// not the origin is refused, because the canonical lattice is centered on the origin.
fn sim_ground_mapping(
    ground: &regia_terrain::LevelGround,
    block: &LevelTerrain,
) -> Result<(f32, f32, f32), LevelMeshLayersError> {
    let (Some(mapping), Some(map)) = (ground.source_mapping, ground.mapped_map(block)?) else {
        // ADR-10079: the lattice is vertex-centered, so a cell-centered level hands it the span
        // between its outer samples, where the run-time map puts them.
        return Ok((
            ground.sample_span_m(),
            ground.height_offset_m,
            ground.height_scale_m / 65535.0,
        ));
    };
    let center = map.grid_origin();
    if center != [0.0, 0.0] {
        return Err(LevelMeshLayersError::GroundOffset {
            center,
            sample0: mapping.sample0_m,
            centered_sample0: -0.5 * map.world_width(),
        });
    }
    Ok((map.world_width(), map.height_offset(), mapping.meters_per_count))
}

/// Reads a file a `.meshlayers.ron` names, relative to the document's directory. An absolute path
/// or one with `..` is refused, so a document cannot read outside its folder.
fn read_named(dir: &Path, path: &str) -> Result<Vec<u8>, String> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    {
        return Err(format!(
            "{path}: must be relative to the layers file, without `..`"
        ));
    }
    std::fs::read(dir.join(rel)).map_err(|e| format!("{path}: {e}"))
}

/// Cooks one job: the same call a host makes with [`cook_mesh_layers`], with the ground's Stage A
/// sampler, the default CDLOD config, a centered map and `manifest`'s materials.
///
/// # Errors
///
/// [`LevelMeshLayersError::Cook`], or a layers file that does not read.
pub fn cook_level_job(
    job: &LevelMeshLayersJob,
    cache_dir: &Path,
    manifest: &[ManifestEntry],
) -> Result<MeshLayersCookOutcome, LevelMeshLayersError> {
    let _span = tracing::info_span!("meshlayers_level_cook", level = %job.level).entered();
    let document = std::fs::read(&job.source).map_err(|e| LevelMeshLayersError::Cook {
        error: crate::terrain_mesh_layers::MeshLayersCookError::Io {
            path: job.source.clone(),
            reason: e.to_string(),
        },
    })?;
    let dir = job.source.parent().unwrap_or_else(|| Path::new("."));
    let read = |p: &str| read_named(dir, p);
    let sampler = stage_a_sampler(&job.ground);
    let resolve = material_resolver(manifest);
    cook_mesh_layers(&MeshLayersCookRequest {
        document: &document,
        ground: &job.ground,
        terrain: sampler.as_ref(),
        read_file: &read,
        map_id: job.map_id,
        out_file: &job.out_file,
        cooked_dir: cache_dir,
        cdlod: CdlodConfig::default(),
        origin: [0.0, 0.0],
        resolve_material: &resolve,
    })
    .map_err(|error| LevelMeshLayersError::Cook { error })
}

/// The manifest rows of a cooked presentation: one per chunk LOD, under `source_rel`
/// (`assets/<map stem>.meshlayers.ron`).
#[must_use]
pub fn chunk_manifest_rows(source_rel: &str, outcome: &MeshLayersCookOutcome) -> Vec<ManifestEntry> {
    let mut rows = Vec::new();
    for layer in &outcome.presentation.layers {
        for c in &layer.chunks {
            for (i, lod) in c.lods.iter().enumerate() {
                rows.push(ManifestEntry {
                    path: format!(
                        "{source_rel}#{}",
                        chunk_lod_asset_path(&layer.id, c.x, c.z, i as u8)
                    ),
                    id: lod.mesh,
                    type_path: CHUNK_TYPE_PATH.to_owned(),
                });
            }
        }
    }
    rows
}

/// The `regia cook` pass (ADR-8451): cooks the mesh layers of every level that has them and
/// replaces their chunk rows in `manifest`. A level that fails is an error of the report keyed by
/// its path; other levels still cook.
pub(crate) fn cook_project_mesh_layers(
    assets_root: &Path,
    cache_dir: &Path,
    manifest: &mut BTreeMap<String, ManifestEntry>,
    report: &mut crate::cook::CookReport,
) {
    let _span = tracing::info_span!("meshlayers_project_cook").entered();
    // One job per layers file, the first level (path order) that names it; a disagreeing level
    // is a conflict and the map is not cooked.
    let mut jobs: BTreeMap<PathBuf, Result<LevelMeshLayersJob, ()>> = BTreeMap::new();
    for (path, level) in discover_levels(assets_root) {
        let job = match level_job(assets_root, cache_dir, &path, &level) {
            Ok(Some(job)) => job,
            Ok(None) => continue,
            Err(e) => {
                report.errors.push((format!("assets/{level}"), e.report_line()));
                continue;
            }
        };
        match jobs.get_mut(&job.source) {
            None => {
                jobs.insert(job.source.clone(), Ok(job));
            }
            Some(Ok(first)) if first.map_id == job.map_id && first.ground == job.ground => {}
            Some(slot) => {
                let first = match slot {
                    Ok(first) => first.level.clone(),
                    Err(()) => String::from("an earlier level"),
                };
                let e = LevelMeshLayersError::Conflict {
                    source_file: job.source.display().to_string(),
                    first,
                    second: job.level.clone(),
                };
                report
                    .errors
                    .push((format!("assets/{}", job.level), e.report_line()));
                *slot = Err(());
            }
        }
    }
    if jobs.is_empty() {
        return;
    }
    let rows: Vec<ManifestEntry> = manifest.values().cloned().collect();
    for job in jobs.into_values().flatten() {
        let source_rel = job
            .source
            .strip_prefix(assets_root)
            .map(|p| format!("assets/{}", p.to_string_lossy().replace('\\', "/")))
            .unwrap_or_else(|_| job.source.display().to_string());
        match cook_level_job(&job, cache_dir, &rows) {
            Ok(outcome) => {
                if outcome.cooked {
                    report.cooked += 1;
                } else {
                    report.skipped += 1;
                }
                let prefix = format!("{source_rel}#meshlayers/");
                manifest.retain(|path, _| !path.starts_with(&prefix));
                for row in chunk_manifest_rows(&source_rel, &outcome) {
                    manifest.insert(row.path.clone(), row);
                }
            }
            Err(e) => report
                .errors
                .push((format!("assets/{}", job.level), e.report_line())),
        }
    }
}
