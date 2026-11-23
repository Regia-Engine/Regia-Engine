// SPDX-License-Identifier: MPL-2.0
//! The Conversation panel: toolbar, graph canvas (pan, zoom, node drag, drag-to-retarget),
//! inspector, findings list and preview (ADR-3298).
//!
//! The canvas maps layout units (y down) into the shared pan/zoom view
//! (`crate::anim::view2d::View2d`, y up) by flipping y. Every document change goes through
//! [`super::edit()`] / [`super::move_node`] as an action performed after the draw, so it is one undo
//! step on the shared stack.

use std::collections::BTreeMap;

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use regia_ecs::prelude::*;
use regia_framework::conversation::{Severity, Step};

use crate::anim::view2d::View2d;

use super::edit::{Branch, ConvOp, NewOption, NewStep};
use super::graph::{EdgeKind, NodeKind};
use super::layout::{NODE_H, NODE_W};
use super::preview::PreviewState;
use super::{ConvEditor, edit, move_node};

/// What a canvas drag is doing.
#[derive(Debug, Clone, PartialEq)]
enum Drag {
    /// Panning the view.
    Pan,
    /// Moving a node: its id and the pointer's offset from its corner (layout units).
    Node(String, Vec2),
    /// Dragging a jump to a new target: the step, which branch, the option.
    Retarget(usize, Branch, Option<usize>),
}

/// Panel-only state.
#[derive(Debug, Clone, Default)]
pub struct PanelState {
    path_field: String,
    view: Option<View2d>,
    /// The session the view was fitted for.
    fitted: Option<u64>,
    drag: Option<Drag>,
    /// The egui frame whose input the canvas already used (egui may run a panel twice a frame).
    input_frame: Option<u64>,
    /// Field buffers, keyed by field, valid for `fields_rev`.
    fields: BTreeMap<String, String>,
    fields_rev: u64,
    new_line: (String, String, String),
    new_name: String,
    new_cond: String,
    play_voice: bool,
    discard_armed: Option<u64>,
    /// Centre the view on this node next frame.
    focus: Option<usize>,
    /// The Nodes list beside the canvas: the keyboard route of the canvas (FU-5).
    keys: crate::canvas_keys::GraphKeys,
    /// The node (by id) the Nodes list focus belongs to.
    keys_node: Option<String>,
}

impl PanelState {
    /// Whether the canvas has laid out its view (it drew at least once).
    #[must_use]
    pub fn has_view(&self) -> bool {
        self.view.is_some()
    }
}

enum Action {
    Open(String),
    Save,
    Undo,
    Redo,
    Edit(String, Vec<ConvOp>),
    Move(String, Option<(f32, f32)>),
    Select(Option<String>),
    PreviewFrom(usize),
    PreviewStep(Option<usize>),
    PreviewStop,
}

/// Draws the panel.
pub fn conversation_panel(ui: &mut egui::Ui, world: &mut World) {
    if !world.contains_resource::<ConvEditor>() {
        world.init_resource::<ConvEditor>();
    }
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.next_undo(), s.next_redo()))
        .unwrap_or_default();
    let mut actions = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<ConvEditor>| {
        draw(ui, &mut editor, (undo, redo), &mut actions);
    });
    for action in actions {
        perform(world, action);
    }
}

fn perform(world: &mut World, action: Action) {
    let result: Result<(), String> = match action {
        Action::Open(path) => {
            {
                let mut editor = world.resource_mut::<ConvEditor>();
                if editor.is_open() && editor.dirty() && editor.ui.discard_armed != Some(editor.revision) {
                    editor.ui.discard_armed = Some(editor.revision);
                    editor.toast = "unsaved changes: press Open again to discard them".into();
                    return;
                }
            }
            let full = crate::bt_editor::resolve_path(world, &path);
            let root = crate::bt_editor::project_root(world);
            world.resource_mut::<ConvEditor>().open(&full, root.as_deref())
        }
        Action::Save => world.resource_mut::<ConvEditor>().save(None).map(|_| ()),
        Action::Undo => {
            crate::command::undo_world(world);
            Ok(())
        }
        Action::Redo => {
            crate::command::redo_world(world);
            Ok(())
        }
        Action::Edit(label, ops) => edit(world, &label, None, &ops).map_err(|e| e.to_string()),
        Action::Move(node, pos) => move_node(world, &node, pos).map_err(|e| e.to_string()),
        Action::Select(node) => {
            world.resource_mut::<ConvEditor>().selected = node;
            Ok(())
        }
        Action::PreviewFrom(node) => {
            let mut editor = world.resource_mut::<ConvEditor>();
            let play = editor.ui.play_voice;
            editor.preview_from(node, play);
            Ok(())
        }
        Action::PreviewStep(choose) => {
            let mut editor = world.resource_mut::<ConvEditor>();
            let play = editor.ui.play_voice;
            editor.preview_step(choose, play).map_err(str::to_string)
        }
        Action::PreviewStop => {
            let mut editor = world.resource_mut::<ConvEditor>();
            editor.preview = None;
            editor.preview_voice = None;
            Ok(())
        }
    };
    if let Err(err) = result {
        world.resource_mut::<ConvEditor>().toast = err;
    }
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut ConvEditor,
    (undo, redo): (Option<String>, Option<String>),
    actions: &mut Vec<Action>,
) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    if editor.ui.fields_rev != editor.revision {
        editor.ui.fields.clear();
        editor.ui.fields_rev = editor.revision;
    }
    // Wraps in a narrow dock instead of cutting Redo off (bug 19, ADR-8586).
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.path_field)
                .hint_text("assets/conversations/….conv.ron")
                .desired_width(240.0),
        );
        if ui.button("Open").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Open(editor.ui.path_field.clone()));
        }
        if ui
            .add_enabled(editor.is_open() && !editor.read_only(), egui::Button::new("Save"))
            .clicked()
        {
            actions.push(Action::Save);
        }
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
        ui.label("Search");
        ui.add(egui::TextEdit::singleline(&mut editor.search).desired_width(140.0));
        let hits = editor.search(&editor.search);
        if !editor.search.trim().is_empty() {
            ui.weak(format!("{} node(s)", hits.len()));
            if let Some(&first) = hits.first()
                && ui.small_button("Go").clicked()
            {
                editor.ui.focus = Some(first);
                actions.push(Action::Select(
                    editor.graph.nodes.get(first).map(|n| n.id.clone()),
                ));
            }
        }
        if ui.button("Fit").clicked() {
            editor.ui.fitted = None;
        }
        ui.separator();
        if let Some(path) = &editor.path {
            let name = path.display().to_string();
            ui.weak(if editor.dirty() { format!("{name} *") } else { name });
        }
        let errors = editor
            .issues
            .iter()
            .filter(|i| i.issue.severity == Severity::Error)
            .count();
        let warnings = editor
            .issues
            .iter()
            .filter(|i| i.issue.severity == Severity::Warning)
            .count();
        if editor.is_open() && !editor.read_only() {
            ui.colored_label(
                if errors > 0 {
                    accents.error_text
                } else {
                    accents.ok
                },
                format!("{errors} error(s), {warnings} warning(s)"),
            );
        }
        if !editor.toast.is_empty() {
            ui.weak(&editor.toast);
        }
    });
    ui.separator();
    if !editor.is_open() {
        ui.weak("No conversation is open. Type a .conv.ron path and press Open.");
        return;
    }
    if let Some(error) = &editor.parse_error {
        ui.colored_label(accents.error_text, "Read-only: the file does not parse.");
        ui.monospace(error);
        ui.weak("Fix the file in a text editor, then open it again.");
        return;
    }
    egui::Panel::right("conversation_side")
        .resizable(true)
        .default_size(340.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                inspector_ui(ui, editor, actions);
                ui.separator();
                issues_ui(ui, editor, actions);
                ui.separator();
                preview_ui(ui, editor, actions);
            });
        });
    egui::Panel::left("conversation_nodes")
        .resizable(true)
        .default_size(220.0)
        .show(ui, |ui| nodes_ui(ui, editor, actions));
    egui::CentralPanel::default().show(ui, |ui| canvas_ui(ui, editor, actions));
}

/// The Nodes list row (and accessible) name of node `i`: its label (or id) and first step.
pub(crate) fn node_row_name(editor: &ConvEditor, i: usize) -> String {
    let Some(node) = editor.graph.nodes.get(i) else {
        return String::new();
    };
    let head = node.label.clone().unwrap_or_else(|| node.id.clone());
    match node_rows(editor, i).first() {
        Some(first) => format!("{head}: {}", short(first, 40)),
        None => head,
    }
}

/// The jump of node `from` a **Connect…** form's "From output" names: `goto`, `then`, `else`,
/// `option <n>` (1-based), or empty for the node's first jump. `None`: no such jump.
fn jump_of(editor: &ConvEditor, from: usize, output: &str) -> Option<usize> {
    let output = output.trim().to_lowercase();
    editor.graph.edges.iter().position(|e| {
        e.from == from
            && e.kind != EdgeKind::Next
            && (output.is_empty()
                || match e.kind {
                    EdgeKind::Option => e.option.is_some_and(|o| output == format!("option {}", o + 1)),
                    kind => output == kind.name(),
                })
    })
}

/// The Nodes list beside the canvas: the keyboard route of the canvas (FU-5, the convention of
/// `canvas_keys`). Enter selects a node and centers the view on it (Enter on the selected node
/// previews from it, as a double-click does); Alt+arrow moves it (the drag's `Move`);
/// **Connect…** retargets one of its jumps (`goto`, `then`, `else`, `option <n>`) to a labelled
/// node typed by label or id (the port drag's `retarget`).
fn nodes_ui(ui: &mut egui::Ui, editor: &mut ConvEditor, actions: &mut Vec<Action>) {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let names: Vec<String> = (0..editor.graph.nodes.len())
        .map(|i| node_row_name(editor, i))
        .collect();
    let anchored = editor
        .ui
        .keys_node
        .as_ref()
        .and_then(|id| editor.graph.node_index(id));
    if anchored.is_some() {
        editor.ui.keys.list.retarget(anchored);
    }
    let selected = editor
        .selected
        .as_ref()
        .and_then(|id| editor.graph.node_index(id));
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let kinds = |_: usize, _: &str| Vec::new();
    let rows = GraphRows {
        len: names.len(),
        name: &name,
        selected,
        kinds: &kinds,
        named_outputs: true,
        read_only: editor.read_only(),
        input_pins: false,
        deletable: false,
        heading: "Nodes",
        noun: "node",
    };
    let keyed = graph_keys_ui(ui, "conversation-nodes", &mut editor.ui.keys, &rows);
    editor.ui.keys_node = editor
        .ui
        .keys
        .list
        .current
        .and_then(|i| editor.graph.nodes.get(i).map(|n| n.id.clone()));
    for action in keyed {
        match action {
            GraphAction::Activate(i) => {
                let Some(node) = editor.graph.nodes.get(i) else {
                    continue;
                };
                if editor.selected.as_deref() == Some(node.id.as_str()) {
                    actions.push(Action::PreviewFrom(i));
                } else {
                    editor.ui.focus = Some(i);
                    actions.push(Action::Select(Some(node.id.clone())));
                }
            }
            GraphAction::Nudge(i, delta) => {
                let (Some(node), Some(&(x, y))) = (editor.graph.nodes.get(i), editor.layout.positions.get(i))
                else {
                    continue;
                };
                actions.push(Action::Move(node.id.clone(), Some((x + delta[0], y + delta[1]))));
            }
            GraphAction::Connect(i, form) => {
                let Some(e) = jump_of(editor, i, &form.output) else {
                    editor.toast = format!("that node has no jump {:?}", form.output.trim());
                    continue;
                };
                let target = form.target.trim();
                let to = editor.graph.nodes.iter().find(|n| {
                    n.id == target || n.label.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(target))
                });
                let Some(label) = to.and_then(|n| n.label.clone()) else {
                    editor.toast = "that node has no label to jump to: add a Label step first".into();
                    continue;
                };
                let edge = &editor.graph.edges[e];
                let branch = match edge.kind {
                    EdgeKind::Then => Branch::Then,
                    EdgeKind::Else => Branch::Else,
                    EdgeKind::Option => Branch::Option,
                    EdgeKind::Goto | EdgeKind::Next => Branch::Goto,
                };
                actions.push(Action::Edit(
                    format!("retarget to {label}"),
                    vec![ConvOp::Retarget {
                        at: edge.step,
                        branch,
                        option: edge.option,
                        label,
                    }],
                ));
            }
            GraphAction::Delete(_) | GraphAction::AddAfter(..) => {}
        }
    }
}

fn to_value(p: (f32, f32)) -> Vec2 {
    vec2(p.0, -p.1)
}

fn node_rect(view: &View2d, pos: (f32, f32)) -> Rect {
    let a = view.to_screen(to_value(pos));
    let b = view.to_screen(to_value((pos.0 + NODE_W, pos.1 + NODE_H)));
    Rect::from_two_pos(a, b)
}

/// The node under a screen point.
fn hit_node(editor: &ConvEditor, view: &View2d, p: Pos2) -> Option<usize> {
    (0..editor.graph.nodes.len())
        .rev()
        .find(|&i| node_rect(view, editor.layout.positions[i]).contains(p))
}

/// The out-ports of a node: one per jump edge, along its bottom edge.
fn ports(editor: &ConvEditor, view: &View2d, node: usize) -> Vec<(Pos2, usize)> {
    let rect = node_rect(view, editor.layout.positions[node]);
    let edges: Vec<usize> = editor
        .graph
        .edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.from == node)
        .map(|(k, _)| k)
        .collect();
    let n = edges.len().max(1) as f32;
    edges
        .into_iter()
        .enumerate()
        .map(|(k, e)| {
            let x = rect.left() + rect.width() * (k as f32 + 1.0) / (n + 1.0);
            (pos2(x, rect.bottom()), e)
        })
        .collect()
}

fn edge_color(kind: EdgeKind) -> Color32 {
    match kind {
        EdgeKind::Next => Color32::from_gray(150),
        EdgeKind::Goto => Color32::from_rgb(120, 170, 240),
        EdgeKind::Then => Color32::from_rgb(110, 200, 120),
        EdgeKind::Else => Color32::from_rgb(230, 150, 90),
        EdgeKind::Option => Color32::from_rgb(200, 140, 230),
    }
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

/// The rows a node shows.
fn node_rows(editor: &ConvEditor, node: usize) -> Vec<String> {
    let Some(asset) = &editor.asset else {
        return Vec::new();
    };
    let n = &editor.graph.nodes[node];
    let mut rows = Vec::new();
    for step in &editor.graph.steps[n.first..n.end] {
        rows.push(match step {
            Step::Line(id) => match asset.line(id) {
                Some(l) => {
                    let text = editor.project.text(&l.text).unwrap_or_else(|| l.text.clone());
                    format!("{}: {text}", l.speaker)
                }
                None => format!("missing line `{id}`"),
            },
            Step::Label(_) => continue,
            Step::Goto(l) => format!("goto {l}"),
            Step::If { cond, .. } => format!("if {cond}"),
            Step::Choice { prompt, options } => {
                let mut s = format!("choice ({} options)", options.len());
                if let Some(p) = prompt {
                    s = format!("{s}: {}", editor.project.text(p).unwrap_or_else(|| p.clone()));
                }
                s
            }
            Step::Event(e) => format!("event {e}"),
            Step::End => "end".into(),
        });
    }
    rows
}

fn canvas_ui(ui: &mut egui::Ui, editor: &mut ConvEditor, actions: &mut Vec<Action>) {
    let _span = tracing::debug_span!(target: "regia_editor", "conversation_canvas").entered();
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
    let rect = response.rect;
    painter.rect_filled(rect, 0.0, Color32::from_gray(24));
    let mut view = editor.ui.view.unwrap_or_default();
    view.rect = rect;
    if editor.ui.fitted != Some(editor.session) {
        let b = editor.layout.bounds();
        view.zoom_to_fit([b[0], -b[3], b[2], -b[1]], 0.08);
        let s = view.scale.x.min(view.scale.y).clamp(0.2, 1.5);
        view.scale = vec2(s, s);
        editor.ui.fitted = Some(editor.session);
    }
    if let Some(node) = editor.ui.focus.take()
        && let Some(&(x, y)) = editor.layout.positions.get(node)
    {
        view.position = to_value((x + NODE_W * 0.5, y + NODE_H * 0.5));
    }
    // Input, once per egui frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = editor.ui.input_frame != Some(frame);
    editor.ui.input_frame = Some(frame);
    let pointer = response.interact_pointer_pos().or(response.hover_pos());
    if fresh {
        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0
                && let Some(p) = response.hover_pos()
            {
                let f = (scroll * 0.0015).exp();
                view.zoom_about(p, vec2(f, f));
            }
        }
        // Hit-test where the button went down: by the time a drag registers the pointer has moved.
        if response.drag_started()
            && let Some(p) = ui.ctx().input(|i| i.pointer.press_origin()).or(pointer)
        {
            let port = (0..editor.graph.nodes.len()).find_map(|n| {
                ports(editor, &view, n)
                    .into_iter()
                    .find(|(at, _)| at.distance(p) < 8.0)
                    .map(|(_, e)| e)
            });
            editor.ui.drag = Some(match port {
                Some(e) => {
                    let edge = &editor.graph.edges[e];
                    let branch = match edge.kind {
                        EdgeKind::Goto => Branch::Goto,
                        EdgeKind::Then => Branch::Then,
                        EdgeKind::Else => Branch::Else,
                        EdgeKind::Option => Branch::Option,
                        EdgeKind::Next => Branch::Goto,
                    };
                    if edge.kind == EdgeKind::Next {
                        Drag::Pan
                    } else {
                        Drag::Retarget(edge.step, branch, edge.option)
                    }
                }
                None => match hit_node(editor, &view, p) {
                    Some(n) => {
                        let (x, y) = editor.layout.positions[n];
                        let v = view.to_value(p);
                        Drag::Node(editor.graph.nodes[n].id.clone(), vec2(v.x - x, -v.y - y))
                    }
                    None => Drag::Pan,
                },
            });
        }
        if response.dragged() {
            match &editor.ui.drag {
                Some(Drag::Pan) => view.pan(response.drag_delta()),
                Some(Drag::Node(id, offset)) => {
                    if let Some(p) = pointer
                        && response.drag_delta() != Vec2::ZERO
                    {
                        let v = view.to_value(p);
                        actions.push(Action::Move(
                            id.clone(),
                            Some(((v.x - offset.x).round(), (-v.y - offset.y).round())),
                        ));
                    }
                }
                _ => {}
            }
        }
        if response.drag_stopped() {
            if let (Some(Drag::Retarget(step, branch, option)), Some(p)) = (editor.ui.drag.clone(), pointer) {
                match hit_node(editor, &view, p).map(|n| editor.graph.nodes[n].label.clone()) {
                    Some(Some(label)) => actions.push(Action::Edit(
                        format!("retarget to {label}"),
                        vec![ConvOp::Retarget {
                            at: step,
                            branch,
                            option,
                            label,
                        }],
                    )),
                    Some(None) => {
                        editor.toast = "that node has no label to jump to: add a Label step first".into();
                    }
                    None => {}
                }
            }
            editor.ui.drag = None;
        }
        if response.clicked()
            && let Some(p) = pointer
        {
            let hit = hit_node(editor, &view, p).map(|n| editor.graph.nodes[n].id.clone());
            actions.push(Action::Select(hit));
        }
        if response.double_clicked()
            && let Some(p) = pointer
            && let Some(n) = hit_node(editor, &view, p)
        {
            actions.push(Action::PreviewFrom(n));
        }
    }
    let hits = editor.search(&editor.search);
    let current = editor
        .preview
        .as_ref()
        .and_then(|p| p.state.step())
        .and_then(|s| editor.graph.node_of_step(s));
    let zoom = view.scale.x;
    let font = egui::FontId::proportional((12.0 * zoom).clamp(6.0, 22.0));
    let small = egui::FontId::proportional((10.0 * zoom).clamp(5.0, 18.0));
    // Edges.
    for edge in &editor.graph.edges {
        let outs = ports(editor, &view, edge.from);
        let start = outs
            .iter()
            .find(|(_, e)| editor.graph.edges[*e] == *edge)
            .map_or_else(
                || node_rect(&view, editor.layout.positions[edge.from]).center_bottom(),
                |(p, _)| *p,
            );
        let color = edge_color(edge.kind);
        let end = match edge.to {
            Some(to) => node_rect(&view, editor.layout.positions[to]).center_top(),
            None => start + vec2(0.0, 40.0 * zoom),
        };
        let bend = ((end.y - start.y).abs() * 0.5).max(30.0 * zoom);
        let curve = egui::epaint::CubicBezierShape::from_points_stroke(
            [start, start + vec2(0.0, bend), end - vec2(0.0, bend), end],
            false,
            Color32::TRANSPARENT,
            Stroke::new(
                1.5,
                if edge.to.is_some() {
                    color
                } else {
                    accents.error_text
                },
            ),
        );
        painter.add(curve);
        painter.circle_filled(end, 3.0, color);
        let tag = match edge.kind {
            EdgeKind::Next => String::new(),
            EdgeKind::Option => format!(
                "{}{}",
                edge.option.map_or(String::new(), |o| format!("{}. ", o + 1)),
                edge.cond.as_ref().map_or(String::new(), |c| format!("[{c}]"))
            ),
            EdgeKind::Then | EdgeKind::Else => edge.kind.name().to_string(),
            EdgeKind::Goto => String::new(),
        };
        let tag = if edge.to.is_none() {
            format!("{tag} ? {}", edge.label)
        } else {
            tag
        };
        if !tag.trim().is_empty() {
            painter.text(
                start + vec2(4.0, 10.0 * zoom),
                egui::Align2::LEFT_TOP,
                tag.trim(),
                small.clone(),
                color,
            );
        }
        for (p, _) in outs.iter().filter(|(_, e)| editor.graph.edges[*e] == *edge) {
            painter.circle_stroke(*p, 4.0, Stroke::new(1.5, color));
        }
    }
    // Retarget drag line.
    if let (Some(Drag::Retarget(..)), Some(p)) = (&editor.ui.drag, response.interact_pointer_pos())
        && let Some(origin) = ui.ctx().input(|i| i.pointer.press_origin())
    {
        painter.line_segment([origin, p], Stroke::new(2.0, accents.select));
    }
    // Nodes.
    for (i, node) in editor.graph.nodes.iter().enumerate() {
        let r = node_rect(&view, editor.layout.positions[i]);
        if !r.intersects(rect) {
            continue;
        }
        let worst = editor
            .issues
            .iter()
            .filter(|f| f.node == Some(i))
            .map(|f| f.issue.severity)
            .max();
        let fill = match (current == Some(i), node.kind) {
            (true, _) => Color32::from_rgb(36, 70, 44),
            (_, NodeKind::End) => Color32::from_rgb(56, 36, 36),
            (_, NodeKind::Choice) => Color32::from_rgb(48, 40, 60),
            (_, NodeKind::If) => Color32::from_rgb(40, 50, 40),
            (_, NodeKind::Block) => Color32::from_gray(44),
        };
        painter.rect_filled(r, 4.0, fill);
        let border = if editor.selected.as_deref() == Some(node.id.as_str()) {
            Stroke::new(2.5, accents.select)
        } else if hits.contains(&i) {
            Stroke::new(2.0, accents.active)
        } else {
            match worst {
                Some(Severity::Error) => Stroke::new(2.0, accents.log_error),
                Some(Severity::Warning) => Stroke::new(1.5, accents.log_warn),
                _ => Stroke::new(1.0, Color32::from_gray(90)),
            }
        };
        painter.rect_stroke(r, 4.0, border, StrokeKind::Inside);
        let title = match (&node.label, node.kind) {
            (Some(l), _) => l.clone(),
            (None, kind) => kind.name().to_string(),
        };
        let pad = 6.0 * zoom;
        painter.text(
            r.left_top() + vec2(pad, pad * 0.6),
            egui::Align2::LEFT_TOP,
            short(&title, 28),
            font.clone(),
            Color32::WHITE,
        );
        if i == 0 {
            painter.text(
                r.right_top() + vec2(-pad, pad * 0.6),
                egui::Align2::RIGHT_TOP,
                "entry",
                small.clone(),
                accents.ok,
            );
        }
        let rows = node_rows(editor, i);
        let row_h = 14.0 * zoom;
        let fit = (((r.height() - 22.0 * zoom) / row_h).floor().max(0.0)) as usize;
        for (k, row) in rows.iter().take(fit.max(1)).enumerate() {
            let text = if k + 1 == fit && rows.len() > fit {
                format!("… +{} more", rows.len() - k)
            } else {
                short(row, 34)
            };
            painter.text(
                r.left_top() + vec2(pad, 22.0 * zoom + k as f32 * row_h),
                egui::Align2::LEFT_TOP,
                text,
                small.clone(),
                Color32::from_gray(205),
            );
        }
        let count = editor.issues.iter().filter(|f| f.node == Some(i)).count();
        if count > 0 {
            let color = if worst == Some(Severity::Error) {
                accents.log_error
            } else {
                accents.log_warn
            };
            painter.circle_filled(r.right_bottom() + vec2(-10.0, -10.0), 8.0, color);
            painter.text(
                r.right_bottom() + vec2(-10.0, -10.0),
                egui::Align2::CENTER_CENTER,
                count.to_string(),
                egui::FontId::proportional(10.0),
                Color32::BLACK,
            );
        }
    }
    // The Nodes list's keyboard focus (FU-5).
    if let Some(&pos) = editor
        .ui
        .keys
        .list
        .focused()
        .and_then(|i| editor.layout.positions.get(i))
    {
        crate::canvas_keys::focus_ring(&painter, node_rect(&view, pos));
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("Conversation canvas, {} nodes", editor.graph.nodes.len()),
    );
    painter.text(
        rect.left_bottom() + vec2(6.0, -6.0),
        egui::Align2::LEFT_BOTTOM,
        "drag: pan / move node · drag a port onto a labelled node: retarget · wheel: zoom · double-click: preview from node",
        egui::FontId::proportional(10.0),
        Color32::from_gray(120),
    );
    editor.ui.view = Some(view);
}

/// A text field over a buffer; returns the new text when the edit is committed (focus lost or
/// Enter) and differs from `current`.
fn field(ui: &mut egui::Ui, state: &mut PanelState, key: &str, current: &str, width: f32) -> Option<String> {
    let buf = state
        .fields
        .entry(key.to_string())
        .or_insert_with(|| current.to_string());
    let response = ui.add(egui::TextEdit::singleline(buf).desired_width(width));
    if response.lost_focus() && *buf != current {
        Some(buf.clone())
    } else {
        None
    }
}

fn label_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    labels: &[String],
    current: &str,
) -> Option<String> {
    let mut picked = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(current)
        .width(120.0)
        .show_ui(ui, |ui| {
            for l in labels {
                if ui.selectable_label(l == current, l.as_str()).clicked() && l != current {
                    picked = Some(l.clone());
                }
            }
        });
    picked
}

fn inspector_ui(ui: &mut egui::Ui, editor: &mut ConvEditor, actions: &mut Vec<Action>) {
    ui.heading("Node");
    let Some(asset) = editor.asset.clone() else {
        return;
    };
    let Some(index) = editor
        .selected
        .as_deref()
        .and_then(|s| editor.graph.node_index(s))
    else {
        ui.weak("Click a node to edit it.");
        return;
    };
    let node = editor.graph.nodes[index].clone();
    let labels: Vec<String> = asset.labels().keys().cloned().collect();
    ui.horizontal(|ui| {
        ui.strong(&node.id);
        if ui.small_button("Preview from here").clicked() {
            actions.push(Action::PreviewFrom(index));
        }
        if editor.layout.pinned.get(index).copied().unwrap_or(false)
            && ui.small_button("Auto place").clicked()
        {
            actions.push(Action::Move(node.id.clone(), None));
        }
    });
    let st = &mut editor.ui;
    let steps = editor.graph.steps.len();
    for s in node.first..node.end {
        let step = editor.graph.steps[s].clone();
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.weak(format!("#{s}"));
                if s > 0 && ui.small_button("Up").clicked() {
                    actions.push(Action::Edit(
                        "move step".into(),
                        vec![ConvOp::MoveStep { from: s, to: s - 1 }],
                    ));
                }
                if s + 1 < steps && ui.small_button("Down").clicked() {
                    actions.push(Action::Edit(
                        "move step".into(),
                        vec![ConvOp::MoveStep { from: s, to: s + 1 }],
                    ));
                }
                if ui.small_button("Delete").clicked() {
                    actions.push(Action::Edit(
                        "delete step".into(),
                        vec![ConvOp::DeleteStep { at: s }],
                    ));
                }
            });
            match &step {
                Step::Line(id) => {
                    let Some(line) = asset.line(id) else {
                        ui.colored_label(Color32::LIGHT_RED, format!("line `{id}` does not exist"));
                        return;
                    };
                    ui.horizontal(|ui| {
                        ui.label("Speaker");
                        if let Some(v) = field(ui, st, &format!("{s}:speaker"), &line.speaker, 120.0) {
                            actions.push(Action::Edit(
                                "edit speaker".into(),
                                vec![ConvOp::SetLine {
                                    line: id.clone(),
                                    text: None,
                                    speaker: Some(v),
                                    note: None,
                                    emotion: None,
                                }],
                            ));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Text key");
                        if let Some(v) = field(ui, st, &format!("{s}:text"), &line.text, 180.0) {
                            actions.push(Action::Edit(
                                "edit line text".into(),
                                vec![ConvOp::SetLine {
                                    line: id.clone(),
                                    text: Some(v),
                                    speaker: None,
                                    note: None,
                                    emotion: None,
                                }],
                            ));
                        }
                    });
                    match editor.project.text(&line.text) {
                        Some(t) => ui.label(egui::RichText::new(t).italics()),
                        None => ui.weak("(no text for this key in the source locale)"),
                    };
                    ui.horizontal(|ui| {
                        ui.label("Note");
                        if let Some(v) = field(ui, st, &format!("{s}:note"), &line.note, 200.0) {
                            actions.push(Action::Edit(
                                "edit note".into(),
                                vec![ConvOp::SetLine {
                                    line: id.clone(),
                                    text: None,
                                    speaker: None,
                                    note: Some(v),
                                    emotion: None,
                                }],
                            ));
                        }
                    });
                }
                Step::Label(name) => {
                    ui.horizontal(|ui| {
                        ui.label("Label");
                        if let Some(v) = field(ui, st, &format!("{s}:label"), name, 160.0) {
                            actions.push(Action::Edit(
                                format!("rename label {name}"),
                                vec![ConvOp::RenameLabel {
                                    from: name.clone(),
                                    to: v,
                                }],
                            ));
                        }
                    });
                }
                Step::Goto(label) => {
                    ui.horizontal(|ui| {
                        ui.label("Goto");
                        if let Some(l) = label_combo(ui, ("goto", s), &labels, label) {
                            actions.push(Action::Edit(
                                "retarget goto".into(),
                                vec![ConvOp::Retarget {
                                    at: s,
                                    branch: Branch::Goto,
                                    option: None,
                                    label: l,
                                }],
                            ));
                        }
                    });
                }
                Step::If { cond, then, else_ } => {
                    ui.horizontal(|ui| {
                        ui.label("If");
                        if let Some(v) = field(ui, st, &format!("{s}:cond"), cond, 140.0) {
                            actions.push(Action::Edit(
                                "edit condition".into(),
                                vec![ConvOp::SetCond { at: s, cond: v }],
                            ));
                        }
                    });
                    for (branch, current) in [(Branch::Then, then), (Branch::Else, else_)] {
                        ui.horizontal(|ui| {
                            ui.label(if branch == Branch::Then { "then" } else { "else" });
                            if let Some(l) =
                                label_combo(ui, ("if", s, branch == Branch::Then), &labels, current)
                            {
                                actions.push(Action::Edit(
                                    "retarget branch".into(),
                                    vec![ConvOp::Retarget {
                                        at: s,
                                        branch,
                                        option: None,
                                        label: l,
                                    }],
                                ));
                            }
                        });
                    }
                }
                Step::Choice { prompt, options } => {
                    ui.horizontal(|ui| {
                        ui.label("Prompt key");
                        let p = prompt.clone().unwrap_or_default();
                        if let Some(v) = field(ui, st, &format!("{s}:prompt"), &p, 160.0) {
                            actions.push(Action::Edit(
                                "edit prompt".into(),
                                vec![ConvOp::SetPrompt { at: s, prompt: v }],
                            ));
                        }
                    });
                    if options.is_empty() {
                        ui.colored_label(Color32::LIGHT_RED, "no options: the run ends here");
                    }
                    for (k, o) in options.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.weak(format!("{}.", k + 1));
                            if let Some(v) = field(ui, st, &format!("{s}:opt{k}:text"), &o.text, 110.0) {
                                actions.push(Action::Edit(
                                    "edit option".into(),
                                    vec![ConvOp::SetOption {
                                        at: s,
                                        option: k,
                                        text: Some(v),
                                        cond: None,
                                        goto: None,
                                    }],
                                ));
                            }
                            let cond = o.cond.clone().unwrap_or_default();
                            if let Some(v) = field(ui, st, &format!("{s}:opt{k}:cond"), &cond, 70.0) {
                                actions.push(Action::Edit(
                                    "edit option condition".into(),
                                    vec![ConvOp::SetOption {
                                        at: s,
                                        option: k,
                                        text: None,
                                        cond: Some(v),
                                        goto: None,
                                    }],
                                ));
                            }
                            if let Some(l) = label_combo(ui, ("opt", s, k), &labels, &o.goto) {
                                actions.push(Action::Edit(
                                    "retarget option".into(),
                                    vec![ConvOp::Retarget {
                                        at: s,
                                        branch: Branch::Option,
                                        option: Some(k),
                                        label: l,
                                    }],
                                ));
                            }
                            if k > 0 && ui.small_button("Up").clicked() {
                                actions.push(Action::Edit(
                                    "move option".into(),
                                    vec![ConvOp::MoveOption {
                                        at: s,
                                        from: k,
                                        to: k - 1,
                                    }],
                                ));
                            }
                            if ui.small_button("x").on_hover_text("Delete the option").clicked() {
                                actions.push(Action::Edit(
                                    "delete option".into(),
                                    vec![ConvOp::DeleteOption { at: s, option: k }],
                                ));
                            }
                        });
                    }
                    if ui.small_button("Add option").clicked() {
                        let goto = labels.first().cloned().unwrap_or_else(|| "end".into());
                        actions.push(Action::Edit(
                            "add option".into(),
                            vec![ConvOp::AddOption {
                                at: s,
                                text: format!("{}-option-{}", asset.id, options.len() + 1),
                                cond: None,
                                goto,
                            }],
                        ));
                    }
                }
                Step::Event(name) => {
                    ui.horizontal(|ui| {
                        ui.label("Event");
                        if let Some(v) = field(ui, st, &format!("{s}:event"), name, 160.0) {
                            actions.push(Action::Edit(
                                "edit event".into(),
                                vec![ConvOp::SetEvent { at: s, name: v }],
                            ));
                        }
                    });
                }
                Step::End => {
                    ui.label("End");
                }
            }
        });
    }
    // Insert after the node's last step.
    let at = Some(node.end);
    ui.label("Insert after this node:");
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.new_line.0)
                .hint_text("line id")
                .desired_width(70.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut st.new_line.1)
                .hint_text("speaker")
                .desired_width(60.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut st.new_line.2)
                .hint_text("text key")
                .desired_width(90.0),
        );
        if ui.small_button("Line").clicked() {
            let (id, speaker, text) = std::mem::take(&mut st.new_line);
            actions.push(Action::Edit(
                "add line".into(),
                vec![ConvOp::AddLine {
                    id,
                    speaker,
                    text,
                    at,
                }],
            ));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.new_name)
                .hint_text("label / event / condition")
                .desired_width(120.0),
        );
        let name = st.new_name.trim().to_string();
        let first_label = labels.first().cloned().unwrap_or_default();
        let mut add = |ui: &mut egui::Ui, title: &str, step: NewStep| {
            if ui.small_button(title).clicked() {
                actions.push(Action::Edit(
                    format!("add {title}"),
                    vec![ConvOp::AddStep { step, at }],
                ));
            }
        };
        add(ui, "Label", NewStep::Label { name: name.clone() });
        add(
            ui,
            "Goto",
            NewStep::Goto {
                label: if name.is_empty() {
                    first_label.clone()
                } else {
                    name.clone()
                },
            },
        );
        add(
            ui,
            "If",
            NewStep::If {
                cond: name.clone(),
                then: first_label.clone(),
                else_: first_label.clone(),
            },
        );
        add(
            ui,
            "Choice",
            NewStep::Choice {
                prompt: None,
                options: vec![NewOption {
                    text: format!("{}-option-1", asset.id),
                    cond: None,
                    goto: first_label.clone(),
                }],
            },
        );
        add(ui, "Event", NewStep::Event { name: name.clone() });
        add(ui, "End", NewStep::End);
    });
}

fn issues_ui(ui: &mut egui::Ui, editor: &mut ConvEditor, actions: &mut Vec<Action>) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    ui.heading(format!("Findings ({})", editor.issues.len()));
    for w in &editor.project.warnings {
        ui.colored_label(accents.log_warn, w.as_str());
    }
    if editor.issues.is_empty() {
        ui.weak("No findings.");
    }
    let mut focus = None;
    for f in &editor.issues {
        let color = match f.issue.severity {
            Severity::Error => accents.log_error,
            Severity::Warning => accents.log_warn,
            Severity::Info => accents.log_info,
        };
        let locale = if f.issue.locale.is_empty() {
            String::new()
        } else {
            format!(" [{}]", f.issue.locale)
        };
        let text = egui::RichText::new(format!("{}{locale}: {}", f.issue.code, f.issue.message)).color(color);
        let response = ui.add(egui::Label::new(text).sense(Sense::click()));
        let response = response.on_hover_text(f.issue.fix.as_str());
        if response.clicked()
            && let Some(n) = f.node
        {
            focus = Some(n);
        }
    }
    if let Some(n) = focus {
        editor.ui.focus = Some(n);
        actions.push(Action::Select(editor.graph.nodes.get(n).map(|n| n.id.clone())));
    }
}

fn preview_ui(ui: &mut egui::Ui, editor: &mut ConvEditor, actions: &mut Vec<Action>) {
    ui.heading("Preview");
    // The condition table: every condition the asset names, plus any added by hand.
    let mut names: Vec<String> = editor
        .graph
        .steps
        .iter()
        .flat_map(|s| match s {
            Step::If { cond, .. } => vec![cond.clone()],
            Step::Choice { options, .. } => options.iter().filter_map(|o| o.cond.clone()).collect(),
            _ => Vec::new(),
        })
        .collect();
    names.extend(editor.conditions.keys().cloned());
    names.sort();
    names.dedup();
    egui::Grid::new("conversation_conditions").show(ui, |ui| {
        for name in &names {
            let mut on = editor.conditions.get(name).copied().unwrap_or(false);
            ui.label(name.as_str());
            if ui.checkbox(&mut on, "true").changed() {
                editor.conditions.insert(name.clone(), on);
            }
            ui.end_row();
        }
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.new_cond)
                .hint_text("condition")
                .desired_width(110.0),
        );
        if ui.small_button("Add").clicked() && !editor.ui.new_cond.trim().is_empty() {
            let name = std::mem::take(&mut editor.ui.new_cond).trim().to_string();
            editor.conditions.insert(name, true);
        }
        ui.checkbox(&mut editor.ui.play_voice, "Play voice");
    });
    ui.horizontal(|ui| {
        if ui.button("Start").clicked() {
            actions.push(Action::PreviewFrom(0));
        }
        let selected = editor
            .selected
            .as_deref()
            .and_then(|s| editor.graph.node_index(s));
        if ui
            .add_enabled(selected.is_some(), egui::Button::new("From selected"))
            .clicked()
            && let Some(n) = selected
        {
            actions.push(Action::PreviewFrom(n));
        }
        if editor.preview.is_some() && ui.button("Stop").clicked() {
            actions.push(Action::PreviewStop);
        }
    });
    let Some(preview) = &editor.preview else {
        ui.weak("Not running. Start, or double-click a node.");
        return;
    };
    match &preview.state {
        PreviewState::Line { line, .. } => {
            if let Some(l) = editor.asset.as_ref().and_then(|a| a.line(line)) {
                let text = editor.project.text(&l.text).unwrap_or_else(|| l.text.clone());
                ui.label(egui::RichText::new(format!("{}: {text}", l.speaker)).strong());
            }
            match &editor.preview_voice {
                Some((clip, true)) => ui.weak(format!("voice: {clip} (playing)")),
                Some((clip, false)) => ui.weak(format!("voice: {clip}")),
                None => ui.weak("voice: none"),
            };
            if ui.button("Next").clicked() {
                actions.push(Action::PreviewStep(None));
            }
        }
        PreviewState::Choice { offered, .. } => {
            for (k, (_, text, goto)) in offered.iter().enumerate() {
                let shown = editor.project.text(text).unwrap_or_else(|| text.clone());
                if ui.button(format!("{}. {shown}  → {goto}", k + 1)).clicked() {
                    actions.push(Action::PreviewStep(Some(k)));
                }
            }
        }
        PreviewState::Finished { reason, detail } => {
            ui.label(format!("finished: {reason}"));
            if let Some(d) = detail {
                ui.weak(d);
            }
        }
    }
    if !preview.events.is_empty() {
        ui.weak(format!("events: {}", preview.events.join(", ")));
    }
    if let Some(node) = preview.state.step().and_then(|s| editor.graph.node_of_step(s))
        && ui.small_button("Show node").clicked()
    {
        editor.ui.focus = Some(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONV: &str = r#"(
    id: "drag",
    lines: [
        (id: "a", speaker: "s", text: "drag-a"),
        (id: "b", speaker: "s", text: "drag-b"),
        (id: "c", speaker: "s", text: "drag-c"),
    ],
    steps: [Line("a"), Goto("one"), Label("one"), Line("b"), End, Label("two"), Line("c"), End],
)
"#;

    fn frame(world: &mut World, ctx: &egui::Context, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| conversation_panel(ui, world));
        output.textures_delta.clear();
    }

    fn button(pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Presses at `from`, moves to `to` in steps over several frames, releases.
    fn drag(world: &mut World, ctx: &egui::Context, from: Pos2, to: Pos2) {
        frame(world, ctx, vec![egui::Event::PointerMoved(from)]);
        frame(world, ctx, vec![button(from, true)]);
        for k in 1..=4 {
            let p = from + (to - from) * (k as f32 / 4.0);
            frame(world, ctx, vec![egui::Event::PointerMoved(p)]);
        }
        frame(world, ctx, vec![button(to, false)]);
        frame(world, ctx, vec![]);
    }

    fn setup(tag: &str) -> (World, egui::Context, std::path::PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let path = dir.join("assets/conversations/drag.conv.ron");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, CONV).expect("write");
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world
            .get_resource_or_insert_with(ConvEditor::default)
            .open(&path, Some(&dir))
            .expect("opens");
        let ctx = egui::Context::default();
        frame(&mut world, &ctx, vec![]);
        frame(&mut world, &ctx, vec![]);
        (world, ctx, dir)
    }

    #[test]
    fn dragging_a_goto_port_onto_a_labelled_node_retargets_it() {
        let (mut world, ctx, dir) = setup("conv-panel-retarget");
        let (from, to) = {
            let editor = world.resource::<ConvEditor>();
            let view = editor.ui.view.expect("a view");
            let source = editor.graph.node_index("line:a").expect("node");
            let target = editor.graph.node_index("label:two").expect("node");
            let port = ports(editor, &view, source)
                .into_iter()
                .find(|(_, e)| editor.graph.edges[*e].kind == EdgeKind::Goto)
                .map(|(p, _)| p)
                .expect("a goto port");
            (port, node_rect(&view, editor.layout.positions[target]).center())
        };
        drag(&mut world, &ctx, from, to);
        let editor = world.resource::<ConvEditor>();
        let asset = editor.asset.as_ref().expect("asset");
        assert_eq!(asset.steps[1], Step::Goto("two".into()), "{}", editor.toast);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 1);
        crate::command::undo_world(&mut world);
        assert_eq!(world.resource::<ConvEditor>().text(), CONV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dragging_a_node_pins_it_as_one_undo_step() {
        let (mut world, ctx, dir) = setup("conv-panel-move");
        let (from, before) = {
            let editor = world.resource::<ConvEditor>();
            let view = editor.ui.view.expect("a view");
            let n = editor.graph.node_index("label:two").expect("node");
            let r = node_rect(&view, editor.layout.positions[n]);
            (r.left_top() + vec2(20.0, 30.0), editor.layout.positions[n])
        };
        drag(&mut world, &ctx, from, from + vec2(80.0, 40.0));
        let editor = world.resource::<ConvEditor>();
        let n = editor.graph.node_index("label:two").expect("node");
        assert!(editor.layout.pinned[n]);
        assert_ne!(editor.layout.positions[n], before);
        assert_eq!(editor.text(), CONV, "a move never touches the file");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            1,
            "one drag, one step"
        );
        crate::command::undo_world(&mut world);
        assert!(!world.resource::<ConvEditor>().layout.pinned[n]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
