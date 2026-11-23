// SPDX-License-Identifier: MPL-2.0
//! The anim graph inspector: properties of the selected node, state or transition. Every
//! change is a [`PendingEdit`] (undoable); continuous controls coalesce per field.

use std::collections::BTreeMap;

use regia_animation::blendspace::{BlendSample1D, BlendSample2D};
use regia_animation::chain_ik::{ChainSolver, JointLimit};
use regia_animation::clip::RootMotionMode;
use regia_animation::debug::{NodeDebug, NodeDebugState};
use regia_animation::graph::{ChainLimits, IkTarget, JointMaskDef, LayerMode, NamedJointLimit};
use regia_animation::params::ParamValue;
use regia_animation::state_machine::{CmpOp, Condition, TransitionBlend, TransitionCurve, TransitionDef};

use super::blendspace_view::{FieldAction, blend_field, points_1d, points_2d};
use super::model::{
    BodyAimModel, ChainIkModel, LayerMeta, NodeId, NodeKind, OrientationWarpModel, StrideWarpModel,
};
use super::preview::PreviewState;
use super::{AnimDocs, AnimUi, PendingEdit, PreviewRig};

/// A pending edit replacing node `id`'s kind data.
fn set_kind(id: NodeId, field: &str, kind: NodeKind) -> PendingEdit {
    PendingEdit::new(
        format!("edit node {id} {field}"),
        Some(format!("node:{id}:{field}")),
        move |d| d.graph.set_kind(id, kind),
    )
}

/// A combo over `options` (plus free text) editing `value`; returns the new value on change.
fn pick(
    ui: &mut egui::Ui,
    salt: impl egui::AsIdSalt,
    label: &str,
    value: &str,
    options: &[String],
) -> Option<String> {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(salt)
            .selected_text(if value.is_empty() { "(none)" } else { value })
            .width(140.0)
            .show_ui(ui, |ui| {
                for option in options {
                    if ui.selectable_label(option == value, option).clicked() {
                        out = Some(option.clone());
                    }
                }
            });
    });
    out
}

/// Free text field; returns the text when it was edited.
fn text(ui: &mut egui::Ui, label: &str, value: &str) -> Option<String> {
    let mut v = value.to_string();
    let mut out = None;
    ui.horizontal(|ui| {
        ui.label(label);
        if ui
            .add(egui::TextEdit::singleline(&mut v).desired_width(140.0))
            .changed()
        {
            out = Some(v.clone());
        }
    });
    out
}

fn params_of(docs: &AnimDocs, kinds: &[&str]) -> Vec<String> {
    docs.graph
        .parameters
        .iter()
        .filter(|p| kinds.contains(&super::graph_panel::param_kind(&p.1)))
        .map(|p| p.0.clone())
        .collect()
}

/// Draws the inspector for the current selection.
#[allow(clippy::too_many_arguments)] // disjoint borrows of the editor resource
pub fn inspector_ui(
    ui: &mut egui::Ui,
    docs: &AnimDocs,
    rig: &PreviewRig,
    ui_state: &mut AnimUi,
    preview: &mut PreviewState,
    debug: &BTreeMap<NodeId, NodeDebug>,
    pending: &mut Vec<PendingEdit>,
) {
    ui.strong("Inspector");
    let Some(id) = ui_state.selected_node else {
        ui.weak("Select a node (click its title). Right-click the canvas to add nodes; drag from a pin to connect.");
        return;
    };
    let Ok(node) = docs.graph.node(id) else {
        ui_state.selected_node = None;
        return;
    };
    let kind = node.kind.clone();
    ui.label(format!("{} #{id}", super::graph_panel::kind_title(kind.label())));
    let clips = rig.clip_names();
    let joints = rig.skeleton.joint_names.clone();
    let floats = params_of(docs, &["float"]);
    match kind {
        NodeKind::Clip(mut c) => {
            if let Some(v) = pick(ui, ("clip", id), "Clip", &c.clip, &clips) {
                c.clip = v;
                pending.push(set_kind(id, "clip", NodeKind::Clip(c.clone())));
            }
            if let Some(v) = text(ui, "Clip name", &c.clip) {
                c.clip = v;
                pending.push(set_kind(id, "clip", NodeKind::Clip(c.clone())));
            }
            let mut looping = c.looping;
            if ui.checkbox(&mut looping, "Loop").changed() {
                c.looping = looping;
                pending.push(set_kind(id, "loop", NodeKind::Clip(c.clone())));
            }
            let mut speed = c.speed;
            if ui
                .add(egui::Slider::new(&mut speed, -2.0..=4.0).text("Speed"))
                .changed()
            {
                c.speed = speed;
                pending.push(set_kind(id, "speed", NodeKind::Clip(c.clone())));
            }
            if let Some(v) = text(ui, "Sync group", c.sync_group.as_deref().unwrap_or("")) {
                c.sync_group = (!v.is_empty()).then_some(v);
                pending.push(set_kind(id, "sync", NodeKind::Clip(c)));
            }
        }
        NodeKind::BlendSpace1D(mut b) => {
            if let Some(v) = pick(ui, ("bs1-param", id), "Param", &b.space.param, &floats) {
                b.space.param = v;
                pending.push(set_kind(id, "param", NodeKind::BlendSpace1D(b.clone())));
            }
            let input = preview.params.float(&b.space.param);
            let weights = match debug.get(&id).map(|d| &d.state) {
                Some(NodeDebugState::BlendSpace { weights, .. }) => weights
                    .iter()
                    .filter_map(|(clip, w)| {
                        b.space
                            .samples
                            .iter()
                            .position(|s| &s.clip == clip)
                            .map(|i| (i, *w))
                    })
                    .collect(),
                _ => b.space.weights(input),
            };
            let labels: Vec<String> = b.space.samples.iter().map(|s| s.clip.clone()).collect();
            let action = blend_field(
                ui,
                egui::Id::new(("bs1", id)),
                true,
                &points_1d(&b.space),
                &labels,
                &[],
                &weights,
                [input, 0.0],
                0.1,
            );
            match action {
                Some(FieldAction::Move(i, p)) => {
                    if let Some(s) = b.space.samples.get_mut(i) {
                        s.value = p[0];
                        pending.push(set_kind(
                            id,
                            &format!("sample{i}"),
                            NodeKind::BlendSpace1D(b.clone()),
                        ));
                    }
                }
                Some(FieldAction::Add(p)) => {
                    b.space.samples.push(BlendSample1D {
                        value: p[0],
                        clip: clips.first().cloned().unwrap_or_default(),
                    });
                    pending.push(set_kind(id, "add-sample", NodeKind::BlendSpace1D(b.clone())));
                }
                Some(FieldAction::Remove(i)) => {
                    b.space.samples.remove(i);
                    pending.push(set_kind(id, "remove-sample", NodeKind::BlendSpace1D(b.clone())));
                }
                Some(FieldAction::SetInput(p)) => preview.params.set_float(&b.space.param, p[0]),
                None => {}
            }
            ui.weak("Drag points · double-click adds · right-click removes · click sets the preview value");
            let mut changed = None;
            for (i, sample) in b.space.samples.iter().enumerate() {
                ui.horizontal(|ui| {
                    let mut v = sample.value;
                    if ui.add(egui::DragValue::new(&mut v).speed(0.05)).changed() {
                        changed = Some((i, Some(v), None));
                    }
                    egui::ComboBox::from_id_salt(("bs1-clip", id, i))
                        .selected_text(sample.clip.as_str())
                        .show_ui(ui, |ui| {
                            for c in &clips {
                                if ui.selectable_label(c == &sample.clip, c).clicked() {
                                    changed = Some((i, None, Some(c.clone())));
                                }
                            }
                        });
                });
            }
            if let Some((i, v, c)) = changed {
                if let Some(v) = v {
                    b.space.samples[i].value = v;
                }
                if let Some(c) = c {
                    b.space.samples[i].clip = c;
                }
                pending.push(set_kind(
                    id,
                    &format!("sample{i}"),
                    NodeKind::BlendSpace1D(b.clone()),
                ));
            }
            if let Some(v) = text(ui, "Sync group", b.sync_group.as_deref().unwrap_or("")) {
                b.sync_group = (!v.is_empty()).then_some(v);
                pending.push(set_kind(id, "sync", NodeKind::BlendSpace1D(b)));
            }
        }
        NodeKind::BlendSpace2D(mut b) => {
            if let Some(v) = pick(ui, ("bs2-x", id), "Param X", &b.space.param_x, &floats) {
                b.space.param_x = v;
                pending.push(set_kind(id, "param_x", NodeKind::BlendSpace2D(b.clone())));
            }
            if let Some(v) = pick(ui, ("bs2-y", id), "Param Y", &b.space.param_y, &floats) {
                b.space.param_y = v;
                pending.push(set_kind(id, "param_y", NodeKind::BlendSpace2D(b.clone())));
            }
            let mut tri = b.space.clone();
            tri.triangulate();
            let input = [
                preview.params.float(&b.space.param_x),
                preview.params.float(&b.space.param_y),
            ];
            let weights = tri.weights(input);
            let labels: Vec<String> = b.space.samples.iter().map(|s| s.clip.clone()).collect();
            let action = blend_field(
                ui,
                egui::Id::new(("bs2", id)),
                false,
                &points_2d(&b.space),
                &labels,
                &tri.triangles,
                &weights,
                input,
                0.1,
            );
            match action {
                Some(FieldAction::Move(i, p)) => {
                    if let Some(s) = b.space.samples.get_mut(i) {
                        s.point = p;
                        pending.push(set_kind(
                            id,
                            &format!("sample{i}"),
                            NodeKind::BlendSpace2D(b.clone()),
                        ));
                    }
                }
                Some(FieldAction::Add(p)) => {
                    b.space.samples.push(BlendSample2D {
                        point: p,
                        clip: clips.first().cloned().unwrap_or_default(),
                    });
                    pending.push(set_kind(id, "add-sample", NodeKind::BlendSpace2D(b.clone())));
                }
                Some(FieldAction::Remove(i)) => {
                    b.space.samples.remove(i);
                    pending.push(set_kind(id, "remove-sample", NodeKind::BlendSpace2D(b.clone())));
                }
                Some(FieldAction::SetInput(p)) => {
                    preview.params.set_float(&b.space.param_x, p[0]);
                    preview.params.set_float(&b.space.param_y, p[1]);
                }
                None => {}
            }
            let mut changed = None;
            for (i, sample) in b.space.samples.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("({:.2}, {:.2})", sample.point[0], sample.point[1]));
                    egui::ComboBox::from_id_salt(("bs2-clip", id, i))
                        .selected_text(sample.clip.as_str())
                        .show_ui(ui, |ui| {
                            for c in &clips {
                                if ui.selectable_label(c == &sample.clip, c).clicked() {
                                    changed = Some((i, c.clone()));
                                }
                            }
                        });
                });
            }
            if let Some((i, c)) = changed {
                b.space.samples[i].clip = c;
                pending.push(set_kind(id, &format!("sample{i}"), NodeKind::BlendSpace2D(b)));
            }
        }
        NodeKind::Blend { alpha_param } => {
            if let Some(v) = pick(ui, ("alpha", id), "Alpha", &alpha_param, &floats) {
                pending.push(set_kind(id, "alpha", NodeKind::Blend { alpha_param: v }));
            }
        }
        NodeKind::Additive { weight_param } => {
            if let Some(v) = pick(ui, ("weight", id), "Weight", &weight_param, &floats) {
                pending.push(set_kind(id, "weight", NodeKind::Additive { weight_param: v }));
            }
        }
        NodeKind::Layer { mut layers } => layers_ui(ui, id, &mut layers, &floats, &joints, pending),
        NodeKind::TwoBoneIK(mut ik) => {
            let chains: Vec<String> = docs.edits.ik_chains.iter().map(|c| c.name.clone()).collect();
            if !chains.is_empty()
                && let Some(name) = pick(ui, ("ik-chain", id), "Chain", "", &chains)
                && let Some(chain) = docs.edits.ik_chain(&name)
            {
                ik.chain = chain.joints.clone();
                if chain.pole_param.is_some() {
                    ik.pole = chain.pole_param.clone();
                }
                pending.push(set_kind(id, "chain", NodeKind::TwoBoneIK(ik.clone())));
            }
            for (slot, label) in ["Root", "Mid", "Tip"].iter().enumerate() {
                if let Some(v) = pick(ui, ("ik-joint", id, slot), label, &ik.chain[slot], &joints) {
                    ik.chain[slot] = v;
                    pending.push(set_kind(id, "chain", NodeKind::TwoBoneIK(ik.clone())));
                }
            }
            if let Some(t) = target_ui(ui, id, &ik.target, docs, rig) {
                ik.target = t;
                pending.push(set_kind(id, "target", NodeKind::TwoBoneIK(ik.clone())));
            }
            let vec3s = params_of(docs, &["vec3"]);
            if let Some(v) = pick(
                ui,
                ("ik-pole", id),
                "Pole",
                ik.pole.as_deref().unwrap_or(""),
                &vec3s,
            ) {
                ik.pole = Some(v);
                pending.push(set_kind(id, "pole", NodeKind::TwoBoneIK(ik.clone())));
            }
            if let Some(v) = pick(
                ui,
                ("ik-w", id),
                "Weight",
                ik.weight_param.as_deref().unwrap_or(""),
                &floats,
            ) {
                ik.weight_param = Some(v);
                pending.push(set_kind(id, "weight", NodeKind::TwoBoneIK(ik)));
            }
        }
        NodeKind::LookAt(mut l) => {
            if let Some(v) = pick(ui, ("la-joint", id), "Joint", &l.joint, &joints) {
                l.joint = v;
                pending.push(set_kind(id, "joint", NodeKind::LookAt(l.clone())));
            }
            if let Some(t) = target_ui(ui, id, &l.target, docs, rig) {
                l.target = t;
                pending.push(set_kind(id, "target", NodeKind::LookAt(l.clone())));
            }
            let (mut yaw, mut pitch) = (l.max_yaw, l.max_pitch);
            let a = ui
                .add(egui::Slider::new(&mut yaw, 0.0..=3.1).text("Max yaw"))
                .changed();
            let b = ui
                .add(egui::Slider::new(&mut pitch, 0.0..=1.6).text("Max pitch"))
                .changed();
            if a || b {
                l.max_yaw = yaw;
                l.max_pitch = pitch;
                pending.push(set_kind(id, "limits", NodeKind::LookAt(l.clone())));
            }
            if let Some(v) = pick(
                ui,
                ("la-w", id),
                "Weight",
                l.weight_param.as_deref().unwrap_or(""),
                &floats,
            ) {
                l.weight_param = Some(v);
                pending.push(set_kind(id, "weight", NodeKind::LookAt(l)));
            }
        }
        NodeKind::RootMotion { mode } => {
            let modes = [
                RootMotionMode::None,
                RootMotionMode::XZ,
                RootMotionMode::XYZ,
                RootMotionMode::Yaw,
            ];
            ui.horizontal(|ui| {
                ui.label("Mode");
                for m in modes {
                    if ui.selectable_label(m == mode, format!("{m:?}")).clicked() {
                        pending.push(set_kind(id, "mode", NodeKind::RootMotion { mode: m }));
                    }
                }
            });
        }
        NodeKind::Slot {
            name,
            eval_idle_source,
        } => {
            if let Some(v) = text(ui, "Slot", &name) {
                pending.push(set_kind(
                    id,
                    "slot",
                    NodeKind::Slot {
                        name: v,
                        eval_idle_source,
                    },
                ));
            }
            let mut idle = eval_idle_source;
            if ui
                .checkbox(&mut idle, "Evaluate source under a full-body montage")
                .changed()
            {
                pending.push(set_kind(
                    id,
                    "eval_idle_source",
                    NodeKind::Slot {
                        name,
                        eval_idle_source: idle,
                    },
                ));
            }
        }
        NodeKind::ChainIK(c) => chain_ik_ui(ui, id, c, docs, rig, &joints, &floats, pending),
        NodeKind::BodyAim(a) => body_aim_ui(ui, id, a, docs, rig, &joints, &floats, pending),
        NodeKind::StrideWarp(w) => stride_warp_ui(ui, id, w, &joints, &floats, pending),
        NodeKind::OrientationWarp(w) => orientation_warp_ui(ui, id, w, &joints, &floats, pending),
        NodeKind::StateMachine(m) => {
            if ui.button("Open state canvas").clicked() {
                ui_state.machine_view = Some(id);
            }
            let states: Vec<String> = m.states.iter().map(|s| s.name.clone()).collect();
            if let Some(v) = pick(ui, ("sm-initial", id), "Initial", &m.initial, &states) {
                pending.push(PendingEdit::new("set initial state", None, move |d| {
                    d.graph.machine_mut(id)?.initial = v;
                    Ok(())
                }));
            }
            if let Some(name) = ui_state.selected_state.clone().filter(|n| states.contains(n)) {
                ui.separator();
                state_ui(ui, id, &name, ui_state, pending);
            }
            if let Some(index) = ui_state.selected_transition
                && let Some(t) = m.transitions.get(index)
            {
                ui.separator();
                transition_ui(ui, id, index, t, &states, docs, pending);
            }
            ui.separator();
            ui.label("Transitions (priority order)");
            for (i, t) in m.transitions.iter().enumerate() {
                let label = format!(
                    "{} -> {} ({} cond)",
                    t.from.as_deref().unwrap_or("Any"),
                    t.to,
                    t.conditions.len()
                );
                if ui
                    .selectable_label(ui_state.selected_transition == Some(i), label)
                    .clicked()
                {
                    ui_state.selected_transition = Some(i);
                }
            }
        }
    }
}

/// The chain IK node's properties: joints from the skeleton, solver, target, pole, limits.
#[allow(clippy::too_many_arguments)] // disjoint borrows of the editor resource
fn chain_ik_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    mut c: ChainIkModel,
    docs: &AnimDocs,
    rig: &PreviewRig,
    joints: &[String],
    floats: &[String],
    pending: &mut Vec<PendingEdit>,
) {
    let push = |pending: &mut Vec<PendingEdit>, field: &str, c: &ChainIkModel| {
        pending.push(set_kind(id, field, NodeKind::ChainIK(c.clone())));
    };
    if let Some(v) = pick(ui, ("cik-root", id), "Root", &c.root, joints) {
        c.root = v;
        push(pending, "root", &c);
    }
    if let Some(v) = pick(ui, ("cik-tip", id), "Tip", &c.tip, joints) {
        c.tip = v;
        push(pending, "tip", &c);
    }
    ui.horizontal(|ui| {
        ui.label("Solver");
        for (solver, label) in [(ChainSolver::Fabrik, "FABRIK"), (ChainSolver::Ccd, "CCD")] {
            if ui.selectable_label(c.solver == solver, label).clicked() && c.solver != solver {
                c.solver = solver;
                push(pending, "solver", &c);
            }
        }
    });
    if let Some(t) = target_ui(ui, id, &c.target, docs, rig) {
        c.target = t;
        push(pending, "target", &c);
    }
    let vec3s = params_of(docs, &["vec3"]);
    if let Some(v) = pick(
        ui,
        ("cik-pole", id),
        "Pole",
        c.pole.as_deref().unwrap_or(""),
        &vec3s,
    ) {
        c.pole = Some(v);
        push(pending, "pole", &c);
    }
    if c.pole.is_some() && ui.small_button("Clear pole").clicked() {
        c.pole = None;
        push(pending, "pole", &c);
    }
    let mut iterations = c.iterations;
    if ui
        .add(egui::Slider::new(&mut iterations, 1..=256).text("Iterations"))
        .changed()
    {
        c.iterations = iterations;
        push(pending, "iterations", &c);
    }
    let mut tolerance = c.tolerance;
    if ui
        .add(
            egui::Slider::new(&mut tolerance, 0.0001..=0.05)
                .logarithmic(true)
                .text("Tolerance (m)"),
        )
        .changed()
    {
        c.tolerance = tolerance;
        push(pending, "tolerance", &c);
    }
    if limits_ui(ui, ("cik-limits", id), &mut c.limits, joints) {
        push(pending, "limits", &c);
    }
    if let Some(v) = pick(
        ui,
        ("cik-w", id),
        "Weight",
        c.weight_param.as_deref().unwrap_or(""),
        floats,
    ) {
        c.weight_param = Some(v);
        push(pending, "weight", &c);
    }
}

/// The joint "Add joint" appends to a body aim node: the first rig joint (in skeleton order) that
/// does not turn yet, else the first rig joint; `None` when the rig has no joints. Never an empty
/// name (ADR-8732).
fn next_turning_joint(rig_joints: &[String], turning: &[(String, f32)]) -> Option<String> {
    rig_joints
        .iter()
        .find(|j| !turning.iter().any(|(t, _)| t == *j))
        .or_else(|| rig_joints.first())
        .cloned()
}

/// The body aim node's properties: turning joints with shares, aim joint and axis, limits.
#[allow(clippy::too_many_arguments)] // disjoint borrows of the editor resource
fn body_aim_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    mut a: BodyAimModel,
    docs: &AnimDocs,
    rig: &PreviewRig,
    joints: &[String],
    floats: &[String],
    pending: &mut Vec<PendingEdit>,
) {
    let push = |pending: &mut Vec<PendingEdit>, field: &str, a: &BodyAimModel| {
        pending.push(set_kind(id, field, NodeKind::BodyAim(a.clone())));
    };
    ui.label("Turning joints (bottom first) and shares");
    let mut remove = None;
    let mut changed = false;
    for (i, (joint, share)) in a.joints.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("aim-joint", id, i))
                .selected_text(joint.as_str())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for option in joints {
                        if ui.selectable_label(option == joint, option).clicked() {
                            *joint = option.clone();
                            changed = true;
                        }
                    }
                });
            if ui
                .add(egui::DragValue::new(share).speed(0.05).range(0.0..=10.0))
                .changed()
            {
                changed = true;
            }
            if ui.small_button("x").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        a.joints.remove(i);
        changed = true;
    }
    // Never an empty name: validation refuses it and the runtime would fail every frame with
    // `MissingJoint("")` (ADR-8732). With no rig joints to offer the button is disabled.
    let next = next_turning_joint(joints, &a.joints);
    if ui
        .add_enabled(next.is_some(), egui::Button::new("Add joint").small())
        .clicked()
        && let Some(joint) = next
    {
        a.joints.push((joint, 1.0));
        changed = true;
    }
    if changed {
        push(pending, "joints", &a);
    }
    if let Some(v) = pick(ui, ("aim-aim", id), "Aim joint", &a.aim_joint, joints) {
        a.aim_joint = v;
        push(pending, "aim_joint", &a);
    }
    let mut axis = a.axis.to_array();
    let mut axis_changed = false;
    ui.horizontal(|ui| {
        ui.label("Axis");
        for c in &mut axis {
            axis_changed |= ui
                .add(egui::DragValue::new(c).speed(0.01).range(-1.0..=1.0))
                .changed();
        }
    });
    if axis_changed {
        a.axis = regia_core::math::Vec3::from_array(axis);
        push(pending, "axis", &a);
    }
    if let Some(t) = target_ui(ui, id, &a.target, docs, rig) {
        a.target = t;
        push(pending, "target", &a);
    }
    let mut passes = a.passes;
    if ui
        .add(egui::Slider::new(&mut passes, 1..=32).text("Passes"))
        .changed()
    {
        a.passes = passes;
        push(pending, "passes", &a);
    }
    if limits_ui(ui, ("aim-limits", id), &mut a.limits, joints) {
        push(pending, "limits", &a);
    }
    if let Some(v) = pick(
        ui,
        ("aim-w", id),
        "Weight",
        a.weight_param.as_deref().unwrap_or(""),
        floats,
    ) {
        a.weight_param = Some(v);
        push(pending, "weight", &a);
    }
}

/// The stride warp node's properties: ground speed, mode, strength and ratio range, legs.
fn stride_warp_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    mut w: StrideWarpModel,
    joints: &[String],
    floats: &[String],
    pending: &mut Vec<PendingEdit>,
) {
    use regia_animation::motion_matching::warp::StrideMode;
    let push = |pending: &mut Vec<PendingEdit>, field: &str, w: &StrideWarpModel| {
        pending.push(set_kind(id, field, NodeKind::StrideWarp(w.clone())));
    };
    if let Some(v) = pick(
        ui,
        ("sw-speed", id),
        "Ground speed",
        &w.ground_speed_param,
        floats,
    ) {
        w.ground_speed_param = v;
        push(pending, "ground_speed_param", &w);
    }
    ui.horizontal(|ui| {
        ui.label("Mode");
        for (mode, label) in [
            (StrideMode::Pose, "Pose (stride IK)"),
            (StrideMode::Rate, "Rate (clock)"),
        ] {
            if ui.selectable_label(w.settings.mode == mode, label).clicked() && w.settings.mode != mode {
                w.settings.mode = mode;
                push(pending, "mode", &w);
            }
        }
    });
    let mut settings = w.settings;
    let mut changed = false;
    changed |= ui
        .add(egui::Slider::new(&mut settings.strength, 0.0..=1.0).text("Strength"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut settings.ratio_min, 0.2..=1.0).text("Ratio min"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut settings.ratio_max, 1.0..=3.0).text("Ratio max"))
        .changed();
    changed |= ui
        .add(egui::Slider::new(&mut settings.min_speed, 0.0..=1.0).text("Min speed (m/s)"))
        .changed();
    changed |= ui
        .add(
            egui::Slider::new(&mut settings.response, 1.0..=40.0)
                .logarithmic(true)
                .text("Response (1/s)"),
        )
        .changed();
    if changed {
        w.settings = settings;
        push(pending, "settings", &w);
    }
    if w.settings.mode == StrideMode::Pose {
        if let Some(v) = pick(ui, ("sw-pelvis", id), "Pelvis", &w.pelvis, joints) {
            w.pelvis = v;
            push(pending, "pelvis", &w);
        }
        let mut length = w.stride_length;
        if ui
            .add(egui::Slider::new(&mut length, 0.05..=2.0).text("Stride length (m)"))
            .changed()
        {
            w.stride_length = length;
            push(pending, "stride_length", &w);
        }
        ui.label("Legs: hip, knee, ankle");
        let mut remove = None;
        let mut legs_changed = false;
        for (i, leg) in w.legs.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                for (k, joint) in leg.iter_mut().enumerate() {
                    egui::ComboBox::from_id_salt(("sw-leg", id, i * 3 + k))
                        .selected_text(joint.as_str())
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for option in joints {
                                if ui.selectable_label(option == joint, option).clicked() {
                                    *joint = option.clone();
                                    legs_changed = true;
                                }
                            }
                        });
                }
                if ui.small_button("x").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            w.legs.remove(i);
            legs_changed = true;
        }
        if ui.small_button("Add leg").clicked() {
            w.legs.push(Default::default());
            legs_changed = true;
        }
        if legs_changed {
            push(pending, "legs", &w);
        }
    }
}

/// The orientation warp node's properties: heading, pelvis, spine shares, limit.
fn orientation_warp_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    mut w: OrientationWarpModel,
    joints: &[String],
    floats: &[String],
    pending: &mut Vec<PendingEdit>,
) {
    let push = |pending: &mut Vec<PendingEdit>, field: &str, w: &OrientationWarpModel| {
        pending.push(set_kind(id, field, NodeKind::OrientationWarp(w.clone())));
    };
    if let Some(v) = pick(ui, ("ow-heading", id), "Heading", &w.heading_param, floats) {
        w.heading_param = v;
        push(pending, "heading_param", &w);
    }
    if let Some(v) = pick(ui, ("ow-pelvis", id), "Pelvis", &w.pelvis, joints) {
        w.pelvis = v;
        push(pending, "pelvis", &w);
    }
    let mut strength = w.strength;
    if ui
        .add(egui::Slider::new(&mut strength, 0.0..=1.0).text("Strength"))
        .changed()
    {
        w.strength = strength;
        push(pending, "strength", &w);
    }
    let mut max_delta = w.max_delta;
    if ui
        .add(egui::Slider::new(&mut max_delta, 0.0..=std::f32::consts::PI).text("Max turn (rad)"))
        .changed()
    {
        w.max_delta = max_delta;
        push(pending, "max_delta", &w);
    }
    ui.label("Spine (low to high) and shares (sum 1)");
    let mut remove = None;
    let mut changed = false;
    for (i, (joint, share)) in w.spine.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("ow-spine", id, i))
                .selected_text(joint.as_str())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for option in joints {
                        if ui.selectable_label(option == joint, option).clicked() {
                            *joint = option.clone();
                            changed = true;
                        }
                    }
                });
            if ui
                .add(egui::DragValue::new(share).speed(0.05).range(0.0..=1.0))
                .changed()
            {
                changed = true;
            }
            if ui.small_button("x").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        w.spine.remove(i);
        changed = true;
    }
    if ui.small_button("Add spine joint").clicked() {
        w.spine.push((String::new(), 0.0));
        changed = true;
    }
    if changed {
        push(pending, "spine", &w);
    }
}

/// One joint limit's controls; true when it changed.
fn limit_ui(ui: &mut egui::Ui, salt: impl egui::AsIdSalt + Copy, limit: &mut JointLimit) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        for (label, kind) in [("free", 0), ("cone", 1), ("hinge", 2)] {
            let current = match limit {
                JointLimit::Free => 0,
                JointLimit::Cone { .. } => 1,
                JointLimit::Hinge { .. } => 2,
            };
            if ui.selectable_label(current == kind, label).clicked() && current != kind {
                *limit = match kind {
                    1 => JointLimit::Cone {
                        max_swing: 0.5,
                        min_twist: -0.2,
                        max_twist: 0.2,
                    },
                    2 => JointLimit::Hinge {
                        axis: regia_core::math::Vec3::X,
                        min: -1.0,
                        max: 1.0,
                    },
                    _ => JointLimit::Free,
                };
                changed = true;
            }
        }
    });
    ui.push_id(salt, |ui| match limit {
        JointLimit::Free => {}
        JointLimit::Cone {
            max_swing,
            min_twist,
            max_twist,
        } => {
            changed |= ui
                .add(egui::Slider::new(max_swing, 0.0..=std::f32::consts::PI).text("Max swing"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(min_twist, -std::f32::consts::PI..=0.0).text("Min twist"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(max_twist, 0.0..=std::f32::consts::PI).text("Max twist"))
                .changed();
        }
        JointLimit::Hinge { axis, min, max } => {
            let mut v = axis.to_array();
            ui.horizontal(|ui| {
                ui.label("Axis");
                for c in &mut v {
                    changed |= ui
                        .add(egui::DragValue::new(c).speed(0.01).range(-1.0..=1.0))
                        .changed();
                }
            });
            *axis = regia_core::math::Vec3::from_array(v);
            changed |= ui
                .add(egui::Slider::new(min, -std::f32::consts::PI..=0.0).text("Min"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(max, 0.0..=std::f32::consts::PI).text("Max"))
                .changed();
        }
    });
    changed
}

/// An IK node's limits: none, one for every joint, or per joint by name; true when changed.
fn limits_ui(
    ui: &mut egui::Ui,
    salt: impl egui::AsIdSalt + Copy,
    limits: &mut Option<ChainLimits>,
    joints: &[String],
) -> bool {
    let mut changed = false;
    ui.push_id(salt, |ui| {
        ui.horizontal(|ui| {
            ui.label("Limits");
            let mode = match limits {
                None => 0,
                Some(ChainLimits::Uniform(_)) => 1,
                Some(ChainLimits::PerJoint(_)) => 2,
            };
            for (label, m) in [("none", 0), ("uniform", 1), ("per joint", 2)] {
                if ui.selectable_label(mode == m, label).clicked() && mode != m {
                    *limits = match m {
                        1 => Some(ChainLimits::Uniform(JointLimit::Cone {
                            max_swing: 0.5,
                            min_twist: -0.2,
                            max_twist: 0.2,
                        })),
                        2 => Some(ChainLimits::PerJoint(Vec::new())),
                        _ => None,
                    };
                    changed = true;
                }
            }
        });
        match limits {
            None => {}
            Some(ChainLimits::Uniform(limit)) => changed |= limit_ui(ui, "uniform", limit),
            Some(ChainLimits::PerJoint(list)) => {
                let mut remove = None;
                for (i, entry) in list.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt(("limit-joint", i))
                            .selected_text(entry.joint.as_str())
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                for option in joints {
                                    if ui.selectable_label(*option == entry.joint, option).clicked() {
                                        entry.joint = option.clone();
                                        changed = true;
                                    }
                                }
                            });
                        if ui.small_button("x").clicked() {
                            remove = Some(i);
                        }
                    });
                    changed |= limit_ui(ui, ("limit", i), &mut entry.limit);
                }
                if let Some(i) = remove {
                    list.remove(i);
                    changed = true;
                }
                if ui.small_button("Add joint limit").clicked() {
                    list.push(NamedJointLimit {
                        joint: joints.first().cloned().unwrap_or_default(),
                        limit: JointLimit::Free,
                    });
                    changed = true;
                }
            }
        }
    });
    changed
}

fn target_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    target: &IkTarget,
    docs: &AnimDocs,
    rig: &PreviewRig,
) -> Option<IkTarget> {
    let mut out = None;
    let (is_param, name) = match target {
        IkTarget::Param(p) => (true, p.clone()),
        IkTarget::Socket(s) => (false, s.clone()),
    };
    ui.horizontal(|ui| {
        ui.label("Target");
        if ui.selectable_label(is_param, "param").clicked() && !is_param {
            out = Some(IkTarget::Param(String::new()));
        }
        if ui.selectable_label(!is_param, "socket").clicked() && is_param {
            out = Some(IkTarget::Socket(String::new()));
        }
    });
    let options: Vec<String> = if is_param {
        params_of(docs, &["vec3"])
    } else {
        rig.edited_skeleton(&docs.edits)
            .sockets
            .iter()
            .map(|s| s.name.clone())
            .collect()
    };
    if let Some(v) = pick(ui, ("ik-target", id), "  name", &name, &options) {
        out = Some(if is_param {
            IkTarget::Param(v)
        } else {
            IkTarget::Socket(v)
        });
    }
    if let Some(v) = text(ui, "  or type", &name) {
        out = Some(if is_param {
            IkTarget::Param(v)
        } else {
            IkTarget::Socket(v)
        });
    }
    out
}

fn layers_ui(
    ui: &mut egui::Ui,
    id: NodeId,
    layers: &mut Vec<LayerMeta>,
    floats: &[String],
    joints: &[String],
    pending: &mut Vec<PendingEdit>,
) {
    let mut changed = false;
    let mut remove = None;
    for (i, layer) in layers.iter_mut().enumerate() {
        egui::CollapsingHeader::new(format!("Layer {i}"))
            .id_salt(("layer", id, i))
            .default_open(true)
            .show(ui, |ui| {
                if let Some(v) = pick(ui, ("layer-w", id, i), "Weight", &layer.weight_param, floats) {
                    layer.weight_param = v;
                    changed = true;
                }
                changed |= ui
                    .add(
                        egui::Slider::new(&mut layer.weight_inertialize, 0.0..=1.0)
                            .text("Weight inertialize (s)"),
                    )
                    .on_hover_text("Ease weight changes with an inertial decay; 0 = follow the parameter")
                    .changed();
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    for m in [LayerMode::Override, LayerMode::Additive] {
                        if ui.selectable_label(layer.mode == m, format!("{m:?}")).clicked() {
                            layer.mode = m;
                            changed = true;
                        }
                    }
                });
                changed |= ui
                    .checkbox(&mut layer.mask.include_children, "Mask includes children")
                    .changed();
                ui.label("Mask joints:");
                let mut drop = None;
                for (j, joint) in layer.mask.joints.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.monospace(joint);
                        if ui.small_button("×").clicked() {
                            drop = Some(j);
                        }
                    });
                }
                if let Some(j) = drop {
                    layer.mask.joints.remove(j);
                    changed = true;
                }
                if let Some(v) = pick(ui, ("layer-add-joint", id, i), "Add joint", "", joints)
                    && !layer.mask.joints.contains(&v)
                {
                    layer.mask.joints.push(v);
                    changed = true;
                }
                if ui.small_button("Remove layer").clicked() {
                    remove = Some(i);
                }
            });
    }
    if let Some(i) = remove {
        // The layer's sub-graph input goes with it: shift later inputs down.
        pending.push(PendingEdit::new("remove layer", None, move |d| {
            let node = d.graph.node(id)?.clone();
            let NodeKind::Layer { mut layers } = node.kind else {
                return Err("not a layer node".into());
            };
            if i >= layers.len() {
                return Err(format!("no layer {i}"));
            }
            layers.remove(i);
            let mut inputs = node.inputs.clone();
            if i + 1 < inputs.len() {
                inputs.remove(i + 1);
            }
            d.graph.set_kind(id, NodeKind::Layer { layers })?;
            if let Some(n) = d.graph.nodes.get_mut(&id) {
                inputs.resize(n.inputs.len(), None);
                n.inputs = inputs;
            }
            Ok(())
        }));
        return;
    }
    if ui.button("Add layer").clicked() {
        layers.push(LayerMeta {
            mask: JointMaskDef {
                joints: Vec::new(),
                include_children: true,
            },
            mode: LayerMode::Override,
            weight_param: floats.first().cloned().unwrap_or_default(),
            weight_inertialize: 0.0,
        });
        changed = true;
    }
    if changed {
        pending.push(set_kind(
            id,
            "layers",
            NodeKind::Layer {
                layers: layers.clone(),
            },
        ));
    }
}

fn state_ui(
    ui: &mut egui::Ui,
    machine: NodeId,
    name: &str,
    ui_state: &mut AnimUi,
    pending: &mut Vec<PendingEdit>,
) {
    ui.label(format!("State {name}"));
    let rename_id = egui::Id::new(("anim-rename-state", machine, name.to_string()));
    let mut new_name: String = ui
        .data(|d| d.get_temp(rename_id))
        .unwrap_or_else(|| name.to_string());
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(110.0));
        if ui.button("Rename").clicked() && new_name != name {
            let (from, to) = (name.to_string(), new_name.clone());
            ui_state.selected_state = Some(to.clone());
            pending.push(PendingEdit::new(format!("rename state {from}"), None, move |d| {
                d.graph.rename_state(machine, &from, &to)
            }));
        }
    });
    ui.data_mut(|d| d.insert_temp(rename_id, new_name));
    ui.horizontal(|ui| {
        if ui.button("Open sub-graph").clicked() {
            ui_state.scope = Some(super::Scope::State {
                machine,
                state: name.to_string(),
            });
            ui_state.machine_view = None;
        }
        if ui.button("Set initial").clicked() {
            let n = name.to_string();
            pending.push(PendingEdit::new("set initial state", None, move |d| {
                d.graph.machine_mut(machine)?.initial = n;
                Ok(())
            }));
        }
    });
}

/// Labels of the condition kinds the editor offers.
pub const CONDITION_KINDS: [&str; 6] = ["float", "int", "bool", "trigger", "clip_finished", "notify"];

/// A default condition of `kind` on the first parameter of a matching type.
#[must_use]
pub fn default_condition(kind: &str, docs: &AnimDocs) -> Option<Condition> {
    let first = |k: &str| params_of(docs, &[k]).into_iter().next().unwrap_or_default();
    Some(match kind {
        "float" => Condition::Float {
            param: first("float"),
            op: CmpOp::Gt,
            value: 0.0,
        },
        "int" => Condition::Int {
            param: first("int"),
            op: CmpOp::Eq,
            value: 0,
        },
        "bool" => Condition::Bool {
            param: first("bool"),
            value: true,
        },
        "trigger" => Condition::Trigger {
            param: first("trigger"),
        },
        "clip_finished" => Condition::ClipFinished,
        "notify" => Condition::Notify(String::new()),
        _ => return None,
    })
}

fn condition_ui(ui: &mut egui::Ui, salt: (NodeId, usize, usize), c: &mut Condition, docs: &AnimDocs) -> bool {
    let ops = [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne];
    let op_ui = |ui: &mut egui::Ui, op: &mut CmpOp| -> bool {
        let mut changed = false;
        egui::ComboBox::from_id_salt(("op", salt))
            .selected_text(format!("{op:?}"))
            .width(44.0)
            .show_ui(ui, |ui| {
                for o in ops {
                    changed |= ui.selectable_value(op, o, format!("{o:?}")).changed();
                }
            });
        changed
    };
    let mut changed = false;
    ui.horizontal(|ui| match c {
        Condition::Float { param, op, value } => {
            if let Some(v) = pick(ui, ("cp", salt), "", param, &params_of(docs, &["float"])) {
                *param = v;
                changed = true;
            }
            changed |= op_ui(ui, op);
            changed |= ui.add(egui::DragValue::new(value).speed(0.05)).changed();
        }
        Condition::Int { param, op, value } => {
            if let Some(v) = pick(ui, ("cp", salt), "", param, &params_of(docs, &["int"])) {
                *param = v;
                changed = true;
            }
            changed |= op_ui(ui, op);
            changed |= ui.add(egui::DragValue::new(value)).changed();
        }
        Condition::Bool { param, value } => {
            if let Some(v) = pick(ui, ("cp", salt), "", param, &params_of(docs, &["bool"])) {
                *param = v;
                changed = true;
            }
            changed |= ui.checkbox(value, "is true").changed();
        }
        Condition::Trigger { param } => {
            if let Some(v) = pick(ui, ("cp", salt), "trigger", param, &params_of(docs, &["trigger"])) {
                *param = v;
                changed = true;
            }
        }
        Condition::ClipFinished => {
            ui.label("source clip finished");
        }
        Condition::Notify(name) => {
            ui.label("notify");
            changed |= ui
                .add(egui::TextEdit::singleline(name).desired_width(100.0))
                .changed();
        }
    });
    changed
}

fn transition_ui(
    ui: &mut egui::Ui,
    machine: NodeId,
    index: usize,
    t: &TransitionDef,
    states: &[String],
    docs: &AnimDocs,
    pending: &mut Vec<PendingEdit>,
) {
    let mut t = t.clone();
    let mut changed = false;
    ui.label(format!("Transition {index}"));
    let mut froms = vec!["Any".to_string()];
    froms.extend(states.iter().cloned());
    if let Some(v) = pick(
        ui,
        ("tr-from", machine, index),
        "From",
        t.from.as_deref().unwrap_or("Any"),
        &froms,
    ) {
        t.from = (v != "Any").then_some(v);
        changed = true;
    }
    if let Some(v) = pick(ui, ("tr-to", machine, index), "To", &t.to, states) {
        t.to = v;
        changed = true;
    }
    changed |= ui
        .add(egui::Slider::new(&mut t.duration, 0.0..=2.0).text("Duration (s)"))
        .changed();
    ui.horizontal(|ui| {
        ui.label("Blend");
        for b in TransitionBlend::ALL {
            changed |= ui
                .selectable_value(&mut t.blend, b, blend_label(b))
                .on_hover_text(blend_hint(b))
                .changed();
        }
    });
    if !t.blend.is_inertial() {
        ui.horizontal(|ui| {
            ui.label("Curve");
            for c in [TransitionCurve::Linear, TransitionCurve::Smooth] {
                changed |= ui.selectable_value(&mut t.curve, c, format!("{c:?}")).changed();
            }
        });
    }
    changed |= ui.checkbox(&mut t.interruptible, "Interruptible").changed();
    ui.label("Conditions (all must hold)");
    let mut remove = None;
    for (i, c) in t.conditions.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            if ui.small_button("×").clicked() {
                remove = Some(i);
            }
            changed |= condition_ui(ui, (machine, index, i), c, docs);
        });
    }
    if let Some(i) = remove {
        t.conditions.remove(i);
        changed = true;
    }
    ui.horizontal(|ui| {
        ui.label("Add");
        for kind in CONDITION_KINDS {
            if ui.small_button(kind).clicked()
                && let Some(c) = default_condition(kind, docs)
            {
                t.conditions.push(c);
                changed = true;
            }
        }
    });
    if ui.button("Delete transition").clicked() {
        pending.push(PendingEdit::new("delete transition", None, move |d| {
            d.graph.remove_transition(machine, index).map(|_| ())
        }));
        return;
    }
    if changed {
        pending.push(PendingEdit::new(
            "edit transition",
            Some(format!("transition:{machine}:{index}")),
            move |d| d.graph.set_transition(machine, index, t),
        ));
    }
}

/// Parses a parameter value of `kind` from JSON-ish numbers (agent protocol helper).
#[must_use]
pub fn value_of_kind(kind: &ParamValue, numbers: &[f32], flag: Option<bool>) -> ParamValue {
    let n = |i: usize| numbers.get(i).copied().unwrap_or(0.0);
    match kind {
        ParamValue::Float(_) => ParamValue::Float(n(0)),
        ParamValue::Int(_) => ParamValue::Int(n(0) as i32),
        ParamValue::Bool(_) => ParamValue::Bool(flag.unwrap_or(n(0) != 0.0)),
        ParamValue::Vec2(_) => ParamValue::Vec2(regia_core::math::Vec2::new(n(0), n(1))),
        ParamValue::Vec3(_) => ParamValue::Vec3(regia_core::math::Vec3::new(n(0), n(1), n(2))),
        ParamValue::Trigger => ParamValue::Trigger,
    }
}

/// Short button label of a transition blend mode.
#[must_use]
pub fn blend_label(blend: TransitionBlend) -> &'static str {
    match blend {
        TransitionBlend::Crossfade => "Crossfade",
        TransitionBlend::Inertialize => "Inertialize",
        TransitionBlend::InertializeSpring => "Inertialize (spring)",
    }
}

fn blend_hint(blend: TransitionBlend) -> &'static str {
    match blend {
        TransitionBlend::Crossfade => "Evaluate both states for the duration and cross-fade them",
        TransitionBlend::Inertialize => {
            "Switch to the target at once; decay the pose offset with Bollo's quintic (overshoot-safe, cheaper)"
        }
        TransitionBlend::InertializeSpring => {
            "Switch to the target at once; decay the pose offset with a critically damped spring"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_conditions_pick_matching_parameters() {
        let mut docs = AnimDocs::default();
        docs.graph.set_param("Speed", ParamValue::Float(0.0));
        docs.graph.set_param("Jump", ParamValue::Trigger);
        assert_eq!(
            default_condition("trigger", &docs),
            Some(Condition::Trigger { param: "Jump".into() })
        );
        assert!(
            matches!(default_condition("float", &docs), Some(Condition::Float { param, .. }) if param == "Speed")
        );
        assert!(default_condition("teleport", &docs).is_none());
        assert_eq!(
            value_of_kind(&ParamValue::Vec3(regia_core::math::Vec3::ZERO), &[1.0, 2.0], None),
            ParamValue::Vec3(regia_core::math::Vec3::new(1.0, 2.0, 0.0))
        );
    }

    /// Review finding 3 (ADR-8732): "Add joint" on a body aim node never appends an empty name
    /// (the save would refuse it, the runtime failed every frame with `MissingJoint("")`).
    #[test]
    fn body_aim_add_joint_never_appends_an_empty_name() {
        let rig: Vec<String> = ["spine", "neck", "head"].map(String::from).to_vec();
        assert_eq!(next_turning_joint(&rig, &[]), Some("spine".into()));
        assert_eq!(
            next_turning_joint(&rig, &[("spine".into(), 1.0)]),
            Some("neck".into())
        );
        let all: Vec<(String, f32)> = rig.iter().map(|j| (j.clone(), 1.0)).collect();
        assert_eq!(next_turning_joint(&rig, &all), Some("spine".into()));
        assert_eq!(
            next_turning_joint(&[], &[]),
            None,
            "no rig joints: the button is disabled"
        );
    }
}
