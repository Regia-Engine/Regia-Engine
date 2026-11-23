// SPDX-License-Identifier: MPL-2.0
//! Content Browser actions over several assets (`docs/specs/19-editor.md` § 2, ADR-7822): the
//! selection list, bulk actions on it, retargeting references (a missing path, or a delete with
//! "replace references with"), and the rich hover details of a row or tile.
//!
//! The model here is shared by the panel (`crate::content`) and the agent methods
//! `editor.content_select`, `editor.bulk_asset_action` and `editor.retarget_references`
//! (`book/agents/content-browser.md`), so a person and an agent change the same state.
//!
//! - [`ContentSelection`]: an ordered list of project-relative paths with an anchor (the last
//!   plain or Ctrl click) and a focus (the keyboard cursor). Ctrl toggles, Shift selects the range
//!   between anchor and the clicked path in the order the panel lists the assets, arrow keys move
//!   the focus and Shift+arrows extend the range.
//! - [`retarget`]: rewrites every reference to one path (string literals that name the path, with
//!   the move fix-up's rule) and, when both assets have a `.meta`, every mention of the old asset
//!   id, onto another asset of the same kind. One undo step ([`RewriteFiles`]).
//! - [`delete_with_replace`]: [`retarget`] and then the delete, one undo step that also puts the
//!   deleted file and its `.meta` back.

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::Handler;

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// How a click changes the selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClickMods {
    /// Ctrl (Cmd on macOS): toggle one path, or add a range with Shift.
    pub toggle: bool,
    /// Shift: select the range from the anchor.
    pub range: bool,
}

impl ClickMods {
    /// The modifiers of an egui click.
    #[must_use]
    pub fn from_egui(modifiers: egui::Modifiers) -> Self {
        Self {
            toggle: modifiers.command,
            range: modifiers.shift,
        }
    }
}

/// The Content Browser's selection: a list of project-relative paths (ADR-7822). An edit-world
/// resource, so the panel and `editor.content_select` share it.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentSelection {
    paths: Vec<String>,
    /// Where a Shift range starts: the last plain or Ctrl click.
    anchor: Option<String>,
    /// The keyboard cursor (the last clicked or stepped-to path).
    focus: Option<String>,
}

impl ContentSelection {
    /// The selected paths, in the order they were selected.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Whether `rel` is selected.
    #[must_use]
    pub fn contains(&self, rel: &str) -> bool {
        self.paths.iter().any(|p| p == rel)
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The keyboard cursor.
    #[must_use]
    pub fn focus(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// Replaces the selection (duplicates dropped, order kept); the last path becomes the anchor
    /// and the focus.
    pub fn set(&mut self, paths: impl IntoIterator<Item = String>) {
        self.paths.clear();
        for path in paths {
            if !self.contains(&path) {
                self.paths.push(path);
            }
        }
        self.anchor = self.paths.last().cloned();
        self.focus = self.anchor.clone();
    }

    /// Adds paths that are not selected yet.
    pub fn add(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            if !self.contains(&path) {
                self.focus = Some(path.clone());
                self.paths.push(path);
            }
        }
    }

    /// Removes paths.
    pub fn remove(&mut self, paths: &[String]) {
        self.paths.retain(|p| !paths.contains(p));
        if self.anchor.as_ref().is_some_and(|a| paths.contains(a)) {
            self.anchor = self.paths.last().cloned();
        }
        if self.focus.as_ref().is_some_and(|f| paths.contains(f)) {
            self.focus = self.paths.last().cloned();
        }
    }

    /// Empties the selection.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// A click on `rel`, with `listed` the paths in the order the panel shows them (the range
    /// order).
    pub fn click(&mut self, rel: &str, mods: ClickMods, listed: &[&str]) {
        if mods.range {
            let range = range_between(listed, self.anchor.as_deref().unwrap_or(rel), rel);
            if !mods.toggle {
                self.paths.clear();
            }
            for path in range {
                if !self.contains(path) {
                    self.paths.push(path.to_string());
                }
            }
            if self.anchor.is_none() {
                self.anchor = Some(rel.to_string());
            }
        } else if mods.toggle {
            if self.contains(rel) {
                self.paths.retain(|p| p != rel);
            } else {
                self.paths.push(rel.to_string());
            }
            self.anchor = Some(rel.to_string());
        } else {
            self.paths = vec![rel.to_string()];
            self.anchor = Some(rel.to_string());
        }
        self.focus = Some(rel.to_string());
    }

    /// Moves the keyboard cursor `delta` places through `listed` (clamped at the ends; with no
    /// cursor the first or last listed path). Without `extend` the cursor's path alone is
    /// selected; with it (Shift) the range from the anchor to the cursor. Returns the new cursor.
    pub fn step(&mut self, delta: isize, extend: bool, listed: &[&str]) -> Option<String> {
        if listed.is_empty() {
            return None;
        }
        let last = listed.len() - 1;
        let at = self
            .focus
            .as_deref()
            .and_then(|focus| listed.iter().position(|p| *p == focus));
        let target = match at {
            Some(at) => at.saturating_add_signed(delta).min(last),
            None if delta < 0 => last,
            None => 0,
        };
        let path = listed[target].to_string();
        if extend {
            let anchor = self.anchor.clone().unwrap_or_else(|| path.clone());
            self.paths = range_between(listed, &anchor, &path)
                .into_iter()
                .map(str::to_string)
                .collect();
            self.anchor = Some(anchor);
            self.focus = Some(path.clone());
        } else {
            self.click(&path, ClickMods::default(), listed);
        }
        Some(path)
    }

    /// Selects every listed path (Ctrl+A).
    pub fn select_all(&mut self, listed: &[&str]) {
        self.paths = listed.iter().map(|p| (*p).to_string()).collect();
        self.anchor = self.paths.first().cloned();
        self.focus = self.paths.last().cloned();
    }

    /// The paths a box drag covers: `hits` (in listed order) replace the selection, or are added
    /// to `before` with Ctrl.
    pub fn box_select(&mut self, before: &[String], hits: &[String], additive: bool) {
        let mut paths = if additive { before.to_vec() } else { Vec::new() };
        for hit in hits {
            if !paths.contains(hit) {
                paths.push(hit.clone());
            }
        }
        self.paths = paths;
        self.anchor = hits.first().cloned().or_else(|| self.anchor.clone());
        self.focus = hits.last().cloned().or_else(|| self.focus.clone());
    }

    /// Renames a selected path after a move (the selection follows the asset).
    pub fn rename(&mut self, from: &str, to: &str) {
        for path in [&mut self.anchor, &mut self.focus].into_iter().flatten() {
            if path == from {
                *path = to.to_string();
            }
        }
        for path in &mut self.paths {
            if path == from {
                *path = to.to_string();
            }
        }
    }
}

/// The listed paths from `a` to `b` inclusive, in listed order (just `b` when either is not
/// listed).
fn range_between<'a>(listed: &[&'a str], a: &str, b: &str) -> Vec<&'a str> {
    let (Some(i), Some(j)) = (
        listed.iter().position(|p| *p == a),
        listed.iter().position(|p| *p == b),
    ) else {
        return listed.iter().filter(|p| **p == b).copied().collect();
    };
    let (lo, hi) = if i <= j { (i, j) } else { (j, i) };
    listed[lo..=hi].to_vec()
}

/// The selection keys over the browser's list or grid: Up/Down (a row; a grid row is `columns`
/// places) and Left/Right (one place, grid only), with Shift to extend; Ctrl+A selects every
/// listed asset; Escape clears. `active` is whether the browser has the keyboard (the pointer is
/// over it and no text field has focus). Returns the new cursor when a key moved it. One function,
/// so a shared grid-keys handler can take it over.
pub fn selection_keys(
    ctx: &egui::Context,
    selection: &mut ContentSelection,
    listed: &[&str],
    columns: usize,
    active: bool,
) -> Option<String> {
    if !active || ctx.text_edit_focused() {
        return None;
    }
    let columns = columns.max(1) as isize;
    let mut moved = None;
    ctx.input_mut(|input| {
        for (key, delta) in [
            (egui::Key::ArrowUp, -columns),
            (egui::Key::ArrowDown, columns),
            (egui::Key::ArrowLeft, -1),
            (egui::Key::ArrowRight, 1),
        ] {
            // Left/Right are the folder history's with Alt; in the list they do nothing.
            if columns == 1 && matches!(key, egui::Key::ArrowLeft | egui::Key::ArrowRight) {
                continue;
            }
            if consume_exact(input, egui::Modifiers::SHIFT, key) {
                moved = selection.step(delta, true, listed);
            } else if consume_exact(input, egui::Modifiers::NONE, key) {
                moved = selection.step(delta, false, listed);
            }
        }
        if consume_exact(input, egui::Modifiers::COMMAND, egui::Key::A) {
            selection.select_all(listed);
        }
        if !selection.is_empty() && consume_exact(input, egui::Modifiers::NONE, egui::Key::Escape) {
            selection.clear();
        }
    });
    moved
}

/// Consumes one press of `key` whose modifiers are exactly `modifiers` (Ctrl and Cmd both count
/// as `COMMAND`; Shift or Alt on top do not match).
fn consume_exact(input: &mut egui::InputState, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    let mut hit = false;
    input.events.retain(|event| {
        let matches = !hit
            && matches!(event, egui::Event::Key { key: k, pressed: true, modifiers: m, .. }
                if *k == key && m.matches_exact(modifiers));
        hit |= matches;
        !matches
    });
    hit
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a content action was refused: a stable code (the agent's `data.reason`, listed in
/// `book/agents/content-browser.md`) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    /// Lowercase ascii, digits and `_`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl ActionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent's refusal.
    #[must_use]
    pub fn to_rpc(&self) -> RpcError {
        crate::asset_ops_agent::reasoned(self.code, self.message.clone())
    }
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

// ---------------------------------------------------------------------------
// Retarget and delete with replace
// ---------------------------------------------------------------------------

/// `rel` as the editor lists it: `assets/<path>`, `/`-separated.
fn project_rel(rel: &str) -> String {
    let rel = rel.trim().replace('\\', "/");
    match rel.strip_prefix("assets/") {
        Some(_) => rel,
        None => format!("assets/{rel}"),
    }
}

/// A path's kind for "same kind" checks: the search type (`mesh`, `texture`, ...) and, for
/// `other`, the compound suffix (`.anim.ron`, `.cue.ron`, ...), so a sound cue never replaces a
/// sequence.
#[must_use]
pub fn kind_key(rel: &str) -> String {
    let kind = crate::asset_search::asset_type(rel);
    if kind != "other" {
        return kind.to_string();
    }
    let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
    match name.split_once('.') {
        Some((_, suffix)) => format!(".{suffix}"),
        None => String::new(),
    }
}

/// The asset id in `rel`'s `.meta`.
fn meta_asset_id(root: &Path, rel: &str) -> Option<regia_asset::AssetId> {
    let full = root.join(project_rel(rel));
    std::fs::read(regia_asset::meta_path_for(&full))
        .ok()
        .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
        .map(|meta| meta.id)
}

/// The asset id in `rel`'s `.meta`, as text.
fn meta_id(root: &Path, rel: &str) -> Option<String> {
    meta_asset_id(root, rel).map(|id| id.to_string())
}

/// Every `.ron` text asset under `assets/` (hidden entries, `.meta` sidecars and symlinks
/// skipped), sorted.
fn ron_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() && name.ends_with(".ron") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

/// One file a retarget rewrites: its bytes before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRewrite {
    /// The file on disk.
    pub full: PathBuf,
    /// Project-relative, `/`-separated.
    pub rel: String,
    before: Vec<u8>,
    after: Vec<u8>,
}

/// The rewrites that point every reference to `from` at `to` (both project-relative): string
/// literals naming `from`'s path (the move fix-up's rule, `regia_asset::project_ops`), and the
/// text of `from`'s asset id when both have a `.meta`. Neither file itself is rewritten.
#[must_use]
pub fn plan_retarget(root: &Path, from: &str, to: &str) -> Vec<FileRewrite> {
    let from = project_rel(from);
    let to = project_rel(to);
    let ids = meta_id(root, &from).zip(meta_id(root, &to));
    let mut out = Vec::new();
    for full in ron_files(root) {
        let rel = crate::content::slash_path(full.strip_prefix(root).unwrap_or(&full));
        if rel == from || rel == to {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let mut next = regia_asset::project_ops::rewrite_path_references(&text, &from, &to);
        if let Some((old_id, new_id)) = &ids {
            let current = next.as_deref().unwrap_or(&text);
            if current.contains(old_id.as_str()) {
                next = Some(current.replace(old_id.as_str(), new_id));
            }
        }
        if let Some(after) = next {
            out.push(FileRewrite {
                full,
                rel,
                before: text.into_bytes(),
                after: after.into_bytes(),
            });
        }
    }
    out
}

/// Writes `bytes` to `path` atomically (temp file + rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.regia-retarget.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Writes each file's `after` (or `before` with `back`), newest-first rollback on a failure.
fn write_all(files: &[FileRewrite], back: bool) -> Result<(), ActionError> {
    for (done, file) in files.iter().enumerate() {
        let bytes = if back { &file.before } else { &file.after };
        if let Err(e) = write_atomic(&file.full, bytes) {
            for undo in files[..done].iter().rev() {
                let bytes = if back { &undo.after } else { &undo.before };
                let _ = write_atomic(&undo.full, bytes);
            }
            return Err(ActionError::new(
                "retarget_write_failed",
                format!("cannot write {}: {e} (nothing changed)", file.rel),
            ));
        }
    }
    Ok(())
}

/// A deleted file and its `.meta`, kept so undo can put them back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeletedFile {
    full: PathBuf,
    bytes: Vec<u8>,
    meta: Option<Vec<u8>>,
}

impl DeletedFile {
    fn delete(&self) -> std::io::Result<()> {
        if self.meta.is_some() {
            let _ = std::fs::remove_file(regia_asset::meta_path_for(&self.full));
        }
        std::fs::remove_file(&self.full)
    }

    fn restore(&self) -> std::io::Result<()> {
        if let Some(parent) = self.full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&self.full, &self.bytes)?;
        if let Some(meta) = &self.meta {
            write_atomic(&regia_asset::meta_path_for(&self.full), meta)?;
        }
        Ok(())
    }
}

/// The undo entry of a retarget (and of a delete with replace): file-only, so it never marks
/// the level dirty.
pub struct RewriteFiles {
    label: String,
    files: Vec<FileRewrite>,
    deleted: Option<DeletedFile>,
    applied: bool,
}

impl EditorCommand for RewriteFiles {
    fn apply(&mut self, world: &mut World) {
        if self.applied {
            return;
        }
        if let Err(e) = write_all(&self.files, false) {
            tracing::warn!(target: "regia_editor", "redo {}: {e}", self.label);
        }
        if let Some(deleted) = &self.deleted
            && let Err(e) = deleted.delete()
        {
            tracing::warn!(target: "regia_editor", "redo {}: {e}", self.label);
        }
        self.applied = true;
        crate::content_index::invalidate(world);
    }

    fn revert(&mut self, world: &mut World) {
        if !self.applied {
            return;
        }
        if let Some(deleted) = &self.deleted
            && let Err(e) = deleted.restore()
        {
            tracing::warn!(target: "regia_editor", "undo {}: {e}", self.label);
        }
        if let Err(e) = write_all(&self.files, true) {
            tracing::warn!(target: "regia_editor", "undo {}: {e}", self.label);
        }
        self.applied = false;
        crate::content_index::invalidate(world);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn changes_level_state(&self) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// What a [`retarget`] or [`delete_with_replace`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetargetOutcome {
    /// The path whose references moved (project-relative).
    pub from: String,
    /// The asset they point at now.
    pub to: String,
    /// The rewritten files (project-relative), sorted.
    pub rewritten: Vec<String>,
    /// Whether `from` was deleted (a delete with replace).
    pub deleted: bool,
}

impl RetargetOutcome {
    /// The agent's answer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "from": self.from,
            "to": self.to,
            "rewritten": self.rewritten,
            "deleted": self.deleted,
        })
    }
}

/// Checks a retarget's two paths: both inside `assets/`, `to` an existing file of `from`'s kind.
fn check_pair(root: &Path, from: &str, to: &str) -> Result<(String, String), ActionError> {
    for (key, path) in [("from", from), ("to", to)] {
        let bare = path.trim().replace('\\', "/");
        let bare = bare.strip_prefix("assets/").unwrap_or(&bare).to_string();
        if bare.is_empty() || regia_core::project_path::check_relative(&bare).is_err() {
            return Err(ActionError::new(
                "path_outside_project",
                format!("`{key}` {path:?} is not a path under assets/"),
            ));
        }
    }
    let (from, to) = (project_rel(from), project_rel(to));
    if from == to {
        return Err(ActionError::new(
            "retarget_same_asset",
            format!("{from} cannot replace itself"),
        ));
    }
    if !root.join(&to).is_file() {
        return Err(ActionError::new(
            "retarget_target_missing",
            format!("{to} is not a file in the project"),
        ));
    }
    if kind_key(&from) != kind_key(&to) {
        return Err(ActionError::new(
            "retarget_kind_mismatch",
            format!(
                "{to} ({}) is not the same kind as {from} ({})",
                kind_key(&to),
                kind_key(&from)
            ),
        ));
    }
    Ok((from, to))
}

/// Points every reference to `from` (a missing path, or any asset) at `to`, an existing asset of
/// the same kind, and pushes one undo step. Nothing is deleted.
///
/// # Errors
/// `path_outside_project`, `retarget_same_asset`, `retarget_target_missing`,
/// `retarget_kind_mismatch`, `retarget_nothing` (no file references `from`) or
/// `retarget_write_failed` (rolled back).
pub fn retarget(
    world: &mut World,
    root: &Path,
    from: &str,
    to: &str,
) -> Result<RetargetOutcome, ActionError> {
    let (from, to) = check_pair(root, from, to)?;
    let files = plan_retarget(root, &from, &to);
    if files.is_empty() {
        return Err(ActionError::new(
            "retarget_nothing",
            format!("no project file references {from}"),
        ));
    }
    write_all(&files, false)?;
    let rewritten: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
    push_undo(
        world,
        RewriteFiles {
            label: format!("Retarget {from} to {to}"),
            files,
            deleted: None,
            applied: true,
        },
    );
    crate::content_index::invalidate(world);
    Ok(RetargetOutcome {
        from,
        to,
        rewritten,
        deleted: false,
    })
}

/// Deletes `rel` after pointing every reference to it at `replace_with` (an asset of the same
/// kind), as one undo step that puts the file, its `.meta` and the references back. A folder
/// cannot be replaced.
///
/// # Errors
/// The [`retarget`] refusals (except `retarget_nothing`: an unreferenced asset is just deleted),
/// `asset_missing`, `asset_replace_folder`, or `asset_delete_failed` (the references are put
/// back).
pub fn delete_with_replace(
    world: &mut World,
    root: &Path,
    rel: &str,
    replace_with: &str,
) -> Result<RetargetOutcome, ActionError> {
    let (from, to) = check_pair(root, rel, replace_with)?;
    let full = root.join(&from);
    if full.is_dir() {
        return Err(ActionError::new(
            "asset_replace_folder",
            format!("{from} is a folder: only a file can be replaced"),
        ));
    }
    let bytes = std::fs::read(&full)
        .map_err(|e| ActionError::new("asset_missing", format!("cannot read {from}: {e}")))?;
    let meta = std::fs::read(regia_asset::meta_path_for(&full)).ok();
    let files = plan_retarget(root, &from, &to);
    write_all(&files, false)?;
    let deleted = DeletedFile { full, bytes, meta };
    if let Err(e) = deleted.delete() {
        let _ = deleted.restore();
        let _ = write_all(&files, true);
        return Err(ActionError::new(
            "asset_delete_failed",
            format!("cannot delete {from}: {e} (references put back)"),
        ));
    }
    let rewritten: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
    push_undo(
        world,
        RewriteFiles {
            label: format!("Delete {from}, references to {to}"),
            files,
            deleted: Some(deleted),
            applied: true,
        },
    );
    crate::content_index::invalidate(world);
    Ok(RetargetOutcome {
        from,
        to,
        rewritten,
        deleted: true,
    })
}

fn push_undo(world: &mut World, command: impl EditorCommand + 'static) {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.seal();
    stack.push(Box::new(command));
    stack.seal();
}

/// Assets named like `missing` (the same file name, any folder) and of its kind: the "Find
/// similar" suggestions for a missing reference, sorted.
#[must_use]
pub fn find_similar(root: &Path, missing: &str) -> Vec<String> {
    let missing = project_rel(missing);
    let name = missing
        .rsplit('/')
        .next()
        .unwrap_or(&missing)
        .to_ascii_lowercase();
    let kind = kind_key(&missing);
    let mut out = Vec::new();
    let mut stack = vec![root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file.starts_with('.') {
                continue;
            }
            let Ok(kind_of) = entry.file_type() else {
                continue;
            };
            if kind_of.is_dir() {
                stack.push(entry.path());
            } else if kind_of.is_file() && file.to_ascii_lowercase() == name {
                let path = entry.path();
                let rel = crate::content::slash_path(path.strip_prefix(root).unwrap_or(&path));
                if rel != missing && kind_key(&rel) == kind {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    out
}

/// The path a missing-reference finding names (`references \`x\`, which is not ...`).
#[must_use]
pub fn missing_path_of(text: &str) -> Option<&str> {
    let start = text.find('`')? + 1;
    let len = text[start..].find('`')?;
    Some(&text[start..start + len]).filter(|p| !p.is_empty())
}

// ---------------------------------------------------------------------------
// Bulk actions
// ---------------------------------------------------------------------------

/// An action over the selection (`editor.bulk_asset_action { action }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkAction {
    /// Run the validators over the assets (the Problems panel shows the findings).
    Validate,
    /// Re-cook each source with its cache ignored.
    Reimport,
    /// Save the open documents among them that have unsaved edits.
    Save,
    /// Move each into this folder (project-relative), references rewritten.
    MoveTo(String),
    /// Delete them all (needs confirmation; referencers are reported).
    Delete {
        /// Delete; otherwise answer the referencers and the confirmation refusal.
        confirm: bool,
    },
    /// Add them to a static collection.
    AddToCollection(String),
}

impl BulkAction {
    /// The agent's names of the actions.
    pub const NAMES: [&'static str; 6] = [
        "validate",
        "reimport",
        "save",
        "move",
        "delete",
        "add_to_collection",
    ];

    /// Parses the agent's parameters.
    ///
    /// # Errors
    /// `bulk_unknown_action`, `bulk_missing_param`.
    pub fn from_params(params: &Value) -> Result<Self, ActionError> {
        let name = params.get("action").and_then(Value::as_str).unwrap_or_default();
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| ActionError::new("bulk_missing_param", format!("action {name} needs `{key}`")))
        };
        Ok(match name {
            "validate" => Self::Validate,
            "reimport" => Self::Reimport,
            "save" => Self::Save,
            "move" => Self::MoveTo(text("to")?),
            "delete" => Self::Delete {
                confirm: params.get("confirm").and_then(Value::as_bool) == Some(true),
            },
            "add_to_collection" => Self::AddToCollection(text("collection")?),
            other => {
                return Err(ActionError::new(
                    "bulk_unknown_action",
                    format!("unknown action {other:?}: one of {}", Self::NAMES.join(", ")),
                ));
            }
        })
    }
}

/// One path's result in a bulk action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkRow {
    /// The path.
    pub path: String,
    /// Whether it went through.
    pub ok: bool,
    /// What happened (the new path of a move, an error, ...).
    pub note: String,
}

/// What a bulk action did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BulkOutcome {
    /// One row per path (in the given order).
    pub rows: Vec<BulkRow>,
    /// The panel's toast line.
    pub summary: String,
    /// Files that reference the deleted paths (a delete), sorted.
    pub referencers: Vec<String>,
}

impl BulkOutcome {
    /// The agent's answer.
    #[must_use]
    pub fn to_json(&self, action: &str) -> Value {
        json!({
            "action": action,
            "summary": self.summary,
            "referencers": self.referencers,
            "results": self.rows.iter().map(|row| json!({
                "path": row.path,
                "ok": row.ok,
                "note": row.note,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Every file that references one of `paths` (other than the paths themselves), sorted: the one
/// list a bulk delete shows.
#[must_use]
pub fn referencers_of_all(root: &Path, paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .flat_map(|rel| crate::content::find_referencers(root, &project_rel(rel)))
        .filter(|found| !paths.iter().any(|p| project_rel(p) == *found))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Runs `action` over `paths` (project-relative, as the panel lists them). Moves push one undo
/// step each; a delete is refused with `needs_confirmation` until confirmed.
///
/// # Errors
/// `bulk_empty`, `needs_confirmation` (a delete without `confirm`; the referencers are in the
/// error's message and `referencers_of_all`).
pub fn run_bulk(
    world: &mut World,
    root: &Path,
    paths: &[String],
    action: &BulkAction,
) -> Result<BulkOutcome, ActionError> {
    let _span = tracing::info_span!(target: "regia_editor", "content_bulk_action").entered();
    if paths.is_empty() {
        return Err(ActionError::new("bulk_empty", "no assets are selected"));
    }
    let paths: Vec<String> = paths.iter().map(|p| project_rel(p)).collect();
    let mut out = BulkOutcome::default();
    let row = |path: &str, result: Result<String, String>| match result {
        Ok(note) => BulkRow {
            path: path.to_string(),
            ok: true,
            note,
        },
        Err(note) => BulkRow {
            path: path.to_string(),
            ok: false,
            note,
        },
    };
    match action {
        BulkAction::Validate => {
            out.summary = crate::asset_panels::validate_paths(world, &paths);
            out.rows = paths.iter().map(|p| row(p, Ok("validated".into()))).collect();
        }
        BulkAction::Reimport => {
            for path in &paths {
                let result = crate::asset_ops_agent::recook_source(root, &root.join(path))
                    .map(|report| format!("cooked {} (up-to-date {})", report.cooked, report.skipped))
                    .map_err(|e| e.to_string());
                out.rows.push(row(path, result));
            }
            crate::content_index::invalidate(world);
        }
        BulkAction::Save => {
            let docs = crate::save_all_dialog::dirty_documents(world);
            let ids: Vec<String> = docs
                .iter()
                .filter(|doc| doc.path.as_ref().is_some_and(|p| paths.contains(p)))
                .map(|doc| doc.id.clone())
                .collect();
            let saved = crate::save_all_dialog::save_documents(world, &ids);
            for path in &paths {
                let doc = docs.iter().find(|doc| doc.path.as_deref() == Some(path.as_str()));
                out.rows.push(match doc {
                    None => row(path, Ok("no unsaved edits".into())),
                    Some(doc) => match saved.failed.iter().find(|(id, _)| *id == doc.id) {
                        Some((_, err)) => row(path, Err(err.clone())),
                        None => row(path, Ok("saved".into())),
                    },
                });
            }
        }
        BulkAction::MoveTo(folder) => {
            let folder = project_rel(folder.trim_end_matches('/'));
            let mut moved = Vec::new();
            for path in &paths {
                let name = path.rsplit('/').next().unwrap_or(path);
                let target = format!("{folder}/{name}");
                if target == *path {
                    out.rows.push(row(path, Ok("already there".into())));
                    continue;
                }
                let result = regia_asset::project_ops::move_asset(root, path, &target)
                    .map(|outcome| {
                        crate::content::push_move_undo(
                            world,
                            root.to_path_buf(),
                            path.clone(),
                            target.clone(),
                        );
                        moved.push((path.clone(), target.clone()));
                        format!(
                            "{target} ({} reference(s) updated)",
                            outcome.fixed_referencers.len()
                        )
                    })
                    .map_err(|e| e.to_string());
                out.rows.push(row(path, result));
            }
            let mut selection = world.get_resource_or_insert_with(ContentSelection::default);
            for (from, to) in &moved {
                selection.rename(from, to);
            }
            crate::content_index::invalidate(world);
        }
        BulkAction::Delete { confirm } => {
            out.referencers = referencers_of_all(root, &paths);
            if !confirm {
                return Err(ActionError::new(
                    "needs_confirmation",
                    format!(
                        "deleting {} asset(s) needs the owner's confirmation ({} referencing file(s)): repeat with `confirm: true`",
                        paths.len(),
                        out.referencers.len()
                    ),
                ));
            }
            for path in &paths {
                let result = crate::content::delete_asset_force(root, path).map(|()| "deleted".to_string());
                out.rows.push(row(path, result));
            }
            world
                .get_resource_or_insert_with(ContentSelection::default)
                .remove(&paths);
            crate::content_index::invalidate(world);
        }
        BulkAction::AddToCollection(name) => {
            out.summary = crate::content_collections::add_paths(world, name, &paths);
            out.rows = paths.iter().map(|p| row(p, Ok(out.summary.clone()))).collect();
        }
    }
    if out.summary.is_empty() {
        let failed = out.rows.iter().filter(|r| !r.ok).count();
        out.summary = format!("{} of {} done", out.rows.len() - failed, out.rows.len());
        if let Some(first) = out.rows.iter().find(|r| !r.ok) {
            out.summary = format!("{}; {}: {}", out.summary, first.path, first.note);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Hover details
// ---------------------------------------------------------------------------

/// A revision-control state a VCS provider can show in the tooltip (an edit-world resource:
/// project-relative path → state text such as "modified" or "locked by ..."). The revision-control
/// status poll fills it once per answer (`crate::vcs`); a key ending in `/` stands for a whole
/// untracked or ignored directory.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetRevisionStates(pub std::collections::BTreeMap<String, String>);

impl AssetRevisionStates {
    /// The state text of `rel`: its own entry, else the entry of a directory above it.
    #[must_use]
    pub fn state_of(&self, rel: &str) -> Option<&str> {
        if let Some(state) = self.0.get(rel) {
            return Some(state);
        }
        let mut end = 0;
        while let Some(slash) = rel[end..].find('/') {
            end += slash + 1;
            if let Some(state) = self.0.get(&rel[..end]) {
                return Some(state);
            }
        }
        None
    }
}

/// What the tooltip and `editor.asset_details` show as the revision state of `rel`: the poll's
/// state, `unchanged` for a path a ready revision-control status does not list, and `None`
/// (shown as "not tracked") without revision control.
#[must_use]
pub fn revision_of(world: &World, rel: &str) -> Option<String> {
    if let Some(state) = world
        .get_resource::<AssetRevisionStates>()
        .and_then(|states| states.state_of(rel))
    {
        return Some(state.to_string());
    }
    crate::vcs::cached(world).map(|_| "unchanged".to_string())
}

/// What the rich tooltip of a row or tile shows (ADR-7822).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetDetails {
    /// The search type (`mesh`, `texture`, ...).
    pub kind: String,
    /// The source file's size in bytes.
    pub file_bytes: Option<u64>,
    /// The cooked artifact's size in bytes (when cooked for this platform).
    pub cooked_bytes: Option<u64>,
    /// Triangles of a glTF mesh (all primitives).
    pub triangles: Option<u64>,
    /// Width and height of a texture.
    pub dimensions: Option<(u32, u32)>,
    /// Length of a WAV sound, milliseconds.
    pub duration_ms: Option<u64>,
    /// The source file's modification time (RFC 3339, UTC).
    pub modified: Option<String>,
    /// Files that reference it.
    pub referencers: usize,
    /// The revision-control state, when a provider set one.
    pub revision: Option<String>,
}

impl AssetDetails {
    /// Reads the details of `rel` (project-relative) from disk.
    #[must_use]
    pub fn read(root: &Path, rel: &str) -> Self {
        let _span = tracing::info_span!(target: "regia_editor", "content_asset_details").entered();
        let rel = project_rel(rel);
        let full = root.join(&rel);
        let metadata = std::fs::metadata(&full).ok();
        let kind = crate::asset_search::asset_type(&rel).to_string();
        let cooked_bytes = meta_asset_id(root, &rel).and_then(|id| {
            let dir = regia_asset::cooked_cache_dir(root, regia_asset_pipeline::host_platform());
            std::fs::metadata(dir.join(regia_asset::cooked_file_name(id)))
                .ok()
                .map(|m| m.len())
        });
        let (triangles, dimensions, duration_ms) = match kind.as_str() {
            "mesh" => (gltf_triangles(&full), None, None),
            "texture" => (None, image::image_dimensions(&full).ok(), None),
            "audio" => (None, None, wav_duration_ms(&full)),
            _ => (None, None, None),
        };
        let modified = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| crate::content::rfc3339_from_unix(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)));
        let referencers = crate::content::find_referencers(root, &rel)
            .into_iter()
            .filter(|found| *found != rel)
            .count();
        Self {
            kind,
            file_bytes: metadata.map(|m| m.len()),
            cooked_bytes,
            triangles,
            dimensions,
            duration_ms,
            modified,
            referencers,
            revision: None,
        }
    }

    /// The tooltip lines (provenance is added by the panel, which already has it).
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let size =
            |bytes: Option<u64>| bytes.map_or_else(|| "–".to_string(), crate::asset_panels::format_bytes);
        let mut lines = vec![format!("Kind: {}", self.kind)];
        lines.push(format!(
            "File: {} · cooked: {}",
            size(self.file_bytes),
            match self.cooked_bytes {
                Some(bytes) => crate::asset_panels::format_bytes(bytes),
                None => "not cooked".to_string(),
            }
        ));
        if let Some(triangles) = self.triangles {
            lines.push(format!("Triangles: {triangles}"));
        }
        if let Some((w, h)) = self.dimensions {
            lines.push(format!("Size: {w} × {h}"));
        }
        if let Some(ms) = self.duration_ms {
            lines.push(format!("Length: {}.{:03} s", ms / 1000, ms % 1000));
        }
        if let Some(modified) = &self.modified {
            lines.push(format!("Modified: {modified}"));
        }
        lines.push(format!("Referenced by {} file(s)", self.referencers));
        lines.push(format!(
            "Revision: {}",
            self.revision.as_deref().unwrap_or("not tracked")
        ));
        lines
    }

    /// The agent's view.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "file_bytes": self.file_bytes,
            "cooked_bytes": self.cooked_bytes,
            "triangles": self.triangles,
            "dimensions": self.dimensions.map(|(w, h)| [w, h]),
            "duration_ms": self.duration_ms,
            "modified": self.modified,
            "referencers": self.referencers,
            "revision": self.revision,
        })
    }
}

/// Triangles of a glTF/GLB file from its JSON alone (index count / 3, or vertex count / 3 for a
/// non-indexed primitive; triangle-list primitives only). No buffer is read.
fn gltf_triangles(full: &Path) -> Option<u64> {
    let bytes = std::fs::read(full).ok()?;
    let gltf = gltf::Gltf::from_slice_without_validation(&bytes).ok()?;
    let mut total = 0u64;
    for mesh in gltf.meshes() {
        for primitive in mesh.primitives() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            let count = match primitive.indices() {
                Some(indices) => indices.count(),
                None => primitive.get(&gltf::Semantic::Positions).map_or(0, |a| a.count()),
            };
            total += count as u64 / 3;
        }
    }
    Some(total)
}

/// Length of a PCM WAV file from its `fmt ` and `data` chunk headers (Microsoft RIFF WAVE
/// layout, public format description); other formats answer `None`.
fn wav_duration_ms(full: &Path) -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(full).ok()?;
    let mut head = vec![0u8; 4096];
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    if head.get(0..4)? != b"RIFF" || head.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut byte_rate = None;
    while at + 8 <= head.len() {
        let id = &head[at..at + 4];
        let size = u32::from_le_bytes(head[at + 4..at + 8].try_into().ok()?);
        if id == b"fmt " {
            byte_rate = Some(u32::from_le_bytes(head.get(at + 16..at + 20)?.try_into().ok()?));
        } else if id == b"data" {
            let rate = u64::from(byte_rate.filter(|r| *r > 0)?);
            return Some(u64::from(size) * 1000 / rate);
        }
        at += 8 + size as usize + (size as usize & 1);
    }
    None
}

/// The details on screen, read once per asset and index generation (the tooltip asks every
/// frame the pointer rests on a row).
#[derive(Resource, Debug, Default)]
pub struct DetailsCache {
    generation: u64,
    entries: std::collections::HashMap<String, AssetDetails>,
    /// Details read from disk so far.
    pub reads: u64,
}

impl DetailsCache {
    /// The details of `rel`, read on first use per index `generation`; the revision state comes
    /// from `revisions` every call.
    pub fn get(
        &mut self,
        root: &Path,
        rel: &str,
        generation: u64,
        revisions: Option<&AssetRevisionStates>,
    ) -> AssetDetails {
        if self.generation != generation {
            self.entries.clear();
            self.generation = generation;
        }
        let mut details = match self.entries.get(rel) {
            Some(details) => details.clone(),
            None => {
                self.reads += 1;
                let details = AssetDetails::read(root, rel);
                self.entries.insert(rel.to_string(), details.clone());
                details
            }
        };
        details.revision = revisions.and_then(|r| r.0.get(rel).cloned());
        details
    }
}

/// The tooltip text of `rel`: its path, the details and the provenance line.
pub fn tooltip_text(world: &mut World, root: &Path, rel: &str, provenance: Option<&str>) -> String {
    let generation = world
        .get_resource::<crate::content_index::ContentIndex>()
        .map_or(0, |index| index.generation);
    let revision = revision_of(world, rel);
    let mut details = world
        .get_resource_or_insert_with(DetailsCache::default)
        .get(root, rel, generation, None);
    details.revision = revision;
    let mut text = rel.to_string();
    for line in details.lines() {
        text.push('\n');
        text.push_str(&line);
    }
    if let Some(provenance) = provenance.filter(|p| !p.is_empty()) {
        text.push('\n');
        text.push_str(provenance);
    }
    text
}

// ---------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------

const PATHS: ParamSchema = ParamSchema {
    name: "paths",
    kind: "array",
    required: true,
    description: "Project-relative paths under assets/.",
};

/// `editor.content_select`, `editor.bulk_asset_action`, `editor.retarget_references` and
/// `editor.asset_details`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.content_select",
                tool: "content_select",
                description: "Sets the Content Browser's selection (a list, what Ctrl/Shift clicks, box select and \
                    Shift+arrows build): mode set (default) replaces it, add and remove change it. Answers the \
                    selection.",
                params: &[
                    PATHS,
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "set (default), add or remove.",
                    },
                ],
                result: "{ selected: [path] }",
                mutating: true,
                permission: Permission::Assets,
            },
            content_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bulk_asset_action",
                tool: "bulk_asset_action",
                description: "Runs one Content Browser bulk action over several assets (the selection menu): \
                    validate, reimport, save (the open documents among them with unsaved edits), move (into \
                    folder `to`, references rewritten, one undo step per asset), delete (needs `confirm: true`; \
                    the first call answers `needs_confirmation` with data.referencers, every file that \
                    references any of them) or add_to_collection (`collection`).",
                params: &[
                    PATHS,
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "validate, reimport, save, move, delete or add_to_collection.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: false,
                        description: "move: the destination folder under assets/.",
                    },
                    ParamSchema {
                        name: "collection",
                        kind: "string",
                        required: false,
                        description: "add_to_collection: a static collection's name.",
                    },
                    ParamSchema {
                        name: "confirm",
                        kind: "boolean",
                        required: false,
                        description: "delete: true deletes.",
                    },
                ],
                result: "{ action, summary, referencers, results: [{path, ok, note}] }",
                mutating: true,
                permission: Permission::Assets,
            },
            bulk_asset_action as Handler,
        ),
        (
            MethodSchema {
                name: "editor.retarget_references",
                tool: "retarget_references",
                description: "Points every reference to `from` (a missing path from the Reference graph or the \
                    Problems panel, or any asset) at `to`, an existing asset of the same kind: string paths are \
                    rewritten, and the asset id too when both have a .meta. One undo step. `similar: true` only \
                    answers the assets with `from`'s file name elsewhere (Find similar).",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "The path the references name (it may be missing).",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: false,
                        description: "The asset they should name; required unless similar is true.",
                    },
                    ParamSchema {
                        name: "similar",
                        kind: "boolean",
                        required: false,
                        description: "true answers { from, similar: [path] } and changes nothing.",
                    },
                ],
                result: "{ from, to, rewritten: [path], deleted } (similar: { from, similar })",
                mutating: true,
                permission: Permission::Assets,
            },
            retarget_references as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_details",
                tool: "asset_details",
                description: "What the Content Browser's hover tooltip shows for one asset: kind, file and cooked \
                    size, triangles (glTF meshes), dimensions (textures), length (WAV sounds), last modified, how \
                    many files reference it, and the revision state when a provider set one.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path under assets/.",
                }],
                result: "{ path, kind, file_bytes, cooked_bytes, triangles, dimensions, duration_ms, modified, referencers, revision }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_details as Handler,
        ),
    ]
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            )
        })
}

/// The `paths` parameter, each confined to the project's `assets/` folder.
fn request_paths(root: &Path, params: &Value) -> Result<Vec<String>, RpcError> {
    let list = params.get("paths").and_then(Value::as_array).ok_or_else(|| {
        RpcError::invalid_params("`paths` (an array of project-relative paths) is required")
    })?;
    let mut out = Vec::new();
    for value in list {
        let path = value
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| RpcError::invalid_params("every entry of `paths` is a non-empty string"))?;
        let full = crate::asset_ops_agent::assets_request_path(root, "paths", path)?;
        out.push(crate::content::slash_path(
            full.strip_prefix(root).unwrap_or(&full),
        ));
    }
    Ok(out)
}

fn content_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let paths = request_paths(&root, params)?;
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("set");
    let mut selection = world.get_resource_or_insert_with(ContentSelection::default);
    match mode {
        "set" => selection.set(paths),
        "add" => selection.add(paths),
        "remove" => selection.remove(&paths),
        other => {
            return Err(crate::asset_ops_agent::reasoned(
                "content_select_bad_mode",
                format!("unknown mode {other:?}: set, add or remove"),
            ));
        }
    }
    Ok(json!({ "selected": selection.paths() }))
}

fn bulk_asset_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let paths = request_paths(&root, params)?;
    let action = BulkAction::from_params(params).map_err(|e| e.to_rpc())?;
    let name = params.get("action").and_then(Value::as_str).unwrap_or_default();
    match run_bulk(world, &root, &paths, &action) {
        Ok(outcome) => Ok(outcome.to_json(name)),
        Err(error) => {
            let mut rpc = error.to_rpc();
            if error.code == "needs_confirmation"
                && let Some(data) = rpc.data.as_mut().and_then(Value::as_object_mut)
            {
                data.insert("referencers".into(), json!(referencers_of_all(&root, &paths)));
            }
            Err(rpc)
        }
    }
}

fn retarget_references(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let from = params
        .get("from")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`from` (the referenced path) is required"))?;
    if params.get("similar").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({ "from": project_rel(from), "similar": find_similar(&root, from) }));
    }
    let to = params
        .get("to")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`to` (the asset to point at) is required"))?;
    // Both are request paths: the agent's confinement first (symlinks, goldens), then the kind
    // and existence checks of `retarget`.
    crate::asset_ops_agent::assets_request_path(&root, "to", to)?;
    retarget(world, &root, from, to)
        .map(|outcome| outcome.to_json())
        .map_err(|e| e.to_rpc())
}

fn asset_details(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (project-relative, under assets/) is required"))?;
    let full = crate::asset_ops_agent::assets_request_path(&root, "path", path)?;
    let rel = crate::content::slash_path(full.strip_prefix(&root).unwrap_or(&full));
    if !full.is_file() {
        return Err(RpcError::not_found(
            format!("{rel} does not exist"),
            "editor.list_assets shows the project's files",
        ));
    }
    let mut details = AssetDetails::read(&root, &rel);
    details.revision = revision_of(world, &rel);
    let mut value = details.to_json();
    if let Some(object) = value.as_object_mut() {
        object.insert("path".into(), json!(rel));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// The asset picker of Retarget… and "Replace references with…"
// ---------------------------------------------------------------------------

/// What the picker's Apply does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPurpose {
    /// Point the references to a (missing) path at the chosen asset.
    Retarget,
    /// Point the references at the chosen asset, then delete the asset.
    DeleteReplace,
}

/// The open picker: the path whose references move, the assets of its kind, the filter and the
/// choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerJob {
    /// Retarget or delete with replace.
    pub purpose: PickerPurpose,
    /// The path the references name.
    pub from: String,
    /// Assets of the same kind (same file name first: the "Find similar" suggestions).
    pub choices: Vec<String>,
    /// How many of `choices` are similar (same file name), at the front.
    pub similar: usize,
    /// The filter text over `choices`.
    pub filter: String,
    /// The chosen asset.
    pub chosen: Option<String>,
    /// The last refusal.
    pub error: Option<String>,
}

/// The asset picker (an edit-world resource; the Content Browser draws it).
#[derive(Resource, Debug, Clone, Default)]
pub struct RetargetPicker {
    /// The open job.
    pub job: Option<PickerJob>,
    focus_asked: bool,
}

/// Opens the picker for `from` (project-relative). The choices are the project's assets of
/// `from`'s kind, the similar ones (same file name) first.
pub fn open_picker(world: &mut World, purpose: PickerPurpose, from: &str) {
    let from = project_rel(from);
    let root = crate::mesh_source::project_root(world);
    let similar = root
        .as_deref()
        .map(|root| find_similar(root, &from))
        .unwrap_or_default();
    let kind = kind_key(&from);
    let mut index = world.get_resource_or_insert_with(crate::content_index::ContentIndex::default);
    index.ensure(root.as_deref());
    let mut choices = similar.clone();
    for (path, _) in index.assets().iter() {
        if *path != from && kind_key(path) == kind && !choices.contains(path) {
            choices.push(path.clone());
        }
    }
    world.insert_resource(RetargetPicker {
        job: Some(PickerJob {
            purpose,
            from,
            similar: similar.len(),
            chosen: similar.first().cloned(),
            choices,
            filter: String::new(),
            error: None,
        }),
        focus_asked: false,
    });
}

/// Carries out the picker's Apply; answers the toast line, or keeps the job open with the error.
pub fn apply_picker(world: &mut World) -> Option<String> {
    let mut picker = world.remove_resource::<RetargetPicker>().unwrap_or_default();
    let Some(job) = picker.job.as_mut() else {
        world.insert_resource(picker);
        return None;
    };
    let Some(root) = crate::mesh_source::project_root(world) else {
        job.error = Some("no project is open".to_string());
        world.insert_resource(picker);
        return None;
    };
    let Some(to) = job.chosen.clone() else {
        job.error = Some("choose an asset first".to_string());
        world.insert_resource(picker);
        return None;
    };
    let (purpose, from) = (job.purpose, job.from.clone());
    let result = match purpose {
        PickerPurpose::Retarget => retarget(world, &root, &from, &to),
        PickerPurpose::DeleteReplace => delete_with_replace(world, &root, &from, &to),
    };
    match result {
        Ok(outcome) => {
            picker.job = None;
            world.insert_resource(picker);
            if outcome.deleted {
                world
                    .get_resource_or_insert_with(ContentSelection::default)
                    .remove(std::slice::from_ref(&outcome.from));
            }
            Some(format!(
                "{} {} references to {} ({} file(s) rewritten)",
                if outcome.deleted {
                    "Deleted, moved"
                } else {
                    "Moved"
                },
                outcome.from,
                outcome.to,
                outcome.rewritten.len()
            ))
        }
        Err(error) => {
            if let Some(job) = picker.job.as_mut() {
                job.error = Some(error.to_string());
            }
            world.insert_resource(picker);
            None
        }
    }
}

/// Draws the picker while a job is open. The filter field takes the keyboard focus when it opens;
/// Up/Down move the choice, Enter applies, Escape cancels; every choice is a focusable widget.
pub fn picker_window(ctx: &egui::Context, world: &mut World, toast: &mut Option<String>) {
    let Some(mut picker) = world.remove_resource::<RetargetPicker>() else {
        return;
    };
    let Some(mut job) = picker.job.take() else {
        world.insert_resource(picker);
        return;
    };
    let mut apply = false;
    let mut cancel = false;
    let title = match job.purpose {
        PickerPurpose::Retarget => "Retarget references",
        PickerPurpose::DeleteReplace => "Replace references and delete",
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(true)
        .default_width(420.0)
        .show(ctx, |ui| {
            // Enter applies and Escape cancels before any row sees the key (a row the arrows
            // focused would otherwise take Enter as its own click).
            apply = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            cancel = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            match job.purpose {
                PickerPurpose::Retarget => ui.label(format!("Point every reference to {} at:", job.from)),
                PickerPurpose::DeleteReplace => ui.label(format!(
                    "Point every reference to {} at this asset, then delete it:",
                    job.from
                )),
            };
            let label = ui.label("Filter");
            let field = ui.text_edit_singleline(&mut job.filter).labelled_by(label.id);
            if !picker.focus_asked {
                field.request_focus();
                picker.focus_asked = true;
            }
            let needle = job.filter.to_lowercase();
            let shown: Vec<(usize, &String)> = job
                .choices
                .iter()
                .enumerate()
                .filter(|(_, path)| needle.is_empty() || path.to_lowercase().contains(&needle))
                .collect();
            // Up/Down step through the shown choices even while the filter field has focus.
            let step = ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    1
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    -1
                } else {
                    0
                }
            });
            if step != 0 {
                // The arrows pick a choice; the filter keeps the keyboard.
                field.request_focus();
            }
            if step != 0 && !shown.is_empty() {
                let at = shown
                    .iter()
                    .position(|(_, p)| job.chosen.as_deref() == Some(p.as_str()));
                let next = match at {
                    Some(at) => at.saturating_add_signed(step).min(shown.len() - 1),
                    None => 0,
                };
                job.chosen = Some(shown[next].1.clone());
            }
            let mut picked = None;
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                if shown.is_empty() {
                    ui.weak("No asset of this kind matches");
                }
                for (at, path) in &shown {
                    let text = if *at < job.similar {
                        format!("{path}  (same name)")
                    } else {
                        (*path).clone()
                    };
                    let on = job.chosen.as_deref() == Some(path.as_str());
                    let response = ui.selectable_label(on, text);
                    if on && step != 0 {
                        response.scroll_to_me(None);
                    }
                    if response.clicked() {
                        picked = Some((*path).clone());
                    }
                }
            });
            if picked.is_some() {
                job.chosen = picked;
            }
            if let Some(error) = &job.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.horizontal(|ui| {
                apply |=
                    crate::agent_parity::action_button(ui, &crate::agent_parity::RETARGET_APPLY).clicked();
                cancel |=
                    crate::agent_parity::action_button(ui, &crate::agent_parity::RETARGET_CANCEL).clicked();
            });
        });
    if !cancel {
        picker.job = Some(job);
    }
    world.insert_resource(picker);
    if apply
        && !cancel
        && let Some(line) = apply_picker(world)
    {
        *toast = Some(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas_keys::test_input::{frame, key};
    use egui::{Key, Modifiers};

    /// A scratch project (pid + counter, never the source tree) with the editor resources.
    fn project(label: &str) -> (World, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets/props")).expect("assets");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    /// Writes `rel` with `bytes` and a `.meta` with a fresh id; answers the id text.
    fn asset(root: &Path, rel: &str, bytes: &[u8]) -> String {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
        std::fs::write(&full, bytes).expect("asset");
        let id = regia_asset::AssetId::new_random();
        let meta = regia_asset::MetaSidecar::new("png", id)
            .to_ron_string()
            .expect("meta");
        std::fs::write(regia_asset::meta_path_for(&full), meta).expect("meta file");
        id.to_string()
    }

    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel)).unwrap_or_default()
    }

    /// Calls a method through the registered extension, as dispatch does (spec 96).
    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = regia_agent::extension::AgentExtension::default();
        for (schema, handler) in methods()
            .into_iter()
            .chain(crate::asset_ops_agent::methods())
            .chain(crate::save_all_dialog::methods())
        {
            extension.register(schema, handler);
        }
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    /// The `data.reason` of a refused call.
    fn reason(result: Result<Value, RpcError>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value}"),
            Err(error) => error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }

    const LISTED: [&str; 5] = [
        "assets/a.png",
        "assets/b.png",
        "assets/c.png",
        "assets/d.png",
        "assets/e.png",
    ];

    #[test]
    fn clicks_build_a_selection_list() {
        let mut selection = ContentSelection::default();
        selection.click("assets/b.png", ClickMods::default(), &LISTED);
        assert_eq!(selection.paths(), ["assets/b.png"]);
        // Ctrl adds, Ctrl again removes.
        let ctrl = ClickMods {
            toggle: true,
            range: false,
        };
        selection.click("assets/e.png", ctrl, &LISTED);
        assert_eq!(selection.paths(), ["assets/b.png", "assets/e.png"]);
        selection.click("assets/b.png", ctrl, &LISTED);
        assert_eq!(selection.paths(), ["assets/e.png"]);
        // Shift selects the range from the anchor (the last plain or Ctrl click), in listed order.
        let shift = ClickMods {
            toggle: false,
            range: true,
        };
        selection.click("assets/a.png", ClickMods::default(), &LISTED);
        selection.click("assets/c.png", shift, &LISTED);
        assert_eq!(
            selection.paths(),
            ["assets/a.png", "assets/b.png", "assets/c.png"]
        );
        // Shift again from the same anchor replaces the range.
        selection.click("assets/b.png", shift, &LISTED);
        assert_eq!(selection.paths(), ["assets/a.png", "assets/b.png"]);
        // Ctrl+Shift adds a second range.
        selection.click("assets/e.png", ctrl, &LISTED);
        selection.click(
            "assets/d.png",
            ClickMods {
                toggle: true,
                range: true,
            },
            &LISTED,
        );
        assert_eq!(
            selection.paths(),
            ["assets/a.png", "assets/b.png", "assets/e.png", "assets/d.png"]
        );
        selection.rename("assets/d.png", "assets/props/d.png");
        assert!(selection.contains("assets/props/d.png"));
        selection.remove(&["assets/a.png".to_string()]);
        assert!(!selection.contains("assets/a.png"));
    }

    #[test]
    fn a_box_replaces_or_adds_to_the_selection() {
        let mut selection = ContentSelection::default();
        let before = vec!["assets/a.png".to_string()];
        let hits = vec!["assets/c.png".to_string(), "assets/d.png".to_string()];
        selection.box_select(&before, &hits, false);
        assert_eq!(selection.paths(), ["assets/c.png", "assets/d.png"]);
        selection.box_select(&before, &hits, true);
        assert_eq!(
            selection.paths(),
            ["assets/a.png", "assets/c.png", "assets/d.png"]
        );
    }

    #[test]
    fn the_keyboard_moves_and_extends_the_selection() {
        // The keyboard path of multi-select (accessibility bar): arrows move, Shift+arrows extend,
        // Ctrl+A selects all, Escape clears; a focused text field keeps its keys.
        let ctx = egui::Context::default();
        let mut selection = ContentSelection::default();
        let press = |selection: &mut ContentSelection, events: Vec<egui::Event>, columns: usize| {
            let mut moved = None;
            frame(&ctx, [400.0, 300.0], events, |ui| {
                moved = selection_keys(ui.ctx(), selection, &LISTED, columns, true);
            });
            moved
        };
        assert_eq!(
            press(&mut selection, vec![key(Key::ArrowDown, Modifiers::NONE)], 1).as_deref(),
            Some("assets/a.png")
        );
        press(&mut selection, vec![key(Key::ArrowDown, Modifiers::SHIFT)], 1);
        press(&mut selection, vec![key(Key::ArrowDown, Modifiers::SHIFT)], 1);
        assert_eq!(
            selection.paths(),
            ["assets/a.png", "assets/b.png", "assets/c.png"]
        );
        // In the grid Right is one place and Down a row of `columns`.
        press(&mut selection, vec![key(Key::ArrowRight, Modifiers::NONE)], 2);
        assert_eq!(selection.paths(), ["assets/d.png"]);
        press(&mut selection, vec![key(Key::ArrowUp, Modifiers::SHIFT)], 2);
        assert_eq!(
            selection.paths(),
            ["assets/b.png", "assets/c.png", "assets/d.png"]
        );
        // Alt+Left is the folder history's: not consumed here.
        press(&mut selection, vec![key(Key::ArrowLeft, Modifiers::ALT)], 2);
        assert_eq!(selection.paths().len(), 3);
        press(&mut selection, vec![key(Key::A, Modifiers::COMMAND)], 1);
        assert_eq!(selection.paths().len(), LISTED.len());
        press(&mut selection, vec![key(Key::Escape, Modifiers::NONE)], 1);
        assert!(selection.is_empty());
        // A focused text field keeps the arrows.
        let field = egui::Id::new("search-field");
        let mut text = String::new();
        for events in [vec![], vec![key(Key::ArrowDown, Modifiers::NONE)]] {
            frame(&ctx, [400.0, 300.0], events, |ui| {
                ui.add(egui::TextEdit::singleline(&mut text).id(field))
                    .request_focus();
                selection_keys(ui.ctx(), &mut selection, &LISTED, 1, true);
            });
        }
        assert!(selection.is_empty(), "{:?}", selection.paths());
    }

    #[test]
    fn retarget_rewrites_paths_and_ids_and_undo_puts_them_back() {
        let (mut world, root) = project("content_retarget");
        let old_id = asset(&root, "assets/a.png", b"a");
        let new_id = asset(&root, "assets/props/b.png", b"b");
        let level = format!(
            "(mesh: \"assets/a.png\", other: \"a.png\", inst: \"asset:{old_id}\", keep: \"assets/props/a.png.txt\")"
        );
        std::fs::write(root.join("assets/arena.level.ron"), &level).expect("level");
        let outcome = retarget(&mut world, &root, "assets/a.png", "assets/props/b.png").expect("retarget");
        assert_eq!(outcome.rewritten, ["assets/arena.level.ron"]);
        let text = read(&root, "assets/arena.level.ron");
        assert!(text.contains("\"assets/props/b.png\""), "{text}");
        assert!(
            text.contains("\"props/b.png\""),
            "the bare form keeps its spelling: {text}"
        );
        assert!(text.contains(&format!("asset:{new_id}")), "{text}");
        assert!(!text.contains(&old_id), "{text}");
        assert!(
            text.contains("assets/props/a.png.txt"),
            "other paths stay: {text}"
        );
        assert!(root.join("assets/a.png").is_file(), "a retarget deletes nothing");
        let mut stack = world.remove_resource::<UndoStack>().expect("undo entry");
        assert!(stack.undo(&mut world));
        assert_eq!(
            read(&root, "assets/arena.level.ron"),
            level,
            "undo restores the text"
        );
        assert!(stack.redo(&mut world));
        assert!(read(&root, "assets/arena.level.ron").contains("props/b.png"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retarget_refuses_bad_pairs() {
        let (mut world, root) = project("content_retarget_refusals");
        asset(&root, "assets/a.png", b"a");
        asset(&root, "assets/b.png", b"b");
        std::fs::write(root.join("assets/s.wav"), b"RIFF").expect("wav");
        std::fs::write(root.join("assets/x.level.ron"), "(m: \"assets/a.png\")").expect("level");
        let code = |r: Result<RetargetOutcome, ActionError>| r.expect_err("refused").code;
        assert_eq!(
            code(retarget(&mut world, &root, "assets/a.png", "assets/a.png")),
            "retarget_same_asset"
        );
        assert_eq!(
            code(retarget(&mut world, &root, "assets/a.png", "assets/s.wav")),
            "retarget_kind_mismatch"
        );
        assert_eq!(
            code(retarget(&mut world, &root, "assets/a.png", "assets/zz.png")),
            "retarget_target_missing"
        );
        assert_eq!(
            code(retarget(&mut world, &root, "assets/b.png", "assets/a.png")),
            "retarget_nothing"
        );
        assert_eq!(
            code(retarget(&mut world, &root, "../a.png", "assets/b.png")),
            "path_outside_project"
        );
        assert_eq!(
            read(&root, "assets/x.level.ron"),
            "(m: \"assets/a.png\")",
            "nothing changed"
        );
        assert!(kind_key("assets/x.cue.ron") != kind_key("assets/x.seq.ron"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_reference_is_retargeted_and_similar_assets_are_found() {
        let (mut world, root) = project("content_missing_fix");
        asset(&root, "assets/props/rock.png", b"r");
        asset(&root, "assets/rocks/rock.png", b"r2");
        asset(&root, "assets/other.png", b"o");
        std::fs::write(
            root.join("assets/m.material.ron"),
            "(albedo: \"assets/old/rock.png\")",
        )
        .expect("material");
        assert_eq!(
            find_similar(&root, "assets/old/rock.png"),
            ["assets/props/rock.png", "assets/rocks/rock.png"]
        );
        assert_eq!(
            missing_path_of("references `assets/old/rock.png`, which is not in the asset set"),
            Some("assets/old/rock.png")
        );
        let out = call(
            &mut world,
            "editor.retarget_references",
            json!({ "from": "assets/old/rock.png", "similar": true }),
        )
        .expect("similar");
        assert_eq!(
            out["similar"],
            json!(["assets/props/rock.png", "assets/rocks/rock.png"])
        );
        let out = call(
            &mut world,
            "editor.retarget_references",
            json!({ "from": "assets/old/rock.png", "to": "assets/rocks/rock.png" }),
        )
        .expect("retarget");
        assert_eq!(out["rewritten"], json!(["assets/m.material.ron"]));
        assert_eq!(
            read(&root, "assets/m.material.ron"),
            "(albedo: \"assets/rocks/rock.png\")"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.retarget_references",
                json!({ "from": "assets/old/rock.png", "to": "/etc/passwd" })
            )),
            "path_outside_project"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_with_replace_is_one_undo_step() {
        let (mut world, root) = project("content_delete_replace");
        let old_id = asset(&root, "assets/a.png", b"a-bytes");
        let new_id = asset(&root, "assets/b.png", b"b");
        let level = format!("(t: \"assets/a.png\", i: \"asset:{old_id}\")");
        std::fs::write(root.join("assets/l.level.ron"), &level).expect("level");
        let meta_before = std::fs::read(root.join("assets/a.png.meta")).expect("meta");
        // The agent route: confirmation first, then the delete with replace.
        assert_eq!(
            reason(call(
                &mut world,
                "editor.delete_asset",
                json!({ "path": "assets/a.png", "replace_with": "assets/b.png" })
            )),
            "needs_confirmation"
        );
        assert!(root.join("assets/a.png").is_file());
        let out = call(
            &mut world,
            "editor.delete_asset",
            json!({ "path": "assets/a.png", "replace_with": "assets/b.png", "confirm": true }),
        )
        .expect("delete with replace");
        assert_eq!(out["replaced_with"], "assets/b.png");
        assert_eq!(out["rewritten"], json!(["assets/l.level.ron"]));
        assert!(!root.join("assets/a.png").exists());
        assert!(!root.join("assets/a.png.meta").exists());
        let text = read(&root, "assets/l.level.ron");
        assert_eq!(text, format!("(t: \"assets/b.png\", i: \"asset:{new_id}\")"));
        // One undo puts the file, its .meta and the references back.
        let mut stack = world.remove_resource::<UndoStack>().expect("undo entry");
        assert_eq!(stack.undo_depth(), 1);
        assert!(stack.undo(&mut world));
        assert_eq!(
            std::fs::read(root.join("assets/a.png")).expect("restored"),
            b"a-bytes"
        );
        assert_eq!(
            std::fs::read(root.join("assets/a.png.meta")).expect("meta back"),
            meta_before
        );
        assert_eq!(read(&root, "assets/l.level.ron"), level);
        // A replacement of another kind is refused before anything changes.
        std::fs::write(root.join("assets/s.wav"), b"RIFF").expect("wav");
        assert_eq!(
            reason(call(
                &mut world,
                "editor.delete_asset",
                json!({ "path": "assets/a.png", "replace_with": "assets/s.wav", "confirm": true })
            )),
            "retarget_kind_mismatch"
        );
        assert!(root.join("assets/a.png").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bulk_move_delete_and_selection_through_the_agent() {
        let (mut world, root) = project("content_bulk");
        asset(&root, "assets/a.png", b"a");
        asset(&root, "assets/b.png", b"b");
        asset(&root, "assets/c.png", b"c");
        std::fs::write(root.join("assets/l.level.ron"), "(t: \"assets/a.png\")").expect("level");
        // Selection.
        let out = call(
            &mut world,
            "editor.content_select",
            json!({ "paths": ["assets/a.png", "b.png"] }),
        )
        .expect("select");
        assert_eq!(out["selected"], json!(["assets/a.png", "assets/b.png"]));
        let out = call(
            &mut world,
            "editor.content_select",
            json!({ "paths": ["assets/c.png"], "mode": "add" }),
        )
        .expect("add");
        assert_eq!(
            out["selected"],
            json!(["assets/a.png", "assets/b.png", "assets/c.png"])
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.content_select",
                json!({ "paths": [], "mode": "toggle" })
            )),
            "content_select_bad_mode"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.content_select",
                json!({ "paths": ["../x.png"] })
            )),
            "path_outside_project"
        );
        // Move two into a folder: the reference follows, so does the selection.
        let out = call(
            &mut world,
            "editor.bulk_asset_action",
            json!({ "paths": ["assets/a.png", "assets/b.png"], "action": "move", "to": "assets/props" }),
        )
        .expect("move");
        assert_eq!(out["results"][0]["ok"], true, "{out}");
        assert!(root.join("assets/props/a.png").is_file());
        assert!(root.join("assets/props/b.png.meta").is_file());
        assert_eq!(read(&root, "assets/l.level.ron"), "(t: \"assets/props/a.png\")");
        let selected = world.resource::<ContentSelection>().paths().to_vec();
        assert!(
            selected.contains(&"assets/props/a.png".to_string()),
            "{selected:?}"
        );
        // Delete asks first and lists every referencer once.
        let err = call(
            &mut world,
            "editor.bulk_asset_action",
            json!({ "paths": ["assets/props/a.png", "assets/c.png"], "action": "delete" }),
        )
        .expect_err("needs confirmation");
        let data = err.data.clone().unwrap_or_default();
        assert_eq!(data["reason"], "needs_confirmation");
        assert_eq!(data["referencers"], json!(["assets/l.level.ron"]));
        assert!(root.join("assets/c.png").is_file());
        let out = call(
            &mut world,
            "editor.bulk_asset_action",
            json!({ "paths": ["assets/props/a.png", "assets/c.png"], "action": "delete", "confirm": true }),
        )
        .expect("delete");
        assert_eq!(out["referencers"], json!(["assets/l.level.ron"]));
        assert!(!root.join("assets/c.png").exists());
        assert!(!root.join("assets/props/a.png.meta").exists());
        assert!(!world.resource::<ContentSelection>().contains("assets/c.png"));
        assert_eq!(
            reason(call(
                &mut world,
                "editor.bulk_asset_action",
                json!({ "paths": ["assets/props/b.png"], "action": "explode" })
            )),
            "bulk_unknown_action"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.bulk_asset_action",
                json!({ "paths": [], "action": "validate" })
            )),
            "bulk_empty"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.bulk_asset_action",
                json!({ "paths": ["assets/props/b.png"], "action": "move" })
            )),
            "bulk_missing_param"
        );
        // Validate with no cooked registry says so instead of failing.
        let out = call(
            &mut world,
            "editor.bulk_asset_action",
            json!({ "paths": ["assets/props/b.png"], "action": "validate" }),
        )
        .expect("validate");
        assert!(
            out["summary"].as_str().unwrap_or_default().contains("registry"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A 1 s mono 16-bit 8 kHz PCM WAV.
    fn wav() -> Vec<u8> {
        let data = 16_000u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        bytes.resize(bytes.len() + data as usize, 0);
        bytes
    }

    /// A 3×2 PNG.
    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(3, 2)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("png");
        bytes
    }

    #[test]
    fn details_show_kind_size_triangles_dimensions_length_and_referencers() {
        let (mut world, root) = project("content_details");
        asset(&root, "assets/t.png", &png());
        std::fs::write(root.join("assets/s.wav"), wav()).expect("wav");
        let gltf = r#"{"asset":{"version":"2.0"},"accessors":[{"componentType":5123,"count":6,"type":"SCALAR"},{"componentType":5126,"count":4,"type":"VEC3"}],"meshes":[{"primitives":[{"attributes":{"POSITION":1},"indices":0}]}]}"#;
        std::fs::write(root.join("assets/m.gltf"), gltf).expect("gltf");
        std::fs::write(
            root.join("assets/u.prefab.ron"),
            "(a: \"assets/t.png\", b: \"assets/m.gltf\")",
        )
        .expect("prefab");
        let texture = AssetDetails::read(&root, "assets/t.png");
        assert_eq!(texture.kind, "texture");
        assert_eq!(texture.dimensions, Some((3, 2)));
        assert_eq!(texture.referencers, 1);
        assert_eq!(texture.cooked_bytes, None, "never cooked");
        assert!(texture.modified.is_some());
        let sound = AssetDetails::read(&root, "assets/s.wav");
        assert_eq!(sound.duration_ms, Some(1000));
        let mesh = AssetDetails::read(&root, "assets/m.gltf");
        assert_eq!(mesh.triangles, Some(2));
        let lines = mesh.lines().join("\n");
        assert!(
            lines.contains("Triangles: 2") && lines.contains("Referenced by 1 file(s)"),
            "{lines}"
        );
        assert!(lines.contains("Revision: not tracked"), "{lines}");
        // The revision slot shows a provider's state; the cache reads each asset once.
        world.insert_resource(AssetRevisionStates(
            [("assets/m.gltf".to_string(), "modified".to_string())].into(),
        ));
        let first = tooltip_text(&mut world, &root, "assets/m.gltf", Some("made by: tool"));
        let again = tooltip_text(&mut world, &root, "assets/m.gltf", Some("made by: tool"));
        assert_eq!(first, again);
        assert!(
            first.contains("Revision: modified") && first.ends_with("made by: tool"),
            "{first}"
        );
        assert_eq!(world.resource::<DetailsCache>().reads, 1);
        let out = call(
            &mut world,
            "editor.asset_details",
            json!({ "path": "assets/s.wav" }),
        )
        .expect("details");
        assert_eq!(out["duration_ms"], 1000);
        assert_eq!(out["kind"], "audio");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_picker_works_from_the_keyboard() {
        // Keyboard path of the Retarget… picker: the filter has the focus, Down picks, Enter
        // applies.
        let (mut world, root) = project("content_picker_keys");
        asset(&root, "assets/props/rock.png", b"r");
        asset(&root, "assets/zz.png", b"z");
        std::fs::write(
            root.join("assets/m.material.ron"),
            "(albedo: \"assets/old/rock.png\")",
        )
        .expect("material");
        open_picker(&mut world, PickerPurpose::Retarget, "assets/old/rock.png");
        {
            let job = world.resource::<RetargetPicker>().job.clone().expect("open");
            assert_eq!(job.choices, ["assets/props/rock.png", "assets/zz.png"]);
            assert_eq!(
                job.chosen.as_deref(),
                Some("assets/props/rock.png"),
                "the similar asset first"
            );
        }
        let ctx = egui::Context::default();
        let mut toast = None;
        let run = |world: &mut World, events: Vec<egui::Event>, toast: &mut Option<String>| {
            frame(&ctx, [800.0, 600.0], events, |ui| {
                picker_window(ui.ctx(), world, toast)
            });
        };
        run(&mut world, vec![], &mut toast);
        assert!(
            ctx.memory(|m| m.focused().is_some()),
            "the filter takes the focus"
        );
        run(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)], &mut toast);
        assert_eq!(
            world
                .resource::<RetargetPicker>()
                .job
                .as_ref()
                .and_then(|j| j.chosen.clone())
                .as_deref(),
            Some("assets/zz.png")
        );
        run(&mut world, vec![key(Key::Enter, Modifiers::NONE)], &mut toast);
        assert!(
            world.resource::<RetargetPicker>().job.is_none(),
            "applied and closed"
        );
        assert_eq!(
            read(&root, "assets/m.material.ron"),
            "(albedo: \"assets/zz.png\")"
        );
        assert!(toast.unwrap_or_default().contains("1 file(s) rewritten"));
        // Escape cancels.
        open_picker(&mut world, PickerPurpose::Retarget, "assets/zz.png");
        let mut toast = None;
        run(&mut world, vec![key(Key::Escape, Modifiers::NONE)], &mut toast);
        assert!(world.resource::<RetargetPicker>().job.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One frame of `draw` in which pass 0 asks for a discard when `discard` is set (as a layout
    /// change or a rich tooltip does); answers the number of passes egui ran.
    fn discard_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        discard: bool,
        mut draw: impl FnMut(&mut egui::Ui),
    ) -> usize {
        let mut passes = 0;
        let raw = crate::canvas_keys::test_input::raw([1200.0, 900.0], events);
        let mut out = ctx.run_ui(raw, |ui| {
            passes += 1;
            if discard && ui.ctx().current_pass_index() == 0 {
                ui.ctx().request_discard("test: a second pass of the input frame");
            }
            draw(ui);
        });
        out.textures_delta.clear();
        passes
    }

    /// Review w12-rev-g10 row 2: a second pass of the frame (egui 0.36 `Context::run_ui` hands it
    /// `RawInput::take()`, which moves the events out) sees no key events, so one arrow press
    /// moves the cursor one place, in the list and in the grid.
    #[test]
    fn a_second_pass_of_the_frame_does_not_step_the_selection_again() {
        let ctx = egui::Context::default();
        let mut selection = ContentSelection::default();
        let press = |selection: &mut ContentSelection, k: Key, columns: usize| {
            discard_frame(&ctx, vec![key(k, Modifiers::NONE)], true, |ui| {
                selection_keys(ui.ctx(), selection, &LISTED, columns, true);
            })
        };
        assert_eq!(press(&mut selection, Key::ArrowDown, 1), 2, "the frame ran twice");
        assert_eq!(selection.paths(), ["assets/a.png"]);
        assert_eq!(press(&mut selection, Key::ArrowDown, 1), 2);
        assert_eq!(selection.paths(), ["assets/b.png"], "one place per press");
        assert_eq!(press(&mut selection, Key::ArrowRight, 2), 2);
        assert_eq!(selection.paths(), ["assets/c.png"], "grid Right: one place");
        assert_eq!(press(&mut selection, Key::ArrowDown, 2), 2);
        assert_eq!(selection.paths(), ["assets/e.png"], "grid Down: one row");
    }

    /// Review w12-rev-g10 row 1: a Ctrl-click whose release frame runs twice (a discard pass) still
    /// toggles the asset once, on the real Content Browser panel.
    #[test]
    fn a_ctrl_click_with_a_second_pass_selects_the_asset_once() {
        let (_, root) = project("content_ctrl_click_passes");
        for rel in ["assets/a.png", "assets/b.png"] {
            asset(&root, rel, b"x");
        }
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let mut world = std::mem::take(app.world_mut());
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut open = crate::level_io::new_level(&mut world);
        let mut state = crate::content::ContentBrowserState {
            project_root: Some(root.clone()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut run = |world: &mut World, events: Vec<egui::Event>, discard: bool| {
            let mut shapes = Vec::new();
            let raw = crate::canvas_keys::test_input::raw([1200.0, 900.0], events);
            let mut passes = 0;
            let mut out = ctx.run_ui(raw, |ui| {
                passes += 1;
                if discard && ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: a second pass of the click frame");
                }
                egui::CentralPanel::default().show(ui, |ui| {
                    crate::content::content_browser_panel(ui, world, &mut state, &mut open);
                });
            });
            out.textures_delta.clear();
            fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
                match shape {
                    egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                    egui::Shape::Text(text) => out.push((
                        text.galley.text().to_string(),
                        shape.visual_bounding_rect().center(),
                    )),
                    _ => {}
                }
            }
            for clipped in &out.shapes {
                walk(&clipped.shape, &mut shapes);
            }
            (passes, shapes)
        };
        run(&mut world, Vec::new(), false);
        run(&mut world, Vec::new(), false);
        let texts = run(&mut world, Vec::new(), false).1;
        let pos = texts
            .iter()
            .find(|(t, _)| t.ends_with("b.png"))
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("no b.png row in {texts:?}"));
        let ctrl = Modifiers::COMMAND;
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: ctrl,
        };
        run(
            &mut world,
            vec![
                egui::Event::ModifiersChanged(ctrl),
                egui::Event::PointerMoved(pos),
            ],
            false,
        );
        run(&mut world, vec![button(true)], false);
        let (passes, _) = run(&mut world, vec![button(false)], true);
        assert_eq!(passes, 2, "the click frame ran twice");
        run(
            &mut world,
            vec![egui::Event::ModifiersChanged(Modifiers::NONE)],
            false,
        );
        let selected = world
            .get_resource::<ContentSelection>()
            .map(|s| s.paths().to_vec())
            .unwrap_or_default();
        assert_eq!(selected, ["assets/b.png"], "one Ctrl-click selects the asset");
        let _ = std::fs::remove_dir_all(&root);
    }
}
