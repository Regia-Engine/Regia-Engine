// SPDX-License-Identifier: MPL-2.0
//! The Layers panel (`docs/specs/37-per-entity-levels-layers-instances.md` § 4.4, § 6.5, § 12): the
//! open level's data layers with visible and loaded toggles, the current layer set new entities
//! join, adding the selection to a layer, and editing a placed level on its own.
//!
//! The toggles are a working state: they request states in the [`LayerTable`] and change neither
//! the level's `initial` states nor any file. *Play* starts from the initial states again.
//! [`LayersPanel`] backs the egui panel and the agent methods alike
//! (`book/agents/levels-layers-instances.md`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_framework::level_assets::{LevelSourceIndex, load_level_source};
use regia_scene::format::SceneFile;
use regia_scene::layers::{
    LAYER_MEMBERSHIP_PATH, LayerKind, LayerMembership, LayerState, LayerTable, Side, valid_layer_name,
};
use regia_scene::level::LevelId;
use regia_scene::placed::PlacedLevel;
use regia_scene::prelude::EntityGuid;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::level_io::{self, OpenLevel};
use crate::placed_create::{PlacedPivot, create_placed_level};
use crate::selection::Selection;
use crate::viewport_agent::Handler;

/// A placed level open for editing on its own, and the level to come back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedEditSession {
    /// The placed level being edited.
    pub level: LevelId,
    /// Its source.
    pub path: PathBuf,
    /// The host level to reopen.
    pub host: PathBuf,
}

/// The panel's model.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayersPanel {
    /// The current layer set: entities placed while it is not empty join these layers.
    pub current: BTreeSet<String>,
    /// The placed level being edited (the panel's banner).
    pub session: Option<PlacedEditSession>,
    /// The last error of a panel action.
    pub error: Option<String>,
    /// *Create placed level*: the new level's path under `assets/`.
    pub create_path: String,
    /// *Create placed level*: the pivot.
    pub create_pivot: PlacedPivot,
}

/// One row of the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerRow {
    /// Layer name.
    pub name: String,
    /// Kind.
    pub kind: LayerKind,
    /// Parent layer.
    pub parent: Option<String>,
    /// Requested state.
    pub requested: LayerState,
    /// Effective state.
    pub effective: LayerState,
    /// Members.
    pub members: usize,
    /// In the current layer set.
    pub current: bool,
}

/// The rows, ordered by name.
#[must_use]
pub fn rows(world: &World) -> Vec<LayerRow> {
    let current = world
        .get_resource::<LayersPanel>()
        .map(|panel| panel.current.clone())
        .unwrap_or_default();
    world
        .get_resource::<LayerTable>()
        .map(|table| {
            table
                .layers()
                .map(|info| LayerRow {
                    name: info.name.to_owned(),
                    kind: info.kind,
                    parent: info.parent.map(str::to_owned),
                    requested: info.requested,
                    effective: info.effective,
                    members: info.member_count,
                    current: current.contains(info.name),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Requests a state from the panel: the editor owns every layer.
fn request(world: &mut World, layer: &str, state: LayerState) -> Result<bool, RpcError> {
    let mut table = world
        .get_resource_mut::<LayerTable>()
        .ok_or_else(|| reason("layer_unknown", NOT_FOUND, "no layers in this level"))?;
    table
        .request(layer, state, false, Side::Local)
        .map_err(|e| reason(e.reason(), NOT_FOUND, e.to_string()))
}

/// Shows or hides a layer's members: `Live` or `Parked`.
pub fn set_visible(world: &mut World, layer: &str, visible: bool) -> Result<bool, RpcError> {
    let state = if visible {
        LayerState::Live
    } else {
        LayerState::Parked
    };
    request(world, layer, state)
}

/// Loads or unloads a layer's members: `Parked` or `Absent`. Loading a layer that shows already
/// keeps it showing.
pub fn set_loaded(world: &mut World, layer: &str, loaded: bool) -> Result<bool, RpcError> {
    if !loaded {
        return request(world, layer, LayerState::Absent);
    }
    let requested = world
        .get_resource::<LayerTable>()
        .and_then(|table| table.requested(layer));
    if requested.is_some_and(|state| state >= LayerState::Parked) {
        return Ok(false);
    }
    request(world, layer, LayerState::Parked)
}

pub(crate) fn reason(code: &str, kind: i64, message: impl Into<String>) -> RpcError {
    RpcError::new(
        kind,
        code,
        message,
        "editor.layers_panel_state lists the layers; book/agents/levels-layers-instances.md lists the reasons",
    )
}

/// Replaces the current layer set. Names must be layers of this level.
pub fn set_current(world: &mut World, names: &[String]) -> Result<(), RpcError> {
    let table = world.get_resource::<LayerTable>();
    if let Some(unknown) = names
        .iter()
        .find(|name| !table.is_some_and(|table| table.contains(name)))
    {
        return Err(reason(
            "layer_unknown",
            NOT_FOUND,
            format!("`{unknown}` is not a layer of this level"),
        ));
    }
    world.get_resource_or_insert_with(LayersPanel::default).current = names.iter().cloned().collect();
    Ok(())
}

/// Adds the current layer set to every record of `file`: what is being placed joins the set
/// (spec 37 § 4.4). Called once, on the first apply of the spawn command, so an undo and redo
/// bring the same membership back.
pub fn stamp_current_set(world: &World, file: &mut SceneFile) {
    let Some(panel) = world
        .get_resource::<LayersPanel>()
        .filter(|p| !p.current.is_empty())
    else {
        return;
    };
    for record in &mut file.entities {
        let mut membership = LayerMembership::from_record(record).unwrap_or_default();
        for name in &panel.current {
            if !membership.layers.contains(name) {
                membership.layers.push(name.clone());
            }
        }
        if let Ok(text) = ron::to_string(&membership)
            && let Ok(raw) = ron::value::RawValue::from_boxed_ron(text.into_boxed_str())
        {
            record.components.insert(LAYER_MEMBERSHIP_PATH.to_owned(), raw);
        }
    }
}

pub(crate) fn entity_of(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(entity, _)| entity)
}

/// Adds entities to, or takes them out of, layers: one undo step, one entity file per entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetMembership {
    /// `(entity, before, after)`.
    edits: Vec<(Guid, Option<LayerMembership>, LayerMembership)>,
}

impl SetMembership {
    /// The command for `entities`. `None` when nothing would change.
    #[must_use]
    pub fn new(world: &World, entities: &[Entity], add: &[String], remove: &[String]) -> Option<Self> {
        let mut edits = Vec::new();
        for entity in entities {
            let Some(guid) = world.get::<EntityGuid>(*entity) else {
                continue;
            };
            let before = world.get::<LayerMembership>(*entity).cloned();
            let mut after = before.clone().unwrap_or_default();
            after.layers.retain(|layer| !remove.contains(layer));
            for layer in add {
                if !after.layers.contains(layer) {
                    after.layers.push(layer.clone());
                }
            }
            if Some(&after) != before.as_ref() && !(before.is_none() && after.layers.is_empty()) {
                edits.push((guid.0, before, after));
            }
        }
        (!edits.is_empty()).then_some(Self { edits })
    }

    /// Entities the command changes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    /// Whether it changes none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    fn write(world: &mut World, guid: Guid, value: Option<LayerMembership>) {
        let Some(entity) = entity_of(world, guid) else {
            return;
        };
        let mut entity = world.entity_mut(entity);
        match value {
            Some(membership) => {
                entity.insert(membership);
            }
            None => {
                entity.remove::<LayerMembership>();
            }
        }
    }

    fn touch(world: &mut World) {
        if let Some(mut table) = world.get_resource_mut::<LayerTable>() {
            table.mark_dirty();
        }
    }
}

impl EditorCommand for SetMembership {
    fn apply(&mut self, world: &mut World) {
        for (guid, _, after) in self.edits.clone() {
            Self::write(world, guid, Some(after));
        }
        Self::touch(world);
    }

    fn revert(&mut self, world: &mut World) {
        for (guid, before, _) in self.edits.clone() {
            Self::write(world, guid, before);
        }
        Self::touch(world);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Set layers of {} entities", self.edits.len())
    }
}

/// Applies a membership change to the selection on the undo stack. Answers the entities changed.
pub fn change_selection(world: &mut World, add: &[String], remove: &[String]) -> usize {
    let entities: Vec<Entity> = world
        .get_resource::<Selection>()
        .map(|selection| selection.entities().to_vec())
        .unwrap_or_default();
    let Some(mut command) = SetMembership::new(world, &entities, add, remove) else {
        return 0;
    };
    let changed = command.len();
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    changed
}

/// The project's `assets/`, else the one the open level lives under.
pub(crate) fn assets_dir(world: &World) -> Option<PathBuf> {
    crate::mesh_source::project_root(world)
        .map(|root| root.join("assets"))
        .or_else(|| {
            let open = world.get_resource::<OpenLevel>()?;
            Some(regia_framework::level_assets::assets_root_of(
                open.path.as_deref()?,
            ))
        })
}

/// Opens the level a placed instance references on its own (spec 37 § 6.5 *Edit*). The host is
/// closed meanwhile, which is the "rest not selectable" of the spec taken one step further: nothing
/// of the host can change during the session (ADR-1953). Refused while the host has unsaved edits.
pub fn begin_placed_edit(world: &mut World, entity: Entity) -> Result<PlacedEditSession, RpcError> {
    let placed = world
        .get::<PlacedLevel>(entity)
        .cloned()
        .ok_or_else(|| reason("not_placed", INVALID_PARAMS, "this entity places no level"))?;
    let level = placed.level_id().ok_or_else(|| {
        reason(
            "level_not_found",
            NOT_FOUND,
            format!("`{}` is not an id", placed.level),
        )
    })?;
    if world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty) {
        return Err(reason(
            "level_dirty",
            INVALID_PARAMS,
            "save the host level before editing what it places",
        ));
    }
    let host = world
        .get_resource::<OpenLevel>()
        .and_then(|open| open.path.clone())
        .ok_or_else(|| reason("level_dirty", INVALID_PARAMS, "save the host level first"))?;
    let assets = assets_dir(world).unwrap_or_default();
    let path = LevelSourceIndex::scan(&assets)
        .levels
        .get(&level.0)
        .cloned()
        .ok_or_else(|| {
            reason(
                "level_not_found",
                NOT_FOUND,
                format!("no source has id {}", level.0),
            )
        })?;
    let open =
        level_io::open_level_file(world, &path).map_err(|e| reason("level_not_found", NOT_FOUND, e))?;
    world.insert_resource(open);
    level_io::reset_edit_state(world);
    let session = PlacedEditSession { level, path, host };
    world.get_resource_or_insert_with(LayersPanel::default).session = Some(session.clone());
    Ok(session)
}

/// Ends the session: `commit` saves the placed level first, and the host reopens either way, so
/// every copy shows the saved level. The session stays in the panel until the host is open again:
/// on an error the placed level stays open and Commit or Discard can run again.
pub fn end_placed_edit(world: &mut World, commit: bool) -> Result<(), RpcError> {
    let session = world
        .get_resource_or_insert_with(LayersPanel::default)
        .session
        .clone()
        .ok_or_else(|| reason("not_placed", INVALID_PARAMS, "no placed level is being edited"))?;
    if !placed_level_is_open(world, &session.path) {
        // Another level was opened during the session: the world no longer holds the placed
        // level, so a commit would write that other level over the placed file. The session is
        // stale; it ends without writing anything and without closing what is open now.
        world.get_resource_or_insert_with(LayersPanel::default).session = None;
        load_level_source(world, &session.path)
            .map_err(|e| reason(e.reason(), INVALID_PARAMS, e.to_string()))?;
        if commit {
            return Err(reason(
                "placed_level_not_open",
                INVALID_PARAMS,
                format!(
                    "{} is no longer the open level; the placed edit ended and nothing was saved",
                    session.path.display()
                ),
            ));
        }
        return Ok(());
    }
    finish_placed_edit(world, &session, commit)?;
    world.get_resource_or_insert_with(LayersPanel::default).session = None;
    Ok(())
}

/// Whether the open level is the file a placed-edit session edits (same path, or the same file
/// under another spelling).
fn placed_level_is_open(world: &World, placed: &Path) -> bool {
    let Some(open) = world
        .get_resource::<OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return false;
    };
    open == placed
        || matches!(
            (std::fs::canonicalize(&open), std::fs::canonicalize(placed)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// The fallible part of [`end_placed_edit`]: save (on `commit`), reload the library entry and
/// reopen the host.
fn finish_placed_edit(world: &mut World, session: &PlacedEditSession, commit: bool) -> Result<(), RpcError> {
    if commit {
        let open = world
            .get_resource::<OpenLevel>()
            .cloned()
            .ok_or_else(|| RpcError::internal("no open level"))?;
        level_io::save_level_file(world, &open, &session.path).map_err(RpcError::internal)?;
    }
    // Saved or not, the library goes back to what is on disk.
    load_level_source(world, &session.path).map_err(|e| reason(e.reason(), INVALID_PARAMS, e.to_string()))?;
    let open = level_io::open_level_file(world, &session.host).map_err(RpcError::internal)?;
    world.insert_resource(open);
    level_io::reset_edit_state(world);
    Ok(())
}

// ---------------------------------------------------------------------------
// egui
// ---------------------------------------------------------------------------

enum Action {
    Visible(String, bool),
    Loaded(String, bool),
    Current(String, bool),
    AddSelection(String),
    RemoveSelection(String),
    Commit,
    Discard,
    ClearCurrent,
    CreatePath(String),
    CreatePivot(PlacedPivot),
    Create,
    Form(LayerEditForm),
    Edit(LayerEdit),
    Lock(String, bool),
    OwnFile(String, bool),
    Levels(LevelsAction),
}

/// A layer edit of the panel (spec 37 §§ 4.1-4.4, 6.5). Each runs the code of its agent method,
/// so the panel and an agent produce the same files and the same undo steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayerEdit {
    /// *Create layer* (`editor.layer_create`).
    Create {
        /// The definition file, relative to `assets/`.
        defs: String,
        /// The new layer's name.
        layer: String,
        /// Its kind.
        kind: LayerKind,
    },
    /// *Bind* / *Unbind* (`editor.layer_bind`).
    Bind {
        /// The definition file, relative to `assets/`.
        defs: String,
        /// Bind (`true`) or unbind.
        bind: bool,
    },
    /// The *initial* column (`editor.layer_initial`).
    Initial {
        /// The layer.
        layer: String,
        /// The state it starts in when the level loads.
        initial: LayerState,
    },
    /// *Delete layer* (`editor.layer_delete`).
    Delete {
        /// The definition file, relative to `assets/`.
        defs: String,
        /// The layer.
        layer: String,
    },
    /// *Edit placed level* (`editor.placed_edit { action: "begin" }`).
    EditPlaced(Entity),
    /// *Break placed level* (`editor.break_placed`).
    BreakPlaced(Entity),
}

/// Runs a panel edit through the code of its agent method.
///
/// # Errors
/// The agent method's refusal.
pub fn apply_layer_edit(world: &mut World, edit: LayerEdit) -> Result<(), RpcError> {
    let result = match edit {
        LayerEdit::Create { defs, layer, kind } => {
            crate::layer_delete_agent::create_layer_in(world, &defs, &layer, kind, None).map(|_| ())
        }
        LayerEdit::Bind { defs, bind } => {
            crate::layer_delete_agent::bind_defs(world, &defs, bind).map(|_| ())
        }
        LayerEdit::Initial { layer, initial } => {
            crate::layer_delete_agent::set_initial_state(world, &layer, initial).map(|_| ())
        }
        LayerEdit::Delete { defs, layer } => {
            crate::layer_delete_agent::delete_layer_from(world, &defs, &layer).map(|_| ())
        }
        LayerEdit::EditPlaced(entity) => begin_placed_edit(world, entity).map(|_| ()),
        LayerEdit::BreakPlaced(entity) => {
            crate::placed_break::break_placed_undoable(world, entity).map(|_| ())
        }
    };
    // What the definition lists show may have changed.
    if let Some(mut form) = world.get_resource_mut::<LayerEditForm>() {
        form.scanned = false;
    }
    result
}

/// The Layers panel's layer-edit form and its cached list of the project's definition files
/// (rescanned when the open level changes, after a panel edit, or on *Refresh*: a scan walks
/// `assets/`, so it never runs per frame). Which files are bound is read from the open level's
/// binding on every draw, so a bind by an agent or an undo shows at once.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerEditForm {
    /// *Create layer*: the definition file, relative to `assets/`.
    pub defs: String,
    /// *Create layer*: the new layer's name.
    pub name: String,
    /// *Create layer*: an editor-only layer.
    pub editor_only: bool,
    /// Whether the file list is current (`false`: scan again on the next draw).
    pub scanned: bool,
    /// The open level the list was scanned for.
    pub scanned_for: Option<LevelId>,
    /// The project's definition files: `(asset id, path relative to assets/)`, sorted by path.
    pub files: Vec<(Guid, String)>,
}

impl LayerEditForm {
    /// Rescans when the open level changed or an edit cleared the list.
    fn refresh(&mut self, world: &World) {
        let open = world.get_resource::<OpenLevel>().map(|open| open.id);
        if self.scanned && self.scanned_for == open {
            return;
        }
        self.scanned = true;
        self.scanned_for = open;
        self.files = assets_dir(world)
            .map(|assets| {
                let mut files: Vec<(Guid, String)> = LevelSourceIndex::scan(&assets)
                    .layer_defs
                    .iter()
                    .map(|(id, path)| {
                        (
                            *id,
                            path.strip_prefix(&assets).unwrap_or(path).display().to_string(),
                        )
                    })
                    .collect();
                files.sort_by(|a, b| a.1.cmp(&b.1));
                files
            })
            .unwrap_or_default();
    }

    /// The files of `binding`, in binding order (ids without a file in the project skipped).
    fn bound(&self, binding: Option<&regia_scene::layers::LevelLayers>) -> Vec<(Guid, String)> {
        binding
            .map(|binding| {
                binding
                    .def_ids()
                    .into_iter()
                    .filter_map(|id| self.files.iter().find(|(file, _)| *file == id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The bound definition file that defines `layer` (the first, as the layer table resolves).
fn defs_of(world: &World, bound: &[(Guid, String)], layer: &str) -> Option<String> {
    let library = world.get_resource::<regia_scene::layers::LayerDefsLibrary>()?;
    bound
        .iter()
        .find(|(id, _)| library.get(*id).is_some_and(|defs| defs.get(layer).is_some()))
        .map(|(_, path)| path.clone())
}

/// The open level's layer binding as loaded (its `LevelLibrary` copy, which a save writes).
fn open_level_binding(world: &World) -> Option<regia_scene::layers::LevelLayers> {
    let open = world.get_resource::<OpenLevel>()?;
    let level = world
        .get_resource::<regia_scene::level::LevelLibrary>()?
        .get(open.id)
        .cloned()?;
    regia_scene::layers::LevelLayers::from_scene(&level.scene)?.ok()
}

/// The saved initial state of each layer of `binding` (`Live` without an entry).
fn initial_states(
    binding: Option<&regia_scene::layers::LevelLayers>,
) -> std::collections::BTreeMap<String, LayerState> {
    binding
        .map(|binding| {
            binding
                .entries
                .iter()
                .map(|entry| (entry.layer.clone(), entry.initial))
                .collect()
        })
        .unwrap_or_default()
}

/// What the Layers panel shows when the open level binds no layers (bug 25, ADR-8586).
pub const LAYERS_EMPTY: &str =
    "This level binds no layers yet. Bind a layer file under Layer definitions to add some.";

/// The Layers panel.
pub fn layers_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut actions = Vec::new();
    let panel = world.get_resource::<LayersPanel>().cloned().unwrap_or_default();
    if let Some(session) = &panel.session {
        ui.horizontal(|ui| {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("Editing placed level {}", session.path.display()),
            );
            if ui.button("Commit").clicked() {
                actions.push(Action::Commit);
            }
            if ui.button("Discard").clicked() {
                actions.push(Action::Discard);
            }
        });
        ui.separator();
    }
    if let Some(error) = &panel.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
    ui.horizontal(|ui| {
        if panel.current.is_empty() {
            ui.label("Current set: none");
        } else {
            let names: Vec<&str> = panel.current.iter().map(String::as_str).collect();
            ui.label(format!("New entities join: {}", names.join(", ")));
            if ui.small_button("clear").clicked() {
                actions.push(Action::ClearCurrent);
            }
        }
    });
    ui.collapsing("Create placed level", |ui| {
        let mut path = panel.create_path.clone();
        ui.horizontal(|ui| {
            ui.label("assets/");
            if ui.text_edit_singleline(&mut path).changed() {
                actions.push(Action::CreatePath(path));
            }
        });
        let primary = world
            .get_resource::<Selection>()
            .and_then(Selection::primary)
            .and_then(|entity| world.get::<EntityGuid>(entity))
            .map(|guid| guid.0);
        ui.horizontal(|ui| {
            let mut pivot = |ui: &mut egui::Ui, label: &str, value: PlacedPivot| {
                if ui.radio(panel.create_pivot == value, label).clicked() {
                    actions.push(Action::CreatePivot(value));
                }
            };
            pivot(ui, "lowest center", PlacedPivot::LowestCenter);
            pivot(ui, "center", PlacedPivot::Center);
            if let Some(guid) = primary {
                pivot(ui, "primary selection", PlacedPivot::Member(guid));
            }
        });
        if ui.button("Create from selection").clicked() {
            actions.push(Action::Create);
        }
    });
    if let Some(primary) = world.get_resource::<Selection>().and_then(Selection::primary)
        && crate::placed_break::is_placed(world, primary)
    {
        ui.horizontal(|ui| {
            let level = world
                .get::<PlacedLevel>(primary)
                .map(|placed| placed.level.clone())
                .unwrap_or_default();
            ui.label(format!("Placed level {level}"));
            if crate::agent_parity::action_button(ui, &crate::agent_parity::LAYERS_EDIT_PLACED).clicked() {
                actions.push(Action::Edit(LayerEdit::EditPlaced(primary)));
            }
            let breakable = crate::placed_break::can_break(world, primary);
            if crate::agent_parity::action_enabled(
                ui,
                breakable,
                &crate::agent_parity::LAYERS_BREAK_PLACED,
                None,
            )
            .clicked()
            {
                actions.push(Action::Edit(LayerEdit::BreakPlaced(primary)));
            }
        });
    }
    let mut form = world.get_resource::<LayerEditForm>().cloned().unwrap_or_default();
    form.refresh(world);
    let binding = open_level_binding(world);
    let bound_files = form.bound(binding.as_ref());
    if form.defs.is_empty()
        && let Some((_, first)) = bound_files.first()
    {
        form.defs = first.clone();
    }
    ui.collapsing("Layer definitions", |ui| {
        for (_, file) in &form.files {
            ui.horizontal(|ui| {
                ui.label(file);
                let bound = bound_files.iter().any(|(_, path)| path == file);
                let (action, bind) = if bound {
                    (&crate::agent_parity::LAYERS_UNBIND, false)
                } else {
                    (&crate::agent_parity::LAYERS_BIND, true)
                };
                if crate::agent_parity::action_button(ui, action).clicked() {
                    actions.push(Action::Edit(LayerEdit::Bind {
                        defs: file.clone(),
                        bind,
                    }));
                }
            });
        }
        if form.files.is_empty() {
            ui.weak("No .layers.ron in assets/ yet: Create layer makes one.");
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::LAYERS_REFRESH_DEFS).clicked() {
            let mut next = form.clone();
            next.scanned = false;
            actions.push(Action::Form(next));
        }
        ui.separator();
        let mut next = form.clone();
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("file assets/");
            changed |= ui.text_edit_singleline(&mut next.defs).changed();
        });
        ui.horizontal(|ui| {
            ui.label("name");
            changed |= ui.text_edit_singleline(&mut next.name).changed();
            changed |= crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::LAYERS_NEW_EDITOR_ONLY,
                &mut next.editor_only,
            )
            .changed();
        });
        if changed {
            actions.push(Action::Form(next.clone()));
        }
        if crate::agent_parity::action_button(ui, &crate::agent_parity::LAYERS_CREATE).clicked() {
            actions.push(Action::Edit(LayerEdit::Create {
                defs: next.defs.clone(),
                layer: next.name.clone(),
                kind: if next.editor_only {
                    LayerKind::EditorOnly
                } else {
                    LayerKind::Runtime
                },
            }));
        }
    });
    open_levels_section(ui, world, &mut actions);
    let initial = initial_states(binding.as_ref());
    let rows = rows(world);
    let separated = separated_of_host(world);
    if rows.is_empty() {
        ui.weak(LAYERS_EMPTY);
    }
    let warnings: Vec<String> = world
        .get_resource::<LayerTable>()
        .map(|table| table.warnings().to_vec())
        .unwrap_or_default();
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("layers-grid").num_columns(7).show(ui, |ui| {
            // No header over an empty table (bug 25, ADR-8586).
            if !rows.is_empty() {
                for heading in ["layer", "visible", "loaded", "current", "initial", "members", ""] {
                    ui.strong(heading);
                }
                ui.end_row();
            }
            for row in &rows {
                let label = match row.kind {
                    LayerKind::EditorOnly => format!("{} (editor only)", row.name),
                    LayerKind::Runtime => row.name.clone(),
                };
                ui.label(label);
                let mut visible = row.effective == LayerState::Live;
                if ui.checkbox(&mut visible, "").changed() {
                    actions.push(Action::Visible(row.name.clone(), visible));
                }
                let mut loaded = row.effective != LayerState::Absent;
                if ui.checkbox(&mut loaded, "").changed() {
                    actions.push(Action::Loaded(row.name.clone(), loaded));
                }
                let mut current = row.current;
                if ui.checkbox(&mut current, "").changed() {
                    actions.push(Action::Current(row.name.clone(), current));
                }
                // The saved state the level starts in (`editor.layer_initial`).
                let saved = initial.get(&row.name).copied().unwrap_or(LayerState::Live);
                let mut chosen = saved;
                egui::ComboBox::from_id_salt(("layer-initial", &row.name))
                    .selected_text(saved.as_str())
                    .show_ui(ui, |ui| {
                        for state in [LayerState::Absent, LayerState::Parked, LayerState::Live] {
                            ui.selectable_value(&mut chosen, state, state.as_str());
                        }
                    })
                    .response
                    .on_hover_text(crate::agent_parity::LAYERS_INITIAL.label);
                if chosen != saved {
                    actions.push(Action::Edit(LayerEdit::Initial {
                        layer: row.name.clone(),
                        initial: chosen,
                    }));
                }
                ui.label(row.members.to_string());
                ui.horizontal(|ui| {
                    // Layer lock and layer file (ADR-7792): a lock glyph marks a locked row.
                    let mut locked = is_layer_locked(world, &row.name);
                    if locked {
                        ui.label(LOCK_GLYPH)
                            .on_hover_text("Locked: members cannot be picked in the viewport");
                    }
                    if crate::agent_parity::action_toggle(ui, &crate::agent_parity::LAYERS_LOCK, &mut locked)
                        .changed()
                    {
                        actions.push(Action::Lock(row.name.clone(), locked));
                    }
                    let own = separated.contains(&row.name);
                    let action = if own {
                        &crate::agent_parity::LAYERS_MERGE_FILE
                    } else {
                        &crate::agent_parity::LAYERS_OWN_FILE
                    };
                    if crate::agent_parity::action_button(ui, action).clicked() {
                        actions.push(Action::OwnFile(row.name.clone(), !own));
                    }
                    if ui.small_button("add selection").clicked() {
                        actions.push(Action::AddSelection(row.name.clone()));
                    }
                    if ui.small_button("remove selection").clicked() {
                        actions.push(Action::RemoveSelection(row.name.clone()));
                    }
                    if let Some(defs) = defs_of(world, &bound_files, &row.name)
                        && crate::agent_parity::action_button(ui, &crate::agent_parity::LAYERS_DELETE)
                            .clicked()
                    {
                        actions.push(Action::Edit(LayerEdit::Delete {
                            defs,
                            layer: row.name.clone(),
                        }));
                    }
                });
                ui.end_row();
            }
        });
        for warning in &warnings {
            ui.colored_label(egui::Color32::YELLOW, warning);
        }
        // A capped list (ADR-7306): a large level can have a finding per entity; the agent's
        // layer methods return them all.
        let found = cached_problems(world);
        for problem in found.iter().take(MAX_PROBLEM_ROWS) {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("{} {}", problem.reason, problem.detail),
            );
        }
        if found.len() > MAX_PROBLEM_ROWS {
            ui.weak(format!("… and {} more findings", found.len() - MAX_PROBLEM_ROWS));
        }
    });
    if world.get_resource::<LayerEditForm>() != Some(&form) {
        world.insert_resource(form.clone());
    }
    let mut error = None;
    for action in actions {
        let result: Result<(), RpcError> = match action {
            Action::Visible(layer, on) => set_visible(world, &layer, on).map(|_| ()),
            Action::Loaded(layer, on) => set_loaded(world, &layer, on).map(|_| ()),
            Action::Current(layer, on) => {
                let mut names: BTreeSet<String> = panel.current.clone();
                if on {
                    names.insert(layer);
                } else {
                    names.remove(&layer);
                }
                set_current(world, &names.into_iter().collect::<Vec<_>>())
            }
            Action::ClearCurrent => set_current(world, &[]),
            Action::AddSelection(layer) => {
                change_selection(world, &[layer], &[]);
                Ok(())
            }
            Action::RemoveSelection(layer) => {
                change_selection(world, &[], &[layer]);
                Ok(())
            }
            Action::Commit => end_placed_edit(world, true),
            Action::Discard => end_placed_edit(world, false),
            Action::CreatePath(path) => {
                world
                    .get_resource_or_insert_with(LayersPanel::default)
                    .create_path = path;
                Ok(())
            }
            Action::CreatePivot(pivot) => {
                world
                    .get_resource_or_insert_with(LayersPanel::default)
                    .create_pivot = pivot;
                Ok(())
            }
            Action::Create => create_placed_level(world, &panel.create_path, panel.create_pivot).map(|_| ()),
            Action::Form(next) => {
                world.insert_resource(next);
                Ok(())
            }
            Action::Edit(edit) => apply_layer_edit(world, edit),
            Action::Lock(layer, on) => set_layer_lock(world, &layer, on).map(|_| ()),
            Action::OwnFile(layer, separate) => level_io::set_layer_file(world, &layer, separate)
                .map(|_| ())
                .map_err(levels_error),
            Action::Levels(action) => run_levels_action(world, action),
        };
        error = result.err().map(|e| e.message);
    }
    world.get_resource_or_insert_with(LayersPanel::default).error = error;
}

// ---------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------

/// Layer findings the panel lists before it says how many more there are (ADR-7306).
pub const MAX_PROBLEM_ROWS: usize = 100;

/// The layer findings of the open level (spec 37 § 4.5), from what is loaded: no disk.
#[must_use]
pub fn problems(world: &World) -> Vec<regia_scene::layers::LayerProblem> {
    let Some(level) = world.get_resource::<OpenLevel>().and_then(|open| {
        world
            .get_resource::<regia_scene::level::LevelLibrary>()?
            .get(open.id)
            .cloned()
    }) else {
        return Vec::new();
    };
    let Some(binding) = regia_scene::layers::LevelLayers::from_scene(&level.scene).and_then(Result::ok)
    else {
        return Vec::new();
    };
    let library = world.get_resource::<regia_scene::layers::LayerDefsLibrary>();
    let mut bound: Vec<&regia_scene::layers::LayerDefs> = binding.embedded.iter().collect();
    bound.extend(binding.def_ids().into_iter().filter_map(|id| library?.get(id)));
    VALIDATIONS.with(|runs| runs.set(runs.get() + 1));
    regia_scene::layers::validate_level(&level.scene, &bound)
}

thread_local! {
    static VALIDATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The panel's layer findings, kept until the open level or the layer definitions change
/// (ADR-7326): the panel used to validate the whole level on every repaint.
#[derive(Resource, Debug, Default)]
struct ProblemsCache {
    /// The open level, the level data validated (levels are replaced, never edited in place; the
    /// held `Arc` keeps its address from being reused) and the definitions' change tick.
    key: Option<(
        LevelId,
        std::sync::Arc<regia_scene::level::Level>,
        Option<regia_ecs::ecs::change_detection::Tick>,
    )>,
    findings: std::sync::Arc<[regia_scene::layers::LayerProblem]>,
}

/// [`problems`], validated again only after the open level, its data in the `LevelLibrary` or the
/// `LayerDefsLibrary` changed. What the Layers panel and `editor.layers_panel_state` list.
pub fn cached_problems(world: &mut World) -> std::sync::Arc<[regia_scene::layers::LayerProblem]> {
    let open = world.get_resource::<OpenLevel>().map(|open| open.id);
    let level = open.and_then(|id| {
        world
            .get_resource::<regia_scene::level::LevelLibrary>()?
            .get(id)
            .cloned()
    });
    let (Some(id), Some(level)) = (open, level) else {
        return problems(world).into();
    };
    let defs = world
        .get_resource_change_ticks::<regia_scene::layers::LayerDefsLibrary>()
        .map(|ticks| ticks.changed);
    if let Some(cache) = world.get_resource::<ProblemsCache>()
        && let Some((cached_id, cached_level, cached_defs)) = &cache.key
        && *cached_id == id
        && std::sync::Arc::ptr_eq(cached_level, &level)
        && *cached_defs == defs
    {
        return std::sync::Arc::clone(&cache.findings);
    }
    let findings: std::sync::Arc<[regia_scene::layers::LayerProblem]> = problems(world).into();
    world.insert_resource(ProblemsCache {
        key: Some((id, level, defs)),
        findings: std::sync::Arc::clone(&findings),
    });
    // A definitions edit later in this tick would leave their tick where it was read.
    world.increment_change_tick();
    findings
}

/// How many times [`problems`] validated a level on the calling thread: a counter for tests (the
/// panel must not re-validate an unchanged level every repaint, ADR-7326).
#[must_use]
pub fn validations() -> u64 {
    VALIDATIONS.with(std::cell::Cell::get)
}

fn state_json(world: &mut World) -> Value {
    let panel = world.get_resource::<LayersPanel>().cloned().unwrap_or_default();
    let findings: Vec<Value> = cached_problems(world)
        .iter()
        .map(|p| json!({ "reason": p.reason, "detail": p.detail, "fatal": p.fatal }))
        .collect();
    let rows: Vec<Value> = rows(world)
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "kind": match row.kind { LayerKind::EditorOnly => "editor_only", LayerKind::Runtime => "runtime" },
                "parent": row.parent,
                "requested": row.requested.as_str(),
                "effective": row.effective.as_str(),
                "members": row.members,
                "visible": row.effective == LayerState::Live,
                "loaded": row.effective != LayerState::Absent,
                "current": row.current,
            })
        })
        .collect();
    let warnings: Vec<String> = world
        .get_resource::<LayerTable>()
        .map(|table| table.warnings().to_vec())
        .unwrap_or_default();
    json!({
        "layers": rows,
        "current": panel.current,
        "warnings": warnings,
        "problems": findings,
        "session": panel.session.as_ref().map(|session| json!({
            "level": session.level.0.to_string(),
            "path": session.path.display().to_string(),
            "host": session.host.display().to_string(),
        })),
    })
}

fn panel_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state_json(world))
}

fn string_array(value: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    let names: Option<Vec<String>> = value
        .as_array()
        .and_then(|items| items.iter().map(|v| v.as_str().map(str::to_owned)).collect());
    let names =
        names.ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an array of names")))?;
    if let Some(bad) = names.iter().find(|name| !valid_layer_name(name)) {
        return Err(RpcError::invalid_params(format!(
            "`{bad}` is not a layer name ([a-z0-9_]{{1,48}})"
        )));
    }
    Ok(names)
}

fn panel_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(value) = params.get("current") {
        set_current(world, &string_array(value, "current")?)?;
    }
    let layer = params.get("layer").and_then(Value::as_str);
    let wants_layer = params.get("visible").is_some() || params.get("loaded").is_some();
    if wants_layer && layer.is_none() {
        return Err(RpcError::invalid_params("`visible` and `loaded` need `layer`"));
    }
    if let (Some(layer), Some(on)) = (layer, params.get("visible").and_then(Value::as_bool)) {
        set_visible(world, layer, on)?;
    }
    if let (Some(layer), Some(on)) = (layer, params.get("loaded").and_then(Value::as_bool)) {
        set_loaded(world, layer, on)?;
    }
    let add = match params.get("add_selection") {
        Some(value) => string_array(value, "add_selection")?,
        None => Vec::new(),
    };
    let remove = match params.get("remove_selection") {
        Some(value) => string_array(value, "remove_selection")?,
        None => Vec::new(),
    };
    let mut answer = state_json(world);
    if !add.is_empty() || !remove.is_empty() {
        let updated = change_selection(world, &add, &remove);
        answer = state_json(world);
        if let Some(object) = answer.as_object_mut() {
            object.insert("updated".into(), json!(updated));
        }
    }
    Ok(answer)
}

fn placed_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match params.get("action").and_then(Value::as_str) {
        Some("begin") => {
            let bits = params
                .get("entity")
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::invalid_params("`entity` is required"))?;
            let entity = Entity::try_from_bits(bits)
                .filter(|entity| world.get_entity(*entity).is_ok())
                .ok_or_else(|| {
                    RpcError::not_found(
                        format!("entity {bits} does not exist"),
                        "use scene.query_entities",
                    )
                })?;
            begin_placed_edit(world, entity)?;
        }
        Some("commit") => end_placed_edit(world, true)?,
        Some("discard") => end_placed_edit(world, false)?,
        _ => return Err(RpcError::invalid_params("`action` is begin, commit or discard")),
    }
    Ok(state_json(world))
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.layers_panel_state",
                tool: "layers_panel_state",
                description: "The Layers panel: each layer's states, members, visible and loaded toggles, the current layer set new entities join, table warnings and the placed level being edited.",
                params: &[],
                result: "{ layers: [{ name, kind, parent, requested, effective, members, visible, loaded, current }], current, warnings, problems, session }",
                mutating: false,
                permission: Permission::Read,
            },
            panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layers_panel_set",
                tool: "layers_panel_set",
                description: "Change what the Layers panel changes: the current layer set, a layer's visible or loaded toggle (working state, no file changes), or add the selection to layers and take it out (one undo step).",
                params: &[
                    ParamSchema {
                        name: "current",
                        kind: "array",
                        required: false,
                        description: "The current layer set; [] clears it.",
                    },
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: false,
                        description: "The layer `visible` and `loaded` apply to.",
                    },
                    ParamSchema {
                        name: "visible",
                        kind: "boolean",
                        required: false,
                        description: "Show (live) or hide (parked) the members.",
                    },
                    ParamSchema {
                        name: "loaded",
                        kind: "boolean",
                        required: false,
                        description: "Load (parked at least) or unload (absent) the members.",
                    },
                    ParamSchema {
                        name: "add_selection",
                        kind: "array",
                        required: false,
                        description: "Layers the selected entities join.",
                    },
                    ParamSchema {
                        name: "remove_selection",
                        kind: "array",
                        required: false,
                        description: "Layers the selected entities leave.",
                    },
                ],
                result: "{ layers, current, warnings, problems, session, updated? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.placed_edit",
                tool: "placed_edit",
                description: "Edit a placed level on its own: begin opens the level the entity places (the host must be saved), commit saves it and reopens the host so every copy updates, discard reopens the host.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "begin, commit or discard.",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: false,
                        description: "begin: the instance entity.",
                    },
                ],
                result: "{ layers, current, warnings, problems, session }",
                mutating: true,
                permission: Permission::EditScene,
            },
            placed_edit as Handler,
        ),
    ]
    .into_iter()
    .chain(levels_methods())
    .collect()
}

// ---------------------------------------------------------------------------
// Layer locks, layer files and several open levels (spec 37 § 12, ADR-7792)
// ---------------------------------------------------------------------------

/// The glyph a locked layer's row (and a member's Outliner row) shows.
pub const LOCK_GLYPH: &str = "\u{1f512}";

/// The locked layers: their members cannot be picked or box-selected in the viewport (the
/// Outliner still selects them). Editor-session state like an entity lock: never in a file, never
/// an undo step.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerLocks {
    /// Locked layer names.
    pub locked: BTreeSet<String>,
}

/// Whether `layer` is locked.
#[must_use]
pub fn is_layer_locked(world: &World, layer: &str) -> bool {
    world
        .get_resource::<LayerLocks>()
        .is_some_and(|locks| locks.locked.contains(layer))
}

/// Whether `entity` is a member of a locked layer (no locks: no component read).
#[must_use]
pub fn layer_locked(world: &World, entity: Entity) -> bool {
    let Some(locks) = world
        .get_resource::<LayerLocks>()
        .filter(|l| !l.locked.is_empty())
    else {
        return false;
    };
    world
        .get::<LayerMembership>(entity)
        .is_some_and(|membership| membership.layers.iter().any(|layer| locks.locked.contains(layer)))
}

/// The lock glyph for `entity`'s Outliner row when a layer it belongs to is locked.
#[must_use]
pub fn layer_lock_glyph(world: &World, entity: Entity) -> Option<&'static str> {
    layer_locked(world, entity).then_some(LOCK_GLYPH)
}

/// Locks or unlocks a layer; locking drops its members from the selection. Not an undo step.
/// Answers whether anything changed.
///
/// # Errors
/// `layer_unknown` when the open level has no such layer.
pub fn set_layer_lock(world: &mut World, layer: &str, on: bool) -> Result<bool, RpcError> {
    let known = world
        .get_resource::<LayerTable>()
        .is_some_and(|table| table.contains(layer));
    if !known {
        return Err(reason(
            "layer_unknown",
            NOT_FOUND,
            format!("`{layer}` is not a layer of this level"),
        ));
    }
    let mut locks = world.get_resource_or_insert_with(LayerLocks::default);
    let changed = if on {
        locks.locked.insert(layer.to_owned())
    } else {
        locks.locked.remove(layer)
    };
    if on && changed {
        let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
        let kept: Vec<Entity> = selection
            .entities()
            .iter()
            .copied()
            .filter(|e| !crate::viewport_ops::is_locked(world, *e))
            .collect();
        selection.clear();
        for entity in kept {
            selection.select(entity, crate::selection::SelectMode::Add);
        }
        world.insert_resource(selection);
    }
    Ok(changed)
}

/// The host level's layers stored in their own files.
fn separated_of_host(world: &mut World) -> BTreeSet<String> {
    level_io::sync_open_levels(world);
    let host = world.get_resource::<OpenLevel>().map(|open| open.id);
    world
        .get_resource::<level_io::OpenLevels>()
        .and_then(|model| host.and_then(|id| model.separated.get(&id).cloned()))
        .unwrap_or_default()
}

fn levels_error(err: level_io::LevelsError) -> RpcError {
    let kind = match err.code {
        "level_not_open" | "document_unknown" | "layer_unknown" | "level_not_found" => NOT_FOUND,
        _ => INVALID_PARAMS,
    };
    RpcError::new(
        kind,
        err.code,
        err.message,
        "editor.levels_open { op: \"list\" } lists the open documents; book/agents/levels-layers.md lists the reasons",
    )
}

/// One level's group in the Outliner: the level's (or layer file's) row and save badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelGroupRow {
    /// The document.
    pub document: level_io::LevelDocument,
    /// Its row label (the file name, `untitled` for a new host).
    pub label: String,
    /// It has unsaved edits (the `*` badge).
    pub dirty: bool,
    /// It is the active level (new entities land in it).
    pub active: bool,
}

/// The level rows the Outliner groups its entities under, host first, with save badges. The
/// Outliner calls it (one appended call; spec 37 § 12).
pub fn level_group_rows(world: &mut World) -> Vec<LevelGroupRow> {
    crate::level_dirty::feed_documents(world);
    level_group_rows_as_fed(world)
}

/// [`level_group_rows`] without a feed pass first: the save badges as the per-frame feed
/// (`level_dirty::feed_on_history_change`) last left them. Cheap enough for every frame.
pub fn level_group_rows_as_fed(world: &mut World) -> Vec<LevelGroupRow> {
    let active = level_io::active_level(world);
    let docs = level_io::documents(world);
    let stack = world.get_resource::<UndoStack>();
    docs.into_iter()
        .map(|doc| {
            let label = doc
                .file()
                .and_then(|file| file.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "untitled".to_owned());
            let dirty = stack.is_some_and(|stack| stack.is_document_dirty(doc.id));
            let is_active =
                !matches!(doc.kind, level_io::DocumentKind::Layer(_)) && Some(doc.level) == active;
            LevelGroupRow {
                document: doc,
                label,
                dirty,
                active: is_active,
            }
        })
        .collect()
}

/// The heading text of a level group: the file name, ` *` when unsaved, `(active)` for the active
/// level.
#[must_use]
pub fn level_group_heading(row: &LevelGroupRow) -> String {
    let mut text = row.label.clone();
    if row.dirty {
        text.push_str(" *");
    }
    if row.active {
        text.push_str("  (active)");
    }
    text
}

/// Heading of the group of entities that belong to no open level (editor-internal or not yet
/// adopted by a save).
pub const NO_LEVEL_GROUP: &str = "Not in an open level";

/// The Outliner's tree grouped by open document (spec 37 § 12): one `(heading, roots)` group per
/// level and layer file, in [`level_group_rows`] order, then [`NO_LEVEL_GROUP`]; the same shape as
/// `outliner::group_by_category`, so `outliner::flatten_groups` draws it. Roots keep their
/// children. A root goes to its level's group, or to the layer file its membership moves it to.
pub fn group_by_level(
    world: &mut World,
    nodes: &[crate::outliner::HierarchyNode],
) -> Vec<(String, Vec<crate::outliner::HierarchyNode>)> {
    let rows = level_group_rows(world);
    let separated = world
        .get_resource::<level_io::OpenLevels>()
        .map(|model| model.separated.clone())
        .unwrap_or_default();
    let mut groups: Vec<(String, Vec<crate::outliner::HierarchyNode>)> = rows
        .iter()
        .map(|row| (level_group_heading(row), Vec::new()))
        .collect();
    let mut loose = Vec::new();
    for node in nodes {
        let document = root_document(world, node.entity, &separated);
        match document.and_then(|doc| rows.iter().position(|row| row.document.id == doc)) {
            Some(index) => groups[index].1.push(node.clone()),
            None => loose.push(node.clone()),
        }
    }
    if !loose.is_empty() {
        groups.push((NO_LEVEL_GROUP.to_owned(), loose));
    }
    groups
}

/// The document an Outliner root belongs to: its level's id, or the id of the layer file its
/// membership moves it to (`separated`: per level, its layers stored in their own files).
fn root_document(
    world: &World,
    entity: Entity,
    separated: &std::collections::BTreeMap<LevelId, BTreeSet<String>>,
) -> Option<u128> {
    let level = world.get::<regia_scene::level::LevelMember>(entity)?.level();
    let layer = world.get::<LayerMembership>(entity).and_then(|membership| {
        separated
            .get(&level)
            .and_then(|set| set.iter().find(|layer| membership.layers.contains(layer)))
    });
    Some(layer.map_or(level.0.as_u128(), |layer| {
        level_io::layer_document_id(level, layer)
    }))
}

/// The Outliner's per-level grouping (spec 37 § 12, P-097): the open documents' rows (save badge,
/// active marker) and, per root, the document it is listed under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelGrouping {
    /// One row per open document, host first ([`level_group_rows`] order).
    pub rows: Vec<LevelGroupRow>,
    /// Each root's document id; a root that is in no listed document is absent (it goes to the
    /// [`NO_LEVEL_GROUP`] group).
    pub roots: std::collections::HashMap<Entity, u128>,
}

/// The grouping the Outliner's hierarchy view draws while more than one document is open (a
/// level beside the host, or a layer in its own file); `None` while only the host is open, so a
/// single level keeps its plain tree, and while a prefab is open (prefab edit mode). Reads no
/// file and runs no feed pass (the badges are the per-frame feed's), so the Outliner may call it
/// every frame.
pub fn outliner_level_grouping(
    world: &mut World,
    nodes: &[crate::outliner::HierarchyNode],
) -> Option<LevelGrouping> {
    let _span = tracing::trace_span!(target: "regia_editor", "outliner_level_grouping").entered();
    world.get_resource::<OpenLevel>()?;
    // A prefab session: the open level is the session's (its contents belong to no level).
    if crate::prefab_edit::in_prefab_mode(world) {
        return None;
    }
    let rows = level_group_rows_as_fed(world);
    if rows.len() < 2 {
        return None;
    }
    let separated = world
        .get_resource::<level_io::OpenLevels>()
        .map(|model| model.separated.clone())
        .unwrap_or_default();
    let roots = nodes
        .iter()
        .filter_map(|node| {
            root_document(world, node.entity, &separated)
                .filter(|doc| rows.iter().any(|row| row.document.id == *doc))
                .map(|doc| (node.entity, doc))
        })
        .collect();
    Some(LevelGrouping { rows, roots })
}

/// Draws one level group header (the Outliner's per-level row): label, `*` when unsaved and the
/// active marker. Text colors come from the theme tokens.
pub fn level_group_header(ui: &mut egui::Ui, world: &World, row: &LevelGroupRow) -> egui::Response {
    let tokens = crate::theme::tokens(world);
    let text = level_group_heading(row);
    let color = if row.active { tokens.signal } else { tokens.strong };
    ui.label(egui::RichText::new(text).color(color))
}

/// A user action of the panel's Open levels section.
enum LevelsAction {
    Open(String),
    Save(u128),
    Close(LevelId),
    Active(Option<LevelId>),
    Path(String),
}

/// The text of the Open levels section's path field.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenLevelsForm {
    /// The level to open beside the host, relative to the project root (or absolute).
    pub path: String,
}

/// Resolves a path parameter: absolute as is, else under the project root (else as given).
fn project_path(world: &World, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    crate::mesh_source::project_root(world)
        .map(|root| root.join(path))
        .unwrap_or_else(|| path.to_path_buf())
}

fn run_levels_action(world: &mut World, action: LevelsAction) -> Result<(), RpcError> {
    match action {
        LevelsAction::Open(path) => {
            let path = project_path(world, &path);
            level_io::open_level_document(world, &path).map(|_| ())
        }
        LevelsAction::Save(doc) => level_io::save_level_document(world, doc).map(|_| ()),
        LevelsAction::Close(level) => level_io::close_level_document(world, level, false),
        LevelsAction::Active(level) => level_io::set_active_level(world, level),
        LevelsAction::Path(path) => {
            world.get_resource_or_insert_with(OpenLevelsForm::default).path = path;
            Ok(())
        }
    }
    .map_err(levels_error)
}

/// The Open levels section: every open document with its save badge, Save and Close, the active
/// level choice and a path field to open another level beside the host. Every control is a
/// focusable egui widget (Tab reaches it, Space or Enter runs it).
fn open_levels_section(ui: &mut egui::Ui, world: &mut World, actions: &mut Vec<Action>) {
    if world.get_resource::<OpenLevel>().is_none() {
        return;
    }
    // The badges as the editor's per-frame feed (`level_dirty::feed_on_history_change`) left
    // them: a feed pass here scanned every member of every open level on each repaint, so a steady
    // frame allocated with the level's size (lane v06-editor-b-reds, `panel_frame_clones`).
    let rows = level_group_rows_as_fed(world);
    let form = world
        .get_resource::<OpenLevelsForm>()
        .cloned()
        .unwrap_or_default();
    ui.collapsing("Open levels", |ui| {
        for row in &rows {
            ui.horizontal(|ui| {
                if !matches!(row.document.kind, level_io::DocumentKind::Layer(_)) {
                    let mut active = row.active;
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::LEVELS_ACTIVE,
                        &mut active,
                    )
                    .changed()
                        && active
                    {
                        let level =
                            (row.document.kind != level_io::DocumentKind::Host).then_some(row.document.level);
                        actions.push(Action::Levels(LevelsAction::Active(level)));
                    }
                } else {
                    ui.add_space(ui.spacing().indent);
                }
                level_group_header(ui, world, row);
                if crate::agent_parity::action_enabled(ui, row.dirty, &crate::agent_parity::LEVELS_SAVE, None)
                    .clicked()
                {
                    actions.push(Action::Levels(LevelsAction::Save(row.document.id)));
                }
                if row.document.kind == level_io::DocumentKind::Level
                    && crate::agent_parity::action_button(ui, &crate::agent_parity::LEVELS_CLOSE).clicked()
                {
                    actions.push(Action::Levels(LevelsAction::Close(row.document.level)));
                }
            });
        }
        ui.horizontal(|ui| {
            let mut path = form.path.clone();
            let field = ui.text_edit_singleline(&mut path);
            let field =
                crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Level to open beside the host");
            if field.changed() {
                actions.push(Action::Levels(LevelsAction::Path(path.clone())));
            }
            let enter = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if (crate::agent_parity::action_button(ui, &crate::agent_parity::LEVELS_OPEN).clicked() || enter)
                && !path.trim().is_empty()
            {
                actions.push(Action::Levels(LevelsAction::Open(path.trim().to_owned())));
            }
        });
    });
}

/// Parses a `level` parameter: `host`, a level id, or the path of an open level.
fn level_param(world: &mut World, text: &str) -> Result<Option<LevelId>, RpcError> {
    if text == "host" {
        return Ok(None);
    }
    if let Ok(guid) = text.parse::<Guid>() {
        return Ok(Some(LevelId(guid)));
    }
    let wanted = project_path(world, text);
    level_io::documents(world)
        .into_iter()
        .find(|doc| {
            !matches!(doc.kind, level_io::DocumentKind::Layer(_))
                && doc.path.as_deref().is_some_and(|p| {
                    p == wanted.as_path()
                        || regia_scene::level_dir::level_paths(p).1
                            == regia_scene::level_dir::level_paths(&wanted).1
                })
        })
        .map(|doc| Some(doc.level))
        .ok_or_else(|| {
            levels_error(level_io::LevelsError {
                code: "level_not_open",
                message: format!("{text} is not an open level"),
            })
        })
}

fn levels_json(world: &mut World) -> Value {
    let rows = level_group_rows(world);
    let host = world
        .get_resource::<OpenLevel>()
        .map(|open| open.id.0.to_string());
    let active = level_io::active_level(world).map(|id| id.0.to_string());
    let documents: Vec<Value> = rows
        .iter()
        .map(|row| {
            let (kind, layer) = match &row.document.kind {
                level_io::DocumentKind::Host => ("host", None),
                level_io::DocumentKind::Level => ("level", None),
                level_io::DocumentKind::Layer(layer) => ("layer", Some(layer.clone())),
            };
            json!({
                "id": row.document.id_text(),
                "level": row.document.level.0.to_string(),
                "kind": kind,
                "layer": layer,
                "path": row.document.path.as_ref().map(|p| p.display().to_string()),
                "file": row.document.file().map(|p| p.display().to_string()),
                "dirty": row.dirty,
                "active": row.active,
            })
        })
        .collect();
    let locked: Vec<String> = world
        .get_resource::<LayerLocks>()
        .map(|locks| locks.locked.iter().cloned().collect())
        .unwrap_or_default();
    json!({ "host": host, "active": active, "documents": documents, "locked_layers": locked })
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

fn required_bool(params: &Value, key: &str) -> Result<bool, RpcError> {
    params
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (boolean) is required")))
}

fn layer_lock_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let layer = required_str(params, "layer")?.to_owned();
    let on = required_bool(params, "on")?;
    let changed = set_layer_lock(world, &layer, on)?;
    let mut out = levels_json(world);
    out["layer"] = json!(layer);
    out["locked"] = json!(on);
    out["changed"] = json!(changed);
    Ok(out)
}

fn layer_file_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let layer = required_str(params, "layer")?.to_owned();
    let separate = required_bool(params, "separate")?;
    let report = level_io::set_layer_file(world, &layer, separate).map_err(levels_error)?;
    let paths = |list: &[PathBuf]| -> Vec<String> { list.iter().map(|p| p.display().to_string()).collect() };
    let mut out = levels_json(world);
    out["layer"] = json!(layer);
    out["separate"] = json!(separate);
    out["written"] = json!(paths(&report.written));
    out["deleted"] = json!(paths(&report.deleted));
    out["separated"] = json!(separated_of_host(world));
    Ok(out)
}

fn levels_open_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or("list");
    let mut written: Vec<String> = Vec::new();
    match op {
        "list" => {}
        "open" => {
            let path = project_path(world, required_str(params, "path")?);
            level_io::open_level_document(world, &path).map_err(levels_error)?;
        }
        "close" => {
            let text = required_str(params, "level")?.to_owned();
            let level = level_param(world, &text)?.ok_or_else(|| {
                levels_error(level_io::LevelsError {
                    code: "host_level",
                    message: "the host level closes with File ▸ Open or New".into(),
                })
            })?;
            let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
            level_io::close_level_document(world, level, discard).map_err(levels_error)?;
        }
        "save" => {
            let text = required_str(params, "document")?;
            let doc = text.parse::<Guid>().map_err(|_| {
                RpcError::invalid_params(format!(
                    "`document` must be a document id from the list, not `{text}`"
                ))
            })?;
            written = level_io::save_level_document(world, doc.as_u128())
                .map_err(levels_error)?
                .iter()
                .map(|p| p.display().to_string())
                .collect();
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "`op` is list, open, close or save, not `{other}`"
            )));
        }
    }
    let mut out = levels_json(world);
    if op == "save" {
        out["written"] = json!(written);
    }
    Ok(out)
}

fn active_level_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(text) = params
        .get("set")
        .or_else(|| params.get("level"))
        .and_then(Value::as_str)
    {
        let text = text.to_owned();
        let level = level_param(world, &text)?;
        level_io::set_active_level(world, level).map_err(levels_error)?;
    }
    Ok(levels_json(world))
}

/// The agent methods of layer locks, layer files and the open levels (ADR-7792).
fn levels_methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.layer_lock",
                tool: "layer_lock",
                description: "Lock or unlock a layer: its members cannot be picked or box-selected in the viewport (the Outliner still selects them). Editor-session state, not an undo step, no file changes.",
                params: &[
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: true,
                        description: "The layer.",
                    },
                    ParamSchema {
                        name: "on",
                        kind: "boolean",
                        required: true,
                        description: "true locks, false unlocks.",
                    },
                ],
                result: "{ layer, locked, changed, host, active, documents, locked_layers }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layer_lock_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layer_file",
                tool: "layer_file",
                description: "Store a layer of the open level in its own file (`<level>.<layer>.layer.ron` next to the level, so two people can edit different layers) or merge its file back into the level. Works on the saved files; merging back an unchanged split gives the level's original bytes.",
                params: &[
                    ParamSchema {
                        name: "layer",
                        kind: "string",
                        required: true,
                        description: "The layer.",
                    },
                    ParamSchema {
                        name: "separate",
                        kind: "boolean",
                        required: true,
                        description: "true moves the members to the layer file, false merges it back.",
                    },
                ],
                result: "{ layer, separate, written, deleted, separated, host, active, documents, locked_layers }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layer_file_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.levels_open",
                tool: "levels_open",
                description: "The open documents: the host level, levels opened beside it and layer files, each with its own unsaved state and Save. op list (default), open (a level beside the host), close (a level beside the host; its unsaved edits refuse unless discard), save (one document: writes only its file).",
                params: &[
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: false,
                        description: "list, open, close or save.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "open: the level (.level.ron or .level.d), project-relative or absolute.",
                    },
                    ParamSchema {
                        name: "level",
                        kind: "string",
                        required: false,
                        description: "close: the level id or path.",
                    },
                    ParamSchema {
                        name: "document",
                        kind: "string",
                        required: false,
                        description: "save: a document id from the list.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "close: drop the level's unsaved edits (default false).",
                    },
                ],
                result: "{ host, active, documents: [{ id, level, kind, layer, path, file, dirty, active }], locked_layers, written? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            levels_open_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.active_level",
                tool: "active_level",
                description: "The active level: new entities land in it. set: `host`, a level id or the path of an open level; without set it answers the current choice.",
                params: &[ParamSchema {
                    name: "set",
                    kind: "string",
                    required: false,
                    description: "host, a level id or an open level's path.",
                }],
                result: "{ host, active, documents, locked_layers }",
                mutating: true,
                permission: Permission::EditScene,
            },
            active_level_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::App;
    use regia_scene::layers::{LayerDefs, LayerDefsLibrary, LayerParked};
    use regia_scene::level::{Level, LevelLibrary, LoadMode, open_level};
    use regia_test_utils::unique_temp_dir;

    const DEFS_ID: Guid = Guid::from_u128(0xd0);
    const LEVEL: LevelId = LevelId(Guid::from_u128(0x1e));
    const MEMBER: Guid = Guid::from_u128(0x20);
    const PLAIN: Guid = Guid::from_u128(0x21);

    fn record(guid: Guid, layers: &str) -> String {
        format!(
            r#"(guid: "{guid}", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), {layers} }}),"#
        )
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "blockout", kind: EditorOnly, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut()
            .resource_mut::<LayerDefsLibrary>()
            .insert(DEFS_ID, defs);
        let text = format!(
            r#"SceneFile(format_version: {v}, entities: [{a} {b}], resources: {{
                "regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: []),
            }})"#,
            v = regia_scene::format::SCENE_FORMAT_VERSION,
            a = record(
                MEMBER,
                r#""regia_scene::layers::LayerMembership": (layers: ["night"]),"#
            ),
            b = record(PLAIN, ""),
        );
        let level = Level::from_ron_str(LEVEL, &text).expect("level");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
        app.update();
        app
    }

    fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("registered");
        handler(world, params)
    }

    fn entity(app: &mut App, guid: Guid) -> Entity {
        entity_of(app.world_mut(), guid).expect("entity")
    }

    #[test]
    fn the_rows_show_the_table() {
        let mut app = app();
        let out = call(app.world_mut(), "editor.layers_panel_state", &json!({})).expect("state");
        let layers = out["layers"].as_array().expect("layers");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0]["name"], "blockout");
        assert_eq!(layers[0]["kind"], "editor_only");
        assert_eq!(layers[1]["members"], 1);
        assert_eq!(layers[1]["visible"], true);
    }

    #[test]
    fn the_toggles_park_and_unload_and_change_no_file_state() {
        let mut app = app();
        let member = entity(&mut app, MEMBER);
        call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "layer": "night", "visible": false }),
        )
        .expect("hide");
        app.update();
        assert!(app.world().get::<LayerParked>(member).is_some());

        call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "layer": "night", "loaded": false }),
        )
        .expect("unload");
        app.update();
        assert!(entity_of(app.world_mut(), MEMBER).is_none());

        let out = call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "layer": "night", "loaded": true }),
        )
        .expect("load");
        app.update();
        assert_eq!(out["layers"][1]["requested"], "parked");
        assert!(entity_of(app.world_mut(), MEMBER).is_some());
    }

    #[test]
    fn a_toggle_of_an_unknown_layer_names_the_reason() {
        let mut app = app();
        let err = call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "layer": "ghost", "visible": true }),
        )
        .expect_err("unknown");
        assert_eq!(err.data.expect("data")["code"], "layer_unknown");
        let err = call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "current": ["ghost"] }),
        )
        .expect_err("unknown");
        assert_eq!(err.data.expect("data")["code"], "layer_unknown");
        let err = call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "visible": true }),
        )
        .expect_err("no layer");
        assert!(err.message.contains("layer"));
    }

    #[test]
    fn placing_with_a_current_set_joins_it_and_redo_keeps_it() {
        use crate::command::SpawnEntities;
        let mut app = app();
        call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "current": ["night"] }),
        )
        .expect("set");
        let guid = Guid::from_u128(0x99);
        let mut command =
            SpawnEntities::new_entity(app.world(), guid, "Crate", regia_scene::Transform::default())
                .expect("spawn");
        command.apply(app.world_mut());
        let placed = entity_of(app.world_mut(), guid).expect("placed");
        let joined = app.world().get::<LayerMembership>(placed).expect("membership");
        assert_eq!(joined.layers, ["night"]);

        command.revert(app.world_mut());
        call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "current": [] }),
        )
        .expect("clear");
        command.apply(app.world_mut());
        let placed = entity_of(app.world_mut(), guid).expect("again");
        assert!(
            app.world().get::<LayerMembership>(placed).is_some(),
            "redo restores it"
        );

        let other = Guid::from_u128(0x9a);
        let mut command =
            SpawnEntities::new_entity(app.world(), other, "Free", regia_scene::Transform::default())
                .expect("spawn");
        command.apply(app.world_mut());
        let free = entity_of(app.world_mut(), other).expect("free");
        assert!(
            app.world().get::<LayerMembership>(free).is_none(),
            "a cleared set adds nothing"
        );
    }

    #[test]
    fn the_selection_joins_and_leaves_in_one_undo_step() {
        let mut app = app();
        let plain = entity(&mut app, PLAIN);
        let member = entity(&mut app, MEMBER);
        app.world_mut().init_resource::<Selection>();
        let mut selection = Selection::default();
        selection.select(plain, crate::selection::SelectMode::Replace);
        selection.select(member, crate::selection::SelectMode::Add);
        app.world_mut().insert_resource(selection);
        app.world_mut().insert_resource(UndoStack::new());

        let out = call(
            app.world_mut(),
            "editor.layers_panel_set",
            &json!({ "add_selection": ["night"] }),
        )
        .expect("add");
        assert_eq!(out["updated"], 1, "the member was in already");
        assert!(app.world().get::<LayerMembership>(plain).is_some());
        app.update();
        let row = rows(app.world());
        assert_eq!(
            row[1].members, 1,
            "members count the level's records, not the working copy"
        );

        let mut stack = app.world_mut().remove_resource::<UndoStack>().expect("stack");
        stack.undo(app.world_mut());
        app.world_mut().insert_resource(stack);
        assert!(app.world().get::<LayerMembership>(plain).is_none());
    }

    #[test]
    fn a_placed_level_opens_alone_and_commit_saves_it_and_reopens_the_host() {
        let root = unique_temp_dir("layers_panel_placed");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let sub_id = Guid::from_u128(0xe0);
        let level_text = |records: &str| {
            format!(
                "SceneFile(format_version: {}, entities: [{records}], resources: {{}})",
                regia_scene::format::SCENE_FORMAT_VERSION
            )
        };
        let sub = assets.join("sub.level.ron");
        std::fs::write(&sub, level_text(&record(Guid::from_u128(0x10), ""))).expect("sub");
        let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(sub_id));
        std::fs::write(
            regia_asset::meta_path_for(&sub),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("meta");
        let host = assets.join("host.level.ron");
        let instance = Guid::from_u128(0x30);
        std::fs::write(
            &host,
            level_text(&record(
                instance,
                &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{sub_id}"),"#),
            )),
        )
        .expect("host");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let open = level_io::open_level_file(app.world_mut(), &host).expect("open host");
        app.world_mut().insert_resource(open);
        app.update();
        let placed = entity_of(app.world_mut(), instance).expect("instance");

        let out = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "begin", "entity": placed.to_bits() }),
        )
        .expect("begin");
        assert!(
            out["session"]["path"]
                .as_str()
                .expect("path")
                .ends_with("sub.level.ron")
        );
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(sub.as_path())
        );

        let out = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "commit" }),
        )
        .expect("commit");
        assert!(out["session"].is_null());
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(host.as_path())
        );
        let err = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "discard" }),
        )
        .expect_err("nothing open");
        assert_eq!(err.data.expect("data")["code"], "not_placed");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn end_placed_edit_restores_the_session_when_commit_cannot_write() {
        let root = unique_temp_dir("layers_panel_placed_fail");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let sub_id = Guid::from_u128(0xe1);
        let level_text = |records: &str| {
            format!(
                "SceneFile(format_version: {}, entities: [{records}], resources: {{}})",
                regia_scene::format::SCENE_FORMAT_VERSION
            )
        };
        let sub = assets.join("sub.level.ron");
        std::fs::write(&sub, level_text(&record(Guid::from_u128(0x11), ""))).expect("sub");
        let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(sub_id));
        std::fs::write(
            regia_asset::meta_path_for(&sub),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("meta");
        let host = assets.join("host.level.ron");
        let instance = Guid::from_u128(0x31);
        std::fs::write(
            &host,
            level_text(&record(
                instance,
                &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{sub_id}"),"#),
            )),
        )
        .expect("host");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let open = level_io::open_level_file(app.world_mut(), &host).expect("open host");
        app.world_mut().insert_resource(open);
        app.update();
        let placed = entity_of(app.world_mut(), instance).expect("instance");
        call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "begin", "entity": placed.to_bits() }),
        )
        .expect("begin");

        // The placed level's file turns into a non-empty directory: the atomic rename cannot
        // replace it, so the commit's save fails.
        std::fs::remove_file(&sub).expect("remove sub");
        std::fs::create_dir_all(sub.join("blocker")).expect("blocker");
        call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "commit" }),
        )
        .expect_err("the save fails");
        let panel = app.world().resource::<LayersPanel>();
        assert_eq!(
            panel.session.as_ref().map(|s| s.path.clone()),
            Some(sub.clone()),
            "the session survives a failed commit"
        );
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(sub.as_path()),
            "the placed level stays open"
        );

        // Once the path is writable again, the same session commits and the host comes back.
        std::fs::remove_dir_all(&sub).expect("unblock");
        let out = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "commit" }),
        )
        .expect("retry commit");
        assert!(out["session"].is_null());
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(host.as_path())
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    /// Corr editor-a finding 1: a session whose placed level is no longer open (another level was
    /// opened in the middle of it) must not save the open level over the placed file. Before the
    /// fix, Commit wrote `other.level.ron`'s entities into `sub.level.ron`.
    #[test]
    fn commit_after_opening_another_level_never_overwrites_the_placed_file() {
        let root = unique_temp_dir("layers_panel_placed_stale");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let sub_id = Guid::from_u128(0xe2);
        let level_text = |records: &str| {
            format!(
                "SceneFile(format_version: {}, entities: [{records}], resources: {{}})",
                regia_scene::format::SCENE_FORMAT_VERSION
            )
        };
        let sub = assets.join("sub.level.ron");
        std::fs::write(&sub, level_text(&record(Guid::from_u128(0x12), ""))).expect("sub");
        let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(sub_id));
        std::fs::write(
            regia_asset::meta_path_for(&sub),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("meta");
        let host = assets.join("host.level.ron");
        let instance = Guid::from_u128(0x32);
        std::fs::write(
            &host,
            level_text(&record(
                instance,
                &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{sub_id}"),"#),
            )),
        )
        .expect("host");
        let other = assets.join("other.level.ron");
        std::fs::write(&other, level_text(&record(Guid::from_u128(0x77), ""))).expect("other");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let open = level_io::open_level_file(app.world_mut(), &host).expect("open host");
        app.world_mut().insert_resource(open);
        app.update();
        let placed = entity_of(app.world_mut(), instance).expect("instance");
        call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "begin", "entity": placed.to_bits() }),
        )
        .expect("begin");
        let sub_before = std::fs::read(&sub).expect("sub bytes");

        // The user leaves the session through File > Open.
        let open = level_io::open_level_file(app.world_mut(), &other).expect("open other");
        app.world_mut().insert_resource(open);
        app.update();

        let err = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "commit" }),
        )
        .expect_err("the placed level is not open");
        assert_eq!(err.data.expect("data")["code"], "placed_level_not_open");
        assert_eq!(
            std::fs::read(&sub).expect("sub bytes"),
            sub_before,
            "the placed file keeps its own entities"
        );
        assert!(
            app.world().resource::<LayersPanel>().session.is_none(),
            "the stale session ends"
        );
        assert_eq!(
            app.world().resource::<OpenLevel>().path.as_deref(),
            Some(other.as_path()),
            "what the user opened stays open"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A stale session also ends through Discard, even when the host it would reopen is gone.
    #[test]
    fn discard_ends_a_stale_session_without_the_host() {
        let mut app = app();
        let other = unique_temp_dir("layers_panel_placed_discard").join("other.level.ron");
        app.world_mut().insert_resource(OpenLevel {
            id: LevelId(Guid::from_u128(0x99)),
            path: Some(other),
        });
        let session = PlacedEditSession {
            level: LevelId(Guid::from_u128(0xe3)),
            path: PathBuf::from("/nonexistent/sub.level.ron"),
            host: PathBuf::from("/nonexistent/host.level.ron"),
        };
        app.world_mut()
            .get_resource_or_insert_with(LayersPanel::default)
            .session = Some(session);
        let result = end_placed_edit(app.world_mut(), false);
        assert!(
            app.world().resource::<LayersPanel>().session.is_none(),
            "{result:?}"
        );
    }

    #[test]
    fn begin_refuses_what_places_nothing() {
        let mut app = app();
        let plain = entity(&mut app, PLAIN);
        let err = call(
            app.world_mut(),
            "editor.placed_edit",
            &json!({ "action": "begin", "entity": plain.to_bits() }),
        )
        .expect_err("not placed");
        assert_eq!(err.data.expect("data")["code"], "not_placed");
    }

    #[test]
    fn the_panel_draws() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            layers_panel(ui, app.world_mut());
        });
        output.textures_delta.clear();
        assert!(app.world().contains_resource::<LayersPanel>());
    }

    /// The findings are validated once for an unchanged level and again after its definitions
    /// or the level itself change (ADR-7326).
    #[test]
    fn the_findings_are_validated_once_until_the_level_or_its_definitions_change() {
        let mut app = app();
        // The editor's open level (the shell sets it when a level opens).
        app.world_mut().insert_resource(OpenLevel {
            id: LEVEL,
            path: None,
        });
        let ctx = egui::Context::default();
        let repaint = |app: &mut App| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                layers_panel(ui, app.world_mut());
            });
            output.textures_delta.clear();
        };
        let start = validations();
        for _ in 0..10 {
            repaint(&mut app);
        }
        assert_eq!(validations() - start, 1, "ten repaints validate once");
        let state = call(app.world_mut(), "editor.layers_panel_state", &json!({})).expect("state");
        assert_eq!(state["problems"], json!([]));

        // The bound definitions lose `night`: the member's layer is unknown now.
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "blockout", kind: EditorOnly, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut()
            .resource_mut::<LayerDefsLibrary>()
            .insert(DEFS_ID, defs);
        let start = validations();
        for _ in 0..10 {
            repaint(&mut app);
        }
        assert_eq!(validations() - start, 1, "a definitions edit validates once more");
        let state = call(app.world_mut(), "editor.layers_panel_state", &json!({})).expect("state");
        assert_eq!(state["problems"][0]["reason"], "layer_unknown", "{state}");

        // The level is replaced in the library (a reload): validated again.
        let level = app
            .world()
            .resource::<LevelLibrary>()
            .get(LEVEL)
            .map(|level| Level::clone(level))
            .expect("level");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        let start = validations();
        for _ in 0..10 {
            repaint(&mut app);
        }
        assert_eq!(validations() - start, 1, "a new level validates once more");
    }
}

/// Layer locks, layer files and several open levels (ADR-7792): the acceptance checks of P-096
/// and P-097, through the agent methods the panel's controls call.
#[cfg(test)]
mod levels_tests {
    use super::*;
    use regia_app::prelude::App;
    use regia_scene::layers::{LayerDefs, LayerDefsLibrary};
    use regia_scene::level::{Level, LevelLibrary, LevelMember, LoadMode, open_level};
    use regia_scene::prelude::Transform;
    use regia_test_utils::unique_temp_dir;

    const DEFS_ID: Guid = Guid::from_u128(0x7792_00d0);
    const HOST_E: Guid = Guid::from_u128(0x7792_0010);
    const A_E: Guid = Guid::from_u128(0x7792_0020);
    const B_E: Guid = Guid::from_u128(0x7792_0030);
    const MEMBER: Guid = Guid::from_u128(0x7792_0040);
    const MEMBER_2: Guid = Guid::from_u128(0x7792_0041);

    fn record(guid: Guid, name: &str, extra: &str) -> String {
        format!(
            r#"(guid: "{guid}", name: "{name}", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), {extra} }}),"#
        )
    }

    fn level_text(records: &str, resources: &str) -> String {
        format!(
            "SceneFile(format_version: {}, entities: [{records}], resources: {{{resources}}})",
            regia_scene::format::SCENE_FORMAT_VERSION
        )
    }

    fn write_level(path: &Path, id: u128, text: &str) {
        std::fs::write(path, text).expect("level");
        let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(Guid::from_u128(id)));
        std::fs::write(
            regia_asset::meta_path_for(path),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("meta");
    }

    fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("registered");
        handler(world, params)
    }

    fn code(err: RpcError) -> String {
        err.data.expect("data")["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// A project with a host level (one entity, a `night` member, the layer binding) and two
    /// more levels `a` and `b`, the host open.
    fn project(test: &str) -> (App, PathBuf, [PathBuf; 3]) {
        let root = unique_temp_dir(test);
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let host = assets.join("host.level.ron");
        let binding =
            format!(r#""regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: []),"#);
        let member = r#""regia_scene::layers::LayerMembership": (layers: ["night"]),"#;
        write_level(
            &host,
            0x7792_0001,
            &level_text(
                &format!(
                    "{}{}{}",
                    record(MEMBER, "lamp", member),
                    record(HOST_E, "host_thing", ""),
                    record(MEMBER_2, "lamp_2", member)
                ),
                &binding,
            ),
        );
        let a = assets.join("a.level.ron");
        write_level(&a, 0x7792_000a, &level_text(&record(A_E, "a_thing", ""), ""));
        let b = assets.join("b.level.ron");
        write_level(&b, 0x7792_000b, &level_text(&record(B_E, "b_thing", ""), ""));

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::mesh_source::register_level_types(app.world_mut());
        app.world_mut().insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut()
            .resource_mut::<LayerDefsLibrary>()
            .insert(DEFS_ID, defs);
        app.world_mut().insert_resource(UndoStack::new());
        let open = level_io::open_level_file(app.world_mut(), &host).expect("open host");
        app.world_mut().insert_resource(open.clone());
        app.update();
        // The host as the editor saves it: canonical bytes the round trips compare against.
        level_io::save_level_file(app.world_mut(), &open, &host).expect("canonical save");
        (app, root, [host, a, b])
    }

    fn move_entity(app: &mut App, guid: Guid, x: f32) {
        let entity = entity_of(app.world_mut(), guid).expect("entity");
        let mut command = crate::command::SetTransform::capture(
            app.world(),
            entity,
            Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0)),
        );
        command.apply(app.world_mut());
        app.world_mut()
            .resource_mut::<UndoStack>()
            .push(Box::new(command));
        app.world_mut().resource_mut::<UndoStack>().seal();
        // The editor's per-frame feed.
        crate::level_dirty::feed_on_history_change(app.world_mut());
    }

    fn doc<'a>(state: &'a Value, path_end: &str) -> &'a Value {
        state["documents"]
            .as_array()
            .expect("documents")
            .iter()
            .find(|d| d["file"].as_str().is_some_and(|f| f.ends_with(path_end)))
            .unwrap_or_else(|| panic!("no document {path_end} in {state}"))
    }

    #[test]
    fn two_levels_open_beside_the_host_and_each_saves_only_its_own_file() {
        let (mut app, root, [host, a, b]) = project("levels-open-two");
        for path in [&a, &b] {
            call(
                app.world_mut(),
                "editor.levels_open",
                &json!({ "op": "open", "path": path.display().to_string() }),
            )
            .expect("open beside");
        }
        let state = call(app.world_mut(), "editor.levels_open", &json!({})).expect("list");
        assert_eq!(state["documents"].as_array().expect("docs").len(), 3, "{state}");
        assert!(entity_of(app.world_mut(), A_E).is_some() && entity_of(app.world_mut(), B_E).is_some());
        let err = call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "open", "path": a.display().to_string() }),
        )
        .expect_err("twice");
        assert_eq!(code(err), "level_already_open");

        let bytes = |p: &Path| std::fs::read(p).expect("bytes");
        let (host_before, b_before) = (bytes(&host), bytes(&b));
        move_entity(&mut app, A_E, 5.0);
        let state = call(app.world_mut(), "editor.levels_open", &json!({})).expect("list");
        assert_eq!(doc(&state, "a.level.ron")["dirty"], true, "{state}");
        assert_eq!(doc(&state, "b.level.ron")["dirty"], false, "{state}");
        assert_eq!(doc(&state, "host.level.ron")["dirty"], false, "{state}");

        let id = doc(&state, "a.level.ron")["id"].clone();
        let out = call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "save", "document": id }),
        )
        .expect("save a");
        assert_eq!(out["written"], json!([a.display().to_string()]), "{out}");
        assert_eq!(bytes(&host), host_before, "the host file is untouched");
        assert_eq!(bytes(&b), b_before, "b is untouched");
        let saved = std::fs::read_to_string(&a).expect("a");
        assert!(saved.contains("5.0"), "{saved}");
        assert_eq!(doc(&out, "a.level.ron")["dirty"], false, "{out}");

        // An undo below the save dirties `a` again; closing it asks (refuses) only for `a`.
        crate::command::undo_world(app.world_mut());
        crate::level_dirty::feed_on_history_change(app.world_mut());
        let err = call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "close", "level": a.display().to_string() }),
        )
        .expect_err("a is dirty");
        assert_eq!(code(err), "level_dirty");
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "close", "level": b.display().to_string() }),
        )
        .expect("b closes: its own edits only");
        assert!(entity_of(app.world_mut(), B_E).is_none(), "b's entities left");
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "close", "level": a.display().to_string(), "discard": true }),
        )
        .expect("discard");
        let state = call(app.world_mut(), "editor.levels_open", &json!({})).expect("list");
        assert_eq!(state["documents"].as_array().expect("docs").len(), 1, "{state}");
        let err = call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "close", "level": "host" }),
        )
        .expect_err("host");
        assert_eq!(code(err), "host_level");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn new_entities_land_in_the_active_level() {
        let (mut app, root, [_host, a, _b]) = project("levels-active");
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "open", "path": a.display().to_string() }),
        )
        .expect("open beside");
        let state = call(
            app.world_mut(),
            "editor.active_level",
            &json!({ "set": a.display().to_string() }),
        )
        .expect("active");
        assert_eq!(
            state["active"],
            json!(Guid::from_u128(0x7792_000a).to_string()),
            "{state}"
        );
        let new = Guid::from_u128(0x7792_0099);
        let mut spawn =
            crate::command::SpawnEntities::new_entity(app.world(), new, "fresh", Transform::IDENTITY)
                .expect("spawn");
        spawn.apply(app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().push(Box::new(spawn));
        crate::level_dirty::feed_on_history_change(app.world_mut());
        let entity = entity_of(app.world_mut(), new).expect("spawned");
        assert_eq!(
            app.world().get::<LevelMember>(entity).map(LevelMember::level),
            Some(LevelId(Guid::from_u128(0x7792_000a)))
        );
        let state = call(app.world_mut(), "editor.levels_open", &json!({})).expect("list");
        assert_eq!(doc(&state, "a.level.ron")["dirty"], true, "{state}");
        assert_eq!(doc(&state, "host.level.ron")["dirty"], false, "{state}");
        let id = doc(&state, "a.level.ron")["id"].clone();
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "save", "document": id }),
        )
        .expect("save");
        assert!(std::fs::read_to_string(&a).expect("a").contains("fresh"));
        let err = call(
            app.world_mut(),
            "editor.active_level",
            &json!({ "set": "no/such.level.ron" }),
        )
        .expect_err("not open");
        assert_eq!(code(err), "level_not_open");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_layer_in_its_own_file_round_trips_to_the_original_bytes() {
        let (mut app, root, [host, _a, _b]) = project("levels-layer-file");
        let original = std::fs::read(&host).expect("host");
        let out = call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "night", "separate": true }),
        )
        .expect("separate");
        let sidecar = regia_scene::layer_file::layer_file_path(&host, "night");
        assert!(sidecar.is_file(), "{out}");
        assert_eq!(out["separated"], json!(["night"]));
        let level_only = std::fs::read_to_string(&host).expect("host");
        assert!(!level_only.contains(&MEMBER.to_string()), "{level_only}");
        assert!(doc(&out, "host.night.layer.ron")["kind"] == "layer", "{out}");
        // What every reader (runtime, cook, editor) sees is the whole level.
        let merged = regia_scene::level_dir::read_level_as_stored(&host).expect("read");
        let whole =
            regia_scene::format::SceneFile::from_ron_str(std::str::from_utf8(&original).expect("utf8"))
                .expect("original");
        assert_eq!(merged, whole);

        let out = call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "night", "separate": false }),
        )
        .expect("merge back");
        assert_eq!(
            std::fs::read(&host).expect("host"),
            original,
            "the original bytes"
        );
        assert!(!sidecar.exists(), "{out}");
        let err = call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "night", "separate": false }),
        )
        .expect_err("not separated");
        assert_eq!(code(err), "layer_not_separated");
        let err = call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "nope", "separate": true }),
        )
        .expect_err("unknown");
        assert_eq!(code(err), "layer_unknown");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_layer_file_is_its_own_document() {
        let (mut app, root, [host, _a, _b]) = project("levels-layer-doc");
        call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "night", "separate": true }),
        )
        .expect("separate");
        let sidecar = regia_scene::layer_file::layer_file_path(&host, "night");
        let level_before = std::fs::read(&host).expect("host");
        move_entity(&mut app, MEMBER, 7.0);
        let state = call(app.world_mut(), "editor.levels_open", &json!({})).expect("list");
        assert_eq!(doc(&state, "host.night.layer.ron")["dirty"], true, "{state}");
        assert_eq!(doc(&state, "host.level.ron")["dirty"], false, "{state}");
        let id = doc(&state, "host.night.layer.ron")["id"].clone();
        let out = call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "save", "document": id }),
        )
        .expect("save layer");
        assert_eq!(out["written"], json!([sidecar.display().to_string()]), "{out}");
        assert_eq!(
            std::fs::read(&host).expect("host"),
            level_before,
            "the level file is untouched"
        );
        assert!(std::fs::read_to_string(&sidecar).expect("layer").contains("7.0"));

        // File ▸ Save of the host keeps the members in their layer file.
        move_entity(&mut app, HOST_E, 3.0);
        let open = app.world().resource::<OpenLevel>().clone();
        level_io::save_level_file(app.world_mut(), &open, &host).expect("save");
        let level_text = std::fs::read_to_string(&host).expect("host");
        assert!(
            !level_text.contains(&MEMBER.to_string()) && level_text.contains("3.0"),
            "{level_text}"
        );
        assert!(sidecar.is_file());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_outliner_groups_roots_under_their_documents() {
        let (mut app, root, [host, a, _b]) = project("levels-outliner-groups");
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "open", "path": a.display().to_string() }),
        )
        .expect("open beside");
        call(
            app.world_mut(),
            "editor.layer_file",
            &json!({ "layer": "night", "separate": true }),
        )
        .expect("separate");
        move_entity(&mut app, A_E, 1.0);
        let tree = crate::outliner::hierarchy_tree(app.world());
        let groups = group_by_level(app.world_mut(), &tree);
        let named: Vec<(String, Vec<String>)> = groups
            .iter()
            .map(|(heading, nodes)| (heading.clone(), nodes.iter().map(|n| n.label.clone()).collect()))
            .collect();
        assert_eq!(
            named[..3],
            [
                (
                    "host.level.ron  (active)".to_owned(),
                    vec!["host_thing".to_owned()]
                ),
                (
                    "host.night.layer.ron".to_owned(),
                    vec!["lamp".to_owned(), "lamp_2".to_owned()]
                ),
                ("a.level.ron *".to_owned(), vec!["a_thing".to_owned()]),
            ],
            "{named:?} (host {})",
            host.display()
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// A small level the layer-lock tests open from the library (no project).
    fn lock_app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
                (name: "day", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut()
            .resource_mut::<LayerDefsLibrary>()
            .insert(DEFS_ID, defs);
        let level = LevelId(Guid::from_u128(0x7792_001e));
        let text = level_text(
            &format!(
                "{}{}",
                record(
                    MEMBER,
                    "lamp",
                    r#""regia_scene::layers::LayerMembership": (layers: ["night"]),"#
                ),
                record(HOST_E, "plain", "")
            ),
            &format!(r#""regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: []),"#),
        );
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_ron_str(level, &text).expect("level"));
        open_level(app.world_mut(), level, LoadMode::Replace).expect("open");
        app.update();
        app
    }

    #[test]
    fn a_locked_layer_s_members_cannot_be_picked_and_leave_the_selection() {
        let mut app = lock_app();
        let member = entity_of(app.world_mut(), MEMBER).expect("member");
        let plain = entity_of(app.world_mut(), HOST_E).expect("plain");
        let mut selection = Selection::default();
        selection.select(member, crate::selection::SelectMode::Add);
        selection.select(plain, crate::selection::SelectMode::Add);
        app.world_mut().insert_resource(selection);
        let depth = app
            .world()
            .get_resource::<UndoStack>()
            .map_or(0, UndoStack::undo_depth);

        let out = call(
            app.world_mut(),
            "editor.layer_lock",
            &json!({ "layer": "night", "on": true }),
        )
        .expect("lock");
        assert_eq!(
            (out["locked"].clone(), out["changed"].clone()),
            (json!(true), json!(true))
        );
        assert!(crate::viewport_ops::is_locked(app.world(), member));
        assert!(!crate::viewport_ops::pickable(app.world(), member));
        assert!(!crate::viewport_ops::is_locked(app.world(), plain));
        assert_eq!(layer_lock_glyph(app.world(), member), Some(LOCK_GLYPH));
        assert_eq!(app.world().resource::<Selection>().entities(), &[plain]);
        assert_eq!(
            app.world()
                .get_resource::<UndoStack>()
                .map_or(0, UndoStack::undo_depth),
            depth,
            "a lock is not an undo step"
        );
        // The Outliner still selects a locked member.
        app.world_mut()
            .resource_mut::<Selection>()
            .select(member, crate::selection::SelectMode::Add);
        assert!(app.world().resource::<Selection>().entities().contains(&member));

        let out = call(
            app.world_mut(),
            "editor.layer_lock",
            &json!({ "layer": "night", "on": false }),
        )
        .expect("unlock");
        assert_eq!(out["changed"], true);
        assert!(!crate::viewport_ops::is_locked(app.world(), member));
        let err = call(
            app.world_mut(),
            "editor.layer_lock",
            &json!({ "layer": "nope", "on": true }),
        )
        .expect_err("unknown");
        assert_eq!(code(err), "layer_unknown");
    }

    /// The keyboard path: Tab reaches a layer row's Lock toggle and Space locks the layer; the new
    /// controls carry their names.
    #[test]
    fn the_lock_toggle_is_reachable_and_operable_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let mut app = lock_app();
        let ctx = egui::Context::default();
        let size = [900.0, 900.0];
        {
            let world = app.world_mut();
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| layers_panel(ui, world));
            };
            frame(&ctx, size, vec![], &mut draw);
            let update = frame(&ctx, size, vec![], &mut draw);
            let names = crate::a11y_audit::widget_names(&update);
            for label in ["Lock", "Store in its own file"] {
                assert!(
                    names.iter().any(|(_, n)| n == label),
                    "{label} is named: {names:?}"
                );
            }
            tab_to(&ctx, size, "Lock", false, 80, &mut draw);
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Space, egui::Modifiers::NONE)],
                &mut draw,
            );
            frame(&ctx, size, vec![], &mut draw);
        }
        // Rows are by name: `day` comes first.
        assert!(
            is_layer_locked(app.world(), "day"),
            "Space on the focused Lock toggle locks the layer"
        );
        assert!(!is_layer_locked(app.world(), "night"));
    }

    /// One Outliner frame over `world` with `events` (the pointer over the panel).
    fn outliner_frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
        crate::visibility::mark_internal_entities(world);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(360.0, 480.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| crate::outliner::outliner_panel(ui, world));
        });
        output.textures_delta.clear();
    }

    /// The listed rows as text: `[level label open]` headings and `name@depth` entity rows.
    fn outliner_text(world: &World) -> Vec<String> {
        world
            .resource::<crate::outliner::OutlinerView>()
            .rows()
            .iter()
            .map(|row| match row {
                crate::outliner::FlatRow::Level { label, open, .. } => format!("[{label} {open}]"),
                crate::outliner::FlatRow::Entity { label, depth, .. } => format!("{label}@{depth}"),
                crate::outliner::FlatRow::Category { name, .. } => format!("<{name}>"),
                crate::outliner::FlatRow::Folder { path, .. } => format!("/{path}"),
            })
            .collect()
    }

    /// P-097 (1): with a level open beside the host the Outliner lists each open level's roots
    /// under one row per level, the active level marked; the keyboard closes (Left on a root) and
    /// opens (Right) a level's group, and the agent rows do the same. With the host alone the
    /// tree has no level rows.
    #[test]
    fn the_outliner_groups_two_open_levels() {
        let (mut app, root, [_host, a, _b]) = project("levels-outliner-two");
        let ctx = egui::Context::default();
        outliner_frame(&ctx, app.world_mut(), vec![]);
        outliner_frame(&ctx, app.world_mut(), vec![]);
        assert!(
            outliner_text(app.world()).iter().all(|row| !row.starts_with('[')),
            "one open level keeps the plain tree: {:?}",
            outliner_text(app.world())
        );

        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "open", "path": a.display().to_string() }),
        )
        .expect("open beside");
        call(
            app.world_mut(),
            "editor.active_level",
            &json!({ "set": a.display().to_string() }),
        )
        .expect("active");
        outliner_frame(&ctx, app.world_mut(), vec![]);
        outliner_frame(&ctx, app.world_mut(), vec![]);
        let rows = outliner_text(app.world());
        let host_at = rows
            .iter()
            .position(|r| r == "[host.level.ron true]")
            .unwrap_or_else(|| panic!("host heading in {rows:?}"));
        let a_at = rows
            .iter()
            .position(|r| r == "[a.level.ron  (active) true]")
            .unwrap_or_else(|| panic!("active a heading in {rows:?}"));
        assert!(host_at < a_at, "{rows:?}");
        let host_rows: Vec<&String> = rows[host_at + 1..a_at].iter().collect();
        for name in ["host_thing@0", "lamp@0", "lamp_2@0"] {
            assert!(
                host_rows.iter().any(|r| *r == name),
                "{name} under the host: {rows:?}"
            );
        }
        assert_eq!(
            rows.get(a_at + 1).map(String::as_str),
            Some("a_thing@0"),
            "{rows:?}"
        );
        assert!(!host_rows.iter().any(|r| *r == "a_thing@0"), "{rows:?}");

        // The keyboard: Left on a's root closes a's group, Right opens it again.
        let a_thing = entity_of(app.world_mut(), A_E).expect("a_thing");
        let mut selection = Selection::default();
        selection.select(a_thing, crate::selection::SelectMode::Replace);
        app.world_mut().insert_resource(selection);
        outliner_frame(&ctx, app.world_mut(), vec![]);
        let over = app
            .world()
            .resource::<crate::outliner::OutlinerRows>()
            .row(a_thing)
            .expect("a_thing drawn")
            .rect
            .center();
        let press = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        outliner_frame(&ctx, app.world_mut(), vec![egui::Event::PointerMoved(over)]);
        outliner_frame(&ctx, app.world_mut(), vec![press(egui::Key::ArrowLeft)]);
        outliner_frame(&ctx, app.world_mut(), vec![]);
        let rows = outliner_text(app.world());
        assert!(
            rows.iter().any(|r| r == "[a.level.ron  (active) false]"),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r == "a_thing@0"),
            "a closed group lists no roots: {rows:?}"
        );
        outliner_frame(&ctx, app.world_mut(), vec![egui::Event::PointerMoved(over)]);
        outliner_frame(&ctx, app.world_mut(), vec![press(egui::Key::ArrowRight)]);
        outliner_frame(&ctx, app.world_mut(), vec![]);
        let rows = outliner_text(app.world());
        assert!(
            rows.iter().any(|r| r == "a_thing@0"),
            "Right opens it again: {rows:?}"
        );

        // The agent rows: one `level` row per open level, `close_levels` closes a group.
        let a_id = Guid::from_u128(0x7792_000a).to_string();
        let (_, rows_method) = crate::outliner::methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.outliner_rows")
            .expect("registered");
        let out = rows_method(app.world_mut(), &json!({ "close_levels": [a_id] })).expect("rows");
        let levels: Vec<&Value> = out["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .filter(|row| row.get("level").is_some() && row["label"] != json!(NO_LEVEL_GROUP))
            .collect();
        assert_eq!(levels.len(), 2, "{out}");
        assert_eq!(levels[1]["level"], json!(a_id), "{out}");
        assert_eq!(levels[1]["active"], true, "{out}");
        assert_eq!(levels[1]["open"], false, "{out}");
        assert_eq!(levels[0]["active"], false, "{out}");
        let err = rows_method(app.world_mut(), &json!({ "open_levels": [7] })).expect_err("not ids");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        assert!(
            !app.world()
                .resource::<crate::outliner::OutlinerView>()
                .is_level_open(Guid::from_u128(0x7792_000a).as_u128()),
            "a refused call leaves the Outliner's view as it was"
        );

        // The heading is a focusable button: Tab reaches the closed group's heading and Enter
        // opens it.
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let size = [360.0, 480.0];
        let keys = egui::Context::default();
        let world = app.world_mut();
        let mut draw = |ui: &mut egui::Ui| {
            crate::visibility::mark_internal_entities(world);
            egui::CentralPanel::default().show(ui, |ui| crate::outliner::outliner_panel(ui, world));
        };
        frame(&keys, size, vec![], &mut draw);
        tab_to(
            &keys,
            size,
            "\u{25b8} a.level.ron  (active) (1)",
            false,
            40,
            &mut draw,
        );
        frame(
            &keys,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&keys, size, vec![], &mut draw);
        drop(draw);
        let rows = outliner_text(app.world());
        assert!(
            rows.iter().any(|r| r == "a_thing@0"),
            "Enter on the heading opens it: {rows:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// P-097 (2): with a level open beside the host and made active, a Content Browser prefab
    /// drop, a prefab created from entities that belong to no level, and a paste all land in the
    /// active level, not the host.
    #[test]
    fn a_browser_drop_lands_in_the_active_level() {
        let (mut app, root, [_host, a, _b]) = project("levels-drop-active");
        call(
            app.world_mut(),
            "editor.levels_open",
            &json!({ "op": "open", "path": a.display().to_string() }),
        )
        .expect("open beside");
        call(
            app.world_mut(),
            "editor.active_level",
            &json!({ "set": a.display().to_string() }),
        )
        .expect("active");
        let a_id = LevelId(Guid::from_u128(0x7792_000a));
        let level_of =
            |world: &World, entity: Entity| world.get::<LevelMember>(entity).map(LevelMember::level);

        // Create prefab from an entity in no level: the instance joins the active level.
        let loose = app
            .world_mut()
            .spawn((
                regia_scene::prelude::Name::new("crate"),
                Transform::IDENTITY,
                EntityGuid(Guid::from_u128(0x7792_0077)),
            ))
            .id();
        let prefab = root.join("assets/crate.prefab.ron");
        let created =
            crate::prefab_create::create_prefab(app.world_mut(), &[loose], &prefab).expect("create");
        assert_eq!(
            level_of(app.world(), created.instance),
            Some(a_id),
            "created instance"
        );

        // The Content Browser drop of that prefab.
        let mut open = app.world().resource::<OpenLevel>().clone();
        let toast = crate::content::apply_content_drop(
            app.world_mut(),
            &mut open,
            &root,
            "assets/crate.prefab.ron",
            crate::dnd::ContentKind::Prefab,
        );
        assert!(toast.starts_with("Spawned prefab"), "{toast}");
        let instances: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<regia_scene::prefab::PrefabInstance>>()
            .iter(app.world())
            .filter(|e| *e != created.instance)
            .collect();
        assert_eq!(instances.len(), 1, "one dropped instance");
        assert_eq!(
            level_of(app.world(), instances[0]),
            Some(a_id),
            "dropped instance"
        );

        // Paste of a host entity: the copy lands in the active level too.
        let host_thing = entity_of(app.world_mut(), HOST_E).expect("host_thing");
        crate::viewport_ops::copy_entities(app.world_mut(), &[host_thing]).expect("copy");
        let pasted = crate::viewport_ops::paste(app.world_mut()).expect("paste");
        assert_eq!(pasted.len(), 1);
        assert_eq!(level_of(app.world(), pasted[0]), Some(a_id), "pasted copy");
        assert_eq!(
            level_of(app.world(), host_thing),
            Some(app.world().resource::<OpenLevel>().id),
            "the original stays in the host"
        );
        std::fs::remove_dir_all(root).ok();
    }
}
