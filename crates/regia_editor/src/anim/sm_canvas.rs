// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// editor/src/plugins/absm/{transition,canvas}.rs. Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors,
// MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! The state machine canvas: states as boxes, transitions as arrows, an "Any state" box for
//! transitions with no source; drag to move, double-click to open a state's sub-graph, right
//! click for menus. The active state and transition (preview or PIE) are highlighted.
//!
//! Portions ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>),
//! `editor/src/plugins/absm/transition.rs` (`draw_transition`: a line with a filled arrow head
//! at the midpoint) and `canvas.rs` (state boxes on a pannable canvas), Copyright (c)
//! 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence (notice in
//! `docs/ported-sources.toml`). Adapted to an egui painter; bidirectional pairs are offset
//! sideways so both arrows stay visible, and arrows end at box borders.

use regia_animation::state_machine::{TransitionCurve, TransitionDef};

use super::model::{MachineModel, NodeId, Scope};
use super::{AnimEditor, AnimUi, PendingEdit};

/// Size of a state box, points.
pub const STATE_SIZE: egui::Vec2 = egui::vec2(120.0, 44.0);

/// Where the "Any state" pseudo-box sits on the canvas.
pub const ANY_POS: [f32; 2] = [-320.0, -200.0];

/// Live highlight for the canvas: the current state and the transition in progress.
#[derive(Clone, Debug, Default)]
pub struct MachineActivity {
    /// Current state.
    pub current: Option<String>,
    /// `(from, to, progress)` of a transition in progress.
    pub transition: Option<(String, String, f32)>,
}

/// Draws an arrow `from → to` with the head at the midpoint (Fyrox's `draw_transition`).
pub fn draw_transition(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, stroke: egui::Stroke) {
    painter.line_segment([from, to], stroke);
    let axis = (to - from).normalized();
    if !axis.is_finite() {
        return;
    }
    let centre = from + (to - from) * 0.5;
    let perp = egui::vec2(axis.y, -axis.x);
    let size = 7.0 + stroke.width * 2.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            centre + axis * size,
            centre + perp * size * 0.5,
            centre - perp * size * 0.5,
        ],
        stroke.color,
        egui::Stroke::NONE,
    ));
}

/// The point where the ray from a box centre towards `towards` leaves the box.
fn box_exit(centre: egui::Pos2, towards: egui::Pos2, half: egui::Vec2) -> egui::Pos2 {
    let d = towards - centre;
    if d.length() < 1e-3 {
        return centre;
    }
    let tx = if d.x.abs() > 1e-6 {
        half.x / d.x.abs()
    } else {
        f32::MAX
    };
    let ty = if d.y.abs() > 1e-6 {
        half.y / d.y.abs()
    } else {
        f32::MAX
    };
    centre + d * tx.min(ty).min(1.0)
}

/// Distance from `p` to the segment `a..b`.
#[must_use]
pub fn segment_distance(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 {
        ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance(a + ab * t)
}

/// The screen segment of transition `t`, or `None` when a state is missing.
fn transition_segment(
    machine: &MachineModel,
    t: &TransitionDef,
    to_screen: &dyn Fn([f32; 2]) -> egui::Pos2,
) -> Option<(egui::Pos2, egui::Pos2)> {
    let centre = |pos: [f32; 2]| to_screen(pos) + STATE_SIZE * 0.5;
    let from = match &t.from {
        Some(name) => centre(machine.states.iter().find(|s| &s.name == name)?.pos),
        None => centre(ANY_POS),
    };
    let to = centre(machine.states.iter().find(|s| s.name == t.to)?.pos);
    // Offset pairs sideways so A→B and B→A do not overlap.
    let axis = (to - from).normalized();
    let offset = if axis.is_finite() {
        egui::vec2(axis.y, -axis.x) * 7.0
    } else {
        egui::Vec2::ZERO
    };
    let half = STATE_SIZE * 0.5;
    Some((
        box_exit(from, to, half) + offset,
        box_exit(to, from, half) + offset,
    ))
}

/// Draws the machine canvas and collects edits; updates `ui_state` selection and navigation.
pub fn machine_canvas(
    ui: &mut egui::Ui,
    machine_id: NodeId,
    machine: &MachineModel,
    activity: &MachineActivity,
    ui_state: &mut AnimUi,
    pending: &mut Vec<PendingEdit>,
) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(24));
    let pan_id = egui::Id::new(("anim-sm-pan", machine_id));
    let mut pan: egui::Vec2 = ui.data(|d| d.get_temp(pan_id)).unwrap_or(egui::Vec2::ZERO);
    let origin = rect.center() + pan;
    let to_screen = |pos: [f32; 2]| origin + egui::vec2(pos[0], pos[1]);
    let to_canvas = |p: egui::Pos2| {
        let v = p - origin;
        [v.x, v.y]
    };
    // Background grid.
    let grid = egui::Stroke::new(1.0, egui::Color32::from_gray(34));
    let step = 40.0;
    let mut x = rect.left() + (origin.x - rect.left()).rem_euclid(step);
    while x < rect.right() {
        painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], grid);
        x += step;
    }
    let mut y = rect.top() + (origin.y - rect.top()).rem_euclid(step);
    while y < rect.bottom() {
        painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], grid);
        y += step;
    }

    // Transitions under the boxes.
    let pointer = response.hover_pos();
    let mut hovered_transition = None;
    for (index, t) in machine.transitions.iter().enumerate() {
        let Some((a, b)) = transition_segment(machine, t, &to_screen) else {
            continue;
        };
        let active = activity
            .transition
            .as_ref()
            .is_some_and(|(from, to, _)| t.to == *to && t.from.as_deref().is_none_or(|f| f == from));
        let selected = ui_state.selected_transition == Some(index);
        let near = pointer.is_some_and(|p| segment_distance(p, a, b) < 6.0);
        if near {
            hovered_transition = Some(index);
        }
        let colour = if active {
            crate::theme::ui_tokens(painter.ctx()).accents.active
        } else if selected {
            crate::theme::ui_tokens(painter.ctx()).accents.select
        } else if near {
            egui::Color32::from_gray(230)
        } else {
            egui::Color32::from_gray(150)
        };
        draw_transition(
            &painter,
            a,
            b,
            egui::Stroke::new(if active || selected { 3.0 } else { 2.0 }, colour),
        );
        if t.blend.is_inertial() {
            // Inertialized transitions are labelled on the arrow (the cross-fade is unlabelled).
            painter.text(
                a + (b - a) * 0.5 + egui::vec2(0.0, -6.0),
                egui::Align2::CENTER_BOTTOM,
                if t.blend == regia_animation::state_machine::TransitionBlend::Inertialize {
                    "inertial"
                } else {
                    "inertial~"
                },
                egui::FontId::proportional(10.0),
                colour,
            );
        }
        if let Some((_, _, progress)) = activity.transition.as_ref().filter(|_| active) {
            painter.circle_filled(a + (b - a) * *progress, 4.0, colour);
        }
    }

    // The States list's keyboard focus (FU-5).
    let focused = ui_state
        .state_keys
        .list
        .focused()
        .and_then(|row| machine_rows(machine).get(row).cloned());
    if let Some(SmRow::Transition(index)) = &focused
        && let Some((a, b)) = machine
            .transitions
            .get(*index)
            .and_then(|t| transition_segment(machine, t, &to_screen))
    {
        painter.circle_stroke(
            a + (b - a) * 0.5,
            12.0,
            egui::Stroke::new(
                crate::canvas_keys::FOCUS_RING_WIDTH,
                crate::canvas_keys::focus_color(painter.ctx()),
            ),
        );
    }

    // State boxes.
    let mut hovered_state: Option<String> = None;
    let font = egui::FontId::proportional(13.0);
    let any_rect = egui::Rect::from_min_size(to_screen(ANY_POS), STATE_SIZE);
    painter.rect(
        any_rect,
        8.0,
        egui::Color32::from_gray(45),
        egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        any_rect.center(),
        egui::Align2::CENTER_CENTER,
        "Any state",
        font.clone(),
        egui::Color32::from_gray(170),
    );
    if focused == Some(SmRow::Any) {
        crate::canvas_keys::focus_ring(&painter, any_rect);
    }
    for state in &machine.states {
        let r = egui::Rect::from_min_size(to_screen(state.pos), STATE_SIZE);
        let current = activity.current.as_deref() == Some(state.name.as_str());
        let selected = ui_state.selected_state.as_deref() == Some(state.name.as_str());
        if pointer.is_some_and(|p| r.contains(p)) {
            hovered_state = Some(state.name.clone());
        }
        let fill = if current {
            egui::Color32::from_rgb(150, 80, 20)
        } else {
            egui::Color32::from_rgb(48, 56, 70)
        };
        let stroke = if selected {
            egui::Stroke::new(2.5, crate::theme::ui_tokens(painter.ctx()).accents.select)
        } else {
            egui::Stroke::new(1.0, egui::Color32::from_gray(120))
        };
        painter.rect(r, 8.0, fill, stroke, egui::StrokeKind::Inside);
        if machine.initial == state.name {
            painter.rect_stroke(
                r.expand(3.0),
                10.0,
                egui::Stroke::new(1.5, crate::theme::ui_tokens(painter.ctx()).accents.ok),
                egui::StrokeKind::Outside,
            );
        }
        let label = if state.output.is_none() {
            format!("{} (empty)", state.name)
        } else {
            state.name.clone()
        };
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            label,
            font.clone(),
            egui::Color32::WHITE,
        );
        if focused.as_ref() == Some(&SmRow::State(state.name.clone())) {
            crate::canvas_keys::focus_ring(&painter, r);
        }
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!(
            "State machine canvas, {} states, {} transitions",
            machine.states.len(),
            machine.transitions.len()
        ),
    );

    // Interaction: drag a state (or pan), click to select, double-click to open.
    let drag_id = egui::Id::new(("anim-sm-drag", machine_id));
    if response.drag_started() {
        let grabbed = hovered_state.clone();
        ui.data_mut(|d| d.insert_temp(drag_id, grabbed));
    }
    let dragging: Option<String> = ui.data(|d| d.get_temp::<Option<String>>(drag_id)).flatten();
    if response.dragged() {
        let delta = response.drag_delta();
        match &dragging {
            Some(name) => {
                if let Some(state) = machine.states.iter().find(|s| &s.name == name) {
                    let pos = [state.pos[0] + delta.x, state.pos[1] + delta.y];
                    let state_name = name.clone();
                    pending.push(PendingEdit::new(
                        format!("move state {name}"),
                        Some(format!("move-state:{machine_id}:{name}")),
                        move |docs| {
                            let m = docs.graph.machine_mut(machine_id)?;
                            if let Some(s) = m.states.iter_mut().find(|s| s.name == state_name) {
                                s.pos = pos;
                            }
                            Ok(())
                        },
                    ));
                }
            }
            None => pan += delta,
        }
    }
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Option<String>>(drag_id));
    }
    ui.data_mut(|d| d.insert_temp(pan_id, pan));
    if response.clicked() {
        ui_state.selected_state = hovered_state.clone();
        ui_state.selected_transition = if hovered_state.is_none() {
            hovered_transition
        } else {
            None
        };
        ui_state.selected_node = Some(machine_id);
    }
    if response.double_clicked()
        && let Some(name) = &hovered_state
    {
        ui_state.scope = Some(Scope::State {
            machine: machine_id,
            state: name.clone(),
        });
        ui_state.machine_view = None;
    }
    let menu_state = hovered_state.clone();
    let click_pos = pointer.map(to_canvas);
    let states: Vec<String> = machine.states.iter().map(|s| s.name.clone()).collect();
    let selected_transition = hovered_transition;
    response.context_menu(|ui| {
        if let Some(name) = &menu_state {
            ui.label(format!("State {name}"));
            ui.menu_button("Add transition to", |ui| {
                for target in &states {
                    if ui.button(target).clicked() {
                        pending.push(add_transition_edit(
                            machine_id,
                            Some(name.clone()),
                            target.clone(),
                        ));
                        ui.close();
                    }
                }
            });
            if ui.button("Set as initial").clicked() {
                let n = name.clone();
                pending.push(PendingEdit::new("set initial state", None, move |docs| {
                    docs.graph.machine_mut(machine_id)?.initial = n;
                    Ok(())
                }));
                ui.close();
            }
            if ui.button("Open sub-graph").clicked() {
                ui_state.scope = Some(Scope::State {
                    machine: machine_id,
                    state: name.clone(),
                });
                ui_state.machine_view = None;
                ui.close();
            }
            if ui.button("Delete state").clicked() {
                let n = name.clone();
                pending.push(PendingEdit::new(format!("delete state {n}"), None, move |docs| {
                    docs.graph.remove_state(machine_id, &n)
                }));
                ui.close();
            }
        } else if let Some(index) = selected_transition {
            if ui.button("Delete transition").clicked() {
                pending.push(PendingEdit::new("delete transition", None, move |docs| {
                    docs.graph.remove_transition(machine_id, index).map(|_| ())
                }));
                ui.close();
            }
        } else {
            if ui.button("Add state").clicked() {
                let pos = click_pos.unwrap_or([0.0, 0.0]);
                let name = unique_state_name(&states);
                pending.push(PendingEdit::new(format!("add state {name}"), None, move |docs| {
                    docs.graph.add_state(machine_id, &name, pos)
                }));
                ui.close();
            }
            ui.menu_button("Add transition from Any to", |ui| {
                for target in &states {
                    if ui.button(target).clicked() {
                        pending.push(add_transition_edit(machine_id, None, target.clone()));
                        ui.close();
                    }
                }
            });
        }
    });
}

/// A row of the States list: a state, the "Any state" source, or a transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmRow {
    /// A state, by name.
    State(String),
    /// The "Any state" pseudo-state (the source of transitions with no `from`).
    Any,
    /// A transition, by index.
    Transition(usize),
}

/// The States list rows of `machine`: its states, "Any state", then its transitions.
#[must_use]
pub fn machine_rows(machine: &MachineModel) -> Vec<SmRow> {
    machine
        .states
        .iter()
        .map(|s| SmRow::State(s.name.clone()))
        .chain(std::iter::once(SmRow::Any))
        .chain((0..machine.transitions.len()).map(SmRow::Transition))
        .collect()
}

/// The row (and accessible) name of `row`: "State Idle (initial)", "Any state",
/// "Transition Idle to Run".
#[must_use]
pub fn sm_row_name(machine: &MachineModel, row: &SmRow) -> String {
    match row {
        SmRow::State(name) if *name == machine.initial => format!("State {name} (initial)"),
        SmRow::State(name) => format!("State {name}"),
        SmRow::Any => "Any state".into(),
        SmRow::Transition(i) => match machine.transitions.get(*i) {
            Some(t) => format!("Transition {} to {}", t.from.as_deref().unwrap_or("Any"), t.to),
            None => format!("Transition {i}"),
        },
    }
}

/// Where Ctrl+Shift+A puts a state added after the state at `pos`, canvas units.
pub const ADD_AFTER_OFFSET: [f32; 2] = [170.0, 0.0];

/// The States list beside the state machine canvas: the keyboard route of its mouse edits
/// (FU-5, the convention of `canvas_keys`). Enter selects a state or transition (Enter on the
/// selected state opens its sub-graph, as a double-click does); Alt+arrow moves a state;
/// Ctrl+Shift+A adds a state with a transition from the focused state (or Any); **Connect…**
/// adds a transition to a state typed by name; Delete removes a state or transition.
pub(crate) fn states_ui(ui: &mut egui::Ui, editor: &mut AnimEditor, pending: &mut Vec<PendingEdit>) {
    use crate::canvas_keys::{GraphAction, GraphRows, graph_keys_ui};
    let Some(machine_id) = editor.ui.machine_view else {
        return;
    };
    let Ok(machine) = editor.docs.graph.machine(machine_id).cloned() else {
        return;
    };
    let rows_of = machine_rows(&machine);
    let names: Vec<String> = rows_of.iter().map(|r| sm_row_name(&machine, r)).collect();
    let anchored = editor
        .ui
        .state_keys_row
        .as_ref()
        .and_then(|r| rows_of.iter().position(|m| m == r));
    if anchored.is_some() {
        editor.ui.state_keys.list.retarget(anchored);
    }
    let selected = match (&editor.ui.selected_state, editor.ui.selected_transition) {
        (Some(name), _) => rows_of.iter().position(|r| *r == SmRow::State(name.clone())),
        (None, Some(i)) => rows_of.iter().position(|r| *r == SmRow::Transition(i)),
        _ => None,
    };
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    let kinds = |row: usize, filter: &str| match rows_of.get(row) {
        Some(SmRow::State(_) | SmRow::Any) if "state".contains(&filter.to_lowercase()) => {
            vec!["State".to_string()]
        }
        _ => Vec::new(),
    };
    let rows = GraphRows {
        len: rows_of.len(),
        name: &name,
        selected,
        kinds: &kinds,
        named_outputs: false,
        read_only: false,
        input_pins: false,
        deletable: true,
        heading: "States",
        noun: "state",
    };
    let actions = graph_keys_ui(
        ui,
        ("anim-sm-states", machine_id),
        &mut editor.ui.state_keys,
        &rows,
    );
    editor.ui.state_keys_row = editor
        .ui
        .state_keys
        .list
        .current
        .and_then(|i| rows_of.get(i).cloned());
    let state_names: Vec<String> = machine.states.iter().map(|s| s.name.clone()).collect();
    for action in actions {
        match action {
            GraphAction::Activate(row) => match rows_of.get(row) {
                Some(SmRow::State(name)) => {
                    if editor.ui.selected_state.as_ref() == Some(name) {
                        // Enter on the selected state: the double-click's open.
                        editor.ui.scope = Some(Scope::State {
                            machine: machine_id,
                            state: name.clone(),
                        });
                        editor.ui.machine_view = None;
                    } else {
                        editor.ui.selected_state = Some(name.clone());
                        editor.ui.selected_transition = None;
                        editor.ui.selected_node = Some(machine_id);
                    }
                }
                Some(SmRow::Transition(i)) => {
                    editor.ui.selected_state = None;
                    editor.ui.selected_transition = Some(*i);
                    editor.ui.selected_node = Some(machine_id);
                }
                Some(SmRow::Any) | None => {}
            },
            GraphAction::Delete(row) => match rows_of.get(row) {
                Some(SmRow::State(name)) => {
                    let n = name.clone();
                    // The state menu's Delete state.
                    pending.push(PendingEdit::new(format!("delete state {n}"), None, move |docs| {
                        docs.graph.remove_state(machine_id, &n)
                    }));
                    if editor.ui.selected_state.as_ref() == Some(name) {
                        editor.ui.selected_state = None;
                    }
                }
                Some(SmRow::Transition(index)) => {
                    let index = *index;
                    // The transition menu's Delete transition.
                    pending.push(PendingEdit::new("delete transition", None, move |docs| {
                        docs.graph.remove_transition(machine_id, index).map(|_| ())
                    }));
                    editor.ui.selected_transition = None;
                }
                Some(SmRow::Any) | None => {}
            },
            GraphAction::Nudge(row, delta) => {
                let Some(SmRow::State(name)) = rows_of.get(row) else {
                    continue;
                };
                let Some(state) = machine.states.iter().find(|s| &s.name == name) else {
                    continue;
                };
                let pos = [state.pos[0] + delta[0], state.pos[1] + delta[1]];
                let state_name = name.clone();
                // The drag's edit, without its merge key: one undo entry per press.
                pending.push(PendingEdit::new(
                    format!("move state {name}"),
                    None,
                    move |docs| {
                        let m = docs.graph.machine_mut(machine_id)?;
                        if let Some(s) = m.states.iter_mut().find(|s| s.name == state_name) {
                            s.pos = pos;
                        }
                        Ok(())
                    },
                ));
            }
            GraphAction::AddAfter(row, _) => {
                let (from, at) = match rows_of.get(row) {
                    Some(SmRow::State(name)) => (
                        Some(name.clone()),
                        machine
                            .states
                            .iter()
                            .find(|s| &s.name == name)
                            .map_or([0.0, 0.0], |s| s.pos),
                    ),
                    Some(SmRow::Any) => (None, ANY_POS),
                    _ => continue,
                };
                let pos = [at[0] + ADD_AFTER_OFFSET[0], at[1] + ADD_AFTER_OFFSET[1]];
                let name = unique_state_name(&state_names);
                let link = add_transition_edit(machine_id, from, name.clone());
                // The canvas menu's Add state, then its Add transition: one undo entry.
                pending.push(PendingEdit::new(format!("add state {name}"), None, move |docs| {
                    docs.graph.add_state(machine_id, &name, pos)?;
                    (link.change)(docs)
                }));
            }
            GraphAction::Connect(row, form) => {
                let from = match rows_of.get(row) {
                    Some(SmRow::State(name)) => Some(name.clone()),
                    Some(SmRow::Any) => None,
                    _ => continue,
                };
                let target = form.target.trim();
                let target = target.strip_prefix("State ").unwrap_or(target);
                match state_names.iter().find(|n| n.eq_ignore_ascii_case(target)) {
                    Some(to) => pending.push(add_transition_edit(machine_id, from, to.clone())),
                    None => editor.toast = format!("no state {target:?} in this machine"),
                }
            }
        }
    }
}

/// A pending edit adding a default transition.
#[must_use]
pub fn add_transition_edit(machine: NodeId, from: Option<String>, to: String) -> PendingEdit {
    PendingEdit::new(
        format!("add transition {} -> {to}", from.as_deref().unwrap_or("Any")),
        None,
        move |docs| {
            docs.graph
                .add_transition(
                    machine,
                    TransitionDef {
                        from,
                        to,
                        conditions: Vec::new(),
                        duration: 0.2,
                        curve: TransitionCurve::Smooth,
                        interruptible: true,
                        blend: Default::default(),
                    },
                )
                .map(|_| ())
        },
    )
}

/// `State`, `State2`, … not in `taken`.
#[must_use]
pub fn unique_state_name(taken: &[String]) -> String {
    (1..)
        .map(|i| {
            if i == 1 {
                "State".to_string()
            } else {
                format!("State{i}")
            }
        })
        .find(|n| !taken.contains(n))
        .unwrap_or_else(|| "State".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_helpers() {
        let c = egui::pos2(0.0, 0.0);
        let exit = box_exit(c, egui::pos2(100.0, 0.0), egui::vec2(60.0, 22.0));
        assert!((exit.x - 60.0).abs() < 1e-4 && exit.y.abs() < 1e-4);
        assert!((segment_distance(egui::pos2(5.0, 3.0), c, egui::pos2(10.0, 0.0)) - 3.0).abs() < 1e-5);
        assert_eq!(unique_state_name(&["State".into()]), "State2");
    }
}
