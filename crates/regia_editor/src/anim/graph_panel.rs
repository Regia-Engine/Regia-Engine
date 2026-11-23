// SPDX-License-Identifier: MPL-2.0
//! The Anim Graph panel: toolbar (new/open/save, undo/redo, preview rig), parameter list with
//! preview controls, the blend-tree canvas (egui-snarl) or a state machine's canvas, the
//! selected item's inspector, and the live preview with PIE highlighting.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use egui_snarl::ui::{PinInfo, SnarlStyle, SnarlViewer, SnarlWidget};
use egui_snarl::{InPin, InPinId, OutPin, OutPinId, Snarl};
use regia_animation::debug::{NodeDebug, NodeDebugState};
use regia_animation::params::ParamValue;
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_render::RenderDevice;

use super::model::{GraphModel, NodeId, NodeKind, Scope};
use super::sm_canvas::{MachineActivity, machine_canvas};
use super::{AnimEditor, AnimUi, PendingEdit, PreviewRig, apply_pending};

/// A node on the blend-tree canvas: a model node, or the scope's output sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasNode {
    /// A model node.
    Node(NodeId),
    /// The scope's output.
    Output,
}

/// A non-document action requested by the panel (applied after drawing).
#[derive(Clone, Debug)]
pub enum PanelAction {
    /// Undo through the shared stack.
    Undo,
    /// Redo through the shared stack.
    Redo,
    /// Start a new graph.
    New,
    /// Open a graph file.
    Open(String),
    /// Save (to the open path, or to the given path).
    Save(Option<String>),
    /// Preview the built-in demo rig.
    DemoRig,
    /// Preview the selected entity's rig.
    SelectedRig,
    /// Preview a glTF file's rig.
    GltfRig(String),
    /// Save the rig's `.animedit.ron`.
    SaveEdits(Option<String>),
}

/// Resolves a user path: absolute as given, otherwise under the project's `assets/`.
#[must_use]
pub fn resolve_path(world: &World, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return p;
    }
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    match root {
        Some(root) => {
            let rel = path.strip_prefix("assets/").unwrap_or(path);
            root.join("assets").join(rel)
        }
        None => p,
    }
}

/// [`resolve_path`] for a path from an agent request (ADR-1816, ADR-7388): an absolute path must lie under
/// the project or the temp dir, a relative one under the project's `assets/` (the working directory
/// with no project open); `..` and symlinks leading out are `path_outside_project`.
///
/// # Errors
/// `path_outside_project`.
pub fn agent_path(world: &World, key: &str, path: &str) -> Result<PathBuf, regia_agent::protocol::RpcError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    match root {
        Some(root) if !std::path::Path::new(path).is_absolute() => {
            let rel = path.strip_prefix("assets/").unwrap_or(path);
            regia_agent::req_path::project_path_in(&root, key, &format!("assets/{rel}"))
        }
        _ => crate::agent_ext::document_request_path(world, key, path),
    }
}

/// The selected entity's preview rig (`SkinnedMesh` + graph `Animator` clips), if any.
#[must_use]
pub fn entity_rig(world: &World, entity: Entity) -> Option<PreviewRig> {
    let binding = world.get::<regia_animation::SkinnedMesh>(entity)?;
    let clips = match world.get::<regia_animation::Animator>(entity).map(|a| &a.source) {
        Some(regia_animation::AnimatorSource::Graph { clips, .. }) => {
            clips.names().filter_map(|n| clips.get(n).cloned()).collect()
        }
        Some(regia_animation::AnimatorSource::SingleClip { clip, .. }) => vec![clip.clone()],
        None => Vec::new(),
    };
    Some(PreviewRig {
        name: crate::outliner::display_name(world, entity),
        source: super::RigSource::Entity(entity.to_bits()),
        skeleton: binding.skeleton.clone(),
        clips,
    })
}

/// Applies a [`PanelAction`]; failures land in the toast.
pub fn perform(world: &mut World, action: PanelAction) {
    let result: Result<(), String> = match action {
        PanelAction::Undo => {
            crate::command::undo_world(world);
            Ok(())
        }
        PanelAction::Redo => {
            crate::command::redo_world(world);
            Ok(())
        }
        PanelAction::New => {
            world.resource_mut::<AnimEditor>().new_graph(None);
            Ok(())
        }
        PanelAction::Open(path) => {
            let full = resolve_path(world, &path);
            world.resource_mut::<AnimEditor>().open_graph(&full)
        }
        PanelAction::Save(path) => {
            let full = path.map(|p| resolve_path(world, &p));
            world
                .resource_mut::<AnimEditor>()
                .save_graph(full.as_deref())
                .map(|_| ())
        }
        PanelAction::DemoRig => world
            .resource_mut::<AnimEditor>()
            .set_rig(PreviewRig::demo(), None),
        PanelAction::SelectedRig => {
            let entity = world
                .get_resource::<crate::selection::Selection>()
                .and_then(crate::selection::Selection::primary);
            match entity.and_then(|e| entity_rig(world, e)) {
                Some(rig) => world.resource_mut::<AnimEditor>().set_rig(rig, None),
                None => Err("the selection has no SkinnedMesh".into()),
            }
        }
        PanelAction::GltfRig(path) => {
            let full = resolve_path(world, &path);
            PreviewRig::from_gltf(&full).and_then(|rig| world.resource_mut::<AnimEditor>().set_rig(rig, None))
        }
        PanelAction::SaveEdits(path) => {
            let full = path.map(|p| resolve_path(world, &p));
            world
                .resource_mut::<AnimEditor>()
                .save_edits(full.as_deref())
                .map(|_| ())
        }
    };
    if let Err(err) = result {
        world.resource_mut::<AnimEditor>().toast = err;
    }
}

/// The debug state per model node: PIE's snapshot when PIE runs this graph, else the preview's.
#[must_use]
pub fn node_debug(editor: &AnimEditor) -> BTreeMap<NodeId, NodeDebug> {
    let debug = editor
        .pie_debug
        .as_ref()
        .map(|p| &p.debug)
        .or(editor.preview.debug.as_ref());
    let Some(debug) = debug else {
        return BTreeMap::new();
    };
    let by_path: HashMap<&str, &NodeDebug> = debug.nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    editor
        .preview
        .paths
        .iter()
        .filter_map(|(id, path)| by_path.get(path.as_str()).map(|d| (*id, (*d).clone())))
        .collect()
}

/// A machine's live state from the debug map.
#[must_use]
pub fn machine_activity(debug: &BTreeMap<NodeId, NodeDebug>, machine: NodeId) -> MachineActivity {
    match debug.get(&machine).map(|d| &d.state) {
        Some(NodeDebugState::StateMachine {
            current, transition, ..
        }) => MachineActivity {
            current: Some(current.clone()),
            transition: transition.clone(),
        },
        _ => MachineActivity::default(),
    }
}

/// One-line summary of a node for its canvas body.
#[must_use]
pub fn node_summary(kind: &NodeKind) -> String {
    match kind {
        NodeKind::Clip(c) => format!(
            "{}{} ×{:.2}{}",
            if c.clip.is_empty() { "(no clip)" } else { &c.clip },
            if c.looping { " ⟳" } else { "" },
            c.speed,
            c.sync_group
                .as_ref()
                .map(|g| format!(" [{g}]"))
                .unwrap_or_default()
        ),
        NodeKind::BlendSpace1D(b) => format!("{} · {} samples", b.space.param, b.space.samples.len()),
        NodeKind::BlendSpace2D(b) => format!(
            "{} × {} · {} samples",
            b.space.param_x,
            b.space.param_y,
            b.space.samples.len()
        ),
        NodeKind::StateMachine(m) => {
            format!("{} states · {} transitions", m.states.len(), m.transitions.len())
        }
        NodeKind::Blend { alpha_param } => format!("α = {alpha_param}"),
        NodeKind::Additive { weight_param } => format!("w = {weight_param}"),
        NodeKind::Layer { layers } => format!("{} layer(s)", layers.len()),
        NodeKind::TwoBoneIK(ik) => format!("{} -> {}", ik.chain[0], ik.chain[2]),
        NodeKind::LookAt(l) => format!("aim {}", l.joint),
        NodeKind::RootMotion { mode } => format!("{mode:?}"),
        NodeKind::Slot { name, .. } => format!("slot {name}"),
        NodeKind::ChainIK(c) => format!("{} -> {} · {:?}", c.root, c.tip, c.solver),
        NodeKind::BodyAim(a) => format!("{} joint(s) aim {}", a.joints.len(), a.aim_joint),
        NodeKind::StrideWarp(w) => format!("{:?} by {}", w.settings.mode, w.ground_speed_param),
        NodeKind::OrientationWarp(w) => format!("{} to {}", w.pelvis, w.heading_param),
    }
}

/// Human title of a kind label.
#[must_use]
pub fn kind_title(label: &str) -> &'static str {
    match label {
        "clip" => "Clip",
        "blend_space_1d" => "Blend Space 1D",
        "blend_space_2d" => "Blend Space 2D",
        "state_machine" => "State Machine",
        "blend" => "Blend",
        "additive" => "Additive",
        "layer" => "Layers",
        "two_bone_ik" => "Two-Bone IK",
        "look_at" => "Look At",
        "root_motion" => "Root Motion",
        "slot" => "Montage Slot",
        "chain_ik" => "Chain IK",
        "body_aim" => "Body Aim",
        "stride_warp" => "Stride Warp",
        "orientation_warp" => "Orientation Warp",
        _ => "Node",
    }
}

/// Builds the canvas for `scope` from the model.
#[must_use]
pub fn build_snarl(model: &GraphModel, scope: &Scope, output_pos: Option<[f32; 2]>) -> Snarl<CanvasNode> {
    let mut snarl = Snarl::new();
    let mut ids = HashMap::new();
    let mut max_x = f32::MIN;
    for (id, node) in model.nodes_in(scope) {
        max_x = max_x.max(node.pos[0]);
        ids.insert(
            id,
            snarl.insert_node(egui::pos2(node.pos[0], node.pos[1]), CanvasNode::Node(id)),
        );
    }
    let out_pos = output_pos.unwrap_or([if max_x == f32::MIN { 300.0 } else { max_x + 460.0 }, 0.0]);
    let out = snarl.insert_node(egui::pos2(out_pos[0], out_pos[1]), CanvasNode::Output);
    for (id, node) in model.nodes_in(scope) {
        for (slot, child) in node.inputs.iter().enumerate() {
            if let (Some(child), Some(parent)) = (child.and_then(|c| ids.get(&c)), ids.get(&id)) {
                snarl.connect(
                    OutPinId {
                        node: *child,
                        output: 0,
                    },
                    InPinId {
                        node: *parent,
                        input: slot,
                    },
                );
            }
        }
    }
    if let Some(o) = model.scope_output(scope).and_then(|o| ids.get(&o)) {
        snarl.connect(OutPinId { node: *o, output: 0 }, InPinId { node: out, input: 0 });
    }
    snarl
}

struct Viewer<'a> {
    model: &'a GraphModel,
    scope: Scope,
    pending: &'a mut Vec<PendingEdit>,
    debug: &'a BTreeMap<NodeId, NodeDebug>,
    selected: &'a mut Option<NodeId>,
    open_machine: &'a mut Option<NodeId>,
    accents: crate::theme::AccentTokens,
    /// The node whose Nodes list row has the keyboard focus: it draws the focus ring.
    focused: Option<NodeId>,
    /// The theme's focus color.
    focus: egui::Color32,
}

impl Viewer<'_> {
    fn kind(&self, node: &CanvasNode) -> Option<&NodeKind> {
        match node {
            CanvasNode::Node(id) => self.model.nodes.get(id).map(|n| &n.kind),
            CanvasNode::Output => None,
        }
    }

    fn wire_edit(&mut self, child: NodeId, target: CanvasNode, slot: usize, connect: bool) {
        let scope = self.scope.clone();
        match target {
            CanvasNode::Node(parent) => {
                let slot = slot.to_string();
                if connect {
                    self.pending.push(PendingEdit::new("connect", None, move |d| {
                        d.graph.connect(child, parent, &slot)
                    }));
                } else {
                    self.pending.push(PendingEdit::new("disconnect", None, move |d| {
                        d.graph.disconnect(parent, &slot)
                    }));
                }
            }
            CanvasNode::Output => {
                let node = connect.then_some(child);
                self.pending.push(PendingEdit::new("set output", None, move |d| {
                    d.graph.set_output(&scope, node)
                }));
            }
        }
    }
}

impl SnarlViewer<CanvasNode> for Viewer<'_> {
    fn title(&mut self, node: &CanvasNode) -> String {
        match self.kind(node) {
            Some(kind) => kind_title(kind.label()).to_string(),
            None => "Output".into(),
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
        let Some(value) = snarl.get_node(node).copied() else {
            return;
        };
        let title = self.title(&value);
        match value {
            CanvasNode::Node(id) => {
                let response = ui.selectable_label(*self.selected == Some(id), format!("{title}  #{id}"));
                if response.clicked() {
                    *self.selected = Some(id);
                }
                if response.double_clicked() && matches!(self.kind(&value), Some(NodeKind::StateMachine(_))) {
                    *self.open_machine = Some(id);
                }
            }
            CanvasNode::Output => {
                ui.strong(title);
            }
        }
    }

    fn inputs(&mut self, node: &CanvasNode) -> usize {
        match self.kind(node) {
            Some(kind) => kind.input_names().len(),
            None => 1,
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
        let name = match snarl.get_node(pin.id.node).and_then(|n| self.kind(n)) {
            Some(kind) => kind.input_names().get(pin.id.input).cloned().unwrap_or_default(),
            None => "pose".into(),
        };
        ui.label(name);
        let fill = if pin.remotes.is_empty() {
            egui::Color32::from_rgb(200, 90, 70)
        } else {
            crate::theme::ui_tokens(ui.ctx()).accents.ok
        };
        PinInfo::circle().with_fill(fill)
    }

    fn show_output(
        &mut self,
        _pin: &OutPin,
        ui: &mut egui::Ui,
        _snarl: &mut Snarl<CanvasNode>,
    ) -> impl egui_snarl::ui::SnarlPin + 'static {
        ui.label("pose");
        PinInfo::circle().with_fill(egui::Color32::from_rgb(120, 170, 240))
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
        let Some(CanvasNode::Node(id)) = snarl.get_node(node).copied() else {
            return;
        };
        if let Some(kind) = self.model.nodes.get(&id).map(|n| &n.kind) {
            ui.weak(node_summary(kind));
            if matches!(kind, NodeKind::StateMachine(_)) && ui.small_button("Open states ›").clicked() {
                *self.open_machine = Some(id);
            }
        }
        if let Some(debug) = self.debug.get(&id) {
            let bar = egui::ProgressBar::new(debug.weight.clamp(0.0, 1.0))
                .desired_width(110.0)
                .text(format!("w {:.2}", debug.weight));
            ui.add(bar);
            if let NodeDebugState::StateMachine { current, .. } = &debug.state {
                ui.colored_label(egui::Color32::from_rgb(255, 160, 60), format!("▶ {current}"));
            }
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
        let Some(CanvasNode::Node(id)) = snarl.get_node(node).copied() else {
            return default.fill(egui::Color32::from_rgb(40, 60, 45));
        };
        if self.focused == Some(id) {
            // The keyboard focus ring (FU-5): wider than the selection outline.
            default.stroke(egui::Stroke::new(
                crate::canvas_keys::FOCUS_RING_WIDTH + 1.0,
                self.focus,
            ))
        } else if *self.selected == Some(id) {
            default.stroke(egui::Stroke::new(2.0, self.accents.select))
        } else if self.debug.get(&id).is_some_and(|d| d.active) {
            default.stroke(egui::Stroke::new(2.0, self.accents.active))
        } else {
            default
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<CanvasNode>) {
        let (Some(CanvasNode::Node(child)), Some(target)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) else {
            return;
        };
        self.wire_edit(child, target, to.id.input, true);
        snarl.drop_inputs(to.id);
        snarl.connect(from.id, to.id);
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<CanvasNode>) {
        let (Some(CanvasNode::Node(child)), Some(target)) = (
            snarl.get_node(from.id.node).copied(),
            snarl.get_node(to.id.node).copied(),
        ) else {
            return;
        };
        self.wire_edit(child, target, to.id.input, false);
        snarl.disconnect(from.id, to.id);
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<CanvasNode>) {
        if let Some(target) = snarl.get_node(pin.id.node).copied()
            && !pin.remotes.is_empty()
        {
            self.wire_edit(0, target, pin.id.input, false);
        }
        snarl.drop_inputs(pin.id);
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<CanvasNode>) {
        for remote in &pin.remotes {
            if let Some(target) = snarl.get_node(remote.node).copied() {
                self.wire_edit(0, target, remote.input, false);
            }
        }
        snarl.drop_outputs(pin.id);
    }

    fn has_graph_menu(&mut self, _pos: egui::Pos2, _snarl: &mut Snarl<CanvasNode>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: egui::Pos2, ui: &mut egui::Ui, _snarl: &mut Snarl<CanvasNode>) {
        ui.label("Add node");
        for label in NodeKind::LABELS {
            if ui.button(kind_title(label)).clicked() {
                let scope = self.scope.clone();
                self.pending.push(PendingEdit::new(
                    format!("add {}", kind_title(label)),
                    None,
                    move |d| {
                        let kind =
                            NodeKind::default_of(label).ok_or_else(|| format!("unknown kind {label}"))?;
                        d.graph.add_node(kind, scope, [pos.x, pos.y]).map(|_| ())
                    },
                ));
                ui.close();
            }
        }
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
        let Some(CanvasNode::Node(id)) = snarl.get_node(node).copied() else {
            return;
        };
        if ui.button("Make output").clicked() {
            let scope = self.scope.clone();
            self.pending.push(PendingEdit::new("set output", None, move |d| {
                d.graph.set_output(&scope, Some(id))
            }));
            ui.close();
        }
        if matches!(self.kind(&CanvasNode::Node(id)), Some(NodeKind::StateMachine(_)))
            && ui.button("Open states").clicked()
        {
            *self.open_machine = Some(id);
            ui.close();
        }
        if ui.button("Duplicate").clicked()
            && let Some(n) = self.model.nodes.get(&id).cloned()
            && !matches!(n.kind, NodeKind::StateMachine(_))
        {
            self.pending
                .push(PendingEdit::new("duplicate node", None, move |d| {
                    d.graph
                        .add_node(n.kind, n.scope, [n.pos[0] + 30.0, n.pos[1] + 30.0])
                        .map(|_| ())
                }));
            ui.close();
        }
        if ui.button("Delete").clicked() {
            self.pending.push(PendingEdit::new("delete node", None, move |d| {
                d.graph.remove_node(id)
            }));
            if *self.selected == Some(id) {
                *self.selected = None;
            }
            ui.close();
        }
    }
}

/// The canvas title chain for the current view, root first.
#[must_use]
pub fn breadcrumbs(model: &GraphModel, ui: &AnimUi) -> Vec<(String, Option<Scope>, Option<NodeId>)> {
    let mut chain = Vec::new();
    let mut scope = ui.scope.clone().unwrap_or(Scope::Root);
    let mut machine_view = ui.machine_view;
    loop {
        if let Some(machine) = machine_view {
            chain.push((format!("State Machine #{machine}"), None, Some(machine)));
            scope = model
                .node(machine)
                .map(|n| n.scope.clone())
                .unwrap_or(Scope::Root);
            machine_view = None;
            continue;
        }
        match scope.clone() {
            Scope::Root => {
                chain.push(("Root".into(), Some(Scope::Root), None));
                break;
            }
            Scope::State { machine, state } => {
                chain.push((state.clone(), Some(scope.clone()), None));
                machine_view = Some(machine);
            }
        }
        if chain.len() > 64 {
            break;
        }
    }
    chain.reverse();
    chain
}

/// Draws the Anim Graph panel against the world's [`AnimEditor`].
#[tracing::instrument(target = "regia_editor", skip_all, name = "anim_graph_panel")]
pub fn anim_graph_panel(ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
    let _span = tracing::info_span!("anim_graph_panel").entered();
    if !world.contains_resource::<AnimEditor>() {
        world.init_resource::<AnimEditor>();
    }
    let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.next_undo(), s.next_redo()))
        .unwrap_or_default();
    let mut pending = Vec::new();
    let mut actions = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<AnimEditor>| {
        let editor = &mut *editor;
        let rig = editor.rig_or_demo().clone();
        let revision = editor.revision;
        editor.preview.tick_graph(&rig, &editor.docs, revision, dt);
        draw(ui, editor, &rig, device, (undo, redo), &mut pending, &mut actions);
    });
    apply_pending(world, pending);
    for action in actions {
        perform(world, action);
    }
    if world.resource::<AnimEditor>().preview.playing {
        ui.ctx().request_repaint();
    }
}

#[allow(clippy::too_many_arguments)] // the panel's disjoint borrows, passed once
fn draw(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    rig: &PreviewRig,
    device: Option<&RenderDevice>,
    (undo, redo): (Option<String>, Option<String>),
    pending: &mut Vec<PendingEdit>,
    actions: &mut Vec<PanelAction>,
) {
    toolbar(ui, editor, (undo, redo), actions);
    ui.separator();
    let debug = node_debug(editor);
    egui::Panel::left("anim-graph-side")
        .resizable(true)
        .default_size(270.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                parameters_ui(ui, editor, pending);
                ui.separator();
                super::graph_inspector::inspector_ui(
                    ui,
                    &editor.docs,
                    rig,
                    &mut editor.ui,
                    &mut editor.preview,
                    &debug,
                    pending,
                );
            });
        });
    egui::Panel::right("anim-graph-preview")
        .resizable(true)
        .default_size(300.0)
        .show(ui, |ui| preview_ui(ui, editor, rig, device));
    egui::Panel::left("anim-graph-nodes")
        .resizable(true)
        .default_size(200.0)
        .show(ui, |ui| {
            if editor.ui.machine_view.is_some() {
                super::sm_canvas::states_ui(ui, editor, pending);
            } else {
                nodes_ui(ui, editor, pending);
            }
        });
    egui::CentralPanel::default().show(ui, |ui| canvas_ui(ui, editor, &debug, pending));
}

fn toolbar(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    (undo, redo): (Option<String>, Option<String>),
    actions: &mut Vec<PanelAction>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.strong("Anim Graph");
        match &editor.graph_path {
            Some(path) => ui.weak(path.display().to_string()),
            None => ui.weak("(unsaved graph)"),
        };
        if editor.graph_dirty() {
            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.unsaved, "· unsaved");
        }
        ui.separator();
        if ui.button("New").clicked() {
            actions.push(PanelAction::New);
        }
        let path_id = egui::Id::new("anim-graph-path");
        let mut path: String = ui.data(|d| d.get_temp(path_id)).unwrap_or_default();
        ui.add(
            egui::TextEdit::singleline(&mut path)
                .hint_text("graphs/x.animgraph.ron")
                .desired_width(170.0),
        );
        if ui.button("Open").clicked() && !path.is_empty() {
            actions.push(PanelAction::Open(path.clone()));
        }
        if ui.button("Save").clicked() {
            actions.push(PanelAction::Save((!path.is_empty()).then(|| path.clone())));
        }
        crate::diff_view::graph_file_buttons(ui, editor.graph_path.as_deref());
        ui.data_mut(|d| d.insert_temp(path_id, path));
        ui.separator();
        if ui
            .add_enabled(undo.is_some(), egui::Button::new("Undo"))
            .on_hover_text(undo.unwrap_or_default())
            .clicked()
        {
            actions.push(PanelAction::Undo);
        }
        if ui
            .add_enabled(redo.is_some(), egui::Button::new("Redo"))
            .on_hover_text(redo.unwrap_or_default())
            .clicked()
        {
            actions.push(PanelAction::Redo);
        }
        ui.separator();
        ui.label("Rig:");
        let rig_name = editor
            .rig
            .as_ref()
            .map_or("Demo biped", |r| r.name.as_str())
            .to_string();
        egui::ComboBox::from_id_salt("anim-rig")
            .selected_text(rig_name)
            .show_ui(ui, |ui| {
                if ui.selectable_label(false, "Demo biped").clicked() {
                    actions.push(PanelAction::DemoRig);
                }
                if ui.selectable_label(false, "Selected entity").clicked() {
                    actions.push(PanelAction::SelectedRig);
                }
            });
        let gltf_id = egui::Id::new("anim-rig-gltf");
        let mut gltf: String = ui.data(|d| d.get_temp(gltf_id)).unwrap_or_default();
        ui.add(
            egui::TextEdit::singleline(&mut gltf)
                .hint_text("characters/x.glb")
                .desired_width(130.0),
        );
        if ui.button("Load rig").clicked() && !gltf.is_empty() {
            actions.push(PanelAction::GltfRig(gltf.clone()));
        }
        ui.data_mut(|d| d.insert_temp(gltf_id, gltf));
    });
    if !editor.toast.is_empty() {
        ui.weak(&editor.toast);
    }
}

/// A short label of a parameter kind.
#[must_use]
pub fn param_kind(value: &ParamValue) -> &'static str {
    match value {
        ParamValue::Float(_) => "float",
        ParamValue::Int(_) => "int",
        ParamValue::Bool(_) => "bool",
        ParamValue::Vec2(_) => "vec2",
        ParamValue::Vec3(_) => "vec3",
        ParamValue::Trigger => "trigger",
    }
}

/// The default value of a parameter kind label.
#[must_use]
pub fn param_default(kind: &str) -> Option<ParamValue> {
    Some(match kind {
        "float" => ParamValue::Float(0.0),
        "int" => ParamValue::Int(0),
        "bool" => ParamValue::Bool(false),
        "vec2" => ParamValue::Vec2(Vec2::ZERO),
        "vec3" => ParamValue::Vec3(Vec3::ZERO),
        "trigger" => ParamValue::Trigger,
        _ => return None,
    })
}

/// One parameter's value field (a focusable widget per component, a button for a trigger).
/// Returns the new value when the user changed it.
fn value_field(ui: &mut egui::Ui, name: &str, value: ParamValue) -> Option<ParamValue> {
    let named = |r: egui::Response, kind| crate::a11y_audit::name(r, kind, &format!("{name} value"));
    match value {
        ParamValue::Float(mut v) => named(
            ui.add(egui::DragValue::new(&mut v).speed(0.05)),
            egui::WidgetType::DragValue,
        )
        .changed()
        .then_some(ParamValue::Float(v)),
        ParamValue::Int(mut v) => named(ui.add(egui::DragValue::new(&mut v)), egui::WidgetType::DragValue)
            .changed()
            .then_some(ParamValue::Int(v)),
        ParamValue::Bool(mut v) => named(ui.checkbox(&mut v, ""), egui::WidgetType::Checkbox)
            .changed()
            .then_some(ParamValue::Bool(v)),
        ParamValue::Vec2(mut v) => {
            let mut changed = false;
            for (axis, c) in [("x", &mut v.x), ("y", &mut v.y)] {
                let r = ui.add(egui::DragValue::new(c).speed(0.05));
                changed |= crate::a11y_audit::name(r, egui::WidgetType::DragValue, &format!("{name} {axis}"))
                    .changed();
            }
            changed.then_some(ParamValue::Vec2(v))
        }
        ParamValue::Vec3(mut v) => {
            let mut changed = false;
            for (axis, c) in [("x", &mut v.x), ("y", &mut v.y), ("z", &mut v.z)] {
                let r = ui.add(egui::DragValue::new(c).speed(0.05));
                changed |= crate::a11y_audit::name(r, egui::WidgetType::DragValue, &format!("{name} {axis}"))
                    .changed();
            }
            changed.then_some(ParamValue::Vec3(v))
        }
        ParamValue::Trigger => ui.small_button("fire").clicked().then_some(ParamValue::Trigger),
    }
}

fn parameters_ui(ui: &mut egui::Ui, editor: &mut AnimEditor, pending: &mut Vec<PendingEdit>) {
    // The recorder (P-206) sits above the parameters it records.
    super::recorder::recorder_ui(ui, editor);
    ui.separator();
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let decls = editor.docs.graph.parameters.clone();
    let pie = editor.pie_debug.as_ref().map(|p| p.debug.parameters.clone());
    // A scrubbed recording shows recorded values; a running session writes live (P-201).
    let replaying = editor.recorder.scrub().is_some();
    ui.horizontal(|ui| {
        ui.strong("Parameters");
        if pie.is_some() && !replaying {
            ui.colored_label(tokens.accents.alert, "Live").on_hover_text(
                "Edits write to the running animator in play. They are not saved and end when play stops.",
            );
        } else if replaying {
            ui.colored_label(tokens.accents.select, "Recorded");
        }
    });
    egui::Grid::new("anim-params")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            for decl in &decls {
                let name = decl.0.clone();
                ui.label(&name);
                ui.weak(param_kind(&decl.1));
                match &pie {
                    Some(pie) if replaying => {
                        let value = pie.iter().find(|p| p.0 == name).map(|p| p.1);
                        ui.monospace(value.map_or_else(|| "–".to_string(), |v| format!("{v:?}")));
                    }
                    Some(pie) => {
                        // The running animator's value; an edit is a live debug write.
                        let value = pie
                            .iter()
                            .find(|p| p.0 == name)
                            .map(|p| p.1)
                            .or_else(|| editor.live.written.get(&name).copied())
                            .unwrap_or(decl.1);
                        if let Some(v) = value_field(ui, &name, value) {
                            editor
                                .live
                                .queue_from_panel(&name, v, ui.ctx().cumulative_frame_nr());
                        }
                    }
                    None => {
                        let value = editor.preview.params.get(&name).unwrap_or(decl.1);
                        if let Some(v) = value_field(ui, &name, value) {
                            editor.preview.set_param(&name, v);
                        }
                    }
                }
                if ui.small_button("×").on_hover_text("Remove declaration").clicked() {
                    let n = name.clone();
                    pending.push(PendingEdit::new(
                        format!("remove parameter {n}"),
                        None,
                        move |d| {
                            if d.graph.remove_param(&n) {
                                Ok(())
                            } else {
                                Err(format!("no parameter {n}"))
                            }
                        },
                    ));
                }
                ui.end_row();
            }
        });
    ui.horizontal(|ui| {
        let name_id = egui::Id::new("anim-new-param");
        let kind_id = egui::Id::new("anim-new-param-kind");
        let mut name: String = ui.data(|d| d.get_temp(name_id)).unwrap_or_default();
        let mut kind: String = ui
            .data(|d| d.get_temp(kind_id))
            .unwrap_or_else(|| "float".to_string());
        let field = ui.add(
            egui::TextEdit::singleline(&mut name)
                .hint_text("name")
                .desired_width(90.0),
        );
        crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "New parameter name");
        let combo = egui::ComboBox::from_id_salt("anim-new-param-kind-combo")
            .selected_text(kind.as_str())
            .width(70.0)
            .show_ui(ui, |ui| {
                for k in ["float", "int", "bool", "vec2", "vec3", "trigger"] {
                    ui.selectable_value(&mut kind, k.to_string(), k);
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "New parameter kind");
        if ui.button("Add").clicked()
            && !name.is_empty()
            && let Some(value) = param_default(&kind)
        {
            let n = std::mem::take(&mut name);
            pending.push(PendingEdit::new(format!("add parameter {n}"), None, move |d| {
                d.graph.set_param(&n, value);
                Ok(())
            }));
        }
        ui.data_mut(|d| {
            d.insert_temp(name_id, name);
            d.insert_temp(kind_id, kind);
        });
    });
}

fn canvas_ui(
    ui: &mut egui::Ui,
    editor: &mut AnimEditor,
    debug: &BTreeMap<NodeId, NodeDebug>,
    pending: &mut Vec<PendingEdit>,
) {
    // Breadcrumbs.
    let crumbs = breadcrumbs(&editor.docs.graph, &editor.ui);
    ui.horizontal(|ui| {
        for (i, (label, scope, machine)) in crumbs.iter().enumerate() {
            if i > 0 {
                ui.weak("›");
            }
            if ui.link(label).clicked() {
                editor.ui.scope = scope.clone().or_else(|| {
                    machine.and_then(|m| editor.docs.graph.node(m).ok().map(|n| n.scope.clone()))
                });
                editor.ui.machine_view = *machine;
            }
        }
        match &editor.preview.error {
            Some(err) => {
                ui.colored_label(egui::Color32::from_rgb(230, 110, 90), err);
            }
            None => {
                if let Err(err) = editor.docs.graph.compile_validated() {
                    ui.colored_label(egui::Color32::from_rgb(230, 180, 90), err);
                }
            }
        }
    });
    // A deleted machine or state falls back to the root canvas.
    if let Some(machine) = editor.ui.machine_view
        && editor.docs.graph.machine(machine).is_err()
    {
        editor.ui.machine_view = None;
    }
    if let Some(Scope::State { machine, state }) = editor.ui.scope.clone()
        && !editor
            .docs
            .graph
            .machine(machine)
            .is_ok_and(|m| m.states.iter().any(|s| s.name == state))
    {
        editor.ui.scope = Some(Scope::Root);
    }

    if let Some(machine_id) = editor.ui.machine_view {
        let Ok(machine) = editor.docs.graph.machine(machine_id).cloned() else {
            return;
        };
        let activity = machine_activity(debug, machine_id);
        machine_canvas(ui, machine_id, &machine, &activity, &mut editor.ui, pending);
        return;
    }

    let scope = editor.ui.scope.clone().unwrap_or(Scope::Root);
    let key = (scope.clone(), editor.revision);
    if editor.ui.snarl_key.as_ref() != Some(&key) || editor.ui.snarl.is_none() {
        let output_pos = output_pos_memory(ui, &scope);
        editor.ui.snarl = Some(build_snarl(&editor.docs.graph, &scope, output_pos));
        editor.ui.snarl_key = Some(key);
    }
    let Some(mut snarl) = editor.ui.snarl.take() else {
        return;
    };
    let mut selected = editor.ui.selected_node;
    let mut open_machine = None;
    {
        let focused = editor
            .ui
            .keys
            .list
            .focused()
            .and_then(|row| scope_nodes(&editor.docs.graph, &scope).get(row).copied());
        let mut viewer = Viewer {
            model: &editor.docs.graph,
            scope: scope.clone(),
            pending,
            debug,
            selected: &mut selected,
            open_machine: &mut open_machine,
            accents: crate::theme::ui_tokens(ui.ctx()).accents,
            focused,
            focus: crate::canvas_keys::focus_color(ui.ctx()),
        };
        let style = SnarlStyle {
            collapsible: Some(false),
            ..SnarlStyle::new()
        };
        // A fresh canvas id per Enter in the Nodes list (ADR-7383).
        let widget = SnarlWidget::new()
            .id_salt((
                "anim-graph-canvas",
                format!("{scope:?}"),
                editor.ui.keys.frame_epoch,
            ))
            .style(style);
        let response = match editor.ui.keys.frame.take() {
            // Enter in the Nodes list: one frame of just that node under a fresh canvas id fits
            // the view on it (crate::canvas_keys::frame_snarl).
            Some(node) => widget.show(
                &mut crate::canvas_keys::frame_snarl(&snarl, node),
                &mut viewer,
                ui,
            ),
            None => widget.show(&mut snarl, &mut viewer, ui),
        };
        crate::canvas_keys::name_canvas(
            ui.ctx(),
            response.id,
            &format!(
                "Animation graph canvas, {} nodes",
                editor.docs.graph.nodes_in(&scope).count()
            ),
        );
    }
    // Node drags: write moved positions back as coalesced edits.
    for (sid, pos, value) in snarl.nodes_pos_ids() {
        let _ = sid;
        match *value {
            CanvasNode::Node(id) => {
                if let Ok(node) = editor.docs.graph.node(id)
                    && ((node.pos[0] - pos.x).abs() > 0.5 || (node.pos[1] - pos.y).abs() > 0.5)
                {
                    let p = [pos.x, pos.y];
                    pending.push(PendingEdit::new(
                        "move node",
                        Some(format!("move:{id}")),
                        move |d| d.graph.move_node(id, p),
                    ));
                }
            }
            CanvasNode::Output => set_output_pos_memory(ui, &scope, [pos.x, pos.y]),
        }
    }
    editor.ui.snarl = Some(snarl);
    editor.ui.selected_node = selected;
    if let Some(machine) = open_machine {
        editor.ui.machine_view = Some(machine);
        editor.ui.selected_node = Some(machine);
    }
}

/// The nodes of `scope` in Nodes list order (by id).
fn scope_nodes(model: &GraphModel, scope: &Scope) -> Vec<NodeId> {
    model.nodes_in(scope).map(|(id, _)| id).collect()
}

/// The Nodes list row (and accessible) name of a node: its kind title and id ("Clip #1").
#[must_use]
pub fn node_row_name(model: &GraphModel, id: NodeId) -> String {
    model.node(id).map_or_else(
        |_| format!("Node #{id}"),
        |n| format!("{} #{id}", kind_title(n.kind.label())),
    )
}

/// The canvas node a **Connect…** form names: "Output" (the canvas output), a row name
/// ("Blend #2"), "#2" or "2". `None`: no such node on this canvas.
#[must_use]
pub fn connect_target(model: &GraphModel, nodes: &[NodeId], text: &str) -> Option<CanvasNode> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("output") {
        return Some(CanvasNode::Output);
    }
    let id = text.trim_start_matches('#').parse::<NodeId>().ok().or_else(|| {
        nodes
            .iter()
            .copied()
            .find(|id| node_row_name(model, *id).eq_ignore_ascii_case(text))
    })?;
    nodes.contains(&id).then_some(CanvasNode::Node(id))
}

/// The Nodes list beside the canvas: the keyboard route to add, wire, move, delete and select
/// nodes (FU-5, the convention of `canvas_keys`). Its edits are the ones the canvas's mouse
/// menus and drags make (`editor.anim_add_node`, `editor.anim_connect`, `editor.anim_set_node`,
/// `editor.anim_remove_node`).
fn nodes_ui(ui: &mut egui::Ui, editor: &mut AnimEditor, pending: &mut Vec<PendingEdit>) {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let scope = editor.ui.scope.clone().unwrap_or(Scope::Root);
    let model = &editor.docs.graph;
    let nodes = scope_nodes(model, &scope);
    let names: Vec<String> = nodes.iter().map(|id| node_row_name(model, *id)).collect();
    // The focus follows its node when rows move.
    let anchored = editor
        .ui
        .keys_node
        .and_then(|n| nodes.iter().position(|m| *m == n));
    if anchored.is_some() {
        editor.ui.keys.list.retarget(anchored);
    }
    let selected = editor
        .ui
        .selected_node
        .and_then(|s| nodes.iter().position(|n| *n == s));
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let kinds = |_: usize, filter: &str| {
        let filter = filter.to_lowercase();
        NodeKind::LABELS
            .iter()
            .map(|l| kind_title(l).to_string())
            .filter(|t| t.to_lowercase().contains(&filter))
            .collect()
    };
    let rows = GraphRows {
        len: nodes.len(),
        name: &name,
        selected,
        kinds: &kinds,
        named_outputs: false,
        read_only: false,
        input_pins: true,
        deletable: true,
        heading: "Nodes",
        noun: "node",
    };
    let actions = graph_keys_ui(ui, "anim-graph-nodes", &mut editor.ui.keys, &rows);
    editor.ui.keys_node = editor.ui.keys.list.current.and_then(|i| nodes.get(i).copied());
    for action in actions {
        let at = |row: usize| nodes.get(row).copied();
        match action {
            GraphAction::Activate(row) => {
                let Some(id) = at(row) else { continue };
                if editor.ui.selected_node == Some(id)
                    && matches!(
                        editor.docs.graph.node(id).map(|n| &n.kind),
                        Ok(NodeKind::StateMachine(_))
                    )
                {
                    // Enter on the selected state machine: the node menu's Open states.
                    editor.ui.machine_view = Some(id);
                    continue;
                }
                editor.ui.keys.frame = editor.ui.snarl.as_ref().and_then(|s| {
                    s.node_ids()
                        .find(|(_, n)| **n == CanvasNode::Node(id))
                        .map(|(sid, _)| sid)
                });
                editor.ui.keys.frame_epoch += 1;
                editor.ui.selected_node = Some(id);
            }
            GraphAction::Delete(row) => {
                let Some(id) = at(row) else { continue };
                if editor.ui.selected_node == Some(id) {
                    editor.ui.selected_node = None;
                }
                // The node menu's Delete.
                pending.push(PendingEdit::new("delete node", None, move |d| {
                    d.graph.remove_node(id)
                }));
            }
            GraphAction::Nudge(row, delta) => {
                let Some(id) = at(row) else { continue };
                let Ok(node) = editor.docs.graph.node(id) else {
                    continue;
                };
                let p = [node.pos[0] + delta[0], node.pos[1] + delta[1]];
                // The canvas drag's edit, without its merge key: one undo entry per press.
                pending.push(PendingEdit::new("move node", None, move |d| {
                    d.graph.move_node(id, p)
                }));
            }
            GraphAction::AddAfter(row, title) => {
                let Some(from) = at(row) else { continue };
                let Some(label) = NodeKind::LABELS.iter().copied().find(|l| kind_title(l) == title) else {
                    continue;
                };
                let Ok(node) = editor.docs.graph.node(from) else {
                    continue;
                };
                let pos = [node.pos[0] + 220.0, node.pos[1]];
                let scope = scope.clone();
                // The graph menu's add, then the wire into the new node's first input: one entry.
                pending.push(PendingEdit::new(format!("add {title}"), None, move |d| {
                    let kind = NodeKind::default_of(label).ok_or_else(|| format!("unknown kind {label}"))?;
                    let wired = !kind.input_names().is_empty();
                    let id = d.graph.add_node(kind, scope, pos)?;
                    if wired {
                        d.graph.connect(from, id, "0")?;
                    }
                    Ok(())
                }));
            }
            GraphAction::Connect(row, form) => {
                let Some(child) = at(row) else { continue };
                match connect_target(&editor.docs.graph, &nodes, &form.target) {
                    Some(CanvasNode::Output) => {
                        let scope = scope.clone();
                        pending.push(PendingEdit::new("set output", None, move |d| {
                            d.graph.set_output(&scope, Some(child))
                        }));
                    }
                    Some(CanvasNode::Node(parent)) => {
                        let slot = match form.input.trim() {
                            "" => "0".to_string(),
                            s => s.to_string(),
                        };
                        pending.push(PendingEdit::new("connect", None, move |d| {
                            d.graph.connect(child, parent, &slot)
                        }));
                    }
                    None => {
                        editor.toast = format!("no node {:?} on this canvas", form.target.trim());
                    }
                }
            }
        }
    }
}

fn output_pos_memory(ui: &egui::Ui, scope: &Scope) -> Option<[f32; 2]> {
    ui.data(|d| d.get_temp(egui::Id::new(("anim-output-pos", format!("{scope:?}")))))
}

fn set_output_pos_memory(ui: &egui::Ui, scope: &Scope, pos: [f32; 2]) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("anim-output-pos", format!("{scope:?}"))), pos));
}

fn preview_ui(ui: &mut egui::Ui, editor: &mut AnimEditor, rig: &PreviewRig, device: Option<&RenderDevice>) {
    ui.horizontal(|ui| {
        ui.strong("Preview");
        ui.weak(&rig.name);
    });
    ui.horizontal(|ui| {
        let label = if editor.preview.playing { "Pause" } else { "Play" };
        if ui.button(label).clicked() {
            editor.preview.playing = !editor.preview.playing;
        }
        if ui.button("Restart").clicked() {
            editor.preview.reset();
        }
        ui.add(egui::Slider::new(&mut editor.preview.speed, 0.0..=2.0).text("speed"));
    });
    ui.checkbox(&mut editor.preview.apply_root_motion, "Apply root motion");
    if let Some(pie) = &editor.pie_debug {
        ui.colored_label(
            egui::Color32::from_rgb(255, 160, 60),
            format!(
                "PIE: debugging {}",
                pie.name.clone().unwrap_or_else(|| format!("{:x}", pie.entity))
            ),
        );
    }
    if let Some(state) = editor
        .pie_debug
        .as_ref()
        .map(|p| &p.debug)
        .or(editor.preview.debug.as_ref())
        .and_then(|d| d.first_state())
    {
        ui.label(format!("State: {state}"));
    }
    for (age, name) in editor.preview.fired.iter().rev().take(4) {
        ui.weak(format!("notify {name} ({age:.1}s ago)"));
    }
    let skeleton = rig.edited_skeleton(&editor.docs.edits);
    // A scrubbed recording poses the preview (when the recorded rig has the preview's joints).
    let globals = match editor.recorder.scrub_frame() {
        Some(frame) if frame.pose.len() == skeleton.len() => skeleton.globals(&frame.pose, None),
        _ => editor.preview.globals(&skeleton),
    };
    let highlight = editor.ui.selected_joint;
    let mut orbit = editor.preview.orbit;
    let clicked = super::preview::preview_widget(
        ui,
        "anim-graph-preview",
        &skeleton,
        &globals,
        &mut orbit,
        highlight,
        device,
    );
    editor.preview.orbit = orbit;
    if clicked.is_some() {
        editor.ui.selected_joint = clicked;
    }
    super::preview::locomotion_section(ui, &mut editor.locomotion);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world with the editor, an undo stack and a new graph holding `nodes` (kind, position)
    /// added through `editor.anim_add_node`; returns the node ids.
    fn graph_world(nodes: &[(&str, [f32; 2])]) -> (World, Vec<NodeId>) {
        let mut world = World::new();
        world.init_resource::<AnimEditor>();
        world.init_resource::<crate::command::UndoStack>();
        call(
            &mut world,
            "editor.anim_open",
            serde_json::json!({ "create": true }),
        );
        let ids = nodes
            .iter()
            .map(|(kind, pos)| {
                call(
                    &mut world,
                    "editor.anim_add_node",
                    serde_json::json!({ "kind": kind, "pos": pos }),
                )["node"]
                    .as_u64()
                    .and_then(|n| NodeId::try_from(n).ok())
                    .unwrap_or_else(|| panic!("node id"))
            })
            .collect();
        (world, ids)
    }

    fn call(world: &mut World, method: &str, params: serde_json::Value) -> serde_json::Value {
        let handler = super::super::agent::methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .map(|(_, h)| h)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
    }

    /// The open graph document as text (its model, field by field): what the keyboard and the
    /// mouse paths must leave identical. The model, not the saved file: saving validates the
    /// whole graph, which a half-built test graph does not pass.
    fn doc_text(world: &World) -> String {
        format!("{:#?}", world.resource::<AnimEditor>().docs.graph)
    }

    const SIZE: [f32; 2] = [1500.0, 900.0];

    /// FU-5: the Nodes list selects (Enter), adds a node wired after the focused one
    /// (Ctrl+Shift+A, filter, Enter), moves it (Alt+Right, one undo entry) and makes it the output
    /// (Ctrl+Shift+C, "Output", Enter) with keys only; the saved graph equals the one the mouse's edits build
    /// (the same model ops, driven here through the agent methods that share them).
    #[test]
    fn the_nodes_list_edits_by_keyboard_what_the_mouse_edits() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to, text};
        use egui::{Key, Modifiers};
        use serde_json::json;
        let (mut mouse, ids) = graph_world(&[("clip", [0.0, 0.0])]);
        let clip = ids[0];
        let add = call(
            &mut mouse,
            "editor.anim_add_node",
            json!({ "kind": "root_motion", "pos": [220.0, 0.0] }),
        )["node"]
            .as_u64()
            .unwrap_or_default();
        call(
            &mut mouse,
            "editor.anim_connect",
            json!({ "child": clip, "parent": add, "slot": "0" }),
        );
        call(
            &mut mouse,
            "editor.anim_set_node",
            json!({ "node": add, "pos": [236.0, 0.0] }),
        );
        call(&mut mouse, "editor.anim_connect", json!({ "child": add }));

        let (mut world, _) = graph_world(&[("clip", [0.0, 0.0])]);
        let toast = world.resource::<AnimEditor>().toast.clone();
        let ctx = egui::Context::default();
        let draw = |world: &mut World, events: Vec<egui::Event>| {
            frame(&ctx, SIZE, events, |ui| anim_graph_panel(ui, world, None))
        };
        let first = draw(&mut world, vec![]);
        assert!(
            labels_of(&first, egui::accesskit::Role::Canvas)
                .contains(&"Animation graph canvas, 1 nodes".to_string()),
            "{:?}",
            labels_of(&first, egui::accesskit::Role::Canvas)
        );
        let clip_row = format!("Clip #{clip}");
        tab_to(&ctx, SIZE, &clip_row, false, 400, |ui| {
            anim_graph_panel(ui, &mut world, None)
        });
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(
            world.resource::<AnimEditor>().ui.selected_node,
            Some(clip),
            "Enter selects"
        );
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        draw(
            &mut world,
            vec![key(Key::A, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        draw(&mut world, vec![]);
        draw(&mut world, vec![text("root")]);
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
            Some(clip_row.as_str()),
            "the focus came back"
        );
        draw(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        let down = draw(&mut world, vec![]);
        assert_eq!(focused_name(&down), Some(format!("Root Motion #{add}")));
        draw(&mut world, vec![key(Key::ArrowRight, Modifiers::ALT)]);
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 2,
            "one move entry per key press"
        );
        // Ctrl+Shift+C on the row starts the connection (Connect…, its first field focused).
        draw(
            &mut world,
            vec![key(Key::C, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        draw(&mut world, vec![]);
        draw(&mut world, vec![text("Output")]);
        draw(&mut world, vec![key(Key::Tab, Modifiers::NONE)]);
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        let done = draw(&mut world, vec![]);
        assert!(
            labels_of(&done, egui::accesskit::Role::Canvas)
                .contains(&"Animation graph canvas, 2 nodes".to_string())
        );
        assert_eq!(world.resource::<AnimEditor>().toast, toast, "no edit was refused");
        assert_eq!(doc_text(&world), doc_text(&mouse));
    }

    /// FU-5: a state machine opens by keyboard (Enter on its selected row), and its States list
    /// adds a state with a transition from Any (Ctrl+Shift+A), moves it (Alt+Down) and selects it
    /// (Enter), with keys only; the saved graph equals the mouse's (Add state, Add transition from
    /// Any, a drag).
    #[test]
    fn the_states_list_edits_by_keyboard_what_the_mouse_edits() {
        use super::super::sm_canvas::{ADD_AFTER_OFFSET, ANY_POS};
        use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};
        use egui::{Key, Modifiers};
        use serde_json::json;
        let at = [ANY_POS[0] + ADD_AFTER_OFFSET[0], ANY_POS[1] + ADD_AFTER_OFFSET[1]];

        let (mut mouse, ids) = graph_world(&[("state_machine", [0.0, 0.0])]);
        let machine = ids[0];
        call(
            &mut mouse,
            "editor.anim_state",
            json!({ "machine": machine, "op": "add", "name": "State", "pos": at }),
        );
        call(
            &mut mouse,
            "editor.anim_transition",
            json!({ "machine": machine, "to": "State" }),
        );
        call(
            &mut mouse,
            "editor.anim_state",
            json!({ "machine": machine, "op": "move", "name": "State", "pos": [at[0], at[1] + 16.0] }),
        );

        let (mut world, _) = graph_world(&[("state_machine", [0.0, 0.0])]);
        let toast = world.resource::<AnimEditor>().toast.clone();
        let ctx = egui::Context::default();
        let draw = |world: &mut World, events: Vec<egui::Event>| {
            frame(&ctx, SIZE, events, |ui| anim_graph_panel(ui, world, None))
        };
        draw(&mut world, vec![]);
        tab_to(
            &ctx,
            SIZE,
            &format!("State Machine #{machine}"),
            false,
            400,
            |ui| anim_graph_panel(ui, &mut world, None),
        );
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(
            world.resource::<AnimEditor>().ui.machine_view,
            Some(machine),
            "Enter on the selected machine opens its states"
        );
        let opened = draw(&mut world, vec![]);
        assert!(
            labels_of(&opened, egui::accesskit::Role::Canvas)
                .contains(&"State machine canvas, 0 states, 0 transitions".to_string()),
            "{:?}",
            labels_of(&opened, egui::accesskit::Role::Canvas)
        );
        tab_to(&ctx, SIZE, "Any state", false, 400, |ui| {
            anim_graph_panel(ui, &mut world, None)
        });
        draw(
            &mut world,
            vec![key(Key::A, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        draw(&mut world, vec![]);
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        draw(&mut world, vec![]);
        let back = draw(&mut world, vec![]);
        assert_eq!(
            focused_name(&back).as_deref(),
            Some("Any state"),
            "the focus stayed on Any"
        );
        draw(&mut world, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        let up = draw(&mut world, vec![]);
        assert_eq!(focused_name(&up).as_deref(), Some("State State (initial)"));
        draw(&mut world, vec![key(Key::ArrowDown, Modifiers::ALT)]);
        draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
        let done = draw(&mut world, vec![]);
        assert_eq!(
            world.resource::<AnimEditor>().ui.selected_state.as_deref(),
            Some("State"),
            "Enter selects the state"
        );
        assert!(
            labels_of(&done, egui::accesskit::Role::Canvas)
                .contains(&"State machine canvas, 1 states, 1 transitions".to_string())
        );
        assert_eq!(world.resource::<AnimEditor>().toast, toast, "no edit was refused");
        assert_eq!(doc_text(&world), doc_text(&mouse));
    }

    #[test]
    fn connect_target_reads_output_ids_and_row_names() {
        let (world, ids) = graph_world(&[("clip", [0.0, 0.0]), ("blend", [200.0, 0.0])]);
        let model = &world.resource::<AnimEditor>().docs.graph;
        assert_eq!(connect_target(model, &ids, " output "), Some(CanvasNode::Output));
        assert_eq!(
            connect_target(model, &ids, &format!("blend #{}", ids[1])),
            Some(CanvasNode::Node(ids[1]))
        );
        assert_eq!(
            connect_target(model, &ids, &format!("#{}", ids[0])),
            Some(CanvasNode::Node(ids[0]))
        );
        assert_eq!(connect_target(model, &ids, "999"), None, "not on this canvas");
        assert_eq!(connect_target(model, &ids, "Layers"), None);
    }

    #[test]
    fn snarl_mirrors_the_model_and_breadcrumbs_follow_scopes() {
        let mut model = GraphModel::default();
        let sm = model
            .add_node(
                NodeKind::default_of("state_machine").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        model.add_state(sm, "Idle", [0.0, 0.0]).expect("state");
        let blend = model
            .add_node(
                NodeKind::default_of("blend").expect("kind"),
                Scope::Root,
                [200.0, 0.0],
            )
            .expect("add");
        model.connect(sm, blend, "a").expect("connect");
        model.set_output(&Scope::Root, Some(blend)).expect("output");
        let snarl = build_snarl(&model, &Scope::Root, None);
        assert_eq!(snarl.nodes().count(), 3, "two nodes + output");
        assert_eq!(snarl.wires().count(), 2, "sm→blend.a and blend→output");

        let mut ui = AnimUi {
            machine_view: Some(sm),
            ..AnimUi::default()
        };
        let crumbs = breadcrumbs(&model, &ui);
        assert_eq!(crumbs.len(), 2);
        assert_eq!(crumbs[0].0, "Root");
        ui.machine_view = None;
        ui.scope = Some(Scope::State {
            machine: sm,
            state: "Idle".into(),
        });
        let crumbs = breadcrumbs(&model, &ui);
        let labels: Vec<&str> = crumbs.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(labels, vec!["Root", "State Machine #1", "Idle"]);
        assert_eq!(param_kind(&param_default("vec3").expect("kind")), "vec3");
        assert!(node_summary(&model.node(blend).expect("node").kind).starts_with("α"));
    }

    /// P-201 / P-206 keyboard path: with play running, Tab reaches a parameter's field and an
    /// arrow key queues a live write; Tab reaches the recorder's frame slider, an arrow key
    /// scrubs, **Live** returns to the session and **Record** starts a new recording.
    #[test]
    fn live_parameters_and_the_recorder_work_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        use regia_animation::debug::GraphDebug;
        let ctx = egui::Context::default();
        let size = [420.0, 700.0];
        let mut editor = AnimEditor::default();
        editor.docs.graph.set_param("Speed", ParamValue::Float(0.0));
        let pie = super::super::PieDebug {
            entity: 1,
            name: Some("Hero".into()),
            debug: GraphDebug {
                nodes: Vec::new(),
                parameters: vec![("Speed".into(), ParamValue::Float(1.0))],
                ik: Vec::new(),
            },
        };
        editor.pie_debug = Some(pie.clone());
        editor.recorder.start(8);
        for i in 0..3 {
            editor.recorder.push(super::super::recorder::RecordedFrame::new(
                0,
                Some(i),
                &pie,
                Default::default(),
            ));
        }
        editor.recorder.stop();
        let mut pending = Vec::new();
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| parameters_ui(ui, &mut editor, &mut pending));
        };
        frame(&ctx, size, vec![], &mut draw);
        let update = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        tab_to(&ctx, size, "Speed value", false, 30, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowUp, egui::Modifiers::NONE)],
            &mut draw,
        );
        tab_to(&ctx, size, "Recorded frame", false, 30, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowLeft, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        drop(draw);
        let queued = editor.live.queued.clone();
        assert!(
            matches!(queued.as_slice(), [(name, ParamValue::Float(v))] if name == "Speed" && *v > 1.0),
            "{queued:?}"
        );
        assert_eq!(editor.recorder.scrub(), Some(1), "Left stepped one frame back");
        assert!(pending.is_empty(), "a live write is not a document edit");
        let mut draw = |ui: &mut egui::Ui| {
            egui::CentralPanel::default().show(ui, |ui| parameters_ui(ui, &mut editor, &mut pending));
        };
        tab_to(&ctx, size, "Live", false, 30, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Record", true, 30, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert_eq!(editor.recorder.scrub(), None);
        assert!(editor.recorder.is_recording());
        assert!(editor.recorder.frames().is_empty(), "a new recording");
    }
}
