// SPDX-License-Identifier: MPL-2.0
//! The material graph editor (`docs/specs/80-matgraph-remainder.md`, `specs/22` T2.4): a node
//! canvas over `.matgraph.ron` with compile errors on the nodes they belong to and a live
//! preview sphere.
//!
//! State lives in the [`MatGraphEditor`] world resource so the panel and the agent protocol
//! (`editor.matgraph_*`, [`agent`]) edit the same document. Every document edit is a
//! [`MatGraphEdit`] snapshot command on the editor's shared [`UndoStack`], so `editor.undo` and
//! Ctrl+Z revert graph edits in order with level edits. Canvas positions are saved to an
//! editor-only `<graph>.matgraph.layout.ron` sidecar ([`GraphLayoutFile`]).

pub mod agent;
pub mod agent_s2;
pub(crate) mod autosave;
pub mod canvas;
pub mod clipboard;
pub mod code_editor;
pub mod function;
pub mod inspector;
pub mod model;
pub mod node_preview;
pub mod panel;
pub mod preview;
pub mod search;
pub mod wire_hit;
// w12 parity (ADR-7851): parameters panel, reparent and hierarchy, per-profile stats, usages.
pub mod agent_w12;
pub mod cost;
pub mod hierarchy;
pub mod params_panel;
/// w13: compile errors of the open documents as a Problems panel source (P-185).
pub mod problems;
pub mod usages;

#[cfg(test)]
mod s2_tests;
#[cfg(test)]
mod w11_tests;
#[cfg(test)]
mod w12_tests;
#[cfg(test)]
mod w13_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use regia_ecs::prelude::*;
use regia_matgraph::{FunctionSource, MaterialFunction, MaterialGraph, MaterialInstance};

use crate::command::{EditorCommand, UndoStack};

pub use model::{
    CommentFrame, Diagnostic, Diagnostics, DocMode, FunctionMeta, LayoutNotes, MatGraphDoc, Reroute, Target,
};

/// Current layout sidecar version: 2 adds comment frames and edge reroute points (ADR-9700).
/// Version 1 files load (both lists empty); the sidecar is editor-only and not a frozen format
/// (spec 80 § 2.8).
pub const LAYOUT_FORMAT_VERSION: u32 = 2;

/// The editor-only layout sidecar: canvas positions by node name, comment frames and edge
/// reroute points. Not cooked; safe to delete.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphLayoutFile {
    /// Format version ([`LAYOUT_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Node positions.
    pub positions: BTreeMap<String, [f32; 2]>,
    /// Comment frames (v2; absent in v1).
    #[serde(default)]
    pub comments: Vec<model::CommentFrame>,
    /// Edge reroute points (v2; absent in v1).
    #[serde(default)]
    pub reroutes: Vec<model::Reroute>,
}

/// The layout sidecar path of a graph file: `x.matgraph.ron` → `x.matgraph.layout.ron`.
#[must_use]
pub fn layout_path(graph: &Path) -> PathBuf {
    let name = graph
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".ron").unwrap_or(&name);
    graph.with_file_name(format!("{stem}.layout.ron"))
}

/// Reads a layout sidecar's positions (see [`read_layout_file`]).
#[must_use]
pub fn read_layout(graph: &Path) -> BTreeMap<String, [f32; 2]> {
    read_layout_file(graph).positions
}

/// Reads a layout sidecar (v1 or v2); a missing, unreadable or newer file gives an empty
/// layout (the canvas lays the nodes out by depth). Non-finite positions, comments and reroute
/// points are dropped.
#[must_use]
pub fn read_layout_file(graph: &Path) -> GraphLayoutFile {
    let finite = |p: &[f32; 2]| p.iter().all(|v| v.is_finite());
    std::fs::read_to_string(layout_path(graph))
        .ok()
        .and_then(|t| ron::from_str::<GraphLayoutFile>(&t).ok())
        .filter(|l| l.format_version <= LAYOUT_FORMAT_VERSION)
        .map(|mut l| {
            l.positions.retain(|_, p| finite(p));
            l.comments.retain(|c| finite(&c.min) && finite(&c.max));
            l.reroutes.retain(|r| r.points.iter().all(finite));
            l
        })
        .unwrap_or_default()
}

/// A document opened with its sidecar's positions and notes.
fn with_layout(mut doc: MatGraphDoc, layout: GraphLayoutFile) -> MatGraphDoc {
    doc.layout = layout.positions;
    doc.notes = model::LayoutNotes {
        comments: layout.comments,
        reroutes: layout.reroutes,
    };
    doc
}

/// The layout sidecar a save writes for `doc`: every node's position (auto-laid ones too),
/// its comments and reroutes, at [`LAYOUT_FORMAT_VERSION`].
#[must_use]
pub fn layout_file_of(doc: &MatGraphDoc) -> GraphLayoutFile {
    GraphLayoutFile {
        format_version: LAYOUT_FORMAT_VERSION,
        positions: doc.positions(),
        comments: doc.notes.comments.clone(),
        reroutes: doc.notes.reroutes.clone(),
    }
}

/// Saves go through the shared [`crate::atomic_write`] helper: a process-unique temp file
/// (`x.matgraph.ron.<pid>.<serial>.tmp`) next to the target, then a rename, with the temp
/// removed on every failure path and a held/read-only destination retried on Windows
/// (audit G editor2-platform L2/L6; the fixed `x.ron.new` name was shared by every writer
/// process, and a failed write used to leave the temp behind).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    crate::atomic_write::write_atomic(path, text.as_bytes())
}

/// A function tab's calling graph: the graph whose declarations the function is checked against.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionContext {
    /// The calling `.matgraph.ron` (`None`: the unsaved graph it was opened from).
    pub path: Option<PathBuf>,
    /// The calling graph, its function files read.
    pub graph: MaterialGraph,
}

/// One open document that is not the active one (see [`MatGraphEditor::tabs`]).
#[derive(Default)]
struct Tab {
    path: Option<PathBuf>,
    doc: MatGraphDoc,
    saved: Option<MatGraphDoc>,
    session: u64,
    context: Option<FunctionContext>,
    parent_path: Option<PathBuf>,
    toast: String,
    ui: panel::PanelState,
}

/// What a tab holds, for the tab bar and `editor.matgraph_tabs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
    /// `graph`, `function` or `instance`.
    pub kind: &'static str,
    /// The file (`None`: not saved yet).
    pub path: Option<PathBuf>,
    /// Short label (file name, or `untitled`).
    pub label: String,
    /// Unsaved edits.
    pub dirty: bool,
    /// The active tab.
    pub active: bool,
}

/// What kind of document `doc` is, as the tabs and the agent name it.
#[must_use]
pub fn doc_kind(doc: &MatGraphDoc) -> &'static str {
    match doc.mode {
        DocMode::Graph => "graph",
        DocMode::Function(_) => "function",
        DocMode::Instance(_) => "instance",
    }
}

/// The material graph editor's world resource.
#[derive(Resource, Default)]
pub struct MatGraphEditor {
    /// The file the active document saves to (`None` for a new one not saved yet).
    pub path: Option<PathBuf>,
    /// The project root (contains `assets/`), for textures and function files.
    pub root: Option<PathBuf>,
    /// The active document.
    pub doc: MatGraphDoc,
    /// The document as last opened or saved (`None`: never saved).
    saved: Option<MatGraphDoc>,
    /// Bumped by every document change (the canvas and the diagnostics key on it).
    pub revision: u64,
    /// Which document is open: a fresh number for every document opened or started. Undo
    /// entries of a document that is closed stay on the shared stack but do nothing.
    session: u64,
    /// The last session number handed out.
    sessions: u64,
    /// Last action's status line.
    pub toast: String,
    /// Bumped when the graph itself changes (not a canvas move): see [`Self::graph_revision`].
    graph_revision: u64,
    /// The graph, mode and live inputs (other tabs it compiles against) `graph_revision` was
    /// last taken for.
    graph_seen: Option<(MaterialGraph, DocMode, LiveInputs)>,
    /// Bumped when the editor writes a function file (callers' signatures may have moved).
    files_epoch: u64,
    /// Diagnostics of (`graph_revision`, the graph's folder, session, files epoch).
    diagnostics: Option<(DiagKey, Diagnostics)>,
    /// A function tab's calling graph.
    pub context: Option<FunctionContext>,
    /// An instance tab's parent graph file.
    pub parent_path: Option<PathBuf>,
    /// The other open documents, in tab order without the active one.
    parked: Vec<Tab>,
    /// The active tab's index among all tabs.
    active_tab: usize,
    /// Autosave labels of untitled documents restored from a crash, by session (the restored
    /// tab keeps its autosave's id; `autosave` module, ADR-9770).
    autosave_labels: BTreeMap<u64, String>,
    /// Panel state (selection, canvas, text buffers, preview image).
    pub ui: panel::PanelState,
    /// The node clipboard (RON, [`clipboard::MatGraphClip`]), shared by every tab (ADR-9700).
    pub clipboard: Option<String>,
    /// The preview scene (lighting preset, environment intensity, floor, turntable) shared by
    /// every tab, as the other asset viewers have it (w13, P-083, ADR-7771).
    pub preview_scene: crate::preview_scene::PreviewContext,
    /// The parked tabs' diagnostics for the Problems panel (w13, [`problems`]).
    problems_cache: problems::ProblemsCache,
}

type DiagKey = (u64, Option<PathBuf>, u64, u64);

/// What a document compiles against besides itself, from the other open tabs (w11,
/// ADR-7730): the function tabs' definitions (a graph calling one of those files reads the
/// tab) and, for a function tab, its calling graph's tab as it is now.
#[derive(Clone, Debug, Default, PartialEq)]
struct LiveInputs {
    functions: model::LiveFunctions,
    caller: Option<MaterialGraph>,
}

/// The graph a preview draws (function files read, the function tabs' definitions first) and
/// the folder its textures resolve against (w11, ADR-7730).
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewSource {
    /// The resolved graph.
    pub graph: MaterialGraph,
    /// The folder its texture paths resolve against.
    pub dir: Option<PathBuf>,
}

impl MatGraphEditor {
    /// Whether the document differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.doc)
    }

    /// Whether this document should be listed and written on Save and exit.
    ///
    /// An empty new graph (never saved, no nodes) is not work yet: opening the panel and
    /// closing the editor must not prompt for it.
    fn needs_save(session: u64, doc: &MatGraphDoc, saved: &Option<MatGraphDoc>) -> bool {
        session > 0
            && saved.as_ref() != Some(doc)
            && !(saved.is_none() && matches!(doc.mode, DocMode::Graph) && doc.graph.nodes.is_empty())
    }

    /// Close-prompt labels for every open graph, function or instance that is not on disk.
    #[must_use]
    pub fn unsaved_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        let mut push =
            |path: &Option<PathBuf>, doc: &MatGraphDoc, saved: &Option<MatGraphDoc>, session: u64| {
                if !Self::needs_save(session, doc, saved) {
                    return;
                }
                let kind = match doc.mode {
                    DocMode::Graph => "material graph",
                    DocMode::Function(_) => "material function",
                    DocMode::Instance(_) => "material instance",
                };
                labels.push(match path {
                    Some(path) => format!("{kind} {}", path.display()),
                    None => format!("an untitled {kind}"),
                });
            };
        for tab in &self.parked {
            push(&tab.path, &tab.doc, &tab.saved, tab.session);
        }
        if self.session > 0 {
            push(&self.path, &self.doc, &self.saved, self.session);
        }
        labels
    }

    /// Writes every open document that [`Self::unsaved_labels`] names. The active tab is
    /// restored. An untitled document with real edits is an error: Save and exit must not
    /// drop it.
    ///
    /// # Errors
    ///
    /// A document has edits and no path, or a write failed.
    pub fn save_dirty_documents(&mut self) -> Result<(), String> {
        let restore = self.active_tab;
        let count = self.tabs().len();
        let saved = self.save_each_dirty_tab(count);
        if count > 0 && restore < count && self.active_tab != restore {
            // A failed tab must not leave a different document in front of the user: the tab
            // that was active before the save is active again whatever the outcome.
            let restored = self.select_tab(restore);
            return saved.and(restored);
        }
        saved
    }

    /// Saves every dirty tab in order, select failures and write failures included.
    fn save_each_dirty_tab(&mut self, count: usize) -> Result<(), String> {
        for index in 0..count {
            self.select_tab(index)?;
            if !Self::needs_save(self.session, &self.doc, &self.saved) {
                continue;
            }
            if self.path.is_none() {
                let kind = doc_kind(&self.doc);
                return Err(format!("material {kind} has unsaved edits and no path"));
            }
            self.save(None)?;
        }
        Ok(())
    }

    /// Whether opening or starting another graph would lose work: the document differs from
    /// the file, and it is not a new graph that is still empty.
    #[must_use]
    pub fn has_unsaved_work(&self) -> bool {
        self.is_open() && self.dirty() && (self.saved.is_some() || !self.doc.graph.nodes.is_empty())
    }

    /// Whether a document is open (opened, or started with [`Self::new_graph`]).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.session > 0 && (self.path.is_some() || self.saved.is_some() || self.revision > 0)
    }

    /// The directory the document lives in.
    #[must_use]
    pub fn dir(&self) -> Option<PathBuf> {
        self.path.as_deref().and_then(Path::parent).map(Path::to_path_buf)
    }

    /// The directory the graph's own files (textures, function files) resolve against: the
    /// parent graph's folder for an instance, the document's folder otherwise.
    #[must_use]
    pub fn graph_dir(&self) -> Option<PathBuf> {
        match self.doc.mode {
            // The instance's own `parent` first, so an undone reparent reads its old parent's
            // folder again (w12).
            DocMode::Instance(ref instance) => {
                let parent =
                    instance_parent_path(self.path.as_deref(), self.root.as_deref(), &instance.parent)
                        .ok()
                        .filter(|p| p.is_file())
                        .or_else(|| self.parent_path.clone());
                parent
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                    .or_else(|| self.dir())
            }
            _ => self.dir(),
        }
    }

    /// A counter that moves only when the graph (or a function's signature, or an instance's
    /// overrides) changes, not when a node is moved on the canvas: compile diagnostics, the
    /// preview and the inspector's text buffers key on it, so dragging a node does not
    /// recompile every frame.
    ///
    /// It also moves when a function tab this document calls is edited, or (for a function
    /// tab) when its calling graph's tab is (w11): the graph then compiles another definition.
    pub fn graph_revision(&mut self) -> u64 {
        let live = self.live_inputs();
        let fresh = self
            .graph_seen
            .as_ref()
            .is_some_and(|(g, m, l)| *g == self.doc.graph && *m == self.doc.mode && *l == live);
        if !fresh {
            self.graph_revision += 1;
            self.graph_seen = Some((self.doc.graph.clone(), self.doc.mode.clone(), live));
        }
        self.graph_revision
    }

    /// The other tabs this document compiles against (see [`LiveInputs`]).
    fn live_inputs(&self) -> LiveInputs {
        let functions = self
            .parked
            .iter()
            .filter_map(|t| Some((t.path.clone()?, t.doc.to_function()?)))
            .collect();
        LiveInputs {
            functions,
            caller: self.live_caller().map(|(g, _)| g.clone()),
        }
    }

    /// Every function tab's definition by the file it saves to (the active tab's too): what a
    /// graph that calls one of those files compiles (w11, ADR-7730). Unsaved function tabs
    /// without a path are not callable by file and are left out.
    #[must_use]
    pub fn live_functions(&self) -> model::LiveFunctions {
        let mut live: model::LiveFunctions = self
            .parked
            .iter()
            .filter_map(|t| Some((t.path.clone()?, t.doc.to_function()?)))
            .collect();
        if let (Some(path), Some(f)) = (&self.path, self.doc.to_function()) {
            live.insert(path.clone(), f);
        }
        live
    }

    /// For a function tab: its calling graph's open tab (matched by the context's path) and that
    /// graph's folder.
    fn live_caller(&self) -> Option<(&MaterialGraph, Option<PathBuf>)> {
        if !matches!(self.doc.mode, DocMode::Function(_)) {
            return None;
        }
        let path = self.context.as_ref()?.path.as_ref()?;
        self.parked
            .iter()
            .find(|t| matches!(t.doc.mode, DocMode::Graph) && t.path.as_ref() == Some(path))
            .map(|t| {
                (
                    &t.doc.graph,
                    t.path.as_deref().and_then(Path::parent).map(Path::to_path_buf),
                )
            })
    }

    /// A function tab's calling graph as it is now, function files read: its open tab when
    /// there is one (with the function tabs' definitions), else the graph read when the tab
    /// opened. `None` without a calling graph.
    #[must_use]
    pub fn function_context_graph(&self) -> Option<MaterialGraph> {
        if let Some((graph, dir)) = self.live_caller()
            && let Ok(resolved) = model::resolved_live(
                graph,
                dir.as_deref(),
                self.root.as_deref(),
                &self.live_functions(),
            )
        {
            return Some(resolved);
        }
        self.context.as_ref().map(|c| c.graph.clone())
    }

    /// The graph the open document compiles to for `editor.matgraph_compile` and the WGSL view:
    /// a graph or instance with its function files read (function tabs first), a function in its
    /// harness against its calling graph (`function::harness`).
    ///
    /// # Errors
    ///
    /// A function file cannot be read or parsed.
    pub fn compile_source(&self) -> Result<MaterialGraph, String> {
        match &self.doc.mode {
            DocMode::Function(meta) => {
                let name = if model::valid_node_name(&meta.name) {
                    meta.name.clone()
                } else {
                    "function".to_string()
                };
                Ok(function::harness(
                    &meta.to_function(&self.doc.graph.nodes),
                    &name,
                    self.function_context_graph().as_ref(),
                ))
            }
            _ => model::resolved_live(
                &self.doc.view_graph(),
                self.graph_dir().as_deref(),
                self.root.as_deref(),
                &self.live_functions(),
            ),
        }
    }

    /// What the preview draws (w11, ADR-7730): a graph or an instance (its parent with the
    /// overrides) with its function files read, function tabs first. A function tab previews its
    /// calling graph with the tab's definition (the caller's open tab, else its file), or, with
    /// no saved calling graph, its first output as an unlit color with its inputs at zero.
    ///
    /// # Errors
    ///
    /// A function file cannot be read, or a function has no output to show.
    pub fn preview_source(&self) -> Result<PreviewSource, String> {
        let live = self.live_functions();
        let root = self.root.as_deref();
        let DocMode::Function(meta) = &self.doc.mode else {
            let dir = self.graph_dir();
            let graph = model::resolved_live(&self.doc.view_graph(), dir.as_deref(), root, &live)?;
            return Ok(PreviewSource { graph, dir });
        };
        if let Some((graph, dir)) = self.live_caller() {
            let graph = model::resolved_live(graph, dir.as_deref(), root, &live)?;
            return Ok(PreviewSource { graph, dir });
        }
        if let Some(path) = self.context.as_ref().and_then(|c| c.path.clone()) {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read the calling graph {}: {e}", path.display()))?;
            let graph = MaterialGraph::from_ron(&text)
                .map_err(|e| format!("cannot load the calling graph {}: {e}", path.display()))?;
            let dir = path.parent().map(Path::to_path_buf);
            let graph = model::resolved_live(&graph, dir.as_deref(), root, &live)?;
            return Ok(PreviewSource { graph, dir });
        }
        let name = if model::valid_node_name(&meta.name) {
            meta.name.clone()
        } else {
            "function".to_string()
        };
        let harness = function::harness(&meta.to_function(&self.doc.graph.nodes), &name, None);
        let first = harness
            .nodes
            .keys()
            .find(|n| n.starts_with(function::HARNESS_OUTPUT_PREFIX))
            .cloned()
            .ok_or_else(|| "the function has no wired output to preview".to_string())?;
        let graph = node_preview::derived_graph(&harness, &first)?;
        Ok(PreviewSource { graph, dir: None })
    }

    /// Compile diagnostics of the current document (compiled once per graph change, and again
    /// when a save-as moves it to another folder, where its function files resolve, or when
    /// the editor saved a function file).
    pub fn diagnostics(&mut self) -> &Diagnostics {
        let key = (
            self.graph_revision(),
            self.graph_dir(),
            self.session,
            self.files_epoch,
        );
        let fresh = self.diagnostics.as_ref().is_some_and(|(k, _)| *k == key);
        if !fresh {
            let _span = tracing::info_span!(target: "regia_editor", "matgraph_diagnose").entered();
            // A graph calling a file open in a function tab compiles the tab's definition; a
            // function tab is checked against its calling graph as it is now (w11).
            let live = self.live_functions();
            let doc = if matches!(self.doc.mode, DocMode::Function(_)) || live.is_empty() {
                std::borrow::Cow::Borrowed(&self.doc)
            } else {
                let mut doc = self.doc.clone();
                doc.graph =
                    model::with_live_functions(&doc.graph, key.1.as_deref(), self.root.as_deref(), &live);
                std::borrow::Cow::Owned(doc)
            };
            let context = self.function_context_graph();
            let d = model::diagnose_doc(&doc, key.1.as_deref(), self.root.as_deref(), context.as_ref());
            self.diagnostics = Some((key, d));
        }
        match &self.diagnostics {
            Some((_, d)) => d,
            None => unreachable_diagnostics(),
        }
    }

    fn next_session(&mut self) -> u64 {
        self.sessions = self.sessions.max(self.session) + 1;
        self.sessions
    }

    fn reset_document(&mut self, doc: MatGraphDoc, path: Option<PathBuf>, saved: bool) {
        self.saved = saved.then(|| doc.clone());
        self.doc = doc;
        self.path = path;
        self.session = self.next_session();
        self.revision += 1;
        self.context = None;
        self.parent_path = None;
        self.ui = panel::PanelState::default();
    }

    /// Starts a new, empty graph (saved to `path` later).
    pub fn new_graph(&mut self, path: Option<PathBuf>) {
        let name = path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().trim_end_matches(".matgraph.ron").to_string())
            .unwrap_or_else(|| "untitled".into());
        self.reset_document(
            MatGraphDoc::new(MaterialGraph::new(name), BTreeMap::new()),
            path,
            false,
        );
        self.toast = "new graph".into();
    }

    /// Opens a `.matgraph.ron` (older versions migrate) and its layout sidecar.
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let graph =
            MaterialGraph::from_ron(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.reset_document(
            with_layout(MatGraphDoc::new(graph, BTreeMap::new()), read_layout_file(path)),
            Some(path.to_path_buf()),
            true,
        );
        self.toast = format!("opened {}", path.display());
        Ok(())
    }

    /// Starts a new material function with one `Vec3` input `x` and one output `out` that
    /// passes it through (saved to `path` later), checked against `context`.
    pub fn new_function(&mut self, path: Option<PathBuf>, context: Option<FunctionContext>) {
        let name = path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().trim_end_matches(".matfunc.ron").to_string())
            .unwrap_or_else(|| "untitled".into());
        let function = MaterialFunction {
            format_version: regia_matgraph::MATFUNC_FORMAT_VERSION,
            name,
            inputs: vec![regia_matgraph::FunctionInput {
                name: "x".into(),
                ty: regia_matgraph::ValueType::Vec3,
            }],
            nodes: BTreeMap::from([("x".to_string(), regia_matgraph::Node::Input("x".into()))]),
            outputs: BTreeMap::from([("out".to_string(), "x".to_string())]),
        };
        self.reset_document(MatGraphDoc::function(function, BTreeMap::new()), path, false);
        self.context = context;
        self.toast = "new function".into();
    }

    /// Opens a `.matfunc.ron` (older versions migrate) and its layout sidecar, checked against
    /// `context` (the graph that calls it, if known).
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed.
    pub fn open_function(&mut self, path: &Path, context: Option<FunctionContext>) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let function =
            MaterialFunction::from_ron(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.reset_document(
            with_layout(
                MatGraphDoc::function(function, BTreeMap::new()),
                read_layout_file(path),
            ),
            Some(path.to_path_buf()),
            true,
        );
        self.context = context;
        self.toast = format!("opened function {}", path.display());
        Ok(())
    }

    /// Opens a `.matinst.ron` over its parent graph (read-only, with the parent's layout).
    /// `parent` is the parent graph's file, found from the instance's `parent` field by the
    /// caller.
    ///
    /// # Errors
    ///
    /// Either file cannot be read or parsed.
    pub fn open_instance(&mut self, path: &Path, parent: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let instance =
            MaterialInstance::from_ron(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        self.open_instance_doc(Some(path.to_path_buf()), instance, parent, true)
    }

    /// Starts a new instance of `parent` (no overrides), saved to `path` later. `reference` is
    /// what the instance's `parent` field says (a project-relative path).
    ///
    /// # Errors
    ///
    /// The parent cannot be read or parsed.
    pub fn new_instance(
        &mut self,
        path: Option<PathBuf>,
        parent: &Path,
        reference: &str,
    ) -> Result<(), String> {
        self.open_instance_doc(path, MaterialInstance::new(reference), parent, false)
    }

    fn open_instance_doc(
        &mut self,
        path: Option<PathBuf>,
        instance: MaterialInstance,
        parent: &Path,
        saved: bool,
    ) -> Result<(), String> {
        let text = std::fs::read_to_string(parent)
            .map_err(|e| format!("cannot read the parent {}: {e}", parent.display()))?;
        let graph = MaterialGraph::from_ron(&text)
            .map_err(|e| format!("cannot load the parent {}: {e}", parent.display()))?;
        self.reset_document(
            with_layout(
                MatGraphDoc::instance(graph, BTreeMap::new(), instance),
                read_layout_file(parent),
            ),
            path,
            saved,
        );
        self.parent_path = Some(parent.to_path_buf());
        self.toast = format!("instance of {} (the graph is read-only)", parent.display());
        Ok(())
    }

    /// Saves the document to `path` (or the open path), atomically: a graph and its layout
    /// sidecar, a function (`.matfunc.ron`) and its layout sidecar, or an instance's
    /// `.matinst.ron` alone (never its parent). A document with compile errors is saved too
    /// (the toast says so). Returns the path.
    ///
    /// # Errors
    ///
    /// No path, or a write failed.
    pub fn save(&mut self, path: Option<&Path>) -> Result<PathBuf, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| self.path.clone())
            .ok_or_else(|| "no path: pass one to save as".to_string())?;
        let text = match &self.doc.mode {
            DocMode::Graph => self.doc.graph.to_ron().map_err(|e| e.to_string())?,
            DocMode::Function(meta) => meta
                .to_function(&self.doc.graph.nodes)
                .to_ron()
                .map_err(|e| e.to_string())?,
            DocMode::Instance(instance) => instance.to_ron().map_err(|e| e.to_string())?,
        };
        write_atomic(&path, &text)?;
        if !matches!(self.doc.mode, DocMode::Instance(_)) {
            let layout = layout_file_of(&self.doc);
            let layout_text =
                ron::ser::to_string_pretty(&layout, regia_core::ron_pretty()).map_err(|e| e.to_string())?;
            write_atomic(&layout_path(&path), &layout_text)?;
        }
        if matches!(self.doc.mode, DocMode::Function(_)) {
            // Graphs calling this file may have a new signature to show.
            self.files_epoch += 1;
        }
        self.path = Some(path.clone());
        self.saved = Some(self.doc.clone());
        let ok = self.diagnostics().ok;
        self.toast = if ok {
            format!("saved {}", path.display())
        } else {
            format!("saved {} (it does not compile yet)", path.display())
        };
        Ok(path)
    }

    fn take_active(&mut self) -> Tab {
        Tab {
            path: self.path.take(),
            doc: std::mem::take(&mut self.doc),
            saved: self.saved.take(),
            session: std::mem::take(&mut self.session),
            context: self.context.take(),
            parent_path: self.parent_path.take(),
            toast: std::mem::take(&mut self.toast),
            ui: std::mem::take(&mut self.ui),
        }
    }

    fn put_active(&mut self, tab: Tab) {
        self.path = tab.path;
        self.doc = tab.doc;
        self.saved = tab.saved;
        self.session = tab.session;
        self.context = tab.context;
        self.parent_path = tab.parent_path;
        self.toast = tab.toast;
        self.ui = tab.ui;
        self.ui.invalidate_canvas();
        self.revision += 1;
    }

    /// Makes room for another document: when one is open it moves to a tab of its own (kept
    /// with its unsaved edits) and the editor is empty until the caller opens or starts one.
    pub fn open_tab(&mut self) {
        if self.is_open() {
            let tab = self.take_active();
            let at = self.active_tab.min(self.parked.len());
            self.parked.insert(at, tab);
            self.active_tab = self.parked.len();
            self.revision += 1;
        }
    }

    /// Every open document in tab order.
    #[must_use]
    pub fn tabs(&self) -> Vec<TabInfo> {
        let info =
            |path: &Option<PathBuf>, doc: &MatGraphDoc, saved: &Option<MatGraphDoc>, active: bool| TabInfo {
                kind: doc_kind(doc),
                label: path.as_deref().and_then(Path::file_name).map_or_else(
                    || format!("untitled {}", doc_kind(doc)),
                    |n| n.to_string_lossy().into_owned(),
                ),
                path: path.clone(),
                dirty: saved.as_ref() != Some(doc),
                active,
            };
        let mut out: Vec<TabInfo> = self
            .parked
            .iter()
            .map(|t| info(&t.path, &t.doc, &t.saved, false))
            .collect();
        if self.is_open() {
            let at = self.active_tab.min(out.len());
            out.insert(at, info(&self.path, &self.doc, &self.saved, true));
        }
        out
    }

    /// Makes tab `index` the active one.
    ///
    /// # Errors
    ///
    /// No such tab.
    pub fn select_tab(&mut self, index: usize) -> Result<(), String> {
        let count = self.tabs().len();
        if index >= count {
            return Err(format!("no tab {index} ({count} open)"));
        }
        if index == self.active_tab && self.is_open() {
            return Ok(());
        }
        if self.is_open() {
            let tab = self.take_active();
            let at = self.active_tab.min(self.parked.len());
            self.parked.insert(at, tab);
        }
        let tab = self.parked.remove(index.min(self.parked.len().saturating_sub(1)));
        self.active_tab = index;
        self.put_active(tab);
        Ok(())
    }

    /// Closes the active document (unsaved edits are dropped: check [`Self::dirty`] first);
    /// the next tab becomes active. Its undo steps stay on the stack and do nothing.
    pub fn close_tab(&mut self) {
        let _ = self.take_active();
        self.revision += 1;
        if self.parked.is_empty() {
            self.active_tab = 0;
            self.toast = "closed".into();
            return;
        }
        let at = self.active_tab.min(self.parked.len() - 1);
        let tab = self.parked.remove(at);
        self.active_tab = at;
        self.put_active(tab);
    }

    /// The document of session `session`: the active one or a parked tab.
    /// Puts `doc` in the tab of `session` (the active one or a parked one) and points that tab's
    /// parent at the instance's own `parent` when it names a file, so an undone or redone
    /// reparent moves the parent with the document (the autosave and the restore read
    /// `parent_path`; review g16 row 8). `false` when no open tab has that session.
    fn restore_doc_of_session(&mut self, session: u64, doc: &MatGraphDoc) -> bool {
        let root = self.root.clone();
        let parent_of = |path: Option<&Path>| match doc.mode {
            DocMode::Instance(ref instance) => instance_parent_path(path, root.as_deref(), &instance.parent)
                .ok()
                .filter(|p| p.is_file()),
            _ => None,
        };
        if self.session == session && session > 0 {
            self.doc = doc.clone();
            if let Some(parent) = parent_of(self.path.as_deref()) {
                self.parent_path = Some(parent);
            }
            return true;
        }
        match self
            .parked
            .iter_mut()
            .find(|t| t.session == session && session > 0)
        {
            Some(tab) => {
                tab.doc = doc.clone();
                if let Some(parent) = parent_of(tab.path.as_deref()) {
                    tab.parent_path = Some(parent);
                }
                true
            }
            None => false,
        }
    }
}

/// `diagnostics()` always fills its cache before reading it; this is the type-level fallback.
fn unreachable_diagnostics() -> &'static Diagnostics {
    static EMPTY: std::sync::OnceLock<Diagnostics> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Diagnostics::default)
}

/// A snapshot edit of the [`MatGraphDoc`] on the shared undo stack. It belongs to the document
/// it was made in: once another graph is open it does nothing, so an undo can never write one
/// graph's snapshot over another (which the next save would put on disk).
pub struct MatGraphEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: MatGraphDoc,
    after: MatGraphDoc,
    /// Files the edit wrote besides the document (a collapse into a `.matfunc.ron`): undo
    /// puts back their bytes before the edit (or removes a file the edit created), redo
    /// writes them again.
    files: Vec<FileWrite>,
}

/// One file an edit wrote: its bytes before (`None`: it did not exist) and after.
#[derive(Clone, Debug)]
pub struct FileWrite {
    /// The file.
    pub path: PathBuf,
    /// Its bytes before the edit.
    pub before: Option<Vec<u8>>,
    /// Its bytes after.
    pub after: Vec<u8>,
}

impl FileWrite {
    /// A write of `after` to `path`, remembering what is there now.
    #[must_use]
    pub fn new(path: PathBuf, after: Vec<u8>) -> Self {
        let before = std::fs::read(&path).ok();
        Self { path, before, after }
    }

    fn put(&self, bytes: Option<&[u8]>) -> Result<(), String> {
        match bytes {
            Some(bytes) => write_atomic(&self.path, &String::from_utf8_lossy(bytes)),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("cannot remove {}: {e}", self.path.display())),
            },
        }
    }
}

impl MatGraphEdit {
    fn set(world: &mut World, session: u64, doc: &MatGraphDoc, files: &[FileWrite], undo: bool) {
        let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        if !editor.restore_doc_of_session(session, doc) {
            editor.toast = "that undo step belongs to a graph that is no longer open".into();
            return;
        }
        editor.revision += 1;
        let mut wrote = false;
        for file in files {
            let bytes = if undo {
                file.before.as_deref()
            } else {
                Some(file.after.as_slice())
            };
            if let Err(e) = file.put(bytes) {
                editor.toast = e;
            }
            wrote = true;
        }
        if wrote {
            editor.files_epoch += 1;
        }
    }
}

impl EditorCommand for MatGraphEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after, &self.files, false);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before, &self.files, true);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<MatGraphEdit>() else {
            return false;
        };
        if self.merge_key.is_some()
            && self.merge_key == next.merge_key
            && self.session == next.session
            && self.files.is_empty()
            && next.files.is_empty()
        {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Material graph: {}", self.label)
    }

    fn changes_level_state(&self) -> bool {
        // The graph, function or instance file is its own document. The edit stays on the
        // shared stack (undo still reaches it) without marking the level dirty, so saving
        // the level cannot clear a graph edit the level file does not contain.
        false
    }
}

/// Applies `change` to a copy of the document; on success the copy becomes current and an
/// undo entry is pushed (merged with the previous one when both carry the same `merge_key`,
/// e.g. a node drag). On error, or when nothing changed, the document and the stack stay.
///
/// # Errors
///
/// Whatever `change` returns.
pub fn edit<R>(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    change: impl FnOnce(&mut MatGraphDoc) -> Result<R, String>,
) -> Result<R, String> {
    edit_with_files(world, label, merge_key, change, Vec::new)
}

/// [`edit`] that also writes files (`files` runs once the change succeeded and changed the
/// document; the writes and the document change are one undo step).
///
/// # Errors
///
/// Whatever `change` returns, a change the document's mode refuses
/// ([`MatGraphDoc::check_mode_change`]), or a file that cannot be written (nothing changes then).
pub fn edit_with_files<R>(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    change: impl FnOnce(&mut MatGraphDoc) -> Result<R, String>,
    files: impl FnOnce() -> Vec<FileWrite>,
) -> Result<R, String> {
    let (before, session) = {
        let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        if editor.session == 0 {
            let session = editor.next_session();
            editor.session = session;
        }
        (editor.doc.clone(), editor.session)
    };
    let mut after = before.clone();
    let result = change(&mut after)?;
    if after == before {
        return Ok(result);
    }
    after.check_mode_change(&before)?;
    let files = files();
    for file in &files {
        file.put(Some(&file.after))?;
    }
    let mut command = MatGraphEdit {
        label: label.to_string(),
        merge_key,
        session,
        before,
        after,
        files,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(result)
}

/// The body of a [`PendingEdit`].
pub type EditFn = Box<dyn FnOnce(&mut MatGraphDoc) -> Result<(), String> + Send>;

/// An edit collected while the panel draws (the panel borrows the editor while drawing, then
/// applies its edits through [`edit`]).
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
        change: impl FnOnce(&mut MatGraphDoc) -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            merge_key,
            change: Box::new(change),
        }
    }
}

/// Applies the panel's pending edits in order; a refused edit becomes the toast.
/// The canvas already shows what the user did (a new wire, a moved node); a refused edit, or
/// one that changed nothing, rebuilds the canvas from the document so it never shows a wire
/// the graph does not have.
pub fn apply_pending(world: &mut World, pending: Vec<PendingEdit>) {
    for p in pending {
        let revision = world
            .get_resource_or_insert_with(MatGraphEditor::default)
            .revision;
        let result = edit(world, &p.label, p.merge_key, p.change);
        let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        if let Err(err) = result {
            editor.toast = err;
        }
        if editor.revision == revision {
            editor.ui.invalidate_canvas();
        }
    }
}

/// What [`collapse_selection`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapseReport {
    /// The new function's name.
    pub function: String,
    /// Its signature.
    pub signature: function::Signature,
    /// The `Call` nodes that replaced the selection.
    pub calls: Vec<String>,
    /// The function file written (`None`: the function is inline).
    pub file: Option<PathBuf>,
    /// The compiled code of the default permutation is the same before and after (comments
    /// aside); `None` when the graph did not compile before.
    pub wgsl_identical: Option<bool>,
}

fn normalized_code(graph: &MaterialGraph, dir: Option<&Path>, root: Option<&Path>) -> Option<String> {
    let resolved = model::resolved(graph, dir, root).ok()?;
    let compiled = regia_matgraph::compile_unvalidated(&resolved).ok()?;
    let mut code = function::normalized_wgsl(&compiled.wgsl);
    if let Some(vertex) = &compiled.vertex_wgsl {
        code.push('\n');
        code.push_str(&function::normalized_wgsl(vertex));
    }
    Some(code)
}

/// Collapses `selection` of the open graph into material function `name`
/// ([`function::collapse`]): inline when `file` is `None`, else written to that
/// project-relative `.matfunc.ron` (next to the graph, else under `assets/`) and referenced
/// by path. One undo step: undo restores the graph and the function file's bytes before (a
/// file the collapse created is removed).
///
/// # Errors
///
/// The reasons [`function::collapse`] gives, a bad file path, or a write that failed.
pub fn collapse_selection(
    world: &mut World,
    selection: &std::collections::BTreeSet<String>,
    name: &str,
    file: Option<&str>,
) -> Result<CollapseReport, String> {
    let (doc, dir, root) = {
        let editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        (editor.doc.clone(), editor.graph_dir(), editor.root.clone())
    };
    let resolved = model::resolved(&doc.graph, dir.as_deref(), root.as_deref())?;
    let collapsed = function::collapse(&doc, &resolved, selection, name)?;
    let (source, write) = match file {
        None => (FunctionSource::Inline(collapsed.function.clone()), None),
        Some(rel) => {
            if !rel.ends_with(".matfunc.ron") {
                return Err(format!("`{rel}` must name a `.matfunc.ron` file"));
            }
            let full = function_file_target(dir.as_deref(), root.as_deref(), rel)?;
            if full.exists() {
                return Err(format!("{} already exists: pick another file", full.display()));
            }
            let text = collapsed.function.to_ron().map_err(|e| e.to_string())?;
            (
                FunctionSource::File(rel.to_string()),
                Some(FileWrite::new(full, text.into_bytes())),
            )
        }
    };
    let before_code = normalized_code(&doc.graph, dir.as_deref(), root.as_deref());
    let file_path = write.as_ref().map(|w| w.path.clone());
    let made = collapsed.clone();
    let selection = selection.clone();
    edit_with_files(
        world,
        "collapse into function",
        None,
        move |d| {
            function::apply_collapse(d, &selection, &made, source);
            Ok(())
        },
        move || write.into_iter().collect(),
    )?;
    let after = world.resource::<MatGraphEditor>().doc.graph.clone();
    let after_code = normalized_code(&after, dir.as_deref(), root.as_deref());
    Ok(CollapseReport {
        function: name.to_string(),
        signature: function::Signature::of(&collapsed.function),
        calls: collapsed.calls,
        file: file_path,
        wgsl_identical: before_code.map(|b| Some(b) == after_code),
    })
}

/// Where a new function file goes: next to the graph when the graph has a folder, else under
/// the project's `assets/` (the path rule of [`model::graph_file_path`]).
fn function_file_target(dir: Option<&Path>, root: Option<&Path>, rel: &str) -> Result<PathBuf, String> {
    let resolved = model::graph_file_path(dir, root, rel)?;
    Ok(match dir {
        Some(dir) if !resolved.exists() => dir.join(rel.replace('\\', "/")),
        _ => resolved,
    })
}

/// The definition of function `name` of the open graph (a file one is read).
///
/// # Errors
///
/// No such function, or its file cannot be read.
pub fn function_definition(
    editor: &MatGraphEditor,
    name: &str,
) -> Result<(MaterialFunction, Option<PathBuf>), String> {
    let dir = editor.graph_dir();
    match editor.doc.graph.functions.get(name) {
        Some(FunctionSource::Inline(f)) => Ok((f.clone(), None)),
        Some(FunctionSource::File(rel)) => {
            let full = model::graph_file_path(dir.as_deref(), editor.root.as_deref(), rel)?;
            let text = std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            let f = MaterialFunction::from_ron(&text).map_err(|e| format!("{}: {e}", full.display()))?;
            Ok((f, Some(full)))
        }
        None => Err(format!("the graph has no function `{name}`")),
    }
}

/// Expands `Call` node `node` of the open graph inline ([`function::expand`]); one undo step.
///
/// # Errors
///
/// Not a `Call` node, its function cannot be read, or [`function::expand`] refuses.
pub fn expand_call(world: &mut World, node: &str) -> Result<function::Expanded, String> {
    let definition = {
        let editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        let Some(regia_matgraph::Node::Call { function, .. }) = editor.doc.graph.nodes.get(node) else {
            return Err(format!("`{node}` is not a Call node"));
        };
        function_definition(&editor, &function.clone())?.0
    };
    let node = node.to_string();
    edit(world, "expand function", None, move |d| {
        function::expand(d, &node, &definition)
    })
}

/// Fixes the stale `Call` nodes of the open graph ([`function::fix_call`]; all of them, or
/// only `nodes`) against the signatures the diagnostics read; one undo step. Returns what
/// changed per node (empty when nothing was stale).
///
/// # Errors
///
/// Not a graph document.
pub fn fix_signatures(world: &mut World, nodes: Option<&[String]>) -> Result<Vec<function::CallFix>, String> {
    let sigs = {
        let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
        if !matches!(editor.doc.mode, DocMode::Graph | DocMode::Instance(_)) {
            return Err("fix signatures in the graph that calls the function".into());
        }
        editor.diagnostics().signatures.clone()
    };
    let wanted: Option<Vec<String>> = nodes.map(<[String]>::to_vec);
    edit(world, "fix function signatures", None, move |d| {
        let mut fixes = Vec::new();
        for (name, node) in &mut d.graph.nodes {
            if wanted.as_ref().is_some_and(|w| !w.contains(name)) {
                continue;
            }
            let regia_matgraph::Node::Call { function, .. } = node else {
                continue;
            };
            let Some(sig) = sigs.get(function.as_str()) else {
                continue;
            };
            let sig = sig.clone();
            if let Some(fix) = function::fix_call(name, node, &sig) {
                fixes.push(fix);
            }
        }
        Ok(fixes)
    })
}

/// Copies nodes `names` of the open document to the editor's clipboard (shared by every tab)
/// and returns the RON text ([`clipboard::copy_nodes`]).
///
/// # Errors
///
/// Nothing picked, or a name the document does not have.
pub fn copy_selection(
    world: &mut World,
    names: &std::collections::BTreeSet<String>,
) -> Result<String, String> {
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    let text = clipboard::copy_nodes(&editor.doc, names)?;
    editor.clipboard = Some(text.clone());
    Ok(text)
}

/// Copies nodes `names`, then removes them (one undo step; readers are unwired).
///
/// # Errors
///
/// As [`copy_selection`], or a document that refuses the removal (an instance).
pub fn cut_selection(
    world: &mut World,
    names: &std::collections::BTreeSet<String>,
) -> Result<String, String> {
    let text = copy_selection(world, names)?;
    let names = names.clone();
    edit(world, "cut nodes", None, move |d| {
        for name in &names {
            d.remove_node(name)?;
        }
        Ok(())
    })?;
    Ok(text)
}

/// Pastes `text` (or the editor's clipboard) into the open document with the copied
/// selection's top-left corner at `at` ([`clipboard::paste_nodes`]); one undo step. Returns
/// the copied name → the pasted name.
///
/// # Errors
///
/// An empty clipboard, text that is not a node clipboard, or a document that refuses the
/// paste (an instance).
pub fn paste(
    world: &mut World,
    text: Option<&str>,
    at: [f32; 2],
) -> Result<BTreeMap<String, String>, String> {
    let text = match text {
        Some(t) => t.to_string(),
        None => world
            .get_resource_or_insert_with(MatGraphEditor::default)
            .clipboard
            .clone()
            .ok_or_else(|| "the clipboard is empty: copy nodes first".to_string())?,
    };
    edit(world, "paste nodes", None, move |d| {
        clipboard::paste_nodes(d, &text, at)
    })
}

/// Sets typed row `field` of node `node` ([`inspector::set_field`]); one undo step, and
/// repeated edits of the same row (a drag) coalesce until the pointer is released.
///
/// # Errors
///
/// No such node, or the row refuses the value.
pub fn set_node_field(
    world: &mut World,
    node: &str,
    field: &str,
    value: inspector::FieldValue,
) -> Result<(), String> {
    let (node, field) = (node.to_string(), field.to_string());
    let key = format!("field:{node}:{field}");
    edit(world, &format!("set {field}"), Some(key), move |d| {
        let current = d
            .graph
            .nodes
            .get(&node)
            .ok_or_else(|| format!("no node `{node}`"))?;
        let next = inspector::set_field(current, &field, value)?;
        d.set_node(&node, next)
    })
}

/// Aligns or distributes nodes `names` ([`clipboard::arrange`]); one undo step.
///
/// # Errors
///
/// Too few nodes, or an unknown one.
pub fn arrange_nodes(
    world: &mut World,
    names: &std::collections::BTreeSet<String>,
    op: clipboard::Arrange,
) -> Result<(), String> {
    let names = names.clone();
    edit(world, op.name(), None, move |d| clipboard::arrange(d, &names, op))
}

/// Adds a node of `kind` at `pos`, wired to the pin a dropped wire came from
/// ([`search::add_wired`]); one undo step. Returns the new node's name.
///
/// # Errors
///
/// As [`search::add_wired`].
pub fn search_add(
    world: &mut World,
    kind: &str,
    pos: [f32; 2],
    from: Option<search::WireFrom>,
) -> Result<String, String> {
    let kind = kind.to_string();
    edit(world, &format!("add {kind}"), None, move |d| {
        search::add_wired(d, &kind, pos, from.as_ref())
    })
}

/// What [`apply_to_selection`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// The project-relative material path assigned.
    pub path: String,
    /// How many mesh owners got it.
    pub owners: usize,
}

/// Assigns the open graph or instance (its saved file) to the selected mesh owners'
/// `MeshSource.material`: the content browser's material drop edit (spec 19 § 3), one
/// `SetProperty` per owner in one undo step.
///
/// # Errors
///
/// No graph or instance open, a function tab, a document never saved or saved outside the
/// project, nothing selected, or no mesh in the selection.
pub fn apply_to_selection(world: &mut World) -> Result<ApplyReport, String> {
    let root = project_root(world);
    let path = {
        let editor = world
            .get_resource::<MatGraphEditor>()
            .filter(|e| e.is_open())
            .ok_or_else(|| "open a material graph or instance first".to_string())?;
        if matches!(editor.doc.mode, DocMode::Function(_)) {
            return Err(
                "a material function is not a material: apply a graph or an instance that calls it".into(),
            );
        }
        editor
            .path
            .clone()
            .ok_or_else(|| "save the graph first: the level references its file".to_string())?
    };
    let root = root.ok_or_else(|| "no project is open".to_string())?;
    let rel = path
        .strip_prefix(&root)
        .map_err(|_| {
            format!(
                "{} is outside the project: save it under assets/ first",
                path.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let selected: Vec<Entity> = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    if selected.is_empty() {
        return Err("select the meshes to apply it to (nothing is selected)".into());
    }
    let mut owners: Vec<Entity> = Vec::new();
    for entity in selected {
        let owner = crate::mesh_source::pick_owner(world, entity);
        if world.get::<regia_render::MeshSource>(owner).is_some() && !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    if owners.is_empty() {
        return Err("the selection has no mesh: select meshes to apply it to".into());
    }
    assign_material(world, &owners, &rel)?;
    crate::mesh_source::resolve_mesh_sources(world);
    Ok(ApplyReport {
        path: rel,
        owners: owners.len(),
    })
}

/// Sets `MeshSource.material` of every owner to `rel` as one undo step, all or nothing
/// (ADR-7451 (1)): every owner is captured before any is changed, so a capture that fails
/// leaves every owner and the undo stack as they were.
fn assign_material(world: &mut World, owners: &[Entity], rel: &str) -> Result<(), String> {
    let commands = owners
        .iter()
        .map(|owner| {
            let new: Box<dyn PartialReflect> = Box::new(rel.to_string());
            crate::command::SetProperty::capture(world, *owner, "MeshSource", "material", new)
                .map_err(|e| format!("cannot assign the material: {e}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut group = crate::command::CommandGroup::new(format!("Apply {rel}"));
    for mut command in commands {
        command.apply(world);
        group.push(Box::new(command));
    }
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(group));
    Ok(())
}

/// The calling-graph context of a function tab opened from the open graph document: its
/// graph with function files read (the function's own file stays a reference and is
/// replaced by the tab's definition when it compiles).
#[must_use]
pub fn context_of_open_graph(editor: &MatGraphEditor) -> Option<FunctionContext> {
    if !matches!(editor.doc.mode, DocMode::Graph) || !editor.is_open() {
        return None;
    }
    let dir = editor.graph_dir();
    let graph = model::resolved(&editor.doc.graph, dir.as_deref(), editor.root.as_deref())
        .unwrap_or_else(|_| editor.doc.graph.clone());
    Some(FunctionContext {
        path: editor.path.clone(),
        graph,
    })
}

/// Reads a calling graph file as a [`FunctionContext`] (its function files resolved).
///
/// # Errors
///
/// The file cannot be read or parsed.
pub fn context_from_file(path: &Path, root: Option<&Path>) -> Result<FunctionContext, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let graph = MaterialGraph::from_ron(&text).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
    let graph = model::resolved(&graph, path.parent(), root).unwrap_or(graph);
    Ok(FunctionContext {
        path: Some(path.to_path_buf()),
        graph,
    })
}

/// The parent graph file of an instance at `instance_path` whose `parent` field says
/// `reference`: next to the instance, else under the project.
///
/// # Errors
///
/// A reference outside the project.
pub fn instance_parent_path(
    instance_path: Option<&Path>,
    root: Option<&Path>,
    reference: &str,
) -> Result<PathBuf, String> {
    model::graph_file_path(instance_path.and_then(Path::parent), root, reference)
}

/// Resolves a user path: absolute as given, otherwise under the project's `assets/` (a
/// leading `assets/` is accepted).
#[must_use]
pub fn resolve_path(world: &World, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return p;
    }
    match project_root(world) {
        Some(root) => {
            let rel = path.strip_prefix("assets/").unwrap_or(path);
            root.join("assets").join(rel)
        }
        None => p,
    }
}

/// The editor's project root, if a project is open.
#[must_use]
pub fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        regia_test_utils::unique_temp_dir(&format!("matgraph-editor-{label}"))
    }

    #[test]
    fn layout_path_appends_layout() {
        assert_eq!(
            layout_path(Path::new("/a/hull.matgraph.ron")),
            PathBuf::from("/a/hull.matgraph.layout.ron")
        );
    }

    /// Audit G editor2-platform L2 (sprint-fix-editor2-platform, consolidated in
    /// sprint-fixrev-08-k): two saves never share a temp file and the old fixed `.ron.new`
    /// name is gone, so two writer processes on one project cannot truncate each other's
    /// in-flight temp. The save goes through the shared `crate::atomic_write` helper, whose
    /// temp name is `x.matgraph.ron.<pid>.<serial>.tmp`.
    #[test]
    fn save_uses_a_process_unique_temp_name() {
        let dir = temp_dir("save-temp");
        let target = dir.join("part.matgraph.ron");
        write_atomic(&target, "(nodes:{})\n").expect("write");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "(nodes:{})\n");
        write_atomic(&target, "(nodes:{a:1})\n").expect("second write");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "(nodes:{a:1})\n",
            "the rename replaces the earlier save"
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.ends_with(".ron.new")),
            "the old fixed temp name is gone: {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.ends_with(".tmp")),
            "no temp file left next to the graph: {names:?}"
        );
        assert!(
            names.contains(&"part.matgraph.ron".to_string()),
            "the save landed: {names:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit LOW (sprint-corr-editor-a L8): a save that failed part way (a dirty tab with no
    /// path, a write error) returned before the active tab was restored, so the editor showed a
    /// different document than the user had open.
    #[test]
    fn a_failed_save_of_every_tab_still_restores_the_active_tab() {
        let dir = temp_dir("save-dirty-restore");
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor
            .doc
            .add_node(Some("c"), regia_matgraph::Node::Float(0.5), None)
            .expect("add");
        editor.open_tab();
        editor.new_graph(Some(dir.join("b.matgraph.ron")));
        assert_eq!(editor.active_tab, 1);
        assert!(
            editor.save_dirty_documents().is_err(),
            "tab 0 has edits and no path"
        );
        assert_eq!(editor.active_tab, 1, "the user's tab is active again");
        assert_eq!(editor.path.as_deref(), Some(dir.join("b.matgraph.ron").as_path()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edits_undo_redo_through_the_shared_stack() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(MatGraphEditor::default)
            .new_graph(None);
        edit(&mut world, "add", None, |d| {
            d.add_node(Some("c"), regia_matgraph::Node::Float(0.5), None)
                .map(|_| ())
        })
        .expect("add");
        edit(&mut world, "output", None, |d| {
            d.set_output("roughness", Some("c"))
        })
        .expect("out");
        assert_eq!(
            world
                .resource::<MatGraphEditor>()
                .doc
                .graph
                .output
                .roughness
                .as_deref(),
            Some("c")
        );
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.resource::<MatGraphEditor>().doc.graph.output.roughness,
            None
        );
        assert!(crate::command::undo_world(&mut world));
        assert!(world.resource::<MatGraphEditor>().doc.graph.nodes.is_empty());
        assert!(crate::command::redo_world(&mut world));
        assert!(
            world
                .resource::<MatGraphEditor>()
                .doc
                .graph
                .nodes
                .contains_key("c")
        );
        // A refused edit pushes nothing.
        let depth = world.resource::<UndoStack>().undo_depth();
        assert!(edit(&mut world, "bad", None, |d| d.set_output("nope", Some("c"))).is_err());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
    }

    /// A graph edit, including one that coalesces, stays undoable without marking the level dirty.
    #[test]
    fn a_graph_edit_does_not_mark_the_level_dirty() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world.resource_mut::<UndoStack>().mark_saved();
        world
            .get_resource_or_insert_with(MatGraphEditor::default)
            .new_graph(None);
        edit(&mut world, "add", None, |d| {
            d.add_node(Some("c"), regia_matgraph::Node::Float(0.5), None)
                .map(|_| ())
        })
        .expect("add");
        let key = Some("move:c".to_string());
        edit(&mut world, "move", key.clone(), |d| d.move_node("c", [1.0, 2.0])).expect("move");
        edit(&mut world, "move", key, |d| d.move_node("c", [3.0, 4.0])).expect("coalesce");
        assert!(!world.resource::<UndoStack>().is_dirty());
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        assert_eq!(
            world
                .resource::<MatGraphEditor>()
                .doc
                .positions()
                .get("c")
                .copied(),
            Some([3.0, 4.0])
        );
    }

    #[test]
    fn an_undo_from_another_graph_never_touches_the_open_one() {
        let dir = temp_dir("undo-session");
        let a = dir.join("a.matgraph.ron");
        let b = dir.join("b.matgraph.ron");
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        for (path, name, value) in [(&a, "from_a", 0.25), (&b, "from_b", 0.75)] {
            let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
            editor.new_graph(Some(path.clone()));
            editor
                .doc
                .add_node(Some(name), regia_matgraph::Node::Float(value), None)
                .expect("add");
            editor.save(None).expect("save");
        }
        world.resource_mut::<MatGraphEditor>().open(&a).expect("open a");
        edit(&mut world, "add", None, |d| {
            d.add_node(Some("edit_a"), regia_matgraph::Node::Time, None)
                .map(|_| ())
        })
        .expect("edit a");
        edit(&mut world, "move", Some("move:from_a".into()), |d| {
            d.move_node("from_a", [1.0, 0.0])
        })
        .expect("move a");

        world.resource_mut::<MatGraphEditor>().open(&b).expect("open b");
        let opened = world.resource::<MatGraphEditor>().doc.clone();
        // The same drag key in the new graph must not merge into the old graph's entry.
        let depth = world.resource::<UndoStack>().undo_depth();
        edit(&mut world, "move", Some("move:from_a".into()), |d| {
            d.move_node("from_b", [2.0, 0.0])
        })
        .expect("move b");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);

        assert!(crate::command::undo_world(&mut world));
        assert_eq!(world.resource::<MatGraphEditor>().doc, opened);
        // These two steps belong to a: b stays b, and saving it writes b.
        assert!(crate::command::undo_world(&mut world));
        assert!(crate::command::undo_world(&mut world));
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(editor.doc, opened);
        assert_eq!(
            editor.toast,
            "that undo step belongs to a graph that is no longer open"
        );
        assert!(!editor.dirty());
        assert!(crate::command::redo_world(&mut world));
        assert_eq!(world.resource::<MatGraphEditor>().doc, opened);
        world.resource_mut::<MatGraphEditor>().save(None).expect("save b");
        let text = std::fs::read_to_string(&b).expect("read b");
        assert!(text.contains("from_b") && !text.contains("from_a"), "{text}");
    }

    #[test]
    fn moves_coalesce_by_key() {
        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        world
            .get_resource_or_insert_with(MatGraphEditor::default)
            .new_graph(None);
        edit(&mut world, "add", None, |d| {
            d.add_node(Some("c"), regia_matgraph::Node::Time, None)
                .map(|_| ())
        })
        .expect("add");
        let depth = world.resource::<UndoStack>().undo_depth();
        for i in 0..5 {
            let x = i as f32;
            edit(&mut world, "move", Some("move:c".into()), move |d| {
                d.move_node("c", [x, 0.0])
            })
            .expect("move");
        }
        assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
        assert_eq!(world.resource::<MatGraphEditor>().doc.layout["c"], [4.0, 0.0]);
    }

    #[test]
    fn save_and_reopen_round_trip_with_layout() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("paint.matgraph.ron");
        let mut editor = MatGraphEditor::default();
        editor.new_graph(Some(path.clone()));
        editor
            .doc
            .add_node(
                Some("v"),
                regia_matgraph::Node::Vec3([0.9, 0.1, 0.1]),
                Some([10.0, 20.0]),
            )
            .expect("add");
        editor
            .doc
            .add_node(Some("r"), regia_matgraph::Node::Float(0.4), None)
            .expect("add");
        editor.doc.set_output("base_color", Some("v")).expect("out");
        editor.doc.set_output("roughness", Some("r")).expect("out");
        editor.revision += 1;
        assert!(editor.dirty());
        let saved = editor.save(None).expect("save");
        assert_eq!(saved, path);
        assert!(!editor.dirty());
        assert!(layout_path(&path).exists());
        let mut again = MatGraphEditor::default();
        again.open(&path).expect("open");
        assert_eq!(again.doc.graph, editor.doc.graph);
        assert_eq!(again.doc.layout["v"], [10.0, 20.0]);
        assert!(
            again.doc.layout.contains_key("r"),
            "auto-laid nodes are saved too"
        );
        assert!(again.diagnostics().ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_or_broken_layout_is_ignored() {
        let dir = temp_dir("layout");
        let path = dir.join("g.matgraph.ron");
        std::fs::write(
            layout_path(&path),
            "(format_version: 3, positions: {\"a\": (1.0, 2.0)})",
        )
        .expect("write");
        assert!(read_layout(&path).is_empty());
        std::fs::write(layout_path(&path), "not ron").expect("write");
        assert!(read_layout(&path).is_empty());
        std::fs::write(
            layout_path(&path),
            "(format_version: 1, positions: {\"a\": (1.0, 2.0)})",
        )
        .expect("write");
        assert_eq!(read_layout(&path)["a"], [1.0, 2.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostics_are_cached_per_graph_change() {
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        let before = regia_matgraph::thread_compile_count();
        let _ = editor.diagnostics();
        let _ = editor.diagnostics();
        assert_eq!(regia_matgraph::thread_compile_count(), before + 1);
        // A canvas move bumps the revision but not the graph: no recompile.
        editor.doc.layout.insert("x".into(), [1.0, 2.0]);
        editor.revision += 1;
        let _ = editor.diagnostics();
        assert_eq!(regia_matgraph::thread_compile_count(), before + 1);
        // A graph change recompiles once.
        editor
            .doc
            .graph
            .nodes
            .insert("t".into(), regia_matgraph::Node::Time);
        editor.revision += 1;
        let _ = editor.diagnostics();
        let _ = editor.diagnostics();
        assert_eq!(regia_matgraph::thread_compile_count(), before + 2);
        // A save-as to another folder recompiles (function files resolve against it).
        editor.path = Some(PathBuf::from("elsewhere/g.matgraph.ron"));
        let _ = editor.diagnostics();
        assert_eq!(regia_matgraph::thread_compile_count(), before + 3);
    }
}
