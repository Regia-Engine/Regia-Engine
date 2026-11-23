// SPDX-License-Identifier: MPL-2.0
//! Animation authoring in the editor (`docs/specs/17-animation.md` § 5): the anim graph editor
//! (blend-tree canvas, state machine canvas, blend-space point editor, layers and masks, live
//! preview, PIE debugging), the clip viewer (timeline, notifies, curves, root-motion preview)
//! and the skeleton viewer (joint tree, sockets, IK chains).
//!
//! Design and parts of the widgets are ported from Fyrox's animation, ABSM and curve editors
//! (MIT; see `docs/ported-sources.toml` and each file's header), rebuilt on egui/egui-snarl
//! and on `regia_animation`'s types.
//!
//! State lives in the [`AnimEditor`] world resource so the panels and the agent protocol
//! (`editor.anim_*`, [`agent`]) edit the same documents. Every document edit is an
//! [`AnimEdit`] snapshot command on the editor's shared [`UndoStack`], so `editor.undo` /
//! Ctrl+Z undo graph edits in order with level edits. Two documents are open at a time: the
//! graph ([`GraphModel`], saved as `.animgraph.ron` plus an editor-only
//! `.animgraph.layout.ron` of canvas positions) and the rig's authoring sidecar
//! ([`AnimEditDocument`], `.animedit.ron`).

pub mod agent;
pub(crate) mod autosave;
pub mod blendspace_view;
pub mod capture;
pub mod clip_panel;
pub mod curve_view;
pub mod demo_rig;
pub mod graph_inspector;
pub mod graph_panel;
pub mod model;
pub mod preview;
// The Retarget panel: chain mapping, alignment, side-by-side preview, batch bake (ADR-7801).
pub mod retarget_panel;
pub mod skeleton_panel;
pub mod sm_canvas;
pub mod timeline;
pub mod view2d;
// Live parameter writes, the recorder and the clip preview's playback options (P-201, P-206, P-207).
pub mod clip_preview;
pub mod live;
pub mod recorder;
// GaitRig / FootPlacement Inspector sections and the locomotion test bed (ADR-8806).
pub mod locomotion_panel;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use regia_animation::authoring::AnimEditDocument;
use regia_animation::clip::AnimationClip;
use regia_animation::debug::{GraphDebug, NodePath};
use regia_animation::eval::ClipLibrary;
use regia_animation::graph::AnimGraph;
use regia_animation::skeleton::Skeleton;
use regia_ecs::prelude::*;

use crate::command::{EditorCommand, UndoStack};

pub use model::{GraphModel, NodeId, NodeKind, Scope};

/// The two documents an edit can touch (the undo snapshot).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnimDocs {
    /// The anim graph being edited.
    pub graph: GraphModel,
    /// The rig's authoring sidecar (notifies, curves, sockets, IK chains).
    pub edits: AnimEditDocument,
}

/// Where the preview rig came from.
#[derive(Clone, Debug, PartialEq)]
pub enum RigSource {
    /// The built-in procedural biped ([`demo_rig`]).
    Demo,
    /// A glTF file (skeleton + clips imported by the asset pipeline).
    Gltf(PathBuf),
    /// An entity's `SkinnedMesh` + `Animator` in the edit world (entity bits).
    Entity(u64),
}

/// The character the editor previews: skeleton and the clips as imported (edits are applied on
/// top when the preview samples them).
#[derive(Clone, Debug)]
pub struct PreviewRig {
    /// Display name.
    pub name: String,
    /// Source.
    pub source: RigSource,
    /// The skeleton as imported.
    pub skeleton: Arc<Skeleton>,
    /// Clips as imported.
    pub clips: Vec<Arc<AnimationClip>>,
}

impl PreviewRig {
    /// The built-in demo biped.
    #[must_use]
    pub fn demo() -> Self {
        Self {
            name: "Demo biped".into(),
            source: RigSource::Demo,
            skeleton: demo_rig::skeleton(),
            clips: demo_rig::clips(),
        }
    }

    /// Imports skeleton and clips from a glTF file.
    ///
    /// # Errors
    ///
    /// The file cannot be read or has no skin.
    pub fn from_gltf(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let settings = regia_asset_pipeline::gltf_anim::GltfAnimSettings::default();
        let imported = regia_asset_pipeline::gltf_anim::import_gltf_animation(&bytes, &settings)
            .map_err(|e| format!("cannot import animation from {}: {e}", path.display()))?;
        let skin = imported
            .skins
            .into_iter()
            .next()
            .ok_or_else(|| format!("{} has no skin", path.display()))?;
        let mut skeleton = skin.skeleton;
        for socket in skin.sockets {
            skeleton.sockets.push(regia_animation::skeleton::SkeletonSocket {
                name: socket.name,
                joint: socket.joint,
                offset: socket.offset,
            });
        }
        Ok(Self {
            name: path
                .file_name()
                .map_or_else(|| "rig".to_string(), |n| n.to_string_lossy().into_owned()),
            source: RigSource::Gltf(path.to_path_buf()),
            skeleton: Arc::new(skeleton),
            clips: skin.clips.into_iter().map(Arc::new).collect(),
        })
    }

    /// The imported clip called `name`.
    #[must_use]
    pub fn clip(&self, name: &str) -> Option<&Arc<AnimationClip>> {
        self.clips.iter().find(|c| c.name == name)
    }

    /// Clip names, in import order.
    #[must_use]
    pub fn clip_names(&self) -> Vec<String> {
        self.clips.iter().map(|c| c.name.clone()).collect()
    }

    /// The clips with `edits` applied (notifies, curves, property tracks, root-motion mode).
    #[must_use]
    pub fn library(&self, edits: &AnimEditDocument) -> ClipLibrary {
        let mut library = ClipLibrary::new();
        for clip in &self.clips {
            library.insert(edited_clip(clip, edits));
        }
        library
    }

    /// The skeleton with `edits`' sockets applied.
    #[must_use]
    pub fn edited_skeleton(&self, edits: &AnimEditDocument) -> Skeleton {
        let mut skeleton = (*self.skeleton).clone();
        let _ = edits.apply_to_skeleton(&mut skeleton);
        skeleton
    }
}

/// One clip with `edits` applied.
#[must_use]
pub fn edited_clip(clip: &AnimationClip, edits: &AnimEditDocument) -> AnimationClip {
    let mut out = clip.clone();
    edits.apply_to_clip(&mut out);
    out
}

/// Which canvas / item the graph editor shows and has selected (UI state, not undoable).
#[derive(Clone, Debug, Default)]
pub struct AnimUi {
    /// The canvas shown: the root, or a state's sub-graph.
    pub scope: Option<Scope>,
    /// The state machine whose state canvas is shown (`None` = node canvas).
    pub machine_view: Option<NodeId>,
    /// Selected node.
    pub selected_node: Option<NodeId>,
    /// Selected state (on the machine canvas).
    pub selected_state: Option<String>,
    /// Selected transition index (on the machine canvas).
    pub selected_transition: Option<usize>,
    /// Clip open in the clip viewer.
    pub clip: Option<String>,
    /// Clip viewer playhead, seconds.
    pub scrub: f32,
    /// Clip viewer playback.
    pub clip_playing: bool,
    /// Selected notify index on the clip timeline.
    pub selected_notify: Option<usize>,
    /// Curve open in the curve editor: `(is_property_track, name/path)`.
    pub curve: Option<(bool, String)>,
    /// Selected key in the curve editor.
    pub selected_key: Option<usize>,
    /// Selected joint in the skeleton viewer.
    pub selected_joint: Option<u32>,
    /// Snarl canvas cache: `(scope, revision)` it was built for.
    pub(crate) snarl_key: Option<(Scope, u64)>,
    /// The snarl canvas (rebuilt from the model when the key changes).
    pub(crate) snarl: Option<egui_snarl::Snarl<graph_panel::CanvasNode>>,
    /// Curve editor view.
    pub(crate) curve_view: view2d::View2d,
    /// Timeline view.
    pub(crate) timeline_view: view2d::View2d,
    /// The keyboard route of the node canvas: its Nodes list, palette and connect form (FU-5).
    pub(crate) keys: crate::canvas_keys::GraphKeys,
    /// The node the Nodes list focus belongs to (the focus follows it when rows move).
    pub(crate) keys_node: Option<NodeId>,
    /// The keyboard route of the state machine canvas: its States list (FU-5).
    pub(crate) state_keys: crate::canvas_keys::GraphKeys,
    /// The row the States list focus belongs to.
    pub(crate) state_keys_row: Option<sm_canvas::SmRow>,
    /// The clip timeline's notify list (FU-5).
    pub(crate) notify_keys: crate::canvas_keys::ListKeys,
    /// The clip preview's playback range, speed, loop mode, onion skins and floor.
    pub clip_preview: clip_preview::ClipPreviewOptions,
    /// The clip picker's search text and sort.
    pub clip_list: clip_preview::ClipListOptions,
    /// The multi-curve view of the clip's curves (visibility, locks).
    pub curves: curve_view::MultiCurve,
}

/// The animation editor's world resource.
#[derive(Resource, Default)]
pub struct AnimEditor {
    /// The open documents (edits go through [`edit`]).
    pub docs: AnimDocs,
    /// The graph's file, when one is open.
    pub graph_path: Option<PathBuf>,
    /// The sidecar's file, when one is open.
    pub edits_path: Option<PathBuf>,
    /// Graph as last saved/opened (dirty tracking).
    saved_graph: Option<GraphModel>,
    /// Sidecar as last saved/opened.
    saved_edits: Option<AnimEditDocument>,
    /// The preview character.
    pub rig: Option<PreviewRig>,
    /// Preview playback and parameter values.
    pub preview: preview::PreviewState,
    /// UI state.
    pub ui: AnimUi,
    /// Status line of the last action.
    pub toast: String,
    /// Bumped on every document change (edit, undo, redo, open).
    pub revision: u64,
    /// The latest PIE debug snapshot of an animator running this graph (see [`sync_pie_debug`]).
    pub pie_debug: Option<PieDebug>,
    /// Live parameter writes to the watched play animator ([`live`]).
    pub live: live::LiveParams,
    /// The recording of the watched animator ([`recorder`]).
    pub recorder: recorder::AnimRecorder,
    /// The locomotion test bed of the preview (ADR-8806): a gait walking over a ground preset.
    pub locomotion: locomotion_panel::LocomotionTestBed,
}

/// A PIE animator's snapshot, mapped onto the model.
#[derive(Clone, Debug)]
pub struct PieDebug {
    /// The play-world entity (bits).
    pub entity: u64,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// The runtime snapshot.
    pub debug: GraphDebug,
}

/// The editor-only layout sidecar: canvas positions keyed by runtime node path.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphLayoutFile {
    /// Format version (1).
    pub format_version: u32,
    /// Node and state positions.
    pub positions: BTreeMap<NodePath, [f32; 2]>,
}

/// The layout sidecar path of a graph file: `x.animgraph.ron` → `x.animgraph.layout.ron`.
#[must_use]
pub fn layout_path(graph: &Path) -> PathBuf {
    let name = graph
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".ron").unwrap_or(&name);
    graph.with_file_name(format!("{stem}.layout.ron"))
}

/// The authoring sidecar path of a rig source: `Fox.glb` → `Fox.glb.animedit.ron`.
#[must_use]
pub fn animedit_path(source: &Path) -> PathBuf {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    source.with_file_name(format!("{name}.animedit.ron"))
}

/// Atomic document write; the retry/fallback and temp cleanup live in
/// [`crate::atomic_write`] (sprint audit g, finding 1).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    crate::atomic_write::write_atomic(path, text.as_bytes())
}

impl AnimEditor {
    /// Whether the graph differs from the file.
    #[must_use]
    pub fn graph_dirty(&self) -> bool {
        self.saved_graph.as_ref() != Some(&self.docs.graph)
    }

    /// Whether the authoring sidecar differs from the file.
    #[must_use]
    pub fn edits_dirty(&self) -> bool {
        self.saved_edits.as_ref() != Some(&self.docs.edits)
    }

    /// Starts a new, empty graph to be saved at `path`.
    pub fn new_graph(&mut self, path: Option<PathBuf>) {
        self.docs.graph = GraphModel::default();
        self.graph_path = path;
        self.saved_graph = None;
        self.ui.scope = Some(Scope::Root);
        self.ui.machine_view = None;
        self.ui.selected_node = None;
        self.revision += 1;
        self.toast = "new graph".into();
    }

    /// Opens a `.animgraph.ron` (and its layout sidecar when present).
    ///
    /// # Errors
    ///
    /// Read, parse or validation failure.
    pub fn open_graph(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let graph =
            AnimGraph::from_ron_str(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        let layout = std::fs::read_to_string(layout_path(path))
            .ok()
            .and_then(|t| ron::from_str::<GraphLayoutFile>(&t).ok())
            .map(|l| l.positions)
            .unwrap_or_default();
        self.docs.graph = GraphModel::from_graph(&graph, &layout);
        self.saved_graph = Some(self.docs.graph.clone());
        self.graph_path = Some(path.to_path_buf());
        self.ui.scope = Some(Scope::Root);
        self.ui.machine_view = None;
        self.ui.selected_node = None;
        self.revision += 1;
        self.toast = format!("opened {}", path.display());
        Ok(())
    }

    /// Saves the graph (validated) to `path` or the open path, plus the layout sidecar.
    /// Returns the nodes that were not saved because nothing reaches them.
    ///
    /// # Errors
    ///
    /// No path, an invalid graph, or IO failure.
    pub fn save_graph(&mut self, path: Option<&Path>) -> Result<Vec<NodeId>, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.graph_path.clone())
            .ok_or_else(|| "no graph path: pass one to save as".to_string())?;
        let compiled = self.docs.graph.compile_validated()?;
        let text = compiled.graph.to_ron_string().map_err(|e| e.to_string())?;
        write_atomic(&path, &text)?;
        let layout = GraphLayoutFile {
            format_version: 1,
            positions: self.docs.graph.layout(),
        };
        let layout_text =
            ron::ser::to_string_pretty(&layout, regia_core::ron_pretty()).map_err(|e| e.to_string())?;
        write_atomic(&layout_path(&path), &layout_text)?;
        self.graph_path = Some(path.clone());
        self.saved_graph = Some(self.docs.graph.clone());
        self.toast = if compiled.unreachable.is_empty() {
            format!("saved {}", path.display())
        } else {
            format!(
                "saved {} ({} unconnected node(s) not saved)",
                path.display(),
                compiled.unreachable.len()
            )
        };
        Ok(compiled.unreachable)
    }

    /// Sets the preview rig and opens its authoring sidecar (`<source>.animedit.ron`, empty
    /// when absent; `sidecar` overrides the path).
    ///
    /// # Errors
    ///
    /// The sidecar exists but does not parse.
    pub fn set_rig(&mut self, rig: PreviewRig, sidecar: Option<PathBuf>) -> Result<(), String> {
        let sidecar = sidecar.or_else(|| match &rig.source {
            RigSource::Gltf(path) => Some(animedit_path(path)),
            _ => None,
        });
        let doc = match &sidecar {
            Some(path) if path.is_file() => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                AnimEditDocument::from_ron_str(&text)
                    .map_err(|e| format!("cannot load {}: {e}", path.display()))?
            }
            _ => AnimEditDocument::default(),
        };
        self.docs.edits = doc;
        self.saved_edits = sidecar
            .as_ref()
            .filter(|p| p.is_file())
            .map(|_| self.docs.edits.clone());
        self.edits_path = sidecar;
        self.ui.clip = rig.clips.first().map(|c| c.name.clone());
        self.ui.scrub = 0.0;
        self.ui.selected_joint = None;
        self.toast = format!("previewing {}", rig.name);
        self.rig = Some(rig);
        self.preview.reset();
        self.revision += 1;
        Ok(())
    }

    /// Saves the authoring sidecar to `path` or its open path.
    ///
    /// # Errors
    ///
    /// No path or IO failure.
    pub fn save_edits(&mut self, path: Option<&Path>) -> Result<PathBuf, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.edits_path.clone())
            .ok_or_else(|| "no .animedit.ron path: pass one to save as".to_string())?;
        let text = self.docs.edits.to_ron_string().map_err(|e| e.to_string())?;
        write_atomic(&path, &text)?;
        self.edits_path = Some(path.clone());
        self.saved_edits = Some(self.docs.edits.clone());
        self.toast = format!("saved {}", path.display());
        Ok(path)
    }

    /// The current rig, or the demo rig when none is loaded.
    #[must_use]
    pub fn rig_or_demo(&mut self) -> &PreviewRig {
        self.rig.get_or_insert_with(PreviewRig::demo)
    }
}

/// A snapshot edit of [`AnimDocs`] on the shared undo stack.
pub struct AnimEdit {
    label: String,
    merge_key: Option<String>,
    before: AnimDocs,
    after: AnimDocs,
}

impl AnimEdit {
    fn set(world: &mut World, docs: &AnimDocs) {
        let mut editor = world.get_resource_or_insert_with(AnimEditor::default);
        editor.docs = docs.clone();
        editor.revision += 1;
    }
}

impl EditorCommand for AnimEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<AnimEdit>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Anim: {}", self.label)
    }
}

/// Applies `change` to a copy of the documents; on success the copy becomes current and an
/// undo entry is pushed (merged with the previous one when both carry the same `merge_key`,
/// e.g. a node drag). On error nothing changes.
///
/// # Errors
///
/// Whatever `change` returns.
pub fn edit<R>(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    change: impl FnOnce(&mut AnimDocs) -> Result<R, String>,
) -> Result<R, String> {
    let before = world
        .get_resource_or_insert_with(AnimEditor::default)
        .docs
        .clone();
    let mut after = before.clone();
    let result = change(&mut after)?;
    if after == before {
        return Ok(result);
    }
    let mut command = AnimEdit {
        label: label.to_string(),
        merge_key,
        before,
        after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(result)
}

/// The body of a [`PendingEdit`].
pub type EditFn = Box<dyn FnOnce(&mut AnimDocs) -> Result<(), String> + Send>;

/// A deferred edit collected while a panel draws (panels borrow the editor immutably while
/// drawing, then apply their edits).
pub struct PendingEdit {
    /// Undo label.
    pub label: String,
    /// Coalescing key.
    pub merge_key: Option<String>,
    /// The change.
    pub change: EditFn,
}

impl PendingEdit {
    /// A pending edit.
    pub fn new(
        label: impl Into<String>,
        merge_key: Option<String>,
        change: impl FnOnce(&mut AnimDocs) -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            merge_key,
            change: Box::new(change),
        }
    }
}

/// Applies pending edits in order; the first failure lands in the toast.
pub fn apply_pending(world: &mut World, pending: Vec<PendingEdit>) {
    for p in pending {
        if let Err(err) = edit(world, &p.label, p.merge_key, p.change) {
            world.get_resource_or_insert_with(AnimEditor::default).toast = err;
        }
    }
}

/// Copies the debug snapshot of the play world's animator that runs the open graph (or, when
/// none matches, the first animator with a graph) into [`AnimEditor::pie_debug`]. Call once a
/// frame while PIE runs; `None` clears it. It also applies the queued live parameter writes
/// ([`AnimEditor::live`]) to the animator it watches and feeds the recorder
/// ([`AnimEditor::recorder`]); a replayed recorded frame stands in for the live snapshot (ADR-7800).
#[tracing::instrument(target = "regia_editor", skip_all, name = "anim_sync_pie_debug")]
pub fn sync_pie_debug(edit_world: &mut World, mut play_world: Option<&mut World>) {
    let _span = tracing::info_span!("anim_sync_pie_debug").entered();
    // Live parameter writes reach the animator watched last frame, before this frame's snapshot
    // (so it shows them); play stopping clears them (P-201).
    if let Some(mut editor) = edit_world.get_resource_mut::<AnimEditor>() {
        let editor = &mut *editor;
        let watched = editor
            .pie_debug
            .as_ref()
            .filter(|_| editor.recorder.scrub().is_none())
            .map(|p| p.entity);
        match (play_world.as_deref_mut(), watched) {
            (Some(play), Some(entity)) => {
                editor.live.apply(play, entity);
            }
            (None, _) => editor.live.clear(),
            (Some(_), None) => {}
        }
    }
    let compiled = edit_world
        .get_resource::<AnimEditor>()
        .and_then(|e| e.docs.graph.compile().ok())
        .map(|c| c.graph.root);
    let found = play_world.as_deref_mut().and_then(|play| {
        let mut query = play.query::<(Entity, &regia_animation::Animator)>();
        let mut fallback = None;
        let mut matched = None;
        for (entity, animator) in query.iter(play) {
            let Some(debug) = animator.debug_snapshot() else {
                continue;
            };
            let candidate = PieDebug {
                entity: entity.to_bits(),
                name: Some(crate::outliner::display_name(play, entity)),
                debug,
            };
            let same = compiled
                .as_ref()
                .zip(animator.graph_def())
                .is_some_and(|(root, graph)| graph.root == *root);
            if same {
                matched = Some(candidate);
                break;
            }
            if fallback.is_none() {
                fallback = Some(candidate);
            }
        }
        matched.or(fallback)
    });
    if let Some(mut editor) = edit_world.get_resource_mut::<AnimEditor>() {
        let editor = &mut *editor;
        // The recorder (P-206): a new play frame is recorded; a stopped session stops it; a
        // scrubbed frame stands in for the live snapshot.
        match (play_world.as_deref(), found.as_ref()) {
            (Some(play), Some(pie)) => {
                editor.recorder.capture(play, pie);
            }
            (None, _) => editor.recorder.stop(),
            (Some(_), None) => {}
        }
        editor.pie_debug = editor
            .recorder
            .scrub_frame()
            .map(recorder::RecordedFrame::pie_debug)
            .or(found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::params::ParamValue;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<AnimEditor>();
        world.init_resource::<UndoStack>();
        world
    }

    #[test]
    fn edits_undo_redo_and_coalesce_on_the_shared_stack() {
        let mut world = world();
        let id = edit(&mut world, "add clip", None, |d| {
            d.graph.add_node(
                NodeKind::default_of("clip").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
        })
        .expect("edit");
        for x in [10.0, 20.0, 30.0] {
            edit(&mut world, "move", Some(format!("move:{id}")), |d| {
                d.graph.move_node(id, [x, 0.0])
            })
            .expect("edit");
        }
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2, "drags coalesce");
        assert_eq!(
            world
                .resource::<AnimEditor>()
                .docs
                .graph
                .node(id)
                .expect("node")
                .pos,
            [30.0, 0.0]
        );
        // A failing edit changes nothing and pushes nothing.
        assert!(edit(&mut world, "bad", None, |d| d.graph.move_node(999, [0.0, 0.0])).is_err());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);

        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world
                .resource::<AnimEditor>()
                .docs
                .graph
                .node(id)
                .expect("node")
                .pos,
            [0.0, 0.0]
        );
        assert!(crate::command::undo_world(&mut world));
        assert!(world.resource::<AnimEditor>().docs.graph.nodes.is_empty());
        assert!(crate::command::redo_world(&mut world));
        assert!(world.resource::<AnimEditor>().docs.graph.node(id).is_ok());
        assert_eq!(
            world.resource::<UndoStack>().next_undo().as_deref(),
            Some("Anim: add clip")
        );
        // A no-op edit pushes nothing.
        let depth = world.resource::<UndoStack>().undo_depth();
        edit(&mut world, "noop", None, |_| Ok(())).expect("edit");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
    }

    #[test]
    fn save_open_round_trip_with_layout_and_sidecar() {
        let dir = std::env::temp_dir().join(format!("regia-anim-editor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("hero.animgraph.ron");
        let mut world = world();
        let id = edit(&mut world, "build", None, |d| {
            d.graph.set_param("Speed", ParamValue::Float(0.0));
            let id = d.graph.add_node(
                NodeKind::Clip(regia_animation::graph::ClipNode {
                    clip: "Walk".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                }),
                Scope::Root,
                [42.0, 7.0],
            )?;
            d.graph.set_output(&Scope::Root, Some(id))?;
            Ok(id)
        })
        .expect("edit");
        let _ = id;
        let mut editor = world.resource_mut::<AnimEditor>();
        assert!(editor.graph_dirty());
        let unsaved = editor.save_graph(Some(&path)).expect("save");
        assert!(unsaved.is_empty());
        assert!(!editor.graph_dirty());
        assert!(layout_path(&path).ends_with("hero.animgraph.layout.ron"));
        assert!(layout_path(&path).is_file());

        let mut fresh = AnimEditor::default();
        fresh.open_graph(&path).expect("open");
        let root = fresh.docs.graph.output.expect("output");
        assert_eq!(
            fresh.docs.graph.node(root).expect("node").pos,
            [42.0, 7.0],
            "layout restored"
        );
        assert!(!fresh.graph_dirty());

        // Rig sidecar: a glTF-less rig with an explicit sidecar path.
        let sidecar = dir.join("demo.animedit.ron");
        fresh
            .set_rig(PreviewRig::demo(), Some(sidecar.clone()))
            .expect("rig");
        fresh.docs.edits.clip_mut("Walk").notifies.clear();
        assert!(fresh.edits_dirty());
        fresh.save_edits(None).expect("save edits");
        assert!(!fresh.edits_dirty());
        let mut again = AnimEditor::default();
        again.set_rig(PreviewRig::demo(), Some(sidecar)).expect("rig");
        assert!(
            again
                .docs
                .edits
                .clip("Walk")
                .is_some_and(|c| c.notifies.is_empty())
        );
        let library = again
            .rig
            .as_ref()
            .map(|r| r.library(&again.docs.edits))
            .expect("rig");
        assert!(
            library.get("Walk").is_some_and(|c| c.notifies.is_empty()),
            "edits applied"
        );
        assert!(
            library.get("Run").is_some_and(|c| c.notifies.len() == 2),
            "others untouched"
        );
        assert_eq!(
            animedit_path(Path::new("a/Fox.glb")),
            Path::new("a/Fox.glb.animedit.ron")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_graph_failure_leaves_no_temp_file() {
        // Sprint audit g, finding 1: a save whose rename fails (on Windows: the destination
        // held open by a viewer/editor/AV) must not leave the temp file next to the graph.
        let dir = regia_test_utils::unique_temp_dir("anim-save-fail");
        let target = dir.join("hero.animgraph.ron");
        // A directory at the target path makes the rename fail deterministically on every OS.
        std::fs::create_dir(&target).expect("blocker dir");
        let mut editor = AnimEditor::default();
        let id = editor
            .docs
            .graph
            .add_node(
                NodeKind::Clip(regia_animation::graph::ClipNode {
                    clip: "Walk".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                }),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("node");
        editor
            .docs
            .graph
            .set_output(&Scope::Root, Some(id))
            .expect("output");
        let err = editor.save_graph(Some(&target)).expect_err("save must fail");
        assert!(err.contains("cannot rename"), "unexpected error: {err}");
        let residue: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp") || n.ends_with(".new"))
            .collect();
        assert!(residue.is_empty(), "temp file left behind: {residue:?}");
    }
}
