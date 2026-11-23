// SPDX-License-Identifier: MPL-2.0
//! Open level, new level, save level: the File menu's level flow over `regia_scene`'s
//! `LevelManager` and canonical `.level.ron` files (`docs/specs/19-editor.md` § 8 M10).

use std::path::{Path, PathBuf};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_scene::level::{open_level, snapshot_level};
use regia_scene::prelude::{Level, LevelId, LevelLibrary, LevelManager, LoadMode};

/// Which level is open and where it saves (`None` path = never saved / untitled).
#[derive(regia_ecs::prelude::Resource, Debug, Clone)]
pub struct OpenLevel {
    /// The level's asset identity.
    pub id: LevelId,
    /// Where the level was last saved.
    pub path: Option<PathBuf>,
}

/// Starts a clean editing session after a different level replaced the world: a new undo
/// history (old commands address entities and level ids of the level that is gone) and an empty
/// selection. The navigation preview of the old level goes too (a game's `NavMesh` stays). A running
/// reflection capture bake belongs to the level that is gone: it ends `cancelled` with
/// `bake_level_changed` (`crate::capture_bake::cancel_for_level_change`).
pub fn reset_edit_state(world: &mut World) {
    crate::capture_bake::cancel_for_level_change(world);
    crate::activity_panel::retain_commands(world);
    world.insert_resource(crate::command::UndoStack::new());
    // `Err` only for a game's mesh, which is not the editor's to drop.
    crate::nav_preview::clear(world).ok();
    if let Some(mut selection) = world.get_resource_mut::<crate::selection::Selection>() {
        selection.clear();
    }
}

/// Creates a fresh untitled level in `world` (unloading the current one) and returns its state.
pub fn new_level(world: &mut World) -> OpenLevel {
    let id = LevelId(Guid::new_random());
    // The `Replace` open below unloads the current level itself. An unload here first would make
    // the manager promote a level opened beside the host to current, and the open would then
    // unload that one too (P-097: a prefab session's open and close dropped such levels).
    let level = Level {
        id,
        settings: Default::default(),
        scene: Default::default(),
    };
    world
        .get_resource_or_insert_with::<LevelLibrary>(LevelLibrary::new)
        .insert(level);
    if let Err(err) = open_level(world, id, LoadMode::Replace) {
        tracing::warn!(target: "regia_editor", "new level: {err}");
    }
    OpenLevel { id, path: None }
}

/// Opens a level, a `.level.ron` file or a `.level.d` directory, into `world` (Replace), with the
/// layer definitions it binds and the levels it places. Returns the open level on success.
///
/// # Errors
/// When the level cannot be read, parsed or spawned.
pub fn open_level_file(world: &mut World, path: &Path) -> Result<OpenLevel, String> {
    // Startup opens levels before agent extensions register editor authoring components.
    crate::blockout::register(world);
    let report = regia_framework::level_assets::load_level_source(world, path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let id = report
        .level
        .unwrap_or_else(|| regia_framework::level_assets::level_id_of(path));
    preload_level_prefabs(world, &report.levels);
    open_level(world, id, LoadMode::Replace).map_err(|e| format!("{}", e))?;
    // From here on the edits to this level feed its dirty set (spec 37 § 3.4, ADR-7203).
    crate::level_dirty::begin_tracking(world, id, path);
    // The per-document feed starts from what is open now (ADR-7792).
    crate::level_dirty::resync_documents(world);
    Ok(OpenLevel {
        id,
        path: Some(path.to_path_buf()),
    })
}

/// Resolve project prefab references before spawning a freshly opened level. Missing assets keep
/// the scene loader's pending-prefab behavior; a valid variant registers its existing base chain.
pub(crate) fn preload_level_prefabs(world: &mut World, levels: &[LevelId]) {
    let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
    else {
        return;
    };
    let Some(library) = world.get_resource::<LevelLibrary>() else {
        return;
    };
    let assets: std::collections::BTreeSet<Guid> = levels
        .iter()
        .filter_map(|id| library.get(*id))
        .flat_map(|level| level.scene.entities.iter())
        .filter_map(|entity| {
            entity
                .prefab_instance
                .as_ref()
                .map(|instance| instance.prefab.guid())
        })
        .collect();
    for asset in assets {
        if world
            .get_resource::<regia_scene::prefab::PrefabLibrary>()
            .is_some_and(|library| library.contains(asset))
        {
            continue;
        }
        let Some(path) = crate::prefab_variant_edit::find_prefab_by_id(&root, asset) else {
            tracing::warn!(target: "regia_editor", %asset, "level prefab not found in project");
            continue;
        };
        if let Err(error) = crate::prefab_variant_edit::register_prefab_file(world, &path) {
            tracing::warn!(target: "regia_editor", %asset, "level prefab: {error}");
        }
    }
}

/// The first free `unsaved.level.ron`, `unsaved-2.level.ron`, … under `dir`: the dialog-free
/// save-on-close path must never overwrite an earlier untitled save.
#[must_use]
pub fn free_untitled_level_path(dir: &Path) -> PathBuf {
    let first = dir.join("unsaved.level.ron");
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| dir.join(format!("unsaved-{n}.level.ron")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// Snapshots the open level's entities and settings to `path` atomically (temp file + rename).
///
/// # Errors
/// When the snapshot or the write fails.
pub fn save_level_file(world: &mut World, open: &OpenLevel, path: &Path) -> Result<(), String> {
    // Runtime spawns (`scene.spawn`) join the open level before it is written (ADR-7382).
    adopt_runtime_spawns(world, open.id);
    // A level variant saves only its differences from the base (ADR-7823).
    if regia_scene::level_variant::is_level_variant_path(path) {
        return crate::level_variant::save_variant(world, open, path);
    }
    // A running sequence preview's values never reach the file (ADR-9710).
    let mut level =
        crate::seq_tracks::with_preview_lifted(world, |world| snapshot_saved_level(world, open.id))
            .map_err(|e| format!("{}", e))?;
    keep_layer_binding(world, open.id, &mut level.scene);
    if is_level_dir(path) {
        // Only the editor's dirty set is written when the tracking is complete for this
        // directory; otherwise every entity file whose bytes changed (spec 37 § 3.4, ADR-7203).
        match crate::level_dirty::dirty_set_for_save(world, open.id, path) {
            Some(dirty) => regia_scene::level_dir::write_level_dir_dirty(path, &level.scene, &dirty),
            None => regia_scene::level_dir::write_level_dir(path, &level.scene),
        }
        .map_err(|e| format!("{e}"))?;
        restart_tracking_after_save(world, open, path);
        return Ok(());
    }
    let text = level.to_ron_string().map_err(|e| format!("{}", e))?;
    // At its own path, a level with layers in their own files writes each member to its layer
    // file (only the files whose bytes changed); any other path (an autosave's recovery copy, a
    // save-as) gets the whole level (ADR-7792).
    let separated = own_separated_layers(world, open, path);
    if separated.is_empty() {
        write_atomic(path, text.as_bytes())?;
    } else {
        regia_scene::layer_file::write_split_level(
            path,
            &text,
            &separated,
            &regia_scene::layer_file::SplitTarget::All,
            false,
        )
        .map_err(|e| format!("{e}"))?;
    }
    restart_tracking_after_save(world, open, path);
    Ok(())
}

/// The layers of `open` stored in their own files when `path` is its own path.
fn own_separated_layers(
    world: &mut World,
    open: &OpenLevel,
    path: &Path,
) -> std::collections::BTreeSet<String> {
    if open.path.as_deref() != Some(path) {
        return Default::default();
    }
    sync_open_levels(world);
    world
        .get_resource::<OpenLevels>()
        .and_then(|model| model.separated.get(&open.id).cloned())
        .unwrap_or_default()
}

/// A snapshot carries the level's settings but not its layer binding (`LevelLayers`): the
/// binding is the one the level was loaded with (its [`LevelLibrary`] copy, the same source
/// play-in-editor uses), so a save writes it back instead of dropping it (ADR-7203).
pub fn keep_layer_binding(world: &World, level: LevelId, scene: &mut regia_scene::prelude::SceneFile) {
    let key = regia_scene::layers::LEVEL_LAYERS_PATH;
    if scene.resources.contains_key(key) {
        return;
    }
    if let Some(binding) = world
        .get_resource::<LevelLibrary>()
        .and_then(|library| library.get(level))
        .and_then(|loaded| loaded.scene.resources.get(key).cloned())
    {
        scene.resources.insert(key.to_string(), binding);
    }
}

/// A save to the open level's own path makes that path clean: the dirty set starts over. A save
/// elsewhere (an autosave's recovery copy, a save-as before the open path moves) leaves the
/// tracking alone.
fn restart_tracking_after_save(world: &mut World, open: &OpenLevel, path: &Path) {
    let own = open.path.as_deref().is_some_and(|own| {
        regia_scene::level_dir::level_paths(own).1 == regia_scene::level_dir::level_paths(path).1
    });
    if own {
        crate::level_dirty::begin_tracking(world, open.id, path);
        crate::level_dirty::mark_level_documents_saved(world, open.id);
    }
}

/// The level as it is saved: [`snapshot_level`] without the `EditorOnly` and `GltfEditorOnly`
/// entities and their descendants (`specs/19` § 1, [`crate::editor_only`]).
///
/// # Errors
/// When the snapshot fails.
pub fn snapshot_saved_level(
    world: &mut World,
    id: LevelId,
) -> Result<Level, regia_scene::spawn::SceneSpawnError> {
    let mut level = snapshot_level(world, id)?;
    crate::editor_only::strip_scene(world, &mut level.scene);
    Ok(level)
}

/// Saves the level and what the terrain tool edited: the re-cooked tiles of sculpted terrains,
/// the holes and the painted layer weights next to the level
/// ([`crate::terrain_tool::save_terrain_files`]). Every save a user asks for goes through here;
/// only the autosave stays [`save_level_file`] (it is a recovery copy of the level, and must not
/// touch the cooked heightfield or the level's holes).
///
/// # Errors
/// When the level or a terrain file cannot be written, or a prefab is open, in place or on its own
/// ([`crate::prefab_edit::level_save_refusal`]; nothing is written).
pub fn save_level_and_terrain(
    world: &mut World,
    open: &OpenLevel,
    path: &Path,
) -> Result<crate::terrain_tool::TerrainFilesSaved, String> {
    // A prefab open in place: the level in the world carries the session's state (ADR-7835).
    if let Some(refusal) = crate::prefab_edit::level_save_refusal(world) {
        return Err(refusal.to_string());
    }
    // A level file someone else locks (git LFS) is never overwritten (ADR-7864).
    crate::vcs::locks::guard_level_save(world, path)?;
    save_level_file(world, open, path)?;
    // The save keeps a version in the project's history (ADR-7928).
    crate::version_history::note_level_saved(world, open, path);
    let saved = crate::terrain_tool::save_terrain_files(world, path)?;
    // The save trigger of the asset validators: a queue insert; the run happens on a later tick.
    crate::save_validation::note_saved(world, path);
    Ok(saved)
}

/// Makes the user entities that belong to no level members of the open level `level` before it
/// is snapshotted ([`crate::visibility::adopt_into_level`], ADR-7382): an entity the runtime
/// methods `scene.spawn` / `scene.spawn_primitive` made is saved by the next save and autosave
/// whether or not a shell frame ran in between. Nothing happens in prefab edit mode (the open
/// level is the mode's scratch level; the prefab's contents are no level's) or when `level` is not
/// the current level. The sequence players the Sequence Curves panel opened stay out: they are
/// that panel's documents, saved to their own `.seq.ron`. Returns how many were adopted.
pub fn adopt_runtime_spawns(world: &mut World, level: LevelId) -> usize {
    if crate::prefab_edit::in_prefab_mode(world) {
        return 0;
    }
    let current = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current);
    if current != Some(level) {
        return 0;
    }
    let keep = crate::seq_autosave::document_players(world);
    crate::visibility::adopt_into_level(world, level, &keep)
}

/// Whether `path` names a level in the directory form.
#[must_use]
pub fn is_level_dir(path: &Path) -> bool {
    path.to_string_lossy()
        .ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX)
}

/// Writes bytes atomically: temp file in the same directory, then rename. This is the
/// shared [`crate::atomic_write`] helper: a failed save (full disk, held or read-only
/// destination on Windows) never leaves a stale temp and a read-only destination is
/// cleared and retried instead of failing every save (audit G editor4-platform L3, the
/// half of audit G finding 10 the earlier lane deferred).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    crate::atomic_write::write_atomic(path, bytes)
}

// ---------------------------------------------------------------------------------------------
// Several open levels (`docs/specs/37-per-entity-levels-layers-instances.md` § 12, ADR-7792)

/// What an open document is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    /// The host level (File ▸ Open; `OpenLevel`).
    Host,
    /// A level opened beside the host (additively).
    Level,
    /// A layer of an open level stored in its own file (`<level>.<layer>.layer.ron`).
    Layer(String),
}

/// One open document: a level or a layer file, with its own dirty state and Save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelDocument {
    /// The document id commands are tagged with: a level's own id, or [`layer_document_id`].
    pub id: u128,
    /// The level the document belongs to.
    pub level: LevelId,
    /// The level's path (`None` for an untitled host).
    pub path: Option<PathBuf>,
    /// What it is.
    pub kind: DocumentKind,
}

impl LevelDocument {
    /// The file a Save of this document writes.
    #[must_use]
    pub fn file(&self) -> Option<PathBuf> {
        let path = self.path.as_deref()?;
        Some(match &self.kind {
            DocumentKind::Layer(layer) => regia_scene::layer_file::layer_file_path(path, layer),
            _ => path.to_path_buf(),
        })
    }

    /// The id as text (a guid spelling), as the agent methods show it.
    #[must_use]
    pub fn id_text(&self) -> String {
        Guid::from_u128(self.id).to_string()
    }
}

/// The documents open beside the host and the active level (ADR-7792).
#[derive(regia_ecs::prelude::Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenLevels {
    /// Levels opened beside the host, in open order.
    pub extra: Vec<(LevelId, PathBuf)>,
    /// Where new entities go; `None` is the host.
    pub active: Option<LevelId>,
    /// Per open level, its layers stored in their own files.
    pub separated: std::collections::BTreeMap<LevelId, std::collections::BTreeSet<String>>,
    /// The host this model was last synchronized with.
    host: Option<(LevelId, Option<PathBuf>)>,
}

/// An error of the open-levels operations: a lowercase reason code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelsError {
    /// `no_host_level`, `level_already_open`, `level_not_open`, `host_level`, `level_dirty`,
    /// `level_untitled`, `layer_unknown`, `layer_not_separated`, `layer_file_needs_file_form`,
    /// `document_unknown` or a level reader's reason.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl LevelsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LevelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The document id of `layer` of `level`: FNV-1a (128-bit) of the level id's bytes and the
/// layer name, stable across sessions.
#[must_use]
pub fn layer_document_id(level: LevelId, layer: &str) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for byte in level.0.as_u128().to_le_bytes().iter().chain(layer.as_bytes()) {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The separated layers of the level stored at `path` (none for the directory form, which the
/// editor never splits, or an unreadable directory).
fn separated_on_disk(path: &Path) -> std::collections::BTreeSet<String> {
    if is_level_dir(path) {
        return Default::default();
    }
    regia_scene::layer_file::separated_layers(path).unwrap_or_default()
}

/// Brings [`OpenLevels`] in line with the world: a new host (File ▸ Open, New) drops the levels
/// that are no longer loaded and reads the host's layer files; a level the manager unloaded leaves
/// the list. Cheap when nothing changed (no file access).
pub fn sync_open_levels(world: &mut World) {
    let host = world
        .get_resource::<OpenLevel>()
        .map(|open| (open.id, open.path.clone()));
    let loaded: std::collections::BTreeSet<LevelId> = world
        .get_resource::<LevelManager>()
        .map(|manager| manager.levels().map(|(id, _)| id).collect())
        .unwrap_or_default();
    let mut model = world.remove_resource::<OpenLevels>().unwrap_or_default();
    if model.host != host {
        model.separated.clear();
        if let Some((id, Some(path))) = &host {
            model.separated.insert(*id, separated_on_disk(path));
        }
        model.host = host.clone();
    }
    model.extra.retain(|(id, _)| loaded.contains(id));
    let keep: std::collections::BTreeSet<LevelId> = model
        .extra
        .iter()
        .map(|(id, _)| *id)
        .chain(host.as_ref().map(|(id, _)| *id))
        .collect();
    model.separated.retain(|id, _| keep.contains(id));
    if model.active.is_some_and(|active| !keep.contains(&active)) {
        model.active = None;
    }
    world.insert_resource(model);
}

/// Every open document: the host, its layer files, then each level opened beside it and its
/// layer files.
pub fn documents(world: &mut World) -> Vec<LevelDocument> {
    sync_open_levels(world);
    let Some(host) = world.get_resource::<OpenLevel>().cloned() else {
        return Vec::new();
    };
    let model = world.get_resource::<OpenLevels>().cloned().unwrap_or_default();
    let mut out = Vec::new();
    let levels = std::iter::once((host.id, host.path.clone(), DocumentKind::Host)).chain(
        model
            .extra
            .iter()
            .map(|(id, path)| (*id, Some(path.clone()), DocumentKind::Level)),
    );
    for (level, path, kind) in levels {
        out.push(LevelDocument {
            id: level.0.as_u128(),
            level,
            path: path.clone(),
            kind,
        });
        for layer in model.separated.get(&level).into_iter().flatten() {
            out.push(LevelDocument {
                id: layer_document_id(level, layer),
                level,
                path: path.clone(),
                kind: DocumentKind::Layer(layer.clone()),
            });
        }
    }
    out
}

/// The level new entities land in: the active level while it is open, else the current (host)
/// level.
#[must_use]
pub fn spawn_target_level(world: &World) -> Option<LevelId> {
    let manager = world.get_resource::<LevelManager>();
    let active = world
        .get_resource::<OpenLevels>()
        .and_then(|model| model.active)
        .filter(|active| manager.is_some_and(|m| m.state(*active).is_some()));
    active.or_else(|| manager.and_then(LevelManager::current))
}

/// Opens the level at `path` beside the host (additively), with its own dirty state and Save.
///
/// # Errors
/// `no_host_level`, `level_already_open`, or the reader's reason when the level cannot be read.
pub fn open_level_document(world: &mut World, path: &Path) -> Result<LevelDocument, LevelsError> {
    sync_open_levels(world);
    let host = world
        .get_resource::<OpenLevel>()
        .map(|open| open.id)
        .ok_or_else(|| LevelsError::new("no_host_level", "open a level first"))?;
    let id = regia_framework::level_assets::level_id_of(path);
    let open = |world: &World, id: LevelId| {
        id == host
            || world
                .get_resource::<OpenLevels>()
                .is_some_and(|model| model.extra.iter().any(|(other, _)| *other == id))
    };
    if open(world, id) {
        return Err(LevelsError::new(
            "level_already_open",
            format!("{} is already open", path.display()),
        ));
    }
    let report = regia_framework::level_assets::load_level_source(world, path)
        .map_err(|e| LevelsError::new(e.reason(), format!("cannot read {}: {e}", path.display())))?;
    let id = report.level.unwrap_or(id);
    if open(world, id) {
        return Err(LevelsError::new(
            "level_already_open",
            format!("{} is already open", path.display()),
        ));
    }
    preload_level_prefabs(world, &report.levels);
    open_level(world, id, LoadMode::Additive)
        .map_err(|e| LevelsError::new("level_invalid", e.to_string()))?;
    {
        let mut model = world.get_resource_or_insert_with(OpenLevels::default);
        model.extra.push((id, path.to_path_buf()));
        model.separated.insert(id, separated_on_disk(path));
    }
    crate::level_dirty::resync_documents(world);
    Ok(LevelDocument {
        id: id.0.as_u128(),
        level: id,
        path: Some(path.to_path_buf()),
        kind: DocumentKind::Level,
    })
}

/// Whether any document of `level` has unsaved edits.
pub fn level_has_unsaved_documents(world: &mut World, level: LevelId) -> bool {
    crate::level_dirty::feed_documents(world);
    let docs: Vec<u128> = documents(world)
        .into_iter()
        .filter(|doc| doc.level == level)
        .map(|doc| doc.id)
        .collect();
    world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(|stack| docs.iter().any(|doc| stack.is_document_dirty(*doc)))
}

/// Closes a level opened beside the host. Its unsaved edits refuse the close (`level_dirty`)
/// unless `discard`; other levels' edits never do.
///
/// # Errors
/// `host_level`, `level_not_open`, `level_dirty`.
pub fn close_level_document(world: &mut World, level: LevelId, discard: bool) -> Result<(), LevelsError> {
    sync_open_levels(world);
    if world
        .get_resource::<OpenLevel>()
        .is_some_and(|open| open.id == level)
    {
        return Err(LevelsError::new(
            "host_level",
            "the host level closes with File ▸ Open or New",
        ));
    }
    let open = world
        .get_resource::<OpenLevels>()
        .is_some_and(|model| model.extra.iter().any(|(id, _)| *id == level));
    if !open {
        return Err(LevelsError::new("level_not_open", format!("{level} is not open")));
    }
    if !discard && level_has_unsaved_documents(world, level) {
        return Err(LevelsError::new(
            "level_dirty",
            format!("{level} has unsaved changes; save it or close with discard"),
        ));
    }
    let docs: Vec<u128> = documents(world)
        .into_iter()
        .filter(|doc| doc.level == level)
        .map(|doc| doc.id)
        .collect();
    if let Err(err) = regia_scene::level::unload_level(world, level) {
        tracing::warn!(target: "regia_editor", %level, "close level: {err}");
    }
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        for doc in docs {
            stack.forget_document(doc);
        }
    }
    let mut model = world.get_resource_or_insert_with(OpenLevels::default);
    model.extra.retain(|(id, _)| *id != level);
    model.separated.remove(&level);
    if model.active == Some(level) {
        model.active = None;
    }
    crate::level_dirty::resync_documents(world);
    Ok(())
}

/// Makes `level` (`None`: the host) the level new entities land in.
///
/// # Errors
/// `level_not_open`.
pub fn set_active_level(world: &mut World, level: Option<LevelId>) -> Result<(), LevelsError> {
    sync_open_levels(world);
    let host = world.get_resource::<OpenLevel>().map(|open| open.id);
    let active = match level {
        Some(level) if Some(level) == host => None,
        Some(level) => {
            let open = world
                .get_resource::<OpenLevels>()
                .is_some_and(|model| model.extra.iter().any(|(id, _)| *id == level));
            if !open {
                return Err(LevelsError::new("level_not_open", format!("{level} is not open")));
            }
            Some(level)
        }
        None => None,
    };
    world.get_resource_or_insert_with(OpenLevels::default).active = active;
    Ok(())
}

/// The active level (the host when none is set).
pub fn active_level(world: &mut World) -> Option<LevelId> {
    sync_open_levels(world);
    world
        .get_resource::<OpenLevels>()
        .and_then(|model| model.active)
        .or_else(|| world.get_resource::<OpenLevel>().map(|open| open.id))
}

/// The canonical text of `level` as a save writes it (the host adopts runtime spawns first).
fn level_text_for_save(world: &mut World, level: LevelId, host: bool) -> Result<String, LevelsError> {
    if host {
        adopt_runtime_spawns(world, level);
    }
    let mut snapshot =
        crate::seq_tracks::with_preview_lifted(world, |world| snapshot_saved_level(world, level))
            .map_err(|e| LevelsError::new("level_invalid", e.to_string()))?;
    keep_layer_binding(world, level, &mut snapshot.scene);
    snapshot
        .to_ron_string()
        .map_err(|e| LevelsError::new("level_invalid", e.to_string()))
}

/// Saves one open document: a level writes only its own file (not its layer files), a layer
/// file writes only itself. The bytes of every other file stay as they are. Returns the files
/// written (none when the bytes were already the same).
///
/// # Errors
/// `prefab_in_place` / `prefab_open` while a prefab is open, `document_unknown`,
/// `level_untitled`, or a write error (`level_io_error`).
pub fn save_level_document(world: &mut World, document: u128) -> Result<Vec<PathBuf>, LevelsError> {
    // A prefab open (in place or on its own): the world holds the session, not the level.
    if let Some((reason, refusal)) = crate::prefab_edit::level_save_refusal_reason(world) {
        return Err(LevelsError::new(reason, refusal));
    }
    crate::level_dirty::feed_documents(world);
    let doc = documents(world)
        .into_iter()
        .find(|doc| doc.id == document)
        .ok_or_else(|| {
            LevelsError::new(
                "document_unknown",
                format!("{} is not open", Guid::from_u128(document)),
            )
        })?;
    let path = doc
        .path
        .clone()
        .ok_or_else(|| LevelsError::new("level_untitled", "save the host level with File ▸ Save As first"))?;
    let separated = world
        .get_resource::<OpenLevels>()
        .and_then(|model| model.separated.get(&doc.level).cloned())
        .unwrap_or_default();
    let host = doc.kind == DocumentKind::Host;
    let text = level_text_for_save(world, doc.level, host)?;
    let written = if is_level_dir(&path) {
        let scene = regia_scene::prelude::SceneFile::from_ron_str(&text)
            .map_err(|e| LevelsError::new("level_invalid", e.to_string()))?;
        let (rest, _) = regia_scene::layer_file::split_layers(&scene, &separated);
        regia_scene::level_dir::write_level_dir(&path, &rest)
            .map_err(|e| LevelsError::new(e.reason(), e.to_string()))?
            .written
    } else {
        let target = match &doc.kind {
            DocumentKind::Layer(layer) => regia_scene::layer_file::SplitTarget::Layer(layer.clone()),
            _ => regia_scene::layer_file::SplitTarget::Level,
        };
        regia_scene::layer_file::write_split_level(&path, &text, &separated, &target, false)
            .map_err(|e| LevelsError::new(e.reason(), e.to_string()))?
            .written
    };
    let mut stack = world
        .remove_resource::<crate::command::UndoStack>()
        .unwrap_or_default();
    stack.mark_document_saved(document);
    world.insert_resource(stack);
    let all_clean = {
        let docs: Vec<u128> = documents(world).into_iter().map(|doc| doc.id).collect();
        world
            .get_resource::<crate::command::UndoStack>()
            .is_some_and(|stack| docs.iter().all(|doc| !stack.is_document_dirty(*doc)))
    };
    if all_clean && let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.mark_saved();
    }
    crate::level_dirty::resync_documents(world);
    Ok(written)
}

/// Moves `layer` of the host level into its own file (`separate`) or merges its file back into
/// the level. Works on the files on disk: unsaved edits stay in the world and a later save puts
/// them in the right file. Merging back an unchanged split gives the level's original bytes.
///
/// # Errors
/// `no_host_level`, `level_untitled`, `layer_file_needs_file_form`, `layer_unknown`,
/// `layer_not_separated`, or a reader or writer reason.
pub fn set_layer_file(
    world: &mut World,
    layer: &str,
    separate: bool,
) -> Result<regia_scene::layer_file::LayerFilesWritten, LevelsError> {
    sync_open_levels(world);
    let open = world
        .get_resource::<OpenLevel>()
        .cloned()
        .ok_or_else(|| LevelsError::new("no_host_level", "open a level first"))?;
    let path = open.path.clone().ok_or_else(|| {
        LevelsError::new(
            "level_untitled",
            "save the level before moving a layer to its own file",
        )
    })?;
    if is_level_dir(&path) {
        return Err(LevelsError::new(
            "layer_file_needs_file_form",
            "a level directory already stores one file per entity",
        ));
    }
    let mut separated = world
        .get_resource::<OpenLevels>()
        .and_then(|model| model.separated.get(&open.id).cloned())
        .unwrap_or_default();
    if separate {
        let known = world
            .get_resource::<regia_scene::layers::LayerTable>()
            .is_some_and(|table| table.contains(layer));
        if !known || !regia_scene::layers::valid_layer_name(layer) {
            return Err(LevelsError::new(
                "layer_unknown",
                format!("`{layer}` is not a layer of this level"),
            ));
        }
        separated.insert(layer.to_owned());
    } else if !separated.remove(layer) {
        return Err(LevelsError::new(
            "layer_not_separated",
            format!("`{layer}` is not stored in its own file"),
        ));
    }
    let whole = regia_scene::level_dir::read_level_as_stored(&path)
        .map_err(|e| LevelsError::new(e.reason(), e.to_string()))?;
    let text = whole
        .to_ron_string()
        .map_err(|e| LevelsError::new("level_invalid", e.to_string()))?;
    let written = regia_scene::layer_file::write_split_level(
        &path,
        &text,
        &separated,
        &regia_scene::layer_file::SplitTarget::All,
        !separate,
    )
    .map_err(|e| LevelsError::new(e.reason(), e.to_string()))?;
    world
        .get_resource_or_insert_with(OpenLevels::default)
        .separated
        .insert(open.id, separated);
    crate::level_dirty::resync_documents(world);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit G editor4-platform L3 (sprint-fixrev-08-k): a read-only `.level.ron`
    /// destination (Perforce-style sync) refuses the replacing rename on Windows; the save
    /// succeeds after the shared helper clears the attribute once. Runs on Windows:
    /// `cargo test -p regia_editor --lib level_io`.
    #[cfg(windows)]
    #[test]
    fn write_atomic_replaces_read_only_destination() {
        let dir = regia_test_utils::unique_temp_dir("level_io_readonly_dest");
        let path = dir.join("arena.level.ron");
        write_atomic(&path, b"first").expect("first write");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("mark read-only");
        write_atomic(&path, b"second").expect("rewrite over read-only");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
    }

    #[test]
    fn untitled_level_path_never_overwrites_an_earlier_save() {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-untitled-{n}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(free_untitled_level_path(&dir), dir.join("unsaved.level.ron"));
        std::fs::write(dir.join("unsaved.level.ron"), "marker").expect("marker");
        assert_eq!(free_untitled_level_path(&dir), dir.join("unsaved-2.level.ron"));
        std::fs::write(dir.join("unsaved-2.level.ron"), "marker").expect("marker");
        assert_eq!(free_untitled_level_path(&dir), dir.join("unsaved-3.level.ron"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn app_with_scene() -> (regia_app::App, PathBuf) {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        // A level with one entity, loaded Replace.
        let level_text = r#"
        (
            format_version: 1,
            entities: [
                (guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Origin",
                 components: { "regia_scene::transform::Transform": () }),
            ],
        )
        "#;
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, level_text).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        // Unique per call: tests run on parallel threads and must not share (and delete) one dir.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-level-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        (app, dir)
    }

    #[test]
    fn reset_edit_state_drops_history_and_selection() {
        use crate::command::{EditorCommand, UndoStack};
        struct Noop;
        impl EditorCommand for Noop {
            fn apply(&mut self, _world: &mut World) {}
            fn revert(&mut self, _world: &mut World) {}
            fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
                false
            }
            fn description(&self) -> String {
                "noop".to_string()
            }
        }
        let (mut app, _dir) = app_with_scene();
        let entity = app.world_mut().spawn_empty().id();
        let mut selection = crate::selection::Selection::default();
        selection.select(entity, crate::selection::SelectMode::Replace);
        app.world_mut().insert_resource(selection);
        let mut undo = UndoStack::new();
        undo.push(Box::new(Noop));
        app.world_mut().insert_resource(undo);

        reset_edit_state(app.world_mut());

        let undo = app.world().resource::<UndoStack>();
        assert_eq!(
            (undo.undo_depth(), undo.redo_depth(), undo.is_dirty()),
            (0, 0, false)
        );
        assert!(app.world().resource::<crate::selection::Selection>().is_empty());
    }

    #[test]
    fn save_and_open_round_trip() {
        let (mut app, dir) = app_with_scene();
        let open = OpenLevel {
            id: app.world().resource::<LevelManager>().current().expect("level"),
            path: None,
        };
        let path = dir.join("test.level.ron");
        save_level_file(app.world_mut(), &open, &path).expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("Origin"), "entity serialized: {text}");
        assert!(text.contains("LevelSettings"), "settings serialized");

        // Edit in memory, save again, reopen in a fresh world: both entities present.
        use regia_scene::prelude::Transform;
        app.world_mut().spawn((
            regia_scene::prelude::Name::new("Added"),
            Transform::default(),
            regia_scene::prelude::LevelMember::from_level(open.id),
            regia_scene::prelude::EntityGuid(Guid::new_random()),
        ));
        save_level_file(app.world_mut(), &open, &path).expect("save 2");

        let mut other = regia_app::App::new();
        other.add_plugins(regia_scene::ScenePlugin);
        let reopened = open_level_file(other.world_mut(), &path).expect("reopen");
        let names: Vec<String> = other
            .world_mut()
            .query::<&regia_scene::prelude::Name>()
            .iter(other.world())
            .map(|n| n.as_str().to_string())
            .collect();
        assert!(names.contains(&"Origin".to_string()), "{names:?}");
        assert!(
            names.contains(&"Added".to_string()),
            "the in-editor edit saved: {names:?}"
        );
        assert!(reopened.path.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_level_clears_the_world() {
        let (mut app, dir) = app_with_scene();
        let old = app.world().resource::<LevelManager>().current();
        let open = new_level(app.world_mut());
        let names: Vec<String> = app
            .world_mut()
            .query::<&regia_scene::prelude::Name>()
            .iter(app.world())
            .map(|n| n.as_str().to_string())
            .collect();
        assert!(
            !names.contains(&"Origin".to_string()),
            "replace cleared: {names:?}"
        );
        let current = app.world().resource::<LevelManager>().current();
        assert_ne!(current, old, "the old level is no longer current");
        assert_eq!(current, Some(open.id), "the new level is current");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
