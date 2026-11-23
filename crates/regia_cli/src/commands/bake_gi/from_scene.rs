// SPDX-License-Identifier: MPL-2.0
//! `regia bake-gi --from-scene <level>`: turns a map into its bake input (ADR-310,
//! `book/agents/baked-gi-export.md`).
//!
//! The level is spawned headless through `regia_scene` (no renderer, no window) and exported by
//! `regia_agent::bake_export`, the same code the agent's `level.export_bake` runs. A DO27 bake
//! bundle (`--from-static-meshes`) is checked here with the bake's own readers
//! (`regia_pt::bake_meshes`) and named in the description; the bake reads it.
//!
//! - default: write `<project>/bake/<stem>.bake.ron` and its `.bake.stamp.json`, print one JSON
//!   line (`statics`, `skipped`, `triangles`, `bounds`, `hash`, `stale`).
//! - `--check`: exit 0 when the description is current, 2 when it is stale, 1 on error.
//! - `--bake`: bake the volume right after the export (`super::bake_job`; through
//!   `--incremental`'s state when given, `super::incremental::bake_for`).
//! - `--write-level`: name the volume in the level's `look.baked_gi.volume` (scene writer,
//!   atomic rewrite).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Result, anyhow};
use clap::Args;
use regia_agent::bake_export::{
    self, BundleRef, Export, ExportError, ExportOptions, MeshInfo, MeshInfoSource, NoMeshInfo, SunOverride,
    slash,
};
use regia_core::id::Guid;
use regia_pt::bake_desc::{BakeDescError, GiBakeDescription};
use regia_pt::bake_meshes::{BakeMeshLoader, Manifest, parse_interiors_json, parse_static_meshes_json};
use regia_render::{DirectionalLight, MeshSource};
use regia_scene::level::{Level, LevelBakedGi, LevelId, LevelSettings};
use serde_json::{Value, json};

use super::{BakeGiArgs, GltfBakeLoader, apply_overrides};

/// Volume bounds `(min, max)` given on the command line.
pub type Bounds = ([f32; 3], [f32; 3]);

/// The `--from-scene` / `--from-static-meshes` arguments of `regia bake-gi`.
#[derive(Debug, Clone, Args)]
pub struct FromSceneArgs {
    /// Export the bake input of a level (`.level.ron`): its `GiMobility::Static` meshes, its sun,
    /// sky and sky bodies, and a probe layout.
    #[arg(long, value_name = "LEVEL")]
    pub from_scene: Option<PathBuf>,
    /// Also (or only) bake a DO27 bundle's `static_meshes.json` (v1). Its directory is the
    /// bundle root; `interiors.json` and `MANIFEST.sha256` there are used when present.
    #[arg(long, value_name = "FILE")]
    pub from_static_meshes: Option<PathBuf>,
    /// The bundle's `interiors.json` (default: next to `static_meshes.json`, when present).
    #[arg(long, value_name = "FILE", requires = "from_static_meshes")]
    pub interiors: Option<PathBuf>,
    /// Project root, which mesh paths are relative to (default: the nearest ancestor of the
    /// level with a `Regia.toml`, else the directory above its `assets/`).
    #[arg(long, value_name = "DIR")]
    pub project: Option<PathBuf>,
    /// Where to write the description (default `<project>/bake/<volume stem>.bake.ron`).
    #[arg(long, value_name = "FILE")]
    pub desc: Option<PathBuf>,
    /// Terrain mesh rasterization spacing, metres (DO27's terrain grid is 1.25).
    #[arg(long, default_value_t = 1.0)]
    pub terrain_spacing: f32,
    /// Volume bounds `minx,miny,minz,maxx,maxy,maxz` instead of the statics' bounds + 4 m.
    #[arg(long, value_parser = parse_bounds, allow_hyphen_values = true)]
    pub bounds: Option<Bounds>,
    /// A sun `elevation_deg,azimuth_deg,lux` replacing the level's clock and directional light
    /// (azimuth clockwise from north = +Z, +X east; for a DO27 `map_look.json` sun, which is in
    /// the sim frame, use 360 − azimuth).
    #[arg(long, value_parser = parse_sun, allow_hyphen_values = true)]
    pub sun: Option<SunOverride>,
    /// Exit 0 when the description is current, 2 when it is stale (writes nothing).
    #[arg(long, conflicts_with_all = ["bake", "write_level"])]
    pub check: bool,
    /// Bake the volume after the export (`--out`, else the level's `look.baked_gi.volume` under
    /// `assets/`, else `assets/maps/<level stem>.fgiv`).
    #[arg(long)]
    pub bake: bool,
    /// Name the volume in the level's `look.baked_gi.volume` (an atomic rewrite through the
    /// scene writer).
    #[arg(long, requires = "from_scene")]
    pub write_level: bool,
}

impl Default for FromSceneArgs {
    fn default() -> Self {
        Self {
            from_scene: None,
            from_static_meshes: None,
            interiors: None,
            project: None,
            desc: None,
            terrain_spacing: 1.0,
            bounds: None,
            sun: None,
            check: false,
            bake: false,
            write_level: false,
        }
    }
}

impl FromSceneArgs {
    /// Whether the command exports (a level or a bundle was given).
    #[must_use]
    pub fn active(&self) -> bool {
        self.from_scene.is_some() || self.from_static_meshes.is_some()
    }
}

/// Parses `minx,miny,minz,maxx,maxy,maxz`.
///
/// # Errors
///
/// Not six finite numbers, or a maximum below its minimum.
pub fn parse_bounds(text: &str) -> Result<Bounds, String> {
    let v: Vec<f32> = text
        .split(',')
        .map(|s| s.trim().parse::<f32>().map_err(|e| format!("`{s}`: {e}")))
        .collect::<Result<_, _>>()?;
    if v.len() != 6 || v.iter().any(|x| !x.is_finite()) {
        return Err("expected six numbers: minx,miny,minz,maxx,maxy,maxz".into());
    }
    let (min, max) = ([v[0], v[1], v[2]], [v[3], v[4], v[5]]);
    if (0..3).any(|k| max[k] <= min[k]) {
        return Err("each maximum must exceed its minimum".into());
    }
    Ok((min, max))
}

/// Parses `elevation_deg,azimuth_deg,lux`.
///
/// # Errors
///
/// Not three finite numbers, an elevation outside −90..=90 or a negative illuminance.
pub fn parse_sun(text: &str) -> Result<SunOverride, String> {
    let v: Vec<f32> = text
        .split(',')
        .map(|s| s.trim().parse::<f32>().map_err(|e| format!("`{s}`: {e}")))
        .collect::<Result<_, _>>()?;
    let [elevation_deg, azimuth_deg, lux] = v[..] else {
        return Err("expected three numbers: elevation_deg,azimuth_deg,lux".into());
    };
    if !(elevation_deg.is_finite() && azimuth_deg.is_finite() && lux.is_finite()) {
        return Err("expected finite numbers".into());
    }
    if !(-90.0..=90.0).contains(&elevation_deg) || lux < 0.0 {
        return Err("elevation must be within -90..=90 and lux not negative".into());
    }
    Ok(SunOverride {
        elevation_deg,
        azimuth_deg,
        lux,
    })
}

/// A command error: the code (an export or bake description code) and what it concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FromSceneError {
    /// Error code (`export_*`, or the bake's `bake_*` codes for a bundle).
    pub code: &'static str,
    /// The entity, file or key path it concerns.
    pub entity: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for FromSceneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.entity {
            Some(e) => write!(f, "{e}: {} [{}]", self.message, self.code),
            None => write!(f, "{} [{}]", self.message, self.code),
        }
    }
}

impl std::error::Error for FromSceneError {}

impl From<ExportError> for FromSceneError {
    fn from(e: ExportError) -> Self {
        Self {
            code: e.code,
            entity: e.entity,
            message: e.message,
        }
    }
}

impl From<BakeDescError> for FromSceneError {
    fn from(e: BakeDescError) -> Self {
        Self {
            code: e.code,
            entity: e.path,
            message: e.message,
        }
    }
}

impl FromSceneError {
    fn new(code: &'static str, entity: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            entity,
            message: message.into(),
        }
    }

    /// `{"ok": false, "code", "entity", "message"}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "ok": false, "code": self.code, "entity": self.entity, "message": self.message })
    }
}

/// Mesh bounds through the bake's own loader ([`GltfBakeLoader`]): every part's positions.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoaderMeshInfo;

impl MeshInfoSource for LoaderMeshInfo {
    fn info(&mut self, path: &Path) -> Result<Option<MeshInfo>, String> {
        let source = GltfBakeLoader.load(path)?;
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut triangles = 0u64;
        for part in &source.parts {
            triangles += part.mesh.triangle_count() as u64;
            for p in &part.mesh.positions {
                min = std::array::from_fn(|k| min[k].min(p[k]));
                max = std::array::from_fn(|k| max[k].max(p[k]));
            }
        }
        Ok((min[0] <= max[0]).then_some(MeshInfo { min, max, triangles }))
    }
}

/// The level file being exported.
#[derive(Clone, Debug)]
pub struct LevelFile {
    /// Its path.
    pub path: PathBuf,
    /// The level as loaded (migrated to the current scene format).
    pub level: Level,
}

/// What [`prepare`] computed: the export and where it goes.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The export (text, input hash, summary).
    pub export: Export,
    /// The description path (absolute).
    pub desc: PathBuf,
    /// The project root (absolute).
    pub project: PathBuf,
    /// The export options (written into the stamp).
    pub options: ExportOptions,
    /// The level, when `--from-scene` was given.
    pub level: Option<LevelFile>,
    /// The volume's path under `assets/` (`maps/<stem>.fgiv`).
    pub volume: String,
    /// Spawn warnings and other notes.
    pub warnings: Vec<String>,
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The project root of `start` (a file or directory): the nearest ancestor holding a
/// `Regia.toml`, else the directory above the nearest `assets` ancestor, else `start`'s
/// directory.
#[must_use]
pub fn find_project(start: &Path) -> PathBuf {
    let start = absolute(start);
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map_or_else(|| start.clone(), Path::to_path_buf)
    };
    if let Some(found) = dir.ancestors().find(|a| regia_core::brand::is_project_root(a)) {
        return found.to_path_buf();
    }
    if let Some(assets) = dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "assets"))
        && let Some(parent) = assets.parent()
    {
        return parent.to_path_buf();
    }
    dir
}

/// Reads and parses the level at `path`, a `.level.ron` or a `.level.d` directory.
///
/// # Errors
///
/// `export_read_failed`, `export_level_parse`.
pub fn read_level(path: &Path) -> Result<LevelFile, FromSceneError> {
    let label = Some(path.display().to_string());
    let level = if path.is_dir() {
        let (scene, _) = regia_scene::level_dir::read_level_dir(path)
            .map_err(|e| FromSceneError::new("export_level_parse", label.clone(), e.to_string()))?;
        // As `Level::from_ron_str` does for a file: bad settings are an error, not the defaults.
        if let Some(raw) = scene.resources.get(regia_scene::level::LEVEL_SETTINGS_PATH) {
            regia_scene::level::LevelSettings::from_ron(raw.get_ron())
                .map_err(|e| FromSceneError::new("export_level_parse", label, e.to_string()))?;
        }
        Level::from_scene_file(LevelId(Guid::NIL), scene)
    } else {
        let text = std::fs::read_to_string(path)
            .map_err(|e| FromSceneError::new("export_read_failed", label.clone(), e.to_string()))?;
        Level::from_ron_str(LevelId(Guid::NIL), &text)
            .map_err(|e| FromSceneError::new("export_level_parse", label, e.to_string()))?
    };
    Ok(LevelFile {
        path: path.to_path_buf(),
        level,
    })
}

/// Checks a DO27 bundle with the bake's readers (header, required fields, the manifest hash of
/// the files read here) and returns what the description names. The terrain is the `terrain`
/// layer entry whose source file is `terrain.*` (else the first `terrain` entry).
fn read_bundle(
    static_meshes: &Path,
    interiors: Option<&Path>,
    summary: &mut Value,
) -> Result<BundleRef, FromSceneError> {
    let static_meshes = absolute(static_meshes);
    let root = static_meshes.parent().map(Path::to_path_buf).ok_or_else(|| {
        FromSceneError::new("export_read_failed", None, "static_meshes.json has no directory")
    })?;
    let rel = |p: &Path| -> String { p.strip_prefix(&root).map_or_else(|_| slash(p), slash) };
    let manifest_path = root.join("MANIFEST.sha256");
    let mut manifest = if manifest_path.is_file() {
        Some(Manifest::load(&manifest_path, &root, "manifest")?)
    } else {
        None
    };
    let mut read = |path: &Path, key: &str| -> Result<String, FromSceneError> {
        if let Some(m) = manifest.as_mut() {
            m.verify(path, key)?;
        }
        std::fs::read_to_string(path)
            .map_err(|e| FromSceneError::new("export_read_failed", Some(slash(path)), e.to_string()))
    };
    let text = read(&static_meshes, "static_meshes")?;
    let import = parse_static_meshes_json(&text, "static_meshes.json")?;
    let interiors = match interiors {
        Some(p) => Some(absolute(p)),
        None => Some(root.join("interiors.json")).filter(|p| p.is_file()),
    };
    let mut interior_count = 0;
    if let Some(path) = &interiors {
        let text = read(path, "interiors_file")?;
        interior_count = parse_interiors_json(&text, "interiors.json")?.len();
    }
    let terrains: Vec<&regia_pt::bake_desc::MeshDesc> = import
        .meshes
        .iter()
        .filter(|m| m.layer.as_deref() == Some("terrain"))
        .collect();
    let terrain = terrains
        .iter()
        .find(|m| {
            m.source
                .file_stem()
                .is_some_and(|s| s.eq_ignore_ascii_case("terrain"))
        })
        .or(terrains.first())
        .map(|m| slash(&m.source));
    summary["bundle_entries"] = json!(import.entries);
    summary["bundle_placed"] = json!(import.meshes.len());
    summary["bundle_skipped"] = json!(import.skipped.len());
    summary["bundle_interiors"] = json!(interior_count);
    summary["bundle_manifest_verified"] = json!(manifest.as_ref().map_or(0, Manifest::verified));
    Ok(BundleRef {
        root: slash(&root),
        static_meshes: rel(&static_meshes),
        interiors_file: interiors.as_deref().map(rel),
        manifest: manifest.is_some().then(|| "MANIFEST.sha256".to_string()),
        terrain_source: terrain,
    })
}

/// Loads the level and the bundle and computes the export (nothing is written).
///
/// # Errors
///
/// A [`FromSceneError`] with its code.
pub fn prepare(args: &BakeGiArgs, mesh_info: &mut dyn MeshInfoSource) -> Result<Prepared, FromSceneError> {
    let a = &args.from_scene;
    let level = a.from_scene.as_deref().map(read_level).transpose()?;
    // A bundle alone anchors at the working directory: the bundle itself is read-only.
    let anchor = level
        .as_ref()
        .map_or_else(|| PathBuf::from("."), |l| l.path.clone());
    let project = a
        .project
        .as_deref()
        .map_or_else(|| find_project(&anchor), absolute);
    let mut warnings = Vec::new();

    let (statics, settings) = match &level {
        Some(l) => {
            let mut headless = regia_scene::bake_query::spawn_headless(&l.level.scene, |r| {
                r.register::<MeshSource>();
                r.register::<DirectionalLight>();
            })
            .map_err(|e| FromSceneError::new("export_level_parse", Some(slash(&l.path)), e.to_string()))?;
            warnings.append(&mut headless.warnings);
            let statics = bake_export::collect_statics(&mut headless.world)?;
            (statics, l.level.settings.clone())
        }
        None => {
            warnings
                .push("no level: no sun, sky or sky bodies (give --from-scene for the map's look)".into());
            (bake_export::SceneStatics::default(), LevelSettings::default())
        }
    };

    let mut extra = json!({});
    let bundle = match &a.from_static_meshes {
        Some(file) => Some(read_bundle(file, a.interiors.as_deref(), &mut extra)?),
        None => None,
    };
    let terrain_holes = level.as_ref().and_then(|l| {
        let holes = regia_terrain::holes_ron_path(&absolute(&l.path));
        holes
            .is_file()
            .then(|| holes.strip_prefix(&project).map_or_else(|_| slash(&holes), slash))
    });
    let level_stem = level
        .as_ref()
        .map(|l| bake_export::file_stem_to_first_dot(&slash(&l.path)));
    let named_volume = level
        .as_ref()
        .and_then(|l| l.level.settings.look.baked_gi.as_ref())
        .map(|b| b.volume.clone())
        .filter(|v| !v.is_empty());
    let stem = named_volume
        .as_deref()
        .map(bake_export::file_stem_to_first_dot)
        .or(level_stem)
        .or_else(|| {
            bundle
                .as_ref()
                .map(|b| bake_export::file_stem_to_first_dot(&b.root))
        })
        .unwrap_or_else(|| "volume".to_string());
    let volume = named_volume.unwrap_or_else(|| format!("maps/{stem}.fgiv"));
    let desc = a
        .desc
        .as_deref()
        .map_or_else(|| bake_export::default_desc_path(&project, &stem), absolute);
    if let Some(b) = &bundle
        && desc.starts_with(&b.root)
    {
        return Err(FromSceneError::new(
            "export_path_invalid",
            Some(slash(&desc)),
            "the description would be written into the bundle, which is read-only: give --desc or --project",
        ));
    }
    let options = ExportOptions {
        terrain_spacing: a.terrain_spacing,
        bounds: a.bounds,
        bundle,
        terrain_holes,
        sun: a.sun,
    };
    let mut export = bake_export::export(&statics, &settings, &project, &desc, &options, mesh_info)?;
    if let (Value::Object(summary), Value::Object(more)) = (&mut export.summary, extra) {
        summary.extend(more);
    }
    Ok(Prepared {
        export,
        desc,
        project,
        options,
        level,
        volume,
        warnings,
    })
}

/// A failed `--bake` as the one error line: a checkpoint error keeps its own code and path
/// (`bake_checkpoint_*`, `bake_interrupted`); anything else is `export_bake_failed`.
fn bake_failure(e: &anyhow::Error, label: &str) -> FromSceneError {
    match e.downcast_ref::<super::CheckpointFailure>() {
        Some(f) => FromSceneError::new(f.code, Some(f.path.clone()), f.message.clone()),
        None => FromSceneError::new("export_bake_failed", Some(label.to_owned()), format!("{e:#}")),
    }
}

fn print_error(e: &FromSceneError) {
    println!("{}", e.to_json());
}

/// `--check`: one JSON line; exit 0 (current), 2 (stale) or 1 (error).
pub fn check(args: &BakeGiArgs) -> ExitCode {
    match check_status(args) {
        Ok((stale, line)) => {
            println!("{line}");
            if stale {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            print_error(&e);
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the description is stale, and the JSON line `--check` prints. Mesh files are hashed,
/// not loaded (the input hash does not depend on their bounds).
///
/// # Errors
///
/// What [`prepare`] reports.
pub fn check_status(args: &BakeGiArgs) -> Result<(bool, Value), FromSceneError> {
    if !args.from_scene.active() {
        return Err(FromSceneError::new(
            "export_no_input",
            None,
            "--check needs --from-scene or --from-static-meshes",
        ));
    }
    let prepared = prepare(args, &mut NoMeshInfo)?;
    let reason = bake_export::staleness(&prepared.export, &prepared.desc);
    let line = json!({
        "ok": true,
        "check": true,
        "desc": slash(&prepared.desc),
        "hash": prepared.export.input_hash,
        "stale": reason.is_some(),
        "reason": reason,
    });
    Ok((reason.is_some(), line))
}

/// Names `volume` in the level's `look.baked_gi.volume` (keeping its overrides) and rewrites the
/// level through the scene writer, atomically; nothing is written when it already does. The file
/// is rewritten in the current scene format (canonical text; comments are not kept). Returns
/// whether the file changed.
///
/// # Errors
///
/// `export_write_failed`, or `export_level_parse` when the rewritten file does not load back.
pub fn write_level(level: &LevelFile, volume: &str) -> Result<bool, FromSceneError> {
    let mut updated = level.level.clone();
    let current = updated.settings.look.baked_gi.clone().unwrap_or_default();
    if current.volume == volume {
        return Ok(false);
    }
    updated.settings.look.baked_gi = Some(LevelBakedGi {
        volume: volume.to_string(),
        settings: current.settings,
    });
    let label = Some(slash(&level.path));
    let text = updated
        .to_ron_string()
        .map_err(|e| FromSceneError::new("export_write_failed", label.clone(), e.to_string()))?;
    let back = Level::from_ron_str(updated.id, &text)
        .map_err(|e| FromSceneError::new("export_level_parse", label.clone(), e.to_string()))?;
    if back.settings.look.baked_gi.as_ref().map(|b| b.volume.as_str()) != Some(volume) {
        return Err(FromSceneError::new(
            "export_level_parse",
            label,
            "the rewritten level does not name the volume",
        ));
    }
    bake_export::write_atomic(&level.path, text.as_bytes())
        .map_err(|e| FromSceneError::new("export_write_failed", label, e.to_string()))?;
    Ok(true)
}

/// The volume's file and its path under `assets/`: `--out` (named under `assets/` when it lies
/// there), else `<project>/assets/<volume>`. The level names `<volume>`, so it must stay under
/// `assets/` (no absolute, drive or UNC path, no `..`, no symlink out; ADR-1816) — `--out` is the
/// user's own argument and may point anywhere.
fn volume_target(args: &BakeGiArgs, prepared: &Prepared) -> Result<(PathBuf, String), FromSceneError> {
    let assets = prepared.project.join("assets");
    match &args.out {
        Some(out) => {
            let out = absolute(out);
            let rel = out
                .strip_prefix(&assets)
                .map_or_else(|_| prepared.volume.clone(), slash);
            Ok((out, rel))
        }
        None => {
            let file = regia_core::project_path::resolve_write(
                &prepared.project,
                &format!("assets/{}", prepared.volume),
            )
            .map_err(|e| {
                FromSceneError::new(
                    "export_path_invalid",
                    None,
                    format!(
                        "the level's baked_gi.volume `{}` leaves assets/ ({}): {e}",
                        prepared.volume,
                        e.code()
                    ),
                )
            })?;
            Ok((file, prepared.volume.clone()))
        }
    }
}

/// Runs `regia bake-gi --from-scene` / `--from-static-meshes` (not `--check`).
///
/// # Errors
///
/// An export, write, level or bake error (printed as one JSON line first).
pub fn run(args: &BakeGiArgs) -> Result<()> {
    let started = Instant::now();
    let result = run_inner(args, &mut LoaderMeshInfo, started);
    match result {
        Ok(line) => {
            println!("{line}");
            Ok(())
        }
        Err(e) => {
            print_error(&e);
            Err(anyhow!(e))
        }
    }
}

/// [`run`] with the mesh bounds from `mesh_info`; returns the summary line.
///
/// # Errors
///
/// As [`run`].
pub fn run_inner(
    args: &BakeGiArgs,
    mesh_info: &mut dyn MeshInfoSource,
    started: Instant,
) -> Result<Value, FromSceneError> {
    let prepared = prepare(args, mesh_info)?;
    let mut summary = prepared.export.summary.clone();
    summary["ok"] = json!(true);
    summary["desc"] = json!(slash(&prepared.desc));
    summary["warnings"] = json!(prepared.warnings);
    if args.dry_run {
        // Validate and lay out the exported description without writing or tracing.
        let job = build_job(args, &prepared)?;
        summary["dry_run"] = json!(true);
        summary["stale"] = json!(bake_export::staleness(&prepared.export, &prepared.desc).is_some());
        summary["layers"] = super::layer_summary(&job, None);
        summary["plan"] = serde_json::to_value(&job.plan).unwrap_or(Value::Null);
        return Ok(summary);
    }
    let written = bake_export::write_export(&prepared.export, &prepared.desc, &prepared.options)?;
    summary["stale"] = json!(written);
    let (volume_file, volume_rel) = volume_target(args, &prepared)?;
    // Bake before naming the volume in the level: a failed bake leaves the level as it was, not
    // naming a file that does not exist (ADR-365).
    if args.from_scene.bake {
        let job = build_job(args, &prepared)?;
        let built = started.elapsed().as_secs_f64();
        let label = slash(&prepared.desc);
        let bake = super::incremental::bake_for(args, &job, &volume_file, &label, started, built)
            .map_err(|e| bake_failure(&e, &label))?;
        bake_export::record_volume(&prepared.desc, &volume_rel, &prepared.export.input_hash)?;
        summary["volume"] = json!(volume_rel);
        summary["bake"] = bake;
    }
    if args.from_scene.write_level
        && let Some(level) = &prepared.level
    {
        summary["level_written"] = json!(write_level(level, &volume_rel)?);
        summary["volume"] = json!(volume_rel);
    }
    Ok(summary)
}

/// Parses the exported text with the bake's reader and builds the job (meshes load through
/// [`GltfBakeLoader`]), with the command-line overrides.
fn build_job(
    args: &BakeGiArgs,
    prepared: &Prepared,
) -> Result<regia_pt::bake_desc::GiBakeJob, FromSceneError> {
    let (desc, _) = GiBakeDescription::parse_versioned(&prepared.export.text)?;
    let base = prepared.desc.parent().unwrap_or(Path::new("."));
    let mut job = if base.exists() {
        desc.build_with(base, &mut GltfBakeLoader)?
    } else {
        // `--dry-run` writes nothing, so the description's directory may not exist yet, and the
        // OS cannot resolve `bake/../assets/...` through it: resolve `..` lexically instead.
        desc.build_with(base, &mut Lexical(GltfBakeLoader))?
    };
    apply_overrides(&mut job, args);
    Ok(job)
}

/// A mesh loader that removes `.` and `..` from a path lexically before loading.
struct Lexical<L>(L);

impl<L: BakeMeshLoader> BakeMeshLoader for Lexical<L> {
    fn load(&mut self, path: &Path) -> Result<regia_pt::bake_meshes::BakeMeshSource, String> {
        self.0.load(&lexical(path))
    }

    fn companions(&self, path: &Path) -> Vec<PathBuf> {
        self.0.companions(&lexical(path))
    }
}

/// `path` with `.` dropped and each `..` removing the component before it (kept at the start of
/// a relative path), without touching the file system.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Object bounds of every mesh: a unit cube, 12 triangles.
    struct UnitBoxes;

    impl MeshInfoSource for UnitBoxes {
        fn info(&mut self, _path: &Path) -> Result<Option<MeshInfo>, String> {
            Ok(Some(MeshInfo {
                min: [-0.5; 3],
                max: [0.5; 3],
                triangles: 12,
            }))
        }
    }

    const BOX_A: u128 = 0xa1;
    const BOX_B: u128 = 0xb2;
    const BOX_DYN: u128 = 0xc3;
    const MARKER: u128 = 0xd4;
    const GROUND: u128 = 0xe5;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        regia_test_utils::write_atomic(path, bytes).expect("write");
    }

    /// The test level: two static boxes (B at `b_x`), one dynamic box, one static entity without
    /// a mesh and a small terrain mesh, under an airless noon sun; it names `maps/yard.fgiv`
    /// unless `volume` is false.
    fn level_text(b_x: f32, volume: bool, box_mesh: &str) -> String {
        let id = Guid::from_u128;
        let mesh = |m: &str| format!(r#""regia_render::runtime::MeshSource": (mesh: "{m}", material: ""),"#);
        let baked = if volume {
            r#", look: (baked_gi: Some((volume: "maps/yard.fgiv")))"#
        } else {
            ""
        };
        format!(
            r#"SceneFile(
    format_version: 10,
    entities: [
        (guid: "{a}", name: "Box A", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0)),
            "regia_scene::components::GiMobility": Static,
            {mesh_a}
        }}),
        (guid: "{b}", name: "Box B", components: {{
            "regia_scene::transform::Transform": (translation: ({b_x:?}, 1.0, 0.0)),
            "regia_scene::components::GiMobility": Static,
            {mesh_b}
        }}),
        (guid: "{c}", name: "Rover", components: {{
            "regia_scene::transform::Transform": (translation: (3.0, 1.0, 3.0)),
            "regia_scene::components::GiMobility": Dynamic,
            {mesh_c}
        }}),
        (guid: "{d}", name: "Marker", components: {{
            "regia_scene::components::GiMobility": Static,
        }}),
        (guid: "{e}", name: "Ground", components: {{
            "regia_scene::components::GiMobility": Static,
            "regia_scene::components::Tags": (["terrain"]),
            {mesh_t}
        }}),
    ],
    resources: {{
        "regia_scene::level::LevelSettings": (atmosphere: (preset: Airless, sun_illuminance_lux: Some(4400.0), time_of_day: Some((hour: 12.0, day_of_year: 172, latitude_deg: 40.0))){baked}),
    }},
)
"#,
            a = id(BOX_A),
            b = id(BOX_B),
            c = id(BOX_DYN),
            d = id(MARKER),
            e = id(GROUND),
            mesh_a = mesh(box_mesh),
            mesh_b = mesh(box_mesh),
            mesh_c = mesh(box_mesh),
            mesh_t = mesh("assets/meshes/ground.gltf"),
        )
    }

    #[test]
    fn lexical_paths_drop_dots_and_parents() {
        assert_eq!(
            lexical(Path::new("/p/bake/../assets/./m.gltf")),
            PathBuf::from("/p/assets/m.gltf")
        );
        assert_eq!(lexical(Path::new("bake/../../x")), PathBuf::from("../x"));
        assert_eq!(lexical(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(lexical(Path::new("a/b")), PathBuf::from("a/b"));
    }

    /// A project with `Regia.toml`, the two mesh files and the level.
    fn project(tag: &str) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        write(&dir.join("Regia.toml"), b"[project]\nname = \"yard\"\n");
        write(&dir.join("assets/meshes/box.gltf"), b"box mesh bytes");
        write(&dir.join("assets/meshes/ground.gltf"), b"ground mesh bytes");
        let level = dir.join("assets/levels/yard.level.ron");
        write(&level, level_text(5.0, true, "assets/meshes/box.gltf").as_bytes());
        (dir, level)
    }

    fn args_for(level: &Path) -> BakeGiArgs {
        let mut args = super::super::tests::args("");
        args.from_scene = FromSceneArgs {
            from_scene: Some(level.to_path_buf()),
            ..FromSceneArgs::default()
        };
        args
    }

    fn export_once(args: &BakeGiArgs) -> Value {
        run_inner(args, &mut UnitBoxes, Instant::now()).expect("export")
    }

    /// A checkpoint error under `--from-scene --bake` keeps its code and path (it was printed as
    /// a second JSON document and the error line said `export_bake_failed`): `bake_for` returns it
    /// unprinted and the error line carries `bake_checkpoint_io`.
    #[test]
    fn audit_a_checkpoint_error_keeps_its_code_in_the_from_scene_line() {
        let dir = regia_test_utils::unique_temp_dir("bake_export_ck_code");
        let mut a = super::super::tests::args("furnace");
        a.cpu = true;
        let missing = dir.join("missing.bakeck");
        a.checkpoint.resume = Some(missing.clone());
        let j = super::super::job(&a).expect("furnace");
        let e =
            super::super::incremental::bake_for(&a, &j, &dir.join("v.fgiv"), "furnace", Instant::now(), 0.0)
                .expect_err("no checkpoint there");
        let failure = e
            .downcast_ref::<super::super::CheckpointFailure>()
            .expect("a typed checkpoint failure");
        assert_eq!(failure.code, "bake_checkpoint_io");
        assert!(e.to_string().starts_with("bake_checkpoint_io"), "{e}");
        let line = bake_failure(&e, "bake/yard.bake.ron");
        assert_eq!(line.code, "bake_checkpoint_io");
        assert_eq!(
            line.entity.as_deref(),
            Some(missing.display().to_string().as_str())
        );
        let other = bake_failure(&anyhow::anyhow!("boom"), "bake/yard.bake.ron");
        assert_eq!(other.code, "export_bake_failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed `--bake` leaves the level as it was: `--write-level` runs after the bake, so the
    /// level never names a volume that was not written (ADR-365). The test meshes are not glTF,
    /// so the bake fails while loading them, before any trace.
    #[test]
    fn a_failed_bake_does_not_rewrite_the_level() {
        let (dir, level) = project("bake_export_failed_bake");
        write(
            &level,
            level_text(5.0, false, "assets/meshes/box.gltf").as_bytes(),
        );
        let before = std::fs::read(&level).expect("level");
        let mut args = args_for(&level);
        args.cpu = true;
        args.from_scene.bake = true;
        args.from_scene.write_level = true;
        let err = run_inner(&args, &mut UnitBoxes, Instant::now()).expect_err("the meshes are not glTF");
        assert_eq!(err.code, "bake_mesh_load", "{err}");
        assert_eq!(
            std::fs::read(&level).expect("level"),
            before,
            "the level is untouched"
        );
        assert!(!dir.join("assets/maps/yard.fgiv").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two static boxes export as two meshes; the dynamic box and the mesh-less static entity are
    /// the two skips; the terrain becomes `terrain_mesh`; the text parses as `.bake.ron` v1.
    #[test]
    fn a_level_exports_its_static_meshes_and_counts_the_skips() {
        let (dir, level) = project("bake_export_counts");
        let args = args_for(&level);
        let line = export_once(&args);
        assert_eq!(line["ok"], true);
        assert_eq!(line["statics"], 2, "{line}");
        assert_eq!(line["skipped"], 2, "{line}");
        assert_eq!(line["skipped_by"]["dynamic"], 1);
        assert_eq!(line["skipped_by"]["no_mesh"], 1);
        assert_eq!(line["triangles"], 24);
        assert_eq!(line["terrain"], "assets/meshes/ground.gltf");
        assert_eq!(line["sun_source"], "time_of_day");
        assert_eq!(line["stale"], true, "first export writes");
        assert_eq!(line["bounds"]["min"], json!([-0.5, 0.5, -0.5]));
        assert_eq!(line["bounds"]["max"], json!([5.5, 1.5, 0.5]));
        let desc = dir.join("bake/yard.bake.ron");
        assert_eq!(line["desc"], slash(&desc));
        let text = std::fs::read_to_string(&desc).expect("desc written");
        assert!(dir.join("bake/yard.bake.stamp.json").is_file());
        let (parsed, version) = GiBakeDescription::parse_versioned(&text).expect("parses as .bake.ron");
        assert_eq!(version, 1);
        parsed.validate().expect("valid");
        assert_eq!(parsed.meshes.len(), 2);
        assert_eq!(
            parsed.meshes[0].id.as_deref(),
            Some(Guid::from_u128(BOX_A).to_string().as_str())
        );
        assert_eq!(parsed.meshes[1].transform[12], 5.0);
        assert_eq!(parsed.root.as_deref(), Some(Path::new("..")));
        assert_eq!(
            parsed.terrain_mesh.as_ref().map(|t| t.source.clone()),
            Some(PathBuf::from("assets/meshes/ground.gltf"))
        );
        assert!(
            parsed.suns.len() == 1 && parsed.sky.is_none(),
            "airless noon: one sun"
        );
        assert_eq!(parsed.layout.terrain_spacing, Some([8.0, 8.0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A re-export of an unchanged level is byte-identical and writes nothing.
    #[test]
    fn a_re_export_is_byte_identical() {
        let (dir, level) = project("bake_export_identical");
        let args = args_for(&level);
        export_once(&args);
        let desc = dir.join("bake/yard.bake.ron");
        let first = std::fs::read(&desc).expect("desc");
        let stamp = std::fs::read(dir.join("bake/yard.bake.stamp.json")).expect("stamp");
        let again = export_once(&args);
        assert_eq!(again["stale"], false);
        assert_eq!(std::fs::read(&desc).expect("desc"), first);
        assert_eq!(
            std::fs::read(dir.join("bake/yard.bake.stamp.json")).expect("stamp"),
            stamp
        );
        let p1 = prepare(&args, &mut UnitBoxes).expect("prepare");
        let p2 = prepare(&args, &mut UnitBoxes).expect("prepare");
        assert_eq!(p1.export.text.as_bytes(), first.as_slice());
        assert_eq!(p1.export, p2.export);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Moving one static entity makes `--check` stale (exit 2); re-exporting makes it current
    /// (exit 0). A hand edit of the description is stale too.
    #[test]
    fn moving_a_static_entity_flips_check_to_stale() {
        let (dir, level) = project("bake_export_check");
        let mut args = args_for(&level);
        assert_eq!(check_status(&args).expect("check").1["reason"], "desc_missing");
        export_once(&args);
        args.from_scene.check = true;
        let (stale, line) = check_status(&args).expect("check");
        assert!(!stale, "{line}");
        write(&level, level_text(6.0, true, "assets/meshes/box.gltf").as_bytes());
        let (stale, line) = check_status(&args).expect("check");
        assert!(stale);
        assert_eq!(line["reason"], "inputs_changed");
        args.from_scene.check = false;
        export_once(&args);
        args.from_scene.check = true;
        assert!(!check_status(&args).expect("check").0, "current after re-export");
        // A mesh file's content is an input.
        write(&dir.join("assets/meshes/box.gltf"), b"box mesh bytes, edited");
        assert!(check_status(&args).expect("check").0, "mesh content changed");
        args.from_scene.check = false;
        export_once(&args);
        // Editing the description by hand is stale as well.
        let desc = dir.join("bake/yard.bake.ron");
        let edited = std::fs::read_to_string(&desc)
            .expect("desc")
            .replace("8.0, 8.0", "4.0, 4.0");
        write(&desc, edited.as_bytes());
        args.from_scene.check = true;
        assert_eq!(check_status(&args).expect("check").1["reason"], "desc_changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A static entity whose mesh file is missing is a typed error naming the entity.
    #[test]
    fn a_missing_mesh_names_the_entity() {
        let (dir, level) = project("bake_export_missing");
        write(
            &level,
            level_text(5.0, true, "assets/meshes/gone.gltf").as_bytes(),
        );
        let err = prepare(&args_for(&level), &mut UnitBoxes).expect_err("missing mesh");
        assert_eq!(err.code, "export_mesh_missing");
        let entity = err.entity.clone().unwrap_or_default();
        assert!(entity.contains("Box A"), "{entity}");
        assert!(entity.contains(&Guid::from_u128(BOX_A).to_string()), "{entity}");
        assert_eq!(err.to_json()["code"], "export_mesh_missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--write-level` names the volume through the scene writer; the level loads back with it
    /// and keeps its entities.
    #[test]
    fn write_level_round_trips_through_the_scene_loader() {
        let (dir, level) = project("bake_export_write_level");
        write(
            &level,
            level_text(5.0, false, "assets/meshes/box.gltf").as_bytes(),
        );
        let mut args = args_for(&level);
        args.from_scene.write_level = true;
        let line = export_once(&args);
        assert_eq!(line["level_written"], true, "{line}");
        assert_eq!(line["volume"], "maps/yard.fgiv");
        let back = read_level(&level).expect("level loads back");
        let named = back.level.settings.look.baked_gi.expect("volume named");
        assert_eq!(named.volume, "maps/yard.fgiv");
        assert_eq!(back.level.scene.entities.len(), 5);
        // The input hash does not cover the volume name: still current, and a second run
        // leaves the level alone.
        args.from_scene.write_level = false;
        assert!(!check_status(&args).expect("check").0);
        args.from_scene.write_level = true;
        assert_eq!(export_once(&args)["level_written"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn bundle(dir: &Path, version: u32) -> PathBuf {
        let root = dir.join("bundle");
        let json = format!(
            r#"{{
  "header": {{ "version": {version}, "units": "metres", "up": "+Y", "handedness": "right" }},
  "meshes": [
    {{ "id": "terrain", "mesh": "terrain/terrain.tpak", "source": "terrain/terrain.gltf",
       "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "static": true, "layer": "terrain" }},
    {{ "id": "SM_Hab", "mesh": "x.gltc", "source": "setpieces/hab.gltf", "mirror_x": true,
       "transform": [-1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,150,1], "static": true, "layer": "setpiece" }},
    {{ "id": "gravel", "source": "scatter/gravel.gltf", "instances": null, "placement": {{"density": 2}},
       "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "static": false, "layer": "scatter" }}
  ]
}}"#
        );
        write(&root.join("static_meshes.json"), json.as_bytes());
        write(
            &root.join("interiors.json"),
            br#"{ "format": "do27-bake-interiors/1", "interiors": [
                { "id": "hab", "aabb_min": [-13.6, 2.7, 142.0], "aabb_max": [14.0, 16.8, 157.5] } ] }"#,
        );
        write(&root.join("terrain/terrain.gltf"), b"terrain");
        write(&root.join("setpieces/hab.gltf"), b"hab");
        write(&root.join("scatter/gravel.gltf"), b"gravel");
        root.join("static_meshes.json")
    }

    /// A DO27 bundle alone: the description names its files (root = the bundle), the terrain
    /// entry becomes `terrain_mesh`, the counts come from the bake's own reader, and the export
    /// is current until a bundle file changes.
    #[test]
    fn a_do27_static_meshes_file_is_named_and_hashed() {
        let dir = regia_test_utils::unique_temp_dir("bake_export_bundle");
        write(&dir.join("Regia.toml"), b"");
        let file = bundle(&dir, 1);
        let mut args = super::super::tests::args("");
        args.from_scene = FromSceneArgs {
            from_static_meshes: Some(file.clone()),
            project: Some(dir.clone()),
            terrain_spacing: 1.25,
            ..FromSceneArgs::default()
        };
        let line = export_once(&args);
        assert_eq!(line["bundle_entries"], 3, "{line}");
        assert_eq!(line["bundle_placed"], 2);
        assert_eq!(line["bundle_skipped"], 1);
        assert_eq!(line["bundle_interiors"], 1);
        assert_eq!(line["terrain"], "terrain/terrain.gltf");
        let desc = dir.join("bake/bundle.bake.ron");
        let text = std::fs::read_to_string(&desc).expect("desc");
        let (parsed, _) = GiBakeDescription::parse_versioned(&text).expect("parses");
        parsed.validate().expect("valid");
        assert_eq!(
            parsed.root.as_deref(),
            Some(Path::new(&slash(&dir.join("bundle"))))
        );
        assert_eq!(
            parsed.static_meshes.as_deref(),
            Some(Path::new("static_meshes.json"))
        );
        assert_eq!(
            parsed.interiors_file.as_deref(),
            Some(Path::new("interiors.json"))
        );
        assert!(parsed.manifest.is_none());
        assert_eq!(parsed.terrain_mesh.as_ref().map(|t| t.spacing), Some(1.25));
        args.from_scene.check = true;
        assert!(!check_status(&args).expect("check").0);
        write(&dir.join("bundle/setpieces/hab.gltf"), b"hab, moved");
        assert!(check_status(&args).expect("check").0, "a bundle source changed");
        // The bundle is read-only: a description inside it is refused.
        args.from_scene.desc = Some(dir.join("bundle/inside.bake.ron"));
        let err = prepare(&args, &mut UnitBoxes).expect_err("inside the bundle");
        assert_eq!(err.code, "export_path_invalid");
        assert!(!dir.join("bundle/inside.bake.ron").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bundle header is checked by the bake's reader (the error names the field), and a
    /// manifest that does not match the file read is refused.
    #[test]
    fn a_bad_bundle_header_or_manifest_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("bake_export_bundle_bad");
        let file = bundle(&dir, 2);
        let mut args = super::super::tests::args("");
        args.from_scene = FromSceneArgs {
            from_static_meshes: Some(file.clone()),
            project: Some(dir.clone()),
            ..FromSceneArgs::default()
        };
        let err = prepare(&args, &mut UnitBoxes).expect_err("version 2");
        assert_eq!(err.code, "bake_desc_invalid");
        assert_eq!(err.entity.as_deref(), Some("static_meshes.json: header.version"));
        bundle(&dir, 1);
        write(
            &dir.join("bundle/MANIFEST.sha256"),
            format!("{}  static_meshes.json\n", "0".repeat(64)).as_bytes(),
        );
        let err = prepare(&args, &mut UnitBoxes).expect_err("manifest mismatch");
        assert_eq!(err.code, "bake_manifest_mismatch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounds_parse_and_override_the_volume_brick() {
        assert_eq!(
            parse_bounds("-1,0,-2, 3,4,5"),
            Ok(([-1.0, 0.0, -2.0], [3.0, 4.0, 5.0]))
        );
        assert!(parse_bounds("1,2,3").is_err());
        assert!(parse_bounds("0,0,0,0,1,1").is_err());
        assert_eq!(
            parse_sun("24,175,4400"),
            Ok(SunOverride {
                elevation_deg: 24.0,
                azimuth_deg: 175.0,
                lux: 4400.0
            })
        );
        assert!(parse_sun("24,175").is_err());
        assert!(parse_sun("95,0,1").is_err());
        let (dir, level) = project("bake_export_bounds");
        let mut args = args_for(&level);
        args.from_scene.bounds = Some(([-10.0, 0.0, -10.0], [10.0, 8.0, 10.0]));
        let p = prepare(&args, &mut UnitBoxes).expect("prepare");
        let (parsed, _) = GiBakeDescription::parse_versioned(&p.export.text).expect("parses");
        assert_eq!(parsed.layout.bricks.len(), 1);
        assert_eq!(parsed.layout.bricks[0].min, [-10.0, 0.0, -10.0]);
        assert_eq!(p.export.summary["bounds_override"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_project_is_found_from_the_level() {
        let (dir, level) = project("bake_export_project");
        assert_eq!(find_project(&level), absolute(&dir));
        let loose = regia_test_utils::unique_temp_dir("bake_export_loose");
        let assets_level = loose.join("game/assets/levels/a.level.ron");
        write(&assets_level, b"");
        assert_eq!(find_project(&assets_level), absolute(&loose.join("game")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&loose);
    }
}
