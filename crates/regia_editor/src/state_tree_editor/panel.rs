// SPDX-License-Identifier: MPL-2.0
//! The State Tree Editor panel: a toolbar (new, open, save, undo, redo), a node view of the
//! hierarchy (parent links) and the transitions (arrows) with the active path highlighted while
//! the game plays, a palette (new child state, the registered tasks, transition triggers), an
//! inspector for the selected state (name, parent, selection, weight, fallback, tasks, parameters,
//! transitions) with its validation issues inline, the full issue list, and the play view (agents,
//! active path, the last save's reload outcome). Edits are collected while drawing and applied
//! through [`super::edit`] afterwards, so every one is an undo step.

use std::collections::{BTreeMap, BTreeSet};

use regia_ai::state_tree::{
    Condition, Fallback, Literal, Selection, StateDef, StateTreeDef, Target, Trigger, UtilityDef,
};
use regia_ai::state_tree_edit::{self, StIssue, StOp, StSeverity};
use regia_ecs::prelude::*;

use super::{StEditor, diagnose, edit, palette_tasks, resolve_path, save};

/// Width of a state box in the node view.
const NODE_W: f32 = 132.0;
/// Height of a state box.
const NODE_H: f32 = 24.0;
/// Horizontal step per hierarchy level.
const LEVEL_DX: f32 = 168.0;
/// Vertical step per row.
const ROW_DY: f32 = 34.0;

/// The panel's own state (not part of the document; not undone).
#[derive(Default)]
pub struct PanelState {
    /// Open / save-as path field.
    path_field: String,
    /// The revision at which New or Open warned about unsaved work.
    discard_armed: Option<u64>,
    /// Name of a new child state.
    new_state: String,
    /// Rename buffer: (state, text).
    rename: Option<(String, String)>,
    /// Task chosen in the palette.
    task_pick: String,
    /// New parameter: key, kind (0 bool, 1 int, 2 float, 3 string), value text.
    param_key: String,
    param_kind: usize,
    param_value: String,
    /// New transition: trigger kind (index into `TRIGGERS`), event name, target kind (index into
    /// `TARGETS`), target state.
    trigger_kind: usize,
    event_name: String,
    target_kind: usize,
    target_state: String,
    /// Condition text buffers (RON), keyed `enter` or `t<index>` for the selected state.
    conditions: BTreeMap<String, String>,
    /// The (document revision, state) the condition buffers were loaded at.
    conditions_at: Option<(u64, String)>,
    /// The tree-fields form (description, seed, state key).
    pub(crate) tree: TreeForm,
    /// The document revision the tree-fields form was loaded at.
    tree_at: Option<u64>,
    /// A new ambient task path.
    pub(crate) ambient_pick: String,
    /// The States list beside the node view: the keyboard route of the canvas (FU-5).
    pub(crate) keys: crate::canvas_keys::ListKeys,
    /// The state the States list focus belongs to (the focus follows it when rows move).
    pub(crate) keys_state: Option<String>,
    /// Ctrl+Shift+A on a row: focus the palette's new-state field on the next draw.
    pub(crate) focus_new_state: bool,
    /// A new include: prefix and path.
    pub(crate) include_prefix: String,
    pub(crate) include_path: String,
    /// Include edit buffers, keyed by the include's prefix: (new prefix, new path).
    pub(crate) include_edits: BTreeMap<String, (String, String)>,
    /// Where **List under** puts each include's root among the parent's children, keyed by the
    /// include's prefix: the child index it goes before (`None` or absent = the end).
    pub(crate) include_list_at: BTreeMap<String, Option<usize>>,
    /// The selected state's utility form: key, scale, bias (loaded with the selection).
    pub(crate) utility: Option<UtilityDef>,
    /// The (document revision, state) the utility form was loaded at.
    utility_at: Option<(u64, String)>,
}

/// The tree-fields form (`set_tree`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeForm {
    /// What the tree is for.
    pub description: String,
    /// The `Weighted` selection seed.
    pub seed: u64,
    /// The blackboard key the active leaf's name is written to (empty: none).
    pub state_key: String,
}

impl TreeForm {
    /// The tree fields of `def`.
    #[must_use]
    pub fn of(def: &StateTreeDef) -> Self {
        Self {
            description: def.description.clone(),
            seed: def.seed,
            state_key: def.state_key.clone(),
        }
    }

    /// The `set_tree` op carrying the fields that differ from `def` (`None` when none do).
    #[must_use]
    pub fn op(&self, def: &StateTreeDef) -> Option<StOp> {
        let description = (self.description != def.description).then(|| self.description.clone());
        let seed = (self.seed != def.seed).then_some(self.seed);
        let state_key = (self.state_key != def.state_key).then(|| self.state_key.clone());
        (description.is_some() || seed.is_some() || state_key.is_some()).then_some(StOp::SetTree {
            description,
            seed,
            state_key,
        })
    }
}

/// The `set_ambient` op that appends `task` to the tree's ambient tasks.
#[must_use]
pub fn ambient_add_op(def: &StateTreeDef, task: &str) -> StOp {
    let mut tasks = def.ambient.clone();
    tasks.push(task.to_string());
    StOp::SetAmbient { tasks }
}

/// The `set_ambient` op that removes ambient task `index`.
#[must_use]
pub fn ambient_remove_op(def: &StateTreeDef, index: usize) -> StOp {
    let mut tasks = def.ambient.clone();
    if index < tasks.len() {
        tasks.remove(index);
    }
    StOp::SetAmbient { tasks }
}

/// The `set_state` op that sets (`Some`) or clears (`None`) state `name`'s utility score.
#[must_use]
pub fn utility_op(name: &str, utility: Option<UtilityDef>) -> StOp {
    let mut op = set_state(name);
    if let StOp::SetState {
        utility: u,
        clear_utility,
        ..
    } = &mut op
    {
        *clear_utility = utility.is_none();
        *u = utility;
    }
    op
}

/// The `set_include` op changing the include `prefix` to `to_prefix` / `path` (unchanged fields left out;
/// `None` when nothing changes).
#[must_use]
pub fn include_set_op(prefix: &str, current_path: &str, to_prefix: &str, path: &str) -> Option<StOp> {
    let to_prefix = (to_prefix != prefix).then(|| to_prefix.to_string());
    let path = (path != current_path).then(|| path.to_string());
    (to_prefix.is_some() || path.is_some()).then(|| StOp::SetInclude {
        prefix: prefix.to_string(),
        to_prefix,
        path,
    })
}

/// Transition triggers, in palette order.
const TRIGGERS: [&str; 5] = ["Tick", "Event", "Succeeded", "Failed", "Completed"];
/// Transition targets, in palette order.
const TARGETS: [&str; 7] = [
    "State",
    "Next",
    "NextEnterable",
    "Parent",
    "Stay",
    "Succeed",
    "Fail",
];
/// Parameter literal kinds.
const LITERALS: [&str; 4] = ["Bool", "Int", "Float", "String"];

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
        ops: Vec<StOp>,
    },
    Select(Option<String>),
    Watch(u64),
    /// List the root of the include `prefix` (read from `path`) under state `parent`, before its child
    /// `index` (`None` = at the end).
    ListInclude {
        prefix: String,
        path: String,
        parent: String,
        index: Option<usize>,
    },
}

/// A trigger as the panel shows it.
#[must_use]
pub fn trigger_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Event(e) => format!("event {e}"),
        other => ron::ser::to_string(other).unwrap_or_default().to_lowercase(),
    }
}

/// A target as the panel shows it.
#[must_use]
pub fn target_text(target: &Target) -> String {
    match target {
        Target::State(s) => s.clone(),
        other => format!(
            "<{}>",
            ron::ser::to_string(other).unwrap_or_default().to_lowercase()
        ),
    }
}

/// Draws the panel.
pub fn state_tree_editor_panel(ui: &mut egui::Ui, world: &mut World) {
    if !world.contains_resource::<StEditor>() {
        world.init_resource::<StEditor>();
    }
    let (undo, redo) = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| (s.next_undo(), s.next_redo()))
        .unwrap_or_default();
    let issues = diagnose(world);
    let tasks = palette_tasks(world);
    let mut actions = Vec::new();
    world.resource_scope(|_world, mut editor: Mut<StEditor>| {
        draw(ui, &mut editor, &issues, &tasks, (undo, redo), &mut actions);
    });
    for action in actions {
        perform(world, action);
    }
}

fn may_replace_document(editor: &mut StEditor) -> bool {
    if !editor.has_unsaved_work() || editor.ui.discard_armed == Some(editor.revision) {
        return true;
    }
    editor.ui.discard_armed = Some(editor.revision);
    editor.toast = "unsaved changes: press again to discard them".into();
    false
}

fn perform(world: &mut World, action: Action) {
    if matches!(action, Action::New | Action::Open(_))
        && !may_replace_document(&mut world.resource_mut::<StEditor>())
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
            world.resource_mut::<StEditor>().new_tree(None, "Root");
            Ok(())
        }
        Action::Open(path) => {
            let path = resolve_path(world, &path);
            world.resource_mut::<StEditor>().open(&path)
        }
        Action::Save(path) => {
            let path = path.map(|p| resolve_path(world, &p));
            save(world, path.as_deref())
                .map(|_| ())
                .map_err(|e| format!("{}: {}", e.code, e.message))
        }
        Action::Edit {
            label,
            merge_key,
            ops,
        } => edit(world, &label, merge_key, &ops).map_err(|e| format!("op {}: {}", e.op_index, e.error)),
        Action::Select(name) => {
            world.resource_mut::<StEditor>().selected = name;
            Ok(())
        }
        Action::Watch(entity) => {
            world.resource_mut::<StEditor>().watch = Some(entity);
            Ok(())
        }
        Action::ListInclude {
            prefix,
            path,
            parent,
            index,
        } => list_include(world, &prefix, &path, &parent, index),
    };
    if let Err(err) = result {
        world.resource_mut::<StEditor>().toast = err;
    }
}

/// The state name of the include `prefix`'s root: `prefix/<the root of the file at path>`, read from the
/// project.
///
/// # Errors
/// The file cannot be read (outside the project, missing) or parsed.
pub fn included_root(world: &World, prefix: &str, path: &str) -> Result<String, String> {
    let root = super::project_root(world);
    let text = super::read_include(root.as_deref(), path)
        .ok_or_else(|| format!("include_missing: cannot read {path} in the project"))?;
    let inner = StateTreeDef::from_ron(&text).map_err(|e| format!("include_invalid: {path}: {e}"))?;
    Ok(format!("{prefix}/{}", inner.root))
}

/// Lists the include `prefix`'s root under `parent` before its child `index` (`None` = at the end): a
/// `reparent_state` with that `index`, one undo step.
fn list_include(
    world: &mut World,
    prefix: &str,
    path: &str,
    parent: &str,
    index: Option<usize>,
) -> Result<(), String> {
    let name = included_root(world, prefix, path)?;
    edit(
        world,
        &format!("list {name}"),
        None,
        &[StOp::ReparentState {
            name,
            parent: parent.to_string(),
            index,
        }],
    )
    .map_err(|e| format!("op {}: {}", e.op_index, e.error))
}

fn draw(
    ui: &mut egui::Ui,
    editor: &mut StEditor,
    issues: &[StIssue],
    tasks: &[String],
    (undo, redo): (Option<String>, Option<String>),
    actions: &mut Vec<Action>,
) {
    toolbar(ui, editor, issues, (undo, redo), actions);
    if !editor.is_open() {
        // Wraps in a narrow dock (bug 19, ADR-8586).
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "No state tree is open. Type a .statetree.ron path and press Open, or press New.",
                )
                .weak(),
            )
            .wrap(),
        );
        return;
    }
    let by_state = issues_by_state(issues);
    egui::ScrollArea::vertical()
        .id_salt("statetree-editor-scroll")
        .show(ui, |ui| {
            egui::CollapsingHeader::new("Node view")
                .default_open(true)
                .show(ui, |ui| {
                    node_view(ui, editor, &by_state, actions);
                    states_list(ui, editor, actions);
                });
            ui.separator();
            ui.columns(2, |cols| {
                inspector(&mut cols[0], editor, &by_state, tasks, actions);
                palette(&mut cols[1], editor, tasks, actions);
            });
            ui.separator();
            egui::CollapsingHeader::new("Tree: fields, ambient tasks, includes")
                .id_salt("statetree-tree-section")
                .default_open(true)
                .show(ui, |ui| tree_section(ui, editor, tasks, actions));
            ui.separator();
            issue_list(ui, issues, actions);
            ui.separator();
            play_view(ui, editor, actions);
        });
}

fn toolbar(
    ui: &mut egui::Ui,
    editor: &mut StEditor,
    issues: &[StIssue],
    (undo, redo): (Option<String>, Option<String>),
    actions: &mut Vec<Action>,
) {
    let errors = issues.iter().filter(|i| i.severity == StSeverity::Error).count();
    ui.horizontal(|ui| {
        if ui.button("New").clicked() {
            actions.push(Action::New);
        }
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.path_field)
                .hint_text("assets/ai/….statetree.ron")
                .desired_width(220.0),
        );
        if ui.button("Open").clicked() && !editor.ui.path_field.is_empty() {
            actions.push(Action::Open(editor.ui.path_field.clone()));
        }
        let can_save = editor.path.is_some() && errors == 0;
        if ui
            .add_enabled(can_save, egui::Button::new("Save"))
            .on_disabled_hover_text("fix the errors (a tree with errors is not saved)")
            .clicked()
        {
            actions.push(Action::Save(None));
        }
        if ui
            .add_enabled(errors == 0, egui::Button::new("Save as"))
            .clicked()
            && !editor.ui.path_field.is_empty()
        {
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
        let file = editor
            .path
            .as_ref()
            .map_or_else(|| "unsaved".to_string(), |p| p.display().to_string());
        let mark = if editor.dirty() { " *" } else { "" };
        ui.label(format!("{file}{mark}"));
        if errors == 0 {
            ui.colored_label(egui::Color32::from_rgb(90, 170, 90), "valid");
        } else {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 70), format!("{errors} error(s)"));
        }
    });
    if !editor.toast.is_empty() {
        ui.weak(editor.toast.as_str());
    }
}

fn issues_by_state(issues: &[StIssue]) -> BTreeMap<String, Vec<&StIssue>> {
    let mut out: BTreeMap<String, Vec<&StIssue>> = BTreeMap::new();
    for issue in issues {
        if let Some(state) = issue.state() {
            out.entry(state.to_string()).or_default().push(issue);
        }
    }
    out
}

/// The node view: states laid out by hierarchy (level to the right, rows down), parent links as
/// thin lines, transitions as arrows; the active path filled, the selection outlined, states with
/// errors outlined red. Clicking a state selects it.
fn node_view(
    ui: &mut egui::Ui,
    editor: &StEditor,
    by_state: &BTreeMap<String, Vec<&StIssue>>,
    actions: &mut Vec<Action>,
) {
    let def = &editor.doc;
    let rows = state_tree_edit::hierarchy(def);
    let depth = rows.iter().map(|(_, d)| *d).max().unwrap_or(0);
    let size = egui::vec2(
        LEVEL_DX * (depth as f32 + 1.0) + 40.0,
        ROW_DY * rows.len().max(1) as f32 + 12.0,
    );
    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let origin = response.rect.min + egui::vec2(8.0, 6.0);
    // The States list's keyboard focus draws the focus ring (FU-5).
    let focused = editor
        .ui
        .keys
        .focused()
        .and_then(|row| rows.get(row).map(|(index, _)| def.states[*index].name.clone()));
    let mut rects: BTreeMap<&str, egui::Rect> = BTreeMap::new();
    for (row, (index, d)) in rows.iter().enumerate() {
        let min = origin + egui::vec2(*d as f32 * LEVEL_DX, row as f32 * ROW_DY);
        rects.insert(
            def.states[*index].name.as_str(),
            egui::Rect::from_min_size(min, egui::vec2(NODE_W, NODE_H)),
        );
    }
    let active: BTreeSet<&str> = editor
        .live
        .as_ref()
        .map(|l| l.active_path.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let link = egui::Stroke::new(1.0, egui::Color32::from_gray(110));
    for s in &def.states {
        let Some(from) = rects.get(s.name.as_str()) else {
            continue;
        };
        for c in &s.children {
            if let Some(to) = rects.get(c.as_str()) {
                let a = egui::pos2(from.min.x + 10.0, from.max.y);
                let b = egui::pos2(a.x, to.center().y);
                painter.line_segment([a, b], link);
                painter.line_segment([b, egui::pos2(to.min.x, b.y)], link);
            }
        }
    }
    let arrow = egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 140, 220));
    for s in &def.states {
        let Some(from) = rects.get(s.name.as_str()) else {
            continue;
        };
        for (k, t) in s.transitions.iter().enumerate() {
            if let Target::State(target) = &t.target
                && let Some(to) = rects.get(target.as_str())
            {
                let lane = 6.0 + 5.0 * k as f32;
                let a = egui::pos2(from.max.x, from.center().y);
                let b = egui::pos2(to.max.x, to.center().y);
                let x = a.x.max(b.x) + lane;
                painter.line_segment([a, egui::pos2(x, a.y)], arrow);
                painter.line_segment([egui::pos2(x, a.y), egui::pos2(x, b.y)], arrow);
                painter.arrow(egui::pos2(x, b.y), egui::vec2(b.x - x, 0.0), arrow);
            }
        }
    }
    for (name, rect) in &rects {
        let selected = editor.selected.as_deref() == Some(*name);
        let has_error = by_state
            .get(*name)
            .is_some_and(|list| list.iter().any(|i| i.severity == StSeverity::Error));
        let fill = if active.contains(name) {
            egui::Color32::from_rgb(52, 110, 60)
        } else {
            egui::Color32::from_gray(48)
        };
        painter.rect_filled(*rect, 4.0, fill);
        let stroke = if has_error {
            egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 80, 70))
        } else if selected {
            egui::Stroke::new(2.0, egui::Color32::from_rgb(230, 200, 90))
        } else {
            egui::Stroke::new(1.0, egui::Color32::from_gray(120))
        };
        painter.rect_stroke(*rect, 4.0, stroke, egui::StrokeKind::Inside);
        let label = if *name == def.root.as_str() {
            format!("{name} (root)")
        } else {
            (*name).to_string()
        };
        painter.text(
            rect.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            egui::Color32::from_gray(230),
        );
        let id = ui.id().with(("statetree-node", *name));
        if ui.interact(*rect, id, egui::Sense::click()).clicked() {
            actions.push(Action::Select(Some((*name).to_string())));
        }
        if focused.as_deref() == Some(*name) {
            crate::canvas_keys::focus_ring(&painter, *rect);
        }
    }
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        &format!("State tree canvas, {} states", def.states.len()),
    );
}

/// The States list under the node view: the keyboard route of the canvas (FU-5, the convention
/// of `canvas_keys`). Up / Down move the focus; Enter selects the state (the canvas click);
/// Alt+Up / Alt+Down move it among its siblings, Alt+Left out to its grandparent, Alt+Right under
/// its previous sibling (the inspector's **Move under** op, `reparent_state`); Delete is
/// **Remove state**; Ctrl+Shift+A selects it and focuses the palette's new-state field (**Add
/// under**).
fn states_list(ui: &mut egui::Ui, editor: &mut StEditor, actions: &mut Vec<Action>) {
    use crate::canvas_keys::{RowKey, list_rows};
    let def = &editor.doc;
    let rows = state_tree_edit::hierarchy(def);
    let names: Vec<String> = rows.iter().map(|(i, _)| def.states[*i].name.clone()).collect();
    let anchored = editor
        .ui
        .keys_state
        .as_ref()
        .and_then(|n| names.iter().position(|m| m == n));
    if anchored.is_some() {
        editor.ui.keys.retarget(anchored);
    }
    let selected = editor
        .selected
        .as_ref()
        .and_then(|s| names.iter().position(|n| n == s));
    let label = |i: usize| {
        let Some((index, depth)) = rows.get(i) else {
            return String::new();
        };
        let state = &def.states[*index];
        match parent_of(def, &state.name) {
            Some(parent) => format!("{}, under {parent}, level {depth}", state.name),
            None => format!("{} (root)", state.name),
        }
    };
    ui.label(format!("States ({})", names.len()));
    let events = list_rows(
        ui,
        "statetree-states",
        &mut editor.ui.keys,
        names.len(),
        crate::canvas_keys::LIST_HEIGHT / 2.0,
        selected,
        &label,
    );
    editor.ui.keys_state = editor.ui.keys.current.and_then(|i| names.get(i).cloned());
    for event in events {
        match event {
            RowKey::Activate(row) => {
                if let Some(name) = names.get(row) {
                    actions.push(Action::Select(Some(name.clone())));
                }
            }
            RowKey::Delete(row) => {
                let Some(name) = names.get(row) else { continue };
                actions.push(Action::Edit {
                    label: format!("remove {name}"),
                    merge_key: None,
                    ops: vec![StOp::RemoveState {
                        name: name.clone(),
                        subtree: false,
                    }],
                });
            }
            RowKey::Palette(row) => {
                if let Some(name) = names.get(row) {
                    actions.push(Action::Select(Some(name.clone())));
                    editor.ui.focus_new_state = true;
                }
            }
            RowKey::Arrow {
                row,
                dx,
                dy,
                alt: true,
                ..
            } => {
                let Some(name) = names.get(row) else { continue };
                if let Some(op) = keyboard_move(def, name, dx, dy) {
                    actions.push(Action::Edit {
                        label: format!("move {name}"),
                        merge_key: None,
                        ops: vec![op],
                    });
                }
            }
            RowKey::Arrow { .. } | RowKey::Connect(_) => {}
        }
    }
}

/// The parent of state `name`, if any.
fn parent_of<'a>(def: &'a StateTreeDef, name: &str) -> Option<&'a str> {
    def.states
        .iter()
        .find(|s| s.children.iter().any(|c| c == name))
        .map(|s| s.name.as_str())
}

/// The `reparent_state` an Alt+arrow asks for: up / down one place among the siblings
/// (`dy`), out to the grandparent after the parent (`dx` -1) or in under the previous sibling, at
/// its end (`dx` 1). `None` where the move has nowhere to go (the root, the first sibling up).
#[must_use]
pub fn keyboard_move(def: &StateTreeDef, name: &str, dx: i32, dy: i32) -> Option<StOp> {
    let parent = parent_of(def, name)?;
    let siblings = &def.states.iter().find(|s| s.name == parent)?.children;
    let at = siblings.iter().position(|c| c == name)?;
    let (parent, index) = match (dx, dy) {
        (0, -1) if at > 0 => (parent.to_string(), Some(at - 1)),
        (0, 1) if at + 1 < siblings.len() => (parent.to_string(), Some(at + 1)),
        (-1, 0) => {
            let grand = parent_of(def, parent)?;
            let place = def
                .states
                .iter()
                .find(|s| s.name == grand)?
                .children
                .iter()
                .position(|c| c == parent)?;
            (grand.to_string(), Some(place + 1))
        }
        (1, 0) if at > 0 => (siblings[at - 1].clone(), None),
        _ => return None,
    };
    Some(StOp::ReparentState {
        name: name.to_string(),
        parent,
        index,
    })
}

fn literal_text(literal: &Literal) -> String {
    ron::ser::to_string(literal).unwrap_or_default()
}

fn parse_literal(kind: usize, text: &str) -> Option<Literal> {
    match kind {
        0 => text.trim().parse().ok().map(Literal::Bool),
        1 => text.trim().parse().ok().map(Literal::Int),
        2 => text
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Literal::Float),
        _ => Some(Literal::String(text.to_string())),
    }
}

fn set_state(name: &str) -> StOp {
    StOp::SetState {
        name: name.to_string(),
        selection: None,
        enter: None,
        weight: None,
        utility: None,
        clear_utility: false,
        fallback: None,
    }
}

fn inspector(
    ui: &mut egui::Ui,
    editor: &mut StEditor,
    by_state: &BTreeMap<String, Vec<&StIssue>>,
    tasks: &[String],
    actions: &mut Vec<Action>,
) {
    ui.strong("Inspector");
    let Some(index) = editor.selected_index() else {
        ui.weak("Select a state in the node view.");
        return;
    };
    let state = editor.doc.states[index].clone();
    let name = state.name.clone();
    for issue in by_state.get(&name).into_iter().flatten() {
        let color = if issue.severity == StSeverity::Error {
            egui::Color32::from_rgb(220, 80, 70)
        } else {
            egui::Color32::from_rgb(220, 180, 70)
        };
        ui.colored_label(color, format!("{}: {}", issue.path, issue.message))
            .on_hover_text(format!("Agent error code: {}", issue.code));
    }
    ui.horizontal(|ui| {
        ui.label("Name");
        if editor.ui.rename.as_ref().is_none_or(|(who, _)| *who != name) {
            editor.ui.rename = Some((name.clone(), name.clone()));
        }
        let to = match editor.ui.rename.as_mut() {
            Some((_, text)) => {
                ui.add(egui::TextEdit::singleline(text).desired_width(140.0));
                text.clone()
            }
            None => name.clone(),
        };
        if ui.button("Rename").clicked() && to != name {
            actions.push(Action::Edit {
                label: format!("rename {name}"),
                merge_key: None,
                ops: vec![StOp::RenameState {
                    name: name.clone(),
                    to,
                }],
            });
        }
    });
    state_fields(ui, &editor.doc, &state, actions);
    utility_row(ui, editor, &state, actions);
    tasks_section(ui, editor, &state, tasks, actions);
    params_section(ui, editor, &state, actions);
    transitions_section(ui, &state, actions);
    conditions_section(ui, editor, &state, actions);
    ui.horizontal(|ui| {
        if ui
            .button("Remove state")
            .on_hover_text("its children take its place")
            .clicked()
        {
            actions.push(Action::Edit {
                label: format!("remove {name}"),
                merge_key: None,
                ops: vec![StOp::RemoveState {
                    name: name.clone(),
                    subtree: false,
                }],
            });
        }
        if ui.button("Remove with children").clicked() {
            actions.push(Action::Edit {
                label: format!("remove {name} and below"),
                merge_key: None,
                ops: vec![StOp::RemoveState {
                    name: name.clone(),
                    subtree: true,
                }],
            });
        }
    });
}

fn state_fields(ui: &mut egui::Ui, def: &StateTreeDef, state: &StateDef, actions: &mut Vec<Action>) {
    let name = &state.name;
    let parents = state_tree_edit::parents(def);
    let parent = parents.get(name).cloned().flatten();
    ui.horizontal(|ui| {
        ui.label("Parent");
        let shown = parent.clone().unwrap_or_else(|| "(none)".into());
        egui::ComboBox::from_id_salt(("statetree-parent", name))
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for s in &def.states {
                    if s.name == *name || parent.as_deref() == Some(s.name.as_str()) {
                        continue;
                    }
                    if ui.selectable_label(false, s.name.as_str()).clicked() {
                        actions.push(Action::Edit {
                            label: format!("move {name}"),
                            merge_key: None,
                            ops: vec![StOp::ReparentState {
                                name: name.clone(),
                                parent: s.name.clone(),
                                index: None,
                            }],
                        });
                    }
                }
            });
        if parent.is_none() && def.root != *name && ui.button("Make root").clicked() {
            actions.push(Action::Edit {
                label: format!("root {name}"),
                merge_key: None,
                ops: vec![StOp::SetRoot { name: name.clone() }],
            });
        }
    });
    ui.horizontal(|ui| {
        ui.label("Selection");
        for (label, value) in [
            ("First", Selection::First),
            ("Utility", Selection::Utility),
            ("Weighted", Selection::Weighted),
        ] {
            if ui.selectable_label(state.selection == value, label).clicked() && state.selection != value {
                let mut op = set_state(name);
                if let StOp::SetState { selection, .. } = &mut op {
                    *selection = Some(value);
                }
                actions.push(Action::Edit {
                    label: format!("{name} selection"),
                    merge_key: None,
                    ops: vec![op],
                });
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Weight");
        let mut weight = state.weight;
        if ui.add(egui::DragValue::new(&mut weight).speed(0.05)).changed() && weight.is_finite() {
            let mut op = set_state(name);
            if let StOp::SetState { weight: w, .. } = &mut op {
                *w = Some(weight);
            }
            actions.push(Action::Edit {
                label: format!("{name} weight"),
                merge_key: Some(format!("weight:{name}")),
                ops: vec![op],
            });
        }
        ui.label("Fallback");
        for (label, value) in [
            ("Stay", Fallback::Stay),
            ("NextEnterable", Fallback::NextEnterable),
        ] {
            if ui.selectable_label(state.fallback == value, label).clicked() && state.fallback != value {
                let mut op = set_state(name);
                if let StOp::SetState { fallback, .. } = &mut op {
                    *fallback = Some(value);
                }
                actions.push(Action::Edit {
                    label: format!("{name} fallback"),
                    merge_key: None,
                    ops: vec![op],
                });
            }
        }
    });
}

fn tasks_section(
    ui: &mut egui::Ui,
    editor: &mut StEditor,
    state: &StateDef,
    tasks: &[String],
    actions: &mut Vec<Action>,
) {
    let name = &state.name;
    ui.label("Tasks (run while active: enter, tick, exit)");
    for (k, task) in state.tasks.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.monospace(task.as_str());
            if ui.small_button("x").clicked() {
                let mut list = state.tasks.clone();
                list.remove(k);
                actions.push(Action::Edit {
                    label: format!("{name} tasks"),
                    merge_key: None,
                    ops: vec![StOp::SetTasks {
                        name: name.clone(),
                        tasks: list,
                    }],
                });
            }
        });
    }
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(("statetree-task", name))
            .selected_text(if editor.ui.task_pick.is_empty() {
                "task…"
            } else {
                editor.ui.task_pick.as_str()
            })
            .show_ui(ui, |ui| {
                if tasks.is_empty() {
                    ui.weak("no task is registered (load the game)");
                }
                for t in tasks {
                    ui.selectable_value(&mut editor.ui.task_pick, t.clone(), t.as_str());
                }
            });
        if ui
            .add_enabled(!editor.ui.task_pick.is_empty(), egui::Button::new("Add task"))
            .clicked()
        {
            let mut list = state.tasks.clone();
            list.push(editor.ui.task_pick.clone());
            actions.push(Action::Edit {
                label: format!("{name} tasks"),
                merge_key: None,
                ops: vec![StOp::SetTasks {
                    name: name.clone(),
                    tasks: list,
                }],
            });
        }
    });
}

fn params_section(ui: &mut egui::Ui, editor: &mut StEditor, state: &StateDef, actions: &mut Vec<Action>) {
    let name = &state.name;
    ui.label("Parameters (blackboard values written on entry)");
    for (key, literal) in &state.enter_set {
        ui.horizontal(|ui| {
            ui.monospace(format!("{key} = {}", literal_text(literal)));
            if ui.small_button("x").clicked() {
                actions.push(Action::Edit {
                    label: format!("{name} parameter {key}"),
                    merge_key: None,
                    ops: vec![StOp::SetParam {
                        state: name.clone(),
                        key: key.clone(),
                        value: None,
                    }],
                });
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.param_key)
                .hint_text("key")
                .desired_width(80.0),
        );
        egui::ComboBox::from_id_salt(("statetree-literal", name))
            .selected_text(LITERALS[editor.ui.param_kind.min(3)])
            .show_ui(ui, |ui| {
                for (i, l) in LITERALS.iter().enumerate() {
                    ui.selectable_value(&mut editor.ui.param_kind, i, *l);
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.param_value)
                .hint_text("value")
                .desired_width(80.0),
        );
        let value = parse_literal(editor.ui.param_kind, &editor.ui.param_value);
        if ui
            .add_enabled(
                value.is_some() && !editor.ui.param_key.is_empty(),
                egui::Button::new("Set"),
            )
            .clicked()
        {
            actions.push(Action::Edit {
                label: format!("{name} parameter {}", editor.ui.param_key),
                merge_key: None,
                ops: vec![StOp::SetParam {
                    state: name.clone(),
                    key: editor.ui.param_key.clone(),
                    value,
                }],
            });
        }
    });
}

fn transitions_section(ui: &mut egui::Ui, state: &StateDef, actions: &mut Vec<Action>) {
    let name = &state.name;
    ui.label("Transitions (higher priority first, then declaration order)");
    let count = state.transitions.len();
    for (k, t) in state.transitions.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.monospace(format!(
                "{} -> {}",
                trigger_text(&t.trigger),
                target_text(&t.target)
            ));
            let mut priority = t.priority;
            ui.label("prio");
            if ui.add(egui::DragValue::new(&mut priority)).changed() {
                actions.push(Action::Edit {
                    label: format!("{name} transition {k}"),
                    merge_key: Some(format!("prio:{name}:{k}")),
                    ops: vec![StOp::SetTransition {
                        state: name.clone(),
                        index: k,
                        trigger: None,
                        condition: None,
                        target: None,
                        priority: Some(priority),
                        hold: None,
                    }],
                });
            }
            let mut hold = t.hold;
            ui.label("hold");
            if !matches!(t.trigger, Trigger::Event(_))
                && ui
                    .add(egui::DragValue::new(&mut hold).speed(0.05).range(0.0..=600.0))
                    .changed()
            {
                actions.push(Action::Edit {
                    label: format!("{name} transition {k}"),
                    merge_key: Some(format!("hold:{name}:{k}")),
                    ops: vec![StOp::SetTransition {
                        state: name.clone(),
                        index: k,
                        trigger: None,
                        condition: None,
                        target: None,
                        priority: None,
                        hold: Some(hold),
                    }],
                });
            }
            if k > 0 && ui.small_button("up").clicked() {
                actions.push(Action::Edit {
                    label: format!("{name} transition order"),
                    merge_key: None,
                    ops: vec![StOp::MoveTransition {
                        state: name.clone(),
                        from: k,
                        to: k - 1,
                    }],
                });
            }
            if k + 1 < count && ui.small_button("down").clicked() {
                actions.push(Action::Edit {
                    label: format!("{name} transition order"),
                    merge_key: None,
                    ops: vec![StOp::MoveTransition {
                        state: name.clone(),
                        from: k,
                        to: k + 1,
                    }],
                });
            }
            if ui.small_button("x").clicked() {
                actions.push(Action::Edit {
                    label: format!("{name} remove transition"),
                    merge_key: None,
                    ops: vec![StOp::RemoveTransition {
                        state: name.clone(),
                        index: k,
                    }],
                });
            }
        });
    }
}

/// The palette: a new child of the selected state, a new transition from it, and the registered
/// tasks for reference.
fn palette(ui: &mut egui::Ui, editor: &mut StEditor, tasks: &[String], actions: &mut Vec<Action>) {
    ui.strong("Palette");
    let selected = editor.selected.clone();
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut editor.ui.new_state)
                .hint_text("new state")
                .desired_width(110.0),
        );
        if std::mem::take(&mut editor.ui.focus_new_state) {
            field.request_focus();
        }
        let parent = selected
            .clone()
            .or_else(|| (!editor.doc.root.is_empty()).then(|| editor.doc.root.clone()));
        let label = match &parent {
            Some(p) => format!("Add under {p}"),
            None => "Add root".to_string(),
        };
        if ui
            .add_enabled(!editor.ui.new_state.is_empty(), egui::Button::new(label))
            .clicked()
        {
            actions.push(Action::Edit {
                label: format!("add {}", editor.ui.new_state),
                merge_key: None,
                ops: vec![StOp::AddState {
                    name: editor.ui.new_state.clone(),
                    parent: if editor.doc.states.is_empty() {
                        None
                    } else {
                        parent
                    },
                    index: None,
                }],
            });
            editor.ui.new_state.clear();
        }
    });
    if let Some(from) = selected {
        ui.label(format!("New transition from {from}"));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("statetree-trigger")
                .selected_text(TRIGGERS[editor.ui.trigger_kind.min(4)])
                .show_ui(ui, |ui| {
                    for (i, t) in TRIGGERS.iter().enumerate() {
                        ui.selectable_value(&mut editor.ui.trigger_kind, i, *t);
                    }
                });
            if editor.ui.trigger_kind == 1 {
                ui.add(
                    egui::TextEdit::singleline(&mut editor.ui.event_name)
                        .hint_text("event")
                        .desired_width(70.0),
                );
            }
            egui::ComboBox::from_id_salt("statetree-target-kind")
                .selected_text(TARGETS[editor.ui.target_kind.min(6)])
                .show_ui(ui, |ui| {
                    for (i, t) in TARGETS.iter().enumerate() {
                        ui.selectable_value(&mut editor.ui.target_kind, i, *t);
                    }
                });
            if editor.ui.target_kind == 0 {
                egui::ComboBox::from_id_salt("statetree-target-state")
                    .selected_text(if editor.ui.target_state.is_empty() {
                        "state…"
                    } else {
                        editor.ui.target_state.as_str()
                    })
                    .show_ui(ui, |ui| {
                        for s in &editor.doc.states {
                            ui.selectable_value(&mut editor.ui.target_state, s.name.clone(), s.name.as_str());
                        }
                    });
            }
        });
        let trigger = match editor.ui.trigger_kind {
            0 => Some(Trigger::Tick),
            1 => (!editor.ui.event_name.is_empty()).then(|| Trigger::Event(editor.ui.event_name.clone())),
            2 => Some(Trigger::Succeeded),
            3 => Some(Trigger::Failed),
            _ => Some(Trigger::Completed),
        };
        let target = match editor.ui.target_kind {
            0 => (!editor.ui.target_state.is_empty()).then(|| Target::State(editor.ui.target_state.clone())),
            1 => Some(Target::Next),
            2 => Some(Target::NextEnterable),
            3 => Some(Target::Parent),
            4 => Some(Target::Stay),
            5 => Some(Target::Succeed),
            _ => Some(Target::Fail),
        };
        if ui
            .add_enabled(
                trigger.is_some() && target.is_some(),
                egui::Button::new("Add transition"),
            )
            .clicked()
            && let (Some(trigger), Some(target)) = (trigger, target)
        {
            actions.push(Action::Edit {
                label: format!("{from} add transition"),
                merge_key: None,
                ops: vec![StOp::AddTransition {
                    state: from.clone(),
                    trigger,
                    condition: None,
                    target,
                    priority: None,
                    hold: None,
                    index: None,
                }],
            });
        }
    }
    ui.label("Registered tasks");
    if tasks.is_empty() {
        ui.weak("none (tasks are checked once the game registers them)");
    }
    for t in tasks {
        ui.monospace(t.as_str());
    }
}

fn issue_list(ui: &mut egui::Ui, issues: &[StIssue], actions: &mut Vec<Action>) {
    ui.strong(format!("Problems ({})", issues.len()));
    if issues.is_empty() {
        ui.weak("none: the tree compiles");
    }
    for issue in issues {
        let color = if issue.severity == StSeverity::Error {
            egui::Color32::from_rgb(220, 80, 70)
        } else {
            egui::Color32::from_rgb(220, 180, 70)
        };
        let text = format!("{}  {}  {}", issue.code, issue.path, issue.message);
        if ui
            .add(egui::Label::new(egui::RichText::new(text).color(color)).sense(egui::Sense::click()))
            .clicked()
            && let Some(state) = issue.state()
        {
            actions.push(Action::Select(Some(state.to_string())));
        }
    }
}

fn play_view(ui: &mut egui::Ui, editor: &StEditor, actions: &mut Vec<Action>) {
    ui.strong("Play");
    if editor.agents.is_empty() {
        ui.weak("No agent runs a state tree here. Press Play: agents whose StateTreeSource names this file reload it on save.");
    } else {
        ui.horizontal(|ui| {
            ui.label("Agent");
            let shown = editor
                .live
                .as_ref()
                .map_or_else(|| "none".to_string(), |l| format!("{}", l.entity));
            egui::ComboBox::from_id_salt("statetree-watch")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    for bits in &editor.agents {
                        if ui.selectable_label(false, bits.to_string()).clicked() {
                            actions.push(Action::Watch(*bits));
                        }
                    }
                });
        });
    }
    if let Some(live) = &editor.live {
        ui.label(format!(
            "{} after {} ticks: {}",
            live.status,
            live.ticks,
            live.active_path.join(" / ")
        ));
        if !live.uses_file {
            ui.weak("this agent's StateTreeSource does not name the open file");
        } else if !live.matches_document {
            ui.weak("the running tree differs from the document (save to apply)");
        }
    }
    if let Some(reload) = &editor.reload {
        let outcome = match reload.last {
            Some(last) => format!(
                "save {}: {last} (kept {}, restarted {}, rejected {})",
                reload.save, reload.kept, reload.restarted, reload.rejected
            ),
            None if reload.delivered => format!("save {}: waiting for the next fixed step", reload.save),
            None => format!("save {}: no play session to reload", reload.save),
        };
        ui.label(outcome);
        if let Some(reason) = &reload.reason {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 70), reason.as_str());
        }
    }
}

/// The selected state's utility score (`bias + scale * value(key)`, read under a `Utility` parent).
fn utility_row(ui: &mut egui::Ui, editor: &mut StEditor, state: &StateDef, actions: &mut Vec<Action>) {
    let name = &state.name;
    let at = (editor.revision, name.clone());
    if editor.ui.utility_at.as_ref() != Some(&at) {
        editor.ui.utility = Some(state.utility.clone().unwrap_or(UtilityDef {
            key: String::new(),
            scale: 1.0,
            bias: 0.0,
        }));
        editor.ui.utility_at = Some(at);
    }
    let Some(form) = editor.ui.utility.as_mut() else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label("Utility");
        ui.add(
            egui::TextEdit::singleline(&mut form.key)
                .hint_text("blackboard key")
                .desired_width(90.0),
        );
        ui.add(egui::DragValue::new(&mut form.scale).speed(0.05).prefix("× "));
        ui.add(egui::DragValue::new(&mut form.bias).speed(0.05).prefix("+ "));
        let valid = !form.key.is_empty() && form.scale.is_finite() && form.bias.is_finite();
        if ui
            .add_enabled(
                valid && state.utility.as_ref() != Some(&*form),
                egui::Button::new("Set utility"),
            )
            .clicked()
        {
            actions.push(Action::Edit {
                label: format!("{name} utility"),
                merge_key: None,
                ops: vec![utility_op(name, Some(form.clone()))],
            });
        }
        if state.utility.is_some() && ui.button("Clear utility").clicked() {
            actions.push(Action::Edit {
                label: format!("{name} utility"),
                merge_key: None,
                ops: vec![utility_op(name, None)],
            });
        }
    });
}

/// The tree-level fields, the ambient tasks and the includes.
fn tree_section(ui: &mut egui::Ui, editor: &mut StEditor, tasks: &[String], actions: &mut Vec<Action>) {
    if editor.ui.tree_at != Some(editor.revision) {
        editor.ui.tree = TreeForm::of(&editor.doc);
        editor.ui.include_edits.clear();
        editor.ui.tree_at = Some(editor.revision);
    }
    ui.horizontal(|ui| {
        ui.label("Description");
        ui.add(egui::TextEdit::singleline(&mut editor.ui.tree.description).desired_width(220.0));
    });
    ui.horizontal(|ui| {
        ui.label("Seed");
        ui.add(egui::DragValue::new(&mut editor.ui.tree.seed));
        ui.label("State key");
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.tree.state_key)
                .hint_text("(none)")
                .desired_width(110.0),
        );
        let op = editor.ui.tree.op(&editor.doc);
        if ui
            .add_enabled(op.is_some(), egui::Button::new("Apply tree fields"))
            .clicked()
            && let Some(op) = op
        {
            actions.push(Action::Edit {
                label: "tree fields".into(),
                merge_key: None,
                ops: vec![op],
            });
        }
    });
    ui.label("Ambient tasks (ticked first on every tick)");
    for (k, task) in editor.doc.ambient.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.monospace(task.as_str());
            if ui.small_button("Remove ambient").clicked() {
                actions.push(Action::Edit {
                    label: "ambient tasks".into(),
                    merge_key: None,
                    ops: vec![ambient_remove_op(&editor.doc, k)],
                });
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.ambient_pick)
                .hint_text("task path")
                .desired_width(140.0),
        );
        egui::ComboBox::from_id_salt("statetree-ambient-pick")
            .selected_text("registered…")
            .show_ui(ui, |ui| {
                if tasks.is_empty() {
                    ui.weak("no task is registered (load the game)");
                }
                for t in tasks {
                    ui.selectable_value(&mut editor.ui.ambient_pick, t.clone(), t.as_str());
                }
            });
        let pick = editor.ui.ambient_pick.trim().to_string();
        if ui
            .add_enabled(!pick.is_empty(), egui::Button::new("Add ambient task"))
            .clicked()
        {
            actions.push(Action::Edit {
                label: "ambient tasks".into(),
                merge_key: None,
                ops: vec![ambient_add_op(&editor.doc, &pick)],
            });
            editor.ui.ambient_pick.clear();
        }
    });
    ui.label("Includes (states named prefix/<name>; list the included root under a state to reach it)");
    for include in editor.doc.includes.clone() {
        let (to_prefix, to_path) = editor
            .ui
            .include_edits
            .entry(include.prefix.clone())
            .or_insert_with(|| (include.prefix.clone(), include.path.clone()));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(to_prefix).desired_width(80.0));
            ui.add(egui::TextEdit::singleline(to_path).desired_width(200.0));
            let op = include_set_op(&include.prefix, &include.path, to_prefix.trim(), to_path.trim());
            if ui
                .add_enabled(op.is_some(), egui::Button::new("Change include"))
                .clicked()
                && let Some(op) = op
            {
                actions.push(Action::Edit {
                    label: format!("include {}", include.prefix),
                    merge_key: None,
                    ops: vec![op],
                });
            }
            let listed = editor.doc.states.iter().find_map(|s| {
                s.children
                    .iter()
                    .any(|c| c.starts_with(&format!("{}/", include.prefix)))
                    .then(|| s.name.clone())
            });
            match (&listed, &editor.selected) {
                (Some(parent), _) => {
                    ui.weak(format!("listed under {parent}"));
                }
                (None, Some(parent)) => {
                    // Where among the parent's children the root goes (`reparent_state`'s `index`).
                    let children = editor
                        .doc
                        .states
                        .iter()
                        .find(|s| &s.name == parent)
                        .map(|s| s.children.clone())
                        .unwrap_or_default();
                    let at = editor
                        .ui
                        .include_list_at
                        .entry(include.prefix.clone())
                        .or_default();
                    if at.is_some_and(|i| i >= children.len()) {
                        *at = None;
                    }
                    let place = |at: Option<usize>| {
                        at.and_then(|i| children.get(i))
                            .map_or_else(|| "at the end".to_owned(), |c| format!("before {c}"))
                    };
                    egui::ComboBox::from_id_salt(("st-include-list-at", &include.prefix))
                        .selected_text(place(*at))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(at, None, place(None));
                            for i in 0..children.len() {
                                ui.selectable_value(at, Some(i), place(Some(i)));
                            }
                        });
                    let index = *at;
                    if ui.button(format!("List under {parent}")).clicked() {
                        actions.push(Action::ListInclude {
                            prefix: include.prefix.clone(),
                            path: include.path.clone(),
                            parent: parent.clone(),
                            index,
                        });
                    }
                }
                (None, None) => {
                    ui.weak("not listed: select a state to list its root under");
                }
            }
            if ui.small_button("Remove include").clicked() {
                actions.push(Action::Edit {
                    label: format!("remove include {}", include.prefix),
                    merge_key: None,
                    ops: vec![StOp::RemoveInclude {
                        prefix: include.prefix.clone(),
                    }],
                });
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.include_prefix)
                .hint_text("prefix")
                .desired_width(80.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut editor.ui.include_path)
                .hint_text("assets/ai/….statetree.ron")
                .desired_width(200.0),
        );
        let ready = !editor.ui.include_prefix.trim().is_empty() && !editor.ui.include_path.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new("Add include")).clicked() {
            actions.push(Action::Edit {
                label: format!("include {}", editor.ui.include_prefix.trim()),
                merge_key: None,
                ops: vec![StOp::AddInclude {
                    prefix: editor.ui.include_prefix.trim().to_string(),
                    path: editor.ui.include_path.trim().to_string(),
                    index: None,
                }],
            });
            editor.ui.include_prefix.clear();
            editor.ui.include_path.clear();
        }
    });
}

/// Parses a condition typed as RON (`Always`, `Key("alert")`, `Compare(key: "dist", op: Lt, value: 12.0)`,
/// `All([...])`); `None` when it does not parse or holds a non-finite number.
#[must_use]
pub fn parse_condition(text: &str) -> Option<Condition> {
    let condition: Condition = ron::from_str(text.trim()).ok()?;
    let mut probe = StateTreeDef::new("R");
    let mut state = StateDef::new("R");
    state.enter = condition.clone();
    probe.states.push(state);
    state_tree_edit::to_canonical_ron(&probe).ok().map(|_| condition)
}

/// One condition as editable RON text with an Apply button. Returns the parsed condition when
/// applied and changed.
fn condition_row(
    ui: &mut egui::Ui,
    buffers: &mut BTreeMap<String, String>,
    key: &str,
    label: &str,
    current: &Condition,
) -> Option<Condition> {
    let text = buffers
        .entry(key.to_string())
        .or_insert_with(|| ron::ser::to_string(current).unwrap_or_default());
    let mut applied = None;
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(220.0)
                .code_editor(),
        );
        let parsed = parse_condition(text);
        let response = ui
            .add_enabled(parsed.is_some(), egui::Button::new("Apply"))
            .on_disabled_hover_text(
                "not a condition: e.g. Key(\"alert\"), Compare(key: \"dist\", op: Lt, value: 12.0)",
            );
        if response.clicked()
            && let Some(condition) = parsed
            && condition != *current
        {
            applied = Some(condition);
        }
    });
    applied
}

/// The selected state's enter condition and transition guards, as RON text.
fn conditions_section(ui: &mut egui::Ui, editor: &mut StEditor, state: &StateDef, actions: &mut Vec<Action>) {
    let name = &state.name;
    let at = (editor.revision, name.clone());
    if editor.ui.conditions_at.as_ref() != Some(&at) {
        editor.ui.conditions.clear();
        editor.ui.conditions_at = Some(at);
    }
    ui.label("Conditions (RON)");
    let buffers = &mut editor.ui.conditions;
    if let Some(enter) = condition_row(ui, buffers, "enter", "enter", &state.enter) {
        let mut op = set_state(name);
        if let StOp::SetState { enter: e, .. } = &mut op {
            *e = Some(enter);
        }
        actions.push(Action::Edit {
            label: format!("{name} enter condition"),
            merge_key: None,
            ops: vec![op],
        });
    }
    for (k, t) in state.transitions.iter().enumerate() {
        let label = format!("{} -> {}", trigger_text(&t.trigger), target_text(&t.target));
        if let Some(condition) = condition_row(ui, buffers, &format!("t{k}"), &label, &t.condition) {
            actions.push(Action::Edit {
                label: format!("{name} transition {k} condition"),
                merge_key: None,
                ops: vec![StOp::SetTransition {
                    state: name.clone(),
                    index: k,
                    trigger: None,
                    condition: Some(condition),
                    target: None,
                    priority: None,
                    hold: None,
                }],
            });
        }
    }
}
