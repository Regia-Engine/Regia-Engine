// SPDX-License-Identifier: MPL-2.0
//! The State Trees panel (`book/agents/editor-gameplay-panels.md`, ADR-2030): live debugging of every
//! [`StateTree`] in the world the editor is showing. It lists the trees, and for the selected one
//! shows the state hierarchy with the active path, each state's transitions with the ones holding
//! and the ones last taken, the blackboard and the recent trace, with pause, resume, restart, send
//! event, reload from a file and the development builds' skip and pause-on-enter switches.
//!
//! The state is in [`StateTreePanel`], which the panel and `editor.state_tree_panel_*` share.

use std::collections::BTreeMap;
use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::state_tree::{
    CompareOp, Condition, StateDef, StateTree, StateTreeDef, StateTreeRunStatus, Target, TraceKind,
    TransitionDef, Trigger,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::gameplay_panel::{
    CallRecord, LiveSource, PanelCall, entity_param, header, play_running, reason_error,
};
use crate::pie::PieSession;
use crate::viewport_agent::Handler;

/// Trees listed at most; `total` still counts them all.
const MAX_TREES: usize = 512;

/// The largest number of trace lines a snapshot keeps (a tree records 64).
const MAX_TRACE: usize = regia_ai::state_tree::MAX_TRACE_ENTRIES;

/// The panel's own call that queues an event.
const SEND_EVENT: &str = "statetree.send_event";

/// One tree in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    /// Its entity.
    pub entity: Entity,
    /// Its display name.
    pub name: String,
    /// Where it is in its life: `not_started`, `running`, `succeeded`, `failed`.
    pub status: &'static str,
    /// Paused by `pause` or a pause-on-enter switch.
    pub paused: bool,
    /// Active states, root first.
    pub active_path: Vec<String>,
    /// Ticks so far.
    pub ticks: u64,
}

/// One transition of a state.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionRow {
    /// `tick`, `event:<name>`, `succeeded`, `failed` or `completed`.
    pub trigger: String,
    /// The target's text: a state name, `<next>`, `<next-enterable>`, `<parent>`, `<stay>`,
    /// `<succeed>` or `<fail>`.
    pub target: String,
    /// The guard as text; empty when it is always true.
    pub condition: String,
    /// Higher wins.
    pub priority: i32,
    /// Seconds the trigger must hold.
    pub hold: f32,
    /// The hold counter is running now.
    pub holding: bool,
    /// The tick it was last taken, as far back as the trace reaches.
    pub last_taken: Option<u64>,
}

/// One state of the selected tree, in depth-first order.
#[derive(Clone, Debug, PartialEq)]
pub struct StateRow {
    /// Its name.
    pub name: String,
    /// Nesting below the root.
    pub depth: usize,
    /// How it picks a child: `first`, `utility`, `weighted`.
    pub selection: &'static str,
    /// The enter condition as text; empty when it is always true.
    pub enter: String,
    /// Task type paths.
    pub tasks: Vec<String>,
    /// It is on the active path.
    pub active: bool,
    /// The debug switch that skips it.
    pub disabled: bool,
    /// The debug switch that pauses the tree when it is entered.
    pub pause_on_enter: bool,
    /// Its transitions.
    pub transitions: Vec<TransitionRow>,
}

/// One trace line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRow {
    /// The tick.
    pub tick: u64,
    /// What happened: `enter`, `exit`, `transition`, `rejected`, `event_ignored`, `event_dropped`,
    /// `start_failed`, `finished`.
    pub kind: &'static str,
    /// The whole line.
    pub text: String,
}

/// One blackboard entry.
#[derive(Clone, Debug, PartialEq)]
pub struct BlackboardRow {
    /// The key.
    pub key: String,
    /// `bool`, `int`, `float`, `string`, `vec3` or `entity`.
    pub kind: &'static str,
    /// The value as the agent reads it.
    pub value: Value,
}

/// The selected tree in full.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeDetail {
    /// States in depth-first order; a state the hierarchy does not reach comes last at depth 0.
    pub states: Vec<StateRow>,
    /// The tree's random seed.
    pub seed: u64,
    /// The blackboard key the active leaf is written to; empty for none.
    pub state_key: String,
    /// Labels of transitions whose hold counter runs.
    pub held: Vec<String>,
    /// The recent trace, newest last.
    pub trace: Vec<TraceRow>,
    /// The blackboard, sorted by key; empty with no blackboard.
    pub blackboard: Vec<BlackboardRow>,
    /// The entity has a blackboard.
    pub has_blackboard: bool,
}

/// The panel's model.
#[derive(Resource, Debug, Default)]
pub struct StateTreePanel {
    /// The world this snapshot came from.
    pub source: LiveSource,
    /// The trees in the list, sorted by entity.
    pub trees: Vec<TreeRow>,
    /// Trees in the world, listed or not.
    pub total: usize,
    /// The tree the detail shows.
    pub selected: Option<Entity>,
    /// The selected tree's detail.
    pub detail: Option<TreeDetail>,
    /// The event field of the panel.
    pub event_text: String,
    /// The reload path field of the panel (relative to the project).
    pub reload_path: String,
    /// What the last call did.
    pub last_call: Option<CallRecord>,
    /// Snapshots taken so far; an agent polling play state can see it advance.
    pub snapshots: u64,
    pending: Vec<PanelCall>,
}

fn status_name(status: StateTreeRunStatus) -> &'static str {
    match status {
        StateTreeRunStatus::NotStarted => "not_started",
        StateTreeRunStatus::Running => "running",
        StateTreeRunStatus::Succeeded => "succeeded",
        StateTreeRunStatus::Failed => "failed",
    }
}

/// A condition as one line of text: `alert`, `hp < 10`, `!(a & b)`. Empty for "always true".
#[must_use]
pub fn condition_text(condition: &Condition) -> String {
    fn inner(condition: &Condition) -> String {
        let list = |items: &[Condition], joint: &str| {
            let parts: Vec<String> = items.iter().map(inner).collect();
            format!("({})", parts.join(joint))
        };
        match condition {
            Condition::Always => "always".to_owned(),
            Condition::Key(key) => key.clone(),
            Condition::Has(key) => format!("has {key}"),
            Condition::Compare { key, op, value } => {
                let op = match op {
                    CompareOp::Lt => "<",
                    CompareOp::Le => "<=",
                    CompareOp::Eq => "==",
                    CompareOp::Ne => "!=",
                    CompareOp::Ge => ">=",
                    CompareOp::Gt => ">",
                };
                format!("{key} {op} {value}")
            }
            Condition::Text { key, value } => format!("{key} == {value:?}"),
            Condition::Not(c) => format!("!{}", inner(c)),
            Condition::All(items) => list(items, " & "),
            Condition::Any(items) => list(items, " | "),
        }
    }
    if *condition == Condition::Always {
        String::new()
    } else {
        inner(condition)
    }
}

fn trigger_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Tick => "tick".to_owned(),
        Trigger::Event(name) => format!("event:{name}"),
        Trigger::Succeeded => "succeeded".to_owned(),
        Trigger::Failed => "failed".to_owned(),
        Trigger::Completed => "completed".to_owned(),
    }
}

fn target_text(target: &Target) -> String {
    match target {
        Target::State(name) => name.clone(),
        Target::Next => "<next>".to_owned(),
        Target::NextEnterable => "<next-enterable>".to_owned(),
        Target::Parent => "<parent>".to_owned(),
        Target::Stay => "<stay>".to_owned(),
        Target::Succeed => "<succeed>".to_owned(),
        Target::Fail => "<fail>".to_owned(),
    }
}

fn trace_kind(kind: &TraceKind) -> &'static str {
    match kind {
        TraceKind::Entered(_) => "enter",
        TraceKind::Exited(_) => "exit",
        TraceKind::Transition { .. } => "transition",
        TraceKind::Rejected { .. } => "rejected",
        TraceKind::EventIgnored(_) => "event_ignored",
        TraceKind::EventDropped(_) => "event_dropped",
        TraceKind::StartFailed => "start_failed",
        TraceKind::Finished(_) => "finished",
    }
}

fn blackboard_row(key: &str, value: &BbValue) -> BlackboardRow {
    let (kind, value) = match value {
        BbValue::Bool(v) => ("bool", json!(v)),
        BbValue::Int(v) => ("int", json!(v)),
        BbValue::Float(v) => ("float", json!(v)),
        BbValue::String(v) => ("string", json!(v)),
        BbValue::Vec3(v) => ("vec3", json!(v.to_array())),
        BbValue::Entity(v) => ("entity", json!(v)),
    };
    BlackboardRow {
        key: key.to_owned(),
        kind,
        value,
    }
}

/// The tick a trace last shows this transition of `owner` being taken. A symbolic target resolves to
/// a state in the trace, so only a named target is compared.
fn last_taken(tree: &StateTree, owner: &str, transition: &TransitionDef) -> Option<u64> {
    let trigger = trigger_text(&transition.trigger);
    tree.trace()
        .filter(|event| match &event.kind {
            TraceKind::Transition {
                from, to, trigger: t, ..
            } => {
                from == owner
                    && *t == trigger
                    && match &transition.target {
                        Target::State(name) => to == name,
                        _ => true,
                    }
            }
            _ => false,
        })
        .map(|event| event.tick)
        .last()
}

fn state_row(tree: &StateTree, def: &StateDef, depth: usize, held: &[String]) -> StateRow {
    let (disabled, pause) = tree.debug_lists();
    StateRow {
        name: def.name.clone(),
        depth,
        selection: match def.selection {
            regia_ai::state_tree::Selection::First => "first",
            regia_ai::state_tree::Selection::Utility => "utility",
            regia_ai::state_tree::Selection::Weighted => "weighted",
        },
        enter: condition_text(&def.enter),
        tasks: def.tasks.clone(),
        active: tree.is_active(&def.name),
        disabled: disabled.contains(&def.name),
        pause_on_enter: pause.contains(&def.name),
        transitions: def
            .transitions
            .iter()
            .map(|transition| {
                let target = target_text(&transition.target);
                // A hold label names the resolved state, so a symbolic target matches by its owner.
                let label = format!("{} -> ", def.name);
                let holding = transition.hold > 0.0
                    && held.iter().any(|held| match &transition.target {
                        Target::State(name) => held.strip_prefix(&label) == Some(name.as_str()),
                        _ => held.starts_with(&label),
                    });
                TransitionRow {
                    trigger: trigger_text(&transition.trigger),
                    condition: condition_text(&transition.condition),
                    priority: transition.priority,
                    hold: transition.hold,
                    holding,
                    last_taken: last_taken(tree, &def.name, transition),
                    target,
                }
            })
            .collect(),
    }
}

/// The states of `def` in depth-first order from the root, then any the hierarchy does not reach.
fn hierarchy(def: &StateTreeDef) -> Vec<(&StateDef, usize)> {
    let by_name: BTreeMap<&str, &StateDef> = def.states.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut out: Vec<(&StateDef, usize)> = Vec::with_capacity(def.states.len());
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<(&str, usize)> = vec![(def.root.as_str(), 0)];
    while let Some((name, depth)) = stack.pop() {
        let Some(state) = by_name.get(name) else {
            continue;
        };
        if !seen.insert(name) {
            continue;
        }
        out.push((state, depth));
        for child in state.children.iter().rev() {
            stack.push((child.as_str(), depth + 1));
        }
    }
    for state in &def.states {
        if seen.insert(state.name.as_str()) {
            out.push((state, 0));
        }
    }
    out
}

fn detail_of(world: &World, entity: Entity) -> Option<TreeDetail> {
    let tree = world.get::<StateTree>(entity)?;
    let def = tree.def();
    let held = tree.held();
    let trace: Vec<TraceRow> = tree
        .trace()
        .map(|event| TraceRow {
            tick: event.tick,
            kind: trace_kind(&event.kind),
            text: event.to_string(),
        })
        .collect();
    let skip = trace.len().saturating_sub(MAX_TRACE);
    let blackboard = world.get::<Blackboard>(entity);
    Some(TreeDetail {
        states: hierarchy(def)
            .into_iter()
            .map(|(state, depth)| state_row(tree, state, depth, &held))
            .collect(),
        seed: def.seed,
        state_key: def.state_key.clone(),
        held,
        trace: trace.into_iter().skip(skip).collect(),
        blackboard: blackboard
            .map(|bb| bb.values.iter().map(|(k, v)| blackboard_row(k, v)).collect())
            .unwrap_or_default(),
        has_blackboard: blackboard.is_some(),
    })
}

impl StateTreePanel {
    /// Copies the trees of `world` into the panel. The selection falls back to the first tree.
    pub fn snapshot(&mut self, world: &mut World, source: LiveSource) {
        let _span = tracing::debug_span!(target: "regia_editor", "state_tree_panel_snapshot").entered();
        if self.source != source {
            // The two worlds are separate id spaces.
            self.selected = None;
        }
        self.source = source;
        let mut query = world.query::<(Entity, &StateTree)>();
        let mut rows: Vec<(Entity, &StateTree)> = query.iter(world).collect();
        rows.sort_by_key(|(entity, _)| entity.to_bits());
        self.total = rows.len();
        self.trees = rows
            .iter()
            .take(MAX_TREES)
            .map(|(entity, tree)| TreeRow {
                entity: *entity,
                name: crate::outliner::display_name(world, *entity),
                status: status_name(tree.status()),
                paused: tree.is_paused(),
                active_path: tree.active_path().into_iter().map(str::to_owned).collect(),
                ticks: tree.ticks(),
            })
            .collect();
        let selected = self
            .selected
            .filter(|entity| world.get::<StateTree>(*entity).is_some())
            .or_else(|| self.trees.first().map(|row| row.entity));
        self.selected = selected;
        self.detail = selected.and_then(|entity| detail_of(world, entity));
        self.snapshots += 1;
    }

    /// Runs the queued calls on `world`.
    pub fn run_pending(&mut self, world: &mut World, source: LiveSource) {
        for call in std::mem::take(&mut self.pending) {
            let result = run_call(world, &call);
            self.last_call = Some(CallRecord::of(&call.method, source, result));
        }
    }

    fn queue(&mut self, method: &str, params: Value) {
        self.pending.push(PanelCall::new(method, params));
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "source": self.source.name(),
            "total": self.total,
            "truncated": self.total > self.trees.len(),
            "snapshots": self.snapshots,
            "dev_build": cfg!(debug_assertions),
            "trees": self.trees.iter().map(|row| json!({
                "entity": row.entity.to_bits(),
                "name": row.name,
                "status": row.status,
                "paused": row.paused,
                "active_path": row.active_path,
                "ticks": row.ticks,
            })).collect::<Vec<_>>(),
            "selected": self.selected.map(Entity::to_bits),
            "detail": self.detail.as_ref().map(detail_json),
            "last_call": self.last_call.as_ref().map(CallRecord::json),
        })
    }
}

fn detail_json(detail: &TreeDetail) -> Value {
    json!({
        "seed": detail.seed,
        "state_key": detail.state_key,
        "held": detail.held,
        "states": detail.states.iter().map(|state| json!({
            "name": state.name,
            "depth": state.depth,
            "selection": state.selection,
            "enter": state.enter,
            "tasks": state.tasks,
            "active": state.active,
            "disabled": state.disabled,
            "pause_on_enter": state.pause_on_enter,
            "transitions": state.transitions.iter().map(|t| json!({
                "trigger": t.trigger,
                "target": t.target,
                "condition": t.condition,
                "priority": t.priority,
                "hold": t.hold,
                "holding": t.holding,
                "last_taken": t.last_taken,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "trace": detail.trace.iter().map(|row| json!({
            "tick": row.tick,
            "kind": row.kind,
            "text": row.text,
        })).collect::<Vec<_>>(),
        "has_blackboard": detail.has_blackboard,
        "blackboard": detail.blackboard.iter().map(|row| json!({
            "key": row.key,
            "type": row.kind,
            "value": row.value,
        })).collect::<Vec<_>>(),
    })
}

/// Runs one call: the panel's own `statetree.send_event`, or an agent handler.
fn run_call(world: &mut World, call: &PanelCall) -> Result<Value, RpcError> {
    if call.method != SEND_EVENT {
        return regia_agent::panel_calls::call(world, &call.method, &call.params);
    }
    let event = call
        .params
        .get("event")
        .and_then(Value::as_str)
        .filter(|event| !event.is_empty())
        .ok_or_else(|| reason_error("missing_param", "`event` (a name) is required"))?;
    let entity = entity_param(&call.params, "entity")?
        .ok_or_else(|| reason_error("missing_param", "`entity` is required"))?;
    if world.get_entity(entity).is_err() {
        return Err(reason_error("entity_not_found", "no such entity"));
    }
    let mut tree = world
        .get_mut::<StateTree>(entity)
        .ok_or_else(|| reason_error("no_state_tree", "the entity has no StateTree"))?;
    Ok(json!({ "queued": tree.send_event(event) }))
}

/// Draws the panel. With no play session it looks at the edit world.
pub fn state_tree_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<StateTreePanel>().unwrap_or_default();
    if !play_running(world) {
        panel.snapshot(world, LiveSource::Edit);
    }
    draw(ui, &mut panel);
    world.insert_resource(panel);
}

/// Editor-loop hook, once per frame: runs the queue on the play world and copies its trees, or runs
/// the queue on the edit world when nothing plays.
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let _span = tracing::debug_span!(target: "regia_editor", "state_tree_panel_pump").entered();
    let Some(mut panel) = edit.remove_resource::<StateTreePanel>() else {
        return;
    };
    match session {
        Some(session) => {
            let play = session.app.world_mut();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot(play, LiveSource::Play);
        }
        None => panel.run_pending(edit, LiveSource::Edit),
    }
    edit.insert_resource(panel);
}

fn draw(ui: &mut egui::Ui, panel: &mut StateTreePanel) {
    let detail = format!("{} trees", panel.total);
    header(ui, "State Trees", panel.source, &detail);
    if panel.trees.is_empty() {
        ui.weak("No entity carries a StateTree here. Trees run in play: press Play to debug a game's trees.");
        return;
    }
    let mut chosen = panel.selected;
    egui::ScrollArea::vertical()
        .id_salt("state-tree-list")
        .max_height(120.0)
        .show(ui, |ui| {
            for row in &panel.trees {
                let text = format!(
                    "{}  [{}{}]  {}  (tick {})",
                    row.name,
                    row.status,
                    if row.paused { ", paused" } else { "" },
                    row.active_path.join(" / "),
                    row.ticks
                );
                if ui.selectable_label(chosen == Some(row.entity), text).clicked() {
                    chosen = Some(row.entity);
                }
            }
        });
    if chosen != panel.selected {
        panel.selected = chosen;
        panel.detail = None;
    }
    ui.separator();
    if let Some(entity) = panel.selected {
        controls(ui, panel, entity);
    }
    if let Some(record) = &panel.last_call {
        record.draw(ui);
    }
    let Some(detail) = panel.detail.clone() else {
        return;
    };
    let mut debug_change = None;
    egui::ScrollArea::vertical()
        .id_salt("state-tree-detail")
        .show(ui, |ui| {
            egui::CollapsingHeader::new("States and transitions")
                .default_open(true)
                .show(ui, |ui| {
                    for state in &detail.states {
                        if let Some(change) = state_line(ui, state) {
                            debug_change = Some(change);
                        }
                    }
                });
            egui::CollapsingHeader::new("Blackboard")
                .default_open(true)
                .show(ui, |ui| blackboard_table(ui, &detail));
            egui::CollapsingHeader::new("Trace")
                .default_open(true)
                .show(ui, |ui| {
                    for row in detail.trace.iter().rev() {
                        ui.monospace(&row.text);
                    }
                });
        });
    if let (Some((state, kind)), Some(entity)) = (debug_change, panel.selected) {
        toggle_debug(panel, &detail, entity, &state, kind);
    }
}

fn controls(ui: &mut egui::Ui, panel: &mut StateTreePanel, entity: Entity) {
    let bits = entity.to_bits();
    let paused = panel
        .trees
        .iter()
        .find(|row| row.entity == entity)
        .is_some_and(|row| row.paused);
    ui.horizontal(|ui| {
        let (label, op) = if paused {
            ("Resume", "resume")
        } else {
            ("Pause", "pause")
        };
        if ui.button(label).clicked() {
            panel.queue("statetree.control", json!({ "entity": bits, "op": op }));
        }
        if ui.button("Restart").clicked() {
            panel.queue("statetree.control", json!({ "entity": bits, "op": "restart" }));
        }
        ui.text_edit_singleline(&mut panel.event_text);
        if ui.button("Send event").clicked() {
            let event = panel.event_text.clone();
            panel.queue(SEND_EVENT, json!({ "entity": bits, "event": event }));
        }
    });
}

fn state_line(ui: &mut egui::Ui, state: &StateRow) -> Option<(String, DebugSwitch)> {
    let mut change = None;
    ui.horizontal(|ui| {
        ui.add_space(state.depth as f32 * 14.0);
        let mut name = egui::RichText::new(&state.name).monospace();
        if state.active {
            name = name.strong().color(egui::Color32::from_rgb(120, 220, 120));
        }
        ui.label(name);
        ui.weak(state.selection);
        if !state.enter.is_empty() {
            ui.weak(format!("if {}", state.enter));
        }
        if !state.tasks.is_empty() {
            ui.weak(state.tasks.join(", "));
        }
        if cfg!(debug_assertions) {
            let mut skip = state.disabled;
            if ui.checkbox(&mut skip, "skip").changed() {
                change = Some((state.name.clone(), DebugSwitch::Skip));
            }
            let mut pause = state.pause_on_enter;
            if ui.checkbox(&mut pause, "pause on enter").changed() {
                change = Some((state.name.clone(), DebugSwitch::PauseOnEnter));
            }
        }
    });
    for t in &state.transitions {
        ui.horizontal(|ui| {
            ui.add_space(state.depth as f32 * 14.0 + 14.0);
            let mut line = format!("-> {} on {}", t.target, t.trigger);
            if !t.condition.is_empty() {
                line.push_str(&format!(" if {}", t.condition));
            }
            if t.priority != 0 {
                line.push_str(&format!(" priority {}", t.priority));
            }
            if t.hold > 0.0 {
                line.push_str(&format!(" hold {:.2} s", t.hold));
            }
            if let Some(tick) = t.last_taken {
                line.push_str(&format!("  (taken at tick {tick})"));
            }
            let color = if t.holding {
                egui::Color32::from_rgb(235, 170, 80)
            } else if t.last_taken.is_some() {
                egui::Color32::from_rgb(120, 180, 235)
            } else {
                egui::Color32::GRAY
            };
            ui.colored_label(color, line);
        });
    }
    change
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DebugSwitch {
    Skip,
    PauseOnEnter,
}

fn toggle_debug(
    panel: &mut StateTreePanel,
    detail: &TreeDetail,
    entity: Entity,
    state: &str,
    kind: DebugSwitch,
) {
    let names = |pick: &dyn Fn(&StateRow) -> bool, flip: bool| -> Vec<String> {
        detail
            .states
            .iter()
            .filter(|row| pick(row) != (flip && row.name == state))
            .map(|row| row.name.clone())
            .collect()
    };
    let disabled = names(&|row| row.disabled, kind == DebugSwitch::Skip);
    let pause = names(&|row| row.pause_on_enter, kind == DebugSwitch::PauseOnEnter);
    panel.queue(
        "statetree.debug",
        json!({ "entity": entity.to_bits(), "disabled": disabled, "pause_on_enter": pause }),
    );
}

fn blackboard_table(ui: &mut egui::Ui, detail: &TreeDetail) {
    if !detail.has_blackboard {
        ui.weak("This entity has no Blackboard.");
        return;
    }
    egui::Grid::new("state-tree-blackboard")
        .num_columns(3)
        .show(ui, |ui| {
            for row in &detail.blackboard {
                ui.monospace(&row.key);
                ui.weak(row.kind);
                ui.monospace(row.value.to_string());
                ui.end_row();
            }
        });
}

/// Reads a `.statetree.ron` of the project with its includes expanded, so the play world (which may
/// have no asset server) compiles it as it is.
fn read_reload_text(project: Option<&Path>, path: &str) -> Result<String, RpcError> {
    let root = project.ok_or_else(|| reason_error("state_tree_panel_no_project", "no project is open"))?;
    let confined = |rel: &str| {
        !rel.is_empty()
            && Path::new(rel)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    };
    if !confined(path) {
        return Err(reason_error(
            "state_tree_panel_bad_path",
            format!("`{path}` must be a plain path relative to the project"),
        ));
    }
    let text = std::fs::read_to_string(root.join(path))
        .map_err(|e| reason_error("state_tree_panel_io", format!("{path}: {e}")))?;
    let refuse = |e: regia_ai::state_tree::StateTreeError| reason_error(e.reason(), e.to_string());
    let def = StateTreeDef::from_ron(&text).map_err(refuse)?;
    let load = |rel: &str| {
        confined(rel)
            .then(|| std::fs::read_to_string(root.join(rel)).ok())
            .flatten()
    };
    def.expand_includes(&load)
        .map_err(refuse)?
        .to_ron()
        .map_err(refuse)
}

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut StateTreePanel, &mut World) -> T) -> T {
    let mut panel = world.remove_resource::<StateTreePanel>().unwrap_or_default();
    let out = f(&mut panel, world);
    world.insert_resource(panel);
    out
}

/// `editor.state_tree_panel_get`.
fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_panel(world, |panel, world| {
        if !play_running(world) {
            panel.snapshot(world, LiveSource::Edit);
        }
        panel.json()
    }))
}

/// `editor.state_tree_panel_select { entity }`.
fn panel_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity =
        entity_param(params, "entity")?.ok_or_else(|| RpcError::invalid_params("`entity` is required"))?;
    with_panel(world, |panel, world| {
        let playing = play_running(world);
        if !playing && world.get::<StateTree>(entity).is_none() {
            return Err(reason_error("no_state_tree", "the entity has no StateTree"));
        }
        if playing && !panel.trees.iter().any(|row| row.entity == entity) {
            return Err(reason_error("no_state_tree", "the play world lists no such tree"));
        }
        panel.selected = Some(entity);
        if !playing {
            panel.snapshot(world, LiveSource::Edit);
        }
        Ok(panel.json())
    })
}

fn string_list(params: &Value, key: &str) -> Result<Option<Value>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) if value.as_array().is_some_and(|a| a.iter().all(Value::is_string)) => {
            Ok(Some(value.clone()))
        }
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{key}` must be an array of state names"
        ))),
    }
}

/// The call an `op` stands for.
fn call_of(panel: &StateTreePanel, project: Option<&Path>, params: &Value) -> Result<PanelCall, RpcError> {
    let entity = entity_param(params, "entity")?
        .or(panel.selected)
        .ok_or_else(|| reason_error("no_state_tree", "no tree is selected and no `entity` was given"))?
        .to_bits();
    let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
    match op {
        "pause" | "resume" | "restart" => Ok(PanelCall::new(
            "statetree.control",
            json!({ "entity": entity, "op": op }),
        )),
        "send_event" => Ok(PanelCall::new(
            SEND_EVENT,
            json!({ "entity": entity, "event": params.get("event").cloned().unwrap_or(Value::Null) }),
        )),
        "debug" => {
            let mut call = json!({ "entity": entity });
            for key in ["disabled", "pause_on_enter"] {
                if let Some(list) = string_list(params, key)? {
                    call[key] = list;
                }
            }
            Ok(PanelCall::new("statetree.debug", call))
        }
        "reload" => {
            let path = params
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| reason_error("missing_param", "`path` is required"))?;
            let text = read_reload_text(project, path)?;
            Ok(PanelCall::new(
                "statetree.reload",
                json!({ "entity": entity, "text": text }),
            ))
        }
        other => Err(reason_error(
            "bad_op",
            format!("unknown op {other:?}: pause, resume, restart, send_event, debug or reload"),
        )),
    }
}

/// `editor.state_tree_panel_control { entity?, op, event?, path?, disabled?, pause_on_enter? }`.
fn panel_control(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let project = crate::mesh_source::project_root(world);
    with_panel(world, |panel, world| {
        let call = call_of(panel, project.as_deref(), params)?;
        if play_running(world) {
            panel.pending.push(call);
            return Ok(json!({
                "queued": true,
                "hint": "applied to the play world on the next frame; read last_call from editor.state_tree_panel_get",
            }));
        }
        let result = run_call(world, &call);
        panel.last_call = Some(CallRecord::of(&call.method, LiveSource::Edit, result.clone()));
        panel.snapshot(world, LiveSource::Edit);
        Ok(json!({ "queued": false, "answer": result? }))
    })
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const SELECT_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "entity",
    kind: "integer",
    required: true,
    description: "The entity of the tree.",
}];

const CONTROL_PARAMS: &[ParamSchema] = &[
    param("entity", "integer", "The tree; default the selected one."),
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "pause, resume, restart, send_event, debug or reload.",
    },
    param("event", "string", "send_event: the event name."),
    param(
        "path",
        "string",
        "reload: a .statetree.ron relative to the project.",
    ),
    param("disabled", "array", "debug: the state names to skip."),
    param(
        "pause_on_enter",
        "array",
        "debug: the state names that pause the tree.",
    ),
];

const STATE: &str = "{ source, total, truncated, snapshots, dev_build, trees: [{ entity, name, status, paused, active_path, \
    ticks }], selected, detail: { seed, state_key, held, states: [{ name, depth, selection, enter, tasks, active, disabled, \
    pause_on_enter, transitions: [{ trigger, target, condition, priority, hold, holding, last_taken }] }], trace: [{ tick, \
    kind, text }], has_blackboard, blackboard: [{ key, type, value }] }, last_call }";

/// The State Trees panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.state_tree_panel_get",
                tool: "state_tree_panel_get",
                description: "The State Trees panel: the trees of the play world (or of the edit world when nothing \
                    plays) and, for the selected one, its states, active path, transitions, blackboard and trace.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.state_tree_panel_select",
                tool: "state_tree_panel_select",
                description: "Choose the tree the State Trees panel shows in detail.",
                params: SELECT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Play,
            },
            panel_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.state_tree_panel_control",
                tool: "state_tree_panel_control",
                description: "Steer a tree as the panel's buttons do: pause, resume, restart, send_event, debug \
                    (skip and pause-on-enter lists) or reload from a file. While playing the call runs on the play \
                    world on the next frame (queued: true) and its result is last_call.",
                params: CONTROL_PARAMS,
                result: "{ queued, answer?, hint? }",
                mutating: true,
                permission: Permission::Play,
            },
            panel_control as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use regia_ai::{BtContext, BtTaskRegistry};

    use super::*;

    const TREE: &str = r#"(
        format_version: 2,
        root: "Root",
        state_key: "state",
        states: [
            (name: "Root", children: ["Idle", "Alert", "Hunt"], selection: First),
            (name: "Idle", transitions: [
                (trigger: Event("heard"), target: State("Alert"), priority: 2),
                (trigger: Tick, condition: Compare(key: "fear", op: Gt, value: 0.5), target: Next, hold: 10.0),
            ]),
            (name: "Alert", enter: Key("awake"), transitions: [
                (trigger: Event("calm"), target: State("Idle")),
            ]),
            (name: "Hunt", transitions: [(trigger: Tick, target: Succeed)]),
        ],
    )"#;

    fn world_with_tree() -> (World, Entity) {
        let mut world = World::new();
        world.init_resource::<BtTaskRegistry>();
        let def = StateTreeDef::from_ron(TREE).expect("parses");
        let tree = StateTree::compile(&def, world.resource::<BtTaskRegistry>()).expect("compiles");
        let mut board = Blackboard::default();
        board.insert("awake", true);
        board.insert("fear", 0.75f32);
        let entity = world.spawn((tree, board)).id();
        (world, entity)
    }

    fn tick(world: &mut World, entity: Entity, n: usize) {
        for _ in 0..n {
            let mut tree = world.entity_mut(entity).take::<StateTree>().expect("tree");
            tree.tick(&mut BtContext {
                world,
                entity,
                dt: 1.0 / 60.0,
            });
            world.entity_mut(entity).insert(tree);
        }
    }

    fn get(world: &mut World) -> Value {
        panel_get(world, &json!({})).expect("get")
    }

    fn draw_frames(world: &mut World) -> usize {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(640.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| state_tree_panel(ui, world));
        output.textures_delta.clear();
        ctx.tessellate(output.shapes, output.pixels_per_point).len()
    }

    #[test]
    fn the_view_shows_the_hierarchy_the_active_path_and_the_blackboard() {
        let (mut world, entity) = world_with_tree();
        tick(&mut world, entity, 2);
        let v = get(&mut world);
        assert_eq!(v["source"], "edit");
        assert_eq!(v["total"], 1);
        assert_eq!(v["selected"], entity.to_bits());
        assert_eq!(v["trees"][0]["status"], "running");
        assert_eq!(v["trees"][0]["active_path"], json!(["Root", "Idle"]));
        assert_eq!(v["trees"][0]["ticks"], 2);
        let states = v["detail"]["states"].as_array().expect("states");
        let names: Vec<&str> = states.iter().filter_map(|s| s["name"].as_str()).collect();
        assert_eq!(names, ["Root", "Idle", "Alert", "Hunt"]);
        assert_eq!(states[0]["depth"], 0);
        assert_eq!(states[1]["depth"], 1);
        assert_eq!(states[1]["active"], true);
        assert_eq!(states[2]["active"], false);
        assert_eq!(states[2]["enter"], "awake");
        let idle = states[1]["transitions"].as_array().expect("transitions");
        assert_eq!(idle[0]["trigger"], "event:heard");
        assert_eq!(idle[0]["target"], "Alert");
        assert_eq!(idle[0]["priority"], 2);
        assert_eq!(idle[1]["target"], "<next>");
        assert_eq!(idle[1]["condition"], "fear > 0.5");
        assert_eq!(idle[1]["holding"], true, "fear holds, the counter runs");
        assert_eq!(v["detail"]["state_key"], "state");
        let board = v["detail"]["blackboard"].as_array().expect("blackboard");
        let awake = board.iter().find(|row| row["key"] == "awake").expect("awake");
        assert_eq!((&awake["type"], &awake["value"]), (&json!("bool"), &json!(true)));
        let state = board.iter().find(|row| row["key"] == "state").expect("state key");
        assert_eq!(state["value"], "Idle");
        assert!(!v["detail"]["trace"].as_array().expect("trace").is_empty());
    }

    #[test]
    fn a_taken_transition_is_marked_and_the_blackboard_follows() {
        let (mut world, entity) = world_with_tree();
        tick(&mut world, entity, 1);
        let answer = panel_control(
            &mut world,
            &json!({ "op": "send_event", "entity": entity.to_bits(), "event": "heard" }),
        )
        .expect("event");
        assert_eq!(answer["queued"], false);
        assert_eq!(answer["answer"]["queued"], true);
        tick(&mut world, entity, 1);
        let v = get(&mut world);
        assert_eq!(v["trees"][0]["active_path"], json!(["Root", "Alert"]));
        let states = v["detail"]["states"].as_array().expect("states");
        assert_eq!(states[1]["transitions"][0]["last_taken"], 2);
        assert!(states[2]["transitions"][0]["last_taken"].is_null());
        let trace = v["detail"]["trace"].as_array().expect("trace");
        assert!(trace.iter().any(|row| row["kind"] == "transition"));
        let board = v["detail"]["blackboard"].as_array().expect("blackboard");
        let state = board.iter().find(|row| row["key"] == "state").expect("key");
        assert_eq!(state["value"], "Alert");
    }

    #[test]
    fn pause_resume_restart_and_debug_go_through_the_agent_handlers() {
        let (mut world, entity) = world_with_tree();
        tick(&mut world, entity, 1);
        panel_control(&mut world, &json!({ "op": "pause", "entity": entity.to_bits() })).expect("pause");
        assert_eq!(get(&mut world)["trees"][0]["paused"], true);
        tick(&mut world, entity, 3);
        assert_eq!(
            get(&mut world)["trees"][0]["ticks"],
            1,
            "a paused tree does not advance"
        );
        // The selected tree is the default.
        panel_control(&mut world, &json!({ "op": "resume" })).expect("resume");
        tick(&mut world, entity, 1);
        assert_eq!(get(&mut world)["trees"][0]["ticks"], 2);
        panel_control(&mut world, &json!({ "op": "restart" })).expect("restart");
        let v = get(&mut world);
        assert_eq!(v["trees"][0]["status"], "not_started");
        assert_eq!(v["trees"][0]["active_path"], json!([]));
        tick(&mut world, entity, 1);
        assert_eq!(
            get(&mut world)["trees"][0]["active_path"],
            json!(["Root", "Idle"])
        );
        let v = panel_control(
            &mut world,
            &json!({ "op": "debug", "disabled": ["Alert"], "pause_on_enter": ["Hunt"] }),
        );
        if cfg!(debug_assertions) {
            v.expect("debug");
            let v = get(&mut world);
            let states = v["detail"]["states"].as_array().expect("states");
            assert_eq!(states[2]["disabled"], true);
            assert_eq!(states[3]["pause_on_enter"], true);
        } else {
            let e = v.expect_err("dev only");
            assert_eq!(crate::gameplay_panel::reason_of(&e).as_deref(), Some("dev_only"));
        }
    }

    #[test]
    fn errors_carry_their_codes() {
        let (mut world, entity) = world_with_tree();
        let reason = |r: Result<Value, RpcError>| {
            crate::gameplay_panel::reason_of(&r.expect_err("refused")).unwrap_or_default()
        };
        let bits = entity.to_bits();
        let mut empty = World::new();
        assert_eq!(
            reason(panel_control(&mut empty, &json!({ "op": "pause" }))),
            "no_state_tree"
        );
        assert_eq!(
            reason(panel_control(&mut world, &json!({ "op": "x", "entity": bits }))),
            "bad_op"
        );
        assert_eq!(
            reason(panel_control(
                &mut world,
                &json!({ "op": "send_event", "entity": bits })
            )),
            "missing_param"
        );
        let plain = world.spawn_empty().id();
        assert_eq!(
            reason(panel_select(&mut world, &json!({ "entity": plain.to_bits() }))),
            "no_state_tree"
        );
        assert_eq!(
            reason(panel_control(
                &mut world,
                &json!({ "op": "pause", "entity": plain.to_bits() })
            )),
            "no_state_tree"
        );
        assert_eq!(
            reason(panel_control(
                &mut world,
                &json!({ "op": "reload", "entity": bits, "path": "a.statetree.ron" })
            )),
            "state_tree_panel_no_project"
        );
    }

    #[test]
    fn reload_reads_the_project_file_with_its_includes() {
        let dir = regia_test_utils::unique_temp_dir("state-tree-panel-reload");
        let (mut world, entity) = world_with_tree();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let bits = entity.to_bits();
        std::fs::write(
            dir.join("part.statetree.ron"),
            r#"(format_version: 2, root: "P", states: [(name: "P")])"#,
        )
        .expect("part");
        std::fs::write(
            dir.join("a.statetree.ron"),
            r#"(format_version: 2, root: "Root",
                includes: [(path: "part.statetree.ron", prefix: "Sub")],
                states: [(name: "Root", children: ["Idle", "Sub/P"]), (name: "Idle")])"#,
        )
        .expect("tree");
        tick(&mut world, entity, 1);
        let answer = panel_control(
            &mut world,
            &json!({ "op": "reload", "entity": bits, "path": "a.statetree.ron" }),
        )
        .expect("reload");
        assert_eq!(answer["answer"]["outcome"], "kept");
        let v = get(&mut world);
        let names: Vec<&str> = v["detail"]["states"]
            .as_array()
            .expect("states")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert_eq!(names, ["Root", "Idle", "Sub/P"]);
        for (path, reason) in [
            ("../a.statetree.ron", "state_tree_panel_bad_path"),
            ("none.statetree.ron", "state_tree_panel_io"),
        ] {
            let e = panel_control(&mut world, &json!({ "op": "reload", "path": path })).expect_err("refused");
            assert_eq!(crate::gameplay_panel::reason_of(&e).as_deref(), Some(reason));
        }
        std::fs::write(dir.join("bad.statetree.ron"), "(").expect("bad");
        let e = panel_control(
            &mut world,
            &json!({ "op": "reload", "path": "bad.statetree.ron" }),
        )
        .expect_err("refused");
        assert!(crate::gameplay_panel::reason_of(&e).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn while_playing_calls_queue_and_the_pump_reads_the_play_world() {
        let (mut play, entity) = world_with_tree();
        tick(&mut play, entity, 1);
        let mut edit = World::new();
        edit.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        // Nothing has been copied yet, and the edit world has no tree.
        assert_eq!(get(&mut edit)["total"], 0);
        let pump_once = |edit: &mut World, play: &mut World| {
            let mut panel = edit.remove_resource::<StateTreePanel>().unwrap_or_default();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot(play, LiveSource::Play);
            edit.insert_resource(panel);
        };
        pump_once(&mut edit, &mut play);
        let v = get(&mut edit);
        assert_eq!(
            (v["source"].clone(), v["total"].clone()),
            (json!("play"), json!(1))
        );
        let queued = panel_control(&mut edit, &json!({ "op": "pause" })).expect("queued");
        assert_eq!(queued["queued"], true);
        assert!(
            !play.get::<StateTree>(entity).expect("tree").is_paused(),
            "not yet"
        );
        pump_once(&mut edit, &mut play);
        assert!(play.get::<StateTree>(entity).expect("tree").is_paused());
        let v = get(&mut edit);
        assert_eq!(v["last_call"]["ok"], true);
        assert_eq!(v["last_call"]["source"], "play");
        assert_eq!(v["trees"][0]["paused"], true);
        let select = panel_select(&mut edit, &json!({ "entity": entity.to_bits() })).expect("select");
        assert_eq!(select["selected"], entity.to_bits());
        let gone = play.spawn_empty().id();
        play.despawn(gone);
        let e = panel_select(&mut edit, &json!({ "entity": gone.to_bits() }));
        assert!(e.is_err());
        // Failures come back through the record.
        panel_control(
            &mut edit,
            &json!({ "op": "send_event", "entity": gone.to_bits(), "event": "x" }),
        )
        .expect("queued");
        pump_once(&mut edit, &mut play);
        let v = get(&mut edit);
        assert_eq!(v["last_call"]["ok"], false);
        assert_eq!(v["last_call"]["reason"], "entity_not_found");
        // The snapshot's count moves, so an agent can tell a new one from an old one.
        let before = v["snapshots"].as_u64().expect("count");
        pump_once(&mut edit, &mut play);
        assert_eq!(get(&mut edit)["snapshots"].as_u64(), Some(before + 1));
    }

    #[test]
    fn the_panel_draws_headless_in_every_state() {
        let mut empty = World::new();
        assert!(draw_frames(&mut empty) > 0, "no trees");
        let (mut world, entity) = world_with_tree();
        assert!(draw_frames(&mut world) > 0, "not started");
        tick(&mut world, entity, 2);
        assert!(draw_frames(&mut world) > 0, "running");
        panel_control(&mut world, &json!({ "op": "pause" })).expect("pause");
        assert!(draw_frames(&mut world) > 0, "paused");
        let plain = world.spawn_empty().id();
        panel_control(&mut world, &json!({ "op": "pause", "entity": plain.to_bits() })).expect_err("refused");
        world.entity_mut(entity).remove::<Blackboard>();
        assert!(draw_frames(&mut world) > 0, "no blackboard, failed call");
    }

    #[test]
    fn the_debug_switch_flips_only_its_own_state() {
        let (mut world, entity) = world_with_tree();
        get(&mut world);
        let mut panel = world.remove_resource::<StateTreePanel>().expect("panel");
        let detail = panel.detail.clone().expect("detail");
        toggle_debug(&mut panel, &detail, entity, "Alert", DebugSwitch::Skip);
        assert_eq!(panel.pending[0].params["disabled"], json!(["Alert"]));
        assert_eq!(panel.pending[0].params["pause_on_enter"], json!([]));
        let mut on = detail;
        on.states[2].disabled = true;
        toggle_debug(&mut panel, &on, entity, "Alert", DebugSwitch::Skip);
        assert_eq!(panel.pending[1].params["disabled"], json!([]));
    }

    #[test]
    fn every_method_answers_by_its_registered_name() {
        let list = methods();
        let (mut world, entity) = world_with_tree();
        for (schema, handler) in &list {
            let params = match schema.name {
                "editor.state_tree_panel_control" => json!({ "op": "pause", "entity": entity.to_bits() }),
                "editor.state_tree_panel_select" => json!({ "entity": entity.to_bits() }),
                _ => json!({}),
            };
            handler(&mut world, &params).unwrap_or_else(|e| panic!("{}: {e:?}", schema.name));
        }
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn condition_text_reads_like_the_asset() {
        assert_eq!(condition_text(&Condition::Always), "");
        let c = Condition::Not(Box::new(Condition::All(vec![
            Condition::Key("a".into()),
            Condition::Has("b".into()),
        ])));
        assert_eq!(condition_text(&c), "!(a & has b)");
        let c = Condition::Any(vec![Condition::Text {
            key: "k".into(),
            value: "v".into(),
        }]);
        assert_eq!(condition_text(&c), "(k == \"v\")");
    }
}
