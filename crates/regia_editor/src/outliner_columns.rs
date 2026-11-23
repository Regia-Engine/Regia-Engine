// SPDX-License-Identifier: MPL-2.0
//! Outliner row extras (`docs/specs/19-editor.md` § 2, ADR-7772): configurable columns, row
//! colors, tags on the selection, script / logic-graph / sequence badges and the unique-name
//! marker's editor rules.
//!
//! - Columns: Type (the kind or role), Layer (the map layer), Prefab source, Tags (chips),
//!   Visible/Locked (the eye and lock buttons) and a Revision slot that the revision-control panel
//!   fills through [`RevisionColumn`]. Each tree instance (`"main"` today) keeps its own shown set,
//!   widths and sort in the per-user view state `.regia/outliner_view.ron`.
//! - Row colors: a user color per entity (editor-only, the same view state, keyed by entity guid),
//!   one of the theme's palette tokens ([`ROW_COLORS`]), tinting the row.
//! - Sort: by name (the default) or by creation order. Creation order is entity id order (the
//!   allocator's generation, then index): a freshly opened level's entities in the order the loader
//!   spawned them, then the entities made in this session in the order they were made. A level file
//!   lists its entities by guid and stores no creation stamp, so the order of entities made in an
//!   earlier session is not kept across a save and reload.
//! - Tags: "Add tag to selection…" and "Remove tag" act on every selected entity as one undo step.
//! - Unique name: the context toggle adds the saved [`regia_scene::unique_name::UniqueName`]
//!   marker; a second marked entity with the same name is refused (`unique_name_taken`) on toggle
//!   and rename, and a new copy (duplicate, paste) is given the next free name in the same undo step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_scene::prelude::{EntityGuid, Name};
use regia_scene::unique_name::UniqueName;
use serde::{Deserialize, Serialize};

use crate::command::{AddComponent, CommandGroup, SetProperty, UndoStack};

/// The per-user view-state file of the Outliner, under the project root.
pub const VIEW_STATE_FILE: &str = ".regia/outliner_view.ron";

/// The reflected type path of `Tags` (full: a short name can be ambiguous in a large registry).
const TAGS: &str = "regia_scene::components::Tags";

/// The reflected type path of `UniqueName`.
const UNIQUE: &str = "regia_scene::unique_name::UniqueName";

/// The tree instance the Outliner panel draws.
pub const MAIN_TREE: &str = "main";

/// One Outliner column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// The entity's kind (or derived role).
    Type,
    /// The map layer (own or inherited).
    Layer,
    /// The prefab an instance comes from.
    PrefabSource,
    /// The entity's tags, as chips.
    Tags,
    /// The eye and lock buttons.
    VisibleLocked,
    /// The revision-control state ([`RevisionColumn`] fills it).
    Revision,
}

impl Column {
    /// Every column, in draw order.
    pub const ALL: [Column; 6] = [
        Self::Type,
        Self::Layer,
        Self::PrefabSource,
        Self::Tags,
        Self::VisibleLocked,
        Self::Revision,
    ];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Layer => "layer",
            Self::PrefabSource => "prefab_source",
            Self::Tags => "tags",
            Self::VisibleLocked => "visible_locked",
            Self::Revision => "revision",
        }
    }

    /// The column header text.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Type => "Type",
            Self::Layer => "Layer",
            Self::PrefabSource => "Prefab source",
            Self::Tags => "Tags",
            Self::VisibleLocked => "Visible / Locked",
            Self::Revision => "Revision",
        }
    }

    /// The column of a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The default width, in points (scaled with the UI like every egui size).
    #[must_use]
    pub fn default_width(self) -> f32 {
        match self {
            Self::Type | Self::PrefabSource | Self::Tags => 90.0,
            Self::Layer | Self::Revision => 48.0,
            Self::VisibleLocked => 44.0,
        }
    }
}

/// How the rows are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowSort {
    /// By label, then entity id (the default).
    #[default]
    Name,
    /// By creation order (spawn order; see the module docs).
    Creation,
}

/// One tree instance's columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSet {
    /// The shown columns, in [`Column::ALL`] order.
    pub shown: Vec<Column>,
    /// Width overrides by column, points.
    #[serde(default)]
    pub widths: BTreeMap<Column, f32>,
    /// Row order.
    #[serde(default)]
    pub sort: RowSort,
}

impl Default for ColumnSet {
    /// The look before columns: the eye and lock only.
    fn default() -> Self {
        Self {
            shown: vec![Column::VisibleLocked],
            widths: BTreeMap::new(),
            sort: RowSort::Name,
        }
    }
}

impl ColumnSet {
    /// Whether `column` is shown.
    #[must_use]
    pub fn shows(&self, column: Column) -> bool {
        self.shown.contains(&column)
    }

    /// Shows or hides `column` (kept in [`Column::ALL`] order).
    pub fn set_shown(&mut self, column: Column, shown: bool) {
        self.shown.retain(|c| *c != column);
        if shown {
            self.shown.push(column);
            self.shown.sort();
        }
    }

    /// The width of `column`, points.
    #[must_use]
    pub fn width(&self, column: Column) -> f32 {
        self.widths
            .get(&column)
            .copied()
            .unwrap_or_else(|| column.default_width())
    }
}

/// The named row colors and the theme palette slot each takes ([`crate::theme::ThemeTokens`]'s
/// `data_kinds`, a colorblind-safe set).
pub const ROW_COLORS: [(&str, usize); 10] = [
    ("orange", 0),
    ("sky", 1),
    ("green", 2),
    ("yellow", 3),
    ("blue", 4),
    ("red", 5),
    ("pink", 6),
    ("gray", 7),
    ("brown", 8),
    ("lime", 9),
];

/// The theme color of a row color name.
#[must_use]
pub fn row_color(tokens: &crate::theme::ThemeTokens, name: &str) -> Option<egui::Color32> {
    ROW_COLORS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, slot)| tokens.data_kinds[*slot])
}

/// The Outliner's per-user view state: columns per tree instance and row colors per entity guid.
/// Saved to [`VIEW_STATE_FILE`]; never on the undo stack and never part of a level.
#[derive(Resource, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OutlinerViewState {
    /// Columns by tree instance.
    #[serde(default)]
    pub trees: BTreeMap<String, ColumnSet>,
    /// Row colors by entity guid (hex).
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
    /// The file it was loaded from (and is saved to).
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// Bumped by every change (the panel keys on it).
    #[serde(skip)]
    pub revision: u64,
    /// The project root the state belongs to (`None`: no project, in memory only). A different
    /// open project reloads the state from its own file.
    #[serde(skip)]
    pub root: Option<PathBuf>,
    /// Why the file at `path` must not be overwritten: it exists but did not read or parse. Saving
    /// is refused (the user's file is kept byte for byte) until the file is fixed or deleted.
    #[serde(skip)]
    pub refuse_save: Option<String>,
}

impl OutlinerViewState {
    /// Reads a view-state file. A missing file is the default state. A file that exists but does
    /// not read or parse is also the default state, logged, with [`refuse_save`](Self::refuse_save)
    /// set: [`save`](Self::save) then leaves the file alone.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let mut state = match std::fs::read_to_string(path) {
            Ok(text) => ron::from_str::<Self>(&text).unwrap_or_else(|err| {
                tracing::warn!(
                    target: "regia_editor",
                    "outliner view state {}: {err} (kept as is; fix or delete the file to save the view again)",
                    path.display()
                );
                Self {
                    refuse_save: Some(format!("{} does not parse: {err}", path.display())),
                    ..Self::default()
                }
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!(
                    target: "regia_editor",
                    "outliner view state {}: {err} (kept as is)",
                    path.display()
                );
                Self {
                    refuse_save: Some(format!("{} cannot be read: {err}", path.display())),
                    ..Self::default()
                }
            }
        };
        state.path = Some(path.to_path_buf());
        state
    }

    /// Writes the state to its file (atomically); a state without a file is kept in memory.
    ///
    /// # Errors
    /// The write failure, as text; the [`refuse_save`](Self::refuse_save) reason when the file on
    /// disk did not read or parse (it is not overwritten).
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(reason) = &self.refuse_save {
            return Err(format!("not saved: {reason}"));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| e.to_string())?;
        crate::atomic_write::write_atomic(path, text.as_bytes())
    }

    /// The columns of tree `tree`.
    #[must_use]
    pub fn columns(&self, tree: &str) -> ColumnSet {
        self.trees.get(tree).cloned().unwrap_or_default()
    }

    /// The color name of `guid`'s row.
    #[must_use]
    pub fn color_of(&self, guid: &EntityGuid) -> Option<&str> {
        self.colors.get(&guid.to_string()).map(String::as_str)
    }
}

/// The view state, loaded from the open project's [`VIEW_STATE_FILE`] the first time and again
/// whenever another project is opened (in memory only when no project is open).
pub fn view_state(world: &mut World) -> &OutlinerViewState {
    ensure_loaded(world);
    world.resource::<OutlinerViewState>()
}

/// The open project's root, if any.
fn open_root(world: &World) -> Option<&Path> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.as_deref())
}

/// The view state when it belongs to the open project (a state left over from a previously open
/// project reads as none until [`view_state`] reloads it).
#[must_use]
pub fn current_view_state(world: &World) -> Option<&OutlinerViewState> {
    world
        .get_resource::<OutlinerViewState>()
        .filter(|state| state.root.as_deref() == open_root(world))
}

fn ensure_loaded(world: &mut World) {
    if current_view_state(world).is_some() {
        return;
    }
    let root = open_root(world).map(Path::to_path_buf);
    let mut state = match &root {
        Some(root) => OutlinerViewState::load(&root.join(VIEW_STATE_FILE)),
        None => OutlinerViewState::default(),
    };
    state.root = root;
    // The panel keys on the revision: keep it moving across a reload.
    state.revision = world
        .get_resource::<OutlinerViewState>()
        .map_or(0, |old| old.revision + 1);
    world.insert_resource(state);
}

/// Changes the view state with `edit` and saves it. A view-state file that did not read or parse
/// is not overwritten: the edit stays in memory until the file is fixed or deleted, then the file
/// is read again and the edit is applied to it.
pub fn edit_view_state(world: &mut World, edit: impl FnOnce(&mut OutlinerViewState)) {
    ensure_loaded(world);
    let mut state = world.resource_mut::<OutlinerViewState>();
    if state.refuse_save.is_some()
        && let Some(path) = state.path.clone()
    {
        let reread = OutlinerViewState::load(&path);
        if reread.refuse_save.is_none() {
            let (root, revision) = (state.root.clone(), state.revision);
            *state = OutlinerViewState {
                root,
                revision,
                ..reread
            };
        }
    }
    edit(&mut state);
    state.revision += 1;
    if let Err(err) = state.save() {
        tracing::warn!(target: "regia_editor", "outliner view state: {err}");
    }
}

/// Sets (or clears, with `None`) `entity`'s row color.
///
/// # Errors
/// `unknown_color` for a name outside [`ROW_COLORS`]; `no_guid` for an entity without a guid.
pub fn set_row_color(world: &mut World, entity: Entity, color: Option<&str>) -> Result<(), &'static str> {
    if color.is_some_and(|c| !ROW_COLORS.iter().any(|(n, _)| *n == c)) {
        return Err("unknown_color");
    }
    let Some(guid) = world.get::<EntityGuid>(entity).copied() else {
        return Err("no_guid");
    };
    edit_view_state(world, |state| match color {
        Some(color) => {
            state.colors.insert(guid.to_string(), color.to_string());
        }
        None => {
            state.colors.remove(&guid.to_string());
        }
    });
    Ok(())
}

/// The row color name of `entity` (none without a guid or a color).
#[must_use]
pub fn row_color_of(world: &World, entity: Entity) -> Option<String> {
    let state = current_view_state(world)?;
    let guid = world.get::<EntityGuid>(entity)?;
    state.color_of(guid).map(str::to_string)
}

/// The revision-control column's source: what the Revision cell of an entity reads (the revision
/// panel, job 3257, sets it; empty cells until then).
#[derive(Resource, Clone, Copy, Default)]
pub struct RevisionColumn(pub Option<fn(&World, Entity) -> Option<String>>);

impl std::fmt::Debug for RevisionColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RevisionColumn").field(&self.0.is_some()).finish()
    }
}

/// The text of one cell of `entity`'s row (the Visible/Locked column's buttons are drawn by the
/// row itself; its cell text reads `visible`/`hidden` and `locked`).
#[must_use]
pub fn cell_text(world: &World, entity: Entity, column: Column) -> String {
    match column {
        Column::Type => {
            let icon = crate::kind_icons::row_icon(world, entity);
            icon.kind
                .or_else(|| icon.role.map(|r| r.name().to_string()))
                .unwrap_or_default()
        }
        Column::Layer => {
            let mut seen = std::collections::HashSet::new();
            let mut cursor = Some(entity);
            while let Some(current) = cursor {
                if !seen.insert(current) {
                    break;
                }
                if let Some(layer) = world.get::<regia_scene::components::MapLayer>(current) {
                    return layer.0.to_string();
                }
                cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
            }
            "0".to_string()
        }
        Column::PrefabSource => world
            .get::<regia_scene::prefab::PrefabInstance>(entity)
            .map(|instance| instance.prefab_guid().to_string())
            .unwrap_or_default(),
        Column::Tags => tags_of(world, entity).join(", "),
        Column::VisibleLocked => {
            let visible = crate::viewport_ops::is_visible(world, entity);
            let locked = world.get::<crate::viewport_ops::EditorLocked>(entity).is_some();
            format!(
                "{}{}",
                if visible { "visible" } else { "hidden" },
                if locked { ", locked" } else { "" }
            )
        }
        Column::Revision => world
            .get_resource::<RevisionColumn>()
            .and_then(|source| source.0)
            .and_then(|read| read(world, entity))
            .unwrap_or_default(),
    }
}

/// The cells of `entity`'s row for the shown columns, in draw order.
#[must_use]
pub fn row_cells(world: &World, entity: Entity, columns: &ColumnSet) -> Vec<(Column, String)> {
    columns
        .shown
        .iter()
        .map(|column| (*column, cell_text(world, entity, *column)))
        .collect()
}

/// The entity's tags, as written.
#[must_use]
pub fn tags_of(world: &World, entity: Entity) -> Vec<String> {
    world
        .get::<regia_scene::components::Tags>(entity)
        .map(|tags| tags.0.iter().map(|t| t.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// Adds (`add`) or removes `tag` on every entity of `entities` as one undo step. Entities that
/// already have (or lack) the tag are left alone. Returns the entities changed.
///
/// # Errors
/// `empty_tag` for a blank tag; `no_entities` when the list is empty.
pub fn tag_entities(
    world: &mut World,
    entities: &[Entity],
    tag: &str,
    add: bool,
) -> Result<Vec<Entity>, &'static str> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err("empty_tag");
    }
    if entities.is_empty() {
        return Err("no_entities");
    }
    let mut group = CommandGroup::new(if add { "Add tag" } else { "Remove tag" });
    let mut changed = Vec::new();
    for entity in entities {
        if world.get_entity(*entity).is_err() {
            continue;
        }
        let current = world.get::<regia_scene::components::Tags>(*entity).cloned();
        let has = current.as_ref().is_some_and(|t| t.contains(tag));
        if has == add {
            continue;
        }
        let mut next = current.clone().unwrap_or_default();
        if add {
            next.0.push(Name::new(tag));
        } else {
            next.0.retain(|t| t.as_str() != tag);
        }
        let command: Result<Box<dyn crate::command::EditorCommand>, _> = if current.is_some() {
            SetProperty::capture(world, *entity, TAGS, "", Box::new(next)).map(|mut set| {
                set.coalesce = false;
                Box::new(set) as Box<dyn crate::command::EditorCommand>
            })
        } else {
            AddComponent::capture(world, *entity, TAGS, Box::new(next))
                .map(|add| Box::new(add) as Box<dyn crate::command::EditorCommand>)
        };
        match command {
            Ok(mut command) => {
                command.apply(world);
                group.push(command);
                changed.push(*entity);
            }
            Err(err) => tracing::warn!(target: "regia_editor", "tag: {err}"),
        }
    }
    if !changed.is_empty() {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(group));
    }
    Ok(changed)
}

/// What is attached to an entity that has its own editor: a badge on its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// A Lua script component.
    Script,
    /// A logic graph.
    LogicGraph,
    /// A sequence player or its bindings.
    Sequence,
    /// The unique-name marker.
    UniqueName,
}

impl Badge {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::LogicGraph => "logic_graph",
            Self::Sequence => "sequence",
            Self::UniqueName => "unique_name",
        }
    }

    /// The glyph drawn on the row.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Script => "\u{1f4dc}",
            Self::LogicGraph => "\u{27a1}",
            Self::Sequence => "\u{1f39e}",
            Self::UniqueName => "\u{2460}",
        }
    }

    /// The accessible name of the badge's button, when the badge opens something (`None`: the badge
    /// is a label).
    #[must_use]
    pub fn open_name(self) -> Option<&'static str> {
        match self {
            Self::Script => Some("Open this entity's script in the code editor"),
            Self::LogicGraph => Some("Open this entity's logic graph"),
            Self::Sequence => Some("Open this entity's sequence curves"),
            Self::UniqueName => None,
        }
    }

    /// The accessible name and hover text.
    #[must_use]
    pub fn spoken(self) -> &'static str {
        match self {
            Self::Script => "Has a script",
            Self::LogicGraph => "Has a logic graph",
            Self::Sequence => "Plays or is bound to a sequence",
            Self::UniqueName => "Unique name: game code finds it by name",
        }
    }
}

/// The badges of `entity`'s row, in draw order.
#[must_use]
pub fn badges(world: &World, entity: Entity) -> Vec<Badge> {
    let mut out = Vec::new();
    if world.get::<regia_script_lua::LuaComponent>(entity).is_some() {
        out.push(Badge::Script);
    }
    if world.get::<regia_ai::LogicGraph>(entity).is_some() {
        out.push(Badge::LogicGraph);
    }
    if world
        .get::<regia_framework::sequencer::SequencePlayer>(entity)
        .is_some()
        || world
            .get::<regia_framework::sequencer::SequenceBindings>(entity)
            .is_some()
    {
        out.push(Badge::Sequence);
    }
    if world.get::<UniqueName>(entity).is_some() {
        out.push(Badge::UniqueName);
    }
    out
}

/// What a sequence badge does: selects the entity and brings the Sequence Curves panel forward.
pub fn open_sequence(world: &mut World, entity: Entity) {
    select_only(world, entity);
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::SeqCurves),
    );
}

/// What opening a badge did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadgeOpened {
    /// A panel was asked to come forward (`seq_curves`, `logic_graph`).
    Panel(&'static str),
    /// A script's declaration was sent to the code editor (or, in a dry run, only resolved).
    CodeEditor {
        /// The Lua file.
        path: PathBuf,
        /// The declaration's line (1-based).
        line: u32,
        /// The command that opens it (never run through a shell).
        command: Vec<String>,
    },
}

/// Why a badge did not open: a lowercase reason code, what happened and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadgeError {
    /// `no_script`, `no_logic_graph`, `no_sequence`, `script_path_unknown`, `no_code_editor`,
    /// `code_editor_failed`, `logic_graph_unsaved` or `badge_not_openable`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
    /// What to do about it.
    pub hint: &'static str,
}

impl BadgeError {
    fn new(code: &'static str, message: impl Into<String>, hint: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            hint,
        }
    }
}

/// A script component's declaration site: the Lua file of its mod and the line of its
/// `regia.component` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSource {
    /// The Lua file.
    pub file: PathBuf,
    /// The line (1-based).
    pub line: u32,
}

/// Most Lua files and bytes per file one declaration search reads.
const SCRIPT_SEARCH_FILES: usize = 256;
const SCRIPT_SEARCH_BYTES: u64 = 1 << 20;

/// Finds the declaration of `decl` (a `regia.component("decl", ...)` call) in the Lua files under
/// `dir`, in file-name order.
fn find_declaration(dir: &Path, decl: &str, budget: &mut usize) -> Option<ScriptSource> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(found) = find_declaration(&path, decl, budget) {
                return Some(found);
            }
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "lua") || *budget == 0 {
            continue;
        }
        *budget -= 1;
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > SCRIPT_SEARCH_BYTES) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let quoted = [format!("\"{decl}\""), format!("'{decl}'")];
        for (index, line) in text.lines().enumerate() {
            if line.contains("component") && quoted.iter().any(|q| line.contains(q.as_str())) {
                return Some(ScriptSource {
                    file: path,
                    line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                });
            }
        }
    }
    None
}

/// Where the script component on `entity` is declared: its mod's directory under the project's
/// `mods/` folder (found by the manifest id the loaded declaration names), then the file and line
/// of the `regia.component` call. Reads files, so call it on a click or a hover, not per frame.
///
/// # Errors
/// `no_script` (no script component) or `script_path_unknown` (the mod is not loaded, or not a
/// directory under the project's `mods/`, or no file declares the component): the message says
/// which.
pub fn script_declaration(world: &World, entity: Entity) -> Result<ScriptSource, BadgeError> {
    let Some(decl) = world
        .get::<regia_script_lua::LuaComponent>(entity)
        .map(|component| component.decl.clone())
    else {
        return Err(BadgeError::new(
            "no_script",
            format!("entity {} has no script component", entity.to_bits()),
            "the Outliner shows the script badge on entities that have one",
        ));
    };
    let unknown = |message: String| {
        BadgeError::new(
            "script_path_unknown",
            message,
            "the declaring mod must be loaded from a folder under the project's mods/ directory",
        )
    };
    let mod_id = world
        .get_resource::<regia_script_lua::LuaMods>()
        .and_then(|mods| {
            mods.iter()
                .find_map(|m| m.components().into_iter().find(|d| d.name == decl))
        })
        .map(|d| d.mod_id)
        .ok_or_else(|| unknown(format!("the mod that declares `{decl}` is not loaded")))?;
    let root = crate::vcs::project_root(world).ok_or_else(|| unknown("no project is open".to_string()))?;
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("mods"))
        .map(|read| read.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    dirs.sort();
    for dir in dirs {
        let id = std::fs::read_to_string(dir.join(regia_script_lua::LUAMOD_FILE))
            .ok()
            .and_then(|text| regia_script_lua::LuaModManifest::parse(&text).ok())
            .map(|manifest| manifest.id);
        if id.as_deref() != Some(mod_id.as_str()) {
            continue;
        }
        let mut budget = SCRIPT_SEARCH_FILES;
        return find_declaration(&dir, &decl, &mut budget)
            .ok_or_else(|| unknown(format!("no Lua file of mod {mod_id} declares `{decl}`")));
    }
    Err(unknown(format!(
        "mod {mod_id} (which declares `{decl}`) is not in the project's mods/ folder"
    )))
}

/// What `badge` on `entity` does, as the row and `editor.open_badge` run it: the sequence badge
/// brings the Sequence Curves panel forward; the logic-graph badge shows the entity's graph in the
/// Logic Graph panel; the script badge opens the declaration of its script component in the
/// configured code editor. Each selects the entity first. `dry_run` resolves a script's command
/// without starting the editor.
///
/// # Errors
/// [`BadgeError`]: the entity lacks the badge, the unique-name badge opens nothing, the Logic Graph
/// panel holds unsaved edits (it is brought forward, not replaced), or the script's file or the code
/// editor is unknown.
pub fn open_badge(
    world: &mut World,
    entity: Entity,
    badge: Badge,
    dry_run: bool,
) -> Result<BadgeOpened, BadgeError> {
    if !badges(world, entity).contains(&badge) {
        let (code, what) = match badge {
            Badge::Script => ("no_script", "a script component"),
            Badge::LogicGraph => ("no_logic_graph", "a logic graph"),
            Badge::Sequence => ("no_sequence", "a sequence"),
            Badge::UniqueName => ("badge_not_openable", "the unique-name marker"),
        };
        return Err(BadgeError::new(
            code,
            format!("entity {} has no {what} badge", entity.to_bits()),
            "the Outliner shows the badges an entity has (editor.outliner_view { entity })",
        ));
    }
    match badge {
        Badge::UniqueName => Err(BadgeError::new(
            "badge_not_openable",
            "the unique-name badge only marks the row",
            "toggle it from the row's context menu or editor.outliner_set { unique }",
        )),
        Badge::Sequence => {
            open_sequence(world, entity);
            Ok(BadgeOpened::Panel("seq_curves"))
        }
        Badge::LogicGraph => {
            select_only(world, entity);
            crate::editor_layout::LayoutRequests::push(
                world,
                crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::LogicGraph),
            );
            let def = world
                .get::<regia_ai::LogicGraph>(entity)
                .map(|graph| graph.def().clone())
                .ok_or_else(|| BadgeError::new("no_logic_graph", "the graph left the entity", ""))?;
            let mut editor =
                world.get_resource_or_insert_with(crate::logic_graph_editor::LogicEditor::default);
            if editor.has_unsaved_work() {
                return Err(BadgeError::new(
                    "logic_graph_unsaved",
                    "the Logic Graph panel holds unsaved edits; it is shown as it is",
                    "save or discard them (editor.logic_open discards), then open the badge again",
                ));
            }
            editor.open_def(&def);
            Ok(BadgeOpened::Panel("logic_graph"))
        }
        Badge::Script => {
            let source = script_declaration(world, entity)?;
            let root = crate::vcs::project_root(world).unwrap_or_default();
            let env =
                |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
            let command =
                crate::log_links::click_command(world, &root, &source.file, source.line, None, &env)
                    .map_err(|e| {
                        BadgeError::new(
                            "no_code_editor",
                            e.message,
                            "set the preference code.editor_command, or $VISUAL / $EDITOR",
                        )
                    })?;
            select_only(world, entity);
            if !dry_run {
                crate::agent_changes::spawn_code_editor(&root, &command).map_err(|e| {
                    BadgeError::new(
                        "code_editor_failed",
                        e.message,
                        "check the code editor command in the preferences",
                    )
                })?;
            }
            Ok(BadgeOpened::CodeEditor {
                path: source.file,
                line: source.line,
                command,
            })
        }
    }
}

/// The hover text of a script badge: where a click goes, or why it cannot go anywhere.
#[must_use]
pub fn script_badge_hover(world: &World, entity: Entity) -> String {
    match script_declaration(world, entity) {
        Ok(source) => format!(
            "Has a script. Click to open {}:{} in the code editor.",
            source.file.display(),
            source.line
        ),
        Err(error) => format!("Has a script. It cannot open: {}.", error.message),
    }
}

/// [`script_badge_hover`], read again at most every 120 frames per entity (it reads files).
fn script_hover_cached(ui: &egui::Ui, world: &World, entity: Entity) -> String {
    let id = egui::Id::new(("outliner-script-hover", entity.to_bits()));
    let frame = ui.ctx().cumulative_frame_nr();
    if let Some((at, text)) = ui.ctx().data(|d| d.get_temp::<(u64, String)>(id))
        && frame.saturating_sub(at) < 120
    {
        return text;
    }
    let text = script_badge_hover(world, entity);
    ui.ctx().data_mut(|d| d.insert_temp(id, (frame, text.clone())));
    text
}

/// Replaces the selection with `entity`.
fn select_only(world: &mut World, entity: Entity) {
    let mut selection = world
        .get_resource::<crate::selection::Selection>()
        .cloned()
        .unwrap_or_default();
    selection.select(entity, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
}

/// Turns the unique-name marker of `entity` on or off as one undo step.
///
/// # Errors
/// `unique_name_taken` when another marked entity has this name; `no_name` for an unnamed entity.
pub fn set_unique_name(world: &mut World, entity: Entity, unique: bool) -> Result<(), &'static str> {
    let Some(name) = world.get::<Name>(entity).map(|n| n.as_str().to_string()) else {
        return Err("no_name");
    };
    let marked = world.get::<UniqueName>(entity).is_some();
    if marked == unique {
        return Ok(());
    }
    let mut command: Box<dyn crate::command::EditorCommand> = if unique {
        if regia_scene::unique_name::unique_conflict(world, &name, Some(entity)).is_some() {
            return Err("unique_name_taken");
        }
        Box::new(
            AddComponent::capture(world, entity, UNIQUE, Box::new(UniqueName))
                .map_err(|_| "unique_name_unregistered")?,
        )
    } else {
        Box::new(
            crate::command::RemoveComponent::capture(world, entity, UNIQUE)
                .map_err(|_| "unique_name_unregistered")?,
        )
    };
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(command);
    Ok(())
}

/// Whether renaming `entity` to `name` breaks the unique-name rule (`Some("unique_name_taken")`).
#[must_use]
pub fn rename_refusal(world: &World, entity: Entity, name: &str) -> Option<&'static str> {
    let marked = world.get::<UniqueName>(entity).is_some();
    (marked && regia_scene::unique_name::unique_conflict(world, name, Some(entity)).is_some())
        .then_some("unique_name_taken")
}

/// Where [`enforce_unique_names`] left off: the change tick it last looked at, the project and
/// level it looked at (another project or level starts over from a baseline), the undo state id it
/// last saw and the highest one it has seen (a larger one means a new edit was pushed since; a
/// different, not larger one means an undo or redo moved through the history).
#[derive(Resource, Debug, Default)]
struct UniqueNamesSince {
    tick: Option<regia_ecs::ecs::change_detection::Tick>,
    scope: (Option<PathBuf>, Option<regia_scene::prelude::LevelId>),
    undo_state: u64,
    undo_high: u64,
}

/// The project and level the unique-name sweep is watching.
fn unique_scope(world: &World) -> (Option<PathBuf>, Option<regia_scene::prelude::LevelId>) {
    (
        open_root(world).map(Path::to_path_buf),
        world
            .get_resource::<crate::level_io::OpenLevel>()
            .map(|open| open.id),
    )
}

/// Keeps the unique names unique after a copy (duplicate, paste) of a marked entity: a marked
/// entity whose name or marker changed since the last call, and whose name another marked entity
/// already holds, gets the next free name. Entities that did not change are never renamed, so a
/// level that was saved with two marked entities of one name keeps them as they are (the rename
/// and toggle refusals still apply). The first call, and the first after another project or level
/// opened, only records where it starts.
///
/// A rename is an undo command: joined into the edit that made the conflict (so one undo removes
/// the copy and its new name together) when that edit is the newest on the undo stack, otherwise
/// pushed as its own step. Returns the renames.
pub fn enforce_unique_names(world: &mut World) -> Vec<(Entity, String)> {
    use regia_ecs::ecs::change_detection::{DetectChanges as _, Ref};
    let scope = unique_scope(world);
    let undo_state = world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id);
    let since = world
        .get_resource::<UniqueNamesSince>()
        .filter(|s| s.scope == scope)
        .and_then(|s| s.tick.map(|tick| (tick, s.undo_state, s.undo_high)));
    let Some((since, seen_undo_state, undo_high)) = since else {
        // Baseline: what is already in the level is not a change.
        let tick = world.increment_change_tick();
        world.insert_resource(UniqueNamesSince {
            tick: Some(tick),
            scope,
            undo_state,
            undo_high: undo_state,
        });
        return Vec::new();
    };
    // A new edit on the undo stack, or a change made outside it (a script, a direct spawn). An
    // undo or redo that brings a conflict back is left alone: renaming it again would push a new
    // step on every undo and the user could never step back past it.
    let new_edit = undo_state > undo_high;
    let history_moved = !new_edit && undo_state != seen_undo_state;
    let now = world.change_tick();
    let mut marked: Vec<(Entity, String, bool)> = {
        let mut query = world.query_filtered::<(Entity, Ref<Name>, Ref<UniqueName>), ()>();
        query
            .iter(world)
            .map(|(entity, name, marker)| {
                let changed = name.last_changed().is_newer_than(since, now)
                    || marker.last_changed().is_newer_than(since, now);
                (entity, name.as_str().to_string(), changed)
            })
            .collect()
    };
    if !marked.iter().any(|(_, _, changed)| *changed) {
        // Idle: nothing to do, and the change tick is left alone.
        if let Some(mut seen) = world.get_resource_mut::<UniqueNamesSince>() {
            seen.undo_state = undo_state;
            seen.undo_high = undo_high.max(undo_state);
        }
        return Vec::new();
    }
    // The entities that did not change keep their names; the changed ones, oldest first, yield.
    marked.sort_by_key(|(entity, _, changed)| (*changed, creation_key(*entity)));
    let mut taken = std::collections::HashSet::new();
    let mut renames = Vec::new();
    for (entity, name, changed) in marked {
        if taken.insert(name.clone()) || !changed || history_moved {
            continue;
        }
        let fresh = regia_scene::unique_name::next_free_name(world, &name, Some(entity));
        let fresh = if taken.contains(&fresh) {
            let mut n = 2;
            loop {
                let candidate = format!("{name} {n}");
                if !taken.contains(&candidate)
                    && regia_scene::unique_name::unique_conflict(world, &candidate, Some(entity)).is_none()
                {
                    break candidate;
                }
                n += 1;
            }
        } else {
            fresh
        };
        taken.insert(fresh.clone());
        renames.push((entity, fresh));
    }
    if !renames.is_empty() {
        let mut group = CommandGroup::new("Keep unique names unique");
        for (entity, fresh) in &renames {
            match SetProperty::capture(world, *entity, "Name", "", Box::new(Name::new(fresh.clone()))) {
                Ok(mut command) => {
                    command.coalesce = false;
                    crate::command::EditorCommand::apply(&mut command, world);
                    group.push(Box::new(command));
                }
                Err(err) => tracing::warn!(target: "regia_editor", "unique name: {err}"),
            }
        }
        let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
        if new_edit && stack.undo_depth() > 0 {
            stack.join_top(Box::new(group));
        } else {
            stack.push(Box::new(group));
            stack.seal();
        }
    }
    // The renames above happen at `now`; the next call looks at what changes after it.
    let tick = world.increment_change_tick();
    let undo_state = world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id);
    world.insert_resource(UniqueNamesSince {
        tick: Some(tick),
        scope,
        undo_state,
        undo_high: undo_high.max(undo_state),
    });
    renames
}

/// Draws the column header: the shown columns' titles and the "Columns" menu (a checkbox per
/// column, a width per shown column, the sort order). Every control is a focusable egui widget.
pub fn header(ui: &mut egui::Ui, world: &mut World) {
    // The tree of the Outliner instance being drawn (`main` for the first; P-003).
    let tree = crate::panel_instances::outliner_tree(world);
    let columns = view_state(world).columns(tree);
    let mut next = columns.clone();
    ui.horizontal(|ui| {
        ui.menu_button("Columns", |ui| {
            for column in Column::ALL {
                let mut shown = next.shows(column);
                if ui.checkbox(&mut shown, column.title()).changed() {
                    next.set_shown(column, shown);
                }
            }
            ui.separator();
            for column in next.shown.clone() {
                let mut width = next.width(column);
                ui.horizontal(|ui| {
                    ui.label(column.title());
                    let drag = ui.add(egui::DragValue::new(&mut width).range(24.0..=400.0).suffix(" pt"));
                    let drag = crate::a11y_audit::named_drag_value(
                        drag,
                        &format!("{} column width", column.title()),
                        f64::from(width),
                    );
                    if drag.changed() {
                        next.widths.insert(column, width);
                    }
                });
            }
            ui.separator();
            ui.radio_value(&mut next.sort, RowSort::Name, "Sort by name");
            ui.radio_value(&mut next.sort, RowSort::Creation, "Sort by creation order");
        });
        for column in &next.shown {
            if *column != Column::VisibleLocked {
                ui.add_sized(
                    [next.width(*column), ui.spacing().interact_size.y],
                    egui::Label::new(egui::RichText::new(column.title()).weak()),
                );
            }
        }
    });
    if next != columns {
        edit_view_state(world, |state| {
            state.trees.insert(tree.to_string(), next);
        });
    }
}

/// Paints the row tint of `entity` (its user color) behind the row at `rect`.
pub fn paint_row_tint(ui: &egui::Ui, world: &World, entity: Entity, rect: egui::Rect) {
    let Some(name) = row_color_of(world, entity) else {
        return;
    };
    let tokens = crate::theme::ui_tokens(ui.ctx());
    if let Some(color) = row_color(&tokens, &name) {
        ui.painter().rect_filled(rect, 2.0, color.gamma_multiply(0.22));
    }
}

/// Draws the row's badges, its configuration-warning badge and the shown cells after the label.
pub fn draw_row_extras(ui: &mut egui::Ui, world: &World, entity: Entity) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    for badge in badges(world, entity) {
        if let Some(name) = badge.open_name() {
            // A badge with an editor is a button that opens it (focusable: Tab, then Enter or Space).
            let open = ui.add(egui::Button::new(badge.glyph()).small().frame(false));
            let enabled = open.enabled();
            open.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
            // The script's hover reads files: only while the pointer rests on the badge.
            let open = if badge == Badge::Script {
                open.on_hover_ui(|ui| {
                    ui.label(script_hover_cached(ui, world, entity));
                })
            } else {
                open.on_hover_text(format!("{}. Click to open it.", badge.spoken()))
            };
            if open.clicked() {
                queue(ui, RowMenuAction::OpenBadge { entity, badge });
            }
        } else {
            let response = ui.label(badge.glyph());
            crate::a11y_audit::name(response, egui::WidgetType::Label, badge.spoken());
        }
    }
    if let Some(checks) = world.get_resource::<crate::config_checks::ConfigChecks>() {
        let warnings = checks.warnings_of(entity);
        if !warnings.is_empty() {
            let text = warnings
                .iter()
                .map(|w| format!("{} ({}) Fix: {}", w.text, w.code, w.fix_hint))
                .collect::<Vec<_>>()
                .join("\n");
            crate::a11y_audit::warning_glyph(ui, tokens.warn, &format!("Configuration warning: {text}"));
        } else if checks.descendant_warned(entity) {
            crate::a11y_audit::warning_glyph(
                ui,
                tokens.warn.gamma_multiply(0.45),
                "A child has a configuration warning",
            );
        }
    }
    let Some(state) = current_view_state(world) else {
        return;
    };
    let columns = state.columns(crate::panel_instances::outliner_tree(world));
    for (column, text) in row_cells(world, entity, &columns) {
        if column == Column::VisibleLocked {
            continue;
        }
        let label = if column == Column::Tags && !text.is_empty() {
            egui::RichText::new(
                tags_of(world, entity)
                    .iter()
                    .map(|t| format!("[{t}]"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .small()
        } else {
            egui::RichText::new(text).weak()
        };
        ui.add_sized(
            [columns.width(column), ui.spacing().interact_size.y],
            egui::Label::new(label).truncate(),
        );
    }
}

/// Whether the eye and lock buttons are shown (the Visible/Locked column).
#[must_use]
pub fn shows_visible_locked(world: &World) -> bool {
    current_view_state(world).is_none_or(|state| {
        state
            .columns(crate::panel_instances::outliner_tree(world))
            .shows(Column::VisibleLocked)
    })
}

/// The creation-order sort of the rows (the tree's siblings by spawn order), when the main tree
/// asks for it.
#[must_use]
pub fn sort_by_creation(world: &World) -> bool {
    current_view_state(world).is_some_and(|state| {
        state.columns(crate::panel_instances::outliner_tree(world)).sort == RowSort::Creation
    })
}

/// The creation-order key of an entity: its generation, then its index (`Entity::to_bits` does not
/// order by index in bevy_ecs 0.20).
#[must_use]
pub fn creation_key(entity: Entity) -> (u32, u32) {
    (entity.generation().to_bits(), entity.index_u32())
}

/// Re-sorts a hierarchy tree's siblings by creation order: entity id order (generation, then
/// index), the allocator's spawn order.
pub fn sort_tree_by_creation(nodes: &mut [crate::outliner::HierarchyNode]) {
    nodes.sort_by_key(|n| creation_key(n.entity));
    for node in nodes {
        sort_tree_by_creation(&mut node.children);
    }
}

/// The context-menu entries of a row: Add tag to selection…, Remove tag ▸, Unique name, Change
/// kind ▸ and Row color ▸.
pub fn row_menu(ui: &mut egui::Ui, world: &World, entity: Entity, targets: &[Entity]) {
    let draft_id = egui::Id::new(("outliner-tag-draft", entity.to_bits()));
    let mut draft = ui
        .ctx()
        .data(|d| d.get_temp::<String>(draft_id))
        .unwrap_or_default();
    ui.menu_button("Add tag to selection…", |ui| {
        let field = ui.add(egui::TextEdit::singleline(&mut draft).hint_text("tag"));
        let field = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "New tag");
        let submit = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("Add").clicked() || submit) && !draft.trim().is_empty() {
            queue(
                ui,
                RowMenuAction::Tag {
                    entities: targets.to_vec(),
                    tag: draft.trim().to_string(),
                    add: true,
                },
            );
            draft.clear();
            ui.close();
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(draft_id, draft));
    let mut tags: Vec<String> = targets.iter().flat_map(|e| tags_of(world, *e)).collect();
    tags.sort();
    tags.dedup();
    ui.add_enabled_ui(!tags.is_empty(), |ui| {
        ui.menu_button("Remove tag", |ui| {
            for tag in tags {
                if ui.button(&tag).clicked() {
                    queue(
                        ui,
                        RowMenuAction::Tag {
                            entities: targets.to_vec(),
                            tag,
                            add: false,
                        },
                    );
                    ui.close();
                }
            }
        });
    });
    if targets.len() == 1 {
        let mut unique = world.get::<UniqueName>(entity).is_some();
        if ui.checkbox(&mut unique, "Unique name").changed() {
            queue(ui, RowMenuAction::Unique { entity, unique });
            ui.close();
        }
        let kinds = crate::change_kind::compatible_kinds(world, entity);
        ui.add_enabled_ui(!kinds.is_empty(), |ui| {
            ui.menu_button("Change kind", |ui| {
                for (kind, label) in kinds {
                    if ui.button(label).clicked() {
                        queue(
                            ui,
                            RowMenuAction::ChangeKind {
                                entity,
                                kind: kind.to_string(),
                            },
                        );
                        ui.close();
                    }
                }
            });
        });
        ui.menu_button("Row color", |ui| {
            for (name, _) in ROW_COLORS {
                if ui.button(name).clicked() {
                    queue(
                        ui,
                        RowMenuAction::Color {
                            entity,
                            color: Some(name.to_string()),
                        },
                    );
                    ui.close();
                }
            }
            if ui.button("No color").clicked() {
                queue(ui, RowMenuAction::Color { entity, color: None });
                ui.close();
            }
        });
    }
}

/// A row-menu action, applied after the panel's draw ([`apply_row_menu_actions`]).
#[derive(Debug, Clone, PartialEq)]
enum RowMenuAction {
    Tag {
        entities: Vec<Entity>,
        tag: String,
        add: bool,
    },
    Unique {
        entity: Entity,
        unique: bool,
    },
    ChangeKind {
        entity: Entity,
        kind: String,
    },
    Color {
        entity: Entity,
        color: Option<String>,
    },
    OpenBadge {
        entity: Entity,
        badge: Badge,
    },
}

fn queue_id() -> egui::Id {
    egui::Id::new("outliner-row-menu-actions")
}

fn queue(ui: &egui::Ui, action: RowMenuAction) {
    ui.ctx().data_mut(|d| {
        d.get_temp_mut_or_default::<Vec<RowMenuAction>>(queue_id())
            .push(action);
    });
}

/// Applies the row-menu actions queued during the draw, and keeps the unique names unique and the
/// configuration warnings current (both run only on change).
pub fn apply_row_menu_actions(ctx: &egui::Context, world: &mut World) {
    let actions = ctx
        .data_mut(|d| d.remove_temp::<Vec<RowMenuAction>>(queue_id()))
        .unwrap_or_default();
    let mut opened: Vec<(Entity, Badge)> = Vec::new();
    for action in actions {
        let result = match action {
            RowMenuAction::Tag { entities, tag, add } => tag_entities(world, &entities, &tag, add).map(drop),
            RowMenuAction::Unique { entity, unique } => set_unique_name(world, entity, unique),
            RowMenuAction::ChangeKind { entity, kind } => {
                crate::change_kind::change_kind(world, entity, &kind)
                    .map(drop)
                    .map_err(|e| e.code())
            }
            RowMenuAction::Color { entity, color } => set_row_color(world, entity, color.as_deref()),
            RowMenuAction::OpenBadge { entity, badge } => {
                // egui may run the panel's UI twice in a frame: one open per badge per frame.
                if opened.contains(&(entity, badge)) {
                    continue;
                }
                opened.push((entity, badge));
                open_badge(world, entity, badge, false)
                    .map(drop)
                    .map_err(|e| e.code)
            }
        };
        if let Err(code) = result {
            tracing::warn!(target: "regia_editor", "outliner: {code}");
        }
    }
    enforce_unique_names(world);
    crate::config_checks::update(world);
}

// ---------------------------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------------------------

fn lane_error(code: &str, message: impl Into<String>, hint: &str) -> regia_agent::protocol::RpcError {
    regia_agent::protocol::RpcError::new(regia_agent::protocol::INVALID_PARAMS, code, message, hint)
}

fn columns_json(columns: &ColumnSet) -> serde_json::Value {
    serde_json::json!({
        "shown": columns.shown.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "widths": Column::ALL.iter().map(|c| (c.name().to_string(), serde_json::json!(columns.width(*c)))).collect::<serde_json::Map<_, _>>(),
        "sort": match columns.sort { RowSort::Name => "name", RowSort::Creation => "creation" },
    })
}

/// `editor.outliner_columns`.
fn outliner_columns_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    use serde_json::Value;
    let tree = params
        .get("tree")
        .and_then(Value::as_str)
        .unwrap_or(MAIN_TREE)
        .to_string();
    let mut columns = view_state(world).columns(&tree);
    let before = columns.clone();
    if let Some(set) = params.get("set").filter(|v| !v.is_null()) {
        let Value::Object(set) = set else {
            return Err(RpcError::wrong_type("set", "an object of column name to boolean"));
        };
        for (name, shown) in set {
            let column = Column::from_name(name).ok_or_else(|| {
                lane_error(
                    "unknown_column",
                    format!("no column `{name}`"),
                    "columns: type, layer, prefab_source, tags, visible_locked, revision",
                )
            })?;
            let shown = shown
                .as_bool()
                .ok_or_else(|| RpcError::wrong_type("set", "an object of column name to boolean"))?;
            columns.set_shown(column, shown);
        }
    }
    if let Some(widths) = params.get("widths").filter(|v| !v.is_null()) {
        let Value::Object(widths) = widths else {
            return Err(RpcError::wrong_type(
                "widths",
                "an object of column name to points",
            ));
        };
        for (name, width) in widths {
            let column = Column::from_name(name).ok_or_else(|| {
                lane_error(
                    "unknown_column",
                    format!("no column `{name}`"),
                    "columns: type, layer, prefab_source, tags, visible_locked, revision",
                )
            })?;
            let width = width
                .as_f64()
                .filter(|w| (24.0..=400.0).contains(w))
                .ok_or_else(|| RpcError::wrong_type("widths", "widths from 24 to 400 points"))?;
            columns.widths.insert(column, width as f32);
        }
    }
    if let Some(sort) = params.get("sort").filter(|v| !v.is_null()) {
        columns.sort = match sort.as_str() {
            Some("name") => RowSort::Name,
            Some("creation") => RowSort::Creation,
            _ => {
                return Err(lane_error(
                    "unknown_sort",
                    "`sort` must be name or creation",
                    "sort: name or creation",
                ));
            }
        };
    }
    if columns != before {
        let saved = columns.clone();
        edit_view_state(world, |state| {
            state.trees.insert(tree.clone(), saved);
        });
    }
    columns_reply(world, &tree, &columns, params)
}

/// The reply of `editor.outliner_columns` and `editor.outliner_view`: the tree's columns, with
/// `entity` that row's cells and badges, the view file and why it is not saved (if it is not).
fn columns_reply(
    world: &mut World,
    tree: &str,
    columns: &ColumnSet,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let entity = match params.get("entity").filter(|v| !v.is_null()) {
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|bits| crate::command::resolve_entity(world, bits))
                .ok_or_else(|| {
                    lane_error(
                        "no_such_entity",
                        "no such entity",
                        "list entities with editor.hierarchy",
                    )
                })?,
        ),
        None => None,
    };
    let mut reply = columns_json(columns);
    reply["tree"] = serde_json::json!(tree);
    if let Some(entity) = entity {
        reply["cells"] = row_cells(world, entity, columns)
            .into_iter()
            .map(|(c, text)| (c.name().to_string(), serde_json::json!(text)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        reply["badges"] =
            serde_json::json!(badges(world, entity).iter().map(|b| b.name()).collect::<Vec<_>>());
    }
    let state = current_view_state(world);
    reply["file"] = serde_json::json!(
        state
            .and_then(|s| s.path.as_ref())
            .map(|p| p.display().to_string())
    );
    reply["file_error"] = serde_json::json!(state.and_then(|s| s.refuse_save.clone()));
    Ok(reply)
}

/// `editor.outliner_view`: the read-only half of `editor.outliner_columns`.
fn outliner_view_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    if let Some(key) = ["set", "widths", "sort"]
        .into_iter()
        .find(|key| params.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(lane_error(
            "view_is_read_only",
            format!("editor.outliner_view does not change `{key}`"),
            "change columns, widths and sort with editor.outliner_columns",
        ));
    }
    let tree = params
        .get("tree")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(MAIN_TREE)
        .to_string();
    let columns = view_state(world).columns(&tree);
    columns_reply(world, &tree, &columns, params)
}

/// `editor.open_sequence`: what the row's sequence badge does.
fn open_sequence_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let entity = params
        .get("entity")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| RpcError::wrong_type("entity", "an entity id"))?;
    let entity = crate::command::resolve_entity(world, entity).ok_or_else(|| {
        lane_error(
            "no_such_entity",
            format!("no entity {entity}"),
            "list entities with editor.hierarchy",
        )
    })?;
    if !badges(world, entity).contains(&Badge::Sequence) {
        return Err(lane_error(
            "no_sequence",
            format!(
                "entity {} plays no sequence and is bound to none",
                entity.to_bits()
            ),
            "the Outliner shows the sequence badge on entities that have one",
        ));
    }
    open_sequence(world, entity);
    Ok(serde_json::json!({
        "entity": entity.to_bits(),
        "selected": world
            .get_resource::<crate::selection::Selection>()
            .map(|s| s.entities().iter().map(|e| e.to_bits()).collect::<Vec<_>>())
            .unwrap_or_default(),
        "panel": "seq_curves",
    }))
}

/// `editor.open_badge`: what clicking one of the row's badges does.
fn open_badge_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let entity = params
        .get("entity")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| RpcError::wrong_type("entity", "an entity id"))?;
    let entity = crate::command::resolve_entity(world, entity).ok_or_else(|| {
        lane_error(
            "no_such_entity",
            format!("no entity {entity}"),
            "list entities with editor.hierarchy",
        )
    })?;
    let name = params
        .get("badge")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| RpcError::wrong_type("badge", "a badge name: script, logic_graph or sequence"))?;
    let badge = [
        Badge::Script,
        Badge::LogicGraph,
        Badge::Sequence,
        Badge::UniqueName,
    ]
    .into_iter()
    .find(|badge| badge.name() == name)
    .ok_or_else(|| {
        lane_error(
            "unknown_badge",
            format!("`{name}` is not a badge"),
            "badge is one of script, logic_graph, sequence (unique_name only marks the row)",
        )
    })?;
    let dry_run = params
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let opened =
        open_badge(world, entity, badge, dry_run).map_err(|e| lane_error(e.code, e.message, e.hint))?;
    let mut reply = serde_json::json!({
        "entity": entity.to_bits(),
        "badge": badge.name(),
        "selected": world
            .get_resource::<crate::selection::Selection>()
            .map(|s| s.entities().iter().map(|e| e.to_bits()).collect::<Vec<_>>())
            .unwrap_or_default(),
        "dry_run": dry_run,
    });
    match opened {
        BadgeOpened::Panel(panel) => {
            reply["opened"] = "panel".into();
            reply["panel"] = panel.into();
        }
        BadgeOpened::CodeEditor { path, line, command } => {
            reply["opened"] = "code_editor".into();
            reply["path"] = path.display().to_string().into();
            reply["line"] = line.into();
            reply["command"] = serde_json::json!(command);
        }
    }
    Ok(reply)
}

/// `editor.tag_selection`.
fn tag_selection_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    use serde_json::{Value, json};
    let (tag, add) = match (
        params.get("add").and_then(Value::as_str),
        params.get("remove").and_then(Value::as_str),
    ) {
        (Some(tag), None) => (tag.to_string(), true),
        (None, Some(tag)) => (tag.to_string(), false),
        _ => {
            return Err(lane_error(
                "tag_op_required",
                "give exactly one of `add` or `remove`",
                "{ add: \"loot\" } or { remove: \"loot\" }",
            ));
        }
    };
    let entities: Vec<Entity> = match params.get("entities").filter(|v| !v.is_null()) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|bits| crate::command::resolve_entity(world, bits))
                    .ok_or_else(|| {
                        lane_error(
                            "no_such_entity",
                            format!("no entity {item}"),
                            "list entities with editor.hierarchy",
                        )
                    })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(RpcError::wrong_type("entities", "an array of entity ids")),
        None => world
            .get_resource::<crate::selection::Selection>()
            .map(|s| s.entities().to_vec())
            .unwrap_or_default(),
    };
    let changed = tag_entities(world, &entities, &tag, add).map_err(|code| {
        lane_error(
            code,
            format!("cannot tag: {code}"),
            "select entities first, and give a non-empty tag",
        )
    })?;
    Ok(json!({
        "tag": tag,
        "added": add,
        "changed": changed.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "entities": entities.iter().map(|e| json!({ "entity": e.to_bits(), "tags": tags_of(world, *e) })).collect::<Vec<_>>(),
        "undo_depth": world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
    }))
}

/// The columns, tag and badge surfaces (`editor.outliner_columns`, `editor.tag_selection`,
/// `editor.outliner_view`, `editor.open_sequence`).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![
        (
            MethodSchema {
                name: "editor.outliner_columns",
                tool: "outliner_columns",
                description: "Change the Outliner's columns per tree instance (type, layer, prefab_source, tags, \
                    visible_locked, revision), their widths and the row sort (name or creation order). `set`, \
                    `widths` and `sort` write the per-user view file .regia/outliner_view.ron (never the level; \
                    a view file that does not parse is left as it is and `file_error` says why). With `entity`, \
                    that row's cells and badges. To read without writing, use editor.outliner_view.",
                params: &[
                    ParamSchema {
                        name: "set",
                        kind: "object",
                        required: false,
                        description: "Column name to shown (true) or hidden (false).",
                    },
                    ParamSchema {
                        name: "widths",
                        kind: "object",
                        required: false,
                        description: "Column name to width in points (24 to 400).",
                    },
                    ParamSchema {
                        name: "sort",
                        kind: "string",
                        required: false,
                        description: "name or creation.",
                    },
                    ParamSchema {
                        name: "tree",
                        kind: "string",
                        required: false,
                        description: "Tree instance (default main).",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: false,
                        description: "An entity id: return its row's cells and badges.",
                    },
                ],
                result: "{ tree, shown, widths, sort, cells?, badges?, file, file_error }",
                mutating: true,
                permission: Permission::EditScene,
            },
            outliner_columns_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.tag_selection",
                tool: "tag_selection",
                description: "Add a tag to, or remove one from, every selected entity (or `entities`) as one undo step \
                    (the Outliner's Add tag to selection… and Remove tag).",
                params: &[
                    ParamSchema {
                        name: "add",
                        kind: "string",
                        required: false,
                        description: "The tag to add.",
                    },
                    ParamSchema {
                        name: "remove",
                        kind: "string",
                        required: false,
                        description: "The tag to remove.",
                    },
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids; absent = the editor selection.",
                    },
                ],
                result: "{ tag, added, changed, entities: [{ entity, tags }], undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tag_selection_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.outliner_view",
                tool: "outliner_view",
                description: "Read the Outliner's columns of a tree instance, their widths and the row sort, and with \
                    `entity` that row's cells and badges. Writes nothing (change columns with \
                    editor.outliner_columns).",
                params: &[
                    ParamSchema {
                        name: "tree",
                        kind: "string",
                        required: false,
                        description: "Tree instance (default main).",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: false,
                        description: "An entity id: return its row's cells and badges.",
                    },
                ],
                result: "{ tree, shown, widths, sort, cells?, badges?, file, file_error }",
                mutating: false,
                permission: Permission::Read,
            },
            outliner_view_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_sequence",
                tool: "open_sequence",
                description: "What the Outliner row's sequence badge does: select the entity (replacing the selection) \
                    and bring the Sequence Curves panel forward (next frame).",
                params: &[ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: true,
                    description: "An entity that plays or is bound to a sequence.",
                }],
                result: "{ entity, selected, panel }",
                mutating: true,
                permission: Permission::EditScene,
            },
            open_sequence_method as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_badge",
                tool: "open_badge",
                description: "What clicking one of an Outliner row's badges does: select the entity, then bring \
                    forward the Sequence Curves panel (`sequence`), show the entity's graph in the Logic Graph \
                    panel (`logic_graph`; refused while that panel holds unsaved edits), or open the declaration \
                    of its script component in the configured code editor (`script`; needs the declaring mod's \
                    folder under the project's mods/). The panels come forward next frame.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "integer",
                        required: true,
                        description: "An entity with the badge.",
                    },
                    ParamSchema {
                        name: "badge",
                        kind: "string",
                        required: true,
                        description: "`script`, `logic_graph` or `sequence`.",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "For a script: resolve the file, line and editor command without starting the editor.",
                    },
                ],
                result: "{ entity, badge, selected, dry_run, opened: \"panel\" | \"code_editor\", panel?, path?, line?, command? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            open_badge_method as crate::viewport_agent::Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::Transform;

    fn app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    #[test]
    fn the_reflected_paths_name_the_types() {
        assert_eq!(TAGS, std::any::type_name::<regia_scene::components::Tags>());
        assert_eq!(UNIQUE, std::any::type_name::<UniqueName>());
    }

    #[test]
    fn toggling_columns_changes_the_row_cells_and_widths_persist() {
        let mut app = app();
        let world = app.world_mut();
        let crate_entity = world
            .spawn((
                Name::new("Crate"),
                EntityGuid::new_random(),
                Transform::default(),
                regia_scene::components::Tags::from_iter_str(["loot", "wood"]),
                regia_scene::components::MapLayer(2),
            ))
            .id();
        let mut columns = ColumnSet::default();
        assert_eq!(
            row_cells(world, crate_entity, &columns),
            [(Column::VisibleLocked, "visible".to_string())]
        );
        columns.set_shown(Column::Tags, true);
        columns.set_shown(Column::Layer, true);
        assert_eq!(
            row_cells(world, crate_entity, &columns),
            [
                (Column::Layer, "2".to_string()),
                (Column::Tags, "loot, wood".to_string()),
                (Column::VisibleLocked, "visible".to_string()),
            ]
        );
        columns.set_shown(Column::VisibleLocked, false);
        columns.set_shown(Column::Revision, true);
        assert_eq!(
            row_cells(world, crate_entity, &columns).last(),
            Some(&(Column::Revision, String::new()))
        );
        // The revision panel's hook fills the slot.
        fn modified(_: &World, _: Entity) -> Option<String> {
            Some("M".into())
        }
        world.insert_resource(RevisionColumn(Some(modified)));
        assert_eq!(cell_text(world, crate_entity, Column::Revision), "M");

        // Widths and the shown set persist through the view-state file.
        let dir = regia_test_utils::unique_temp_dir("outliner-view-state");
        let path = dir.join(VIEW_STATE_FILE);
        let mut state = OutlinerViewState::load(&path);
        columns.widths.insert(Column::Tags, 140.0);
        columns.sort = RowSort::Creation;
        state.trees.insert(MAIN_TREE.into(), columns.clone());
        state.save().expect("save");
        let back = OutlinerViewState::load(&path);
        assert_eq!(back.columns(MAIN_TREE), columns);
        assert_eq!(back.columns(MAIN_TREE).width(Column::Tags), 140.0);
        assert_eq!(back.columns("other"), ColumnSet::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tags_on_the_selection_are_one_undo_step() {
        let mut app = app();
        let world = app.world_mut();
        let a = world
            .spawn((
                Name::new("A"),
                EntityGuid::new_random(),
                regia_scene::components::Tags::from_iter_str(["old"]),
            ))
            .id();
        let b = world.spawn((Name::new("B"), EntityGuid::new_random())).id();
        let changed = tag_entities(world, &[a, b], "loot", true).expect("tagged");
        assert_eq!(changed, [a, b]);
        assert_eq!(tags_of(world, a), ["old", "loot"]);
        assert_eq!(tags_of(world, b), ["loot"]);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        // Tagging again changes nothing and pushes nothing.
        assert!(
            tag_entities(world, &[a, b], "loot", true)
                .expect("noop")
                .is_empty()
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        crate::command::undo_world(world);
        assert_eq!(tags_of(world, a), ["old"]);
        assert!(world.get::<regia_scene::components::Tags>(b).is_none());
        crate::command::redo_world(world);
        tag_entities(world, &[a, b], "loot", false).expect("removed");
        assert_eq!(tags_of(world, a), ["old"]);
        assert!(tags_of(world, b).is_empty());
        assert_eq!(tag_entities(world, &[a], "  ", true), Err("empty_tag"));
        assert_eq!(tag_entities(world, &[], "x", true), Err("no_entities"));
    }

    #[test]
    fn unique_names_refuse_a_second_holder_and_copies_get_the_next_free_name() {
        let mut app = app();
        let world = app.world_mut();
        let door = world.spawn((Name::new("Door"), EntityGuid::new_random())).id();
        let other = world.spawn((Name::new("Door"), EntityGuid::new_random())).id();
        set_unique_name(world, door, true).expect("marked");
        assert!(badges(world, door).contains(&Badge::UniqueName));
        assert_eq!(set_unique_name(world, other, true), Err("unique_name_taken"));
        // Rename to a taken unique name is refused.
        let gate = world
            .spawn((Name::new("Gate"), EntityGuid::new_random(), UniqueName))
            .id();
        assert_eq!(rename_refusal(world, gate, "Door"), Some("unique_name_taken"));
        assert_eq!(rename_refusal(world, gate, "Gate 2"), None);
        assert_eq!(
            rename_refusal(world, other, "Door"),
            None,
            "unmarked entities may share it"
        );
        enforce_unique_names(world);
        // A copy (as a duplicate makes) is renamed to the next free name.
        let copy = world
            .spawn((Name::new("Door"), EntityGuid::new_random(), UniqueName))
            .id();
        let renames = enforce_unique_names(world);
        assert_eq!(renames, [(copy, "Door 2".to_string())]);
        assert_eq!(
            world.get::<Name>(copy).map(|n| n.as_str().to_string()).as_deref(),
            Some("Door 2")
        );
        assert_eq!(regia_scene::unique_name::find_unique(world, "Door"), Some(door));
        // Idle: nothing changed, nothing renamed.
        assert!(enforce_unique_names(world).is_empty());
        // The copy was spawned outside the undo stack, so its rename is its own undo step
        // (review rev-g3 finding 2: a rename the sweep makes is an editor command).
        assert!(crate::command::undo_world(world));
        assert_eq!(
            world.get::<Name>(copy).map(|n| n.as_str().to_string()).as_deref(),
            Some("Door")
        );
        // Undo of the toggle removes the marker.
        crate::command::undo_world(world);
        assert!(world.get::<UniqueName>(door).is_none());
    }

    fn name_of(world: &World, entity: Entity) -> String {
        world
            .get::<Name>(entity)
            .map(|n| n.as_str().to_string())
            .unwrap_or_default()
    }

    /// Review rev-g3 finding 2: the first sweep must not rename what a saved level already holds,
    /// an idle sweep must not move the change tick, and the rename of a new conflict is part of
    /// the undo step of the edit that made it (a duplicate names its copy itself, ADR-7757).
    #[test]
    fn a_loaded_level_keeps_its_names_and_a_new_copy_is_renamed_in_the_same_undo_step() {
        let mut app = app();
        let world = app.world_mut();
        // A level saved with two marked "Door" entities (a hand edit or an old save).
        let first = world
            .spawn((
                Name::new("Door"),
                EntityGuid::new_random(),
                Transform::default(),
                UniqueName,
            ))
            .id();
        let second = world
            .spawn((
                Name::new("Door"),
                EntityGuid::new_random(),
                Transform::default(),
                UniqueName,
            ))
            .id();
        world.insert_resource(UndoStack::new());
        assert!(
            enforce_unique_names(world).is_empty(),
            "the first sweep renamed an entity the level was loaded with"
        );
        assert_eq!(name_of(world, first), "Door");
        assert_eq!(name_of(world, second), "Door");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        // Idle sweeps change nothing, not even the change tick.
        let tick = world.change_tick();
        assert!(enforce_unique_names(world).is_empty());
        assert!(enforce_unique_names(world).is_empty());
        assert_eq!(world.change_tick(), tick, "an idle sweep moved the change tick");
        // A duplicate of a marked entity: since w12-outliner-tree the duplicate names the copy
        // itself ("Door" -> "Door 2", ADR-7757), inside its own undo step, so the sweep finds
        // nothing to rename and adds no step.
        let copies = crate::hierarchy_ops::duplicate_entities(world, &[first], regia_core::math::Vec3::ZERO)
            .expect("duplicate");
        let copy = copies[0];
        assert_eq!(name_of(world, copy), "Door 2");
        let depth = world.resource::<UndoStack>().undo_depth();
        assert!(enforce_unique_names(world).is_empty());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
        // An edit that makes a conflict (the copy renamed back to "Door"): the sweep renames the
        // new holder, not the loaded ones, inside that edit's undo step.
        crate::outliner::rename_entity(world, copy, "Door").expect("rename");
        let depth = world.resource::<UndoStack>().undo_depth();
        assert_eq!(enforce_unique_names(world), [(copy, "Door 2".to_string())]);
        assert_eq!(name_of(world, first), "Door");
        assert_eq!(name_of(world, second), "Door", "an unchanged entity was renamed");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            depth,
            "the rename joins the edit's undo step"
        );
        // One undo takes back the edit and its rename together; the sweep leaves it alone.
        assert!(crate::command::undo_world(world));
        assert_eq!(name_of(world, copy), "Door 2");
        assert!(enforce_unique_names(world).is_empty());
        // One more undo removes the copy.
        assert!(crate::command::undo_world(world));
        assert!(world.get_entity(copy).is_err());
        assert!(enforce_unique_names(world).is_empty());
        // Redo brings the copy back under its new name, and the edit with its rename, with
        // nothing left to rename.
        assert!(crate::command::redo_world(world));
        assert!(enforce_unique_names(world).is_empty());
        assert!(crate::command::redo_world(world));
        let names: Vec<String> = world
            .query_filtered::<&Name, With<UniqueName>>()
            .iter(world)
            .map(|n| n.as_str().to_string())
            .collect();
        assert_eq!(names.iter().filter(|n| *n == "Door 2").count(), 1, "{names:?}");
        assert!(enforce_unique_names(world).is_empty());
        assert_eq!(world.resource::<UndoStack>().redo_depth(), 0);
    }

    /// Review rev-g3 finding 2: opening another level starts the sweep over from a baseline.
    #[test]
    fn another_level_starts_the_unique_name_sweep_over() {
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::prelude::LevelId(regia_core::id::Guid::new_random()),
            path: None,
        });
        assert!(enforce_unique_names(world).is_empty());
        // The next level arrives with two marked "Gate" entities.
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::prelude::LevelId(regia_core::id::Guid::new_random()),
            path: None,
        });
        let a = world
            .spawn((Name::new("Gate"), EntityGuid::new_random(), UniqueName))
            .id();
        let b = world
            .spawn((Name::new("Gate"), EntityGuid::new_random(), UniqueName))
            .id();
        assert!(
            enforce_unique_names(world).is_empty(),
            "the newly opened level's names were treated as edits"
        );
        assert_eq!(
            (name_of(world, a), name_of(world, b)),
            ("Gate".into(), "Gate".into())
        );
    }

    /// Review rev-g3 finding 1: a view-state file that does not parse is never overwritten.
    #[test]
    fn an_unreadable_view_state_file_is_kept_byte_for_byte() {
        let dir = regia_test_utils::unique_temp_dir("outliner-view-bad");
        let path = dir.join(VIEW_STATE_FILE);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        let bad =
            "(trees: { \"main\": (shown: [tags], widths: { tags: 140.0 }, sort: creation) }, colors: {\n";
        std::fs::write(&path, bad).expect("write");
        let mut state = OutlinerViewState::load(&path);
        assert!(state.refuse_save.is_some(), "a parse error must refuse saves");
        state.trees.insert(MAIN_TREE.into(), ColumnSet::default());
        assert!(state.save().is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            bad,
            "the unreadable file was replaced"
        );
        // A missing file is a fresh default that saves.
        let fresh = dir.join("other").join(VIEW_STATE_FILE);
        let state = OutlinerViewState::load(&fresh);
        assert!(state.refuse_save.is_none());
        state.save().expect("a new file saves");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Review rev-g3 finding 5: opening another project reads that project's view file, and an
    /// edit there never writes the first project's file or paints its colors.
    #[test]
    fn another_project_reads_and_writes_its_own_view_file() {
        let mut app = app();
        let world = app.world_mut();
        let base = regia_test_utils::unique_temp_dir("outliner-view-switch");
        let (a, b) = (base.join("a"), base.join("b"));
        for root in [&a, &b] {
            std::fs::create_dir_all(root.join(".regia")).expect("dir");
        }
        let door = world.spawn((Name::new("Door"), EntityGuid::new_random())).id();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(a.clone()),
        });
        edit_view_state(world, |state| {
            state
                .trees
                .entry(MAIN_TREE.into())
                .or_default()
                .set_shown(Column::Tags, true);
        });
        set_row_color(world, door, Some("red")).expect("colored");
        let a_bytes = std::fs::read_to_string(a.join(VIEW_STATE_FILE)).expect("A saved");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(b.clone()),
        });
        assert!(
            !view_state(world).columns(MAIN_TREE).shows(Column::Tags),
            "project B shows project A's columns"
        );
        assert_eq!(
            row_color_of(world, door),
            None,
            "project B paints project A's row colors"
        );
        edit_view_state(world, |state| {
            state
                .trees
                .entry(MAIN_TREE.into())
                .or_default()
                .set_shown(Column::Layer, true);
        });
        assert_eq!(
            std::fs::read_to_string(a.join(VIEW_STATE_FILE)).expect("A"),
            a_bytes,
            "B's edit was written to A's file"
        );
        assert!(
            OutlinerViewState::load(&b.join(VIEW_STATE_FILE))
                .columns(MAIN_TREE)
                .shows(Column::Layer)
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// Review rev-g3 findings 1 and 5: through the editor's own edit path, a bad file stays as it
    /// is until it is fixed, and another project reads (and writes) its own file.
    #[test]
    fn view_state_follows_the_open_project_and_never_writes_a_bad_file() {
        let mut app = app();
        let world = app.world_mut();
        let base = regia_test_utils::unique_temp_dir("outliner-view-projects");
        let (a, b) = (base.join("a"), base.join("b"));
        for root in [&a, &b] {
            std::fs::create_dir_all(root.join(".regia")).expect("dir");
        }
        let bad = "not ron at all {";
        std::fs::write(a.join(VIEW_STATE_FILE), bad).expect("write");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(a.clone()),
        });
        edit_view_state(world, |state| {
            state
                .trees
                .entry(MAIN_TREE.into())
                .or_default()
                .set_shown(Column::Tags, true);
        });
        assert_eq!(
            std::fs::read_to_string(a.join(VIEW_STATE_FILE)).expect("read"),
            bad,
            "a column toggle replaced the unreadable view file"
        );
        assert!(
            view_state(world).columns(MAIN_TREE).shows(Column::Tags),
            "the edit applies in memory"
        );
        // Project B opens: its own (missing) file, not A's state.
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(b.clone()),
        });
        assert!(
            !view_state(world).columns(MAIN_TREE).shows(Column::Tags),
            "B shows A's columns"
        );
        edit_view_state(world, |state| {
            state
                .trees
                .entry(MAIN_TREE.into())
                .or_default()
                .set_shown(Column::Layer, true);
        });
        assert!(b.join(VIEW_STATE_FILE).is_file(), "B's edit is saved to B's file");
        assert_eq!(
            std::fs::read_to_string(a.join(VIEW_STATE_FILE)).expect("read"),
            bad,
            "B's edit was written to A's file"
        );
        assert!(
            !OutlinerViewState::load(&b.join(VIEW_STATE_FILE))
                .columns(MAIN_TREE)
                .shows(Column::Tags)
        );
        // Back to A after the user deleted the bad file: the next edit saves a fresh file.
        std::fs::remove_file(a.join(VIEW_STATE_FILE)).expect("delete");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(a.clone()),
        });
        edit_view_state(world, |state| {
            state
                .trees
                .entry(MAIN_TREE.into())
                .or_default()
                .set_shown(Column::Type, true);
        });
        let saved = OutlinerViewState::load(&a.join(VIEW_STATE_FILE));
        assert!(saved.refuse_save.is_none());
        assert!(saved.columns(MAIN_TREE).shows(Column::Type));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn row_colors_are_theme_tokens_and_creation_sort_follows_spawn_order() {
        let tokens = crate::theme::ThemeTokens::regia_default();
        for (name, slot) in ROW_COLORS {
            assert_eq!(row_color(&tokens, name), Some(tokens.data_kinds[slot]));
        }
        assert_eq!(row_color(&tokens, "magenta"), None);
        let mut app = app();
        let world = app.world_mut();
        let zed = world.spawn((Name::new("Zed"), EntityGuid::new_random())).id();
        let alpha = world.spawn((Name::new("Alpha"), EntityGuid::new_random())).id();
        assert_eq!(set_row_color(world, zed, Some("magenta")), Err("unknown_color"));
        set_row_color(world, zed, Some("sky")).expect("colored");
        assert_eq!(row_color_of(world, zed).as_deref(), Some("sky"));
        set_row_color(world, zed, None).expect("cleared");
        assert_eq!(row_color_of(world, zed), None);
        let mut tree: Vec<crate::outliner::HierarchyNode> = crate::outliner::hierarchy_tree(world)
            .into_iter()
            .filter(|n| n.entity == zed || n.entity == alpha)
            .collect();
        assert_eq!(tree.iter().map(|n| n.entity).collect::<Vec<_>>(), [alpha, zed]);
        sort_tree_by_creation(&mut tree);
        assert_eq!(tree.iter().map(|n| n.entity).collect::<Vec<_>>(), [zed, alpha]);
    }
}
