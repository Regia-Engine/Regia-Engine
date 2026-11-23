// SPDX-License-Identifier: MPL-2.0
//! The behavior tree editor (`docs/specs/22-tier2.md` § T2.2, ADR-2408): a panel over `.bt.ron`
//! with a node palette (the node kinds plus the registered leaf tasks), add, remove, connect and
//! reorder, parameter editing, inline validation errors, undo and, while the game plays, a live
//! status and running highlight per node.
//!
//! State lives in the [`BtEditor`] world resource so the panel and the agent protocol
//! (`editor.bt_*`, [`agent`]) edit the same document. Every document edit is a [`BtEdit`]
//! snapshot command on the editor's shared [`UndoStack`], so `editor.undo` and Ctrl+Z revert tree
//! edits in order with level edits. The structural operations and the validator live in
//! `regia_ai::bt_edit` and are shared with the `ai.bt_*` file methods.

pub mod agent;
pub(crate) mod autosave;
pub mod panel;

use std::path::{Path, PathBuf};

use regia_ai::bt_edit::{self, BtIssue, BtOp};
use regia_ai::{BehaviorTree, BehaviorTreeDef, BtDebugger, BtStatus, BtTaskRegistry};
use regia_ecs::prelude::*;

use crate::command::{EditorCommand, UndoStack};
use crate::pie::PieSession;

/// A copy of a watched tree's live state in the play world (see [`pump`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveView {
    /// The agent entity (bits) in the play world.
    pub entity: u64,
    /// Ticks recorded since the watch began.
    pub ticks: u64,
    /// The root's status from the latest tick.
    pub last_status: BtStatus,
    /// Each node's status, parallel to the running tree's definition.
    pub node_status: Vec<BtStatus>,
    /// The recorded tick on which each node's status last changed.
    pub changed_at: Vec<u64>,
    /// The running tree's definition equals the open document (so indices line up).
    pub matches_document: bool,
}

/// The behavior tree editor's world resource.
#[derive(Resource, Default)]
pub struct BtEditor {
    /// The file the tree saves to (`None` for a new tree not saved yet).
    pub path: Option<PathBuf>,
    /// The document.
    pub doc: BehaviorTreeDef,
    /// The document as last opened or saved (`None`: never saved).
    saved: Option<BehaviorTreeDef>,
    /// Bumped by every document change.
    pub revision: u64,
    /// Which document is open: undo entries of another document do nothing.
    session: u64,
    /// Last action's status line.
    pub toast: String,
    /// The selected node.
    pub selected: Option<usize>,
    /// Agents with a tree in the play world (entity bits), refreshed by [`pump`].
    pub agents: Vec<u64>,
    /// The agent whose live state to show (entity bits). `None`: the first one.
    pub watch: Option<u64>,
    /// The watched tree's live state while the game plays.
    pub live: Option<LiveView>,
    /// Panel state (text fields, discard warning).
    pub ui: panel::PanelState,
}

impl BtEditor {
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
        self.is_open() && self.dirty() && (self.saved.is_some() || !self.doc.nodes.is_empty())
    }

    /// Starts a new, empty tree (saved to `path` later).
    pub fn new_tree(&mut self, path: Option<PathBuf>) {
        self.doc = BehaviorTreeDef::default();
        self.path = path;
        self.saved = None;
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.toast = "new tree".into();
    }

    /// Opens a `.bt.ron`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let doc = bt_edit::parse_bt_ron(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.doc = doc;
        self.saved = Some(self.doc.clone());
        self.path = Some(path.to_path_buf());
        self.session += 1;
        self.revision += 1;
        self.selected = None;
        self.toast = format!("opened {}", path.display());
        Ok(())
    }

    /// Saves the canonical text to `path` (or the open path), atomically. A tree with errors is
    /// saved too (the toast says so). Returns the path.
    ///
    /// # Errors
    ///
    /// No path, a path that is not a `.bt.ron`, or a failed write.
    pub fn save(&mut self, path: Option<&Path>, valid: bool) -> Result<PathBuf, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.path.clone())
            .ok_or_else(|| "no tree path: pass one to save as".to_string())?;
        if !path.to_string_lossy().ends_with(".bt.ron") {
            return Err(format!("{} is not a .bt.ron path", path.display()));
        }
        let text = bt_edit::to_canonical_ron(&self.doc)?;
        write_atomic(&path, &text)?;
        self.path = Some(path.clone());
        self.saved = Some(self.doc.clone());
        self.toast = if valid {
            format!("saved {}", path.display())
        } else {
            format!("saved {} (it does not compile yet)", path.display())
        };
        Ok(path)
    }
}

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

/// Every problem of the open tree, with leaf tasks checked against the world's registry when it
/// has any task registered.
#[must_use]
pub fn diagnose(world: &World) -> Vec<BtIssue> {
    let registry = world.get_resource::<BtTaskRegistry>().filter(|r| !r.is_empty());
    let doc = world
        .get_resource::<BtEditor>()
        .map(|e| &e.doc)
        .cloned()
        .unwrap_or_default();
    bt_edit::validate(&doc, registry)
}

/// The registered leaf task paths (the palette's leaf list).
#[must_use]
pub fn palette_tasks(world: &World) -> Vec<String> {
    world
        .get_resource::<BtTaskRegistry>()
        .map(|r| r.task_paths().map(str::to_string).collect())
        .unwrap_or_default()
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

/// A snapshot edit of the document on the shared undo stack. It belongs to the document it was
/// made in: once another tree is open it does nothing.
pub struct BtEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: BehaviorTreeDef,
    after: BehaviorTreeDef,
}

impl BtEdit {
    fn set(world: &mut World, session: u64, doc: &BehaviorTreeDef) {
        let mut editor = world.get_resource_or_insert_with(BtEditor::default);
        if editor.session != session {
            editor.toast = "that undo step belongs to a tree that is no longer open".into();
            return;
        }
        editor.doc = doc.clone();
        editor.revision += 1;
        let count = editor.doc.nodes.len();
        if editor.selected.is_some_and(|s| s >= count) {
            editor.selected = None;
        }
    }
}

impl EditorCommand for BtEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<BtEdit>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key && self.session == next.session {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Behavior tree: {}", self.label)
    }
}

/// Applies `ops` to a copy of the document as one undo step (merged with the previous step when
/// both carry the same `merge_key`, e.g. typing in one field). Returns the index each op created.
/// On error, or when nothing changed, the document and the stack stay.
///
/// # Errors
///
/// The first operation that does not fit the tree: its position, code and message.
pub fn edit(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    ops: &[BtOp],
) -> Result<Vec<Option<usize>>, bt_edit::BtBatchError> {
    let (before, session) = {
        let editor = world.get_resource_or_insert_with(BtEditor::default);
        (editor.doc.clone(), editor.session)
    };
    let mut after = before.clone();
    let created = bt_edit::apply_ops(&mut after, ops)?;
    if after == before {
        return Ok(created);
    }
    let mut command = BtEdit {
        label: label.to_string(),
        merge_key,
        session,
        before,
        after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(created)
}

/// Editor-loop hook, once per frame: while the game plays, finds the agents with a behavior tree
/// in the play world, attaches the read-only debugger to the watched one and copies its snapshot
/// into [`BtEditor::live`]; with no play session it clears them. Does nothing until the editor
/// resource exists (the panel was opened).
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let Some(mut editor) = edit.get_resource_mut::<BtEditor>() else {
        return;
    };
    let Some(session) = session else {
        editor.agents.clear();
        editor.live = None;
        return;
    };
    let _span = tracing::debug_span!(target: "regia_editor", "bt_editor_pump").entered();
    let play = session.app.world_mut();
    let mut agents: Vec<u64> = {
        let mut query = play.query::<(Entity, &BehaviorTree)>();
        query.iter(play).map(|(e, _)| e.to_bits()).collect()
    };
    agents.sort_unstable();
    let watch = editor
        .watch
        .filter(|w| agents.contains(w))
        .or_else(|| agents.first().copied());
    editor.agents = agents;
    let Some(bits) = watch else {
        editor.live = None;
        return;
    };
    let Some(entity) = Entity::try_from_bits(bits) else {
        editor.live = None;
        return;
    };
    BtDebugger::attach(play, entity);
    let running = play.get::<BehaviorTree>(entity).map(|t| t.def.clone());
    let snapshot = play.resource::<BtDebugger>().snapshot(entity).cloned();
    editor.live = match (running, snapshot) {
        (Some(def), Some(snap)) => Some(LiveView {
            entity: bits,
            ticks: snap.ticks,
            last_status: snap.last_status,
            node_status: snap.node_status,
            changed_at: snap.changed_at,
            matches_document: def == editor.doc,
        }),
        _ => None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ai::DefKind;

    fn add(kind: DefKind, parent: Option<usize>) -> BtOp {
        BtOp::AddNode {
            kind,
            name: None,
            parent,
            index: None,
            task: None,
            key: None,
            seconds: None,
            times: None,
            policy: None,
            abort: None,
            op: None,
            value_kind: None,
            literal: None,
            literal_hi: None,
            rhs_key: None,
        }
    }

    #[test]
    fn edits_undo_redo_through_the_shared_stack() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(BtEditor::default)
            .new_tree(None);
        edit(&mut world, "add root", None, &[add(DefKind::Selector, None)]).expect("adds");
        edit(&mut world, "add child", None, &[add(DefKind::Sequence, Some(0))]).expect("adds");
        assert_eq!(world.resource::<BtEditor>().doc.nodes.len(), 2);
        crate::command::undo_world(&mut world);
        assert_eq!(world.resource::<BtEditor>().doc.nodes.len(), 1);
        crate::command::undo_world(&mut world);
        assert!(world.resource::<BtEditor>().doc.nodes.is_empty());
        crate::command::redo_world(&mut world);
        crate::command::redo_world(&mut world);
        assert_eq!(world.resource::<BtEditor>().doc.nodes.len(), 2);
    }

    #[test]
    fn a_refused_batch_adds_no_undo_step() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(BtEditor::default)
            .new_tree(None);
        let err = edit(&mut world, "bad", None, &[add(DefKind::Leaf, Some(3))]).expect_err("refused");
        assert_eq!(err.error.code, "bad_node");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    }

    #[test]
    fn same_key_edits_merge_into_one_step_and_an_old_session_does_nothing() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(BtEditor::default)
            .new_tree(None);
        edit(&mut world, "add", None, &[add(DefKind::Cooldown, None)]).expect("adds");
        let set = |s: f32| BtOp::SetNode {
            node: 0,
            name: None,
            kind: None,
            key: None,
            seconds: Some(s),
            times: None,
            clear_times: false,
            task: None,
            policy: None,
            abort: None,
            op: None,
            value_kind: None,
            literal: None,
            literal_hi: None,
            clear_literal: false,
            clear_literal_hi: false,
            rhs_key: None,
        };
        for s in [1.0, 1.5, 2.0] {
            edit(&mut world, "seconds", Some("sec:0".into()), &[set(s)]).expect("sets");
        }
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            2,
            "three drags, one step"
        );
        world.resource_mut::<BtEditor>().new_tree(None);
        crate::command::undo_world(&mut world);
        assert!(
            world.resource::<BtEditor>().doc.nodes.is_empty(),
            "the old tree's undo is ignored"
        );
    }

    #[test]
    fn save_writes_canonical_text_and_open_reads_it_back() {
        let dir = regia_test_utils::unique_temp_dir("bt-editor-save");
        let path = dir.join("ai").join("guard.bt.ron");
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(BtEditor::default)
            .new_tree(Some(path.clone()));
        edit(
            &mut world,
            "build",
            None,
            &[
                add(DefKind::Selector, None),
                add(DefKind::Sequence, Some(0)),
                add(DefKind::Sequence, Some(0)),
            ],
        )
        .expect("builds");
        let saved_doc = world.resource::<BtEditor>().doc.clone();
        assert!(world.resource::<BtEditor>().dirty());
        world.resource_mut::<BtEditor>().save(None, true).expect("saves");
        assert!(!world.resource::<BtEditor>().dirty());
        let mut reopened = BtEditor::default();
        reopened.open(&path).expect("opens");
        assert_eq!(reopened.doc, saved_doc);
        let err = reopened
            .save(Some(&dir.join("x.ron")), true)
            .expect_err("wrong extension");
        assert!(err.contains(".bt.ron"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
