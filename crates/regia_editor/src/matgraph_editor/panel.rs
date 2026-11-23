// SPDX-License-Identifier: MPL-2.0
//! The Material Graph panel: toolbar (new/open/save, undo/redo, compile status), the node
//! canvas (egui-snarl) with a Surface sink, and a side column with the live preview, the
//! selected node's inspector (rename, RON edit, errors), the declaration blocks and the error
//! list. Edits are collected while drawing and applied through [`super::edit`] afterwards.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use egui_snarl::ui::{
    AnyPins, BackgroundPattern, PinInfo, PinWireInfo, SnarlPin, SnarlStyle, SnarlViewer, SnarlWidget,
    WireStyle,
};
use egui_snarl::{InPin, InPinId, OutPin, OutPinId, Snarl};
use regia_ecs::prelude::*;
use regia_matgraph::{FunctionSource, Node, ParamValue};
use regia_render::RenderDevice;

use super::canvas::{self, FrameGrip};
use super::clipboard::Arrange;
use super::function::Signature;
use super::inspector::{FieldKind, FieldValue};
use super::node_preview::NodePreviewCache;
use super::search::WireFrom;
use super::wire_hit::{DrawnWire, WireShape};
use super::{
    Diagnostics, DocMode, MatGraphDoc, MatGraphEditor, PendingEdit, Target, apply_pending, resolve_path,
};

/// A node on the canvas: a graph node by name, or the surface sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanvasNode {
    /// A graph node.
    Node(String),
    /// The surface outputs.
    Surface,
}

/// The panel's own state (not part of the document; not undone).
#[derive(Default)]
pub struct PanelState {
    /// The selected node.
    pub selected: Option<String>,
    /// The canvas, rebuilt when the revision changes.
    snarl: Option<Snarl<CanvasNode>>,
    /// The revision the canvas was built from.
    snarl_revision: Option<u64>,
    /// Where the Surface sink sits.
    surface_pos: Option<[f32; 2]>,
    /// RON buffer of the selected node: (node name, revision it was loaded at, text).
    node_text: Option<(String, u64, String)>,
    /// Rename buffer.
    rename: String,
    /// RON buffers of the declaration blocks: (revision, params, textures, switches, shading).
    decl_text: Option<(u64, [String; 4])>,
    /// Open / save-as path field.
    path_field: String,
    /// The preview: (revision, size, shape and view hash) it shows and the uploaded image.
    pub(crate) preview: Option<(PreviewKey, egui::TextureHandle)>,
    /// Why the preview of the current revision could not render.
    pub preview_error: Option<String>,
    /// The revision at which New or Open warned about unsaved work; the same button again at
    /// that revision discards it.
    discard_armed: Option<u64>,
    /// The last permutation-policy count: (graph revision, lines or the compile error).
    policy: Option<(u64, Result<Vec<String>, String>)>,
    /// A rename asked for this frame: (from, to); the selection moves once it is applied.
    rename_follow: Option<(String, String)>,
    /// Nodes picked with Shift+click for a collapse (with `selected`).
    pub multi: BTreeSet<String>,
    /// The collapse form: (function name, optional `.matfunc.ron` path).
    collapse_form: Option<(String, String)>,
    /// A function's signature as text: (graph revision, inputs RON, outputs list).
    signature_text: Option<(u64, String, String)>,
    /// What the preview draws the material on (ADR-9700).
    pub preview_shape: crate::material_editor::PreviewShape,
    /// The preview's orbit and light turn.
    pub preview_view: crate::material_editor::PreviewView,
    /// The project mesh path field of the preview.
    mesh_field: String,
    /// Show a swatch of each node's value on the canvas.
    pub node_previews: bool,
    /// The per-node swatches.
    node_cache: NodePreviewCache,
    /// The open node search: (query, graph position, the wire it adds to, screen position).
    pub search: Option<SearchState>,
    /// The dropped-wire menu's search text.
    wire_query: String,
    /// The canvas transform (graph to screen) of the last frame.
    to_global: Option<egui::emath::TSTransform>,
    /// The canvas's box selection as last seen (names), to notice a new one.
    box_seen: Vec<String>,
    /// Copy the editor clipboard to the system clipboard on the next draw.
    export_clipboard: bool,
    /// Texture thumbnails by file path (`None`: it does not load).
    thumbs: HashMap<std::path::PathBuf, Option<egui::TextureHandle>>,
    /// The comment-frame or reroute-point drag in progress on the canvas (ADR-9970).
    drag: Option<CanvasDrag>,
    /// The comment whose text is being edited on the canvas: (index, text).
    comment_text: Option<(usize, String)>,
    /// The rerouted wires the canvas drew last frame, as polylines (graph space).
    reroute_paths: Vec<Vec<egui::Pos2>>,
    /// Every wire into a node input the canvas drew last frame (graph space), plain and
    /// rerouted, for the wire hit test (spec 80 § 12.1, w11).
    pub wires: Vec<super::wire_hit::DrawnWire>,
    /// The WGSL view: (graph revision, surface and vertex code or the compile error).
    wgsl: Option<(u64, WgslView)>,
    /// The Function users list: (graph revision it was listed at, users or the reason).
    users: Option<(u64, Result<Vec<super::function::FunctionUser>, String>)>,
    /// Node rectangles snarl drew last frame (graph space).
    node_rects: Vec<egui::Rect>,
    /// The frame (`cumulative_frame_nr`) a wire click was last taken in: a frame egui runs
    /// twice takes it once.
    wire_click_frame: Option<u64>,
    /// The keyboard route: the Nodes list, its palette and connect form (ADR-7383).
    pub keys: crate::canvas_keys::GraphKeys,
    /// The node the Nodes list's focus belongs to (rows move when nodes are added or removed).
    keys_node: Option<String>,
    /// The w12 parity state: parameters form, tier preview, pinned node, stats, usages,
    /// hierarchy (ADR-7851).
    pub parity: super::params_panel::ParityState,
    /// The preview render on the preview thread: the key it was asked for and the image or why
    /// it has none (ADR-8584).
    pub(crate) preview_job:
        crate::preview_worker::AsyncPreview<(PreviewKey, Result<egui::ColorImage, String>)>,
    /// The node swatches rendering on the preview thread: (node, revision, key planned, swatch).
    node_job: crate::preview_worker::AsyncPreview<Vec<NodeSwatchResult>>,
}

/// One node swatch the preview thread rendered: (node, revision planned at, the swatch or why
/// it has none).
type NodeSwatchResult = (String, u64, Result<super::node_preview::NodeSwatch, String>);

/// A drag on the canvas that is not snarl's: a comment frame grip or a reroute point. The
/// drag keeps what it started from; each frame sets the absolute result (start + pointer
/// travel) as one coalesced edit, sealed when the pointer is released (one undo step).
#[derive(Clone, Debug)]
enum CanvasDrag {
    /// A comment frame's header (moves it and the nodes inside) or an edge or corner.
    Comment {
        /// The comment's index.
        index: usize,
        /// The grip held.
        grip: FrameGrip,
        /// The frame at the start.
        min: [f32; 2],
        /// The frame at the start.
        max: [f32; 2],
        /// The nodes inside at the start (header drags only), with their positions.
        nodes: Vec<(String, [f32; 2])>,
        /// Where the press was (graph space).
        origin: egui::Pos2,
    },
    /// One reroute point.
    Reroute {
        /// The node whose input the wire feeds.
        to: String,
        /// That input.
        slot: String,
        /// The point held.
        point: usize,
        /// The points at the start.
        points: Vec<[f32; 2]>,
        /// Where the press was (graph space).
        origin: egui::Pos2,
    },
}

/// Pin centers (graph space) and wire colors recorded while snarl draws the pins, keyed by pin.
#[derive(Default)]
struct PinMap {
    inputs: HashMap<InPinId, (egui::Pos2, egui::Color32)>,
    outputs: HashMap<OutPinId, (egui::Pos2, egui::Color32)>,
}

/// Which pin a [`RecordedPin`] is.
#[derive(Clone, Copy)]
enum PinKey {
    In(InPinId),
    Out(OutPinId),
}

/// A pin that draws as its [`PinInfo`] and records where it was drawn, so the canvas can draw
/// a rerouted wire between two pins snarl has no wire for (egui-snarl 0.12 has no wire hook;
/// ADR-9970).
struct RecordedPin {
    info: PinInfo,
    key: PinKey,
    map: Arc<Mutex<PinMap>>,
}

impl SnarlPin for RecordedPin {
    fn draw(
        self,
        snarl_style: &SnarlStyle,
        style: &egui::Style,
        rect: egui::Rect,
        painter: &egui::Painter,
    ) -> PinWireInfo {
        let wire = self.info.draw(snarl_style, style, rect, painter);
        let mut map = self.map.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.key {
            PinKey::In(id) => map.inputs.insert(id, (rect.center(), wire.color)),
            PinKey::Out(id) => map.outputs.insert(id, (rect.center(), wire.color)),
        };
        wire
    }
}

/// The inputs whose wire is rerouted: `(node, slot)` pairs that have reroute points and are
/// wired to a node that exists. Snarl gets no wire for them; the canvas draws them through
/// their points (ADR-9970).
#[must_use]
pub fn rerouted_inputs(doc: &MatGraphDoc) -> BTreeSet<(String, String)> {
    doc.notes
        .reroutes
        .iter()
        .filter(|r| !r.points.is_empty())
        .filter(|r| {
            doc.graph
                .nodes
                .get(&r.to)
                .and_then(|n| wired(n, &r.slot))
                .is_some_and(|from| doc.graph.nodes.contains_key(from))
        })
        .map(|r| (r.to.clone(), r.slot.clone()))
        .collect()
}

/// What a preview image was made for: (graph revision, size, shape and view hash).
type PreviewKey = (u64, [u32; 2], u64);

/// The canvas's open node search (Space or Tab).
#[derive(Clone, Debug, Default)]
pub struct SearchState {
    /// The letters typed.
    pub query: String,
    /// Where the node goes (graph space).
    pub pos: [f32; 2],
    /// The wire the new node takes, if the search came from a dropped wire.
    pub from: Option<WireFrom>,
    /// Where the popup shows (screen space).
    screen: [f32; 2],
}

impl PanelState {
    /// Rebuilds the canvas from the document on the next draw.
    pub fn invalidate_canvas(&mut self) {
        self.snarl_revision = None;
    }
}

/// A non-document action requested by the panel (applied after drawing).
#[derive(Clone, Debug)]
enum Action {
    Undo,
    Redo,
    New,
    Open(String),
    Save(Option<String>),
    /// Collapse these nodes into function `name` (inline, or written to the path).
    Collapse(BTreeSet<String>, String, Option<String>),
    /// Expand a `Call` node inline.
    Expand(String),
    /// Fix stale `Call` nodes (all, or one).
    Fix(Option<String>),
    /// Open the function file a `Call` node calls in its own tab.
    OpenFunction(String),
    /// Make a tab active.
    SelectTab(usize),
    /// Close the active tab.
    CloseTab,
    /// Add a node of `kind` at `pos`, wired to the pin a wire came from (search, ADR-9700).
    SearchAdd {
        /// Node kind.
        kind: String,
        /// Graph position.
        pos: [f32; 2],
        /// The wire it takes.
        from: Option<WireFrom>,
    },
    /// Set a typed row of a node.
    SetField {
        /// Node name.
        node: String,
        /// Row name.
        field: &'static str,
        /// New value.
        value: FieldValue,
    },
    /// Copy the picked nodes.
    Copy,
    /// Cut the picked nodes.
    Cut,
    /// Paste the clipboard at a graph position.
    Paste([f32; 2]),
    /// Paste clipboard text (from the system clipboard) at a graph position.
    PasteText(String, [f32; 2]),
    /// Align or distribute the picked nodes.
    Arrange(Arrange),
    /// Frame the picked nodes (or a default rectangle) with a comment.
    AddComment,
    /// Assign the open graph or instance to the selected meshes.
    ApplyToSelection,
    /// Find usages of what a node names across the project (w12).
    FindUsages(String),
    /// Pin a node's value in the preview, or unpin (w12).
    Pin(Option<String>),
    /// The Hierarchy view's request (w12).
    Hier(super::hierarchy::HierarchyAction),
    /// List the graphs that use the open function file (w11).
    ListUsers,
    /// Open a graph from the Function users list in a tab of its own, keeping the function tab
    /// (w11).
    OpenUser(std::path::PathBuf),
}

/// The nodes an action on "the selection" takes: the selected node and the multi-selection.
fn picked(editor: &MatGraphEditor) -> BTreeSet<String> {
    let mut picked = editor.ui.multi.clone();
    if let Some(sel) = &editor.ui.selected {
        picked.insert(sel.clone());
    }
    picked.retain(|n| editor.doc.graph.nodes.contains_key(n));
    picked
}

/// The input slot names the canvas shows for `node`: the kind's slots; for a `Call` of an
/// inline function, the function's inputs (then any other argument names).
#[must_use]
pub fn slot_names(doc: &MatGraphDoc, node: &Node) -> Vec<String> {
    slot_names_with(doc, &BTreeMap::new(), node)
}

/// [`slot_names`] with the signatures of the graph's functions (file functions too, as the
/// diagnostics read them): a `Call` shows its function's inputs in order, then the arguments
/// the signature no longer has (stale pins).
#[must_use]
pub fn slot_names_with(doc: &MatGraphDoc, sigs: &BTreeMap<String, Signature>, node: &Node) -> Vec<String> {
    let mut names: Vec<String> = node.input_slots().into_iter().map(|(n, _)| n).collect();
    if let Node::Call { function, .. } = node {
        let inputs: Option<Vec<String>> = match (sigs.get(function), doc.graph.functions.get(function)) {
            (Some(sig), _) => Some(sig.inputs.iter().map(|i| i.name.clone()).collect()),
            (None, Some(FunctionSource::Inline(f))) => {
                Some(f.inputs.iter().map(|i| i.name.clone()).collect())
            }
            _ => None,
        };
        if let Some(mut from_fn) = inputs {
            for extra in names {
                if !from_fn.contains(&extra) {
                    from_fn.push(extra);
                }
            }
            names = from_fn;
        }
    }
    names
}

/// The node a slot is wired to (`None` when unwired or the node is missing).
fn wired<'a>(node: &'a Node, slot: &str) -> Option<&'a str> {
    node.input_slots()
        .into_iter()
        .find(|(n, _)| n == slot)
        .and_then(|(_, w)| w)
}

/// Builds the canvas from the document.
#[must_use]
pub fn build_snarl(doc: &MatGraphDoc, surface_pos: Option<[f32; 2]>) -> Snarl<CanvasNode> {
    build_snarl_with(doc, &BTreeMap::new(), surface_pos)
}

/// [`build_snarl`] with the graph's function signatures (`Call` pins follow them).
#[must_use]
pub fn build_snarl_with(
    doc: &MatGraphDoc,
    sigs: &BTreeMap<String, Signature>,
    surface_pos: Option<[f32; 2]>,
) -> Snarl<CanvasNode> {
    let mut snarl = Snarl::new();
    let positions = doc.positions();
    let rerouted = rerouted_inputs(doc);
    let mut ids = HashMap::new();
    let mut max_x = f32::MIN;
    for name in doc.graph.nodes.keys() {
        let pos = positions.get(name).copied().unwrap_or([0.0, 0.0]);
        max_x = max_x.max(pos[0]);
        ids.insert(
            name.clone(),
            snarl.insert_node(egui::pos2(pos[0], pos[1]), CanvasNode::Node(name.clone())),
        );
    }
    let surface = surface_pos.unwrap_or([if max_x == f32::MIN { 0.0 } else { max_x + 260.0 }, 0.0]);
    let sink = snarl.insert_node(egui::pos2(surface[0], surface[1]), CanvasNode::Surface);
    for (name, node) in &doc.graph.nodes {
        let Some(&to) = ids.get(name) else { continue };
        for (index, slot) in slot_names_with(doc, sigs, node).iter().enumerate() {
            if rerouted.contains(&(name.clone(), slot.clone())) {
                // The canvas draws this wire through its reroute points itself.
                continue;
            }
            if let Some(&from) = wired(node, slot).and_then(|w| ids.get(w)) {
                snarl.connect(
                    OutPinId {
                        node: from,
                        output: 0,
                    },
                    InPinId {
                        node: to,
                        input: index,
                    },
                );
            }
        }
    }
    for (index, slot) in doc.sink_slots().iter().enumerate() {
        if let Some(&from) = doc.sink_get(slot).and_then(|w| ids.get(w)) {
            snarl.connect(
                OutPinId {
                    node: from,
                    output: 0,
                },
                InPinId {
                    node: sink,
                    input: index,
                },
            );
        }
    }
    snarl
}

/// The WGSL view's text (w11): the surface code `editor.matgraph_compile { include_wgsl: true }`
/// answers as `wgsl` (default switches, full validation; a function in its harness).
///
/// # Errors
///
/// A function file cannot be read, or the document does not compile (the message lists why).
pub fn wgsl_view_text(editor: &mut MatGraphEditor) -> Result<String, String> {
    wgsl_view(editor).map(|(wgsl, _)| wgsl)
}

/// The WGSL view's content: the surface code and the vertex code (when the graph offsets
/// vertices), or why the document does not compile.
type WgslView = Result<(String, Option<String>), String>;

/// The surface code and the vertex code (when the graph offsets vertices) of the open document.
fn wgsl_view(editor: &MatGraphEditor) -> WgslView {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_wgsl_view").entered();
    let graph = editor.compile_source()?;
    let compiled = regia_matgraph::compile_with(&graph, &BTreeMap::new()).map_err(|e| e.to_string())?;
    Ok((
        compiled.wgsl.to_string(),
        compiled.vertex_wgsl.as_deref().map(str::to_string),
    ))
}

/// The Function users list (w11): the project's graphs that use the open function tab's file,
/// as `editor.matgraph_function_users {}` lists them (the tab's unsaved signature checks the
/// calls).
///
/// # Errors
///
/// The active tab is not a saved function.
pub fn function_users_view(world: &mut World) -> Result<Vec<super::function::FunctionUser>, String> {
    let root = super::project_root(world);
    let editor = world
        .get_resource::<MatGraphEditor>()
        .ok_or_else(|| "no material graph editor".to_string())?;
    let (Some(function), Some(path)) = (editor.doc.to_function(), editor.path.clone()) else {
        return Err("save the function first: its users name its file".into());
    };
    let root = root.or_else(|| editor.root.clone());
    let scan = root
        .clone()
        .or_else(|| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_default();
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_function_users_view").entered();
    Ok(super::function::function_users(
        &scan,
        root.as_deref(),
        &path,
        &Signature::of(&function),
    ))
}

/// The side column's WGSL view: the compiled surface code, read-only, with a Copy button;
/// compiled again only when the graph changes while the section is open.
fn wgsl_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor) {
    egui::CollapsingHeader::new("WGSL")
        .default_open(false)
        .show(ui, |ui| {
            let revision = editor.graph_revision();
            if editor.ui.wgsl.as_ref().is_none_or(|(r, _)| *r != revision) {
                editor.ui.wgsl = Some((revision, wgsl_view(editor)));
            }
            let accents = crate::theme::ui_tokens(ui.ctx()).accents;
            match editor.ui.wgsl.as_ref().map(|(_, w)| w) {
                Some(Ok((wgsl, vertex))) => {
                    let mut text = wgsl.clone();
                    if let Some(vertex) = vertex {
                        text.push_str("\n// vertex stage (world position offset)\n");
                        text.push_str(vertex);
                    }
                    if ui
                        .button("Copy")
                        .on_hover_text(
                            "Copy the compiled code\nAgent: editor.matgraph_compile { include_wgsl }",
                        )
                        .clicked()
                    {
                        ui.ctx().copy_text(text.clone());
                        editor.toast = "copied the WGSL".into();
                    }
                    egui::ScrollArea::both().max_height(320.0).show(ui, |ui| {
                        let field = ui.add(
                            egui::TextEdit::multiline(&mut text.as_str())
                                .code_editor()
                                .desired_width(f32::INFINITY),
                        );
                        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Compiled WGSL");
                    });
                }
                Some(Err(err)) => {
                    ui.colored_label(accents.error_text, err);
                }
                None => {}
            }
        });
}

/// A function tab's Function users: press List to scan the project for graphs that use this
/// file; click a graph to open it.
fn function_users_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, actions: &mut Vec<Action>) {
    egui::CollapsingHeader::new("Function users")
        .default_open(false)
        .show(ui, |ui| {
            if ui
                .button("List")
                .on_hover_text(
                    "The project's graphs that use this function file\nAgent: editor.matgraph_function_users",
                )
                .clicked()
            {
                actions.push(Action::ListUsers);
            }
            let revision = editor.graph_revision();
            let accents = crate::theme::ui_tokens(ui.ctx()).accents;
            match &editor.ui.users {
                Some((listed, Ok(users))) => {
                    if *listed != revision {
                        ui.weak("The function changed since this list.");
                    }
                    if users.is_empty() {
                        ui.weak("No graph uses this file.");
                    }
                    for user in users {
                        let label = format!(
                            "{} ({} call(s){})",
                            user.path.display(),
                            user.calls.len(),
                            if user.stale.is_empty() { "" } else { ", stale" }
                        );
                        if ui
                            .link(label)
                            .on_hover_text(format!("Open it; it names the function `{}`", user.function))
                            .clicked()
                        {
                            actions.push(Action::OpenUser(user.path.clone()));
                        }
                    }
                }
                Some((_, Err(err))) => {
                    ui.colored_label(accents.error_text, err);
                }
                None => {}
            }
        });
}

/// Short text under a node's title: its constant, parameter, texture or swizzle.
fn node_summary(node: &Node) -> Option<String> {
    Some(match node {
        Node::Param(p) => format!("param {p}"),
        Node::Input(i) => format!("input {i}"),
        Node::Float(v) => format!("{v}"),
        Node::Vec2(v) => format!("{v:?}"),
        Node::Vec3(v) => format!("{v:?}"),
        Node::Vec4(v) => format!("{v:?}"),
        Node::Sample { texture, .. } | Node::Triplanar { texture, .. } | Node::Parallax { texture, .. } => {
            format!("texture {texture}")
        }
        Node::Swizzle { channels, .. } => format!(".{channels}"),
        Node::Switch { switch, .. } => format!("switch {switch}"),
        Node::Call { function, .. } => format!("fn {function}"),
        Node::BlendNormals { mode, .. } => format!("{mode:?}"),
        Node::Noise { octaves, .. } => format!("{octaves} octave(s)"),
        _ => return None,
    })
}

struct Viewer<'a> {
    doc: &'a MatGraphDoc,
    diag: &'a Diagnostics,
    pending: &'a mut Vec<PendingEdit>,
    selected: &'a mut Option<String>,
    multi: &'a mut BTreeSet<String>,
    actions: &'a mut Vec<Action>,
    collapse_form: &'a mut Option<(String, String)>,
    accents: crate::theme::AccentTokens,
    wire_query: &'a mut String,
    to_global: &'a mut Option<egui::emath::TSTransform>,
    swatches: Option<&'a NodePreviewCache>,
    /// Inputs whose wire the canvas draws through reroute points.
    rerouted: &'a BTreeSet<(String, String)>,
    /// Where snarl drew each pin this frame.
    pins: Arc<Mutex<PinMap>>,
    /// The canvas drag in progress.
    drag: &'a mut Option<CanvasDrag>,
    /// The comment text being edited on the canvas.
    comment_text: &'a mut Option<(usize, String)>,
    /// The snarl layer's painter and the shape slot (behind the nodes, above the comment
    /// frames) the rerouted wires are drawn into once the pins are placed.
    wire_slot: Option<(egui::Painter, egui::layers::ShapeIdx)>,
    /// Node rectangles drawn this frame (graph space), for the wire hit test.
    node_rects: Vec<egui::Rect>,
    /// The nodes on screen this frame (their swatches stay cached, w11).
    visible: Vec<String>,
    /// The rerouted wires drawn last frame: a right-click on one disconnects it, so the canvas
    /// menu does not open there.
    rerouted_last: Vec<DrawnWire>,
}

impl Viewer<'_> {
    fn read_only(&self) -> bool {
        matches!(self.doc.mode, DocMode::Instance(_))
    }

    /// The parameter a `Param` node reads, when the open instance overrides it.
    fn overridden(&self, node: &Node) -> Option<(String, ParamValue)> {
        let (Node::Param(p), DocMode::Instance(instance)) = (node, &self.doc.mode) else {
            return None;
        };
        instance.params.get(p).map(|v| (p.clone(), *v))
    }
}

impl Viewer<'_> {
    fn node(&self, value: &CanvasNode) -> Option<(&str, &Node)> {
        match value {
            CanvasNode::Node(name) => self
                .doc
                .graph
                .nodes
                .get_key_value(name)
                .map(|(k, v)| (k.as_str(), v)),
            CanvasNode::Surface => None,
        }
    }

    fn slot_name(&self, value: &CanvasNode, index: usize) -> Option<String> {
        match value {
            CanvasNode::Node(_) => {
                let (_, node) = self.node(value)?;
                slot_names_with(self.doc, &self.diag.signatures, node)
                    .get(index)
                    .cloned()
            }
            CanvasNode::Surface => self.doc.sink_slots().get(index).cloned(),
        }
    }

    /// Whether input `index` of `target` is drawn by the canvas through reroute points.
    fn is_rerouted(&self, target: &CanvasNode, index: usize) -> bool {
        match (target, self.slot_name(target, index)) {
            (CanvasNode::Node(name), Some(slot)) => self.rerouted.contains(&(name.clone(), slot)),
            _ => false,
        }
    }

    /// The node search inside a canvas menu: a filter field and the matching kinds; a pick
    /// (or Enter) adds the first match at `pos`, wired to `from`.
    fn search_menu(&mut self, ui: &mut egui::Ui, pos: egui::Pos2, from: Option<WireFrom>) {
        ui.label(match &from {
            Some(_) => "Add a node on this wire",
            None => "Add node",
        });
        let function = matches!(self.doc.mode, DocMode::Function(_));
        let list = crate::graph_kit::search::SearchList {
            hint: "search…",
            spoken: "Search nodes",
            max_hits: 48,
            max_height: 320.0,
            empty: Some("no kind matches"),
            width: Some(180.0),
        };
        let kinds = |query: &str| super::search::search_kinds(query, from.as_ref(), function);
        if let Some(kind) = list.show(ui, &mut *self.wire_query, kinds, |k| (*k).to_string()) {
            self.actions.push(Action::SearchAdd {
                kind: kind.to_string(),
                pos: [pos.x, pos.y],
                from: from.clone(),
            });
            ui.close();
        }
    }

    fn wire_edit(&mut self, from: Option<String>, target: &CanvasNode, index: usize) {
        let Some(slot) = self.slot_name(target, index) else {
            return;
        };
        if self.read_only() {
            // The canvas rebuilds from the document: the wire the user drew goes away.
            self.pending.push(PendingEdit::new("wire (read-only)", None, |_| {
                Err(format!(
                    "{}an instance cannot rewire its parent's graph",
                    super::model::READ_ONLY
                ))
            }));
            return;
        }
        match (target, from) {
            (CanvasNode::Node(to), Some(from)) => {
                let to = to.clone();
                self.pending.push(PendingEdit::new("connect", None, move |d| {
                    d.connect(&from, &to, &slot)
                }));
            }
            (CanvasNode::Node(to), None) => {
                let to = to.clone();
                self.pending.push(PendingEdit::new("disconnect", None, move |d| {
                    d.disconnect(&to, &slot)
                }));
            }
            (CanvasNode::Surface, from) => {
                self.pending.push(PendingEdit::new("set output", None, move |d| {
                    d.set_output(&slot, from.as_deref())
                }));
            }
        }
    }
}

impl Viewer<'_> {
    /// The comment frame grips and reroute point grips, registered on the snarl layer after
    /// its canvas response and before the nodes: they win over panning, nodes win over them.
    fn canvas_grips(&mut self, painter: &egui::Painter) {
        let ctx = painter.ctx().clone();
        let layer = painter.layer_id();
        let mut ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("matgraph-canvas-grips"),
            egui::UiBuilder::new()
                .layer_id(layer)
                .max_rect(painter.clip_rect()),
        );
        ui.set_clip_rect(painter.clip_rect());
        // The canvas transform snarl reported this frame (`current_transform` runs first).
        let from_global = self.to_global.unwrap_or_default().inverse();
        let (press, latest) = ctx.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos()));
        let travel = match (self.drag.as_ref(), latest) {
            (Some(CanvasDrag::Comment { origin, .. } | CanvasDrag::Reroute { origin, .. }), Some(latest)) => {
                (from_global * latest) - *origin
            }
            _ => egui::Vec2::ZERO,
        };
        let origin = press.map_or(egui::Pos2::ZERO, |p| from_global * p);
        let read_only = self.read_only();
        let comments = self.doc.notes.comments.clone();
        for (index, comment) in comments.iter().enumerate() {
            if read_only {
                break;
            }
            for (k, (grip, rect)) in canvas::frame_grips(comment.min, comment.max)
                .into_iter()
                .enumerate()
            {
                let sense = if grip == FrameGrip::Move {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::drag()
                };
                let r = ui.interact(rect, egui::Id::new(("matgraph-comment-grip", index, k)), sense);
                if r.hovered() || r.dragged() {
                    ctx.set_cursor_icon(grip.cursor());
                }
                if grip == FrameGrip::Move && r.double_clicked() {
                    *self.comment_text = Some((index, comment.text.clone()));
                }
                if r.drag_started() {
                    let positions = self.doc.positions();
                    let nodes = if grip == FrameGrip::Move {
                        self.doc
                            .nodes_in_comment(index)
                            .into_iter()
                            .filter_map(|n| positions.get(&n).map(|p| (n, *p)))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    *self.drag = Some(CanvasDrag::Comment {
                        index,
                        grip,
                        min: comment.min,
                        max: comment.max,
                        nodes,
                        origin,
                    });
                } else if r.dragged()
                    && let Some(CanvasDrag::Comment {
                        index: held,
                        grip: held_grip,
                        min,
                        max,
                        nodes,
                        ..
                    }) = self.drag.as_ref()
                    && *held == index
                    && *held_grip == grip
                {
                    let (lo, hi) = canvas::drag_frame(*min, *max, grip, travel);
                    let moved = crate::graph_kit::frames::carry(nodes, travel);
                    let label = if grip == FrameGrip::Move {
                        "move comment"
                    } else {
                        "resize comment"
                    };
                    self.pending.push(PendingEdit::new(
                        label,
                        Some(format!("canvas-comment:{index}")),
                        move |d| {
                            d.set_comment(index, None, Some((lo, hi)))?;
                            for (name, p) in moved {
                                d.move_node(&name, p)?;
                            }
                            Ok(())
                        },
                    ));
                }
                if r.drag_stopped() {
                    *self.drag = None;
                }
            }
        }
        let point_color = egui::Color32::from_rgb(120, 170, 240);
        for reroute in &self.doc.notes.reroutes {
            if !self
                .rerouted
                .contains(&(reroute.to.clone(), reroute.slot.clone()))
            {
                continue;
            }
            for (i, p) in reroute.points.iter().enumerate() {
                let center = egui::pos2(p[0], p[1]);
                painter.circle_filled(center, 5.0, point_color);
                if read_only {
                    continue;
                }
                let rect =
                    egui::Rect::from_center_size(center, egui::Vec2::splat(2.0 * canvas::REROUTE_GRIP));
                let id = egui::Id::new(("matgraph-reroute-grip", &reroute.to, &reroute.slot, i));
                let r = ui
                    .interact(rect, id, egui::Sense::click_and_drag())
                    .on_hover_text(
                        "Drag to move; double-click adds a point after it; right-click removes it",
                    );
                if r.hovered() || r.dragged() {
                    ctx.set_cursor_icon(egui::CursorIcon::Grab);
                }
                let (to, slot) = (reroute.to.clone(), reroute.slot.clone());
                if r.drag_started() {
                    *self.drag = Some(CanvasDrag::Reroute {
                        to,
                        slot,
                        point: i,
                        points: reroute.points.clone(),
                        origin,
                    });
                } else if r.dragged()
                    && let Some(CanvasDrag::Reroute {
                        to: held_to,
                        slot: held_slot,
                        point,
                        points,
                        ..
                    }) = self.drag.as_ref()
                    && *held_to == reroute.to
                    && *held_slot == reroute.slot
                    && *point == i
                {
                    let mut points = points.clone();
                    if let Some(q) = points.get_mut(i) {
                        *q = [q[0] + travel.x, q[1] + travel.y];
                    }
                    let key = format!("canvas-reroute:{to}:{slot}");
                    self.pending
                        .push(PendingEdit::new("move reroute point", Some(key), move |d| {
                            d.set_reroute(&to, &slot, points)
                        }));
                } else if r.double_clicked() {
                    let mut points = reroute.points.clone();
                    points.insert(i + 1, [p[0] + 40.0, p[1]]);
                    self.pending
                        .push(PendingEdit::new("add reroute point", None, move |d| {
                            d.set_reroute(&to, &slot, points)
                        }));
                } else if r.secondary_clicked() {
                    let mut points = reroute.points.clone();
                    points.remove(i);
                    self.pending
                        .push(PendingEdit::new("remove reroute point", None, move |d| {
                            d.set_reroute(&to, &slot, points)
                        }));
                }
                if r.drag_stopped() {
                    *self.drag = None;
                }
            }
        }
    }
}

impl SnarlViewer<CanvasNode> for Viewer<'_> {
    fn title(&mut self, node: &CanvasNode) -> String {
        match node {
            CanvasNode::Node(name) => name.clone(),
            CanvasNode::Surface if matches!(self.doc.mode, DocMode::Function(_)) => "Outputs".into(),
            CanvasNode::Surface => "Surface".into(),
        }
    }

    fn show_header(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<CanvasNode>,
    ) {
        let Some(value) = snarl.get_node(node).cloned() else {
            return;
        };
        match &value {
            CanvasNode::Node(name) => {
                let kind = self.node(&value).map_or("?", |(_, n)| n.kind());
                let stale = self.diag.stale.iter().any(|s| &s.node == name);
                let mark = if stale { "  ⚠ stale" } else { "" };
                let text = egui::RichText::new(format!("{name}  ·  {kind}{mark}"));
                let has_errors = self.diag.for_target(&Target::Node(name.clone())).next().is_some();
                let text = if has_errors {
                    text.color(self.accents.error_text)
                } else if stale {
                    text.color(self.accents.log_warn)
                } else {
                    text
                };
                let picked = self.selected.as_deref() == Some(name.as_str()) || self.multi.contains(name);
                let response = ui.selectable_label(picked, text);
                if response.clicked() {
                    if ui.input(|i| i.modifiers.shift) {
                        // Shift+click builds a selection for a collapse.
                        if !self.multi.remove(name) {
                            self.multi.insert(name.clone());
                        }
                        if let Some(sel) = self.selected.take()
                            && sel != *name
                        {
                            self.multi.insert(sel);
                        }
                    } else {
                        self.multi.clear();
                        *self.selected = Some(name.clone());
                    }
                }
            }
            CanvasNode::Surface => {
                ui.strong(if matches!(self.doc.mode, DocMode::Function(_)) {
                    "Outputs"
                } else {
                    "Surface"
                });
            }
        }
    }

    fn inputs(&mut self, node: &CanvasNode) -> usize {
        match node {
            CanvasNode::Node(_) => self.node(node).map_or(0, |(_, n)| {
                slot_names_with(self.doc, &self.diag.signatures, n).len()
            }),
            CanvasNode::Surface => self.doc.sink_slots().len(),
        }
    }

    fn outputs(&mut self, node: &CanvasNode) -> usize {
        usize::from(matches!(node, CanvasNode::Node(_)))
    }

    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<CanvasNode>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        let value = snarl.get_node(pin.id.node).cloned();
        let name = value
            .as_ref()
            .and_then(|v| self.slot_name(v, pin.id.input))
            .unwrap_or_default();
        let optional = (matches!(value, Some(CanvasNode::Surface))
            && !matches!(self.doc.mode, DocMode::Function(_)))
            || (name == "lod"
                && value
                    .as_ref()
                    .and_then(|v| self.node(v))
                    .is_some_and(|(_, n)| n.kind() == "Sample"));
        // A Call's argument its function no longer has is a stale pin.
        let stale = value
            .as_ref()
            .and_then(|v| self.node(v))
            .is_some_and(|(_, n)| match n {
                Node::Call { function, .. } => self
                    .diag
                    .signatures
                    .get(function)
                    .is_some_and(|sig| !sig.inputs.iter().any(|i| i.name == name)),
                _ => false,
            });
        if stale {
            ui.colored_label(self.accents.log_warn, format!("{name} (stale)"));
        } else {
            ui.label(&name);
        }
        let rerouted =
            matches!(&value, Some(CanvasNode::Node(n)) if self.rerouted.contains(&(n.clone(), name.clone())));
        let fill = if !pin.remotes.is_empty() || rerouted {
            self.accents.ok
        } else if optional {
            egui::Color32::from_gray(120)
        } else {
            self.accents.alert
        };
        RecordedPin {
            info: PinInfo::circle().with_fill(fill),
            key: PinKey::In(pin.id),
            map: Arc::clone(&self.pins),
        }
    }

    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut egui::Ui,
        _snarl: &mut Snarl<CanvasNode>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        ui.label("out");
        RecordedPin {
            info: PinInfo::circle().with_fill(egui::Color32::from_rgb(120, 170, 240)),
            key: PinKey::Out(pin.id),
            map: Arc::clone(&self.pins),
        }
    }

    fn has_body(&mut self, node: &CanvasNode) -> bool {
        matches!(node, CanvasNode::Node(_))
    }

    fn show_body(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<CanvasNode>,
    ) {
        let Some(value) = snarl.get_node(node).cloned() else {
            return;
        };
        let Some((name, n)) = self.node(&value) else {
            return;
        };
        let name = name.to_string();
        let n = n.clone();
        if let Some(summary) = node_summary(&n) {
            ui.weak(summary);
        }
        if let Some(cache) = self.swatches {
            use crate::graph_kit::preview::{NODE_SWATCH_SIDE, Slot, show};
            let slot = cache
                .swatch(&name)
                .map(Slot::Ready)
                .or_else(|| cache.reason(&name).map(Slot::Failed))
                .unwrap_or(Slot::Empty);
            show(ui, slot, NODE_SWATCH_SIDE);
        }
        // An instance edits its parameter overrides right on the Param node.
        if let (Node::Param(param), DocMode::Instance(_)) = (&n, &self.doc.mode) {
            let declared = self
                .doc
                .graph
                .params
                .iter()
                .find(|p| &p.name == param)
                .map(|p| p.value);
            match (self.overridden(&n), declared) {
                (Some((param, value)), _) => {
                    let mut edited = value;
                    let changed = param_value_ui(ui, &mut edited);
                    let clear = ui.small_button("Clear override").clicked();
                    if changed && edited.is_finite() {
                        self.pending.push(PendingEdit::new(
                            "instance parameter",
                            Some(format!("override:{param}")),
                            move |d| d.set_param_override(&param, Some(edited)),
                        ));
                    } else if clear {
                        self.pending
                            .push(PendingEdit::new("clear override", None, move |d| {
                                d.set_param_override(&param, None)
                            }));
                    }
                }
                (None, Some(default)) => {
                    if ui.small_button("Override").clicked() {
                        let param = param.clone();
                        self.pending
                            .push(PendingEdit::new("instance parameter", None, move |d| {
                                d.set_param_override(&param, Some(default))
                            }));
                    }
                }
                (None, None) => {}
            }
        }
        if let Some(stale) = self.diag.stale.iter().find(|s| s.node == name) {
            ui.colored_label(self.accents.log_warn, stale.message());
            if ui.small_button("Fix signature").clicked() {
                self.actions.push(Action::Fix(Some(name.clone())));
            }
        }
        for message in self.diag.for_target(&Target::Node(name.clone())) {
            ui.colored_label(self.accents.error_text, message);
        }
    }

    fn node_frame(
        &mut self,
        default: egui::Frame,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        snarl: &Snarl<CanvasNode>,
    ) -> egui::Frame {
        match snarl.get_node(node) {
            Some(CanvasNode::Node(name)) => {
                let overridden = self
                    .doc
                    .graph
                    .nodes
                    .get(name)
                    .is_some_and(|n| self.overridden(n).is_some());
                if self.selected.as_deref() == Some(name.as_str()) || self.multi.contains(name) {
                    default.stroke(egui::Stroke::new(2.0, self.accents.select))
                } else if overridden {
                    // An instance's overridden parameter stands out.
                    default.stroke(egui::Stroke::new(2.5, self.accents.ok))
                } else if self.diag.for_target(&Target::Node(name.clone())).next().is_some() {
                    default.stroke(egui::Stroke::new(2.0, self.accents.error_text))
                } else {
                    default
                }
            }
            _ => default.fill(egui::Color32::from_rgb(40, 52, 64)),
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<CanvasNode>) {
        let (Some(CanvasNode::Node(src)), Some(target)) = (
            snarl.get_node(from.id.node).cloned(),
            snarl.get_node(to.id.node).cloned(),
        ) else {
            return;
        };
        self.wire_edit(Some(src), &target, to.id.input);
        snarl.drop_inputs(to.id);
        snarl.connect(from.id, to.id);
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<CanvasNode>) {
        if let Some(target) = snarl.get_node(to.id.node).cloned() {
            self.wire_edit(None, &target, to.id.input);
        }
        snarl.disconnect(from.id, to.id);
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<CanvasNode>) {
        // A rerouted input has no snarl wire, but right-clicking its pin still unwires it.
        if let Some(target) = snarl.get_node(pin.id.node).cloned()
            && (!pin.remotes.is_empty() || self.is_rerouted(&target, pin.id.input))
        {
            self.wire_edit(None, &target, pin.id.input);
        }
        snarl.drop_inputs(pin.id);
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<CanvasNode>) {
        for remote in &pin.remotes {
            if let Some(target) = snarl.get_node(remote.node).cloned() {
                self.wire_edit(None, &target, remote.input);
            }
        }
        snarl.drop_outputs(pin.id);
    }

    fn final_node_rect(
        &mut self,
        node: egui_snarl::NodeId,
        rect: egui::Rect,
        ui: &mut egui::Ui,
        snarl: &mut Snarl<CanvasNode>,
    ) {
        self.node_rects.push(rect);
        if let Some(CanvasNode::Node(name)) = snarl.get_node(node)
            && ui.is_rect_visible(rect)
        {
            self.visible.push(name.clone());
        }
    }

    fn has_graph_menu(&mut self, pos: egui::Pos2, _snarl: &mut Snarl<CanvasNode>) -> bool {
        // `pos` is the pointer (screen space): over a rerouted wire a right-click disconnects
        // the wire instead (w11).
        let transform = self.to_global.unwrap_or_default();
        let on_rerouted = super::wire_hit::hit_wire(
            transform.inverse() * pos,
            &self.rerouted_last,
            super::wire_hit::WIRE_HIT / transform.scaling.max(1e-3),
        )
        .is_some();
        !self.read_only() && !on_rerouted
    }

    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, _snarl: &mut Snarl<CanvasNode>) {
        // `Input` only exists inside material functions (the search leaves it out elsewhere).
        self.search_menu(ui, pos, None);
        ui.separator();
        if ui.button("Paste").clicked() {
            self.actions.push(Action::Paste([pos.x, pos.y]));
            ui.close();
        }
        if ui.button("Add comment").clicked() {
            self.actions.push(Action::AddComment);
            ui.close();
        }
    }

    fn has_dropped_wire_menu(&mut self, _src_pins: AnyPins, _snarl: &mut Snarl<CanvasNode>) -> bool {
        !self.read_only()
    }

    fn show_dropped_wire_menu(
        &mut self,
        pos: egui::Pos2,
        ui: &mut egui::Ui,
        src_pins: AnyPins,
        snarl: &mut Snarl<CanvasNode>,
    ) {
        let from = match src_pins {
            AnyPins::Out(pins) => pins.first().and_then(|p| match snarl.get_node(p.node) {
                Some(CanvasNode::Node(name)) => Some(WireFrom::Output(name.clone())),
                _ => None,
            }),
            AnyPins::In(pins) => pins.first().and_then(|p| {
                let value = snarl.get_node(p.node).cloned()?;
                let slot = self.slot_name(&value, p.input)?;
                Some(match value {
                    CanvasNode::Node(node) => WireFrom::Input { node, slot },
                    CanvasNode::Surface => WireFrom::Surface(slot),
                })
            }),
        };
        self.search_menu(ui, pos, from);
    }

    fn current_transform(
        &mut self,
        to_global: &mut egui::emath::TSTransform,
        _snarl: &mut Snarl<CanvasNode>,
    ) {
        *self.to_global = Some(*to_global);
    }

    fn draw_background(
        &mut self,
        background: Option<&BackgroundPattern>,
        viewport: &egui::Rect,
        snarl_style: &SnarlStyle,
        style: &egui::Style,
        painter: &egui::Painter,
        _snarl: &Snarl<CanvasNode>,
    ) {
        if let Some(background) = background {
            background.draw(viewport, snarl_style, style, painter);
        }
        // Comment frames live in graph space, behind the nodes.
        for (index, comment) in self.doc.notes.comments.iter().enumerate() {
            let [r, g, b] = comment.color;
            let rect = egui::Rect::from_min_max(
                egui::pos2(comment.min[0], comment.min[1]),
                egui::pos2(comment.max[0], comment.max[1]),
            );
            painter.rect_filled(rect, 6.0, egui::Color32::from_rgba_unmultiplied(r, g, b, 70));
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.5, egui::Color32::from_rgb(r, g, b)),
                egui::StrokeKind::Inside,
            );
            if self.comment_text.as_ref().is_some_and(|(i, _)| *i == index) {
                continue;
            }
            painter.text(
                rect.min + egui::vec2(8.0, 6.0),
                egui::Align2::LEFT_TOP,
                &comment.text,
                egui::FontId::proportional(15.0),
                egui::Color32::from_gray(230),
            );
        }
        // Rerouted wires go here once snarl has placed the pins: above the frames, behind the
        // nodes.
        self.wire_slot = Some((painter.clone(), painter.add(egui::Shape::Noop)));
        self.canvas_grips(painter);
    }

    fn has_node_menu(&mut self, node: &CanvasNode) -> bool {
        matches!(node, CanvasNode::Node(_))
    }

    fn show_node_menu(
        &mut self,
        node: egui_snarl::NodeId,
        _inputs: &[InPin],
        _outputs: &[OutPin],
        ui: &mut egui::Ui,
        snarl: &mut Snarl<CanvasNode>,
    ) {
        let Some(CanvasNode::Node(name)) = snarl.get_node(node).cloned() else {
            return;
        };
        if let Some(Node::Call { function, .. }) = self.doc.graph.nodes.get(&name) {
            if matches!(
                self.doc.graph.functions.get(function),
                Some(FunctionSource::File(_))
            ) && ui.button("Open function").clicked()
            {
                self.actions.push(Action::OpenFunction(name.clone()));
                ui.close();
            }
            if !self.read_only() {
                if self.diag.stale.iter().any(|s| s.node == name) && ui.button("Fix signature").clicked() {
                    self.actions.push(Action::Fix(Some(name.clone())));
                    ui.close();
                }
                if ui.button("Expand inline").clicked() {
                    self.actions.push(Action::Expand(name.clone()));
                    ui.close();
                }
            }
        }
        // w12 (ADR-7851): usages across the project and the node pinned in the preview.
        if super::usages::query_for_node(&self.doc.graph, &name, None, None).is_some()
            && ui.button("Find usages").clicked()
        {
            self.actions.push(Action::FindUsages(name.clone()));
            ui.close();
        }
        if ui.button("Preview this node").clicked() {
            self.actions.push(Action::Pin(Some(name.clone())));
            ui.close();
        }
        if self.read_only() {
            ui.weak("Read-only: this instance's graph belongs to its parent.");
            return;
        }
        if matches!(self.doc.mode, DocMode::Graph) && ui.button("Collapse into function…").clicked() {
            if let Some(sel) = self.selected.clone() {
                self.multi.insert(sel);
            }
            self.multi.insert(name.clone());
            if self.collapse_form.is_none() {
                *self.collapse_form = Some((format!("{name}_fn"), String::new()));
            }
            ui.close();
        }
        let slots = self.doc.sink_slots();
        ui.menu_button("Drive output", |ui| {
            for slot in slots {
                if ui.button(slot.as_str()).clicked() {
                    let from = name.clone();
                    self.pending.push(PendingEdit::new("set output", None, move |d| {
                        d.set_output(&slot, Some(&from))
                    }));
                    ui.close();
                }
            }
        });
        if ui.button("Copy").clicked() {
            if self.selected.as_deref() != Some(name.as_str()) && !self.multi.contains(&name) {
                self.multi.clear();
                *self.selected = Some(name.clone());
            }
            self.actions.push(Action::Copy);
            ui.close();
        }
        if ui.button("Cut").clicked() {
            if self.selected.as_deref() != Some(name.as_str()) && !self.multi.contains(&name) {
                self.multi.clear();
                *self.selected = Some(name.clone());
            }
            self.actions.push(Action::Cut);
            ui.close();
        }
        if ui.button("Duplicate").clicked()
            && let Some(n) = self.doc.graph.nodes.get(&name).cloned()
        {
            let pos = self.doc.positions().get(&name).copied().unwrap_or([0.0, 0.0]);
            self.pending
                .push(PendingEdit::new("duplicate node", None, move |d| {
                    d.add_node(None, n, Some([pos[0] + 30.0, pos[1] + 30.0]))
                        .map(|_| ())
                }));
            ui.close();
        }
        if ui.button("Disconnect all").clicked() {
            let n = name.clone();
            self.pending
                .push(PendingEdit::new("disconnect all", None, move |d| {
                    d.disconnect_all(&n).map(|_| ())
                }));
            ui.close();
        }
        if ui.button("Delete").clicked() {
            if self.selected.as_deref() == Some(name.as_str()) {
                *self.selected = None;
            }
            self.pending.push(PendingEdit::new("delete node", None, move |d| {
                d.remove_node(&name).map(|_| ())
            }));
            ui.close();
        }
    }
}

/// Draws the Material Graph panel against the world's [`MatGraphEditor`].
#[tracing::instrument(target = "regia_editor", skip_all, name = "matgraph_editor_panel")]
pub fn matgraph_editor_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
    if !world.contains_resource::<MatGraphEditor>() {
        world.init_resource::<MatGraphEditor>();
    }
    let root = super::project_root(world);
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.next_undo(), s.next_redo()))
        .unwrap_or_default();
    let mut pending = Vec::new();
    let mut actions = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<MatGraphEditor>| {
        let editor = &mut *editor;
        if editor.root.is_none() {
            editor.root = root;
        }
        draw(ui, editor, device, (undo, redo), &mut pending, &mut actions);
    });
    apply_pending(world, pending);
    // A drag ends with the pointer: the next drag of the same node is its own undo step.
    if ui.ctx().input(|i| i.pointer.any_released())
        && let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>()
    {
        stack.seal();
    }
    // The selection follows a rename only when the document took it.
    if let Some(mut editor) = world.get_resource_mut::<MatGraphEditor>()
        && let Some((from, to)) = editor.ui.rename_follow.take()
        && editor.doc.graph.nodes.contains_key(&to)
        && !editor.doc.graph.nodes.contains_key(&from)
    {
        editor.ui.selected = Some(to);
    }
    for action in actions {
        perform(world, action);
    }
}

/// New and Open replace the document: with unsaved work the first press warns and the second
/// goes ahead. An edit in between takes the warning back.
fn may_replace_document(editor: &mut MatGraphEditor) -> bool {
    if !editor.has_unsaved_work() || editor.ui.discard_armed == Some(editor.revision) {
        return true;
    }
    editor.ui.discard_armed = Some(editor.revision);
    editor.toast = "unsaved changes: press again to discard them".into();
    false
}

/// Opens a path from the toolbar: a `.matfunc.ron` or a `.matinst.ron` in a tab of its own,
/// a `.matgraph.ron` in place of the active document.
fn open_any(world: &mut World, path: &str) -> Result<(), String> {
    let full = resolve_path(world, path);
    let root = super::project_root(world);
    if path.ends_with(".matfunc.ron") {
        let mut editor = world.resource_mut::<MatGraphEditor>();
        let context = super::context_of_open_graph(&editor);
        editor.open_tab();
        let result = editor.open_function(&full, context);
        if result.is_err() {
            editor.close_tab();
        }
        result
    } else if path.ends_with(".matinst.ron") {
        let text =
            std::fs::read_to_string(&full).map_err(|e| format!("cannot read {}: {e}", full.display()))?;
        let instance = regia_matgraph::MaterialInstance::from_ron(&text).map_err(|e| e.to_string())?;
        let parent = super::instance_parent_path(Some(&full), root.as_deref(), &instance.parent)?;
        let mut editor = world.resource_mut::<MatGraphEditor>();
        editor.open_tab();
        let result = editor.open_instance(&full, &parent);
        if result.is_err() {
            editor.close_tab();
        }
        result
    } else {
        world.resource_mut::<MatGraphEditor>().open(&full)
    }
}

/// Opens a project file from a list (Find usages, Hierarchy): a graph in a tab of its own (the
/// active document stays open), a function or an instance as the toolbar's Open does (w12).
pub(super) fn open_path(world: &mut World, path: &str) -> Result<(), String> {
    if path.ends_with(".matgraph.ron") {
        let full = resolve_path(world, path);
        let mut editor = world.resource_mut::<MatGraphEditor>();
        editor.open_tab();
        let result = editor.open(&full);
        if result.is_err() {
            editor.close_tab();
        }
        result
    } else {
        open_any(world, path)
    }
}

/// The usage query of node `node` of the open document and its answer over the project (w12).
fn find_usages_of(
    world: &mut World,
    node: &str,
) -> Option<(
    super::usages::UsageQuery,
    Result<Vec<super::usages::Usage>, String>,
)> {
    let editor = world.resource::<MatGraphEditor>();
    let graph = editor.doc.view_graph().into_owned();
    let dir = editor.graph_dir();
    let root = editor.root.clone();
    let query = super::usages::query_for_node(&graph, node, dir.as_deref(), root.as_deref())?;
    let found = match &root {
        Some(root) => Ok(super::usages::find_usages(root, Some(root), &query)),
        None => Err("no project is open: Find usages scans the project".to_string()),
    };
    Some((query, found))
}

/// Opens the function file `Call` node `node` calls in its own tab, checked against the graph.
fn open_function_of(world: &mut World, node: &str) -> Result<(), String> {
    let mut editor = world.resource_mut::<MatGraphEditor>();
    let Some(Node::Call { function, .. }) = editor.doc.graph.nodes.get(node).cloned() else {
        return Err(format!("`{node}` is not a Call node"));
    };
    let Some(FunctionSource::File(rel)) = editor.doc.graph.functions.get(&function).cloned() else {
        return Err(format!(
            "`{function}` is inline: edit it as RON, or collapse to a file"
        ));
    };
    let path = super::model::graph_file_path(editor.graph_dir().as_deref(), editor.root.as_deref(), &rel)?;
    let context = super::context_of_open_graph(&editor);
    editor.open_tab();
    let result = editor.open_function(&path, context);
    if result.is_err() {
        editor.close_tab();
    }
    result
}

fn perform(world: &mut World, action: Action) {
    let replaces = match &action {
        Action::New => true,
        Action::Open(p) => p.ends_with(".matgraph.ron") || !p.ends_with(".ron"),
        _ => false,
    };
    if replaces && !may_replace_document(&mut world.resource_mut::<MatGraphEditor>()) {
        return;
    }
    let result = match action {
        Action::Undo => {
            crate::command::undo_world(world);
            Ok(())
        }
        Action::Redo => {
            crate::command::redo_world(world);
            Ok(())
        }
        Action::New => {
            world.resource_mut::<MatGraphEditor>().new_graph(None);
            Ok(())
        }
        Action::Open(path) => open_any(world, &path),
        Action::Collapse(nodes, name, file) => {
            let result = super::collapse_selection(world, &nodes, &name, file.as_deref());
            let mut editor = world.resource_mut::<MatGraphEditor>();
            match result {
                Ok(report) => {
                    editor.ui.multi.clear();
                    editor.ui.collapse_form = None;
                    editor.ui.selected = report.calls.first().cloned();
                    editor.toast = format!(
                        "collapsed {} node(s) into `{}`{}",
                        nodes.len(),
                        report.function,
                        match report.wgsl_identical {
                            Some(false) => " (each output expands the function: the code grew)",
                            _ => "",
                        }
                    );
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Action::Expand(node) => super::expand_call(world, &node).map(|out| {
            world.resource_mut::<MatGraphEditor>().toast =
                format!("expanded `{node}` ({} node(s))", out.nodes.len());
        }),
        Action::Fix(node) => {
            let nodes = node.map(|n| vec![n]);
            super::fix_signatures(world, nodes.as_deref()).map(|fixes| {
                world.resource_mut::<MatGraphEditor>().toast = format!("fixed {} call(s)", fixes.len());
            })
        }
        Action::OpenFunction(node) => open_function_of(world, &node),
        Action::SelectTab(index) => world.resource_mut::<MatGraphEditor>().select_tab(index),
        Action::CloseTab => {
            world.resource_mut::<MatGraphEditor>().close_tab();
            Ok(())
        }
        Action::Save(path) => {
            let path = path.map(|p| resolve_path(world, &p));
            world
                .resource_mut::<MatGraphEditor>()
                .save(path.as_deref())
                .map(|_| ())
        }
        Action::SearchAdd { kind, pos, from } => super::search_add(world, &kind, pos, from).map(|name| {
            let mut editor = world.resource_mut::<MatGraphEditor>();
            editor.ui.multi.clear();
            editor.ui.selected = Some(name);
            editor.ui.search = None;
        }),
        Action::SetField { node, field, value } => super::set_node_field(world, &node, field, value),
        Action::Copy => {
            let names = picked(world.resource::<MatGraphEditor>());
            super::copy_selection(world, &names).map(|_| {
                let mut editor = world.resource_mut::<MatGraphEditor>();
                editor.ui.export_clipboard = true;
                editor.toast = format!("copied {} node(s)", names.len());
            })
        }
        Action::Cut => {
            let names = picked(world.resource::<MatGraphEditor>());
            super::cut_selection(world, &names).map(|_| {
                let mut editor = world.resource_mut::<MatGraphEditor>();
                editor.ui.export_clipboard = true;
                editor.ui.selected = None;
                editor.ui.multi.clear();
                editor.toast = format!("cut {} node(s)", names.len());
            })
        }
        Action::Paste(at) => paste_and_pick(world, None, at),
        Action::PasteText(text, at) => paste_and_pick(world, Some(&text), at),
        Action::Arrange(op) => {
            let names = picked(world.resource::<MatGraphEditor>());
            super::arrange_nodes(world, &names, op)
        }
        Action::AddComment => {
            let rect = {
                let editor = world.resource::<MatGraphEditor>();
                let names = picked(editor);
                let positions = editor.doc.positions();
                comment_rect(names.iter().filter_map(|n| positions.get(n).copied()))
            };
            super::edit(world, "add comment", None, move |d| {
                d.add_comment("Comment", rect.0, rect.1).map(|_| ())
            })
        }
        Action::FindUsages(node) => {
            let result = find_usages_of(world, &node);
            let mut editor = world.resource_mut::<MatGraphEditor>();
            if result.is_none() {
                editor.toast = format!("`{node}` names no parameter, texture, switch, function or constant");
            }
            editor.ui.parity.usages = result;
            Ok(())
        }
        Action::Pin(node) => {
            let mut editor = world.resource_mut::<MatGraphEditor>();
            editor.toast = match &node {
                Some(n) => format!("the preview shows `{n}`"),
                None => "the preview shows the material".into(),
            };
            editor.ui.parity.pinned = node;
            Ok(())
        }
        Action::Hier(action) => {
            super::hierarchy::perform(world, vec![action]);
            Ok(())
        }
        Action::ListUsers => {
            let users = function_users_view(world);
            let mut editor = world.resource_mut::<MatGraphEditor>();
            let revision = editor.graph_revision();
            editor.ui.users = Some((revision, users));
            Ok(())
        }
        Action::OpenUser(path) => {
            let mut editor = world.resource_mut::<MatGraphEditor>();
            editor.open_tab();
            let result = editor.open(&path);
            if result.is_err() {
                editor.close_tab();
            }
            result
        }
        Action::ApplyToSelection => super::apply_to_selection(world).map(|report| {
            world.resource_mut::<MatGraphEditor>().toast =
                format!("applied {} to {} mesh(es)", report.path, report.owners);
        }),
    };
    if let Err(err) = result {
        world.resource_mut::<MatGraphEditor>().toast = err;
    }
}

/// Pastes (the editor's clipboard, or `text`) at `at` and picks the pasted nodes.
fn paste_and_pick(world: &mut World, text: Option<&str>, at: [f32; 2]) -> Result<(), String> {
    let renamed = super::paste(world, text, at)?;
    let mut editor = world.resource_mut::<MatGraphEditor>();
    editor.ui.multi = renamed.values().cloned().collect();
    editor.ui.selected = None;
    editor.toast = format!("pasted {} node(s)", renamed.len());
    Ok(())
}

/// A comment frame around node positions (a node is about 200 × 110 points), or a default
/// rectangle at the origin when nothing is picked.
fn comment_rect(positions: impl Iterator<Item = [f32; 2]>) -> ([f32; 2], [f32; 2]) {
    let mut lo = [f32::MAX, f32::MAX];
    let mut hi = [f32::MIN, f32::MIN];
    let mut any = false;
    for p in positions {
        any = true;
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0] + 200.0), hi[1].max(p[1] + 110.0)];
    }
    if !any {
        return ([0.0, 0.0], [300.0, 200.0]);
    }
    ([lo[0] - 20.0, lo[1] - 40.0], [hi[0] + 20.0, hi[1] + 20.0])
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    device: Option<&RenderDevice>,
    (undo, redo): (Option<String>, Option<String>),
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let diag = editor.diagnostics().clone();
    let tabs = editor.tabs();
    if tabs.len() > 1 || !matches!(editor.doc.mode, DocMode::Graph) {
        ui.horizontal(|ui| {
            for (index, tab) in tabs.iter().enumerate() {
                let label = format!("{} {}{}", tab.kind, tab.label, if tab.dirty { " *" } else { "" });
                if ui.selectable_label(tab.active, label).clicked() && !tab.active {
                    actions.push(Action::SelectTab(index));
                }
            }
            if ui
                .small_button("Close tab")
                .on_hover_text("Close the active tab (unsaved edits are dropped)")
                .clicked()
            {
                actions.push(Action::CloseTab);
            }
        });
    }
    // The file row wraps: a row wider than the panel (a long path, 3230's two diff buttons)
    // would widen the panel's rect, push the side panel off screen and stretch the canvas.
    ui.horizontal_wrapped(|ui| {
        if ui.button("New").clicked() {
            actions.push(Action::New);
        }
        let field = ui.add(
            egui::TextEdit::singleline(&mut editor.ui.path_field)
                .hint_text("assets/….matgraph.ron / .matfunc.ron / .matinst.ron")
                .desired_width(220.0),
        );
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Graph path");
        if ui.button("Open").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Open(editor.ui.path_field.clone()));
        }
        let can_save = editor.path.is_some();
        if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
            actions.push(Action::Save(None));
        }
        if ui.button("Save as").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Save(Some(editor.ui.path_field.clone())));
        }
        crate::diff_view::graph_file_buttons(ui, editor.path.as_deref());
        ui.separator();
        if ui
            .add_enabled(undo.is_some(), egui::Button::new("Undo"))
            .on_hover_text(undo.unwrap_or_default())
            .clicked()
        {
            actions.push(Action::Undo);
        }
        if ui
            .add_enabled(redo.is_some(), egui::Button::new("Redo"))
            .on_hover_text(redo.unwrap_or_default())
            .clicked()
        {
            actions.push(Action::Redo);
        }
        ui.separator();
        let before = editor.ui.node_previews;
        ui.checkbox(&mut editor.ui.node_previews, "Node previews")
            .on_hover_text(
                "A swatch of each node's value on the canvas\nAgent: editor.matgraph_node_preview",
            );
        if before && !editor.ui.node_previews {
            editor.ui.node_cache.clear();
        }
        let can_apply = editor.path.is_some() && !matches!(editor.doc.mode, DocMode::Function(_));
        if ui
            .add_enabled(can_apply, egui::Button::new("Apply to selection"))
            .on_hover_text("Assign this saved graph or instance to the selected meshes (one undo step)")
            .clicked()
        {
            actions.push(Action::ApplyToSelection);
        }
        ui.separator();
        let name = editor.path.as_ref().map_or_else(
            || format!("(unsaved {})", super::doc_kind(&editor.doc)),
            |p| p.display().to_string(),
        );
        ui.weak(if editor.dirty() { format!("{name} *") } else { name });
        if diag.ok {
            ui.colored_label(accents.ok, "compiles");
        } else {
            ui.colored_label(accents.error_text, format!("{} error(s)", diag.errors.len()));
        }
        for warning in &diag.warnings {
            // The glyph alone carries no meaning: the spoken name is the warning text (FU-7).
            crate::a11y_audit::warning_glyph(ui, accents.log_warn, warning);
        }
        if !editor.toast.is_empty() {
            ui.weak(&editor.toast);
        }
    });
    ui.separator();

    egui::Panel::right("matgraph-side")
        .resizable(true)
        .default_size(300.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match editor.doc.mode {
                    DocMode::Function(_) => {
                        // Its calling graph with this definition (w11).
                        preview_ui(ui, editor, device, &diag);
                        ui.separator();
                        signature_ui(ui, editor, pending);
                        function_users_ui(ui, editor, actions);
                    }
                    DocMode::Instance(_) => {
                        preview_ui(ui, editor, device, &diag);
                        ui.separator();
                        instance_ui(ui, editor, pending);
                        let mut hier = Vec::new();
                        super::hierarchy::reparent_picker_ui(ui, editor, &mut hier);
                        actions.extend(hier.into_iter().map(Action::Hier));
                    }
                    DocMode::Graph => {
                        preview_ui(ui, editor, device, &diag);
                        collapse_ui(ui, editor, actions);
                    }
                }
                // w12 (ADR-7851): Find usages answers and the Hierarchy view.
                let mut open = None;
                super::usages::usages_ui(ui, editor, &mut open);
                actions.extend(open.map(|p| Action::Hier(super::hierarchy::HierarchyAction::Open(p.into()))));
                let mut hier = Vec::new();
                super::hierarchy::hierarchy_ui(ui, editor, &mut hier);
                actions.extend(hier.into_iter().map(Action::Hier));
                ui.separator();
                inspector_ui(ui, editor, &diag, pending, actions);
                arrange_ui(ui, editor, actions);
                if !matches!(editor.doc.mode, DocMode::Instance(_)) {
                    comments_ui(ui, editor, pending, actions);
                }
                ui.separator();
                if matches!(editor.doc.mode, DocMode::Graph) {
                    super::params_panel::params_ui(ui, editor, pending);
                    declarations_ui(ui, editor, &diag, pending);
                }
                ui.separator();
                errors_ui(ui, editor, &diag);
                wgsl_ui(ui, editor);
                if !matches!(editor.doc.mode, DocMode::Function(_)) {
                    ui.separator();
                    policy_ui(ui, editor);
                }
            });
        });
    egui::Panel::left("matgraph-nodes")
        .resizable(true)
        .default_size(200.0)
        .show(ui, |ui| nodes_ui(ui, editor, pending));
    egui::CentralPanel::default().show(ui, |ui| {
        canvas_ui(ui, editor, device, &diag, accents, pending, actions)
    });
}

/// Kinds of the palette that can be wired after a node (they have an input), matching `filter`
/// (case-insensitive); `Input` only inside a function.
#[must_use]
pub fn palette_kinds(function: bool, filter: &str) -> Vec<String> {
    let filter = filter.trim().to_ascii_lowercase();
    Node::KINDS
        .iter()
        .filter(|k| function || **k != "Input")
        .filter(|k| Node::default_of(k).is_some_and(|n| !n.input_slots().is_empty()))
        .filter(|k| filter.is_empty() || k.to_ascii_lowercase().contains(&filter))
        .map(|k| (*k).to_string())
        .collect()
}

/// The Nodes list beside the canvas: the keyboard route to add, wire, move, delete and select
/// nodes (ADR-7383). Its edits are the ones `editor.matgraph_edit` makes.
fn nodes_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, pending: &mut Vec<PendingEdit>) {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let names: Vec<String> = editor.doc.graph.nodes.keys().cloned().collect();
    // The focus follows its node when rows move.
    crate::graph_kit::canvas::follow_anchor(&mut editor.ui.keys, editor.ui.keys_node.as_ref(), &names);
    let function = matches!(editor.doc.mode, DocMode::Function(_));
    let selected = editor
        .ui
        .selected
        .as_ref()
        .and_then(|s| names.iter().position(|n| n == s));
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let kinds = |_: usize, filter: &str| palette_kinds(function, filter);
    let rows = GraphRows {
        len: names.len(),
        name: &name,
        selected,
        kinds: &kinds,
        named_outputs: false,
        read_only: matches!(editor.doc.mode, DocMode::Instance(_)),
        input_pins: true,
        deletable: true,
        heading: "Nodes",
        noun: "node",
    };
    let actions = graph_keys_ui(ui, "matgraph-nodes", &mut editor.ui.keys, &rows);
    editor.ui.keys_node = crate::graph_kit::canvas::anchor_of(&editor.ui.keys, &names);
    let positions = editor.doc.positions();
    let sink = if function { "Outputs" } else { "Surface" };
    for action in actions {
        match action {
            GraphAction::Activate(row) => {
                let Some(name) = names.get(row).cloned() else {
                    continue;
                };
                let node = editor.ui.snarl.as_ref().and_then(|s| {
                    s.node_ids()
                        .find(|(_, n)| **n == CanvasNode::Node(name.clone()))
                        .map(|(id, _)| id)
                });
                editor.ui.keys.frame = node;
                editor.ui.keys.frame_epoch += 1;
                editor.ui.selected = Some(name);
            }
            GraphAction::Delete(row) => {
                let Some(name) = names.get(row).cloned() else {
                    continue;
                };
                if editor.ui.selected.as_deref() == Some(name.as_str()) {
                    editor.ui.selected = None;
                }
                pending.push(PendingEdit::new("delete node", None, move |d| {
                    d.remove_node(&name).map(|_| ())
                }));
            }
            GraphAction::Nudge(row, delta) => {
                let Some(name) = names.get(row).cloned() else {
                    continue;
                };
                let at = positions.get(&name).copied().unwrap_or([0.0, 0.0]);
                let to = [at[0] + delta[0], at[1] + delta[1]];
                // No merge key: each key press is its own undo entry.
                pending.push(PendingEdit::new("move node", None, move |d| {
                    d.move_node(&name, to)
                }));
            }
            GraphAction::AddAfter(row, kind) => {
                let Some(from) = names.get(row).cloned() else {
                    continue;
                };
                let at = positions.get(&from).copied().unwrap_or([0.0, 0.0]);
                let pos = [at[0] + 220.0, at[1]];
                pending.push(PendingEdit::new(format!("add {kind}"), None, move |d| {
                    let node = Node::default_of(&kind).ok_or_else(|| format!("unknown kind {kind}"))?;
                    let slot = node.input_slots().into_iter().next().map(|(s, _)| s);
                    let name = d.add_node(None, node, Some(pos))?;
                    match slot {
                        Some(slot) => d.connect(&from, &name, &slot),
                        None => Ok(()),
                    }
                }));
            }
            GraphAction::Connect(row, form) => {
                let Some(from) = names.get(row).cloned() else {
                    continue;
                };
                let target = form.target.trim().to_string();
                let slot = form.input.trim().to_string();
                if target == sink {
                    pending.push(PendingEdit::new("set output", None, move |d| {
                        d.set_output(&slot, Some(&from))
                    }));
                } else {
                    pending.push(PendingEdit::new("connect", None, move |d| {
                        d.connect(&from, &target, &slot)
                    }));
                }
            }
        }
    }
}

fn canvas_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    device: Option<&RenderDevice>,
    diag: &Diagnostics,
    accents: crate::theme::AccentTokens,
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    if editor.ui.export_clipboard {
        editor.ui.export_clipboard = false;
        if let Some(text) = &editor.clipboard {
            ui.ctx().copy_text(text.clone());
        }
    }
    if editor.ui.node_previews
        && let Some(device) = device
    {
        node_previews_frame(ui.ctx(), editor, device);
    }
    let revision = editor.revision;
    if editor.ui.snarl_revision != Some(revision) || editor.ui.snarl.is_none() {
        editor.ui.snarl = Some(build_snarl_with(
            &editor.doc,
            &diag.signatures,
            editor.ui.surface_pos,
        ));
        editor.ui.snarl_revision = Some(revision);
    }
    let Some(mut snarl) = editor.ui.snarl.take() else {
        return;
    };
    let mut selected = editor.ui.selected.clone();
    let mut multi = std::mem::take(&mut editor.ui.multi);
    let mut collapse_form = editor.ui.collapse_form.take();
    let mut wire_query = std::mem::take(&mut editor.ui.wire_query);
    let mut to_global = editor.ui.to_global;
    let rerouted = rerouted_inputs(&editor.doc);
    let pins = Arc::new(Mutex::new(PinMap::default()));
    let mut drag = editor.ui.drag.take();
    let mut comment_text = editor.ui.comment_text.take();
    let (response, wire_slot, visible) = {
        let mut viewer = Viewer {
            doc: &editor.doc,
            diag,
            pending,
            selected: &mut selected,
            multi: &mut multi,
            actions,
            collapse_form: &mut collapse_form,
            accents,
            wire_query: &mut wire_query,
            to_global: &mut to_global,
            swatches: editor.ui.node_previews.then_some(&editor.ui.node_cache),
            rerouted: &rerouted,
            pins: Arc::clone(&pins),
            drag: &mut drag,
            comment_text: &mut comment_text,
            wire_slot: None,
            node_rects: Vec::new(),
            visible: Vec::new(),
            rerouted_last: editor
                .ui
                .wires
                .iter()
                .filter(|w| matches!(w.shape, WireShape::Rerouted(_)))
                .cloned()
                .collect(),
        };
        // Plain wires are cubic Béziers with a fixed frame, so the canvas's wire hit test
        // (`wire_hit::wire_curve`) measures the curve snarl draws (w11, ADR-7730).
        let style = SnarlStyle {
            collapsible: Some(false),
            wire_style: Some(WireStyle::Bezier3),
            wire_frame_size: Some(super::wire_hit::WIRE_FRAME),
            downscale_wire_frame: Some(true),
            upscale_wire_frame: Some(false),
            ..SnarlStyle::new()
        };
        // A fresh canvas id per Enter in the Nodes list (ADR-7383).
        let widget = SnarlWidget::new()
            .id_salt(("matgraph-canvas", editor.ui.keys.frame_epoch))
            .style(style);
        let what = match editor.doc.mode {
            DocMode::Function(_) => "Material function canvas",
            DocMode::Instance(_) => "Material instance canvas (read-only)",
            DocMode::Graph => "Material graph canvas",
        };
        // Enter in the Nodes list frames its node (graph_kit::canvas).
        let response = crate::graph_kit::canvas::show(
            ui,
            &widget,
            &mut snarl,
            &mut viewer,
            editor.ui.keys.frame.take(),
            &format!("{what}, {} nodes", editor.doc.graph.nodes.len()),
        );
        // A box selection (Shift+drag on the canvas) becomes the multi-selection.
        let boxed: Vec<String> = widget
            .get_selected_nodes(ui)
            .into_iter()
            .filter_map(|id| match snarl.get_node(id) {
                Some(CanvasNode::Node(name)) => Some(name.clone()),
                _ => None,
            })
            .collect();
        if boxed != editor.ui.box_seen {
            if boxed.len() > 1 {
                *viewer.multi = boxed.iter().cloned().collect();
            }
            editor.ui.box_seen = boxed;
        }
        let wire_slot = viewer.wire_slot.take();
        editor.ui.node_rects = std::mem::take(&mut viewer.node_rects);
        (response, wire_slot, std::mem::take(&mut viewer.visible))
    };
    if editor.ui.node_previews {
        editor.ui.node_cache.set_visible(visible);
    }
    // A drag ends with the pointer even when its grip went away.
    if !ui.ctx().input(|i| i.pointer.any_down()) {
        drag = None;
    }
    editor.ui.drag = drag;
    let rerouted_wires = draw_reroutes(&editor.doc, &diag.signatures, &snarl, &rerouted, &pins, wire_slot);
    editor.ui.reroute_paths = rerouted_wires
        .iter()
        .filter_map(|w| match &w.shape {
            WireShape::Rerouted(path) => Some(path.clone()),
            WireShape::Plain(_) => None,
        })
        .collect();
    editor.ui.wires = plain_wires(&editor.doc, &diag.signatures, &snarl, &pins);
    editor.ui.wires.extend(rerouted_wires);
    comment_text_ui(ui, editor, comment_text, pending);
    editor.ui.wire_query = wire_query;
    editor.ui.to_global = to_global;
    wire_clicks(ui, editor, &response, pending);
    canvas_keys(ui, editor, &response, actions);
    search_popup(ui, editor, actions);
    editor.ui.multi = multi;
    editor.ui.collapse_form = collapse_form;
    // Node drags become coalesced move edits (one undo entry per node and drag). An instance's
    // layout is its parent's: a drag there is undone by rebuilding the canvas.
    let read_only = matches!(editor.doc.mode, DocMode::Instance(_));
    let positions = editor.doc.positions();
    let mut moved_read_only = false;
    for (_, pos, value) in snarl.nodes_pos_ids() {
        match value {
            CanvasNode::Node(name) => {
                if let Some(old) = positions.get(name)
                    && ((old[0] - pos.x).abs() > 0.5 || (old[1] - pos.y).abs() > 0.5)
                {
                    if read_only {
                        moved_read_only = true;
                        continue;
                    }
                    let name = name.clone();
                    let p = [pos.x, pos.y];
                    pending.push(PendingEdit::new(
                        "move node",
                        Some(format!("move:{name}")),
                        move |d| d.move_node(&name, p),
                    ));
                }
            }
            CanvasNode::Surface => editor.ui.surface_pos = Some([pos.x, pos.y]),
        }
    }
    editor.ui.snarl = Some(snarl);
    editor.ui.selected = selected;
    if moved_read_only && !ui.ctx().input(|i| i.pointer.any_down()) {
        editor.ui.invalidate_canvas();
    }
}

/// The plain wires snarl drew this frame into node inputs, as the cubic Béziers it draws
/// ([`super::wire_hit::wire_curve`] between the pins it placed; graph space). Wires into the
/// Surface (or Outputs) sink are left out: reroute points belong to node inputs.
fn plain_wires(
    doc: &MatGraphDoc,
    sigs: &BTreeMap<String, Signature>,
    snarl: &Snarl<CanvasNode>,
    pins: &Mutex<PinMap>,
) -> Vec<DrawnWire> {
    let pins = pins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut wires = Vec::new();
    for (out, input) in snarl.wires() {
        let (Some(CanvasNode::Node(to)), Some(&(from_pos, _)), Some(&(to_pos, _))) = (
            snarl.get_node(input.node),
            pins.outputs.get(&out),
            pins.inputs.get(&input),
        ) else {
            continue;
        };
        let Some(slot) = doc
            .graph
            .nodes
            .get(to)
            .and_then(|n| slot_names_with(doc, sigs, n).get(input.input).cloned())
        else {
            continue;
        };
        wires.push(DrawnWire {
            to: to.clone(),
            slot,
            shape: WireShape::Plain(super::wire_hit::wire_curve(from_pos, to_pos)),
        });
    }
    wires
}

/// Clicks on the canvas's wires (spec 80 § 12.1, w11): a click on a plain wire adds a reroute
/// point there, a click on a rerouted wire adds one on the span it hit, and a right-click on a
/// rerouted wire disconnects its input (snarl already disconnects a plain wire on a
/// right-click). One undo step each. A click over a node, a pin, a reroute point or a comment
/// frame's grips is theirs; an instance's graph is read-only. A frame egui runs twice acts once.
fn wire_clicks(
    ui: &egui::Ui,
    editor: &mut MatGraphEditor,
    response: &egui::Response,
    pending: &mut Vec<PendingEdit>,
) {
    let ctx = ui.ctx();
    let (primary, secondary, pointer) = ctx.input(|i| {
        (
            i.pointer.primary_clicked(),
            i.pointer.secondary_clicked(),
            i.pointer.interact_pos(),
        )
    });
    if !(primary || secondary) || matches!(editor.doc.mode, DocMode::Instance(_)) {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    let (Some(pointer), Some(transform)) = (pointer, editor.ui.to_global) else {
        return;
    };
    if editor.ui.wire_click_frame == Some(frame) || !response.rect.contains(pointer) {
        return;
    }
    let at = transform.inverse() * pointer;
    let pin_margin = 10.0;
    let over_node = editor
        .ui
        .node_rects
        .iter()
        .any(|r| r.expand(pin_margin).contains(at));
    let over_point = editor
        .doc
        .notes
        .reroutes
        .iter()
        .flat_map(|r| r.points.iter())
        .any(|p| (egui::pos2(p[0], p[1]) - at).length() <= canvas::REROUTE_GRIP + 2.0);
    let over_frame = editor
        .doc
        .notes
        .comments
        .iter()
        .flat_map(|c| canvas::frame_grips(c.min, c.max))
        .any(|(_, r)| r.contains(at));
    if over_node || over_point || over_frame || ctx.any_popup_open() {
        return;
    }
    let threshold = super::wire_hit::WIRE_HIT / transform.scaling.max(1e-3);
    let Some(hit) = super::wire_hit::hit_wire(at, &editor.ui.wires, threshold) else {
        return;
    };
    let Some(wire) = editor.ui.wires.get(hit.index).cloned() else {
        return;
    };
    editor.ui.wire_click_frame = Some(frame);
    let (to, slot) = (wire.to, wire.slot);
    let point = [hit.at.x, hit.at.y];
    match (&wire.shape, primary) {
        (WireShape::Plain(_), true) => {
            pending.push(PendingEdit::new("add reroute point", None, move |d| {
                d.set_reroute(&to, &slot, vec![point])
            }));
        }
        (WireShape::Rerouted(_), true) => {
            let mut points = editor
                .doc
                .notes
                .reroutes
                .iter()
                .find(|r| r.to == to && r.slot == slot)
                .map(|r| r.points.clone())
                .unwrap_or_default();
            points.insert(hit.span.min(points.len()), point);
            pending.push(PendingEdit::new("add reroute point", None, move |d| {
                d.set_reroute(&to, &slot, points)
            }));
        }
        (WireShape::Rerouted(_), false) => {
            pending.push(PendingEdit::new("disconnect", None, move |d| {
                d.disconnect(&to, &slot)
            }));
        }
        // Snarl disconnects a plain wire on a right-click itself (`Viewer::disconnect`).
        (WireShape::Plain(_), false) => {}
    }
}

/// Draws each rerouted wire as one curve from its output pin through its points to its input
/// pin ([`canvas::reroute_path`]) into the reserved slot behind the nodes. Returns the wires
/// drawn (graph space).
fn draw_reroutes(
    doc: &MatGraphDoc,
    sigs: &BTreeMap<String, Signature>,
    snarl: &Snarl<CanvasNode>,
    rerouted: &BTreeSet<(String, String)>,
    pins: &Mutex<PinMap>,
    slot: Option<(egui::Painter, egui::layers::ShapeIdx)>,
) -> Vec<DrawnWire> {
    let ids: HashMap<&str, egui_snarl::NodeId> = snarl
        .node_ids()
        .filter_map(|(id, n)| match n {
            CanvasNode::Node(name) => Some((name.as_str(), id)),
            CanvasNode::Surface => None,
        })
        .collect();
    let pins = pins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut paths = Vec::new();
    let mut shapes = Vec::new();
    for reroute in doc
        .notes
        .reroutes
        .iter()
        .filter(|r| rerouted.contains(&(r.to.clone(), r.slot.clone())))
    {
        let Some(node) = doc.graph.nodes.get(&reroute.to) else {
            continue;
        };
        let Some(input) = slot_names_with(doc, sigs, node)
            .iter()
            .position(|s| *s == reroute.slot)
        else {
            continue;
        };
        let (Some(&to_id), Some(&from_id)) = (
            ids.get(reroute.to.as_str()),
            wired(node, &reroute.slot).and_then(|f| ids.get(f)),
        ) else {
            continue;
        };
        let (Some(&(to, to_color)), Some(&(from, from_color))) = (
            pins.inputs.get(&InPinId { node: to_id, input }),
            pins.outputs.get(&OutPinId {
                node: from_id,
                output: 0,
            }),
        ) else {
            continue;
        };
        let points: Vec<egui::Pos2> = reroute.points.iter().map(|p| egui::pos2(p[0], p[1])).collect();
        let path = canvas::reroute_path(from, &points, to);
        let color = egui::Color32::from_rgba_premultiplied(
            from_color.r().midpoint(to_color.r()),
            from_color.g().midpoint(to_color.g()),
            from_color.b().midpoint(to_color.b()),
            from_color.a().midpoint(to_color.a()),
        );
        shapes.push(egui::Shape::line(path.clone(), egui::Stroke::new(2.0, color)));
        paths.push(DrawnWire {
            to: reroute.to.clone(),
            slot: reroute.slot.clone(),
            shape: WireShape::Rerouted(path),
        });
    }
    if let Some((painter, idx)) = slot {
        painter.set(idx, egui::Shape::Vec(shapes));
    }
    paths
}

/// The comment text field on the canvas (a double-click on a frame's header opens it): Enter
/// or leaving the field keeps the text as one edit, Escape drops it.
fn comment_text_ui(
    ui: &egui::Ui,
    editor: &mut MatGraphEditor,
    editing: Option<(usize, String)>,
    pending: &mut Vec<PendingEdit>,
) {
    let Some((index, mut text)) = editing else {
        return;
    };
    let (Some(comment), Some(to_global)) = (editor.doc.notes.comments.get(index), editor.ui.to_global) else {
        return;
    };
    let at = to_global * egui::pos2(comment.min[0] + 6.0, comment.min[1] + 4.0);
    let width = ((comment.max[0] - comment.min[0] - 12.0) * to_global.scaling).max(80.0);
    let mut keep = true;
    egui::Area::new(egui::Id::new("matgraph-comment-text"))
        .fixed_pos(at)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            let field = ui.add(egui::TextEdit::singleline(&mut text).desired_width(width));
            if !field.has_focus() && !field.lost_focus() {
                field.request_focus();
            }
            let (enter, escape) =
                ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
            if escape {
                keep = false;
            } else if enter || field.lost_focus() {
                keep = false;
                let text = text.clone();
                pending.push(PendingEdit::new("edit comment", None, move |d| {
                    d.set_comment(index, Some(&text), None)
                }));
            }
        });
    if keep {
        editor.ui.comment_text = Some((index, text));
    }
}

/// Canvas keys while the pointer is over it and no text field has focus: Space or Tab opens
/// the node search at the pointer; Copy, Cut and Paste (the platform's shortcuts) act on the
/// picked nodes. A node clipboard on the system clipboard pastes as such (across editors).
fn canvas_keys(
    ui: &egui::Ui,
    editor: &mut MatGraphEditor,
    response: &egui::Response,
    actions: &mut Vec<Action>,
) {
    let ctx = ui.ctx();
    if !response.contains_pointer() || ctx.memory(|m| m.focused().is_some()) {
        return;
    }
    let read_only = matches!(editor.doc.mode, DocMode::Instance(_));
    let pointer = ctx
        .input(|i| i.pointer.hover_pos())
        .unwrap_or_else(|| response.rect.center());
    let at = editor.ui.to_global.map_or(pointer, |t| t.inverse() * pointer);
    let (open, copy, cut, paste) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::Tab),
            i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
            i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            }),
        )
    });
    if open && !read_only && editor.ui.search.is_none() {
        editor.ui.search = Some(SearchState {
            query: String::new(),
            pos: [at.x, at.y],
            from: None,
            screen: [pointer.x, pointer.y],
        });
    }
    if copy {
        actions.push(Action::Copy);
    }
    if cut && !read_only {
        actions.push(Action::Cut);
    }
    if let Some(text) = paste
        && !read_only
    {
        if text.contains("format_version") && text.contains("offsets") {
            actions.push(Action::PasteText(text, [at.x, at.y]));
        } else {
            actions.push(Action::Paste([at.x, at.y]));
        }
    }
}

/// The node search popup (Space or Tab on the canvas): type to filter, Enter or a click adds
/// the best match where the search opened, Escape closes it.
fn search_popup(ui: &egui::Ui, editor: &mut MatGraphEditor, actions: &mut Vec<Action>) {
    let Some(mut search) = editor.ui.search.take() else {
        return;
    };
    let function = matches!(editor.doc.mode, DocMode::Function(_));
    let list = crate::graph_kit::search::SearchList {
        hint: "search nodes…",
        spoken: "Search nodes",
        max_hits: 12,
        max_height: f32::INFINITY,
        empty: Some("no kind matches"),
        width: None,
    };
    let from = search.from.clone();
    let kinds = |query: &str| super::search::search_kinds(query, from.as_ref(), function);
    let screen = egui::pos2(search.screen[0], search.screen[1]);
    let (picked, escape) =
        crate::graph_kit::search::popup(ui.ctx(), egui::Id::new("matgraph-node-search"), screen, |ui| {
            list.show(ui, &mut search.query, kinds, |k| (*k).to_string())
        });
    let mut keep = !escape;
    if let Some(kind) = picked {
        actions.push(Action::SearchAdd {
            kind: kind.to_string(),
            pos: search.pos,
            from: search.from.clone(),
        });
        keep = false;
    }
    if keep {
        editor.ui.search = Some(search);
    }
}

/// Renders this frame's share of node swatches (at most
/// `node_preview::NODE_PREVIEWS_PER_FRAME`, only after the graph changed).
fn node_previews_frame(ctx: &egui::Context, editor: &mut MatGraphEditor, device: &RenderDevice) {
    if matches!(editor.doc.mode, DocMode::Function(_)) {
        return;
    }
    // Swatches the preview thread finished (ADR-8584): upload and settle them.
    if let Some((_, results)) = editor.ui.node_job.take() {
        for (node, revision, rendered) in results {
            match rendered {
                Ok(swatch) => {
                    let texture = ctx.load_texture(
                        format!("matgraph-node-{:016x}", swatch.key),
                        swatch.image,
                        egui::TextureOptions::LINEAR,
                    );
                    editor
                        .ui
                        .node_cache
                        .settle(&node, revision, Ok(swatch.key), Some(texture));
                }
                Err(err) => editor.ui.node_cache.settle(&node, revision, Err(err), None),
            }
        }
    }
    if editor.ui.node_job.is_busy() {
        return;
    }
    let revision = editor.graph_revision();
    let view = editor.doc.view_graph().into_owned();
    if editor.ui.node_cache.settled(&view, revision) {
        return;
    }
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_node_previews").entered();
    let root = editor.root.clone();
    // Function tabs' definitions first (w11): a swatch follows an edit in a function tab.
    let (graph, dir) = match editor.preview_source() {
        Ok(source) => (source.graph, source.dir),
        Err(_) => (view, editor.graph_dir()),
    };
    let plan = editor
        .ui
        .node_cache
        .plan(&graph, revision, super::node_preview::NODE_PREVIEWS_PER_FRAME);
    if plan.is_empty() {
        return;
    }
    let job_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (revision, &plan).hash(&mut h);
        h.finish()
    };
    // The renders (a pipeline per new swatch key) run on the preview thread.
    let device = device.clone();
    editor.ui.node_job.request(job_key, Some(ctx), move || {
        plan.into_iter()
            .map(|(node, _)| {
                let rendered = super::node_preview::render_node_preview(
                    &device,
                    &graph,
                    &node,
                    dir.as_deref(),
                    root.as_deref(),
                    super::node_preview::NODE_PREVIEW_SIZE,
                );
                (node, revision, rendered)
            })
            .collect()
    });
}

/// Align / distribute and the clipboard for a multi-selection (box select with Shift+drag, or
/// Shift+click).
fn arrange_ui(ui: &mut egui::Ui, editor: &MatGraphEditor, actions: &mut Vec<Action>) {
    let picked = picked(editor);
    if picked.len() < 2 {
        return;
    }
    ui.separator();
    ui.strong(format!("{} nodes picked", picked.len()));
    if !matches!(editor.doc.mode, DocMode::Instance(_)) {
        ui.horizontal_wrapped(|ui| {
            for (op, name) in Arrange::ALL {
                let need = if name.starts_with("distribute") { 3 } else { 2 };
                if ui
                    .add_enabled(
                        picked.len() >= need,
                        egui::Button::new(name.replace('_', " ")).small(),
                    )
                    .clicked()
                {
                    actions.push(Action::Arrange(op));
                }
            }
        });
    }
    ui.horizontal(|ui| {
        if ui.button("Copy").clicked() {
            actions.push(Action::Copy);
        }
        if !matches!(editor.doc.mode, DocMode::Instance(_)) {
            if ui.button("Cut").clicked() {
                actions.push(Action::Cut);
            }
            if ui.button("Comment selection").clicked() {
                actions.push(Action::AddComment);
            }
        }
    });
}

/// The canvas's comment frames and reroutes (layout sidecar v2): rename, resize, remove.
fn comments_ui(
    ui: &mut egui::Ui,
    editor: &MatGraphEditor,
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    let notes = editor.doc.notes.clone();
    egui::CollapsingHeader::new(format!(
        "Comments ({}) and reroutes ({})",
        notes.comments.len(),
        notes.reroutes.len()
    ))
    .id_salt("matgraph-comments")
    .show(ui, |ui| {
        if ui.small_button("Add comment").clicked() {
            actions.push(Action::AddComment);
        }
        for (index, comment) in notes.comments.iter().enumerate() {
            ui.horizontal(|ui| {
                let mut text = comment.text.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut text).desired_width(120.0))
                    .changed()
                {
                    pending.push(PendingEdit::new(
                        "edit comment",
                        Some(format!("comment:{index}")),
                        move |d| d.set_comment(index, Some(&text), None),
                    ));
                }
                let (mut min, mut max) = (comment.min, comment.max);
                let mut moved = false;
                for v in min.iter_mut().chain(max.iter_mut()) {
                    moved |= ui.add(egui::DragValue::new(v).speed(1.0)).changed();
                }
                if moved {
                    pending.push(PendingEdit::new(
                        "resize comment",
                        Some(format!("comment:{index}")),
                        move |d| d.set_comment(index, None, Some((min, max))),
                    ));
                }
                if ui
                    .small_button("\u{1f5d9}")
                    .on_hover_text("Remove the comment")
                    .clicked()
                {
                    pending.push(PendingEdit::new("remove comment", None, move |d| {
                        d.remove_comment(index)
                    }));
                }
            });
        }
        for reroute in &notes.reroutes {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{}.{}: {} point(s)",
                    reroute.to,
                    reroute.slot,
                    reroute.points.len()
                ));
                if ui.small_button("Clear").clicked() {
                    let (to, slot) = (reroute.to.clone(), reroute.slot.clone());
                    pending.push(PendingEdit::new("clear reroute", None, move |d| {
                        d.set_reroute(&to, &slot, Vec::new())
                    }));
                }
            });
        }
    });
}

/// Edits one parameter value in place; true when it changed.
pub(super) fn param_value_ui(ui: &mut egui::Ui, value: &mut ParamValue) -> bool {
    fn drag(ui: &mut egui::Ui, values: &mut [f32]) -> bool {
        // Every component is drawn; any one changing counts.
        values.iter_mut().fold(false, |changed, v| {
            ui.add(egui::DragValue::new(v).speed(0.01)).changed() | changed
        })
    }
    ui.horizontal(|ui| match value {
        ParamValue::Float(x) => drag(ui, std::slice::from_mut(x)),
        ParamValue::Vec2(v) => drag(ui, v),
        ParamValue::Vec3(v) => drag(ui, v),
        ParamValue::Vec4(v) => drag(ui, v),
        ParamValue::Color(c) => ui.color_edit_button_rgb(c).changed(),
    })
    .inner
}

/// A function tab: its calling graph and its signature (inputs as RON, outputs as a list).
fn signature_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, pending: &mut Vec<PendingEdit>) {
    ui.strong("Function");
    match &editor.context {
        Some(c) => {
            ui.weak(format!(
                "Checked against {}",
                c.path.as_ref().map_or_else(
                    || "the unsaved graph it was opened from".into(),
                    |p| p.display().to_string()
                )
            ));
        }
        None => {
            ui.weak("No calling graph: open it from a graph's Call node to check its parameters.");
        }
    }
    let revision = editor.graph_revision();
    let Some(meta) = editor.doc.function_meta().cloned() else {
        return;
    };
    if editor
        .ui
        .signature_text
        .as_ref()
        .is_none_or(|(r, _, _)| *r != revision)
    {
        let inputs = ron::ser::to_string_pretty(&meta.inputs, regia_core::ron_pretty()).unwrap_or_default();
        let outputs = meta.outputs.keys().cloned().collect::<Vec<_>>().join(", ");
        editor.ui.signature_text = Some((revision, inputs, outputs));
    }
    let Some((_, inputs, outputs)) = editor.ui.signature_text.as_mut() else {
        return;
    };
    ui.label("Inputs");
    ui.add(
        egui::TextEdit::multiline(inputs)
            .code_editor()
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        let label = ui.label("Outputs");
        ui.add(egui::TextEdit::singleline(outputs).desired_width(f32::INFINITY))
            .labelled_by(label.id);
    });
    if ui.button("Apply signature").clicked() {
        match ron::from_str::<Vec<regia_matgraph::FunctionInput>>(inputs) {
            Ok(list) => {
                let names: Vec<String> = outputs
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                pending.push(PendingEdit::new("function signature", None, move |d| {
                    d.set_signature(list, names)
                }));
            }
            Err(err) => editor.toast = format!("inputs RON: {err}"),
        }
    }
}

/// An instance tab: every parameter, texture and switch of the parent with its override.
fn instance_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, pending: &mut Vec<PendingEdit>) {
    let Some(instance) = editor.doc.instance_meta().cloned() else {
        return;
    };
    ui.strong("Instance overrides");
    ui.weak(format!("Parent: {} (read-only)", instance.parent));
    // The parent's groups, ranges and tooltips (w13, ADR-8105 (b)).
    super::params_panel::instance_params_ui(ui, &editor.doc.graph, &instance, pending);
    for decl in editor.doc.graph.textures.clone() {
        ui.horizontal(|ui| {
            let mut on = instance.textures.contains_key(&decl.name);
            if ui.checkbox(&mut on, format!("texture {}", decl.name)).changed() {
                let name = decl.name.clone();
                let path = on.then(|| decl.path.clone());
                pending.push(PendingEdit::new("instance texture", None, move |d| {
                    d.set_texture_override(&name, path.as_deref())
                }));
            }
            let mut text = instance
                .textures
                .get(&decl.name)
                .cloned()
                .unwrap_or_else(|| decl.path.clone());
            let response = crate::a11y_audit::name(
                ui.add_enabled(
                    instance.textures.contains_key(&decl.name),
                    egui::TextEdit::singleline(&mut text).desired_width(160.0),
                ),
                egui::WidgetType::TextEdit,
                &format!("Texture {}", decl.name),
            );
            if response.lost_focus() && instance.textures.get(&decl.name) != Some(&text) {
                let name = decl.name.clone();
                pending.push(PendingEdit::new("instance texture", None, move |d| {
                    d.set_texture_override(&name, Some(&text))
                }));
            }
        });
    }
    for decl in editor.doc.graph.switches.clone() {
        ui.horizontal(|ui| {
            let overridden = instance.switches.get(&decl.name).copied();
            let mut on = overridden.is_some();
            if ui.checkbox(&mut on, format!("switch {}", decl.name)).changed() {
                let name = decl.name.clone();
                let value = on.then_some(decl.default);
                pending.push(PendingEdit::new("instance switch", None, move |d| {
                    d.set_switch_override(&name, value)
                }));
            }
            if let Some(mut value) = overridden
                && ui.checkbox(&mut value, "on").changed()
            {
                let name = decl.name.clone();
                pending.push(PendingEdit::new("instance switch", None, move |d| {
                    d.set_switch_override(&name, Some(value))
                }));
            }
        });
    }
}

/// The collapse form, shown once nodes are picked (Shift+click, or a node's "Collapse into
/// function…").
fn collapse_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, actions: &mut Vec<Action>) {
    let mut picked = editor.ui.multi.clone();
    if let Some(sel) = &editor.ui.selected
        && !picked.is_empty()
    {
        picked.insert(sel.clone());
    }
    if picked.is_empty() && editor.ui.collapse_form.is_none() {
        return;
    }
    ui.separator();
    ui.strong(format!("Collapse {} node(s) into a function", picked.len()));
    let form = editor
        .ui
        .collapse_form
        .get_or_insert_with(|| ("collapsed".to_string(), String::new()));
    ui.horizontal(|ui| {
        let label = ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut form.0).desired_width(140.0))
            .labelled_by(label.id);
    });
    ui.horizontal(|ui| {
        let label = ui.label("File");
        ui.add(
            egui::TextEdit::singleline(&mut form.1)
                .hint_text("inline, or assets/….matfunc.ron")
                .desired_width(180.0),
        )
        .labelled_by(label.id);
    });
    let mut cancel = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!picked.is_empty(), egui::Button::new("Collapse"))
            .clicked()
        {
            let file = (!form.1.trim().is_empty()).then(|| form.1.trim().to_string());
            actions.push(Action::Collapse(picked.clone(), form.0.clone(), file));
        }
        cancel = ui.button("Cancel").clicked();
    });
    if cancel {
        editor.ui.collapse_form = None;
        editor.ui.multi.clear();
    }
}

fn preview_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    device: Option<&RenderDevice>,
    diag: &Diagnostics,
) {
    use crate::material_editor::PreviewShape;
    ui.strong("Preview");
    // The tier also picks the Stats strip's profile, so it shows without a GPU too (w12).
    tier_ui(ui, editor);
    // The shared preview scene, as the other asset viewers have it (w13, P-083, ADR-7771); like
    // the tier it shows without a GPU, so the agent and the keyboard reach it everywhere.
    crate::preview_scene::preview_scene_controls(ui, &mut editor.preview_scene.settings);
    editor.preview_scene.tick(ui.ctx());
    let Some(device) = device else {
        ui.weak("No GPU device: the preview needs one.");
        super::cost::stats_strip_ui(ui, editor, diag.key);
        return;
    };
    preview_controls_ui(
        ui,
        &mut editor.ui.preview_shape,
        &mut editor.ui.preview_view,
        &mut editor.ui.mesh_field,
    );
    let side = ui.available_width().clamp(96.0, 320.0);
    let size = [side as u32, side as u32];
    let settings = preview_settings_hash(&editor.ui.preview_shape, &editor.ui.preview_view)
        ^ editor.ui.parity.preview_hash().rotate_left(17)
        ^ super::preview::scene_hash(&editor.preview_scene).rotate_left(31);
    let want = (editor.graph_revision(), size, settings);
    // A render the preview thread finished (ADR-8584): upload it, or say why it has none.
    if let Some((_, (have, rendered))) = editor.ui.preview_job.take() {
        match rendered {
            Ok(image) => {
                let texture =
                    ui.ctx()
                        .load_texture("regia-matgraph-preview", image, egui::TextureOptions::LINEAR);
                editor.ui.preview = Some((have, texture));
                editor.ui.preview_error = None;
            }
            Err(err) => {
                // Keep the last good image; say why this revision has none.
                editor.ui.preview_error = Some(err);
                if let Some((shown, _)) = editor.ui.preview.as_mut() {
                    *shown = have;
                }
            }
        }
    }
    let stale = editor.ui.preview.as_ref().is_none_or(|(have, _)| *have != want);
    let job_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        want.hash(&mut h);
        h.finish()
    };
    if stale && diag.ok && editor.ui.preview_job.requested() != Some(job_key) {
        // An instance previews as its parent with the overrides; a function tab as its calling
        // graph with the tab's definition (w11, `MatGraphEditor::preview_source`).
        let shape: PreviewShape = editor.ui.preview_shape.clone();
        let pinned = editor.ui.parity.pinned.clone();
        let profile = editor.ui.parity.profile;
        let scene = editor.preview_scene;
        let view = editor.ui.preview_view;
        let root = editor.root.clone();
        let device = device.clone();
        match editor.preview_source() {
            Ok(source) => {
                // The compile, the mesh load and the render (the renderer's pipelines on first
                // use) run on the preview thread; this frame only queues them.
                editor.ui.preview_job.request(job_key, Some(ui.ctx()), move || {
                    let rendered = (|| {
                        // A pinned node draws its own value (w12, the swatch graph of
                        // `node_preview`).
                        let graph = match &pinned {
                            Some(node) => super::node_preview::derived_graph(&source.graph, node)?,
                            None => source.graph,
                        };
                        let geometry = crate::material_editor::preview_geometry(&shape, root.as_deref())
                            .map_err(|e| format!("the preview mesh cannot load: {e}"))?;
                        super::preview::render_graph_preview_scene(
                            &device,
                            &graph,
                            source.dir.as_deref(),
                            root.as_deref(),
                            [side, side],
                            &geometry,
                            &view,
                            profile,
                            &scene,
                        )
                    })();
                    (want, rendered)
                });
            }
            Err(err) => {
                editor.ui.preview_error = Some(err);
                if let Some((have, _)) = editor.ui.preview.as_mut() {
                    *have = want;
                }
            }
        }
    } else if stale && !diag.ok {
        editor.ui.preview_error = Some("fix the errors to update the preview".into());
    }
    if let Some((_, texture)) = &editor.ui.preview {
        let response =
            ui.add(egui::Image::new((texture.id(), egui::vec2(side, side))).sense(egui::Sense::drag()));
        orbit_drag(&response, &mut editor.ui.preview_view);
    } else if editor.ui.preview_job.is_busy() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak("Compiling the preview…");
        });
    }
    if let Some(err) = &editor.ui.preview_error {
        ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.log_warn, err);
    }
    super::cost::stats_strip_ui(ui, editor, diag.key);
}

/// The preview's tier picker (the render profiles the permutation policy enumerates, or the
/// editor's own quality), its badge, and the pinned node with its Unpin button (w12).
fn tier_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    ui.horizontal_wrapped(|ui| {
        ui.label("Tier");
        let shown = editor
            .ui
            .parity
            .profile
            .map_or("Editor", regia_render::RenderProfilePreset::name);
        let combo = egui::ComboBox::from_id_salt("matgraph-preview-tier")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut editor.ui.parity.profile, None, "Editor");
                for preset in super::cost::profiles() {
                    ui.selectable_value(&mut editor.ui.parity.profile, Some(preset), preset.name());
                }
            })
            .response;
        crate::a11y_audit::name(combo, egui::WidgetType::ComboBox, "Preview tier");
        // The badge: which tier the image shows.
        ui.colored_label(accents.active, format!("[{shown}]"));
    });
    if let Some(node) = editor.ui.parity.pinned.clone() {
        ui.horizontal(|ui| {
            ui.colored_label(accents.select, format!("Showing node `{node}`"));
            let unpin = crate::a11y_audit::name(
                ui.small_button("Unpin"),
                egui::WidgetType::Button,
                "Show the material again",
            );
            if unpin.clicked() || !editor.doc.graph.nodes.contains_key(&node) {
                editor.ui.parity.pinned = None;
            }
        });
    }
}

/// A hash of the preview shape and view (the preview re-renders when it changes).
#[must_use]
pub fn preview_settings_hash(
    shape: &crate::material_editor::PreviewShape,
    view: &crate::material_editor::PreviewView,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    shape.hash(&mut h);
    view.yaw.to_bits().hash(&mut h);
    view.pitch.to_bits().hash(&mut h);
    view.light_yaw.to_bits().hash(&mut h);
    h.finish()
}

/// Dragging the preview image orbits the camera (x: yaw, y: pitch).
pub fn orbit_drag(response: &egui::Response, view: &mut crate::material_editor::PreviewView) {
    if response.dragged() {
        let delta = response.drag_delta();
        view.yaw -= delta.x * 0.01;
        view.pitch = (view.pitch + delta.y * 0.01).clamp(-1.4, 1.4);
    }
}

/// The preview's shape buttons (sphere, cube, plane, a project mesh), the light turn and a view
/// reset; shared by the material graph editor and the Material panel (ADR-9700).
pub fn preview_controls_ui(
    ui: &mut egui::Ui,
    shape: &mut crate::material_editor::PreviewShape,
    view: &mut crate::material_editor::PreviewView,
    mesh_field: &mut String,
) {
    use crate::material_editor::PreviewShape;
    ui.horizontal_wrapped(|ui| {
        for name in PreviewShape::NAMES {
            if name == "mesh" {
                continue;
            }
            if ui.selectable_label(shape.name() == name, name).clicked()
                && let Ok(next) = PreviewShape::parse(name, None)
            {
                *shape = next;
            }
        }
        if ui.small_button("reset view").clicked() {
            *view = crate::material_editor::PreviewView::default();
        }
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(mesh_field)
                .hint_text("assets/….glb / .gltf")
                .desired_width(150.0),
        );
        if ui
            .selectable_label(matches!(shape, PreviewShape::Mesh(_)), "mesh")
            .on_hover_text("Preview on this project mesh (its first part)")
            .clicked()
            && !mesh_field.trim().is_empty()
        {
            *shape = PreviewShape::Mesh(mesh_field.trim().to_string());
        }
    });
    ui.add(
        egui::Slider::new(&mut view.light_yaw, -std::f32::consts::PI..=std::f32::consts::PI).text("light"),
    );
}

fn inspector_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    diag: &Diagnostics,
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    ui.strong("Node");
    let Some(name) = editor.ui.selected.clone() else {
        ui.weak("Click a node's title to select it; right-click the canvas to add one.");
        return;
    };
    let Some(node) = editor.doc.graph.nodes.get(&name).cloned() else {
        editor.ui.selected = None;
        return;
    };
    let graph_revision = editor.graph_revision();
    let reload = editor
        .ui
        .node_text
        .as_ref()
        .is_none_or(|(n, rev, _)| *n != name || *rev != graph_revision);
    if reload {
        let pretty = ron::ser::to_string_pretty(&node, regia_core::ron_pretty()).unwrap_or_default();
        editor.ui.node_text = Some((name.clone(), graph_revision, pretty));
        editor.ui.rename = name.clone();
    }
    ui.horizontal(|ui| {
        ui.label(node.kind());
        let field = ui.add(egui::TextEdit::singleline(&mut editor.ui.rename).desired_width(140.0));
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Node name");
        if ui.button("Rename").clicked() && editor.ui.rename != name {
            let (from, to) = (name.clone(), editor.ui.rename.clone());
            editor.ui.rename_follow = Some((from.clone(), to.clone()));
            pending.push(PendingEdit::new("rename node", None, move |d| {
                d.rename_node(&from, &to)
            }));
        }
    });
    typed_rows_ui(ui, editor, &name, &node, pending, actions);
    node_actions_ui(ui, editor, &name, pending, actions);
    reroute_buttons_ui(ui, editor, &name, &node, pending);
    // The Code node's code view (P-175, ADR-7852).
    super::code_editor::code_editor_ui(ui, editor, &name, &node, pending);
    egui::CollapsingHeader::new("Advanced (RON)")
        .id_salt("matgraph-node-ron")
        .show(ui, |ui| {
            if let Some((_, _, text)) = editor.ui.node_text.as_mut() {
                ui.add(
                    egui::TextEdit::multiline(text)
                        .code_editor()
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                if ui.button("Apply").clicked() {
                    match ron::from_str::<Node>(text) {
                        Ok(parsed) => {
                            let n = name.clone();
                            pending.push(PendingEdit::new("set node", None, move |d| {
                                d.set_node(&n, parsed)
                            }));
                        }
                        Err(err) => editor.toast = format!("node RON: {err}"),
                    }
                }
            }
        });
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    for message in diag.for_target(&Target::Node(name)) {
        ui.colored_label(accents.error_text, message);
    }
}

/// The node menu's w12 items for the selected node, as buttons (the keyboard route of the
/// context menu, ADR-7851): Find usages, Preview this node, Disconnect all.
fn node_actions_ui(
    ui: &mut egui::Ui,
    editor: &MatGraphEditor,
    name: &str,
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        if super::usages::query_for_node(&editor.doc.graph, name, None, None).is_some()
            && ui.button("Find usages").clicked()
        {
            actions.push(Action::FindUsages(name.to_string()));
        }
        if editor.ui.parity.pinned.as_deref() == Some(name) {
            if ui.button("Unpin preview").clicked() {
                actions.push(Action::Pin(None));
            }
        } else if ui.button("Preview this node").clicked() {
            actions.push(Action::Pin(Some(name.to_string())));
        }
        if !matches!(editor.doc.mode, DocMode::Instance(_)) && ui.button("Disconnect all").clicked() {
            let n = name.to_string();
            pending.push(PendingEdit::new("disconnect all", None, move |d| {
                d.disconnect_all(&n).map(|_| ())
            }));
        }
    });
}

/// The selected node's typed rows (spec 80 § 12, ADR-9700): drag values and color pickers
/// for constants, dropdowns for names and enums, the texture picker with a thumbnail, the
/// parameter's default with its type's widget, `Noise` octaves clamped 1..8, the function
/// picker. Each change is one edit; a drag coalesces until the pointer is released.
fn typed_rows_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    name: &str,
    node: &Node,
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<Action>,
) {
    let rows = super::inspector::fields(node);
    if rows.is_empty() {
        ui.weak("No values to set: its inputs are wired on the canvas.");
    }
    for row in rows {
        ui.horizontal(|ui| {
            ui.label(row.name);
            match (row.kind, &row.value) {
                (FieldKind::Float, FieldValue::Float(v)) => {
                    let mut x = *v;
                    if ui.add(egui::DragValue::new(&mut x).speed(0.01)).changed() {
                        actions.push(Action::SetField {
                            node: name.to_string(),
                            field: row.name,
                            value: FieldValue::Float(x),
                        });
                    }
                }
                (FieldKind::Floats(n), FieldValue::Floats(v)) => {
                    let mut values = v.clone();
                    let mut changed = false;
                    for x in &mut values {
                        changed |= ui.add(egui::DragValue::new(x).speed(0.01)).changed();
                    }
                    if n >= 3 && values.len() >= 3 {
                        let mut rgb = [values[0], values[1], values[2]];
                        if ui.color_edit_button_rgb(&mut rgb).changed() {
                            values[..3].copy_from_slice(&rgb);
                            changed = true;
                        }
                    }
                    if changed {
                        actions.push(Action::SetField {
                            node: name.to_string(),
                            field: row.name,
                            value: FieldValue::Floats(values),
                        });
                    }
                }
                (FieldKind::Int { min, max }, FieldValue::Int(v)) => {
                    let mut x = *v;
                    if ui.add(egui::DragValue::new(&mut x).range(min..=max)).changed() {
                        actions.push(Action::SetField {
                            node: name.to_string(),
                            field: row.name,
                            value: FieldValue::Int(x),
                        });
                    }
                }
                (FieldKind::Channels, FieldValue::Text(current)) => {
                    let mut letters = current.clone();
                    let response = ui.add(egui::TextEdit::singleline(&mut letters).desired_width(60.0));
                    if response.lost_focus() && letters != *current {
                        if super::inspector::valid_channels(&letters) {
                            actions.push(Action::SetField {
                                node: name.to_string(),
                                field: row.name,
                                value: FieldValue::Text(letters),
                            });
                        } else {
                            editor.toast = format!("bad swizzle `{letters}`: 1–4 letters of xyzw or of rgba");
                        }
                    }
                }
                (FieldKind::CallOutput, FieldValue::OptText(current)) => {
                    let options = super::agent_s2::options(editor, node, row.kind).unwrap_or_default();
                    let shown = current.clone().unwrap_or_else(|| "(its only output)".into());
                    egui::ComboBox::from_id_salt(("matgraph-field", row.name))
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(current.is_none(), "(its only output)")
                                .clicked()
                            {
                                actions.push(Action::SetField {
                                    node: name.to_string(),
                                    field: row.name,
                                    value: FieldValue::OptText(None),
                                });
                            }
                            for option in options {
                                let on = current.as_deref() == Some(option.as_str());
                                if ui.selectable_label(on, option.as_str()).clicked() {
                                    actions.push(Action::SetField {
                                        node: name.to_string(),
                                        field: row.name,
                                        value: FieldValue::OptText(Some(option)),
                                    });
                                }
                            }
                        });
                }
                (kind, FieldValue::Text(current)) => {
                    let options = super::agent_s2::options(editor, node, kind).unwrap_or_default();
                    let shown = if current.is_empty() {
                        "(pick)"
                    } else {
                        current.as_str()
                    };
                    egui::ComboBox::from_id_salt(("matgraph-field", row.name))
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            for option in options {
                                if ui.selectable_label(option == *current, option.as_str()).clicked() {
                                    actions.push(Action::SetField {
                                        node: name.to_string(),
                                        field: row.name,
                                        value: FieldValue::Text(option),
                                    });
                                }
                            }
                        });
                }
                (_, other) => {
                    ui.weak(format!("{other:?}"));
                }
            }
        });
        match (row.kind, &row.value) {
            (FieldKind::Param, FieldValue::Text(param)) => param_default_ui(ui, editor, param, pending),
            (FieldKind::Texture, FieldValue::Text(texture)) => {
                texture_picker_ui(ui, editor, texture, pending)
            }
            _ => {}
        }
    }
}

/// A `Param` node's declared default, edited with its type's widget (one coalesced edit).
fn param_default_ui(ui: &mut egui::Ui, editor: &MatGraphEditor, param: &str, pending: &mut Vec<PendingEdit>) {
    let Some(decl) = editor.doc.graph.params.iter().find(|d| d.name == param) else {
        if !param.is_empty() {
            ui.weak(format!("`{param}` is not declared (Parameters below)"));
        }
        return;
    };
    ui.horizontal(|ui| {
        ui.label("default");
        let mut value = decl.value;
        if param_value_ui(ui, &mut value) && value.is_finite() {
            let param = param.to_string();
            pending.push(PendingEdit::new(
                "parameter default",
                Some(format!("param_default:{param}")),
                move |d| {
                    let decl = d
                        .graph
                        .params
                        .iter_mut()
                        .find(|p| p.name == param)
                        .ok_or_else(|| format!("no parameter `{param}`"))?;
                    decl.value = value;
                    Ok(())
                },
            ));
        }
    });
}

/// The image files under the project's `assets/` (project-relative, at most 400), for the
/// texture picker.
fn project_images(root: Option<&std::path::Path>) -> Vec<String> {
    let Some(root) = root else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && ["png", "jpg", "jpeg", "tga", "bmp", "hdr", "exr", "ktx2", "dds"]
                    .contains(&ext.to_ascii_lowercase().as_str())
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= 400 {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

/// A texture node's picker extras: the declared texture's thumbnail and a project image
/// picker that changes the declaration's file (one edit).
fn texture_picker_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    texture: &str,
    pending: &mut Vec<PendingEdit>,
) {
    let Some(decl) = editor
        .doc
        .graph
        .textures
        .iter()
        .find(|d| d.name == texture)
        .cloned()
    else {
        if !texture.is_empty() {
            ui.weak(format!("`{texture}` is not declared (Textures below)"));
        }
        return;
    };
    let dir = editor.graph_dir();
    let file = super::model::graph_file_path(dir.as_deref(), editor.root.as_deref(), &decl.path).ok();
    ui.horizontal(|ui| {
        if let Some(file) = file {
            let ctx = ui.ctx().clone();
            let thumb = editor
                .ui
                .thumbs
                .entry(file.clone())
                .or_insert_with(|| load_thumbnail(&ctx, &file));
            match thumb {
                Some(t) => {
                    ui.image((t.id(), egui::vec2(48.0, 48.0)));
                }
                None => {
                    ui.weak("(no image)");
                }
            }
        }
        let root = editor.root.clone();
        egui::ComboBox::from_id_salt(("matgraph-texture-file", texture))
            .selected_text(decl.path.clone())
            .width(160.0)
            .show_ui(ui, |ui| {
                for image in project_images(root.as_deref()) {
                    if ui.selectable_label(image == decl.path, image.as_str()).clicked() {
                        let name = decl.name.clone();
                        pending.push(PendingEdit::new("texture file", None, move |d| {
                            let decl = d
                                .graph
                                .textures
                                .iter_mut()
                                .find(|t| t.name == name)
                                .ok_or_else(|| format!("no texture `{name}`"))?;
                            decl.path = image;
                            Ok(())
                        }));
                    }
                }
            });
    });
}

/// A small thumbnail of an image file (nearest sampling to at most 64 pixels a side).
fn load_thumbnail(ctx: &egui::Context, file: &std::path::Path) -> Option<egui::TextureHandle> {
    let texture = crate::mesh_source::load_texture_file(file).ok()?;
    let (w, h) = (texture.width.max(1) as usize, texture.height.max(1) as usize);
    let scale = (w.max(h) as f32 / 64.0).max(1.0);
    let (tw, th) = (
        ((w as f32 / scale) as usize).max(1),
        ((h as f32 / scale) as usize).max(1),
    );
    let mut pixels = Vec::with_capacity(tw * th * 4);
    for y in 0..th {
        for x in 0..tw {
            let sx = ((x as f32 * scale) as usize).min(w - 1);
            let sy = ((y as f32 * scale) as usize).min(h - 1);
            let i = (sy * w + sx) * 4;
            pixels.extend_from_slice(texture.rgba.get(i..i + 4).unwrap_or(&[0, 0, 0, 255]));
        }
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([tw, th], &pixels);
    Some(ctx.load_texture(
        format!("matgraph-thumb-{}", file.display()),
        image,
        egui::TextureOptions::LINEAR,
    ))
}

/// Per wired input of the selected node, a button that adds a reroute point halfway along the
/// wire (layout sidecar v2); the canvas draws the points and the side list clears them.
fn reroute_buttons_ui(
    ui: &mut egui::Ui,
    editor: &MatGraphEditor,
    name: &str,
    node: &Node,
    pending: &mut Vec<PendingEdit>,
) {
    if matches!(editor.doc.mode, DocMode::Instance(_)) {
        return;
    }
    let wired: Vec<(String, String)> = node
        .input_slots()
        .into_iter()
        .filter_map(|(slot, from)| from.map(|f| (slot, f.to_string())))
        .collect();
    if wired.is_empty() {
        return;
    }
    let positions = editor.doc.positions();
    ui.horizontal_wrapped(|ui| {
        ui.weak("reroute:");
        for (slot, from) in wired {
            if ui.small_button(format!("+ {slot}")).clicked() {
                let a = positions.get(&from).copied().unwrap_or([0.0, 0.0]);
                let b = positions.get(name).copied().unwrap_or([0.0, 0.0]);
                let mut points = editor
                    .doc
                    .notes
                    .reroutes
                    .iter()
                    .find(|r| r.to == name && r.slot == slot)
                    .map(|r| r.points.clone())
                    .unwrap_or_default();
                points.push([(a[0] + b[0]) * 0.5 + 100.0, (a[1] + b[1]) * 0.5 + 40.0]);
                let to = name.to_string();
                pending.push(PendingEdit::new("reroute", None, move |d| {
                    d.set_reroute(&to, &slot, points)
                }));
            }
        }
    });
}

fn declarations_ui(
    ui: &mut egui::Ui,
    editor: &mut MatGraphEditor,
    diag: &Diagnostics,
    pending: &mut Vec<PendingEdit>,
) {
    let graph_revision = editor.graph_revision();
    let reload = editor
        .ui
        .decl_text
        .as_ref()
        .is_none_or(|(rev, _)| *rev != graph_revision);
    if reload {
        let pretty = regia_core::ron_pretty();
        let g = &editor.doc.graph;
        let text = |r: Result<String, ron::Error>| r.unwrap_or_default();
        editor.ui.decl_text = Some((
            graph_revision,
            [
                text(ron::ser::to_string_pretty(&g.params, pretty.clone())),
                text(ron::ser::to_string_pretty(&g.textures, pretty.clone())),
                text(ron::ser::to_string_pretty(&g.switches, pretty.clone())),
                text(ron::ser::to_string_pretty(&g.shading, pretty)),
            ],
        ));
    }
    let Some((_, texts)) = editor.ui.decl_text.as_mut() else {
        return;
    };
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let labels = ["Parameters", "Textures", "Switches", "Shading"];
    // Parameters, textures and switches are the typed Parameters panel's (w12); the shading
    // block keeps its RON editor.
    for (index, label) in labels.into_iter().enumerate().skip(3) {
        let errors: Vec<&str> = diag
            .errors
            .iter()
            .filter(|e| {
                matches!(
                    (&e.target, index),
                    (Target::Param(_), 0) | (Target::Texture(_), 1) | (Target::Switch(_), 2)
                )
            })
            .map(|e| e.message.as_str())
            .collect();
        let title = if errors.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({} error(s))", errors.len())
        };
        egui::CollapsingHeader::new(title)
            .id_salt(("matgraph-decl", index))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut texts[index])
                        .code_editor()
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                for e in &errors {
                    ui.colored_label(accents.error_text, *e);
                }
                if ui.button("Apply").clicked() {
                    let text = texts[index].clone();
                    let edit = match index {
                        0 => ron::from_str::<Vec<regia_matgraph::ParamDecl>>(&text).map(|v| {
                            PendingEdit::new("parameters", None, move |d| {
                                d.graph.params = v;
                                Ok(())
                            })
                        }),
                        1 => ron::from_str::<Vec<regia_matgraph::TextureDecl>>(&text).map(|v| {
                            PendingEdit::new("textures", None, move |d| {
                                d.graph.textures = v;
                                Ok(())
                            })
                        }),
                        2 => ron::from_str::<Vec<regia_matgraph::SwitchDecl>>(&text).map(|v| {
                            PendingEdit::new("switches", None, move |d| {
                                d.graph.switches = v;
                                Ok(())
                            })
                        }),
                        _ => ron::from_str::<regia_matgraph::Shading>(&text).map(|v| {
                            PendingEdit::new("shading", None, move |d| {
                                d.graph.shading = v;
                                Ok(())
                            })
                        }),
                    };
                    match edit {
                        Ok(edit) => pending.push(edit),
                        Err(err) => editor.toast = format!("{label} RON: {err}"),
                    }
                }
            });
    }
}

/// Counts the open graph under the shipped shader-permutation policy (ADR-2026,
/// [`super::model::policy_cost`]) when asked: it compiles every switch permutation, too much to
/// repeat each frame. One line per render profile, as the cook report writes it.
pub fn policy_lines(editor: &MatGraphEditor) -> Result<Vec<String>, String> {
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_policy").entered();
    let graph = super::model::resolved_live(
        &editor.doc.view_graph(),
        editor.graph_dir().as_deref(),
        editor.root.as_deref(),
        &editor.live_functions(),
    )?;
    let cost = super::model::policy_cost(&graph).map_err(|e| e.to_string())?;
    let mut lines = vec![format!(
        "{} permutation(s), {} counted, {} distinct code(s)",
        cost.permutations, cost.enumerated, cost.distinct_codes
    )];
    lines.extend(cost.profiles.iter().map(|p| p.summary_line()));
    Ok(lines)
}

/// The shader statistics of every profile (w12, [`super::cost::cost_table`]), one line each.
pub fn cost_lines(editor: &MatGraphEditor) -> Result<Vec<String>, String> {
    let graph = super::model::resolved_live(
        &editor.doc.view_graph(),
        editor.graph_dir().as_deref(),
        editor.root.as_deref(),
        &editor.live_functions(),
    )?;
    Ok(super::cost::cost_table(&graph)?
        .iter()
        .map(super::cost::stats_line)
        .collect())
}

fn policy_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor) {
    egui::CollapsingHeader::new("Permutation policy")
        .default_open(false)
        .show(ui, |ui| {
            let revision = editor.graph_revision();
            if ui
                .button("Count")
                .on_hover_text("Pipelines and cold compile time per render profile, against its budget")
                .clicked()
            {
                editor.ui.policy = Some((
                    revision,
                    policy_lines(editor).and_then(|mut lines| {
                        lines.extend(cost_lines(editor)?);
                        Ok(lines)
                    }),
                ));
            }
            let accents = crate::theme::ui_tokens(ui.ctx()).accents;
            match &editor.ui.policy {
                Some((counted, Ok(lines))) => {
                    if *counted != revision {
                        ui.weak("The graph changed since this count.");
                    }
                    for line in lines {
                        ui.label(line);
                    }
                }
                Some((_, Err(message))) => {
                    ui.colored_label(accents.error_text, message);
                }
                None => {}
            }
        });
}

fn errors_ui(ui: &mut egui::Ui, editor: &mut MatGraphEditor, diag: &Diagnostics) {
    ui.strong("Errors");
    if diag.errors.is_empty() {
        ui.weak("None.");
        return;
    }
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let by_target: BTreeMap<String, Vec<&str>> = diag.errors.iter().fold(BTreeMap::new(), |mut map, e| {
        map.entry(e.target.label()).or_default().push(e.message.as_str());
        map
    });
    for (label, messages) in by_target {
        for message in messages {
            let response = ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{label}: {message}")).color(accents.error_text),
                )
                .sense(egui::Sense::click()),
            );
            if response.clicked()
                && let Some(node) = label.strip_prefix("node:")
            {
                editor.ui.selected = Some(node.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a11y_audit::{unnamed_widgets, widget_names};
    use regia_matgraph::SurfaceOutputs;

    fn doc() -> MatGraphDoc {
        let graph = regia_matgraph::MaterialGraph::from_ron(
            r#"(
                format_version: 2,
                functions: {
                    "f": Inline((
                        inputs: [(name: "c", ty: Vec3), (name: "k", ty: Float)],
                        nodes: { "c": Input("c"), "k": Input("k"), "o": Mul("c", "k") },
                        outputs: { "out": "o" },
                    )),
                },
                nodes: {
                    "v": Vec3((1.0, 0.5, 0.2)),
                    "call": Call(function: "f", args: { "c": "v" }),
                    "lerp": Lerp(a: "v", b: "call", t: ""),
                },
                output: (base_color: Some("lerp"), emissive: Some("v")),
            )"#,
        )
        .expect("parses");
        MatGraphDoc::new(graph, BTreeMap::new())
    }

    #[test]
    fn call_slots_come_from_the_inline_function() {
        let d = doc();
        assert_eq!(slot_names(&d, &d.graph.nodes["call"]), ["c", "k"]);
        assert_eq!(slot_names(&d, &d.graph.nodes["lerp"]), ["a", "b", "t"]);
    }

    #[test]
    fn the_canvas_has_every_node_edge_and_output() {
        let d = doc();
        let snarl = build_snarl(&d, None);
        let nodes: Vec<CanvasNode> = snarl.nodes().cloned().collect();
        assert_eq!(nodes.len(), 4, "three nodes and the surface: {nodes:?}");
        // v→call.c, v→lerp.a, call→lerp.b, lerp→base_color, v→emissive; lerp.t is unwired.
        assert_eq!(snarl.wires().count(), 5);
        let surface = snarl
            .node_ids()
            .find(|(_, n)| **n == CanvasNode::Surface)
            .map(|(id, _)| id)
            .expect("surface");
        let emissive = SurfaceOutputs::SLOTS
            .iter()
            .position(|s| *s == "emissive")
            .expect("slot");
        assert!(
            snarl
                .wires()
                .any(|(_, to)| to.node == surface && to.input == emissive),
            "emissive wired"
        );
    }

    /// The panel draws headless (no GPU device) over an open graph: the canvas is built from
    /// the document, drawing twice makes no edit, and the selection survives.
    #[test]
    fn the_panel_draws_headless_without_editing() {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor.doc = doc();
        editor.revision += 1;
        editor.ui.selected = Some("lerp".into());
        world.insert_resource(editor);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| matgraph_editor_panel(ui, &mut world, None));
            output.textures_delta.clear();
        }
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(editor.doc, doc(), "drawing does not edit the document");
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
        assert!(editor.ui.snarl.is_some(), "the canvas was built");
        assert_eq!(editor.ui.selected.as_deref(), Some("lerp"));
        assert!(editor.ui.preview.is_none(), "no device, no preview");
    }

    /// The agent and the panel share one document: a graph built over the protocol (open,
    /// add, connect, output, compile, save) is what the headless panel draws, drawing it makes
    /// no edit, and the saved file reopens to the same graph and pipeline key.
    #[test]
    fn a_graph_built_over_the_agent_draws_in_the_panel_and_saves() {
        use serde_json::{Value, json};

        fn call(world: &mut World, method: &str, params: Value) -> Value {
            let (_, handler) = super::super::agent::methods()
                .into_iter()
                .find(|(s, _)| s.name == method)
                .unwrap_or_else(|| panic!("no method {method}"));
            handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
        }

        let dir = regia_test_utils::unique_temp_dir("matgraph-editor-panel-agent");
        let path = dir.join("noise.matgraph.ron");
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        call(
            &mut world,
            "editor.matgraph_open",
            json!({ "create": true, "path": path.display().to_string() }),
        );
        for (name, node) in [
            ("uv", json!("TexCoord")),
            ("tint", json!("Vec3((0.9, 0.4, 0.2))")),
        ] {
            call(
                &mut world,
                "editor.matgraph_edit",
                json!({ "op": "add", "name": name, "node": node }),
            );
        }
        for (name, kind) in [("n", "Noise"), ("base", "Mul")] {
            call(
                &mut world,
                "editor.matgraph_edit",
                json!({ "op": "add", "name": name, "kind": kind }),
            );
        }
        for (from, to, slot) in [("uv", "n", "input"), ("tint", "base", "a"), ("n", "base", "b")] {
            call(
                &mut world,
                "editor.matgraph_edit",
                json!({ "op": "connect", "from": from, "to": to, "slot": slot }),
            );
        }
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": "base" }),
        );
        let compiled = call(&mut world, "editor.matgraph_compile", json!({}));
        assert_eq!(compiled["ok"], true, "{compiled}");
        let key = compiled["key"].clone();
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        assert_eq!(depth, 8, "every edit is one undo step");

        // The panel draws the agent's document headless.
        let before = world.resource::<MatGraphEditor>().doc.clone();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| matgraph_editor_panel(ui, &mut world, None));
            output.textures_delta.clear();
        }
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(editor.doc, before, "drawing does not edit the document");
        let snarl = editor.ui.snarl.as_ref().expect("the canvas was built");
        assert_eq!(snarl.nodes().count(), 5, "four nodes and the surface");
        // uv→n, tint→base.a, n→base.b, base→base_color.
        assert_eq!(snarl.wires().count(), 4);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), depth);

        // Save, then reopen in a fresh world: same graph, same key.
        let saved = call(&mut world, "editor.matgraph_save", json!({}));
        assert_eq!(saved["ok"], true, "{saved}");
        let mut other = World::new();
        other.insert_resource(crate::command::UndoStack::new());
        let opened = call(
            &mut other,
            "editor.matgraph_open",
            json!({ "path": path.display().to_string() }),
        );
        assert_eq!(opened["nodes"], 4);
        assert_eq!(other.resource::<MatGraphEditor>().doc.graph, before.graph);
        let again = call(&mut other, "editor.matgraph_compile", json!({}));
        assert_eq!(again["key"], key);
        // The panel's policy count is the shipped cook-report line per profile.
        let lines = policy_lines(other.resource::<MatGraphEditor>()).expect("counts");
        assert_eq!(lines[0], "1 permutation(s), 1 counted, 1 distinct code(s)");
        assert_eq!(lines.len(), 1 + super::super::model::policy_presets().len());
        assert!(lines[1..].iter().all(|l| l.contains("1 graph sets")), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn agent_call(world: &mut World, method: &str, params: serde_json::Value) -> serde_json::Value {
        let (_, handler) = super::super::agent::methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
    }

    /// FU-5 (ADR-7383): the canvas is named with its node count, the panel has no unnamed
    /// widget, and the Nodes list adds a node wired after the focused one (Ctrl+Shift+A, filter,
    /// Enter), moves it by Alt+Right (one undo entry), and drives an output through Connect…, with
    /// keys only. The saved graph and layout equal the ones the agent methods build.
    #[test]
    fn the_nodes_list_builds_by_keyboard_what_the_agent_builds() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to, text};
        use egui::{Key, Modifiers};
        use serde_json::json;

        const SIZE: [f32; 2] = [1400.0, 900.0];
        let dir = regia_test_utils::unique_temp_dir("matgraph-canvas-keys");
        let open = |world: &mut World, file: &str| {
            world.insert_resource(crate::command::UndoStack::new());
            // The same file name in two folders: the graph's name follows its file name.
            let folder = dir.join(file);
            std::fs::create_dir_all(&folder).expect("folder");
            let path = folder.join("g.matgraph.ron").display().to_string();
            agent_call(
                world,
                "editor.matgraph_open",
                json!({ "create": true, "path": path }),
            );
            agent_call(
                world,
                "editor.matgraph_edit",
                json!({ "op": "add", "name": "uv", "node": "TexCoord", "pos": [0.0, 0.0] }),
            );
        };

        // The agent's graph.
        let mut agent = World::new();
        open(&mut agent, "agent");
        let added = agent_call(
            &mut agent,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Noise", "pos": [220.0, 0.0] }),
        );
        let noise = added["name"].as_str().expect("name").to_string();
        for params in [
            json!({ "op": "connect", "from": "uv", "to": noise, "slot": "input" }),
            json!({ "op": "move", "name": noise, "pos": [236.0, 0.0] }),
            json!({ "op": "output", "slot": "base_color", "from": noise }),
        ] {
            agent_call(&mut agent, "editor.matgraph_edit", params);
        }

        // The same by keyboard.
        let mut world = World::new();
        open(&mut world, "keys");
        let toast = world.resource::<MatGraphEditor>().toast.clone();
        let ctx = egui::Context::default();
        let draw = |world: &mut World, events: Vec<egui::Event>| {
            frame(&ctx, SIZE, events, |ui| matgraph_editor_panel(ui, world, None))
        };
        let first = draw(&mut world, vec![]);
        assert_eq!(
            unnamed_widgets(&first),
            Vec::<String>::new(),
            "{:?}",
            widget_names(&first)
        );
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Material graph canvas, 1 nodes".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        tab_to(&ctx, SIZE, "uv", false, 400, |ui| {
            matgraph_editor_panel(ui, &mut world, None)
        });
        let shortcut = Modifiers::COMMAND | Modifiers::SHIFT;
        draw(&mut world, vec![key(Key::A, shortcut)]);
        let palette = draw(&mut world, vec![]);
        let filter = palette
            .nodes
            .iter()
            .find(|(id, _)| *id == palette.focus)
            .map(|(_, n)| (n.role(), n.labelled_by().len()));
        assert_eq!(
            filter,
            Some((egui::accesskit::Role::TextInput, 1)),
            "the palette filter has the focus and a label"
        );
        draw(&mut world, vec![text("noi")]);
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1,
            "add and wire is one undo entry"
        );
        draw(&mut world, vec![]);
        let back = draw(&mut world, vec![]);
        assert_eq!(
            focused_name(&back).as_deref(),
            Some("uv"),
            "the focus came back to uv"
        );
        draw(&mut world, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        let up = draw(&mut world, vec![]);
        assert_eq!(focused_name(&up).as_deref(), Some(noise.as_str()));
        draw(&mut world, vec![key(Key::ArrowRight, Modifiers::ALT)]);
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 2,
            "one move entry per key press"
        );
        tab_to(&ctx, SIZE, "Connect…", true, 10, |ui| {
            matgraph_editor_panel(ui, &mut world, None)
        });
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        draw(&mut world, vec![text("Surface")]);
        draw(&mut world, vec![key(Key::Tab, Modifiers::NONE)]);
        draw(&mut world, vec![text("base_color")]);
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        let done = draw(&mut world, vec![]);
        assert_eq!(
            world.resource::<MatGraphEditor>().toast,
            toast,
            "no edit was refused (a refusal replaces the toast)"
        );
        assert!(
            labels_of(&done, egui::accesskit::Role::Canvas)
                .contains(&"Material graph canvas, 2 nodes".to_string())
        );

        let saved_keys = agent_call(&mut world, "editor.matgraph_save", json!({}));
        let saved_agent = agent_call(&mut agent, "editor.matgraph_save", json!({}));
        let read = |v: &serde_json::Value, field: &str| {
            std::fs::read_to_string(v[field].as_str().expect("path")).expect("saved file")
        };
        assert_eq!(
            read(&saved_keys, "path"),
            read(&saved_agent, "path"),
            "same graph"
        );
        assert_eq!(
            read(&saved_keys, "layout"),
            read(&saved_agent, "layout"),
            "same layout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Enter on a row selects the node and fits the canvas view on it (a fresh canvas id).
    #[test]
    fn enter_on_a_row_selects_and_frames_the_node() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        use egui::{Key, Modifiers};
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor.doc = doc();
        editor.revision += 1;
        world.insert_resource(editor);
        let ctx = egui::Context::default();
        frame(&ctx, [1400.0, 900.0], vec![], |ui| {
            matgraph_editor_panel(ui, &mut world, None)
        });
        tab_to(&ctx, [1400.0, 900.0], "lerp", false, 400, |ui| {
            matgraph_editor_panel(ui, &mut world, None);
        });
        frame(
            &ctx,
            [1400.0, 900.0],
            vec![key(Key::Enter, Modifiers::NONE)],
            |ui| {
                matgraph_editor_panel(ui, &mut world, None);
            },
        );
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(editor.ui.selected.as_deref(), Some("lerp"));
        assert_eq!(editor.ui.keys.frame_epoch, 1, "the canvas refits once");
        assert!(editor.ui.keys.frame.is_none(), "the framing draw happened");
        assert_eq!(editor.doc, doc(), "selecting edits nothing");
    }

    #[test]
    fn the_palette_lists_kinds_that_take_an_input() {
        let all = palette_kinds(false, "");
        assert!(all.contains(&"Noise".to_string()) && all.contains(&"Mul".to_string()));
        assert!(!all.contains(&"TexCoord".to_string()), "nothing to wire into");
        assert!(!all.contains(&"Input".to_string()), "graphs have no Input node");
        assert_eq!(palette_kinds(false, "NOIS"), ["Noise"]);
    }

    #[test]
    fn new_and_open_warn_once_before_dropping_unsaved_work() {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        world.insert_resource(editor);

        // An empty new graph has nothing to lose.
        perform(&mut world, Action::New);
        assert_eq!(world.resource::<MatGraphEditor>().toast, "new graph");

        let add = |world: &mut World, name: &'static str| {
            super::super::edit(world, "add", None, move |d| {
                d.add_node(Some(name), Node::Time, None).map(|_| ())
            })
            .expect("add");
        };
        add(&mut world, "t");
        perform(&mut world, Action::New);
        let editor = world.resource::<MatGraphEditor>();
        assert!(
            editor.doc.graph.nodes.contains_key("t"),
            "the first press keeps the work"
        );
        assert!(editor.toast.contains("unsaved"), "{}", editor.toast);

        // An edit takes the warning back.
        add(&mut world, "u");
        perform(&mut world, Action::New);
        assert!(
            world
                .resource::<MatGraphEditor>()
                .doc
                .graph
                .nodes
                .contains_key("u")
        );

        perform(&mut world, Action::New);
        assert!(world.resource::<MatGraphEditor>().doc.graph.nodes.is_empty());
    }

    #[test]
    fn summaries_show_constants_and_names() {
        assert_eq!(
            node_summary(&Node::Param("tint".into())).as_deref(),
            Some("param tint")
        );
        assert_eq!(node_summary(&Node::Float(0.5)).as_deref(), Some("0.5"));
        assert_eq!(node_summary(&Node::Time), None);
    }
    fn draw_twice(world: &mut World) {
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| matgraph_editor_panel(ui, world, None));
            output.textures_delta.clear();
        }
    }

    /// ADR-2490: a function tab draws an Outputs sink with its named outputs, an instance tab
    /// draws its parent read-only; drawing edits neither, and both tabs stay open.
    #[test]
    fn function_and_instance_tabs_draw_headless_without_editing() {
        let dir = regia_test_utils::unique_temp_dir("matgraph-editor-panel-tabs");
        let parent = dir.join("p.matgraph.ron");
        std::fs::write(&parent, doc().graph.to_ron().expect("ron")).expect("write");
        let instance = regia_matgraph::MaterialInstance::new("p.matgraph.ron");
        let inst = dir.join("i.matinst.ron");
        std::fs::write(&inst, instance.to_ron().expect("ron")).expect("write");

        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_function(Some(dir.join("f.matfunc.ron")), None);
        editor.open_tab();
        editor.open_instance(&inst, &parent).expect("instance");
        world.insert_resource(editor);
        draw_twice(&mut world);
        {
            let editor = world.resource::<MatGraphEditor>();
            assert_eq!(super::super::doc_kind(&editor.doc), "instance");
            let snarl = editor.ui.snarl.as_ref().expect("canvas");
            assert_eq!(
                snarl.nodes().count(),
                4,
                "the parent's three nodes and the surface"
            );
            assert!(!editor.dirty(), "drawing does not edit the instance");
        }
        world.resource_mut::<MatGraphEditor>().select_tab(0).expect("tab");
        draw_twice(&mut world);
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(super::super::doc_kind(&editor.doc), "function");
        let snarl = editor.ui.snarl.as_ref().expect("canvas");
        // Input `x` and the Outputs sink with `out` wired from it.
        assert_eq!(snarl.nodes().count(), 2);
        assert_eq!(snarl.wires().count(), 1);
        assert_eq!(editor.tabs().len(), 2);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stage 2 (ADR-9700): the headless panel drives open, search-add, a typed edit, copy and
    /// paste, apply to selection and save through the actions its buttons and keys push.
    #[test]
    fn the_panel_drives_search_add_typed_edit_paste_apply_and_save() {
        let root = regia_test_utils::unique_temp_dir("matgraph-editor-panel-s2");
        std::fs::create_dir_all(root.join("assets/mat")).expect("assets");
        let (mut world, owner) = super::super::s2_tests::level_world(&root);
        world.insert_resource(MatGraphEditor::default());
        let path = root.join("assets/mat/red.matgraph.ron");
        let graph = super::super::s2_tests::red_graph();
        std::fs::write(&path, graph.to_ron().expect("ron")).expect("graph");
        perform(&mut world, Action::Open("assets/mat/red.matgraph.ron".into()));
        draw_twice(&mut world);
        perform(
            &mut world,
            Action::SearchAdd {
                kind: "Float".into(),
                pos: [0.0, 300.0],
                from: None,
            },
        );
        let added = world
            .resource::<MatGraphEditor>()
            .ui
            .selected
            .clone()
            .expect("the new node is selected");
        perform(
            &mut world,
            Action::SetField {
                node: added.clone(),
                field: "value",
                value: super::super::inspector::FieldValue::Float(0.3),
            },
        );
        perform(&mut world, Action::Copy);
        perform(&mut world, Action::Paste([200.0, 300.0]));
        draw_twice(&mut world);
        perform(&mut world, Action::ApplyToSelection);
        perform(&mut world, Action::Save(None));
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(
            editor.doc.graph.nodes[&added],
            Node::Float(0.3),
            "{}",
            editor.toast
        );
        let floats = editor
            .doc
            .graph
            .nodes
            .values()
            .filter(|n| **n == Node::Float(0.3))
            .count();
        assert_eq!(floats, 2, "the copy was pasted under a new name");
        assert!(!editor.dirty(), "saved: {}", editor.toast);
        let on_disk = regia_matgraph::MaterialGraph::from_ron(&std::fs::read_to_string(&path).expect("read"))
            .expect("parses");
        assert_eq!(on_disk, editor.doc.graph);
        assert_eq!(
            world
                .get::<regia_render::MeshSource>(owner)
                .map(|m| m.material.as_str()),
            Some("assets/mat/red.matgraph.ron")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- w5 canvas finish (ADR-9970): reroutes, comment frames ----

    /// One headless frame at time `t` with these input events.
    fn frame_with(ctx: &egui::Context, world: &mut World, t: f64, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            time: Some(t),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| matgraph_editor_panel(ui, world, None));
        output.textures_delta.clear();
    }

    /// Drags with the primary button from graph point `from` by graph `delta`, through the
    /// canvas transform the panel last drew with: hover, press, four moves, release.
    fn drag_on_canvas(ctx: &egui::Context, world: &mut World, t: &mut f64, from: [f32; 2], delta: [f32; 2]) {
        let to_global = world
            .resource::<MatGraphEditor>()
            .ui
            .to_global
            .expect("the canvas was drawn");
        let a = to_global * egui::pos2(from[0], from[1]);
        let b = to_global * egui::pos2(from[0] + delta[0], from[1] + delta[1]);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0));
        assert!(
            screen.contains(a) && screen.contains(b),
            "the drag {a:?} → {b:?} must stay on the screen (canvas {to_global:?})"
        );
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut steps = vec![vec![egui::Event::PointerMoved(a)], vec![button(a, true)]];
        for k in 1..=4 {
            steps.push(vec![egui::Event::PointerMoved(a + (b - a) * (k as f32 / 4.0))]);
        }
        steps.push(vec![button(b, false)]);
        steps.push(Vec::new());
        for events in steps {
            *t += 0.05;
            frame_with(ctx, world, *t, events);
        }
    }

    /// `doc()` laid out: `v` and `call` inside a comment frame, `lerp` outside it.
    fn framed_doc() -> MatGraphDoc {
        let mut d = doc();
        d.move_node("v", [0.0, 0.0]).expect("v");
        d.move_node("call", [0.0, 160.0]).expect("call");
        d.move_node("lerp", [420.0, 40.0]).expect("lerp");
        d.add_comment("inputs", [-30.0, -60.0], [260.0, 320.0])
            .expect("comment");
        d
    }

    fn world_with(d: MatGraphDoc) -> World {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        editor.doc = d;
        editor.revision += 1;
        world.insert_resource(editor);
        world
    }

    /// A rerouted pin pair is not a snarl wire: the canvas draws it through its points itself.
    #[test]
    fn a_rerouted_pin_pair_has_no_snarl_wire() {
        let mut d = doc();
        d.set_reroute("lerp", "a", vec![[120.0, -80.0], [200.0, -60.0]])
            .expect("reroute");
        let snarl = build_snarl(&d, None);
        let lerp = snarl
            .node_ids()
            .find(|(_, n)| **n == CanvasNode::Node("lerp".into()))
            .map(|(id, _)| id)
            .expect("lerp");
        assert!(
            !snarl.wires().any(|(_, to)| to.node == lerp && to.input == 0),
            "v→lerp.a is rerouted: snarl must not draw it"
        );
        // The other four wires stay snarl's.
        assert_eq!(snarl.wires().count(), 4);
    }

    /// A headless draw counts one rerouted wire, drawn through both its points, and the
    /// canvas's snarl has no wire for that pin pair.
    #[test]
    fn the_panel_draws_a_rerouted_wire_through_its_points() {
        let mut d = framed_doc();
        let points = [[300.0, -140.0], [360.0, -100.0]];
        d.set_reroute("lerp", "a", points.to_vec()).expect("reroute");
        let mut world = world_with(d);
        draw_twice(&mut world);
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(editor.ui.reroute_paths.len(), 1, "one rerouted wire drawn");
        let path = &editor.ui.reroute_paths[0];
        for p in points {
            assert!(path.contains(&egui::pos2(p[0], p[1])), "through {p:?}");
        }
        let snarl = editor.ui.snarl.as_ref().expect("canvas");
        let lerp = snarl
            .node_ids()
            .find(|(_, n)| **n == CanvasNode::Node("lerp".into()))
            .map(|(id, _)| id)
            .expect("lerp");
        assert!(!snarl.wires().any(|(_, to)| to.node == lerp && to.input == 0));
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
    }

    /// Dragging a comment frame's header by (40, 0) moves the frame and the two nodes inside
    /// it by (40, 0) in one undo step; the node outside stays; undo restores both.
    #[test]
    fn dragging_a_comment_header_moves_the_frame_and_its_nodes() {
        let mut world = world_with(framed_doc());
        let ctx = egui::Context::default();
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        drag_on_canvas(&ctx, &mut world, &mut t, [100.0, -48.0], [40.0, 0.0]);
        let editor = world.resource::<MatGraphEditor>();
        let comment = &editor.doc.notes.comments[0];
        let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01;
        assert!(
            close(comment.min, [10.0, -60.0]) && close(comment.max, [300.0, 320.0]),
            "the frame moved by (40, 0): {comment:?} ({})",
            editor.toast
        );
        assert!(
            close(editor.doc.layout["v"], [40.0, 0.0]),
            "{:?}",
            editor.doc.layout
        );
        assert!(
            close(editor.doc.layout["call"], [40.0, 160.0]),
            "{:?}",
            editor.doc.layout
        );
        assert!(
            close(editor.doc.layout["lerp"], [420.0, 40.0]),
            "outside the frame"
        );
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1,
            "one undo step per drag"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(
            world.resource::<MatGraphEditor>().doc,
            framed_doc(),
            "undo restores both"
        );
    }

    /// Double-clicking a comment frame's header opens its text on the canvas; typing and Enter
    /// keep the new text as one undo step.
    #[test]
    fn double_clicking_a_comment_header_edits_its_text() {
        let mut world = world_with(framed_doc());
        let ctx = egui::Context::default();
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        let to_global = world
            .resource::<MatGraphEditor>()
            .ui
            .to_global
            .expect("the canvas was drawn");
        let at = to_global * egui::pos2(100.0, -48.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let steps = vec![
            vec![egui::Event::PointerMoved(at)],
            vec![button(true)],
            vec![button(false)],
            vec![button(true)],
            vec![button(false)],
            Vec::new(),
            vec![egui::Event::Text("!".into())],
            vec![enter],
            Vec::new(),
        ];
        for events in steps {
            t += 0.05;
            frame_with(&ctx, &mut world, t, events);
        }
        let editor = world.resource::<MatGraphEditor>();
        let text = &editor.doc.notes.comments[0].text;
        assert!(
            text.len() == "inputs!".len() && text.contains('!') && text.contains("inputs"),
            "typed into the frame's text: {text:?} ({})",
            editor.toast
        );
        assert!(editor.ui.comment_text.is_none(), "Enter closes the field");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1
        );
    }

    /// A reroute point dragged on the canvas and a comment frame dragged by a corner survive a
    /// save and a reopen.
    #[test]
    fn a_dragged_reroute_point_and_comment_survive_save_and_reopen() {
        let dir = regia_test_utils::unique_temp_dir("matgraph-canvas-reopen");
        let path = dir.join("framed.matgraph.ron");
        std::fs::write(&path, framed_doc().graph.to_ron().expect("ron")).expect("write");
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.open(&path).expect("open");
        world.insert_resource(editor);
        super::super::edit(&mut world, "lay out", None, |d| {
            let framed = framed_doc();
            d.layout = framed.layout;
            d.notes = framed.notes;
            d.set_reroute("lerp", "a", vec![[300.0, -140.0], [360.0, -140.0]])
        })
        .expect("layout");
        let ctx = egui::Context::default();
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        // The reroute's first point, then the frame's top-right corner (the canvas fits this
        // graph at about 1.6x: the bottom corners are off the 800-point screen).
        drag_on_canvas(&ctx, &mut world, &mut t, [300.0, -140.0], [30.0, 20.0]);
        drag_on_canvas(&ctx, &mut world, &mut t, [260.0, -60.0], [20.0, -40.0]);
        perform(&mut world, Action::Save(None));
        let saved = world.resource::<MatGraphEditor>().doc.notes.clone();
        let close = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01;
        assert!(
            close(saved.reroutes[0].points[0], [330.0, -120.0]),
            "the point moved: {saved:?} ({})",
            world.resource::<MatGraphEditor>().toast
        );
        assert!(close(saved.reroutes[0].points[1], [360.0, -140.0]));
        assert!(
            close(saved.comments[0].min, [-30.0, -100.0]) && close(saved.comments[0].max, [280.0, 320.0]),
            "the corner resized the frame: {saved:?}"
        );
        let mut reopened = MatGraphEditor::default();
        reopened.open(&path).expect("reopen");
        assert_eq!(reopened.doc.notes, saved, "the sidecar keeps both");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- w11 wire clicks (spec 80 § 12.1) ----

    /// The drawn wire into `to.slot` and its screen transform, after a few frames.
    fn drawn_wire(
        world: &World,
        to: &str,
        slot: &str,
    ) -> (super::super::wire_hit::WireShape, egui::emath::TSTransform) {
        let editor = world.resource::<MatGraphEditor>();
        let wire = editor
            .ui
            .wires
            .iter()
            .find(|w| w.to == to && w.slot == slot)
            .unwrap_or_else(|| panic!("the wire into {to}.{slot} was drawn: {:?}", editor.ui.wires));
        (
            wire.shape.clone(),
            editor.ui.to_global.expect("the canvas was drawn"),
        )
    }

    /// One click (press and release at `at`, screen space) with `button`.
    fn click_at(
        ctx: &egui::Context,
        world: &mut World,
        t: &mut f64,
        at: egui::Pos2,
        button: egui::PointerButton,
    ) {
        let event = |pressed| egui::Event::PointerButton {
            pos: at,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![event(true)],
            vec![event(false)],
            Vec::new(),
        ] {
            *t += 0.05;
            frame_with(ctx, world, *t, events);
        }
    }

    /// A click on a plain wire adds one reroute point there (one undo step; undo removes it);
    /// the wire is then drawn through the point.
    #[test]
    fn clicking_a_plain_wire_inserts_a_reroute_point() {
        use super::super::wire_hit::{WireShape, bezier_point};
        let mut world = world_with(framed_doc());
        let ctx = egui::Context::default();
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        let (shape, to_global) = drawn_wire(&world, "lerp", "a");
        let WireShape::Plain(curve) = shape else {
            panic!("v → lerp.a is a plain wire: {shape:?}");
        };
        // A point of the curve clear of the nodes (and their pins) and of the comment frame's
        // grips (the frame ends at x = 260).
        let mid = {
            let editor = world.resource::<MatGraphEditor>();
            (4..18)
                .map(|k| bezier_point(&curve, k as f32 / 20.0))
                .find(|p| p.x > 275.0 && !editor.ui.node_rects.iter().any(|r| r.expand(14.0).contains(*p)))
                .unwrap_or_else(|| panic!("a clear point on {curve:?}: nodes {:?}", editor.ui.node_rects))
        };
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        click_at(
            &ctx,
            &mut world,
            &mut t,
            to_global * mid,
            egui::PointerButton::Primary,
        );
        let editor = world.resource::<MatGraphEditor>();
        let reroute = editor
            .doc
            .notes
            .reroutes
            .iter()
            .find(|r| r.to == "lerp" && r.slot == "a")
            .unwrap_or_else(|| panic!("a point was added: {:?} ({})", editor.doc.notes, editor.toast));
        assert_eq!(reroute.points.len(), 1);
        let p = reroute.points[0];
        assert!(
            (egui::pos2(p[0], p[1]) - mid).length() < 1.5,
            "the point is where the click was: {p:?} vs {mid:?}"
        );
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1,
            "one undo step"
        );
        assert!(
            matches!(drawn_wire(&world, "lerp", "a").0, WireShape::Rerouted(_)),
            "drawn through the point"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(
            world.resource::<MatGraphEditor>().doc,
            framed_doc(),
            "undo removes it"
        );
    }

    /// A click on a rerouted wire adds a point on the span it hit; a right-click on it
    /// disconnects the input (one undo step each; undo wires it again).
    #[test]
    fn clicking_and_right_clicking_a_rerouted_wire() {
        use super::super::wire_hit::WireShape;
        let mut d = framed_doc();
        d.set_reroute("lerp", "a", vec![[300.0, -140.0], [360.0, -100.0]])
            .expect("reroute");
        let mut world = world_with(d.clone());
        let ctx = egui::Context::default();
        let mut t = 0.0;
        for _ in 0..3 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        let (shape, to_global) = drawn_wire(&world, "lerp", "a");
        let WireShape::Rerouted(path) = shape else {
            panic!("lerp.a is rerouted: {shape:?}");
        };
        // Halfway between the two points: the middle span (after the first point).
        let between = path[canvas::REROUTE_SAMPLES + canvas::REROUTE_SAMPLES / 2];
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        click_at(
            &ctx,
            &mut world,
            &mut t,
            to_global * between,
            egui::PointerButton::Primary,
        );
        {
            let editor = world.resource::<MatGraphEditor>();
            let points = &editor.doc.notes.reroutes[0].points;
            assert_eq!(points.len(), 3, "{points:?} ({})", editor.toast);
            assert_eq!(points[0], [300.0, -140.0]);
            assert_eq!(points[2], [360.0, -100.0], "the new point went between the two");
        }
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1
        );
        crate::command::undo_world(&mut world);
        for _ in 0..2 {
            t += 0.05;
            frame_with(&ctx, &mut world, t, Vec::new());
        }
        let (shape, to_global) = drawn_wire(&world, "lerp", "a");
        let WireShape::Rerouted(path) = shape else {
            panic!("still rerouted: {shape:?}");
        };
        let between = path[canvas::REROUTE_SAMPLES + canvas::REROUTE_SAMPLES / 2];
        let nodes = world.resource::<MatGraphEditor>().doc.graph.nodes.len();
        click_at(
            &ctx,
            &mut world,
            &mut t,
            to_global * between,
            egui::PointerButton::Secondary,
        );
        let editor = world.resource::<MatGraphEditor>();
        assert_eq!(
            wired(&editor.doc.graph.nodes["lerp"], "a"),
            None,
            "the right-click unwired lerp.a ({})",
            editor.toast
        );
        assert_eq!(editor.doc.graph.nodes.len(), nodes, "no node menu add");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1,
            "one undo step"
        );
        crate::command::undo_world(&mut world);
        assert_eq!(world.resource::<MatGraphEditor>().doc, d, "undo wires it again");
    }

    /// FU-7: a warning in the header carries its meaning as a spoken name, not only in hover
    /// text ("⚠"): the graph with five static switches reports the permutation warning.
    #[test]
    fn a_header_warning_carries_its_text_as_a_spoken_name() {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = MatGraphEditor::default();
        editor.new_graph(None);
        for i in 0..5 {
            editor.doc.graph.switches.push(regia_matgraph::SwitchDecl {
                name: format!("s{i}"),
                default: false,
            });
        }
        let diag = editor.diagnostics();
        assert_eq!(diag.warnings.len(), 1, "{:?}", diag.warnings);
        let warning = diag.warnings[0].clone();
        world.insert_resource(editor);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| matgraph_editor_panel(ui, &mut world, None));
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        // The glyph is static text read in place (a label), never an operable button that does
        // nothing (ADR-8870, WCAG 4.1.2).
        let labels = crate::a11y_audit::label_names(&update);
        assert!(
            labels.iter().any(|name| name.contains(&warning)),
            "the warning text in the labels {labels:?}"
        );
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert!(
            !names.iter().any(|name| name.contains(&warning)),
            "the warning glyph is not announced as an interactive widget: {names:?}"
        );
    }
}
