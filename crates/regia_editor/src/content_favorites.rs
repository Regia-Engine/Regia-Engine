// SPDX-License-Identifier: MPL-2.0
//! Content Browser navigation (`book/agents/content-browser.md`, ADR-7759): the collapsible
//! folder tree, breadcrumbs, back/forward history, favorite folders, folder colors and the
//! browser settings.
//!
//! [`ContentNav`] (an edit-world resource) mirrors the folder the panel shows and keeps the
//! history and the tree's open folders; the agent changes it through `editor.content_view`
//! ([`view_nav`]) and the panel applies the change on its next frame ([`ContentNav::sync`]).
//! Favorites, folder colors and "hide empty folders" live in the user's personal collections file
//! ([`crate::content_collections::ContentLibrary`]).
//!
//! Keyboard: the tree rows are focusable; Up/Down move to the previous or next row, Left closes a
//! folder or moves to its parent, Right opens a folder or moves to its first subfolder, Home/End
//! jump to the first or last row. Alt+Up/Alt+Down move a focused favorite. Alt+Left/Alt+Right
//! (and mouse buttons 4/5) step back and forward while the pointer is over the browser or a tree
//! row has focus ([`history_shortcuts`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use egui::{Color32, Key, Modifiers, PointerButton};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::content::ContentBrowserState;
use crate::content_collections::{CollectionError, CollectionScope, ContentLibrary};
use crate::dnd::ContentDrag;
use crate::viewport_agent::Handler;

/// How many folders the back history keeps.
pub const HISTORY_LIMIT: usize = 64;

/// The theme color tokens a folder can be colored with (`crate::theme::ThemeTokens` fields).
pub const FOLDER_COLORS: [&str; 5] = ["signal", "warn", "error", "strong", "muted"];

/// The color of a folder color token in `tokens` (`None` for an unknown name).
#[must_use]
pub fn folder_color(tokens: &crate::theme::ThemeTokens, name: &str) -> Option<Color32> {
    Some(match name {
        "signal" => tokens.signal,
        "warn" => tokens.warn,
        "error" => tokens.error,
        "strong" => tokens.strong,
        "muted" => tokens.muted,
        _ => return None,
    })
}

/// The tree path of a panel folder (`""` is the `assets` root).
#[must_use]
pub fn tree_path(folder: &str) -> &str {
    if folder.is_empty() { "assets" } else { folder }
}

/// The panel folder of a tree path (`assets` is `""`).
#[must_use]
pub fn panel_folder(path: &str) -> String {
    if path == "assets" {
        String::new()
    } else {
        path.to_string()
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

/// One row of the folder tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// Project-relative folder (`assets` for the root).
    pub path: String,
    /// Nesting depth (0 for `assets`).
    pub depth: usize,
    /// Whether it has subfolders (after hiding empty folders).
    pub has_children: bool,
    /// Whether its subfolders are listed.
    pub expanded: bool,
}

impl TreeRow {
    /// The last path segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The rows of the folder tree: `assets` and every open folder's subfolders, depth first, each
/// level sorted by name. `keep`, when set, lists the folders to show (the others are hidden).
#[must_use]
pub fn tree_rows(
    folders: &[String],
    expanded: &BTreeSet<String>,
    root_open: bool,
    keep: Option<&HashSet<String>>,
) -> Vec<TreeRow> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for folder in folders {
        if keep.is_some_and(|keep| !keep.contains(folder)) {
            continue;
        }
        if let Some(parent) = parent_of(folder) {
            children.entry(parent).or_default().push(folder.as_str());
        }
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| a.rsplit('/').next().cmp(&b.rsplit('/').next()));
    }
    let mut out = Vec::new();
    let mut stack = vec![("assets", 0usize)];
    while let Some((path, depth)) = stack.pop() {
        let kids = children.get(path);
        let has_children = kids.is_some_and(|kids| !kids.is_empty());
        let open = if path == "assets" {
            root_open
        } else {
            expanded.contains(path)
        };
        out.push(TreeRow {
            path: path.to_string(),
            depth,
            has_children,
            expanded: open && has_children,
        });
        if open && let Some(kids) = kids {
            for kid in kids.iter().rev() {
                stack.push((kid, depth + 1));
            }
        }
    }
    out
}

/// The folders that hold an asset somewhere under them (for "hide empty folders").
#[must_use]
pub fn folders_with_assets<'a>(assets: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    let mut out = HashSet::new();
    for rel in assets {
        let mut parent = parent_of(rel);
        while let Some(folder) = parent {
            if folder == "assets" || !out.insert(folder.to_string()) {
                break;
            }
            parent = parent_of(folder);
        }
    }
    out
}

/// The breadcrumbs of a panel folder: `assets`, then each folder down to it.
#[must_use]
pub fn breadcrumbs(folder: &str) -> Vec<String> {
    let path = tree_path(folder);
    let mut out: Vec<String> = path
        .char_indices()
        .filter(|(_, c)| *c == '/')
        .map(|(i, _)| path[..i].to_string())
        .collect();
    out.push(path.to_string());
    out
}

/// The subfolders of `path` (a crumb's dropdown: the next crumb's siblings), sorted by name.
#[must_use]
pub fn subfolders(folders: &[String], path: &str) -> Vec<String> {
    let mut out: Vec<String> = folders
        .iter()
        .filter(|folder| parent_of(folder) == Some(path))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.rsplit('/').next().cmp(&b.rsplit('/').next()));
    out
}

/// A key the folder tree acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKey {
    /// The previous row.
    Up,
    /// The next row.
    Down,
    /// Close the folder, or go to its parent.
    Left,
    /// Open the folder, or go to its first subfolder.
    Right,
    /// The first row.
    Home,
    /// The last row.
    End,
}

type TreeCacheKey = (u64, usize, u64, bool);

/// The Content Browser's navigation state (an edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct ContentNav {
    /// The folder the panel shows (`""` = `assets/`).
    pub folder: String,
    back: Vec<String>,
    forward: Vec<String>,
    pending: Option<String>,
    /// Open folders of the tree (project-relative).
    pub expanded: BTreeSet<String>,
    /// The `assets` root row is closed.
    pub root_collapsed: bool,
    /// The collection the panel lists instead of a folder.
    pub collection: Option<String>,
    /// Bumped by every tree change (keys the cached rows).
    pub revision: u64,
    synced: bool,
    tree_cache: Option<(TreeCacheKey, Arc<[TreeRow]>)>,
    /// The tree row to scroll to and focus next frame (keyboard moves in the tree only).
    focus_row: Option<String>,
    /// The tree row to scroll into view next frame without taking the keyboard focus (any other
    /// navigation: an agent's folder, a reveal, a crumb or favorite click; review row 14).
    scroll_row: Option<String>,
    /// The egui frame whose keyboard or drop action already ran (egui may run a panel twice).
    acted_frame: Option<u64>,
    /// The collection the Delete key or the context menu asked to delete, waiting for the
    /// confirmation (review row 18), and whether its Delete button takes the focus next frame.
    confirm_delete: Option<(String, bool)>,
    /// The folders "Hide empty folders" keeps, for one index generation (review row 17).
    keep_cache: Option<(u64, Arc<HashSet<String>>)>,
    /// Keep sets built so far (once per index generation, never on an idle frame).
    pub keep_builds: u64,
}

impl ContentNav {
    /// Shows `folder` (`""` or `assets` = the root): the current folder goes on the back history,
    /// the forward history is dropped, the tree opens down to it. Returns whether it changed.
    pub fn navigate(&mut self, folder: &str) -> bool {
        let folder = panel_folder(folder.trim().trim_end_matches('/'));
        self.collection = None;
        if folder == self.folder {
            return false;
        }
        let previous = std::mem::replace(&mut self.folder, folder.clone());
        self.back.push(previous);
        if self.back.len() > HISTORY_LIMIT {
            self.back.remove(0);
        }
        self.forward.clear();
        self.pending = Some(folder);
        self.open_to_current();
        true
    }

    /// Steps back; `false` when there is nothing to go back to.
    pub fn back(&mut self) -> bool {
        let Some(folder) = self.back.pop() else {
            return false;
        };
        self.forward
            .push(std::mem::replace(&mut self.folder, folder.clone()));
        self.pending = Some(folder);
        self.collection = None;
        self.open_to_current();
        true
    }

    /// Steps forward; `false` when there is nothing to go forward to.
    pub fn forward(&mut self) -> bool {
        let Some(folder) = self.forward.pop() else {
            return false;
        };
        self.back
            .push(std::mem::replace(&mut self.folder, folder.clone()));
        self.pending = Some(folder);
        self.collection = None;
        self.open_to_current();
        true
    }

    /// Whether [`Self::back`] would move.
    #[must_use]
    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether [`Self::forward`] would move.
    #[must_use]
    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// The back history, oldest first.
    #[must_use]
    pub fn back_history(&self) -> &[String] {
        &self.back
    }

    /// Lines the panel state up with the navigation: a folder the agent or a shortcut asked for
    /// is shown; a folder something else picked (an audit panel's reveal) joins the history.
    pub fn sync(&mut self, state: &mut ContentBrowserState) {
        if let Some(folder) = self.pending.take() {
            state.folder = folder;
        } else if self.synced && state.folder != self.folder {
            let folder = state.folder.clone();
            self.navigate(&folder);
            self.pending = None;
        }
        self.folder = state.folder.clone();
        self.synced = true;
    }

    fn open_to_current(&mut self) {
        let path = tree_path(&self.folder).to_string();
        let mut parent = parent_of(&path);
        while let Some(folder) = parent {
            if folder == "assets" {
                break;
            }
            self.expanded.insert(folder.to_string());
            parent = parent_of(folder);
        }
        self.root_collapsed = false;
        self.scroll_row = Some(path);
        self.revision += 1;
    }

    /// Opens or closes a tree folder (`assets` is the root).
    pub fn set_expanded(&mut self, path: &str, open: bool) {
        if path == "assets" {
            self.root_collapsed = !open;
        } else if open {
            self.expanded.insert(path.to_string());
        } else {
            self.expanded.remove(path);
        }
        self.revision += 1;
    }

    /// The tree rows for `folders`, cached until the folders, the open set or the filter change.
    pub fn rows(
        &mut self,
        folders: &[String],
        generation: u64,
        keep: Option<&HashSet<String>>,
    ) -> Arc<[TreeRow]> {
        let key = (generation, folders.len(), self.revision, keep.is_some());
        if let Some((cached, rows)) = &self.tree_cache
            && *cached == key
        {
            return Arc::clone(rows);
        }
        let rows: Arc<[TreeRow]> = tree_rows(folders, &self.expanded, !self.root_collapsed, keep).into();
        self.tree_cache = Some((key, Arc::clone(&rows)));
        rows
    }

    /// Acts on a tree key; returns whether the folder or the tree changed.
    pub fn tree_key(&mut self, rows: &[TreeRow], key: TreeKey) -> bool {
        if rows.is_empty() {
            return false;
        }
        let current = tree_path(&self.folder);
        let Some(at) = rows.iter().position(|row| row.path == current) else {
            return self.navigate(&rows[0].path);
        };
        let row = &rows[at];
        let target = match key {
            TreeKey::Up => at.checked_sub(1),
            TreeKey::Down => (at + 1 < rows.len()).then_some(at + 1),
            TreeKey::Home => Some(0),
            TreeKey::End => Some(rows.len() - 1),
            TreeKey::Left => {
                if row.expanded {
                    let path = row.path.clone();
                    self.set_expanded(&path, false);
                    self.focus_row = Some(path);
                    return true;
                }
                parent_of(&row.path).and_then(|parent| rows.iter().position(|r| r.path == parent))
            }
            TreeKey::Right => {
                if row.has_children && !row.expanded {
                    let path = row.path.clone();
                    self.set_expanded(&path, true);
                    self.focus_row = Some(path);
                    return true;
                }
                (row.expanded && at + 1 < rows.len()).then_some(at + 1)
            }
        };
        match target {
            Some(target) if target != at => {
                let path = rows[target].path.clone();
                self.navigate(&path)
            }
            _ => false,
        }
    }

    /// The folders to keep in the tree while `hide` (Hide empty folders) is on: those with an asset
    /// anywhere under them, built once per index generation.
    pub fn keep(
        &mut self,
        hide: bool,
        index: Option<&crate::content_index::ContentIndex>,
    ) -> Option<Arc<HashSet<String>>> {
        if !hide {
            return None;
        }
        let generation = index.map_or(0, |index| index.generation);
        if let Some((at, keep)) = &self.keep_cache
            && *at == generation
        {
            return Some(Arc::clone(keep));
        }
        let assets = index.map_or_else(
            || Arc::from(Vec::new()),
            crate::content_index::ContentIndex::assets,
        );
        let keep = Arc::new(folders_with_assets(assets.iter().map(|(rel, _)| rel.as_str())));
        self.keep_cache = Some((generation, Arc::clone(&keep)));
        self.keep_builds += 1;
        Some(keep)
    }

    /// Whether this egui frame already ran a keyboard or drop action (and marks it as run).
    fn first_action_this_frame(&mut self, frame: u64) -> bool {
        if self.acted_frame == Some(frame) {
            return false;
        }
        self.acted_frame = Some(frame);
        true
    }
}

/// Alt+Left/Alt+Right step the history while `active` (the pointer is over the browser and no
/// other panel's widget has the keyboard focus, or one of the browser's rows has it) and no text
/// field has the focus (Alt+Left moves its cursor by a word); mouse buttons 4/5 step it while the
/// pointer is over `ui` (review row 15). Kept in one function so the scoped-shortcut system can
/// take it over (scope: content). Returns whether it moved.
pub fn history_shortcuts(ui: &egui::Ui, nav: &mut ContentNav, active: bool) -> bool {
    let keys = active && !ui.ctx().text_edit_focused();
    let mouse = ui.ui_contains_pointer();
    if !(keys || mouse) {
        return false;
    }
    let (back, forward) = ui.input_mut(|input| {
        let back = (keys && consume_exact(input, Modifiers::ALT, Key::ArrowLeft))
            || (mouse && input.pointer.button_pressed(PointerButton::Extra1));
        let forward = (keys && consume_exact(input, Modifiers::ALT, Key::ArrowRight))
            || (mouse && input.pointer.button_pressed(PointerButton::Extra2));
        (back, forward)
    });
    if !(back || forward) || !nav.first_action_this_frame(ui.ctx().cumulative_frame_nr()) {
        return false;
    }
    if back { nav.back() } else { nav.forward() }
}

/// Consumes one press of `key` with exactly `modifiers` (egui's `consume_key` also matches the
/// key with Shift or Alt held on top).
fn consume_exact(input: &mut egui::InputState, modifiers: Modifiers, key: Key) -> bool {
    let mut hit = false;
    input.events.retain(|event| {
        let matches = !hit
            && matches!(event, egui::Event::Key { key: k, pressed: true, modifiers: m, .. }
                if *k == key && *m == modifiers);
        hit |= matches;
        !matches
    });
    hit
}

/// The favorites of the open project (personal file), and whether `folder` is one.
#[must_use]
pub fn is_favorite(library: &ContentLibrary, folder: &str) -> bool {
    let path = tree_path(folder);
    library.personal.favorites.iter().any(|f| f == path)
}

/// Adds or removes a favorite folder; returns the toast line.
pub fn toggle_favorite(world: &mut World, folder: &str) -> String {
    crate::content_collections::library(world);
    let path = tree_path(folder).to_string();
    let mut library = world.get_resource_or_insert_with(ContentLibrary::default);
    let result = library.edit(CollectionScope::Personal, |file| {
        let favorites = &mut file.favorites;
        Ok(match favorites.iter().position(|f| *f == path) {
            Some(at) => {
                favorites.remove(at);
                format!("Removed {path} from favorites")
            }
            None => {
                favorites.push(path.clone());
                format!("Added {path} to favorites")
            }
        })
    });
    result.unwrap_or_else(|e| e.message)
}

/// Moves the favorite at `from` to `to` (indices into the list); returns whether it moved.
pub fn move_favorite(world: &mut World, from: usize, to: usize) -> bool {
    let mut library = world.get_resource_or_insert_with(ContentLibrary::default);
    let result = library.edit(CollectionScope::Personal, |file| {
        let favorites = &mut file.favorites;
        if from >= favorites.len() || to >= favorites.len() || from == to {
            return Ok(false);
        }
        let item = favorites.remove(from);
        favorites.insert(to, item);
        Ok(true)
    });
    result.unwrap_or_else(|e| {
        tracing::warn!(target: "regia_editor", "favorites: {}", e.message);
        false
    })
}

/// A favorite row being dragged (its index), to reorder the list.
#[derive(Debug, Clone, Copy)]
struct FavoriteDrag(usize);

/// What the folder pane asked for this frame, applied after it draws.
enum PaneAction {
    Navigate(String),
    ShowCollection(String),
    MoveAsset {
        from: String,
        folder: String,
    },
    AddToCollection {
        name: String,
        path: String,
    },
    MoveFavorite {
        from: usize,
        to: usize,
    },
    DeleteCollection(String),
    CreateCollection {
        name: String,
        scope: CollectionScope,
        query: Option<String>,
    },
}

/// The new-collection form under the Collections section (an edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct NewCollectionForm {
    /// The name being typed.
    pub name: String,
    /// Shared instead of personal.
    pub shared: bool,
    /// Save the current search as a dynamic collection.
    pub from_search: bool,
}

/// Draws the folder pane: Favorites, Collections and the folder tree (the panel's left side).
/// Returns the folder row under the pointer ("" for `assets`), the target of an OS file drop.
pub fn folder_pane(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ContentBrowserState,
    folders: &Arc<[String]>,
) -> Option<String> {
    crate::content_collections::library(world);
    // The folder row under the pointer, for an OS file drop on the tree ("" is `assets`; 3222).
    let mut hovered_folder: Option<String> = None;
    let mut nav = world.remove_resource::<ContentNav>().unwrap_or_default();
    // Borrowed for the frame (put back before the actions run), never cloned (review row 17).
    let library = world.remove_resource::<ContentLibrary>().unwrap_or_default();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let frame = ui.ctx().cumulative_frame_nr();
    let mut actions: Vec<PaneAction> = Vec::new();
    let mut any_focus = false;

    // Favorites: click to open, drag to reorder, Alt+Up/Alt+Down on a focused row.
    egui::CollapsingHeader::new("Favorites")
        .id_salt("content-favorites")
        .default_open(true)
        .show(ui, |ui| {
            if library.personal.favorites.is_empty() {
                // One line in the narrow folder column, cut with "…" and whole on hover, so a
                // large text scale does not stack it four lines deep (ADR-8679).
                ui.add(
                    egui::Label::new(egui::RichText::new("Star a folder to add it here").weak()).truncate(),
                );
            }
            let count = library.personal.favorites.len();
            for (at, path) in library.personal.favorites.iter().enumerate() {
                let label = path.strip_prefix("assets/").unwrap_or(path);
                let chosen = nav.collection.is_none() && tree_path(&nav.folder) == path;
                let response = ui
                    .selectable_label(chosen, format!("\u{2605} {label}"))
                    .interact(egui::Sense::drag());
                if response.clicked() {
                    actions.push(PaneAction::Navigate(path.clone()));
                }
                response.dnd_set_drag_payload(FavoriteDrag(at));
                if let Some(dragged) = response.dnd_release_payload::<FavoriteDrag>() {
                    actions.push(PaneAction::MoveFavorite {
                        from: dragged.0,
                        to: at,
                    });
                }
                if response.has_focus() {
                    any_focus = true;
                    let (up, down) = ui.input_mut(|i| {
                        (
                            i.consume_key(Modifiers::ALT, Key::ArrowUp),
                            i.consume_key(Modifiers::ALT, Key::ArrowDown),
                        )
                    });
                    if up && at > 0 {
                        actions.push(PaneAction::MoveFavorite { from: at, to: at - 1 });
                    } else if down && at + 1 < count {
                        actions.push(PaneAction::MoveFavorite { from: at, to: at + 1 });
                    }
                }
            }
        });

    // Collections: click to list, drop an asset on a static one to add it, Delete on the focused
    // collection asks to delete it (its assets stay).
    egui::CollapsingHeader::new("Collections")
        .id_salt("content-collections")
        .default_open(true)
        .show(ui, |ui| {
            for error in &library.errors {
                ui.colored_label(tokens.warn, error);
            }
            for (scope, collection) in library.all() {
                let glyph = if collection.is_dynamic() {
                    "\u{1f50d}"
                } else {
                    "\u{25a4}"
                };
                let chosen = nav.collection.as_deref() == Some(collection.name.as_str());
                let response = ui
                    .selectable_label(chosen, format!("{glyph} {}", collection.name))
                    .on_hover_text(match (&collection.query, scope) {
                        (Some(query), _) => format!("Saved search: {query} ({})", scope.name()),
                        (None, _) => format!("{} asset(s), {}", collection.assets.len(), scope.name()),
                    });
                if response.clicked() {
                    actions.push(PaneAction::ShowCollection(collection.name.clone()));
                }
                if !collection.is_dynamic()
                    && let Some(drag) = response.dnd_release_payload::<ContentDrag>()
                {
                    actions.push(PaneAction::AddToCollection {
                        name: collection.name.clone(),
                        path: drag.path.clone(),
                    });
                }
                if response.has_focus() {
                    any_focus = true;
                    if ui.input_mut(|i| consume_exact(i, Modifiers::NONE, Key::Delete)) {
                        nav.confirm_delete = Some((collection.name.clone(), true));
                    }
                }
                response.context_menu(|ui| {
                    if ui.button("Delete collection").clicked() {
                        nav.confirm_delete = Some((collection.name.clone(), true));
                        ui.close();
                    }
                });
                if let Some((name, focus)) = nav.confirm_delete.clone()
                    && name == collection.name
                {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(match scope {
                            CollectionScope::Shared => {
                                format!("Delete {name} for everyone on the project? Its assets stay.")
                            }
                            CollectionScope::Personal => format!("Delete {name}? Its assets stay."),
                        });
                        ui.horizontal(|ui| {
                            let yes = ui.button("Delete");
                            let yes = crate::a11y_audit::name(
                                yes,
                                egui::WidgetType::Button,
                                &format!("Delete the collection {name}"),
                            );
                            if focus {
                                yes.request_focus();
                                nav.confirm_delete = Some((name.clone(), false));
                            }
                            if yes.clicked() {
                                actions.push(PaneAction::DeleteCollection(name.clone()));
                                nav.confirm_delete = None;
                            }
                            if ui.button("Cancel").clicked() {
                                nav.confirm_delete = None;
                            }
                        });
                    });
                }
            }
            let mut form = world.remove_resource::<NewCollectionForm>().unwrap_or_default();
            // "Save the search" saves what the panel lists: the typed text plus the Filter chips
            // (review row 31).
            let search = world
                .get_resource::<crate::content_index::ContentFilter>()
                .map_or_else(
                    || state.search.clone(),
                    |filter| filter.search_text(&state.search),
                );
            ui.horizontal(|ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut form.name)
                        .hint_text("New collection")
                        .desired_width(90.0),
                );
                let field =
                    crate::a11y_audit::spoken_only(field, egui::WidgetType::TextEdit, "New collection name");
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (crate::a11y_audit::glyph_button(ui, "+", "Create the collection").clicked() || enter)
                    && !form.name.trim().is_empty()
                {
                    let query =
                        (form.from_search && !search.trim().is_empty()).then(|| search.trim().to_string());
                    actions.push(PaneAction::CreateCollection {
                        name: std::mem::take(&mut form.name),
                        scope: if form.shared {
                            CollectionScope::Shared
                        } else {
                            CollectionScope::Personal
                        },
                        query,
                    });
                }
            });
            ui.checkbox(&mut form.shared, "Shared with the project");
            ui.add_enabled_ui(!search.trim().is_empty(), |ui| {
                ui.checkbox(&mut form.from_search, "Save the search");
            });
            world.insert_resource(form);
        });

    // The folder tree: only the rows on screen are laid out.
    let generation = world
        .get_resource::<crate::content_index::ContentIndex>()
        .map_or(0, |index| index.generation);
    let keep = nav.keep(
        library.personal.hide_empty_folders,
        world.get_resource::<crate::content_index::ContentIndex>(),
    );
    let rows = nav.rows(folders, generation, keep.as_deref());
    ui.separator();
    let row_height = ui.spacing().interact_size.y;
    let indent = ui.spacing().indent;
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("content-tree")
        .auto_shrink([false; 2]);
    let focus_target = nav.focus_row.take();
    let scroll_target = nav.scroll_row.take();
    if let Some(target) = focus_target.as_ref().or(scroll_target.as_ref())
        && let Some(at) = rows.iter().position(|row| &row.path == target)
    {
        // Keep the keyboard's row in view (the rows outside it are not laid out).
        let top = at as f32 * (row_height + ui.spacing().item_spacing.y);
        scroll = scroll.vertical_scroll_offset((top - 4.0 * row_height).max(0.0));
    }
    let mut key_action: Option<TreeKey> = None;
    scroll.show_rows(ui, row_height, rows.len(), |ui, range| {
        for row in rows.get(range).unwrap_or_default() {
            ui.horizontal(|ui| {
                ui.add_space(indent * row.depth as f32);
                if row.has_children {
                    let (glyph, verb) = if row.expanded {
                        ("\u{25be}", "Collapse")
                    } else {
                        ("\u{25b8}", "Expand")
                    };
                    let toggle = crate::a11y_audit::glyph_button(ui, glyph, &format!("{verb} {}", row.path));
                    if toggle.clicked() {
                        let path = row.path.clone();
                        nav.set_expanded(&path, !row.expanded);
                    }
                } else {
                    ui.add_space(ui.spacing().interact_size.y);
                }
                let chosen = nav.collection.is_none() && tree_path(&nav.folder) == row.path;
                let mut text = egui::RichText::new(row.name());
                if let Some(color) = library
                    .folder_color(&row.path)
                    .and_then(|name| folder_color(&tokens, name))
                {
                    text = text.color(color);
                }
                let response = ui.add(egui::Button::selectable(chosen, text));
                let response = crate::a11y_audit::spoken_only(
                    response,
                    egui::WidgetType::SelectableLabel,
                    &format!("Folder {}", row.path),
                );
                if focus_target.as_deref() == Some(row.path.as_str()) && !response.has_focus() {
                    response.request_focus();
                }
                if response.contains_pointer() {
                    hovered_folder = Some(if row.path == "assets" {
                        String::new()
                    } else {
                        row.path.clone()
                    });
                }
                if response.clicked() {
                    actions.push(PaneAction::Navigate(row.path.clone()));
                }
                // A folder's menu: Validate, Reference graph, Size map, Audit (ADR-7725; kept from
                // the flat list this tree replaced, integ-v06j).
                if row.path != "assets" {
                    crate::content::folder_context_menu(&response, state, &row.path);
                }
                if response.double_clicked() && row.has_children {
                    let path = row.path.clone();
                    nav.set_expanded(&path, !row.expanded);
                }
                if let Some(drag) = response.dnd_release_payload::<ContentDrag>() {
                    actions.push(PaneAction::MoveAsset {
                        from: drag.path.clone(),
                        folder: row.path.clone(),
                    });
                }
                if response.has_focus() {
                    any_focus = true;
                    // Arrow keys move inside the tree instead of between widgets.
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            response.id,
                            egui::EventFilter {
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                ..Default::default()
                            },
                        );
                    });
                    // Alt+Left/Alt+Right first: the plain arrows below would also take them.
                    history_shortcuts(ui, &mut nav, true);
                    key_action = ui
                        .input_mut(|i| {
                            [
                                (Key::ArrowUp, TreeKey::Up),
                                (Key::ArrowDown, TreeKey::Down),
                                (Key::ArrowLeft, TreeKey::Left),
                                (Key::ArrowRight, TreeKey::Right),
                                (Key::Home, TreeKey::Home),
                                (Key::End, TreeKey::End),
                            ]
                            .into_iter()
                            .find(|(key, _)| consume_exact(i, Modifiers::NONE, *key))
                            .map(|(_, action)| action)
                        })
                        .or(key_action);
                }
            });
        }
    });
    if let Some(key) = key_action
        && nav.first_action_this_frame(frame)
    {
        nav.tree_key(&rows, key);
        if nav.focus_row.is_none() {
            nav.focus_row = Some(tree_path(&nav.folder).to_string());
        }
    }
    if any_focus {
        history_shortcuts(ui, &mut nav, true);
    }
    let drop_ok = actions.iter().any(|a| {
        matches!(
            a,
            PaneAction::MoveAsset { .. }
                | PaneAction::AddToCollection { .. }
                | PaneAction::MoveFavorite { .. }
        )
    });
    let run_drops = !drop_ok || nav.first_action_this_frame(frame);
    world.insert_resource(library);
    for action in actions {
        match action {
            PaneAction::Navigate(path) => {
                nav.navigate(&path);
            }
            PaneAction::ShowCollection(name) => nav.collection = Some(name),
            PaneAction::MoveAsset { from, folder } if run_drops => {
                state.toast = Some(move_into_folder(world, state, &from, &folder));
            }
            PaneAction::AddToCollection { name, path } if run_drops => {
                state.toast = Some(crate::content_collections::add_paths(world, &name, &[path]));
            }
            PaneAction::MoveFavorite { from, to } if run_drops => {
                move_favorite(world, from, to);
            }
            PaneAction::DeleteCollection(name) => {
                let result = world.resource_mut::<ContentLibrary>().delete(&name);
                if nav.collection.as_deref() == Some(name.as_str()) {
                    nav.collection = None;
                }
                state.toast = Some(match result {
                    Ok(_) => format!("Deleted the collection {name} (its assets stay)"),
                    Err(e) => e.message,
                });
            }
            PaneAction::CreateCollection { name, scope, query } => {
                let result = world
                    .resource_mut::<ContentLibrary>()
                    .create(scope, &name, query.as_deref());
                state.toast = Some(match result {
                    Ok(()) => format!("Created the collection {name}"),
                    Err(e) => e.message,
                });
            }
            _ => {}
        }
    }
    // The panel shows the folder the pane picked from this frame on.
    if let Some(folder) = nav.pending.take() {
        state.folder = folder;
    }
    world.insert_resource(nav);
    hovered_folder
}

/// Moves `from` (project-relative) into `folder` with the same move as Rename / move (the `.meta`
/// moves too, path references are rewritten, one undo entry). Returns the toast line.
pub fn move_into_folder(world: &mut World, state: &ContentBrowserState, from: &str, folder: &str) -> String {
    let Some(root) = state.project_root.clone() else {
        return "no project open".to_string();
    };
    let name = from.rsplit('/').next().unwrap_or(from);
    let target = format!("{folder}/{name}");
    if target == from || parent_of(from) == Some(folder) {
        return format!("{from} is already in {folder}");
    }
    match regia_asset::project_ops::move_asset(&root, from, &target) {
        Ok(outcome) => {
            crate::content::push_move_undo(world, root, from.to_string(), target.clone());
            crate::content_index::invalidate(world);
            format!(
                "Moved {from} to {target} ({} reference(s) updated)",
                outcome.fixed_referencers.len()
            )
        }
        Err(e) => format!("Move failed: {e}"),
    }
}

/// Draws the breadcrumb bar: Back, Forward, the crumbs (each with a dropdown of its subfolders)
/// and the favorite star. Returns whether a control of the bar has focus.
pub fn breadcrumb_bar(
    ui: &mut egui::Ui,
    world: &mut World,
    state: &mut ContentBrowserState,
    folders: &[String],
) -> bool {
    let mut nav = world.remove_resource::<ContentNav>().unwrap_or_default();
    let mut focus = false;
    let mut toggle_star = false;
    ui.horizontal(|ui| {
        let back = ui.add_enabled_ui(nav.can_back(), |ui| {
            crate::a11y_audit::glyph_button(ui, "\u{2190}", "Back (Alt+Left)")
        });
        if back.inner.clicked() {
            nav.back();
        }
        let forward = ui.add_enabled_ui(nav.can_forward(), |ui| {
            crate::a11y_audit::glyph_button(ui, "\u{2192}", "Forward (Alt+Right)")
        });
        if forward.inner.clicked() {
            nav.forward();
        }
        focus |= back.inner.has_focus() || forward.inner.has_focus();
        if let Some(name) = nav.collection.clone() {
            ui.strong(format!("Collection: {name}"));
            if ui.button("Back to folders").clicked() {
                nav.collection = None;
            }
        } else {
            let crumbs = breadcrumbs(&nav.folder);
            let mut go: Option<String> = None;
            for crumb in &crumbs {
                let name = crumb.rsplit('/').next().unwrap_or(crumb);
                let button = ui.button(name);
                focus |= button.has_focus();
                if button.clicked() {
                    go = Some(crumb.clone());
                }
                let inside = subfolders(folders, crumb);
                if !inside.is_empty() {
                    let menu = ui.menu_button("\u{203a}", |ui| {
                        for folder in &inside {
                            let label = folder.rsplit('/').next().unwrap_or(folder);
                            if ui.button(label).clicked() {
                                go = Some(folder.clone());
                                ui.close();
                            }
                        }
                    });
                    let menu = crate::a11y_audit::name(
                        menu.response,
                        egui::WidgetType::Button,
                        &format!("Folders in {crumb}"),
                    );
                    focus |= menu.has_focus();
                }
            }
            if let Some(folder) = go {
                nav.navigate(&folder);
            }
            let library = world.get_resource::<ContentLibrary>();
            let starred = library.is_some_and(|library| is_favorite(library, &nav.folder));
            let (glyph, spoken) = if starred {
                ("\u{2605}", "Remove the folder from favorites")
            } else {
                ("\u{2606}", "Add the folder to favorites")
            };
            let star = crate::a11y_audit::glyph_button(ui, glyph, spoken);
            focus |= star.has_focus();
            if star.clicked() {
                toggle_star = true;
            }
        }
    });
    if let Some(folder) = nav.pending.take() {
        state.folder = folder;
    }
    let folder = nav.folder.clone();
    world.insert_resource(nav);
    if toggle_star {
        state.toast = Some(toggle_favorite(world, &folder));
    }
    focus
}

/// The browser settings menu (toolbar): hide empty folders, and the color of the current folder.
pub fn settings_menu(ui: &mut egui::Ui, world: &mut World, state: &ContentBrowserState) {
    crate::content_collections::library(world);
    let folder = tree_path(&state.folder).to_string();
    let response = ui.menu_button("View", |ui| {
        let mut library = world.resource_mut::<ContentLibrary>();
        let mut hide = library.personal.hide_empty_folders;
        if ui.checkbox(&mut hide, "Hide empty folders").changed()
            && let Err(e) = library.edit(CollectionScope::Personal, |file| {
                file.hide_empty_folders = hide;
                Ok(())
            })
        {
            tracing::warn!(target: "regia_editor", "browser settings: {}", e.message);
        }
        ui.menu_button(format!("Color of {folder}"), |ui| {
            let current = library.personal.folder_colors.get(&folder).cloned();
            let mut pick: Option<Option<String>> = None;
            if ui.radio(current.is_none(), "None").clicked() {
                pick = Some(None);
            }
            for name in FOLDER_COLORS {
                if ui.radio(current.as_deref() == Some(name), name).clicked() {
                    pick = Some(Some(name.to_string()));
                }
            }
            if let Some(pick) = pick {
                let result = library.edit(CollectionScope::Personal, |file| {
                    match pick {
                        Some(name) => file.folder_colors.insert(folder.clone(), name),
                        None => file.folder_colors.remove(&folder),
                    };
                    Ok(())
                });
                if let Err(e) = result {
                    tracing::warn!(target: "regia_editor", "folder color: {}", e.message);
                }
                ui.close();
            }
        });
    });
    let _ = response
        .response
        .on_hover_text("Browser settings: empty folders, folder colors");
}

// ---------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------

fn coded(mut error: RpcError, field: &str, code: &str) -> RpcError {
    match error.data.as_mut() {
        Some(data) => data[field] = json!(code),
        None => error.data = Some(json!({ field: code })),
    }
    error
}

fn bad_view(message: String) -> RpcError {
    coded(RpcError::invalid_params(message), "view_error", "view_bad_params")
}

/// The navigation part of `editor.content_view`: applies `folder`, `back`, `forward`, `expand`,
/// `collapse` and `collection`, and answers the folder, breadcrumbs, history and tree.
///
/// # Errors
///
/// `view_bad_params`, `view_bad_folder` (not a folder of the project), `view_no_project`,
/// `view_bad_collection`.
pub fn view_nav(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::content_collections::library(world);
    let folders = match &root {
        Some(root) => {
            let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
            index.ensure(Some(root));
            index.folders()
        }
        None => Arc::from(Vec::new()),
    };
    let known = |path: &str| path == "assets" || folders.binary_search_by(|f| f.as_str().cmp(path)).is_ok();
    let wants_project = ["folder", "expand", "collapse", "collection"]
        .iter()
        .any(|key| params.get(*key).is_some());
    if wants_project && root.is_none() {
        return Err(coded(
            RpcError::not_found("no project is open", "start the editor with --project <dir>"),
            "view_error",
            "view_no_project",
        ));
    }
    let mut nav = world.remove_resource::<ContentNav>().unwrap_or_default();
    let result = (|| {
        let list = |key: &str| -> Result<Vec<String>, RpcError> {
            match params.get(key) {
                None => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(|s| tree_path(s.trim().trim_end_matches('/')).replace('\\', "/"))
                            .ok_or_else(|| bad_view(format!("`{key}` must be an array of folder paths")))
                    })
                    .collect(),
                Some(_) => Err(bad_view(format!("`{key}` must be an array of folder paths"))),
            }
        };
        for (key, open) in [("expand", true), ("collapse", false)] {
            for path in list(key)? {
                if !known(&path) {
                    return Err(coded(
                        RpcError::invalid_params(format!("{path} is not a folder of the project")),
                        "view_error",
                        "view_bad_folder",
                    ));
                }
                nav.set_expanded(&path, open);
            }
        }
        if let Some(folder) = params.get("folder") {
            let folder = folder
                .as_str()
                .ok_or_else(|| bad_view("`folder` must be a string".to_string()))?
                .replace('\\', "/");
            let path = tree_path(folder.trim().trim_end_matches('/')).to_string();
            if !known(&path) {
                return Err(coded(
                    RpcError::invalid_params(format!("{path} is not a folder of the project")),
                    "view_error",
                    "view_bad_folder",
                ));
            }
            nav.navigate(&path);
        }
        for (key, step) in [("back", true), ("forward", false)] {
            match params.get(key) {
                None | Some(Value::Bool(false)) => {}
                Some(Value::Bool(true)) => {
                    if step {
                        nav.back();
                    } else {
                        nav.forward();
                    }
                }
                Some(_) => return Err(bad_view(format!("`{key}` must be true or false"))),
            }
        }
        if let Some(collection) = params.get("collection") {
            match collection {
                Value::Null => nav.collection = None,
                Value::String(name) => {
                    let library = world.get_resource::<ContentLibrary>();
                    let Some((_, found)) = library.and_then(|l| l.find(name, None)) else {
                        return Err(coded(
                            RpcError::not_found(
                                format!("no collection named {name:?}"),
                                "list them with editor.collection_list",
                            ),
                            "view_error",
                            "view_bad_collection",
                        ));
                    };
                    nav.collection = Some(found.name.clone());
                }
                _ => return Err(bad_view("`collection` must be a name or null".to_string())),
            }
        }
        Ok(())
    })();
    let library = world.remove_resource::<ContentLibrary>().unwrap_or_default();
    let generation = world
        .get_resource::<crate::content_index::ContentIndex>()
        .map_or(0, |index| index.generation);
    let keep = nav.keep(
        library.personal.hide_empty_folders,
        world.get_resource::<crate::content_index::ContentIndex>(),
    );
    let rows = nav.rows(&folders, generation, keep.as_deref());
    let out = json!({
        "folder": tree_path(&nav.folder),
        "collection": nav.collection,
        "breadcrumbs": breadcrumbs(&nav.folder),
        "can_back": nav.can_back(),
        "can_forward": nav.can_forward(),
        "tree": {
            "expanded": nav.expanded.iter().collect::<Vec<_>>(),
            "root_open": !nav.root_collapsed,
            "rows": rows.iter().take(500).map(|row| json!({
                "path": row.path,
                "depth": row.depth,
                "has_children": row.has_children,
                "expanded": row.expanded,
                "favorite": library.personal.favorites.contains(&row.path),
                "color": library.folder_color(&row.path),
            })).collect::<Vec<_>>(),
        },
    });
    world.insert_resource(library);
    world.insert_resource(nav);
    result.map(|()| out)
}

/// The `folder` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_FOLDER: ParamSchema = ParamSchema {
    name: "folder",
    kind: "string",
    required: false,
    description: "Show this folder (assets/... or assets); the current one goes on the back history.",
};

/// The `back` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_BACK: ParamSchema = ParamSchema {
    name: "back",
    kind: "boolean",
    required: false,
    description: "Step back in the folder history (Alt+Left).",
};

/// The `forward` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_FORWARD: ParamSchema = ParamSchema {
    name: "forward",
    kind: "boolean",
    required: false,
    description: "Step forward in the folder history (Alt+Right).",
};

/// The `expand` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_EXPAND: ParamSchema = ParamSchema {
    name: "expand",
    kind: "array",
    required: false,
    description: "Folders to open in the tree.",
};

/// The `collapse` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_COLLAPSE: ParamSchema = ParamSchema {
    name: "collapse",
    kind: "array",
    required: false,
    description: "Folders to close in the tree.",
};

/// The `collection` parameter of `editor.content_view` ([`view_nav`]).
pub const NAV_COLLECTION: ParamSchema = ParamSchema {
    name: "collection",
    kind: "string",
    required: false,
    description: "List this collection instead of a folder (null: back to the folder).",
};

fn bad_favorites(message: String, code: &str) -> RpcError {
    coded(RpcError::invalid_params(message), "favorites_error", code)
}

/// A favorites edit that failed, as a protocol error (`favorites_error`).
fn favorites_rpc(error: CollectionError) -> RpcError {
    match error.code {
        "collection_file_unreadable" => coded(
            RpcError::invalid_params(error.message),
            "favorites_error",
            "favorites_file_unreadable",
        ),
        "collection_write_failed" => coded(
            RpcError::internal(error.message),
            "favorites_error",
            "favorites_write_failed",
        ),
        code => bad_favorites(error.message, code),
    }
}

/// `editor.content_favorites_get`: the report only (a read-only session's way in).
fn content_favorites_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    content_favorites_method(world, &json!({}))
}

/// `editor.content_favorites`.
fn content_favorites_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(root) = crate::content_collections::library(world) else {
        return Err(coded(
            RpcError::not_found("no project is open", "start the editor with --project <dir>"),
            "favorites_error",
            "favorites_no_project",
        ));
    };
    let folders = {
        let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
        index.ensure(Some(&root));
        index.folders()
    };
    let folder_param = |key: &str| -> Result<Option<String>, RpcError> {
        let Some(value) = params.get(key) else {
            return Ok(None);
        };
        let text = value
            .as_str()
            .ok_or_else(|| bad_favorites(format!("`{key}` must be a folder path"), "favorites_bad_params"))?;
        let path = tree_path(text.trim().trim_end_matches('/')).replace('\\', "/");
        if path != "assets" && folders.binary_search_by(|f| f.as_str().cmp(&path)).is_err() {
            return Err(bad_favorites(
                format!("{path} is not a folder of the project"),
                "favorites_bad_folder",
            ));
        }
        Ok(Some(path))
    };
    let add = folder_param("add")?;
    let remove = folder_param("remove")?;
    let order: Option<Vec<String>> = match params.get("order") {
        None => None,
        Some(order) => Some(
            order
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|i| i.as_str().map(|s| tree_path(s).to_string()))
                        .collect()
                })
                .ok_or_else(|| {
                    bad_favorites(
                        "`order` must be an array of favorite folders".to_string(),
                        "favorites_bad_params",
                    )
                })?,
        ),
    };
    let color: Option<(String, Option<String>)> = match params.get("color") {
        None => None,
        Some(color) => {
            let folder = color.get("folder").and_then(Value::as_str).ok_or_else(|| {
                bad_favorites(
                    "`color` is {folder, token} (token null clears it)".to_string(),
                    "favorites_bad_params",
                )
            })?;
            let path = tree_path(folder.trim().trim_end_matches('/')).to_string();
            if path != "assets" && folders.binary_search_by(|f| f.as_str().cmp(&path)).is_err() {
                return Err(bad_favorites(
                    format!("{path} is not a folder of the project"),
                    "favorites_bad_folder",
                ));
            }
            let token = match color.get("token") {
                None | Some(Value::Null) => None,
                Some(Value::String(token)) if FOLDER_COLORS.contains(&token.as_str()) => Some(token.clone()),
                Some(_) => {
                    return Err(bad_favorites(
                        format!("the color token must be one of {}", FOLDER_COLORS.join(", ")),
                        "favorites_bad_color",
                    ));
                }
            };
            Some((path, token))
        }
    };
    let hide = match params.get("hide_empty_folders") {
        None => None,
        Some(hide) => Some(hide.as_bool().ok_or_else(|| {
            bad_favorites(
                "`hide_empty_folders` must be true or false".to_string(),
                "favorites_bad_params",
            )
        })?),
    };
    let mut library = world.resource_mut::<ContentLibrary>();
    // Only a call that changes something reads the file again and writes it (review row 12).
    if add.is_some() || remove.is_some() || order.is_some() || color.is_some() || hide.is_some() {
        library
            .edit(CollectionScope::Personal, |file| {
                let favorites = &mut file.favorites;
                if let Some(path) = add
                    && !favorites.contains(&path)
                {
                    favorites.push(path);
                }
                if let Some(path) = remove {
                    let Some(at) = favorites.iter().position(|f| *f == path) else {
                        return Err(CollectionError::new(
                            "favorites_not_found",
                            format!("{path} is not a favorite"),
                        ));
                    };
                    favorites.remove(at);
                }
                if let Some(order) = order {
                    let mut sorted_new = order.clone();
                    sorted_new.sort();
                    let mut sorted_old = favorites.clone();
                    sorted_old.sort();
                    if sorted_new != sorted_old {
                        return Err(CollectionError::new(
                            "favorites_bad_params",
                            "`order` must list every favorite exactly once",
                        ));
                    }
                    *favorites = order;
                }
                if let Some((path, token)) = color {
                    match token {
                        Some(token) => file.folder_colors.insert(path, token),
                        None => file.folder_colors.remove(&path),
                    };
                }
                if let Some(hide) = hide {
                    file.hide_empty_folders = hide;
                }
                Ok(())
            })
            .map_err(favorites_rpc)?;
    }
    let mut colors = library.shared.folder_colors.clone();
    colors.extend(library.personal.folder_colors.clone());
    Ok(json!({
        "favorites": library.personal.favorites,
        "folder_colors": colors,
        "hide_empty_folders": library.personal.hide_empty_folders,
        "color_tokens": FOLDER_COLORS,
    }))
}

/// The navigation's agent surface (`editor.content_favorites`; the folder, history and tree are
/// part of `editor.content_view`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.content_favorites",
                tool: "content_favorites",
                description: "Changes the Content Browser's favorite folders and browser settings (personal file): add \
                or remove a favorite, reorder them, color a folder with a theme token, hide empty folders. Each \
                change is applied to the file as it is on disk now; a file this editor cannot read is never \
                written (favorites_file_unreadable). Without params it reports and writes nothing; a read-only \
                session reads them with editor.content_favorites_get.",
                params: &[
                    ParamSchema {
                        name: "add",
                        kind: "string",
                        required: false,
                        description: "A folder to star (assets/...).",
                    },
                    ParamSchema {
                        name: "remove",
                        kind: "string",
                        required: false,
                        description: "A favorite to remove.",
                    },
                    ParamSchema {
                        name: "order",
                        kind: "array",
                        required: false,
                        description: "Every favorite, in the new order.",
                    },
                    ParamSchema {
                        name: "color",
                        kind: "object",
                        required: false,
                        description: "{folder, token}: signal, warn, error, strong or muted; token null clears it.",
                    },
                    ParamSchema {
                        name: "hide_empty_folders",
                        kind: "boolean",
                        required: false,
                        description: "Hide folders with no asset under them in the tree.",
                    },
                ],
                result: FAVORITES_RESULT,
                mutating: true,
                permission: Permission::Assets,
            },
            content_favorites_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_favorites_get",
                tool: "content_favorites_get",
                description: "The Content Browser's favorite folders, folder colors (shared and personal; personal \
                wins) and browser settings. Read-only: change them with editor.content_favorites.",
                params: &[],
                result: FAVORITES_RESULT,
                mutating: false,
                permission: Permission::Read,
            },
            content_favorites_get as Handler,
        ),
    ]
}

const FAVORITES_RESULT: &str = "{ favorites, folder_colors, hide_empty_folders, color_tokens }";

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(list: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        out.sort();
        out
    }

    const TREE: &[&str] = &[
        "assets/props",
        "assets/props/rocks",
        "assets/props/rocks/big",
        "assets/props-old",
        "assets/tex",
    ];

    fn paths(rows: &[TreeRow]) -> Vec<&str> {
        rows.iter().map(|row| row.path.as_str()).collect()
    }

    #[test]
    fn the_tree_lists_open_folders_depth_first_by_name() {
        let folders = folders(TREE);
        let mut open = BTreeSet::new();
        let rows = tree_rows(&folders, &open, true, None);
        assert_eq!(
            paths(&rows),
            ["assets", "assets/props", "assets/props-old", "assets/tex"]
        );
        assert!(rows[1].has_children && !rows[1].expanded);
        open.insert("assets/props".to_string());
        open.insert("assets/props/rocks".to_string());
        let rows = tree_rows(&folders, &open, true, None);
        assert_eq!(
            paths(&rows),
            [
                "assets",
                "assets/props",
                "assets/props/rocks",
                "assets/props/rocks/big",
                "assets/props-old",
                "assets/tex"
            ],
            "a folder's subfolders come right after it, not after a sibling that sorts between"
        );
        assert_eq!(rows[3].depth, 3);
        assert_eq!(
            paths(&tree_rows(&folders, &open, false, None)),
            ["assets"],
            "closed root"
        );
        let keep = folders_with_assets(["assets/props/rocks/big/a.glb", "assets/tex/t.png"]);
        let rows = tree_rows(&folders, &open, true, Some(&keep));
        assert_eq!(
            paths(&rows),
            [
                "assets",
                "assets/props",
                "assets/props/rocks",
                "assets/props/rocks/big",
                "assets/tex"
            ],
            "hide empty folders drops props-old"
        );
    }

    #[test]
    fn keys_walk_open_and_close_the_tree() {
        let folders = folders(TREE);
        let mut nav = ContentNav::default();
        let rows = |nav: &mut ContentNav| nav.rows(&folders, 1, None);
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::Down));
        assert_eq!(nav.folder, "assets/props");
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::Right), "opens props");
        assert!(nav.expanded.contains("assets/props"));
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::Right), "moves to the first subfolder");
        assert_eq!(nav.folder, "assets/props/rocks");
        let r = rows(&mut nav);
        assert!(
            nav.tree_key(&r, TreeKey::Left),
            "rocks is closed: go to its parent"
        );
        assert_eq!(nav.folder, "assets/props");
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::Left), "closes props");
        assert!(!nav.expanded.contains("assets/props"));
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::End));
        assert_eq!(nav.folder, "assets/tex");
        let r = rows(&mut nav);
        assert!(!nav.tree_key(&r, TreeKey::Down), "the last row stays");
        assert!(nav.tree_key(&r, TreeKey::Home));
        assert_eq!(nav.folder, "", "the root row is the assets folder");
        let r = rows(&mut nav);
        assert!(nav.tree_key(&r, TreeKey::Left), "closes the root");
        assert_eq!(paths(&rows(&mut nav)), ["assets"]);
    }

    #[test]
    fn history_steps_back_and_forward_and_a_new_folder_drops_forward() {
        let mut nav = ContentNav::default();
        let mut state = ContentBrowserState::default();
        nav.sync(&mut state);
        assert!(nav.navigate("assets/props"));
        assert!(!nav.navigate("assets/props"), "the same folder is no step");
        nav.navigate("assets/props/rocks/big");
        assert!(nav.expanded.contains("assets/props") && nav.expanded.contains("assets/props/rocks"));
        nav.sync(&mut state);
        assert_eq!(
            state.folder, "assets/props/rocks/big",
            "the panel shows the pending folder"
        );
        assert!(nav.back());
        assert!(nav.back());
        nav.sync(&mut state);
        assert_eq!(state.folder, "");
        assert!(!nav.back(), "nothing before the start");
        assert!(nav.forward());
        assert_eq!(nav.folder, "assets/props");
        nav.navigate("assets/tex");
        assert!(!nav.can_forward(), "a new folder drops the forward history");
        // Something else (an audit panel's reveal) picks a folder: it joins the history.
        nav.sync(&mut state);
        state.reveal("assets/props/rocks/a.glb");
        nav.sync(&mut state);
        assert_eq!(nav.folder, "assets/props/rocks");
        assert!(nav.back());
        assert_eq!(nav.folder, "assets/tex");
        for i in 0..(HISTORY_LIMIT + 10) {
            nav.navigate(&format!("assets/f{i}"));
        }
        assert_eq!(nav.back_history().len(), HISTORY_LIMIT);
    }

    #[test]
    fn breadcrumbs_and_crumb_menus() {
        assert_eq!(breadcrumbs(""), ["assets"]);
        assert_eq!(
            breadcrumbs("assets/props/rocks"),
            ["assets", "assets/props", "assets/props/rocks"]
        );
        let folders = folders(TREE);
        assert_eq!(
            subfolders(&folders, "assets"),
            ["assets/props", "assets/props-old", "assets/tex"]
        );
        assert_eq!(subfolders(&folders, "assets/props"), ["assets/props/rocks"]);
        assert!(subfolders(&folders, "assets/tex").is_empty());
    }

    fn project(tag: &str) -> std::path::PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        for rel in [
            "assets/props/rocks/big/a.glb",
            "assets/tex/t.png",
            "assets/empty/.keep",
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            regia_test_utils::write_atomic(&path, b"x").expect("write");
        }
        root
    }

    fn world_for(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world
    }

    fn code<'a>(err: &'a RpcError, field: &str) -> Option<&'a str> {
        err.data.as_ref()?.get(field)?.as_str()
    }

    #[test]
    fn the_agent_navigates_the_tree_and_the_history() {
        let root = project("content_nav_agent");
        let mut world = world_for(&root);
        let reply = view_nav(&mut world, &json!({ "folder": "assets/props/rocks/big" })).expect("folder");
        assert_eq!(reply["folder"], "assets/props/rocks/big");
        assert_eq!(
            reply["breadcrumbs"],
            json!([
                "assets",
                "assets/props",
                "assets/props/rocks",
                "assets/props/rocks/big"
            ])
        );
        assert_eq!(reply["can_back"], true);
        assert_eq!(
            reply["tree"]["rows"][4]["path"], "assets/props/rocks/big",
            "after assets/empty"
        );
        let reply = view_nav(&mut world, &json!({ "back": true })).expect("back");
        assert_eq!(reply["folder"], "assets");
        assert_eq!(reply["can_forward"], true);
        let reply = view_nav(&mut world, &json!({ "collapse": ["assets/props"] })).expect("collapse");
        assert!(
            !reply["tree"]["rows"]
                .as_array()
                .expect("rows")
                .iter()
                .any(|r| r["path"] == "assets/props/rocks"),
            "{reply}"
        );
        let err = view_nav(&mut world, &json!({ "folder": "assets/nope" })).expect_err("bad folder");
        assert_eq!(code(&err, "view_error"), Some("view_bad_folder"));
        let err = view_nav(&mut world, &json!({ "back": 1 })).expect_err("bad param");
        assert_eq!(code(&err, "view_error"), Some("view_bad_params"));
        let err = view_nav(&mut world, &json!({ "collection": "none" })).expect_err("no collection");
        assert_eq!(code(&err, "view_error"), Some("view_bad_collection"));
        let mut empty = World::new();
        let err = view_nav(&mut empty, &json!({ "folder": "assets" })).expect_err("no project");
        assert_eq!(code(&err, "view_error"), Some("view_no_project"));
        // The panel picks the agent's folder up on its next frame.
        view_nav(&mut world, &json!({ "folder": "assets/tex" })).expect("folder");
        let mut state = ContentBrowserState::default();
        world.resource_mut::<ContentNav>().sync(&mut state);
        assert_eq!(state.folder, "assets/tex");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn favorites_colors_and_hidden_empty_folders_through_the_agent() {
        let root = project("content_nav_favorites");
        let mut world = world_for(&root);
        content_favorites_method(&mut world, &json!({ "add": "assets/tex" })).expect("add");
        let reply = content_favorites_method(&mut world, &json!({ "add": "assets/props/" })).expect("add");
        assert_eq!(reply["favorites"], json!(["assets/tex", "assets/props"]));
        let reply = content_favorites_method(&mut world, &json!({ "order": ["assets/props", "assets/tex"] }))
            .expect("order");
        assert_eq!(reply["favorites"], json!(["assets/props", "assets/tex"]));
        assert!(move_favorite(&mut world, 1, 0));
        let reply = content_favorites_method(
            &mut world,
            &json!({ "color": { "folder": "assets/tex", "token": "warn" }, "hide_empty_folders": true }),
        )
        .expect("color");
        assert_eq!(reply["favorites"], json!(["assets/tex", "assets/props"]));
        assert_eq!(reply["folder_colors"]["assets/tex"], "warn");
        let tree = view_nav(&mut world, &json!({})).expect("view");
        let rows = tree["tree"]["rows"].as_array().expect("rows");
        assert!(
            !rows.iter().any(|r| r["path"] == "assets/empty"),
            "hidden: {tree}"
        );
        assert!(
            rows.iter()
                .any(|r| r["path"] == "assets/tex" && r["favorite"] == true && r["color"] == "warn")
        );
        for (params, expected) in [
            (json!({ "add": "assets/nope" }), "favorites_bad_folder"),
            (json!({ "remove": "assets/empty" }), "favorites_not_found"),
            (json!({ "order": ["assets/tex"] }), "favorites_bad_params"),
            (
                json!({ "color": { "folder": "assets/tex", "token": "#ff0000" } }),
                "favorites_bad_color",
            ),
        ] {
            let err = content_favorites_method(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err, "favorites_error"), Some(expected), "{params}");
        }
        let mut empty = World::new();
        let err = content_favorites_method(&mut empty, &json!({})).expect_err("no project");
        assert_eq!(code(&err, "favorites_error"), Some("favorites_no_project"));
        assert!(toggle_favorite(&mut world, "assets/tex").starts_with("Removed"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The folder pane drawn alone in a window-sized frame.
    fn draw_pane(
        ctx: &egui::Context,
        world: &mut World,
        state: &mut ContentBrowserState,
        events: Vec<egui::Event>,
    ) -> egui::accesskit::TreeUpdate {
        let folders = {
            let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
            index.ensure(state.project_root.as_deref());
            index.folders()
        };
        crate::canvas_keys::test_input::frame(ctx, [600.0, 800.0], events, |ui| {
            folder_pane(ui, world, state, &folders);
        })
    }

    #[test]
    fn the_tree_is_operated_from_the_keyboard() {
        use crate::canvas_keys::test_input::{focused_name, key, tab_to};
        let root = project("content_nav_keys");
        let mut world = world_for(&root);
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        {
            let world = &mut world;
            let state = &mut state;
            let folders = {
                let index = world.resource::<crate::content_index::ContentIndex>();
                index.folders()
            };
            tab_to(&ctx, [600.0, 800.0], "Folder assets", false, 20, |ui| {
                folder_pane(ui, world, state, &folders);
            });
        }
        let none = Modifiers::NONE;
        for (press, folder, focused) in [
            (Key::ArrowDown, "assets/empty", "Folder assets/empty"),
            (Key::ArrowDown, "assets/props", "Folder assets/props"),
            (Key::ArrowRight, "assets/props", "Folder assets/props"),
            (Key::ArrowRight, "assets/props/rocks", "Folder assets/props/rocks"),
            (Key::ArrowLeft, "assets/props", "Folder assets/props"),
            (Key::End, "assets/tex", "Folder assets/tex"),
        ] {
            draw_pane(&ctx, &mut world, &mut state, vec![key(press, none)]);
            let update = draw_pane(&ctx, &mut world, &mut state, vec![]);
            assert_eq!(state.folder, folder, "{press:?}");
            assert_eq!(
                focused_name(&update).as_deref(),
                Some(focused),
                "{press:?} keeps the focus on the row"
            );
        }
        // Alt+Left from a focused row steps back through the history.
        draw_pane(
            &ctx,
            &mut world,
            &mut state,
            vec![key(Key::ArrowLeft, Modifiers::ALT)],
        );
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        assert_eq!(state.folder, "assets/props", "back");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 17: with "Hide empty folders" on, idle frames build the keep set once.
    #[test]
    fn idle_frames_build_the_hidden_folder_set_once() {
        let root = project("content_nav_idle");
        let mut world = world_for(&root);
        content_favorites_method(&mut world, &json!({ "hide_empty_folders": true })).expect("hide");
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        for _ in 0..10 {
            draw_pane(&ctx, &mut world, &mut state, vec![]);
        }
        assert_eq!(
            world.resource::<ContentNav>().keep_builds,
            1,
            "once per index generation"
        );
        let tree = view_nav(&mut world, &json!({})).expect("view");
        assert!(
            !tree["tree"]["rows"]
                .as_array()
                .expect("rows")
                .iter()
                .any(|r| r["path"] == "assets/empty"),
            "{tree}"
        );
        assert_eq!(
            world.resource::<ContentNav>().keep_builds,
            1,
            "the agent reuses it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 31: "Save the search" saves the typed text and the Filter chips.
    #[test]
    fn saving_the_search_keeps_the_filter_chips() {
        use crate::canvas_keys::test_input::{key, tab_to};
        let root = project("content_nav_save_search");
        let mut world = world_for(&root);
        let mut filter = crate::content_index::ContentFilter::default();
        filter.toggle_kind("mesh");
        world.insert_resource(filter);
        world.insert_resource(NewCollectionForm {
            name: "Meshes".to_string(),
            shared: false,
            from_search: true,
        });
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        {
            let world = &mut world;
            let state = &mut state;
            let folders = world.resource::<crate::content_index::ContentIndex>().folders();
            tab_to(&ctx, [600.0, 800.0], "Create the collection", false, 20, |ui| {
                folder_pane(ui, world, state, &folders);
            });
        }
        draw_pane(
            &ctx,
            &mut world,
            &mut state,
            vec![key(Key::Enter, Modifiers::NONE)],
        );
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        let library = world.resource::<ContentLibrary>();
        let (_, saved) = library.find("Meshes", None).expect("created");
        assert_eq!(saved.query.as_deref(), Some("type:mesh"), "{saved:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 12: a report call (`{}`) never writes the personal file, an edit is refused
    /// while the file cannot be read, and the method that writes is declared as one.
    #[test]
    fn reading_favorites_never_writes_the_personal_file() {
        let root = project("favorites_read_only");
        let config = regia_test_utils::unique_temp_dir("favorites_read_only_cfg");
        let mut world = world_for(&root);
        world.insert_resource(crate::editor_prefs::EditorConfigPaths {
            dir: Some(config.clone()),
        });
        let personal = crate::content_collections::personal_path(&root);
        std::fs::create_dir_all(personal.parent().expect("parent")).expect("dirs");
        // A newer editor's file: unreadable here, and it must survive.
        let newer = b"(format_version: 2, collections: [(name: \"Mine\")], favorites: [\"assets/tex\"])";
        regia_test_utils::write_atomic(&personal, newer).expect("write");
        let _ = content_favorites_method(&mut world, &json!({}));
        assert_eq!(
            std::fs::read(&personal).expect("read"),
            newer,
            "a report writes nothing"
        );
        let err =
            content_favorites_method(&mut world, &json!({ "add": "assets/props" })).expect_err("refused");
        assert_eq!(
            code(&err, "favorites_error"),
            Some("favorites_file_unreadable"),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read(&personal).expect("read"),
            newer,
            "an edit is refused"
        );
        // A readable, hand-formatted file: a report leaves it byte-identical too.
        let hand = "// my favorites\n(format_version: 1, favorites: [\"assets/tex\"])\n";
        regia_test_utils::write_atomic(&personal, hand.as_bytes()).expect("write");
        let mut fresh = world_for(&root);
        fresh.insert_resource(crate::editor_prefs::EditorConfigPaths {
            dir: Some(config.clone()),
        });
        let reply = content_favorites_method(&mut fresh, &json!({})).expect("report");
        assert_eq!(reply["favorites"], json!(["assets/tex"]));
        assert_eq!(std::fs::read_to_string(&personal).expect("read"), hand);
        // The writing method is declared as one; a read-only session reads through `_get`.
        let methods = methods();
        let schema = |name: &str| methods.iter().find(|(s, _)| s.name == name).map(|(s, _)| s);
        let set = schema("editor.content_favorites").expect("set method");
        assert!(set.mutating && set.permission != Permission::Read, "{}", set.name);
        let get = schema("editor.content_favorites_get").expect("get method");
        assert!(
            !get.mutating && get.permission == Permission::Read,
            "{}",
            get.name
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }

    /// Review row 14: an agent's (or a reveal's) navigation scrolls the tree but never takes the
    /// keyboard focus from a text field the user is typing in.
    #[test]
    fn navigation_does_not_steal_the_keyboard_focus() {
        let root = project("content_nav_focus");
        let mut world = world_for(&root);
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let field = egui::Id::new("another-panel-field");
        let mut text = String::new();
        let draw = |world: &mut World, state: &mut ContentBrowserState, text: &mut String, focus: bool| {
            let folders = {
                let mut index =
                    world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
                index.ensure(state.project_root.as_deref());
                index.folders()
            };
            crate::canvas_keys::test_input::frame(&ctx, [600.0, 800.0], vec![], |ui| {
                let response = ui.add(egui::TextEdit::singleline(text).id(field));
                if focus {
                    response.request_focus();
                }
                folder_pane(ui, world, state, &folders);
            });
        };
        draw(&mut world, &mut state, &mut text, true);
        draw(&mut world, &mut state, &mut text, false);
        assert!(ctx.memory(|m| m.has_focus(field)), "the field has the focus");
        view_nav(&mut world, &json!({ "folder": "assets/props/rocks" })).expect("agent folder");
        for _ in 0..3 {
            draw(&mut world, &mut state, &mut text, false);
        }
        assert_eq!(state.folder, "assets/props/rocks", "the panel follows the agent");
        assert!(
            ctx.memory(|m| m.has_focus(field)),
            "the text field keeps the focus, focused: {:?}",
            ctx.memory(|m| m.focused())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review row 15: Alt+Left in a focused text field moves its cursor, not the folder history;
    /// mouse button 4 outside the browser does not step it either.
    #[test]
    fn history_keys_leave_text_fields_and_other_panels_alone() {
        use crate::canvas_keys::test_input::{frame, key};
        let mut nav = ContentNav::default();
        nav.navigate("assets/props");
        let ctx = egui::Context::default();
        let field = egui::Id::new("search-field");
        let mut text = "rock moss".to_string();
        let mut step = |events: Vec<egui::Event>, focus: bool, nav: &mut ContentNav| {
            let mut moved = false;
            frame(&ctx, [600.0, 400.0], events, |ui| {
                let response = ui.add(egui::TextEdit::singleline(&mut text).id(field));
                if focus {
                    response.request_focus();
                }
                // The browser: a 200 x 200 area at the top left.
                ui.allocate_ui(egui::vec2(200.0, 200.0), |ui| {
                    moved |= history_shortcuts(ui, nav, true);
                });
            });
            moved
        };
        step(vec![], true, &mut nav);
        step(vec![], false, &mut nav);
        assert!(!step(vec![key(Key::ArrowLeft, Modifiers::ALT)], false, &mut nav));
        assert_eq!(nav.folder, "assets/props", "Alt+Left belongs to the text field");
        assert!(nav.can_back());
        ctx.memory_mut(|m| m.surrender_focus(field));
        let press = |button| egui::Event::PointerButton {
            pos: egui::pos2(500.0, 350.0),
            button,
            pressed: true,
            modifiers: Modifiers::NONE,
        };
        let release = |button| egui::Event::PointerButton {
            pos: egui::pos2(500.0, 350.0),
            button,
            pressed: false,
            modifiers: Modifiers::NONE,
        };
        assert!(
            !step(
                vec![press(PointerButton::Extra1), release(PointerButton::Extra1)],
                false,
                &mut nav
            ),
            "mouse button 4 over another panel"
        );
        assert_eq!(nav.folder, "assets/props");
        // With no text field focused, Alt+Left over the browser still steps back.
        let field_focused = ctx.memory(|m| m.has_focus(field));
        assert!(!field_focused);
        assert!(step(vec![key(Key::ArrowLeft, Modifiers::ALT)], false, &mut nav));
        assert_eq!(nav.folder, "");
    }

    /// Review row 18: Delete on a focused collection asks first (Shift+Delete does nothing), and
    /// the confirmation is operated from the keyboard.
    #[test]
    fn deleting_a_collection_from_the_keyboard_asks_first() {
        use crate::canvas_keys::test_input::{focused_name, key, tab_to};
        let root = project("content_nav_delete");
        let mut world = world_for(&root);
        crate::content_collections::library(&mut world);
        world
            .resource_mut::<ContentLibrary>()
            .create(CollectionScope::Shared, "Hero", None)
            .expect("create");
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        {
            let world = &mut world;
            let state = &mut state;
            let folders = world.resource::<crate::content_index::ContentIndex>().folders();
            tab_to(&ctx, [600.0, 800.0], "\u{25a4} Hero", false, 20, |ui| {
                folder_pane(ui, world, state, &folders);
            });
        }
        let exists = |world: &World| world.resource::<ContentLibrary>().find("Hero", None).is_some();
        draw_pane(
            &ctx,
            &mut world,
            &mut state,
            vec![key(Key::Delete, Modifiers::SHIFT)],
        );
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        assert!(exists(&world), "Shift+Delete is not Delete");
        draw_pane(
            &ctx,
            &mut world,
            &mut state,
            vec![key(Key::Delete, Modifiers::NONE)],
        );
        let update = draw_pane(&ctx, &mut world, &mut state, vec![]);
        assert!(exists(&world), "Delete asks first");
        assert_eq!(
            focused_name(&update).as_deref(),
            Some("Delete the collection Hero"),
            "the confirmation takes the focus"
        );
        draw_pane(
            &ctx,
            &mut world,
            &mut state,
            vec![key(Key::Enter, Modifiers::NONE)],
        );
        draw_pane(&ctx, &mut world, &mut state, vec![]);
        assert!(!exists(&world), "confirmed");
        assert!(root.join("assets/tex/t.png").is_file(), "its assets stay");
        let _ = std::fs::remove_dir_all(&root);
    }
}
