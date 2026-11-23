// SPDX-License-Identifier: MPL-2.0
//! Prefab variants in the editor (`docs/specs/83-prefab-profiler-volumes.md` § 2.4, ADR-2323):
//! create a `.variant.ron` (empty, or from a live instance's overrides), register a prefab or
//! variant file with its whole base chain, edit one override of a variant, and the agent methods
//! `editor.create_prefab_variant`, `editor.prefab_variant` and `editor.set_variant_override`
//! (`book/agents/prefab-variants.md`).
//!
//! Error reasons (`data.reason`): `no_project`, `bad_path`, `base_missing`, `not_a_prefab`,
//! `variant_exists`, `variant_missing`, `bad_variant`, `entity_missing`, `not_an_instance`,
//! `instance_of_other_prefab`, `bad_value`, `chain_broken`, `io_failed`,
//! `instance_update_failed`.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_scene::prefab::{PrefabInstance, PrefabTemplate};
use regia_scene::prefab_variant::{
    MAX_VARIANT_CHAIN, PREFAB_VARIANT_SUFFIX, PrefabVariant, PrefabVariantFile,
};
use regia_scene::prelude::{PrefabLibrary, PrefabOverride, SceneFile};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The `.meta` kind of a variant file.
pub const VARIANT_META_KIND: &str = "prefab_variant_ron";

/// Deepest directory walk when looking a prefab up by id.
const MAX_SEARCH_DEPTH: usize = 24;

/// A failed variant operation: a stable reason (the agent's `data.reason`) and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantEditError {
    /// Stable lowercase reason (listed in the module docs and the agent page).
    pub reason: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl VariantEditError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for VariantEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

/// Whether `path` names a prefab variant file.
#[must_use]
pub fn is_variant_path(path: &Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(PREFAB_VARIANT_SUFFIX)
}

fn is_prefab_path(path: &Path) -> bool {
    path.to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".prefab.ron")
}

/// The stable asset id of a prefab or variant file (its `.meta`, created on first use with the
/// matching kind).
///
/// # Errors
///
/// The meta cannot be read, parsed or written.
pub fn asset_id_of(path: &Path) -> Result<Guid, VariantEditError> {
    if is_variant_path(path) && !regia_asset::meta_path_for(path).is_file() {
        let meta = regia_asset::MetaSidecar::new(VARIANT_META_KIND, regia_asset::AssetId::new_random());
        let text = meta
            .to_ron_string()
            .map_err(|e| VariantEditError::new("io_failed", format!("meta serialize: {e}")))?;
        write_atomic(&regia_asset::meta_path_for(path), text.as_bytes())?;
        return Ok(meta.id.0);
    }
    crate::content::prefab_asset_id(path)
        .map(|id| id.0)
        .map_err(|e| VariantEditError::new("io_failed", e))
}

/// Writes through the editor's shared atomic write (unique temp name, Windows sharing-lock
/// retries and read-only clearing, the temp file removed on failure; ADR-7412).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), VariantEditError> {
    crate::atomic_write::write_atomic(path, bytes).map_err(|e| VariantEditError::new("io_failed", e))
}

/// Reads a `.variant.ron`.
///
/// # Errors
///
/// `variant_missing` when the file cannot be read, `bad_variant` when it does not parse.
pub fn read_variant_file(path: &Path) -> Result<PrefabVariantFile, VariantEditError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        VariantEditError::new("variant_missing", format!("cannot read {}: {e}", path.display()))
    })?;
    PrefabVariantFile::from_ron_str(&text).map_err(|e| VariantEditError::new("bad_variant", e.to_string()))
}

/// The prefab or variant file under `<project>/assets` whose `.meta` id is `id` (a bounded,
/// sorted walk, so the answer is stable when two files claim the same id).
#[must_use]
pub fn find_prefab_by_id(project_root: &Path, id: Guid) -> Option<PathBuf> {
    let mut stack = vec![(project_root.join("assets"), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths.iter().rev() {
            if path.is_dir() {
                let hidden = path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'));
                if !hidden && depth < MAX_SEARCH_DEPTH {
                    stack.push((path.clone(), depth + 1));
                }
            }
        }
        for path in paths {
            if !(is_prefab_path(&path) || is_variant_path(&path)) {
                continue;
            }
            let meta = regia_asset::meta_path_for(&path);
            let Ok(bytes) = std::fs::read(&meta) else {
                continue;
            };
            if regia_asset::MetaSidecar::from_ron(&bytes).is_ok_and(|meta| meta.id.0 == id) {
                return Some(path);
            }
        }
    }
    None
}

/// Registers a `.prefab.ron` (baked) or `.variant.ron` in the world's [`PrefabLibrary`] under its
/// `.meta` id. A variant's base chain is registered too, found under the project by id when not
/// already in the library. Returns the id.
///
/// # Errors
///
/// `not_a_prefab`, `bad_variant`, `variant_missing`, `chain_broken` (a base is not in the library
/// and not in the project, the chain loops, or it is longer than the limit), `io_failed`.
pub fn register_prefab_file(world: &mut World, path: &Path) -> Result<Guid, VariantEditError> {
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    let asset = register_with_chain(world, project.as_deref(), path, 0)?;
    // Every step is registered now, so a loop or an over-long chain shows here, not at spawn.
    if let Some(library) = world.get_resource::<PrefabLibrary>()
        && let Err(err) = library.resolve(asset)
    {
        return Err(VariantEditError::new("chain_broken", err.to_string()));
    }
    Ok(asset)
}

fn register_with_chain(
    world: &mut World,
    project: Option<&Path>,
    path: &Path,
    depth: usize,
) -> Result<Guid, VariantEditError> {
    if depth > MAX_VARIANT_CHAIN {
        return Err(VariantEditError::new(
            "chain_broken",
            format!(
                "variant chain longer than {MAX_VARIANT_CHAIN} at {}",
                path.display()
            ),
        ));
    }
    let asset = asset_id_of(path)?;
    if is_variant_path(path) {
        let file = read_variant_file(path)?;
        let variant = PrefabVariant::from_file(asset, &file)
            .map_err(|e| VariantEditError::new("bad_variant", e.to_string()))?;
        let base = variant.base;
        world
            .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
            .insert_variant(variant);
        let loaded = world
            .get_resource::<PrefabLibrary>()
            .is_some_and(|library| library.contains(base));
        if !loaded {
            let base_path = project
                .and_then(|root| find_prefab_by_id(root, base))
                .ok_or_else(|| {
                    VariantEditError::new(
                        "chain_broken",
                        format!(
                            "the base {base} of {} is not loaded and not in the project",
                            path.display()
                        ),
                    )
                })?;
            register_with_chain(world, project, &base_path, depth + 1)?;
        }
        return Ok(asset);
    }
    if !is_prefab_path(path) {
        return Err(VariantEditError::new(
            "not_a_prefab",
            format!(
                "{} is not a .prefab.ron or {PREFAB_VARIANT_SUFFIX}",
                path.display()
            ),
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| VariantEditError::new("not_a_prefab", format!("cannot read {}: {e}", path.display())))?;
    let file =
        SceneFile::from_ron_str(&text).map_err(|e| VariantEditError::new("not_a_prefab", e.to_string()))?;
    let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read())
        .map_err(|e| VariantEditError::new("not_a_prefab", format!("prefab bake: {e}")))?;
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);
    Ok(asset)
}

/// Creates `variant` (a new `.variant.ron` and its `.meta`) on `base` (a prefab or a variant).
/// With `from_instance`, the variant's edits are that live instance's overrides and removed
/// children (the instance must instantiate `base`). Registers the variant; returns its id.
///
/// # Errors
///
/// `bad_path`, `base_missing`, `not_a_prefab`, `variant_exists`, `entity_missing`,
/// `not_an_instance`, `instance_of_other_prefab`, `chain_broken`, `io_failed`.
pub fn create_prefab_variant(
    world: &mut World,
    base: &Path,
    variant: &Path,
    from_instance: Option<Entity>,
) -> Result<Guid, VariantEditError> {
    if !is_variant_path(variant) {
        return Err(VariantEditError::new(
            "bad_path",
            format!("a variant file name ends in {PREFAB_VARIANT_SUFFIX}"),
        ));
    }
    if !(is_prefab_path(base) || is_variant_path(base)) {
        return Err(VariantEditError::new(
            "not_a_prefab",
            format!(
                "{} is not a .prefab.ron or {PREFAB_VARIANT_SUFFIX}",
                base.display()
            ),
        ));
    }
    if !base.is_file() {
        return Err(VariantEditError::new(
            "base_missing",
            format!("{} does not exist", base.display()),
        ));
    }
    if variant.exists() {
        return Err(VariantEditError::new(
            "variant_exists",
            format!("{} already exists", variant.display()),
        ));
    }
    let base_id = register_prefab_file(world, base)?;
    let file = match from_instance {
        None => PrefabVariantFile::new(base_id),
        Some(entity) => {
            if world.get_entity(entity).is_err() {
                return Err(VariantEditError::new(
                    "entity_missing",
                    format!("no entity {entity:?}"),
                ));
            }
            let instance = world.get::<PrefabInstance>(entity).ok_or_else(|| {
                VariantEditError::new(
                    "not_an_instance",
                    format!("{entity:?} is not a prefab instance root"),
                )
            })?;
            if instance.prefab_guid() != base_id {
                return Err(VariantEditError::new(
                    "instance_of_other_prefab",
                    format!(
                        "{entity:?} instantiates {}, not the base {base_id}",
                        instance.prefab_guid()
                    ),
                ));
            }
            let mut data = instance.to_data();
            data.prefab = regia_scene::format::SceneAssetRef(base_id);
            PrefabVariantFile::from_instance(&data)
        }
    };
    if let Some(parent) = variant.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            VariantEditError::new("io_failed", format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    let text = file
        .to_ron_string()
        .map_err(|e| VariantEditError::new("io_failed", e.to_string()))?;
    write_atomic(variant, text.as_bytes())?;
    let asset = register_prefab_file(world, variant)?;
    // One undo step (ADR-7614): the undo removes the new file and its `.meta`, a redo writes them
    // back with the same id.
    let meta = std::fs::read(regia_asset::meta_path_for(variant)).unwrap_or_default();
    if let Some(mut history) = world.get_resource_mut::<crate::command::UndoStack>() {
        history.push(Box::new(VariantFileCreate {
            path: variant.to_path_buf(),
            bytes: text.into_bytes(),
            meta,
        }));
    }
    Ok(asset)
}

/// The undo entry of a created variant file: exact bytes of the file and its `.meta`.
struct VariantFileCreate {
    path: PathBuf,
    bytes: Vec<u8>,
    meta: Vec<u8>,
}

impl crate::command::EditorCommand for VariantFileCreate {
    fn apply(&mut self, world: &mut World) {
        let meta = regia_asset::meta_path_for(&self.path);
        let written = write_atomic(&meta, &self.meta)
            .and_then(|()| write_atomic(&self.path, &self.bytes))
            .and_then(|()| register_prefab_file(world, &self.path));
        if let Err(error) = written {
            tracing::error!(target: "regia_editor", path = %self.path.display(), "redo create variant: {error}");
        }
    }

    fn revert(&mut self, _world: &mut World) {
        for path in [self.path.clone(), regia_asset::meta_path_for(&self.path)] {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::error!(target: "regia_editor", path = %path.display(), "undo create variant: {error}");
            }
        }
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    /// A file-only step: creating (or undoing) a variant changes no level document, so a saved
    /// level stays clean (rev-claude-64 (3) finding 4).
    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Create prefab variant {}", self.path.display())
    }
}

/// The egui memory id of the Create variant dialog.
const VARIANT_DIALOG: &str = "create-prefab-variant-dialog";

/// The Create variant dialog's state (Outliner *Create variant…* on an instance, Content Browser
/// *Create variant…* on a prefab or variant file).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateVariantDialog {
    /// The base, relative to `assets/`.
    pub base: String,
    /// The new `.variant.ron`, relative to `assets/`.
    pub path: String,
    /// The instance whose overrides the variant keeps (`None`: an empty variant).
    pub from: Option<Entity>,
    /// The last refusal.
    pub error: Option<String>,
    /// The egui pass the dialog was last drawn in (it is drawn once per pass whichever panel
    /// asks first).
    drawn_pass: u64,
}

/// The default new variant path next to `base` (`lamp.prefab.ron` → `lamp_variant.variant.ron`).
#[must_use]
pub fn default_variant_path(base: &str) -> String {
    let lower = base.to_ascii_lowercase();
    let stem = [".prefab.ron", PREFAB_VARIANT_SUFFIX]
        .iter()
        .find(|suffix| lower.ends_with(*suffix))
        .map_or(base, |suffix| &base[..base.len() - suffix.len()]);
    format!("{stem}_variant{PREFAB_VARIANT_SUFFIX}")
}

/// Opens the Create variant dialog for `base` (relative to `assets/`).
pub fn open_variant_dialog(ctx: &egui::Context, base: &str, from: Option<Entity>) {
    let dialog = CreateVariantDialog {
        base: base.to_owned(),
        path: default_variant_path(base),
        from,
        error: None,
        drawn_pass: u64::MAX,
    };
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(VARIANT_DIALOG), dialog));
}

/// A request to open the Create variant dialog that has no egui context at hand (the palette's
/// `prefab.create_variant`, ADR-7744): [`variant_dialog`] opens the dialog on the next draw, from
/// the shell (which draws it with every panel closed) or from a panel.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct VariantDialogRequest {
    /// The base, relative to `assets/`, and the instance whose overrides the variant keeps.
    pub pending: Option<(String, Option<Entity>)>,
}

/// Asks for the Create variant dialog for the selected prefab instance (the palette command
/// `prefab.create_variant`); answers the status line. A selection that is not exactly one prefab
/// instance of a file under the project opens nothing.
pub fn request_dialog_for_selection(world: &mut World) -> String {
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .map(|selection| selection.entities().to_vec())
        .unwrap_or_default();
    let target = match selected.as_slice() {
        [entity] => instance_base_path(world, *entity).map(|base| (base, *entity)),
        _ => None,
    };
    match target {
        Some((base, entity)) => {
            world.insert_resource(VariantDialogRequest {
                pending: Some((base.clone(), Some(entity))),
            });
            format!("Create variant of assets/{base}")
        }
        None => {
            "Select one prefab instance first (or use Content ▸ Create variant… on a prefab file)".to_owned()
        }
    }
}

/// Whether the Create variant dialog is open (the dialog state; `variant_dialog` draws it).
#[must_use]
pub fn dialog_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<CreateVariantDialog>(egui::Id::new(VARIANT_DIALOG)))
        .is_some()
}

/// The open Create variant dialog's base and file, relative to `assets/` (`None` when closed).
#[must_use]
pub fn dialog_target(ctx: &egui::Context) -> Option<(String, String)> {
    ctx.data(|d| d.get_temp::<CreateVariantDialog>(egui::Id::new(VARIANT_DIALOG)))
        .map(|dialog| (dialog.base, dialog.path))
}

/// The base file of the instance `entity` relative to `assets/` (its prefab or variant, found
/// by id under the project): what the Outliner's *Create variant…* bases the variant on.
#[must_use]
pub fn instance_base_path(world: &World, entity: Entity) -> Option<String> {
    let asset = world.get::<PrefabInstance>(entity)?.prefab_guid();
    let root = crate::mesh_source::project_root(world)?;
    let path = find_prefab_by_id(&root, asset)?;
    let assets = root.join("assets");
    Some(path.strip_prefix(&assets).unwrap_or(&path).display().to_string())
}

/// Creates the variant the dialog names through `editor.create_prefab_variant`'s code (paths
/// confined to the project like the agent method's).
///
/// # Errors
/// As [`create_prefab_variant`], plus `bad_path` and `no_project`.
pub fn create_variant_from_dialog(
    world: &mut World,
    dialog: &CreateVariantDialog,
) -> Result<Guid, VariantEditError> {
    let params = json!({ "base": dialog.base, "path": dialog.path });
    let base = project_path(world, &params, "base")?;
    let variant = project_path(world, &params, "path")?;
    create_prefab_variant(world, &base, &variant, dialog.from)
}

/// Draws the Create variant dialog while it is open (once per egui pass, whichever panel calls
/// first).
pub fn variant_dialog(ctx: &egui::Context, world: &mut World) {
    // A request from the palette opens the dialog here (ADR-7744).
    if let Some((base, from)) = world
        .get_resource_mut::<VariantDialogRequest>()
        .and_then(|mut request| request.pending.take())
    {
        open_variant_dialog(ctx, &base, from);
    }
    let id = egui::Id::new(VARIANT_DIALOG);
    let Some(mut dialog) = ctx.data(|d| d.get_temp::<CreateVariantDialog>(id)) else {
        return;
    };
    let pass = ctx.cumulative_pass_nr();
    if dialog.drawn_pass == pass {
        return;
    }
    dialog.drawn_pass = pass;
    let mut close = false;
    egui::Window::new("Create prefab variant")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(format!("Base: assets/{}", dialog.base));
            ui.label(if dialog.from.is_some() {
                "The variant keeps the selected instance's overrides."
            } else {
                "An empty variant: edit its overrides afterwards."
            });
            ui.horizontal(|ui| {
                let label = ui.label("File assets/");
                ui.text_edit_singleline(&mut dialog.path).labelled_by(label.id);
            });
            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(ui, &crate::agent_parity::VARIANT_CREATE_CONFIRM)
                    .clicked()
                {
                    match create_variant_from_dialog(world, &dialog) {
                        Ok(_) => close = true,
                        Err(err) => dialog.error = Some(err.to_string()),
                    }
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::VARIANT_CREATE_CANCEL)
                    .clicked()
                {
                    close = true;
                }
            });
        });
    ctx.data_mut(|d| {
        if close {
            d.remove::<CreateVariantDialog>(id);
        } else {
            d.insert_temp(id, dialog);
        }
    });
}

/// Sets (or, with `value` `None`, clears) one override of a variant: `target` local id,
/// `component` type path, field `path` (`"."` or `""` for the whole component), `value` RON.
/// Writes the file, re-registers the variant and re-instantiates the live instances of it and of
/// every variant built on it. Records one undo entry when the editor history is installed.
/// Returns how many instances updated.
///
/// # Errors
///
/// `variant_missing`, `bad_variant`, `bad_value` (not RON, nil target, empty component),
/// `io_failed`, `chain_broken`, `instance_update_failed` (the live instances cannot spawn with
/// the edit: the file goes back to what it was and the instances keep the old variant).
pub fn set_variant_override(
    world: &mut World,
    variant: &Path,
    target: Guid,
    component: &str,
    path: &str,
    value: Option<&str>,
) -> Result<usize, VariantEditError> {
    if target.is_nil() || component.trim().is_empty() {
        return Err(VariantEditError::new(
            "bad_value",
            "target and component are required",
        ));
    }
    let before = std::fs::read(variant).map_err(|e| {
        VariantEditError::new(
            "variant_missing",
            format!("cannot read {}: {e}", variant.display()),
        )
    })?;
    let mut file = read_variant_file(variant)?;
    let path = if path.is_empty() { "." } else { path };
    file.base
        .overrides
        .retain(|entry| !(entry.target == target && entry.component == component && entry.path == path));
    if let Some(value) = value {
        let raw = ron::value::RawValue::from_ron(value)
            .map_err(|e| VariantEditError::new("bad_value", format!("value is not RON: {e}")))?;
        file.base.overrides.push(PrefabOverride {
            target,
            component: component.to_string(),
            path: path.to_string(),
            value: raw.to_owned(),
        });
    }
    let text = file
        .to_ron_string()
        .map_err(|e| VariantEditError::new("io_failed", e.to_string()))?;
    write_atomic(variant, text.as_bytes())?;
    let library = crate::prefab_edit::library_checkpoint(world);
    let asset = register_prefab_file(world, variant)?;
    let updated = match respawn_with_dependents(world, asset, &library) {
        Ok(updated) => updated,
        Err(message) => {
            // The library is back to `library`; put the file back to match it.
            write_atomic(variant, &before)?;
            return Err(VariantEditError::new("instance_update_failed", message));
        }
    };
    if before != text.as_bytes()
        && let Some(mut history) = world.get_resource_mut::<crate::command::UndoStack>()
    {
        history.push(Box::new(VariantFileEdit {
            path: variant.to_path_buf(),
            before,
            after: text.into_bytes(),
        }));
    }
    Ok(updated)
}

/// Exact file snapshots keep formatting and every independent override intact across history.
struct VariantFileEdit {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl VariantFileEdit {
    fn restore(&self, world: &mut World, bytes: &[u8]) {
        let library = crate::prefab_edit::library_checkpoint(world);
        let result = write_atomic(&self.path, bytes).and_then(|()| register_prefab_file(world, &self.path));
        match result {
            Ok(asset) => {
                if let Err(error) = respawn_with_dependents(world, asset, &library) {
                    tracing::error!(target: "regia_editor", path = %self.path.display(),
                        "variant history: {error}");
                }
            }
            Err(error) => tracing::error!(target: "regia_editor", path = %self.path.display(),
                "variant history: {error}"),
        }
    }
}

impl crate::command::EditorCommand for VariantFileEdit {
    fn apply(&mut self, world: &mut World) {
        self.restore(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        self.restore(world, &self.before);
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Edit prefab variant {}", self.path.display())
    }
}

/// Re-instantiates the live instances of `asset` and of every registered variant built on it, in
/// one pass: a level-authored root holding instances of both a base and its variant is rebuilt
/// and counted once. `before` is the library before the change (see
/// `prefab_edit::respawn_instances`, which also explains the rollback on failure).
pub(crate) fn respawn_with_dependents(
    world: &mut World,
    asset: Guid,
    before: &PrefabLibrary,
) -> Result<usize, String> {
    let mut assets = vec![asset];
    if let Some(library) = world.get_resource::<PrefabLibrary>() {
        assets.extend(library.dependent_variants(asset));
    }
    crate::prefab_edit::respawn_instances(world, &assets, before)
}

/// A summary of a variant file for `editor.prefab_variant`.
///
/// # Errors
///
/// As [`register_prefab_file`].
pub fn describe_variant(world: &mut World, path: &Path) -> Result<Value, VariantEditError> {
    let file = read_variant_file(path)?;
    let asset = register_prefab_file(world, path)?;
    let chain: Vec<String> = world
        .get_resource::<PrefabLibrary>()
        .map(|library| library.variant_chain(asset))
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect();
    let edits = &file.base;
    Ok(json!({
        "asset": asset.to_string(),
        "base": file.base_guid().to_string(),
        "format_version": file.format_version,
        "chain": chain,
        "overrides": edits.overrides.iter().map(|o| json!({
            "target": o.target.to_string(),
            "component": o.component,
            "path": o.path,
            "value": o.value.get_ron(),
        })).collect::<Vec<_>>(),
        "added_components": edits.added_components.len(),
        "removed_components": edits.removed_components.len(),
        "added_children": edits.added_children.len(),
        // Spec 83 § 2.4: the agent answer is edit counts, the same shape as the other three.
        "removed_children": edits.removed_children.len(),
    }))
}

// ---------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------

fn rpc(err: VariantEditError) -> RpcError {
    let (code, regia_code, hint) = match err.reason {
        "no_project" | "base_missing" | "variant_missing" | "entity_missing" | "chain_broken" => (
            protocol::NOT_FOUND,
            "F0003",
            "check the path with editor.list_assets, the entity with scene.query_entities",
        ),
        "io_failed" => (protocol::INTERNAL_ERROR, "F0001", "see logs.read for details"),
        _ => (
            protocol::INVALID_PARAMS,
            "F0002",
            "see book/agents/prefab-variants.md for the parameters",
        ),
    };
    RpcError {
        code,
        message: err.message,
        data: Some(json!({ "code": regia_code, "reason": err.reason, "hint": hint })),
    }
}

fn project_path(world: &World, params: &Value, key: &str) -> Result<PathBuf, VariantEditError> {
    let rel = params
        .get(key)
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            VariantEditError::new("bad_path", format!("`{key}` (project-relative path) is required"))
        })?;
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| VariantEditError::new("no_project", "no project is open"))?;
    let rel = rel.replace('\\', "/");
    if rel.split('/').any(|part| part == "..") || Path::new(&rel).is_absolute() {
        return Err(VariantEditError::new(
            "bad_path",
            format!("`{key}` must stay inside the project"),
        ));
    }
    let rel = rel.strip_prefix("assets/").map(str::to_string).unwrap_or(rel);
    Ok(root.join("assets").join(rel))
}

fn editor_create_prefab_variant(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let base = project_path(world, params, "base").map_err(rpc)?;
    let variant = project_path(world, params, "path").map_err(rpc)?;
    let from = match params.get("from_entity") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(crate::command::entity_from_bits)
                .ok_or_else(|| {
                    rpc(VariantEditError::new(
                        "entity_missing",
                        "`from_entity` is not an entity id",
                    ))
                })?,
        ),
    };
    let asset = create_prefab_variant(world, &base, &variant, from).map_err(rpc)?;
    let overrides = read_variant_file(&variant)
        .map(|f| f.base.overrides.len())
        .unwrap_or(0);
    Ok(json!({ "path": params["path"], "asset": asset.to_string(), "overrides": overrides }))
}

fn editor_prefab_variant(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = project_path(world, params, "path").map_err(rpc)?;
    if !is_variant_path(&path) {
        return Err(rpc(VariantEditError::new(
            "bad_path",
            format!("a variant file name ends in {PREFAB_VARIANT_SUFFIX}"),
        )));
    }
    describe_variant(world, &path).map_err(rpc)
}

fn editor_set_variant_override(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = project_path(world, params, "path").map_err(rpc)?;
    let target = params
        .get("target")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Guid>().ok())
        .ok_or_else(|| {
            rpc(VariantEditError::new(
                "bad_value",
                "`target` (a local id guid) is required",
            ))
        })?;
    let component = params.get("component").and_then(Value::as_str).ok_or_else(|| {
        rpc(VariantEditError::new(
            "bad_value",
            "`component` (a type path) is required",
        ))
    })?;
    let field = params.get("field").and_then(Value::as_str).unwrap_or(".");
    let value = match params.get("value") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    };
    let updated =
        set_variant_override(world, &path, target, component, field, value.as_deref()).map_err(rpc)?;
    let overrides = read_variant_file(&path)
        .map(|f| f.base.overrides.len())
        .unwrap_or(0);
    Ok(json!({ "path": params["path"], "overrides": overrides, "instances_updated": updated }))
}

/// The variant methods (`editor.create_prefab_variant`, `editor.prefab_variant`,
/// `editor.set_variant_override`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.create_prefab_variant",
                tool: "create_prefab_variant",
                description: "Create a prefab variant (.variant.ron): a base prefab (or variant) plus edits. \
                    Empty, or with from_entity the overrides and removed children of a live instance of the \
                    base (save an instance as a variant). Registers it; editor.spawn_prefab spawns it.",
                params: &[
                    ParamSchema {
                        name: "base",
                        kind: "string",
                        required: true,
                        description: "Project-relative base, e.g. assets/lamp.prefab.ron (or a .variant.ron).",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative new file ending in .variant.ron.",
                    },
                    ParamSchema {
                        name: "from_entity",
                        kind: "integer",
                        required: false,
                        description: "Entity bits of an instance root of the base whose overrides to keep.",
                    },
                ],
                result: "{ path, asset, overrides }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_create_prefab_variant as Handler,
        ),
        (
            MethodSchema {
                name: "editor.prefab_variant",
                tool: "prefab_variant",
                description: "Read a prefab variant: its id, base, the chain down to the base prefab, its \
                    overrides and edit counts. Registers it (and its chain) if needed.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative .variant.ron.",
                }],
                result: "{ asset, base, format_version, chain, overrides: [{target, component, path, value}], \
                    added_components, removed_components, added_children, removed_children } \
                    (the four edit fields are counts)",
                mutating: false,
                permission: Permission::Read,
            },
            editor_prefab_variant as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_variant_override",
                tool: "set_variant_override",
                description: "Set or clear one override of a prefab variant, write the file and update every \
                    live instance of it and of the variants built on it.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative .variant.ron.",
                    },
                    ParamSchema {
                        name: "target",
                        kind: "string",
                        required: true,
                        description: "Local id (guid) of the base's entity to edit.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "Component type path.",
                    },
                    ParamSchema {
                        name: "field",
                        kind: "string",
                        required: false,
                        description: "Reflect path inside the component (\".intensity\"); default \".\" (whole).",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "string",
                        required: false,
                        description: "RON value (a JSON number or bool is used as is); null or omitted clears.",
                    },
                ],
                result: "{ path, overrides, instances_updated }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_set_variant_override as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
