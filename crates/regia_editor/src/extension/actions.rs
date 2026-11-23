// SPDX-License-Identifier: MPL-2.0
//! Editor actions and asset-type tools extensions register (ADR-9850 (2), `specs/57` § 3.3 item 3),
//! and the agent surface of everything an extension adds to the editor's UI.
//!
//! An **action** is a menu entry (the Extensions menu of the menu bar) and optionally a toolbar
//! button; running it calls the extension's callback against the edited world. An **asset tool**
//! acts on one asset file of the types it names by file suffix (`.ability.ron`, `.png`). Panels
//! come from `ToolExtensionContext::register_editor_panel`. All three are registered through
//! `ToolExtensionContext` (capability `editor_panel`), owner-tracked in the extension registry
//! (`plugin.info` lists them under `editor_panel`, `editor_action` and `asset_tool`), removed with
//! the extension's other registrations on a reload, and shown only while the registry still
//! records the owner's claim.
//!
//! The agent methods run the same functions the menu, the toolbar and the panel tabs do:
//! `editor.extension_ui` lists them, `editor.extension_action` runs an action,
//! `editor.extension_asset_tool` runs a tool on a project asset and `editor.extension_panel` draws
//! a panel once off screen and answers the text it shows (`book/agents/editor-extensions.md`).
//! `editor.command { run }` also runs an action by id.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::extension::ExtensionRegistry;
use regia_framework::extension::registry::RegistrationKind;
use serde_json::{Value, json};

use super::context::{DockRegion, EditorExtensions, PanelDesc, PanelDraw};
use crate::command_palette::{COMMANDS, CommandDef, CommandKeymap, KeyChord};

/// Runs an action against the edited world; `Ok` is the status line the editor shows.
pub type ActionRun = Arc<dyn Fn(&mut World) -> Result<String, String> + Send + Sync>;

/// A menu (and optionally toolbar) action an extension registers. It is also a command: the
/// palette lists it under [`Self::category`], `editor.command { run }` runs it by id, and its
/// [`Self::shortcut`] is a default the user can rebind in the Keymap tab (ADR-7791).
#[derive(Clone)]
pub struct ActionDesc {
    /// `<ext>.<action>`.
    pub id: String,
    /// The menu entry and button text.
    pub title: String,
    /// A submenu of the Extensions menu (`"Greeter"`); empty puts the entry at the top level.
    pub menu: String,
    /// Whether the action also gets a toolbar button.
    pub toolbar: bool,
    /// The callback.
    pub run: ActionRun,
    /// The command palette's category; empty files it under the extension's display name.
    pub category: String,
    /// The default shortcut in the Keymap's spelling (`Ctrl+Alt+G`; `Ctrl` is Cmd on macOS);
    /// empty for none. A default another command already uses is listed as a conflict and does
    /// not run.
    pub shortcut: String,
    /// Context menus that also list the action (the Extensions menu always does).
    pub places: Vec<ActionPlace>,
    /// When the action can run (`None`: always). A disabled entry shows greyed out.
    pub enabled_when: Option<SelectionPredicate>,
    /// When the action shows in menus, the toolbar and the palette (`None`: always).
    pub visible_when: Option<SelectionPredicate>,
}

impl ActionDesc {
    /// An action with no submenu, toolbar button, category, shortcut, context menu or predicate;
    /// set those with struct update syntax (`ActionDesc { shortcut: .., ..ActionDesc::new(..) }`).
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, run: ActionRun) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            menu: String::new(),
            toolbar: false,
            run,
            category: String::new(),
            shortcut: String::new(),
            places: Vec::new(),
            enabled_when: None,
            visible_when: None,
        }
    }
}

/// A context menu an action can join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionPlace {
    /// The Outliner's row menu (the level selection).
    Outliner,
    /// The viewport's right-click menu (the level selection).
    Viewport,
    /// A Content Browser asset's menu (that asset).
    ContentBrowser,
}

impl ActionPlace {
    /// The lowercase name the agent surface uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Outliner => "outliner",
            Self::Viewport => "viewport",
            Self::ContentBrowser => "content_browser",
        }
    }
}

/// What a selection holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelectionKind {
    /// Level entities (Outliner, viewport).
    Entity,
    /// Project assets (Content Browser).
    Asset,
}

/// A condition on the selection a menu entry acts on. Every part must hold; an empty part holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionPredicate {
    /// What the selection may hold; a selection holding another kind fails.
    pub kinds: Vec<SelectionKind>,
    /// Components every selected entity must have, by short type name (`Name`, `PointLight`),
    /// case-insensitive, as the Outliner's `type:` search term reads them.
    pub components: Vec<String>,
    /// File suffixes every selected asset must end with (one of them), case-insensitive.
    pub asset_suffixes: Vec<String>,
    /// The least number of selected items (entities plus assets).
    pub min_count: usize,
    /// The most (`None`: no limit).
    pub max_count: Option<usize>,
}

impl SelectionPredicate {
    /// Whether `selection` meets the predicate in `world`.
    #[must_use]
    pub fn holds(&self, world: &World, selection: &MenuSelection) -> bool {
        let count = selection.entities.len() + selection.assets.len();
        if count < self.min_count || self.max_count.is_some_and(|max| count > max) {
            return false;
        }
        if !self.kinds.is_empty()
            && ((!selection.entities.is_empty() && !self.kinds.contains(&SelectionKind::Entity))
                || (!selection.assets.is_empty() && !self.kinds.contains(&SelectionKind::Asset)))
        {
            return false;
        }
        if !self.components.is_empty() {
            let wanted: Vec<String> = self
                .components
                .iter()
                .map(|c| c.rsplit("::").next().unwrap_or(c).to_lowercase())
                .collect();
            for &entity in &selection.entities {
                if world.get_entity(entity).is_err() {
                    return false;
                }
                let (have, _) = crate::kind_icons::row_terms(world, entity);
                if !wanted.iter().all(|w| have.contains(w)) {
                    return false;
                }
            }
        }
        if !self.asset_suffixes.is_empty() {
            let suffixes: Vec<String> = self
                .asset_suffixes
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            for asset in &selection.assets {
                let lower = asset.to_ascii_lowercase();
                if !suffixes.iter().any(|s| lower.ends_with(s.as_str())) {
                    return false;
                }
            }
        }
        true
    }

    fn to_json(&self) -> Value {
        json!({
            "kinds": self.kinds.iter().map(|k| match k {
                SelectionKind::Entity => "entity",
                SelectionKind::Asset => "asset",
            }).collect::<Vec<_>>(),
            "components": self.components,
            "asset_suffixes": self.asset_suffixes,
            "min_count": self.min_count,
            "max_count": self.max_count,
        })
    }
}

/// The selection an action runs on. The editor inserts it before it runs an action, so the
/// callback reads it from the world: the level selection for the Extensions menu, the toolbar,
/// the palette, shortcuts, the Outliner and the viewport; the asset for a Content Browser entry.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuSelection {
    /// Selected level entities, oldest first.
    pub entities: Vec<Entity>,
    /// Selected project assets (project-relative, forward slashes).
    pub assets: Vec<String>,
}

impl MenuSelection {
    /// The level selection.
    #[must_use]
    pub fn level(world: &World) -> Self {
        Self {
            entities: world
                .get_resource::<crate::selection::Selection>()
                .map(|s| s.entities().to_vec())
                .unwrap_or_default(),
            assets: Vec::new(),
        }
    }
}

/// `(visible, enabled)` of `action` for `selection`. A hidden action is never enabled.
#[must_use]
pub fn action_state(world: &World, action: &ActionDesc, selection: &MenuSelection) -> (bool, bool) {
    let visible = action
        .visible_when
        .as_ref()
        .is_none_or(|p| p.holds(world, selection));
    let enabled = visible
        && action
            .enabled_when
            .as_ref()
            .is_none_or(|p| p.holds(world, selection));
    (visible, enabled)
}

/// The live actions that joined the context menu `place`, `(owner, action)` by id.
#[must_use]
pub fn place_actions(world: &World, place: ActionPlace) -> Vec<(String, ActionDesc)> {
    actions(world)
        .into_iter()
        .filter(|(_, action)| action.places.contains(&place))
        .collect()
}

/// The asset an asset tool runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetToolInput {
    /// Project-relative path, forward slashes (`assets/abilities/bolt.ability.ron`).
    pub path: String,
    /// The file on disk.
    pub file: PathBuf,
}

/// Runs an asset tool; `Ok` is the tool's JSON answer.
pub type AssetToolRun = Arc<dyn Fn(&mut World, &AssetToolInput) -> Result<Value, String> + Send + Sync>;

/// A tool for one asset type an extension registers.
#[derive(Clone)]
pub struct AssetToolDesc {
    /// `<ext>.<tool>`.
    pub id: String,
    /// The menu entry text.
    pub title: String,
    /// File suffixes the tool takes, matched case-insensitively (`.ability.ron`, `.png`).
    pub suffixes: Vec<String>,
    /// The callback.
    pub run: AssetToolRun,
}

impl AssetToolDesc {
    /// Whether the tool takes `path` (by suffix, case-insensitive).
    #[must_use]
    pub fn takes(&self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        self.suffixes
            .iter()
            .any(|suffix| lower.ends_with(&suffix.to_ascii_lowercase()))
    }
}

/// Registered entries whose owner the extension registry still records for `kind` (a disabled or
/// reloaded-away extension's entries disappear without touching [`EditorExtensions`]).
fn live<T: Clone>(
    world: &World,
    kind: RegistrationKind,
    map: impl Fn(&EditorExtensions) -> &BTreeMap<String, (String, T)>,
) -> Vec<(String, T)> {
    let (Some(editor), Some(registry)) = (
        world.get_resource::<EditorExtensions>(),
        world.get_resource::<ExtensionRegistry>(),
    ) else {
        return Vec::new();
    };
    map(editor)
        .iter()
        .filter(|(name, (owner, _))| registry.owner_of(kind, name) == Some(owner.as_str()))
        .map(|(_, (owner, desc))| (owner.clone(), desc.clone()))
        .collect()
}

/// `(owner, action)` of every live action, by id.
#[must_use]
pub fn actions(world: &World) -> Vec<(String, ActionDesc)> {
    live(world, RegistrationKind::EditorAction, |editor| &editor.actions)
}

/// `(owner, tool)` of every live asset tool, by id.
#[must_use]
pub fn asset_tools(world: &World) -> Vec<(String, AssetToolDesc)> {
    live(world, RegistrationKind::AssetTool, |editor| &editor.asset_tools)
}

/// `(owner, panel)` of every live extension panel, by id.
#[must_use]
pub fn panels(world: &World) -> Vec<(String, PanelDesc)> {
    live(world, RegistrationKind::EditorPanel, |editor| &editor.panels)
}

/// The live asset tools that take `path`.
#[must_use]
pub fn tools_for(world: &World, path: &str) -> Vec<(String, AssetToolDesc)> {
    asset_tools(world)
        .into_iter()
        .filter(|(_, tool)| tool.takes(path))
        .collect()
}

fn refuse(code: i64, reason: &str, message: String, hint: &str) -> RpcError {
    RpcError::new(code, reason, message, hint)
}

/// Runs the live action `id`: `(owner, status line)`.
///
/// # Errors
///
/// `extension_action_not_found`; `extension_action_disabled` when its predicates do not hold for
/// the level selection; `extension_action_failed` with the callback's message (a panic in the
/// callback is caught and reported the same way).
pub fn run_action(world: &mut World, id: &str) -> Result<(String, String), RpcError> {
    let selection = MenuSelection::level(world);
    run_action_on(world, id, selection)
}

/// Runs the live action `id` on `selection` (inserted as [`MenuSelection`] first): `(owner,
/// status line)`.
///
/// # Errors
///
/// `extension_action_not_found`; `extension_action_disabled` when its predicates do not hold for
/// `selection`; `extension_action_failed` with the callback's message (a panic is caught).
pub fn run_action_on(
    world: &mut World,
    id: &str,
    selection: MenuSelection,
) -> Result<(String, String), RpcError> {
    let Some((owner, action)) = actions(world).into_iter().find(|(_, a)| a.id == id) else {
        return Err(refuse(
            protocol::NOT_FOUND,
            "extension_action_not_found",
            format!("no extension action `{id}`"),
            "editor.extension_ui lists the actions of the loaded extensions",
        ));
    };
    if !action_state(world, &action, &selection).1 {
        return Err(refuse(
            protocol::INVALID_PARAMS,
            "extension_action_disabled",
            format!("`{id}` does not apply to the current selection"),
            "editor.extension_ui shows each action's enabled_when and visible_when",
        ));
    }
    world.insert_resource(selection);
    let _span = tracing::info_span!("extension_action", id, owner = owner.as_str()).entered();
    let run = Arc::clone(&action.run);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(world))) {
        Ok(Ok(message)) => Ok((owner, message)),
        Ok(Err(message)) => Err(refuse(
            protocol::INTERNAL_ERROR,
            "extension_action_failed",
            format!("{id}: {message}"),
            "the extension reported the failure; see its documentation",
        )),
        Err(_) => Err(refuse(
            protocol::INTERNAL_ERROR,
            "extension_action_failed",
            format!("{id}: the action panicked"),
            "the extension's callback panicked; the editor kept running",
        )),
    }
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| {
            refuse(
                protocol::NOT_FOUND,
                "no_project",
                "no project is open in the editor".into(),
                "start the editor with --project <dir>",
            )
        })
}

/// Runs the live asset tool `id` on the project-relative `path`: `(owner, answer)`.
///
/// # Errors
///
/// `asset_tool_not_found`; `no_project`; `bad_path` (absolute, `..`, or no such file);
/// `asset_tool_wrong_type` when the tool does not take the file's suffix; `asset_tool_failed`
/// with the callback's message (a panic is caught).
pub fn run_asset_tool(world: &mut World, id: &str, path: &str) -> Result<(String, Value), RpcError> {
    let Some((owner, tool)) = asset_tools(world).into_iter().find(|(_, t)| t.id == id) else {
        return Err(refuse(
            protocol::NOT_FOUND,
            "asset_tool_not_found",
            format!("no extension asset tool `{id}`"),
            "editor.extension_ui lists the asset tools of the loaded extensions",
        ));
    };
    let root = project_root(world)?;
    let relative = regia_core::project_path::check_relative(path).map_err(|error| {
        refuse(
            protocol::INVALID_PARAMS,
            "bad_path",
            format!("`{path}`: {error}"),
            "pass a project-relative path such as assets/abilities/bolt.ability.ron",
        )
    })?;
    let file = root.join(relative);
    if !file.is_file() {
        return Err(refuse(
            protocol::NOT_FOUND,
            "bad_path",
            format!("`{path}` is not a file of the project"),
            "pass a project-relative path such as assets/abilities/bolt.ability.ron",
        ));
    }
    if !tool.takes(path) {
        return Err(refuse(
            protocol::INVALID_PARAMS,
            "asset_tool_wrong_type",
            format!("`{id}` takes {} files, not `{path}`", tool.suffixes.join(", ")),
            "editor.extension_ui lists each tool's suffixes",
        ));
    }
    let _span = tracing::info_span!("extension_asset_tool", id, owner = owner.as_str()).entered();
    let input = AssetToolInput {
        path: path.replace('\\', "/"),
        file,
    };
    let run = Arc::clone(&tool.run);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(world, &input))) {
        Ok(Ok(answer)) => Ok((owner, answer)),
        Ok(Err(message)) => Err(refuse(
            protocol::INTERNAL_ERROR,
            "asset_tool_failed",
            format!("{id}: {message}"),
            "the extension reported the failure; see its documentation",
        )),
        Err(_) => Err(refuse(
            protocol::INTERNAL_ERROR,
            "asset_tool_failed",
            format!("{id}: the tool panicked"),
            "the extension's callback panicked; the editor kept running",
        )),
    }
}

fn status_line(result: Result<(String, String), RpcError>) -> String {
    match result {
        Ok((_, message)) => message,
        Err(error) => error.message,
    }
}

/// The Extensions menu of the menu bar: every live action shown for the level selection, grouped
/// by its submenu (a disabled one greyed out). Returns the status line of an action run this
/// frame. Draws nothing when no extension registered one.
pub fn menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let rows = actions(world);
    if rows.is_empty() {
        return None;
    }
    let selection = MenuSelection::level(world);
    let mut groups: BTreeMap<String, Vec<(String, String, bool)>> = BTreeMap::new();
    for (_, action) in rows {
        let (visible, enabled) = action_state(world, &action, &selection);
        if visible {
            groups
                .entry(action.menu.clone())
                .or_default()
                .push((action.id, action.title, enabled));
        }
    }
    let mut clicked = None;
    ui.menu_button("Extensions", |ui| {
        for (group, entries) in &groups {
            let mut list = |ui: &mut egui::Ui| {
                for (id, title, enabled) in entries {
                    if ui
                        .add_enabled(*enabled, egui::Button::new(title))
                        .on_hover_text(id)
                        .clicked()
                    {
                        clicked = Some(id.clone());
                        ui.close();
                    }
                }
            };
            if group.is_empty() {
                list(ui);
            } else {
                ui.menu_button(group, list);
            }
        }
    });
    clicked.map(|id| status_line(run_action(world, &id)))
}

/// Toolbar buttons of the live actions that asked for one and show for the level selection.
/// Returns the status line of an action run this frame.
pub fn toolbar(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let selection = MenuSelection::level(world);
    let rows: Vec<(ActionDesc, bool)> = actions(world)
        .into_iter()
        .map(|(_, action)| action)
        .filter(|action| action.toolbar)
        .filter_map(|action| {
            let (visible, enabled) = action_state(world, &action, &selection);
            visible.then_some((action, enabled))
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    ui.separator();
    let mut clicked = None;
    for (action, enabled) in rows {
        if ui
            .add_enabled(enabled, egui::Button::new(&action.title))
            .on_hover_text(&action.id)
            .clicked()
        {
            clicked = Some(action.id);
        }
    }
    clicked.map(|id| status_line(run_action(world, &id)))
}

/// Menu entries of the actions that joined the context menu `place` and show for `selection`
/// (greyed out when disabled), with a separator before them. Returns the id of the entry clicked
/// this frame; a panel that draws with a shared world runs it later with [`run_action_on`].
pub fn context_entries(
    ui: &mut egui::Ui,
    world: &World,
    place: ActionPlace,
    selection: &MenuSelection,
) -> Option<String> {
    let rows: Vec<(ActionDesc, bool)> = place_actions(world, place)
        .into_iter()
        .filter_map(|(_, action)| {
            let (visible, enabled) = action_state(world, &action, selection);
            visible.then_some((action, enabled))
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    ui.separator();
    let mut clicked = None;
    for (action, enabled) in rows {
        if ui
            .add_enabled(enabled, egui::Button::new(action.title.as_str()))
            .on_hover_text(&action.id)
            .clicked()
        {
            clicked = Some(action.id);
            ui.close();
        }
    }
    clicked
}

/// [`context_entries`], then runs the clicked entry on `selection`. Returns its status line.
pub fn context_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    place: ActionPlace,
    selection: &MenuSelection,
) -> Option<String> {
    let id = context_entries(ui, world, place, selection)?;
    Some(status_line(run_action_on(world, &id, selection.clone())))
}

/// Menu entries for the asset tools that take `path`, then the actions that joined the Content
/// Browser's menu and show for that asset (for a content browser's context menu). Returns the
/// outcome line of a tool or action run this frame.
pub fn asset_menu(ui: &mut egui::Ui, world: &mut World, path: &str) -> Option<String> {
    let tools = tools_for(world, path);
    let mut clicked = None;
    for (_, tool) in tools {
        if ui.button(&tool.title).on_hover_text(&tool.id).clicked() {
            clicked = Some(tool.id);
            ui.close();
        }
    }
    let selection = MenuSelection {
        entities: Vec::new(),
        assets: vec![path.replace('\\', "/")],
    };
    let action = context_menu(ui, world, ActionPlace::ContentBrowser, &selection);
    let tool = clicked.map(|id| match run_asset_tool(world, &id, path) {
        Ok((_, answer)) => format!("{id}: {answer}"),
        Err(error) => error.message,
    });
    tool.or(action)
}

// ---------------------------------------------------------------------------------------------
// Commands: palette rows and shortcuts (ADR-7791 (1))

/// The extension's display name (its manifest's `name`), else its id.
pub(crate) fn display_name(world: &World, owner: &str) -> String {
    world
        .get_resource::<ExtensionRegistry>()
        .and_then(|registry| registry.discovered(owner))
        .map_or_else(|| owner.to_owned(), |found| found.manifest.name.clone())
}

/// A `'static` copy of `text`, made once per distinct text: palette rows are `CommandDef`s, whose
/// fields are `'static`. Extension ids, titles and categories are few and stable, so the table
/// stays small (a reload with the same strings adds nothing).
fn intern(text: &str) -> &'static str {
    static TABLE: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let mut table = TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = table.get(text) {
        return found;
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    table.insert(leaked);
    leaked
}

/// The user's shortcuts for extension actions: a chord, or `None` where the user cleared the
/// default. Saved in `editor_keymap.ron` beside the built-in bindings (the file keeps ids it does
/// not know, so no format change): a cleared shortcut is an empty chord.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ExtensionShortcuts {
    /// Action id → user chord (`None`: cleared).
    pub user: BTreeMap<String, Option<KeyChord>>,
    /// Whether the keymap file's entries were taken over.
    loaded: bool,
}

/// Takes the extension entries of the keymap file over once it was read.
fn shortcuts(world: &mut World) -> ExtensionShortcuts {
    let mut shortcuts = world
        .get_resource::<ExtensionShortcuts>()
        .cloned()
        .unwrap_or_default();
    if shortcuts.loaded {
        return shortcuts;
    }
    let Some(kept) = world
        .get_resource::<crate::editor_keymap::KeymapStore>()
        .filter(|store| store.loaded)
        .map(|store| store.unknown.clone())
    else {
        return shortcuts;
    };
    for (id, keys) in kept {
        shortcuts.user.entry(id).or_insert_with(|| KeyChord::parse(&keys));
    }
    shortcuts.loaded = true;
    world.insert_resource(shortcuts.clone());
    shortcuts
}

/// The chord `action` answers to: the user's when set (or none when cleared), else its default.
fn chord_of(shortcuts: &ExtensionShortcuts, action: &ActionDesc) -> Option<KeyChord> {
    match shortcuts.user.get(&action.id) {
        Some(user) => *user,
        None => KeyChord::parse(&action.shortcut),
    }
}

/// What holds `chord` besides extension actions: a built-in command (its user or built-in
/// shortcut) or a fixed editor key.
fn builtin_holder(keymap: &CommandKeymap, chord: KeyChord) -> Option<String> {
    for def in COMMANDS.iter() {
        let keys = keymap.shortcut_of(def);
        if !keys.is_empty() && KeyChord::parse(&keys) == Some(chord) {
            return Some(format!("{}: {} ({})", def.category, def.title, def.id));
        }
    }
    crate::editor_keymap::FIXED_EDITOR_KEYS
        .iter()
        .find(|(keys, _)| KeyChord::parse(keys) == Some(chord))
        .map(|(_, what)| (*what).to_owned())
}

/// An extension shortcut that does not run because something else has the chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutConflict {
    /// The action whose shortcut loses.
    pub id: String,
    /// The chord.
    pub keys: String,
    /// What keeps it: a built-in command, a fixed editor key, or an extension action earlier in
    /// id order.
    pub with: String,
}

/// `(action id, chord)` of every live action that has a shortcut and no conflict, plus the
/// conflicts. Built-in commands and fixed keys win; between extension actions the first id wins.
fn resolve(world: &mut World) -> (Vec<(String, KeyChord)>, Vec<ShortcutConflict>) {
    let shortcuts = shortcuts(world);
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    let mut active: Vec<(String, KeyChord)> = Vec::new();
    let mut conflicts = Vec::new();
    for (_, action) in actions(world) {
        let Some(chord) = chord_of(&shortcuts, &action) else {
            continue;
        };
        let holder = builtin_holder(&keymap, chord).or_else(|| {
            active
                .iter()
                .find(|(_, c)| *c == chord)
                .map(|(id, _)| format!("extension action {id}"))
        });
        match holder {
            Some(with) => conflicts.push(ShortcutConflict {
                id: action.id.clone(),
                keys: chord.label(),
                with,
            }),
            None => active.push((action.id.clone(), chord)),
        }
    }
    (active, conflicts)
}

/// The extension shortcuts that lose their chord to something else.
#[must_use]
pub fn shortcut_conflicts(world: &mut World) -> Vec<ShortcutConflict> {
    resolve(world).1
}

/// The shortcut label of the action `id` now (user or default; empty for none).
#[must_use]
pub fn shortcut_label(world: &mut World, id: &str) -> String {
    let shortcuts = shortcuts(world);
    actions(world)
        .into_iter()
        .find(|(_, a)| a.id == id)
        .and_then(|(_, action)| chord_of(&shortcuts, &action))
        .map(|chord| chord.label())
        .unwrap_or_default()
}

/// Palette rows for the live actions shown for the level selection: category (the extension's
/// display name when empty), title and current shortcut. The palette ranks them with the
/// built-in commands; running one goes through `command_palette::run`.
#[must_use]
pub fn command_defs(world: &World) -> Vec<CommandDef> {
    let selection = MenuSelection::level(world);
    let shortcuts = world
        .get_resource::<ExtensionShortcuts>()
        .cloned()
        .unwrap_or_default();
    actions(world)
        .into_iter()
        .filter(|(_, action)| action_state(world, action, &selection).0)
        .map(|(owner, action)| {
            let category = if action.category.is_empty() {
                display_name(world, &owner)
            } else {
                action.category.clone()
            };
            let shortcut = chord_of(&shortcuts, &action)
                .map(|c| c.label())
                .unwrap_or_default();
            CommandDef {
                id: intern(&action.id),
                category: intern(&category),
                title: intern(&action.title),
                shortcut: intern(&shortcut),
            }
        })
        .collect()
}

/// Runs the extension action whose shortcut was pressed this frame (the caller skips this while
/// a text field has focus, during play and while the palette records a chord). Returns its
/// status line.
pub fn dispatch_shortcuts(ctx: &egui::Context, world: &mut World) -> Option<String> {
    let (active, _) = resolve(world);
    if active.is_empty() {
        return None;
    }
    let hit = active
        .iter()
        .find(|(_, chord)| ctx.input_mut(|i| i.consume_shortcut(&chord.shortcut())))
        .map(|(id, _)| id.clone())?;
    Some(status_line(run_action(world, &hit)))
}

/// Why a shortcut was not bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutError {
    /// No live action has the id.
    NotFound,
    /// The chord does not parse.
    BadChord(String),
    /// Ctrl+Shift+P opens the palette.
    Reserved,
    /// Something else has the chord (what).
    Conflict(String),
}

/// Sets (`Some`) or clears (`None`) the user shortcut of the live action `id`, refusing a chord
/// that a built-in command, a fixed key or another extension action holds, and saves the keymap
/// file when the host gave one. Returns whether the file was written.
///
/// # Errors
///
/// See [`ShortcutError`].
pub fn bind_shortcut(world: &mut World, id: &str, chord: Option<KeyChord>) -> Result<bool, ShortcutError> {
    if !actions(world).iter().any(|(_, a)| a.id == id) {
        return Err(ShortcutError::NotFound);
    }
    if let Some(chord) = chord {
        if chord.command && chord.shift && !chord.alt && chord.key == egui::Key::P {
            return Err(ShortcutError::Reserved);
        }
        let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
        if let Some(with) = builtin_holder(&keymap, chord) {
            return Err(ShortcutError::Conflict(with));
        }
        let shortcuts = shortcuts(world);
        if let Some((_, other)) = actions(world)
            .into_iter()
            .find(|(_, a)| a.id != id && chord_of(&shortcuts, a) == Some(chord))
        {
            return Err(ShortcutError::Conflict(format!("extension action {}", other.id)));
        }
    }
    let mut shortcuts = shortcuts(world);
    shortcuts.user.insert(id.to_owned(), chord);
    world.insert_resource(shortcuts);
    Ok(save_shortcut(world, id, chord))
}

/// Drops the user shortcut of `id` (its default applies again) and saves the keymap file. Returns
/// whether the file was written.
pub fn reset_shortcut(world: &mut World, id: &str) -> bool {
    let mut shortcuts = shortcuts(world);
    shortcuts.user.remove(id);
    world.insert_resource(shortcuts);
    let Some(mut store) = world.get_resource::<crate::editor_keymap::KeymapStore>().cloned() else {
        return false;
    };
    store.unknown.remove(id);
    write_keymap(world, store)
}

/// Writes `id`'s chord into the keymap file's kept entries and saves the file (when the host gave
/// one and a newer editor did not write it).
fn save_shortcut(world: &mut World, id: &str, chord: Option<KeyChord>) -> bool {
    let Some(mut store) = world.get_resource::<crate::editor_keymap::KeymapStore>().cloned() else {
        return false;
    };
    store
        .unknown
        .insert(id.to_owned(), chord.map(|c| c.label()).unwrap_or_default());
    write_keymap(world, store)
}

/// Saves the keymap with `store`'s kept entries, then stores `store`.
fn write_keymap(world: &mut World, mut store: crate::editor_keymap::KeymapStore) -> bool {
    let path = world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(crate::editor_prefs::EditorConfigPaths::keymap_file);
    let written = match (&path, &store.read_only) {
        (Some(path), None) => {
            let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
            let text = crate::editor_keymap::keymap_to_text(&keymap, &store.unknown);
            let result = crate::level_io::write_atomic(path, text.as_bytes());
            store.last_error = result.as_ref().err().cloned();
            result.is_ok()
        }
        _ => false,
    };
    world.insert_resource(store);
    written
}

/// The draft chord text of each row of [`keymap_section`].
#[derive(Resource, Debug, Clone, Default)]
struct ShortcutDrafts {
    text: BTreeMap<String, String>,
    status: String,
}

/// The Keymap tab's section for extension commands: each live action with its chord, a chord
/// field (typed, e.g. `Ctrl+Alt+G`), Set, Clear and Default buttons, and its conflict. Every
/// control is a focusable widget.
pub fn keymap_section(ui: &mut egui::Ui, world: &mut World) {
    let rows = actions(world);
    if rows.is_empty() {
        return;
    }
    let conflicts = shortcut_conflicts(world);
    let shortcuts = shortcuts(world);
    let mut drafts = world
        .get_resource::<ShortcutDrafts>()
        .cloned()
        .unwrap_or_default();
    let mut request: Option<(String, Option<Option<KeyChord>>)> = None;
    ui.separator();
    ui.strong("Extension commands");
    egui::Grid::new("extension_keymap").striped(true).show(ui, |ui| {
        for (owner, action) in &rows {
            let category = if action.category.is_empty() {
                display_name(world, owner)
            } else {
                action.category.clone()
            };
            ui.label(format!("{category}: {}", action.title))
                .on_hover_text(&action.id);
            let current = chord_of(&shortcuts, action)
                .map(|c| c.label())
                .unwrap_or_default();
            let draft = drafts
                .text
                .entry(action.id.clone())
                .or_insert_with(|| current.clone());
            ui.add(
                egui::TextEdit::singleline(draft)
                    .hint_text("Ctrl+Alt+G")
                    .desired_width(120.0),
            )
            .on_hover_text(format!("Shortcut for {}", action.title));
            if ui.button("Set").on_hover_text("Bind the typed chord").clicked() {
                match KeyChord::parse(draft) {
                    Some(chord) => request = Some((action.id.clone(), Some(Some(chord)))),
                    None => drafts.status = format!("`{draft}` is not a chord (e.g. Ctrl+Alt+G)"),
                }
            }
            if ui.button("Clear").on_hover_text("No shortcut").clicked() {
                request = Some((action.id.clone(), Some(None)));
            }
            if ui
                .button("Default")
                .on_hover_text("The extension's own shortcut")
                .clicked()
            {
                request = Some((action.id.clone(), None));
            }
            if let Some(conflict) = conflicts.iter().find(|c| c.id == action.id) {
                ui.colored_label(
                    crate::theme::ui_tokens(ui.ctx()).warn,
                    format!("{} is taken by {}", conflict.keys, conflict.with),
                );
            } else {
                ui.label("");
            }
            ui.end_row();
        }
    });
    if let Some((id, change)) = request {
        drafts.status = match change {
            Some(chord) => match bind_shortcut(world, &id, chord) {
                Ok(_) => format!("{id}: shortcut changed"),
                Err(error) => shortcut_error_text(&id, &error),
            },
            None => {
                reset_shortcut(world, &id);
                format!("{id}: back to its default shortcut")
            }
        };
        let label = shortcut_label(world, &id);
        drafts.text.insert(id, label);
    }
    if !drafts.status.is_empty() {
        ui.label(&drafts.status);
    }
    world.insert_resource(drafts);
}

fn shortcut_error_text(id: &str, error: &ShortcutError) -> String {
    match error {
        ShortcutError::NotFound => format!("no extension action `{id}`"),
        ShortcutError::BadChord(keys) => format!("`{keys}` is not a chord (e.g. Ctrl+Alt+G)"),
        ShortcutError::Reserved => "Ctrl+Shift+P opens the command palette".to_owned(),
        ShortcutError::Conflict(with) => format!("the chord is taken by {with}"),
    }
}

fn dock_name(region: DockRegion) -> &'static str {
    match region {
        DockRegion::Left => "left",
        DockRegion::Right => "right",
        DockRegion::Bottom => "bottom",
        DockRegion::Center => "center",
    }
}

/// Every text run in `shape` (nested shapes included), in paint order.
fn shape_text(shape: &egui::Shape, out: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(text) => {
            let run = text.galley.text().trim();
            if !run.is_empty() {
                out.push(run.to_owned());
            }
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                shape_text(shape, out);
            }
        }
        _ => {}
    }
}

/// Draws the live panel `id` once into an off-screen egui context and returns the text it painted,
/// in paint order (the same callback the panel's dock tab runs).
///
/// # Errors
///
/// `extension_panel_not_found`.
pub fn panel_text(world: &mut World, id: &str) -> Result<(String, PanelDesc, Vec<String>), RpcError> {
    let Some((owner, panel)) = panels(world).into_iter().find(|(_, p)| p.id == id) else {
        return Err(refuse(
            protocol::NOT_FOUND,
            "extension_panel_not_found",
            format!("no extension panel `{id}`"),
            "editor.extension_ui lists the panels of the loaded extensions",
        ));
    };
    let text = offscreen_text(world, &panel.draw);
    Ok((owner, panel, text))
}

/// Runs `draw` once in an off-screen egui context and returns the text it painted, in paint
/// order (panels and overlays).
pub(crate) fn offscreen_text(world: &mut World, draw: &PanelDraw) -> Vec<String> {
    let draw = Arc::clone(draw);
    let context = egui::Context::default();
    // egui may run the closure more than once in a frame; the text comes from the final pass.
    let mut output = context.run_ui(egui::RawInput::default(), |ui| draw(ui, world));
    // Nothing paints this frame: the font atlas upload is dropped on purpose.
    output.textures_delta.clear();
    let mut text = Vec::new();
    for clipped in &output.shapes {
        shape_text(&clipped.shape, &mut text);
    }
    text
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn string_param<'a>(params: &'a Value, method: &str, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("{method}: `{name}` (string) is required")))
}

fn extension_ui(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let panels: Vec<Value> = panels(world)
        .into_iter()
        .map(|(owner, panel)| {
            json!({
                "id": panel.id,
                "title": panel.title_key,
                "dock": dock_name(panel.dock_region),
                "owner": owner,
            })
        })
        .collect();
    let selection = MenuSelection::level(world);
    let rows = actions(world);
    let mut listed = Vec::with_capacity(rows.len());
    for (owner, action) in rows {
        let (visible, enabled) = action_state(world, &action, &selection);
        let category = if action.category.is_empty() {
            display_name(world, &owner)
        } else {
            action.category.clone()
        };
        listed.push(json!({
            "id": action.id,
            "title": action.title,
            "menu": action.menu,
            "toolbar": action.toolbar,
            "owner": owner,
            "category": category,
            "shortcut": shortcut_label(world, &action.id),
            "default_shortcut": action.shortcut,
            "places": action.places.iter().map(|p| p.name()).collect::<Vec<_>>(),
            "visible": visible,
            "enabled": enabled,
            "visible_when": action.visible_when.as_ref().map(SelectionPredicate::to_json),
            "enabled_when": action.enabled_when.as_ref().map(SelectionPredicate::to_json),
        }));
    }
    let actions = listed;
    let conflicts: Vec<Value> = shortcut_conflicts(world)
        .into_iter()
        .map(|c| json!({ "id": c.id, "keys": c.keys, "with": c.with }))
        .collect();
    let hooks: Vec<Value> = super::hooks::all(world)
        .into_iter()
        .map(|(owner, hook)| {
            json!({ "id": hook.id, "title": hook.title, "stage": hook.stage.name(), "owner": owner })
        })
        .collect();
    let tools: Vec<Value> = asset_tools(world)
        .into_iter()
        .map(|(owner, tool)| {
            json!({
                "id": tool.id,
                "title": tool.title,
                "suffixes": tool.suffixes,
                "owner": owner,
            })
        })
        .collect();
    Ok(json!({
        "panels": panels,
        "actions": actions,
        "asset_tools": tools,
        "shortcut_conflicts": conflicts,
        "hooks": hooks,
    }))
}

fn extension_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = string_param(params, "editor.extension_action", "id")?.to_owned();
    let (owner, message) = match params.get("assets") {
        None | Some(Value::Null) => run_action(world, &id)?,
        Some(assets) => {
            let assets: Vec<String> = assets
                .as_array()
                .and_then(|list| {
                    list.iter()
                        .map(|a| a.as_str().map(|a| a.replace('\\', "/")))
                        .collect()
                })
                .ok_or_else(|| {
                    RpcError::invalid_params(
                        "editor.extension_action: `assets` is a list of project-relative paths",
                    )
                })?;
            run_action_on(
                world,
                &id,
                MenuSelection {
                    entities: Vec::new(),
                    assets,
                },
            )?
        }
    };
    Ok(json!({ "id": id, "owner": owner, "message": message }))
}

fn extension_shortcut(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = string_param(params, "editor.extension_shortcut", "id")?.to_owned();
    let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let reset = params.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let saved = if reset {
        if !actions(world).iter().any(|(_, a)| a.id == id) {
            return Err(shortcut_rpc(&id, &ShortcutError::NotFound));
        }
        reset_shortcut(world, &id)
    } else {
        let chord = if clear {
            None
        } else {
            let keys = string_param(params, "editor.extension_shortcut", "keys")?;
            Some(
                KeyChord::parse(keys)
                    .ok_or_else(|| shortcut_rpc(&id, &ShortcutError::BadChord(keys.to_owned())))?,
            )
        };
        bind_shortcut(world, &id, chord).map_err(|error| shortcut_rpc(&id, &error))?
    };
    Ok(json!({
        "id": id,
        "shortcut": shortcut_label(world, &id),
        "saved": saved,
        "conflicts": shortcut_conflicts(world)
            .into_iter()
            .map(|c| json!({ "id": c.id, "keys": c.keys, "with": c.with }))
            .collect::<Vec<_>>(),
    }))
}

fn shortcut_rpc(id: &str, error: &ShortcutError) -> RpcError {
    let (code, reason) = match error {
        ShortcutError::NotFound => (protocol::NOT_FOUND, "extension_action_not_found"),
        ShortcutError::BadChord(_) => (protocol::INVALID_PARAMS, "extension_shortcut_bad_chord"),
        ShortcutError::Reserved => (protocol::INVALID_PARAMS, "extension_shortcut_reserved"),
        ShortcutError::Conflict(_) => (protocol::INVALID_PARAMS, "extension_shortcut_conflict"),
    };
    refuse(
        code,
        reason,
        shortcut_error_text(id, error),
        "editor.extension_ui lists the actions, their shortcuts and conflicts",
    )
}

fn extension_asset_tool(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = string_param(params, "editor.extension_asset_tool", "id")?.to_owned();
    let path = string_param(params, "editor.extension_asset_tool", "path")?.to_owned();
    let (owner, result) = run_asset_tool(world, &id, &path)?;
    Ok(json!({ "id": id, "path": path, "owner": owner, "result": result }))
}

fn extension_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = string_param(params, "editor.extension_panel", "id")?.to_owned();
    let (owner, panel, text) = panel_text(world, &id)?;
    Ok(json!({
        "id": id,
        "title": panel.title_key,
        "dock": dock_name(panel.dock_region),
        "owner": owner,
        "text": text,
    }))
}

/// The agent methods of extension UI, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.extension_ui",
                tool: "editor_extension_ui",
                description: "What loaded extensions add to the editor: their panels (id, title, dock region), menu and toolbar actions (palette category, current and default shortcut, context menus, and whether they show and run for the level selection under their predicates), asset-type tools (with the file suffixes each takes), the shortcuts that lose their chord to something else, and build and launch hooks, each with its owning extension. Only entries of active extensions are listed.",
                params: &[],
                result: "{ panels: [{ id, title, dock, owner }], actions: [{ id, title, menu, toolbar, owner, category, shortcut, default_shortcut, places, visible, enabled, visible_when, enabled_when }], asset_tools: [{ id, title, suffixes, owner }], shortcut_conflicts: [{ id, keys, with }], hooks: [{ id, title, stage, owner }] }",
                mutating: false,
                permission: Permission::Read,
            },
            extension_ui as Handler,
        ),
        (
            MethodSchema {
                name: "editor.extension_action",
                tool: "editor_extension_action",
                description: "Runs an extension's menu or toolbar action by id, exactly as clicking it in the Extensions menu or the toolbar does (on the level selection), or as its Content Browser entry does when `assets` is given, and answers its status line. Errors: extension_action_not_found, extension_action_disabled, extension_action_failed.",
                params: &[
                    ParamSchema {
                        name: "id",
                        kind: "string",
                        required: true,
                        description: "The action id (<extension>.<action>), as editor.extension_ui lists it.",
                    },
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "Project-relative asset paths to run on, as the Content Browser's menu does (default: the level selection).",
                    },
                ],
                result: "{ id, owner, message }",
                mutating: true,
                permission: Permission::EditScene,
            },
            extension_action as Handler,
        ),
        (
            MethodSchema {
                name: "editor.extension_shortcut",
                tool: "editor_extension_shortcut",
                description: "Rebinds, clears or resets an extension action's shortcut, as the Keymap tab's Extension commands rows do; a chord a built-in command, a fixed key or another extension action has is refused. Saved in the keymap file. Errors: extension_action_not_found, extension_shortcut_bad_chord, extension_shortcut_reserved, extension_shortcut_conflict.",
                params: &[
                    ParamSchema {
                        name: "id",
                        kind: "string",
                        required: true,
                        description: "The action id (<extension>.<action>).",
                    },
                    ParamSchema {
                        name: "keys",
                        kind: "string",
                        required: false,
                        description: "The chord, e.g. Ctrl+Alt+G (Ctrl is Cmd on macOS).",
                    },
                    ParamSchema {
                        name: "clear",
                        kind: "boolean",
                        required: false,
                        description: "Remove the shortcut (the default does not apply either).",
                    },
                    ParamSchema {
                        name: "reset",
                        kind: "boolean",
                        required: false,
                        description: "Go back to the extension's default shortcut.",
                    },
                ],
                result: "{ id, shortcut, saved, conflicts: [{ id, keys, with }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            extension_shortcut as Handler,
        ),
        (
            MethodSchema {
                name: "editor.extension_asset_tool",
                tool: "editor_extension_asset_tool",
                description: "Runs an extension's asset-type tool on one project asset and answers the tool's JSON result. Errors: asset_tool_not_found, no_project, bad_path, asset_tool_wrong_type, asset_tool_failed.",
                params: &[
                    ParamSchema {
                        name: "id",
                        kind: "string",
                        required: true,
                        description: "The tool id (<extension>.<tool>), as editor.extension_ui lists it.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path of the asset, with a suffix the tool takes.",
                    },
                ],
                result: "{ id, path, owner, result }",
                mutating: true,
                permission: Permission::EditScene,
            },
            extension_asset_tool as Handler,
        ),
        (
            MethodSchema {
                name: "editor.extension_panel",
                tool: "editor_extension_panel",
                description: "Draws an extension's panel once off screen, with the same callback its dock tab runs, and answers the text it painted in paint order. Errors: extension_panel_not_found.",
                params: &[ParamSchema {
                    name: "id",
                    kind: "string",
                    required: true,
                    description: "The panel id (<extension>.<panel>), as editor.extension_ui lists it.",
                }],
                result: "{ id, title, dock, owner, text: [string] }",
                mutating: false,
                permission: Permission::Read,
            },
            extension_panel as Handler,
        ),
    ]
}
