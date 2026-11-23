// SPDX-License-Identifier: MPL-2.0
//! The state tree editor (`docs/specs/38-state-trees.md`, ADR-3342): a panel that authors
//! `.statetree.ron` files. It shows the hierarchy and the transitions as a node view, a palette
//! (new states, the registered tasks, transition triggers), an inspector for the selected state's
//! selection, tasks, parameters (`enter_set` literals) and transitions, inline validation errors,
//! and, while the game plays, the active path of a running tree and what the last save did to the
//! agents running that file (`Kept` or `Restarted`, through the existing file reload of ADR-2408).
//!
//! State lives in the [`StEditor`] world resource so the panel and the agent protocol
//! (`editor.statetree_*`, [`agent`]) edit the same document. Every document edit is a [`StEdit`]
//! snapshot command on the editor's shared [`UndoStack`], so `editor.undo` and Ctrl+Z revert tree
//! edits in order with level edits. The structural operations, the validator and the canonical
//! text live in `regia_ai::state_tree_edit` and are shared with the `ai.statetree_*` file methods.

pub mod agent;
pub(crate) mod autosave;
pub mod panel;

use std::path::{Component, Path, PathBuf};

use regia_ai::BtTaskRegistry;
use regia_ai::state_tree::{
    ReloadOutcome, StateTree, StateTreeDef, StateTreeReloads, StateTreeRunStatus, StateTreeSource,
    StateTreeStatus,
};
use regia_ai::state_tree_edit::{self, StBatchError, StIssue, StOp, ValidateOptions};
use regia_asset::SourceAssetChanged;
use regia_ecs::prelude::*;

use crate::command::{EditorCommand, UndoStack};
use crate::pie::PieSession;

/// A running tree in the play world, as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveView {
    /// The agent entity (bits) in the play world.
    pub entity: u64,
    /// `not_started`, `running`, `succeeded` or `failed`.
    pub status: &'static str,
    /// The active states, root first (the highlighted path).
    pub active_path: Vec<String>,
    /// Ticks so far.
    pub ticks: u64,
    /// The agent's `StateTreeSource` names the open file.
    pub uses_file: bool,
    /// The running definition equals the open document (state names line up).
    pub matches_document: bool,
}

/// What the last save did to the agents running the saved file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReloadView {
    /// Which save this is about (counts from 1).
    pub save: u64,
    /// The save reached a play session (its file-change message was delivered).
    pub delivered: bool,
    /// Agents whose active path survived (`ReloadOutcome::Kept`).
    pub kept: u64,
    /// Agents that restarted from the root (`ReloadOutcome::Restarted`).
    pub restarted: u64,
    /// Agents whose reload was rejected (the file did not compile there).
    pub rejected: u64,
    /// The latest outcome: `kept`, `restarted` or `rejected`.
    pub last: Option<&'static str>,
    /// Why the latest rejection happened.
    pub reason: Option<String>,
}

/// The state tree editor's world resource.
#[derive(Resource)]
pub struct StEditor {
    /// The file the tree saves to (`None` for a new tree not saved yet).
    pub path: Option<PathBuf>,
    /// The document.
    pub doc: StateTreeDef,
    /// The document as last opened or saved (`None`: never saved).
    saved: Option<StateTreeDef>,
    /// Bumped by every document change.
    pub revision: u64,
    /// Which document is open: undo entries of another document do nothing.
    session: u64,
    /// Last action's status line.
    pub toast: String,
    /// The selected state (by name).
    pub selected: Option<String>,
    /// Agents with a state tree in the play world (entity bits), refreshed by [`pump`].
    pub agents: Vec<u64>,
    /// The agent whose live state to show (entity bits). `None`: the first one running the open
    /// file, else the first one.
    pub watch: Option<u64>,
    /// The watched tree's live state while the game plays.
    pub live: Option<LiveView>,
    /// What the last save did to running agents.
    pub reload: Option<ReloadView>,
    /// Saves so far.
    pub saves: u64,
    /// A saved file whose change message the next [`pump`] delivers to the play world.
    pending_reload: Option<PathBuf>,
    /// The play world's reload record total when the last save was delivered.
    reload_baseline: Option<u64>,
    /// Panel state (text fields, discard warning).
    pub ui: panel::PanelState,
}

impl Default for StEditor {
    fn default() -> Self {
        Self {
            path: None,
            doc: StateTreeDef::new(""),
            saved: None,
            revision: 0,
            session: 0,
            toast: String::new(),
            selected: None,
            agents: Vec::new(),
            watch: None,
            live: None,
            reload: None,
            saves: 0,
            pending_reload: None,
            reload_baseline: None,
            ui: panel::PanelState::default(),
        }
    }
}

impl StEditor {
    /// Whether the document differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.doc)
    }

    /// Whether a tree is open (opened, or started with [`Self::new_tree`]).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.path.is_some() || self.saved.is_some() || self.revision > 0
    }

    /// Whether opening or starting another tree would lose work.
    #[must_use]
    pub fn has_unsaved_work(&self) -> bool {
        self.is_open() && self.dirty() && (self.saved.is_some() || !self.doc.states.is_empty())
    }

    /// Starts a new tree (saved to `path` later). A non-empty `root` adds that root state.
    pub fn new_tree(&mut self, path: Option<PathBuf>, root: &str) {
        self.doc = StateTreeDef::new(root);
        if !root.is_empty() {
            self.doc.states.push(regia_ai::state_tree::StateDef::new(root));
        }
        self.path = path;
        self.saved = None;
        self.session += 1;
        self.revision += 1;
        self.selected = (!root.is_empty()).then(|| root.to_string());
        self.reload = None;
        self.toast = "new tree".into();
    }

    /// Opens a `.statetree.ron`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let doc = state_tree_edit::parse_statetree_ron(&text)
            .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.doc = doc;
        self.saved = Some(self.doc.clone());
        self.path = Some(path.to_path_buf());
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.reload = None;
        self.toast = format!("opened {}", path.display());
        Ok(())
    }

    /// The selected state's index in the document, if it still exists.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let name = self.selected.as_deref()?;
        self.doc.states.iter().position(|s| s.name == name)
    }
}

/// Writes `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("ron.new");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
}

/// The editor's project root, if a project is open.
#[must_use]
pub fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

/// Resolves a user path: absolute as given, otherwise under the project root.
#[must_use]
pub fn resolve_path(world: &World, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    match project_root(world) {
        Some(root) if !p.is_absolute() => root.join(p),
        _ => p,
    }
}

/// Reads an include (project-relative, no `..`) under `root`.
fn read_include(root: Option<&Path>, include: &str) -> Option<String> {
    let rel = Path::new(include);
    if include.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let root = root?;
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Every problem of the open tree: tasks checked against the world's registry when it has any task
/// registered, includes read from the project.
#[must_use]
pub fn diagnose(world: &World) -> Vec<StIssue> {
    let Some(editor) = world.get_resource::<StEditor>() else {
        return state_tree_edit::validate(&StateTreeDef::new(""), ValidateOptions::default());
    };
    let registry = world.get_resource::<BtTaskRegistry>().filter(|r| !r.is_empty());
    let root = project_root(world);
    let load = |include: &str| read_include(root.as_deref(), include);
    let self_path = match (&editor.path, &root) {
        (Some(path), Some(root)) => path
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/")),
        _ => None,
    };
    state_tree_edit::validate(
        &editor.doc,
        ValidateOptions {
            registry,
            load: Some(&load),
            self_path: self_path.as_deref(),
        },
    )
}

/// The registered task paths (the palette's task list).
#[must_use]
pub fn palette_tasks(world: &World) -> Vec<String> {
    world
        .get_resource::<BtTaskRegistry>()
        .map(|r| r.task_paths().map(str::to_string).collect())
        .unwrap_or_default()
}

/// A refused save: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveError {
    /// `no_tree_open`, `bad_path`, `invalid_tree`, `bad_value` or `save_failed`.
    pub code: &'static str,
    /// A sentence for people.
    pub message: String,
}

impl SaveError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Saves the open tree as canonical text to `path` (or where it was opened), atomically. A tree
/// with validation errors is refused (`invalid_tree`) and nothing is written. While a play session
/// runs, the next [`pump`] tells it the file changed, so agents whose `StateTreeSource` names it
/// reload it; [`StEditor::reload`] then shows the outcome.
///
/// # Errors
///
/// No tree is open, no path, a path that is not a `.statetree.ron`, validation errors, or a
/// failed write.
pub fn save(world: &mut World, path: Option<&Path>) -> Result<PathBuf, SaveError> {
    let issues = diagnose(world);
    let mut editor = world
        .get_resource_mut::<StEditor>()
        .filter(|e| e.is_open())
        .ok_or_else(|| SaveError::new("no_tree_open", "no state tree is open"))?;
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| editor.path.clone())
        .ok_or_else(|| SaveError::new("bad_path", "no tree path: pass one to save as"))?;
    if !path.to_string_lossy().ends_with(".statetree.ron") {
        return Err(SaveError::new(
            "bad_path",
            format!("{} is not a .statetree.ron path", path.display()),
        ));
    }
    if !state_tree_edit::is_valid(&issues) {
        let first = issues
            .iter()
            .find(|i| i.severity == state_tree_edit::StSeverity::Error)
            .map(|i| format!("{} at {}", i.code, i.path))
            .unwrap_or_default();
        editor.toast = format!("not saved: the tree has errors ({first})");
        return Err(SaveError::new(
            "invalid_tree",
            format!("the tree has errors and was not saved (first: {first})"),
        ));
    }
    let text =
        state_tree_edit::to_canonical_ron(&editor.doc).map_err(|e| SaveError::new(e.code, e.message))?;
    write_atomic(&path, &text).map_err(|e| SaveError::new("save_failed", e))?;
    editor.path = Some(path.clone());
    editor.saved = Some(editor.doc.clone());
    editor.saves += 1;
    editor.reload = Some(ReloadView {
        save: editor.saves,
        ..ReloadView::default()
    });
    editor.pending_reload = Some(path.clone());
    editor.reload_baseline = None;
    editor.toast = format!("saved {}", path.display());
    Ok(path)
}

/// A snapshot edit of the document on the shared undo stack. It belongs to the document it was
/// made in: once another tree is open it does nothing.
pub struct StEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: StateTreeDef,
    after: StateTreeDef,
    selected_before: Option<String>,
    selected_after: Option<String>,
}

impl StEdit {
    fn set(world: &mut World, session: u64, doc: &StateTreeDef, selected: Option<&String>) {
        let mut editor = world.get_resource_or_insert_with(StEditor::default);
        if editor.session != session {
            editor.toast = "that undo step belongs to a tree that is no longer open".into();
            return;
        }
        editor.doc = doc.clone();
        editor.revision += 1;
        editor.selected = selected
            .filter(|s| doc.states.iter().any(|st| &st.name == *s))
            .cloned();
    }
}

impl EditorCommand for StEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after, self.selected_after.as_ref());
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before, self.selected_before.as_ref());
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<StEdit>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key && self.session == next.session {
            self.after = next.after.clone();
            self.selected_after.clone_from(&next.selected_after);
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("State tree: {}", self.label)
    }
}

/// Where the selection goes after `ops`: it follows renames and drops a removed state; a newly
/// added state becomes selected.
fn selection_after(selected: Option<&String>, ops: &[StOp], doc: &StateTreeDef) -> Option<String> {
    let mut sel = selected.cloned();
    for op in ops {
        match op {
            StOp::RenameState { name, to } if sel.as_ref() == Some(name) => sel = Some(to.clone()),
            StOp::AddState { name, .. } => sel = Some(name.clone()),
            _ => {}
        }
    }
    sel.filter(|s| doc.states.iter().any(|st| &st.name == s))
}

/// Applies `ops` to a copy of the document as one undo step (merged with the previous step when
/// both carry the same `merge_key`, e.g. typing in one field). On error, or when nothing changed,
/// the document and the stack stay.
///
/// # Errors
///
/// The first operation that does not fit the tree: its position, code and message.
pub fn edit(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    ops: &[StOp],
) -> Result<(), StBatchError> {
    let (before, session, selected_before) = {
        let editor = world.get_resource_or_insert_with(StEditor::default);
        (editor.doc.clone(), editor.session, editor.selected.clone())
    };
    let mut after = before.clone();
    state_tree_edit::apply_ops(&mut after, ops)?;
    if after == before {
        return Ok(());
    }
    let selected_after = selection_after(selected_before.as_ref(), ops, &after);
    let mut command = StEdit {
        label: label.to_string(),
        merge_key,
        session,
        before,
        after,
        selected_before,
        selected_after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(())
}

fn status_name(status: StateTreeRunStatus) -> &'static str {
    match status {
        StateTreeRunStatus::NotStarted => "not_started",
        StateTreeRunStatus::Running => "running",
        StateTreeRunStatus::Succeeded => "succeeded",
        StateTreeRunStatus::Failed => "failed",
    }
}

/// Whether two paths name the same file (equal, or equal once resolved on disk).
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Editor-loop hook, once per frame: while the game plays, delivers a pending save to the play
/// world, finds the agents with a state tree, copies the watched one's active path into
/// [`StEditor::live`] and counts the reload outcomes of the last save into [`StEditor::reload`];
/// with no play session it clears the agents and the live view. Does nothing until the editor
/// resource exists (the panel or `editor.statetree_open` was used).
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    pump_world(edit, session.map(|s| s.app.world_mut()));
}

/// [`pump`] against any play world (the headless tests drive one directly).
pub fn pump_world(edit: &mut World, play: Option<&mut World>) {
    let Some(mut editor) = edit.remove_resource::<StEditor>() else {
        return;
    };
    match play {
        Some(play) => {
            let _span = tracing::debug_span!(target: "regia_editor", "statetree_editor_pump").entered();
            pump_play(&mut editor, play);
        }
        None => {
            editor.agents.clear();
            editor.live = None;
        }
    }
    edit.insert_resource(editor);
}

fn reload_total(play: &World) -> u64 {
    play.get_resource::<StateTreeReloads>()
        .map_or(0, |r| r.applied + r.rejected)
}

fn pump_play(editor: &mut StEditor, play: &mut World) {
    // A save waiting for this session: tell the play world the file changed. Its watcher compiles
    // the file once for every agent whose `StateTreeSource` names it (ADR-2408, ADR-3342); a repeat
    // report of the same text from the asset watcher compiles nothing.
    if let Some(path) = editor.pending_reload.take() {
        if play.contains_resource::<Messages<SourceAssetChanged>>() {
            editor.reload_baseline = Some(reload_total(play));
            play.write_message(SourceAssetChanged::new(path));
            if let Some(view) = editor.reload.as_mut() {
                view.delivered = true;
            }
        } else {
            editor.toast = "saved; the play world has no state-tree reload (AiPlugin) to apply it".into();
        }
    }
    let project = play
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf());
    let mut rows: Vec<(u64, bool)> = {
        let mut query = play.query::<(Entity, Option<&StateTreeSource>, &StateTree)>();
        query
            .iter(play)
            .map(|(e, source, _)| {
                let uses = match (source, editor.path.as_deref()) {
                    (Some(source), Some(open)) => {
                        let full = match &project {
                            Some(root) if !source.path.is_absolute() => root.join(&source.path),
                            _ => source.path.clone(),
                        };
                        same_file(&full, open)
                    }
                    _ => false,
                };
                (e.to_bits(), uses)
            })
            .collect()
    };
    rows.sort_unstable();
    editor.agents = rows.iter().map(|(e, _)| *e).collect();
    let watch = editor
        .watch
        .filter(|w| editor.agents.contains(w))
        .or_else(|| rows.iter().find(|(_, uses)| *uses).map(|(e, _)| *e))
        .or_else(|| editor.agents.first().copied());
    editor.live = watch.and_then(|bits| {
        let entity = Entity::try_from_bits(bits)?;
        let tree = play.get::<StateTree>(entity)?;
        let (status, active_path, ticks) = match play.get::<StateTreeStatus>(entity) {
            Some(s) => (s.status, s.active_path.clone(), s.ticks),
            None => (
                tree.status(),
                tree.active_path().into_iter().map(str::to_string).collect(),
                tree.ticks(),
            ),
        };
        Some(LiveView {
            entity: bits,
            status: status_name(status),
            active_path,
            ticks,
            uses_file: rows.iter().any(|(e, uses)| *e == bits && *uses),
            matches_document: tree.def() == &editor.doc,
        })
    });
    // The outcomes of the last delivered save: the reload records added since it was delivered
    // that name the saved file.
    if let (Some(baseline), Some(path), Some(view)) = (
        editor.reload_baseline,
        editor.path.clone(),
        editor.reload.as_mut(),
    ) && let Some(reloads) = play.get_resource::<StateTreeReloads>()
    {
        let added = (reloads.applied + reloads.rejected).saturating_sub(baseline);
        let log: Vec<_> = reloads.log().collect();
        let fresh = usize::try_from(added).unwrap_or(usize::MAX).min(log.len());
        let (mut kept, mut restarted, mut rejected) = (0, 0, 0);
        let mut last = None;
        let mut reason = None;
        for record in &log[log.len() - fresh..] {
            if !same_file(&record.path, &path) {
                continue;
            }
            match &record.result {
                Ok(ReloadOutcome::Kept) => {
                    kept += 1;
                    last = Some("kept");
                }
                Ok(ReloadOutcome::Restarted) => {
                    restarted += 1;
                    last = Some("restarted");
                }
                Err(why) => {
                    rejected += 1;
                    last = Some("rejected");
                    reason = Some(why.clone());
                }
            }
        }
        view.kept = kept;
        view.restarted = restarted;
        view.rejected = rejected;
        view.last = last;
        view.reason = reason;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod authoring_tests;
