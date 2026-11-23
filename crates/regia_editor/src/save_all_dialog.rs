// SPDX-License-Identifier: MPL-2.0
//! The Save all dialog (`docs/specs/19-editor.md` § 7, ADR-7822): one list of every document
//! with unsaved edits (the level, prefab edit mode and every asset editor that reports a dirty
//! document through its `crate::doc_autosave` provider), each with a checkbox, and **Save
//! selected**, **Don't save** and **Cancel**.
//!
//! File ▸ Save all opens it; the editor's close uses it in place of the old per-document prompt
//! (the host still takes the decision; an agent's `quit.request` keeps its own semantics). The
//! agent's route is `editor.save_all` (`book/agents/content-browser.md`).
//!
//! A document can be saved here when its editor has a save that needs no question: the level
//! (when it has a path) and every document whose editor registered a save hook
//! (`crate::doc_autosave::DocAutosaveRegistry::register_save`, P-147, ADR-8147): every built-in
//! asset editor does. An untitled document (no file yet) is listed with the reason and saved from
//! its editor, which asks for the file name.
//!
//! The shell's own documents (the Material panel's material and the Project Settings'
//! `Regia.toml`) live outside the world: the shell publishes their rows every frame
//! ([`ShellDocuments`]); `editor.save_all` lists them and queues their saves, which the shell runs
//! on its next frame (the answer's `queued`).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The id of the level's row.
pub const LEVEL_ID: &str = "level";

/// One document with unsaved edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyDoc {
    /// Stable id: [`LEVEL_ID`], or the document autosave id (`crate::doc_autosave::DocKey::id`).
    pub id: String,
    /// The editor kind (`level`, `prefab`, `matgraph`, ...).
    pub kind: String,
    /// What the row shows.
    pub label: String,
    /// The file it saves to, project-relative when it is inside the project.
    pub path: Option<String>,
    /// Whether this dialog can save it.
    pub can_save: bool,
    /// Why it cannot be saved here.
    pub note: Option<String>,
}

impl DirtyDoc {
    /// The agent's view.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "label": self.label,
            "path": self.path,
            "can_save": self.can_save,
            "note": self.note,
        })
    }
}

/// The shell's own documents with unsaved edits, published by the shell every frame, and the saves
/// `editor.save_all` asked for, which the shell runs on its next frame (an edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct ShellDocuments {
    /// The rows the shell published last (`shell:material`, `shell:settings`).
    pub rows: Vec<DirtyDoc>,
    /// Row ids an agent asked to save, in order.
    pub requested: Vec<String>,
    /// The last saves the shell ran for an agent: id and the error, if it failed.
    pub results: Vec<(String, Option<String>)>,
}

/// Publishes the shell's rows (the shell, once a frame).
pub fn publish_shell_rows(world: &mut World, rows: Vec<DirtyDoc>) {
    world.get_resource_or_insert_with(ShellDocuments::default).rows = rows;
}

/// Takes the shell rows' saves an agent asked for (the shell runs them).
pub fn take_shell_requests(world: &mut World) -> Vec<String> {
    world
        .get_resource_mut::<ShellDocuments>()
        .map(|mut docs| std::mem::take(&mut docs.requested))
        .unwrap_or_default()
}

/// Records what the shell's saves of the requested rows did (shown by the next `editor.save_all`).
pub fn record_shell_results(world: &mut World, results: Vec<(String, Option<String>)>) {
    if results.is_empty() {
        return;
    }
    world.get_resource_or_insert_with(ShellDocuments::default).results = results;
}

fn shell_rows(world: &World) -> Vec<DirtyDoc> {
    world
        .get_resource::<ShellDocuments>()
        .map(|docs| docs.rows.clone())
        .unwrap_or_default()
}

/// Whether the editor registered a save hook for `kind`, and whether it saves untitled documents.
fn save_hook(world: &World, kind: &str) -> Option<crate::doc_autosave::DocSaveHook> {
    world
        .get_resource::<crate::doc_autosave::DocAutosaveRegistry>()
        .and_then(|registry| registry.save_hook(kind))
}

fn display_path(root: Option<&Path>, path: &Path) -> String {
    match root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(rel) => crate::content::slash_path(rel),
        None => user_path(path),
    }
}

/// `text` (a path as `Path::display` writes it) without Windows' verbatim prefix (bug 27,
/// ADR-8586): `\\?\C:\x` reads `C:\x` and `\\?\UNC\server\share\x` reads `\\server\share\x`. The
/// project root is canonicalized, which adds the prefix on Windows; file IO keeps the canonical path,
/// only what people read drops it. Any other text is returned as it is, on every host.
#[must_use]
pub fn user_path_text(text: &str) -> String {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        rest.to_owned()
    } else {
        text.to_owned()
    }
}

/// `path` as people read it: [`Path::display`] without the verbatim prefix ([`user_path_text`]).
#[must_use]
pub fn user_path(path: &Path) -> String {
    user_path_text(&path.display().to_string())
}

/// `text` cut in the middle to at most `max_chars` characters, with `…` where the cut is (a path
/// keeps its root and its file name; ADR-8586). Shorter text is returned as it is.
#[must_use]
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let keep = max_chars - 1;
    let head = keep / 2;
    let tail = keep - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(count - tail));
    out
}

/// Draws `text` (a path) as a weak label that fits the width left in `ui`: cut in the middle when
/// it is too long, with the full text in the tooltip (bug 19, ADR-8586).
pub fn path_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let width = ui.available_width().max(0.0);
    let fits = |ui: &egui::Ui, shown: &str| {
        ui.painter()
            .layout_no_wrap(shown.to_owned(), font.clone(), color)
            .size()
            .x
            <= width
    };
    let count = text.chars().count();
    if fits(ui, text) {
        return ui.weak(text);
    }
    // The longest middle cut that fits (binary search on the kept characters).
    let (mut lo, mut hi) = (1usize, count);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(ui, &truncate_middle(text, mid)) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    ui.add(egui::Label::new(egui::RichText::new(truncate_middle(text, lo)).color(color)).truncate())
        .on_hover_text(text)
}

/// Every document with unsaved edits: the level first, then each asset editor's.
#[must_use]
pub fn dirty_documents(world: &World) -> Vec<DirtyDoc> {
    let root = crate::mesh_source::project_root(world);
    let mut out = Vec::new();
    // In prefab edit mode the undo history is the prefab's and the open level is the mode's
    // scratch level: the level's state is the stash, saved once the mode exits (ADR-8147).
    let prefab_mode = crate::prefab_edit::in_prefab_mode(world);
    if prefab_mode
        && let Some((open, _)) = world
            .get_resource::<crate::prefab_edit::PrefabEditMode>()
            .filter(|mode| mode.editing().is_some())
            .and_then(crate::prefab_edit::PrefabEditMode::dirty_stashed_level)
    {
        out.push(DirtyDoc {
            id: LEVEL_ID.to_string(),
            kind: "level".to_string(),
            label: open.path.as_deref().and_then(Path::file_name).map_or_else(
                || "untitled level".to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            path: open.path.as_deref().map(|p| display_path(root.as_deref(), p)),
            can_save: false,
            note: Some("in prefab edit mode: exit it to save the level".to_string()),
        });
    }
    if !prefab_mode
        && world
            .get_resource::<crate::command::UndoStack>()
            .is_some_and(crate::command::UndoStack::is_dirty)
    {
        let path = world
            .get_resource::<crate::level_io::OpenLevel>()
            .and_then(|open| open.path.clone());
        // An untitled level is parked as `assets/unsaved*.level.ron`, as the close always did.
        let parked = match (&path, root.as_deref()) {
            (None, Some(root)) => Some(crate::level_io::free_untitled_level_path(&root.join("assets"))),
            _ => None,
        };
        out.push(DirtyDoc {
            id: LEVEL_ID.to_string(),
            kind: "level".to_string(),
            label: path.as_deref().and_then(Path::file_name).map_or_else(
                || "untitled level".to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            path: path.as_deref().map(|p| display_path(root.as_deref(), p)),
            can_save: path.is_some() || parked.is_some(),
            note: match (&path, &parked) {
                (Some(_), _) => None,
                (None, Some(parked)) => Some(format!(
                    "no file yet: saves as {}",
                    display_path(root.as_deref(), parked)
                )),
                (None, None) => Some("no file yet: use File ▸ Save level as…".to_string()),
            },
        });
    }
    for key in crate::doc_autosave::dirty_documents(world) {
        let hook = save_hook(world, &key.kind);
        let note = match hook {
            None => Some("save it from its editor".to_string()),
            Some(hook) if key.original.is_none() && !hook.saves_untitled => {
                Some("untitled: save it from its editor".to_string())
            }
            Some(_) => None,
        };
        out.push(DirtyDoc {
            id: key.id(),
            kind: key.kind.clone(),
            label: key.label.clone(),
            path: key.original.as_deref().map(|p| display_path(root.as_deref(), p)),
            can_save: note.is_none(),
            note,
        });
    }
    out
}

/// Every document with unsaved edits and the shell's rows ([`ShellDocuments`]): what
/// `editor.save_all` lists.
#[must_use]
pub fn all_documents(world: &World) -> Vec<DirtyDoc> {
    let mut docs = dirty_documents(world);
    docs.extend(shell_rows(world));
    docs
}

/// What [`save_documents`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveOutcome {
    /// Ids saved.
    pub saved: Vec<String>,
    /// Ids that failed, with the reason.
    pub failed: Vec<(String, String)>,
    /// Ids that are not dirty (anymore) or that this dialog cannot save.
    pub skipped: Vec<String>,
    /// Shell rows queued for the shell's next frame ([`ShellDocuments`]).
    pub queued: Vec<String>,
}

impl SaveOutcome {
    /// The agent's answer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "saved": self.saved,
            "failed": self.failed.iter().map(|(id, error)| json!({ "id": id, "error": error })).collect::<Vec<_>>(),
            "skipped": self.skipped,
            "queued": self.queued,
        })
    }
}

fn save_one(world: &mut World, doc: &DirtyDoc) -> Result<(), String> {
    match doc.kind.as_str() {
        "level" => {
            let untitled = world
                .get_resource::<crate::level_io::OpenLevel>()
                .is_none_or(|open| open.path.is_none());
            let params = match (untitled, crate::mesh_source::project_root(world)) {
                (true, Some(root)) => {
                    let parked = crate::level_io::free_untitled_level_path(&root.join("assets"));
                    json!({ "path": display_path(Some(&root), &parked) })
                }
                _ => json!({}),
            };
            crate::agent_ext::editor_save_level(world, &params)
                .map(|_| ())
                .map_err(|e| e.message)
        }
        _ => {
            // The editor's own save, through the hook it registered (P-147, ADR-8147).
            // Inline assets (ADR-8107) register one hook and save one file per row.
            let key = crate::doc_autosave::dirty_documents(world)
                .into_iter()
                .find(|key| key.id() == doc.id);
            match key {
                Some(key) => crate::doc_autosave::save_doc(world, &key),
                // Saved by a sibling's save already (an editor that writes several at once).
                None => Ok(()),
            }
        }
    }
}

/// Saves the dirty documents named by `ids` (all savable ones when `ids` is empty is the
/// caller's choice: pass them). An id that is not dirty is skipped.
pub fn save_documents(world: &mut World, ids: &[String]) -> SaveOutcome {
    let _span = tracing::info_span!(target: "regia_editor", "save_all_documents").entered();
    let docs = dirty_documents(world);
    let shell = shell_rows(world);
    let mut out = SaveOutcome::default();
    for id in ids {
        if let Some(row) = shell.iter().find(|row| row.id == *id) {
            if row.can_save {
                let mut requests = world.get_resource_or_insert_with(ShellDocuments::default);
                if !requests.requested.contains(id) {
                    requests.requested.push(id.clone());
                }
                out.queued.push(id.clone());
            } else {
                out.skipped.push(id.clone());
            }
            continue;
        }
        let Some(doc) = docs.iter().find(|doc| doc.id == *id) else {
            out.skipped.push(id.clone());
            continue;
        };
        if !doc.can_save {
            out.skipped.push(id.clone());
            continue;
        }
        // An editor that writes several documents at once (or a level save that saved the
        // prefab's stash) may have saved this one already. Inline assets stay listed until
        // their own file is clean, because one save does not cover the kind.
        if doc.kind != "level" && !dirty_documents(world).iter().any(|now| now.id == doc.id) {
            out.saved.push(id.clone());
            continue;
        }
        match save_one(world, doc) {
            Ok(()) => out.saved.push(id.clone()),
            Err(error) => out.failed.push((id.clone(), error)),
        }
    }
    crate::content_index::invalidate(world);
    out
}

// ---------------------------------------------------------------------------
// The dialog
// ---------------------------------------------------------------------------

/// The level a switch goes to once the unsaved documents are dealt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LevelSwitch {
    /// File ▸ New level.
    New,
    /// File ▸ Open level… or File ▸ Recent: this file.
    Open(std::path::PathBuf),
}

/// Why the dialog is open: what happens after **Save selected** or **Don't save**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavePurpose {
    /// File ▸ Save all: nothing else happens.
    SaveAll,
    /// The editor is closing: the host exits after the choice.
    Close,
    /// Another level is about to replace the open one.
    LevelSwitch(LevelSwitch),
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveDecision {
    /// The checked documents saved (and the shell's extra rows named here are for the caller).
    Saved {
        /// Checked extra rows (ids the caller passed in), to be saved by the caller.
        extra: Vec<String>,
    },
    /// Nothing saved, go ahead.
    DontSave,
    /// Nothing saved, stay.
    Cancel,
}

/// The dialog's state (an edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct SaveAllDialog {
    /// Open, and for what.
    pub purpose: Option<SavePurpose>,
    /// Unchecked row ids (every row starts checked).
    unchecked: Vec<String>,
    /// The failures of the last **Save selected**.
    pub errors: Vec<String>,
    /// The choice, until the caller takes it.
    decision: Option<(SavePurpose, SaveDecision)>,
    /// Keyboard focus asked for the first button once.
    focus_asked: bool,
}

impl SaveAllDialog {
    /// Opens the dialog.
    pub fn open(&mut self, purpose: SavePurpose) {
        *self = Self {
            purpose: Some(purpose),
            ..Self::default()
        };
    }

    /// Whether `id` is checked.
    #[must_use]
    pub fn is_checked(&self, id: &str) -> bool {
        !self.unchecked.iter().any(|u| u == id)
    }

    /// Checks or unchecks a row.
    pub fn set_checked(&mut self, id: &str, on: bool) {
        self.unchecked.retain(|u| u != id);
        if !on {
            self.unchecked.push(id.to_string());
        }
    }

    /// The choice once made (taken once).
    pub fn take_decision(&mut self) -> Option<(SavePurpose, SaveDecision)> {
        self.decision.take()
    }

    /// Carries out a choice: Save selected saves the checked savable rows of the world and
    /// returns the checked `extra` ids for the caller; a failure keeps the dialog open with the
    /// errors listed.
    pub fn choose(world: &mut World, choice: SaveDecision, extra: &[DirtyDoc]) {
        let mut dialog = world.remove_resource::<Self>().unwrap_or_default();
        let Some(purpose) = dialog.purpose.clone() else {
            world.insert_resource(dialog);
            return;
        };
        let decision = match choice {
            SaveDecision::Saved { .. } => {
                let ids: Vec<String> = dirty_documents(world)
                    .into_iter()
                    .filter(|doc| doc.can_save && dialog.is_checked(&doc.id))
                    .map(|doc| doc.id)
                    .collect();
                let outcome = save_documents(world, &ids);
                dialog.errors = outcome
                    .failed
                    .iter()
                    .map(|(id, error)| format!("{id}: {error}"))
                    .collect();
                if !dialog.errors.is_empty() {
                    world.insert_resource(dialog);
                    return;
                }
                SaveDecision::Saved {
                    extra: extra
                        .iter()
                        .filter(|doc| doc.can_save && dialog.is_checked(&doc.id))
                        .map(|doc| doc.id.clone())
                        .collect(),
                }
            }
            other => other,
        };
        dialog.purpose = None;
        dialog.decision = Some((purpose, decision));
        world.insert_resource(dialog);
    }
}

/// Opens the dialog (File ▸ Save all, or the close).
pub fn open_dialog(world: &mut World, purpose: SavePurpose) {
    world
        .get_resource_or_insert_with(SaveAllDialog::default)
        .open(purpose);
}

/// Draws the dialog when it is open. `extra` rows are the caller's own documents (the shell's
/// material panel and project settings); their saving is the caller's on
/// [`SaveDecision::Saved`]. Every control is a focusable widget: Tab reaches each checkbox and
/// button, Space/Enter operates them, and the first button takes the focus when it opens.
pub fn dialog_window(ctx: &egui::Context, world: &mut World, extra: &[DirtyDoc]) {
    let Some(mut dialog) = world.remove_resource::<SaveAllDialog>() else {
        return;
    };
    let Some(purpose) = dialog.purpose.clone() else {
        world.insert_resource(dialog);
        return;
    };
    let mut rows = dirty_documents(world);
    rows.extend(extra.iter().cloned());
    let mut choice = None;
    let title = match purpose {
        SavePurpose::SaveAll => "Save all",
        SavePurpose::Close | SavePurpose::LevelSwitch(_) => "Unsaved changes",
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            if rows.is_empty() {
                ui.label("Every document is saved.");
            } else {
                ui.label("These documents have unsaved changes:");
            }
            for row in &rows {
                let mut on = row.can_save && dialog.is_checked(&row.id);
                let text = match &row.path {
                    Some(path) => format!("{} ({path})", row.label),
                    None => row.label.clone(),
                };
                ui.horizontal(|ui| {
                    let response = ui.add_enabled(row.can_save, egui::Checkbox::new(&mut on, text));
                    if response.changed() {
                        dialog.set_checked(&row.id, on);
                    }
                    if let Some(note) = &row.note {
                        ui.weak(note);
                    }
                });
            }
            for error in &dialog.errors {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            // File ▸ Save all with nothing unsaved: the sentence above and one Close (ADR-8583).
            if rows.is_empty() && matches!(purpose, SavePurpose::SaveAll) {
                let close = crate::agent_parity::action_button_text(
                    ui,
                    &crate::agent_parity::SAVE_ALL_CANCEL,
                    "Close",
                );
                if !dialog.focus_asked {
                    close.request_focus();
                    dialog.focus_asked = true;
                }
                if close.clicked()
                    || ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    choice = Some(SaveDecision::Cancel);
                }
                return;
            }
            ui.horizontal(|ui| {
                let save =
                    crate::agent_parity::action_button(ui, &crate::agent_parity::SAVE_ALL_SAVE_SELECTED);
                if !dialog.focus_asked {
                    save.request_focus();
                    dialog.focus_asked = true;
                }
                if save.clicked() {
                    choice = Some(SaveDecision::Saved { extra: Vec::new() });
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::SAVE_ALL_DONT_SAVE).clicked()
                {
                    choice = Some(SaveDecision::DontSave);
                }
                if crate::agent_parity::action_button(ui, &crate::agent_parity::SAVE_ALL_CANCEL).clicked()
                    || ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    choice = Some(SaveDecision::Cancel);
                }
            });
        });
    world.insert_resource(dialog);
    if let Some(choice) = choice {
        SaveAllDialog::choose(world, choice, extra);
    }
}

// ---------------------------------------------------------------------------
// Agent method
// ---------------------------------------------------------------------------

/// `editor.save_all`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.save_all",
            tool: "save_all",
            description: "The Save all dialog's lists and saves. `dialog: true` opens the dialog in the editor and \
                answers the documents; `list: true` answers them without opening it; `ids` saves those \
                documents (ids from the list: `level`, a document id, or the shell's `shell:material` and \
                `shell:settings`); with none of them every document the dialog can save is saved. Every asset \
                editor's document saves through its editor's save hook; the shell rows are queued and saved by \
                the shell's next frame (`queued`; `shell_results` reports the last ones); `cancel: true` \
                closes the open dialog as its Cancel button does. Each document: {id, kind, label, path, \
                can_save, note}.",
            params: &[
                ParamSchema {
                    name: "ids",
                    kind: "array",
                    required: false,
                    description: "Document ids to save.",
                },
                ParamSchema {
                    name: "dialog",
                    kind: "boolean",
                    required: false,
                    description: "true opens the dialog in the editor (saves nothing).",
                },
                ParamSchema {
                    name: "list",
                    kind: "boolean",
                    required: false,
                    description: "true answers the dirty documents (saves nothing).",
                },
                ParamSchema {
                    name: "cancel",
                    kind: "boolean",
                    required: false,
                    description: "true closes the open dialog as its Cancel (or, with nothing unsaved, Close) button does: nothing saved, a close or level switch does not go ahead. `cancelled` says whether one was open.",
                },
            ],
            result: "{ documents } (dialog / list), { cancelled, dialog: false } (cancel) or { saved, failed: [{id, error}], skipped, queued, documents, shell_results: [{id, error}] }",
            mutating: true,
            permission: Permission::Assets,
        },
        editor_save_all as Handler,
    )]
}

fn editor_save_all(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let documents =
        |world: &World| -> Vec<Value> { all_documents(world).iter().map(DirtyDoc::to_json).collect() };
    if params.get("list").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({ "documents": documents(world) }));
    }
    if params.get("cancel").and_then(Value::as_bool) == Some(true) {
        let open = world
            .get_resource::<SaveAllDialog>()
            .is_some_and(|dialog| dialog.purpose.is_some());
        if open {
            SaveAllDialog::choose(world, SaveDecision::Cancel, &[]);
        }
        return Ok(json!({ "cancelled": open, "dialog": false }));
    }
    if params.get("dialog").and_then(Value::as_bool) == Some(true) {
        open_dialog(world, SavePurpose::SaveAll);
        return Ok(json!({ "documents": documents(world), "dialog": true }));
    }
    let ids: Vec<String> = match params.get("ids") {
        None | Some(Value::Null) => all_documents(world)
            .into_iter()
            .filter(|doc| doc.can_save)
            .map(|doc| doc.id)
            .collect(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| RpcError::invalid_params("every entry of `ids` is a string"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(RpcError::invalid_params("`ids` is an array of document ids")),
    };
    let outcome = save_documents(world, &ids);
    let mut value = outcome.to_json();
    if let Some(object) = value.as_object_mut() {
        object.insert("documents".into(), json!(documents(world)));
        let shell_results: Vec<Value> = world
            .get_resource::<ShellDocuments>()
            .map(|docs| {
                docs.results
                    .iter()
                    .map(|(id, error)| json!({ "id": id, "error": error }))
                    .collect()
            })
            .unwrap_or_default();
        object.insert("shell_results".into(), json!(shell_results));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_keys::test_input::{frame, key};

    /// Bug 27 (ADR-8586): the verbatim prefix leaves displayed paths on every host; other text
    /// stays as it is.
    #[test]
    fn displayed_paths_drop_the_windows_verbatim_prefix() {
        assert_eq!(user_path_text(r"\\?\C:\x"), r"C:\x");
        assert_eq!(user_path_text(r"\\?\C:\work\proj\assets"), r"C:\work\proj\assets");
        assert_eq!(user_path_text(r"\\?\UNC\server\share\x"), r"\\server\share\x");
        assert_eq!(user_path_text("/home/me/proj"), "/home/me/proj");
        assert_eq!(user_path_text(r"\\server\share\x"), r"\\server\share\x");
        assert_eq!(user_path_text(r"\\?\Volume{1234}\x"), r"\\?\Volume{1234}\x");
        assert_eq!(user_path(Path::new(r"\\?\C:\x")), r"C:\x");
        // Outside the root the save list shows the person's path, not the canonical one.
        assert_eq!(
            display_path(None, Path::new(r"\\?\C:\x\level.level.ron")),
            r"C:\x\level.level.ron"
        );
    }

    /// Bug 19 (ADR-8586): a middle cut keeps the root and the file name and marks the cut.
    #[test]
    fn a_long_path_is_cut_in_the_middle() {
        assert_eq!(truncate_middle("short", 10), "short");
        assert_eq!(
            truncate_middle("/home/me/projects/game/assets", 11),
            "/home…ssets"
        );
        assert_eq!(truncate_middle("abcdef", 1), "…");
        assert_eq!(truncate_middle("abcdef", 5).chars().count(), 5);
    }

    /// Bug 19 (ADR-8586): in a narrow panel the path label fits its width and shows the cut; a short
    /// path draws whole.
    #[test]
    fn the_path_label_fits_the_panel() {
        let long = "/srv/work/projects/a-rather-long-project-folder-name/assets";
        let ctx = egui::Context::default();
        let clock = std::cell::Cell::new(0.0);
        let run = |pointer: Option<egui::Pos2>, text: &str| {
            clock.set(clock.get() + 0.25);
            let mut events = Vec::new();
            if let Some(at) = pointer {
                events.push(egui::Event::PointerMoved(at));
            }
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(200.0, 100.0),
                )),
                events,
                time: Some(clock.get()),
                ..Default::default()
            };
            let mut rect = egui::Rect::NOTHING;
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    rect = path_label(ui, text).rect;
                });
            });
            output.textures_delta.clear();
            let mut texts = Vec::new();
            for clipped in &output.shapes {
                if let egui::epaint::Shape::Text(t) = &clipped.shape {
                    texts.push((
                        t.galley.text().to_owned(),
                        t.galley.rect.translate(t.pos.to_vec2()),
                    ));
                }
            }
            (rect, texts)
        };
        let (rect, texts) = run(None, long);
        let (shown, at) = texts.first().cloned().expect("the label draws");
        assert!(
            shown.contains('…') && shown.starts_with("/media") && shown.ends_with("assets"),
            "{shown}"
        );
        assert!(at.max.x <= 200.0, "the cut text fits the panel: {at:?}");
        // The full path in the hover tooltip is not checked here: the headless frames never showed
        // egui's tooltip (ADR-8586 report, owed); `path_label` attaches it with `on_hover_text`.
        let _ = rect;
        let (_, texts) = run(None, "/short");
        assert_eq!(texts.first().map(|(t, _)| t.as_str()), Some("/short"));
    }

    /// An editor world with a project, an open untitled level and unsaved level edits.
    fn dirty_app(label: &str) -> (regia_app::App, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = crate::level_io::new_level(app.world_mut());
        let world = app.world_mut();
        world.insert_resource(open);
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world
            .get_resource_or_insert_with(crate::command::UndoStack::new)
            .mark_dirty();
        (app, root)
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods().into_iter().next().expect("editor.save_all");
        handler(world, &params)
    }

    #[test]
    fn the_dirty_level_is_listed_and_an_untitled_one_saves_into_assets() {
        let (mut app, root) = dirty_app("save_all_level");
        let world = app.world_mut();
        let docs = dirty_documents(world);
        assert_eq!(docs.len(), 1, "{docs:?}");
        assert_eq!(docs[0].id, LEVEL_ID);
        assert!(docs[0].can_save);
        assert_eq!(
            docs[0].note.as_deref(),
            Some("no file yet: saves as assets/unsaved.level.ron")
        );
        let listed = call(world, json!({ "list": true })).expect("list");
        assert_eq!(listed["documents"][0]["id"], "level");
        assert!(dirty_documents(world).len() == 1, "listing saves nothing");
        let out = call(world, json!({ "ids": ["level", "nope"] })).expect("save");
        assert_eq!(out["saved"], json!(["level"]), "{out}");
        assert_eq!(out["skipped"], json!(["nope"]));
        assert!(root.join("assets/unsaved.level.ron").is_file());
        assert!(
            dirty_documents(world).is_empty(),
            "the level is clean after the save"
        );
        assert_eq!(out["documents"], json!([]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_dialog_works_from_the_keyboard() {
        // Keyboard path (accessibility bar): the dialog opens with Save selected focused; Space on a
        // focused button presses it. Don't save closes it with nothing saved; Escape cancels.
        let (mut app, root) = dirty_app("save_all_keys");
        let world = app.world_mut();
        let ctx = egui::Context::default();
        let run = |world: &mut World, events: Vec<egui::Event>| {
            frame(&ctx, [800.0, 600.0], events, |ui| {
                dialog_window(ui.ctx(), world, &[])
            });
        };
        // The agent's dialog: true opens it.
        let out = call(world, json!({ "dialog": true })).expect("dialog");
        assert_eq!(out["dialog"], true);
        run(world, vec![]);
        run(world, vec![]);
        assert!(ctx.memory(|m| m.focused().is_some()), "a button has the focus");
        // Escape cancels and saves nothing.
        run(world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        let decision = world.resource_mut::<SaveAllDialog>().take_decision();
        assert_eq!(decision, Some((SavePurpose::SaveAll, SaveDecision::Cancel)));
        assert_eq!(dirty_documents(world).len(), 1);
        // Space on the focused Save selected saves the checked level.
        open_dialog(world, SavePurpose::Close);
        run(world, vec![]);
        run(world, vec![]);
        run(world, vec![key(egui::Key::Space, egui::Modifiers::NONE)]);
        let decision = world.resource_mut::<SaveAllDialog>().take_decision();
        assert_eq!(
            decision,
            Some((SavePurpose::Close, SaveDecision::Saved { extra: Vec::new() }))
        );
        assert!(dirty_documents(world).is_empty());
        assert!(root.join("assets/unsaved.level.ron").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unchecked_row_is_not_saved_and_extra_rows_go_back_to_the_caller() {
        let (mut app, root) = dirty_app("save_all_unchecked");
        let world = app.world_mut();
        open_dialog(world, SavePurpose::SaveAll);
        world.resource_mut::<SaveAllDialog>().set_checked(LEVEL_ID, false);
        let extra = [DirtyDoc {
            id: "shell:settings".into(),
            kind: "project_settings".into(),
            label: "project settings (Regia.toml)".into(),
            path: Some("Regia.toml".into()),
            can_save: true,
            note: None,
        }];
        SaveAllDialog::choose(world, SaveDecision::Saved { extra: Vec::new() }, &extra);
        let decision = world.resource_mut::<SaveAllDialog>().take_decision();
        assert_eq!(
            decision,
            Some((
                SavePurpose::SaveAll,
                SaveDecision::Saved {
                    extra: vec!["shell:settings".to_string()]
                }
            ))
        );
        assert_eq!(
            dirty_documents(world).len(),
            1,
            "the unchecked level stays unsaved"
        );
        assert!(!root.join("assets/unsaved.level.ron").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
    /// The texts the dialog draws (the second of two frames: a new egui window's first frame is
    /// its invisible sizing pass).
    fn dialog_texts(ctx: &egui::Context, world: &mut World) -> Vec<String> {
        let mut output = ctx.run_ui(
            crate::canvas_keys::test_input::raw([800.0, 600.0], Vec::new()),
            |ui| dialog_window(ui.ctx(), world, &[]),
        );
        output.textures_delta.clear();
        let mut output = ctx.run_ui(
            crate::canvas_keys::test_input::raw([800.0, 600.0], Vec::new()),
            |ui| dialog_window(ui.ctx(), world, &[]),
        );
        output.textures_delta.clear();
        fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }
        texts
    }

    /// The editor's registered handler of `method` (the agent dispatcher's table).
    fn dispatch(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("registered");
        handler(world, &params)
    }

    /// Lane v06-editor-undo-agent-routes (ADR-8583, smoke bug 13): an agent that opened the dialog
    /// with `dialog: true` closes it with `cancel: true` (the Cancel button: nothing saved, the
    /// caller gets `SaveDecision::Cancel`); with nothing unsaved the dialog says so with one Close
    /// button instead of the three choices.
    #[test]
    fn agent_can_cancel_the_save_all_dialog() {
        let (mut app, root) = dirty_app("save_all_agent_cancel");
        let world = app.world_mut();
        let ctx = egui::Context::default();
        let out = dispatch(world, "editor.save_all", json!({ "dialog": true })).expect("dialog");
        assert_eq!(out["dialog"], true);
        assert!(dialog_texts(&ctx, world).iter().any(|t| t == "Save selected"));
        let out = dispatch(world, "editor.save_all", json!({ "cancel": true })).expect("cancel");
        assert_eq!(out["cancelled"], true, "{out}");
        assert_eq!(out["dialog"], false);
        assert!(world.resource::<SaveAllDialog>().purpose.is_none(), "closed");
        assert_eq!(
            world.resource_mut::<SaveAllDialog>().take_decision(),
            Some((SavePurpose::SaveAll, SaveDecision::Cancel))
        );
        assert_eq!(dirty_documents(world).len(), 1, "nothing saved");
        assert!(dialog_texts(&ctx, world).iter().all(|t| t != "Save selected"));
        // Nothing open: answered, nothing happens.
        let out = dispatch(world, "editor.save_all", json!({ "cancel": true })).expect("again");
        assert_eq!(out["cancelled"], false);
        assert_eq!(dirty_documents(world).len(), 1);
        // The unsaved-changes prompt of a close is cancelled the same way (the editor stays).
        open_dialog(world, SavePurpose::Close);
        dispatch(world, "editor.save_all", json!({ "cancel": true })).expect("cancel close");
        assert_eq!(
            world.resource_mut::<SaveAllDialog>().take_decision(),
            Some((SavePurpose::Close, SaveDecision::Cancel))
        );
        // Nothing unsaved: one sentence and a Close button, no Save selected / Don't save.
        dispatch(world, "editor.save_all", json!({})).expect("save everything");
        assert!(dirty_documents(world).is_empty());
        dispatch(world, "editor.save_all", json!({ "dialog": true })).expect("dialog");
        let texts = dialog_texts(&ctx, world);
        assert!(texts.iter().any(|t| t == "Every document is saved."), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Close"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t == "Save selected" || t == "Don't save"),
            "{texts:?}"
        );
        let out = dispatch(world, "editor.save_all", json!({ "cancel": true })).expect("close");
        assert_eq!(out["cancelled"], true);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod kinds_tests;
