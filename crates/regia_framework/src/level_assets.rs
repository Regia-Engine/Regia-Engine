// SPDX-License-Identifier: MPL-2.0
//! Levels and layer definitions from their sources, in either level form (spec 37 § 3.1, § 4.1).
//!
//! A level is read at run time, not cooked to a blob, so the seam spec 37 calls the asset server's
//! lives here: [`load_level_source`] reads `forest.level.ron` or `forest.level.d/` (one asset type,
//! [`Level`]), fills the [`LevelLibrary`], and loads what the level needs: the `.layers.ron`
//! definitions it binds into the [`LayerDefsLibrary`] and the levels it places, transitively.
//!
//! Ids come from the `.meta` sidecars (`forest.level.d.meta` next to the directory), through a
//! [`LevelSourceIndex`] of the project's `assets/`. A level without a sidecar gets an id derived
//! from its path, so reopening it is a reload under the same id. A packaged game has no
//! sidecars: `regia package` folds the bound definitions into the level
//! ([`LevelLayers::embedded`]).
//!
//! [`LevelAssetsPlugin`] re-reads what changed on disk in a dev run: an edited entity file or
//! header reloads its level, an edited `.layers.ron` its definitions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{MetaSidecar, SourceAssetChanged, SourceKind};
use regia_core::id::Guid;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_scene::format::SceneFile;
use regia_scene::layers::{
    LEVEL_LAYERS_PATH, LayerDefs, LayerDefsLibrary, LayerKind, LayerProblem, LayerTable, LevelLayers,
    strip_component_holders, strip_editor_only, validate_defs, validate_level,
};
use regia_scene::level::{Level, LevelId, LevelLibrary, LevelManager, LevelState, LoadMode, open_level};
use regia_scene::level_dir::{
    LEVEL_DIR_SUFFIX, LEVEL_FILE_SUFFIX, LevelDirError, containing_level_dir, level_paths,
    read_level_as_stored, read_level_source,
};
use regia_scene::placed::PlacedLevel;

/// Type path of the functional test component in scene records (the type itself exists only with
/// the `functional-tests` feature; a packaging tool needs the name either way).
pub const FUNCTIONAL_TEST_PATH: &str = "regia_framework::functional::FunctionalTest";

/// File name suffix of a layer definition asset.
pub const LAYERS_SUFFIX: &str = ".layers.ron";
/// File name suffix of a terrain layer stack, which is another format.
const TERRAIN_LAYERS_SUFFIX: &str = "terrain.layers.ron";

/// Why a level or its definitions did not load. The `reason` strings are spec 37 § 9's.
#[derive(Debug, thiserror::Error)]
pub enum LevelAssetError {
    /// The level source (either form) is unusable.
    #[error(transparent)]
    Source(#[from] LevelDirError),
    /// A layer definition file does not parse or does not validate.
    #[error("layer_parent_invalid: {path}: {detail}")]
    Definitions {
        /// The `.layers.ron` file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
    /// A level variant does not resolve (ADR-7823): a missing base, a loop, a bad file.
    #[error("{}: {path}: {error}", .error.reason())]
    Variant {
        /// The `.levelvariant.ron` file.
        path: PathBuf,
        /// Why it does not resolve (boxed: the RON parse error is large).
        error: Box<regia_scene::level_variant::LevelVariantError>,
    },
}

impl LevelAssetError {
    /// The agent `data.reason` string.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Source(err) => err.reason(),
            Self::Definitions { .. } => "layer_parent_invalid",
            Self::Variant { error, .. } => error.reason(),
        }
    }
}

/// Whether `name` is a layer definition asset (and not a terrain layer stack).
#[must_use]
pub fn is_layer_defs_name(name: &str) -> bool {
    name.ends_with(LAYERS_SUFFIX) && !name.ends_with(TERRAIN_LAYERS_SUFFIX)
}

/// Where the project's level and layer definition sources are, by asset id.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelSourceIndex {
    /// Level sources (the path as `.level.ron` or `.level.d`).
    pub levels: BTreeMap<Guid, PathBuf>,
    /// `.layers.ron` files.
    pub layer_defs: BTreeMap<Guid, PathBuf>,
}

impl LevelSourceIndex {
    /// Walks `assets_root` for the `.meta` sidecars of levels and layer definitions. Unreadable
    /// directories and sidecars are skipped with a log line: a missing entry is reported when
    /// something needs it. The visited paths are sorted before insertion, so the index is the same
    /// whatever order `read_dir` returns; a guid two sidecars share keeps the lexicographically
    /// first path (a warning names the duplicate).
    #[must_use]
    pub fn scan(assets_root: &Path) -> Self {
        let mut index = Self::default();
        let mut found: Vec<(PathBuf, Guid, bool)> = Vec::new();
        let mut stack = vec![assets_root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::debug!(target: "regia_framework", dir = %dir.display(), "skipped: {err}");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    // The inside of a level directory holds no assets of its own.
                    if !name.ends_with(LEVEL_DIR_SUFFIX) {
                        stack.push(path);
                    }
                    continue;
                }
                let Some(source) = name.strip_suffix(".meta") else {
                    continue;
                };
                let is_level = source.ends_with(LEVEL_FILE_SUFFIX)
                    || source.ends_with(LEVEL_DIR_SUFFIX)
                    || source.ends_with(regia_scene::level_variant::LEVEL_VARIANT_SUFFIX);
                if !is_level && !is_layer_defs_name(source) {
                    continue;
                }
                let Some(id) = read_meta_id(&path) else { continue };
                found.push((path.with_file_name(source), id, is_level));
            }
        }
        found.sort();
        for (source, id, is_level) in found {
            let table = if is_level {
                &mut index.levels
            } else {
                &mut index.layer_defs
            };
            match table.entry(id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(source);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    tracing::warn!(
                        target: "regia_framework",
                        guid = %id,
                        kept = %entry.get().display(),
                        dropped = %source.display(),
                        "duplicate asset id: keeping the lexicographically first path"
                    );
                }
            }
        }
        index
    }
}

/// The asset id in a `.meta` sidecar.
fn read_meta_id(meta: &Path) -> Option<Guid> {
    let bytes = std::fs::read(meta).ok()?;
    match MetaSidecar::from_ron(&bytes) {
        Ok(sidecar) => Some(sidecar.id.0),
        Err(err) => {
            tracing::warn!(target: "regia_framework", path = %meta.display(), "bad sidecar: {err}");
            None
        }
    }
}

/// The `assets/` directory `path` lives under, else its parent.
#[must_use]
pub fn assets_root_of(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "assets"))
        .or_else(|| path.parent())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// The string the path-derived id hashes: the level's path below the project's `assets/` root
/// with `/` separators, whatever the platform's separator and whatever spelling the path arrived
/// with (relative, absolute, `.`/`..` segments, the other form). Two spellings of one level name
/// one id on every machine; a path with no `assets/` component hashes its whole string with `\`
/// folded to `/`.
fn level_id_path_key(file: &Path) -> String {
    let text = file.to_string_lossy();
    // Lexical normalization only (no filesystem): drop `.` segments and resolve each `..`
    // against the previous component, so `assets/./town.level.ron` and
    // `assets/levels/../town.level.ron` hash the same key as `assets/town.level.ron`.
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let start = parts.iter().rposition(|part| *part == "assets").unwrap_or(0);
    parts[start..].join("/")
}

/// The id of the level at `path`: its sidecar's, else one derived from the path, so the same level
/// has the same id in every run and in either form.
#[must_use]
pub fn level_id_of(path: &Path) -> LevelId {
    // A level variant is one file with its own sidecar (ADR-7823).
    if regia_scene::level_variant::is_level_variant_path(path)
        && let Some(id) = read_meta_id(&regia_asset::meta_path_for(path))
    {
        return LevelId(id);
    }
    let (file, dir) = level_paths(path);
    for source in [&dir, &file] {
        if let Some(id) = read_meta_id(&regia_asset::meta_path_for(source)) {
            return LevelId(id);
        }
    }
    let hash = blake3::hash(level_id_path_key(&file).as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    LevelId(Guid::from_u128(u128::from_le_bytes(bytes)))
}

/// What [`load_level_source`] brought in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelLoadReport {
    /// The id of the level asked for.
    pub level: Option<LevelId>,
    /// Every level loaded (the one asked for, then the ones it places).
    pub levels: Vec<LevelId>,
    /// The layer definition assets loaded.
    pub layer_defs: Vec<Guid>,
    /// What could not be found: the level still loads, and the layer table and the placement
    /// report the same names when they use them.
    pub missing: Vec<String>,
}

/// Reads one `.layers.ron` file and validates it (fatal findings refuse it).
pub fn read_layer_defs(path: &Path) -> Result<LayerDefs, LevelAssetError> {
    let definitions = |detail: String| LevelAssetError::Definitions {
        path: path.to_path_buf(),
        detail,
    };
    let text = std::fs::read_to_string(path).map_err(|e| definitions(e.to_string()))?;
    let defs = LayerDefs::from_ron_str(&text).map_err(|e| definitions(e.to_string()))?;
    let fatal: Vec<String> = validate_defs(&defs)
        .into_iter()
        .filter(|problem| problem.fatal)
        .map(|problem| format!("{}: {}", problem.reason, problem.detail))
        .collect();
    if fatal.is_empty() {
        Ok(defs)
    } else {
        Err(definitions(fatal.join("; ")))
    }
}

/// Checks a level's layers (spec 37 § 4.5): its bound definitions and its memberships. What does
/// not load is a finding, so one run lists everything.
pub fn validate_level_source(path: &Path) -> Result<Vec<LayerProblem>, LevelAssetError> {
    let scene = read_level_source(path)?;
    let binding = LevelLayers::from_scene(&scene).and_then(Result::ok);
    let mut problems = Vec::new();
    let mut bound: Vec<LayerDefs> = binding.iter().flat_map(|b| b.embedded.iter().cloned()).collect();
    let ids: Vec<Guid> = binding.iter().flat_map(LevelLayers::def_ids).collect();
    if !ids.is_empty() {
        let index = LevelSourceIndex::scan(&assets_root_of(path));
        for id in ids {
            let Some(file) = index.layer_defs.get(&id) else {
                problems.push(LayerProblem {
                    reason: "layer_unknown",
                    detail: format!("no `.layers.ron` has id {id}"),
                    fatal: false,
                });
                continue;
            };
            match read_layer_defs(file) {
                Ok(defs) => {
                    problems.extend(validate_defs(&defs));
                    bound.push(defs);
                }
                Err(err) => problems.push(LayerProblem {
                    reason: err.reason(),
                    detail: err.to_string(),
                    fatal: true,
                }),
            }
        }
    }
    let references: Vec<&LayerDefs> = bound.iter().collect();
    problems.extend(validate_level(&scene, &references));
    Ok(problems)
}

/// The level sources one load touched, so a change on disk finds them.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedLevelSources {
    /// Level path to id.
    pub levels: BTreeMap<PathBuf, LevelId>,
    /// `.layers.ron` path to id.
    pub layer_defs: BTreeMap<PathBuf, Guid>,
}

/// Loads the level at `path` (`.level.ron` or `.level.d`, either names the level) into
/// the [`LevelLibrary`] with the definitions it binds and the levels it places. Bindings and
/// placements already in their libraries are not read again. It opens nothing; call
/// [`open_level`] with the id.
pub fn load_level_source(world: &mut World, path: &Path) -> Result<LevelLoadReport, LevelAssetError> {
    load_level_source_inner(world, path, None)
}

/// [`load_level_source`] for a level whose own source was already read and parsed elsewhere (the
/// level switch reads it on a worker, ADR-8027): `level` goes into the [`LevelLibrary`] under its
/// id, then the definitions it binds and the levels it places load as usual. `path` is the source
/// it came from (for the change watch and the project's `assets/` root).
pub fn load_level_source_parsed(
    world: &mut World,
    path: &Path,
    level: Level,
) -> Result<LevelLoadReport, LevelAssetError> {
    load_level_source_inner(world, path, Some(level))
}

fn load_level_source_inner(
    world: &mut World,
    path: &Path,
    mut parsed: Option<Level>,
) -> Result<LevelLoadReport, LevelAssetError> {
    let _span = tracing::info_span!("level_source_load", path = %path.display()).entered();
    let mut report = LevelLoadReport::default();
    let id = parsed
        .as_ref()
        .map_or_else(|| level_id_of(path), |level| level.id);
    let mut index: Option<LevelSourceIndex> = None;
    let root = assets_root_of(path);
    let mut pending: Vec<(LevelId, PathBuf)> = vec![(id, path.to_path_buf())];
    let mut seen: BTreeSet<LevelId> = BTreeSet::new();
    report.level = Some(id);
    while let Some((level_id, source)) = pending.pop() {
        if !seen.insert(level_id) {
            continue;
        }
        let level = match parsed.take() {
            // The first entry is the level asked for.
            Some(level) => level,
            None => {
                let scene = if regia_scene::level_variant::is_level_variant_path(&source) {
                    // A variant resolves through its chain into a level (ADR-7823); its bases are found by
                    // id in the project index.
                    let index = index.get_or_insert_with(|| LevelSourceIndex::scan(&root));
                    let find = |id: Guid| index.levels.get(&id).cloned();
                    regia_scene::level_variant::resolve_variant_file(level_id.0, &source, &find)
                        .map_err(|error| LevelAssetError::Variant {
                            path: source.clone(),
                            error: Box::new(error),
                        })?
                        .scene
                } else {
                    read_level_as_stored(&source)?
                };
                Level::from_scene_file(level_id, scene)
            }
        };
        // Bound definitions: only a level that binds some pays for the scan.
        let bound = LevelLayers::from_scene(&level.scene).and_then(Result::ok);
        for def_id in bound.iter().flat_map(LevelLayers::def_ids) {
            if world
                .get_resource::<LayerDefsLibrary>()
                .is_some_and(|library| library.get(def_id).is_some())
            {
                continue;
            }
            let index = index.get_or_insert_with(|| LevelSourceIndex::scan(&root));
            let Some(file) = index.layer_defs.get(&def_id).cloned() else {
                report.missing.push(format!("layer definition {def_id}"));
                continue;
            };
            let defs = read_layer_defs(&file)?;
            world
                .get_resource_or_insert_with::<LayerDefsLibrary>(LayerDefsLibrary::default)
                .insert(def_id, defs);
            world
                .get_resource_or_insert_with::<LoadedLevelSources>(LoadedLevelSources::default)
                .layer_defs
                .insert(file, def_id);
            report.layer_defs.push(def_id);
        }
        // Placed levels, transitively; `check_placement` refuses cycles when one is filled.
        let placed: Vec<LevelId> = level
            .scene
            .entities
            .iter()
            .filter_map(PlacedLevel::from_record)
            .filter_map(|placement| placement.level_id())
            .collect();
        for reference in placed {
            if seen.contains(&reference)
                || world
                    .get_resource::<LevelLibrary>()
                    .is_some_and(|library| library.contains(reference))
            {
                continue;
            }
            let index = index.get_or_insert_with(|| LevelSourceIndex::scan(&root));
            match index.levels.get(&reference.0) {
                Some(file) => pending.push((reference, file.clone())),
                None => report.missing.push(format!("level {}", reference.0)),
            }
        }
        world
            .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
            .insert(level);
        world
            .get_resource_or_insert_with::<LoadedLevelSources>(LoadedLevelSources::default)
            .levels
            .insert(source, level_id);
        report.levels.push(level_id);
    }
    for missing in &report.missing {
        tracing::warn!(target: "regia_framework", "level {}: {missing} not found", path.display());
    }
    Ok(report)
}

/// A level ready for a package: what shipped levels contain (spec 37 § 4.1, spec 41 § 9).
#[derive(Clone, Debug, PartialEq)]
pub struct PackagedLevel {
    /// The level to write.
    pub scene: SceneFile,
    /// Functional test entities (with their descendants) taken out.
    pub tests_stripped: usize,
    /// Members of editor-only layers (with their descendants) taken out.
    pub editor_only_stripped: usize,
}

/// Prepares a level for a package. Members of editor-only layers are dropped and those layers
/// leave the binding; the definitions the level binds move into it ([`LevelLayers::embedded`]),
/// because a package has no `.meta` ids to find them by; with `strip_tests` (shipping profiles) the
/// functional test entities are dropped. `defs` finds a bound definition by id; an id it does not
/// know stays bound and reports `layer definition <id> is not loaded` at run time.
#[must_use]
pub fn prepare_level_for_package(
    scene: &SceneFile,
    defs: &dyn Fn(Guid) -> Option<LayerDefs>,
    strip_tests: bool,
) -> PackagedLevel {
    let total = scene.entities.len();
    let binding = LevelLayers::from_scene(scene).and_then(Result::ok);
    let mut bound = Vec::new();
    let mut unresolved = Vec::new();
    for id in binding.iter().flat_map(LevelLayers::def_ids) {
        match defs(id) {
            Some(asset) => bound.push(asset),
            None => unresolved.push(format!("asset:{id}")),
        }
    }
    bound.extend(binding.iter().flat_map(|b| b.embedded.iter().cloned()));
    let references: Vec<&LayerDefs> = bound.iter().collect();
    let mut out = strip_editor_only(scene, &references);
    let editor_only_stripped = total - out.entities.len();
    let mut tests_stripped = 0;
    if strip_tests {
        (out, tests_stripped) = strip_component_holders(&out, FUNCTIONAL_TEST_PATH);
    }
    if let Some(binding) = binding {
        let runtime = |name: &str| {
            bound
                .iter()
                .find_map(|asset| asset.get(name))
                .is_none_or(|def| def.kind != LayerKind::EditorOnly)
        };
        let packaged = LevelLayers {
            defs: unresolved,
            entries: binding
                .entries
                .into_iter()
                .filter(|e| runtime(&e.layer))
                .collect(),
            embedded: bound
                .iter()
                .map(|asset| LayerDefs {
                    format_version: asset.format_version,
                    layers: asset
                        .layers
                        .iter()
                        .filter(|def| def.kind != LayerKind::EditorOnly)
                        .cloned()
                        .collect(),
                })
                .collect(),
        };
        if let Ok(text) = ron::to_string(&packaged)
            && let Ok(raw) = ron::value::RawValue::from_boxed_ron(text.into_boxed_str())
        {
            out.resources.insert(LEVEL_LAYERS_PATH.to_owned(), raw);
        }
    }
    PackagedLevel {
        scene: out,
        tests_stripped,
        editor_only_stripped,
    }
}

/// Reloads the level sources and layer definitions that changed on disk.
pub struct LevelAssetsPlugin;

impl Plugin for LevelAssetsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<SourceAssetChanged>()
            .init_resource::<LoadedLevelSources>()
            .add_systems(Update, reload_changed_level_sources);
    }
}

/// `Update`: re-reads changed `.layers.ron` files, and reopens changed levels that are active (a
/// reload under the same id, so their layers start over).
pub fn reload_changed_level_sources(world: &mut World, mut cursor: Local<MessageCursor<SourceAssetChanged>>) {
    let changes: Vec<(SourceKind, PathBuf)> = {
        let Some(messages) = world.get_resource::<Messages<SourceAssetChanged>>() else {
            return;
        };
        cursor
            .read(messages)
            .map(|change| (change.kind, change.path.clone()))
            .collect()
    };
    if changes.is_empty() {
        return;
    }
    let loaded = world.resource::<LoadedLevelSources>().clone();
    // One save can touch many files of a level directory: each source reloads once a frame.
    let mut defs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut levels: BTreeSet<PathBuf> = BTreeSet::new();
    for (kind, path) in changes {
        match kind {
            SourceKind::LayerDefs => {
                let key = source_key(&path);
                defs.extend(loaded.layer_defs.keys().find(|p| source_key(p) == key).cloned());
            }
            SourceKind::Level => {
                let root = containing_level_dir(&path).unwrap_or(path);
                let key = level_key(&root);
                levels.extend(loaded.levels.keys().find(|p| level_key(p) == key).cloned());
            }
            _ => {}
        }
    }
    for path in defs {
        reload_layer_defs(world, &loaded, &path);
    }
    for path in levels {
        reload_level(world, &loaded, &path);
    }
}

/// `path` with its folder resolved, so a source loaded by a relative path matches the absolute
/// path of a change event. The map keys stay as loaded: a level's id can derive from that spelling.
fn source_key(path: &Path) -> PathBuf {
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let folder = std::fs::canonicalize(parent)
        .or_else(|_| std::path::absolute(parent))
        .unwrap_or_else(|_| parent.to_path_buf());
    folder.join(name)
}

/// [`source_key`] of a level, the same for its file and directory spellings.
pub(crate) fn level_key(path: &Path) -> PathBuf {
    source_key(&level_paths(path).0)
}

fn reload_layer_defs(world: &mut World, loaded: &LoadedLevelSources, path: &Path) {
    let Some(id) = loaded.layer_defs.get(path).copied() else {
        return;
    };
    match read_layer_defs(path) {
        Ok(defs) => {
            world
                .get_resource_or_insert_with::<LayerDefsLibrary>(LayerDefsLibrary::default)
                .insert(id, defs);
            if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
                table.invalidate();
            }
        }
        Err(err) => tracing::warn!(target: "regia_framework", "layer definitions kept: {err}"),
    }
}

fn reload_level(world: &mut World, loaded: &LoadedLevelSources, path: &Path) {
    let Some(id) = loaded.levels.get(path).copied() else {
        return;
    };
    // `regia run --level` watches its level itself.
    if world
        .get_resource::<crate::start_level::StartLevel>()
        .is_some_and(|start| level_key(&start.path) == level_key(path))
    {
        return;
    }
    if let Err(err) = load_level_source(world, path) {
        tracing::warn!(target: "regia_framework", path = %path.display(), "level kept: {err}");
        return;
    }
    let Some(manager) = world.get_resource::<LevelManager>() else {
        return;
    };
    if manager.state(id) != Some(LevelState::Active) {
        return;
    }
    let mode = if manager.current() == Some(id) {
        LoadMode::Replace
    } else {
        LoadMode::Additive
    };
    if let Err(err) = open_level(world, id, mode) {
        tracing::warn!(target: "regia_framework", "level {id} not reopened: {err}");
        return;
    }
    regia_scene::placed::reattach_streamed(world, id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::ScenePlugin;
    use regia_scene::format::SCENE_FORMAT_VERSION;
    use regia_scene::layers::{LayerMembership, LayerState};
    use regia_scene::level_dir::{split_level_file, write_level_dir};
    use regia_test_utils::unique_temp_dir;

    const DEFS_ID: Guid = Guid::from_u128(0xd0);
    const SUB_ID: Guid = Guid::from_u128(0xe0);

    fn meta(path: &Path, id: Guid) {
        let sidecar = MetaSidecar::new("level", regia_asset::AssetId(id));
        std::fs::write(
            regia_asset::meta_path_for(path),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("write meta");
    }

    fn level_text(records: &str, resources: &str) -> String {
        format!(
            "SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [{records}], resources: {{{resources}}})"
        )
    }

    fn record(id: u128, components: &str) -> String {
        format!(
            r#"(guid: "{}", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), {components} }}),"#,
            Guid::from_u128(id)
        )
    }

    fn write_defs(assets: &Path) -> PathBuf {
        let path = assets.join("forest.layers.ron");
        std::fs::write(
            &path,
            r#"LayerDefs(format_version: 1, layers: [(name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None)])"#,
        )
        .expect("defs");
        meta(&path, DEFS_ID);
        path
    }

    /// A project: `assets/forest.level.d` binding the definitions and placing `assets/sub.level.ron`.
    fn project() -> (PathBuf, PathBuf) {
        let root = unique_temp_dir("level_assets");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        write_defs(&assets);
        let sub = assets.join("sub.level.ron");
        std::fs::write(&sub, level_text(&record(0x10, ""), "")).expect("sub");
        meta(&sub, SUB_ID);
        let binding = format!(
            r#""regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: [(layer: "night", initial: Parked, editor_visible: true, editor_resident: true, peers: Everyone)]),"#
        );
        let member = record(
            0x20,
            r#""regia_scene::layers::LayerMembership": (layers: ["night"]),"#,
        );
        let placement = record(
            0x21,
            &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB_ID}"),"#),
        );
        let file = assets.join("forest.level.ron");
        std::fs::write(&file, level_text(&format!("{member}{placement}"), &binding)).expect("level");
        let dir = assets.join("forest.level.d");
        split_level_file(&file, &dir).expect("split");
        std::fs::remove_file(&file).expect("remove the file form");
        (root, dir)
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, ScenePlugin, LevelAssetsPlugin));
        app
    }

    #[test]
    fn a_level_directory_loads_with_its_definitions_and_placed_levels() {
        let (root, dir) = project();
        let mut app = app();
        let report = load_level_source(app.world_mut(), &dir).expect("load");
        assert_eq!(report.layer_defs, [DEFS_ID]);
        assert_eq!(report.levels.len(), 2, "{report:?}");
        assert!(report.missing.is_empty(), "{report:?}");
        assert!(app.world().resource::<LevelLibrary>().contains(LevelId(SUB_ID)));

        let id = report.level.expect("id");
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        app.update();
        let table = app.world().resource::<LayerTable>();
        assert_eq!(table.effective("night"), Some(LayerState::Parked));
        assert!(table.warnings().is_empty(), "{:?}", table.warnings());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn both_forms_of_one_level_are_refused() {
        let (root, dir) = project();
        std::fs::write(root.join("assets/forest.level.ron"), level_text("", "")).expect("file");
        let mut app = app();
        let err = load_level_source(app.world_mut(), &dir).expect_err("ambiguous");
        assert_eq!(err.reason(), "ambiguous_level_form");
        assert!(!app.world().resource::<LevelLibrary>().contains(level_id_of(&dir)));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_missing_definition_or_placed_level_is_reported_and_the_level_loads() {
        let root = unique_temp_dir("level_assets_missing");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let binding =
            format!(r#""regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: []),"#);
        let placement = record(
            0x21,
            &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB_ID}"),"#),
        );
        let path = assets.join("a.level.ron");
        std::fs::write(&path, level_text(&placement, &binding)).expect("level");
        let mut app = app();
        let report = load_level_source(app.world_mut(), &path).expect("load");
        assert_eq!(report.missing.len(), 2, "{report:?}");
        assert_eq!(report.levels.len(), 1);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn invalid_definitions_are_refused_with_the_spec_reason() {
        let root = unique_temp_dir("level_assets_defs");
        let path = root.join("bad.layers.ron");
        std::fs::write(
            &path,
            r#"LayerDefs(format_version: 1, layers: [(name: "a", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: Some("ghost"))])"#,
        )
        .expect("defs");
        let err = read_layer_defs(&path).expect_err("bad parent");
        assert_eq!(err.reason(), "layer_parent_invalid");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    fn packaged_defs() -> LayerDefs {
        LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "blockout", kind: EditorOnly, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs")
    }

    fn packaged_level() -> SceneFile {
        let binding = format!(
            r#""regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: [
                (layer: "night", initial: Parked, editor_visible: true, editor_resident: true, peers: Everyone),
                (layer: "blockout", initial: Live, editor_visible: true, editor_resident: true, peers: Everyone),
            ]),"#
        );
        let member = r#""regia_scene::layers::LayerMembership": (layers: ["night", "blockout"]),"#;
        let editor = r#""regia_scene::layers::LayerMembership": (layers: ["blockout"]),"#;
        let test = format!(r#""{FUNCTIONAL_TEST_PATH}": (format_version: 1),"#);
        let child = format!(
            r#"(guid: "{}", parent: Some("{}"), components: {{}}),"#,
            Guid::from_u128(0x41),
            Guid::from_u128(0x40)
        );
        let records = format!(
            "{}{}{}{child}{}",
            record(0x20, member),
            record(0x30, editor),
            record(0x40, &test),
            record(0x50, ""),
        );
        SceneFile::from_ron_str(&level_text(&records, &binding)).expect("level")
    }

    fn guids(scene: &SceneFile) -> Vec<u128> {
        scene.entities.iter().map(|e| e.guid.as_u128()).collect()
    }

    #[test]
    fn validation_lists_what_is_wrong_in_one_run() {
        let (root, dir) = project();
        assert_eq!(validate_level_source(&dir).expect("validate"), Vec::new());

        let mut scene = regia_scene::level_dir::read_level_source(&dir).expect("read");
        let stray = record(
            0x30,
            r#""regia_scene::layers::LayerMembership": (layers: ["ghost"]),"#,
        );
        let extra = SceneFile::from_ron_str(&level_text(&stray, "")).expect("extra");
        scene.entities.extend(extra.entities);
        write_level_dir(&dir, &scene).expect("write");
        std::fs::remove_file(root.join("assets/forest.layers.ron.meta")).expect("orphan the definitions");

        let problems = validate_level_source(&dir).expect("validate");
        let reasons: Vec<&str> = problems.iter().map(|p| p.reason).collect();
        assert_eq!(
            reasons.iter().filter(|r| **r == "layer_unknown").count(),
            3,
            "{problems:?}"
        );
        assert!(problems.iter().all(|p| !p.fatal));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_shipping_level_loses_its_tests_and_editor_only_members_and_carries_its_definitions() {
        let scene = packaged_level();
        let packaged = prepare_level_for_package(&scene, &|_| Some(packaged_defs()), true);
        assert_eq!(packaged.tests_stripped, 2, "the test and its child");
        assert_eq!(packaged.editor_only_stripped, 1);
        assert_eq!(guids(&packaged.scene), [0x20, 0x50]);
        let member = LayerMembership::from_record(&packaged.scene.entities[0]).expect("membership");
        assert_eq!(member.layers, ["night"], "editor-only names leave too");

        let binding = LevelLayers::from_scene(&packaged.scene)
            .expect("bound")
            .expect("parses");
        assert!(binding.defs.is_empty());
        assert_eq!(binding.entries.len(), 1);
        assert_eq!(binding.embedded.len(), 1);
        assert_eq!(binding.embedded[0].layers.len(), 1);
        assert_eq!(binding.embedded[0].layers[0].name, "night");

        // It round trips through the file a package holds.
        let text = packaged.scene.to_ron_string().expect("ron");
        let back = SceneFile::from_ron_str(&text).expect("parse");
        assert_eq!(guids(&back), [0x20, 0x50]);
    }

    #[test]
    fn a_dev_package_keeps_its_tests() {
        let packaged = prepare_level_for_package(&packaged_level(), &|_| Some(packaged_defs()), false);
        assert_eq!(packaged.tests_stripped, 0);
        assert!(guids(&packaged.scene).contains(&0x40));
    }

    #[test]
    fn an_unknown_definition_stays_bound() {
        let packaged = prepare_level_for_package(&packaged_level(), &|_| None, true);
        let binding = LevelLayers::from_scene(&packaged.scene)
            .expect("bound")
            .expect("parses");
        assert_eq!(binding.defs, [format!("asset:{DEFS_ID}")]);
        assert!(binding.embedded.is_empty());
    }

    #[test]
    fn a_level_without_layers_or_tests_is_left_alone() {
        let scene = SceneFile::from_ron_str(&level_text(&record(0x20, ""), "")).expect("level");
        let packaged = prepare_level_for_package(&scene, &|_| None, true);
        assert_eq!(packaged.scene, scene);
    }

    #[test]
    fn the_id_is_stable_and_shared_by_the_forms() {
        let plain = level_id_of(Path::new("assets/a.level.ron"));
        assert_eq!(plain, level_id_of(Path::new("assets/a.level.ron")));
        assert_eq!(plain, level_id_of(Path::new("assets/a.level.d")));
        // The path-derived id names the level under its `assets/` root, not by the spelling it
        // was opened with: separators and any directory prefix fold away, so a save or a travel
        // that stores the id resolves on every machine.
        assert_eq!(
            plain,
            level_id_of(Path::new("assets\\a.level.ron")),
            "a Windows-separator spelling"
        );
        assert_eq!(
            plain,
            level_id_of(Path::new("/opt/game/assets/a.level.ron")),
            "an absolute host path"
        );
        assert_eq!(
            plain,
            level_id_of(Path::new("D:\\game\\assets\\a.level.ron")),
            "an absolute Windows path"
        );
        assert_eq!(
            plain,
            level_id_of(Path::new("assets/./a.level.ron")),
            "a same-directory spelling"
        );
        assert_eq!(
            plain,
            level_id_of(Path::new("assets/levels/../a.level.ron")),
            "a parent-directory spelling"
        );
        assert_eq!(
            plain,
            level_id_of(Path::new("assets\\levels\\..\\a.level.ron")),
            "a parent-directory Windows spelling"
        );
        assert_ne!(plain, level_id_of(Path::new("assets/b.level.ron")));
    }

    /// `LevelSourceIndex::scan` is order-independent: two sidecars sharing a guid keep the
    /// lexicographically first source, whatever order `read_dir` visited them in.
    #[test]
    fn duplicate_level_guid_keeps_the_sorted_path() {
        let root = unique_temp_dir("level_assets_dup_guid");
        let assets = root.join("assets");
        // Create the later path first, so a directory that reports entries in creation order
        // visits it before the one that must win.
        let later = assets.join("b/copy/town.level.ron");
        std::fs::create_dir_all(later.parent().expect("parent")).expect("dirs");
        meta(&later, SUB_ID);
        let first = assets.join("a/town.level.ron");
        std::fs::create_dir_all(first.parent().expect("parent")).expect("dirs");
        meta(&first, SUB_ID);
        let index = LevelSourceIndex::scan(&assets);
        assert_eq!(index.levels.get(&SUB_ID), Some(&first));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn terrain_layer_stacks_are_not_layer_definitions() {
        assert!(is_layer_defs_name("forest.layers.ron"));
        assert!(!is_layer_defs_name("forest.terrain.layers.ron"));
    }

    #[test]
    fn an_edited_entity_file_reloads_the_active_level() {
        let (root, dir) = project();
        let mut app = app();
        let report = load_level_source(app.world_mut(), &dir).expect("load");
        let id = report.level.expect("id");
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let before = app.world().resource::<LevelManager>().load_generation(id);

        // One more entity, written the way an editor saves it.
        let (mut scene, _) = regia_scene::level_dir::read_level_dir(&dir).expect("read");
        let extra: SceneFile = SceneFile::from_ron_str(&level_text(&record(0x30, ""), "")).expect("extra");
        scene.entities.extend(extra.entities);
        write_level_dir(&dir, &scene).expect("write");
        let changed = dir.join(regia_scene::level_dir::entity_rel_path(Guid::from_u128(0x30)));
        app.world_mut().write_message(SourceAssetChanged::new(changed));
        app.update();
        let manager = app.world().resource::<LevelManager>();
        assert!(manager.load_generation(id) > before);
        let level = app
            .world()
            .resource::<LevelLibrary>()
            .get(id)
            .expect("level")
            .clone();
        assert_eq!(level.scene.entities.len(), 3);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn one_save_of_many_entity_files_reloads_the_level_once() {
        let (root, dir) = project();
        let mut app = app();
        let report = load_level_source(app.world_mut(), &dir).expect("load");
        let id = report.level.expect("id");
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let before = app.world().resource::<LevelManager>().load_generation(id);
        for guid in [0x20, 0x21] {
            let changed = dir.join(regia_scene::level_dir::entity_rel_path(Guid::from_u128(guid)));
            app.world_mut().write_message(SourceAssetChanged::new(changed));
        }
        app.world_mut()
            .write_message(SourceAssetChanged::new(dir.join("level.ron")));
        app.update();
        let after = app.world().resource::<LevelManager>().load_generation(id);
        assert_eq!(after, before + 1, "three events, one reopen");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn both_spellings_of_a_level_and_a_relative_path_share_one_key() {
        let cwd = std::env::current_dir().expect("cwd");
        let absolute = cwd.join("no_such_folder/forest.level.ron");
        assert_eq!(
            level_key(Path::new("no_such_folder/forest.level.d")),
            level_key(&absolute)
        );
        assert_eq!(
            level_key(Path::new("no_such_folder/forest.level.ron")),
            level_key(&absolute)
        );
        let root = unique_temp_dir("level_assets_key");
        std::fs::create_dir_all(root.join("sub")).expect("folder");
        assert_eq!(
            source_key(&root.join("sub/../forest.layers.ron")),
            source_key(&root.join("forest.layers.ron")),
            "a resolved folder"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_level_loaded_by_its_other_spelling_still_reloads() {
        let (root, dir) = project();
        let mut app = app();
        let spelled = dir.with_file_name("forest.level.ron");
        let report = load_level_source(app.world_mut(), &spelled).expect("load");
        let id = report.level.expect("id");
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let before = app.world().resource::<LevelManager>().load_generation(id);
        app.world_mut()
            .write_message(SourceAssetChanged::new(dir.join("level.ron")));
        app.update();
        assert_eq!(
            app.world().resource::<LevelManager>().load_generation(id),
            before + 1
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_streamed_level_stays_under_its_instance_across_a_reload() {
        let root = unique_temp_dir("level_assets_streamed");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let sub = assets.join("sub.level.ron");
        std::fs::write(&sub, level_text(&record(0x10, ""), "")).expect("sub");
        meta(&sub, SUB_ID);
        let host = assets.join("host.level.ron");
        let placement = record(
            0x21,
            &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB_ID}", mode: Streamed),"#),
        );
        std::fs::write(&host, level_text(&placement, "")).expect("host");
        let mut app = app();
        let report = load_level_source(app.world_mut(), &host).expect("load");
        open_level(app.world_mut(), report.level.expect("id"), LoadMode::Replace).expect("open");
        app.update();
        app.update();

        let parents = |app: &mut App| -> BTreeMap<u128, Option<Entity>> {
            let world = app.world_mut();
            let mut query = world.query::<(&regia_scene::prelude::EntityGuid, Option<&ChildOf>)>();
            query
                .iter(world)
                .map(|(guid, link)| (guid.0.as_u128(), link.map(ChildOf::parent)))
                .collect()
        };
        let instance = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
            query
                .iter(world)
                .find(|(_, guid)| guid.0.as_u128() == 0x21)
                .map(|(entity, _)| entity)
                .expect("the instance")
        };
        assert_eq!(parents(&mut app).get(&0x10), Some(&Some(instance)));

        let two = format!("{}{}", record(0x10, ""), record(0x11, ""));
        std::fs::write(&sub, level_text(&two, "")).expect("edit");
        app.world_mut().write_message(SourceAssetChanged::new(sub));
        app.update();
        app.update();
        let after = parents(&mut app);
        assert_eq!(
            after.get(&0x10),
            Some(&Some(instance)),
            "the root moved off its instance"
        );
        assert_eq!(after.get(&0x11), Some(&Some(instance)), "the new root too");
        let spawned = app
            .world()
            .get::<regia_scene::placed::PlacedSpawned>(instance)
            .expect("still filled");
        assert_eq!(spawned.entities, 2);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn an_edited_definition_file_rebuilds_the_layer_table() {
        let (root, dir) = project();
        let mut app = app();
        let report = load_level_source(app.world_mut(), &dir).expect("load");
        open_level(app.world_mut(), report.level.expect("id"), LoadMode::Replace).expect("open");
        app.update();
        let path = root.join("assets/forest.layers.ron");
        std::fs::write(
            &path,
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "day", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut().write_message(SourceAssetChanged::new(path));
        app.update();
        app.update();
        assert!(app.world().resource::<LayerTable>().contains("day"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
