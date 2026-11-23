// SPDX-License-Identifier: MPL-2.0
//! The open procedural graph document (spec 67 § 2): [`ProcGraphDoc`], the snapshot command every
//! edit pushes on the shared [`UndoStack`], the editing operations, collapse/expand, save and the
//! hot-reload choice.
//!
//! Edits work on a copy of the graph; on success the copy becomes the document and one
//! [`ProcGraphCommand`] holding the graph before and after is pushed (merged with the previous one
//! when both carry the same merge key: node drags, slider scrubs). A valid edited graph is also
//! written to the [`ProcGraphOverlay`] so generators bound to the file run the unsaved document at
//! once; an invalid one is not (the last good output stays, spec 67 § 3).

use std::collections::{BTreeMap, BTreeSet};

use regia_ecs::prelude::*;
use regia_procgen::graph::{CALL_NODES, GraphError, GraphPin, ParamDecl, ProcGraphVariant};
use regia_procgen::world::{ProcGenRegistry, ProcGenSettings, ProcGraphOverlay, ProcGraphSource};
use regia_procgen::{AttrType, Edge, KindSet, NodeEntry, ProcGraph, ProcNodeRegistry, SettingValue};

use crate::command::{EditorCommand, UndoStack};

/// A document error: a stable lowercase code (the agent's `data.reason`) and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocError {
    /// `procgen_not_open`, `procgen_file_exists`, `procgen_collapse_invalid`, ...
    pub code: String,
    /// Detail.
    pub message: String,
}

impl DocError {
    /// An error with a code and message.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A `.pgvar.ron` opened in the panel: the parent graph is shown read-only and the Parameters tab
/// edits the variant's overrides (spec 67 § 2.3).
#[derive(Clone, Debug, PartialEq)]
pub struct VariantView {
    /// The variant file.
    pub path: String,
    /// The variant (its overrides are the document's edits).
    pub variant: ProcGraphVariant,
}

/// An outside change to the open file while it had unsaved edits (spec 67 § 2.3): the panel asks
/// "keep mine / take theirs"; nothing is overwritten until the author chooses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadConflict {
    /// The file's new text.
    pub theirs: String,
}

/// The open document (world resource; the panel, the viewport overlays and the agent share it).
#[derive(Resource, Default)]
pub struct ProcGraphDoc {
    /// Project-relative path of the open graph (`None` = nothing open).
    pub path: Option<String>,
    /// The graph being edited (the parent graph when a variant is open).
    pub graph: ProcGraph,
    /// The open variant, when a `.pgvar.ron` was opened.
    pub variant: Option<VariantView>,
    /// The canonical text of the file at the last open, save or reload (dirty tracking).
    pub(crate) disk_text: Option<String>,
    /// The file's bytes as read at the last open, save or reload (outside-change detection).
    pub(crate) disk_raw: Option<String>,
    /// Selected node ids.
    pub selection: BTreeSet<u32>,
    /// Compile errors of the current graph (empty = valid).
    pub errors: Vec<GraphError>,
    /// A pending hot-reload choice.
    pub conflict: Option<ReloadConflict>,
    /// The call path of opened called graphs (breadcrumb): the graphs above the open one.
    pub breadcrumb: Vec<String>,
    /// Bumped on every document change (edit, undo, redo, open, reload).
    pub revision: u64,
    /// Status line of the last action.
    pub toast: String,
    /// Last copied fragment (RON), also put on the system clipboard by the panel.
    pub clipboard: Option<String>,
    /// Override pins shown on request (`(node, setting)`, spec 62 § 4.1); a pin with an edge is
    /// always shown. Editor state, not saved.
    pub shown_pins: BTreeSet<(u32, String)>,
    /// Every file of the open variant's chain, the variant first and the graph it resolves to
    /// last (empty when no variant is open). All of them are read-only while it is open.
    pub(crate) variant_files: Vec<String>,
}

impl ProcGraphDoc {
    /// Whether the document differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        match (&self.variant, &self.disk_text) {
            (Some(v), Some(disk)) => v.variant.to_ron() != *disk,
            (None, Some(disk)) => self.graph.to_ron() != *disk,
            (_, None) => self.path.is_some(),
        }
    }

    /// Whether graph content is read-only (a variant is open: only its overrides change).
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.variant.is_some()
    }
}

/// A path as the graph source names it: relative to a `Dir` source root when inside it (forward
/// slashes), else as given.
#[must_use]
pub fn project_relative(world: &World, path: &std::path::Path) -> String {
    if let Some(ProcGraphSource::Dir(root)) = world.get_resource::<ProcGenSettings>().map(|s| &s.source)
        && let Ok(rel) = path.strip_prefix(root)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.to_string_lossy().replace('\\', "/")
}

/// The text of a project graph file from the graph source (not the overlay).
#[must_use]
pub fn source_text(world: &World, path: &str) -> Option<String> {
    world.get_resource::<ProcGenSettings>()?.source.read(path)
}

/// The text of a graph as generators see it (overlay first, then the source); the open document
/// wins for its own path.
pub(crate) fn graph_text(world: &World, path: &str) -> Option<String> {
    if let Some(doc) = world.get_resource::<ProcGraphDoc>()
        && doc.path.as_deref() == Some(path)
        && doc.variant.is_none()
    {
        return Some(doc.graph.to_ron());
    }
    regia_procgen::world::read_graph_text(world, path)
}

pub(crate) fn registry(world: &World) -> std::sync::Arc<ProcNodeRegistry> {
    world
        .get_resource::<ProcGenRegistry>()
        .map_or_else(|| ProcGenRegistry::default().0, |r| r.0.clone())
}

/// Compiles `g` the way generators do (called graphs through the overlay and source).
pub(crate) fn compile_errors(world: &World, g: &ProcGraph) -> Vec<GraphError> {
    let registry = registry(world);
    let load = |p: &str| graph_text(world, p);
    g.compile_full(&registry, &BTreeMap::new(), &load)
        .err()
        .map(|e| e.0)
        .unwrap_or_default()
}

/// Writes a project file through the graph source: an atomic write under a `Dir` root, or the
/// in-memory map.
///
/// # Errors
///
/// `procgen_bad_path` (absolute, drive or UNC prefix, `..`, or a symlink leading out of the
/// project; ADR-1816), `procgen_unavailable`, `procgen_io`.
pub fn write_source(world: &mut World, path: &str, text: &str) -> Result<(), DocError> {
    let bad_path = |e: regia_core::project_path::ProjectPathError| {
        DocError::new(
            "procgen_bad_path",
            format!("graph paths are project-relative ({}): {e}", e.code()),
        )
    };
    regia_core::project_path::check_relative(path).map_err(bad_path)?;
    let mut settings = world
        .get_resource_mut::<ProcGenSettings>()
        .ok_or_else(|| DocError::new("procgen_unavailable", "ProcGenPlugin is not installed"))?;
    match &mut settings.source {
        ProcGraphSource::Dir(root) => {
            let target = regia_core::project_path::resolve_write(root, path).map_err(bad_path)?;
            regia_procgen::pgout::write_atomic(&target, text.as_bytes())
                .map_err(|e| DocError::new("procgen_io", e.to_string()))
        }
        ProcGraphSource::Memory(files) => {
            files.insert(path.to_string(), text.to_string());
            Ok(())
        }
    }
}

/// Publishes the document to generators: a valid graph goes to the overlay (or leaves it when the
/// document equals the file); an invalid one leaves the overlay as it was.
fn publish(world: &mut World) {
    let Some(doc) = world.get_resource::<ProcGraphDoc>() else {
        return;
    };
    let Some(path) = doc.path.clone() else {
        return;
    };
    let (key, text, clean, valid) = match &doc.variant {
        Some(v) => (v.path.clone(), v.variant.to_ron(), !doc.dirty(), true),
        None => (path, doc.graph.to_ron(), !doc.dirty(), doc.errors.is_empty()),
    };
    if !valid {
        return;
    }
    let mut overlay = world.get_resource_or_insert_with(ProcGraphOverlay::default);
    if clean {
        overlay.0.remove(&key);
    } else {
        overlay.0.insert(key, text);
    }
}

/// Recompiles the document, bumps its revision, publishes it and tells the preview to re-run
/// after the quiet period.
pub(crate) fn changed(world: &mut World) {
    let errors = match world.get_resource::<ProcGraphDoc>() {
        Some(doc) if doc.path.is_some() => compile_errors(world, &doc.graph),
        _ => return,
    };
    if let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() {
        doc.errors = errors;
        doc.revision += 1;
        let live: BTreeSet<u32> = doc.graph.nodes.keys().copied().collect();
        doc.selection.retain(|id| live.contains(id));
    }
    publish(world);
    super::debug_view::schedule_preview(world);
}

/// The document state one edit changes: the graph and, for variants, the overrides.
#[derive(Clone, Debug, PartialEq)]
pub struct DocSnapshot {
    /// The graph.
    pub graph: ProcGraph,
    /// The variant's overrides (variants only).
    pub overrides: Option<BTreeMap<String, SettingValue>>,
}

/// One undo entry: the document before and after an edit (spec 67 § 2).
pub struct ProcGraphCommand {
    label: String,
    merge_key: Option<String>,
    path: String,
    before: DocSnapshot,
    after: DocSnapshot,
}

impl ProcGraphCommand {
    fn set(world: &mut World, path: &str, snap: &DocSnapshot) {
        let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() else {
            return;
        };
        if doc.path.as_deref() != Some(path) {
            // Another document is open: the entry belongs to a closed one; leave it.
            doc.toast = format!("undo entry belongs to {path}, which is not open");
            return;
        }
        doc.graph = snap.graph.clone();
        if let (Some(v), Some(o)) = (doc.variant.as_mut(), snap.overrides.as_ref()) {
            v.variant.overrides = o.clone();
        }
        changed(world);
    }
}

impl EditorCommand for ProcGraphCommand {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, &self.path, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<ProcGraphCommand>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key && self.path == next.path {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Procgen: {}", self.label)
    }
}

/// What an edit closure sees: the graph copy and the node registry.
pub struct EditCtx<'a> {
    /// The graph copy (becomes the document on success).
    pub graph: &'a mut ProcGraph,
    /// Variant overrides (when a variant is open).
    pub overrides: Option<&'a mut BTreeMap<String, SettingValue>>,
    /// Node types.
    pub registry: &'a ProcNodeRegistry,
}

/// Applies `change` to a copy of the document; on success the copy becomes current and one undo
/// entry is pushed (merged with the previous one when both carry the same `merge_key`). On error
/// nothing changes. Graph edits on an open variant fail with `procgen_read_only` unless
/// `overrides_only` (the closure then only edits overrides).
///
/// # Errors
///
/// `procgen_not_open`, `procgen_read_only`, or whatever `change` returns.
pub fn edit<R>(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    change: impl FnOnce(&mut EditCtx<'_>) -> Result<R, DocError>,
) -> Result<R, DocError> {
    let registry = registry(world);
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let path = doc
        .path
        .clone()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let before = DocSnapshot {
        graph: doc.graph.clone(),
        overrides: doc.variant.as_ref().map(|v| v.variant.overrides.clone()),
    };
    let read_only = doc.read_only();
    let mut after = before.clone();
    let result = {
        let mut ctx = EditCtx {
            graph: &mut after.graph,
            overrides: after.overrides.as_mut(),
            registry: &registry,
        };
        change(&mut ctx)?
    };
    if after == before {
        return Ok(result);
    }
    if read_only && after.graph != before.graph {
        return Err(DocError::new(
            "procgen_read_only",
            "a variant is open: the parent graph is read-only (edit the parameter overrides)",
        ));
    }
    let mut command = ProcGraphCommand {
        label: label.to_string(),
        merge_key,
        path,
        before,
        after,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(result)
}

// ---- Open, new, save, reload ----

/// A blank graph (spec 67 § 2.3): `in.self` → `sample.region` → `out.points`, valid as is.
#[must_use]
pub fn blank_graph() -> ProcGraph {
    let mut g = ProcGraph::default();
    for (id, ty, x) in [
        (1, "in.self", 0.0),
        (2, "sample.region", 280.0),
        (3, "out.points", 560.0),
    ] {
        let mut n = NodeEntry::new(ty);
        n.at = (x, 0.0);
        g.nodes.insert(id, n);
    }
    g.edges.push(Edge::new(1, "out", 2, "region"));
    g.edges.push(Edge::new(2, "out", 3, "in"));
    g
}

/// Opens `path` (a `.pgraph.ron` or `.pgvar.ron`) in the document, replacing what was open.
///
/// # Errors
///
/// `procgen_unknown_graph` (cannot read), `procgen_graph_invalid` (does not parse).
pub fn open(world: &mut World, path: &str) -> Result<(), DocError> {
    let text = source_text(world, path)
        .or_else(|| regia_procgen::world::read_graph_text(world, path))
        .ok_or_else(|| DocError::new("procgen_unknown_graph", format!("cannot read `{path}`")))?;
    let (graph, variant, variant_files) = if path.ends_with(".pgvar.ron") {
        let variant = ProcGraphVariant::from_ron(&text)
            .map_err(|e| DocError::new("procgen_graph_invalid", format!("{path}: {e}")))?;
        let resolved =
            regia_procgen::graph::resolve_graph(path, &|p| regia_procgen::world::read_graph_text(world, p))
                .map_err(|e| DocError::new("procgen_graph_invalid", format!("{path}: {e}")))?;
        (
            resolved.graph,
            Some(VariantView {
                path: path.to_string(),
                variant,
            }),
            resolved.files,
        )
    } else {
        let g = ProcGraph::from_ron(&text)
            .map_err(|e| DocError::new("procgen_graph_invalid", format!("{path}: {e}")))?;
        (g, None, Vec::new())
    };
    let canonical = match &variant {
        Some(v) => v.variant.to_ron(),
        None => graph.to_ron(),
    };
    let mut doc = world.get_resource_or_insert_with(ProcGraphDoc::default);
    doc.path = Some(path.to_string());
    doc.graph = graph;
    doc.variant = variant;
    doc.variant_files = variant_files;
    doc.disk_text = Some(canonical);
    doc.disk_raw = Some(text);
    doc.selection.clear();
    doc.conflict = None;
    doc.toast = format!("opened {path}");
    changed(world);
    Ok(())
}

/// Creates `path` from a starter graph (`starter`, a graph path whose `flags.starter` is true) or
/// blank, writes it and opens it.
///
/// # Errors
///
/// `procgen_file_exists`, `procgen_unknown_graph` (starter missing), `procgen_not_starter`,
/// `procgen_bad_path`, IO.
pub fn new_graph(world: &mut World, path: &str, starter: Option<&str>) -> Result<(), DocError> {
    if !path.ends_with(".pgraph.ron") {
        return Err(DocError::new(
            "procgen_bad_path",
            "a new graph's path ends in `.pgraph.ron`",
        ));
    }
    if source_text(world, path).is_some() {
        return Err(DocError::new(
            "procgen_file_exists",
            format!("`{path}` already exists"),
        ));
    }
    let mut g = match starter {
        Some(s) => {
            let text = regia_procgen::world::read_graph_text(world, s)
                .ok_or_else(|| DocError::new("procgen_unknown_graph", format!("cannot read `{s}`")))?;
            let g = ProcGraph::from_ron(&text)
                .map_err(|e| DocError::new("procgen_graph_invalid", format!("{s}: {e}")))?;
            if !g.flags.starter {
                return Err(DocError::new(
                    "procgen_not_starter",
                    format!("`{s}` is not a starter graph (flags.starter is false)"),
                ));
            }
            g
        }
        None => blank_graph(),
    };
    // A copy of a starter is an ordinary graph.
    g.flags.starter = false;
    write_source(world, path, &g.to_ron())?;
    open(world, path)
}

/// Saves the document (the graph, or the variant's overrides) with an atomic write.
///
/// # Errors
///
/// `procgen_not_open`, IO.
pub fn save(world: &mut World) -> Result<String, DocError> {
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let path = doc
        .path
        .clone()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let (file, text) = match &doc.variant {
        Some(v) => (v.path.clone(), v.variant.to_ron()),
        None => (path, doc.graph.to_ron()),
    };
    write_source(world, &file, &text)?;
    if let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() {
        doc.disk_text = Some(text.clone());
        doc.disk_raw = Some(text);
        doc.conflict = None;
        doc.toast = format!("saved {file}");
    }
    world
        .get_resource_or_insert_with(ProcGraphOverlay::default)
        .0
        .remove(&file);
    Ok(file)
}

/// Checks the open file for an outside change (hot reload, spec 67 § 2.3): with no unsaved edits
/// the document reloads silently (`true`); with unsaved edits a [`ReloadConflict`] is set and
/// nothing is written.
pub fn check_external(world: &mut World) -> bool {
    let Some(doc) = world.get_resource::<ProcGraphDoc>() else {
        return false;
    };
    let Some(file) = doc
        .variant
        .as_ref()
        .map(|v| v.path.clone())
        .or_else(|| doc.path.clone())
    else {
        return false;
    };
    let Some(disk) = source_text(world, &file) else {
        return false;
    };
    let doc = world.resource::<ProcGraphDoc>();
    if doc.disk_raw.as_deref() == Some(disk.as_str()) {
        return false;
    }
    if doc.conflict.as_ref().is_some_and(|c| c.theirs == disk) {
        return false;
    }
    if doc.dirty() {
        let mut doc = world.resource_mut::<ProcGraphDoc>();
        doc.conflict = Some(ReloadConflict { theirs: disk });
        doc.toast = format!("{file} changed on disk: keep mine or take theirs");
        return false;
    }
    let path = doc.path.clone().unwrap_or(file);
    let selection = doc.selection.clone();
    if open(world, &path).is_ok() {
        if let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() {
            let live: BTreeSet<u32> = doc.graph.nodes.keys().copied().collect();
            doc.selection = selection.intersection(&live).copied().collect();
            doc.toast = "reloaded (changed on disk)".into();
        }
        true
    } else {
        false
    }
}

/// Resolves a pending reload conflict: `take_theirs` reloads the file (dropping unsaved edits),
/// otherwise the document keeps its edits and the next save overwrites the file.
///
/// # Errors
///
/// `procgen_no_conflict`, or the reload's error.
pub fn resolve_conflict(world: &mut World, take_theirs: bool) -> Result<(), DocError> {
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let conflict = doc
        .conflict
        .clone()
        .ok_or_else(|| DocError::new("procgen_no_conflict", "no outside change is pending"))?;
    if take_theirs {
        let path = doc.path.clone().unwrap_or_default();
        open(world, &path)
    } else {
        // Mine stays dirty against their file; the next save overwrites it.
        let canonical = if conflict.theirs.contains("ProcGraphVariant") {
            ProcGraphVariant::from_ron(&conflict.theirs)
                .map(|v| v.to_ron())
                .ok()
        } else {
            ProcGraph::from_ron(&conflict.theirs).map(|g| g.to_ron()).ok()
        };
        let mut doc = world.resource_mut::<ProcGraphDoc>();
        doc.disk_text = canonical.or(Some(conflict.theirs.clone()));
        doc.disk_raw = Some(conflict.theirs);
        doc.conflict = None;
        doc.toast = "kept my edits".into();
        Ok(())
    }
}

// ---- Graph operations (each one undo entry through `edit`) ----

fn node(g: &mut ProcGraph, id: u32) -> Result<&mut NodeEntry, DocError> {
    g.nodes
        .get_mut(&id)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {id}")))
}

/// Adds a node of type `ty` at `at`; returns its id.
///
/// # Errors
///
/// `unknown_node_type` for a type the registry lacks (`note` and `reroute` are always allowed).
pub fn add_node(ctx: &mut EditCtx<'_>, ty: &str, at: (f64, f64)) -> Result<u32, DocError> {
    if !regia_procgen::nodes::EDITOR_AIDS.contains(&ty) && ctx.registry.get(ty).is_none() {
        return Err(DocError::new(
            "unknown_node_type",
            format!("unknown node type `{ty}`"),
        ));
    }
    let mut n = NodeEntry::new(ty);
    n.at = at;
    let id = ctx.graph.next_node_id();
    ctx.graph.nodes.insert(id, n);
    Ok(id)
}

/// Removes nodes and every edge touching them.
pub fn remove_nodes(g: &mut ProcGraph, ids: &BTreeSet<u32>) {
    for id in ids {
        g.nodes.remove(id);
    }
    g.edges
        .retain(|e| !ids.contains(&e.from.0) && !ids.contains(&e.to.0));
}

/// Connects an output pin to an input pin. A single (not multi) input keeps one edge: the new edge
/// replaces the old one.
///
/// # Errors
///
/// `procgen_unknown_node` for a missing end.
pub fn connect(ctx: &mut EditCtx<'_>, from: (u32, &str), to: (u32, &str)) -> Result<(), DocError> {
    for id in [from.0, to.0] {
        if !ctx.graph.nodes.contains_key(&id) {
            return Err(DocError::new("procgen_unknown_node", format!("no node {id}")));
        }
    }
    let edge = Edge::new(from.0, from.1, to.0, to.1);
    if ctx.graph.edges.contains(&edge) {
        return Ok(());
    }
    let load = |_: &str| None;
    // Pins that cannot be resolved here (a `graph.call`/`graph.loop` whose subgraph this context
    // cannot load; subgraph inputs are multi) keep their existing wires: dropping one silently lost
    // data, while an extra wire on a single pin is a compile error the author sees (audit zai M1).
    let multi = match ctx.graph.node_pins(to.0, ctx.registry, &load) {
        None => true,
        Some(p) => p
            .inputs
            .into_iter()
            .find(|i| i.name == to.1)
            .is_some_and(|p| p.multi),
    };
    let is_reroute = ctx.graph.nodes.get(&to.0).is_some_and(|n| n.ty == "reroute");
    if !multi && !is_reroute {
        ctx.graph.edges.retain(|e| !(e.to.0 == to.0 && e.to.1 == to.1));
    }
    ctx.graph.edges.push(edge);
    Ok(())
}

/// Removes one edge (no error when it is absent).
pub fn disconnect(g: &mut ProcGraph, from: (u32, &str), to: (u32, &str)) {
    let edge = Edge::new(from.0, from.1, to.0, to.1);
    g.edges.retain(|e| *e != edge);
}

/// Moves a node on the canvas.
///
/// # Errors
///
/// `procgen_unknown_node`.
pub fn move_node(g: &mut ProcGraph, id: u32, at: (f64, f64)) -> Result<(), DocError> {
    node(g, id)?.at = at;
    Ok(())
}

/// Sets one setting. The value is checked the way the compiler checks it (type, range, enum
/// options): an invalid value is not written and the error names it (spec 67 § 2.1).
///
/// # Errors
///
/// `procgen_unknown_node`, or the compiler's setting code (`unknown_setting`, `setting_type`,
/// `setting_out_of_range`, ...).
pub fn set_setting(ctx: &mut EditCtx<'_>, id: u32, name: &str, value: SettingValue) -> Result<(), DocError> {
    let before = setting_errors(ctx.graph, ctx.registry, id);
    let old = node(ctx.graph, id)?.settings.insert(name.to_string(), value);
    let after = setting_errors(ctx.graph, ctx.registry, id);
    if let Some(e) = after.iter().find(|e| !before.contains(e)) {
        let n = node(ctx.graph, id)?;
        match old {
            Some(v) => n.settings.insert(name.to_string(), v),
            None => n.settings.remove(name),
        };
        return Err(DocError::new(&e.code, e.message.clone()));
    }
    Ok(())
}

/// The setting-level compile errors of one node.
fn setting_errors(g: &ProcGraph, registry: &ProcNodeRegistry, id: u32) -> Vec<GraphError> {
    let codes = [
        "unknown_setting",
        "setting_type",
        "setting_out_of_range",
        "setting_invalid",
        "unknown_param",
        "param_type_mismatch",
    ];
    match g.compile(registry) {
        Ok(_) => Vec::new(),
        Err(errors) => errors
            .0
            .into_iter()
            .filter(|e| {
                e.node == Some(id) && (codes.contains(&e.code.as_str()) || e.code.starts_with("setting"))
            })
            .collect(),
    }
}

/// Bypass toggle (`enabled`, spec 62 § 4).
///
/// # Errors
///
/// `procgen_unknown_node`.
pub fn set_enabled(g: &mut ProcGraph, id: u32, enabled: bool) -> Result<(), DocError> {
    node(g, id)?.enabled = enabled;
    Ok(())
}

/// Declares a parameter.
///
/// # Errors
///
/// `procgen_param_name` (invalid or taken name).
pub fn add_param(g: &mut ProcGraph, decl: ParamDecl) -> Result<(), DocError> {
    if !regia_procgen::graph::valid_param_name(&decl.name) || g.param(&decl.name).is_some() {
        return Err(DocError::new(
            "procgen_param_name",
            format!("`{}` is not a new valid parameter name", decl.name),
        ));
    }
    g.params.push(decl);
    Ok(())
}

fn bind(name: &str) -> SettingValue {
    SettingValue::Call("Bind".into(), vec![SettingValue::Str(name.to_string())])
}

fn rename_binds(v: &mut SettingValue, from: &str, to: &str) -> usize {
    if v.as_bind() == Some(from) {
        *v = bind(to);
        return 1;
    }
    match v {
        SettingValue::Call(_, items) | SettingValue::Tuple(items) | SettingValue::List(items) => {
            items.iter_mut().map(|x| rename_binds(x, from, to)).sum()
        }
        SettingValue::Record(_, fields) => fields.iter_mut().map(|(_, x)| rename_binds(x, from, to)).sum(),
        SettingValue::Map(entries) => entries.iter_mut().map(|(_, x)| rename_binds(x, from, to)).sum(),
        _ => 0,
    }
}

fn binds_in(v: &SettingValue, out: &mut BTreeSet<String>) {
    if let Some(p) = v.as_bind() {
        out.insert(p.to_string());
        return;
    }
    match v {
        SettingValue::Call(_, items) | SettingValue::Tuple(items) | SettingValue::List(items) => {
            items.iter().for_each(|x| binds_in(x, out));
        }
        SettingValue::Record(_, fields) => fields.iter().for_each(|(_, x)| binds_in(x, out)),
        SettingValue::Map(entries) => entries.iter().for_each(|(_, x)| binds_in(x, out)),
        _ => {}
    }
}

fn unbind_all(v: &mut SettingValue, name: &str, default: &SettingValue) {
    if v.as_bind() == Some(name) {
        *v = default.clone();
        return;
    }
    match v {
        SettingValue::Call(_, items) | SettingValue::Tuple(items) | SettingValue::List(items) => {
            items.iter_mut().for_each(|x| unbind_all(x, name, default));
        }
        SettingValue::Record(_, fields) => fields.iter_mut().for_each(|(_, x)| unbind_all(x, name, default)),
        SettingValue::Map(entries) => entries.iter_mut().for_each(|(_, x)| unbind_all(x, name, default)),
        _ => {}
    }
}

/// Renames a parameter and every `Bind` of it in the graph; returns the number of binds updated.
///
/// # Errors
///
/// `procgen_unknown_param`, `procgen_param_name`.
pub fn rename_param(g: &mut ProcGraph, from: &str, to: &str) -> Result<usize, DocError> {
    if !regia_procgen::graph::valid_param_name(to) || g.param(to).is_some() {
        return Err(DocError::new(
            "procgen_param_name",
            format!("`{to}` is not a new valid parameter name"),
        ));
    }
    let p = g
        .params
        .iter_mut()
        .find(|p| p.name == from)
        .ok_or_else(|| DocError::new("procgen_unknown_param", format!("no parameter `{from}`")))?;
    p.name = to.to_string();
    let mut count = 0;
    for n in g.nodes.values_mut() {
        for v in n.settings.values_mut() {
            count += rename_binds(v, from, to);
        }
    }
    Ok(count)
}

/// Removes a parameter; settings bound to it get its default value.
///
/// # Errors
///
/// `procgen_unknown_param`.
pub fn remove_param(g: &mut ProcGraph, name: &str) -> Result<(), DocError> {
    let pos = g
        .params
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| DocError::new("procgen_unknown_param", format!("no parameter `{name}`")))?;
    let decl = g.params.remove(pos);
    for n in g.nodes.values_mut() {
        for v in n.settings.values_mut() {
            unbind_all(v, name, &decl.default);
        }
    }
    Ok(())
}

/// Moves a parameter to position `to` in the list.
///
/// # Errors
///
/// `procgen_unknown_param`.
pub fn reorder_param(g: &mut ProcGraph, name: &str, to: usize) -> Result<(), DocError> {
    let pos = g
        .params
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| DocError::new("procgen_unknown_param", format!("no parameter `{name}`")))?;
    let p = g.params.remove(pos);
    let to = to.min(g.params.len());
    g.params.insert(to, p);
    Ok(())
}

/// Sets a parameter's value from the Parameters tab: on an open variant it writes (or with `None`
/// removes, "reset") the variant's override; on a graph it sets the declared default.
///
/// # Errors
///
/// `procgen_unknown_param`, `procgen_param_value` (a graph default cannot be reset).
pub fn set_param_value(
    ctx: &mut EditCtx<'_>,
    name: &str,
    value: Option<SettingValue>,
) -> Result<(), DocError> {
    if ctx.graph.param(name).is_none() {
        return Err(DocError::new(
            "procgen_unknown_param",
            format!("no parameter `{name}`"),
        ));
    }
    match (ctx.overrides.as_deref_mut(), value) {
        (Some(o), Some(v)) => {
            o.insert(name.to_string(), v);
        }
        (Some(o), None) => {
            o.remove(name);
        }
        (None, Some(v)) => {
            if let Some(p) = ctx.graph.params.iter_mut().find(|p| p.name == name) {
                p.default = v;
            }
        }
        (None, None) => {
            return Err(DocError::new(
                "procgen_param_value",
                "a graph parameter's default cannot be reset (only a variant override can)",
            ));
        }
    }
    Ok(())
}

/// Binds a setting to a parameter (writes `Bind(param)`). With `create`, the parameter is
/// declared first, its default taken from the setting's current value ("promote to parameter").
///
/// # Errors
///
/// `procgen_unknown_node`, `procgen_unknown_param`, `procgen_param_name`.
pub fn bind_setting(
    ctx: &mut EditCtx<'_>,
    id: u32,
    setting: &str,
    param: &str,
    create: Option<regia_procgen::AttrType>,
) -> Result<(), DocError> {
    if let Some(ty) = create {
        let current = current_setting(ctx.graph, ctx.registry, id, setting)?;
        add_param(
            ctx.graph,
            ParamDecl {
                name: param.to_string(),
                ty,
                default: current,
                min: None,
                max: None,
                doc: String::new(),
            },
        )?;
    } else if ctx.graph.param(param).is_none() {
        return Err(DocError::new(
            "procgen_unknown_param",
            format!("no parameter `{param}`"),
        ));
    }
    node(ctx.graph, id)?
        .settings
        .insert(setting.to_string(), bind(param));
    Ok(())
}

/// The name the promote prompt starts with: the setting's name, else `<setting>_2`, `_3`, ... the
/// first that no parameter of `g` uses (spec 67 § 2.4, "promote to parameter").
#[must_use]
pub fn suggest_param_name(g: &ProcGraph, setting: &str) -> String {
    let mut name = setting.to_string();
    let mut k = 2;
    while g.param(&name).is_some() {
        name = format!("{setting}_{k}");
        k += 1;
    }
    name
}

/// The parameter type a setting of `node` promotes to (`None`: the setting cannot be a parameter).
pub(crate) fn promote_type(
    g: &ProcGraph,
    registry: &ProcNodeRegistry,
    node: u32,
    setting: &str,
) -> Option<AttrType> {
    use regia_procgen::nodes::SettingType as T;
    let ty = g
        .nodes
        .get(&node)
        .and_then(|n| registry.get(&n.ty))
        .and_then(|k| k.settings().iter().find(|d| d.name == setting).map(|d| d.ty))?;
    match ty {
        T::Float => Some(AttrType::Float),
        T::Int => Some(AttrType::Int),
        T::Bool => Some(AttrType::Bool),
        T::Vec3 => Some(AttrType::Vec3),
        T::Name | T::Enum(_) => Some(AttrType::Name),
        T::Asset => Some(AttrType::Asset),
        _ => None,
    }
}

/// Promote to parameter with the name prompt (spec 67 § 2.4): binds `setting` of `node` to the
/// parameter `name` (default [`suggest_param_name`]). With `existing` false a new parameter is
/// declared, typed after the setting with the setting's current value as its default; with
/// `existing` true the named parameter is picked and must already exist with that type. One undo
/// entry. Returns the parameter's name and whether it was declared.
///
/// # Errors
///
/// `procgen_not_open`, `procgen_read_only`, `procgen_unknown_node`, `unknown_setting`,
/// `procgen_param_name` (invalid or taken name), `procgen_unknown_param` (no such parameter to
/// pick), `procgen_param_type` (a setting kind that cannot be a parameter, or a picked parameter of
/// another type).
pub fn promote_setting(
    world: &mut World,
    node: u32,
    setting: &str,
    name: Option<&str>,
    existing: bool,
) -> Result<(String, bool), DocError> {
    let registry = registry(world);
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    if doc.read_only() {
        return Err(DocError::new("procgen_read_only", "a variant is open"));
    }
    let g = &doc.graph;
    let ty_name = g
        .nodes
        .get(&node)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {node}")))?
        .ty
        .clone();
    if !registry
        .get(&ty_name)
        .is_some_and(|k| k.settings().iter().any(|d| d.name == setting))
    {
        return Err(DocError::new(
            "unknown_setting",
            format!("`{ty_name}` has no setting `{setting}`"),
        ));
    }
    let ty = promote_type(g, &registry, node, setting).ok_or_else(|| {
        DocError::new(
            "procgen_param_type",
            format!(
                "`{setting}` of `{ty_name}` cannot be a parameter (curves, lists and ranges stay on the node)"
            ),
        )
    })?;
    let name = name.map_or_else(|| suggest_param_name(g, setting), str::to_string);
    if existing {
        let p = g
            .param(&name)
            .ok_or_else(|| DocError::new("procgen_unknown_param", format!("no parameter `{name}`")))?;
        if p.ty != ty {
            return Err(DocError::new(
                "procgen_param_type",
                format!("`{name}` is {}, `{setting}` needs {}", p.ty.name(), ty.name()),
            ));
        }
    } else if !regia_procgen::graph::valid_param_name(&name) || g.param(&name).is_some() {
        return Err(DocError::new(
            "procgen_param_name",
            format!("`{name}` is not a new valid parameter name"),
        ));
    }
    edit(world, "promote to parameter", None, |c| {
        bind_setting(c, node, setting, &name, (!existing).then_some(ty))
    })?;
    Ok((name, !existing))
}

/// The value a setting has now: written, else the type's default.
fn current_setting(
    g: &ProcGraph,
    registry: &ProcNodeRegistry,
    id: u32,
    setting: &str,
) -> Result<SettingValue, DocError> {
    let n = g
        .nodes
        .get(&id)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {id}")))?;
    if let Some(v) = n.settings.get(setting) {
        return Ok(v.clone());
    }
    registry
        .get(&n.ty)
        .and_then(|k| k.settings().iter().find(|d| d.name == setting).copied())
        .and_then(|d| SettingValue::parse(d.default).ok())
        .ok_or_else(|| {
            DocError::new(
                "unknown_setting",
                format!("`{}` has no setting `{setting}`", n.ty),
            )
        })
}

/// Unbinds a setting: it gets the bound parameter's default as a literal value.
///
/// # Errors
///
/// `procgen_unknown_node`, `procgen_not_bound`.
pub fn unbind_setting(g: &mut ProcGraph, id: u32, setting: &str) -> Result<(), DocError> {
    let param = node(g, id)?
        .settings
        .get(setting)
        .and_then(|v| v.as_bind().map(str::to_string))
        .ok_or_else(|| DocError::new("procgen_not_bound", format!("`{setting}` is not bound")))?;
    let default = g
        .param(&param)
        .map_or(SettingValue::Ident("None".into()), |p| p.default.clone());
    node(g, id)?.settings.insert(setting.to_string(), default);
    Ok(())
}

/// Shows or hides the override pin `@<setting>` of node `id` ("show as pin"). Showing is editor
/// state; hiding also removes the edge into the pin (one undo entry when there is one).
///
/// # Errors
///
/// `procgen_not_open`, `procgen_unknown_node`, `unknown_setting`.
pub fn show_pin(world: &mut World, id: u32, setting: &str, show: bool) -> Result<(), DocError> {
    let registry = registry(world);
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let n = doc
        .graph
        .nodes
        .get(&id)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {id}")))?;
    let known = registry
        .get(&n.ty)
        .is_some_and(|k| k.settings().iter().any(|d| d.name == setting));
    if !known {
        return Err(DocError::new(
            "unknown_setting",
            format!("`{}` has no setting `{setting}`", n.ty),
        ));
    }
    let pin = format!("@{setting}");
    if !show {
        edit(world, "hide override pin", None, |c| {
            c.graph.edges.retain(|e| !(e.to.0 == id && e.to.1 == pin));
            Ok(())
        })?;
    }
    let mut doc = world.resource_mut::<ProcGraphDoc>();
    if show {
        doc.shown_pins.insert((id, setting.to_string()));
    } else {
        doc.shown_pins.remove(&(id, setting.to_string()));
    }
    doc.revision += 1;
    Ok(())
}

/// Adds a canvas note; returns its id.
pub fn add_note(g: &mut ProcGraph, text: &str, at: (f64, f64)) -> u32 {
    let mut n = NodeEntry::new("note");
    n.settings
        .insert("text".into(), SettingValue::Str(text.to_string()));
    n.at = at;
    let id = g.next_node_id();
    g.nodes.insert(id, n);
    id
}

/// Adds a reroute (an editor aid the compiler collapses); returns its id.
pub fn add_reroute(g: &mut ProcGraph, at: (f64, f64)) -> u32 {
    let mut n = NodeEntry::new("reroute");
    n.at = at;
    let id = g.next_node_id();
    g.nodes.insert(id, n);
    id
}

/// The selected nodes and the edges between them as a RON fragment (a graph file holding only
/// them), for the clipboard.
#[must_use]
pub fn copy_fragment(g: &ProcGraph, ids: &BTreeSet<u32>) -> String {
    let mut frag = ProcGraph::default();
    for id in ids {
        if let Some(n) = g.nodes.get(id) {
            frag.nodes.insert(*id, n.clone());
        }
    }
    frag.edges = g
        .edges
        .iter()
        .filter(|e| ids.contains(&e.from.0) && ids.contains(&e.to.0))
        .cloned()
        .collect();
    frag.to_ron()
}

/// Pastes a fragment with fresh ids, offset by `offset` on the canvas; returns the new ids in the
/// fragment's id order.
///
/// # Errors
///
/// `procgen_paste_invalid` when the text is not a graph fragment.
pub fn paste_fragment(g: &mut ProcGraph, text: &str, offset: (f64, f64)) -> Result<Vec<u32>, DocError> {
    let frag = ProcGraph::from_ron(text)
        .map_err(|e| DocError::new("procgen_paste_invalid", format!("not a graph fragment: {e}")))?;
    let mut map = BTreeMap::new();
    let mut out = Vec::new();
    for (old, n) in &frag.nodes {
        let id = g.next_node_id();
        let mut n = n.clone();
        n.at = (n.at.0 + offset.0, n.at.1 + offset.1);
        // A pasted node keeps its random stream (seed salt) only when it had an explicit one.
        g.nodes.insert(id, n);
        map.insert(*old, id);
        out.push(id);
    }
    for e in &frag.edges {
        if let (Some(&a), Some(&b)) = (map.get(&e.from.0), map.get(&e.to.0)) {
            g.edges.push(Edge::new(a, &e.from.1, b, &e.to.1));
        }
    }
    Ok(out)
}

/// Lines selected nodes up: `vertical` aligns their x to the first node's, else their y.
pub fn align(g: &mut ProcGraph, ids: &BTreeSet<u32>, vertical: bool) {
    let Some(first) = ids.iter().find_map(|id| g.nodes.get(id).map(|n| n.at)) else {
        return;
    };
    for id in ids {
        if let Some(n) = g.nodes.get_mut(id) {
            if vertical {
                n.at.0 = first.0;
            } else {
                n.at.1 = first.1;
            }
        }
    }
}

// ---- Starters (spec 67 § 2.3) ----

/// One starter graph of the "New procedural graph" picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StarterInfo {
    /// Project-relative path.
    pub path: String,
    /// `flags.tool` (a placement tool kind), when set.
    pub tool: Option<String>,
    /// `flags.domain` name.
    pub domain: String,
    /// The graph's description.
    pub description: String,
}

/// The project's starter graphs (`flags.starter` true), sorted by path.
#[must_use]
pub fn starters(world: &World) -> Vec<StarterInfo> {
    project_graphs(world)
        .into_iter()
        .filter_map(|(path, text)| {
            let g = ProcGraph::from_ron(&text).ok()?;
            g.flags.starter.then(|| StarterInfo {
                path,
                tool: g.flags.tool.clone(),
                domain: g.flags.domain.name().to_string(),
                description: g.description.clone(),
            })
        })
        .collect()
}

/// "Make starter": sets the open graph's `flags.starter` (one undo entry).
///
/// # Errors
///
/// As [`edit`].
pub fn set_starter(world: &mut World, starter: bool) -> Result<(), DocError> {
    edit(world, "make starter", None, |c| {
        c.graph.flags.starter = starter;
        Ok(())
    })
}

// ---- Palette (spec 67 § 2.1) ----

/// One palette entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    /// Node type id.
    pub ty: String,
    /// Category.
    pub category: String,
    /// One-line doc.
    pub doc: String,
    /// With a `from` kind filter: the first input pin that accepts it (dropping connects there).
    pub pin: Option<String>,
}

/// The node types matching `search` (over id, category and doc, case-insensitive), and with
/// `from` only those with an input pin accepting one of those kinds (the drop-from-pin filter).
/// Sorted by category then id.
#[must_use]
pub fn palette(registry: &ProcNodeRegistry, search: &str, from: Option<KindSet>) -> Vec<PaletteEntry> {
    let q = search.to_lowercase();
    let mut out: Vec<PaletteEntry> = registry
        .iter()
        .filter(|k| {
            q.is_empty()
                || k.id().to_lowercase().contains(&q)
                || k.category().to_lowercase().contains(&q)
                || k.doc().to_lowercase().contains(&q)
        })
        .filter_map(|k| {
            let pin = match from {
                None => None,
                Some(kinds) => {
                    let defaults = regia_procgen::nodes::Settings(
                        k.settings()
                            .iter()
                            .filter_map(|d| {
                                SettingValue::parse(d.default)
                                    .ok()
                                    .map(|v| (d.name.to_string(), v))
                            })
                            .collect(),
                    );
                    let pins = k.pins(&defaults);
                    Some(pins.inputs.into_iter().find(|p| p.kinds.intersects(kinds))?.name)
                }
            };
            Some(PaletteEntry {
                ty: k.id().to_string(),
                category: k.category().to_string(),
                doc: k.doc().to_string(),
                pin,
            })
        })
        .collect();
    out.sort_by(|a, b| (a.category.as_str(), a.ty.as_str()).cmp(&(b.category.as_str(), b.ty.as_str())));
    out
}

/// The kinds an output pin of the open graph produces.
#[must_use]
pub fn output_kinds(world: &World, node: u32, pin: &str) -> Option<KindSet> {
    let doc = world.get_resource::<ProcGraphDoc>()?;
    let registry = registry(world);
    let load = |p: &str| graph_text(world, p);
    doc.graph
        .node_pins(node, &registry, &load)?
        .outputs
        .into_iter()
        .find(|o| o.name == pin)
        .map(|o| o.kinds)
}

// ---- Called graphs (spec 67 § 2.4 "Open called graph") ----

/// Opens the graph a `graph.call` / `graph.loop` node calls; the breadcrumb gains the current
/// graph so [`back`] returns to it.
///
/// # Errors
///
/// `procgen_not_open`, `procgen_unknown_node`, `procgen_not_call`, and [`open`]'s errors.
pub fn enter_call(world: &mut World, node: u32) -> Result<String, DocError> {
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let here = doc
        .path
        .clone()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let n = doc
        .graph
        .nodes
        .get(&node)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {node}")))?;
    if !CALL_NODES.contains(&n.ty.as_str()) {
        return Err(DocError::new(
            "procgen_not_call",
            format!("node {node} is `{}`, not a graph call", n.ty),
        ));
    }
    let target = n
        .settings
        .get("graph")
        .and_then(SettingValue::as_text)
        .unwrap_or("")
        .to_string();
    let mut crumbs = doc.breadcrumb.clone();
    open(world, &target)?;
    crumbs.push(here);
    world.resource_mut::<ProcGraphDoc>().breadcrumb = crumbs;
    Ok(target)
}

/// Returns to the graph above in the breadcrumb (`None` when at the top).
///
/// # Errors
///
/// [`open`]'s errors.
pub fn back(world: &mut World) -> Result<Option<String>, DocError> {
    let Some(mut crumbs) = world.get_resource::<ProcGraphDoc>().map(|d| d.breadcrumb.clone()) else {
        return Ok(None);
    };
    let Some(up) = crumbs.pop() else {
        return Ok(None);
    };
    open(world, &up)?;
    world.resource_mut::<ProcGraphDoc>().breadcrumb = crumbs;
    Ok(Some(up))
}

// ---- Seed (spec 67 § 3) ----

/// A generator's pinned seed before and after (one undo entry).
pub struct SetGeneratorSeed {
    entity: u64,
    before: Option<u32>,
    after: Option<u32>,
}

impl SetGeneratorSeed {
    fn set(world: &mut World, bits: u64, seed: Option<u32>) {
        let Some(e) = crate::command::resolve_entity(world, bits) else {
            return;
        };
        if let Some(mut g) = world.get_mut::<regia_procgen::world::ProcGenerator>(e) {
            g.seed = seed;
        }
        regia_procgen::world::request_run(world, e);
    }
}

impl EditorCommand for SetGeneratorSeed {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.entity, self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.entity, self.before);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Procgen: generator seed".to_string()
    }
}

/// What the preview bar's seed buttons and `editor.procgen_seed` do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedAction {
    /// Pin the seed now in effect.
    Pin,
    /// Clear the pin (the guid-derived seed is in effect again).
    Unpin,
    /// Pin a new seed derived from the one now in effect.
    New,
    /// Pin this value.
    Set(u32),
    /// Pin the seed of another generator.
    SameAs(Entity),
}

/// Applies a seed action and returns `(pinned, seed now in effect)`. `pinned` is `None` for unpin.
/// New is `pcg_hash(current ^ 0x9E3779B9)`, the same mix `editor.procgen_seed` has always used.
///
/// # Errors
///
/// `procgen_not_generator` when `entity` (or the `SameAs` source) has no generator.
pub fn apply_seed_action(
    world: &mut World,
    entity: Entity,
    action: SeedAction,
) -> Result<(Option<u32>, u32), DocError> {
    let seed = match action {
        SeedAction::Pin => Some(regia_procgen::world::generator_seed(world, entity)),
        SeedAction::Unpin => None,
        SeedAction::New => Some(regia_core::math::hash::pcg_hash(
            regia_procgen::world::generator_seed(world, entity) ^ 0x9e37_79b9,
        )),
        SeedAction::Set(value) => Some(value),
        SeedAction::SameAs(from) => {
            if world.get::<regia_procgen::world::ProcGenerator>(from).is_none() {
                return Err(DocError::new(
                    "procgen_not_generator",
                    "`from` is another generator",
                ));
            }
            Some(regia_procgen::world::generator_seed(world, from))
        }
    };
    let effective = set_seed(world, entity, seed)?;
    Ok((seed, effective))
}

/// Seed actions of the preview bar: pin the current seed, a new seed, the same seed as another
/// generator, or unpin (`None`). Returns the seed now in effect.
///
/// # Errors
///
/// `procgen_not_generator`.
pub fn set_seed(world: &mut World, e: Entity, seed: Option<u32>) -> Result<u32, DocError> {
    let before = world
        .get::<regia_procgen::world::ProcGenerator>(e)
        .ok_or_else(|| {
            DocError::new(
                "procgen_not_generator",
                format!("entity {} has no ProcGenerator", e.to_bits()),
            )
        })?
        .seed;
    let mut command = SetGeneratorSeed {
        entity: e.to_bits(),
        before,
        after: seed,
    };
    command.apply(world);
    world
        .get_resource_or_insert_with(UndoStack::new)
        .push(Box::new(command));
    Ok(regia_procgen::world::generator_seed(world, e))
}

// ---- Collapse and expand (spec 67 § 2.4) ----

/// What [`collapse`] made.
#[derive(Clone, Debug, PartialEq)]
pub struct Collapsed {
    /// The `graph.call` node that replaced the selection.
    pub call_node: u32,
    /// The new graph (to be written at the collapse path).
    pub graph: ProcGraph,
}

fn kinds_of_output(g: &ProcGraph, registry: &ProcNodeRegistry, id: u32, pin: &str) -> KindSet {
    let load = |_: &str| None;
    g.node_pins(id, registry, &load)
        .and_then(|p| p.outputs.into_iter().find(|o| o.name == pin))
        .map_or_else(|| KindSet::of(&regia_procgen::DataKind::ALL), |p| p.kinds)
}

/// The collapse boundary check (spec 67 § 6, `procgen_collapse_invalid` "no single connected
/// boundary", ADR-7239): the selected nodes, notes left out, must form one group connected by
/// edges between them, and no path may leave the selection and come back into it through outside
/// nodes (the `graph.call` that replaces the selection would then feed itself, a cycle).
///
/// # Errors
///
/// `procgen_collapse_invalid` naming the second group's node, or the outside node on a path that
/// re-enters the selection.
pub fn collapse_check(g: &ProcGraph, ids: &BTreeSet<u32>) -> Result<(), DocError> {
    let invalid = |m: String| DocError::new("procgen_collapse_invalid", m);
    let nodes: BTreeSet<u32> = ids
        .iter()
        .copied()
        .filter(|id| g.nodes.get(id).is_some_and(|n| n.ty != "note"))
        .collect();
    let Some(&first) = nodes.iter().next() else {
        return Ok(());
    };
    // No re-entering path: walk forward from the selection's outside successors; reaching a
    // selected node means a path out and back in.
    let mut seen = BTreeSet::new();
    let mut stack: Vec<(u32, u32)> = g
        .edges
        .iter()
        .filter(|e| nodes.contains(&e.from.0) && !ids.contains(&e.to.0))
        .map(|e| (e.to.0, e.to.0))
        .collect();
    while let Some((at, via)) = stack.pop() {
        if !seen.insert(at) {
            continue;
        }
        for e in g.edges.iter().filter(|e| e.from.0 == at) {
            if nodes.contains(&e.to.0) {
                return Err(invalid(format!(
                    "a path leaves the selection and comes back through node {via}: collapsing would \
                     make the call feed itself (add node {via} to the selection)"
                )));
            }
            if !ids.contains(&e.to.0) {
                stack.push((e.to.0, via));
            }
        }
    }
    // One connected group: a flood over edges with both ends selected, either direction.
    let mut reached = BTreeSet::from([first]);
    let mut stack = vec![first];
    while let Some(at) = stack.pop() {
        for e in &g.edges {
            let next = if e.from.0 == at {
                e.to.0
            } else if e.to.0 == at {
                e.from.0
            } else {
                continue;
            };
            if nodes.contains(&next) && reached.insert(next) {
                stack.push(next);
            }
        }
    }
    if let Some(other) = nodes.iter().find(|id| !reached.contains(id)) {
        return Err(invalid(format!(
            "the selection is not one connected group: node {other} is not connected to node {first} \
             through selected nodes"
        )));
    }
    Ok(())
}

/// Moves `ids` into a new graph saved at `path` and replaces them with one `graph.call`. Edges
/// entering the selection become the new graph's inputs (one per outside source pin, read through
/// `graph.input` nodes), edges leaving it its outputs (one per inside source pin, through
/// `graph.output` nodes). Inside nodes keep their ids and seed salts, so the run is unchanged
/// (seeds depend on the salt, spec 62 § 6.2); the call node takes the smallest selected id so the
/// evaluation order (ties by id) stays the same.
///
/// # Errors
///
/// `procgen_collapse_invalid`: an empty selection, a missing node, a selection holding graph
/// inputs/outputs (`graph.input`, `graph.output`) or graph-reading nodes (`in.*` are allowed), or
/// one that fails [`collapse_check`] (not one connected group, or a path out and back in).
pub fn collapse(
    g: &mut ProcGraph,
    registry: &ProcNodeRegistry,
    ids: &BTreeSet<u32>,
    path: &str,
) -> Result<Collapsed, DocError> {
    let invalid = |m: String| DocError::new("procgen_collapse_invalid", m);
    if ids.is_empty() {
        return Err(invalid("select the nodes to collapse".into()));
    }
    for id in ids {
        let n = g.nodes.get(id).ok_or_else(|| invalid(format!("no node {id}")))?;
        if matches!(n.ty.as_str(), "graph.input" | "graph.output" | "graph.loop_index") {
            return Err(invalid(format!(
                "node {id} (`{}`) is a graph input or output and cannot move into a subgraph",
                n.ty
            )));
        }
    }
    if !path.ends_with(".pgraph.ron") {
        return Err(invalid("the new graph's path ends in `.pgraph.ron`".into()));
    }
    collapse_check(g, ids)?;
    // The parameters the moved nodes bind: declared in the new graph, forwarded by the call.
    let mut bound = BTreeSet::new();
    for id in ids {
        if let Some(n) = g.nodes.get(id) {
            for v in n.settings.values() {
                binds_in(v, &mut bound);
            }
        }
    }
    let mut sub = ProcGraph {
        params: g
            .params
            .iter()
            .filter(|p| bound.contains(&p.name))
            .cloned()
            .collect(),
        flags: regia_procgen::graph::GraphFlags {
            domain: g.flags.domain,
            ..Default::default()
        },
        description: format!("collapsed from {} node(s)", ids.len()),
        ..ProcGraph::default()
    };
    for id in ids {
        if let Some(n) = g.nodes.get(id) {
            sub.nodes.insert(*id, n.clone());
        }
    }
    let incoming: Vec<Edge> = g
        .edges
        .iter()
        .filter(|e| !ids.contains(&e.from.0) && ids.contains(&e.to.0))
        .cloned()
        .collect();
    let outgoing: Vec<Edge> = g
        .edges
        .iter()
        .filter(|e| ids.contains(&e.from.0) && !ids.contains(&e.to.0))
        .cloned()
        .collect();
    sub.edges = g
        .edges
        .iter()
        .filter(|e| ids.contains(&e.from.0) && ids.contains(&e.to.0))
        .cloned()
        .collect();

    // Ids for the proxy nodes: an outside source keeps its own id when free (multi-input order is
    // by source id), otherwise the next free one.
    let mut used: BTreeSet<u32> = ids.clone();
    let max = g.nodes.keys().next_back().copied().unwrap_or(0);
    let mut next = max + 1;
    let mut fresh = |used: &mut BTreeSet<u32>, wanted: Option<u32>| {
        if let Some(w) = wanted
            && used.insert(w)
        {
            return w;
        }
        while used.contains(&next) {
            next += 1;
        }
        used.insert(next);
        next
    };

    // Inputs: one per distinct outside source pin.
    let mut sources: Vec<(u32, String)> = incoming.iter().map(|e| e.from.clone()).collect();
    sources.sort();
    sources.dedup();
    let mut call_inputs: Vec<(String, (u32, String))> = Vec::new();
    let min_x = ids
        .iter()
        .filter_map(|id| g.nodes.get(id).map(|n| n.at.0))
        .fold(f64::INFINITY, f64::min);
    for (k, src) in sources.iter().enumerate() {
        let pin = format!("in_{k}");
        let proxy = fresh(&mut used, Some(src.0));
        let mut n = NodeEntry::new("graph.input");
        n.settings.insert("pin".into(), SettingValue::Str(pin.clone()));
        n.at = (min_x - 240.0, 120.0 * k as f64);
        sub.nodes.insert(proxy, n);
        for e in incoming.iter().filter(|e| e.from == *src) {
            sub.edges.push(Edge::new(proxy, "out", e.to.0, &e.to.1));
        }
        sub.inputs.push(GraphPin {
            name: pin.clone(),
            kinds: kinds_of_output(g, registry, src.0, &src.1),
        });
        call_inputs.push((pin, src.clone()));
    }
    // Outputs: one per distinct inside source pin that leaves the selection.
    let mut outs: Vec<(u32, String)> = outgoing.iter().map(|e| e.from.clone()).collect();
    outs.sort();
    outs.dedup();
    let max_x = ids
        .iter()
        .filter_map(|id| g.nodes.get(id).map(|n| n.at.0))
        .fold(f64::NEG_INFINITY, f64::max);
    let mut call_outputs: Vec<(String, (u32, String))> = Vec::new();
    for (k, src) in outs.iter().enumerate() {
        let pin = format!("out_{k}");
        let proxy = fresh(&mut used, None);
        let mut n = NodeEntry::new("graph.output");
        n.settings.insert("pin".into(), SettingValue::Str(pin.clone()));
        n.at = (max_x + 240.0, 120.0 * k as f64);
        sub.nodes.insert(proxy, n);
        sub.edges.push(Edge::new(src.0, &src.1, proxy, "in"));
        sub.outputs.push(GraphPin {
            name: pin.clone(),
            kinds: kinds_of_output(g, registry, src.0, &src.1),
        });
        call_outputs.push((pin, src.clone()));
    }

    // The outer graph: the call node takes the smallest selected id.
    let call_id = *ids.iter().next().unwrap_or(&g.next_node_id());
    let (cx, cy) = {
        let pts: Vec<(f64, f64)> = ids
            .iter()
            .filter_map(|id| g.nodes.get(id).map(|n| n.at))
            .collect();
        let n = pts.len().max(1) as f64;
        (
            pts.iter().map(|p| p.0).sum::<f64>() / n,
            pts.iter().map(|p| p.1).sum::<f64>() / n,
        )
    };
    remove_nodes(g, ids);
    let mut call = NodeEntry::new("graph.call");
    call.settings
        .insert("graph".into(), SettingValue::Str(path.to_string()));
    if !bound.is_empty() {
        call.settings.insert(
            "params".into(),
            SettingValue::Record(None, bound.iter().map(|p| (p.clone(), bind(p))).collect()),
        );
    }
    call.at = (cx, cy);
    g.nodes.insert(call_id, call);
    for (pin, src) in &call_inputs {
        g.edges.push(Edge::new(src.0, &src.1, call_id, pin));
    }
    for (pin, src) in &call_outputs {
        for e in outgoing.iter().filter(|e| e.from == *src) {
            g.edges.push(Edge::new(call_id, pin, e.to.0, &e.to.1));
        }
    }
    g.edges.sort();
    sub.edges.sort();
    Ok(Collapsed {
        call_node: call_id,
        graph: sub,
    })
}

/// Replaces the `graph.call` node `call` with the called graph's nodes (the inverse of
/// [`collapse`]): inner nodes keep their ids when free in this graph (fresh ids otherwise), the
/// `graph.input` / `graph.output` proxies are dissolved into direct edges. Returns the inserted ids.
///
/// # Errors
///
/// `procgen_unknown_node`, `procgen_not_call` (not a `graph.call`), `procgen_graph_invalid` (the
/// called graph has parameters this graph lacks, or uses `graph.loop_index`).
pub fn expand(g: &mut ProcGraph, call: u32, called: &ProcGraph) -> Result<Vec<u32>, DocError> {
    let n = g
        .nodes
        .get(&call)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {call}")))?;
    if n.ty != "graph.call" {
        return Err(DocError::new(
            "procgen_not_call",
            format!("node {call} is `{}`, not `graph.call`", n.ty),
        ));
    }
    for p in &called.params {
        if g.param(&p.name).is_none() {
            return Err(DocError::new(
                "procgen_graph_invalid",
                format!("the called graph's parameter `{}` is not declared here", p.name),
            ));
        }
    }
    let pin_of = |n: &NodeEntry| {
        n.settings
            .get("pin")
            .and_then(SettingValue::as_text)
            .map(str::to_string)
    };
    let mut input_proxies: BTreeMap<u32, String> = BTreeMap::new();
    let mut output_proxies: BTreeMap<u32, String> = BTreeMap::new();
    for (id, n) in &called.nodes {
        match n.ty.as_str() {
            "graph.input" => {
                input_proxies.insert(*id, pin_of(n).unwrap_or_else(|| "in".into()));
            }
            "graph.output" => {
                output_proxies.insert(*id, pin_of(n).unwrap_or_else(|| "out".into()));
            }
            "graph.loop_index" => {
                return Err(DocError::new(
                    "procgen_graph_invalid",
                    "a loop body cannot be expanded inline",
                ));
            }
            _ => {}
        }
    }
    let into_call: Vec<Edge> = g.edges.iter().filter(|e| e.to.0 == call).cloned().collect();
    let out_of_call: Vec<Edge> = g.edges.iter().filter(|e| e.from.0 == call).cloned().collect();
    g.nodes.remove(&call);
    g.edges.retain(|e| e.to.0 != call && e.from.0 != call);

    let mut map: BTreeMap<u32, u32> = BTreeMap::new();
    let mut inserted = Vec::new();
    for (id, n) in &called.nodes {
        if input_proxies.contains_key(id) || output_proxies.contains_key(id) {
            continue;
        }
        let new_id = if g.nodes.contains_key(id) {
            g.next_node_id()
        } else {
            *id
        };
        let mut n = n.clone();
        if new_id != *id && n.seed_salt.is_none() {
            // Keep the random stream the node had in the called graph.
            n.seed_salt = Some(*id);
        }
        g.nodes.insert(new_id, n);
        map.insert(*id, new_id);
        inserted.push(new_id);
    }
    for e in &called.edges {
        match (map.get(&e.from.0), map.get(&e.to.0)) {
            (Some(&a), Some(&b)) => g.edges.push(Edge::new(a, &e.from.1, b, &e.to.1)),
            (None, Some(&b)) => {
                // From an input proxy: every caller edge into that pin.
                if let Some(pin) = input_proxies.get(&e.from.0) {
                    for c in into_call.iter().filter(|c| c.to.1 == *pin) {
                        g.edges.push(Edge::new(c.from.0, &c.from.1, b, &e.to.1));
                    }
                }
            }
            (Some(&a), None) => {
                // Into an output proxy: to every caller edge out of that pin.
                if let Some(pin) = output_proxies.get(&e.to.0) {
                    for c in out_of_call.iter().filter(|c| c.from.1 == *pin) {
                        g.edges.push(Edge::new(a, &e.from.1, c.to.0, &c.to.1));
                    }
                }
            }
            (None, None) => {}
        }
    }
    g.edges.sort();
    g.edges.dedup();
    Ok(inserted)
}

/// Collapses `ids` of the open graph into a new graph file at `path` (written first, so the call
/// node's pins resolve) and replaces them with one `graph.call` (one undo entry; the new file is
/// not removed by undo). Returns the call node id.
///
/// # Errors
///
/// `procgen_not_open`, `procgen_read_only`, `procgen_collapse_invalid`, IO.
pub fn collapse_selection(world: &mut World, ids: &BTreeSet<u32>, path: &str) -> Result<u32, DocError> {
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    if doc.read_only() {
        return Err(DocError::new("procgen_read_only", "a variant is open"));
    }
    let registry = registry(world);
    let mut probe = doc.graph.clone();
    let made = collapse(&mut probe, &registry, ids, path)?;
    write_source(world, path, &made.graph.to_ron())?;
    let id = edit(world, "collapse to subgraph", None, |ctx| {
        collapse(ctx.graph, ctx.registry, ids, path).map(|c| c.call_node)
    })?;
    if let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() {
        doc.selection = BTreeSet::from([id]);
    }
    Ok(id)
}

/// Expands the `graph.call` node `call` of the open graph (one undo entry). A called graph used by
/// other files or other nodes needs `copy` (its file is left in place either way).
///
/// # Errors
///
/// `procgen_not_open`, `procgen_unknown_node`, `procgen_not_call`, `procgen_unknown_graph`,
/// `procgen_expand_shared` (used elsewhere and `copy` is false), `procgen_graph_invalid`.
pub fn expand_call(world: &mut World, call: u32, copy: bool) -> Result<Vec<u32>, DocError> {
    let doc = world
        .get_resource::<ProcGraphDoc>()
        .ok_or_else(|| DocError::new("procgen_not_open", "no procedural graph is open"))?;
    let n = doc
        .graph
        .nodes
        .get(&call)
        .ok_or_else(|| DocError::new("procgen_unknown_node", format!("no node {call}")))?;
    if n.ty != "graph.call" {
        return Err(DocError::new(
            "procgen_not_call",
            format!("node {call} is `{}`, not `graph.call`", n.ty),
        ));
    }
    let target = n
        .settings
        .get("graph")
        .and_then(SettingValue::as_text)
        .unwrap_or("")
        .to_string();
    let here = doc.path.clone().unwrap_or_default();
    let other_nodes = doc.graph.nodes.iter().any(|(id, x)| {
        *id != call
            && CALL_NODES.contains(&x.ty.as_str())
            && x.settings.get("graph").and_then(SettingValue::as_text) == Some(target.as_str())
    });
    let callers = callers_of(world, &target, &here);
    if !copy && (other_nodes || !callers.is_empty()) {
        return Err(DocError::new(
            "procgen_expand_shared",
            format!(
                "`{target}` is also used by {}; pass copy: true to expand a copy",
                if callers.is_empty() {
                    "other nodes here".to_string()
                } else {
                    callers.join(", ")
                }
            ),
        ));
    }
    let text = graph_text(world, &target)
        .ok_or_else(|| DocError::new("procgen_unknown_graph", format!("cannot read `{target}`")))?;
    let called = ProcGraph::from_ron(&text)
        .map_err(|e| DocError::new("procgen_graph_invalid", format!("{target}: {e}")))?;
    let ids = edit(world, "expand subgraph", None, |ctx| {
        expand(ctx.graph, call, &called)
    })?;
    if let Some(mut doc) = world.get_resource_mut::<ProcGraphDoc>() {
        doc.selection = ids.iter().copied().collect();
    }
    Ok(ids)
}

/// Every `.pgraph.ron` under the graph source, `(project-relative path, text)`, sorted by path
/// (a directory walk skips hidden directories and `target`, at most 20 000 entries).
pub(crate) fn project_graphs(world: &World) -> Vec<(String, String)> {
    let Some(settings) = world.get_resource::<ProcGenSettings>() else {
        return Vec::new();
    };
    let mut files: Vec<(String, String)> = match &settings.source {
        ProcGraphSource::Memory(files) => files
            .iter()
            .filter(|(k, _)| k.ends_with(".pgraph.ron"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ProcGraphSource::Dir(root) => {
            let mut out = Vec::new();
            let mut stack = vec![root.clone()];
            let mut seen = 0usize;
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in rd.flatten() {
                    seen += 1;
                    if seen > 20_000 {
                        break;
                    }
                    let p = entry.path();
                    if p.is_dir() {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if !name.starts_with('.') && name != "target" {
                            stack.push(p);
                        }
                    } else if p.to_string_lossy().ends_with(".pgraph.ron")
                        && let (Ok(rel), Ok(text)) = (p.strip_prefix(root), std::fs::read_to_string(&p))
                    {
                        out.push((rel.to_string_lossy().replace('\\', "/"), text));
                    }
                }
            }
            out
        }
    };
    files.sort();
    files
}

/// Graph files under the source that call `path` (`graph.call` / `graph.loop` with that literal
/// path), other than `except`.
pub(crate) fn callers_of(world: &World, path: &str, except: &str) -> Vec<String> {
    let files = project_graphs(world);
    let mut callers: Vec<String> = files
        .into_iter()
        .filter(|(k, _)| k != except)
        .filter(|(_, text)| {
            ProcGraph::from_ron(text).is_ok_and(|g| {
                g.nodes.values().any(|n| {
                    CALL_NODES.contains(&n.ty.as_str())
                        && n.settings.get("graph").and_then(SettingValue::as_text) == Some(path)
                })
            })
        })
        .map(|(k, _)| k)
        .collect();
    callers.sort();
    callers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_counts_nested_binds() {
        let mut g = ProcGraph::default();
        g.params.push(ParamDecl {
            name: "a".into(),
            ty: regia_procgen::AttrType::Float,
            default: SettingValue::Float(1.0),
            min: None,
            max: None,
            doc: String::new(),
        });
        let mut n = NodeEntry::new("points.jitter");
        n.settings.insert("amount".into(), bind("a"));
        n.settings.insert(
            "offset".into(),
            SettingValue::List(vec![bind("a"), SettingValue::Float(0.0)]),
        );
        g.nodes.insert(1, n);
        assert_eq!(rename_param(&mut g, "a", "b").expect("rename"), 2);
        assert!(g.param("b").is_some());
        assert_eq!(g.nodes[&1].settings["amount"].as_bind(), Some("b"));
        remove_param(&mut g, "b").expect("remove");
        assert_eq!(g.nodes[&1].settings["amount"], SettingValue::Float(1.0));
    }

    #[test]
    fn fragments_round_trip_with_fresh_ids() {
        let mut g = blank_graph();
        let frag = copy_fragment(&g, &BTreeSet::from([2, 3]));
        let ids = paste_fragment(&mut g, &frag, (10.0, 20.0)).expect("paste");
        assert_eq!(ids, vec![4, 5]);
        assert_eq!(g.nodes[&4].at, (290.0, 20.0));
        assert!(g.edges.contains(&Edge::new(4, "out", 5, "in")));
        assert!(
            g.compile(&ProcNodeRegistry::with_builtins()).is_err(),
            "the pasted sampler has no region"
        );
        assert!(blank_graph().compile(&ProcNodeRegistry::with_builtins()).is_ok());
        assert!(paste_fragment(&mut g, "nope(", (0.0, 0.0)).is_err());
    }

    /// Audit zai M1: wiring a second source into a `graph.call` input whose subgraph cannot be
    /// loaded here keeps the first wire (subgraph inputs are multi).
    #[test]
    fn connect_keeps_wires_into_unresolved_call_pins() {
        let registry = ProcNodeRegistry::with_builtins();
        let mut g = ProcGraph::default();
        g.nodes.insert(1, regia_procgen::NodeEntry::new("gen.one"));
        g.nodes.insert(2, regia_procgen::NodeEntry::new("gen.one"));
        let call = regia_procgen::NodeEntry::new("graph.call")
            .with("graph", SettingValue::Str("missing.pgraph.ron".into()));
        g.nodes.insert(3, call);
        let mut ctx = EditCtx {
            graph: &mut g,
            overrides: None,
            registry: &registry,
        };
        connect(&mut ctx, (1, "out"), (3, "in_0")).expect("first");
        connect(&mut ctx, (2, "out"), (3, "in_0")).expect("second");
        assert_eq!(g.edges.iter().filter(|e| e.to.0 == 3).count(), 2, "{:?}", g.edges);
    }
}
