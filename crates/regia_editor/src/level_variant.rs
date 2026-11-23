// SPDX-License-Identifier: MPL-2.0
//! Level variants in the editor (ADR-7823, `book/agents/level-variants.md`): File ▸ New level
//! variant…, open and edit a `.levelvariant.ron` like any level (the framework loader resolves
//! its chain), save only the differences from the base, override marks in the Outliner and the
//! Inspector, and Revert to base per entity, component or field.
//!
//! While a variant is open, a [`LevelVariantSession`] holds its resolved base in the form a save
//! writes (every component decoded and re-encoded through reflection, so a value the base file
//! spells differently is not a difference). A save diffs the level's snapshot against it
//! ([`regia_scene::level_variant::diff_levels`]); the marks read a copy of that diff refreshed
//! after edits.
//!
//! Agent methods: `editor.new_level_variant`, `editor.level_variant_state`,
//! `editor.level_variant_revert`. Error reasons (`data.reason`): `no_project`, `bad_path`,
//! `base_missing`, `not_a_level`, `variant_exists`, `no_variant_open`, `entity_missing`,
//! `bad_field`, `nothing_to_revert`, `variant_loop`, `chain_too_long`, `bad_variant`,
//! `newer_version`, `resolved_invalid`, `io_failed`.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_ecs::serialize::ReflectCodec;
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::level_variant::{
    EntityMark, LEVEL_VARIANT_META_KIND, LEVEL_VARIANT_SUFFIX, LevelSource, LevelVariantError,
    LevelVariantFile, is_level_variant_path, read_source_file, resolve_level_variant,
};
use regia_scene::prelude::{LevelSettings, Name};
use serde_json::{Value, json};

use crate::command::{
    AddComponent, CommandGroup, DespawnEntities, EditorCommand, RemoveComponent, Reparent, SpawnEntities,
    UndoStack,
};
use crate::level_io::OpenLevel;
use crate::viewport_agent::Handler;

/// The type path of the entity name component (a record field in scene files).
const NAME_PATH: &str = "regia_scene::components::Name";

/// Frames between two refreshes of the marks while edits keep coming (a gizmo drag).
const MARK_REFRESH_FRAMES: u64 = 15;

/// A failed level variant operation: a stable reason (the agent's `data.reason`) and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelVariantEditError {
    /// Stable lowercase reason (listed in the module docs and the agent page).
    pub reason: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl LevelVariantEditError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl From<LevelVariantError> for LevelVariantEditError {
    fn from(err: LevelVariantError) -> Self {
        Self::new(err.reason(), err.to_string())
    }
}

impl std::fmt::Display for LevelVariantEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

/// The open level variant: where it lives, its base and the resolved base in saved form.
#[derive(Resource, Clone, Debug)]
pub struct LevelVariantSession {
    /// The `.levelvariant.ron` being edited.
    pub path: PathBuf,
    /// The variant's asset id.
    pub asset: Guid,
    /// The base it names.
    pub base: Guid,
    /// The ids from the variant down to the plain level at the bottom of its chain.
    pub chain: Vec<Guid>,
    /// The resolved base, canonicalized like a snapshot.
    base_scene: SceneFile,
    /// The last computed differences and the undo state and frame they belong to.
    marks: Option<(u64, u64, LevelVariantFile)>,
}

fn is_level_source(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    text.ends_with(regia_scene::level_dir::LEVEL_FILE_SUFFIX)
        || text
            .trim_end_matches(['/', '\\'])
            .ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX)
        || text.ends_with(LEVEL_VARIANT_SUFFIX)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), LevelVariantEditError> {
    crate::atomic_write::write_atomic(path, bytes).map_err(|e| LevelVariantEditError::new("io_failed", e))
}

fn write_meta(source: &Path, kind: &str, id: Guid) -> Result<(), LevelVariantEditError> {
    let meta = regia_asset::MetaSidecar::new(kind, regia_asset::AssetId(id));
    let text = meta
        .to_ron_string()
        .map_err(|e| LevelVariantEditError::new("io_failed", format!("meta: {e}")))?;
    write_atomic(&regia_asset::meta_path_for(source), text.as_bytes())
}

/// The asset id of the level or variant at `path`, writing its `.meta` when it has none (a level
/// keeps the id it already loads under, so nothing that names it moves).
fn ensure_level_id(path: &Path) -> Result<Guid, LevelVariantEditError> {
    let id = regia_framework::level_assets::level_id_of(path).0;
    let has_meta = if is_level_variant_path(path) {
        regia_asset::meta_path_for(path).is_file()
    } else {
        let (file, dir) = regia_scene::level_dir::level_paths(path);
        regia_asset::meta_path_for(&file).is_file() || regia_asset::meta_path_for(&dir).is_file()
    };
    if !has_meta {
        let source = if path.is_dir() {
            regia_scene::level_dir::level_paths(path).1
        } else {
            path.to_path_buf()
        };
        let kind = if is_level_variant_path(path) {
            LEVEL_VARIANT_META_KIND
        } else {
            "level"
        };
        write_meta(&source, kind, id)?;
    }
    Ok(id)
}

/// Creates the variant `path` of the level (or variant) `base` with no differences, with its
/// `.meta`, and gives `base` a `.meta` if it had none. Returns the variant's asset id; it does
/// not open it ([`open_level_variant`]).
///
/// # Errors
///
/// `bad_path` (not a `.levelvariant.ron`), `not_a_level`, `base_missing`, `variant_exists`,
/// `io_failed`.
pub fn create_level_variant(base: &Path, path: &Path) -> Result<Guid, LevelVariantEditError> {
    if !is_level_variant_path(path) {
        return Err(LevelVariantEditError::new(
            "bad_path",
            format!("a level variant file name ends in {LEVEL_VARIANT_SUFFIX}"),
        ));
    }
    if !is_level_source(base) {
        return Err(LevelVariantEditError::new(
            "not_a_level",
            format!(
                "{} is not a .level.ron, .level.d or {LEVEL_VARIANT_SUFFIX}",
                base.display()
            ),
        ));
    }
    let exists = if is_level_variant_path(base) {
        base.is_file()
    } else {
        regia_scene::level_dir::level_form(base).is_ok()
    };
    if !exists {
        return Err(LevelVariantEditError::new(
            "base_missing",
            format!("{} does not exist", base.display()),
        ));
    }
    if path.exists() {
        return Err(LevelVariantEditError::new(
            "variant_exists",
            format!("{} already exists", path.display()),
        ));
    }
    let base_id = ensure_level_id(base)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LevelVariantEditError::new("io_failed", e.to_string()))?;
    }
    let text = LevelVariantFile::new(base_id)
        .to_ron_string()
        .map_err(|e| LevelVariantEditError::new("io_failed", e.to_string()))?;
    let asset = Guid::new_random();
    write_meta(path, LEVEL_VARIANT_META_KIND, asset)?;
    write_atomic(path, text.as_bytes())?;
    Ok(asset)
}

/// Opens the variant at `path` as the level being edited (Replace), like File ▸ Open level, and
/// starts its session.
///
/// # Errors
///
/// What the loader reports (`variant_loop`, `base_missing`, …) and `io_failed`.
pub fn open_level_variant(world: &mut World, path: &Path) -> Result<OpenLevel, LevelVariantEditError> {
    let open = crate::level_io::open_level_file(world, path).map_err(|message| {
        let reason = [
            "variant_loop",
            "chain_too_long",
            "base_missing",
            "bad_variant",
            "newer_version",
            "resolved_invalid",
        ]
        .into_iter()
        .find(|reason| message.contains(reason))
        .unwrap_or("io_failed");
        LevelVariantEditError::new(reason, message)
    })?;
    world.insert_resource(open.clone());
    crate::level_io::reset_edit_state(world);
    world.remove_resource::<LevelVariantSession>();
    ensure_session(world)?;
    Ok(open)
}

/// The project-relative file of a variant created next to `base` (`arena.level.ron` →
/// `arena_variant.levelvariant.ron`).
#[must_use]
pub fn default_variant_path(base: &Path) -> PathBuf {
    let name = base
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = [
        regia_scene::level_dir::LEVEL_FILE_SUFFIX,
        regia_scene::level_dir::LEVEL_DIR_SUFFIX,
        LEVEL_VARIANT_SUFFIX,
    ]
    .iter()
    .find_map(|suffix| name.strip_suffix(suffix))
    .unwrap_or(&name)
    .to_owned();
    base.with_file_name(format!("{stem}_variant{LEVEL_VARIANT_SUFFIX}"))
}

/// The open level's variant path, when the open level is a variant.
fn open_variant_path(world: &World) -> Option<(regia_scene::prelude::LevelId, PathBuf)> {
    let open = world.get_resource::<OpenLevel>()?;
    let path = open.path.clone()?;
    is_level_variant_path(&path).then_some((open.id, path))
}

/// Whether the level being edited is a level variant.
#[must_use]
pub fn is_variant_open(world: &World) -> bool {
    open_variant_path(world).is_some()
}

/// Makes sure [`LevelVariantSession`] describes the open variant (reading and resolving its base
/// when it does not yet).
///
/// # Errors
///
/// `no_variant_open`, and what resolving the base reports.
pub fn ensure_session(world: &mut World) -> Result<(), LevelVariantEditError> {
    let Some((_, path)) = open_variant_path(world) else {
        return Err(LevelVariantEditError::new(
            "no_variant_open",
            "the open level is not a level variant",
        ));
    };
    if world
        .get_resource::<LevelVariantSession>()
        .is_some_and(|session| session.path == path)
    {
        return Ok(());
    }
    let _span = tracing::info_span!("level_variant_session", path = %path.display()).entered();
    let asset = regia_framework::level_assets::level_id_of(&path).0;
    let file = match read_source_file(asset, &path)? {
        LevelSource::Variant(file) => file,
        LevelSource::Level(_) => {
            return Err(LevelVariantEditError::new(
                "bad_variant",
                "this file holds a cooked (flattened) level; open the project's source variant",
            ));
        }
    };
    let root = regia_framework::level_assets::assets_root_of(&path);
    let index = regia_framework::level_assets::LevelSourceIndex::scan(&root);
    let find = |id: Guid| index.levels.get(&id).cloned();
    let base = file.base_guid();
    let mut load = |id: Guid| match find(id) {
        Some(file) => read_source_file(id, &file),
        None => Err(LevelVariantError::BaseMissing(
            id,
            "no level source has this id".into(),
        )),
    };
    if base == asset {
        return Err(LevelVariantError::Loop(vec![asset, asset]).into());
    }
    let base_source = load(base)?;
    let resolved = resolve_level_variant(base, base_source, &mut load)?;
    if resolved.chain.contains(&asset) {
        let mut chain = vec![asset];
        chain.extend(resolved.chain.iter().copied());
        return Err(LevelVariantError::Loop(chain).into());
    }
    let mut chain = vec![asset];
    chain.extend(resolved.chain);
    let base_scene = canonical_scene(world, &resolved.scene);
    world.insert_resource(LevelVariantSession {
        path,
        asset,
        base,
        chain,
        base_scene,
        marks: None,
    });
    Ok(())
}

/// `scene` in the form a snapshot writes it: each registered component decoded and re-encoded
/// through reflection, the level settings re-encoded (their defaults when absent). A value that
/// does not decode keeps its text.
fn canonical_scene(world: &World, scene: &SceneFile) -> SceneFile {
    let mut out = scene.clone();
    if let Some(registry) = world.get_resource::<AppTypeRegistry>() {
        let registry = registry.read();
        let mut codec = ReflectCodec::new(&registry);
        for entity in &mut out.entities {
            for (path, value) in &mut entity.components {
                let Some(registration) =
                    regia_core::brand::lookup_type_path(path, |p| registry.get_with_type_path(p))
                else {
                    continue;
                };
                let text = codec
                    .from_ron_typed(value.get_ron(), registration)
                    .ok()
                    .and_then(|decoded| codec.to_ron_typed(decoded.as_partial_reflect()).ok());
                if let Some(raw) = text.and_then(|t| {
                    ron::value::RawValue::from_ron(t.trim_end())
                        .ok()
                        .map(|r| r.to_owned())
                }) {
                    *value = raw;
                }
            }
        }
    }
    let settings = out
        .resources
        .get(LEVEL_SETTINGS_PATH)
        .and_then(|raw| LevelSettings::from_ron(raw.get_ron()).ok())
        .unwrap_or_default();
    if let Some(raw) = settings.to_ron().ok().and_then(|text| {
        ron::value::RawValue::from_ron(text.trim_end())
            .ok()
            .map(|r| r.to_owned())
    }) {
        out.resources.insert(LEVEL_SETTINGS_PATH.to_owned(), raw);
    }
    out
}

/// The open level as a save writes it: the snapshot, its layer binding and its settings.
fn edited_scene(
    world: &mut World,
    level: regia_scene::prelude::LevelId,
) -> Result<SceneFile, LevelVariantEditError> {
    let level = crate::seq_tracks::with_preview_lifted(world, |world| {
        crate::level_io::snapshot_saved_level(world, level)
    })
    .map_err(|e| LevelVariantEditError::new("io_failed", e.to_string()))?;
    let mut scene = level.scene;
    crate::level_io::keep_layer_binding(world, level.id, &mut scene);
    if let Some(raw) = level.settings.to_ron().ok().and_then(|text| {
        ron::value::RawValue::from_ron(text.trim_end())
            .ok()
            .map(|r| r.to_owned())
    }) {
        scene.resources.insert(LEVEL_SETTINGS_PATH.to_owned(), raw);
    }
    Ok(scene)
}

/// The differences between the open variant's base and the level as edited now, with the edited
/// scene they came from.
///
/// # Errors
///
/// `no_variant_open`, a base that does not resolve, a failed snapshot.
pub fn current_diff(world: &mut World) -> Result<(LevelVariantFile, SceneFile), LevelVariantEditError> {
    ensure_session(world)?;
    let Some((level, _)) = open_variant_path(world) else {
        return Err(LevelVariantEditError::new(
            "no_variant_open",
            "the open level is not a level variant",
        ));
    };
    let _span = tracing::info_span!("level_variant_diff").entered();
    let edited = edited_scene(world, level)?;
    let session = world.resource::<LevelVariantSession>();
    Ok((
        regia_scene::level_variant::diff_levels(session.base, &session.base_scene, &edited),
        edited,
    ))
}

/// The save of an open variant (File ▸ Save level reaches it through `level_io::save_level_file`):
/// writes only the differences from the base to `path`.
///
/// # Errors
///
/// A message naming the reason, like the other save paths.
pub fn save_variant(world: &mut World, open: &OpenLevel, path: &Path) -> Result<(), String> {
    if open_variant_path(world).is_none() {
        return Err(format!(
            "no_variant_open: {} is a level variant file; create one with File ▸ New level variant…",
            path.display()
        ));
    }
    let (file, _) = current_diff(world).map_err(|e| e.to_string())?;
    let text = file.to_ron_string().map_err(|e| format!("io_failed: {e}"))?;
    crate::atomic_write::write_atomic(path, text.as_bytes())?;
    if open.path.as_deref() == Some(path) {
        crate::level_dirty::begin_tracking(world, open.id, path);
    }
    if let Some(mut session) = world.get_resource_mut::<LevelVariantSession>() {
        session.marks = None;
    }
    Ok(())
}

/// Refreshes the cached differences the marks read: after an edit (the undo state moved), at most
/// once every `MARK_REFRESH_FRAMES` frames. Call with the panel's frame number.
pub fn refresh_marks(world: &mut World, frame: u64) {
    if !is_variant_open(world) {
        return;
    }
    if ensure_session(world).is_err() {
        return;
    }
    let state = world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id);
    let stale = match world
        .get_resource::<LevelVariantSession>()
        .and_then(|s| s.marks.as_ref())
    {
        None => true,
        Some((old_state, old_frame, _)) => {
            *old_state != state && frame.saturating_sub(*old_frame) >= MARK_REFRESH_FRAMES
        }
    };
    if !stale {
        return;
    }
    if let Ok((file, _)) = current_diff(world)
        && let Some(mut session) = world.get_resource_mut::<LevelVariantSession>()
    {
        session.marks = Some((state, frame, file));
    }
}

fn cached_marks(world: &World) -> Option<&LevelVariantFile> {
    open_variant_path(world)?;
    world
        .get_resource::<LevelVariantSession>()?
        .marks
        .as_ref()
        .map(|(_, _, file)| file)
}

/// How `entity` relates to the open variant's base, from the cached differences (`None` when no
/// variant is open or the entity has no guid).
#[must_use]
pub fn entity_mark(world: &World, entity: Entity) -> Option<EntityMark> {
    let guid = world.get::<regia_scene::prelude::EntityGuid>(entity)?.0;
    cached_marks(world).map(|file| file.mark(guid))
}

/// The Outliner's mark of an entity in an open variant: a filled diamond when it differs from the
/// base, a plus when the variant added it (the prefab override mark's style).
pub fn outliner_mark(ui: &mut egui::Ui, world: &World, entity: Entity) {
    match entity_mark(world, entity) {
        Some(EntityMark::Overridden) => {
            ui.strong("\u{25c6}")
                .on_hover_text("Differs from the base level of this level variant");
        }
        Some(EntityMark::Added) => {
            ui.strong("+").on_hover_text("Added by this level variant");
        }
        _ => {}
    }
}

/// The Inspector's entity line in an open variant: the mark and Revert to base for the whole
/// entity.
pub fn inspector_entity_header(ui: &mut egui::Ui, world: &mut World, entity: Entity) {
    refresh_marks(world, ui.ctx().cumulative_frame_nr());
    let Some(mark) = entity_mark(world, entity) else {
        return;
    };
    if mark == EntityMark::Base {
        return;
    }
    let Some(guid) = world.get::<regia_scene::prelude::EntityGuid>(entity).map(|g| g.0) else {
        return;
    };
    let mut revert_now = false;
    ui.horizontal(|ui| {
        ui.strong("\u{25c6}");
        ui.label(if mark == EntityMark::Added {
            "Added by this level variant"
        } else {
            "Differs from the base level"
        });
        if crate::agent_parity::action_button(ui, &crate::agent_parity::INSPECTOR_REVERT_LEVEL_VARIANT_ENTITY)
            .clicked()
        {
            revert_now = true;
        }
    });
    if revert_now && let Err(err) = revert(world, guid, None, None) {
        tracing::error!(target: "regia_editor", "revert to base: {err}");
    }
}

/// The Inspector's rows for the fields of `component` the open variant overrides, each with
/// Revert to base.
pub fn inspector_controls(ui: &mut egui::Ui, world: &mut World, entity: Entity, component: &str) {
    let Some(guid) = world.get::<regia_scene::prelude::EntityGuid>(entity).map(|g| g.0) else {
        return;
    };
    let Some(file) = cached_marks(world) else {
        return;
    };
    if file.mark(guid) != EntityMark::Overridden {
        return;
    }
    let rows: Vec<String> = file
        .overridden_fields(guid)
        .into_iter()
        .filter(|(c, _)| c == component)
        .map(|(_, path)| path)
        .collect();
    if rows.is_empty() {
        return;
    }
    let mut clear: Option<String> = None;
    ui.separator();
    ui.weak("Overridden in this level variant:");
    for path in &rows {
        ui.horizontal(|ui| {
            ui.strong("\u{25c6}")
                .on_hover_text("This field differs from the base level");
            ui.label(if path.is_empty() {
                "(whole component)"
            } else {
                path.as_str()
            });
            if crate::agent_parity::action_button(
                ui,
                &crate::agent_parity::INSPECTOR_REVERT_LEVEL_VARIANT_FIELD,
            )
            .clicked()
            {
                clear = Some(path.clone());
            }
        });
    }
    if let Some(path) = clear
        && let Err(err) = revert(world, guid, Some(component), Some(&path))
    {
        tracing::error!(target: "regia_editor", "revert field to base: {err}");
    }
}

fn decode(
    world: &World,
    component: &str,
    text: &str,
) -> Result<Box<dyn PartialReflect>, LevelVariantEditError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = regia_core::brand::lookup_type_path(component, |p| registry.get_with_type_path(p))
        .ok_or_else(|| LevelVariantEditError::new("bad_field", format!("`{component}` is not registered")))?;
    let mut codec = ReflectCodec::new(&registry);
    codec
        .from_ron_typed(text, registration)
        .map(|value| value.into_partial_reflect())
        .map_err(|e| LevelVariantEditError::new("bad_field", format!("{component}: {e}")))
}

/// Reverts `entity` (a guid of the open variant's level) to its base: the whole entity, one
/// component (`component`), or one field of it (`component` and `path`). The world changes as one
/// undo step; the next save writes the variant without those differences. Returns how many
/// difference entries went.
///
/// # Errors
///
/// `no_variant_open`, `bad_field` (a path without a component, a type that is not registered),
/// `nothing_to_revert`.
pub fn revert(
    world: &mut World,
    entity: Guid,
    component: Option<&str>,
    path: Option<&str>,
) -> Result<usize, LevelVariantEditError> {
    if component.is_none() && path.is_some_and(|p| !p.is_empty()) {
        return Err(LevelVariantEditError::new(
            "bad_field",
            "a field path needs its component",
        ));
    }
    let (current, edited) = current_diff(world)?;
    let mut reverted = current.clone();
    let count = match component {
        None => reverted.revert_entity(entity),
        Some(c) => reverted.revert_field(entity, c, path.unwrap_or("")),
    };
    if count == 0 {
        return Err(LevelVariantEditError::new(
            "nothing_to_revert",
            format!("entity {entity} has no such difference from the base"),
        ));
    }
    let mut target = world.resource::<LevelVariantSession>().base_scene.clone();
    let mut warnings = Vec::new();
    reverted.apply(&mut target, &mut warnings);
    let record = target.entities.iter().find(|e| e.guid == entity).cloned();
    let live_record = edited.entities.iter().find(|e| e.guid == entity).cloned();
    let live = crate::hierarchy_ops::guid_entity(world, entity);
    let mut commands: Vec<Box<dyn EditorCommand>> = Vec::new();
    match (live, record) {
        (Some(e), None) => commands.push(Box::new(DespawnEntities {
            entities: vec![e.to_bits()],
            snapshot: None,
        })),
        (None, Some(record)) => {
            let mut file = SceneFile::new();
            let mut record = record;
            if record
                .parent
                .is_some_and(|p| crate::hierarchy_ops::guid_entity(world, p).is_none())
            {
                record.parent = None;
            }
            file.entities.push(record);
            commands.push(Box::new(SpawnEntities {
                file,
                spawned_guids: Vec::new(),
                spawned_entities: Vec::new(),
            }));
        }
        (Some(e), Some(record)) => {
            let live_record = live_record.unwrap_or_default();
            let mut names: Vec<&String> = record
                .components
                .keys()
                .chain(live_record.components.keys())
                .collect();
            names.sort();
            names.dedup();
            for name in names {
                if component.is_some_and(|c| c != name) {
                    continue;
                }
                match (record.components.get(name), live_record.components.get(name)) {
                    (Some(want), have) if have.is_none_or(|have| have.get_ron() != want.get_ron()) => {
                        let value = decode(world, name, want.get_ron())?;
                        let command = AddComponent::capture(world, e, name, value)
                            .map_err(|err| LevelVariantEditError::new("bad_field", err.to_string()))?;
                        commands.push(Box::new(command));
                    }
                    (None, Some(_)) => {
                        let command = RemoveComponent::capture(world, e, name)
                            .map_err(|err| LevelVariantEditError::new("bad_field", err.to_string()))?;
                        commands.push(Box::new(command));
                    }
                    _ => {}
                }
            }
            if component.is_none() || component == Some("#name") {
                push_name(world, e, &record, &live_record, &mut commands)?;
            }
            if (component.is_none() || component == Some("#parent")) && record.parent != live_record.parent {
                let parent = record
                    .parent
                    .and_then(|p| crate::hierarchy_ops::guid_entity(world, p));
                commands.push(Box::new(Reparent::capture(world, e, parent)));
            }
        }
        (None, None) => {}
    }
    if commands.is_empty() {
        return Err(LevelVariantEditError::new(
            "nothing_to_revert",
            format!("entity {entity} already matches its base"),
        ));
    }
    let mut group = CommandGroup::new("Revert to base");
    for mut command in commands {
        command.apply(world);
        group.push(command);
    }
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(Box::new(group));
    stack.seal();
    if let Some(mut session) = world.get_resource_mut::<LevelVariantSession>() {
        session.marks = None;
    }
    Ok(count)
}

fn push_name(
    world: &World,
    e: Entity,
    record: &SceneEntity,
    live: &SceneEntity,
    commands: &mut Vec<Box<dyn EditorCommand>>,
) -> Result<(), LevelVariantEditError> {
    if record.name == live.name {
        return Ok(());
    }
    let command: Box<dyn EditorCommand> = match &record.name {
        Some(name) => Box::new(
            AddComponent::capture(world, e, NAME_PATH, Box::new(Name::new(name.clone())))
                .map_err(|err| LevelVariantEditError::new("bad_field", err.to_string()))?,
        ),
        None => Box::new(
            RemoveComponent::capture(world, e, NAME_PATH)
                .map_err(|err| LevelVariantEditError::new("bad_field", err.to_string()))?,
        ),
    };
    commands.push(command);
    Ok(())
}

/// The state `editor.level_variant_state` reports.
///
/// # Errors
///
/// A base that does not resolve, a failed snapshot.
pub fn describe(world: &mut World) -> Result<Value, LevelVariantEditError> {
    let Some((_, path)) = open_variant_path(world) else {
        return Ok(json!({ "open": false }));
    };
    let (file, edited) = current_diff(world)?;
    let session = world.resource::<LevelVariantSession>();
    let entities: Vec<Value> = edited
        .entities
        .iter()
        .map(|e| {
            let fields: Vec<Value> = file
                .overridden_fields(e.guid)
                .into_iter()
                .map(|(component, path)| json!({ "component": component, "path": path }))
                .collect();
            json!({
                "guid": e.guid.to_string(),
                "name": e.name,
                "mark": file.mark(e.guid).as_str(),
                "fields": fields,
            })
        })
        .collect();
    let saved = std::fs::read_to_string(&path).ok();
    let dirty = file.to_ron_string().ok() != saved;
    Ok(json!({
        "open": true,
        "path": path.display().to_string(),
        "asset": session.asset.to_string(),
        "base": session.base.to_string(),
        "chain": session.chain.iter().map(Guid::to_string).collect::<Vec<_>>(),
        "differences": file.len(),
        "overrides": file.overrides.len(),
        "added_components": file.added_components.len(),
        "removed_components": file.removed_components.len(),
        "record_edits": file.record_edits.len(),
        "added_entities": file.added_entities.iter().map(|e| e.guid.to_string()).collect::<Vec<_>>(),
        "removed_entities": file.removed_entities.iter().map(Guid::to_string).collect::<Vec<_>>(),
        "resources": file.resources.keys().cloned().collect::<Vec<_>>(),
        "removed_resources": file.removed_resources,
        "entities": entities,
        "unsaved": dirty,
    }))
}

/// File ▸ New level variant… with unsaved changes was clicked once: the next click discards them
/// (the two-click confirm this item kept when the other level switches moved to the Save all
/// dialog, integ-v06k).
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct NewVariantDiscardArmed;

/// Whether File ▸ New level variant… may go ahead: always without unsaved changes; with them, on the
/// second click (the first arms it). A click that goes ahead disarms it.
pub fn confirm_new_variant(world: &mut World, unsaved: bool) -> bool {
    let armed = world.remove_resource::<NewVariantDiscardArmed>().is_some();
    if !unsaved || armed {
        return true;
    }
    world.insert_resource(NewVariantDiscardArmed);
    false
}

/// File ▸ New level variant…: picks the base level, then where the variant goes, creates it and
/// opens it. Returns the toast text.
pub fn new_variant_from_dialogs(world: &mut World) -> Option<String> {
    let base = crate::file_dialog::FileDialog::new()
        .set_title("Base level of the new variant")
        .add_filter("Regia Engine level", &["ron"])
        .pick_file()?;
    let suggested = default_variant_path(&base);
    let mut dialog = crate::file_dialog::FileDialog::new()
        .set_title("New level variant")
        .add_filter("Regia Engine level variant", &["ron"]);
    if let Some(name) = suggested.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    if let Some(dir) = base.parent() {
        dialog = dialog.set_directory(dir);
    }
    let path = dialog.save_file()?;
    Some(
        match create_level_variant(&base, &path).and_then(|_| open_level_variant(world, &path)) {
            Ok(_) => format!("New level variant {}", path.display()),
            Err(err) => format!("New level variant failed: {err}"),
        },
    )
}

fn rpc(err: LevelVariantEditError) -> RpcError {
    let (code, regia_code, hint) = match err.reason {
        "no_project" | "base_missing" | "entity_missing" | "no_variant_open" => (
            protocol::NOT_FOUND,
            "F0003",
            "open a variant with editor.open_level or create one with editor.new_level_variant",
        ),
        "io_failed" => (protocol::INTERNAL_ERROR, "F0001", "see logs.read for details"),
        _ => (
            protocol::INVALID_PARAMS,
            "F0002",
            "see book/agents/level-variants.md for the parameters and reasons",
        ),
    };
    RpcError {
        code,
        message: err.message,
        data: Some(json!({ "code": regia_code, "reason": err.reason, "hint": hint })),
    }
}

fn project_path(world: &World, params: &Value, key: &str) -> Result<PathBuf, LevelVariantEditError> {
    let rel = params
        .get(key)
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            LevelVariantEditError::new("bad_path", format!("`{key}` (project-relative path) is required"))
        })?;
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| LevelVariantEditError::new("no_project", "no project is open"))?;
    let rel = rel.replace('\\', "/");
    if rel.split('/').any(|part| part == "..") || Path::new(&rel).is_absolute() {
        return Err(LevelVariantEditError::new(
            "bad_path",
            format!("`{key}` must stay inside the project"),
        ));
    }
    let rel = rel.strip_prefix("assets/").map(str::to_string).unwrap_or(rel);
    Ok(root.join("assets").join(rel))
}

fn editor_new_level_variant(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let base = project_path(world, params, "base").map_err(rpc)?;
    let path = project_path(world, params, "path").map_err(rpc)?;
    let asset = create_level_variant(&base, &path).map_err(rpc)?;
    let open = open_level_variant(world, &path).map_err(rpc)?;
    let session = world.resource::<LevelVariantSession>();
    Ok(json!({
        "path": params["path"],
        "asset": asset.to_string(),
        "level": open.id.0.to_string(),
        "base": session.base.to_string(),
        "chain": session.chain.iter().map(Guid::to_string).collect::<Vec<_>>(),
    }))
}

fn editor_level_variant_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    describe(world).map_err(rpc)
}

fn editor_level_variant_revert(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = params
        .get("entity")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Guid>().ok())
        .ok_or_else(|| {
            rpc(LevelVariantEditError::new(
                "entity_missing",
                "`entity` (a guid) is required",
            ))
        })?;
    let component = params.get("component").and_then(Value::as_str);
    let path = params.get("path").and_then(Value::as_str);
    let reverted = revert(world, entity, component, path).map_err(rpc)?;
    let (file, _) = current_diff(world).map_err(rpc)?;
    Ok(json!({ "reverted": reverted, "differences": file.len() }))
}

/// The agent methods of level variants.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.new_level_variant",
                tool: "new_level_variant",
                description: "Create a level variant (.levelvariant.ron): a base level (or variant) plus only \
                    its differences. Writes the file and its .meta (and the base's .meta if missing), then opens \
                    it for editing; a save writes only what differs from the base.",
                params: &[
                    ParamSchema {
                        name: "base",
                        kind: "string",
                        required: true,
                        description: "Project-relative base: a .level.ron, .level.d or another .levelvariant.ron.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative new file ending in .levelvariant.ron.",
                    },
                ],
                result: "{ path, asset, level, base, chain }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_new_level_variant as Handler,
        ),
        (
            MethodSchema {
                name: "editor.level_variant_state",
                tool: "level_variant_state",
                description: "The open level variant: its base, chain, the differences a save would write, \
                    and each entity's mark (base, overridden or added) with its overridden fields. \
                    { open: false } when the open level is not a variant.",
                params: &[],
                result: "{ open, path, asset, base, chain, differences, overrides, added_components, \
                    removed_components, record_edits, added_entities, removed_entities, resources, \
                    removed_resources, entities: [{guid, name, mark, fields: [{component, path}]}], unsaved }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_level_variant_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.level_variant_revert",
                tool: "level_variant_revert",
                description: "Revert to base in the open level variant: a whole entity (an added one is \
                    deleted, a removed one comes back), one component, or one field. One undo step.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "string",
                        required: true,
                        description: "The entity's guid.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: false,
                        description: "Component type path, or #name / #parent for the entity record; omit for the whole entity.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Field path inside the component (\".translation\"); omit for the whole component.",
                    },
                ],
                result: "{ reverted, differences }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_level_variant_revert as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
