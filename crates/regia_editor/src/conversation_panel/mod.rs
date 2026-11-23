// SPDX-License-Identifier: MPL-2.0
//! The conversation graph editor (spec 56 § 13, ADR-3298): a panel over `<id>.conv.ron` that
//! shows the steps as a graph (blocks between labels, branch nodes, End nodes), lays it out
//! automatically, edits it with undo, draws the `conv_*` validator's findings on the nodes and
//! previews a run from any node with a mocked condition table.
//!
//! State lives in the [`ConvEditor`] world resource so the panel and the agent protocol
//! (`editor.conversation_*`, [`agent`]) edit the same document. The document is its file text:
//! opening keeps the bytes as they are, every edit writes the asset's canonical form
//! (`regia_framework::conversation::asset::to_ron`) and is a [`ConvEdit`] snapshot of the text and
//! the layout sidecar on the editor's shared [`UndoStack`]. So saving without edits writes the
//! bytes that were read, and undoing an edit restores the exact bytes before it.

pub mod agent;
pub(crate) mod autosave;
pub mod check;
pub mod edit;
pub mod graph;
pub mod layout;
pub mod panel;
pub mod preview;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_framework::conversation::asset::to_ron;
use regia_framework::conversation::{ConversationAsset, Step, parse_conversation};

use crate::command::{EditorCommand, UndoStack};

use check::{NodeIssue, ProjectContext};
use edit::{ConvOp, OpError};
use graph::ConvGraph;
use layout::{Layout, LayoutSidecar};
use preview::Preview;

/// What one undo step restores: the file text and the layout sidecar.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// The conversation file text.
    pub text: String,
    /// The dragged positions.
    pub sidecar: LayoutSidecar,
}

/// The conversation editor's world resource.
#[derive(Resource, Default)]
pub struct ConvEditor {
    /// The open file (`None`: nothing open).
    pub path: Option<PathBuf>,
    /// The document: file text and positions.
    doc: Snapshot,
    /// The document as last opened or saved.
    saved: Option<Snapshot>,
    /// The parsed document (`None` when the text does not parse).
    pub asset: Option<ConversationAsset>,
    /// Why the file does not parse (`path:line:col: message`); the editor is read-only then.
    pub parse_error: Option<String>,
    /// Bumped by every document change.
    pub revision: u64,
    /// Which document is open: undo entries of another document do nothing.
    session: u64,
    /// The project around the file.
    pub project: ProjectContext,
    /// The graph of the document.
    pub graph: ConvGraph,
    /// Its layout.
    pub layout: Layout,
    /// The findings, with their nodes.
    pub issues: Vec<NodeIssue>,
    /// The selected node id.
    pub selected: Option<String>,
    /// The search text.
    pub search: String,
    /// The preview run, if one is going.
    pub preview: Option<Preview>,
    /// The preview's condition table.
    pub conditions: BTreeMap<String, bool>,
    /// The locale the preview resolves text and clips in (empty: the source locale).
    pub preview_locale: String,
    /// The voice clip of the line the preview shows, and whether it was sent to the output.
    pub preview_voice: Option<(String, bool)>,
    /// Last action's status line.
    pub toast: String,
    /// Panel-only state (view, text fields, drags).
    pub ui: panel::PanelState,
}

impl std::fmt::Debug for ConvEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvEditor")
            .field("path", &self.path)
            .field("revision", &self.revision)
            .field("read_only", &self.read_only())
            .finish()
    }
}

/// Why an edit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No conversation is open.
    NotOpen,
    /// The file did not parse; it opens read-only.
    ReadOnly(String),
    /// An op did not fit the document.
    Op(OpError),
    /// The edited asset could not be written as text.
    Serialize(String),
}

impl EditError {
    /// The lowercase reason code.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotOpen => "no_conversation_open",
            Self::ReadOnly(_) => "read_only",
            Self::Op(e) => e.reason,
            Self::Serialize(_) => "serialize_failed",
        }
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOpen => f.write_str("no conversation is open"),
            Self::ReadOnly(e) => write!(f, "the conversation is read-only: it does not parse ({e})"),
            Self::Op(e) => write!(f, "op {}: {}", e.index, e.message),
            Self::Serialize(e) => write!(f, "cannot write the conversation: {e}"),
        }
    }
}

impl ConvEditor {
    /// Whether a conversation is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.path.is_some()
    }

    /// Whether the open file is read-only (it does not parse).
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.parse_error.is_some()
    }

    /// Whether the document differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.doc)
    }

    /// The document's file text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.doc.text
    }

    /// The dragged positions.
    #[must_use]
    pub fn sidecar(&self) -> &LayoutSidecar {
        &self.doc.sidecar
    }

    /// Where the layout sidecar of the open file lives.
    #[must_use]
    pub fn sidecar_path(&self) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        Some(layout::sidecar_path(path, self.project.root.as_deref()))
    }

    /// Opens a `.conv.ron`. A file that does not parse opens read-only with the error in
    /// [`Self::parse_error`]. `root` is the project root (found from the path when `None`).
    ///
    /// # Errors
    ///
    /// The file cannot be read.
    pub fn open(&mut self, path: &Path, root: Option<&Path>) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let root = check::find_root(path, root);
        self.project = ProjectContext::load(root);
        self.path = Some(path.to_path_buf());
        let mut toast = format!("opened {}", path.display());
        let sidecar = match self.sidecar_path() {
            Some(p) => LayoutSidecar::load(&p).unwrap_or_else(|e| {
                toast = format!("{toast} (layout positions ignored: {e})");
                LayoutSidecar::default()
            }),
            None => LayoutSidecar::default(),
        };
        self.doc = Snapshot { text, sidecar };
        self.saved = Some(self.doc.clone());
        self.session += 1;
        self.selected = None;
        self.preview = None;
        self.preview_voice = None;
        self.ui = panel::PanelState::default();
        self.refresh();
        if let Some(e) = &self.parse_error {
            toast = format!("opened {} read-only: {e}", path.display());
        }
        self.toast = toast;
        Ok(())
    }

    /// Re-derives the asset, graph, layout and findings from the document.
    pub fn refresh(&mut self) {
        let _span = tracing::debug_span!(target: "regia_editor", "conversation_refresh").entered();
        self.revision += 1;
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match parse_conversation(&name, &self.doc.text) {
            Ok(asset) => {
                self.parse_error = None;
                self.graph = ConvGraph::build(&asset);
                self.layout = layout::layout(&self.graph, &self.doc.sidecar);
                self.issues = check::check(&asset, &self.graph, &self.project);
                self.asset = Some(asset);
            }
            Err(e) => {
                self.parse_error = Some(e.to_string());
                self.asset = None;
                self.graph = ConvGraph::default();
                self.layout = Layout::default();
                self.issues = Vec::new();
            }
        }
        if let Some(sel) = &self.selected
            && self.graph.node_index(sel).is_none()
        {
            self.selected = None;
        }
    }

    /// Saves the document (to `path`, or where it was opened) and the layout sidecar when it
    /// has positions, both atomically. Returns the path.
    ///
    /// # Errors
    ///
    /// No path, a path that is not a `.conv.ron`, or a failed write.
    pub fn save(&mut self, path: Option<&Path>) -> Result<PathBuf, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.path.clone())
            .ok_or_else(|| "no conversation is open".to_string())?;
        if !path.to_string_lossy().ends_with(".conv.ron") {
            return Err(format!("{} is not a .conv.ron path", path.display()));
        }
        write_atomic(&path, &self.doc.text)?;
        self.path = Some(path.clone());
        if let Some(sidecar_path) = self.sidecar_path()
            && (!self.doc.sidecar.positions.is_empty() || sidecar_path.exists())
        {
            write_atomic(&sidecar_path, &self.doc.sidecar.to_ron()?)?;
        }
        self.saved = Some(self.doc.clone());
        self.toast = format!("saved {}", path.display());
        Ok(path)
    }

    /// The nodes whose label, line ids, speakers, text keys, resolved text, event names, option
    /// keys or conditions contain `query` (case-insensitive), in node order.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        let Some(asset) = &self.asset else {
            return Vec::new();
        };
        if q.is_empty() {
            return Vec::new();
        }
        let hit = |s: &str| s.to_lowercase().contains(&q);
        self.graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                self.graph.steps[node.first..node.end]
                    .iter()
                    .any(|step| match step {
                        Step::Line(id) => {
                            hit(id)
                                || asset.line(id).is_some_and(|l| {
                                    hit(&l.speaker)
                                        || hit(&l.text)
                                        || self.project.text(&l.text).is_some_and(|t| hit(&t))
                                })
                        }
                        Step::Label(n) | Step::Goto(n) | Step::Event(n) => hit(n),
                        Step::If { cond, then, else_ } => hit(cond) || hit(then) || hit(else_),
                        Step::Choice { prompt, options } => {
                            prompt.as_deref().is_some_and(hit)
                                || options.iter().any(|o| {
                                    hit(&o.text)
                                        || hit(&o.goto)
                                        || o.cond.as_deref().is_some_and(hit)
                                        || self.project.text(&o.text).is_some_and(|t| hit(&t))
                                })
                        }
                        Step::End => hit("end"),
                    })
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Starts a preview at node `node` (its first step).
    pub fn preview_from(&mut self, node: usize, play: bool) {
        let Some(asset) = &self.asset else {
            return;
        };
        let start = self.graph.nodes.get(node).map_or(0, |n| n.first);
        self.preview = Some(Preview::start(
            asset,
            start,
            self.conditions.clone(),
            self.project.settings.max_steps_per_frame,
        ));
        self.preview_voice_update(play);
    }

    /// Moves the preview on (see [`Preview::step`]).
    ///
    /// # Errors
    ///
    /// No preview runs, or the preview refused the step (its reason code).
    pub fn preview_step(&mut self, choose: Option<usize>, play: bool) -> Result<(), &'static str> {
        let Some(asset) = &self.asset else {
            return Err("no_conversation_open");
        };
        let preview = self.preview.as_mut().ok_or("no_preview")?;
        preview.conditions.clone_from(&self.conditions);
        preview.step(asset, choose)?;
        self.preview_voice_update(play);
        Ok(())
    }

    fn preview_locale(&self) -> String {
        if self.preview_locale.is_empty() {
            self.project.source.clone()
        } else {
            self.preview_locale.clone()
        }
    }

    fn preview_voice_update(&mut self, play: bool) {
        self.preview_voice = None;
        let (Some(asset), Some(preview)) = (&self.asset, &self.preview) else {
            return;
        };
        let preview::PreviewState::Line { line, .. } = &preview.state else {
            return;
        };
        let locale = self.preview_locale();
        let Some(clip) = self.project.clip_for(asset, line, &locale) else {
            return;
        };
        let played = play
            && self
                .project
                .root
                .as_ref()
                .is_some_and(|root| play_clip(&root.join("assets").join(&clip)));
        self.preview_voice = Some((clip, played));
    }
}

thread_local! {
    static PREVIEW_OUTPUT: std::cell::RefCell<Option<regia_audio::Audio>> =
        const { std::cell::RefCell::new(None) };
}

/// Plays a clip on the editor's preview output (any format the audio crate reads); false when it
/// cannot be read.
fn play_clip(path: &Path) -> bool {
    let Ok(sound) = regia_audio::SoundAsset::from_file_static(path) else {
        return false;
    };
    PREVIEW_OUTPUT.with(|cell| {
        let mut slot = cell.borrow_mut();
        let audio =
            slot.get_or_insert_with(|| regia_audio::Audio::new(&regia_audio::AudioSettings::default()));
        let ok = audio
            .play_asset(&sound, regia_audio::PlaybackParams::default())
            .is_ok();
        audio.drain();
        ok
    })
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}-{seq}", std::process::id()));
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot rename to {}: {e}", path.display())
    })
}

/// A snapshot edit on the shared undo stack. It belongs to the document it was made in: once
/// another conversation is open it does nothing.
pub struct ConvEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: Snapshot,
    after: Snapshot,
}

impl ConvEdit {
    fn set(world: &mut World, session: u64, doc: &Snapshot) {
        let mut editor = world.get_resource_or_insert_with(ConvEditor::default);
        if editor.session != session {
            editor.toast = "that undo step belongs to a conversation that is no longer open".into();
            return;
        }
        editor.doc = doc.clone();
        editor.refresh();
    }
}

impl EditorCommand for ConvEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<ConvEdit>() else {
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
        format!("Conversation: {}", self.label)
    }
}

fn push(world: &mut World, label: &str, merge_key: Option<String>, after: Snapshot) {
    let (before, session) = {
        let editor = world.get_resource_or_insert_with(ConvEditor::default);
        (editor.doc.clone(), editor.session)
    };
    if before == after {
        return;
    }
    let mut command = ConvEdit {
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
}

/// Applies `ops` to the open conversation as one undo step (merged with the previous step when
/// both carry the same `merge_key`, e.g. typing in one field). On error, or when nothing
/// changed, the document and the stack stay.
///
/// # Errors
///
/// Nothing open, a read-only file, or the first op that does not fit.
pub fn edit(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    ops: &[ConvOp],
) -> Result<(), EditError> {
    let after = {
        let editor = world.get_resource_or_insert_with(ConvEditor::default);
        if !editor.is_open() {
            return Err(EditError::NotOpen);
        }
        if let Some(e) = &editor.parse_error {
            return Err(EditError::ReadOnly(e.clone()));
        }
        let Some(asset) = &editor.asset else {
            return Err(EditError::NotOpen);
        };
        let edited = edit::apply_ops(asset, ops).map_err(EditError::Op)?;
        if &edited == asset {
            return Ok(());
        }
        let text = to_ron(&edited).map_err(|e| EditError::Serialize(e.to_string()))?;
        Snapshot {
            text,
            sidecar: editor.doc.sidecar.clone(),
        }
    };
    push(world, label, merge_key, after);
    Ok(())
}

/// Pins node `node` at `pos` (top-left, layout units) in the sidecar as one undo step; `None`
/// returns it to the automatic layout. Drags of one node merge into one step.
///
/// # Errors
///
/// Nothing open, a read-only file, or no such node.
pub fn move_node(world: &mut World, node: &str, pos: Option<(f32, f32)>) -> Result<(), EditError> {
    let after = {
        let editor = world.get_resource_or_insert_with(ConvEditor::default);
        if !editor.is_open() {
            return Err(EditError::NotOpen);
        }
        if let Some(e) = &editor.parse_error {
            return Err(EditError::ReadOnly(e.clone()));
        }
        if editor.graph.node_index(node).is_none() {
            return Err(EditError::Op(OpError {
                index: 0,
                reason: "bad_node",
                message: format!("no node `{node}`"),
            }));
        }
        let mut doc = editor.doc.clone();
        match pos {
            Some(p) => {
                doc.sidecar.positions.insert(node.to_string(), p);
            }
            None => {
                doc.sidecar.positions.remove(node);
            }
        }
        doc
    };
    push(world, "move node", Some(format!("move:{node}")), after);
    Ok(())
}
