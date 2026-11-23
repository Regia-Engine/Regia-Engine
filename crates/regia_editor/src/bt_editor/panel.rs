// SPDX-License-Identifier: MPL-2.0
//! The Behavior Tree panel: toolbar (new/open/save, undo/redo, validity), a node palette, the tree
//! as an indented outline with the live status per node while the game plays, and an inspector
//! for the selected node (name, parameters, reorder, connect, remove) with the validation issues.
//! Edits are collected while drawing and applied through [`super::edit`] afterwards.

use regia_ai::bt_edit::{BtIssue, BtOp, BtSeverity, is_childless, is_decorator};
use regia_ai::{
    AbortMode, BehaviorTreeDef, BtCompareOp, BtLiteral, BtStatus, BtValueKind, DefKind, ParallelPolicy,
};
use regia_ecs::prelude::*;

use super::{BtEditor, diagnose, edit, palette_tasks, resolve_path};

/// Every node kind, in palette order.
pub const KINDS: [DefKind; 12] = [
    DefKind::Selector,
    DefKind::Sequence,
    DefKind::Parallel,
    DefKind::Inverter,
    DefKind::Condition,
    DefKind::Compare,
    DefKind::Cooldown,
    DefKind::Repeat,
    DefKind::Timeout,
    DefKind::Service,
    DefKind::StateTree,
    DefKind::Leaf,
];

/// The panel's own state (not part of the document; not undone).
#[derive(Default)]
pub struct PanelState {
    /// Open / save-as path field.
    path_field: String,
    /// The revision at which New or Open warned about unsaved work.
    discard_armed: Option<u64>,
    /// Text buffer of the selected node's name: (node, revision loaded at, text).
    name_text: Option<(usize, u64, String)>,
}

/// What the panel asks for after drawing.
enum Action {
    New,
    Open(String),
    Save(Option<String>),
    Undo,
    Redo,
    Edit {
        label: String,
        merge_key: Option<String>,
        ops: Vec<BtOp>,
        select_created: bool,
    },
    Select(Option<usize>),
    Watch(u64),
}

/// The kind's name as the file spells it.
#[must_use]
pub fn kind_name(kind: DefKind) -> String {
    ron::ser::to_string(&kind).unwrap_or_default()
}

fn status_text(status: BtStatus) -> &'static str {
    match status {
        BtStatus::Success => "success",
        BtStatus::Failure => "failure",
        BtStatus::Running => "running",
    }
}

/// Draws the panel.
pub fn bt_editor_panel(ui: &mut egui::Ui, world: &mut World) {
    if !world.contains_resource::<BtEditor>() {
        world.init_resource::<BtEditor>();
    }
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.next_undo(), s.next_redo()))
        .unwrap_or_default();
    let issues = diagnose(world);
    let tasks = palette_tasks(world);
    let mut actions = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<BtEditor>| {
        draw(ui, &mut editor, &issues, &tasks, (undo, redo), &mut actions);
    });
    for action in actions {
        perform(world, action);
    }
}

fn may_replace_document(editor: &mut BtEditor) -> bool {
    if !editor.has_unsaved_work() || editor.ui.discard_armed == Some(editor.revision) {
        return true;
    }
    editor.ui.discard_armed = Some(editor.revision);
    editor.toast = "unsaved changes: press again to discard them".into();
    false
}

fn perform(world: &mut World, action: Action) {
    if matches!(action, Action::New | Action::Open(_))
        && !may_replace_document(&mut world.resource_mut::<BtEditor>())
    {
        return;
    }
    let result: Result<(), String> = match action {
        Action::Undo => {
            crate::command::undo_world(world);
            Ok(())
        }
        Action::Redo => {
            crate::command::redo_world(world);
            Ok(())
        }
        Action::New => {
            world.resource_mut::<BtEditor>().new_tree(None);
            Ok(())
        }
        Action::Open(path) => {
            let path = resolve_path(world, &path);
            world.resource_mut::<BtEditor>().open(&path)
        }
        Action::Save(path) => {
            let path = path.map(|p| resolve_path(world, &p));
            let valid = regia_ai::bt_edit::is_valid(&diagnose(world));
            world
                .resource_mut::<BtEditor>()
                .save(path.as_deref(), valid)
                .map(|_| ())
        }
        Action::Edit {
            label,
            merge_key,
            ops,
            select_created,
        } => match edit(world, &label, merge_key, &ops) {
            Ok(created) => {
                let mut editor = world.resource_mut::<BtEditor>();
                if select_created && let Some(node) = created.iter().rev().flatten().next() {
                    editor.selected = Some(*node);
                }
                let count = editor.doc.nodes.len();
                if editor.selected.is_some_and(|s| s >= count) {
                    editor.selected = None;
                }
                Ok(())
            }
            Err(e) => Err(e.error.to_string()),
        },
        Action::Select(node) => {
            world.resource_mut::<BtEditor>().selected = node;
            Ok(())
        }
        Action::Watch(entity) => {
            world.resource_mut::<BtEditor>().watch = Some(entity);
            Ok(())
        }
    };
    if let Err(err) = result {
        world.resource_mut::<BtEditor>().toast = err;
    }
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut BtEditor,
    issues: &[BtIssue],
    tasks: &[String],
    (undo, redo): (Option<String>, Option<String>),
    actions: &mut Vec<Action>,
) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let errors = issues.iter().filter(|i| i.severity == BtSeverity::Error).count();
    ui.horizontal(|ui| {
        if ui.button("New").clicked() {
            actions.push(Action::New);
        }
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.path_field)
                .hint_text("assets/ai/….bt.ron")
                .desired_width(220.0),
        );
        if ui.button("Open").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Open(editor.ui.path_field.clone()));
        }
        if ui
            .add_enabled(editor.path.is_some(), egui::Button::new("Save"))
            .clicked()
        {
            actions.push(Action::Save(None));
        }
        if ui.button("Save as").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Save(Some(editor.ui.path_field.clone())));
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
        let name = editor
            .path
            .as_ref()
            .map_or_else(|| "(unsaved tree)".to_string(), |p| p.display().to_string());
        ui.weak(if editor.dirty() { format!("{name} *") } else { name });
        if errors == 0 {
            ui.colored_label(accents.ok, "compiles");
        } else {
            ui.colored_label(accents.error_text, format!("{errors} error(s)"));
        }
        if !editor.toast.is_empty() {
            ui.weak(&editor.toast);
        }
    });
    ui.separator();
    if !editor.is_open() {
        ui.weak("No tree is open. New starts an empty one; Open reads a .bt.ron.");
        return;
    }
    palette_ui(ui, editor, tasks, actions);
    ui.separator();
    live_header(ui, editor, actions);
    outline_ui(ui, editor, issues, actions);
    ui.separator();
    inspector_ui(ui, editor, tasks, actions);
    ui.separator();
    issues_ui(ui, issues, actions);
}

/// Whether node `parent` can take one more child.
fn can_take_child(def: &BehaviorTreeDef, parent: usize) -> bool {
    let Some(node) = def.nodes.get(parent) else {
        return false;
    };
    !is_childless(node.kind) && !(is_decorator(node.kind) && !node.children.is_empty())
}

fn add_op(kind: DefKind, parent: Option<usize>, task: Option<String>) -> BtOp {
    BtOp::AddNode {
        kind,
        name: None,
        parent,
        index: None,
        task,
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

fn palette_ui(ui: &mut egui::Ui, editor: &BtEditor, tasks: &[String], actions: &mut Vec<Action>) {
    let target = editor.selected.filter(|s| can_take_child(&editor.doc, *s));
    let note = match (target, editor.doc.nodes.is_empty()) {
        (Some(_), _) => "adds under the selected node",
        (None, true) => "adds the root",
        (None, false) => "adds an unconnected node (connect it in the inspector)",
    };
    ui.horizontal_wrapped(|ui| {
        ui.label("Add:");
        for kind in KINDS {
            if kind == DefKind::Leaf {
                continue;
            }
            if ui.button(kind_name(kind)).on_hover_text(note).clicked() {
                actions.push(Action::Edit {
                    label: format!("add {}", kind_name(kind)),
                    merge_key: None,
                    ops: vec![add_op(kind, target, None)],
                    select_created: true,
                });
            }
        }
        ui.separator();
        ui.label("Leaf:");
        if tasks.is_empty() && ui.button("leaf").on_hover_text(note).clicked() {
            actions.push(Action::Edit {
                label: "add leaf".into(),
                merge_key: None,
                ops: vec![add_op(DefKind::Leaf, target, None)],
                select_created: true,
            });
        }
        for task in tasks {
            if ui.button(task).on_hover_text(note).clicked() {
                actions.push(Action::Edit {
                    label: format!("add leaf {task}"),
                    merge_key: None,
                    ops: vec![add_op(DefKind::Leaf, target, Some(task.clone()))],
                    select_created: true,
                });
            }
        }
    });
}

fn live_header(ui: &mut egui::Ui, editor: &BtEditor, actions: &mut Vec<Action>) {
    let Some(live) = &editor.live else {
        if editor.agents.is_empty() {
            ui.weak("Live state: press Play; a running tree shows its node statuses here.");
        }
        return;
    };
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("bt-live-agent")
            .selected_text(format!("agent {}", live.entity))
            .show_ui(ui, |ui| {
                // A screenful at a time (ADR-7306): a crowd can run one tree thousands of times.
                crate::virtual_list::rows(
                    ui,
                    "bt-live-agent-rows",
                    editor.agents.len(),
                    crate::virtual_list::DROP_DOWN_HEIGHT,
                    |ui, i| {
                        let agent = editor.agents[i];
                        if ui
                            .selectable_label(agent == live.entity, format!("agent {agent}"))
                            .clicked()
                        {
                            actions.push(Action::Watch(agent));
                        }
                    },
                );
            });
        ui.label(format!(
            "tick {}  root {}",
            live.ticks,
            status_text(live.last_status)
        ));
        if !live.matches_document {
            ui.weak("the running tree differs from the open document: statuses are not shown on nodes");
        }
    });
}

fn outline_rows(def: &BehaviorTreeDef) -> (Vec<(usize, usize)>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut seen = vec![false; def.nodes.len()];
    if def.root < def.nodes.len() {
        let mut stack = vec![(def.root, 0usize)];
        while let Some((index, depth)) = stack.pop() {
            if std::mem::replace(&mut seen[index], true) {
                continue;
            }
            rows.push((index, depth));
            for child in def.nodes[index].children.iter().rev() {
                if *child < def.nodes.len() {
                    stack.push((*child, depth + 1));
                }
            }
        }
    }
    let orphans = (0..def.nodes.len()).filter(|i| !seen[*i]).collect();
    (rows, orphans)
}

fn outline_ui(ui: &mut egui::Ui, editor: &BtEditor, issues: &[BtIssue], actions: &mut Vec<Action>) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    let (rows, orphans) = outline_rows(&editor.doc);
    let live = editor.live.as_ref().filter(|l| l.matches_document);
    let mut row_ui = |ui: &mut egui::Ui, index: usize, depth: usize| {
        let node = &editor.doc.nodes[index];
        let marks = issues.iter().filter(|i| i.node == Some(index)).count();
        let mut text = format!(
            "{}{} {}{}",
            "    ".repeat(depth),
            kind_name(node.kind),
            node.name,
            if marks > 0 {
                format!("  ⚠{marks}")
            } else {
                String::new()
            }
        );
        if matches!(node.kind, DefKind::Leaf | DefKind::Service | DefKind::StateTree) {
            text.push_str(&format!("  [{}]", node.task));
        }
        let mut label = egui::RichText::new(text);
        if let Some(status) = live.and_then(|l| l.node_status.get(index)) {
            let color = match status {
                BtStatus::Running => accents.active,
                BtStatus::Success => accents.ok,
                BtStatus::Failure => accents.alert,
            };
            label = label.color(color);
        }
        let response = ui.selectable_label(editor.selected == Some(index), label);
        if response.clicked() {
            actions.push(Action::Select(Some(index)));
        }
        if let Some(status) = live.and_then(|l| l.node_status.get(index)) {
            response.on_hover_text(status_text(*status));
        }
    };
    egui::ScrollArea::vertical()
        .id_salt("bt-outline")
        .max_height(240.0)
        .show(ui, |ui| {
            if editor.doc.nodes.is_empty() {
                ui.weak("The tree is empty: add a node to make the root.");
            }
            for (index, depth) in rows {
                row_ui(ui, index, depth);
            }
            if !orphans.is_empty() {
                ui.separator();
                ui.weak("Unconnected nodes");
                for index in orphans {
                    row_ui(ui, index, 0);
                }
            }
        });
}

/// The fields of a `set_node` op a panel edit changes; the rest stay.
#[derive(Default)]
struct Patch {
    name: Option<String>,
    key: Option<String>,
    seconds: Option<f32>,
    times: Option<u32>,
    clear_times: bool,
    task: Option<String>,
    policy: Option<ParallelPolicy>,
    abort: Option<AbortMode>,
    op: Option<BtCompareOp>,
    value_kind: Option<BtValueKind>,
    literal: Option<BtLiteral>,
    literal_hi: Option<BtLiteral>,
    clear_literal: bool,
    clear_literal_hi: bool,
    rhs_key: Option<String>,
}

fn patch(node: usize, p: Patch) -> BtOp {
    BtOp::SetNode {
        node,
        name: p.name,
        kind: None,
        key: p.key,
        seconds: p.seconds,
        times: p.times,
        clear_times: p.clear_times,
        task: p.task,
        policy: p.policy,
        abort: p.abort,
        op: p.op,
        value_kind: p.value_kind,
        literal: p.literal,
        literal_hi: p.literal_hi,
        clear_literal: p.clear_literal,
        clear_literal_hi: p.clear_literal_hi,
        rhs_key: p.rhs_key,
    }
}

fn allows_abort(kind: DefKind) -> bool {
    matches!(
        kind,
        DefKind::Condition
            | DefKind::Compare
            | DefKind::Inverter
            | DefKind::Cooldown
            | DefKind::Repeat
            | DefKind::Timeout
            | DefKind::Service
    )
}

fn push_field(actions: &mut Vec<Action>, index: usize, label: &str, merge: &str, fields: Patch) {
    actions.push(Action::Edit {
        label: label.into(),
        merge_key: Some(format!("{merge}:{index}")),
        ops: vec![patch(index, fields)],
        select_created: false,
    });
}

fn abort_row(ui: &mut egui::Ui, index: usize, current: AbortMode, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.label("abort");
        for mode in [
            AbortMode::None,
            AbortMode::AbortSelf,
            AbortMode::LowerPriority,
            AbortMode::Both,
        ] {
            let name = ron::ser::to_string(&mode).unwrap_or_default();
            if ui.selectable_label(current == mode, name).clicked() {
                push_field(
                    actions,
                    index,
                    "set abort",
                    "abort",
                    Patch {
                        abort: Some(mode),
                        ..Patch::default()
                    },
                );
            }
        }
    });
}

fn compare_fields(ui: &mut egui::Ui, index: usize, node: &regia_ai::DefNode, actions: &mut Vec<Action>) {
    let mut key = node.key.clone();
    ui.horizontal(|ui| {
        ui.label("blackboard key");
        if ui.text_edit_singleline(&mut key).changed() {
            push_field(
                actions,
                index,
                "set key",
                "key",
                Patch {
                    key: Some(key.clone()),
                    ..Patch::default()
                },
            );
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("op");
        for op in [
            BtCompareOp::Eq,
            BtCompareOp::Ne,
            BtCompareOp::Lt,
            BtCompareOp::Le,
            BtCompareOp::Gt,
            BtCompareOp::Ge,
            BtCompareOp::IsSet,
            BtCompareOp::InRange,
        ] {
            let name = ron::ser::to_string(&op).unwrap_or_default();
            if ui.selectable_label(node.op == op, name).clicked() {
                push_field(
                    actions,
                    index,
                    "set op",
                    "op",
                    Patch {
                        op: Some(op),
                        ..Patch::default()
                    },
                );
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("type");
        for kind in [
            BtValueKind::Bool,
            BtValueKind::Int,
            BtValueKind::Float,
            BtValueKind::Tag,
            BtValueKind::Entity,
        ] {
            let name = ron::ser::to_string(&kind).unwrap_or_default();
            if ui.selectable_label(node.value_kind == kind, name).clicked() {
                push_field(
                    actions,
                    index,
                    "set value kind",
                    "kind",
                    Patch {
                        value_kind: Some(kind),
                        ..Patch::default()
                    },
                );
            }
        }
    });
    literal_row(
        ui,
        index,
        "literal",
        node.value_kind,
        node.literal.as_ref(),
        false,
        actions,
    );
    if node.op == BtCompareOp::InRange {
        literal_row(
            ui,
            index,
            "high",
            node.value_kind,
            node.literal_hi.as_ref(),
            true,
            actions,
        );
    }
    let mut rhs = node.rhs_key.clone();
    ui.horizontal(|ui| {
        ui.label("other key");
        if ui.text_edit_singleline(&mut rhs).changed() {
            push_field(
                actions,
                index,
                "set other key",
                "rhs",
                Patch {
                    rhs_key: Some(rhs.clone()),
                    ..Patch::default()
                },
            );
        }
    });
}

fn literal_row(
    ui: &mut egui::Ui,
    index: usize,
    label: &str,
    kind: BtValueKind,
    current: Option<&BtLiteral>,
    high: bool,
    actions: &mut Vec<Action>,
) {
    let store = |value: BtLiteral| {
        if high {
            Patch {
                literal_hi: Some(value),
                ..Patch::default()
            }
        } else {
            Patch {
                literal: Some(value),
                ..Patch::default()
            }
        }
    };
    ui.horizontal(|ui| {
        ui.label(label);
        match kind {
            BtValueKind::Bool => {
                let mut on = matches!(current, Some(BtLiteral::Bool(true)));
                if ui.checkbox(&mut on, "true").changed() {
                    push_field(actions, index, "set literal", label, store(BtLiteral::Bool(on)));
                }
            }
            BtValueKind::Int => {
                let mut value = match current {
                    Some(BtLiteral::Int(n)) => *n,
                    _ => 0,
                };
                if ui.add(egui::DragValue::new(&mut value)).changed() {
                    push_field(actions, index, "set literal", label, store(BtLiteral::Int(value)));
                }
            }
            BtValueKind::Float => {
                let mut value = match current {
                    Some(BtLiteral::Float(n)) => *n,
                    _ => 0.0,
                };
                if ui.add(egui::DragValue::new(&mut value).speed(0.05)).changed() {
                    push_field(
                        actions,
                        index,
                        "set literal",
                        label,
                        store(BtLiteral::Float(value)),
                    );
                }
            }
            BtValueKind::Tag => {
                let mut text = match current {
                    Some(BtLiteral::Tag(text)) => text.clone(),
                    _ => String::new(),
                };
                if ui.text_edit_singleline(&mut text).changed() {
                    push_field(
                        actions,
                        index,
                        "set literal",
                        label,
                        store(BtLiteral::Tag(text.clone())),
                    );
                }
            }
            BtValueKind::Entity => {
                let mut value = match current {
                    Some(BtLiteral::Entity(n)) => *n,
                    _ => 0,
                };
                if ui.add(egui::DragValue::new(&mut value)).changed() {
                    push_field(
                        actions,
                        index,
                        "set literal",
                        label,
                        store(BtLiteral::Entity(value)),
                    );
                }
            }
        }
    });
}

fn inspector_ui(ui: &mut egui::Ui, editor: &mut BtEditor, tasks: &[String], actions: &mut Vec<Action>) {
    let Some(index) = editor.selected.filter(|s| *s < editor.doc.nodes.len()) else {
        ui.weak("Select a node to edit it.");
        return;
    };
    let node = editor.doc.nodes[index].clone();
    let parent = editor.doc.nodes.iter().position(|n| n.children.contains(&index));
    let revision = editor.revision;
    ui.label(format!("Node {index}: {}", kind_name(node.kind)));
    // Name.
    let stale = editor
        .ui
        .name_text
        .as_ref()
        .is_none_or(|(n, r, _)| *n != index || *r != revision);
    if stale {
        editor.ui.name_text = Some((index, revision, node.name.clone()));
    }
    if let Some((_, _, text)) = editor.ui.name_text.as_mut() {
        ui.horizontal(|ui| {
            ui.label("name");
            if ui.text_edit_singleline(text).changed() {
                actions.push(Action::Edit {
                    label: "rename".into(),
                    merge_key: Some(format!("name:{index}")),
                    ops: vec![patch(
                        index,
                        Patch {
                            name: Some(text.clone()),
                            ..Patch::default()
                        },
                    )],
                    select_created: false,
                });
            }
        });
    }
    // Parameters by kind.
    match node.kind {
        DefKind::Condition => {
            let mut key = node.key.clone();
            ui.horizontal(|ui| {
                ui.label("blackboard key");
                if ui.text_edit_singleline(&mut key).changed() {
                    actions.push(Action::Edit {
                        label: "set key".into(),
                        merge_key: Some(format!("key:{index}")),
                        ops: vec![patch(
                            index,
                            Patch {
                                key: Some(key.clone()),
                                ..Patch::default()
                            },
                        )],
                        select_created: false,
                    });
                }
            });
        }
        DefKind::Cooldown | DefKind::Timeout => {
            let mut seconds = node.seconds;
            ui.horizontal(|ui| {
                ui.label("seconds");
                if ui
                    .add(egui::DragValue::new(&mut seconds).speed(0.05).range(0.0..=3600.0))
                    .changed()
                {
                    actions.push(Action::Edit {
                        label: "set seconds".into(),
                        merge_key: Some(format!("seconds:{index}")),
                        ops: vec![patch(
                            index,
                            Patch {
                                seconds: Some(seconds),
                                ..Patch::default()
                            },
                        )],
                        select_created: false,
                    });
                }
            });
        }
        DefKind::Repeat => {
            let mut forever = node.times.is_none();
            let mut times = node.times.unwrap_or(1);
            ui.horizontal(|ui| {
                if ui.checkbox(&mut forever, "repeat for ever").changed() {
                    let op = if forever {
                        patch(
                            index,
                            Patch {
                                clear_times: true,
                                ..Patch::default()
                            },
                        )
                    } else {
                        patch(
                            index,
                            Patch {
                                times: Some(times),
                                ..Patch::default()
                            },
                        )
                    };
                    actions.push(Action::Edit {
                        label: "set repeat".into(),
                        merge_key: None,
                        ops: vec![op],
                        select_created: false,
                    });
                }
                if !forever
                    && ui
                        .add(egui::DragValue::new(&mut times).range(1..=1_000_000))
                        .changed()
                {
                    actions.push(Action::Edit {
                        label: "set times".into(),
                        merge_key: Some(format!("times:{index}")),
                        ops: vec![patch(
                            index,
                            Patch {
                                times: Some(times),
                                ..Patch::default()
                            },
                        )],
                        select_created: false,
                    });
                }
            });
        }
        DefKind::Parallel => {
            ui.horizontal(|ui| {
                ui.label("policy");
                for policy in [ParallelPolicy::RequireAll, ParallelPolicy::RequireOne] {
                    let name = ron::ser::to_string(&policy).unwrap_or_default();
                    if ui.selectable_label(node.policy == policy, name).clicked() {
                        actions.push(Action::Edit {
                            label: "set policy".into(),
                            merge_key: None,
                            ops: vec![patch(
                                index,
                                Patch {
                                    policy: Some(policy),
                                    ..Patch::default()
                                },
                            )],
                            select_created: false,
                        });
                    }
                }
            });
        }
        DefKind::Leaf | DefKind::Service => {
            let mut task = node.task.clone();
            ui.horizontal(|ui| {
                ui.label("task");
                let changed = ui.text_edit_singleline(&mut task).changed();
                egui::ComboBox::from_id_salt("bt-task-pick")
                    .selected_text("registered")
                    .show_ui(ui, |ui| {
                        for t in tasks {
                            if ui.selectable_label(*t == node.task, t).clicked() {
                                task.clone_from(t);
                                actions.push(Action::Edit {
                                    label: "pick task".into(),
                                    merge_key: None,
                                    ops: vec![patch(
                                        index,
                                        Patch {
                                            task: Some(t.clone()),
                                            ..Patch::default()
                                        },
                                    )],
                                    select_created: false,
                                });
                            }
                        }
                    });
                if changed {
                    actions.push(Action::Edit {
                        label: "set task".into(),
                        merge_key: Some(format!("task:{index}")),
                        ops: vec![patch(
                            index,
                            Patch {
                                task: Some(task.clone()),
                                ..Patch::default()
                            },
                        )],
                        select_created: false,
                    });
                }
            });
            if node.kind == DefKind::Service {
                let mut seconds = node.seconds;
                ui.horizontal(|ui| {
                    ui.label("seconds");
                    if ui
                        .add(egui::DragValue::new(&mut seconds).speed(0.05).range(0.0..=3600.0))
                        .changed()
                    {
                        actions.push(Action::Edit {
                            label: "set seconds".into(),
                            merge_key: Some(format!("seconds:{index}")),
                            ops: vec![patch(
                                index,
                                Patch {
                                    seconds: Some(seconds),
                                    ..Patch::default()
                                },
                            )],
                            select_created: false,
                        });
                    }
                });
            }
        }
        DefKind::Compare => compare_fields(ui, index, &node, actions),
        DefKind::StateTree => {
            let mut task = node.task.clone();
            ui.horizontal(|ui| {
                ui.label("state tree");
                if ui.text_edit_singleline(&mut task).changed() {
                    push_field(
                        actions,
                        index,
                        "set state tree",
                        "task",
                        Patch {
                            task: Some(task.clone()),
                            ..Patch::default()
                        },
                    );
                }
            });
        }
        DefKind::Selector | DefKind::Sequence | DefKind::Inverter => {}
    }
    if allows_abort(node.kind) {
        abort_row(ui, index, node.abort, actions);
    }
    // Structure.
    ui.horizontal_wrapped(|ui| {
        if let Some(p) = parent {
            let at = editor.doc.nodes[p]
                .children
                .iter()
                .position(|c| *c == index)
                .unwrap_or(0);
            let last = editor.doc.nodes[p].children.len().saturating_sub(1);
            if ui.add_enabled(at > 0, egui::Button::new("Move up")).clicked() {
                actions.push(Action::Edit {
                    label: "move up".into(),
                    merge_key: None,
                    ops: vec![BtOp::Reorder {
                        parent: p,
                        from: at,
                        to: at - 1,
                    }],
                    select_created: false,
                });
            }
            if ui
                .add_enabled(at < last, egui::Button::new("Move down"))
                .clicked()
            {
                actions.push(Action::Edit {
                    label: "move down".into(),
                    merge_key: None,
                    ops: vec![BtOp::Reorder {
                        parent: p,
                        from: at,
                        to: at + 1,
                    }],
                    select_created: false,
                });
            }
            if ui.button("Disconnect").clicked() {
                actions.push(Action::Edit {
                    label: "disconnect".into(),
                    merge_key: None,
                    ops: vec![BtOp::Disconnect {
                        parent: p,
                        child: index,
                    }],
                    select_created: false,
                });
            }
        }
        if editor.doc.root != index && ui.button("Make root").clicked() {
            actions.push(Action::Edit {
                label: "set root".into(),
                merge_key: None,
                ops: vec![BtOp::SetRoot { node: index }],
                select_created: false,
            });
        }
        if ui
            .button("Remove")
            .on_hover_text("removes the node and everything below it")
            .clicked()
        {
            actions.push(Action::Edit {
                label: "remove".into(),
                merge_key: None,
                ops: vec![BtOp::RemoveNode {
                    node: index,
                    subtree: true,
                }],
                select_created: false,
            });
        }
        if ui.button("Remove (keep children)").clicked() {
            actions.push(Action::Edit {
                label: "remove".into(),
                merge_key: None,
                ops: vec![BtOp::RemoveNode {
                    node: index,
                    subtree: false,
                }],
                select_created: false,
            });
        }
    });
    // Connect under another node.
    let candidates: Vec<usize> = (0..editor.doc.nodes.len())
        .filter(|c| *c != index && Some(*c) != parent && can_take_child(&editor.doc, *c))
        .collect();
    if !candidates.is_empty() {
        ui.horizontal(|ui| {
            ui.label("connect under");
            egui::ComboBox::from_id_salt("bt-connect")
                .selected_text("choose a node")
                .show_ui(ui, |ui| {
                    for c in candidates {
                        let n = &editor.doc.nodes[c];
                        if ui
                            .selectable_label(false, format!("{c}: {} {}", kind_name(n.kind), n.name))
                            .clicked()
                        {
                            actions.push(Action::Edit {
                                label: "connect".into(),
                                merge_key: None,
                                ops: vec![BtOp::Connect {
                                    parent: c,
                                    child: index,
                                    index: None,
                                }],
                                select_created: false,
                            });
                        }
                    }
                });
        });
    }
}

fn issues_ui(ui: &mut egui::Ui, issues: &[BtIssue], actions: &mut Vec<Action>) {
    let accents = crate::theme::ui_tokens(ui.ctx()).accents;
    if issues.is_empty() {
        ui.colored_label(accents.ok, "no issues");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("bt-issues")
        .max_height(120.0)
        .show(ui, |ui| {
            for issue in issues {
                let color = if issue.severity == BtSeverity::Error {
                    accents.error_text
                } else {
                    accents.log_warn
                };
                let text = egui::RichText::new(format!("{}: {}", issue.code, issue.message)).color(color);
                if ui.selectable_label(false, text).clicked() {
                    actions.push(Action::Select(issue.node));
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_once(world: &mut World, ctx: &egui::Context) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| bt_editor_panel(ui, world));
        output.textures_delta.clear();
    }

    fn open_with_tree(world: &mut World) {
        world.insert_resource(crate::command::UndoStack::new());
        let mut editor = BtEditor::default();
        editor.new_tree(None);
        world.insert_resource(editor);
        let ops = [
            add_op(DefKind::Selector, None, None),
            add_op(DefKind::Leaf, Some(0), Some("game::A".into())),
            add_op(DefKind::Cooldown, Some(0), None),
            add_op(DefKind::Leaf, Some(2), Some("game::B".into())),
        ];
        edit(world, "build", None, &ops).expect("builds");
    }

    #[test]
    fn the_panel_draws_headless_without_editing() {
        let mut world = World::new();
        open_with_tree(&mut world);
        let doc = world.resource::<BtEditor>().doc.clone();
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        world.resource_mut::<BtEditor>().selected = Some(2);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            draw_once(&mut world, &ctx);
        }
        let editor = world.resource::<BtEditor>();
        assert_eq!(editor.doc, doc, "drawing does not edit the document");
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), depth);
        assert_eq!(editor.selected, Some(2));
    }

    #[test]
    fn the_panel_draws_with_live_state_and_an_empty_tree() {
        let mut world = World::new();
        open_with_tree(&mut world);
        {
            let mut editor = world.resource_mut::<BtEditor>();
            editor.selected = Some(1);
            editor.agents = vec![7];
            editor.live = Some(super::super::LiveView {
                entity: 7,
                ticks: 3,
                last_status: BtStatus::Running,
                node_status: vec![
                    BtStatus::Running,
                    BtStatus::Failure,
                    BtStatus::Running,
                    BtStatus::Success,
                ],
                changed_at: vec![0; 4],
                matches_document: true,
            });
        }
        let ctx = egui::Context::default();
        draw_once(&mut world, &ctx);
        let mut empty = World::new();
        empty.insert_resource(crate::command::UndoStack::new());
        let mut editor = BtEditor::default();
        editor.new_tree(None);
        empty.insert_resource(editor);
        draw_once(&mut empty, &ctx);
    }

    #[test]
    fn outline_lists_the_tree_depth_first_and_the_unconnected_nodes() {
        let mut world = World::new();
        open_with_tree(&mut world);
        edit(
            &mut world,
            "orphan",
            None,
            &[add_op(DefKind::Sequence, None, None)],
        )
        .expect("adds");
        let (rows, orphans) = outline_rows(&world.resource::<BtEditor>().doc);
        assert_eq!(rows, vec![(0, 0), (1, 1), (2, 1), (3, 2)]);
        assert_eq!(orphans, vec![4]);
    }

    #[test]
    fn panel_actions_are_undoable_edits() {
        let mut world = World::new();
        open_with_tree(&mut world);
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        perform(
            &mut world,
            Action::Edit {
                label: "move down".into(),
                merge_key: None,
                ops: vec![BtOp::Reorder {
                    parent: 0,
                    from: 0,
                    to: 1,
                }],
                select_created: false,
            },
        );
        assert_eq!(world.resource::<BtEditor>().doc.nodes[0].children, vec![2, 1]);
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1
        );
        perform(&mut world, Action::Undo);
        assert_eq!(world.resource::<BtEditor>().doc.nodes[0].children, vec![1, 2]);
        // A refused op becomes the toast and changes nothing.
        perform(
            &mut world,
            Action::Edit {
                label: "bad".into(),
                merge_key: None,
                ops: vec![BtOp::Connect {
                    parent: 1,
                    child: 0,
                    index: None,
                }],
                select_created: false,
            },
        );
        assert!(world.resource::<BtEditor>().toast.contains("would_cycle"));
    }

    #[test]
    fn new_and_open_warn_before_discarding_unsaved_work() {
        let mut world = World::new();
        open_with_tree(&mut world);
        perform(&mut world, Action::New);
        assert!(world.resource::<BtEditor>().toast.contains("unsaved"));
        assert_eq!(
            world.resource::<BtEditor>().doc.nodes.len(),
            4,
            "the first press only warns"
        );
        perform(&mut world, Action::New);
        assert!(world.resource::<BtEditor>().doc.nodes.is_empty());
    }
}
