// SPDX-License-Identifier: MPL-2.0
//! The Content Browser panel: the project's `assets/` folder tree and asset list with the
//! asset DB, search, and drag payloads into the viewport (`docs/specs/19-editor.md` § 2).
//!
//! Browse, search, create, import, the thumbnail grid, referencer checks on delete, reimport
//! and show-in-file-manager. Double-click a level to open it, and drag a mesh or prefab into
//! the viewport to place it. A material dropped on a mesh sets that mesh's material. A sound
//! spawns an emitter. Any other asset is left where it is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_render::MeshSource;
use regia_scene::prefab::PrefabTemplate;
use regia_scene::prelude::{EntityGuid, PrefabLibrary, PrefabWorldExt, SpawnOptions, Transform};

use crate::asset_panels::AssetAction;
use crate::command::{EditorCommand, SpawnEntities, SpawnedRoots, UndoStack};
use crate::dnd::{ContentDrag, ContentKind, kind_of_path};
use crate::level_io::{self, OpenLevel};

/// A project-relative path as the editor lists, stores and reports it: `/`-separated on every OS
/// (`assets/props/rock.glb`). Windows separators become `/`; elsewhere the text is unchanged.
pub(crate) fn slash_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Panel state.
#[derive(Debug, Clone, Default)]
pub struct ContentBrowserState {
    /// The project root (contains `Regia.toml` / `assets/`); `None` = no project open.
    pub project_root: Option<PathBuf>,
    /// Search text over asset paths.
    pub search: String,
    /// The currently selected folder (relative to the project root), `""` = assets/.
    pub folder: String,
    /// A pending status line from an import/create action (shown once).
    pub toast: Option<String>,
    /// A delete awaiting confirmation: (project-relative path, referencers when refused).
    pub pending_delete: Option<(String, Vec<String>)>,
    /// A move/rename awaiting the destination: the source (project-relative), and the edited
    /// destination path.
    pub pending_rename: Option<String>,
    /// The edit buffer of [`ContentBrowserState::pending_rename`].
    pub rename_target: String,
    /// A `.material.ron` or `.animgraph.ron` the user double-clicked (the shell opens it in the
    /// Material or Anim Graph panel).
    pub open_request: Option<PathBuf>,
    /// A mesh, texture or sound double-clicked for the asset viewer (project-relative).
    pub view_request: Option<String>,
    /// A `.webm`, `.mkv` or `.video.ron` double-clicked (project-relative); the shell opens it in the Video panel.
    pub video_request: Option<String>,
    /// An audit action from a row's menu, run when the panel's borrow of the world ends.
    pub asset_action: Option<AssetAction>,
    /// The asset a panel picked (Problems, Reference graph): the panel makes it the selection
    /// ([`crate::content_actions::ContentSelection`]) on its next frame.
    pub selected: Option<String>,
    /// A bulk delete awaiting confirmation: (the selected paths, every file that references one
    /// of them) (ADR-7822).
    pub pending_bulk_delete: Option<(Vec<String>, Vec<String>)>,
    /// A bulk move awaiting its destination: (the selected paths, the edited folder).
    pub pending_bulk_move: Option<(Vec<String>, String)>,
}

impl ContentBrowserState {
    /// Shows `asset`: its folder, and the asset marked selected (an audit panel's pick).
    pub fn reveal(&mut self, asset: &str) {
        self.folder = asset
            .rsplit_once('/')
            .map_or_else(String::new, |(folder, _)| folder.to_owned());
        self.selected = Some(asset.to_owned());
    }

    /// The scanned assets that match the search: `(relative path, kind)`. With a blank search,
    /// every asset sorted by path; otherwise the [`crate::asset_search`] matches, best rank first
    /// and then by path (a search that does not parse matches nothing).
    #[must_use]
    pub fn assets(&self) -> Vec<(String, ContentKind)> {
        let Some(root) = self.project_root.as_deref() else {
            return Vec::new();
        };
        let assets_dir = root.join("assets");
        if !assets_dir.is_dir() {
            return Vec::new();
        }
        let Ok(query) = crate::asset_search::AssetQuery::parse(&self.search) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let stack = vec![assets_dir.clone()];
        let mut stack = stack;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || name.ends_with(".meta") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = slash_path(path.strip_prefix(root).unwrap_or(&path));
                if let Some(rank) = query.rank_path(&rel) {
                    let kind = kind_of_path(&rel);
                    out.push((rank, (rel, kind)));
                }
            }
        }
        out.sort();
        out.into_iter().map(|(_, asset)| asset).collect()
    }

    /// The folders under `assets/` (relative paths, sorted).
    #[must_use]
    pub fn folders(&self) -> Vec<String> {
        let Some(root) = self.project_root.as_deref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![root.join("assets")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') {
                    continue;
                }
                let rel = slash_path(path.strip_prefix(root).unwrap_or(&path));
                out.push(rel.clone());
                stack.push(path);
            }
        }
        out.sort();
        out
    }

    /// The assets visible with the current folder + search selection: a non-blank search matches
    /// everywhere, otherwise only the selected folder's direct children.
    #[must_use]
    pub fn visible_assets(&self) -> Vec<(String, ContentKind)> {
        let assets = self.assets();
        if !self.search.trim().is_empty() {
            return assets;
        }
        let prefix = if self.folder.is_empty() {
            "assets".to_string()
        } else {
            self.folder.clone()
        };
        assets
            .into_iter()
            .filter(|(rel, _)| {
                Path::new(rel)
                    .parent()
                    .map(slash_path)
                    .is_some_and(|parent| parent == prefix)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Import and asset creation (specs/19 § 2 "import", specs/12 § 6 cook)
// ---------------------------------------------------------------------------

/// What one import run did (the toast line and the agent reply are built from this).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Project-relative paths created under `assets/`.
    pub imported: Vec<String>,
    /// Assets cooked this run.
    pub cooked: usize,
    /// Assets already up to date in the dev cache (cache hit).
    pub skipped: usize,
    /// Failures: (path, message).
    pub errors: Vec<(String, String)>,
    /// The import was cancelled: every file it had copied (and its `.meta`) was removed again,
    /// so `imported` is empty (`specs/19` § 7).
    pub cancelled: bool,
}

impl ImportReport {
    /// One line for the status bar / toast.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.cancelled {
            return "import cancelled (nothing was added)".to_string();
        }
        let mut line = format!(
            "imported {} (cooked {}, up-to-date {})",
            self.imported.len(),
            self.cooked,
            self.skipped
        );
        if !self.errors.is_empty() {
            line.push_str(&format!(", {} failed — see the Output Log", self.errors.len()));
        }
        line
    }
}

/// A level or prefab source (`*.level.ron`, `*.prefab.ron`, any case): spawned from its RON,
/// never cooked.
fn is_scene_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".level.ron") || lower.ends_with(".prefab.ron") || lower.ends_with(".variant.ron")
}

/// Source extensions the pipeline can cook today (specs/12 §§ 3–5: glTF meshes, PNG, DDS, TGA and
/// BMP textures, `.material.ron`; FBX, OBJ, PLY, STL and USD meshes through the converted-glTF path,
/// ADR-1602, ADR-3244 and ADR-3235; PSD images, ADR-3244; WAV, Ogg, FLAC and MP3 sounds, measured at
/// import, ADR-1855; Aseprite sprites and LDtk worlds into the 2D assets, ADR-3358; MaterialX `.mtlx`
/// materials, ADR-3341: the images a document names are not copied with it). Anything else fails here
/// with a clear message instead of a cook crash.
#[must_use]
pub fn importable_extensions() -> &'static [&'static str] {
    &[
        "glb", "gltf", "png", "ron", "fbx", "obj", "ply", "stl", "psd", "bvh", "dds", "tga", "bmp", "exr",
        "hdr", "wav", "ogg", "flac", "mp3", "aseprite", "ase", "ldtk", "mtlx", "usd", "usda", "usdc", "usdz",
    ]
}

/// Copies the sources into `assets/<folder>/` and cooks the project into the dev cache
/// (`specs/19` § 2 "import", `specs/12` § 6). Incremental: only changed sources cook.
///
/// `client` is the agent client name when the import came through the protocol (provenance
/// records `tool: agent`); `None` marks a human import (`tool: import`, `specs/23` § 5).
///
/// # Errors
///
/// A message naming the first unsatisfiable precondition (no project, unreadable source, copy
/// failure). Per-file failures land in [`ImportReport::errors`] instead.
pub fn import_files(
    project_root: &Path,
    folder: &str,
    sources: &[PathBuf],
    client: Option<&str>,
) -> Result<ImportReport, String> {
    import_files_cancellable(project_root, folder, sources, client, &|| false)
}

/// Removes what a cancelled import had copied (each source and its `.meta`), so no partial asset
/// stays behind; the dev cache's entries for them are orphans the next cook ignores.
fn roll_back_import(report: &mut ImportReport, copied: &[PathBuf]) {
    for dest in copied {
        for path in [dest.clone(), regia_asset::meta_path_for(dest)] {
            if path.exists()
                && let Err(e) = std::fs::remove_file(&path)
            {
                report.errors.push((
                    path.display().to_string(),
                    format!("cannot remove after cancel: {e}"),
                ));
            }
        }
    }
    report.imported.clear();
    report.cooked = 0;
    report.skipped = 0;
    report.cancelled = true;
    tracing::info!(target: "regia_editor", files = copied.len(), "import cancelled; copied files removed");
}

/// [`import_files`] that stops when `cancelled()` turns true (`specs/19` § 7): it is checked before
/// each source, before the cook and after it. A cancelled import removes every file it had copied
/// and answers [`ImportReport::cancelled`].
///
/// # Errors
///
/// As [`import_files`].
pub fn import_files_cancellable(
    project_root: &Path,
    folder: &str,
    sources: &[PathBuf],
    client: Option<&str>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportReport, String> {
    if sources.is_empty() {
        return Err("nothing to import: pick at least one file".to_string());
    }
    let folder = folder.replace('\\', "/");
    let folder = folder.trim_start_matches("assets/").trim_matches('/');
    // A destination must stay under `assets/` (the same rule `create_asset` applies).
    if folder
        .split('/')
        .any(|segment| segment == ".." || segment.contains(':'))
    {
        return Err(format!(
            "invalid import folder {folder:?}: it must stay under assets/"
        ));
    }
    let dest_dir = project_root.join("assets").join(folder);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;

    let mut report = ImportReport::default();
    let mut imported_sources: Vec<PathBuf> = Vec::new();
    for source in sources {
        if cancelled() {
            roll_back_import(&mut report, &imported_sources);
            return Ok(report);
        }
        let source_display = source.display().to_string();
        if !source.is_file() {
            report
                .errors
                .push((source_display.clone(), "not a readable file".to_string()));
            continue;
        }
        let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("asset");
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !importable_extensions().contains(&ext.as_str()) {
            report.errors.push((
                source_display,
                format!(
                    "unsupported extension `.{ext}` — the pipeline imports {}",
                    importable_extensions().join(", ")
                ),
            ));
            continue;
        }
        // Level and prefab `.ron` sources must parse before they enter the project (they are
        // never cooked, so nothing else would catch a bad file; `.material.ron` fails in the cook).
        if ext == "ron" {
            let lower = name.to_ascii_lowercase();
            let parsed = if lower.ends_with(".level.ron") || lower.ends_with(".prefab.ron") {
                std::fs::read_to_string(source)
                    .map_err(|e| format!("cannot read: {e}"))
                    .and_then(|text| {
                        regia_scene::prelude::SceneFile::from_ron_str(&text)
                            .map(|_| ())
                            .map_err(|e| format!("does not parse: {e}"))
                    })
            } else if lower.ends_with(regia_scene::prefab_variant::PREFAB_VARIANT_SUFFIX) {
                std::fs::read_to_string(source)
                    .map_err(|e| format!("cannot read: {e}"))
                    .and_then(|text| {
                        regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(&text)
                            .map(|_| ())
                            .map_err(|e| format!("does not parse: {e}"))
                    })
            } else {
                Ok(())
            };
            if let Err(message) = parsed {
                report.errors.push((source_display, message));
                continue;
            }
        }
        let dest = dest_dir.join(name);
        if dest.exists() {
            report.errors.push((
                name.to_string(),
                format!("already exists in assets/{folder} — rename one of them"),
            ));
            continue;
        }
        match std::fs::copy(source, &dest) {
            Ok(_) => {
                let rel = slash_path(dest.strip_prefix(project_root).unwrap_or(&dest));
                report.imported.push(rel);
                imported_sources.push(dest);
            }
            Err(e) => report.errors.push((source_display, format!("copy failed: {e}"))),
        }
    }

    if report.imported.is_empty() {
        return Ok(report);
    }
    if cancelled() {
        roll_back_import(&mut report, &imported_sources);
        return Ok(report);
    }

    // Cook the project: the pipeline discovers the new sources under assets/ and cooks what
    // changed into the dev cache (`specs/12` § 6 incremental).
    match regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions::for_host(
        project_root.to_path_buf(),
    )) {
        Ok(cook) => {
            report.cooked = cook.cooked;
            report.skipped = cook.skipped;
            for (path, message) in cook.errors {
                // Failures of the files this import brought in belong to the report; failures
                // of pre-existing project files are logged once (visible in the Output Log)
                // but do not turn the import toast red.
                if report.imported.iter().any(|imported| imported == &path) {
                    // Level and prefab sources are read as they are and never cooked (checked
                    // above), so the cook's "not a cookable file" is no import failure; `regia
                    // cook` treats it as benign too (ADR-1389).
                    if is_scene_source(&path) && message.starts_with("unsupported source extension") {
                        continue;
                    }
                    report.errors.push((path, message));
                } else {
                    tracing::warn!(target: "regia_editor", "cook {path}: {message}");
                }
            }
        }
        Err(e) => report.errors.push(("cook".to_string(), e.to_string())),
    }
    if cancelled() {
        roll_back_import(&mut report, &imported_sources);
        return Ok(report);
    }

    // Provenance on every imported meta (specs/23 § 5): who brought the file in.
    for dest in &imported_sources {
        let tool = if client.is_some() {
            regia_asset::ProvenanceTool::Agent
        } else {
            regia_asset::ProvenanceTool::Import
        };
        let provenance = regia_asset::Provenance {
            origin: None,
            tool,
            generator: None,
            seed: None,
            client: client.map(str::to_string),
            session: None,
            model: None,
            created_utc: Some(rfc3339_utc_now()),
            reviewer: regia_asset::ReviewerState::default(),
            extensions: BTreeMap::new(),
        };
        if let Err(e) = stamp_provenance(dest, &provenance) {
            tracing::warn!(target: "regia_editor", "provenance for {}: {e}", dest.display());
        }
    }

    Ok(report)
}

/// Reads the `.meta` of `source`, sets `provenance` and writes it back (created by the cook).
pub(crate) fn stamp_provenance(source: &Path, provenance: &regia_asset::Provenance) -> Result<(), String> {
    let meta_path = regia_asset::meta_path_for(source);
    let bytes = std::fs::read(&meta_path).map_err(|e| format!("cannot read {}: {e}", meta_path.display()))?;
    let mut meta = regia_asset::MetaSidecar::from_ron(&bytes)
        .map_err(|e| format!("cannot parse {}: {e}", meta_path.display()))?;
    meta.provenance = Some(provenance.clone());
    let text = meta
        .to_ron_string()
        .map_err(|e| format!("cannot serialize {}: {e}", meta_path.display()))?;
    std::fs::write(&meta_path, text).map_err(|e| format!("cannot write {}: {e}", meta_path.display()))
}

/// The current time as RFC 3339 UTC (`2026-10-04T12:34:56Z`).
///
/// Days→(y, m, d) is Hinnant's `civil_from_days` (public algorithm, see `docs/provenance.md`);
/// the same re-implementation as `xtask::soak::report` and `regia_core`'s log timestamps.
#[must_use]
pub fn rfc3339_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(secs as i64)
}

/// Formats a unix timestamp as RFC 3339 UTC.
#[must_use]
pub fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `(year, month, day)` of a day count relative to 1970-01-01 (Hinnant, `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Kinds the editor can create (`specs/19` § 2 "create assets").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewAssetKind {
    /// A `.material.ron` with default standard parameters.
    Material,
    /// A `.prefab.ron` with one named root entity.
    Prefab,
    /// An empty `.level.ron`.
    Level,
    /// A folder under `assets/`.
    Folder,
    /// An `.animgraph.ron` whose root plays one looping clip named `idle` (ADR-7234).
    AnimGraph,
    /// A `.cue.ron` with no variations yet, on the Master bus (ADR-7234).
    SoundCue,
    /// A `.ui.ron` with a full-size column root (ADR-7234).
    UiLayout,
    /// A `.data.ron` holding a registered type's default value; needs the type
    /// ([`create_asset_of_type`], ADR-7234).
    DataAsset,
    /// An empty `.matgraph.ron` material graph (ADR-9700).
    MaterialGraph,
    /// A `.matfunc.ron` material function passing one `Vec3` input through (ADR-9700).
    MaterialFunction,
    /// A `.seq.ron` sequence named after the file: no tracks, 5 s long at 30 fps (ADR-8730).
    Sequence,
    /// A `.shots.ron` shot list named after the file: one still shot, 5 s long (ADR-8730).
    ShotList,
    /// An `.attrs.ron` attribute set with one `health` attribute (spec 53 § 4.2, ADR-7472).
    AttributeSet,
    /// An Instant `.effect.ron` with no modifiers yet (spec 53 § 4.3, ADR-7472).
    Effect,
    /// An `.ability.ron` with the format's defaults (spec 53 § 4.4, ADR-7472).
    Ability,
    /// A `.matinst.ron` material instance of a parent graph with no overrides yet (spec 80
    /// § 11.5; needs the parent, so [`create_asset`] alone refuses —
    /// [`create_material_instance`]).
    MaterialInstance,
    /// A `.pgraph.ron` procedural graph: the panel's blank graph, `in.self` → `sample.region` →
    /// `out.points` (spec 67 § 2.3, ADR-7725).
    ProcgenGraph,
    /// A `.vfx.ron` VFX system named after the file, one emitter `main` (`vfx_editor::template`,
    /// spec 26 § 3, ADR-7725).
    VfxSystem,
    /// A `.conv.ron` conversation whose id is the file's stem, with no lines yet (spec 56 § 13,
    /// ADR-7725).
    Conversation,
    /// A `.curves.ron` curve table with no curves, linear (spec 51 § 6, ADR-7725).
    CurveTable,
    /// A `.chooser.ron` chooser table: first-match, any-asset results, no columns or rows (spec 51
    /// § 6, ADR-7725).
    Chooser,
    /// A `.table.ron` data table with no rows; its `row_type` is the given type, else unnamed (spec
    /// 51 § 6, ADR-7725).
    DataTable,
    // End of the w11 creator block (ADR-7725). Later kinds append below this line.
    /// A `.montage.ron` named after the file: one track on slot `default` playing a clip named
    /// `idle` to its end (spec 34; ADR-7731).
    Montage,
    /// A `.cloth.ron` named after the file whose sim mesh is node `<stem>` of `<stem>.glb` beside it,
    /// default parameters (spec 55 § 4.1; ADR-7731).
    Cloth,
    /// A `.soft.ron` pressure soft body named after the file whose surface is node `<stem>` of
    /// `<stem>.glb` beside it (spec 55 § 4.2; ADR-7731).
    SoftBody,
    /// A `.camera.ron` named after the file: one `follow` rig a fixed director runs (spec 35,
    /// ADR-7734).
    CameraRig,
    /// A `.smartobject.ron` smart-object definition with one slot at the object's origin (spec 39
    /// § 3, ADR-7736).
    SmartObject,
    /// A `.tilemap.ron` at the current format version: 16 x 16 cells of 16 px, one empty `ground`
    /// tile layer, no tileset yet (bind one in the 2D tab, ADR-7742).
    Tilemap,
    /// A `.motiondb.ron` motion database with the schema defaults, no tags and no clips, opened in
    /// the motion-database editor (spec 52 § 6, ADR-7740).
    MotionDatabase,
    /// A `.biome.ron` named after the file with no layers yet (spec 65 § 3; the Biome editor adds
    /// them, w11-biome-procgen-tooling).
    Biome,
    /// An empty `.logic.ron` logic graph at format v1: no rules, locals or world boards; the bytes
    /// the Logic Graph panel saves for an empty graph (spec 102 § 6, w11-logic-graph-finish).
    LogicGraph,
}

impl NewAssetKind {
    /// Stable protocol name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Material => "material",
            Self::Prefab => "prefab",
            Self::Level => "level",
            Self::Folder => "folder",
            Self::AnimGraph => "anim_graph",
            Self::SoundCue => "sound_cue",
            Self::UiLayout => "ui_layout",
            Self::DataAsset => "data_asset",
            Self::MaterialGraph => "material_graph",
            Self::MaterialFunction => "material_function",
            Self::Sequence => "sequence",
            Self::ShotList => "shot_list",
            Self::AttributeSet => "attribute_set",
            Self::Effect => "effect",
            Self::Ability => "ability",
            Self::MaterialInstance => "material_instance",
            Self::ProcgenGraph => "procgen_graph",
            Self::VfxSystem => "vfx_system",
            Self::Conversation => "conversation",
            Self::CurveTable => "curve_table",
            Self::Chooser => "chooser",
            Self::DataTable => "data_table",
            Self::Montage => "montage",
            Self::Cloth => "cloth",
            Self::SoftBody => "soft_body",
            Self::CameraRig => "camera",
            Self::SmartObject => "smart_object",
            Self::Tilemap => "tilemap",
            Self::MotionDatabase => "motion_database",
            Self::Biome => "biome",
            Self::LogicGraph => "logic_graph",
        }
    }

    /// Every kind, in the Content Browser's New menu order.
    pub const ALL: [Self; 31] = [
        Self::Material,
        Self::Prefab,
        Self::Level,
        Self::AnimGraph,
        Self::SoundCue,
        Self::UiLayout,
        Self::DataAsset,
        Self::MaterialGraph,
        Self::MaterialFunction,
        Self::Sequence,
        Self::ShotList,
        Self::AttributeSet,
        Self::Effect,
        Self::Ability,
        Self::MaterialInstance,
        Self::ProcgenGraph,
        Self::VfxSystem,
        Self::Conversation,
        Self::CurveTable,
        Self::Chooser,
        Self::DataTable,
        // End of the w11 creator block (ADR-7725): later kinds append above `Folder`.
        Self::Montage,
        Self::Cloth,
        Self::SoftBody,
        Self::CameraRig,
        Self::SmartObject,
        Self::Tilemap,
        Self::MotionDatabase,
        Self::Biome,
        Self::LogicGraph,
        Self::Folder,
    ];

    /// The New menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Material => "Material",
            Self::Prefab => "Prefab",
            Self::Level => "Level",
            Self::Folder => "Folder",
            Self::AnimGraph => "Anim graph",
            Self::SoundCue => "Sound cue",
            Self::UiLayout => "UI layout",
            Self::DataAsset => "Data asset",
            Self::MaterialGraph => "Material graph",
            Self::MaterialFunction => "Material function",
            Self::Sequence => "Sequence",
            Self::ShotList => "Shot list",
            Self::AttributeSet => "Attribute set",
            Self::Effect => "Effect",
            Self::Ability => "Ability",
            Self::MaterialInstance => "Material instance",
            Self::ProcgenGraph => "Procedural graph",
            Self::VfxSystem => "VFX system",
            Self::Conversation => "Conversation",
            Self::CurveTable => "Curve table",
            Self::Chooser => "Chooser",
            Self::DataTable => "Data table",
            Self::Montage => "Montage",
            Self::Cloth => "Cloth",
            Self::SoftBody => "Soft body",
            Self::CameraRig => "Camera rig",
            Self::SmartObject => "Smart object",
            Self::Tilemap => "Tile map",
            Self::MotionDatabase => "Motion database",
            Self::Biome => "Biome",
            Self::LogicGraph => "Logic graph",
        }
    }

    /// Parses the protocol name.
    ///
    /// # Errors
    ///
    /// Unknown kind.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "material" => Ok(Self::Material),
            "prefab" => Ok(Self::Prefab),
            "level" => Ok(Self::Level),
            "folder" => Ok(Self::Folder),
            "anim_graph" => Ok(Self::AnimGraph),
            "sound_cue" => Ok(Self::SoundCue),
            "ui_layout" => Ok(Self::UiLayout),
            "data_asset" => Ok(Self::DataAsset),
            "material_graph" => Ok(Self::MaterialGraph),
            "material_function" => Ok(Self::MaterialFunction),
            "sequence" => Ok(Self::Sequence),
            "shot_list" => Ok(Self::ShotList),
            "attribute_set" => Ok(Self::AttributeSet),
            "effect" => Ok(Self::Effect),
            "ability" => Ok(Self::Ability),
            "material_instance" => Ok(Self::MaterialInstance),
            "procgen_graph" => Ok(Self::ProcgenGraph),
            "vfx_system" => Ok(Self::VfxSystem),
            "conversation" => Ok(Self::Conversation),
            "curve_table" => Ok(Self::CurveTable),
            "chooser" => Ok(Self::Chooser),
            "data_table" => Ok(Self::DataTable),
            "montage" => Ok(Self::Montage),
            "cloth" => Ok(Self::Cloth),
            "soft_body" => Ok(Self::SoftBody),
            "camera" => Ok(Self::CameraRig),
            "smart_object" => Ok(Self::SmartObject),
            "tilemap" => Ok(Self::Tilemap),
            "motion_database" => Ok(Self::MotionDatabase),
            "biome" => Ok(Self::Biome),
            "logic_graph" => Ok(Self::LogicGraph),
            other => Err(format!(
                "unknown asset kind {other:?} (material, prefab, level, anim_graph, sound_cue, ui_layout, \
                 data_asset, folder, material_graph, material_function, sequence, shot_list, attribute_set, effect, ability, material_instance, \
                 procgen_graph, vfx_system, conversation, curve_table, chooser, data_table, montage, cloth, soft_body, camera, smart_object, \
                 tilemap, motion_database, biome, logic_graph)"
            )),
        }
    }

    /// The file extension this kind creates (`None` for folders).
    #[must_use]
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::Material => Some("material.ron"),
            Self::Prefab => Some("prefab.ron"),
            Self::Level => Some("level.ron"),
            Self::Folder => None,
            Self::AnimGraph => Some("animgraph.ron"),
            Self::SoundCue => Some("cue.ron"),
            Self::UiLayout => Some("ui.ron"),
            Self::DataAsset => Some(crate::data_asset::DATA_ASSET_EXTENSION),
            Self::MaterialGraph => Some("matgraph.ron"),
            Self::MaterialFunction => Some("matfunc.ron"),
            Self::Sequence => Some("seq.ron"),
            Self::ShotList => Some("shots.ron"),
            Self::AttributeSet => Some("attrs.ron"),
            Self::Effect => Some("effect.ron"),
            Self::Ability => Some("ability.ron"),
            Self::MaterialInstance => Some("matinst.ron"),
            Self::ProcgenGraph => Some("pgraph.ron"),
            Self::VfxSystem => Some("vfx.ron"),
            Self::Conversation => Some("conv.ron"),
            Self::CurveTable => Some("curves.ron"),
            Self::Chooser => Some("chooser.ron"),
            Self::DataTable => Some("table.ron"),
            Self::Montage => Some("montage.ron"),
            Self::Cloth => Some("cloth.ron"),
            Self::SoftBody => Some("soft.ron"),
            Self::CameraRig => Some("camera.ron"),
            Self::SmartObject => Some("smartobject.ron"),
            Self::Tilemap => Some("tilemap.ron"),
            Self::MotionDatabase => Some(crate::mm_db_editor::MOTIONDB_EXTENSION),
            Self::Biome => Some("biome.ron"),
            Self::LogicGraph => Some("logic.ron"),
        }
    }
}

/// The checked target of a new asset: the project-relative path under `assets/` (validated by
/// the shared project-path rule and carrying `kind`'s extension, which a folder has none of), the
/// absolute path (its parent is created) and the file's stem for naming the asset.
fn checked_new_asset_path(
    project_root: &Path,
    kind: NewAssetKind,
    path: &str,
) -> Result<(String, PathBuf, String), String> {
    let rel = path.replace('\\', "/");
    let rel = rel.trim_start_matches("assets/").trim_matches('/').to_string();
    if rel.is_empty()
        || rel
            .split('/')
            .any(|segment| segment.is_empty() || segment.starts_with('.'))
    {
        return Err(format!("invalid asset path {path:?}"));
    }
    // The shared project-path rule: no drive prefix (`C:` makes `Path::join` drop the base on
    // Windows), no `..`, no Windows device names (`CON.material.ron`), no trailing dot/space.
    if let Err(why) = regia_core::project_path::check_relative(&rel) {
        return Err(format!("invalid asset path {path:?}: {why}"));
    }
    if let Some(ext) = kind.extension()
        && !rel.ends_with(&format!(".{ext}"))
    {
        return Err(format!("the path must end with `.{ext}`: {path}"));
    }
    let full = project_root.join("assets").join(&rel);
    if full.exists() {
        return Err(format!("assets/{rel} already exists"));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let stem = if let Some(ext) = kind.extension() {
        rel.strip_suffix(&format!(".{ext}"))
            .unwrap_or(&rel)
            .rsplit('/')
            .next()
            .unwrap_or("Asset")
            .to_string()
    } else {
        rel.rsplit('/').next().unwrap_or("Asset").to_string()
    };
    Ok((rel, full, stem))
}

/// Creates a new asset at project-relative `path`. A material, prefab or level path must
/// already end with that kind's extension (`.material.ron`, `.prefab.ron` or `.level.ron`);
/// a path that omits it is refused and nothing is written. A folder has no extension.
/// Returns the created project-relative path.
///
/// # Errors
///
/// When the path omits its kind extension, escapes `assets/`, already exists, or cannot be
/// written.
pub fn create_asset(project_root: &Path, kind: NewAssetKind, path: &str) -> Result<String, String> {
    if kind == NewAssetKind::DataAsset {
        return Err("a data asset needs its type: use create_asset_of_type".to_owned());
    }
    if kind == NewAssetKind::MaterialInstance {
        return Err(
            "a material instance needs its parent graph: use create_material_instance (or \
             editor.create_asset with parent)"
                .to_owned(),
        );
    }
    let (rel, full, stem) = checked_new_asset_path(project_root, kind, path)?;
    match kind {
        NewAssetKind::Folder => {
            std::fs::create_dir_all(&full).map_err(|e| format!("cannot create folder: {e}"))?;
        }
        NewAssetKind::Material => {
            let mut material = regia_asset::MaterialAssetRon::default_standard();
            material.name = Some(stem);
            let text = material
                .to_ron_string()
                .map_err(|e| format!("material serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::Prefab => {
            let mut file = regia_scene::prelude::SceneFile::new();
            file.entities.push(regia_scene::prelude::SceneEntity {
                guid: Guid::new_random(),
                name: Some(stem),
                parent: None,
                components: BTreeMap::from([(
                    "regia_scene::transform::Transform".to_string(),
                    ron::value::RawValue::from_ron("()")
                        .map_err(|e| e.to_string())?
                        .to_owned(),
                )]),
                prefab_instance: None,
            });
            let text = file
                .to_ron_string()
                .map_err(|e| format!("prefab serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::MaterialGraph => {
            let text = regia_matgraph::MaterialGraph::new(stem)
                .to_ron()
                .map_err(|e| format!("material graph serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::MaterialFunction => {
            let function = regia_matgraph::MaterialFunction {
                format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
                name: stem,
                inputs: vec![regia_matgraph::FunctionInput {
                    name: "x".into(),
                    ty: regia_matgraph::ValueType::Vec3,
                }],
                nodes: BTreeMap::from([("x".to_string(), regia_matgraph::Node::Input("x".into()))]),
                outputs: BTreeMap::from([("out".to_string(), "x".to_string())]),
            };
            let text = function
                .to_ron()
                .map_err(|e| format!("material function serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::Level => {
            let level = regia_scene::prelude::Level {
                id: regia_scene::prelude::LevelId(Guid::new_random()),
                settings: regia_scene::prelude::LevelSettings::default(),
                scene: regia_scene::prelude::SceneFile::new(),
            };
            let text = level
                .to_ron_string()
                .map_err(|e| format!("level serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::AnimGraph => write_text(&full, &new_anim_graph_text()?)?,
        NewAssetKind::SoundCue => write_text(&full, &new_sound_cue_text(&stem)?)?,
        NewAssetKind::UiLayout => write_text(&full, &new_ui_layout_text()?)?,
        NewAssetKind::Sequence => write_text(&full, &new_sequence_text(&stem)?)?,
        NewAssetKind::ShotList => write_text(&full, &new_shot_list_text(&stem)?)?,
        // Refused above: they need their type / their parent graph.
        NewAssetKind::DataAsset | NewAssetKind::MaterialInstance => {}
        NewAssetKind::AttributeSet | NewAssetKind::Effect | NewAssetKind::Ability => {
            write_text(&full, &new_ability_asset_text(kind, &stem)?)?;
        }
        NewAssetKind::ProcgenGraph
        | NewAssetKind::VfxSystem
        | NewAssetKind::Conversation
        | NewAssetKind::CurveTable
        | NewAssetKind::Chooser
        | NewAssetKind::DataTable => write_text(&full, &new_w11_asset_text(kind, &stem, None)?)?,
        NewAssetKind::Montage => write_text(&full, &new_montage_text(&stem)?)?,
        NewAssetKind::Cloth | NewAssetKind::SoftBody => write_text(&full, &new_cloth_text(kind, &stem)?)?,
        NewAssetKind::CameraRig => write_text(&full, &crate::camera_panel::edit::new_asset_text(&stem)?)?,
        NewAssetKind::SmartObject => {
            // The format's own canonical writer, so the file opens in the definition editor as saved.
            let mut def = regia_ai::smart_object_def::SmartObjectDef::new();
            def.slots
                .push(crate::smart_object_def_panel::default_slot("slot_0"));
            let text = def.to_ron().map_err(|e| format!("smart object serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::Tilemap => {
            let map =
                crate::mode2d::tilemap_new::new_tilemap(&crate::mode2d::tilemap_new::NewTilemap::default())
                    .map_err(|e| format!("tile map: {e}"))?;
            let text = map.to_ron().map_err(|e| format!("tile map serialize: {e}"))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::MotionDatabase => {
            let text = crate::mm_db_editor::canonical_text(&crate::mm_db_editor::new_document(""))
                .map_err(|e| format!("motion database serialize: {}", e.message))?;
            write_text(&full, &text)?;
        }
        NewAssetKind::Biome => {
            let def = regia_procgen::biome::def::BiomeDef {
                name: stem,
                ..regia_procgen::biome::def::BiomeDef::default()
            };
            write_text(&full, &def.to_ron())?;
        }
        NewAssetKind::LogicGraph => {
            // The panel's own writer for an empty graph, so the file opens clean and re-saves
            // byte-identical.
            let text = regia_ai::logic_edit::LogicGraphDoc::new()
                .to_ron()
                .map_err(|e| format!("logic graph serialize: {e}"))?;
            write_text(&full, &text)?;
        }
    }
    Ok(format!("assets/{rel}"))
}

/// The text of a new procgen graph, VFX system, conversation, curve table, chooser or data table, at
/// the format's current version, by its canonical writer (ADR-7725). `row_type` names a data
/// table's row struct (unnamed when `None`; any typed load accepts an unnamed table).
pub fn new_w11_asset_text(kind: NewAssetKind, stem: &str, row_type: Option<&str>) -> Result<String, String> {
    match kind {
        NewAssetKind::ProcgenGraph => Ok(crate::procgen::doc::blank_graph().to_ron()),
        NewAssetKind::VfxSystem => crate::vfx_editor::template(stem)
            .to_ron()
            .map_err(|e| format!("vfx system serialize: {e}")),
        NewAssetKind::Conversation => {
            let asset = regia_framework::conversation::asset::ConversationAsset {
                format_version: regia_framework::conversation::asset::CONVERSATION_FORMAT_VERSION,
                id: stem.to_owned(),
                notes: String::new(),
                lines: Vec::new(),
                steps: Vec::new(),
            };
            regia_framework::conversation::asset::to_ron(&asset)
                .map_err(|e| format!("conversation serialize: {e}"))
        }
        NewAssetKind::CurveTable => Ok(regia_framework::curve_table::CurveTable::empty(
            regia_framework::curve_table::TableInterp::Linear,
        )
        .to_ron_string()),
        NewAssetKind::Chooser => Ok(regia_framework::chooser::ChooserTable::new(
            regia_framework::chooser::Mode::First,
            regia_framework::chooser::ResultType::Asset("any".to_owned()),
        )
        .to_ron_string()),
        NewAssetKind::DataTable => Ok(regia_asset::data_table::RawTable {
            row_type: row_type.unwrap_or_default().to_owned(),
            source_version: regia_asset::data_table::DATA_TABLE_FORMAT_VERSION,
            columns: Vec::new(),
            rows: Vec::new(),
        }
        .to_ron()),
        other => Err(format!("{} is not a w11 creator kind", other.as_str())),
    }
}

/// A new montage (ADR-7731): one track on the engine default slot (`default`, full body) playing
/// a clip named `idle` to its end (a montage needs a track with a segment; the Montage timeline
/// replaces it), at the current version, by the format's own writer.
fn new_montage_text(stem: &str) -> Result<String, String> {
    use regia_animation::montage::{MontageSegment, MontageTrack};
    let montage = regia_animation::Montage {
        format_version: regia_animation::MONTAGE_FORMAT_VERSION,
        name: stem.to_owned(),
        rate: 1.0,
        tracks: vec![MontageTrack {
            slot: "default".to_owned(),
            segments: vec![MontageSegment {
                clip: "idle".to_owned(),
                start: 0.0,
                end: None,
                rate: 1.0,
                loops: 1,
            }],
        }],
        sections: Vec::new(),
        notifies: Vec::new(),
        blend_in: regia_animation::montage::MontageBlend::default(),
        blend_out: regia_animation::montage::MontageBlend::default(),
        blend_out_lead: None,
        auto_blend_out: true,
        root_motion: true,
        root_motion_off: Vec::new(),
        warps: Vec::new(),
        sync_group: None,
        rate_curve: None,
    };
    montage.validate().map_err(|e| format!("montage: {e}"))?;
    montage
        .to_ron_string()
        .map_err(|e| format!("montage serialize: {e}"))
}

/// A new cloth or soft body (ADR-7731): named after the file, its mesh node `<stem>` of
/// `<stem>.glb` beside it (export the sim mesh there, or point the field at another file),
/// default parameters, at the current version; a soft body is a pressure model at rest volume.
fn new_cloth_text(kind: NewAssetKind, stem: &str) -> Result<String, String> {
    use regia_physics::cloth::asset as cloth;
    let mesh = cloth::MeshRef {
        source: format!("{stem}.glb"),
        node: stem.to_owned(),
    };
    let text = if kind == NewAssetKind::SoftBody {
        let soft = cloth::SoftSource {
            format_version: cloth::SOFT_FORMAT_VERSION,
            name: stem.to_owned(),
            model: cloth::SoftModelSource::Pressure {
                pressure: 1.0,
                volume_compliance: 0.0,
            },
            surface: mesh,
            params: cloth::SoftParamsSource::default(),
            maps: BTreeMap::new(),
        };
        ron::ser::to_string_pretty(&soft, regia_core::ron_pretty()).map_err(|e| e.to_string())
    } else {
        let source = cloth::ClothSource {
            format_version: cloth::CLOTH_FORMAT_VERSION,
            name: stem.to_owned(),
            skeleton: None,
            sim_mesh: mesh,
            render_mesh: None,
            params: cloth::ClothParams::default(),
            preset: None,
            maps: BTreeMap::new(),
            constraint_sets: Vec::new(),
            proxies: Vec::new(),
            lods: Vec::new(),
        };
        source.to_ron_string().map_err(|e| e.to_string())
    };
    text.map_err(|e| format!("{} serialize: {e}", kind.as_str()))
}

/// The text of a new attribute set, effect or ability: the current format version, written by the
/// format's own canonical writer (ADR-7472).
fn new_ability_asset_text(kind: NewAssetKind, stem: &str) -> Result<String, String> {
    use regia_framework::abilities as ab;
    let text = match kind {
        NewAssetKind::AttributeSet => ab::to_ron(&ab::AttributesAsset {
            format_version: ab::ATTRIBUTES_FORMAT_VERSION,
            description: String::new(),
            attributes: vec![ab::AttributeDecl {
                name: "health".to_owned(),
                base: 100.0,
                min: 0.0,
                max: 100.0,
                max_from: None,
                derived: None,
            }],
        }),
        NewAssetKind::Effect => {
            let mut effect = ab::EffectAsset::new(ab::Lifetime::Instant);
            effect.format_version = ab::EFFECT_FORMAT_VERSION;
            effect.name = stem.to_owned();
            ab::to_ron(&effect)
        }
        _ => {
            let mut ability = ab::AbilityAsset::new(stem);
            ability.format_version = ab::ABILITY_FORMAT_VERSION;
            ab::to_ron(&ability)
        }
    };
    text.map_err(|e| format!("{} serialize: {e}", kind.as_str()))
}

/// Creates a new asset like [`create_asset`]; a [`NewAssetKind::DataAsset`] holds `type_path`'s
/// default value (`registry` resolves it; it must be a registered data asset type, what a game
/// loads). Other kinds ignore `type_path`.
///
/// # Errors
///
/// As [`create_asset`]; for a data asset, the `data_asset_*` message (no type, unknown or
/// unregistered type, bad path, exists).
pub fn create_asset_of_type(
    project_root: &Path,
    kind: NewAssetKind,
    path: &str,
    type_path: Option<&str>,
    registry: Option<&regia_ecs::reflect::TypeRegistry>,
) -> Result<String, String> {
    if kind == NewAssetKind::DataTable
        && let Some(row_type) = type_path.filter(|t| !t.is_empty())
    {
        let (rel, full, stem) = checked_new_asset_path(project_root, kind, path)?;
        write_text(&full, &new_w11_asset_text(kind, &stem, Some(row_type))?)?;
        return Ok(format!("assets/{rel}"));
    }
    if kind != NewAssetKind::DataAsset {
        return create_asset(project_root, kind, path);
    }
    let type_path = type_path
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "a data asset needs `type` (see editor.data_asset_types)".to_owned())?;
    let registry = registry.ok_or_else(|| "the type registry is not available".to_owned())?;
    crate::data_asset::create_data_asset(project_root, path, type_path, registry)
        .map_err(|e| format!("{} ({})", e.message, e.code))
}

/// Creates a material instance of `parent` (a project-relative `.matgraph.ron` path that must
/// exist) at `path`, with no overrides: exactly the bytes
/// [`regia_matgraph::MaterialInstance::to_ron`] writes for the same parent (spec 80 § 11.5).
/// Returns the created project-relative path.
///
/// # Errors
///
/// When `parent` is not an existing material graph, the path omits `.matinst.ron`, escapes
/// `assets/`, already exists, or cannot be written.
pub fn create_material_instance(project_root: &Path, path: &str, parent: &str) -> Result<String, String> {
    let parent_rel = asset_needle(parent);
    // Inside the project, by the agent path's rule (`project_path_in`): no absolute path, drive
    // prefix or `..` (sprint-w9-rev-v06-lanes § 2 finding 4).
    if parent_rel.is_empty()
        || !parent_rel.ends_with(".matgraph.ron")
        || regia_core::project_path::check_relative(&parent_rel).is_err()
    {
        return Err(format!(
            "the parent must be a project .matgraph.ron path: {parent}"
        ));
    }
    if !project_root.join("assets").join(&parent_rel).is_file() {
        return Err(format!("the parent graph assets/{parent_rel} does not exist"));
    }
    let (rel, full, _stem) = checked_new_asset_path(project_root, NewAssetKind::MaterialInstance, path)?;
    let reference = format!("assets/{parent_rel}");
    let text = regia_matgraph::MaterialInstance::new(&reference)
        .to_ron()
        .map_err(|e| format!("material instance serialize: {e}"))?;
    write_text(&full, &text)?;
    Ok(format!("assets/{rel}"))
}

/// A new anim graph: one looping clip `idle` at the root (a graph needs a root node; the Anim
/// Graph editor replaces it).
fn new_anim_graph_text() -> Result<String, String> {
    use regia_animation::graph::{AnimGraph, AnimNode, ClipNode};
    AnimGraph {
        parameters: Vec::new(),
        root: AnimNode::Clip(ClipNode {
            clip: "idle".to_owned(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        }),
    }
    .to_ron_string()
    .map_err(|e| format!("anim graph serialize: {e}"))
}

/// A new sound cue named after the file, with no variations yet (the Cue Editor adds them).
fn new_sound_cue_text(name: &str) -> Result<String, String> {
    let cue: regia_audio::SoundCueDef =
        ron::from_str(&format!("(name: {name:?}, sounds: [])")).map_err(|e| format!("sound cue: {e}"))?;
    let cue = regia_audio::SoundCueDef {
        version: regia_audio::CUE_VERSION,
        ..cue
    };
    cue.to_ron().map_err(|e| format!("sound cue serialize: {e}"))
}

/// Length of a new sequence or shot list, seconds (the Sequence Curves panel's New row default).
const NEW_SEQUENCE_LENGTH: f32 = 5.0;
/// Frame rate of a new sequence or shot list.
const NEW_SEQUENCE_FPS: f32 = 30.0;

/// A new sequence named after the file: what `editor.seq_new` writes with its defaults (no
/// tracks, an end marker at 5 s, 30 fps), at the current `.seq.ron` version.
fn new_sequence_text(name: &str) -> Result<String, String> {
    regia_framework::sequencer::edit::tracks::new_sequence(name, NEW_SEQUENCE_LENGTH, NEW_SEQUENCE_FPS)
        .map_err(|e| format!("sequence: {}", e.message))?
        .to_ron()
        .map_err(|e| format!("sequence serialize: {e}"))
}

/// A new shot list named after the file: one still shot (`shot1`) at the origin looking down
/// -Z with a 60° field of view, lasting 5 s at 30 fps (what the timeline's first **Add cut**
/// makes), at the current `.shots.ron` version.
fn new_shot_list_text(name: &str) -> Result<String, String> {
    use regia_framework::sequencer::edit::timeline::{CutPose, new_shot_list};
    let pose = CutPose {
        position: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        fov_degrees: 60.0,
    };
    let mut list = new_shot_list(pose, NEW_SEQUENCE_LENGTH, NEW_SEQUENCE_FPS)
        .map_err(|e| format!("shot list: {}", e.message))?;
    list.name = name.to_owned();
    list.to_ron().map_err(|e| format!("shot list serialize: {e}"))
}

/// A new UI layout: a full-size column root, as the UI Designer's New starts.
fn new_ui_layout_text() -> Result<String, String> {
    let mut root = regia_ui::authoring::UiNodeDef::default();
    root.style.size = [
        regia_ui::Dimension::Percent(100.0),
        regia_ui::Dimension::Percent(100.0),
    ];
    root.style.direction = regia_ui::style::FlexDirection::Column;
    let file = regia_ui::UiFile {
        version: regia_ui::UI_RON_VERSION,
        component: None,
        root,
        variants: Vec::new(),
        back: None,
    };
    regia_ui::write_ui_ron(&file).map_err(|e| format!("ui layout serialize: {e}"))
}

/// Writes `text` to `path` atomically (temp file + rename, like the level writer).
fn write_text(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("new.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
}

/// The undo entry of a content-browser move/rename (spec 12 § 9, M11): moves the source and its
/// `.meta` back and forth with [`regia_asset::project_ops::move_asset`], which rewrites the path
/// references on each leg and never changes the asset id.
pub struct MoveAsset {
    root: PathBuf,
    from: String,
    to: String,
    /// Whether the forward move is on disk (the caller moves first, then pushes the entry).
    applied: bool,
}

impl MoveAsset {
    /// An entry for a move that already happened (the outcome went to the user as a toast).
    pub fn applied(root: PathBuf, from: String, to: String) -> Self {
        Self {
            root,
            from,
            to,
            applied: true,
        }
    }

    fn do_move(&self, from: &str, to: &str) {
        if let Err(e) = regia_asset::project_ops::move_asset(&self.root, from, to) {
            tracing::warn!(target: "regia_editor", "move {from} -> {to}: {e}");
        }
    }
}

impl EditorCommand for MoveAsset {
    fn apply(&mut self, _world: &mut World) {
        if !self.applied {
            self.do_move(&self.from.clone(), &self.to.clone());
            self.applied = true;
        }
    }
    fn revert(&mut self, _world: &mut World) {
        if self.applied {
            self.do_move(&self.to.clone(), &self.from.clone());
            self.applied = false;
        }
    }
    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        // File-only command: the level document is untouched, so the entry must not mark a saved
        // level dirty (which would prompt on close and trigger an autosave).
        false
    }
    fn description(&self) -> String {
        format!("Move {} to {}", self.from, self.to)
    }
}

/// Pushes a finished move's undo entry on the editor's undo stack (creating the stack resource
/// when the host did not, the same as the undo/redo handlers do).
pub fn push_move_undo(world: &mut World, root: PathBuf, from: String, to: String) {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(Box::new(MoveAsset::applied(root, from, to)));
    stack.seal();
}

/// Project files that mention `rel` (a cheap referencer check for deletes/renames: scene,
/// prefab, level and material files are all text). A file matches by path or by the asset id in
/// `rel`'s `.meta` sidecar (saved instances store `asset:<guid>`, not the path). Thin wrapper
/// over [`regia_asset::project_ops::find_referencers`], which the move/rename fix-up shares.
#[must_use]
pub fn find_referencers(project_root: &Path, rel: &str) -> Vec<String> {
    regia_asset::project_ops::find_referencers(project_root, rel)
}

/// [`find_referencers`] by path mention only: the files an `asset:<guid>` reference alone would
/// not list. A move keeps the id (the `.meta` moves with the file), so only these break on one.
#[must_use]
pub fn find_path_referencers(project_root: &Path, rel: &str) -> Vec<String> {
    regia_asset::project_ops::find_path_referencers(project_root, rel)
}

/// The asset path as the level's string fields store it (spec 19 § 3): trimmed, `/`-separated,
/// without its `assets/` prefix.
fn asset_needle(path: &str) -> String {
    let normalized = path.trim().replace('\\', "/");
    normalized
        .strip_prefix("assets/")
        .unwrap_or(&normalized)
        .to_string()
}

/// Whether any string leaf of `value` equals `needle` or `assets/<needle>` (the level's fields
/// store the path either way). Walks the same reflected shapes
/// [`crate::agent_ext::reflected_is_finite`] walks.
fn reflects_asset_path(value: &dyn regia_ecs::reflect::PartialReflect, needle: &str) -> bool {
    if let Some(text) = value.try_downcast_ref::<String>() {
        return text == needle || text == &format!("assets/{needle}");
    }
    use regia_ecs::reflect::ReflectRef;
    match value.reflect_ref() {
        ReflectRef::Struct(fields) => fields
            .iter_fields()
            .any(|(_, field)| reflects_asset_path(field, needle)),
        ReflectRef::TupleStruct(fields) => fields.iter_fields().any(|f| reflects_asset_path(f, needle)),
        ReflectRef::Tuple(fields) => fields.iter_fields().any(|f| reflects_asset_path(f, needle)),
        ReflectRef::List(list) => list.iter().any(|item| reflects_asset_path(item, needle)),
        ReflectRef::Array(array) => array.iter().any(|item| reflects_asset_path(item, needle)),
        ReflectRef::Map(map) => map
            .iter()
            .any(|(key, item)| reflects_asset_path(key, needle) || reflects_asset_path(item, needle)),
        ReflectRef::Set(set) => set.iter().any(|item| reflects_asset_path(item, needle)),
        ReflectRef::Enum(value) => value
            .iter_fields()
            .any(|field| reflects_asset_path(field.value(), needle)),
        ReflectRef::Opaque(_) => false,
    }
}

/// Every level entity that references the asset `path` (spec 19 § 3 "select by asset",
/// `editor.select_by_asset`): each reflected string field of each component that equals the path
/// with or without its `assets/` prefix — a mesh's [`regia_render::MeshSource::mesh`], its
/// material override, a sound emitter's `sound_name`, a cue — and, because a saved prefab
/// instance names its source by asset id, the instances of the id in `path`'s `.meta` sidecar
/// when the project is open. Editor-internal and resource entities are left out; an unknown
/// path matches nothing and is not an error. The selection itself is the caller's (one place:
/// the agent handler and the Content Browser's **Select in level**).
#[must_use]
pub fn entities_using_asset(world: &World, path: &str) -> Vec<Entity> {
    let needle = asset_needle(path);
    if needle.is_empty() {
        return Vec::new();
    }
    // A prefab instance's source: the path's asset id, when the sidecar can be read.
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    // Only a path inside the project is probed (no absolute path, drive prefix or `..`).
    let inside = regia_core::project_path::check_relative(&needle).is_ok();
    let prefab_id = root.as_deref().filter(|_| inside).and_then(|root| {
        let meta = root.join("assets").join(&needle);
        std::fs::read(regia_asset::meta_path_for(&meta))
            .ok()
            .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
            .map(|meta| meta.id.0.as_u128())
    });
    let registry = world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
        .cloned();
    let mut found = Vec::new();
    for entity in world.iter_entities() {
        let id = entity.id();
        // Resources are never scene content (ADR-1034); editor-internal entities stay hidden.
        if entity.contains::<regia_ecs::ecs::resource::IsResource>()
            || crate::visibility::is_hidden(world, id)
        {
            continue;
        }
        if let Some(prefab_id) = prefab_id
            && world
                .get::<regia_scene::prefab::PrefabInstance>(id)
                .is_some_and(|instance| instance.prefab == prefab_id)
        {
            found.push(id);
            continue;
        }
        let Some(registry) = registry.as_ref() else {
            continue;
        };
        let registry = registry.read();
        let referenced = world.inspect_entity(id).is_ok_and(|infos| {
            infos.into_iter().any(|(_, info)| {
                let full = info.name().to_string();
                let Some(registration) = registry.get_with_type_path(&full) else {
                    return false;
                };
                let Some(data) = registration.data::<regia_ecs::reflect::ReflectComponent>() else {
                    return false;
                };
                world
                    .get_entity(id)
                    .ok()
                    .and_then(|entity| data.reflect(entity))
                    .is_some_and(|value| reflects_asset_path(value.as_partial_reflect(), &needle))
            })
        });
        if referenced {
            found.push(id);
        }
    }
    found
}

/// Selects [`entities_using_asset`] into the editor selection (spec 19 § 3): the Content
/// Browser's **Select in level** menu item. Returns the now-selected entities.
pub fn select_by_asset(world: &mut World, path: &str) -> Vec<Entity> {
    let found = entities_using_asset(world, path);
    let mut selection = world
        .get_resource_or_insert_with::<crate::selection::Selection>(Default::default)
        .clone();
    selection.clear();
    for entity in &found {
        // Add after the clear: the per-entity Replace event would keep only the last.
        selection.select(*entity, crate::selection::SelectMode::Add);
    }
    world.insert_resource(selection);
    found
}

/// Deletes `rel` (project-relative) when nothing references it. Returns the referencers on
/// refusal.
///
/// # Errors
///
/// `Err` with the referencers when other assets mention the file. A path that is not on disk
/// is `Err` with an empty list: nothing is deleted and there is no message. An IO failure is
/// `Err` with one entry, the message.
pub fn delete_asset(project_root: &Path, rel: &str) -> Result<(), Vec<String>> {
    let rel = rel.trim_start_matches("assets/");
    let full = project_root.join("assets").join(rel);
    if !full.exists() {
        return Err(Vec::new());
    }
    let referencers = find_referencers(project_root, &format!("assets/{rel}"));
    let referencers: Vec<String> = referencers
        .into_iter()
        .filter(|r| r != &format!("assets/{rel}"))
        .collect();
    if !referencers.is_empty() {
        return Err(referencers);
    }
    let meta = regia_asset::meta_path_for(&full);
    let _ = std::fs::remove_file(&meta);
    let result = if full.is_dir() {
        std::fs::remove_dir_all(&full).map_err(|e| format!("cannot delete {}: {e}", full.display()))
    } else {
        std::fs::remove_file(&full).map_err(|e| format!("cannot delete {}: {e}", full.display()))
    };
    result.map_err(|message| vec![message])
}

/// Deletes without the referencer check (the "delete anyway" path after showing them).
///
/// # Errors
///
/// IO failure.
pub fn delete_asset_force(project_root: &Path, rel: &str) -> Result<(), String> {
    let full = project_root
        .join("assets")
        .join(rel.trim_start_matches("assets/"));
    let _ = std::fs::remove_file(regia_asset::meta_path_for(&full));
    if full.is_dir() {
        std::fs::remove_dir_all(&full).map_err(|e| format!("cannot delete {}: {e}", full.display()))
    } else {
        std::fs::remove_file(&full).map_err(|e| format!("cannot delete {}: {e}", full.display()))
    }
}

/// One cook-report line (the panel's reimport toast).
fn report_summary(report: &regia_asset_pipeline::CookReport) -> String {
    let mut line = format!("cooked {} (up-to-date {})", report.cooked, report.skipped);
    if !report.errors.is_empty() {
        line.push_str(&format!(", {} failed — see the Output Log", report.errors.len()));
        for (path, message) in report.errors.iter().take(3) {
            tracing::error!(target: "regia_editor", "cook {path}: {message}");
        }
    }
    line
}

/// Opens the OS file manager at `path` (Linux `xdg-open`, Windows `explorer`).
fn open_in_file_manager(path: &Path) {
    let shown = path.display().to_string();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(target_os = "windows"))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if result.is_err() {
        tracing::warn!(target: "regia_editor", "cannot open a file manager at {shown}");
    }
}

/// The first free `assets/<folder>/<kind>-<n>.<ext>` for a New menu entry.
#[must_use]
pub fn next_new_asset_path(root: &Path, folder: &str, kind: NewAssetKind) -> String {
    let stem = kind.as_str();
    let ext = kind.extension().map_or_else(String::new, |e| format!(".{e}"));
    let folder = if folder.is_empty() {
        String::new()
    } else {
        format!("{}/", folder.trim_start_matches("assets/").trim_end_matches('/'))
    };
    let mut n = 1;
    let mut rel = format!("{folder}{stem}-{n}{ext}");
    while root.join("assets").join(&rel).exists() {
        n += 1;
        rel = format!("{folder}{stem}-{n}{ext}");
    }
    rel
}

/// What a Content Browser New menu entry does: creates the asset in the shown folder under the
/// first free name, reports it in the toast, and opens a data asset in its panel.
pub fn new_asset_from_menu(
    world: &mut World,
    state: &mut ContentBrowserState,
    kind: NewAssetKind,
    type_path: Option<&str>,
) {
    let Some(root) = state.project_root.clone() else {
        state.toast = Some("no project is open".to_owned());
        return;
    };
    let rel = next_new_asset_path(&root, &state.folder, kind);
    let registry = world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
        .cloned();
    let created = {
        let guard = registry.as_ref().map(|r| r.read());
        create_asset_of_type(&root, kind, &rel, type_path, guard.as_deref())
    };
    match created {
        Ok(created) => {
            if kind == NewAssetKind::DataAsset
                && let Err(e) = crate::data_asset::open_data_asset(world, &created)
            {
                tracing::warn!(target: "regia_editor", "open {created}: {}", e.message);
            }
            // A new motion database opens in its editor window (ADR-7740).
            if kind == NewAssetKind::MotionDatabase {
                let opened = crate::mm_db_editor::open(world, &root.join(&created), false);
                let mut editor = world.get_resource_or_insert_with(crate::mm_db_editor::MmDbEditor::default);
                editor.ui.window_open = true;
                if let Err(e) = opened {
                    editor.toast = format!("{} ({})", e.message, e.code);
                }
            }
            state.selected = Some(created.clone());
            state.toast = Some(format!("Created {created}"));
        }
        Err(err) => state.toast = Some(err),
    }
}

/// What the New ▸ Material instance menu entry does: creates the instance of `parent` in the
/// shown folder under the first free name, reports it in the toast (spec 80 § 11.5).
pub fn new_material_instance_from_menu(state: &mut ContentBrowserState, parent: &str) {
    let Some(root) = state.project_root.clone() else {
        state.toast = Some("no project is open".to_owned());
        return;
    };
    let rel = next_new_asset_path(&root, &state.folder, NewAssetKind::MaterialInstance);
    match create_material_instance(&root, &rel, parent) {
        Ok(created) => {
            state.selected = Some(created.clone());
            state.toast = Some(format!("Created {created} (an instance of {parent})"));
        }
        Err(err) => state.toast = Some(err),
    }
}

// Tab completes a token, then cycles its candidates; Shift+Tab always leaves the field.
fn browser_search_box(ui: &mut egui::Ui, text: &mut String, label: egui::Id) -> egui::Response {
    let response = ui.text_edit_singleline(text).labelled_by(label);
    let vocab = crate::asset_search::CompletionVocab::content_browser();
    crate::asset_search::tab_complete(ui.ctx(), &response, text, &vocab);
    let candidates = crate::asset_search::active_completions(ui.ctx(), response.id, text);
    if response.has_focus() {
        let completes = !candidates.is_empty() || crate::asset_search::complete_token(text, &vocab).is_some();
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: completes,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
    }
    response
}

/// Draws the Content Browser panel; `open` is the level state for level double-clicks.
#[tracing::instrument(target = "regia_editor", skip_all, name = "content_panel")]
pub fn content_browser_panel(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ContentBrowserState,
    open: &mut OpenLevel,
) {
    // One draw per pass for the first browser: the docked tab and the drawer share its ids (ADR-8702).
    crate::drawers::note_content_draw(ui.ctx(), world);
    let view_before = world.get_resource::<ContentView>().copied().unwrap_or_default();
    let mut view = view_before;
    // Folder history and the agent's folder requests (ADR-7759).
    world
        .get_resource_or_insert_with(crate::content_favorites::ContentNav::default)
        .sync(state);
    // OS file drops (spec addendum § 8.7): the panel's area and the folder row under the pointer.
    let drop_rect = ui.max_rect();
    let mut drop_folder: Option<String> = None;
    // A pick from another panel becomes the selection (a list since ADR-7822).
    if let Some(picked) = state.selected.take() {
        world
            .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
            .set([picked]);
    }
    ui.horizontal(|ui| {
        ui.strong("Content");
        match &state.project_root {
            // The root without the Windows verbatim prefix, cut in the middle to fit, full on
            // hover (bugs 19 and 27, ADR-8586).
            Some(root) => crate::save_all_dialog::path_label(ui, &crate::save_all_dialog::user_path(root)),
            None => ui.weak("(no project open)"),
        }
    });
    let mut search_candidates = Vec::new();
    // Wrapped: at a large text scale View, Filter and the settings continue on a second row
    // instead of being cut at the panel's right edge (ADR-8679).
    ui.horizontal_wrapped(|ui| {
        let search = ui.label(egui::RichText::new("Search").small());
        let response = browser_search_box(ui, &mut state.search, search.id);
        if response.has_focus() {
            search_candidates = crate::asset_search::active_completions(ui.ctx(), response.id, &state.search);
        }
        ui.separator();
        if ui.button("Import…").clicked()
            && let Some(sources) = crate::file_dialog::FileDialog::new()
                .add_filter(
                    "Importable assets",
                    &crate::extension::host::import_file_types(world),
                )
                .pick_files()
        {
            let root = state.project_root.clone().unwrap_or_default();
            // Extension importers run here; the pipeline import runs in the background and can be
            // cancelled (`specs/19` § 7).
            let (rest, from_extensions) =
                crate::extension::host::run_importers(world, &root, &state.folder, &sources);
            for (path, message) in &from_extensions.errors {
                tracing::error!(target: "regia_editor", "import {path}: {message}");
            }
            if rest.is_empty() {
                state.toast = Some(from_extensions.summary());
            } else if crate::import_dialog::wants_dialog(&rest) {
                // Several files or a model: the import dialog first (ADR-7904).
                if let Err(error) = crate::import_dialog::open_for_files(world, &root, &state.folder, &rest) {
                    state.toast = Some(error.message);
                }
            } else {
                match crate::editor_jobs::spawn_import(world, root, state.folder.clone(), rest, None) {
                    Ok(job) => world.insert_resource(crate::editor_jobs::ContentImport(job)),
                    Err(err) => state.toast = Some(err),
                }
            }
        }
        // Import package… (spec addendum § 8.9, ADR-7788).
        crate::asset_package::import_button(ui, world);
        // The background import: Cancel while it runs, its summary once it ends.
        if let Some(import) = world.get_resource::<crate::editor_jobs::ContentImport>().cloned() {
            if import.0.running() {
                ui.spinner();
                ui.weak("importing…");
                if ui.button("Cancel import").clicked() {
                    import.0.cancel();
                }
            } else {
                world.remove_resource::<crate::editor_jobs::ContentImport>();
                state.toast = Some(match import.0.take_outcome() {
                    Some(Ok(report)) => {
                        crate::content_lint_jobs::note_imported(world, &report.imported);
                        report.summary()
                    }
                    Some(Err(err)) => err,
                    None => "import ended".to_string(),
                });
            }
        }
        if ui.button("Validate project").clicked() {
            state.asset_action = Some(AssetAction::ValidateProject);
        }
        ui.menu_button("New", |ui| {
            let data_types = world
                .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
                .map(|r| crate::data_asset::data_asset_types(&r.read()))
                .unwrap_or_default();
            for kind in NewAssetKind::ALL {
                if kind == NewAssetKind::DataAsset {
                    ui.menu_button(kind.label(), |ui| {
                        if data_types.is_empty() {
                            ui.weak(
                                "No registered data asset types (#[derive(Asset)] plus register_data_asset or #[reflect(Asset)])",
                            );
                        }
                        for type_path in &data_types {
                            let label = crate::inspector::egui_utils::pretty_type_name(type_path);
                            if ui.button(label).on_hover_text(type_path.as_str()).clicked() {
                                new_asset_from_menu(world, state, kind, Some(type_path.as_str()));
                                ui.close();
                            }
                        }
                    });
                } else if kind == NewAssetKind::MaterialInstance {
                    // Spec 80 § 11.5: pick the parent graph, then the instance lands in the
                    // shown folder with no overrides.
                    ui.menu_button(kind.label(), |ui| {
                        let graphs: Vec<String> = {
                            let mut index = world
                                .get_resource_or_insert_with(crate::content_index::ContentIndex::default);
                            index.ensure(state.project_root.as_deref());
                            index
                                .assets()
                                .iter()
                                .filter(|(path, _)| path.ends_with(".matgraph.ron"))
                                .map(|(path, _)| path.clone())
                                .collect()
                        };
                        if graphs.is_empty() {
                            ui.weak("No material graphs in the project");
                        }
                        for graph in graphs {
                            if ui.button(graph.as_str()).clicked() {
                                new_material_instance_from_menu(state, &graph);
                                ui.close();
                            }
                        }
                    });
                } else if ui.button(kind.label()).clicked() {
                    new_asset_from_menu(world, state, kind, None);
                    ui.close();
                }
            }
            // New ▸ Terrain… opens the Terrain panel's New terrain dialog on the shown folder: a
            // terrain is a folder of sources plus a cook, not one file (ADR-7727).
            if ui.button("Terrain…").clicked() {
                crate::terrain_create::open_dialog(world, Some(&state.folder));
                crate::editor_layout::LayoutRequests::push(
                    world,
                    crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::Terrain),
                );
                ui.close();
            }
        });
        if state.toast.is_some() {
            // Import and New wrote files: the next frame rescans the index.
            crate::content_index::invalidate(world);
        }
        if crate::a11y_audit::glyph_button(ui, "\u{21bb}", "Rescan the assets folder").clicked() {
            // A full rescan reads every `.meta` again too (ADR-7759).
            crate::content_index::invalidate_all(world);
        }
        ui.separator();
        ui.selectable_value(&mut view.grid, false, "List")
            .on_hover_text("One row per asset");
        ui.selectable_value(&mut view.grid, true, "Grid")
            .on_hover_text("Thumbnail tiles (cached in .regia/thumbs)");
        if view.grid {
            ui.add(
                egui::Slider::new(&mut view.tile, crate::thumb_cache::TILE_RANGE)
                    .show_value(false)
                    .text("size"),
            );
        }
        ui.separator();
        filter_menu(ui, world);
        crate::content_favorites::settings_menu(ui, world, state);
        if let Some(toast) = state.toast.take() {
            ui.weak(toast);
        }
    });
    if !search_candidates.is_empty() {
        ui.weak(format!(
            "Tab: {} · Shift+Tab: leave search",
            search_candidates.join(" · ")
        ));
    }
    filter_chips(ui, world);
    ui.separator();

    if state.project_root.is_none() {
        ui.weak("Open a project directory to browse assets/");
        if view != view_before {
            world.insert_resource(view);
        }
        return;
    }

    // The cached index (scanned once, re-checked against folder times once a second) instead
    // of two full tree walks per frame.
    let collection = world
        .get_resource::<crate::content_favorites::ContentNav>()
        .and_then(|nav| nav.collection.clone());
    let (folders, visible) = match &collection {
        // A collection lists its members instead of a folder (ADR-7759).
        Some(name) => crate::content_collections::panel_view(world, state, name),
        None => crate::content_index::panel_view(world, state),
    };
    let query_error = world
        .get_resource::<crate::content_index::ContentIndex>()
        .and_then(|index| index.query_error().cloned());
    // The grid draws from the thumbnail cache; a toggle takes effect from the next frame.
    let grid = view_before.grid;
    let mut thumbs = if grid {
        let mut thumbs = world
            .remove_resource::<crate::thumb_cache::ThumbCache>()
            .unwrap_or_default();
        thumbs.set_root(state.project_root.as_deref());
        thumbs.begin_frame();
        thumbs.pump();
        thumbs
    } else {
        crate::thumb_cache::ThumbCache::default()
    };
    let current = match &collection {
        Some(name) => format!("Collection {name}"),
        None if state.folder.is_empty() => "assets".to_string(),
        None => state.folder.clone(),
    };

    // Each Content Browser instance has its own folder pane (P-003; the plain id for the first).
    egui::Panel::left(crate::panel_instances::scoped_id(world, "content-folders"))
        .resizable(true)
        .default_size(200.0)
        .show(ui, |ui| {
            crate::content_library::folder_root(ui, world, state);
            // Favorites, collections and the collapsible folder tree (ADR-7759); the folder row under
            // the pointer is the target of an OS file drop (3222).
            if let Some(folder) = crate::content_favorites::folder_pane(ui, world, state, &folders) {
                drop_folder = Some(folder);
            }
        });

    // Provenance and review badges (ADR-2428), read once per index generation for the rows on screen.
    let generation = world
        .get_resource::<crate::content_index::ContentIndex>()
        .map_or(0, |index| index.generation);
    let mut badges = world
        .remove_resource::<crate::content_provenance::ProvenanceBadges>()
        .unwrap_or_default();
    badges.begin(state.project_root.as_deref(), generation);

    let mut bar_focus = false;
    // The order a Shift range and the arrow keys follow (ADR-7822).
    let listed: Vec<&str> = visible.iter().map(|(path, _)| path.as_str()).collect();
    let keyed = world.remove_resource::<ContentKeyed>().and_then(|keyed| keyed.0);
    let mut columns = 1;
    let central = egui::CentralPanel::default().show(ui, |ui| {
        bar_focus = crate::content_favorites::breadcrumb_bar(ui, world, state, &folders);
        ui.weak(format!("{current} · {} assets", visible.len()));
        if let Some(err) = badges.policy_error() {
            ui.colored_label(ui.visuals().warn_fg_color, err);
        }
        ui.separator();
        // A delete refused by the referencer check waits for the user here.
        if let Some((rel, referencers)) = state.pending_delete.clone() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(format!("Delete {rel}?"));
                ui.label("These files reference it:");
                for referencer in &referencers {
                    ui.weak(format!("· {referencer}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        state.pending_delete = None;
                    }
                    if ui.button("Delete anyway").clicked() {
                        match state.project_root.clone() {
                            Some(root) => match delete_asset_force(&root, &rel) {
                                Ok(()) => state.toast = Some(format!("Deleted {rel}")),
                                Err(err) => state.toast = Some(err),
                            },
                            None => state.toast = Some("no project open".to_string()),
                        }
                        state.pending_delete = None;
                    }
                    // Point every referencer at another asset of the same kind, then delete: one
                    // undo step (ADR-7822).
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::CONTENT_DELETE_REPLACE)
                        .clicked()
                    {
                        crate::content_actions::open_picker(
                            world,
                            crate::content_actions::PickerPurpose::DeleteReplace,
                            &rel,
                        );
                        state.pending_delete = None;
                    }
                });
            });
            ui.separator();
        }
        selection_bar(ui, world, state);
        bulk_confirmations(ui, world, state);
        // A rename/move waits for the destination here; confirming moves the source and its
        // `.meta`, rewrites the path references, and pushes an undo entry (spec 12 § 9, M11).
        if let Some(rel) = state.pending_rename.clone() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(format!("Rename / move {rel}"));
                ui.label("New path (under assets/):");
                ui.add(egui::TextEdit::singleline(&mut state.rename_target).desired_width(320.0));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        state.pending_rename = None;
                    }
                    if ui.button("Move").clicked() {
                        let target = state.rename_target.trim().to_string();
                        state.pending_rename = None;
                        if target.is_empty() || target == rel {
                            state.toast = Some("move cancelled: empty or unchanged path".to_string());
                            return;
                        }
                        match state.project_root.clone() {
                            Some(root) => match regia_asset::project_ops::move_asset(&root, &rel, &target) {
                                Ok(outcome) => {
                                    push_move_undo(world, root, rel.clone(), target.clone());
                                    crate::content_index::invalidate(world);
                                    state.toast = Some(format!(
                                        "Moved {rel} to {target} ({} reference(s) updated)",
                                        outcome.fixed_referencers.len()
                                    ));
                                }
                                Err(e) => state.toast = Some(format!("Move failed: {e}")),
                            },
                            None => state.toast = Some("no project open".to_string()),
                        }
                    }
                });
            });
            ui.separator();
        }
        if let Some(err) = &query_error {
            ui.colored_label(ui.visuals().warn_fg_color, &err.message);
        }
        if grid {
            let keyed_index = keyed
                .as_deref()
                .and_then(|path| visible.iter().position(|(p, _)| p == path));
            columns = asset_grid(
                ui,
                world,
                state,
                open,
                (&visible, &listed, keyed_index),
                (&mut thumbs, &mut badges),
                view.tile,
            );
            return;
        }
        // Only the rows in view are laid out (a folder of thousands of assets stays cheap).
        let row_height = ui.spacing().interact_size.y;
        let offset_id = ui.id().with("content-list-offset");
        let keyed_row = keyed.and_then(|path| visible.iter().position(|(p, _)| *p == path));
        let scroll = keep_in_view(ui, keyed_row, row_height + ui.spacing().item_spacing.y, offset_id);
        let output = scroll.show_rows(ui, row_height, visible.len(), |ui, rows| {
            let mut boxes = BoxHits::begin(ui);
            for (rel, kind) in visible.get(rows).unwrap_or_default() {
                let label = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or(rel);
                let chosen = selection_has(world, rel);
                let badge = badges.get(rel).cloned();
                let text = match &badge {
                    Some(badge) => format!("{} {label}   {}", kind_icon(*kind), badge.text),
                    None => format!("{} {label}", kind_icon(*kind)),
                };
                // The revision-control letter (ADR-7837).
                let text = crate::vcs::decorate_row(world, rel, text);
                let mut response = ui.selectable_label(chosen, text);
                // The unsaved dot and its spoken name (P-147, ADR-8147).
                if unsaved_assets(world, ui.ctx().cumulative_frame_nr()).contains(rel) {
                    paint_unsaved_dot(ui, response.rect, None);
                    let spoken = asset_spoken_name(label, true);
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, chosen, &spoken)
                    });
                }
                boxes.item(response.rect, rel);
                if response.hovered()
                    && let Some(root) = state.project_root.clone()
                {
                    let provenance = badge.as_ref().map(|b| format!("{}\n{}", b.text, b.hover));
                    let tip = crate::content_actions::tooltip_text(world, &root, rel, provenance.as_deref());
                    response = response.on_hover_text(tip);
                }
                asset_item(ui, world, state, open, (rel, *kind), &listed, &response);
            }
            boxes.finish(ui, world);
        });
        ui.ctx()
            .data_mut(|d| d.insert_temp(offset_id, output.state.offset.y));
    });
    world.insert_resource(badges);
    // Thumbnail refresh and capture, package export and import, asked for by the menus above (ADR-7788).
    if let Some(line) =
        crate::thumb_cache::run_pending(world).or_else(|| crate::asset_package::run_pending(world))
    {
        state.toast = Some(line);
    }
    crate::os_drop::note_browser(
        world,
        drop_rect,
        drop_folder,
        &state.folder,
        ui.ctx().cumulative_frame_nr(),
    );
    // The selection keys while the pointer is over the asset area and nothing else has the
    // keyboard (ADR-7822); the cursor row scrolls into view next frame.
    let over = ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|pos| central.response.rect.contains(pos));
    let any_key = ui.input(|i| {
        i.events
            .iter()
            .any(|e| matches!(e, egui::Event::Key { pressed: true, .. }))
    });
    if any_key {
        let ctx = ui.ctx().clone();
        let focus_elsewhere = ctx
            .memory(|m| m.focused())
            .and_then(|id| ctx.read_response(id))
            .is_some_and(|focused| !central.response.rect.contains_rect(focused.rect));
        let mut selection = world
            .remove_resource::<crate::content_actions::ContentSelection>()
            .unwrap_or_default();
        let moved = crate::content_actions::selection_keys(
            &ctx,
            &mut selection,
            &listed,
            columns,
            over && !focus_elsewhere,
        );
        world.insert_resource(selection);
        if moved.is_some() {
            world.insert_resource(ContentKeyed(moved));
        }
    }
    // Retarget… and Replace references with… (ADR-7822).
    crate::content_actions::picker_window(ui.ctx(), world, &mut state.toast);
    // Content ▸ Import settings… (ADR-7469): the window floats over the editor.
    crate::import_settings_panel::import_settings_window(ui.ctx(), world);
    // The import dialog (ADR-7904) floats over the editor too.
    crate::import_dialog::import_dialog_window(ui.ctx(), world);
    if state.toast.is_some() {
        // Delete and Reimport changed files: rescan next frame.
        crate::content_index::invalidate(world);
    }
    if grid {
        // A few mesh thumbnails per frame on the GPU; repaint while pictures are on their way.
        let device = world.get_resource::<regia_render::RenderDevice>().cloned();
        if let Some(device) = &device
            && thumbs.gpu_waiting() > 0
        {
            let library = world
                .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
                .clone();
            thumbs.render_meshes(device, &library, MESH_THUMBS_PER_FRAME);
        }
        if thumbs.busy() || (device.is_some() && thumbs.gpu_waiting() > 0) {
            ui.ctx().request_repaint();
        }
        world.insert_resource(thumbs);
    }
    if view != view_before {
        world.insert_resource(view);
    }
    // Alt+Left/Alt+Right and mouse buttons 4/5 over the browser step the folder history.
    content_history_keys(ui, world, state, bar_focus);
    // The Create variant dialog (drawn once per pass, here or by the Outliner; ADR-7614).
    crate::prefab_variant_edit::variant_dialog(ui.ctx(), world);
    // The asset diff window (drawn once per pass, here or by a graph panel; ADR-7799).
    crate::diff_view::show(ui.ctx());
}

/// The browser's back/forward shortcuts while the pointer is over it (and no other panel's widget
/// has the keyboard focus) or its breadcrumb bar has focus (one small function the scoped-shortcut
/// system takes over, scope: content).
fn content_history_keys(ui: &egui::Ui, world: &mut World, state: &mut ContentBrowserState, bar_focus: bool) {
    let ctx = ui.ctx();
    let focus_elsewhere = ctx
        .memory(|m| m.focused())
        .and_then(|id| ctx.read_response(id))
        .is_some_and(|focused| !ui.max_rect().contains_rect(focused.rect));
    let active = bar_focus || (ui.ui_contains_pointer() && !focus_elsewhere);
    let mut nav = world.get_resource_or_insert_with(crate::content_favorites::ContentNav::default);
    if crate::content_favorites::history_shortcuts(ui, &mut nav, active) {
        nav.sync(state);
    }
}

/// The toolbar's Filter menu: kind, tag and state chips (ADR-7759). The choices come from the
/// content index (kinds present, tags from `config/asset_tags.ron`).
fn filter_menu(ui: &mut egui::Ui, world: &mut World) {
    let active = world
        .get_resource::<crate::content_index::ContentFilter>()
        .map_or(0, |f| f.kinds.len() + f.tags.len() + f.states.len());
    let label = if active == 0 {
        "Filter".to_string()
    } else {
        format!("Filter ({active})")
    };
    ui.menu_button(label, |ui| {
        let mut filter = world
            .remove_resource::<crate::content_index::ContentFilter>()
            .unwrap_or_default();
        let (kinds, tags) = {
            let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
            (
                crate::content_index::kind_choices(&index),
                crate::content_index::tag_choices(&mut index),
            )
        };
        ui.strong("Kinds");
        for kind in kinds {
            let mut on = filter.kinds.iter().any(|k| k == kind);
            if ui.checkbox(&mut on, kind).changed() {
                filter.toggle_kind(kind);
            }
        }
        ui.separator();
        ui.strong("Tags");
        if tags.is_empty() {
            ui.weak("No tags (config/asset_tags.ron)");
        }
        for tag in tags {
            let mut on = filter.tags.contains(&tag);
            if ui.checkbox(&mut on, tag.as_str()).changed() {
                filter.toggle_tag(&tag);
            }
        }
        ui.separator();
        ui.strong("States");
        for asset_state in crate::asset_search::AssetState::ALL {
            let mut on = filter.states.contains(&asset_state);
            if ui.checkbox(&mut on, asset_state.label()).changed() {
                filter.toggle_state(asset_state);
            }
        }
        ui.separator();
        if ui.button("Clear filters").clicked() {
            filter = crate::content_index::ContentFilter::default();
            ui.close();
        }
        world.insert_resource(filter);
    });
}

/// The active chips under the toolbar; each button removes its chip.
fn filter_chips(ui: &mut egui::Ui, world: &mut World) {
    let Some(mut filter) = world
        .get_resource::<crate::content_index::ContentFilter>()
        .cloned()
        .filter(|f| !f.is_empty())
    else {
        return;
    };
    let before = filter.clone();
    ui.horizontal_wrapped(|ui| {
        ui.weak("Filters:");
        let kinds = filter.kinds.clone();
        for kind in kinds {
            if chip(ui, &kind).clicked() {
                filter.toggle_kind(&kind);
            }
        }
        let tags = filter.tags.clone();
        for tag in tags {
            if chip(ui, &format!("tag {tag}")).clicked() {
                filter.toggle_tag(&tag);
            }
        }
        let states = filter.states.clone();
        for asset_state in states {
            if chip(ui, asset_state.label()).clicked() {
                filter.toggle_state(asset_state);
            }
        }
    });
    if filter != before {
        world.insert_resource(filter);
    }
}

/// One chip: its text and a remove mark, named for screen readers.
fn chip(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let response = ui.small_button(format!("{text} \u{1f5d9}"));
    crate::a11y_audit::name(
        response,
        egui::WidgetType::Button,
        &format!("Remove the filter {text}"),
    )
}

/// Mesh thumbnails rendered per frame while the grid shows (each is one small offscreen draw).
pub const MESH_THUMBS_PER_FRAME: usize = 1;

/// How the Content Browser lists assets (an edit-world resource; the list is the default).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct ContentView {
    /// Thumbnail tiles instead of one row per asset.
    pub grid: bool,
    /// Tile edge in points (grid view).
    pub tile: f32,
}

impl Default for ContentView {
    fn default() -> Self {
        Self {
            grid: false,
            tile: 96.0,
        }
    }
}

/// The list icon of an asset kind.
fn kind_icon(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Level => "\u{1f5fa}",
        ContentKind::Prefab => "\u{1f4e6}",
        ContentKind::Mesh => "\u{25b6}",
        ContentKind::Other => "\u{00b7}",
    }
}

/// `name` cut to about `max_chars` characters with an ellipsis.
fn elide(name: &str, max_chars: usize) -> String {
    if name.chars().count() <= max_chars {
        return name.to_string();
    }
    let kept: String = name.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// The grid view: one tile per asset with its thumbnail (or its kind's icon), laid out only for
/// the rows in view, so the cache is asked only for pictures on screen.
#[tracing::instrument(target = "regia_editor", skip_all, name = "content_grid")]
fn asset_grid(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ContentBrowserState,
    open: &mut OpenLevel,
    (visible, listed, keyed): (&[crate::content_index::IndexedAsset], &[&str], Option<usize>),
    (thumbs, badges): (
        &mut crate::thumb_cache::ThumbCache,
        &mut crate::content_provenance::ProvenanceBadges,
    ),
    tile: f32,
) -> usize {
    let spacing = ui.spacing().item_spacing;
    let font = egui::TextStyle::Small.resolve(ui.style());
    let cell = egui::vec2(tile, tile + font.size + 4.0);
    let columns = ((ui.available_width() + spacing.x) / (cell.x + spacing.x))
        .floor()
        .max(1.0) as usize;
    let rows = visible.len().div_ceil(columns);
    let max_chars = (tile / (font.size * 0.55)).max(4.0) as usize;
    // FU-5: arrows move the keyboard focus (and the selection) between tiles; the tile to focus
    // next and the rows last in view live in egui memory (grid_keys). The keyboard cursor's tile
    // row stays in view too (ADR-7822); a focus move scrolls first.
    let (want, shown) = grid_keys::load(ui);
    let offset_id = ui.id().with("content-grid-offset");
    let mut area = keep_in_view(ui, keyed.map(|i| i / columns), cell.y + spacing.y, offset_id);
    if let Some(offset) = grid_keys::scroll_for(want, columns, shown, cell.y + spacing.y) {
        area = area.vertical_scroll_offset(offset);
    }
    let mut moved_to = None;
    let output = area.show_rows(ui, cell.y, rows, |ui, row_range| {
        grid_keys::store_shown(ui, row_range.clone());
        let mut boxes = BoxHits::begin(ui);
        for row in row_range {
            let start = row * columns;
            let end = (start + columns).min(visible.len());
            ui.horizontal(|ui| {
                for (offset, (rel, kind)) in visible.get(start..end).unwrap_or_default().iter().enumerate() {
                    let (rect, response) = ui.allocate_exact_size(cell, egui::Sense::click_and_drag());
                    let index = start + offset;
                    let name = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or(rel);
                    // The spoken name carries the unsaved state (P-147, ADR-8147).
                    let unsaved = unsaved_assets(world, ui.ctx().cumulative_frame_nr()).contains(rel);
                    let spoken = asset_spoken_name(name, unsaved);
                    response
                        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &spoken));
                    if want == Some(index) {
                        response.request_focus();
                        response.scroll_to_me(None);
                    }
                    if response.has_focus() {
                        crate::canvas_keys::focus_ring(ui.painter(), rect);
                        if let Some(to) = grid_keys::arrow_target(ui, index, columns, visible.len()) {
                            moved_to = Some(to);
                        }
                    }
                    boxes.item(rect, rel);
                    let chosen = selection_has(world, rel);
                    if ui.is_rect_visible(rect) {
                        let picture = egui::Rect::from_min_size(rect.min, egui::vec2(tile, tile));
                        let visuals = ui.visuals();
                        let fill = if chosen {
                            visuals.selection.bg_fill
                        } else if response.hovered() {
                            visuals.widgets.hovered.weak_bg_fill
                        } else {
                            visuals.faint_bg_color
                        };
                        let text_color = visuals.text_color();
                        ui.painter().rect_filled(picture, 4.0, fill);
                        // Requested only for tiles on screen; never waits for the picture.
                        thumbs.request(rel);
                        match thumbs.texture(ui.ctx(), rel) {
                            Some(texture) => {
                                egui::Image::new((texture, picture.size())).paint_at(ui, picture.shrink(2.0));
                            }
                            None => {
                                ui.painter().text(
                                    picture.center(),
                                    egui::Align2::CENTER_CENTER,
                                    kind_icon(*kind),
                                    egui::FontId::proportional(tile * 0.35),
                                    text_color,
                                );
                            }
                        }
                        // Animation clips loop on hover or keyboard focus (spec addendum § 8.8, ADR-7788).
                        if response.hovered() || response.has_focus() {
                            crate::asset_viewer::paint_tile_animation(
                                ui,
                                world,
                                state.project_root.as_deref(),
                                rel,
                                picture,
                            );
                        }
                        ui.painter().text(
                            egui::pos2(picture.center().x, picture.bottom() + 2.0),
                            egui::Align2::CENTER_TOP,
                            elide(name, max_chars),
                            font.clone(),
                            text_color,
                        );
                        // A corner mark for a policy problem or a pending review (ADR-2428).
                        if let Some(badge) = badges.get(rel) {
                            let mark = if badge.policy_errors > 0 {
                                Some(("\u{26a0}", ui.visuals().warn_fg_color))
                            } else if badge.needs_review || badge.text.contains("review pending") {
                                Some(("\u{2022}", ui.visuals().warn_fg_color))
                            } else {
                                None
                            };
                            if let Some((mark, color)) = mark {
                                ui.painter().text(
                                    picture.right_top() + egui::vec2(-4.0, 4.0),
                                    egui::Align2::RIGHT_TOP,
                                    mark,
                                    font.clone(),
                                    color,
                                );
                            }
                        }
                    }
                    if response.has_focus() {
                        // The keyboard cursor's ring (tiles are painted, not widgets with a frame).
                        ui.painter().rect_stroke(
                            rect,
                            4.0,
                            ui.visuals().selection.stroke,
                            egui::StrokeKind::Outside,
                        );
                    }
                    // The revision-control letter in the top-left corner (ADR-7837).
                    if ui.is_rect_visible(rect) {
                        let picture = egui::Rect::from_min_size(rect.min, egui::vec2(tile, tile));
                        crate::vcs::paint_tile_badge(ui, world, rel, picture, &font);
                        if unsaved {
                            paint_unsaved_dot(ui, rect, Some(picture));
                        }
                    }
                    asset_item(ui, world, state, open, (rel, *kind), listed, &response);
                    if response.hovered()
                        && let Some(root) = state.project_root.clone()
                    {
                        // Kind, sizes, triangles/dimensions/length, referencers, provenance (ADR-7822).
                        let provenance = badges.get(rel).map(|b| format!("{}\n{}", b.text, b.hover));
                        let tip =
                            crate::content_actions::tooltip_text(world, &root, rel, provenance.as_deref());
                        response.on_hover_text(tip);
                    }
                }
            });
        }
        boxes.finish(ui, world);
    });
    ui.ctx()
        .data_mut(|d| d.insert_temp(offset_id, output.state.offset.y));
    grid_keys::store_want(ui, moved_to);
    if let Some(rel) = moved_to.and_then(|i| visible.get(i)).map(|(rel, _)| rel.clone()) {
        state.selected = Some(rel);
    }
    columns
}

/// The content grid's keyboard route (FU-5): arrows move the focus between tiles, a row or a
/// column at a time, scrolling the target into view; the focused tile draws the focus ring and
/// becomes the selected asset.
mod grid_keys {
    /// The memory id of the grid's keyboard state.
    fn id() -> egui::Id {
        egui::Id::new("content-grid-keys")
    }

    /// `(tile to focus this frame, rows laid out last frame)`.
    pub(super) fn load(ui: &egui::Ui) -> (Option<usize>, std::ops::Range<usize>) {
        ui.data(|d| d.get_temp::<(Option<usize>, std::ops::Range<usize>)>(id()))
            .unwrap_or((None, 0..0))
    }

    /// Remembers the rows in view.
    pub(super) fn store_shown(ui: &egui::Ui, shown: std::ops::Range<usize>) {
        ui.data_mut(|d| {
            let entry = d.get_temp_mut_or_default::<(Option<usize>, std::ops::Range<usize>)>(id());
            entry.1 = shown;
            entry.0 = None;
        });
    }

    /// Remembers the tile to focus on the next frame.
    pub(super) fn store_want(ui: &egui::Ui, want: Option<usize>) {
        ui.data_mut(|d| {
            d.get_temp_mut_or_default::<(Option<usize>, std::ops::Range<usize>)>(id())
                .0 = want
        });
    }

    /// The scroll offset that lays out the row of `want` when it is out of view.
    pub(super) fn scroll_for(
        want: Option<usize>,
        columns: usize,
        shown: std::ops::Range<usize>,
        pitch: f32,
    ) -> Option<f32> {
        let row = want? / columns.max(1);
        (!shown.contains(&row)).then_some(row as f32 * pitch)
    }

    /// The tile an arrow on focused tile `index` moves to, consuming the key.
    pub(super) fn arrow_target(ui: &egui::Ui, index: usize, columns: usize, len: usize) -> Option<usize> {
        use egui::{Key, Modifiers};
        let (pressed, target) = ui.input_mut(|i| {
            let mut pressed = false;
            let mut to = None;
            for (key, step) in [
                (Key::ArrowLeft, -1),
                (Key::ArrowRight, 1),
                (Key::ArrowUp, -(columns as isize)),
                (Key::ArrowDown, columns as isize),
            ] {
                if i.consume_key(Modifiers::NONE, key) {
                    pressed = true;
                    to = index.checked_add_signed(step).filter(|t| *t < len);
                }
            }
            (pressed, to)
        });
        if pressed {
            // egui moves the focus with the arrows too: the grid moves it itself.
            ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        }
        target
    }
}

/// A vertical scroll area that brings `row` (of `stride` points each) into view when it is outside
/// the view the last frame showed (its offset is kept under `offset_id`). Rows outside the view
/// are not laid out, so the keyboard cursor cannot scroll itself into view.
fn keep_in_view(ui: &egui::Ui, row: Option<usize>, stride: f32, offset_id: egui::Id) -> egui::ScrollArea {
    let scroll = egui::ScrollArea::vertical();
    let Some(row) = row else {
        return scroll;
    };
    let top = row as f32 * stride;
    let view = ui.available_height().max(stride);
    let offset = ui.ctx().data(|d| d.get_temp::<f32>(offset_id)).unwrap_or(0.0);
    if top < offset {
        scroll.vertical_scroll_offset(top)
    } else if top + stride > offset + view {
        scroll.vertical_scroll_offset(top + stride - view)
    } else {
        scroll
    }
}

/// The asset the arrow keys moved to last frame (scrolled into view this frame).
#[derive(Resource, Debug, Default)]
struct ContentKeyed(Option<String>);

/// Whether `rel` is in the Content Browser's selection.
fn selection_has(world: &World, rel: &str) -> bool {
    world
        .get_resource::<crate::content_actions::ContentSelection>()
        .is_some_and(|s| s.contains(rel))
}

/// Box select over the list or the grid (ADR-7822): a drag that starts on the empty background
/// selects every row or tile it covers (Ctrl adds to the selection); a click there clears it.
struct BoxHits {
    background: egui::Response,
    items: Vec<(egui::Rect, String)>,
}

/// The temp-memory record of a box drag: where it started, the selection before it, and whether
/// it adds.
type BoxDrag = (egui::Pos2, Vec<String>, bool);

impl BoxHits {
    /// Senses the visible background; rows and tiles laid out after it win their own clicks.
    fn begin(ui: &mut egui::Ui) -> Self {
        let rect = ui.max_rect().intersect(ui.clip_rect());
        let background = ui.interact(
            rect,
            ui.id().with("content-box-select"),
            egui::Sense::click_and_drag(),
        );
        Self {
            background,
            items: Vec::new(),
        }
    }

    /// One laid-out row or tile.
    fn item(&mut self, rect: egui::Rect, rel: &str) {
        self.items.push((rect, rel.to_string()));
    }

    /// Updates the selection from the box and paints it.
    fn finish(self, ui: &mut egui::Ui, world: &mut World) {
        let id = ui.id().with("content-box-drag");
        let command = ui.input(|i| i.modifiers.command);
        if self.background.clicked() && !command {
            world
                .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
                .clear();
        }
        if self.background.drag_started() {
            let origin = ui
                .input(|i| i.pointer.press_origin())
                .unwrap_or(self.background.rect.min);
            let before = world
                .get_resource::<crate::content_actions::ContentSelection>()
                .map(|s| s.paths().to_vec())
                .unwrap_or_default();
            ui.data_mut(|d| d.insert_temp::<BoxDrag>(id, (origin, before, command)));
        }
        let Some((origin, before, additive)) = ui.data(|d| d.get_temp::<BoxDrag>(id)) else {
            return;
        };
        if !self.background.dragged() {
            ui.data_mut(|d| d.remove::<BoxDrag>(id));
            return;
        }
        let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let area = egui::Rect::from_two_pos(origin, pointer);
        let visuals = ui.visuals();
        ui.painter().rect(
            area,
            0.0,
            visuals.selection.bg_fill.gamma_multiply(0.25),
            visuals.selection.stroke,
            egui::StrokeKind::Inside,
        );
        let hits: Vec<String> = self
            .items
            .iter()
            .filter(|(rect, _)| rect.intersects(area))
            .map(|(_, rel)| rel.clone())
            .collect();
        world
            .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
            .box_select(&before, &hits, additive);
    }
}

/// The selection's bulk actions in a row's menu (ADR-7822). Validate, Re-import and Save run at
/// once; Move to… and Delete… ask first (`bulk_confirmations`).
fn bulk_menu(ui: &mut egui::Ui, world: &mut World, state: &mut ContentBrowserState, selected: &[String]) {
    use crate::content_actions::{BulkAction, run_bulk};
    ui.weak(format!("{} selected", selected.len()));
    let Some(root) = state.project_root.clone() else {
        return;
    };
    let mut run = |world: &mut World, action: BulkAction| {
        state.toast = Some(match run_bulk(world, &root, selected, &action) {
            Ok(outcome) => outcome.summary,
            Err(error) => error.to_string(),
        });
    };
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_VALIDATE).clicked() {
        run(world, BulkAction::Validate);
        ui.close();
    }
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_REIMPORT).clicked() {
        run(world, BulkAction::Reimport);
        ui.close();
    }
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_SAVE).clicked() {
        run(world, BulkAction::Save);
        ui.close();
    }
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_MOVE).clicked() {
        let folder = if state.folder.is_empty() {
            "assets".to_string()
        } else {
            state.folder.clone()
        };
        state.pending_bulk_move = Some((selected.to_vec(), folder));
        ui.close();
    }
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_DELETE).clicked() {
        let referencers = crate::content_actions::referencers_of_all(&root, selected);
        state.pending_bulk_delete = Some((selected.to_vec(), referencers));
        ui.close();
    }
    // Edit selection in grid and Export selection with dependencies… (P-145, ADR-8147). egui may
    // run the frame's UI twice: one click acts once.
    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_BULK_EDIT_IN_GRID).clicked()
        && crate::asset_package::first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
    {
        state.toast = Some(
            match crate::bulk_edit_panel::edit_selection_in_grid(world, &root, selected) {
                Ok(opened) if opened.skipped.is_empty() => {
                    format!("{} assets in the Bulk Edit grid", opened.rows.len())
                }
                Ok(opened) => format!(
                    "{} assets in the Bulk Edit grid ({} not data assets skipped)",
                    opened.rows.len(),
                    opened.skipped.len()
                ),
                Err(error) => format!("{} ({})", error.message, error.code),
            },
        );
        ui.close();
    }
    crate::asset_package::export_selection_item(ui, world, selected);
    let names: Vec<String> = world
        .get_resource::<crate::content_collections::ContentLibrary>()
        .map(|library| {
            library
                .all()
                .into_iter()
                .filter(|(_, c)| c.query.is_none())
                .map(|(_, c)| c.name.clone())
                .collect()
        })
        .unwrap_or_default();
    if !names.is_empty() {
        crate::agent_parity::menu(ui, crate::agent_parity::CONTENT_BULK_COLLECTION.label, |ui| {
            for name in names {
                if crate::agent_parity::action_button_text(
                    ui,
                    &crate::agent_parity::CONTENT_BULK_COLLECTION,
                    name.clone(),
                )
                .clicked()
                {
                    state.toast = Some(crate::content_collections::add_paths(world, &name, selected));
                    ui.close();
                }
            }
        });
    }
}

/// The project-relative files of every open document with unsaved edits (the Save all dialog's
/// rows: the level, every asset editor's documents and the shell's material), gathered once per
/// egui frame for the Content Browser's unsaved dots (P-147, ADR-8147).
#[derive(Resource, Debug, Clone, Default)]
pub struct UnsavedAssets {
    /// The egui frame the set was gathered in.
    frame: Option<u64>,
    /// `assets/...` paths, `/`-separated.
    paths: std::collections::BTreeSet<String>,
}

impl UnsavedAssets {
    /// Whether `rel` (`assets/...`) has unsaved edits in an open editor.
    #[must_use]
    pub fn contains(&self, rel: &str) -> bool {
        self.paths.contains(rel)
    }
}

/// The unsaved files, gathered again when `frame` is a new egui frame.
pub fn unsaved_assets(world: &mut World, frame: u64) -> UnsavedAssets {
    if let Some(cached) = world
        .get_resource::<UnsavedAssets>()
        .filter(|cached| cached.frame == Some(frame))
    {
        return cached.clone();
    }
    let _span = tracing::info_span!(target: "regia_editor", "content_unsaved_assets").entered();
    let root = crate::mesh_source::project_root(world);
    let mut paths = std::collections::BTreeSet::new();
    for doc in crate::save_all_dialog::all_documents(world) {
        let Some(path) = doc.path else {
            continue;
        };
        let path = path.replace('\\', "/");
        // A path outside the project (or absolute under it) is made project-relative.
        let rel = match root.as_deref() {
            Some(root) if Path::new(&path).is_absolute() => Path::new(&path)
                .strip_prefix(root)
                .map(slash_path)
                .unwrap_or(path),
            _ => path,
        };
        // Some editors keep their file relative to `assets/`.
        if rel.starts_with("assets/") {
            paths.insert(rel);
        } else {
            paths.insert(format!("assets/{rel}"));
        }
    }
    let set = UnsavedAssets {
        frame: Some(frame),
        paths,
    };
    world.insert_resource(set.clone());
    set
}

/// The accessible name of an asset row or tile: the file name, and ", unsaved changes" when an
/// open editor holds unsaved edits of it.
#[must_use]
pub fn asset_spoken_name(name: &str, unsaved: bool) -> String {
    if unsaved {
        format!("{name}, unsaved changes")
    } else {
        name.to_string()
    }
}

/// Paints the unsaved dot of a row (at its right end) or a tile (the picture's bottom-right
/// corner) in the theme's unsaved color, sized with the UI scale.
fn paint_unsaved_dot(ui: &egui::Ui, rect: egui::Rect, tile: Option<egui::Rect>) {
    let radius = (ui.spacing().interact_size.y * 0.18).max(2.0);
    let center = match tile {
        Some(picture) => picture.right_bottom() + egui::vec2(-radius - 3.0, -radius - 3.0),
        None => egui::pos2(rect.right() - radius - 4.0, rect.center().y),
    };
    let color = crate::theme::ui_tokens(ui.ctx()).accents.unsaved;
    ui.painter().circle_filled(center, radius, color);
}

/// The selection bar above the asset list (P-145, ADR-8147): with several assets selected, the
/// row menu's bulk actions as a row of buttons, so Tab reaches every one of them (the row menu
/// needs a pointer).
fn selection_bar(ui: &mut egui::Ui, world: &mut World, state: &mut ContentBrowserState) {
    let selected: Vec<String> = world
        .get_resource::<crate::content_actions::ContentSelection>()
        .map(|s| s.paths().to_vec())
        .unwrap_or_default();
    if selected.len() < 2 || state.project_root.is_none() {
        return;
    }
    ui.horizontal_wrapped(|ui| bulk_menu(ui, world, state, &selected));
    ui.separator();
}

/// The bulk delete and bulk move confirmations, above the asset list (ADR-7822).
fn bulk_confirmations(ui: &mut egui::Ui, world: &mut World, state: &mut ContentBrowserState) {
    use crate::content_actions::{BulkAction, run_bulk};
    let Some(root) = state.project_root.clone() else {
        return;
    };
    if let Some((paths, referencers)) = state.pending_bulk_delete.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong(format!("Delete {} assets?", paths.len()));
            for path in paths.iter().take(12) {
                ui.weak(format!("· {path}"));
            }
            if paths.len() > 12 {
                ui.weak(format!("… and {} more", paths.len() - 12));
            }
            if referencers.is_empty() {
                ui.label("No other file references them.");
            } else {
                ui.label(format!("{} file(s) reference them:", referencers.len()));
                for referencer in &referencers {
                    ui.weak(format!("· {referencer}"));
                }
            }
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(ui, &crate::agent_parity::CONTENT_ACTION_CANCEL)
                    .clicked()
                {
                    state.pending_bulk_delete = None;
                }
                let delete =
                    crate::agent_parity::action_button(ui, &crate::agent_parity::CONTENT_BULK_DELETE_CONFIRM);
                if delete.clicked() {
                    state.toast = Some(
                        match run_bulk(world, &root, &paths, &BulkAction::Delete { confirm: true }) {
                            Ok(outcome) => outcome.summary,
                            Err(error) => error.to_string(),
                        },
                    );
                    state.pending_bulk_delete = None;
                }
            });
        });
        ui.separator();
    }
    if let Some((paths, mut folder)) = state.pending_bulk_move.clone() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong(format!("Move {} assets", paths.len()));
            let label = ui.label("Into folder (under assets/):");
            ui.add(egui::TextEdit::singleline(&mut folder).desired_width(320.0))
                .labelled_by(label.id);
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(ui, &crate::agent_parity::CONTENT_ACTION_CANCEL)
                    .clicked()
                {
                    state.pending_bulk_move = None;
                    return;
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::CONTENT_BULK_MOVE_CONFIRM)
                    .clicked()
                {
                    state.toast = Some(
                        match run_bulk(
                            world,
                            &root,
                            &paths,
                            &BulkAction::MoveTo(folder.trim().to_string()),
                        ) {
                            Ok(outcome) => outcome.summary,
                            Err(error) => error.to_string(),
                        },
                    );
                    state.pending_bulk_move = None;
                    return;
                }
                state.pending_bulk_move = Some((paths.clone(), folder.clone()));
            });
        });
        ui.separator();
    }
}

/// What a list row or grid tile does: drag payload, context menu, double-click actions.
fn asset_item(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ContentBrowserState,
    open: &mut OpenLevel,
    (rel, kind): (&str, ContentKind),
    listed: &[&str],
    response: &egui::Response,
) {
    // A click selects the asset: what the Inspector's Use selected takes (ADR-7786).
    if response.clicked() {
        state.selected = Some(rel.to_string());
    }
    // A click selects (Ctrl toggles, Shift selects the range); a right click on an unselected
    // asset selects it first, so the menu acts on what it shows (ADR-7822).
    if response.clicked() || (response.secondary_clicked() && !selection_has(world, rel)) {
        let mods = if response.clicked() {
            crate::content_actions::ClickMods::from_egui(ui.input(|i| i.modifiers))
        } else {
            crate::content_actions::ClickMods::default()
        };
        world
            .get_resource_or_insert_with(crate::content_actions::ContentSelection::default)
            .click(rel, mods, listed);
    }
    // Drag payload: drop into the viewport to spawn/open.
    if response.drag_started() {
        egui::DragAndDrop::set_payload(
            ui.ctx(),
            ContentDrag {
                path: rel.to_string(),
                kind,
            },
        );
    }
    response.context_menu(|ui| {
        // Several selected: the bulk actions on all of them come first (ADR-7822).
        let selected: Vec<String> = world
            .get_resource::<crate::content_actions::ContentSelection>()
            .map(|s| s.paths().to_vec())
            .unwrap_or_default();
        if selected.len() > 1 && selected.iter().any(|p| p == rel) {
            bulk_menu(ui, world, state, &selected);
            ui.separator();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(rel.to_string());
            ui.close();
        }
        // Spec 19 § 3 "select by asset": every level entity whose fields reference this asset.
        if ui.button("Select in level").clicked() {
            let found = select_by_asset(world, rel);
            state.toast = Some(if found.is_empty() {
                format!("no entity in the level uses {rel}")
            } else {
                format!("selected {} entities using {rel}", found.len())
            });
            ui.close();
        }
        if ui.button("Why is this in its chunk?").clicked() {
            let mut chunks = world
                .remove_resource::<crate::chunks_panel::ContentChunksPanel>()
                .unwrap_or_default();
            if chunks.view.is_none() {
                chunks.refresh(state.project_root.as_deref());
            }
            state.toast = Some(match chunks.ask_why(rel) {
                Ok(answer) => format!(
                    "{rel} is in chunk {} ({}); details in the Content Chunks tab",
                    answer.chunk, answer.reason
                ),
                Err(e) => e.message,
            });
            world.insert_resource(chunks);
            ui.close();
        }
        // Spec 32 § 8; the action runs after the row, the panels show the result.
        let actions = [
            ("Validate", AssetAction::Validate(rel.to_string())),
            (
                "Validate with dependencies",
                AssetAction::ValidateWithDependencies(rel.to_string()),
            ),
            ("Reference graph", AssetAction::ReferenceGraph(rel.to_string())),
            ("Size map", AssetAction::SizeMap(rel.to_string())),
            ("Audit to CSV", AssetAction::AuditCsv(rel.to_string())),
        ];
        for (label, action) in actions {
            if ui.button(label).clicked() {
                state.asset_action = Some(action);
                ui.close();
            }
        }
        // Tools extensions registered for this file type (ADR-9850).
        if let Some(message) = crate::extension::actions::asset_menu(ui, world, rel) {
            state.toast = Some(message);
        }
        if ui.button("Show in file manager").clicked() {
            if let Some(root) = state.project_root.clone() {
                open_in_file_manager(&root.join(rel));
            }
            ui.close();
        }
        if ui.button("Reimport (forget cache)").clicked() {
            if let Some(root) = state.project_root.clone() {
                // Re-cook with the cache ignored, keeping the `.meta` (id, import settings,
                // provenance): review rev-kimi-8 finding 3.
                match crate::asset_ops_agent::recook_source(&root, &root.join(rel)) {
                    Ok(report) => {
                        state.toast = Some(report_summary(&report));
                    }
                    Err(err) => state.toast = Some(err.to_string()),
                }
            }
            ui.close();
        }
        if crate::import_settings_panel::ImporterKind::of_path(rel).is_some()
            && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_IMPORT_SETTINGS)
                .clicked()
        {
            if let Some(root) = state.project_root.clone() {
                let mut panel = world
                    .get_resource_or_insert_with(crate::import_settings_panel::ImportSettingsPanel::default);
                if let Err(error) = panel.open_path(&root, rel) {
                    state.toast = Some(error.message);
                }
            }
            ui.close();
        }
        // Re-import with options… on an imported model: the import dialog (ADR-7904).
        if crate::import_settings_panel::ImporterKind::of_path(rel).is_some_and(|k| {
            matches!(
                k,
                crate::import_settings_panel::ImporterKind::Gltf
                    | crate::import_settings_panel::ImporterKind::Fbx
                    | crate::import_settings_panel::ImporterKind::Obj
            )
        }) && crate::agent_parity::action_menu_item(
            ui,
            &crate::agent_parity::CONTENT_REIMPORT_WITH_OPTIONS,
        )
        .clicked()
        {
            if let Some(root) = state.project_root.clone()
                && let Err(error) = crate::import_dialog::open_for_reimport(world, &root, rel)
            {
                state.toast = Some(error.message);
            }
            ui.close();
        }
        // Create variant… on a prefab or variant (spec 83 § 2.4, ADR-7614): an empty variant.
        if (rel.ends_with(".prefab.ron") || crate::prefab_variant_edit::is_variant_path(Path::new(rel)))
            && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_CREATE_VARIANT)
                .clicked()
        {
            let base = rel.strip_prefix("assets/").unwrap_or(rel);
            crate::prefab_variant_edit::open_variant_dialog(ui.ctx(), base, None);
            ui.close();
        }
        // Mesh helpers on a glTF source (v0.6 UI parity P-088, ADR-7771): the collision request, the
        // lightmap UV (opens the mesh viewer on it) and the nav preview.
        if crate::asset_viewer::is_gltf_source(rel) {
            if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_ADD_COLLISION)
                .clicked()
            {
                state.toast = Some(
                    match crate::asset_viewer::add_collision(
                        world,
                        rel,
                        crate::asset_viewer::CollisionShape::Convex,
                        true,
                    ) {
                        Ok(out) => format!(
                            "collision for {rel}: job {}",
                            out["job"].as_str().unwrap_or("started")
                        ),
                        Err(e) => e.message,
                    },
                );
                ui.close();
            }
            if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_LIGHTMAP_UV).clicked()
            {
                let mut queue =
                    world.get_resource_or_insert_with(crate::asset_viewer::MeshHelperQueue::default);
                if !queue.lightmap_uv.iter().any(|p| p == rel) {
                    queue.lightmap_uv.push(rel.to_string());
                }
                state.view_request = Some(rel.to_string());
                ui.close();
            }
            if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_NAV_PREVIEW).clicked()
            {
                state.toast = Some(match crate::asset_viewer::build_nav_preview(world, true) {
                    Ok(out) => format!("nav preview: job {}", out["job"].as_str().unwrap_or("started")),
                    Err(e) => e.message,
                });
                ui.close();
            }
        }
        // Edit in grid on a data asset (ADR-7787): the folder's assets of its type in the Bulk Edit grid.
        if rel.ends_with(&format!(".{}", crate::data_asset::DATA_ASSET_EXTENSION))
            && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::CONTENT_EDIT_IN_GRID).clicked()
        {
            if let Some(root) = state.project_root.clone() {
                state.toast = Some(crate::bulk_edit_panel::edit_assets_in_grid(world, &root, rel));
            }
            ui.close();
        }
        // The file's revision history in the Revision Control window (ADR-7837).
        if ui.button("Show history").clicked() {
            crate::vcs::panel::show_history(world, Some(rel));
            ui.close();
        }
        // Lock / Unlock of a lockable file (git LFS, ADR-7864).
        crate::vcs::locks::context_menu_items(ui, world, rel);
        if ui.button("Rename / Move…").clicked() {
            state.pending_rename = Some(rel.to_string());
            state.rename_target = rel.to_string();
            ui.close();
        }
        if ui.button("Delete…").clicked() {
            if let Some(root) = state.project_root.clone() {
                match delete_asset(&root, rel) {
                    Ok(()) => state.toast = Some(format!("Deleted {rel}")),
                    Err(referencers) if !referencers.is_empty() => {
                        state.pending_delete = Some((rel.to_string(), referencers));
                    }
                    Err(_) => state.toast = Some(format!("Delete failed for {rel}")),
                }
            }
            ui.close();
        }
        // Thumbnails and packages (spec addendum §§ 8.8-8.9, ADR-7788): one appended block.
        ui.separator();
        crate::thumb_cache::thumbnail_menu_items(ui, world, rel);
        crate::asset_package::export_menu_item(ui, world, rel);
        // Diff against HEAD… / Diff against revision… on a RON asset (P-182, ADR-7799).
        crate::diff_view::content_menu_items(ui, state.project_root.as_deref(), rel);
    });
    // One dispatcher for every kind (ADR-7725): the same route as a log link and editor.open_asset.
    if response.double_clicked()
        && let Some(toast) = crate::asset_open::open_from_browser(world, state, open, rel)
    {
        tracing::warn!(target: "regia_editor", "open {rel}: {toast}");
        state.toast = Some(toast);
    }
}

/// A folder row's menu (spec 32 § 8, ADR-7725): Validate, Reference graph, Size map and Audit,
/// scoped to what is under the folder. The action runs after the panel draws, as a file row's.
pub(crate) fn folder_context_menu(response: &egui::Response, state: &mut ContentBrowserState, folder: &str) {
    use crate::agent_parity as parity;
    response.context_menu(|ui| {
        let actions = [
            (
                &parity::CONTENT_FOLDER_VALIDATE,
                AssetAction::Validate(folder.to_owned()),
            ),
            (
                &parity::CONTENT_FOLDER_REFERENCE_GRAPH,
                AssetAction::ReferenceGraph(folder.to_owned()),
            ),
            (
                &parity::CONTENT_FOLDER_SIZE_MAP,
                AssetAction::SizeMap(folder.to_owned()),
            ),
            (
                &parity::CONTENT_FOLDER_AUDIT,
                AssetAction::AuditCsv(folder.to_owned()),
            ),
        ];
        for (action, asset_action) in actions {
            if parity::action_menu_item(ui, action).clicked() {
                state.asset_action = Some(asset_action);
                ui.close();
            }
        }
    });
}

/// The pointer ray of the viewport pane under the cursor this frame.
///
/// The viewport writes it while the pointer is over a pane. [`handle_viewport_drop`] runs after
/// the UI, when that pane's rectangle is gone, so the drop reads this instead.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ViewportDropAim {
    /// Ray origin, world space.
    pub origin: Vec3,
    /// Unit ray direction.
    pub direction: Vec3,
    /// Distance along the ray to the pane's orbit target. Used when the ray hits nothing.
    pub target_depth: f32,
}

/// Handles a content drag released over the viewport: opens a level, spawns a prefab, places a
/// mesh or a sound, or assigns a material. Returns a short toast line for the status bar.
#[tracing::instrument(target = "regia_editor", skip_all, name = "content_drop")]
pub fn handle_viewport_drop(
    ctx: &egui::Context,
    world: &mut World,
    state: &ContentBrowserState,
    open: &mut OpenLevel,
) -> Option<String> {
    // The ghost lives only while a drag is in flight over a pane (P-067); a release or a
    // cancelled drag removes it.
    let in_flight = egui::DragAndDrop::payload::<ContentDrag>(ctx).is_some();
    if !in_flight || ctx.input(|i| i.pointer.any_released()) {
        world.remove_resource::<DropPreview>();
    }
    if !ctx.input(|i| i.pointer.any_released()) {
        return None;
    }
    let payload = egui::DragAndDrop::payload::<ContentDrag>(ctx)?;
    let drag = (*payload).clone();
    egui::DragAndDrop::clear_payload(ctx);
    let root = state.project_root.clone()?;
    // An asset no drop places (not a material or a sound) gives no toast, as `ContentKind::Other`
    // documents.
    if matches!(drag.kind, ContentKind::Other) && !is_material_path(&drag.path) && !is_sound_path(&drag.path)
    {
        return None;
    }
    Some(apply_content_drop(world, open, &root, &drag.path, drag.kind))
}

/// Places `path` (project-relative, `assets/...`) the way a viewport drop does.
///
/// A mesh becomes an entity with [`MeshSource`] at the drop hit. When the glTF `.meta` asks for
/// collision, that entity also gets a triangle-mesh [`regia::physics::Collider`] built from the
/// loaded model; a model that fails to load keeps the source and gets no collider. A material
/// sets `MeshSource.material` on the mesh under the cursor and does not spawn. A sound spawns an
/// [`regia_audio::AudioEmitter`] whose `sound_name` is the path. Anything else is not placed.
pub fn apply_content_drop(
    world: &mut World,
    open: &mut OpenLevel,
    project_root: &Path,
    path: &str,
    kind: ContentKind,
) -> String {
    let full = project_root.join(path);
    match kind {
        ContentKind::Level => match level_io::open_level_file(world, &full) {
            Ok(level) => {
                *open = level;
                level_io::reset_edit_state(world);
                format!("Opened {path}")
            }
            Err(err) => {
                tracing::error!(target: "regia_editor", "{err}");
                format!("Open failed: {err}")
            }
        },
        // At the drop point, where the drag's ghost stood (P-067).
        ContentKind::Prefab => {
            let at = drop_position(world).0;
            spawn_prefab_from_file(world, &full, path, at).unwrap_or_else(|| format!("Spawn failed: {path}"))
        }
        ContentKind::Mesh => place_mesh(world, path, &full),
        ContentKind::Other if is_material_path(path) => assign_dropped_material(world, path),
        ContentKind::Other if is_sound_path(path) => place_sound(world, path),
        ContentKind::Other => format!("no viewport drop for {path}"),
    }
}

fn is_material_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".material.ron") || path.ends_with(".matgraph.ron") || path.ends_with(".matinst.ron")
}

fn is_sound_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".wav") || path.ends_with(".ogg") || path.ends_with(".flac") || path.ends_with(".mp3")
}

/// Mesh hit, else terrain, else the point on the ray at the orbit-target depth. Origin when
/// the frame stored no ray.
pub(crate) fn drop_position(world: &World) -> (Vec3, Option<Entity>) {
    let Some(aim) = world.get_resource::<ViewportDropAim>().copied() else {
        return (Vec3::ZERO, None);
    };
    if aim.direction.length_squared() < 1.0e-8 {
        return (Vec3::ZERO, None);
    }
    if let Some(hit) = crate::picking::raycast_world(
        world,
        aim.origin,
        aim.direction,
        crate::picking::FaceCull::None,
        &|_| true,
    ) {
        let owner = if world.get::<MeshSource>(hit.owner).is_some() {
            Some(hit.owner)
        } else if world.get::<MeshSource>(hit.entity).is_some() {
            Some(hit.entity)
        } else {
            None
        };
        return (hit.position, owner);
    }
    if let Some((_, distance)) = crate::picking::pick_terrain(world, aim.origin, aim.direction, 1.0e5) {
        return (aim.origin + aim.direction * distance, None);
    }
    if aim.target_depth.is_finite() && aim.target_depth > 0.0 {
        return (aim.origin + aim.direction * aim.target_depth, None);
    }
    (Vec3::ZERO, None)
}

/// The ghost of an asset being dragged from the Content Browser over a viewport pane (P-067,
/// ADR-7769): where a release would place it, updated every frame from the same
/// [`ViewportDropAim`] math the drop uses ([`drop_placement`]). Nothing is spawned until the
/// release; the resource is removed on release or cancel. The viewport draws it as a wire box of
/// the asset's bounds at [`Self::transform`].
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct DropPreview {
    /// The dragged asset (project-relative).
    pub path: String,
    /// Its kind.
    pub kind: ContentKind,
    /// Where a release places it.
    pub transform: Transform,
    /// Model-space bounds of the asset (a unit cube when they cannot be read).
    pub bounds: (Vec3, Vec3),
    /// The egui frame that last updated it (a pane draws it only when it is current).
    pub frame: u64,
}

/// The transform a drop of a `kind` asset at the current [`ViewportDropAim`] places it with:
/// meshes, prefabs and sounds stand at `drop_position` (mesh, terrain, then target depth); a level or a material places nothing
/// (`None`).
#[must_use]
pub fn drop_placement(world: &World, kind: ContentKind, path: &str) -> Option<Transform> {
    let places = match kind {
        ContentKind::Mesh | ContentKind::Prefab => true,
        ContentKind::Other => is_sound_path(path),
        ContentKind::Level => false,
    };
    places.then(|| Transform::from_translation(drop_position(world).0))
}

/// Updates [`DropPreview`] for the content drag in flight (called by a viewport pane while the
/// pointer is over it, after it wrote [`ViewportDropAim`]). Bounds are read once per dragged
/// path. Returns whether a ghost is shown.
pub fn update_drop_preview(ctx: &egui::Context, world: &mut World) -> bool {
    let _span = tracing::info_span!(target: "regia_editor", "drop_preview").entered();
    let Some(drag) = egui::DragAndDrop::payload::<ContentDrag>(ctx) else {
        return false;
    };
    let Some(transform) = drop_placement(world, drag.kind, &drag.path) else {
        world.remove_resource::<DropPreview>();
        return false;
    };
    let frame = ctx.cumulative_frame_nr();
    let bounds = match world.get_resource::<DropPreview>() {
        Some(old) if old.path == drag.path => old.bounds,
        _ => drop_bounds(world, drag.kind, &drag.path),
    };
    world.insert_resource(DropPreview {
        path: drag.path.clone(),
        kind: drag.kind,
        transform,
        bounds,
        frame,
    });
    true
}

/// The ghost a drag of `path` would show with the current [`ViewportDropAim`] (the agent's
/// `editor.drop_preview`): its placement transform and bounds, `None` when a drop of it places
/// nothing (a level or a material). Spawns nothing.
#[must_use]
pub fn preview_for_path(world: &World, path: &str) -> Option<(Transform, (Vec3, Vec3))> {
    let kind = kind_of_path(path);
    let transform = drop_placement(world, kind, path)?;
    Some((transform, drop_bounds(world, kind, path)))
}

/// Drops `path` the way a release over the viewport does, at the current [`ViewportDropAim`]
/// (the agent's `editor.drop_preview { place: true }`). Needs an open project and level.
///
/// # Errors
/// `drop_no_project` without a project root, `drop_no_level` without an open level.
pub fn drop_at_aim(world: &mut World, path: &str) -> Result<String, &'static str> {
    let root = crate::mesh_source::project_root(world).ok_or("drop_no_project")?;
    let mut open = world
        .get_resource::<OpenLevel>()
        .cloned()
        .ok_or("drop_no_level")?;
    let toast = apply_content_drop(world, &mut open, &root, path, kind_of_path(path));
    world.insert_resource(open);
    Ok(toast)
}

/// The model-space bounds of a dragged mesh (its glTF, loaded once per drag), else a unit cube.
fn drop_bounds(world: &World, kind: ContentKind, path: &str) -> (Vec3, Vec3) {
    let unit = (Vec3::splat(-0.5), Vec3::splat(0.5));
    if kind != ContentKind::Mesh {
        return unit;
    }
    let root = crate::mesh_source::project_root(world);
    let full = crate::mesh_source::resolve_asset_path(root.as_deref(), path);
    let material = std::sync::Arc::new(regia_render::CpuMaterial::default());
    match crate::mesh_source::load_gltf_model(&full, &material) {
        Ok(model) if model.aabb_min.is_finite() && model.aabb_max.is_finite() => {
            (model.aabb_min, model.aabb_max)
        }
        _ => unit,
    }
}

fn file_stem_name(path: &str, fallback: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn push_command(world: &mut World, command: impl EditorCommand) {
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
}

fn place_mesh(world: &mut World, path: &str, full: &Path) -> String {
    let name = file_stem_name(path, "Mesh");
    let (position, _) = drop_position(world);
    let mesh = MeshSource::new(path, "");
    let collider = collider_for_import(full);
    let guid = Guid::new_random();
    let built = SpawnEntities::new_entity(world, guid, &name, Transform::from_translation(position))
        .and_then(|command| command.with_component(world, mesh.as_partial_reflect()));
    let built = match collider {
        Some(collider) => {
            built.and_then(|command| command.with_component(world, collider.as_partial_reflect()))
        }
        None => built,
    };
    match built {
        Ok(mut command) => {
            command.apply(world);
            push_command(world, command);
            crate::mesh_source::resolve_mesh_sources(world);
            let collider_note = if world_has_collider(world, guid) {
                " with a collider"
            } else {
                ""
            };
            format!("Spawned {name}{collider_note}")
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "mesh drop: {err}");
            format!("Spawn failed: {err}")
        }
    }
}

fn world_has_collider(world: &World, guid: Guid) -> bool {
    world.iter_entities().any(|entity| {
        entity.get::<EntityGuid>().is_some_and(|id| id.0 == guid)
            && entity.get::<regia::physics::Collider>().is_some()
    })
}

fn collider_for_import(full: &Path) -> Option<regia::physics::Collider> {
    let meta_path = regia_asset::meta_path_for(full);
    let bytes = std::fs::read(&meta_path).ok()?;
    let meta = regia_asset::MetaSidecar::from_ron(&bytes).ok()?;
    let settings = regia_asset_pipeline::GltfImporterSettings::from_meta_string(meta.settings.as_deref());
    if settings.collision == regia_asset_pipeline::CollisionImport::None {
        return None;
    }
    let material = std::sync::Arc::new(regia_render::LabMaterial::default());
    let model = crate::mesh_source::load_gltf_model(full, &material)
        .map_err(|err| {
            tracing::warn!(
                target: "regia_editor",
                "{}: collision import skipped, the model did not load: {err}",
                full.display()
            );
        })
        .ok()?;
    trimesh_collider(&model)
}

fn trimesh_collider(model: &crate::mesh_source::LoadedModel) -> Option<regia::physics::Collider> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for part in &model.parts {
        let base = u32::try_from(vertices.len()).ok()?;
        for vertex in &part.mesh.vertices {
            vertices.push(Vec3::new(
                vertex.position[0],
                vertex.position[1],
                vertex.position[2],
            ));
        }
        let (tris, _) = part.mesh.indices.as_chunks::<3>();
        for tri in tris {
            indices.push([base + tri[0], base + tri[1], base + tri[2]]);
        }
    }
    if vertices.is_empty() || indices.is_empty() {
        return None;
    }
    Some(regia::physics::Collider {
        shape: regia::physics::ColliderShape::TriMesh { vertices, indices },
        ..regia::physics::Collider::default()
    })
}

fn assign_dropped_material(world: &mut World, path: &str) -> String {
    let (_, owner) = drop_position(world);
    let Some(owner) = owner else {
        return format!("no mesh under the cursor for {path}");
    };
    let new: Box<dyn PartialReflect> = Box::new(path.to_string());
    match crate::command::SetProperty::capture(world, owner, "MeshSource", "material", new) {
        Ok(mut command) => {
            command.apply(world);
            push_command(world, command);
            crate::mesh_source::resolve_mesh_sources(world);
            format!("Assigned {path}")
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "material drop: {err}");
            format!("Assign failed: {err}")
        }
    }
}

fn place_sound(world: &mut World, path: &str) -> String {
    let name = file_stem_name(path, "Sound");
    let (position, _) = drop_position(world);
    let emitter = regia_audio::AudioEmitter {
        sound_name: path.to_string(),
        ..regia_audio::AudioEmitter::default()
    };
    let guid = Guid::new_random();
    match SpawnEntities::new_entity(world, guid, &name, Transform::from_translation(position))
        .and_then(|command| command.with_component(world, emitter.as_partial_reflect()))
    {
        Ok(mut command) => {
            command.apply(world);
            push_command(world, command);
            format!("Spawned {name}")
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "sound drop: {err}");
            format!("Spawn failed: {err}")
        }
    }
}

/// The stable asset id of a `.prefab.ron` file: the `.meta`'s id, created on first use (the
/// same lifecycle the cook uses for its sources). Instances reference this id, so saved levels
/// resolve against the asset server's prefab library instead of a per-session random guid
/// (`specs/12` § 3 seam, `specs/14` A3).
///
/// # Errors
///
/// The meta cannot be read, parsed or written.
pub fn prefab_asset_id(path: &Path) -> Result<regia_asset::AssetId, String> {
    let meta_path = regia_asset::meta_path_for(path);
    if meta_path.is_file() {
        let bytes =
            std::fs::read(&meta_path).map_err(|e| format!("cannot read {}: {e}", meta_path.display()))?;
        let meta = regia_asset::MetaSidecar::from_ron(&bytes)
            .map_err(|e| format!("cannot parse {}: {e}", meta_path.display()))?;
        return Ok(meta.id);
    }
    let meta = regia_asset::MetaSidecar::new("prefab_ron", regia_asset::AssetId::new_random());
    let text = meta
        .to_ron_string()
        .map_err(|e| format!("cannot serialize {}: {e}", meta_path.display()))?;
    std::fs::write(&meta_path, text).map_err(|e| format!("cannot write {}: {e}", meta_path.display()))?;
    Ok(meta.id)
}

/// Bakes and registers a `.prefab.ron` file, then spawns an instance at `translation`
/// (undoable through a despawn command whose revert re-serializes the subtree). Returns the
/// instance root.
///
/// # Errors
///
/// The prefab file cannot be read, parsed, baked or spawned.
pub fn spawn_prefab_asset(
    world: &mut World,
    path: &Path,
    translation: regia_core::math::Vec3,
) -> Result<Entity, String> {
    // A variant registers itself and its base chain (ADR-2323); a prefab is baked here.
    let asset = if crate::prefab_variant_edit::is_variant_path(path) {
        crate::prefab_variant_edit::register_prefab_file(world, path).map_err(|e| e.to_string())?
    } else {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let file = regia_scene::prelude::SceneFile::from_ron_str(&text).map_err(|e| format!("{e}"))?;
        let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
        // The template is registered under the file's stable asset id (its `.meta`), so the
        // instance reference survives saves and PIE copies.
        let asset_id = prefab_asset_id(path)?;
        let template = PrefabTemplate::from_scene_file(asset_id.0, &file, &registry.read())
            .map_err(|e| format!("prefab bake: {e}"))?;
        let asset = template.asset;
        world
            .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
            .insert(template);
        asset
    };
    // The active level while several are open (P-097, ADR-7792), else the open one.
    let level = crate::level_io::spawn_target_level(world);
    let opts = SpawnOptions {
        level,
        persistent: false,
        unknown_types: Default::default(),
    };
    // The editor is not lockstep: it draws the root guid from OS entropy (a lockstep game
    // passes a guid from its sim RNG instead, so both peers derive the same member guids).
    let root = world
        .spawn_prefab(
            asset,
            Some(Transform {
                translation,
                ..Transform::default()
            }),
            &opts,
            Guid::new_random(),
        )
        .map_err(|e| format!("prefab spawn: {e}"))?;
    // The wrapper carries the file's stem as its name, so the outliner and the agent surface
    // can address the instance (prefab-internal entities are nameless by design — the template
    // keeps identity in its local ids).
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "Prefab".to_string());
    let stem = stem
        .strip_suffix(".prefab")
        .or_else(|| stem.strip_suffix(".variant"))
        .unwrap_or(&stem)
        .to_string();
    if let Ok(mut entry) = world.get_entity_mut(root) {
        entry.insert(regia_scene::prelude::Name::new(stem));
    }
    // A capture the template holds starts with the template's id in every instance: give this
    // instance's its own, in the instance's overrides, before the undo entry snapshots it
    // (ADR-7716).
    crate::shapes::renumber_instance_captures(world, root);
    // Undo entry: the spawn already happened; SpawnedRoots snapshots it now and reverts by
    // despawning (redo re-spawns from the snapshot).
    let mut command = SpawnedRoots {
        entities: vec![root.to_bits()],
        snapshot: None,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok(root)
}

/// Bakes and registers a `.prefab.ron` file, then spawns an instance (undoable through a
/// despawn command whose revert re-serializes the instantiated subtree).
fn spawn_prefab_from_file(world: &mut World, path: &Path, display: &str, at: Vec3) -> Option<String> {
    match spawn_prefab_asset(world, path, at) {
        Ok(_) => Some(format!("Spawned prefab {display}")),
        Err(err) => {
            tracing::error!(target: "regia_editor", "prefab drop: {err}");
            Some(format!("Prefab spawn failed: {err}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_assets() -> PathBuf {
        // One directory per call: several tests use this fixture and run in parallel.
        let dir = regia_test_utils::unique_temp_dir("editor-content");
        std::fs::create_dir_all(dir.join("assets/env")).expect("dir");
        std::fs::write(dir.join("assets/rock.prefab.ron"), "()").expect("prefab");
        std::fs::write(dir.join("assets/env/terrain.level.ron"), "()").expect("level");
        std::fs::write(dir.join("assets/env/rock.gltf"), b"glb").expect("mesh");
        std::fs::write(dir.join("assets/env/rock.gltf.meta"), b"meta").expect("meta");
        dir
    }

    /// The undo entry of a content-browser move/rename reverts and re-applies the whole move:
    /// files land back at the old path with references restored, then move again (spec 12 § 9).
    #[test]
    fn move_asset_command_undo_and_redo_round_trip() {
        let dir = project_with_assets();
        std::fs::write(
            dir.join("assets/env/terrain.level.ron"),
            "(rock: \"rock.prefab.ron\")",
        )
        .expect("level text");

        let mut app = regia_app::App::new();
        let world = app.world_mut();
        regia_asset::project_ops::move_asset(&dir, "rock.prefab.ron", "props/rock.prefab.ron").expect("move");
        let mut command = MoveAsset::applied(
            dir.clone(),
            "rock.prefab.ron".to_string(),
            "props/rock.prefab.ron".to_string(),
        );
        assert!(!command.changes_level_state());

        command.revert(world);
        assert!(dir.join("assets/rock.prefab.ron").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("assets/env/terrain.level.ron")).expect("level"),
            "(rock: \"rock.prefab.ron\")"
        );

        command.apply(world);
        assert!(dir.join("assets/props/rock.prefab.ron").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("assets/env/terrain.level.ron")).expect("level"),
            "(rock: \"props/rock.prefab.ron\")"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A viewport drop of a `ContentKind::Other` asset that is not a material or a sound (a
    /// `.txt`) does nothing — no entity, no toast — as `ContentKind::Other` documents.
    #[test]
    fn viewport_drop_of_other_asset_does_nothing() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        // The fresh level itself spawns nothing here, but count after it exists so only the
        // drop can change the total.
        let mut open = level_io::new_level(world);
        let mut count = world.query::<Entity>();
        let before = count.iter(world).count();

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![egui::Event::PointerButton {
                pos: egui::pos2(4.0, 4.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        });
        egui::DragAndDrop::set_payload(
            &ctx,
            ContentDrag {
                path: "assets/notes.txt".to_string(),
                kind: ContentKind::Other,
            },
        );
        let state = ContentBrowserState {
            project_root: Some(std::path::PathBuf::from("/project")),
            ..Default::default()
        };
        let toast = handle_viewport_drop(&ctx, world, &state, &mut open);
        assert!(toast.is_none(), "{toast:?}");
        let after = count.iter(world).count();
        assert_eq!(after, before, "an Other drop must not spawn an entity");
    }

    /// A unique temp project dir per call (tests run on parallel threads).
    fn temp_project(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "regia-editor-{label}-{}-{n}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("project dir");
        dir
    }

    /// A minimal valid PNG (1×1 white), encoded with the pipeline's own image dependency.
    fn tiny_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
            encoder
                .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
                .expect("png encode");
        }
        bytes
    }

    #[test]
    fn import_refuses_folders_that_escape_assets() {
        let root = temp_project("import-escape");
        let source = root.join("source.png");
        std::fs::write(&source, tiny_png()).expect("png");
        for folder in ["../outside", "a/../../outside", "C:/elsewhere"] {
            let result = import_files(&root, folder, std::slice::from_ref(&source), None);
            assert!(result.is_err(), "{folder} must be refused");
        }
        assert!(!root.join("outside").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_copies_cooks_and_stamps_provenance() {
        let root = temp_project("import");
        let source = root.join("source.png");
        std::fs::write(&source, tiny_png()).expect("png");

        let report = import_files(&root, "", std::slice::from_ref(&source), Some("claude")).expect("import");
        assert_eq!(report.imported, ["assets/source.png"]);
        assert_eq!(report.errors, []);
        assert!(report.cooked >= 1, "the pipeline cooked: {report:?}");

        // The file is in the project; the content browser sees it.
        let state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        assert!(state.assets().iter().any(|(p, _)| p == "assets/source.png"));

        // The cooked dev cache exists for the host platform.
        let platform = regia_asset_pipeline::host_platform();
        let cache = regia_asset::cooked_cache_dir(&root, platform);
        assert!(cache.join("asset_manifest.bin").is_file(), "manifest written");

        // Provenance: agent tool, client, RFC 3339 timestamp.
        let meta_bytes = std::fs::read(root.join("assets/source.png.meta")).expect("meta");
        let meta = regia_asset::MetaSidecar::from_ron(&meta_bytes).expect("meta parse");
        let provenance = meta.provenance.expect("provenance");
        assert_eq!(provenance.tool, regia_asset::ProvenanceTool::Agent);
        assert_eq!(provenance.client.as_deref(), Some("claude"));
        let stamp = provenance.created_utc.expect("timestamp");
        assert_eq!(stamp.len(), 20, "RFC 3339: {stamp}");
        assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");

        // Importing the same file again is refused (no silent overwrite).
        let again = import_files(&root, "", std::slice::from_ref(&source), None);
        assert!(again.expect("import run").errors.len() == 1, "duplicate refused");

        // Unsupported extensions fail with a clear message: the editor refuses instead of
        // letting the cook crash.
        let notes = root.join("notes.txt");
        std::fs::write(&notes, b"text").expect("notes");
        let report = import_files(&root, "", &[notes], None).expect("import run");
        assert!(report.errors[0].1.contains("unsupported extension"), "{report:?}");
        assert!(report.errors[0].1.contains("txt"), "{report:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_asset_refuses_drive_and_device_names() {
        // Audit LOW (sprint-corr-editor-a L6): only empty and dot-leading segments were refused, so
        // `C:/x` (which `Path::join` turns into an absolute path on Windows) and device names
        // passed; on Linux `C:` became a folder under assets/.
        let root = temp_project("create-names");
        for bad in [
            "C:/x.material.ron",
            "assets/C:x.material.ron",
            "CON.material.ron",
            "mat/nul.material.ron",
            "mat /x.material.ron",
        ] {
            assert!(create_asset(&root, NewAssetKind::Material, bad).is_err(), "{bad}");
        }
        assert!(!root.join("assets/C:").exists());
        assert!(create_asset(&root, NewAssetKind::Material, "mat/ok.material.ron").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_assets_round_trip_into_valid_files() {
        let root = temp_project("create");

        // Material: parses back with the default standard parameters.
        let path =
            create_asset(&root, NewAssetKind::Material, "assets/mat/crate.material.ron").expect("material");
        assert_eq!(path, "assets/mat/crate.material.ron");
        let bytes = std::fs::read(root.join(&path)).expect("material file");
        let material = regia_asset::MaterialAssetRon::from_ron(&bytes).expect("material parses");
        assert_eq!(material.name.as_deref(), Some("crate"));
        assert_eq!(material.base.roughness, 1.0);

        // Prefab: a scene file with one named root entity.
        let path = create_asset(&root, NewAssetKind::Prefab, "assets/pillar.prefab.ron").expect("prefab");
        let text = std::fs::read_to_string(root.join(&path)).expect("prefab file");
        let file = regia_scene::prelude::SceneFile::from_ron_str(&text).expect("prefab parses");
        assert_eq!(file.entities.len(), 1);
        assert_eq!(file.entities[0].name.as_deref(), Some("pillar"));

        // Level: parses as an empty level and opens.
        let path = create_asset(&root, NewAssetKind::Level, "assets/maps/arena.level.ron").expect("level");
        let text = std::fs::read_to_string(root.join(&path)).expect("level file");
        let id = regia_scene::prelude::LevelId(Guid::new_random());
        let level = regia_scene::prelude::Level::from_ron_str(id, &text).expect("level parses");
        assert!(level.scene.entities.is_empty());

        // Folder.
        let path = create_asset(&root, NewAssetKind::Folder, "assets/sfx").expect("folder");
        assert_eq!(path, "assets/sfx");
        assert!(root.join("assets/sfx").is_dir());

        // Guards: a missing kind extension writes nothing. The editor does not append it.
        let missing = create_asset(&root, NewAssetKind::Material, "props/crate").expect_err("no extension");
        assert!(missing.contains(".material.ron"), "{missing}");
        assert!(!root.join("assets/props/crate").exists());
        assert!(!root.join("assets/props/crate.material.ron").exists());

        // Guards: wrong extension, escape attempts and duplicates refuse.
        assert!(create_asset(&root, NewAssetKind::Material, "assets/x.prefab.ron").is_err());
        assert!(
            create_asset(&root, NewAssetKind::Folder, "../outside").is_err(),
            "paths must stay under assets/"
        );
        assert!(
            create_asset(&root, NewAssetKind::Folder, "assets/sfx").is_err(),
            "duplicate"
        );
        assert!(
            create_asset(&root, NewAssetKind::Material, "assets/dupe.material.ron").is_ok(),
            "first create succeeds"
        );
        assert!(
            create_asset(&root, NewAssetKind::Material, "assets/dupe.material.ron").is_err(),
            "second create refuses"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// w7 item 1: New ▸ Sequence and New ▸ Shot list write files at the current save versions
    /// that load back through their runtime loaders, and the New menu lists both.
    #[test]
    fn new_sequences_and_shot_lists_load_through_their_runtime_loaders() {
        let root = temp_project("create-seq");
        let kinds: Vec<&str> = NewAssetKind::ALL.iter().map(|k| k.label()).collect();
        assert!(kinds.contains(&"Sequence"), "{kinds:?}");
        assert!(kinds.contains(&"Shot list"), "{kinds:?}");
        assert_eq!(NewAssetKind::parse("sequence"), Ok(NewAssetKind::Sequence));
        assert_eq!(NewAssetKind::parse("shot_list"), Ok(NewAssetKind::ShotList));
        let unknown = NewAssetKind::parse("film").expect_err("unknown");
        assert!(
            unknown.contains("sequence") && unknown.contains("shot_list"),
            "{unknown}"
        );

        let path =
            create_asset(&root, NewAssetKind::Sequence, "assets/cine/intro.seq.ron").expect("sequence");
        assert_eq!(path, "assets/cine/intro.seq.ron");
        let text = std::fs::read_to_string(root.join(&path)).expect("sequence file");
        let sequence = regia_framework::sequencer::Sequence::from_ron(&text).expect("sequence loads");
        assert_eq!(
            sequence.version,
            regia_framework::sequencer::SEQUENCE_FORMAT_VERSION
        );
        assert_eq!(sequence.name, "intro");
        assert_eq!(
            sequence.to_ron().expect("text"),
            text,
            "the file is what a save writes"
        );

        let path =
            create_asset(&root, NewAssetKind::ShotList, "assets/cine/intro.shots.ron").expect("shot list");
        let text = std::fs::read_to_string(root.join(&path)).expect("shot list file");
        let shots = regia_framework::shots::ShotSequence::from_ron(&text).expect("shot list loads");
        assert_eq!(
            shots.version,
            regia_framework::shots::SHOT_SEQUENCE_FORMAT_VERSION
        );
        assert_eq!(shots.name, "intro");
        assert_eq!(shots.shots.len(), 1, "a shot list needs one shot");
        assert_eq!(
            shots.to_ron().expect("text"),
            text,
            "the file is what a save writes"
        );

        // The kind extension is required, as for the other kinds.
        let err = create_asset(&root, NewAssetKind::ShotList, "assets/cine/x.seq.ron").expect_err("ext");
        assert!(err.contains(".shots.ron"), "{err}");
        let next = next_new_asset_path(&root, "cine", NewAssetKind::ShotList);
        assert_eq!(next, "cine/shot_list-1.shots.ron");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_refuses_referenced_assets() {
        let root = temp_project("delete");
        std::fs::create_dir_all(root.join("assets")).expect("assets dir");
        std::fs::write(
            root.join("assets/rock.prefab.ron"),
            r#"( format_version: 1, entities: [] )"#,
        )
        .expect("prefab");
        // A level that references the prefab.
        let level = r#"( format_version: 1, entities: [ ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "A", prefab_instance: Some(( prefab: "assets/rock.prefab.ron", overrides: [], added_components: [], removed_components: [], added_children: [], removed_children: [] )) ) ] )"#;
        std::fs::write(root.join("assets/map.level.ron"), level).expect("level");

        let referencers = find_referencers(&root, "assets/rock.prefab.ron");
        assert_eq!(referencers, ["assets/map.level.ron"], "{referencers:?}");

        match delete_asset(&root, "assets/rock.prefab.ron") {
            Err(refusers) => assert_eq!(refusers, ["assets/map.level.ron"]),
            other => panic!("delete must refuse: {other:?}"),
        }
        assert!(root.join("assets/rock.prefab.ron").is_file(), "still there");

        // The unreferenced level deletes.
        delete_asset(&root, "assets/map.level.ron").expect("delete level");
        assert!(!root.join("assets/map.level.ron").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_refuses_a_level_that_instances_the_prefab_by_asset_id() {
        let root = temp_project("delete-guid");
        std::fs::create_dir_all(root.join("assets")).expect("assets dir");
        let prefab = root.join("assets/rock.prefab.ron");
        std::fs::write(&prefab, r#"( format_version: 1, entities: [] )"#).expect("prefab");
        let id = prefab_asset_id(&prefab).expect("meta");
        // A saved instance names the prefab by `asset:<guid>`, never by path.
        let level = format!(
            r#"( format_version: 1, entities: [ ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "A", prefab_instance: Some(( prefab: "asset:{id}", overrides: [] )) ) ] )"#
        );
        std::fs::write(root.join("assets/map.level.ron"), level).expect("level");
        match delete_asset(&root, "assets/rock.prefab.ron") {
            Err(refusers) => assert_eq!(refusers, ["assets/map.level.ron"]),
            other => panic!("delete must refuse: {other:?}"),
        }
        assert!(prefab.is_file());

        // A level that only mentions a longer file name does not block the delete.
        std::fs::write(
            root.join("assets/map.level.ron"),
            r#"( format_version: 1, note: "assets/redrock.prefab.ron" )"#,
        )
        .expect("level");
        delete_asset(&root, "assets/rock.prefab.ron").expect("longer name is not a reference");
        assert!(!prefab.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_level_and_prefab_ron_that_does_not_parse() {
        let root = temp_project("import-bad-ron");
        let bad = root.join("garbage.level.ron");
        std::fs::write(&bad, "this is not ron (((").expect("bad");
        let good = root.join("fine.prefab.ron");
        std::fs::write(&good, r#"( format_version: 1, entities: [] )"#).expect("good");
        let report = import_files(&root, "", &[bad, good], None).expect("import");
        assert_eq!(report.errors.len(), 1, "{report:?}");
        assert!(report.errors[0].0.ends_with("garbage.level.ron"), "{report:?}");
        assert!(!root.join("assets/garbage.level.ron").exists());
        assert!(root.join("assets/fine.prefab.ron").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_rejects_a_variant_that_does_not_parse() {
        let root = temp_project("import-bad-variant");
        let bad = root.join("broken.variant.ron");
        std::fs::write(&bad, "(format_version: 1, base: 7)").expect("bad");
        let good = root.join("red.variant.ron");
        std::fs::write(
            &good,
            r#"(base: (prefab: "8b6f2a11-0000-4000-8000-000000000001"))"#,
        )
        .expect("good");
        let report = import_files(&root, "", &[bad, good], None).expect("import");
        assert_eq!(report.errors.len(), 1, "{report:?}");
        assert!(report.errors[0].0.ends_with("broken.variant.ron"), "{report:?}");
        assert!(!root.join("assets/broken.variant.ron").exists());
        assert!(root.join("assets/red.variant.ron").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_735_689_600), "2025-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z", "leap day");
        assert_eq!(rfc3339_from_unix(86_399), "1970-01-01T23:59:59Z");
    }

    #[test]
    fn assets_scan_finds_and_kinds() {
        let root = project_with_assets();
        let state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let assets = state.assets();
        let paths: Vec<&str> = assets.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "assets/env/rock.gltf",
                "assets/env/terrain.level.ron",
                "assets/rock.prefab.ron"
            ],
            "sorted, .meta skipped: {paths:?}"
        );
        let kind = |name: &str| {
            assets
                .iter()
                .find(|(p, _)| p.ends_with(name))
                .map(|(_, k)| *k)
                .expect("asset")
        };
        assert_eq!(kind("rock.gltf"), ContentKind::Mesh);
        assert_eq!(kind("terrain.level.ron"), ContentKind::Level);
        assert_eq!(kind("rock.prefab.ron"), ContentKind::Prefab);

        let folders = state.folders();
        assert_eq!(folders, ["assets/env"]);

        // Search narrows.
        let mut searching = state.clone();
        searching.search = "terrain".into();
        assert_eq!(searching.assets().len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_project_is_empty() {
        let state = ContentBrowserState::default();
        assert!(state.assets().is_empty());
        assert!(state.folders().is_empty());
    }

    fn write_triangle(dir: &Path, name: &str) -> String {
        let mut bin = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend(value.to_le_bytes());
        }
        for index in [0u32, 1, 2] {
            bin.extend(index.to_le_bytes());
        }
        let bin_name = format!("{name}.bin");
        std::fs::write(dir.join(&bin_name), &bin).expect("bin");
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
"nodes":[{{"mesh":0}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
"buffers":[{{"uri":"{bin_name}","byteLength":{len}}}],
"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],
"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}]}}"#,
            len = bin.len()
        );
        std::fs::write(dir.join(format!("{name}.gltf")), json).expect("gltf");
        format!("assets/{name}.gltf")
    }

    fn drop_app(root: &Path) -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        crate::mesh_source::register_level_types(app.world_mut());
        app
    }

    fn guid_count(world: &World) -> usize {
        world
            .iter_entities()
            .filter(|entity| entity.get::<EntityGuid>().is_some())
            .count()
    }

    #[test]
    fn viewport_drop_places_mesh_material_and_sound() {
        let root = regia_test_utils::unique_temp_dir("content_drop");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let rel = write_triangle(&root.join("assets"), "crate");
        std::fs::write(root.join("assets/paint.material.ron"), b"material").expect("mat");
        std::fs::write(root.join("assets/boom.wav"), b"wav").expect("wav");
        std::fs::write(root.join("assets/note.png"), b"png").expect("png");
        let mut app = drop_app(&root);
        let world = app.world_mut();
        let mut open = level_io::new_level(world);
        let before = guid_count(world);

        let toast = apply_content_drop(world, &mut open, &root, "assets/note.png", ContentKind::Other);
        assert_eq!(toast, "no viewport drop for assets/note.png");
        assert_eq!(guid_count(world), before);

        let toast = apply_content_drop(
            world,
            &mut open,
            &root,
            "assets/paint.material.ron",
            ContentKind::Other,
        );
        assert_eq!(toast, "no mesh under the cursor for assets/paint.material.ron");
        assert_eq!(guid_count(world), before);

        let toast = apply_content_drop(world, &mut open, &root, &rel, ContentKind::Mesh);
        assert!(toast.starts_with("Spawned crate"), "{toast}");
        assert!(!toast.contains("collider"), "{toast}");
        let owner = world
            .iter_entities()
            .find(|entity| entity.get::<MeshSource>().is_some_and(|mesh| mesh.mesh == rel))
            .map(|entity| entity.id())
            .expect("mesh owner");
        assert!(world.get::<regia_render::MeshRenderer>(owner).is_none());
        assert!(world.iter_entities().any(|entity| {
            entity
                .get::<crate::mesh_source::MeshPart>()
                .is_some_and(|part| part.owner == owner)
                && entity.get::<regia_render::MeshRenderer>().is_some()
        }));
        let placed = world.get::<Transform>(owner).expect("transform").translation;
        assert!(placed.length() < 1.0e-4, "{placed}");
        assert!(world.get::<regia::physics::Collider>(owner).is_none());
        world.insert_resource(ViewportDropAim {
            origin: Vec3::new(4.0, 0.0, 0.0),
            direction: Vec3::Y,
            target_depth: 2.5,
        });
        apply_content_drop(world, &mut open, &root, &rel, ContentKind::Mesh);
        let offset = world
            .iter_entities()
            .filter(|entity| entity.get::<MeshSource>().is_some_and(|mesh| mesh.mesh == rel))
            .find_map(|entity| {
                let translation = entity.get::<Transform>()?.translation;
                ((translation - Vec3::new(4.0, 2.5, 0.0)).length() < 1.0e-4).then_some(entity.id())
            });
        assert!(
            offset.is_some(),
            "a miss places the mesh at the orbit-target depth"
        );
        world.remove_resource::<ViewportDropAim>();
        // One editor frame between the drops (`render.rs` propagates, then parts copy their
        // owner's `GlobalTransform`). Without it the new mesh's part still sits at the origin and
        // ties with the first mesh under the next ray.
        regia_scene::prelude::propagate_transforms(world);
        crate::mesh_source::resolve_mesh_sources(world);

        world.insert_resource(ViewportDropAim {
            origin: Vec3::new(0.2, 0.2, 2.0),
            direction: Vec3::new(0.0, 0.0, -1.0),
            target_depth: 2.0,
        });
        let toast = apply_content_drop(
            world,
            &mut open,
            &root,
            "assets/paint.material.ron",
            ContentKind::Other,
        );
        assert_eq!(toast, "Assigned assets/paint.material.ron");
        assert_eq!(
            world.get::<MeshSource>(owner).map(|mesh| mesh.material.as_str()),
            Some("assets/paint.material.ron")
        );
        assert_eq!(guid_count(world), before + 2);

        let toast = apply_content_drop(world, &mut open, &root, "assets/boom.wav", ContentKind::Other);
        assert_eq!(toast, "Spawned boom");
        assert!(world.iter_entities().any(|entity| {
            entity
                .get::<regia_audio::AudioEmitter>()
                .is_some_and(|emitter| emitter.sound_name == "assets/boom.wav")
        }));

        let mut meta = regia_asset::MetaSidecar::new("gltf", regia_asset::AssetId::new_random());
        meta.settings = Some("(collision: ConvexDecomposition)".to_string());
        let gltf = root.join("assets/crate.gltf");
        std::fs::write(
            regia_asset::meta_path_for(&gltf),
            meta.to_ron_string().expect("meta ron"),
        )
        .expect("meta");
        world.remove_resource::<ViewportDropAim>();
        let toast = apply_content_drop(world, &mut open, &root, &rel, ContentKind::Mesh);
        assert!(toast.contains("collider"), "{toast}");
        let with_hull = world
            .iter_entities()
            .find(|entity| {
                entity.get::<MeshSource>().is_some_and(|mesh| mesh.mesh == rel)
                    && entity.get::<regia::physics::Collider>().is_some()
            })
            .map(|entity| entity.id())
            .expect("collider owner");
        assert!(matches!(
            world
                .get::<regia::physics::Collider>(with_hull)
                .expect("collider")
                .shape,
            regia::physics::ColliderShape::TriMesh { .. }
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `specs/19` § 3 drag and drop: a prefab dropped in the viewport spawns an instance and a level
    /// opens (replacing the open level), as `apply_content_drop` documents (ADR-5615 audit).
    #[test]
    fn viewport_drop_spawns_a_prefab_instance_and_opens_a_level() {
        let root = regia_test_utils::unique_temp_dir("content-drop-prefab-level");
        let prefab =
            create_asset(&root, NewAssetKind::Prefab, "assets/props/crate.prefab.ron").expect("prefab");
        let level =
            create_asset(&root, NewAssetKind::Level, "assets/levels/second.level.ron").expect("level");
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let mut open = level_io::new_level(world);
        let mut count = world.query::<Entity>();
        let before = count.iter(world).count();

        let toast = apply_content_drop(world, &mut open, &root, &prefab, ContentKind::Prefab);
        assert!(toast.starts_with("Spawned prefab"), "{toast}");
        assert!(
            count.iter(world).count() > before,
            "the prefab instance is spawned"
        );

        let toast = apply_content_drop(world, &mut open, &root, &level, ContentKind::Level);
        assert_eq!(toast, format!("Opened {level}"));
        assert!(
            open.path
                .as_deref()
                .is_some_and(|p| p.ends_with("levels/second.level.ron")),
            "{:?}",
            open.path
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `editor.select_by_asset` (spec 19 § 3): the entities whose reflected fields reference the
    /// asset path (a mesh's source, a material override, a sound's name) are exactly the ones
    /// selected; an unknown path selects nothing and answers ok.
    #[test]
    fn select_by_asset_picks_the_entities_that_reference_the_path() {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        crate::mesh_source::register_level_types(world);
        let a1 = world
            .spawn((Name::new("Rock A"), MeshSource::new("assets/props/a.glb", "")))
            .id();
        let a2 = world
            .spawn((Name::new("Rock B"), MeshSource::new("assets/props/a.glb", "")))
            .id();
        let b = world
            .spawn((Name::new("Rock C"), MeshSource::new("assets/props/b.glb", "")))
            .id();
        let materialized = world
            .spawn((
                Name::new("Painted"),
                MeshSource::new("assets/props/b.glb", "assets/paint/red.matgraph.ron"),
            ))
            .id();
        let siren = world
            .spawn((
                Name::new("Siren"),
                regia_audio::AudioEmitter {
                    sound_name: "assets/sfx/alarm.ogg".into(),
                    ..Default::default()
                },
            ))
            .id();

        let sorted = |found: Vec<regia_ecs::ecs::entity::Entity>| {
            let mut bits: Vec<u64> = found.iter().map(|e| e.to_bits()).collect();
            bits.sort_unstable();
            bits
        };
        let pair = sorted(vec![a1, a2]);
        assert_eq!(
            sorted(select_by_asset(world, "assets/props/a.glb")),
            pair,
            "exactly the two entities using a.glb"
        );
        // Without the assets/ prefix and case-normalised separators, the same two.
        assert_eq!(sorted(select_by_asset(world, "props/a.glb")), pair);
        // A material override references its own path; a sound's name does too.
        assert_eq!(
            sorted(select_by_asset(world, "assets/paint/red.matgraph.ron")),
            sorted(vec![materialized])
        );
        assert_eq!(
            sorted(select_by_asset(world, "assets/sfx/alarm.ogg")),
            sorted(vec![siren])
        );
        // A path nothing references selects nothing and is not an error.
        assert!(select_by_asset(world, "assets/props/absent.glb").is_empty());
        // Without the prefix, the same rule: both entities that use b.glb.
        assert_eq!(
            sorted(select_by_asset(world, "props/b.glb")),
            sorted(vec![b, materialized])
        );
    }

    /// New ▸ Material instance (spec 80 § 11.5): the created file's bytes are exactly
    /// `MaterialInstance::to_ron` for the parent, and the instance loads back.
    #[test]
    fn creating_a_material_instance_writes_the_instances_own_bytes_and_loads_back() {
        let root = temp_project("create-instance");
        let graph = regia_matgraph::MaterialGraph::new("rock");
        std::fs::create_dir_all(root.join("assets/materials")).expect("dir");
        std::fs::write(
            root.join("assets/materials/rock.matgraph.ron"),
            graph.to_ron().expect("graph ron"),
        )
        .expect("graph");

        // The kind parses, is listed in the New menu and maps to its extension.
        let kind = NewAssetKind::parse("material_instance").expect("kind");
        assert!(NewAssetKind::ALL.contains(&kind));
        assert_eq!(kind.extension(), Some("matinst.ron"));
        assert_eq!(kind.label(), "Material instance");
        // create_asset alone cannot make one: it needs the parent.
        let refused =
            create_asset(&root, kind, "assets/materials/rock_bare.matinst.ron").expect_err("needs a parent");
        assert!(refused.contains("parent"), "{refused}");
        assert!(!root.join("assets/materials/rock_bare.matinst.ron").exists());

        let made = create_material_instance(
            &root,
            "assets/materials/rock_Inst.matinst.ron",
            "assets/materials/rock.matgraph.ron",
        )
        .expect("create");
        assert_eq!(made, "assets/materials/rock_Inst.matinst.ron");
        let bytes = std::fs::read(root.join(&made)).expect("file");
        let expected = regia_matgraph::MaterialInstance::new("assets/materials/rock.matgraph.ron")
            .to_ron()
            .expect("instance ron");
        assert_eq!(bytes, expected.as_bytes(), "the same bytes as to_ron");
        let instance = regia_matgraph::MaterialInstance::from_ron(
            std::fs::read_to_string(root.join(&made)).expect("text").as_str(),
        )
        .expect("loads back");
        assert_eq!(instance.parent, "assets/materials/rock.matgraph.ron");
        assert!(instance.params.is_empty() && instance.textures.is_empty() && instance.switches.is_empty());

        // A missing parent is refused with its path in the message; a wrong extension refuses.
        let no_parent = create_material_instance(
            &root,
            "assets/materials/x.matinst.ron",
            "assets/materials/absent.matgraph.ron",
        )
        .expect_err("parent must exist");
        assert!(no_parent.contains("absent.matgraph.ron"), "{no_parent}");
        assert!(
            create_material_instance(
                &root,
                "assets/materials/x.material.ron",
                "assets/materials/rock.matgraph.ron"
            )
            .is_err()
        );
        assert!(
            create_material_instance(
                &root,
                "assets/materials/rock_Inst.matinst.ron",
                "assets/materials/rock.matgraph.ron"
            )
            .is_err(),
            "duplicate refuses"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// w10 (sprint-w9-rev-v06-lanes § 2 finding 3): `select_by_asset` on a `.prefab.ron` selects
    /// its instances, which name the prefab by the asset id in its `.meta` (not by path): exactly
    /// the instances of that id, with or without the `assets/` prefix; another prefab's instance
    /// and a project with no sidecar for the path select nothing.
    #[test]
    fn select_by_asset_picks_the_instances_of_a_prefab_by_its_meta_id() {
        use regia_scene::prefab::PrefabInstance;
        let root = temp_project("select-prefab");
        std::fs::create_dir_all(root.join("assets/prefabs")).expect("dir");
        for name in ["crate", "barrel", "unsaved"] {
            std::fs::write(
                root.join(format!("assets/prefabs/{name}.prefab.ron")),
                "(entities: [])",
            )
            .expect("prefab");
        }
        let crate_id = prefab_asset_id(&root.join("assets/prefabs/crate.prefab.ron"))
            .expect("crate id")
            .0
            .as_u128();
        let barrel_id = prefab_asset_id(&root.join("assets/prefabs/barrel.prefab.ron"))
            .expect("barrel id")
            .0
            .as_u128();
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let instance = |prefab: u128| PrefabInstance {
            prefab,
            ..PrefabInstance::default()
        };
        let c1 = world.spawn((Name::new("Crate 1"), instance(crate_id))).id();
        let c2 = world.spawn((Name::new("Crate 2"), instance(crate_id))).id();
        let barrel = world.spawn((Name::new("Barrel"), instance(barrel_id))).id();
        let sorted = |found: Vec<regia_ecs::ecs::entity::Entity>| {
            let mut bits: Vec<u64> = found.iter().map(|e| e.to_bits()).collect();
            bits.sort_unstable();
            bits
        };
        assert_eq!(
            sorted(select_by_asset(world, "assets/prefabs/crate.prefab.ron")),
            sorted(vec![c1, c2]),
            "exactly the crate's instances"
        );
        assert_eq!(
            sorted(select_by_asset(world, "prefabs/crate.prefab.ron")),
            sorted(vec![c1, c2])
        );
        assert_eq!(
            sorted(select_by_asset(world, "assets/prefabs/barrel.prefab.ron")),
            sorted(vec![barrel])
        );
        // No sidecar for this prefab: nothing names it.
        assert!(select_by_asset(world, "assets/prefabs/unsaved.prefab.ron").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// w10 (sprint-w9-rev-v06-lanes § 2 finding 4): `create_material_instance` confines `parent`
    /// to the project as the agent path does (`project_path_in`): an absolute parent, a `..`
    /// parent or a drive prefix is refused even when the file it names exists, and nothing is
    /// written; `select_by_asset` never probes a `.meta` outside the project (an escaping path
    /// matches nothing even where a sidecar with an instance's id sits outside).
    #[test]
    fn a_material_instance_parent_and_the_select_by_asset_probe_stay_inside_the_project() {
        use regia_scene::prefab::PrefabInstance;
        let outer = temp_project("instance-confine");
        let root = outer.join("game");
        std::fs::create_dir_all(root.join("assets/materials")).expect("dir");
        let graph = regia_matgraph::MaterialGraph::new("rock")
            .to_ron()
            .expect("graph ron");
        // A real graph outside the project, reachable by `..` and by an absolute path.
        std::fs::write(outer.join("stolen.matgraph.ron"), &graph).expect("outside graph");
        std::fs::create_dir_all(root.join("assets/x")).expect("dir");
        let absolute = outer
            .join("stolen.matgraph.ron")
            .to_string_lossy()
            .replace('\\', "/");
        for parent in [
            "../../stolen.matgraph.ron".to_string(),
            "assets/../../stolen.matgraph.ron".to_string(),
            "x/../../../stolen.matgraph.ron".to_string(),
            absolute,
            "C:/stolen.matgraph.ron".to_string(),
        ] {
            let err = create_material_instance(&root, "assets/materials/bad.matinst.ron", &parent)
                .expect_err("outside the project");
            assert!(err.contains("parent"), "{parent}: {err}");
            assert!(
                !root.join("assets/materials/bad.matinst.ron").exists(),
                "{parent}: nothing written"
            );
        }

        // A sidecar outside the project names a prefab id; a path that escapes must not read it.
        std::fs::write(outer.join("stolen.prefab.ron"), "(entities: [])").expect("prefab");
        let id = prefab_asset_id(&outer.join("stolen.prefab.ron"))
            .expect("id")
            .0
            .as_u128();
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.spawn((
            Name::new("Instance"),
            PrefabInstance {
                prefab: id,
                ..PrefabInstance::default()
            },
        ));
        for path in ["../../stolen.prefab.ron", "assets/../../stolen.prefab.ron"] {
            assert!(
                entities_using_asset(world, path).is_empty(),
                "{path}: no probe outside the project"
            );
        }
        let _ = std::fs::remove_dir_all(&outer);
    }

    /// FU-5: the grid's tiles are named, Tab reaches them, and arrows move the focus and the
    /// selection a tile (Left / Right) or a row (Up / Down) at a time, without a pointer.
    #[test]
    fn grid_arrows_move_the_focus_and_the_selection_between_tiles() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
        use egui::{Key, Modifiers};
        const SIZE: [f32; 2] = [330.0, 500.0];
        let visible: Vec<crate::content_index::IndexedAsset> = ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
            .iter()
            .map(|n| (format!("tex/{n}.png"), ContentKind::Other))
            .collect();
        let mut world = World::new();
        let mut state = ContentBrowserState::default();
        let mut open = crate::level_io::new_level(&mut World::new());
        let mut thumbs = crate::thumb_cache::ThumbCache::default();
        let mut badges = crate::content_provenance::ProvenanceBadges::default();
        let ctx = egui::Context::default();
        let mut columns = 0;
        let mut draw = |ui: &mut egui::Ui| {
            let spacing = ui.spacing().item_spacing.x;
            columns = ((ui.available_width() + spacing) / (64.0 + spacing)).floor() as usize;
            asset_grid(
                ui,
                &mut world,
                &mut state,
                &mut open,
                (visible.as_slice(), &[], None),
                (&mut thumbs, &mut badges),
                64.0,
            );
        };
        frame(&ctx, SIZE, vec![], &mut draw);
        tab_to(&ctx, SIZE, "a.png", false, 10, &mut draw);
        frame(&ctx, SIZE, vec![key(Key::ArrowRight, Modifiers::NONE)], &mut draw);
        let right = frame(&ctx, SIZE, vec![], &mut draw);
        assert_eq!(focused_name(&right).as_deref(), Some("b.png"));
        frame(&ctx, SIZE, vec![key(Key::ArrowDown, Modifiers::NONE)], &mut draw);
        let down = frame(&ctx, SIZE, vec![], &mut draw);
        assert!(columns > 1 && columns < 9, "{columns} columns");
        let below = format!(
            "{}.png",
            ["a", "b", "c", "d", "e", "f", "g", "h", "i"][1 + columns]
        );
        assert_eq!(focused_name(&down), Some(below.clone()), "Down moves one row");
        assert_eq!(
            state.selected,
            Some(format!("tex/{below}")),
            "the focused tile is selected"
        );
    }

    /// P-067 acceptance (ADR-7769): while an asset is dragged over the viewport a ghost stands at
    /// the drop point and the level gains no entity; the release spawns the mesh exactly where
    /// the ghost stood and removes the ghost; a cancelled drag removes it with no spawn.
    #[test]
    fn a_drag_shows_a_ghost_at_the_final_placement_and_spawns_only_on_release() {
        let root = regia_test_utils::unique_temp_dir("content_drop_preview");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let rel = write_triangle(&root.join("assets"), "ghost");
        let mut app = drop_app(&root);
        let world = app.world_mut();
        let mut open = level_io::new_level(world);
        let state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        world.insert_resource(ViewportDropAim {
            origin: Vec3::new(4.0, 0.0, 0.0),
            direction: Vec3::Y,
            target_depth: 2.5,
        });
        let drag = ContentDrag {
            path: rel.clone(),
            kind: ContentKind::Mesh,
        };
        let ctx = egui::Context::default();
        // Level entities (resources are entities too, and the ghost is a resource).
        let entities = |world: &mut World| {
            world
                .iter_entities()
                .filter(|e| !e.contains::<regia_ecs::ecs::resource::IsResource>())
                .count()
        };
        let before = entities(world);
        // Three frames of dragging: the ghost follows the aim, nothing spawns.
        let mut ghost = None;
        for step in 0..3 {
            ctx.begin_pass(egui::RawInput::default());
            egui::DragAndDrop::set_payload(&ctx, drag.clone());
            world.resource_mut::<ViewportDropAim>().target_depth = 2.5 + step as f32;
            assert!(update_drop_preview(&ctx, world), "a mesh drag shows a ghost");
            assert!(handle_viewport_drop(&ctx, world, &state, &mut open).is_none());
            assert_eq!(entities(world), before, "no entity while dragging (frame {step})");
            let preview = world.get_resource::<DropPreview>().expect("ghost").clone();
            assert_eq!(preview.path, rel);
            assert!(
                (preview.transform.translation - Vec3::new(4.0, 2.5 + step as f32, 0.0)).length() < 1.0e-4,
                "the ghost follows the drop aim: {:?}",
                preview.transform.translation
            );
            assert!(
                preview.bounds.1.x >= 0.99,
                "the triangle's bounds: {:?}",
                preview.bounds
            );
            ghost = Some(preview.transform);
            ctx.end_pass().textures_delta.clear();
        }
        // The release places it where the ghost stood.
        ctx.begin_pass(egui::RawInput {
            events: vec![egui::Event::PointerButton {
                pos: egui::pos2(4.0, 4.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        });
        egui::DragAndDrop::set_payload(&ctx, drag.clone());
        let toast = handle_viewport_drop(&ctx, world, &state, &mut open).expect("dropped");
        assert!(toast.starts_with("Spawned ghost"), "{toast}");
        ctx.end_pass().textures_delta.clear();
        assert!(
            !world.contains_resource::<DropPreview>(),
            "the release removes the ghost"
        );
        let placed = world
            .iter_entities()
            .find(|e| e.get::<MeshSource>().is_some_and(|m| m.mesh == rel))
            .and_then(|e| e.get::<Transform>().copied())
            .expect("spawned");
        assert_eq!(
            Some(placed),
            ghost,
            "the ghost's transform is the final placement"
        );
        // A cancelled drag (no payload any more) removes a ghost without spawning.
        let after_drop = entities(world);
        world.insert_resource(DropPreview {
            path: rel.clone(),
            kind: ContentKind::Mesh,
            transform: Transform::IDENTITY,
            bounds: (Vec3::ZERO, Vec3::ONE),
            frame: 0,
        });
        ctx.begin_pass(egui::RawInput::default());
        assert!(handle_viewport_drop(&ctx, world, &state, &mut open).is_none());
        ctx.end_pass().textures_delta.clear();
        assert!(
            !world.contains_resource::<DropPreview>(),
            "cancel removes the ghost"
        );
        assert_eq!(entities(world), after_drop);
        // A material drag shows no ghost (it assigns, it does not place).
        ctx.begin_pass(egui::RawInput::default());
        egui::DragAndDrop::set_payload(
            &ctx,
            ContentDrag {
                path: "assets/paint.material.ron".into(),
                kind: ContentKind::Other,
            },
        );
        assert!(!update_drop_preview(&ctx, world));
        ctx.end_pass().textures_delta.clear();
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod unsaved_tests;

#[cfg(test)]
mod selection_bar_tests;

#[cfg(test)]
mod search_completion_tests {
    use super::*;

    #[test]
    fn tab_completes_tokens_in_the_browser_search_box() {
        use crate::canvas_keys::test_input::{key, raw};
        let ctx = egui::Context::default();
        let mut text = "t".to_owned();
        let mut id = egui::Id::NULL;
        let mut frame = |text: &mut String, events, focus| {
            ctx.run_ui(raw([1200.0, 600.0], events), |ui| {
                if ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: repeat the search input pass");
                }
                let _ = ui.button("Before search");
                let label = ui.label("Search");
                let response = browser_search_box(ui, text, label.id);
                id = response.id;
                if focus {
                    response.request_focus();
                }
            })
            .textures_delta
            .clear();
        };
        frame(&mut text, vec![], true);
        frame(&mut text, vec![], false);
        frame(&mut text, vec![key(egui::Key::Tab, egui::Modifiers::NONE)], false);
        assert_eq!(text, "t:");
        frame(&mut text, vec![key(egui::Key::Tab, egui::Modifiers::NONE)], false);
        assert_eq!(text, "type:");
        frame(&mut text, vec![key(egui::Key::Tab, egui::Modifiers::NONE)], false);
        assert_eq!(text, "t:");
        text = "type:te".into();
        frame(&mut text, vec![], false);
        frame(&mut text, vec![key(egui::Key::Tab, egui::Modifiers::NONE)], false);
        assert_eq!(text, "type:texture");
        frame(
            &mut text,
            vec![key(egui::Key::Tab, egui::Modifiers::SHIFT)],
            false,
        );
        frame(&mut text, vec![], false);
        assert_eq!(text, "type:texture", "Shift+Tab does not complete");

        assert_eq!(
            crate::asset_search::active_completions(&ctx, id, &text),
            ["type:texture"]
        );
    }
}
